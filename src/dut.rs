//! Resolves the DUT module: asks the analyzer whether the module can have a
//! harness, and what its ports are.
//!
//! The data comes from two sources:
//!
//! - The guard (no parameters, no generics) uses the symbol table. The IR's
//!   `Module.variables` drops type parameters (`param TYPE: type = logic<WIDTH>`),
//!   so an IR-only check misses exactly the parameter that sets a port width.
//! - Widths and clock domains come from the IR. The symbol table keeps types
//!   unresolved.
//!
//! The IR holds a parameterized module only as elaborated with its defaults. A
//! harness built from it would silently use the default widths, so such a
//! module is rejected here.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;
use veryl::incremental::OutputIntent;
use veryl::pipeline::{self, AnalyzeOptions};
use veryl_analyzer::ir::{Component, Ir, Module as IrModule};
use veryl_analyzer::namespace::Namespace;
use veryl_analyzer::symbol::{ClockDomain, Direction, ParameterKind, SymbolKind};
use veryl_analyzer::symbol_table;
use veryl_metadata::Metadata;
use veryl_parser::resource_table;
use veryl_parser::veryl_token::TokenSource;

/// Error messages list at most this many candidates. A message that fills the
/// screen is not read.
const MAX_CANDIDATES: usize = 20;

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// A resolved DUT.
#[derive(Debug)]
pub struct Dut {
    pub name: String,

    /// Where the module is defined. `check` prints it, so the user can see which
    /// module was picked when two share a name.
    pub file: PathBuf,
    pub line: u32,

    /// Ports in declaration order, from `ModuleProperty.ports` in the symbol
    /// table. The IR's `port_types` is a `HashMap` with no fixed order, so it
    /// must not decide the order, or the output is not deterministic.
    pub ports: Vec<Port>,
}

/// Details of a port that is a modport of `std::axi4_if`.
///
/// Interfaces do not appear in the IR, so the port type gives no widths or
/// members. Only three things are known: which interface, which modport, and
/// the generic arguments. The generator knows the AXI4 signal set itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Axi4Port {
    /// `master` / `slave` / `write_master` / `read_master` / ...
    pub modport: String,

    /// All arguments of `axi4_pkg::<..>`, in the written order. The harness
    /// must `inst` an interface of the same type, so every argument is kept.
    /// Filling some with defaults would give a different type.
    pub args: Vec<u32>,
}

impl Axi4Port {
    /// Address width (`ADDR_W`).
    pub fn addr_width(&self) -> u32 {
        self.args[0]
    }

    /// Data width in bytes (`DATA_W_BYTES`), as `std::axi4_pkg` takes it.
    pub fn data_bytes(&self) -> u32 {
        self.args[1]
    }

    /// ID width (`ID_W`).
    pub fn id_width(&self) -> u32 {
        self.args[2]
    }

    /// Data width in bits.
    pub fn data_width(&self) -> u32 {
        self.data_bytes() * 8
    }

    /// The type the harness uses for its `inst`, with the same arguments as
    /// the DUT.
    pub fn pkg(&self) -> String {
        let args = self
            .args
            .iter()
            .map(|arg| arg.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("$std::axi4_pkg::<{args}>")
    }

    /// Whether the DUT is the master side. The harness takes the other side.
    pub fn is_master(&self) -> bool {
        self.modport == "master" || self.modport.ends_with("_master")
    }
}

/// One declared port.
#[derive(Debug)]
pub struct Port {
    pub name: String,

    /// Taken from the DUT declaration (symbol table), never guessed from the
    /// name. Widths and domains come from the IR (`signals`).
    pub direction: PortDirection,

    /// The signals after expansion. Usually one; an interface port expands to
    /// several. It can be empty when the IR has no type for the port: then the
    /// width and domain are unknown, and they are shown as unknown.
    pub signals: Vec<Signal>,

    /// Set only for a modport of `std::axi4_if`.
    pub axi4: Option<Axi4Port>,
}

impl Port {
    /// The bit width of the whole port. `None` for several signals (an
    /// interface) or an unresolved width.
    ///
    /// Arrays are not flattened. Treating `logic<64> [8]` as one 512-bit signal
    /// broke synthesis; `feasibility::check_ports` rejects arrays first.
    pub fn width(&self) -> Option<usize> {
        match self.signals.as_slice() {
            [signal] => match (signal.width, signal.array) {
                (Some(width), Some(1)) => Some(width),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether this is a clock or reset port, decided by type, not by name.
    ///
    /// These ports are not bundled; the clock and reset plan drives them. An
    /// interface that contains a clock is not a clock port, so this is true
    /// only when every signal is a clock or a reset.
    pub fn is_clock_or_reset(&self) -> bool {
        !self.signals.is_empty()
            && self
                .signals
                .iter()
                .all(|signal| matches!(signal.role, SignalRole::Clock | SignalRole::Reset))
    }
}

/// Port direction, in the generator's own terms rather than Veryl's
/// `Direction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDirection {
    Input,
    Output,
    Inout,
    Interface,
    Modport,
    Import,
}

impl PortDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            PortDirection::Input => "input",
            PortDirection::Output => "output",
            PortDirection::Inout => "inout",
            PortDirection::Interface => "interface",
            PortDirection::Modport => "modport",
            PortDirection::Import => "import",
        }
    }
}

impl fmt::Display for PortDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

impl From<&Direction> for PortDirection {
    fn from(x: &Direction) -> Self {
        match x {
            Direction::Input => PortDirection::Input,
            Direction::Output => PortDirection::Output,
            Direction::Inout => PortDirection::Inout,
            Direction::Interface => PortDirection::Interface,
            Direction::Modport => PortDirection::Modport,
            Direction::Import => PortDirection::Import,
        }
    }
}

/// One signal as the IR sees it.
#[derive(Debug)]
pub struct Signal {
    /// The key in the IR's `port_types`: `i_data`, or `i_bus.valid` when
    /// expanded.
    pub path: String,
    pub role: SignalRole,

    /// The type as text (`logic<8>`, `reset_async_low`, ...).
    pub type_text: String,

    /// The bit width after parameters are resolved. `None` means it could not
    /// be resolved; it is not 0 or 1. The user must then state it in TOML.
    pub width: Option<usize>,

    /// The number of array elements. `None` means it could not be resolved.
    pub array: Option<usize>,

    pub domain: Domain,
}

/// Clock, reset, or anything else. The clock and reset plan uses this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalRole {
    Clock,
    Reset,
    Data,
}

/// A clock domain.
///
/// **Never merge `Inferred` into `Explicit`.** An XDC false path on a crossing
/// of inferred domains hides CDC bugs. The harness relaxes only crossings
/// through synchronizers it inserted itself, so this difference must be kept
/// to the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Domain {
    /// Written in the source as `'a`.
    Explicit(String),
    /// Inferred by the analyzer. Not written in the source.
    Inferred(String),
    /// `'_` (any domain).
    Implicit,
    /// No domain applies (for example, a signal unrelated to clock or reset).
    None,
}

impl Domain {
    /// For display. Inferred and explicit domains read differently.
    pub fn label(&self) -> String {
        match self {
            Domain::Explicit(name) => format!("'{name} (explicit)"),
            Domain::Inferred(name) => format!("'{name} (inferred)"),
            Domain::Implicit => "'_ (any)".to_string(),
            Domain::None => "-".to_string(),
        }
    }
}

impl From<&ClockDomain> for Domain {
    fn from(x: &ClockDomain) -> Self {
        match x {
            ClockDomain::Explicit(id) => Domain::Explicit(domain_name(*id)),
            ClockDomain::Inferred(id) => Domain::Inferred(domain_name(*id)),
            ClockDomain::Implicit => Domain::Implicit,
            ClockDomain::None => Domain::None,
        }
    }
}

fn domain_name(id: veryl_analyzer::symbol::SymbolId) -> String {
    symbol_table::get(id)
        .map(|symbol| format!("{}", symbol.token.text))
        .unwrap_or_else(|| "?".to_string())
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, Diagnostic)]
pub enum DutError {
    /// The widths are read from the generic arguments on the port, because
    /// interfaces do not appear in the IR.
    #[error("port `{port}` of `{module}` is a `$std::axi4_if`, but {why}")]
    #[diagnostic(
        code(harness::dut::axi4_args_unreadable),
        help(
            "The harness reads the AXI4 widths from the port's type, so write them as numbers:\n\n    {port}: modport $std::axi4_if::<$std::axi4_pkg::<32, 8, 4, 1, 1, 1, 1, 1>>::master,\n\nIf the DUT takes them from constants, give the harness a wrapper whose port spells them out."
        )
    )]
    Axi4ArgsUnreadable {
        module: String,
        port: String,
        why: &'static str,
    },

    #[error("[dut] module `{module}` is not a module of project `{project}`")]
    #[diagnostic(
        code(harness::dut::not_found),
        help(
            "Modules in `{project}`:\n{candidates}\n\nThe name is matched against the analyzer's symbol table, so a rename in the source shows up here."
        )
    )]
    NotFound {
        module: String,
        project: String,
        candidates: String,
    },

    #[error(
        "[dut] module `{module}` belongs to dependency `{dependency}`, not to project `{project}`"
    )]
    #[diagnostic(
        code(harness::dut::in_dependency),
        help(
            "The DUT must be a module of the project the harness runs in. Instantiate `{module}` in a wrapper in `{project}`, and point [dut] at the wrapper:\n\n    module {module}_top (\n        // ports of {module}\n    ) {{\n        inst u: {dependency}::{module} ( .. );\n    }}\n\nThe wrapper must have no parameters."
        )
    )]
    InDependency {
        module: String,
        project: String,
        dependency: String,
    },

    #[error("[dut] module `{module}` is declared more than once in project `{project}`")]
    #[diagnostic(
        code(harness::dut::ambiguous),
        help(
            "Declared at:\n{sites}\n\nRename one of them, or point [dut] at a wrapper that instantiates the one you mean."
        )
    )]
    Ambiguous {
        module: String,
        project: String,
        sites: String,
    },

    /// With the defaults, the harness would silently get widths nobody asked for.
    #[error("[dut] module `{module}` has {count} parameter(s)")]
    #[diagnostic(
        code(harness::dut::has_parameters),
        help(
            "Parameters: {names}\n\nA parameterized module would be built with its default values. Instantiate it with the values you want in a wrapper without parameters, and point [dut] at the wrapper:\n\n    module {module}_top (\n        // ports, with the widths the parameters resolve to\n    ) {{\n        inst u: {module} #( /* the values */ ) ( .. );\n    }}"
        )
    )]
    HasParameters {
        module: String,
        count: usize,
        names: String,
    },

    #[error("[dut] module `{module}` has {count} generic parameter(s)")]
    #[diagnostic(
        code(harness::dut::has_generics),
        help(
            "Generic parameters: {names}\n\nA generic module has no fixed ports. Instantiate it in a wrapper without parameters, and point [dut] at the wrapper."
        )
    )]
    HasGenerics {
        module: String,
        count: usize,
        names: String,
    },

    #[error("the analyzer produced no IR for module `{module}`")]
    #[diagnostic(
        code(harness::dut::no_ir),
        help(
            "The module has no body, so its port widths and clock domains cannot be read. This happens with a `proto` module or a SystemVerilog blackbox. Point [dut] at a Veryl module with a body."
        )
    )]
    NoIr { module: String },
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/// Analyzes the project and returns the IR. The symbol table is filled as a
/// global side effect (as in Veryl itself), so it is not returned.
pub fn analyze(metadata: &mut Metadata) -> miette::Result<Ir> {
    let no_files: Vec<PathBuf> = Vec::new();
    let paths = metadata.paths(&no_files, true, true)?;

    let options = AnalyzeOptions {
        defines: &[],
        // Only the IR is read; nothing is written.
        output_intent: OutputIntent::Never,
        // The generator needs a fresh IR each time.
        incremental: false,
        // Report every error at once, so the user can fix them all in one pass.
        fail_fast: false,
    };

    let mut ir = Ir::default();
    let output = pipeline::analyze(metadata, &paths, options, Some(&mut ir), None)?;

    // Do not skip this. With `fail_fast: false`, `analyze` returns Ok even on
    // errors and keeps the diagnostics in `check_error`. Without this check,
    // the port table would come from a broken IR.
    //
    // `check_err`, not `check_all`: warnings do not stop us. The DUT is often
    // someone else's RTL, and requiring zero warnings is too strict.
    let _ = output.check_error.check_err()?;

    Ok(ir)
}

/// Resolves the DUT from the analyzed IR and the symbol table.
pub fn resolve(ir: &Ir, project: &str, module: &str) -> Result<Dut, DutError> {
    let mut prj_namespace = Namespace::new();
    prj_namespace.push(resource_table::insert_str(project));

    let mut in_project = Vec::new();
    let mut elsewhere = Vec::new();
    let mut all_names = Vec::new();
    let mut seen_sites = HashSet::new();

    for symbol in symbol_table::get_all() {
        if !matches!(symbol.kind, SymbolKind::Module(_)) {
            continue;
        }

        // Only top-level modules whose namespace is the project itself.
        // `matched`, not `included`, so that a symbol with the same name inside
        // a module's namespace is not picked up.
        let is_project = symbol.namespace.matched(&prj_namespace);
        let name = format!("{}", symbol.token.text);

        if is_project {
            all_names.push(name.clone());
        }
        if name != module {
            continue;
        }

        if is_project {
            // A generic instantiation gives one declaration several symbols.
            // Remove duplicates by site, or it looks like a double definition.
            let site = (token_file(&symbol), symbol.token.line, symbol.token.column);
            if seen_sites.insert(site) {
                in_project.push(symbol);
            }
        } else {
            elsewhere.push(symbol);
        }
    }

    let symbol = match in_project.len() {
        1 => in_project.pop().unwrap(),
        0 => {
            if let Some(other) = elsewhere.first() {
                return Err(DutError::InDependency {
                    module: module.to_string(),
                    project: project.to_string(),
                    dependency: format!("{}", other.namespace),
                });
            }
            all_names.sort();
            all_names.dedup();
            return Err(DutError::NotFound {
                module: module.to_string(),
                project: project.to_string(),
                candidates: bullet_list(&all_names),
            });
        }
        _ => {
            let sites: Vec<String> = in_project
                .iter()
                .map(|symbol| {
                    format!(
                        "{}:{}:{}",
                        token_file(symbol).display(),
                        symbol.token.line,
                        symbol.token.column
                    )
                })
                .collect();
            return Err(DutError::Ambiguous {
                module: module.to_string(),
                project: project.to_string(),
                sites: bullet_list(&sites),
            });
        }
    };

    let SymbolKind::Module(property) = &symbol.kind else {
        unreachable!("filtered above");
    };

    // Guard 1: generics. The ports are not fixed.
    if !property.generic_parameters.is_empty() || !property.generic_consts.is_empty() {
        let names: Vec<String> = property
            .generic_parameters
            .iter()
            .chain(property.generic_consts.iter())
            .map(|id| {
                symbol_table::get(*id)
                    .map(|symbol| format!("{}", symbol.token.text))
                    .unwrap_or_else(|| "?".to_string())
            })
            .collect();
        return Err(DutError::HasGenerics {
            module: module.to_string(),
            count: names.len(),
            names: names.join(", "),
        });
    }

    // Guard 2: parameters. Only `param` counts. A `const` cannot be
    // overridden, so its default value is the real value.
    let params: Vec<String> = property
        .parameters
        .iter()
        .filter(|parameter| parameter.property().kind == ParameterKind::Param)
        .map(|parameter| format!("{}", parameter.name))
        .collect();
    if !params.is_empty() {
        return Err(DutError::HasParameters {
            module: module.to_string(),
            count: params.len(),
            names: params.join(", "),
        });
    }

    let file = token_file(&symbol);
    let ir_module = find_ir_module(ir, module, &file).ok_or_else(|| DutError::NoIr {
        module: module.to_string(),
    })?;

    let ports = property
        .ports
        .iter()
        .map(|port| {
            let name = format!("{}", port.name());
            let signals = signals_of(ir_module, &name);
            let axi4 = axi4_of(module, ir_module, &name)?;
            Ok(Port {
                direction: PortDirection::from(&port.property().direction),
                name,
                signals,
                axi4,
            })
        })
        .collect::<Result<Vec<_>, DutError>>()?;

    Ok(Dut {
        name: module.to_string(),
        file,
        line: symbol.token.line,
        ports,
    })
}

/// Lists the ports the IR shows, for a human to read. It is for checking the
/// analyzer contract by eye, not for generation (`examples/dump_ports.rs`).
///
/// It needs only `Veryl.toml`, unlike `check`. After a Veryl upgrade, it shows
/// whether the IR still gives widths and domains, without a manifest or
/// bundles. `tests/dut_resolve.rs` keeps it working.
pub fn describe_ports(ir: &Ir, filter: Option<&str>) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let _ = writeln!(out, "components: {}", ir.components.len());

    for component in &ir.components {
        let name = match component {
            Component::Module(x) => format!("{}", x.name),
            Component::Interface(_) => continue,
            Component::SystemVerilog(_) => {
                // A SystemVerilog blackbox. Show that the IR keeps it as a
                // separate kind.
                let _ = writeln!(out, "[systemverilog component]");
                continue;
            }
        };
        if let Some(f) = filter
            && name != f
        {
            continue;
        }
        let Component::Module(module) = component else {
            continue;
        };

        // The IR holds a parameterized module only with its defaults, so the
        // parameters must be visible here.
        let params: Vec<String> = module
            .variables
            .values()
            .filter(|var| var.kind.is_param())
            .map(|var| format!("{}", var.path))
            .collect();

        let _ = writeln!(
            out,
            "\nmodule {name}  ({} ports, {} params{})",
            module.port_types.len(),
            params.len(),
            if params.is_empty() {
                String::new()
            } else {
                format!(": {}", params.join(", "))
            },
        );

        // A HashMap has no fixed order. Sort by name to make it readable.
        let mut ports: Vec<_> = module.port_types.iter().collect();
        ports.sort_by_key(|(path, _)| format!("{path}"));

        for (path, (ty, domain)) in ports {
            let dir = module
                .ports
                .get(path)
                .and_then(|id| module.variables.get(id))
                .map(|var| var.kind.description())
                .unwrap_or_else(|| "?".to_string());

            // `std::axi4_if` is known by name. Its widths come from the generic
            // arguments on the port; show them, so it is clear they were read.
            let axi4 = match axi4_of(&name, module, &format!("{path}")) {
                Ok(axi4) => axi4,
                Err(err) => {
                    let _ = writeln!(out, "  {:<16} {:<8} axi4 ({err})", format!("{path}"), dir);
                    continue;
                }
            };
            if let Some(axi4) = axi4 {
                let _ = writeln!(
                    out,
                    "  {:<16} {:<8} axi4 {} addr={} data={}bit id={}",
                    format!("{path}"),
                    dir,
                    axi4.modport,
                    axi4.addr_width(),
                    axi4.data_width(),
                    axi4.id_width()
                );
                continue;
            }
            let width = match ty.total_width() {
                Some(w) => w.to_string(),
                None => "UNRESOLVED".to_string(),
            };
            let array = match ty.total_array() {
                Some(1) => String::new(),
                Some(n) => format!("[{n}]"),
                None => "[UNRESOLVED]".to_string(),
            };
            let domain = match format!("{domain}").as_str() {
                "" => "-".to_string(),
                s => s.to_string(),
            };

            let _ = writeln!(
                out,
                "  {:<16} {:<8} width={:<10}{:<8} domain={:<8} kind={:?}",
                format!("{path}"),
                dir,
                width,
                array,
                domain,
                ty.kind,
            );
        }
    }
    out
}

/// The names of the `$sv::` blackboxes used in the DUT, including its
/// submodules.
///
/// The generator does not read them. Whether their Verilog reaches synthesis
/// depends on `include(inline, "...")` in the DUT. If it is not in the
/// `veryl build` file list, only synthesis fails. `check` lists these names
/// under `not_checked`.
pub fn sv_blackboxes(ir: &Ir, dut: &Dut) -> Vec<String> {
    fn walk(module: &IrModule, found: &mut std::collections::BTreeSet<String>) {
        for declaration in &module.declarations {
            let veryl_analyzer::ir::Declaration::Inst(inst) = declaration else {
                continue;
            };
            match inst.component.as_ref() {
                Component::SystemVerilog(sv) => {
                    found.insert(text_of(&sv.name));
                }
                Component::Module(sub) => walk(sub, found),
                Component::Interface(_) => {}
            }
        }
    }
    let mut found = std::collections::BTreeSet::new();
    if let Some(module) = find_ir_module(ir, &dut.name, &dut.file) {
        walk(module, &mut found);
    }
    found.into_iter().collect()
}

/// Matches a symbol-table module to an IR module by name and file. The IR
/// `Module` has a name but no namespace, so the file narrows it down.
fn find_ir_module<'a>(ir: &'a Ir, module: &str, file: &PathBuf) -> Option<&'a IrModule> {
    ir.components.iter().find_map(|component| {
        let Component::Module(x) = component else {
            return None;
        };
        let same_name = format!("{}", x.name) == module;
        let same_file = source_path(&x.token.beg.source).as_ref() == Some(file);
        (same_name && same_file).then_some(x)
    })
}

/// If the port is a modport of `std::axi4_if`, returns its widths and modport.
///
/// Interfaces do not appear in the IR, so the widths come from the generic
/// arguments on the port, not from the interface body. It walks the
/// `GenericSymbolPath` structure instead of splitting the name as text.
///
/// If the port is a `$std::axi4_if` but its arguments cannot be read, this is
/// an error. Returning `None` would pass on a modport without widths, and later
/// checks would reject it for the wrong reason.
fn axi4_of(dut: &str, module: &IrModule, port_name: &str) -> Result<Option<Axi4Port>, DutError> {
    use veryl_analyzer::ir::TypeKind;

    let Some((_, (ty, _))) = module.port_types.iter().find(|(path, _)| {
        path.0
            .first()
            .is_some_and(|head| format!("{head}") == port_name)
    }) else {
        return Ok(None);
    };
    let TypeKind::Modport(sig, modport) = &ty.kind else {
        return Ok(None);
    };

    // Only `$std::axi4_if` is known. Check that it comes from std, so that a
    // user interface with the same name is not mistaken for it.
    let head = sig.full_path.first().map(text_of);
    let last = sig.full_path.last().map(text_of);
    if head.as_deref() != Some("$std") || !last.is_some_and(|last| last.contains("axi4_if")) {
        return Ok(None);
    }
    let unreadable = |why: &'static str| DutError::Axi4ArgsUnreadable {
        module: dut.to_string(),
        port: port_name.to_string(),
        why,
    };

    // There is one generic argument (`PKG`). Read the arguments of the
    // `axi4_pkg::<..>` inside it.
    let (_, path) = sig
        .generic_parameters
        .first()
        .ok_or_else(|| unreadable("it has no package argument"))?;
    let pkg = path
        .paths
        .iter()
        .find(|p| format!("{}", p.base.text).contains("axi4_pkg"))
        .ok_or_else(|| unreadable("its argument is not a `$std::axi4_pkg::<..>`"))?;
    let args: Vec<u32> = pkg
        .arguments
        .iter()
        .map(|arg| {
            arg.paths
                .first()
                .and_then(|p| format!("{}", p.base.text).parse::<u32>().ok())
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| unreadable("an argument of `axi4_pkg` is not a number"))?;
    // `axi4_pkg::<ADDR_W, DATA_W_BYTES, ID_W, AWUSER_W, WUSER_W, BUSER_W,
    //             ARUSER_W, RUSER_W>` has no defaults, so all 8 are present.
    if args.len() != 8 {
        return Err(unreadable("`axi4_pkg` does not have its 8 arguments"));
    }

    Ok(Some(Axi4Port {
        modport: text_of(modport),
        args,
    }))
}

fn text_of(id: &veryl_parser::resource_table::StrId) -> String {
    resource_table::get_str_value(*id).unwrap_or_default()
}

/// Collects the IR signals of one declared port.
///
/// An interface port expands in `port_types` (for example `i_bus.valid`), so
/// every path whose first element matches is taken. None found gives an empty
/// list: width and domain unknown.
fn signals_of(module: &IrModule, port_name: &str) -> Vec<Signal> {
    let mut signals: Vec<Signal> = module
        .port_types
        .iter()
        .filter(|(path, _)| {
            path.0
                .first()
                .is_some_and(|head| format!("{head}") == port_name)
        })
        .map(|(path, (ty, domain))| Signal {
            path: format!("{path}"),
            role: role_of(ty),
            type_text: format!("{ty}"),
            width: ty.total_width(),
            array: ty.total_array(),
            domain: Domain::from(domain),
        })
        .collect();

    // `port_types` is a HashMap. Sort, or the order is not deterministic.
    signals.sort_by(|a, b| a.path.cmp(&b.path));
    signals
}

fn role_of(ty: &veryl_analyzer::ir::Type) -> SignalRole {
    use veryl_analyzer::ir::TypeKind;
    match ty.kind {
        TypeKind::Clock | TypeKind::ClockPosedge | TypeKind::ClockNegedge => SignalRole::Clock,
        TypeKind::Reset
        | TypeKind::ResetAsyncHigh
        | TypeKind::ResetAsyncLow
        | TypeKind::ResetSyncHigh
        | TypeKind::ResetSyncLow => SignalRole::Reset,
        _ => SignalRole::Data,
    }
}

fn token_file(symbol: &veryl_analyzer::symbol::Symbol) -> PathBuf {
    source_path(&symbol.token.source).unwrap_or_default()
}

fn source_path(source: &TokenSource) -> Option<PathBuf> {
    match source {
        TokenSource::File { path, .. } => Some(PathBuf::from(format!("{path}"))),
        _ => None,
    }
}

/// A bullet list for an error message. A long list is cut, because a message
/// that fills the screen is not read.
pub(crate) fn bullet_list(items: &[String]) -> String {
    if items.is_empty() {
        return "    (none)".to_string();
    }

    let shown: Vec<String> = items
        .iter()
        .take(MAX_CANDIDATES)
        .map(|item| format!("    {item}"))
        .collect();
    let mut text = shown.join("\n");
    if items.len() > MAX_CANDIDATES {
        text.push_str(&format!(
            "\n    ... and {} more",
            items.len() - MAX_CANDIDATES
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bullet_list_truncates() {
        let items: Vec<String> = (0..MAX_CANDIDATES + 5).map(|i| format!("m{i}")).collect();
        let text = bullet_list(&items);

        assert!(text.contains("    m0"));
        assert!(text.contains("... and 5 more"));
        assert!(!text.contains("    m24"));
    }

    #[test]
    fn bullet_list_says_none_rather_than_nothing() {
        assert_eq!(bullet_list(&[]), "    (none)");
    }

    /// An inferred domain must differ from an explicit one. If they mix, a later
    /// step could put a false path on an inferred domain.
    #[test]
    fn inferred_domain_is_labelled_differently_from_explicit() {
        let explicit = Domain::Explicit("clk".to_string());
        let inferred = Domain::Inferred("clk".to_string());

        assert_ne!(explicit, inferred);
        assert_ne!(explicit.label(), inferred.label());
        assert!(inferred.label().contains("inferred"));
    }
}

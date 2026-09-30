//! The output of `check --json`.
//!
//! Agents drive this tool in a loop: write TOML, run `check`, read the error,
//! fix it, run again. The JSON makes that loop easy to parse:
//!
//! - Success and failure both print one JSON document on stdout. The exit code
//!   is 0 / 1, as for humans.
//! - The error is the main output. It has `code`, `message` and `help`, the
//!   position (`file` / `line` / `column`), and the `related` diagnostics that
//!   Veryl groups together.
//! - `checked` and `not_checked` are always present. They are the machine
//!   form of "NOT CHECKED YET", so exit 0 is not read as "the harness works".
//!
//! ## The wire format is separate from the internal types
//!
//! Internal types such as `dut::Port` do not derive `Serialize`. If they did,
//! renaming a field would silently break the external contract. With separate
//! types here, every schema change is an edit to this file. `format_version`
//! is the version number for it.

use miette::Diagnostic;
use serde::Serialize;

use crate::dut::{Domain, SignalRole};
use crate::regmap::{self, RegisterMap};
use crate::unconnected::Kind;

/// The schema version.
///
/// - 1: first version
/// - 2: `bundles[].contract` is the resolved value (never `null`); added
///   `contract_declared`, `roles` and `ports[].bundle_role`
/// - 3: added `ports[].unconnected`
/// - 4: added `target`
/// - 5: added `feasibility`
/// - 6: added `registers`
/// - 7: added `clocks`
/// - 8: added `written`
/// - 9: added `bundles[].fifo`, `registers[].role` and
///   `registers[].self_clearing.from`
/// - 10: added `bundles[].memory`
/// - 11: added `bundles[].host_mem` / `slave` / `axi_mem`, `heartbeat` and
///   `registers.target_source`; `gen --json` has `checked` / `not_checked`
/// - 12: added `registers.window_clock_mhz` / `window_cycles`
/// - 13: removed `checked[].reference` / `not_checked[].reference`
pub const FORMAT_VERSION: u32 = 13;

/// How deep `related` is followed. Veryl diagnostics can nest.
const MAX_RELATED_DEPTH: usize = 3;

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Error,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub format_version: u32,
    pub status: Status,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<Project>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dut: Option<DutOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundles: Option<Vec<BundleOutput>>,

    /// Present only with `--target` / `--target-file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetOutput>,

    /// The feasibility verdict. Present only with a target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feasibility: Option<FeasibilityOutput>,

    /// The files `gen` wrote. Empty for `check`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub written: Vec<WrittenOutput>,

    /// The heartbeat UART. Present only with `[heartbeat]` and a target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<HeartbeatOutput>,

    /// The clock plan. Present only with a target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clocks: Option<ClockOutput>,

    /// The register map. The same content that `--emit-regs` writes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registers: Option<RegisterMapOutput>,

    /// What was checked, and what was not checked yet. Always present on
    /// success.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checked: Vec<Item>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_checked: Vec<Item>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorOutput>,
}

#[derive(Debug, Serialize)]
pub struct Project {
    pub name: String,
    pub manifest: String,
}

/// One checked or not-checked item.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: &'static str,
    /// Usually a constant. Only items that depend on the DUT (`$sv::` names)
    /// are built at run time.
    pub what: std::borrow::Cow<'static, str>,
}

impl Item {
    pub fn new(id: &'static str, what: &'static str) -> Self {
        Self {
            id,
            what: what.into(),
        }
    }

    /// An item whose text is built at run time.
    pub fn owned(id: &'static str, what: String) -> Self {
        Self {
            id,
            what: what.into(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DutOutput {
    pub module: String,
    pub file: String,
    pub line: u32,
    pub ports: Vec<PortOutput>,
}

#[derive(Debug, Serialize)]
pub struct PortOutput {
    pub name: String,
    pub direction: &'static str,

    /// The bundle the port is in. `null` for clock and reset, which the clock
    /// plan drives.
    pub bundle: Option<String>,

    /// The role in the bundle. `null` for a port outside a bundle.
    pub bundle_role: Option<&'static str>,

    /// The termination from `[tie_off]` / `[leave_open]` / `[pin]`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unconnected: Option<UnconnectedOutput>,

    /// The role was written inverted (`ready = "!o_full"`).
    pub invert: bool,

    pub signals: Vec<SignalOutput>,
}

#[derive(Debug, Serialize)]
pub struct SignalOutput {
    pub path: String,
    pub role: &'static str,
    pub r#type: String,

    /// `null` if it could not be resolved. Never filled with 0 or 1.
    pub width: Option<usize>,
    pub array: Option<usize>,

    pub clock_domain: DomainOutput,
}

/// A clock domain. `inferred` is never merged into `explicit`: a false path on
/// an inferred crossing hides CDC bugs, so downstream tools must see the
/// difference.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DomainOutput {
    Explicit { name: String },
    Inferred { name: String },
    Implicit,
    None,
}

/// The termination of a port outside any bundle.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnconnectedOutput {
    /// An input driven by a constant. `value` is as written (`0x3f`, ...).
    TieOff { value: String },
    /// An output left unconnected.
    LeaveOpen,
    /// An output on a board pin. `pin` / `standard` are set only when a
    /// target is given.
    Pin {
        resource: String,
        pin: Option<String>,
        standard: Option<String>,
    },
}

#[derive(Debug, Serialize)]
pub struct BundleOutput {
    pub name: String,
    pub backing: &'static str,

    /// The resolved contract. If none was written, it is inferred from the
    /// ports.
    pub contract: &'static str,

    /// Whether the manifest wrote the contract. `false` means it was inferred
    /// from the ports; it is shown so it does not look like a silent default.
    pub contract_declared: bool,

    pub latency: Option<u32>,

    /// `explicit` (named in `ports`) or `naming` (the naming rule).
    pub matched_by: &'static str,
    pub ports: Vec<String>,

    /// The role of each port. `source` is `explicit` / `dictionary` / `payload`.
    pub roles: Vec<RoleOutput>,

    /// The `host_poll_fifo` terminator. Absent for other backings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fifo: Option<FifoOutput>,

    /// The fixed-latency `bram` / `bram_preload` terminator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryOutput>,

    /// The terminator of a transfer-level port (`rd_*` / `wr_*`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_mem: Option<HostMemOutput>,

    /// An addressable port inside the DUT (`slave`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slave: Option<SlaveOutput>,

    /// The memory a DUT AXI4 master reaches (`bram` / `dram`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub axi_mem: Option<AxiMemOutput>,
}

/// What a transfer-level terminator contains.
#[derive(Debug, Serialize)]
pub struct HostMemOutput {
    pub depth: u32,
    /// Whether `depth` was written. `false` means the default was used.
    pub depth_declared: bool,
    pub data_width: usize,
    /// Has a read side (`rd_*`).
    pub read: bool,
    /// Has a write side (`wr_*`).
    pub write: bool,
}

/// A `slave` port. The harness drives `addr` and reads `rdata` after
/// `latency` cycles.
#[derive(Debug, Serialize)]
pub struct SlaveOutput {
    pub addr_width: usize,
    pub width: usize,
    /// The window can write it (`wdata` and `we` exist).
    pub writable: bool,
    pub latency: u32,
    /// Its size in the window, in bytes.
    pub size_bytes: usize,
}

/// An AXI4 memory.
#[derive(Debug, Serialize)]
pub struct AxiMemOutput {
    pub port: String,
    pub addr_width: u32,
    pub data_bytes: u32,
    pub id_width: u32,
    pub depth: u32,
    /// Whether `depth` was written. `false` means the default (`bram`) or the
    /// whole board memory (`dram`).
    pub depth_declared: bool,
    /// The size shown in the host window. `None` means the whole memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aperture_bytes: Option<usize>,
}

/// The heartbeat UART.
#[derive(Debug, Serialize)]
pub struct HeartbeatOutput {
    pub resource: String,
    pub pin: String,
    pub standard: String,
    /// The written baud rate, and the rate the divider really gives.
    pub baud: u64,
    pub actual_baud: u64,
    pub clocks_per_bit: u64,
}

/// What a memory terminator contains, including where the depth came from.
#[derive(Debug, Serialize)]
pub struct MemoryOutput {
    pub depth: u64,
    /// `depth` was not written and comes from the address width.
    pub depth_from_addr_width: bool,
    pub width: usize,
    pub latency: u32,
    /// The DUT only reads (`bram_preload`).
    pub read_only: bool,
    /// Out-of-range accesses are counted (`depth < 2^addr`).
    pub out_of_range_counter: bool,
}

/// What a `host_poll_fifo` contains. A default depth is shown, not hidden.
#[derive(Debug, Serialize)]
pub struct FifoOutput {
    pub depth: u32,
    /// Whether `depth` was written. `false` means the default was used.
    pub depth_declared: bool,
    /// Dropped beats are counted.
    pub drop_counter: bool,
    pub width: usize,
}

#[derive(Debug, Serialize)]
pub struct RoleOutput {
    pub role: &'static str,
    pub port: String,
    pub invert: bool,
    pub source: &'static str,
}

/// The resolved target.
#[derive(Debug, Serialize)]
pub struct TargetOutput {
    pub name: String,
    /// `targets` / `targets-private` / `file`.
    pub source: &'static str,
    pub path: String,

    /// Whether CI tests this configuration. `false` with a patch or a file.
    pub verified: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unverified_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<String>,

    /// The description itself, after config and patches are applied. Its
    /// schema is not fixed yet, so it is output untyped.
    pub description: serde_json::Value,
}

/// The feasibility verdict. Passing is not a guarantee: see `requirements`.
#[derive(Debug, Serialize)]
pub struct FeasibilityOutput {
    pub transport: String,
    pub bundles: Vec<VerdictOutput>,
}

#[derive(Debug, Serialize)]
pub struct VerdictOutput {
    pub bundle: String,
    pub backing: &'static str,
    pub contract: &'static str,

    /// Conditions for passing. `drop_counter`: a lossy terminator must count
    /// what it loses.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<RequirementOutput>,
}

#[derive(Debug, Serialize)]
pub struct RequirementOutput {
    pub id: &'static str,
    pub why: String,
}

/// One file `gen` wrote.
#[derive(Debug, Serialize)]
pub struct WrittenOutput {
    pub path: String,
    pub what: &'static str,
}

/// The clock plan. It has no M/D/O: Vivado solves them.
#[derive(Debug, Serialize)]
pub struct ClockOutput {
    pub input: InputClockOutput,
    pub outputs: Vec<ClockOutputEntry>,
}

#[derive(Debug, Serialize)]
pub struct InputClockOutput {
    pub name: String,
    pub freq_mhz: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClockOutputEntry {
    /// The DUT ports this clock drives. Several ports if they share a domain.
    pub ports: Vec<String>,
    pub freq_mhz: f64,
    /// Why they were grouped: a domain such as `'s`, or `-` if not grouped.
    pub domain: String,
}

/// The register map. It is identical to `regs.json`, so there is only one
/// true form.
#[derive(Debug, Serialize)]
pub struct RegisterMapOutput {
    /// Marks the file as generated. `gen` does not overwrite a file without
    /// it, so this lets `gen` rewrite its own output (`src/generate.rs`).
    pub marker: &'static str,
    pub format_version: u32,
    pub generator: &'static str,
    pub dut: String,

    /// The target it was generated for (`digilent/arty-a7-35`, ...).
    ///
    /// It lets the host omit `--target`: the map belongs to one bitstream, and
    /// the target is part of that. Written only for a target given by name. A
    /// `--target-file` path depends on the machine, and a later change to the
    /// file would go unnoticed, so it is not recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,

    /// How the target was given: `"name"` (`--target`) or `"file"`
    /// (`--target-file`). Absent in a map made without a target.
    ///
    /// With `"file"`, the host does not look for the description. There is no
    /// `target`, so it stops and asks for `--target-file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_source: Option<&'static str>,

    /// The window clock frequency (MHz). Present only with a target.
    ///
    /// A JTAG host uses it with `window_cycles` to choose how many TCKs to wait
    /// between scans (`update_gap` in `hns-host`). Without it, it does not wait.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_clock_mhz: Option<f64>,

    /// Window clock cycles the bridge needs per command
    /// (`regmap::WINDOW_CYCLES`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_cycles: Option<u32>,

    /// Bits per word. Word `i` is bits `[32i+31 : 32i]`.
    pub word_bits: usize,

    /// The window size in bytes.
    pub size_bytes: usize,

    /// The identity header constant (at the end of the window) and the map
    /// hash. The host reads both to check that its map matches the loaded
    /// bitstream.
    pub magic: u32,
    pub map_hash: u32,

    pub registers: Vec<RegisterOutput>,

    /// Contiguous ranges in the window. Empty for a design without them, so
    /// such maps look the same as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<RegionOutput>,

    /// Only for a PCIe design. The host needs it to find the card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pcie: Option<PcieOutput>,
}

/// The IDs the bitstream reports, and the BAR size.
///
/// The host selects cards by ID and detects a wrong card by BAR size. The
/// default IDs are borrowed from an example and can clash with others, so
/// that is not enough: the host also reads the magic before any write.
#[derive(Debug, Serialize)]
pub struct PcieOutput {
    pub vendor_id: u32,
    pub device_id: u32,
    pub bar_bytes: u32,
    /// The bundle the card's DMA engine reaches. Used on any other bundle, it
    /// would access the wrong memory, so the host uses it only on a match.
    /// Absent if there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterOutput {
    pub name: String,
    /// `header` (placed by the generator), `port` (a DUT port) or `terminator`
    /// (from a terminator).
    pub kind: &'static str,
    pub offset: usize,
    pub words: usize,
    pub width: usize,
    /// `rw` (the host writes it; a DUT input) or `ro` (the host reads it; a
    /// DUT output).
    pub access: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    /// Only for fixed values (magic, map_hash, the `host_poll_fifo` depth).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<u32>,

    /// The purpose of a terminator register (`data` / `level` / `depth` /
    /// `drops` / `pop`). The host reads meaning from this, not from the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,

    /// A register where one write is exactly one beat. It returns to 0 when
    /// its partner (ready / ack) is high, so the host can read it to see
    /// whether the beat was taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_clearing: Option<SelfClearingOutput>,
}

#[derive(Debug, Serialize)]
pub struct SelfClearingOutput {
    /// The partner's name (a DUT port, or a terminator register).
    pub on: String,
    /// Where the partner is (`dut_port` / `terminator`).
    pub from: &'static str,
    /// Its role (`ready` / `ack`).
    pub role: &'static str,
    /// The partner is declared inverted (`ready = "!o_full"`).
    pub invert: bool,
}

/// A contiguous range in the window. Addresses count from 0 inside it; the
/// host adds `base`.
#[derive(Debug, Serialize)]
pub struct RegionOutput {
    pub name: String,
    /// `memory` (harness storage; accesses are idempotent) or `dut` (a DUT
    /// slave port).
    pub kind: &'static str,
    pub base: usize,
    pub size_bytes: usize,
    pub entry_bytes: usize,
    pub depth: u64,
    /// The whole memory in bytes. If it is larger than `size_bytes`, the range
    /// is a moving window, and the `<name>_base_<master>` register sets where
    /// it looks.
    pub total_bytes: u64,
}

/// Used by both `regs.json` and `check --json`.
pub fn register_map(
    map: &RegisterMap,
    target: Option<&crate::target::Target>,
    clocks: Option<&crate::clock::ClockPlan>,
    pcie: Option<&crate::manifest::Pcie>,
) -> RegisterMapOutput {
    let from_file = target.is_some_and(|t| matches!(t.source, crate::target::Source::File { .. }));
    RegisterMapOutput {
        marker: crate::generate::MARKER,
        format_version: regmap::FORMAT_VERSION,
        generator: "veryl-harness",
        dut: map.dut.clone(),
        target: target.filter(|_| !from_file).map(|t| t.name.clone()),
        target_source: target.map(|_| if from_file { "file" } else { "name" }),
        window_clock_mhz: clocks.map(|clocks| clocks.window_output().freq_mhz),
        window_cycles: clocks.map(|_| regmap::WINDOW_CYCLES),
        word_bits: regmap::WORD_BITS,
        size_bytes: map.size_bytes(),
        magic: regmap::MAGIC,
        map_hash: map.map_hash,
        pcie: pcie.map(|pcie| PcieOutput {
            vendor_id: pcie.vendor_id,
            device_id: pcie.device_id,
            bar_bytes: pcie.bar_bytes,
            requester: map.requester.clone(),
        }),
        registers: map
            .registers
            .iter()
            .map(|register| RegisterOutput {
                name: register.name.clone(),
                kind: register.kind.as_str(),
                offset: register.offset,
                words: register.words,
                width: register.width,
                access: register.access.as_str(),
                bundle: register.bundle.clone(),
                value: register.value,
                role: register.role,
                self_clearing: register
                    .self_clearing
                    .as_ref()
                    .map(|clear| SelfClearingOutput {
                        on: clear.on().to_string(),
                        from: clear.from(),
                        role: clear.role,
                        invert: clear.invert,
                    }),
            })
            .collect(),
        regions: map
            .regions
            .iter()
            .map(|region| RegionOutput {
                name: region.bundle.clone(),
                kind: region.kind.as_str(),
                base: region.base,
                size_bytes: region.size_bytes,
                entry_bytes: region.entry_bytes,
                depth: region.depth,
                total_bytes: region.total_bytes,
            })
            .collect(),
    }
}

/// The text written to `regs.json`.
pub fn render_register_map(
    map: &RegisterMap,
    target: Option<&crate::target::Target>,
    clocks: Option<&crate::clock::ClockPlan>,
    pcie: Option<&crate::manifest::Pcie>,
) -> String {
    serde_json::to_string_pretty(&register_map(map, target, clocks, pcie)).unwrap_or_default()
}

#[derive(Debug, Serialize)]
pub struct ErrorOutput {
    /// The miette diagnostic code (`harness::bundle::empty_bundle`, ...).
    /// Agents branch on this, so it comes before the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<LabelOutput>,
    /// Veryl groups several diagnostics in one report; they are listed here.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<ErrorOutput>,
}

#[derive(Debug, Serialize)]
pub struct LabelOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Starts at 1, as in editors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    pub offset: usize,
    pub length: usize,
}

// ---------------------------------------------------------------------------
// Building
// ---------------------------------------------------------------------------

/// The output on success.
#[allow(clippy::too_many_arguments)]
pub fn ok(
    project: Project,
    plan: &crate::plan::Plan,
    checked: Vec<Item>,
    not_checked: Vec<Item>,
) -> Output {
    let (loaded, dut, bindings, contracts) =
        (&plan.loaded, &plan.dut, &plan.bindings, &plan.contracts);
    let (fifos, memories, unconnected) = (&plan.fifos, &plan.memories, &plan.unconnected);
    let (target, feasibility, clocks) = (plan.target(), plan.feasibility(), plan.clocks());
    let registers = &plan.registers;
    let owner: std::collections::HashMap<&str, &str> = bindings
        .iter()
        .flat_map(|binding| {
            binding
                .ports
                .iter()
                .map(|port| (port.as_str(), binding.bundle.as_str()))
        })
        .collect();

    // Port name -> role, so each port can show its contract role.
    let roles: std::collections::HashMap<&str, (&'static str, bool)> = contracts
        .iter()
        .flat_map(|contract| {
            contract.roles.iter().map(|assignment| {
                (
                    assignment.port.as_str(),
                    (assignment.role.as_str(), assignment.invert),
                )
            })
        })
        .collect();

    let ports = dut
        .ports
        .iter()
        .map(|port| PortOutput {
            name: port.name.clone(),
            direction: port.direction.as_str(),
            bundle: owner.get(port.name.as_str()).map(|x| (*x).to_string()),
            bundle_role: roles.get(port.name.as_str()).map(|(role, _)| *role),
            unconnected: unconnected
                .iter()
                .find(|entry| entry.port == port.name)
                .map(|entry| match &entry.kind {
                    Kind::Tie(value) => UnconnectedOutput::TieOff {
                        value: value.text.clone(),
                    },
                    Kind::Open => UnconnectedOutput::LeaveOpen,
                    Kind::Pin {
                        resource,
                        pin,
                        standard,
                    } => UnconnectedOutput::Pin {
                        resource: resource.clone(),
                        pin: pin.clone(),
                        standard: standard.clone(),
                    },
                }),
            invert: roles
                .get(port.name.as_str())
                .is_some_and(|(_, invert)| *invert),
            signals: port
                .signals
                .iter()
                .map(|signal| SignalOutput {
                    path: signal.path.clone(),
                    role: role_str(signal.role),
                    r#type: signal.type_text.clone(),
                    width: signal.width,
                    array: signal.array,
                    clock_domain: domain_output(&signal.domain),
                })
                .collect(),
        })
        .collect();

    let bundles = bindings
        .iter()
        .map(|binding| {
            let bundle = &loaded.manifest.bundle[&binding.bundle];
            let resolved = contracts
                .iter()
                .find(|contract| contract.bundle == binding.bundle);
            BundleOutput {
                name: binding.bundle.clone(),
                backing: bundle.backing.as_str(),
                contract: resolved
                    .map(|resolved| resolved.contract.as_str())
                    .unwrap_or("?"),
                contract_declared: resolved.is_some_and(|resolved| resolved.declared),
                latency: bundle.latency,
                matched_by: binding.how.as_str(),
                ports: binding.ports.clone(),
                roles: resolved
                    .map(|resolved| {
                        resolved
                            .roles
                            .iter()
                            .map(|assignment| RoleOutput {
                                role: assignment.role.as_str(),
                                port: assignment.port.clone(),
                                invert: assignment.invert,
                                source: assignment.source.as_str(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                fifo: fifos
                    .iter()
                    .find(|fifo| fifo.bundle == binding.bundle)
                    .map(|fifo| FifoOutput {
                        depth: fifo.depth,
                        depth_declared: !fifo.depth_defaulted,
                        drop_counter: fifo.counts_drops(),
                        width: fifo.width,
                    }),
                memory: memories
                    .iter()
                    .find(|mem| mem.bundle == binding.bundle)
                    .map(|mem| MemoryOutput {
                        depth: mem.depth,
                        depth_from_addr_width: mem.depth_from_addr,
                        width: mem.width,
                        latency: mem.latency,
                        read_only: mem.read_only,
                        out_of_range_counter: mem.counts_out_of_range(),
                    }),
                host_mem: plan
                    .host_mems
                    .iter()
                    .find(|mem| mem.bundle == binding.bundle)
                    .map(|mem| HostMemOutput {
                        depth: mem.depth,
                        depth_declared: !mem.depth_defaulted,
                        data_width: mem.data_width,
                        read: mem.read.is_some(),
                        write: mem.write.is_some(),
                    }),
                slave: plan
                    .slaves
                    .iter()
                    .find(|slave| slave.bundle == binding.bundle)
                    .map(|slave| SlaveOutput {
                        addr_width: slave.addr_width,
                        width: slave.width,
                        writable: slave.wdata.is_some(),
                        latency: slave.latency,
                        size_bytes: slave.size_bytes(),
                    }),
                axi_mem: plan
                    .axi_mems
                    .iter()
                    .find(|mem| mem.bundle == binding.bundle)
                    .map(|mem| AxiMemOutput {
                        port: mem.port.clone(),
                        addr_width: mem.addr_width,
                        data_bytes: mem.data_bytes,
                        id_width: mem.id_width,
                        depth: mem.depth,
                        depth_declared: !mem.depth_defaulted,
                        aperture_bytes: mem.aperture_bytes,
                    }),
            }
        })
        .collect();

    Output {
        format_version: FORMAT_VERSION,
        status: Status::Ok,
        manifest: Some(loaded.path.display().to_string()),
        project: Some(project),
        dut: Some(DutOutput {
            module: dut.name.clone(),
            file: dut.file.display().to_string(),
            line: dut.line,
            ports,
        }),
        bundles: Some(bundles),
        target: target.map(|target| TargetOutput {
            name: target.name.clone(),
            source: target.source.as_str(),
            path: target.source.path(),
            verified: target.verified(),
            unverified_reasons: target.unverified_reasons(),
            patches: target
                .patches
                .iter()
                .map(|patch| patch.display().to_string())
                .collect(),
            description: serde_json::to_value(&target.table).unwrap_or(serde_json::Value::Null),
        }),
        feasibility: feasibility.map(|feasibility| FeasibilityOutput {
            transport: feasibility.transport.clone(),
            bundles: feasibility
                .bundles
                .iter()
                .map(|verdict| VerdictOutput {
                    bundle: verdict.bundle.clone(),
                    backing: verdict.backing.as_str(),
                    contract: verdict.contract.as_str(),
                    requirements: verdict
                        .requirements
                        .iter()
                        .map(|requirement| RequirementOutput {
                            id: requirement.id(),
                            why: requirement.why().to_string(),
                        })
                        .collect(),
                })
                .collect(),
        }),
        written: Vec::new(),
        heartbeat: plan.heartbeat.as_ref().map(|heartbeat| HeartbeatOutput {
            resource: heartbeat.resource.clone(),
            pin: heartbeat.pin.clone(),
            standard: heartbeat.standard.clone(),
            baud: loaded
                .manifest
                .heartbeat
                .as_ref()
                .map(|config| config.baud)
                .unwrap_or(heartbeat.actual_baud),
            actual_baud: heartbeat.actual_baud,
            clocks_per_bit: heartbeat.div,
        }),
        clocks: clocks.map(|plan| ClockOutput {
            input: InputClockOutput {
                name: plan.input.name.clone(),
                freq_mhz: plan.input.freq_mhz,
                pin: plan.input.pin.clone(),
                standard: plan.input.standard.clone(),
            },
            outputs: plan
                .outputs
                .iter()
                .map(|output| ClockOutputEntry {
                    ports: output.ports.clone(),
                    freq_mhz: output.freq_mhz,
                    domain: output.domain.clone(),
                })
                .collect(),
        }),
        registers: Some(register_map(
            registers,
            target,
            clocks,
            crate::plan::pcie_identity(&loaded.manifest, feasibility.map(|f| f.transport.as_str()))
                .as_ref(),
        )),
        checked,
        not_checked,
        error: None,
    }
}

/// The output on failure.
pub fn error(report: &miette::Report) -> Output {
    Output {
        format_version: FORMAT_VERSION,
        status: Status::Error,
        manifest: None,
        project: None,
        dut: None,
        bundles: None,
        target: None,
        feasibility: None,
        written: Vec::new(),
        heartbeat: None,
        clocks: None,
        registers: None,
        checked: Vec::new(),
        not_checked: Vec::new(),
        error: Some(diagnostic_output(&**report, 0)),
    }
}

fn diagnostic_output(diagnostic: &dyn Diagnostic, depth: usize) -> ErrorOutput {
    let mut labels = Vec::new();
    if let Some(spans) = diagnostic.labels() {
        let source = diagnostic.source_code();
        for span in spans {
            // The position is best effort. offset/length are always kept.
            let contents = source.and_then(|source| source.read_span(span.inner(), 0, 0).ok());
            labels.push(LabelOutput {
                label: span.label().map(str::to_string),
                file: contents
                    .as_ref()
                    .and_then(|contents| contents.name().map(str::to_string)),
                line: contents.as_ref().map(|contents| contents.line() + 1),
                column: contents.as_ref().map(|contents| contents.column() + 1),
                offset: span.offset(),
                length: span.len(),
            });
        }
    }

    let related = if depth >= MAX_RELATED_DEPTH {
        Vec::new()
    } else {
        diagnostic
            .related()
            .into_iter()
            .flatten()
            .map(|related| diagnostic_output(related, depth + 1))
            .collect()
    };

    ErrorOutput {
        code: non_empty(diagnostic.code().map(|code| code.to_string())),
        message: diagnostic.to_string(),
        help: non_empty(diagnostic.help().map(|help| help.to_string())),
        url: non_empty(diagnostic.url().map(|url| url.to_string())),
        labels,
        related,
    }
}

/// Treats an empty string as absent. Veryl diagnostics can have an empty help,
/// and `"help": ""` looks like help that says nothing.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn domain_output(domain: &Domain) -> DomainOutput {
    match domain {
        Domain::Explicit(name) => DomainOutput::Explicit { name: name.clone() },
        Domain::Inferred(name) => DomainOutput::Inferred { name: name.clone() },
        Domain::Implicit => DomainOutput::Implicit,
        Domain::None => DomainOutput::None,
    }
}

fn role_str(role: SignalRole) -> &'static str {
    match role {
        SignalRole::Clock => "clock",
        SignalRole::Reset => "reset",
        SignalRole::Data => "data",
    }
}

/// Renders the output. It is pretty-printed because people read it too, for
/// example when they copy a failure to reproduce it.
pub fn render(output: &Output) -> String {
    serde_json::to_string_pretty(output).unwrap_or_else(|err| {
        // Only our own types are serialized, so this should not fail. If it
        // does, report it as parseable JSON instead of a panic.
        format!(
            "{{\n  \"format_version\": {FORMAT_VERSION},\n  \"status\": \"error\",\n  \"error\": {{ \"message\": \"failed to serialize the report: {err}\" }}\n}}"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestError;

    #[test]
    fn an_error_carries_the_code_and_the_help() {
        let report = miette::Report::new(ManifestError::EmptyModule);
        let value: serde_json::Value = serde_json::from_str(&render(&error(&report))).unwrap();

        assert_eq!(value["status"], "error");
        assert_eq!(value["format_version"], FORMAT_VERSION);
        assert_eq!(value["error"]["code"], "harness::manifest::empty_module");
        // The help says how to fix it.
        assert!(
            value["error"]["help"]
                .as_str()
                .unwrap()
                .contains("no parameters")
        );
    }

    /// A diagnostic with a span carries its position. Agents use the line to
    /// find the fix.
    #[test]
    fn an_error_with_a_span_carries_the_position() {
        let src = "[dut]\nmodule = 1\n";
        let err = ManifestError::Parse {
            path: "Harness.toml".into(),
            src: miette::NamedSource::new("Harness.toml", src.to_string()),
            span: Some((15, 1).into()),
            message: "invalid type".to_string(),
        };
        let value: serde_json::Value =
            serde_json::from_str(&render(&error(&miette::Report::new(err)))).unwrap();

        let label = &value["error"]["labels"][0];
        assert_eq!(label["file"], "Harness.toml");
        assert_eq!(label["line"], 2);
        assert_eq!(label["offset"], 15);
    }

    /// Inferred and explicit domains stay different in JSON.
    #[test]
    fn an_inferred_domain_is_not_serialized_as_explicit() {
        let explicit =
            serde_json::to_value(domain_output(&Domain::Explicit("clk".into()))).unwrap();
        let inferred =
            serde_json::to_value(domain_output(&Domain::Inferred("clk".into()))).unwrap();

        assert_eq!(explicit["kind"], "explicit");
        assert_eq!(inferred["kind"], "inferred");
        assert_ne!(explicit, inferred);
    }

    /// An unresolved width is `null`, never 0 or 1.
    #[test]
    fn an_unresolved_width_is_null() {
        let signal = SignalOutput {
            path: "i_x".to_string(),
            role: "data",
            r#type: "logic".to_string(),
            width: None,
            array: None,
            clock_domain: DomainOutput::None,
        };
        let value = serde_json::to_value(&signal).unwrap();

        assert!(value["width"].is_null());
        assert!(value["array"].is_null());
    }
}

//! Matching bundles to ports.
//!
//! The naming rule is the default, and an explicit `ports` list overrides it:
//!
//! 1. Clock and reset ports are excluded. They are found by type, not name.
//! 2. With `ports`, that list is the only definition. Names match exactly,
//!    and an unknown name is an error.
//! 3. Without it, a port belongs to the bundle when its name without the
//!    direction prefix is `<bundle>` or starts with `<bundle>_`.
//! 4. If several bundles match, the longest name wins. A tie is an error.
//! 5. A port in no bundle is an error: every port must be terminated.
//! 6. A bundle with no port is an error. This catches a port renamed in RTL.
//!
//! The direction comes from the DUT declaration (symbol table). The prefix
//! only marks where the bundle name starts; `i_` / `o_` never decide the
//! direction.

use std::collections::{BTreeMap, HashMap};

use miette::Diagnostic;
use thiserror::Error;
use veryl_metadata::Metadata;

use crate::dut::{Dut, Port, PortDirection, bullet_list};
use crate::manifest::Manifest;
use crate::unconnected::Unconnected;

#[derive(Debug, PartialEq, Eq)]
pub struct Binding {
    pub bundle: String,

    /// Port names in DUT declaration order, so that generation is
    /// deterministic.
    pub ports: Vec<String>,

    /// How the ports were matched. `check` prints it, so that a naming rule
    /// that matched the wrong ports is visible.
    pub how: How,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// Listed in `ports = [...]`.
    Explicit,
    /// Matched by the naming rule.
    Naming,
}

impl How {
    pub fn as_str(&self) -> &'static str {
        match self {
            How::Explicit => "explicit",
            How::Naming => "naming",
        }
    }
}

/// Direction prefixes to strip, from `[lint.naming]` in `Veryl.toml`.
///
/// When set there, the compiler enforces them, so the prefix is sure to be
/// in place. Otherwise the defaults are used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectionPrefixes {
    pub input: Option<String>,
    pub output: Option<String>,
    pub inout: Option<String>,
    pub modport: Option<String>,
}

impl Default for DirectionPrefixes {
    fn default() -> Self {
        Self {
            input: Some("i".to_string()),
            output: Some("o".to_string()),
            inout: Some("io".to_string()),
            // There is no common default for modport (interface) ports, so
            // nothing is stripped unless one is configured.
            modport: None,
        }
    }
}

impl DirectionPrefixes {
    pub fn from_metadata(metadata: &Metadata) -> Self {
        let naming = &metadata.lint.naming;
        let default = Self::default();
        Self {
            input: naming.prefix_port_input.clone().or(default.input),
            output: naming.prefix_port_output.clone().or(default.output),
            inout: naming.prefix_port_inout.clone().or(default.inout),
            modport: naming.prefix_port_modport.clone().or(default.modport),
        }
    }

    fn for_direction(&self, direction: PortDirection) -> Option<&str> {
        let prefix = match direction {
            PortDirection::Input => &self.input,
            PortDirection::Output => &self.output,
            PortDirection::Inout => &self.inout,
            PortDirection::Modport => &self.modport,
            // Interface and import ports have no direction prefix.
            PortDirection::Interface | PortDirection::Import => &None,
        };
        prefix.as_deref()
    }

    /// Returns the name without its direction prefix, or the name unchanged.
    ///
    /// The prefix is stripped only when `_` follows it, so `i` does not strip
    /// `io_x`.
    pub fn strip<'a>(&self, name: &'a str, direction: PortDirection) -> &'a str {
        let Some(prefix) = self.for_direction(direction) else {
            return name;
        };
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('_'))
            .unwrap_or(name)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, Diagnostic)]
pub enum BundleError {
    /// Catches a port renamed in RTL here, not on the board.
    #[error("[bundle.{bundle}] lists port `{port}`, which module `{module}` does not have")]
    #[diagnostic(
        code(harness::bundle::unknown_port),
        help(
            "Ports of `{module}` not yet claimed by a bundle:\n{candidates}\n\nPort names are matched exactly. Fix the name in `ports`."
        )
    )]
    UnknownPort {
        bundle: String,
        port: String,
        module: String,
        candidates: String,
    },

    #[error("[bundle.{bundle}] lists port `{port}`, which is already terminated by [{section}]")]
    #[diagnostic(
        code(harness::bundle::port_also_unconnected),
        help(
            "A port is terminated exactly once. Either remove `{port}` from [{section}], or drop it from the bundle."
        )
    )]
    PortAlsoUnconnected {
        bundle: String,
        port: String,
        section: &'static str,
    },

    #[error("[bundle.{bundle}] lists port `{port}`, which is a {role}")]
    #[diagnostic(
        code(harness::bundle::clock_or_reset_in_bundle),
        help("The harness drives clock and reset ports itself. Remove `{port}` from the list.")
    )]
    ClockOrResetInBundle {
        bundle: String,
        port: String,
        role: String,
    },

    #[error("port `{port}` is claimed by more than one bundle: {bundles}")]
    #[diagnostic(
        code(harness::bundle::port_claimed_twice),
        help(
            "A port is terminated exactly once. Give the port to one bundle by listing it explicitly:\n\n    [bundle.<name>]\n    ports = [\"{port}\", ..]\n\nAn explicit `ports` list wins over the naming rule, so listing it in one bundle settles it."
        )
    )]
    PortClaimedTwice { port: String, bundles: String },

    #[error("[bundle.{bundle}] matches no port of module `{module}`")]
    #[diagnostic(
        code(harness::bundle::empty_bundle),
        help(
            "A port belongs to `{bundle}` when its name without the direction prefix is `{bundle}` or starts with `{bundle}_`. Ports not in any bundle:\n{candidates}\n\nRename the ports, or list them:\n    ports = [..]"
        )
    )]
    EmptyBundle {
        bundle: String,
        module: String,
        candidates: String,
    },

    /// A floating input or dangling output looks connected, and fails only on
    /// the board.
    #[error("{count} port(s) of module `{module}` belong to no bundle")]
    #[diagnostic(
        code(harness::bundle::unterminated_ports),
        help(
            "Unterminated:\n{ports}\n\nEvery port has to be connected. Declare a bundle for them:\n\n    [bundle.<name>]\n    backing = \"reg\"\n    ports   = [..]   # or rename the ports to match the bundle name"
        )
    )]
    UnterminatedPorts {
        module: String,
        count: usize,
        ports: String,
    },
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// Matches the manifest's bundles to the DUT's ports.
pub fn resolve(
    dut: &Dut,
    manifest: &Manifest,
    prefixes: &DirectionPrefixes,
    unconnected: &[Unconnected],
) -> Result<Vec<Binding>, BundleError> {
    // Ports a terminator can take, in declaration order. Clocks, resets and
    // ports already in [tie_off] / [leave_open] are excluded.
    let terminable: Vec<&Port> = dut
        .ports
        .iter()
        .filter(|port| !port.is_clock_or_reset())
        .filter(|port| !unconnected.iter().any(|entry| entry.port == port.name))
        .collect();

    let claims = claim(dut, &terminable, manifest, prefixes, unconnected)?;
    assemble(dut, &terminable, claims)
}

/// Output of `claim`: which bundle took each port, and how.
struct Claims<'a> {
    /// Port name -> bundle name.
    owner: HashMap<&'a str, String>,

    /// Holds every declared bundle, also one that got no port. `assemble`
    /// detects empty bundles, so none may be dropped here.
    how: BTreeMap<String, How>,
}

/// The matching policy. A new rule replaces only this function.
///
/// Now: explicit `ports`, then the naming rule, then the longest match. The
/// checks in `assemble` (declaration order, empty bundles, unterminated
/// ports) hold for any rule, so they stay separate.
///
/// Contract: the returned `how` contains every declared bundle.
fn claim<'a>(
    dut: &Dut,
    terminable: &[&'a Port],
    manifest: &Manifest,
    prefixes: &DirectionPrefixes,
    unconnected: &[Unconnected],
) -> Result<Claims<'a>, BundleError> {
    let mut owner: HashMap<&'a str, String> = HashMap::new();
    let mut how: BTreeMap<String, How> = BTreeMap::new();

    // --- 1. Explicit `ports`, first because they override the naming rule ---
    for (name, bundle) in &manifest.bundle {
        let Some(listed) = &bundle.ports else {
            continue;
        };
        how.insert(name.clone(), How::Explicit);

        for port_name in listed.port_names() {
            let Some(port) = terminable
                .iter()
                .find(|port| port.name == port_name)
                .copied()
            else {
                // There are three reasons a port is not terminable. Each
                // needs a different fix, so tell them apart.
                if let Some(entry) = unconnected.iter().find(|entry| entry.port == port_name) {
                    return Err(BundleError::PortAlsoUnconnected {
                        bundle: name.clone(),
                        port: port_name.to_string(),
                        section: entry.kind.as_str(),
                    });
                }
                if let Some(port) = dut.ports.iter().find(|port| port.name == port_name) {
                    return Err(BundleError::ClockOrResetInBundle {
                        bundle: name.clone(),
                        port: port_name.to_string(),
                        role: role_word(port),
                    });
                }
                return Err(BundleError::UnknownPort {
                    bundle: name.clone(),
                    port: port_name.to_string(),
                    module: dut.name.clone(),
                    candidates: bullet_list(&unclaimed_names(terminable, &owner)),
                });
            };

            if let Some(other) = owner.get(port_name) {
                return Err(BundleError::PortClaimedTwice {
                    port: port_name.to_string(),
                    bundles: format!("{other}, {name}"),
                });
            }
            owner.insert(port.name.as_str(), name.clone());
        }
    }

    // --- 2. The naming rule, only for bundles without `ports` ---
    let by_naming: Vec<&str> = manifest
        .bundle
        .iter()
        .filter(|(_, bundle)| bundle.ports.is_none())
        .map(|(name, _)| name.as_str())
        .collect();

    for name in &by_naming {
        how.insert((*name).to_string(), How::Naming);
    }

    for port in terminable {
        if owner.contains_key(port.name.as_str()) {
            continue;
        }

        let stripped = prefixes.strip(&port.name, port.direction);
        let mut matched: Vec<&str> = by_naming
            .iter()
            .copied()
            .filter(|bundle| matches_name(stripped, bundle))
            .collect();

        // The longest match wins: with `mem` and `mem_wr`, `mem_wr_data`
        // goes to `mem_wr`.
        matched.sort_by_key(|bundle| std::cmp::Reverse(bundle.len()));
        match matched.as_slice() {
            [] => {}
            [only] => {
                owner.insert(port.name.as_str(), (*only).to_string());
            }
            [first, second, ..] if first.len() > second.len() => {
                owner.insert(port.name.as_str(), (*first).to_string());
            }
            // A tie cannot happen today: two names of one length that both
            // prefix the port are the same name, and bundle names are
            // unique. It stays an error in case the rule changes.
            _ => {
                return Err(BundleError::PortClaimedTwice {
                    port: port.name.clone(),
                    bundles: matched.join(", "),
                });
            }
        }
    }

    Ok(Claims { owner, how })
}

/// Checks that hold for any matching rule:
///
/// - ports are in DUT declaration order, so generation is deterministic,
/// - a bundle with no port is an error (catches a renamed port),
/// - a port in no bundle is an error (every port must be terminated).
fn assemble(
    dut: &Dut,
    terminable: &[&Port],
    claims: Claims<'_>,
) -> Result<Vec<Binding>, BundleError> {
    let Claims { owner, how } = claims;

    let mut ports: BTreeMap<&str, Vec<String>> = how
        .keys()
        .map(|bundle| (bundle.as_str(), Vec::new()))
        .collect();

    for port in terminable {
        if let Some(bundle) = owner.get(port.name.as_str())
            && let Some(list) = ports.get_mut(bundle.as_str())
        {
            list.push(port.name.clone());
        }
    }

    if let Some((bundle, _)) = ports.iter().find(|(_, ports)| ports.is_empty()) {
        return Err(BundleError::EmptyBundle {
            bundle: (*bundle).to_string(),
            module: dut.name.clone(),
            candidates: bullet_list(&unclaimed_names(terminable, &owner)),
        });
    }

    let unclaimed = unclaimed_names(terminable, &owner);
    if !unclaimed.is_empty() {
        return Err(BundleError::UnterminatedPorts {
            module: dut.name.clone(),
            count: unclaimed.len(),
            ports: bullet_list(&unclaimed),
        });
    }

    Ok(ports
        .into_iter()
        .map(|(bundle, ports)| Binding {
            how: how[bundle],
            bundle: bundle.to_string(),
            ports,
        })
        .collect())
}

/// True for `<bundle>` itself or `<bundle>_...`.
fn matches_name(stripped: &str, bundle: &str) -> bool {
    stripped == bundle
        || (stripped.len() > bundle.len()
            && stripped.starts_with(bundle)
            && stripped.as_bytes()[bundle.len()] == b'_')
}

fn unclaimed_names(terminable: &[&Port], owner: &HashMap<&str, String>) -> Vec<String> {
    terminable
        .iter()
        .filter(|port| !owner.contains_key(port.name.as_str()))
        .map(|port| format!("{} ({})", port.name, port.direction))
        .collect()
}

fn role_word(port: &Port) -> String {
    use crate::dut::SignalRole;
    if port
        .signals
        .iter()
        .any(|signal| signal.role == SignalRole::Clock)
    {
        "clock".to_string()
    } else {
        "reset".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dut::{Domain, Signal, SignalRole};

    fn port(name: &str, direction: PortDirection, role: SignalRole) -> Port {
        Port {
            name: name.to_string(),
            direction,
            axi4: None,
            signals: vec![Signal {
                path: name.to_string(),
                role,
                type_text: "logic<1>".to_string(),
                width: Some(1),
                array: Some(1),
                domain: Domain::None,
            }],
        }
    }

    fn dut(ports: Vec<Port>) -> Dut {
        Dut {
            name: "dut_top".to_string(),
            file: "src/dut_top.veryl".into(),
            line: 1,
            ports,
        }
    }

    fn manifest(toml: &str) -> Manifest {
        toml::from_str(toml).unwrap()
    }

    const FIFO_PORTS: fn() -> Vec<Port> = || {
        vec![
            port("i_clk", PortDirection::Input, SignalRole::Clock),
            port("i_rst", PortDirection::Input, SignalRole::Reset),
            port("i_push", PortDirection::Input, SignalRole::Data),
            port("i_data", PortDirection::Input, SignalRole::Data),
            port("o_full", PortDirection::Output, SignalRole::Data),
            port("i_pop", PortDirection::Input, SignalRole::Data),
            port("o_data", PortDirection::Output, SignalRole::Data),
            port("o_empty", PortDirection::Output, SignalRole::Data),
        ]
    };

    // -----------------------------------------------------------------------
    // Policy (`claim`) tests. They change when the rule changes.
    // -----------------------------------------------------------------------

    #[test]
    fn the_naming_rule_collects_by_prefix() {
        let dut = dut(vec![
            port("i_clk", PortDirection::Input, SignalRole::Clock),
            port("o_dmem_addr", PortDirection::Output, SignalRole::Data),
            port("i_dmem_rdata", PortDirection::Input, SignalRole::Data),
            port("o_imem_addr", PortDirection::Output, SignalRole::Data),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.dmem]\nbacking = \"bram\"\n\n[bundle.imem]\nbacking = \"bram_preload\"\n",
        );

        let bindings = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap();

        assert_eq!(bindings[0].bundle, "dmem");
        // In declaration order.
        assert_eq!(bindings[0].ports, ["o_dmem_addr", "i_dmem_rdata"]);
        assert_eq!(bindings[0].how, How::Naming);
        assert_eq!(bindings[1].ports, ["o_imem_addr"]);
    }

    /// The FIFO reference DUT. The naming rule cannot split it, so `ports`
    /// lists the ports.
    #[test]
    fn the_mvp_fifo_needs_explicit_ports() {
        let dut = dut(FIFO_PORTS());
        let manifest = manifest(
            r#"
[dut]
module = "dut_top"

[bundle.push]
backing = "reg"
ports   = ["i_push", "i_data", "o_full"]

[bundle.pop]
backing = "host_poll_fifo"
ports   = ["i_pop", "o_data", "o_empty"]
"#,
        );

        let bindings = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap();

        let pop = bindings.iter().find(|b| b.bundle == "pop").unwrap();
        assert_eq!(pop.ports, ["i_pop", "o_data", "o_empty"]);
        assert_eq!(pop.how, How::Explicit);
    }

    /// With the naming rule alone, `i_data` / `o_data` belong to no bundle.
    /// This is why explicit `ports` exist.
    #[test]
    fn the_naming_rule_alone_leaves_the_fifo_unterminated() {
        let dut = dut(FIFO_PORTS());
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.push]\nbacking = \"reg\"\n\n[bundle.pop]\nbacking = \"host_poll_fifo\"\n",
        );

        let err = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap_err();

        let BundleError::UnterminatedPorts { ports, .. } = &err else {
            panic!("expected UnterminatedPorts, got {err:?}");
        };
        assert!(ports.contains("i_data"), "ports were: {ports}");
        assert!(ports.contains("o_data"), "ports were: {ports}");
    }

    #[test]
    fn clock_and_reset_are_not_terminated_by_a_bundle() {
        let dut = dut(vec![
            port("i_clk", PortDirection::Input, SignalRole::Clock),
            port("i_rst", PortDirection::Input, SignalRole::Reset),
            port("i_csr_addr", PortDirection::Input, SignalRole::Data),
        ]);
        let manifest = manifest("[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\nbacking = \"reg\"\n");

        // Clock and reset must not be reported as unterminated.
        let bindings = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap();
        assert_eq!(bindings[0].ports, ["i_csr_addr"]);
    }

    #[test]
    fn naming_a_clock_in_a_bundle_is_rejected() {
        let dut = dut(vec![
            port("i_clk", PortDirection::Input, SignalRole::Clock),
            port("i_csr_addr", PortDirection::Input, SignalRole::Data),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\nbacking = \"reg\"\nports = [\"i_clk\", \"i_csr_addr\"]\n",
        );

        let err = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap_err();
        assert!(
            matches!(err, BundleError::ClockOrResetInBundle { .. }),
            "got {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Invariant (`assemble`) tests. They hold for any rule.
    // -----------------------------------------------------------------------

    #[test]
    fn a_bundle_that_matches_nothing_is_rejected() {
        let dut = dut(vec![
            port("i_clk", PortDirection::Input, SignalRole::Clock),
            port("i_csr_addr", PortDirection::Input, SignalRole::Data),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\nbacking = \"reg\"\n\n[bundle.dmem]\nbacking = \"bram\"\n",
        );

        let err = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap_err();
        let BundleError::EmptyBundle { bundle, .. } = &err else {
            panic!("expected EmptyBundle, got {err:?}");
        };
        assert_eq!(bundle, "dmem");
    }

    #[test]
    fn an_unknown_port_name_is_rejected() {
        let dut = dut(vec![port(
            "i_csr_addr",
            PortDirection::Input,
            SignalRole::Data,
        )]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\nbacking = \"reg\"\nports = [\"i_csr_adr\"]\n",
        );

        let err = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap_err();
        let BundleError::UnknownPort { candidates, .. } = &err else {
            panic!("expected UnknownPort, got {err:?}");
        };
        assert!(candidates.contains("i_csr_addr"), "{candidates}");
    }

    #[test]
    fn a_port_listed_by_two_bundles_is_rejected() {
        let dut = dut(vec![
            port("i_a", PortDirection::Input, SignalRole::Data),
            port("i_b", PortDirection::Input, SignalRole::Data),
        ]);
        let manifest = manifest(
            r#"
[dut]
module = "dut_top"

[bundle.one]
backing = "reg"
ports   = ["i_a", "i_b"]

[bundle.two]
backing = "reg"
ports   = ["i_b"]
"#,
        );

        let err = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap_err();
        assert!(
            matches!(err, BundleError::PortClaimedTwice { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn the_longest_bundle_name_wins() {
        let dut = dut(vec![
            port("o_mem_addr", PortDirection::Output, SignalRole::Data),
            port("o_mem_wr_data", PortDirection::Output, SignalRole::Data),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.mem]\nbacking = \"bram\"\n\n[bundle.mem_wr]\nbacking = \"bram\"\n",
        );

        let bindings = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap();

        let mem = bindings.iter().find(|b| b.bundle == "mem").unwrap();
        let mem_wr = bindings.iter().find(|b| b.bundle == "mem_wr").unwrap();
        assert_eq!(mem.ports, ["o_mem_addr"]);
        assert_eq!(mem_wr.ports, ["o_mem_wr_data"]);
    }

    #[test]
    fn an_explicit_list_wins_over_the_naming_rule() {
        let dut = dut(vec![
            port("i_csr_addr", PortDirection::Input, SignalRole::Data),
            port("i_other", PortDirection::Input, SignalRole::Data),
        ]);
        let manifest = manifest(
            r#"
[dut]
module = "dut_top"

[bundle.csr]
backing = "reg"
ports   = ["i_csr_addr", "i_other"]
"#,
        );

        let bindings = resolve(&dut, &manifest, &DirectionPrefixes::default(), &[]).unwrap();
        assert_eq!(bindings[0].ports, ["i_csr_addr", "i_other"]);
    }

    #[test]
    fn the_prefix_is_stripped_only_when_an_underscore_follows() {
        let prefixes = DirectionPrefixes::default();

        assert_eq!(prefixes.strip("i_clk", PortDirection::Input), "clk");
        assert_eq!(prefixes.strip("o_data", PortDirection::Output), "data");
        // `i` does not strip `io_x`: no `_` follows it.
        assert_eq!(prefixes.strip("io_x", PortDirection::Input), "io_x");
        assert_eq!(prefixes.strip("io_x", PortDirection::Inout), "x");
        // A DUT without prefixes keeps its names.
        assert_eq!(
            prefixes.strip("dmem_addr", PortDirection::Output),
            "dmem_addr"
        );
    }

    #[test]
    fn the_prefix_is_chosen_by_direction() {
        let prefixes = DirectionPrefixes::default();

        // An input port named `o_...` breaks the convention; keep its name.
        assert_eq!(prefixes.strip("o_weird", PortDirection::Input), "o_weird");
    }
}

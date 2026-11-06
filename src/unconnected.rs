//! Ports outside any bundle.
//!
//! A port in no bundle is an error, and these sections are the way out. They
//! are still terminators, not exclusions: an unused input needs a driver,
//! and an open output affects synthesis, so both appear in the RTL.
//!
//! ```toml
//! [tie_off]                 # drive inputs with constants
//! i_spare_en = 0
//! i_mode     = 3
//! i_key      = "0xdead_beef"
//!
//! [leave_open]              # leave outputs unconnected
//! ports = ["o_debug_state"]
//! ```
//!
//! Inputs (which need a value) and outputs (which do not) have separate
//! sections, so the schema itself catches a wrong direction. One shared
//! table would allow `"open"` on an input.
//!
//! `check` always shows these in the port table (`(tie 0)` / `(open)`). They
//! are neither errors nor warnings: a person wrote them on purpose, and
//! showing them is enough.

use std::collections::BTreeMap;

use miette::Diagnostic;
use thiserror::Error;

use crate::dut::{Dut, Port, PortDirection};
use crate::manifest::{Manifest, TieValue};

/// The terminator of one port.
#[derive(Debug, PartialEq, Eq)]
pub struct Unconnected {
    pub port: String,
    pub kind: Kind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    /// An input driven by a constant.
    Tie(TieValue),
    /// An output left unconnected.
    Open,
    /// A port on board pins.
    ///
    /// A `Direct` port becomes a port of `hns_top` under its own name. The
    /// ports of a tri-state pin become one port `o_<resource>` (no `i`) or
    /// `io_<resource>` (with `i`). Either way the XDC gets one constraint per
    /// bit. `pins` and `standard` are set only with a target; without one,
    /// `check` still checks directions, widths and domains.
    Pin {
        resource: String,
        role: PinRole,
        /// One per bit, bit 0 first. Empty without a target.
        pins: Vec<String>,
        standard: Option<String>,
    },
}

/// What a port is to its pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinRole {
    /// The pin carries the port as it is: an output driven all the time, or
    /// an input read with `sync = false`.
    Direct,
    /// The value a tri-state pin drives (`o`, the key of the entry).
    Out,
    /// Its enable (`t`): 1 = high impedance.
    Enable,
    /// What it reads back (`i`, `sync = false`).
    In,
}

impl PinRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            PinRole::Direct => "direct",
            PinRole::Out => "o",
            PinRole::Enable => "t",
            PinRole::In => "i",
        }
    }
}

impl Kind {
    /// Short form for display.
    pub fn label(&self) -> String {
        match self {
            Kind::Tie(value) => format!("(tie {value})"),
            Kind::Open => "(open)".to_string(),
            Kind::Pin {
                resource,
                role,
                pins,
                ..
            } => {
                let role = match role {
                    PinRole::Direct => String::new(),
                    role => format!(" {}", role.as_str()),
                };
                match pins.as_slice() {
                    [] => format!("(pin {resource}{role})"),
                    pins => format!("(pin {resource}{role} = {})", pins.join(" ")),
                }
            }
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Tie(_) => "tie_off",
            Kind::Open => "leave_open",
            Kind::Pin { .. } => "pin",
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
pub enum UnconnectedError {
    #[error("[{section}] names port `{port}`, which module `{module}` does not have")]
    #[diagnostic(
        code(harness::unconnected::unknown_port),
        help(
            "Ports of `{module}`:\n{candidates}\n\nPort names are matched exactly, so a rename in the RTL breaks the manifest here rather than on the board."
        )
    )]
    UnknownPort {
        section: &'static str,
        port: String,
        module: String,
        candidates: String,
    },

    #[error("[tie_off] drives `{port}`, but it is an {direction}")]
    #[diagnostic(
        code(harness::unconnected::tie_off_not_an_input),
        help(
            "[tie_off] drives an INPUT with a constant. To ignore an output, leave it open instead:\n\n    [leave_open]\n    ports = [\"{port}\"]"
        )
    )]
    TieOffNotAnInput {
        port: String,
        direction: &'static str,
    },

    #[error("[leave_open] leaves `{port}` unconnected, but it is an {direction}")]
    #[diagnostic(
        code(harness::unconnected::leave_open_not_an_output),
        help(
            "[leave_open] applies to OUTPUTs. An unconnected input floats. Drive it instead:\n\n    [tie_off]\n    {port} = 0"
        )
    )]
    LeaveOpenNotAnOutput {
        port: String,
        direction: &'static str,
    },

    #[error("[{section}] names `{port}`, which is a {role}")]
    #[diagnostic(
        code(harness::unconnected::clock_or_reset),
        help(
            "Clock and reset ports are driven by the target's clocking, not by the manifest. Remove `{port}` from [{section}]."
        )
    )]
    ClockOrReset {
        section: &'static str,
        port: String,
        role: String,
    },

    #[error(
        "[tie_off] gives `{port}` the value {value}, which needs {needs} bits but the port is {width} bits wide"
    )]
    #[diagnostic(
        code(harness::unconnected::tie_value_too_wide),
        help("Write a value that fits, or widen the port.")
    )]
    TieValueTooWide {
        port: String,
        value: String,
        needs: u32,
        width: usize,
    },

    /// An input from a pin is asynchronous to the DUT clock, and the tool
    /// never inserts a synchronizer silently, nor leaves one out silently.
    #[error("[pin] reads `{port}` from a board pin, but does not say `sync`")]
    #[diagnostic(
        code(harness::unconnected::pin_input_needs_sync),
        help(
            "A pin is asynchronous to the DUT clock, and only you know whether the DUT samples it safely. Say so:\n\n    [pin]\n    {port} = {{ pin = \"{resource}\", sync = false }}\n\n`sync = false` connects the pin as it is: the DUT samples it itself, for example with a clock it drives out. A synchronizer from the harness (`sync = true`) is not built yet."
        )
    )]
    PinInputNeedsSync { port: String, resource: String },

    #[error("[pin] `{port}` is an input, so it cannot have `{key}`")]
    #[diagnostic(
        code(harness::unconnected::pin_input_with_companion),
        help(
            "An input only reads its pin. For a pin the DUT also drives, make the output the key and name the input as `i`:\n\n    [pin]\n    <output> = {{ pin = \"{resource}\", t = \"<enable>\", i = \"{port}\", sync = false }}"
        )
    )]
    PinInputWithCompanion {
        port: String,
        key: &'static str,
        resource: String,
    },

    #[error("[pin] `{port}` reads its pin back with `i`, but has no `t`")]
    #[diagnostic(
        code(harness::unconnected::pin_read_without_enable),
        help(
            "Without `t` the output drives the pin all the time, and `i` would only read that value back. Name the DUT output that turns the driver off (1 = high impedance):\n\n    [pin]\n    {port} = {{ pin = \"{resource}\", t = \"<enable>\", i = \"<input>\", sync = false }}\n\nOr remove `i`."
        )
    )]
    PinReadWithoutEnable { port: String, resource: String },

    #[error("[pin] `{port}` says `sync`, but reads no input")]
    #[diagnostic(
        code(harness::unconnected::pin_sync_without_input),
        help(
            "`sync` is about an input read from the pin, and `{port}` only drives it. Remove `sync`, or name the input with `i`."
        )
    )]
    PinSyncWithoutInput { port: String },

    #[error("[pin] `{port}` is an {direction}")]
    #[diagnostic(code(harness::unconnected::pin_wrong_direction), help("{fix}"))]
    PinWrongDirection {
        port: String,
        direction: &'static str,
        fix: &'static str,
    },

    #[error(
        "[pin] `{port}` and its `{key}` `{other}` differ in width: {width} and {other_width} bits"
    )]
    #[diagnostic(
        code(harness::unconnected::pin_widths_differ),
        help(
            "Each bit of the pin has its own value, enable and input, so the three ports have the same width. Change the ports in the DUT, or bring them out at one width in a wrapper."
        )
    )]
    PinWidthsDiffer {
        port: String,
        key: &'static str,
        other: String,
        width: usize,
        other_width: usize,
    },

    #[error(
        "[pin] `{port}` and its `{key}` `{other}` are in different clock domains: {domain} and {other_domain}"
    )]
    #[diagnostic(
        code(harness::unconnected::pin_domains_differ),
        help(
            "The value, the enable and the input of a pin change together, so they must be in one clock domain. Put them in one domain in the DUT."
        )
    )]
    PinDomainsDiffer {
        port: String,
        key: &'static str,
        other: String,
        domain: String,
        other_domain: String,
    },

    #[error("[pin] sends `{port}` to board pins, but its width could not be resolved")]
    #[diagnostic(
        code(harness::unconnected::pin_width_unknown),
        help(
            "Without a width, the pins cannot be counted against the port. Give the port a width the analyzer can resolve, for example in a wrapper around the DUT:\n\n    {port}: output logic<4>,"
        )
    )]
    PinWidthUnknown { port: String },

    /// One pin per bit. Otherwise the XDC would put one pin on a whole bus,
    /// which fails only at Vivado placement.
    #[error(
        "[pin] `{port}` is {width} bits wide, and `{resource}` of target `{target}` has {count} pins"
    )]
    #[diagnostic(
        code(harness::unconnected::pin_count_mismatch),
        help(
            "One pin carries one bit. Bind a resource with {width} pins, or bring out {count} bits in a wrapper around the DUT. A resource of several pins lists them, bit 0 first:\n\n    [pins.{resource}]\n    pin = [\"<bit 0>\", \"<bit 1>\", ...]"
        )
    )]
    PinCountMismatch {
        port: String,
        resource: String,
        target: String,
        width: usize,
        count: usize,
    },

    #[error(
        "[pin] `{port}` makes a top-level port `{name}`, and [pin] `{other}` already has that name"
    )]
    #[diagnostic(
        code(harness::unconnected::pin_port_name_taken),
        help(
            "A tri-state pin reaches the top as `o_<resource>` or `io_<resource>`, and a plain [pin] port keeps its DUT name. Rename the resource in the target description, or rename `{other}` in the DUT."
        )
    )]
    PinPortNameTaken {
        name: String,
        port: String,
        other: String,
    },

    #[error("[pin] binds `{port}` to `{resource}`, which target `{target}` does not have")]
    #[diagnostic(
        code(harness::unconnected::unknown_pin_resource),
        help(
            "`{target}` declares:\n{available}\n\nThe manifest names a resource, not a pin, so it works on other boards. If this board has the signal, add it to the target description:\n\n    [pins.{resource}]\n    pin       = \"<package pin>\"\n    standard  = \"<IOSTANDARD>\"\n    direction = \"output\""
        )
    )]
    UnknownPinResource {
        port: String,
        resource: String,
        target: String,
        available: String,
    },

    #[error("[pin] binds both `{first}` and `{second}` to `{resource}`")]
    #[diagnostic(
        code(harness::unconnected::pin_resource_taken),
        help(
            "A resource carries one [pin] entry. Give one of them a different resource, or remove it from [pin]."
        )
    )]
    PinResourceTaken {
        resource: String,
        first: String,
        second: String,
    },

    #[error(
        "[pins.{resource}] of target `{target}` has `direction = \"{direction}\"`, and `{port}` needs {needs}"
    )]
    #[diagnostic(
        code(harness::unconnected::pin_direction_mismatch),
        help("{why} Pick a resource with {needs}, or change how `{port}` uses it.")
    )]
    PinDirectionMismatch {
        resource: String,
        port: String,
        target: String,
        direction: String,
        needs: &'static str,
        why: &'static str,
    },

    #[error("[pins.{resource}] of target `{target}` does not say {missing}")]
    #[diagnostic(
        code(harness::unconnected::pin_incomplete),
        help(
            "A pin constraint needs all three:\n\n    [pins.{resource}]\n    pin       = \"<package pin>\"\n    standard  = \"<IOSTANDARD>\"\n    direction = \"output\"\n\nThis is a fault in the description, not in your project. Take the values from the board file rather than writing them from memory."
        )
    )]
    PinIncomplete {
        resource: String,
        target: String,
        missing: &'static str,
    },

    /// A constant of the wrong width shows up only on the board.
    #[error("[tie_off] gives `{port}` a value, but its width could not be resolved")]
    #[diagnostic(
        code(harness::unconnected::tie_width_unknown),
        help(
            "Without a width, the value cannot be checked against the port, and widths cannot be written in Harness.toml yet. Give the port a width the analyzer can resolve, for example in a wrapper around the DUT:\n\n    i_mode: input logic<4>,"
        )
    )]
    TieWidthUnknown { port: String },
}

/// Resolves `[tie_off]`, `[leave_open]` and `[pin]`.
///
/// The result is in DUT declaration order, so that generation is
/// deterministic.
pub fn resolve(
    dut: &Dut,
    manifest: &Manifest,
    target: Option<&crate::target::Target>,
) -> Result<Vec<Unconnected>, UnconnectedError> {
    let mut kinds: BTreeMap<&str, Kind> = BTreeMap::new();

    for (name, value) in &manifest.tie_off {
        let port = find_port(dut, name, "tie_off")?;
        reject_clock_or_reset(port, "tie_off")?;

        if port.direction != PortDirection::Input {
            return Err(UnconnectedError::TieOffNotAnInput {
                port: name.clone(),
                direction: port.direction.as_str(),
            });
        }

        // An unknown width is refused, not assumed.
        match port.width() {
            Some(width) => {
                let needs = value.bit_width();
                if needs as usize > width {
                    return Err(UnconnectedError::TieValueTooWide {
                        port: name.clone(),
                        value: value.text.clone(),
                        needs,
                        width,
                    });
                }
            }
            None => {
                return Err(UnconnectedError::TieWidthUnknown { port: name.clone() });
            }
        }

        kinds.insert(port.name.as_str(), Kind::Tie(value.clone()));
    }

    for name in &manifest.leave_open.ports {
        let port = find_port(dut, name, "leave_open")?;
        reject_clock_or_reset(port, "leave_open")?;

        if port.direction != PortDirection::Output {
            return Err(UnconnectedError::LeaveOpenNotAnOutput {
                port: name.clone(),
                direction: port.direction.as_str(),
            });
        }

        kinds.insert(port.name.as_str(), Kind::Open);
    }

    // `[pin]`: terminate at board pins. Directions, widths and domains are
    // checked even without a target; the pins themselves need one.
    let mut taken: BTreeMap<&str, String> = BTreeMap::new();
    for (name, binding) in &manifest.pin {
        let resource = binding.resource();
        let port = find_port(dut, name, "pin")?;
        reject_clock_or_reset(port, "pin")?;

        // The key decides the shape: an output drives the pin, an input only
        // reads it.
        let tristate = match port.direction {
            PortDirection::Output => {
                if binding.i().is_some() && binding.t().is_none() {
                    return Err(UnconnectedError::PinReadWithoutEnable {
                        port: name.clone(),
                        resource: resource.to_string(),
                    });
                }
                if binding.i().is_none() && binding.sync().is_some() {
                    return Err(UnconnectedError::PinSyncWithoutInput { port: name.clone() });
                }
                if binding.i().is_some() && binding.sync().is_none() {
                    return Err(UnconnectedError::PinInputNeedsSync {
                        port: binding.i().unwrap_or_default().to_string(),
                        resource: resource.to_string(),
                    });
                }
                binding.t().is_some()
            }
            PortDirection::Input => {
                for (key, value) in [("t", binding.t()), ("i", binding.i())] {
                    if value.is_some() {
                        return Err(UnconnectedError::PinInputWithCompanion {
                            port: name.clone(),
                            key,
                            resource: resource.to_string(),
                        });
                    }
                }
                if binding.sync().is_none() {
                    return Err(UnconnectedError::PinInputNeedsSync {
                        port: name.clone(),
                        resource: resource.to_string(),
                    });
                }
                false
            }
            direction => {
                return Err(UnconnectedError::PinWrongDirection {
                    port: name.clone(),
                    direction: direction.as_str(),
                    fix: "[pin] takes the DUT's side of a pin as plain ports: an output that drives it, and for a tri-state pin an output `t` (1 = high impedance) and an input `i`. The harness makes the bidirectional port in the top. Split the port in a wrapper around the DUT.",
                });
            }
        };
        let width = port
            .width()
            .ok_or_else(|| UnconnectedError::PinWidthUnknown { port: name.clone() })?;

        // The enable and the input next to the key: same width, same domain.
        let mut members = vec![(
            port,
            if tristate {
                PinRole::Out
            } else {
                PinRole::Direct
            },
        )];
        for (key, other, role, direction, fix) in [
            (
                "t",
                binding.t(),
                PinRole::Enable,
                PortDirection::Output,
                "`t` is the DUT output that turns the driver off (1 = high impedance). Name an output.",
            ),
            (
                "i",
                binding.i(),
                PinRole::In,
                PortDirection::Input,
                "`i` is the DUT input that reads the pin. Name an input.",
            ),
        ] {
            let Some(other) = other else { continue };
            let found = find_port(dut, other, "pin")?;
            reject_clock_or_reset(found, "pin")?;
            if found.direction != direction {
                return Err(UnconnectedError::PinWrongDirection {
                    port: other.to_string(),
                    direction: found.direction.as_str(),
                    fix,
                });
            }
            let other_width = found
                .width()
                .ok_or_else(|| UnconnectedError::PinWidthUnknown {
                    port: other.to_string(),
                })?;
            if other_width != width {
                return Err(UnconnectedError::PinWidthsDiffer {
                    port: name.clone(),
                    key,
                    other: other.to_string(),
                    width,
                    other_width,
                });
            }
            let (domain, other_domain) = (port_domain(port), port_domain(found));
            if !same_domain(domain, other_domain) {
                return Err(UnconnectedError::PinDomainsDiffer {
                    port: name.clone(),
                    key,
                    other: other.to_string(),
                    domain: domain.map(|d| d.label()).unwrap_or_default(),
                    other_domain: other_domain.map(|d| d.label()).unwrap_or_default(),
                });
            }
            members.push((found, role));
        }

        if let Some(first) = taken.get(resource) {
            return Err(UnconnectedError::PinResourceTaken {
                resource: resource.to_string(),
                first: first.clone(),
                second: name.clone(),
            });
        }
        taken.insert(resource, name.clone());

        let (pins, standard) = match target {
            Some(target) => {
                let found = crate::target::pin_resource(target, resource).ok_or_else(|| {
                    UnconnectedError::UnknownPinResource {
                        port: name.clone(),
                        resource: resource.to_string(),
                        target: target.name.clone(),
                        available: crate::target::pin_resources(target),
                    }
                })?;
                if let Some(missing) = found.missing() {
                    return Err(UnconnectedError::PinIncomplete {
                        resource: resource.to_string(),
                        target: target.name.clone(),
                        missing,
                    });
                }
                let reads = binding.i().is_some() || port.direction == PortDirection::Input;
                let (allowed, needs, why) = match (port.direction, tristate, reads) {
                    (PortDirection::Input, ..) => (
                        found.fpga_reads(),
                        "`direction = \"input\"` or `\"inout\"`",
                        "The FPGA only reads this pin, so the board must drive it.",
                    ),
                    (_, false, _) => (
                        found.fpga_drives(),
                        "`direction = \"output\"`",
                        "Without `t` the FPGA drives the pin all the time, which could fight a driver on the board. For a pin both sides drive, add `t`.",
                    ),
                    (_, true, false) => (
                        found.fpga_may_drive(),
                        "`direction = \"output\"` or `\"inout\"`",
                        "A tri-state output drives the pin.",
                    ),
                    (_, true, true) => (
                        found.direction.as_deref() == Some("inout"),
                        "`direction = \"inout\"`",
                        "With `t` and `i` both sides drive the pin in turn.",
                    ),
                };
                if !allowed {
                    return Err(UnconnectedError::PinDirectionMismatch {
                        resource: resource.to_string(),
                        port: name.clone(),
                        target: target.name.clone(),
                        direction: found.direction.clone().unwrap_or_default(),
                        needs,
                        why,
                    });
                }
                if found.pins.len() != width {
                    return Err(UnconnectedError::PinCountMismatch {
                        port: name.clone(),
                        resource: resource.to_string(),
                        target: target.name.clone(),
                        width,
                        count: found.pins.len(),
                    });
                }
                (found.pins, found.standard)
            }
            None => (Vec::new(), None),
        };

        for (member, role) in members {
            kinds.insert(
                member.name.as_str(),
                Kind::Pin {
                    resource: resource.to_string(),
                    role,
                    pins: pins.clone(),
                    standard: standard.clone(),
                },
            );
        }
    }

    // A tri-state pin makes a top-level port of its own. It must not take
    // the name of a port that keeps its DUT name there.
    for (name, binding) in &manifest.pin {
        let Some(top) = tristate_port_name(binding) else {
            continue;
        };
        if let Some((other, _)) = manifest
            .pin
            .iter()
            .find(|(other, b)| **other == top && tristate_port_name(b).is_none())
        {
            return Err(UnconnectedError::PinPortNameTaken {
                name: top,
                port: name.clone(),
                other: other.clone(),
            });
        }
    }

    // Back into declaration order.
    Ok(dut
        .ports
        .iter()
        .filter_map(|port| {
            kinds.remove(port.name.as_str()).map(|kind| Unconnected {
                port: port.name.clone(),
                kind,
            })
        })
        .collect())
}

/// The top-level port of a tri-state pin: `io_<resource>` when the DUT reads
/// it back, `o_<resource>` when it only drives it. `None` for a port that
/// reaches the top under its own name.
pub fn tristate_port_name(binding: &crate::manifest::PinBinding) -> Option<String> {
    binding.t()?;
    let prefix = if binding.i().is_some() { "io" } else { "o" };
    Some(format!("{prefix}_{}", binding.resource()))
}

/// The clock domain of a one-signal port.
fn port_domain(port: &Port) -> Option<&crate::dut::Domain> {
    port.signals.first().map(|signal| &signal.domain)
}

/// Whether two ports are in one clock domain. A written and an inferred
/// domain of the same name are the same domain.
fn same_domain(a: Option<&crate::dut::Domain>, b: Option<&crate::dut::Domain>) -> bool {
    use crate::dut::Domain;
    match (a, b) {
        (
            Some(Domain::Explicit(a) | Domain::Inferred(a)),
            Some(Domain::Explicit(b) | Domain::Inferred(b)),
        ) => a == b,
        (a, b) => a == b,
    }
}

fn find_port<'a>(
    dut: &'a Dut,
    name: &str,
    section: &'static str,
) -> Result<&'a Port, UnconnectedError> {
    dut.ports
        .iter()
        .find(|port| port.name == name)
        .ok_or_else(|| UnconnectedError::UnknownPort {
            section,
            port: name.to_string(),
            module: dut.name.clone(),
            candidates: crate::dut::bullet_list(
                &dut.ports
                    .iter()
                    .map(|port| format!("{} ({})", port.name, port.direction))
                    .collect::<Vec<_>>(),
            ),
        })
}

fn reject_clock_or_reset(port: &Port, section: &'static str) -> Result<(), UnconnectedError> {
    use crate::dut::SignalRole;
    if !port.is_clock_or_reset() {
        return Ok(());
    }
    let role = if port
        .signals
        .iter()
        .any(|signal| signal.role == SignalRole::Clock)
    {
        "clock"
    } else {
        "reset"
    };
    Err(UnconnectedError::ClockOrReset {
        section,
        port: port.name.clone(),
        role: role.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dut::{Domain, Signal, SignalRole};

    fn port(name: &str, direction: PortDirection, width: usize) -> Port {
        Port {
            name: name.to_string(),
            direction,
            axi4: None,
            corebus: None,
            signals: vec![Signal {
                path: name.to_string(),
                role: SignalRole::Data,
                type_text: format!("logic<{width}>"),
                width: Some(width),
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

    const HEAD: &str = "[dut]\nmodule = \"dut_top\"\n";

    fn arty() -> crate::target::Target {
        crate::target::resolve("digilent/arty-a7-35", &[]).unwrap()
    }

    #[test]
    fn a_pin_binding_takes_its_pin_from_the_target() {
        let dut = dut(vec![port("o_uart_tx", PortDirection::Output, 1)]);
        let manifest = manifest(&format!("{HEAD}\n[pin]\no_uart_tx = \"uart_tx\"\n"));

        let resolved = resolve(&dut, &manifest, Some(&arty())).unwrap();
        assert_eq!(resolved.len(), 1);
        let Kind::Pin {
            resource,
            role,
            pins,
            standard,
        } = &resolved[0].kind
        else {
            panic!("wrong kind: {:?}", resolved[0].kind);
        };
        assert_eq!(resource, "uart_tx");
        assert_eq!(*role, PinRole::Direct);
        assert_eq!(pins, &["D10"]);
        assert_eq!(standard.as_deref(), Some("LVCMOS33"));
    }

    /// An input must say whether it gets a synchronizer, with or without a
    /// target.
    #[test]
    fn a_pin_input_must_say_sync() {
        let dut = dut(vec![port("i_uart_rx", PortDirection::Input, 1)]);
        let manifest = manifest(&format!("{HEAD}\n[pin]\ni_uart_rx = \"uart_rx\"\n"));

        let err = resolve(&dut, &manifest, None).unwrap_err();
        assert!(matches!(err, UnconnectedError::PinInputNeedsSync { .. }));
        let help = miette::Diagnostic::help(&err).unwrap().to_string();
        assert!(help.contains("sync = false"), "{help}");
    }

    #[test]
    fn a_pin_input_with_sync_false_is_read_directly() {
        let dut = dut(vec![port("i_uart_rx", PortDirection::Input, 1)]);
        let manifest = manifest(&format!(
            "{HEAD}\n[pin]\ni_uart_rx = {{ pin = \"uart_rx\", sync = false }}\n"
        ));

        let resolved = resolve(&dut, &manifest, Some(&arty())).unwrap();
        let Kind::Pin { role, pins, .. } = &resolved[0].kind else {
            panic!("wrong kind: {:?}", resolved[0].kind);
        };
        assert_eq!(*role, PinRole::Direct);
        assert_eq!(pins.len(), 1);
    }

    /// A board with a 4-bit bidirectional pin and a 1-bit output, as a patch
    /// over the Arty.
    fn pad_board() -> crate::target::Target {
        let dir = tempfile::tempdir().unwrap();
        let patch = dir.path().join("pads.toml");
        std::fs::write(
            &patch,
            "[pins.cfg_dat]\npin = [\"A1\", \"A2\", \"A3\", \"A4\"]\nstandard = \"LVCMOS18\"\ndirection = \"inout\"\n\n[pins.cfg_cs]\npin = \"B1\"\nstandard = \"LVCMOS18\"\ndirection = \"output\"\n",
        )
        .unwrap();
        let target = crate::target::resolve("digilent/arty-a7-35", &[patch]).unwrap();
        std::mem::forget(dir);
        target
    }

    fn pad_dut() -> Dut {
        dut(vec![
            port("o_dout", PortDirection::Output, 4),
            port("o_dout_t", PortDirection::Output, 4),
            port("i_din", PortDirection::Input, 4),
            port("o_cs", PortDirection::Output, 1),
            port("o_cs_t", PortDirection::Output, 1),
        ])
    }

    const PADS: &str = "[pin]\no_dout = { pin = \"cfg_dat\", t = \"o_dout_t\", i = \"i_din\", sync = false }\no_cs = { pin = \"cfg_cs\", t = \"o_cs_t\" }\n";

    #[test]
    fn a_tristate_pin_gives_each_port_its_role() {
        let manifest = manifest(&format!("{HEAD}\n{PADS}"));
        let resolved = resolve(&pad_dut(), &manifest, Some(&pad_board())).unwrap();

        let roles: Vec<(&str, PinRole, usize)> = resolved
            .iter()
            .map(|entry| match &entry.kind {
                Kind::Pin { role, pins, .. } => (entry.port.as_str(), *role, pins.len()),
                kind => panic!("wrong kind: {kind:?}"),
            })
            .collect();
        assert_eq!(
            roles,
            [
                ("o_dout", PinRole::Out, 4),
                ("o_dout_t", PinRole::Enable, 4),
                ("i_din", PinRole::In, 4),
                ("o_cs", PinRole::Out, 1),
                ("o_cs_t", PinRole::Enable, 1),
            ]
        );
    }

    /// `i` needs `inout`: an `output` resource cannot be read back.
    #[test]
    fn reading_back_needs_an_inout_resource() {
        let manifest = manifest(&format!(
            "{HEAD}\n[pin]\no_cs = {{ pin = \"cfg_cs\", t = \"o_cs_t\", i = \"i_cs\", sync = false }}\n"
        ));
        let dut = dut(vec![
            port("o_cs", PortDirection::Output, 1),
            port("o_cs_t", PortDirection::Output, 1),
            port("i_cs", PortDirection::Input, 1),
        ]);

        let err = resolve(&dut, &manifest, Some(&pad_board())).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::PinDirectionMismatch { .. }),
            "{err:?}"
        );
    }

    /// Without `t` the FPGA would drive a bidirectional pin all the time.
    #[test]
    fn an_inout_resource_needs_t() {
        let manifest = manifest(&format!("{HEAD}\n[pin]\no_dout = \"cfg_dat\"\n"));
        let dut = dut(vec![port("o_dout", PortDirection::Output, 4)]);

        let err = resolve(&dut, &manifest, Some(&pad_board())).unwrap_err();
        let UnconnectedError::PinDirectionMismatch { why, .. } = &err else {
            panic!("{err:?}");
        };
        assert!(why.contains("add `t`"), "{why}");
    }

    #[test]
    fn the_ports_of_a_pin_have_one_width() {
        let manifest = manifest(&format!("{HEAD}\n{PADS}"));
        let mut dut = pad_dut();
        dut.ports[1] = port("o_dout_t", PortDirection::Output, 1);

        let err = resolve(&dut, &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::PinWidthsDiffer { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn the_ports_of_a_pin_are_in_one_domain() {
        let manifest = manifest(&format!("{HEAD}\n{PADS}"));
        let mut dut = pad_dut();
        dut.ports[2].signals[0].domain = Domain::Explicit("b".to_string());

        let err = resolve(&dut, &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::PinDomainsDiffer { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn i_without_t_is_rejected() {
        let manifest = manifest(&format!(
            "{HEAD}\n[pin]\no_dout = {{ pin = \"cfg_dat\", i = \"i_din\", sync = false }}\n"
        ));
        let err = resolve(&pad_dut(), &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::PinReadWithoutEnable { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn i_must_say_sync() {
        let manifest = manifest(&format!(
            "{HEAD}\n[pin]\no_dout = {{ pin = \"cfg_dat\", t = \"o_dout_t\", i = \"i_din\" }}\n"
        ));
        let err = resolve(&pad_dut(), &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::PinInputNeedsSync { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn t_must_be_an_output() {
        let manifest = manifest(&format!(
            "{HEAD}\n[pin]\no_dout = {{ pin = \"cfg_dat\", t = \"i_din\" }}\n"
        ));
        let err = resolve(&pad_dut(), &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::PinWrongDirection { .. }),
            "{err:?}"
        );
    }

    /// `o_<resource>` would also be the top-level name of a direct pin.
    #[test]
    fn a_tristate_port_name_cannot_be_taken() {
        let manifest = manifest(&format!(
            "{HEAD}\n[pin]\no_dout = {{ pin = \"cs\", t = \"o_dout_t\" }}\no_cs = \"uart_tx\"\n"
        ));
        let err = resolve(&pad_dut(), &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::PinPortNameTaken { .. }),
            "{err:?}"
        );
    }

    /// The board decides the direction. An output cannot drive a resource
    /// the FPGA receives.
    #[test]
    fn driving_a_resource_the_board_drives_is_rejected() {
        let dut = dut(vec![port("o_uart_tx", PortDirection::Output, 1)]);
        let manifest = manifest(&format!("{HEAD}\n[pin]\no_uart_tx = \"uart_rx\"\n"));

        let err = resolve(&dut, &manifest, Some(&arty())).unwrap_err();
        assert!(matches!(err, UnconnectedError::PinDirectionMismatch { .. }));
    }

    /// One pin per bit. Without a target the pins cannot be counted.
    #[test]
    fn a_pin_carries_one_bit() {
        let dut = dut(vec![port("o_led", PortDirection::Output, 8)]);
        let manifest = manifest(&format!("{HEAD}\n[pin]\no_led = \"uart_tx\"\n"));

        assert!(resolve(&dut, &manifest, None).is_ok());
        let err = resolve(&dut, &manifest, Some(&arty())).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::PinCountMismatch { .. }),
            "got {err:?}"
        );
        assert!(err.to_string().contains("8 bits wide"), "{err}");
    }

    #[test]
    fn two_ports_cannot_share_one_resource() {
        let dut = dut(vec![
            port("o_a", PortDirection::Output, 1),
            port("o_b", PortDirection::Output, 1),
        ]);
        let manifest = manifest(&format!(
            "{HEAD}\n[pin]\no_a = \"uart_tx\"\no_b = \"uart_tx\"\n"
        ));

        let err = resolve(&dut, &manifest, Some(&arty())).unwrap_err();
        assert!(matches!(err, UnconnectedError::PinResourceTaken { .. }));
    }

    #[test]
    fn tie_off_and_leave_open_are_resolved_in_declaration_order() {
        let dut = dut(vec![
            port("i_mode", PortDirection::Input, 4),
            port("o_dbg", PortDirection::Output, 8),
            port("i_spare", PortDirection::Input, 1),
        ]);
        let manifest = manifest(&format!(
            "{HEAD}\n[tie_off]\ni_spare = 1\ni_mode = 3\n\n[leave_open]\nports = [\"o_dbg\"]\n"
        ));

        let resolved = resolve(&dut, &manifest, None).unwrap();

        let names: Vec<&str> = resolved.iter().map(|x| x.port.as_str()).collect();
        assert_eq!(names, ["i_mode", "o_dbg", "i_spare"]);
        assert_eq!(resolved[1].kind, Kind::Open);
        assert_eq!(resolved[0].kind.label(), "(tie 3)");
    }

    #[test]
    fn a_radix_value_is_accepted_and_kept_as_written() {
        let dut = dut(vec![port("i_key", PortDirection::Input, 32)]);
        let manifest = manifest(&format!("{HEAD}\n[tie_off]\ni_key = \"0xdead_beef\"\n"));

        let resolved = resolve(&dut, &manifest, None).unwrap();

        let Kind::Tie(value) = &resolved[0].kind else {
            panic!("expected a tie");
        };
        assert_eq!(value.value, 0xdead_beef);
        // Kept as written. In decimal it could not be compared with the
        // manifest.
        assert_eq!(value.text, "0xdead_beef");
    }

    #[test]
    fn a_value_that_does_not_fit_is_rejected() {
        let dut = dut(vec![port("i_mode", PortDirection::Input, 2)]);
        let manifest = manifest(&format!("{HEAD}\n[tie_off]\ni_mode = 7\n"));

        let err = resolve(&dut, &manifest, None).unwrap_err();
        let UnconnectedError::TieValueTooWide { needs, width, .. } = &err else {
            panic!("expected TieValueTooWide, got {err:?}");
        };
        assert_eq!(*needs, 3);
        assert_eq!(*width, 2);
    }

    /// Separate sections let the error name the right one.
    #[test]
    fn tying_an_output_is_rejected_with_the_other_section_offered() {
        let dut = dut(vec![port("o_dbg", PortDirection::Output, 8)]);
        let manifest = manifest(&format!("{HEAD}\n[tie_off]\no_dbg = 0\n"));

        let err = resolve(&dut, &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::TieOffNotAnInput { .. }),
            "got {err:?}"
        );
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("leave_open"), "rendered: {rendered}");
    }

    /// A floating input would fail silently on the board.
    #[test]
    fn leaving_an_input_open_is_rejected() {
        let dut = dut(vec![port("i_spare", PortDirection::Input, 1)]);
        let manifest = manifest(&format!("{HEAD}\n[leave_open]\nports = [\"i_spare\"]\n"));

        let err = resolve(&dut, &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::LeaveOpenNotAnOutput { .. }),
            "got {err:?}"
        );
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("tie_off"), "rendered: {rendered}");
    }

    #[test]
    fn tying_a_clock_is_rejected() {
        let mut clk = port("i_clk", PortDirection::Input, 1);
        clk.signals[0].role = SignalRole::Clock;
        let dut = dut(vec![clk]);
        let manifest = manifest(&format!("{HEAD}\n[tie_off]\ni_clk = 0\n"));

        let err = resolve(&dut, &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::ClockOrReset { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn an_unknown_port_lists_the_ports_that_exist() {
        let dut = dut(vec![port("i_spare", PortDirection::Input, 1)]);
        let manifest = manifest(&format!("{HEAD}\n[tie_off]\ni_spar = 0\n"));

        let err = resolve(&dut, &manifest, None).unwrap_err();
        let UnconnectedError::UnknownPort { candidates, .. } = &err else {
            panic!("expected UnknownPort, got {err:?}");
        };
        assert!(candidates.contains("i_spare"), "{candidates}");
    }

    #[test]
    fn an_unresolved_width_blocks_the_tie() {
        let mut unresolved = port("i_x", PortDirection::Input, 1);
        unresolved.signals[0].width = None;
        let dut = dut(vec![unresolved]);
        let manifest = manifest(&format!("{HEAD}\n[tie_off]\ni_x = 0\n"));

        let err = resolve(&dut, &manifest, None).unwrap_err();
        assert!(
            matches!(err, UnconnectedError::TieWidthUnknown { .. }),
            "got {err:?}"
        );
    }

    /// A negative value has no meaning without a width.
    #[test]
    fn a_negative_value_is_rejected_by_the_schema() {
        let err =
            toml::from_str::<Manifest>(&format!("{HEAD}\n[tie_off]\ni_x = -1\n")).unwrap_err();
        assert!(err.message().contains("negative"), "{}", err.message());
    }
}

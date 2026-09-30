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
    /// An output sent to a board pin.
    ///
    /// It becomes a port of `hns_top` with an XDC constraint. `pin` and
    /// `standard` are set only with a target; without one, `check` still
    /// checks the port direction.
    Pin {
        resource: String,
        pin: Option<String>,
        standard: Option<String>,
    },
}

impl Kind {
    /// Short form for display.
    pub fn label(&self) -> String {
        match self {
            Kind::Tie(value) => format!("(tie {value})"),
            Kind::Open => "(open)".to_string(),
            Kind::Pin { resource, pin, .. } => match pin {
                Some(pin) => format!("(pin {resource} = {pin})"),
                None => format!("(pin {resource})"),
            },
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
    /// never inserts a synchronizer silently.
    #[error("[pin] sends `{port}` to a board pin, but it is an {direction}")]
    #[diagnostic(
        code(harness::unconnected::pin_not_an_output),
        help(
            "[pin] takes OUTPUTs. Inputs from pins need a synchroniser, and they are not supported yet. Drive it from the manifest instead:\n\n    [tie_off]\n    {port} = 0"
        )
    )]
    PinNotAnOutput {
        port: String,
        direction: &'static str,
    },

    /// One resource is one pin. For a wider port, the XDC would put one pin
    /// on the whole bus, which fails only at Vivado placement.
    #[error("[pin] sends `{port}` to one board pin, but it is {width}")]
    #[diagnostic(
        code(harness::unconnected::pin_not_one_bit),
        help(
            "One pin carries one bit. Bring the bit out as its own 1-bit port in a wrapper around the DUT, or leave this port open:\n\n    [leave_open]\n    ports = [\"{port}\"]"
        )
    )]
    PinNotOneBit { port: String, width: String },

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
            "One resource is one pin, so it can carry one port. Give one of them a different resource, or remove it from [pin]."
        )
    )]
    PinResourceTaken {
        resource: String,
        first: String,
        second: String,
    },

    #[error(
        "[pins.{resource}] of target `{target}` is not an FPGA output, but `{port}` is an output"
    )]
    #[diagnostic(
        code(harness::unconnected::pin_direction_mismatch),
        help(
            "On this board, `{resource}` has `direction = \"{direction}\"`. Driving it could fight the board's driver. Pick a resource with `direction = \"output\"`."
        )
    )]
    PinDirectionMismatch {
        resource: String,
        port: String,
        target: String,
        direction: String,
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

    // `[pin]`: terminate at a board pin. The port direction and width are
    // checked even without a target.
    let mut taken: BTreeMap<&str, String> = BTreeMap::new();
    for (name, resource) in &manifest.pin {
        let port = find_port(dut, name, "pin")?;
        reject_clock_or_reset(port, "pin")?;

        // Outputs only. An input needs a synchronizer, which the tool must
        // not insert silently.
        if port.direction != PortDirection::Output {
            return Err(UnconnectedError::PinNotAnOutput {
                port: name.clone(),
                direction: port.direction.as_str(),
            });
        }
        // One pin carries one bit.
        match port.width() {
            Some(1) => {}
            width => {
                return Err(UnconnectedError::PinNotOneBit {
                    port: name.clone(),
                    width: match width {
                        Some(width) => format!("{width} bits wide"),
                        None => "of a width that could not be resolved".to_string(),
                    },
                });
            }
        }

        if let Some(first) = taken.get(resource.as_str()) {
            return Err(UnconnectedError::PinResourceTaken {
                resource: resource.clone(),
                first: first.clone(),
                second: name.clone(),
            });
        }
        taken.insert(resource.as_str(), name.clone());

        let (pin, standard) = match target {
            Some(target) => {
                let found = crate::target::pin_resource(target, resource).ok_or_else(|| {
                    UnconnectedError::UnknownPinResource {
                        port: name.clone(),
                        resource: resource.clone(),
                        target: target.name.clone(),
                        available: crate::target::pin_resources(target),
                    }
                })?;
                if let Some(missing) = found.missing() {
                    return Err(UnconnectedError::PinIncomplete {
                        resource: resource.clone(),
                        target: target.name.clone(),
                        missing,
                    });
                }
                if !found.fpga_drives() {
                    return Err(UnconnectedError::PinDirectionMismatch {
                        resource: resource.clone(),
                        port: name.clone(),
                        target: target.name.clone(),
                        direction: found.direction.clone().unwrap_or_default(),
                    });
                }
                (found.pin, found.standard)
            }
            None => (None, None),
        };

        kinds.insert(
            port.name.as_str(),
            Kind::Pin {
                resource: resource.clone(),
                pin,
                standard,
            },
        );
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
            pin,
            standard,
        } = &resolved[0].kind
        else {
            panic!("wrong kind: {:?}", resolved[0].kind);
        };
        assert_eq!(resource, "uart_tx");
        assert_eq!(pin.as_deref(), Some("D10"));
        assert_eq!(standard.as_deref(), Some("LVCMOS33"));
    }

    #[test]
    fn a_pin_binding_without_a_target_still_checks_the_direction() {
        let dut = dut(vec![port("i_uart_rx", PortDirection::Input, 1)]);
        let manifest = manifest(&format!("{HEAD}\n[pin]\ni_uart_rx = \"uart_rx\"\n"));

        let err = resolve(&dut, &manifest, None).unwrap_err();
        assert!(matches!(err, UnconnectedError::PinNotAnOutput { .. }));
        let help = miette::Diagnostic::help(&err).unwrap().to_string();
        assert!(help.contains("synchroniser"), "{help}");
    }

    #[test]
    fn an_unknown_pin_resource_lists_what_the_board_has() {
        let dut = dut(vec![port("o_led", PortDirection::Output, 1)]);
        let manifest = manifest(&format!("{HEAD}\n[pin]\no_led = \"led0\"\n"));

        let err = resolve(&dut, &manifest, Some(&arty())).unwrap_err();
        assert!(matches!(err, UnconnectedError::UnknownPinResource { .. }));
        let help = miette::Diagnostic::help(&err).unwrap().to_string();
        assert!(help.contains("uart_tx"), "{help}");
        assert!(help.contains("[pins.led0]"), "{help}");
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

    /// The width is checked with and without a target.
    #[test]
    fn a_pin_carries_one_bit() {
        let dut = dut(vec![port("o_led", PortDirection::Output, 8)]);
        let manifest = manifest(&format!("{HEAD}\n[pin]\no_led = \"uart_tx\"\n"));

        for target in [None, Some(arty())] {
            let err = resolve(&dut, &manifest, target.as_ref()).unwrap_err();
            assert!(
                matches!(err, UnconnectedError::PinNotOneBit { .. }),
                "got {err:?}"
            );
            assert!(err.to_string().contains("8 bits wide"), "{err}");
        }
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

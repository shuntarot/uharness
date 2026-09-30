//! `[heartbeat]`: a UART that the harness itself drives.
//!
//! It tells whether the board is alive and which bitstream it runs, even when
//! the transport does not work. This module only plans; `emit::uart_module`
//! writes the RTL.
//!
//! Pins follow the same rules as `[pin]` (`unconnected`): named by resource,
//! the target gives `pin`, `standard` and `direction`, and the FPGA may drive
//! it. A resource or top-level port name already used by `[pin]` is refused,
//! so that no pin gets two drivers.

use miette::Diagnostic;
use thiserror::Error;

use crate::clock::ClockPlan;
use crate::manifest::Manifest;
use crate::regmap::RegisterMap;
use crate::target::Target;
use crate::unconnected::{Kind, Unconnected};

pub struct HeartbeatPlan {
    /// Clocks per bit.
    pub div: u64,
    /// The baud rate after division.
    pub actual_baud: u64,
    /// Clocks between lines: about one second.
    pub gap: u64,
    pub message: String,
    /// Pin resource name in the target.
    pub resource: String,
    pub pin: String,
    pub standard: String,
}

impl HeartbeatPlan {
    /// Name of the top-level port.
    pub fn port(&self) -> String {
        port_name(&self.resource)
    }
}

fn port_name(resource: &str) -> String {
    format!("o_{resource}")
}

#[derive(Debug, Error, Diagnostic)]
pub enum HeartbeatError {
    #[error("[heartbeat] wants pin resource `{resource}`, which target `{target}` does not have")]
    #[diagnostic(
        code(harness::heartbeat::unknown_pin_resource),
        help(
            "`{target}` declares:\n{available}\n\nName a resource the FPGA drives, usually the TX of the board's USB-UART:\n\n    [heartbeat]\n    pin = \"uart_tx\""
        )
    )]
    UnknownPinResource {
        resource: String,
        target: String,
        available: String,
    },

    #[error("[pins.{resource}] of target `{target}` is not an FPGA output")]
    #[diagnostic(
        code(harness::heartbeat::pin_direction_mismatch),
        help(
            "On this board, `{resource}` has `direction = \"{direction}\"`. Driving it could fight the board's driver. Pick a resource with `direction = \"output\"`."
        )
    )]
    PinDirectionMismatch {
        resource: String,
        target: String,
        direction: String,
    },

    #[error("[pins.{resource}] of target `{target}` does not say {missing}")]
    #[diagnostic(
        code(harness::heartbeat::pin_incomplete),
        help(
            "A pin constraint needs all three:\n\n    [pins.{resource}]\n    pin       = \"<package pin>\"\n    standard  = \"<IOSTANDARD>\"\n    direction = \"output\"\n\nThis is a fault in the description, not in your project. Take the values from the board file rather than writing them from memory."
        )
    )]
    PinIncomplete {
        resource: String,
        target: String,
        missing: &'static str,
    },

    /// Two drivers on one pin. Otherwise this fails only at Vivado placement.
    #[error("[heartbeat] and [pin] both use `{resource}`")]
    #[diagnostic(
        code(harness::heartbeat::pin_resource_taken),
        help(
            "One resource is one pin, and [pin] already sends `{port}` there. Give the heartbeat or `{port}` another resource."
        )
    )]
    PinResourceTaken { resource: String, port: String },

    /// The heartbeat port is `o_<resource>`; a `[pin]` port keeps the DUT name.
    #[error("[heartbeat] and [pin] would both make a top-level port `{name}`")]
    #[diagnostic(
        code(harness::heartbeat::port_name_taken),
        help(
            "The heartbeat names its port after the resource, `o_{resource}`, and [pin] keeps the DUT's port name. Give the heartbeat another resource, or rename the DUT port."
        )
    )]
    PortNameTaken { name: String, resource: String },

    #[error("[heartbeat] baud = 0 cannot be sent")]
    #[diagnostic(
        code(harness::heartbeat::zero_baud),
        help("State the rate the terminal expects:\n\n    [heartbeat]\n    baud = 115200")
    )]
    ZeroBaud,

    /// Below 4 clocks per bit, the receiver cannot find the start bit.
    #[error("[heartbeat] baud = {baud} leaves {div} clocks per bit at {clk_hz} Hz")]
    #[diagnostic(
        code(harness::heartbeat::clock_too_slow),
        help(
            "8N1 needs at least 4 clocks per bit. Lower the baud rate, or run the harness on a faster clock."
        )
    )]
    ClockTooSlow { clk_hz: u64, baud: u64, div: u64 },

    /// Past 2% error, the 8th bit is sampled in the wrong place.
    #[error("[heartbeat] baud = {baud} comes out as {actual} at {clk_hz} Hz ({error_pct}% off)")]
    #[diagnostic(
        code(harness::heartbeat::baud_off),
        help(
            "8N1 loses the last bit past about 2%. Pick a baud rate the clock divides more evenly, or change the clock."
        )
    )]
    BaudOff {
        clk_hz: u64,
        baud: u64,
        actual: u64,
        error_pct: String,
    },
}

/// Resolves `[heartbeat]`, or returns `None` when it is absent.
///
/// It needs a target to look up the pin. `check` without a target does not
/// call it and lists it under `not_checked`.
pub fn resolve(
    manifest: &Manifest,
    target: &Target,
    clocks: &ClockPlan,
    registers: &RegisterMap,
    unconnected: &[Unconnected],
) -> Result<Option<HeartbeatPlan>, HeartbeatError> {
    let Some(config) = &manifest.heartbeat else {
        return Ok(None);
    };
    let resource = &config.pin;

    let found = crate::target::pin_resource(target, resource).ok_or_else(|| {
        HeartbeatError::UnknownPinResource {
            resource: resource.clone(),
            target: target.name.clone(),
            available: crate::target::pin_resources(target),
        }
    })?;
    if let Some(missing) = found.missing() {
        return Err(HeartbeatError::PinIncomplete {
            resource: resource.clone(),
            target: target.name.clone(),
            missing,
        });
    }
    if !found.fpga_drives() {
        return Err(HeartbeatError::PinDirectionMismatch {
            resource: resource.clone(),
            target: target.name.clone(),
            direction: found.direction.clone().unwrap_or_default(),
        });
    }

    let name = port_name(resource);
    for item in unconnected {
        let Kind::Pin {
            resource: other, ..
        } = &item.kind
        else {
            continue;
        };
        if other == resource {
            return Err(HeartbeatError::PinResourceTaken {
                resource: resource.clone(),
                port: item.port.clone(),
            });
        }
        if item.port == name {
            return Err(HeartbeatError::PortNameTaken {
                name,
                resource: resource.clone(),
            });
        }
    }

    // `clock::resolve` has checked that the frequency is positive.
    let clk_hz = (clocks.window_output().freq_mhz * 1_000_000.0) as u64;
    let (div, actual_baud) = divide(clk_hz, config.baud)?;
    Ok(Some(HeartbeatPlan {
        div,
        actual_baud,
        gap: clk_hz,
        // The 3-digit counter is not known at generation time. `___` holds
        // its place, and the RTL replaces those 3 bytes (`emit::uart_module`).
        message: format!(
            "hns {:08x} {:08x} {} ___\r\n",
            crate::regmap::MAGIC,
            registers.map_hash,
            registers.dut
        ),
        resource: resource.clone(),
        pin: found.pin.unwrap_or_default(),
        standard: found.standard.unwrap_or_default(),
    }))
}

/// Returns the divider and the resulting baud rate. Too much rounding error
/// is an error, not a silent approximation.
fn divide(clk_hz: u64, baud: u64) -> Result<(u64, u64), HeartbeatError> {
    if baud == 0 {
        return Err(HeartbeatError::ZeroBaud);
    }
    let div = (clk_hz + baud / 2) / baud;
    if div < 4 {
        return Err(HeartbeatError::ClockTooSlow { clk_hz, baud, div });
    }
    let actual = clk_hz / div;
    let error_pct = (actual as f64 - baud as f64).abs() / baud as f64 * 100.0;
    if error_pct > 2.0 {
        return Err(HeartbeatError::BaudOff {
            clk_hz,
            baud,
            actual,
            error_pct: format!("{error_pct:.1}"),
        });
    }
    Ok((div, actual))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_the_clock_divides_is_accepted() {
        // 100 MHz / 115200 = 868.05 -> divide by 868, 115207 baud (0.006%).
        assert_eq!(divide(100_000_000, 115_200).unwrap(), (868, 115_207));
    }

    #[test]
    fn a_rate_the_clock_cannot_carry_is_refused() {
        assert!(matches!(
            divide(100_000_000, 0),
            Err(HeartbeatError::ZeroBaud)
        ));
        assert!(matches!(
            divide(1_000_000, 500_000),
            Err(HeartbeatError::ClockTooSlow { div: 2, .. })
        ));
        // 1 MHz / 230400 = 4.34 -> divide by 4, 250000 baud (8.5%).
        assert!(matches!(
            divide(1_000_000, 230_400),
            Err(HeartbeatError::BaudOff { .. })
        ));
    }
}

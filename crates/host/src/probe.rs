//! Finding the JTAG probe and filling its `ProbeConfig`.
//!
//! Values come from what the caller asked for first, then from the target
//! description, then from what is plugged in. Nothing is guessed: a value
//! that cannot be filled stops with a message that says what to pass.

use crate::bus::Error;
use crate::ftdi::{self, Found};
use crate::mpsse::{self, Chain, ProbeConfig};

/// A TCK the cable is known to carry. Start slow.
pub const DEFAULT_TCK_HZ: u32 = 3_000_000;
/// IR length on 7-series. Checked against the IR capture pattern.
pub const DEFAULT_IR_LENGTH: u8 = 6;
/// USER1 on 7-series. Checked by whether the magic comes back.
pub const DEFAULT_USER1_IR: u32 = 2;

/// What the caller asks for. Each value overrides the target description,
/// so a value can be changed while debugging.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub product: Option<String>,
    /// The whole serial, or a part of it that only one probe has.
    pub serial: Option<String>,
    pub interface: Option<u8>,
    pub layout_init: Option<(u16, u16)>,
    pub tck_hz: Option<u32>,
    pub ir_length: Option<u8>,
    pub user1_ir: Option<u32>,
}

/// Builds a `ProbeConfig` from what is plugged in.
///
/// `target` is the `[jtag]` table of the target description
/// (`hns_targets::jtag`), or the default (all `None`) without one.
pub fn resolve(query: &Query, target: &hns_targets::Jtag) -> Result<ProbeConfig, Error> {
    let vid = query.vid.or(target.vid);
    let pid = query.pid.or(target.pid);
    let found = ftdi::candidates(vid, pid)?;
    let found: Vec<_> = found
        .into_iter()
        .filter(
            |f| match query.product.as_deref().or(target.product.as_deref()) {
                Some(p) => f.product.as_deref() == Some(p),
                None => true,
            },
        )
        .collect();
    let found = match &query.serial {
        Some(s) => {
            let named = by_serial(found.clone(), s);
            if named.is_empty() && !found.is_empty() {
                let list: Vec<String> = found.iter().map(|f| format!("  {f}")).collect();
                return Err(Error::Refused(format!(
                    "no probe's serial contains {s:?}. These are plugged in:\n{}\n\
                     Pass one of their serials, or a part of it, with --serial.",
                    list.join("\n")
                )));
            }
            named
        }
        None => found,
    };

    let one = match found.len() {
        0 => {
            return Err(Error::Refused(
                "no FTDI probe with an MPSSE channel is plugged in.\n\
                 hio looked for the MPSSE product ids under vendor 0x0403. If your probe \
                 uses another id, pass --vid/--pid. `lsusb` shows them."
                    .to_string(),
            ));
        }
        1 => found[0].clone(),
        _ => {
            let list: Vec<String> = found.iter().map(|f| format!("  {f}")).collect();
            return Err(Error::Refused(format!(
                "{} probes match, so hio cannot choose:\n{}\n\
                 Pick one with --serial (a part of it is enough when only one has it), \
                 or with --product.",
                found.len(),
                list.join("\n")
            )));
        }
    };

    // The level shifter is board wiring and USB cannot see it. Do not drive
    // GPIO on a guess: use the known value only for Digilent, and otherwise
    // touch only the four JTAG pins.
    let layout_init = query.layout_init.or(target.layout_init).unwrap_or_else(|| {
        let digilent = one
            .product
            .as_deref()
            .is_some_and(|p| p.contains("Digilent"));
        if digilent {
            mpsse::LAYOUT_DIGILENT
        } else {
            mpsse::LAYOUT_PLAIN
        }
    });

    // Use `host_tck_mhz`, not `max_tck_mhz`. The latter is the device limit
    // for the XDC constraint, not what the cable can carry (Arty claims 30 MHz,
    // but 20 MHz is the measured limit).
    let tck_hz = match query.tck_hz {
        Some(hz) => hz,
        None => target
            .host_tck_mhz
            .map(|mhz| (mhz * 1e6) as u32)
            .unwrap_or(DEFAULT_TCK_HZ),
    };

    Ok(ProbeConfig {
        vid: one.vid,
        pid: one.pid,
        interface: query.interface.or(target.interface).unwrap_or(0),
        serial: one.serial.clone(),
        product: one.product.clone(),
        layout_init,
        max_tck_hz: tck_hz,
        ir_length: query
            .ir_length
            .or(target.ir_length)
            .unwrap_or(DEFAULT_IR_LENGTH),
        user1_ir: query
            .user1_ir
            .or(target.user1_ir)
            .unwrap_or(DEFAULT_USER1_IR),
        chain: Chain::new(&target.chain_tdo_side, &target.chain_tdi_side),
    })
}

/// The probes whose serial is `part`, or else contains it.
fn by_serial(found: Vec<Found>, part: &str) -> Vec<Found> {
    if found.iter().any(|f| f.serial.as_deref() == Some(part)) {
        return found
            .into_iter()
            .filter(|f| f.serial.as_deref() == Some(part))
            .collect();
    }
    found
        .into_iter()
        .filter(|f| f.serial.as_deref().is_some_and(|s| s.contains(part)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_with(serial: &str) -> Found {
        Found {
            vid: 0x0403,
            pid: 0x6010,
            product: None,
            serial: Some(serial.to_string()),
        }
    }

    #[test]
    fn a_part_of_a_serial_picks_the_one_probe_that_has_it() {
        let serials = |found: Vec<Found>| -> Vec<String> {
            found.into_iter().filter_map(|f| f.serial).collect()
        };
        let plugged = || {
            vec![
                probe_with("210319788107"),
                probe_with("210319A8F2D1"),
                probe_with("8107"),
            ]
        };
        assert_eq!(serials(by_serial(plugged(), "8F2D")), ["210319A8F2D1"]);
        // Two have `2103`, so it does not choose.
        assert_eq!(by_serial(plugged(), "2103").len(), 2);
        // `8107` is a whole serial, and a part of another: the whole one wins.
        assert_eq!(serials(by_serial(plugged(), "8107")), ["8107"]);
        assert!(by_serial(plugged(), "9999").is_empty());
    }
}

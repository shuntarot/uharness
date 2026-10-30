//! Programming a PCIe card while the machine keeps it, without enumerating it
//! again.
//!
//! Programming over JTAG resets the PCIe block. The link comes back by
//! itself, but the config space is at its reset values: BAR0 is 0, memory
//! decoding and bus mastering are off, and Max Payload Size is back to 128.
//! The kernel still holds the old assignment, so every read returns
//! `0xffffffff`. Writing back the config space saved before programming makes
//! the card answer at the same address again.
//!
//! That works only when the new design looks the same from the bus: the same
//! ID, class, BAR size and interrupt pin. Otherwise the kernel has to
//! enumerate the card again (a remove and rescan, or a reboot when the BAR no
//! longer fits the bridge window), so the change is refused before
//! programming.
//!
//! Measured on a VCU118: the link came back at the same speed and width, and
//! writing BAR0 and Command back was enough to reach the window.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::pcie::Info;

/// What the bus sees of a card. A change in any of these needs the kernel to
/// enumerate the card again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub vendor: u16,
    pub device: u16,
    /// `None` when unknown. It is then not compared.
    pub class: Option<u32>,
    pub bar_bytes: u64,
    /// The card has an interrupt pin (INTA).
    pub intx: bool,
}

impl Shape {
    /// The card as the kernel enumerated it. The interrupt pin comes from the
    /// card itself: sysfs does not keep it.
    pub fn of_card(info: &Info) -> Shape {
        Shape {
            vendor: info.vendor,
            device: info.device,
            class: info.class,
            bar_bytes: info.bar0_bytes,
            intx: info.interrupt_pin.unwrap_or(0) != 0,
        }
    }

    /// The card a design will be, from its map. `None` for a design without
    /// a PCIe endpoint.
    pub fn of_map(map: &hns_regs::RegisterMap) -> Option<Shape> {
        let pcie = map.pcie.as_ref()?;
        Some(Shape {
            vendor: pcie.vendor_id as u16,
            device: pcie.device_id as u16,
            class: pcie.class_code,
            bar_bytes: u64::from(pcie.bar_bytes),
            intx: map
                .registers
                .iter()
                .any(|r| r.role.as_deref() == Some("irq_level")),
        })
    }
}

/// What differs between the card now and the design to be programmed, one
/// line per difference. Empty when the config space can simply be written
/// back.
pub fn changes(now: &Shape, next: &Shape) -> Vec<String> {
    let mut out = Vec::new();
    if (now.vendor, now.device) != (next.vendor, next.device) {
        out.push(format!(
            "ID {:04x}:{:04x} -> {:04x}:{:04x}",
            now.vendor, now.device, next.vendor, next.device
        ));
    }
    if let (Some(a), Some(b)) = (now.class, next.class)
        && a != b
    {
        out.push(format!("class {a:06x} -> {b:06x}"));
    }
    if now.bar_bytes != next.bar_bytes {
        out.push(format!(
            "BAR0 {} -> {} bytes",
            now.bar_bytes, next.bar_bytes
        ));
    }
    if now.intx != next.intx {
        let pin = |intx| if intx { "INTA" } else { "none" };
        out.push(format!(
            "interrupt pin {} -> {}",
            pin(now.intx),
            pin(next.intx)
        ));
    }
    out
}

/// Reads the card's config space. As root this is all 4096 bytes; others get
/// only the first 64.
pub fn read_config(root: &Path, bdf: &str) -> std::io::Result<Vec<u8>> {
    fs::read(root.join(bdf).join("config"))
}

/// Whether the config space shows a card that answers at the address the
/// kernel gave it: BAR0 holds that address and memory decoding is on. Only
/// such a config space is worth saving.
pub fn answering(config: &[u8], bar0_start: u64) -> bool {
    let (Some(command), Some(bar0)) = (u16_at(config, 0x04), u32_at(config, 0x10)) else {
        return false;
    };
    command & COMMAND_MEMORY != 0 && u64::from(bar0 & !0xf) == bar0_start && bar0_start != 0
}

/// One write to config space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Write {
    pub offset: u32,
    /// 1, 2 or 4.
    pub bytes: u8,
    pub value: u32,
}

/// The writes that put a saved config space back, in order.
///
/// It follows what the kernel restores after a reset (`pci_restore_state`),
/// for the parts a harness card has: the PCI Express control registers
/// first, so MPS and MRRS are right before any TLP moves; the AER masks; then
/// the header, with Command last, so the card starts decoding only once its
/// BAR is in place. Read-only fields are not written.
pub fn restore_plan(saved: &[u8]) -> Vec<Write> {
    let mut out = Vec::new();
    let mut put = |offset: usize, bytes: u8| {
        let value = match bytes {
            1 => saved.get(offset).map(|&b| u32::from(b)),
            2 => u16_at(saved, offset).map(u32::from),
            _ => u32_at(saved, offset),
        };
        if let Some(value) = value {
            out.push(Write {
                offset: offset as u32,
                bytes,
                value,
            });
        }
    };

    if let Some(cap) = capability(saved, CAP_PCIE) {
        put(cap + 0x08, 2); // Device Control: MPS, MRRS, error reporting
        put(cap + 0x10, 2); // Link Control
        // Device Control 2 and Link Control 2 exist from version 2 on.
        if saved.get(cap + 2).is_some_and(|v| v & 0xf >= 2) {
            put(cap + 0x28, 2);
            put(cap + 0x30, 2);
        }
    }
    if let Some(aer) = extended_capability(saved, EXT_CAP_AER) {
        put(aer + 0x08, 4); // uncorrectable error mask
        put(aer + 0x0c, 4); // uncorrectable error severity
        put(aer + 0x14, 4); // correctable error mask
    }
    put(0x0c, 1); // cache line size
    for bar in 0..6 {
        put(0x10 + 4 * bar, 4);
    }
    put(0x3c, 1); // interrupt line
    put(0x04, 2); // Command, last
    // Never ask the link to retrain or go down.
    if let Some(cap) = capability(saved, CAP_PCIE)
        && let Some(w) = out.iter_mut().find(|w| w.offset as usize == cap + 0x10)
    {
        w.value &= !LINK_CONTROL_ACTION;
    }
    out
}

/// Writes to the card's config space. Needs root.
pub fn write_config(root: &Path, bdf: &str, writes: &[Write]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let file = fs::OpenOptions::new()
        .write(true)
        .open(root.join(bdf).join("config"))?;
    for w in writes {
        let bytes = w.value.to_le_bytes();
        file.write_all_at(&bytes[..w.bytes as usize], u64::from(w.offset))?;
    }
    Ok(())
}

/// Waits until the card answers config reads with its ID again. While the
/// link is down, reads return all ones.
pub fn wait_for_card(
    root: &Path,
    bdf: &str,
    vendor: u16,
    device: u16,
    timeout: Duration,
) -> Result<(), String> {
    let want = u32::from(vendor) | u32::from(device) << 16;
    let started = Instant::now();
    loop {
        let got = read_config(root, bdf)
            .ok()
            .and_then(|config| u32_at(&config, 0));
        if got == Some(want) {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "{bdf} did not answer as {vendor:04x}:{device:04x} within {} s after \
                 programming (it reads {}).\n\
                 The link did not come back. Check the board, then reboot the machine.",
                timeout.as_secs(),
                got.map_or("nothing".to_string(), |v| format!("{v:#010x}"))
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

const COMMAND_MEMORY: u16 = 1 << 1;
const CAP_PCIE: u8 = 0x10;
const EXT_CAP_AER: u16 = 0x0001;
/// Link Control: Link Disable (bit 4) and Retrain Link (bit 5).
const LINK_CONTROL_ACTION: u32 = 0x30;

fn u16_at(raw: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(raw.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(raw: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(raw.get(at..at + 4)?.try_into().ok()?))
}

/// The offset of a capability in the list that starts at 0x34.
fn capability(config: &[u8], id: u8) -> Option<usize> {
    const STATUS_CAP_LIST: u16 = 1 << 4;
    if u16_at(config, 0x06)? & STATUS_CAP_LIST == 0 {
        return None;
    }
    let mut at = usize::from(*config.get(0x34)? & !3);
    // A broken list could loop. There are at most 48 capabilities in 256 bytes.
    for _ in 0..48 {
        if at < 0x40 {
            return None;
        }
        if *config.get(at)? == id {
            return Some(at);
        }
        at = usize::from(*config.get(at + 1)? & !3);
    }
    None
}

/// The offset of an extended capability in the list that starts at 0x100.
fn extended_capability(config: &[u8], id: u16) -> Option<usize> {
    let mut at = 0x100;
    for _ in 0..(4096 - 0x100) / 4 {
        let head = u32_at(config, at)?;
        if head == 0 || head == u32::MAX {
            return None;
        }
        if head & 0xffff == u32::from(id) {
            return Some(at);
        }
        at = (head >> 20) as usize & !3;
        if at < 0x100 {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config space shaped like the VCU118 harness: a PCIe capability at
    /// 0x70 (version 2) and AER at 0x100.
    fn saved() -> Vec<u8> {
        let mut c = vec![0u8; 4096];
        let mut put32 = |at: usize, v: u32| c[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put32(0x00, 0x0001_1234);
        put32(0x04, 0x0010_0006); // status: cap list; command: memory, master
        put32(0x10, 0x7a00_0000);
        put32(0x3c, 0x0000_01ff);
        put32(0x34, 0x40);
        put32(0x40, 0x0000_7001); // PM capability, next 0x70
        put32(0x70, 0x0002_0010); // PCIe capability, version 2, last
        put32(0x78, 0x0000_2830); // Device Control: MPS 256, MRRS 512
        put32(0x80, 0x0000_0040 | 0x20); // Link Control, with Retrain set
        put32(0x98, 0x0000_0005);
        put32(0xa0, 0x0000_0003);
        put32(0x100, 0x0001_0001); // AER, version 1, last
        put32(0x108, 0x0040_0000);
        put32(0x10c, 0x0046_2030);
        put32(0x114, 0x0000_e000);
        c
    }

    #[test]
    fn the_control_registers_go_back_first_and_command_last() {
        let plan = restore_plan(&saved());
        let at = |offset| plan.iter().position(|w| w.offset == offset).unwrap();
        // MPS before anything decodes; Command after the BAR.
        assert!(at(0x78) < at(0x10));
        assert!(at(0x10) < at(0x04));
        assert_eq!(plan.last().unwrap().offset, 0x04);
        assert_eq!(plan[at(0x78)].value, 0x2830);
        assert_eq!(plan[at(0x10)].value, 0x7a00_0000);
        assert_eq!(
            plan[at(0x04)],
            Write {
                offset: 0x04,
                bytes: 2,
                value: 6
            }
        );
        // Device Control 2 and the AER masks.
        assert_eq!(plan[at(0x98)].value, 5);
        assert_eq!(plan[at(0x10c)].value, 0x0046_2030);
        // Read-only fields are left alone.
        assert!(plan.iter().all(|w| w.offset != 0x00 && w.offset != 0x08));
    }

    #[test]
    fn writing_back_never_retrains_the_link() {
        let plan = restore_plan(&saved());
        let link = plan.iter().find(|w| w.offset == 0x80).unwrap();
        assert_eq!(link.value, 0x40);
    }

    /// Without root only 64 bytes can be read. The header still goes back.
    #[test]
    fn a_short_config_space_restores_the_header() {
        let plan = restore_plan(&saved()[..64]);
        let offsets: Vec<u32> = plan.iter().map(|w| w.offset).collect();
        assert_eq!(
            offsets,
            [0x0c, 0x10, 0x14, 0x18, 0x1c, 0x20, 0x24, 0x3c, 0x04]
        );
    }

    #[test]
    fn only_a_card_that_answers_is_worth_saving() {
        let mut c = saved();
        assert!(answering(&c, 0x7a00_0000));
        // Assigned elsewhere by the kernel.
        assert!(!answering(&c, 0x7b00_0000));
        // Just programmed: the BAR and Command are 0.
        c[0x04] = 0;
        c[0x10..0x14].copy_from_slice(&[0; 4]);
        assert!(!answering(&c, 0x7a00_0000));
    }

    #[test]
    fn a_change_of_shape_is_named() {
        let now = Shape {
            vendor: 0x1234,
            device: 1,
            class: Some(0xff0000),
            bar_bytes: 4096,
            intx: false,
        };
        assert!(changes(&now, &now).is_empty());
        let next = Shape {
            bar_bytes: 65536,
            intx: true,
            ..now.clone()
        };
        assert_eq!(
            changes(&now, &next),
            ["BAR0 4096 -> 65536 bytes", "interrupt pin none -> INTA"]
        );
        // An unknown class is not compared.
        let next = Shape {
            class: None,
            ..now.clone()
        };
        assert!(changes(&now, &next).is_empty());
    }

    #[test]
    fn the_card_is_waited_for_until_it_answers() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("0000:03:00.0");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("config"), [0xff; 64]).unwrap();
        let err = wait_for_card(
            root.path(),
            "0000:03:00.0",
            0x1234,
            1,
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(err.contains("0xffffffff"), "{err}");
        fs::write(dir.join("config"), &saved()[..64]).unwrap();
        wait_for_card(
            root.path(),
            "0000:03:00.0",
            0x1234,
            1,
            Duration::from_secs(1),
        )
        .unwrap();
    }

    #[test]
    fn writes_land_at_their_offsets() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("0000:03:00.0");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("config"), [0u8; 256]).unwrap();
        let plan = restore_plan(&saved()[..256]);
        write_config(root.path(), "0000:03:00.0", &plan).unwrap();
        let back = read_config(root.path(), "0000:03:00.0").unwrap();
        assert_eq!(&back[0x10..0x14], &0x7a00_0000u32.to_le_bytes());
        assert_eq!(&back[0x78..0x7a], &0x2830u16.to_le_bytes());
        assert_eq!(&back[0x04..0x06], &6u16.to_le_bytes());
    }
}

//! Reaching the window through a PCIe BAR.
//!
//! This maps the physical address of the card's BAR into the process, so a
//! 32-bit load or store becomes a TLP. No driver is needed.
//!
//! ```text
//! /sys/bus/pci/devices/0000:04:00.0/
//!   vendor / device     "0x1234" / "0x0001"      used to find the card
//!   resource            start end flags, one line per BAR
//!   enable              writing 1 turns on memory decoding
//!   resource0           this is mmapped
//! ```
//!
//! This uses sysfs, not VFIO: it changes no machine state, so it is light to
//! try on a shared server. Both end in "a `*mut u32` and a length", so moving to
//! VFIO changes only this file.
//!
//! ## 32-bit accesses only
//!
//! The borrowed `pcie_us_axil_master.v` rejects requests whose dword count is
//! not 1. A read gets Completer Abort; a write is posted, so it is lost without
//! a trace. So only `read32` / `write32` exist. If the BAR were a `&mut [u32]`
//! for `copy_from_slice`, the compiler could pick a 64-bit or SIMD store, and
//! the write would be lost. This is a correctness rule, so the type enforces it.
//!
//! ## Tests
//!
//! `open_at` takes the sysfs root, so tests run on a fake sysfs without a card.
//! A plain file can be mmapped as `resource0`, so tests use the same code path
//! to the end.

use std::fs;
use std::path::{Path, PathBuf};

/// The real sysfs.
pub const SYSFS: &str = "/sys/bus/pci/devices";

/// Flag of a memory BAR (`IORESOURCE_MEM`).
const IORESOURCE_MEM: u64 = 0x0000_0200;

#[derive(Debug)]
pub enum Error {
    /// No device with that BDF.
    NoDevice { bdf: String, path: PathBuf },
    /// A sysfs file exists but cannot be read or has an unexpected shape.
    Sysfs { path: PathBuf, why: String },
    /// Memory decoding is off.
    Disabled { bdf: String, path: PathBuf },
    /// BAR0 is an I/O BAR. The harness declares a 32-bit non-prefetchable
    /// memory BAR, so another bitstream is loaded.
    NotMemory { bdf: String, flags: u64 },
    /// The device's BAR register is 0: reprogramming cleared its config space.
    NotDecoding { bdf: String, assigned: u64 },
    /// `resource0` cannot be opened.
    Open { path: PathBuf, why: std::io::Error },
    /// mmap failed.
    Map { path: PathBuf, why: std::io::Error },
    /// Outside the BAR. On PCIe such a read returns 0 and a write is lost, and
    /// the host cannot tell 0 from a real value, so it is refused here.
    OutOfWindow { offset: u32, bytes: usize },
    /// Not on a word boundary.
    Unaligned { offset: u32 },
    /// This OS has no way to reach a BAR.
    Unsupported,
    /// No card with that ID. Endpoints are enumerated once at boot, so a card
    /// programmed after boot is not visible until a reboot.
    NotFound { vendor: u16, device: u16 },
    /// Several cards have that ID. The default ID comes from a verilog-pcie
    /// example, so two harness boards look the same from outside.
    Ambiguous {
        vendor: u16,
        device: u16,
        found: Vec<String>,
    },
    /// The BAR size differs from the size in `regs.json`.
    WrongSize {
        bdf: String,
        found: u64,
        expected: u64,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoDevice { bdf, path } => write!(
                f,
                "no PCI device {bdf} ({} does not exist).\n\
                 `lspci -D` lists the cards. The BDF is the first column (0000:04:00.0). \
                 If the card is not listed, its bitstream has no PCIe endpoint, or the \
                 machine was not rebooted after programming.",
                path.display()
            ),
            Error::Sysfs { path, why } => write!(f, "cannot read {}: {why}", path.display()),
            Error::Disabled { bdf, path } => write!(
                f,
                "PCI device {bdf} has memory decoding turned off. Reads return \
                 0xffffffff and writes are lost.\n\
                 Turn it on:  echo 1 | sudo tee {}\n\
                 This is normal when no driver owns the device.",
                path.display()
            ),
            Error::NotMemory { bdf, flags } => write!(
                f,
                "BAR0 of {bdf} is not a memory BAR (flags {flags:#x}).\n\
                 The bitstream on the board is not the one this map came from."
            ),
            Error::NotDecoding { bdf, assigned } => write!(
                f,
                "BAR0 of {bdf} reads back 0, but the kernel assigned {assigned:#x}.\n\
                 Programming the FPGA while the link was up cleared its config space. \
                 Re-enumerate:\n\
                 \x20   echo 1 | sudo tee /sys/bus/pci/devices/{bdf}/remove\n\
                 \x20   echo 1 | sudo tee /sys/bus/pci/rescan"
            ),
            Error::Open { path, why } => {
                if why.kind() == std::io::ErrorKind::PermissionDenied {
                    write!(
                        f,
                        "cannot open {} ({why}).\n\
                         The sysfs resource files are root-only. Use sudo.",
                        path.display()
                    )
                } else {
                    write!(f, "cannot open {}: {why}", path.display())
                }
            }
            Error::Map { path, why } => write!(f, "cannot map {}: {why}", path.display()),
            Error::OutOfWindow { offset, bytes } => write!(
                f,
                "offset {offset:#x} is past the end of the BAR ({bytes} bytes).\n\
                 The card would return 0 there without an error, so hio refuses it."
            ),
            Error::Unaligned { offset } => write!(
                f,
                "offset {offset:#x} is not on a word boundary.\n\
                 The window takes one 32-bit word at a time. Use a multiple of 4."
            ),
            Error::NotFound { vendor, device } => write!(
                f,
                "no PCI device with ID {vendor:04x}:{device:04x} (the ID in regs.json).\n\
                 `lspci -Dnn` lists the cards. If the card is in the slot, it runs \
                 another bitstream, or the machine was not rebooted after programming. \
                 Linux finds the card only at boot."
            ),
            Error::Ambiguous {
                vendor,
                device,
                found,
            } => write!(
                f,
                "more than one PCI device reports {vendor:04x}:{device:04x}:\n{}\n\
                 Pick one with --bdf. To tell boards apart, set [pcie] vendor_id / \
                 device_id in Harness.toml.",
                found
                    .iter()
                    .map(|bdf| format!("    {bdf}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            Error::WrongSize {
                bdf,
                found,
                expected,
            } => write!(
                f,
                "BAR0 of {bdf} is {found} bytes, but regs.json says {expected} bytes.\n\
                 The bitstream on the board is not the one this map came from. \
                 Re-generate and re-synthesize, or point --regs at the map that was \
                 built."
            ),
            Error::Unsupported => write!(
                f,
                "reaching a PCI BAR needs Linux sysfs. Use --transport jtag."
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Facts about one device, read from sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub bdf: String,
    pub vendor: u16,
    pub device: u16,
    /// Start of BAR0 as the kernel assigned it.
    pub bar0_start: u64,
    /// Size of BAR0 in bytes.
    pub bar0_bytes: u64,
    /// The BAR0 register in the device itself, with the low flag bits cleared.
    ///
    /// This can differ from the kernel's assignment. Reprogramming clears the
    /// config space and the register goes back to 0, while the kernel keeps the
    /// old assignment (`lspci` shows `[virtual]`). `None` when unreadable.
    pub bar0_reg: Option<u32>,
    /// BAR0 flags. Used to check that it is a memory BAR.
    pub bar0_flags: u64,
    /// Whether memory decoding is on.
    pub enabled: bool,
    /// The class code (sysfs `class`). `None` when unreadable: it is only for
    /// diagnosis.
    pub class: Option<u32>,
    /// The Interrupt Pin (config space 0x3d): 0 for none, 1 for INTA.
    /// `None` when unreadable.
    pub interrupt_pin: Option<u8>,
    /// The IRQ the kernel gave the card (sysfs `irq`). 0 when it has none.
    pub irq: Option<u32>,
}

/// Link speed and width, and their maximum.
///
/// The maximum is this device's own (`max_link_*` in sysfs), not the lower of
/// slot and card. A lower current value means the slot or the other side
/// limits it (the VCU118 is an x8 card, and it trained at x4 on one host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Written like `8.0 GT/s`: the sysfs value without the trailing `PCIe`.
    pub speed: String,
    pub width: u32,
    pub max_speed: String,
    pub max_width: u32,
}

/// Read the link state from sysfs. `None` when unreadable: it is only for
/// diagnosis, and nothing else depends on it.
pub fn link_at(root: &Path, bdf: &str) -> Option<Link> {
    let dir = root.join(bdf);
    let text = |name: &str| read_trimmed(&dir.join(name)).ok();
    let speed = |name: &str| text(name).map(|s| s.trim_end_matches(" PCIe").to_string());
    let width = |name: &str| text(name).and_then(|s| s.parse().ok());
    Some(Link {
        speed: speed("current_link_speed")?,
        width: width("current_link_width")?,
        max_speed: speed("max_link_speed")?,
        max_width: width("max_link_width")?,
    })
}

/// Collect the facts about one device from sysfs.
pub fn info_at(root: &Path, bdf: &str) -> Result<Info, Error> {
    let dir = root.join(bdf);
    if !dir.is_dir() {
        return Err(Error::NoDevice {
            bdf: bdf.to_string(),
            path: dir,
        });
    }

    let hex16 = |name: &str| -> Result<u16, Error> {
        let path = dir.join(name);
        let text = read_trimmed(&path)?;
        u16::from_str_radix(text.trim_start_matches("0x"), 16).map_err(|why| Error::Sysfs {
            path,
            why: format!("`{text}` is not a 16-bit hex value ({why})"),
        })
    };

    // BAR0 is the first line of `resource`: start, end, flags in hex.
    let path = dir.join("resource");
    let text = read_trimmed(&path)?;
    let line = text.lines().next().unwrap_or_default();
    let fields: Vec<u64> = line
        .split_whitespace()
        .map(|x| u64::from_str_radix(x.trim_start_matches("0x"), 16).unwrap_or(u64::MAX))
        .collect();
    if fields.len() < 3 || fields.contains(&u64::MAX) {
        return Err(Error::Sysfs {
            path,
            why: format!("cannot read a BAR0 line out of `{line}`"),
        });
    }
    // An empty BAR has start and end 0. Its size is 0, not end - start + 1.
    let bar0_bytes = if fields[1] >= fields[0] && fields[1] != 0 {
        fields[1] - fields[0] + 1
    } else {
        0
    };

    // Any user can read the first 64 bytes of config space; BAR0 is at 0x10.
    // It is only for diagnosis, so an unreadable file is not an error.
    let config = fs::read(dir.join("config")).ok();
    let bar0_reg = config
        .as_ref()
        .filter(|raw| raw.len() >= 0x14)
        .map(|raw| u32::from_le_bytes([raw[0x10], raw[0x11], raw[0x12], raw[0x13]]) & !0xf);
    let interrupt_pin = config.as_ref().and_then(|raw| raw.get(0x3d).copied());

    Ok(Info {
        bdf: bdf.to_string(),
        vendor: hex16("vendor")?,
        device: hex16("device")?,
        bar0_start: fields[0],
        bar0_bytes,
        bar0_reg,
        bar0_flags: fields[2],
        enabled: read_trimmed(&dir.join("enable"))? != "0",
        class: read_trimmed(&dir.join("class"))
            .ok()
            .and_then(|text| u32::from_str_radix(text.trim_start_matches("0x"), 16).ok()),
        interrupt_pin,
        irq: read_trimmed(&dir.join("irq"))
            .ok()
            .and_then(|text| text.parse().ok()),
    })
}

fn read_trimmed(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path)
        .map(|text| text.trim().to_string())
        .map_err(|why| Error::Sysfs {
            path: path.to_path_buf(),
            why: why.to_string(),
        })
}

/// List all devices in sysfs. Unreadable ones are skipped: another device's
/// problem must not stop us.
pub fn devices_at(root: &Path) -> Vec<Info> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<Info> = entries
        .flatten()
        .filter_map(|entry| {
            let bdf = entry.file_name().to_string_lossy().to_string();
            info_at(root, &bdf).ok()
        })
        .collect();
    // A fixed order, so the list of several matches is the same every run.
    found.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    found
}

/// Find the one card with this ID.
///
/// The default ID is borrowed from an example and can collide with other cards,
/// so this only narrows the choice. Reading the magic before any write is what
/// finally catches a wrong card.
pub fn find_at(root: &Path, vendor: u16, device: u16) -> Result<Info, Error> {
    let mut matched: Vec<Info> = devices_at(root)
        .into_iter()
        .filter(|info| info.vendor == vendor && info.device == device)
        .collect();
    match matched.len() {
        0 => Err(Error::NotFound { vendor, device }),
        1 => Ok(matched.remove(0)),
        _ => Err(Error::Ambiguous {
            vendor,
            device,
            found: matched.into_iter().map(|info| info.bdf).collect(),
        }),
    }
}

/// A mapped BAR.
///
/// It offers only 32-bit volatile accesses (see the module doc).
pub struct Bar {
    base: *mut u32,
    bytes: usize,
    info: Info,
}

impl Bar {
    /// Open from the real sysfs.
    pub fn open(bdf: &str) -> Result<Self, Error> {
        Self::open_at(Path::new(SYSFS), bdf)
    }

    /// Open under the given sysfs root. Tests pass a fake sysfs.
    pub fn open_at(root: &Path, bdf: &str) -> Result<Self, Error> {
        let info = info_at(root, bdf)?;
        // A device without an address must be fixed first. Turning on decoding
        // does not help, since there is nothing to decode, and reads return
        // `0xffffffff` (seen on a VCU118).
        if info.bar0_reg == Some(0) && info.bar0_start != 0 {
            return Err(Error::NotDecoding {
                bdf: bdf.to_string(),
                assigned: info.bar0_start,
            });
        }
        // Do not go on with decoding off. mmap succeeds, but every read returns
        // `0xffffffff`: the link is up and nothing reads. A test host was in
        // this state.
        if !info.enabled {
            return Err(Error::Disabled {
                bdf: bdf.to_string(),
                path: root.join(bdf).join("enable"),
            });
        }
        if info.bar0_flags & IORESOURCE_MEM == 0 {
            return Err(Error::NotMemory {
                bdf: bdf.to_string(),
                flags: info.bar0_flags,
            });
        }
        let bytes = info.bar0_bytes as usize;
        let base = map(&root.join(bdf).join("resource0"), bytes)?;
        Ok(Bar { base, bytes, info })
    }

    /// Check the BAR size against `regs.json`.
    ///
    /// A matching ID is not enough. A different size means the loaded bitstream
    /// is not the one this map came from.
    pub fn expect_bytes(&self, expected: u64) -> Result<(), Error> {
        if self.bytes as u64 != expected {
            return Err(Error::WrongSize {
                bdf: self.info.bdf.clone(),
                found: self.bytes as u64,
                expected,
            });
        }
        Ok(())
    }

    /// Size of the BAR in bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    /// Read one word.
    pub fn read32(&self, offset: u32) -> Result<u32, Error> {
        let index = self.index(offset)?;
        // SAFETY: `index` is checked to be inside `bytes`. The target is MMIO,
        // so the read is volatile: it must not be removed, merged or reordered.
        Ok(unsafe { std::ptr::read_volatile(self.base.add(index)) })
    }

    /// Write one word.
    pub fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
        let index = self.index(offset)?;
        // SAFETY: same as `read32`.
        unsafe { std::ptr::write_volatile(self.base.add(index), value) };
        Ok(())
    }

    /// Run a batch.
    ///
    /// A BAR does not drop commands, so there is no resending as on JTAG, and
    /// `Batch::replayable` has no effect here. Each access is one TLP, so a
    /// batch saves no round trips, but writes are posted and go out quickly.
    ///
    /// A read does not pass an earlier write (PCI ordering rules), so a read
    /// after a write in the same batch sees the written value.
    pub fn run(&self, batch: &crate::bridge::Batch) -> Result<crate::bridge::Reads, Error> {
        let mut values = Vec::new();
        for (frame, _) in batch.ops() {
            match frame.op {
                crate::frame::Op::Write => self.write32(frame.addr, frame.wdata)?,
                crate::frame::Op::Read => values.push(self.read32(frame.addr)?),
                crate::frame::Op::Nop => {}
            }
        }
        Ok(crate::bridge::Reads::new(values))
    }

    /// Read consecutive words. Same signature as on `Bridge`.
    pub fn read_burst(&self, addr: u32, out: &mut [u32]) -> Result<(), Error> {
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.read32(addr + 4 * i as u32)?;
        }
        Ok(())
    }

    /// Read consecutive words with several threads.
    ///
    /// MMIO reads are limited by round trips, not by link speed. The core stalls
    /// while one load fetches 4 bytes (1.30 us on a VCU118), and the link is idle
    /// all that time. Other cores can have their own reads in flight, so the
    /// waits overlap. Writes take 0.12 us because they are posted; reads cannot
    /// do that. This hides latency; it does not add bandwidth.
    ///
    /// Only reads are offered. It takes `&mut self`, so nothing else touches the
    /// BAR during the call, and the threads only read.
    ///
    /// This does not move the base of a sliding window. There is one base
    /// register per master, so parallel reads across pages would move each
    /// other's window. The caller splits the reads by page.
    pub fn read_parallel(
        &mut self,
        addr: u32,
        out: &mut [u32],
        threads: usize,
    ) -> Result<(), Error> {
        if threads <= 1 || out.len() <= 1 {
            return self.read_burst(addr, out);
        }
        // Check the range before starting threads, so no thread has to return
        // an error and `out` is never half filled.
        self.index(addr)?;
        let last = addr as u64 + 4 * (out.len() as u64 - 1);
        let last = u32::try_from(last).map_err(|_| Error::OutOfWindow {
            offset: addr,
            bytes: self.bytes,
        })?;
        self.index(last)?;

        let base = Shared(self.base);
        let first = addr as usize / 4;
        let per = out.len().div_ceil(threads);
        std::thread::scope(|s| {
            for (n, chunk) in out.chunks_mut(per).enumerate() {
                let from = first + n * per;
                s.spawn(move || {
                    for (i, slot) in chunk.iter_mut().enumerate() {
                        // SAFETY: `index` checked both ends against the mapping,
                        // and `chunks_mut` does not overlap. MMIO, so volatile.
                        *slot = unsafe { std::ptr::read_volatile(base.at(from + i)) };
                    }
                });
            }
        });
        Ok(())
    }

    /// Turn a byte offset into a word index. Refuses offsets outside the BAR and
    /// unaligned offsets.
    ///
    /// On PCIe, reads outside the BAR return 0, so only the host can catch them.
    /// (The JTAG bridge wraps such addresses instead, and `offset_of` in `hio`
    /// catches those.)
    fn index(&self, offset: u32) -> Result<usize, Error> {
        if !offset.is_multiple_of(4) {
            return Err(Error::Unaligned { offset });
        }
        if offset as usize + 4 > self.bytes {
            return Err(Error::OutOfWindow {
                offset,
                bytes: self.bytes,
            });
        }
        Ok(offset as usize / 4)
    }
}

/// Base of the mapping, to pass to threads.
///
/// A raw pointer is neither `Send` nor `Sync`. Here it is safe to share because
/// `read_parallel` holds `&mut Bar`, so the threads only do shared reads.
#[derive(Clone, Copy)]
struct Shared(*mut u32);

impl Shared {
    /// Use this method, not `.0`, inside a closure. With `.0`, the closure
    /// captures the raw pointer itself (disjoint capture, edition 2021), not
    /// `Shared`, and it does not compile.
    fn at(self, index: usize) -> *mut u32 {
        // SAFETY: the caller (`read_parallel`) checked the range.
        unsafe { self.0.add(index) }
    }
}

// SAFETY: only reads go through this (see above).
unsafe impl Send for Shared {}
// SAFETY: same as above.
unsafe impl Sync for Shared {}

impl Drop for Bar {
    fn drop(&mut self) {
        unmap(self.base, self.bytes);
    }
}

impl std::fmt::Debug for Bar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bar")
            .field("bdf", &self.info.bdf)
            .field("bytes", &self.bytes)
            .finish()
    }
}

#[cfg(unix)]
fn map(path: &Path, bytes: usize) -> Result<*mut u32, Error> {
    use std::os::fd::AsRawFd;

    // Open for writing too: the window is for writing registers.
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|why| Error::Open {
            path: path.to_path_buf(),
            why,
        })?;

    // SAFETY: the fd was just opened, and the length is the BAR size from sysfs.
    // It must be `MAP_SHARED`: with `MAP_PRIVATE`, writes go to a private copy
    // and never reach the device.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(Error::Map {
            path: path.to_path_buf(),
            why: std::io::Error::last_os_error(),
        });
    }
    // The file may close here; the mapping does not depend on the fd.
    Ok(addr as *mut u32)
}

#[cfg(not(unix))]
fn map(_path: &Path, _bytes: usize) -> Result<*mut u32, Error> {
    Err(Error::Unsupported)
}

#[cfg(unix)]
fn unmap(base: *mut u32, bytes: usize) {
    // SAFETY: `base` and `bytes` are exactly what `map` returned and was given.
    unsafe { libc::munmap(base as *mut libc::c_void, bytes) };
}

#[cfg(not(unix))]
fn unmap(_base: *mut u32, _bytes: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake sysfs. `resource0` is a plain file, which mmap accepts, so
    /// the code path is the same as on real hardware.
    fn fake_sysfs(bytes: u64, flags: u64, enabled: bool) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        add_device(
            root.path(),
            "0000:04:00.0",
            0x1234,
            0x0001,
            bytes,
            flags,
            enabled,
        );
        root
    }

    /// A VCU118 on one test host looks like this: an x8 card trained at x4.
    #[test]
    fn a_link_narrower_than_the_card_is_read_as_such() {
        let root = fake_sysfs(4096, IORESOURCE_MEM, true);
        let dir = root.path().join("0000:04:00.0");
        for (name, value) in [
            ("current_link_speed", "8.0 GT/s PCIe\n"),
            ("current_link_width", "4\n"),
            ("max_link_speed", "8.0 GT/s PCIe\n"),
            ("max_link_width", "8\n"),
        ] {
            fs::write(dir.join(name), value).unwrap();
        }
        assert_eq!(
            link_at(root.path(), "0000:04:00.0"),
            Some(Link {
                speed: "8.0 GT/s".into(),
                width: 4,
                max_speed: "8.0 GT/s".into(),
                max_width: 8,
            })
        );
        // An unreadable file means no link information.
        fs::remove_file(dir.join("max_link_width")).unwrap();
        assert_eq!(link_at(root.path(), "0000:04:00.0"), None);
    }

    /// This cannot measure speed, but it checks the split arithmetic. A thread
    /// that reads one word off would show on real hardware only as a value
    /// that is sometimes shifted by one.
    #[test]
    fn reading_in_parallel_returns_the_same_words_in_the_same_order() {
        let root = fake_sysfs(4096, IORESOURCE_MEM, true);
        // Each word holds its own index, so a shift shows in the values.
        let raw: Vec<u8> = (0..1024u32)
            .flat_map(|i| (0xc0de_0000 | i).to_le_bytes())
            .collect();
        fs::write(root.path().join("0000:04:00.0").join("resource0"), raw).unwrap();

        let mut bar = Bar::open_at(root.path(), "0000:04:00.0").unwrap();
        let want: Vec<u32> = (0..1024u32).map(|i| 0xc0de_0000 | i).collect();

        // Include counts that do not divide 1024 (3, 7): the last thread gets
        // a shorter chunk, which is where the arithmetic breaks.
        for threads in [1usize, 2, 3, 7, 8, 64] {
            let mut got = vec![0u32; 1024];
            bar.read_parallel(0, &mut got, threads).unwrap();
            assert_eq!(got, want, "threads={threads}");
        }

        // From an offset.
        let mut got = vec![0u32; 100];
        bar.read_parallel(400, &mut got, 4).unwrap();
        assert_eq!(got, want[100..200], "from an offset");

        // Fewer words than threads.
        let mut got = vec![0u32; 3];
        bar.read_parallel(0, &mut got, 8).unwrap();
        assert_eq!(got, want[..3]);
    }

    #[test]
    fn a_parallel_read_past_the_window_is_refused() {
        let root = fake_sysfs(4096, IORESOURCE_MEM, true);
        let mut bar = Bar::open_at(root.path(), "0000:04:00.0").unwrap();

        let mut got = vec![0u32; 2];
        // The last word is one word past the end.
        let err = bar.read_parallel(4092, &mut got, 4).unwrap_err();
        assert!(matches!(err, Error::OutOfWindow { .. }), "{err:?}");

        // An unaligned offset too.
        let err = bar.read_parallel(2, &mut got, 4).unwrap_err();
        assert!(matches!(err, Error::Unaligned { .. }), "{err:?}");
    }

    #[allow(clippy::too_many_arguments)]
    fn add_device(
        root: &Path,
        bdf: &str,
        vendor: u16,
        device: u16,
        bytes: u64,
        flags: u64,
        enabled: bool,
    ) {
        let dir = root.join(bdf);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("vendor"), format!("0x{vendor:04x}\n")).unwrap();
        fs::write(dir.join("device"), format!("0x{device:04x}\n")).unwrap();
        fs::write(
            dir.join("resource"),
            format!(
                "0x000000007a000000 0x{:016x} 0x{flags:016x}\n\
                 0x0000000000000000 0x0000000000000000 0x0000000000000000\n",
                0x7a00_0000u64 + bytes - 1
            ),
        )
        .unwrap();
        fs::write(dir.join("enable"), if enabled { "1\n" } else { "0\n" }).unwrap();
        fs::write(dir.join("class"), "0xff0000\n").unwrap();
        fs::write(dir.join("irq"), "16\n").unwrap();
        fs::write(dir.join("resource0"), vec![0u8; bytes as usize]).unwrap();
        write_config(&dir, 0x7a00_0000);
    }

    /// The first 64 bytes of config space. Only BAR0 (0x10) has a value.
    fn write_config(dir: &Path, bar0: u32) {
        let mut raw = vec![0u8; 64];
        raw[0x10..0x14].copy_from_slice(&bar0.to_le_bytes());
        raw[0x3d] = 1;
        fs::write(dir.join("config"), raw).unwrap();
    }

    #[test]
    fn the_facts_come_from_sysfs() {
        let root = fake_sysfs(4096, 0x40200, true);
        let info = info_at(root.path(), "0000:04:00.0").unwrap();
        assert_eq!(info.vendor, 0x1234);
        assert_eq!(info.device, 0x0001);
        assert_eq!(info.bar0_bytes, 4096);
        assert!(info.enabled);
        assert_eq!(info.class, Some(0xff0000));
        assert_eq!(info.interrupt_pin, Some(1));
        assert_eq!(info.irq, Some(16));
    }

    #[test]
    fn a_word_written_reads_back() {
        let root = fake_sysfs(4096, 0x40200, true);
        let bar = Bar::open_at(root.path(), "0000:04:00.0").unwrap();
        assert_eq!(bar.bytes(), 4096);

        bar.write32(0x18, 0x5648_524e).unwrap();
        assert_eq!(bar.read32(0x18).unwrap(), 0x5648_524e);
        // The neighbours stay 0. A wrong access width would change them.
        assert_eq!(bar.read32(0x14).unwrap(), 0);
        assert_eq!(bar.read32(0x1c).unwrap(), 0);
    }

    /// On PCIe such a read just returns 0, which the host cannot tell from a
    /// real 0.
    #[test]
    fn an_offset_past_the_bar_is_refused() {
        let root = fake_sysfs(4096, 0x40200, true);
        let bar = Bar::open_at(root.path(), "0000:04:00.0").unwrap();
        let err = bar.read32(4096).unwrap_err().to_string();
        assert!(err.contains("past the end"), "{err}");
        assert!(err.contains("4096"), "{err}");
        assert!(bar.write32(4096, 1).is_err());
        // The last word just fits.
        assert!(bar.read32(4092).is_ok());
    }

    /// An unaligned offset is not silently rounded down to a word.
    #[test]
    fn an_unaligned_offset_is_refused() {
        let root = fake_sysfs(4096, 0x40200, true);
        let bar = Bar::open_at(root.path(), "0000:04:00.0").unwrap();
        let err = bar.read32(0x12).unwrap_err().to_string();
        assert!(err.contains("word boundary"), "{err}");
    }

    /// Without this check, every read returns `0xffffffff` and looks like a
    /// link that is down.
    #[test]
    fn a_disabled_device_says_how_to_turn_it_on() {
        let root = fake_sysfs(4096, 0x40200, false);
        let err = Bar::open_at(root.path(), "0000:04:00.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("memory decoding"), "{err}");
        assert!(err.contains("echo 1 | sudo tee"), "{err}");
        assert!(err.contains("enable"), "{err}");
    }

    #[test]
    fn an_absent_device_says_how_to_look() {
        let root = fake_sysfs(4096, 0x40200, true);
        let err = Bar::open_at(root.path(), "0000:99:00.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no PCI device 0000:99:00.0"), "{err}");
        assert!(err.contains("lspci -D"), "{err}");
    }

    #[test]
    fn the_id_from_the_map_finds_the_card() {
        let root = fake_sysfs(4096, 0x40200, true);
        // Another device on the same machine.
        add_device(
            root.path(),
            "0000:01:00.0",
            0x10ee,
            0x7024,
            65536,
            0x40200,
            true,
        );
        let info = find_at(root.path(), 0x1234, 0x0001).unwrap();
        assert_eq!(info.bdf, "0000:04:00.0");
        assert_eq!(devices_at(root.path()).len(), 2);
    }

    #[test]
    fn an_id_that_is_not_there_says_how_to_look() {
        let root = fake_sysfs(4096, 0x40200, true);
        let err = find_at(root.path(), 0x1234, 0x0002)
            .unwrap_err()
            .to_string();
        assert!(err.contains("1234:0002"), "{err}");
        assert!(err.contains("lspci"), "{err}");
        // It mentions a reboot: reprogramming alone does not re-enumerate.
        assert!(err.contains("rebooted"), "{err}");
    }

    /// The default ID is borrowed, so two cards look the same.
    #[test]
    fn two_cards_with_the_same_id_ask_for_a_bdf() {
        let root = fake_sysfs(4096, 0x40200, true);
        add_device(
            root.path(),
            "0000:05:00.0",
            0x1234,
            0x0001,
            4096,
            0x40200,
            true,
        );
        let err = find_at(root.path(), 0x1234, 0x0001)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--bdf"), "{err}");
        assert!(err.contains("0000:04:00.0"), "{err}");
        assert!(err.contains("0000:05:00.0"), "{err}");
    }

    /// A different size means a different bitstream, even with the same ID.
    #[test]
    fn a_bar_of_the_wrong_size_is_refused() {
        let root = fake_sysfs(8192, 0x40200, true);
        let bar = Bar::open_at(root.path(), "0000:04:00.0").unwrap();
        let err = bar.expect_bytes(4096).unwrap_err().to_string();
        assert!(err.contains("8192"), "{err}");
        assert!(err.contains("4096"), "{err}");
        assert!(err.contains("not the one this map came from"), "{err}");
        assert!(bar.expect_bytes(8192).is_ok());
    }

    /// Reprogramming cleared the config space (seen on a VCU118). The link is up
    /// and `lspci` lists the card, but the BAR register is back to 0 and only
    /// the kernel keeps the old assignment (`lspci` shows `[virtual]`). Every
    /// access returns `0xffffffff`, which looks the same as a broken JTAG link.
    #[test]
    fn a_cleared_bar_register_says_to_re_enumerate() {
        let root = fake_sysfs(4096, 0x40200, true);
        write_config(&root.path().join("0000:04:00.0"), 0);
        let err = Bar::open_at(root.path(), "0000:04:00.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reads back 0"), "{err}");
        assert!(err.contains("rescan"), "{err}");
        // It does not suggest `enable`: there is no address to decode.
        assert!(!err.contains("memory decoding"), "{err}");
    }

    #[test]
    fn an_io_bar_is_refused() {
        let root = fake_sysfs(4096, 0x100, true);
        let err = Bar::open_at(root.path(), "0000:04:00.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a memory BAR"), "{err}");
    }
}

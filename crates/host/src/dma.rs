//! Host-side conditions for letting a card access host memory.
//!
//! These conditions can all be checked without a card:
//!
//! 1. The IOMMU does not translate for the card. The card gets physical
//!    addresses, so in translating mode nobody creates a mapping, and the
//!    accesses end in DMAR faults.
//! 2. Huge pages are reserved. Anonymous pages are not physically contiguous,
//!    so a transfer above 4KB needs one huge page.
//! 3. The physical address is real. Without root, `/proc/self/pagemap` returns
//!    the PFN as 0; without this check, the card gets physical address 0.
//! 4. The card may master the bus. With Bus Master Enable 0 in the command
//!    register, the card cannot send requests. A descriptor is accepted but
//!    nothing comes back, and the DMA engine stays busy. Setting the bit later
//!    does not recover it; only reprogramming does. No driver sets the bit, so
//!    a host reboot clears it (seen on a VCU118).
//!
//! The approach is the one AWS uses on F2
//! (`sdk/userspace/fpga_libs/fpga_dma/fpga_dma_mem.c`). It is not VFIO: F2 has
//! no VFIO path, and the same code should work on both.
//!
//! ## The cost: no protection
//!
//! With the IOMMU in pass-through, the card writes to the address it is given.
//! A wrong address overwrites whatever is there. So check 3 is a correctness
//! rule, and the AWS code does not do it.
//!
//! ## Tests
//!
//! The IOMMU and huge page checks take their root as an argument, so they run
//! on a fake sysfs / meminfo (as `open_at` in `pcie.rs` does). The pagemap
//! decoding is a pure function, so the refusal of PFN 0 is tested without root.

use std::path::{Path, PathBuf};

/// The real sysfs / procfs paths.
const IOMMU_GROUPS: &str = "/sys/kernel/iommu_groups";
const MEMINFO: &str = "/proc/meminfo";
const PAGEMAP: &str = "/proc/self/pagemap";

/// Bit 63 of a pagemap entry: the page is present.
const PAGEMAP_PRESENT: u64 = 1 << 63;
/// The PFN is the low 55 bits.
const PAGEMAP_PFN: u64 = (1 << 55) - 1;

/// 4KB page. The pagemap index uses this unit.
const PAGE_BYTES: usize = 4096;

#[derive(Debug)]
pub enum Error {
    /// The IOMMU translates for this device. DMA does not arrive until the group
    /// is in pass-through (nothing is mapped, so every write faults). The card
    /// is alone in its group and has no driver, so the group can switch without
    /// a reboot and without `iommu=pt`.
    Translating {
        bdf: String,
        group: String,
        kind: String,
    },
    /// Not enough huge pages. Transparent huge pages can be split and moved, so
    /// they cannot back DMA. On a fragmented machine the reservation can partly
    /// fail, so the message asks the user to read it back.
    NoHugepages {
        want: u64,
        free: u64,
        size_bytes: u64,
    },
    /// The huge page size is missing or 0.
    NoHugepageSize,
    /// pagemap says the page is not present.
    ///
    /// A non-root process also ends here: the kernel returns 0 entries to it,
    /// so the present bit is clear too.
    NotPresent {
        at: u64,
    },
    Io {
        what: PathBuf,
        err: std::io::Error,
    },
    /// mmap or mlock failed.
    Mmap {
        what: &'static str,
        err: std::io::Error,
    },
    /// The card may not master the bus (bit 2 of the command register is 0).
    /// A descriptor gets no answer and the DMA engine stays busy. No driver
    /// sets the bit, so a reboot clears it. `COMMAND=0x4:0x4` sets only that
    /// bit and leaves memory decoding alone.
    NoBusMaster {
        bdf: String,
    },
    /// Not possible on this OS.
    Unsupported,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Translating { bdf, group, kind } => write!(
                f,
                "the IOMMU is translating for {bdf} (group {group} is `{kind}`).\n\
                 The card cannot reach host memory like this. Switch the group to identity. \
                 No reboot is needed:\n\n\
                 \x20   echo identity | sudo tee {IOMMU_GROUPS}/{group}/type\n\n\
                 This removes the IOMMU's protection for the card. Set it back to `DMA-FQ` \
                 when you are done."
            ),
            Error::NoHugepages {
                want,
                free,
                size_bytes,
            } => write!(
                f,
                "{want} huge page(s) of {size_bytes} bytes are needed, and {free} are free.\n\
                 None are reserved by default, and transparent huge pages do not work for \
                 this. Reserve them:\n\n\
                 \x20   sudo sysctl -w vm.nr_hugepages={want}\n\n\
                 Then check `HugePages_Total` in /proc/meminfo. The reservation can partly fail."
            ),
            Error::NoHugepageSize => write!(
                f,
                "{MEMINFO} does not say `Hugepagesize`. This machine has no huge page support."
            ),
            Error::NotPresent { at } => write!(
                f,
                "{PAGEMAP} says nothing is mapped at 0x{at:x}.\n\
                 Without root, the kernel hides physical addresses, and the card would \
                 write to physical address 0. Use sudo."
            ),
            Error::Io { what, err } => write!(f, "{}: {err}", what.display()),
            Error::Mmap { what, err } => write!(f, "{what} failed: {err}"),
            Error::NoBusMaster { bdf } => write!(
                f,
                "{bdf} is not allowed to master the bus (Bus Master Enable is 0).\n\
                 A transfer would never finish, and the engine would stay busy until the card \
                 is reprogrammed. Turn it on:\n\n\
                 \x20   sudo setpci -s {bdf} COMMAND=0x4:0x4\n\n\
                 A reboot turns it off again."
            ),
            Error::Unsupported => write!(
                f,
                "handing host memory to a card needs Linux (sysfs, procfs and MAP_HUGETLB)."
            ),
        }
    }
}

impl std::error::Error for Error {}

fn io(what: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let what = what.into();
    move |err| Error::Io { what, err }
}

/// Whether the IOMMU translates for this device.
///
/// Only this yes/no matters. "No IOMMU" and "pass-through" both give physical
/// addresses, so they are not told apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Iommu {
    /// Nothing translates: there is no IOMMU, or it does not manage this device.
    ///
    /// F2 is in this state (the guest has no IOMMU), so "not found" is normal.
    Absent,
    /// Pass-through. IOVA maps 1:1 to physical addresses.
    Identity { group: String },
    /// Translating. DMA does not arrive as it is.
    Translating { group: String, kind: String },
}

impl Iommu {
    /// Whether physical addresses can be given to the card as they are.
    pub fn passes_addresses_through(&self) -> bool {
        !matches!(self, Iommu::Translating { .. })
    }
}

/// Check in the real sysfs.
pub fn iommu(bdf: &str) -> Result<Iommu, Error> {
    iommu_at(Path::new(IOMMU_GROUPS), bdf)
}

/// Check under the given root. Tests pass a fake tree.
pub fn iommu_at(root: &Path, bdf: &str) -> Result<Iommu, Error> {
    let Ok(groups) = std::fs::read_dir(root) else {
        // No tree at all: no IOMMU is active.
        return Ok(Iommu::Absent);
    };
    for group in groups.flatten() {
        let name = group.file_name().to_string_lossy().into_owned();
        if !group.path().join("devices").join(bdf).exists() {
            continue;
        }
        let kind = std::fs::read_to_string(group.path().join("type"))
            .map_err(io(group.path().join("type")))?
            .trim()
            .to_string();
        // Every type except `identity` counts as translating. Treating an
        // unknown type as pass-through would hide the cause of DMAR faults.
        return Ok(if kind == "identity" {
            Iommu::Identity { group: name }
        } else {
            Iommu::Translating { group: name, kind }
        });
    }
    // The tree exists, but the device is in no group: not translated.
    Ok(Iommu::Absent)
}

/// Bus Master Enable in the command register (config space 0x04).
const COMMAND_BUS_MASTER: u16 = 1 << 2;

/// Whether the card may send requests as a bus master. Checks the real sysfs.
pub fn bus_master(bdf: &str) -> Result<bool, Error> {
    bus_master_at(Path::new(crate::pcie::SYSFS), bdf)
}

/// Check under the given root. Tests pass a fake tree.
///
/// Any user can read the first 64 bytes of config space, so `check` gives an
/// answer without sudo.
pub fn bus_master_at(root: &Path, bdf: &str) -> Result<bool, Error> {
    let path = root.join(bdf).join("config");
    let raw = std::fs::read(&path).map_err(io(&path))?;
    let Some(command) = raw.get(4..6) else {
        return Err(Error::Io {
            what: path,
            err: std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "shorter than the command register",
            ),
        });
    };
    Ok(u16::from_le_bytes([command[0], command[1]]) & COMMAND_BUS_MASTER != 0)
}

/// The card-side condition before a transfer. Refuses with the fix when the
/// bit is off.
fn require_bus_master(bdf: &str) -> Result<(), Error> {
    if bus_master(bdf)? {
        Ok(())
    } else {
        Err(Error::NoBusMaster {
            bdf: bdf.to_string(),
        })
    }
}

/// The huge page pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hugepages {
    pub total: u64,
    pub free: u64,
    pub size_bytes: u64,
}

pub fn hugepages() -> Result<Hugepages, Error> {
    let text = std::fs::read_to_string(MEMINFO).map_err(io(MEMINFO))?;
    hugepages_from(&text)
}

/// Parse the text of `/proc/meminfo`.
pub fn hugepages_from(meminfo: &str) -> Result<Hugepages, Error> {
    let field = |name: &str| -> Option<u64> {
        meminfo.lines().find_map(|line| {
            line.strip_prefix(name)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
    };
    let size_kb = field("Hugepagesize:").ok_or(Error::NoHugepageSize)?;
    if size_kb == 0 {
        return Err(Error::NoHugepageSize);
    }
    Ok(Hugepages {
        total: field("HugePages_Total:").unwrap_or(0),
        free: field("HugePages_Free:").unwrap_or(0),
        size_bytes: size_kb * 1024,
    })
}

/// Physical address from one pagemap entry.
///
/// This checks the present bit. The AWS code does not, so without root it gets
/// physical address 0 with no error. With the IOMMU in pass-through, giving
/// that to the card corrupts memory.
pub fn physical_from(entry: u64, offset: usize, at: u64) -> Result<u64, Error> {
    if entry & PAGEMAP_PRESENT == 0 {
        return Err(Error::NotPresent { at });
    }
    let pfn = entry & PAGEMAP_PFN;
    if pfn == 0 {
        return Err(Error::NotPresent { at });
    }
    Ok(pfn * PAGE_BYTES as u64 + offset as u64)
}

/// One piece of host memory that a card can access.
///
/// It is one huge page. Anonymous pages are not physically contiguous, so this
/// is the only way to give more than 4KB as one address. AWS draws the same line
/// (`sde_lib/sde_mem.c`: above 4KB, use a huge page).
///
/// With one page, no descriptor list is needed, and one outstanding transfer is
/// enough. 2MB takes a few hundred µs on PCIe, so setting up each transfer again
/// costs little.
pub struct Buffer {
    va: *mut u8,
    len: usize,
    bus: u64,
}

impl Buffer {
    /// Get one huge page.
    ///
    /// The conditions are checked before allocating, so a refusal can say why.
    pub fn huge(bdf: &str) -> Result<Buffer, Error> {
        let state = iommu(bdf)?;
        if let Iommu::Translating { group, kind } = state {
            return Err(Error::Translating {
                bdf: bdf.to_string(),
                group,
                kind,
            });
        }
        require_bus_master(bdf)?;
        let pages = hugepages()?;
        if pages.free == 0 {
            return Err(Error::NoHugepages {
                want: 1,
                free: pages.free,
                size_bytes: pages.size_bytes,
            });
        }
        map_huge(pages.size_bytes as usize)
    }

    /// Get one huge page even while the IOMMU translates. Only for the first
    /// bring-up step.
    ///
    /// With translation on, the IOMMU blocks the address the card sends, and
    /// `dmesg` logs it. If it matches the given address, the TLP header is
    /// right, and no memory was touched. The card cannot reach this address
    /// while translation is on, so the returned `Buffer` is a number to compare,
    /// not a place for data.
    ///
    /// Bus mastering is still required. A card without it sends no address, so
    /// the IOMMU blocks nothing and `dmesg` shows nothing.
    pub fn huge_while_translating(bdf: &str) -> Result<Buffer, Error> {
        require_bus_master(bdf)?;
        let pages = hugepages()?;
        if pages.free == 0 {
            return Err(Error::NoHugepages {
                want: 1,
                free: pages.free,
                size_bytes: pages.size_bytes,
            });
        }
        map_huge(pages.size_bytes as usize)
    }

    /// The address to give the card. The IOMMU passes addresses through, so it
    /// is the physical address.
    pub fn bus_addr(&self) -> u64 {
        self.bus
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping and its length come from `map_huge`.
        unsafe { std::slice::from_raw_parts(self.va, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as in `as_slice`; `&mut self` means nothing else touches it.
        unsafe { std::slice::from_raw_parts_mut(self.va, self.len) }
    }
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("len", &self.len)
            .field("bus", &format_args!("0x{:x}", self.bus))
            .finish()
    }
}

/// Linux only. `MAP_HUGETLB` exists only on Linux, and macOS is also `unix`,
/// so `cfg(unix)` would fail to compile on macOS.
#[cfg(target_os = "linux")]
fn map_huge(len: usize) -> Result<Buffer, Error> {
    // SAFETY: the length comes from `Hugepagesize`, and no fd is used.
    let va = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
            -1,
            0,
        )
    };
    if va == libc::MAP_FAILED {
        return Err(Error::Mmap {
            what: "mmap(MAP_HUGETLB)",
            err: std::io::Error::last_os_error(),
        });
    }
    let va = va as *mut u8;

    // Touch the page so it exists. pagemap answers only for present pages.
    // SAFETY: start of the mapping just made.
    unsafe { std::ptr::write_volatile(va, 0) };

    // Lock the page. AWS uses `mlockall(MCL_CURRENT)`; locking this range is
    // enough. mlock only stops swapping, not page migration. This relies on
    // hugetlb pages being outside normal compaction and NUMA balancing.
    // SAFETY: the range just mapped.
    if unsafe { libc::mlock(va as *const libc::c_void, len) } != 0 {
        let err = std::io::Error::last_os_error();
        // SAFETY: the mapping just made.
        unsafe { libc::munmap(va as *mut libc::c_void, len) };
        return Err(Error::Mmap { what: "mlock", err });
    }

    match physical_of(va) {
        Ok(bus) => Ok(Buffer { va, len, bus }),
        Err(err) => {
            // SAFETY: the mapping just made.
            unsafe { libc::munmap(va as *mut libc::c_void, len) };
            Err(err)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn map_huge(_len: usize) -> Result<Buffer, Error> {
    Err(Error::Unsupported)
}

/// Look up the physical address of `va` in `/proc/self/pagemap`. Linux only.
#[cfg(target_os = "linux")]
fn physical_of(va: *const u8) -> Result<u64, Error> {
    use std::io::{Read, Seek, SeekFrom};

    let at = va as u64;
    let page = at / PAGE_BYTES as u64;
    let offset = (at % PAGE_BYTES as u64) as usize;

    let mut file = std::fs::File::open(PAGEMAP).map_err(io(PAGEMAP))?;
    file.seek(SeekFrom::Start(page * 8)).map_err(io(PAGEMAP))?;
    let mut raw = [0u8; 8];
    file.read_exact(&mut raw).map_err(io(PAGEMAP))?;
    physical_from(u64::from_le_bytes(raw), offset, at)
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // Only Linux can create a `Buffer` (`map_huge`).
        #[cfg(target_os = "linux")]
        // SAFETY: `va` and `len` are exactly what `map_huge` mapped.
        unsafe {
            libc::munmap(self.va as *mut libc::c_void, self.len)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `/sys/kernel/iommu_groups`.
    fn fake_groups(entries: &[(&str, &str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (group, kind, bdf) in entries {
            let dir = root.path().join(group);
            std::fs::create_dir_all(dir.join("devices")).unwrap();
            std::fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
            std::fs::write(dir.join("devices").join(bdf), "").unwrap();
        }
        root
    }

    #[test]
    fn a_translating_group_is_told_apart_from_a_passed_through_one() {
        let bdf = "0000:04:00.0";

        // A test host started in this state.
        let root = fake_groups(&[("15", "DMA-FQ", bdf)]);
        assert_eq!(
            iommu_at(root.path(), bdf).unwrap(),
            Iommu::Translating {
                group: "15".into(),
                kind: "DMA-FQ".into()
            }
        );

        // After switching.
        let root = fake_groups(&[("15", "identity", bdf)]);
        assert_eq!(
            iommu_at(root.path(), bdf).unwrap(),
            Iommu::Identity { group: "15".into() }
        );

        // An unknown type counts as translating.
        let root = fake_groups(&[("15", "future-mode", bdf)]);
        assert!(
            !iommu_at(root.path(), bdf)
                .unwrap()
                .passes_addresses_through()
        );
    }

    /// F2 has no IOMMU.
    #[test]
    fn no_iommu_at_all_is_not_an_error() {
        // No tree at all.
        let empty = tempfile::tempdir().unwrap();
        let gone = empty.path().join("nothing-here");
        assert_eq!(iommu_at(&gone, "0000:04:00.0").unwrap(), Iommu::Absent);

        // The tree exists, but the device is in no group.
        let root = fake_groups(&[("7", "DMA", "0000:03:00.0")]);
        assert_eq!(
            iommu_at(root.path(), "0000:04:00.0").unwrap(),
            Iommu::Absent
        );
        assert!(
            iommu_at(root.path(), "0000:04:00.0")
                .unwrap()
                .passes_addresses_through()
        );
    }

    /// "Turn off the IOMMU" does not say which group to change.
    #[test]
    fn the_refusal_names_the_group_to_switch() {
        let err = Error::Translating {
            bdf: "0000:04:00.0".into(),
            group: "15".into(),
            kind: "DMA-FQ".into(),
        }
        .to_string();
        assert!(err.contains("0000:04:00.0"), "{err}");
        assert!(err.contains("/sys/kernel/iommu_groups/15/type"), "{err}");
        assert!(err.contains("identity"), "{err}");
        // It says no reboot is needed.
        assert!(err.contains("No reboot"), "{err}");
        // It says the protection goes away.
        assert!(err.contains("protection"), "{err}");
    }

    /// A fake `/sys/bus/pci/devices/<bdf>/config`. Only the first 64 bytes,
    /// which is what a non-root user can read.
    fn fake_config(bdf: &str, command: u16) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(bdf)).unwrap();
        let mut raw = vec![0u8; 64];
        raw[4..6].copy_from_slice(&command.to_le_bytes());
        std::fs::write(root.path().join(bdf).join("config"), raw).unwrap();
        root
    }

    /// A transfer without bus mastering gets no answer, and the DMA engine
    /// stays busy.
    #[test]
    fn a_card_without_bus_mastering_is_told_apart() {
        let bdf = "0000:04:00.0";

        // A test host after a reboot (`lspci`: `Mem+ BusMaster-`).
        let root = fake_config(bdf, 0x0002);
        assert!(!bus_master_at(root.path(), bdf).unwrap());

        // After `setpci -s <bdf> COMMAND=0x4:0x4`.
        let root = fake_config(bdf, 0x0006);
        assert!(bus_master_at(root.path(), bdf).unwrap());

        // The message has the fix with this bdf, and what happens without it.
        let err = Error::NoBusMaster { bdf: bdf.into() }.to_string();
        assert!(
            err.contains("setpci -s 0000:04:00.0 COMMAND=0x4:0x4"),
            "{err}"
        );
        assert!(err.contains("stay busy"), "{err}");
    }

    /// `/proc/meminfo` as read on a test host.
    #[test]
    fn the_hugepage_pool_is_read_from_meminfo() {
        let meminfo = "\
MemTotal:       65331296 kB
AnonHugePages:     86016 kB
HugePages_Total:      64
HugePages_Free:       60
HugePages_Rsvd:        0
Hugepagesize:       2048 kB
";
        let got = hugepages_from(meminfo).unwrap();
        assert_eq!(got.total, 64);
        assert_eq!(got.free, 60);
        assert_eq!(got.size_bytes, 2 * 1024 * 1024);

        // Not confused by the THP line `AnonHugePages`, which comes first.
        assert_ne!(got.size_bytes, 86016 * 1024);
    }

    /// A machine with no reservation (the default).
    #[test]
    fn an_unreserved_pool_says_how_to_reserve() {
        let meminfo =
            "HugePages_Total:       0\nHugePages_Free:        0\nHugepagesize:       2048 kB\n";
        let got = hugepages_from(meminfo).unwrap();
        assert_eq!(got.free, 0);

        let err = Error::NoHugepages {
            want: 1,
            free: 0,
            size_bytes: got.size_bytes,
        }
        .to_string();
        assert!(err.contains("sysctl -w vm.nr_hugepages=1"), "{err}");
        // It says THP cannot replace them.
        assert!(err.contains("transparent"), "{err}");
        // It says to read the reservation back; it can partly fail.
        assert!(err.contains("HugePages_Total"), "{err}");
    }

    /// The AWS code (`fpga_dma_mem.c`) does not check this. Without root the
    /// PFN is 0, so physical address 0 comes out with no error.
    #[test]
    fn a_pagemap_entry_without_present_is_refused() {
        // What a non-root read gives: all 0.
        let err = physical_from(0, 0, 0x7f00_0000).unwrap_err();
        assert!(matches!(err, Error::NotPresent { .. }), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("sudo"), "{text}");
        assert!(text.contains("physical address 0"), "{text}");

        // Present, but PFN 0: refused too.
        let err = physical_from(PAGEMAP_PRESENT, 0, 0).unwrap_err();
        assert!(matches!(err, Error::NotPresent { .. }), "{err:?}");
    }

    #[test]
    fn a_present_entry_becomes_a_physical_address() {
        let pfn = 0x12_3456u64;
        let entry = PAGEMAP_PRESENT | pfn;
        assert_eq!(physical_from(entry, 0, 0).unwrap(), pfn * 4096);
        // The offset in the page is kept.
        assert_eq!(physical_from(entry, 0x123, 0).unwrap(), pfn * 4096 + 0x123);

        // High flags (soft-dirty and others) do not leak into the PFN.
        let noisy = entry | (1 << 62) | (1 << 55);
        assert_eq!(physical_from(noisy, 0, 0).unwrap(), pfn * 4096);
    }

    /// Needs root and reserved huge pages, so it is ignored by default
    /// (`cargo test -- --ignored`).
    #[test]
    #[ignore]
    fn a_huge_page_comes_back_with_a_usable_address() {
        let pages = hugepages().unwrap();
        assert!(pages.free > 0, "reserve huge pages first: {pages:?}");

        let mut buf = map_huge(pages.size_bytes as usize).unwrap();
        assert_eq!(buf.len(), pages.size_bytes as usize);
        // Not 0, and aligned to the huge page size.
        assert_ne!(buf.bus_addr(), 0);
        assert_eq!(buf.bus_addr() % pages.size_bytes, 0);

        // Write and read back (host side only).
        buf.as_mut_slice()[0] = 0xa5;
        buf.as_mut_slice()[pages.size_bytes as usize - 1] = 0x5a;
        assert_eq!(buf.as_slice()[0], 0xa5);
        assert_eq!(buf.as_slice()[pages.size_bytes as usize - 1], 0x5a);
    }
}

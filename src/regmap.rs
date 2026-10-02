//! The register map.
//!
//! The generator emits data, not code. It does not generate accessors per DUT.
//! It writes a machine-readable map, and a generic client reads it at run time
//! (the same approach as `csr.csv` + `RemoteClient` in LiteX). The RTL and the
//! host API come from the same IR, so CSR offsets cannot drift apart.
//!
//! ## Allocation
//!
//! - One port uses whole 32-bit words. Ports are placed in declaration order on
//!   4-byte boundaries, and a port wider than 32 bits takes several consecutive
//!   words. Ports are not bit-packed: at this size, a readable map diff and a
//!   dump you can follow by eye are worth more than the space.
//! - Word `i` holds bits `[32i+31 : 32i]` (from the low end).
//! - Input ports are `rw`, output ports are `ro`. The direction comes from the
//!   DUT declaration (symbol table), never from the name.
//!
//! ## The identity header at the end of the window
//!
//! Two words, `magic` and `map_hash`, sit at the end of the window, so that the
//! user area starts at address 0. With them, the host checks at run time that
//! the bitstream on the board and the local `regs.json` match. An offset
//! mismatch also happens when an old `regs.json` is used with a new bitstream;
//! the hash, built into the RTL as a constant, makes that detectable.

use std::collections::BTreeSet;

use miette::Diagnostic;
use thiserror::Error;

use crate::bundle::Binding;
use crate::dut::{Dut, PortDirection};
use crate::manifest::{Backing, Manifest};

/// The format version of the map. Raise it only for a breaking change.
///
/// - 1: first version
/// - 2: adds the `marker` of generated files
/// - 3: adds `self_clearing`
/// - 4: adds terminator registers (`kind = "terminator"`), `role`, and
///   `self_clearing.from`
/// - 5: adds `target`, so the host can omit `--target`
/// - 6: adds contiguous ranges in the window (`regions`)
///
/// Later fields (`pcie`, `target_source`, `window_clock_mhz` / `window_cycles`)
/// did not raise it. Readers ignore unknown fields and use defaults for missing
/// ones, so old and new readers both keep working.
pub const FORMAT_VERSION: u32 = 6;

pub const WORD_BITS: usize = 32;

/// The constant in the identity header: ASCII `"VHRN"`. The host uses it to
/// check that it talks to a veryl-harness register window.
pub const MAGIC: u32 = 0x5648_524e;

pub const MAGIC_NAME: &str = "harness_magic";
pub const MAP_HASH_NAME: &str = "harness_map_hash";

/// The number of transactions the window ended by timeout.
///
/// It is the only way for PCIe to see that nothing answered, so it is always
/// present, like the identity header. The borrowed CQ/CC bridge does not turn
/// `rresp` into an error completion, so `SLVERR` never reaches the host.
pub const TIMEOUT_NAME: &str = "harness_timeout";

/// The maximum number of window clock cycles that the JTAG bridge (`hns::dr`)
/// needs for one command.
///
/// From the request toggle at Update to the done toggle: TCK phase (0 to 1),
/// 2 synchronizer stages, 1 stage to issue AR / AW, and 2 stages downstream
/// (`hns::axil`, and the CSR, regions and `slave`, which always answer in 1
/// cycle). That is 6 for a read and 5 for a write. Between Update and the next
/// Capture, the host waits so that one TCK period covers this (`window_cycles`
/// in `regs.json`, `update_gap` in `hns-host`). The 2 cycles to synchronize
/// done back to TCK are needed on top of that.
///
/// Measured on an Arty board with a 50 MHz window: with a TCK period of 5
/// cycles (10 MHz), 20 of 20 runs lost data; with 6.7 cycles (7.5 MHz), 20 of 20
/// passed. Count again when a responder that does not always answer in 1 cycle
/// is added, for example `slave` with `latency` of 2 or more. `dram` and AXI4
/// memories have variable latency, so they are not counted here; the host
/// resends when it sees `busy`.
pub const WINDOW_CYCLES: u32 = 6;

/// The registers for the card to write host memory. Only present on PCIe with
/// `dram`. About 200 times faster than reading through the window.
pub const DMA_PREFIX: &str = "dma";

/// The role that fires one descriptor. A write becomes a pulse and the value
/// is not stored: `hns::dma_gate` expects a 1-cycle pulse, so the CSR has no
/// holding register for this one.
pub const DMA_GO: &str = "dma_go";

/// Holds the DUT in reset while it is 1. The harness itself is not reset.
pub const DUT_RESET: &str = "dut_reset";

/// 1 means the DUT is in reset. After writing `dut_reset`, poll this until it
/// changes.
pub const DUT_RESET_STATE: &str = "dut_reset_state";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct RegisterMap {
    pub dut: String,
    pub registers: Vec<Register>,

    /// Contiguous ranges in the window. They sit below the registers (from
    /// address 0).
    pub regions: Vec<Region>,

    /// The hash of the whole map. The RTL has the same value as a constant; the
    /// host reads it and compares it with its local map.
    pub map_hash: u32,

    /// The bundle that the card DMA engine (`dma_*`) reaches, if there is one.
    ///
    /// The host needs it to decide whether a memory may be moved by the DMA
    /// engine. The engine is wired to one `dram` only, so using it for another
    /// bundle would access a different memory. The `dma_*` registers have no
    /// `bundle`, so this cannot be found from them. It is not part of the hash:
    /// the register layout does not change, so the check against the bitstream
    /// stays valid.
    pub requester: Option<String>,
}

impl RegisterMap {
    /// The total number of bytes (the window size).
    pub fn size_bytes(&self) -> usize {
        self.registers
            .last()
            .map(|register| register.offset + register.words * (WORD_BITS / 8))
            .unwrap_or(0)
    }
}

/// What is behind a region. It tells whether accessing the same address twice
/// gives the same result.
///
/// When a batch is lost, the host may resend it for `Memory`, because running it
/// twice does not change the result. For `Dut` this is unknown, so it does not
/// resend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Storage held by the harness (`bram` / `dram`). Access is idempotent.
    Memory,
    /// An addressable slave interface of the DUT. Even a read can change the
    /// DUT state.
    Dut,
}

impl RegionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RegionKind::Memory => "memory",
            RegionKind::Dut => "dut",
        }
    }
}

/// A contiguous range in the window. Addresses count from 0 inside it; the host
/// adds `base`.
///
/// Unlike an indirect port, it has no shared state, so two window masters (JTAG
/// and BAR) cannot break each other.
#[derive(Debug, PartialEq, Eq)]
pub struct Region {
    pub bundle: String,

    pub kind: RegionKind,

    /// The offset from the start of the window. It is aligned to the region
    /// size, so the RTL decodes it with a mask instead of a comparison.
    pub base: usize,

    /// The number of bytes it takes. A power of two.
    pub size_bytes: usize,

    pub entry_bytes: usize,

    pub depth: u64,

    /// The bytes of the whole memory. For a moving window (aperture) it is
    /// larger than `size_bytes`. When equal, the window does not move.
    pub total_bytes: u64,
}

impl Region {
    /// Whether this is a moving window.
    pub fn is_aperture(&self) -> bool {
        self.total_bytes > self.size_bytes as u64
    }
}

/// The window masters. Each master has its own base register.
///
/// There are always this many, for any transport: on a target without PCIe the
/// second one is simply unused. The map does not depend on the transport. The
/// only exception is `dma_*`, which is present only on PCIe with `dram`; JTAG
/// has no place for the card to write back to, so the registers would look
/// usable but do nothing.
pub const WINDOW_MASTERS: &[&str] = &["jtag", "pcie"];

#[derive(Debug, PartialEq, Eq)]
pub struct Register {
    pub name: String,
    pub kind: Kind,

    /// The byte offset of the first word.
    pub offset: usize,

    /// The number of words. Word `i` holds bits `[32i+31 : 32i]`.
    pub words: usize,

    /// The original bit width. It may be less than `words * 32`; the rest sits
    /// in the low bits of the last word.
    pub width: usize,

    pub access: Access,

    /// The bundle it comes from. `None` for the header.
    pub bundle: Option<String>,

    /// Set only for fixed values, such as the header.
    pub value: Option<u32>,

    /// Clears the register so that one write gives exactly one beat.
    ///
    /// In a `valid_ready` bundle, it is on the register the host drives (valid).
    /// The register returns to 0 when the other side (ready) completes one beat.
    /// JTAG works in milliseconds and the DUT in nanoseconds, so without it a
    /// single write of 1 would send hundreds of thousands of beats.
    pub self_clearing: Option<ClearOn>,

    /// The purpose of a terminator register (`data` / `level` / `drops` /
    /// `pop`). Set only for `Kind::Terminator`.
    pub role: Option<&'static str>,
}

/// When a self-clearing register clears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearOn {
    /// The signal that marks one completed beat.
    pub source: ClearSource,
    /// Whether that signal is declared inverted (`ready = "!o_full"`).
    pub invert: bool,
    /// The role of the other side (for display and the map).
    pub role: &'static str,
}

/// Where the other side of a self-clearing register is.
///
/// It is not always a DUT port. The `pop` of `host_poll_fifo` clears on the
/// level held by the terminator: the other side is a FIFO the harness added.
/// With a single string here, the host could not tell where to look to see the
/// beat consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearSource {
    /// A DUT port (`ready`).
    DutPort(String),
    /// A terminator register (`<bundle>_level`).
    Terminator(String),
}

impl ClearOn {
    /// The name of the other side (a DUT port name or a terminator register
    /// name).
    pub fn on(&self) -> &str {
        match &self.source {
            ClearSource::DutPort(name) | ClearSource::Terminator(name) => name,
        }
    }

    /// Where it comes from, as written to the map.
    pub fn from(&self) -> &'static str {
        match self.source {
            ClearSource::DutPort(_) => "dut_port",
            ClearSource::Terminator(_) => "terminator",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Placed by the generator (magic / hash).
    Header,
    Port,
    /// Status and control from a terminator (FIFO level, drop counter, pop,
    /// and so on). Not a DUT port: a terminator that can lose data must expose
    /// what it lost, so the map holds more than DUT ports.
    Terminator,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Header => "header",
            Kind::Port => "port",
            Kind::Terminator => "terminator",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The host writes it (a DUT input) and can read it back.
    ReadWrite,
    /// The host only reads it (a DUT output).
    ReadOnly,
}

impl Access {
    pub fn as_str(&self) -> &'static str {
        match self {
            Access::ReadWrite => "rw",
            Access::ReadOnly => "ro",
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, Diagnostic)]
pub enum RegMapError {
    /// A read past the BAR returns the bus value for an empty address, with no
    /// error.
    #[error("[pcie] bar_bytes = {bar} is smaller than the {window}-byte window")]
    #[diagnostic(
        code(harness::regmap::bar_too_small),
        help(
            "The host cannot reach a register past the end of the BAR. Make it at least:\n\n    [pcie]\n    bar_bytes = {suggestion}"
        )
    )]
    BarTooSmall {
        bar: u32,
        window: usize,
        suggestion: u32,
    },

    // No article before `{direction}`: it can be `modport`, `interface`, `inout`
    // or `import`, so a fixed "an" would give "an modport".
    #[error("[bundle.{bundle}] port `{port}` has direction `{direction}`, which reg cannot map")]
    #[diagnostic(
        code(harness::regmap::unsupported_direction),
        help(
            "A reg register is written by the host (a DUT input) or read by it (a DUT output), and a `{direction}` port is neither. Split it, or use a different backing for this bundle."
        )
    )]
    UnsupportedDirection {
        bundle: String,
        port: String,
        direction: &'static str,
    },

    #[error("port `{port}` has a width the analyzer cannot resolve")]
    #[diagnostic(
        code(harness::regmap::unresolved_width),
        help(
            "A register needs a width. Widths in Harness.toml are not supported yet, so use a port whose width the analyzer can resolve."
        )
    )]
    UnresolvedWidth { port: String },

    /// The first two let the host check the bitstream. The third shows that the
    /// window answered in place of a terminator.
    #[error("port `{port}` collides with the register the harness itself places")]
    #[diagnostic(
        code(harness::regmap::reserved_name),
        help(
            "The harness puts `{MAGIC_NAME}`, `{MAP_HASH_NAME}` and `{TIMEOUT_NAME}` at the end of the window, and also places `dut_reset`, `dut_reset_state`, `dma_*` and `<bundle>_base_*`. Rename the port in the wrapper you point [dut] at."
        )
    )]
    ReservedName { port: String },

    #[error("two registers would be named `{name}`")]
    #[diagnostic(
        code(harness::regmap::duplicate_name),
        help(
            "The host finds registers by name, so names must be unique. A host_poll_fifo adds `<bundle>_data`, `_level`, `_depth`, `_drops` and `_pop`. Rename the bundle, or rename the port in the wrapper you point [dut] at."
        )
    )]
    DuplicateName { name: String },

    /// The RTL decodes a region by its upper address bits only.
    #[error(
        "[bundle.{bundle}] has base = {base:#x}, which is not a multiple of its {size:#x}-byte size"
    )]
    #[diagnostic(
        code(harness::regmap::base_not_aligned),
        help(
            "A region must start at a multiple of its size, because the window finds it by its upper address bits. The nearest bases that work are {below:#x} and {above:#x}."
        )
    )]
    BaseNotAligned {
        bundle: String,
        base: usize,
        size: usize,
        below: usize,
        above: usize,
    },

    #[error(
        "[bundle.{bundle}] at base = {base:#x} overlaps `{other}` ({other_base:#x}..{other_end:#x})"
    )]
    #[diagnostic(
        code(harness::regmap::base_overlaps),
        help(
            "Two regions cannot share an address. Move one of them: `{bundle}` takes {size:#x} bytes from its base."
        )
    )]
    BaseOverlaps {
        bundle: String,
        base: usize,
        size: usize,
        other: String,
        other_base: usize,
        other_end: usize,
    },

    #[error("[bundle.{bundle}] has `base`, but it has no region in the window")]
    #[diagnostic(
        code(harness::regmap::base_without_region),
        help(
            "`base` places a region: `bram` or `bram_preload` with access = \"region\", `dram`, or `slave`. Remove `base` from this bundle."
        )
    )]
    BaseWithoutRegion { bundle: String },
}

// ---------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------

/// Builds the register map from the `reg` bundles.
///
/// The order is by bundle name, then by port declaration order. `bundle` is a
/// `BTreeMap`, so the first is deterministic, and `Binding.ports` is already in
/// declaration order.
///
/// The five terminator plans are passed one by one. They cannot be grouped as a
/// `Plan`, because `Plan` holds the map this function returns.
#[allow(clippy::too_many_arguments)]
pub fn build(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
    contracts: &[crate::contract::BundleContract],
    fifos: &[crate::terminator::FifoPlan],
    memories: &[crate::terminator::MemPlan],
    host_mems: &[crate::terminator::HostMemPlan],
    slaves: &[crate::terminator::SlavePlan],
    axi_mems: &[crate::terminator::AxiMemPlan],
    // With PCIe: the IDs and the BAR (`plan::pcie_identity`, from the resolved
    // transport). The `dma` registers exist only on PCIe, because JTAG has no
    // place for the card to write back to. It also checks that the window fits
    // in the BAR.
    pcie: Option<&crate::manifest::Pcie>,
) -> Result<RegisterMap, RegMapError> {
    let mut registers = Vec::new();

    // Regions come first, so that user addresses start at 0. The registers and
    // the identity header sit above them. `place_regions` gives the bases;
    // `--info` shows the gaps that the alignment leaves.
    let mut wanted = Vec::new();
    for mem in memories
        .iter()
        .filter(|mem| mem.access == crate::manifest::MemAccess::Region)
    {
        let entry_bytes = mem.entry_width.div_ceil(WORD_BITS).max(1) * (WORD_BITS / 8);
        wanted.push(Wanted::new(
            &mem.bundle,
            RegionKind::Memory,
            entry_bytes,
            mem.depth,
            mem.aperture_bytes,
        ));
    }
    // The host side door of `dram` is a region too. It shows the storage behind
    // AXI4 to the host, at the same addresses the DUT uses.
    for dram in axi_mems {
        // One entry in the window is one word. For a wider AXI bus,
        // `hns::axi_host` does the split.
        wanted.push(Wanted::new(
            &dram.bundle,
            RegionKind::Memory,
            WORD_BITS / 8,
            dram.region_words(),
            dram.aperture_bytes,
        ));
    }
    // A DUT slave interface is a region too. The only difference is whether
    // `hns::mem` or the DUT is behind it, so decoding and allocation are shared.
    for slave in slaves {
        wanted.push(Wanted::new(
            &slave.bundle,
            RegionKind::Dut,
            WORD_BITS / 8,
            1u64 << slave.addr_width,
            None,
        ));
    }
    let regions = place_regions(manifest, wanted)?;
    let mut offset = regions
        .iter()
        .map(|region| region.base + region.size_bytes)
        .max()
        .unwrap_or(0);

    // The base of a moving window: one per master. With a shared base, one
    // master could move it just before the other reads, and the other would
    // read a different address.
    for region in &regions {
        if !region.is_aperture() {
            continue;
        }
        // The number of pages. The base is a page number.
        let pages = region.total_bytes / region.size_bytes as u64;
        let width = (pages.trailing_zeros() as usize).max(1);
        for master in WINDOW_MASTERS {
            registers.push(Register {
                name: format!("{}_base_{master}", region.bundle),
                kind: Kind::Terminator,
                offset,
                words: 1,
                width,
                access: Access::ReadWrite,
                bundle: Some(region.bundle.clone()),
                value: None,
                self_clearing: None,
                // One role name per master. The generated wire is named
                // `t_<bundle>_<role>`, so a shared name would collide.
                role: Some(if *master == "jtag" {
                    "base_jtag"
                } else {
                    "base_pcie"
                }),
            });
            offset += WORD_BITS / 8;
        }
    }

    // The header goes at the end. Its position is known only after everything
    // else is placed, so it is added below. Its names are reserved now.
    let reserved: BTreeSet<&str> = [MAGIC_NAME, MAP_HASH_NAME, TIMEOUT_NAME]
        .into_iter()
        .collect();

    for binding in bindings {
        if manifest.bundle[&binding.bundle].backing != Backing::Reg {
            continue;
        }
        for name in &binding.ports {
            if reserved.contains(name.as_str()) {
                return Err(RegMapError::ReservedName { port: name.clone() });
            }

            let port = dut
                .ports
                .iter()
                .find(|port| &port.name == name)
                .expect("bundle ports come from the DUT");

            let access = match port.direction {
                PortDirection::Input => Access::ReadWrite,
                PortDirection::Output => Access::ReadOnly,
                other => {
                    return Err(RegMapError::UnsupportedDirection {
                        bundle: binding.bundle.clone(),
                        port: name.clone(),
                        direction: other.as_str(),
                    });
                }
            };

            let width = port
                .width()
                .ok_or_else(|| RegMapError::UnresolvedWidth { port: name.clone() })?;
            let words = width.div_ceil(WORD_BITS).max(1);

            registers.push(Register {
                name: name.clone(),
                kind: Kind::Port,
                offset,
                words,
                width,
                access,
                bundle: Some(binding.bundle.clone()),
                value: None,
                self_clearing: clear_on(dut, contracts, &binding.bundle, name),
                role: None,
            });
            offset += words * (WORD_BITS / 8);
        }
    }

    // Terminator registers, in bundle name order: `fifos` is built in the order
    // of `bindings`.
    for fifo in fifos {
        let bundle = fifo.bundle.clone();
        let mut place = |name: String, width: usize, access: Access, role, value, clear| {
            let words = width.div_ceil(WORD_BITS).max(1);
            let register = Register {
                name,
                kind: Kind::Terminator,
                offset,
                words,
                width,
                access,
                bundle: Some(bundle.clone()),
                value,
                self_clearing: clear,
                role: Some(role),
            };
            offset += words * (WORD_BITS / 8);
            register
        };

        registers.push(place(
            format!("{}_data", fifo.bundle),
            fifo.width,
            Access::ReadOnly,
            "data",
            None,
            None,
        ));
        registers.push(place(
            format!("{}_level", fifo.bundle),
            fifo.level_width(),
            Access::ReadOnly,
            "level",
            None,
            None,
        ));
        // The depth is readable on the board. If the host could not see that
        // the default was used, a missing `depth` and a FIFO that is too
        // shallow would go unnoticed.
        registers.push(place(
            format!("{}_depth", fifo.bundle),
            WORD_BITS,
            Access::ReadOnly,
            "depth",
            Some(fifo.depth),
            None,
        ));
        if fifo.counts_drops() {
            registers.push(place(
                format!("{}_drops", fifo.bundle),
                WORD_BITS,
                Access::ReadOnly,
                "drops",
                None,
                None,
            ));
        }
        // One write of pop takes exactly one entry. While the FIFO is empty,
        // pop stays high; it clears after it takes the next entry that arrives.
        registers.push(place(
            format!("{}_pop", fifo.bundle),
            1,
            Access::ReadWrite,
            "pop",
            None,
            Some(ClearOn {
                source: ClearSource::Terminator(format!("{}_level", fifo.bundle)),
                invert: false,
                role: "level",
            }),
        ));
    }

    // The interrupt line as the DUT drives it, so it can be watched without
    // the host's interrupt handler, and over JTAG too.
    for binding in bindings {
        if manifest.bundle[&binding.bundle].backing != Backing::HostIrq {
            continue;
        }
        registers.push(Register {
            name: format!("{}_level", binding.bundle),
            kind: Kind::Terminator,
            offset,
            words: 1,
            width: 1,
            access: Access::ReadOnly,
            bundle: Some(binding.bundle.clone()),
            value: None,
            self_clearing: None,
            role: Some("irq_level"),
        });
        offset += WORD_BITS / 8;
    }

    // The indirect memory port: only address and data, so the window stays small.
    for mem in memories {
        let bundle = mem.bundle.clone();
        let mut place = |name: String, width: usize, access: Access, role, value| {
            let words = width.div_ceil(WORD_BITS).max(1);
            let register = Register {
                name,
                kind: Kind::Terminator,
                offset,
                words,
                width,
                access,
                bundle: Some(bundle.clone()),
                value,
                self_clearing: None,
                role: Some(role),
            };
            offset += words * (WORD_BITS / 8);
            register
        };

        // A region has no indirect port. The address is part of each access,
        // so `maddr` is not needed, and it would only add shared state.
        // `depth` and `oor` are status, so both forms have them.
        let indirect = mem.access != crate::manifest::MemAccess::Region;

        // A data write increments the address, so a contiguous write needs one
        // transaction per word (one JTAG transaction takes milliseconds). A read
        // does not increment: if reading changed state, `dump` would walk the
        // memory.
        if indirect {
            registers.push(place(
                format!("{}_maddr", mem.bundle),
                mem.host_addr_width(),
                Access::ReadWrite,
                "maddr",
                None,
            ));
            // One entry wide. For a line-wide write, one commit writes the line.
            registers.push(place(
                format!("{}_mdata", mem.bundle),
                mem.entry_width,
                Access::ReadWrite,
                "mdata",
                None,
            ));
        }
        registers.push(place(
            format!("{}_depth", mem.bundle),
            WORD_BITS,
            Access::ReadOnly,
            "depth",
            Some(u32::try_from(mem.depth).unwrap_or(u32::MAX)),
        ));
        // Only when out-of-range access is possible. It is counted, not wrapped.
        if mem.counts_out_of_range() {
            registers.push(place(
                format!("{}_oor", mem.bundle),
                WORD_BITS,
                Access::ReadOnly,
                "oor",
                None,
            ));
        }
    }

    // `dram` is a region, so it has no indirect port. Its size depends on
    // whether it is a stand-in, so it is readable on the board.
    for dram in axi_mems {
        // Whether calibration is done. A real controller takes tens of ms to
        // calibrate and accepts no access meanwhile. Without this, a stall has
        // no visible reason, and people look for the cause in the wrong place.
        if dram.backing == Backing::Dram {
            registers.push(Register {
                name: format!("{}_calib", dram.bundle),
                kind: Kind::Terminator,
                offset,
                words: 1,
                width: 1,
                access: Access::ReadOnly,
                bundle: Some(dram.bundle.clone()),
                value: None,
                self_clearing: None,
                role: Some("calib"),
            });
            offset += WORD_BITS / 8;
        }
        registers.push(Register {
            name: format!("{}_depth", dram.bundle),
            kind: Kind::Terminator,
            offset,
            words: 1,
            width: WORD_BITS,
            access: Access::ReadOnly,
            bundle: Some(dram.bundle.clone()),
            value: Some(dram.depth),
            self_clearing: None,
            role: Some("depth"),
        });
        offset += WORD_BITS / 8;
    }

    // DUT reset, in every design. The host writes 1 and polls until `state` is
    // 1, then writes 0 and polls until it is 0. No timing on the host side is
    // assumed.
    for (name, access) in [
        (DUT_RESET, Access::ReadWrite),
        (DUT_RESET_STATE, Access::ReadOnly),
    ] {
        registers.push(Register {
            name: name.to_string(),
            kind: Kind::Terminator,
            offset,
            words: 1,
            width: 1,
            access,
            bundle: None,
            value: None,
            self_clearing: None,
            role: Some(name),
        });
        offset += WORD_BITS / 8;
    }

    // The registers for the card to write host memory: only on PCIe with
    // `dram`. The DMA engine master is fixed at 256 bits and cannot connect
    // behind a `bram` (DUT bus width). Registers that lead nowhere are not
    // placed: the host would use them, and nothing would happen.
    let requester = axi_mems
        .iter()
        .find(|m| m.backing == crate::manifest::Backing::Dram)
        .filter(|_| pcie.is_some())
        .map(|m| m.bundle.clone());
    if requester.is_some() {
        let mut place = |name: &str, width: usize, access: Access, role: &'static str| {
            let words = width.div_ceil(WORD_BITS).max(1);
            registers.push(Register {
                name: format!("{DMA_PREFIX}_{name}"),
                kind: Kind::Terminator,
                offset,
                words,
                width,
                access,
                bundle: None,
                value: None,
                self_clearing: None,
                role: Some(role),
            });
            offset += words * (WORD_BITS / 8);
        };
        // The allowed window. The host writes it once, with one page from
        // pagemap. `hns::dma_gate` does not issue a descriptor outside it: on a
        // machine with the IOMMU in pass-through, nothing else would stop it.
        place("base", 64, Access::ReadWrite, "dma_base");
        place("size", 32, Access::ReadWrite, "dma_size");
        // One descriptor.
        place("pcie_addr", 64, Access::ReadWrite, "dma_pcie_addr");
        place("axi_addr", 64, Access::ReadWrite, "dma_axi_addr");
        place("len", 20, Access::ReadWrite, "dma_len");
        // The direction. 0: the card writes host memory (a read for the host).
        // 1: the card reads it (a write for the host). The window check is the
        // same for both.
        place("dir", 1, Access::ReadWrite, "dma_dir");
        // A write becomes a pulse. No value is kept.
        place("go", 1, Access::ReadWrite, DMA_GO);
        // Separate "not issued" from "issued and failed".
        place("busy", 1, Access::ReadOnly, "dma_busy");
        place("error", 4, Access::ReadOnly, "dma_error");
        place("done", 16, Access::ReadOnly, "dma_done");
        place("oor", 16, Access::ReadOnly, "dma_oor");
        place("blocked", 16, Access::ReadOnly, "dma_blocked");
        // The max payload size the link negotiated (encoded; 0 is 128 bytes).
        // It sets the speed, so the host can see it.
        place("mps", 3, Access::ReadOnly, "dma_mps");
        // A limit for measurements. 0 keeps the negotiated value. The RTL takes
        // the minimum, so no value here can exceed what the link allows.
        place("mps_limit", 3, Access::ReadWrite, "dma_mps_limit");
        // The same pair for reads. The max read request size is negotiated
        // separately from the payload size.
        place("mrrs", 3, Access::ReadOnly, "dma_mrrs");
        place("mrrs_limit", 3, Access::ReadWrite, "dma_mrrs_limit");
        // Counters for what vanished silently. All saturate; the host compares
        // them before and after a transfer.
        // RQ TLPs dropped by the hard block (PG213 local error 10100b). If
        // `tvalid` falls in the middle of a TLP, it is nullified and nothing
        // reaches the host.
        //
        // On a VCU118 (x8 configuration on an x4 link) this counter does not
        // count. PG213 also says it may not work for some link widths and
        // speeds. It stays, because it may work on other boards.
        place("rq_drops", 16, Access::ReadOnly, "dma_rq_drops");
        // TLPs where `tvalid` fell in the middle (at the input of
        // `hns::tlp_hold`). They are buffered before they go on, so nothing is
        // lost; this shows that the buffer stage works. On a VCU118 almost every
        // TLP has gaps, because it crosses from 100 MHz to 250 MHz.
        place("rq_gaps", 16, Access::ReadOnly, "dma_rq_gaps");
        // Completions dropped by the borrowed read side. `cor`: unused tag,
        // poisoned, or bad state. `uncor`: format mismatch or timeout.
        place("rc_cor", 16, Access::ReadOnly, "dma_rc_cor");
        place("rc_uncor", 16, Access::ReadOnly, "dma_rc_uncor");
    }

    // The `host_mem` stand-in. It has the same indirect port as a memory, so
    // the host accesses it like a `bram`, and `hio load` works unchanged.
    for hmem in host_mems {
        let bundle = hmem.bundle.clone();
        let mut place = |name: String, width: usize, access: Access, role, value| {
            let words = width.div_ceil(WORD_BITS).max(1);
            let register = Register {
                name,
                kind: Kind::Terminator,
                offset,
                words,
                width,
                access,
                bundle: Some(bundle.clone()),
                value,
                self_clearing: None,
                role: Some(role),
            };
            offset += words * (WORD_BITS / 8);
            register
        };

        registers.push(place(
            format!("{}_maddr", hmem.bundle),
            hmem.host_addr_width(),
            Access::ReadWrite,
            "maddr",
            None,
        ));
        registers.push(place(
            format!("{}_mdata", hmem.bundle),
            hmem.data_width,
            Access::ReadWrite,
            "mdata",
            None,
        ));
        registers.push(place(
            format!("{}_depth", hmem.bundle),
            WORD_BITS,
            Access::ReadOnly,
            "depth",
            Some(hmem.depth),
        ));
        // Requests that could not be served: 0-byte requests and transfers past
        // the end. The DUT sees both only as a short transfer, so this is the
        // only clue.
        registers.push(place(
            format!("{}_oor", hmem.bundle),
            WORD_BITS,
            Access::ReadOnly,
            "oor",
            None,
        ));
        // Two registers that add delay on purpose. The default is 0, which does
        // nothing. They are registers, not manifest keys, so they can be changed
        // without synthesizing again.
        registers.push(place(
            format!("{}_delay", hmem.bundle),
            8,
            Access::ReadWrite,
            "delay",
            None,
        ));
        registers.push(place(
            format!("{}_jitter", hmem.bundle),
            1,
            Access::ReadWrite,
            "jitter",
            None,
        ));
    }

    // The header goes at the end of the window, so the user area starts at 0.
    // The window is rounded up to a power of two, because the range check in
    // `hns::axil` uses `1 << ADDR_BITS`. Without rounding, `size_bytes` would
    // be smaller than the window that is actually decoded.
    let used = registers
        .iter()
        .map(|register| register.offset + register.words * (WORD_BITS / 8))
        .chain(regions.iter().map(|region| region.base + region.size_bytes))
        .max()
        .unwrap_or(0);
    let header_bytes = 3 * (WORD_BITS / 8);
    let window = 1usize << hns_regs::addr_bits(used + header_bytes);

    // The timeout count comes first. `harness_map_hash` must be the last word:
    // the host finds it from the window size.
    registers.push(Register {
        self_clearing: None,
        role: None,
        name: TIMEOUT_NAME.to_string(),
        kind: Kind::Header,
        offset: window - header_bytes,
        words: 1,
        width: WORD_BITS,
        // A read returns the count; a write clears it (w1c). Without a clear,
        // every read after the first timeout would be nonzero, and nobody could
        // tell whether a timeout just happened. Waiting for calibration almost
        // always causes one, so this matters.
        access: Access::ReadWrite,
        bundle: None,
        // Not a constant: the window (`hns::axil`) counts it and drives a wire.
        value: None,
    });
    registers.push(Register {
        self_clearing: None,
        role: None,
        name: MAGIC_NAME.to_string(),
        kind: Kind::Header,
        offset: window - 2 * (WORD_BITS / 8),
        words: 1,
        width: WORD_BITS,
        access: Access::ReadOnly,
        bundle: None,
        value: Some(MAGIC),
    });
    registers.push(Register {
        self_clearing: None,
        role: None,
        name: MAP_HASH_NAME.to_string(),
        kind: Kind::Header,
        offset: window - WORD_BITS / 8,
        words: 1,
        width: WORD_BITS,
        access: Access::ReadOnly,
        bundle: None,
        value: None,
    });

    // A name collision breaks the host API. Terminator register names come from
    // bundle names, so they can collide with DUT port names.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for register in &registers {
        if !seen.insert(register.name.as_str()) {
            return Err(RegMapError::DuplicateName {
                name: register.name.clone(),
            });
        }
    }

    // Whether the window fits in the BAR. The window size is known only now.
    // With a BAR that is too small, the host cannot see the upper part of the
    // window. Without `[pcie]`, PCIe still compares with the default BAR.
    if let Some(pcie) = pcie {
        let size = registers
            .iter()
            .map(|r| r.offset + r.words * (WORD_BITS / 8))
            .max()
            .unwrap_or(0);
        if (pcie.bar_bytes as usize) < size {
            return Err(RegMapError::BarTooSmall {
                bar: pcie.bar_bytes,
                window: size,
                suggestion: (size as u32).next_power_of_two(),
            });
        }
    }

    let map_hash = hash(&registers, &regions);
    if let Some(register) = registers
        .iter_mut()
        .find(|register| register.name == MAP_HASH_NAME)
    {
        register.value = Some(map_hash);
    }

    Ok(RegisterMap {
        dut: dut.name.clone(),
        registers,
        regions,
        map_hash,
        requester,
    })
}

/// A region before it has a base.
struct Wanted {
    bundle: String,
    kind: RegionKind,
    entry_bytes: usize,
    depth: u64,
    total_bytes: u64,
    size_bytes: usize,
}

impl Wanted {
    fn new(
        bundle: &str,
        kind: RegionKind,
        entry_bytes: usize,
        depth: u64,
        aperture: Option<usize>,
    ) -> Wanted {
        let total_bytes = depth * entry_bytes as u64;
        // Only the aperture goes into the window; without one, the whole memory.
        let size_bytes = aperture.unwrap_or((total_bytes as usize).next_power_of_two());
        Wanted {
            bundle: bundle.to_string(),
            kind,
            entry_bytes,
            depth,
            total_bytes,
            size_bytes,
        }
    }

    fn at(self, base: usize) -> Region {
        Region {
            bundle: self.bundle,
            kind: self.kind,
            base,
            size_bytes: self.size_bytes,
            entry_bytes: self.entry_bytes,
            depth: self.depth,
            total_bytes: self.total_bytes,
        }
    }
}

/// Gives each region its base, aligned to its size so the RTL decodes it with
/// a mask. Those with `base` in Harness.toml go exactly there. The others, in
/// the order of `wanted` (memory, dram, slave; by bundle name within a kind),
/// each take the first aligned gap, so adding a bundle without `base` does not
/// move an existing one. The result keeps the order of `wanted`.
fn place_regions(manifest: &Manifest, wanted: Vec<Wanted>) -> Result<Vec<Region>, RegMapError> {
    let pinned = |bundle: &str| {
        manifest
            .bundle
            .get(bundle)
            .and_then(|declared| declared.base)
            .map(|base| base as usize)
    };
    for (name, declared) in &manifest.bundle {
        if declared.base.is_some() && !wanted.iter().any(|w| &w.bundle == name) {
            return Err(RegMapError::BaseWithoutRegion {
                bundle: name.clone(),
            });
        }
    }

    // (start, end, bundle) of what is taken so far.
    let mut taken: Vec<(usize, usize, String)> = Vec::new();
    let mut bases = vec![0; wanted.len()];
    for (i, region) in wanted.iter().enumerate() {
        let Some(base) = pinned(&region.bundle) else {
            continue;
        };
        let size = region.size_bytes;
        if !base.is_multiple_of(size) {
            let below = base / size * size;
            return Err(RegMapError::BaseNotAligned {
                bundle: region.bundle.clone(),
                base,
                size,
                below,
                above: below + size,
            });
        }
        if let Some((other_base, other_end, other)) = taken
            .iter()
            .find(|(start, end, _)| base < *end && *start < base + size)
        {
            return Err(RegMapError::BaseOverlaps {
                bundle: region.bundle.clone(),
                base,
                size,
                other: other.clone(),
                other_base: *other_base,
                other_end: *other_end,
            });
        }
        taken.push((base, base + size, region.bundle.clone()));
        bases[i] = base;
    }
    for (i, region) in wanted.iter().enumerate() {
        if pinned(&region.bundle).is_some() {
            continue;
        }
        let size = region.size_bytes;
        let mut base = 0;
        // Each step jumps past one region in the way, so this ends.
        while let Some((_, end, _)) = taken
            .iter()
            .find(|(start, end, _)| base < *end && *start < base + size)
        {
            base = end.next_multiple_of(size);
        }
        taken.push((base, base + size, region.bundle.clone()));
        bases[i] = base;
    }
    Ok(wanted
        .into_iter()
        .zip(bases)
        .map(|(region, base)| region.at(base))
        .collect())
}

/// Whether this port self-clears (one write = one beat).
///
/// Only when the host drives it (`valid`) and the DUT drives the other side
/// (`ready`). In the other direction (the host drives ready), a level is
/// correct: it only keeps saying "ready to accept".
fn clear_on(
    dut: &Dut,
    contracts: &[crate::contract::BundleContract],
    bundle: &str,
    port: &str,
) -> Option<ClearOn> {
    use crate::manifest::{Contract, Role};

    let resolved = contracts
        .iter()
        .find(|contract| contract.bundle == bundle)?;
    let (request, acknowledge) = match resolved.contract {
        Contract::ValidReady => (Role::Valid, Role::Ready),
        // These contracts have no handshake pair to clear on.
        Contract::FixedLatency | Contract::ValidOnly | Contract::Axi => return None,
    };

    let driven = resolved
        .roles
        .iter()
        .find(|assignment| assignment.role == request)?;
    if driven.port != port {
        return None;
    }
    if direction_of(dut, &driven.port)? != PortDirection::Input {
        return None;
    }

    let other = resolved
        .roles
        .iter()
        .find(|assignment| assignment.role == acknowledge)?;
    if direction_of(dut, &other.port)? != PortDirection::Output {
        return None;
    }

    Some(ClearOn {
        source: ClearSource::DutPort(other.port.clone()),
        invert: other.invert,
        role: acknowledge.as_str(),
    })
}

fn direction_of(dut: &Dut, name: &str) -> Option<PortDirection> {
    dut.ports
        .iter()
        .find(|port| port.name == name)
        .map(|port| port.direction)
}

/// The map hash (FNV-1a, 32 bits). The host must be able to compute it again,
/// so it is chosen to be easy to reimplement, not to be cryptographically strong.
///
/// The input is `name|kind|offset|width|words|access|clear` for each register
/// (`clear` is the other side of self-clearing, or `-`), then
/// `region|bundle|kind|base|size|entry|total` for each region, joined with
/// `\n`. `value` is not included: the hash is itself a value, so it would
/// refer to itself.
///
/// Regions are in it because `base` can move one without moving any register.
pub fn hash(registers: &[Register], regions: &[Region]) -> u32 {
    let canonical = canonical_text(registers, regions);

    let mut hash: u32 = 0x811c_9dc5;
    for byte in canonical.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// The canonical text that is hashed. Its exact form is documented: a hash that
/// clients cannot reimplement is useless for checking.
pub fn canonical_text(registers: &[Register], regions: &[Region]) -> String {
    let regions = regions.iter().map(|region| {
        format!(
            "region|{}|{}|{}|{}|{}|{}",
            region.bundle,
            region.kind.as_str(),
            region.base,
            region.size_bytes,
            region.entry_bytes,
            region.total_bytes,
        )
    });
    registers
        .iter()
        .map(|register| {
            format!(
                "{}|{}|{}|{}|{}|{}|{}",
                register.name,
                register.kind.as_str(),
                register.offset,
                register.width,
                register.words,
                register.access.as_str(),
                match &register.self_clearing {
                    Some(clear) => clear.on(),
                    None => "-",
                },
            )
        })
        .chain(regions)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::How;
    use crate::dut::{Domain, Port, Signal, SignalRole};

    /// Otherwise the upper part of the window is missing for the host, and a read
    /// there gives no error: it only returns whatever the bus returns for an
    /// unclaimed address.
    ///
    /// The check uses the BAR only when the resolved transport is PCIe. Without
    /// `[pcie]` it uses the default BAR. On JTAG, `[pcie]` is ignored.
    #[test]
    fn a_bar_smaller_than_the_window_is_refused() {
        let names: Vec<String> = (0..1100).map(|i| format!("i_csr_{i}")).collect();
        let dut = dut(names
            .iter()
            .map(|n| port(n, PortDirection::Input, 32))
            .collect());
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let bindings = vec![binding("csr", &refs)];
        let map = |manifest: &Manifest, pcie: Option<&crate::manifest::Pcie>| {
            build(
                &dut,
                manifest,
                &bindings,
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                pcie,
            )
        };

        // 1100 words are more than the default 4 KiB BAR.
        let bare = manifest(CSR);
        let err = map(&bare, Some(&crate::manifest::Pcie::default())).unwrap_err();
        let head = err.to_string();
        let help = miette::Diagnostic::help(&err)
            .map(|h| h.to_string())
            .unwrap_or_default();
        assert!(head.contains("smaller than"), "{head}");
        // The help gives the next power of two, not only "make it larger".
        assert!(help.contains("bar_bytes = 8192"), "{help}");

        let big = manifest(&format!("{CSR}\n[pcie]\nbar_bytes = 8192\n"));
        assert!(map(&big, big.pcie.as_ref()).is_ok());

        // JTAG has no BAR.
        let small = manifest(&format!("{CSR}\n[pcie]\nbar_bytes = 4096\n"));
        assert!(map(&small, None).is_ok());
    }

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

    fn binding(bundle: &str, ports: &[&str]) -> Binding {
        Binding {
            bundle: bundle.to_string(),
            ports: ports.iter().map(|port| port.to_string()).collect(),
            how: How::Naming,
        }
    }

    fn manifest(toml: &str) -> Manifest {
        toml::from_str(toml).unwrap()
    }

    const CSR: &str = "[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n";

    #[test]
    fn the_ports_come_first_and_the_header_sits_at_the_end() {
        let dut = dut(vec![
            port("i_csr_addr", PortDirection::Input, 32),
            port("o_csr_rdata", PortDirection::Output, 8),
        ]);
        let map = build(
            &dut,
            &manifest(CSR),
            &[binding("csr", &["i_csr_addr", "o_csr_rdata"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        // User ports start at 0. The header is at the end of the window.
        let names: Vec<&str> = map.registers.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "i_csr_addr",
                "o_csr_rdata",
                DUT_RESET,
                DUT_RESET_STATE,
                TIMEOUT_NAME,
                MAGIC_NAME,
                MAP_HASH_NAME
            ]
        );
        assert_eq!(map.registers[0].offset, 0);
        assert_eq!(map.registers[1].offset, 4);

        // The direction decides access, not the name.
        assert_eq!(map.registers[0].access, Access::ReadWrite);
        assert_eq!(map.registers[1].access, Access::ReadOnly);

        // DUT reset comes right after the ports.
        assert_eq!(map.registers[2].offset, 8);
        assert_eq!(map.registers[3].offset, 12);

        // Ports 8 + DUT reset 8 + header 12 = 28, so the window is 32.
        assert_eq!(map.size_bytes(), 32);
        assert_eq!(map.registers[4].offset, 20);
        // The timeout count is not a constant.
        assert_eq!(map.registers[4].value, None);
        assert_eq!(map.registers[5].offset, 24);
        assert_eq!(map.registers[5].value, Some(MAGIC));
        assert_eq!(map.registers[6].offset, 28);
    }

    #[test]
    fn a_wide_port_takes_consecutive_words() {
        let dut = dut(vec![
            port("i_wide", PortDirection::Input, 64),
            port("i_after", PortDirection::Input, 1),
        ]);
        let map = build(
            &dut,
            &manifest(CSR),
            &[binding("csr", &["i_wide", "i_after"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        let wide = &map.registers[0];
        assert_eq!(wide.words, 2);
        assert_eq!(wide.width, 64);
        assert_eq!(wide.offset, 0);
        assert_eq!(map.registers[1].offset, 8);
    }

    /// Not bit-packed: even 1 bit takes a word.
    #[test]
    fn a_narrow_port_still_takes_a_whole_word() {
        let dut = dut(vec![port("i_en", PortDirection::Input, 1)]);
        let map = build(
            &dut,
            &manifest(CSR),
            &[binding("csr", &["i_en"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        assert_eq!(map.registers[0].words, 1);
        assert_eq!(map.registers[0].width, 1);
    }

    #[test]
    fn only_reg_bundles_become_registers() {
        let dut = dut(vec![
            port("i_csr_addr", PortDirection::Input, 32),
            port("o_dbg", PortDirection::Output, 8),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n\n[bundle.dbg]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"observe\"\n",
        );
        let map = build(
            &dut,
            &manifest,
            &[binding("csr", &["i_csr_addr"]), binding("dbg", &["o_dbg"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        // 1 user port + 2 DUT reset registers + 3 header registers.
        assert_eq!(map.registers.len(), 6);
        assert!(map.registers.iter().all(|x| x.name != "o_dbg"));
    }

    /// The same map gives the same hash, and a different map a different one.
    /// The host check depends on this.
    #[test]
    fn the_hash_follows_the_layout() {
        let a = dut(vec![port("i_csr_addr", PortDirection::Input, 32)]);
        let b = dut(vec![port("i_csr_addr", PortDirection::Input, 64)]);

        let map_a = build(
            &a,
            &manifest(CSR),
            &[binding("csr", &["i_csr_addr"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();
        let map_a2 = build(
            &a,
            &manifest(CSR),
            &[binding("csr", &["i_csr_addr"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();
        let map_b = build(
            &b,
            &manifest(CSR),
            &[binding("csr", &["i_csr_addr"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        assert_eq!(map_a.map_hash, map_a2.map_hash, "same map, same hash");
        assert_ne!(map_a.map_hash, map_b.map_hash, "width changed the layout");

        // The hash is also a register value (built into the RTL), in the last
        // register.
        let last = map_a.registers.last().unwrap();
        assert_eq!(last.name, MAP_HASH_NAME);
        assert_eq!(last.value, Some(map_a.map_hash));
    }

    /// The canonical text must keep its documented form, so clients can
    /// reimplement it.
    #[test]
    fn the_canonical_text_is_the_documented_shape() {
        let dut = dut(vec![port("i_csr_addr", PortDirection::Input, 32)]);
        let map = build(
            &dut,
            &manifest(CSR),
            &[binding("csr", &["i_csr_addr"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        // User ports first, header last. The window covers 4 (port) + 8 (DUT
        // reset) + 12 (header), so it is 32 bytes: magic at 24, hash at 28.
        let text = canonical_text(&map.registers, &map.regions);
        assert!(text.starts_with("i_csr_addr|port|0|32|1|rw|-\n"), "{text}");
        assert!(
            text.ends_with("harness_map_hash|header|28|32|1|ro|-"),
            "{text}"
        );
    }

    #[test]
    fn a_port_that_collides_with_a_header_register_is_rejected() {
        let dut = dut(vec![port(MAGIC_NAME, PortDirection::Input, 32)]);
        let err = build(
            &dut,
            &manifest(CSR),
            &[binding("csr", &[MAGIC_NAME])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap_err();

        assert!(matches!(err, RegMapError::ReservedName { .. }), "{err:?}");
    }

    #[test]
    fn an_inout_port_is_rejected() {
        let dut = dut(vec![port("b_pad", PortDirection::Inout, 1)]);
        let err = build(
            &dut,
            &manifest(CSR),
            &[binding("csr", &["b_pad"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap_err();

        assert!(
            matches!(err, RegMapError::UnsupportedDirection { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn an_unresolved_width_is_rejected() {
        let mut unresolved = port("i_x", PortDirection::Input, 1);
        unresolved.signals[0].width = None;
        let dut = dut(vec![unresolved]);

        let err = build(
            &dut,
            &manifest(CSR),
            &[binding("csr", &["i_x"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, RegMapError::UnresolvedWidth { .. }),
            "{err:?}"
        );
    }

    /// Self-clearing applies only in `valid_ready`, when the host drives valid
    /// and the DUT drives ready.
    #[test]
    fn a_host_driven_valid_clears_itself_on_ready() {
        use crate::contract::{BundleContract, RoleAssignment, RoleSource};
        use crate::manifest::{Contract, Role};

        let dut = dut(vec![
            port("i_push", PortDirection::Input, 1),
            port("o_full", PortDirection::Output, 1),
        ]);
        let contracts = vec![BundleContract {
            bundle: "csr".to_string(),
            contract: Contract::ValidReady,
            declared: true,
            roles: vec![
                RoleAssignment {
                    port: "i_push".to_string(),
                    role: Role::Valid,
                    invert: false,
                    source: RoleSource::Explicit,
                },
                RoleAssignment {
                    port: "o_full".to_string(),
                    role: Role::Ready,
                    invert: true,
                    source: RoleSource::Explicit,
                },
            ],
        }];

        let map = build(
            &dut,
            &manifest(CSR),
            &[binding("csr", &["i_push", "o_full"])],
            &contracts,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        let valid = map.registers.iter().find(|r| r.name == "i_push").unwrap();
        let clear = valid.self_clearing.as_ref().expect("valid must self-clear");
        assert_eq!(clear.on(), "o_full");
        assert_eq!(clear.role, "ready");
        // The inversion of `ready = "!o_full"` is kept.
        assert!(clear.invert);

        // The ready side (a DUT output) does not self-clear.
        let ready = map.registers.iter().find(|r| r.name == "o_full").unwrap();
        assert!(ready.self_clearing.is_none());
    }

    #[test]
    fn a_fixed_latency_bundle_has_no_self_clearing() {
        use crate::contract::BundleContract;
        use crate::manifest::Contract;

        let dut = dut(vec![port("i_csr_addr", PortDirection::Input, 32)]);
        let contracts = vec![BundleContract {
            bundle: "csr".to_string(),
            contract: Contract::FixedLatency,
            declared: true,
            roles: Vec::new(),
        }];

        let map = build(
            &dut,
            &manifest(CSR),
            &[binding("csr", &["i_csr_addr"])],
            &contracts,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(map.registers.iter().all(|r| r.self_clearing.is_none()));
    }

    /// When the meaning of a register changes, an old client must notice.
    #[test]
    fn self_clearing_changes_the_hash() {
        use crate::contract::{BundleContract, RoleAssignment, RoleSource};
        use crate::manifest::{Contract, Role};

        let dut = dut(vec![
            port("i_push", PortDirection::Input, 1),
            port("o_full", PortDirection::Output, 1),
        ]);
        let bindings = [binding("csr", &["i_push", "o_full"])];

        let plain = build(
            &dut,
            &manifest(CSR),
            &bindings,
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();
        let clearing = build(
            &dut,
            &manifest(CSR),
            &bindings,
            &[BundleContract {
                bundle: "csr".to_string(),
                contract: Contract::ValidReady,
                declared: true,
                roles: vec![
                    RoleAssignment {
                        port: "i_push".to_string(),
                        role: Role::Valid,
                        invert: false,
                        source: RoleSource::Explicit,
                    },
                    RoleAssignment {
                        port: "o_full".to_string(),
                        role: Role::Ready,
                        invert: false,
                        source: RoleSource::Explicit,
                    },
                ],
            }],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        assert_ne!(plain.map_hash, clearing.map_hash);
    }

    /// JTAG has no place for the card to write back to, and without a memory
    /// there is nothing to read. Registers placed anyway would look usable, and
    /// using them would do nothing.
    #[test]
    fn the_dma_registers_are_placed_only_for_pcie_with_a_memory() {
        let dut = dut(vec![port("i_csr_addr", PortDirection::Input, 32)]);
        let bindings = [binding("csr", &["i_csr_addr"])];
        let mem = crate::terminator::AxiMemPlan {
            bundle: "mem".to_string(),
            backing: Backing::Dram,
            port: "axi".to_string(),
            pkg: "$std::axi4_pkg::<28, 4, 4, 1, 1, 1, 1, 1>".to_string(),
            addr_width: 28,
            data_bytes: 4,
            id_width: 4,
            depth: 64,
            depth_defaulted: false,
            aperture_bytes: None,
        };
        let card = crate::manifest::Pcie::default();
        let map = |pcie: Option<&crate::manifest::Pcie>, mems: &[crate::terminator::AxiMemPlan]| {
            build(
                &dut,
                &manifest(CSR),
                &bindings,
                &[],
                &[],
                &[],
                &[],
                &[],
                mems,
                pcie,
            )
            .unwrap()
        };
        let names = |pcie: Option<&crate::manifest::Pcie>,
                     mems: &[crate::terminator::AxiMemPlan]| {
            map(pcie, mems)
                .registers
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>()
        };

        let pcie = names(Some(&card), std::slice::from_ref(&mem));
        assert!(pcie.iter().any(|n| n == "dma_base"), "{pcie:?}");
        // The map also names the memory it reaches, so the host does not use
        // the DMA engine for another bundle.
        assert_eq!(
            map(Some(&card), std::slice::from_ref(&mem))
                .requester
                .as_deref(),
            Some("mem")
        );
        assert_eq!(map(None, std::slice::from_ref(&mem)).requester, None);
        assert!(pcie.iter().any(|n| n == "dma_go"), "{pcie:?}");
        // Each reason for not issuing has its own counter.
        assert!(pcie.iter().any(|n| n == "dma_oor"), "{pcie:?}");
        assert!(pcie.iter().any(|n| n == "dma_blocked"), "{pcie:?}");
        // So does each silent loss.
        assert!(pcie.iter().any(|n| n == "dma_rq_drops"), "{pcie:?}");
        assert!(pcie.iter().any(|n| n == "dma_rq_gaps"), "{pcie:?}");
        assert!(pcie.iter().any(|n| n == "dma_rc_cor"), "{pcie:?}");
        assert!(pcie.iter().any(|n| n == "dma_rc_uncor"), "{pcie:?}");

        // Not on JTAG.
        let jtag = names(None, std::slice::from_ref(&mem));
        assert!(!jtag.iter().any(|n| n.starts_with("dma_")), "{jtag:?}");

        // Not without a memory.
        let empty = names(Some(&card), &[]);
        assert!(!empty.iter().any(|n| n.starts_with("dma_")), "{empty:?}");

        // Not behind a `bram`: it is not wide enough for the 256-bit DMA engine.
        let bram = crate::terminator::AxiMemPlan {
            backing: Backing::Bram,
            ..mem
        };
        assert_eq!(
            map(Some(&card), std::slice::from_ref(&bram)).requester,
            None
        );
        let bram = names(Some(&card), std::slice::from_ref(&bram));
        assert!(!bram.iter().any(|n| n.starts_with("dma_")), "{bram:?}");
    }

    /// A driver may expect its registers at a fixed offset (NVMe reads them
    /// from BAR offset 0). The others fill the gaps.
    #[test]
    fn a_region_with_a_base_goes_there_and_the_others_fill_the_gaps() {
        let dut = dut(vec![]);
        let slave = |bundle: &str, addr_width| crate::terminator::SlavePlan {
            bundle: bundle.to_string(),
            addr: format!("i_{bundle}_addr"),
            addr_width,
            rdata: format!("o_{bundle}_rdata"),
            width: 32,
            wdata: None,
            we: None,
            latency: 1,
        };
        // 4 KB and 64 bytes.
        let slaves = [slave("big", 10), slave("small", 4)];
        let toml = |big: &str, small: &str| {
            format!(
                "[dut]\nmodule = \"dut_top\"\n\n\
                 [bundle.big]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"slave\"\n{big}\n\
                 [bundle.small]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"slave\"\n{small}\n"
            )
        };
        let map = |big: &str, small: &str| {
            build(
                &dut,
                &manifest(&toml(big, small)),
                &[],
                &[],
                &[],
                &[],
                &[],
                &slaves,
                &[],
                None,
            )
        };
        let bases = |big: &str, small: &str| {
            map(big, small)
                .unwrap()
                .regions
                .iter()
                .map(|region| (region.bundle.clone(), region.base))
                .collect::<Vec<_>>()
        };
        let at = |big, small| vec![("big".to_string(), big), ("small".to_string(), small)];

        assert_eq!(bases("", ""), at(0, 0x1000));
        assert_eq!(bases("", "base = 0"), at(0x1000, 0));
        // `small` fills the gap below `big`.
        assert_eq!(bases("base = \"8k\"", ""), at(0x2000, 0));
        // The registers sit above the highest region.
        let high = map("base = 0x10000", "").unwrap();
        assert!(high.registers.iter().all(|r| r.offset >= 0x11000));

        // These two move no register, but the hash still changes, so `id`
        // catches a map that does not match the bitstream.
        // Both end at 0x2000.
        let swap_a = map("base = 0", "base = 0x1fc0").unwrap();
        let swap_b = map("base = 0x1000", "base = 0").unwrap();
        assert_eq!(
            swap_a
                .registers
                .iter()
                .map(|r| r.offset)
                .collect::<Vec<_>>(),
            swap_b
                .registers
                .iter()
                .map(|r| r.offset)
                .collect::<Vec<_>>()
        );
        assert_ne!(swap_a.map_hash, swap_b.map_hash);

        assert!(matches!(
            map("base = 0x800", ""),
            Err(RegMapError::BaseNotAligned {
                below: 0,
                above: 0x1000,
                ..
            })
        ));
        assert!(matches!(
            map("base = 0", "base = 0x40"),
            Err(RegMapError::BaseOverlaps { .. })
        ));
    }

    #[test]
    fn a_base_on_a_bundle_without_a_region_is_refused() {
        let dut = dut(vec![port("i_csr_addr", PortDirection::Input, 32)]);
        let result = build(
            &dut,
            &manifest(&format!("{CSR}base = 0\n")),
            &[binding("csr", &["i_csr_addr"])],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        );
        assert!(matches!(result, Err(RegMapError::BaseWithoutRegion { .. })));
    }
}

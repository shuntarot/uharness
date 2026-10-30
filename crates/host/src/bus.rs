//! The window, whichever way it is reached: JTAG, PCIe, or HNS2 over TCP.
//!
//! This is the entry point for a program that uses this crate as a library.
//! Open a `Bus`, check that the board runs the register map you hold, then
//! send `Batch`es.

use std::path::Path;

use hns_regs::RegisterMap;

use crate::bridge::{self, Batch, Bridge, Reads};
use crate::ftdi::{self, Ftdi};
use crate::mpsse::{Mpsse, ProbeConfig};
use crate::{bitstream, config, hns2, pcie};

/// Anything that went wrong on the way to the window or in it.
///
/// Each variant prints the cause and what to change, as the parts below it do.
#[derive(Debug)]
pub enum Error {
    /// The JTAG window.
    Jtag(bridge::Error<ftdi::Error>),
    /// The USB probe itself (opening it, or a transfer).
    Probe(ftdi::Error),
    /// The PCIe BAR.
    Pcie(pcie::Error),
    /// An HNS2 server.
    Net(hns2::Error),
    /// Sending a bitstream over JTAG.
    Config(config::Error<ftdi::Error>),
    /// Reading a bitstream file.
    Bitstream(bitstream::Error),
    /// A check refused to go on. The text says why and what to change.
    Refused(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Jtag(e) => e.fmt(f),
            Error::Probe(e) => e.fmt(f),
            Error::Pcie(e) => e.fmt(f),
            Error::Net(e) => e.fmt(f),
            Error::Config(e) => e.fmt(f),
            Error::Bitstream(e) => e.fmt(f),
            Error::Refused(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for Error {}

impl From<bridge::Error<ftdi::Error>> for Error {
    fn from(e: bridge::Error<ftdi::Error>) -> Self {
        Error::Jtag(e)
    }
}

impl From<ftdi::Error> for Error {
    fn from(e: ftdi::Error) -> Self {
        Error::Probe(e)
    }
}

impl From<pcie::Error> for Error {
    fn from(e: pcie::Error) -> Self {
        Error::Pcie(e)
    }
}

impl From<hns2::Error> for Error {
    fn from(e: hns2::Error) -> Self {
        Error::Net(e)
    }
}

impl From<config::Error<ftdi::Error>> for Error {
    fn from(e: config::Error<ftdi::Error>) -> Self {
        Error::Config(e)
    }
}

impl From<bitstream::Error> for Error {
    fn from(e: bitstream::Error) -> Self {
        Error::Bitstream(e)
    }
}

/// `hio` reports errors as text.
impl From<Error> for String {
    fn from(e: Error) -> Self {
        e.to_string()
    }
}

/// The window of one design.
pub enum Bus {
    Jtag(Bridge<Mpsse<Ftdi>>),
    Pcie(pcie::Bar),
    /// An HNS2 server: `veryl harness sim`, or `hio serve` with a board.
    Net(hns2::Link),
}

impl Bus {
    /// Opens the window over JTAG and selects its USER slot.
    ///
    /// `update_gap` is the number of idle TCK cycles after each Update. `None`
    /// works it out from the window clock in the map: a window slower than
    /// TCK needs them, or commands are dropped as busy.
    pub fn jtag(
        cfg: &ProbeConfig,
        map: &RegisterMap,
        update_gap: Option<usize>,
    ) -> Result<Bus, Error> {
        let mut io = Mpsse::open(Ftdi::open(cfg)?, cfg)?;
        io.set_update_gap(update_gap.unwrap_or_else(|| default_update_gap(io.tck(), map)));
        Ok(Bus::Jtag(Bridge::new(io, map.addr_bits() as u32)))
    }

    /// Opens the BAR of the card at `bdf`, or of the one card with the map's
    /// PCIe IDs when `bdf` is `None`.
    ///
    /// The ID alone is not enough: the default ID is borrowed from an example
    /// and may clash with another card. The BAR size is compared too, and the
    /// magic is read later (`check_identity`).
    pub fn pcie(bdf: Option<&str>, map: &RegisterMap) -> Result<Bus, Error> {
        let Some(ids) = &map.pcie else {
            return Err(Error::Refused(
                "regs.json has no `pcie` section, so the design has no PCIe endpoint.\n\
                 Generate with `--transport pcie` and re-synthesize, or reach the window \
                 over JTAG."
                    .to_string(),
            ));
        };
        let bar = pcie::Bar::open(&pcie_bdf(bdf, ids)?)?;
        bar.expect_bytes(ids.bar_bytes as u64)?;
        Ok(Bus::Pcie(bar))
    }

    /// Connects to an HNS2 server at `addr` (`host:port`).
    pub fn connect(addr: &str) -> Result<Bus, Error> {
        Ok(Bus::Net(hns2::Link::connect(addr)?))
    }

    /// Connects to the simulation whose `sim.addr` sits in `dir`.
    pub fn sim(dir: &Path) -> Result<Bus, Error> {
        Ok(Bus::Net(hns2::Link::open(dir)?))
    }

    pub fn run(&mut self, batch: &Batch) -> Result<Reads, Error> {
        Ok(match self {
            Bus::Jtag(bridge) => bridge.run(batch)?,
            Bus::Pcie(bar) => bar.run(batch)?,
            Bus::Net(link) => link.run(batch)?,
        })
    }

    /// The master this bus is. A moving window has one base register per
    /// master. With a shared one, a move by one master would make the other
    /// read a different address.
    pub fn master(&self) -> &'static str {
        match self {
            Bus::Jtag(_) => "jtag",
            Bus::Pcie(_) => "pcie",
            // The simulator drives the AXI4-Lite port JTAG drives on a board.
            Bus::Net(link) if link.info().master == "pcie" => "pcie",
            Bus::Net(_) => "jtag",
        }
    }

    pub fn read32(&mut self, addr: u32) -> Result<u32, Error> {
        Ok(match self {
            Bus::Jtag(bridge) => bridge.read32(addr)?,
            Bus::Pcie(bar) => bar.read32(addr)?,
            Bus::Net(link) => link.read32(addr)?,
        })
    }

    pub fn read_burst(&mut self, addr: u32, out: &mut [u32]) -> Result<(), Error> {
        match self {
            Bus::Jtag(bridge) => bridge.read_burst(addr, out)?,
            Bus::Pcie(bar) => bar.read_burst(addr, out)?,
            Bus::Net(link) => link.read_burst(addr, out)?,
        }
        Ok(())
    }

    /// Only PCIe reads in parallel. JTAG is one cable, so more threads only
    /// wait longer; there, batching (`Batch`) already cuts round trips.
    pub fn read_parallel(
        &mut self,
        addr: u32,
        out: &mut [u32],
        threads: usize,
    ) -> Result<(), Error> {
        match self {
            Bus::Pcie(bar) => bar.read_parallel(addr, out, threads)?,
            _ => self.read_burst(addr, out)?,
        }
        Ok(())
    }

    /// The magic and the map hash the window answers with.
    pub fn identity(&mut self, map: &RegisterMap) -> Result<(u32, u32), Error> {
        let mut b = Batch::new();
        let m = b.read(register(map, "harness_magic")?);
        let h = b.read(register(map, "harness_map_hash")?);
        let got = self.run(&b)?;
        Ok((got[m], got[h]))
    }

    /// Checks that the board runs the design `map` describes.
    pub fn check_identity(&mut self, map: &RegisterMap) -> Result<(), Error> {
        let (magic, hash) = self.identity(map)?;
        check_identity(magic, hash, map)
    }

    /// `check_identity`, given a moment for the harness to come out of reset
    /// after programming.
    pub fn check_identity_settled(&mut self, map: &RegisterMap) -> Result<(), Error> {
        let started = std::time::Instant::now();
        loop {
            match self.check_identity(map) {
                Ok(()) => return Ok(()),
                Err(_) if started.elapsed() < std::time::Duration::from_secs(2) => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// The update gap a JTAG window needs at `tck_hz`, from its clock in the map.
pub fn default_update_gap(tck_hz: u32, map: &RegisterMap) -> usize {
    match (map.window_clock_mhz, map.window_cycles) {
        (Some(mhz), Some(cycles)) => crate::mpsse::update_gap(tck_hz, mhz, cycles),
        _ => 0,
    }
}

/// The offset of a register the harness always has.
fn register(map: &RegisterMap, name: &str) -> Result<u32, Error> {
    map.get(name)
        .map(|r| r.offset as u32)
        .map_err(|e| Error::Refused(e.to_string()))
}

/// The card's BDF: `bdf` checked against the map's IDs, or the one card
/// that has them.
pub fn pcie_bdf(bdf: Option<&str>, ids: &hns_regs::Pcie) -> Result<String, Error> {
    let root = Path::new(pcie::SYSFS);
    let (vendor, device) = (ids.vendor_id as u16, ids.device_id as u16);
    Ok(match bdf {
        Some(bdf) => pcie::named_at(root, bdf, vendor, device)?,
        None => pcie::find_at(root, vendor, device)?.bdf,
    })
}

/// Compares the magic and the map hash with the map.
///
/// A magic with only some bits wrong means TCK is too fast. TDO must return
/// within half a cycle, and the board sets that limit, not the chip. The
/// same bits fail every time, so the same wrong value comes back.
pub fn check_identity(magic: u32, hash: u32, map: &RegisterMap) -> Result<(), Error> {
    if magic != map.magic {
        return Err(Error::Refused(format!(
            "the window answered with magic {magic:#010x}, not {:#010x}.\n\
             The board does not run a veryl-harness design, or the transport does not \
             reach its window.\n\
             If the board was just powered on, or runs another design (from flash or an \
             SD card), run `hio program` first.\n\
             If every bit is 0 or every bit is 1, suspect the JTAG chain.\n\
             If only some bits are wrong, --tck-hz is probably too fast for the cable. \
             Lower it and try again.\n\
             If the USER slot is the default, --user1-ir may be wrong: the wrong slot \
             reaches BYPASS, not the window.",
            map.magic
        )));
    }
    if hash != map.map_hash {
        return Err(Error::Refused(format!(
            "the bitstream reports map hash {hash:#010x}, but {} says {:#010x}.\n\
             The board runs a harness built from a different register map. Re-generate \
             and re-synthesize, or point --regs at the map that was built.",
            "regs.json", map.map_hash
        )));
    }
    Ok(())
}

//! Programming an FPGA over JTAG, with any bitstream.
//!
//! `plan` checks the bitstream against the target and works out what to
//! send; nothing touches the probe until `send`. Whether the new design is a
//! harness is the caller's business: check it afterwards with
//! `Bus::check_identity` when it is one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hns_regs::RegisterMap;

use crate::bitstream::{self, Bitstream};
use crate::bus::{Bus, Error, default_update_gap};
use crate::config::{self, Check, Chunk, ConfigIr};
use crate::ftdi::Ftdi;
use crate::mpsse::{self, Mpsse, ProbeConfig};
use crate::pcie;
use crate::reprogram::{self, Shape};

/// What to send, and where.
#[derive(Debug, Clone)]
pub struct Plan {
    pub ir: ConfigIr,
    pub chunks: Vec<Chunk>,
    /// Set when the device was not checked (`allow_any_device`). Show it.
    pub warning: Option<String>,
}

/// Checks the bitstream's device against the target, and works out the IR
/// values and which part of the bitstream goes where. Touches no probe.
///
/// Without a target there is nothing to compare with, so this refuses
/// unless `allow_any_device` is set.
pub fn plan(
    target: Option<&hns_targets::Target>,
    bit: &Bitstream,
    allow_any_device: bool,
) -> Result<Plan, Error> {
    let mut warning = None;
    match (target, allow_any_device) {
        (Some(t), _) => {
            let part = &t.head.device.part;
            if !bitstream::part_matches(&bit.part, part) {
                let e = bitstream::Error::WrongDevice {
                    bitstream: bit.part.clone(),
                    target: part.clone(),
                };
                if !allow_any_device {
                    return Err(e.into());
                }
                warning = Some(e.to_string());
            }
        }
        (None, true) => warning = Some("no target given, so the device is not checked".to_string()),
        (None, false) => {
            return Err(Error::Refused(
                "no target was given, so the bitstream's device cannot be checked.\n\
                 Pass --target (or --target-file) naming the board this .bit was built for. \
                 --allow-any-device skips the check."
                    .to_string(),
            ));
        }
    }

    let jtag = target.map(hns_targets::jtag).unwrap_or_default();
    let check = |c: hns_targets::Check| Check {
        ir: c.ir,
        expect: c.expect,
        mask: c.mask,
    };
    let ir = match (jtag.jprogram_ir, jtag.jstart_ir, jtag.bypass_ir) {
        (Some(jprogram), Some(jstart), Some(bypass)) => ConfigIr {
            jprogram,
            jstart,
            bypass,
            ready: jtag.ready.map(check),
            done: jtag.done.map(check),
        },
        _ => {
            return Err(Error::Refused(
                "the target does not say which IR values select JTAG configuration.\n\
                 `jprogram_ir`, `jstart_ir` and `bypass_ir` are needed. They differ by \
                 device family. Find them in the device's BSDL file, under \
                 data/parts/xilinx/<family>/public/bsdl/ in Vivado."
                    .to_string(),
            ));
        }
    };
    let chunks = split(bit, &jtag)?;
    Ok(Plan {
        ir,
        chunks,
        warning,
    })
}

/// Decides which part of the `.bit` goes to which `CFG_IN`.
///
/// One SLR gets everything in one go. With several, the `.bit` says where
/// each part starts and ends, and the target says which `CFG_IN` reaches
/// each SLR, in file order.
fn split(bit: &Bitstream, jtag: &hns_targets::Jtag) -> Result<Vec<Chunk>, Error> {
    let pieces = bit
        .slr_pieces()
        .map_err(|e| Error::Refused(e.to_string()))?;
    let slrs = pieces.iter().map(|p| p.slr + 1).max().unwrap_or(1);
    if slrs == 1 {
        if jtag.slr_cfg_in_ir.len() > 1 {
            return Err(Error::Refused(format!(
                "the target names {} SLRs, but the bitstream holds one.\n\
                 It was probably built for another device.",
                jtag.slr_cfg_in_ir.len()
            )));
        }
        let cfg_in = jtag
            .cfg_in_ir
            .or(jtag.slr_cfg_in_ir.first().copied())
            .ok_or(Error::Refused(
                "the target does not say which IR value selects CFG_IN.".to_string(),
            ))?;
        return Ok(vec![Chunk {
            at: 0,
            bytes: bit.data.len(),
            cfg_in,
            sync: false,
        }]);
    }
    if jtag.slr_cfg_in_ir.len() != slrs {
        return Err(Error::Refused(format!(
            "the bitstream holds {slrs} sub-bitstreams (one per SLR), but the target names \
             {} `slr_cfg_in_ir` values.\n\
             List the CFG_IN of each SLR in the order its sub-bitstream appears in the .bit \
             (the master SLR first). The BSDL file has one CFG_IN per SLR.",
            jtag.slr_cfg_in_ir.len()
        )));
    }
    Ok(pieces
        .iter()
        .map(|p| Chunk {
            at: p.at,
            bytes: p.bytes,
            cfg_in: jtag.slr_cfg_in_ir[p.slr],
            sync: p.tail,
        })
        .collect())
}

/// Sends the bitstream through a probe already set up.
pub fn send(
    m: &mut Mpsse<Ftdi>,
    plan: &Plan,
    ir_length: u8,
    bit: &Bitstream,
    erase_wait: Duration,
) -> Result<(), Error> {
    config::program(m, &plan.ir, ir_length, &bit.data, &plan.chunks, erase_wait)?;
    Ok(())
}

/// Opens the probe and sends the bitstream. Answers the probe, still open,
/// so a check afterwards does not have to open it again (a second open
/// fails while this one is held).
pub fn open_and_send(
    cfg: &ProbeConfig,
    plan: &Plan,
    bit: &Bitstream,
    erase_wait: Duration,
) -> Result<Mpsse<Ftdi>, Error> {
    let mut m = Mpsse::attach(Ftdi::open(cfg)?, cfg)?;
    send(&mut m, plan, cfg.ir_length, bit, erase_wait)?;
    Ok(m)
}

/// Resets the TAP and selects the USER slot again, after configuration.
pub fn select_window(m: &mut Mpsse<Ftdi>, cfg: &ProbeConfig) -> Result<(), Error> {
    let mut cmds = Vec::new();
    mpsse::push_tap_reset(&mut cmds);
    m.send(&cmds)?;
    m.scan_ir(cfg.user1_ir, cfg.ir_length)?;
    Ok(())
}

impl Bus {
    /// Programs through the probe this JTAG window already holds.
    ///
    /// The window does not answer until `reselect`. Between the two, the
    /// probe is free for other TAPs on the chain (`Bridge::io_mut`).
    pub fn program_jtag(
        &mut self,
        plan: &Plan,
        ir_length: u8,
        bit: &Bitstream,
        erase_wait: Duration,
    ) -> Result<(), Error> {
        let Bus::Jtag(bridge) = self else {
            return Err(Error::Refused(
                "this window is not reached over JTAG, so it holds no probe to program \
                 with."
                    .to_string(),
            ));
        };
        let m = bridge.io_mut();
        // The gap is for window commands; configuration runs without it.
        m.set_update_gap(0);
        let mut cmds = Vec::new();
        mpsse::push_tap_reset(&mut cmds);
        m.send(&cmds)?;
        send(m, plan, ir_length, bit, erase_wait)
    }

    /// Selects the window again after `program_jtag`. The new design's window
    /// may be a different size, so the DR length follows `map`.
    pub fn reselect(
        &mut self,
        cfg: &ProbeConfig,
        map: &RegisterMap,
        update_gap: Option<usize>,
    ) -> Result<(), Error> {
        let Bus::Jtag(bridge) = self else {
            return Ok(());
        };
        let m = bridge.io_mut();
        select_window(m, cfg)?;
        let gap = update_gap.unwrap_or_else(|| default_update_gap(m.tck(), map));
        m.set_update_gap(gap);
        bridge.set_addr_bits(map.addr_bits() as u32);
        Ok(())
    }
}

/// A PCIe card the machine keeps while it is programmed over JTAG.
///
/// Programming resets the card's config space. `prepare` saves it while the
/// card answers, and `restore` writes it back, so the window answers at the
/// same address. A design that looks different from the bus is refused by
/// `prepare`, before anything is programmed.
pub struct Card {
    root: PathBuf,
    bdf: String,
    /// The config space from when the card last answered at its address.
    saved: Option<Vec<u8>>,
    ids: (u16, u16),
}

impl Card {
    pub fn new(bdf: &str) -> Card {
        Card::at(Path::new(pcie::SYSFS), bdf)
    }

    /// Under another sysfs root, for tests.
    pub fn at(root: &Path, bdf: &str) -> Card {
        Card {
            root: root.to_path_buf(),
            bdf: bdf.to_string(),
            saved: None,
            ids: (0, 0),
        }
    }

    pub fn bdf(&self) -> &str {
        &self.bdf
    }

    /// Saves the config space if the card answers at its address now. Call
    /// it once when the window is opened: after a failed programming, the
    /// card no longer answers, and only an earlier copy can bring it back.
    pub fn save(&mut self) -> Result<(), Error> {
        let card = pcie::info_at(&self.root, &self.bdf)?;
        let config = reprogram::read_config(&self.root, &self.bdf).map_err(|e| {
            Error::Refused(format!("cannot read the config space of {}: {e}", self.bdf))
        })?;
        if reprogram::answering(&config, card.bar0_start) {
            self.saved = Some(config);
        }
        self.ids = (card.vendor, card.device);
        Ok(())
    }

    /// Checks that `next` looks the same from the bus, and saves the config
    /// space if the card answers now.
    pub fn prepare(&mut self, next: &RegisterMap) -> Result<(), Error> {
        let Some(want) = Shape::of_map(next) else {
            return Err(Error::Refused(
                "the regs.json sent has no `pcie` section: that design has no PCIe \
                 endpoint, and the machine would lose the card.\n\
                 Generate it with `--transport pcie`."
                    .to_string(),
            ));
        };
        let card = pcie::info_at(&self.root, &self.bdf)?;
        let changed = reprogram::changes(&Shape::of_card(&card), &want);
        if !changed.is_empty() {
            return Err(Error::Refused(format!(
                "the new design looks different from the bus ({}). Nothing was \
                 programmed.\n\
                 The machine has to enumerate the card again, which a running server \
                 cannot do. On the board's machine: stop `hio serve`, run `hio program`, \
                 then as root remove the card and rescan the bus (reboot instead if the \
                 BAR grew past the bridge window), and start `hio serve` again.",
                changed.join(", ")
            )));
        }
        self.save()?;
        if self.saved.is_none() {
            return Err(Error::Refused(format!(
                "{} does not answer at its address now, and no config space was saved \
                 from when it did. Nothing was programmed.\n\
                 Make the card answer by hand (as root, remove it and rescan the bus), \
                 then start `hio serve` again.",
                self.bdf
            )));
        }
        Ok(())
    }

    /// Waits for the link to come back and writes the saved config space.
    pub fn restore(&self) -> Result<(), Error> {
        let bdf = &self.bdf;
        let Some(saved) = &self.saved else {
            return Err(Error::Refused(format!(
                "no config space of {bdf} was saved; call `prepare` before programming."
            )));
        };
        reprogram::wait_for_card(
            &self.root,
            bdf,
            self.ids.0,
            self.ids.1,
            Duration::from_secs(10),
        )
        .map_err(Error::Refused)?;
        reprogram::write_config(&self.root, bdf, &reprogram::restore_plan(saved)).map_err(|e| {
            Error::Refused(format!("cannot write the config space of {bdf} back: {e}"))
        })
    }
}

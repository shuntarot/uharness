//! Configures the FPGA with a bitstream over JTAG, without Vivado.
//!
//! 7-series configuration only selects dedicated TAP instructions and shifts data
//! into the DR. It uses the same JTAG cable that normally programs the board.
//!
//! A failure does no harm. JTAG configuration is volatile, so a power cycle
//! restores the board. Nothing is written to flash.
//!
//! ## Sequence
//!
//! ```text
//!   IR = JPROGRAM   clear the configuration memory, and wait until it is clear
//!   IR = CFG_IN     shift the whole bitstream into the DR
//!   IR = JSTART     run the startup sequence
//!   IR = BYPASS     release the TAP
//! ```
//!
//! The best check is that the design runs: if the bitstream is a harness,
//! `hio id` returns the magic value.
//!
//! ## Wait in real time, not in clocks
//!
//! The erase wait must be real time. A wait counted in TCK clocks gets shorter as
//! TCK gets faster: 100,000 clocks at 20 MHz is only 5 ms. With too short a wait,
//! the device ignores all data sent to `CFG_IN`, even though the IR values and
//! bit order are correct.

use crate::mpsse::{Chan, Mpsse, push_enter, push_idle_clocks, push_write_bytes};

/// One check on the IR capture value.
///
/// It does not interpret the bits. It keeps the `TDO`/`MASK` values from the SVF
/// as they are, because a guessed meaning would silently break on another device
/// family.
#[derive(Debug, Clone, Copy)]
pub struct Check {
    pub ir: u32,
    pub expect: u32,
    pub mask: u32,
}

/// IR values for configuration. They differ per device family, so they come from
/// the target description.
#[derive(Debug, Clone)]
pub struct ConfigIr {
    pub jprogram: u32,
    pub jstart: u32,
    pub bypass: u32,
    /// Reads whether the erase is done. Without it, `program` only waits.
    pub ready: Option<Check>,
    /// Reads DONE. Without it, `program` does not check that configuration worked.
    pub done: Option<Check>,
}

/// A configuration failure.
#[derive(Debug)]
pub enum Error<E> {
    Io(E),
    /// The erase never finished.
    NotReady {
        got: u32,
        expect: u32,
        mask: u32,
    },
    /// DONE did not go high. The device is not configured.
    NotDone {
        got: u32,
        expect: u32,
        mask: u32,
    },
}

impl<E: std::fmt::Debug> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "jtag io failed: {e:?}"),
            Error::NotReady { got, expect, mask } => write!(
                f,
                "the device never reported the configuration memory as cleared \
                 (IR capture {got:#x}, wanted {expect:#x} under mask {mask:#x}).\n\
                 Erasing takes longer on bigger devices. Raise the wait with --erase-wait-ms. \
                 If it never clears, JPROGRAM may not be the instruction the target says."
            ),
            Error::NotDone { got, expect, mask } => write!(
                f,
                "the bitstream was sent, but DONE did not go high \
                 (IR capture {got:#x}, wanted {expect:#x} under mask {mask:#x}).\n\
                 The device is not configured. On a device with several SLRs, the parts may \
                 have gone to the wrong SLRs: check the order of the target's `slr_cfg_in_ir`. \
                 `hio program <file>.svf` replays Vivado's own sequence and shows whether the \
                 board itself is fine."
            ),
        }
    }
}

impl<E: std::fmt::Debug> std::error::Error for Error<E> {}

/// A part of the `.bit` file and where to send it.
#[derive(Debug, Clone, Copy)]
pub struct Chunk {
    pub at: usize,
    pub bytes: usize,
    pub cfg_in: u32,
    /// Send the sync word before this chunk.
    pub sync: bool,
}

/// TCK clocks run while the erase proceeds.
const ERASE_CLOCKS: usize = 10_000;

/// Default real-time wait for the erase. Vivado waits 0.1 s.
pub const ERASE_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// Clocks run before `JSTART`. Vivado runs 100000 here.
const BEFORE_START_CLOCKS: usize = 100_000;

/// Clocks run after `JSTART`. Vivado runs only 100.
const AFTER_START_CLOCKS: usize = 100;

/// Bitstream bytes per USB transfer.
const CHUNK: usize = 16 * 1024;

/// Sync word. The last chunk of each SLR starts with it.
const SYNC: [u8; 4] = [0xaa, 0x99, 0x55, 0x66];

/// Only clears the configuration memory. Use it to check the `JPROGRAM` IR value
/// on its own.
pub fn erase<C: Chan>(
    m: &mut Mpsse<C>,
    jprogram_ir: u32,
    ir_length: u8,
    wait: std::time::Duration,
) -> Result<(), C::Error> {
    m.scan_ir(jprogram_ir, ir_length)?;
    let mut cmds = Vec::new();
    push_idle_clocks(&mut cmds, ERASE_CLOCKS);
    m.send(&cmds)?;
    std::thread::sleep(wait);
    Ok(())
}

/// Configures the device with a bitstream.
///
/// `chunks` says which part of the `.bit` goes to which `CFG_IN`. A single-SLR
/// device has one chunk that covers the whole file. The order follows Vivado's
/// SVF: the long clock run comes before `JSTART`.
pub fn program<C: Chan>(
    m: &mut Mpsse<C>,
    ir: &ConfigIr,
    ir_length: u8,
    data: &[u8],
    chunks: &[Chunk],
    erase_wait: std::time::Duration,
) -> Result<(), Error<C::Error>> {
    assert!(!chunks.is_empty(), "nothing to send");

    // 1. Clear the configuration memory, then read back that it is ready.
    m.scan_ir(ir.jprogram, ir_length).map_err(Error::Io)?;
    if let Some(c) = ir.ready {
        m.scan_ir(c.ir, ir_length).map_err(Error::Io)?;
    }
    let mut cmds = Vec::new();
    push_idle_clocks(&mut cmds, ERASE_CLOCKS);
    m.send(&cmds).map_err(Error::Io)?;
    std::thread::sleep(erase_wait);

    // A fixed wait is not enough: bigger devices take longer to erase.
    if let Some(c) = ir.ready {
        poll(m, ir_length, c, erase_wait).map_err(|e| match e {
            Polled::Io(e) => Error::Io(e),
            Polled::Never(got) => Error::NotReady {
                got,
                expect: c.expect,
                mask: c.mask,
            },
        })?;
    }

    // 2. Send each chunk to its own CFG_IN.
    for c in chunks {
        m.scan_ir(c.cfg_in, ir_length).map_err(Error::Io)?;
        if c.sync {
            // Send the sync word first and stop in Pause-DR, as Vivado does.
            let mut cmds = Vec::new();
            crate::mpsse::push_enter(&mut cmds, false);
            let flipped: Vec<u8> = SYNC.iter().map(|b| b.reverse_bits()).collect();
            shift_out(m, &mut cmds, &flipped, crate::mpsse::Exit::Pause).map_err(Error::Io)?;
            shift_in(m, &data[c.at..c.at + c.bytes], true).map_err(Error::Io)?;
        } else {
            shift_in(m, &data[c.at..c.at + c.bytes], false).map_err(Error::Io)?;
        }
    }

    // 3. Startup. The long clock run comes first.
    let mut cmds = Vec::new();
    push_idle_clocks(&mut cmds, BEFORE_START_CLOCKS);
    m.send(&cmds).map_err(Error::Io)?;
    m.scan_ir(ir.jstart, ir_length).map_err(Error::Io)?;
    let mut cmds = Vec::new();
    push_idle_clocks(&mut cmds, AFTER_START_CLOCKS);
    m.send(&cmds).map_err(Error::Io)?;

    // 4. Read DONE. Without this, a failed configuration goes unnoticed.
    if let Some(c) = ir.done {
        poll(m, ir_length, c, std::time::Duration::from_millis(200)).map_err(|e| match e {
            Polled::Io(e) => Error::Io(e),
            Polled::Never(got) => Error::NotDone {
                got,
                expect: c.expect,
                mask: c.mask,
            },
        })?;
    }

    // 5. Release the TAP.
    m.scan_ir(ir.bypass, ir_length).map_err(Error::Io)?;
    Ok(())
}

enum Polled<E> {
    Io(E),
    /// The value never matched. Holds the last value read.
    Never(u32),
}

/// Reads the IR capture until it matches or the budget runs out.
fn poll<C: Chan>(
    m: &mut Mpsse<C>,
    ir_length: u8,
    c: Check,
    budget: std::time::Duration,
) -> Result<(), Polled<C::Error>> {
    let deadline = std::time::Instant::now() + budget.max(std::time::Duration::from_millis(50));
    loop {
        let last = m.scan_ir(c.ir, ir_length).map_err(Polled::Io)?;
        if last & c.mask == c.expect & c.mask {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(Polled::Never(last));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Shifts out `bytes` and stops in `exit`.
fn shift_out<C: Chan>(
    m: &mut Mpsse<C>,
    cmds: &mut Vec<u8>,
    bytes: &[u8],
    exit: crate::mpsse::Exit,
) -> Result<(), C::Error> {
    let last_byte = bytes[bytes.len() - 1];
    crate::mpsse::push_write_bytes(cmds, &bytes[..bytes.len() - 1]);
    crate::mpsse::push_write_bits(cmds, last_byte, 7);
    crate::mpsse::push_tail_quiet(cmds, last_byte & 0x80 != 0, exit);
    m.send(cmds)?;
    cmds.clear();
    Ok(())
}

/// Enters Shift-DR, shifts all of `data`, and returns to Run-Test/Idle.
///
/// The TAP stays in Shift-DR until the end. The data is split only for USB
/// transfers. With `resume`, it continues from Pause-DR.
fn shift_in<C: Chan>(m: &mut Mpsse<C>, data: &[u8], resume: bool) -> Result<(), C::Error> {
    let (last, body) = data.split_last().expect("checked non-empty");

    let mut cmds = Vec::new();
    if resume {
        crate::mpsse::push_resume_from_pause(&mut cmds);
    } else {
        push_enter(&mut cmds, false);
    }

    for part in body.chunks(CHUNK) {
        let flipped: Vec<u8> = part.iter().map(|b| b.reverse_bits()).collect();
        push_write_bytes(&mut cmds, &flipped);
        m.send(&cmds)?;
        cmds.clear();
    }

    // Last byte: send 7 bits normally. Send the final bit on the same clock as
    // TMS=1, then walk Exit1 -> Update -> Run-Test/Idle.
    let flipped = last.reverse_bits();
    crate::mpsse::push_write_bits(&mut cmds, flipped, 7);
    crate::mpsse::push_tail_quiet(&mut cmds, flipped & 0x80 != 0, crate::mpsse::Exit::Idle);
    m.send(&cmds)?;
    Ok(())
}

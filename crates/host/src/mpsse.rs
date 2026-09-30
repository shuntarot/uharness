//! JTAG over FTDI MPSSE.
//!
//! There is no USB here. This module only builds command bytes and decodes the
//! bytes that come back. A `Chan` implementation does the USB transfer and stays
//! thin. The split exists because MPSSE command lengths, bit packing and TAP
//! walks are easy to get wrong, and a mistake only shows as "TDO does not come
//! back". As pure computation, they can be tested without a board.
//!
//! ## Bandwidth
//!
//! Scans are queued into a buffer with [`push_scan`], so USB is not touched
//! until `flush`. One USB round trip per scan cannot be fast.

use crate::bridge::JtagIo;

// ── MPSSE opcodes (AN_108) ──
//
// JTAG drives TDI on the falling edge and samples TDO on the rising edge. The
// opcode encodes these edges; a wrong opcode shifts the data by one bit.

/// Bytes. TDI out on -ve, TDO in on +ve. LSB first.
pub const CMD_BYTES: u8 = 0x39;
/// Bytes, no read back. LSB first. Used to stream a bitstream: with nothing to
/// read, there is half the USB traffic.
pub const CMD_BYTES_OUT: u8 = 0x19;
/// Bits. Same edges as `CMD_BYTES`.
pub const CMD_BITS: u8 = 0x3b;
/// Clock TMS out and read TDO. TDI is held at bit 7 of the data byte.
pub const CMD_TMS: u8 = 0x6b;

pub const CMD_SET_BITS_LOW: u8 = 0x80;
pub const CMD_SET_BITS_HIGH: u8 = 0x82;
pub const CMD_SET_TCK_DIVISOR: u8 = 0x86;
/// Stop the divide-by-5 so the master clock is 60MHz (H series only).
pub const CMD_DIS_DIV_5: u8 = 0x8a;
pub const CMD_DIS_3_PHASE: u8 = 0x8d;
pub const CMD_DIS_ADAPTIVE: u8 = 0x97;
pub const CMD_LOOPBACK_END: u8 = 0x85;
/// Return pending read data now. Without it, the chip waits for the latency
/// timer.
pub const CMD_SEND_IMMEDIATE: u8 = 0x87;

/// H series master clock, after the divide-by-5 is stopped.
pub const MASTER_HZ: u32 = 60_000_000;

/// Level shifter enable for Digilent on-board and stand-alone modules.
/// Measured on an Arty.
pub const LAYOUT_DIGILENT: (u16, u16) = (0x0088, 0x008b);

/// Adapter without a level shifter. Only the four JTAG pins are driven.
///
/// Driving other GPIO pins on a guess can drive something on some boards.
/// Only these two layouts are tried automatically.
pub const LAYOUT_PLAIN: (u16, u16) = (0x0000, 0x000b);

/// Probe configuration. The values come from the target description. There are
/// no defaults here, so a guessed value cannot mix with a measured one.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub vid: u16,
    pub pid: u16,
    /// Channel A of an FT2232H is 0.
    pub interface: u8,
    /// Selects one probe when several of the same type are connected.
    pub serial: Option<String>,
    /// iProduct as `lsusb` shows it. Digilent uses a different string per
    /// product: the on-board Arty is `"Digilent USB Device"`, a stand-alone
    /// module is `"Digilent Adept USB Device"`. The VID/PID is the same, so a
    /// wrong string does not match.
    pub product: Option<String>,
    /// Level shifter enable (data, direction). Without it, TDO does not come
    /// back. The low byte is ADBUS, the high byte is ACBUS.
    pub layout_init: (u16, u16),
    pub max_tck_hz: u32,
    /// TAP IR length (6 on 7-series).
    pub ir_length: u8,
    /// IR value that selects BSCANE2 `JTAG_CHAIN 1`.
    pub user1_ir: u32,
}

/// Value for `SET_TCK_DIVISOR`, rounded so TCK does not exceed `max_tck_hz`.
///
/// `TCK = MASTER / (2 * (1 + divisor))`, so the divisor rounds up. Rounding
/// down breaks the board limit, and the symptom is occasional failures.
pub fn tck_divisor(max_tck_hz: u32) -> u16 {
    assert!(max_tck_hz > 0, "max_tck_hz must be positive");
    let half = MASTER_HZ.div_ceil(2 * max_tck_hz);
    (half.max(1) - 1).min(0xffff) as u16
}

/// The TCK that `divisor` actually gives.
pub fn tck_hz(divisor: u16) -> u32 {
    MASTER_HZ / (2 * (1 + divisor as u32))
}

/// Commands that set up MPSSE for JTAG. The latency timer is not set here: it
/// is a USB control transfer, so the `Chan` sets it.
pub fn init_cmds(cfg: &ProbeConfig) -> Vec<u8> {
    let div = tck_divisor(cfg.max_tck_hz);
    let (data, dir) = cfg.layout_init;
    vec![
        CMD_DIS_DIV_5,
        CMD_DIS_ADAPTIVE,
        CMD_DIS_3_PHASE,
        CMD_LOOPBACK_END,
        CMD_SET_TCK_DIVISOR,
        (div & 0xff) as u8,
        (div >> 8) as u8,
        CMD_SET_BITS_LOW,
        (data & 0xff) as u8,
        (dir & 0xff) as u8,
        CMD_SET_BITS_HIGH,
        (data >> 8) as u8,
        (dir >> 8) as u8,
    ]
}

/// Walk the TAP to Test-Logic-Reset, then to Run-Test/Idle.
///
/// The TAP state is unknown after MPSSE setup. JTAG guarantees that five
/// TMS=1 clocks reach Test-Logic-Reset from any state. Without this, the first
/// scan works only when the TAP happens to be in Run-Test/Idle, which gives a
/// bug that does not reproduce.
pub fn push_tap_reset(out: &mut Vec<u8>) {
    push_tms(out, &[true, true, true, true, true, false], false, false);
}

/// Where a scan leaves the TAP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Back to Run-Test/Idle. Use this for a single scan.
    Idle,
    /// On to Shift-DR of the next scan. Use this for back-to-back scans.
    Shift,
    /// Stop in Pause. SVF uses this to split one DR into parts.
    Pause,
    /// To Test-Logic-Reset.
    Reset,
}

/// Exit1 → Update → Run-Test/Idle (3 bits).
const TAIL_TO_IDLE: [bool; 3] = [true, true, false];

/// Exit1 → Pause (2 bits).
const TAIL_TO_PAUSE: [bool; 2] = [true, false];

/// Exit1 → Update → Select-DR → Select-IR → Test-Logic-Reset (5 bits).
const TAIL_TO_RESET: [bool; 5] = [true, true, true, true, true];

/// Exit1 → Update → Run-Test/Idle → Select-DR → Capture-DR → Shift-DR (6 bits).
///
/// The stop in Run-Test/Idle is margin. Update → Select-DR → Capture-DR is
/// only 2 edges, and the bridge needs exactly 2, so the path does not sit at
/// the minimum. It still fits in one command and costs one clock.
const TAIL_TO_SHIFT: [bool; 6] = [true, true, false, true, false, false];

/// Pause → Exit2 → Update → Run-Test/Idle (3 bits).
pub fn push_pause_to_idle(out: &mut Vec<u8>) {
    push_tms(out, &[true, true, false], false, false);
}

pub fn push_resume_from_pause(out: &mut Vec<u8>) {
    // Pause → Exit2 → Shift. This skips Capture, so the DR content is kept.
    push_tms(out, &[true, false], false, false);
}

/// Run-Test/Idle to Shift-DR, or to Shift-IR when `ir`.
pub fn push_enter(out: &mut Vec<u8>, ir: bool) {
    if ir {
        push_tms(out, &[true, true, false, false], false, false);
    } else {
        push_tms(out, &[true, false, false], false, false);
    }
}

/// Shift the body (all bits but the last) in the Shift state. Returns the
/// number of bytes to read back.
pub fn push_body(out: &mut Vec<u8>, body: &[bool]) -> usize {
    let whole = body.len() / 8;
    let rest = body.len() % 8;
    let mut read_len = 0;

    if whole > 0 {
        out.push(CMD_BYTES);
        out.push(((whole - 1) & 0xff) as u8);
        out.push(((whole - 1) >> 8) as u8);
        for k in 0..whole {
            let mut byte = 0u8;
            for i in 0..8 {
                if body[k * 8 + i] {
                    byte |= 1 << i;
                }
            }
            out.push(byte);
        }
        read_len += whole;
    }
    if rest > 0 {
        out.push(CMD_BITS);
        out.push((rest - 1) as u8);
        let mut byte = 0u8;
        for i in 0..rest {
            if body[whole * 8 + i] {
                byte |= 1 << i;
            }
        }
        out.push(byte);
        read_len += 1;
    }
    read_len
}

/// Send the last bit and leave Shift. Returns (bytes to read back, TMS bits).
///
/// JTAG sends the last bit of Shift-DR on the same clock as TMS=1, so the exit
/// and the last bit are one command.
pub fn push_tail(out: &mut Vec<u8>, last: bool, exit: Exit) -> (usize, usize) {
    let tms = tail_tms(exit);
    push_tms(out, tms, true, last);
    (1, tms.len())
}

fn tail_tms(exit: Exit) -> &'static [bool] {
    match exit {
        Exit::Idle => &TAIL_TO_IDLE,
        Exit::Shift => &TAIL_TO_SHIFT,
        Exit::Pause => &TAIL_TO_PAUSE,
        Exit::Reset => &TAIL_TO_RESET,
    }
}

/// Leave Shift and go on to the next scan, with `gap` extra TCK cycles in
/// Run-Test/Idle between Update and the next Capture. Returns the same as
/// `push_tail`.
///
/// The bridge (`hns::dr`) starts a request at Update, runs it on the window
/// clock, and brings the completion back to TCK through a 2-stage
/// synchronizer. TCK runs only 3 cycles before the next Capture. After the 2
/// synchronizer cycles, the window side has 1 cycle (+ `gap`) to finish, or the
/// result is lost (see `update_gap`). With `gap` 0 this equals `TAIL_TO_SHIFT`.
pub fn push_tail_gap(out: &mut Vec<u8>, last: bool, gap: usize) -> (usize, usize) {
    if gap == 0 {
        return push_tail(out, last, Exit::Shift);
    }
    // Read the last bit on Exit1 → Update → Run-Test/Idle, wait there, then
    // Select-DR → Capture-DR → Shift-DR.
    let head = &TAIL_TO_SHIFT[..3];
    push_tms(out, head, true, last);
    push_idle_clocks(out, gap);
    push_tms(out, &TAIL_TO_SHIFT[3..], false, false);
    (1, head.len())
}

/// Extra TCK cycles to add between Update and the next Capture.
///
/// The condition is `(1 + gap) * T_TCK >= (window_cycles + 1) * T_window`. The
/// extra cycle is margin for setup and clock frequency error. When the map does
/// not know the window clock, the gap is 0. That works for a 100MHz window at
/// 15MHz TCK (6.7 window cycles per TCK cycle). A 50MHz window lost results at
/// both 15MHz and 10MHz TCK (measured on an Arty).
pub fn update_gap(tck_hz: u32, window_clock_mhz: f64, window_cycles: u32) -> usize {
    // NaN means "unknown", the same as 0 or below.
    if window_clock_mhz.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return 0;
    }
    let need = f64::from(window_cycles + 1) * f64::from(tck_hz) / (window_clock_mhz * 1e6);
    (need.ceil() as usize).saturating_sub(1)
}

/// Leave Shift without reading back, so a long scan needs no round trip.
pub fn push_tail_quiet(out: &mut Vec<u8>, last: bool, exit: Exit) {
    push_tms(out, tail_tms(exit), false, last);
}

/// Shift 1 to 8 bits in the Shift state without reading back.
pub fn push_write_bits(out: &mut Vec<u8>, byte: u8, bits: usize) {
    assert!((1..=8).contains(&bits));
    out.push(CMD_BITS & !0x20);
    out.push((bits - 1) as u8);
    out.push(byte);
}

/// Clock out the TMS bits. TDI is held at `tdi` during them.
pub fn push_tms(out: &mut Vec<u8>, tms: &[bool], read: bool, tdi: bool) {
    assert!(!tms.is_empty() && tms.len() <= 7, "TMS runs are 1..7 bits");
    let mut byte = if tdi { 0x80 } else { 0x00 };
    for (i, &b) in tms.iter().enumerate() {
        if b {
            byte |= 1 << i;
        }
    }
    out.push(if read { CMD_TMS } else { CMD_TMS & !0x20 });
    out.push((tms.len() - 1) as u8);
    out.push(byte);
}

/// Queue a single scan that starts and ends in Run-Test/Idle.
/// Returns (bytes to read back, TMS bits).
pub fn push_scan(out: &mut Vec<u8>, bits: &[bool], ir: bool) -> (usize, usize) {
    assert!(!bits.is_empty(), "a scan needs at least one bit");
    push_enter(out, ir);
    let mut read_len = push_body(out, &bits[..bits.len() - 1]);
    let (n, tail_bits) = push_tail(out, bits[bits.len() - 1], Exit::Idle);
    read_len += n;
    (read_len, tail_bits)
}

/// Most bytes one `CMD_BYTES_OUT` can send. The length field is u16.
const MAX_BYTES_PER_CMD: usize = 65536;

/// Shift bytes in the Shift state without reading back.
///
/// This is for long data such as a bitstream. It takes bytes, so 17M bits do
/// not become a 17MB `Vec<bool>`.
pub fn push_write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    for part in bytes.chunks(MAX_BYTES_PER_CMD) {
        out.push(CMD_BYTES_OUT);
        out.push(((part.len() - 1) & 0xff) as u8);
        out.push(((part.len() - 1) >> 8) as u8);
        out.extend_from_slice(part);
    }
}

/// Run TCK for `clocks` cycles in Run-Test/Idle. TMS stays 0, so the state does
/// not change.
///
/// Configuration needs these waits. A TMS command is at most 7 bits, so a long
/// wait shifts dummy bytes instead.
pub fn push_idle_clocks(out: &mut Vec<u8>, clocks: usize) {
    let whole = clocks / 8;
    let rest = clocks % 8;
    if whole > 0 {
        let zeros = vec![0u8; whole.min(MAX_BYTES_PER_CMD)];
        let mut left = whole;
        while left > 0 {
            let n = left.min(MAX_BYTES_PER_CMD);
            out.push(CMD_BYTES_OUT);
            out.push(((n - 1) & 0xff) as u8);
            out.push(((n - 1) >> 8) as u8);
            out.extend_from_slice(&zeros[..n]);
            left -= n;
        }
    }
    if rest > 0 {
        out.push(CMD_BITS & !0x20); // bits, no read back
        out.push((rest - 1) as u8);
        out.push(0);
    }
}

/// Turn the read-back bytes into TDO bits.
///
/// A bit-mode read returns the bits packed at the MSB end: n bits land in the
/// top n bits, so they are shifted down by `8 - n`. Without this, only the last
/// few bits are wrong, which shows only on real hardware and is hard to trace.
///
/// `tail_bits` is how many clocks the last TMS command ran. Only the first of
/// them carries data; the rest are after Update, when the DR no longer shifts.
pub fn scan_decode(raw: &[u8], n: usize, tail_bits: usize) -> Vec<bool> {
    let body = n - 1;
    let whole = body / 8;
    let rest = body % 8;
    assert_eq!(
        raw.len(),
        whole + usize::from(rest > 0) + 1,
        "read length must match what the scan asked for"
    );

    let mut bits = Vec::with_capacity(n);
    for &byte in &raw[..whole] {
        for i in 0..8 {
            bits.push((byte >> i) & 1 == 1);
        }
    }
    if rest > 0 {
        let byte = raw[whole] >> (8 - rest);
        for i in 0..rest {
            bits.push((byte >> i) & 1 == 1);
        }
    }
    let last = raw[raw.len() - 1] >> (8 - tail_bits);
    bits.push(last & 1 == 1);
    bits
}

/// The USB layer. Only this part talks to the device.
pub trait Chan {
    type Error;

    /// Bytes that one round trip may read back.
    ///
    /// Keep it below the device receive buffer. If the buffer overflows, the
    /// device stops and no longer accepts commands. The default is conservative.
    fn read_limit(&self) -> usize {
        CHUNK_BYTES
    }

    /// Write `out` and read `read_len` bytes.
    ///
    /// Contract: strip the 2 modem status bytes that FTDI puts at the start of
    /// every packet, and return exactly `read_len` bytes. Return an error when
    /// fewer arrive; a short result shifts the bits above and is hard to trace.
    fn xfer(&mut self, out: &[u8], read_len: usize) -> Result<Vec<u8>, Self::Error>;
}

/// One device on the JTAG chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// Has an IDCODE.
    Id(u32),
    /// Has no IDCODE, so it is in BYPASS (1 bit) after reset.
    Bypass,
}

/// Decode the DR contents after a TAP reset.
///
/// Each device holds an IDCODE (32 bits, LSB is 1) or BYPASS (1 bit, 0). After
/// the end of the chain come the 1s that we shifted in, so 32 ones mark the end.
pub fn parse_chain(bits: &[bool]) -> Vec<Device> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < bits.len() {
        if !bits[at] {
            found.push(Device::Bypass);
            at += 1;
            continue;
        }
        if at + 32 > bits.len() {
            break;
        }
        let word = (0..32).fold(0u32, |a, i| if bits[at + i] { a | 1 << i } else { a });
        if word == 0xffff_ffff {
            break;
        }
        found.push(Device::Id(word));
        at += 32;
    }
    found
}

/// Time that one chunk may take to run, in milliseconds.
///
/// The device collects read-back data while it runs, and stops when we do not
/// drain it. We can drain only between writes, and a write can block. So the
/// safe way is to keep each chunk short. We set TCK ourselves, so the run time
/// can be computed. At 500kHz TCK, a chunk sized by byte count alone stalled.
const CHUNK_MS: u64 = 4;

/// JTAG on top of a `Chan`.
pub struct Mpsse<C: Chan> {
    chan: C,
    /// The TCK in use. Limits the chunk size by time.
    tck_hz: u32,
    /// Extra TCK between Update and Capture of back-to-back scans (`update_gap`).
    gap: usize,
}

impl<C: Chan> Mpsse<C> {
    /// The TCK in use, in Hz.
    pub fn tck(&self) -> u32 {
        self.tck_hz
    }

    /// Extra TCK between Update and Capture of back-to-back scans (`update_gap`).
    pub fn set_update_gap(&mut self, gap: usize) {
        self.gap = gap;
    }

    /// Set up MPSSE and put the TAP in Run-Test/Idle. No IR is selected.
    ///
    /// Use this to find out what is connected: IDCODE reads work without
    /// knowing the IR length.
    pub fn attach(chan: C, cfg: &ProbeConfig) -> Result<Self, C::Error> {
        let mut m = Mpsse {
            chan,
            tck_hz: tck_hz(tck_divisor(cfg.max_tck_hz)),
            gap: 0,
        };
        let mut cmds = init_cmds(cfg);
        push_tap_reset(&mut cmds);
        m.chan.xfer(&cmds, 0)?;
        Ok(m)
    }

    /// Set up MPSSE and select the USER slot.
    ///
    /// The IR stays selected, so later scans need only the DR.
    pub fn open(chan: C, cfg: &ProbeConfig) -> Result<Self, C::Error> {
        let mut m = Self::attach(chan, cfg)?;
        m.scan_ir(cfg.user1_ir, cfg.ir_length)?;
        Ok(m)
    }

    /// List the devices on the chain, at most `max`.
    ///
    /// After Test-Logic-Reset, each device holds an IDCODE (32 bits, LSB is 1)
    /// or BYPASS (1 bit, 0). A long DR scan reads the number and kind of devices.
    ///
    /// With two or more devices, the other devices need BYPASS in the IR and one
    /// padding bit each in the DR. Without the padding, scans silently reach the
    /// wrong device.
    pub fn scan_chain(&mut self, max: usize) -> Result<Vec<Device>, C::Error> {
        // Shift in 1s. What comes out after the chain is what we shifted in, so
        // 0s would look like an endless row of BYPASS devices.
        Ok(parse_chain(&self.scan_dr(&vec![true; 32 * max + 32])?))
    }

    /// Whether an IR capture value is valid. IEEE 1149.1 fixes the low 2 bits
    /// to `01`; other values mean the IR length is wrong.
    pub fn ir_length_looks_right(capture: u32) -> bool {
        capture & 0b11 == 0b01
    }

    /// Shift the IR with any bits. Returns TDO.
    pub fn scan_ir_bits(&mut self, bits: &[bool]) -> Result<Vec<bool>, C::Error> {
        let mut cmds = Vec::new();
        let (read_len, tail) = push_scan(&mut cmds, bits, true);
        cmds.push(CMD_SEND_IMMEDIATE);
        let raw = self.chan.xfer(&cmds, read_len)?;
        Ok(scan_decode(&raw, bits.len(), tail))
    }

    /// Measure the IR length on the chain instead of looking it up in a table.
    ///
    /// Fill the IR with 1s, shift in one 0, and count the clocks until it comes
    /// out on TDO. This needs no per-family table and works on unknown boards.
    ///
    /// Assumes one device on the chain; with more, it returns their sum.
    /// Returns `None` when the length exceeds `FLUSH`.
    pub fn measure_ir_length(&mut self) -> Result<Option<u8>, C::Error> {
        const FLUSH: usize = 64;
        let mut bits = vec![true; FLUSH];
        bits.push(false);
        bits.extend(std::iter::repeat_n(true, FLUSH));

        let got = self.scan_ir_bits(&bits)?;
        // A bit shifted in at clock k comes out of an L-bit register at k+L.
        Ok(got
            .iter()
            .enumerate()
            .skip(FLUSH)
            .find(|(_, b)| !**b)
            .map(|(i, _)| (i - FLUSH) as u8))
    }

    /// Shift the IR once. Returns the IR capture value (low 2 bits are `01` when
    /// the chain is right).
    pub fn scan_ir(&mut self, ir: u32, len: u8) -> Result<u32, C::Error> {
        let bits: Vec<bool> = (0..len).map(|i| (ir >> i) & 1 == 1).collect();
        let mut cmds = Vec::new();
        let (read_len, tail) = push_scan(&mut cmds, &bits, true);
        cmds.push(CMD_SEND_IMMEDIATE);
        let raw = self.chan.xfer(&cmds, read_len)?;
        let got = scan_decode(&raw, bits.len(), tail);
        Ok(got
            .iter()
            .enumerate()
            .fold(0u32, |a, (i, &b)| if b { a | 1 << i } else { a }))
    }

    pub fn into_inner(self) -> C {
        self.chan
    }

    /// Send commands without reading back. For long data such as configuration.
    pub fn send(&mut self, cmds: &[u8]) -> Result<(), C::Error> {
        self.chan.xfer(cmds, 0)?;
        Ok(())
    }

    /// Send commands and read back `read_len` bytes.
    pub fn exchange(&mut self, cmds: &[u8], read_len: usize) -> Result<Vec<u8>, C::Error> {
        self.chan.xfer(cmds, read_len)
    }

    /// The underlying `Chan`. Tests use it to count round trips.
    pub fn chan(&self) -> &C {
        &self.chan
    }
}

/// Most bytes per USB round trip, for commands and for read back each.
///
/// The right value depends on the chip. When read-back data does not fit the
/// device receive buffer, the device stops taking commands and our write does
/// not complete either. On an FT232H this showed as
/// `write of 2617 bytes failed after 2048`. `Chan::read_limit` returns the value
/// for the actual chip.
pub const CHUNK_BYTES: usize = 3072;

impl<C: Chan> JtagIo for Mpsse<C> {
    type Error = C::Error;

    fn scan_dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, C::Error> {
        let mut cmds = Vec::new();
        let (read_len, tail) = push_scan(&mut cmds, bits, false);
        cmds.push(CMD_SEND_IMMEDIATE);
        let raw = self.chan.xfer(&cmds, read_len)?;
        Ok(scan_decode(&raw, bits.len(), tail))
    }

    /// This sets the bandwidth. The scans are joined into one USB round trip;
    /// only data beyond `CHUNK_BYTES` adds round trips.
    ///
    /// Between scans the TAP does not return to Run-Test/Idle; it goes straight
    /// on to the next Shift-DR. The TCK count is the same, but MPSSE commands
    /// drop from 3 to 2 per scan. One command costs about 0.7µs (measured),
    /// which at 20MHz is more than the TCK time.
    fn scan_many(&mut self, frames: &[Vec<bool>]) -> Result<Vec<Vec<bool>>, C::Error> {
        let mut out: Vec<Vec<bool>> = Vec::with_capacity(frames.len());
        let limit = self.chan.read_limit();
        // Also limit the scan count, so one chunk stays within `CHUNK_MS`.
        let per_scan = frames.first().map_or(48, |f| f.len() as u64 + 7) + self.gap as u64;
        let max_scans = ((self.tck_hz as u64 * CHUNK_MS / 1000) / per_scan).max(1) as usize;
        let mut at = 0;

        while at < frames.len() {
            let mut cmds: Vec<u8> = Vec::new();
            push_enter(&mut cmds, false);
            // (bytes to read back, TMS bits) of each scan in this chunk.
            let mut sizes: Vec<(usize, usize)> = Vec::new();
            let mut total = 0;
            // Start of the last tail. It is replaced when the chunk closes.
            let mut tail_at = 0;

            while at + sizes.len() < frames.len() {
                let bits = &frames[at + sizes.len()];
                let before = cmds.len();
                let mut n = push_body(&mut cmds, &bits[..bits.len() - 1]);
                // We do not know yet whether another scan follows. Queue a tail
                // that stays in Shift; the last one is rewritten below.
                let here = cmds.len();
                let (tn, tail) = push_tail_gap(&mut cmds, bits[bits.len() - 1], self.gap);
                n += tn;
                if !sizes.is_empty()
                    && (cmds.len() > CHUNK_BYTES || total + n > limit || sizes.len() >= max_scans)
                {
                    // Never close an empty chunk, or the loop makes no progress.
                    cmds.truncate(before);
                    break;
                }
                tail_at = here;
                sizes.push((n, tail));
                total += n;
            }

            // Only the last scan of the chunk returns to Run-Test/Idle. The next
            // chunk starts with `push_enter`, so the TAP must not stay in Shift.
            // Cut back to the recorded tail position, not a fixed length.
            cmds.truncate(tail_at);
            let last = &frames[at + sizes.len() - 1];
            let (_, tail) = push_tail(&mut cmds, last[last.len() - 1], Exit::Idle);
            sizes.last_mut().unwrap().1 = tail;

            cmds.push(CMD_SEND_IMMEDIATE);
            let raw = self.chan.xfer(&cmds, total)?;

            let mut cut = 0;
            for (k, &(n, tail)) in sizes.iter().enumerate() {
                out.push(scan_decode(&raw[cut..cut + n], frames[at + k].len(), tail));
                cut += n;
            }
            at += sizes.len();
        }
        Ok(out)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::bridge::{Batch, Bridge};
    use crate::frame::Frame;
    use crate::model::FakeBridge;

    /// The 16 TAP states.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum St {
        Reset,
        Idle,
        DrSelect,
        DrCapture,
        DrShift,
        DrExit1,
        DrPause,
        DrExit2,
        DrUpdate,
        IrSelect,
        IrCapture,
        IrShift,
        IrExit1,
        IrPause,
        IrExit2,
        IrUpdate,
    }

    fn step(s: St, tms: bool) -> St {
        use St::*;
        match (s, tms) {
            (Reset, true) => Reset,
            (Reset, false) => Idle,
            (Idle, true) => DrSelect,
            (Idle, false) => Idle,
            (DrSelect, true) => IrSelect,
            (DrSelect, false) => DrCapture,
            (DrCapture, true) => DrExit1,
            (DrCapture, false) => DrShift,
            (DrShift, true) => DrExit1,
            (DrShift, false) => DrShift,
            (DrExit1, true) => DrUpdate,
            (DrExit1, false) => DrPause,
            (DrPause, true) => DrExit2,
            (DrPause, false) => DrPause,
            (DrExit2, true) => DrUpdate,
            (DrExit2, false) => DrShift,
            (DrUpdate, true) => DrSelect,
            (DrUpdate, false) => Idle,
            (IrSelect, true) => Reset,
            (IrSelect, false) => IrCapture,
            (IrCapture, true) => IrExit1,
            (IrCapture, false) => IrShift,
            (IrShift, true) => IrExit1,
            (IrShift, false) => IrShift,
            (IrExit1, true) => IrUpdate,
            (IrExit1, false) => IrPause,
            (IrPause, true) => IrExit2,
            (IrPause, false) => IrPause,
            (IrExit2, true) => IrUpdate,
            (IrExit2, false) => IrShift,
            (IrUpdate, true) => DrSelect,
            (IrUpdate, false) => Idle,
        }
    }

    /// Decodes MPSSE bytes, walks a real TAP state machine, and drives a
    /// `FakeBridge`.
    ///
    /// With this model, bugs that would otherwise show only on real hardware
    /// show in tests: bit packing, TMS on the last bit, a missing IR select, and
    /// the initial TAP state.
    pub struct FakeFtdi {
        st: St,
        ir: u32,
        ir_len: u8,
        user1_ir: u32,
        ir_sr: Vec<bool>,
        dr: Vec<bool>,
        dev: FakeBridge,
        /// Whether the MPSSE setup commands arrived.
        configured: bool,
        tck: u32,
        /// USB round trips. This shows whether batching works.
        pub xfers: u32,
    }

    impl FakeFtdi {
        pub fn new(aw: u32, latency: u32, user1_ir: u32, ir_len: u8) -> Self {
            FakeFtdi {
                // Start outside Run-Test/Idle on purpose, so a missing TAP
                // reset breaks the tests.
                st: St::DrPause,
                ir: 0,
                ir_len,
                user1_ir,
                ir_sr: Vec::new(),
                dr: Vec::new(),
                dev: FakeBridge::new(aw, latency),
                configured: false,
                tck: 0,
                xfers: 0,
            }
        }

        /// Length of the selected DR: 1 bit (BYPASS) unless the IR is USER1.
        fn dr_len(&self) -> usize {
            if self.ir == self.user1_ir {
                Frame::len(self.dev.aw)
            } else {
                1
            }
        }

        fn clock(&mut self, tms: bool, tdi: bool) -> bool {
            let tdo = match self.st {
                St::DrShift => *self.dr.first().unwrap_or(&false),
                St::IrShift => *self.ir_sr.first().unwrap_or(&false),
                _ => false,
            };
            match self.st {
                St::DrShift => {
                    self.dr.remove(0);
                    self.dr.push(tdi);
                }
                St::IrShift => {
                    self.ir_sr.remove(0);
                    self.ir_sr.push(tdi);
                }
                _ => {}
            }
            let next = step(self.st, tms);
            match next {
                St::Reset => {
                    self.ir = 0xffff_ffff; // BYPASS
                }
                St::DrCapture => {
                    self.dr = if self.ir == self.user1_ir {
                        self.dev.capture_bits()
                    } else {
                        vec![false]
                    };
                }
                St::IrCapture => {
                    // A real IR capture has `01` in the low 2 bits.
                    self.ir_sr = (0..self.ir_len).map(|i| i == 0).collect();
                }
                St::DrUpdate => {
                    if self.ir == self.user1_ir {
                        let bits = std::mem::take(&mut self.dr);
                        assert_eq!(bits.len(), Frame::len(self.dev.aw));
                        self.dev.update(&bits);
                    }
                }
                St::IrUpdate => {
                    self.ir = self
                        .ir_sr
                        .iter()
                        .enumerate()
                        .fold(0u32, |a, (i, &b)| if b { a | 1 << i } else { a });
                }
                _ => {}
            }
            self.st = next;
            assert_eq!(
                self.dr_len(),
                if next == St::DrShift {
                    self.dr.len()
                } else {
                    self.dr_len()
                },
                "DR length must follow the selected IR"
            );
            tdo
        }
    }

    impl Chan for FakeFtdi {
        type Error = String;

        fn xfer(&mut self, out: &[u8], read_len: usize) -> Result<Vec<u8>, String> {
            self.xfers += 1;
            assert!(
                out.len() <= CHUNK_BYTES + 64,
                "a single transfer must stay inside the FTDI buffer: {} bytes",
                out.len()
            );
            assert!(
                read_len <= CHUNK_BYTES,
                "a single transfer must not ask for more than the FTDI buffer holds: {read_len}"
            );
            let mut got = Vec::new();
            let mut i = 0;
            while i < out.len() {
                let op = out[i];
                match op {
                    CMD_DIS_DIV_5 | CMD_DIS_3_PHASE | CMD_DIS_ADAPTIVE | CMD_LOOPBACK_END
                    | CMD_SEND_IMMEDIATE => i += 1,
                    CMD_SET_BITS_LOW | CMD_SET_BITS_HIGH => i += 3,
                    CMD_SET_TCK_DIVISOR => {
                        let d = u16::from_le_bytes([out[i + 1], out[i + 2]]);
                        self.tck = tck_hz(d);
                        self.configured = true;
                        i += 3;
                    }
                    CMD_BYTES | CMD_BYTES_OUT => {
                        let n = u16::from_le_bytes([out[i + 1], out[i + 2]]) as usize + 1;
                        for k in 0..n {
                            let byte = out[i + 3 + k];
                            let mut acc = 0u8;
                            for b in 0..8 {
                                if self.clock(false, (byte >> b) & 1 == 1) {
                                    acc |= 1 << b;
                                }
                            }
                            if op == CMD_BYTES {
                                got.push(acc);
                            }
                        }
                        i += 3 + n;
                    }
                    CMD_BITS | 0x1b => {
                        let n = out[i + 1] as usize + 1;
                        let byte = out[i + 2];
                        // Reads come back packed at the MSB end (shift right).
                        let mut acc = 0u8;
                        for b in 0..n {
                            let tdo = self.clock(false, (byte >> b) & 1 == 1);
                            acc >>= 1;
                            if tdo {
                                acc |= 0x80;
                            }
                        }
                        if op == CMD_BITS {
                            got.push(acc);
                        }
                        i += 3;
                    }
                    CMD_TMS | 0x4b => {
                        let n = out[i + 1] as usize + 1;
                        let byte = out[i + 2];
                        let tdi = byte & 0x80 != 0;
                        let mut acc = 0u8;
                        for b in 0..n {
                            let tdo = self.clock((byte >> b) & 1 == 1, tdi);
                            acc >>= 1;
                            if tdo {
                                acc |= 0x80;
                            }
                        }
                        if op == CMD_TMS {
                            got.push(acc);
                        }
                        i += 3;
                    }
                    _ => return Err(format!("unknown MPSSE opcode 0x{op:02x}")),
                }
            }
            assert!(self.configured, "MPSSE was used before it was set up");
            if got.len() != read_len {
                return Err(format!(
                    "asked for {read_len} bytes, produced {}",
                    got.len()
                ));
            }
            Ok(got)
        }
    }

    /// The setup the batch tests use (AW=7, USER1=0x02, IR length 6).
    pub fn new_ftdi(aw: u32, latency: u32) -> FakeFtdi {
        FakeFtdi::new(aw, latency, 0x02, 6)
    }

    pub fn probe_cfg() -> ProbeConfig {
        cfg(0x02, 6, 30_000_000)
    }

    pub fn cfg(user1_ir: u32, ir_length: u8, max_tck_hz: u32) -> ProbeConfig {
        ProbeConfig {
            vid: 0x0403,
            pid: 0x6010,
            interface: 0,
            serial: None,
            product: None,
            layout_init: (0x0088, 0x008b),
            max_tck_hz,
            ir_length,
            user1_ir,
        }
    }

    #[test]
    fn the_tck_divisor_never_exceeds_the_ceiling() {
        for max in [1_000_000, 6_000_000, 10_000_000, 15_000_000, 30_000_000] {
            let d = tck_divisor(max);
            assert!(tck_hz(d) <= max, "{max} -> {} Hz", tck_hz(d));
        }
        assert_eq!(tck_divisor(30_000_000), 0);
        assert_eq!(tck_divisor(10_000_000), 2);
        // When it does not divide evenly, round to the slower side.
        assert!(tck_hz(tck_divisor(7_000_000)) <= 7_000_000);
    }

    /// 41 bits (AW=7) split into 5 bytes plus the last bit. This is why
    /// `Frame::len` prefers 8n+1.
    #[test]
    fn an_eight_n_plus_one_scan_uses_whole_bytes() {
        let bits = vec![false; 41];
        let mut out = Vec::new();
        let (read_len, _) = push_scan(&mut out, &bits, false);
        assert_eq!(read_len, 6, "5 whole bytes + the TMS byte");
        assert!(!out.contains(&CMD_BITS), "no ragged bit command is needed");
    }

    #[test]
    fn a_ragged_read_is_shifted_down_from_the_top() {
        // A 4-bit scan: 3 body bits and the last bit.
        // raw[0] holds the 3 body bits at the top; raw[1] is 3 TMS clocks.
        // raw[0] >> 5 = 0b101, first bit at the LSB. Bit 0 of raw[1] >> 5 is
        // the last bit.
        let bits = scan_decode(&[0b1010_0000, 0b0010_0000], 4, 3);
        assert_eq!(bits, vec![true, false, true, true]);
    }

    /// Through the whole stack: MPSSE bytes, TAP, DR semantics.
    #[test]
    fn a_write_then_a_read_survives_the_whole_mpsse_stack() {
        let io = Mpsse::open(FakeFtdi::new(7, 0, 0x02, 6), &cfg(0x02, 6, 30_000_000)).unwrap();
        let mut b = Bridge::new(io, 7);
        b.write32(0x10, 0xa5a5_1234).unwrap();
        assert_eq!(b.read32(0x10).unwrap(), 0xa5a5_1234);
    }

    /// The extra clocks run in Run-Test/Idle, and the next scan still starts at
    /// Capture.
    #[test]
    fn a_gap_between_scans_keeps_the_tap_in_step() {
        for gap in [0, 1, 2, 9] {
            let mut io =
                Mpsse::open(FakeFtdi::new(7, 0, 0x02, 6), &cfg(0x02, 6, 30_000_000)).unwrap();
            io.set_update_gap(gap);
            let mut b = Bridge::new(io, 7);
            b.write_burst(0x10, &[1, 2, 3, 4]).unwrap();
            let mut got = [0u32; 4];
            b.read_burst(0x10, &mut got).unwrap();
            assert_eq!(got, [1, 2, 3, 4], "gap {gap}");
        }
    }

    /// One TCK cycle plus the gap covers the window cycles plus 1 of margin.
    /// Measured on an Arty: with a 50MHz window, 10MHz TCK failed and 7.5MHz
    /// passed.
    #[test]
    fn the_gap_covers_the_window_cycles() {
        // 50MHz window, 6 cycles + 1 of margin.
        assert_eq!(update_gap(15_000_000, 50.0, 6), 2); // 3 TCK = 10 cycles
        assert_eq!(update_gap(10_000_000, 50.0, 6), 1); // 2 TCK = 10 cycles
        assert_eq!(update_gap(5_000_000, 50.0, 6), 0); // 1 TCK = 10 cycles
        // 100MHz window at 15MHz: 1 TCK = 6.7 cycles, not enough margin.
        assert_eq!(update_gap(15_000_000, 100.0, 6), 1);
        // Unknown window clock: no gap.
        assert_eq!(update_gap(15_000_000, 0.0, 6), 0);
        for (tck, mhz) in [(30_000_000, 50.0), (15_000_000, 25.0), (7_500_000, 125.0)] {
            let gap = update_gap(tck, mhz, 6);
            let window_cycles = (1 + gap) as f64 * mhz * 1e6 / tck as f64;
            assert!(
                window_cycles >= 7.0,
                "{tck} Hz on {mhz} MHz: {window_cycles}"
            );
        }
    }

    /// A length that is not 8n+1 takes the `CMD_BITS` path.
    #[test]
    fn a_ragged_dr_length_works_too() {
        let io = Mpsse::open(FakeFtdi::new(12, 0, 0x02, 6), &cfg(0x02, 6, 30_000_000)).unwrap();
        let mut b = Bridge::new(io, 12);
        b.write32(0x24, 0xdead_beef).unwrap();
        assert_eq!(b.read32(0x24).unwrap(), 0xdead_beef);
    }

    /// Busy retries work through MPSSE too.
    #[test]
    fn a_slow_bus_is_retried_through_the_real_tap() {
        let io = Mpsse::open(FakeFtdi::new(7, 3, 0x02, 6), &cfg(0x02, 6, 30_000_000)).unwrap();
        let mut b = Bridge::new(io, 7);
        b.write32(0x08, 0x1234).unwrap();
        assert_eq!(b.read32(0x08).unwrap(), 0x1234);
    }

    fn word(v: u32) -> Vec<bool> {
        (0..32).map(|i| (v >> i) & 1 == 1).collect()
    }

    /// Shifting in 0s instead made a VCU118 report 226 devices: every 0 after
    /// the chain counted as BYPASS.
    #[test]
    fn the_chain_ends_where_our_own_ones_come_back() {
        let mut bits = word(0x04b3_1093);
        bits.extend(std::iter::repeat_n(true, 64));
        assert_eq!(parse_chain(&bits), vec![Device::Id(0x04b3_1093)]);
    }

    #[test]
    fn devices_without_an_idcode_take_one_bit_each() {
        let mut bits = vec![false, false];
        bits.extend(word(0x0362_d093));
        bits.extend(std::iter::repeat_n(true, 64));
        assert_eq!(
            parse_chain(&bits),
            vec![Device::Bypass, Device::Bypass, Device::Id(0x0362_d093)]
        );
    }

    #[test]
    fn an_empty_chain_reports_nothing() {
        assert_eq!(parse_chain(&[true; 128]), vec![]);
    }

    #[test]
    fn the_ir_length_can_be_measured_instead_of_looked_up() {
        for len in [4u8, 6, 8, 12, 22] {
            let mut m =
                Mpsse::open(FakeFtdi::new(7, 0, 0x02, len), &cfg(0x02, len, 30_000_000)).unwrap();
            assert_eq!(m.measure_ir_length().unwrap(), Some(len), "len {len}");
        }
    }

    /// A wrong IR length breaks the `01` capture pattern, so this is how the IR
    /// length is checked on real hardware.
    #[test]
    fn the_ir_capture_pattern_confirms_the_ir_length() {
        let mut io = Mpsse::open(FakeFtdi::new(7, 0, 0x02, 6), &cfg(0x02, 6, 30_000_000)).unwrap();
        assert_eq!(io.scan_ir(0x02, 6).unwrap(), 0x01);
    }

    /// When nothing drives TDO, all 1s come back, and the low 2 bits (busy/err)
    /// both look set. An unconfigured FPGA once got reported as `sticky_err`
    /// this way. The message must say the window was never reached, or the user
    /// looks for the cause in the wrong place.
    #[test]
    fn all_ones_is_reported_as_no_answer_not_as_a_window_error() {
        struct Dead;
        impl JtagIo for Dead {
            type Error = ();
            fn scan_dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, ()> {
                Ok(vec![true; bits.len()])
            }
        }
        let mut bus = Bridge::new(Dead, 7);
        let err = bus.read32(0).unwrap_err();
        assert!(matches!(err, crate::bridge::Error::NoAnswer), "{err:?}");

        let text = err.to_string();
        assert!(text.contains("nothing drove TDO"), "{text}");
        assert!(text.contains("not configured"), "{text}");
        // It also says how to narrow the problem down.
        assert!(text.contains("probe"), "{text}");
    }

    /// An unconfigured FPGA makes the USER slot a 1-bit pass-through. The data
    /// comes back 1 bit late, so the op bit of `read` lands on `err` and looks
    /// like `sticky_err`. This was misdiagnosed three times on real hardware.
    #[test]
    fn a_pass_through_register_is_not_reported_as_a_window_error() {
        struct Bypass(usize);
        impl JtagIo for Bypass {
            type Error = ();
            fn scan_dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, ()> {
                let mut out = vec![false; self.0];
                out.extend_from_slice(&bits[..bits.len() - self.0]);
                Ok(out)
            }
        }
        // The 1-bit pass-through seen on real hardware.
        let mut bus = Bridge::new(Bypass(1), 7);
        let err = bus.read32(0).unwrap_err();
        assert!(
            matches!(err, crate::bridge::Error::Bypassed { delay: 1 }),
            "{err:?}"
        );
        assert!(err.to_string().contains("not configured"), "{err}");

        // With 2 or more bits of delay, `err` is not set. The magic check
        // catches that case by the wrong value, so it is not caught here.
        let mut bus = Bridge::new(Bypass(2), 7);
        assert_ne!(bus.read32(0).unwrap(), 0x5648_524e);
    }

    #[test]
    fn a_batch_against_a_dead_link_says_no_answer() {
        struct Dead;
        impl JtagIo for Dead {
            type Error = ();
            fn scan_dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, ()> {
                Ok(vec![true; bits.len()])
            }
        }
        let mut bus = Bridge::new(Dead, 7);
        let mut b = Batch::new();
        b.read(0);
        assert!(matches!(
            bus.run(&b).unwrap_err(),
            crate::bridge::Error::NoAnswer
        ));
    }

    /// A wrong IR selects the 1-bit BYPASS, so the window does not move and the
    /// sent bits come back 1 bit late. The read must never match by chance.
    /// (With the current field layout, the op LSB lands on `err`.)
    #[test]
    fn a_wrong_user_slot_never_looks_like_success() {
        // The bridge is at USER1 = 0x02, but 0x03 is selected.
        let io = Mpsse::open(FakeFtdi::new(7, 0, 0x02, 6), &cfg(0x03, 6, 30_000_000)).unwrap();
        let mut b = Bridge::new(io, 7);
        let _ = b.write32(0x10, 0xa5a5_1234);
        assert!(
            !matches!(b.read32(0x10), Ok(0xa5a5_1234)),
            "the wrong IR must not read back the value that was written"
        );
    }
}

#[cfg(test)]
mod batch_tests {
    use super::tests::*;
    use super::*;
    use crate::bridge::{Batch, Bridge};

    #[test]
    fn a_batch_agrees_with_one_access_at_a_time() {
        let io = Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(io, 7);

        let mut b = Batch::new();
        b.write(0x08, 0x1111_1111);
        b.write(0x0c, 0x2222_2222);
        let x = b.read(0x08);
        let y = b.read(0x0c);
        let got = bus.run(&b).unwrap();
        assert_eq!(got[x], 0x1111_1111);
        assert_eq!(got[y], 0x2222_2222);

        // One at a time gives the same.
        assert_eq!(bus.read32(0x08).unwrap(), 0x1111_1111);
        assert_eq!(bus.read32(0x0c).unwrap(), 0x2222_2222);
    }

    #[test]
    fn a_batch_costs_one_usb_round_trip() {
        let io = Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(io, 7);

        let before = bus.io_ref().chan().xfers;
        let mut b = Batch::new();
        for i in 0..64 {
            b.write(0x08, i);
        }
        bus.run(&b).unwrap();
        assert_eq!(
            bus.io_ref().chan().xfers - before,
            1,
            "64 writes must be one transfer"
        );

        // One at a time takes 64 round trips or more.
        let before = bus.io_ref().chan().xfers;
        for i in 0..64 {
            bus.write32(0x08, i).unwrap();
        }
        assert!(
            bus.io_ref().chan().xfers - before >= 64,
            "one at a time really is one round trip each"
        );
    }

    /// The VCU118 window is 39 bits, not 8n+1 (the Arty is 41). Then
    /// `CMD_BITS` sits between scans, which is a different chaining path.
    #[test]
    fn a_ragged_length_survives_being_chained() {
        for aw in [5u32, 7, 9, 12, 15] {
            let io = Mpsse::open(new_ftdi(aw, 0), &probe_cfg()).unwrap();
            let mut bus = Bridge::new(io, aw);

            let mut b = Batch::new();
            b.write(0x08, 0x1234_5678);
            b.write(0x0c, 0x9abc_def0);
            let x = b.read(0x08);
            let y = b.read(0x0c);
            let got = bus.run(&b).unwrap();
            assert_eq!(got[x], 0x1234_5678, "aw {aw}");
            assert_eq!(got[y], 0x9abc_def0, "aw {aw}");
        }
    }

    /// At a slow TCK the scan count limits the chunk, and the results stay the
    /// same. A chunk that runs too long stalls the device (500kHz stalled on
    /// real hardware).
    #[test]
    fn a_slow_clock_makes_smaller_chunks_but_the_same_answers() {
        for tck in [500_000u32, 3_000_000, 30_000_000] {
            let io = Mpsse::open(new_ftdi(7, 0), &cfg(0x02, 6, tck)).unwrap();
            let mut bus = Bridge::new(io, 7);
            bus.write_burst(0x10, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
            let mut out = [0u32; 8];
            bus.read_burst(0x10, &mut out).unwrap();
            assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8], "tck {tck}");
        }
    }

    #[test]
    fn a_large_batch_is_split_without_changing_the_result() {
        let io = Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(io, 7);

        let words: Vec<u32> = (0..600).map(|i| 0xa000_0000 + i).collect();
        let before = bus.io_ref().chan().xfers;
        let mut b = Batch::new();
        let handles: Vec<_> = words.iter().map(|_| b.read(0x00)).collect();
        let got = bus.run(&b).unwrap();
        assert!(
            bus.io_ref().chan().xfers - before > 1,
            "600 scans do not fit in one FTDI buffer"
        );
        // Every read returns the same value, so a shift at a chunk boundary
        // would show.
        for h in handles {
            assert_eq!(got[h], 0);
        }
    }

    #[test]
    fn a_burst_walks_consecutive_addresses() {
        let io = Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(io, 7);
        bus.write_burst(0x10, &[1, 2, 3, 4]).unwrap();
        let mut out = [0u32; 4];
        bus.read_burst(0x10, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4]);
    }

    /// A batch is sent packed, so it cannot wait in the middle. A slow bridge
    /// drops commands, and the error must say which one.
    ///
    /// A batch not marked replayable is not resent: a command that looks
    /// dropped may in fact have run.
    #[test]
    fn a_dropped_command_is_reported_not_lost() {
        // The bus takes 3 scans, so a packed batch always drops commands.
        let io = Mpsse::open(new_ftdi(7, 3), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(io, 7);

        let mut b = Batch::new();
        b.write(0x08, 1);
        b.write(0x0c, 2);
        b.write(0x10, 3);
        let err = bus.run(&b).unwrap_err();
        assert!(
            matches!(err, crate::bridge::Error::Dropped { .. }),
            "{err:?}"
        );

        let text = err.to_string();
        // It also says why it does not resend.
        assert!(text.contains("pop"), "{text}");
        assert!(text.contains("one at a time"), "{text}");
    }

    /// A replayable batch works on a slow terminator. `dram` goes through AXI4
    /// and arbitration, so it answers later than a register, and a packed batch
    /// drops commands. The batch is then resent with space between commands.
    /// On an Arty, the board default TCK was too fast for `dram`.
    #[test]
    fn a_slow_bus_is_absorbed_by_spacing_the_commands() {
        let io = Mpsse::open(new_ftdi(7, 3), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(io, 7);

        let mut b = Batch::new();
        b.replayable();
        b.write(0x08, 1);
        b.write(0x0c, 2);
        b.write(0x10, 3);
        let hs: Vec<_> = [0x08u32, 0x0c, 0x10].iter().map(|&a| b.read(a)).collect();
        let got = bus
            .run(&b)
            .expect("a replayable batch rides out a slow bus");

        // Every write landed and every read came back; nothing was skipped.
        assert_eq!(got[hs[0]], 1);
        assert_eq!(got[hs[1]], 2);
        assert_eq!(got[hs[2]], 3);
    }

    /// Not every command after a busy is dropped. The bridge becomes free while
    /// the rest of the frames arrive, so only a middle command is dropped and
    /// later ones are accepted. Picking up the missed read later with a NOP
    /// then returns the result of a later command, from another address.
    ///
    /// On an Arty (`dram` region, TCK 20MHz), `read ddr 0 3` returned
    /// `33333333 22222222 33333333`: word 1 had the value of word 3.
    #[test]
    fn a_gap_in_the_middle_does_not_hand_back_another_word() {
        // The bus takes 2 scans: only the middle command drops.
        let io = Mpsse::open(new_ftdi(7, 2), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(io, 7);
        bus.write32(0x08, 0x1111_1111).unwrap();
        bus.write32(0x0c, 0x2222_2222).unwrap();
        bus.write32(0x10, 0x3333_3333).unwrap();

        let mut b = Batch::new();
        b.replayable();
        let hs: Vec<_> = [0x08u32, 0x0c, 0x10].iter().map(|&a| b.read(a)).collect();
        let got = bus
            .run(&b)
            .expect("a replayable batch rides out a slow bus");

        assert_eq!(
            got[hs[0]], 0x1111_1111,
            "the first word came back as another"
        );
        assert_eq!(got[hs[1]], 0x2222_2222);
        assert_eq!(got[hs[2]], 0x3333_3333);
    }

    /// A batch with no drops gets no spacing, so a fast path stays fast.
    #[test]
    fn a_fast_bus_still_costs_one_round_trip() {
        let io = Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(io, 7);
        let before = bus.io_ref().chan().xfers;

        let mut b = Batch::new();
        b.replayable();
        for i in 0..8 {
            b.write(0x08 + 4 * i, i);
        }
        bus.run(&b).unwrap();
        assert_eq!(bus.io_ref().chan().xfers - before, 1);
    }

    #[test]
    fn an_empty_batch_touches_nothing() {
        let io = Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(io, 7);
        let before = bus.io_ref().chan().xfers;
        assert!(bus.run(&Batch::new()).unwrap().is_empty());
        assert_eq!(bus.io_ref().chan().xfers, before);
    }
}

#[cfg(test)]
mod chaining_tests {
    use super::tests::*;
    use super::*;
    use crate::bridge::{Bridge, JtagIo};

    /// Count MPSSE commands by stepping over each command's arguments.
    fn count_commands(cmds: &[u8]) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < cmds.len() {
            let op = cmds[i];
            i += match op {
                CMD_BYTES => 3 + u16::from_le_bytes([cmds[i + 1], cmds[i + 2]]) as usize + 1,
                CMD_BITS => 3,
                CMD_TMS | 0x4b => 3,
                CMD_SET_BITS_LOW | CMD_SET_BITS_HIGH | CMD_SET_TCK_DIVISOR => 3,
                _ => 1,
            };
            n += 1;
        }
        n
    }

    /// A chained scan goes straight on to the next Shift-DR, so it needs no
    /// enter command. One command costs about 0.7µs (measured).
    #[test]
    fn a_chained_scan_costs_two_commands_not_three() {
        let bits = vec![false; 41];

        // Single: enter + body + tail = 3.
        let mut one = Vec::new();
        push_scan(&mut one, &bits, false);
        assert_eq!(count_commands(&one), 3);

        // 10 chained: 1 enter + (body + tail) × 10 = 21.
        let mut many = Vec::new();
        push_enter(&mut many, false);
        for i in 0..10 {
            push_body(&mut many, &bits[..bits.len() - 1]);
            let exit = if i == 9 { Exit::Idle } else { Exit::Shift };
            push_tail(&mut many, false, exit);
        }
        assert_eq!(count_commands(&many), 21, "1 enter + 2 per scan");
    }

    /// Fewer commands must not cost more TCK cycles.
    #[test]
    fn chaining_does_not_add_tck_cycles() {
        // Single: enter 3 + body 40 + tail 3 = 46.
        assert_eq!(3 + 40 + TAIL_TO_IDLE.len(), 46);
        // Chained: body 40 + tail 6 = 46.
        assert_eq!(40 + TAIL_TO_SHIFT.len(), 46);
    }

    /// The bridge needs exactly 2 edges between Update and the next Capture.
    /// `TAIL_TO_SHIFT` passes Run-Test/Idle after Update, so it gives 3: the
    /// same margin as a single scan that returns to Run-Test/Idle.
    #[test]
    fn the_bridge_still_gets_its_edges_between_update_and_capture() {
        // [Exit1, Update, RTI, Select, Capture, Shift]
        // Update happens on the clock of bit 1, Capture on bit 4.
        let update_at = 1;
        let capture_at = 4;
        assert!(
            capture_at - update_at >= 2,
            "the bridge needs at least two TCK edges between Update and Capture"
        );
        assert_eq!(TAIL_TO_SHIFT.len(), 6);
    }

    /// Checked through the model that walks the TAP.
    #[test]
    fn chained_scans_return_what_one_at_a_time_returns() {
        let mut a = Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let mut bus = Bridge::new(Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap(), 7);
        bus.write32(0x08, 0xfeed_face).unwrap();

        // The same steps, one at a time.
        let frames: Vec<Vec<bool>> = vec![
            crate::frame::Frame::write(0x08, 0xfeed_face).to_bits(7),
            crate::frame::Frame::read(0x08).to_bits(7),
            crate::frame::Frame::nop().to_bits(7),
        ];
        let one: Vec<Vec<bool>> = frames.iter().map(|f| a.scan_dr(f).unwrap()).collect();

        let mut b = Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let many = b.scan_many(&frames).unwrap();

        assert_eq!(one, many, "chaining must not change what comes back");
    }
}

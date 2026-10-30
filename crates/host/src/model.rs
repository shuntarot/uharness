//! A model of `rtl/hns/src/dr.veryl`, for tests only.
//!
//! It has the same semantics as the RTL: Capture returns the previous result,
//! and an Update while busy is dropped. `FakeBridge::v2` is the current bridge:
//! the drop is decided at Capture, it keeps dropping until a resync, and `err`
//! belongs to the last command. `FakeBridge::new` is the older one, still on
//! boards: the drop is decided at Update, so a command that looked dropped can
//! run, and `err` is sticky. This lets tests run the protocol without a board.
//!
//! `capture` and `update` are separate because the `mpsse` tests walk the TAP and
//! shift one bit at a time between Capture-DR and Update-DR. The model also
//! implements `JtagIo`, which does a whole DR scan in one call.

use crate::bridge::JtagIo;
use crate::frame::{Frame, Op, Response};
use std::collections::{HashMap, HashSet};

pub struct FakeBridge {
    pub aw: u32,
    mem: HashMap<u32, u32>,
    /// Scans the bus needs before it answers. 0 means it answers at once.
    latency_scans: u32,
    pending: Option<(Frame, u32)>,
    result: Response,
    /// The current bridge, not the older one.
    v2: bool,
    /// A command was dropped and nothing has resynced since (`v2` only).
    dropping: bool,
    /// What the last Capture showed as busy (`v2` only).
    stall_cap: bool,
    /// Addresses the window answers with an error.
    failing: HashSet<u32>,
    /// Commands that ran, in order. Tests check that a dropped one did not.
    pub ran: Vec<Frame>,
    /// Number of DR scans. Tests use it to count pipeline entries.
    pub scans: u32,
}

impl FakeBridge {
    /// The older bridge.
    pub fn new(aw: u32, latency_scans: u32) -> Self {
        FakeBridge {
            aw,
            mem: HashMap::new(),
            latency_scans,
            pending: None,
            result: Response {
                busy: false,
                err: false,
                rdata: 0,
                v2: false,
            },
            v2: false,
            dropping: false,
            stall_cap: false,
            failing: HashSet::new(),
            ran: Vec::new(),
            scans: 0,
        }
    }

    /// The current bridge.
    pub fn v2(aw: u32, latency_scans: u32) -> Self {
        let mut b = Self::new(aw, latency_scans);
        b.v2 = true;
        b.result.v2 = true;
        b
    }

    /// The window answers accesses to `addr` with an error.
    pub fn fail_at(mut self, addr: u32) -> Self {
        self.failing.insert(addr);
        self
    }

    /// Capture-DR. Returns the current result and does not change any state.
    pub fn capture(&self) -> Response {
        let mut r = self.result;
        r.busy |= self.dropping;
        r
    }

    /// The Capture value as bits, LSB first. It also latches what Capture
    /// showed, which the current bridge decides the Update by.
    pub fn capture_bits(&mut self) -> Vec<bool> {
        let c = self.capture();
        self.stall_cap = c.busy;
        let mut out = vec![false; Frame::len(self.aw)];
        out[0] = c.busy;
        out[1] = c.err;
        for i in 0..32 {
            out[2 + i] = (c.rdata >> i) & 1 == 1;
        }
        if self.aw > 0 {
            out[34] = c.v2;
        }
        out
    }

    /// Update-DR. It first advances the pending bus transaction by one step, then
    /// takes the new command.
    pub fn update(&mut self, bits: &[bool]) {
        self.scans += 1;
        assert_eq!(bits.len(), Frame::len(self.aw), "DR length must match");

        if let Some((frame, left)) = self.pending.take() {
            if left <= 1 {
                self.settle(frame);
                self.result.busy = false;
            } else {
                self.pending = Some((frame, left - 1));
            }
        }

        let op = take(bits, 0, 2);
        if self.v2 && op == Op::Resync as u32 {
            self.dropping = false;
            return;
        }
        if op == Op::Nop as u32 {
            return;
        }
        let frame = if op == Op::Write as u32 {
            Frame::write(take(bits, 34, self.aw as usize), take(bits, 2, 32))
        } else {
            // The older bridge reads on `11` too.
            Frame::read(take(bits, 34, self.aw as usize))
        };

        // The current bridge decides by what Capture showed; the older one by
        // whether it is busy now, which may have changed during the shift.
        let dropped = if self.v2 {
            self.stall_cap
        } else {
            self.result.busy
        };
        if dropped {
            if self.v2 {
                self.dropping = true;
            }
            return;
        }
        if self.v2 {
            self.result.err = false;
        }
        if self.latency_scans == 0 {
            self.settle(frame);
        } else {
            self.pending = Some((frame, self.latency_scans));
            self.result.busy = true;
        }
    }

    fn settle(&mut self, frame: Frame) {
        self.ran.push(frame);
        if self.failing.contains(&frame.addr) {
            self.result.err = true;
            if frame.op == Op::Read {
                self.result.rdata = 0;
            }
            return;
        }
        match frame.op {
            Op::Write => {
                self.mem.insert(frame.addr, frame.wdata);
            }
            Op::Read => {
                self.result.rdata = *self.mem.get(&frame.addr).unwrap_or(&0);
            }
            Op::Nop | Op::Resync => {}
        }
    }
}

/// Reads `n` bits from `bits` starting at `at`, LSB first.
fn take(bits: &[bool], at: usize, n: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..n {
        if bits[at + i] {
            v |= 1 << i;
        }
    }
    v
}

impl JtagIo for FakeBridge {
    type Error = ();

    /// One whole DR scan. Capture must come before Update.
    fn scan_dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, ()> {
        let captured = self.capture_bits();
        self.update(bits);
        Ok(captured)
    }
}

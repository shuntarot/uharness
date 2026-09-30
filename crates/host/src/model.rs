//! A model of `rtl/hns/src/dr.veryl`, for tests only.
//!
//! It has the same semantics as the RTL: Capture returns the previous result, an
//! Update while busy is silently dropped, and `err` is sticky. This lets tests run
//! the protocol without a board.
//!
//! `capture` and `update` are separate because the `mpsse` tests walk the TAP and
//! shift one bit at a time between Capture-DR and Update-DR. The model also
//! implements `JtagIo`, which does a whole DR scan in one call.

use crate::bridge::JtagIo;
use crate::frame::{Frame, Op, Response};
use std::collections::HashMap;

pub struct FakeBridge {
    pub aw: u32,
    mem: HashMap<u32, u32>,
    /// Scans the bus needs before it answers. 0 means it answers at once.
    latency_scans: u32,
    pending: Option<(Frame, u32)>,
    result: Response,
    /// Number of DR scans. Tests use it to count pipeline entries.
    pub scans: u32,
}

impl FakeBridge {
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
            },
            scans: 0,
        }
    }

    /// Capture-DR. Returns the current result and does not change any state.
    pub fn capture(&self) -> Response {
        self.result
    }

    /// The Capture value as bits, LSB first.
    pub fn capture_bits(&self) -> Vec<bool> {
        let c = self.capture();
        let mut out = vec![false; Frame::len(self.aw)];
        out[0] = c.busy;
        out[1] = c.err;
        for i in 0..32 {
            out[2 + i] = (c.rdata >> i) & 1 == 1;
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
        if op == 0 {
            return;
        }
        let frame = if op == 1 {
            Frame::read(take(bits, 34, self.aw as usize))
        } else {
            Frame::write(take(bits, 34, self.aw as usize), take(bits, 2, 32))
        };

        if self.result.busy {
            // Dropped without setting the sticky error, as in the RTL.
            // The host sees busy in Capture and sends the command again.
        } else if self.latency_scans == 0 {
            self.settle(frame);
        } else {
            self.pending = Some((frame, self.latency_scans));
            self.result.busy = true;
        }
    }

    fn settle(&mut self, frame: Frame) {
        match frame.op {
            Op::Write => {
                self.mem.insert(frame.addr, frame.wdata);
            }
            Op::Read => {
                self.result.rdata = *self.mem.get(&frame.addr).unwrap_or(&0);
            }
            Op::Nop => {}
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

//! Accessing the window. Everything below the DR scan is behind `JtagIo`.

use crate::frame::{Frame, Response};

/// The JTAG layer. An implementation only has to shift the DR once.
///
/// How the TAP is walked is up to the implementation (for MPSSE, a command
/// list). Here one operation means: enter Capture-DR, send `bits`, collect TDO,
/// and leave through Update-DR.
///
/// Contract: between Update and the next Capture, TCK must run at least 2
/// edges. The bridge brings `ack` into TCK through a 2-FF synchronizer, and TCK
/// stops while the host sends nothing, so with fewer edges `busy` is stale.
/// A real TAP walks Exit1-DR → Update-DR → Select-DR-Scan → Capture-DR, which
/// is 3 edges or more, so a plain implementation meets this.
pub trait JtagIo {
    type Error;

    /// Shift the DR once. Returns the value loaded at Capture, which is the
    /// result of the previous command.
    fn scan_dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, Self::Error>;

    /// Shift the DR many times. Returns the same as calling `scan_dr` for each.
    ///
    /// The default does one at a time. Only an implementation that can send
    /// them together overrides it; MPSSE joins the commands into one USB round
    /// trip. This sets the bandwidth: measured 0.8ms per transaction one at a
    /// time, 1.4µs batched.
    fn scan_many(&mut self, frames: &[Vec<bool>]) -> Result<Vec<Vec<bool>>, Self::Error> {
        frames.iter().map(|bits| self.scan_dr(bits)).collect()
    }
}

#[derive(Debug)]
pub enum Error<E> {
    Io(E),
    /// The bridge kept returning busy.
    Busy {
        addr: u32,
        tries: u32,
    },
    /// sticky_err is set. It stays set until the bridge is reset. Only an
    /// older bridge (`Response::v2` not set) has it.
    Sticky {
        addr: u32,
    },
    /// The window answered this access with an error. The bridge keeps
    /// working; commands after it in the same batch have run.
    Failed {
        index: usize,
        addr: u32,
    },
    /// The bridge was busy in the middle of a batch, and this command was
    /// dropped. Only an older bridge: it decides at Update, but the host only
    /// sees busy from the Capture before it, so a command reported as dropped
    /// may have run. That is why only harness memory is resent there.
    Dropped {
        index: usize,
        addr: u32,
    },
    /// The DR came back as all ones. Nothing answered.
    NoAnswer,
    /// What came back is what was sent, a few bits later. The scan reached a
    /// short pass-through register, not the window. On an unconfigured FPGA the
    /// USER slot is a 1-bit register, so this looks like an answer, not like
    /// no answer.
    Bypassed {
        delay: usize,
    },
}

impl<E: std::fmt::Debug> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "jtag io failed: {e:?}"),
            Error::Busy { addr, tries } => write!(
                f,
                "the bridge stayed busy for address 0x{addr:x} after {tries} scans.\n\
                 The window answers every access within its timeout, so something behind \
                 it is stalling longer than that, or TCK is too fast. Try a lower --tck-hz."
            ),
            Error::Failed { index, addr } => write!(
                f,
                "the window answered an error for 0x{addr:x} (command {index} of the batch).\n\
                 The address is outside the window, the write was partial, or what is behind \
                 it did not answer within the window's timeout (`harness_timeout` counts \
                 those). A DUT slave that takes longer needs `timeout` on its bundle in \
                 Harness.toml. The window still works; later commands of the batch have run."
            ),
            Error::Dropped { index, addr } => write!(
                f,
                "command {index} of the batch (address 0x{addr:x}) was dropped: the bridge \
                 was still busy.\n\
                 Harness memory (`bram`, `dram`) is already retried with wider spacing, and \
                 that was not enough. Other accesses are not retried: the command may have run \
                 anyway, and a `pop` read run twice takes two FIFO entries.\n\
                 Lower TCK (`--tck-hz`), or issue these accesses one at a time."
            ),
            Error::NoAnswer => write!(
                f,
                "the scan came back as all ones: nothing drove TDO.\n\
                 This is not an error from the window. There was no answer at all. The \
                 usual causes, in order: the FPGA is not configured (program it), the level \
                 shifters are off (`--layout-init`), or the USER slot is wrong \
                 (`--user1-ir`). `hio probe` reads the IDCODE even on an unconfigured FPGA, \
                 so it tells these apart."
            ),
            Error::Bypassed { delay } => write!(
                f,
                "the scan came back as what was sent, {delay} bit(s) later.\n\
                 This is a {delay}-bit pass-through register, not the window. The FPGA is \
                 not configured, or the USER slot is wrong and reaches BYPASS. `hio probe` \
                 cannot tell these apart. Program the FPGA and try again."
            ),
            Error::Sticky { addr } => write!(
                f,
                "the bridge reported sticky_err while accessing 0x{addr:x}.\n\
                 The window answered with an error (address outside it, or a partial write), \
                 or a command was dropped because another was in flight.\n\
                 Only reconfiguring the FPGA clears it. If you see it right after programming, \
                 the old bitstream is still running. Run `erase` alone: if the window still \
                 answers after it, JPROGRAM does not work as the target says."
            ),
        }
    }
}

/// All ones means no answer, not an error answer.
///
/// When nothing drives TDO, all bits are 1, so `busy` and `err` both look set.
/// Read as a response, this reports `sticky_err` as if the window returned an
/// error, but the scan never reached the window. This happens on an
/// unconfigured FPGA.
fn check_answered<E>(bits: &[bool]) -> Result<(), Error<E>> {
    if bits.iter().all(|&b| b) {
        return Err(Error::NoAnswer);
    }
    Ok(())
}

/// Check whether what was sent came back a few bits late.
///
/// A pass-through register (BYPASS, or the USER slot of an unconfigured FPGA)
/// returns TDI delayed by a few bits. Read as a response, `err` or `busy` looks
/// set and gets reported as a window error, but the window was never reached.
///
/// This runs only when `busy` or `err` looks set: it explains a false error.
/// Checking every response gives false hits when a write response happens to
/// match the delayed bits.
///
/// If the bits before the delayed copy are not all zero, this reports nothing.
/// A pass-through register first shifts out its own content (zero), so a set
/// bit there means the window answered. Without this rule, runs of zeros
/// matched each other and a real answer was called a bypass:
///
/// ```text
/// sent 1000000000000000000000000000000000001010000
/// got  1000000110000000000000000000000000000000000   busy=1, rdata=0x60
/// ```
///
/// This is a valid `busy` response. With `rdata` in 0x40..0x7F, an 8-bit shift
/// matches: 64 in 2^32, so once in about 67M words. A full memory test on real
/// hardware failed near 95% every time because of this, and stopped where a
/// resend would have worked.
///
/// A pass-through of 2 bits or more is not caught here, since neither `busy`
/// nor `err` is set. The magic check catches it.
fn check_bypassed<E>(sent: &[bool], got: &[bool]) -> Result<(), Error<E>> {
    let r = Response::from_bits(got);
    if !r.busy && !r.err {
        return Ok(());
    }
    if !sent.iter().any(|&b| b) {
        return Ok(());
    }
    for delay in 1..=8usize {
        if delay >= got.len() {
            break;
        }
        // A pass-through first shifts out its content, which is zero. A set bit
        // here means the window answered, and a match is only zeros lining up.
        if got[..delay].iter().any(|&b| b) {
            continue;
        }
        if got[delay..] == sent[..got.len() - delay] {
            return Err(Error::Bypassed { delay });
        }
    }
    Ok(())
}

/// How many scans to wait on busy. Each scan is one USB round trip, which is
/// long; a bus still stuck after this will not recover by waiting longer.
const BUSY_TRIES: u32 = 8;

/// Attempts for a batch that dropped commands. Only for a replayable batch
/// (`Batch::replayable`).
const BATCH_TRIES: u32 = 6;

/// Most NOPs to put after each command on a resend.
///
/// Commands drop because the window has not answered between Update and the
/// next Capture, so extra scans in between fix it. This needs far fewer round
/// trips than sending one at a time.
const MAX_PAD: usize = 16;

/// Accesses the window over JTAG.
pub struct Bridge<J: JtagIo> {
    io: J,
    aw: u32,
    /// The bridge says it is the current one (`Response::v2`). Learned from
    /// every scan, so a reprogrammed device with the other kind is followed.
    v2: bool,
}

impl<J: JtagIo> Bridge<J> {
    pub fn new(io: J, aw: u32) -> Self {
        Bridge { io, aw, v2: false }
    }

    /// Send one command. Returns what that scan captured: the previous result.
    fn scan(&mut self, frame: Frame) -> Result<Response, Error<J::Error>> {
        let bits = frame.to_bits(self.aw);
        let got = self.io.scan_dr(&bits).map_err(Error::Io)?;
        check_answered(&got)?;
        check_bypassed(&bits, &got)?;
        let r = Response::from_bits(&got);
        self.v2 = r.v2;
        Ok(r)
    }

    /// Scan until the bridge is free. Returns the Capture at that point, which
    /// holds the result of the last command that ran.
    ///
    /// Do not wait by resending the command. Capture comes before Update, so the
    /// scan right after an accepted command always looks busy. Resending would
    /// loop forever: accepted, looks busy, resent. A NOP does not change the
    /// bridge state, so it can wait. The current bridge also keeps showing busy
    /// after a drop until a resync, so it gets resyncs instead (a resync runs
    /// nothing either).
    fn wait_idle(&mut self, addr: u32) -> Result<Response, Error<J::Error>> {
        for _ in 0..BUSY_TRIES {
            let frame = if self.v2 {
                Frame::resync()
            } else {
                Frame::nop()
            };
            let r = self.scan(frame)?;
            if r.err && !r.v2 {
                return Err(Error::Sticky { addr });
            }
            if !r.busy {
                return Ok(r);
            }
        }
        Err(Error::Busy {
            addr,
            tries: BUSY_TRIES,
        })
    }

    /// Get one command accepted.
    ///
    /// The returned `busy` tells whether the Update just sent was accepted:
    /// Capture comes before Update, and busy cannot rise between them. When it
    /// was not accepted, wait until the bridge is free and send it again. On
    /// the older bridge a command that looked busy may have run anyway (busy
    /// can fall during the shift); the current one decides at Capture.
    fn send(&mut self, frame: Frame, addr: u32) -> Result<(), Error<J::Error>> {
        for _ in 0..BUSY_TRIES {
            let r = self.scan(frame)?;
            // The current bridge's `err` is about the command before, which
            // its caller has already collected.
            if r.err && !r.v2 {
                return Err(Error::Sticky { addr });
            }
            if !r.busy {
                return Ok(());
            }
            self.wait_idle(addr)?;
        }
        Err(Error::Busy {
            addr,
            tries: BUSY_TRIES,
        })
    }

    /// The current bridge waits for the write to finish and reports an error
    /// answer. The older one returns once the write is accepted, like a posted
    /// write: its errors stick and show at the next access.
    pub fn write32(&mut self, addr: u32, data: u32) -> Result<(), Error<J::Error>> {
        self.send(Frame::write(addr, data), addr)?;
        if self.v2 && self.wait_idle(addr)?.err {
            return Err(Error::Failed { index: 0, addr });
        }
        Ok(())
    }

    /// The result comes in the next scan (pipelined), so a read takes at least
    /// 2 scans.
    pub fn read32(&mut self, addr: u32) -> Result<u32, Error<J::Error>> {
        self.send(Frame::read(addr), addr)?;
        let r = self.wait_idle(addr)?;
        if r.err {
            return Err(Error::Failed { index: 0, addr });
        }
        Ok(r.rdata)
    }

    /// The underlying transport. Tests use it to count round trips.
    pub fn io_ref(&self) -> &J {
        &self.io
    }

    /// The underlying transport, for scans that are not window commands
    /// (programming the device through the probe the bridge holds).
    pub fn io_mut(&mut self) -> &mut J {
        &mut self.io
    }

    /// Changes the address width, for a new design whose window is a
    /// different size. The DR length follows it.
    pub fn set_addr_bits(&mut self, aw: u32) {
        self.aw = aw;
    }

    /// Write consecutive words.
    pub fn write_burst(&mut self, addr: u32, data: &[u32]) -> Result<(), Error<J::Error>> {
        let mut b = Batch::new();
        for (i, &w) in data.iter().enumerate() {
            b.write(addr + 4 * i as u32, w);
        }
        self.run(&b)?;
        Ok(())
    }

    /// Read consecutive words.
    pub fn read_burst(&mut self, addr: u32, out: &mut [u32]) -> Result<(), Error<J::Error>> {
        let mut b = Batch::new();
        let handles: Vec<ReadHandle> = (0..out.len())
            .map(|i| b.read(addr + 4 * i as u32))
            .collect();
        let got = self.run(&b)?;
        for (slot, h) in out.iter_mut().zip(handles) {
            *slot = got[h];
        }
        Ok(())
    }
}

/// Ticket for a read queued in a batch.
///
/// The value is available only from what `run()` returns, so code cannot read
/// a ticket that is not filled yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadHandle(usize);

/// Read results of a batch. Index it with a `ReadHandle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reads(Vec<u32>);

impl std::ops::Index<ReadHandle> for Reads {
    type Output = u32;
    fn index(&self, h: ReadHandle) -> &u32 {
        &self.0[h.0]
    }
}

impl Reads {
    /// Built by a transport from its read values, in order.
    pub(crate) fn new(values: Vec<u32>) -> Self {
        Reads(values)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The values in the order of the reads.
    pub fn values(&self) -> &[u32] {
        &self.0
    }
}

/// Accesses queued and sent together.
///
/// Nothing touches the transport until `run()`. Thousands of `write` / `read`
/// calls cost one round trip, unless the transport splits them into chunks.
///
/// ## Dropped commands
///
/// A batch sends commands packed, so it cannot wait on busy in the middle. The
/// bridge drops an Update while busy, so every response is checked.
///
/// The current bridge decides at Capture and keeps dropping until a resync, so
/// a busy response means that command and every later one did not run. They
/// are resent after a resync, whatever the batch is. A resend puts NOPs
/// between commands to give the window time to answer.
///
/// The older bridge accepts or drops at Update, but the host only sees busy
/// from the Capture before it, with a DR shift of TCK in between. So a command
/// that looks dropped may have run. For a register with a side effect such as
/// `pop`, a resend advances the FIFO twice and loses data. There, only a batch
/// marked `replayable` is resent: if running twice gives the same result, it
/// does not matter whether it ran.
///
/// A batch does not know the transport. It is only a list of "what to do at
/// which address": JTAG packs it into DR scans and handles resending, PCIe runs
/// it as loads and stores.
#[derive(Debug, Default, Clone)]
pub struct Batch {
    /// Queued commands. `addr` is kept for error messages.
    ops: Vec<(Frame, u32)>,
    /// Index into `ops` of each read.
    reads: Vec<usize>,
    /// Whether dropped commands may be resent to the older bridge. Set only
    /// when the caller knows the batch is idempotent.
    ///
    /// Only JTAG uses it; a BAR does not drop commands, and the current bridge
    /// drops only commands that did not run.
    replay: bool,
}

impl Batch {
    pub fn new() -> Self {
        Batch::default()
    }

    /// The queued commands, for a transport to run.
    pub fn ops(&self) -> &[(Frame, u32)] {
        &self.ops
    }

    /// Declare that running this batch twice gives the same result.
    ///
    /// Only for harness memory (`bram` / `dram` regions). Registers and DUT
    /// slave interfaces can include reads that change state.
    pub fn replayable(&mut self) -> &mut Self {
        self.replay = true;
        self
    }

    pub fn is_replayable(&self) -> bool {
        self.replay
    }

    pub fn write(&mut self, addr: u32, data: u32) -> &mut Self {
        self.ops.push((Frame::write(addr, data), addr));
        self
    }

    pub fn read(&mut self, addr: u32) -> ReadHandle {
        let h = ReadHandle(self.reads.len());
        self.reads.push(self.ops.len());
        self.ops.push((Frame::read(addr), addr));
        h
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

impl<J: JtagIo> Bridge<J> {
    /// Run a batch.
    ///
    /// A result comes in the scan after its command (pipelined), so one NOP is
    /// added at the end to receive the result of the last command.
    pub fn run(&mut self, batch: &Batch) -> Result<Reads, Error<J::Error>> {
        let Batch { ops, reads, replay } = batch;
        let (ops, reads, replay) = (ops.as_slice(), reads.as_slice(), *replay);
        if ops.is_empty() {
            return Ok(Reads(Vec::new()));
        }
        let aw = self.aw;
        // Index into ops -> index into the read results.
        let mut slot = vec![usize::MAX; ops.len()];
        for (s, &at) in reads.iter().enumerate() {
            slot[at] = s;
        }
        let mut values = vec![0u32; reads.len()];

        // On drops, resend with more spacing. The current bridge always makes
        // progress (the first command after a resync is taken), so it is not
        // bounded by attempts; the older one gives a batch that is not
        // replayable one attempt only.
        let mut next = 0usize;
        let mut pad = 0usize;
        let mut attempt = 0u32;
        loop {
            // `1 + pad` scans per command, and one NOP at the end for the
            // result of the last command.
            let stride = 1 + pad;
            let mut frames: Vec<Vec<bool>> = Vec::with_capacity((ops.len() - next) * stride + 1);
            for (frame, _) in &ops[next..] {
                frames.push(frame.to_bits(aw));
                for _ in 0..pad {
                    frames.push(Frame::nop().to_bits(aw));
                }
            }
            frames.push(Frame::nop().to_bits(aw));

            let got = self.io.scan_many(&frames).map_err(Error::Io)?;
            debug_assert_eq!(got.len(), frames.len());
            for (sent, bits) in frames.iter().zip(&got) {
                check_answered(bits)?;
                check_bypassed(sent, bits)?;
            }
            let resp: Vec<Response> = got.iter().map(|bits| Response::from_bits(bits)).collect();
            self.v2 = resp[0].v2;
            let rest = ops.len() - next;

            if self.v2 {
                // Busy at a command's own Capture means it was dropped, and so
                // was every later one: the bridge keeps dropping until a
                // resync. So the commands before the first busy all ran, in
                // order, and nothing after.
                let taken = (0..rest).find(|&k| resp[k * stride].busy).unwrap_or(rest);
                for k in 0..taken {
                    let at = next + k;
                    // The first free Capture after the command holds its
                    // result. The next command's own Capture is free when it
                    // was taken, so only the last one taken can lack a result;
                    // nothing ran after it, so wait for it.
                    let last = ((k + 1) * stride).min(resp.len() - 1);
                    let r = match (k * stride + 1..=last).find(|&p| !resp[p].busy) {
                        Some(p) => resp[p],
                        None => self.wait_idle(ops[at].1)?,
                    };
                    if r.err {
                        return Err(Error::Failed {
                            index: at,
                            addr: ops[at].1,
                        });
                    }
                    if slot[at] != usize::MAX {
                        values[slot[at]] = r.rdata;
                    }
                }
                next += taken;
                if next == ops.len() {
                    return Ok(Reads(values));
                }
                // A dropped command never ran, so it can be sent again, even a
                // `pop`. Resync first, and space the rest out more.
                self.wait_idle(ops[next].1)?;
                pad = if pad == 0 { 1 } else { (pad * 2).min(MAX_PAD) };
                attempt += 1;
                if attempt as usize > ops.len() + BATCH_TRIES as usize {
                    return Err(Error::Busy {
                        addr: ops[next].1,
                        tries: attempt,
                    });
                }
                continue;
            }

            for (k, r) in resp.iter().enumerate() {
                if r.err {
                    let at = (next + k / stride).min(ops.len() - 1);
                    return Err(Error::Sticky { addr: ops[at].1 });
                }
            }

            // How far the results are certain.
            //
            // The Capture of `resp[k*stride]` comes before the Update of
            // `ops[next+k]`, so busy=0 there means the command is accepted
            // (`hns::dr`). busy=1 means "maybe not accepted", not "surely
            // dropped", so only a replayable batch is resent. (This is the
            // older bridge; the current one is handled above.)
            //
            // Not every command after a busy is dropped. The bridge becomes
            // free while the rest of the frames arrive, so a middle command can
            // drop while later ones are accepted. So the batch is cut where the
            // last certain value is, and the rest is resent; running twice is
            // fine for a replayable batch.
            let mut taken = rest;
            for k in 0..rest {
                if resp[k * stride].busy {
                    taken = k;
                    break;
                }
                let at = next + k;
                if slot[at] == usize::MAX {
                    continue;
                }
                // The result comes in the next scan. The Capture of the next
                // command is also before its Update, so the scans up to it all
                // show this result.
                let last = ((k + 1) * stride).min(resp.len() - 1);
                match (k * stride + 1..=last).find(|&p| !resp[p].busy) {
                    Some(p) => values[slot[at]] = resp[p].rdata,
                    // No answer yet. Do not pick it up with a NOP now: the
                    // whole batch was sent, so the bridge may hold the result
                    // of a later command.
                    None => {
                        taken = k;
                        break;
                    }
                }
            }
            next += taken;
            if next == ops.len() {
                return Ok(Reads(values));
            }

            // The rest did not arrive. Without `replay`, do not resend.
            if !replay {
                return Err(Error::Dropped {
                    index: next,
                    addr: ops[next].1,
                });
            }
            // Add spacing. The scans the window needs cannot be measured, so
            // double the spacing until it works.
            pad = if pad == 0 { 1 } else { pad * 2 };
            if pad > MAX_PAD || attempt + 1 == BATCH_TRIES {
                return Err(Error::Dropped {
                    index: next,
                    addr: ops[next].1,
                });
            }
            // Wait until the bridge is free before resending. A command from the
            // last attempt may still be running, and would cause new drops.
            self.wait_idle(ops[next].1)?;
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FakeBridge;

    fn bits(s: &str) -> Vec<bool> {
        s.chars().map(|c| c == '1').collect()
    }

    /// Seen near 95% of every full DDR3 test on an Arty. The sent frame is a
    /// leading 1 and zeros, so zeros alone lined up for an 8-bit shift. It
    /// happens only for `rdata` in 0x40..0x7F: 64 in 2^32, once in about 67M
    /// words.
    #[test]
    fn a_busy_answer_that_happens_to_line_up_is_not_a_bypass() {
        let sent = bits("1000000000000000000000000000000000001010000");
        let got = bits("1000000110000000000000000000000000000000000");
        // The shift does match, so a match alone cannot decide.
        assert_eq!(got[8..], sent[..got.len() - 8]);
        // Still, it is not a bypass.
        let r: Result<(), Error<()>> = check_bypassed(&sent, &got);
        assert!(r.is_ok(), "{:?}", r.err());
    }

    /// A frame with real content that comes back delayed is proof of a bypass.
    #[test]
    fn a_real_bypass_is_still_reported() {
        let sent = bits("1011010011100101101001110010110100111001011");
        let mut got = vec![false];
        got.extend_from_slice(&sent[..sent.len() - 1]);
        // Read as a response, busy or err is set, so the check runs.
        assert!(Response::from_bits(&got).busy || Response::from_bits(&got).err);
        let r: Result<(), Error<()>> = check_bypassed(&sent, &got);
        assert!(matches!(r, Err(Error::Bypassed { delay: 1 })), "{r:?}");
    }

    #[test]
    fn a_write_then_a_read_returns_the_value() {
        let mut b = Bridge::new(FakeBridge::new(15, 0), 15);
        b.write32(0x10, 0xa5a5_1234).unwrap();
        assert_eq!(b.read32(0x10).unwrap(), 0xa5a5_1234);
    }

    /// The result comes at the next Capture, as in RISC-V DMI.
    #[test]
    fn a_read_costs_two_scans_because_the_result_is_pipelined() {
        let mut b = Bridge::new(FakeBridge::new(15, 0), 15);
        b.write32(0x20, 7).unwrap();
        let before = b.io.scans;
        b.read32(0x20).unwrap();
        assert_eq!(b.io.scans - before, 2);
    }

    #[test]
    fn a_busy_bridge_is_retried_until_it_accepts() {
        let mut b = Bridge::new(FakeBridge::new(15, 2), 15);
        b.write32(0x30, 0xbeef).unwrap();
        assert_eq!(b.read32(0x30).unwrap(), 0xbeef);
    }

    /// A write succeeds once the bridge accepts it, like a posted write. A
    /// stuck bus shows only at the next access.
    #[test]
    fn a_write_is_accepted_even_if_the_bus_never_answers() {
        let mut b = Bridge::new(FakeBridge::new(15, 1000), 15);
        assert!(b.write32(0x40, 1).is_ok());
    }

    /// A read waits for its result, so a stuck bus shows, and the message says
    /// what to look at.
    #[test]
    fn a_read_from_a_stuck_bus_reports_what_to_look_at() {
        let mut b = Bridge::new(FakeBridge::new(15, 1000), 15);
        let err = b.read32(0x40).unwrap_err();
        assert!(matches!(err, Error::Busy { .. }));
        let text = err.to_string();
        assert!(text.contains("within its timeout"), "{text}");
    }

    #[test]
    fn the_current_bridge_reads_back_a_write() {
        let mut b = Bridge::new(FakeBridge::v2(15, 0), 15);
        b.write32(0x10, 0xa5a5_1234).unwrap();
        assert_eq!(b.read32(0x10).unwrap(), 0xa5a5_1234);
    }

    /// An error answer fails that access only. The older bridge would stay
    /// failed until the FPGA is reconfigured.
    #[test]
    fn an_error_answer_fails_one_access_and_the_bridge_goes_on() {
        let mut b = Bridge::new(FakeBridge::v2(15, 2).fail_at(0x7), 15);
        b.write32(0x10, 5).unwrap();
        let err = b.read32(0x7).unwrap_err();
        assert!(matches!(err, Error::Failed { addr: 0x7, .. }), "{err:?}");
        assert!(err.to_string().contains("`timeout`"), "{err}");
        assert_eq!(b.read32(0x10).unwrap(), 5);
    }

    /// A slow window drops packed commands. The current bridge drops every
    /// one after the first until a resync, so a resend keeps the order and
    /// runs each command once, even in a batch that is not replayable.
    #[test]
    fn a_slow_window_runs_a_packed_batch_once_and_in_order() {
        let mut b = Bridge::new(FakeBridge::v2(15, 3), 15);
        let mut batch = Batch::new();
        for i in 0..6u32 {
            batch.write(0x40 + 4 * i, 100 + i);
        }
        let reads: Vec<ReadHandle> = (0..6u32).map(|i| batch.read(0x40 + 4 * i)).collect();
        let got = b.run(&batch).unwrap();
        for (i, h) in reads.into_iter().enumerate() {
            assert_eq!(got[h], 100 + i as u32);
        }
        let ran: Vec<(u32, u32)> = b.io.ran.iter().map(|f| (f.addr, f.wdata)).collect();
        let sent: Vec<(u32, u32)> = batch.ops().iter().map(|(f, _)| (f.addr, f.wdata)).collect();
        assert_eq!(ran, sent);
    }

    /// The error names the command, and the next batch works.
    #[test]
    fn a_failing_command_in_a_batch_is_named() {
        let mut b = Bridge::new(FakeBridge::v2(15, 1).fail_at(0x8), 15);
        let mut batch = Batch::new();
        batch.write(0x4, 1);
        batch.read(0x8);
        batch.write(0xc, 3);
        let err = b.run(&batch).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Failed {
                    index: 1,
                    addr: 0x8
                }
            ),
            "{err:?}"
        );

        let mut batch = Batch::new();
        let h = batch.read(0xc);
        assert_eq!(b.run(&batch).unwrap()[h], 3);
    }

    /// A bridge left dropping (a host that stopped mid-batch) is resynced.
    #[test]
    fn a_bridge_left_dropping_is_resynced() {
        let mut b = Bridge::new(FakeBridge::v2(15, 5), 15);
        let mut batch = Batch::new();
        batch.write(0x4, 1);
        batch.write(0x8, 2);
        // Send it raw, so nothing resyncs afterwards.
        let frames: Vec<Vec<bool>> = batch.ops().iter().map(|(f, _)| f.to_bits(15)).collect();
        b.io.scan_many(&frames).unwrap();
        assert_eq!(b.read32(0x4).unwrap(), 1);
        b.write32(0x8, 2).unwrap();
        assert_eq!(b.read32(0x8).unwrap(), 2);
    }
}

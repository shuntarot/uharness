//! `Chan` over FTDI: sends MPSSE bytes to the FTDI bulk endpoints.
//!
//! Only this layer knows USB. Nothing here knows JTAG; that is `mpsse.rs`.
//!
//! ## FTDI reads start with 2 status bytes
//!
//! Every IN packet starts with 2 modem status bytes. This layer strips them, as
//! the `Chan` contract says. If they stay, the bits above shift by 2 bytes and the
//! only symptom is wrong TDO data.
//!
//! ## Latency timer
//!
//! With the default 16 ms, every read back waits 16 ms. It is set to 1 ms.

use crate::mpsse::{Chan, ProbeConfig};
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Bulk, ControlOut, ControlType, Direction, In, Out, Recipient};
use nusb::{Device, Endpoint, Interface, MaybeFuture};
use std::time::{Duration, Instant};

// ── FTDI vendor requests ──
const SIO_RESET: u8 = 0x00;
const SIO_SET_LATENCY_TIMER: u8 = 0x09;
const SIO_SET_BITMODE: u8 = 0x0b;

const RESET_SIO: u16 = 0;
const RESET_PURGE_RX: u16 = 1;
const RESET_PURGE_TX: u16 = 2;

const BITMODE_RESET: u16 = 0x00;
const BITMODE_MPSSE: u16 = 0x02;

/// Down from the default 16 ms. Without it, every read back is slow.
const LATENCY_MS: u16 = 1;

/// The byte MPSSE returns for an invalid opcode.
const BAD_COMMAND: u8 = 0xfa;

#[derive(Debug)]
pub enum Error {
    /// No device matches.
    NotFound {
        vid: u16,
        pid: u16,
        product: Option<String>,
        serial: Option<String>,
    },
    /// Several devices match. The first one is not taken silently.
    Ambiguous {
        vid: u16,
        pid: u16,
        serials: Vec<String>,
    },
    /// No bulk endpoint pair.
    NoEndpoint {
        interface: u8,
    },
    /// MPSSE does not answer.
    NotMpsse {
        got: Vec<u8>,
    },
    /// A write sent fewer bytes than asked.
    Short {
        want: usize,
        got: usize,
    },
    /// More bytes came back than asked.
    Desync {
        want: usize,
        got: usize,
    },
    Timeout {
        want: usize,
        got: usize,
    },
    Usb(nusb::Error),
    Transfer(nusb::transfer::TransferError),
    /// A transfer stopped. It records the direction and the byte counts, because
    /// a bare `Transfer(Cancelled)` gives nothing to debug with.
    Stalled {
        writing: bool,
        bytes: usize,
        got: usize,
        source: nusb::transfer::TransferError,
    },
}

impl From<nusb::Error> for Error {
    fn from(e: nusb::Error) -> Self {
        Error::Usb(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound {
                vid,
                pid,
                product,
                serial,
            } => {
                write!(f, "no JTAG probe matched {vid:04x}:{pid:04x}")?;
                if let Some(p) = product {
                    write!(f, " with product string {p:?}")?;
                }
                if let Some(s) = serial {
                    write!(f, " with serial {s:?}")?;
                }
                write!(
                    f,
                    ".\nCheck that the board is powered and shows up in `lsusb`. \
                     The product string differs between Digilent products: an Arty reports \
                     \"Digilent USB Device\", and the HS1/HS2 report \"Digilent Adept USB \
                     Device\". Set it with `[jtag] product` in the target description."
                )
            }
            Error::Ambiguous { vid, pid, serials } => write!(
                f,
                "{} devices match {vid:04x}:{pid:04x}, so hio cannot choose.\n\
                 Serial numbers seen: {}.\n\
                 Pick one with --probe-serial, or with `[jtag] serial` in the target \
                 description.",
                serials.len(),
                serials.join(", ")
            ),
            Error::NoEndpoint { interface } => write!(
                f,
                "interface {interface} has no bulk IN/OUT endpoint pair.\n\
                 On an FT2232H, interface 0 (channel A) is MPSSE and interface 1 is the UART. \
                 Check `[jtag] interface` in the target description."
            ),
            Error::NotMpsse { got } => write!(
                f,
                "the device did not answer as an MPSSE engine (got {got:02x?}, expected \
                 [{BAD_COMMAND:02x}, aa]).\n\
                 The channel cannot do MPSSE, or another program holds it. Vivado's \
                 hw_server uses the same channel: stop it with `pkill hw_server` and retry."
            ),
            Error::Short { want, got } => write!(
                f,
                "the USB write was short: {got} of {want} bytes went out."
            ),
            Error::Desync { want, got } => write!(
                f,
                "the probe returned {got} bytes where {want} were expected.\n\
                 The host and the probe are out of step, so every later bit would be wrong."
            ),
            Error::Timeout { want, got } => write!(
                f,
                "timed out reading from the probe: {got} of {want} bytes arrived.\n\
                 On the first scan, this usually means the level shifters are off. \
                 Set `[jtag] layout_init`."
            ),
            Error::Usb(e) => write!(f, "usb error: {e}"),
            Error::Stalled {
                writing,
                bytes,
                got,
                source,
            } => write!(
                f,
                "the USB {} of {bytes} bytes failed after {got} ({source}).\n\
                 The probe stopped partway through a transfer. The host and the probe are \
                 now out of step, so the next access would read garbage.",
                if *writing { "write" } else { "read" }
            ),
            Error::Transfer(e) => write!(f, "usb transfer failed: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// PIDs of FTDI chips with MPSSE, only the ones seen so far. Other chips work
/// with an explicit `--pid`.
pub const MPSSE_PIDS: &[u16] = &[
    0x6010, // FT2232H / FT2232D
    0x6011, // FT4232H
    0x6014, // FT232H
];

pub const FTDI_VID: u16 = 0x0403;

/// A connected candidate device.
#[derive(Debug, Clone)]
pub struct Found {
    pub vid: u16,
    pub pid: u16,
    pub product: Option<String>,
    pub serial: Option<String>,
}

impl std::fmt::Display for Found {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vid, self.pid)?;
        if let Some(p) = &self.product {
            write!(f, " {p:?}")?;
        }
        if let Some(s) = &self.serial {
            write!(f, " serial {s}")?;
        }
        Ok(())
    }
}

/// Lists FTDI devices that probably have MPSSE.
///
/// This does not prove a JTAG connection. The same chip may serve a UART. Only
/// opening it and reading an IDCODE proves it.
pub fn candidates(vid: Option<u16>, pid: Option<u16>) -> Result<Vec<Found>, Error> {
    Ok(nusb::list_devices()
        .wait()?
        .filter(|d| match vid {
            Some(v) => d.vendor_id() == v,
            None => d.vendor_id() == FTDI_VID,
        })
        .filter(|d| match pid {
            Some(p) => d.product_id() == p,
            None => MPSSE_PIDS.contains(&d.product_id()),
        })
        .map(|d| Found {
            vid: d.vendor_id(),
            pid: d.product_id(),
            product: d.product_string().map(str::to_string),
            serial: d.serial_number().map(str::to_string),
        })
        .collect())
}

/// Size of the chip's receive buffer (device -> host).
///
/// These are datasheet values, and they match the board: on an FT232H, a read
/// back of 1206 bytes stalled the write at 2048 bytes, and 606 bytes worked. The
/// 1 kB buffer overflowed and MPSSE stopped.
///
/// An unknown chip gets the small value. That is only slower, which is better
/// than a stall.
fn rx_buffer(pid: u16) -> usize {
    match pid {
        0x6010 | 0x6011 => 4096, // FT2232H / FT4232H
        0x6014 => 1024,          // FT232H
        _ => 1024,
    }
}

/// One FTDI channel.
pub struct Ftdi {
    _dev: Device,
    /// Must be kept. Dropping the `Interface` releases the claim, and on Linux
    /// `ftdi_sio` attaches again. Today nusb endpoints hold a reference to the
    /// interface, but relying on that could break silently in a later nusb.
    _iface: Option<Interface>,
    ep_out: Endpoint<Bulk, Out>,
    ep_in: Endpoint<Bulk, In>,
    timeout: Duration,
    /// Maximum bytes to read back at once. Set from the chip's receive buffer.
    read_limit: usize,
}

impl Ftdi {
    /// Selects the device and puts it in MPSSE mode.
    ///
    /// Fails if several devices match. Taking the first one would start driving
    /// another board after a cable is moved.
    pub fn open(cfg: &ProbeConfig) -> Result<Self, Error> {
        let found: Vec<_> = nusb::list_devices()
            .wait()?
            .filter(|d| d.vendor_id() == cfg.vid && d.product_id() == cfg.pid)
            .filter(|d| match &cfg.product {
                Some(p) => d.product_string() == Some(p.as_str()),
                None => true,
            })
            .filter(|d| match &cfg.serial {
                Some(s) => d.serial_number() == Some(s.as_str()),
                None => true,
            })
            .collect();

        let info = match found.len() {
            0 => {
                return Err(Error::NotFound {
                    vid: cfg.vid,
                    pid: cfg.pid,
                    product: cfg.product.clone(),
                    serial: cfg.serial.clone(),
                });
            }
            1 => &found[0],
            _ => {
                return Err(Error::Ambiguous {
                    vid: cfg.vid,
                    pid: cfg.pid,
                    serials: found
                        .iter()
                        .map(|d| d.serial_number().unwrap_or("<none>").to_string())
                        .collect(),
                });
            }
        };

        let dev = info.open().wait()?;
        // On Linux `ftdi_sio` holds both channels, so detach it first. After
        // this, the `ttyUSB*` of channel A goes away, and channel B (UART) stays.
        let iface = dev.detach_and_claim_interface(cfg.interface).wait()?;

        let (out_addr, in_addr) = bulk_pair(&iface).ok_or(Error::NoEndpoint {
            interface: cfg.interface,
        })?;
        let ep_out = iface.endpoint::<Bulk, Out>(out_addr)?;
        let ep_in = iface.endpoint::<Bulk, In>(in_addr)?;

        let mut me = Ftdi {
            _dev: dev,
            _iface: None,
            ep_out,
            ep_in,
            timeout: Duration::from_secs(1),
            // Leave a margin. A full buffer can fill before the host drains it.
            read_limit: rx_buffer(info.product_id()) * 3 / 4,
        };
        me.setup(&iface, cfg.interface)?;
        me._iface = Some(iface);
        Ok(me)
    }

    fn setup(&mut self, iface: &Interface, interface: u8) -> Result<(), Error> {
        // The FTDI wIndex is the channel number (channel A = 1).
        let index = interface as u16 + 1;
        let ctl = |request: u8, value: u16| ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request,
            value,
            index,
            data: &[],
        };
        let send = |request: u8, value: u16| -> Result<(), Error> {
            iface
                .control_out(ctl(request, value), self.timeout)
                .wait()
                .map_err(Error::Transfer)
        };
        send(SIO_RESET, RESET_SIO)?;
        send(SIO_RESET, RESET_PURGE_RX)?;
        send(SIO_RESET, RESET_PURGE_TX)?;
        send(SIO_SET_LATENCY_TIMER, LATENCY_MS)?;
        // Reset the bit mode, then enter MPSSE. The low-byte mask is unused in
        // MPSSE; `SET_BITS_*` sets the directions.
        //
        // Purge again after the reset. If MPSSE stopped inside a command, the
        // leftover bytes break the next sync, and they clear only after the mode
        // is reset (on the board the sync read `got [ff]`).
        send(SIO_SET_BITMODE, BITMODE_RESET << 8)?;
        send(SIO_RESET, RESET_PURGE_RX)?;
        send(SIO_RESET, RESET_PURGE_TX)?;
        send(SIO_SET_BITMODE, BITMODE_MPSSE << 8)?;

        // Drain what is left before the sync check.
        //
        // Once the command stream is out of sync, MPSSE reads the data that
        // follows as commands and returns many `0xfa` (bad command) bytes. If
        // they are still there at the next open, a purge does not clear them and
        // `sync` fails (seen on a VCU118). Draining here lets a new open recover.
        self.drain();
        self.sync()
    }

    /// Discards pending input. Stops after several empty reads.
    fn drain(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut quiet = 0;
        while Instant::now() < deadline && quiet < 3 {
            match self.read_once() {
                Ok(got) if got.is_empty() => quiet += 1,
                Ok(_) => quiet = 0,
                Err(_) => break,
            }
        }
    }

    /// Checks that MPSSE answers. An invalid opcode returns `0xfa` and the
    /// opcode. Checking here rules out the mode when TDO later looks wrong.
    fn sync(&mut self) -> Result<(), Error> {
        // Retry: leftover bytes can make the first try fail.
        let mut last = Vec::new();
        for _ in 0..3 {
            match self.sync_once() {
                Ok(()) => return Ok(()),
                Err(Error::NotMpsse { got }) => {
                    last = got;
                    self.drain();
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::NotMpsse { got: last })
    }

    fn sync_once(&mut self) -> Result<(), Error> {
        self.write_all(&[0xaa])?;
        // The answer is not always the first 2 bytes: old bytes can survive a
        // purge. So search for it, or a working MPSSE is reported as dead.
        let mut seen: Vec<u8> = Vec::new();
        let deadline = Instant::now() + self.timeout;
        loop {
            seen.extend_from_slice(&self.read_once()?);
            if seen.windows(2).any(|w| w == [BAD_COMMAND, 0xaa]) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                // Keep the message short. Out of sync, MPSSE returns hundreds
                // of `0xfa` bytes.
                seen.truncate(16);
                return Err(Error::NotMpsse { got: seen });
            }
        }
    }

    /// One IN transfer, with the 2 modem status bytes stripped. May be empty.
    fn read_once(&mut self) -> Result<Vec<u8>, Error> {
        let mps = self.ep_in.max_packet_size();
        // An IN request length must be a multiple of the max packet size.
        let want = mps * READ_PACKETS;
        let buf = self.ep_in.allocate(want);
        let c = self.ep_in.transfer_blocking(buf, self.timeout);
        c.status.map_err(|source| Error::Stalled {
            writing: false,
            bytes: want,
            got: c.actual_len,
            source,
        })?;
        let n = c.actual_len.min(c.buffer.len());
        let mut out = Vec::with_capacity(n);
        for chunk in c.buffer[..n].chunks(mps) {
            if chunk.len() > 2 {
                out.extend_from_slice(&chunk[2..]);
            }
        }
        Ok(out)
    }

    fn write_all(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() {
            return Ok(());
        }
        let c = self
            .ep_out
            .transfer_blocking(Buffer::from(data), self.timeout);
        c.status.map_err(|source| Error::Stalled {
            writing: true,
            bytes: data.len(),
            got: c.actual_len,
            source,
        })?;
        if c.actual_len != data.len() {
            return Err(Error::Short {
                want: data.len(),
                got: c.actual_len,
            });
        }
        Ok(())
    }

    /// Queues IN transfers for `want` bytes before the write starts.
    ///
    /// If the host writes everything first, nothing drains the device while it
    /// runs the commands. Its read-back buffer fills, it stops taking commands,
    /// and the write never completes.
    ///
    /// Seen on the FT232H of a VCU118:
    ///
    /// ```text
    ///   TCK 5 MHz or more     -> works
    ///   TCK 3.75 MHz or less  -> write of 1668 bytes failed after 1536
    /// ```
    ///
    /// The commands are the same at every frequency, so the device is stalled;
    /// the framing is fine. With IN transfers queued first, the kernel drains
    /// in parallel with the write, and buffer size and speed no longer matter.
    fn queue_reads(&mut self, want: usize, per: usize) {
        let queue = (want.div_ceil(per) + 1).min(READ_QUEUE);
        let bufs: Vec<Buffer> = (0..queue).map(|_| self.ep_in.allocate(per)).collect();
        for b in bufs {
            self.ep_in.submit(b);
        }
    }

    /// Writes while reading.
    ///
    /// Each queued IN transfer is used up once per latency timer period, so a
    /// fixed queue lasts only a fixed time and fails at a slow enough TCK (a
    /// bigger queue only moved the limit from 3.75 MHz to 1 MHz on the board).
    /// So the write is split into pieces, and between pieces the finished IN
    /// transfers are collected and queued again. This works for any run time.
    fn write_while_reading(
        &mut self,
        out: &[u8],
        want: usize,
        mps: usize,
        per: usize,
    ) -> Result<Vec<u8>, Error> {
        let mut got: Vec<u8> = Vec::with_capacity(want);
        let mut at = 0;
        while at < out.len() {
            let n = (out.len() - at).min(WRITE_PIECE);
            self.write_all(&out[at..at + n])?;
            at += n;
            // Do not wait. Take only finished transfers and queue new ones.
            self.reap(&mut got, mps, per)?;
        }
        self.collect_into(got, want, mps, per)
    }

    /// Takes finished IN transfers and queues new ones. Does not wait.
    fn reap(&mut self, got: &mut Vec<u8>, mps: usize, per: usize) -> Result<(), Error> {
        while let Some(c) = self.ep_in.wait_next_complete(Duration::ZERO) {
            c.status.map_err(|source| Error::Stalled {
                writing: false,
                bytes: per,
                got: c.actual_len,
                source,
            })?;
            let n = c.actual_len.min(c.buffer.len());
            for chunk in c.buffer[..n].chunks(mps) {
                if chunk.len() > 2 {
                    got.extend_from_slice(&chunk[2..]);
                }
            }
            let buf = self.ep_in.allocate(per);
            self.ep_in.submit(buf);
        }
        Ok(())
    }

    /// Collects `want` bytes from the queued IN transfers.
    fn collect_into(
        &mut self,
        mut got: Vec<u8>,
        want: usize,
        mps: usize,
        per: usize,
    ) -> Result<Vec<u8>, Error> {
        let deadline = Instant::now() + self.timeout;

        while got.len() < want {
            let left = deadline.saturating_duration_since(Instant::now());
            let done = if left.is_zero() {
                None
            } else {
                self.ep_in.wait_next_complete(left)
            };
            let Some(c) = done else {
                return Err(Error::Timeout {
                    want,
                    got: got.len(),
                });
            };
            c.status.map_err(|source| Error::Stalled {
                writing: false,
                bytes: per,
                got: c.actual_len,
                source,
            })?;
            let n = c.actual_len.min(c.buffer.len());
            for chunk in c.buffer[..n].chunks(mps) {
                if chunk.len() > 2 {
                    got.extend_from_slice(&chunk[2..]);
                }
            }
            if got.len() < want {
                let buf = self.ep_in.allocate(per);
                self.ep_in.submit(buf);
            }
        }
        if got.len() > want {
            // Do not truncate. Extra bytes mean the read-back count is wrong,
            // and every later bit would be shifted.
            return Err(Error::Desync {
                want,
                got: got.len(),
            });
        }
        Ok(got)
    }

    /// Cancels queued IN transfers. Left over, they would mix into the next
    /// transfer and shift all its bits.
    fn flush_pending(&mut self) {
        self.ep_in.cancel_all();
        while self.ep_in.pending() > 0 {
            if self
                .ep_in
                .wait_next_complete(Duration::from_millis(500))
                .is_none()
            {
                break;
            }
        }
    }
}

/// Packets per IN transfer.
const READ_PACKETS: usize = 8;

/// IN transfers queued before a write.
const READ_QUEUE: usize = 8;

/// Write piece size. Finished reads are collected between pieces.
const WRITE_PIECE: usize = 512;

/// Finds the bulk (OUT, IN) endpoint pair in the interface descriptor.
///
/// Addresses are not hard-coded. Channel A of an FT2232H uses `0x02`/`0x81`, but
/// an FT4232H or a board design can differ.
fn bulk_pair(iface: &Interface) -> Option<(u8, u8)> {
    let desc = iface.descriptor()?;
    let mut out = None;
    let mut input = None;
    for ep in desc.endpoints() {
        if ep.transfer_type() != TransferType::Bulk {
            continue;
        }
        match ep.direction() {
            Direction::Out if out.is_none() => out = Some(ep.address()),
            Direction::In if input.is_none() => input = Some(ep.address()),
            _ => {}
        }
    }
    Some((out?, input?))
}

impl Chan for Ftdi {
    type Error = Error;

    fn read_limit(&self) -> usize {
        self.read_limit
    }

    fn xfer(&mut self, out: &[u8], read_len: usize) -> Result<Vec<u8>, Error> {
        if read_len == 0 {
            return self.write_all(out).map(|()| Vec::new());
        }
        let mps = self.ep_in.max_packet_size();
        let per = mps * READ_PACKETS;

        self.queue_reads(read_len, per);
        let result = self.write_while_reading(out, read_len, mps, per);
        self.flush_pending();
        result
    }
}

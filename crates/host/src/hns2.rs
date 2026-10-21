//! HNS2: the harness window over TCP.
//!
//! `veryl harness sim` serves the window of a simulated harness, and
//! `hio serve` the window of a board. `Link` is the client side, and `serve`
//! the server side of `hio serve`. The protocol is in `doc/hns2.md`.
//!
//! The simulation writes its address to `sim.addr` beside `regs.json`, so a
//! local `hio` finds it without being told.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bridge::{Batch, Reads};
use crate::frame::Op;

/// Beside `regs.json`.
pub const ADDR_FILE: &str = "sim.addr";

pub const GREETING: &[u8; 4] = b"HNS2";

pub const BATCH: u8 = 0x01;
pub const FINISH: u8 = 0x02;
pub const INFO: u8 = 0x80;
pub const READS: u8 = 0x81;
pub const ERROR: u8 = 0x82;

pub const OP_READ: u8 = 1;
pub const OP_WRITE: u8 = 2;

/// An op as it goes on the wire: `op`, `addr`, `data`.
pub type WireOp = (u8, u32, u32);

/// `flags` bit 0 of `BATCH`.
pub const REPLAYABLE: u8 = 1;

/// The `op` of an `ERROR` that no single op caused.
pub const NO_OP: u32 = u32::MAX;

/// The most ops `hio serve` takes in one `BATCH`. 9 MB of ops.
pub const MAX_OPS: u32 = 1 << 20;

/// What a server says about itself after the greeting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub server: String,
    /// `"sim"` or `"board"`.
    pub kind: String,
    pub target: Option<String>,
    /// `"jtag"` or `"pcie"`.
    pub master: String,
    pub map_hash: Option<u32>,
    pub max_ops: u32,
}

#[derive(Debug)]
pub enum Error {
    /// No `sim.addr`: the simulation is not running.
    NotRunning {
        addr_file: String,
    },
    /// Nothing answers at the address.
    NoAnswer {
        addr: String,
        source: std::io::Error,
    },
    Io(std::io::Error),
    /// The other end does not speak HNS2.
    Greeting {
        addr: String,
    },
    /// The server answered the greeting with `ERROR`, e.g. it is busy.
    Refused(String),
    /// A message that breaks the protocol.
    Protocol(String),
    /// The window answered with an AXI error.
    Resp {
        addr: u32,
        resp: u8,
        write: bool,
    },
    /// The server could not run a batch, for another reason.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotRunning { addr_file } => write!(
                f,
                "the simulation is not running ({addr_file} is missing).\n\
                 Start it with `veryl harness sim`, and keep it running. \
                 If it runs on another machine, pass `--connect <host:port>`."
            ),
            Error::NoAnswer { addr, source } => write!(
                f,
                "nothing answers at {addr} ({source}).\n\
                 Start the server again: `veryl harness sim` or `hio serve`."
            ),
            Error::Io(e) => write!(f, "the connection failed: {e}"),
            Error::Greeting { addr } => write!(
                f,
                "what answered at {addr} does not speak HNS2.\n\
                 A simulation generated before HNS2 speaks an older protocol. \
                 Generate it again with `veryl harness update`, and restart it."
            ),
            Error::Refused(message) => write!(f, "the server refused: {message}"),
            Error::Protocol(message) => write!(f, "the server broke the protocol: {message}"),
            Error::Resp { addr, resp, write } => write!(
                f,
                "{} 0x{addr:x} answered {}. Nothing is mapped there.",
                if *write { "a write to" } else { "a read of" },
                match resp {
                    2 => "SLVERR".to_string(),
                    3 => "DECERR".to_string(),
                    other => format!("response {other}"),
                }
            ),
            Error::Failed(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Sends one message, header and body in one write.
pub fn send_message(w: &mut impl Write, kind: u8, body: &[u8]) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(kind);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    w.write_all(&out)
}

/// Reads one message. A body longer than `max` is refused before it is read:
/// a stream that is not HNS2 would otherwise ask for gigabytes.
pub fn read_message(r: &mut impl Read, max: usize) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
    if len > max {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("a message of {len} bytes, more than {max}"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok((head[0], body))
}

/// The body of a `BATCH`.
pub fn encode_batch(flags: u8, ops: &[WireOp]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + ops.len() * 9);
    out.push(flags);
    out.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    for (op, addr, data) in ops {
        out.push(*op);
        out.extend_from_slice(&addr.to_le_bytes());
        out.extend_from_slice(&data.to_le_bytes());
    }
    out
}

/// Reads the body of a `BATCH`: the flags and the ops.
pub fn decode_batch(body: &[u8]) -> Result<(u8, Vec<WireOp>), String> {
    let (Some(&flags), Some(count)) = (body.first(), body.get(1..5)) else {
        return Err("a BATCH shorter than its flags and count".to_string());
    };
    let count = u32::from_le_bytes(count.try_into().unwrap()) as usize;
    if body.len() != 5 + count * 9 {
        return Err(format!(
            "a BATCH of {count} ops must be {} bytes, and it is {}",
            5 + count * 9,
            body.len()
        ));
    }
    let ops = body[5..]
        .as_chunks::<9>()
        .0
        .iter()
        .map(|b| {
            (
                b[0],
                u32::from_le_bytes(b[1..5].try_into().unwrap()),
                u32::from_le_bytes(b[5..9].try_into().unwrap()),
            )
        })
        .collect();
    Ok((flags, ops))
}

/// The body of `READS`.
pub fn encode_reads(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + values.len() * 4);
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn decode_reads(body: &[u8]) -> Result<Vec<u32>, String> {
    let Some(count) = body.get(..4) else {
        return Err("a READS shorter than its count".to_string());
    };
    let count = u32::from_le_bytes(count.try_into().unwrap()) as usize;
    if body.len() != 4 + count * 4 {
        return Err(format!(
            "a READS of {count} values must be {} bytes, and it is {}",
            4 + count * 4,
            body.len()
        ));
    }
    Ok(body[4..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect())
}

/// The body of `ERROR`.
pub fn encode_error(op: u32, resp: u8, message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + message.len());
    out.extend_from_slice(&op.to_le_bytes());
    out.push(resp);
    out.extend_from_slice(message.as_bytes());
    out
}

fn decode_error(body: &[u8]) -> Result<(u32, u8, String), String> {
    if body.len() < 5 {
        return Err("an ERROR shorter than its op and resp".to_string());
    }
    let op = u32::from_le_bytes(body[..4].try_into().unwrap());
    Ok((
        op,
        body[4],
        String::from_utf8_lossy(&body[5..]).into_owned(),
    ))
}

/// The largest message a client accepts: `READS` for a full batch, or an
/// `INFO` or `ERROR`, which are far smaller.
fn max_reply(max_ops: u32) -> usize {
    4 + 4 * max_ops as usize + 64 * 1024
}

pub struct Link {
    stream: TcpStream,
    info: Info,
}

impl Link {
    /// Connects to the simulation whose `sim.addr` sits in `dir`.
    pub fn open(dir: &Path) -> Result<Link, Error> {
        let file = dir.join(ADDR_FILE);
        let addr = std::fs::read_to_string(&file)
            .map_err(|_| Error::NotRunning {
                addr_file: file.display().to_string(),
            })?
            .trim()
            .to_string();
        Link::connect(&addr)
    }

    pub fn connect(addr: &str) -> Result<Link, Error> {
        let mut stream = TcpStream::connect(addr).map_err(|source| Error::NoAnswer {
            addr: addr.to_string(),
            source,
        })?;
        stream.set_nodelay(true)?;
        keep_alive(&stream)?;
        stream.write_all(GREETING)?;
        // An older server closes the connection, or answers something else.
        let mut greeting = [0u8; 4];
        if stream.read_exact(&mut greeting).is_err() || &greeting != GREETING {
            return Err(Error::Greeting {
                addr: addr.to_string(),
            });
        }
        let info = match read_message(&mut stream, max_reply(0))
            .map_err(|e| Error::Protocol(format!("no INFO after the greeting: {e}")))?
        {
            (INFO, body) => serde_json::from_slice(&body)
                .map_err(|e| Error::Protocol(format!("an INFO that is not valid: {e}")))?,
            (ERROR, body) => {
                let (_, _, message) = decode_error(&body).map_err(Error::Protocol)?;
                return Err(Error::Refused(message));
            }
            (kind, _) => {
                return Err(Error::Protocol(format!(
                    "message {kind:#04x} after the greeting, not INFO"
                )));
            }
        };
        Ok(Link { stream, info })
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    /// Runs the batch, in one round trip unless it has more than `max_ops`.
    pub fn run(&mut self, batch: &Batch) -> Result<Reads, Error> {
        let ops: Vec<WireOp> = batch
            .ops()
            .iter()
            .filter_map(|(frame, _)| match frame.op {
                Op::Read => Some((OP_READ, frame.addr, 0)),
                Op::Write => Some((OP_WRITE, frame.addr, frame.wdata)),
                Op::Nop => None,
            })
            .collect();
        let flags = if batch.is_replayable() { REPLAYABLE } else { 0 };
        let mut values = Vec::new();
        for chunk in ops.chunks(self.info.max_ops.max(1) as usize) {
            values.extend(self.exchange(flags, chunk)?);
        }
        Ok(Reads::new(values))
    }

    pub fn read32(&mut self, addr: u32) -> Result<u32, Error> {
        let mut out = [0];
        self.read_burst(addr, &mut out)?;
        Ok(out[0])
    }

    /// Reads consecutive words in one round trip.
    pub fn read_burst(&mut self, addr: u32, out: &mut [u32]) -> Result<(), Error> {
        let mut batch = Batch::new();
        for i in 0..out.len() {
            batch.read(addr + 4 * i as u32);
        }
        let reads = self.run(&batch)?;
        out.copy_from_slice(reads.values());
        Ok(())
    }

    /// Ends the simulation, as Ctrl-C on `veryl harness sim` would.
    pub fn finish(mut self) -> Result<(), Error> {
        send_message(&mut self.stream, FINISH, &[])?;
        self.reply(&[]).map(|_| ())
    }

    fn exchange(&mut self, flags: u8, ops: &[WireOp]) -> Result<Vec<u32>, Error> {
        send_message(&mut self.stream, BATCH, &encode_batch(flags, ops))?;
        let values = self.reply(ops)?;
        let reads = ops.iter().filter(|(op, _, _)| *op == OP_READ).count();
        if values.len() != reads {
            return Err(Error::Protocol(format!(
                "{} values for {reads} reads",
                values.len()
            )));
        }
        Ok(values)
    }

    /// Waits for the answer to what was just sent.
    fn reply(&mut self, ops: &[WireOp]) -> Result<Vec<u32>, Error> {
        match read_message(&mut self.stream, max_reply(self.info.max_ops))? {
            (READS, body) => decode_reads(&body).map_err(Error::Protocol),
            (ERROR, body) => {
                let (op, resp, message) = decode_error(&body).map_err(Error::Protocol)?;
                match ops.get(op as usize) {
                    Some(&(kind, addr, _)) if resp != 0 => Err(Error::Resp {
                        addr,
                        resp,
                        write: kind == OP_WRITE,
                    }),
                    _ => Err(Error::Failed(message)),
                }
            }
            (kind, _) => Err(Error::Protocol(format!(
                "message {kind:#04x} as an answer, not READS or ERROR"
            ))),
        }
    }
}

/// Serves the window to one client at a time, for good.
///
/// `run` runs a batch on the window; its error becomes an `ERROR` with no op.
/// A second client is refused while one is served: a command made of several
/// batches would break if another ran batches in between. `log` gets one line
/// per client.
pub fn serve(
    listener: TcpListener,
    info: &Info,
    mut run: impl FnMut(&Batch) -> Result<Reads, String>,
    log: impl Fn(&str),
) -> std::io::Result<()> {
    let busy = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<TcpStream>();
    let held = busy.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (held, tx) = (held.clone(), tx.clone());
            std::thread::spawn(move || take_turn(stream, &held, &tx));
        }
    });
    let info_body = serde_json::to_vec(info).expect("Info is plain data");
    for mut stream in rx {
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "a client".to_string());
        log(&format!("{peer} connected"));
        let end = serve_one(&mut stream, &info_body, info.max_ops, &mut run);
        log(&match end {
            Ok(()) => format!("{peer} left"),
            Err(e) => format!("{peer} dropped: {e}"),
        });
        busy.store(false, Ordering::Release);
    }
    Ok(())
}

fn serve_one(
    stream: &mut TcpStream,
    info: &[u8],
    max_ops: u32,
    run: &mut impl FnMut(&Batch) -> Result<Reads, String>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    keep_alive(stream)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut greeting = [0u8; 4];
    stream.read_exact(&mut greeting)?;
    if &greeting != GREETING {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "it did not greet with HNS2",
        ));
    }
    stream.set_read_timeout(None)?;
    stream.write_all(GREETING)?;
    send_message(stream, INFO, info)?;
    loop {
        let (kind, body) = match read_message(stream, 5 + 9 * max_ops as usize) {
            Ok(message) => message,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => {
                let _ = send_message(stream, ERROR, &encode_error(NO_OP, 0, &e.to_string()));
                return Err(e);
            }
        };
        let answer = match kind {
            BATCH => run_batch(&body, run),
            FINISH => Err((
                NO_OP,
                "this is a board, and FINISH ends only a simulation".to_string(),
            )),
            other => {
                let message = format!("message {other:#04x} is not one this server knows");
                send_message(stream, ERROR, &encode_error(NO_OP, 0, &message))?;
                return Err(std::io::Error::new(ErrorKind::InvalidData, message));
            }
        };
        match answer {
            Ok(values) => send_message(stream, READS, &encode_reads(&values))?,
            Err((op, message)) => send_message(stream, ERROR, &encode_error(op, 0, &message))?,
        }
    }
}

fn run_batch(
    body: &[u8],
    run: &mut impl FnMut(&Batch) -> Result<Reads, String>,
) -> Result<Vec<u32>, (u32, String)> {
    let (flags, ops) = decode_batch(body).map_err(|e| (NO_OP, e))?;
    let mut batch = Batch::new();
    if flags & REPLAYABLE != 0 {
        batch.replayable();
    }
    for (i, (op, addr, data)) in ops.into_iter().enumerate() {
        match op {
            OP_READ => {
                batch.read(addr);
            }
            OP_WRITE => {
                batch.write(addr, data);
            }
            other => {
                return Err((
                    i as u32,
                    format!("op {other} is not 1 (read) or 2 (write); nothing ran"),
                ));
            }
        }
    }
    run(&batch)
        .map(|reads| reads.values().to_vec())
        .map_err(|e| (NO_OP, e))
}

/// Seconds of silence before the first probe, between probes, and the
/// probes left unanswered before the peer counts as gone: about a minute.
const KEEPALIVE: (i32, i32, i32) = (30, 10, 3);

/// Notices a peer that vanished without closing (a cable pulled, a machine
/// turned off). Without it, a server waits for it for good and refuses
/// everyone else. A peer that is there but quiet, such as an open console,
/// answers the probes and stays.
fn keep_alive(stream: &TcpStream) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let set = |level, name, value: i32| {
        // SAFETY: an int option on a socket this function borrows.
        let r = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                level,
                name,
                (&value as *const i32).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if r == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1)?;
    #[cfg(target_os = "linux")]
    {
        let (idle, interval, count) = KEEPALIVE;
        set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, idle)?;
        set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, interval)?;
        set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, count)?;
        // Data sent and never acknowledged: the same limit, in milliseconds.
        set(
            libc::IPPROTO_TCP,
            libc::TCP_USER_TIMEOUT,
            (idle + interval * count) * 1000,
        )?;
    }
    Ok(())
}

/// How long a connection waits for the client before it to leave.
const WAIT_TO_REFUSE: Duration = Duration::from_secs(1);

/// Hands the connection to the server once no client is served, or refuses
/// it. The client that just left may not be noticed yet when the next one
/// arrives (`hio` connects once per command), so it waits a moment first.
fn take_turn(stream: TcpStream, busy: &AtomicBool, tx: &std::sync::mpsc::Sender<TcpStream>) {
    let since = std::time::Instant::now();
    loop {
        if busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = tx.send(stream);
            return;
        }
        if since.elapsed() >= WAIT_TO_REFUSE {
            refuse(stream);
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Tells a client that another one is being served. The greeting is read
/// first: closing with it unread would reset the connection, and the client
/// might lose the answer.
fn refuse(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut greeting = [0u8; 4];
    let _ = stream.read_exact(&mut greeting);
    let _ = stream.write_all(GREETING);
    let _ = send_message(
        &mut stream,
        ERROR,
        &encode_error(
            NO_OP,
            0,
            "another client is connected. Try again when it is done.",
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> Info {
        Info {
            server: "test".to_string(),
            kind: "board".to_string(),
            target: Some("arty".to_string()),
            master: "jtag".to_string(),
            map_hash: Some(0x1234),
            max_ops: 2,
        }
    }

    /// A server whose window is a 16-word memory. Address 0x100 answers an
    /// error, as a lost transport would.
    fn start() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let mut mem = [0u32; 16];
            serve(
                listener,
                &info(),
                |batch| {
                    let mut values = Vec::new();
                    for (frame, addr) in batch.ops() {
                        if *addr == 0x100 {
                            return Err("the cable is gone".to_string());
                        }
                        match frame.op {
                            Op::Read => values.push(mem[*addr as usize / 4]),
                            Op::Write => mem[*addr as usize / 4] = frame.wdata,
                            Op::Nop => {}
                        }
                    }
                    Ok(Reads::new(values))
                },
                |_| {},
            )
            .unwrap();
        });
        addr
    }

    /// The bytes of the spec's tables, written out once.
    #[test]
    fn a_batch_is_laid_out_as_the_spec_says() {
        let body = encode_batch(REPLAYABLE, &[(OP_WRITE, 0x10, 0x5a), (OP_READ, 0x14, 0)]);
        let mut wire = Vec::new();
        send_message(&mut wire, BATCH, &body).unwrap();
        assert_eq!(
            wire,
            [
                0x01, 23, 0, 0, 0, // BATCH, len
                1, 2, 0, 0, 0, // flags, count
                2, 0x10, 0, 0, 0, 0x5a, 0, 0, 0, // write 0x5a to 0x10
                1, 0x14, 0, 0, 0, 0, 0, 0, 0, // read 0x14
            ]
        );
        let (kind, body) = read_message(&mut wire.as_slice(), 1024).unwrap();
        assert_eq!(kind, BATCH);
        assert_eq!(
            decode_batch(&body).unwrap(),
            (REPLAYABLE, vec![(OP_WRITE, 0x10, 0x5a), (OP_READ, 0x14, 0)])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_connection_is_kept_alive() {
        use std::os::fd::AsRawFd;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        keep_alive(&stream).unwrap();
        let get = |level, name| {
            let mut value = 0i32;
            let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
            // SAFETY: reads an int option into a local.
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    level,
                    name,
                    (&mut value as *mut i32).cast(),
                    &mut len,
                )
            };
            value
        };
        assert_eq!(get(libc::SOL_SOCKET, libc::SO_KEEPALIVE), 1);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE), KEEPALIVE.0);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPCNT), KEEPALIVE.2);
    }

    #[test]
    fn a_message_longer_than_allowed_is_not_read() {
        let mut wire = Vec::new();
        send_message(&mut wire, READS, &[0; 100]).unwrap();
        assert!(read_message(&mut wire.as_slice(), 99).is_err());
    }

    #[test]
    fn a_client_runs_batches_and_reads_the_info() {
        let mut link = Link::connect(&start()).unwrap();
        assert_eq!(link.info(), &info());
        // Three ops with max_ops = 2: two round trips, one answer.
        let mut batch = Batch::new();
        batch.write(0x8, 7);
        let a = batch.read(0x8);
        let b = batch.read(0x4);
        let reads = link.run(&batch).unwrap();
        assert_eq!((reads[a], reads[b]), (7, 0));
        let mut out = [0; 3];
        link.read_burst(0x4, &mut out).unwrap();
        assert_eq!(out, [0, 7, 0]);
    }

    #[test]
    fn an_error_leaves_the_connection_usable() {
        let mut link = Link::connect(&start()).unwrap();
        let err = link.read32(0x100).unwrap_err();
        assert!(err.to_string().contains("the cable is gone"), "{err}");
        assert_eq!(link.read32(0x0).unwrap(), 0);
        // A board is not finished.
        let err = link.finish().unwrap_err();
        assert!(err.to_string().contains("only a simulation"), "{err}");
    }

    #[test]
    fn a_second_client_is_refused_while_one_is_served() {
        let addr = start();
        let mut first = Link::connect(&addr).unwrap();
        first.read32(0).unwrap();
        let err = match Link::connect(&addr) {
            Err(e) => e,
            Ok(_) => panic!("a second client must be refused"),
        };
        assert!(matches!(err, Error::Refused(_)), "{err}");
    }

    /// The next command connects as soon as the last one is done, before
    /// the server has noticed. It is served all the same.
    #[test]
    fn a_client_right_after_another_is_served() {
        let addr = start();
        for _ in 0..20 {
            let mut link = Link::connect(&addr).unwrap_or_else(|e| panic!("{e}"));
            link.read32(0).unwrap();
        }
    }

    #[test]
    fn a_server_that_does_not_greet_is_named() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut hello = [0u8; 4];
            stream.read_exact(&mut hello).unwrap();
            stream.write_all(b"HNS1").unwrap();
        });
        let err = match Link::connect(&addr) {
            Err(e) => e,
            Ok(_) => panic!("an HNS1 server must not be taken"),
        };
        assert!(err.to_string().contains("veryl harness update"), "{err}");
    }
}

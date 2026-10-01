//! The window of a harness running in the Veryl simulator (`--target sim`).
//!
//! `veryl harness sim` listens on a TCP port and writes the address to
//! `sim.addr` beside `regs.json`. The component on the other end
//! (`$comp::hns_link`) drives the window's AXI4-Lite port.
//!
//! The protocol, little endian:
//!
//! ```text
//! client -> server   "HNS1", once per connection
//! server -> client   "HNS1"
//! client -> server   count: u32, then count x (op: u8, addr: u32, data: u32)
//! server -> client   count x (resp: u8, data: u32), once every op has run
//! ```
//!
//! `op` is 1 for a read, 2 for a write and 3 to finish the simulation.
//! `resp` is the AXI response. The component waits for every response before
//! it replies, so nothing is busy or dropped here.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

use crate::bridge::{Batch, Reads};
use crate::frame::Op;

/// Beside `regs.json`.
pub const ADDR_FILE: &str = "sim.addr";

const HELLO: &[u8; 4] = b"HNS1";
const OP_READ: u8 = 1;
const OP_WRITE: u8 = 2;
const OP_FINISH: u8 = 3;

#[derive(Debug)]
pub enum Error {
    /// No `sim.addr`: the simulation is not running.
    NotRunning {
        addr_file: String,
    },
    /// `sim.addr` is there but nothing answers at it.
    NoAnswer {
        addr: String,
        source: std::io::Error,
    },
    Io(std::io::Error),
    /// The other end is not `hns_link`.
    Greeting,
    /// The window answered with an AXI error.
    Resp {
        addr: u32,
        resp: u8,
        write: bool,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotRunning { addr_file } => write!(
                f,
                "the simulation is not running ({addr_file} is missing).\n\
                 Start it with `veryl harness sim`, and keep it running."
            ),
            Error::NoAnswer { addr, source } => write!(
                f,
                "nothing answers at {addr} ({source}).\n\
                 The simulation has stopped. Start it again with `veryl harness sim`."
            ),
            Error::Io(e) => write!(f, "the simulation connection failed: {e}"),
            Error::Greeting => write!(
                f,
                "what answered is not a harness simulation. Check sim.addr."
            ),
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
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub struct Link {
    stream: TcpStream,
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
        stream.write_all(HELLO)?;
        let mut hello = [0u8; 4];
        stream.read_exact(&mut hello)?;
        if &hello != HELLO {
            return Err(Error::Greeting);
        }
        Ok(Link { stream })
    }

    /// Runs the batch in one round trip.
    pub fn run(&mut self, batch: &Batch) -> Result<Reads, Error> {
        let ops: Vec<(u8, u32, u32)> = batch
            .ops()
            .iter()
            .filter_map(|(frame, _)| match frame.op {
                Op::Read => Some((OP_READ, frame.addr, 0)),
                Op::Write => Some((OP_WRITE, frame.addr, frame.wdata)),
                Op::Nop => None,
            })
            .collect();
        let replies = self.exchange(&ops)?;
        let mut values = Vec::new();
        for ((op, addr, _), (resp, data)) in ops.iter().zip(replies) {
            if resp != 0 {
                return Err(Error::Resp {
                    addr: *addr,
                    resp,
                    write: *op == OP_WRITE,
                });
            }
            if *op == OP_READ {
                values.push(data);
            }
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
        let ops: Vec<_> = (0..out.len())
            .map(|i| (OP_READ, addr + 4 * i as u32, 0))
            .collect();
        for (i, (resp, data)) in self.exchange(&ops)?.into_iter().enumerate() {
            if resp != 0 {
                return Err(Error::Resp {
                    addr: addr + 4 * i as u32,
                    resp,
                    write: false,
                });
            }
            out[i] = data;
        }
        Ok(())
    }

    /// Ends the simulation, as Ctrl-C on `veryl harness sim` would.
    pub fn finish(mut self) -> Result<(), Error> {
        self.exchange(&[(OP_FINISH, 0, 0)])?;
        Ok(())
    }

    fn exchange(&mut self, ops: &[(u8, u32, u32)]) -> Result<Vec<(u8, u32)>, Error> {
        let mut out = Vec::with_capacity(4 + ops.len() * 9);
        out.extend_from_slice(&(ops.len() as u32).to_le_bytes());
        for (op, addr, data) in ops {
            out.push(*op);
            out.extend_from_slice(&addr.to_le_bytes());
            out.extend_from_slice(&data.to_le_bytes());
        }
        self.stream.write_all(&out)?;
        let mut back = vec![0u8; ops.len() * 5];
        self.stream.read_exact(&mut back)?;
        Ok(back
            .as_chunks::<5>()
            .0
            .iter()
            .map(|c| (c[0], u32::from_le_bytes(c[1..5].try_into().unwrap())))
            .collect())
    }
}

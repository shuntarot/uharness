//! `$comp::hns_link`: serves the harness window to `hio` over TCP.
//!
//! The testbench connects it to the AXI4-Lite ports of `hns_sim`. At time 0
//! it listens on `127.0.0.1` (another address with `HNS_SIM_ADDR`), writes
//! the address it got to `sim.addr` in the current directory (`veryl harness
//! sim` runs in the harness directory), and holds the simulation
//! there until the first client connects. So `hio` sees everything from the
//! reset release on. After that the simulation keeps running, and clients
//! come and go: `hio` connects once per command.
//!
//! `HNS_SIM_WATCH=1` logs the simulated clock rate once a second.
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
//! `resp` is the AXI response (0 OKAY, 2 SLVERR). `data` is the read value,
//! or 0.
//!
//! This file is copied into the generated output as it is. Keep it to itself
//! and `veryl-component`.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;
use veryl_component::*;

pub const HELLO: &[u8; 4] = b"HNS1";
pub const OP_READ: u8 = 1;
pub const OP_WRITE: u8 = 2;
pub const OP_FINISH: u8 = 3;

/// Cycles between socket polls while the client is quiet. A poll is a
/// system call, and one every cycle held the simulation to 0.16 MHz.
const POLL_CYCLES: u64 = 256;
/// After a reply, poll every cycle for this long: a client that is in the
/// middle of a command sends its next batch soon.
const HOT_CYCLES: u64 = 4096;
/// Cycles between clock reads for the watch.
const WATCH_CYCLES: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op {
    pub op: u8,
    pub addr: u32,
    pub data: u32,
}

/// Takes one whole batch off the front of `buf`, or `None` if it has not all
/// arrived.
pub fn take_batch(buf: &mut Vec<u8>) -> Option<Vec<Op>> {
    let count = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let end = 4 + count * 9;
    if buf.len() < end {
        return None;
    }
    let ops = buf[4..end]
        .as_chunks::<9>()
        .0
        .iter()
        .map(|b| Op {
            op: b[0],
            addr: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            data: u32::from_le_bytes(b[5..9].try_into().unwrap()),
        })
        .collect();
    buf.drain(..end);
    Some(ops)
}

#[derive(Default)]
enum Bus {
    #[default]
    Idle,
    Write {
        aw_done: bool,
        w_done: bool,
    },
    Resp,
    Addr,
    Data,
}

struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
    greeted: bool,
}

#[derive(Component)]
#[component(kind = clocked, requires(native))]
struct HnsLink {
    clk: ClockPort,
    awaddr: OutputPort,
    awvalid: OutputPort,
    awready: InputPort,
    wdata: OutputPort,
    wstrb: OutputPort,
    wvalid: OutputPort,
    wready: InputPort,
    bresp: InputPort,
    bvalid: InputPort,
    bready: OutputPort,
    araddr: OutputPort,
    arvalid: OutputPort,
    arready: InputPort,
    rdata: InputPort,
    rresp: InputPort,
    rvalid: InputPort,
    rready: OutputPort,
    #[state]
    listener: Option<TcpListener>,
    #[state]
    client: Option<Client>,
    /// Ops of the running batch that have not started.
    queue: VecDeque<Op>,
    /// Results of the running batch, in order.
    results: Vec<(u8, u32)>,
    /// Ops in the running batch. 0 when none runs.
    batch_len: usize,
    bus: Bus,
    cycles: u64,
    /// The cycle of the last reply.
    replied: u64,
    watch: bool,
    #[state]
    watched: Option<(Instant, u64)>,
}

impl HnsLink {
    /// Accepts a waiting client, reads what arrived, and queues a batch once
    /// it is whole. Never blocks.
    fn poll(&mut self, ctx: &mut SimCtx) -> Result<()> {
        if self.client.is_none() {
            let Some(listener) = &self.listener else {
                return Ok(());
            };
            match listener.accept() {
                Ok((stream, _)) => self.client = Some(Client::new(stream)?),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        let client = self.client.as_mut().unwrap();
        let mut chunk = [0u8; 4096];
        loop {
            match client.stream.read(&mut chunk) {
                Ok(0) => {
                    self.drop_client();
                    return Ok(());
                }
                Ok(n) => client.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.drop_client();
                    return Ok(());
                }
            }
        }
        if !client.greeted {
            if client.buf.len() < HELLO.len() {
                return Ok(());
            }
            if client.buf[..HELLO.len()] != HELLO[..] {
                ctx.log("hns_link: a client did not greet as hio does; dropped it");
                self.drop_client();
                return Ok(());
            }
            client.buf.drain(..HELLO.len());
            client.greeted = true;
            if client.send(HELLO).is_err() {
                self.drop_client();
                return Ok(());
            }
        }
        if self.batch_len == 0
            && let Some(ops) = take_batch(&mut client.buf)
        {
            self.batch_len = ops.len();
            self.queue = ops.into();
            if self.batch_len == 0 {
                self.reply();
            }
        }
        Ok(())
    }

    /// A client that left takes its batch with it. An op already on the bus
    /// still finishes there.
    fn drop_client(&mut self) {
        self.client = None;
        self.queue.clear();
        self.results.clear();
        self.batch_len = 0;
    }

    fn done(&mut self, resp: u8, data: u32) {
        if self.batch_len == 0 {
            return;
        }
        self.results.push((resp, data));
        if self.results.len() == self.batch_len {
            self.reply();
        }
    }

    fn reply(&mut self) {
        let mut out = Vec::with_capacity(self.results.len() * 5);
        for (resp, data) in self.results.drain(..) {
            out.push(resp);
            out.extend_from_slice(&data.to_le_bytes());
        }
        self.batch_len = 0;
        self.replied = self.cycles;
        let sent = self.client.as_mut().map(|c| c.send(&out));
        if matches!(sent, Some(Err(_))) {
            self.drop_client();
        }
    }

    fn watch(&mut self, ctx: &mut SimCtx) {
        if !self.watch || !self.cycles.is_multiple_of(WATCH_CYCLES) {
            return;
        }
        let now = Instant::now();
        match self.watched {
            None => self.watched = Some((now, self.cycles)),
            Some((then, cycles)) => {
                let secs = now.duration_since(then).as_secs_f64();
                if secs >= 1.0 {
                    let mhz = (self.cycles - cycles) as f64 / secs / 1e6;
                    ctx.log(format!("hns_link: {mhz:.3} MHz"));
                    self.watched = Some((now, self.cycles));
                }
            }
        }
    }
}

impl Client {
    fn new(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        Ok(Client {
            stream,
            buf: Vec::new(),
            greeted: false,
        })
    }

    /// Replies are small and the client is waiting for them, so this blocks.
    fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stream.set_nonblocking(false)?;
        let sent = self.stream.write_all(bytes);
        self.stream.set_nonblocking(true)?;
        sent
    }
}

#[component_impl]
impl HnsLink {
    fn on_init(&mut self, ctx: &mut SimCtx) -> Result<()> {
        for port in [
            self.awvalid,
            self.wvalid,
            self.bready,
            self.arvalid,
            self.rready,
        ] {
            ctx.write(port, 0u64);
        }
        self.watch = std::env::var("HNS_SIM_WATCH").is_ok_and(|v| !v.is_empty() && v != "0");

        let addr = std::env::var("HNS_SIM_ADDR").unwrap_or_else(|_| "127.0.0.1:0".to_string());
        let listener = TcpListener::bind(&addr)
            .map_err(|e| anyhow!("hns_link: cannot listen on {addr}: {e}"))?;
        let local = listener.local_addr()?;
        // Not `ctx.create`: it writes under `target/veryl-components/out/`,
        // where hio does not look.
        std::fs::write("sim.addr", format!("{local}\n"))
            .map_err(|e| anyhow!("hns_link: cannot write sim.addr: {e}"))?;
        ctx.log(format!("hns_link: waiting for hio on {local}"));

        // Time does not start until the first client is here.
        let (stream, _) = listener.accept()?;
        listener.set_nonblocking(true)?;
        self.client = Some(Client::new(stream)?);
        self.listener = Some(listener);
        Ok(())
    }

    fn on_clock(&mut self, ctx: &mut SimCtx) -> Result<()> {
        let _ = ctx.fired(self.clk);
        self.cycles += 1;
        self.watch(ctx);

        match std::mem::take(&mut self.bus) {
            Bus::Idle => {
                let hot = self.batch_len > 0 || self.cycles - self.replied < HOT_CYCLES;
                if self.queue.is_empty() && (hot || self.cycles.is_multiple_of(POLL_CYCLES)) {
                    self.poll(ctx)?;
                }
                match self.queue.pop_front() {
                    Some(Op {
                        op: OP_WRITE,
                        addr,
                        data,
                    }) => {
                        ctx.write(self.awaddr, addr as u64);
                        ctx.write(self.wdata, data as u64);
                        ctx.write(self.wstrb, 0xfu64);
                        ctx.write(self.awvalid, 1u64);
                        ctx.write(self.wvalid, 1u64);
                        self.bus = Bus::Write {
                            aw_done: false,
                            w_done: false,
                        };
                    }
                    Some(Op {
                        op: OP_READ, addr, ..
                    }) => {
                        ctx.write(self.araddr, addr as u64);
                        ctx.write(self.arvalid, 1u64);
                        self.bus = Bus::Addr;
                    }
                    Some(Op { op: OP_FINISH, .. }) => {
                        self.done(0, 0);
                        ctx.finish();
                    }
                    Some(Op { op, .. }) => {
                        ctx.log(format!("hns_link: unknown op {op}; dropped the client"));
                        self.drop_client();
                    }
                    None => {}
                }
            }
            // Inputs are the values before this edge, so a ready seen here
            // with our valid high is a handshake at this edge.
            Bus::Write {
                mut aw_done,
                mut w_done,
            } => {
                if !aw_done && ctx.read_u64(self.awready) == 1 {
                    aw_done = true;
                    ctx.write(self.awvalid, 0u64);
                }
                if !w_done && ctx.read_u64(self.wready) == 1 {
                    w_done = true;
                    ctx.write(self.wvalid, 0u64);
                }
                self.bus = if aw_done && w_done {
                    ctx.write(self.bready, 1u64);
                    Bus::Resp
                } else {
                    Bus::Write { aw_done, w_done }
                };
            }
            Bus::Resp => {
                if ctx.read_u64(self.bvalid) == 1 {
                    ctx.write(self.bready, 0u64);
                    let resp = ctx.read_u64(self.bresp) as u8;
                    self.done(resp, 0);
                } else {
                    self.bus = Bus::Resp;
                }
            }
            Bus::Addr => {
                if ctx.read_u64(self.arready) == 1 {
                    ctx.write(self.arvalid, 0u64);
                    ctx.write(self.rready, 1u64);
                    self.bus = Bus::Data;
                } else {
                    self.bus = Bus::Addr;
                }
            }
            Bus::Data => {
                if ctx.read_u64(self.rvalid) == 1 {
                    ctx.write(self.rready, 0u64);
                    let resp = ctx.read_u64(self.rresp) as u8;
                    let data = ctx.read_u64(self.rdata) as u32;
                    self.done(resp, data);
                } else {
                    self.bus = Bus::Data;
                }
            }
        }
        Ok(())
    }
}

veryl_component_export!("hns_link" => HnsLink);

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(ops: &[(u8, u32, u32)]) -> Vec<u8> {
        let mut out = (ops.len() as u32).to_le_bytes().to_vec();
        for (op, addr, data) in ops {
            out.push(*op);
            out.extend_from_slice(&addr.to_le_bytes());
            out.extend_from_slice(&data.to_le_bytes());
        }
        out
    }

    /// `gen` writes `veryl.manifest.json` beside the copied source, so that
    /// Veryl can analyze the testbench before cargo has built anything.
    #[test]
    fn the_committed_manifest_is_the_one_the_library_exports() {
        let exported = std::str::from_utf8(&VERYL_COMPONENT_MANIFEST_JSON).unwrap();
        assert_eq!(include_str!("../veryl.manifest.json").trim_end(), exported);
    }

    #[test]
    fn a_batch_is_taken_only_when_all_of_it_arrived() {
        let whole = batch(&[(OP_WRITE, 0x10, 0x5a), (OP_READ, 0x38, 0)]);
        let mut buf = whole[..whole.len() - 1].to_vec();
        assert_eq!(take_batch(&mut buf), None);
        buf.push(*whole.last().unwrap());
        buf.extend_from_slice(&batch(&[]));
        let ops = take_batch(&mut buf).unwrap();
        assert_eq!(
            ops,
            vec![
                Op {
                    op: OP_WRITE,
                    addr: 0x10,
                    data: 0x5a
                },
                Op {
                    op: OP_READ,
                    addr: 0x38,
                    data: 0
                },
            ]
        );
        // The next batch (empty) stays for the next call.
        assert_eq!(take_batch(&mut buf), Some(vec![]));
        assert!(buf.is_empty());
    }
}

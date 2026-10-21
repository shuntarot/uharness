//! `$comp::hns_link`: serves the harness window to `hio` over TCP.
//!
//! The testbench connects it to the AXI4-Lite ports of `hns_sim`. At time 0
//! it listens on `127.0.0.1` (another address with `HNS_SIM_ADDR`), writes
//! the address it got to `sim.addr` in the current directory (`veryl harness
//! sim` runs in the harness directory; `0.0.0.0` is written as `127.0.0.1`,
//! which a local `hio` can connect to), and holds the simulation
//! there until the first client connects. So `hio` sees everything from the
//! reset release on. After that the simulation keeps running, and clients
//! come and go: `hio` connects once per command.
//!
//! `HNS_SIM_WATCH=1` logs the simulated clock rate once a second.
//!
//! The protocol is HNS2 (`doc/hns2.md` in veryl-harness): after the
//! greeting, messages of `kind: u8, len: u32, body`. A batch stops at the
//! first op the window answers with an AXI error. One client is served at a
//! time; another is refused until it leaves.
//!
//! `$comp::hns_dram` (`dram.rs`) is the memory of `backing = "dram"`.
//!
//! These files are copied into the generated output as they are. Keep them
//! to themselves and `veryl-component`.

mod dram;

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Instant;
use veryl_component::*;

pub const GREETING: &[u8; 4] = b"HNS2";
pub const BATCH: u8 = 0x01;
pub const FINISH: u8 = 0x02;
pub const INFO: u8 = 0x80;
pub const READS: u8 = 0x81;
pub const ERROR: u8 = 0x82;
pub const OP_READ: u8 = 1;
pub const OP_WRITE: u8 = 2;
/// Not on the wire: queued for `FINISH`, so it runs after the ops before it.
const OP_FINISH: u8 = 0xff;
/// The `op` of an `ERROR` that no single op caused.
const NO_OP: u32 = u32::MAX;
/// The most ops one `BATCH` may carry, as `INFO` says.
const MAX_OPS: usize = 1 << 20;

/// Cycles between socket polls while the client is quiet. A poll is a
/// system call, and one every cycle held the simulation to 0.16 MHz.
const POLL_CYCLES: u64 = 256;
/// After a reply, poll every cycle for this long: a client that is in the
/// middle of a command sends its next batch soon.
const HOT_CYCLES: u64 = 4096;
/// How long a connection waits for the client before it to leave.
const WAIT_TO_REFUSE: std::time::Duration = std::time::Duration::from_secs(1);
/// Cycles between clock reads for the watch.
const WATCH_CYCLES: u64 = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Op {
    pub op: u8,
    pub addr: u32,
    pub data: u32,
}

/// What a client asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Batch(Vec<Op>),
    Finish,
}

/// Takes one whole message off the front of `buf`. `None` if it has not all
/// arrived; `Err` if it breaks the protocol, with the op it names.
pub fn take_request(buf: &mut Vec<u8>) -> Option<std::result::Result<Request, (u32, String)>> {
    let kind = *buf.first()?;
    let len = u32::from_le_bytes(buf.get(1..5)?.try_into().ok()?) as usize;
    if len > 5 + MAX_OPS * 9 {
        return Some(Err((
            NO_OP,
            format!("a message of {len} bytes is too long"),
        )));
    }
    let body = buf.get(5..5 + len)?.to_vec();
    buf.drain(..5 + len);
    Some(match kind {
        BATCH => parse_batch(&body).map(Request::Batch),
        FINISH => Ok(Request::Finish),
        other => Err((
            NO_OP,
            format!("message {other:#04x} is not one this server knows"),
        )),
    })
}

fn parse_batch(body: &[u8]) -> std::result::Result<Vec<Op>, (u32, String)> {
    let count = match body.get(1..5) {
        Some(b) => u32::from_le_bytes(b.try_into().unwrap()) as usize,
        None => {
            return Err((
                NO_OP,
                "a BATCH shorter than its flags and count".to_string(),
            ));
        }
    };
    if body.len() != 5 + count * 9 {
        return Err((
            NO_OP,
            format!("a BATCH of {count} ops must be {} bytes", 5 + count * 9),
        ));
    }
    // The flags do not matter here: the simulation drops nothing.
    let ops: Vec<Op> = body[5..]
        .as_chunks::<9>()
        .0
        .iter()
        .map(|b| Op {
            op: b[0],
            addr: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            data: u32::from_le_bytes(b[5..9].try_into().unwrap()),
        })
        .collect();
    if let Some(i) = ops.iter().position(|o| o.op != OP_READ && o.op != OP_WRITE) {
        return Err((
            i as u32,
            format!("op {} is not 1 (read) or 2 (write); nothing ran", ops[i].op),
        ));
    }
    Ok(ops)
}

fn message(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(kind);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn error(op: u32, resp: u8, text: &str) -> Vec<u8> {
    let mut body = op.to_le_bytes().to_vec();
    body.push(resp);
    body.extend_from_slice(text.as_bytes());
    message(ERROR, &body)
}

fn info() -> Vec<u8> {
    let json = format!(
        "{{\"server\":\"hns_link {}\",\"kind\":\"sim\",\"target\":\"sim\",\
         \"master\":\"jtag\",\"map_hash\":null,\"max_ops\":{MAX_OPS}}}",
        env!("CARGO_PKG_VERSION")
    );
    message(INFO, json.as_bytes())
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
    /// Connections that came while a client is served, and since when.
    #[state]
    waiting: VecDeque<(TcpStream, Instant)>,
    /// Ops of the running batch that have not started.
    queue: VecDeque<Op>,
    /// The op on the bus.
    current: Option<Op>,
    /// Ops of the running batch that are done.
    finished: usize,
    /// Values of the running batch's reads, in order.
    values: Vec<u32>,
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
        if let Some(listener) = &self.listener {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => self.waiting.push_back((stream, Instant::now())),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        // The client that just left may not be noticed yet when the next one
        // arrives (`hio` connects once per command), so a newcomer waits a
        // moment before it is refused.
        if !self.read_client() {
            self.drop_client();
        }
        if self.client.is_none()
            && let Some((stream, _)) = self.waiting.pop_front()
        {
            self.client = Some(Client::new(stream)?);
            if !self.read_client() {
                self.drop_client();
            }
        }
        while let Some((_, since)) = self.waiting.front()
            && since.elapsed() >= WAIT_TO_REFUSE
        {
            let (stream, _) = self.waiting.pop_front().unwrap();
            // Off the simulation's thread: it waits for the greeting.
            std::thread::spawn(move || refuse(stream));
        }
        let Some(client) = self.client.as_mut() else {
            return Ok(());
        };
        if !client.greeted {
            if client.buf.len() < GREETING.len() {
                return Ok(());
            }
            if client.buf[..GREETING.len()] != GREETING[..] {
                ctx.log("hns_link: a client did not greet with HNS2; dropped it");
                self.drop_client();
                return Ok(());
            }
            client.buf.drain(..GREETING.len());
            client.greeted = true;
            let mut hello = GREETING.to_vec();
            hello.extend(info());
            if client.send(&hello).is_err() {
                self.drop_client();
                return Ok(());
            }
        }
        if self.batch_len == 0 {
            match take_request(&mut client.buf) {
                None => {}
                Some(Ok(Request::Batch(ops))) => {
                    self.batch_len = ops.len();
                    self.queue = ops.into();
                    if self.batch_len == 0 {
                        self.reply();
                    }
                }
                Some(Ok(Request::Finish)) => {
                    self.batch_len = 1;
                    self.queue = [Op {
                        op: OP_FINISH,
                        addr: 0,
                        data: 0,
                    }]
                    .into();
                }
                // A bad op is the client's mistake, and the stream is still
                // in step. A bad message is not.
                Some(Err((op, text))) => {
                    let sent = client.send(&error(op, 0, &text));
                    if op == NO_OP || sent.is_err() {
                        ctx.log(format!("hns_link: {text}; dropped the client"));
                        self.drop_client();
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads what the client sent. False when it has left.
    fn read_client(&mut self) -> bool {
        let Some(client) = self.client.as_mut() else {
            return true;
        };
        let mut chunk = [0u8; 4096];
        loop {
            match client.stream.read(&mut chunk) {
                Ok(0) => return false,
                Ok(n) => client.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
    }

    /// A client that left takes its batch with it. An op already on the bus
    /// still finishes there.
    fn drop_client(&mut self) {
        self.client = None;
        self.queue.clear();
        self.values.clear();
        self.finished = 0;
        self.batch_len = 0;
    }

    /// The op on the bus got its response. The batch stops at an error.
    fn done(&mut self, resp: u8, data: u32) {
        let op = self.current.take();
        if self.batch_len == 0 {
            return;
        }
        if resp != 0 {
            let Op { op, addr, .. } = op.unwrap_or_default();
            let text = format!(
                "{} 0x{addr:x} answered {}",
                if op == OP_WRITE {
                    "a write to"
                } else {
                    "a read of"
                },
                if resp == 2 { "SLVERR" } else { "DECERR" }
            );
            self.send(error(self.finished as u32, resp, &text));
            return;
        }
        if op.is_some_and(|o| o.op == OP_READ) {
            self.values.push(data);
        }
        self.finished += 1;
        if self.finished == self.batch_len {
            self.reply();
        }
    }

    fn reply(&mut self) {
        let mut body = (self.values.len() as u32).to_le_bytes().to_vec();
        for v in &self.values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        self.send(message(READS, &body));
    }

    /// Ends the running batch with this answer.
    fn send(&mut self, answer: Vec<u8>) {
        self.queue.clear();
        self.values.clear();
        self.finished = 0;
        self.batch_len = 0;
        self.replied = self.cycles;
        let sent = self.client.as_mut().map(|c| c.send(&answer));
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

/// Tells a client that another one is being served. The greeting is read
/// first: closing with it unread would reset the connection, and the client
/// might lose the answer.
fn refuse(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut greeting = [0u8; 4];
    let _ = stream.read_exact(&mut greeting);
    let mut out = GREETING.to_vec();
    out.extend(error(
        NO_OP,
        0,
        "another client is connected. Try again when it is done.",
    ));
    let _ = stream.write_all(&out);
}

/// The address a local client connects to. One cannot connect to
/// `0.0.0.0` or `::`, so those become the loopback address.
fn reachable(local: SocketAddr) -> SocketAddr {
    match local {
        SocketAddr::V4(a) if a.ip().is_unspecified() => {
            SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, a.port()))
        }
        SocketAddr::V6(a) if a.ip().is_unspecified() => {
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, a.port()))
        }
        other => other,
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
        std::fs::write("sim.addr", format!("{}\n", reachable(local)))
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
                self.current = self.queue.pop_front();
                match self.current {
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
                    // `take_request` lets no other op through.
                    Some(_) | None => {}
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

veryl_component_export!("hns_link" => HnsLink, "hns_dram" => dram::HnsDram);

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(ops: &[(u8, u32, u32)]) -> Vec<u8> {
        let mut body = vec![0];
        body.extend_from_slice(&(ops.len() as u32).to_le_bytes());
        for (op, addr, data) in ops {
            body.push(*op);
            body.extend_from_slice(&addr.to_le_bytes());
            body.extend_from_slice(&data.to_le_bytes());
        }
        message(BATCH, &body)
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
        assert_eq!(take_request(&mut buf), None);
        buf.push(*whole.last().unwrap());
        buf.extend_from_slice(&batch(&[]));
        let request = take_request(&mut buf).unwrap();
        assert_eq!(
            request,
            Ok(Request::Batch(vec![
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
            ]))
        );
        // The next batch (empty) stays for the next call.
        assert_eq!(take_request(&mut buf), Some(Ok(Request::Batch(vec![]))));
        assert!(buf.is_empty());
    }

    #[test]
    fn a_bad_op_is_named_and_nothing_runs() {
        let mut buf = batch(&[(OP_READ, 0, 0), (3, 0, 0)]);
        let Some(Err((op, text))) = take_request(&mut buf) else {
            panic!("op 3 must be refused");
        };
        assert_eq!(op, 1);
        assert!(text.contains("nothing ran"), "{text}");
    }

    #[test]
    fn finish_and_unknown_messages() {
        let mut buf = message(FINISH, &[]);
        buf.extend(message(0x7f, &[1, 2]));
        assert_eq!(take_request(&mut buf), Some(Ok(Request::Finish)));
        assert!(matches!(take_request(&mut buf), Some(Err((NO_OP, _)))));
    }

    /// The JSON is written by hand, so check it is JSON with the keys the
    /// spec lists.
    #[test]
    fn the_info_names_every_key() {
        let info = info();
        assert_eq!(info[0], INFO);
        let json = std::str::from_utf8(&info[5..]).unwrap();
        for key in ["server", "kind", "target", "master", "map_hash", "max_ops"] {
            assert!(json.contains(&format!("\"{key}\":")), "{json}");
        }
        assert!(json.starts_with('{') && json.ends_with('}'), "{json}");
    }

    #[test]
    fn an_unspecified_address_is_written_as_loopback() {
        let any: SocketAddr = "0.0.0.0:5555".parse().unwrap();
        assert_eq!(reachable(any), "127.0.0.1:5555".parse().unwrap());
        let any6: SocketAddr = "[::]:5555".parse().unwrap();
        assert_eq!(reachable(any6), "[::1]:5555".parse().unwrap());
        let one: SocketAddr = "192.168.1.2:5555".parse().unwrap();
        assert_eq!(reachable(one), one);
    }
}

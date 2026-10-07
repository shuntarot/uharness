//! `$comp::hns_dram`: the memory behind `backing = "dram"` in `--target sim`.
//!
//! An AXI4 slave on `$std::axi4_if`. It keeps only the pages that were
//! written, so a 256 MB memory costs what the test touches. An array in
//! Veryl cannot be that large: the simulator stops evaluating it.
//!
//! Memory that was never written reads as 0.
//!
//! One read burst and one write burst run at a time, each at one beat per
//! cycle. The response is always OKAY: the address port is as wide as the
//! memory, so no address can miss it.

use std::collections::HashMap;
use veryl_component::*;

const PAGE_BYTES: u64 = 4096;

/// The written pages. Unwritten bytes read as 0.
#[derive(Default)]
pub struct Sparse {
    pages: HashMap<u64, Box<[u8]>>,
}

impl Sparse {
    pub fn read(&self, addr: u64, out: &mut [u8]) {
        for (i, byte) in out.iter_mut().enumerate() {
            let at = addr + i as u64;
            *byte = self
                .pages
                .get(&(at / PAGE_BYTES))
                .map_or(0, |page| page[(at % PAGE_BYTES) as usize]);
        }
    }

    /// Writes the bytes whose bit in `strb` is set.
    pub fn write(&mut self, addr: u64, data: &[u8], strb: &[u8]) {
        for (i, byte) in data.iter().enumerate() {
            if strb[i / 8] >> (i % 8) & 1 == 0 {
                continue;
            }
            let at = addr + i as u64;
            let page = self
                .pages
                .entry(at / PAGE_BYTES)
                .or_insert_with(|| vec![0; PAGE_BYTES as usize].into_boxed_slice());
            page[(at % PAGE_BYTES) as usize] = *byte;
        }
    }

    /// Pages held, for the tests.
    #[cfg(test)]
    pub fn pages(&self) -> usize {
        self.pages.len()
    }
}

/// One burst, as the address channel gave it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Burst {
    pub addr: u64,
    pub id: u64,
    /// Beats minus one, as on the bus.
    pub len: u64,
    /// Log2 of the bytes per beat.
    pub size: u64,
    /// 0 FIXED, 1 INCR, 2 WRAP.
    pub burst: u64,
}

impl Burst {
    /// The address of beat `n`, by the AXI4 rules.
    pub fn beat(&self, n: u64) -> u64 {
        let bytes = 1u64 << self.size;
        match self.burst {
            0 => self.addr,
            2 => {
                let total = bytes * (self.len + 1);
                let base = self.addr & !(total - 1);
                base + (self.addr - base + n * bytes) % total
            }
            // INCR. The first beat may be unaligned; the rest are aligned.
            _ if n == 0 => self.addr,
            _ => (self.addr & !(bytes - 1)) + n * bytes,
        }
    }
}

#[derive(veryl_component::VerylInterface)]
#[interface(path = "$std::axi4_if", modport = "slave")]
struct Axi4Slave {
    awvalid: InputPort,
    awready: OutputPort,
    awaddr: InputPort,
    awsize: InputPort,
    awburst: InputPort,
    awid: InputPort,
    awlen: InputPort,
    wvalid: InputPort,
    wready: OutputPort,
    wlast: InputPort,
    wdata: InputPort,
    wstrb: InputPort,
    bvalid: OutputPort,
    bready: InputPort,
    bresp: OutputPort,
    bid: OutputPort,
    buser: OutputPort,
    arvalid: InputPort,
    arready: OutputPort,
    araddr: InputPort,
    arsize: InputPort,
    arburst: InputPort,
    arid: InputPort,
    arlen: InputPort,
    rvalid: OutputPort,
    rready: InputPort,
    rlast: OutputPort,
    rdata: OutputPort,
    rresp: OutputPort,
    rid: OutputPort,
    ruser: OutputPort,
}

#[derive(Default)]
enum Write {
    /// `awready` is high.
    #[default]
    Addr,
    /// `wready` is high; the beat count so far.
    Data(Burst, u64),
    /// `bvalid` is high.
    Resp,
}

#[derive(Default)]
enum Read {
    /// `arready` is high.
    #[default]
    Addr,
    /// `rvalid` is high with beat `n`.
    Data(Burst, u64),
}

#[derive(Component)]
#[component(kind = clocked, requires(native))]
pub struct HnsDram {
    clk: ClockPort,
    #[interface]
    axi: Axi4Slave,
    mem: Sparse,
    write: Write,
    read: Read,
    /// Bytes per beat of the data bus, and the address mask.
    bus_bytes: u64,
    addr_mask: u64,
    data: Vec<u8>,
    strb: Vec<u8>,
    words: Vec<u64>,
}

impl HnsDram {
    /// The data bus, aligned down: what one beat at `addr` covers.
    fn lane_base(&self, addr: u64) -> u64 {
        (addr & self.addr_mask) & !(self.bus_bytes - 1)
    }

    fn burst(
        ctx: &mut SimCtx,
        addr: InputPort,
        id: InputPort,
        len: InputPort,
        size: InputPort,
        burst: InputPort,
    ) -> Burst {
        Burst {
            addr: ctx.read_u64(addr),
            id: ctx.read_u64(id),
            len: ctx.read_u64(len),
            size: ctx.read_u64(size),
            burst: ctx.read_u64(burst),
        }
    }

    /// Drives `rdata` / `rlast` for beat `n` of `b`.
    fn drive_beat(&mut self, ctx: &mut SimCtx, b: &Burst, n: u64) {
        let base = self.lane_base(b.beat(n));
        let mut bytes = std::mem::take(&mut self.data);
        self.mem.read(base, &mut bytes);
        for (i, word) in self.words.iter_mut().enumerate() {
            let mut le = [0u8; 8];
            let chunk = &bytes[(i * 8).min(bytes.len())..((i + 1) * 8).min(bytes.len())];
            le[..chunk.len()].copy_from_slice(chunk);
            *word = u64::from_le_bytes(le);
        }
        self.data = bytes;
        ctx.write_words(self.axi.rdata, &self.words);
        ctx.write_u64(self.axi.rlast, u64::from(n == b.len));
    }

    fn write_beat(&mut self, ctx: &mut SimCtx, b: &Burst, n: u64) {
        let base = self.lane_base(b.beat(n));
        ctx.read_words(self.axi.wdata, &mut self.words);
        for (i, byte) in self.data.iter_mut().enumerate() {
            *byte = (self.words[i / 8] >> (8 * (i % 8))) as u8;
        }
        let mut strb = [0u64; 2];
        let strb_words = self.axi.wstrb.words();
        if strb_words <= strb.len() {
            ctx.read_words(self.axi.wstrb, &mut strb[..strb_words]);
        }
        for (i, byte) in self.strb.iter_mut().enumerate() {
            *byte = (strb[i / 8] >> (8 * (i % 8))) as u8;
        }
        self.mem.write(base, &self.data, &self.strb);
    }
}

#[component_impl]
impl HnsDram {
    fn on_init(&mut self, ctx: &mut SimCtx) -> Result<()> {
        let width = self.axi.wdata.width() as u64;
        if !width.is_power_of_two() || width < 8 {
            bail!("hns_dram: the data bus is {width} bits; it must be a power of two bytes");
        }
        // A strobe of 128 bits covers a 1024-bit bus, the widest AXI4 allows.
        if self.axi.wstrb.words() > 2 {
            bail!("hns_dram: the data bus is wider than AXI4 allows");
        }
        self.bus_bytes = width / 8;
        let addr_bits = self.axi.awaddr.width();
        self.addr_mask = if addr_bits >= 64 {
            u64::MAX
        } else {
            (1u64 << addr_bits) - 1
        };
        self.data = vec![0; self.bus_bytes as usize];
        self.strb = vec![0; self.bus_bytes.div_ceil(8) as usize];
        self.words = vec![0; self.axi.wdata.words()];

        for port in [
            self.axi.wready,
            self.axi.bvalid,
            self.axi.bresp,
            self.axi.bid,
            self.axi.buser,
            self.axi.rvalid,
            self.axi.rlast,
            self.axi.rresp,
            self.axi.rid,
            self.axi.ruser,
        ] {
            ctx.write_u64(port, 0);
        }
        ctx.write_words(self.axi.rdata, &self.words);
        ctx.write_u64(self.axi.awready, 1);
        ctx.write_u64(self.axi.arready, 1);
        Ok(())
    }

    // Inputs are the values before this edge, and so are our outputs: a
    // valid seen here with our ready high is a handshake at this edge.
    fn on_clock(&mut self, ctx: &mut SimCtx) -> Result<()> {
        let _ = ctx.fired(self.clk);
        let axi = &self.axi;
        let (awaddr, awid, awlen, awsize, awburst) =
            (axi.awaddr, axi.awid, axi.awlen, axi.awsize, axi.awburst);
        let (araddr, arid, arlen, arsize, arburst) =
            (axi.araddr, axi.arid, axi.arlen, axi.arsize, axi.arburst);

        self.write = match std::mem::take(&mut self.write) {
            Write::Addr if ctx.read_u64(self.axi.awvalid) == 1 => {
                let b = Self::burst(ctx, awaddr, awid, awlen, awsize, awburst);
                ctx.write_u64(self.axi.awready, 0);
                ctx.write_u64(self.axi.wready, 1);
                Write::Data(b, 0)
            }
            Write::Data(b, n) if ctx.read_u64(self.axi.wvalid) == 1 => {
                self.write_beat(ctx, &b, n);
                // `wlast` ends the burst; the count is the bus's own word.
                if ctx.read_u64(self.axi.wlast) == 1 || n == b.len {
                    ctx.write_u64(self.axi.wready, 0);
                    ctx.write_u64(self.axi.bid, b.id);
                    ctx.write_u64(self.axi.bvalid, 1);
                    Write::Resp
                } else {
                    Write::Data(b, n + 1)
                }
            }
            Write::Resp if ctx.read_u64(self.axi.bready) == 1 => {
                ctx.write_u64(self.axi.bvalid, 0);
                ctx.write_u64(self.axi.awready, 1);
                Write::Addr
            }
            other => other,
        };

        self.read = match std::mem::take(&mut self.read) {
            Read::Addr if ctx.read_u64(self.axi.arvalid) == 1 => {
                let b = Self::burst(ctx, araddr, arid, arlen, arsize, arburst);
                ctx.write_u64(self.axi.arready, 0);
                ctx.write_u64(self.axi.rid, b.id);
                ctx.write_u64(self.axi.rvalid, 1);
                self.drive_beat(ctx, &b, 0);
                Read::Data(b, 0)
            }
            Read::Data(b, n) if ctx.read_u64(self.axi.rready) == 1 => {
                if n == b.len {
                    ctx.write_u64(self.axi.rvalid, 0);
                    ctx.write_u64(self.axi.rlast, 0);
                    ctx.write_u64(self.axi.arready, 1);
                    Read::Addr
                } else {
                    self.drive_beat(ctx, &b, n + 1);
                    Read::Data(b, n + 1)
                }
            }
            other => other,
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwritten_memory_reads_as_zero_and_costs_nothing() {
        let mut mem = Sparse::default();
        let mut out = [0xffu8; 8];
        mem.read(0x0fff_fff8, &mut out);
        assert_eq!(out, [0; 8]);
        assert_eq!(mem.pages(), 0);
        mem.write(0x0fff_fffe, &[1, 2, 3, 4], &[0b1011]);
        // Across a page boundary, and byte 2 is not strobed.
        assert_eq!(mem.pages(), 2);
        let mut out = [0u8; 4];
        mem.read(0x0fff_fffe, &mut out);
        assert_eq!(out, [1, 2, 0, 4]);
    }

    #[test]
    fn beats_follow_the_axi4_burst_rules() {
        let incr = Burst {
            addr: 0x102,
            len: 3,
            size: 2,
            burst: 1,
            ..Default::default()
        };
        let beats: Vec<u64> = (0..4).map(|n| incr.beat(n)).collect();
        assert_eq!(beats, [0x102, 0x104, 0x108, 0x10c]);
        let wrap = Burst {
            addr: 0x108,
            len: 3,
            size: 2,
            burst: 2,
            ..Default::default()
        };
        let beats: Vec<u64> = (0..4).map(|n| wrap.beat(n)).collect();
        assert_eq!(beats, [0x108, 0x10c, 0x100, 0x104]);
        let fixed = Burst {
            addr: 0x40,
            len: 2,
            size: 2,
            burst: 0,
            ..Default::default()
        };
        assert_eq!(fixed.beat(2), 0x40);
    }
}

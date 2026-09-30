//! Contents of the bus DR. The layout must match `rtl/hns/src/dr.veryl`.
//!
//! ```text
//!   in : [AW+33:34] addr | [33:2] wdata | [1:0] op   (00=nop, 01=read, 10=write)
//!   out: [AW+33:34] 0    | [33:2] rdata | [1] sticky_err | [0] busy
//! ```
//!
//! Bits are sent LSB first. TDI enters the JTAG shift register at the MSB and
//! TDO leaves at the LSB, so the first bit the host sends lands in `[0]`.

/// Operation in the DR. The values must match `OP_*` in the RTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Nop = 0b00,
    Read = 0b01,
    Write = 0b10,
}

/// One command sent to the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub op: Op,
    pub addr: u32,
    pub wdata: u32,
}

impl Frame {
    pub fn nop() -> Self {
        Frame {
            op: Op::Nop,
            addr: 0,
            wdata: 0,
        }
    }

    pub fn read(addr: u32) -> Self {
        Frame {
            op: Op::Read,
            addr,
            wdata: 0,
        }
    }

    pub fn write(addr: u32, wdata: u32) -> Self {
        Frame {
            op: Op::Write,
            addr,
            wdata,
        }
    }

    /// DR length in bits. When AW makes this 8n+1, MPSSE sends it as n bytes plus
    /// one final bit, which is the cheapest shape.
    pub fn len(aw: u32) -> usize {
        aw as usize + 34
    }

    /// Bits in send order (LSB first).
    pub fn to_bits(&self, aw: u32) -> Vec<bool> {
        let mut bits = Vec::with_capacity(Self::len(aw));
        let op = self.op as u32;
        for i in 0..2 {
            bits.push((op >> i) & 1 == 1);
        }
        for i in 0..32 {
            bits.push((self.wdata >> i) & 1 == 1);
        }
        for i in 0..aw {
            bits.push((self.addr >> i) & 1 == 1);
        }
        bits
    }
}

/// What Capture returns. Note: this is the result of the previous command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response {
    /// The previous transaction was still running, so the command just sent was
    /// dropped.
    pub busy: bool,
    /// Sticky: an error response or a dropped command happened at least once.
    pub err: bool,
    pub rdata: u32,
}

impl Response {
    pub fn from_bits(bits: &[bool]) -> Self {
        let bit = |i: usize| bits.get(i).copied().unwrap_or(false);
        let mut rdata = 0u32;
        for i in 0..32 {
            if bit(2 + i) {
                rdata |= 1 << i;
            }
        }
        Response {
            busy: bit(0),
            err: bit(1),
            rdata,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Must match the RTL field layout. A mismatch shows up only on real hardware.
    #[test]
    fn a_write_frame_puts_each_field_where_the_rtl_reads_it() {
        let bits = Frame::write(0x12, 0xa5a5_1234).to_bits(15);
        assert_eq!(bits.len(), Frame::len(15));

        // op = write = 0b10 -> bit0 = 0, bit1 = 1
        assert!(!bits[0]);
        assert!(bits[1]);

        // wdata is [33:2]
        let mut wdata = 0u32;
        for i in 0..32 {
            if bits[2 + i] {
                wdata |= 1 << i;
            }
        }
        assert_eq!(wdata, 0xa5a5_1234);

        // addr is [AW+33:34]
        let mut addr = 0u32;
        for i in 0..15 {
            if bits[34 + i] {
                addr |= 1 << i;
            }
        }
        assert_eq!(addr, 0x12);
    }

    /// AW=15 gives 49 = 8*6+1, the cheapest shape for MPSSE.
    #[test]
    fn the_length_lands_on_a_byte_plus_one_bit() {
        for aw in [7, 15, 23] {
            assert_eq!(Frame::len(aw) % 8, 1, "AW={aw}");
        }
    }

    #[test]
    fn a_response_splits_into_busy_err_and_data() {
        let mut bits = vec![false; 49];
        bits[0] = true; // busy
        bits[1] = false; // err
        for i in 0..32 {
            bits[2 + i] = (0xdead_beefu32 >> i) & 1 == 1;
        }
        let r = Response::from_bits(&bits);
        assert!(r.busy);
        assert!(!r.err);
        assert_eq!(r.rdata, 0xdead_beef);
    }
}

//! ARM debug access over JTAG: an ADIv5 JTAG-DP and its MEM-APs.
//!
//! A Zynq-7000 has an ARM DAP on the same JTAG chain as the PL. Through a
//! MEM-AP the host reads and writes the PS's memory and registers, with no
//! program on the CPUs. That is how the PS gets set up before the PL can use
//! the DDR behind it.
//!
//! ## The protocol, in short
//!
//! The DAP's IR is 4 bits. DPACC and APACC select a 35-bit DR: bit 0 is RnW,
//! bits 2:1 are A[3:2], bits 34:3 the data. What comes back holds an ACK in
//! bits 2:0 and, in bits 34:3, the result of the *previous* read. So a read
//! is posted, and its value is fetched by reading the DP's RDBUFF.
//!
//! WAIT means the DAP did not take the request; the same scan is sent again.
//! An error is not in the ACK (OK and FAULT share one code over JTAG): it is
//! a sticky bit in CTRL/STAT, read once an access is over.

use crate::bridge::JtagIo;
use crate::mpsse::{Chan, Mpsse};

/// What the DAP needs from the JTAG layer.
pub trait Tap {
    type Error;
    /// Shift the IR; returns its capture value.
    fn ir(&mut self, ir: u32, len: u8) -> Result<u32, Self::Error>;
    /// Shift the DR; returns TDO.
    fn dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, Self::Error>;
}

impl<C: Chan> Tap for Mpsse<C> {
    type Error = C::Error;

    fn ir(&mut self, ir: u32, len: u8) -> Result<u32, C::Error> {
        self.scan_ir(ir, len)
    }

    fn dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, C::Error> {
        self.scan_dr(bits)
    }
}

/// A borrowed TAP, so a probe held elsewhere can reach the DAP for a while.
impl<T: Tap + ?Sized> Tap for &mut T {
    type Error = T::Error;

    fn ir(&mut self, ir: u32, len: u8) -> Result<u32, T::Error> {
        (**self).ir(ir, len)
    }

    fn dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, T::Error> {
        (**self).dr(bits)
    }
}

/// The DAP's IR length, the same on every ADIv5 JTAG-DP.
pub const IR_LEN: u8 = 4;
pub const IR_ABORT: u32 = 0b1000;
pub const IR_DPACC: u32 = 0b1010;
pub const IR_APACC: u32 = 0b1011;
pub const IR_IDCODE: u32 = 0b1110;

/// DP registers (A[3:2] as a byte address).
pub const DP_CTRL_STAT: u8 = 0x4;
pub const DP_SELECT: u8 = 0x8;
pub const DP_RDBUFF: u8 = 0xc;

/// MEM-AP registers.
pub const AP_CSW: u8 = 0x00;
pub const AP_TAR: u8 = 0x04;
pub const AP_DRW: u8 = 0x0c;
pub const AP_IDR: u8 = 0xfc;

const ACK_OK: u8 = 0b010;
const ACK_WAIT: u8 = 0b001;

/// CTRL/STAT: power-up requests and their acknowledges.
const CSYSPWRUPACK: u32 = 1 << 31;
const CSYSPWRUPREQ: u32 = 1 << 30;
const CDBGPWRUPACK: u32 = 1 << 29;
const CDBGPWRUPREQ: u32 = 1 << 28;
/// CTRL/STAT: sticky error flags. Writing 1 clears them on a JTAG-DP.
const STICKYERR: u32 = 1 << 5;
const STICKYCMP: u32 = 1 << 4;
const STICKYORUN: u32 = 1 << 1;
const STICKY: u32 = STICKYERR | STICKYCMP | STICKYORUN;

/// CSW: 32-bit accesses, the address does not move.
const CSW_SIZE_MASK: u32 = 0x3f;
const CSW_SIZE_32: u32 = 0b010;

/// How often a WAIT is answered by sending the scan again.
const WAIT_TRIES: usize = 100;
/// How often CTRL/STAT is read while waiting for power-up.
const POWER_TRIES: usize = 100;

#[derive(Debug)]
pub enum Error<E> {
    Io(E),
    /// An ACK that is neither OK nor WAIT: the chain or the IR is wrong.
    Ack {
        got: u8,
    },
    /// The DAP kept answering WAIT. The transaction was aborted.
    Wait,
    /// A memory access kept the DAP answering WAIT. It was aborted.
    Stuck {
        what: String,
    },
    /// The debug or system domain did not power up.
    PowerUp {
        ctrl_stat: u32,
    },
    /// An access failed; CTRL/STAT had a sticky error flag set.
    Fault {
        what: String,
        ctrl_stat: u32,
    },
}

impl<E: std::fmt::Debug> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "jtag io failed: {e:?}"),
            Error::Ack { got } => write!(
                f,
                "the DAP answered ACK {got:#05b}, which is neither OK nor WAIT.\n\
                 The scan reached another device, or nothing. Check `dap` and the chain \
                 in the target's [jtag] against `hio probe`."
            ),
            Error::Wait => write!(
                f,
                "the DAP answered WAIT {WAIT_TRIES} times in a row, so hio aborted the \
                 access.\n\
                 The access it waits on does not finish: the address may have no slave \
                 behind it, or its clock is off."
            ),
            Error::Stuck { what } => write!(
                f,
                "{what} did not finish: the DAP answered WAIT {WAIT_TRIES} times in a row, \
                 so hio aborted it.\n\
                 The bus behind the access port did not complete the transfer: nothing \
                 answers there yet (on a Zynq, the DDR before `hio dap ps7-init`), or its \
                 clock is off. Until the board is power-cycled, the bus may stay stuck \
                 and fail every access."
            ),
            Error::PowerUp { ctrl_stat } => write!(
                f,
                "the DAP did not power up its debug and system domains \
                 (CTRL/STAT {ctrl_stat:#010x}).\n\
                 On a Zynq-7000 the PS must be powered, and debug must not be disabled \
                 (secure boot)."
            ),
            Error::Fault { what, ctrl_stat } => write!(
                f,
                "{what} failed: the DAP set a sticky error (CTRL/STAT {ctrl_stat:#010x}).\n\
                 Nothing answered at that address, or it refused the access."
            ),
        }
    }
}

/// An ADIv5 JTAG-DP, reached through `T`.
pub struct Dap<T: Tap> {
    tap: T,
    /// The IR now selected, so it is not shifted again.
    ir: Option<u32>,
    /// What SELECT holds, so it is not written again.
    select: Option<u32>,
    /// CSW for 32-bit accesses, per AP, built once from what the AP holds.
    csw: std::collections::HashMap<u8, u32>,
}

impl<T: Tap> Dap<T> {
    pub fn new(tap: T) -> Self {
        Dap {
            tap,
            ir: None,
            select: None,
            csw: Default::default(),
        }
    }

    pub fn into_inner(self) -> T {
        self.tap
    }

    /// The DP's IDCODE (DPIDR).
    pub fn idcode(&mut self) -> Result<u32, Error<T::Error>> {
        self.set_ir(IR_IDCODE)?;
        let got = self.tap.dr(&[false; 32]).map_err(Error::Io)?;
        Ok(bits_to_u32(&got))
    }

    /// Power up the debug and system domains. Needed before any AP access.
    pub fn power_up(&mut self) -> Result<(), Error<T::Error>> {
        self.dp_write(DP_CTRL_STAT, CSYSPWRUPREQ | CDBGPWRUPREQ | STICKY)?;
        let mut ctrl = 0;
        for _ in 0..POWER_TRIES {
            ctrl = self.dp_read(DP_CTRL_STAT)?;
            if ctrl & (CSYSPWRUPACK | CDBGPWRUPACK) == CSYSPWRUPACK | CDBGPWRUPACK {
                return Ok(());
            }
        }
        Err(Error::PowerUp { ctrl_stat: ctrl })
    }

    pub fn dp_read(&mut self, a: u8) -> Result<u32, Error<T::Error>> {
        self.scan(IR_DPACC, true, a, 0)?;
        self.scan(IR_DPACC, true, DP_RDBUFF, 0)
    }

    pub fn dp_write(&mut self, a: u8, value: u32) -> Result<(), Error<T::Error>> {
        self.scan(IR_DPACC, false, a, value)?;
        Ok(())
    }

    pub fn ap_read(&mut self, ap: u8, reg: u8) -> Result<u32, Error<T::Error>> {
        self.select(ap, reg)?;
        self.scan(IR_APACC, true, reg & 0xc, 0)?;
        self.scan(IR_DPACC, true, DP_RDBUFF, 0)
    }

    pub fn ap_write(&mut self, ap: u8, reg: u8, value: u32) -> Result<(), Error<T::Error>> {
        self.select(ap, reg)?;
        self.scan(IR_APACC, false, reg & 0xc, value)?;
        Ok(())
    }

    /// Read one 32-bit word through MEM-AP `ap`.
    pub fn read32(&mut self, ap: u8, addr: u32) -> Result<u32, Error<T::Error>> {
        let what = || format!("reading {addr:#010x} through AP {ap}");
        self.setup(ap, addr).map_err(|e| stuck(e, what))?;
        let value = self.ap_read(ap, AP_DRW).map_err(|e| stuck(e, what))?;
        self.sticky(what)?;
        Ok(value)
    }

    /// Write one 32-bit word through MEM-AP `ap`.
    pub fn write32(&mut self, ap: u8, addr: u32, value: u32) -> Result<(), Error<T::Error>> {
        let what = || format!("writing {addr:#010x} through AP {ap}");
        self.setup(ap, addr).map_err(|e| stuck(e, what))?;
        self.ap_write(ap, AP_DRW, value)?;
        // RDBUFF waits until the write is done, so the sticky flags are current.
        self.scan(IR_DPACC, true, DP_RDBUFF, 0)
            .map_err(|e| stuck(e, what))?;
        self.sticky(what)
    }

    /// Use this CSW for `ap` instead of the one built from what it holds.
    /// The size field is set to 32 bits and the address increment to off.
    pub fn set_csw(&mut self, ap: u8, csw: u32) -> Result<(), Error<T::Error>> {
        let csw = (csw & !CSW_SIZE_MASK) | CSW_SIZE_32;
        self.ap_write(ap, AP_CSW, csw)?;
        self.csw.insert(ap, csw);
        Ok(())
    }

    /// Abort the transaction the DAP is stuck on (DAPABORT), so the next
    /// access is not held up behind it.
    pub fn abort(&mut self) -> Result<(), Error<T::Error>> {
        self.set_ir(IR_ABORT)?;
        let mut bits = vec![false; 3];
        bits.extend((0..32).map(|i| i == 0));
        self.tap.dr(&bits).map_err(Error::Io)?;
        Ok(())
    }

    /// CSW for 32-bit accesses that leave the address alone, then TAR.
    fn setup(&mut self, ap: u8, addr: u32) -> Result<(), Error<T::Error>> {
        if !self.csw.contains_key(&ap) {
            // Keep what the AP holds above the size and increment fields
            // (protection and mode bits, which differ between APs).
            let held = self.ap_read(ap, AP_CSW)?;
            let csw = (held & !CSW_SIZE_MASK) | CSW_SIZE_32;
            self.ap_write(ap, AP_CSW, csw)?;
            self.csw.insert(ap, csw);
        }
        self.ap_write(ap, AP_TAR, addr)
    }

    /// Fail when an access left a sticky error, and clear it.
    fn sticky(&mut self, what: impl FnOnce() -> String) -> Result<(), Error<T::Error>> {
        let ctrl = self.dp_read(DP_CTRL_STAT)?;
        if ctrl & STICKY != 0 {
            self.dp_write(
                DP_CTRL_STAT,
                (ctrl & (CSYSPWRUPREQ | CDBGPWRUPREQ)) | STICKY,
            )?;
            return Err(Error::Fault {
                what: what(),
                ctrl_stat: ctrl,
            });
        }
        Ok(())
    }

    fn select(&mut self, ap: u8, reg: u8) -> Result<(), Error<T::Error>> {
        let value = (ap as u32) << 24 | ((reg >> 4) as u32) << 4;
        if self.select != Some(value) {
            self.dp_write(DP_SELECT, value)?;
            self.select = Some(value);
        }
        Ok(())
    }

    fn set_ir(&mut self, ir: u32) -> Result<(), Error<T::Error>> {
        if self.ir != Some(ir) {
            self.tap.ir(ir, IR_LEN).map_err(Error::Io)?;
            self.ir = Some(ir);
        }
        Ok(())
    }

    /// One DPACC/APACC scan, sent again on WAIT. Returns the data that came
    /// back: the result of the previous read.
    fn scan(&mut self, ir: u32, read: bool, a: u8, data: u32) -> Result<u32, Error<T::Error>> {
        self.set_ir(ir)?;
        let mut bits = Vec::with_capacity(35);
        bits.push(read);
        bits.push(a & 0x4 != 0);
        bits.push(a & 0x8 != 0);
        bits.extend((0..32).map(|i| (data >> i) & 1 == 1));
        for _ in 0..WAIT_TRIES {
            let got = self.tap.dr(&bits).map_err(Error::Io)?;
            let ack = (got[0] as u8) | (got[1] as u8) << 1 | (got[2] as u8) << 2;
            match ack {
                ACK_OK => return Ok(bits_to_u32(&got[3..35])),
                ACK_WAIT => continue,
                got => return Err(Error::Ack { got }),
            }
        }
        // Left as it is, the stuck transaction holds up every later access.
        self.abort()?;
        Err(Error::Wait)
    }
}

/// A WAIT on a memory access, said with what the access was.
fn stuck<E>(e: Error<E>, what: impl FnOnce() -> String) -> Error<E> {
    match e {
        Error::Wait => Error::Stuck { what: what() },
        e => e,
    }
}

fn bits_to_u32(bits: &[bool]) -> u32 {
    bits.iter()
        .take(32)
        .enumerate()
        .fold(0, |a, (i, &b)| if b { a | 1 << i } else { a })
}

/// What an AP's IDR says, for `hio dap info`.
pub fn describe_idr(idr: u32) -> String {
    if idr == 0 {
        return "none".to_string();
    }
    let class = (idr >> 13) & 0xf;
    let kind = idr & 0xf;
    let what = match (class, kind) {
        (8, 0) => "MEM-AP, JTAG",
        (8, 1) => "MEM-AP, AHB",
        (8, 2) => "MEM-AP, APB",
        (8, 4) => "MEM-AP, AXI",
        (8, _) => "MEM-AP",
        (0, _) => "JTAG-AP",
        _ => "unknown class",
    };
    format!("{what} (IDR {idr:#010x})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeAp {
        idr: u32,
        csw: u32,
        tar: u32,
        mem: HashMap<u32, u32>,
        /// Addresses with no slave: an access sets STICKYERR.
        holes: Vec<u32>,
    }

    /// A JTAG-DP with MEM-APs, at the level of IR and DR scans.
    struct FakeDap {
        ir: u32,
        ctrl: u32,
        select: u32,
        aps: Vec<FakeAp>,
        /// The result of the last read, returned by the next scan.
        last: u32,
        /// WAITs to answer before taking a request.
        waits: usize,
        powers_up: bool,
    }

    impl FakeDap {
        fn new() -> Self {
            FakeDap {
                ir: IR_IDCODE,
                ctrl: 0,
                select: 0,
                aps: vec![
                    FakeAp {
                        idr: 0x2477_0011,
                        csw: 0x0300_0040,
                        tar: 0,
                        mem: HashMap::new(),
                        holes: vec![0xdead_0000],
                    },
                    FakeAp {
                        idr: 0x2477_0002,
                        csw: 0x8000_0040,
                        tar: 0,
                        mem: HashMap::new(),
                        holes: vec![],
                    },
                ],
                last: 0,
                waits: 0,
                powers_up: true,
            }
        }

        fn ap_access(&mut self, read: bool, reg: u8, data: u32) {
            let apsel = (self.select >> 24) as usize;
            let reg = (self.select & 0xf0) as u8 | reg;
            let Some(ap) = self.aps.get_mut(apsel) else {
                self.last = 0;
                return;
            };
            match (reg, read) {
                (AP_IDR, true) => self.last = ap.idr,
                (AP_CSW, true) => self.last = ap.csw,
                (AP_CSW, false) => {
                    // Only 32-bit, non-incrementing accesses are modelled.
                    assert_eq!(data & CSW_SIZE_MASK, CSW_SIZE_32, "CSW size/increment");
                    assert_eq!(data & !CSW_SIZE_MASK, ap.csw & !CSW_SIZE_MASK, "CSW kept");
                    ap.csw = data;
                }
                (AP_TAR, false) => ap.tar = data,
                (AP_DRW, _) if ap.holes.contains(&ap.tar) => self.ctrl |= STICKYERR,
                (AP_DRW, true) => self.last = *ap.mem.get(&ap.tar).unwrap_or(&0),
                (AP_DRW, false) => {
                    ap.mem.insert(ap.tar, data);
                }
                _ => {}
            }
        }
    }

    impl Tap for FakeDap {
        type Error = ();

        fn ir(&mut self, ir: u32, len: u8) -> Result<u32, ()> {
            assert_eq!(len, IR_LEN);
            self.ir = ir;
            Ok(0b0001)
        }

        fn dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, ()> {
            if self.ir == IR_IDCODE {
                assert_eq!(bits.len(), 32);
                return Ok((0..32).map(|i| (0x4ba0_0477u32 >> i) & 1 == 1).collect());
            }
            assert_eq!(bits.len(), 35, "DPACC/APACC scans are 35 bits");
            if self.ir == IR_ABORT {
                assert!(bits[3], "DAPABORT");
                self.waits = 0;
                return Ok(ack_bits(ACK_OK, 0));
            }
            if self.waits > 0 {
                self.waits -= 1;
                return Ok(ack_bits(ACK_WAIT, 0));
            }
            let out = ack_bits(ACK_OK, self.last);
            let read = bits[0];
            let a = (bits[1] as u8) << 2 | (bits[2] as u8) << 3;
            let data = bits_to_u32(&bits[3..35]);
            match (self.ir, a) {
                (IR_DPACC, DP_CTRL_STAT) if read => self.last = self.ctrl,
                (IR_DPACC, DP_CTRL_STAT) => {
                    let req = data & (CSYSPWRUPREQ | CDBGPWRUPREQ);
                    self.ctrl &= !(data & STICKY);
                    self.ctrl = (self.ctrl & STICKY) | req;
                    if self.powers_up {
                        self.ctrl |= req << 1;
                    }
                }
                (IR_DPACC, DP_SELECT) if !read => self.select = data,
                (IR_DPACC, DP_RDBUFF) => {}
                (IR_APACC, reg) => {
                    assert_eq!(
                        self.ctrl & CDBGPWRUPACK,
                        CDBGPWRUPACK,
                        "AP access before power-up"
                    );
                    self.ap_access(read, reg, data);
                }
                other => panic!("unexpected scan {other:?}"),
            }
            Ok(out)
        }
    }

    fn ack_bits(ack: u8, data: u32) -> Vec<bool> {
        let mut v: Vec<bool> = (0..3).map(|i| (ack >> i) & 1 == 1).collect();
        v.extend((0..32).map(|i| (data >> i) & 1 == 1));
        v
    }

    fn up() -> Dap<FakeDap> {
        let mut dap = Dap::new(FakeDap::new());
        dap.power_up().unwrap();
        dap
    }

    #[test]
    fn the_dp_idcode_is_read_with_its_own_instruction() {
        assert_eq!(Dap::new(FakeDap::new()).idcode().unwrap(), 0x4ba0_0477);
    }

    /// A read is posted: its value comes back with the next scan.
    #[test]
    fn an_ap_register_is_read_through_rdbuff() {
        let mut dap = up();
        assert_eq!(dap.ap_read(0, AP_IDR).unwrap(), 0x2477_0011);
        assert_eq!(dap.ap_read(1, AP_IDR).unwrap(), 0x2477_0002);
        assert_eq!(dap.ap_read(5, AP_IDR).unwrap(), 0);
    }

    #[test]
    fn memory_written_through_a_mem_ap_reads_back() {
        let mut dap = up();
        dap.write32(0, 0xf800_0008, 0xdf0d).unwrap();
        dap.write32(1, 0xf800_0008, 0x1234).unwrap();
        assert_eq!(dap.read32(0, 0xf800_0008).unwrap(), 0xdf0d);
        assert_eq!(dap.read32(1, 0xf800_0008).unwrap(), 0x1234);
    }

    #[test]
    fn a_wait_is_answered_by_sending_the_scan_again() {
        let mut dap = up();
        dap.write32(0, 0x100, 7).unwrap();
        dap.tap.waits = 5;
        assert_eq!(dap.read32(0, 0x100).unwrap(), 7);
        dap.tap.waits = WAIT_TRIES + 1;
        let err = dap.read32(0, 0x100).unwrap_err();
        assert!(matches!(err, Error::Stuck { .. }), "{err:?}");
        assert!(err.to_string().contains("reading 0x00000100"), "{err}");
        // Aborted, so the next access is not held up.
        assert_eq!(dap.read32(0, 0x100).unwrap(), 7);
    }

    #[test]
    fn a_csw_can_be_given() {
        let mut dap = up();
        dap.set_csw(1, 0x8000_0040).unwrap();
        dap.write32(1, 0x300, 5).unwrap();
        assert_eq!(dap.tap.aps[1].csw, 0x8000_0042);
    }

    /// An address with nothing behind it fails, and the flag is cleared so the
    /// next access works.
    #[test]
    fn a_failed_access_is_reported_and_cleared() {
        let mut dap = up();
        let err = dap.read32(0, 0xdead_0000).unwrap_err();
        assert!(matches!(err, Error::Fault { .. }), "{err:?}");
        assert!(err.to_string().contains("0xdead0000"), "{err}");
        dap.write32(0, 0x200, 9).unwrap();
        assert_eq!(dap.read32(0, 0x200).unwrap(), 9);
    }

    #[test]
    fn a_dap_that_does_not_power_up_says_so() {
        let mut fake = FakeDap::new();
        fake.powers_up = false;
        let err = Dap::new(fake).power_up().unwrap_err();
        assert!(matches!(err, Error::PowerUp { .. }), "{err:?}");
    }

    #[test]
    fn an_idr_is_named_by_its_class_and_type() {
        assert_eq!(describe_idr(0x2477_0011), "MEM-AP, AHB (IDR 0x24770011)");
        assert_eq!(describe_idr(0x2477_0002), "MEM-AP, APB (IDR 0x24770002)");
        assert_eq!(describe_idr(0), "none");
    }
}

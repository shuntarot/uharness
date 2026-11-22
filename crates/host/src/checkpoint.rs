//! Saving and restoring a hart through its Debug Module: the registers that
//! `hio cpu save` and `restore` keep in `checkpoint.json`.
//!
//! Memory is not here. The harness memories go through the window or the
//! card, and the caller writes them to an ELF; this module only reaches what
//! the Debug Module reaches.

use serde::{Deserialize, Serialize};

use crate::dm::{Debug, Dmi, Reg};

/// The version of `checkpoint.json`. A reader refuses another one.
pub const FORMAT: u32 = 1;

/// A number written as a hex string, so a 64-bit value with the top bit set
/// survives JSON readers that take numbers as doubles. A plain JSON number
/// is read too, as `map_hash` in regs.json is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hex(pub u64);

impl Serialize for Hex {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{:#x}", self.0))
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Number(u64),
            Text(String),
        }
        let text = match Either::deserialize(d)? {
            Either::Number(n) => return Ok(Hex(n)),
            Either::Text(t) => t,
        };
        let digits = text
            .strip_prefix("0x")
            .ok_or_else(|| serde::de::Error::custom(format!("{text:?} is not 0x and hex")))?;
        u64::from_str_radix(&digits.replace('_', ""), 16)
            .map(Hex)
            .map_err(|_| serde::de::Error::custom(format!("{text:?} is not a 64-bit hex number")))
    }
}

/// `checkpoint.json`: everything but the memory images.
///
/// `hio cpu save` writes every field. A file made elsewhere (from a
/// simulator, say) may leave out what it does not know, and `restore` leaves
/// that part of the target as it is: a hart not listed, a register or CSR
/// not given, a memory not in `memory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub format: u32,
    /// The `hio` that wrote it.
    #[serde(default)]
    pub hio: String,
    /// The DUT and the map hash of the harness it came from. A restore onto
    /// another harness is refused: the memories and the core may differ.
    pub dut: String,
    pub map_hash: Hex,
    /// The `dmi` bundle.
    #[serde(default)]
    pub dmi: String,
    #[serde(default)]
    pub harts: Vec<HartState>,
    /// Device registers (`checkpoint.mmio`), in the order they are written.
    #[serde(default)]
    pub mmio: Vec<MmioValue>,
    /// The harness memory saved in `memory.elf`. Within a range, what the
    /// image leaves out is zero.
    #[serde(default)]
    pub memory: Vec<Range>,
    /// Memories inside the core (`checkpoint.region`), each in `<name>.bin`.
    #[serde(default)]
    pub region: Vec<Range>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmioValue {
    pub name: String,
    pub addr: Hex,
    pub width: u32,
    pub value: Hex,
}

/// A range of memory, by the address the core sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    /// The bundle, or the region's name.
    pub name: String,
    pub addr: Hex,
    pub size: Hex,
}

/// What one hart holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HartState {
    pub hart: u32,
    pub xlen: u32,
    /// `dpc`: where the hart goes on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pc: Option<Hex>,
    /// `dcsr.prv`: 0 U, 1 S, 3 M.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prv: Option<u32>,
    /// `x0`..`x31` (`x0` is always 0): all 32, or none.
    #[serde(default)]
    pub gpr: Vec<Hex>,
    /// FLEN and `f0`..`f31`, if the hart has an FPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fpr: Option<Fprs>,
    /// The CSRs the hart has, in the order they are written back.
    #[serde(default)]
    pub csr: Vec<Csr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fprs {
    pub flen: u32,
    pub regs: Vec<Hex>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Csr {
    /// The number, which is what counts. The name is for a person.
    pub number: Hex,
    #[serde(default)]
    pub name: String,
    pub value: Hex,
}

const MSTATUS: u32 = 0x300;
const FCSR: u32 = 0x003;
const SATP: u32 = 0x180;
const DCSR: u32 = 0x7b0;
const DPC: u32 = 0x7b1;
/// `mstatus.FS`: the FPU's state. Off makes every FP access trap.
const FS: u64 = 0b11 << 13;
/// `dcsr.stopcount`: the counters stop while the hart is halted, so the
/// debugger's own instructions are not counted.
const STOPCOUNT: u64 = 1 << 10;
const PRV: u64 = 0b11;

const SFENCE_VMA: u32 = 0x1200_0073;
const FENCE_I: u32 = 0x0000_100f;

/// The standard CSRs a hart may have that a program can write and that
/// hold its state. Read-only ones (`mvendorid`, ...), aliases (`cycle`,
/// `sstatus`, `sie`, `sip`) and the debugger's own (`dcsr`, `dpc`,
/// `dscratch*`) are left out. `mstatus` and `fcsr` are handled apart.
///
/// The order is the order of writing back: the PMP addresses before their
/// configuration, which may lock them.
pub fn standard_csrs(xlen: u32) -> Vec<u32> {
    let rv32 = xlen == 32;
    let mut out = vec![
        0x302, 0x303, 0x304, 0x305, 0x306, // medeleg mideleg mie mtvec mcounteren
        0x30a, // menvcfg
    ];
    if rv32 {
        out.extend([0x310, 0x31a]); // mstatush menvcfgh
    }
    out.push(0x320); // mcountinhibit
    out.extend(0x323..=0x33f); // mhpmevent3..31
    out.extend(0x340..=0x344); // mscratch mepc mcause mtval mip
    out.extend(0x3b0..=0x3ef); // pmpaddr0..63
    // pmpcfg: RV64 has the even ones only.
    out.extend((0x3a0..=0x3af).filter(|n| rv32 || n % 2 == 0));
    out.extend([0xb00, 0xb02]); // mcycle minstret
    out.extend(0xb03..=0xb1f); // mhpmcounter3..31
    if rv32 {
        out.extend([0xb80, 0xb82]);
        out.extend(0xb83..=0xb9f);
    }
    // S-mode.
    out.extend([0x105, 0x106, 0x10a, 0x140, 0x141, 0x142, 0x143, SATP]);
    out
}

/// Saves a halted hart. `extra` are CSRs to keep besides the standard ones.
///
/// It sets `dcsr.stopcount` first, so the counters read here are the ones
/// the program left. A CSR the hart does not have is skipped. Returns the
/// state and notes for the user (what was skipped and why).
pub fn save_hart<D: Dmi>(
    dm: &mut Debug<D>,
    extra: &[u32],
) -> Result<(HartState, Vec<String>), String> {
    let mut notes = Vec::new();
    let xlen = dm.xlen()?;
    let dcsr = dm.read_reg(Reg::Csr(DCSR))?;
    if dcsr & STOPCOUNT == 0 {
        dm.write_reg(Reg::Csr(DCSR), dcsr | STOPCOUNT)?;
        if dm.read_reg(Reg::Csr(DCSR))? & STOPCOUNT == 0 {
            notes.push(
                "the counters run while the hart is halted (`dcsr.stopcount` is fixed at 0), \
                 so mcycle and minstret include the debugger's own work"
                    .to_string(),
            );
        }
    }
    let mut gpr = vec![Hex(0)];
    for n in 1..32 {
        gpr.push(Hex(dm.read_reg(Reg::Gpr(n))?));
    }
    let pc = dm.read_reg(Reg::Csr(DPC))?;

    let mstatus = dm.read_reg(Reg::Csr(MSTATUS))?;
    let mut csr = vec![Csr {
        number: Hex(MSTATUS.into()),
        name: Reg::Csr(MSTATUS).to_string(),
        value: Hex(mstatus),
    }];
    let mut wanted = standard_csrs(xlen);
    for c in extra {
        if !wanted.contains(c) && *c != MSTATUS {
            wanted.push(*c);
        }
    }
    for number in wanted {
        if let Some(value) = dm.read_csr_if_any(number)? {
            csr.push(Csr {
                number: Hex(number.into()),
                name: Reg::Csr(number).to_string(),
                value: Hex(value),
            });
        } else if extra.contains(&number) {
            notes.push(format!(
                "csr {number:#05x} is in `checkpoint.csrs`, and the hart does not have it"
            ));
        }
    }

    // The FPU: on only while it is read, as the program left it after.
    let mut fpr = None;
    dm.write_reg(Reg::Csr(MSTATUS), mstatus | FS)?;
    if dm.read_reg(Reg::Csr(MSTATUS))? & FS != 0 {
        let flen = dm.flen()?;
        let mut regs = Vec::new();
        for n in 0..32 {
            regs.push(Hex(dm.read_reg(Reg::Fpr(n))?));
        }
        let fcsr = dm.read_reg(Reg::Csr(FCSR))?;
        csr.push(Csr {
            number: Hex(FCSR.into()),
            name: Reg::Csr(FCSR).to_string(),
            value: Hex(fcsr),
        });
        fpr = Some(Fprs { flen, regs });
    }
    dm.write_reg(Reg::Csr(MSTATUS), mstatus)?;

    Ok((
        HartState {
            hart: dm.hart(),
            xlen,
            pc: Some(Hex(pc)),
            prv: Some((dcsr & PRV) as u32),
            gpr,
            fpr,
            csr,
        },
        notes,
    ))
}

/// Restores a halted hart. Memory must already hold what the hart will see:
/// the TLB and the instruction cache are flushed here.
///
/// A CSR that does not take its value is reported, not fatal: some bits are
/// fixed by the hardware (`mip` has bits only the hardware sets).
pub fn restore_hart<D: Dmi>(dm: &mut Debug<D>, state: &HartState) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    let xlen = dm.xlen()?;
    if xlen != state.xlen {
        return Err(format!(
            "hart {} is {xlen}-bit, and the checkpoint was taken from a {}-bit hart.",
            dm.hart(),
            state.xlen
        ));
    }
    check_hart(state)?;
    let mstatus = match state.csr.iter().find(|c| c.number.0 == u64::from(MSTATUS)) {
        Some(c) => c.value.0,
        None => dm.read_reg(Reg::Csr(MSTATUS))?,
    };
    let dcsr = dm.read_reg(Reg::Csr(DCSR))?;
    dm.write_reg(Reg::Csr(DCSR), dcsr | STOPCOUNT)?;

    if let Some(fpr) = &state.fpr {
        dm.write_reg(Reg::Csr(MSTATUS), mstatus | FS)?;
        if dm.read_reg(Reg::Csr(MSTATUS))? & FS == 0 {
            return Err(format!(
                "hart {} has no FPU, and the checkpoint holds FP registers.",
                dm.hart()
            ));
        }
        for (n, v) in fpr.regs.iter().enumerate() {
            dm.write_reg(Reg::Fpr(n as u32), v.0)?;
        }
    }
    for c in state
        .csr
        .iter()
        .filter(|c| c.number.0 != u64::from(MSTATUS))
    {
        let number = c.number.0 as u32;
        if let Err(e) = dm.write_reg(Reg::Csr(number), c.value.0) {
            notes.push(format!("{} was not written back: {e}", Reg::Csr(number)));
        }
    }
    let has = |n: u32| state.csr.iter().any(|c| c.number.0 == u64::from(n));
    if has(SATP) {
        dm.exec(&[SFENCE_VMA])
            .map_err(|e| format!("sfence.vma after writing satp: {e}"))?;
    }
    if let Err(e) = dm.exec(&[FENCE_I]) {
        notes.push(format!(
            "fence.i did not run ({e}); the instruction cache may hold old code"
        ));
    }
    dm.write_reg(Reg::Csr(MSTATUS), mstatus)?;
    for (n, v) in state.gpr.iter().enumerate().skip(1) {
        dm.write_reg(Reg::Gpr(n as u32), v.0)?;
    }
    if let Some(pc) = state.pc {
        dm.write_reg(Reg::Csr(DPC), pc.0)?;
    }
    if let Some(prv) = state.prv {
        let dcsr = dm.read_reg(Reg::Csr(DCSR))?;
        dm.write_reg(Reg::Csr(DCSR), (dcsr & !PRV) | u64::from(prv))?;
    }
    Ok(notes)
}

/// Refuses a hart state that cannot be meant: one made by hand or by
/// another tool may be wrong in ways `hio cpu save` never is.
pub fn check_hart(state: &HartState) -> Result<(), String> {
    let hart = state.hart;
    if state.xlen != 32 && state.xlen != 64 {
        return Err(format!(
            "hart {hart}: xlen is {}; it is 32 or 64.",
            state.xlen
        ));
    }
    if !state.gpr.is_empty() && state.gpr.len() != 32 {
        return Err(format!(
            "hart {hart}: gpr has {} values; give all 32 (x0 to x31), or leave it out.",
            state.gpr.len()
        ));
    }
    if let Some(prv) = state.prv
        && ![0, 1, 3].contains(&prv)
    {
        return Err(format!(
            "hart {hart}: prv is {prv}; it is 0 (U), 1 (S) or 3 (M)."
        ));
    }
    if let Some(fpr) = &state.fpr
        && fpr.regs.len() != 32
    {
        return Err(format!(
            "hart {hart}: fpr.regs has {} values; give all 32 (f0 to f31).",
            fpr.regs.len()
        ));
    }
    for c in &state.csr {
        if c.number.0 >= 0x1000 {
            return Err(format!(
                "hart {hart}: csr {:#x} is not a CSR number (0 to 0xfff).",
                c.number.0
            ));
        }
    }
    Ok(())
}

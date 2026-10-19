//! A RISC-V Debug Module, reached through its DMI.
//!
//! This is the debugger side of the RISC-V Debug Specification (0.13.2 and
//! 1.0): halt, resume, step, registers and reset. It knows nothing of the
//! transport. The caller gives it a `Dmi`, which reads and writes DMI
//! registers; on a harness that is a window region marked `dmi`.
//!
//! Registers go through the abstract command "access register". A Debug
//! Module that cannot reach a CSR that way (Hazard3 reaches only GPRs) gets
//! it through the program buffer instead, with `s0` saved and put back.

use std::fmt;

/// Reads and writes DMI registers.
///
/// `run` takes a list so a transport can send it in one go: over JTAG each
/// round trip costs far more than the accesses themselves. The accesses run
/// in order, and the result holds one value per read.
pub trait Dmi {
    fn run(&mut self, ops: &[Op]) -> Result<Vec<u32>, String>;
}

/// One DMI access. The address is the DMI register number, not a byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read(u32),
    Write(u32, u32),
}

/// DMI register numbers.
pub mod reg {
    pub const DATA0: u32 = 0x04;
    pub const DATA1: u32 = 0x05;
    pub const DMCONTROL: u32 = 0x10;
    pub const DMSTATUS: u32 = 0x11;
    pub const HARTINFO: u32 = 0x12;
    pub const ABSTRACTCS: u32 = 0x16;
    pub const COMMAND: u32 = 0x17;
    pub const ABSTRACTAUTO: u32 = 0x18;
    pub const PROGBUF0: u32 = 0x20;
    pub const SBCS: u32 = 0x38;
    pub const HALTSUM0: u32 = 0x40;

    /// Names for the registers a person types, as the spec spells them.
    pub const NAMES: &[(&str, u32)] = &[
        ("data0", DATA0),
        ("data1", DATA1),
        ("dmcontrol", DMCONTROL),
        ("dmstatus", DMSTATUS),
        ("hartinfo", HARTINFO),
        ("abstractcs", ABSTRACTCS),
        ("command", COMMAND),
        ("abstractauto", ABSTRACTAUTO),
        ("progbuf0", PROGBUF0),
        ("progbuf1", PROGBUF0 + 1),
        ("sbcs", SBCS),
        ("haltsum0", HALTSUM0),
    ];

    /// A register by name or number (`dmstatus`, `0x11`, `17`).
    pub fn parse(text: &str) -> Option<u32> {
        if let Some((_, n)) = NAMES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(text))
        {
            return Some(*n);
        }
        let n = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Some(hex) => u32::from_str_radix(&hex.replace('_', ""), 16).ok()?,
            None => text.parse().ok()?,
        };
        (n < 0x80).then_some(n)
    }
}

// dmcontrol
const DMACTIVE: u32 = 1 << 0;
const NDMRESET: u32 = 1 << 1;
const CLRRESETHALTREQ: u32 = 1 << 2;
const SETRESETHALTREQ: u32 = 1 << 3;
const ACKHAVERESET: u32 = 1 << 28;
const RESUMEREQ: u32 = 1 << 30;
const HALTREQ: u32 = 1 << 31;

// dmstatus
const HASRESETHALTREQ: u32 = 1 << 5;
const ALLHALTED: u32 = 1 << 9;
const ALLRUNNING: u32 = 1 << 11;
const ANYUNAVAIL: u32 = 1 << 12;
const ANYNONEXISTENT: u32 = 1 << 14;
const ALLRESUMEACK: u32 = 1 << 17;
const ALLHAVERESET: u32 = 1 << 19;
const IMPEBREAK: u32 = 1 << 22;
const NDMRESETPENDING: u32 = 1 << 24;

// abstractcs
const BUSY: u32 = 1 << 12;
const CMDERR_CLEAR: u32 = 0x7 << 8;

// The CSRs the debugger itself uses.
const DCSR: u32 = 0x7b0;
const DPC: u32 = 0x7b1;
const TSELECT: u32 = 0x7a0;
const TDATA1: u32 = 0x7a1;
const TDATA2: u32 = 0x7a2;
const TINFO: u32 = 0x7a4;

// `tdata1` bits that `mcontrol` (type 2) and `mcontrol6` (type 6) share.
const T_LOAD: u64 = 1 << 0;
const T_STORE: u64 = 1 << 1;
const T_EXECUTE: u64 = 1 << 2;
const T_U: u64 = 1 << 3;
const T_S: u64 = 1 << 4;
const T_M: u64 = 1 << 6;
/// `action = 1`: enter debug mode.
const T_ACTION_DEBUG: u64 = 1 << 12;

// `tdata1` of an exception trigger (`etrigger`, type 5).
const E_ACTION_DEBUG: u64 = 1;
const E_U: u64 = 1 << 6;
const E_S: u64 = 1 << 7;
const E_M: u64 = 1 << 9;

const MCAUSE: u32 = 0x342;
const MEPC: u32 = 0x341;
const MTVAL: u32 = 0x343;

/// Exceptions a program does not mean to raise: faults and misalignment.
/// Not `ecall` (8, 9, 11) or `ebreak` (3), which programs use on purpose.
pub const FAULTS: u64 = (1 << 0)
    | (1 << 1)
    | (1 << 2)
    | (1 << 4)
    | (1 << 5)
    | (1 << 6)
    | (1 << 7)
    | (1 << 12)
    | (1 << 13)
    | (1 << 15);

/// The name of an exception cause (`mcause` without the interrupt bit).
pub fn exception_text(cause: u64) -> &'static str {
    match cause {
        0 => "instruction address misaligned",
        1 => "instruction access fault",
        2 => "illegal instruction",
        3 => "breakpoint",
        4 => "load address misaligned",
        5 => "load access fault",
        6 => "store address misaligned",
        7 => "store access fault",
        8 => "ecall from U-mode",
        9 => "ecall from S-mode",
        11 => "ecall from M-mode",
        12 => "instruction page fault",
        13 => "load page fault",
        15 => "store page fault",
        _ => "unknown exception",
    }
}

/// Why a hart is halted, read from `dcsr.cause` and, for a trigger, which
/// trigger it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// `halt`, or a halt-on-reset (Hazard3 reports both as a request).
    Request,
    Step,
    Ebreak,
    Reset,
    Breakpoint,
    /// An exception the trigger catches: the hart is at the handler.
    Trap {
        cause: u64,
        mepc: u64,
        mtval: u64,
    },
    /// Another trigger.
    Trigger,
    Other(u64),
}

// dcsr
const DCSR_STEP: u64 = 1 << 2;
const DCSR_EBREAKM: u64 = 1 << 15;
const DCSR_EBREAKS: u64 = 1 << 13;
const DCSR_EBREAKU: u64 = 1 << 12;

/// GPR `s0`, which the program buffer uses to carry a CSR value.
const S0: u32 = 8;

const EBREAK: u32 = 0x0010_0073;

/// Polls before a wait counts as stuck. Each is a round trip over the
/// transport, so this is far longer than any Debug Module needs.
const POLLS: usize = 1000;

/// A register of the hart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg {
    /// `x0`..`x31`.
    Gpr(u32),
    /// A CSR by number. `dpc` is the PC while halted.
    Csr(u32),
}

/// ABI names of the GPRs, in order.
pub const GPR_NAMES: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// The CSRs `regs` shows besides the GPRs.
pub const CSR_NAMES: &[(&str, u32)] = &[
    ("mstatus", 0x300),
    ("misa", 0x301),
    ("mie", 0x304),
    ("mtvec", 0x305),
    ("mscratch", 0x340),
    ("mepc", 0x341),
    ("mcause", 0x342),
    ("mtval", 0x343),
    ("mip", 0x344),
    ("dcsr", DCSR),
    ("dpc", DPC),
];

impl Reg {
    /// `pc`, a GPR (`a0`, `x10`), a CSR by name (`mstatus`) or number
    /// (`csr0x300`, `0x300`).
    pub fn parse(text: &str) -> Option<Reg> {
        let text = text.to_ascii_lowercase();
        if text == "pc" {
            return Some(Reg::Csr(DPC));
        }
        if text == "fp" {
            return Some(Reg::Gpr(S0));
        }
        if let Some(n) = GPR_NAMES.iter().position(|name| *name == text) {
            return Some(Reg::Gpr(n as u32));
        }
        if let Some(n) = text.strip_prefix('x').and_then(|n| n.parse::<u32>().ok()) {
            return (n < 32).then_some(Reg::Gpr(n));
        }
        if let Some((_, csr)) = CSR_NAMES.iter().find(|(name, _)| *name == text) {
            return Some(Reg::Csr(*csr));
        }
        // Every other standard name the disassembler knows (`mcountinhibit`,
        // `mcycle`, `pmpaddr0`, ...).
        if let Some(csr) = (0..0x1000).find(|&n| crate::disasm::csr_name(n) == text) {
            return Some(Reg::Csr(csr));
        }
        let number = text.strip_prefix("csr").unwrap_or(&text);
        let csr = u32::from_str_radix(number.strip_prefix("0x")?, 16).ok()?;
        (csr < 0x1000).then_some(Reg::Csr(csr))
    }

    /// The `regno` of the access register command.
    fn regno(self) -> u32 {
        match self {
            Reg::Gpr(n) => 0x1000 + n,
            Reg::Csr(n) => n,
        }
    }
}

impl fmt::Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reg::Gpr(n) => write!(f, "{}", GPR_NAMES[*n as usize]),
            Reg::Csr(DPC) => write!(f, "pc"),
            Reg::Csr(n) => match crate::disasm::csr_name(*n) {
                name if name.starts_with("0x") => write!(f, "csr{n:#05x}"),
                name => write!(f, "{name}"),
            },
        }
    }
}

/// Whether a `dmstatus` value says the selected harts are all halted.
pub fn all_halted(dmstatus: u32) -> bool {
    dmstatus & ALLHALTED != 0
}

/// Why the hart is halted, from `dcsr.cause`.
pub fn cause_text(cause: u64) -> &'static str {
    match cause {
        1 => "ebreak",
        2 => "trigger",
        3 => "halt request",
        4 => "single step",
        5 => "halt at reset",
        6 => "halt group",
        _ => "unknown cause",
    }
}

/// What an abstract command said when it failed (`abstractcs.cmderr`).
fn cmderr_text(cmderr: u32) -> &'static str {
    match cmderr {
        1 => "busy: a command was written while one was running",
        2 => "not supported",
        3 => "an exception in the hart",
        4 => "the hart is not halted, or not running, as the command needs",
        5 => "a bus error",
        _ => "an unknown error",
    }
}

/// A failed abstract command, kept apart so a caller can fall back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CmdErr(u32);

/// How the Debug Module describes itself, read without disturbing the hart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub dmstatus: u32,
    pub dmcontrol: u32,
    pub hartinfo: u32,
    pub abstractcs: u32,
    pub sbcs: u32,
    /// Bits `hartsel` keeps.
    pub hartsel_bits: u32,
    /// Harts that answer, counted from 0 up to the first that does not.
    pub harts: u32,
}

impl Info {
    /// `dmstatus.version`: 2 is 0.13, 3 is 1.0.
    pub fn version(&self) -> &'static str {
        match self.dmstatus & 0xf {
            0 => "none",
            1 => "0.11",
            2 => "0.13",
            3 => "1.0",
            _ => "unknown",
        }
    }

    pub fn progbuf_size(&self) -> u32 {
        (self.abstractcs >> 24) & 0x1f
    }

    pub fn data_count(&self) -> u32 {
        self.abstractcs & 0xf
    }

    pub fn impebreak(&self) -> bool {
        self.dmstatus & IMPEBREAK != 0
    }

    pub fn has_reset_halt(&self) -> bool {
        self.dmstatus & HASRESETHALTREQ != 0
    }

    /// `sbcs.sbversion`: 0 means no system bus access.
    pub fn sb_version(&self) -> u32 {
        self.sbcs >> 29
    }

    /// The access widths the system bus takes, in bits.
    pub fn sb_widths(&self) -> Vec<u32> {
        [8, 16, 32, 64, 128]
            .into_iter()
            .enumerate()
            .filter(|(i, _)| self.sbcs & (1 << i) != 0)
            .map(|(_, w)| w)
            .collect()
    }
}

impl fmt::Display for Info {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.dmstatus;
        let yes = |bit: u32| if s & bit != 0 { "yes" } else { "no" };
        writeln!(f, "debug module  version {} ({:#010x})", self.version(), s)?;
        writeln!(
            f,
            "harts         {} answering, hartsel {} bits",
            self.harts, self.hartsel_bits
        )?;
        writeln!(
            f,
            "selected hart {}",
            if s & ALLHALTED != 0 {
                "halted"
            } else if s & ALLRUNNING != 0 {
                "running"
            } else if s & ANYUNAVAIL != 0 {
                "unavailable"
            } else {
                "unknown"
            }
        )?;
        writeln!(
            f,
            "abstract      {} data registers, program buffer {} words, implicit ebreak {}",
            self.data_count(),
            self.progbuf_size(),
            yes(IMPEBREAK)
        )?;
        writeln!(f, "halt at reset {}", yes(HASRESETHALTREQ))?;
        let h = self.hartinfo;
        writeln!(
            f,
            "hartinfo      {} scratch, data at {:#x} ({} words, {})",
            (h >> 20) & 0xf,
            h & 0xfff,
            (h >> 12) & 0xf,
            if h & (1 << 16) != 0 { "memory" } else { "CSRs" }
        )?;
        match self.sb_version() {
            0 => write!(f, "system bus    none"),
            v => write!(
                f,
                "system bus    version {v}, {} address bits, widths {:?}",
                (self.sbcs >> 5) & 0x7f,
                self.sb_widths()
            ),
        }
    }
}

/// A breakpoint: an address trigger that halts into the debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakpoint {
    pub index: u32,
    pub addr: u64,
}

/// One trigger, as `tinfo` lists the types it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    pub index: u32,
    /// One bit per `tdata1.type` it supports.
    pub types: u32,
}

impl Trigger {
    /// Takes an exception match (`etrigger`).
    pub fn matches_exceptions(&self) -> bool {
        self.types & (1 << 5) != 0
    }

    /// Takes an address match (`mcontrol` or `mcontrol6`).
    pub fn matches_addresses(&self) -> bool {
        self.types & ((1 << 2) | (1 << 6)) != 0
    }

    pub fn type_names(&self) -> Vec<&'static str> {
        const NAMES: [(u32, &str); 7] = [
            (2, "mcontrol (address/data)"),
            (3, "icount"),
            (4, "itrigger (interrupt)"),
            (5, "etrigger (exception)"),
            (6, "mcontrol6 (address/data)"),
            (7, "tmexttrigger"),
            (15, "disabled"),
        ];
        NAMES
            .iter()
            .filter(|(t, _)| self.types & (1 << t) != 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

/// A Debug Module with one hart selected.
pub struct Debug<D: Dmi> {
    dmi: D,
    hart: u32,
    /// Learnt on the first register access, while halted.
    xlen: Option<u32>,
    progbuf_size: u32,
    impebreak: bool,
    /// The CSRs that the access register command refused; they go through
    /// the program buffer from then on.
    csr_by_progbuf: bool,
}

impl<D: Dmi> Debug<D> {
    /// Turns the Debug Module on if it is off, and selects `hart`.
    pub fn attach(dmi: D, hart: u32) -> Result<Self, String> {
        let mut dm = Debug {
            dmi,
            hart,
            xlen: None,
            progbuf_size: 0,
            impebreak: false,
            csr_by_progbuf: false,
        };
        let control = dm.read(reg::DMCONTROL)?;
        if control & DMACTIVE == 0 {
            dm.write(reg::DMCONTROL, DMACTIVE)?;
            dm.wait("the Debug Module to come out of reset", |dm| {
                Ok(dm.read(reg::DMCONTROL)? & DMACTIVE != 0)
            })?;
        }
        dm.write(reg::DMCONTROL, dm.control(0))?;
        let status = dm.read(reg::DMSTATUS)?;
        if status & 0xf == 0 {
            return Err(
                "the Debug Module reports no version (dmstatus.version is 0). \
                 Either nothing answers on the DMI, or the module is held in reset."
                    .to_string(),
            );
        }
        if status & ANYNONEXISTENT != 0 {
            return Err(format!(
                "hart {hart} does not exist. `hio cpu info` says how many harts answer."
            ));
        }
        let abstractcs = dm.read(reg::ABSTRACTCS)?;
        dm.progbuf_size = (abstractcs >> 24) & 0x1f;
        dm.impebreak = status & IMPEBREAK != 0;
        Ok(dm)
    }

    /// The DMI underneath, for other traffic on the same link.
    pub fn dmi_mut(&mut self) -> &mut D {
        &mut self.dmi
    }

    pub fn hart(&self) -> u32 {
        self.hart
    }

    fn read(&mut self, reg: u32) -> Result<u32, String> {
        Ok(self.dmi.run(&[Op::Read(reg)])?[0])
    }

    fn write(&mut self, reg: u32, value: u32) -> Result<(), String> {
        self.dmi.run(&[Op::Write(reg, value)]).map(|_| ())
    }

    /// `dmcontrol` with the module on, this hart selected, and `bits`.
    fn control(&self, bits: u32) -> u32 {
        let hartsello = (self.hart & 0x3ff) << 16;
        let hartselhi = ((self.hart >> 10) & 0x3ff) << 6;
        DMACTIVE | hartsello | hartselhi | bits
    }

    fn wait(
        &mut self,
        what: &str,
        mut done: impl FnMut(&mut Self) -> Result<bool, String>,
    ) -> Result<(), String> {
        for _ in 0..POLLS {
            if done(self)? {
                return Ok(());
            }
        }
        Err(format!("timed out waiting for {what}"))
    }

    pub fn status(&mut self) -> Result<u32, String> {
        self.read(reg::DMSTATUS)
    }

    pub fn is_halted(&mut self) -> Result<bool, String> {
        Ok(self.status()? & ALLHALTED != 0)
    }

    /// Reads the Debug Module's own registers. Changes nothing the hart sees:
    /// `hartsel` is probed and put back.
    pub fn info(&mut self) -> Result<Info, String> {
        let r = self.dmi.run(&[
            Op::Read(reg::DMSTATUS),
            Op::Read(reg::DMCONTROL),
            Op::Read(reg::HARTINFO),
            Op::Read(reg::ABSTRACTCS),
            Op::Read(reg::SBCS),
        ])?;
        // The bits of `hartsel` that stick say how many harts it can name.
        let all = (0x3ff << 16) | (0x3ff << 6);
        let kept = self.dmi.run(&[
            Op::Write(reg::DMCONTROL, DMACTIVE | all),
            Op::Read(reg::DMCONTROL),
        ])?[0];
        let hartsel = ((kept >> 16) & 0x3ff) | (((kept >> 6) & 0x3ff) << 10);
        let hartsel_bits = 32 - hartsel.leading_zeros();
        let mut harts = 0;
        let limit = (1u32 << hartsel_bits.min(10)).min(1024);
        while harts < limit {
            let status = self.dmi.run(&[
                Op::Write(
                    reg::DMCONTROL,
                    DMACTIVE | ((harts & 0x3ff) << 16) | (((harts >> 10) & 0x3ff) << 6),
                ),
                Op::Read(reg::DMSTATUS),
            ])?[0];
            if status & ANYNONEXISTENT != 0 {
                break;
            }
            harts += 1;
        }
        self.write(reg::DMCONTROL, self.control(0))?;
        Ok(Info {
            dmstatus: r[0],
            dmcontrol: r[1],
            hartinfo: r[2],
            abstractcs: r[3],
            sbcs: r[4],
            hartsel_bits,
            harts,
        })
    }

    /// Halts the hart. Already halted is fine.
    pub fn halt(&mut self) -> Result<(), String> {
        if self.is_halted()? {
            return Ok(());
        }
        self.write(reg::DMCONTROL, self.control(HALTREQ))?;
        let waited = self.wait("the hart to halt", |dm| dm.is_halted());
        self.write(reg::DMCONTROL, self.control(0))?;
        waited?;
        self.catch_ebreak()
    }

    /// Makes `ebreak` in the program halt into the debugger, as probe-rs does,
    /// so a breakpoint written in the code stops here instead of trapping.
    fn catch_ebreak(&mut self) -> Result<(), String> {
        let dcsr = self.read_reg(Reg::Csr(DCSR))?;
        let want = dcsr | DCSR_EBREAKM | DCSR_EBREAKS | DCSR_EBREAKU;
        if want != dcsr {
            // `ebreaks` / `ebreaku` are WARL: a hart without S or U keeps 0.
            self.write_reg(Reg::Csr(DCSR), want)?;
        }
        Ok(())
    }

    /// Lets the hart run. Halted on a breakpoint, it first steps past it:
    /// the trigger fires before the instruction runs, so it would fire again.
    pub fn resume(&mut self) -> Result<(), String> {
        if self.is_halted()? && self.cause()? == 2 {
            self.step()?;
        }
        self.run()
    }

    fn run(&mut self) -> Result<(), String> {
        self.write(reg::DMCONTROL, self.control(RESUMEREQ))?;
        let waited = self.wait("the hart to resume", |dm| {
            Ok(dm.status()? & (ALLRESUMEACK | ALLRUNNING) != 0)
        });
        self.write(reg::DMCONTROL, self.control(0))?;
        waited
    }

    /// Runs one instruction and halts again. The hart must be halted.
    ///
    /// Halted on a breakpoint, the trigger at the PC is off for the step, or
    /// it would fire before the instruction runs.
    pub fn step(&mut self) -> Result<(), String> {
        self.need_halted("step")?;
        if self.cause()? != 2 {
            return self.step_once();
        }
        let pc = self.read_reg(Reg::Csr(DPC))?;
        let here: Vec<Breakpoint> = self
            .breakpoints()?
            .into_iter()
            .filter(|b| b.addr == pc)
            .collect();
        for b in &here {
            self.arm(b.index, None)?;
        }
        let stepped = self.step_once();
        for b in &here {
            self.arm(b.index, Some(b.addr))?;
        }
        stepped
    }

    fn step_once(&mut self) -> Result<(), String> {
        let dcsr = self.read_reg(Reg::Csr(DCSR))?;
        self.write_reg(Reg::Csr(DCSR), dcsr | DCSR_STEP)?;
        self.write(reg::DMCONTROL, self.control(RESUMEREQ))?;
        let waited = self.wait("the hart to finish the step", |dm| dm.is_halted());
        self.write(reg::DMCONTROL, self.control(0))?;
        if waited.is_err() {
            // It ran away (a WFI, or a step the hart ignored): stop it.
            self.halt()?;
        }
        let dcsr = self.read_reg(Reg::Csr(DCSR))?;
        self.write_reg(Reg::Csr(DCSR), dcsr & !DCSR_STEP)?;
        waited
    }

    /// Resets the hart through the Debug Module (`ndmreset`). With `halt`, it
    /// stops before its first instruction.
    ///
    /// The Debug Module itself is not reset; whatever `ndmreset` resets is up
    /// to the design. The harness memories keep their contents.
    pub fn reset(&mut self, halt: bool) -> Result<(), String> {
        let catch = halt && self.status()? & HASRESETHALTREQ != 0;
        let hold = if !halt {
            0
        } else if catch {
            self.write(reg::DMCONTROL, self.control(SETRESETHALTREQ))?;
            0
        } else {
            // No halt-on-reset: keep asking to halt through the reset. The
            // hart may run a few instructions first.
            HALTREQ
        };
        self.write(reg::DMCONTROL, self.control(NDMRESET | hold))?;
        self.write(reg::DMCONTROL, self.control(hold))?;
        self.wait("the reset to finish", |dm| {
            let s = dm.status()?;
            Ok(s & NDMRESETPENDING == 0 && s & ALLHAVERESET != 0)
        })?;
        if halt {
            self.wait("the hart to halt after the reset", |dm| dm.is_halted())?;
        }
        let mut done = ACKHAVERESET;
        if catch {
            done |= CLRRESETHALTREQ;
        }
        self.write(reg::DMCONTROL, self.control(done))?;
        self.write(reg::DMCONTROL, self.control(0))?;
        if halt {
            self.catch_ebreak()?;
        }
        Ok(())
    }

    /// Asks the hart to halt at its next reset, whatever resets it, or stops
    /// asking.
    pub fn halt_on_reset(&mut self, on: bool) -> Result<(), String> {
        if on && self.status()? & HASRESETHALTREQ == 0 {
            return Err(
                "the Debug Module cannot halt a hart at reset: it has no halt-on-reset \
                 (`dmstatus.hasresethaltreq` is 0)."
                    .to_string(),
            );
        }
        let bit = if on { SETRESETHALTREQ } else { CLRRESETHALTREQ };
        self.write(reg::DMCONTROL, self.control(bit))?;
        self.write(reg::DMCONTROL, self.control(0))
    }

    /// Waits for a halt that something else brings, such as a reset with
    /// halt-on-reset, and acknowledges the reset.
    pub fn wait_halted(&mut self) -> Result<(), String> {
        self.wait("the hart to halt", |dm| dm.is_halted())?;
        self.write(reg::DMCONTROL, self.control(ACKHAVERESET))?;
        self.write(reg::DMCONTROL, self.control(0))?;
        self.catch_ebreak()
    }

    fn need_halted(&mut self, what: &str) -> Result<(), String> {
        if self.is_halted()? {
            return Ok(());
        }
        Err(format!(
            "the hart is running, and {what} needs it halted. Halt it first: `hio cpu halt`."
        ))
    }

    /// Runs an abstract command and waits for it.
    fn command(&mut self, command: u32) -> Result<(), CmdErr> {
        let run = |dm: &mut Self| -> Result<u32, String> {
            let r = dm
                .dmi
                .run(&[Op::Write(reg::COMMAND, command), Op::Read(reg::ABSTRACTCS)])?;
            let mut cs = r[0];
            let mut polls = 0;
            while cs & BUSY != 0 {
                polls += 1;
                if polls > POLLS {
                    return Err("timed out waiting for an abstract command".to_string());
                }
                cs = dm.read(reg::ABSTRACTCS)?;
            }
            Ok(cs)
        };
        // A transport error is not a command error; report it as "other".
        let cs = run(self).map_err(|_| CmdErr(7))?;
        let cmderr = (cs >> 8) & 0x7;
        if cmderr != 0 {
            let _ = self.write(reg::ABSTRACTCS, CMDERR_CLEAR);
            return Err(CmdErr(cmderr));
        }
        Ok(())
    }

    /// The access register command for `regno`.
    fn access(&self, regno: u32, write: bool, transfer: bool, postexec: bool, xlen: u32) -> u32 {
        let aarsize = if xlen == 64 { 3 } else { 2 };
        (aarsize << 20)
            | (u32::from(postexec) << 18)
            | (u32::from(transfer) << 17)
            | (u32::from(write) << 16)
            | regno
    }

    /// XLEN, learnt by asking for a 64-bit `s0` and falling back to 32. The
    /// hart must be halted the first time.
    pub fn xlen(&mut self) -> Result<u32, String> {
        if let Some(xlen) = self.xlen {
            return Ok(xlen);
        }
        let xlen = match self.command(self.access(0x1000 + S0, false, true, false, 64)) {
            Ok(()) => 64,
            Err(CmdErr(2)) => {
                match self.command(self.access(0x1000 + S0, false, true, false, 32)) {
                    Ok(()) => 32,
                    Err(CmdErr(e)) => {
                        return Err(format!(
                            "cannot read a register of hart {}: {}",
                            self.hart,
                            cmderr_text(e)
                        ));
                    }
                }
            }
            Err(CmdErr(e)) => {
                return Err(format!(
                    "cannot read a register of hart {}: {}",
                    self.hart,
                    cmderr_text(e)
                ));
            }
        };
        self.xlen = Some(xlen);
        Ok(xlen)
    }

    fn read_data(&mut self, xlen: u32) -> Result<u64, String> {
        if xlen == 64 {
            let r = self
                .dmi
                .run(&[Op::Read(reg::DATA0), Op::Read(reg::DATA1)])?;
            Ok(u64::from(r[0]) | (u64::from(r[1]) << 32))
        } else {
            Ok(u64::from(self.read(reg::DATA0)?))
        }
    }

    fn write_data(&mut self, xlen: u32, value: u64) -> Result<(), String> {
        let mut ops = vec![Op::Write(reg::DATA0, value as u32)];
        if xlen == 64 {
            ops.push(Op::Write(reg::DATA1, (value >> 32) as u32));
        }
        self.dmi.run(&ops).map(|_| ())
    }

    /// Reads a register. The hart must be halted.
    pub fn read_reg(&mut self, reg: Reg) -> Result<u64, String> {
        self.need_halted("reading a register")?;
        let xlen = self.xlen()?;
        if let Reg::Csr(csr) = reg
            && self.csr_by_progbuf
        {
            return self.read_csr_by_progbuf(csr, xlen);
        }
        match self.command(self.access(reg.regno(), false, true, false, xlen)) {
            Ok(()) => self.read_data(xlen),
            Err(CmdErr(2)) if matches!(reg, Reg::Csr(_)) => {
                self.csr_by_progbuf = true;
                let Reg::Csr(csr) = reg else { unreachable!() };
                self.read_csr_by_progbuf(csr, xlen)
            }
            Err(CmdErr(e)) => Err(format!("cannot read {reg}: {}", cmderr_text(e))),
        }
    }

    /// Writes a register. The hart must be halted.
    pub fn write_reg(&mut self, reg: Reg, value: u64) -> Result<(), String> {
        self.need_halted("writing a register")?;
        let xlen = self.xlen()?;
        if let Reg::Csr(csr) = reg
            && self.csr_by_progbuf
        {
            return self.write_csr_by_progbuf(csr, value, xlen);
        }
        self.write_data(xlen, value)?;
        match self.command(self.access(reg.regno(), true, true, false, xlen)) {
            Ok(()) => Ok(()),
            Err(CmdErr(2)) if matches!(reg, Reg::Csr(_)) => {
                self.csr_by_progbuf = true;
                let Reg::Csr(csr) = reg else { unreachable!() };
                self.write_csr_by_progbuf(csr, value, xlen)
            }
            Err(CmdErr(e)) => Err(format!("cannot write {reg}: {}", cmderr_text(e))),
        }
    }

    /// Puts `insn` and an `ebreak` in the program buffer.
    fn load_program(&mut self, insn: u32) -> Result<(), String> {
        if self.progbuf_size == 0 || (self.progbuf_size == 1 && !self.impebreak) {
            return Err(
                "the Debug Module cannot reach CSRs: the access register command refuses them, \
                 and it has no program buffer to run `csrr` in."
                    .to_string(),
            );
        }
        let mut ops = vec![Op::Write(reg::PROGBUF0, insn)];
        if self.progbuf_size >= 2 {
            ops.push(Op::Write(reg::PROGBUF0 + 1, EBREAK));
        }
        self.dmi.run(&ops).map(|_| ())
    }

    /// `csrr s0, csr`, run in the program buffer, with `s0` kept.
    fn read_csr_by_progbuf(&mut self, csr: u32, xlen: u32) -> Result<u64, String> {
        let s0 = self.read_reg(Reg::Gpr(S0))?;
        // csrrs s0, csr, x0
        self.load_program((csr << 20) | (0b010 << 12) | (S0 << 7) | 0x73)?;
        let ran = self
            .command(self.access(0, false, false, true, xlen))
            .map_err(|CmdErr(e)| format!("cannot read csr {csr:#05x}: {}", cmderr_text(e)));
        let value = match ran {
            Ok(()) => self.read_reg(Reg::Gpr(S0)),
            Err(e) => Err(e),
        };
        self.write_reg(Reg::Gpr(S0), s0)?;
        value
    }

    /// `csrw csr, s0`, run in the program buffer, with `s0` kept.
    fn write_csr_by_progbuf(&mut self, csr: u32, value: u64, xlen: u32) -> Result<(), String> {
        let s0 = self.read_reg(Reg::Gpr(S0))?;
        // csrrw x0, csr, s0
        self.load_program((csr << 20) | (S0 << 15) | (0b001 << 12) | 0x73)?;
        self.write_data(xlen, value)?;
        // Write s0, then run the program.
        let ran = self
            .command(self.access(0x1000 + S0, true, true, true, xlen))
            .map_err(|CmdErr(e)| format!("cannot write csr {csr:#05x}: {}", cmderr_text(e)));
        self.write_reg(Reg::Gpr(S0), s0)?;
        ran
    }

    /// Runs `f` with the hart halted, and lets it run again afterwards if it
    /// was running.
    fn halted_for<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, String> {
        let was_running = !self.is_halted()?;
        if was_running {
            self.halt()?;
        }
        let out = f(self);
        if was_running {
            self.run()?;
        }
        out
    }

    /// The breakpoints set: address triggers that halt into the debugger on
    /// execute. A running hart is halted for a moment to read them.
    pub fn breakpoints(&mut self) -> Result<Vec<Breakpoint>, String> {
        self.halted_for(|dm| {
            let xlen = dm.xlen()?;
            let saved = dm.read_reg(Reg::Csr(TSELECT))?;
            let mut out = Vec::new();
            for t in dm
                .triggers()?
                .into_iter()
                .filter(Trigger::matches_addresses)
            {
                dm.write_reg(Reg::Csr(TSELECT), u64::from(t.index))?;
                if is_breakpoint(dm.read_reg(Reg::Csr(TDATA1))?, xlen) {
                    let addr = dm.read_reg(Reg::Csr(TDATA2))?;
                    out.push(Breakpoint {
                        index: t.index,
                        addr,
                    });
                }
            }
            dm.write_reg(Reg::Csr(TSELECT), saved)?;
            Ok(out)
        })
    }

    /// Sets a breakpoint at `addr` on a free address trigger. One already
    /// there is kept.
    pub fn set_breakpoint(&mut self, addr: u64) -> Result<Breakpoint, String> {
        self.halted_for(|dm| {
            if let Some(b) = dm.breakpoints()?.into_iter().find(|b| b.addr == addr) {
                return Ok(b);
            }
            let saved = dm.read_reg(Reg::Csr(TSELECT))?;
            let mut free = None;
            let mut total = 0;
            for t in dm
                .triggers()?
                .into_iter()
                .filter(Trigger::matches_addresses)
            {
                total += 1;
                dm.write_reg(Reg::Csr(TSELECT), u64::from(t.index))?;
                let tdata1 = dm.read_reg(Reg::Csr(TDATA1))?;
                if tdata1 & (T_EXECUTE | T_LOAD | T_STORE) == 0 {
                    free = Some(t.index);
                    break;
                }
            }
            dm.write_reg(Reg::Csr(TSELECT), saved)?;
            let Some(index) = free else {
                return Err(if total == 0 {
                    "the hart has no address triggers, so it cannot take a hardware breakpoint."
                        .to_string()
                } else {
                    format!(
                        "all {total} address triggers are in use. Clear one first: \
                         `hio cpu break --clear <where>`."
                    )
                });
            };
            dm.arm(index, Some(addr))?;
            Ok(Breakpoint { index, addr })
        })
    }

    /// Clears the breakpoint at `addr`, or every one. Returns how many.
    pub fn clear_breakpoints(&mut self, addr: Option<u64>) -> Result<usize, String> {
        self.halted_for(|dm| {
            let found: Vec<Breakpoint> = dm
                .breakpoints()?
                .into_iter()
                .filter(|b| addr.is_none_or(|a| a == b.addr))
                .collect();
            for b in &found {
                dm.arm(b.index, None)?;
            }
            Ok(found.len())
        })
    }

    /// Points trigger `index` at `addr`, or turns it off with `None`.
    /// `tselect` is put back.
    fn arm(&mut self, index: u32, addr: Option<u64>) -> Result<(), String> {
        let xlen = self.xlen()?;
        let saved = self.read_reg(Reg::Csr(TSELECT))?;
        self.write_reg(Reg::Csr(TSELECT), u64::from(index))?;
        let kind = self.read_reg(Reg::Csr(TDATA1))? >> (xlen - 4);
        let off = kind << (xlen - 4);
        // Off first, so the trigger never matches a half-written address.
        self.write_reg(Reg::Csr(TDATA1), off)?;
        let result = match addr {
            None => Ok(()),
            Some(addr) => {
                self.write_reg(Reg::Csr(TDATA2), addr)?;
                let want = off | (1 << (xlen - 5)) | T_ACTION_DEBUG | T_M | T_S | T_U | T_EXECUTE;
                self.write_reg(Reg::Csr(TDATA1), want)?;
                // `s` and `u` are WARL: a hart without them keeps 0.
                if is_breakpoint(self.read_reg(Reg::Csr(TDATA1))?, xlen) {
                    Ok(())
                } else {
                    Err(format!(
                        "trigger {index} did not take the breakpoint: it may not halt into \
                         the debugger, or not match on execute."
                    ))
                }
            }
        };
        self.write_reg(Reg::Csr(TSELECT), saved)?;
        result
    }

    /// Points the exception trigger at `causes` (a bitmap of `mcause`
    /// values), or turns it off with 0. Returns the causes the hart took:
    /// it keeps only those it can raise. A running hart is halted for a
    /// moment.
    pub fn catch_exceptions(&mut self, causes: u64) -> Result<u64, String> {
        self.halted_for(|dm| {
            let xlen = dm.xlen()?;
            let Some(t) = dm.triggers()?.into_iter().find(Trigger::matches_exceptions) else {
                return Err(
                    "the hart has no exception trigger, so it cannot stop on a trap.".to_string(),
                );
            };
            let saved = dm.read_reg(Reg::Csr(TSELECT))?;
            dm.write_reg(Reg::Csr(TSELECT), u64::from(t.index))?;
            let off = 5 << (xlen - 4);
            dm.write_reg(Reg::Csr(TDATA1), off)?;
            let taken = if causes == 0 {
                0
            } else {
                dm.write_reg(Reg::Csr(TDATA2), causes)?;
                // `m` / `s` / `u` are WARL: a hart without S or U keeps 0.
                let want = off | (1 << (xlen - 5)) | E_M | E_S | E_U | E_ACTION_DEBUG;
                dm.write_reg(Reg::Csr(TDATA1), want)?;
                dm.read_reg(Reg::Csr(TDATA2))?
            };
            dm.write_reg(Reg::Csr(TSELECT), saved)?;
            Ok(taken)
        })
    }

    /// The causes the exception trigger is set to catch, or 0.
    pub fn caught_exceptions(&mut self) -> Result<u64, String> {
        self.halted_for(|dm| {
            let xlen = dm.xlen()?;
            let Some(t) = dm.triggers()?.into_iter().find(Trigger::matches_exceptions) else {
                return Ok(0);
            };
            let saved = dm.read_reg(Reg::Csr(TSELECT))?;
            dm.write_reg(Reg::Csr(TSELECT), u64::from(t.index))?;
            let tdata1 = dm.read_reg(Reg::Csr(TDATA1))?;
            let armed = tdata1 & (1 << (xlen - 5)) != 0 && tdata1 & (E_M | E_S | E_U) != 0;
            let causes = if armed {
                dm.read_reg(Reg::Csr(TDATA2))?
            } else {
                0
            };
            dm.write_reg(Reg::Csr(TSELECT), saved)?;
            Ok(causes)
        })
    }

    /// Why the hart is halted, telling a breakpoint from a caught trap. The
    /// hart must be halted.
    pub fn stop(&mut self) -> Result<Stop, String> {
        Ok(match self.cause()? {
            1 => Stop::Ebreak,
            3 => Stop::Request,
            4 => Stop::Step,
            5 => Stop::Reset,
            2 => {
                let pc = self.read_reg(Reg::Csr(DPC))?;
                if self.breakpoints()?.iter().any(|b| b.addr == pc) {
                    Stop::Breakpoint
                } else {
                    let mcause = self.read_reg(Reg::Csr(MCAUSE))?;
                    let interrupt = mcause >> (self.xlen()? - 1) != 0;
                    let caught = self.caught_exceptions()?;
                    if !interrupt && mcause < 64 && caught & (1 << mcause) != 0 {
                        Stop::Trap {
                            cause: mcause,
                            mepc: self.read_reg(Reg::Csr(MEPC))?,
                            mtval: self.read_reg(Reg::Csr(MTVAL))?,
                        }
                    } else {
                        Stop::Trigger
                    }
                }
            }
            other => Stop::Other(other),
        })
    }

    /// Why the hart halted. The hart must be halted.
    pub fn cause(&mut self) -> Result<u64, String> {
        Ok((self.read_reg(Reg::Csr(DCSR))? >> 6) & 0x7)
    }

    /// The triggers, as `tselect` and `tinfo` list them. The hart must be
    /// halted. `tselect` is put back.
    pub fn triggers(&mut self) -> Result<Vec<Trigger>, String> {
        self.need_halted("listing the triggers")?;
        let xlen = self.xlen()?;
        let saved = self.read_reg(Reg::Csr(TSELECT))?;
        let mut out = Vec::new();
        for index in 0..64 {
            self.write_reg(Reg::Csr(TSELECT), u64::from(index))?;
            if self.read_reg(Reg::Csr(TSELECT))? != u64::from(index) {
                break;
            }
            let types = match self.read_reg(Reg::Csr(TINFO)) {
                Ok(tinfo) => (tinfo & 0xffff) as u32,
                // Without `tinfo`, the type in `tdata1` is the only one.
                Err(_) => 1 << (self.read_reg(Reg::Csr(TDATA1))? >> (xlen - 4)),
            };
            // Type 0 is "no trigger here"; 1 in `tinfo` means the same.
            if types == 0 || types == 1 {
                break;
            }
            out.push(Trigger { index, types });
        }
        self.write_reg(Reg::Csr(TSELECT), saved)?;
        Ok(out)
    }
}

/// An address trigger set by a debugger to halt on execute.
fn is_breakpoint(tdata1: u64, xlen: u32) -> bool {
    let kind = tdata1 >> (xlen - 4);
    (kind == 2 || kind == 6)
        && tdata1 & (1 << (xlen - 5)) != 0
        && tdata1 & T_EXECUTE != 0
        && (tdata1 >> 12) & 0xf == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Debug Module and one hart, enough to run the debugger's sequences:
    /// halt / resume / step, access register for GPRs only, and a program
    /// buffer that runs `csrr` / `csrw` on a small CSR file. Like Hazard3, it
    /// has no abstract CSR access, so CSRs take the program buffer path.
    #[derive(Default)]
    struct Model {
        active: bool,
        halted: bool,
        resumeack: bool,
        havereset: bool,
        resethaltreq: bool,
        cmderr: u32,
        data: [u32; 2],
        progbuf: [u32; 2],
        gpr: [u64; 32],
        csr: std::collections::HashMap<u32, u64>,
        pc: u64,
        xlen: u32,
        /// Access register commands that named a CSR, which it refuses.
        refused_csr: usize,
        /// `tdata1` / `tdata2` of the triggers: an address match, an
        /// instruction count, and an exception trigger.
        tdata: [[u64; 2]; 3],
        log: Vec<Op>,
    }

    impl Model {
        fn new(xlen: u32) -> Self {
            let mut m = Model {
                xlen,
                ..Model::default()
            };
            m.csr.insert(0x301, 0x4000_1105);
            m.csr.insert(DCSR, 4 << 28);
            let shift = xlen - 4;
            m.tdata = [[2 << shift, 0], [3 << shift, 0], [5 << shift, 0]];
            m.csr.insert(TSELECT, 0);
            m
        }

        fn mask(&self, v: u64) -> u64 {
            if self.xlen == 64 { v } else { v & 0xffff_ffff }
        }

        fn execute(&mut self, insn: u32) {
            let csr = insn >> 20;
            let rd = (insn >> 7) & 0x1f;
            let rs1 = (insn >> 15) & 0x1f;
            match (insn >> 12) & 0x7 {
                // csrrs rd, csr, x0
                0b010 if insn & 0x7f == 0x73 => {
                    let sel = (self.csr[&TSELECT] as usize).min(3);
                    let v = match csr {
                        DPC => self.pc,
                        TDATA1 | TDATA2 if sel < 3 => self.tdata[sel][(csr - TDATA1) as usize],
                        _ => *self.csr.get(&csr).unwrap_or(&0),
                    };
                    self.gpr[rd as usize] = v;
                }
                // csrrw x0, csr, rs1
                0b001 if insn & 0x7f == 0x73 => {
                    let v = self.gpr[rs1 as usize];
                    let sel = (self.csr[&TSELECT] as usize).min(3);
                    if csr == DPC {
                        self.pc = v;
                    } else if csr == TDATA1 && sel < 3 {
                        // The type is fixed.
                        let shift = self.xlen - 4;
                        let kind = self.tdata[sel][0] >> shift;
                        self.tdata[sel][0] = (kind << shift) | (v & ((1 << shift) - 1));
                    } else if csr == TDATA2 && sel < 3 {
                        // The exception trigger keeps the causes this hart
                        // can raise (not 0, as with compressed instructions).
                        self.tdata[sel][1] = if sel == 2 { v & 0xb0f6 } else { v };
                    } else if csr == TSELECT {
                        // Three triggers.
                        self.csr.insert(TSELECT, v.min(3));
                    } else {
                        self.csr.insert(csr, v);
                    }
                }
                _ if insn == EBREAK => {}
                _ => self.cmderr = 3,
            }
            if csr == TINFO && (insn >> 12) & 0x7 == 0b010 {
                let sel = self.csr[&TSELECT];
                self.gpr[rd as usize] = match sel {
                    0 => 1 << 2,
                    1 => 1 << 3,
                    2 => 1 << 5,
                    _ => 1,
                };
            }
        }

        fn command(&mut self, cmd: u32) {
            if self.cmderr != 0 {
                return;
            }
            if !self.halted {
                self.cmderr = 4;
                return;
            }
            let size = (cmd >> 20) & 0x7;
            let want = if self.xlen == 64 { 3 } else { 2 };
            let transfer = cmd & (1 << 17) != 0;
            if transfer && size != want {
                self.cmderr = 2;
                return;
            }
            let regno = cmd & 0xffff;
            if transfer {
                if !(0x1000..0x1020).contains(&regno) {
                    self.refused_csr += 1;
                    self.cmderr = 2;
                    return;
                }
                let n = (regno - 0x1000) as usize;
                if cmd & (1 << 16) != 0 {
                    let v = u64::from(self.data[0]) | (u64::from(self.data[1]) << 32);
                    if n != 0 {
                        self.gpr[n] = self.mask(v);
                    }
                } else {
                    let v = self.gpr[n];
                    self.data = [v as u32, (v >> 32) as u32];
                }
            }
            if cmd & (1 << 18) != 0 {
                for insn in self.progbuf {
                    self.execute(insn);
                }
            }
        }

        fn dmstatus(&self) -> u32 {
            let mut s = 2 | HASRESETHALTREQ | IMPEBREAK;
            if self.halted {
                s |= ALLHALTED | (1 << 8);
            } else {
                s |= ALLRUNNING | (1 << 10);
            }
            if self.resumeack {
                s |= ALLRESUMEACK;
            }
            if self.havereset {
                s |= ALLHAVERESET;
            }
            s
        }

        fn dmcontrol(&mut self, v: u32) {
            self.active = v & DMACTIVE != 0;
            if v & SETRESETHALTREQ != 0 {
                self.resethaltreq = true;
            }
            if v & CLRRESETHALTREQ != 0 {
                self.resethaltreq = false;
            }
            if v & ACKHAVERESET != 0 {
                self.havereset = false;
            }
            if v & NDMRESET != 0 {
                self.pc = 0x8000_0000;
                self.havereset = true;
                self.halted = self.resethaltreq;
                if self.halted {
                    self.csr.insert(DCSR, (4 << 28) | (5 << 6));
                }
            }
            if v & HALTREQ != 0 && !self.halted {
                self.halted = true;
                let dcsr = self.csr[&DCSR] & !(0x7 << 6);
                self.csr.insert(DCSR, dcsr | (3 << 6));
            }
            if v & RESUMEREQ != 0 && self.halted {
                self.resumeack = true;
                let dcsr = self.csr[&DCSR] & !(0x7 << 6);
                // The address trigger fires before the instruction at the PC.
                if is_breakpoint(self.tdata[0][0], self.xlen) && self.tdata[0][1] == self.pc {
                    self.csr.insert(DCSR, dcsr | (2 << 6));
                } else if self.csr[&DCSR] & DCSR_STEP != 0 {
                    self.pc += 4;
                    self.csr.insert(DCSR, dcsr | (4 << 6));
                } else {
                    self.halted = false;
                }
            }
        }
    }

    impl Dmi for &mut Model {
        fn run(&mut self, ops: &[Op]) -> Result<Vec<u32>, String> {
            let mut out = Vec::new();
            for op in ops {
                self.log.push(*op);
                match *op {
                    Op::Read(r) => out.push(match r {
                        reg::DATA0 => self.data[0],
                        reg::DATA1 => self.data[1],
                        reg::DMCONTROL => u32::from(self.active),
                        reg::DMSTATUS => self.dmstatus(),
                        reg::ABSTRACTCS => (2 << 24) | (self.cmderr << 8) | 2,
                        reg::HARTINFO => 0x0010_0bff,
                        _ => 0,
                    }),
                    Op::Write(r, v) => match r {
                        reg::DATA0 => self.data[0] = v,
                        reg::DATA1 => self.data[1] = v,
                        reg::DMCONTROL => self.dmcontrol(v),
                        reg::ABSTRACTCS => self.cmderr &= !((v >> 8) & 0x7),
                        reg::COMMAND => self.command(v),
                        r if r == reg::PROGBUF0 || r == reg::PROGBUF0 + 1 => {
                            self.progbuf[(r - reg::PROGBUF0) as usize] = v;
                        }
                        _ => {}
                    },
                }
            }
            Ok(out)
        }
    }

    #[test]
    fn attaching_turns_the_module_on() {
        let mut m = Model::new(32);
        Debug::attach(&mut m, 0).unwrap();
        assert!(m.active);
    }

    #[test]
    fn registers_need_a_halted_hart() {
        let mut m = Model::new(32);
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        let err = dm.read_reg(Reg::Gpr(10)).unwrap_err();
        assert!(err.contains("hio cpu halt"), "{err}");
    }

    #[test]
    fn halting_reads_gprs_and_takes_csrs_through_the_program_buffer() {
        let mut m = Model::new(32);
        m.gpr[8] = 0x1111;
        m.gpr[10] = 0xa0a0;
        m.pc = 0x8000_0040;
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        dm.halt().unwrap();
        assert_eq!(dm.read_reg(Reg::Gpr(10)).unwrap(), 0xa0a0);
        assert_eq!(dm.read_reg(Reg::parse("pc").unwrap()).unwrap(), 0x8000_0040);
        assert_eq!(
            dm.read_reg(Reg::parse("misa").unwrap()).unwrap(),
            0x4000_1105
        );
        assert_eq!(dm.cause().unwrap(), 3);
        dm.write_reg(Reg::parse("pc").unwrap(), 0x8000_0100)
            .unwrap();
        assert_eq!(m.pc, 0x8000_0100);
        // s0 carried the values and was put back.
        assert_eq!(m.gpr[8], 0x1111);
        // ebreak halts into the debugger from now on.
        assert_ne!(m.csr[&DCSR] & DCSR_EBREAKM, 0);
        // The first CSR access found out; later ones go straight to the
        // program buffer.
        assert_eq!(m.refused_csr, 1);
    }

    #[test]
    fn a_64_bit_hart_is_found_and_read_whole() {
        let mut m = Model::new(64);
        m.gpr[5] = 0x1234_5678_9abc_def0;
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        dm.halt().unwrap();
        assert_eq!(dm.read_reg(Reg::Gpr(5)).unwrap(), 0x1234_5678_9abc_def0);
    }

    #[test]
    fn a_step_moves_one_instruction_and_clears_the_step_bit() {
        let mut m = Model::new(32);
        m.pc = 0x8000_0000;
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        dm.halt().unwrap();
        dm.step().unwrap();
        assert_eq!(dm.read_reg(Reg::Csr(DPC)).unwrap(), 0x8000_0004);
        assert_eq!(dm.cause().unwrap(), 4);
        assert_eq!(m.csr[&DCSR] & DCSR_STEP, 0);
        assert!(m.halted);
    }

    #[test]
    fn resume_lets_the_hart_run() {
        let mut m = Model::new(32);
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        dm.halt().unwrap();
        dm.resume().unwrap();
        assert!(!dm.is_halted().unwrap());
    }

    #[test]
    fn a_reset_with_halt_stops_at_the_first_instruction() {
        let mut m = Model::new(32);
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        dm.reset(true).unwrap();
        assert!(dm.is_halted().unwrap());
        assert_eq!(dm.cause().unwrap(), 5);
        assert_eq!(dm.read_reg(Reg::Csr(DPC)).unwrap(), 0x8000_0000);
        assert!(!m.resethaltreq, "the request is cleared afterwards");
        assert!(!m.havereset, "the reset is acknowledged");
    }

    #[test]
    fn the_triggers_are_listed_and_tselect_is_put_back() {
        let mut m = Model::new(32);
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        dm.halt().unwrap();
        let triggers = dm.triggers().unwrap();
        assert_eq!(
            triggers,
            vec![
                Trigger {
                    index: 0,
                    types: 1 << 2
                },
                Trigger {
                    index: 1,
                    types: 1 << 3
                },
                Trigger {
                    index: 2,
                    types: 1 << 5
                },
            ]
        );
        assert_eq!(triggers[0].type_names(), vec!["mcontrol (address/data)"]);
        assert_eq!(m.csr[&TSELECT], 0);
    }

    #[test]
    fn a_breakpoint_is_set_listed_stepped_over_and_cleared() {
        let mut m = Model::new(32);
        m.pc = 0x8000_0000;
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        // Set while running: halted for a moment, then running again.
        let b = dm.set_breakpoint(0x8000_0010).unwrap();
        assert_eq!(
            b,
            Breakpoint {
                index: 0,
                addr: 0x8000_0010
            }
        );
        assert!(!dm.is_halted().unwrap());
        assert_eq!(dm.breakpoints().unwrap(), vec![b]);
        // The same address again is the same breakpoint.
        assert_eq!(dm.set_breakpoint(0x8000_0010).unwrap(), b);
        // The one address trigger is taken.
        let err = dm.set_breakpoint(0x8000_0020).unwrap_err();
        assert!(err.contains("all 1 address triggers"), "{err}");

        // Halted on it, a step runs the instruction instead of firing again,
        // and the breakpoint is back afterwards.
        dm.halt().unwrap();
        dm.write_reg(Reg::Csr(DPC), 0x8000_0010).unwrap();
        let dcsr = dm.read_reg(Reg::Csr(DCSR)).unwrap() & !(0x7 << 6);
        dm.write_reg(Reg::Csr(DCSR), dcsr | (2 << 6)).unwrap();
        dm.step().unwrap();
        assert_eq!(dm.read_reg(Reg::Csr(DPC)).unwrap(), 0x8000_0014);
        assert_eq!(dm.breakpoints().unwrap(), vec![b]);

        // Resuming from it does the same, then runs.
        dm.write_reg(Reg::Csr(DPC), 0x8000_0010).unwrap();
        let dcsr = dm.read_reg(Reg::Csr(DCSR)).unwrap() & !(0x7 << 6);
        dm.write_reg(Reg::Csr(DCSR), dcsr | (2 << 6)).unwrap();
        dm.resume().unwrap();
        assert!(!dm.is_halted().unwrap());

        assert_eq!(dm.clear_breakpoints(Some(0x8000_0010)).unwrap(), 1);
        assert!(dm.breakpoints().unwrap().is_empty());
        assert_eq!(dm.clear_breakpoints(None).unwrap(), 0);
    }

    /// `boot --halt`: the request is set before something else resets the
    /// hart, the hart stops there, and the request and the reset are
    /// cleared afterwards.
    #[test]
    fn a_halt_on_reset_catches_a_reset_from_outside() {
        let mut m = Model::new(32);
        Debug::attach(&mut m, 0)
            .unwrap()
            .halt_on_reset(true)
            .unwrap();
        assert!(m.resethaltreq);
        // The DUT reset, which the Debug Module does not see coming.
        m.pc = 0x8000_0000;
        m.havereset = true;
        m.halted = m.resethaltreq;
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        dm.wait_halted().unwrap();
        dm.halt_on_reset(false).unwrap();
        assert_eq!(dm.read_reg(Reg::Csr(DPC)).unwrap(), 0x8000_0000);
        assert!(!m.resethaltreq);
        assert!(!m.havereset);
    }

    /// The exception trigger takes the faults the hart can raise, and a halt
    /// it causes reads as a trap with where it happened.
    #[test]
    fn a_caught_trap_is_told_from_a_breakpoint() {
        let mut m = Model::new(32);
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        assert_eq!(dm.caught_exceptions().unwrap(), 0);
        let taken = dm.catch_exceptions(FAULTS).unwrap();
        assert_eq!(taken, FAULTS & 0xb0f6);
        assert_eq!(dm.caught_exceptions().unwrap(), taken);
        assert!(
            !dm.is_halted().unwrap(),
            "set while running, then running again"
        );

        // An illegal instruction at 0x8000_0040: the hart stops at the
        // handler.
        dm.halt().unwrap();
        dm.write_reg(Reg::Csr(MCAUSE), 2).unwrap();
        dm.write_reg(Reg::Csr(MEPC), 0x8000_0040).unwrap();
        dm.write_reg(Reg::Csr(MTVAL), 0xffff_ffff).unwrap();
        dm.write_reg(Reg::Csr(DPC), 0x8000_00f8).unwrap();
        let dcsr = dm.read_reg(Reg::Csr(DCSR)).unwrap() & !(0x7 << 6);
        dm.write_reg(Reg::Csr(DCSR), dcsr | (2 << 6)).unwrap();
        assert_eq!(
            dm.stop().unwrap(),
            Stop::Trap {
                cause: 2,
                mepc: 0x8000_0040,
                mtval: 0xffff_ffff
            }
        );
        // A breakpoint at the PC is a breakpoint, whatever `mcause` says.
        dm.set_breakpoint(0x8000_00f8).unwrap();
        assert_eq!(dm.stop().unwrap(), Stop::Breakpoint);

        assert_eq!(dm.catch_exceptions(0).unwrap(), 0);
        assert_eq!(dm.caught_exceptions().unwrap(), 0);
    }

    #[test]
    fn info_reads_without_halting() {
        let mut m = Model::new(32);
        let mut dm = Debug::attach(&mut m, 0).unwrap();
        let info = dm.info().unwrap();
        assert_eq!(info.version(), "0.13");
        assert_eq!(info.progbuf_size(), 2);
        assert!(info.impebreak());
        assert!(info.has_reset_halt());
        let text = info.to_string();
        assert!(text.contains("program buffer 2 words"), "{text}");
        assert!(!m.halted);
    }

    #[test]
    fn names_parse_as_people_type_them() {
        assert_eq!(Reg::parse("a0"), Some(Reg::Gpr(10)));
        assert_eq!(Reg::parse("X31"), Some(Reg::Gpr(31)));
        assert_eq!(Reg::parse("fp"), Some(Reg::Gpr(8)));
        assert_eq!(Reg::parse("pc"), Some(Reg::Csr(DPC)));
        assert_eq!(Reg::parse("mcause"), Some(Reg::Csr(0x342)));
        assert_eq!(Reg::parse("0x7a0"), Some(Reg::Csr(0x7a0)));
        // Names beyond the ones `regs` shows.
        assert_eq!(Reg::parse("mcountinhibit"), Some(Reg::Csr(0x320)));
        assert_eq!(Reg::parse("mcycle"), Some(Reg::Csr(0xb00)));
        assert_eq!(Reg::parse("pmpaddr3"), Some(Reg::Csr(0x3b3)));
        assert_eq!(Reg::Csr(0x320).to_string(), "mcountinhibit");
        assert_eq!(Reg::parse("x32"), None);
        assert_eq!(Reg::parse("nope"), None);
        assert_eq!(reg::parse("dmstatus"), Some(0x11));
        assert_eq!(reg::parse("0x40"), Some(0x40));
        assert_eq!(reg::parse("0x80"), None);
        assert_eq!(Reg::Gpr(2).to_string(), "sp");
        assert_eq!(Reg::Csr(0x7b1).to_string(), "pc");
        assert_eq!(Reg::Csr(0x123).to_string(), "csr0x123");
    }
}

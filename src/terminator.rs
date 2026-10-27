//! Terminator resolution: for each bundle, decide the circuit the harness adds.
//!
//! `reg` is one register per port, so `regmap` alone handles it. The other
//! backings add a circuit to the harness: the FIFO of `host_poll_fifo`
//! (`resolve`), fixed-latency memories (`resolve_memories`), the transfer-level
//! interface (`resolve_host_mem`), slaves inside the DUT (`resolve_slaves`,
//! `resolve_apbs`, `resolve_axi_slaves`), AXI4 memories (`resolve_axi_mems`),
//! and the PCIe lines (`resolve_irqs`, `resolve_flrs`). `check_claimed` checks
//! that each bundle is taken by exactly one of them.
//!
//! ## What `host_poll_fifo` terminates
//!
//! Only an output stream of the DUT: it is a FIFO in the FPGA that the host
//! reads by polling. The other direction (host -> FIFO -> DUT input) is not
//! built. It gets a clear error, so that nothing different is generated
//! silently.

use miette::Diagnostic;
use thiserror::Error;

use crate::bundle::{Binding, DirectionPrefixes};
use crate::contract::BundleContract;
use crate::dut::{Dut, Port, PortDirection};
use crate::manifest::{Addressing, Backing, Contract, Manifest, Role, TraceFormat};

/// The default number of entries. It is readable on the board from the
/// `<bundle>_depth` register, so a default never takes effect unseen.
pub const DEFAULT_DEPTH: u32 = 256;

/// The default number of words in the BRAM behind AXI4. It is far smaller than
/// a real DDR, so set `depth` for a test where size matters. This value is also
/// readable from the `<bundle>_depth` register.
pub const DEFAULT_AXI_MEM_DEPTH: u32 = 1024;

/// The resolved plan of one `host_poll_fifo` bundle.
#[derive(Debug, PartialEq, Eq)]
pub struct FifoPlan {
    pub bundle: String,

    /// The push strobe (a DUT output).
    pub valid: String,
    pub valid_invert: bool,

    /// The payload (a DUT output). Only one port.
    pub data: String,
    pub width: usize,

    /// Where backpressure goes (a DUT input). None for `valid_only`.
    pub ready: Option<Ready>,

    pub depth: u32,

    /// Whether the default depth was used. Kept so that the value used is
    /// reported.
    pub depth_defaulted: bool,
}

impl FifoPlan {
    /// The width of the level counter. It counts up to `depth`, so
    /// `$clog2(depth) + 1`.
    pub fn level_width(&self) -> usize {
        self.depth.trailing_zeros() as usize + 1
    }

    /// Whether drops must be counted.
    ///
    /// With a contract that has backpressure, the harness only lowers `ready`
    /// while full, so nothing is dropped. A counter that can never count would
    /// only confuse the reader.
    pub fn counts_drops(&self) -> bool {
        self.ready.is_none()
    }
}

/// The resolved plan of one `trace` bundle.
#[derive(Debug, PartialEq, Eq)]
pub struct TracePlan {
    pub bundle: String,

    /// The beat strobe (a DUT output).
    pub valid: String,
    pub valid_invert: bool,

    /// The payload, in the order of the bundle's ports. The first field sits
    /// right above the cycle count.
    pub fields: Vec<TraceField>,

    pub depth: u32,

    pub format: Option<TraceFormat>,
}

/// One field of a trace record.
#[derive(Debug, PartialEq, Eq)]
pub struct TraceField {
    /// The port name without its direction prefix and the bundle name
    /// (`o_rvfi_pc_rdata` in bundle `rvfi` is `pc_rdata`).
    pub name: String,
    pub port: String,
    /// The lowest bit in the record.
    pub lsb: usize,
    pub width: usize,
}

/// The name of the field the harness adds: cycles since the start, in the
/// lowest bits of every record.
pub const TRACE_CYCLE: &str = "cycle";

/// The width of the cycle count. It wraps after 2^32 cycles.
pub const TRACE_CYCLE_BITS: usize = 32;

impl TracePlan {
    /// The payload bits, without the cycle count.
    pub fn payload_width(&self) -> usize {
        self.fields.iter().map(|field| field.width).sum()
    }

    /// The bits of one record.
    pub fn record_width(&self) -> usize {
        TRACE_CYCLE_BITS + self.payload_width()
    }

    /// Window words per entry. The window picks a word inside an entry with
    /// the low address bits, so it is a power of two; the words above the
    /// record read as 0.
    pub fn entry_words(&self) -> usize {
        self.record_width().div_ceil(32).next_power_of_two()
    }

    /// The block RAM it takes, in bytes. The words the window adds are not
    /// stored.
    pub fn bytes(&self) -> u64 {
        (self.record_width() as u64 * u64::from(self.depth)).div_ceil(8)
    }
}

/// The resolved plan of one `bram` / `bram_preload` bundle.
#[derive(Debug, PartialEq, Eq)]
pub struct MemPlan {
    pub bundle: String,

    /// Whether the DUT only reads it (`bram_preload`).
    pub read_only: bool,

    /// The address (a DUT output).
    pub addr: String,
    pub addr_width: usize,

    /// The bits to drop to turn the address into an entry index.
    /// `log2(word bytes)` for a byte address, 0 for a word index.
    pub addr_shift: usize,

    /// Read data (a DUT input).
    pub rdata: String,

    /// The read width (the word one read returns).
    pub width: usize,

    /// The width of one entry. With a write channel wider than the read, it is
    /// the line width, and middle address bits select the word in the entry.
    pub entry_width: usize,

    /// The write side. None for `bram_preload`.
    pub write: Option<MemWrite>,

    /// Read enable. With it, a read happens only in a cycle where it is high;
    /// without it, every cycle reads.
    pub enable: Option<Enable>,

    /// The declared latency. The terminator is built to match it exactly.
    pub latency: u32,

    pub depth: u64,

    /// Whether the depth came from the address width because none was written.
    pub depth_from_addr: bool,

    /// How the host accesses it. With `Region`, it takes a contiguous range in
    /// the window, and there is no `<bundle>_maddr` / `_mdata`.
    pub access: crate::manifest::MemAccess,

    /// The bytes shown in the window. Smaller than the memory means a moving
    /// window. Only with `access = "region"`.
    pub aperture_bytes: Option<usize>,

    /// The DUT address of entry 0, in bytes. 0 without `dut_base`.
    pub dut_base: u64,

    /// `dut_base` in steps of the DUT address: bytes for a byte address, read
    /// words for a word index. The RTL subtracts this.
    pub base_steps: u64,
}

impl MemPlan {
    /// The number of entries the DUT address can reach. Not the address width
    /// itself: for a byte address, the low bits are an offset inside the entry,
    /// so fewer entries are reachable.
    pub fn reach(&self) -> Option<u64> {
        1u64.checked_shl((self.addr_width - self.addr_shift) as u32)
    }

    /// Whether out-of-range accesses can be counted per access. Without an
    /// enable, only cycles can be counted, because no cycle is known to be a
    /// read.
    pub fn counts_accesses(&self) -> bool {
        self.enable.is_some()
    }

    /// Whether an out-of-range access is possible.
    ///
    /// With `depth == 2^addr_width`, every address the DUT can drive exists, so
    /// it cannot happen, and no counter is placed (like the drop counter of
    /// `host_poll_fifo`).
    ///
    /// Do not compute `1 << width` for an address width of 64 or more. Real CPU
    /// addresses are 64 bits, and a plain shift overflows and panics. In that
    /// case some addresses are always unreachable, so it always counts.
    pub fn counts_out_of_range(&self) -> bool {
        // Below the base is out of range too.
        if self.base_steps != 0 {
            return true;
        }
        match self.reach() {
            Some(reach) => self.depth < reach,
            None => true,
        }
    }

    /// How many read words fit in one entry (`entry_width / width`). 1 means
    /// read and write have the same width.
    pub fn slices(&self) -> usize {
        self.entry_width / self.width
    }

    /// The address width of the host-side indirect port.
    pub fn host_addr_width(&self) -> usize {
        let bits = 64 - (self.depth - 1).leading_zeros() as usize;
        bits.max(1)
    }

    /// The size in bytes, for the capacity check.
    pub fn bytes(&self) -> u64 {
        self.depth * self.width.div_ceil(8) as u64
    }
}

/// The plan to terminate a transfer-level interface (`rd_cmd_*` / `rd_*` /
/// `wr_*`).
///
/// `backing` says where the data lives: BRAM in the FPGA for `bram`, real host
/// memory for `host_mem`. The latter needs a PCIe requester for the DUT, which
/// does not exist yet (`plan::generatable` rejects it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMemPlan {
    pub bundle: String,

    /// The beat width. It must be the same in both directions, because both
    /// access the same storage.
    pub data_width: usize,
    pub cmd_addr_width: usize,
    pub cmd_size_width: usize,

    /// The read side. May be missing: a DUT with only one side is accepted.
    pub read: Option<HostMemRead>,
    /// The write side. May be missing too.
    pub write: Option<HostMemWrite>,

    /// The number of beats in the stand-in BRAM.
    pub depth: u32,
    /// Whether the default depth was used. Kept so that the value is reported.
    pub depth_defaulted: bool,
}

/// The read side of `host_mem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMemRead {
    pub cmd_valid: String,
    pub cmd_ready: String,
    pub cmd_addr: String,
    pub cmd_size: String,
    pub valid: String,
    pub ready: String,
    pub data: String,
    pub last: String,
}

/// The write side of `host_mem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMemWrite {
    pub cmd_valid: String,
    pub cmd_ready: String,
    pub cmd_addr: String,
    pub cmd_size: String,
    pub valid: String,
    pub ready: String,
    pub data: String,
    /// Optional. Without it, the whole beat is written.
    pub strb: Option<String>,
    pub last: String,
    /// Optional. A DUT that does not wait for completion leaves it out.
    pub done_valid: Option<String>,
}

impl HostMemPlan {
    /// The width of the address register the host writes.
    pub fn host_addr_width(&self) -> usize {
        self.depth.trailing_zeros() as usize
    }

    /// The BRAM bytes the stand-in uses.
    pub fn bram_bytes(&self) -> u64 {
        self.depth as u64 * (self.data_width.div_ceil(8) as u64)
    }
}

/// The write side of a memory (the DUT writes).
#[derive(Debug, PartialEq, Eq)]
pub struct MemWrite {
    pub wdata: String,
    pub we: String,
    pub we_invert: bool,

    /// Byte strobe (one bit per byte). Without it, a whole entry is written.
    pub wstrb: Option<String>,
}

/// The read enable of a memory.
#[derive(Debug, PartialEq, Eq)]
pub struct Enable {
    pub port: String,
    pub invert: bool,
}

/// Where backpressure goes.
#[derive(Debug, PartialEq, Eq)]
pub struct Ready {
    pub port: String,
    /// Whether it is declared inverted, as in `ready = "!i_stall"`.
    pub invert: bool,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, Diagnostic)]
pub enum TerminatorError {
    #[error(
        "[bundle.{bundle}] is `host_poll_fifo` with contract `{contract}`, which is not supported"
    )]
    #[diagnostic(
        code(harness::terminator::unsupported_contract),
        help(
            "A host_poll_fifo needs a signal that starts each beat. Use one of:\n\n    contract = \"valid_ready\"   # the harness stalls the DUT when full\n    contract = \"valid_only\"    # the harness counts dropped beats\n\nOr terminate this bundle with `reg`."
        )
    )]
    UnsupportedContract { bundle: String, contract: String },

    #[error("[bundle.{bundle}] is an addressable interface but has no `rdata`")]
    #[diagnostic(
        code(harness::terminator::slave_needs_rdata),
        help(
            "The host reads this interface, so the DUT has to answer. Name the port it answers on:\n\n    ports = {{ .., rdata = \"<port>\" }}\n\nIf the host only writes, drop the `addr` role. Each port then becomes its own register."
        )
    )]
    SlaveNeedsRData { bundle: String },

    /// INTx is one level-sensitive wire, so the bundle is one 1-bit output.
    #[error("[bundle.{bundle}] is a host_irq with {what}, not one 1-bit output")]
    #[diagnostic(
        code(harness::terminator::irq_shape),
        help(
            "A host_irq bundle is the DUT's interrupt line: one output, 1 bit wide, high while the interrupt is pending. The harness sends it to the host as INTA.\n\nName that port alone:\n\n    [bundle.{bundle}]\n    backing = \"host_irq\"\n    ports   = [\"<port>\"]"
        )
    )]
    IrqShape { bundle: String, what: String },

    #[error("[bundle.{first}] and [bundle.{second}] are both host_irq")]
    #[diagnostic(
        code(harness::terminator::irq_more_than_one),
        help(
            "The card has one interrupt pin (INTA), so the harness takes one host_irq bundle. OR the lines together in the DUT, and keep the reasons in a register the host can read."
        )
    )]
    IrqMoreThanOne { first: String, second: String },

    /// The FLR request is one 1-bit input; the DUT's answer, if any, is one
    /// 1-bit output.
    #[error("[bundle.{bundle}] is a pcie_flr with {what}")]
    #[diagnostic(
        code(harness::terminator::flr_shape),
        help(
            "A pcie_flr bundle is one 1-bit input, high while the host's Function Level Reset is in progress, and optionally one 1-bit output the DUT raises when it has finished:\n\n    [bundle.{bundle}]\n    backing = \"pcie_flr\"\n    ports   = [\"<input>\", \"<done output>\"]\n\nWithout the output, the input is high for 256 cycles of the DUT's clock."
        )
    )]
    FlrShape { bundle: String, what: String },

    #[error("[bundle.{first}] and [bundle.{second}] are both pcie_flr")]
    #[diagnostic(
        code(harness::terminator::flr_more_than_one),
        help("The card has one function, so there is one FLR. Fan the input out inside the DUT.")
    )]
    FlrMoreThanOne { first: String, second: String },

    /// The window address is 32 bits (AXI-Lite), and the whole slave is mapped
    /// into it.
    #[error(
        "[bundle.{bundle}] addresses {addr_width} bits of words, which does not fit the host window"
    )]
    #[diagnostic(
        code(harness::terminator::slave_too_large),
        help(
            "The whole slave is mapped into the host window, and the window has a 32-bit address, so `addr` can be at most {max} bits wide. Narrow `addr` in a wrapper around the DUT."
        )
    )]
    SlaveTooLarge {
        bundle: String,
        addr_width: usize,
        max: usize,
    },

    #[error("[bundle.{bundle}] is `slave` with no `addr` port")]
    #[diagnostic(
        code(harness::terminator::slave_needs_addr),
        help(
            "A slave is an interface the host addresses: a RAM port, where the harness drives `addr` and reads `rdata`, or an APB slave (`psel`, `penable`, `pwrite`, `paddr`, `pwdata`, `prdata`). Name the roles:\n\n    ports = {{ addr = \"<port>\", rdata = \"<port>\" }}\n\nIf the ports are plain registers, use `backing = \"reg\"`."
        )
    )]
    SlaveNeedsAddr { bundle: String },

    /// Never inferred. A silent default would silently build a BRAM with a
    /// combinational read.
    #[error("[bundle.{bundle}] is `{backing}` with no `latency`")]
    #[diagnostic(
        code(harness::terminator::latency_required),
        help(
            "{why} Only you know that number. State it:\n\n    [bundle.{bundle}]\n    latency = 1"
        )
    )]
    LatencyRequired {
        bundle: String,
        backing: String,
        why: &'static str,
    },

    #[error("[bundle.{bundle}] is `dram` but has no AXI4 port")]
    #[diagnostic(
        code(harness::terminator::dram_needs_axi4),
        help(
            "The board's memory controller has a variable latency, so the DUT has to be able to wait: present a `$std::axi4_if::master` port, or the transfer-level roles (`rd_cmd_*` / `rd_*` / `wr_*`).\n\nFor a memory port with a fixed latency, use `backing = \"bram\"`."
        )
    )]
    DramNeedsAxi4 { bundle: String },

    /// Ignoring it silently would make the manifest disagree with the hardware.
    #[error("[bundle.{bundle}] has `{key}`, which {shape} does not use")]
    #[diagnostic(
        code(harness::terminator::key_not_applicable),
        help("{shape} takes {takes}. Remove `{key}`, or change the ports or the backing.")
    )]
    KeyNotApplicable {
        bundle: String,
        key: &'static str,
        shape: &'static str,
        takes: String,
    },

    /// A generator bug: no terminator took the bundle, and none said why.
    #[error("[bundle.{bundle}] reaches no terminator for `{backing}`")]
    #[diagnostic(
        code(harness::terminator::not_terminated),
        help(
            "This is a bug in veryl-harness: every bundle should get a terminator or an error that says why not. Please report it with this bundle's section of Harness.toml."
        )
    )]
    NotTerminated { bundle: String, backing: String },

    #[error("[bundle.{bundle}] carries `{port}`, which is {width} bits wide")]
    #[diagnostic(
        code(harness::terminator::slave_wider_than_a_word),
        help(
            "The window is 32 bits, and a register wider than one window word is not supported yet. Split it into 32-bit registers in a wrapper around the DUT."
        )
    )]
    SlaveWiderThanAWord {
        bundle: String,
        port: String,
        width: usize,
    },

    #[error("[bundle.{bundle}] carries `{port}`, which is {width} bits wide")]
    #[diagnostic(
        code(harness::terminator::slave_width_not_a_word),
        help(
            "The window is 32 bits, and other widths are not supported yet. Make the port 32 bits, or drop the `addr` role so each port becomes its own register."
        )
    )]
    SlaveWidthNotAWord {
        bundle: String,
        port: String,
        width: usize,
    },

    #[error("[bundle.{bundle}] addresses {addr_width} bits of bytes on `{port}`, which {what}")]
    #[diagnostic(
        code(harness::terminator::dmi_address_width),
        help(
            "`paddr` is a byte address, and the Debug Module registers end at 0x7f, byte 0x1fc, so it needs at least {min} bits. The harness maps the whole range into the host window, so it takes at most {max}.\n\nGive the port {min} to {max} bits in a wrapper around the DUT."
        )
    )]
    DmiAddressWidth {
        bundle: String,
        port: String,
        addr_width: usize,
        what: &'static str,
        min: usize,
        max: usize,
    },

    #[error("[bundle.{bundle}] says `reset = \"{port}\"`, but {what}")]
    #[diagnostic(
        code(harness::terminator::dmi_reset_port),
        help(
            "`reset` names the Debug Module's reset input, a port of type `reset` on the DUT. The harness drives it with its own reset, so `hio reset` does not clear the debugger's state.\n\nName that port, or remove the line to leave the Debug Module on the DUT reset."
        )
    )]
    DmiResetPort {
        bundle: String,
        port: String,
        what: &'static str,
    },

    #[error("[bundle.{bundle}] addresses {addr_width} bits of bytes on `{port}`, which {what}")]
    #[diagnostic(
        code(harness::terminator::apb_slave_address_width),
        help(
            "`paddr` is a byte address of 32-bit registers, so it needs at least {min} bits. The harness maps the whole range into the host window, so it takes at most {max}.\n\nGive the port {min} to {max} bits in a wrapper around the DUT."
        )
    )]
    ApbSlaveAddressWidth {
        bundle: String,
        port: String,
        addr_width: usize,
        what: &'static str,
        min: usize,
        max: usize,
    },

    #[error("[bundle.{bundle}] is an APB `slave` and also carries `{port}`, which is no APB port")]
    #[diagnostic(
        code(harness::terminator::apb_slave_stray_port),
        help(
            "An APB `slave` bundle holds the APB slave and nothing else: `psel`, `penable`, `pwrite`, `paddr`, `pwdata`, `prdata`, and optionally `pready` and `pslverr`.\n\nMove `{port}` to another bundle, or tie it off with `[tie_off]` / `[leave_open]`."
        )
    )]
    ApbSlaveStrayPort { bundle: String, port: String },

    #[error("[bundle.{bundle}] is `dmi` and also carries `{port}`, which is no APB port")]
    #[diagnostic(
        code(harness::terminator::dmi_stray_port),
        help(
            "A `dmi` bundle holds the APB slave and nothing else: `psel`, `penable`, `pwrite`, `paddr`, `pwdata`, `prdata` and optionally `pready`. `pslverr` is not taken: the Debug Module reports its errors in its own registers.\n\nMove `{port}` to another bundle, or tie it off with `[tie_off]` / `[leave_open]`."
        )
    )]
    DmiStrayPort { bundle: String, port: String },

    #[error("[bundle.{bundle}] names one of `wdata` / `we` but not the other")]
    #[diagnostic(
        code(harness::terminator::slave_half_write),
        help(
            "`wdata` carries the value and `we` says when to write. Name both, or name neither for a read-only interface."
        )
    )]
    SlaveHalfWrite { bundle: String },

    /// The text depends on the interface shape (`Shape`), so a wrong `rdata` on a
    /// `bram` talks about `bram`, not about `host_poll_fifo`.
    #[error("[bundle.{bundle}] is {shape}, but `{port}` ({role}) is a DUT {direction}")]
    #[diagnostic(code(harness::terminator::wrong_direction), help("{rule}\n\n{fix}"))]
    WrongDirection {
        bundle: String,
        port: String,
        role: &'static str,
        direction: &'static str,
        shape: &'static str,
        rule: &'static str,
        fix: &'static str,
    },

    #[error("[bundle.{bundle}] is `host_poll_fifo` with {count} data ports")]
    #[diagnostic(
        code(harness::terminator::data_port_count),
        help(
            "A host_poll_fifo needs exactly one data port. Roles found:\n{roles}\n\nUse one bundle per payload, or name the roles:\n\n    ports = {{ valid = \"..\", data = \"..\" }}"
        )
    )]
    DataPortCount {
        bundle: String,
        count: usize,
        roles: String,
    },

    #[error("[bundle.{bundle}] port `{port}` has a width the analyzer cannot resolve")]
    #[diagnostic(
        code(harness::terminator::unresolved_width),
        help(
            "The terminator needs the payload width. Widths in Harness.toml are not supported yet, so use a port whose width the analyzer can resolve."
        )
    )]
    UnresolvedWidth { bundle: String, port: String },

    #[error("[bundle.{bundle}] {role} port `{port}` is {width}")]
    #[diagnostic(
        code(harness::terminator::wide_handshake),
        help(
            "`{role}` must be 1 bit. If the port carries data, give it the `data` role or move it to its own bundle."
        )
    )]
    WideHandshake {
        bundle: String,
        port: String,
        role: &'static str,
        /// "8 bits wide" or "of a width that could not be resolved".
        width: String,
    },

    /// With different widths, one address would mean different bytes for read
    /// and write.
    #[error("[bundle.{bundle}] reads {read}-bit beats but writes {write}-bit ones")]
    #[diagnostic(
        code(harness::terminator::host_mem_width_mismatch),
        help(
            "Both directions reach the same memory, so the beats must be the same width. Make `rd_data` and `wr_data` the same width on the DUT."
        )
    )]
    HostMemWidthMismatch {
        bundle: String,
        read: usize,
        write: usize,
    },

    /// `emit` declares the read and write commands with one width. A mismatch
    /// fails in `veryl build`.
    #[error("[bundle.{bundle}] has a {read}-bit `rd_cmd_{what}` but a {write}-bit `wr_cmd_{what}`")]
    #[diagnostic(
        code(harness::terminator::host_mem_command_width_mismatch),
        help(
            "Both commands address the same memory, so they must be the same width. Make `rd_cmd_{what}` and `wr_cmd_{what}` the same width on the DUT."
        )
    )]
    HostMemCommandWidthMismatch {
        bundle: String,
        what: &'static str,
        read: usize,
        write: usize,
    },

    /// A strobe has one bit per byte. The harness declares it as `data / 8` bits.
    #[error("[bundle.{bundle}] has a {width}-bit `{port}` ({role}) for {data}-bit data")]
    #[diagnostic(
        code(harness::terminator::strobe_width),
        help("A strobe has one bit per data byte, so it has to be {want} bits wide.")
    )]
    StrobeWidth {
        bundle: String,
        port: String,
        role: &'static str,
        width: usize,
        data: usize,
        want: usize,
    },

    /// The command address becomes an entry number by dropping low bits, so a
    /// beat must be a power-of-two number of bytes.
    #[error("[bundle.{bundle}] serves {width}-bit beats, which the stand-in cannot address")]
    #[diagnostic(
        code(harness::terminator::host_mem_beat_width),
        help(
            "A beat must be a power-of-two number of bytes, and {width} bits is not. Change the width of the `rd_data` port on the DUT."
        )
    )]
    HostMemBeatWidth { bundle: String, width: usize },

    /// The pointers wrap by counting. With a depth that is not a power of two,
    /// they wrap early and the order breaks.
    #[error("[bundle.{bundle}] depth = {depth} is not a power of two of at least 2")]
    #[diagnostic(
        code(harness::terminator::depth_not_power_of_two),
        help(
            "The pointers wrap only at a power of two, and it takes at least 2 entries. Round it:\n\n    depth = {suggestion}"
        )
    )]
    DepthNotPowerOfTwo {
        bundle: String,
        depth: u32,
        suggestion: u32,
    },

    #[error("[bundle.{bundle}] is `{backing}` with contract `{contract}`")]
    #[diagnostic(
        code(harness::terminator::memory_contract),
        help(
            "A memory port takes one of three shapes:\n\n- `addr` and `rdata` with no handshake, answered `latency` cycles later\n- the transfer-level roles `rd_cmd_*` / `rd_*` / `wr_*`, which can make the DUT wait\n- a `$std::axi4_if::master` port\n\nThis one has a handshake but none of the transfer-level roles. Name them in `ports`, or remove the handshake."
        )
    )]
    MemoryContract {
        bundle: String,
        backing: String,
        contract: String,
    },

    #[error("[bundle.{bundle}] is `{backing}` with no `{role}` port")]
    #[diagnostic(
        code(harness::terminator::memory_role_missing),
        help(
            "A memory port needs an address and read data. Roles found:\n{roles}\n\nOnly the exact suffixes `addr` / `rdata` / `wdata` / `we` are matched. Name the rest:\n\n    ports = {{ addr = \"..\", rdata = \"..\" }}"
        )
    )]
    MemoryRoleMissing {
        bundle: String,
        backing: String,
        role: &'static str,
        roles: String,
    },

    #[error("[bundle.{bundle}] is `bram_preload`, but the DUT writes to it (`{port}`)")]
    #[diagnostic(
        code(harness::terminator::preload_is_read_only),
        help(
            "The host fills a `bram_preload` and the DUT only reads it. The `{role}` port writes to it.\n\nUse `backing = \"bram\"` if the DUT writes, or remove the write ports from this bundle."
        )
    )]
    PreloadIsReadOnly {
        bundle: String,
        port: String,
        role: &'static str,
    },

    /// With only one of them, the memory would write every cycle or never.
    #[error("[bundle.{bundle}] has `{present}` but no `{missing}`")]
    #[diagnostic(
        code(harness::terminator::incomplete_write_port),
        help(
            "A write needs both the data and the write enable. Name both:\n\n    ports = {{ addr = \"..\", rdata = \"..\", wdata = \"..\", we = \"..\" }}"
        )
    )]
    IncompleteWritePort {
        bundle: String,
        present: &'static str,
        missing: &'static str,
    },

    /// A write wider than the read is the case of a cache that writes back a
    /// line while a load reads one word. Address bits select the read word, so
    /// the ratio must be a power of two.
    #[error("[bundle.{bundle}] writes {wdata_width} bits but reads {rdata_width}")]
    #[diagnostic(
        code(harness::terminator::write_width_mismatch),
        help(
            "The write data may be wider than the read data only by a power-of-two factor. `{wdata}` is {wdata_width} bits and `{rdata}` is {rdata_width}.\n\nThe two share one address, so they cannot be split into separate bundles. Change the widths, or adapt the wide channel in the wrapper that [dut] points at."
        )
    )]
    WriteWidthMismatch {
        bundle: String,
        wdata: String,
        wdata_width: usize,
        rdata: String,
        rdata_width: usize,
    },

    /// A wrong choice still compiles and looks connected, and the DUT reads the
    /// wrong word.
    #[error("[bundle.{bundle}] does not say whether `{port}` counts bytes or entries")]
    #[diagnostic(
        code(harness::terminator::addressing_unknown),
        help(
            "An entry is {width} bits ({bytes} bytes), so byte and word addresses pick different entries. State which:\n\n    addressing = \"byte\"   # a CPU address bus: entry = addr / {bytes}\n    addressing = \"word\"   # a plain RAM port: entry = addr"
        )
    )]
    AddressingUnknown {
        bundle: String,
        port: String,
        width: usize,
        bytes: usize,
    },

    /// A strobe that does not match the data writes the wrong bytes, and the
    /// DUT reads back values it never wrote.
    #[error("[bundle.{bundle}] has a {strobe_width}-bit strobe for a {entry_width}-bit entry")]
    #[diagnostic(
        code(harness::terminator::strobe_width_mismatch),
        help("A byte strobe has one bit per byte, so `{wstrb}` must be {expected} bits wide.")
    )]
    StrobeWidthMismatch {
        bundle: String,
        wstrb: String,
        strobe_width: usize,
        entry_width: usize,
        expected: usize,
    },

    #[error("[bundle.{bundle}] is byte addressed but an entry is {width} bits")]
    #[diagnostic(
        code(harness::terminator::byte_address_needs_whole_bytes),
        help(
            "With byte addresses, an entry must be a power-of-two number of bytes. Widen the port, or use `addressing = \"word\"` if the address counts entries."
        )
    )]
    ByteAddressNeedsWholeBytes { bundle: String, width: usize },

    #[error(
        "[bundle.{bundle}] address `{port}` is {bits} bits, too narrow to carry a byte address"
    )]
    #[diagnostic(
        code(harness::terminator::address_too_narrow),
        help(
            "The low {shift} bits of a byte address pick a byte inside the entry, so {bits} bits cannot reach the second entry. Widen the address, or use `addressing = \"word\"`."
        )
    )]
    AddressTooNarrowForBytes {
        bundle: String,
        port: String,
        bits: usize,
        shift: usize,
    },

    /// Out-of-range addresses are not wrapped: the DUT could not notice that it
    /// got the wrong word.
    #[error("[bundle.{bundle}] would need {words} words to cover its {bits}-bit address")]
    #[diagnostic(
        code(harness::terminator::address_space_too_large),
        help(
            "By default the memory covers the whole address space, and {bits} bits is too large for an FPGA. State the size the DUT needs:\n\n    depth = 4096\n\nReads at or above `depth` return 0, writes there are dropped, and both are counted in `{bundle}_oor`."
        )
    )]
    AddressSpaceTooLarge {
        bundle: String,
        bits: usize,
        words: String,
    },

    #[error("[bundle.{bundle}] depth = {depth} does not fit its {bits}-bit address")]
    #[diagnostic(
        code(harness::terminator::depth_exceeds_address),
        help(
            "The DUT can reach only {reach} words. Widen the address port, or use a smaller depth."
        )
    )]
    DepthExceedsAddress {
        bundle: String,
        depth: u64,
        bits: usize,
        reach: u64,
    },

    /// The RTL subtracts the base in address steps, so it must be whole steps.
    #[error(
        "[bundle.{bundle}] has dut_base = {base:#x}, which is not a multiple of its {step}-byte word"
    )]
    #[diagnostic(
        code(harness::terminator::dut_base_not_aligned),
        help(
            "`{port}` counts {step}-byte words, so entry 0 must start on a word. The nearest values that work are {below:#x} and {above:#x}."
        )
    )]
    DutBaseNotAligned {
        bundle: String,
        port: String,
        base: u64,
        step: u64,
        below: u64,
        above: u64,
    },

    /// A word index of a width that is not whole bytes has no byte address.
    #[error(
        "[bundle.{bundle}] has `dut_base`, and `{port}` counts {width}-bit words, which are not whole bytes"
    )]
    #[diagnostic(
        code(harness::terminator::dut_base_needs_whole_bytes),
        help(
            "`dut_base` is a byte address, and a {width}-bit word does not start on a byte. Remove `dut_base` and subtract the base in the DUT."
        )
    )]
    DutBaseNeedsWholeBytes {
        bundle: String,
        port: String,
        width: usize,
    },

    /// Above the address space, the subtraction would wrap into the memory.
    #[error(
        "[bundle.{bundle}] ends at {end:#x} (dut_base = {base:#x} plus the memory), past the {bits}-bit address of `{port}`"
    )]
    #[diagnostic(
        code(harness::terminator::dut_base_past_address),
        help(
            "The DUT cannot reach the top of this memory. Lower `dut_base`, set a smaller `depth`, or widen the address port."
        )
    )]
    DutBasePastAddress {
        bundle: String,
        port: String,
        base: u64,
        end: u128,
        bits: usize,
    },

    /// A burst must not cross 4 KB, before or after the base comes off.
    #[error("[bundle.{bundle}] has dut_base = {base:#x}, which is not a multiple of 4 KB")]
    #[diagnostic(
        code(harness::terminator::dut_base_not_4k),
        help(
            "An AXI4 burst never crosses a 4 KB boundary. Taking off a base that is not a multiple of 4 KB would make some bursts cross one in the memory. The nearest values that work are {below:#x} and {above:#x}."
        )
    )]
    DutBaseNot4k {
        bundle: String,
        base: u64,
        below: u64,
        above: u64,
    },

    #[error("[bundle.{bundle}] is `trace` with contract `{contract}`")]
    #[diagnostic(
        code(harness::terminator::trace_contract),
        help(
            "A trace records the beats a DUT output marks with `valid`, and never stalls the DUT: the ring overwrites its oldest record instead. Give the bundle a `valid` and no `ready`:\n\n    contract = \"valid_only\""
        )
    )]
    TraceContract { bundle: String, contract: String },

    #[error("[bundle.{bundle}] is `trace` with nothing to record")]
    #[diagnostic(
        code(harness::terminator::trace_no_payload),
        help(
            "The bundle has a `valid` and no other port. List the ports to record:\n\n    ports = [\"o_rvfi_valid\", \"o_rvfi_pc_rdata\", \"o_rvfi_insn\"]"
        )
    )]
    TraceNoPayload { bundle: String },

    #[error("[bundle.{bundle}] is `trace` with no `depth`")]
    #[diagnostic(
        code(harness::terminator::trace_depth_missing),
        help(
            "Say how many records the ring keeps, a power of two. It takes block RAM: depth x record bits. For example:\n\n    depth = 1024"
        )
    )]
    TraceDepthMissing { bundle: String },

    #[error("[bundle.{bundle}] records `{port}` as field `{field}`, which is taken")]
    #[diagnostic(
        code(harness::terminator::trace_field_clash),
        help(
            "A field is the port name without its direction prefix and the bundle name, and the harness adds `cycle` itself. Rename the port in the wrapper you point [dut] at."
        )
    )]
    TraceFieldClash {
        bundle: String,
        port: String,
        field: String,
    },

    #[error("[bundle.{bundle}] is an RVFI trace with no `{field}`")]
    #[diagnostic(
        code(harness::terminator::rvfi_missing),
        help(
            "Without `pc_rdata` and `insn` a record does not say which instruction it is. Add the port (for example `o_rvfi_{field}`) to the bundle, or remove `format = \"rvfi\"`."
        )
    )]
    RvfiMissing { bundle: String, field: &'static str },

    #[error("[bundle.{bundle}] has `{port}`, which is no RVFI signal (`{field}`)")]
    #[diagnostic(
        code(harness::terminator::rvfi_unknown),
        help(
            "With `format = \"rvfi\"`, each port is `rvfi_<name>`, where <name> is one of: {known}, or `csr_*`. Check the spelling, or move the port to another bundle."
        )
    )]
    RvfiUnknown {
        bundle: String,
        port: String,
        field: String,
        known: String,
    },

    #[error("[bundle.{bundle}] has `{port}` of {width} bits")]
    #[diagnostic(
        code(harness::terminator::rvfi_width),
        help(
            "RVFI makes it {want}. A core that retires more than one instruction per cycle (NRET > 1) is not supported yet."
        )
    )]
    RvfiWidth {
        bundle: String,
        port: String,
        width: usize,
        want: String,
    },

    /// Ignoring it silently would make the manifest disagree with the hardware.
    #[error("[bundle.{bundle}] has `depth`, which `{backing}` does not use")]
    #[diagnostic(
        code(harness::terminator::depth_not_applicable),
        help(
            "Only a host_poll_fifo, a trace or a memory uses `depth`, so `{backing}` would ignore it. Remove it, or change the backing."
        )
    )]
    DepthNotApplicable { bundle: String, backing: String },

    /// The harness drives every channel of a DUT slave, so a one-sided
    /// modport would leave a read or a write with nowhere to go.
    #[error("[bundle.{bundle}] is `slave` but presents `{port}` as `{modport}`, not `slave`")]
    #[diagnostic(
        code(harness::terminator::axi_slave_not_a_slave),
        help(
            "In a `slave` bundle the host drives the DUT, so the DUT's AXI4 port must be `::slave`. A `::master` port reads and writes a memory: use `backing = \"bram\"` or `\"dram\"`."
        )
    )]
    AxiSlaveNotASlave {
        bundle: String,
        port: String,
        modport: String,
    },

    /// The window has a 32-bit address and other regions share it.
    #[error(
        "[bundle.{bundle}] addresses {addr_width} bits on `{port}`, which does not fit the host window"
    )]
    #[diagnostic(
        code(harness::terminator::axi_slave_too_large),
        help(
            "The host window has a 32-bit address, so it shows at most 2^{max} bytes of a slave at once. Show it through a moving window of a size you choose; `<bundle>_base_jtag` picks which part:\n\n    [bundle.{bundle}]\n    aperture = \"64k\""
        )
    )]
    AxiSlaveTooLarge {
        bundle: String,
        port: String,
        addr_width: u32,
        max: u32,
    },

    /// An unconnected channel gives a read that never returns or a write that
    /// never ends.
    #[error("[bundle.{bundle}] presents `{port}` as `{modport}`, not `master`")]
    #[diagnostic(
        code(harness::terminator::axi_mem_not_a_master),
        help(
            "The harness is the slave, so the DUT must be the master. `{modport}` leaves some channels unconnected.\n\nUse `::master`."
        )
    )]
    AxiMemNotAMaster {
        bundle: String,
        port: String,
        modport: String,
    },

    /// `hns::axi_mem` accepts DUT writes as they come, so it cannot keep the
    /// memory read-only for the DUT.
    #[error("[bundle.{bundle}] is `bram_preload` on an AXI4 port, which is not generated yet")]
    #[diagnostic(
        code(harness::terminator::axi_preload_unsupported),
        help(
            "The AXI4 memory takes the DUT's writes, so it cannot keep the contents read-only for the DUT. Use `backing = \"bram\"` and load it from the host before releasing the DUT (`hio load`, with `hio reset --hold`)."
        )
    )]
    AxiPreloadUnsupported { bundle: String },

    #[error("[bundle.{bundle}] leaves `depth` to the board, and no board was named")]
    #[diagnostic(
        code(harness::terminator::dram_depth_needs_a_target),
        help(
            "Without `depth`, a `dram` bundle uses all of the board's memory, and only the board description knows that size.\n\nName a board:\n\n    veryl harness check --target <name>\n\nOr state the size. The register map then stays the same on every board:\n\n    [bundle.{bundle}]\n    depth = <entries>    # e.g. \"64M\" for 256MB of 4-byte words"
        )
    )]
    DramDepthNeedsATarget { bundle: String },

    /// The board was named, but its description has no memory controller. The
    /// KCU105 is one: its DDR4 part is chosen with a config.
    #[error("[bundle.{bundle}] is backed by `dram`, and `{target}` describes no memory controller")]
    #[diagnostic(code(harness::terminator::dram_not_on_target), help("{how}"))]
    DramNotOnTarget {
        bundle: String,
        target: String,
        how: String,
    },

    /// The stand-in memory decodes with a mask, and its window is aligned to
    /// its size.
    #[error("[bundle.{bundle}] has depth = {depth}, which is not a power of two of at least 2")]
    #[diagnostic(
        code(harness::terminator::axi_mem_depth_not_power_of_two),
        help(
            "The stand-in memory needs a power-of-two word count of at least 2: one word leaves no address bits, and Veryl rejects a zero-width index. Round it: 1024, 2048, 4096."
        )
    )]
    AxiMemDepthNotPowerOfTwo { bundle: String, depth: u32 },

    /// A split where one 32-bit word spans two beats is not implemented.
    #[error("[bundle.{bundle}] carries {bits} bits on `{port}`")]
    #[diagnostic(
        code(harness::terminator::axi_mem_width_not_a_word_multiple),
        help(
            "The harness splits the bus into 32-bit words. Use a power-of-two width of at least 32 bits: 32, 64, 128, 256."
        )
    )]
    AxiMemWidthNotAWordMultiple {
        bundle: String,
        port: String,
        bits: u32,
    },

    /// The window decodes an aperture with a mask and aligns it to its size.
    #[error("[bundle.{bundle}] has aperture = {aperture}, which is not a power of two")]
    #[diagnostic(
        code(harness::terminator::aperture_not_power_of_two),
        help("An aperture must be a power of two. Round it: 1024, 4096, 65536.")
    )]
    ApertureNotPowerOfTwo { bundle: String, aperture: u64 },

    #[error(
        "[bundle.{bundle}] has aperture = {aperture}, smaller than one {entry_bytes}-byte entry"
    )]
    #[diagnostic(
        code(harness::terminator::aperture_smaller_than_entry),
        help("An aperture must hold at least one entry. Make it at least {entry_bytes} bytes.")
    )]
    ApertureSmallerThanEntry {
        bundle: String,
        aperture: u64,
        entry_bytes: usize,
    },

    #[error("[bundle.{bundle}] has aperture = {aperture}, and the memory is {total_bytes} bytes")]
    #[diagnostic(
        code(harness::terminator::aperture_not_smaller),
        help(
            "An aperture is for a memory too big to map whole, and this one covers the whole memory. Remove `aperture`."
        )
    )]
    ApertureNotSmaller {
        bundle: String,
        aperture: u64,
        total_bytes: u64,
    },

    #[error("[bundle.{bundle}] has `aperture`, which needs `access = \"region\"`")]
    #[diagnostic(
        code(harness::terminator::aperture_needs_a_region),
        help(
            "An indirect port (`maddr` / `mdata`) already reaches the whole memory, so an aperture has nothing to move.\n\nAdd `access = \"region\"`, or remove `aperture`."
        )
    )]
    ApertureNeedsARegion { bundle: String },

    /// A 256 MB DRAM would need a 256 MB window, and on PCIe a BAR of that size.
    #[error("[bundle.{bundle}] is backed by `dram` but says no `aperture`")]
    #[diagnostic(
        code(harness::terminator::dram_needs_aperture),
        help(
            "DRAM is too big to map whole into the window. Set how much to show at once. The host moves the window for you:\n\n    [bundle.{bundle}]\n    aperture = \"1M\""
        )
    )]
    DramNeedsAperture { bundle: String },
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// The ports of a transfer-level bundle, checked against the DUT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XferPorts {
    pub read: Option<HostMemRead>,
    pub write: Option<HostMemWrite>,
    /// The beat width, the same in both directions.
    pub data_width: usize,
    pub cmd_addr_width: usize,
    pub cmd_size_width: usize,
}

/// Checks the transfer-level ports of a bundle: each present side whole, the
/// directions, the widths, and the strobe. Shared by `bram` (`hns::hmem`) and
/// `dram` (`hns::xfer2axi`), which serve the same port.
fn resolve_xfer_ports(
    dut: &Dut,
    binding: &Binding,
    contract: &BundleContract,
) -> Result<XferPorts, TerminatorError> {
    let want = |role: Role| {
        contract
            .roles
            .iter()
            .find(|assignment| assignment.role == role)
            .map(|assignment| assignment.port.clone())
    };
    // The contract check already ensured that a side that is present is
    // complete.
    let find = |role: Role| want(role).expect("check_host_mem has required this half whole");

    let width_of = |name: &str| -> Result<usize, TerminatorError> {
        port_of(dut, name)
            .width()
            .ok_or_else(|| TerminatorError::UnresolvedWidth {
                bundle: binding.bundle.clone(),
                port: name.to_string(),
            })
    };
    let single_bit = |port: &str, role: &'static str| -> Result<(), TerminatorError> {
        expect_single_bit(&binding.bundle, port_of(dut, port), role)
    };
    // Check each direction against the DUT declaration (symbol table).
    // Otherwise an `rd_cmd_valid` that is a DUT input would pass, and fail
    // later in `veryl build` or on the board.
    let direction = |port: &str, role: &'static str, want| -> Result<(), TerminatorError> {
        expect_direction(
            Shape::TransferLevel,
            &binding.bundle,
            port_of(dut, port),
            role,
            want,
        )
    };
    use PortDirection::{Input, Output};

    let read = match want(Role::RdCmdValid) {
        Some(cmd_valid) => {
            let r = HostMemRead {
                cmd_valid,
                cmd_ready: find(Role::RdCmdReady),
                cmd_addr: find(Role::RdCmdAddr),
                cmd_size: find(Role::RdCmdSize),
                valid: find(Role::RdValid),
                ready: find(Role::RdReady),
                data: find(Role::RdData),
                last: find(Role::RdLast),
            };
            for (port, role, want) in [
                (&r.cmd_valid, "rd_cmd_valid", Output),
                (&r.cmd_ready, "rd_cmd_ready", Input),
                (&r.cmd_addr, "rd_cmd_addr", Output),
                (&r.cmd_size, "rd_cmd_size", Output),
                (&r.valid, "rd_valid", Input),
                (&r.ready, "rd_ready", Output),
                (&r.data, "rd_data", Input),
                (&r.last, "rd_last", Input),
            ] {
                direction(port, role, want)?;
            }
            for (port, role) in [
                (&r.cmd_valid, "rd_cmd_valid"),
                (&r.cmd_ready, "rd_cmd_ready"),
                (&r.valid, "rd_valid"),
                (&r.ready, "rd_ready"),
                (&r.last, "rd_last"),
            ] {
                single_bit(port, role)?;
            }
            Some(r)
        }
        None => None,
    };

    let write = match want(Role::WrCmdValid) {
        Some(cmd_valid) => {
            let w = HostMemWrite {
                cmd_valid,
                cmd_ready: find(Role::WrCmdReady),
                cmd_addr: find(Role::WrCmdAddr),
                cmd_size: find(Role::WrCmdSize),
                valid: find(Role::WrValid),
                ready: find(Role::WrReady),
                data: find(Role::WrData),
                strb: want(Role::WrStrb),
                last: find(Role::WrLast),
                done_valid: want(Role::WrDoneValid),
            };
            for (port, role, want) in [
                (&w.cmd_valid, "wr_cmd_valid", Output),
                (&w.cmd_ready, "wr_cmd_ready", Input),
                (&w.cmd_addr, "wr_cmd_addr", Output),
                (&w.cmd_size, "wr_cmd_size", Output),
                (&w.valid, "wr_valid", Output),
                (&w.ready, "wr_ready", Input),
                (&w.data, "wr_data", Output),
                (&w.last, "wr_last", Output),
            ] {
                direction(port, role, want)?;
            }
            if let Some(strb) = &w.strb {
                direction(strb, "wr_strb", Output)?;
            }
            for (port, role) in [
                (&w.cmd_valid, "wr_cmd_valid"),
                (&w.cmd_ready, "wr_cmd_ready"),
                (&w.valid, "wr_valid"),
                (&w.ready, "wr_ready"),
                (&w.last, "wr_last"),
            ] {
                single_bit(port, role)?;
            }
            if let Some(done) = &w.done_valid {
                direction(done, "wr_done_valid", Input)?;
                single_bit(done, "wr_done_valid")?;
            }
            Some(w)
        }
        None => None,
    };

    // Take the widths from the side that is present. With both sides, they
    // must match.
    let (data_width, cmd_addr_width, cmd_size_width) = match (&read, &write) {
        (Some(r), _) => (
            width_of(&r.data)?,
            width_of(&r.cmd_addr)?,
            width_of(&r.cmd_size)?,
        ),
        (None, Some(w)) => (
            width_of(&w.data)?,
            width_of(&w.cmd_addr)?,
            width_of(&w.cmd_size)?,
        ),
        (None, None) => unreachable!("check_host_mem refuses a bundle with no role"),
    };
    if let (Some(r), Some(w)) = (&read, &write) {
        let rw = width_of(&r.data)?;
        let ww = width_of(&w.data)?;
        if rw != ww {
            return Err(TerminatorError::HostMemWidthMismatch {
                bundle: binding.bundle.clone(),
                read: rw,
                write: ww,
            });
        }
        for (what, read, write) in [
            ("addr", &r.cmd_addr, &w.cmd_addr),
            ("size", &r.cmd_size, &w.cmd_size),
        ] {
            let (read, write) = (width_of(read)?, width_of(write)?);
            if read != write {
                return Err(TerminatorError::HostMemCommandWidthMismatch {
                    bundle: binding.bundle.clone(),
                    what,
                    read,
                    write,
                });
            }
        }
    }

    let bytes = data_width / 8;
    // The address becomes an entry index by dropping low bits, so a beat
    // must be a power-of-two number of bytes.
    if data_width % 8 != 0 || !bytes.is_power_of_two() {
        return Err(TerminatorError::HostMemBeatWidth {
            bundle: binding.bundle.clone(),
            width: data_width,
        });
    }

    // One strobe bit per byte. emit declares `w_<strb>` as `data / 8` bits.
    if let Some(strb) = write.as_ref().and_then(|w| w.strb.as_ref()) {
        let width = width_of(strb)?;
        if width * 8 != data_width {
            return Err(TerminatorError::StrobeWidth {
                bundle: binding.bundle.clone(),
                port: strb.clone(),
                role: "wr_strb",
                width,
                data: data_width,
                want: data_width / 8,
            });
        }
    }

    Ok(XferPorts {
        read,
        write,
        data_width,
        cmd_addr_width,
        cmd_size_width,
    })
}

/// Plans the transfer-level bundles, only where a stand-in exists.
///
/// The contract check (`contract::check_host_mem`) has already checked that
/// each present side has all its roles. Here we check that a stand-in exists
/// and that it can hold the shape (beat width, depth).
pub fn resolve_host_mem(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
    contracts: &[BundleContract],
) -> Result<Vec<HostMemPlan>, TerminatorError> {
    let mut plans = Vec::new();

    for binding in bindings {
        let declared = &manifest.bundle[&binding.bundle];
        // The interface shape decides: bundles with transfer-level roles
        // (`rd_cmd_*` / `wr_*`) come here. `backing` says where the data lives:
        // the FPGA for `bram`, the host for `host_mem` (not built yet, so `gen`
        // rejects it).
        if !matches!(declared.backing, Backing::Bram | Backing::HostMem) {
            continue;
        }
        let has_transfer_roles = contracts
            .iter()
            .find(|contract| contract.bundle == binding.bundle)
            .is_some_and(|contract| {
                contract
                    .roles
                    .iter()
                    .any(|assignment| assignment.role.is_host_mem())
            });
        if !has_transfer_roles {
            continue;
        }

        let contract = contracts
            .iter()
            .find(|contract| contract.bundle == binding.bundle)
            .expect("every bundle gets a contract");
        let ports = resolve_xfer_ports(dut, binding, contract)?;
        let XferPorts {
            read,
            write,
            data_width,
            cmd_addr_width,
            cmd_size_width,
        } = ports;

        let depth = declared.depth.unwrap_or(DEFAULT_DEPTH);
        if depth < 2 || !depth.is_power_of_two() {
            return Err(TerminatorError::DepthNotPowerOfTwo {
                bundle: binding.bundle.clone(),
                depth,
                suggestion: depth.max(2).next_power_of_two(),
            });
        }

        plans.push(HostMemPlan {
            bundle: binding.bundle.clone(),
            data_width,
            cmd_addr_width,
            cmd_size_width,
            read,
            write,
            depth,
            depth_defaulted: declared.depth.is_none(),
        });
    }

    Ok(plans)
}

/// An addressable slave interface of the DUT (`backing = "slave"`; `addr` is a
/// DUT input). The window bus connects straight to the DUT.
///
/// It uses the same roles as `MemPlan`, in the other direction. The harness has
/// no memory; the DUT is the target.
#[derive(Debug)]
pub struct SlavePlan {
    pub bundle: String,

    /// The address (a DUT input).
    pub addr: String,
    pub addr_width: usize,

    /// Read data (a DUT output), at most one window word. A narrower one
    /// reads back with zeros above it.
    pub rdata: String,
    pub width: usize,

    /// The write side. None for a read-only interface. A narrower `wdata`
    /// takes the low bits of the window word.
    pub wdata: Option<String>,
    pub wdata_width: usize,
    pub we: Option<String>,

    /// The declared latency. The decoder waits exactly this long; with 0 it
    /// holds the answer itself.
    pub latency: u32,
}

impl SlavePlan {
    /// The bytes it takes in the window. The address is passed as a word index.
    pub fn size_bytes(&self) -> usize {
        (1usize << self.addr_width) * (crate::regmap::WORD_BITS / 8)
    }
}

/// A `host_irq` terminator: the DUT's interrupt line, sent to the host as
/// INTA over PCIe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrqPlan {
    pub bundle: String,
    /// The DUT output. High while an interrupt is pending.
    pub port: String,
}

/// Resolves the `host_irq` bundle. There is at most one: INTx is one pin.
pub fn resolve_irqs(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
) -> Result<Vec<IrqPlan>, TerminatorError> {
    let mut plans: Vec<IrqPlan> = Vec::new();
    for binding in bindings {
        if manifest.bundle[&binding.bundle].backing != crate::manifest::Backing::HostIrq {
            continue;
        }
        if let Some(first) = plans.first() {
            return Err(TerminatorError::IrqMoreThanOne {
                first: first.bundle.clone(),
                second: binding.bundle.clone(),
            });
        }
        let shape = |what: String| TerminatorError::IrqShape {
            bundle: binding.bundle.clone(),
            what,
        };
        let [name] = binding.ports.as_slice() else {
            return Err(shape(format!("{} ports", binding.ports.len())));
        };
        let port = port_of(dut, name);
        if port.direction != PortDirection::Output {
            return Err(shape(format!(
                "`{name}` as {}",
                match port.direction {
                    PortDirection::Input => "an input".to_string(),
                    other => format!("a `{}` port", other.as_str()),
                }
            )));
        }
        match port.width() {
            Some(1) => {}
            Some(width) => return Err(shape(format!("`{name}` {width} bits wide"))),
            None => {
                return Err(shape(format!(
                    "`{name}` of a width that could not be resolved"
                )));
            }
        }
        plans.push(IrqPlan {
            bundle: binding.bundle.clone(),
            port: name.clone(),
        });
    }
    Ok(plans)
}

/// A `pcie_flr` terminator: the host's Function Level Reset, handed to the
/// DUT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlrPlan {
    pub bundle: String,
    /// The DUT input. High while the FLR is in progress.
    pub port: String,
    /// The DUT output that says it has finished, if it has one.
    pub done: Option<String>,
}

/// Resolves the `pcie_flr` bundle. There is at most one: the card has one
/// function.
pub fn resolve_flrs(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
) -> Result<Vec<FlrPlan>, TerminatorError> {
    let mut plans: Vec<FlrPlan> = Vec::new();
    for binding in bindings {
        if manifest.bundle[&binding.bundle].backing != crate::manifest::Backing::PcieFlr {
            continue;
        }
        if let Some(first) = plans.first() {
            return Err(TerminatorError::FlrMoreThanOne {
                first: first.bundle.clone(),
                second: binding.bundle.clone(),
            });
        }
        let shape = |what: String| TerminatorError::FlrShape {
            bundle: binding.bundle.clone(),
            what,
        };
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        for name in &binding.ports {
            let port = port_of(dut, name);
            match port.width() {
                Some(1) => {}
                Some(width) => return Err(shape(format!("`{name}` {width} bits wide"))),
                None => {
                    return Err(shape(format!(
                        "`{name}` of a width that could not be resolved"
                    )));
                }
            }
            match port.direction {
                PortDirection::Input => inputs.push(name.clone()),
                PortDirection::Output => outputs.push(name.clone()),
                other => return Err(shape(format!("`{name}` as a `{}` port", other.as_str()))),
            }
        }
        let (port, done) = match (inputs.as_slice(), outputs.as_slice()) {
            ([port], []) => (port.clone(), None),
            ([port], [done]) => (port.clone(), Some(done.clone())),
            _ => {
                return Err(shape(format!(
                    "{} inputs and {} outputs, not one input and at most one output",
                    inputs.len(),
                    outputs.len()
                )));
            }
        };
        plans.push(FlrPlan {
            bundle: binding.bundle.clone(),
            port,
            done,
        });
    }
    Ok(plans)
}

/// Resolves the `backing = "slave"` bundles. The contract resolution has already
/// checked the direction of `addr`. The other directions, the widths, and the
/// size in the window are checked here.
pub fn resolve_slaves(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
    contracts: &[BundleContract],
) -> Result<Vec<SlavePlan>, TerminatorError> {
    let mut plans = Vec::new();

    for binding in bindings {
        let declared = &manifest.bundle[&binding.bundle];
        if declared.backing != crate::manifest::Backing::Slave {
            continue;
        }
        let contract = contracts
            .iter()
            .find(|contract| contract.bundle == binding.bundle)
            .expect("every bundle gets a contract");
        // An APB slave goes to `resolve_apbs`, an AXI4 one to
        // `resolve_axi_slaves`.
        if matches!(contract.contract, Contract::Apb | Contract::Axi) {
            continue;
        }
        let role_port = |role: Role| {
            contract
                .roles
                .iter()
                .find(|r| r.role == role)
                .map(|r| r.port.clone())
        };
        let addr = role_port(Role::Addr).ok_or_else(|| TerminatorError::SlaveNeedsAddr {
            bundle: binding.bundle.clone(),
        })?;
        let latency = declared
            .latency
            .ok_or_else(|| TerminatorError::LatencyRequired {
                bundle: binding.bundle.clone(),
                backing: declared.backing.as_str().to_string(),
                why: "The harness reads `rdata` a fixed number of cycles after it drives `addr`.",
            })?;

        // A slave without a read port makes no sense. For writes only, one
        // register per port is enough.
        let rdata = role_port(Role::RData).ok_or_else(|| TerminatorError::SlaveNeedsRData {
            bundle: binding.bundle.clone(),
        })?;

        let addr_port = port_of(dut, &addr);
        let rdata_port = port_of(dut, &rdata);
        // The contract resolution checked `addr`. Check the rest against the
        // DUT declaration.
        expect_direction(
            Shape::Slave,
            &binding.bundle,
            rdata_port,
            Role::RData.as_str(),
            PortDirection::Output,
        )?;
        let addr_width = addr_port
            .width()
            .ok_or_else(|| TerminatorError::UnresolvedWidth {
                bundle: binding.bundle.clone(),
                port: addr.clone(),
            })?;
        // Whether it fits in the window. A word is 4 bytes, so the window
        // address is `addr` + 2 bits. The limit leaves room in the 32-bit
        // window for other regions and registers, and it also keeps
        // `1 << addr_width` from panicking on a 64-bit address.
        const MAX_SLAVE_ADDR_BITS: usize = 29;
        if addr_width > MAX_SLAVE_ADDR_BITS {
            return Err(TerminatorError::SlaveTooLarge {
                bundle: binding.bundle.clone(),
                addr_width,
                max: MAX_SLAVE_ADDR_BITS,
            });
        }
        let width = rdata_port
            .width()
            .ok_or_else(|| TerminatorError::UnresolvedWidth {
                bundle: binding.bundle.clone(),
                port: rdata.clone(),
            })?;

        // The window is 32 bits. A narrower port reads with zeros above it;
        // splitting a wider one is not implemented yet.
        if width > crate::regmap::WORD_BITS {
            return Err(TerminatorError::SlaveWiderThanAWord {
                bundle: binding.bundle.clone(),
                port: rdata.clone(),
                width,
            });
        }

        let wdata = role_port(Role::WData);
        let we = role_port(Role::We);
        let mut wdata_width = 0;
        // Write ports come in pairs. With only one, either when to write or
        // what to write is unknown.
        if wdata.is_some() != we.is_some() {
            return Err(TerminatorError::SlaveHalfWrite {
                bundle: binding.bundle.clone(),
            });
        }
        if let (Some(wdata), Some(we)) = (&wdata, &we) {
            let wdata_port = port_of(dut, wdata);
            let we_port = port_of(dut, we);
            for (port, role) in [(wdata_port, Role::WData), (we_port, Role::We)] {
                expect_direction(
                    Shape::Slave,
                    &binding.bundle,
                    port,
                    role.as_str(),
                    PortDirection::Input,
                )?;
            }
            expect_single_bit(&binding.bundle, we_port, Role::We.as_str())?;
            // emit gives it the low bits of the window write data.
            wdata_width = wdata_port
                .width()
                .ok_or_else(|| TerminatorError::UnresolvedWidth {
                    bundle: binding.bundle.clone(),
                    port: wdata.clone(),
                })?;
            if wdata_width > crate::regmap::WORD_BITS {
                return Err(TerminatorError::SlaveWiderThanAWord {
                    bundle: binding.bundle.clone(),
                    port: wdata.clone(),
                    width: wdata_width,
                });
            }
        }

        plans.push(SlavePlan {
            bundle: binding.bundle.clone(),
            addr,
            addr_width,
            rdata,
            width,
            wdata,
            wdata_width,
            we,
            latency,
        });
    }

    Ok(plans)
}

/// An APB slave in the DUT, mapped whole into the window: a `dmi` bundle (the
/// DMI of a RISC-V Debug Module) or a `slave` bundle with APB roles. The
/// harness is the APB master (`hns::apb_host`).
#[derive(Debug, Clone)]
pub struct ApbPlan {
    pub bundle: String,
    /// A `dmi` bundle. Its region is marked for `hio cpu`.
    pub debug_module: bool,
    pub psel: String,
    pub penable: String,
    pub pwrite: String,
    /// The byte address (a DUT input).
    pub paddr: String,
    pub addr_width: usize,
    pub pwdata: String,
    pub prdata: String,
    /// None when the slave always answers in the access phase.
    pub pready: Option<String>,
    /// The error answer (`slave` only). The harness counts the transfers it
    /// ends in `<bundle>_slverr`.
    pub pslverr: Option<String>,
    /// The Debug Module's reset port, which gets the harness reset. None:
    /// the Debug Module is on the DUT reset, and `hio reset` resets it too.
    pub reset: Option<String>,
}

impl ApbPlan {
    /// The bytes it takes in the window: the whole address range.
    pub fn size_bytes(&self) -> usize {
        1 << self.addr_width
    }
}

/// `paddr` bits: up to `0x7f` words (byte `0x1fc`) is what the Debug Spec
/// defines, so 9 bits at least.
pub const DMI_MIN_ADDR_BITS: usize = 9;
/// Far more than one Debug Module, and still a small part of the window.
pub const DMI_MAX_ADDR_BITS: usize = 16;
/// An APB `slave`: one 32-bit register at least, and the same 2^29 words as a
/// `slave` with a RAM port at most.
pub const APB_SLAVE_MIN_ADDR_BITS: usize = 2;
pub const APB_SLAVE_MAX_ADDR_BITS: usize = 31;

/// Resolves the APB bundles (`dmi`, and `slave` with APB roles), in bundle
/// name order.
pub fn resolve_apbs(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
    contracts: &[BundleContract],
) -> Result<Vec<ApbPlan>, TerminatorError> {
    let mut plans = Vec::new();
    for binding in bindings {
        let contract = contracts
            .iter()
            .find(|contract| contract.bundle == binding.bundle)
            .expect("every bundle gets a contract");
        let debug_module = match manifest.bundle[&binding.bundle].backing {
            crate::manifest::Backing::Dmi => true,
            crate::manifest::Backing::Slave if contract.contract == Contract::Apb => false,
            _ => continue,
        };
        let shape = if debug_module {
            Shape::Dmi
        } else {
            Shape::ApbSlave
        };
        // A Debug Module reports its errors in its own registers, so `dmi`
        // takes no `pslverr`.
        if let Some(stray) = contract
            .roles
            .iter()
            .find(|r| !r.role.is_apb() || (debug_module && r.role == Role::Pslverr))
        {
            return Err(if debug_module {
                TerminatorError::DmiStrayPort {
                    bundle: binding.bundle.clone(),
                    port: stray.port.clone(),
                }
            } else {
                TerminatorError::ApbSlaveStrayPort {
                    bundle: binding.bundle.clone(),
                    port: stray.port.clone(),
                }
            });
        }
        // `contract::check_apb` made sure the needed roles are there.
        let role_port = |role: Role| {
            contract
                .roles
                .iter()
                .find(|r| r.role == role)
                .map(|r| r.port.clone())
        };
        let needed = |role: Role| role_port(role).expect("checked by contract::check_apb");

        let inputs = [
            Role::Psel,
            Role::Penable,
            Role::Pwrite,
            Role::Paddr,
            Role::Pwdata,
        ];
        for role in inputs {
            let port = port_of(dut, &needed(role));
            expect_direction(
                shape,
                &binding.bundle,
                port,
                role.as_str(),
                PortDirection::Input,
            )?;
        }
        let pready = role_port(Role::Pready);
        let pslverr = role_port(Role::Pslverr);
        for (role, name) in [
            (Role::Prdata, Some(needed(Role::Prdata))),
            (Role::Pready, pready.clone()),
            (Role::Pslverr, pslverr.clone()),
        ] {
            if let Some(name) = name {
                let port = port_of(dut, &name);
                expect_direction(
                    shape,
                    &binding.bundle,
                    port,
                    role.as_str(),
                    PortDirection::Output,
                )?;
            }
        }
        for role in [Role::Psel, Role::Penable, Role::Pwrite] {
            expect_single_bit(&binding.bundle, port_of(dut, &needed(role)), role.as_str())?;
        }
        for (role, name) in [(Role::Pready, &pready), (Role::Pslverr, &pslverr)] {
            if let Some(name) = name {
                expect_single_bit(&binding.bundle, port_of(dut, name), role.as_str())?;
            }
        }
        // The window is 32 bits, and the Debug Module registers are too.
        for role in [Role::Pwdata, Role::Prdata] {
            let name = needed(role);
            let width =
                port_of(dut, &name)
                    .width()
                    .ok_or_else(|| TerminatorError::UnresolvedWidth {
                        bundle: binding.bundle.clone(),
                        port: name.clone(),
                    })?;
            if width != crate::regmap::WORD_BITS {
                return Err(TerminatorError::SlaveWidthNotAWord {
                    bundle: binding.bundle.clone(),
                    port: name,
                    width,
                });
            }
        }
        let paddr = needed(Role::Paddr);
        let addr_width =
            port_of(dut, &paddr)
                .width()
                .ok_or_else(|| TerminatorError::UnresolvedWidth {
                    bundle: binding.bundle.clone(),
                    port: paddr.clone(),
                })?;
        if debug_module {
            let what = if addr_width < DMI_MIN_ADDR_BITS {
                Some("is too few to reach every Debug Module register")
            } else if addr_width > DMI_MAX_ADDR_BITS {
                Some("is more than the window gives one Debug Module")
            } else {
                None
            };
            if let Some(what) = what {
                return Err(TerminatorError::DmiAddressWidth {
                    bundle: binding.bundle.clone(),
                    port: paddr,
                    addr_width,
                    what,
                    min: DMI_MIN_ADDR_BITS,
                    max: DMI_MAX_ADDR_BITS,
                });
            }
        } else {
            let what = if addr_width < APB_SLAVE_MIN_ADDR_BITS {
                Some("is too few to reach one 32-bit register")
            } else if addr_width > APB_SLAVE_MAX_ADDR_BITS {
                Some("does not fit the host window")
            } else {
                None
            };
            if let Some(what) = what {
                return Err(TerminatorError::ApbSlaveAddressWidth {
                    bundle: binding.bundle.clone(),
                    port: paddr,
                    addr_width,
                    what,
                    min: APB_SLAVE_MIN_ADDR_BITS,
                    max: APB_SLAVE_MAX_ADDR_BITS,
                });
            }
        }

        let reset = manifest.bundle[&binding.bundle].reset.clone();
        if let Some(name) = &reset {
            let what = match dut.ports.iter().find(|port| &port.name == name) {
                None => Some("the DUT has no such port"),
                Some(port) if port.direction != PortDirection::Input => Some("it is not an input"),
                Some(port)
                    if !port
                        .signals
                        .iter()
                        .any(|signal| signal.role == crate::dut::SignalRole::Reset) =>
                {
                    Some("it is not of type `reset`")
                }
                Some(_) => None,
            };
            if let Some(what) = what {
                return Err(TerminatorError::DmiResetPort {
                    bundle: binding.bundle.clone(),
                    port: name.clone(),
                    what,
                });
            }
        }

        plans.push(ApbPlan {
            bundle: binding.bundle.clone(),
            debug_module,
            psel: needed(Role::Psel),
            penable: needed(Role::Penable),
            pwrite: needed(Role::Pwrite),
            paddr,
            addr_width,
            pwdata: needed(Role::Pwdata),
            prdata: needed(Role::Prdata),
            pready,
            pslverr,
            reset,
        });
    }
    Ok(plans)
}

/// Resolves the `bram` / `bram_preload` bundles, in bundle name order.
pub fn resolve_memories(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
    contracts: &[BundleContract],
) -> Result<Vec<MemPlan>, TerminatorError> {
    let mut plans = Vec::new();

    for binding in bindings {
        let declared = &manifest.bundle[&binding.bundle];
        if !declared.backing.is_memory() {
            continue;
        }
        // The interface shape decides the terminator. For the same `bram`,
        // `resolve_axi_mems` takes AXI4 and `resolve_host_mem` takes the
        // transfer level. Only fixed-latency interfaces are handled here.
        if binding
            .ports
            .iter()
            .any(|name| port_of(dut, name).axi4.is_some())
        {
            continue;
        }
        let backing = declared.backing.as_str().to_string();

        let contract = contracts
            .iter()
            .find(|contract| contract.bundle == binding.bundle)
            .expect("every bundle gets a contract");
        if contract.roles.iter().any(|r| r.role.is_host_mem()) {
            continue;
        }

        // Only fixed latency is left. A handshake without transfer-level roles
        // would reach no terminator, so reject it here.
        if contract.contract != Contract::FixedLatency {
            return Err(TerminatorError::MemoryContract {
                bundle: binding.bundle.clone(),
                backing,
                contract: contract.contract.as_str().to_string(),
            });
        }

        let roles = || {
            contract
                .roles
                .iter()
                .map(|assignment| format!("    {} = {}", assignment.role.as_str(), assignment.port))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let find = |want: Role| {
            contract
                .roles
                .iter()
                .find(|assignment| assignment.role == want)
        };

        let (Some(addr), Some(rdata)) = (find(Role::Addr), find(Role::RData)) else {
            return Err(TerminatorError::MemoryRoleMissing {
                bundle: binding.bundle.clone(),
                backing,
                role: if find(Role::Addr).is_none() {
                    "addr"
                } else {
                    "rdata"
                },
                roles: roles(),
            });
        };

        let addr_port = port_of(dut, &addr.port);
        expect_direction(
            Shape::Memory,
            &binding.bundle,
            addr_port,
            Role::Addr.as_str(),
            PortDirection::Output,
        )?;
        let rdata_port = port_of(dut, &rdata.port);
        expect_direction(
            Shape::Memory,
            &binding.bundle,
            rdata_port,
            Role::RData.as_str(),
            PortDirection::Input,
        )?;

        let addr_width = addr_port
            .width()
            .ok_or_else(|| TerminatorError::UnresolvedWidth {
                bundle: binding.bundle.clone(),
                port: addr.port.clone(),
            })?;
        let width = rdata_port
            .width()
            .ok_or_else(|| TerminatorError::UnresolvedWidth {
                bundle: binding.bundle.clone(),
                port: rdata.port.clone(),
            })?;

        // The entry width. Replaced below if the write channel is wider.
        let mut entry_width = width;

        // The write ports come in pairs. Only one of them is always an error.
        let write = match (find(Role::WData), find(Role::We)) {
            (Some(wdata), Some(we)) => {
                if declared.backing == Backing::BramPreload {
                    return Err(TerminatorError::PreloadIsReadOnly {
                        bundle: binding.bundle.clone(),
                        port: wdata.port.clone(),
                        role: "wdata",
                    });
                }
                let wdata_port = port_of(dut, &wdata.port);
                expect_direction(
                    Shape::Memory,
                    &binding.bundle,
                    wdata_port,
                    Role::WData.as_str(),
                    PortDirection::Output,
                )?;
                // The write can be wider than the read. For example, a CPU core
                // drains its store buffer as one 64-byte line, and reads the
                // same data as eight 64-bit words. One entry is the write width,
                // and middle address bits select the read slice in the entry.
                //
                // Without an integer ratio, the entry cannot be sliced, so
                // reject it.
                let wdata_width =
                    wdata_port
                        .width()
                        .ok_or_else(|| TerminatorError::UnresolvedWidth {
                            bundle: binding.bundle.clone(),
                            port: wdata.port.clone(),
                        })?;
                if wdata_width < width
                    || wdata_width % width != 0
                    || !(wdata_width / width).is_power_of_two()
                {
                    return Err(TerminatorError::WriteWidthMismatch {
                        bundle: binding.bundle.clone(),
                        wdata: wdata.port.clone(),
                        wdata_width,
                        rdata: rdata.port.clone(),
                        rdata_width: width,
                    });
                }
                entry_width = wdata_width;

                // Byte strobe: one bit per byte of the entry.
                let wstrb = match find(Role::Wstrb) {
                    Some(assignment) => {
                        let port = port_of(dut, &assignment.port);
                        expect_direction(
                            Shape::Memory,
                            &binding.bundle,
                            port,
                            Role::Wstrb.as_str(),
                            PortDirection::Output,
                        )?;
                        let strobe_width =
                            port.width()
                                .ok_or_else(|| TerminatorError::UnresolvedWidth {
                                    bundle: binding.bundle.clone(),
                                    port: assignment.port.clone(),
                                })?;
                        if entry_width % 8 != 0 || strobe_width != entry_width / 8 {
                            return Err(TerminatorError::StrobeWidthMismatch {
                                bundle: binding.bundle.clone(),
                                wstrb: assignment.port.clone(),
                                strobe_width,
                                entry_width,
                                expected: entry_width / 8,
                            });
                        }
                        Some(assignment.port.clone())
                    }
                    None => None,
                };
                let we_port = port_of(dut, &we.port);
                expect_direction(
                    Shape::Memory,
                    &binding.bundle,
                    we_port,
                    Role::We.as_str(),
                    PortDirection::Output,
                )?;
                expect_single_bit(&binding.bundle, we_port, Role::We.as_str())?;
                Some(MemWrite {
                    wdata: wdata.port.clone(),
                    we: we.port.clone(),
                    we_invert: we.invert,
                    wstrb,
                })
            }
            (Some(wdata), None) => {
                if declared.backing == Backing::BramPreload {
                    return Err(TerminatorError::PreloadIsReadOnly {
                        bundle: binding.bundle.clone(),
                        port: wdata.port.clone(),
                        role: "wdata",
                    });
                }
                return Err(TerminatorError::IncompleteWritePort {
                    bundle: binding.bundle.clone(),
                    present: "wdata",
                    missing: "we",
                });
            }
            (None, Some(we)) => {
                if declared.backing == Backing::BramPreload {
                    return Err(TerminatorError::PreloadIsReadOnly {
                        bundle: binding.bundle.clone(),
                        port: we.port.clone(),
                        role: "we",
                    });
                }
                return Err(TerminatorError::IncompleteWritePort {
                    bundle: binding.bundle.clone(),
                    present: "we",
                    missing: "wdata",
                });
            }
            (None, None) => None,
        };

        // Whether the address counts bytes or entries is never inferred. For
        // 8-bit words there is no difference; otherwise it must be written.
        // A word index counts read words, so a wider entry drops more bits.
        let word_shift = (entry_width / width).trailing_zeros() as usize;
        let addr_shift = match (declared.addressing, entry_width) {
            (Some(Addressing::Word), _) => word_shift,
            (None, 8) => 0,
            (Some(Addressing::Byte), _) => {
                let bytes = entry_width.div_ceil(8);
                if entry_width % 8 != 0 || !bytes.is_power_of_two() {
                    return Err(TerminatorError::ByteAddressNeedsWholeBytes {
                        bundle: binding.bundle.clone(),
                        width: entry_width,
                    });
                }
                bytes.trailing_zeros() as usize
            }
            (None, _) => {
                return Err(TerminatorError::AddressingUnknown {
                    bundle: binding.bundle.clone(),
                    port: addr.port.clone(),
                    width: entry_width,
                    bytes: entry_width.div_ceil(8),
                });
            }
        };
        if addr_shift >= addr_width {
            return Err(TerminatorError::AddressTooNarrowForBytes {
                bundle: binding.bundle.clone(),
                port: addr.port.clone(),
                bits: addr_width,
                shift: addr_shift,
            });
        }

        // The default covers the address space exactly, so no access can be out
        // of range. The reach is not the address width itself: for a byte
        // address, the low bits are the offset inside the entry.
        let reach = 1u64.checked_shl((addr_width - addr_shift) as u32);
        // Read enable. Without it, every cycle reads (plain BRAM behavior).
        let enable = match find(Role::Re) {
            Some(assignment) => {
                let port = port_of(dut, &assignment.port);
                expect_direction(
                    Shape::Memory,
                    &binding.bundle,
                    port,
                    Role::Re.as_str(),
                    PortDirection::Output,
                )?;
                expect_single_bit(&binding.bundle, port, Role::Re.as_str())?;
                Some(Enable {
                    port: assignment.port.clone(),
                    invert: assignment.invert,
                })
            }
            None => None,
        };

        let depth = match declared.depth {
            Some(depth) => u64::from(depth),
            None => match reach {
                Some(reach) if reach <= MAX_DEFAULT_WORDS => reach,
                _ => {
                    return Err(TerminatorError::AddressSpaceTooLarge {
                        bundle: binding.bundle.clone(),
                        bits: addr_width - addr_shift,
                        words: match reach {
                            Some(reach) => reach.to_string(),
                            None => format!("2^{}", addr_width - addr_shift),
                        },
                    });
                }
            },
        };
        if depth < 2 || !depth.is_power_of_two() {
            return Err(TerminatorError::DepthNotPowerOfTwo {
                bundle: binding.bundle.clone(),
                depth: depth.min(u64::from(u32::MAX)) as u32,
                suggestion: depth.max(2).next_power_of_two().min(u64::from(u32::MAX)) as u32,
            });
        }
        // Entries the DUT cannot reach are reported too: storage nobody can
        // address is useless.
        if let Some(reach) = reach
            && depth > reach
        {
            return Err(TerminatorError::DepthExceedsAddress {
                bundle: binding.bundle.clone(),
                depth,
                bits: addr_width - addr_shift,
                reach,
            });
        }

        let dut_base = declared.dut_base.unwrap_or(0);
        let base_steps = if dut_base == 0 {
            0
        } else {
            // Bytes per step of the DUT address.
            let step = match declared.addressing {
                Some(Addressing::Word) if width % 8 != 0 => {
                    return Err(TerminatorError::DutBaseNeedsWholeBytes {
                        bundle: binding.bundle.clone(),
                        port: addr.port.clone(),
                        width,
                    });
                }
                Some(Addressing::Word) => (width / 8) as u64,
                _ => 1,
            };
            if !dut_base.is_multiple_of(step) {
                return Err(TerminatorError::DutBaseNotAligned {
                    bundle: binding.bundle.clone(),
                    port: addr.port.clone(),
                    base: dut_base,
                    step,
                    below: dut_base - dut_base % step,
                    above: dut_base - dut_base % step + step,
                });
            }
            let base_steps = dut_base / step;
            // The subtraction is done in the DUT's address width. Below the
            // base, the result wraps to a large number and is out of range,
            // as long as the whole memory fits under the top of the address
            // space. Then no comparator is needed.
            let end = u128::from(base_steps) + (u128::from(depth) << addr_shift);
            if end > 1u128 << addr_width.min(127) {
                return Err(TerminatorError::DutBasePastAddress {
                    bundle: binding.bundle.clone(),
                    port: addr.port.clone(),
                    base: dut_base,
                    end: end * u128::from(step),
                    bits: addr_width,
                });
            }
            base_steps
        };

        let access = declared.access.unwrap_or_default();
        // A moving window only makes sense for a region. An indirect port
        // reaches the whole memory with two registers, so there is no window
        // to move.
        if declared.aperture.is_some() && access != crate::manifest::MemAccess::Region {
            return Err(TerminatorError::ApertureNeedsARegion {
                bundle: binding.bundle.clone(),
            });
        }
        let entry_bytes = entry_width.div_ceil(32).max(1) * 4;
        let aperture_bytes = aperture_of(
            &binding.bundle,
            declared.aperture,
            depth * entry_bytes as u64,
            entry_bytes,
        )?;

        plans.push(MemPlan {
            bundle: binding.bundle.clone(),
            read_only: declared.backing == Backing::BramPreload,
            addr: addr.port.clone(),
            addr_width,
            addr_shift,
            rdata: rdata.port.clone(),
            width,
            entry_width,
            write,
            enable,
            latency: declared
                .latency
                .ok_or_else(|| TerminatorError::LatencyRequired {
                    bundle: binding.bundle.clone(),
                    backing: declared.backing.as_str().to_string(),
                    why: "The harness builds this memory to return read data a fixed number of cycles after the address.",
                })?,
            depth,
            depth_from_addr: declared.depth.is_none(),
            access,
            aperture_bytes,
            dut_base,
            base_steps,
        });
    }

    Ok(plans)
}

/// The largest default depth (in words). Above it, `depth` must be written.
/// Asking "how many do you need" gives a clearer fix than trying to build the
/// whole address space and failing with "does not fit".
const MAX_DEFAULT_WORDS: u64 = 1 << 20;

/// Resolves the `host_poll_fifo` bundles, in bundle name order (the order of
/// `bindings`).
pub fn resolve(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
    contracts: &[BundleContract],
) -> Result<Vec<FifoPlan>, TerminatorError> {
    let mut plans = Vec::new();

    for binding in bindings {
        let declared = &manifest.bundle[&binding.bundle];

        if declared.backing != Backing::HostPollFifo {
            // Only host_poll_fifo and memories take `depth`. Do not drop it
            // silently. `host_mem` has a size too: `depth` is the range on the
            // host.
            let stand_in_memory = declared.backing.is_host_mem();
            // `dram` has a size too. It is not in `is_memory`, because then
            // `resolve_memories` would take it and silently build a BRAM in
            // place of the controller. So `depth` is checked separately.
            let sized = declared.backing.is_memory()
                || matches!(declared.backing, Backing::Dram | Backing::Trace);
            if declared.depth.is_some() && !sized && !stand_in_memory {
                return Err(TerminatorError::DepthNotApplicable {
                    bundle: binding.bundle.clone(),
                    backing: declared.backing.as_str().to_string(),
                });
            }
            continue;
        }

        let contract = contracts
            .iter()
            .find(|contract| contract.bundle == binding.bundle)
            .expect("every bundle gets a contract");

        // Only contracts where the start of each beat is a visible signal.
        if !matches!(
            contract.contract,
            Contract::ValidReady | Contract::ValidOnly
        ) {
            return Err(TerminatorError::UnsupportedContract {
                bundle: binding.bundle.clone(),
                contract: contract.contract.as_str().to_string(),
            });
        }

        let mut valid = None;
        let mut ready = None;
        let mut data = Vec::new();
        for assignment in &contract.roles {
            match assignment.role {
                Role::Valid => valid = Some(assignment),
                Role::Ready => ready = Some(assignment),
                Role::Data => data.push(assignment),
                _ => {}
            }
        }

        // Role resolution has passed, so valid exists (it fails there otherwise).
        let valid = valid.expect("valid_ready / valid_only always resolve a valid");

        if data.len() != 1 {
            return Err(TerminatorError::DataPortCount {
                bundle: binding.bundle.clone(),
                count: data.len(),
                roles: contract
                    .roles
                    .iter()
                    .map(|assignment| {
                        format!("    {} = {}", assignment.role.as_str(), assignment.port)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            });
        }
        let data = data[0];

        // The direction comes from the DUT declaration (symbol table), never
        // from the name.
        let valid_port = port_of(dut, &valid.port);
        expect_direction(
            Shape::Fifo,
            &binding.bundle,
            valid_port,
            Role::Valid.as_str(),
            PortDirection::Output,
        )?;
        let data_port = port_of(dut, &data.port);
        expect_direction(
            Shape::Fifo,
            &binding.bundle,
            data_port,
            Role::Data.as_str(),
            PortDirection::Output,
        )?;

        expect_single_bit(&binding.bundle, valid_port, Role::Valid.as_str())?;

        let ready = match ready {
            Some(assignment) => {
                let port = port_of(dut, &assignment.port);
                expect_direction(
                    Shape::Fifo,
                    &binding.bundle,
                    port,
                    Role::Ready.as_str(),
                    PortDirection::Input,
                )?;
                expect_single_bit(&binding.bundle, port, Role::Ready.as_str())?;
                Some(Ready {
                    port: assignment.port.clone(),
                    invert: assignment.invert,
                })
            }
            None => None,
        };

        let width = data_port
            .width()
            .ok_or_else(|| TerminatorError::UnresolvedWidth {
                bundle: binding.bundle.clone(),
                port: data.port.clone(),
            })?;

        let depth = declared.depth.unwrap_or(DEFAULT_DEPTH);
        if depth < 2 || !depth.is_power_of_two() {
            return Err(TerminatorError::DepthNotPowerOfTwo {
                bundle: binding.bundle.clone(),
                depth,
                suggestion: depth.max(2).next_power_of_two(),
            });
        }

        plans.push(FifoPlan {
            bundle: binding.bundle.clone(),
            valid: valid.port.clone(),
            valid_invert: valid.invert,
            data: data.port.clone(),
            width,
            ready,
            depth,
            depth_defaulted: declared.depth.is_none(),
        });
    }

    Ok(plans)
}

/// The RVFI signals a trace knows, with their widths.
const RVFI_FIELDS: &[(&str, RvfiWidth)] = &[
    ("order", RvfiWidth::Bits(64)),
    ("insn", RvfiWidth::Bits(32)),
    ("trap", RvfiWidth::Bits(1)),
    ("halt", RvfiWidth::Bits(1)),
    ("intr", RvfiWidth::Bits(1)),
    ("mode", RvfiWidth::Bits(2)),
    ("ixl", RvfiWidth::Bits(2)),
    ("rs1_addr", RvfiWidth::Bits(5)),
    ("rs2_addr", RvfiWidth::Bits(5)),
    ("rs1_rdata", RvfiWidth::Xlen),
    ("rs2_rdata", RvfiWidth::Xlen),
    ("rd_addr", RvfiWidth::Bits(5)),
    ("rd_wdata", RvfiWidth::Xlen),
    ("pc_rdata", RvfiWidth::Xlen),
    ("pc_wdata", RvfiWidth::Xlen),
    ("mem_addr", RvfiWidth::Xlen),
    ("mem_rmask", RvfiWidth::Bytes),
    ("mem_wmask", RvfiWidth::Bytes),
    ("mem_rdata", RvfiWidth::Xlen),
    ("mem_wdata", RvfiWidth::Xlen),
    ("mem_fault", RvfiWidth::Bits(1)),
    ("mem_fault_rmask", RvfiWidth::Bytes),
    ("mem_fault_wmask", RvfiWidth::Bytes),
];

/// Without these, a record does not say which instruction it is.
const RVFI_REQUIRED: &[&str] = &["pc_rdata", "insn"];

#[derive(Clone, Copy)]
enum RvfiWidth {
    Bits(usize),
    /// The width of `pc_rdata`.
    Xlen,
    /// XLEN / 8: a byte mask.
    Bytes,
}

/// Resolves the `trace` bundles, in bundle name order.
pub fn resolve_traces(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
    contracts: &[BundleContract],
    prefixes: &DirectionPrefixes,
) -> Result<Vec<TracePlan>, TerminatorError> {
    let mut plans = Vec::new();

    for binding in bindings {
        let declared = &manifest.bundle[&binding.bundle];
        if declared.backing != Backing::Trace {
            continue;
        }
        let bundle = &binding.bundle;
        let contract = contracts
            .iter()
            .find(|contract| &contract.bundle == bundle)
            .expect("every bundle gets a contract");

        // The ring always has room, so it never needs to stall the DUT.
        if contract.contract != Contract::ValidOnly {
            return Err(TerminatorError::TraceContract {
                bundle: bundle.clone(),
                contract: contract.contract.as_str().to_string(),
            });
        }

        let mut valid = None;
        let mut payload = Vec::new();
        for assignment in &contract.roles {
            match assignment.role {
                Role::Valid => valid = Some(assignment),
                _ => payload.push(assignment),
            }
        }
        let valid = valid.expect("valid_only always resolves a valid");
        let valid_port = port_of(dut, &valid.port);
        expect_direction(
            Shape::Trace,
            bundle,
            valid_port,
            Role::Valid.as_str(),
            PortDirection::Output,
        )?;
        expect_single_bit(bundle, valid_port, Role::Valid.as_str())?;

        if payload.is_empty() {
            return Err(TerminatorError::TraceNoPayload {
                bundle: bundle.clone(),
            });
        }

        let mut fields: Vec<TraceField> = Vec::new();
        let mut lsb = TRACE_CYCLE_BITS;
        for assignment in payload {
            let port = port_of(dut, &assignment.port);
            expect_direction(
                Shape::Trace,
                bundle,
                port,
                assignment.role.as_str(),
                PortDirection::Output,
            )?;
            let width = port
                .width()
                .ok_or_else(|| TerminatorError::UnresolvedWidth {
                    bundle: bundle.clone(),
                    port: assignment.port.clone(),
                })?;
            let bare = prefixes.strip(&assignment.port, port.direction);
            let mut name = bare
                .strip_prefix(bundle.as_str())
                .and_then(|rest| rest.strip_prefix('_'))
                .unwrap_or(bare);
            if declared.format == Some(TraceFormat::Rvfi) {
                name = name.strip_prefix("rvfi_").unwrap_or(name);
            }
            if name == TRACE_CYCLE || fields.iter().any(|field| field.name == name) {
                return Err(TerminatorError::TraceFieldClash {
                    bundle: bundle.clone(),
                    port: assignment.port.clone(),
                    field: name.to_string(),
                });
            }
            fields.push(TraceField {
                name: name.to_string(),
                port: assignment.port.clone(),
                lsb,
                width,
            });
            lsb += width;
        }

        if declared.format == Some(TraceFormat::Rvfi) {
            check_rvfi(bundle, &fields)?;
        }

        // Not a fact about the DUT, but the records are what is being looked
        // for: a ring too short to hold them is not noticed until it is read.
        let depth = declared
            .depth
            .ok_or_else(|| TerminatorError::TraceDepthMissing {
                bundle: bundle.clone(),
            })?;
        if depth < 2 || !depth.is_power_of_two() {
            return Err(TerminatorError::DepthNotPowerOfTwo {
                bundle: bundle.clone(),
                depth,
                suggestion: depth.max(2).next_power_of_two(),
            });
        }

        plans.push(TracePlan {
            bundle: bundle.clone(),
            valid: valid.port.clone(),
            valid_invert: valid.invert,
            fields,
            depth,
            format: declared.format,
        });
    }

    Ok(plans)
}

/// Checks the fields against RVFI: known names, widths that agree with
/// XLEN, and the fields that say which instruction a record is.
fn check_rvfi(bundle: &str, fields: &[TraceField]) -> Result<(), TerminatorError> {
    for want in RVFI_REQUIRED {
        if fields.iter().all(|field| field.name != *want) {
            return Err(TerminatorError::RvfiMissing {
                bundle: bundle.to_string(),
                field: want,
            });
        }
    }
    let pc = fields
        .iter()
        .find(|field| field.name == "pc_rdata")
        .expect("checked above");
    if pc.width != 32 && pc.width != 64 {
        return Err(TerminatorError::RvfiWidth {
            bundle: bundle.to_string(),
            port: pc.port.clone(),
            width: pc.width,
            want: "32 or 64 bits (XLEN)".to_string(),
        });
    }
    let xlen = pc.width;
    for field in fields {
        // The CSR extension names its fields after the CSR.
        if field.name.starts_with("csr_") {
            continue;
        }
        let Some((_, width)) = RVFI_FIELDS.iter().find(|(name, _)| *name == field.name) else {
            return Err(TerminatorError::RvfiUnknown {
                bundle: bundle.to_string(),
                port: field.port.clone(),
                field: field.name.clone(),
                known: RVFI_FIELDS
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        };
        let (want, text) = match width {
            RvfiWidth::Bits(bits) => (*bits, format!("{bits} bits")),
            RvfiWidth::Xlen => (xlen, format!("{xlen} bits, as wide as `pc_rdata`")),
            RvfiWidth::Bytes => (xlen / 8, format!("{} bits, one per byte of XLEN", xlen / 8)),
        };
        if field.width != want {
            return Err(TerminatorError::RvfiWidth {
                bundle: bundle.to_string(),
                port: field.port.clone(),
                width: field.width,
                want: text,
            });
        }
    }
    Ok(())
}

/// The resolved plan of one memory that the DUT presents as an AXI4 master.
///
/// The interface shape and the location are separate. Whether the interface is
/// AXI4 or plain addr/data is a fact about the DUT; `backing` says where the
/// data lives. This plan is for AXI4: with `bram`, `hns::axi_mem` sits behind
/// it, and with `dram`, a memory controller. The DUT sees the same interface in
/// both cases.
#[derive(Debug, PartialEq, Eq)]
pub struct AxiMemPlan {
    pub bundle: String,

    /// Where it lives: `hns::axi_mem` for `Bram`, the controller for `Dram`.
    pub backing: Backing,

    /// The DUT port name (a modport port).
    pub port: String,

    /// `std::axi4_pkg::<..>`, with the same arguments the DUT used.
    pub pkg: String,

    pub addr_width: u32,
    pub data_bytes: u32,
    pub id_width: u32,

    /// The number of words.
    pub depth: u32,

    /// Whether the default depth was used. Kept so that the value is reported.
    pub depth_defaulted: bool,

    /// The bytes shown in the window. Smaller than the memory means a moving
    /// window.
    pub aperture_bytes: Option<usize>,

    /// The DUT address of the first byte, 0 without `dut_base`. With one,
    /// `hns::axi_range` takes it off and answers the rest with DECERR.
    pub dut_base: u64,

    /// A `dram` whose DUT side is a transfer-level port. `hns::xfer2axi`
    /// turns it into the AXI4 master, and `port` is empty.
    pub xfer: Option<XferPorts>,
}

impl AxiMemPlan {
    /// The number of 32-bit words seen from the window. The window is 32 bits,
    /// so a wider AXI bus splits each word into several. `hns::axi_host` does
    /// the split, so the window decoder keeps the simple form of one entry per
    /// word.
    pub fn region_words(&self) -> u64 {
        self.depth as u64 * (self.data_bytes as u64 / 4)
    }

    /// The width of the word index in the window.
    pub fn word_addr_width(&self) -> usize {
        (self.region_words().trailing_zeros() as usize).max(1)
    }
}

/// A board without `[provides] dram`. If one of its configs adds it, say which.
fn dram_not_on_target(bundle: &str, target: &hns_targets::Target) -> TerminatorError {
    let board = target.name.split(':').next().unwrap_or(&target.name);
    let configs: Vec<String> = hns_targets::list()
        .into_iter()
        .flatten()
        .find(|listed| listed.name == board)
        .map(|listed| listed.configs)
        .unwrap_or_default();
    let how = if configs.is_empty() {
        "The harness has no memory controller for this board. Use `backing = \"bram\"`; the same DUT works with it.".to_string()
    } else {
        // Name the memory part each config describes, so the user can match it
        // against the chips on the board.
        let lines: Vec<String> = configs
            .iter()
            .map(|config| {
                let name = format!("{board}:{config}");
                let device = hns_targets::resolve(&name, &[])
                    .ok()
                    .and_then(|t| hns_targets::dram(&t)?.device)
                    .unwrap_or_default();
                format!("    --target {name}    {device}")
                    .trim_end()
                    .to_string()
            })
            .collect();
        format!(
            "A config of this board adds it. Pick the one that matches the memory chips on the board:\n\n{}",
            lines.join("\n")
        )
    };
    TerminatorError::DramNotOnTarget {
        bundle: bundle.to_string(),
        target: target.name.clone(),
        how,
    }
}

/// An AXI4 slave port of the DUT (`slave`), which the host reads and writes
/// through `hns::axi_host`, one word at a time.
#[derive(Debug)]
pub struct AxiSlavePlan {
    pub bundle: String,
    /// The DUT port name (a `::slave` modport port).
    pub port: String,
    /// `std::axi4_pkg::<..>`, with the same arguments the DUT used.
    pub pkg: String,
    pub addr_width: u32,
    pub data_bytes: u32,
    /// The bytes shown in the window. Smaller than the address space means a
    /// moving window.
    pub aperture_bytes: Option<usize>,
}

impl AxiSlavePlan {
    /// The 32-bit words of the whole address space. `hns::axi_host` picks the
    /// word inside a wider beat.
    pub fn region_words(&self) -> u64 {
        1u64 << (self.addr_width - 2)
    }

    /// The width of the word index.
    pub fn word_addr_width(&self) -> usize {
        self.addr_width as usize - 2
    }
}

/// The widest address an AXI4 `slave` maps whole into the window, as for a
/// `slave` RAM port (2^29 words).
pub const AXI_SLAVE_MAX_ADDR_BITS: u32 = 31;

/// Resolves the `slave` bundles with an AXI4 port.
pub fn resolve_axi_slaves(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
) -> Result<Vec<AxiSlavePlan>, TerminatorError> {
    let mut plans = Vec::new();
    for binding in bindings {
        let declared = &manifest.bundle[&binding.bundle];
        if declared.backing != Backing::Slave {
            continue;
        }
        let Some((port, axi4)) = binding.ports.iter().find_map(|name| {
            let port = port_of(dut, name);
            port.axi4.as_ref().map(|axi4| (port, axi4))
        }) else {
            continue;
        };
        if axi4.modport != "slave" {
            return Err(TerminatorError::AxiSlaveNotASlave {
                bundle: binding.bundle.clone(),
                port: port.name.clone(),
                modport: axi4.modport.clone(),
            });
        }
        // The window is 32 bits, so a word must not span two beats.
        if axi4.data_bytes() < 4 || !axi4.data_bytes().is_power_of_two() {
            return Err(TerminatorError::AxiMemWidthNotAWordMultiple {
                bundle: binding.bundle.clone(),
                port: port.name.clone(),
                bits: axi4.data_width(),
            });
        }
        let addr_width = axi4.addr_width();
        if declared.aperture.is_none() && addr_width > AXI_SLAVE_MAX_ADDR_BITS {
            return Err(TerminatorError::AxiSlaveTooLarge {
                bundle: binding.bundle.clone(),
                port: port.name.clone(),
                addr_width,
                max: AXI_SLAVE_MAX_ADDR_BITS,
            });
        }
        let aperture_bytes = aperture_of(
            &binding.bundle,
            declared.aperture,
            1u64 << addr_width.min(63),
            4,
        )?;
        plans.push(AxiSlavePlan {
            bundle: binding.bundle.clone(),
            port: port.name.clone(),
            pkg: axi4.pkg(),
            addr_width,
            data_bytes: axi4.data_bytes(),
            aperture_bytes,
        });
    }
    Ok(plans)
}

/// The number of words the controller can reach, only from a measured value.
///
/// The address width is `axi_addr_bits` in the target description, read from
/// the generated IP. Without it, the result is `None`; no default is used.
fn controller_reach(target: Option<&hns_targets::Target>, data_bytes: u32) -> Option<u32> {
    let bits = hns_targets::dram(target?)?.axi_addr_bits?;
    // words = 2^bits / word bytes; both are powers of two, so it divides
    // exactly. Cap it at a power of two that fits `depth` (`u32`). Do not return
    // `None` for a large value: `None` means "no description", and the error
    // would then ask the user to name a board. `checked_shl` avoids a panic for
    // 64 bits or more.
    let words = 1u64
        .checked_shl(bits)
        .map_or(u64::MAX, |bytes| bytes / u64::from(data_bytes));
    Some(words.min(1 << 31) as u32)
}

/// Resolves the memory bundles with an AXI4 master.
///
/// The interface shape decides, not `backing`: any location (`bram` / `dram`)
/// comes here when the DUT side is AXI4.
pub fn resolve_axi_mems(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
    contracts: &[BundleContract],
    target: Option<&hns_targets::Target>,
) -> Result<Vec<AxiMemPlan>, TerminatorError> {
    let mut plans = Vec::new();

    for binding in bindings {
        let declared = &manifest.bundle[&binding.bundle];
        if !declared.backing.is_memory() && declared.backing != Backing::Dram {
            continue;
        }
        let contract = contracts
            .iter()
            .find(|contract| contract.bundle == binding.bundle)
            .expect("every bundle gets a contract");
        let axi = binding.ports.iter().find_map(|name| {
            let port = port_of(dut, name);
            port.axi4.as_ref().map(|axi4| (port, axi4))
        });
        // A `dram` with a transfer-level port gets `hns::xfer2axi` in front.
        // `bram` serves that port itself (`resolve_host_mem`).
        let xfer = if axi.is_none()
            && declared.backing == Backing::Dram
            && contract.roles.iter().any(|r| r.role.is_host_mem())
        {
            Some(resolve_xfer_ports(dut, binding, contract)?)
        } else {
            None
        };
        let (port_name, data_bytes, data_port) = match (&axi, &xfer) {
            (Some((port, axi4)), _) => {
                // The harness is always the slave, so the DUT must be the
                // master. A one-sided modport (`write_master` /
                // `read_master`) would leave the other channels undriven while
                // they only look connected, so reject it.
                if axi4.modport != "master" {
                    return Err(TerminatorError::AxiMemNotAMaster {
                        bundle: binding.bundle.clone(),
                        port: port.name.clone(),
                        modport: axi4.modport.clone(),
                    });
                }
                // `bram_preload` on AXI4 is not built yet. Accepting only
                // `read_master` would need a read-only form of `hns::axi_mem`.
                if declared.backing == Backing::BramPreload {
                    return Err(TerminatorError::AxiPreloadUnsupported {
                        bundle: binding.bundle.clone(),
                    });
                }
                (port.name.clone(), axi4.data_bytes(), port.name.clone())
            }
            (None, Some(xfer)) => {
                // A command address is already an offset into the memory.
                if declared.dut_base.is_some() {
                    return Err(TerminatorError::KeyNotApplicable {
                        bundle: binding.bundle.clone(),
                        key: "dut_base",
                        shape: "a transfer-level `dram` port",
                        takes: "`depth`, `aperture`".to_string(),
                    });
                }
                let data_port = xfer
                    .read
                    .as_ref()
                    .map(|r| r.data.clone())
                    .or_else(|| xfer.write.as_ref().map(|w| w.data.clone()))
                    .expect("a transfer-level port has a side");
                (String::new(), (xfer.data_width / 8) as u32, data_port)
            }
            (None, None) => {
                // `bram` has other shapes. The `dram` controller takes AXI4,
                // or a transfer-level port through `hns::xfer2axi`.
                if declared.backing == Backing::Dram {
                    return Err(TerminatorError::DramNeedsAxi4 {
                        bundle: binding.bundle.clone(),
                    });
                }
                continue;
            }
        };

        // For `dram` without `depth`, the default is everything the controller
        // can reach. The row and bank pins for the upper part of the chip never
        // toggle until that part is accessed, so a default that covers only
        // part of the chip could never test those pins. The reach is
        // `axi_addr_bits` in the target description, measured from the
        // generated controller IP, so it is not a guess.
        //
        // The cost: the register map then differs per board (256 MB on Arty,
        // 2 GB on VCU118). This is unavoidable when one manifest says "all of
        // it". Writing `depth` keeps the map the same on every board.
        let depth = match declared.depth {
            Some(depth) => depth,
            None if declared.backing == Backing::Dram => {
                let Some(reach) = controller_reach(target, data_bytes) else {
                    return Err(match target {
                        Some(target) => dram_not_on_target(&binding.bundle, target),
                        None => TerminatorError::DramDepthNeedsATarget {
                            bundle: binding.bundle.clone(),
                        },
                    });
                };
                // Do not shrink it for a narrow DUT interface. The arbiter and
                // the host side use the controller address width, and only the
                // DUT is widened by `hns::axi_aw`. The DUT still reaches only
                // the low part, but the host reaches the whole chip.
                reach
            }
            None => DEFAULT_AXI_MEM_DEPTH,
        };
        // A power-of-two word count only, so decoding is a mask and regions
        // pack without gaps.
        if depth < 2 || !depth.is_power_of_two() {
            return Err(TerminatorError::AxiMemDepthNotPowerOfTwo {
                bundle: binding.bundle.clone(),
                depth,
            });
        }

        // The bus must be a power-of-two multiple of 32 bits. The window is
        // 32 bits, so otherwise one word would span two bus words, and that
        // slicing is not implemented.
        if data_bytes < 4 || !data_bytes.is_power_of_two() {
            return Err(TerminatorError::AxiMemWidthNotAWordMultiple {
                bundle: binding.bundle.clone(),
                port: data_port,
                bits: data_bytes * 8,
            });
        }

        // The AXI4 type. A transfer-level port has none, so `hns::xfer2axi`
        // gets one just wide enough for the memory, at least 4 KB, the span
        // of a burst.
        let (pkg, addr_width, id_width) = match &axi {
            Some((_, axi4)) => (axi4.pkg(), axi4.addr_width(), axi4.id_width()),
            None => {
                let bytes = u64::from(depth) * u64::from(data_bytes);
                let bits = bytes.trailing_zeros().max(12);
                (
                    format!("$std::axi4_pkg::<{bits}, {data_bytes}, 1, 1, 1, 1, 1, 1>"),
                    bits,
                    1,
                )
            }
        };

        // `dram` requires `aperture`. It is known not to fit in the window, so
        // the default "whole memory" would silently build a 256 MB window, and
        // on PCIe also require a BAR that large.
        if declared.backing == Backing::Dram && declared.aperture.is_none() {
            return Err(TerminatorError::DramNeedsAperture {
                bundle: binding.bundle.clone(),
            });
        }
        let aperture_bytes = aperture_of(
            &binding.bundle,
            declared.aperture,
            depth as u64 * data_bytes as u64,
            4,
        )?;

        let dut_base = declared.dut_base.unwrap_or(0);
        if dut_base != 0 {
            const PAGE: u64 = 4096;
            if !dut_base.is_multiple_of(PAGE) {
                return Err(TerminatorError::DutBaseNot4k {
                    bundle: binding.bundle.clone(),
                    base: dut_base,
                    below: dut_base - dut_base % PAGE,
                    above: dut_base - dut_base % PAGE + PAGE,
                });
            }
            let end = u128::from(dut_base) + depth as u128 * data_bytes as u128;
            if end > 1u128 << addr_width.min(127) {
                return Err(TerminatorError::DutBasePastAddress {
                    bundle: binding.bundle.clone(),
                    port: port_name.clone(),
                    base: dut_base,
                    end,
                    bits: addr_width as usize,
                });
            }
        }

        plans.push(AxiMemPlan {
            bundle: binding.bundle.clone(),
            backing: declared.backing,
            port: port_name,
            pkg,
            addr_width,
            data_bytes,
            id_width,
            depth,
            depth_defaulted: declared.depth.is_none(),
            aperture_bytes,
            dut_base,
            xfer,
        });
    }

    Ok(plans)
}

/// Checks `aperture` and returns it.
///
/// It is the only way to reach a memory that does not fit in the window.
/// Without it, the whole memory is in the window.
fn aperture_of(
    bundle: &str,
    declared: Option<u32>,
    total_bytes: u64,
    entry_bytes: usize,
) -> Result<Option<usize>, TerminatorError> {
    let Some(aperture) = declared else {
        return Ok(None);
    };
    let aperture = aperture as u64;
    // Powers of two only, so decoding is a mask and regions pack without gaps.
    if !aperture.is_power_of_two() {
        return Err(TerminatorError::ApertureNotPowerOfTwo {
            bundle: bundle.to_string(),
            aperture,
        });
    }
    // It must not split an entry. A window that holds no word shows nothing.
    if aperture < entry_bytes as u64 {
        return Err(TerminatorError::ApertureSmallerThanEntry {
            bundle: bundle.to_string(),
            aperture,
            entry_bytes,
        });
    }
    // A window as large as the memory means nothing. The user meant a moving
    // window, so do not silently map the whole memory.
    if aperture >= total_bytes {
        return Err(TerminatorError::ApertureNotSmaller {
            bundle: bundle.to_string(),
            aperture,
            total_bytes,
        });
    }
    Ok(Some(aperture as usize))
}

/// Checks that each bundle is taken by exactly one terminator, and that no key
/// is without effect.
///
/// Each `resolve_*` skips shapes that are not its own, so a bundle that matches
/// none would pass unnoticed, and the DUT ports would connect to undeclared
/// wires (failing only in `veryl build`). The keys each shape uses are checked
/// here too: a key that is written but has no effect makes the manifest lie
/// about the hardware.
///
/// `latency` is not checked here. `contract::resolve` reports a mismatch with
/// the contract, and each `resolve_*` reports a missing one. Backings without a
/// terminator yet (`observe` and others) are rejected by `gen`.
#[allow(clippy::too_many_arguments)]
pub fn check_claimed(
    manifest: &Manifest,
    bindings: &[Binding],
    fifos: &[FifoPlan],
    traces: &[TracePlan],
    memories: &[MemPlan],
    host_mems: &[HostMemPlan],
    slaves: &[SlavePlan],
    apbs: &[ApbPlan],
    axi_mems: &[AxiMemPlan],
    axi_slaves: &[AxiSlavePlan],
    irqs: &[IrqPlan],
    flrs: &[FlrPlan],
) -> Result<(), TerminatorError> {
    const DEPTH: &str = "depth";
    const ADDRESSING: &str = "addressing";
    const ACCESS: &str = "access";
    const APERTURE: &str = "aperture";
    const DUT_BASE: &str = "dut_base";
    const RESET: &str = "reset";
    const FORMAT: &str = "format";

    for binding in bindings {
        let name = binding.bundle.as_str();
        let declared = &manifest.bundle[name];
        let claims = [
            (
                fifos.iter().any(|p| p.bundle == name),
                "a `host_poll_fifo`",
                &[DEPTH][..],
            ),
            (
                traces.iter().any(|p| p.bundle == name),
                "a `trace`",
                &[DEPTH, FORMAT][..],
            ),
            (
                memories.iter().any(|p| p.bundle == name),
                "a memory port with a fixed latency",
                &[DEPTH, ADDRESSING, ACCESS, APERTURE, DUT_BASE][..],
            ),
            (
                host_mems.iter().any(|p| p.bundle == name),
                "a transfer-level memory port",
                &[DEPTH][..],
            ),
            (
                slaves.iter().any(|p| p.bundle == name),
                "a `slave`",
                &[][..],
            ),
            (
                apbs.iter().any(|p| p.bundle == name && p.debug_module),
                "a `dmi`",
                &[RESET][..],
            ),
            (
                apbs.iter().any(|p| p.bundle == name && !p.debug_module),
                "an APB `slave`",
                &[][..],
            ),
            (
                axi_mems
                    .iter()
                    .any(|p| p.bundle == name && p.xfer.is_none()),
                "an AXI4 memory port",
                &[DEPTH, APERTURE, DUT_BASE][..],
            ),
            (
                axi_mems
                    .iter()
                    .any(|p| p.bundle == name && p.xfer.is_some()),
                "a transfer-level `dram` port",
                &[DEPTH, APERTURE][..],
            ),
            (
                axi_slaves.iter().any(|p| p.bundle == name),
                "an AXI4 `slave`",
                &[APERTURE][..],
            ),
            (
                irqs.iter().any(|p| p.bundle == name),
                "a `host_irq`",
                &[][..],
            ),
            (
                flrs.iter().any(|p| p.bundle == name),
                "a `pcie_flr`",
                &[][..],
            ),
        ];
        let mut claimed = claims.iter().filter(|(hit, _, _)| *hit);
        let (shape, takes) = match (claimed.next(), claimed.next()) {
            (Some((_, shape, takes)), None) => (*shape, *takes),
            (Some(_), Some(_)) => {
                unreachable!("[bundle.{name}] was claimed by two terminators")
            }
            (None, _) => match declared.backing {
                Backing::Reg => ("a `reg` bundle", &[][..]),
                // No terminator yet. `gen` rejects it (`plan::generatable`).
                Backing::HostMem | Backing::Observe => continue,
                backing => {
                    return Err(TerminatorError::NotTerminated {
                        bundle: name.to_string(),
                        backing: backing.as_str().to_string(),
                    });
                }
            },
        };

        for (key, set) in [
            (DEPTH, declared.depth.is_some()),
            (ADDRESSING, declared.addressing.is_some()),
            (ACCESS, declared.access.is_some()),
            (APERTURE, declared.aperture.is_some()),
            (DUT_BASE, declared.dut_base.is_some()),
            (RESET, declared.reset.is_some()),
            (FORMAT, declared.format.is_some()),
        ] {
            if set && !takes.contains(&key) {
                return Err(TerminatorError::KeyNotApplicable {
                    bundle: name.to_string(),
                    key,
                    shape,
                    takes: if takes.is_empty() {
                        "none of `depth`, `addressing`, `access`, `aperture`, `dut_base`, `reset`, `format`"
                            .to_string()
                    } else {
                        takes
                            .iter()
                            .map(|k| format!("`{k}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                });
            }
        }
    }
    Ok(())
}

fn port_of<'a>(dut: &'a Dut, name: &str) -> &'a Port {
    dut.ports
        .iter()
        .find(|port| port.name == name)
        .expect("bundle ports come from the DUT")
}

/// A handshake is 1 bit. Any other width means something else is connected.
fn expect_single_bit(bundle: &str, port: &Port, role: &'static str) -> Result<(), TerminatorError> {
    match port.width() {
        Some(1) => Ok(()),
        // An unresolved width is not assumed to be 1 bit.
        width => Err(TerminatorError::WideHandshake {
            bundle: bundle.to_string(),
            port: port.name.clone(),
            role,
            width: match width {
                Some(width) => format!("{width} bits wide"),
                None => "of a width that could not be resolved".to_string(),
            },
        }),
    }
}

/// The interface shape. It only selects the text of a direction error.
#[derive(Clone, Copy)]
enum Shape {
    Fifo,
    Trace,
    Memory,
    TransferLevel,
    Slave,
    Dmi,
    ApbSlave,
}

impl Shape {
    /// (shape name, direction rule, fix).
    fn text(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Shape::Fifo => (
                "a `host_poll_fifo`",
                "In a host_poll_fifo, `valid` and `data` are DUT outputs and `ready` is a DUT input.",
                "If the host feeds the DUT, use `reg` instead. Each host write then sends one beat.",
            ),
            Shape::Trace => (
                "a `trace`",
                "In a trace, `valid` and every field it records are DUT outputs.",
                "A trace only records what the DUT drives. Move the input to another bundle.",
            ),
            Shape::Memory => (
                "a memory port",
                "In a memory port, the DUT drives `addr`, `wdata`, `wstrb`, `we` and `re`, and the memory answers on `rdata`.",
                "If the host drives the address and the DUT answers, use `backing = \"slave\"`.",
            ),
            Shape::TransferLevel => (
                "a transfer-level memory port",
                "In a transfer-level port, the DUT drives the commands (`*_cmd_valid`, `*_cmd_addr`, `*_cmd_size`), the write data (`wr_valid`, `wr_data`, `wr_strb`, `wr_last`) and `rd_ready`. The memory drives `*_cmd_ready`, the read data (`rd_valid`, `rd_data`, `rd_last`), `wr_ready` and `wr_done_valid`.",
                "Check the role of the port in `ports`. The usual cause is a read and a write role swapped.",
            ),
            Shape::Dmi => (
                "a `dmi`",
                "In a `dmi` bundle the harness is the APB master: it drives `psel`, `penable`, `pwrite`, `paddr` and `pwdata`, and the Debug Module answers on `prdata` and `pready`.",
                "Check the role of the port in `ports`, or use the Debug Module's own APB ports.",
            ),
            Shape::ApbSlave => (
                "an APB `slave`",
                "In an APB slave the harness is the APB master: it drives `psel`, `penable`, `pwrite`, `paddr` and `pwdata`, and the DUT answers on `prdata`, `pready` and `pslverr`.",
                "Check the role of the port in `ports`.",
            ),
            Shape::Slave => (
                "a `slave`",
                "In a slave, the harness drives `addr`, `wdata` and `we`, and the DUT answers on `rdata`.",
                "If the DUT drives the address, use `backing = \"bram\"`.",
            ),
        }
    }
}

fn expect_direction(
    shape: Shape,
    bundle: &str,
    port: &Port,
    role: &'static str,
    want: PortDirection,
) -> Result<(), TerminatorError> {
    if port.direction == want {
        return Ok(());
    }
    let (shape, rule, fix) = shape.text();
    Err(TerminatorError::WrongDirection {
        bundle: bundle.to_string(),
        port: port.name.clone(),
        role,
        direction: port.direction.as_str(),
        shape,
        rule,
        fix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A named board without DRAM says which of its configs adds it, and which
    /// memory part each one is.
    #[test]
    fn a_board_without_dram_points_at_its_configs() {
        let kcu105 = hns_targets::resolve("xilinx/kcu105", &[]).unwrap();
        let err = dram_not_on_target("mem", &kcu105);
        let help = miette::Diagnostic::help(&err).unwrap().to_string();
        for want in [
            "--target xilinx/kcu105:dr",
            "EDY4016AABG-DR-F",
            "--target xilinx/kcu105:062",
            "MT40A256M16LY-062E",
        ] {
            assert!(help.contains(want), "{help}");
        }

        // No config to offer: the way out is a BRAM.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.toml");
        std::fs::write(
            &path,
            "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"artix7\"\npart = \"xc7a35t\"\n",
        )
        .unwrap();
        let target = hns_targets::load_file(&path, &[]).unwrap();
        let help = miette::Diagnostic::help(&dram_not_on_target("mem", &target))
            .unwrap()
            .to_string();
        assert!(help.contains("backing = \"bram\""), "{help}");
    }
}

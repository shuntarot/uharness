//! A CLI that reaches the generated window over JTAG or PCIe.
//!
//! It has no DUT-specific code. Register names and widths all come from
//! `regs.json`.
//!
//! The probe serial is always given at run time. It names one physical probe,
//! so a target description cannot hold it.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;
use hns_host::{Batch, Bridge, Ftdi, Mpsse, ProbeConfig, Reads};
use hns_regs::{Access, RegisterMap};

#[derive(Parser)]
#[command(
    name = "hio",
    version = env!("HNS_VERSION"),
    about = "Poke a veryl-harness window over JTAG or PCIe",
    long_about = None,
    disable_help_flag = true,
    after_help = "See 'hio help <command>' for more on a command, and 'hio help options' for every option."
)]
struct Cli {
    /// The generated regs.json.
    ///
    /// Names, widths and the window size come from it. Without it: ./regs.json if
    /// there is one, else hns/regs.json, else the only hns/*/regs.json.
    ///
    /// Also read from HIO_REGS, so a cargo runner can point at it from
    /// `[env]` in .cargo/config.toml.
    #[arg(long, value_name = "PATH", env = "HIO_REGS")]
    regs: Option<String>,

    /// Board target, e.g. digilent/arty-a7-35.
    ///
    /// Supplies the probe details that cannot be discovered over USB. Without it, the
    /// one recorded in regs.json is used. The start of the name is enough while only
    /// one board starts that way.
    #[arg(long, value_name = "NAME")]
    target: Option<String>,

    /// A target description of your own, instead of a shipped one.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "target",
        hide_short_help = true
    )]
    target_file: Option<PathBuf>,

    /// Layer a TOML patch over the target description.
    ///
    /// Repeatable; applied in order.
    #[arg(long, value_name = "PATH", hide_short_help = true)]
    target_patch: Vec<PathBuf>,

    /// USB vendor id of the probe.
    ///
    /// Defaults to FTDI (0x0403).
    #[arg(long, value_name = "ID", value_parser = parse_u32, hide_short_help = true)]
    vid: Option<u32>,

    /// USB product id.
    ///
    /// Without it, the MPSSE-capable FTDI chips are searched and the one device found
    /// is used; if several match, they are listed.
    #[arg(long, value_name = "ID", value_parser = parse_u32, hide_short_help = true)]
    pid: Option<u32>,

    /// USB interface carrying MPSSE.
    ///
    /// On an FT2232H channel A is 0; channel B is the UART.
    #[arg(long, value_name = "N", hide_short_help = true)]
    interface: Option<u8>,

    /// Match the iProduct string.
    ///
    /// Digilent uses a different one per product: an on-board Arty module reports "Digilent
    /// USB Device", a stand-alone HS1/HS2 reports "Digilent Adept USB Device".
    #[arg(long, value_name = "STRING", hide_short_help = true)]
    product: Option<String>,

    /// Match the serial number, or a part of it that only one probe has.
    ///
    /// Required when more than one probe of the same type is plugged in.
    /// `--serial 8107` picks `210319788107` when no other serial contains
    /// `8107`.
    #[arg(long, value_name = "STRING")]
    serial: Option<String>,

    /// Level-shifter enable as data:direction, e.g. 0x0088:0x008b.
    ///
    /// Without it, a Digilent product string selects the known Digilent value; anything
    /// else drives only the four JTAG pins.
    #[arg(long, value_name = "DATA:DIR", value_parser = parse_pair, hide_short_help = true)]
    layout_init: Option<(u16, u16)>,

    /// Ceiling for TCK in Hz.
    ///
    /// The divisor is rounded so the clock never exceeds it. The default is slow;
    /// raise it once the link is known to work.
    #[arg(long, value_name = "HZ", value_parser = parse_rate_u32, hide_short_help = true)]
    tck_hz: Option<u32>,

    /// Ceiling for TCK in MHz.
    ///
    /// Fractions are fine (0.5 is 500 kHz). The same as --tck-hz, in MHz.
    #[arg(long, value_name = "MHZ", conflicts_with = "tck_hz")]
    tck_mhz: Option<f64>,

    /// TCK cycles to wait between one scan's Update and the next Capture.
    ///
    /// Worked out from `window_clock_mhz` and `window_cycles` in regs.json. This
    /// overrides it, for measuring.
    #[arg(long, value_name = "CYCLES", hide = true)]
    update_gap: Option<usize>,

    /// Instruction register length of the TAP.
    ///
    /// 6 on 7-series. Whatever is used is checked against the IR capture pattern.
    #[arg(long, value_name = "BITS", hide_short_help = true)]
    ir_length: Option<u8>,

    /// IR value selecting the USER slot the bridge sits in.
    ///
    /// 0x02 is USER1 on 7-series.
    #[arg(long, value_name = "IR", value_parser = parse_u32, hide_short_help = true)]
    user1_ir: Option<u32>,

    /// Reach the window over `jtag` (default) or `pcie`.
    ///
    /// A PCIe design carries both, and JTAG is the one that still answers when PCIe does
    /// not -- so it stays the default. `pcie` needs root and a regs.json that names the card.
    #[arg(short = 't', long, value_name = "NAME")]
    transport: Option<String>,

    /// Short for `--transport pcie`.
    #[arg(short = 'p', conflicts_with = "transport")]
    pcie: bool,

    /// Reach the window through a server, e.g. localhost:5555.
    ///
    /// The server is `hio serve` on the machine the board is on, or `veryl harness sim
    /// --addr`. Use the same regs.json as the server. `program` sends the bitstream to the
    /// server; the other commands that need the cable or the card's memory (probe, DMA)
    /// run only on the server's machine.
    #[arg(
        long,
        value_name = "HOST:PORT",
        env = "HIO_CONNECT",
        conflicts_with_all = ["transport", "pcie"],
        hide_short_help = true
    )]
    connect: Option<String>,

    /// PCI address of the card: the first column of `lspci`.
    ///
    /// The domain (`0000:`) may be left out. Without this option the ID in regs.json
    /// picks the card; it is only needed when more than one answers to it.
    #[arg(long, value_name = "BDF", hide_short_help = true)]
    bdf: Option<String>,

    /// Do not check the magic and map hash before acting, or after `program`.
    ///
    /// Only useful when the window is known not to answer yet, or to program
    /// a bitstream that is not a harness: `program` then stops after DONE.
    #[arg(long, hide_short_help = true)]
    no_verify: bool,

    // `run` answers --list before parsing; the field is here for the help.
    /// List all commands.
    #[arg(long)]
    #[allow(dead_code)]
    list: bool,

    /// Print help.
    #[arg(short, long, action = clap::ArgAction::HelpShort)]
    help: Option<bool>,

    /// Without a command, `hio` opens a prompt that keeps the link open.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a file of commands, holding the link open across them.
    ///
    /// One command per line, written exactly as on the command line. `#` starts a
    /// comment. Two extra commands exist only here: `echo <text>` and `sleep <ms>`.
    Run {
        /// The file, or `-` for standard input.
        file: String,
    },

    /// List the board targets that ship with this build.
    Targets,

    /// List the FTDI probes that are plugged in.
    Probe,

    /// Read the FPGA's temperature and supply voltages over JTAG.
    ///
    /// It uses the system monitor's own JTAG instruction (`sysmon_ir` in the
    /// target's [jtag]), so it works with no bitstream loaded, and while any
    /// design runs. Max and min are kept by the monitor since power-up or the last
    /// erase or configuration, which restart it.
    Sysmon,

    /// Read and write memory through the ARM DAP on the JTAG chain.
    ///
    /// A Zynq-7000 has one. It reaches the PS's memory and registers with no
    /// program on the CPUs. `dap` in the target's [jtag] says where it is.
    #[command(subcommand)]
    Dap(DapCmd),

    /// Write a .bit to the device over JTAG, without Vivado.
    Program {
        /// The .bit that Vivado wrote, or a .svf.
        ///
        /// A raw .bin has no header and is refused, because it carries no device name to
        /// check against the board.
        ///
        /// A .svf is replayed as it is, so it also works on a device this build
        /// does not know. `make svf` writes one next to the .bit.
        ///
        /// Left out, the one beside the register map is used: `syn/output/*.bit`
        /// under the directory regs.json was found in. That is where `gen` puts it,
        /// and the map is what says the bitstream is the right one anyway.
        file: Option<PathBuf>,
        /// Program even though the bitstream names a different device.
        ///
        /// Configuration is volatile, so the worst case is a board that does not come up.
        #[arg(long)]
        allow_any_device: bool,
        /// How long to wait for the configuration memory to clear, in milliseconds.
        ///
        /// Clearing takes longer on bigger devices, and waiting too little makes the data
        /// that follows be ignored -- with no error, just a board that stays dark.
        #[arg(long, value_name = "MS", default_value_t = 200)]
        erase_wait_ms: u64,
    },

    /// Clear the configuration memory and stop there.
    ///
    /// On a board that was running, the heartbeat UART stops. Configuration is
    /// volatile, so a power cycle brings the board back.
    Erase,

    /// Check the board is running this register map.
    ///
    /// Reads the magic and the map hash and compares them with regs.json.
    Id,

    /// Read a register, or a range of a region.
    ///
    /// `read <register>` or `read <region> <offset> [words]`. A byte offset like
    /// 0x08 works in place of a register name.
    Read {
        /// Register name, region name, or a byte offset like 0x08.
        who: String,
        /// Byte offset inside the region. Regions only.
        #[arg(value_parser = parse_u64)]
        at: Option<u64>,
        /// How many words to read. Regions only; one by default.
        #[arg(value_parser = parse_usize)]
        words: Option<usize>,
    },

    /// Write a register, or a range of a region.
    ///
    /// `write <register> <word>...` or `write <region> <offset> <word>...`.
    Write {
        /// Register name, region name, or a byte offset like 0x08.
        who: String,
        /// Words to write, lowest first. For a region, the offset comes first.
        ///
        /// Decimal, or 0x-prefixed hex. A register wider than one word takes one value per
        /// word; a single value is also accepted for registers of at most two words.
        #[arg(required = true, value_parser = parse_u64)]
        values: Vec<u64>,
    },

    /// Read every register in the map and print them.
    Dump,

    /// Reset the DUT, or hold it in reset.
    ///
    /// Only the DUT is reset. Memory contents, `reg` registers and the harness keep
    /// their state. With --hold the DUT stays in reset, so memory can be loaded
    /// before it starts: `hio reset --hold`, `hio load ...`, `hio reset --release`.
    Reset {
        /// Leave the DUT in reset.
        #[arg(long, conflicts_with = "release")]
        hold: bool,
        /// Take the DUT out of reset.
        #[arg(long)]
        release: bool,
    },

    /// Check that the board and this machine are ready, and say what to fix.
    ///
    /// Over JTAG: the window and the memory. Over PCIe also the card, its BAR,
    /// the link, and -- for a design with a requester -- bus mastering, the
    /// IOMMU, huge pages and the engine. Nothing is written.
    Check,

    /// Measure how fast the window can be read.
    ///
    /// This measures the transport, not the DUT.
    Bench {
        /// Region to sweep. Without it, `harness_magic` is read over and over.
        bundle: Option<String>,
        /// Reads per batch. Only when no region is named.
        ///
        /// One batch is one USB round trip until it exceeds the device buffer, at
        /// which point it is split.
        #[arg(long, value_name = "N", default_value_t = 1000)]
        reads: usize,
        /// How many batches to run.
        #[arg(long, value_name = "R", default_value_t = 10)]
        repeat: usize,
        /// Write as well as read. **This destroys what is in the region.**
        #[arg(long)]
        both: bool,
        /// Byte offset inside the region.
        #[arg(long, value_name = "BYTES", value_parser = parse_u64)]
        base: Option<u64>,
        /// How many bytes to sweep. The whole region by default.
        #[arg(long, value_name = "BYTES", value_parser = parse_u64)]
        size: Option<u64>,
        /// Read with this many threads. PCIe only, and only reads.
        ///
        /// A read over PCIe stalls the core for the whole round trip while the
        /// link sits idle, so other cores can overlap their own. Writes are
        /// posted and already fast.
        #[arg(long, value_name = "N", default_value_t = 1)]
        threads: usize,
    },

    /// Load a file into a memory bundle, one entry at a time.
    Load {
        /// Bundle name, as it appears in Harness.toml.
        bundle: String,
        /// The file to load.
        ///
        /// A .elf is placed by its program headers; anything else is taken as raw
        /// bytes and needs --at to say where it goes.
        file: PathBuf,
        /// Entry index to start at, for a raw file.
        #[arg(long, value_name = "N", default_value_t = 0)]
        at: usize,
        /// Address that entry 0 of this memory has in the DUT's address space.
        ///
        /// An ELF segment at address A goes to entry (A - base) / entry-bytes, so this is
        /// what places the program. The default is the bundle's `dut_base` from
        /// regs.json; give it here when the DUT moves addresses itself. Guessing it
        /// wrong puts the program somewhere else, so there is no other default; the
        /// addresses it worked out are printed, to check against the linker script.
        #[arg(long, value_name = "ADDR", value_parser = parse_u64)]
        base: Option<u64>,
        /// Read it back and compare.
        ///
        /// Costs a second pass over the file.
        #[arg(long)]
        verify: bool,
        /// Always go through the window, however much there is.
        ///
        /// For telling the two paths apart while debugging; the output says which
        /// one it used either way.
        #[arg(long, conflicts_with = "dma")]
        pio: bool,
        /// Always have the card move the data, however little there is.
        ///
        /// Refuses rather than falling back, so a machine that cannot do it says
        /// why (`check` reports the same things).
        #[arg(long)]
        dma: bool,
    },

    /// Run firmware on a CPU in the DUT and talk to it.
    #[command(subcommand)]
    Cpu(CpuCmd),

    /// Write a pattern to a memory bundle and read it back.
    ///
    /// This destroys what is in the memory.
    Memtest {
        /// Bundle name, as it appears in Harness.toml.
        bundle: String,
        /// Read back through the window with this many threads. PCIe only.
        ///
        /// Defaults to 4 over PCIe and 1 over JTAG. Over PCIe a read stalls one
        /// core for the whole round trip while the link sits idle, so other cores
        /// can overlap theirs; it stops helping past 4. JTAG walks a single cable.
        /// Asking for threads reads back through the window, since that is the
        /// only path they apply to.
        #[arg(long, value_name = "N")]
        threads: Option<usize>,
        /// Byte offset to start at. Has to land on an entry.
        #[arg(long, value_name = "BYTES", default_value_t = 0, value_parser = parse_u64)]
        base: u64,
        /// How many bytes to test. The whole memory by default.
        #[arg(long, value_name = "BYTES", value_parser = parse_u64)]
        size: Option<u64>,
        /// Vary the pattern. The same seed writes the same values.
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = parse_u32)]
        seed: u32,
        /// Always go through the window, however much there is.
        ///
        /// For telling the two paths apart while debugging; the output says which
        /// one it used either way.
        #[arg(long, conflicts_with = "dma")]
        pio: bool,
        /// Always have the card move the data, however little there is.
        ///
        /// Refuses rather than falling back, so a machine that cannot do it says
        /// why (`check` reports the same things).
        #[arg(long)]
        dma: bool,
    },

    /// Fire one descriptor: the card moves data itself.
    ///
    /// This is the harness's own requester, not the DUT's. By default the card
    /// reads the memory the DUT shares and writes it into a huge page on this
    /// machine; --to-card goes the other way. Either way a pattern is put on
    /// the sending side and poison on the receiving side, so what lands can be
    /// checked against what was meant.
    ///
    /// Fire the first one with the IOMMU still translating (--expect-fault): a
    /// wrong address is refused by the IOMMU and dmesg records the address the
    /// card actually put on the bus, which is what proves the TLP header
    /// without risking memory. Pass the card through only once that matches.
    DmaFire {
        /// Memory bundle to read from. The only one by default.
        bundle: Option<String>,
        /// Byte offset inside that memory.
        #[arg(long, value_name = "BYTES", default_value_t = 0, value_parser = parse_u64)]
        at: u64,
        /// How many bytes to move.
        #[arg(long, value_name = "BYTES", default_value_t = 4, value_parser = parse_u64)]
        len: u64,
        /// Vary the pattern written into the memory first.
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = parse_u32)]
        seed: u32,
        /// Fire with the IOMMU still translating, expecting the access to fault.
        #[arg(long)]
        expect_fault: bool,
        /// Split the payload no larger than this, for measuring.
        ///
        /// The link decides the real limit and the card cannot exceed it; this
        /// only asks for less. 128, 256, 512, 1024, 2048 or 4096.
        #[arg(long, value_name = "BYTES", value_parser = parse_u32)]
        mps: Option<u32>,
        /// Go the other way: the card reads host memory and writes the bundle.
        ///
        /// The pattern is put in the host's page first and the bundle is
        /// poisoned, so what is read back through the window says whether the
        /// completions came home.
        #[arg(long)]
        to_card: bool,
        /// Fire at this bus address instead of a page hio hands out.
        ///
        /// For example another card's BAR, for a peer-to-peer test. hio cannot
        /// read that address, so what lands is not checked; with --to-card the
        /// memory is read back and shown. An address in RAM is refused.
        #[arg(long, value_name = "ADDR", value_parser = parse_u64, conflicts_with = "expect_fault")]
        pcie_addr: Option<u64>,
    },

    /// Empty a host_poll_fifo bundle, oldest entry first.
    Drain {
        /// Bundle name, as it appears in Harness.toml.
        bundle: String,
        /// Stop after this many entries.
        #[arg(long, value_name = "N")]
        max: Option<usize>,
    },

    /// Start, stop and read a trace ring (`backing = "trace"`).
    #[command(subcommand)]
    Trace(TraceCmd),

    /// Serve the window to `hio --connect` on other machines.
    ///
    /// Opens the window as any command does (JTAG, or PCIe with `-p`) and keeps it
    /// open. One client at a time. There is no password: keep the default address
    /// and reach it with `ssh -L`. Ctrl-C stops it.
    Serve {
        /// Where to listen. Port 0 picks a free one.
        #[arg(long, value_name = "IP:PORT", default_value = "127.0.0.1:0")]
        listen: String,
    },

    /// Print a shell completion script on standard output.
    ///
    /// Install it where the shell looks, then start a new shell; the manual has the
    /// paths. Register and bundle names are not completed -- they live in regs.json.
    Completions {
        /// bash, zsh, fish, elvish or powershell.
        shell: Shell,
    },
}

/// `hio dap`: the ARM DAP on the JTAG chain.
#[derive(Subcommand)]
enum DapCmd {
    /// Show the DAP's IDCODE and every access port it has.
    Info,
    /// Read 32-bit words through a memory access port.
    Read {
        /// The address, in the system's address map (not a bundle offset).
        #[arg(value_parser = parse_u32)]
        addr: u32,
        /// How many words.
        #[arg(default_value_t = 1)]
        words: u32,
        #[command(flatten)]
        ap: ApArg,
    },
    /// Write 32-bit words through a memory access port, one after another.
    Write {
        #[arg(value_parser = parse_u32)]
        addr: u32,
        #[arg(value_parser = parse_u32, required = true)]
        values: Vec<u32>,
        #[command(flatten)]
        ap: ApArg,
    },
    /// Set up a Zynq-7000 PS from the `ps7_init.tcl` Vivado wrote.
    ///
    /// Runs its MIO, PLL, clock, DDR and peripheral steps for the silicon
    /// version it finds, then the post-configuration step, which turns on the
    /// level shifters between the PS and the PL. Run it after programming
    /// the PL, once per power-up.
    Ps7Init {
        /// The `ps7_init.tcl` from the PS7 IP's output products.
        file: PathBuf,
        /// Leave out the post-configuration step (level shifters, PL reset).
        #[arg(long)]
        no_post: bool,
        #[command(flatten)]
        ap: ApArg,
    },
}

/// The DAP and the access port the `ps7` steps run through.
struct DapMem<'a, T: hns_host::adi::Tap> {
    dap: &'a mut hns_host::adi::Dap<T>,
    ap: u8,
}

impl<T: hns_host::adi::Tap> hns_host::ps7::Mem for DapMem<'_, T>
where
    T::Error: std::fmt::Debug,
{
    type Error = String;
    fn read32(&mut self, addr: u32) -> Result<u32, String> {
        self.dap.read32(self.ap, addr).map_err(|e| e.to_string())
    }
    fn write32(&mut self, addr: u32, value: u32) -> Result<(), String> {
        self.dap
            .write32(self.ap, addr, value)
            .map_err(|e| e.to_string())
    }
}

#[derive(clap::Args, Debug, Clone)]
struct ApArg {
    /// The access port to use.
    ///
    /// Without it, `mem_ap` in the target's `dap`. `hio dap info` lists them.
    #[arg(long)]
    ap: Option<u8>,
    /// The CSW the access port uses, instead of the one it holds.
    ///
    /// Its protection bits decide what kind of master the access looks like
    /// (privileged, secure, debug). The size is always set to 32 bits.
    #[arg(long, value_parser = parse_u32)]
    csw: Option<u32>,
}

/// `hio trace`: the ring of a `trace` bundle. Each command takes the bundle
/// name, which may be left out when the design has one trace.
#[derive(Subcommand)]
enum TraceCmd {
    /// Empty the ring and record from now on.
    ///
    /// Without a trigger it records until `stop`. With one, it keeps
    /// `--after` records past the trigger and then freezes.
    Start {
        bundle: Option<String>,
        /// Trigger when a field has this value, such as `pc_rdata=0x80001234`.
        ///
        /// `field=value/mask` tests some bits, and `field!=0` that it is not
        /// 0. Repeat it for an AND. Only the ports in `trigger` in
        /// Harness.toml can trigger.
        #[arg(long, value_name = "FIELD=VALUE[/MASK]")]
        when: Vec<String>,
        /// Trigger on the first beat, on no condition.
        #[arg(long, conflicts_with = "when")]
        now: bool,
        /// Records to keep after the trigger, then freeze (default 0).
        #[arg(long, value_name = "N")]
        after: Option<u32>,
    },
    /// Stop recording. The ring keeps the last records.
    Stop { bundle: Option<String> },
    /// Say whether it records, and how many records the ring holds.
    Status { bundle: Option<String> },
    /// Print the records, oldest first.
    ///
    /// Stop the trace first, or halt the CPU: records that arrive while the
    /// ring is read are mixed in, and a warning says so.
    Dump {
        bundle: Option<String>,
        /// Only the newest N records.
        #[arg(long, value_name = "N")]
        last: Option<usize>,
    },
}

/// `hio cpu`: what needs a CPU in the DUT and an ELF for it.
#[derive(Subcommand)]
enum CpuCmd {
    /// Load an ELF, run it, and connect to its RTT console.
    ///
    /// Made to be a cargo runner (`runner = "hio cpu boot"`). It holds the DUT in
    /// reset, places each segment in every memory that covers its address
    /// (`dut_base` in regs.json), clears the RTT control block, and releases
    /// the DUT. With `_SEGGER_RTT` it then shows up channel 0 and sends keys to
    /// down channel 0; Ctrl-A x leaves. The exit status is the program's, from
    /// `tohost` or from an up channel named `hio-exit`.
    ///
    /// Ctrl-A c switches to the prompt and back. It opens at the CPU
    /// commands (`halt`, `step 3`, `regs`); `hio` reaches the others. The
    /// target's output waits until you go back.
    Boot {
        /// The ELF to run.
        elf: PathBuf,
        /// Stop at the first instruction, at the prompt, before the program
        /// runs.
        ///
        /// Set breakpoints there, then `resume`. It needs the Debug Module on
        /// a reset of its own (`reset` in the `dmi` bundle), so that it is up
        /// while the DUT is held in reset.
        #[arg(long)]
        halt: bool,
        /// Stop on a fault the program did not mean to raise.
        ///
        /// Like `cpu catch`, set at the first instruction: the triggers live
        /// in the core and a reset clears them. Needs what `--halt` needs.
        #[arg(long)]
        catch: bool,
        #[command(flatten)]
        attach: AttachArgs,
        /// Arguments after the ELF, which are ignored.
        ///
        /// cargo passes the program's arguments here; the firmware has no way
        /// to receive them.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },

    /// Connect to the RTT console of a program that is already running.
    ///
    /// Like `cpu boot`, but it loads nothing and does not touch the reset.
    Console {
        /// The ELF the program was built from. Only its symbols are used.
        elf: PathBuf,
        #[command(flatten)]
        attach: AttachArgs,
    },

    /// Describe the Debug Module and the hart.
    ///
    /// Reads the Debug Module's own registers and leaves the hart as it is.
    /// A halted hart also shows its ISA and its triggers.
    Info {
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Read a DMI register, or write one.
    ///
    /// The register is a name (`dmstatus`) or a number (`0x11`). This goes
    /// past the debugger: a read or a write can change what the Debug Module
    /// does next.
    Dmi {
        /// Register name or number.
        reg: String,
        /// The value to write. Without it, the register is read.
        #[arg(value_parser = parse_u32)]
        value: Option<u32>,
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Halt the hart and show where it stopped.
    Halt {
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Let a halted hart run.
    Resume {
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Measure the IPC of the program that runs on the hart.
    ///
    /// It halts the hart, reads `mcycle` and `minstret`, lets it run for the
    /// given time, halts it and reads them again, then leaves the hart as it
    /// was. The counters are never written; if `mcountinhibit` stops them, it
    /// is lifted for the run and put back.
    Ipc {
        /// How long to let it run, in seconds.
        #[arg(long, value_name = "SECONDS", default_value_t = 1.0)]
        time: f64,
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Run one instruction, or `count`, and show the PC after each.
    Step {
        /// How many instructions.
        #[arg(default_value_t = 1)]
        count: u32,
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Show the registers of a halted hart, or read or write one.
    ///
    /// A name is `pc`, a GPR (`a0`, `x10`) or a CSR (`mstatus`, `0x300`).
    Regs {
        /// One register.
        name: Option<String>,
        /// The value to write to it.
        #[arg(value_parser = parse_u64)]
        value: Option<u64>,
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Read memory at a DUT address, or write it.
    ///
    /// The place is an address or a symbol (with the ELF). It goes through
    /// the window to the memories `dut_base` places there, not through the
    /// CPU, so the hart need not halt; a CPU with a data cache may hold newer
    /// data. Without a count a symbol is read whole, an address as one word.
    Mem {
        /// Address or symbol.
        place: String,
        /// How many 32-bit words.
        words: Option<u32>,
        /// Words to write there instead, in order.
        #[arg(long, num_args = 1.., value_name = "WORD", value_parser = parse_u32)]
        write: Vec<u32>,
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Show the instructions at a place, or around the halted pc.
    ///
    /// The place is an address or a symbol (with the ELF). Without one, the
    /// code around the pc is shown, and the pc's line is marked `=>`. The
    /// instructions are read through the window from the memory the DUT
    /// fetches from, so the hart need not halt. A line notes where the ELF
    /// holds other bytes.
    Disasm {
        /// Address or symbol.
        place: Option<String>,
        /// How many instructions. Default: a whole symbol (up to 64), else 12.
        count: Option<u32>,
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Set a hardware breakpoint, list them, or clear them.
    ///
    /// The place is an address or a symbol (with the ELF). Without one, the
    /// breakpoints are listed; with `--clear` and no place, all go. A running
    /// hart is halted for a moment to reach the triggers.
    Break {
        /// Address or symbol.
        place: Option<String>,
        /// Clear the breakpoint at the place, or all of them.
        #[arg(long)]
        clear: bool,
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Stop when the program raises a fault, or stop doing so.
    ///
    /// The faults are access faults, misaligned accesses, illegal
    /// instructions and page faults; not `ecall` or `ebreak`, which programs
    /// raise on purpose. The hart stops at the trap handler, and the prompt
    /// says where the fault was. A reset clears it.
    Catch {
        /// Stop catching.
        #[arg(long)]
        clear: bool,
        #[command(flatten)]
        hart: HartArgs,
    },

    /// Reset the hart through the Debug Module (`ndmreset`).
    ///
    /// The Debug Module stays up, and so do the harness memories. What else
    /// `ndmreset` resets is up to the design.
    Reset {
        /// Stop before the first instruction.
        #[arg(long)]
        halt: bool,
        #[command(flatten)]
        hart: HartArgs,
    },
}

/// Which Debug Module and which hart a `cpu` debug command means.
#[derive(clap::Args, Debug, Clone)]
struct HartArgs {
    /// The hart to select.
    #[arg(long, value_name = "N", default_value_t = 0)]
    hart: u32,
    /// The `dmi` bundle, when the design has more than one.
    #[arg(long, value_name = "BUNDLE")]
    dmi: Option<String>,
    /// The program's ELF, for symbols.
    ///
    /// Places can then be named, and PCs are shown inside functions. In the
    /// console the program's own ELF is used.
    #[arg(long, value_name = "PATH")]
    elf: Option<PathBuf>,
}

/// How `cpu boot` and `cpu console` find the RTT control block.
#[derive(clap::Args, Debug, Clone)]
struct AttachArgs {
    /// Address of the RTT control block, instead of `_SEGGER_RTT`.
    ///
    /// For an image whose symbols are not physical addresses, such as a kernel
    /// behind an MMU.
    #[arg(long, value_name = "ADDR", value_parser = parse_u64)]
    rtt: Option<u64>,
    /// The field widths of the control block, when they cannot be told apart.
    ///
    /// `rtt-target` makes every field as wide as a pointer; SEGGER's C code
    /// keeps the numbers at 32 bits.
    #[arg(long, value_name = "LAYOUT")]
    rtt_layout: Option<RttLayout>,
    /// How long to wait for the program to set up RTT, in milliseconds.
    #[arg(long, value_name = "MS", default_value_t = 3000)]
    wait_ms: u64,
    /// Give up when the program has not exited after this many seconds.
    ///
    /// The exit status is then 124, as from `timeout(1)`. Without it, `hio`
    /// waits until Ctrl-A x, which a test run cannot press.
    #[arg(long, value_name = "SECONDS", value_parser = parse_seconds)]
    timeout: Option<std::time::Duration>,
}

/// The exit status when `--timeout` runs out.
const TIMED_OUT: i32 = 124;

fn parse_seconds(s: &str) -> Result<std::time::Duration, String> {
    let secs: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("`{s}` is not a number of seconds"))?;
    std::time::Duration::try_from_secs_f64(secs)
        .map_err(|_| format!("`{s}` is not a time hio can wait (it must be 0 or more)"))
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
enum RttLayout {
    RttTarget,
    Segger,
}

fn parse_u32(s: &str) -> Result<u32, String> {
    parse_u64(s).map(|v| v as u32)
}

fn parse_u64(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(&h.replace('_', ""), 16)
            .map_err(|e| format!("{s:?} is not a number ({e})"));
    }
    // Accept `4k` and `256M`, the same spellings as `Harness.toml`. The rule
    // lives only in `hns-regs`, `_` handling included. Stripping `_` here would
    // make the error quote a spelling the user did not type.
    hns_regs::parse_size(s)
}

fn parse_usize(s: &str) -> Result<usize, String> {
    let v = parse_u64(s)?;
    usize::try_from(v).map_err(|_| format!("{v} does not fit on this machine"))
}

/// Parses a rate. Unlike a size, the suffixes are powers of 1000: `15M` Hz is
/// 15,000,000.
fn parse_rate_u32(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u32::from_str_radix(&h.replace('_', ""), 16)
            .map_err(|e| format!("{s:?} is not a number ({e})"));
    }
    let v = hns_regs::parse_rate(s)?;
    u32::try_from(v).map_err(|_| format!("{v} does not fit in 32 bits"))
}

fn parse_pair(s: &str) -> Result<(u16, u16), String> {
    let (a, b) = s
        .split_once(':')
        .ok_or_else(|| format!("expected `data:direction`, got {s:?}"))?;
    Ok((parse_u32(a)? as u16, parse_u32(b)? as u16))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// The probes `--serial` names. A serial equal to the value wins, so a full
/// serial still picks one probe when it is also a part of another's.
fn by_serial(found: Vec<hns_host::ftdi::Found>, part: &str) -> Vec<hns_host::ftdi::Found> {
    if found.iter().any(|f| f.serial.as_deref() == Some(part)) {
        return found
            .into_iter()
            .filter(|f| f.serial.as_deref() == Some(part))
            .collect();
    }
    found
        .into_iter()
        .filter(|f| f.serial.as_deref().is_some_and(|s| s.contains(part)))
        .collect()
}

/// Builds a `ProbeConfig` from what is plugged in.
///
/// It does not guess. A value it cannot fill stops it with a message that says
/// what to pass. Filled values are checked later (IR capture pattern, magic).
fn resolve_probe(cli: &Cli) -> Result<ProbeConfig, String> {
    // Values only the target description has (level shifter, usable TCK).
    // The command line wins, so a value can be overridden while debugging.
    let from_target = load_target(cli)?;

    let vid = cli.vid.map(|v| v as u16).or(from_target.vid);
    let pid = cli.pid.map(|v| v as u16).or(from_target.pid);
    let found = hns_host::ftdi::candidates(vid, pid).map_err(|e| e.to_string())?;
    let found: Vec<_> = found
        .into_iter()
        .filter(
            |f| match cli.product.as_deref().or(from_target.product.as_deref()) {
                Some(p) => f.product.as_deref() == Some(p),
                None => true,
            },
        )
        .collect();
    let found = match &cli.serial {
        Some(s) => {
            let named = by_serial(found.clone(), s);
            if named.is_empty() && !found.is_empty() {
                let list: Vec<String> = found.iter().map(|f| format!("  {f}")).collect();
                return Err(format!(
                    "no probe's serial contains {s:?}. These are plugged in:\n{}\n\
                     Pass one of their serials, or a part of it, with --serial.",
                    list.join("\n")
                ));
            }
            named
        }
        None => found,
    };

    let one = match found.len() {
        0 => {
            return Err("no FTDI probe with an MPSSE channel is plugged in.\n\
                 hio looked for the MPSSE product ids under vendor 0x0403. If your probe \
                 uses another id, pass --vid/--pid. `lsusb` shows them."
                .to_string());
        }
        1 => found[0].clone(),
        _ => {
            let list: Vec<String> = found.iter().map(|f| format!("  {f}")).collect();
            return Err(format!(
                "{} probes match, so hio cannot choose:\n{}\n\
                 Pick one with --serial (a part of it is enough when only one has it), \
                 or with --product.",
                found.len(),
                list.join("\n")
            ));
        }
    };

    // The level shifter is board wiring and USB cannot see it. Do not drive
    // GPIO on a guess: use the known value only for Digilent, and otherwise
    // touch only the four JTAG pins.
    let layout_init = cli
        .layout_init
        .or(from_target.layout_init)
        .unwrap_or_else(|| {
            let digilent = one
                .product
                .as_deref()
                .is_some_and(|p| p.contains("Digilent"));
            if digilent {
                hns_host::mpsse::LAYOUT_DIGILENT
            } else {
                hns_host::mpsse::LAYOUT_PLAIN
            }
        });

    // Use `host_tck_mhz`, not `max_tck_mhz`. The latter is the device limit
    // for the XDC constraint, not what the cable can carry (Arty claims 30 MHz,
    // but 20 MHz is the measured limit).
    let asked = cli
        .tck_hz
        .or_else(|| cli.tck_mhz.map(|mhz| (mhz * 1e6) as u32));
    let tck_hz = match asked {
        Some(hz) => hz,
        None => from_target
            .host_tck_mhz
            .map(|mhz| (mhz * 1e6) as u32)
            .unwrap_or(DEFAULT_TCK_HZ),
    };

    Ok(ProbeConfig {
        vid: one.vid,
        pid: one.pid,
        interface: cli.interface.or(from_target.interface).unwrap_or(0),
        serial: one.serial.clone(),
        product: one.product.clone(),
        layout_init,
        max_tck_hz: tck_hz,
        ir_length: cli
            .ir_length
            .or(from_target.ir_length)
            .unwrap_or(DEFAULT_IR_LENGTH),
        user1_ir: cli
            .user1_ir
            .or(from_target.user1_ir)
            .unwrap_or(DEFAULT_USER1_IR),
        chain: hns_host::mpsse::Chain::new(
            &from_target.chain_tdo_side,
            &from_target.chain_tdi_side,
        ),
    })
}

/// Upper bound on what `bench` sweeps without `--size`. It measures time per
/// access, so a few thousand words are enough.
const BENCH_DEFAULT_BYTES: u64 = 16 * 1024;

/// Most entries packed into one batch.
///
/// This is separate from the window size. A large window means fewer base
/// rewrites, but one window per round trip would be millions of frames over
/// JTAG. A lost frame is resent per batch, so a large batch is also costly
/// to resend.
const MAX_BATCH_ENTRIES: usize = 1024;

/// Default thread count for reading a region back over PCIe.
///
/// Measured on VCU118: 1.30us per read with 1 thread, 0.84us with 2, 0.77us
/// with 4, and still 0.77us with 8 or more. A 0.77us serial part cannot
/// overlap, so more threads only wait longer.
const MEMTEST_THREADS: usize = 4;

/// Read back more than this, and the card moves the data. `memtest` also uses
/// it to choose how to write, because the read back costs more.
///
/// The setup cost sets the limit. With 4 threads the window gives 4.8 MB/s
/// and the card 460 MB/s, but taking a huge page and telling the window
/// about it costs about 10ms. They break even near 50KB, so the limit is a
/// little above that.
const DMA_ABOVE: u64 = 64 * 1024;

/// Write only, and more than this, and the card moves the data.
///
/// Window writes are faster than reads: they are posted and do not wait for
/// a round trip (2GB in 75 s, 28.6 MB/s). They break even with the 10ms setup
/// near 290KB.
const DMA_WRITE_ABOVE: u64 = 256 * 1024;

/// Above this size, `memtest` no longer defaults to the whole memory.
const ASK_SIZE_ABOVE: u64 = 1 << 20;

/// Most addresses `memtest` lists. Beyond this it only gives the count.
const SHOW_BAD: usize = 32;

/// Entry count above which `memtest` prints progress.
const PROGRESS_ABOVE: usize = 1 << 16;

/// A TCK the cable is known to carry. Start slow.
const DEFAULT_TCK_HZ: u32 = 3_000_000;
/// IR length on 7-series. Checked against the IR capture pattern.
const DEFAULT_IR_LENGTH: u8 = 6;
/// USER1 on 7-series. Checked by whether the magic comes back.
const DEFAULT_USER1_IR: u32 = 2;

/// The `[jtag]` table of the target description. Empty (all `None`) when no
/// target is given.
fn load_target(cli: &Cli) -> Result<hns_targets::Jtag, String> {
    Ok(resolve_target(cli)?
        .as_ref()
        .map(hns_targets::jtag)
        .unwrap_or_default())
}

/// Parses one script line the same way as the command line.
#[derive(Parser)]
#[command(name = "", no_binary_name = true)]
struct Line {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Runs a script file, opening the link once.
///
/// The goal is a procedure that fits in one file. Shell functions carry paths,
/// `--target` and the serial, which differ per person and machine, so a pasted
/// procedure does not reproduce.
///
/// Programming is not allowed in a script, so a script can never erase the
/// board by surprise.
fn run_script(cli: &Cli, file: &str) -> Result<(), String> {
    let text = if file == "-" {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("cannot read standard input: {e}"))?;
        s
    } else {
        std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?
    };

    // Parse the whole file before touching the board. A typo on line 20 must
    // not fail after lines 1-19 have already run.
    let steps = parse_script(&text)?;

    let (map_path, map) = find_regs(cli)?;
    let mut bus = open_bus(cli, &map_path, &map)?;

    if !cli.no_verify {
        check_identity(&mut bus, &map)?;
    }

    for (n, line, step) in steps {
        run_step(cli, &mut bus, &map, step).map_err(|e| format!("line {n}: {line}\n{e}"))?;
    }
    Ok(())
}

/// One line of a script.
enum Step {
    Echo(String),
    Sleep(u64),
    Window(Cmd),
}

/// Parses the whole script into steps. It does not touch the board.
fn parse_script(text: &str) -> Result<Vec<(usize, String, Step)>, String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let step = parse_step(line).map_err(|e| format!("line {n}: {line}\n{e}"))?;
        out.push((n, line.to_string(), step));
    }
    if out.is_empty() {
        return Err("the script has nothing to run".to_string());
    }
    Ok(out)
}

fn parse_step(line: &str) -> Result<Step, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words[0] {
        // Headings and waits, so the output can be pasted as it is.
        "echo" => return Ok(Step::Echo(words[1..].join(" "))),
        "sleep" => {
            let ms: u64 = words
                .get(1)
                .and_then(|w| w.parse().ok())
                .ok_or("sleep needs a number of milliseconds")?;
            return Ok(Step::Sleep(ms));
        }
        _ => {}
    }

    let parsed = Line::try_parse_from(&words).map_err(|e| {
        format!(
            "{}\nCommands are written as on the command line. `echo` and `sleep` also work.",
            e.to_string().trim_end()
        )
    })?;

    match parsed.cmd {
        // Refuse programming and commands that reopen the link. A script that
        // can silently erase the board cannot be trusted.
        Cmd::Program { .. } | Cmd::Erase => Err(
            "programming is not available in a procedure file. Run `hio program` on its own."
                .to_string(),
        ),
        Cmd::Run { .. } => Err("a script cannot run another script".to_string()),
        // They take over the terminal and end the process with the
        // program's status.
        Cmd::Cpu(CpuCmd::Boot { .. } | CpuCmd::Console { .. }) => Err(
            "`cpu boot` and `cpu console` take over the terminal; run them on their own."
                .to_string(),
        ),
        Cmd::Probe
        | Cmd::Sysmon
        | Cmd::Dap(_)
        | Cmd::Targets
        | Cmd::Check
        | Cmd::Serve { .. }
        | Cmd::Completions { .. } => {
            Err("that only makes sense before the link is open; run it on its own.".to_string())
        }
        cmd => Ok(Step::Window(cmd)),
    }
}

fn run_step(cli: &Cli, bus: &mut Bus, map: &RegisterMap, step: Step) -> Result<(), String> {
    let cmd = match step {
        Step::Echo(text) => {
            println!("{text}");
            return Ok(());
        }
        Step::Sleep(ms) => {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            return Ok(());
        }
        Step::Window(cmd) => cmd,
    };

    match cmd {
        Cmd::Program { .. }
        | Cmd::Erase
        | Cmd::Run { .. }
        | Cmd::Cpu(CpuCmd::Boot { .. } | CpuCmd::Console { .. })
        | Cmd::Probe
        | Cmd::Sysmon
        | Cmd::Dap(_)
        | Cmd::Targets
        | Cmd::Serve { .. }
        | Cmd::Completions { .. } => {
            unreachable!("refused when the script was read")
        }
        Cmd::Cpu(cmd) => cpu_debug(bus, map, &cmd, None),
        Cmd::Id => id(bus, map),
        Cmd::Read { who, at, words } => do_read(bus, map, &who, at, words),
        Cmd::Write { who, values } => do_write(bus, map, &who, &values),
        Cmd::Dump => dump(bus, map),
        Cmd::Reset { hold, release } => reset(bus, map, hold, release),
        Cmd::Check => unreachable!("refused by parse_step"),
        Cmd::Drain { bundle, max } => drain(bus, map, &bundle, max),
        Cmd::Trace(cmd) => trace(bus, map, &cmd),
        Cmd::Bench {
            reads,
            repeat,
            bundle,
            both,
            base,
            size,
            threads,
        } => bench(
            bus,
            map,
            bundle.as_deref(),
            reads,
            repeat,
            both,
            base,
            size,
            threads,
        ),
        Cmd::Memtest {
            ref bundle,
            base,
            size,
            seed,
            threads,
            pio,
            dma,
        } => memtest(bus, map, cli, bundle, base, size, seed, threads, pio, dma),
        Cmd::Load {
            ref bundle,
            ref file,
            at,
            base,
            verify,
            pio,
            dma,
        } => load(bus, map, cli, bundle, file, at, base, verify, pio, dma),
        Cmd::DmaFire {
            bundle,
            at,
            len,
            seed,
            expect_fault,
            mps,
            to_card,
            pcie_addr,
        } => dma_fire(
            bus,
            map,
            cli,
            bundle.as_deref(),
            DmaFireArgs {
                at,
                len,
                seed,
                expect_fault,
                mps,
                to_card,
                pcie_addr,
            },
        ),
    }
}

/// Lists the shipped targets.
///
/// The table shares its layout with `veryl harness targets`
/// (`hns_targets::listing`). It adds what the host can do with each target:
/// reach the window, or program. A gap here should show before anyone tries
/// the board.
fn targets() -> Result<(), String> {
    let entries: Result<Vec<_>, _> = hns_targets::list().into_iter().collect();
    let listed = entries.map_err(|e| format!("{:?}", miette::Report::new(e)))?;
    if listed.is_empty() {
        println!("no target descriptions ship with this build");
        return Ok(());
    }

    let width = hns_targets::name_width(&listed);
    for entry in &listed {
        print!("{}", hns_targets::listing_row(entry, width));
        // A `Listed` entry has no tables, so resolve it again.
        let Ok(t) = hns_targets::resolve(&entry.name, &[]) else {
            continue;
        };
        let j = hns_targets::jtag(&t);
        let mut can = Vec::new();
        if j.user1_ir.is_some() && j.ir_length.is_some() {
            can.push("window");
        }
        if j.jprogram_ir.is_some()
            && j.jstart_ir.is_some()
            && j.bypass_ir.is_some()
            && (j.cfg_in_ir.is_some() || !j.slr_cfg_in_ir.is_empty())
        {
            can.push("program");
        }
        let tck = match (j.host_tck_mhz, j.max_tck_mhz) {
            (Some(h), _) => format!("{h} MHz measured"),
            (None, Some(m)) => format!("{m} MHz is the ceiling, none measured"),
            (None, None) => "no TCK stated".to_string(),
        };
        println!(
            "    jtag: {}  ({tck})",
            if can.is_empty() {
                "nothing yet -- the [jtag] section is not filled in".to_string()
            } else {
                can.join(", ")
            }
        );
    }
    println!();
    println!(
        "`window` means the [jtag] section can reach the harness. `program` means it can\n\
         also write a bitstream."
    );
    Ok(())
}

/// Shows what is plugged in and what answers.
fn probe(cli: &Cli) -> Result<(), String> {
    let found = hns_host::ftdi::candidates(cli.vid.map(|v| v as u16), cli.pid.map(|v| v as u16))
        .map_err(|e| e.to_string())?;
    if found.is_empty() {
        println!("no FTDI probe with an MPSSE channel is plugged in");
        return Ok(());
    }
    for f in &found {
        println!("{f}");
    }
    println!();

    let cfg = resolve_probe(cli)?;
    println!(
        "using   {:04x}:{:04x} interface {}",
        cfg.vid, cfg.pid, cfg.interface
    );
    println!(
        "layout  {:#06x}:{:#06x}",
        cfg.layout_init.0, cfg.layout_init.1
    );
    // The real TCK differs from the one asked for. The divisor is an integer
    // and rounds down (4MHz asked gives 3.75MHz). Print the real value, or a
    // frequency limit is misread while debugging.
    let div = hns_host::mpsse::tck_divisor(cfg.max_tck_hz);
    let real = hns_host::mpsse::tck_hz(div);
    println!(
        "tck     {:.3} MHz (asked for at most {:.3})",
        real as f64 / 1e6,
        cfg.max_tck_hz as f64 / 1e6
    );
    // Also show whether TCK is too fast for the window clock.
    if let Ok(Some((_, map))) = find_regs_opt(cli)
        && let (Some(mhz), Some(cycles)) = (map.window_clock_mhz, map.window_cycles)
    {
        let gap = cli
            .update_gap
            .unwrap_or_else(|| hns_host::mpsse::update_gap(real, mhz, cycles));
        println!("gap     {gap} TCK between scans (window {mhz} MHz, {cycles} cycles per command)");
    }

    let mut m = Mpsse::attach(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
        .map_err(|e| e.to_string())?;

    // This works without knowing the IR length. If nothing comes back, the
    // signal does not get through at all (often the level shifters).
    let chain = m.scan_chain(8).map_err(|e| e.to_string())?;
    if chain.is_empty() {
        return Err("the TAP answered all ones, which is not an IDCODE.\n\
             TDO never came back. On a Digilent module, the level shifters are off: pass \
             --layout-init. The values are in OpenOCD's interface/ftdi/*.cfg, and they \
             differ between Digilent modules."
            .to_string());
    }
    // The device nearest TDO answers first.
    let ours = cfg.chain.dr.tdo;
    for (i, d) in chain.iter().enumerate() {
        let near = if i == 0 && chain.len() > 1 {
            "  (nearest TDO)"
        } else {
            ""
        };
        let fpga = if i == ours && chain.len() > 1 {
            "  <- the FPGA hio talks to"
        } else {
            ""
        };
        match d {
            hns_host::mpsse::Device::Id(id) => {
                println!("device  {i}: idcode {id:#010x}{near}{fpga}")
            }
            hns_host::mpsse::Device::Bypass => {
                println!("device  {i}: no idcode (bypass){near}{fpga}")
            }
        }
    }
    let expected = cfg.chain.dr.tdo + cfg.chain.dr.tdi + 1;
    if chain.len() != expected {
        // Scans padded for the wrong chain silently reach the wrong device.
        if cfg.chain.is_alone() {
            println!(
                "        note: {} devices are in the chain, and the target says the FPGA is \
                 alone. List the IR lengths of the other devices in its [jtag], as \
                 `chain_tdo_side` (between the FPGA and TDO) and `chain_tdi_side` (between \
                 the FPGA and TDI).",
                chain.len()
            );
        } else {
            println!(
                "        note: {} devices are in the chain, but `chain_tdo_side` and \
                 `chain_tdi_side` in the target describe {expected}. Fix them to match.",
                chain.len()
            );
        }
    } else if let Some(want) = load_target(cli)?.idcode
        && let Some(hns_host::mpsse::Device::Id(got)) = chain.get(ours)
        // The top 4 bits are the silicon revision.
        && got & 0x0fff_ffff != want & 0x0fff_ffff
    {
        println!(
            "        note: device {ours} has idcode {got:#010x}, and the target expects \
             {want:#010x}. Check `chain_tdo_side` in the target, or the board."
        );
    }

    // Measure on the chain instead of looking it up, so unknown boards work.
    // This covers the whole chain, so the other devices' IRs add to ours.
    let others = cfg.chain.ir.tdo + cfg.chain.ir.tdi;
    match m.measure_ir_length().map_err(|e| e.to_string())? {
        Some(len) => {
            if others > 0 {
                println!("ir      {len} bits on the whole chain (measured)");
            } else {
                println!("ir      {len} bits (measured)");
            }
            if len as usize != cfg.ir_length as usize + others {
                if others > 0 {
                    println!(
                        "        note: the target gives {} bits: {} for the FPGA and {others} \
                         for the other devices. Fix `ir_length`, `chain_tdo_side` or \
                         `chain_tdi_side` in the target.",
                        cfg.ir_length as usize + others,
                        cfg.ir_length
                    );
                } else {
                    println!(
                        "        note: {} bits is being used instead. Set `ir_length = {len}` \
                         in the target, or pass --ir-length {len}. With more than one \
                         device, the bits are shared between them.",
                        cfg.ir_length
                    );
                }
            }
        }
        None => println!("ir      could not be measured (longer than 64 bits?)"),
    }

    let capture = m
        .scan_ir(cfg.user1_ir, cfg.ir_length)
        .map_err(|e| e.to_string())?;
    if !Mpsse::<Ftdi>::ir_length_looks_right(capture) {
        return Err(format!(
            "with --ir-length {} the IR captured {capture:#x}, and its low two bits are \
             not `01`.\n\
             IEEE 1149.1 fixes those two bits, so the length is wrong.",
            cfg.ir_length
        ));
    }
    println!("        capture {capture:#x}, as IEEE 1149.1 specifies");
    println!(
        "user ir {:#04x} (not checked here -- `id` is what confirms it)",
        cfg.user1_ir
    );
    Ok(())
}

/// Reads the system monitor through its JTAG instruction.
fn sysmon(cli: &Cli) -> Result<(), String> {
    let Some(target) = resolve_target(cli)? else {
        return Err(
            "`sysmon` needs to know the board: its IR value and the monitor type.\n\
             Pass --target naming the board."
                .to_string(),
        );
    };
    let jtag = hns_targets::jtag(&target);
    let Some(ir) = jtag.sysmon_ir else {
        return Err(format!(
            "`{}` does not say which IR value selects the system monitor.\n\
             Add `sysmon_ir` to its [jtag]. The device's BSDL lists it as `XADC_DRP` \
             (7-series) or `SYSMON_DRP` (UltraScale).",
            target.name
        ));
    };
    let family = &target.head.device.family;
    let Some(kind) = hns_host::sysmon::Kind::of_family(family) else {
        return Err(format!(
            "hio does not know the system monitor of the `{family}` family, so it cannot \
             convert what it reads.\n\
             Read the monitor in Vivado's hardware manager instead."
        ));
    };
    let cfg = resolve_probe(cli)?;
    let mut m = Mpsse::attach(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
        .map_err(|e| e.to_string())?;
    let capture = m.scan_ir(ir, cfg.ir_length).map_err(|e| e.to_string())?;
    if !Mpsse::<Ftdi>::ir_length_looks_right(capture) {
        return Err(format!(
            "the IR captured {capture:#x}, and its low two bits are not `01`.\n\
             The IR length ({}) is wrong, or the chain is. `hio probe` measures it.",
            cfg.ir_length
        ));
    }
    // Erasing or configuring the device restarts the monitor, and it reads 0
    // until its first conversion (measured: a few seconds after `erase`).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let r = loop {
        let r = hns_host::sysmon::read_all(&mut m, kind, family == "zynq")
            .map_err(|e| e.to_string())?;
        if r.has_reading() {
            break r;
        }
        if std::time::Instant::now() >= deadline {
            return Err("the system monitor has no reading yet.\n\
                 It restarts when the device is erased or configured. Run `hio sysmon` \
                 again in a few seconds."
                .to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };

    let reference = if r.internal_ref {
        "internal"
    } else {
        "external"
    };
    println!("monitor   {} ({reference} reference)", kind.name());
    println!(
        "{:<10}{:>9}{:>9}{:>9}     codes (now min max)",
        "", "now", "min", "max"
    );
    for s in &r.sensors {
        let (now, max, min) = r.values(s);
        let (now_code, max_code, min_code) = hns_host::sysmon::Readings::codes(s);
        let (unit, digits) = if s.temperature { ("C", 1) } else { ("V", 3) };
        let show = |v: Option<f64>| match v {
            Some(v) => format!("{v:>9.digits$}"),
            None => format!("{:>9}", "?"),
        };
        println!(
            "{:<10}{}{}{}  {unit}  {now_code:#06x} {min_code:#06x} {max_code:#06x}",
            s.name,
            show(now),
            show(min),
            show(max)
        );
    }
    match kind {
        hns_host::sysmon::Kind::SysmonE1 => println!(
            "note      hio does not know the temperature conversion of SYSMONE1, so only \
             the codes are shown."
        ),
        // Measured on a VCU118 with no bitstream: the same codes, 4 C less in
        // Vivado. With the external reference (an AS02MC04) Vivado agreed.
        hns_host::sysmon::Kind::SysmonE4 if r.internal_ref => println!(
            "note      temp uses the formula of Xilinx's drivers. Vivado's hardware manager \
             can show a few degrees less for the same codes."
        ),
        hns_host::sysmon::Kind::SysmonE4 | hns_host::sysmon::Kind::Xadc => {}
    }
    if r.over_temperature {
        println!(
            "ALARM     the over-temperature alarm is set. Power the board off and check its \
             cooling."
        );
    }
    Ok(())
}

type DapLink = hns_host::adi::Dap<Mpsse<Ftdi>>;

/// Opens the probe with the scans padded for the DAP instead of the FPGA.
///
/// Every other TAP, the FPGA included, gets BYPASS. Returns the DAP and the
/// target's `mem_ap`.
fn open_dap(cli: &Cli) -> Result<(DapLink, Option<u8>), String> {
    let (cfg, mem_ap) = dap_probe(cli)?;
    let m = Mpsse::attach(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
        .map_err(|e| e.to_string())?;
    Ok((hns_host::adi::Dap::new(m), mem_ap))
}

/// The probe settings with the scans padded for the DAP, and the access
/// port of the system memory. Touches no probe.
fn dap_probe(cli: &Cli) -> Result<(ProbeConfig, Option<u8>), String> {
    let jtag = load_target(cli)?;
    let Some(dap) = jtag.dap.clone() else {
        return Err(
            "the target does not say where an ARM DAP is on the JTAG chain.\n\
             Add `dap = { side = \"tdi\", index = 0 }` to its [jtag] (side: which of \
             `chain_tdi_side` / `chain_tdo_side` holds it; index: 0 nearest the FPGA)."
                .to_string(),
        );
    };
    let mut cfg = resolve_probe(cli)?;
    let (tdo, tdi) = dap_sides(&jtag, &dap, cfg.ir_length)?;
    cfg.chain = hns_host::mpsse::Chain::new(&tdo, &tdi);
    cfg.ir_length = hns_host::adi::IR_LEN;
    Ok((cfg, dap.mem_ap))
}

/// The IR lengths of the other TAPs on each side of the DAP: (TDO side, TDI
/// side). The lists in the target count from the FPGA outwards.
fn dap_sides(
    jtag: &hns_targets::Jtag,
    dap: &hns_targets::Dap,
    fpga_ir: u8,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let (own, other) = if dap.tdi_side {
        (&jtag.chain_tdi_side, &jtag.chain_tdo_side)
    } else {
        (&jtag.chain_tdo_side, &jtag.chain_tdi_side)
    };
    let Some(&len) = own.get(dap.index) else {
        return Err(format!(
            "`dap` says index {} on the {} side, and that side lists {} TAP(s).",
            dap.index,
            if dap.tdi_side { "TDI" } else { "TDO" },
            own.len()
        ));
    };
    if len != hns_host::adi::IR_LEN {
        return Err(format!(
            "the TAP that `dap` names has a {len}-bit IR; an ARM DAP has {}.",
            hns_host::adi::IR_LEN
        ));
    }
    // Toward the FPGA: the other side, the FPGA, and this side up to the DAP.
    let mut inner = other.clone();
    inner.push(fpga_ir);
    inner.extend_from_slice(&own[..dap.index]);
    let outer = own[dap.index + 1..].to_vec();
    Ok(if dap.tdi_side {
        (inner, outer)
    } else {
        (outer, inner)
    })
}

fn dap_cmd(cli: &Cli, cmd: &DapCmd) -> Result<(), String> {
    let (mut dap, mem_ap) = open_dap(cli)?;
    let e = |e: hns_host::adi::Error<_>| e.to_string();
    let idcode = dap.idcode().map_err(e)?;
    let pick = |arg: &ApArg| {
        arg.ap.or(mem_ap).ok_or_else(|| {
            "which access port reaches the memory is not known.\n\
             Pass --ap <n>, or add `mem_ap = <n>` to `dap` in the target. \
             `hio dap info` lists the ports."
                .to_string()
        })
    };
    match cmd {
        DapCmd::Info => {
            println!("dp      idcode {idcode:#010x}");
            dap.power_up().map_err(e)?;
            println!("        debug and system domains powered up");
            // APs are numbered from 0 with no gaps; the first empty IDR ends them.
            for ap in 0..=255u8 {
                let idr = dap.ap_read(ap, hns_host::adi::AP_IDR).map_err(e)?;
                if idr == 0 {
                    break;
                }
                let mark = if Some(ap) == mem_ap {
                    "  <- mem_ap"
                } else {
                    ""
                };
                println!("ap {ap:<4} {}{mark}", hns_host::adi::describe_idr(idr));
                // A MEM-AP's CSW says how its accesses look on the bus.
                if (idr >> 13) & 0xf == 8 {
                    let csw = dap.ap_read(ap, hns_host::adi::AP_CSW).map_err(e)?;
                    println!("        csw {csw:#010x}");
                }
            }
        }
        DapCmd::Read {
            addr,
            words,
            ap: arg,
        } => {
            let ap = pick(arg)?;
            dap.power_up().map_err(e)?;
            if let Some(csw) = arg.csw {
                dap.set_csw(ap, csw).map_err(e)?;
            }
            for row in (0..*words).step_by(4) {
                let at = addr.wrapping_add(row * 4);
                let mut line = format!("{at:#010x} ");
                for i in row..(*words).min(row + 4) {
                    let v = dap.read32(ap, addr.wrapping_add(i * 4)).map_err(e)?;
                    line.push_str(&format!(" {v:08x}"));
                }
                println!("{line}");
            }
        }
        DapCmd::Write {
            addr,
            values,
            ap: arg,
        } => {
            let ap = pick(arg)?;
            dap.power_up().map_err(e)?;
            if let Some(csw) = arg.csw {
                dap.set_csw(ap, csw).map_err(e)?;
            }
            for (i, v) in values.iter().enumerate() {
                dap.write32(ap, addr.wrapping_add(i as u32 * 4), *v)
                    .map_err(e)?;
            }
        }
        DapCmd::Ps7Init {
            file,
            no_post,
            ap: ap_arg,
        } => {
            let ap = pick(ap_arg)?;
            dap.power_up().map_err(e)?;
            if let Some(csw) = ap_arg.csw {
                dap.set_csw(ap, csw).map_err(e)?;
            }
            ps7_init(&mut dap, ap, file, !no_post)?;
        }
    }
    Ok(())
}

/// Runs a `ps7_init.tcl` through the DAP: the init stages, then, with
/// `post`, the post-configuration step (level shifters, PL reset).
fn ps7_init(dap: &mut DapLink, ap: u8, file: &std::path::Path, post: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| read_error(file, e))?;
    let init = hns_host::ps7::Ps7Init::parse(&text)?;
    ps7_run(dap, ap, &init, post)
}

/// Runs the steps of a `ps7_init.tcl` through the DAP.
fn ps7_run<T: hns_host::adi::Tap>(
    dap: &mut hns_host::adi::Dap<T>,
    ap: u8,
    init: &hns_host::ps7::Ps7Init,
    post: bool,
) -> Result<(), String>
where
    T::Error: std::fmt::Debug,
{
    let code = dap
        .read32(ap, hns_host::ps7::PS_VERSION_ADDR)
        .map_err(|e| e.to_string())?
        >> 28;
    let ver = init.version(code).to_string();
    println!("silicon version code {code}: running the {ver} steps");
    let mut stages: Vec<&str> = hns_host::ps7::INIT.to_vec();
    if post {
        stages.push(hns_host::ps7::POST);
    }
    let mut mem = DapMem { dap, ap };
    for stage in stages {
        let steps = init.stage(stage, &ver)?;
        let started = std::time::Instant::now();
        hns_host::ps7::run(&mut mem, steps).map_err(|err| match err {
            hns_host::ps7::RunError::Mem(m) => format!("ps7_{stage}: {m}"),
            hns_host::ps7::RunError::Poll { addr, mask, last } => format!(
                "ps7_{stage}: {addr:#010x} never showed {mask:#x} (last {last:#010x}).\n\
                 A PLL did not lock or the DDR did not finish initialising: \
                 is this ps7_init.tcl for this board?"
            ),
        })?;
        println!(
            "  ps7_{stage:<22} {:>4} steps  {:.2} s",
            steps.len(),
            started.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

/// Whether the target's `dram` is a Zynq PS's memory, which the host sets
/// up after every programming.
fn uses_ps7(target: Option<&hns_targets::Target>) -> bool {
    target
        .and_then(hns_targets::dram)
        .is_some_and(|d| d.is_ps7())
}

/// Whether this design reaches the PS's memory, so the PS must be set up
/// after programming. A design that keeps to the PL (no `dram` bundle) does
/// not: `make` writes no `ps7_init.tcl` for it, and its map has no
/// calibration register.
fn ps7_needed(cli: &Cli, bitstream: &std::path::Path) -> bool {
    let beside = bitstream
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("ps7_init.tcl")
        .is_file();
    beside
        || find_regs_opt(cli).ok().flatten().is_some_and(|(_, map)| {
            map.registers
                .iter()
                .any(|r| r.role.as_deref() == Some("calib"))
        })
}

/// Sets the PS up after programming, from the `ps7_init.tcl` beside the
/// bitstream (`make` puts it there, and `make zip` takes it along).
///
/// The PS is set up only by software, after every power-up. Until then its
/// DDR hangs the bus when touched, so programming is not done without it.
fn ps7_after_program(cli: &Cli, bitstream: &std::path::Path) -> Result<(), String> {
    let file = bitstream
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("ps7_init.tcl");
    if !file.is_file() {
        return Err(format!(
            "the bitstream is in, but the PS is not set up: {} is not there.\n\
             The harness reaches its memory through the Zynq's PS, which software sets up \
             after every power-up. `make` writes ps7_init.tcl beside the bitstream, and \
             `make zip` takes it along. Put it there, or run \
             `hio dap ps7-init <ps7_init.tcl>` yourself.",
            file.display()
        ));
    }
    let (mut dap, mem_ap) = open_dap(cli)?;
    let Some(ap) = mem_ap else {
        return Err(
            "the PS must be set up, and the target's `dap` has no `mem_ap`.\n\
             Add the access port of the system memory (`hio dap info` lists them)."
                .to_string(),
        );
    };
    dap.power_up().map_err(|e| e.to_string())?;
    println!("setting the PS up from {}", file.display());
    ps7_init(&mut dap, ap, &file, true)
}

/// Only clears the configuration memory. This checks the JPROGRAM IR value on
/// its own.
fn erase(cli: &Cli) -> Result<(), String> {
    let jtag = load_target(cli)?;
    let Some(jprogram) = jtag.jprogram_ir else {
        return Err("the target does not say which IR value selects JPROGRAM.\n\
             Pass --target naming the board."
            .to_string());
    };
    let cfg = resolve_probe(cli)?;
    let mut m = Mpsse::attach(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
        .map_err(|e| e.to_string())?;
    hns_host::config::erase(
        &mut m,
        jprogram,
        cfg.ir_length,
        hns_host::config::ERASE_WAIT,
    )
    .map_err(|e| e.to_string())?;
    println!(
        "sent JPROGRAM (ir {jprogram:#04x}).\n\
         If the board was running a harness, its heartbeat UART should stop now. That \
         confirms it was really JPROGRAM."
    );
    Ok(())
}

/// Finds the bitstream when `program` is given no file.
///
/// `gen` writes it to `syn/output/`, whose parent holds `regs.json`. So once
/// the map is found, the bitstream is found too.
///
/// A `.bit` wins over a `.svf`: `make svf` writes the `.svf` from the `.bit`,
/// so the two are the same design. Only with no `.bit` is a single `.svf`
/// taken. With two or more of the same kind it lists them and refuses.
/// Silently writing the wrong one leaves a board that does not work, with no
/// clue why.
fn beside_the_map(cli: &Cli) -> Result<PathBuf, String> {
    let (map_path, _) = find_regs(cli)?;
    bitstream_beside(&map_path)
}

/// Looks next to the map. Kept apart from reading the map, so it can be tested
/// without a board or a real `regs.json`.
fn bitstream_beside(map_path: &str) -> Result<PathBuf, String> {
    let root = std::path::Path::new(&map_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let dir = root.join("syn").join("output");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| {
            format!(
                "no bitstream was named, and {} cannot be read ({e}).\n\
                 `make -C {}/syn bit` writes one there, or name the file.",
                dir.display(),
                root.display()
            )
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("bit") || e.eq_ignore_ascii_case("svf"))
        })
        .collect();
    found.sort();
    let is_bit = |p: &PathBuf| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bit"));
    if found.iter().any(is_bit) {
        found.retain(is_bit);
    }
    match found.len() {
        1 => {
            println!("using {}", found[0].display());
            Ok(found.remove(0))
        }
        0 => Err(format!(
            "no bitstream was named, and there is none in {}.\n\
             `make -C {}/syn bit` writes one there, or name the file.",
            dir.display(),
            root.display()
        )),
        _ => Err(format!(
            "no bitstream was named, and {} holds several:\n{}\n\
             Name the one to write.",
            dir.display(),
            found
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

/// Writes a bitstream over JTAG, without Vivado.
fn program(
    cli: &Cli,
    file: &PathBuf,
    allow_any_device: bool,
    erase_wait_ms: u64,
) -> Result<(), String> {
    if file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svf"))
    {
        return play_svf(cli, file);
    }

    let bytes = std::fs::read(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let bit = hns_host::bitstream::parse(&bytes).map_err(|e| e.to_string())?;
    println!("{}  {}  {} {}", bit.top(), bit.part, bit.date, bit.time);

    let target = resolve_target(cli)?;
    let (m, cfg) = configure(
        cli,
        target.as_ref(),
        &bit,
        allow_any_device,
        std::time::Duration::from_millis(erase_wait_ms),
    )?;

    // A Zynq's PS memory needs the PS set up, through the DAP. The probe is
    // released for that and opened again after.
    let ps7 = uses_ps7(target.as_ref()) && ps7_needed(cli, file);
    let mut m = if ps7 {
        drop(m);
        ps7_after_program(cli, file)?;
        Mpsse::attach(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
            .map_err(|e| e.to_string())?
    } else {
        m
    };

    // If the bitstream is a harness, the window returning the magic is the
    // strongest proof that it worked.
    //
    // Reuse the same handle. Reopening clashes with the one still held and
    // fails with `interface is busy`.
    match after_config_map(cli, "configuration finished") {
        Some(map) => {
            // After configuration, reset the TAP, then select the USER slot again.
            let mut cmds = Vec::new();
            hns_host::mpsse::push_tap_reset(&mut cmds);
            m.send(&cmds).map_err(|e| e.to_string())?;
            m.scan_ir(cfg.user1_ir, cfg.ir_length)
                .map_err(|e| e.to_string())?;
            set_update_gap(cli, &map, &mut m);
            let mut bus = Bus::Jtag(Bridge::new(m, map.addr_bits() as u32));
            if ps7 {
                id_after_ps7(&mut bus, &map)
            } else {
                id(&mut bus, &map)
            }
        }
        None => Ok(()),
    }
}

/// Checks the bitstream's device against the target and sends it over JTAG.
/// Answers the probe, still open.
fn configure(
    cli: &Cli,
    target: Option<&hns_targets::Target>,
    bit: &hns_host::bitstream::Bitstream,
    allow_any_device: bool,
    erase_wait: std::time::Duration,
) -> Result<(Mpsse<Ftdi>, ProbeConfig), String> {
    let (ir, chunks) = config_plan(target, bit, allow_any_device)?;
    let cfg = resolve_probe(cli)?;
    let mut m = Mpsse::attach(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
        .map_err(|e| e.to_string())?;
    send_config(&mut m, &ir, cfg.ir_length, bit, &chunks, erase_wait)?;
    Ok((m, cfg))
}

/// Sends the bitstream through a probe already set up.
fn send_config(
    m: &mut Mpsse<Ftdi>,
    ir: &hns_host::config::ConfigIr,
    ir_length: u8,
    bit: &hns_host::bitstream::Bitstream,
    chunks: &[hns_host::config::Chunk],
    erase_wait: std::time::Duration,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    hns_host::config::program(m, ir, ir_length, &bit.data, chunks, erase_wait)
        .map_err(|e| e.to_string())?;
    println!(
        "sent {} bytes in {:.2} s",
        bit.data.len(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Checks the bitstream's device against the target, and works out the IR
/// values and which part of the bitstream goes where. Touches no probe.
fn config_plan(
    target: Option<&hns_targets::Target>,
    bit: &hns_host::bitstream::Bitstream,
    allow_any_device: bool,
) -> Result<(hns_host::config::ConfigIr, Vec<hns_host::config::Chunk>), String> {
    // Check the device before writing. Without a target there is nothing to
    // compare with, so do not go on silently.
    match (target, allow_any_device) {
        (Some(t), _) => {
            let part = &t.head.device.part;
            if !hns_host::bitstream::part_matches(&bit.part, part) {
                let e = hns_host::bitstream::Error::WrongDevice {
                    bitstream: bit.part.clone(),
                    target: part.clone(),
                };
                if !allow_any_device {
                    return Err(e.to_string());
                }
                eprintln!("warning: {e}");
            }
        }
        (None, true) => eprintln!("warning: no target given, so the device is not checked"),
        (None, false) => {
            return Err(
                "no target was given, so hio cannot check the bitstream's device.\n\
                 Pass --target (or --target-file) naming the board this .bit was built for. \
                 --allow-any-device skips the check."
                    .to_string(),
            );
        }
    }

    let jtag = target.map(hns_targets::jtag).unwrap_or_default();
    let ir = match (jtag.jprogram_ir, jtag.jstart_ir, jtag.bypass_ir) {
        (Some(jprogram), Some(jstart), Some(bypass)) => hns_host::config::ConfigIr {
            jprogram,
            jstart,
            bypass,
            ready: jtag.ready.map(|c| hns_host::config::Check {
                ir: c.ir,
                expect: c.expect,
                mask: c.mask,
            }),
            done: jtag.done.map(|c| hns_host::config::Check {
                ir: c.ir,
                expect: c.expect,
                mask: c.mask,
            }),
        },
        _ => {
            return Err(
                "the target does not say which IR values select JTAG configuration.\n\
                 `jprogram_ir`, `jstart_ir` and `bypass_ir` are needed. They differ by \
                 device family. Find them in the device's BSDL file, under \
                 data/parts/xilinx/<family>/public/bsdl/ in Vivado."
                    .to_string(),
            );
        }
    };
    let chunks = split(bit, &jtag)?;
    Ok((ir, chunks))
}

/// `id`, given a moment to come back after the PS was set up.
///
/// Setting the PS up rewrites its MIO, and on the PYNQ-Z1 the PL clock comes
/// from the Ethernet PHY on those pins: the window stopped for a moment
/// after ps7_init and the first read was dropped as busy (measured). So it
/// is tried again for a short while before it counts as a failure.
fn id_after_ps7(bus: &mut Bus, map: &RegisterMap) -> Result<(), String> {
    let started = std::time::Instant::now();
    loop {
        match id(bus, map) {
            Ok(()) => return Ok(()),
            Err(_) if started.elapsed() < PS7_SETTLE => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// How long the window may take to come back after ps7_init.
const PS7_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Finds the map used to check the board after configuration.
///
/// Configuration itself succeeded, so a missing map is not an error. If the
/// map could not be read, say why. "Not found" would tell someone who already
/// passed `--regs` to pass `--regs`. With `--no-verify` nothing is checked or
/// said: the bitstream need not be a harness.
fn after_config_map(cli: &Cli, what: &str) -> Option<RegisterMap> {
    if cli.no_verify {
        return None;
    }
    match find_regs_opt(cli) {
        Ok(Some((_, map))) => Some(map),
        Ok(None) => {
            println!(
                "{what}, but it was not checked: no register map was found (looked for {}).\n\
                 Pass --regs with the map for this bitstream to check that the window \
                 answers.",
                match &cli.regs {
                    Some(path) => path.as_str(),
                    None => REGS_PLACES,
                }
            );
            None
        }
        Err(e) => {
            println!("{what}, but it was not checked: {e}");
            None
        }
    }
}

/// Decides which part of the `.bit` goes to which `CFG_IN`.
///
/// One SLR gets everything in one go. With several, the `.bit` says where
/// each part starts and ends, and the target says which `CFG_IN` reaches
/// each SLR, in file order.
fn split(
    bit: &hns_host::bitstream::Bitstream,
    jtag: &hns_targets::Jtag,
) -> Result<Vec<hns_host::config::Chunk>, String> {
    let pieces = bit.slr_pieces().map_err(|e| e.to_string())?;
    let slrs = pieces.iter().map(|p| p.slr + 1).max().unwrap_or(1);
    if slrs == 1 {
        if jtag.slr_cfg_in_ir.len() > 1 {
            return Err(format!(
                "the target names {} SLRs, but the bitstream holds one.\n\
                 It was probably built for another device.",
                jtag.slr_cfg_in_ir.len()
            ));
        }
        let cfg_in = jtag
            .cfg_in_ir
            .or(jtag.slr_cfg_in_ir.first().copied())
            .ok_or("the target does not say which IR value selects CFG_IN.".to_string())?;
        return Ok(vec![hns_host::config::Chunk {
            at: 0,
            bytes: bit.data.len(),
            cfg_in,
            sync: false,
        }]);
    }
    if jtag.slr_cfg_in_ir.len() != slrs {
        return Err(format!(
            "the bitstream holds {slrs} sub-bitstreams (one per SLR), but the target names \
             {} `slr_cfg_in_ir` values.\n\
             List the CFG_IN of each SLR in the order its sub-bitstream appears in the .bit \
             (the master SLR first). The BSDL file has one CFG_IN per SLR.",
            jtag.slr_cfg_in_ir.len()
        ));
    }
    Ok(pieces
        .iter()
        .map(|p| hns_host::config::Chunk {
            at: p.at,
            bytes: p.bytes,
            cfg_in: jtag.slr_cfg_in_ir[p.slr],
            sync: p.tail,
        })
        .collect())
}

/// Replays an SVF as it is.
///
/// The configuration steps come from the file, not from the target
/// description, so it works on a device this build does not know. Vivado
/// writes an SVF without a board, so the FPGA host needs no Vivado.
fn play_svf(cli: &Cli, file: &PathBuf) -> Result<(), String> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;

    let cfg = resolve_probe(cli)?;
    let mut m = Mpsse::attach(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
        .map_err(|e| e.to_string())?;

    let total: u64 = text
        .split(';')
        .filter_map(|s| {
            let w: Vec<&str> = s.split_whitespace().collect();
            match w.first() {
                Some(&"SDR") | Some(&"SIR") => w.get(1)?.parse::<u64>().ok(),
                _ => None,
            }
        })
        .sum();
    println!("{} : {total} bits to shift", file.display());

    let start = std::time::Instant::now();
    let mut shown = 0u64;
    let mut player = hns_host::svf::Player::new(&mut m);
    player
        .run(&text, |done| {
            // Every 1%. It takes long, and with no output it looks stuck.
            if total > 0 && done * 100 / total > shown {
                shown = done * 100 / total;
                eprint!("\r  {shown}%");
            }
        })
        .map_err(|e| e.to_string())?;
    eprintln!("\r  done in {:.1} s", start.elapsed().as_secs_f64());

    // As after a `.bit`: a Zynq's PS memory needs the PS set up.
    let ps7 = uses_ps7(resolve_target(cli).ok().flatten().as_ref()) && ps7_needed(cli, file);
    let mut m = if ps7 {
        drop(m);
        ps7_after_program(cli, file)?;
        Mpsse::attach(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
            .map_err(|e| e.to_string())?
    } else {
        m
    };

    // Check that the harness answers.
    match after_config_map(cli, "the SVF replayed without complaint") {
        Some(map) => {
            let mut cmds = Vec::new();
            hns_host::mpsse::push_tap_reset(&mut cmds);
            m.send(&cmds).map_err(|e| e.to_string())?;
            m.scan_ir(cfg.user1_ir, cfg.ir_length)
                .map_err(|e| e.to_string())?;
            set_update_gap(cli, &map, &mut m);
            let mut bus = Bus::Jtag(Bridge::new(m, map.addr_bits() as u32));
            if ps7 {
                id_after_ps7(&mut bus, &map)
            } else {
                id(&mut bus, &map)
            }
        }
        None => Ok(()),
    }
}

/// Where to look for `regs.json`.
///
/// Fixed default places keep a written procedure portable. A path typed each
/// time only works on the machine it was typed on.
///
/// `gen --out-dir hns/<board>` puts the map one level deeper. When only one
/// such map exists, it is the one meant; with several, guessing could pick
/// another board's map, so the user is asked.
fn regs_candidates(cli: &Cli) -> Result<Vec<String>, String> {
    match &cli.regs {
        Some(p) => Ok(vec![p.clone()]),
        None => regs_candidates_in(std::path::Path::new(".")),
    }
}

/// The default places, under `root`. The paths returned are relative to it.
fn regs_candidates_in(root: &std::path::Path) -> Result<Vec<String>, String> {
    let fixed = vec!["regs.json".to_string(), "hns/regs.json".to_string()];
    if fixed.iter().any(|p| root.join(p).is_file()) {
        return Ok(fixed);
    }
    let mut nested: Vec<String> = std::fs::read_dir(root.join("hns"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join("regs.json").is_file())
        .map(|entry| format!("hns/{}/regs.json", entry.file_name().to_string_lossy()))
        .collect();
    nested.sort();
    match nested.as_slice() {
        [] | [_] => Ok(fixed.into_iter().chain(nested).collect()),
        many => Err(format!(
            "there are {} register maps under hns/, and hio will not guess which board \
             this is:\n{}\nPass one with --regs, or run hio in its directory.",
            many.len(),
            many.iter()
                .map(|p| format!("    {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

/// The places `regs_candidates` looks, for messages.
const REGS_PLACES: &str = "regs.json, hns/regs.json, hns/*/regs.json";

/// Reads the map. "Missing" and "unreadable" are different results.
///
/// `Ok(None)` means only that no candidate exists. A file that exists but has
/// the wrong version or is broken gives `Err` with the reason. Mixing the two
/// turns a stale `regs.json` into "no map found", and tells someone who passed
/// `--regs` to pass `--regs`.
fn read_map(candidates: &[String]) -> Result<Option<(String, RegisterMap)>, String> {
    for path in candidates {
        if !std::path::Path::new(path).is_file() {
            continue;
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let map = RegisterMap::from_json(&text).map_err(|e| format!("{path}: {e}"))?;
        return Ok(Some((path.clone(), map)));
    }
    Ok(None)
}

/// Like `find_regs`, but a missing map is `Ok(None)`. An unreadable map is
/// still an error with its reason.
fn find_regs_opt(cli: &Cli) -> Result<Option<(String, RegisterMap)>, String> {
    read_map(&regs_candidates(cli)?)
}

fn find_regs(cli: &Cli) -> Result<(String, RegisterMap), String> {
    let candidates = regs_candidates(cli)?;
    match read_map(&candidates)? {
        Some(found) => Ok(found),
        None => Err(format!(
            "no register map was found (looked for {}).\n\
             `veryl harness gen` writes one into hns/. Run from the project directory, \
             copy regs.json here, or pass --regs.",
            match &cli.regs {
                Some(path) => path.as_str(),
                None => REGS_PLACES,
            }
        )),
    }
}

/// Stops when the map was generated from a target file. The map does not
/// record which file (`RegisterMap::target_source`), so the user must pass it.
///
/// Older generators wrote the file path into `target`. Looking it up as a
/// name always fails without pointing at `regs.json`, so it is caught here.
/// The recorded path is not used: the file may have changed since generation.
fn file_origin(path: &str, map: &RegisterMap) -> Result<(), String> {
    let legacy = map
        .target
        .as_deref()
        .filter(|target| map.target_source.is_none() && target.ends_with(".toml"));
    if map.target_source.as_deref() == Some("file") {
        return Err(format!(
            "{path} was generated from a target file, and does not record which one.\n\
             Pass the same file: hio --target-file <path> <command>"
        ));
    }
    if let Some(file) = legacy {
        return Err(format!(
            "{path} was generated from the target file {file}, not from a board name.\n\
             Pass the file: hio --target-file {file} <command>\n\
             Or regenerate {path} with this veryl-harness, and pass the file each time."
        ));
    }
    Ok(())
}

/// Resolves `--target` / `--target-file`. `None` when neither is given and the
/// map records no target.
fn resolve_target(cli: &Cli) -> Result<Option<hns_targets::Target>, String> {
    // Without a flag, use the target recorded in `regs.json`. The map belongs
    // to one bitstream, which was built for one board.
    //
    // An unreadable map is an error. A silent `None` would report "no target
    // given" instead of pointing at the stale `regs.json`.
    let from_map = if cli.target.is_none() && cli.target_file.is_none() {
        match find_regs_opt(cli)? {
            Some((path, map)) => {
                file_origin(&path, &map)?;
                map.target
            }
            None => None,
        }
    } else {
        None
    };
    match (&cli.target, &cli.target_file, &from_map) {
        (Some(name), _, _) | (None, None, Some(name)) => {
            hns_targets::resolve(name, &cli.target_patch)
                .map(Some)
                .map_err(|e| format!("{:?}", miette::Report::new(e)))
        }
        (None, Some(path), _) => hns_targets::load_file(path, &cli.target_patch)
            .map(Some)
            .map_err(|e| format!("{:?}", miette::Report::new(e))),
        (None, None, None) => Ok(None),
    }
}

/// Says where the file was looked for. A relative path in a script starts from
/// the current directory, not from the script, so a file placed next to the
/// script is not found when the script runs from elsewhere.
fn read_error(file: &std::path::Path, err: std::io::Error) -> String {
    if file.is_absolute() {
        return format!("cannot read {}: {err}", file.display());
    }
    let here = std::env::current_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    format!(
        "cannot read {}: {err}\n\
         Relative paths start from the current directory ({here}), not from the file \
         that names them. Run from the data's directory, or use an absolute path.",
        file.display()
    )
}

/// Prints the completion script. It touches neither the probe nor `regs.json`.
///
/// Only subcommand and option names are completed. Register names would need
/// dynamic completion, which is an unstable API and would open USB on every TAB.
fn completions(shell: Shell) -> Result<(), String> {
    use std::io::Write;

    // Generate into a Vec first. With stdout passed directly, a closed pipe
    // (`hio completions zsh | head`) panics inside clap_complete (`expect`).
    // A closed pipe is not an error.
    let mut out = Vec::new();
    let mut cmd = with_help_flags(<Cli as clap::CommandFactory>::command());
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut out);

    match std::io::stdout().write_all(&out) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(format!("could not write the completion script: {e}")),
    }
}

/// Commands left out of the short help. `--list` and completions still show them.
const RARE_COMMANDS: &[&str] = &["dma-fire"];

fn run() -> Result<(), String> {
    // Both are answered before parsing: clap would ask for a command first.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        print!("{}", command_list());
        return Ok(());
    }
    if args == ["help", "options"] {
        print!("{}", options_help());
        return Ok(());
    }
    // clap's `help` prints the long help; `hio help` should match `hio --help`.
    if args == ["help"] {
        return short_help_command()
            .print_help()
            .map_err(|e| format!("could not print the help: {e}"));
    }
    let matches = short_help_command().get_matches();
    let cli =
        <Cli as clap::FromArgMatches>::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if cli.cmd.is_none() {
        return shell(&cli);
    }
    run_cli(cli)
}

/// The command as parsed: the short help lists the common commands and ends
/// the list with a pointer to `--list`, the way `cargo --help` does.
fn short_help_command() -> clap::Command {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    for name in RARE_COMMANDS {
        cmd = cmd.mut_subcommand(*name, |s| s.hide(true));
    }
    cmd = with_help_flags(cmd);
    let width = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set())
        .map(|s| s.get_name().len())
        .chain(["help".len()])
        .max()
        .unwrap_or(0);
    let header = cmd.get_styles().get_header();
    let heading = |text: &str| format!("{}{text}{}", header.render(), header.render_reset());
    let template = format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{}\n{{subcommands}}\n  {:width$}  See all commands with --list\n\n{}\n{{options}}{{after-help}}",
        heading("Commands:"),
        "...",
        heading("Options:"),
    );
    cmd.help_template(template)
}

/// Gives every command under `hio` its own `-h` / `--help`. Turning the help
/// flag off on `hio` turns it off below too. There `--help` is the long help,
/// the same as `hio help <command>`; only `hio --help` stays short.
fn with_help_flags(cmd: clap::Command) -> clap::Command {
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    names.iter().fold(cmd, |cmd, name| {
        cmd.mut_subcommand(name, |sub| {
            with_help_flags(sub).arg(
                clap::Arg::new("help")
                    .short('h')
                    .long("help")
                    .action(clap::ArgAction::Help)
                    .help("Print help"),
            )
        })
    })
}

/// `hio --list`: every command, one line each.
fn command_list() -> String {
    use clap::CommandFactory;
    let cmd = Cli::command();
    let subs: Vec<_> = cmd.get_subcommands().collect();
    let width = subs.iter().map(|s| s.get_name().len()).max().unwrap_or(0);
    let mut out = String::from("Commands:\n");
    for s in subs {
        let about = s.get_about().map(|a| a.to_string()).unwrap_or_default();
        out.push_str(&format!("  {:width$}  {about}\n", s.get_name()));
    }
    out
}

/// `hio help options`: every option before the command, with its full text.
fn options_help() -> String {
    use clap::CommandFactory;
    let mut cmd = Cli::command()
        .help_template("Options, given before the command:\n\n{options}\n")
        .color(clap::ColorChoice::Never);
    cmd.render_long_help().to_string()
}

/// Split from `run` so tests can check that refusals happen before the board
/// is touched.
fn run_cli(mut cli: Cli) -> Result<(), String> {
    let cmd = cli
        .cmd
        .take()
        .expect("`run` opens the prompt when there is no command");
    if let Cmd::Run { file } = &cmd {
        return run_script(&cli, file);
    }
    if let Cmd::Completions { shell } = cmd {
        return completions(shell);
    }
    if matches!(cmd, Cmd::Targets) {
        return targets();
    }
    if cli.connect.is_some() {
        refuse_over_connect(&cmd)?;
    }
    // The card moves data only over PCIe. Allowing `--dma` over JTAG would
    // test PCIe while the user thinks they test JTAG.
    let forced = match &cmd {
        Cmd::Memtest { dma: true, .. } => Some("memtest"),
        Cmd::Load { dma: true, .. } => Some("load"),
        _ => None,
    };
    if let Some(command) = forced
        && cli.transport_name() != "pcie"
    {
        return Err(not_over_pcie(cli.transport_name(), command));
    }
    // Parallel reads help only over PCIe. JTAG is one cable, so more threads
    // only wait longer. Refuse, or the numbers read as a bug.
    let asked_threads = match &cmd {
        Cmd::Bench { threads, .. } => Some(*threads),
        Cmd::Memtest { threads, .. } => *threads,
        _ => None,
    };
    if let Some(threads) = asked_threads
        && threads > 1
        && cli.transport_name() != "pcie"
    {
        return Err(format!(
            "`--threads` only does anything over PCIe, and this is {}.\n\
             Over JTAG, batching cuts round trips instead, and `bench` already batches \
             (`--reads`).\n\
             Add `-p` to measure the PCIe path.",
            cli.transport_name()
        ));
    }
    // Do not silently fall back to JTAG. Programming, erasing and SVF walk
    // the TAP and have no BAR equivalent. Without this, `--transport pcie`
    // would quietly use JTAG.
    if cli.transport_name() == "pcie" {
        let only_jtag = match &cmd {
            Cmd::Probe => Some("probe"),
            Cmd::Sysmon => Some("sysmon"),
            Cmd::Dap(_) => Some("dap"),
            Cmd::Program { .. } => Some("program"),
            Cmd::Erase => Some("erase"),
            _ => None,
        };
        if let Some(name) = only_jtag {
            return Err(format!(
                "`{name}` walks the JTAG TAP, so it cannot run over PCIe.\n\
                 Run it without `--transport pcie`. The JTAG cable works on a PCIe \
                 design too.\n\
                 After programming a live endpoint, remove the device and rescan the bus \
                 as root, or the machine keeps the old BARs."
            ));
        }
    }

    if matches!(cmd, Cmd::Probe) {
        return probe(&cli);
    }
    if matches!(cmd, Cmd::Sysmon) {
        return sysmon(&cli);
    }
    if let Cmd::Dap(dap) = &cmd {
        return dap_cmd(&cli, dap);
    }
    if let Cmd::Program {
        file,
        allow_any_device,
        erase_wait_ms,
    } = &cmd
    {
        let file = match file {
            Some(f) => f.clone(),
            None => beside_the_map(&cli)?,
        };
        if cli.connect.is_some() {
            return program_remote(&cli, &file, *erase_wait_ms);
        }
        return program(&cli, &file, *allow_any_device, *erase_wait_ms);
    }
    if matches!(cmd, Cmd::Erase) {
        return erase(&cli);
    }

    let (map_path, map) = find_regs(&cli)?;

    // `check` opens the window itself, so a window that does not open is one
    // line of its report.
    if matches!(cmd, Cmd::Check) {
        return check(&cli, &map_path, &map);
    }

    // A missing requester is known from the map. Check before opening, or the
    // error would be about the probe.
    if matches!(cmd, Cmd::DmaFire { .. }) {
        require_requester(&map)?;
    }

    let mut bus = open_bus(&cli, &map_path, &map)?;

    // Always check the identity first. Many probe values can come from
    // defaults, and a wrong one silently reaches something else. It costs
    // one round trip.
    if !cli.no_verify && !matches!(cmd, Cmd::Id) {
        check_identity(&mut bus, &map)?;
    }

    match cmd {
        Cmd::Targets
        | Cmd::Probe
        | Cmd::Sysmon
        | Cmd::Dap(_)
        | Cmd::Program { .. }
        | Cmd::Erase
        | Cmd::Run { .. }
        | Cmd::Check
        | Cmd::Completions { .. } => {
            unreachable!("handled above")
        }
        Cmd::Serve { listen } => serve(&cli, &mut bus, &map, &listen),
        Cmd::Id => id(&mut bus, &map),
        Cmd::Read { who, at, words } => do_read(&mut bus, &map, &who, at, words),
        Cmd::Write { who, values } => do_write(&mut bus, &map, &who, &values),
        Cmd::Dump => dump(&mut bus, &map),
        Cmd::Reset { hold, release } => reset(&mut bus, &map, hold, release),
        Cmd::Drain { bundle, max } => drain(&mut bus, &map, &bundle, max),
        Cmd::Trace(ref cmd) => trace(&mut bus, &map, cmd),
        Cmd::Memtest {
            ref bundle,
            base,
            size,
            seed,
            threads,
            pio,
            dma,
        } => memtest(
            &mut bus, &map, &cli, bundle, base, size, seed, threads, pio, dma,
        ),
        Cmd::Load {
            ref bundle,
            ref file,
            at,
            base,
            verify,
            pio,
            dma,
        } => load(
            &mut bus, &map, &cli, bundle, file, at, base, verify, pio, dma,
        ),
        Cmd::Bench {
            reads,
            repeat,
            bundle,
            both,
            base,
            size,
            threads,
        } => bench(
            &mut bus,
            &map,
            bundle.as_deref(),
            reads,
            repeat,
            both,
            base,
            size,
            threads,
        ),
        Cmd::Cpu(CpuCmd::Boot {
            ref elf,
            halt,
            catch,
            ref attach,
            ref args,
        }) => {
            if !args.is_empty() {
                eprintln!(
                    "warning: the firmware cannot receive arguments; ignoring {}",
                    args.join(" ")
                );
            }
            let code = boot(
                &cli,
                &mut bus,
                &map,
                elf,
                attach,
                Start::Load { halt, catch },
            )?;
            exit_with(code)
        }
        Cmd::Cpu(CpuCmd::Console {
            ref elf,
            ref attach,
        }) => {
            let code = boot(&cli, &mut bus, &map, elf, attach, Start::Attach)?;
            exit_with(code)
        }
        Cmd::Cpu(ref cmd) => cpu_debug(&mut bus, &map, cmd, None),
        Cmd::DmaFire {
            ref bundle,
            at,
            len,
            seed,
            expect_fault,
            mps,
            to_card,
            pcie_addr,
        } => dma_fire(
            &mut bus,
            &map,
            &cli,
            bundle.as_deref(),
            DmaFireArgs {
                at,
                len,
                seed,
                expect_fault,
                mps,
                to_card,
                pcie_addr,
            },
        ),
    }
}

impl Cli {
    /// The transport to use. JTAG by default.
    ///
    /// `-p` is short for `--transport pcie`, which is typed under sudo every
    /// time. A multi-letter short form like `-pcie` is not possible: it parses
    /// as `-p -c -i -e`.
    fn transport_name(&self) -> &str {
        if self.pcie {
            "pcie"
        } else if self.connect.is_some() {
            "a server (--connect)"
        } else {
            self.transport.as_deref().unwrap_or("jtag")
        }
    }
}

/// Checks that the server serves what regs.json describes, before any batch.
/// `from` says where its address came from.
fn check_server(
    cli: &Cli,
    link: &hns_host::hns2::Link,
    map: &RegisterMap,
    from: &str,
) -> Result<(), String> {
    let info = link.info();
    let sim = map.target.as_deref() == Some(hns_targets::SIM);
    let serves = match info.kind.as_str() {
        "sim" => "a simulation".to_string(),
        _ => format!(
            "a board ({})",
            info.target.as_deref().unwrap_or("target not named")
        ),
    };
    let fits = if sim {
        info.kind == "sim"
    } else {
        info.kind != "sim"
            && (info.target.is_none() || map.target.is_none() || info.target == map.target)
    };
    if !fits {
        return Err(format!(
            "the server at {from} serves {serves}, and regs.json is for {}.\n\
             Pass the regs.json the server uses (copy it from the server's machine).",
            map.target.as_deref().unwrap_or("a target file")
        ));
    }
    if let Some(hash) = info.map_hash
        && hash != map.map_hash
        && !cli.no_verify
    {
        return Err(format!(
            "the server at {from} uses a regs.json with map hash {hash:#010x}, and this one \
             has {:#010x}.\n\
             Pass the regs.json the server uses (copy it from the server's machine), or \
             put this design on the board: `hio --connect {from} program`.",
            map.map_hash
        ));
    }
    Ok(())
}

/// Refuses what a server cannot do for a client: it passes on batches for
/// the window, and programs a bitstream, and nothing else.
fn refuse_over_connect(cmd: &Cmd) -> Result<(), String> {
    let cable = match cmd {
        Cmd::Probe => Some("probe"),
        Cmd::Sysmon => Some("sysmon"),
        Cmd::Dap(_) => Some("dap"),
        Cmd::Program {
            file: Some(file), ..
        } if file
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svf")) =>
        {
            Some("program <svf>")
        }
        Cmd::Erase => Some("erase"),
        _ => None,
    };
    if let Cmd::Program {
        allow_any_device: true,
        ..
    } = cmd
    {
        return Err(
            "`--allow-any-device` does not go through `--connect`: the server checks the \
             bitstream against the board it serves.\n\
             Run `hio program` on the machine the board is on."
                .to_string(),
        );
    }
    if let Some(name) = cable {
        return Err(format!(
            "`{name}` works on the JTAG cable, and `--connect` reaches only the window.\n\
             Run it on the machine the board is on."
        ));
    }
    let dma = match cmd {
        Cmd::Memtest { dma: true, .. } => Some("memtest --dma"),
        Cmd::Load { dma: true, .. } => Some("load --dma"),
        Cmd::DmaFire { .. } => Some("dma-fire"),
        _ => None,
    };
    if let Some(name) = dma {
        return Err(format!(
            "`{name}` has the card move data in the memory of the machine it is in, and \
             `--connect` reaches only the window.\n\
             Run it on the machine the board is on, or use `--pio`."
        ));
    }
    if matches!(cmd, Cmd::Serve { .. }) {
        return Err("`serve` opens the window on this machine; drop `--connect`.".to_string());
    }
    Ok(())
}

/// `hio serve`: passes batches from clients to the window, until Ctrl-C.
fn serve(cli: &Cli, bus: &mut Bus, map: &RegisterMap, listen: &str) -> Result<(), String> {
    use hns_host::hns2;

    if map.target.as_deref() == Some(hns_targets::SIM) {
        return Err(
            "a simulation serves its window itself: start it with `veryl harness sim --addr`.\n\
             `hio serve` is for a board."
                .to_string(),
        );
    }
    let info = hns2::Info {
        server: format!("hio {}", env!("CARGO_PKG_VERSION")),
        kind: "board".to_string(),
        target: map.target.clone(),
        master: bus.master().to_string(),
        map_hash: Some(map.map_hash),
        max_ops: hns2::MAX_OPS,
        program_files: program_files(bus, resolve_target(cli)?.as_ref()),
    };
    // Saved while the card answers, to be written back after programming.
    let saved = match &*bus {
        Bus::Pcie(bar) => hns_host::reprogram::read_config(
            std::path::Path::new(hns_host::pcie::SYSFS),
            &bar.info().bdf,
        )
        .ok()
        .filter(|c| hns_host::reprogram::answering(c, bar.info().bar0_start)),
        _ => None,
    };
    let listener = std::net::TcpListener::bind(listen)
        .map_err(|e| format!("cannot listen on {listen}: {e}"))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    println!(
        "hio: serving {} over {} on {local}; Ctrl-C stops",
        map.dut, info.master
    );
    if !local.ip().is_loopback() {
        eprintln!("warning: {local} is reachable from other machines, and HNS2 has no password");
    }
    let mut window = BoardWindow {
        cli,
        bus,
        map: map.clone(),
        info,
        saved,
    };
    hns2::serve(listener, &mut window, |line| println!("hio: {line}")).map_err(|e| e.to_string())
}

/// The name of a Zynq-7000's PS setup, beside the bitstream.
const PS7_INIT_FILE: &str = "ps7_init.tcl";

/// The files `hio serve` asks for with a bitstream: those `hio program`
/// reads from beside it on this board.
fn program_files(bus: &Bus, target: Option<&hns_targets::Target>) -> Vec<String> {
    match bus {
        Bus::Jtag(_) if uses_ps7(target) => vec![PS7_INIT_FILE.to_string()],
        _ => Vec::new(),
    }
}

/// What `hio serve` serves: the window, and programming the board under it.
struct BoardWindow<'a> {
    cli: &'a Cli,
    bus: &'a mut Bus,
    /// The map of the design on the board. Programming replaces it.
    map: RegisterMap,
    info: hns_host::hns2::Info,
    /// The card's config space from when it last answered at its address.
    saved: Option<Vec<u8>>,
}

impl hns_host::hns2::Window for BoardWindow<'_> {
    fn info(&self) -> hns_host::hns2::Info {
        self.info.clone()
    }

    fn run(&mut self, batch: &Batch) -> Result<Reads, String> {
        self.bus.run(batch)
    }

    fn program(
        &mut self,
        request: &hns_host::hns2::Program,
    ) -> Result<hns_host::hns2::Info, String> {
        self.reprogram(request)
    }
}

impl BoardWindow<'_> {
    /// Programs the board and takes the map sent with the bitstream, once the
    /// window answers with it.
    fn reprogram(
        &mut self,
        request: &hns_host::hns2::Program,
    ) -> Result<hns_host::hns2::Info, String> {
        let next = std::str::from_utf8(request.map)
            .map_err(|_| "the regs.json sent is not UTF-8".to_string())
            .and_then(|text| {
                RegisterMap::from_json(text)
                    .map_err(|e| format!("the regs.json sent cannot be read: {e}"))
            })?;
        if next.target != self.map.target {
            return Err(format!(
                "the regs.json sent is for {}, and this server serves {}.\n\
                 Pass the regs.json of a design built for this board.",
                next.target.as_deref().unwrap_or("no named target"),
                self.map.target.as_deref().unwrap_or("no named target"),
            ));
        }
        let bit = hns_host::bitstream::parse(request.bitstream).map_err(|e| e.to_string())?;
        let target = resolve_target(self.cli)?;
        let erase_wait = std::time::Duration::from_millis(u64::from(request.erase_wait_ms));
        match &*self.bus {
            Bus::Pcie(_) => self.reprogram_pcie(&next, target.as_ref(), &bit, erase_wait)?,
            Bus::Jtag(_) => self.reprogram_jtag(
                &next,
                target.as_ref(),
                &bit,
                erase_wait,
                request.file(PS7_INIT_FILE),
            )?,
            Bus::Net(_) => unreachable!("`hio serve` opens a board, not a server"),
        }
        check_identity_settled(self.bus, &next)?;
        self.map = next;
        self.info.map_hash = Some(self.map.map_hash);
        Ok(self.info.clone())
    }

    /// Programs through the probe the window already holds, then selects the
    /// window again. The new design's window may be a different size, so the
    /// DR length follows the new map.
    ///
    /// Nothing about the PCIe side is checked, as with `hio program`: whether
    /// the machine also has the board as a card is the user's business.
    ///
    /// On a Zynq, the PS is set up from the `ps7_init.tcl` the client sent,
    /// through the DAP on the same probe.
    fn reprogram_jtag(
        &mut self,
        next: &RegisterMap,
        target: Option<&hns_targets::Target>,
        bit: &hns_host::bitstream::Bitstream,
        erase_wait: std::time::Duration,
        ps7_init: Option<&[u8]>,
    ) -> Result<(), String> {
        let ps7 = match ps7_init {
            _ if !uses_ps7(target) => None,
            Some(text) => {
                let text = std::str::from_utf8(text)
                    .map_err(|_| "the ps7_init.tcl sent is not UTF-8".to_string())?;
                let init = hns_host::ps7::Ps7Init::parse(text)
                    .map_err(|e| format!("the ps7_init.tcl sent: {e}"))?;
                let (dap_cfg, mem_ap) = dap_probe(self.cli)?;
                let Some(ap) = mem_ap else {
                    return Err(
                        "the PS must be set up, and the target's `dap` has no `mem_ap`.\n\
                         Add the access port of the system memory (`hio dap info` lists them)."
                            .to_string(),
                    );
                };
                Some((init, dap_cfg.chain, ap))
            }
            None if next
                .registers
                .iter()
                .any(|r| r.role.as_deref() == Some("calib")) =>
            {
                return Err(
                    "the new design reaches the PS's memory, so the PS must be set up after \
                     programming, and no ps7_init.tcl came with the bitstream. Nothing was \
                     programmed.\n\
                     `make` writes ps7_init.tcl beside the bitstream, and `hio --connect … \
                     program` sends it from there."
                        .to_string(),
                );
            }
            None => None,
        };
        let (ir, chunks) = config_plan(target, bit, false)?;
        let cfg = resolve_probe(self.cli)?;
        let Bus::Jtag(bridge) = &mut *self.bus else {
            unreachable!("called for a JTAG window")
        };

        // From here the board no longer runs the map the server had.
        self.info.map_hash = None;
        let m = bridge.io_mut();
        // The gap is for window commands; configuration runs without it.
        m.set_update_gap(0);
        let mut cmds = Vec::new();
        hns_host::mpsse::push_tap_reset(&mut cmds);
        m.send(&cmds).map_err(|e| e.to_string())?;
        send_config(m, &ir, cfg.ir_length, bit, &chunks, erase_wait)?;

        // The DAP is on the same chain: pad the scans for it for a while.
        if let Some((init, chain, ap)) = &ps7 {
            let fpga = m.chain();
            m.set_chain(*chain);
            let done = (|| {
                let mut cmds = Vec::new();
                hns_host::mpsse::push_tap_reset(&mut cmds);
                m.send(&cmds).map_err(|e| e.to_string())?;
                let mut dap = hns_host::adi::Dap::new(&mut *m);
                dap.power_up().map_err(|e| e.to_string())?;
                println!("setting the PS up from the ps7_init.tcl sent");
                ps7_run(&mut dap, *ap, init, true)
            })();
            m.set_chain(fpga);
            done?;
        }

        // After configuration, reset the TAP, then select the USER slot again.
        let mut cmds = Vec::new();
        hns_host::mpsse::push_tap_reset(&mut cmds);
        m.send(&cmds).map_err(|e| e.to_string())?;
        m.scan_ir(cfg.user1_ir, cfg.ir_length)
            .map_err(|e| e.to_string())?;
        set_update_gap(self.cli, next, m);
        bridge.set_addr_bits(next.addr_bits() as u32);
        Ok(())
    }

    /// Programs over JTAG while the machine keeps the card, then writes the
    /// card's config space back, so the window answers at the same address.
    ///
    /// A design that looks different from the bus is refused before anything
    /// is programmed: the machine would have to enumerate the card again.
    fn reprogram_pcie(
        &mut self,
        next: &RegisterMap,
        target: Option<&hns_targets::Target>,
        bit: &hns_host::bitstream::Bitstream,
        erase_wait: std::time::Duration,
    ) -> Result<(), String> {
        use hns_host::reprogram::{self, Shape};

        let Bus::Pcie(bar) = &*self.bus else {
            unreachable!("called for a PCIe window")
        };
        let bdf = bar.info().bdf.clone();
        let Some(pcie) = &next.pcie else {
            return Err(
                "the regs.json sent has no `pcie` section: that design has no PCIe \
                 endpoint, and the machine would lose the card.\n\
                 Generate it with `--transport pcie`."
                    .to_string(),
            );
        };

        let root = std::path::Path::new(hns_host::pcie::SYSFS);
        let card = hns_host::pcie::info_at(root, &bdf).map_err(|e| e.to_string())?;
        let changed = reprogram::changes(
            &Shape::of_card(&card),
            &Shape {
                vendor: pcie.vendor_id as u16,
                device: pcie.device_id as u16,
                class: pcie.class_code,
                bar_bytes: u64::from(pcie.bar_bytes),
                intx: next
                    .registers
                    .iter()
                    .any(|r| r.role.as_deref() == Some("irq_level")),
            },
        );
        if !changed.is_empty() {
            return Err(format!(
                "the new design looks different from the bus ({}). Nothing was \
                 programmed.\n\
                 The machine has to enumerate the card again, which a running server \
                 cannot do. On the board's machine: stop `hio serve`, run `hio program`, \
                 then as root remove the card and rescan the bus (reboot instead if the \
                 BAR grew past the bridge window), and start `hio serve` again.",
                changed.join(", ")
            ));
        }
        let config = reprogram::read_config(root, &bdf)
            .map_err(|e| format!("cannot read the config space of {bdf}: {e}"))?;
        if reprogram::answering(&config, card.bar0_start) {
            self.saved = Some(config);
        }
        let Some(saved) = self.saved.clone() else {
            return Err(format!(
                "{bdf} does not answer at its address now, and this server saved no \
                 config space from when it did. Nothing was programmed.\n\
                 Make the card answer by hand (as root, remove it and rescan the bus), \
                 then start `hio serve` again."
            ));
        };

        // From here the board no longer runs the map the server had.
        self.info.map_hash = None;
        let (m, _) = configure(self.cli, target, bit, false, erase_wait)?;
        drop(m);
        reprogram::wait_for_card(
            root,
            &bdf,
            card.vendor,
            card.device,
            std::time::Duration::from_secs(10),
        )?;
        reprogram::write_config(root, &bdf, &reprogram::restore_plan(&saved))
            .map_err(|e| format!("cannot write the config space of {bdf} back: {e}"))
    }
}

/// `check_identity`, given a moment for the harness to come out of reset
/// after programming.
fn check_identity_settled(bus: &mut Bus, map: &RegisterMap) -> Result<(), String> {
    let started = std::time::Instant::now();
    loop {
        match check_identity(bus, map) {
            Ok(()) => return Ok(()),
            Err(_) if started.elapsed() < std::time::Duration::from_secs(2) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// `program` through `hio serve`. The server programs the board, writes the
/// card's config space back, and takes this regs.json as its map.
fn program_remote(cli: &Cli, file: &std::path::Path, erase_wait_ms: u64) -> Result<(), String> {
    let addr = cli.connect.as_deref().expect("called with --connect");
    let (map_path, map) = find_regs(cli)?;
    let map_text = std::fs::read(&map_path).map_err(|e| format!("cannot read {map_path}: {e}"))?;
    let bytes = std::fs::read(file).map_err(|e| read_error(file, e))?;
    let bit = hns_host::bitstream::parse(&bytes).map_err(|e| e.to_string())?;
    println!("{}  {}  {} {}", bit.top(), bit.part, bit.date, bit.time);
    let erase_wait_ms = u32::try_from(erase_wait_ms)
        .map_err(|_| format!("--erase-wait-ms {erase_wait_ms} is too long"))?;

    let mut link = hns_host::hns2::Link::connect(addr).map_err(|e| e.to_string())?;
    let info = link.info();
    if info.kind == "sim" {
        return Err(format!(
            "the server at {addr} is a simulation, and it has no FPGA to program.\n\
             Generate the simulation again instead: `veryl harness gen --target sim`."
        ));
    }
    if info.target.is_some() && map.target.is_some() && info.target != map.target {
        return Err(format!(
            "the server at {addr} serves {}, and regs.json is for {}.\n\
             Pass the regs.json of a design built for that board.",
            info.target.as_deref().unwrap_or("a board"),
            map.target.as_deref().unwrap_or("another board"),
        ));
    }
    // The server names the files it uses from beside the bitstream (a
    // Zynq's PS setup), and decides whether a missing one matters.
    let beside = file.parent().unwrap_or(std::path::Path::new("."));
    for name in link.info().program_files.clone() {
        if !plain_file_name(&name) {
            return Err(format!(
                "the server at {addr} asks for a file named {name:?}, which is not a plain \
                 file name. Nothing was sent."
            ));
        }
        let path = beside.join(&name);
        if path.is_file() {
            let contents = std::fs::read(&path).map_err(|e| read_error(&path, e))?;
            link.send_file(&name, &contents)
                .map_err(|e| format!("the server at {addr} did not take {name}: {e}"))?;
            println!("sent {}", path.display());
        }
    }
    let start = std::time::Instant::now();
    link.program(erase_wait_ms, &map_text, &bytes)
        .map_err(|e| e.to_string())?;
    println!(
        "sent {} bytes; programmed in {:.2} s",
        bytes.len(),
        start.elapsed().as_secs_f64()
    );
    id(&mut Bus::Net(link), &map)
}

/// A name with no directory in it. A server names files for the client to
/// send, and must not reach anything but the ones beside the bitstream.
fn plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && std::path::Path::new(name).file_name() == Some(std::ffi::OsStr::new(name))
}

/// Sets the TCK gap between scans (`hns_host::mpsse::update_gap`).
///
/// The gap comes from the window clock. With a 50MHz window at Arty's default
/// TCK (15MHz), the bridge dropped back-to-back commands. A few extra cycles
/// between scans cost less than a slower TCK.
fn set_update_gap<C: hns_host::mpsse::Chan>(cli: &Cli, map: &RegisterMap, m: &mut Mpsse<C>) {
    let gap = cli
        .update_gap
        .unwrap_or_else(|| match (map.window_clock_mhz, map.window_cycles) {
            (Some(mhz), Some(cycles)) => hns_host::mpsse::update_gap(m.tck(), mhz, cycles),
            _ => 0,
        });
    m.set_update_gap(gap);
}

/// Opens the window over the chosen transport. JTAG is the default: a PCIe
/// design has both, and JTAG is the one used to debug the other.
fn open_bus(cli: &Cli, map_path: &str, map: &RegisterMap) -> Result<Bus, String> {
    // A map generated for the simulator has only the socket.
    let sim = map.target.as_deref() == Some(hns_targets::SIM);
    let transport = if sim && cli.transport.is_none() && !cli.pcie {
        "sim"
    } else {
        cli.transport_name()
    };
    if let Some(addr) = &cli.connect {
        let link = hns_host::hns2::Link::connect(addr).map_err(|e| e.to_string())?;
        check_server(cli, &link, map, addr)?;
        return Ok(Bus::Net(link));
    }
    match transport {
        "sim" => {
            let dir = std::path::Path::new(map_path)
                .parent()
                .unwrap_or(std::path::Path::new("."));
            let link = hns_host::hns2::Link::open(dir).map_err(|e| e.to_string())?;
            check_server(cli, &link, map, hns_host::hns2::ADDR_FILE)?;
            Ok(Bus::Net(link))
        }
        "jtag" => {
            let cfg = resolve_probe(cli)?;
            let mut io = Mpsse::open(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
                .map_err(|e| e.to_string())?;
            set_update_gap(cli, map, &mut io);
            Ok(Bus::Jtag(Bridge::new(io, map.addr_bits() as u32)))
        }
        "pcie" => Ok(Bus::Pcie(open_bar(cli, map)?)),
        other => Err(format!(
            "`--transport {other}` is not a transport this tool has; use `jtag`, `pcie` or `sim`."
        )),
    }
}

/// Finds exactly one PCIe card and opens its BAR.
///
/// The ID alone is not enough: the default ID is borrowed from an example and
/// may clash with another card. The BAR size is compared too, and the magic
/// is read later (`check_identity`).
fn open_bar(cli: &Cli, map: &RegisterMap) -> Result<hns_host::pcie::Bar, String> {
    let Some(pcie) = &map.pcie else {
        return Err(format!(
            "{} has no `pcie` section, so the design has no PCIe endpoint.\n\
             Generate with `--transport pcie` and re-synthesize, or drop `--transport pcie` \
             here.",
            "regs.json"
        ));
    };
    let bdf = find_bdf(cli, pcie)?;
    let bar = hns_host::pcie::Bar::open(&bdf).map_err(|e| e.to_string())?;
    bar.expect_bytes(pcie.bar_bytes as u64)
        .map_err(|e| e.to_string())?;
    Ok(bar)
}

/// The card's BDF. Separate from `open_bar` because `check` looks at the card
/// (decode, bus mastering, IOMMU) before it opens the window.
fn find_bdf(cli: &Cli, pcie: &hns_regs::Pcie) -> Result<String, String> {
    match &cli.bdf {
        Some(bdf) => hns_host::pcie::named_at(
            std::path::Path::new(hns_host::pcie::SYSFS),
            bdf,
            pcie.vendor_id as u16,
            pcie.device_id as u16,
        )
        .map_err(|e| e.to_string()),
        None => Ok(hns_host::pcie::find_at(
            std::path::Path::new(hns_host::pcie::SYSFS),
            pcie.vendor_id as u16,
            pcie.device_id as u16,
        )
        .map_err(|e| e.to_string())?
        .bdf),
    }
}

/// The lines of `check`. There are only three marks (`ok` / `NG` / `--`), so
/// the failures and their fixes stand out.
#[derive(Default)]
struct Checks {
    bad: usize,
}

impl Checks {
    fn ok(&self, what: &str, value: impl std::fmt::Display) {
        println!("ok  {what:<7} {value}");
    }

    /// A failed item. The fix follows on indented lines.
    fn ng(&mut self, what: &str, value: impl std::fmt::Display, fix: &str) {
        self.bad += 1;
        println!("NG  {what:<7} {value}");
        for line in fix.lines().filter(|l| !l.trim().is_empty()) {
            println!("            {}", line.trim_start());
        }
    }

    /// Prints an error as a failed item. It relies on this tool's error shape:
    /// the first line says what is wrong, the rest says how to fix it.
    fn ng_err(&mut self, what: &str, err: &str) {
        let (head, rest) = err.split_once('\n').unwrap_or((err, ""));
        self.ng(what, head, rest);
    }

    /// An item that could not be tried because an earlier one failed. It is
    /// printed, because "not tried" is not the same as "not there".
    fn skip(&self, what: &str, why: &str) {
        println!("--  {what:<7} {why}");
    }

    fn finish(&self) -> Result<(), String> {
        if self.bad == 0 {
            println!("ready");
            Ok(())
        } else {
            Err(format!(
                "{} check(s) failed; the fixes are under each NG line.",
                self.bad
            ))
        }
    }
}

/// Formats a size the way `--size` spells it (`64M`, `2M`, `4k`), so it can
/// be pasted back.
fn size_str(bytes: u64) -> String {
    for (unit, shift) in [("G", 30), ("M", 20), ("k", 10)] {
        if bytes >= 1 << shift && bytes.is_multiple_of(1 << shift) {
            return format!("{}{unit}", bytes >> shift);
        }
    }
    bytes.to_string()
}

/// Checks that everything this design needs is ready. Nothing is written.
///
/// One line per item, with a fix for each failure. An item that cannot be
/// tried because an earlier one failed gets `--` and a reason (for example,
/// the window cannot open while decode is off). It checks:
///
/// - always: the map, the window (magic, hash, `timeouts`), `dram` calibration
/// - over PCIe: the card, the BAR, decode, the link
/// - with a DMA engine: bus mastering, the IOMMU, huge pages, whether the DMA
///   engine is idle, and whether an address for the card can be taken
///
/// The point is to show at once what a reboot or reprogramming turns off
/// (decode, bus mastering, IOMMU, huge pages).
fn check(cli: &Cli, map_path: &str, map: &RegisterMap) -> Result<(), String> {
    use hns_host::{dma, pcie};

    let mut c = Checks::default();
    c.ok("map", format!("{map_path} (hash {:#010x})", map.map_hash));

    let pcie_run = cli.transport_name() == "pcie";
    let requester = pcie_run && map.get("dma_base").is_ok();
    // Whether to open the window. Do not open it when it is known to fail,
    // or a second error hides the first.
    let mut window = true;
    // Whether to try taking an address for the card (IOMMU and huge pages ready).
    let mut handover = requester;
    let mut bdf = None;

    if pcie_run {
        let Some(card) = &map.pcie else {
            c.ng(
                "card",
                "this map has no PCIe endpoint",
                "Generate with `--transport pcie` and re-synthesize, or drop `-p` here.",
            );
            return c.finish();
        };
        let found = match find_bdf(cli, card) {
            Ok(found) => found,
            Err(why) => {
                c.ng_err("card", &why);
                return c.finish();
            }
        };
        c.ok("card", &found);

        let root = std::path::Path::new(pcie::SYSFS);
        match pcie::info_at(root, &found) {
            Ok(info) => {
                if info.bar0_reg == Some(0) && info.bar0_start != 0 {
                    let why = pcie::Error::NotDecoding {
                        bdf: found.clone(),
                        assigned: info.bar0_start,
                    };
                    c.ng_err("bar", &why.to_string());
                    window = false;
                } else if info.bar0_bytes != u64::from(card.bar_bytes) {
                    c.ng(
                        "bar",
                        format!(
                            "{} on the card, {} in the map",
                            size_str(info.bar0_bytes),
                            size_str(u64::from(card.bar_bytes))
                        ),
                        "The card runs another bitstream: `hio program` the one beside the map.",
                    );
                    window = false;
                } else {
                    c.ok("bar", size_str(info.bar0_bytes));
                }
                // Another bitstream, or one from a generator that wrote the
                // class code in a form the PCIE3 IP ignores.
                match (card.class_code, info.class) {
                    (Some(want), Some(got)) if want != got => c.ng(
                        "class",
                        format!("{got:#08x} on the card, {want:#08x} in the map"),
                        "The card runs another bitstream, or one generated before the class code \
                         reached the PCIE3 IP. Generate again and re-synthesize.",
                    ),
                    (Some(_), Some(got)) => c.ok("class", format!("{got:#08x}")),
                    _ => {}
                }
                // With a `host_irq` bundle the card must have INTA, or the
                // driver gets no interrupt (a probe can fail with -22).
                if map
                    .registers
                    .iter()
                    .any(|r| r.role.as_deref() == Some("irq_level"))
                {
                    match (info.interrupt_pin, info.irq) {
                        (Some(0), _) => c.ng(
                            "irq",
                            "the card has no interrupt pin",
                            "The card runs a bitstream built without host_irq. Program the one \
                             this map was generated with.",
                        ),
                        (Some(_), Some(0) | None) => c.ok("irq", "INTA, no IRQ assigned yet"),
                        (Some(_), Some(irq)) => c.ok("irq", format!("INTA, IRQ {irq}")),
                        (None, _) => {}
                    }
                }
                if info.enabled {
                    c.ok("decode", "on");
                } else {
                    c.ng(
                        "decode",
                        "off",
                        &format!("echo 1 | sudo tee /sys/bus/pci/devices/{found}/enable"),
                    );
                    window = false;
                }
            }
            Err(why) => {
                c.ng_err("bar", &why.to_string());
                window = false;
            }
        }
        // A narrow link is not a failure, only slow. Show it next to the maximum.
        if let Some(link) = pcie::link_at(root, &found) {
            let mut value = format!("{} x{}", link.speed, link.width);
            if link.width < link.max_width || link.speed != link.max_speed {
                value += &format!(" (the card allows {} x{})", link.max_speed, link.max_width);
            }
            c.ok("link", value);
        }

        if requester {
            match dma::bus_master(&found) {
                Ok(true) => c.ok("master", "on"),
                Ok(false) => {
                    c.ng(
                        "master",
                        "off",
                        &format!("sudo setpci -s {found} COMMAND=0x4:0x4"),
                    );
                    handover = false;
                }
                Err(why) => {
                    c.ng_err("master", &why.to_string());
                    handover = false;
                }
            }
            match dma::iommu(&found) {
                Ok(dma::Iommu::Absent) => c.ok("iommu", "nothing translates for this card"),
                Ok(dma::Iommu::Identity { group }) => {
                    c.ok("iommu", format!("identity (group {group})"))
                }
                Ok(dma::Iommu::Translating { group, kind }) => {
                    c.ng(
                        "iommu",
                        format!("{kind} (group {group})"),
                        &format!("echo identity | sudo tee /sys/kernel/iommu_groups/{group}/type"),
                    );
                    handover = false;
                }
                Err(why) => {
                    c.ng_err("iommu", &why.to_string());
                    handover = false;
                }
            }
            match dma::hugepages() {
                Ok(pages) if pages.free > 0 => c.ok(
                    "pages",
                    format!(
                        "{} of {} free ({} each)",
                        pages.free,
                        pages.total,
                        size_str(pages.size_bytes)
                    ),
                ),
                Ok(pages) => {
                    c.ng(
                        "pages",
                        format!("{} of {} free", pages.free, pages.total),
                        "sudo sysctl -w vm.nr_hugepages=64",
                    );
                    handover = false;
                }
                Err(why) => {
                    c.ng_err("pages", &why.to_string());
                    handover = false;
                }
            }
        }
        bdf = Some(found);
    }

    // From here on, read through the window.
    let calib: Vec<&hns_regs::Register> = map
        .registers
        .iter()
        .filter(|r| r.role.as_deref() == Some("calib"))
        .collect();
    let mut bus = None;
    if !window {
        c.skip(
            "window",
            "cannot be opened until the NG lines above are fixed",
        );
    } else {
        match open_bus(cli, map_path, map) {
            Ok(opened) => bus = Some(opened),
            Err(why) => c.ng_err("window", &why),
        }
    }
    if let Some(bus) = &mut bus {
        match read_identity(bus, map).and_then(|(m, h)| check_identity_values(m, h, map)) {
            Err(why) => c.ng_err("window", &why),
            Ok(()) => {
                let timeouts = if map.get("harness_timeout").is_ok() {
                    rd(bus, offset_of(map, "harness_timeout")?)?
                } else {
                    0
                };
                if timeouts == 0 {
                    c.ok("window", "magic and map hash match, no timeouts");
                } else {
                    c.ng(
                        "window",
                        format!("the window answered on its own {timeouts} time(s)"),
                        "A terminator did not answer in time, so the window returned 0. \
                         Read it with `hio id`, then clear it: hio write harness_timeout 1",
                    );
                }
                for r in &calib {
                    let name = r.bundle.as_deref().unwrap_or(&r.name);
                    if rd(bus, r.offset as u32)? & 1 == 1 {
                        c.ok("memory", format!("{name} calibrated"));
                    } else {
                        c.ng(
                            "memory",
                            format!("{name} not calibrated"),
                            "The controller calibrates for tens of ms after configuration. \
                             If it stays 0, check the memory clock and the board reset.",
                        );
                    }
                }
                // Report a DUT still held by `--hold`. The window answers as
                // usual, so a forgotten release is otherwise invisible.
                if let Ok(state) = map.get("dut_reset_state")
                    && rd(bus, state.offset as u32)? & 1 == 1
                {
                    c.ok("dut", "held in reset (hio reset --release)");
                }
                if let (true, Some(found)) = (requester, &bdf) {
                    match require_idle(bus, map, found) {
                        Ok(()) => c.ok("engine", "idle"),
                        Err(why) => c.ng_err("engine", &why),
                    }
                }
            }
        }
    } else {
        if !calib.is_empty() {
            c.skip("memory", "needs the window");
        }
        if requester {
            c.skip("engine", "needs the window");
        }
    }

    // Last, take a real page: does pagemap give a physical address (needs root)?
    if let (true, Some(found)) = (requester, &bdf) {
        if !handover {
            c.skip(
                "address",
                "needs bus mastering, the IOMMU and huge pages above",
            );
        } else {
            match dma::Buffer::huge(found) {
                Ok(buf) => c.ok(
                    "address",
                    format!("{:#x} ({})", buf.bus_addr(), size_str(buf.len() as u64)),
                ),
                Err(why) => c.ng_err("address", &why.to_string()),
            }
        }
    }
    c.finish()
}

#[allow(clippy::too_many_arguments)]
/// The arguments of `dma-fire`, grouped so the call stays readable.
struct DmaFireArgs {
    at: u64,
    len: u64,
    seed: u32,
    expect_fault: bool,
    mps: Option<u32>,
    to_card: bool,
    pcie_addr: Option<u64>,
}

/// Where `dma-fire` points the card.
enum Dest {
    /// A huge page on this machine, which hio fills or checks.
    Page(hns_host::dma::Buffer),
    /// A bus address given with `--pcie-addr`, for `len` bytes.
    Addr { addr: u64, len: u64 },
}

impl Dest {
    fn bus_addr(&self) -> u64 {
        match self {
            Dest::Page(buffer) => buffer.bus_addr(),
            Dest::Addr { addr, .. } => *addr,
        }
    }

    fn len(&self) -> u64 {
        match self {
            Dest::Page(buffer) => buffer.len() as u64,
            Dest::Addr { len, .. } => *len,
        }
    }
}

/// Fires one descriptor.
///
/// This is the harness's own DMA engine, not the DUT's. By default it reads
/// the memory the DUT shares and writes one huge page on this machine.
///
/// The source gets a pattern first. Otherwise a page of zeros cannot tell
/// "arrived" from "nothing happened".
fn dma_fire(
    bus: &mut Bus,
    map: &RegisterMap,
    cli: &Cli,
    bundle: Option<&str>,
    args: DmaFireArgs,
) -> Result<(), String> {
    use hns_host::dma;

    let DmaFireArgs {
        at,
        len,
        seed,
        expect_fault,
        mps,
        to_card,
        pcie_addr,
    } = args;

    require_requester(map)?;
    let Some(pcie) = &map.pcie else {
        return Err(
            "regs.json has no `pcie` section, so there is no card to hand memory to.".to_string(),
        );
    };

    let bundle = match bundle {
        Some(name) => name.to_string(),
        None => {
            let memories: Vec<&str> = map
                .regions
                .iter()
                .filter(|r| r.kind.as_deref() == Some("memory"))
                .map(|r| r.name.as_str())
                .collect();
            match memories.as_slice() {
                [only] => (*only).to_string(),
                [] => return Err("this design has no memory region to read from.".to_string()),
                many => {
                    return Err(format!(
                        "say which memory to read from: {}",
                        many.join(", ")
                    ));
                }
            }
        }
    };
    let port = mem_port(map, &bundle)?;
    let entry_bytes = port.entry_bytes();
    let words_per_entry = port.words();
    if !at.is_multiple_of(entry_bytes as u64) || !len.is_multiple_of(entry_bytes as u64) {
        return Err(format!(
            "`{bundle}` holds {entry_bytes} bytes per entry.\n\
             --at and --len must both be multiples of {entry_bytes}."
        ));
    }
    if len == 0 {
        return Err(
            "--len 0 moves nothing; the gate refuses a zero-length descriptor.".to_string(),
        );
    }
    // The length must fit the length field. Extra bits are silently dropped,
    // the gate sees length 0, and its error would not show the real reason.
    let len_bits = map.get("dma_len").map(|r| r.width).unwrap_or(20);
    let most = (1u64 << len_bits) - 1;
    if len > most {
        return Err(format!(
            "{len} bytes does not fit one descriptor: its length field is {len_bits} bits, \
             so the maximum is {most} bytes.\nFire several, or ask for at most \
             {} bytes at a time.",
            most - most % entry_bytes as u64
        ));
    }
    let from = at as usize / entry_bytes;
    let entries = len as usize / entry_bytes;
    if from + entries > port.depth() {
        return Err(format!(
            "0x{at:x} + {len} bytes runs past `{bundle}`, which holds {} bytes.",
            port.depth() * entry_bytes
        ));
    }

    // The host page. With --expect-fault the card cannot reach it; its address
    // is taken only to compare with the fault address.
    let bdf = find_bdf(cli, pcie)?;
    let mut dest = match pcie_addr {
        Some(addr) => {
            if !addr.is_multiple_of(4) {
                return Err(format!(
                    "--pcie-addr 0x{addr:x} is not a multiple of 4. The engine moves whole words."
                ));
            }
            // Under translation the address would not be the one the card
            // reaches, and the RAM check below would mean nothing.
            dma::require_untranslated(&bdf).map_err(|e| e.to_string())?;
            dma::require_bus_master(&bdf).map_err(|e| e.to_string())?;
            dma::require_not_ram(addr, len).map_err(|e| e.to_string())?;
            Dest::Addr { addr, len }
        }
        None if expect_fault => {
            Dest::Page(dma::Buffer::huge_while_translating(&bdf).map_err(|e| e.to_string())?)
        }
        None => Dest::Page(dma::Buffer::huge(&bdf).map_err(|e| e.to_string())?),
    };
    if len > dest.len() {
        return Err(format!(
            "{len} bytes does not fit the page this machine hands out ({} bytes).",
            dest.len()
        ));
    }
    println!("card     {bdf}");
    match &dest {
        Dest::Page(buffer) => println!(
            "page     0x{:x} for {} bytes",
            buffer.bus_addr(),
            buffer.len()
        ),
        Dest::Addr { addr, len } => println!("address  0x{addr:x} for {len} bytes (--pcie-addr)"),
    }

    // Poison the destination before firing. Unwritten words keep the poison,
    // so "not arrived" and "wrong data" look different. The pattern goes to
    // the source; which side is which depends on the direction.
    const POISON_WORD: u32 = u32::from_le_bytes([POISON; 4]);
    if let Dest::Page(buffer) = &mut dest
        && to_card
    {
        for e in 0..entries {
            for w in 0..words_per_entry {
                let byte = (e * words_per_entry + w) * 4;
                if byte + 4 > len as usize {
                    break;
                }
                let word = pattern(seed, from + e, w).to_le_bytes();
                buffer.as_mut_slice()[byte..byte + 4].copy_from_slice(&word);
            }
        }
    } else if let Dest::Page(buffer) = &mut dest {
        buffer.as_mut_slice()[..len as usize].fill(POISON);
    }

    // Write the memory side, with the same seed and formula as `memtest`.
    for (a, b_end) in entry_groups(&port, from, from + entries) {
        let mut b = mem_batch(&port);
        set_mem_page(&mut b, map, &port, a, bus.master())?;
        if let MemPort::Indirect { maddr, .. } = &port {
            b.write(maddr.offset as u32, a as u32);
        }
        for e in a..b_end {
            for w in 0..words_per_entry {
                let value = if to_card {
                    POISON_WORD
                } else {
                    pattern(seed, e, w)
                };
                b.write(word_at(&port, e, w), value);
            }
        }
        bus.run(&b)?;
    }

    require_idle(bus, map, &bdf)?;

    // One descriptor: the page and sizes first, `go` last. A batch runs in
    // order, so one round trip is enough.
    let before = dma_counters(bus, map)?;
    let lost_before = Losses::read(bus, map)?;
    let mut b = Batch::new();
    // Write the limits every time. Keeping the last run's limit would run the
    // same command with a different TLP size.
    write_wide(&mut b, map, "dma_mps_limit", mps_limit(mps)?)?;
    write_wide(&mut b, map, "dma_mrrs_limit", mps_limit(mps)?)?;
    write_wide(&mut b, map, "dma_dir", u64::from(to_card))?;
    // The gate lets the card reach only this range.
    write_wide(&mut b, map, "dma_base", dest.bus_addr())?;
    write_wide(&mut b, map, "dma_size", dest.len())?;
    write_wide(&mut b, map, "dma_pcie_addr", dest.bus_addr())?;
    write_wide(&mut b, map, "dma_axi_addr", at)?;
    write_wide(&mut b, map, "dma_len", len)?;
    write_wide(&mut b, map, "dma_go", 1)?;
    let started = std::time::Instant::now();
    bus.run(&b)?;

    // `busy` falling means "all sent", not "all arrived". MemWr is posted, so
    // only the data shows arrival. The time step is one window round trip
    // (about 1.2us), so transfers under tens of us are not measured well.
    let deadline = started + std::time::Duration::from_secs(2);
    let mut busy = 1;
    while std::time::Instant::now() < deadline {
        busy = rd(bus, offset_of(map, "dma_busy")?)?;
        if busy == 0 {
            break;
        }
    }
    let took = started.elapsed();
    let after = dma_counters(bus, map)?;
    println!(
        "gate     done {} (+{})  out-of-range {} (+{})  while-busy {} (+{})  error {}",
        after.done,
        after.done.wrapping_sub(before.done),
        after.out_of_range,
        after.out_of_range.wrapping_sub(before.out_of_range),
        after.while_busy,
        after.while_busy.wrapping_sub(before.while_busy),
        after.error,
    );
    if let (Some(before), Some(now)) = (lost_before, Losses::read(bus, map)?) {
        for line in now.since(&before) {
            println!("lost     {line}");
        }
    }
    // Bytes per TLP, which mostly sets the speed. Read it after firing: the
    // limit is written just before, so an earlier read shows the last run.
    // Reads and writes are negotiated apart, so they use different registers.
    if to_card {
        println!(
            "request  {} bytes per read TLP",
            tlp_bytes(bus, map, "dma_mrrs")?
        );
    } else {
        println!("payload  {} bytes per TLP", tlp_bytes(bus, map, "dma_mps")?);
    }
    // Speed is why this path exists, so always print it (reading through the
    // window gives 3.06 MB/s).
    println!(
        "moved    {len} bytes in {:.0} us ({:.1} MB/s)",
        took.as_secs_f64() * 1e6,
        len as f64 / took.as_secs_f64() / 1e6,
    );
    if busy != 0 {
        return Err(
            "the engine is still busy after 2 seconds: the descriptor went in but \
             nothing came back.\nCheck that the memory answers (`hio read` the region) \
             and that the link is up."
                .to_string(),
        );
    }
    if after.out_of_range != before.out_of_range {
        return Err(format!(
            "the gate refused the descriptor: 0x{:x} + {len} bytes is not inside the range \
             it was given (0x{:x} for {} bytes).",
            dest.bus_addr(),
            dest.bus_addr(),
            dest.len()
        ));
    }

    if expect_fault {
        println!(
            "\nfired with the IOMMU still translating. The card should have been refused. \
             Look for the address it put on the bus:\n\
             \x20   sudo dmesg | tail\n\
             \x20   DMAR: [DMA {}] Request device [{bdf}] fault addr 0x{:x}\n\n\
             If that address matches the one above, the TLP header is right. Only then \
             pass the card through:\n\
             \x20   echo identity | sudo tee /sys/kernel/iommu_groups/<n>/type",
            if to_card { "Read" } else { "Write" },
            dest.bus_addr()
        );
        return Ok(());
    }
    if matches!(dest, Dest::Addr { .. }) && !to_card {
        println!(
            "landed   not checked: hio cannot read 0x{:x}. Read it from the other side.",
            dest.bus_addr()
        );
        return Ok(());
    }

    // Compare what landed, on the destination side. Toward the card, the
    // memory is read back through the window, which is slow for large sizes.
    let landed: Vec<u32> = if to_card {
        let mut got = Vec::with_capacity(entries * words_per_entry);
        for (a, b_end) in entry_groups(&port, from, from + entries) {
            let mut b = mem_batch(&port);
            set_mem_page(&mut b, map, &port, a, bus.master())?;
            if let MemPort::Indirect { maddr, .. } = &port {
                b.write(maddr.offset as u32, a as u32);
            }
            let mut hs = Vec::with_capacity((b_end - a) * words_per_entry);
            for e in a..b_end {
                for w in 0..words_per_entry {
                    hs.push(b.read(word_at(&port, e, w)));
                }
            }
            let values = bus.run(&b)?;
            got.extend(hs.into_iter().map(|h| values[h]));
        }
        got
    } else if let Dest::Page(buffer) = &dest {
        let got = buffer.as_slice();
        let (words, _) = got.as_chunks::<4>();
        words.iter().map(|w| u32::from_le_bytes(*w)).collect()
    } else {
        unreachable!("an address that is not hio's is returned above when not --to-card")
    };

    // From an address that is not hio's there is nothing to compare with.
    // Show what came back, and whether every word was written.
    if let Dest::Addr { addr, .. } = dest {
        let words = len as usize / 4;
        let untouched = landed
            .iter()
            .take(words)
            .filter(|&&word| word == POISON_WORD)
            .count();
        let first: Vec<String> = landed
            .iter()
            .take(words.min(8))
            .map(|word| format!("{word:08x}"))
            .collect();
        println!("read     {}", first.join(" "));
        if untouched == 0 {
            println!("landed   {len} bytes from 0x{addr:x} in the memory");
            return Ok(());
        }
        return Err(format!(
            "{untouched} of {words} words in the memory still hold the poison \
             (0x{POISON_WORD:08x}): no completion wrote them, or 0x{addr:x} holds that value."
        ));
    }

    let words = len as usize / 4;
    let expect = |index: usize| {
        pattern(
            seed,
            from + index / words_per_entry,
            index % words_per_entry,
        )
    };

    // Say what arrived, not only where it differs. The kind of damage (shifted,
    // doubled, not written) shows only once we know which address a wrong
    // value came from.
    const SHOW: usize = 12;
    let mut bad = 0usize;
    let mut lines: Vec<String> = Vec::new();
    let mut deltas: std::collections::BTreeMap<i64, usize> = std::collections::BTreeMap::new();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (index, &have) in landed.iter().enumerate().take(words) {
        let want = expect(index);
        if have == want {
            continue;
        }
        bad += 1;
        match runs.last_mut() {
            Some(run) if run.1 == index * 4 => run.1 = index * 4 + 4,
            _ => runs.push((index * 4, index * 4 + 4)),
        }
        // Which address the value came from. Search only nearby; a far match
        // is chance.
        let near = (index.saturating_sub(4096))..(index + 4096).min(words);
        let source = near.clone().find(|&i| i != index && expect(i) == have);
        if let Some(src) = source {
            *deltas.entry((src as i64 - index as i64) * 4).or_default() += 1;
        }
        if lines.len() < SHOW {
            let what = match source {
                Some(src) => format!(
                    "the word from byte {} ({:+} bytes)",
                    src * 4,
                    (src as i64 - index as i64) * 4
                ),
                None if have == u32::from_le_bytes([POISON; 4]) => {
                    "still the poison, so nothing was written there".to_string()
                }
                None => "not a word of this pattern at all".to_string(),
            };
            lines.push(format!(
                "  byte {:>7}: wanted 0x{want:08x}, found 0x{have:08x} -- {what}",
                index * 4
            ));
        }
    }
    if bad == 0 {
        if to_card {
            println!("landed   {len} bytes in the memory matched what the host page held");
        } else {
            println!("landed   {len} bytes matched what the memory holds");
        }
        return Ok(());
    }

    let mut report = format!("{bad} of {words} words differ.\n{}", lines.join("\n"));
    if bad > lines.len() {
        report.push_str(&format!("\n  ... and {} more", bad - lines.len()));
    }
    // Print the bad ranges. The first address and a count cannot tell "bad
    // from here on" from "bad, then good again", and that tells where it fails.
    if !runs.is_empty() {
        let shown: Vec<String> = runs
            .iter()
            .take(8)
            .map(|&(a, b)| format!("{a}..{b} ({} bytes)", b - a))
            .collect();
        report.push_str(&format!("\nbad runs: {}", shown.join(", ")));
        if runs.len() > shown.len() {
            report.push_str(&format!(" ... and {} more", runs.len() - shown.len()));
        }
    }
    // The spread of offsets shows the shape. One dominant offset means one
    // lost beat.
    if !deltas.is_empty() {
        let mut by_count: Vec<_> = deltas.into_iter().collect();
        by_count.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        let top: Vec<String> = by_count
            .iter()
            .take(4)
            .map(|(d, n)| format!("{d:+} bytes x{n}"))
            .collect();
        report.push_str(&format!("\nwhere it came from: {}", top.join(", ")));
    }
    Err(report)
}

/// Turns `--mps` into the register value. 0 means no limit.
fn mps_limit(bytes: Option<u32>) -> Result<u64, String> {
    let Some(bytes) = bytes else {
        return Ok(0);
    };
    // PCIe encodes only six sizes, doubling from 128 to 4096.
    let code = (0..6).find(|code| 128u32 << code == bytes).ok_or_else(|| {
        format!("{bytes} is not a TLP payload size. Use one of 128, 256, 512, 1024, 2048, 4096.")
    })?;
    Ok(code as u64 + 1)
}

/// Bytes per TLP the DMA engine uses now.
///
/// The register holds a code: 0 is 128 bytes, and each step doubles, up to
/// PCIe's 4096. Writes (`dma_mps`) and reads (`dma_mrrs`) are negotiated
/// apart, so the caller names the register.
fn tlp_bytes(bus: &mut Bus, map: &RegisterMap, register: &str) -> Result<u32, String> {
    let code = rd(bus, offset_of(map, register)?)?;
    Ok(128 << code.min(5))
}

/// The error for `--dma` when the window is not reached over PCIe.
fn not_over_pcie(transport: &str, command: &str) -> String {
    format!(
        "the card moves data over PCIe, and this is {transport}.\n\
         Reach the window over PCIe too:\n\n    \
         sudo \"$(command -v hio)\" -p {command} <bundle> ..."
    )
}

/// Checks that the design has a requester, before the window is opened.
/// Otherwise the error would be about the probe. Only a PCIe design with a
/// `dram` bundle has `dma_*`: the borrowed DMA engine reads 256 bits at a
/// time, and only the memory controller bus carries that width.
fn require_requester(map: &RegisterMap) -> Result<(), String> {
    if map.get("dma_base").is_ok() {
        return Ok(());
    }
    Err(
        "this design has no requester: regs.json has no `dma_*` registers.\n\
         They exist only in a design generated with `--transport pcie` whose memory is a \
         `dram` bundle."
            .to_string(),
    )
}

/// Move the data through the window, or have the card move it.
///
/// The size decides. `--pio` / `--dma` only override that choice, so the user
/// does not have to know which path is faster.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// Through the window. A `note` says why the fast path was not used.
    Window { note: Option<String> },
    /// The card moves it. With `forced`, a failed setup is an error instead
    /// of a fallback to the window.
    Card { forced: bool },
}

/// Chooses the route. It touches no hardware, so tests can run it.
///
/// `asked` is `Some(false)` for the window (`--pio`, `--threads`),
/// `Some(true)` for the card (`--dma`), and `None` to decide by size. `bytes`
/// is the amount this command moves, `above` the size where the card takes
/// over. `command` is the subcommand named in the fix of an error.
#[allow(clippy::too_many_arguments)]
fn route(
    map: &RegisterMap,
    master: &str,
    command: &str,
    bundle: &str,
    port: &MemPort,
    bytes: u64,
    above: u64,
    asked: Option<bool>,
) -> Result<Route, String> {
    let quiet = Route::Window { note: None };
    // Choose only over PCIe. Using the card over JTAG would test PCIe while
    // the user thinks they test JTAG. Speed must not swap what is tested.
    if asked == Some(true) && master != "pcie" {
        return Err(not_over_pcie(master, command));
    }
    if asked == Some(false) || master != "pcie" {
        return Ok(quiet);
    }
    let forced = asked == Some(true);
    // When the design cannot use the card, say why only if it was forced.
    // Otherwise use the window quietly; the answer never changes.
    if map.get("dma_base").is_err() {
        return if forced {
            Err(require_requester(map).unwrap_err())
        } else {
            Ok(quiet)
        };
    }
    let flat = matches!(port, MemPort::Region(_)) && port.entry_bytes() == port.words() * 4;
    if !flat {
        return if forced {
            Err(format!(
                "the requester cannot move `{bundle}`: the window does not show its bytes \
                 as the memory holds them (an indirect port, or entries that are not \
                 contiguous).\nDrop --dma for this bundle."
            ))
        } else {
            Ok(quiet)
        };
    }
    // The DMA engine is wired to one `dram` bundle only. Used for another
    // bundle, it would move that one instead: a write silently damages it,
    // and a read may "match" data an earlier run left with the same seed.
    match map.pcie.as_ref().and_then(|p| p.requester.as_deref()) {
        Some(name) if name == bundle => {}
        Some(name) => {
            return if forced {
                Err(format!(
                    "the card's requester is wired to `{name}`, not `{bundle}`. It would \
                     move `{name}` instead.\nDrop --dma for this bundle."
                ))
            } else {
                Ok(quiet)
            };
        }
        None => {
            // The user can fix this, so say it. It explains why a fast
            // command became slow.
            let why = "regs.json does not say which memory the card's requester reaches \
                       (it is older than that field).\nRegenerate it with \
                       `veryl harness gen`. The register layout is the same, so the \
                       bitstream on the card still matches.";
            return if forced {
                Err(why.to_string())
            } else {
                Ok(Route::Window {
                    note: Some(why.to_string()),
                })
            };
        }
    }
    if forced {
        Ok(Route::Card { forced: true })
    } else if bytes >= above {
        Ok(Route::Card { forced: false })
    } else {
        Ok(quiet)
    }
}

/// Opens the chosen route. If the card cannot be set up on this machine, it
/// falls back to the window, unless the card was forced.
fn open_route(
    bus: &mut Bus,
    map: &RegisterMap,
    cli: &Cli,
    route: Route,
) -> Result<Option<Mover>, String> {
    match route {
        Route::Window { note } => {
            if let Some(note) = note {
                println!(
                    "not using the card's requester: {}",
                    note.lines().next().unwrap_or(&note)
                );
            }
            Ok(None)
        }
        Route::Card { forced: true } => Mover::new(bus, map, cli).map(Some),
        // Fall back to the window: only the speed changes, not the answer.
        // Still say so, or a slow run has no explanation.
        Route::Card { forced: false } => match Mover::new(bus, map, cli) {
            Ok(mover) => Ok(Some(mover)),
            Err(why) => {
                println!(
                    "not using the card's requester: {}",
                    why.lines().next().unwrap_or(&why)
                );
                Ok(None)
            }
        },
    }
}

/// The value written to the receiving side. Unwritten bytes keep it, so "not
/// arrived" and "wrong data" look different.
const POISON: u8 = 0xa5;

/// Moves data with the card instead of the window. Reads and writes share one
/// page.
///
/// Through the window, reads top out at 1.30us per word, and posted writes
/// still take 75 s for 2GB. The card measured 947.7 MB/s to the host and
/// 1521.7 MB/s to the card.
///
/// The page and its size are written once. After that, each descriptor
/// rewrites only four registers.
struct Mover {
    buffer: hns_host::dma::Buffer,
    /// Bytes per descriptor: the smaller of the length field limit and the
    /// page size, rounded down to 4KB (the borrowed engine splits TLPs at 4KB).
    chunk: usize,
    /// The gate's refusal counts so far. Compare differences: the counters
    /// saturate and never return to 0.
    refused: u32,
    blocked: u32,
    /// Silent-loss counts when opened. The difference is reported at the end.
    losses: Option<Losses>,
}

impl Mover {
    fn new(bus: &mut Bus, map: &RegisterMap, cli: &Cli) -> Result<Mover, String> {
        require_requester(map)?;
        let Some(pcie) = &map.pcie else {
            return Err(
                "regs.json has no `pcie` section, so there is no card to move data with."
                    .to_string(),
            );
        };
        let bdf = find_bdf(cli, pcie)?;
        let buffer = hns_host::dma::Buffer::huge(&bdf).map_err(|e| e.to_string())?;
        require_idle(bus, map, &bdf)?;

        // Set the page once; the gate refuses a descriptor outside it. Reset
        // the limits too, so a value left by `dma-fire --mps` is not used.
        let mut b = Batch::new();
        write_wide(&mut b, map, "dma_mps_limit", 0)?;
        write_wide(&mut b, map, "dma_mrrs_limit", 0)?;
        write_wide(&mut b, map, "dma_base", buffer.bus_addr())?;
        write_wide(&mut b, map, "dma_size", buffer.len() as u64)?;
        bus.run(&b)?;
        let counters = dma_counters(bus, map)?;
        let losses = Losses::read(bus, map)?;

        let len_bits = map.get("dma_len").map(|r| r.width).unwrap_or(20);
        let most = ((1u64 << len_bits) - 1) as usize;
        let chunk = most.min(buffer.len()) & !0xfff;
        Ok(Mover {
            buffer,
            chunk,
            refused: counters.out_of_range,
            blocked: counters.while_busy,
            losses,
        })
    }

    /// Reports what was silently lost since opening. It reports even when the
    /// data matched: then the loss only hit a range that was not tested.
    fn report_losses(&self, bus: &mut Bus, map: &RegisterMap) -> Result<(), String> {
        let (Some(before), Some(now)) = (self.losses, Losses::read(bus, map)?) else {
            return Ok(());
        };
        for line in now.since(&before) {
            println!("lost on the way: {line}");
        }
        Ok(())
    }

    /// Moves `len` bytes from memory offset `at` to the host and returns them.
    fn fetch(
        &mut self,
        bus: &mut Bus,
        map: &RegisterMap,
        at: u64,
        len: usize,
    ) -> Result<&[u8], String> {
        // Poison first. Data left by the last `send` would look like a good
        // read even if nothing arrived, and in a write-then-read test it is
        // exactly the expected value.
        self.buffer.as_mut_slice()[..len].fill(POISON);
        self.fire(bus, map, at, len, false)?;
        Ok(&self.buffer.as_slice()[..len])
    }

    /// Moves the `len` bytes that `fill` writes to memory offset `at`.
    fn send(
        &mut self,
        bus: &mut Bus,
        map: &RegisterMap,
        at: u64,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<(), String> {
        fill(&mut self.buffer.as_mut_slice()[..len]);
        self.fire(bus, map, at, len, true)
    }

    /// Fires one descriptor and waits until it finishes.
    fn fire(
        &mut self,
        bus: &mut Bus,
        map: &RegisterMap,
        at: u64,
        len: usize,
        to_card: bool,
    ) -> Result<(), String> {
        use std::sync::atomic::{Ordering, fence};

        let (dir, way) = if to_card { (1, "to") } else { (0, "from") };
        // The page writes must be visible before `go`. They are plain memory
        // and `go` is volatile MMIO, so nothing else orders them.
        fence(Ordering::SeqCst);
        let mut b = Batch::new();
        // Write the direction every time; reads and writes share the page.
        write_wide(&mut b, map, "dma_dir", dir)?;
        write_wide(&mut b, map, "dma_pcie_addr", self.buffer.bus_addr())?;
        write_wide(&mut b, map, "dma_axi_addr", at)?;
        write_wide(&mut b, map, "dma_len", len as u64)?;
        write_wide(&mut b, map, "dma_go", 1)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        bus.run(&b)?;
        // PCIe ordering: the read of `busy` cannot pass earlier MemWr. Once
        // `busy` reads 0, the writes to the host have landed.
        loop {
            if rd(bus, offset_of(map, "dma_busy")?)? == 0 {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err(format!(
                    "the engine did not finish moving {len} bytes {way} 0x{at:x} within 5 \
                     seconds.\nCheck that the memory answers (`hio read` the region) and \
                     that the link is up."
                ));
            }
        }
        fence(Ordering::SeqCst);

        // "Not sent" is not "not arrived". `hns::dma_gate` counts refusals;
        // without this check they would only show as a full mismatch.
        let after = dma_counters(bus, map)?;
        if after.out_of_range != self.refused {
            self.refused = after.out_of_range;
            return Err(format!(
                "the gate refused a descriptor: 0x{at:x} + {len} bytes against a page of {} \
                 bytes at 0x{:x}.",
                self.buffer.len(),
                self.buffer.bus_addr()
            ));
        }
        if after.while_busy != self.blocked {
            self.blocked = after.while_busy;
            return Err(
                "the gate was still busy with an earlier descriptor, so it did not take this \
                 one.\nAnother `hio` may be driving the card. If not, reprogram it to clear \
                 the engine."
                    .to_string(),
            );
        }
        // `error` is the status of the last descriptor; success sets it to 0.
        if after.error != 0 {
            return Err(format!(
                "the engine reported error {} ({}) moving {len} bytes {way} 0x{at:x}.\n\
                 `hio check` shows the host side. `hio read` the region to see whether \
                 the memory answers.",
                after.error,
                engine_error(to_card, after.error)
            ));
        }
        Ok(())
    }
}

/// Names an error code from the borrowed DMA engine.
///
/// The table depends on the direction. The read side (`pcie_us_axi_dma_rd.v`)
/// and the write side (`pcie_us_axi_dma_wr.v`) number codes differently: 10
/// is a completion UR on one and an AXI write SLVERR on the other.
fn engine_error(to_card: bool, code: u32) -> &'static str {
    if to_card {
        match code {
            0 => "none",
            1 => "timeout",
            2 => "parity",
            4 => "AXI read SLVERR",
            5 => "AXI read DECERR",
            6 => "AXI write SLVERR",
            7 => "AXI write DECERR",
            8 => "function level reset",
            9 => "completion poisoned",
            10 => "completion status UR: the host refused the read",
            11 => "completion status CA",
            _ => "unknown",
        }
    } else {
        match code {
            0 => "none",
            1 => "parity",
            2 => "completion poisoned",
            3 => "completion status UR",
            4 => "completion status CRS",
            5 => "completion status CA",
            6 => "function level reset",
            8 => "AXI read SLVERR",
            9 => "AXI read DECERR",
            10 => "AXI write SLVERR",
            11 => "AXI write DECERR",
            15 => "timeout",
            _ => "unknown",
        }
    }
}

/// Checks that the DMA engine is idle, before firing.
///
/// If an earlier descriptor never finished, `busy` stays high and the gate
/// ignores the next `go` (only `blocked` grows). Firing without bus mastering
/// causes this, and turning bus mastering on later does not clear it: the
/// request was lost, and only reprogramming helps (seen on VCU118).
fn require_idle(bus: &mut Bus, map: &RegisterMap, bdf: &str) -> Result<(), String> {
    if rd(bus, offset_of(map, "dma_busy")?)? == 0 {
        return Ok(());
    }
    let done = rd(bus, offset_of(map, "dma_done")?)?;
    Err(format!(
        "the engine is still busy with an earlier descriptor that never finished ({done} \
         finished before it), so it will not take another.\n\
         This happens when the card fired without bus mastering. Turning bus mastering on \
         later does not clear it. Reprogram the card. The rescan turns decoding, bus \
         mastering and the IOMMU setting back to their defaults, so finish with `check`:\n\n\
         \x20   echo 1 | sudo tee /sys/bus/pci/devices/{bdf}/remove\n\
         \x20   sudo \"$(command -v hio)\" program\n\
         \x20   echo 1 | sudo tee /sys/bus/pci/rescan\n\
         \x20   echo 1 | sudo tee /sys/bus/pci/devices/{bdf}/enable\n\
         \x20   sudo setpci -s {bdf} COMMAND=0x4:0x4\n\
         \x20   sudo \"$(command -v hio)\" -p check"
    ))
}

/// Counts of silent losses on the way. All are saturating counters, so
/// compare differences.
///
/// - `rq_drops`: RQ TLPs the hard block dropped. A TLP whose `tvalid` falls
///   in the middle is nullified, and nothing reaches the host (PG213).
/// - `rc_cor` / `rc_uncor`: completions the borrowed read engine discarded.
///   Those bytes are not written to memory.
///
/// An older `regs.json` has none of these, and then nothing is counted.
///
/// `dma_rq_gaps` (TLPs with a gap inside) is left out. On VCU118 nearly every
/// TLP has one, and `hns::tlp_hold` buffers it, so it is normal, not a loss.
/// `dump` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Losses {
    rq_drops: u32,
    rc_cor: u32,
    rc_uncor: u32,
}

impl Losses {
    fn read(bus: &mut Bus, map: &RegisterMap) -> Result<Option<Losses>, String> {
        if map.get("dma_rq_drops").is_err() {
            return Ok(None);
        }
        Ok(Some(Losses {
            rq_drops: rd(bus, offset_of(map, "dma_rq_drops")?)?,
            rc_cor: rd(bus, offset_of(map, "dma_rc_cor")?)?,
            rc_uncor: rd(bus, offset_of(map, "dma_rc_uncor")?)?,
        }))
    }

    /// Describes each count that grew since `before`. Empty if none grew.
    fn since(&self, before: &Losses) -> Vec<String> {
        let mut out = Vec::new();
        let rq = self.rq_drops.wrapping_sub(before.rq_drops);
        if rq != 0 {
            out.push(format!(
                "the PCIe block dropped {rq} request TLP(s) (dma_rq_drops +{rq})"
            ));
        }
        let cor = self.rc_cor.wrapping_sub(before.rc_cor);
        if cor != 0 {
            out.push(format!(
                "the read engine discarded {cor} completion(s) as unexpected, poisoned or \
                 failed (dma_rc_cor +{cor})"
            ));
        }
        let uncor = self.rc_uncor.wrapping_sub(before.rc_uncor);
        if uncor != 0 {
            out.push(format!(
                "the read engine discarded {uncor} completion(s) as malformed or timed out \
                 (dma_rc_uncor +{uncor})"
            ));
        }
        out
    }
}

/// The gate's counters. "Not sent" and "sent but failed" are counted apart.
struct DmaCounters {
    done: u32,
    out_of_range: u32,
    while_busy: u32,
    error: u32,
}

fn dma_counters(bus: &mut Bus, map: &RegisterMap) -> Result<DmaCounters, String> {
    Ok(DmaCounters {
        done: rd(bus, offset_of(map, "dma_done")?)?,
        out_of_range: rd(bus, offset_of(map, "dma_oor")?)?,
        while_busy: rd(bus, offset_of(map, "dma_blocked")?)?,
        error: rd(bus, offset_of(map, "dma_error")?)?,
    })
}

/// Queues a write to a register wider than 32 bits: lowest word first, as
/// many words as the map says.
fn write_wide(b: &mut Batch, map: &RegisterMap, name: &str, value: u64) -> Result<(), String> {
    let offset = offset_of(map, name)?;
    for (w, word) in wide_words(value, words_of(map, name))
        .into_iter()
        .enumerate()
    {
        b.write(offset + 4 * w as u32, word);
    }
    Ok(())
}

/// Splits a wide value into words. Word `i` is bits `[32i+31 : 32i]`. This
/// order is fixed by `regs.json` and matches the decode in the RTL.
fn wide_words(value: u64, words: usize) -> Vec<u32> {
    (0..words).map(|w| (value >> (32 * w)) as u32).collect()
}

/// The path to the window. A `Batch` only says what to do at which address;
/// this runs it. The commands above (`load` / `memtest` / `dump`) see only
/// this enum.
enum Bus {
    Jtag(Bridge<Mpsse<Ftdi>>),
    Pcie(hns_host::pcie::Bar),
    /// An HNS2 server: `veryl harness sim`, or `hio serve` with a board.
    Net(hns_host::hns2::Link),
}

impl Bus {
    fn run(&mut self, batch: &Batch) -> Result<Reads, String> {
        match self {
            Bus::Jtag(bridge) => bridge.run(batch).map_err(|e| e.to_string()),
            Bus::Pcie(bar) => bar.run(batch).map_err(|e| e.to_string()),
            Bus::Net(link) => link.run(batch).map_err(|e| e.to_string()),
        }
    }

    /// The master this bus is. A moving window has one base register per
    /// master. With a shared one, a move by one master would make the other
    /// read a different address.
    fn master(&self) -> &'static str {
        match self {
            Bus::Jtag(_) => "jtag",
            Bus::Pcie(_) => "pcie",
            // The simulator drives the AXI4-Lite port JTAG drives on a board.
            Bus::Net(link) if link.info().master == "pcie" => "pcie",
            Bus::Net(_) => "jtag",
        }
    }

    fn read32(&mut self, addr: u32) -> Result<u32, String> {
        match self {
            Bus::Jtag(bridge) => bridge.read32(addr).map_err(|e| e.to_string()),
            Bus::Pcie(bar) => bar.read32(addr).map_err(|e| e.to_string()),
            Bus::Net(link) => link.read32(addr).map_err(|e| e.to_string()),
        }
    }

    fn read_burst(&mut self, addr: u32, out: &mut [u32]) -> Result<(), String> {
        match self {
            Bus::Jtag(bridge) => bridge.read_burst(addr, out).map_err(|e| e.to_string()),
            Bus::Pcie(bar) => bar.read_burst(addr, out).map_err(|e| e.to_string()),
            Bus::Net(link) => link.read_burst(addr, out).map_err(|e| e.to_string()),
        }
    }

    /// Only PCIe reads in parallel. JTAG is one cable, so more threads only
    /// wait longer; there, batching (`Batch`) already cuts round trips.
    fn read_parallel(&mut self, addr: u32, out: &mut [u32], threads: usize) -> Result<(), String> {
        match self {
            Bus::Jtag(bridge) => bridge.read_burst(addr, out).map_err(|e| e.to_string()),
            Bus::Pcie(bar) => bar
                .read_parallel(addr, out, threads)
                .map_err(|e| e.to_string()),
            Bus::Net(link) => link.read_burst(addr, out).map_err(|e| e.to_string()),
        }
    }
}

fn rd(bus: &mut Bus, addr: u32) -> Result<u32, String> {
    bus.read32(addr)
}

/// Puts the DUT into reset or takes it out.
///
/// After the write, it reads until the state changes. Host timing is not
/// used: the hardware keeps the minimum assert width. The DUT enters reset
/// after the fence closes, and leaves it after everything downstream drains.
fn reset(bus: &mut Bus, map: &RegisterMap, hold: bool, release: bool) -> Result<(), String> {
    let (Ok(req), Ok(state)) = (map.get("dut_reset"), map.get("dut_reset_state")) else {
        return Err(
            "this design has no DUT reset: regs.json carries no `dut_reset`.\n\
                    Regenerate it with this veryl-harness and synthesize again."
                .to_string(),
        );
    };
    let (req, state) = (req.offset as u32, state.offset as u32);
    // Out of reset the hart runs, so an instruction trace records again. If
    // it halts at reset instead, `hio cpu` pauses the trace before it acts.
    let pauses = trace_pauses(map);

    let mut set = |on: bool| -> Result<(), String> {
        let mut b = Batch::new();
        if !on {
            for pause in &pauses {
                b.write(*pause, 0);
            }
        }
        b.write(req, u32::from(on));
        bus.run(&b)?;
        // One read takes about 1ms over JTAG and microseconds over PCIe.
        // After 2 s, something is stuck on either transport.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if rd(bus, state)? & 1 == u32::from(on) {
                return Ok(());
            }
        }
        Err(if on {
            "the DUT did not enter reset within 2 s.\n\
             The harness waits for the DUT to finish an AXI handshake it has started. \
             If the memory is not calibrated, that never happens: run `hio check`."
                .to_string()
        } else {
            "the DUT did not leave reset within 2 s.\n\
             The harness waits until the memory has answered everything the DUT sent. \
             Run `hio check` to see if the memory is calibrated."
                .to_string()
        })
    };

    if release {
        set(false)?;
        println!("ok: the DUT is running");
    } else if hold {
        set(true)?;
        println!("ok: the DUT is held in reset. Release it with: hio reset --release");
    } else {
        set(true)?;
        set(false)?;
        println!("ok: the DUT was reset");
    }
    Ok(())
}

/// Checks that the board runs the local register map.
///
/// The heartbeat UART prints the same values without any transport, so this
/// is a second, independent check.
fn id(bus: &mut Bus, map: &RegisterMap) -> Result<(), String> {
    let (magic, hash) = read_identity(bus, map)?;
    println!("magic    {magic:#010x} (expected {:#010x})", map.magic);
    println!("map hash {hash:#010x} (expected {:#010x})", map.map_hash);
    println!("dut      {}", map.dut);
    check_identity_values(magic, hash, map)?;
    report_timeouts(bus, map)?;
    let what = if matches!(bus, Bus::Net(link) if link.info().kind == "sim") {
        "the simulation"
    } else {
        "the board"
    };
    println!("ok: {what} is running this register map");
    Ok(())
}

/// Shows how many times the window answered on its own.
///
/// The window does not wait forever for a terminator. After a limit it
/// returns `SLVERR` and 0, and counts it here. Over PCIe that `SLVERR` never
/// reaches the host (the borrowed CQ/CC bridge does not convert `rresp`), so
/// this count is the only way to tell whether a read value is real.
///
/// The count is w1c: writing a nonzero value clears it. If it is not cleared,
/// every later read shows nonzero, and a new timeout cannot be seen.
///
/// Older maps do not have it; then this does nothing.
fn report_timeouts(bus: &mut Bus, map: &RegisterMap) -> Result<(), String> {
    if map.get("harness_timeout").is_err() {
        return Ok(());
    }
    let n = rd(bus, offset_of(map, "harness_timeout")?)?;
    println!("timeouts {n}");
    if n != 0 {
        println!(
            "  the window answered on its own {n} time(s): a terminator did not answer \
             in time, so the window returned 0.\n  \
             With `dram`, read <bundle>_calib first. The controller needs tens of ms to \
             calibrate and refuses the bus until then.\n  \
             After reading it, clear the count with `hio write harness_timeout 1`, so the \
             next one shows as new."
        );
    }
    Ok(())
}

fn read_identity(bus: &mut Bus, map: &RegisterMap) -> Result<(u32, u32), String> {
    let mut b = Batch::new();
    let m = b.read(offset_of(map, "harness_magic")?);
    let h = b.read(offset_of(map, "harness_map_hash")?);
    let got = bus.run(&b)?;
    Ok((got[m], got[h]))
}

fn check_identity(bus: &mut Bus, map: &RegisterMap) -> Result<(), String> {
    let (magic, hash) = read_identity(bus, map)?;
    check_identity_values(magic, hash, map)
}

/// Compares the magic and the map hash with the map.
///
/// A magic with only some bits wrong means TCK is too fast. TDO must return
/// within half a cycle, and the board sets that limit, not the chip. The
/// same bits fail every time, so the same wrong value comes back.
fn check_identity_values(magic: u32, hash: u32, map: &RegisterMap) -> Result<(), String> {
    if magic != map.magic {
        return Err(format!(
            "the window answered with magic {magic:#010x}, not {:#010x}.\n\
             The board does not run a veryl-harness design, or the transport does not \
             reach its window.\n\
             If the board was just powered on, or runs another design (from flash or an \
             SD card), run `hio program` first.\n\
             If every bit is 0 or every bit is 1, suspect the JTAG chain.\n\
             If only some bits are wrong, --tck-hz is probably too fast for the cable. \
             Lower it and try again.\n\
             If the USER slot is the default, --user1-ir may be wrong: the wrong slot \
             reaches BYPASS, not the window.",
            map.magic
        ));
    }
    if hash != map.map_hash {
        return Err(format!(
            "the bitstream reports map hash {hash:#010x}, but {} says {:#010x}.\n\
             The board runs a harness built from a different register map. Re-generate \
             and re-synthesize, or point --regs at the map that was built.",
            "regs.json", map.map_hash
        ));
    }
    Ok(())
}

/// Resolves a register name or a byte offset.
fn offset_of(map: &RegisterMap, who: &str) -> Result<u32, String> {
    if who.starts_with("0x") || who.starts_with("0X") {
        let offset = parse_u64(who)?;
        // Refuse an offset outside the window before sending it. The bridge
        // carries only the window's address bits, so a larger offset silently
        // wraps (on the board, `0x1018` returned the magic at `0x18`). The RTL
        // cannot detect this. Likewise, the window reads whole words, so an
        // unaligned offset silently becomes the word below it.
        if offset % 4 != 0 {
            return Err(format!(
                "0x{offset:x} is not on a word boundary.\nThe window is read one \
                 32-bit word at a time. Use 0x{:x} for the word it falls in.",
                offset & !3
            ));
        }
        if offset + 4 > map.size_bytes as u64 {
            return Err(format!(
                "0x{offset:x} is past the end of the window (0x0 .. 0x{:x}).\nThe \
                 bridge carries {} address bits, so it would wrap around to another \
                 register without an error. `dump` lists what is there.",
                map.size_bytes - 4,
                map.addr_bits()
            ));
        }
        return Ok(offset as u32);
    }
    map.get(who)
        .map(|r| r.offset as u32)
        .map_err(|e| e.to_string())
}

fn words_of(map: &RegisterMap, who: &str) -> usize {
    map.get(who).map(|r| r.words).unwrap_or(1)
}

/// The body of `read`. Both the command line and `run` call it, so a script
/// behaves exactly like the command line.
fn do_read(
    bus: &mut Bus,
    map: &RegisterMap,
    who: &str,
    at: Option<u64>,
    words: Option<usize>,
) -> Result<(), String> {
    // Regions and registers share one namespace, so a name is one or the other.
    if let Some(region) = map.region(who) {
        let at = at.ok_or_else(|| {
            format!(
                "`{who}` is a region, so it needs an offset: `hio read {who} 0x0 [words]`.\n\
                     It covers {} bytes ({} entries of {}). `dump` lists registers, \
                     not regions.",
                region.depth as usize * region.entry_bytes,
                region.depth,
                region.entry_bytes
            )
        })?;
        let count = words.unwrap_or(1);
        let words = read_region(bus, map, region, at, count)?;
        // N words of a region are N values, not one wide value. Joined like a
        // register they read as `0xc0de0003c0de0002...`. Print a hexdump
        // instead: 16 bytes per line, split every 4 bytes. A single word is
        // printed as a plain value, like a register, so it can be passed on.
        if count == 1 {
            println!("{}", show(&words));
        } else {
            for (i, line) in words.chunks(4).enumerate() {
                let cells: Vec<String> = line.iter().map(|word| format!("{word:08x}")).collect();
                println!("{:#06x}  {}", at as usize + 16 * i, cells.join(" "));
            }
        }
        return Ok(());
    }
    if at.is_some() {
        return Err(format!(
            "`{who}` is a register, so it takes no offset.\n\
                 Only regions take one. This map has {}.",
            region_list(map)
        ));
    }
    let words = read_reg(bus, map, who)?;
    println!("{}", show(&words));
    Ok(())
}

/// The body of `write`. Shared the same way as `do_read`.
fn do_write(bus: &mut Bus, map: &RegisterMap, who: &str, values: &[u64]) -> Result<(), String> {
    if let Some(region) = map.region(who) {
        let (at, rest) = values.split_first().expect("clap requires one value");
        if rest.is_empty() {
            return Err(format!(
                "`{who}` is a region, so a write needs an offset and at least one word: \
                     `hio write {who} 0x0 0xdeadbeef`.\n\
                     Only `0x{at:x}` was given. hio does not guess whether it is the offset \
                     or the word."
            ));
        }
        return write_region(bus, map, region, *at, rest);
    }
    write_reg(bus, map, who, values)
}

/// Lists the region names, so an error can say what does exist.
fn region_list(map: &RegisterMap) -> String {
    if map.regions.is_empty() {
        "none".to_string()
    } else {
        map.regions
            .iter()
            .map(|region| region.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Splits a byte range of a region into moving-window pages.
///
/// The window shows only `size_bytes` at a time, so a longer request is split,
/// and `base` is set again for each part. A fixed window gives one part.
fn region_pages(
    region: &hns_regs::Region,
    at: u64,
    words: usize,
) -> Result<Vec<(u64, u32, usize)>, String> {
    let span = region.size_bytes as u64;
    let mut out = Vec::new();
    let mut left = words.max(1);
    let mut at = at;
    while left > 0 {
        let page = at / span;
        let off = at % span;
        let here = (((span - off) / 4) as usize).min(left);
        out.push((page, (region.window_base as u64 + off) as u32, here));
        at += 4 * here as u64;
        left -= here;
    }
    Ok(out)
}

/// Sets `base` for a moving window. Without it, the last page is read.
fn set_page(
    b: &mut Batch,
    map: &RegisterMap,
    region: &hns_regs::Region,
    page: u64,
    master: &str,
) -> Result<(), String> {
    if !region.is_aperture() {
        return Ok(());
    }
    let at = offset_of(map, &region.base_register(master))?;
    b.write(at, page as u32);
    Ok(())
}

/// Checks that an offset in a region is backed by real memory.
///
/// The limit comes from the entry count, not the window size (`size_bytes`).
/// A region is rounded up to a power of two. Past the entries, a read returns
/// 0 and bumps `<bundle>_oor`, and that 0 must not look like data.
fn region_span(region: &hns_regs::Region, at: u64, words: usize) -> Result<u32, String> {
    let bytes = region.reach_bytes() as usize;
    if !at.is_multiple_of(4) {
        return Err(format!(
            "0x{at:x} is not on a word boundary.\n\
             A region is read one 32-bit word at a time. Use 0x{:x} for the word it falls in.",
            at & !3
        ));
    }
    let end = at as usize + 4 * words.max(1);
    if end > bytes {
        return Err(format!(
            "0x{at:x} + {words} word(s) runs past `{}`, which holds {bytes} bytes \
             ({} entries of {}).\n\
             Past the end, the memory returns 0 and counts the access in \
             `{}_oor`, so the value would look like data.",
            region.name, region.depth, region.entry_bytes, region.name
        ));
    }
    Ok((region.window_base + at as usize) as u32)
}

/// A batch for a region.
///
/// For harness memory, it marks the batch replayable after a loss. Doing a
/// read or write twice gives the same result there. A DUT slave interface is
/// not marked, because a read alone may change its state.
fn region_batch(region: &hns_regs::Region) -> Batch {
    let mut b = Batch::new();
    if region.is_idempotent() {
        b.replayable();
    }
    b
}

/// A batch for one memory. Replayable only for a region.
///
/// An indirect port is not: each write to `mdata` advances the address by
/// one, so a second write lands in another entry.
fn mem_batch(port: &MemPort<'_>) -> Batch {
    let mut b = Batch::new();
    if let MemPort::Region(region) = port
        && region.is_idempotent()
    {
        b.replayable();
    }
    b
}

/// Makes sure a region can answer before it is touched. Otherwise the window
/// waits out its timeout and answers with an error, which the JTAG bridge
/// keeps until the FPGA is configured again.
///
/// - A `dram` answers only once its controller has calibrated, which takes
///   a while after configuration. It is waited for, then refused.
/// - A slave of the DUT is not driven while the DUT is held in reset.
fn not_held(bus: &mut Bus, map: &RegisterMap, region: &hns_regs::Region) -> Result<(), String> {
    calibrated(bus, map, &region.name)?;
    if !region.held_in_dut_reset {
        return Ok(());
    }
    let Ok(state) = map.get("dut_reset_state") else {
        return Ok(());
    };
    if rd(bus, state.offset as u32)? & 1 == 1 {
        return Err(format!(
            "the DUT is held in reset, and `{}` is a slave inside it: the harness does not \
             drive it until the DUT runs again. The access was not made.\n\
             Release the DUT first: hio reset --release",
            region.name
        ));
    }
    Ok(())
}

/// How long a `dram` is given to calibrate. The VCU118's DDR4 was done within
/// 2 s of `program`; the Arty's DDR3 before `program` returned.
const CALIBRATION_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Waits until the memory controller behind `bundle` has calibrated, if the
/// bundle has one (`<bundle>_calib`).
fn calibrated(bus: &mut Bus, map: &RegisterMap, bundle: &str) -> Result<(), String> {
    let Ok(calib) = map.get(&format!("{bundle}_calib")) else {
        return Ok(());
    };
    let offset = calib.offset as u32;
    let start = std::time::Instant::now();
    loop {
        if rd(bus, offset)? & 1 == 1 {
            return Ok(());
        }
        if start.elapsed() > CALIBRATION_WAIT && map_uses_ps7(map) {
            return Err(format!(
                "`{bundle}` is not ready ({bundle}_calib is 0): its memory is the Zynq PS's, \
                 and the PS is not set up. The access was not made.\n\
                 The PS is set up by software after every power-up. `hio program` does it \
                 after the bitstream; otherwise run `hio dap ps7-init <ps7_init.tcl>` \
                 (the file is beside the bitstream)."
            ));
        }
        if start.elapsed() > CALIBRATION_WAIT {
            return Err(format!(
                "`{bundle}` is not calibrated yet ({bundle}_calib is 0 after {} s): its memory \
                 controller answers nothing until it is. The access was not made.\n\
                 Calibration runs once after configuration. If it never ends, check the \
                 memory clock and the board reset.",
                CALIBRATION_WAIT.as_secs()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Whether the map's target reaches its memory through a Zynq PS.
fn map_uses_ps7(map: &RegisterMap) -> bool {
    map.target
        .as_deref()
        .and_then(|name| hns_targets::resolve(name, &[]).ok())
        .as_ref()
        .and_then(hns_targets::dram)
        .is_some_and(|d| d.is_ps7())
}

/// Reads a region, one round trip per window page.
fn read_region(
    bus: &mut Bus,
    map: &RegisterMap,
    region: &hns_regs::Region,
    at: u64,
    words: usize,
) -> Result<Vec<u32>, String> {
    not_held(bus, map, region)?;
    let master = bus.master();
    region_span(region, at, words)?;
    let mut out = Vec::new();
    for (page, addr, count) in region_pages(region, at, words)? {
        let mut b = region_batch(region);
        set_page(&mut b, map, region, page, master)?;
        let handles: Vec<_> = (0..count).map(|i| b.read(addr + 4 * i as u32)).collect();
        let got = bus.run(&b)?;
        out.extend(handles.into_iter().map(|h| got[h]));
    }
    Ok(out)
}

/// Writes a region, one round trip per window page.
fn write_region(
    bus: &mut Bus,
    map: &RegisterMap,
    region: &hns_regs::Region,
    at: u64,
    values: &[u64],
) -> Result<(), String> {
    not_held(bus, map, region)?;
    let master = bus.master();
    region_span(region, at, values.len())?;
    let mut done = 0usize;
    for (page, addr, count) in region_pages(region, at, values.len())? {
        let mut b = region_batch(region);
        set_page(&mut b, map, region, page, master)?;
        for i in 0..count {
            b.write(addr + 4 * i as u32, values[done + i] as u32);
        }
        bus.run(&b)?;
        done += count;
    }
    Ok(())
}

/// Reads a register in one round trip, however many words it has.
fn read_reg(bus: &mut Bus, map: &RegisterMap, who: &str) -> Result<Vec<u32>, String> {
    let base = offset_of(map, who)?;
    let mut out = vec![0u32; words_of(map, who)];
    bus.read_burst(base, &mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Writes a register in one round trip. This is about order, not only speed:
/// `mdata` is written lowest word first and takes its value when the highest
/// word is written. One batch keeps that order.
fn write_reg(bus: &mut Bus, map: &RegisterMap, who: &str, values: &[u64]) -> Result<(), String> {
    if let Ok(r) = map.get(who)
        && r.access == Access::Ro
    {
        return Err(format!(
            "`{who}` is read-only.\n\
             It is a DUT output. Only `rw` registers (DUT inputs) can be written."
        ));
    }
    let base = offset_of(map, who)?;
    let words = words_of(map, who);

    // A single value is split across up to two words.
    let plan: Vec<u32> = if values.len() == 1 && words <= 2 {
        (0..words).map(|i| (values[0] >> (32 * i)) as u32).collect()
    } else {
        if map.get(who).is_ok() && values.len() != words {
            return Err(format!(
                "`{who}` is {words} words wide, but {} value(s) were given.\n\
                 Pass one value per word, lowest first. A wide register takes its value \
                 when its highest word is written, so a partial write is refused.",
                values.len()
            ));
        }
        values.iter().map(|&v| v as u32).collect()
    };

    let mut b = Batch::new();
    for (i, &w) in plan.iter().enumerate() {
        b.write(base + 4 * i as u32, w);
    }
    bus.run(&b)?;
    Ok(())
}

/// Formats register words as one hex value, highest word first.
fn show(words: &[u32]) -> String {
    if words.len() == 1 {
        return format!("{:#010x}", words[0]);
    }
    let mut s = String::from("0x");
    for w in words.iter().rev() {
        s.push_str(&format!("{w:08x}"));
    }
    s
}

/// Prints a rate. One word is 4 bytes.
fn rate(what: &str, done: usize, total: std::time::Duration) {
    let secs = total.as_secs_f64();
    println!("{done} {what}s in {secs:.3} s");
    println!(
        "  {:.2} us per {what}",
        total.as_secs_f64() / done as f64 * 1e6
    );
    println!("  {:.0} {what}s/s", done as f64 / secs);
    println!("  {:.2} MB/s", done as f64 * 4.0 / secs / 1e6);
}

/// Reads the window over and over to measure the transport.
///
/// Without a region it reads only `harness_magic`. Its value is known, so
/// corruption shows too, and the DUT is not touched.
///
/// Measuring writes needs a safe target. `harness_magic` is read-only, and a
/// `rw` register is a DUT input, so writing it drives the DUT. Only a region
/// is safe, and its contents are lost, as with `memtest`.
#[allow(clippy::too_many_arguments)]
fn bench(
    bus: &mut Bus,
    map: &RegisterMap,
    bundle: Option<&str>,
    reads: usize,
    repeat: usize,
    both: bool,
    base: Option<u64>,
    size: Option<u64>,
    threads: usize,
) -> Result<(), String> {
    if reads == 0 || repeat == 0 {
        return Err("--reads and --repeat must both be at least 1".into());
    }
    if threads == 0 {
        return Err("--threads has to be at least 1".into());
    }
    // Without a region, one word is read again and again; nothing to split.
    if threads > 1 && bundle.is_none() {
        return Err(format!(
            "--threads sweeps a region, so it needs one named.\n\
             Without one, `bench` only reads `harness_magic`.\n\n\
             Regions here: {}.",
            region_list(map)
        ));
    }
    // Refuse options that would have no effect. A range and writes only
    // apply inside a region.
    if bundle.is_none() && (both || base.is_some() || size.is_some()) {
        return Err(format!(
            "`--both`, `--base` and `--size` work on a region, so name one:\n\n\
             \x20   hio bench <region> --both\n\n\
             Regions here: {}.",
            region_list(map)
        ));
    }
    if let Some(bundle) = bundle {
        let region = map.region(bundle).ok_or_else(|| {
            format!(
                "`{bundle}` is not a region, and only a region can be swept.\n\
                 Writing a `rw` register would drive the DUT, not measure the link.\n\n\
                 Regions here: {}.",
                region_list(map)
            )
        })?;
        return bench_region(
            bus,
            map,
            region,
            repeat,
            base.unwrap_or(0),
            size,
            both,
            threads,
        );
    }
    let at = offset_of(map, "harness_magic")?;
    let want = map.magic;

    // Discard one run, so the first-time USB and driver cost is not measured.
    {
        let mut b = Batch::new();
        b.replayable();
        for _ in 0..reads.min(16) {
            b.read(at);
        }
        bus.run(&b)?;
    }

    let mut total = std::time::Duration::ZERO;
    let mut done = 0usize;
    for _ in 0..repeat {
        let mut b = Batch::new();
        b.replayable();
        let handles: Vec<_> = (0..reads).map(|_| b.read(at)).collect();
        let start = std::time::Instant::now();
        let got = bus.run(&b)?;
        total += start.elapsed();
        // Check the data too. A fast rate means nothing if the data is corrupt.
        for h in handles {
            if got[h] != want {
                return Err(format!(
                    "the window returned 0x{:08x} where 0x{want:08x} was expected.\n\
                     The transfer is corrupt, so the rate means nothing. Lower --tck-hz \
                     and try again.",
                    got[h]
                ));
            }
        }
        done += reads;
    }

    rate("read", done, total);
    Ok(())
}

/// Writes a region and reads it back. Writes and reads are reported apart:
/// over PCIe a write is posted and does not wait, and a read is a round trip.
#[allow(clippy::too_many_arguments)]
fn bench_region(
    bus: &mut Bus,
    map: &RegisterMap,
    region: &hns_regs::Region,
    repeat: usize,
    at: u64,
    size: Option<u64>,
    both: bool,
    threads: usize,
) -> Result<(), String> {
    not_held(bus, map, region)?;
    let master = bus.master();
    // Count only the entries. Past them a read returns 0 and bumps `oor`,
    // which says nothing about the transport.
    let bytes = region.depth as usize * region.entry_bytes;
    // For a moving window, default to one page. The whole memory would always
    // be refused below for crossing a page.
    let page = (region.size_bytes as u64) - (at % region.size_bytes as u64);
    let all = (bytes as u64).saturating_sub(at);
    // Also cap the default. A 32MB page is millions of words, far more than
    // a rate needs. `--size` still measures exactly what it names.
    let want = size.unwrap_or(if region.is_aperture() {
        page.min(all).min(BENCH_DEFAULT_BYTES)
    } else {
        all.min(BENCH_DEFAULT_BYTES)
    }) as usize;
    let words = region_span(region, at, want.div_ceil(4)).map(|_| want / 4)?;
    if words == 0 {
        return Err(format!(
            "0 words to measure. `{}` holds {bytes} bytes; `--size` has to be at least 4.",
            region.name
        ));
    }
    if both {
        println!(
            "writing {words} words into `{}` at 0x{at:x} -- what was there is lost",
            region.name
        );
    } else {
        // A read-only run cannot check the data. Say so, or the rate looks
        // verified.
        println!(
            "reading {words} words of `{}` at 0x{at:x}; the contents are not checked \
             (`--both` writes a known pattern and checks it)",
            region.name
        );
    }
    // Print the thread count next to the rate, so a saved log does not mix
    // it up with a single-thread rate.
    if threads > 1 {
        println!("  with {threads} threads");
    }
    // Do not cross a page of a moving window. The base writes would be
    // measured too. A range inside one page is fine.
    if region.is_aperture()
        && (at % region.size_bytes as u64) + (words as u64 * 4) > region.size_bytes as u64
    {
        return Err(format!(
            "`{}` is a moving window of {} bytes, and this range crosses a page.\n\
             That would also measure the base register writes. Keep --base and --size \
             inside one page.",
            region.name, region.size_bytes
        ));
    }
    let base = (region.window_base + (at as usize % region.size_bytes)) as u32;
    let value = |i: usize| 0xbe11_0000u32 | i as u32;

    // Discard one run, so the first-time USB and driver cost is not measured.
    {
        let mut b = region_batch(region);
        set_page(&mut b, map, region, at / region.size_bytes as u64, master)?;
        for i in 0..words.min(16) {
            if both {
                b.write(base + 4 * i as u32, value(i));
            } else {
                b.read(base + 4 * i as u32);
            }
        }
        bus.run(&b)?;
    }

    let mut wrote = std::time::Duration::ZERO;
    let mut read = std::time::Duration::ZERO;
    for _ in 0..repeat {
        if both {
            let mut b = region_batch(region);
            for i in 0..words {
                b.write(base + 4 * i as u32, value(i));
            }
            let start = std::time::Instant::now();
            bus.run(&b)?;
            wrote += start.elapsed();
        }

        // Only region reads run in parallel. Writes are posted and already
        // fast.
        let got: Vec<u32> = if threads > 1 {
            let mut out = vec![0u32; words];
            let start = std::time::Instant::now();
            bus.read_parallel(base, &mut out, threads)?;
            read += start.elapsed();
            out
        } else {
            let mut b = region_batch(region);
            let handles: Vec<_> = (0..words).map(|i| b.read(base + 4 * i as u32)).collect();
            let start = std::time::Instant::now();
            let values = bus.run(&b)?;
            read += start.elapsed();
            handles.into_iter().map(|h| values[h]).collect()
        };

        // What was written must come back; a rate over corrupt data means
        // nothing. A read-only run has nothing to compare with.
        //
        // A value from another word means a wrong address, not a bad
        // transfer. Name it the way `memtest` does.
        for (i, &found) in got.iter().enumerate().filter(|_| both) {
            if found != value(i) {
                let elsewhere = (0..words).find(|&other| value(other) == found);
                let why = match elsewhere {
                    Some(other) => format!(
                        "That value belongs to word {other}: the address goes to the \
                         wrong place, and the data is fine. `memtest {}` checks this alone.",
                        region.name
                    ),
                    None => "The value belongs to no word that was written, so the \
                             transfer itself looks bad. Lower --tck-hz and try again."
                        .to_string(),
                };
                return Err(format!(
                    "word {i} of `{}` reads {:#010x}, but {:#010x} was written.\n\
                     Any rate measured here would be meaningless.\n\n{why}",
                    region.name,
                    found,
                    value(i)
                ));
            }
        }
    }

    if both {
        rate("write", words * repeat, wrote);
    }
    rate("read", words * repeat, read);
    Ok(())
}

/// Finds the register of a bundle with the given `role`.
fn role_of<'a>(map: &'a RegisterMap, bundle: &str, role: &str) -> Option<&'a hns_regs::Register> {
    map.registers
        .iter()
        .find(|r| r.bundle.as_deref() == Some(bundle) && r.role.as_deref() == Some(role))
}

/// Progress of a long test. Without it, "stuck" and "slow" look the same.
///
/// One line every 10 seconds. Nothing for a small total.
struct Progress {
    started: std::time::Instant,
    noted: std::time::Instant,
    total: usize,
    on: bool,
    what: &'static str,
}

impl Progress {
    fn new(total: usize, what: &'static str) -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            noted: now,
            total,
            on: total > PROGRESS_ABOVE,
            what,
        }
    }

    fn at(&mut self, done: usize, note: &str) {
        if !self.on || self.noted.elapsed().as_secs() < 10 {
            return;
        }
        self.noted = std::time::Instant::now();
        let frac = done as f64 / self.total.max(1) as f64;
        let left = self.started.elapsed().as_secs_f64() * (1.0 - frac) / frac.max(1e-9);
        println!(
            "  {}: {:.1}% ({done} of {}), about {:.0} s left{note}",
            self.what,
            frac * 100.0,
            self.total,
            left
        );
    }

    fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

/// The test value for an entry and word. The same seed gives the same value,
/// so the writer and the checker need no table.
fn pattern(seed: u32, entry: usize, word: usize) -> u32 {
    // Like splitmix32. The only need: neighbouring entries get unlike values.
    let mut x =
        seed ^ (entry as u32).wrapping_mul(0x9e37_79b9) ^ (word as u32).wrapping_mul(0x85eb_ca6b);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

/// Writes a range and reads it back.
///
/// Unlike `load --verify`, it chooses the data. File contents are biased
/// (many zeros, repeated words) and hide wiring mistakes. A value derived from
/// entry and word always shows a swap with a neighbouring entry or word.
#[allow(clippy::too_many_arguments)]
fn memtest(
    bus: &mut Bus,
    map: &RegisterMap,
    cli: &Cli,
    bundle: &str,
    base: u64,
    size: Option<u64>,
    seed: u32,
    threads: Option<usize>,
    pio: bool,
    dma: bool,
) -> Result<(), String> {
    let master = bus.master();
    let port = mem_port(map, bundle)?;
    if let MemPort::Region(region) = &port {
        not_held(bus, map, region)?;
    }
    let depth = port.depth();
    let words_per_entry = port.words();
    let entry_bytes = port.entry_bytes();

    // Take the range in bytes, like `hio read <bundle> <off>` and
    // `bench --base`, and convert to entries here.
    if !base.is_multiple_of(entry_bytes as u64) {
        return Err(format!(
            "0x{base:x} is not the start of an entry. `{bundle}` holds {entry_bytes} bytes \
             per entry.\nUse 0x{:x} for the entry it falls in.",
            base - base % entry_bytes as u64
        ));
    }
    let bytes = depth * entry_bytes;
    let all = (bytes as u64).saturating_sub(base);
    // Do not start a full test silently. Testing all of it is right (the
    // upper row and bank pins move only then), but 256MB over JTAG takes
    // nearly an hour. Make the user choose.
    if size.is_none() && all > ASK_SIZE_ABOVE {
        return Err(format!(
            "`{bundle}` holds {bytes} bytes ({depth} entries), so say how much to test.\n\
             All of it takes minutes over PCIe and about an hour per 256MB over JTAG. \
             Use `--size 64k` for a quick check, or `--size {all}` for the whole memory \
             (this tests the upper address, bank and bank-group pins)."
        ));
    }
    let want = size.unwrap_or(all);
    if !want.is_multiple_of(entry_bytes as u64) {
        return Err(format!(
            "{want} bytes is not a whole number of entries. `{bundle}` holds {entry_bytes} \
             bytes per entry."
        ));
    }
    let from = base as usize / entry_bytes;
    let to = from + want as usize / entry_bytes;
    if want == 0 || to > depth {
        return Err(format!(
            "0x{base:x} + {want} bytes does not fit `{bundle}`, which holds {bytes} bytes \
             ({depth} entries of {entry_bytes})."
        ));
    }

    let indirect = matches!(port, MemPort::Indirect { .. });

    // The size chooses the route. The card is far faster, but taking a huge
    // page costs time, so small sizes finish sooner through the window. The
    // write and the read back use the same route: one setup, and results do
    // not mix with window-only runs.
    let asked = match (pio, dma) {
        (true, _) => Some(false),
        (_, true) => Some(true),
        // `--threads` applies only to the window, so it must not be ignored.
        _ if threads.is_some() => Some(false),
        _ => None,
    };
    let chosen = route(
        map, master, "memtest", bundle, &port, want, DMA_ABOVE, asked,
    )?;
    let mut mover = open_route(bus, map, cli, chosen)?;
    let per_descriptor = mover.as_ref().map(|m| m.chunk / entry_bytes);

    let mut writing = Progress::new(to - from, "writing");
    if let (Some(mover), Some(per)) = (&mut mover, per_descriptor) {
        for a in (from..to).step_by(per) {
            let b_end = (a + per).min(to);
            let at = a as u64 * entry_bytes as u64;
            mover.send(bus, map, at, (b_end - a) * entry_bytes, |page| {
                for e in a..b_end {
                    for w in 0..words_per_entry {
                        let byte = (e - a) * entry_bytes + w * 4;
                        page[byte..byte + 4].copy_from_slice(&pattern(seed, e, w).to_le_bytes());
                    }
                }
            })?;
            writing.at(b_end - from, "");
        }
    } else {
        // Entries can be batched. Split at moving-window pages: the window
        // shows one page at a time.
        for (a, b_end) in entry_groups(&port, from, to) {
            let mut b = mem_batch(&port);
            set_mem_page(&mut b, map, &port, a, master)?;
            if let MemPort::Indirect { maddr, .. } = &port {
                b.write(maddr.offset as u32, a as u32);
            }
            for e in a..b_end {
                for w in 0..words_per_entry {
                    b.write(word_at(&port, e, w), pattern(seed, e, w));
                }
            }
            bus.run(&b)?;
            writing.at(b_end - from, "");
        }
    }
    let wrote_secs = writing.elapsed();

    // Read back. A region is grouped by page, like the writes. One round trip
    // per entry would make a full test (2GB = 540 million entries) far too
    // slow. An indirect port cannot be grouped: each entry needs `maddr` set.
    let groups: Vec<(usize, usize)> = if let Some(per) = per_descriptor {
        (from..to)
            .step_by(per)
            .map(|a| (a, (a + per).min(to)))
            .collect()
    } else if indirect {
        (from..to).map(|e| (e, e + 1)).collect()
    } else {
        entry_groups(&port, from, to)
    };

    // Parallel reads need PCIe and contiguous addresses. An indirect port
    // needs `maddr` set per entry, so it cannot run in parallel.
    let threads = threads.unwrap_or(if master == "pcie" { MEMTEST_THREADS } else { 1 });
    let spread =
        mover.is_none() && threads > 1 && !indirect && port.entry_bytes() == words_per_entry * 4;
    // Always name the route in the result line. Otherwise a saved time does
    // not say what was measured.
    let window = if spread {
        format!("through the window, {threads} threads")
    } else if threads > 1 && !indirect {
        // No region is non-contiguous today. If one appears, say why it is slow.
        format!("through the window, one at a time: the entries of `{bundle}` are not contiguous")
    } else if threads > 1 && indirect {
        format!(
            "through the window, one at a time: `{bundle}` is an indirect port, so each \
             entry needs its own address write"
        )
    } else {
        "through the window".to_string()
    };

    let mut bad = 0usize;
    let mut shown = 0usize;
    // Suspect address bits: set when a value from another address comes back.
    let mut suspect = 0u64;
    // Ranges that still hold the poison. This matters only with the DMA
    // engine: the memory was not wrong, the card never wrote these bytes here.
    let poison = u32::from_le_bytes([POISON; 4]);
    let mut undelivered = 0usize;
    let mut unwritten: Vec<(u64, u64)> = Vec::new();
    let mut reading = Progress::new(to - from, "reading back");
    for (a, b_end) in groups {
        let got: Vec<u32> = if let Some(mover) = &mut mover {
            let bytes = (b_end - a) * entry_bytes;
            let landed = mover.fetch(bus, map, a as u64 * entry_bytes as u64, bytes)?;
            let (words, _) = landed.as_chunks::<4>();
            words.iter().copied().map(u32::from_le_bytes).collect()
        } else if spread {
            // Set the page and confirm it before the parallel reads. The base
            // write is posted, and a read from another core may pass it and
            // read the wrong page. Reading one word back proves the device
            // has handled the write (PCIe ordering). Only then start threads.
            let mut b = mem_batch(&port);
            set_mem_page(&mut b, map, &port, a, master)?;
            bus.run(&b)?;
            let at = word_at(&port, a, 0);
            bus.read32(at)?;

            let mut out = vec![0u32; (b_end - a) * words_per_entry];
            bus.read_parallel(at, &mut out, threads)?;
            out
        } else {
            let mut b = mem_batch(&port);
            set_mem_page(&mut b, map, &port, a, master)?;
            if let MemPort::Indirect { maddr, .. } = &port {
                b.write(maddr.offset as u32, a as u32);
            }
            let mut hs = Vec::with_capacity((b_end - a) * words_per_entry);
            for e in a..b_end {
                for w in 0..words_per_entry {
                    hs.push(b.read(word_at(&port, e, w)));
                }
            }
            let values = bus.run(&b)?;
            hs.into_iter().map(|h| values[h]).collect()
        };
        for (i, &found) in got.iter().enumerate() {
            let e = a + i / words_per_entry;
            let w = i % words_per_entry;
            let want = pattern(seed, e, w);
            if found == want {
                continue;
            }
            bad += 1;
            let at = e as u64 * entry_bytes as u64 + w as u64 * 4;
            if mover.is_some() && found == poison {
                undelivered += 1;
                match unwritten.last_mut() {
                    Some((_, end)) if *end == at => *end = at + 4,
                    _ => unwritten.push((at, at + 4)),
                }
                continue;
            }
            // There may be many bad words (a whole address range can be
            // dead). Print the first few addresses and count the rest.
            if shown < SHOW_BAD {
                shown += 1;
                match alias_of(seed, e, w, found, depth, words_per_entry) {
                    Some((oe, ow, bit)) => {
                        suspect |= 1u64 << bit;
                        let other = oe as u64 * entry_bytes as u64 + ow as u64 * 4;
                        println!(
                            "0x{at:x}: read {found:#010x}, wrote {want:#010x}; \
                             that value belongs to 0x{other:x} (address bit {bit})"
                        );
                    }
                    None => println!("0x{at:x}: read {found:#010x}, wrote {want:#010x}"),
                }
            }
        }
        reading.at(
            b_end - from,
            &if bad > 0 {
                format!(", {bad} bad so far")
            } else {
                String::new()
            },
        );
    }

    let words = (to - from) * words_per_entry;
    let took = reading.elapsed();
    // Always print the rate when the DMA engine moved the data. It is what
    // compares with the window (702 s for 2GB), and a fast run falls under
    // the 10 s threshold below.
    let bytes = (to - from) * entry_bytes;
    let rate = |secs: f64| bytes as f64 / secs / 1e6;
    let how = if mover.is_some() {
        format!(
            "by the card: wrote {:.0} MB/s, read back {:.0} MB/s",
            rate(wrote_secs),
            rate(took)
        )
    } else {
        window
    };
    if let Some(mover) = &mover {
        mover.report_losses(bus, map)?;
    }
    let total = wrote_secs + took;
    let secs = if total >= 10.0 {
        format!(", {total:.0} s")
    } else {
        String::new()
    };
    if bad == 0 {
        println!(
            "{} entries, {words} words: all read back as written ({how}{secs})",
            to - from
        );
        Ok(())
    } else {
        // Give details only on failure. The TLP sizes help when the DMA
        // engine path is in doubt.
        match &mover {
            Some(mover) => println!(
                "moved by the card, {} bytes per descriptor: wrote {:.0} MB/s in {}-byte read \
                 requests, read back {:.0} MB/s in {}-byte TLPs{secs}",
                mover.chunk,
                rate(wrote_secs),
                tlp_bytes(bus, map, "dma_mrrs")?,
                rate(took),
                tlp_bytes(bus, map, "dma_mps")?
            ),
            None => println!("read back {how}{secs}"),
        }
        let wrong = bad - undelivered;
        if shown < wrong {
            println!("... and {} more", wrong - shown);
        }
        if !unwritten.is_empty() {
            let listed: Vec<String> = unwritten
                .iter()
                .take(SHOW_BAD)
                .map(|(a, b)| format!("0x{a:x}..0x{b:x} ({} bytes)", b - a))
                .collect();
            println!(
                "never written back by the card: {}{}",
                listed.join(", "),
                if unwritten.len() > SHOW_BAD {
                    format!(", and {} more", unwritten.len() - SHOW_BAD)
                } else {
                    String::new()
                }
            );
        }
        // If words only failed to arrive, do not blame the memory. The poison
        // is still there, so the path from the card to this machine failed.
        if wrong == 0 {
            return Err(format!(
                "{bad} of {words} words never came back: the card did not write them into \
                 this machine.\n\
                 The problem is the path from the card to this machine, not the memory. \
                 Run the same range with --pio to check the memory itself."
            ));
        }
        if suspect != 0 {
            let bits: Vec<String> = (0..64)
                .filter(|i| suspect & (1u64 << i) != 0)
                .map(|i| i.to_string())
                .collect();
            println!(
                "address bits that came back holding another word: {}",
                bits.join(", ")
            );
        }
        Err(format!(
            "{wrong} of {words} words came back wrong{}.\n\
             Each address gets its own pattern. A value from another address points at \
             the address wiring, not the storage.",
            if undelivered > 0 {
                format!(", and {undelivered} more never came back from the card")
            } else {
                String::new()
            }
        ))
    }
}

/// Finds which address a read value belongs to, trying only addresses one bit
/// away.
///
/// A full search would try hundreds of millions of candidates. A dead or
/// shorted address line maps to an address one bit away, so this catches
/// nearly all real faults. Returns the other entry and word, and the bit.
fn alias_of(
    seed: u32,
    e: usize,
    w: usize,
    got: u32,
    depth: usize,
    words: usize,
) -> Option<(usize, usize, u32)> {
    let entry_bits = usize::BITS - depth.saturating_sub(1).leading_zeros();
    let word_bits = usize::BITS - words.saturating_sub(1).leading_zeros();
    // Word bits are low and entry bits above them, as in the address. "Bit 3"
    // must mean bit 3 of the DUT address.
    for i in 0..word_bits {
        let ow = w ^ (1usize << i);
        if ow < words && pattern(seed, e, ow) == got {
            return Some((e, ow, i));
        }
    }
    for i in 0..entry_bits {
        let oe = e ^ (1usize << i);
        if oe < depth && pattern(seed, oe, w) == got {
            return Some((oe, w, i + word_bits));
        }
    }
    None
}

/// Takes the pieces to load from an ELF's program headers.
///
/// It uses `p_paddr`, where a loader really places a segment; `p_vaddr` is the
/// view through the MMU. The harness window is physical memory.
fn elf_pieces(raw: &[u8], base: Option<u64>, entry_bytes: usize) -> Result<Vec<Piece>, String> {
    let Some(base) = base else {
        return Err(
            "an ELF says where its segments go, but not where this memory starts in the \
             DUT's address space.\n\
             Set `dut_base` on the bundle in Harness.toml and generate again, or pass --base \
             with that address (for example --base 0x80000000). There is no other default: \
             a wrong guess puts the program in the wrong place."
                .to_string(),
        );
    };
    let elf = hns_host::elf::Elf::parse(raw)?;

    let mut out = Vec::new();
    for segment in elf.segments.into_iter().filter(|s| !s.bytes.is_empty()) {
        let paddr = segment.paddr;
        if paddr < base {
            return Err(format!(
                "a segment goes to {paddr:#x}, which is below the memory's base {base:#x}.\n\
                 Either --base is wrong, or this ELF is not for this memory."
            ));
        }
        let off = (paddr - base) as usize;
        if !off.is_multiple_of(entry_bytes) {
            return Err(format!(
                "a segment goes to {paddr:#x}, which is {} bytes into an entry.\n\
                 The window addresses whole entries of {entry_bytes} bytes. Link the \
                 program so its sections align.",
                off % entry_bytes
            ));
        }
        out.push(Piece {
            entry: off / entry_bytes,
            addr: Some(paddr),
            bytes: segment.bytes,
        });
    }
    if out.is_empty() {
        return Err("this ELF cannot be read: it has no loadable segments".to_string());
    }
    Ok(out)
}

/// One piece to load into memory.
struct Piece {
    /// The first entry it goes to.
    entry: usize,
    /// The DUT address, known only for an ELF. Printed so a wrong `--base`
    /// shows.
    addr: Option<u64>,
    bytes: Vec<u8>,
}

/// Loads a file into a memory bundle.
///
/// It writes whole entries. `mdata` is a holding register as wide as an
/// entry. Writing its highest word commits the entry and advances the
/// address, so `maddr` is set only for the first entry. The entry size comes
/// from `words` in `regs.json`; writing one word per entry by hand is an easy
/// mistake.
#[allow(clippy::too_many_arguments)]
fn load(
    bus: &mut Bus,
    map: &RegisterMap,
    cli: &Cli,
    bundle: &str,
    file: &PathBuf,
    at: usize,
    base: Option<u64>,
    verify: bool,
    pio: bool,
    dma: bool,
) -> Result<(), String> {
    // A region and an indirect port load the same way; only the access differs.
    let port = mem_port(map, bundle)?;
    if let MemPort::Region(region) = &port {
        not_held(bus, map, region)?;
    }
    let depth = port.depth();
    let entry_bytes = port.entry_bytes();
    let raw = std::fs::read(file).map_err(|e| read_error(file, e))?;

    let pieces = if hns_host::elf::Elf::looks_like(&raw) {
        // `--base` wins: it is there for a DUT that moves addresses itself.
        let base = base.or_else(|| map.dut_base.get(bundle).copied());
        elf_pieces(&raw, base, entry_bytes)?
    } else {
        if base.is_some() {
            eprintln!("warning: --base only means something for an ELF; ignoring it");
        }
        vec![Piece {
            entry: at,
            addr: None,
            bytes: raw,
        }]
    };

    for p in &pieces {
        let entries = p.bytes.len().div_ceil(entry_bytes);
        if p.entry + entries > depth {
            return Err(format!(
                "the file needs entries {}..{} of `{bundle}`, which is {depth} deep.\n\
                 Each entry is {entry_bytes} bytes ({} words of the window), so {} bytes do \
                 not fit. Widen the memory in Harness.toml, or load less.",
                p.entry,
                p.entry + entries,
                port.words(),
                p.bytes.len()
            ));
        }
    }

    // The size chooses the route, as in `memtest`. With --verify the data is
    // also read back, and window reads are slow, so the card takes over sooner.
    let bytes: u64 = pieces.iter().map(|p| p.bytes.len() as u64).sum();
    let above = if verify { DMA_ABOVE } else { DMA_WRITE_ABOVE };
    let asked = match (pio, dma) {
        (true, _) => Some(false),
        (_, true) => Some(true),
        _ => None,
    };
    let chosen = route(
        map,
        bus.master(),
        "load",
        bundle,
        &port,
        bytes,
        above,
        asked,
    )?;
    let mut mover = open_route(bus, map, cli, chosen)?;

    let mut total = 0usize;
    // Time only the transfer. With `time hio load`, process start and `sudo`
    // dominate: a measured 54ms for 1KB was almost all fixed cost.
    let started = std::time::Instant::now();
    for p in &pieces {
        match &mut mover {
            Some(mover) => send_piece(bus, map, mover, &port, p)?,
            None => write_entries(bus, map, &port, p)?,
        }
        total += p.bytes.len();
        // Print the address too, so a wrong `--base` shows.
        match p.addr {
            Some(a) => println!(
                "  {a:#012x} -> entry {:>6} .. {:<6}  {} bytes",
                p.entry,
                p.entry + p.bytes.len().div_ceil(entry_bytes),
                p.bytes.len()
            ),
            None => println!(
                "  entry {:>6} .. {:<6}  {} bytes",
                p.entry,
                p.entry + p.bytes.len().div_ceil(entry_bytes),
                p.bytes.len()
            ),
        }
    }
    let took = started.elapsed().as_secs_f64();
    // Route and rate on one line. For the window, also the time per word,
    // which compares with `bench`'s time per write.
    let mbps = total as f64 / took / 1e6;
    let how = if mover.is_some() {
        format!("by the card, {mbps:.0} MB/s")
    } else {
        format!(
            "through the window, {:.2} us per word, {mbps:.1} MB/s",
            took / (total / 4).max(1) as f64 * 1e6
        )
    };
    println!("loaded {total} bytes into {bundle} ({how})");

    // Report losses before verifying, so they show before a verify failure.
    if let Some(mover) = &mover {
        mover.report_losses(bus, map)?;
    }
    if verify {
        for p in &pieces {
            match &mut mover {
                Some(mover) => check_piece(bus, map, mover, &port, p)?,
                None => check_entries(bus, map, &port, p)?,
            }
        }
        if let Some(mover) = &mover {
            mover.report_losses(bus, map)?;
        }
        println!("verified: what is in the memory is what was in the file");
    }
    Ok(())
}

/// The size of a piece, rounded up to whole entries. The last entry is padded
/// with 0, the same as a window write (`entry_words`).
fn piece_span(port: &MemPort, p: &Piece) -> usize {
    p.bytes.len().div_ceil(port.entry_bytes()) * port.entry_bytes()
}

/// Splits a piece into descriptor-sized parts: `(offset in piece, length)`.
fn piece_chunks(port: &MemPort, p: &Piece, chunk: usize) -> Vec<(usize, usize)> {
    let entry_bytes = port.entry_bytes();
    let per = (chunk / entry_bytes).max(1) * entry_bytes;
    let span = piece_span(port, p);
    (0..span)
        .step_by(per)
        .map(|off| (off, per.min(span - off)))
        .collect()
}

/// Copies `len` bytes of a piece from `off`. Bytes past the file are 0.
fn piece_bytes(p: &Piece, off: usize, len: usize, out: &mut [u8]) {
    let have = p.bytes.get(off..).unwrap_or(&[]);
    let n = have.len().min(len);
    out[..n].copy_from_slice(&have[..n]);
    out[n..len].fill(0);
}

/// Has the card write one piece. The DMA engine version of `write_entries`.
fn send_piece(
    bus: &mut Bus,
    map: &RegisterMap,
    mover: &mut Mover,
    port: &MemPort,
    p: &Piece,
) -> Result<(), String> {
    let first = (p.entry * port.entry_bytes()) as u64;
    for (off, len) in piece_chunks(port, p, mover.chunk) {
        mover.send(bus, map, first + off as u64, len, |page| {
            piece_bytes(p, off, len, page)
        })?;
    }
    Ok(())
}

/// Reads one piece back and compares it. The DMA engine version of
/// `check_entries`, with the same error.
fn check_piece(
    bus: &mut Bus,
    map: &RegisterMap,
    mover: &mut Mover,
    port: &MemPort,
    p: &Piece,
) -> Result<(), String> {
    let entry_bytes = port.entry_bytes();
    let first = (p.entry * entry_bytes) as u64;
    let mut want = Vec::new();
    for (off, len) in piece_chunks(port, p, mover.chunk) {
        want.resize(len, 0);
        piece_bytes(p, off, len, &mut want);
        let got = mover.fetch(bus, map, first + off as u64, len)?;
        let Some(word) = (0..len / 4).find(|w| got[w * 4..w * 4 + 4] != want[w * 4..w * 4 + 4])
        else {
            continue;
        };
        let byte = off + word * 4;
        let value = |b: &[u8]| u32::from_le_bytes(b[word * 4..word * 4 + 4].try_into().unwrap());
        return Err(format!(
            "entry {} word {} reads {:#010x}, but the file has {:#010x}.\n\
             The memory does not hold what was written. Check the depth and the \
             entry width against Harness.toml.",
            p.entry + byte / entry_bytes,
            (byte % entry_bytes) / 4,
            value(got),
            value(&want)
        ));
    }
    Ok(())
}

fn missing_memory(map: &RegisterMap, bundle: &str, what: &str) -> String {
    let names: Vec<&str> = {
        let mut v: Vec<&str> = map
            .registers
            .iter()
            .filter(|r| r.role.as_deref() == Some("mdata"))
            .filter_map(|r| r.bundle.as_deref())
            .collect();
        v.dedup();
        // List regions too. They have no `mdata`, but they can be loaded.
        v.extend(map.regions.iter().map(|region| region.name.as_str()));
        v
    };
    format!(
        "`{bundle}` has no `{what}` register and is not a region, so a file cannot be \
         loaded into it.\n\
         Loading needs a bundle backed by `bram` or `bram_preload`. Bundles that have one \
         here: {}.",
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    )
}

/// How a memory is reached. An indirect port and a region differ only in
/// access.
///
/// `load` means "place from entry N on", whatever the access. Keeping the
/// access here means every memory command works on both kinds.
enum MemPort<'a> {
    /// An address and a data register. Writing the highest word of `mdata`
    /// commits the entry.
    Indirect {
        maddr: &'a hns_regs::Register,
        mdata: &'a hns_regs::Register,
        depth: usize,
    },
    /// A contiguous range of the window. The address is part of each access.
    Region(&'a hns_regs::Region),
}

impl MemPort<'_> {
    fn entry_bytes(&self) -> usize {
        match self {
            MemPort::Indirect { mdata, .. } => mdata.words * 4,
            MemPort::Region(region) => region.entry_bytes,
        }
    }

    fn words(&self) -> usize {
        match self {
            MemPort::Indirect { mdata, .. } => mdata.words,
            MemPort::Region(region) => region.entry_bytes / 4,
        }
    }

    fn depth(&self) -> usize {
        match self {
            MemPort::Indirect { depth, .. } => *depth,
            MemPort::Region(region) => region.depth as usize,
        }
    }
}

/// Finds how a bundle's memory is reached. Regions first: a region bundle has
/// no `maddr`.
fn mem_port<'a>(map: &'a RegisterMap, bundle: &str) -> Result<MemPort<'a>, String> {
    if let Some(region) = map.region(bundle) {
        return Ok(MemPort::Region(region));
    }
    let maddr =
        role_of(map, bundle, "maddr").ok_or_else(|| missing_memory(map, bundle, "maddr"))?;
    let mdata =
        role_of(map, bundle, "mdata").ok_or_else(|| missing_memory(map, bundle, "mdata"))?;
    let depth = role_of(map, bundle, "depth")
        .and_then(|r| r.value)
        .ok_or_else(|| format!("`{bundle}` does not say how deep it is."))?
        as usize;
    Ok(MemPort::Indirect {
        maddr,
        mdata,
        depth,
    })
}

/// Where word `w` of entry `e` is in the window. For an indirect port it is
/// in `mdata` (`maddr` picks the entry); for a region it is the address.
fn word_at(port: &MemPort, e: usize, w: usize) -> u32 {
    match port {
        MemPort::Indirect { mdata, .. } => mdata.offset as u32 + 4 * w as u32,
        // A moving window shows only one page. `entry_page` names the page,
        // and the batch sets `base` first.
        MemPort::Region(region) => {
            (region.window_base + (e * region.entry_bytes) % region.size_bytes + 4 * w) as u32
        }
    }
}

/// The moving-window page that holds entry `e`. 0 for a fixed window.
fn entry_page(port: &MemPort, e: usize) -> u64 {
    match port {
        MemPort::Region(region) if region.is_aperture() => {
            (e * region.entry_bytes) as u64 / region.size_bytes as u64
        }
        _ => 0,
    }
}

/// Sets `base` for a moving window (the `MemPort` version of `set_page`).
fn set_mem_page(
    b: &mut Batch,
    map: &RegisterMap,
    port: &MemPort,
    e: usize,
    master: &str,
) -> Result<(), String> {
    if let MemPort::Region(region) = port {
        set_page(b, map, region, entry_page(port, e), master)?;
    }
    Ok(())
}

/// Splits entries `from..to` at moving-window pages and at batch size.
fn entry_groups(port: &MemPort, from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = from;
    for e in from..to {
        // Split by length too. A large window makes a page millions of
        // entries, and one batch of that over JTAG needs gigabytes of frames.
        if entry_page(port, e) != entry_page(port, start) || e - start >= MAX_BATCH_ENTRIES {
            out.push((start, e));
            start = e;
        }
    }
    if start < to {
        out.push((start, to));
    }
    out
}

/// Splits one entry of the file into 32-bit window words, padding with 0.
fn entry_words(chunk: &[u8], words: usize) -> Vec<u32> {
    (0..words)
        .map(|w| {
            let mut v = 0u32;
            for byte in 0..4 {
                if let Some(&x) = chunk.get(w * 4 + byte) {
                    v |= (x as u32) << (8 * byte);
                }
            }
            v
        })
        .collect()
}

/// Writes one piece through the window, entry by entry.
fn write_entries(
    bus: &mut Bus,
    map: &RegisterMap,
    port: &MemPort,
    p: &Piece,
) -> Result<(), String> {
    let master = bus.master();
    let entry_bytes = port.entry_bytes();
    let words = port.words();
    let mut b = mem_batch(port);
    set_mem_page(&mut b, map, port, p.entry, master)?;
    match port {
        MemPort::Indirect { maddr, mdata, .. } => {
            b.write(maddr.offset as u32, p.entry as u32);
            for chunk in p.bytes.chunks(entry_bytes) {
                // Lowest word first. The highest word commits the entry.
                for (w, v) in entry_words(chunk, words).into_iter().enumerate() {
                    b.write(mdata.offset as u32 + 4 * w as u32, v);
                }
            }
        }
        MemPort::Region(_) => {
            // The address is in each access, so just stream the words. A
            // moving window cannot cross a page, so split there.
            let entries: Vec<&[u8]> = p.bytes.chunks(entry_bytes).collect();
            let mut page = entry_page(port, p.entry);
            let mut in_batch = 0usize;
            for (i, chunk) in entries.iter().enumerate() {
                let e = p.entry + i;
                // Split at pages and by length. A large window makes a page
                // millions of entries, too many for one round trip.
                if entry_page(port, e) != page || in_batch >= MAX_BATCH_ENTRIES {
                    bus.run(&b)?;
                    b = mem_batch(port);
                    set_mem_page(&mut b, map, port, e, master)?;
                    page = entry_page(port, e);
                    in_batch = 0;
                }
                in_batch += 1;
                for (w, v) in entry_words(chunk, words).into_iter().enumerate() {
                    b.write(word_at(port, e, w), v);
                }
            }
        }
    }
    bus.run(&b)?;
    Ok(())
}

/// Reads one piece back through the window and compares it.
fn check_entries(
    bus: &mut Bus,
    map: &RegisterMap,
    port: &MemPort,
    p: &Piece,
) -> Result<(), String> {
    let master = bus.master();
    let entry_bytes = port.entry_bytes();
    let words = port.words();
    for (i, chunk) in p.bytes.chunks(entry_bytes).enumerate() {
        let mut b = mem_batch(port);
        set_mem_page(&mut b, map, port, p.entry + i, master)?;
        let hs: Vec<_> = match port {
            MemPort::Indirect { maddr, mdata, .. } => {
                b.write(maddr.offset as u32, (p.entry + i) as u32);
                (0..words)
                    .map(|w| b.read(mdata.offset as u32 + 4 * w as u32))
                    .collect()
            }
            MemPort::Region(region) => {
                let at = (region.window_base + (p.entry + i) * entry_bytes) as u32;
                (0..words).map(|w| b.read(at + 4 * w as u32)).collect()
            }
        };
        let got = bus.run(&b)?;
        for (w, want) in entry_words(chunk, words).into_iter().enumerate() {
            let h = hs[w];
            if got[h] != want {
                return Err(format!(
                    "entry {} word {w} reads {:#010x}, but the file has {want:#010x}.\n\
                     The memory does not hold what was written. Check the depth and the \
                     entry width against Harness.toml.",
                    p.entry + i,
                    got[h]
                ));
            }
        }
    }
    Ok(())
}

/// A memory bundle at its place in the DUT's address space.
struct Span {
    bundle: String,
    /// The DUT address of entry 0 (`dut_base`, else 0).
    base: u64,
    /// Bytes the host can reach.
    bytes: u64,
    /// The DUT writes it. Only such a memory shows what the program stored.
    dut_writes: bool,
}

impl Span {
    fn covers(&self, addr: u64, len: u64) -> bool {
        addr >= self.base && addr - self.base + len <= self.bytes
    }
}

/// Bytes of zeros to write after a segment's file bytes.
///
/// Past the file bytes the program expects zeros. A loader clears them, and
/// riscv-tests (`-v`) relies on it: what the last program left there makes it
/// fail. Only the tail of a segment with file bytes is cleared (`.bss` after
/// `.data`). A segment that is all zero-fill is often the stack, megabytes
/// that the startup code does not need cleared and that take long to clear
/// over JTAG. The tail stops where a memory it goes to ends.
fn zero_fill(segment: &hns_host::elf::Segment, into: &[&Span]) -> u64 {
    let filled = segment.paddr + segment.bytes.len() as u64;
    if segment.bytes.is_empty() {
        return 0;
    }
    into.iter()
        .map(|s| (s.base + s.bytes).min(segment.paddr + segment.memsz))
        .min()
        .unwrap_or(filled)
        .saturating_sub(filled)
}

/// Every memory bundle in the map, with its DUT address.
fn spans(map: &RegisterMap) -> Result<Vec<Span>, String> {
    let mut names: Vec<String> = map
        .regions
        .iter()
        .filter(|r| r.kind.as_deref() == Some("memory"))
        .map(|r| r.name.clone())
        .collect();
    for r in &map.registers {
        if r.role.as_deref() == Some("maddr")
            && let Some(bundle) = &r.bundle
            && !names.contains(bundle)
        {
            names.push(bundle.clone());
        }
    }
    names
        .into_iter()
        .map(|bundle| {
            let port = mem_port(map, &bundle)?;
            let bytes = match &port {
                MemPort::Region(region) => region.reach_bytes(),
                _ => (port.depth() * port.entry_bytes()) as u64,
            };
            Ok(Span {
                base: map.dut_base.get(&bundle).copied().unwrap_or(0),
                bytes,
                dut_writes: !map.dut_read_only.contains(&bundle),
                bundle,
            })
        })
        .collect()
}

/// Says where the memories are, for an address none of them covers.
fn span_list(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|s| {
            format!(
                "  {:<12} {:#x}..{:#x}{}",
                s.bundle,
                s.base,
                s.base + s.bytes,
                if s.dut_writes {
                    ""
                } else {
                    " (the DUT only reads it)"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// DUT memory through the window, by DUT byte address.
///
/// The window moves whole 32-bit words and whole entries, so a write of part
/// of an entry reads the entry first. That is safe for RTT: the DUT only
/// reads the bytes the host writes (the down buffer and the read offset of
/// an up channel), and the other half of the word is the host's too.
struct WindowMem<'a> {
    bus: &'a mut Bus,
    map: &'a RegisterMap,
    spans: Vec<Span>,
}

impl WindowMem<'_> {
    /// Entries `from..to` of a memory, as bytes.
    fn read_entries(&mut self, bundle: &str, from: usize, to: usize) -> Result<Vec<u8>, String> {
        let port = mem_port(self.map, bundle)?;
        let master = self.bus.master();
        let words = port.words();
        let mut out = Vec::with_capacity((to - from) * port.entry_bytes());
        for (start, end) in entry_groups(&port, from, to) {
            let mut b = mem_batch(&port);
            set_mem_page(&mut b, self.map, &port, start, master)?;
            let mut hs = Vec::new();
            for e in start..end {
                if let MemPort::Indirect { maddr, .. } = &port {
                    b.write(maddr.offset as u32, e as u32);
                }
                for w in 0..words {
                    hs.push(b.read(word_at(&port, e, w)));
                }
            }
            let got = self.bus.run(&b)?;
            for h in hs {
                out.extend_from_slice(&got[h].to_le_bytes());
            }
        }
        Ok(out)
    }

    /// The entries that hold `addr..addr + len` in `span`, and where the
    /// range starts inside the first.
    fn entries_of(
        &self,
        span: &Span,
        addr: u64,
        len: usize,
    ) -> Result<(usize, usize, usize), String> {
        let entry_bytes = mem_port(self.map, &span.bundle)?.entry_bytes();
        let off = (addr - span.base) as usize;
        Ok((
            off / entry_bytes,
            (off + len).div_ceil(entry_bytes),
            off % entry_bytes,
        ))
    }

    /// Up to `len` bytes from `addr`, from the memory the DUT fetches from:
    /// where two memories hold the address, the one the DUT does not write
    /// (a preloaded instruction memory). Fewer at the end of the memory.
    fn read_fetch(&mut self, addr: u64, len: usize) -> Result<Vec<u8>, String> {
        let (bundle, base, bytes) = self
            .spans
            .iter()
            .filter(|s| s.covers(addr, 1))
            .min_by_key(|s| s.dut_writes)
            .map(|s| (s.bundle.clone(), s.base, s.bytes))
            .ok_or_else(|| {
                format!(
                    "no memory holds {addr:#x}. The memories are at:\n{}",
                    span_list(&self.spans)
                )
            })?;
        let len = len.min((base + bytes - addr) as usize);
        let span = Span {
            bundle: bundle.clone(),
            base,
            bytes,
            dut_writes: false,
        };
        let (from, to, skip) = self.entries_of(&span, addr, len)?;
        let got = self.read_entries(&bundle, from, to)?;
        Ok(got[skip..skip + len].to_vec())
    }
}

impl hns_host::rtt::Memory for WindowMem<'_> {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), String> {
        let len = buf.len() as u64;
        // A memory the DUT writes first: a preloaded copy of the same address
        // does not see what the program stored.
        let span = self
            .spans
            .iter()
            .filter(|s| s.covers(addr, len))
            .max_by_key(|s| s.dut_writes)
            .ok_or_else(|| {
                format!(
                    "no memory holds {addr:#x}..{:#x}. The memories are at:\n{}",
                    addr + len,
                    span_list(&self.spans)
                )
            })?;
        let (bundle, (from, to, skip)) =
            (span.bundle.clone(), self.entries_of(span, addr, buf.len())?);
        let bytes = self.read_entries(&bundle, from, to)?;
        buf.copy_from_slice(&bytes[skip..skip + buf.len()]);
        Ok(())
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), String> {
        let len = data.len() as u64;
        let targets: Vec<(String, (usize, usize, usize))> = self
            .spans
            .iter()
            .filter(|s| s.covers(addr, len))
            .map(|s| Ok((s.bundle.clone(), self.entries_of(s, addr, data.len())?)))
            .collect::<Result<_, String>>()?;
        if targets.is_empty() {
            return Err(format!(
                "no memory holds {addr:#x}..{:#x}. The memories are at:\n{}",
                addr + len,
                span_list(&self.spans)
            ));
        }
        // Every copy, as `cpu boot` loads every copy.
        for (bundle, (from, to, skip)) in targets {
            let port = mem_port(self.map, &bundle)?;
            let mut bytes = if skip == 0 && data.len().is_multiple_of(port.entry_bytes()) {
                vec![0; data.len()]
            } else {
                self.read_entries(&bundle, from, to)?
            };
            bytes[skip..skip + data.len()].copy_from_slice(data);
            let piece = Piece {
                entry: from,
                addr: None,
                bytes,
            };
            write_entries(self.bus, self.map, &port, &piece)?;
        }
        Ok(())
    }
}

/// Exits with the program's status. 0 returns, so the caller ends normally.
fn exit_with(code: i32) -> Result<(), String> {
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// What `tohost` says, riscv-tests style: 1 is a pass, `(code << 1) | 1` a
/// failure with `code`. An even value is an HTIF system call, which only
/// `wait_tohost` answers (and only when the ELF has `fromhost`).
fn tohost_status(value: u64) -> Result<Option<i32>, String> {
    match value {
        0 => Ok(None),
        v if v & 1 == 1 => Ok(Some(((v >> 1) as i32).min(255))),
        v => Err(format!(
            "the program wrote {v:#x} to `tohost`, an HTIF system call.\n\
             Only the exit values are handled: 1 for a pass, (code << 1) | 1 for a failure."
        )),
    }
}

/// The RTT block of an ELF: its address and, from the symbol, its size.
fn rtt_place(elf: &hns_host::elf::Elf, attach: &AttachArgs) -> Option<(u64, Option<u64>)> {
    match attach.rtt {
        Some(addr) => Some((addr, None)),
        None => elf.symbol("_SEGGER_RTT").map(|s| (s.value, Some(s.size))),
    }
}

/// How `boot` starts the console.
#[derive(Clone, Copy)]
enum Start {
    /// Load the ELF and run it; with `halt`, stop at the first instruction,
    /// and with `catch`, stop on a fault.
    Load { halt: bool, catch: bool },
    /// Connect to a program that is already running.
    Attach,
}

/// Loads (with `Start::Load`) and runs an ELF, then stays on its console.
/// Returns the program's exit status.
fn boot(
    cli: &Cli,
    bus: &mut Bus,
    map: &RegisterMap,
    path: &PathBuf,
    attach: &AttachArgs,
    start: Start,
) -> Result<i32, String> {
    let (load, halt, catch) = match start {
        Start::Load { halt, catch } => (true, halt, catch),
        Start::Attach => (false, false, false),
    };
    // Everything that can be wrong with the file is found before the board
    // is touched.
    let raw = std::fs::read(path).map_err(|e| read_error(path, e))?;
    let elf = hns_host::elf::Elf::parse(&raw)?;
    let ptr = if elf.wide { 8 } else { 4 };
    let rtt_at = rtt_place(&elf, attach);
    let tohost = elf.symbol("tohost").map(|s| s.value);
    let spans = spans(map)?;
    // The `ebreak` of a semihosting call must halt into the debugger, which
    // takes `dcsr.ebreakm` set before the program runs.
    let semihosting = elf
        .segments
        .iter()
        .any(|s| hns_host::semihosting::appears_in(&s.bytes));
    if semihosting && dmi_base(map, None).is_err() {
        return Err(
            "the program calls the host (semihosting), and answering takes a Debug Module. \
             Give the core's DMI a bundle with `backing = \"dmi\"`."
                .to_string(),
        );
    }

    // (segment, memories it goes to)
    let mut plan = Vec::new();
    if load {
        for segment in elf.segments.iter().filter(|s| !s.bytes.is_empty()) {
            let len = segment.bytes.len() as u64;
            let into: Vec<&Span> = spans
                .iter()
                .filter(|s| s.covers(segment.paddr, len))
                .collect();
            if into.is_empty() {
                return Err(format!(
                    "a segment goes to {:#x}..{:#x}, and no memory holds all of it. \
                     The memories are at:\n{}\n\
                     Set `dut_base` on the memory in Harness.toml, or link the program \
                     for these addresses.",
                    segment.paddr,
                    segment.paddr + len,
                    span_list(&spans)
                ));
            }
            // `.bss` and the stack need room too, but a generous stack in the
            // linker script is common, so this only warns.
            for s in &into {
                if !s.covers(segment.paddr, segment.memsz) {
                    eprintln!(
                        "warning: the segment at {:#x} needs {:#x} bytes in memory, past the end \
                         of `{}`. Its zero-filled part (.bss, the stack) has no memory there.",
                        segment.paddr, segment.memsz, s.bundle
                    );
                }
            }
            let zeros = zero_fill(segment, &into);
            plan.push((
                segment,
                into.iter().map(|s| s.bundle.clone()).collect::<Vec<_>>(),
                zeros,
            ));
        }
        if plan.is_empty() {
            return Err("this ELF has nothing to load".to_string());
        }
    }

    // The program goes into memories that may still be calibrating.
    for span in &spans {
        calibrated(bus, map, &span.bundle)?;
    }
    let mut mem = WindowMem { bus, map, spans };
    if load {
        reset(mem.bus, map, true, false)?;
        for (segment, names, zeros) in &plan {
            hns_host::rtt::Memory::write(&mut mem, segment.paddr, &segment.bytes)?;
            println!(
                "  {:#012x} -> {}  {} bytes",
                segment.paddr,
                names.join(", "),
                segment.bytes.len()
            );
            if *zeros > 0 {
                let at = segment.paddr + segment.bytes.len() as u64;
                hns_host::rtt::Memory::write(&mut mem, at, &vec![0; *zeros as usize])?;
                println!(
                    "  {at:#012x} -> {}  {zeros} bytes of zeros",
                    names.join(", ")
                );
            }
        }
        // A control block left by the last program would be taken for a live
        // one before the new program sets up its own. Clearing just the block
        // is enough; zeroing all of .bss and the stack takes long over JTAG.
        if let Some((addr, size)) = rtt_at {
            let size = size.unwrap_or(hns_host::rtt::MAGIC.len() as u64);
            hns_host::rtt::Memory::write(&mut mem, addr, &vec![0; size as usize])?;
        }
        if let Some(addr) = tohost {
            hns_host::rtt::Memory::write(&mut mem, addr, &[0; 8][..ptr as usize])?;
        }
        if halt || catch || semihosting {
            debug_module(mem.bus, map)?.halt_on_reset(true)?;
        }
        reset(mem.bus, map, false, true)?;
    }

    let program = Program {
        elf: &elf,
        rtt_at,
        tohost,
        ptr,
        attach,
        semihosting,
    };
    if halt || catch || (semihosting && load) {
        let mut dm = debug_module(mem.bus, map)?;
        let waited = dm.wait_halted();
        dm.halt_on_reset(false)?;
        waited?;
        if catch {
            let taken = dm.catch_exceptions(hns_host::dm::FAULTS)?;
            println!("{}", catching(taken));
        }
        if semihosting {
            println!("semihosting: answering the terminal, the exit and the time");
        }
        if !halt {
            dm.resume()?;
        }
    }
    if halt {
        let stop = stop_line(mem.bus, map, Some(&elf))?;
        return console(cli, &mut mem, &program, None, Some(stop));
    }

    let Some((rtt_addr, rtt_size)) = rtt_at else {
        return match tohost {
            Some(addr) => {
                let fromhost = elf.symbol("fromhost").map(|s| s.value);
                wait_tohost(&mut mem, addr, fromhost, ptr, attach.timeout)
            }
            None => {
                println!("no `_SEGGER_RTT` and no `tohost` in the ELF: nothing to wait for");
                Ok(0)
            }
        };
    };
    let rtt = wait_rtt(&mut mem, rtt_addr, ptr, rtt_size, attach)?;
    let link = Link::open(&mut mem, rtt)?;
    console(cli, &mut mem, &program, Some(link), None)
}

/// The Debug Module for `boot --halt`, which must be up while the DUT is
/// held in reset.
fn debug_module<'a>(
    bus: &'a mut Bus,
    map: &RegisterMap,
) -> Result<hns_host::dm::Debug<WindowDmi<'a>>, String> {
    let base = dmi_base(map, None)?;
    hns_host::dm::Debug::attach(WindowDmi::new(bus, map, base), 0).map_err(|e| {
        format!(
            "{e}\n`--halt` needs the Debug Module up while the DUT is held in reset. \
             Give its reset port in the `dmi` bundle (`reset = \"<port>\"`), so it \
             takes the harness reset instead."
        )
    })
}

/// What the console needs to know about the program.
struct Program<'a> {
    elf: &'a hns_host::elf::Elf,
    /// Where the RTT control block is, and how large.
    rtt_at: Option<(u64, Option<u64>)>,
    tohost: Option<u64>,
    /// Bytes in a pointer.
    ptr: u64,
    attach: &'a AttachArgs,
    /// The program may call the host (semihosting).
    semihosting: bool,
}

/// The RTT control block the console reads and types into.
struct Link {
    rtt: hns_host::rtt::Rtt,
    /// The up channel named `hio-exit`.
    exit: Option<u64>,
    can_type: bool,
}

impl Link {
    fn open(mem: &mut WindowMem, rtt: hns_host::rtt::Rtt) -> Result<Self, String> {
        let exit = rtt.find(mem, hns_host::rtt::Dir::Up, "hio-exit")?;
        Ok(Link {
            can_type: rtt.down > 0,
            exit,
            rtt,
        })
    }

    fn hello(&self) -> String {
        format!(
            "hio: connected to RTT at {:#x}; Ctrl-A c debug prompt, Ctrl-A x leaves\n",
            self.rtt.addr
        )
    }
}

/// Polls `tohost` until the program reports. An HTIF `write` on the way
/// (riscv-tests' benchmarks print with it) is answered through `fromhost`.
fn wait_tohost(
    mem: &mut WindowMem,
    addr: u64,
    fromhost: Option<u64>,
    ptr: u64,
    timeout: Option<std::time::Duration>,
) -> Result<i32, String> {
    let started = std::time::Instant::now();
    loop {
        let mut raw = [0u8; 8];
        hns_host::rtt::Memory::read(mem, addr, &mut raw[..ptr as usize])?;
        let value = u64::from_le_bytes(raw);
        if value != 0
            && value & 1 == 0
            && let Some(fromhost) = fromhost
        {
            htif_call(mem, value, addr, fromhost)?;
            continue;
        }
        if let Some(code) = tohost_status(value)? {
            return Ok(code);
        }
        if let Some(limit) = timeout
            && started.elapsed() >= limit
        {
            eprintln!("{}", timed_out(limit));
            return Ok(TIMED_OUT);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Answers one HTIF system call, the way riscv-tests' `syscall()` makes it:
/// `tohost` holds the address of `magic_mem`, whose 64-bit words are the
/// call number and its arguments. Only `write` (64) to stdout or stderr is
/// answered. Its result goes to `magic_mem[0]`; then `tohost` is cleared and
/// `fromhost` set, which lets the program go on.
fn htif_call(mem: &mut WindowMem, magic: u64, tohost: u64, fromhost: u64) -> Result<(), String> {
    use hns_host::rtt::Memory;
    use std::io::Write;
    /// The HTIF system call number of `write`.
    const SYS_WRITE: u64 = 64;
    let mut raw = [0u8; 32];
    mem.read(magic, &mut raw)?;
    let word = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
    let (which, fd, buf, len) = (word(0), word(1), word(2), word(3));
    if which != SYS_WRITE || !(1..=2).contains(&fd) {
        return Err(format!(
            "the program made HTIF system call {which} (arguments {fd:#x}, {buf:#x}, {len:#x}). \
             hio answers only `write` (64) to stdout or stderr."
        ));
    }
    let mut text = vec![0; len as usize];
    mem.read(buf, &mut text)?;
    if fd == 1 {
        std::io::stdout()
            .write_all(&text)
            .and_then(|()| std::io::stdout().flush())
    } else {
        std::io::stderr().write_all(&text)
    }
    .map_err(|e| format!("cannot print the program's output: {e}"))?;
    mem.write(magic, &len.to_le_bytes())?;
    mem.write(tohost, &[0; 8])?;
    mem.write(fromhost, &1u64.to_le_bytes())?;
    Ok(())
}

fn timed_out(limit: std::time::Duration) -> String {
    format!(
        "hio: the program did not exit within {} s (--timeout); exit status {TIMED_OUT}",
        limit.as_secs_f64()
    )
}

fn read_tohost(mem: &mut WindowMem, addr: u64, ptr: u64) -> Result<Option<i32>, String> {
    let mut raw = [0u8; 8];
    hns_host::rtt::Memory::read(mem, addr, &mut raw[..ptr as usize])?;
    tohost_status(u64::from_le_bytes(raw))
}

/// Looks once for the control block. `None` while the program has not set it
/// up yet.
fn try_rtt(
    mem: &mut WindowMem,
    addr: u64,
    ptr: u64,
    size: Option<u64>,
    attach: &AttachArgs,
) -> Result<Option<hns_host::rtt::Rtt>, String> {
    use hns_host::rtt::{AttachError, Layout, Rtt};
    let found = match attach.rtt_layout {
        Some(RttLayout::RttTarget) => Rtt::attach_as(mem, addr, Layout { ptr, field: ptr }),
        Some(RttLayout::Segger) => Rtt::attach_as(mem, addr, Layout { ptr, field: 4 }),
        None => Rtt::attach(mem, addr, ptr, size),
    };
    match found {
        Ok(rtt) => Ok(Some(rtt)),
        Err(AttachError::NotReady) => Ok(None),
        Err(AttachError::Broken(e)) => Err(e),
    }
}

/// Waits for the program to set up its control block.
fn wait_rtt(
    mem: &mut WindowMem,
    addr: u64,
    ptr: u64,
    size: Option<u64>,
    attach: &AttachArgs,
) -> Result<hns_host::rtt::Rtt, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(attach.wait_ms);
    loop {
        if let Some(rtt) = try_rtt(mem, addr, ptr, size, attach)? {
            return Ok(rtt);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "the RTT control block at {addr:#x} did not appear within {} ms.\n\
                 Either the program has not reached its RTT setup, or the host cannot \
                 see it: a write-back data cache holds the block where the window does \
                 not look. Put RTT in memory the CPU does not cache, or wait longer \
                 with --wait-ms.",
                attach.wait_ms
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The console: up channel 0 to the terminal, keys to down channel 0, until
/// Ctrl-A x or until the program reports an exit status. Ctrl-A c switches to
/// the prompt and back.
///
/// Without a `link` (the program has not set up RTT yet, after `boot
/// --halt`) it starts at the prompt, and looks for the control block while
/// it waits.
fn console(
    cli: &Cli,
    mem: &mut WindowMem,
    program: &Program,
    mut link: Option<Link>,
    halted: Option<String>,
) -> Result<i32, String> {
    use std::io::Write;

    let elf = program.elf;
    let raw = RawTerminal::enter();
    let mut out = std::io::stdout();
    let mut ctrl_a = false;
    let mut idle = 0u32;
    // The debug prompt. While it is up, keys go to it and the target's
    // output waits in `held`.
    let mut prompt = Prompt::new(Level::Cpu, true);
    let mut at_prompt = halted.is_some();
    let mut held = Vec::new();
    let mut watch = HaltWatch::new(mem.bus, mem.map, program.semihosting);
    let started = std::time::Instant::now();
    match (&link, &halted) {
        (Some(link), _) => to_terminal(&raw, &mut out, link.hello().as_bytes())?,
        (None, Some(stop)) => {
            let text = format!("{stop}\n{}", prompt.text());
            to_terminal(&raw, &mut out, text.as_bytes())?;
        }
        (None, None) => {}
    }
    loop {
        let mut worked = false;

        if let Some(limit) = program.attach.timeout
            && started.elapsed() >= limit
        {
            drop(raw);
            eprintln!("\n{}", timed_out(limit));
            return Ok(TIMED_OUT);
        }

        // Keys first, so typing is not held up behind a busy output.
        for key in raw.keys() {
            worked = true;
            if ctrl_a {
                ctrl_a = false;
                match key {
                    b'x' => {
                        drop(raw);
                        eprintln!();
                        return Ok(0);
                    }
                    b'c' if at_prompt && link.is_none() => {
                        let why = if program.rtt_at.is_some() {
                            "the program has not set up RTT yet: `resume` lets it run."
                        } else {
                            "the ELF has no `_SEGGER_RTT`, so there is no console."
                        };
                        let text = format!(
                            "\n({why} Ctrl-A x leaves.)\n{}{}",
                            prompt.text(),
                            prompt.edit.line
                        );
                        to_terminal(&raw, &mut out, text.as_bytes())?;
                        continue;
                    }
                    b'c' => {
                        at_prompt = !at_prompt;
                        if at_prompt {
                            let line = format!("\n{}{}", prompt.text(), prompt.edit.line);
                            to_terminal(&raw, &mut out, line.as_bytes())?;
                        } else {
                            to_terminal(&raw, &mut out, b"\n")?;
                            to_terminal(&raw, &mut out, &std::mem::take(&mut held))?;
                        }
                        continue;
                    }
                    _ => {}
                }
            } else if key == 0x01 {
                ctrl_a = true;
                continue;
            }
            if at_prompt {
                let mut echo = Vec::new();
                let line = prompt.edit.key(key, &mut echo);
                if raw.active() {
                    to_terminal(&raw, &mut out, &echo)?;
                }
                if let Some(line) = line {
                    // Commands print plain lines, so the terminal leaves raw
                    // mode while one runs.
                    raw.pause();
                    match prompt.run(&line, cli, mem.bus, mem.map, Some(elf)) {
                        Ok(Ran::Halted) => watch.sync(mem.bus),
                        Ok(Ran::Resumed) => watch.running(),
                        Ok(_) => {}
                        Err(e) => eprintln!("{e}"),
                    }
                    let _ = out.flush();
                    raw.resume();
                    to_terminal(&raw, &mut out, prompt.text().as_bytes())?;
                }
                continue;
            }
            if let Some(link) = &link
                && link.can_type
            {
                // A key that does not fit is dropped; the target is not reading.
                link.rtt.write_down(mem, 0, &[key])?;
            }
        }

        // At the prompt the output is kept for later, up to a limit; past it,
        // the target's own buffer holds the rest.
        if let Some(link) = &link
            && (!at_prompt || held.len() < HELD_MAX)
        {
            let text = link.rtt.read_up(mem, 0, 4096)?;
            if !text.is_empty() {
                worked = true;
                if at_prompt {
                    held.extend_from_slice(&text);
                } else {
                    to_terminal(&raw, &mut out, &text)?;
                }
            }
        }

        // A hart that halts by itself, at a breakpoint: say where, and go to
        // the prompt.
        let look = if worked { None } else { watch.look(mem.bus) };
        // A semihosting call may be waiting from before the console started,
        // so any halt is looked at once, not only a new one.
        if look.is_some() && program.semihosting && !watch.examined {
            match semihosting_call(mem, started)? {
                None => watch.examined = true,
                Some((outcome, said)) => {
                    let mut text = said.stdout;
                    text.extend(said.stderr);
                    if at_prompt {
                        held.extend(text);
                    } else {
                        to_terminal(&raw, &mut out, &text)?;
                    }
                    match outcome {
                        hns_host::semihosting::Outcome::Exit(code) => {
                            drop(raw);
                            return Ok(code);
                        }
                        hns_host::semihosting::Outcome::Return(_) => {
                            watch.running();
                            continue;
                        }
                    }
                }
            }
        }
        if look == Some(true) {
            raw.pause();
            let stop = stop_line(mem.bus, mem.map, Some(elf)).unwrap_or_else(|e| e);
            raw.resume();
            // Without a terminal nobody can debug it, and waiting would hang
            // a test run.
            if !raw.active() {
                return Err(format!(
                    "{stop}\nThere is no terminal to debug it from, so hio stops here."
                ));
            }
            at_prompt = true;
            prompt.level = Level::Cpu;
            let text = format!("\n{stop}\n{}{}", prompt.text(), prompt.edit.line);
            to_terminal(&raw, &mut out, text.as_bytes())?;
            continue;
        }

        if !worked {
            idle += 1;
        } else {
            idle = 0;
        }

        // Not connected yet: look for the control block now and then. Its
        // output waits in `held` until Ctrl-A c.
        if link.is_none()
            && idle.is_multiple_of(16)
            && let Some((addr, size)) = program.rtt_at
            && let Some(rtt) = try_rtt(mem, addr, program.ptr, size, program.attach)?
        {
            let opened = Link::open(mem, rtt)?;
            held.extend_from_slice(opened.hello().as_bytes());
            link = Some(opened);
        }

        // The end of the program, after its last output is shown. Not while
        // at the prompt: a reset from there rebuilds the control block, and
        // the output is not shown yet.
        if !worked && !at_prompt {
            if let Some(link) = &link
                && let Some(n) = link.exit
            {
                let code = link.rtt.read_up(mem, n, 1)?;
                if let Some(&code) = code.first() {
                    drop(raw);
                    return Ok(code as i32);
                }
            }
            if let Some(addr) = program.tohost
                && let Some(code) = read_tohost(mem, addr, program.ptr)?
            {
                drop(raw);
                return Ok(code);
            }
            // The magic costs a round trip, so it is looked at only when idle.
            if let Some(link) = &link
                && idle.is_multiple_of(16)
                && !link.rtt.alive(mem)?
            {
                drop(raw);
                return Err(format!(
                    "the RTT control block at {:#x} is gone: the target was reset or \
                     reloaded under the console.",
                    link.rtt.addr
                ));
            }
        }
        if !worked {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Target output kept while at the prompt.
const HELD_MAX: usize = 1 << 20;

/// Writes to the terminal. In raw mode a bare LF does not return the
/// carriage, so it gets a CR.
fn to_terminal(raw: &RawTerminal, out: &mut std::io::Stdout, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if bytes.is_empty() {
        return Ok(());
    }
    let text: Vec<u8> = if raw.active() {
        bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect()
    } else {
        bytes.to_vec()
    };
    out.write_all(&text)
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())
}

/// The line typed at the prompt. The terminal is raw, so this does its own
/// echo: printable keys, Backspace, Ctrl-C / Ctrl-U to drop the line, and the
/// up and down arrows for earlier lines.
#[derive(Default)]
struct LineEdit {
    line: String,
    history: Vec<String>,
    /// How far back in `history` the line came from; 0 is a new line.
    back: usize,
    /// An escape sequence being read.
    esc: Vec<u8>,
}

impl LineEdit {
    /// Takes one key. Returns the line when Enter ends it; `echo` gets what
    /// to show.
    fn key(&mut self, key: u8, echo: &mut Vec<u8>) -> Option<String> {
        if !self.esc.is_empty() || key == 0x1b {
            self.esc.push(key);
            match self.esc.as_slice() {
                [0x1b] | [0x1b, b'['] => return None,
                [0x1b, b'[', b'A'] => self.recall(true, echo),
                [0x1b, b'[', b'B'] => self.recall(false, echo),
                _ => {}
            }
            self.esc.clear();
            return None;
        }
        match key {
            b'\r' | b'\n' => {
                echo.push(b'\n');
                let line = std::mem::take(&mut self.line);
                self.back = 0;
                if !line.trim().is_empty() && self.history.last() != Some(&line) {
                    self.history.push(line.clone());
                }
                Some(line)
            }
            0x7f | 0x08 => {
                if self.line.pop().is_some() {
                    echo.extend_from_slice(b"\x08 \x08");
                }
                None
            }
            0x03 | 0x15 => {
                self.erase(echo);
                self.back = 0;
                None
            }
            0x20..=0x7e => {
                self.line.push(key as char);
                echo.push(key);
                None
            }
            _ => None,
        }
    }

    fn erase(&mut self, echo: &mut Vec<u8>) {
        for _ in 0..self.line.len() {
            echo.extend_from_slice(b"\x08 \x08");
        }
        self.line.clear();
    }

    /// Up (`older`) or down through the lines typed before.
    fn recall(&mut self, older: bool, echo: &mut Vec<u8>) {
        let back = if older {
            (self.back + 1).min(self.history.len())
        } else {
            self.back.saturating_sub(1)
        };
        if back == self.back {
            return;
        }
        self.back = back;
        self.erase(echo);
        if back > 0 {
            self.line = self.history[self.history.len() - back].clone();
            echo.extend_from_slice(self.line.as_bytes());
        }
    }
}

/// Which commands a bare word names at the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// The lines of a procedure file: `read dmem 0x100`, `cpu halt`.
    Top,
    /// `hio cpu` commands without `cpu`: `halt`, `step 3`.
    Cpu,
}

/// What a line at the prompt did to the hart, so that a halt someone typed
/// is not announced and one that follows a resume is.
#[derive(Debug, PartialEq, Eq)]
enum Ran {
    /// Nothing that moves the hart on purpose. A halt seen later is news:
    /// `break` on a running hart may hit at once.
    Done,
    /// Halted on purpose (`halt`, `step`, `reset --halt`).
    Halted,
    /// Let run (`resume`, a reset).
    Resumed,
    Quit,
}

/// The prompt: the line being typed and the level it is at.
struct Prompt {
    level: Level,
    edit: LineEdit,
    /// Inside `cpu boot` / `cpu console`, where Ctrl-A c is the way out.
    in_console: bool,
}

/// What a line at the prompt asks for.
enum PromptAction {
    Nothing,
    /// Text to print, such as `help`.
    Say(String),
    Enter(Level),
    Quit,
    /// A `hio cpu` command.
    Cpu(CpuCmd),
    /// A line of a procedure file.
    Hio(Step),
}

impl Prompt {
    fn new(level: Level, in_console: bool) -> Self {
        Prompt {
            level,
            edit: LineEdit::default(),
            in_console,
        }
    }

    fn text(&self) -> &'static str {
        match self.level {
            Level::Top => "(hio) ",
            Level::Cpu => "(hio cpu) ",
        }
    }

    /// Runs one line.
    fn run(
        &mut self,
        line: &str,
        cli: &Cli,
        bus: &mut Bus,
        map: &RegisterMap,
        elf: Option<&hns_host::elf::Elf>,
    ) -> Result<Ran, String> {
        match prompt_action(self.level, self.in_console, line)? {
            PromptAction::Nothing => {}
            PromptAction::Say(text) => print!("{text}"),
            PromptAction::Enter(level) => self.level = level,
            PromptAction::Quit => return Ok(Ran::Quit),
            PromptAction::Cpu(cmd) | PromptAction::Hio(Step::Window(Cmd::Cpu(cmd))) => {
                cpu_debug(bus, map, &cmd, elf)?;
                return Ok(match cmd {
                    CpuCmd::Halt { .. }
                    | CpuCmd::Step { .. }
                    | CpuCmd::Reset { halt: true, .. } => Ran::Halted,
                    CpuCmd::Resume { .. } | CpuCmd::Reset { halt: false, .. } => Ran::Resumed,
                    _ => Ran::Done,
                });
            }
            PromptAction::Hio(step) => {
                let reset = matches!(step, Step::Window(Cmd::Reset { .. }));
                run_step(cli, bus, map, step)?;
                if reset {
                    return Ok(Ran::Resumed);
                }
            }
        }
        Ok(Ran::Done)
    }
}

/// Reads a line at `level`. A level's name moves there (`cpu`, `hio`; `up`
/// is `hio` too), and a line that starts with it runs there once. `help`
/// lists what a level takes.
fn prompt_action(level: Level, in_console: bool, line: &str) -> Result<PromptAction, String> {
    use clap::Parser;

    #[derive(Parser)]
    #[command(name = "", no_binary_name = true)]
    struct CpuLine {
        #[command(subcommand)]
        cmd: CpuCmd,
    }

    let shown = |e: &clap::Error| {
        matches!(
            e.kind(),
            clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        )
    };
    let mut words: Vec<&str> = line.split_whitespace().collect();
    let Some(&first) = words.first() else {
        return Ok(PromptAction::Nothing);
    };
    if first == "hio" {
        if words.len() == 1 {
            return Ok(PromptAction::Enter(Level::Top));
        }
        // A line from the shell, or one top-level line from `(hio cpu)`.
        return prompt_action(Level::Top, in_console, &words[1..].join(" "));
    }
    let level = if level == Level::Top && first == "cpu" && words.len() > 1 {
        // `cpu halt` at `(hio)` runs as `halt` at `(hio cpu)`, which also
        // gives it the ELF's symbols.
        words.remove(0);
        Level::Cpu
    } else {
        level
    };
    let first = words[0];
    let line = &words.join(" ");
    if words.len() == 1 {
        match (level, first) {
            (Level::Top, "help") => {
                return Ok(PromptAction::Say(format!(
                    "{}\nLines are as in a procedure file. `cpu` goes to the CPU commands; \
                     `help <command>` says more.\n",
                    command_list()
                )));
            }
            (Level::Top, "cpu") => return Ok(PromptAction::Enter(Level::Cpu)),
            (Level::Cpu, "up") => return Ok(PromptAction::Enter(Level::Top)),
            (Level::Cpu, "cpu") => return Ok(PromptAction::Nothing),
            (_, "exit") if in_console => {
                return Err("Ctrl-A c goes back to the console; Ctrl-A x leaves.".to_string());
            }
            (_, "exit") => return Ok(PromptAction::Quit),
            _ => {}
        }
    }
    let busy = |what: &str| -> Result<PromptAction, String> {
        Err(if in_console {
            "the console is already open: Ctrl-A c goes back to it.".to_string()
        } else {
            format!("`{what}` takes over the terminal; run it on its own, outside the prompt.")
        })
    };
    // The instruction trace goes with the debugger: `trace dump` right
    // after a breakpoint, without `hio`.
    if level == Level::Cpu && first == "trace" {
        return prompt_action(Level::Top, in_console, line);
    }
    match level {
        Level::Top => {
            if let Err(e) = Line::try_parse_from(&words)
                && shown(&e)
            {
                return Ok(PromptAction::Say(e.to_string()));
            }
            Ok(PromptAction::Hio(parse_step(line)?))
        }
        Level::Cpu => match CpuLine::try_parse_from(&words) {
            Ok(parsed) => match parsed.cmd {
                CpuCmd::Boot { .. } | CpuCmd::Console { .. } => busy(first),
                cmd => Ok(PromptAction::Cpu(cmd)),
            },
            Err(e) if shown(&e) => Ok(PromptAction::Say(format!(
                "{}`hio` goes back to the other commands; `hio <line>` runs one of them.\n",
                e
            ))),
            Err(e) => Err(format!(
                "{}\nThese are the `hio cpu` commands; `hio` goes back to the others, \
                 and `hio <line>` runs one of them.",
                e.to_string().trim_end()
            )),
        },
    }
}

/// `hio` with no command: a prompt that keeps the link open, so one command
/// after another costs no reconnection. Lines are those of a procedure file.
/// Read from a pipe, the lines just run, and the first error stops them.
fn shell(cli: &Cli) -> Result<(), String> {
    let help = |e: String| format!("{e}\n`hio --help` lists the commands.");
    let (map_path, map) = find_regs(cli).map_err(help)?;
    let mut bus = open_bus(cli, &map_path, &map).map_err(help)?;
    if !cli.no_verify {
        check_identity(&mut bus, &map)?;
    }
    let mut prompt = Prompt::new(Level::Top, false);

    // SAFETY: a plain query on fd 0.
    if unsafe { libc::isatty(0) } == 0 {
        use std::io::BufRead;
        for (i, line) in std::io::stdin().lock().lines().enumerate() {
            let line = line.map_err(|e| format!("cannot read standard input: {e}"))?;
            let text = line.split('#').next().unwrap_or("").trim();
            let ran = prompt
                .run(text, cli, &mut bus, &map, None)
                .map_err(|e| format!("line {}: {line}\n{e}", i + 1))?;
            if ran == Ran::Quit {
                break;
            }
        }
        return Ok(());
    }

    eprintln!(
        "hio: connected to {} ({}, map {:#010x}); `help` lists commands, Ctrl-D leaves",
        map.target.as_deref().unwrap_or("the board"),
        map.dut,
        map.map_hash
    );
    let raw = RawTerminal::enter();
    let mut out = std::io::stdout();
    let mut watch = HaltWatch::new(&mut bus, &map, false);
    to_terminal(&raw, &mut out, prompt.text().as_bytes())?;
    loop {
        let keys = raw.keys();
        if keys.is_empty() {
            if watch.poll(&mut bus) {
                raw.pause();
                let stop = stop_line(&mut bus, &map, None).unwrap_or_else(|e| e);
                raw.resume();
                let text = format!("\n{stop}\n{}{}", prompt.text(), prompt.edit.line);
                to_terminal(&raw, &mut out, text.as_bytes())?;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        for key in keys {
            // Ctrl-D on an empty line leaves, as in a shell.
            if key == 0x04 && prompt.edit.line.is_empty() {
                to_terminal(&raw, &mut out, b"\n")?;
                return Ok(());
            }
            let mut echo = Vec::new();
            let line = prompt.edit.key(key, &mut echo);
            to_terminal(&raw, &mut out, &echo)?;
            if let Some(line) = line {
                raw.pause();
                let ran = prompt.run(&line, cli, &mut bus, &map, None);
                match &ran {
                    Ok(Ran::Halted) => watch.sync(&mut bus),
                    Ok(Ran::Resumed) => watch.running(),
                    _ => {}
                }
                if let Err(e) = &ran {
                    eprintln!("{e}");
                }
                raw.resume();
                if ran == Ok(Ran::Quit) {
                    return Ok(());
                }
                to_terminal(&raw, &mut out, prompt.text().as_bytes())?;
            }
        }
    }
}

/// `cpu mem`: words at a DUT address, four to a line, each line named by
/// the symbol it falls in.
fn mem(
    bus: &mut Bus,
    map: &RegisterMap,
    elf: Option<&hns_host::elf::Elf>,
    place: &str,
    words: Option<u32>,
    write: &[u32],
) -> Result<(), String> {
    use hns_host::rtt::Memory;
    /// Words read when a symbol is larger, unless a count is given.
    const MOST: u64 = 256;

    let addr = place_address(place, elf)?;
    let size = elf
        .and_then(|e| e.symbols.iter().find(|s| s.value == addr && s.size > 0))
        .map(|s| s.size.div_ceil(4));
    let count = match (words, write.is_empty()) {
        (_, false) => write.len() as u64,
        (Some(n), true) => u64::from(n.max(1)),
        (None, true) => size.unwrap_or(1).min(MOST),
    };
    let mut mem = WindowMem {
        bus,
        map,
        spans: spans(map)?,
    };
    if !write.is_empty() {
        let bytes: Vec<u8> = write.iter().flat_map(|w| w.to_le_bytes()).collect();
        mem.write(addr, &bytes)?;
    }
    let mut bytes = vec![0; count as usize * 4];
    mem.read(addr, &mut bytes)?;
    for (i, line) in bytes.chunks(16).enumerate() {
        let at = addr + i as u64 * 16;
        let words: Vec<String> = line
            .chunks(4)
            .map(|w| {
                format!(
                    "{:#010x}",
                    u32::from_le_bytes(w.try_into().expect("4 bytes"))
                )
            })
            .collect();
        println!("{at:#010x}{}  {}", symbolize(elf, at), words.join(" "));
    }
    if words.is_none() && write.is_empty() && size.is_some_and(|s| s > MOST) {
        println!("(the first {MOST} words; give a count for more)");
    }
    Ok(())
}

/// Instructions, as `hio cpu disasm` prints them.
///
/// `count` instructions from `from`, read from the memory the DUT fetches
/// from. A line starting a symbol of the ELF gets a `<name>:` heading. The
/// line at `pc` is marked `=>`. Where the ELF holds other bytes at the same
/// address, the line says so: the memory changed after loading, or holds
/// another program.
fn disasm_lines(
    mem: &mut WindowMem,
    elf: Option<&hns_host::elf::Elf>,
    from: u64,
    count: usize,
    end: Option<u64>,
    xlen: u32,
    pc: Option<u64>,
) -> Result<Vec<String>, String> {
    use hns_host::disasm;
    let bytes = mem.read_fetch(from, count * 4)?;
    let mut lines = Vec::new();
    let mut at = 0;
    // The last instruction, for an `auipc` pair.
    let mut last: Option<(u64, disasm::Insn)> = None;
    for n in 0..count {
        if at + 2 > bytes.len() {
            break;
        }
        let addr = from + at as u64;
        if end.is_some_and(|end| addr >= end) {
            break;
        }
        let low = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let len = disasm::length(low);
        if at + len > bytes.len() {
            break;
        }
        let here = &bytes[at..at + len];
        let label = symbolize(elf, addr);
        if !label.is_empty() && (n == 0 || !label.contains('+')) {
            lines.push(format!("{}:", label.trim_start()));
        }
        let insn = disasm::decode(here, addr, xlen);
        let pair = match (&last, &insn) {
            (Some((first_addr, first)), Some(i)) => disasm::auipc_pair(first, *first_addr, i, xlen)
                .map(|t| format!("  # {}{}", word(t, xlen), symbolize(elf, t))),
            _ => None,
        };
        last = insn.clone().map(|i| (addr, i));
        let (raw, text) = match insn {
            Some(i) => {
                let raw = if i.len == 2 {
                    format!("{:04x}    ", i.raw)
                } else {
                    format!("{:08x}", i.raw)
                };
                let target = i
                    .target
                    .map(|t| format!("{}{}", word(t, xlen), symbolize(elf, t)))
                    .unwrap_or_default();
                let pair = pair.unwrap_or_default();
                (
                    raw,
                    format!("{:<8}{}{target}{pair}", i.mnemonic, i.operands),
                )
            }
            None if len == 2 => (format!("{low:04x}    "), format!(".short  {low:#06x}")),
            None => {
                let w = u32::from_le_bytes(here.try_into().expect("4 bytes"));
                (format!("{w:08x}"), format!(".word   {w:#010x}"))
            }
        };
        let mark = if pc == Some(addr) { "=>" } else { "  " };
        let note = match elf.and_then(|e| elf_bytes(e, addr, len)) {
            Some(file) if file != here => format!(
                "    (the ELF has {} here)",
                file.iter()
                    .rev()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
            _ => String::new(),
        };
        lines.push(format!(
            "{mark} {}:  {raw}  {}{note}",
            word(addr, xlen),
            text.trim_end()
        ));
        at += len;
    }
    Ok(lines)
}

/// Where a listing of `place` starts, how many instructions it shows, and
/// where it ends: a whole symbol (up to 64) unless a count is given.
fn disasm_range(
    place: &str,
    count: Option<u32>,
    elf: Option<&hns_host::elf::Elf>,
) -> Result<(u64, usize, Option<u64>), String> {
    let addr = place_address(place, elf)?;
    let sized = elf.and_then(|e| e.symbols.iter().find(|s| s.value == addr && s.size > 0));
    Ok(match (count, sized) {
        (Some(n), _) => (addr, n as usize, None),
        (None, Some(s)) => (addr, 64, Some(s.value + s.size)),
        (None, None) => (addr, 12, None),
    })
}

/// The line of the instruction at `pc`, for a stop report. Nothing when no
/// memory holds it.
fn pc_insn(
    bus: &mut Bus,
    map: &RegisterMap,
    elf: Option<&hns_host::elf::Elf>,
    pc: u64,
    xlen: u32,
) -> Option<String> {
    let mut mem = WindowMem {
        bus,
        map,
        spans: spans(map).ok()?,
    };
    let lines = disasm_lines(&mut mem, elf, pc, 1, None, xlen, Some(pc)).ok()?;
    lines.into_iter().last()
}

/// The bytes a loadable segment of the ELF puts at `addr`, if one does.
fn elf_bytes(elf: &hns_host::elf::Elf, addr: u64, len: usize) -> Option<&[u8]> {
    elf.segments.iter().find_map(|s| {
        let off = addr.checked_sub(s.paddr)? as usize;
        s.bytes.get(off..off + len)
    })
}

/// Where `disasm` starts when it shows the code around `pc`: a few
/// instructions before it, counted from the start of its symbol, since
/// compressed instructions cannot be walked backwards.
fn disasm_start(mem: &mut WindowMem, elf: Option<&hns_host::elf::Elf>, pc: u64) -> u64 {
    /// Instructions shown before the pc.
    const BEFORE: usize = 4;
    /// Bytes walked from the start of the symbol, at most.
    const WALK: u64 = 1024;
    let Some(start) = elf.and_then(|e| {
        e.symbols
            .iter()
            .filter(|s| s.size > 0 && s.value <= pc && pc < s.value + s.size)
            .min_by_key(|s| s.size)
            .map(|s| s.value)
    }) else {
        return pc;
    };
    let start = start.max(pc.saturating_sub(WALK));
    let Ok(bytes) = mem.read_fetch(start, (pc - start) as usize) else {
        return pc;
    };
    let mut seen = Vec::new();
    let mut at = 0;
    while at + 2 <= bytes.len() {
        seen.push(start + at as u64);
        at += hns_host::disasm::length(u16::from_le_bytes([bytes[at], bytes[at + 1]]));
    }
    // Walking from the middle of a function may be out of step; then show
    // from the pc.
    if start + at as u64 != pc {
        return pc;
    }
    seen.len().checked_sub(BEFORE).map_or(start, |i| seen[i])
}

/// An address typed as a number or a symbol. A Rust symbol may be named as
/// it reads (`hello::main`) or by its last part (`main`) when that is unique.
fn place_address(place: &str, elf: Option<&hns_host::elf::Elf>) -> Result<u64, String> {
    if let Ok(addr) = parse_u64(place) {
        return Ok(addr);
    }
    let Some(elf) = elf else {
        return Err(format!(
            "`{place}` is not an address, and there is no ELF to look it up in. \
             Give an address, or the program with --elf."
        ));
    };
    let readable = |name: &str| format!("{:#}", rustc_demangle::demangle(name));
    let exact: Vec<_> = elf
        .symbols
        .iter()
        .filter(|s| s.name == place || readable(&s.name) == place)
        .collect();
    let found = if exact.is_empty() {
        elf.symbols
            .iter()
            .filter(|s| readable(&s.name).ends_with(&format!("::{place}")))
            .collect()
    } else {
        exact
    };
    let mut addrs: Vec<u64> = found.iter().map(|s| s.value).collect();
    addrs.dedup();
    match addrs.as_slice() {
        [addr] => Ok(*addr),
        [] => Err(format!("the ELF has no symbol `{place}`.")),
        _ => Err(format!(
            "`{place}` names several symbols: {}. Give the whole name.",
            found
                .iter()
                .map(|s| readable(&s.name))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// ` <name+0x1c>` for an address inside a symbol of the ELF, or nothing.
///
/// A sized symbol that holds the address wins; a label without a size only
/// names its own address. Assembler locals (`.L…`), mapping symbols (`$x`)
/// and plain numbers (`SHN_ABS`) name nothing a reader looks for.
fn symbolize(elf: Option<&hns_host::elf::Elf>, addr: u64) -> String {
    let Some(elf) = elf else {
        return String::new();
    };
    let named = || {
        elf.symbols
            .iter()
            .filter(|s| !s.name.is_empty() && !s.name.starts_with(['.', '$']) && !s.absolute)
    };
    let inside = named()
        .filter(|s| s.size > 0 && s.value <= addr && addr < s.value + s.size)
        .min_by_key(|s| s.size)
        .or_else(|| named().find(|s| s.value == addr));
    let Some(s) = inside else {
        return String::new();
    };
    // `{:#}` leaves out the hash that Rust adds to every name.
    let name = format!("{:#}", rustc_demangle::demangle(&s.name));
    match addr - s.value {
        0 => format!(" <{name}>"),
        off => format!(" <{name}+{off:#x}>"),
    }
}

/// Notices the hart halting by itself, at a breakpoint, while the console or
/// the prompt waits for keys. It reads `dmstatus` every so many idle rounds,
/// and only on a design with a Debug Module.
struct HaltWatch {
    base: Option<u32>,
    halted: bool,
    rounds: u32,
    /// Idle rounds between looks.
    every: u32,
    /// This halt was looked at for a semihosting call, and was not one.
    examined: bool,
}

impl HaltWatch {
    /// Idle rounds between looks; a round sleeps 5 to 10 ms. With `every`
    /// look, for a program that calls the host and waits on each answer.
    fn new(bus: &mut Bus, map: &RegisterMap, every: bool) -> Self {
        let mut watch = HaltWatch {
            base: dmi_base(map, None).ok(),
            halted: false,
            rounds: 0,
            every: if every { 1 } else { 20 },
            examined: false,
        };
        watch.sync(bus);
        watch
    }

    fn halted_now(bus: &mut Bus, base: u32) -> Result<bool, String> {
        use hns_host::dm::{Dmi, Op, reg};
        let status = WindowDmi {
            bus,
            base,
            pauses: Vec::new(),
        }
        .run(&[Op::Read(reg::DMSTATUS)])?[0];
        Ok(hns_host::dm::all_halted(status))
    }

    /// After a resume: a halt from here on is news, even one that came
    /// before the next look.
    fn running(&mut self) {
        self.halted = false;
    }

    /// Takes the state as it is now, so a halt someone typed is not news.
    fn sync(&mut self, bus: &mut Bus) {
        if let Some(base) = self.base
            && let Ok(halted) = Self::halted_now(bus, base)
        {
            self.halted = halted;
        }
    }

    /// True when the hart has halted since the last look.
    fn poll(&mut self, bus: &mut Bus) -> bool {
        self.look(bus).is_some_and(|news| news)
    }

    /// On a look: `Some(news)` while the hart is halted, where `news` says it
    /// halted since the last look. `None` while it runs, or between looks.
    fn look(&mut self, bus: &mut Bus) -> Option<bool> {
        let base = self.base?;
        self.rounds += 1;
        if !self.rounds.is_multiple_of(self.every) {
            return None;
        }
        let halted = Self::halted_now(bus, base).ok()?;
        let news = halted && !self.halted;
        self.halted = halted;
        if !halted {
            self.examined = false;
        }
        halted.then_some(news)
    }
}

/// Answers the semihosting call hart 0 is halted on, and lets it go on.
/// `None` when it is halted for another reason.
fn semihosting_call(
    mem: &mut WindowMem,
    started: std::time::Instant,
) -> Result<
    Option<(
        hns_host::semihosting::Outcome,
        hns_host::semihosting::Output,
    )>,
    String,
> {
    use hns_host::dm::{Debug, Reg};
    use hns_host::rtt::Memory;
    use hns_host::semihosting::{Outcome, Output, SEQUENCE, serve};

    let base = dmi_base(mem.map, None)?;
    let pc = Reg::parse("pc").expect("pc is a name");
    let (at, op, param, xlen) = {
        let mut dm = Debug::attach(WindowDmi::new(mem.bus, mem.map, base), 0)?;
        if dm.cause()? != 1 {
            return Ok(None);
        }
        let at = dm.read_reg(pc)?;
        (
            at,
            dm.read_reg(Reg::Gpr(10))?,
            dm.read_reg(Reg::Gpr(11))?,
            dm.xlen()?,
        )
    };
    let mut code = [0u8; 12];
    if at < 4 || mem.read(at - 4, &mut code).is_err() {
        return Ok(None);
    }
    let words: Vec<u32> = code
        .chunks(4)
        .map(|w| u32::from_le_bytes(w.try_into().expect("4 bytes")))
        .collect();
    if words != SEQUENCE {
        return Ok(None);
    }
    let mut said = Output::default();
    let outcome = serve(mem, op, param, xlen, started, &mut said)?;
    if let Outcome::Return(value) = outcome {
        let mut dm = Debug::attach(WindowDmi::new(mem.bus, mem.map, base), 0)?;
        dm.write_reg(Reg::Gpr(10), value)?;
        // On to the `srai` after the `ebreak`, which does nothing.
        dm.write_reg(pc, at + 4)?;
        dm.resume()?;
    }
    Ok(Some((outcome, said)))
}

/// "hart 0 halted at pc ... (why)" for hart 0.
fn stop_line(
    bus: &mut Bus,
    map: &RegisterMap,
    elf: Option<&hns_host::elf::Elf>,
) -> Result<String, String> {
    use hns_host::dm::Stop;
    use hns_host::dm::{Debug, Reg};
    let base = dmi_base(map, None)?;
    let mut dm = Debug::attach(WindowDmi::new(bus, map, base), 0)?;
    let pc = dm.read_reg(Reg::parse("pc").expect("pc is a name"))?;
    let xlen = dm.xlen()?;
    let at = |addr: u64| format!("{}{}", word(addr, xlen), symbolize(elf, addr));
    let why = match dm.stop()? {
        Stop::Trap { cause, mepc, mtval } => {
            return Ok(format!(
                "hart 0 trapped: {} at {} (mtval {}); halted at the handler, pc {}",
                hns_host::dm::exception_text(cause),
                at(mepc),
                word(mtval, xlen),
                at(pc)
            ));
        }
        Stop::Breakpoint => "breakpoint",
        Stop::Request => "halt request",
        Stop::Step => "single step",
        Stop::Ebreak => "ebreak",
        Stop::Reset => "halt at reset",
        Stop::Trigger => "trigger",
        Stop::Other(cause) => hns_host::dm::cause_text(cause),
    };
    let mut text = format!("hart 0 halted at pc {} ({why})", at(pc));
    if let Some(line) = pc_insn(bus, map, elf, pc, xlen) {
        text.push('\n');
        text.push_str(&line);
    }
    Ok(text)
}

/// "catching: ..." for the faults the exception trigger takes.
fn catching(causes: u64) -> String {
    if causes == 0 {
        return "not catching faults".to_string();
    }
    let names: Vec<&str> = (0..64)
        .filter(|c| causes & (1 << c) != 0)
        .map(hns_host::dm::exception_text)
        .collect();
    format!("catching: {}", names.join(", "))
}

/// The DMI registers of a `dmi` region, reached through the window.
struct WindowDmi<'a> {
    bus: &'a mut Bus,
    base: u32,
    /// The `pause` registers of the instruction traces, set while the hart
    /// is halted.
    pauses: Vec<u32>,
}

impl<'a> WindowDmi<'a> {
    fn new(bus: &'a mut Bus, map: &RegisterMap, base: u32) -> WindowDmi<'a> {
        WindowDmi {
            bus,
            base,
            pauses: trace_pauses(map),
        }
    }
}

/// The `pause` registers of the traces that record instructions (`rvfi`).
/// A trace of something else goes on while the hart is halted.
fn trace_pauses(map: &RegisterMap) -> Vec<u32> {
    map.traces
        .iter()
        .filter(|t| t.format.as_deref() == Some("rvfi"))
        .filter_map(|t| role_of(map, &t.name, "pause"))
        .map(|r| r.offset as u32)
        .collect()
}

impl hns_host::dm::Dmi for WindowDmi<'_> {
    fn hart_runs(&mut self, runs: bool) -> Result<(), String> {
        if self.pauses.is_empty() {
            return Ok(());
        }
        let mut b = Batch::new();
        for pause in &self.pauses {
            b.write(*pause, u32::from(!runs));
        }
        self.bus.run(&b).map(|_| ())
    }

    fn run(&mut self, ops: &[hns_host::dm::Op]) -> Result<Vec<u32>, String> {
        use hns_host::dm::Op;
        // Not replayable: a Debug Module read can change state.
        let mut b = Batch::new();
        let mut reads = Vec::new();
        for op in ops {
            match *op {
                Op::Read(reg) => reads.push(b.read(self.base + reg * 4)),
                Op::Write(reg, value) => {
                    b.write(self.base + reg * 4, value);
                }
            }
        }
        let got = self.bus.run(&b)?;
        Ok(reads.into_iter().map(|h| got[h]).collect())
    }
}

/// Where the Debug Module is: the region of kind `dmi`.
fn dmi_base(map: &RegisterMap, bundle: Option<&str>) -> Result<u32, String> {
    let found: Vec<_> = map
        .regions
        .iter()
        .filter(|r| r.kind.as_deref() == Some("dmi"))
        .collect();
    let names = || {
        found
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let region = match (bundle, found.as_slice()) {
        (_, []) => {
            return Err(
                "this design has no Debug Module: regs.json has no region of kind `dmi`.\n\
                 Give the DUT's DMI a bundle with `backing = \"dmi\"`, then generate and \
                 synthesize again."
                    .to_string(),
            );
        }
        (None, [only]) => *only,
        (None, _) => {
            return Err(format!(
                "this design has several Debug Modules ({}). Name one with --dmi.",
                names()
            ));
        }
        (Some(name), _) => *found
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| format!("no Debug Module is called `{name}`. There is: {}.", names()))?,
    };
    Ok(region.window_base as u32)
}

/// What `cpu ipc` prints about one run, without the PC.
fn ipc_report(m: &hns_host::dm::Measured, secs: f64) -> String {
    let mut out = format!("cycles    {}\ninstret   {}\n", m.cycles, m.instret);
    if m.cycles > 0 && m.instret > 0 {
        out.push_str(&format!(
            "IPC       {:.3}  (CPI {:.1})\n",
            m.instret as f64 / m.cycles as f64,
            m.cycles as f64 / m.instret as f64
        ));
    } else {
        out.push_str(
            "IPC       -  (a counter did not move: is the hart in wfi, or does it lack the counters?)\n",
        );
    }
    out.push_str(&format!(
        "over      {secs:.2} s, about {:.1} MHz, {:.2} MIPS",
        m.cycles as f64 / secs / 1e6,
        m.instret as f64 / secs / 1e6
    ));
    if m.inhibit & 0b101 != 0 {
        out.push_str(&format!(
            "\nnote      mcountinhibit was {:#x}; lifted for the run, then put back",
            m.inhibit
        ));
    }
    out
}

/// The `cpu` commands that talk to the Debug Module.
fn cpu_debug(
    bus: &mut Bus,
    map: &RegisterMap,
    cmd: &CpuCmd,
    elf: Option<&hns_host::elf::Elf>,
) -> Result<(), String> {
    use hns_host::dm::{Debug, Reg, reg};

    let args = match cmd {
        CpuCmd::Boot { .. } | CpuCmd::Console { .. } => {
            unreachable!("boot and console are run elsewhere")
        }
        CpuCmd::Info { hart }
        | CpuCmd::Dmi { hart, .. }
        | CpuCmd::Halt { hart }
        | CpuCmd::Resume { hart }
        | CpuCmd::Ipc { hart, .. }
        | CpuCmd::Step { hart, .. }
        | CpuCmd::Regs { hart, .. }
        | CpuCmd::Break { hart, .. }
        | CpuCmd::Mem { hart, .. }
        | CpuCmd::Disasm { hart, .. }
        | CpuCmd::Catch { hart, .. }
        | CpuCmd::Reset { hart, .. } => hart,
    };
    let loaded;
    let elf = match (elf, &args.elf) {
        (Some(elf), _) => Some(elf),
        (None, Some(path)) => {
            let raw = std::fs::read(path).map_err(|e| read_error(path, e))?;
            loaded = hns_host::elf::Elf::parse(&raw)?;
            Some(&loaded)
        }
        (None, None) => None,
    };
    // Memory goes through the window, not the Debug Module.
    if let CpuCmd::Mem {
        place,
        words,
        write,
        ..
    } = cmd
    {
        return mem(bus, map, elf, place, *words, write);
    }
    // A listing of a named place needs only the memory and the ELF's width,
    // so it works on a CPU without a Debug Module too.
    let base = match (dmi_base(map, args.dmi.as_deref()), cmd, elf) {
        (Ok(base), _, _) => base,
        (
            Err(_),
            CpuCmd::Disasm {
                place: Some(place),
                count,
                ..
            },
            Some(e),
        ) => {
            let mut mem = WindowMem {
                bus,
                map,
                spans: spans(map)?,
            };
            let (from, n, end) = disasm_range(place, *count, Some(e))?;
            let xlen = if e.wide { 64 } else { 32 };
            for line in disasm_lines(&mut mem, Some(e), from, n, end, xlen, None)? {
                println!("{line}");
            }
            return Ok(());
        }
        (Err(e), _, _) => return Err(e),
    };
    let window = WindowDmi::new(bus, map, base);

    // Raw access needs no attach: it is for when the debugger cannot.
    if let CpuCmd::Dmi {
        reg: name, value, ..
    } = cmd
    {
        let mut window = window;
        let n = reg::parse(name).ok_or_else(|| {
            let names: Vec<&str> = reg::NAMES.iter().map(|(n, _)| *n).collect();
            format!(
                "`{name}` is not a DMI register. Give a number below 0x80, or one of: {}.",
                names.join(", ")
            )
        })?;
        use hns_host::dm::{Dmi, Op};
        match value {
            Some(v) => {
                window.run(&[Op::Write(n, *v)])?;
                println!("{name} ({n:#04x}) <- {v:#010x}");
            }
            None => {
                let v = window.run(&[Op::Read(n)])?[0];
                println!("{name} ({n:#04x}) = {v:#010x}");
            }
        }
        return Ok(());
    }

    let mut dm = Debug::attach(window, args.hart)?;
    let hart = dm.hart();
    // "pc 0x... (cause)" for a halted hart.
    // With the instruction there on a second line, when a memory holds it.
    let stopped = |dm: &mut Debug<WindowDmi>| -> Result<String, String> {
        let pc = dm.read_reg(Reg::parse("pc").expect("pc is a name"))?;
        let cause = hns_host::dm::cause_text(dm.cause()?);
        let xlen = dm.xlen()?;
        let mut text = format!("pc {}{} ({cause})", word(pc, xlen), symbolize(elf, pc));
        if let Some(line) = pc_insn(dm.dmi_mut().bus, map, elf, pc, xlen) {
            text.push('\n');
            text.push_str(&line);
        }
        Ok(text)
    };
    match cmd {
        CpuCmd::Info { .. } => {
            println!("{}", dm.info()?);
            if dm.is_halted()? {
                let misa = dm.read_reg(Reg::parse("misa").expect("misa is a name"))?;
                let xlen = dm.xlen()?;
                let letters: String = ('A'..='Z')
                    .filter(|c| misa & (1 << (*c as u32 - 'A' as u32)) != 0)
                    .collect();
                println!(
                    "isa           RV{xlen}{letters} (misa {})",
                    word(misa, xlen)
                );
                let triggers = dm.triggers()?;
                if triggers.is_empty() {
                    println!("triggers      none");
                }
                for t in triggers {
                    println!("trigger {:<5} {}", t.index, t.type_names().join(", "));
                }
            } else {
                println!("(halt the hart to see its ISA and triggers: `hio cpu halt`)");
            }
        }
        CpuCmd::Halt { .. } => {
            dm.halt()?;
            println!("hart {hart} halted at {}", stopped(&mut dm)?);
        }
        CpuCmd::Resume { .. } => {
            dm.resume()?;
            println!("hart {hart} running");
        }
        CpuCmd::Ipc { time, .. } => {
            if !(time.is_finite() && *time > 0.0) {
                return Err(format!(
                    "--time {time} is not a time: give seconds above 0."
                ));
            }
            let run = std::time::Duration::from_secs_f64(*time);
            let (m, took) = dm.measure(|_| {
                let start = std::time::Instant::now();
                std::thread::sleep(run);
                start.elapsed()
            })?;
            let xlen = dm.xlen()?;
            println!("{}", ipc_report(&m, took.as_secs_f64()));
            println!("last pc   {}{}", word(m.pc, xlen), symbolize(elf, m.pc));
        }
        CpuCmd::Step { count, .. } => {
            for _ in 0..*count {
                dm.step()?;
                println!("{}", stopped(&mut dm)?);
            }
        }
        CpuCmd::Regs { name: None, .. } => {
            let xlen = dm.xlen()?;
            let mut line = String::new();
            for n in 0..32 {
                let v = dm.read_reg(Reg::Gpr(n))?;
                line.push_str(&format!(
                    "{:>4} {:<4} {}  ",
                    format!("x{n}"),
                    hns_host::dm::GPR_NAMES[n as usize],
                    word(v, xlen)
                ));
                if n % 4 == 3 {
                    println!("{}", line.trim_end());
                    line.clear();
                }
            }
            for (name, csr) in hns_host::dm::CSR_NAMES {
                match dm.read_reg(Reg::Csr(*csr)) {
                    Ok(v) => println!("{name:<9} {}", word(v, xlen)),
                    Err(e) => println!("{name:<9} -  ({e})"),
                }
            }
            println!("halted at {}", stopped(&mut dm)?);
        }
        CpuCmd::Regs {
            name: Some(name),
            value,
            ..
        } => {
            let r = Reg::parse(name).ok_or_else(|| {
                format!(
                    "`{name}` is not a register. Give `pc`, a GPR (`a0`, `x10`), \
                     or a CSR by name (`mstatus`) or number (`0x300`)."
                )
            })?;
            if let Some(v) = value {
                dm.write_reg(r, *v)?;
            }
            let v = dm.read_reg(r)?;
            println!("{r} = {}", word(v, dm.xlen()?));
        }
        CpuCmd::Break { place, clear, .. } => {
            let addr = match place {
                Some(place) => Some(place_address(place, elf)?),
                None => None,
            };
            // The triggers are reached first: on a running hart that halts it
            // for a moment, which is also when XLEN can be learnt.
            let (listed, cleared) = match (addr, *clear) {
                (Some(addr), false) => (vec![dm.set_breakpoint(addr)?], None),
                (addr, true) => (Vec::new(), Some((addr, dm.clear_breakpoints(addr)?))),
                (None, false) => (dm.breakpoints()?, None),
            };
            let xlen = dm.xlen()?;
            match cleared {
                Some((Some(_), 0)) => return Err("no breakpoint is set there.".to_string()),
                Some((_, n)) => println!("cleared {n} breakpoint{}", if n == 1 { "" } else { "s" }),
                None if listed.is_empty() => println!("no breakpoints"),
                None => {
                    for b in &listed {
                        println!(
                            "breakpoint {} at {}{}",
                            b.index,
                            word(b.addr, xlen),
                            symbolize(elf, b.addr)
                        );
                    }
                }
            }
        }
        CpuCmd::Catch { clear, .. } => {
            let causes = if *clear { 0 } else { hns_host::dm::FAULTS };
            println!("{}", catching(dm.catch_exceptions(causes)?));
        }
        CpuCmd::Reset { halt, .. } => {
            dm.reset(*halt)?;
            if *halt {
                println!("hart {hart} reset and halted at {}", stopped(&mut dm)?);
            } else {
                println!("hart {hart} reset and running");
            }
        }
        CpuCmd::Disasm { place, count, .. } => {
            let pc = match dm.is_halted()? {
                true => Some(dm.read_reg(Reg::parse("pc").expect("pc is a name"))?),
                false => None,
            };
            let xlen = match (elf, pc) {
                (Some(e), _) => {
                    if e.wide {
                        64
                    } else {
                        32
                    }
                }
                (None, Some(_)) => dm.xlen()?,
                (None, None) => {
                    return Err(
                        "the hart is running, and without the ELF hio does not know \
                                whether it is RV32 or RV64. Halt it, or give the program \
                                with --elf."
                            .to_string(),
                    );
                }
            };
            let mut mem = WindowMem {
                bus: dm.dmi_mut().bus,
                map,
                spans: spans(map)?,
            };
            let (from, n, end) = match place {
                Some(place) => disasm_range(place, *count, elf)?,
                None => {
                    let pc = pc.ok_or(
                        "the hart is running. Halt it to see the code around the pc, \
                         or give a place.",
                    )?;
                    let n = count.map_or(12, |n| n as usize);
                    (disasm_start(&mut mem, elf, pc), n, None)
                }
            };
            for line in disasm_lines(&mut mem, elf, from, n, end, xlen, pc)? {
                println!("{line}");
            }
        }
        CpuCmd::Boot { .. } | CpuCmd::Console { .. } | CpuCmd::Dmi { .. } | CpuCmd::Mem { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

/// A register value at the hart's width.
fn word(value: u64, xlen: u32) -> String {
    if xlen == 64 {
        format!("{value:#018x}")
    } else {
        format!("{value:#010x}")
    }
}

/// The terminal in raw mode while it lives: keys arrive one at a time and are
/// not echoed. Without a terminal (a pipe, a test), it does nothing and reads
/// stdin as it comes.
struct RawTerminal {
    saved: Option<libc::termios>,
    raw: Option<libc::termios>,
}

impl RawTerminal {
    fn enter() -> RawTerminal {
        // SAFETY: plain calls on fd 0 with a zeroed termios they fill in.
        unsafe {
            let none = RawTerminal {
                saved: None,
                raw: None,
            };
            if libc::isatty(0) == 0 {
                return none;
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return none;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSAFLUSH, &raw) != 0 {
                return none;
            }
            RawTerminal {
                saved: Some(saved),
                raw: Some(raw),
            }
        }
    }

    fn active(&self) -> bool {
        self.saved.is_some()
    }

    /// The terminal as it was, for a while. Keys typed meanwhile are kept.
    fn pause(&self) {
        if let Some(saved) = &self.saved {
            // SAFETY: a termios that `enter` read from the same fd.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, saved);
            }
        }
    }

    /// Raw again after `pause`.
    fn resume(&self) {
        if let Some(raw) = &self.raw {
            // SAFETY: a termios that `enter` made for the same fd.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, raw);
            }
        }
    }

    /// The bytes waiting on stdin, without blocking.
    fn keys(&self) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut fds = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd, no timeout.
            let ready = unsafe { libc::poll(&mut fds, 1, 0) };
            if ready <= 0 || fds.revents & libc::POLLIN == 0 {
                return out;
            }
            let mut buf = [0u8; 64];
            // SAFETY: reads into a local buffer of the given length.
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved {
            // SAFETY: restores what `enter` read from the same fd.
            unsafe {
                libc::tcsetattr(0, libc::TCSAFLUSH, saved);
            }
        }
    }
}

/// Takes entries out of a `host_poll_fifo`, oldest first.
///
/// It needs no knowledge of the DUT, only the roles `data` / `level` /
/// `pop`. Reading does not remove an entry; each write to `pop` does.
fn drain(bus: &mut Bus, map: &RegisterMap, bundle: &str, max: Option<usize>) -> Result<(), String> {
    let bundles: Vec<&str> = {
        let mut v: Vec<&str> = map
            .registers
            .iter()
            .filter(|r| r.role.as_deref() == Some("data"))
            .filter_map(|r| r.bundle.as_deref())
            .collect();
        v.dedup();
        v
    };
    let missing = |what: &str| {
        format!(
            "`{bundle}` has no `{what}` register, so it cannot be drained.\n\
             Draining needs a bundle backed by `host_poll_fifo`. Bundles that have one \
             here: {}.",
            if bundles.is_empty() {
                "none".to_string()
            } else {
                bundles.join(", ")
            }
        )
    };
    let data = role_of(map, bundle, "data").ok_or_else(|| missing("data"))?;
    let level = role_of(map, bundle, "level").ok_or_else(|| missing("level"))?;
    let pop = role_of(map, bundle, "pop").ok_or_else(|| missing("pop"))?;

    if let Some(d) = role_of(map, bundle, "drops") {
        let n = rd(bus, d.offset as u32)?;
        if n != 0 {
            // Dropped entries never come out, so the drained count alone
            // cannot show the gap.
            eprintln!("warning: {bundle} has dropped {n} entries since reset");
        }
    }

    let have = rd(bus, level.offset as u32)? as usize;
    let want = max.map_or(have, |m| m.min(have));
    for i in 0..want {
        // One entry's reads and its `pop` share one round trip. Do not batch
        // across entries: a lost batch would lose FIFO contents.
        let mut b = Batch::new();
        let handles: Vec<_> = (0..data.words)
            .map(|w| b.read(data.offset as u32 + 4 * w as u32))
            .collect();
        b.write(pop.offset as u32, 1);
        let got = bus.run(&b)?;
        let words: Vec<u32> = handles.into_iter().map(|h| got[h]).collect();
        println!("{i:5}  {}", show(&words));
    }
    if want < have {
        println!("({} left)", have - want);
    }
    if want == 0 {
        // Print something, or in a script it looks like the line never ran.
        println!("{bundle}: empty");
    }
    Ok(())
}

/// `hio trace`: starts, stops, and reads the ring of a `trace` bundle.
fn trace(bus: &mut Bus, map: &RegisterMap, cmd: &TraceCmd) -> Result<(), String> {
    let (TraceCmd::Start { bundle, .. }
    | TraceCmd::Stop { bundle }
    | TraceCmd::Status { bundle }
    | TraceCmd::Dump { bundle, .. }) = cmd;
    let names: Vec<&str> = map.traces.iter().map(|t| t.name.as_str()).collect();
    let layout = match (bundle, names.as_slice()) {
        (_, []) => {
            return Err("this design has no trace: no bundle in Harness.toml has                  `backing = \"trace\"`."
                .to_string());
        }
        (Some(name), _) => map.traces.iter().find(|t| &t.name == name).ok_or_else(|| {
            format!(
                "`{name}` is not a trace. Traces here: {}.",
                names.join(", ")
            )
        })?,
        (None, [_]) => &map.traces[0],
        // The CPU's own, when it is the only instruction trace.
        (None, many) => {
            let mut rvfi = map
                .traces
                .iter()
                .filter(|t| t.format.as_deref() == Some("rvfi"));
            match (rvfi.next(), rvfi.next()) {
                (Some(only), None) => only,
                _ => return Err(format!("say which trace: {}.", many.join(", "))),
            }
        }
    };
    let name = layout.name.as_str();
    let reg = |role: &str| {
        role_of(map, name, role).ok_or_else(|| {
            format!("regs.json has no `{name}_{role}`. Regenerate it with this veryl-harness.")
        })
    };
    let run = reg("run")?.offset as u32;
    let pause = reg("pause")?.offset as u32;
    let arm = reg("arm")?.offset as u32;
    let after_reg = reg("after")?.offset as u32;
    let hit = reg("hit")?.offset as u32;
    let hit_at = reg("hit_at")?.offset as u32;
    let count = reg("count")?.offset as u32;
    let wrapped = reg("wrapped")?.offset as u32;
    let region = map
        .region(name)
        .ok_or_else(|| format!("regs.json has no region `{name}`. Regenerate it."))?;
    let depth = region.depth as u32;

    match cmd {
        TraceCmd::Start {
            when, now, after, ..
        } => {
            let armed = *now || !when.is_empty();
            if after.is_some() && !armed {
                return Err(
                    "`--after` counts from a trigger: give `--when` or `--now` too.".to_string(),
                );
            }
            let after = after.unwrap_or(0);
            if after >= depth {
                return Err(format!(
                    "`--after {after}` keeps more than the ring holds ({depth}), so the trigger \
                     would be overwritten. Give at most {}.",
                    depth - 1
                ));
            }
            let (mask, value) = hns_host::trace::trigger_words(layout, when)?;
            // 0 then 1: a rise starts over, also when it is recording already.
            let mut b = Batch::new();
            b.write(run, 0);
            b.write(pause, 0);
            b.write(arm, u32::from(armed));
            b.write(after_reg, after);
            if let (Some(m), Some(v)) = (role_of(map, name, "tmask"), role_of(map, name, "tvalue"))
            {
                for (i, (mw, vw)) in mask.iter().zip(&value).enumerate() {
                    b.write(m.offset as u32 + 4 * i as u32, *mw);
                    b.write(v.offset as u32 + 4 * i as u32, *vw);
                }
            }
            b.write(run, 1);
            bus.run(&b)?;
            let how = if *now {
                format!(", triggering on the first beat and keeping {after} after it")
            } else if !when.is_empty() {
                format!(", until {} and {after} after", when.join(" and "))
            } else {
                String::new()
            };
            println!("{name}: recording{how}");
            Ok(())
        }
        TraceCmd::Stop { .. } => {
            let mut b = Batch::new();
            b.write(run, 0);
            bus.run(&b)?;
            println!("{name}: stopped");
            Ok(())
        }
        TraceCmd::Status { .. } => {
            let (on, paused) = (rd(bus, run)?, rd(bus, pause)?);
            let (n, full) = (rd(bus, count)?, rd(bus, wrapped)? == 1);
            // Exactly `depth` records fill the ring without overwriting one.
            let held = match (full, n == depth) {
                (false, _) => n.to_string(),
                (true, true) => format!("{depth} (full)"),
                (true, false) => format!("{depth} (full: older ones were overwritten)"),
            };
            let trigger = if rd(bus, hit)? == 1 {
                let (at, kept) = (rd(bus, hit_at)?, rd(bus, after_reg)?);
                let since = n.wrapping_sub(at);
                Some((at, kept, since > kept, since))
            } else {
                None
            };
            let state = match (on, paused, trigger) {
                (0, _, _) => "stopped",
                (_, _, Some((_, _, true, _))) => "frozen after the trigger",
                (_, 0, _) => "recording",
                _ => "paused while the hart is halted",
            };
            println!("{name}: {state}, {n} records since the start, {held} in the ring of {depth}");
            match trigger {
                Some((at, kept, true, _)) => {
                    println!("{name}: triggered at record {at}, and kept {kept} after it")
                }
                Some((at, kept, false, since)) => println!(
                    "{name}: triggered at record {at}; {} of {kept} after it so far",
                    since - 1
                ),
                None if rd(bus, arm)? == 1 => println!("{name}: waiting for the trigger"),
                None => {}
            }
            Ok(())
        }
        TraceCmd::Dump { last, .. } => {
            // Recording is fine while nothing arrives, as on a halted hart.
            // Whether something did is told by the count, read again after.
            let (n, full) = (rd(bus, count)?, rd(bus, wrapped)? == 1);
            let mut picked = hns_host::trace::entries(n, full, depth);
            if let Some(last) = last {
                picked.drain(..picked.len().saturating_sub(*last));
            }
            // Only the words that hold the record; the window pads an entry
            // to a power of two of words.
            let words = layout.record_bits.div_ceil(32);
            let mut printer = hns_host::trace::Printer::new(layout);
            // The record number of each line, to mark the trigger.
            let trigger = (rd(bus, hit)? == 1).then_some(rd(bus, hit_at)?);
            let mut seq = n.wrapping_sub(picked.len() as u32);
            for chunk in picked.chunks((1024 / words).max(1)) {
                let mut b = region_batch(region);
                let handles: Vec<Vec<_>> = chunk
                    .iter()
                    .map(|entry| {
                        let base = region.window_base + *entry as usize * region.entry_bytes;
                        (0..words).map(|w| b.read((base + 4 * w) as u32)).collect()
                    })
                    .collect();
                let got = bus.run(&b)?;
                for record in handles {
                    let record: Vec<u32> = record.into_iter().map(|h| got[h]).collect();
                    let mark = if trigger == Some(seq) {
                        "  <= trigger"
                    } else {
                        ""
                    };
                    println!("{}{mark}", printer.line(&record));
                    seq = seq.wrapping_add(1);
                }
            }
            if picked.is_empty() {
                println!("{name}: empty");
            } else if full && n != depth {
                println!(
                    "({} of {n} records; the older ones were overwritten)",
                    picked.len()
                );
            }
            let later = rd(bus, count)?;
            if later != n {
                eprintln!(
                    "warning: {} records arrived while the ring was read, so some lines may be \
                     newer than the ones around them. Stop it first (`hio trace stop {name}`), \
                     or halt the CPU.",
                    later.wrapping_sub(n)
                );
            }
            Ok(())
        }
    }
}

/// Reads every register in one round trip. A round trip per register is slow,
/// because a JTAG round trip is expensive.
fn dump(bus: &mut Bus, map: &RegisterMap) -> Result<(), String> {
    let width = map
        .registers
        .iter()
        .map(|r| r.name.len())
        .chain(map.regions.iter().map(|region| region.name.len()))
        .max()
        .unwrap_or(4);

    // List regions too, with how to read them. Their contents are not read
    // (too many round trips), but without the names the window shows
    // unexplained holes.
    for region in &map.regions {
        println!(
            "{:#06x}  {:<width$}  region, {} ({} x {} bytes) -- `hio read {} 0x0 {}`",
            region.window_base,
            region.name,
            size_str(region.depth * region.entry_bytes as u64),
            size_str(region.depth),
            region.entry_bytes,
            region.name,
            region.depth.min(8),
            width = width
        );
    }
    if !map.regions.is_empty() {
        println!();
    }

    let mut b = Batch::new();
    let handles: Vec<Vec<_>> = map
        .registers
        .iter()
        .map(|r| {
            (0..r.words)
                .map(|i| b.read(r.offset as u32 + 4 * i as u32))
                .collect()
        })
        .collect();
    let got = bus.run(&b)?;

    for (r, hs) in map.registers.iter().zip(handles) {
        let words: Vec<u32> = hs.into_iter().map(|h| got[h]).collect();
        println!(
            "{:#06x}  {:<width$}  {}{}",
            r.offset,
            r.name,
            show(&words),
            size_note(r, &words),
            width = width
        );
    }
    Ok(())
}

/// Adds a readable size to a size register (`(512M entries)`, `(2M)`).
///
/// The hex value stays, for pasting. Size registers are found by role, not
/// by name; by name, addresses and the magic would get an `M` too.
fn size_note(r: &hns_regs::Register, words: &[u32]) -> String {
    let value = words
        .iter()
        .rev()
        .fold(0u64, |acc, w| (acc << 32) | u64::from(*w));
    match r.role.as_deref() {
        Some("depth") => format!("  ({} entries)", size_str(value)),
        Some("dma_size" | "dma_len") => format!("  ({})", size_str(value)),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_with(serial: &str) -> hns_host::ftdi::Found {
        hns_host::ftdi::Found {
            vid: 0x0403,
            pid: 0x6010,
            product: None,
            serial: Some(serial.to_string()),
        }
    }

    #[test]
    fn a_part_of_a_serial_picks_the_one_probe_that_has_it() {
        let serials = |found: Vec<hns_host::ftdi::Found>| -> Vec<String> {
            found.into_iter().filter_map(|f| f.serial).collect()
        };
        let plugged = || {
            vec![
                probe_with("210319788107"),
                probe_with("210319A8F2D1"),
                probe_with("8107"),
            ]
        };
        assert_eq!(serials(by_serial(plugged(), "8F2D")), ["210319A8F2D1"]);
        // Two have `2103`, so it does not choose.
        assert_eq!(by_serial(plugged(), "2103").len(), 2);
        // `8107` is a whole serial, and a part of another: the whole one wins.
        assert_eq!(serials(by_serial(plugged(), "8107")), ["8107"]);
        assert!(by_serial(plugged(), "9999").is_empty());
    }
    use clap::CommandFactory;

    /// Wide values go out lowest word first. Reversed, a descriptor address
    /// would have its halves swapped, and with the IOMMU in identity mode the
    /// card would write to another physical address.
    #[test]
    fn a_wide_value_goes_out_low_word_first() {
        assert_eq!(
            wide_words(0x1122_3344_5566_7788, 2),
            vec![0x5566_7788, 0x1122_3344]
        );
        assert_eq!(wide_words(0x1234, 1), vec![0x1234]);
    }

    fn jtag(tdo: &[u8], tdi: &[u8]) -> hns_targets::Jtag {
        hns_targets::Jtag {
            chain_tdo_side: tdo.to_vec(),
            chain_tdi_side: tdi.to_vec(),
            ..Default::default()
        }
    }

    fn dap(tdi_side: bool, index: usize) -> hns_targets::Dap {
        hns_targets::Dap {
            tdi_side,
            index,
            mem_ap: None,
        }
    }

    /// A Zynq-7000: TDI -> DAP -> PL -> TDO. Talking to the DAP, the PL is
    /// the one TAP on its TDO side.
    #[test]
    fn the_dap_of_a_zynq_has_the_fpga_on_its_tdo_side() {
        assert_eq!(
            dap_sides(&jtag(&[], &[4]), &dap(true, 0), 6),
            Ok((vec![6], vec![]))
        );
    }

    /// Lists count from the FPGA outwards, on either side.
    #[test]
    fn the_other_taps_move_to_the_right_side_of_the_dap() {
        let j = jtag(&[8], &[5, 4, 7]);
        assert_eq!(
            dap_sides(&j, &dap(true, 1), 6),
            Ok((vec![8, 6, 5], vec![7]))
        );
        let j = jtag(&[5, 4], &[7]);
        assert_eq!(
            dap_sides(&j, &dap(false, 1), 6),
            Ok((vec![], vec![7, 6, 5]))
        );
    }

    #[test]
    fn a_dap_that_is_not_there_or_not_a_dap_is_refused() {
        assert!(dap_sides(&jtag(&[], &[4]), &dap(true, 1), 6).is_err());
        let err = dap_sides(&jtag(&[], &[5]), &dap(true, 0), 6).unwrap_err();
        assert!(err.contains("4"), "{err}");
    }

    fn span(base: u64, bytes: u64) -> Span {
        Span {
            bundle: "mem".to_string(),
            base,
            bytes,
            dut_writes: true,
        }
    }

    fn segment(paddr: u64, file: usize, memsz: u64) -> hns_host::elf::Segment {
        hns_host::elf::Segment {
            paddr,
            memsz,
            bytes: vec![0xaa; file],
        }
    }

    /// riscv-tests `-v`: `.data` with `.bss` after it in one segment. The
    /// tail is cleared, up to where the smallest memory it goes to ends.
    #[test]
    fn the_bss_after_the_data_is_cleared() {
        let mem = span(0x8000_0000, 0x10_0000);
        let seg = segment(0x8000_1000, 0x2124, 0x8010);
        assert_eq!(zero_fill(&seg, &[&mem]), 0x8010 - 0x2124);
        let small = span(0x8000_0000, 0x4000);
        assert_eq!(zero_fill(&seg, &[&mem, &small]), 0x4000 - 0x3124);
        // Nothing past the file bytes: nothing to clear.
        assert_eq!(zero_fill(&segment(0x8000_0000, 0x100, 0x100), &[&mem]), 0);
    }

    /// A segment that is all zero-fill (a Rust firmware's stack can be tens
    /// of megabytes) is left alone: its startup code clears what it needs.
    #[test]
    fn a_segment_with_no_file_bytes_is_not_cleared() {
        let mem = span(0x8000_0000, 0x200_0000);
        assert_eq!(zero_fill(&segment(0x8000_24e8, 0, 0x1ff_db18), &[&mem]), 0);
    }

    #[test]
    fn a_timeout_is_given_in_seconds() {
        assert_eq!(parse_seconds("30"), Ok(std::time::Duration::from_secs(30)));
        assert_eq!(
            parse_seconds("0.5"),
            Ok(std::time::Duration::from_millis(500))
        );
        assert!(parse_seconds("-1").is_err());
        assert!(parse_seconds("ten").is_err());
    }

    /// riscv-tests' convention: 1 passes, an odd value fails with value >> 1,
    /// and an even one is a system call this tool cannot serve.
    #[test]
    fn tohost_says_pass_or_the_failing_code() {
        assert_eq!(tohost_status(0), Ok(None));
        assert_eq!(tohost_status(1), Ok(Some(0)));
        assert_eq!(tohost_status((5 << 1) | 1), Ok(Some(5)));
        assert!(tohost_status(0x8000_1000).unwrap_err().contains("HTIF"));
    }

    /// A map without a requester is refused, and the error names the fix.
    #[test]
    fn a_design_without_a_requester_says_what_it_would_take() {
        let map = RegisterMap::from_json(
            r#"{"marker":"m","format_version":7,"generator":"g","dut":"d","word_bits":32,
                "size_bytes":8,"magic":1,"map_hash":2,"registers":[],"regions":[]}"#,
        )
        .unwrap();
        let err = require_requester(&map).unwrap_err();
        assert!(err.contains("--transport pcie"), "{err}");
        assert!(err.contains("dram"), "{err}");
    }

    /// A map of a design with a DMA engine. `requester` fills that field;
    /// `None` leaves the field out.
    fn requester_map(requester: Option<&str>) -> RegisterMap {
        let pcie = match requester {
            Some(name) => format!(
                r#","pcie":{{"vendor_id":1,"device_id":2,"bar_bytes":4096,"requester":"{name}"}}"#
            ),
            None => r#","pcie":{"vendor_id":1,"device_id":2,"bar_bytes":4096}"#.to_string(),
        };
        let region = |name: &str, base: usize| {
            format!(
                r#"{{"name":"{name}","kind":"memory","window_base":{base},"size_bytes":1024,
                    "entry_bytes":4,"depth":256}}"#
            )
        };
        RegisterMap::from_json(&format!(
            r#"{{"marker":"m","format_version":{},"generator":"g","dut":"d","word_bits":32,
                "size_bytes":4096,"magic":1,"map_hash":2,
                "registers":[{{"name":"dma_base","kind":"terminator","offset":16,"words":2,
                  "width":64,"access":"rw","role":"dma_base"}}],
                "regions":[{},{}]{pcie}}}"#,
            hns_regs::FORMAT_VERSION,
            region("mem", 1024),
            region("scratch", 2048),
        ))
        .unwrap()
    }

    /// Types `keys` into a fresh editor; the lines it finished, and the echo.
    fn typed(edit: &mut LineEdit, keys: &[u8]) -> (Vec<String>, Vec<u8>) {
        let (mut lines, mut echo) = (Vec::new(), Vec::new());
        for &k in keys {
            if let Some(line) = edit.key(k, &mut echo) {
                lines.push(line);
            }
        }
        (lines, echo)
    }

    /// The prompt's line editing: echo, Backspace, dropping a line, and
    /// going back to earlier lines with the arrows.
    #[test]
    fn the_prompt_edits_and_recalls_lines() {
        let mut edit = LineEdit::default();
        let (lines, echo) = typed(&mut edit, b"haltt\x7f\r");
        assert_eq!(lines, ["halt"]);
        assert_eq!(echo, b"haltt\x08 \x08\n");

        // Ctrl-C drops what was typed; the next line starts clean.
        let (lines, _) = typed(&mut edit, b"regs\x03step 2\r");
        assert_eq!(lines, ["step 2"]);

        // Up twice gives the line before the last one.
        let (lines, _) = typed(&mut edit, b"\x1b[A\x1b[A\r");
        assert_eq!(lines, ["halt"]);
        // Down past the newest gives an empty line again.
        let (lines, _) = typed(&mut edit, b"\x1b[A\x1b[B\r");
        assert_eq!(lines, [""]);
        assert_eq!(edit.history, ["halt", "step 2", "halt"]);
    }

    /// At `(hio)` a line is a procedure-file line; at `(hio cpu)` it is a
    /// `hio cpu` command without `cpu`. `cpu` and `up` move between them.
    #[test]
    fn the_prompt_reads_lines_by_level() {
        let top = |line| prompt_action(Level::Top, false, line);
        let cpu = |line| prompt_action(Level::Cpu, false, line);
        assert!(matches!(
            top("reset --hold"),
            Ok(PromptAction::Hio(Step::Window(Cmd::Reset {
                hold: true,
                ..
            })))
        ));
        assert!(matches!(
            top("cpu halt"),
            Ok(PromptAction::Cpu(CpuCmd::Halt { .. }))
        ));
        assert!(matches!(top("cpu"), Ok(PromptAction::Enter(Level::Cpu))));
        assert!(matches!(
            cpu("step 3"),
            Ok(PromptAction::Cpu(CpuCmd::Step { count: 3, .. }))
        ));
        // `reset` is the CPU's at `(hio cpu)`, the DUT's at `(hio)`.
        assert!(matches!(
            cpu("reset --halt"),
            Ok(PromptAction::Cpu(CpuCmd::Reset { halt: true, .. }))
        ));
        assert!(matches!(cpu("up"), Ok(PromptAction::Enter(Level::Top))));
        // The trace is the same command at both levels.
        for line in ["trace dump --last 5", "cpu trace dump --last 5"] {
            assert!(
                matches!(
                    top(line).or_else(|_| cpu(line)),
                    Ok(PromptAction::Hio(Step::Window(Cmd::Trace(
                        TraceCmd::Dump { last: Some(5), .. }
                    ))))
                ),
                "{line}"
            );
        }
        assert!(matches!(
            cpu("trace stop"),
            Ok(PromptAction::Hio(Step::Window(Cmd::Trace(
                TraceCmd::Stop { .. }
            ))))
        ));
        assert!(matches!(top("   "), Ok(PromptAction::Nothing)));
        assert!(matches!(top("help"), Ok(PromptAction::Say(_))));
        assert!(matches!(top("help read"), Ok(PromptAction::Say(_))));
        assert!(matches!(cpu("help"), Ok(PromptAction::Say(_))));
        assert!(matches!(top("exit"), Ok(PromptAction::Quit)));

        let Err(err) = cpu("read dmem 0x100") else {
            panic!("a top-level command was taken at (hio cpu)");
        };
        assert!(err.contains("`hio <line>`"), "{err}");
        let Err(err) = top("cpu boot a.elf") else {
            panic!("boot was taken at the prompt");
        };
        assert!(err.contains("on its own"), "{err}");

        // `hio` is a level's name too, and a prefix for one line.
        assert!(matches!(cpu("hio"), Ok(PromptAction::Enter(Level::Top))));
        assert!(matches!(top("hio"), Ok(PromptAction::Enter(Level::Top))));
        assert!(matches!(
            cpu("hio reset --hold"),
            Ok(PromptAction::Hio(Step::Window(Cmd::Reset {
                hold: true,
                ..
            })))
        ));
        assert!(matches!(
            top("hio read harness_magic"),
            Ok(PromptAction::Hio(Step::Window(Cmd::Read { .. })))
        ));
        assert!(matches!(
            cpu("hio cpu step 2"),
            Ok(PromptAction::Cpu(CpuCmd::Step { count: 2, .. }))
        ));

        // Inside the console, `exit` and `boot` point at its keys.
        let Err(err) = prompt_action(Level::Cpu, true, "exit") else {
            panic!("exit was taken inside the console");
        };
        assert!(err.contains("Ctrl-A x"), "{err}");
        let Err(err) = prompt_action(Level::Cpu, true, "console a.elf") else {
            panic!("console was taken inside the console");
        };
        assert!(err.contains("Ctrl-A c"), "{err}");
    }

    /// The PC is shown inside the function that holds it.
    #[test]
    fn an_address_is_named_by_the_symbol_around_it() {
        let elf = hns_host::elf::Elf {
            wide: false,
            segments: Vec::new(),
            symbols: vec![
                hns_host::elf::Symbol {
                    name: "main".to_string(),
                    value: 0x8000_0b56,
                    size: 0x40,
                    absolute: false,
                },
                hns_host::elf::Symbol {
                    name: "_start".to_string(),
                    value: 0x8000_0000,
                    size: 0,
                    absolute: false,
                },
                // A local label inside `main`, and a mangled Rust name.
                hns_host::elf::Symbol {
                    name: ".L0 ".to_string(),
                    value: 0x8000_0b5a,
                    size: 0,
                    absolute: false,
                },
                hns_host::elf::Symbol {
                    name: "_RNvMs3_Cslv7u3prg1jP_10rtt_targetNtB5_11DownChannel4read".to_string(),
                    value: 0x8000_0c00,
                    size: 0x40,
                    absolute: false,
                },
            ],
        };
        assert_eq!(symbolize(Some(&elf), 0x8000_0b5a), " <main+0x4>");
        assert_eq!(
            symbolize(Some(&elf), 0x8000_0c20),
            " <<rtt_target::DownChannel>::read+0x20>"
        );
        assert_eq!(symbolize(Some(&elf), 0x8000_0b72), " <main+0x1c>");
        assert_eq!(symbolize(Some(&elf), 0x8000_0b56), " <main>");
        assert_eq!(symbolize(Some(&elf), 0x8000_0000), " <_start>");
        assert_eq!(symbolize(Some(&elf), 0x8000_0100), "");
        assert_eq!(symbolize(None, 0x8000_0b72), "");
        // A number from the linker script is not an address.
        let mut with_abs = elf.clone();
        with_abs.symbols.push(hns_host::elf::Symbol {
            name: "_hart_stack_size".to_string(),
            value: 0x1_0000,
            size: 0,
            absolute: true,
        });
        assert_eq!(symbolize(Some(&with_abs), 0x1_0000), "");
    }

    /// A place is an address, or a symbol by its stored, readable or last
    /// name; an unclear one is refused with the names to choose from.
    #[test]
    fn a_place_is_an_address_or_a_symbol() {
        let sym = |name: &str, value| hns_host::elf::Symbol {
            name: name.to_string(),
            value,
            size: 4,
            absolute: false,
        };
        let elf = hns_host::elf::Elf {
            wide: false,
            segments: Vec::new(),
            symbols: vec![
                sym("main", 0x100),
                sym("_ZN11hazard3_bsp4getc17h0123456789abcdefE", 0x200),
                sym("_ZN11hazard3_bsp3rtt4exit17h0123456789abcdefE", 0x300),
                sym("_ZN11hazard3_bsp4exit17h0123456789abcdefE", 0x400),
            ],
        };
        let at = |place| place_address(place, Some(&elf));
        assert_eq!(at("0x80000000"), Ok(0x8000_0000));
        assert_eq!(at("main"), Ok(0x100));
        assert_eq!(at("hazard3_bsp::getc"), Ok(0x200));
        assert_eq!(at("getc"), Ok(0x200));
        assert_eq!(at("hazard3_bsp::exit"), Ok(0x400));
        let err = at("exit").unwrap_err();
        assert!(err.contains("hazard3_bsp::rtt::exit"), "{err}");
        assert!(at("nope").unwrap_err().contains("no symbol"));
        let err = place_address("main", None).unwrap_err();
        assert!(err.contains("--elf"), "{err}");
    }

    /// A map with regions `(name, kind, base)`.
    fn region_map(regions: &[(&str, &str, usize)]) -> RegisterMap {
        let regions: Vec<String> = regions
            .iter()
            .map(|(name, kind, base)| {
                format!(
                    r#"{{"name":"{name}","kind":"{kind}","window_base":{base},"size_bytes":512,
                        "entry_bytes":4,"depth":128}}"#
                )
            })
            .collect();
        RegisterMap::from_json(&format!(
            r#"{{"marker":"m","format_version":{},"generator":"g","dut":"d","word_bits":32,
                "size_bytes":4096,"magic":1,"map_hash":2,"registers":[],"regions":[{}]}}"#,
            hns_regs::FORMAT_VERSION,
            regions.join(",")
        ))
        .unwrap()
    }

    /// `hio cpu` finds the Debug Module by the region kind, not by its name,
    /// and asks when there is more than one.
    #[test]
    fn the_debug_module_is_the_region_of_kind_dmi() {
        let one = region_map(&[("ram", "memory", 0), ("dbg", "dmi", 1024)]);
        assert_eq!(dmi_base(&one, None).unwrap(), 1024);

        let none = region_map(&[("ram", "memory", 0)]);
        let err = dmi_base(&none, None).unwrap_err();
        assert!(err.contains("backing = \"dmi\""), "{err}");

        let two = region_map(&[("dm0", "dmi", 0), ("dm1", "dmi", 512)]);
        let err = dmi_base(&two, None).unwrap_err();
        assert!(err.contains("--dmi") && err.contains("dm0, dm1"), "{err}");
        assert_eq!(dmi_base(&two, Some("dm1")).unwrap(), 512);
        let err = dmi_base(&two, Some("dm2")).unwrap_err();
        assert!(err.contains("dm0, dm1"), "{err}");
    }

    /// A procedure file may halt and read registers, but not take over the
    /// terminal.
    #[test]
    fn a_procedure_file_takes_the_debug_commands_but_not_boot() {
        assert!(matches!(
            parse_step("cpu halt --hart 1"),
            Ok(Step::Window(Cmd::Cpu(CpuCmd::Halt { .. })))
        ));
        assert!(matches!(
            parse_step("cpu regs pc 0x80000000"),
            Ok(Step::Window(Cmd::Cpu(CpuCmd::Regs { .. })))
        ));
        let Err(err) = parse_step("cpu boot a.elf") else {
            panic!("boot was taken in a procedure file");
        };
        assert!(err.contains("take over the terminal"), "{err}");
    }

    /// The DMA engine is used only on the memory it is wired to.
    ///
    /// Used for another bundle, it would move the wrong memory: a write
    /// silently damages it, and a read may "match" data left by an earlier
    /// run with the same seed.
    #[test]
    fn the_requester_is_only_used_on_the_memory_it_reaches() {
        let map = requester_map(Some("mem"));
        let mem = mem_port(&map, "mem").unwrap();
        let scratch = mem_port(&map, "scratch").unwrap();
        let big = 1 << 20;

        // Large goes to the card, small to the window.
        assert_eq!(
            route(&map, "pcie", "memtest", "mem", &mem, big, DMA_ABOVE, None).unwrap(),
            Route::Card { forced: false }
        );
        assert_eq!(
            route(&map, "pcie", "memtest", "mem", &mem, 1024, DMA_ABOVE, None).unwrap(),
            Route::Window { note: None }
        );
        // `--dma` and `--pio` ignore the size.
        assert_eq!(
            route(
                &map,
                "pcie",
                "memtest",
                "mem",
                &mem,
                4,
                DMA_ABOVE,
                Some(true)
            )
            .unwrap(),
            Route::Card { forced: true }
        );
        assert_eq!(
            route(
                &map,
                "pcie",
                "memtest",
                "mem",
                &mem,
                big,
                DMA_ABOVE,
                Some(false)
            )
            .unwrap(),
            Route::Window { note: None }
        );

        // Another bundle quietly uses the window, however large. The design
        // decides this, so a message would never change.
        assert_eq!(
            route(
                &map, "pcie", "memtest", "scratch", &scratch, big, DMA_ABOVE, None
            )
            .unwrap(),
            Route::Window { note: None }
        );
        // Forced, it refuses and names the memory it is wired to.
        let err = route(
            &map,
            "pcie",
            "memtest",
            "scratch",
            &scratch,
            big,
            DMA_ABOVE,
            Some(true),
        )
        .unwrap_err();
        assert!(err.contains("`mem`, not `scratch`"), "{err}");
        assert!(err.contains("Drop --dma"), "{err}");
    }

    /// A map that does not name the requester's memory does not use the card.
    /// A wrong guess would damage another memory.
    ///
    /// The fallback says why, because a fast command becomes slow. It also
    /// gives the fix: regenerate the map; no new bitstream is needed.
    #[test]
    fn a_map_that_does_not_name_the_requester_memory_falls_back_and_says_why() {
        let map = requester_map(None);
        let mem = mem_port(&map, "mem").unwrap();
        let Route::Window { note: Some(note) } = route(
            &map,
            "pcie",
            "memtest",
            "mem",
            &mem,
            1 << 20,
            DMA_ABOVE,
            None,
        )
        .unwrap() else {
            panic!("expected the window with a note");
        };
        assert!(note.contains("veryl harness gen"), "{note}");
        assert!(note.contains("still matches"), "{note}");

        let err = route(
            &map,
            "pcie",
            "memtest",
            "mem",
            &mem,
            1 << 20,
            DMA_ABOVE,
            Some(true),
        )
        .unwrap_err();
        assert!(err.contains("veryl harness gen"), "{err}");
    }

    /// The card moves data over PCIe. In a JTAG run it would test PCIe while
    /// the user thinks they test JTAG.
    #[test]
    fn the_card_does_not_move_data_for_a_jtag_run() {
        let map = requester_map(Some("mem"));
        let mem = mem_port(&map, "mem").unwrap();
        assert_eq!(
            route(
                &map,
                "jtag",
                "memtest",
                "mem",
                &mem,
                1 << 30,
                DMA_ABOVE,
                None
            )
            .unwrap(),
            Route::Window { note: None }
        );
        assert!(
            route(
                &map,
                "jtag",
                "memtest",
                "mem",
                &mem,
                4,
                DMA_ABOVE,
                Some(true)
            )
            .is_err()
        );
    }

    /// The card writes the same bytes as the window.
    ///
    /// The window pads the last entry with 0 (`entry_words`). If the DMA
    /// engine stopped at a partial length, old data would remain there, and
    /// the result would depend on the route.
    #[test]
    fn a_piece_is_cut_into_descriptors_and_padded_like_the_window() {
        let map = requester_map(Some("mem"));
        let port = mem_port(&map, "mem").unwrap();
        // 10 bytes = 3 entries (the last holds only 2 bytes).
        let piece = Piece {
            entry: 5,
            addr: None,
            bytes: (1..=10).collect(),
        };
        assert_eq!(piece_span(&port, &piece), 12);
        // With 8 bytes per descriptor: 8 + 4. Never split inside an entry.
        assert_eq!(piece_chunks(&port, &piece, 8), vec![(0, 8), (8, 4)]);
        assert_eq!(piece_chunks(&port, &piece, 4096), vec![(0, 12)]);

        let mut out = [0xffu8; 4];
        piece_bytes(&piece, 8, 4, &mut out);
        assert_eq!(out, [9, 10, 0, 0]);

        // The same words as a window write.
        let mut all = [0u8; 12];
        piece_bytes(&piece, 0, 12, &mut all);
        let window: Vec<u32> = piece
            .bytes
            .chunks(4)
            .flat_map(|chunk| entry_words(chunk, 1))
            .collect();
        let card: Vec<u32> = all
            .chunks(4)
            .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
            .collect();
        assert_eq!(window, card);
    }

    /// Only the counters that grew are reported, each by name. They saturate
    /// and never return to 0, so only differences count.
    #[test]
    fn only_what_grew_during_the_run_is_reported() {
        let before = Losses {
            rq_drops: 3,
            rc_cor: 7,
            rc_uncor: 0,
        };
        assert!(before.since(&before).is_empty());

        let now = Losses {
            rq_drops: 5,
            rc_cor: 7,
            rc_uncor: 1,
        };
        let lines = now.since(&before);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("dma_rq_drops +2"), "{lines:?}");
        assert!(lines[1].contains("dma_rc_uncor +1"), "{lines:?}");
    }

    /// Sizes print the way `--size` takes them, so `check` output can be pasted.
    #[test]
    fn a_size_is_printed_the_way_it_is_typed() {
        assert_eq!(size_str(64 << 20), "64M");
        assert_eq!(size_str(2 << 20), "2M");
        assert_eq!(size_str(4096), "4k");
        assert_eq!(size_str(2 << 30), "2G");
        assert_eq!(size_str(1000), "1000");
        // Parsing the output gives the same value.
        for bytes in [64u64 << 20, 4096, 2 << 30] {
            assert_eq!(parse_u64(&size_str(bytes)).unwrap(), bytes);
        }
    }

    /// Only size registers get a readable size. An `M` on an address or the
    /// magic would mislead.
    #[test]
    fn only_size_registers_get_a_readable_size() {
        let map = RegisterMap::from_json(&format!(
            r#"{{"marker":"m","format_version":{},"generator":"g","dut":"d","word_bits":32,
                "size_bytes":64,"magic":1,"map_hash":2,
                "registers":[
                  {{"name":"mem_depth","kind":"terminator","offset":0,"words":1,"width":32,
                    "access":"ro","bundle":"mem","role":"depth"}},
                  {{"name":"dma_size","kind":"terminator","offset":4,"words":1,"width":32,
                    "access":"rw","role":"dma_size"}},
                  {{"name":"dma_base","kind":"terminator","offset":8,"words":2,"width":64,
                    "access":"rw","role":"dma_base"}}]}}"#,
            hns_regs::FORMAT_VERSION
        ))
        .unwrap();
        let by = |name: &str| map.get(name).unwrap();
        assert_eq!(
            size_note(by("mem_depth"), &[0x2000_0000]),
            "  (512M entries)"
        );
        assert_eq!(size_note(by("dma_size"), &[0x0020_0000]), "  (2M)");
        assert_eq!(size_note(by("dma_base"), &[0x0020_0000, 0]), "");
    }

    /// A script refuses `check`. It looks at things before the window opens,
    /// and in a script the window is already open.
    #[test]
    fn a_script_refuses_check() {
        let Err(err) = parse_step("check") else {
            panic!("check was accepted in a script");
        };
        assert!(err.contains("on its own"), "{err}");
    }

    /// A script can use `reset`, so "hold, load, release" fits in one script.
    #[test]
    fn a_script_holds_and_releases_the_dut() {
        for line in ["reset", "reset --hold", "reset --release"] {
            assert!(
                matches!(parse_step(line), Ok(Step::Window(Cmd::Reset { .. }))),
                "{line}"
            );
        }
        assert!(parse_step("reset --hold --release").is_err());
    }

    /// The same code means different things per direction. On the read side
    /// 10 is a completion UR (the IOMMU refused); on the write side it is an
    /// AXI write SLVERR. Mixing them up blames the memory for a host setting.
    #[test]
    fn an_engine_error_is_named_by_the_table_of_its_direction() {
        assert!(engine_error(true, 10).contains("UR"));
        assert_eq!(engine_error(false, 10), "AXI write SLVERR");
        assert_eq!(engine_error(false, 15), "timeout");
        assert_eq!(engine_error(true, 1), "timeout");
    }

    /// A wrong value is traced to an address one bit away.
    ///
    /// A dead address line maps to an address a power of two away, not to
    /// a neighbour. This tells wiring from storage faults without a full
    /// search.
    #[test]
    fn a_value_from_one_address_bit_away_is_named() {
        let seed = 1;
        let depth = 1 << 20;
        let words = 1;

        // Bit 12 is stuck: reading 0x1234 returns the contents of 0x0234.
        let e = 0x1234;
        let other = e ^ (1 << 12);
        let got = pattern(seed, other, 0);
        let (oe, ow, bit) = alias_of(seed, e, 0, got, depth, words).unwrap();
        assert_eq!((oe, ow), (other, 0));
        assert_eq!(bit, 12);

        // A value from nowhere gets no guess.
        assert!(alias_of(seed, e, 0, 0xdead_beef, depth, words).is_none());
    }

    /// Without a file, `program` takes the bitstream next to the map.
    #[test]
    fn the_bitstream_beside_the_map_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let map = dir.path().join("regs.json");
        std::fs::write(&map, "{}").unwrap();
        let out = dir.path().join("syn").join("output");
        std::fs::create_dir_all(&out).unwrap();

        // Nothing yet: say where it looked and how to build one.
        let err = bitstream_beside(map.to_str().unwrap()).unwrap_err();
        assert!(err.contains("syn/output"), "{err}");
        assert!(err.contains("make"), "{err}");

        // One file: use it.
        std::fs::write(out.join("dut_hns_top.bit"), b"x").unwrap();
        assert_eq!(
            bitstream_beside(map.to_str().unwrap()).unwrap(),
            out.join("dut_hns_top.bit")
        );

        // Two files: do not choose.
        std::fs::write(out.join("other.bit"), b"x").unwrap();
        let err = bitstream_beside(map.to_str().unwrap()).unwrap_err();
        assert!(err.contains("dut_hns_top.bit"), "{err}");
        assert!(err.contains("other.bit"), "{err}");

        // A .svf beside the .bit is the same design: the .bit is taken.
        std::fs::remove_file(out.join("other.bit")).unwrap();
        std::fs::write(out.join("dut_hns_top.svf"), b"x").unwrap();
        assert_eq!(
            bitstream_beside(map.to_str().unwrap()).unwrap(),
            out.join("dut_hns_top.bit")
        );

        // A .svf alone is taken.
        std::fs::remove_file(out.join("dut_hns_top.bit")).unwrap();
        assert_eq!(
            bitstream_beside(map.to_str().unwrap()).unwrap(),
            out.join("dut_hns_top.svf")
        );
        std::fs::remove_file(out.join("dut_hns_top.svf")).unwrap();
        std::fs::write(out.join("dut_hns_top.bit"), b"x").unwrap();

        // Files that are neither .bit nor .svf do not count.
        std::fs::write(out.join("dut_hns_top.dcp"), b"x").unwrap();
        std::fs::write(out.join("timing.rpt"), b"x").unwrap();
        assert_eq!(
            bitstream_beside(map.to_str().unwrap()).unwrap(),
            out.join("dut_hns_top.bit")
        );
    }

    /// `--size 256M` means the same number as `256M` in `Harness.toml`. An
    /// error that suggests `--size 256M` must accept it.
    #[test]
    fn sizes_are_spelled_the_same_way_everywhere() {
        assert_eq!(parse_u64("256M").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_u64("4k").unwrap(), 4096);
        assert_eq!(parse_u64("64K").unwrap(), 65536);
        assert_eq!(parse_u64("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        // Plain numbers and hex are unchanged.
        assert_eq!(parse_u64("1024").unwrap(), 1024);
        assert_eq!(parse_u64("0x100").unwrap(), 256);
        assert_eq!(parse_u64("1_048_576").unwrap(), 1024 * 1024);
        // A wrong unit gets the right spelling in the error.
        let err = parse_u64("256MB").unwrap_err();
        assert!(err.contains("256M"), "{err}");

        // Word counts take the same spelling (`hio read mem 0 16k`).
        assert_eq!(parse_usize("16k").unwrap(), 16384);
    }

    /// A rate counts in thousands: `--tck-hz 15M` is 15,000,000, not
    /// 15,728,640. It has its own parser, so each caller picks the rule.
    #[test]
    fn a_rate_counts_in_thousands_not_in_kibi() {
        assert_eq!(parse_rate_u32("15M").unwrap(), 15_000_000);
        assert_eq!(parse_rate_u32("48m").unwrap(), 48_000_000);
        assert_eq!(parse_rate_u32("100k").unwrap(), 100_000);
        assert_eq!(parse_rate_u32("15000000").unwrap(), 15_000_000);
        // The same spelling as a size counts in powers of two.
        assert_eq!(parse_u64("15M").unwrap(), 15 * 1024 * 1024);
    }

    /// A read past a region's entries is refused before it is sent.
    ///
    /// A region is rounded up to a power of two, so part of it has no memory.
    /// A read there returns 0 and bumps `oor`, and that 0 must not look like
    /// data.
    #[test]
    fn a_read_past_the_end_of_a_region_is_refused() {
        // 100 entries x 4 bytes = 400 bytes, in a 512-byte window.
        let region = hns_regs::Region {
            kind: Some("memory".to_string()),
            total_bytes: None,
            held_in_dut_reset: false,
            name: "imem".to_string(),
            window_base: 0x200,
            size_bytes: 512,
            entry_bytes: 4,
            depth: 100,
        };

        // Inside, it passes, with `window_base` added.
        assert_eq!(region_span(&region, 0, 1).unwrap(), 0x200);
        assert_eq!(region_span(&region, 8, 2).unwrap(), 0x208);
        assert_eq!(region_span(&region, 396, 1).unwrap(), 0x200 + 396);

        // Past the last word.
        let err = region_span(&region, 396, 2).unwrap_err();
        assert!(err.contains("runs past"), "{err}");
        assert!(err.contains("400 bytes"), "{err}");
        // The error says a 0 would come back.
        assert!(err.contains("imem_oor"), "{err}");

        // The rounded-up part (400..512) is refused too, though in the window.
        assert!(region_span(&region, 400, 1).is_err());

        // An unaligned offset.
        let err = region_span(&region, 6, 1).unwrap_err();
        assert!(err.contains("word boundary"), "{err}");
        assert!(err.contains("0x4"), "{err}");
    }

    /// An offset past the window is refused before it is sent.
    ///
    /// The bridge carries only the window's address bits, so a larger offset
    /// wraps. On the board, `hio read 0x1018` returned the magic at `0x18`
    /// with exit code 0. The RTL cannot detect this, so it is stopped here.
    #[test]
    fn an_offset_past_the_window_is_refused_instead_of_wrapping() {
        let map = RegisterMap::from_json(&sample_map()).unwrap();

        // Inside the window, it passes.
        assert_eq!(offset_of(&map, "0x8").unwrap(), 8);
        assert_eq!(offset_of(&map, "0xc").unwrap(), 12);

        // Just past the end, and offsets that would wrap into the window.
        for past in ["0x10", "0x18", "0x1018"] {
            let err = offset_of(&map, past).expect_err(past);
            assert!(err.contains("past the end of the window"), "{err}");
            // The error names the window's last word.
            assert!(err.contains("0xc"), "{err}");
        }

        // An unaligned offset does not silently become the word below.
        let err = offset_of(&map, "0x6").unwrap_err();
        assert!(err.contains("word boundary"), "{err}");
        assert!(err.contains("0x4"), "{err}");
    }

    /// A 16-byte window. The version comes from the constant, so a format bump
    /// does not break the test.
    fn sample_map() -> String {
        format!(
            r#"{{
            "marker": "veryl-harness:generated",
            "format_version": {},
            "generator": "veryl-harness",
            "dut": "dut_top",
            "target": "digilent/arty-a7-35",
            "word_bits": 32,
            "size_bytes": 16,
            "magic": 1447580238,
            "map_hash": 305419896,
            "registers": [
                {{"name": "i_data", "kind": "port", "offset": 8, "words": 1,
                 "width": 8, "access": "rw", "bundle": "csr", "value": null,
                 "role": null, "self_clearing": null}}
            ]
        }}"#,
            hns_regs::FORMAT_VERSION
        )
    }

    /// JTAG-only commands are refused over PCIe instead of silently using JTAG.
    #[test]
    fn the_jtag_only_commands_are_refused_over_pcie() {
        for args in [
            vec!["hio", "--transport", "pcie", "probe"],
            vec!["hio", "--transport", "pcie", "erase"],
            vec!["hio", "--transport", "pcie", "program", "x.bit"],
        ] {
            let cli = Cli::parse_from(args.clone());
            let err = run_cli(cli).unwrap_err();
            assert!(
                err.contains("cannot run over PCIe"),
                "{args:?} should be refused, got: {err}"
            );
            // The error gives the fix.
            assert!(err.contains("without `--transport pcie`"), "{err}");
        }
    }

    /// `--pio` and `--dma` cannot both be given. Picking one of them would
    /// ignore the other.
    #[test]
    fn the_two_paths_cannot_both_be_forced() {
        let text = match Cli::try_parse_from(["hio", "memtest", "mem", "--pio", "--dma"]) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("both paths were accepted at once"),
        };
        assert!(text.contains("--pio") && text.contains("--dma"), "{text}");

        // Each alone is accepted.
        assert!(Cli::try_parse_from(["hio", "memtest", "mem", "--pio"]).is_ok());
        assert!(Cli::try_parse_from(["hio", "memtest", "mem", "--dma"]).is_ok());
    }

    /// `--mps` takes only TLP payload sizes. No value means no limit (0).
    #[test]
    fn a_payload_size_that_is_not_a_step_is_refused() {
        assert_eq!(mps_limit(None).unwrap(), 0);
        assert_eq!(mps_limit(Some(128)).unwrap(), 1);
        assert_eq!(mps_limit(Some(512)).unwrap(), 3);
        assert_eq!(mps_limit(Some(4096)).unwrap(), 6);

        let err = mps_limit(Some(300)).unwrap_err();
        // The error lists the valid sizes.
        assert!(err.contains("128, 256, 512, 1024, 2048, 4096"), "{err}");
    }

    /// In a JTAG run, `--dma` is refused before the board is touched. The
    /// card moves data over PCIe, so it would test PCIe instead of JTAG.
    #[test]
    fn the_card_does_not_carry_the_data_for_a_jtag_run() {
        let err = run_cli(Cli::parse_from(["hio", "memtest", "mem", "--dma"])).unwrap_err();
        assert!(err.contains("over PCIe"), "{err}");
        // The error gives the fix.
        assert!(err.contains("-p memtest"), "{err}");

        // The same for `load`.
        let err = run_cli(Cli::parse_from(["hio", "load", "mem", "a.bin", "--dma"])).unwrap_err();
        assert!(err.contains("-p load"), "{err}");
    }

    /// The `memtest` thread default (4 over PCIe) is an `Option`, so it does
    /// not trigger the JTAG refusal. Only an explicit `--threads` does.
    #[test]
    fn the_memtest_default_does_not_refuse_over_jtag() {
        // Without the flag, JTAG passes (and stops later at the missing map).
        let err = run_cli(Cli::parse_from(["hio", "memtest", "mem"])).unwrap_err();
        assert!(!err.contains("only does anything over PCIe"), "{err}");

        // Refused only when given explicitly.
        let err =
            run_cli(Cli::parse_from(["hio", "memtest", "mem", "--threads", "4"])).unwrap_err();
        assert!(err.contains("only does anything over PCIe"), "{err}");
    }

    /// `--threads` works only over PCIe, so it is refused before the board is
    /// touched.
    #[test]
    fn threads_without_pcie_is_refused() {
        let err = run_cli(Cli::parse_from(["hio", "bench", "mem", "--threads", "8"])).unwrap_err();
        assert!(err.contains("only does anything over PCIe"), "{err}");
        // The error gives the fix.
        assert!(err.contains("-p"), "{err}");
        // And why it does not help over JTAG.
        assert!(err.contains("batching"), "{err}");

        // One thread is the default, so it passes (and stops at the missing map).
        let err = run_cli(Cli::parse_from(["hio", "bench", "mem", "--threads", "1"])).unwrap_err();
        assert!(!err.contains("only does anything over PCIe"), "{err}");
    }

    /// `-p` means `--transport pcie`.
    #[test]
    fn the_short_flag_selects_pcie() {
        let short = Cli::parse_from(["hio", "-p", "id"]);
        let long = Cli::parse_from(["hio", "--transport", "pcie", "id"]);
        let named = Cli::parse_from(["hio", "-t", "pcie", "id"]);
        assert_eq!(short.transport_name(), "pcie");
        assert_eq!(long.transport_name(), "pcie");
        assert_eq!(named.transport_name(), "pcie");
        // The default stays jtag.
        assert_eq!(Cli::parse_from(["hio", "id"]).transport_name(), "jtag");
        // Both at once are refused, so there is no "which one wins" rule.
        assert!(Cli::try_parse_from(["hio", "-p", "--transport", "jtag", "id"]).is_err());
    }

    /// An unknown transport does not silently become jtag.
    #[test]
    fn an_unknown_transport_is_refused() {
        let map: RegisterMap = RegisterMap::from_json(&sample_map()).unwrap();
        let cli = Cli::parse_from(["hio", "--transport", "nonsense", "id"]);
        let err = match open_bus(&cli, "regs.json", &map) {
            Err(err) => err,
            Ok(_) => panic!("an unknown transport must not open anything"),
        };
        assert!(err.contains("not a transport"), "{err}");
        assert!(err.contains("jtag"), "{err}");
    }

    /// `--connect` reaches a server, checks what it serves, and refuses what
    /// only the server's machine can do.
    #[test]
    fn connect_reaches_a_server_that_serves_this_map() {
        let map: RegisterMap = RegisterMap::from_json(&sample_map()).unwrap();
        let start = |info: hns_host::hns2::Info| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            std::thread::spawn(move || {
                let mut window = hns_host::hns2::Batches {
                    info,
                    run: |_: &Batch| Err("no window".to_string()),
                };
                hns_host::hns2::serve(listener, &mut window, |_| {})
            });
            addr
        };
        let info = hns_host::hns2::Info {
            server: "test".to_string(),
            kind: "board".to_string(),
            target: map.target.clone(),
            master: "pcie".to_string(),
            map_hash: Some(map.map_hash),
            max_ops: 16,
            program_files: Vec::new(),
        };
        let addr = start(info.clone());
        let cli = Cli::parse_from(["hio", "--connect", &addr, "id"]);
        match open_bus(&cli, "no-such-dir/regs.json", &map) {
            Ok(bus) => assert_eq!(bus.master(), "pcie"),
            Err(e) => panic!("{e}"),
        }
        // Another map, or a simulation, is named before anything runs.
        let other = start(hns_host::hns2::Info {
            map_hash: Some(map.map_hash ^ 1),
            ..info.clone()
        });
        let cli = Cli::parse_from(["hio", "--connect", &other, "id"]);
        let err = open_bus(&cli, "regs.json", &map).err().unwrap();
        assert!(err.contains("map hash"), "{err}");
        let sim = start(hns_host::hns2::Info {
            kind: "sim".to_string(),
            ..info
        });
        let cli = Cli::parse_from(["hio", "--connect", &sim, "id"]);
        let err = open_bus(&cli, "regs.json", &map).err().unwrap();
        assert!(err.contains("a simulation"), "{err}");
    }

    #[test]
    fn a_server_names_only_files_beside_the_bitstream() {
        assert!(plain_file_name("ps7_init.tcl"));
        for name in ["", ".", "..", ".hidden", "../x", "a/b", "/etc/passwd"] {
            assert!(!plain_file_name(name), "{name:?}");
        }
    }

    #[test]
    fn connect_refuses_the_cable_and_dma() {
        for args in [
            vec!["hio", "--connect", "h:1", "program", "x.svf"],
            vec![
                "hio",
                "--connect",
                "h:1",
                "program",
                "--allow-any-device",
                "x.bit",
            ],
            vec!["hio", "--connect", "h:1", "probe"],
            vec!["hio", "--connect", "h:1", "load", "mem", "x.elf", "--dma"],
            vec!["hio", "--connect", "h:1", "serve"],
        ] {
            let err = run_cli(Cli::parse_from(&args)).unwrap_err();
            assert!(err.contains("--connect"), "{args:?}: {err}");
        }
        // A transport and a server at once: which one would win?
        assert!(Cli::try_parse_from(["hio", "-p", "--connect", "h:1", "id"]).is_err());
    }

    /// clap's own consistency check. Conflicts in the argument definitions
    /// (name clashes, a `conflicts_with` naming nothing) panic at run time.
    #[test]
    fn the_command_line_definition_is_consistent() {
        Cli::command().debug_assert();
        short_help_command().debug_assert();
    }

    #[test]
    fn the_short_help_leaves_the_rare_parts_to_list_and_help_options() {
        let short = short_help_command()
            .color(clap::ColorChoice::Never)
            .render_help()
            .to_string();
        assert!(short.contains("  read "), "{short}");
        assert!(short.contains("See all commands with --list"), "{short}");
        assert!(short.contains("--serial"), "{short}");
        for rare in ["dma-fire", "--vid", "--layout-init", "--bdf"] {
            assert!(
                !short.contains(rare),
                "{rare} is in the short help:\n{short}"
            );
        }
        // --help prints the same short help, not the long one.
        let err = short_help_command()
            .color(clap::ColorChoice::Never)
            .try_get_matches_from(["hio", "--help"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        assert_eq!(err.to_string(), short);
        let help = short_help_command()
            .color(clap::ColorChoice::Never)
            .render_help()
            .to_string();
        assert_eq!(help, short, "`hio help` prints the same as `hio --help`");

        // Below `hio`, -h is short and --help is long, as clap has it.
        let help_of = |args: &[&str]| {
            let err = short_help_command()
                .color(clap::ColorChoice::Never)
                .try_get_matches_from(args)
                .unwrap_err();
            assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp, "{args:?}");
            err.to_string()
        };
        let long = help_of(&["hio", "cpu", "boot", "--help"]);
        assert!(long.contains("Made to be a cargo runner"), "{long}");
        let brief = help_of(&["hio", "cpu", "boot", "-h"]);
        assert!(!brief.contains("Made to be a cargo runner"), "{brief}");
        assert!(brief.contains("--wait-ms"), "{brief}");
        assert!(help_of(&["hio", "cpu", "--help"]).contains("  console "));
        assert!(help_of(&["hio", "read", "--help"]).contains("<WHO>"));

        assert!(command_list().contains("  dma-fire "));
        let options = options_help();
        assert!(options.contains("--layout-init <DATA:DIR>"), "{options}");
        assert!(options.contains("Digilent Adept USB Device"), "{options}");
    }

    fn map_json(version: u32) -> String {
        format!(
            r#"{{"marker":"veryl-harness:generated","format_version":{version},
               "generator":"veryl-harness","dut":"d","target":"digilent/arty-a7-35",
               "word_bits":32,"size_bytes":16,"magic":1447580238,"map_hash":1,
               "registers":[]}}"#
        )
    }

    fn write_temp(name: &str, text: &str) -> String {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// `gen --out-dir hns/<board>` puts the map one level down. One such map
    /// is found; with two, hio asks rather than pick another board's.
    #[test]
    fn the_only_map_under_hns_is_found_and_two_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let place = |sub: &str| {
            std::fs::create_dir_all(root.join(sub)).unwrap();
            std::fs::write(root.join(sub).join("regs.json"), "{}").unwrap();
        };
        let fixed = ["regs.json", "hns/regs.json"];

        assert_eq!(regs_candidates_in(root).unwrap(), fixed);
        place("hns/vcu118");
        assert_eq!(
            regs_candidates_in(root).unwrap(),
            ["regs.json", "hns/regs.json", "hns/vcu118/regs.json"]
        );
        place("hns/sim");
        let two = regs_candidates_in(root).unwrap_err();
        assert!(two.contains("hns/sim/regs.json"), "{two}");
        assert!(two.contains("hns/vcu118/regs.json"), "{two}");
        assert!(two.contains("--regs"), "{two}");
        // A map in a fixed place wins, as before.
        place("hns");
        assert_eq!(regs_candidates_in(root).unwrap(), fixed);
    }

    /// "Missing" and "unreadable" are different. Mixed up, a stale
    /// `regs.json` reads as "no map found", and someone who passed `--regs`
    /// is told to pass `--regs`.
    #[test]
    fn a_map_that_cannot_be_read_says_why_instead_of_looking_absent() {
        let missing = std::env::temp_dir().join("hio-no-such-regs.json");
        let _ = std::fs::remove_file(&missing);
        let missing = missing.to_string_lossy().into_owned();
        assert!(read_map(&[missing]).unwrap().is_none());

        let stale = write_temp(
            "hio-stale-regs.json",
            &map_json(hns_regs::FORMAT_VERSION - 1),
        );
        let err = read_map(std::slice::from_ref(&stale)).unwrap_err();
        assert!(err.contains("format_version"), "{err}");
        assert!(err.contains(&stale), "the path is named: {err}");
        assert!(err.contains("veryl harness gen"), "the fix is named: {err}");

        let good = write_temp("hio-good-regs.json", &map_json(hns_regs::FORMAT_VERSION));
        let (_, map) = read_map(&[good]).unwrap().unwrap();
        // This is what lets `--target` be left out.
        assert_eq!(map.target.as_deref(), Some("digilent/arty-a7-35"));
    }

    /// A map generated from a target file stops until the file is passed.
    ///
    /// Older generators wrote the path into `target`. The error must give the
    /// fix (`--target-file`) and name `regs.json`. A map made from a name
    /// still resolves.
    #[test]
    fn a_map_from_a_target_file_asks_for_the_file() {
        let with = |name: &str, target: &str| {
            let text = map_json(hns_regs::FORMAT_VERSION)
                .replace(r#""target":"digilent/arty-a7-35","#, target);
            write_temp(name, &text)
        };
        let run = |path: &str| resolve_target(&Cli::parse_from(["hio", "--regs", path, "id"]));

        let file = with("hio-file-regs.json", r#""target_source":"file","#);
        let err = run(&file).unwrap_err();
        assert!(err.contains("--target-file"), "{err}");
        assert!(err.contains(&file), "the map is named: {err}");

        let legacy = with(
            "hio-legacy-regs.json",
            r#""target":"/somewhere/private/board/default.toml","#,
        );
        let err = run(&legacy).unwrap_err();
        assert!(
            err.contains("--target-file /somewhere/private/board/default.toml"),
            "{err}"
        );
        assert!(!err.contains("not a target name"), "{err}");

        let named = with(
            "hio-named-regs.json",
            r#""target":"digilent/arty-a7-35","target_source":"name","#,
        );
        let target = run(&named).unwrap().unwrap();
        assert_eq!(target.name, "digilent/arty-a7-35");
    }

    /// Every short help fits on one line.
    ///
    /// clap takes the whole first paragraph of a doc comment as the short
    /// help, so a long first paragraph makes the `-h` list long. Put details
    /// after a blank line; they show in `--help`.
    #[test]
    fn the_short_help_of_everything_fits_on_one_line() {
        const LIMIT: usize = 72;

        fn check(cmd: &clap::Command, path: &str) {
            for arg in cmd.get_arguments() {
                if let Some(help) = arg.get_help() {
                    let text = help.to_string();
                    assert!(
                        !text.contains('\n') && text.chars().count() <= LIMIT,
                        "{path} --{}: short help is {} chars, over {LIMIT}.\n  {text}\n\
                         Put the first sentence on its own line and leave a blank \
                         /// line before the rest.",
                        arg.get_id(),
                        text.chars().count()
                    );
                }
            }
            for sub in cmd.get_subcommands() {
                if let Some(about) = sub.get_about() {
                    let text = about.to_string();
                    assert!(
                        !text.contains('\n') && text.chars().count() <= LIMIT,
                        "{path} {}: short help is {} chars, over {LIMIT}.\n  {text}\n\
                         Put the first sentence on its own line and leave a blank \
                         /// line before the rest.",
                        sub.get_name(),
                        text.chars().count()
                    );
                }
                check(sub, sub.get_name());
            }
        }

        check(&Cli::command(), "hio");
    }

    /// Every shell gets a script that names the subcommands. The format is
    /// clap_complete's job; this only checks that new subcommands appear.
    #[test]
    fn every_shell_gets_a_script_that_mentions_the_subcommands() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Elvish,
            Shell::PowerShell,
        ] {
            let mut out = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "hio", &mut out);
            let text = String::from_utf8(out).unwrap();
            for name in ["memtest", "drain", "program", "completions"] {
                assert!(text.contains(name), "{shell} script lacks `{name}`");
            }
        }
    }
}

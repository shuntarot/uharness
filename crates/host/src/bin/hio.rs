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
    version,
    about = "Poke a veryl-harness window over JTAG or PCIe",
    long_about = None,
    disable_help_flag = true,
    arg_required_else_help = true,
    after_help = "See 'hio help <command>' for more on a command, and 'hio help options' for every option."
)]
struct Cli {
    /// The generated regs.json.
    ///
    /// Names, widths and the window size come from it. Without it: ./regs.json if
    /// there is one, else hns/regs.json.
    #[arg(long, value_name = "PATH")]
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

    /// Match the serial number.
    ///
    /// Required when more than one probe of the same type is plugged in.
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

    /// PCI address of the card, e.g. 0000:04:00.0.
    ///
    /// Without it the ID in regs.json picks the card; this is only needed when more than
    /// one answers to it.
    #[arg(long, value_name = "BDF", hide_short_help = true)]
    bdf: Option<String>,

    /// Do not check the magic and map hash before acting.
    ///
    /// Only useful when the window is known not to answer yet.
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

    #[command(subcommand)]
    cmd: Cmd,
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

    /// Write a .bit to the device over JTAG, without Vivado.
    Program {
        /// The .bit that Vivado wrote, or a .svf.
        ///
        /// A raw .bin has no header and is refused, because it carries no device name to
        /// check against the board.
        ///
        /// Devices with more than one SLR (VU9P and friends) need the .svf: their
        /// .bit is one sub-bitstream per SLR, and splitting it means guessing at a
        /// format Xilinx does not document. `make svf` writes one next to the .bit.
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
        /// what places the program. Guessing it wrong puts the program somewhere else, so
        /// it has no clever default; the addresses it worked out are printed, to check
        /// against the linker script.
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
    },

    /// Empty a host_poll_fifo bundle, oldest entry first.
    Drain {
        /// Bundle name, as it appears in Harness.toml.
        bundle: String,
        /// Stop after this many entries.
        #[arg(long, value_name = "N")]
        max: Option<usize>,
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
        .filter(|f| match &cli.serial {
            Some(s) => f.serial.as_deref() == Some(s.as_str()),
            None => true,
        })
        .collect();

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
                 Pick one with --serial (or --product).",
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

    let (_, map) = find_regs(cli)?;
    let mut bus = open_bus(cli, &map)?;

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
        Cmd::Probe | Cmd::Targets | Cmd::Check | Cmd::Completions { .. } => {
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
        | Cmd::Probe
        | Cmd::Targets
        | Cmd::Completions { .. } => {
            unreachable!("refused when the script was read")
        }
        Cmd::Id => id(bus, map),
        Cmd::Read { who, at, words } => do_read(bus, map, &who, at, words),
        Cmd::Write { who, values } => do_write(bus, map, &who, &values),
        Cmd::Dump => dump(bus, map),
        Cmd::Reset { hold, release } => reset(bus, map, hold, release),
        Cmd::Check => unreachable!("refused by parse_step"),
        Cmd::Drain { bundle, max } => drain(bus, map, &bundle, max),
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
            && (j.cfg_in_ir.is_some() || !j.slr.is_empty())
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
    for (i, d) in chain.iter().enumerate() {
        match d {
            hns_host::mpsse::Device::Id(id) => println!("device  {i}: idcode {id:#010x}"),
            hns_host::mpsse::Device::Bypass => println!("device  {i}: no idcode (bypass)"),
        }
    }
    if chain.len() > 1 {
        // Code that assumes one device silently reaches the wrong one.
        println!(
            "        note: {} devices are in the chain. hio supports only one device in \
             the chain for now.",
            chain.len()
        );
    }

    // Measure on the chain instead of looking it up, so unknown boards work.
    match m.measure_ir_length().map_err(|e| e.to_string())? {
        Some(len) => {
            println!("ir      {len} bits (measured)");
            if len != cfg.ir_length {
                println!(
                    "        note: {} bits is being used instead. Set `ir_length = {len}` \
                     in the target, or pass --ir-length {len}.",
                    cfg.ir_length
                );
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
/// With two or more candidates it lists them and refuses. Silently writing
/// the wrong one leaves a board that does not work, with no clue why.
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

    // Check the device before writing. Without a target there is nothing to
    // compare with, so do not go on silently.
    let target = resolve_target(cli)?;
    match (&target, allow_any_device) {
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

    let jtag = target.as_ref().map(hns_targets::jtag).unwrap_or_default();
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
    let chunks = split(&bit, &jtag)?;

    let cfg = resolve_probe(cli)?;
    let mut m = Mpsse::attach(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
        .map_err(|e| e.to_string())?;

    let start = std::time::Instant::now();
    hns_host::config::program(
        &mut m,
        &ir,
        cfg.ir_length,
        &bit.data,
        &chunks,
        std::time::Duration::from_millis(erase_wait_ms),
    )
    .map_err(|e| e.to_string())?;
    println!(
        "sent {} bytes in {:.2} s",
        bit.data.len(),
        start.elapsed().as_secs_f64()
    );

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
            id(&mut bus, &map)
        }
        None => Ok(()),
    }
}

/// Finds the map used to check the board after configuration.
///
/// Configuration itself succeeded, so a missing map is not an error. If the
/// map could not be read, say why. "Not found" would tell someone who already
/// passed `--regs` to pass `--regs`.
fn after_config_map(cli: &Cli, what: &str) -> Option<RegisterMap> {
    match find_regs_opt(cli) {
        Ok(Some((_, map))) => Some(map),
        Ok(None) => {
            println!(
                "{what}, but it was not checked: no register map was found (looked for {}).\n\
                 Pass --regs with the map for this bitstream to check that the window \
                 answers.",
                regs_candidates(cli).join(", ")
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
/// One SLR gets everything in one go. With several, the chunk starts come
/// from the bitstream, and the lengths and destinations from the target.
///
/// The chunks and skipped bytes must add up to the file length exactly.
/// Otherwise the target and the bitstream disagree, and no SLR would get a
/// complete sub-bitstream.
fn split(
    bit: &hns_host::bitstream::Bitstream,
    jtag: &hns_targets::Jtag,
) -> Result<Vec<hns_host::config::Chunk>, String> {
    let starts = bit.slr_starts();
    if jtag.slr.is_empty() {
        if starts.len() > 1 {
            return Err(format!(
                "the bitstream holds {} sub-bitstreams (one per SLR), but the target does \
                 not say how to split it.\n\
                 The split is not marked in the .bit. Add a `[[jtag.slr]]` table to the \
                 target, or program with an SVF (`hio program <file>.svf`).",
                starts.len()
            ));
        }
        return Ok(vec![hns_host::config::Chunk {
            at: 0,
            bytes: bit.data.len(),
            cfg_in: jtag
                .cfg_in_ir
                .ok_or("the target does not say which IR value selects CFG_IN.".to_string())?,
            sync: false,
        }]);
    }

    let mut out = Vec::new();
    let mut at = 0usize;
    for (i, c) in jtag.slr.iter().enumerate() {
        // A chunk that starts a sub-bitstream must sit where the bitstream
        // says one starts.
        if let Some(&want) = starts.get(i)
            && !c.sync
            && want != at
        {
            return Err(format!(
                "the target puts SLR chunk {i} at byte {at}, but the bitstream has a \
                 sub-bitstream starting at {want}.\n\
                 The SLR table does not fit this device. Check the chunk sizes against a \
                 Vivado SVF for this part."
            ));
        }
        if at + c.bytes > bit.data.len() {
            return Err(format!(
                "the target's SLR table runs past the end of the bitstream ({} bytes).",
                bit.data.len()
            ));
        }
        out.push(hns_host::config::Chunk {
            at,
            bytes: c.bytes,
            cfg_in: c.cfg_in_ir,
            sync: c.sync,
        });
        at += c.bytes + c.skip;
    }
    if at != bit.data.len() {
        return Err(format!(
            "the target's SLR table covers {at} bytes, but the bitstream has {}.\n\
             The rest would never reach the device. The table is probably for a \
             different part.",
            bit.data.len()
        ));
    }
    Ok(out)
}

/// Replays an SVF as it is.
///
/// Devices with several SLRs can only be programmed this way. Their `.bit`
/// joins one sub-bitstream per SLR, and Xilinx does not document where they
/// split. Vivado writes an SVF without a board, so the FPGA host needs no
/// Vivado.
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
            id(&mut bus, &map)
        }
        None => Ok(()),
    }
}

/// Where to look for `regs.json`.
///
/// Fixed default places keep a written procedure portable. A path typed each
/// time only works on the machine it was typed on.
fn regs_candidates(cli: &Cli) -> Vec<String> {
    match &cli.regs {
        Some(p) => vec![p.clone()],
        None => vec!["regs.json".to_string(), "hns/regs.json".to_string()],
    }
}

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
    read_map(&regs_candidates(cli))
}

fn find_regs(cli: &Cli) -> Result<(String, RegisterMap), String> {
    let candidates = regs_candidates(cli);
    match read_map(&candidates)? {
        Some(found) => Ok(found),
        None => Err(format!(
            "no register map was found (looked for {}).\n\
             `veryl harness gen` writes one into hns/. Run from the project directory, \
             copy regs.json here, or pass --regs.",
            candidates.join(", ")
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
    let mut cmd = <Cli as clap::CommandFactory>::command();
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
fn run_cli(cli: Cli) -> Result<(), String> {
    if let Cmd::Run { file } = &cli.cmd {
        return run_script(&cli, file);
    }
    if let Cmd::Completions { shell } = cli.cmd {
        return completions(shell);
    }
    if matches!(cli.cmd, Cmd::Targets) {
        return targets();
    }
    // The card moves data only over PCIe. Allowing `--dma` over JTAG would
    // test PCIe while the user thinks they test JTAG.
    let forced = match &cli.cmd {
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
    let asked_threads = match &cli.cmd {
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
        let only_jtag = match &cli.cmd {
            Cmd::Probe => Some("probe"),
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

    if matches!(cli.cmd, Cmd::Probe) {
        return probe(&cli);
    }
    if let Cmd::Program {
        file,
        allow_any_device,
        erase_wait_ms,
    } = &cli.cmd
    {
        let file = match file {
            Some(f) => f.clone(),
            None => beside_the_map(&cli)?,
        };
        return program(&cli, &file, *allow_any_device, *erase_wait_ms);
    }
    if matches!(cli.cmd, Cmd::Erase) {
        return erase(&cli);
    }

    let (map_path, map) = find_regs(&cli)?;

    // `check` opens the window itself, so a window that does not open is one
    // line of its report.
    if matches!(cli.cmd, Cmd::Check) {
        return check(&cli, &map_path, &map);
    }

    // A missing requester is known from the map. Check before opening, or the
    // error would be about the probe.
    if matches!(cli.cmd, Cmd::DmaFire { .. }) {
        require_requester(&map)?;
    }

    let mut bus = open_bus(&cli, &map)?;

    // Always check the identity first. Many probe values can come from
    // defaults, and a wrong one silently reaches something else. It costs
    // one round trip.
    if !cli.no_verify && !matches!(cli.cmd, Cmd::Id) {
        check_identity(&mut bus, &map)?;
    }

    match cli.cmd {
        Cmd::Targets
        | Cmd::Probe
        | Cmd::Program { .. }
        | Cmd::Erase
        | Cmd::Run { .. }
        | Cmd::Check
        | Cmd::Completions { .. } => {
            unreachable!("handled above")
        }
        Cmd::Id => id(&mut bus, &map),
        Cmd::Read { who, at, words } => do_read(&mut bus, &map, &who, at, words),
        Cmd::Write { who, values } => do_write(&mut bus, &map, &who, &values),
        Cmd::Dump => dump(&mut bus, &map),
        Cmd::Reset { hold, release } => reset(&mut bus, &map, hold, release),
        Cmd::Drain { bundle, max } => drain(&mut bus, &map, &bundle, max),
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
        Cmd::DmaFire {
            ref bundle,
            at,
            len,
            seed,
            expect_fault,
            mps,
            to_card,
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
        } else {
            self.transport.as_deref().unwrap_or("jtag")
        }
    }
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
fn open_bus(cli: &Cli, map: &RegisterMap) -> Result<Bus, String> {
    match cli.transport_name() {
        "jtag" => {
            let cfg = resolve_probe(cli)?;
            let mut io = Mpsse::open(Ftdi::open(&cfg).map_err(|e| e.to_string())?, &cfg)
                .map_err(|e| e.to_string())?;
            set_update_gap(cli, map, &mut io);
            Ok(Bus::Jtag(Bridge::new(io, map.addr_bits() as u32)))
        }
        "pcie" => Ok(Bus::Pcie(open_bar(cli, map)?)),
        other => Err(format!(
            "`--transport {other}` is not a transport this tool has; use `jtag` or `pcie`."
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
        Some(bdf) => Ok(bdf.clone()),
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
        match open_bus(cli, map) {
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
                .filter(|r| r.is_idempotent())
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
    let mut buffer = if expect_fault {
        dma::Buffer::huge_while_translating(&bdf).map_err(|e| e.to_string())?
    } else {
        dma::Buffer::huge(&bdf).map_err(|e| e.to_string())?
    };
    if (len as usize) > buffer.len() {
        return Err(format!(
            "{len} bytes does not fit the page this machine hands out ({} bytes).",
            buffer.len()
        ));
    }
    println!("card     {bdf}");
    println!(
        "page     0x{:x} for {} bytes",
        buffer.bus_addr(),
        buffer.len()
    );

    // Poison the destination before firing. Unwritten words keep the poison,
    // so "not arrived" and "wrong data" look different. The pattern goes to
    // the source; which side is which depends on the direction.
    const POISON_WORD: u32 = u32::from_le_bytes([POISON; 4]);
    if to_card {
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
    } else {
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
    write_wide(&mut b, map, "dma_base", buffer.bus_addr())?;
    write_wide(&mut b, map, "dma_size", buffer.len() as u64)?;
    write_wide(&mut b, map, "dma_pcie_addr", buffer.bus_addr())?;
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
            "the gate refused the descriptor: 0x{:x} + {len} bytes is not inside the page \
             it was given (0x{:x} for {} bytes).",
            buffer.bus_addr(),
            buffer.bus_addr(),
            buffer.len()
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
            buffer.bus_addr()
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
    } else {
        let got = buffer.as_slice();
        let (words, _) = got.as_chunks::<4>();
        words.iter().map(|w| u32::from_le_bytes(*w)).collect()
    };

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
}

impl Bus {
    fn run(&mut self, batch: &Batch) -> Result<Reads, String> {
        match self {
            Bus::Jtag(bridge) => bridge.run(batch).map_err(|e| e.to_string()),
            Bus::Pcie(bar) => bar.run(batch).map_err(|e| e.to_string()),
        }
    }

    /// The master this bus is. A moving window has one base register per
    /// master. With a shared one, a move by one master would make the other
    /// read a different address.
    fn master(&self) -> &'static str {
        match self {
            Bus::Jtag(_) => "jtag",
            Bus::Pcie(_) => "pcie",
        }
    }

    fn read32(&mut self, addr: u32) -> Result<u32, String> {
        match self {
            Bus::Jtag(bridge) => bridge.read32(addr).map_err(|e| e.to_string()),
            Bus::Pcie(bar) => bar.read32(addr).map_err(|e| e.to_string()),
        }
    }

    fn read_burst(&mut self, addr: u32, out: &mut [u32]) -> Result<(), String> {
        match self {
            Bus::Jtag(bridge) => bridge.read_burst(addr, out).map_err(|e| e.to_string()),
            Bus::Pcie(bar) => bar.read_burst(addr, out).map_err(|e| e.to_string()),
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

    let mut set = |on: bool| -> Result<(), String> {
        let mut b = Batch::new();
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
    println!("ok: the board is running this register map");
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
             The bitstream is not a veryl-harness build, or the transport does not reach \
             the window. If every bit is 0 or every bit is 1, suspect the JTAG chain.\n\
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
        out.push((page, (region.base as u64 + off) as u32, here));
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
    Ok((region.base + at as usize) as u32)
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

/// Reads a region, one round trip per window page.
fn read_region(
    bus: &mut Bus,
    map: &RegisterMap,
    region: &hns_regs::Region,
    at: u64,
    words: usize,
) -> Result<Vec<u32>, String> {
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
    let base = (region.base + (at as usize % region.size_bytes)) as u32;
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
///
/// To avoid a dependency, it reads only the fields it needs: type
/// (`PT_LOAD`), file offset and size, and destination.
fn elf_pieces(raw: &[u8], base: Option<u64>, entry_bytes: usize) -> Result<Vec<Piece>, String> {
    let Some(base) = base else {
        return Err(
            "an ELF says where its segments go, but not where this memory starts in the \
             DUT's address space.\n\
             Pass --base with that address (for example --base 0x80000000). There is no \
             default: a wrong guess puts the program in the wrong place."
                .to_string(),
        );
    };
    let bad = |what: &str| format!("this ELF cannot be read: {what}");

    let class = *raw.get(4).ok_or_else(|| bad("truncated header"))?;
    if *raw.get(5).ok_or_else(|| bad("truncated header"))? != 1 {
        return Err(bad("only little-endian ELFs are handled"));
    }
    let wide = match class {
        1 => false,
        2 => true,
        _ => return Err(bad("neither 32-bit nor 64-bit")),
    };

    let u16at = |at: usize| -> Result<usize, String> {
        Ok(u16::from_le_bytes(
            raw.get(at..at + 2)
                .ok_or_else(|| bad("truncated header"))?
                .try_into()
                .unwrap(),
        ) as usize)
    };
    let u32at = |at: usize| -> Result<u64, String> {
        Ok(u32::from_le_bytes(
            raw.get(at..at + 4)
                .ok_or_else(|| bad("truncated header"))?
                .try_into()
                .unwrap(),
        ) as u64)
    };
    let u64at = |at: usize| -> Result<u64, String> {
        Ok(u64::from_le_bytes(
            raw.get(at..at + 8)
                .ok_or_else(|| bad("truncated header"))?
                .try_into()
                .unwrap(),
        ))
    };

    let (phoff, phentsize, phnum) = if wide {
        (u64at(0x20)? as usize, u16at(0x36)?, u16at(0x38)?)
    } else {
        (u32at(0x1c)? as usize, u16at(0x2a)?, u16at(0x2c)?)
    };

    let mut out = Vec::new();
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32at(ph)? != 1 {
            continue; // not PT_LOAD
        }
        let (offset, paddr, filesz) = if wide {
            (
                u64at(ph + 8)? as usize,
                u64at(ph + 0x18)?,
                u64at(ph + 0x20)? as usize,
            )
        } else {
            (
                u32at(ph + 4)? as usize,
                u32at(ph + 0xc)?,
                u32at(ph + 0x10)? as usize,
            )
        };
        if filesz == 0 {
            continue;
        }
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
        let bytes = raw
            .get(offset..offset + filesz)
            .ok_or_else(|| bad("a segment runs past the end of the file"))?
            .to_vec();
        out.push(Piece {
            entry: off / entry_bytes,
            addr: Some(paddr),
            bytes,
        });
    }
    if out.is_empty() {
        return Err(bad("it has no loadable segments"));
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
    let depth = port.depth();
    let entry_bytes = port.entry_bytes();
    let raw = std::fs::read(file).map_err(|e| read_error(file, e))?;

    let pieces = if raw.starts_with(&[0x7f, b'E', b'L', b'F']) {
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
            (region.base + (e * region.entry_bytes) % region.size_bytes + 4 * w) as u32
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
                let at = (region.base + (p.entry + i) * entry_bytes) as u32;
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
            region.base,
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

    /// A map without a requester is refused, and the error names the fix.
    #[test]
    fn a_design_without_a_requester_says_what_it_would_take() {
        let map = RegisterMap::from_json(
            r#"{"marker":"m","format_version":6,"generator":"g","dut":"d","word_bits":32,
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
                r#"{{"name":"{name}","kind":"memory","base":{base},"size_bytes":1024,
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

        // Files that are neither .bit nor .svf do not count.
        std::fs::remove_file(out.join("other.bit")).unwrap();
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
            name: "imem".to_string(),
            base: 0x200,
            size_bytes: 512,
            entry_bytes: 4,
            depth: 100,
        };

        // Inside, it passes, with `base` added.
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
        let err = match open_bus(&cli, &map) {
            Err(err) => err,
            Ok(_) => panic!("an unknown transport must not open anything"),
        };
        assert!(err.contains("not a transport"), "{err}");
        assert!(err.contains("jtag"), "{err}");
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

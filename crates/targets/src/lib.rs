//! Target descriptions and how they resolve. Both the generator and the host
//! read them.
//!
//! `TargetError` carries a miette `NamedSource`, so it is large. Boxing it
//! only adds a level of indirection for the reader, so it is returned as is.
#![allow(clippy::result_large_err)]

//! ```text
//! --target      <provider>/<board>[:<config>]   look up by name
//! --target-file <path>                          your own file (unofficial, warns)
//! --target-patch <path>                         layer a patch (repeatable, warns)
//! ```
//!
//! - Names resolve only in `targets/` and `targets-private/`. A user
//!   directory would make descriptions that CI never ran reachable by name,
//!   and "found by name" would no longer mean "verified".
//! - A name in both directories is an error. The two are equals, so there is
//!   no precedence.
//! - Paths and names use separate flags, so no rule has to tell a path from
//!   a name in one argument. A name may be cut short (`--target d`).
//!
//! The descriptions are embedded with `rust-embed`, as veryl-std does. A
//! binary on PATH must work without knowing where the repository is.

use std::path::{Path, PathBuf};

use miette::{Diagnostic, NamedSource, SourceSpan};
use rust_embed::Embed;
use serde::Deserialize;
use thiserror::Error;

/// Public targets, shipped in the repository.
#[derive(Embed)]
#[folder = "targets"]
#[include = "*.toml"]
#[include = "*.prj"]
struct Public;

/// Private targets. They are not in the repository; they live on the machine
/// that builds (`targets-private/README.md`). They are optional, and appear
/// in the listing as `(private)`.
#[derive(Embed)]
#[folder = "targets-private"]
#[include = "*.toml"]
#[include = "*.prj"]
struct Private;

const DEFAULT_CONFIG: &str = "default";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Target {
    /// The name as given (`digilent/arty-a7-35:fmc-xxx`), or the file path.
    pub name: String,
    pub source: Source,
    pub head: Head,

    /// The resolved description, with config and patches applied.
    ///
    /// Not a struct, because the schema is provisional. Types would make the
    /// current shape a public API.
    pub table: toml::Table,

    /// Applied patches. Always reported, because they change what is
    /// verified.
    pub patches: Vec<PathBuf>,
}

impl Target {
    /// Whether CI exercises this configuration. A patch or `--target-file`
    /// makes it false.
    pub fn verified(&self) -> bool {
        self.patches.is_empty() && !matches!(self.source, Source::File { .. })
    }

    /// Why the target is not verified, for both the human report and JSON.
    pub fn unverified_reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if let Source::File { path } = &self.source {
            reasons.push(format!(
                "the description came from a file ({}), not from a shipped target",
                path.display()
            ));
        }
        for patch in &self.patches {
            reasons.push(format!("patched by {}", patch.display()));
        }
        reasons
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// From `targets/`.
    Public { path: String },
    /// From `targets-private/`.
    Private { path: String },
    /// Given with `--target-file`.
    File { path: PathBuf },
}

impl Source {
    /// Written to JSON as is.
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Public { .. } => "targets",
            Source::Private { .. } => "targets-private",
            Source::File { .. } => "file",
        }
    }

    /// Path for display.
    pub fn path(&self) -> String {
        match self {
            Source::Public { path } | Source::Private { path } => path.clone(),
            Source::File { path } => path.display().to_string(),
        }
    }

    /// Where a file beside the description is, for display.
    pub fn beside(&self, name: &str) -> String {
        let path = self.path();
        match path.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{name}"),
            None => name.to_string(),
        }
    }
}

/// Reads a file that sits beside the description, such as `mig.prj`.
/// `None` if it is not there. For `--target-file`, it is looked up beside
/// that file.
pub fn read_beside(target: &Target, name: &str) -> Option<String> {
    let embedded = |path: &str, root: &str| {
        let dir = path.strip_prefix(&format!("{root}/"))?.rsplit_once('/')?.0;
        Some(format!("{dir}/{name}"))
    };
    let bytes = match &target.source {
        Source::Public { path } => Public::get(&embedded(path, "targets")?)?.data.to_vec(),
        Source::Private { path } => Private::get(&embedded(path, "targets-private")?)?
            .data
            .to_vec(),
        Source::File { path } => std::fs::read(path.parent()?.join(name)).ok()?,
    };
    String::from_utf8(bytes).ok()
}

/// The head of a description, the only typed part. The listing and the
/// directory-name check need it; the rest stays in `table`.
#[derive(Debug, Clone, Deserialize)]
pub struct Head {
    pub board: Board,
    pub device: Device,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Board {
    pub provider: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// The board has not been run on real hardware yet. `check` and `gen`
    /// say so in one line.
    #[serde(default)]
    pub untested: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub vendor: String,
    pub family: String,
    pub part: String,

    /// Regex matched against hw_manager device names, so that the first
    /// device is not assumed when several FPGAs are attached. Without it,
    /// the first element of `part` is used.
    #[serde(default)]
    pub hw_pattern: Option<String>,

    /// An ES (engineering sample) device. Programming must then turn off the
    /// bitstream version check. Defaults to false: turning the check off
    /// would hide an ES/production mix-up.
    #[serde(default)]
    pub engineering_sample: bool,
}

impl Device {
    /// Regex to find the device in hw_manager.
    pub fn hw_pattern(&self) -> String {
        match &self.hw_pattern {
            Some(pattern) => pattern.clone(),
            // The pattern cannot be derived from the part number; the first
            // element is the best guess.
            None => self
                .part
                .split('-')
                .next()
                .unwrap_or(&self.part)
                .to_string(),
        }
    }
}

/// One line of the `targets` listing.
#[derive(Debug)]
pub struct Listed {
    pub name: String,
    pub private: bool,
    pub head: Head,
    pub configs: Vec<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, Diagnostic)]
pub enum TargetError {
    #[error("`{name}` is not a target name")]
    #[diagnostic(
        code(harness::target::malformed_name),
        help(
            "A target is named <provider>/<board>, optionally with a config:\n\n    --target digilent/arty-a7-35\n    --target digilent/arty-a7-35:some-config\n\nThe start of the name is enough when only one target starts that way:\n\n    --target d\n\nTo use a description that does not ship with the tool, pass the file instead:\n\n    --target-file ./my-board.toml"
        )
    )]
    MalformedName { name: String },

    #[error("no target named `{name}`")]
    #[diagnostic(
        code(harness::target::not_found),
        help(
            "Targets that ship with this build:\n{candidates}\n\nOr pass a description of your own:\n\n    --target-file ./my-board.toml"
        )
    )]
    NotFound { name: String, candidates: String },

    #[error("more than one target starts with `{name}`")]
    #[diagnostic(
        code(harness::target::ambiguous_prefix),
        help("{candidates}\n\nType more of the name, so that only one target starts with it.")
    )]
    AmbiguousPrefix { name: String, candidates: String },

    #[error("`{name}` exists in both targets/ and targets-private/")]
    #[diagnostic(
        code(harness::target::ambiguous),
        help(
            "    {public}\n    {private}\n\nThe two directories are equals with a shared namespace, so there is no precedence to fall back on, and picking one silently would leave no record of which description the bitstream was built from.\n\nRename one of them, or keep the public one and put the difference in a patch:\n\n    --target {name} --target-patch <path>"
        )
    )]
    Ambiguous {
        name: String,
        public: String,
        private: String,
    },

    #[error("target `{board}` has no config named `{config}`")]
    #[diagnostic(
        code(harness::target::config_not_found),
        help(
            "Configs of `{board}`:\n{candidates}\n\nA config is a file beside default.toml; it is layered on top of default.toml rather than replacing it."
        )
    )]
    ConfigNotFound {
        board: String,
        config: String,
        candidates: String,
    },

    #[error("cannot read `{}`", path.display())]
    #[diagnostic(
        code(harness::target::unreadable),
        help("{what} must be a readable TOML file.")
    )]
    Unreadable {
        path: PathBuf,
        what: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("`{path}` is not valid TOML")]
    #[diagnostic(
        code(harness::target::parse),
        help("See targets/digilent/arty-a7-35/default.toml for a working description.")
    )]
    Parse {
        path: String,
        #[source_code]
        src: NamedSource<String>,
        #[label("{message}")]
        span: Option<SourceSpan>,
        message: String,
    },

    #[error("`{path}` does not describe a board")]
    #[diagnostic(
        code(harness::target::malformed),
        help(
            "{message}\n\nA description starts with:\n\n    [board]\n    provider = \"digilent\"\n    name     = \"arty-a7-35\"\n\n    [device]\n    vendor = \"xilinx\"\n    family = \"artix7\"\n    part   = \"xc7a35ticsg324-1L\""
        )
    )]
    Malformed { path: String, message: String },

    #[error("`{path}` declares board `{declared}` but sits in `{expected}`")]
    #[diagnostic(
        code(harness::target::head_mismatch),
        help(
            "The directory decides the name a target is looked up by, so a mismatch means one of the two is a copy-paste leftover — and the wrong one would be silently used. Make [board] provider/name agree with the directory."
        )
    )]
    HeadMismatch {
        path: String,
        declared: String,
        expected: String,
    },

    /// Defaults for missing fields would silently give a 0 MHz clock or an
    /// unconstrained pin.
    #[error("[{section}.{name}] of target `{target}` does not say {missing}")]
    #[diagnostic(
        code(harness::target::board_signal_incomplete),
        help(
            "Every board clock and reset needs all of its fields:\n\n{example}\n\nThis is a fault in the description, not in your project. Take the values from the board file rather than writing them from memory."
        )
    )]
    BoardSignalIncomplete {
        target: String,
        section: &'static str,
        name: String,
        missing: &'static str,
        example: &'static str,
    },
}

/// Examples shown in the help text.
const CLOCK_EXAMPLE: &str = "    [clocks.sys]            # single-ended\n    freq_mhz = 100\n    diff     = false\n    pin      = \"E3\"\n    standard = \"LVCMOS33\"\n\n    [clocks.sys]            # differential\n    freq_mhz = 125\n    diff     = true\n    pin_p    = \"AY24\"\n    pin_n    = \"AY23\"\n    standard = \"LVDS\"";
const RESET_EXAMPLE: &str =
    "    [resets.sys]\n    pin      = \"C2\"\n    standard = \"LVCMOS33\"\n    active   = \"low\"";

/// Checks that every board clock and reset has all its fields. It runs on
/// every load, after patches, because a patch can remove a field.
///
/// A missing `diff` is not read as single-ended: that would be a guess.
/// DCI cascade: I/O banks that take their DCI reference from another bank,
/// as `(master, slaves)`. From `[vivado] dci_cascade = { master = 33, slaves =
/// [32, 34] }`.
///
/// A memory controller with DCI I/O needs it when a slave bank's own
/// reference pins carry other signals (the KC705 reset button sits on one).
/// The MIG does not write this constraint, even when its `mig.prj` asks for a
/// cascade.
pub fn dci_cascade(target: &Target) -> Result<Option<(u32, Vec<u32>)>, TargetError> {
    let Some(value) = target
        .table
        .get("vivado")
        .and_then(|v| v.as_table())
        .and_then(|v| v.get("dci_cascade"))
    else {
        return Ok(None);
    };
    let malformed = || {
        TargetError::Malformed {
        path: target.source.path(),
        message: "`[vivado] dci_cascade` must be `{ master = <bank>, slaves = [<bank>, ...] }`, with at least one slave".to_string(),
    }
    };
    let bank = |v: &toml::Value| v.as_integer().and_then(|b| u32::try_from(b).ok());
    let table = value.as_table().ok_or_else(malformed)?;
    let master = table.get("master").and_then(bank).ok_or_else(malformed)?;
    let slaves = table
        .get("slaves")
        .and_then(|v| v.as_array())
        .ok_or_else(malformed)?
        .iter()
        .map(|v| bank(v).ok_or_else(malformed))
        .collect::<Result<Vec<_>, _>>()?;
    if slaves.is_empty() {
        return Err(malformed());
    }
    Ok(Some((master, slaves)))
}

fn check_board_signals(target: &Target) -> Result<(), TargetError> {
    dci_cascade(target)?;
    let incomplete = |section, name: &str, missing, example| TargetError::BoardSignalIncomplete {
        target: target.name.clone(),
        section,
        name: name.to_string(),
        missing,
        example,
    };
    let text = |table: &toml::Table, key: &str| table.get(key).and_then(|v| v.as_str()).is_some();

    if let Some(clocks) = target.table.get("clocks").and_then(|v| v.as_table()) {
        for (name, value) in clocks {
            let Some(table) = value.as_table() else {
                return Err(incomplete(
                    "clocks",
                    name,
                    "anything (it is not a table)",
                    CLOCK_EXAMPLE,
                ));
            };
            let freq = table
                .get("freq_mhz")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)));
            if !freq.is_some_and(|mhz| mhz > 0.0) {
                return Err(incomplete(
                    "clocks",
                    name,
                    "a positive `freq_mhz`",
                    CLOCK_EXAMPLE,
                ));
            }
            let Some(diff) = table.get("diff").and_then(|v| v.as_bool()) else {
                return Err(incomplete(
                    "clocks",
                    name,
                    "whether it is differential (`diff`)",
                    CLOCK_EXAMPLE,
                ));
            };
            let pins: &[&str] = if diff { &["pin_p", "pin_n"] } else { &["pin"] };
            for key in pins {
                if !text(table, key) {
                    return Err(incomplete(
                        "clocks",
                        name,
                        if diff {
                            "both of its pins (`pin_p` and `pin_n`)"
                        } else {
                            "which package pin it is (`pin`)"
                        },
                        CLOCK_EXAMPLE,
                    ));
                }
            }
            if !text(table, "standard") {
                return Err(incomplete(
                    "clocks",
                    name,
                    "its IOSTANDARD (`standard`)",
                    CLOCK_EXAMPLE,
                ));
            }
        }
    }

    if let Some(resets) = target.table.get("resets").and_then(|v| v.as_table()) {
        for (name, value) in resets {
            let Some(table) = value.as_table() else {
                return Err(incomplete(
                    "resets",
                    name,
                    "anything (it is not a table)",
                    RESET_EXAMPLE,
                ));
            };
            if !text(table, "pin") {
                return Err(incomplete(
                    "resets",
                    name,
                    "which package pin it is (`pin`)",
                    RESET_EXAMPLE,
                ));
            }
            if !text(table, "standard") {
                return Err(incomplete(
                    "resets",
                    name,
                    "its IOSTANDARD (`standard`)",
                    RESET_EXAMPLE,
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// One `[pins.<name>]`: a signal the board brings out.
///
/// Only facts about the board; the manifest decides which port goes there.
/// Values come from the board files (`part0_pins.xml` / `board.xml`).
#[derive(Debug, Clone, Default)]
pub struct PinResource {
    pub pin: Option<String>,
    pub standard: Option<String>,
    /// Direction as the FPGA sees it, from `port_map ... dir=` in
    /// `board.xml`.
    pub direction: Option<String>,
}

impl PinResource {
    /// Whether the FPGA may drive this pin: only for `output`. An `input`
    /// would fight the board's driver, and any other value has no known
    /// meaning. `[pin]` and `[heartbeat]` share this rule.
    pub fn fpga_drives(&self) -> bool {
        self.direction.as_deref() == Some("output")
    }

    /// The first field a constraint needs that is missing.
    pub fn missing(&self) -> Option<&'static str> {
        match (&self.pin, &self.standard, &self.direction) {
            (None, _, _) => Some("which package pin it is (`pin`)"),
            (_, None, _) => Some("its IOSTANDARD (`standard`)"),
            (_, _, None) => Some("which way it goes (`direction`)"),
            _ => None,
        }
    }
}

/// `[jtag] max_tck_mhz`: the TCK limit constrained on the bridge.
///
/// A choice, not a measured fact, so a description may state it. `None` when
/// absent; the caller fails and says what to write.
pub fn max_tck_mhz(target: &Target) -> Option<f64> {
    // Same value as `jtag()`. The generator needs only this one.
    let value = target.table.get("jtag")?.as_table()?.get("max_tck_mhz")?;
    value
        .as_float()
        .or_else(|| value.as_integer().map(|v| v as f64))
}

/// Formats the listing for people. The generator (`veryl harness targets`)
/// and the host (`hio targets`) share it, so the two cannot drift apart.
pub fn listing(entries: &[Listed]) -> String {
    let width = name_width(entries);
    entries.iter().map(|e| listing_row(e, width)).collect()
}

/// Width that aligns the name column.
pub fn name_width(entries: &[Listed]) -> usize {
    entries
        .iter()
        .map(|e| e.name.chars().count())
        .max()
        .unwrap_or(4)
}

/// One entry. With `configs` it takes two lines, so it is returned as one
/// string; callers must not assume one line per entry.
pub fn listing_row(e: &Listed, width: usize) -> String {
    let visibility = if e.private { "  (private)" } else { "" };
    let mut out = format!(
        "{:<width$}  {}  [{}]{visibility}\n",
        e.name,
        e.head.device.part,
        e.head.board.description.as_deref().unwrap_or("-"),
    );
    if !e.configs.is_empty() {
        out.push_str(&format!(
            "{:<width$}    configs: {}\n",
            "",
            e.configs.join(", ")
        ));
    }
    out
}

/// `[jtag]`, read by both the generator and the host.
///
/// Every field is an `Option`, so that nothing absent is filled in. The
/// caller decides what to do without it: the generator fails, and the host
/// detects the value and checks it.
#[derive(Debug, Default, Clone)]
pub struct Jtag {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub interface: Option<u8>,
    pub product: Option<String>,
    /// Level-shifter enable (data, direction).
    pub layout_init: Option<(u16, u16)>,
    /// The XDC constraint: the most the device accepts.
    pub max_tck_mhz: Option<f64>,
    /// The TCK the host may actually run. Not `max_tck_mhz`: it is set by the
    /// round-trip delay of cable and level shifters, and only a measurement
    /// gives it.
    pub host_tck_mhz: Option<f64>,
    pub ir_length: Option<u8>,
    pub user1_ir: Option<u32>,
    pub idcode: Option<u32>,
    /// IRs for configuring over JTAG. Usable only as a complete set.
    pub jprogram_ir: Option<u32>,
    pub cfg_in_ir: Option<u32>,
    pub jstart_ir: Option<u32>,
    pub bypass_ir: Option<u32>,
    /// Reads whether the erase finished. It carries the SVF `TDO`/`MASK` as
    /// is, with no meaning given to the bits. Without it, the host only
    /// waits.
    pub ready: Option<Check>,
    /// Reads DONE. Without it, the host does not check that programming
    /// worked.
    pub done: Option<Check>,
    /// How a `.bit` splits on a device with several SLRs.
    ///
    /// A `.bit` is one sub-bitstream per SLR, end to end, and the file does
    /// not mark the boundaries. Lengths and destinations are device
    /// constants, so they are written here. Empty for a single SLR.
    pub slr: Vec<SlrChunk>,
}

/// One check that reads the IR capture.
#[derive(Debug, Clone, Copy)]
pub struct Check {
    pub ir: u32,
    pub expect: u32,
    pub mask: u32,
}

/// A part of a `.bit`, and where it goes.
#[derive(Debug, Clone, Copy)]
pub struct SlrChunk {
    pub bytes: usize,
    /// The `CFG_IN` that receives it.
    pub cfg_in_ir: u32,
    /// Bytes to skip after it (the CRC word at an SLR boundary).
    pub skip: usize,
    /// Whether a sync word goes before it (the last chunk has one).
    pub sync: bool,
}

/// `[pcie]` of a target description: facts about the board only.
///
/// Which IDs to present and how large a BAR to claim are design choices.
/// They live in the section of the same name in `Harness.toml`, which is a
/// different thing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pcie {
    pub lanes: Option<u32>,
    /// The generation the board supports. The link may train lower.
    pub max_gen: Option<u32>,
    pub refclk_mhz: Option<f64>,
    pub refclk_p: Option<String>,
    pub reset_n: Option<String>,
    /// IOSTANDARD of `reset_n`, set by the bank voltage (LVCMOS18 on VCU118).
    pub reset_standard: Option<String>,
    /// Only the p side of each pair. The board wiring fixes n, and Vivado
    /// derives it.
    pub rx_p: Vec<String>,
    pub tx_p: Vec<String>,
}

impl Pcie {
    /// Whether the description has everything PCIe needs. A partial one was
    /// left unfinished, so it is not used.
    pub fn is_complete(&self) -> bool {
        self.lanes.is_some_and(|n| n > 0)
            && self.max_gen.is_some()
            && self.refclk_mhz.is_some()
            && self.refclk_p.is_some()
            && self.reset_n.is_some()
            && self.reset_standard.is_some()
            && self.rx_p.len() as u32 == self.lanes.unwrap_or(0)
            && self.tx_p.len() as u32 == self.lanes.unwrap_or(0)
    }
}

/// Reads `[pcie]`. Empty when absent.
pub fn pcie(target: &Target) -> Pcie {
    let Some(t) = target.table.get("pcie").and_then(|v| v.as_table()) else {
        return Pcie::default();
    };
    let int = |key: &str| {
        t.get(key)
            .and_then(|v| v.as_integer())
            .and_then(|i| u32::try_from(i).ok())
    };
    let num = |key: &str| {
        let v = t.get(key)?;
        v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
    };
    let text = |key: &str| t.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
    let list = |key: &str| {
        t.get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    Pcie {
        lanes: int("lanes"),
        max_gen: int("max_gen"),
        refclk_mhz: num("refclk_mhz"),
        refclk_p: text("refclk_p"),
        reset_n: text("reset_n"),
        reset_standard: text("reset_standard"),
        rx_p: list("rx_p"),
        tx_p: list("tx_p"),
    }
}

pub fn jtag(target: &Target) -> Jtag {
    let Some(t) = target.table.get("jtag").and_then(|v| v.as_table()) else {
        return Jtag::default();
    };
    let int = |key: &str| t.get(key).and_then(|v| v.as_integer());
    let num = |key: &str| {
        let v = t.get(key)?;
        v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
    };
    let pair = |key: &str| {
        let a = t.get(key)?.as_array()?;
        if a.len() != 2 {
            return None;
        }
        Some((a[0].as_integer()? as u16, a[1].as_integer()? as u16))
    };
    Jtag {
        vid: int("vid").map(|v| v as u16),
        pid: int("pid").map(|v| v as u16),
        interface: int("interface").map(|v| v as u8),
        product: t
            .get("product")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        layout_init: pair("layout_init"),
        max_tck_mhz: num("max_tck_mhz"),
        host_tck_mhz: num("host_tck_mhz"),
        ir_length: int("ir_length").map(|v| v as u8),
        user1_ir: int("user1_ir").map(|v| v as u32),
        idcode: int("idcode").map(|v| v as u32),
        ready: check(t, "ready"),
        done: check(t, "done"),
        slr: t
            .get("slr")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        let e = e.as_table()?;
                        let n = |k: &str| e.get(k).and_then(|v| v.as_integer());
                        Some(SlrChunk {
                            bytes: n("bytes")? as usize,
                            cfg_in_ir: n("cfg_in_ir")? as u32,
                            skip: n("skip").unwrap_or(0) as usize,
                            sync: e.get("sync").and_then(|v| v.as_bool()).unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        jprogram_ir: int("jprogram_ir").map(|v| v as u32),
        cfg_in_ir: int("cfg_in_ir").map(|v| v as u32),
        jstart_ir: int("jstart_ir").map(|v| v as u32),
        bypass_ir: int("bypass_ir").map(|v| v as u32),
    }
}

/// `[jtag.<name>]`, usable only with all of `ir`, `expect` and `mask`.
fn check(t: &toml::Table, name: &str) -> Option<Check> {
    let c = t.get(name)?.as_table()?;
    let n = |k: &str| c.get(k).and_then(|v| v.as_integer());
    Some(Check {
        ir: n("ir")? as u32,
        expect: n("expect")? as u32,
        mask: n("mask")? as u32,
    })
}

pub fn pin_resource(target: &Target, name: &str) -> Option<PinResource> {
    let table = target
        .table
        .get("pins")?
        .as_table()?
        .get(name)?
        .as_table()?;
    let text = |key: &str| {
        table
            .get(key)
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
    };
    Some(PinResource {
        pin: text("pin"),
        standard: text("standard"),
        direction: text("direction"),
    })
}

/// The target's pin resources, formatted for an error message.
pub fn pin_resources(target: &Target) -> String {
    let Some(pins) = target.table.get("pins").and_then(|value| value.as_table()) else {
        return "    (none)".to_string();
    };
    if pins.is_empty() {
        return "    (none)".to_string();
    }
    pins.iter()
        .map(|(name, value)| {
            let dir = value
                .as_table()
                .and_then(|table| table.get("direction"))
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            format!("    {name} ({dir})")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolves `--target <name>`.
///
/// The board part may be cut short: `d` names `digilent/arty-a7-35` while no
/// other target starts with `d`. The returned target carries the full name.
pub fn resolve(name: &str, patches: &[PathBuf]) -> Result<Target, TargetError> {
    let (given, config) = split_name(name)?;
    let board = complete(&given, &board_names())?;
    let name = match &config {
        Some(config) => format!("{board}:{config}"),
        None => board.clone(),
    };
    let name = name.as_str();

    let public = find_dir(&Public::iter().collect::<Vec<_>>(), &board);
    let private = find_dir(&Private::iter().collect::<Vec<_>>(), &board);

    // The two directories are equals, so there is no precedence.
    if public.is_some() && private.is_some() {
        return Err(TargetError::Ambiguous {
            name: name.to_string(),
            public: format!("targets/{board}/{DEFAULT_CONFIG}.toml"),
            private: format!("targets-private/{board}/{DEFAULT_CONFIG}.toml"),
        });
    }

    let (files, is_private) = match (public, private) {
        (Some(files), None) => (files, false),
        (None, Some(files)) => (files, true),
        (None, None) => {
            return Err(TargetError::NotFound {
                name: name.to_string(),
                candidates: bullet_list(&list_names()),
            });
        }
        (Some(_), Some(_)) => unreachable!("handled above"),
    };

    let read = |file: &str| -> Option<String> {
        let content = if is_private {
            Private::get(file)?
        } else {
            Public::get(file)?
        };
        String::from_utf8(content.data.to_vec()).ok()
    };

    let root = if is_private {
        "targets-private"
    } else {
        "targets"
    };
    let default_file = format!("{board}/{DEFAULT_CONFIG}.toml");
    let default_path = format!("{root}/{default_file}");
    let default_text = read(&default_file).ok_or_else(|| TargetError::NotFound {
        name: name.to_string(),
        candidates: bullet_list(&list_names()),
    })?;

    let mut table = parse(&default_text, &default_path)?;

    // A config is layered on top of default.toml; it does not replace it.
    if let Some(config) = &config {
        let config_file = format!("{board}/{config}.toml");
        let Some(text) = read(&config_file) else {
            let mut configs: Vec<String> = files
                .iter()
                .filter_map(|file| {
                    let stem = file
                        .strip_prefix(&format!("{board}/"))?
                        .strip_suffix(".toml")?;
                    (stem != DEFAULT_CONFIG).then(|| stem.to_string())
                })
                .collect();
            configs.sort();
            return Err(TargetError::ConfigNotFound {
                board: board.clone(),
                config: config.clone(),
                candidates: bullet_list(&configs),
            });
        };
        let overlay = parse(&text, &format!("{root}/{config_file}"))?;
        merge(&mut table, overlay);
    }

    let head = read_head(&table, &default_path)?;
    check_head(&head, &board, &default_path)?;

    let source = if is_private {
        Source::Private { path: default_path }
    } else {
        Source::Public { path: default_path }
    };

    let mut target = Target {
        name: name.to_string(),
        source,
        head,
        table,
        patches: Vec::new(),
    };
    apply_patches(&mut target, patches)?;
    check_board_signals(&target)?;
    Ok(target)
}

/// Reads `--target-file <path>`. A `:config` suffix has no meaning here.
pub fn load_file(path: &Path, patches: &[PathBuf]) -> Result<Target, TargetError> {
    let text = std::fs::read_to_string(path).map_err(|source| TargetError::Unreadable {
        path: path.to_path_buf(),
        what: "--target-file",
        source,
    })?;
    let shown = path.display().to_string();
    let table = parse(&text, &shown)?;
    let head = read_head(&table, &shown)?;

    let mut target = Target {
        name: shown.clone(),
        source: Source::File {
            path: path.to_path_buf(),
        },
        head,
        table,
        patches: Vec::new(),
    };
    apply_patches(&mut target, patches)?;
    check_board_signals(&target)?;
    Ok(target)
}

fn apply_patches(target: &mut Target, patches: &[PathBuf]) -> Result<(), TargetError> {
    // Applied in argument order.
    for patch in patches {
        let text = std::fs::read_to_string(patch).map_err(|source| TargetError::Unreadable {
            path: patch.clone(),
            what: "--target-patch",
            source,
        })?;
        let overlay = parse(&text, &patch.display().to_string())?;
        merge(&mut target.table, overlay);
        target.patches.push(patch.clone());
    }

    // A patch may change the head too, so read it again.
    target.head = read_head(&target.table, &target.name)?;
    Ok(())
}

/// The shipped targets, for the `targets` subcommand.
pub fn list() -> Vec<Result<Listed, TargetError>> {
    let mut entries = Vec::new();
    for (private, files) in [
        (false, Public::iter().collect::<Vec<_>>()),
        (true, Private::iter().collect::<Vec<_>>()),
    ] {
        let mut boards: Vec<String> = files
            .iter()
            .filter_map(|file| {
                let dir = file.rsplit_once('/')?.0;
                file.ends_with(&format!("/{DEFAULT_CONFIG}.toml"))
                    .then(|| dir.to_string())
            })
            .collect();
        boards.sort();

        for board in boards {
            let file = format!("{board}/{DEFAULT_CONFIG}.toml");
            let root = if private {
                "targets-private"
            } else {
                "targets"
            };
            let path = format!("{root}/{file}");
            let content = if private {
                Private::get(&file)
            } else {
                Public::get(&file)
            };
            let Some(text) = content.and_then(|x| String::from_utf8(x.data.to_vec()).ok()) else {
                continue;
            };

            entries.push(parse(&text, &path).and_then(|table| {
                let head = read_head(&table, &path)?;
                check_head(&head, &board, &path)?;
                let mut configs: Vec<String> = files
                    .iter()
                    .filter_map(|file| {
                        let stem = file
                            .strip_prefix(&format!("{board}/"))?
                            .strip_suffix(".toml")?;
                        (stem != DEFAULT_CONFIG).then(|| stem.to_string())
                    })
                    .collect();
                configs.sort();
                Ok(Listed {
                    name: board.clone(),
                    private,
                    head,
                    configs,
                })
            }));
        }
    }
    entries
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Splits `<board>[:<config>]`. The board part may be cut short, so it is
/// not checked for `<provider>/<board>` here.
fn split_name(name: &str) -> Result<(String, Option<String>), TargetError> {
    let malformed = || TargetError::MalformedName {
        name: name.to_string(),
    };

    let (board, config) = match name.split_once(':') {
        Some((board, config)) => {
            if config.is_empty() || config.contains(':') {
                return Err(malformed());
            }
            (board, Some(config.to_string()))
        }
        None => (name, None),
    };
    if board.is_empty() {
        return Err(malformed());
    }

    Ok((board.to_string(), config))
}

/// Completes a board name that was cut short. An exact match wins. Otherwise
/// exactly one board must start with `given`: with two, either choice would
/// be a guess.
fn complete(given: &str, boards: &[String]) -> Result<String, TargetError> {
    if boards.iter().any(|board| board == given) {
        return Ok(given.to_string());
    }
    let hits: Vec<String> = boards
        .iter()
        .filter(|board| board.starts_with(given))
        .cloned()
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(TargetError::NotFound {
            name: given.to_string(),
            candidates: bullet_list(&list_names()),
        }),
        _ => Err(TargetError::AmbiguousPrefix {
            name: given.to_string(),
            candidates: bullet_list(&hits),
        }),
    }
}

/// Every shipped board, public and private, once each. A board in both
/// directories appears once; `resolve` reports that case.
fn board_names() -> Vec<String> {
    let mut names: Vec<String> = Public::iter()
        .chain(Private::iter())
        .filter_map(|file| {
            file.strip_suffix(&format!("/{DEFAULT_CONFIG}.toml"))
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The files of one board, or `None` if there are none.
fn find_dir(files: &[std::borrow::Cow<'static, str>], board: &str) -> Option<Vec<String>> {
    let prefix = format!("{board}/");
    let found: Vec<String> = files
        .iter()
        .filter(|file| file.starts_with(&prefix))
        .map(|file| file.to_string())
        .collect();
    (!found.is_empty()).then_some(found)
}

fn list_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (private, files) in [
        (false, Public::iter().collect::<Vec<_>>()),
        (true, Private::iter().collect::<Vec<_>>()),
    ] {
        for file in files {
            if let Some(dir) = file.strip_suffix(&format!("/{DEFAULT_CONFIG}.toml")) {
                names.push(if private {
                    format!("{dir}  (private)")
                } else {
                    dir.to_string()
                });
            }
        }
    }
    names.sort();
    names
}

fn parse(text: &str, path: &str) -> Result<toml::Table, TargetError> {
    text.parse::<toml::Table>()
        .map_err(|err| TargetError::Parse {
            path: path.to_string(),
            src: NamedSource::new(path, text.to_string()),
            span: err.span().map(SourceSpan::from),
            message: err.message().to_string(),
        })
}

fn read_head(table: &toml::Table, path: &str) -> Result<Head, TargetError> {
    Head::deserialize(table.clone()).map_err(|err| TargetError::Malformed {
        path: path.to_string(),
        message: err.to_string(),
    })
}

/// The name in `[board]` must match the directory.
fn check_head(head: &Head, board: &str, path: &str) -> Result<(), TargetError> {
    let declared = format!("{}/{}", head.board.provider, head.board.name);
    if declared != board {
        return Err(TargetError::HeadMismatch {
            path: path.to_string(),
            declared,
            expected: board.to_string(),
        });
    }
    Ok(())
}

/// Deep merge: tables merge recursively, and everything else is replaced.
///
/// Arrays are not concatenated. If "add" and "replace" had the same syntax,
/// a patch would not show which one its author meant.
fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base_table)), toml::Value::Table(overlay_table)) => {
                merge(base_table, overlay_table);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn bullet_list(items: &[String]) -> String {
    if items.is_empty() {
        return "    (none)".to_string();
    }
    items
        .iter()
        .map(|item| format!("    {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod jtag_tests {
    use super::*;

    /// After a sub-table such as `[jtag.ready]`, plain keys written below it
    /// belong to the sub-table, not to `[jtag]`. That mistake is hard to see,
    /// and on the board it only looks like an unreachable window.
    #[test]
    fn the_shipped_targets_still_say_how_to_reach_the_window() {
        for name in ["digilent/arty-a7-35", "xilinx/vcu118"] {
            let t = resolve(name, &[]).expect("shipped target must resolve");
            let j = jtag(&t);
            assert!(j.user1_ir.is_some(), "{name}: user1_ir");
            assert!(j.ir_length.is_some(), "{name}: ir_length");
            assert!(j.layout_init.is_some(), "{name}: layout_init");
            assert!(j.host_tck_mhz.is_some(), "{name}: host_tck_mhz");
            // Programming too.
            assert!(j.jprogram_ir.is_some(), "{name}: jprogram_ir");
            assert!(j.jstart_ir.is_some(), "{name}: jstart_ir");
            assert!(j.bypass_ir.is_some(), "{name}: bypass_ir");
            assert!(
                j.cfg_in_ir.is_some() || !j.slr.is_empty(),
                "{name}: nothing says where the bitstream goes"
            );
            // Programming is checked by reading back, not only by waiting.
            assert!(j.ready.is_some(), "{name}: ready check");
            assert!(j.done.is_some(), "{name}: done check");
        }
    }

    /// Chunks plus skipped bytes must add up to the whole bitstream.
    #[test]
    fn the_slr_table_has_no_empty_chunks() {
        let t = resolve("xilinx/vcu118", &[]).unwrap();
        let j = jtag(&t);
        assert_eq!(j.slr.len(), 5);
        assert!(j.slr.iter().all(|c| c.bytes > 0));
        let total: usize = j.slr.iter().map(|c| c.bytes + c.skip).sum();
        assert_eq!(total, 80_159_108, "must account for the whole bitstream");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing field must not become 0 MHz, single-ended, or a missing
    /// constraint.
    #[test]
    fn a_board_clock_or_reset_with_a_missing_field_is_refused() {
        const HEAD: &str = "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"artix7\"\npart = \"xc7a35t\"\n\n";
        const RESET: &str =
            "[resets.sys]\npin = \"C2\"\nstandard = \"LVCMOS33\"\nactive = \"low\"\n";
        let dir = tempfile::tempdir().unwrap();
        let load = |body: &str| {
            let path = dir.path().join("t.toml");
            std::fs::write(&path, format!("{HEAD}{body}")).unwrap();
            load_file(&path, &[])
        };
        let clock = |fields: &str| format!("[clocks.sys]\n{fields}\n{RESET}");

        let full = "freq_mhz = 100\ndiff = false\npin = \"E3\"\nstandard = \"LVCMOS33\"\n";
        assert!(load(&clock(full)).is_ok());
        let diff = "freq_mhz = 125\ndiff = true\npin_p = \"AY24\"\npin_n = \"AY23\"\nstandard = \"LVDS\"\n";
        assert!(load(&clock(diff)).is_ok());

        for (body, says) in [
            (
                clock("diff = false\npin = \"E3\"\nstandard = \"LVCMOS33\"\n"),
                "freq_mhz",
            ),
            (
                clock("freq_mhz = 0\ndiff = false\npin = \"E3\"\nstandard = \"LVCMOS33\"\n"),
                "freq_mhz",
            ),
            (
                clock("freq_mhz = 100\npin = \"E3\"\nstandard = \"LVCMOS33\"\n"),
                "`diff`",
            ),
            (
                clock("freq_mhz = 100\ndiff = true\npin_p = \"AY24\"\nstandard = \"LVDS\"\n"),
                "pin_n",
            ),
            (
                clock("freq_mhz = 100\ndiff = false\npin = \"E3\"\n"),
                "`standard`",
            ),
            (
                format!("[clocks.sys]\n{full}\n[resets.sys]\npin = \"C2\"\nactive = \"low\"\n"),
                "`standard`",
            ),
        ] {
            let err = load(&body).unwrap_err();
            assert!(
                matches!(err, TargetError::BoardSignalIncomplete { .. }),
                "{body}\n{err:?}"
            );
            assert!(err.to_string().contains(says), "{body}\n{err}");
        }
    }

    #[test]
    fn a_dci_cascade_is_read_and_a_malformed_one_is_refused() {
        let kc705 = resolve("xilinx/kc705", &[]).unwrap();
        assert_eq!(dci_cascade(&kc705).unwrap(), Some((33, vec![32, 34])));
        assert_eq!(
            dci_cascade(&resolve("xilinx/vcu118", &[]).unwrap()).unwrap(),
            None
        );

        const HEAD: &str = "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"kintex7\"\npart = \"xc7k325t\"\n\n";
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "dci_cascade = 33",
            "dci_cascade = { master = 33 }",
            "dci_cascade = { master = 33, slaves = [] }",
            "dci_cascade = { master = 33, slaves = [\"34\"] }",
        ] {
            let path = dir.path().join("t.toml");
            std::fs::write(&path, format!("{HEAD}[vivado]\n{bad}\n")).unwrap();
            let err = load_file(&path, &[]).unwrap_err();
            assert!(
                err.to_string().contains("does not describe a board"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn the_arty_carries_its_mig_prj() {
        let target = resolve("digilent/arty-a7-35", &[]).unwrap();
        let prj = read_beside(&target, "mig.prj").unwrap();
        assert!(prj.contains("<Project NoOfControllers=\"1\""), "{prj}");
        assert!(read_beside(&target, "nope.prj").is_none());
        assert_eq!(
            target.source.beside("mig.prj"),
            "targets/digilent/arty-a7-35/mig.prj"
        );
    }

    #[test]
    fn a_board_not_run_on_hardware_says_so() {
        assert!(resolve("xilinx/kcu105", &[]).unwrap().head.board.untested);
        assert!(
            !resolve("digilent/arty-a7-35", &[])
                .unwrap()
                .head
                .board
                .untested
        );
    }

    #[test]
    fn the_shipped_arty_description_resolves() {
        let target = resolve("digilent/arty-a7-35", &[]).unwrap();

        assert_eq!(target.head.board.provider, "digilent");
        assert_eq!(target.head.device.part, "xc7a35ticsg324-1L");
        assert!(target.verified(), "a shipped target is CI territory");
        assert!(matches!(target.source, Source::Public { .. }));
    }

    #[test]
    fn the_start_of_a_name_is_enough() {
        for given in ["d", "digilent/arty", "digilent/arty-a7-35"] {
            let target = resolve(given, &[]).unwrap();
            assert_eq!(target.name, "digilent/arty-a7-35", "{given}");
        }
        let err = resolve("d:nope", &[]).unwrap_err();
        assert!(matches!(err, TargetError::ConfigNotFound { .. }), "{err:?}");
    }

    #[test]
    fn a_prefix_of_two_boards_is_refused() {
        let boards = [
            "acme/a1".to_string(),
            "acme/a1-big".to_string(),
            "acme/b".to_string(),
        ];
        assert_eq!(complete("acme/a1", &boards).unwrap(), "acme/a1");
        assert_eq!(complete("acme/b", &boards).unwrap(), "acme/b");
        assert_eq!(complete("acme/a1-", &boards).unwrap(), "acme/a1-big");

        let err = complete("acme/a", &boards).unwrap_err();
        let TargetError::AmbiguousPrefix { candidates, .. } = &err else {
            panic!("expected AmbiguousPrefix, got {err:?}");
        };
        assert!(candidates.contains("acme/a1-big"), "{candidates}");
        assert!(!candidates.contains("acme/b"), "{candidates}");
    }

    #[test]
    fn an_empty_name_is_rejected() {
        for given in ["", ":cfg", "digilent/arty-a7-35:"] {
            let err = resolve(given, &[]).unwrap_err();
            assert!(
                matches!(err, TargetError::MalformedName { .. }),
                "{given}: {err:?}"
            );
        }
    }

    #[test]
    fn a_path_given_as_a_name_points_at_target_file() {
        let err = resolve("./my-board.toml", &[]).unwrap_err();
        assert!(matches!(err, TargetError::NotFound { .. }), "{err:?}");
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("--target-file"), "{rendered}");
    }

    #[test]
    fn an_unknown_target_lists_what_ships() {
        let err = resolve("digilent/nope", &[]).unwrap_err();
        let TargetError::NotFound { candidates, .. } = &err else {
            panic!("expected NotFound, got {err:?}");
        };
        assert!(candidates.contains("digilent/arty-a7-35"), "{candidates}");
    }

    #[test]
    fn an_unknown_config_lists_the_configs_that_exist() {
        let err = resolve("digilent/arty-a7-35:nope", &[]).unwrap_err();
        assert!(
            matches!(err, TargetError::ConfigNotFound { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_patch_merges_deeply_and_replaces_arrays() {
        let mut base = "[provides]\ntransport = [\"jtag\", \"pcie\"]\nbram_kb = 225\n\n[provides.dram]\nchannels = 2\nmb = 512\n"
            .parse::<toml::Table>()
            .unwrap();
        let overlay = "[provides]\ntransport = [\"jtag\"]\n\n[provides.dram]\nchannels = 1\n"
            .parse::<toml::Table>()
            .unwrap();

        merge(&mut base, overlay);

        let provides = base["provides"].as_table().unwrap();
        // Arrays are replaced, not concatenated.
        assert_eq!(provides["transport"].as_array().unwrap().len(), 1);
        // Keys the patch does not touch stay.
        assert_eq!(provides["bram_kb"].as_integer(), Some(225));
        let dram = provides["dram"].as_table().unwrap();
        assert_eq!(dram["channels"].as_integer(), Some(1));
        assert_eq!(dram["mb"].as_integer(), Some(512));
    }

    #[test]
    fn a_patch_makes_the_target_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let patch = dir.path().join("rev-c.toml");
        std::fs::write(&patch, "[provides.dram]\nchannels = 0\n").unwrap();

        let target = resolve("digilent/arty-a7-35", std::slice::from_ref(&patch)).unwrap();

        assert!(!target.verified());
        assert_eq!(target.patches, [patch]);
        assert!(!target.unverified_reasons().is_empty());
    }

    #[test]
    fn a_file_target_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my-board.toml");
        std::fs::write(
            &path,
            "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"artix7\"\npart = \"xc7a35t\"\n",
        )
        .unwrap();

        let target = load_file(&path, &[]).unwrap();

        assert!(!target.verified());
        assert_eq!(target.head.board.name, "proto");
    }

    #[test]
    fn a_head_that_disagrees_with_the_directory_is_rejected() {
        let head = Head {
            board: Board {
                provider: "acme".to_string(),
                name: "proto".to_string(),
                description: None,
                url: None,
                untested: false,
            },
            device: Device {
                vendor: "xilinx".to_string(),
                family: "artix7".to_string(),
                part: "xc7a35t".to_string(),
                hw_pattern: None,
                engineering_sample: false,
            },
        };

        let err = check_head(&head, "digilent/arty-a7-35", "targets/x/default.toml").unwrap_err();
        assert!(matches!(err, TargetError::HeadMismatch { .. }), "{err:?}");
    }

    #[test]
    fn the_listing_includes_the_shipped_target() {
        let listed: Vec<Listed> = list().into_iter().map(|entry| entry.unwrap()).collect();

        // Private descriptions may appear too: each machine has its own
        // `targets-private/`. Only the shipped one is checked, as public.
        let arty = listed
            .iter()
            .find(|entry| entry.name == "digilent/arty-a7-35")
            .expect("the shipped target is listed");
        assert!(!arty.private);
    }
}

#[cfg(test)]
mod pcie_tests {
    use super::*;

    #[test]
    fn the_vcu118_description_is_complete() {
        let target = resolve("xilinx/vcu118", &[]).expect("a shipped target");
        let p = pcie(&target);
        assert!(p.is_complete(), "partial [pcie]: {p:?}");
        assert_eq!(p.lanes, Some(8));
        assert_eq!(p.refclk_p.as_deref(), Some("AC9"));
        assert_eq!(p.reset_n.as_deref(), Some("AM17"));
        // If lane and pin counts differ, one of them is wrong.
        let lanes = p.lanes.unwrap() as usize;
        assert_eq!(p.rx_p.len(), lanes);
        assert_eq!(p.tx_p.len(), lanes);
        // No pin is listed twice.
        let mut all = p.rx_p.clone();
        all.extend(p.tx_p.clone());
        let unique: std::collections::BTreeSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len(), "a pin is listed twice: {all:?}");
    }

    #[test]
    fn a_partial_description_is_not_complete() {
        let mut p = pcie(&resolve("xilinx/vcu118", &[]).unwrap());
        p.rx_p.pop();
        assert!(!p.is_complete(), "a missing lane must not read as complete");
    }

    /// No `[pcie]` is not an error; it reads as empty.
    #[test]
    fn a_board_without_pcie_reads_as_empty() {
        let arty = resolve("digilent/arty-a7-35", &[]).unwrap();
        assert_eq!(pcie(&arty), Pcie::default());
        assert!(!pcie(&arty).is_complete());
    }
}

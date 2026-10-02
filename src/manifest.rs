//! `Harness.toml`: the schema of the sidecar manifest, and how it is found and read.
//!
//! Search order:
//!
//! 1. `--config <path>`, given explicitly (highest priority)
//! 2. `Harness.toml`, written by the user of the harness
//! 3. `[metadata.harness]` in `Veryl.toml`, written by the DUT author (not supported yet)
//!
//! A lowercase `harness.toml` is never searched. On macOS and Windows, names that
//! differ only in case are the same file, so a priority between them cannot work.
//! It is not ignored either: finding one is an error that asks for a rename.
//!
//! Agents read the error text and act on it, so every error says why it fails
//! and how to fix it.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use miette::{Diagnostic, NamedSource, SourceSpan};
use serde::Deserialize;
use thiserror::Error;

/// The manifest name to search for. Capitalized, like `Veryl.toml` and `Cargo.toml`.
pub const MANIFEST_NAME: &str = "Harness.toml";

/// A name detected only to report the wrong case. It is never searched.
const MANIFEST_NAME_WRONG_CASE: &str = "harness.toml";

const VERYL_MANIFEST_NAME: &str = "Veryl.toml";

/// The section in `Veryl.toml` that holds the annotation.
const METADATA_SECTION: &str = "harness";

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// The timing contract. It is a small fixed set, because a free-form contract
/// would need a generator of arbitrary protocol converters. A port that fits
/// none of them is out of scope.
///
/// `Deserialize` is written by hand so that a removed name (`req_ack`, `tagged`)
/// gets an error that names its replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contract {
    /// A fixed number of cycles, no flow control. `latency` is required.
    FixedLatency,
    /// Backpressure, in order.
    ValidReady,
    /// Valid only. There is no flow control, so the DUT cannot be made to wait.
    ///
    /// When the terminator overflows, it can only drop data, and the DUT does
    /// not see the drop. Whether drops happen depends on the DUT rate, the FIFO
    /// depth and the polling bandwidth, and the generator cannot check that.
    /// So a terminator of this contract must expose a saturating drop counter
    /// in the CSR. The promise is then "never drops silently", which can be
    /// checked, instead of "never drops", which cannot.
    ValidOnly,
    /// A fixed interface (`std::axi4_if`). The standard defines the protocol.
    ///
    /// The port type already decides it. It can be written only to state it.
    Axi,
}

/// The values `contract` accepts. Errors list them in this order.
const CONTRACT_VALUES: &[&str] = &["fixed_latency", "valid_ready", "valid_only", "axi"];

impl<'de> Deserialize<'de> for Contract {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        match text.as_str() {
            "fixed_latency" => Ok(Contract::FixedLatency),
            "valid_ready" => Ok(Contract::ValidReady),
            "valid_only" => Ok(Contract::ValidOnly),
            "axi" => Ok(Contract::Axi),
            // Removed names: say what replaces them.
            "req_ack" => Err(serde::de::Error::custom(
                "`req_ack` is gone. A request/response pair with one outstanding transfer is `valid_ready`. Rename it",
            )),
            "tagged" => Err(serde::de::Error::custom(
                "`tagged` is gone. For out-of-order transfers, declare the port as `modport $std::axi4_if::<..>::master`",
            )),
            other => Err(serde::de::Error::unknown_variant(other, CONTRACT_VALUES)),
        }
    }
}

impl Contract {
    pub fn as_str(&self) -> &'static str {
        match self {
            Contract::FixedLatency => "fixed_latency",
            Contract::ValidReady => "valid_ready",
            Contract::ValidOnly => "valid_only",
            Contract::Axi => "axi",
        }
    }
}

impl fmt::Display for Contract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

/// The port terminator.
///
/// `Deserialize` is written by hand so that an old name (`host_fifo`) gets an
/// error that names the new one, not only "unknown value".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// BRAM in the FPGA. Fixed latency.
    Bram,
    /// BRAM that the host writes in advance. The DUT only reads it.
    BramPreload,
    /// Host memory, through a PCIe requester.
    HostMem,
    /// Real DRAM (MIG or similar).
    Dram,
    /// Harness registers, one register per port.
    Reg,
    /// Inside the DUT. The DUT has an addressable slave interface, and the host
    /// accesses it.
    ///
    /// Unlike `reg`, the data lives in the DUT, not in the harness. The harness
    /// only opens a window to it.
    Slave,
    /// Interrupt.
    HostIrq,
    /// A FIFO in the FPGA that the host reads by polling.
    HostPollFifo,
    /// Output for observation only.
    Observe,
}

/// The values `backing` accepts. Errors list them in this order.
const BACKING_VALUES: &[&str] = &[
    "bram",
    "bram_preload",
    "host_mem",
    "dram",
    "reg",
    "slave",
    "host_irq",
    "host_poll_fifo",
    "observe",
];

impl<'de> Deserialize<'de> for Backing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        match text.as_str() {
            "bram" => Ok(Backing::Bram),
            "bram_preload" => Ok(Backing::BramPreload),
            "host_mem" => Ok(Backing::HostMem),
            "dram" => Ok(Backing::Dram),
            "reg" => Ok(Backing::Reg),
            "slave" => Ok(Backing::Slave),
            "host_irq" => Ok(Backing::HostIrq),
            "host_poll_fifo" => Ok(Backing::HostPollFifo),
            "observe" => Ok(Backing::Observe),
            // `host_bar` became `reg` or `slave`. Only the author knows which,
            // so it cannot be mapped automatically.
            "host_bar" => Err(serde::de::Error::custom(
                "`host_bar` was split in two. Use `reg` if the harness holds the data in registers, or `slave` if the DUT has an addressable interface (`addr` is a DUT input)",
            )),
            // An old name: reject it, and name the new one.
            "host_fifo" => Err(serde::de::Error::custom(
                "`host_fifo` was renamed to `host_poll_fifo`. Rename it in the manifest",
            )),
            other => Err(serde::de::Error::unknown_variant(other, BACKING_VALUES)),
        }
    }
}

impl Backing {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backing::Bram => "bram",
            Backing::BramPreload => "bram_preload",
            Backing::HostMem => "host_mem",
            Backing::Dram => "dram",
            Backing::Reg => "reg",
            Backing::Slave => "slave",
            Backing::HostIrq => "host_irq",
            Backing::HostPollFifo => "host_poll_fifo",
            Backing::Observe => "observe",
        }
    }
}

impl Backing {
    /// A memory terminator (`bram`, `bram_preload`). The memory roles apply
    /// only to these.
    pub fn is_memory(&self) -> bool {
        matches!(self, Backing::Bram | Backing::BramPreload)
    }

    /// The backing that terminates the transfer-level host memory interface.
    pub fn is_host_mem(&self) -> bool {
        matches!(self, Backing::HostMem)
    }
}

impl fmt::Display for Backing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

/// The role of a port in a bundle. It is the key when `ports` is a table.
///
/// The memory roles are known only in `bram` / `bram_preload` bundles. Otherwise
/// a plain payload such as `o_csr_rdata` would become a memory port only because
/// of its spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Valid,
    Ready,
    /// Memory address (DUT -> terminator).
    Addr,
    /// Memory read data (terminator -> DUT).
    #[serde(rename = "rdata")]
    RData,
    /// Memory write data (DUT -> terminator).
    #[serde(rename = "wdata")]
    WData,
    /// Memory write enable (DUT -> terminator).
    We,
    /// Memory byte strobe (DUT -> terminator). One bit per byte.
    Wstrb,
    /// Memory read enable (DUT -> terminator). A read happens only in a cycle
    /// where it is high.
    ///
    /// It is independent of `we`. When both are high in one cycle, the write
    /// happens first (write-first). It is not the SRAM `en` (access enable, with
    /// `we` as the direction), so it is named `re`.
    #[serde(rename = "re")]
    Re,

    // --- `host_mem` read. The command and the data stream have separate
    //     handshakes, so they have their own roles instead of `valid` / `ready`.
    RdCmdValid,
    RdCmdReady,
    RdCmdAddr,
    RdCmdSize,
    /// Optional. Not needed with one outstanding transfer.
    RdCmdTag,
    RdValid,
    RdReady,
    RdData,
    RdLast,
    /// Optional. Pairs with `rd_cmd_tag`.
    RdTag,

    // --- `host_mem` write. Only the names are fixed; the terminator comes
    //     after the read side. Read and write have separate ports for full
    //     duplex: a shared command interface could not issue both in one cycle.
    WrCmdValid,
    WrCmdReady,
    WrCmdAddr,
    WrCmdSize,
    /// Optional.
    WrCmdTag,
    WrValid,
    WrReady,
    WrData,
    /// The bytes to write. Optional: without it, the whole beat is written.
    /// It matches `wstrb` of `fixed_latency` and `WSTRB` of AXI4.
    WrStrb,
    WrLast,
    WrDoneValid,
    /// Optional.
    WrDoneTag,
    /// Optional.
    WrDoneError,

    /// A modport port of `std::axi4_if`. The port type decides it, so it does
    /// not need to be in `ports`. It can be written to state it explicitly.
    Axi4,

    /// A payload that is none of the above.
    Data,
}

impl Role {
    /// Common roles listed in error messages. The names come from `as_str`, so a
    /// renamed or removed role cannot stay in the list. The many transfer-level
    /// `rd_*` / `wr_*` roles are mentioned as a group.
    pub const COMMON: [Role; 9] = [
        Role::Valid,
        Role::Ready,
        Role::Data,
        Role::Addr,
        Role::RData,
        Role::WData,
        Role::We,
        Role::Wstrb,
        Role::Re,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Valid => "valid",
            Role::Ready => "ready",
            Role::Addr => "addr",
            Role::RData => "rdata",
            Role::WData => "wdata",
            Role::We => "we",
            Role::Wstrb => "wstrb",
            Role::Re => "re",
            Role::RdCmdValid => "rd_cmd_valid",
            Role::RdCmdReady => "rd_cmd_ready",
            Role::RdCmdAddr => "rd_cmd_addr",
            Role::RdCmdSize => "rd_cmd_size",
            Role::RdCmdTag => "rd_cmd_tag",
            Role::RdValid => "rd_valid",
            Role::RdReady => "rd_ready",
            Role::RdData => "rd_data",
            Role::RdLast => "rd_last",
            Role::RdTag => "rd_tag",
            Role::WrCmdValid => "wr_cmd_valid",
            Role::WrCmdReady => "wr_cmd_ready",
            Role::WrCmdAddr => "wr_cmd_addr",
            Role::WrCmdSize => "wr_cmd_size",
            Role::WrCmdTag => "wr_cmd_tag",
            Role::WrValid => "wr_valid",
            Role::WrReady => "wr_ready",
            Role::WrData => "wr_data",
            Role::WrStrb => "wr_strb",
            Role::WrLast => "wr_last",
            Role::WrDoneValid => "wr_done_valid",
            Role::WrDoneTag => "wr_done_tag",
            Role::WrDoneError => "wr_done_error",
            Role::Axi4 => "axi4",
            Role::Data => "data",
        }
    }

    pub fn is_memory(&self) -> bool {
        matches!(
            self,
            Role::Addr | Role::RData | Role::WData | Role::We | Role::Wstrb | Role::Re
        )
    }

    /// A role of the transfer-level `host_mem` interface.
    pub fn is_host_mem(&self) -> bool {
        matches!(
            self,
            Role::RdCmdValid
                | Role::RdCmdReady
                | Role::RdCmdAddr
                | Role::RdCmdSize
                | Role::RdCmdTag
                | Role::RdValid
                | Role::RdReady
                | Role::RdData
                | Role::RdLast
                | Role::RdTag
                | Role::WrCmdValid
                | Role::WrCmdReady
                | Role::WrCmdAddr
                | Role::WrCmdSize
                | Role::WrCmdTag
                | Role::WrValid
                | Role::WrReady
                | Role::WrData
                | Role::WrStrb
                | Role::WrLast
                | Role::WrDoneValid
                | Role::WrDoneTag
                | Role::WrDoneError
        )
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

/// A value in a role map. A leading `!` inverts it, as in `"!o_full"`.
///
/// Real RTL often has `full` as the inverse of ready, or `empty` as the inverse
/// of valid. Without a way to write that, the generator could emit a handshake
/// with the wrong polarity. Polarity is never inferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleTarget {
    pub port: String,
    pub invert: bool,
}

impl RoleTarget {
    fn parse(text: &str) -> Self {
        match text.strip_prefix('!') {
            Some(port) => Self {
                port: port.trim().to_string(),
                invert: true,
            },
            None => Self {
                port: text.trim().to_string(),
                invert: false,
            },
        }
    }
}

/// How `ports` is written.
///
/// - a list: only the ports. Roles come from the suffix dictionary.
/// - a table: the roles too. For bundles whose names do not follow a pattern.
#[derive(Debug, Clone)]
pub enum PortSpec {
    List(Vec<String>),
    Roles(BTreeMap<Role, RoleTarget>),
}

impl PortSpec {
    /// The port names of this bundle, for either form.
    pub fn port_names(&self) -> Vec<&str> {
        match self {
            PortSpec::List(ports) => ports.iter().map(String::as_str).collect(),
            PortSpec::Roles(roles) => roles.values().map(|target| target.port.as_str()).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            PortSpec::List(ports) => ports.is_empty(),
            PortSpec::Roles(roles) => roles.is_empty(),
        }
    }
}

/// Not `#[serde(untagged)]`: untagged drops the error of each branch and says only
/// "no variant matched". Then an unknown role could not get an error that lists
/// the valid roles.
impl<'de> serde::Deserialize<'de> for PortSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct PortSpecVisitor;

        impl<'de> serde::de::Visitor<'de> for PortSpecVisitor {
            type Value = PortSpec;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                let common: Vec<&str> = Role::COMMON.iter().map(Role::as_str).collect();
                write!(
                    f,
                    "a list of port names, or a table of role = port ({}, or the transfer-level rd_* / wr_* roles)",
                    common.join(", ")
                )
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<PortSpec, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut ports = Vec::new();
                while let Some(port) = seq.next_element::<String>()? {
                    ports.push(port);
                }
                Ok(PortSpec::List(ports))
            }

            fn visit_map<A>(self, mut map: A) -> Result<PortSpec, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut roles = BTreeMap::new();
                // The key is read as a Role, so serde rejects an unknown role
                // with the list of valid values.
                while let Some((role, target)) = map.next_entry::<Role, String>()? {
                    roles.insert(role, RoleTarget::parse(&target));
                }
                Ok(PortSpec::Roles(roles))
            }
        }

        deserializer.deserialize_any(PortSpecVisitor)
    }
}

/// Which module the harness is for.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dut {
    /// The module name of the DUT.
    ///
    /// It must be a module without parameters. For a module with parameters,
    /// the IR has only the instance elaborated with default values, so the
    /// harness would silently use the defaults. That check needs
    /// `ModuleProperty` in the symbol table, so it is not done here.
    pub module: String,
}

/// The declaration of one bundle. The unit is a bundle, not a single port.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    /// May be omitted. Then `valid_ready` is required, and it is an error when
    /// the ports have no ready/valid. That check needs the source, so `check`
    /// does it, not the reader. Do not fill a default here: a silent
    /// `fixed_latency(0)` is the worst possible result.
    #[serde(default)]
    pub contract: Option<Contract>,

    /// The cycle count of `fixed_latency`. A human writes it and the tool checks
    /// it. It is never inferred.
    #[serde(default)]
    pub latency: Option<u32>,

    /// The terminator. Required.
    pub backing: Backing,

    /// The ports of this bundle, stated explicitly.
    ///
    /// When it is given, it is the only definition of the bundle, and naming
    /// rules do not apply. It exists for DUTs with irregular names, such as
    /// `std::fifo`, where the names do not tell which bundle `i_data` and
    /// `o_data` belong to.
    ///
    /// A table fixes the roles too:
    ///
    /// ```toml
    /// ports = { valid = "i_push", ready = "!o_full", data = "i_data" }
    /// ```
    #[serde(default)]
    pub ports: Option<PortSpec>,

    /// Whether a memory address is a byte address or a word index
    /// (`bram` / `bram_preload`).
    ///
    /// Never inferred. It is required for memories wider than 8 bits. If it is
    /// wrong, the DUT reads the next entry at every byte instead of every 8
    /// bytes: it compiles, looks connected, and reads the wrong address.
    #[serde(default)]
    pub addressing: Option<Addressing>,

    /// How the host accesses the memory (`bram` / `bram_preload`).
    ///
    /// `indirect` uses two registers, address and data (`<bundle>_maddr` /
    /// `_mdata`), and takes only two words of the window at any memory size.
    ///
    /// `region` maps a contiguous range into the window, and the window offset
    /// is the memory address. It needs fewer round trips per word, and it has
    /// no shared `maddr` state: with two window masters (JTAG and BAR), an
    /// indirect port lets one master break the pointer of the other. It uses
    /// window space equal to the memory size, so on PCIe it must fit in
    /// `bar_bytes`.
    ///
    /// The default is `region`, because it is faster at any realistic size. A
    /// larger window costs only JTAG DR length: +32% even for a 1 MB window,
    /// while an indirect port costs two round trips (+100%).
    ///
    /// The mode never switches by size. Adding one entry must not change how
    /// the host accesses the memory.
    #[serde(default)]
    pub access: Option<MemAccess>,

    /// The number of entries. It must be a power of two. Used by
    /// `host_poll_fifo` (default 256), memories (default from the address
    /// width), the transfer-level interface, and `dram` (default: all board
    /// memory).
    ///
    /// It may be omitted. It is not a fact about the DUT but the amount of
    /// resources the harness adds, so a default does not break anything
    /// silently. The value used can be read back on the board from the
    /// `<bundle>_depth` register.
    #[serde(default, deserialize_with = "size::option_u32")]
    pub depth: Option<u32>,

    /// With `access = "region"`: the number of bytes shown in the window.
    ///
    /// Without it, the whole memory is in the window. With it, the window has a
    /// fixed size and moves, and the `<bundle>_base_*` registers select which
    /// part of the memory it shows. This is the only way to reach a memory that
    /// does not fit in the window, such as a 256 MB DRAM.
    ///
    /// Each window master has its own base. With a single base, one master could
    /// move it just before the other reads, and the other would read a
    /// different address.
    #[serde(default, deserialize_with = "size::option_u32")]
    pub aperture: Option<u32>,

    /// Where this bundle's region starts in the window, in bytes. Only for a
    /// bundle with a region: a memory with `access = "region"`, `dram`, or
    /// `slave`.
    ///
    /// Without it, the region takes the first free place. Use it when software
    /// expects the region at a fixed offset, such as a device driver that
    /// reads its registers from BAR offset 0. It must be a multiple of the
    /// region size.
    #[serde(default, deserialize_with = "size::option_u32")]
    pub base: Option<u32>,
}

/// Sizes may also be written like `4k` or `256M`.
///
/// These values are always powers of two, so the suffixes are powers of 1024
/// (`k` = 1024, `m` = 1024^2, `g` = 1024^3), in any case. A plain integer also
/// works. With powers of 1000, `4k` would be 4000 and fail the power-of-two
/// check.
mod size {
    use serde::{Deserialize, Deserializer};

    /// The spelling rules live in `hns-regs`. The same `256M` must mean the same
    /// in `Harness.toml` and in `hio --size`, so there is one implementation.
    pub use hns_regs::parse_size as parse;

    /// Accepts an integer or a string.
    pub fn option_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Number(u64),
            Text(String),
        }
        let value = match Option::<Either>::deserialize(deserializer)? {
            None => return Ok(None),
            Some(Either::Number(n)) => n,
            Some(Either::Text(text)) => parse(&text).map_err(serde::de::Error::custom)?,
        };
        u32::try_from(value)
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("{value} does not fit in 32 bits")))
    }

    pub fn bar_bytes<'de, D>(deserializer: D) -> Result<u32, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(option_u32(deserializer)?.unwrap_or(super::default_bar_bytes()))
    }
}

/// How the host accesses a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemAccess {
    /// Two registers, address and data. Uses only two words of the window.
    Indirect,
    /// A contiguous range in the window. The window offset is the address.
    ///
    /// The default. See `Bundle::access` for why.
    #[default]
    Region,
}

impl MemAccess {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemAccess::Indirect => "indirect",
            MemAccess::Region => "region",
        }
    }
}

/// What a memory address counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Addressing {
    /// One step is one byte, as on a CPU address bus.
    Byte,
    /// One step is one entry, as on a simple RAM port.
    Word,
}

impl Addressing {
    pub fn as_str(&self) -> &'static str {
        match self {
            Addressing::Byte => "byte",
            Addressing::Word => "word",
        }
    }
}

/// A constant in `[tie_off]`: a TOML integer, or a string with a radix prefix.
///
/// A human writes the value; it is never inferred. The generator can only check
/// that it fits the port width. It cannot know what the port should get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TieValue {
    /// The text as written. Diagnostics and output show it unchanged.
    pub text: String,
    pub value: u128,
}

impl TieValue {
    /// The number of bits needed for the value, to compare with the port width.
    pub fn bit_width(&self) -> u32 {
        128 - self.value.leading_zeros()
    }

    fn parse(text: &str) -> Result<Self, String> {
        let cleaned = text.trim().replace('_', "");
        let (radix, digits) = match cleaned
            .strip_prefix("0x")
            .or_else(|| cleaned.strip_prefix("0X"))
        {
            Some(digits) => (16, digits),
            None => match cleaned
                .strip_prefix("0b")
                .or_else(|| cleaned.strip_prefix("0B"))
            {
                Some(digits) => (2, digits),
                None => match cleaned
                    .strip_prefix("0o")
                    .or_else(|| cleaned.strip_prefix("0O"))
                {
                    Some(digits) => (8, digits),
                    None => (10, cleaned.as_str()),
                },
            },
        };

        if digits.is_empty() {
            return Err(format!(
                "`{text}` has no digits. Write a value such as 0, 1, 0x3f, 0b1010"
            ));
        }

        u128::from_str_radix(digits, radix)
            .map(|value| TieValue {
                text: text.trim().to_string(),
                value,
            })
            .map_err(|err| {
                format!(
                    "`{text}` is not a non-negative integer ({err}). Write it as 0, 1, 0x3f, 0b1010, or 0o17. Values wider than 128 bits are not supported yet"
                )
            })
    }
}

impl fmt::Display for TieValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.text.fmt(f)
    }
}

impl<'de> serde::Deserialize<'de> for TieValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct TieValueVisitor;

        impl serde::de::Visitor<'_> for TieValueVisitor {
            type Value = TieValue;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a non-negative integer, or a string such as \"0x3f\" / \"0b1010\"")
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<TieValue, E> {
                Ok(TieValue {
                    text: value.to_string(),
                    value: value as u128,
                })
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<TieValue, E> {
                u64::try_from(value).map(|value| TieValue {
                    text: value.to_string(),
                    value: value as u128,
                }).map_err(|_| {
                    // A negative value often means "all ones", but the number of
                    // ones depends on a width we do not know. Ask for the bits.
                    E::custom(format!(
                        "`{value}` is negative, and tie values are unsigned. Write the bits you mean, e.g. 0xffff for 16 bits of ones"
                    ))
                })
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<TieValue, E> {
                TieValue::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(TieValueVisitor)
    }
}

/// `[heartbeat]`: a UART that shows the harness is alive.
///
/// It is a UART, not an LED, because nobody can see the board when working
/// remotely. It must be readable with `cat /dev/ttyUSB*`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    /// A resource name the target declares, not a pin number (same reason as
    /// `[pin]`).
    pub pin: String,

    /// Baud rate. The format is always 8N1.
    #[serde(default = "default_baud")]
    pub baud: u64,
}

fn default_baud() -> u64 {
    115_200
}

/// `[pcie]`: choices of the design. The board facts (lanes, pins) are in the
/// section of the same name in the target description. They are separate.
///
/// It may be omitted. The defaults below also go into `regs.json`, so the board
/// shows that the defaults were used.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pcie {
    /// PCI Vendor ID. The default is an ID we do not own, so it is for testing
    /// only. It is the value that verilog-pcie uses.
    #[serde(default = "default_vendor_id")]
    pub vendor_id: u32,

    #[serde(default = "default_device_id")]
    pub device_id: u32,

    /// PCI class code, base / sub / interface in 24 bits as `lspci -n` shows
    /// them. The default says "none of the classes", so no host driver binds
    /// to the card by its class.
    #[serde(default = "default_class_code")]
    pub class_code: u32,

    /// BAR size in bytes. It must be a power of two larger than the window.
    ///
    /// The window is only tens of bytes. A BAR sized to the window would change
    /// with each new bitstream, and the host would have to follow. The default
    /// is a fixed 4 KiB.
    #[serde(default = "default_bar_bytes", deserialize_with = "size::bar_bytes")]
    pub bar_bytes: u32,
}

impl Default for Pcie {
    fn default() -> Self {
        Self {
            vendor_id: default_vendor_id(),
            device_id: default_device_id(),
            class_code: default_class_code(),
            bar_bytes: default_bar_bytes(),
        }
    }
}

fn default_vendor_id() -> u32 {
    0x1234
}

fn default_device_id() -> u32 {
    0x0001
}

fn default_class_code() -> u32 {
    0xff0000
}

fn default_bar_bytes() -> u32 {
    4 * 1024
}

/// The smallest BAR. The IP (`pcie4_uscale_plus`) accepts BAR sizes only in
/// 1 KB steps, so a smaller value becomes 1 KB in the IP and differs from
/// `regs.json`. `hio` compares the BAR0 size and then rejects the card. The
/// BAR0 of the test machine also has this size.
pub const MIN_BAR_BYTES: u32 = 4 * 1024;

/// `[leave_open]`: outputs that stay unconnected.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaveOpen {
    #[serde(default)]
    pub ports: Vec<String>,
}

/// What one clock port needs.
///
/// The key is the clock port name. The domain name cannot be the key, because
/// a single-clock DUT has the unnamed domain `'_` (Implicit).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clock {
    /// The frequency (MHz) the DUT needs on this port. Never inferred: the
    /// generator cannot know what the DUT needs from the MMCM.
    pub freq_mhz: f64,
}

/// The content of `Harness.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub dut: Dut,

    /// Bundle name -> declaration. A `BTreeMap` keeps generation deterministic:
    /// the same input must always give the same output.
    #[serde(default)]
    pub bundle: BTreeMap<String, Bundle>,

    /// Inputs driven by a constant. Port name -> value.
    ///
    /// This is a kind of terminator. An unused input still needs a driver, so a
    /// human always writes what it gets.
    #[serde(default)]
    pub tie_off: BTreeMap<String, TieValue>,

    /// Outputs that stay unconnected.
    #[serde(default)]
    pub leave_open: LeaveOpen,

    /// `[pin]`: outputs that end at a board pin. Port name -> a resource name
    /// the target declares.
    ///
    /// Pin numbers must not be written here. Otherwise the manifest works for
    /// one board only, and the same DUT can no longer go on every target. The
    /// port name is DUT knowledge, the pin number is board knowledge, and only
    /// the binding between them is the user's choice.
    ///
    /// ```toml
    /// [pin]
    /// o_uart_tx = "uart_tx"
    /// ```
    #[serde(default)]
    pub pin: BTreeMap<String, String>,

    /// Clock port name -> the frequency it needs.
    #[serde(default)]
    pub clock: BTreeMap<String, Clock>,

    /// `[heartbeat]`: a UART driven by the harness itself.
    ///
    /// It shows whether the harness is alive and which bitstream it is, even
    /// when the transport does not work. Without it, nothing is generated.
    #[serde(default)]
    pub heartbeat: Option<Heartbeat>,

    /// The PCIe IDs and the BAR size. The defaults work, but the default IDs
    /// are not ours, so set them before using a real board.
    #[serde(default)]
    pub pcie: Option<Pcie>,
}

#[derive(Debug)]
pub struct Loaded {
    pub manifest: Manifest,

    /// The file that was used. `check` always prints it: with several
    /// candidates, the user must see which one took effect.
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, Diagnostic)]
pub enum ManifestError {
    #[error("[bundle.{bundle}] sets `access`, but its backing is `{backing}`")]
    #[diagnostic(
        code(harness::manifest::access_needs_a_memory),
        help(
            "`access` says how the host reaches a memory, and a `{backing}` bundle has none.\n\nRemove it, or change the backing to `bram` / `bram_preload`."
        )
    )]
    AccessNeedsAMemory { bundle: String, backing: String },

    /// The host decodes by masking address bits, so a size that is not a power
    /// of two answers at unintended addresses too. Below 4 KiB, the IP (1 KB
    /// steps only) and the written value disagree.
    #[error("[pcie] bar_bytes = {bytes} cannot be a BAR")]
    #[diagnostic(
        code(harness::manifest::bar_not_power_of_two),
        help(
            "A BAR must be a power of two and at least 4096 bytes. Round it:\n\n    [pcie]\n    bar_bytes = 4096"
        )
    )]
    BarNotPowerOfTwo { bytes: u32 },

    #[error("[pcie] {what} = {value:#x} does not fit in 16 bits")]
    #[diagnostic(
        code(harness::manifest::pci_id_too_wide),
        help("PCI vendor and device ids are 16 bits each.")
    )]
    PciIdTooWide { what: &'static str, value: u32 },

    #[error("[pcie] class_code = {value:#x} does not fit in 24 bits")]
    #[diagnostic(
        code(harness::manifest::class_code_too_wide),
        help(
            "A class code is base, sub class and interface, one byte each, as `lspci -n` shows them:\n\n    [pcie]\n    class_code = 0x120000   # processing accelerator"
        )
    )]
    ClassCodeTooWide { value: u32 },

    #[error("cannot read `{}` given by --config", path.display())]
    #[diagnostic(
        code(harness::manifest::explicit_unreadable),
        help(
            "--config takes the path to a Harness.toml. Fix the path, or drop the flag to search for {MANIFEST_NAME} in the project root."
        )
    )]
    ExplicitUnreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("cannot read `{}`", path.display())]
    #[diagnostic(code(harness::manifest::unreadable))]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("`{}` is not valid TOML", path.display())]
    #[diagnostic(
        code(harness::manifest::parse),
        help("See doc/guide.md for the full schema.")
    )]
    Parse {
        path: PathBuf,
        #[source_code]
        src: NamedSource<String>,
        #[label("{message}")]
        span: Option<SourceSpan>,
        message: String,
    },

    #[error("found `{}`, but the expected name is `{MANIFEST_NAME}`", path.display())]
    #[diagnostic(
        code(harness::manifest::wrong_case),
        help(
            "Rename it:\n    mv {MANIFEST_NAME_WRONG_CASE} {MANIFEST_NAME}\n\nOnly the capitalized name is searched."
        )
    )]
    WrongCase { path: PathBuf },

    /// An error, so that a written section is never skipped silently.
    #[error("`[metadata.{METADATA_SECTION}]` in `{}` is not supported yet", path.display())]
    #[diagnostic(
        code(harness::manifest::metadata_section_unsupported),
        help(
            "This section is not read yet. Move it into a {MANIFEST_NAME} beside Veryl.toml for now."
        )
    )]
    MetadataSectionUnsupported { path: PathBuf },

    #[error("no {MANIFEST_NAME} found in `{}`", dir.display())]
    #[diagnostic(
        code(harness::manifest::not_found),
        help(
            "Create {MANIFEST_NAME} there:\n\n    [dut]\n    module = \"dut_top\"   # must be a module with NO parameters\n\n    [bundle.csr]\n    contract = \"valid_ready\"\n    backing  = \"reg\"\n\nOr pass an explicit path with --config <path>."
        )
    )]
    NotFound { dir: PathBuf },

    /// Not guessed: a latency that is off by one still runs, and breaks only on
    /// the board.
    #[error("[bundle.{bundle}] has contract = \"fixed_latency\" but no `latency`")]
    #[diagnostic(
        code(harness::manifest::latency_required),
        help("Add the cycle count:\n    latency = <N>\n\nThe latency is never inferred.")
    )]
    LatencyRequired { bundle: String },

    #[error("[bundle.{bundle}] sets `latency` but its contract is \"{contract}\"")]
    #[diagnostic(
        code(harness::manifest::latency_not_allowed),
        help(
            "`latency` applies only to contract = \"fixed_latency\". Remove `latency`, or change the contract."
        )
    )]
    LatencyNotAllowed { bundle: String, contract: Contract },

    #[error("[bundle.{bundle}] has an empty `ports` list")]
    #[diagnostic(
        code(harness::manifest::empty_ports),
        help(
            "List the ports that make up the bundle:\n    ports = [\"i_push\", \"i_data\", \"o_full\"]\n\nOr remove the key to match ports by name. A port belongs to bundle X when its name without the direction prefix is X or starts with X_."
        )
    )]
    EmptyPorts { bundle: String },

    #[error("[bundle.{bundle}] lists port `{port}` twice")]
    #[diagnostic(
        code(harness::manifest::duplicate_port),
        help("Remove the duplicate. It may hide a typo in another port name.")
    )]
    DuplicatePort { bundle: String, port: String },

    #[error("`{port}` is in both [tie_off] and [leave_open]")]
    #[diagnostic(
        code(harness::manifest::tie_and_open),
        help(
            "[tie_off] drives an input with a constant, and [leave_open] leaves an output unconnected. Keep the one that matches the port's direction."
        )
    )]
    TieAndOpen { port: String },

    /// One termination per port. With two, one of them would vanish silently.
    #[error("`{port}` is in both [pin] and [{other}]")]
    #[diagnostic(
        code(harness::manifest::pin_and_other),
        help(
            "Each port gets one termination: [pin] sends an output to a board pin, [leave_open] leaves an output unconnected, and [tie_off] drives an input with a constant. Keep one."
        )
    )]
    PinAndOther { port: String, other: &'static str },

    #[error("[leave_open] lists port `{port}` twice")]
    #[diagnostic(
        code(harness::manifest::duplicate_open),
        help("Remove the duplicate. It may hide a typo in another port name.")
    )]
    DuplicateOpen { port: String },

    #[error("[dut] module is empty")]
    #[diagnostic(
        code(harness::manifest::empty_module),
        help(
            "Name the module the harness connects to. It must have no parameters. For a parameterized DUT, wrap it in a module without parameters and name that."
        )
    )]
    EmptyModule,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Finds and reads the manifest.
///
/// `base_dir` is where the search starts (the directory of `Veryl.toml`).
/// When `explicit` is `Some`, only that file is read and nothing is searched.
pub fn load(base_dir: &Path, explicit: Option<&Path>) -> Result<Loaded, ManifestError> {
    if let Some(path) = explicit {
        let text =
            fs::read_to_string(path).map_err(|source| ManifestError::ExplicitUnreadable {
                path: path.to_path_buf(),
                source,
            })?;
        return parse(&text, path);
    }

    if let Some(path) = find_exact(base_dir, MANIFEST_NAME) {
        let text = fs::read_to_string(&path).map_err(|source| ManifestError::Unreadable {
            path: path.clone(),
            source,
        })?;
        return parse(&text, &path);
    }

    // The wrong case. find_exact compares the real entry name, so this is
    // detected on case-insensitive file systems too.
    if let Some(path) = find_exact(base_dir, MANIFEST_NAME_WRONG_CASE) {
        return Err(ManifestError::WrongCase { path });
    }

    // The DUT side (search step 3) is not implemented. A section that is
    // written but has no effect is the worst case, so it is an error.
    let veryl_toml = base_dir.join(VERYL_MANIFEST_NAME);
    if has_metadata_section(&veryl_toml) {
        return Err(ManifestError::MetadataSectionUnsupported { path: veryl_toml });
    }

    Err(ManifestError::NotFound {
        dir: base_dir.to_path_buf(),
    })
}

/// Returns the entry of `dir` whose real name is exactly `name`.
///
/// `Path::is_file()` alone is not enough. On a case-insensitive file system
/// (default APFS on macOS, Windows), `Harness.toml` is a file even when the real
/// name is `harness.toml`, and errors would name a file that does not exist.
fn find_exact(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        if entry.file_name() == name && entry.path().is_file() {
            return Some(entry.path());
        }
    }
    None
}

/// Whether `Veryl.toml` has `[metadata.harness]`. An unreadable or broken file
/// counts as "no": this is not the place to validate the Veryl manifest.
fn has_metadata_section(veryl_toml: &Path) -> bool {
    let Ok(text) = fs::read_to_string(veryl_toml) else {
        return false;
    };
    let Ok(value) = text.parse::<toml::Table>() else {
        return false;
    };
    value
        .get("metadata")
        .and_then(|metadata| metadata.as_table())
        .is_some_and(|metadata| metadata.contains_key(METADATA_SECTION))
}

fn parse(text: &str, path: &Path) -> Result<Loaded, ManifestError> {
    let manifest: Manifest = toml::from_str(text).map_err(|err| ManifestError::Parse {
        path: path.to_path_buf(),
        src: NamedSource::new(path.display().to_string(), text.to_string()),
        span: err.span().map(SourceSpan::from),
        message: err.message().to_string(),
    })?;

    manifest.validate()?;

    Ok(Loaded {
        manifest,
        path: path.to_path_buf(),
    })
}

impl Manifest {
    /// Checks the consistency that serde cannot express.
    fn validate(&self) -> Result<(), ManifestError> {
        if self.dut.module.trim().is_empty() {
            return Err(ManifestError::EmptyModule);
        }

        if let Some(pcie) = &self.pcie {
            // Whether the window fits in the BAR is checked in `regmap`, after the
            // window size is known. Here we check only that it can be a BAR.
            if pcie.bar_bytes < MIN_BAR_BYTES || !pcie.bar_bytes.is_power_of_two() {
                return Err(ManifestError::BarNotPowerOfTwo {
                    bytes: pcie.bar_bytes,
                });
            }
            for (what, value) in [("vendor_id", pcie.vendor_id), ("device_id", pcie.device_id)] {
                if value > 0xffff {
                    return Err(ManifestError::PciIdTooWide { what, value });
                }
            }
            if pcie.class_code > 0xff_ffff {
                return Err(ManifestError::ClassCodeTooWide {
                    value: pcie.class_code,
                });
            }
        }

        let mut open = std::collections::HashSet::new();
        for port in &self.leave_open.ports {
            if !open.insert(port) {
                return Err(ManifestError::DuplicateOpen { port: port.clone() });
            }
            if self.tie_off.contains_key(port) {
                return Err(ManifestError::TieAndOpen { port: port.clone() });
            }
        }
        for port in self.pin.keys() {
            if open.contains(port) {
                return Err(ManifestError::PinAndOther {
                    port: port.clone(),
                    other: "leave_open",
                });
            }
            if self.tie_off.contains_key(port) {
                return Err(ManifestError::PinAndOther {
                    port: port.clone(),
                    other: "tie_off",
                });
            }
        }

        for (name, bundle) in &self.bundle {
            // `access` only means something for a memory. On any other
            // terminator it would have no effect, so reject it.
            if bundle.access.is_some() && !bundle.backing.is_memory() {
                return Err(ManifestError::AccessNeedsAMemory {
                    bundle: name.clone(),
                    backing: bundle.backing.to_string(),
                });
            }
            if let Some(spec) = &bundle.ports {
                if spec.is_empty() {
                    return Err(ManifestError::EmptyPorts {
                        bundle: name.clone(),
                    });
                }
                let mut seen = std::collections::HashSet::new();
                for port in spec.port_names() {
                    if !seen.insert(port) {
                        return Err(ManifestError::DuplicatePort {
                            bundle: name.clone(),
                            port: port.to_string(),
                        });
                    }
                }
            }

            match (bundle.contract, bundle.latency) {
                (Some(Contract::FixedLatency), None) => {
                    return Err(ManifestError::LatencyRequired {
                        bundle: name.clone(),
                    });
                }
                (Some(contract), Some(_)) if contract != Contract::FixedLatency => {
                    return Err(ManifestError::LatencyNotAllowed {
                        bundle: name.clone(),
                        contract,
                    });
                }
                // No contract with a `latency` is accepted. The ports decide
                // the contract, and a mismatch is reported when it is resolved.
                _ => {}
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcie_has_defaults_that_are_deliberately_borrowed() {
        let m: Manifest = toml::from_str("[dut]\nmodule = \"dut_top\"\n\n[pcie]\n").unwrap();
        let pcie = m.pcie.unwrap();
        // Not our IDs: the values that verilog-pcie uses.
        assert_eq!(pcie.vendor_id, 0x1234);
        assert_eq!(pcie.device_id, 0x0001);
        assert_eq!(pcie.bar_bytes, 4096);
    }

    /// Ignoring it silently would be worst: the author thinks it took effect.
    #[test]
    fn access_on_something_that_is_not_a_memory_is_refused() {
        let text =
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\nbacking = \"reg\"\naccess = \"region\"\n";
        let m: Manifest = toml::from_str(text).unwrap();
        let err = m.validate().unwrap_err();
        let help = miette::Diagnostic::help(&err)
            .map(|h| h.to_string())
            .unwrap_or_default();
        assert!(help.contains("bram"), "{help}");
        assert!(err.to_string().contains("reg"), "{err}");

        // A memory accepts it. The default is `region`.
        let text = "[dut]\nmodule = \"dut_top\"\n\n[bundle.imem]\nbacking = \"bram\"\naccess = \"region\"\n";
        let m: Manifest = toml::from_str(text).unwrap();
        m.validate().unwrap();
        assert_eq!(m.bundle["imem"].access, Some(MemAccess::Region));

        let text = "[dut]\nmodule = \"dut_top\"\n\n[bundle.imem]\nbacking = \"bram\"\n";
        let m: Manifest = toml::from_str(text).unwrap();
        assert_eq!(m.bundle["imem"].access, None);
        assert_eq!(
            m.bundle["imem"].access.unwrap_or_default(),
            MemAccess::Region
        );
    }

    #[test]
    fn a_size_can_be_written_with_a_suffix() {
        let mem = |value: &str| {
            let text = format!(
                "[dut]\nmodule = \"dut_top\"\n\n[bundle.imem]\nbacking = \"bram\"\ndepth = {value}\n"
            );
            toml::from_str::<Manifest>(&text)
                .unwrap_or_else(|e| panic!("{value}: {e}"))
                .bundle["imem"]
                .depth
        };
        assert_eq!(mem("1024"), Some(1024));
        assert_eq!(mem("\"1k\""), Some(1024));
        assert_eq!(mem("\"4K\""), Some(4096));
        assert_eq!(mem("\"2m\""), Some(2 * 1024 * 1024));
        assert_eq!(mem("\"1M\""), Some(1024 * 1024));

        // Powers of 1024, not 1000.
        assert_eq!(mem("\"4k\""), Some(4096));

        // An unreadable size is rejected with the accepted spelling.
        let text =
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.imem]\nbacking = \"bram\"\ndepth = \"lots\"\n";
        let err = toml::from_str::<Manifest>(text).unwrap_err().to_string();
        assert!(err.contains("is not a size"), "{err}");
        assert!(err.contains("256M"), "{err}");
    }

    /// The list must not contain removed roles (`req`, `ack`, `tag`) or old names
    /// (`en`).
    #[test]
    fn a_wrong_ports_shape_lists_the_roles_that_exist() {
        let err = toml::from_str::<Manifest>(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.b]\nbacking = \"reg\"\nports = \"i_data\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("valid, ready, data, addr, rdata"), "{err}");
        for gone in ["req", "ack", "tag", " en,"] {
            assert!(!err.contains(gone), "{gone}: {err}");
        }
        // Every listed name parses as a role.
        for role in Role::COMMON {
            let text = format!("\"{}\"", role.as_str());
            let parsed: Role = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed, role);
        }
    }

    /// Without this check, a port in both `[pin]` and `[leave_open]` would take
    /// `[pin]` silently.
    #[test]
    fn a_port_gets_one_termination() {
        let with = |extra: &str| -> Manifest {
            toml::from_str(&format!(
                "[dut]\nmodule = \"dut_top\"\n\n[pin]\no_tx = \"uart_tx\"\n\n{extra}"
            ))
            .unwrap()
        };
        for (extra, other) in [
            ("[leave_open]\nports = [\"o_tx\"]\n", "leave_open"),
            ("[tie_off]\no_tx = 0\n", "tie_off"),
        ] {
            let err = with(extra).validate().unwrap_err();
            assert!(
                matches!(&err, ManifestError::PinAndOther { port, other: o } if port == "o_tx" && *o == other),
                "{err:?}"
            );
        }
        let err = with("[leave_open]\nports = [\"o_tx\"]\n\n[tie_off]\no_tx = 0\n")
            .validate()
            .unwrap_err();
        assert!(matches!(err, ManifestError::TieAndOpen { .. }), "{err:?}");
        assert!(with("").validate().is_ok());
    }

    #[test]
    fn a_bar_that_is_not_a_power_of_two_is_refused() {
        for bytes in [0u32, 128, 2048, 3000] {
            let text = format!("[dut]\nmodule = \"dut_top\"\n\n[pcie]\nbar_bytes = {bytes}\n");
            let m: Manifest = toml::from_str(&text).unwrap();
            let err = m.validate().unwrap_err();
            let help = miette::Diagnostic::help(&err)
                .map(|h| h.to_string())
                .unwrap_or_default();
            assert!(format!("{err}").contains("cannot be a BAR"), "{err}");
            assert!(help.contains("bar_bytes = 4096"), "{help}");
        }
        // A power of two of 4096 or more is accepted.
        let m: Manifest =
            toml::from_str("[dut]\nmodule = \"dut_top\"\n\n[pcie]\nbar_bytes = 4096\n").unwrap();
        assert!(m.validate().is_ok());
    }

    #[test]
    fn a_pci_id_wider_than_16_bits_is_refused() {
        let m: Manifest =
            toml::from_str("[dut]\nmodule = \"dut_top\"\n\n[pcie]\nvendor_id = 0x1_0000\n")
                .unwrap();
        let err = m.validate().unwrap_err();
        assert!(format!("{err}").contains("vendor_id"), "{err}");
    }

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    const GOOD: &str = r#"
[dut]
module = "dut_top"

[bundle.dmem]
contract = "fixed_latency"
latency  = 2
backing  = "bram"

[bundle.csr]
backing = "reg"

[bundle.dbg]
backing = "observe"
"#;

    #[test]
    fn loads_harness_toml_from_the_base_dir() {
        let dir = dir_with(&[(MANIFEST_NAME, GOOD)]);
        let loaded = load(dir.path(), None).unwrap();

        assert_eq!(loaded.manifest.dut.module, "dut_top");
        assert_eq!(loaded.path, dir.path().join(MANIFEST_NAME));

        let dmem = &loaded.manifest.bundle["dmem"];
        assert_eq!(dmem.contract, Some(Contract::FixedLatency));
        assert_eq!(dmem.latency, Some(2));
        assert_eq!(dmem.backing, Backing::Bram);

        // An omitted contract stays None. No default is filled in.
        assert_eq!(loaded.manifest.bundle["csr"].contract, None);
    }

    #[test]
    fn bundles_keep_a_deterministic_order() {
        let dir = dir_with(&[(MANIFEST_NAME, GOOD)]);
        let loaded = load(dir.path(), None).unwrap();

        let names: Vec<&str> = loaded.manifest.bundle.keys().map(String::as_str).collect();
        assert_eq!(names, ["csr", "dbg", "dmem"]);
    }

    #[test]
    fn explicit_path_wins_over_the_searched_name() {
        let dir = dir_with(&[
            (MANIFEST_NAME, GOOD),
            ("elsewhere.toml", "[dut]\nmodule = \"other_top\"\n"),
        ]);
        let loaded = load(dir.path(), Some(&dir.path().join("elsewhere.toml"))).unwrap();

        assert_eq!(loaded.manifest.dut.module, "other_top");
    }

    #[test]
    fn explicit_path_that_is_missing_is_an_error() {
        let dir = dir_with(&[(MANIFEST_NAME, GOOD)]);
        let err = load(dir.path(), Some(&dir.path().join("nope.toml"))).unwrap_err();

        assert!(matches!(err, ManifestError::ExplicitUnreadable { .. }));
    }

    /// The lowercase name is not searched, but it is not ignored either.
    #[test]
    fn lowercase_name_is_reported_not_ignored() {
        let dir = dir_with(&[(MANIFEST_NAME_WRONG_CASE, GOOD)]);
        let err = load(dir.path(), None).unwrap_err();

        assert!(matches!(err, ManifestError::WrongCase { .. }));
        // The message carries the fix.
        let help = format!("{:?}", miette::Report::new(err));
        assert!(help.contains(MANIFEST_NAME));
    }

    #[test]
    fn correct_name_takes_precedence_over_the_wrong_case() {
        let dir = dir_with(&[
            (MANIFEST_NAME, GOOD),
            (MANIFEST_NAME_WRONG_CASE, "[dut]\nmodule = \"wrong\"\n"),
        ]);
        let loaded = load(dir.path(), None).unwrap();

        assert_eq!(loaded.manifest.dut.module, "dut_top");
    }

    #[test]
    fn metadata_section_is_rejected_rather_than_skipped() {
        let veryl = r#"
[project]
name    = "demo"
version = "0.1.0"

[metadata.harness]
dut = { module = "dut_top" }
"#;
        let dir = dir_with(&[(VERYL_MANIFEST_NAME, veryl)]);
        let err = load(dir.path(), None).unwrap_err();

        assert!(matches!(
            err,
            ManifestError::MetadataSectionUnsupported { .. }
        ));
    }

    #[test]
    fn an_unrelated_metadata_section_does_not_trigger_the_error() {
        let veryl = r#"
[project]
name    = "demo"
version = "0.1.0"

[metadata.other_tool]
whatever = true
"#;
        let dir = dir_with(&[(VERYL_MANIFEST_NAME, veryl)]);
        let err = load(dir.path(), None).unwrap_err();

        assert!(matches!(err, ManifestError::NotFound { .. }));
    }

    #[test]
    fn nothing_found_suggests_a_scaffold() {
        let dir = dir_with(&[]);
        let err = load(dir.path(), None).unwrap_err();

        assert!(matches!(err, ManifestError::NotFound { .. }));
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("[dut]"));
        assert!(rendered.contains("--config"));
    }

    #[test]
    fn fixed_latency_requires_a_latency() {
        let dir = dir_with(&[(
            MANIFEST_NAME,
            "[dut]\nmodule = \"t\"\n\n[bundle.m]\ncontract = \"fixed_latency\"\nbacking = \"bram\"\n",
        )]);
        let err = load(dir.path(), None).unwrap_err();

        assert!(matches!(err, ManifestError::LatencyRequired { .. }));
    }

    #[test]
    fn latency_on_a_flow_controlled_contract_is_rejected() {
        let dir = dir_with(&[(
            MANIFEST_NAME,
            "[dut]\nmodule = \"t\"\n\n[bundle.m]\ncontract = \"valid_ready\"\nlatency = 2\nbacking = \"bram\"\n",
        )]);
        let err = load(dir.path(), None).unwrap_err();

        assert!(matches!(
            err,
            ManifestError::LatencyNotAllowed {
                contract: Contract::ValidReady,
                ..
            }
        ));
    }

    /// The ports decide the contract, so `fixed_latency` need not be written.
    #[test]
    fn latency_without_a_contract_is_accepted() {
        let dir = dir_with(&[(
            MANIFEST_NAME,
            "[dut]\nmodule = \"t\"\n\n[bundle.m]\nlatency = 2\nbacking = \"bram\"\n",
        )]);
        let loaded = load(dir.path(), None).unwrap();

        assert_eq!(loaded.manifest.bundle["m"].latency, Some(2));
        assert_eq!(loaded.manifest.bundle["m"].contract, None);
    }

    #[test]
    fn the_old_backing_name_names_its_replacement() {
        let dir = dir_with(&[(
            MANIFEST_NAME,
            "[dut]\nmodule = \"t\"\n\n[bundle.tx]\ncontract = \"valid_only\"\nbacking = \"host_fifo\"\n",
        )]);
        let err = load(dir.path(), None).unwrap_err();
        let text = format!("{err:?}");
        assert!(text.contains("host_poll_fifo"), "{text}");
    }

    #[test]
    fn an_unknown_backing_lists_the_valid_values() {
        let dir = dir_with(&[(
            MANIFEST_NAME,
            "[dut]\nmodule = \"t\"\n\n[bundle.m]\nbacking = \"hostmem\"\n",
        )]);
        let err = load(dir.path(), None).unwrap_err();

        let ManifestError::Parse { message, .. } = &err else {
            panic!("expected a parse error, got {err:?}");
        };
        assert!(message.contains("host_mem"), "message was: {message}");
        assert!(message.contains("bram_preload"), "message was: {message}");
    }

    #[test]
    fn a_typo_in_a_key_is_rejected() {
        let dir = dir_with(&[(
            MANIFEST_NAME,
            "[dut]\nmodule = \"t\"\n\n[bundle.m]\nbackng = \"bram\"\n",
        )]);
        let err = load(dir.path(), None).unwrap_err();

        assert!(matches!(err, ManifestError::Parse { .. }));
    }

    #[test]
    fn valid_only_is_accepted() {
        let dir = dir_with(&[(
            MANIFEST_NAME,
            "[dut]\nmodule = \"t\"\n\n[bundle.uart_tx]\ncontract = \"valid_only\"\nbacking = \"host_poll_fifo\"\n",
        )]);
        let loaded = load(dir.path(), None).unwrap();

        assert_eq!(
            loaded.manifest.bundle["uart_tx"].contract,
            Some(Contract::ValidOnly)
        );
    }

    /// `valid_only` has no flow control, but it is not a fixed latency.
    #[test]
    fn latency_on_valid_only_is_rejected() {
        let dir = dir_with(&[(
            MANIFEST_NAME,
            "[dut]\nmodule = \"t\"\n\n[bundle.m]\ncontract = \"valid_only\"\nlatency = 1\nbacking = \"host_poll_fifo\"\n",
        )]);
        let err = load(dir.path(), None).unwrap_err();

        assert!(matches!(
            err,
            ManifestError::LatencyNotAllowed {
                contract: Contract::ValidOnly,
                ..
            }
        ));
    }

    #[test]
    fn an_empty_module_name_is_rejected() {
        let dir = dir_with(&[(MANIFEST_NAME, "[dut]\nmodule = \"\"\n")]);
        let err = load(dir.path(), None).unwrap_err();

        assert!(matches!(err, ManifestError::EmptyModule));
    }
}

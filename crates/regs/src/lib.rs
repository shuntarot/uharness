//! Reads `regs.json`.
//!
//! The generator writes it and the host client reads it. This crate is the
//! reader, kept separate as the contract between the two. The generator has a
//! test that reads its own output back with these types (`tests/regs_json.rs`),
//! so `cargo test` fails when writer and reader drift apart.
//!
//! The first two window words are `magic` and `map_hash`. The host uses them
//! to check that the local map belongs to the bitstream on the board.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The `regs.json` format this crate reads. It must equal the generator's
/// `regmap::FORMAT_VERSION`; change both together.
pub const FORMAT_VERSION: u32 = 6;

/// The constant in the first window word ("VHRN").
pub const MAGIC: u32 = 0x5648_524e;

#[derive(Debug)]
pub enum Error {
    Json(serde_json::Error),
    /// Another format version. Reading a changed field with the old meaning
    /// is the worst failure for this file, so it is refused.
    FormatVersion {
        found: u32,
        expected: u32,
    },
    UnknownRegister {
        name: String,
        known: Vec<String>,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Json(err) => write!(f, "regs.json is not valid JSON: {err}"),
            Error::FormatVersion { found, expected } => write!(
                f,
                "regs.json has format_version {found}, but this client reads {expected}.\n\
                 Regenerate with a matching veryl-harness (`veryl harness gen`), or use a \
                 client built from the same commit."
            ),
            Error::UnknownRegister { name, known } => {
                write!(f, "no register named `{name}`. Known: {}", known.join(", "))
            }
        }
    }
}

impl std::error::Error for Error {}

/// `ro` registers are DUT outputs; the host cannot write them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Ro,
    Rw,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SelfClearing {
    pub on: String,
    pub from: String,
    pub role: String,
    pub invert: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Register {
    pub name: String,
    pub kind: String,
    /// Byte offset from the start of the window.
    pub offset: usize,
    /// Number of words. Word `i` holds bits `[32i+31 : 32i]`.
    pub words: usize,
    pub width: usize,
    pub access: Access,
    pub bundle: Option<String>,
    pub value: Option<u64>,
    pub role: Option<String>,
    pub self_clearing: Option<SelfClearing>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterMap {
    pub marker: String,
    pub format_version: u32,
    pub generator: String,
    pub dut: String,
    /// The target the map was generated for, so that the host can omit
    /// `--target`. Set only when the target was given by name (`--target`).
    #[serde(default)]
    pub target: Option<String>,
    /// How the target was given: `"name"` or `"file"` (`--target-file`).
    /// A `"file"` map does not record which description: the path depends on
    /// the machine, and the file may change after generation. The host then
    /// asks for `--target-file`. Absent in maps without a target and in older
    /// maps.
    #[serde(default)]
    pub target_source: Option<String>,
    /// The window clock (MHz), and the window cycles the JTAG bridge needs
    /// per command. The host uses both to choose the TCK gap between scans
    /// (`update_gap` in `hns-host`). Absent in maps without a target and in
    /// older maps; the host then adds no gap.
    #[serde(default)]
    pub window_clock_mhz: Option<f64>,
    #[serde(default)]
    pub window_cycles: Option<u32>,
    pub word_bits: usize,
    pub size_bytes: usize,
    pub magic: u32,
    pub map_hash: u32,
    pub registers: Vec<Register>,

    /// Contiguous ranges in the window: terminators where the address is part
    /// of the access. Unlike the indirect ports (`<bundle>_maddr` /
    /// `_mdata`), they hold no shared state, so two window masters do not
    /// disturb each other. Empty when the design has none.
    #[serde(default)]
    pub regions: Vec<Region>,

    /// Present only for a PCIe design; the host needs it to find the card.
    ///
    /// Absent means the design has no PCIe port. On a JTAG design, it would
    /// make the host search for a device that is not there.
    #[serde(default)]
    pub pcie: Option<Pcie>,
}

/// The IDs the bitstream presents, and the BAR size.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Pcie {
    pub vendor_id: u32,
    pub device_id: u32,
    /// The class code, as sysfs `class` shows it. Absent in older `regs.json`;
    /// the host then does not compare it.
    #[serde(default)]
    pub class_code: Option<u32>,
    /// BAR0 size in bytes. The host compares it with the size sysfs reports;
    /// a mismatch means another bitstream is loaded.
    pub bar_bytes: u32,
    /// The bundle the card's DMA engine (`dma_*`) reaches. Using it for
    /// another bundle reads and writes a different memory.
    ///
    /// Absent for a design without a DMA engine and in older `regs.json`.
    /// In both cases the target is unknown, so the host does not use it.
    #[serde(default)]
    pub requester: Option<String>,
}

/// A contiguous range in the window. Addresses start at 0 inside it; the host
/// adds `base`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Region {
    /// Bundle name. The host looks regions up by it.
    pub name: String,

    /// What is behind the range: `memory` (harness memory) or `dut` (a DUT
    /// slave interface).
    ///
    /// This decides whether a batch may be sent again. `memory` gives the
    /// same result twice, so a batch the bridge dropped can be resent. A read
    /// of `dut` may change state, so it is not resent.
    ///
    /// Absent in older `regs.json`; it is then treated as `dut`, the safe
    /// side.
    #[serde(default)]
    pub kind: Option<String>,

    /// Byte offset from the start of the window. It is aligned to the region
    /// size, so the RTL decodes it with a mask.
    pub base: usize,

    /// Size in the window, in bytes. A power of two.
    pub size_bytes: usize,

    /// Bytes per entry. The window is 32 bits wide, so a wider entry takes
    /// several words.
    pub entry_bytes: usize,

    pub depth: u64,

    /// Bytes in the whole memory. When larger than `size_bytes`, the region
    /// is a moving aperture: only `size_bytes` are visible at a time, and the
    /// `<name>_base_<master>` register selects which part.
    ///
    /// Absent in older `regs.json`; the region then does not move
    /// (`size_bytes` is everything).
    #[serde(default)]
    pub total_bytes: Option<u64>,
}

impl RegisterMap {
    /// Looks up a region by name. Registers and regions share one name
    /// space, so a name is in at most one of them.
    pub fn region(&self, name: &str) -> Option<&Region> {
        self.regions.iter().find(|region| region.name == name)
    }
}

impl Region {
    /// Whether the same access twice gives the same result.
    ///
    /// An unknown kind counts as not idempotent: an older `regs.json` has no
    /// `kind`, and its regions may include DUT slave interfaces.
    pub fn is_idempotent(&self) -> bool {
        self.kind.as_deref() == Some("memory")
    }

    /// Total bytes that can be reached: the whole memory.
    ///
    /// Not `size_bytes`: that is the window footprint, rounded up to a power
    /// of two, and the rounded part cannot be read.
    pub fn reach_bytes(&self) -> u64 {
        self.total_bytes
            .unwrap_or(self.depth * self.entry_bytes as u64)
    }

    /// Whether this region is a moving aperture.
    pub fn is_aperture(&self) -> bool {
        self.reach_bytes() > self.size_bytes as u64
    }

    /// Name of the base register this transport (`master`) uses.
    pub fn base_register(&self, master: &str) -> String {
        format!("{}_base_{master}", self.name)
    }
}

/// Reads a size written as `4096`, `4k` or `256M`.
///
/// `Harness.toml` and the command line both use it, so one spelling has one
/// meaning everywhere.
///
/// Suffixes are 1024-based (`k` = 1024, `m` = 1024^2, `g` = 1024^3), in
/// either case. These sizes are powers of two, so `4k` = 4000 would surprise.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (digits, scale) = match text.chars().last() {
        Some('k') | Some('K') => (&text[..text.len() - 1], 1024u64),
        Some('m') | Some('M') => (&text[..text.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    digits
        .trim()
        .replace('_', "")
        .parse::<u64>()
        .map_err(|_| {
            format!(
                "`{text}` is not a size. Write a number, optionally with `k`, `m` or `g` (1024-based): 4096, `4k`, `256M`."
            )
        })
        .and_then(|n| {
            n.checked_mul(scale)
                .ok_or_else(|| format!("`{text}` does not fit in 64 bits."))
        })
}

/// Reads a rate written as `15000000`, `15M` or `48m`.
///
/// Suffixes are 1000-based here, unlike `parse_size`: `15M` Hz must be
/// 15,000,000. The two stay separate functions so that the caller always
/// knows which rule applies.
pub fn parse_rate(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (digits, scale) = match text.chars().last() {
        Some('k') | Some('K') => (&text[..text.len() - 1], 1_000u64),
        Some('m') | Some('M') => (&text[..text.len() - 1], 1_000_000),
        Some('g') | Some('G') => (&text[..text.len() - 1], 1_000_000_000),
        _ => (text, 1),
    };
    digits
        .trim()
        .replace('_', "")
        .parse::<u64>()
        .map_err(|_| {
            format!(
                "`{text}` is not a rate. Write a number, optionally with `k`, `m` or `g` (1000-based): 15000000, `15M`, `48m`."
            )
        })
        .and_then(|n| {
            n.checked_mul(scale)
                .ok_or_else(|| format!("`{text}` does not fit in 64 bits."))
        })
}

/// Address bits needed to cover the window.
///
/// The generator (the RTL `AW`) and the host (the DR length) must agree. If
/// they do not, every shifted bit moves, and on the board this only looks
/// like corrupted TDO. So the rule lives here once, and both call it.
pub fn addr_bits(size_bytes: usize) -> usize {
    let mut bits = 1;
    while (1usize << bits) < size_bytes {
        bits += 1;
    }
    bits
}

impl RegisterMap {
    /// Address width of the DR and `hns::bscan` for this window.
    pub fn addr_bits(&self) -> usize {
        addr_bits(self.size_bytes)
    }

    /// Fails on another format version.
    pub fn from_json(text: &str) -> Result<Self, Error> {
        let map: RegisterMap = serde_json::from_str(text).map_err(Error::Json)?;
        if map.format_version != FORMAT_VERSION {
            return Err(Error::FormatVersion {
                found: map.format_version,
                expected: FORMAT_VERSION,
            });
        }
        Ok(map)
    }

    /// Looks up a register by name. The error lists the known names.
    pub fn get(&self, name: &str) -> Result<&Register, Error> {
        self.registers
            .iter()
            .find(|register| register.name == name)
            .ok_or_else(|| Error::UnknownRegister {
                name: name.to_string(),
                known: self.registers.iter().map(|r| r.name.clone()).collect(),
            })
    }

    /// Name -> register, for many lookups.
    pub fn by_name(&self) -> BTreeMap<&str, &Register> {
        self.registers
            .iter()
            .map(|register| (register.name.as_str(), register))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version comes from the constant, so a version bump does not break
    /// the sample. The test is about a matching version, not a number.
    fn sample() -> String {
        SAMPLE.replace("__VERSION__", &FORMAT_VERSION.to_string())
    }

    const SAMPLE: &str = r#"{
        "marker": "veryl-harness:generated",
        "format_version": __VERSION__,
        "generator": "veryl-harness",
        "dut": "dut_top",
        "target": "digilent/arty-a7-35",
        "word_bits": 32,
        "size_bytes": 16,
        "magic": 1447580238,
        "map_hash": 305419896,
        "registers": [
            {"name": "harness_magic", "kind": "identity", "offset": 0, "words": 1,
             "width": 32, "access": "ro", "bundle": null, "value": 1447580238,
             "role": null, "self_clearing": null},
            {"name": "i_data", "kind": "port", "offset": 8, "words": 1,
             "width": 8, "access": "rw", "bundle": "csr", "value": null,
             "role": null, "self_clearing": null}
        ]
    }"#;

    #[test]
    fn a_map_reads_back_with_its_registers() {
        let map = RegisterMap::from_json(&sample()).unwrap();
        assert_eq!(map.magic, MAGIC);
        assert_eq!(map.get("i_data").unwrap().offset, 8);
        assert_eq!(map.get("i_data").unwrap().access, Access::Rw);
        assert!(map.get("harness_magic").unwrap().bundle.is_none());
        // The host uses this to omit `--target`.
        assert_eq!(map.target.as_deref(), Some("digilent/arty-a7-35"));
    }

    #[test]
    fn a_different_format_version_is_refused() {
        let text = SAMPLE.replace("__VERSION__", "99");
        let err = RegisterMap::from_json(&text).unwrap_err();
        assert!(matches!(err, Error::FormatVersion { found: 99, .. }));
        assert!(err.to_string().contains("veryl harness gen"), "{err}");
    }

    #[test]
    fn an_unknown_register_lists_what_exists() {
        let map = RegisterMap::from_json(&sample()).unwrap();
        let err = map.get("i_dta").unwrap_err();
        assert!(err.to_string().contains("i_data"), "{err}");
    }

    /// The error quotes the text as typed. `_` is skipped here, not by the
    /// caller; otherwise the error would quote `memcalib`, which nobody
    /// typed.
    #[test]
    fn a_refusal_quotes_what_was_typed() {
        let err = parse_size("mem_calib").unwrap_err();
        assert!(err.contains("`mem_calib`"), "{err}");

        let err = parse_rate("mem_calib").unwrap_err();
        assert!(err.contains("`mem_calib`"), "{err}");

        // As a digit separator, `_` is skipped.
        assert_eq!(parse_size("1_048_576").unwrap(), 1024 * 1024);
        assert_eq!(parse_rate("15_000_000").unwrap(), 15_000_000);
    }
}

#[cfg(test)]
mod addr_bits_tests {
    use super::addr_bits;

    /// Must match the generator's `bits_for`, or RTL and host disagree on the
    /// DR length.
    #[test]
    fn the_width_covers_the_window_and_no_more() {
        assert_eq!(addr_bits(1), 1);
        assert_eq!(addr_bits(2), 1);
        assert_eq!(addr_bits(3), 2);
        assert_eq!(addr_bits(108), 7);
        assert_eq!(addr_bits(128), 7);
        assert_eq!(addr_bits(129), 8);
        for n in 1..1000 {
            assert!(1usize << addr_bits(n) >= n, "{n}");
        }
    }
}

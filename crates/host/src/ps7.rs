//! Sets up a Zynq-7000 PS from the `ps7_init.tcl` Vivado writes.
//!
//! The PS's DDR controller, PLLs and the level shifters between the PS and
//! the PL are set by software after every power-up, normally the FSBL. With
//! no program on the CPUs, the host does it over JTAG instead, through the
//! ARM DAP (`adi`). The file is Vivado's own output for the board, so the
//! values are never written by hand.
//!
//! Only what the file's register procs use is read: `mwr -force`,
//! `mask_write`, `mask_poll` and `mask_delay`. Anything else stops the
//! parse: skipping a step would leave the PS half set up, with no sign of it.

use std::collections::BTreeMap;
use std::time::Duration;

/// One register step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Write the whole word.
    Write { addr: u32, value: u32 },
    /// Read, replace the bits in `mask` with those of `value`, write back.
    MaskWrite { addr: u32, mask: u32, value: u32 },
    /// Wait until a bit in `mask` reads as 1.
    Poll { addr: u32, mask: u32 },
    /// Wait this long. The file times it with the CPUs' private timer, which
    /// the DAP cannot reach, so the host waits instead.
    Delay(Duration),
}

/// The stages a `ps7_init.tcl` defines, in the order `ps7_init` runs them,
/// and then the post-configuration step.
pub const INIT: [&str; 5] = [
    "mio_init_data",
    "pll_init_data",
    "clock_init_data",
    "ddr_init_data",
    "peripherals_init_data",
];
pub const POST: &str = "post_config";

/// A parsed `ps7_init.tcl`.
#[derive(Debug, Default)]
pub struct Ps7Init {
    /// `proc ps7_<stage>_<ver>` bodies, by (stage, version suffix like `3_0`).
    procs: BTreeMap<(String, String), Vec<Step>>,
    /// Silicon version code (from the devcfg's MCTRL) to version suffix.
    versions: BTreeMap<u32, String>,
}

/// Where the silicon version is: devcfg MCTRL, bits 31:28.
pub const PS_VERSION_ADDR: u32 = 0xf800_7080;

impl Ps7Init {
    pub fn parse(text: &str) -> Result<Ps7Init, String> {
        let mut out = Ps7Init::default();
        let mut current: Option<((String, String), Vec<Step>)> = None;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            let at = || format!("line {} of ps7_init.tcl", n + 1);
            if let Some((key, steps)) = current.as_mut() {
                if line == "}" {
                    let (key, steps) = (key.clone(), std::mem::take(steps));
                    out.procs.insert(key, steps);
                    current = None;
                    continue;
                }
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let step = parse_step(line).ok_or_else(|| {
                    format!(
                        "{}: `{line}` in `ps7_{}_{}` is not a step hio knows \
                         (mwr -force, mask_write, mask_poll, mask_delay).\n\
                         Is this a ps7_init.tcl that Vivado wrote for a Zynq-7000?",
                        at(),
                        key.0,
                        key.1
                    )
                })?;
                steps.push(step);
                continue;
            }
            // `proc ps7_ddr_init_data_3_0 {} {`
            if let Some(rest) = line.strip_prefix("proc ps7_")
                && let Some(name) = rest.strip_suffix(" {} {")
                && let Some((stage, ver)) = split_version(name)
                && (INIT.contains(&stage) || stage == POST)
            {
                current = Some(((stage.to_string(), ver.to_string()), Vec::new()));
                continue;
            }
            // `set PCW_SILICON_VER_3_0 "0x2"`
            if let Some(rest) = line.strip_prefix("set PCW_SILICON_VER_")
                && let Some((ver, code)) = rest.split_once(' ')
                && let Some(code) = number(code.trim_matches('"'))
            {
                out.versions.insert(code, ver.to_string());
            }
        }
        if current.is_some() {
            return Err("ps7_init.tcl ends inside a proc".to_string());
        }
        if out.versions.is_empty() || out.procs.is_empty() {
            return Err(
                "this file has no ps7 init procs or no PCW_SILICON_VER_* values. \
                 Pass the ps7_init.tcl from the PS7 IP's output products."
                    .to_string(),
            );
        }
        Ok(out)
    }

    /// The version suffix for a silicon version code. Like the file's own
    /// `ps7_init`, a code it does not list takes the newest version.
    pub fn version(&self, code: u32) -> &str {
        self.versions
            .get(&code)
            .or_else(|| self.versions.values().max())
            .map(String::as_str)
            .unwrap_or("")
    }

    /// The steps of one stage for one version.
    pub fn stage(&self, stage: &str, ver: &str) -> Result<&[Step], String> {
        self.procs
            .get(&(stage.to_string(), ver.to_string()))
            .map(Vec::as_slice)
            .ok_or_else(|| format!("ps7_init.tcl has no `ps7_{stage}_{ver}`"))
    }
}

/// `ddr_init_data_3_0` -> (`ddr_init_data`, `3_0`).
fn split_version(name: &str) -> Option<(&str, &str)> {
    let (head, minor) = name.rsplit_once('_')?;
    let (stage, major) = head.rsplit_once('_')?;
    if major.chars().all(|c| c.is_ascii_digit()) && minor.chars().all(|c| c.is_ascii_digit()) {
        Some((stage, &name[stage.len() + 1..]))
    } else {
        None
    }
}

fn number(s: &str) -> Option<u32> {
    let s = s.trim();
    match s.strip_prefix("0X").or_else(|| s.strip_prefix("0x")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn parse_step(line: &str) -> Option<Step> {
    let w: Vec<&str> = line.split_whitespace().collect();
    match w.as_slice() {
        ["mwr", "-force", addr, value] => Some(Step::Write {
            addr: number(addr)?,
            value: number(value)?,
        }),
        ["mask_write", addr, mask, value] => Some(Step::MaskWrite {
            addr: number(addr)?,
            mask: number(mask)?,
            value: number(value)?,
        }),
        ["mask_poll", addr, mask] => Some(Step::Poll {
            addr: number(addr)?,
            mask: number(mask)?,
        }),
        ["mask_delay", _, ms] => Some(Step::Delay(Duration::from_millis(number(ms)? as u64))),
        _ => None,
    }
}

/// Memory the steps run against.
pub trait Mem {
    type Error;
    fn read32(&mut self, addr: u32) -> Result<u32, Self::Error>;
    fn write32(&mut self, addr: u32, value: u32) -> Result<(), Self::Error>;
}

/// Why a stage did not finish.
#[derive(Debug)]
pub enum RunError<E> {
    Mem(E),
    /// A poll never saw its bit.
    Poll {
        addr: u32,
        mask: u32,
        last: u32,
    },
}

/// How long a poll may take. PLL lock and DDR initialisation take
/// microseconds to milliseconds; a second means it is not coming.
pub const POLL_LIMIT: Duration = Duration::from_secs(1);

/// Run steps in order.
pub fn run<M: Mem>(mem: &mut M, steps: &[Step]) -> Result<(), RunError<M::Error>> {
    for step in steps {
        match *step {
            Step::Write { addr, value } => mem.write32(addr, value).map_err(RunError::Mem)?,
            Step::MaskWrite { addr, mask, value } => {
                let old = mem.read32(addr).map_err(RunError::Mem)?;
                mem.write32(addr, (old & !mask) | (value & mask))
                    .map_err(RunError::Mem)?;
            }
            Step::Poll { addr, mask } => {
                let started = std::time::Instant::now();
                loop {
                    let got = mem.read32(addr).map_err(RunError::Mem)?;
                    if got & mask != 0 {
                        break;
                    }
                    if started.elapsed() > POLL_LIMIT {
                        return Err(RunError::Poll {
                            addr,
                            mask,
                            last: got,
                        });
                    }
                }
            }
            Step::Delay(d) => std::thread::sleep(d),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SAMPLE: &str = r#"proc ps7_pll_init_data_3_0 {} {
    mwr -force 0XF8000008 0x0000DF0D
    mask_write 0XF8000100 0x00000010 0x00000010
    mask_poll 0XF800010C 0x00000001
    mwr -force 0XF8000004 0x0000767B
}
proc ps7_peripherals_init_data_3_0 {} {
    mask_delay 0XF8F00200 1
}
proc ps7_post_config_3_0 {} {
    mask_write 0XF8000900 0x0000000F 0x0000000F
}
proc ps7_debug_3_0 {} {
    mwr -force 0XF8898FB0 0xC5ACCE55
}
proc ps7_pll_init_data_2_0 {} {
    mwr -force 0XF8000008 0x0000DF0D
}
set PCW_SILICON_VER_2_0 "0x1"
set PCW_SILICON_VER_3_0 "0x2"
proc mask_poll { addr mask } {
    set count 1
}
"#;

    #[test]
    fn the_register_procs_are_read_per_version() {
        let p = Ps7Init::parse(SAMPLE).unwrap();
        assert_eq!(p.version(2), "3_0");
        assert_eq!(p.version(1), "2_0");
        // A code the file does not list takes the newest version.
        assert_eq!(p.version(7), "3_0");
        assert_eq!(
            p.stage("pll_init_data", "3_0").unwrap(),
            [
                Step::Write {
                    addr: 0xf800_0008,
                    value: 0xdf0d
                },
                Step::MaskWrite {
                    addr: 0xf800_0100,
                    mask: 0x10,
                    value: 0x10
                },
                Step::Poll {
                    addr: 0xf800_010c,
                    mask: 1
                },
                Step::Write {
                    addr: 0xf800_0004,
                    value: 0x767b
                },
            ]
        );
        assert_eq!(
            p.stage("peripherals_init_data", "3_0").unwrap(),
            [Step::Delay(Duration::from_millis(1))]
        );
        // `ps7_debug_*` is not a stage hio runs, so it is not read at all.
        assert!(p.stage("debug", "3_0").is_err());
        assert!(p.stage("ddr_init_data", "3_0").is_err());
    }

    #[test]
    fn an_unknown_command_in_a_stage_stops_the_parse() {
        let text = "proc ps7_ddr_init_data_3_0 {} {\n    mrd 0xF8006000\n}\n";
        let err = Ps7Init::parse(text).unwrap_err();
        assert!(err.contains("mrd 0xF8006000"), "{err}");
    }

    struct FakeMem {
        words: HashMap<u32, u32>,
    }

    impl Mem for FakeMem {
        type Error = ();
        fn read32(&mut self, addr: u32) -> Result<u32, ()> {
            Ok(*self.words.get(&addr).unwrap_or(&0))
        }
        fn write32(&mut self, addr: u32, value: u32) -> Result<(), ()> {
            self.words.insert(addr, value);
            Ok(())
        }
    }

    #[test]
    fn a_mask_write_keeps_the_other_bits() {
        let mut mem = FakeMem {
            words: HashMap::from([(0x100, 0xffff_0000), (0x10c, 1)]),
        };
        run(
            &mut mem,
            &[
                Step::MaskWrite {
                    addr: 0x100,
                    mask: 0x0000_ff00,
                    value: 0x1234_5678,
                },
                Step::Poll {
                    addr: 0x10c,
                    mask: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(mem.words[&0x100], 0xffff_5600);
    }

    #[test]
    fn a_poll_that_never_sees_its_bit_gives_up() {
        let mut mem = FakeMem {
            words: HashMap::new(),
        };
        let err = run(
            &mut mem,
            &[Step::Poll {
                addr: 0x10c,
                mask: 1,
            }],
        )
        .unwrap_err();
        assert!(matches!(err, RunError::Poll { addr: 0x10c, .. }));
    }

    /// The file Vivado wrote for the PYNQ-Z1, if it is around.
    #[test]
    fn a_real_file_parses() {
        let path = std::env::var("PS7_INIT_TCL").unwrap_or_default();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let p = Ps7Init::parse(&text).unwrap();
        let ver = p.version(2).to_string();
        for stage in INIT.iter().chain([&POST]) {
            assert!(!p.stage(stage, &ver).unwrap().is_empty(), "{stage}");
        }
    }
}

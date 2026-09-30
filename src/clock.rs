//! The clock and reset plan.
//!
//! The target provides a physical clock source. The DUT asks for logical clock
//! domains and frequencies. The harness generates the MMCM between them.
//!
//! This module only plans how many outputs there are, at what frequency, and
//! which ports they drive. It does not solve M/D/O: the tool keeps no device
//! limits, so it passes the frequencies to `clk_wiz` and Vivado solves them.
//!
//! ## Ports in one domain must share one clock
//!
//! Two MMCM outputs are two physical clocks, even at the same frequency. If
//! ports in one explicit domain (`'s`) got separate outputs, the DUT's domain
//! annotation would be false. So ports are grouped by domain, and one domain
//! with two frequencies is an error.
//!
//! Ports in `'_` (Implicit) are not grouped: `'_` means "any domain", not
//! "the same domain". A single-clock DUT is this case.

use std::collections::BTreeMap;

use miette::Diagnostic;
use thiserror::Error;

use crate::bundle::Binding;
use crate::dut::{Domain, Dut, SignalRole, bullet_list};
use crate::manifest::Manifest;
use crate::target::Target;

/// The clock plan.
#[derive(Debug)]
pub struct ClockPlan {
    /// The board clock source.
    pub input: InputClock,

    /// The board reset. Together with the MMCM `locked`, it makes the DUT reset.
    pub reset: InputReset,

    /// The MMCM outputs. Each one is one `CLKOUT` of `clk_wiz`.
    pub outputs: Vec<ClockOutput>,

    /// DUT reset port -> the clock output (`ident`) whose reset it gets.
    ///
    /// A reset belongs to one clock. If the clock cannot be decided, this is an
    /// error; it is never inferred.
    pub resets: Vec<(String, String)>,

    /// A board clock source that the memory controller takes directly.
    ///
    /// It does not go through the MMCM. Some boards have a clock pin only for
    /// memory (VCU118: 250 MHz differential), and the IP takes it directly. On
    /// Arty the MIG uses `No Buffer`, and the harness MMCM makes the clock. The
    /// path differs by board, so the target names the source in
    /// `provides.dram.sys_clk`. `None` means the MMCM makes the clock.
    pub controller_clock: Option<InputClock>,

    /// The window clock (an `ident` in `outputs`). The CSR, the terminators,
    /// the host-side DMA engines, the synchronizers and the heartbeat run on it.
    ///
    /// It is decided once, here, so that every part of the generator uses the
    /// same clock. Do not use `outputs[0]` instead: with two clocks it can be
    /// a different domain.
    pub window: String,
}

impl ClockPlan {
    /// The output of the window clock.
    pub fn window_output(&self) -> &ClockOutput {
        self.outputs
            .iter()
            .find(|output| output.ident == self.window)
            .expect("`window` names one of `outputs`")
    }
}

#[derive(Debug, PartialEq)]
pub struct InputReset {
    pub name: String,
    pub pin: Option<String>,
    pub standard: Option<String>,
    /// Taken from `active` in the target description. Never inferred.
    pub active_low: bool,
}

#[derive(Debug, PartialEq)]
pub struct InputClock {
    pub name: String,
    pub freq_mhz: f64,

    /// `pin` for a single-ended clock; `pin_p` / `pin_n` for a differential one.
    pub pin: Option<String>,
    pub pin_n: Option<String>,
    pub standard: Option<String>,

    /// Taken from `diff` in the target description. Never inferred.
    /// It changes the `clk_wiz` port names and the XDC.
    pub diff: bool,
}

#[derive(Debug, PartialEq)]
pub struct ClockOutput {
    /// The DUT ports this clock drives. Several ports if they share a domain.
    pub ports: Vec<String>,
    pub freq_mhz: f64,

    /// Why these ports were grouped. It is shown, so the user can see why they
    /// share one output.
    pub domain: String,

    /// The name used in the generated RTL: the domain name, or `c<n>`.
    pub ident: String,
}

#[derive(Debug, Error, Diagnostic)]
pub enum ClockError {
    #[error("clock port `{port}` has no frequency")]
    #[diagnostic(
        code(harness::clock::missing_frequency),
        help(
            "The harness makes this clock with an MMCM, and there is no default frequency. State it:\n\n    [clock.{port}]\n    freq_mhz = 100"
        )
    )]
    MissingFrequency { port: String },

    /// The key is a port name, because a single-clock DUT has no domain name
    /// to use (its ports are `'_`).
    #[error("[clock.{port}] names a port module `{module}` does not have")]
    #[diagnostic(
        code(harness::clock::unknown_port),
        help(
            "Clock ports of `{module}`:\n{candidates}\n\nThe key is a port name, not a domain name."
        )
    )]
    UnknownPort {
        port: String,
        module: String,
        candidates: String,
    },

    #[error("[clock.{port}] names `{port}`, which is not a clock")]
    #[diagnostic(
        code(harness::clock::not_a_clock),
        help(
            "`{port}` is {what}. Clock ports are the ones typed `clock` in the source. The harness drives them from its MMCM."
        )
    )]
    NotAClock { port: String, what: String },

    #[error("[clock.{port}] has freq_mhz = {freq}")]
    #[diagnostic(
        code(harness::clock::invalid_frequency),
        help(
            "A frequency has to be positive. Write the frequency the DUT needs on this port, in MHz."
        )
    )]
    InvalidFrequency { port: String, freq: f64 },

    /// Separate MMCM outputs would make the DUT's own domain annotation false.
    #[error("ports in clock domain `{domain}` ask for different frequencies")]
    #[diagnostic(
        code(harness::clock::domain_frequency_conflict),
        help(
            "{ports}\n\nPorts in one clock domain must share one clock. Give them the same frequency, or separate the domains in the DUT."
        )
    )]
    DomainFrequencyConflict { domain: String, ports: String },

    /// The harness window runs on a DUT clock, so a DUT without one cannot work.
    #[error("module `{module}` has no clock port")]
    #[diagnostic(
        code(harness::clock::no_clock_port),
        help(
            "The harness runs its registers and memories on the DUT's clock, so the DUT needs one. Wrap the DUT in a module with a `clock` port."
        )
    )]
    NoClockPort { module: String },

    /// The window, the CSR and the terminators run on one clock. It is not
    /// guessed.
    #[error("the terminated ports span more than one clock domain")]
    #[diagnostic(
        code(harness::clock::window_domain_ambiguous),
        help(
            "Domains: {domains}\n\nThe harness runs its registers and memories on one clock, and splitting them is not supported yet. Keep the terminated ports in one domain."
        )
    )]
    WindowDomainAmbiguous { domains: String },

    #[error("no terminated port says which of the DUT's clocks the harness runs on")]
    #[diagnostic(
        code(harness::clock::window_clock_ambiguous),
        help(
            "The DUT has several clocks:\n{clocks}\n\nThe harness runs its registers and memories on the clock of the ports it terminates, and none of them names a domain. Give those ports a clock domain in the DUT, e.g. `i_data: input 'a logic`."
        )
    )]
    WindowClockAmbiguous { clocks: String },

    #[error("the terminated ports are in domain `'{domain}`, which no clock port carries")]
    #[diagnostic(
        code(harness::clock::window_domain_has_no_clock),
        help(
            "The clocks are:\n{clocks}\n\nGive the DUT a clock port in `'{domain}`, or move the ports to a domain that has one."
        )
    )]
    WindowDomainHasNoClock { domain: String, clocks: String },

    #[error("the DUT's clock domain `'{domain}` has a name the harness uses itself")]
    #[diagnostic(
        code(harness::clock::domain_name_taken),
        help(
            "The harness names its own clocks `'sys` (the board), `'mig` (the memory controller) and `'pcie` (the PCIe block), and calls clocks without a domain `c0`, `c1`, ... Rename the domain in the DUT, for example `'core`."
        )
    )]
    DomainNameTaken { domain: String },

    #[error("cannot tell which clock reset port `{port}` belongs to")]
    #[diagnostic(
        code(harness::clock::reset_domain_ambiguous),
        help(
            "The harness releases each reset in its own clock domain. `{port}` is in domain `{domain}`, and the clocks are:\n{clocks}\n\nGive the DUT's reset the same clock domain as its clock."
        )
    )]
    ResetDomainAmbiguous {
        port: String,
        domain: String,
        clocks: String,
    },

    #[error("target `{target}` provides no reset")]
    #[diagnostic(
        code(harness::clock::no_input_reset),
        help(
            "The harness releases the DUT's reset from the board reset and the MMCM lock, so the description needs one:\n\n    [resets.sys]\n    pin    = \"C2\"\n    active = \"low\"\n\nThis is a fault in the description, not in your project."
        )
    )]
    NoInputReset { target: String },

    /// A reset with the wrong polarity holds the DUT in reset or never resets
    /// it. Both look like "the design does not work".
    #[error("[resets.{name}] of target `{target}` does not say which level asserts it")]
    #[diagnostic(
        code(harness::clock::reset_polarity_unknown),
        help(
            "Write it:\n\n    [resets.{name}]\n    active = \"low\"   # or \"high\"\n\nPolarity is not guessed."
        )
    )]
    ResetPolarityUnknown { target: String, name: String },

    #[error("target `{target}` provides {count} resets")]
    #[diagnostic(
        code(harness::clock::many_input_resets),
        help(
            "Resets: {names}\n\nChoosing among them is not supported yet. Patch the target down to the one you want:\n\n    --target-patch <path>"
        )
    )]
    ManyInputResets {
        target: String,
        count: usize,
        names: String,
    },

    #[error("target `{target}` provides no clock source")]
    #[diagnostic(
        code(harness::clock::no_input_clock),
        help(
            "A target description needs at least one:\n\n    [clocks.sys]\n    freq_mhz = 100\n    pin      = \"E3\"\n\nThis is a fault in the description, not in your project."
        )
    )]
    NoInputClock { target: String },

    #[error("target `{target}` provides {count} clock sources")]
    #[diagnostic(
        code(harness::clock::many_input_clocks),
        help(
            "Sources: {names}\n\nThe harness uses one board clock, and choosing among them is not supported yet. Patch the target down to the one you want:\n\n    --target-patch <path>"
        )
    )]
    ManyInputClocks {
        target: String,
        count: usize,
        names: String,
    },

    #[error(
        "target `{target}` says its memory controller takes the clock source `{name}`, which it does not have"
    )]
    #[diagnostic(
        code(harness::clock::no_such_clock_source),
        help(
            "Sources in `{target}`: {names}\n\n`[provides] dram.sys_clk` names the board clock that goes straight to the memory controller. Name one that exists, or remove the key and state `sys_clk_mhz` to make it with the harness MMCM."
        )
    )]
    NoSuchClockSource {
        target: String,
        name: String,
        names: String,
    },
}

/// Whether the generated RTL uses this domain name for itself.
///
/// `sys` is the board clock, `mig` the memory controller, and `pcie` the PCIe
/// hard block (see `emit`). `migsys` / `migref` name the controller clocks.
/// `c<n>` names a clock without a domain.
fn is_harness_domain(name: &str) -> bool {
    matches!(name, "sys" | "mig" | "pcie" | "migsys" | "migref")
        || name
            .strip_prefix('c')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// The clocks the memory controller needs, or none.
///
/// They come from the target description. `mig.prj` fixes the frequencies; they
/// are a board fact. A wrong value makes Vivado fail when it generates the IP,
/// so it cannot pass silently.
fn controller_clocks(manifest: &Manifest, target: &Target) -> Vec<ClockOutput> {
    let wants_dram = manifest
        .bundle
        .values()
        .any(|bundle| bundle.backing == crate::manifest::Backing::Dram);
    if !wants_dram {
        return Vec::new();
    }
    let Some(dram) = target
        .table
        .get("provides")
        .and_then(|x| x.as_table())
        .and_then(|provides| provides.get("dram"))
        .and_then(|x| x.as_table())
    else {
        return Vec::new();
    };
    let mhz = |key: &str| dram.get(key).and_then(value_as_f64);
    // A named source means the IP takes the system clock from a board pin, so
    // the MMCM does not make it.
    let from_board = controller_clock_name(target).is_some();
    let mut out = Vec::new();
    for (key, ident, what) in [
        ("sys_clk_mhz", "migsys", "memory controller system clock"),
        ("ref_clk_mhz", "migref", "memory controller reference clock"),
    ] {
        if from_board && key == "sys_clk_mhz" {
            continue;
        }
        if let Some(freq_mhz) = mhz(key) {
            out.push(ClockOutput {
                ports: Vec::new(),
                freq_mhz,
                domain: what.to_string(),
                ident: ident.to_string(),
            });
        }
    }
    out
}

/// Makes the clock plan.
///
/// `bindings` decides the window clock: it is the domain of the terminated
/// ports.
pub fn resolve(
    dut: &Dut,
    manifest: &Manifest,
    target: &Target,
    bindings: &[Binding],
) -> Result<ClockPlan, ClockError> {
    // Resolve the named source first. Otherwise a wrong name is reported as
    // "two sources", but the name is what needs fixing. Only a design that
    // uses `dram` gets the pin; others must not grow a memory clock pin.
    let wants_dram = manifest
        .bundle
        .values()
        .any(|bundle| bundle.backing == crate::manifest::Backing::Dram);
    let controller_clock = match controller_clock_name(target).filter(|_| wants_dram) {
        Some(name) => Some(named_clock(target, &name)?),
        None => None,
    };
    // The claim is a target fact. Remove the memory-only source from the MMCM
    // candidates even without `dram`, or such a design fails with "two sources".
    let input = input_clock(target, controller_clock_name(target).as_deref())?;
    let reset = input_reset(target)?;

    // Clock ports in declaration order, found by type, not by name.
    let clock_ports: Vec<&crate::dut::Port> = dut
        .ports
        .iter()
        .filter(|port| {
            port.signals
                .iter()
                .any(|signal| signal.role == SignalRole::Clock)
        })
        .collect();
    if clock_ports.is_empty() {
        return Err(ClockError::NoClockPort {
            module: dut.name.clone(),
        });
    }

    // 1. Each name must be an existing clock port. This catches a renamed port.
    for name in manifest.clock.keys() {
        let Some(port) = dut.ports.iter().find(|port| &port.name == name) else {
            return Err(ClockError::UnknownPort {
                port: name.clone(),
                module: dut.name.clone(),
                candidates: bullet_list(
                    &clock_ports
                        .iter()
                        .map(|port| port.name.clone())
                        .collect::<Vec<_>>(),
                ),
            });
        };
        if !clock_ports.iter().any(|clock| clock.name == port.name) {
            return Err(ClockError::NotAClock {
                port: name.clone(),
                what: describe(port),
            });
        }
    }

    // 2. Every clock port must have a frequency. There is no default.
    for port in &clock_ports {
        let Some(clock) = manifest.clock.get(&port.name) else {
            return Err(ClockError::MissingFrequency {
                port: port.name.clone(),
            });
        };
        // Written as "not greater than 0" so that NaN is rejected too. With
        // `<= 0.0`, NaN would pass and break the divider math later.
        if !matches!(
            clock.freq_mhz.partial_cmp(&0.0),
            Some(std::cmp::Ordering::Greater)
        ) {
            return Err(ClockError::InvalidFrequency {
                port: port.name.clone(),
                freq: clock.freq_mhz,
            });
        }
    }

    // 3. Group by domain. Implicit is not grouped: it means "any", not "same".
    let mut groups: BTreeMap<String, ClockOutput> = BTreeMap::new();
    for port in &clock_ports {
        let freq = manifest.clock[&port.name].freq_mhz;
        let (key, label) = match port.signals.first().map(|signal| &signal.domain) {
            Some(Domain::Explicit(name)) => (format!("domain:{name}"), format!("'{name}")),
            Some(Domain::Inferred(name)) => (format!("domain:{name}"), format!("'{name}")),
            // Implicit / None: one group per port.
            _ => (format!("port:{}", port.name), "-".to_string()),
        };

        match groups.get_mut(&key) {
            Some(output) => {
                if output.freq_mhz != freq {
                    let ports = output
                        .ports
                        .iter()
                        .map(|other| {
                            format!("    {other} = {} MHz", manifest.clock[other].freq_mhz)
                        })
                        .chain(std::iter::once(format!("    {} = {freq} MHz", port.name)))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Err(ClockError::DomainFrequencyConflict {
                        domain: label,
                        ports,
                    });
                }
                output.ports.push(port.name.clone());
            }
            None => {
                groups.insert(
                    key,
                    ClockOutput {
                        ports: vec![port.name.clone()],
                        freq_mhz: freq,
                        domain: label,
                        ident: String::new(),
                    },
                );
            }
        }
    }

    // Name each output for the RTL: the domain name, or `c<n>`.
    let mut outputs: Vec<ClockOutput> = groups.into_values().collect();
    for (index, output) in outputs.iter_mut().enumerate() {
        output.ident = match output.domain.strip_prefix('\'') {
            Some(name) => {
                // The harness's own names are reserved. The same name as `c<n>`
                // or a controller clock declares `clk_<name>` twice; the same
                // name as the board `'sys` mixes two domains.
                if is_harness_domain(name) {
                    return Err(ClockError::DomainNameTaken {
                        domain: name.to_string(),
                    });
                }
                name.to_string()
            }
            None => format!("c{index}"),
        };
    }

    // Decide which clock each DUT reset port belongs to.
    //
    // Controller clocks are not counted here. They drive no DUT port, so no
    // reset can belong to them. Counting them would break the "only one clock"
    // rule below and make simple designs ambiguous.
    let mut resets = Vec::new();
    for port in dut.ports.iter().filter(|port| {
        port.signals
            .iter()
            .any(|signal| signal.role == SignalRole::Reset)
    }) {
        let domain = port.signals.first().map(|signal| &signal.domain);
        let ident = match domain {
            Some(Domain::Explicit(name)) | Some(Domain::Inferred(name)) => outputs
                .iter()
                .find(|output| output.domain == format!("'{name}"))
                .map(|output| output.ident.clone()),
            // A reset without a domain is decided only when there is one clock.
            _ if outputs.len() == 1 => Some(outputs[0].ident.clone()),
            _ => None,
        };

        let Some(ident) = ident else {
            return Err(ClockError::ResetDomainAmbiguous {
                port: port.name.clone(),
                domain: port
                    .signals
                    .first()
                    .map(|signal| signal.domain.label())
                    .unwrap_or_else(|| "-".to_string()),
                clocks: outputs
                    .iter()
                    .map(|output| format!("    {} ({})", output.domain, output.ports.join(", ")))
                    .collect::<Vec<_>>()
                    .join("\n"),
            });
        };
        resets.push((port.name.clone(), ident));
    }

    // The window clock is chosen from the DUT clocks, so choose it before the
    // controller clocks are added. The MIG does not make `sys_clk` and `ref_clk`
    // itself (`No Buffer`), so the harness MMCM makes them. They drive no DUT
    // port, so their `ports` is empty.
    let window = window_clock(dut, bindings, &outputs)?;
    outputs.extend(controller_clocks(manifest, target));

    Ok(ClockPlan {
        controller_clock,
        input,
        reset,
        outputs,
        resets,
        window,
    })
}

/// Decides the window clock.
///
/// It is the domain of the terminated ports, because the CSR, the FIFOs and the
/// memories run on it. If those ports span several domains, it fails. If no
/// port names a domain (a single-clock DUT uses `'_`), it is decided only
/// when the DUT has one clock.
fn window_clock(
    dut: &Dut,
    bindings: &[Binding],
    outputs: &[ClockOutput],
) -> Result<String, ClockError> {
    let mut domains: Vec<&str> = Vec::new();
    for name in bindings.iter().flat_map(|binding| &binding.ports) {
        if let Some(port) = dut.ports.iter().find(|port| &port.name == name)
            && let Some(signal) = port.signals.first()
            && let Domain::Explicit(domain) | Domain::Inferred(domain) = &signal.domain
            && !domains.contains(&domain.as_str())
        {
            domains.push(domain);
        }
    }
    let clocks = || {
        outputs
            .iter()
            .map(|output| format!("    {} ({})", output.domain, output.ports.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    };
    match domains.as_slice() {
        [] => match outputs {
            [only] => Ok(only.ident.clone()),
            _ => Err(ClockError::WindowClockAmbiguous { clocks: clocks() }),
        },
        [domain] => outputs
            .iter()
            .find(|output| output.domain == format!("'{domain}"))
            .map(|output| output.ident.clone())
            .ok_or_else(|| ClockError::WindowDomainHasNoClock {
                domain: domain.to_string(),
                clocks: clocks(),
            }),
        _ => Err(ClockError::WindowDomainAmbiguous {
            domains: domains
                .iter()
                .map(|domain| format!("'{domain}"))
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

/// The target reset. Exactly one is used.
fn input_reset(target: &Target) -> Result<InputReset, ClockError> {
    let resets = target.table.get("resets").and_then(|x| x.as_table());
    let Some(resets) = resets.filter(|resets| !resets.is_empty()) else {
        return Err(ClockError::NoInputReset {
            target: target.name.clone(),
        });
    };

    if resets.len() > 1 {
        let mut names: Vec<&str> = resets.keys().map(String::as_str).collect();
        names.sort();
        return Err(ClockError::ManyInputResets {
            target: target.name.clone(),
            count: resets.len(),
            names: names.join(", "),
        });
    }

    let (name, value) = resets.iter().next().expect("checked above");
    let table = value.as_table();
    // Polarity is never inferred: a reversed reset only looks like "it does
    // not work".
    let active = table
        .and_then(|table| table.get("active"))
        .and_then(|x| x.as_str());
    let active_low = match active {
        Some("low") => true,
        Some("high") => false,
        _ => {
            return Err(ClockError::ResetPolarityUnknown {
                target: target.name.clone(),
                name: name.clone(),
            });
        }
    };

    Ok(InputReset {
        name: name.clone(),
        pin: table
            .and_then(|table| table.get("pin"))
            .and_then(|x| x.as_str())
            .map(str::to_string),
        standard: table
            .and_then(|table| table.get("standard"))
            .and_then(|x| x.as_str())
            .map(str::to_string),
        active_low,
    })
}

/// The name of the clock source the memory controller takes.
///
/// It must be named. Matching a 250 MHz controller to a 250 MHz source would be
/// a guess.
fn controller_clock_name(target: &Target) -> Option<String> {
    target
        .table
        .get("provides")
        .and_then(|x| x.as_table())
        .and_then(|provides| provides.get("dram"))
        .and_then(|x| x.as_table())
        .and_then(|dram| dram.get("sys_clk"))
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

/// The target clock source for the MMCM. Exactly one is used.
fn input_clock(target: &Target, claimed: Option<&str>) -> Result<InputClock, ClockError> {
    // A source the controller claimed is not counted. The MMCM uses what is
    // left, and that must be exactly one.
    let clocks = target.table.get("clocks").and_then(|x| x.as_table());
    let unclaimed: Vec<(&String, &toml::Value)> = clocks
        .map(|clocks| {
            clocks
                .iter()
                .filter(|(name, _)| Some(name.as_str()) != claimed)
                .collect()
        })
        .unwrap_or_default();
    if unclaimed.is_empty() {
        return Err(ClockError::NoInputClock {
            target: target.name.clone(),
        });
    }

    if unclaimed.len() > 1 {
        let mut names: Vec<&str> = unclaimed.iter().map(|(name, _)| name.as_str()).collect();
        names.sort();
        return Err(ClockError::ManyInputClocks {
            target: target.name.clone(),
            count: unclaimed.len(),
            names: names.join(", "),
        });
    }

    let (name, value) = unclaimed.into_iter().next().expect("checked above");
    let table = value.as_table();
    // `hns-targets` checks `diff` and `freq_mhz` when it loads the target.
    let diff = table
        .and_then(|table| table.get("diff"))
        .and_then(|x| x.as_bool())
        .expect("hns-targets checks `diff`");

    Ok(InputClock {
        name: name.clone(),
        freq_mhz: table
            .and_then(|table| table.get("freq_mhz"))
            .and_then(value_as_f64)
            .expect("hns-targets checks `freq_mhz`"),
        // For a differential clock, `pin_p` is the main pin.
        pin: table
            .and_then(|table| table.get(if diff { "pin_p" } else { "pin" }))
            .and_then(|x| x.as_str())
            .map(str::to_string),
        pin_n: table
            .and_then(|table| table.get("pin_n"))
            .and_then(|x| x.as_str())
            .map(str::to_string),
        standard: table
            .and_then(|table| table.get("standard"))
            .and_then(|x| x.as_str())
            .map(str::to_string),
        diff,
    })
}

/// Looks up a clock source by name.
fn named_clock(target: &Target, name: &str) -> Result<InputClock, ClockError> {
    let table = target
        .table
        .get("clocks")
        .and_then(|x| x.as_table())
        .and_then(|clocks| clocks.get(name))
        .and_then(|x| x.as_table())
        .ok_or_else(|| {
            let mut names: Vec<&str> = target
                .table
                .get("clocks")
                .and_then(|x| x.as_table())
                .map(|clocks| clocks.keys().map(String::as_str).collect())
                .unwrap_or_default();
            names.sort();
            ClockError::NoSuchClockSource {
                target: target.name.clone(),
                name: name.to_string(),
                names: names.join(", "),
            }
        })?;
    let diff = table
        .get("diff")
        .and_then(|x| x.as_bool())
        .expect("hns-targets checks `diff`");
    Ok(InputClock {
        name: name.to_string(),
        freq_mhz: table
            .get("freq_mhz")
            .and_then(value_as_f64)
            .expect("hns-targets checks `freq_mhz`"),
        pin: table
            .get(if diff { "pin_p" } else { "pin" })
            .and_then(|x| x.as_str())
            .map(str::to_string),
        pin_n: table
            .get("pin_n")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        standard: table
            .get("standard")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        diff,
    })
}

/// A number in TOML, integer or float. `200` and `200.0` are the same clock.
pub(crate) fn value_as_f64(value: &toml::Value) -> Option<f64> {
    value
        .as_float()
        .or_else(|| value.as_integer().map(|x| x as f64))
}

fn describe(port: &crate::dut::Port) -> String {
    match port.signals.first() {
        Some(signal) => format!("{} ({})", signal.type_text, port.direction),
        None => format!("a port the analyzer could not type ({})", port.direction),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dut::{Port, PortDirection, Signal};
    use crate::target;

    fn target_from(toml: &str) -> Target {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.toml");
        std::fs::write(&path, toml).unwrap();
        let target = target::load_file(&path, &[]).unwrap();
        std::mem::forget(dir);
        target
    }

    const TWO_SOURCES: &str = "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n\
        [device]\nvendor = \"xilinx\"\nfamily = \"virtexuplus\"\npart = \"xcvu9p\"\n\n\
        [clocks.sys]\nfreq_mhz = 125\ndiff = true\npin_p = \"AY24\"\npin_n = \"AY23\"\nstandard = \"LVDS\"\n\n\
        [clocks.ddr]\nfreq_mhz = 250\ndiff = true\npin_p = \"E12\"\npin_n = \"D12\"\nstandard = \"DIFF_SSTL12\"\n\n\
        [resets.sys]\npin = \"L19\"\nstandard = \"LVCMOS12\"\nactive = \"high\"\n\n\
        [provides]\ntransport = [\"jtag\"]\n";

    fn dut_with_clock() -> Dut {
        Dut {
            name: "dut_top".to_string(),
            file: std::path::PathBuf::from("dut.veryl"),
            line: 1,
            ports: vec![clock_port("i_clk", Domain::None)],
        }
    }

    /// The controller takes a named board clock source.
    ///
    /// On a board with a memory-only clock pin (VCU118), the IP takes it
    /// directly. That source is not an MMCM candidate, or the target would fail
    /// with "two sources".
    #[test]
    fn a_controller_can_take_a_board_clock_of_its_own() {
        let manifest: Manifest = toml::from_str(
            "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n\
             [bundle.mem]\nbacking = \"dram\"\n",
        )
        .unwrap();

        // Without a name, two sources are refused.
        let plain = target_from(TWO_SOURCES);
        assert!(matches!(
            resolve(&dut_with_clock(), &manifest, &plain, &[]),
            Err(ClockError::ManyInputClocks { .. })
        ));

        // With a name, the other source feeds the MMCM.
        let claimed = target_from(&format!(
            "{TWO_SOURCES}dram = {{kind = \"ddr4\", sys_clk = \"ddr\"}}\n"
        ));
        let plan = resolve(&dut_with_clock(), &manifest, &claimed, &[]).unwrap();
        assert_eq!(plan.input.name, "sys");
        let controller = plan.controller_clock.expect("the controller took `ddr`");
        assert_eq!(controller.pin.as_deref(), Some("E12"));
        assert_eq!(controller.pin_n.as_deref(), Some("D12"));
        assert!(controller.diff);
        assert_eq!(controller.freq_mhz, 250.0);

        // The MMCM does not make it: no `migsys` output.
        assert!(
            !plan.outputs.iter().any(|o| o.ident == "migsys"),
            "the board feeds it directly: {:?}",
            plan.outputs.iter().map(|o| &o.ident).collect::<Vec<_>>()
        );
    }

    /// A name that does not exist is refused. Falling back to the MMCM would
    /// pass a design whose controller clock is not on the pin.
    #[test]
    fn naming_a_clock_source_that_does_not_exist_is_refused() {
        let manifest: Manifest = toml::from_str(
            "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n\
             [bundle.mem]\nbacking = \"dram\"\n",
        )
        .unwrap();
        let target = target_from(&format!(
            "{TWO_SOURCES}dram = {{kind = \"ddr4\", sys_clk = \"nope\"}}\n"
        ));
        let err = resolve(&dut_with_clock(), &manifest, &target, &[]).unwrap_err();
        let ClockError::NoSuchClockSource { names, .. } = &err else {
            panic!("expected NoSuchClockSource, got {err:?}");
        };
        // The error lists the sources that exist.
        assert!(names.contains("ddr"), "{names}");
        assert!(names.contains("sys"), "{names}");
    }

    fn clock_port(name: &str, domain: Domain) -> Port {
        Port {
            name: name.to_string(),
            direction: PortDirection::Input,
            axi4: None,
            signals: vec![Signal {
                path: name.to_string(),
                role: SignalRole::Clock,
                type_text: "clock<1>".to_string(),
                width: Some(1),
                array: Some(1),
                domain,
            }],
        }
    }

    fn data_port(name: &str) -> Port {
        Port {
            name: name.to_string(),
            direction: PortDirection::Input,
            axi4: None,
            signals: vec![Signal {
                path: name.to_string(),
                role: SignalRole::Data,
                type_text: "logic<1>".to_string(),
                width: Some(1),
                array: Some(1),
                domain: Domain::None,
            }],
        }
    }

    fn data_port_in(name: &str, domain: &str) -> Port {
        let mut port = data_port(name);
        port.signals[0].domain = Domain::Explicit(domain.to_string());
        port
    }

    fn binding(bundle: &str, ports: &[&str]) -> Binding {
        Binding {
            bundle: bundle.to_string(),
            ports: ports.iter().map(|x| x.to_string()).collect(),
            how: crate::bundle::How::Explicit,
        }
    }

    fn dut(ports: Vec<Port>) -> Dut {
        Dut {
            name: "dut_top".to_string(),
            file: "src/dut_top.veryl".into(),
            line: 1,
            ports,
        }
    }

    fn manifest(toml: &str) -> Manifest {
        toml::from_str(toml).unwrap()
    }

    fn arty() -> Target {
        target::resolve("digilent/arty-a7-35", &[]).unwrap()
    }

    const HEAD: &str = "[dut]\nmodule = \"dut_top\"\n";

    #[test]
    fn a_single_clock_dut_takes_one_output() {
        let dut = dut(vec![
            clock_port("i_clk", Domain::Implicit),
            data_port("i_d"),
        ]);
        let manifest = manifest(&format!("{HEAD}\n[clock.i_clk]\nfreq_mhz = 200\n"));

        let plan = resolve(&dut, &manifest, &arty(), &[]).unwrap();

        // The input is the board oscillator (Arty: 100 MHz on E3).
        assert_eq!(plan.input.name, "sys");
        assert_eq!(plan.input.freq_mhz, 100.0);
        assert_eq!(plan.input.pin.as_deref(), Some("E3"));

        assert_eq!(plan.outputs.len(), 1);
        assert_eq!(plan.outputs[0].ports, ["i_clk"]);
        assert_eq!(plan.outputs[0].freq_mhz, 200.0);
    }

    /// Both `freq_mhz = 100` and `100.0` are accepted.
    #[test]
    fn an_integer_frequency_is_accepted() {
        let dut = dut(vec![clock_port("i_clk", Domain::Implicit)]);
        let manifest = manifest(&format!("{HEAD}\n[clock.i_clk]\nfreq_mhz = 166.666\n"));

        let plan = resolve(&dut, &manifest, &arty(), &[]).unwrap();
        assert!((plan.outputs[0].freq_mhz - 166.666).abs() < 1e-9);
    }

    /// Ports in one explicit domain share one output. Separate outputs would
    /// make the DUT's domain annotation false.
    #[test]
    fn ports_in_one_explicit_domain_share_an_output() {
        let dut = dut(vec![
            clock_port("is_clk", Domain::Explicit("s".to_string())),
            clock_port("is_clk2", Domain::Explicit("s".to_string())),
            clock_port("id_clk", Domain::Explicit("d".to_string())),
            data_port_in("i_d", "d"),
        ]);
        let manifest = manifest(&format!(
            "{HEAD}\n[clock.is_clk]\nfreq_mhz = 100\n\n[clock.is_clk2]\nfreq_mhz = 100\n\n[clock.id_clk]\nfreq_mhz = 50\n"
        ));

        let plan = resolve(&dut, &manifest, &arty(), &[binding("csr", &["i_d"])]).unwrap();

        assert_eq!(plan.outputs.len(), 2);
        let s = plan.outputs.iter().find(|x| x.domain == "'s").unwrap();
        assert_eq!(s.ports, ["is_clk", "is_clk2"]);
        let d = plan.outputs.iter().find(|x| x.domain == "'d").unwrap();
        assert_eq!(d.ports, ["id_clk"]);
    }

    /// Two frequencies in one domain contradict the domain annotation.
    #[test]
    fn one_domain_with_two_frequencies_is_rejected() {
        let dut = dut(vec![
            clock_port("is_clk", Domain::Explicit("s".to_string())),
            clock_port("is_clk2", Domain::Explicit("s".to_string())),
        ]);
        let manifest = manifest(&format!(
            "{HEAD}\n[clock.is_clk]\nfreq_mhz = 100\n\n[clock.is_clk2]\nfreq_mhz = 200\n"
        ));

        let err = resolve(&dut, &manifest, &arty(), &[]).unwrap_err();
        assert!(
            matches!(err, ClockError::DomainFrequencyConflict { .. }),
            "got {err:?}"
        );
    }

    /// `'_` (Implicit) means "any", not "same", so these are not grouped.
    #[test]
    fn implicit_domains_are_not_merged() {
        let dut = dut(vec![
            clock_port("i_clk_a", Domain::Implicit),
            clock_port("i_clk_b", Domain::Implicit),
        ]);
        let manifest = manifest(&format!(
            "{HEAD}\n[clock.i_clk_a]\nfreq_mhz = 100\n\n[clock.i_clk_b]\nfreq_mhz = 100\n"
        ));

        // The window clock cannot be decided. The error lists both clocks
        // separately.
        let err = resolve(&dut, &manifest, &arty(), &[]).unwrap_err();
        assert!(
            matches!(err, ClockError::WindowClockAmbiguous { .. }),
            "got {err:?}"
        );
        let help = miette::Diagnostic::help(&err).unwrap().to_string();
        assert!(help.contains("(i_clk_a)"), "{help}");
        assert!(help.contains("(i_clk_b)"), "{help}");
    }

    /// The window clock is the domain of the terminated ports, not the first
    /// domain by name. With the wrong clock, timeouts count at the wrong
    /// frequency.
    #[test]
    fn the_window_runs_on_the_terminated_ports_clock() {
        let dut = dut(vec![
            clock_port("ia_clk", Domain::Explicit("a".to_string())),
            clock_port("ib_clk", Domain::Explicit("b".to_string())),
            data_port_in("i_x", "b"),
            data_port_in("i_y", "a"),
        ]);
        let manifest = manifest(&format!(
            "{HEAD}\n[clock.ia_clk]\nfreq_mhz = 100\n\n[clock.ib_clk]\nfreq_mhz = 50\n"
        ));

        let plan = resolve(&dut, &manifest, &arty(), &[binding("csr", &["i_x"])]).unwrap();
        assert_eq!(plan.outputs[0].ident, "a");
        assert_eq!(plan.window, "b");
        assert_eq!(plan.window_output().freq_mhz, 50.0);

        // Ports in two domains: not decided.
        let err = resolve(
            &dut,
            &manifest,
            &arty(),
            &[binding("csr", &["i_x"]), binding("dbg", &["i_y"])],
        )
        .unwrap_err();
        assert!(
            matches!(err, ClockError::WindowDomainAmbiguous { .. }),
            "got {err:?}"
        );
    }

    /// A domain with a name the harness uses is refused. `'c0` clashes with the
    /// name of a clock without a domain; `'sys` clashes with the board clock.
    #[test]
    fn a_domain_named_like_a_harness_clock_is_refused() {
        for name in ["c0", "c12", "sys", "mig", "pcie", "migsys", "migref"] {
            let dut = dut(vec![
                clock_port("ia_clk", Domain::Explicit(name.to_string())),
                data_port_in("i_x", name),
            ]);
            let manifest = manifest(&format!("{HEAD}\n[clock.ia_clk]\nfreq_mhz = 100\n"));
            let err = resolve(&dut, &manifest, &arty(), &[binding("csr", &["i_x"])]).unwrap_err();
            assert!(
                matches!(&err, ClockError::DomainNameTaken { domain } if domain == name),
                "{name}: {err:?}"
            );
        }
        // `c` alone, `core` and `c0x` are allowed.
        for name in ["c", "core", "c0x"] {
            assert!(!is_harness_domain(name), "{name}");
        }
    }

    /// A DUT without a clock is refused. Otherwise the window would run on a
    /// controller clock without a word, or the plan would panic on an index.
    #[test]
    fn a_dut_without_a_clock_is_rejected() {
        let dut = dut(vec![data_port("i_d")]);
        let err = resolve(&dut, &manifest(HEAD), &arty(), &[]).unwrap_err();
        assert!(matches!(err, ClockError::NoClockPort { .. }), "got {err:?}");
    }

    /// A clock without a frequency is an error. There is no default.
    #[test]
    fn a_clock_port_without_a_frequency_is_rejected() {
        let dut = dut(vec![clock_port("i_clk", Domain::Implicit)]);
        let manifest = manifest(HEAD);

        let err = resolve(&dut, &manifest, &arty(), &[]).unwrap_err();
        assert!(
            matches!(err, ClockError::MissingFrequency { .. }),
            "got {err:?}"
        );
        // The message shows how to write the fix.
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("[clock.i_clk]"), "{rendered}");
    }

    #[test]
    fn a_frequency_on_a_non_clock_port_is_rejected() {
        let dut = dut(vec![
            clock_port("i_clk", Domain::Implicit),
            data_port("i_d"),
        ]);
        let manifest = manifest(&format!(
            "{HEAD}\n[clock.i_clk]\nfreq_mhz = 100\n\n[clock.i_d]\nfreq_mhz = 100\n"
        ));

        let err = resolve(&dut, &manifest, &arty(), &[]).unwrap_err();
        assert!(matches!(err, ClockError::NotAClock { .. }), "got {err:?}");
    }

    #[test]
    fn an_unknown_port_lists_the_clock_ports() {
        let dut = dut(vec![clock_port("i_clk", Domain::Implicit)]);
        let manifest = manifest(&format!(
            "{HEAD}\n[clock.i_clk]\nfreq_mhz = 100\n\n[clock.i_clkk]\nfreq_mhz = 100\n"
        ));

        let err = resolve(&dut, &manifest, &arty(), &[]).unwrap_err();
        let ClockError::UnknownPort { candidates, .. } = &err else {
            panic!("expected UnknownPort, got {err:?}");
        };
        assert!(candidates.contains("i_clk"), "{candidates}");
    }

    #[test]
    fn a_non_positive_frequency_is_rejected() {
        let dut = dut(vec![clock_port("i_clk", Domain::Implicit)]);
        let manifest = manifest(&format!("{HEAD}\n[clock.i_clk]\nfreq_mhz = 0\n"));

        let err = resolve(&dut, &manifest, &arty(), &[]).unwrap_err();
        assert!(
            matches!(err, ClockError::InvalidFrequency { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_target_without_a_clock_source_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-clock.toml");
        std::fs::write(
            &path,
            "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"artix7\"\npart = \"xc7a35t\"\n\n[provides]\ntransport = [\"jtag\"]\n",
        )
        .unwrap();
        let target = target::load_file(&path, &[]).unwrap();

        let dut = dut(vec![clock_port("i_clk", Domain::Implicit)]);
        let manifest = manifest(&format!("{HEAD}\n[clock.i_clk]\nfreq_mhz = 100\n"));

        let err = resolve(&dut, &manifest, &target, &[]).unwrap_err();
        assert!(
            matches!(err, ClockError::NoInputClock { .. }),
            "got {err:?}"
        );
    }
}

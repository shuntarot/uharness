//! The resolution shared by `check` and `gen`.
//!
//! `gen` must never generate what `check` rejects, so both use one entry
//! point. `check` only prints the `Plan` built here, and `gen` only writes it
//! out. No path can then pass one command and fail the other.

use std::path::{Path, PathBuf};

use veryl_metadata::Metadata;

use crate::bundle::{self, Binding, DirectionPrefixes};
use crate::clock::{self, ClockPlan};
use crate::contract::{self, BundleContract};
use crate::dut::{self, Dut};
use crate::feasibility::{self, Feasibility};
use crate::heartbeat;
use crate::manifest::{self, Loaded};
use crate::regmap::{self, RegisterMap};
use crate::target::Target;
use crate::terminator::{self, AxiMemPlan, FifoPlan, HostMemPlan, MemPlan, SlavePlan};
use crate::unconnected::{self, Unconnected};

pub struct Plan {
    pub metadata: Metadata,
    pub metadata_path: PathBuf,
    pub loaded: Loaded,

    pub dut: Dut,
    /// `$sv::` blackboxes the DUT uses (`dut::sv_blackboxes`), sorted by name.
    pub sv_blackboxes: Vec<String>,
    pub bindings: Vec<Binding>,
    pub contracts: Vec<BundleContract>,
    pub unconnected: Vec<Unconnected>,

    /// `host_poll_fifo` terminators, sorted by bundle name.
    pub fifos: Vec<FifoPlan>,

    /// `bram` / `bram_preload` terminators, sorted by bundle name.
    pub memories: Vec<MemPlan>,

    /// `host_mem` terminators. For now only where a BRAM can stand in.
    pub host_mems: Vec<HostMemPlan>,

    /// Addressable slave interfaces of the DUT, sorted by bundle name.
    pub slaves: Vec<SlavePlan>,

    /// Memories the DUT reaches as an AXI4 master, sorted by bundle name.
    pub axi_mems: Vec<AxiMemPlan>,

    pub registers: RegisterMap,

    /// Present only with `[heartbeat]` and a target.
    pub heartbeat: Option<crate::heartbeat::HeartbeatPlan>,

    /// Veryl project name of the generated harness:
    /// `<DUT project>_<out-dir base name>`.
    ///
    /// Veryl uses it as the prefix of every SV module name, so separate
    /// out-dirs can hold several harnesses for one DUT. `check` writes
    /// nothing and uses the default `hns`.
    pub harness: String,

    /// Set only when a target is given. The three parts always come together.
    pub board: Option<Board>,
}

/// The board-dependent part of the resolution.
pub struct Board {
    pub target: Target,
    /// Also holds the window clock (`ClockPlan::window`).
    pub clocks: ClockPlan,
    pub feasibility: Feasibility,
}

impl Plan {
    pub fn target(&self) -> Option<&Target> {
        self.board.as_ref().map(|board| &board.target)
    }

    pub fn clocks(&self) -> Option<&ClockPlan> {
        self.board.as_ref().map(|board| &board.clocks)
    }

    pub fn feasibility(&self) -> Option<&Feasibility> {
        self.board.as_ref().map(|board| &board.feasibility)
    }

    /// Panics without a target. `gen` always has one (`generate::run`), so
    /// the emitters it calls use this.
    pub fn board(&self) -> &Board {
        self.board.as_ref().expect("gen requires a target")
    }

    /// PCIe lanes the board offers, or 0 without PCIe.
    pub fn pcie_lanes(&self) -> u32 {
        self.target()
            .map(|t| hns_targets::pcie(t).lanes.unwrap_or(0))
            .unwrap_or(0)
    }

    /// See the free function `pcie_identity`.
    pub fn pcie_identity(&self) -> Option<crate::manifest::Pcie> {
        pcie_identity(
            &self.loaded.manifest,
            self.feasibility().map(|f| f.transport.as_str()),
        )
    }

    /// The directory that holds `Veryl.toml`.
    pub fn project_dir(&self) -> &Path {
        dir_of(&self.metadata_path)
    }
}

fn dir_of(metadata_path: &Path) -> &Path {
    metadata_path.parent().unwrap_or_else(|| Path::new("."))
}

/// Project name of the harness.
///
/// The base name alone is not enough: the default out-dir is `hns/`, which is
/// also the name of the parts package `rtl/hns`, and two projects with one
/// name cannot share a dependency graph. The DUT name prefix avoids that, and
/// the default SV names become `<dut>_hns_top`.
pub fn harness_project(dut_project: &str, out_dir_name: &str) -> String {
    format!("{dut_project}_{out_dir_name}")
}

/// For a PCIe design, the IDs the device presents and the BAR size.
///
/// Returns `None` for any other transport, so `regs.json` carries it only
/// then; otherwise the host would search for a device that is not there.
/// Without `[pcie]`, it returns the defaults that the generator gives the IP.
pub fn pcie_identity(
    manifest: &crate::manifest::Manifest,
    transport: Option<&str>,
) -> Option<crate::manifest::Pcie> {
    (transport? == "pcie").then(|| manifest.pcie.clone().unwrap_or_default())
}

/// Errors raised by `plan::build` itself.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum PlanError {
    #[error("`--transport {transport}` was given without `--target`")]
    #[diagnostic(
        code(harness::plan::transport_needs_a_target),
        help(
            "A transport is checked against the board, so it needs a target. Pass the board too:\n\n    --target <name> --transport {transport}\n\nOr remove `--transport`. `check` then reports what it could not decide without a target."
        )
    )]
    TransportNeedsATarget { transport: String },

    #[error("`{name}` cannot be a Veryl project name")]
    #[diagnostic(
        code(harness::plan::bad_out_dir),
        help(
            "The directory name becomes the Veryl project name, and Veryl puts that in front of every module name it emits. It has to start with a letter and hold only letters, digits and underscores.\n\nPick another directory:\n\n    veryl harness gen --out-dir harness_arty ..."
        )
    )]
    BadOutDir { name: String },

    /// `axi_mem` in `hns` uses `std::axi4_if` and does not analyze without std.
    /// The analyzer error would point inside the dependency, which is hard to
    /// trace back.
    #[error("the project sets `exclude_std`, which the `hns` package cannot work under")]
    #[diagnostic(
        code(harness::plan::exclude_std),
        help(
            "Remove it from Veryl.toml:\n\n    [build]\n    exclude_std = true    # <- delete this line\n\nThe `hns` package uses std (`std::axi4_if`), so it needs std."
        )
    )]
    ExcludeStd,

    #[error("transport `{transport}` has no implementation yet")]
    #[diagnostic(
        code(harness::plan::transport_unimplemented),
        help(
            "The harness reaches the host over `jtag` or `pcie`. Pick a target and transport with one of them."
        )
    )]
    TransportUnimplemented { transport: String },

    /// With half the pins, synthesis passes and the link never comes up,
    /// which is very hard to debug.
    #[error("target `{target}` says it carries pcie but does not say where its pins are")]
    #[diagnostic(
        code(harness::plan::target_has_no_pcie_pins),
        help(
            "Add a [pcie] section with `lanes`, `max_gen`, `refclk_mhz`, `refclk_p`, `reset_n`, `reset_standard`, and one `rx_p`/`tx_p` entry per lane."
        )
    )]
    TargetHasNoPciePins { target: String },

    #[error(
        "target `{target}` says its PCIe link is gen{generation} x{lanes}, which is not generated yet"
    )]
    #[diagnostic(
        code(harness::plan::pcie_link_unsupported),
        help(
            "The PCIe wrapper (`hns_pcie_wrap.v`) is written for 8 lanes and a 256-bit stream, which is gen3 x8. Other links would connect lanes or stream bits that do not exist.\n\nIf the board can run gen3 x8, state that in the target description. Otherwise use `--transport jtag`."
        )
    )]
    PcieLinkUnsupported {
        target: String,
        generation: u32,
        lanes: u32,
    },

    /// The PCIe hard block, and so the IP and the wrapper, depend on the
    /// device family.
    #[error("target `{target}` is a `{family}` device, and the generator has no PCIe for it")]
    #[diagnostic(
        code(harness::plan::pcie_family_unsupported),
        help(
            "PCIe is generated for these families: {known}.\n\nOther families have another hard block, with other ports. Use `--transport jtag`."
        )
    )]
    PcieFamilyUnsupported {
        target: String,
        family: String,
        known: String,
    },

    /// A default constraint would make timing analysis wrong when the host
    /// runs TCK faster.
    #[error("target `{target}` does not say how fast TCK may run (`[jtag] max_tck_mhz`)")]
    #[diagnostic(
        code(harness::plan::tck_rate_missing),
        help(
            "Add it to the target description:\n\n    [jtag]\n    max_tck_mhz = 30\n\nThe harness constrains the bridge's TCK to this rate."
        )
    )]
    TckRateMissing { target: String },

    #[error("[bundle.{bundle}] uses backing `{backing}`, which has no terminator yet")]
    #[diagnostic(
        code(harness::plan::no_terminator),
        help(
            "The generator can terminate `reg`, `slave`, `host_poll_fifo`, `bram`, `bram_preload`, and `dram`. `{backing}` is not supported yet.\n\n`check` without `--target` still tells you whether the manifest is feasible."
        )
    )]
    NoTerminator { bundle: String, backing: String },

    /// `latency = 0` needs a place to hold the answer until the window
    /// finishes the transaction; 2 or more needs a counter.
    #[error(
        "[bundle.{bundle}] is an addressable interface with `latency = {latency}`, which is not generated yet"
    )]
    #[diagnostic(
        code(harness::plan::slave_latency_not_one),
        help(
            "Only `latency = 1` is supported so far. State the DUT's real latency. If it is not 1, drop the `addr` role so each port becomes its own register."
        )
    )]
    SlaveLatencyNotOne { bundle: String, latency: u32 },

    /// A write to the partial last word has nowhere to go. The indirect ports
    /// collect words until the entry is complete.
    #[error(
        "[bundle.{bundle}] is a region whose entry is {entry_width} bits, which is not a whole number of words"
    )]
    #[diagnostic(
        code(harness::plan::region_entry_ragged),
        help(
            "A region maps 32-bit window words straight onto the memory, so a {entry_width}-bit entry leaves a partial last word.\n\nRound the entry to a multiple of 32 bits, or use the indirect ports:\n\n    [bundle.{bundle}]\n    access = \"indirect\""
        )
    )]
    RegionEntryRagged { bundle: String, entry_width: usize },

    /// Host memory needs a PCIe requester for the DUT, which does not exist yet.
    #[error("[bundle.{bundle}] asks for real host memory, which is not generated yet")]
    #[diagnostic(
        code(harness::plan::host_mem_not_generated),
        help(
            "Reaching host memory needs a PCIe requester for the DUT, which is not built yet. A BRAM in the FPGA takes the same ports:\n\n    [bundle.{bundle}]\n    backing = \"bram\"\n\nKeep the `rd_*` / `wr_*` roles as they are."
        )
    )]
    HostMemNotGenerated { bundle: String },
}

/// Resolves the manifest and the DUT, and runs every check.
///
/// `announce` runs before analysis, so the caller can report which manifest
/// was read even when analysis fails.
pub fn build(
    config: Option<&Path>,
    target: Option<Target>,
    transport: Option<&str>,
    out_dir_name: Option<&str>,
    announce: impl FnOnce(&Loaded, &Metadata, &Path),
) -> miette::Result<Plan> {
    // A transport means something only against the target's
    // `provides.transport`. Without a target, silently dropping `--transport`
    // would make the flag look accepted while it does nothing.
    if target.is_none()
        && let Some(transport) = transport
    {
        return Err(PlanError::TransportNeedsATarget {
            transport: transport.to_string(),
        }
        .into());
    }

    // Search upward from the current directory, as veryl does.
    let metadata_path = Metadata::search_from_current()?;
    let mut metadata = Metadata::load(&metadata_path)?;
    let base_dir = dir_of(&metadata_path).to_path_buf();

    let harness = harness_name(
        &metadata.project.name,
        out_dir_name.unwrap_or(crate::generate::OUT_DIR),
    )?;

    let loaded = manifest::load(&base_dir, config)?;
    announce(&loaded, &metadata, &metadata_path);

    let ir = dut::analyze(&mut metadata)?;
    let dut = dut::resolve(&ir, &metadata.project.name, &loaded.manifest.dut.module)?;
    let sv_blackboxes = dut::sv_blackboxes(&ir, &dut);

    // Port shapes are checked first, before any wiring is decided.
    feasibility::check_ports(&dut, &loaded.manifest)?;

    let prefixes = DirectionPrefixes::from_metadata(&metadata);
    // `[pin]` needs the target to map resource names to pins. Without one,
    // only directions are checked.
    let unconnected = unconnected::resolve(&dut, &loaded.manifest, target.as_ref())?;
    let bindings = bundle::resolve(&dut, &loaded.manifest, &prefixes, &unconnected)?;
    let contracts = contract::resolve(&dut, &loaded.manifest, &bindings, &prefixes)?;

    // Terminators come before the register map, which records FIFO depth
    // and width.
    let fifos = terminator::resolve(&dut, &loaded.manifest, &bindings, &contracts)?;
    let memories = terminator::resolve_memories(&dut, &loaded.manifest, &bindings, &contracts)?;
    let host_mems = terminator::resolve_host_mem(&dut, &loaded.manifest, &bindings, &contracts)?;
    let slaves = terminator::resolve_slaves(&dut, &loaded.manifest, &bindings, &contracts)?;
    let axi_mems =
        terminator::resolve_axi_mems(&dut, &loaded.manifest, &bindings, target.as_ref())?;
    terminator::check_claimed(
        &loaded.manifest,
        &bindings,
        &fifos,
        &memories,
        &host_mems,
        &slaves,
        &axi_mems,
    )?;

    // The clock plan and the feasibility checks need a target.
    let board = match target {
        Some(target) => Some(Board {
            clocks: clock::resolve(&dut, &loaded.manifest, &target, &bindings)?,
            feasibility: {
                let verdict = feasibility::check(
                    &loaded.manifest,
                    &contracts,
                    &memories,
                    &axi_mems,
                    &target,
                    transport,
                )?;
                feasibility::check_capacity(&target, &memories, &axi_mems, &host_mems, &fifos)?;
                verdict
            },
            target,
        }),
        None => None,
    };

    // Feasibility comes before the register map. The transport is decided
    // once there (the board picks it when `--transport` is omitted), and only
    // that resolved value is used below: it decides whether `dma_*` exists
    // and how large the BAR must be.
    let pcie = pcie_identity(
        &loaded.manifest,
        board.as_ref().map(|b| b.feasibility.transport.as_str()),
    );
    // The contracts decide which registers clear themselves.
    let registers = regmap::build(
        &dut,
        &loaded.manifest,
        &bindings,
        &contracts,
        &fifos,
        &memories,
        &host_mems,
        &slaves,
        &axi_mems,
        pcie.as_ref(),
    )?;

    // The heartbeat needs both the clocks and the register map.
    let heartbeat = match &board {
        Some(board) => heartbeat::resolve(
            &loaded.manifest,
            &board.target,
            &board.clocks,
            &registers,
            &unconnected,
        )?,
        None => None,
    };

    let plan = Plan {
        harness,
        metadata,
        metadata_path,
        loaded,
        dut,
        sv_blackboxes,
        bindings,
        contracts,
        unconnected,
        fifos,
        memories,
        host_mems,
        slaves,
        axi_mems,
        registers,
        heartbeat,
        board,
    };
    // What `gen` refuses, `check --target` refuses too. Without a target
    // there is nothing to generate, so this is skipped.
    if plan.board.is_some() {
        generatable(&plan)?;
    }
    Ok(plan)
}

/// Derives the harness project name from the out-dir name.
///
/// Refuses a name that is not a Veryl identifier. Otherwise Veryl would fail
/// to read the generated `Veryl.toml`, which is confusing.
fn harness_name(dut_project: &str, out_dir_name: &str) -> Result<String, PlanError> {
    let ok = out_dir_name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && out_dir_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(PlanError::BadOutDir {
            name: out_dir_name.to_string(),
        });
    }
    Ok(harness_project(dut_project, out_dir_name))
}

/// Whether the current generator can build this plan. Call it only with a
/// target.
///
/// Exit 0 from `check --target` means that `gen` succeeds.
fn generatable(plan: &Plan) -> Result<(), PlanError> {
    use crate::manifest::Backing;

    let board = plan.board();

    // The output uses the `hns` package, which needs std.
    if plan.metadata.build.exclude_std {
        return Err(PlanError::ExcludeStd);
    }

    // Never fall back to a JTAG harness for another transport.
    let transport = &board.feasibility.transport;
    if transport != "jtag" && transport != "pcie" {
        return Err(PlanError::TransportUnimplemented {
            transport: transport.clone(),
        });
    }
    if transport == "pcie" {
        let pcie = hns_targets::pcie(&board.target);
        if !pcie.is_complete() {
            return Err(PlanError::TargetHasNoPciePins {
                target: board.target.name.clone(),
            });
        }
        // Only links whose stream width and user clock are known. A silent
        // 128-bit / 125 MHz default would not match the real link in the IP
        // and the XDC.
        if crate::emit::pcie_block(&board.target).is_none() {
            return Err(PlanError::PcieFamilyUnsupported {
                target: board.target.name.clone(),
                family: board.target.head.device.family.clone(),
                known: crate::emit::PCIE_FAMILIES
                    .iter()
                    .map(|(family, _)| format!("`{family}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }
        let (generation, lanes) = (pcie.max_gen.unwrap_or(0), pcie.lanes.unwrap_or(0));
        // The wrapper has 8 lanes and 256-bit streams.
        let fits_wrapper = lanes == 8
            && crate::emit::pcie_stream(generation, lanes).is_some_and(|(w, _)| w == "256_bit");
        if !fits_wrapper {
            return Err(PlanError::PcieLinkUnsupported {
                target: board.target.name.clone(),
                generation,
                lanes,
            });
        }
    }
    // A PCIe build keeps JTAG too, so it also needs the TCK limit.
    if crate::target::max_tck_mhz(&board.target).is_none() {
        return Err(PlanError::TckRateMissing {
            target: board.target.name.clone(),
        });
    }

    for binding in &plan.bindings {
        let backing = plan.loaded.manifest.bundle[&binding.bundle].backing;
        if backing.is_host_mem() {
            return Err(PlanError::HostMemNotGenerated {
                bundle: binding.bundle.clone(),
            });
        }
        if !matches!(
            backing,
            Backing::Reg
                | Backing::Slave
                | Backing::HostPollFifo
                | Backing::Bram
                | Backing::BramPreload
                | Backing::Dram
        ) {
            return Err(PlanError::NoTerminator {
                bundle: binding.bundle.clone(),
                backing: backing.to_string(),
            });
        }
    }

    if let Some(slave) = plan.slaves.iter().find(|slave| slave.latency != 1) {
        return Err(PlanError::SlaveLatencyNotOne {
            bundle: slave.bundle.clone(),
            latency: slave.latency,
        });
    }

    // A region needs entries that are whole window words. With 48 bits, for
    // example, the last word sticks out of the entry, and no host strobe can
    // name those bytes.
    for region in &plan.registers.regions {
        let width = plan
            .memories
            .iter()
            .find(|mem| mem.bundle == region.bundle)
            .map(|mem| mem.entry_width)
            .unwrap_or(0);
        if !width.is_multiple_of(crate::regmap::WORD_BITS) {
            return Err(PlanError::RegionEntryRagged {
                bundle: region.bundle.clone(),
                entry_width: width,
            });
        }
    }
    Ok(())
}

//! Feasibility checks at generation time.
//!
//! A design that cannot work fails when it is generated, not after hours of
//! place and route.
//!
//! The verdict depends on contract, backing, transport and target. Examples:
//!
//! - `contract = fixed_latency` with a variable-latency backing (`host_mem`,
//!   `dram`) cannot work: there is no way to make the DUT wait.
//! - `backing = host_mem` over `transport = jtag`: there is no PCIe requester.
//! - two `dram` channels needed on a target that has one.
//!
//! ## Rules come from properties, not from a table
//!
//! A full backing x contract x transport table grows with every backing, and
//! it hides the reasons. Instead, each backing declares its properties
//! (`Capability`), and the rules use them. A new backing only fills in its
//! properties.
//!
//! ## A lossy sink must count its losses
//!
//! A contract that cannot stall the DUT (`valid_only`, `fixed_latency`) on a
//! sink backing drops beats when the sink is full. This is allowed, but the
//! terminator must expose a saturating drop counter. The promise becomes "it
//! never drops silently", which can be checked.

use miette::Diagnostic;
use thiserror::Error;

use crate::contract::BundleContract;
use crate::dut::{Dut, SignalRole};
use crate::manifest::{Backing, Contract, Manifest};
use crate::target::Target;

// ---------------------------------------------------------------------------
// Backing properties
// ---------------------------------------------------------------------------

/// The properties of one backing. The feasibility rules use these.
#[derive(Debug, Clone, Copy)]
pub struct Capability {
    /// It answers requests the DUT starts (a memory the DUT reads or writes).
    pub serves_dut_requests: bool,

    /// Its response latency is fixed. Meaningful only with
    /// `serves_dut_requests`.
    pub deterministic: bool,

    /// It receives what the DUT sends (a sink). It can overflow.
    pub sink: bool,

    /// It needs a PCIe requester.
    pub needs_pcie: bool,

    /// The target resources it needs.
    pub needs: &'static [Resource],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// On-chip BRAM.
    Bram,
    /// Real DRAM (MIG or similar).
    Dram,
}

impl Resource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Resource::Bram => "bram",
            Resource::Dram => "dram",
        }
    }

    /// Whether the target description provides it.
    fn provided_by(&self, target: &Target) -> bool {
        let Some(provides) = target.table.get("provides").and_then(|x| x.as_table()) else {
            return false;
        };
        match self {
            Resource::Bram => provides
                .get("bram_kb")
                .and_then(|x| x.as_integer())
                .is_some_and(|kb| kb > 0),
            Resource::Dram => hns_targets::dram(target)
                .and_then(|dram| dram.channels)
                .is_some_and(|channels| channels > 0),
        }
    }
}

/// The property table. A new backing only needs an entry here.
pub fn capability(backing: Backing) -> Capability {
    match backing {
        // On-chip BRAM. The DUT reads and writes it, with fixed latency.
        Backing::Bram | Backing::BramPreload => Capability {
            serves_dut_requests: true,
            deterministic: true,
            sink: false,
            needs_pcie: false,
            needs: &[Resource::Bram],
        },

        // Host memory. Hundreds of ns to µs, and variable. Needs a PCIe
        // requester.
        Backing::HostMem => Capability {
            serves_dut_requests: true,
            deterministic: false,
            sink: false,
            needs_pcie: true,
            needs: &[],
        },

        // Real DRAM. Variable, because of refresh and bank conflicts.
        Backing::Dram => Capability {
            serves_dut_requests: true,
            deterministic: false,
            sink: false,
            needs_pcie: false,
            needs: &[Resource::Dram],
        },

        // Harness registers. The host starts every access, so the DUT never
        // waits. Works over JTAG.
        Backing::Reg => Capability {
            serves_dut_requests: false,
            deterministic: true,
            sink: false,
            needs_pcie: false,
            needs: &[],
        },

        // Inside the DUT. The harness only maps it into the window; it needs
        // no resources.
        Backing::Slave => Capability {
            serves_dut_requests: false,
            deterministic: true,
            sink: false,
            needs_pcie: false,
            needs: &[],
        },

        // An on-chip FIFO that the host polls. No PCIe requester, so it works
        // over `transport = jtag`. It can overflow at its depth.
        Backing::HostPollFifo => Capability {
            serves_dut_requests: false,
            deterministic: true,
            sink: true,
            needs_pcie: false,
            needs: &[],
        },

        // Interrupts. They need an asynchronous path to the host (INTA over
        // PCIe). JTAG has none; polling would be a different backing.
        //
        // Not a `sink`: the line is a level, held until the DUT drops it, so
        // there is nothing to drop and nothing to count.
        Backing::HostIrq => Capability {
            serves_dut_requests: false,
            deterministic: true,
            sink: false,
            needs_pcie: true,
            needs: &[],
        },

        // An FLR comes from the host as a configuration write, so only over
        // PCIe. The DUT's answer is a level too: nothing to drop.
        Backing::PcieFlr => Capability {
            serves_dut_requests: false,
            deterministic: true,
            sink: false,
            needs_pcie: true,
            needs: &[],
        },

        // Observe-only output. It only holds a sample, so any transport works.
        //
        // Not a `sink`: that would require a drop counter. Whether `observe`
        // drops values depends on its implementation (a sample register or a
        // trace ring), which is not decided yet.
        Backing::Observe => Capability {
            serves_dut_requests: false,
            deterministic: true,
            sink: false,
            needs_pcie: false,
            needs: &[],
        },
    }
}

/// Whether the contract has flow control, so the harness can make the DUT
/// wait.
fn can_stall_the_dut(contract: Contract) -> bool {
    match contract {
        // AXI4 has a ready on each channel.
        Contract::ValidReady | Contract::Axi => true,
        Contract::FixedLatency | Contract::ValidOnly => false,
    }
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Feasibility {
    /// The transport used. Chosen automatically if the target has only one.
    pub transport: String,
    pub bundles: Vec<Verdict>,
}

#[derive(Debug)]
pub struct Verdict {
    pub bundle: String,
    pub backing: Backing,
    pub contract: Contract,

    /// Conditions for passing. Passing alone is no guarantee, so they are
    /// always reported.
    pub requirements: Vec<Requirement>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Requirement {
    /// A lossy terminator. It must expose a saturating drop counter in the
    /// CSR.
    DropCounter { why: String },
}

impl Requirement {
    pub fn id(&self) -> &'static str {
        match self {
            Requirement::DropCounter { .. } => "drop_counter",
        }
    }

    pub fn why(&self) -> &str {
        match self {
            Requirement::DropCounter { why } => why,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The transport when none is given. `pcie` builds both PCIe and JTAG, so
/// JTAG is always there.
pub const DEFAULT_TRANSPORT: &str = "jtag";

#[derive(Debug, Error, Diagnostic)]
pub enum FeasibilityError {
    #[error("`{target}` does not describe its memory controller fully enough to build against")]
    #[diagnostic(
        code(harness::feasibility::dram_not_described),
        help(
            "`[bundle.{bundle}]` is backed by `dram`, so the harness needs the controller's AXI4 port and clocks. `{target}` is missing: {missing}.\n\nAdd them to the target description. Read them from the generated controller IP, not from a datasheet. Until then, use `backing = \"bram\"`. The same DUT works with it."
        )
    )]
    DramNotDescribed {
        bundle: String,
        target: String,
        missing: String,
    },

    /// Without it, `ip.tcl` refers to a controller nobody creates, and only
    /// synthesis fails.
    #[error("`{target}` does not say how to generate its memory controller")]
    #[diagnostic(
        code(harness::feasibility::dram_has_no_ip_recipe),
        help(
            "`[bundle.{bundle}]` is backed by `dram`, so the build has to create the controller IP. `{target}` has neither `[vivado] mig_prj` (for 7-series MIG) nor the controller settings (for UltraScale+ DDR4).\n\nAdd one of them to the target description."
        )
    )]
    DramHasNoIpRecipe { bundle: String, target: String },

    /// `gen` copies the file into `hns/syn/`; the MIG cannot be made without it.
    #[error("`{target}` names `{name}` in `[vivado] mig_prj`, but the file is not there")]
    #[diagnostic(
        code(harness::feasibility::mig_prj_missing),
        help(
            "It is looked for beside the target description:\n\n    {looked}\n\nPut the board's mig.prj there. Xilinx's board store has one for each board with a 7-series MIG: boards/<vendor>/<board>/<revision>/mig.prj in https://github.com/Xilinx/XilinxBoardStore"
        )
    )]
    MigPrjMissing {
        target: String,
        name: String,
        looked: String,
    },

    #[error("target `{target}` does not provide transport `{requested}`")]
    #[diagnostic(
        code(harness::feasibility::transport_not_provided),
        help("`{target}` provides: {available}\n\nPick one of these with `--transport`.")
    )]
    TransportNotProvided {
        target: String,
        requested: String,
        available: String,
    },

    /// `hns::axi_dw` does the widening. The id widths must match only when no
    /// converter is inserted (equal data widths).
    #[error("[bundle.{bundle}] carries {has} {what} bits, and `{target}` gives {want}")]
    #[diagnostic(
        code(harness::feasibility::controller_width),
        help(
            "The harness widens a narrower master only from 32, 64 or 128 bits, and only by a power-of-two factor. Declare the port with one of those:\n\n    axi: modport $std::axi4_if::<$std::axi4_pkg::<..>>::master\n\nThe id width must match only when the data widths are already equal.\n\nTo keep the DUT as it is, use `backing = \"bram\"`. It takes any width."
        )
    )]
    ControllerWidth {
        bundle: String,
        what: &'static str,
        has: u32,
        want: u32,
        target: String,
    },

    #[error("[bundle.{bundle}] addresses {has} bits, and `{target}` has {want}")]
    #[diagnostic(
        code(harness::feasibility::controller_address),
        help(
            "A narrower address is fine. A wider one is not, because the extra bits would be dropped and the address would wrap.\n\nDeclare the port with {want} address bits:\n\n    axi: modport $std::axi4_if::<$std::axi4_pkg::<{want}, .., ..>>::master"
        )
    )]
    ControllerAddress {
        bundle: String,
        has: u32,
        want: u32,
        target: String,
    },

    /// The DMA engine (`hns::dma_wr`) is borrowed code, and its AXI master
    /// is fixed at 256 bits (other widths fail to build). `hns::axi_dw` only
    /// widens.
    #[error("`{target}` reaches its memory {has} bits at a time, and the requester needs 256")]
    #[diagnostic(
        code(harness::feasibility::requester_needs_wide_memory),
        help(
            "Over PCIe, the harness adds a DMA engine whose AXI master is fixed at 256 bits. It cannot be narrowed to a {has}-bit bus.\n\nUse `--transport jtag`, or a board whose memory controller is at least 256 bits wide."
        )
    )]
    RequesterNeedsWideMemory { target: String, has: u32 },

    #[error("{bundles} all want `dram`, and only one can be served so far")]
    #[diagnostic(
        code(harness::feasibility::too_many_dram),
        help(
            "Arbitration between several `dram` bundles is not built yet.\n\nKeep one `dram` bundle for now."
        )
    )]
    TooManyDram { bundles: String },

    #[error("target `{target}` provides no transport")]
    #[diagnostic(
        code(harness::feasibility::transport_missing),
        help(
            "A target description needs at least one:\n\n    [provides]\n    transport = [\"jtag\"]\n\nThe fault is in the target description, not in your project."
        )
    )]
    TransportMissing { target: String },

    #[error("target `{target}` provides more than one transport and no `jtag`")]
    #[diagnostic(
        code(harness::feasibility::transport_ambiguous),
        help("`{target}` provides: {available}\n\nPick one with `--transport`.")
    )]
    TransportAmbiguous { target: String, available: String },

    #[error("[bundle.{bundle}] cannot use backing `{backing}` over transport `{transport}`")]
    #[diagnostic(
        code(harness::feasibility::needs_pcie),
        help(
            "`{backing}` needs PCIe ({why}), and `{transport}` does not have it.\n\nPick a target and transport with PCIe, or use a backing that works over `{transport}`: {alternatives}"
        )
    )]
    NeedsPcie {
        bundle: String,
        backing: Backing,
        transport: String,
        why: &'static str,
        alternatives: String,
    },

    #[error(
        "[bundle.{bundle}] needs {needed_kb} kB of BRAM; target `{target}` has {available_kb} kB"
    )]
    #[diagnostic(
        code(harness::feasibility::not_enough_bram),
        help(
            "{depth} words of {width} bits is {needed_kb} kB, and the whole device holds {available_kb} kB. State a depth that fits:\n\n    [bundle.{bundle}]\n    depth = <words>\n\nReads beyond the depth return 0 and are counted in `{bundle}_oor`."
        )
    )]
    NotEnoughBram {
        bundle: String,
        needed_kb: u64,
        available_kb: u64,
        target: String,
        depth: u64,
        width: usize,
    },

    /// Each memory fits alone, but not all together.
    #[error(
        "the harness memories need {needed_kb} kB of BRAM together; target `{target}` has {available_kb} kB"
    )]
    #[diagnostic(
        code(harness::feasibility::not_enough_bram_together),
        help(
            "Each fits on its own, but not all at once:\n{parts}\n\nMake the largest smaller with `depth`, or move it to `dram` if the board has one."
        )
    )]
    NotEnoughBramTogether {
        needed_kb: u64,
        available_kb: u64,
        target: String,
        parts: String,
    },

    /// A wrapped address makes the host and the DUT silently read the wrong
    /// word. The harness path also uses the DUT's AXI4 package, so a port
    /// narrower than the controller limits both sides.
    #[error(
        "[bundle.{bundle}] asks for {needed_bytes} bytes of DRAM; only {reach_bytes} can be addressed"
    )]
    #[diagnostic(
        code(harness::feasibility::dram_out_of_reach),
        help(
            "{depth} entries of {entry_bytes} bytes is {needed_bytes} bytes, and {what} is {addr_bits} bits ({reach_bytes} bytes). The top of the memory would wrap onto the bottom.\n\nState a depth that fits:\n\n    [bundle.{bundle}]\n    depth = <entries>    # at most {fits} here"
        )
    )]
    DramOutOfReach {
        bundle: String,
        /// Which side limits the address. The user needs it to fix it.
        what: &'static str,
        needed_bytes: u64,
        reach_bytes: u64,
        target: String,
        depth: u64,
        entry_bytes: u64,
        addr_bits: u32,
        fits: u64,
    },

    #[error("[bundle.{bundle}] needs `{resource}`, which target `{target}` does not provide")]
    #[diagnostic(
        code(harness::feasibility::missing_resource),
        help(
            "`{backing}` is terminated by the board's {resource}. `{target}` declares:\n{provides}\n\nEither pick a target that has it, or use a backing the board can serve."
        )
    )]
    MissingResource {
        bundle: String,
        backing: Backing,
        resource: &'static str,
        target: String,
        provides: String,
    },

    #[error(
        "[bundle.{bundle}] has contract `{contract}` but backing `{backing}` has variable latency"
    )]
    #[diagnostic(
        code(harness::feasibility::variable_latency),
        help(
            "With `{contract}`, the harness cannot make the DUT wait, and `{backing}` cannot answer on a fixed schedule ({why}). Beats would be lost without the DUT knowing.\n\nUse a contract that can stall the DUT (`valid_ready`, or `modport $std::axi4_if::<..>::master`), or a backing with a fixed schedule (`bram` / `bram_preload`)."
        )
    )]
    VariableLatency {
        bundle: String,
        backing: Backing,
        contract: Contract,
        why: &'static str,
    },

    #[error("port `{port}` is an unpacked array (`{type_text}`)")]
    #[diagnostic(
        code(harness::feasibility::unpacked_array_port),
        help(
            "The harness connects a port as one flat signal. Vivado cannot bind a packed {total}-bit signal to an unpacked array (`type mismatch in port association`).\n\nFlatten it in the wrapper that [dut] points at:\n\n    {port}: {dir} logic<{total}>,\n    ...\n    var w_{port}: {element} [{array}];\n    always_comb {{\n        for i in 0..{array} {{\n            w_{port}[i] = {port}[i * {width}+:{width}];   // input\n        }}\n    }}\n\nThe harness does not guess where element 0 sits in the flat signal. If the harness should not touch this port at all:\n\n    [leave_open]\n    ports = [\"{port}\"]"
        )
    )]
    UnpackedArrayPort {
        port: String,
        type_text: String,
        element: String,
        dir: &'static str,
        width: usize,
        array: usize,
        total: usize,
    },
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// Checks only the shape of the ports. It does not depend on the target, so
/// it runs before the clock plan.
///
/// It rejects unpacked array ports (`logic<64> [8]`). The harness connects a
/// port as one flat signal. A packed signal on an unpacked array port passes
/// simulation but fails synthesis in Vivado (`type mismatch in port
/// association`, after the whole DUT is elaborated). Which end of the flat
/// signal holds element 0 is the DUT's convention, so it is not guessed.
///
/// `[leave_open]` ports are not connected, so they are skipped.
pub fn check_ports(dut: &Dut, manifest: &Manifest) -> Result<(), FeasibilityError> {
    for port in &dut.ports {
        if manifest.leave_open.ports.iter().any(|x| x == &port.name) {
            continue;
        }
        let [signal] = port.signals.as_slice() else {
            continue; // An interface has several signals; not checked here.
        };
        if matches!(signal.role, SignalRole::Clock | SignalRole::Reset) {
            continue;
        }
        let (Some(width), Some(array)) = (signal.width, signal.array) else {
            continue; // Another check reports an unresolved width.
        };
        if array <= 1 {
            continue;
        }
        return Err(FeasibilityError::UnpackedArrayPort {
            port: port.name.clone(),
            type_text: signal.type_text.clone(),
            element: format!("logic<{width}>"),
            dir: port.direction.as_str(),
            width,
            array,
            total: width * array,
        });
    }
    Ok(())
}

/// Judges feasibility. A bundle that passes can still carry conditions
/// (`Verdict.requirements`).
pub fn check(
    manifest: &Manifest,
    contracts: &[BundleContract],
    memories: &[crate::terminator::MemPlan],
    axi_mems: &[crate::terminator::AxiMemPlan],
    target: &Target,
    requested_transport: Option<&str>,
) -> Result<Feasibility, FeasibilityError> {
    let transport = select_transport(target, requested_transport)?;

    // The DUT's AXI4 must fit the controller's AXI4. A shape the harness
    // cannot convert is refused.
    for plan in axi_mems.iter().filter(|p| p.backing == Backing::Dram) {
        let dram = hns_targets::dram(target).unwrap_or_default();
        // An incomplete description is refused. Filling a missing key with a
        // default would let `check` and `gen` pass and only synthesis fail. A
        // missing key means the board's controller was not measured yet.
        // `sys_clk_mhz` is not needed when the clock comes from a board pin;
        // the frequency is then in `[clocks.<name>]`.
        let from_board = dram.sys_clk.is_some();
        let mut wanted = vec![
            ("axi_data_bits", dram.axi_data_bits.is_some()),
            ("axi_addr_bits", dram.axi_addr_bits.is_some()),
            ("axi_id_bits", dram.axi_id_bits.is_some()),
            ("ui_clk_mhz", dram.ui_clk_mhz.is_some()),
            ("width", dram.width.is_some()),
            ("row_bits", dram.row_bits.is_some()),
            ("bank_bits", dram.bank_bits.is_some()),
        ];
        if !from_board {
            wanted.push(("sys_clk_mhz", dram.sys_clk_mhz.is_some()));
        }
        // DDR4 also needs the bank group width: its pin list differs from
        // DDR3 (`emit::dram_pins`). Without `kind`, the pins cannot be chosen.
        if dram
            .kind
            .as_deref()
            .is_some_and(|kind| kind.starts_with("ddr4"))
        {
            wanted.push(("bank_group_bits", dram.bank_group_bits.is_some()));
        }
        let mut missing: Vec<&str> = wanted
            .into_iter()
            .filter(|(_, present)| !present)
            .map(|(key, _)| key)
            .collect();
        if dram.kind.is_none() {
            missing.insert(0, "kind");
        }
        // A 7-series MIG takes its reference clock from the harness MMCM too.
        if !dram.is_ddr4_ip() && !from_board && dram.ref_clk_mhz.is_none() {
            missing.push("ref_clk_mhz");
        }
        // The reset polarity differs between controllers; it is not guessed.
        if !matches!(dram.sys_rst_active.as_deref(), Some("low" | "high")) {
            missing.push("sys_rst_active (\"low\" or \"high\")");
        }
        if !missing.is_empty() {
            return Err(FeasibilityError::DramNotDescribed {
                bundle: plan.bundle.clone(),
                target: target.name.clone(),
                missing: missing.join(", "),
            });
        }
        // Generation also needs a recipe for the IP, in one of two forms: a
        // `mig.prj` (7-series), or the controller settings (UltraScale+ DDR4
        // has no prj).
        let prj = crate::emit::mig_prj_name(target);
        if let Some(name) = prj
            && crate::target::read_beside(target, name).is_none()
        {
            return Err(FeasibilityError::MigPrjMissing {
                target: target.name.clone(),
                name: name.to_string(),
                looked: target.source.beside(name),
            });
        }
        let has_prj = prj.is_some();
        // The settings are either a board-file interface, or the part and
        // both clock periods.
        let has_settings = dram.board_interface.is_some()
            || (dram.part.is_some() && dram.mem_clk_ps.is_some() && dram.sys_clk_ps.is_some());
        if !has_prj && !has_settings {
            return Err(FeasibilityError::DramHasNoIpRecipe {
                bundle: plan.bundle.clone(),
                target: target.name.clone(),
            });
        }

        // A narrower data width is fine: the harness inserts `hns::axi_dw`. It
        // takes 32 / 64 / 128, by a power-of-two factor of the controller width.
        let bits = plan.data_bytes * 8;
        if let Some(want_bits) = dram.axi_data_bits {
            let (has, want) = (bits, want_bits);
            let usable = matches!(has, 32 | 64 | 128)
                && has <= want
                && want.is_multiple_of(has)
                && (want / has).is_power_of_two();
            if !usable {
                return Err(FeasibilityError::ControllerWidth {
                    bundle: plan.bundle.clone(),
                    what: "data",
                    has,
                    want,
                    target: target.name.clone(),
                });
            }
            // The DMA engine master is fixed at 256 bits and cannot drive a
            // narrower controller. Refuse here, so no registers are made for
            // an engine that cannot be built.
            if transport == "pcie" && want < 256 {
                return Err(FeasibilityError::RequesterNeedsWideMemory {
                    target: target.name.clone(),
                    has: want,
                });
            }
            // Equal widths insert no converter, so the id goes through as is.
            if has == want
                && let Some(want_id) = dram.axi_id_bits
                && want_id != plan.id_width
            {
                return Err(FeasibilityError::ControllerWidth {
                    bundle: plan.bundle.clone(),
                    what: "id",
                    has: plan.id_width,
                    want: want_id,
                    target: target.name.clone(),
                });
            }
        }
        // A narrower address only reaches less. A wider one is refused:
        // cutting the top bits makes addresses wrap silently.
        if let Some(want) = dram.axi_addr_bits
            && plan.addr_width > want
        {
            return Err(FeasibilityError::ControllerAddress {
                bundle: plan.bundle.clone(),
                has: plan.addr_width,
                want,
                target: target.name.clone(),
            });
        }

        // Check against the controller's reach, not the memory on the board.
        // The board key `gb` is the chips fitted; the AXI address width
        // decides where addresses wrap, and it was read from the generated IP.
        // The arbiter and the host port use that width, and only the DUT port
        // is widened with `hns::axi_aw`, so a narrow DUT port does not shrink
        // the region. Beyond the reach, addresses wrap silently. Seen on the
        // board: a word written at 768 MB was read back at address 0.
        {
            let (bits, what) = match dram.axi_addr_bits {
                Some(c) => (c, "the controller's AXI4 address"),
                None => (plan.addr_width, "the DUT's own AXI4 address"),
            };
            // 64 or more address bits exceed a u64 byte count. Saturate.
            let reach = 1u64.checked_shl(bits).unwrap_or(u64::MAX);
            let entry_bytes = u64::from(plan.data_bytes);
            let needed = u64::from(plan.depth) * entry_bytes;
            if needed > reach {
                return Err(FeasibilityError::DramOutOfReach {
                    bundle: plan.bundle.clone(),
                    what,
                    needed_bytes: needed,
                    reach_bytes: reach,
                    target: target.name.clone(),
                    depth: u64::from(plan.depth),
                    entry_bytes,
                    addr_bits: bits,
                    fits: reach / entry_bytes,
                });
            }
        }
    }

    // At most one `dram` bundle for now. Sharing one channel among masters is
    // possible (the window arbitrates JTAG and PCIe with `hns::axil_rr`), but
    // that arbiter is not built yet, so it is not reported as ok.
    let drams: Vec<&str> = contracts
        .iter()
        .filter(|c| manifest.bundle[&c.bundle].backing == Backing::Dram)
        .map(|c| c.bundle.as_str())
        .collect();
    if drams.len() > 1 {
        return Err(FeasibilityError::TooManyDram {
            bundles: drams.join(", "),
        });
    }

    let mut bundles = Vec::new();
    for resolved in contracts {
        let backing = manifest.bundle[&resolved.bundle].backing;
        let capability = capability(backing);
        let contract = resolved.contract;

        // 1. A PCIe requester (e.g. host_mem over jtag). The same port on
        //    on-chip BRAM is `backing = "bram"`, which needs no PCIe.
        if capability.needs_pcie && transport != "pcie" {
            return Err(FeasibilityError::NeedsPcie {
                bundle: resolved.bundle.clone(),
                backing,
                why: why_pcie(backing),
                alternatives: alternatives_over(&transport),
                transport,
            });
        }

        // 2. Target resources.
        for resource in capability.needs {
            if !resource.provided_by(target) {
                return Err(FeasibilityError::MissingResource {
                    bundle: resolved.bundle.clone(),
                    backing,
                    resource: resource.as_str(),
                    target: target.name.clone(),
                    provides: provides_summary(target),
                });
            }
        }

        // 3. The main rule: a variable-latency terminator with a contract that
        //    cannot make the DUT wait.
        if capability.serves_dut_requests
            && !capability.deterministic
            && !can_stall_the_dut(contract)
        {
            return Err(FeasibilityError::VariableLatency {
                bundle: resolved.bundle.clone(),
                backing,
                contract,
                why: why_variable(backing),
            });
        }

        // 4. A lossy terminator is allowed, but it must count its drops.
        let mut requirements = Vec::new();
        if capability.sink && !can_stall_the_dut(contract) {
            requirements.push(Requirement::DropCounter {
                why: format!(
                    "`{contract}` cannot be back-pressured, so `{backing}` drops when it fills"
                ),
            });
        }

        // 5. Capacity. It needs the depth and word width, so the terminators
        //    must be resolved first.
        if let Some(mem) = memories.iter().find(|mem| mem.bundle == resolved.bundle) {
            let available_kb = target
                .table
                .get("provides")
                .and_then(|x| x.as_table())
                .and_then(|provides| provides.get("bram_kb"))
                .and_then(|x| x.as_integer())
                .unwrap_or(0);
            let needed_kb = mem.bytes().div_ceil(1024);
            if needed_kb > available_kb.max(0) as u64 {
                return Err(FeasibilityError::NotEnoughBram {
                    bundle: resolved.bundle.clone(),
                    needed_kb,
                    available_kb: available_kb.max(0) as u64,
                    target: target.name.clone(),
                    depth: mem.depth,
                    width: mem.width,
                });
            }
        }

        bundles.push(Verdict {
            bundle: resolved.bundle.clone(),
            backing,
            contract,
            requirements,
        });
    }

    Ok(Feasibility { transport, bundles })
}

/// Checks that all harness memories together fit in the device BRAM.
///
/// `check` looks at one bundle at a time, so two memories of 60% each would
/// pass there. This counts fixed-latency memories, AXI4 `bram`, transfer-level
/// `bram` and `host_poll_fifo`. It only adds bytes. It does not round up to
/// blocks, and it does not count small memories that go to LUTs or the DUT's
/// own memories (`check` lists that under `not_checked`).
pub fn check_capacity(
    target: &Target,
    memories: &[crate::terminator::MemPlan],
    axi_mems: &[crate::terminator::AxiMemPlan],
    host_mems: &[crate::terminator::HostMemPlan],
    fifos: &[crate::terminator::FifoPlan],
) -> Result<(), FeasibilityError> {
    let mut parts: Vec<(String, u64)> = Vec::new();
    parts.extend(memories.iter().map(|m| (m.bundle.clone(), m.bytes())));
    parts.extend(
        axi_mems
            .iter()
            .filter(|m| m.backing == Backing::Bram)
            .map(|m| {
                (
                    m.bundle.clone(),
                    u64::from(m.depth) * u64::from(m.data_bytes),
                )
            }),
    );
    parts.extend(host_mems.iter().map(|m| (m.bundle.clone(), m.bram_bytes())));
    parts.extend(fifos.iter().map(|f| {
        (
            f.bundle.clone(),
            u64::from(f.depth) * f.width.div_ceil(8) as u64,
        )
    }));
    let needed_kb = parts
        .iter()
        .map(|(_, bytes)| bytes)
        .sum::<u64>()
        .div_ceil(1024);
    let available_kb = target
        .table
        .get("provides")
        .and_then(|x| x.as_table())
        .and_then(|provides| provides.get("bram_kb"))
        .and_then(|x| x.as_integer())
        .unwrap_or(0)
        .max(0) as u64;
    if parts.len() > 1 && needed_kb > available_kb {
        parts.sort_by_key(|part| std::cmp::Reverse(part.1));
        return Err(FeasibilityError::NotEnoughBramTogether {
            needed_kb,
            available_kb,
            target: target.name.clone(),
            parts: parts
                .iter()
                .map(|(bundle, bytes)| format!("    {bundle}: {} kB", bytes.div_ceil(1024)))
                .collect::<Vec<_>>()
                .join("\n"),
        });
    }
    Ok(())
}

/// Chooses the transport.
///
/// If none is requested, it is jtag when the target has it, or else the only
/// one. With several and no jtag, the user must choose: the choice changes the
/// verdict and the register map (`dma_*`), so the first in the list is not
/// taken silently.
fn select_transport(target: &Target, requested: Option<&str>) -> Result<String, FeasibilityError> {
    let available: Vec<String> = target
        .table
        .get("provides")
        .and_then(|x| x.as_table())
        .and_then(|provides| provides.get("transport"))
        .and_then(|x| x.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if available.is_empty() {
        return Err(FeasibilityError::TransportMissing {
            target: target.name.clone(),
        });
    }

    match requested {
        Some(requested) => {
            if available.iter().any(|x| x == requested) {
                Ok(requested.to_string())
            } else {
                Err(FeasibilityError::TransportNotProvided {
                    target: target.name.clone(),
                    requested: requested.to_string(),
                    available: available.join(", "),
                })
            }
        }
        // The default is JTAG. When a board gains a second transport, existing
        // setups keep their meaning and do not start asking for one. `pcie`
        // builds both PCIe and JTAG anyway.
        None if available.iter().any(|x| x == DEFAULT_TRANSPORT) => {
            Ok(DEFAULT_TRANSPORT.to_string())
        }
        None => match available.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err(FeasibilityError::TransportAmbiguous {
                target: target.name.clone(),
                available: available.join(", "),
            }),
        },
    }
}

fn why_pcie(backing: Backing) -> &'static str {
    match backing {
        Backing::HostMem => "it fetches from host memory one request at a time over PCIe",
        Backing::HostIrq => "an interrupt has to reach the host asynchronously, as INTA over PCIe",
        Backing::PcieFlr => "a Function Level Reset is a PCIe configuration write from the host",
        _ => "it needs a PCIe path to the host",
    }
}

fn why_variable(backing: Backing) -> &'static str {
    match backing {
        Backing::HostMem => "a host-memory read takes hundreds of ns to µs and varies",
        Backing::Dram => "refresh and bank conflicts make the response time vary",
        _ => "its response time is not fixed",
    }
}

/// Lists the backings that work over the transport, so the error can say what
/// to use instead.
fn alternatives_over(transport: &str) -> String {
    let all = [
        Backing::Bram,
        Backing::BramPreload,
        Backing::HostMem,
        Backing::Dram,
        Backing::Reg,
        Backing::Slave,
        Backing::HostIrq,
        Backing::PcieFlr,
        Backing::HostPollFifo,
        Backing::Observe,
    ];
    all.iter()
        .filter(|backing| !(capability(**backing).needs_pcie && transport != "pcie"))
        .map(|backing| backing.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn provides_summary(target: &Target) -> String {
    match target.table.get("provides") {
        Some(provides) => provides
            .to_string()
            .lines()
            .map(|line| format!("    {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        None => "    (nothing)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::BundleContract;
    use crate::target;

    fn arty() -> Target {
        target::resolve("digilent/arty-a7-35", &[]).unwrap()
    }

    /// Builds a target from TOML, to test descriptions that are not shipped.
    fn target_from(toml: &str) -> Target {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.toml");
        std::fs::write(&path, toml).unwrap();
        std::fs::write(dir.path().join("mig.prj"), "<Project/>").unwrap();
        let target = target::load_file(&path, &[]).unwrap();
        std::mem::forget(dir);
        target
    }

    fn manifest(toml: &str) -> Manifest {
        toml::from_str(toml).unwrap()
    }

    fn contracts(pairs: &[(&str, Contract)]) -> Vec<BundleContract> {
        pairs
            .iter()
            .map(|(bundle, contract)| BundleContract {
                bundle: (*bundle).to_string(),
                contract: *contract,
                declared: true,
                roles: Vec::new(),
            })
            .collect()
    }

    fn dut_with(port: &str, type_text: &str, width: usize, array: usize) -> Dut {
        use crate::dut::{Domain, Port, PortDirection, Signal};
        Dut {
            name: "dut_top".to_string(),
            file: "src/dut_top.veryl".into(),
            line: 1,
            ports: vec![Port {
                name: port.to_string(),
                direction: PortDirection::Output,
                axi4: None,
                signals: vec![Signal {
                    path: port.to_string(),
                    role: SignalRole::Data,
                    type_text: type_text.to_string(),
                    width: Some(width),
                    array: Some(array),
                    domain: Domain::None,
                }],
            }],
        }
    }

    /// A real case: `logic<64> [8]` connected as one 512-bit signal. It passed
    /// simulation and failed synthesis after the whole DUT was elaborated.
    #[test]
    fn an_unpacked_array_port_is_rejected() {
        let dut = dut_with("o_dmem_wdata", "logic<64> [8]", 64, 8);
        let manifest = manifest("[dut]\nmodule = \"dut_top\"\n");

        let error = check_ports(&dut, &manifest).unwrap_err();
        let FeasibilityError::UnpackedArrayPort {
            port,
            width,
            array,
            total,
            ..
        } = &error
        else {
            panic!("wrong variant: {error:?}");
        };
        assert_eq!(port, "o_dmem_wdata");
        assert_eq!((*width, *array, *total), (64, 8, 512));

        // The help shows how to fix it.
        let help = miette::Diagnostic::help(&error).unwrap().to_string();
        assert!(help.contains("logic<512>"), "{help}");
        assert!(help.contains("leave_open"), "{help}");
    }

    /// A port that is not connected needs no flattening.
    #[test]
    fn an_unpacked_array_port_left_open_is_allowed() {
        let dut = dut_with("o_dmem_wdata", "logic<64> [8]", 64, 8);
        let manifest =
            manifest("[dut]\nmodule = \"dut_top\"\n\n[leave_open]\nports = [\"o_dmem_wdata\"]\n");
        assert!(check_ports(&dut, &manifest).is_ok());
    }

    /// A port that is not an array passes. `array = Some(1)` means "not an
    /// array".
    #[test]
    fn a_scalar_port_passes_the_shape_check() {
        let dut = dut_with("o_data", "logic<32>", 32, 1);
        let manifest = manifest("[dut]\nmodule = \"dut_top\"\n");
        assert!(check_ports(&dut, &manifest).is_ok());
    }

    /// The simplest case: Arty + jtag + reg. Arty has one transport, so it is
    /// chosen automatically.
    #[test]
    fn the_mvp_combination_is_feasible() {
        let manifest = manifest(
            "[dut]\nmodule = \"t\"\n\n[bundle.csr]\ncontract = \"valid_ready\"\nbacking = \"reg\"\n",
        );
        let result = check(
            &manifest,
            &contracts(&[("csr", Contract::ValidReady)]),
            &[],
            &[],
            &arty(),
            None,
        )
        .unwrap();

        assert_eq!(result.transport, "jtag");
        assert!(result.bundles[0].requirements.is_empty());
    }

    /// `host_mem` over `transport = jtag`.
    #[test]
    fn host_mem_over_jtag_is_rejected() {
        let manifest = manifest(
            "[dut]\nmodule = \"t\"\n\n[bundle.mem]\ncontract = \"valid_ready\"\nbacking = \"host_mem\"\n",
        );
        let err = check(
            &manifest,
            &contracts(&[("mem", Contract::ValidReady)]),
            &[],
            &[],
            &arty(),
            None,
        )
        .unwrap_err();

        let FeasibilityError::NeedsPcie { alternatives, .. } = &err else {
            panic!("expected NeedsPcie, got {err:?}");
        };
        // The error lists what can be used instead.
        assert!(alternatives.contains("bram"), "{alternatives}");
        assert!(!alternatives.contains("host_mem"), "{alternatives}");
    }

    /// `dram` on a target with an incomplete description is refused.
    ///
    /// With defaults for the missing keys, `check` and `gen` would pass and
    /// only synthesis would fail.
    #[test]
    fn dram_on_a_target_that_does_not_describe_its_controller_is_refused() {
        // A DUT with an AXI4 port, on a target without the controller keys.
        let target = target_from(
            "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"virtexuplus\"\npart = \"xcvu9p\"\n\n[provides]\ntransport = [\"jtag\"]\ndram = {kind = \"ddr4\", channels = 1, gb = 4, width = 72}\n",
        );
        let plans = vec![crate::terminator::AxiMemPlan {
            bundle: "mem".to_string(),
            backing: Backing::Dram,
            port: "axi".to_string(),
            pkg: "$std::axi4_pkg::<32, 4, 4, 1, 1, 1, 1, 1>".to_string(),
            addr_width: 32,
            data_bytes: 4,
            id_width: 4,
            depth: 256,
            depth_defaulted: false,
            aperture_bytes: Some(256),
        }];
        let err = check(
            &manifest("[dut]\nmodule = \"t\"\n\n[bundle.mem]\nbacking = \"dram\"\n"),
            &[],
            &[],
            &plans,
            &target,
            None,
        )
        .unwrap_err();
        let FeasibilityError::DramNotDescribed { missing, .. } = &err else {
            panic!("expected DramNotDescribed, got {err:?}");
        };
        // The error names each missing key. "Incomplete" alone cannot be fixed.
        assert!(missing.contains("axi_data_bits"), "{missing}");
        assert!(missing.contains("ui_clk_mhz"), "{missing}");
        // DDR4 also needs the bank group width. It must not default to 1.
        assert!(missing.contains("bank_group_bits"), "{missing}");

        // It also says what to use instead.
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("bram"), "{rendered}");
    }

    /// No PCIe on a narrow controller.
    ///
    /// PCIe adds a DMA engine (`hns::dma_wr`) whose AXI master is fixed at 256
    /// bits, and `hns::axi_dw` only widens. With only its registers in place,
    /// the host would write them and nothing would happen.
    #[test]
    fn a_requester_on_a_narrow_controller_is_refused() {
        let target = target_from(
            "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"artix7\"\npart = \"xc7a35t\"\n\n[provides]\ntransport = [\"jtag\", \"pcie\"]\ndram = {kind = \"ddr3\", channels = 1, gb = 1, width = 16, controller = \"mig_7series\", axi_data_bits = 128, axi_addr_bits = 28, axi_id_bits = 4, ui_clk_mhz = 81.25, sys_clk_mhz = 100, ref_clk_mhz = 200, row_bits = 14, bank_bits = 3, sys_rst_active = \"low\"}\n\n[vivado]\nmig_prj = \"mig.prj\"\n",
        );
        let plans = vec![crate::terminator::AxiMemPlan {
            bundle: "mem".to_string(),
            backing: Backing::Dram,
            port: "axi".to_string(),
            pkg: "$std::axi4_pkg::<28, 4, 4, 1, 1, 1, 1, 1>".to_string(),
            addr_width: 28,
            data_bytes: 4,
            id_width: 4,
            depth: 256,
            depth_defaulted: false,
            aperture_bytes: Some(256),
        }];
        let manifest = manifest("[dut]\nmodule = \"t\"\n\n[bundle.mem]\nbacking = \"dram\"\n");
        let err = check(&manifest, &[], &[], &plans, &target, Some("pcie")).unwrap_err();
        let FeasibilityError::RequesterNeedsWideMemory { has, .. } = &err else {
            panic!("expected RequesterNeedsWideMemory, got {err:?}");
        };
        assert_eq!(*has, 128);
        // The help names the way out.
        let help = miette::Diagnostic::help(&err).unwrap().to_string();
        assert!(help.contains("--transport jtag"), "{help}");

        // The same board passes over JTAG: there is no DMA engine.
        assert!(check(&manifest, &[], &[], &plans, &target, Some("jtag")).is_ok());
    }

    /// A `mig.prj` that is not beside the description is refused before
    /// anything is written.
    #[test]
    fn a_missing_mig_prj_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.toml");
        std::fs::write(
            &path,
            "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"artix7\"\npart = \"xc7a35t\"\n\n[provides]\ntransport = [\"jtag\"]\ndram = {kind = \"ddr3\", channels = 1, gb = 1, width = 16, controller = \"mig_7series\", axi_data_bits = 128, axi_addr_bits = 28, axi_id_bits = 4, ui_clk_mhz = 81.25, sys_clk_mhz = 100, ref_clk_mhz = 200, row_bits = 14, bank_bits = 3, sys_rst_active = \"low\"}\n\n[vivado]\nmig_prj = \"mig.prj\"\n",
        )
        .unwrap();
        let target = target::load_file(&path, &[]).unwrap();
        let plans = vec![crate::terminator::AxiMemPlan {
            bundle: "mem".to_string(),
            backing: Backing::Dram,
            port: "axi".to_string(),
            pkg: "$std::axi4_pkg::<28, 4, 4, 1, 1, 1, 1, 1>".to_string(),
            addr_width: 28,
            data_bytes: 16,
            id_width: 4,
            depth: 256,
            depth_defaulted: false,
            aperture_bytes: Some(256),
        }];
        let manifest = manifest("[dut]\nmodule = \"t\"\n\n[bundle.mem]\nbacking = \"dram\"\n");
        let err = check(&manifest, &[], &[], &plans, &target, Some("jtag")).unwrap_err();
        let FeasibilityError::MigPrjMissing { looked, .. } = &err else {
            panic!("expected MigPrjMissing, got {err:?}");
        };
        assert!(looked.ends_with("mig.prj"), "{looked}");

        // With the file in place, the same target passes.
        std::fs::write(dir.path().join("mig.prj"), "<Project/>").unwrap();
        assert!(check(&manifest, &[], &[], &plans, &target, Some("jtag")).is_ok());
    }

    /// A region beyond the controller's reach is refused.
    ///
    /// A silent wrap is the worst case. The host and the DUT read another word
    /// than the one they wrote, and it looks like broken memory.
    #[test]
    fn a_dram_region_beyond_the_controllers_reach_is_refused() {
        // Measured on VCU118: the AXI4 address is 31 bits = 2 GB.
        let target = target_from(
            "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"virtexuplus\"\npart = \"xcvu9p\"\n\n[provides]\ntransport = [\"jtag\"]\ndram = {kind = \"ddr4\", channels = 1, gb = 4, width = 64, controller = \"ddr4\", board_interface = \"ddr4_sdram\", axi_data_bits = 512, axi_addr_bits = 31, axi_id_bits = 8, ui_clk_mhz = 333.333, sys_clk_mhz = 250, row_bits = 17, bank_bits = 2, bank_group_bits = 1, sys_rst_active = \"high\"}\n",
        );
        let plan = |depth: u32| crate::terminator::AxiMemPlan {
            bundle: "mem".to_string(),
            backing: Backing::Dram,
            port: "axi".to_string(),
            pkg: "$std::axi4_pkg::<28, 4, 8, 1, 1, 1, 1, 1>".to_string(),
            addr_width: 28,
            data_bytes: 4,
            id_width: 8,
            depth,
            depth_defaulted: false,
            aperture_bytes: Some(256),
        };
        let m = manifest("[dut]\nmodule = \"t\"\n\n[bundle.mem]\nbacking = \"dram\"\n");

        // The controller sets the limit (31 bits = 2 GB). A 28-bit DUT port
        // does not shrink the region: the arbiter and the host port use the
        // controller width, and only the DUT port is widened with
        // `hns::axi_aw`, so the host reaches all of the memory. Exactly 2 GB
        // passes: walking all of it is the only way to toggle the top row and
        // bank pins.
        check(&m, &[], &[], &[plan(512 * 1024 * 1024)], &target, None).unwrap();

        // One word more is refused. It would wrap silently.
        let err = check(&m, &[], &[], &[plan(512 * 1024 * 1024 + 1)], &target, None).unwrap_err();
        let FeasibilityError::DramOutOfReach {
            fits,
            addr_bits,
            what,
            ..
        } = &err
        else {
            panic!("expected DramOutOfReach, got {err:?}");
        };
        assert_eq!(*addr_bits, 31);
        assert_eq!(*fits, 512 * 1024 * 1024);
        // The error names what limits the address.
        assert!(what.contains("controller"), "{what}");

        // It also gives the largest value that fits.
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("536870912"), "{rendered}");
    }

    /// The main rule: a variable-latency terminator with a contract that cannot
    /// make the DUT wait.
    #[test]
    fn fixed_latency_with_a_variable_backing_is_rejected() {
        let manifest = manifest(
            "[dut]\nmodule = \"t\"\n\n[bundle.mem]\ncontract = \"fixed_latency\"\nlatency = 2\nbacking = \"dram\"\n",
        );
        let err = check(
            &manifest,
            &contracts(&[("mem", Contract::FixedLatency)]),
            &[],
            &[],
            &arty(),
            None,
        )
        .unwrap_err();

        let FeasibilityError::VariableLatency { why, .. } = &err else {
            panic!("expected VariableLatency, got {err:?}");
        };
        assert!(why.contains("refresh"), "why was: {why}");

        // The message gives both fixes: change the contract or the backing.
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("valid_ready"), "{rendered}");
        assert!(rendered.contains("bram"), "{rendered}");

        // It must not suggest removed contract names. Following that advice
        // would fail in the `Contract` deserializer.
        for gone in ["req_ack", "tagged"] {
            assert!(!rendered.contains(gone), "{rendered}");
        }
    }

    /// A difference the converter can fill passes; others are refused.
    ///
    /// `hns::axi_dw` widens a 32/64/128-bit master. A wider address is
    /// different: cutting it makes addresses wrap silently.
    #[test]
    fn a_narrower_axi4_master_is_widened_but_a_wider_address_is_refused() {
        use crate::terminator::AxiMemPlan;
        let manifest = manifest(
            "[dut]\nmodule = \"t\"\n\n[bundle.mem]\ncontract = \"valid_ready\"\nbacking = \"dram\"\n",
        );
        let plan = |data_bytes: u32, addr_width: u32| AxiMemPlan {
            bundle: "mem".to_string(),
            backing: Backing::Dram,
            port: "axi".to_string(),
            pkg: "-".to_string(),
            addr_width,
            data_bytes,
            id_width: 4,
            depth: 1024,
            depth_defaulted: true,
            aperture_bytes: None,
        };
        let run = |p: AxiMemPlan| {
            check(
                &manifest,
                &contracts(&[("mem", Contract::ValidReady)]),
                &[],
                &[p],
                &arty(),
                None,
            )
        };

        // 32 bits with a 28-bit address passes; the converter widens it.
        assert!(run(plan(4, 28)).is_ok());
        // 64 bits passes too (128 / 64 = 2).
        assert!(run(plan(8, 28)).is_ok());

        // A wider address is refused.
        let err = run(plan(4, 32)).unwrap_err();
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("32 bits"), "{rendered}");
        assert!(rendered.contains("28"), "{rendered}");
        // The message says what would happen.
        assert!(rendered.contains("would wrap"), "{rendered}");

        // A ratio that is not a power of two is refused (96 bits).
        let err = run(plan(12, 28)).unwrap_err();
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("96 data bits"), "{rendered}");
        assert!(rendered.contains("bram"), "{rendered}");
    }

    /// `dram` passes with a contract that can make the DUT wait.
    #[test]
    fn dram_with_valid_ready_is_feasible() {
        let manifest = manifest(
            "[dut]\nmodule = \"t\"\n\n[bundle.mem]\ncontract = \"valid_ready\"\nbacking = \"dram\"\n",
        );
        let result = check(
            &manifest,
            &contracts(&[("mem", Contract::ValidReady)]),
            &[],
            &[],
            &arty(),
            None,
        )
        .unwrap();

        assert!(result.bundles[0].requirements.is_empty());
    }

    /// A resource the target does not have is refused.
    #[test]
    fn a_backing_the_board_cannot_serve_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-dram.toml");
        std::fs::write(
            &path,
            "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"artix7\"\npart = \"xc7a35t\"\n\n[provides]\ntransport = [\"jtag\"]\nbram_kb = 100\n",
        )
        .unwrap();
        let target = target::load_file(&path, &[]).unwrap();

        let manifest = manifest(
            "[dut]\nmodule = \"t\"\n\n[bundle.mem]\ncontract = \"valid_ready\"\nbacking = \"dram\"\n",
        );
        let err = check(
            &manifest,
            &contracts(&[("mem", Contract::ValidReady)]),
            &[],
            &[],
            &target,
            None,
        )
        .unwrap_err();

        assert!(
            matches!(err, FeasibilityError::MissingResource { .. }),
            "got {err:?}"
        );
    }

    /// A lossy terminator is allowed, but it must have a drop counter.
    #[test]
    fn a_lossy_sink_is_allowed_but_must_expose_a_drop_counter() {
        let manifest = manifest(
            "[dut]\nmodule = \"t\"\n\n[bundle.tx]\ncontract = \"valid_only\"\nbacking = \"host_poll_fifo\"\n",
        );
        let result = check(
            &manifest,
            &contracts(&[("tx", Contract::ValidOnly)]),
            &[],
            &[],
            &arty(),
            None,
        )
        .unwrap();

        assert_eq!(result.bundles[0].requirements.len(), 1);
        assert_eq!(result.bundles[0].requirements[0].id(), "drop_counter");
    }

    /// With back pressure, there is no condition.
    #[test]
    fn a_sink_with_backpressure_needs_no_drop_counter() {
        let manifest = manifest(
            "[dut]\nmodule = \"t\"\n\n[bundle.tx]\ncontract = \"valid_ready\"\nbacking = \"host_poll_fifo\"\n",
        );
        let result = check(
            &manifest,
            &contracts(&[("tx", Contract::ValidReady)]),
            &[],
            &[],
            &arty(),
            None,
        )
        .unwrap();

        assert!(result.bundles[0].requirements.is_empty());
    }

    #[test]
    fn an_unavailable_transport_lists_what_the_target_has() {
        let manifest = manifest("[dut]\nmodule = \"t\"\n");
        let err = check(&manifest, &[], &[], &[], &arty(), Some("pcie")).unwrap_err();

        let FeasibilityError::TransportNotProvided { available, .. } = &err else {
            panic!("expected TransportNotProvided, got {err:?}");
        };
        assert_eq!(available, "jtag");
    }

    /// With several transports, jtag is chosen if it is one of them.
    #[test]
    fn more_than_one_transport_defaults_to_jtag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("two.toml");
        std::fs::write(
            &path,
            "[board]\nprovider = \"acme\"\nname = \"proto\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"artix7\"\npart = \"xc7a35t\"\n\n[provides]\ntransport = [\"jtag\", \"pcie\"]\n",
        )
        .unwrap();
        let target = target::load_file(&path, &[]).unwrap();

        let manifest = manifest("[dut]\nmodule = \"t\"\n");
        // Two transports do not stop it. The default is JTAG, and `pcie`
        // builds both PCIe and JTAG.
        let result = check(&manifest, &[], &[], &[], &target, None).unwrap();
        assert_eq!(result.transport, "jtag");

        // A requested transport wins.
        let result = check(&manifest, &[], &[], &[], &target, Some("pcie")).unwrap();
        assert_eq!(result.transport, "pcie");
    }

    /// Without jtag, a single transport is used, and several must be chosen.
    /// Taking the first would make the design depend on the order in the file.
    #[test]
    fn without_jtag_only_a_single_transport_is_picked() {
        let dir = tempfile::tempdir().unwrap();
        let load = |name: &str, transports: &str| {
            let path = dir.path().join(format!("{name}.toml"));
            std::fs::write(
                &path,
                format!("[board]\nprovider = \"acme\"\nname = \"{name}\"\n\n[device]\nvendor = \"xilinx\"\nfamily = \"artix7\"\npart = \"xc7a35t\"\n\n[provides]\ntransport = [{transports}]\n"),
            )
            .unwrap();
            target::load_file(&path, &[]).unwrap()
        };
        let manifest = manifest("[dut]\nmodule = \"t\"\n");

        let one = load("one", "\"pcie\"");
        let result = check(&manifest, &[], &[], &[], &one, None).unwrap();
        assert_eq!(result.transport, "pcie");

        let two = load("two", "\"pcie\", \"eth\"");
        let err = check(&manifest, &[], &[], &[], &two, None).unwrap_err();
        assert!(
            matches!(err, FeasibilityError::TransportAmbiguous { .. }),
            "{err}"
        );
        let help = miette::Diagnostic::help(&err).unwrap().to_string();
        assert!(help.contains("pcie, eth"), "{help}");
        assert!(help.contains("--transport"), "{help}");
    }
    /// Memories that fit alone but not together are refused. Arty has 225 kB;
    /// two `bram` of 140 kB each pass one by one.
    #[test]
    fn memories_that_fit_alone_but_not_together_are_refused() {
        let mem = |bundle: &str| crate::terminator::AxiMemPlan {
            bundle: bundle.to_string(),
            backing: Backing::Bram,
            port: "axi".to_string(),
            pkg: "$std::axi4_pkg::<32, 4, 4, 1, 1, 1, 1, 1>".to_string(),
            addr_width: 32,
            data_bytes: 4,
            id_width: 4,
            depth: 35 * 1024,
            depth_defaulted: false,
            aperture_bytes: None,
        };
        let one = [mem("a")];
        assert!(check_capacity(&arty(), &[], &one, &[], &[]).is_ok());

        let two = [mem("a"), mem("b")];
        let err = check_capacity(&arty(), &[], &two, &[], &[]).unwrap_err();
        assert!(
            matches!(
                err,
                FeasibilityError::NotEnoughBramTogether {
                    needed_kb: 280,
                    available_kb: 225,
                    ..
                }
            ),
            "{err:?}"
        );
        let help = miette::Diagnostic::help(&err).unwrap().to_string();
        assert!(
            help.contains("a: 140 kB") && help.contains("b: 140 kB"),
            "{help}"
        );

        // `dram` uses no on-chip BRAM.
        let dram = [
            mem("a"),
            crate::terminator::AxiMemPlan {
                backing: Backing::Dram,
                ..mem("b")
            },
        ];
        assert!(check_capacity(&arty(), &[], &dram, &[], &[]).is_ok());
    }
}

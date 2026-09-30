//! The `check` subcommand: compares the manifest with the DUT source and
//! generates nothing.
//!
//! It is the step a user or an agent repeats: write TOML, check, read the
//! error, fix, run again. Not everything is checked yet, so every run lists
//! what was not checked. Exit 0 must not be read as "the harness can be
//! built".

use std::collections::HashMap;
use std::path::Path;

use crate::bundle::Binding;
use crate::clock::{ClockOutput, ClockPlan};
use crate::contract::BundleContract;
use crate::dut::{Dut, SignalRole};
use crate::feasibility::Feasibility;
use crate::json::{self, Item};
use crate::manifest;
use crate::plan;
use crate::regmap::RegisterMap;
use crate::target::Target;
use crate::unconnected::Unconnected;

/// Output format. JSON is for agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Human,
    Json,
}

/// `config` is `--config <path>`. With `None`, the manifest is searched for.
pub fn run(
    config: Option<&Path>,
    target: Option<Target>,
    transport: Option<&str>,
    emit_regs: Option<&Path>,
    format: Format,
    verbose: bool,
) -> miette::Result<()> {
    let plan = plan::build(
        config,
        target,
        transport,
        None,
        |loaded, metadata, metadata_path| {
            // Printed before analysis, so that the manifest is known even
            // when analysis fails. JSON is one document, so nothing here.
            if format == Format::Human && verbose {
                println!("manifest: {}", loaded.path.display());
                println!(
                    "project:  {} ({})",
                    metadata.project.name,
                    metadata_path.display()
                );
                println!();
            } else if format == Format::Human {
                println!(
                    "ok  {:<8} {} (project {})",
                    "manifest",
                    shown(&loaded.path),
                    metadata.project.name
                );
            }
        },
    )?;

    if let Some(path) = emit_regs {
        write_regs(
            path,
            &plan.registers,
            plan.target(),
            plan.clocks(),
            plan.pcie_identity().as_ref(),
        )?;
    }

    match format {
        Format::Human if !verbose => print_summary(&plan, emit_regs),
        Format::Human => {
            print_dut(
                &plan.dut,
                &plan.bindings,
                &plan.contracts,
                &plan.unconnected,
            );
            print_bundles(&plan);
            print_target(plan.target());
            print_feasibility(plan.feasibility());
            print_clocks(plan.clocks());
            print_heartbeat(plan.heartbeat.as_ref());
            print_registers(&plan.registers, emit_regs);
            print_not_checked_yet(plan.board.is_some(), &plan.sv_blackboxes);
        }
        Format::Json => {
            let project = json::Project {
                name: plan.metadata.project.name.clone(),
                manifest: plan.metadata_path.display().to_string(),
            };
            let output = json::ok(
                project,
                &plan,
                checked_items(plan.board.is_some()),
                not_checked_items(plan.board.is_some(), &plan.sv_blackboxes),
            );
            println!("{}", json::render(&output));
        }
    }

    Ok(())
}

/// What was checked. The human report and JSON share this list.
///
/// Only checks that actually ran are listed: a run without a target does not
/// claim target resolution.
pub(crate) fn checked_items(with_target: bool) -> Vec<Item> {
    let mut items = checked_always();
    if with_target {
        items.push(Item::new(
            "target_resolution",
            "the named target exists, its description parses, and any patches applied cleanly",
        ));
        items.push(Item::new(
            "feasibility",
            "each bundle's contract x backing works over the chosen transport, and the target provides what the backing needs",
        ));
        items.push(Item::new(
            "clock_plan",
            "every clock port has a frequency, and ports sharing a clock domain share one MMCM output",
        ));
        items.push(Item::new(
            "generatable",
            "gen can build this: every backing has a terminator, the transport is built, and the target states its PCIe pins and TCK rate",
        ));
    }
    items
}

fn checked_always() -> Vec<Item> {
    vec![
        Item::new(
            "manifest_schema",
            "Harness.toml parses and its keys are consistent",
        ),
        Item::new(
            "dut_resolution",
            "[dut] module exists in this project and has no parameters or generics",
        ),
        Item::new(
            "bundle_ports",
            "every bundle matches at least one port, and every port belongs to exactly one bundle",
        ),
        Item::new(
            "contract_vs_ports",
            "each bundle's contract follows from its ports, and a contract written in Harness.toml matches them",
        ),
        Item::new(
            "register_map",
            "every reg port has a register, and the map carries a hash the host can check against the bitstream",
        ),
        Item::new(
            "unconnected_ports",
            "[tie_off] drives only inputs, with values that fit the port width, and [leave_open] applies only to outputs",
        ),
        Item::new(
            "memory_terminator",
            "every bram / bram_preload bundle has addr and rdata ports in the right direction, a power-of-two depth the address can reach, and a word that fits the host register, and it fits the target's BRAM",
        ),
        Item::new(
            "host_poll_fifo_terminator",
            "every host_poll_fifo bundle is a stream from the DUT with a power-of-two depth, and counts drops when it cannot stall the DUT",
        ),
        Item::new(
            "host_mem_port",
            "every bundle with transfer-level roles names the whole read port (rd_cmd_* and rd_*)",
        ),
    ]
}

/// What was not checked, so that exit 0 is not read as "the harness can be
/// built". JSON always carries it too, because agents need it most.
pub(crate) fn not_checked_items(with_target: bool, sv_blackboxes: &[String]) -> Vec<Item> {
    let mut items = vec![
        Item::new(
            "latency_vs_rtl",
            "whether a fixed_latency bundle really has the stated latency, which is written by hand",
        ),
        Item::new(
            "clock_feasibility",
            "whether the MMCM can produce the requested frequencies, which Vivado decides when it generates the IP",
        ),
        Item::new(
            "reset_and_cdc",
            "reset release from the MMCM lock, and the CDC the JTAG transport introduces",
        ),
        Item::new(
            "capacity",
            "whether the whole design fits the board. The harness's memories are summed in bytes against the device's BRAM; block rounding, LUTs and the DUT's own memories are not counted",
        ),
        Item::new(
            "latency_the_dut_expects",
            "whether the DUT really wants the latency a memory bundle states. The harness builds the memory to that number",
        ),
        Item::new(
            "observe_loss",
            "whether an observe bundle loses samples, which depends on an open design question",
        ),
    ];
    if !with_target {
        items.push(Item::new(
            "feasibility",
            "contract x backing x transport x target feasibility (no --target was given)",
        ));
        items.push(Item::new(
            "board_pins",
            "whether the resources [pin] and [heartbeat] name exist on the board and may be driven, and the heartbeat's baud rate (no --target was given)",
        ));
    }
    // `$sv::` sources are not read. Whether they arrive depends on the DUT's
    // `include(inline, ...)`, and a missing one shows up only in synthesis.
    if !sv_blackboxes.is_empty() {
        items.push(Item::owned(
            "sv_blackbox_sources",
            format!(
                "whether the Verilog behind {} reaches synthesis. Bring it in with include(inline, \"<file>\") in the DUT project",
                sv_blackboxes
                    .iter()
                    .map(|name| format!("$sv::{name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    items
}

fn print_dut(
    dut: &Dut,
    bindings: &[Binding],
    contracts: &[BundleContract],
    unconnected: &[Unconnected],
) {
    println!("dut: {} ({}:{})", dut.name, dut.file.display(), dut.line);
    println!();

    // Port name -> bundle name. Name-based matching is allowed only because
    // this table is always shown: a rule that matched the wrong port is
    // visible here.
    let owner: HashMap<&str, &str> = bindings
        .iter()
        .flat_map(|binding| {
            binding
                .ports
                .iter()
                .map(|port| (port.as_str(), binding.bundle.as_str()))
        })
        .collect();

    // Port name -> role. Roles are inferred from a dictionary, so they are
    // always shown; a wrong guess must be visible.
    let roles: HashMap<&str, String> = contracts
        .iter()
        .flat_map(|contract| {
            contract.roles.iter().map(|assignment| {
                let invert = if assignment.invert { "!" } else { "" };
                (
                    assignment.port.as_str(),
                    format!("{invert}{}", assignment.role),
                )
            })
        })
        .collect();

    let name_width = width_of(dut.ports.iter().map(|port| port.name.as_str()), 4);
    let type_width = width_of(
        dut.ports
            .iter()
            .flat_map(|port| port.signals.iter())
            .map(|signal| signal.type_text.as_str()),
        4,
    );

    println!(
        "  {:<name_width$}  {:<6}  {:<type_width$}  {:>5}  {:>5}  {:<16}  {:<8}  role",
        "port", "dir", "type", "width", "array", "clock domain", "bundle"
    );

    let mut unresolved = 0;
    for port in &dut.ports {
        let bundle = match owner.get(port.name.as_str()) {
            Some(bundle) => (*bundle).to_string(),
            None => match unconnected.iter().find(|entry| entry.port == port.name) {
                // [tie_off] / [leave_open] are always shown.
                Some(entry) => entry.kind.label(),
                None => match role_of(port) {
                    // Clocks and resets never belong to a bundle.
                    Some(SignalRole::Clock) => "(clock)".to_string(),
                    Some(SignalRole::Reset) => "(reset)".to_string(),
                    _ => "-".to_string(),
                },
            },
        };

        if port.signals.is_empty() {
            // The IR has no type. Show that; do not fill in a default.
            println!(
                "  {:<name_width$}  {:<6}  {:<type_width$}  {:>5}  {:>5}  {:<16}  {bundle:<8}  {}",
                port.name,
                port.direction.as_str(),
                "?",
                "?",
                "?",
                "?",
                role_cell(&roles, &port.name),
            );
            unresolved += 1;
            continue;
        }

        for (index, signal) in port.signals.iter().enumerate() {
            // An expanded port indents its later signals under the
            // declaration.
            let label = if index == 0 {
                port.name.clone()
            } else {
                signal.path.clone()
            };
            let direction = if index == 0 {
                port.direction.as_str()
            } else {
                ""
            };
            let bundle = if index == 0 { bundle.as_str() } else { "" };

            if signal.width.is_none() || signal.array.is_none() {
                unresolved += 1;
            }

            let role = if index == 0 {
                role_cell(&roles, &port.name)
            } else {
                String::new()
            };
            // Show AXI4 decoded. The IR spells the type as
            // `modport axi4_if::<$std __axi4_pkg__32__4__4__1__1__1__1__1>::master`,
            // where neither widths nor direction are readable.
            let type_text = match &port.axi4 {
                Some(axi4) if index == 0 => format!(
                    "axi4 {} addr={} data={} id={}",
                    axi4.modport,
                    axi4.addr_width(),
                    axi4.data_width(),
                    axi4.id_width()
                ),
                _ => signal.type_text.clone(),
            };
            println!(
                "  {:<name_width$}  {:<6}  {:<type_width$}  {:>5}  {:>5}  {:<16}  {bundle:<8}  {role}",
                label,
                direction,
                type_text,
                opt(signal.width),
                opt(signal.array),
                signal.domain.label(),
            );
        }
    }
    println!();

    let clocks = count_role(dut, SignalRole::Clock);
    let resets = count_role(dut, SignalRole::Reset);
    let total = dut.ports.len();
    println!(
        "{total} port{}: {clocks} clock, {resets} reset, {} data",
        plural(total),
        total.saturating_sub(clocks + resets),
    );

    if unresolved > 0 {
        // What cannot be inferred must be written by a person. These will
        // need a TOML key, which does not exist yet.
        println!(
            "{unresolved} signal{} with a width or array size the analyzer cannot resolve. \
             Stating those in {} is not supported yet",
            plural(unresolved),
            manifest::MANIFEST_NAME
        );
    }
    println!();
}

fn print_bundles(plan: &crate::plan::Plan) {
    let (loaded, bindings, contracts) = (&plan.loaded, &plan.bindings, &plan.contracts);
    let (fifos, memories) = (&plan.fifos, &plan.memories);
    if bindings.is_empty() {
        println!("no bundles declared");
        println!();
        return;
    }

    println!("bundles declared in {}:", loaded.path.display());

    let name_width = width_of(bindings.iter().map(|binding| binding.bundle.as_str()), 6);
    for binding in bindings {
        let bundle = &loaded.manifest.bundle[&binding.bundle];
        let resolved = contracts
            .iter()
            .find(|contract| contract.bundle == binding.bundle);
        let contract = match resolved {
            // Say when the contract was inferred rather than written.
            Some(resolved) if resolved.declared => resolved.contract.to_string(),
            Some(resolved) => format!("{} (inferred)", resolved.contract),
            None => "?".to_string(),
        };
        let latency = match bundle.latency {
            Some(latency) => format!(" latency={latency}"),
            None => String::new(),
        };
        let count = binding.ports.len();

        // host_poll_fifo depth. Say when it is a default, as for contracts.
        let depth = match fifos.iter().find(|fifo| fifo.bundle == binding.bundle) {
            Some(fifo) if fifo.depth_defaulted => format!(" depth={} (defaulted)", fifo.depth),
            Some(fifo) => format!(" depth={}", fifo.depth),
            None => String::new(),
        };
        let drops = match fifos.iter().find(|fifo| fifo.bundle == binding.bundle) {
            Some(fifo) if fifo.counts_drops() => " drop_counter",
            _ => "",
        };

        // Memory depth, and whether it came from the address width.
        let memory = match memories.iter().find(|mem| mem.bundle == binding.bundle) {
            Some(mem) => {
                let source = if mem.depth_from_addr {
                    " (from addr width)"
                } else {
                    ""
                };
                let oor = if mem.counts_out_of_range() {
                    " oor_counter"
                } else {
                    ""
                };
                // Show both widths when they differ (line write, word read).
                let width = if mem.entry_width == mem.width {
                    format!("width={}", mem.width)
                } else {
                    format!("entry={} word={}", mem.entry_width, mem.width)
                };
                format!(" depth={}{source} {width}{oor}", mem.depth)
            }
            None => String::new(),
        };

        // The other terminators. Defaults are marked, as for FIFOs and
        // memories.
        let defaulted = |declared: bool| if declared { "" } else { " (defaulted)" };
        let other = if let Some(mem) = plan.host_mems.iter().find(|m| m.bundle == binding.bundle) {
            let ports = match (&mem.read, &mem.write) {
                (Some(_), Some(_)) => "read+write",
                (Some(_), None) => "read",
                (None, Some(_)) => "write",
                (None, None) => "-",
            };
            format!(
                " depth={}{} width={} transfer-level {ports}",
                mem.depth,
                defaulted(!mem.depth_defaulted),
                mem.data_width
            )
        } else if let Some(slave) = plan.slaves.iter().find(|s| s.bundle == binding.bundle) {
            format!(
                " addr={} bits width={}{}",
                slave.addr_width,
                slave.width,
                if slave.wdata.is_some() {
                    ""
                } else {
                    " read-only"
                }
            )
        } else if let Some(mem) = plan.axi_mems.iter().find(|m| m.bundle == binding.bundle) {
            let aperture = mem
                .aperture_bytes
                .map(|bytes| format!(" aperture={}", size_str(bytes as u64)))
                .unwrap_or_default();
            let source = match (mem.depth_defaulted, mem.backing) {
                (true, crate::manifest::Backing::Dram) => " (board's whole memory)",
                (true, _) => " (defaulted)",
                _ => "",
            };
            format!(
                " depth={}{source} axi4 data={} bytes{aperture}",
                mem.depth, mem.data_bytes
            )
        } else {
            String::new()
        };

        let matched = format!("{count} port{} ({})", plural(count), binding.how.as_str());
        println!(
            "  {:<name_width$}  backing={:<14} {matched:<20} contract={contract}{latency}{depth}{drops}{memory}{other}",
            binding.bundle, bundle.backing,
        );
    }
    println!();
}

fn print_heartbeat(heartbeat: Option<&crate::heartbeat::HeartbeatPlan>) {
    let Some(heartbeat) = heartbeat else {
        return;
    };
    println!(
        "heartbeat: {} -> {} ({}), {} baud, {} clocks per bit",
        heartbeat.port(),
        heartbeat.pin,
        heartbeat.standard,
        heartbeat.actual_baud,
        heartbeat.div
    );
    println!();
}

/// The target, and whether CI exercises its configuration.
fn print_target(target: Option<&Target>) {
    let Some(target) = target else {
        println!("target: (none given. Pass --target <provider>/<board> to check feasibility)");
        println!();
        return;
    };

    println!(
        "target: {}  {} ({})",
        target.name,
        target.head.device.part,
        target.source.path()
    );
    for patch in &target.patches {
        println!("  patch: {}", patch.display());
    }
    if target.head.board.untested {
        println!("note: not yet run on real hardware");
    }

    // Always say when the configuration is outside what CI covers.
    if !target.verified() {
        println!("WARNING: this target configuration is not one the CI exercises:");
        for reason in target.unverified_reasons() {
            println!("  - {reason}");
        }
    }
    println!();
}

/// The clock plan, with the domain of each output. Ports of one domain on one
/// output show that the plan agrees with the DUT's domain declarations.
fn print_clocks(clocks: Option<&ClockPlan>) {
    let Some(plan) = clocks else {
        return;
    };

    let pin = match (&plan.input.pin, &plan.input.standard) {
        (Some(pin), Some(standard)) => format!(" ({pin}, {standard})"),
        (Some(pin), None) => format!(" ({pin})"),
        _ => String::new(),
    };
    println!(
        "clocks: input {} = {} MHz{pin}",
        plan.input.name, plan.input.freq_mhz
    );
    for output in &plan.outputs {
        println!(
            "  {:>10} MHz  domain={:<6}  {}",
            output.freq_mhz,
            output.domain,
            if output.ports.is_empty() {
                "(no DUT port)".to_string()
            } else {
                output.ports.join(", ")
            },
        );
    }
    println!();
}

/// What a clock output drives: its DUT ports, or, for a clock that only the
/// memory controller takes, what it is for.
fn clock_user(output: &ClockOutput) -> String {
    if output.ports.is_empty() {
        output.domain.clone()
    } else {
        output.ports.join(", ")
    }
}

/// The register map. RTL and host API come from the same IR, so the offsets
/// are already visible at check time.
fn print_registers(registers: &RegisterMap, emitted: Option<&Path>) {
    println!(
        "registers: {} entries, {} bytes, map_hash = {:#010x}",
        registers.registers.len(),
        registers.size_bytes(),
        registers.map_hash,
    );

    let width = registers
        .registers
        .iter()
        .map(|register| register.name.chars().count())
        .max()
        .unwrap_or(4);

    for register in &registers.registers {
        let words = if register.words > 1 {
            format!(" ({} words)", register.words)
        } else {
            String::new()
        };
        println!(
            "  {:#06x}  {:<width$}  {:<2}  width={}{words}",
            register.offset,
            register.name,
            register.access.as_str(),
            register.width,
        );
    }

    if let Some(path) = emitted {
        println!("wrote {}", path.display());
    }
    println!();
}

/// Feasibility results. A bundle that passes under a condition always shows
/// the condition.
fn print_feasibility(feasibility: Option<&Feasibility>) {
    let Some(feasibility) = feasibility else {
        return;
    };

    println!("feasibility (transport = {}):", feasibility.transport);
    let width = feasibility
        .bundles
        .iter()
        .map(|verdict| verdict.bundle.chars().count())
        .max()
        .unwrap_or(6)
        .max(6);

    for verdict in &feasibility.bundles {
        println!(
            "  {:<width$}  {:<14} {:<14} ok",
            verdict.bundle,
            verdict.backing.as_str(),
            verdict.contract.as_str(),
        );
        for requirement in &verdict.requirements {
            println!(
                "  {:<width$}    requires {}: {}",
                "",
                requirement.id(),
                requirement.why()
            );
        }
    }
    println!();
}

/// The list that keeps exit 0 from reading as "the harness can be built".
fn print_not_checked_yet(with_target: bool, sv_blackboxes: &[String]) {
    println!("NOT CHECKED YET (exit 0 does not mean the harness is feasible):");
    for item in not_checked_items(with_target, sv_blackboxes) {
        println!("  - {}", item.what);
    }
    println!("What ran here:");
    for item in checked_items(with_target) {
        println!("  - {}", item.what);
    }
}

/// The default report: one line per item, marked `ok` or `--`, as in
/// `hio check`.
///
/// A failure never gets here; the error and its fix are printed instead.
/// Each line still shows defaults (`inferred` / `defaulted`), conditions
/// (`requires`), and a target configuration CI does not cover. `--verbose`
/// gives the full tables.
fn print_summary(plan: &plan::Plan, emit_regs: Option<&Path>) {
    let dut = &plan.dut;
    let clocks = count_role(dut, SignalRole::Clock);
    let resets = count_role(dut, SignalRole::Reset);
    line(
        "ok",
        "dut",
        &format!(
            "{}, {} port{} ({clocks} clock, {resets} reset, {} data)",
            dut.name,
            dut.ports.len(),
            plural(dut.ports.len()),
            // An interface port with both a clock and a reset counts twice.
            dut.ports.len().saturating_sub(clocks + resets)
        ),
    );

    for binding in &plan.bindings {
        let bundle = &plan.loaded.manifest.bundle[&binding.bundle];
        let mut value = format!("{}: {}", binding.bundle, bundle.backing);
        match plan
            .contracts
            .iter()
            .find(|contract| contract.bundle == binding.bundle)
        {
            Some(resolved) if resolved.declared => value += &format!(", {}", resolved.contract),
            Some(resolved) => value += &format!(", {} (inferred)", resolved.contract),
            None => {}
        }
        if let Some(latency) = bundle.latency {
            value += &format!(", latency {latency}");
        }
        if let Some(fifo) = plan.fifos.iter().find(|fifo| fifo.bundle == binding.bundle) {
            value += &format!(", depth {}", fifo.depth);
            if fifo.depth_defaulted {
                value += " (defaulted)";
            }
        }
        if let Some(mem) = plan
            .memories
            .iter()
            .find(|mem| mem.bundle == binding.bundle)
        {
            value += &format!(", depth {}", size_str(mem.depth));
        }
        let depth = plan
            .axi_mems
            .iter()
            .find(|mem| mem.bundle == binding.bundle)
            .map(|mem| {
                let dram = mem.backing == crate::manifest::Backing::Dram;
                (u64::from(mem.depth), mem.depth_defaulted, dram)
            })
            .or_else(|| {
                plan.host_mems
                    .iter()
                    .find(|mem| mem.bundle == binding.bundle)
                    .map(|mem| (u64::from(mem.depth), mem.depth_defaulted, false))
            });
        if let Some((depth, defaulted, dram)) = depth {
            value += &format!(", depth {}", size_str(depth));
            match (defaulted, dram) {
                // For `dram`, no depth means the whole board memory, not a
                // default.
                (true, true) => value += " (board's whole memory)",
                (true, false) => value += " (defaulted)",
                _ => {}
            }
        }
        let count = binding.ports.len();
        value += &format!(", {count} port{}", plural(count));
        if let Some(verdict) = plan
            .feasibility()
            .and_then(|f| f.bundles.iter().find(|v| v.bundle == binding.bundle))
        {
            for requirement in &verdict.requirements {
                value += &format!(", requires {}", requirement.id());
            }
        }
        line("ok", "bundle", &value);
    }

    match plan.target() {
        Some(target) => {
            let transport = plan
                .feasibility()
                .map(|f| format!(" over {}", f.transport))
                .unwrap_or_default();
            let patched = if target.patches.is_empty() {
                ""
            } else {
                ", patched"
            };
            line(
                "ok",
                "target",
                &format!(
                    "{} ({}){transport}{patched}",
                    target.name, target.head.device.part
                ),
            );
            if !target.verified() {
                line(
                    "--",
                    "target",
                    &format!(
                        "not a configuration CI exercises: {}",
                        target.unverified_reasons().join("; ")
                    ),
                );
            }
            if target.head.board.untested {
                line("--", "target", "not yet run on real hardware");
            }
        }
        None => line(
            "--",
            "target",
            "none given. Pass --target <provider>/<board> to check feasibility and clocks",
        ),
    }

    if let Some(clocks) = plan.clocks() {
        for output in &clocks.outputs {
            line(
                "ok",
                "clock",
                &format!(
                    "{} {} MHz (from {} {} MHz)",
                    clock_user(output),
                    output.freq_mhz,
                    clocks.input.name,
                    clocks.input.freq_mhz
                ),
            );
        }
    }
    if let Some(heartbeat) = &plan.heartbeat {
        line(
            "ok",
            "uart",
            &format!(
                "{} -> {}, {} baud",
                heartbeat.port(),
                heartbeat.pin,
                heartbeat.actual_baud
            ),
        );
    }

    let map = &plan.registers;
    line(
        "ok",
        "map",
        &format!(
            "{} registers, {}, hash {:#010x}",
            map.registers.len(),
            size_str(map.size_bytes() as u64),
            map.map_hash
        ),
    );
    if let Some(path) = emit_regs {
        line("ok", "regs", &format!("wrote {}", path.display()));
    }

    let not_checked: Vec<&str> = not_checked_items(plan.board.is_some(), &plan.sv_blackboxes)
        .iter()
        .map(|item| short_not_checked(item.id))
        .collect();
    println!("not checked: {}", not_checked.join(", "));
}

fn line(mark: &str, what: &str, value: &str) {
    println!("{mark}  {what:<8} {value}");
}

/// Short names for the not-checked items. `--verbose` and `--json` give the
/// long text.
fn short_not_checked(id: &str) -> &'static str {
    match id {
        "latency_vs_rtl" => "the stated latency against the RTL",
        "clock_feasibility" => "whether the MMCM can make the clocks",
        "reset_and_cdc" => "reset release and CDC",
        "capacity" => "whether the whole design fits",
        "latency_the_dut_expects" => "the latency the DUT expects",
        "observe_loss" => "observe sample loss",
        "feasibility" => "feasibility (no --target)",
        "board_pins" => "board pins (no --target)",
        "sv_blackbox_sources" => "$sv:: sources",
        _ => "other items (see --verbose)",
    }
}

/// A size spelled as `--size` takes it (`64M`, `4k`).
fn size_str(n: u64) -> String {
    for (unit, shift) in [("G", 30), ("M", 20), ("k", 10)] {
        if n >= 1 << shift && n.is_multiple_of(1 << shift) {
            return format!("{}{unit}", n >> shift);
        }
    }
    n.to_string()
}

/// The path relative to the current directory, when possible.
fn shown(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(|p| p.display().to_string()))
        .unwrap_or_else(|| path.display().to_string())
}

/// Empty for ports outside any bundle (clock/reset).
fn role_cell(roles: &HashMap<&str, String>, port: &str) -> String {
    roles.get(port).cloned().unwrap_or_default()
}

fn write_regs(
    path: &Path,
    registers: &RegisterMap,
    target: Option<&crate::target::Target>,
    clocks: Option<&crate::clock::ClockPlan>,
    pcie: Option<&crate::manifest::Pcie>,
) -> miette::Result<()> {
    let text = json::render_register_map(registers, target, clocks, pcie);
    std::fs::write(path, format!("{text}\n")).map_err(|source| {
        miette::miette!(
            code = "harness::check::emit_regs_failed",
            help = "--emit-regs writes the register map as JSON. The directory has to exist and be writable.",
            "cannot write `{}`: {source}",
            path.display()
        )
    })
}

fn role_of(port: &crate::dut::Port) -> Option<SignalRole> {
    port.signals.first().map(|signal| signal.role)
}

fn count_role(dut: &Dut, role: SignalRole) -> usize {
    dut.ports
        .iter()
        .filter(|port| port.signals.iter().any(|signal| signal.role == role))
        .count()
}

/// Column width, at least `min` so that the header stays aligned.
fn width_of<'a>(items: impl Iterator<Item = &'a str>, min: usize) -> usize {
    items
        .map(|item| item.chars().count())
        .max()
        .unwrap_or(min)
        .max(min)
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// `?` for an unresolved value, never 0 or 1.
fn opt(value: Option<usize>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "?".to_string(),
    }
}

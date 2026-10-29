//! The text of the generated files: Veryl, Tcl, XDC and Makefile.
//!
//! Integration tests run the generated Veryl through the analyzer, and
//! `tests/sim.rs` runs it in the simulator.
//!
//! ## `unsafe (cdc)` marks only crossings the harness inserts itself
//!
//! - the MMCM instance: it makes a new clock, so it is a domain boundary.
//! - the reset synchronizers: `locked` and the board reset are in the input
//!   domain, and the flops run on the output clock.
//! - on a board whose board reset is PERST# (one pin), the board reset taken
//!   from the PCIe port. It is asynchronous and both ends synchronize it.
//! - the edges of Vivado black boxes (the memory controller, the Zynq PS, the
//!   PCIe wrapper), and the synchronizers that bring their status and counts
//!   to the window clock.
//!
//! Only these may get a false path. The compiler rejects any other crossing
//! that is not wrapped.

use miette::Diagnostic;
use thiserror::Error;
use veryl_formatter::Formatter;
use veryl_metadata::Metadata;
use veryl_parser::Parser;

use veryl_metadata::ResetType;

use crate::bundle::DirectionPrefixes;
use crate::clock::ClockPlan;
use crate::dut::{Dut, PortDirection};
use crate::generate::MARKER;
use crate::plan::Plan;
use crate::regmap::{Access, ClearSource, DMA_GO, DUT_RESET, DUT_RESET_STATE, Kind, WORD_BITS};
use crate::unconnected::Kind as UnconnectedKind;

#[derive(Debug, Error, Diagnostic)]
#[error("the generator produced Veryl it cannot parse: {message}")]
#[diagnostic(
    code(harness::emit::unparsable),
    help(
        "This is a bug in veryl-harness, not in your project -- nothing was written.\n\nSet HNS_DEBUG_EMIT=1 to print what was generated; without it there is no way to see the text that failed, which is the only thing that helps."
    )
)]
pub struct EmitError {
    pub message: String,
}

/// Formats generated Veryl with Veryl's own formatter.
///
/// - The output matches `veryl fmt`, so a user CI that runs `fmt --check`
///   does not fail on generated files.
/// - It parses the output before anything is written. Broken Veryl from the
///   generator stops here, not in a file.
pub fn format(text: &str, metadata: &Metadata) -> Result<String, EmitError> {
    // `Parser::parse` takes `&T`, so this borrow is needed (clippy is wrong).
    #[allow(clippy::needless_borrow)]
    let parser = Parser::parse(&text, &"<generated>").map_err(|err| {
        // Without this there is no way to see the text that failed to parse.
        if std::env::var("HNS_DEBUG_EMIT").is_ok() {
            eprintln!("--- generated Veryl that would not parse ---");
            for (i, line) in text.lines().enumerate() {
                eprintln!("{:4} {line}", i + 1);
            }
            eprintln!("--- end ---");
        }
        EmitError {
            message: format!("{err}"),
        }
    })?;
    let mut formatter = Formatter::new(metadata);
    formatter.format(&parser.veryl, text);
    Ok(formatter.as_str().to_string())
}

/// The IP module name as Veryl sees it. It must match `-module_name` in the Tcl.
pub const MMCM_MODULE: &str = "hns_mmcm";

/// `hns/src/clk.veryl`.
///
/// The board reset polarity and the DUT reset polarity are different things.
/// The first comes from the target (`[resets.sys] active`), the second from
/// `[build] reset_type` of the project, and this module converts between them.
/// On VCU118 the board reset is active high.
/// The DUT reset (`o_drst_*`) is separate from the harness reset (`o_rst_*`).
/// It is the same two-stage synchronizer plus the host request (`i_dut_rst`,
/// in the window clock).
pub fn clock_module(clocks: &ClockPlan, dut_reset: ResetType, window: &str) -> String {
    let mut out = String::new();

    out.push_str(&header_veryl());
    out.push_str("///\n/// Clock assignment:\n");
    out.push_str(&format!(
        "///   input {} = {} MHz{}\n",
        clocks.input.name,
        clocks.input.freq_mhz,
        if clocks.input.diff {
            " (differential)"
        } else {
            ""
        }
    ));
    for output in &clocks.outputs {
        out.push_str(&format!(
            "///     -> {:>10} MHz  domain={:<6} {}\n",
            output.freq_mhz,
            output.domain,
            output.ports.join(", ")
        ));
    }

    // The input uses the board polarity; the outputs use the DUT polarity.
    let board_reset_type = if clocks.reset.active_low {
        "reset_async_low"
    } else {
        "reset_async_high"
    };
    let reset_type = reset_type_name(dut_reset);

    out.push_str("module clk (\n");
    if clocks.input.diff {
        // Differential input: the pair is one clock source, so both go to the IP.
        out.push_str("    i_sys_clk_p: input 'sys clock,\n");
        out.push_str("    i_sys_clk_n: input 'sys clock,\n");
    } else {
        out.push_str("    i_sys_clk: input 'sys clock,\n");
    }
    out.push_str(&format!("    i_sys_rst: input 'sys {board_reset_type},\n"));
    for output in &clocks.outputs {
        let ident = &output.ident;
        out.push_str(&format!("    o_clk_{ident}: output '{ident} clock,\n"));
        out.push_str(&format!(
            "    o_rst_{ident}: output '{ident} {reset_type},\n"
        ));
        out.push_str(&format!(
            "    o_drst_{ident}: output '{ident} {reset_type},\n"
        ));
    }
    out.push_str(&format!("    i_dut_rst: input '{window} logic,\n"));
    out.push_str(") {\n");

    out.push_str("    var locked: 'sys logic;\n\n");
    out.push_str("    // The MMCM makes a new clock, so it IS a domain boundary.\n");
    out.push_str("    // Declared here as an intended crossing.\n");
    out.push_str("    unsafe (cdc) {\n");
    out.push_str(&format!("        inst u_mmcm: $sv::{MMCM_MODULE} (\n"));
    if clocks.input.diff {
        out.push_str("            clk_in1_p: i_sys_clk_p,\n");
        out.push_str("            clk_in1_n: i_sys_clk_n,\n");
    } else {
        out.push_str("            clk_in1: i_sys_clk,\n");
    }
    // The port name depends on the reset polarity (measured: `resetn` for
    // ACTIVE_LOW, `reset` for ACTIVE_HIGH).
    out.push_str(&format!(
        "            {}: i_sys_rst,\n",
        veryl_ident(if clocks.reset.active_low {
            "resetn"
        } else {
            "reset"
        })
    ));
    out.push_str("            locked: locked,\n");
    for (index, output) in clocks.outputs.iter().enumerate() {
        out.push_str(&format!(
            "            clk_out{}: o_clk_{},\n",
            index + 1,
            output.ident
        ));
    }
    out.push_str("        );\n    }\n\n");

    for output in &clocks.outputs {
        let ident = &output.ident;
        out.push_str(&format!("    var rst_meta_{ident}: '{ident} logic;\n"));
        out.push_str(&format!("    var rst_sync_{ident}: '{ident} logic;\n\n"));
        out.push_str(&format!(
            "    // `locked` and the board reset are in 'sys; this is clocked by o_clk_{ident},\n"
        ));
        out.push_str(
            "    // so it crosses. This is the synchroniser the harness itself inserted.\n",
        );
        out.push_str("    unsafe (cdc) {\n");
        out.push_str(&format!(
            "        always_ff (o_clk_{ident}, i_sys_rst) {{\n"
        ));
        // The asserted value follows the output reset polarity: 0 for active
        // low, 1 for active high. A wrong polarity only shows as "nothing runs".
        let asserted_value = asserted(dut_reset);
        let released = if asserted_value == 0 {
            "locked"
        } else {
            "~locked"
        };
        out.push_str("            if_reset {\n");
        out.push_str(&format!(
            "                rst_meta_{ident} = {asserted_value};\n"
        ));
        out.push_str(&format!(
            "                rst_sync_{ident} = {asserted_value};\n"
        ));
        out.push_str("            } else {\n");
        out.push_str(&format!("                rst_meta_{ident} = {released};\n"));
        out.push_str(&format!(
            "                rst_sync_{ident} = rst_meta_{ident};\n"
        ));
        out.push_str("            }\n        }\n    }\n\n");
        out.push_str(&format!(
            "    assign o_rst_{ident} = rst_sync_{ident} as {reset_type};\n\n"
        ));

        // The DUT reset. The host request takes the same path as `locked`, so
        // no LUT sits on the asynchronous reset side.
        let drop = if asserted_value == 0 {
            "locked & ~i_dut_rst"
        } else {
            "~locked | i_dut_rst"
        };
        out.push_str(&format!("    var drst_meta_{ident}: '{ident} logic;\n"));
        out.push_str(&format!("    var drst_sync_{ident}: '{ident} logic;\n\n"));
        out.push_str(
            "    // The DUT reset: the same synchroniser, plus the host's request (dut_reset).\n",
        );
        out.push_str("    unsafe (cdc) {\n");
        out.push_str(&format!(
            "        always_ff (o_clk_{ident}, i_sys_rst) {{\n"
        ));
        out.push_str("            if_reset {\n");
        out.push_str(&format!(
            "                drst_meta_{ident} = {asserted_value};\n"
        ));
        out.push_str(&format!(
            "                drst_sync_{ident} = {asserted_value};\n"
        ));
        out.push_str("            } else {\n");
        out.push_str(&format!("                drst_meta_{ident} = {drop};\n"));
        out.push_str(&format!(
            "                drst_sync_{ident} = drst_meta_{ident};\n"
        ));
        out.push_str("            }\n        }\n    }\n\n");
        out.push_str(&format!(
            "    assign o_drst_{ident} = drst_sync_{ident} as {reset_type};\n\n"
        ));
    }

    out.push_str("}\n");
    out
}

/// `hns/syn/mig.tcl`: the memory controller.
///
/// The settings are read whole from the board's `mig.prj`
/// (`CONFIG.XML_INPUT_FILE`), which `gen` writes beside this script. It holds
/// pin locations and timing, so the generator does not restate them.
pub fn mig_tcl(plan: &Plan) -> Option<String> {
    if dram_pins(plan).is_empty() || is_ps7(plan) {
        return None;
    }
    let target = plan.target()?;
    // UltraScale+ DDR4 has no `mig.prj`, so its settings are listed instead.
    if let Some(dram) = hns_targets::dram(target).filter(|d| d.is_ddr4_ip()) {
        return Some(ddr4_tcl(&dram));
    }
    mig_prj_name(target)?;
    let mut out = header_tcl();
    out.push_str(
        "#
# The controller's settings come from mig.prj, a copy of the board file.

",
    );
    out.push_str(&format!(
        "create_ip -vendor xilinx.com -library ip -name mig_7series -module_name {MIG_MODULE} -dir $ip_dir -force
"
    ));
    out.push_str(&format!(
        "set_property CONFIG.XML_INPUT_FILE [file normalize mig.prj] [get_ips {MIG_MODULE}]
"
    ));
    Some(out)
}

/// The module names Vivado gives the Zynq PS and the AXI4 to AXI3 converter.
pub const PS7_MODULE: &str = "hns_ps7";
pub const AXI_PC_MODULE: &str = "hns_axi_pc";

/// Whether `dram` is a Zynq-7000 PS's memory (`controller = "ps7"`).
pub fn is_ps7(plan: &Plan) -> bool {
    has_dram(plan) && target_dram(plan).is_some_and(|d| d.is_ps7())
}

/// Whether the manifest puts a bundle on the board's memory. What the target
/// offers matters only then.
fn has_dram(plan: &Plan) -> bool {
    plan.axi_mems
        .iter()
        .any(|m| m.backing == crate::manifest::Backing::Dram)
}

/// `hns/syn/ps7.tcl`: the Zynq PS with S_AXI_HP0, and the converter in
/// front of it.
pub fn ps7_tcl(plan: &Plan) -> Option<String> {
    if !is_ps7(plan) {
        return None;
    }
    let mut out = header_tcl();
    out.push_str(&format!(
        r#"#
# The Zynq PS and the AXI4 to AXI3 converter in front of its S_AXI_HP0.
# The PS settings are the board's (ps7_preset.tcl). Only HP0 and
# FCLK_RESET0_N reach the PL: the harness drives HP0 on its own clock, and
# FCLK_RESET0_N rises once the host has set the PS up (ps7_init).

source [file join [file dirname [info script]] ps7_preset.tcl]
create_ip -vendor xilinx.com -library ip -name processing_system7 -module_name {PS7_MODULE} -dir $ip_dir -force
set_property -dict $hns_ps7_config [get_ips {PS7_MODULE}]
set_property -dict [list     CONFIG.PCW_USE_S_AXI_HP0 {{1}} CONFIG.PCW_S_AXI_HP0_DATA_WIDTH {{64}}     CONFIG.PCW_USE_S_AXI_HP1 {{0}} CONFIG.PCW_USE_S_AXI_HP2 {{0}} CONFIG.PCW_USE_S_AXI_HP3 {{0}}     CONFIG.PCW_USE_S_AXI_ACP {{0}} CONFIG.PCW_USE_S_AXI_GP0 {{0}} CONFIG.PCW_USE_S_AXI_GP1 {{0}}     CONFIG.PCW_USE_M_AXI_GP0 {{0}} CONFIG.PCW_USE_M_AXI_GP1 {{0}}     CONFIG.PCW_EN_CLK0_PORT {{0}} CONFIG.PCW_EN_CLK1_PORT {{0}} CONFIG.PCW_EN_CLK2_PORT {{0}} CONFIG.PCW_EN_CLK3_PORT {{0}}     CONFIG.PCW_EN_RST0_PORT {{1}} CONFIG.PCW_EN_RST1_PORT {{0}} CONFIG.PCW_EN_RST2_PORT {{0}} CONFIG.PCW_EN_RST3_PORT {{0}}     CONFIG.PCW_USE_FABRIC_INTERRUPT {{0}} CONFIG.PCW_GPIO_EMIO_GPIO_ENABLE {{0}} ] [get_ips {PS7_MODULE}]

create_ip -vendor xilinx.com -library ip -name axi_protocol_converter -module_name {AXI_PC_MODULE} -dir $ip_dir -force
set_property -dict [list     CONFIG.SI_PROTOCOL {{AXI4}} CONFIG.MI_PROTOCOL {{AXI3}} CONFIG.TRANSLATION_MODE {{2}}     CONFIG.DATA_WIDTH {{64}} CONFIG.ADDR_WIDTH {{32}} CONFIG.ID_WIDTH {{6}} ] [get_ips {AXI_PC_MODULE}]
"#
    ));
    Some(out)
}

/// `hns/syn/ps7_preset.tcl`: copied from beside the target description.
/// `feasibility::check` has made sure it is there.
pub fn ps7_preset(plan: &Plan) -> Option<String> {
    if !is_ps7(plan) {
        return None;
    }
    let target = plan.target()?;
    let name = ps7_preset_name(target)?;
    let body = crate::target::read_beside(target, name)?;
    let mut out = header_tcl();
    out.push_str(&format!(
        "# Copied from {}
#
",
        target.source.beside(name)
    ));
    out.push_str(&body);
    Some(out)
}

/// `hns/syn/mig.prj`: copied from beside the target description.
/// `feasibility::check` has made sure it is there.
pub fn mig_prj(plan: &Plan) -> Option<String> {
    mig_tcl(plan)?;
    let target = plan.target()?;
    let name = mig_prj_name(target)?;
    let prj = crate::target::read_beside(target, name)?;
    Some(mark_xml(&prj, &target.source.beside(name)))
}

/// `hns/syn/dram.xdc`: the memory pins, copied from beside the target
/// description (`[provides.dram] pins_xdc`). Only a DDR4 controller without a
/// board interface has one; `feasibility::check` has made sure it is there and
/// places every memory port.
pub fn dram_xdc(plan: &Plan) -> Option<String> {
    if dram_pins(plan).is_empty() || !is_ddr4(plan) {
        return None;
    }
    let target = plan.target()?;
    let name = target_dram(plan)?.pins_xdc?;
    let xdc = crate::target::read_beside(target, &name)?;
    let mut out = header_tcl();
    out.push_str(&format!(
        "#\n# The memory pins. Copied from {}\n\n",
        target.source.beside(&name)
    ));
    out.push_str(&xdc);
    Some(out)
}

/// The memory ports an XDC gives a `PACKAGE_PIN`, one entry per bit
/// (`c0_ddr4_dq[3]`, or `c0_ddr4_act_n` for a 1-bit port).
pub fn placed_ports(xdc: &str) -> Vec<String> {
    let mut ports = Vec::new();
    for line in xdc.lines() {
        let line = line.trim();
        if line.starts_with('#') || !line.contains("PACKAGE_PIN") {
            continue;
        }
        let Some((_, rest)) = line.split_once("get_ports") else {
            continue;
        };
        // `{c0_ddr4_dq[3]}` ends at the brace; a bare `c0_ddr4_act_n` at the
        // bracket that closes `[get_ports ...]`.
        let rest = rest.trim_start();
        let name: String = match rest.strip_prefix('{') {
            Some(braced) => braced
                .trim_start()
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '}')
                .collect(),
            None => rest
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != ']')
                .collect(),
        };
        if !name.is_empty() {
            ports.push(name);
        }
    }
    ports
}

/// Adds the marker as a comment after the XML declaration, so that `gen` may
/// write the file again. The declaration must stay on the first line.
fn mark_xml(xml: &str, from: &str) -> String {
    // `--` is not allowed inside an XML comment.
    let note = format!(
        "<!-- {MARKER}. Copied from {} -->\n",
        from.replace("--", "- -")
    );
    // A byte order mark comes before the declaration (the KC705 file has one).
    let (bom, body) = match xml.strip_prefix('\u{feff}') {
        Some(body) => ("\u{feff}", body),
        None => ("", xml),
    };
    match body.split_once('\n') {
        Some((first, rest)) if first.starts_with("<?xml") => {
            format!("{bom}{first}\n{note}{rest}")
        }
        _ => format!("{bom}{note}{body}"),
    }
}

/// `[vivado] mig_prj`: a file name beside the target description.
pub fn mig_prj_name(target: &crate::target::Target) -> Option<&str> {
    target
        .table
        .get("vivado")
        .and_then(|x| x.as_table())
        .and_then(|v| v.get("mig_prj"))
        .and_then(|x| x.as_str())
}

/// The file of PS7 settings a Zynq target names in `[vivado] ps7_preset`.
pub fn ps7_preset_name(target: &crate::target::Target) -> Option<&str> {
    target
        .table
        .get("vivado")
        .and_then(|x| x.as_table())
        .and_then(|v| v.get("ps7_preset"))
        .and_then(|x| x.as_str())
}

/// The line that sets the board part. Both IP generation and synthesis need it.
///
/// The IP writes its XDC as `set_property BOARD_PIN {c1_ddr4_adr0} ...`.
/// Without a board part, BOARD_PIN does not resolve to a real pin. Vivado only
/// gives a critical warning (`Undefined BOARD_PART property`) and fails later,
/// at placement. IP generation and synthesis are separate Vivado runs, so each
/// needs the line.
///
/// It is set only when needed. Boards whose `mig.prj` holds the pins (Arty)
/// do not need it, and setting it could change their flow.
fn board_part_tcl(out: &mut String, plan: &Plan) {
    if !uses_board_interface(plan) {
        return;
    }
    if let Some(board) = plan
        .target()
        .and_then(|t| t.table.get("vivado"))
        .and_then(|x| x.as_table())
        .and_then(|v| v.get("board_part"))
        .and_then(|x| x.as_str())
    {
        out.push_str(&format!(
            "set_property board_part {{{board}}} [current_project]\n"
        ));
    }
}

/// Whether the controller takes its pin placement from the board files.
fn uses_board_interface(plan: &Plan) -> bool {
    has_dram(plan) && target_dram(plan).is_some_and(|d| d.board_interface.is_some())
}

/// Whether the controller is UltraScale+ DDR4. Its port names are all different.
fn is_ddr4(plan: &Plan) -> bool {
    target_dram(plan).is_some_and(|d| d.is_ddr4_ip())
}

/// `[provides.dram]` of the target, if there is a target and it has memory.
fn target_dram(plan: &Plan) -> Option<hns_targets::Dram> {
    hns_targets::dram(plan.target()?)
}

/// The top-level port names of the clock that the controller takes itself.
///
/// A differential clock has two (p/n). The IP has its own IBUFDS, so the
/// harness does not route it.
pub fn controller_clock_ports(clock: &crate::clock::InputClock) -> Vec<String> {
    if clock.diff {
        vec!["i_mig_clk_p".to_string(), "i_mig_clk_n".to_string()]
    } else {
        vec!["i_mig_clk".to_string()]
    }
}

/// The Tcl line continuation. A bare `\` at the end of a Rust string line is
/// itself a line continuation and eats the newline, so it is kept here.
const BACKSLASH: &str = "\\";

/// The UltraScale+ DDR4 controller. There is no `mig.prj`, so the settings
/// are listed.
///
/// The values come from the target. They were measured by generating the IP
/// once and reading it back, not taken from a datasheet.
fn ddr4_tcl(dram: &hns_targets::Dram) -> String {
    let mut out = header_tcl();
    out.push_str(
        "#
# The DDR4 controller. UltraScale+ has no project file to point at, so the
# settings are listed; they come from the target description, which recorded
# them by generating the IP once and reading it back.

",
    );
    out.push_str(&format!(
        "create_ip -vendor xilinx.com -library ip -name ddr4 -module_name {MIG_MODULE} -dir $ip_dir -force
"
    ));
    // Pass everything in one `set_property -dict`. The IP validates settings as
    // a set, so some values are rejected when they are added one at a time.
    out.push_str("set_property -dict [list ");
    out.push_str(BACKSLASH);
    out.push('\n');
    let mut put = |key: &str, value: String| {
        // One key is spelled differently: the board interface is
        // `CONFIG.C0_DDR4_BOARD_INTERFACE`, not `C0.DDR4_...`.
        let full = if key == "_BOARD_INTERFACE" {
            "C0_DDR4_BOARD_INTERFACE".to_string()
        } else {
            format!("C0.DDR4_{key}")
        };
        out.push_str(&format!("    CONFIG.{full} {{{value}}} "));
        out.push_str(BACKSLASH);
        out.push('\n');
    };
    // A board interface makes the board file fill in everything: part, speed,
    // CAS, and the pin constraints. Without it, the pins come from the XDC
    // beside the target description (`dram_xdc`); with neither, `opt_design`
    // fails with "ports are not placed" (measured).
    if let Some(name) = &dram.board_interface {
        put("_BOARD_INTERFACE", name.clone());
    }
    put("AxiSelection", "true".to_string());
    if let Some(v) = dram.axi_data_bits {
        put("AxiDataWidth", v.to_string());
    }
    if let Some(v) = dram.axi_id_bits {
        put("AxiIDWidth", v.to_string());
    }
    // The memory settings are listed only when there is no board interface.
    if dram.board_interface.is_none() {
        if let Some(v) = dram.mem_clk_ps {
            put("TimePeriod", v.to_string());
        }
        if let Some(v) = dram.sys_clk_ps {
            put("InputClockPeriod", v.to_string());
        }
        if let Some(v) = &dram.memory_type {
            put("MemoryType", v.clone());
        }
        if let Some(v) = &dram.part {
            put("MemoryPart", v.clone());
        }
        if let Some(v) = dram.width {
            put("DataWidth", v.to_string());
        }
        if let Some(v) = dram.cas_latency {
            put("CasLatency", v.to_string());
        }
        if let Some(v) = dram.cas_write_latency {
            put("CasWriteLatency", v.to_string());
        }
        if let Some(v) = &dram.data_mask {
            put("DataMask", v.clone());
        }
        put("Mem_Add_Map", "ROW_COLUMN_BANK_INTLV".to_string());
    }
    // Drop the last line continuation.
    let mut out = out.trim_end().trim_end_matches(BACKSLASH).to_string();
    out.push_str(&format!(
        "
] [get_ips {MIG_MODULE}]
"
    ));
    out
}

/// `hns/syn/pcie.xdc`: the PCIe pins of the board.
///
/// Only the reference clock, the reset and the lanes, from `[pcie]` in the
/// target. The n side of each pair is not written; Vivado derives it.
pub fn pcie_xdc(board: &hns_targets::Pcie) -> String {
    let mut out = header_tcl();
    out.push_str("#\n# PCIe pins, from the target description.\n\n");

    if let Some(pin) = &board.refclk_p {
        out.push_str(&format!(
            "set_property -dict {{ PACKAGE_PIN {pin} }} [get_ports {{ i_pcie_refclk_p }}]\n"
        ));
    }
    if let Some(mhz) = board.refclk_mhz {
        // Only on the p side: the pair is one clock source.
        out.push_str(&format!(
            "create_clock -period {:.3} -name pcie_refclk [get_ports {{ i_pcie_refclk_p }}]\n",
            1000.0 / mhz
        ));
    }
    if let (Some(pin), Some(standard)) = (&board.reset_n, &board.reset_standard) {
        out.push_str(&format!(
            "set_property -dict {{ PACKAGE_PIN {pin} IOSTANDARD {standard} PULLUP true }} [get_ports {{ i_pcie_reset_n }}]\n"
        ));
        // The reset arrives asynchronously, so it is not timed.
        out.push_str("set_false_path -from [get_ports { i_pcie_reset_n }]\n");
    }
    out.push('\n');

    for (i, pin) in board.rx_p.iter().enumerate() {
        out.push_str(&format!(
            "set_property -dict {{ PACKAGE_PIN {pin} }} [get_ports {{ i_pcie_rx_p[{i}] }}]\n"
        ));
    }
    for (i, pin) in board.tx_p.iter().enumerate() {
        out.push_str(&format!(
            "set_property -dict {{ PACKAGE_PIN {pin} }} [get_ports {{ o_pcie_tx_p[{i}] }}]\n"
        ));
    }
    out
}

/// The PCIe AXI-Stream width and user clock, set by the link bandwidth.
///
/// x8 Gen3 is 256 bit at 250 MHz; fewer lanes mean a lower frequency.
///
/// The XDC uses this too. The user clock is faster than the harness clock, so
/// it sets the period that bounds the PCIe CDC. One source keeps the IP and
/// the constraints in step when the lane count changes.
///
/// Returns `None` for a pair that is not in the table;
/// `plan::generatable` rejects it.
pub fn pcie_stream(generation: u32, lanes: u32) -> Option<(&'static str, u32)> {
    match (generation, lanes) {
        (3, 16) | (4, 8) => Some(("512_bit", 250)),
        (3, 8) | (4, 4) => Some(("256_bit", 250)),
        (3, 4) | (4, 2) => Some(("256_bit", 125)),
        _ => None,
    }
}

/// The PCIe hard block of a device family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcieBlock {
    /// UltraScale+ (`pcie4_uscale_plus`).
    Pcie4,
    /// UltraScale (`pcie3_ultrascale`).
    Pcie3,
}

impl PcieBlock {
    /// The Vivado IP, which is also the module name without `_0`.
    pub fn ip(self) -> &'static str {
        match self {
            PcieBlock::Pcie4 => "pcie4_uscale_plus",
            PcieBlock::Pcie3 => "pcie3_ultrascale",
        }
    }
}

/// Families with a known PCIe block. Only families a harness was built for
/// are listed: the VCU118 (`virtexuplus`), and the KCU105 (`kintexu`) and
/// VU440 (`virtexu`), whose PCIE3 block is the same.
pub const PCIE_FAMILIES: &[(&str, PcieBlock)] = &[
    ("virtexuplus", PcieBlock::Pcie4),
    ("kintexuplus", PcieBlock::Pcie4),
    ("kintexu", PcieBlock::Pcie3),
    ("virtexu", PcieBlock::Pcie3),
];

/// The PCIe block of the target's device, or `None` if it is not known.
pub fn pcie_block(target: &crate::target::Target) -> Option<PcieBlock> {
    PCIE_FAMILIES
        .iter()
        .find(|(family, _)| *family == target.head.device.family)
        .map(|(_, block)| *block)
}

/// The link, already checked by `plan::generatable`.
fn board_link(board: &hns_targets::Pcie) -> (u32, u32, &'static str, u32) {
    let generation = board.max_gen.expect("plan::generatable checks max_gen");
    let lanes = board.lanes.expect("plan::generatable checks lanes");
    let (width, mhz) = pcie_stream(generation, lanes).expect("plan::generatable checks the link");
    (generation, lanes, width, mhz)
}

/// `hns/syn/pcie.tcl`: the PCIe hard IP. `ip.tcl` sources it.
///
/// Lanes and generation come from the target. The IDs and the BAR size come
/// from `Harness.toml`.
///
/// The settings follow the verilog-pcie VCU118 example. Where they differ,
/// a comment says why.
pub fn pcie_tcl(plan: &Plan, board: &hns_targets::Pcie) -> String {
    let pcie = plan.loaded.manifest.pcie.clone().unwrap_or_default();
    let (generation, lanes, width, freq) = board_link(board);
    let block = plan
        .target()
        .and_then(pcie_block)
        .expect("plan::generatable checks the family");
    let ip = block.ip();
    let speed = if generation == 4 {
        "16.0_GT/s"
    } else {
        "8.0_GT/s"
    };

    let mut out = header_tcl();
    out.push_str("#\n# PCIe hard block.\n#\n");
    out.push_str(&format!(
        "# Lanes and generation come from the target description ({lanes} lanes, gen {generation}).\n"
    ));
    out.push_str(
        "# What the device calls itself, and how much address space it claims, come\n         # from [pcie] in Harness.toml.\n\n",
    );
    out.push_str(
        &format!("create_ip -name {ip} -vendor xilinx.com -library ip \\\n             -module_name {ip}_0 -dir $ip_dir -force\n\n"),
    );
    out.push_str("set_property -dict [list \\\n");
    out.push_str(&format!(
        "    CONFIG.PL_LINK_CAP_MAX_LINK_SPEED {{{speed}}} \\\n"
    ));
    out.push_str(&format!(
        "    CONFIG.PL_LINK_CAP_MAX_LINK_WIDTH {{X{lanes}}} \\\n"
    ));
    out.push_str(&format!("    CONFIG.axisten_if_width {{{width}}} \\\n"));
    out.push_str(&format!("    CONFIG.axisten_freq {{{freq}}} \\\n"));
    out.push_str("    CONFIG.axisten_if_enable_client_tag {true} \\\n");
    out.push_str("    CONFIG.AXISTEN_IF_RC_STRADDLE {false} \\\n");
    out.push_str(&format!(
        "    CONFIG.vendor_id {{{:04x}}} \\\n",
        pcie.vendor_id
    ));
    out.push_str(&format!(
        "    CONFIG.PF0_DEVICE_ID {{{:04x}}} \\\n",
        pcie.device_id
    ));
    out.push_str(&format!(
        "    CONFIG.PF0_SUBSYSTEM_VENDOR_ID {{{:04x}}} \\\n",
        pcie.vendor_id
    ));
    out.push_str(&format!(
        "    CONFIG.PF0_SUBSYSTEM_ID {{{:04x}}} \\\n",
        pcie.device_id
    ));
    // The class code. The verilog-pcie example claims to be an Ethernet
    // controller, but this is not a network card, and a host driver must not
    // bind to it; the default is "none of the classes" (`ff0000`).
    out.push_str("    CONFIG.PF0_Use_Class_Code_Lookup_Assistant {false} \\\n");
    match block {
        PcieBlock::Pcie4 => out.push_str(&format!(
            "    CONFIG.PF0_CLASS_CODE {{{:06x}}} \\\n",
            pcie.class_code
        )),
        // On the PCIE3 IP, `PF0_CLASS_CODE` is derived and cannot be set:
        // Vivado ignores it with a warning (IP_Flow 19-3374) and the card
        // reports `058000`. It is built from these three.
        PcieBlock::Pcie3 => {
            let [_, base, sub, interface] = pcie.class_code.to_be_bytes();
            out.push_str(&format!(
                "    CONFIG.pf0_class_code_base {{{base:02x}}} \\\n"
            ));
            out.push_str(&format!("    CONFIG.pf0_class_code_sub {{{sub:02x}}} \\\n"));
            out.push_str(&format!(
                "    CONFIG.pf0_class_code_interface {{{interface:02x}}} \\\n"
            ));
        }
    }
    let (scale, size) = bar_scale(pcie.bar_bytes);
    out.push_str(&format!("    CONFIG.pf0_bar0_scale {{{scale}}} \\\n"));
    out.push_str(&format!("    CONFIG.pf0_bar0_size {{{size}}} \\\n"));
    // A 32-bit BAR, not prefetchable. The window is a few KB, and some
    // registers change state on read (a FIFO pop).
    out.push_str("    CONFIG.pf0_bar0_64bit {false} \\\n");
    out.push_str("    CONFIG.pf0_bar0_prefetchable {false} \\\n");
    // A `host_irq` bundle is sent as INTA. Without one the card has no
    // interrupt pin; the PCIE3 IP would otherwise default to INTA. MSI and
    // MSI-X stay off.
    out.push_str(&format!(
        "    CONFIG.PF0_INTERRUPT_PIN {{{}}} \\\n",
        if plan.irqs.is_empty() { "NONE" } else { "INTA" }
    ));
    out.push_str("    CONFIG.pf0_msi_enabled {false} \\\n");
    // A `pcie_flr` bundle sets the FLR bit in Device Capabilities. Without
    // one the host never sends an FLR, and nothing would answer it.
    out.push_str(&format!(
        "    CONFIG.PF0_DEV_CAP_FUNCTION_LEVEL_RESET_CAPABLE {{{}}} \\\n",
        !plan.flrs.is_empty()
    ));
    out.push_str("    CONFIG.pf0_msix_enabled {false} \\\n");
    // Where lane 0 is. The IP's default quad suits some boards and not
    // others: on the AS02MC04 it puts lane 0 at the wrong end, and the link
    // trains reversed at x4 at best.
    let quad = board
        .gt_quad
        .as_ref()
        .expect("plan::generatable checks gt_quad");
    let site = board
        .block
        .as_ref()
        .expect("plan::generatable checks block");
    out.push_str("    CONFIG.mode_selection {Advanced} \\\n");
    out.push_str(&format!("    CONFIG.pcie_blk_locn {{{site}}} \\\n"));
    out.push_str("    CONFIG.en_gt_selection {true} \\\n");
    out.push_str(&format!("    CONFIG.select_quad {{{quad}}} \\\n"));
    // PERST# reaches the PCIE3 block through the fabric. With
    // `dedicate_perst` on, the IP grows three MCAP reset ports, and its default
    // differs by device (on for the KU040, off for the VU440). Off everywhere,
    // one wrapper fits every device.
    if block == PcieBlock::Pcie3 {
        out.push_str("    CONFIG.dedicate_perst {false} \\\n");
    }
    out.push_str(&format!("] [get_ips {ip}_0]\n"));
    out
}

/// Vivado takes the BAR size as a unit and a count. The manifest already
/// checks that it is a power of two and at least `MIN_BAR_BYTES`.
fn bar_scale(bytes: u32) -> (&'static str, u32) {
    if bytes >= 1024 * 1024 {
        ("Megabytes", bytes / (1024 * 1024))
    } else {
        ("Kilobytes", bytes / 1024)
    }
}

/// `hns/syn/mmcm.tcl`. It does not pass M/D/O; Vivado solves them.
pub fn mmcm_tcl(plan: &ClockPlan) -> String {
    let mut out = String::new();
    out.push_str(&header_tcl());
    out.push_str(&format!(
        "#\n# Input: {} = {} MHz{}\n",
        plan.input.name,
        plan.input.freq_mhz,
        if plan.input.diff {
            " (differential)"
        } else {
            ""
        }
    ));
    out.push_str("# Only the requested output frequencies are passed; Vivado solves M/D/O.\n");
    out.push_str(
        "# The device limits (VCO range and friends) live in the speed files, so a copy\n",
    );
    out.push_str("# inside the tool could not be verified.\n\n");

    out.push_str(&format!(
        "create_ip -vendor xilinx.com -library ip -name clk_wiz -module_name {MMCM_MODULE} -dir $ip_dir -force\n"
    ));
    out.push_str("set_property -dict [list \\\n");
    out.push_str("    CONFIG.PRIMITIVE {MMCM} \\\n");
    out.push_str(&format!(
        "    CONFIG.RESET_TYPE {{{}}} \\\n",
        if plan.reset.active_low {
            "ACTIVE_LOW"
        } else {
            "ACTIVE_HIGH"
        }
    ));
    if plan.input.diff {
        out.push_str("    CONFIG.PRIM_SOURCE {Differential_clock_capable_pin} \\\n");
    }
    out.push_str(&format!(
        "    CONFIG.PRIM_IN_FREQ {{{:.3}}} \\\n",
        plan.input.freq_mhz
    ));
    for (index, output) in plan.outputs.iter().enumerate() {
        let n = index + 1;
        out.push_str(&format!("    CONFIG.CLKOUT{n}_USED {{true}} \\\n"));
        out.push_str(&format!(
            "    CONFIG.CLKOUT{n}_REQUESTED_OUT_FREQ {{{:.3}}} \\\n",
            output.freq_mhz
        ));
    }
    out.push_str(&format!("] [get_ips {MMCM_MODULE}]\n"));
    out
}

/// The name of the signal that faces a DUT port. It follows the lint prefix
/// rules, so a project with `[lint.naming] prefix_port_*` accepts it.
fn faced(prefixes: &DirectionPrefixes, port: &crate::dut::Port, to_dut: bool) -> String {
    let stripped = prefixes.strip(&port.name, port.direction);
    let dir = if to_dut {
        PortDirection::Output
    } else {
        PortDirection::Input
    };
    let prefix = match dir {
        PortDirection::Output => prefixes.output.as_deref().unwrap_or("o"),
        _ => prefixes.input.as_deref().unwrap_or("i"),
    };
    format!("{prefix}_dut_{stripped}")
}

/// The name of the signal that faces a terminator register. It is not a DUT
/// port, so it is not `faced`. The name already holds the bundle name
/// (`uart_tx_level`), so only the prefix is added.
fn faced_terminator(prefixes: &DirectionPrefixes, name: &str, from_csr: bool) -> String {
    let prefix = if from_csr {
        prefixes.output.as_deref().unwrap_or("o")
    } else {
        prefixes.input.as_deref().unwrap_or("i")
    };
    format!("{prefix}_{name}")
}

/// The asserted value of a reset. This is the one place that maps polarity.
fn asserted(reset: ResetType) -> u8 {
    match reset {
        ResetType::AsyncLow | ResetType::SyncLow => 0,
        ResetType::AsyncHigh | ResetType::SyncHigh => 1,
    }
}

fn reset_type_name(reset: ResetType) -> &'static str {
    match reset {
        ResetType::AsyncLow => "reset_async_low",
        ResetType::AsyncHigh => "reset_async_high",
        ResetType::SyncLow => "reset_sync_low",
        ResetType::SyncHigh => "reset_sync_high",
    }
}

/// A `width`-bit type. One bit is written `logic`, not `logic<1>`, to match
/// the other terminator wires.
fn logic(width: usize) -> String {
    if width == 1 {
        "logic".to_string()
    } else {
        format!("logic<{width}>")
    }
}

/// The registers that let the card issue requests (the harness DMA engine).
///
/// These are the only terminator registers with no bundle: the DMA engine is
/// part of the harness, not of the DUT.
fn dma_registers(map: &crate::regmap::RegisterMap) -> Vec<&crate::regmap::Register> {
    // Match by role prefix. The DUT reset (`dut_reset`) also has no bundle.
    map.registers
        .iter()
        .filter(|r| {
            r.kind == Kind::Terminator
                && r.bundle.is_none()
                && r.role.is_some_and(|role| role.starts_with("dma_"))
        })
        .collect()
}

/// The wires between the gate and the DMA engine. The widths match the ports
/// of `hns::dma_gate` one to one.
///
/// There is one set of descriptor fields, and a handshake and status per
/// direction. Only one transfer runs at a time, so the fields are shared.
const DMA_DESC_WIRES: [(&str, usize); 14] = [
    ("dma_mps_used", 3),
    ("dma_mrrs_used", 3),
    ("dma_desc_pcie_addr", 64),
    ("dma_desc_axi_addr", 64),
    ("dma_desc_len", 20),
    ("dma_desc_tag", 8),
    ("dma_wr_valid", 1),
    ("dma_wr_ready", 1),
    ("dma_rd_valid", 1),
    ("dma_rd_ready", 1),
    ("dma_wr_status_valid", 1),
    ("dma_wr_status_error", 4),
    ("dma_rd_status_valid", 1),
    ("dma_rd_status_error", 4),
];

/// The byte width of the AXI master of the borrowed DMA engine. It is fixed at
/// 256 bits: `AXI_DATA_WIDTH != AXIS_PCIE_DATA_WIDTH` is an `$error`.
const DMA_BUS_BYTES: u32 = 32;

/// The domain of the hard block's user clock. It is not the harness clock.
const PCIE_DOMAIN: &str = "pcie";

/// What the PCIe wrapper shows of the completer streams, for
/// `hns::tlp_count`: `o_<name>` on the wrapper, `i_<name>` on the counter.
const TLP_TAP: [(&str, usize); 9] = [
    ("cq_tvalid", 1),
    ("cq_tready", 1),
    ("cq_tlast", 1),
    ("cq_type", 4),
    ("cc_tvalid", 1),
    ("cc_tready", 1),
    ("cc_tlast", 1),
    ("cc_status", 3),
    ("cq_dropped", 1),
];

/// The synchronizer that brings one TLP count to the window clock.
fn tlp_sync_instance(name: &str) -> String {
    format!("u_tlp_{}_sync", name.trim_start_matches("pcie_"))
}

/// Requester stream signals and widths (256-bit setup). They match
/// `i_rq_*` / `o_rq_tready` of `hns_pcie_wrap` one to one.
const RQ_STREAM: [(&str, usize); 6] = [
    ("tdata", 256),
    ("tkeep", 8),
    ("tlast", 1),
    ("tuser", 60),
    ("tvalid", 1),
    ("tready", 1),
];

/// The returning completion stream (`o_rc_*` of `hns_pcie_wrap`). Same shape
/// as RQ, but `tuser` is wider: it carries completion status and error bits.
const RC_STREAM: [(&str, usize); 6] = [
    ("tdata", 256),
    ("tkeep", 8),
    ("tlast", 1),
    ("tuser", 75),
    ("tvalid", 1),
    ("tready", 1),
];

/// An expression that zero-extends a `width`-bit value to a 32-bit word.
fn widen(expr: &str, width: usize) -> String {
    if width >= WORD_BITS {
        expr.to_string()
    } else {
        format!("{{{}'b0, {expr}}}", WORD_BITS - width)
    }
}

/// `hns/src/csr.veryl`: the `reg` terminator.
///
/// It uses the layout from `regmap` as is. The RTL and the host API must come
/// from the same IR, so no offset is computed again here.
pub fn csr_module(plan: &Plan, prefixes: &DirectionPrefixes) -> String {
    let map = &plan.registers;
    let reset = plan.metadata.build.reset_type;
    let words = map.size_bytes() / (WORD_BITS / 8);
    let addr_bits = bits_for(map.size_bytes());
    let word_bits = bits_for(words);

    let mut out = header_veryl();
    out.push_str("///\n/// The register layout is the one regs.json publishes.\n");
    out.push_str("module csr (\n");
    out.push_str("    i_clk: input clock,\n");
    out.push_str(&format!("    i_rst: input {},\n", reset_type_name(reset)));
    out.push_str(&format!("    i_addr: input logic<{addr_bits}>,\n"));
    out.push_str(&format!("    i_wdata: input logic<{WORD_BITS}>,\n"));
    out.push_str("    i_we: input logic,\n");
    out.push_str("    i_re: input logic,\n");
    out.push_str(&format!("    o_rdata: output logic<{WORD_BITS}>,\n"));
    out.push_str("    o_rvalid: output logic,\n");
    // The CSR never stalls. The port exists so that a terminator that does
    // stall can join without changing the internal bus.
    out.push_str("    o_wready: output logic,\n");
    // The window timeout count. `hns::axil` counts; the CSR only makes it
    // readable. A write goes back as a w1c pulse and is not stored.
    out.push_str(&format!("    i_timeouts: input logic<{WORD_BITS}>,\n"));
    out.push_str("    o_clear_timeouts: output logic,\n");

    for register in map.registers.iter().filter(|r| r.kind == Kind::Port) {
        let port = dut_port(&plan.dut, &register.name);
        let to_dut = register.access == Access::ReadWrite;
        let dir = if to_dut { "output" } else { "input" };
        out.push_str(&format!(
            "    {}: {dir} logic<{}>,\n",
            faced(prefixes, port, to_dut),
            register.width
        ));
    }
    // Terminator registers. Constants (depth) get no wire; they are consts
    // inside the CSR.
    for register in map.registers.iter().filter(|r| r.kind == Kind::Terminator) {
        if register.value.is_some() {
            continue;
        }
        // The memory data window writes to the memory and reads what the
        // memory returns, so one register cannot express it.
        if register.role == Some("mdata") {
            let bundle = register
                .bundle
                .as_deref()
                .expect("terminator registers have a bundle");
            out.push_str(&format!(
                "    {}: output logic<{}>,\n",
                faced_terminator(prefixes, &register.name, true),
                register.width
            ));
            out.push_str(&format!(
                "    {}: output logic,\n",
                faced_terminator(prefixes, &format!("{bundle}_mwe"), true)
            ));
            out.push_str(&format!(
                "    {}: input logic<{}>,\n",
                faced_terminator(prefixes, &format!("{bundle}_mrdata"), false),
                register.width
            ));
            continue;
        }
        let from_csr = register.access == Access::ReadWrite;
        let dir = if from_csr { "output" } else { "input" };
        out.push_str(&format!(
            "    {}: {dir} logic<{}>,\n",
            faced_terminator(prefixes, &register.name, from_csr),
            register.width
        ));
    }
    out.push_str(") {\n");

    // Constant registers (magic / hash, the depth of host_poll_fifo).
    for register in map
        .registers
        .iter()
        .filter(|r| r.kind == Kind::Header || r.kind == Kind::Terminator)
    {
        if let Some(value) = register.value {
            out.push_str(&format!(
                "    const {}: logic<{WORD_BITS}> = {WORD_BITS}'h{value:08x};\n",
                register.name.to_uppercase()
            ));
        }
    }
    out.push('\n');

    out.push_str("    // Word index. The low 2 bits of the byte address are dropped.\n");
    out.push_str(&format!(
        "    let word: logic<{word_bits}> = i_addr[{}:2];\n\n",
        addr_bits - 1
    ));

    // The write side (the host writes, the DUT inputs).
    // The identity header is left out. `harness_timeout` is writable (w1c),
    // but it is a clear pulse to `hns::axil`, not a stored value; a `reg_*`
    // for it would have nothing to drive.
    let writable: Vec<_> = map
        .registers
        .iter()
        .filter(|r| r.access == Access::ReadWrite && r.kind != Kind::Header)
        // `dma_go` is not stored. `hns::dma_gate` expects a one-cycle pulse; a
        // held register would issue descriptors after a single write. It is
        // decoded directly, like the `harness_timeout` clear.
        .filter(|r| r.role != Some(DMA_GO))
        .collect();

    // Memory data windows. A wide entry is written through a 32-bit window, so
    // the memory is written only when the top word is written. No entry is
    // ever half written.
    let mem_windows: Vec<(usize, &crate::regmap::Register)> = map
        .registers
        .iter()
        .filter(|r| r.role == Some("mdata"))
        .map(|r| (r.offset / (WORD_BITS / 8) + r.words - 1, r))
        .collect();
    for register in &writable {
        out.push_str(&format!(
            "    var reg_{}: logic<{}>;\n",
            register.name, register.width
        ));
    }
    for (_, window) in &mem_windows {
        let bundle = window
            .bundle
            .as_deref()
            .expect("terminator registers have a bundle");
        out.push_str(&format!("    var mwe_{bundle}: logic;\n"));
    }

    if !writable.is_empty() {
        out.push_str("\n    always_ff {\n        if_reset {\n");
        for register in &writable {
            out.push_str(&format!("            reg_{} = 0;\n", register.name));
        }
        for (_, window) in &mem_windows {
            let bundle = window
                .bundle
                .as_deref()
                .expect("terminator registers have a bundle");
            out.push_str(&format!("            mwe_{bundle} = 0;\n"));
        }
        out.push_str("        } else {\n");

        // One write is exactly one beat. When the other side (ready / ack) is
        // high, the beat is done and the register clears itself.
        //
        // The clear comes before the write. In one always_ff the later
        // assignment wins; in the other order a new host write would be lost.
        for register in &writable {
            let Some(clear) = &register.self_clearing else {
                continue;
            };
            let expr = match &clear.source {
                ClearSource::DutPort(port) => {
                    let other = faced(prefixes, dut_port(&plan.dut, port), false);
                    if clear.invert {
                        format!("~{other}")
                    } else {
                        other
                    }
                }
                // The terminator fill level. Not empty means one entry can be
                // popped, so the beat is done.
                ClearSource::Terminator(name) => faced_terminator(prefixes, name, false),
            };
            match &clear.source {
                ClearSource::DutPort(_) => {
                    out.push_str(&format!(
                        "            // One beat is transferred once {} is high. JTAG takes ms, the DUT ns,\n",
                        clear.role
                    ));
                    out.push_str(
                        "            // so without this the write would keep streaming beats forever.\n",
                    );
                }
                ClearSource::Terminator(_) => {
                    out.push_str(
                        "            // One entry leaves the FIFO as soon as it is not empty. A pop asked for\n",
                    );
                    out.push_str(
                        "            // while empty stays pending and takes the next entry that arrives.\n",
                    );
                }
            }
            out.push_str(&format!(
                "            if reg_{} != 0 && ({expr}) != 0 {{\n",
                register.name
            ));
            out.push_str(&format!("                reg_{} = 0;\n", register.name));
            out.push_str("            }\n");
        }

        // The commit is one cycle after the top word is written. On the same
        // edge the top word of the holding register is not updated yet, and
        // the old value would reach the memory. The address advances on the
        // commit edge, so the write goes to the address before the advance.
        for (index, window) in &mem_windows {
            let bundle = window
                .bundle
                .as_deref()
                .expect("terminator registers have a bundle");
            out.push_str(&format!(
                "            // A write of the last {bundle}_mdata word commits the whole entry one\n"
            ));
            out.push_str(&format!(
                "            // cycle later, then {bundle}_maddr advances. A read never moves it.\n"
            ));
            out.push_str(&format!(
                "            if mwe_{bundle} {{\n                reg_{bundle}_maddr = reg_{bundle}_maddr + 1;\n            }}\n"
            ));
            out.push_str(&format!(
                "            mwe_{bundle} = i_we && word == {index};\n"
            ));
        }

        out.push_str("            if i_we {\n                case word {\n");
        for register in &writable {
            for word in 0..register.words {
                let lo = word * WORD_BITS;
                let hi = ((word + 1) * WORD_BITS).min(register.width);
                let dst = if register.words == 1 {
                    format!("reg_{}", register.name)
                } else {
                    format!("reg_{}[{}:{lo}]", register.name, hi - 1)
                };
                out.push_str(&format!(
                    "                    {}: {dst} = i_wdata[{}:0];\n",
                    register.offset / (WORD_BITS / 8) + word,
                    hi - lo - 1
                ));
            }
        }
        out.push_str("                    default: {}\n                }\n            }\n");
        out.push_str("        }\n    }\n\n");

        for register in &writable {
            let driven = match register.kind {
                Kind::Terminator => faced_terminator(prefixes, &register.name, true),
                _ => faced(prefixes, dut_port(&plan.dut, &register.name), true),
            };
            out.push_str(&format!("    assign {driven} = reg_{};\n", register.name));
        }
        out.push('\n');
    }

    // The write strobes of the memory data windows. `writable` drives the data.
    for (_, window) in &mem_windows {
        let bundle = window
            .bundle
            .as_deref()
            .expect("terminator registers have a bundle");
        out.push_str(&format!(
            "    assign {} = mwe_{bundle};\n",
            faced_terminator(prefixes, &format!("{bundle}_mwe"), true)
        ));
    }
    if !mem_windows.is_empty() {
        out.push('\n');
    }

    // The timeout count clear (w1c). It is a pulse in the write cycle, not a
    // stored value. Writing 0 does not clear, so writing back a read value of
    // 0 is harmless.
    {
        let index = map
            .registers
            .iter()
            .find(|r| r.name == crate::regmap::TIMEOUT_NAME)
            .map(|r| r.offset / (WORD_BITS / 8))
            .expect("the identity header is always placed");
        out.push_str(&format!(
            "    assign o_clear_timeouts = i_we && word == {index} && i_wdata != 0;\n\n"
        ));
    }

    // The pulse that issues one descriptor. Same shape as above. Writing 0
    // does nothing, so writing back the read value (always 0) is harmless.
    if let Some(register) = map.registers.iter().find(|r| r.role == Some(DMA_GO)) {
        let index = register.offset / (WORD_BITS / 8);
        out.push_str(&format!(
            "    assign {} = i_we && word == {index} && i_wdata != 0;\n\n",
            faced_terminator(prefixes, &register.name, true)
        ));
    }

    // The read side.
    out.push_str(&format!("    var rdata: logic<{WORD_BITS}>;\n\n"));
    out.push_str("    always_comb {\n        case word {\n");
    for register in &map.registers {
        for word in 0..register.words {
            let index = register.offset / (WORD_BITS / 8) + word;
            let expr = match register.kind {
                // The identity header is constant, except the timeout count,
                // which comes in on a wire.
                Kind::Header if register.name == crate::regmap::TIMEOUT_NAME => {
                    "i_timeouts".to_string()
                }
                Kind::Header => register.name.to_uppercase(),
                // Terminator registers: constants (depth) are consts, the rest
                // are wires from the FIFO.
                Kind::Terminator if register.value.is_some() => register.name.to_uppercase(),
                // Write-only: it reads 0. A stored-looking value would make the
                // host think a transfer is still running; `dma_busy` says that.
                Kind::Terminator if register.role == Some(DMA_GO) => "0".to_string(),
                Kind::Terminator | Kind::Port => {
                    let source = match (register.kind, register.access) {
                        // The memory window reads the entry at `maddr`.
                        (Kind::Terminator, _) if register.role == Some("mdata") => {
                            let bundle = register
                                .bundle
                                .as_deref()
                                .expect("terminator registers have a bundle");
                            faced_terminator(prefixes, &format!("{bundle}_mrdata"), false)
                        }
                        (_, Access::ReadWrite) => format!("reg_{}", register.name),
                        (Kind::Terminator, _) => faced_terminator(prefixes, &register.name, false),
                        (_, Access::ReadOnly) => {
                            faced(prefixes, dut_port(&plan.dut, &register.name), false)
                        }
                    };
                    let lo = word * WORD_BITS;
                    let hi = ((word + 1) * WORD_BITS).min(register.width);
                    let slice = if register.words == 1 {
                        source
                    } else {
                        format!("{source}[{}:{lo}]", hi - 1)
                    };
                    widen(&slice, hi - lo)
                }
            };
            out.push_str(&format!("            {index}: rdata = {expr};\n"));
        }
    }
    out.push_str("            default: rdata = 0;\n        }\n    }\n\n");

    out.push_str("    // Always takes a write in the cycle it is offered.\n");
    out.push_str("    assign o_wready = 1'b1;\n\n");
    out.push_str("    always_ff {\n        if_reset {\n");
    out.push_str("            o_rdata  = 0;\n            o_rvalid = 0;\n");
    out.push_str("        } else {\n");
    out.push_str("            // One cycle, always. hns::axil waits for this rather\n");
    out.push_str("            // than assuming how long the read takes.\n");
    out.push_str("            o_rvalid = i_re;\n");
    out.push_str("            if i_re {\n                o_rdata = rdata;\n            }\n");
    out.push_str("        }\n    }\n");
    out.push_str("}\n");
    out
}

/// How long the window waits for an answer (wall clock, microseconds).
///
/// It must be well below the smallest PCIe root complex completion timeout
/// (50 us, Range A), and well above the slowest access in the window (a few
/// hundred ns for a calibrated MIG, several times that with a CDC round trip).
/// Too short cuts normal accesses; too long lets the PCIe side time out first.
const WINDOW_TIMEOUT_US: f64 = 10.0;

/// `WINDOW_TIMEOUT_US` in cycles of the harness clock.
///
/// The frequency comes from `[clock] freq_mhz` in the manifest. With no clock
/// (no `--target`, as in `check`) it returns 0, which disables the watchdog.
/// Those paths emit no RTL, so the 0 never reaches a generated file.
pub fn window_timeout_cycles(freq_mhz: Option<f64>) -> u32 {
    // Round up. Rounding down could cut a normal access.
    freq_mhz.map_or(0, |mhz| (mhz * WINDOW_TIMEOUT_US).ceil() as u32)
}

/// AXI4-Lite slave signals and widths. They match the ports of `hns_axil`.
const AXI_SIGNALS: [(&str, usize, bool); 17] = [
    // (name, width, driven by the slave)
    ("awaddr", 32, false),
    ("awvalid", 1, false),
    ("awready", 1, true),
    ("wdata", WORD_BITS, false),
    ("wstrb", WORD_BITS / 8, false),
    ("wvalid", 1, false),
    ("wready", 1, true),
    ("bresp", 2, true),
    ("bvalid", 1, true),
    ("bready", 1, false),
    ("araddr", 32, false),
    ("arvalid", 1, false),
    ("arready", 1, true),
    ("rdata", WORD_BITS, true),
    ("rresp", 2, true),
    ("rvalid", 1, true),
    ("rready", 1, false),
];

/// The maximum number of entries of the `dram` stand-in in simulation.
///
/// Checking the wiring needs no capacity. The real `depth` (256 MB, about 67
/// million words) as an array would not fit in the simulator.
const SIM_DRAM_ENTRIES: u32 = 4096;

/// The `dram` memories that `--target sim` serves from the testbench.
///
/// A Veryl array cannot hold a whole chip, so `$comp::hns_dram` (a sparse
/// memory in Rust) serves them. A component lives only in a test module, so
/// `hns_sim` takes the bus as a port (`saxi_<bundle>`), and the testbench
/// connects the component to it.
fn sim_drams(plan: &Plan) -> Vec<&crate::terminator::AxiMemPlan> {
    if !plan.target().is_some_and(|target| target.is_sim()) {
        return Vec::new();
    }
    plan.axi_mems
        .iter()
        .filter(|m| m.backing == crate::manifest::Backing::Dram)
        .collect()
}

/// `hns/src/sim.veryl`: the top level for simulation.
///
/// It is `hns_top` without the transport and the clock generation. The MMCM
/// and JTAG are Vivado IP (black boxes) and cannot be simulated. The AXI4-Lite
/// side of the window becomes ports, so a testbench can drive it directly.
///
/// The body is the same `core_body` as `hns_top`, so the two cannot diverge.
///
/// With one clock the ports are `i_clk` / `i_rst`. With several, each clock
/// comes in as `i_clk_<ident>` (and its reset as `i_rst_<ident>`), and the
/// DUT reset is synchronized into each domain as `hns_clk` does.
pub fn sim_module(plan: &Plan, prefixes: &DirectionPrefixes) -> String {
    let reset = plan.metadata.build.reset_type;
    let clocks = plan.clocks().filter(|clocks| clocks.outputs.len() > 1);
    // Every port names its domain once there are several clocks.
    let tag = clocks.map_or(String::new(), |clocks| format!("'{} ", clocks.window));

    let mut out = header_veryl();
    out.push_str("///\n/// The harness without its transport: AXI4-Lite straight to the CSR.\n");
    out.push_str("/// A testbench can drive this; hns_top cannot be simulated as it holds the\n");
    out.push_str("/// MMCM and the JTAG-to-AXI master, both vendor black boxes.\n");
    out.push_str("module sim (\n");
    match clocks {
        None => {
            out.push_str("    i_clk: input clock,\n");
            out.push_str(&format!("    i_rst: input {},\n", reset_type_name(reset)));
        }
        Some(clocks) => {
            for output in &clocks.outputs {
                let ident = &output.ident;
                out.push_str(&format!("    i_clk_{ident}: input '{ident} clock,\n"));
            }
            for ident in sim_reset_idents(clocks) {
                out.push_str(&format!(
                    "    i_rst_{ident}: input '{ident} {},\n",
                    reset_type_name(reset)
                ));
            }
        }
    }
    for (signal, width, from_slave) in AXI_SIGNALS {
        let dir = if from_slave { "output" } else { "input" };
        out.push_str(&format!(
            "    {}_{signal}: {dir} {tag}logic<{width}>,\n",
            if from_slave { "o" } else { "i" }
        ));
    }
    for axi_mem in sim_drams(plan) {
        out.push_str(&format!(
            "    saxi_{}: modport {tag}$std::axi4_if::<{}>::master,\n",
            axi_mem.bundle,
            host_pkg(plan, axi_mem).0
        ));
    }
    // The heartbeat is visible in simulation too, so the liveness circuit
    // itself is tested.
    if let Some(heartbeat) = &plan.heartbeat {
        out.push_str(&format!(
            "    o_{}: output {tag}logic,\n",
            heartbeat.resource
        ));
    }
    out.push_str(") {\n");

    // `core_body` expects the bus as `axi_*`; connect it to the ports.
    for (signal, width, from_slave) in AXI_SIGNALS {
        out.push_str(&format!("    var axi_{signal}: {tag}logic<{width}>;\n"));
        if from_slave {
            out.push_str(&format!("    assign o_{signal} = axi_{signal};\n"));
        } else {
            out.push_str(&format!("    assign axi_{signal} = i_{signal};\n"));
        }
    }
    out.push('\n');

    let (clk, rst) = match clocks {
        None => ("i_clk".to_string(), "i_rst".to_string()),
        Some(clocks) => {
            sim_clock_names(&mut out, clocks, reset);
            (
                format!("clk_{}", clocks.window),
                format!("rst_{}", clocks.window),
            )
        }
    };

    if let Some(heartbeat) = &plan.heartbeat {
        out.push_str("    inst u_uart: uart (\n");
        out.push_str(&format!("        i_clk: {clk},\n        i_rst: {rst},\n"));
        out.push_str(&format!("        o_tx : o_{},\n", heartbeat.resource));
        out.push_str("    );\n\n");
    }

    core_body(
        &mut out,
        plan,
        false,
        prefixes,
        clocks,
        Domain {
            clk: &clk,
            rst: &rst,
            tag: &tag,
        },
    );
    out.push_str("}\n");
    out
}

/// The clocks that `hns_sim` takes a reset for: the window (the harness runs
/// on it), and each one a DUT reset belongs to.
fn sim_reset_idents(clocks: &ClockPlan) -> Vec<&str> {
    clocks
        .outputs
        .iter()
        .map(|output| output.ident.as_str())
        .filter(|ident| {
            *ident == clocks.window || clocks.resets.iter().any(|(_, owner)| owner == ident)
        })
        .collect()
}

/// The names `core_body` uses with several clocks (`clk_*`, `rst_*`,
/// `drst_*`, `dut_rst_req`), as `hns_top` declares them. The DUT reset takes
/// the host's request through two flops in each domain, as in `hns_clk`.
fn sim_clock_names(out: &mut String, clocks: &ClockPlan, reset: ResetType) {
    let reset_type = reset_type_name(reset);
    let on = asserted(reset);
    let request = if on == 0 {
        "~dut_rst_req"
    } else {
        "dut_rst_req"
    };
    for output in &clocks.outputs {
        let ident = &output.ident;
        out.push_str(&format!("    var clk_{ident}: '{ident} clock;\n"));
        out.push_str(&format!("    assign clk_{ident} = i_clk_{ident};\n"));
    }
    let resets = sim_reset_idents(clocks);
    for ident in &resets {
        out.push_str(&format!("    var rst_{ident}: '{ident} {reset_type};\n"));
        out.push_str(&format!("    assign rst_{ident} = i_rst_{ident};\n"));
    }
    out.push_str(&format!(
        "    var dut_rst_req: '{} logic;\n\n",
        clocks.window
    ));
    for ident in resets
        .iter()
        .filter(|ident| clocks.resets.iter().any(|(_, owner)| owner == *ident))
    {
        out.push_str(&format!("    var drst_meta_{ident}: '{ident} logic;\n"));
        out.push_str(&format!("    var drst_sync_{ident}: '{ident} logic;\n"));
        out.push_str(&format!("    var drst_{ident}: '{ident} {reset_type};\n"));
        out.push_str("    unsafe (cdc) {\n");
        out.push_str(&format!(
            "        always_ff (clk_{ident}, rst_{ident}) {{\n"
        ));
        out.push_str(&format!(
            "            if_reset {{\n                drst_meta_{ident} = {on};\n                drst_sync_{ident} = {on};\n"
        ));
        out.push_str(&format!(
            "            }} else {{\n                drst_meta_{ident} = {request};\n                drst_sync_{ident} = drst_meta_{ident};\n            }}\n"
        ));
        out.push_str("        }\n    }\n");
        out.push_str(&format!(
            "    assign drst_{ident} = drst_sync_{ident} as {reset_type};\n\n"
        ));
    }
}

/// The source of `$comp::hns_link`, which `--target sim` copies into the
/// output. It is tested as the `hns-sim-link` crate.
const SIM_LINK: &str = include_str!("../crates/sim-link/src/lib.rs");

/// The export name of the component, as `$comp::<name>`.
const SIM_LINK_NAME: &str = "hns_link";

/// The export name of the sparse memory behind `dram`.
const SIM_DRAM_NAME: &str = "hns_dram";

/// The source of `$comp::hns_dram`, a module of the same package.
const SIM_DRAM: &str = include_str!("../crates/sim-link/src/dram.rs");

/// The directory of the component package, under the output.
pub const SIM_LINK_DIR: &str = "link";

/// `sim_tb.veryl`: the testbench `veryl harness sim` runs. It drives
/// `hns_sim` from `$comp::hns_link`, which serves the window over TCP.
///
/// It runs until the component finishes it (or Ctrl-C); the cycle count only
/// has to be more than anyone waits. With several clocks, each has its own
/// `initial` and a period in picoseconds, so they keep their ratio.
pub fn sim_testbench(plan: &Plan) -> String {
    let heartbeat = plan.heartbeat.as_ref().map(|h| h.resource.as_str());
    let clocks = plan.clocks().filter(|clocks| clocks.outputs.len() > 1);
    let mut out = header_veryl();
    out.push_str("///\n/// Serves the harness window to hio over TCP (`veryl harness sim`).\n");
    out.push_str("#[test(sim)]\nmodule sim_tb {\n");
    let window = match clocks {
        None => {
            out.push_str("    inst clk: $tb::clock_gen;\n");
            out.push_str("    inst rst: $tb::reset_gen (clk);\n\n");
            "clk".to_string()
        }
        Some(clocks) => {
            for output in &clocks.outputs {
                let period = (1.0e6 / output.freq_mhz).round().max(2.0) as u64;
                out.push_str(&format!(
                    "    // {} MHz\n    inst clk_{}: $tb::clock_gen #(\n        period: {period},\n    );\n",
                    output.freq_mhz, output.ident
                ));
            }
            for ident in sim_reset_idents(clocks) {
                out.push_str(&format!(
                    "    inst rst_{ident}: $tb::reset_gen (\n        clk: clk_{ident},\n    );\n"
                ));
            }
            out.push('\n');
            format!("clk_{}", clocks.window)
        }
    };
    for (signal, width, _) in AXI_SIGNALS {
        out.push_str(&format!("    var {signal}: logic<{width}>;\n"));
    }
    if let Some(tx) = heartbeat {
        out.push_str(&format!("    var {tx}: logic;\n"));
    }
    let drams = sim_drams(plan);
    for axi_mem in &drams {
        out.push_str(&format!(
            "    inst dram_{}: $std::axi4_if::<{}>;\n",
            axi_mem.bundle,
            host_pkg(plan, axi_mem).0
        ));
    }
    out.push_str("\n    inst u_sim: sim (\n");
    match clocks {
        None => out.push_str("        i_clk: clk,\n        i_rst: rst,\n"),
        Some(clocks) => {
            for output in &clocks.outputs {
                let ident = &output.ident;
                out.push_str(&format!("        i_clk_{ident}: clk_{ident},\n"));
            }
            for ident in sim_reset_idents(clocks) {
                out.push_str(&format!("        i_rst_{ident}: rst_{ident},\n"));
            }
        }
    }
    for (signal, _, from_slave) in AXI_SIGNALS {
        let dir = if from_slave { "o" } else { "i" };
        out.push_str(&format!("        {dir}_{signal}: {signal},\n"));
    }
    if let Some(tx) = heartbeat {
        out.push_str(&format!("        o_{tx}: {tx},\n"));
    }
    for axi_mem in &drams {
        let bundle = &axi_mem.bundle;
        out.push_str(&format!("        saxi_{bundle}: dram_{bundle},\n"));
    }
    out.push_str("    );\n\n");
    // The component's ports are matched by name, so `clk` is named when the
    // window clock is not called that.
    let link_clk = if window == "clk" {
        window
    } else {
        format!("clk: {window}")
    };
    out.push_str(&format!(
        "    inst u_link: $comp::{SIM_LINK_NAME} (\n        {link_clk},\n"
    ));
    for (signal, _, _) in AXI_SIGNALS {
        out.push_str(&format!("        {signal},\n"));
    }
    out.push_str("    );\n\n");
    // The memory of each `dram`, on the window clock as the stand-in is.
    for axi_mem in &drams {
        let bundle = &axi_mem.bundle;
        out.push_str(&format!(
            "    inst u_dram_{bundle}: $comp::{SIM_DRAM_NAME} (\n        {link_clk},\n        axi: dram_{bundle}.slave,\n    );\n\n"
        ));
    }
    match clocks {
        None => out.push_str(
            "    initial {\n        rst.assert();\n        clk.next(1000000000000);\n        $finish();\n    }\n",
        ),
        Some(clocks) => {
            // The window's block ends the run. The others only have to
            // outlast it, or their clock would stop first.
            let resets = sim_reset_idents(clocks);
            for output in &clocks.outputs {
                let ident = &output.ident;
                out.push_str("    initial {\n");
                if resets.contains(&ident.as_str()) {
                    out.push_str(&format!("        rst_{ident}.assert();\n"));
                }
                if *ident == clocks.window {
                    out.push_str(&format!(
                        "        clk_{ident}.next(1000000000000);\n        $finish();\n"
                    ));
                } else {
                    out.push_str(&format!(
                        "        clk_{ident}.next(1000000000000000000);\n"
                    ));
                }
                out.push_str("    }\n");
            }
        }
    }
    out.push_str("}\n");
    out
}

/// `link/Cargo.toml`: the package of `$comp::hns_link`. The empty
/// `[workspace]` keeps it out of a cargo workspace around the project.
pub fn sim_link_cargo_toml() -> String {
    let mut out = header_tcl();
    out.push_str(&format!("\n[package]\nname    = \"{SIM_LINK_NAME}\"\n"));
    out.push_str("version = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n");
    out.push_str("[lib]\ncrate-type = [\"cdylib\"]\n\n");
    out.push_str("[dependencies]\nveryl-component = \"=0.1.2\"\n\n");
    out.push_str("[workspace]\n");
    out
}

/// `link/.cargo/config.toml`: builds the component for the machine that runs
/// the simulation. Veryl runs cargo in `link/`, so a project's own cargo
/// config is read too, and a firmware project names its CPU there
/// (`[build] target = "riscv32im-unknown-none-elf"`), where no cdylib can be
/// built. A config nearer the package wins.
pub fn sim_link_cargo_config() -> String {
    let mut out = header_tcl();
    out.push_str(
        "
[build]
target = \"host-tuple\"
",
    );
    out
}

/// `link/veryl.manifest.json`: the component's ports, so that Veryl can
/// analyze the testbench before cargo has built the component. The crate's
/// tests keep it equal to what the library exports.
pub fn sim_link_manifest() -> String {
    let manifest = include_str!("../crates/sim-link/veryl.manifest.json").trim_end();
    let rest = manifest.strip_prefix('{').expect("a JSON object");
    format!("{{\"marker\":\"{MARKER}\",{rest}\n")
}

/// `link/src/lib.rs`: the component source, behind the marker so that `gen`
/// may rewrite and remove it.
pub fn sim_link_source() -> String {
    copied_source(SIM_LINK)
}

/// `link/src/dram.rs`: the module of `$comp::hns_dram`, as `sim_link_source`.
pub fn sim_dram_source() -> String {
    copied_source(SIM_DRAM)
}

fn copied_source(source: &str) -> String {
    format!(
        "// {MARKER}\n//\n// Copied by veryl-harness. DO NOT EDIT -- `veryl harness gen` rewrites this file.\n\n{source}"
    )
}

/// Whether the plan reaches the window over PCIe. JTAG is always kept, so
/// this means "the window gets a second master".
pub fn has_pcie(plan: &Plan) -> bool {
    plan.feasibility().is_some_and(|f| f.transport == "pcie")
}

/// Whether the board reset and PERST# are the same pin. The top then has one
/// port for both, and the harness takes its reset from PERST#.
pub fn perst_is_board_reset(plan: &Plan) -> bool {
    let board_pin = &plan.board().clocks.reset.pin;
    has_pcie(plan)
        && board_pin.is_some()
        && plan.target().and_then(|t| hns_targets::pcie(t).reset_n) == *board_pin
}

/// Whether PERST# also resets the DUT (`[pcie] perst_resets_dut`).
fn perst_resets_dut(plan: &Plan) -> bool {
    has_pcie(plan)
        && plan
            .loaded
            .manifest
            .pcie
            .as_ref()
            .is_none_or(|pcie| pcie.perst_resets_dut)
}

/// The clock that the body runs on. The three fields always change together.
struct Domain<'a> {
    clk: &'a str,
    rst: &'a str,
    /// The Veryl domain annotation (`'c0 ` and so on). Empty in `hns_sim`.
    tag: &'a str,
}

/// AXI4-Lite signals and widths of the window. They are outside `core_body`
/// because the PCIe wrapper drives them and is instantiated before it.
const AXI_LITE_WIDTH: [(&str, u32); 17] = [
    ("awaddr", 32),
    ("awvalid", 1),
    ("awready", 1),
    ("wdata", 32),
    ("wstrb", 4),
    ("wvalid", 1),
    ("wready", 1),
    ("bresp", 2),
    ("bvalid", 1),
    ("bready", 1),
    ("araddr", 32),
    ("arvalid", 1),
    ("arready", 1),
    ("rdata", 32),
    ("rresp", 2),
    ("rvalid", 1),
    ("rready", 1),
];

/// The body of the top level, from the AXI4-Lite slave down to the DUT.
///
/// `hns_top` and `hns_sim` share it. They differ only in where AXI comes from
/// (JTAG or a testbench); everything below must be the same, so it comes
/// from the same code.
///
/// `d.tag` is the clock domain annotation (`"'c0 "` or empty). `hns_sim` has
/// no MMCM, so it has no domain to name.
fn core_body(
    out: &mut String,
    plan: &Plan,
    // Whether the second window master (the PCIe BAR) is driven from outside.
    // `hns_sim` has no transport, so it passes false even for a PCIe design.
    second_master: bool,
    prefixes: &DirectionPrefixes,
    clocks: Option<&ClockPlan>,
    d: Domain<'_>,
) {
    let (clk, rst, domain) = (d.clk, d.rst, d.tag);
    let map = &plan.registers;
    let addr_bits = bits_for(map.size_bytes());

    out.push_str(&format!("    var bus_addr: {domain}logic<{addr_bits}>;\n"));
    out.push_str(&format!("    var bus_wdata: {domain}logic<{WORD_BITS}>;\n"));
    out.push_str(&format!("    var bus_we: {domain}logic;\n"));
    out.push_str(&format!("    var bus_re: {domain}logic;\n"));
    out.push_str(&format!(
        "    var bus_rdata: {domain}logic<{WORD_BITS}>;\n    var bus_rvalid: {domain}logic;\n    var bus_wready: {domain}logic;\n    var bus_timeouts: {domain}logic<{WORD_BITS}>;\n    var bus_clear_timeouts: {domain}logic;\n"
    ));
    // High while the DUT is being reset (`hns::reset_ctl`). Declared here
    // because the parts that drive the DUT's slaves use it before the reset
    // controller is emitted.
    out.push_str(&format!("    var dut_fence: {domain}logic;\n\n"));

    // The window can have two masters: JTAG (or the testbench) and the PCIe
    // BAR. The arbiter is always present, so adding PCIe changes nothing else.
    // An absent second master is tied to 0, never left open.
    const AXI_SIGNAL_DIR: [(&str, &str); 17] = [
        ("awaddr", "i"),
        ("awvalid", "i"),
        ("awready", "o"),
        ("wdata", "i"),
        ("wstrb", "i"),
        ("wvalid", "i"),
        ("wready", "o"),
        ("bresp", "o"),
        ("bvalid", "o"),
        ("bready", "i"),
        ("araddr", "i"),
        ("arvalid", "i"),
        ("arready", "o"),
        ("rdata", "o"),
        ("rresp", "o"),
        ("rvalid", "o"),
        ("rready", "i"),
    ];
    const AXI_WIDTH: [(&str, u32); 17] = AXI_LITE_WIDTH;

    for (signal, width) in AXI_WIDTH {
        out.push_str(&format!("    var arb_{signal}: {domain}logic<{width}>;\n"));
    }
    // The `bar_*` wires are declared by `top_module`, because the wrapper is
    // instantiated first.
    let second = second_master;
    out.push('\n');

    // Which master was granted. A paged window needs it to pick its base.
    // Reads and writes are arbitrated separately, so there are two.
    let paged_window = map.regions.iter().any(|region| region.is_aperture());
    if paged_window {
        out.push_str(&format!("    var bus_wmaster: {domain}logic;\n"));
        out.push_str(&format!("    var bus_rmaster: {domain}logic;\n\n"));
    }

    out.push_str("    inst u_rr: hns::axil_rr #(\n        ADDR_W: 32,\n    ) (\n");
    out.push_str(&format!("        i_clk: {clk},\n"));
    out.push_str(&format!("        i_rst: {rst},\n"));
    for (signal, dir) in AXI_SIGNAL_DIR {
        // On the master side of the arbiter the directions are reversed.
        let m = if dir == "i" { "i" } else { "o" };
        out.push_str(&format!("        {m}_m0_{signal}: axi_{signal},\n"));
    }
    for (signal, dir) in AXI_SIGNAL_DIR {
        let width = AXI_WIDTH
            .iter()
            .find(|(name, _)| *name == signal)
            .map(|(_, w)| *w)
            .unwrap_or(1);
        let m = if dir == "i" { "i" } else { "o" };
        if second {
            out.push_str(&format!("        {m}_m1_{signal}: bar_{signal},\n"));
        } else if dir == "i" {
            out.push_str(&format!("        i_m1_{signal}: {width}'b0,\n"));
        } else {
            out.push_str(&format!("        o_m1_{signal}: _,\n"));
        }
    }
    for (signal, dir) in AXI_SIGNAL_DIR {
        // On the slave side the arbiter's directions are used as they are.
        let s = if dir == "i" { "o" } else { "i" };
        out.push_str(&format!("        {s}_s_{signal}: arb_{signal},\n"));
    }
    if paged_window {
        out.push_str("        o_s_wmaster: bus_wmaster,\n");
        out.push_str("        o_s_rmaster: bus_rmaster,\n");
    } else {
        out.push_str("        o_s_wmaster: _,\n");
        out.push_str("        o_s_rmaster: _,\n");
    }
    out.push_str("    );\n\n");

    // Fixed-shape parts come from the `hns` package; only the wiring is generated.
    let timeout =
        window_timeout_cycles(plan.clocks().map(|clocks| clocks.window_output().freq_mhz));
    out.push_str(&format!(
        "    inst u_axil: hns::axil #(\n        ADDR_BITS: {addr_bits},\n        TIMEOUT: {timeout},\n    ) (\n"
    ));
    out.push_str(&format!("        i_clk: {clk},\n"));
    out.push_str(&format!("        i_rst: {rst},\n"));
    for (signal, dir) in AXI_SIGNAL_DIR {
        out.push_str(&format!("        {dir}_{signal}: arb_{signal},\n"));
    }
    out.push_str("        o_addr: bus_addr,\n");
    out.push_str("        o_wdata: bus_wdata,\n");
    out.push_str("        o_we: bus_we,\n");
    out.push_str("        o_re: bus_re,\n");
    out.push_str("        i_rdata: bus_rdata,\n");
    out.push_str("        i_rvalid: bus_rvalid,\n");
    out.push_str("        i_wready: bus_wready,\n");
    out.push_str("        i_clear: bus_clear_timeouts,\n");
    out.push_str("        o_timeouts: bus_timeouts,\n");
    out.push_str("    );\n\n");

    // Wires between the CSR and the DUT.
    for register in map.registers.iter().filter(|r| r.kind == Kind::Port) {
        out.push_str(&format!(
            "    var w_{}: {domain}logic<{}>;\n",
            register.name, register.width
        ));
    }

    // The interrupt line. The CSR shows its level; on PCIe, `top_module`
    // also sends it to the hard block.
    for irq in &plan.irqs {
        out.push_str(&format!("    var w_{}: {domain}logic;\n", irq.port));
        out.push_str(&format!(
            "    var t_{}_irq_level: {domain}logic;\n",
            irq.bundle
        ));
        out.push_str(&format!(
            "    assign t_{}_irq_level = w_{};\n",
            irq.bundle, irq.port
        ));
    }

    // The host's FLR. `hns::flr_ctl` answers it for the DUT; on PCIe,
    // `top_module` connects it to the hard block. `hns_sim` has none, so
    // there the request stays low.
    for flr in &plan.flrs {
        out.push_str(&format!("    var w_{}: {domain}logic;\n", flr.port));
        if let Some(done) = &flr.done {
            out.push_str(&format!("    var w_{done}: {domain}logic;\n"));
        }
        let (req, ack) = if second_master {
            ("pcie_flr", "pcie_flr_ack")
        } else {
            ("0", "_")
        };
        let (use_done, done) = match &flr.done {
            Some(done) => ("true", format!("w_{done}")),
            None => ("false", "0".to_string()),
        };
        out.push_str(&format!(
            "    inst u_flr_{}: hns::flr_ctl #(\n        USE_DONE: {use_done},\n    ) (\n",
            flr.bundle
        ));
        out.push_str(&format!("        i_clk : {clk},\n"));
        out.push_str(&format!("        i_rst : {rst},\n"));
        out.push_str(&format!("        i_req : {req},\n"));
        out.push_str(&format!("        i_done: {done},\n"));
        out.push_str(&format!("        o_flr : w_{},\n", flr.port));
        out.push_str(&format!("        o_ack : {ack},\n"));
        out.push_str("    );\n\n");
    }

    // host_poll_fifo wires: `w_<port>` on the DUT side (the DUT connection
    // uses these names), `t_<bundle>_*` on the terminator side.
    for fifo in &plan.fifos {
        out.push_str(&format!("    var w_{}: {domain}logic;\n", fifo.valid));
        out.push_str(&format!(
            "    var w_{}: {domain}logic<{}>;\n",
            fifo.data, fifo.width
        ));
        if let Some(ready) = &fifo.ready {
            out.push_str(&format!("    var w_{}: {domain}logic;\n", ready.port));
            out.push_str(&format!(
                "    var t_{}_ready: {domain}logic;\n",
                fifo.bundle
            ));
        }
        out.push_str(&format!(
            "    var t_{}_data: {domain}logic<{}>;\n",
            fifo.bundle, fifo.width
        ));
        out.push_str(&format!(
            "    var t_{}_level: {domain}logic<{}>;\n",
            fifo.bundle,
            fifo.level_width()
        ));
        if fifo.counts_drops() {
            out.push_str(&format!(
                "    var t_{}_drops: {domain}logic<{WORD_BITS}>;\n",
                fifo.bundle
            ));
        }
        out.push_str(&format!("    var t_{}_pop: {domain}logic;\n", fifo.bundle));
    }

    // trace wires: `w_<port>` on the DUT side, `t_<bundle>_*` on the host side.
    for trace in &plan.traces {
        let bundle = &trace.bundle;
        out.push_str(&format!("    var w_{}: {domain}logic;\n", trace.valid));
        for field in &trace.fields {
            out.push_str(&format!(
                "    var w_{}: {domain}{};\n",
                field.port,
                logic(field.width)
            ));
        }
        out.push_str(&format!("    var t_{bundle}_run: {domain}logic;\n"));
        out.push_str(&format!("    var t_{bundle}_pause: {domain}logic;\n"));
        out.push_str(&format!(
            "    var t_{bundle}_count: {domain}logic<{WORD_BITS}>;\n"
        ));
        out.push_str(&format!("    var t_{bundle}_wrapped: {domain}logic;\n"));
        out.push_str(&format!("    var t_{bundle}_arm: {domain}logic;\n"));
        out.push_str(&format!(
            "    var t_{bundle}_after: {domain}logic<{WORD_BITS}>;\n"
        ));
        let twidth = trace.trigger_width();
        if twidth > 0 {
            out.push_str(&format!(
                "    var t_{bundle}_tmask: {domain}{};\n",
                logic(twidth)
            ));
            out.push_str(&format!(
                "    var t_{bundle}_tvalue: {domain}{};\n",
                logic(twidth)
            ));
        }
        out.push_str(&format!("    var t_{bundle}_hit: {domain}logic;\n"));
        out.push_str(&format!(
            "    var t_{bundle}_hit_at: {domain}logic<{WORD_BITS}>;\n"
        ));
        out.push_str(&format!(
            "    var t_{bundle}_maddr: {domain}logic<{}>;\n",
            trace.depth.trailing_zeros()
        ));
        out.push_str(&format!(
            "    var t_{bundle}_hrdata: {domain}logic<{}>;\n",
            trace.record_width()
        ));
        out.push_str(&format!(
            "    var t_{bundle}_mrdata: {domain}logic<{}>;\n",
            trace.entry_words() * WORD_BITS
        ));
    }

    // bram / bram_preload wires: `w_<port>` on the DUT side, `t_<bundle>_m*`
    // on the host side.
    for mem in &plan.memories {
        out.push_str(&format!(
            "    var w_{}: {domain}logic<{}>;\n",
            mem.addr, mem.addr_width
        ));
        out.push_str(&format!(
            "    var w_{}: {domain}logic<{}>;\n",
            mem.rdata, mem.width
        ));
        if let Some(enable) = &mem.enable {
            out.push_str(&format!("    var w_{}: {domain}logic;\n", enable.port));
        }
        if let Some(write) = &mem.write {
            out.push_str(&format!(
                "    var w_{}: {domain}logic<{}>;\n",
                write.wdata, mem.entry_width
            ));
            out.push_str(&format!("    var w_{}: {domain}logic;\n", write.we));
            if let Some(wstrb) = &write.wstrb {
                out.push_str(&format!(
                    "    var w_{wstrb}: {domain}logic<{}>;\n",
                    mem.entry_width / 8
                ));
            }
        }
        out.push_str(&format!(
            "    var t_{}_maddr: {domain}logic<{}>;\n",
            mem.bundle,
            mem.host_addr_width()
        ));
        out.push_str(&format!(
            "    var t_{}_mdata: {domain}logic<{}>;\n",
            mem.bundle, mem.entry_width
        ));
        out.push_str(&format!("    var t_{}_mwe: {domain}logic;\n", mem.bundle));
        out.push_str(&format!(
            "    var t_{}_mrdata: {domain}logic<{}>;\n",
            mem.bundle, mem.entry_width
        ));
        if mem.counts_out_of_range() {
            out.push_str(&format!(
                "    var t_{}_oor: {domain}logic<{WORD_BITS}>;\n",
                mem.bundle
            ));
        }
    }

    // axi_mem. The DUT connects through the AXI4 interface itself. The host
    // reaches the same memory through its own master and the arbiter, not a
    // back door, because a real memory controller has no back door.
    for axi_mem in &plan.axi_mems {
        let bundle = &axi_mem.bundle;
        out.push_str(&format!(
            "    var t_{bundle}_maddr: {domain}logic<{}>;\n",
            axi_mem.word_addr_width()
        ));
        out.push_str(&format!(
            "    var t_{bundle}_mdata: {domain}logic<{WORD_BITS}>;\n"
        ));
        out.push_str(&format!("    var t_{bundle}_mwe: {domain}logic;\n"));
        out.push_str(&format!("    var t_{bundle}_mre: {domain}logic;\n"));
        out.push_str(&format!(
            "    var t_{bundle}_mrdata: {domain}logic<{WORD_BITS}>;\n"
        ));
        out.push_str(&format!("    var t_{bundle}_mrvalid: {domain}logic;\n"));
        out.push_str(&format!("    var t_{bundle}_mwready: {domain}logic;\n"));
        if axi_mem.backing == crate::manifest::Backing::Dram {
            out.push_str(&format!("    var t_{bundle}_calib: {domain}logic;\n"));
        }
        if axi_mem.dut_base != 0 || axi_mem.xfer.is_some() {
            out.push_str(&format!(
                "    var t_{bundle}_oor: {domain}logic<{WORD_BITS}>;\n"
            ));
        }
        // A transfer-level port: the DUT side of `hns::xfer2axi`.
        if let Some(xfer) = &axi_mem.xfer {
            for (port, width) in xfer_wires(xfer) {
                out.push_str(&format!("    var w_{port}: {domain}logic<{width}>;\n"));
            }
        }
        // A pzcorebus port: the DUT side of `hns::corebus2axi`.
        if let Some(corebus) = &axi_mem.corebus {
            out.push_str(&format!(
                "    var t_{bundle}_refused: {domain}logic<{WORD_BITS}>;\n"
            ));
            out.push_str(&format!(
                "    inst cb_{bundle}: {domain}{};\n",
                corebus.iface
            ));
            for (name, width) in [
                ("mcmd", "4"),
                ("burst", "16"),
                ("byteen", &format!("{}", axi_mem.data_bytes)),
                ("rdata", "1"),
                ("rlast", "1"),
            ] {
                let width = if width == "1" {
                    String::new()
                } else {
                    format!("<{width}>")
                };
                out.push_str(&format!(
                    "    var cbw_{bundle}_{name}: {domain}logic{width};\n"
                ));
            }
        }
    }

    // An AXI4 slave of the DUT: the window drives it through `hns::axi_host`,
    // which says itself when it answers and when it takes a write.
    for axi in &plan.axi_slaves {
        let bundle = &axi.bundle;
        out.push_str(&format!(
            "    var t_{bundle}_maddr: {domain}logic<{}>;\n",
            axi.word_addr_width()
        ));
        out.push_str(&format!(
            "    var t_{bundle}_mdata: {domain}logic<{WORD_BITS}>;\n"
        ));
        out.push_str(&format!("    var t_{bundle}_mwe: {domain}logic;\n"));
        out.push_str(&format!("    var t_{bundle}_mre: {domain}logic;\n"));
        out.push_str(&format!(
            "    var t_{bundle}_mrdata: {domain}logic<{WORD_BITS}>;\n"
        ));
        out.push_str(&format!("    var t_{bundle}_mrvalid: {domain}logic;\n"));
        out.push_str(&format!("    var t_{bundle}_mwready: {domain}logic;\n"));
        if let Some(corebus) = &axi.corebus {
            emit_corebus_host(out, axi, corebus, domain, clk, rst);
            continue;
        }
        out.push_str(&format!(
            "    inst axi_{bundle}: {domain}$std::axi4_if::<{}>;\n\n",
            axi.pkg
        ));
        out.push_str(&format!(
            "    inst u_haxi_{bundle}: hns::axi_host::<{}> #(\n",
            axi.pkg
        ));
        out.push_str(&format!("        ADDR_W: {},\n", axi.word_addr_width()));
        out.push_str("    ) (\n");
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        // A DUT slave may stop answering when it is reset.
        out.push_str("        i_hold: dut_fence,\n");
        out.push_str(&format!("        axi: axi_{bundle},\n"));
        out.push_str(&format!("        i_haddr: t_{bundle}_maddr,\n"));
        out.push_str(&format!("        i_hwdata: t_{bundle}_mdata,\n"));
        out.push_str(&format!("        i_hwe: t_{bundle}_mwe,\n"));
        out.push_str(&format!("        i_hre: t_{bundle}_mre,\n"));
        out.push_str(&format!("        o_hrdata: t_{bundle}_mrdata,\n"));
        out.push_str(&format!("        o_hrvalid: t_{bundle}_mrvalid,\n"));
        out.push_str(&format!("        o_hwready: t_{bundle}_mwready,\n"));
        out.push_str("    );\n\n");
    }
    for axi_mem in &plan.axi_mems {
        // State the clock domain. An interface instance has no domain of its
        // own, so without it both the DUT and the stand-in look like a
        // crossing with an unknown domain.
        let bundle = &axi_mem.bundle;
        out.push_str(&format!(
            "    inst axi_{bundle}: {domain}$std::axi4_if::<{}>;\n",
            axi_mem.pkg
        ));
        // The output of the DUT reset fence. Everything downstream uses it.
        out.push_str(&format!(
            "    inst faxi_{bundle}: {domain}$std::axi4_if::<{}>;\n",
            axi_mem.pkg
        ));
        // From the arbiter on, the address width is the controller's. Only the
        // DUT may be narrower, so it is widened into `xaxi_` first.
        let (hpkg, widen) = host_pkg(plan, axi_mem);
        if widen {
            out.push_str(&format!(
                "    inst xaxi_{bundle}: {domain}$std::axi4_if::<{hpkg}>;\n"
            ));
        }
        // The output of `hns::axi_range`: inside the memory, base taken off.
        if axi_mem.dut_base != 0 {
            out.push_str(&format!(
                "    inst raxi_{bundle}: {domain}$std::axi4_if::<{hpkg}>;\n"
            ));
        }
        out.push_str(&format!(
            "    inst haxi_{bundle}: {domain}$std::axi4_if::<{hpkg}>;\n"
        ));
        // A port of `hns_sim` for a `dram` served by the testbench.
        if !sim_drams(plan).iter().any(|m| m.bundle == *bundle) {
            out.push_str(&format!(
                "    inst saxi_{bundle}: {domain}$std::axi4_if::<{hpkg}>;\n"
            ));
        }
    }
    if !plan.axi_mems.is_empty() {
        out.push('\n');
    }

    // The DUT reset: wires between the CSR and `hns::reset_ctl`.
    out.push_str(&format!("    var t_{DUT_RESET}: {domain}logic;\n"));
    out.push_str(&format!("    var t_{DUT_RESET_STATE}: {domain}logic;\n"));
    // In `hns_top`, `hns_clk` is instantiated first and uses this wire, so it
    // is declared there.
    if clocks.is_none() {
        out.push_str(&format!("    var dut_rst_req: {domain}logic;\n"));
    }
    out.push('\n');

    // The card-issued DMA: wires between the CSR and `hns::dma_gate`.
    for register in dma_registers(map) {
        out.push_str(&format!(
            "    var t_{}: {domain}{};\n",
            register.name,
            logic(register.width)
        ));
    }
    if !dma_registers(map).is_empty() {
        out.push('\n');
    }

    // The TLP counts, from the PCIe wrapper to the CSR.
    let tlp_counts = map
        .registers
        .iter()
        .filter(|r| {
            crate::regmap::PCIE_TLP_COUNTERS
                .iter()
                .any(|(name, _)| r.role == Some(*name))
        })
        .collect::<Vec<_>>();
    for register in &tlp_counts {
        out.push_str(&format!(
            "    var t_{}: {domain}{};\n",
            register.name,
            logic(register.width)
        ));
    }
    if !tlp_counts.is_empty() {
        out.push('\n');
    }

    // The host_mem stand-in: the DUT interface, and the indirect port the host
    // uses.
    for hmem in &plan.host_mems {
        let bundle = &hmem.bundle;
        let mut wires: Vec<(String, usize)> = Vec::new();
        if let Some(read) = &hmem.read {
            wires.extend([
                (read.cmd_valid.clone(), 1),
                (read.cmd_ready.clone(), 1),
                (read.cmd_addr.clone(), hmem.cmd_addr_width),
                (read.cmd_size.clone(), hmem.cmd_size_width),
                (read.valid.clone(), 1),
                (read.ready.clone(), 1),
                (read.data.clone(), hmem.data_width),
                (read.last.clone(), 1),
                (read.error.clone(), 1),
            ]);
        }
        if let Some(write) = &hmem.write {
            wires.extend([
                (write.cmd_valid.clone(), 1),
                (write.cmd_ready.clone(), 1),
                (write.cmd_addr.clone(), hmem.cmd_addr_width),
                (write.cmd_size.clone(), hmem.cmd_size_width),
                (write.valid.clone(), 1),
                (write.ready.clone(), 1),
                (write.data.clone(), hmem.data_width),
                (write.last.clone(), 1),
                (write.done_valid.clone(), 1),
                (write.done_error.clone(), 1),
            ]);
            if let Some(strb) = &write.strb {
                wires.push((strb.clone(), hmem.data_width / 8));
            }
        }
        for (port, width) in wires {
            out.push_str(&format!("    var w_{port}: {domain}logic<{width}>;\n"));
        }
        out.push_str(&format!(
            "    var t_{bundle}_maddr: {domain}logic<{}>;\n",
            hmem.host_addr_width()
        ));
        out.push_str(&format!(
            "    var t_{bundle}_mdata: {domain}logic<{}>;\n",
            hmem.data_width
        ));
        out.push_str(&format!("    var t_{bundle}_mwe: {domain}logic;\n"));
        out.push_str(&format!(
            "    var t_{bundle}_mrdata: {domain}logic<{}>;\n",
            hmem.data_width
        ));
        out.push_str(&format!(
            "    var t_{bundle}_oor: {domain}logic<{WORD_BITS}>;\n"
        ));
        out.push_str(&format!("    var t_{bundle}_delay: {domain}logic<8>;\n"));
        out.push_str(&format!("    var t_{bundle}_jitter: {domain}logic;\n"));
        // Between the stress stage and the terminator; not the DUT-side `w_*`.
        for (role, width) in [
            ("rd_cmd_valid", 1),
            ("rd_cmd_ready", 1),
            ("rd_cmd_addr", hmem.cmd_addr_width),
            ("rd_cmd_size", hmem.cmd_size_width),
            ("rd_valid", 1),
            ("rd_ready", 1),
            ("rd_data", hmem.data_width),
            ("rd_last", 1),
            ("rd_error", 1),
            ("wr_cmd_valid", 1),
            ("wr_cmd_ready", 1),
            ("wr_cmd_addr", hmem.cmd_addr_width),
            ("wr_cmd_size", hmem.cmd_size_width),
            ("wr_valid", 1),
            ("wr_ready", 1),
            ("wr_data", hmem.data_width),
            ("wr_strb", hmem.data_width / 8),
            ("wr_last", 1),
            ("wr_done", 1),
            ("wr_done_error", 1),
        ] {
            out.push_str(&format!(
                "    var h_{bundle}_{role}: {domain}logic<{width}>;\n"
            ));
        }
    }
    out.push('\n');

    // An APB slave (a Debug Module's DMI, or a `slave`): the window drives it
    // as an APB master. Like axi_mem, it says itself when it answers and when
    // it takes a write.
    for dmi in &plan.apbs {
        let bundle = &dmi.bundle;
        let aw = dmi.addr_width;
        out.push_str(&format!("    var t_{bundle}_maddr: {domain}logic<{aw}>;\n"));
        out.push_str(&format!(
            "    var t_{bundle}_mdata: {domain}logic<{WORD_BITS}>;\n"
        ));
        out.push_str(&format!("    var t_{bundle}_mwe: {domain}logic;\n"));
        out.push_str(&format!("    var t_{bundle}_mre: {domain}logic;\n"));
        out.push_str(&format!(
            "    var t_{bundle}_mrdata: {domain}logic<{WORD_BITS}>;\n"
        ));
        out.push_str(&format!("    var t_{bundle}_mrvalid: {domain}logic;\n"));
        out.push_str(&format!("    var t_{bundle}_mwready: {domain}logic;\n"));
        for (port, width) in [
            (&dmi.psel, 1),
            (&dmi.penable, 1),
            (&dmi.pwrite, 1),
            (&dmi.paddr, aw),
            (&dmi.pwdata, WORD_BITS),
            (&dmi.prdata, WORD_BITS),
        ] {
            out.push_str(&format!("    var w_{port}: {domain}logic<{width}>;\n"));
        }
        for port in [&dmi.pready, &dmi.pslverr].into_iter().flatten() {
            out.push_str(&format!("    var w_{port}: {domain}logic;\n"));
        }
        if dmi.pslverr.is_some() {
            out.push_str(&format!(
                "    var t_{bundle}_slverr: {domain}logic<{WORD_BITS}>;\n"
            ));
        }
        out.push_str(&format!(
            "\n    inst u_apb_{bundle}: hns::apb_host #(\n        ADDR_BITS: {aw},\n    ) (\n"
        ));
        out.push_str(&format!("        i_clk    : {clk},\n"));
        out.push_str(&format!("        i_rst    : {rst},\n"));
        // A slave on the DUT reset may stop answering when it is reset. A
        // Debug Module with a `reset` of its own is never reset with the DUT.
        let hold = if dmi.reset.is_some() {
            "1'b0"
        } else {
            "dut_fence"
        };
        out.push_str(&format!("        i_hold   : {hold},\n"));
        out.push_str(&format!("        i_addr   : t_{bundle}_maddr,\n"));
        out.push_str(&format!("        i_wdata  : t_{bundle}_mdata,\n"));
        out.push_str(&format!("        i_we     : t_{bundle}_mwe,\n"));
        out.push_str(&format!("        i_re     : t_{bundle}_mre,\n"));
        out.push_str(&format!("        o_rdata  : t_{bundle}_mrdata,\n"));
        out.push_str(&format!("        o_rvalid : t_{bundle}_mrvalid,\n"));
        out.push_str(&format!("        o_wready : t_{bundle}_mwready,\n"));
        out.push_str(&format!("        o_psel   : w_{},\n", dmi.psel));
        out.push_str(&format!("        o_penable: w_{},\n", dmi.penable));
        out.push_str(&format!("        o_pwrite : w_{},\n", dmi.pwrite));
        out.push_str(&format!("        o_paddr  : w_{},\n", dmi.paddr));
        out.push_str(&format!("        o_pwdata : w_{},\n", dmi.pwdata));
        out.push_str(&format!("        i_prdata : w_{},\n", dmi.prdata));
        // Without `pready` the slave answers in the access phase.
        let pready = dmi
            .pready
            .as_ref()
            .map(|port| format!("w_{port}"))
            .unwrap_or_else(|| "1'b1".to_string());
        out.push_str(&format!("        i_pready : {pready},\n"));
        match &dmi.pslverr {
            Some(port) => {
                out.push_str(&format!("        i_pslverr: w_{port},\n"));
                out.push_str(&format!("        o_slverrs: t_{bundle}_slverr,\n"));
            }
            None => {
                out.push_str("        i_pslverr: 1'b0,\n");
                out.push_str("        o_slverrs: _,\n");
            }
        }
        out.push_str("    );\n\n");
    }

    // Window decode, only when there are regions.
    //
    // Each region is aligned to its own size, so matching the upper address
    // bits against the base is enough. The reserved space matches none.
    //
    // `o_rvalid` values are ORed, and the data is picked by which region took
    // the request. The window takes one request at a time, so this is enough.
    if !map.regions.is_empty() {
        // axi_mem and a DMI say themselves when they answer and when they
        // take a write. Their latency is not fixed, so a delayed `re` is not
        // enough.
        let answers_itself = |bundle: &str| {
            plan.axi_mems.iter().any(|d| d.bundle == bundle)
                || plan.axi_slaves.iter().any(|d| d.bundle == bundle)
                || plan.apbs.iter().any(|d| d.bundle == bundle)
        };
        let word = WORD_BITS / 8;
        out.push_str(&format!("    var csr_addr: {domain}logic<{addr_bits}>;\n"));
        out.push_str(&format!("    var csr_wdata: {domain}logic<{WORD_BITS}>;\n"));
        out.push_str(&format!("    var csr_we: {domain}logic;\n"));
        out.push_str(&format!("    var csr_re: {domain}logic;\n"));
        out.push_str(&format!("    var csr_rdata: {domain}logic<{WORD_BITS}>;\n"));
        out.push_str(&format!("    var csr_rvalid: {domain}logic;\n"));
        out.push_str(&format!("    var csr_wready: {domain}logic;\n"));
        for region in &map.regions {
            let bundle = &region.bundle;
            let per_entry = region.entry_bytes / (WORD_BITS / 8);
            out.push_str(&format!("    var sel_{bundle}: {domain}logic;\n"));
            if region.is_aperture() {
                let pages = region.total_bytes / region.size_bytes as u64;
                let bits = (pages.trailing_zeros() as usize).max(1);
                for master in crate::regmap::WINDOW_MASTERS {
                    out.push_str(&format!(
                        "    var t_{bundle}_base_{master}: {domain}logic<{bits}>;\n"
                    ));
                }
                out.push_str(&format!("    var page_{bundle}: {domain}logic<{bits}>;\n"));
            }
            if answers_itself(bundle) {
                continue;
            }
            out.push_str(&format!("    var ans_{bundle}: {domain}logic;\n"));
            // For a DUT slave interface the other side is DUT ports: emit `w_*`
            // wires instead of the memory wires (`t_*_m*`).
            if let Some(slave) = plan.slaves.iter().find(|s| s.bundle == *bundle) {
                out.push_str(&format!(
                    "    var w_{}: {domain}logic<{}>;\n",
                    slave.addr, slave.addr_width
                ));
                out.push_str(&format!(
                    "    var w_{}: {domain}logic<{}>;\n",
                    slave.rdata, slave.width
                ));
                if let (Some(wdata), Some(we)) = (&slave.wdata, &slave.we) {
                    out.push_str(&format!(
                        "    var w_{wdata}: {domain}logic<{}>;\n",
                        slave.wdata_width
                    ));
                    out.push_str(&format!("    var w_{we}: {domain}logic;\n"));
                }
                // With `latency = 0` the answer is gone when the window looks
                // for it, so it is held. With 2 or more, `re` is delayed by a
                // shift register.
                match slave.latency {
                    0 => out.push_str(&format!(
                        "    var hold_{bundle}: {domain}logic<{}>;\n",
                        slave.width
                    )),
                    1 => {}
                    n => out.push_str(&format!(
                        "    var ansp_{bundle}: {domain}logic<{}>;\n",
                        n - 1
                    )),
                }
                continue;
            }
            // A trace ring is read only: nothing to strobe.
            if region.kind != crate::regmap::RegionKind::Trace {
                out.push_str(&format!(
                    "    var t_{bundle}_mstrb: {domain}logic<{}>;\n",
                    region.entry_bytes
                ));
            }
            if per_entry > 1 {
                let bits = per_entry.trailing_zeros() as usize;
                out.push_str(&format!(
                    "    var t_{bundle}_mword: {domain}logic<{bits}>;\n"
                ));
                out.push_str(&format!("    var answ_{bundle}: {domain}logic<{bits}>;\n"));
            }
        }
        out.push('\n');

        let hi = addr_bits - 1;
        for region in &map.regions {
            let bundle = &region.bundle;
            let inside = region.size_bytes.trailing_zeros() as usize;
            if inside > hi {
                // The region is the whole window; there are no upper bits.
                out.push_str(&format!("    assign sel_{bundle} = 1'b1;\n"));
            } else {
                let top = region.window_base >> inside;
                out.push_str(&format!(
                    "    assign sel_{bundle} = bus_addr[{hi}:{inside}] == {}'d{top};\n",
                    hi - inside + 1
                ));
            }
        }
        // Each master has its own base. With a shared base, one master could
        // move it just before the other reads.
        for region in map.regions.iter().filter(|r| r.is_aperture()) {
            let bundle = &region.bundle;
            out.push_str(&format!(
                "    assign page_{bundle} = if (if bus_we ? bus_wmaster : bus_rmaster) ? t_{bundle}_base_pcie : t_{bundle}_base_jtag;\n"
            ));
        }
        let any = map
            .regions
            .iter()
            .map(|region| format!("sel_{}", region.bundle))
            .collect::<Vec<_>>()
            .join(" | ");
        out.push_str("\n    assign csr_addr  = bus_addr;\n");
        out.push_str("    assign csr_wdata = bus_wdata;\n");
        out.push_str(&format!("    assign csr_we    = bus_we & ~({any});\n"));
        out.push_str(&format!("    assign csr_re    = bus_re & ~({any});\n"));

        for region in &map.regions {
            let bundle = &region.bundle;
            let inside = region.size_bytes.trailing_zeros() as usize;
            let entry_hi = inside - 1;
            let shift = word.trailing_zeros() as usize;
            // axi_mem is one word per entry. `hns::axi_host` handles a wider
            // bus, so a plain word index is enough here.
            let paged = |inner: String| {
                if region.is_aperture() {
                    format!("{{page_{bundle}, {inner}}}")
                } else {
                    inner
                }
            };
            // An APB slave takes the byte address as it is.
            if plan.apbs.iter().any(|d| d.bundle == *bundle) {
                out.push_str(&format!(
                    "    assign t_{bundle}_maddr = bus_addr[{entry_hi}:0];\n"
                ));
                out.push_str(&format!("    assign t_{bundle}_mdata = bus_wdata;\n"));
                out.push_str(&format!(
                    "    assign t_{bundle}_mwe   = bus_we & sel_{bundle};\n"
                ));
                out.push_str(&format!(
                    "    assign t_{bundle}_mre   = bus_re & sel_{bundle};\n"
                ));
                continue;
            }
            if plan.axi_mems.iter().any(|d| d.bundle == *bundle)
                || plan.axi_slaves.iter().any(|d| d.bundle == *bundle)
            {
                out.push_str(&format!(
                    "    assign t_{bundle}_maddr = {};\n",
                    paged(format!("bus_addr[{entry_hi}:{shift}]"))
                ));
                out.push_str(&format!("    assign t_{bundle}_mdata = bus_wdata;\n"));
                out.push_str(&format!(
                    "    assign t_{bundle}_mwe   = bus_we & sel_{bundle};\n"
                ));
                out.push_str(&format!(
                    "    assign t_{bundle}_mre   = bus_re & sel_{bundle};\n"
                ));
                continue;
            }
            // A slave interface gets the window bus as is. The address is a
            // word index: the window offset without its low 2 bits.
            if let Some(slave) = plan.slaves.iter().find(|s| s.bundle == *bundle) {
                out.push_str(&format!(
                    "    assign w_{} = bus_addr[{entry_hi}:{shift}];\n",
                    slave.addr
                ));
                if let (Some(wdata), Some(we)) = (&slave.wdata, &slave.we) {
                    let low = if slave.wdata_width == WORD_BITS {
                        String::new()
                    } else {
                        format!("[{}:0]", slave.wdata_width - 1)
                    };
                    out.push_str(&format!("    assign w_{wdata} = bus_wdata{low};\n"));
                    out.push_str(&format!("    assign w_{we}    = bus_we & sel_{bundle};\n"));
                }
                continue;
            }
            // Window words per entry. A wide entry spans several words, and the
            // low bits select the word inside the entry.
            let per_entry = region.entry_bytes / word;
            let lo = shift + per_entry.trailing_zeros() as usize;

            // A trace ring is read only. A write is accepted and dropped, so
            // that the window does not hang on it.
            if region.kind == crate::regmap::RegionKind::Trace {
                out.push_str(&format!(
                    "    assign t_{bundle}_maddr = bus_addr[{entry_hi}:{lo}];\n"
                ));
                if per_entry > 1 {
                    out.push_str(&format!(
                        "    assign t_{bundle}_mword = bus_addr[{}:{shift}];\n",
                        lo - 1
                    ));
                }
                continue;
            }
            let strb_w = region.entry_bytes;

            // Reuse the wires of the indirect port. The memory does not care
            // whether the CSR or the decoder drives them.
            out.push_str(&format!(
                "    assign t_{bundle}_maddr = {};\n",
                paged(format!("bus_addr[{entry_hi}:{lo}]"))
            ));
            // Replicate the data to the entry width; the strobe selects the bytes.
            let spread = std::iter::repeat_n("bus_wdata", per_entry)
                .collect::<Vec<_>>()
                .join(", ");
            if per_entry == 1 {
                out.push_str(&format!("    assign t_{bundle}_mdata = bus_wdata;\n"));
                out.push_str(&format!(
                    "    assign t_{bundle}_mstrb = {strb_w}'h{:x};\n",
                    (1u64 << strb_w) - 1
                ));
            } else {
                out.push_str(&format!("    assign t_{bundle}_mdata = {{{spread}}};\n"));
                out.push_str(&format!(
                    "    assign t_{bundle}_mword = bus_addr[{}:{shift}];\n",
                    lo - 1
                ));
                out.push_str(&format!(
                    "    assign t_{bundle}_mstrb = {strb_w}'hf << (t_{bundle}_mword * {word});\n"
                ));
            }
            out.push_str(&format!(
                "    assign t_{bundle}_mwe   = bus_we & sel_{bundle};\n"
            ));
        }

        // Which region answers. A memory read takes one cycle, so `re` delayed
        // by one cycle marks the answering region. It selects the data and is
        // also `rvalid`, so it must be a pulse; a held value never lowers
        // `rvalid`. The top has several clocks, so `always_ff` names its clock.
        out.push_str(&format!(
            "\n    always_ff ({clk}, {rst}) {{\n        if_reset {{\n"
        ));
        for region in &map.regions {
            let bundle = &region.bundle;
            if answers_itself(bundle) {
                continue;
            }
            out.push_str(&format!("            ans_{bundle} = 0;\n"));
            if let Some(slave) = plan.slaves.iter().find(|s| s.bundle == *bundle) {
                match slave.latency {
                    0 => out.push_str(&format!("            hold_{bundle} = 0;\n")),
                    1 => {}
                    _ => out.push_str(&format!("            ansp_{bundle} = 0;\n")),
                }
            }
            // Same condition as in the `else` below. Without a reset,
            // `veryl check` warns, and a warning fails the check.
            if plan.slaves.iter().all(|s| s.bundle != *bundle)
                && region.entry_bytes / (WORD_BITS / 8) > 1
            {
                out.push_str(&format!("            answ_{bundle} = 0;\n"));
            }
        }
        out.push_str("        } else {\n");
        for region in &map.regions {
            let bundle = &region.bundle;
            if answers_itself(bundle) {
                continue;
            }
            match plan
                .slaves
                .iter()
                .find(|s| s.bundle == *bundle)
                .map(|s| s.latency)
            {
                Some(0) => {
                    out.push_str(&format!(
                        "            ans_{bundle} = bus_re & sel_{bundle};\n"
                    ));
                    let rdata = &plan
                        .slaves
                        .iter()
                        .find(|s| s.bundle == *bundle)
                        .expect("found above")
                        .rdata;
                    out.push_str(&format!(
                        "            if bus_re & sel_{bundle} {{\n                hold_{bundle} = w_{rdata};\n            }}\n"
                    ));
                }
                Some(2) => {
                    out.push_str(&format!(
                        "            ansp_{bundle} = bus_re & sel_{bundle};\n"
                    ));
                    out.push_str(&format!("            ans_{bundle} = ansp_{bundle};\n"));
                }
                Some(n) if n > 2 => {
                    out.push_str(&format!(
                        "            ansp_{bundle} = {{ansp_{bundle}[{}:0], bus_re & sel_{bundle}}};\n",
                        n - 3
                    ));
                    out.push_str(&format!(
                        "            ans_{bundle} = ansp_{bundle}[{}];\n",
                        n - 2
                    ));
                }
                _ => out.push_str(&format!(
                    "            ans_{bundle} = bus_re & sel_{bundle};\n"
                )),
            }
            // A wide entry also needs to remember which word to return.
            // A slave interface is exactly one word, so it does not.
            if plan.slaves.iter().all(|s| s.bundle != *bundle)
                && region.entry_bytes / (WORD_BITS / 8) > 1
            {
                out.push_str(&format!("            answ_{bundle} = t_{bundle}_mword;\n"));
            }
        }
        out.push_str("        }\n    }\n\n");

        // "This region answers": a delayed `re` for memories, the terminator's
        // own signal for axi_mem and a DMI.
        let answering = |bundle: &str| {
            if answers_itself(bundle) {
                format!("t_{bundle}_mrvalid")
            } else {
                format!("ans_{bundle}")
            }
        };

        let mut rdata = String::from("csr_rdata");
        for region in map.regions.iter().rev() {
            let bundle = &region.bundle;
            let from = if let Some(slave) = plan.slaves.iter().find(|s| s.bundle == *bundle) {
                let read = if slave.latency == 0 {
                    format!("hold_{bundle}")
                } else {
                    format!("w_{}", slave.rdata)
                };
                // A narrower register reads back with zeros above it.
                if slave.width == WORD_BITS {
                    read
                } else {
                    format!("{{{}'h0, {read}}}", WORD_BITS - slave.width)
                }
            } else if plan.axi_mems.iter().all(|d| d.bundle != *bundle)
                && region.entry_bytes / (WORD_BITS / 8) > 1
            {
                format!("t_{bundle}_mrdata[answ_{bundle} * {WORD_BITS}+:{WORD_BITS}]")
            } else {
                format!("t_{bundle}_mrdata")
            };
            rdata = format!("if {} ? {from} : {rdata}", answering(bundle));
        }
        out.push_str(&format!("    assign bus_rdata = {rdata};\n"));
        let rvalid = std::iter::once("csr_rvalid".to_string())
            .chain(map.regions.iter().map(|region| answering(&region.bundle)))
            .collect::<Vec<_>>()
            .join(" | ");
        out.push_str(&format!("    assign bus_rvalid = {rvalid};\n"));
        // Each term is gated by its own select, and the CSR by `~({any})`. A
        // plain OR would accept a write when any target is ready, and the
        // write pulse to a busy target would be lost.
        let wready = std::iter::once(format!("(~({any}) & csr_wready)"))
            .chain(map.regions.iter().map(|region| {
                let bundle = &region.bundle;
                if answers_itself(bundle) {
                    format!("(sel_{bundle} & t_{bundle}_mwready)")
                } else {
                    // Region memories and DUT slave interfaces always accept.
                    format!("sel_{bundle}")
                }
            }))
            .collect::<Vec<_>>()
            .join(" | ");
        out.push_str(&format!("    assign bus_wready = {wready};\n\n"));
    }

    out.push_str("    inst u_csr: csr (\n");
    out.push_str(&format!("        i_clk: {clk},\n"));
    out.push_str(&format!("        i_rst: {rst},\n"));
    // The decoder sits in between only when there are regions. Otherwise the
    // CSR is the whole window.
    let csr = if map.regions.is_empty() { "bus" } else { "csr" };
    out.push_str(&format!("        i_addr: {csr}_addr,\n"));
    out.push_str(&format!("        i_wdata: {csr}_wdata,\n"));
    out.push_str(&format!("        i_we: {csr}_we,\n"));
    out.push_str(&format!("        i_re: {csr}_re,\n"));
    out.push_str(&format!("        o_rdata: {csr}_rdata,\n"));
    out.push_str(&format!("        o_rvalid: {csr}_rvalid,\n"));
    out.push_str(&format!("        o_wready: {csr}_wready,\n"));
    out.push_str("        i_timeouts: bus_timeouts,\n");
    out.push_str("        o_clear_timeouts: bus_clear_timeouts,\n");
    for register in map.registers.iter().filter(|r| r.kind == Kind::Port) {
        let port = dut_port(&plan.dut, &register.name);
        let to_dut = register.access == Access::ReadWrite;
        out.push_str(&format!(
            "        {}: w_{},\n",
            faced(prefixes, port, to_dut),
            register.name
        ));
    }
    // Terminator registers. Constants (depth) are consts in the CSR: no wire.
    for register in map.registers.iter().filter(|r| r.kind == Kind::Terminator) {
        if register.value.is_some() {
            continue;
        }
        let from_csr = register.access == Access::ReadWrite;
        // The DMA registers have no bundle: the DMA engine belongs to the
        // harness, not to the DUT.
        let Some(bundle) = register.bundle.as_deref() else {
            out.push_str(&format!(
                "        {}: t_{},\n",
                faced_terminator(prefixes, &register.name, from_csr),
                register.name
            ));
            continue;
        };
        let role = register.role.expect("terminator registers carry a role");
        out.push_str(&format!(
            "        {}: t_{bundle}_{role},\n",
            faced_terminator(prefixes, &register.name, from_csr)
        ));
        // The memory window writes and reads different signals (the pair of
        // the ports in `hns_csr`).
        if role == "mdata" {
            out.push_str(&format!(
                "        {}: t_{bundle}_mwe,\n",
                faced_terminator(prefixes, &format!("{bundle}_mwe"), true)
            ));
            out.push_str(&format!(
                "        {}: t_{bundle}_mrdata,\n",
                faced_terminator(prefixes, &format!("{bundle}_mrdata"), false)
            ));
        }
    }
    out.push_str("    );\n\n");

    // The DUT reset. The harness is not reset. Each AXI4 master gets a fence:
    // close it, reset the DUT, and release only when it is empty. If the DUT
    // stops in the middle of a burst, the arbiter waits for B forever.
    if second_master && perst_resets_dut(plan) {
        out.push_str(
            "    // DUT reset from the host (dut_reset) or PERST#. The harness itself is not reset.\n",
        );
    } else {
        out.push_str(
            "    // DUT reset from the host (dut_reset). The harness itself is not reset.\n",
        );
    }
    for axi_mem in &plan.axi_mems {
        let bundle = &axi_mem.bundle;
        out.push_str(&format!("    var fence_closed_{bundle}: {domain}logic;\n"));
        out.push_str(&format!("    var fence_idle_{bundle}: {domain}logic;\n"));
    }
    let closed = if plan.axi_mems.is_empty() {
        "1".to_string()
    } else {
        plan.axi_mems
            .iter()
            .map(|m| format!("fence_closed_{}", m.bundle))
            .collect::<Vec<_>>()
            .join(" & ")
    };
    let idle = if plan.axi_mems.is_empty() {
        "1".to_string()
    } else {
        plan.axi_mems
            .iter()
            .map(|m| format!("fence_idle_{}", m.bundle))
            .collect::<Vec<_>>()
            .join(" & ")
    };
    out.push_str("    inst u_dut_reset: hns::reset_ctl (\n");
    out.push_str(&format!("        i_clk   : {clk},\n"));
    out.push_str(&format!("        i_rst   : {rst},\n"));
    // PERST# holds the DUT the same way, so the fence closes first.
    let hold = if second_master && perst_resets_dut(plan) {
        format!("t_{DUT_RESET} | pcie_perst")
    } else {
        format!("t_{DUT_RESET}")
    };
    out.push_str(&format!("        i_hold  : {hold},\n"));
    out.push_str("        i_pulse : 0,\n");
    out.push_str(&format!("        i_closed: {closed},\n"));
    out.push_str(&format!("        i_idle  : {idle},\n"));
    out.push_str("        o_fence : dut_fence,\n");
    out.push_str("        o_rst   : dut_rst_req,\n");
    out.push_str("        o_busy  : _,\n");
    out.push_str("    );\n");
    out.push_str(&format!(
        "    assign t_{DUT_RESET_STATE} = dut_rst_req;\n\n"
    ));
    for axi_mem in &plan.axi_mems {
        let bundle = &axi_mem.bundle;
        out.push_str(&format!(
            "    inst u_fence_{bundle}: hns::axi_fence::<{}> (\n",
            axi_mem.pkg
        ));
        out.push_str(&format!("        i_clk   : {clk},\n"));
        out.push_str(&format!("        i_rst   : {rst},\n"));
        out.push_str("        i_fence : dut_fence,\n");
        out.push_str(&format!("        o_closed: fence_closed_{bundle},\n"));
        out.push_str(&format!("        o_idle  : fence_idle_{bundle},\n"));
        out.push_str(&format!("        s_axi   : axi_{bundle},\n"));
        out.push_str(&format!("        m_axi   : faxi_{bundle},\n"));
        out.push_str("    );\n\n");
    }
    // `hns_sim` has no `hns_clk`, so the DUT reset is made here. It comes from
    // a flop, so no LUT drives the reset net.
    if clocks.is_none() {
        let reset = plan.metadata.build.reset_type;
        let on = asserted(reset);
        let off = 1 - on;
        out.push_str(&format!("    var dut_rst_q: {domain}logic;\n"));
        out.push_str(&format!(
            "    var dut_rst: {domain}{};\n",
            reset_type_name(reset)
        ));
        out.push_str(&format!("    always_ff ({clk}, {rst}) {{\n"));
        out.push_str(&format!(
            "        if_reset {{\n            dut_rst_q = {on};\n"
        ));
        out.push_str(&format!(
            "        }} else {{\n            dut_rst_q = if dut_rst_req ? {on} : {off};\n        }}\n    }}\n"
        ));
        out.push_str(&format!(
            "    assign dut_rst = dut_rst_q as {};\n\n",
            reset_type_name(reset)
        ));
    }

    // The card-issued DMA. The gate is always present. The DMA engine exists
    // only in a PCIe design, so `hns_sim` has the gate alone.
    let dma = dma_registers(map);
    if !dma.is_empty() {
        for (name, width) in DMA_DESC_WIRES {
            out.push_str(&format!("    var {name}: {domain}{};\n", logic(width)));
        }
        let count_w = dma
            .iter()
            .find(|r| r.role == Some("dma_done"))
            .map(|r| r.width)
            .expect("the dma registers are placed together");
        out.push_str(&format!(
            "\n    inst u_dma_gate: hns::dma_gate #(\n        COUNT_W: {count_w},\n    ) (\n"
        ));
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        for (port, wire) in [
            ("i_base", "t_dma_base"),
            ("i_size", "t_dma_size"),
            ("i_pcie_addr", "t_dma_pcie_addr"),
            ("i_axi_addr", "t_dma_axi_addr"),
            ("i_len", "t_dma_len"),
            ("i_dir", "t_dma_dir"),
            ("i_go", "t_dma_go"),
            ("o_desc_pcie_addr", "dma_desc_pcie_addr"),
            ("o_desc_axi_addr", "dma_desc_axi_addr"),
            ("o_desc_len", "dma_desc_len"),
            ("o_desc_tag", "dma_desc_tag"),
            ("o_wr_valid", "dma_wr_valid"),
            ("i_wr_ready", "dma_wr_ready"),
            ("o_rd_valid", "dma_rd_valid"),
            ("i_rd_ready", "dma_rd_ready"),
            ("i_wr_status_valid", "dma_wr_status_valid"),
            ("i_wr_status_error", "dma_wr_status_error"),
            ("i_rd_status_valid", "dma_rd_status_valid"),
            ("i_rd_status_error", "dma_rd_status_error"),
            ("o_busy", "t_dma_busy"),
            ("o_error", "t_dma_error"),
            ("o_done", "t_dma_done"),
            ("o_out_of_range", "t_dma_oor"),
            ("o_while_busy", "t_dma_blocked"),
        ] {
            out.push_str(&format!("        {port}: {wire},\n"));
        }
        out.push_str("    );\n\n");

        // The host sees the values the DMA engine uses (`o_mps` of
        // `hns::dma_wr`), not the negotiated link values. Otherwise the host
        // cannot tell whether `dma_mps_limit` took effect.
        out.push_str("    assign t_dma_mps = dma_mps_used;\n");
        out.push_str("    assign t_dma_mrrs = dma_mrrs_used;\n\n");
        // RQ TLPs that the hard block dropped. They are already synchronized
        // next to the wrapper (`u_rqdrop_sync`); this is the window clock.
        if second_master {
            out.push_str("    assign t_dma_rq_drops = rq_drops;\n");
            out.push_str("    assign t_dma_rq_gaps = rq_gaps;\n\n");
        }

        // A top with no DMA engine (`hns_sim`): tie the inputs so that nothing
        // is ever accepted.
        if !second_master {
            out.push_str("    // No requester here: hns_sim has no PCIe to issue on.\n");
            out.push_str("    assign dma_mps_used = 0;\n");
            out.push_str("    assign dma_mrrs_used = 0;\n");
            out.push_str("    assign dma_wr_ready = 0;\n");
            out.push_str("    assign dma_rd_ready = 0;\n");
            out.push_str("    assign dma_wr_status_valid = 0;\n");
            out.push_str("    assign dma_wr_status_error = 0;\n");
            out.push_str("    assign dma_rd_status_valid = 0;\n");
            out.push_str("    assign dma_rd_status_error = 0;\n");
            out.push_str("    assign t_dma_rq_drops = 0;\n");
            out.push_str("    assign t_dma_rq_gaps = 0;\n");
            out.push_str("    assign t_dma_rc_cor = 0;\n");
            out.push_str("    assign t_dma_rc_uncor = 0;\n\n");
        }
    }

    // The TLP counts, from the wrapper in `top_module`. `hns_sim` has no PCIe
    // to count, so they read 0 there.
    if map
        .registers
        .iter()
        .any(|r| r.role == Some(crate::regmap::PCIE_TLP_COUNTERS[0].0))
    {
        for (name, _) in crate::regmap::PCIE_TLP_COUNTERS {
            if second_master {
                out.push_str(&format!("    assign t_{name} = tlp_{name};\n"));
            } else {
                out.push_str(&format!("    assign t_{name} = 0;\n"));
            }
        }
        out.push('\n');
    }

    // host_poll_fifo terminators: one module, with depth and width as parameters.
    for fifo in &plan.fifos {
        let bundle = &fifo.bundle;
        out.push_str(&format!("    inst u_fifo_{bundle}: hns::fifo #(\n"));
        out.push_str(&format!("        WIDTH: {},\n", fifo.width));
        out.push_str(&format!("        DEPTH: {},\n", fifo.depth));
        out.push_str("    ) (\n");
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        let push = if fifo.valid_invert {
            format!("~w_{}", fifo.valid)
        } else {
            format!("w_{}", fifo.valid)
        };
        out.push_str(&format!("        i_push: {push},\n"));
        out.push_str(&format!("        i_data: w_{},\n", fifo.data));
        match &fifo.ready {
            Some(_) => out.push_str(&format!("        o_ready: t_{bundle}_ready,\n")),
            // The DUT cannot be stalled. Nobody reads ready; drops are counted.
            None => out.push_str("        o_ready: _,\n"),
        }
        out.push_str(&format!("        i_pop: t_{bundle}_pop,\n"));
        out.push_str(&format!("        o_data: t_{bundle}_data,\n"));
        out.push_str(&format!("        o_level: t_{bundle}_level,\n"));
        if fifo.counts_drops() {
            out.push_str(&format!("        o_drops: t_{bundle}_drops,\n"));
        } else {
            out.push_str("        o_drops: _,\n");
        }
        out.push_str("    );\n\n");

        // Undo the inversion here (`ready = "!i_stall"`).
        if let Some(ready) = &fifo.ready {
            let expr = if ready.invert {
                format!("~t_{bundle}_ready")
            } else {
                format!("t_{bundle}_ready")
            };
            out.push_str(&format!("    assign w_{} = {expr};\n\n", ready.port));
        }
    }

    // trace terminators. The payload goes in above the cycle count, the first
    // field lowest.
    for trace in &plan.traces {
        let bundle = &trace.bundle;
        out.push_str(&format!("    inst u_trace_{bundle}: hns::trace #(\n"));
        let twidth = trace.trigger_width();
        out.push_str(&format!("        WIDTH: {},\n", trace.payload_width()));
        // Without `trigger` the comparison is one constant bit, masked off.
        out.push_str(&format!("        TWIDTH: {},\n", twidth.max(1)));
        out.push_str(&format!("        DEPTH: {},\n", trace.depth));
        out.push_str("    ) (\n");
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        out.push_str(&format!("        i_run: t_{bundle}_run,\n"));
        out.push_str(&format!("        i_pause: t_{bundle}_pause,\n"));
        let valid = if trace.valid_invert {
            format!("~w_{}", trace.valid)
        } else {
            format!("w_{}", trace.valid)
        };
        out.push_str(&format!("        i_valid: {valid},\n"));
        let data = trace
            .fields
            .iter()
            .rev()
            .map(|field| format!("w_{}", field.port))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("        i_data: {{{data}}},\n"));
        if twidth > 0 {
            // The "not 0" bits above the fields, the first field lowest in
            // each group.
            let nonzero = trace
                .fields
                .iter()
                .rev()
                .filter(|field| field.nonzero_lsb.is_some())
                .map(|field| format!("|w_{}", field.port));
            let fields = trace
                .fields
                .iter()
                .rev()
                .filter(|field| field.trigger_lsb.is_some())
                .map(|field| format!("w_{}", field.port));
            let tdata = nonzero.chain(fields).collect::<Vec<_>>().join(", ");
            out.push_str(&format!("        i_tdata: {{{tdata}}},\n"));
            out.push_str(&format!("        i_tmask: t_{bundle}_tmask,\n"));
            out.push_str(&format!("        i_tvalue: t_{bundle}_tvalue,\n"));
        } else {
            out.push_str("        i_tdata: 1'b0,\n");
            out.push_str("        i_tmask: 1'b0,\n");
            out.push_str("        i_tvalue: 1'b0,\n");
        }
        out.push_str(&format!("        i_arm: t_{bundle}_arm,\n"));
        out.push_str(&format!("        i_after: t_{bundle}_after,\n"));
        out.push_str(&format!("        o_count: t_{bundle}_count,\n"));
        out.push_str(&format!("        o_wrapped: t_{bundle}_wrapped,\n"));
        out.push_str(&format!("        o_hit: t_{bundle}_hit,\n"));
        out.push_str(&format!("        o_hit_at: t_{bundle}_hit_at,\n"));
        out.push_str(&format!("        i_haddr: t_{bundle}_maddr,\n"));
        out.push_str(&format!("        o_hrdata: t_{bundle}_hrdata,\n"));
        out.push_str("    );\n");
        // The window reads whole entries of a power of two of words; the
        // words above the record read as 0.
        let pad = trace.entry_words() * WORD_BITS - trace.record_width();
        if pad == 0 {
            out.push_str(&format!(
                "    assign t_{bundle}_mrdata = t_{bundle}_hrdata;\n\n"
            ));
        } else {
            out.push_str(&format!(
                "    assign t_{bundle}_mrdata = {{{pad}'h0, t_{bundle}_hrdata}};\n\n"
            ));
        }
    }

    // bram / bram_preload terminators, with depth, width and latency as parameters.
    for mem in &plan.memories {
        let bundle = &mem.bundle;
        out.push_str(&format!("    inst u_mem_{bundle}: hns::mem #(\n"));
        out.push_str(&format!("        WIDTH: {},\n", mem.width));
        out.push_str(&format!("        SLICES: {},\n", mem.slices()));
        out.push_str(&format!("        DEPTH: {},\n", mem.depth));
        out.push_str(&format!("        LATENCY: {},\n", mem.latency));
        out.push_str(&format!("        ADDR_W: {},\n", mem.addr_width));
        out.push_str(&format!("        SHIFT: {},\n", mem.addr_shift));
        out.push_str(&format!(
            "        STRB_W: {},\n",
            (mem.entry_width / 8).max(1)
        ));
        let has_strb = mem
            .write
            .as_ref()
            .is_some_and(|write| write.wstrb.is_some());
        out.push_str(&format!("        HAS_STRB: {has_strb},\n"));
        // Only a region needs a host-side strobe. The indirect port collects the
        // whole entry in a holding register, so it writes the whole entry.
        let region = mem.access == crate::manifest::MemAccess::Region;
        out.push_str(&format!(
            "        HSTRB_W: {},\n",
            (mem.entry_width / 8).max(1)
        ));
        out.push_str(&format!("        HAS_HSTRB: {region},\n"));
        out.push_str(&format!("        HAS_EN: {},\n", mem.enable.is_some()));
        out.push_str("    ) (\n");
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        // The address does not wrap. The terminator returns 0 for an
        // out-of-range address and counts it. Below `dut_base`, the
        // subtraction wraps past the end, which the plan made sure of.
        if mem.base_steps == 0 {
            out.push_str(&format!("        i_addr: w_{},\n", mem.addr));
        } else {
            out.push_str(&format!(
                "        i_addr: w_{} - {}'h{:x},\n",
                mem.addr, mem.addr_width, mem.base_steps
            ));
        }
        out.push_str(&format!("        o_rdata: w_{},\n", mem.rdata));
        match &mem.write {
            Some(write) => {
                out.push_str(&format!("        i_wdata: w_{},\n", write.wdata));
                match &write.wstrb {
                    Some(wstrb) => out.push_str(&format!("        i_wstrb: w_{wstrb},\n")),
                    // A DUT without a strobe writes the whole entry.
                    None => out.push_str(&format!(
                        "        i_wstrb: {}'b1,\n",
                        (mem.entry_width / 8).max(1)
                    )),
                }
                let we = if write.we_invert {
                    format!("~w_{}", write.we)
                } else {
                    format!("w_{}", write.we)
                };
                out.push_str(&format!("        i_we: {we},\n"));
            }
            // With bram_preload the DUT does not write.
            None => {
                out.push_str(&format!("        i_wdata: {}'b0,\n", mem.entry_width));
                out.push_str(&format!(
                    "        i_wstrb: {}'b0,\n",
                    (mem.entry_width / 8).max(1)
                ));
                out.push_str("        i_we: 1'b0,\n");
            }
        }
        match &mem.enable {
            Some(enable) => {
                let en = if enable.invert {
                    format!("~w_{}", enable.port)
                } else {
                    format!("w_{}", enable.port)
                };
                out.push_str(&format!("        i_en: {en},\n"));
            }
            None => out.push_str("        i_en: 1'b1,\n"),
        }
        if mem.counts_out_of_range() {
            out.push_str(&format!("        o_oor: t_{bundle}_oor,\n"));
        } else {
            out.push_str("        o_oor: _,\n");
        }
        out.push_str(&format!("        i_haddr: t_{bundle}_maddr,\n"));
        out.push_str(&format!("        i_hwdata: t_{bundle}_mdata,\n"));
        if region {
            out.push_str(&format!("        i_hwstrb: t_{bundle}_mstrb,\n"));
        } else {
            out.push_str(&format!(
                "        i_hwstrb: {}'b0,\n",
                (mem.entry_width / 8).max(1)
            ));
        }
        out.push_str(&format!("        i_hwe: t_{bundle}_mwe,\n"));
        out.push_str(&format!("        o_hrdata: t_{bundle}_mrdata,\n"));
        out.push_str("    );\n\n");
    }

    // axi_mem. The window gets its own master, arbitrated with the DUT, to the
    // same memory. Downstream is either `hns::axi_mem` (a BRAM stand-in) or
    // the MIG; the DUT and the host see the same shape in both cases.
    for axi_mem in &plan.axi_mems {
        let bundle = &axi_mem.bundle;

        let (hpkg, widen) = host_pkg(plan, axi_mem);

        // A transfer-level port becomes the AXI4 master here. The adapter
        // holds the DUT's transfers, so it is reset with the DUT, and the
        // fence behind it finishes what a reset cuts off.
        if let Some(xfer) = &axi_mem.xfer {
            xfer_adapter(out, clocks, clk, rst, axi_mem, xfer);
        }
        if let Some(corebus) = &axi_mem.corebus {
            corebus_adapter(out, clocks, clk, rst, axi_mem, corebus);
        }

        // With `dut_base`, only what is inside the memory goes on, with the
        // base taken off. The rest is answered with DECERR and counted. It
        // also sets the address width, so no `hns::axi_aw` is needed.
        if axi_mem.dut_base != 0 {
            out.push_str(&format!(
                "    inst u_range_{bundle}: hns::axi_range::<\n        {},\n        {hpkg},\n    > #(\n",
                axi_mem.pkg
            ));
            out.push_str(&format!("        BASE: 64'h{:x},\n", axi_mem.dut_base));
            out.push_str(&format!(
                "        SPAN: 64'h{:x},\n",
                u64::from(axi_mem.depth) * u64::from(axi_mem.data_bytes)
            ));
            out.push_str("    ) (\n");
            out.push_str(&format!("        i_clk: {clk},\n"));
            out.push_str(&format!("        i_rst: {rst},\n"));
            out.push_str(&format!("        o_oor: t_{bundle}_oor,\n"));
            out.push_str(&format!("        s_axi: faxi_{bundle},\n"));
            out.push_str(&format!("        m_axi: raxi_{bundle},\n"));
            out.push_str("    );\n\n");
        }

        // Widen the DUT address. It is wiring only (`hns::axi_aw`), so it adds
        // no latency to the DUT under test.
        if widen {
            out.push_str(&format!(
                "    inst u_aw_{bundle}: hns::axi_aw::<\n        {},\n        {hpkg},\n    > (\n",
                axi_mem.pkg
            ));
            out.push_str(&format!("        s_axi: faxi_{bundle},\n"));
            out.push_str(&format!("        m_axi: xaxi_{bundle},\n"));
            out.push_str("    );\n\n");
        }

        out.push_str(&format!(
            "    inst u_haxi_{bundle}: hns::axi_host::<{hpkg}> #(\n"
        ));
        out.push_str(&format!("        ADDR_W: {},\n", axi_mem.word_addr_width()));
        out.push_str("    ) (\n");
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        // The memory is the harness's, never reset with the DUT.
        out.push_str("        i_hold: 1'b0,\n");
        out.push_str(&format!("        axi: haxi_{bundle},\n"));
        out.push_str(&format!("        i_haddr: t_{bundle}_maddr,\n"));
        out.push_str(&format!("        i_hwdata: t_{bundle}_mdata,\n"));
        out.push_str(&format!("        i_hwe: t_{bundle}_mwe,\n"));
        out.push_str(&format!("        i_hre: t_{bundle}_mre,\n"));
        out.push_str(&format!("        o_hrdata: t_{bundle}_mrdata,\n"));
        out.push_str(&format!("        o_hrvalid: t_{bundle}_mrvalid,\n"));
        out.push_str(&format!("        o_hwready: t_{bundle}_mwready,\n"));
        out.push_str("    );\n\n");

        // The DUT is m0, the host m1. The order has no meaning, but it is
        // fixed so that diffs of the output stay readable.
        out.push_str(&format!(
            "    inst u_rr_{bundle}: hns::axi_rr::<{hpkg}> (\n"
        ));
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        out.push_str(&format!(
            "        m0: {}_{bundle},\n",
            if axi_mem.dut_base != 0 {
                "raxi"
            } else if widen {
                "xaxi"
            } else {
                "faxi"
            }
        ));
        out.push_str(&format!("        m1: haxi_{bundle},\n"));
        out.push_str(&format!("        s: saxi_{bundle},\n"));
        out.push_str("    );\n\n");

        if sim_drams(plan).iter().any(|m| m.bundle == *bundle) {
            out.push_str(&format!(
                "    // `saxi_{bundle}` goes to `$comp::hns_dram` in the testbench, which\n    // has nothing to calibrate.\n    assign t_{bundle}_calib = 1;\n\n"
            ));
            continue;
        }

        // `dram` goes to the real controller. Only this differs from the
        // stand-in.
        if axi_mem.backing == crate::manifest::Backing::Dram && clocks.is_some() {
            if is_ps7(plan) {
                ps7_instance(out, plan, axi_mem, domain);
            } else {
                mig_instance(out, plan, axi_mem, domain);
            }
            continue;
        }

        // The stand-in is smaller than the real memory. The `depth` of `dram`
        // can be 256 MB, and the simulator cannot build that array (it hits
        // the Veryl evaluation limit). The stand-in checks wiring, not
        // capacity, so it keeps only the first entries.
        let depth = if axi_mem.backing == crate::manifest::Backing::Dram {
            axi_mem.depth.min(SIM_DRAM_ENTRIES)
        } else {
            axi_mem.depth
        };
        if depth != axi_mem.depth {
            out.push_str(&format!(
                "    // stands in for the controller: the first {depth} entries of {}.\n",
                axi_mem.depth
            ));
        }
        out.push_str(&format!(
            "    inst u_axi_mem_{bundle}: hns::axi_mem::<{hpkg}> #(\n"
        ));
        out.push_str(&format!("        DEPTH: {depth},\n"));
        // The back door is not used. A real controller has none, so the host
        // would lose access when the stand-in is replaced.
        out.push_str("        BACKDOOR: false,\n");
        out.push_str("    ) (\n");
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        out.push_str(&format!("        axi: saxi_{bundle},\n"));
        out.push_str("        i_haddr: '0,\n");
        out.push_str("        i_hwdata: '0,\n");
        out.push_str("        i_hwstrb: '0,\n");
        out.push_str("        i_hwe: '0,\n");
        out.push_str("        o_hrdata: _,\n");
        out.push_str("    );\n\n");
        // The stand-in has no calibration and is ready from the start. On the
        // real controller this is `init_calib_complete` (`u_calib_*`). Left
        // unassigned, `veryl check` warns, and a warning fails the check.
        if axi_mem.backing == crate::manifest::Backing::Dram {
            out.push_str(&format!(
                "    // A stand-in has nothing to calibrate.\n    assign t_{bundle}_calib = 1;\n\n"
            ));
        }
    }

    // The host_mem stand-in: a BRAM behind a transfer-level interface.
    //
    // A stress stage (`hns::delay`) sits in front. It passes through by
    // default, but can add wait states without a new synthesis. The stand-in
    // is fast and regular, so without it the DUT is never tested against a
    // slow partner on the board.
    for hmem in &plan.host_mems {
        let bundle = &hmem.bundle;
        let dut_side = |port: Option<&String>, width: usize, input: bool| -> String {
            match port {
                Some(name) => format!("w_{name}"),
                None if input => format!("{width}'b0"),
                None => "_".to_string(),
            }
        };
        let read = hmem.read.as_ref();
        let write = hmem.write.as_ref();

        out.push_str(&format!("    inst u_delay_{bundle}: hns::delay #(\n"));
        out.push_str(&format!("        DW: {},\n", hmem.data_width));
        out.push_str(&format!("        ADDR_W: {},\n", hmem.cmd_addr_width));
        out.push_str(&format!("        SIZE_W: {},\n", hmem.cmd_size_width));
        out.push_str("    ) (\n");
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        out.push_str(&format!("        i_delay: t_{bundle}_delay,\n"));
        out.push_str(&format!("        i_jitter: t_{bundle}_jitter,\n"));
        for (port, chosen, width, input) in [
            ("i_d_rd_cmd_valid", read.map(|r| &r.cmd_valid), 1, true),
            ("o_d_rd_cmd_ready", read.map(|r| &r.cmd_ready), 1, false),
            (
                "i_d_rd_cmd_addr",
                read.map(|r| &r.cmd_addr),
                hmem.cmd_addr_width,
                true,
            ),
            (
                "i_d_rd_cmd_size",
                read.map(|r| &r.cmd_size),
                hmem.cmd_size_width,
                true,
            ),
            ("o_d_rd_valid", read.map(|r| &r.valid), 1, false),
            ("i_d_rd_ready", read.map(|r| &r.ready), 1, true),
            ("o_d_rd_data", read.map(|r| &r.data), hmem.data_width, false),
            ("o_d_rd_last", read.map(|r| &r.last), 1, false),
            ("o_d_rd_error", read.map(|r| &r.error), 1, false),
            ("i_d_wr_cmd_valid", write.map(|w| &w.cmd_valid), 1, true),
            ("o_d_wr_cmd_ready", write.map(|w| &w.cmd_ready), 1, false),
            (
                "i_d_wr_cmd_addr",
                write.map(|w| &w.cmd_addr),
                hmem.cmd_addr_width,
                true,
            ),
            (
                "i_d_wr_cmd_size",
                write.map(|w| &w.cmd_size),
                hmem.cmd_size_width,
                true,
            ),
            ("i_d_wr_valid", write.map(|w| &w.valid), 1, true),
            ("o_d_wr_ready", write.map(|w| &w.ready), 1, false),
            ("i_d_wr_data", write.map(|w| &w.data), hmem.data_width, true),
            (
                "i_d_wr_strb",
                write.and_then(|w| w.strb.as_ref()),
                hmem.data_width / 8,
                true,
            ),
            ("i_d_wr_last", write.map(|w| &w.last), 1, true),
            ("o_d_wr_done", write.map(|w| &w.done_valid), 1, false),
            ("o_d_wr_done_error", write.map(|w| &w.done_error), 1, false),
        ] {
            out.push_str(&format!(
                "        {port}: {},\n",
                dut_side(chosen, width, input)
            ));
        }
        for role in [
            "rd_cmd_valid",
            "rd_cmd_ready",
            "rd_cmd_addr",
            "rd_cmd_size",
            "rd_valid",
            "rd_ready",
            "rd_data",
            "rd_last",
            "rd_error",
            "wr_cmd_valid",
            "wr_cmd_ready",
            "wr_cmd_addr",
            "wr_cmd_size",
            "wr_valid",
            "wr_ready",
            "wr_data",
            "wr_strb",
            "wr_last",
            "wr_done",
            "wr_done_error",
        ] {
            // Directions seen from the terminator side. The terminator drives
            // ready, data and done.
            let prefix = match role {
                "rd_cmd_ready" | "rd_valid" | "rd_data" | "rd_last" | "rd_error"
                | "wr_cmd_ready" | "wr_ready" | "wr_done" | "wr_done_error" => "i_t_",
                _ => "o_t_",
            };
            out.push_str(&format!("        {prefix}{role}: h_{bundle}_{role},\n"));
        }
        out.push_str("    );\n\n");

        out.push_str(&format!("    inst u_hmem_{bundle}: hns::hmem #(\n"));
        // If the DUT has a byte strobe, it is carried as is. Without one, each
        // beat is written whole.
        let has_strb = hmem.write.as_ref().is_some_and(|w| w.strb.is_some());
        out.push_str(&format!("        HAS_STRB: {has_strb},\n"));
        out.push_str(&format!("        DW: {},\n", hmem.data_width));
        out.push_str(&format!("        DEPTH: {},\n", hmem.depth));
        out.push_str(&format!("        ADDR_W: {},\n", hmem.cmd_addr_width));
        out.push_str(&format!("        SIZE_W: {},\n", hmem.cmd_size_width));
        out.push_str("    ) (\n");
        out.push_str(&format!("        i_clk: {clk},\n"));
        out.push_str(&format!("        i_rst: {rst},\n"));
        for (port, role) in [
            ("i_rd_cmd_valid", "rd_cmd_valid"),
            ("o_rd_cmd_ready", "rd_cmd_ready"),
            ("i_rd_cmd_addr", "rd_cmd_addr"),
            ("i_rd_cmd_size", "rd_cmd_size"),
            ("o_rd_valid", "rd_valid"),
            ("i_rd_ready", "rd_ready"),
            ("o_rd_data", "rd_data"),
            ("o_rd_last", "rd_last"),
            ("o_rd_error", "rd_error"),
            ("i_wr_cmd_valid", "wr_cmd_valid"),
            ("o_wr_cmd_ready", "wr_cmd_ready"),
            ("i_wr_cmd_addr", "wr_cmd_addr"),
            ("i_wr_cmd_size", "wr_cmd_size"),
            ("i_wr_valid", "wr_valid"),
            ("o_wr_ready", "wr_ready"),
            ("i_wr_data", "wr_data"),
            ("i_wr_last", "wr_last"),
            ("o_wr_done_valid", "wr_done"),
            ("o_wr_done_error", "wr_done_error"),
        ] {
            out.push_str(&format!("        {port}: h_{bundle}_{role},\n"));
        }
        out.push_str(&format!("        i_wr_strb: h_{bundle}_wr_strb,\n"));
        out.push_str(&format!("        i_haddr: t_{bundle}_maddr,\n"));
        out.push_str(&format!("        i_hwdata: t_{bundle}_mdata,\n"));
        out.push_str(&format!("        i_hwe: t_{bundle}_mwe,\n"));
        out.push_str(&format!("        o_hrdata: t_{bundle}_mrdata,\n"));
        out.push_str(&format!("        o_oor: t_{bundle}_oor,\n"));
        // The count of strobes that could not be carried. `bram` always carries
        // them, so a register for it would always read 0.
        out.push_str("        o_widened: _,\n");
        out.push_str("    );\n\n");
    }

    // The DUT, with ports in declaration order. The output is its own Veryl
    // project, so the DUT needs its project name to resolve.
    out.push_str(&format!(
        "    inst u_dut: {}::{} (\n",
        plan.metadata.project.name, plan.dut.name
    ));
    for port in &plan.dut.ports {
        let value = connection(plan, clocks, clk, rst, port);
        out.push_str(&format!("        {}: {value},\n", port.name));
    }
    out.push_str("    );\n");
}

/// `hns/src/uart.veryl`: the heartbeat UART (8N1).
///
/// It tells whether the harness is alive and which bitstream runs, even when
/// the transport does not work. It is a UART, not an LED, because the board
/// is often remote and nobody can see it.
///
/// The magic and the map hash are constants at generation time, so the line
/// needs no hex conversion circuit. Lines that keep arriving show liveness.
pub fn uart_module(plan: &crate::heartbeat::HeartbeatPlan) -> String {
    let bytes: Vec<u8> = plan.message.bytes().collect();
    let mut out = header_veryl();
    out.push_str("///\n/// Harness heartbeat: one identity line per second, 8N1.\n");
    out.push_str("/// Readable without the transport -- `cat /dev/ttyUSB*`.\n");
    out.push_str(&format!(
        "/// Line: {:?}\n",
        plan.message
            .replace("___", "<seq>")
            .replace('\r', "\\r")
            .replace('\n', "\\n")
    ));
    out.push_str(
        "/// `<seq>` counts the lines, so a stopped harness does not look like a running one.\n",
    );
    out.push_str("module uart (\n");
    out.push_str("    i_clk: input  clock,\n");
    out.push_str("    i_rst: input  reset,\n");
    out.push_str("    o_tx : output logic,\n");
    out.push_str(") {\n");
    out.push_str(&format!(
        "    /// Clocks per bit ({} baud from {} Hz).\n    const DIV: u32 = {};\n",
        plan.actual_baud, plan.gap, plan.div
    ));
    out.push_str(&format!(
        "    /// Bytes in the line.\n    const LEN: u32 = {};\n",
        bytes.len()
    ));
    out.push_str(&format!(
        "    /// Idle between lines (about one second).\n    const GAP: u32 = {};\n\n",
        plan.gap
    ));
    out.push_str("    const DIV_W: u32 = $clog2(DIV);\n");
    // Wide enough to hold `GAP` itself. `$clog2(GAP)` is one bit short when GAP
    // is a power of two; then `gap = GAP` becomes 0 and lines get no gap.
    out.push_str("    const GAP_W: u32 = $clog2(GAP + 1);\n");
    out.push_str("    const IDX_W: u32 = $clog2(LEN);\n\n");

    // The counter position: the three bytes `___` placed by `heartbeat::resolve`.
    let seq_at = plan
        .message
        .find("___")
        .expect("the heartbeat line reserves three bytes for the counter");
    out.push_str(&format!(
        "    /// Where the counter's three hex digits go.\n    const SEQ_AT: u32 = {seq_at};\n\n"
    ));

    out.push_str("    /// One hex digit, lower case.\n");
    out.push_str("    function hexdigit (v: input logic<4>) -> logic<8> {\n");
    out.push_str("        if v <: 4'd10 {\n");
    out.push_str("            return 8'h30 + {4'b0, v};\n");
    out.push_str("        } else {\n");
    out.push_str("            return 8'h57 + {4'b0, v};\n");
    out.push_str("        }\n    }\n\n");

    out.push_str("    /// The line. Constant at generation time, so no hex conversion here.\n");
    out.push_str("    function msg (i: input logic<IDX_W>) -> logic<8> {\n");
    out.push_str("        case i {\n");
    for (index, byte) in bytes.iter().enumerate() {
        if index + 1 == bytes.len() {
            out.push_str(&format!("            default: return 8'h{byte:02x};\n"));
        } else {
            out.push_str(&format!("            {index:<6}: return 8'h{byte:02x};\n"));
        }
    }
    out.push_str("        }\n    }\n\n");

    out.push_str("    /// The byte to send: the fixed line, with the counter patched in.\n");
    out.push_str("    ///\n");
    out.push_str("    /// **The counter is what says it is alive.** Without it the same line\n");
    out.push_str("    /// repeats and a stopped harness looks like a running one.\n");
    out.push_str("    function at (i: input logic<IDX_W>, s: input logic<12>) -> logic<8> {\n");
    out.push_str("        case i {\n");
    out.push_str("            SEQ_AT    : return hexdigit(s[11:8]);\n");
    out.push_str("            SEQ_AT + 1: return hexdigit(s[7:4]);\n");
    out.push_str("            SEQ_AT + 2: return hexdigit(s[3:0]);\n");
    out.push_str("            default   : return msg(i);\n");
    out.push_str("        }\n    }\n\n");

    out.push_str("    var busy: logic       ;\n");
    out.push_str("    var sr  : logic<10>   ;\n");
    out.push_str("    var left: logic<4>    ;\n");
    out.push_str("    var div : logic<DIV_W>;\n");
    out.push_str("    var idx : logic<IDX_W>;\n");
    out.push_str("    var gap : logic<GAP_W>;\n");
    out.push_str("    /// Lines sent so far. Wraps after 4096, about an hour at one per second.\n");
    out.push_str("    var seq : logic<12>   ;\n\n");
    out.push_str("    assign o_tx = if busy ? sr[0] : 1'b1;\n\n");
    out.push_str("    always_ff {\n");
    out.push_str("        if_reset {\n");
    out.push_str("            busy = 0;\n            sr   = 10'h3ff;\n            left = 0;\n");
    out.push_str("            div  = 0;\n            idx  = 0;\n            gap  = 0;\n");
    out.push_str("            seq  = 0;\n");
    out.push_str("        } else {\n");
    out.push_str("            if !busy {\n");
    out.push_str("                if gap == 0 {\n");
    out.push_str("                    // {stop, data, start}, sent LSB first.\n");
    out.push_str("                    sr   = {1'b1, at(0, seq), 1'b0};\n");
    out.push_str("                    left = 10;\n                    div  = 0;\n");
    out.push_str("                    idx  = 0;\n                    busy = 1;\n");
    out.push_str(
        "                } else {\n                    gap = gap - 1;\n                }\n",
    );
    out.push_str("            } else if div == DIV - 1 {\n");
    out.push_str("                div  = 0;\n");
    out.push_str("                sr   = {1'b1, sr[9:1]};\n");
    out.push_str("                left = left - 1;\n");
    out.push_str("                if left == 1 {\n");
    out.push_str("                    if idx == LEN - 1 {\n");
    out.push_str("                        busy = 0;\n                        gap  = GAP;\n");
    out.push_str("                        seq  = seq + 1;\n");
    out.push_str("                    } else {\n");
    out.push_str("                        idx  = idx + 1;\n");
    out.push_str("                        sr   = {1'b1, at(idx + 1, seq), 1'b0};\n");
    out.push_str("                        left = 10;\n");
    out.push_str("                    }\n                }\n");
    out.push_str("            } else {\n                div = div + 1;\n            }\n");
    out.push_str("        }\n    }\n}\n");
    out
}

/// Ports terminated by `[pin]`, in DUT declaration order:
/// (port name, resource name, width, domain).
///
/// A port with an unknown width is left out. Emitting it as one bit would be
/// a silent default.
fn pin_ports(plan: &Plan) -> Vec<(String, String, usize, String)> {
    plan.unconnected
        .iter()
        .filter_map(|entry| match &entry.kind {
            UnconnectedKind::Pin { resource, .. } => {
                let port = plan.dut.ports.iter().find(|p| p.name == entry.port)?;
                let width = match port.signals.as_slice() {
                    [signal] => signal.width?,
                    _ => return None,
                };
                Some((
                    entry.port.clone(),
                    resource.clone(),
                    width,
                    port_domain(plan, port),
                ))
            }
            _ => None,
        })
        .collect()
}

/// The clock domain written on a top-level port. It maps the DUT port domain
/// to a clock that the harness makes. Without it Veryl rejects the connection
/// as a CDC: a signal with a known domain would meet a `'_` port.
fn port_domain(plan: &Plan, port: &crate::dut::Port) -> String {
    let Some(clocks) = plan.clocks() else {
        return String::new();
    };
    let domain = port.signals.first().map(|signal| &signal.domain);
    let ident = match domain {
        Some(crate::dut::Domain::Explicit(name)) | Some(crate::dut::Domain::Inferred(name)) => {
            clocks
                .outputs
                .iter()
                .find(|output| output.domain == format!("'{name}"))
                .map(|output| output.ident.clone())
        }
        // A signal with no domain name is known only when there is one clock.
        _ if clocks.outputs.len() == 1 => Some(clocks.window.clone()),
        _ => None,
    };
    match ident {
        Some(ident) => format!("'{ident} "),
        None => String::new(),
    }
}

/// The DRAM pins. Widths come from the target (measured from `mig.prj`).
///
/// `dq` / `dqs` are bidirectional and go to the top as `inout`. The MIG has
/// the IOBUFs, so the harness only passes them through.
pub fn dram_pins(plan: &Plan) -> Vec<(String, &'static str, usize)> {
    let Some(target) = plan.target() else {
        return Vec::new();
    };
    if !has_dram(plan) {
        return Vec::new();
    }
    let Some(dram) = hns_targets::dram(target) else {
        return Vec::new();
    };
    // The PS's fixed pins: the DDR, the MIO and the PS clock and resets. They
    // are the PS7 IP's port names (measured), and its own XDC places them.
    if dram.is_ps7() {
        let mut pins = vec![("MIO".to_string(), "inout", 54)];
        for (name, width) in [
            ("DDR_CAS_n", 1),
            ("DDR_CKE", 1),
            ("DDR_Clk_n", 1),
            ("DDR_Clk", 1),
            ("DDR_CS_n", 1),
            ("DDR_DRSTB", 1),
            ("DDR_ODT", 1),
            ("DDR_RAS_n", 1),
            ("DDR_WEB", 1),
            ("DDR_BankAddr", 3),
            ("DDR_Addr", 15),
            ("DDR_VRN", 1),
            ("DDR_VRP", 1),
            ("DDR_DM", 4),
            ("DDR_DQ", 32),
            ("DDR_DQS_n", 4),
            ("DDR_DQS", 4),
            ("PS_SRSTB", 1),
            ("PS_CLK", 1),
            ("PS_PORB", 1),
        ] {
            pins.push((name.to_string(), "inout", width));
        }
        return pins;
    }
    memory_pins(&dram)
}

/// The memory pins of a MIG or DDR4 controller: (port, direction, width).
///
/// No defaults. `feasibility` already rejects a target that lacks a key, so
/// every key is present here. A default would let a wrong width pass until
/// synthesis.
pub fn memory_pins(dram: &hns_targets::Dram) -> Vec<(String, &'static str, usize)> {
    let int = |value: Option<u32>| {
        value.expect("feasibility refuses a dram target that does not state this") as usize
    };
    let dq = int(dram.width);
    let lanes = (dq / 8).max(1);
    let kind = dram
        .kind
        .as_deref()
        .expect("feasibility refuses a dram target that does not state this");
    // DDR4 is not a variant of DDR3 (measured). `ras_n` / `cas_n` / `we_n`
    // become `act_n`, and bank groups (`bg`) are added. `addr` -> `adr`,
    // `ck_p/n` -> `ck_t/c`, `dqs_p/n` -> `dqs_t/c`, `dm` -> `dm_dbi_n` (an
    // inout). No name matches after a prefix change, so the lists are separate.
    if kind.starts_with("ddr4") {
        // One cs_n, cke, odt and ck per rank. The boards with a board
        // interface are single rank (measured on the generated IP); the others
        // state `ranks`.
        let ranks = if dram.board_interface.is_some() {
            1
        } else {
            int(dram.ranks)
        };
        return vec![
            ("c0_ddr4_adr".to_string(), "output", int(dram.row_bits)),
            ("c0_ddr4_ba".to_string(), "output", int(dram.bank_bits)),
            (
                "c0_ddr4_bg".to_string(),
                "output",
                int(dram.bank_group_bits),
            ),
            ("c0_ddr4_act_n".to_string(), "output", 1),
            ("c0_ddr4_cke".to_string(), "output", ranks),
            ("c0_ddr4_cs_n".to_string(), "output", ranks),
            ("c0_ddr4_odt".to_string(), "output", ranks),
            ("c0_ddr4_reset_n".to_string(), "output", 1),
            ("c0_ddr4_ck_t".to_string(), "output", ranks),
            ("c0_ddr4_ck_c".to_string(), "output", ranks),
            ("c0_ddr4_dq".to_string(), "inout", dq),
            ("c0_ddr4_dqs_t".to_string(), "inout", lanes),
            ("c0_ddr4_dqs_c".to_string(), "inout", lanes),
            ("c0_ddr4_dm_dbi_n".to_string(), "inout", lanes),
        ];
    }
    // For `ddr3l` too, the MIG port names are `ddr3_*`.
    vec![
        ("ddr3_addr".to_string(), "output", int(dram.row_bits)),
        ("ddr3_ba".to_string(), "output", int(dram.bank_bits)),
        ("ddr3_ras_n".to_string(), "output", 1),
        ("ddr3_cas_n".to_string(), "output", 1),
        ("ddr3_we_n".to_string(), "output", 1),
        ("ddr3_reset_n".to_string(), "output", 1),
        ("ddr3_ck_p".to_string(), "output", 1),
        ("ddr3_ck_n".to_string(), "output", 1),
        ("ddr3_cke".to_string(), "output", 1),
        ("ddr3_cs_n".to_string(), "output", 1),
        ("ddr3_dm".to_string(), "output", lanes),
        ("ddr3_odt".to_string(), "output", 1),
        ("ddr3_dq".to_string(), "inout", dq),
        ("ddr3_dqs_p".to_string(), "inout", lanes),
        ("ddr3_dqs_n".to_string(), "inout", lanes),
    ]
}

/// Places the Zynq PS and reaches its memory through S_AXI_HP0.
///
/// Unlike a MIG, the PS has no clock to give: HP0 runs on the harness clock,
/// so there is no crossing. Vivado's converter turns AXI4 into the AXI3 that
/// HP0 takes (16-beat bursts, WID). The window is `2^axi_addr_bits` bytes at
/// the PS address `base`, which `feasibility` has made aligned, so the high
/// address bits are just set.
///
/// Nothing passes until the PS is set up. Touching its DDR before then hangs
/// the bus until a power cycle (measured), so valid and ready wait for
/// FCLK_RESET0_N, which rises when the host's ps7_init releases the PL reset.
fn ps7_instance(
    out: &mut String,
    plan: &Plan,
    axi_mem: &crate::terminator::AxiMemPlan,
    domain: &str,
) {
    let bundle = &axi_mem.bundle;
    let host = plan.board().clocks.window.clone();
    let controller = controller_axi(plan, axi_mem);
    let cpkg = &controller.pkg;
    let aw = controller.addr_width;
    let dw = controller.data_bytes * 8;
    let idw = controller.id_width;
    let base = target_dram(plan)
        .and_then(|d| d.base)
        .expect("feasibility refuses a ps7 target without `base`");
    let high = format!("{}'h{:x}", 32 - aw, base >> aw);

    // Convert the width when it differs, as for a MIG.
    let into = if controller.needs_dw {
        out.push_str(&format!(
            "    inst waxi_{bundle}: {domain}$std::axi4_if::<{cpkg}>;\n\n"
        ));
        out.push_str(&format!(
            "    inst u_dw_{bundle}: hns::axi_dw::<\n        {},\n        {cpkg},\n    > (\n",
            host_pkg(plan, axi_mem).0
        ));
        out.push_str(&format!("        i_clk: clk_{host},\n"));
        out.push_str(&format!("        i_rst: rst_{host},\n"));
        out.push_str(&format!("        s_axi: saxi_{bundle},\n"));
        out.push_str(&format!("        m_axi: waxi_{bundle},\n"));
        out.push_str("    );\n\n");
        format!("waxi_{bundle}")
    } else {
        format!("saxi_{bundle}")
    };

    // From the converter. They go through the gate, not straight on.
    let back = [
        ("awready", 1),
        ("wready", 1),
        ("bvalid", 1),
        ("bresp", 2),
        ("bid", idw),
        ("arready", 1),
        ("rvalid", 1),
        ("rlast", 1),
        ("rresp", 2),
        ("rid", idw),
        ("rdata", dw),
    ];
    for (name, width) in back {
        out.push_str(&format!(
            "    var pc_{name}_{bundle}: {domain}logic<{width}>;\n"
        ));
    }
    // AXI3 between the converter and HP0.
    let hp = [
        ("awid", idw),
        ("awaddr", 32),
        ("awlen", 4),
        ("awsize", 3),
        ("awburst", 2),
        ("awlock", 2),
        ("awcache", 4),
        ("awprot", 3),
        ("awqos", 4),
        ("awvalid", 1),
        ("awready", 1),
        ("wid", idw),
        ("wdata", dw),
        ("wstrb", dw / 8),
        ("wlast", 1),
        ("wvalid", 1),
        ("wready", 1),
        ("bid", idw),
        ("bresp", 2),
        ("bvalid", 1),
        ("bready", 1),
        ("arid", idw),
        ("araddr", 32),
        ("arlen", 4),
        ("arsize", 3),
        ("arburst", 2),
        ("arlock", 2),
        ("arcache", 4),
        ("arprot", 3),
        ("arqos", 4),
        ("arvalid", 1),
        ("arready", 1),
        ("rid", idw),
        ("rdata", dw),
        ("rresp", 2),
        ("rlast", 1),
        ("rvalid", 1),
        ("rready", 1),
    ];
    for (name, width) in hp {
        out.push_str(&format!(
            "    var hp_{name}_{bundle}: {domain}logic<{width}>;\n"
        ));
    }
    // FCLK_RESET0_N comes from the PS, on no clock of the harness.
    out.push_str(&format!("    var ps_ready_{bundle}: logic;\n\n"));

    // Nothing passes until the PS is set up. With ready low, valid waits.
    out.push_str("    always_comb {\n");
    for name in ["awready", "wready", "arready", "bvalid", "rvalid"] {
        out.push_str(&format!(
            "        {into}.{name} = pc_{name}_{bundle} & t_{bundle}_calib;\n"
        ));
    }
    for name in ["bresp", "bid", "rlast", "rresp", "rid", "rdata"] {
        out.push_str(&format!("        {into}.{name} = pc_{name}_{bundle};\n"));
    }
    for name in ["buser", "ruser"] {
        out.push_str(&format!("        {into}.{name} = 0;\n"));
    }
    out.push_str("    }\n\n");

    let asserted_value = asserted(plan.metadata.build.reset_type);
    let aresetn = if asserted_value == 0 {
        format!("rst_{host}")
    } else {
        format!("~rst_{host}")
    };
    let gated = |sig: &str| format!("{into}.{sig} & t_{bundle}_calib");
    out.push_str("    // Vivado black boxes, so the analyser cannot check them.\n");
    out.push_str("    unsafe (cdc) {\n");
    out.push_str(&format!(
        "        inst u_pc_{bundle}: $sv::{AXI_PC_MODULE} (\n"
    ));
    out.push_str(&format!("            aclk   : clk_{host},\n"));
    out.push_str(&format!("            aresetn: {aresetn},\n"));
    for (port, expr) in [
        ("s_axi_awid", format!("{into}.awid")),
        (
            "s_axi_awaddr",
            format!("{{{high}, {into}.awaddr[{}:0]}}", aw - 1),
        ),
        ("s_axi_awlen", format!("{into}.awlen")),
        ("s_axi_awsize", format!("{into}.awsize")),
        ("s_axi_awburst", format!("{into}.awburst")),
        ("s_axi_awlock", format!("{into}.awlock")),
        ("s_axi_awcache", format!("{into}.awcache")),
        ("s_axi_awprot", format!("{into}.awprot")),
        ("s_axi_awregion", format!("{into}.awregion")),
        ("s_axi_awqos", format!("{into}.awqos")),
        ("s_axi_awvalid", gated("awvalid")),
        ("s_axi_awready", format!("pc_awready_{bundle}")),
        ("s_axi_wdata", format!("{into}.wdata")),
        ("s_axi_wstrb", format!("{into}.wstrb")),
        ("s_axi_wlast", format!("{into}.wlast")),
        ("s_axi_wvalid", gated("wvalid")),
        ("s_axi_wready", format!("pc_wready_{bundle}")),
        ("s_axi_bid", format!("pc_bid_{bundle}")),
        ("s_axi_bresp", format!("pc_bresp_{bundle}")),
        ("s_axi_bvalid", format!("pc_bvalid_{bundle}")),
        ("s_axi_bready", gated("bready")),
        ("s_axi_arid", format!("{into}.arid")),
        (
            "s_axi_araddr",
            format!("{{{high}, {into}.araddr[{}:0]}}", aw - 1),
        ),
        ("s_axi_arlen", format!("{into}.arlen")),
        ("s_axi_arsize", format!("{into}.arsize")),
        ("s_axi_arburst", format!("{into}.arburst")),
        ("s_axi_arlock", format!("{into}.arlock")),
        ("s_axi_arcache", format!("{into}.arcache")),
        ("s_axi_arprot", format!("{into}.arprot")),
        ("s_axi_arregion", format!("{into}.arregion")),
        ("s_axi_arqos", format!("{into}.arqos")),
        ("s_axi_arvalid", gated("arvalid")),
        ("s_axi_arready", format!("pc_arready_{bundle}")),
        ("s_axi_rid", format!("pc_rid_{bundle}")),
        ("s_axi_rdata", format!("pc_rdata_{bundle}")),
        ("s_axi_rresp", format!("pc_rresp_{bundle}")),
        ("s_axi_rlast", format!("pc_rlast_{bundle}")),
        ("s_axi_rvalid", format!("pc_rvalid_{bundle}")),
        ("s_axi_rready", gated("rready")),
    ] {
        out.push_str(&format!("            {port}: {expr},\n"));
    }
    for (name, _) in hp {
        out.push_str(&format!("            m_axi_{name}: hp_{name}_{bundle},\n"));
    }
    out.push_str("        );\n");

    out.push_str(&format!(
        "        inst u_ps7_{bundle}: $sv::{PS7_MODULE} (\n"
    ));
    for (name, _dir, _width) in dram_pins(plan) {
        out.push_str(&format!("            {name}: {name},\n"));
    }
    // The board's USB0 is in the PS settings; its PL-side ports are not used.
    out.push_str("            USB0_PORT_INDCTL   : _,\n");
    out.push_str("            USB0_VBUS_PWRSELECT: _,\n");
    out.push_str("            USB0_VBUS_PWRFAULT : 1'b0,\n");
    out.push_str(&format!(
        "            FCLK_RESET0_N      : ps_ready_{bundle},\n"
    ));
    out.push_str(&format!("            S_AXI_HP0_ACLK     : clk_{host},\n"));
    for name in ["RDISSUECAP1_EN", "WRISSUECAP1_EN"] {
        out.push_str(&format!("            S_AXI_HP0_{name}: 1'b0,\n"));
    }
    for name in ["RCOUNT", "WCOUNT", "RACOUNT", "WACOUNT"] {
        out.push_str(&format!("            S_AXI_HP0_{name}: _,\n"));
    }
    for (name, _) in hp {
        out.push_str(&format!(
            "            S_AXI_HP0_{}: hp_{name}_{bundle},\n",
            name.to_uppercase()
        ));
    }
    out.push_str("        );\n\n");

    // Bring the PS's "set up" to the window clock: it gates the bus, and the
    // host reads it as `<bundle>_calib`.
    out.push_str(&format!(
        "        inst u_calib_{bundle}: $std::synchronizer_basic #(\n            WIDTH: 1,\n        ) (\n"
    ));
    out.push_str(&format!("            i_clk: clk_{host},\n"));
    out.push_str(&format!("            i_rst: rst_{host},\n"));
    out.push_str(&format!("            i_d: ps_ready_{bundle},\n"));
    out.push_str(&format!("            o_d: t_{bundle}_calib,\n"));
    out.push_str("        );\n    }\n\n");
}

/// Places the Vivado MIG and connects AXI4 to it through a CDC.
///
/// The ports were measured. `s_axi_*` are flat ports with no `region` and no
/// `user`; those are dropped on the interface side.
///
/// Nothing passes until calibration ends: valid and ready are held until
/// `init_calib_complete`. Calibration takes tens of ms. A host access to the
/// region in that time makes the window wait; no value is corrupted.
fn mig_instance(
    out: &mut String,
    plan: &Plan,
    axi_mem: &crate::terminator::AxiMemPlan,
    domain: &str,
) {
    let bundle = &axi_mem.bundle;
    let reset_type = reset_type_name(plan.metadata.build.reset_type);
    let asserted_value = asserted(plan.metadata.build.reset_type);
    let host = plan.board().clocks.window.clone();
    let controller = controller_axi(plan, axi_mem);
    let (cpkg, needs_dw) = (&controller.pkg, controller.needs_dw);
    // Widths are the controller's. `uaxi_*` is past the width converter, so
    // DUT widths would be wrong.
    let aw = controller.addr_width;
    let dw = controller.data_bytes * 8;
    let idw = controller.id_width;

    // The controller domain. The MIG makes this clock.
    out.push_str(&format!("    var mig_clk_{bundle}: 'mig clock;\n"));
    out.push_str(&format!("    var mig_srst_{bundle}: 'mig logic;\n"));
    out.push_str(&format!("    var mig_rst_{bundle}: 'mig {reset_type};\n"));
    out.push_str(&format!("    var mig_calib_{bundle}: 'mig logic;\n"));
    out.push_str(&format!(
        "    inst uaxi_{bundle}: 'mig $std::axi4_if::<{cpkg}>;\n"
    ));
    // Signals from the MIG. They go through a gate, not straight to the
    // interface.
    for (name, width) in [
        ("awready", 1),
        ("wready", 1),
        ("bvalid", 1),
        ("bresp", 2),
        ("bid", idw),
        ("arready", 1),
        ("rvalid", 1),
        ("rlast", 1),
        ("rresp", 2),
        ("rid", idw),
        ("rdata", dw),
    ] {
        out.push_str(&format!(
            "    var mig_{name}_{bundle}: 'mig logic<{width}>;\n"
        ));
    }
    // ui_clk_sync_rst is synchronous and active high. Convert it to the
    // project polarity.
    let released = if asserted_value == 0 { "~" } else { "" };
    out.push_str(&format!(
        "    assign mig_rst_{bundle} = {released}mig_srst_{bundle} as {reset_type};\n\n"
    ));

    // Convert the width when it differs. The MIG does not accept narrow
    // bursts, so a narrow master cannot use it directly.
    let into_cdc = if needs_dw {
        out.push_str(&format!(
            "    inst waxi_{bundle}: {domain}$std::axi4_if::<{cpkg}>;\n\n"
        ));
        out.push_str(&format!(
            "    inst u_dw_{bundle}: hns::axi_dw::<\n        {},\n        {cpkg},\n    > (\n",
            host_pkg(plan, axi_mem).0
        ));
        out.push_str(&format!("        i_clk: clk_{host},\n"));
        out.push_str(&format!("        i_rst: rst_{host},\n"));
        out.push_str(&format!("        s_axi: saxi_{bundle},\n"));
        out.push_str(&format!("        m_axi: waxi_{bundle},\n"));
        out.push_str("    );\n\n");
        format!("waxi_{bundle}")
    } else {
        format!("saxi_{bundle}")
    };

    // The card-issued DMA path. It is arbitrated at the controller width. The
    // DMA engine master is fixed at 256 bits and cannot join the arbiter of
    // the DUT and the window (DUT bus width). The DUT path does not change.
    let into_cdc = if has_pcie(plan) && !dma_registers(&plan.registers).is_empty() {
        let dpkg = format!("$std::axi4_pkg::<{aw}, {DMA_BUS_BYTES}, {idw}, 1, 1, 1, 1, 1>");
        out.push_str(&format!(
            "    inst qaxi_{bundle}: {domain}$std::axi4_if::<{dpkg}>;\n"
        ));
        // No width converter when the DMA bus has the controller width.
        let dma_side = if dw == DMA_BUS_BYTES * 8 {
            format!("qaxi_{bundle}")
        } else {
            out.push_str(&format!(
                "    inst daxi_{bundle}: {domain}$std::axi4_if::<{cpkg}>;\n"
            ));
            out.push_str(&format!(
                "    inst u_ddw_{bundle}: hns::axi_dw::<\n        {dpkg},\n        {cpkg},\n    > (\n"
            ));
            out.push_str(&format!("        i_clk: clk_{host},\n"));
            out.push_str(&format!("        i_rst: rst_{host},\n"));
            out.push_str(&format!("        s_axi: qaxi_{bundle},\n"));
            out.push_str(&format!("        m_axi: daxi_{bundle},\n"));
            out.push_str("    );\n\n");
            format!("daxi_{bundle}")
        };
        // The arbiter's output. Not `raxi_`: that is the range check's output,
        // and a bundle with `dut_base` has both.
        out.push_str(&format!(
            "    inst paxi_{bundle}: {domain}$std::axi4_if::<{cpkg}>;\n"
        ));
        // Wires between the DMA engine and the hard block. They are declared
        // first; `veryl check` rejects a name used before its declaration.
        for (signal, width) in RQ_STREAM {
            out.push_str(&format!(
                "    var h_rq_{signal}: {domain}{};\n",
                logic(width)
            ));
        }
        // Wires between the crossing output and the TLP buffer stage (hard
        // block clock side).
        for (signal, width) in RQ_STREAM {
            out.push_str(&format!(
                "    var p_rq_{signal}: '{PCIE_DOMAIN} {};\n",
                logic(width)
            ));
        }
        out.push('\n');

        // Two RQ streams leave, and the write engine merges them into one. The
        // borrowed code works this way, so no stream arbiter is needed.
        for (signal, width) in RQ_STREAM {
            out.push_str(&format!(
                "    var j_rq_{signal}: {domain}{};\n",
                logic(width)
            ));
        }
        for (signal, width) in RC_STREAM {
            out.push_str(&format!(
                "    var h_rc_{signal}: {domain}{};\n",
                logic(width)
            ));
        }
        out.push('\n');

        // Read and write connect to one interface separately. AXI4 channels
        // are independent, so with `read_master` and `write_master` the two
        // DMA engines form one master, with one arbiter and one converter.
        out.push_str(&format!("    inst u_dma_wr: hns::dma_wr::<{dpkg}> (\n"));
        out.push_str(&format!("        i_clk: clk_{host},\n"));
        out.push_str(&format!("        i_rst: rst_{host},\n"));
        for (port, wire) in [
            ("i_desc_pcie_addr", "dma_desc_pcie_addr"),
            ("i_desc_axi_addr", "dma_desc_axi_addr"),
            ("i_desc_len", "dma_desc_len"),
            ("i_desc_tag", "dma_desc_tag"),
            ("i_desc_valid", "dma_wr_valid"),
            ("o_desc_ready", "dma_wr_ready"),
            ("o_status_tag", "_"),
            ("o_status_error", "dma_wr_status_error"),
            ("o_status_valid", "dma_wr_status_valid"),
        ] {
            out.push_str(&format!("        {port}: {wire},\n"));
        }
        out.push_str(&format!("        m_axi: qaxi_{bundle},\n"));
        for (signal, _width) in RQ_STREAM {
            let d = if signal == "tready" { "i" } else { "o" };
            out.push_str(&format!("        {d}_rq_{signal}: h_rq_{signal},\n"));
        }
        for (signal, _width) in RQ_STREAM {
            let d = if signal == "tready" { "o" } else { "i" };
            out.push_str(&format!("        {d}_join_{signal}: j_rq_{signal},\n"));
        }
        out.push_str("        i_max_payload: pcie_max_payload,\n");
        out.push_str("        i_limit: t_dma_mps_limit,\n");
        out.push_str("        o_mps: dma_mps_used,\n");
        // `hns::dma_gate` tracks busy.
        out.push_str("        o_busy: _,\n");
        out.push_str("    );\n\n");

        out.push_str(&format!("    inst u_dma_rd: hns::dma_rd::<{dpkg}> (\n"));
        out.push_str(&format!("        i_clk: clk_{host},\n"));
        out.push_str(&format!("        i_rst: rst_{host},\n"));
        for (port, wire) in [
            ("i_desc_pcie_addr", "dma_desc_pcie_addr"),
            ("i_desc_axi_addr", "dma_desc_axi_addr"),
            ("i_desc_len", "dma_desc_len"),
            ("i_desc_tag", "dma_desc_tag"),
            ("i_desc_valid", "dma_rd_valid"),
            ("o_desc_ready", "dma_rd_ready"),
            ("o_status_tag", "_"),
            ("o_status_error", "dma_rd_status_error"),
            ("o_status_valid", "dma_rd_status_valid"),
        ] {
            out.push_str(&format!("        {port}: {wire},\n"));
        }
        out.push_str(&format!("        m_axi: qaxi_{bundle},\n"));
        for (signal, _width) in RQ_STREAM {
            let d = if signal == "tready" { "i" } else { "o" };
            out.push_str(&format!("        {d}_rq_{signal}: j_rq_{signal},\n"));
        }
        for (signal, _width) in RC_STREAM {
            let d = if signal == "tready" { "o" } else { "i" };
            out.push_str(&format!("        {d}_rc_{signal}: h_rc_{signal},\n"));
        }
        out.push_str("        i_max_read_req: pcie_max_read_req,\n");
        out.push_str("        i_limit: t_dma_mrrs_limit,\n");
        out.push_str("        o_mrrs: dma_mrrs_used,\n");
        // Completions dropped by the borrowed code. The host compares the
        // counts before and after a transfer.
        out.push_str("        o_cor_count: t_dma_rc_cor,\n");
        out.push_str("        o_uncor_count: t_dma_rc_uncor,\n");
        out.push_str("        o_busy: _,\n");
        out.push_str("    );\n\n");

        // This is the only crossing. The DMA engine is on the harness side, so
        // only the TLP stream goes to the hard block (`hns::tlp_cdc`).
        // The depth must hold a whole TLP. With 8 entries (256 bytes), a TLP
        // with a 256-byte payload (9 beats with the header) did not fit, and
        // transfers got 12% slower even with half as many TLPs (measured).
        // PCIe allows 4096-byte payloads, but that needs BRAM; the depth is
        // sized for two 512-byte TLPs, the payload negotiated today.
        out.push_str(
            "    inst u_rq_cdc: hns::tlp_cdc #(\n        DEPTH : 64,\n        USER_W: 60,\n    ) (\n",
        );
        out.push_str(&format!("        is_clk: clk_{host},\n"));
        out.push_str(&format!("        is_rst: rst_{host},\n"));
        for (signal, _width) in RQ_STREAM {
            let d = if signal == "tready" { "os" } else { "is" };
            out.push_str(&format!("        {d}_{signal}: h_rq_{signal},\n"));
        }
        out.push_str("        id_clk: pcie_user_clk,\n");
        out.push_str("        id_rst: pcie_user_rst,\n");
        for (signal, _width) in RQ_STREAM {
            let d = if signal == "tready" { "id" } else { "od" };
            out.push_str(&format!("        {d}_{signal}: p_rq_{signal},\n"));
        }
        out.push_str("    );\n\n");

        // Buffer each whole TLP before sending it. On RQ, a `tvalid` drop in
        // the middle of a TLP makes the hard block nullify it (PG213). The DMA
        // engine streams while it reads DRAM, so a gap in R would leave a gap
        // in the TLP (`hns::tlp_hold`).
        out.push_str(
            "    inst u_rq_hold: hns::tlp_hold #(\n        DEPTH : 64,\n        USER_W: 60,\n    ) (\n",
        );
        out.push_str("        i_clk: pcie_user_clk,\n");
        out.push_str("        i_rst: pcie_user_rst,\n");
        for (signal, _width) in RQ_STREAM {
            let d = if signal == "tready" { "o" } else { "i" };
            out.push_str(&format!("        {d}_{signal}: p_rq_{signal},\n"));
        }
        for (signal, _width) in RQ_STREAM {
            let d = if signal == "tready" { "i" } else { "o" };
            out.push_str(&format!("        {d}_{signal}: rq_{signal},\n"));
        }
        out.push_str("        o_gaps_gray: pcie_rq_gaps_gray,\n");
        out.push_str("    );\n\n");

        // Completions must always be taken, or the hard block stalls. So this
        // must be a FIFO, not a handshake. The depth holds a whole TLP, as
        // for RQ.
        out.push_str(
            "    inst u_rc_cdc: hns::tlp_cdc #(\n        DEPTH : 64,\n        USER_W: 75,\n    ) (\n",
        );
        out.push_str("        is_clk: pcie_user_clk,\n");
        out.push_str("        is_rst: pcie_user_rst,\n");
        for (signal, _width) in RC_STREAM {
            let d = if signal == "tready" { "os" } else { "is" };
            out.push_str(&format!("        {d}_{signal}: rc_{signal},\n"));
        }
        out.push_str(&format!("        id_clk: clk_{host},\n"));
        out.push_str(&format!("        id_rst: rst_{host},\n"));
        for (signal, _width) in RC_STREAM {
            let d = if signal == "tready" { "id" } else { "od" };
            out.push_str(&format!("        {d}_{signal}: h_rc_{signal},\n"));
        }
        out.push_str("    );\n\n");

        // The DUT and the window are m0, the DMA engine m1. The order has no
        // meaning, but it is fixed so that diffs of the output stay readable.
        out.push_str(&format!(
            "    inst u_drr_{bundle}: hns::axi_rr::<{cpkg}> (\n"
        ));
        out.push_str(&format!("        i_clk: clk_{host},\n"));
        out.push_str(&format!("        i_rst: rst_{host},\n"));
        out.push_str(&format!("        m0: {into_cdc},\n"));
        out.push_str(&format!("        m1: {dma_side},\n"));
        out.push_str(&format!("        s: paxi_{bundle},\n"));
        out.push_str("    );\n\n");
        format!("paxi_{bundle}")
    } else {
        into_cdc
    };

    out.push_str(&format!(
        "    inst u_cdc_{bundle}: hns::axi_cdc::<{cpkg}> #(\n        DEPTH: 8,\n    ) (\n"
    ));
    out.push_str(&format!(
        "        is_clk: clk_{},\n",
        plan.board().clocks.window
    ));
    out.push_str(&format!(
        "        is_rst: rst_{},\n",
        plan.board().clocks.window
    ));
    out.push_str(&format!("        s_axi: {into_cdc},\n"));
    out.push_str(&format!("        id_clk: mig_clk_{bundle},\n"));
    out.push_str(&format!("        id_rst: mig_rst_{bundle},\n"));
    out.push_str(&format!("        m_axi: uaxi_{bundle},\n"));
    out.push_str("    );\n\n");

    // Nothing passes until calibration ends. With ready low, valid waits.
    out.push_str("    always_comb {\n");
    for name in ["awready", "wready", "arready", "bvalid", "rvalid"] {
        out.push_str(&format!(
            "        uaxi_{bundle}.{name} = mig_{name}_{bundle} & mig_calib_{bundle};\n"
        ));
    }
    for name in ["bresp", "bid", "rlast", "rresp", "rid", "rdata"] {
        out.push_str(&format!(
            "        uaxi_{bundle}.{name} = mig_{name}_{bundle};\n"
        ));
    }
    // Fields the MIG does not have. They are tied off here so that the drop is
    // explicit.
    for name in ["buser", "ruser"] {
        out.push_str(&format!("        uaxi_{bundle}.{name} = 0;\n"));
    }
    out.push_str("    }\n\n");

    out.push_str("    // The controller is a Vivado black box, so this crossing cannot be\n");
    out.push_str("    // checked by the analyser. hns::axi_cdc is the crossing that matters.\n");
    out.push_str("    unsafe (cdc) {\n");
    // UltraScale+ DDR4 has other port names (measured): `c0_ddr4_*` on the
    // memory side, `c0_ddr4_s_axi_*` for AXI, and it takes its own clock.
    let ddr4 = is_ddr4(plan);
    let p = if ddr4 { "c0_ddr4_" } else { "" };
    out.push_str(&format!(
        "        inst u_mig_{bundle}: $sv::{MIG_MODULE} (\n"
    ));
    for (name, _dir, _width) in dram_pins(plan) {
        // The top-level ports use the IP port names (see `dram_pins`).
        out.push_str(&format!("            {name}: {name},\n"));
    }
    if ddr4 {
        out.push_str(&format!(
            "            c0_init_calib_complete: mig_calib_{bundle},\n"
        ));
        out.push_str(&format!("            c0_ddr4_ui_clk: mig_clk_{bundle},\n"));
        out.push_str(&format!(
            "            c0_ddr4_ui_clk_sync_rst: mig_srst_{bundle},\n"
        ));
        out.push_str(&format!(
            "            c0_ddr4_aresetn: ~mig_srst_{bundle},\n"
        ));
        // The control ports and the interrupt exist only with ECC (measured).
        // The board default is 64 bits without ECC; connecting them fails
        // synthesis with "no such port".
    } else {
        out.push_str(&format!(
            "            init_calib_complete: mig_calib_{bundle},\n"
        ));
        out.push_str(&format!("            ui_clk: mig_clk_{bundle},\n"));
        out.push_str(&format!(
            "            ui_clk_sync_rst: mig_srst_{bundle},\n"
        ));
        out.push_str("            mmcm_locked: _,\n");
        out.push_str(&format!("            aresetn: ~mig_srst_{bundle},\n"));
        for name in ["app_sr_req", "app_ref_req", "app_zq_req"] {
            out.push_str(&format!("            {name}: 1'b0,\n"));
        }
        for name in ["app_sr_active", "app_ref_ack", "app_zq_ack"] {
            out.push_str(&format!("            {name}: _,\n"));
        }
    }
    // Write and read address and data. `region` and `user` are not passed.
    let gated = |sig: &str| format!("uaxi_{bundle}.{sig} & mig_calib_{bundle}");
    for (port, expr) in [
        ("s_axi_awid", format!("uaxi_{bundle}.awid")),
        (
            "s_axi_awaddr",
            format!("uaxi_{bundle}.awaddr[{}:0]", aw - 1),
        ),
        ("s_axi_awlen", format!("uaxi_{bundle}.awlen")),
        ("s_axi_awsize", format!("uaxi_{bundle}.awsize")),
        ("s_axi_awburst", format!("uaxi_{bundle}.awburst")),
        ("s_axi_awlock", format!("uaxi_{bundle}.awlock")),
        ("s_axi_awcache", format!("uaxi_{bundle}.awcache")),
        ("s_axi_awprot", format!("uaxi_{bundle}.awprot")),
        ("s_axi_awqos", format!("uaxi_{bundle}.awqos")),
        ("s_axi_awvalid", gated("awvalid")),
        ("s_axi_awready", format!("mig_awready_{bundle}")),
        ("s_axi_wdata", format!("uaxi_{bundle}.wdata")),
        ("s_axi_wstrb", format!("uaxi_{bundle}.wstrb")),
        ("s_axi_wlast", format!("uaxi_{bundle}.wlast")),
        ("s_axi_wvalid", gated("wvalid")),
        ("s_axi_wready", format!("mig_wready_{bundle}")),
        ("s_axi_bid", format!("mig_bid_{bundle}")),
        ("s_axi_bresp", format!("mig_bresp_{bundle}")),
        ("s_axi_bvalid", format!("mig_bvalid_{bundle}")),
        ("s_axi_bready", gated("bready")),
        ("s_axi_arid", format!("uaxi_{bundle}.arid")),
        (
            "s_axi_araddr",
            format!("uaxi_{bundle}.araddr[{}:0]", aw - 1),
        ),
        ("s_axi_arlen", format!("uaxi_{bundle}.arlen")),
        ("s_axi_arsize", format!("uaxi_{bundle}.arsize")),
        ("s_axi_arburst", format!("uaxi_{bundle}.arburst")),
        ("s_axi_arlock", format!("uaxi_{bundle}.arlock")),
        ("s_axi_arcache", format!("uaxi_{bundle}.arcache")),
        ("s_axi_arprot", format!("uaxi_{bundle}.arprot")),
        ("s_axi_arqos", format!("uaxi_{bundle}.arqos")),
        ("s_axi_arvalid", gated("arvalid")),
        ("s_axi_arready", format!("mig_arready_{bundle}")),
        ("s_axi_rid", format!("mig_rid_{bundle}")),
        ("s_axi_rdata", format!("mig_rdata_{bundle}")),
        ("s_axi_rresp", format!("mig_rresp_{bundle}")),
        ("s_axi_rlast", format!("mig_rlast_{bundle}")),
        ("s_axi_rvalid", format!("mig_rvalid_{bundle}")),
        ("s_axi_rready", gated("rready")),
    ] {
        out.push_str(&format!("            {p}{port}: {expr},\n"));
    }
    // The clock source depends on the board. If the target gives the
    // controller its own clock pins, connect them; otherwise use MMCM outputs.
    match plan.clocks().and_then(|c| c.controller_clock.as_ref()) {
        Some(clock) => {
            let ports = controller_clock_ports(clock);
            if clock.diff {
                out.push_str(&format!("            c0_sys_clk_p: {},\n", ports[0]));
                out.push_str(&format!("            c0_sys_clk_n: {},\n", ports[1]));
            } else {
                out.push_str(&format!("            c0_sys_clk_i: {},\n", ports[0]));
            }
        }
        None => {
            out.push_str("            sys_clk_i: clk_migsys,\n");
            out.push_str("            clk_ref_i: clk_migref,\n");
        }
    }
    // Release the reset only after the MMCM locks. With the board reset
    // connected directly, the MIG starts when the button is released, before
    // `sys_clk_i` runs. `rst_migsys` is made from `locked`.
    // Without an MMCM `migsys` clock, pass the reset of the main harness
    // domain (the IP takes it asynchronously).
    let rst_source = if plan
        .clocks()
        .and_then(|c| c.controller_clock.as_ref())
        .is_some()
    {
        format!("rst_{host}")
    } else {
        "rst_migsys".to_string()
    };
    // The polarity comes from the target. The Arty MIG has `RST_ACT_LOW = 1`;
    // the UltraScale+ DDR4 IP metadata says `POLARITY = ACTIVE_HIGH` (measured).
    let active_high = target_dram(plan).and_then(|d| d.sys_rst_active).as_deref() == Some("high");
    // The harness reset has the `asserted_value` polarity. Convert it for the IP.
    let asserted_high = asserted_value != 0;
    let sys_rst = if active_high == asserted_high {
        rst_source
    } else {
        format!("~{rst_source}")
    };
    out.push_str(&format!("            sys_rst: {sys_rst},\n"));
    out.push_str("        );\n    }\n\n");

    // Bring the calibration flag to the window clock, so the host can see why
    // accesses wait.
    let ident = &plan.board().clocks.window;
    out.push_str("    unsafe (cdc) {\n");
    out.push_str(&format!(
        "        inst u_calib_{bundle}: $std::synchronizer_basic #(\n            WIDTH: 1,\n        ) (\n"
    ));
    out.push_str(&format!("            i_clk: clk_{ident},\n"));
    out.push_str(&format!("            i_rst: rst_{ident},\n"));
    out.push_str(&format!("            i_d: mig_calib_{bundle},\n"));
    out.push_str(&format!("            o_d: t_{bundle}_calib,\n"));
    out.push_str("        );\n    }\n\n");
}

/// The AXI4 package of the arbiter and the host port. The address width is
/// the controller's.
///
/// Even when the DUT port is narrow, the host must reach the whole memory:
/// the upper row and bank pins never toggle until someone accesses there.
/// Only the address is widened; data and ID widths stay the DUT's
/// (`hns::axi_dw` downstream converts them).
///
/// The second value says whether the DUT address must be widened.
fn host_pkg(plan: &Plan, axi_mem: &crate::terminator::AxiMemPlan) -> (String, bool) {
    if axi_mem.backing != crate::manifest::Backing::Dram {
        return (axi_mem.pkg.clone(), false);
    }
    // `hns::axi_range` sets the width itself, wider or narrower: what passes
    // it is inside the memory.
    if axi_mem.dut_base != 0
        && let Some(addr) = target_dram(plan).and_then(|d| d.axi_addr_bits)
    {
        return (
            format!(
                "$std::axi4_pkg::<{addr}, {}, {}, 1, 1, 1, 1, 1>",
                axi_mem.data_bytes, axi_mem.id_width
            ),
            false,
        );
    }
    match target_dram(plan).and_then(|d| d.axi_addr_bits) {
        Some(addr) if addr > axi_mem.addr_width => (
            format!(
                "$std::axi4_pkg::<{addr}, {}, {}, 1, 1, 1, 1, 1>",
                axi_mem.data_bytes, axi_mem.id_width
            ),
            true,
        ),
        _ => (axi_mem.pkg.clone(), false),
    }
}

/// The AXI4 type on the controller side.
struct ControllerAxi {
    /// `std::axi4_pkg::<..>`. Without the controller widths it is the DUT's.
    pkg: String,
    addr_width: u32,
    data_bytes: u32,
    id_width: u32,
    /// Whether a width converter (`hns::axi_dw`) is needed.
    needs_dw: bool,
}

fn controller_axi(plan: &Plan, axi_mem: &crate::terminator::AxiMemPlan) -> ControllerAxi {
    let dram = target_dram(plan).unwrap_or_default();
    let (Some(bits), Some(addr), Some(id)) =
        (dram.axi_data_bits, dram.axi_addr_bits, dram.axi_id_bits)
    else {
        return ControllerAxi {
            pkg: axi_mem.pkg.clone(),
            addr_width: axi_mem.addr_width,
            data_bytes: axi_mem.data_bytes,
            id_width: axi_mem.id_width,
            needs_dw: false,
        };
    };
    // Compare the arbiter type, not the DUT: what leaves the arbiter must
    // match the controller.
    let widened = if axi_mem.dut_base != 0 {
        addr
    } else {
        addr.max(axi_mem.addr_width)
    };
    let needs_dw = !(bits == axi_mem.data_bytes * 8 && widened == addr && id == axi_mem.id_width);
    let data_bytes = if needs_dw {
        bits / 8
    } else {
        axi_mem.data_bytes
    };
    ControllerAxi {
        pkg: format!("$std::axi4_pkg::<{addr}, {data_bytes}, {id}, 1, 1, 1, 1, 1>"),
        addr_width: addr,
        data_bytes,
        id_width: id,
        needs_dw,
    }
}

/// The module name of the memory controller that Vivado generates.
pub const MIG_MODULE: &str = "hns_mig";

/// `hns/src/top.veryl`: the harness top level.
///
/// It takes the board pins, makes the clocks, reaches the CSR from the JTAG
/// bridge (and from the PCIe BAR on PCIe), and connects the DUT. JTAG goes
/// through BSCANE2 and needs no pins. The body is `core_body`, shared with
/// `hns_sim`.
pub fn top_module(plan: &Plan, prefixes: &DirectionPrefixes, csr_ident: &str) -> String {
    let clocks = &plan.board().clocks;
    let reset = plan.metadata.build.reset_type;
    let board_reset = if clocks.reset.active_low {
        "reset_async_low"
    } else {
        "reset_async_high"
    };
    let mut out = header_veryl();
    out.push_str("///\n/// Board pins -> clock generation -> JTAG -> CSR -> DUT.\n");
    out.push_str("/// The host reaches the window through the JTAG bridge. Only the board\n");
    out.push_str(
        "/// clock and reset reach the top: JTAG goes through BSCANE2 and needs no pins.\n",
    );

    // Ports terminated by `[pin]` pass through the top to physical pins.
    let pins = pin_ports(plan);
    if !pins.is_empty() {
        out.push_str("/// Ports the manifest sent to board pins ([pin]) also reach the top.\n");
    }

    out.push_str("module top (\n");
    if clocks.input.diff {
        out.push_str("    i_sys_clk_p: input 'sys clock,\n");
        out.push_str("    i_sys_clk_n: input 'sys clock,\n");
    } else {
        out.push_str("    i_sys_clk: input 'sys clock,\n");
    }
    let shared_reset = perst_is_board_reset(plan);
    if !shared_reset {
        out.push_str(&format!("    i_sys_rst: input 'sys {board_reset},\n"));
    }
    // PCIe: only the reference clock, the reset and the lanes. The rest is
    // inside the wrapper.
    if has_pcie(plan) {
        let lanes = plan.pcie_lanes();
        out.push_str("\n    // The PCIe end. Everything else about it is inside hns_pcie_wrap.\n");
        out.push_str("    i_pcie_refclk_p: input 'pcie clock,\n");
        out.push_str("    i_pcie_refclk_n: input 'pcie clock,\n");
        out.push_str("    i_pcie_reset_n : input 'pcie reset_async_low,\n");
        out.push_str(&format!("    i_pcie_rx_p: input 'pcie logic<{lanes}>,\n"));
        out.push_str(&format!("    i_pcie_rx_n: input 'pcie logic<{lanes}>,\n"));
        out.push_str(&format!("    o_pcie_tx_p: output 'pcie logic<{lanes}>,\n"));
        out.push_str(&format!(
            "    o_pcie_tx_n: output 'pcie logic<{lanes}>,\n\n"
        ));
    }
    if let Some(heartbeat) = &plan.heartbeat {
        out.push_str(&format!(
            "    o_{}: output '{} logic,\n",
            heartbeat.resource, clocks.window
        ));
    }
    for (port, _resource, width, domain) in &pins {
        let ty = if *width == 1 {
            "logic".to_string()
        } else {
            format!("logic<{width}>")
        };
        // Use the DUT port name as is. It already has a direction prefix;
        // adding `o_` would give `o_o_uart_tx`.
        out.push_str(&format!("    {port}: output {domain}{ty},\n"));
    }
    // The clock that the controller takes itself. It does not go through the
    // MMCM, so the board pins reach the top directly.
    if let Some(clock) = plan.clocks().and_then(|c| c.controller_clock.as_ref()) {
        out.push_str("\n    // The memory controller takes this straight from the board.\n");
        for port in controller_clock_ports(clock) {
            out.push_str(&format!("    {port}: input 'mig logic,\n"));
        }
    }
    // Memory controller pins: names and widths only. The MIG writes the pin
    // locations in its own XDC.
    let dram = dram_pins(plan);
    if !dram.is_empty() && is_ps7(plan) {
        out.push_str(
            "\n    // The PS's own pins: its DDR, MIO, clock and resets. The harness only\n",
        );
        out.push_str(
            "    // passes them through, and the PS7 IP writes their constraints itself.\n",
        );
    } else if !dram.is_empty() {
        out.push_str(
            "\n    // The DDR3 end. The controller drives these; the harness only passes\n",
        );
        out.push_str("    // them through, and MIG writes their pin constraints itself.\n");
    }
    for (name, dir, width) in &dram {
        // Veryl requires `tri` on a bidirectional port: it has more than one
        // driver. `tri` is a modifier, not a type.
        let base = if *dir == "inout" {
            "tri logic"
        } else {
            "logic"
        };
        let ty = if *width == 1 {
            base.to_string()
        } else {
            format!("{base}<{width}>")
        };
        out.push_str(&format!("    {name}: {dir} 'mig {ty},\n"));
    }
    out.push_str(") {\n");

    // Clocks and resets. The DUT reset (`drst_*`) is separate from the harness
    // reset: the host `dut_reset` also asserts it.
    for output in &clocks.outputs {
        let ident = &output.ident;
        out.push_str(&format!("    var clk_{ident}: '{ident} clock;\n"));
        out.push_str(&format!(
            "    var rst_{ident}: '{ident} {};\n",
            reset_type_name(reset)
        ));
        out.push_str(&format!(
            "    var drst_{ident}: '{ident} {};\n",
            reset_type_name(reset)
        ));
    }
    // The request from `hns::reset_ctl` (inside `core_body`). `u_clk` uses it
    // first, so it is declared here.
    out.push_str(&format!("    var dut_rst_req: '{csr_ident} logic;\n"));
    if shared_reset {
        // One pin is both PERST# and the board reset, so it reaches the
        // clocking as the board reset too. Both sides synchronize it.
        out.push_str(&format!("    var sys_rst: 'sys {board_reset};\n"));
        out.push_str("    unsafe (cdc) {\n");
        out.push_str("        assign sys_rst = i_pcie_reset_n;\n");
        out.push_str("    }\n");
    }
    out.push('\n');

    out.push_str("    inst u_clk: clk (\n");
    if clocks.input.diff {
        out.push_str("        i_sys_clk_p,\n        i_sys_clk_n,\n");
    } else {
        out.push_str("        i_sys_clk,\n");
    }
    out.push_str(if shared_reset {
        "        i_sys_rst: sys_rst,\n"
    } else {
        "        i_sys_rst,\n"
    });
    for output in &clocks.outputs {
        let ident = &output.ident;
        out.push_str(&format!("        o_clk_{ident}: clk_{ident},\n"));
        out.push_str(&format!("        o_rst_{ident}: rst_{ident},\n"));
        out.push_str(&format!("        o_drst_{ident}: drst_{ident},\n"));
    }
    out.push_str("        i_dut_rst: dut_rst_req,\n");
    out.push_str("    );\n\n");

    // The heartbeat UART, on the window clock. It is outside the transport, so
    // lines still come out when JTAG does not work at all.
    if let Some(heartbeat) = &plan.heartbeat {
        let ident = &clocks.window;
        out.push_str("    // Heartbeat: one identity line per second, readable without JTAG.\n");
        out.push_str("    inst u_uart: uart (\n");
        out.push_str(&format!("        i_clk: clk_{ident},\n"));
        out.push_str(&format!("        i_rst: rst_{ident},\n"));
        out.push_str(&format!("        o_tx : o_{},\n", heartbeat.resource));
        out.push_str("    );\n\n");
    }

    // The PCIe end. The hard block has 121 ports, but from here it is a few
    // wires and one AXI4-Lite bus (`rtl/pcie/rtl/hns_pcie_wrap.v`).
    //
    // The wrapper gets the window clock. Its `axil_cdc` brings the bus into
    // that clock, so everything after it runs in one domain.
    if has_pcie(plan) {
        let ident = &clocks.window;
        // The only crossing into the window. `axil_cdc` inside the wrapper
        // handles it, and `unsafe (cdc)` tells Veryl so. Only a synchronizer
        // the harness inserts itself may be marked this way.
        out.push_str("    // The only crossing. axil_cdc inside the wrapper handles it.\n");
        out.push_str("    unsafe (cdc) {\n");
        // Pass the BAR address width. The completer passes the request
        // address as is, with the BAR base (set by the BIOS, such as
        // 0x7a000000) still in it. Cutting it to this width makes it relative
        // to the BAR. The window range-checks addresses, so without this
        // every access is outside the window and returns SLVERR.
        let bar_bytes = plan
            .loaded
            .manifest
            .pcie
            .clone()
            .unwrap_or_default()
            .bar_bytes;
        let bar_addr_width = bar_bytes.trailing_zeros();
        // Declare these first. The wrapper drives them and comes before
        // `core_body`; a later declaration makes `veryl check` report them as
        // unassigned.
        for (signal, width) in AXI_LITE_WIDTH {
            out.push_str(&format!(
                "    var bar_{signal}: '{} logic<{width}>;\n",
                clocks.window
            ));
        }
        // The interrupt line, in the window clock. The wrapper brings it into
        // the hard block clock. Declared here because the wrapper comes first.
        out.push_str(&format!("    var pcie_intx: '{} logic;\n", clocks.window));
        // The FLR request and its answer, in the window clock.
        if !plan.flrs.is_empty() {
            out.push_str(&format!("    var pcie_flr: '{} logic;\n", clocks.window));
            out.push_str(&format!(
                "    var pcie_flr_ack: '{} logic;\n",
                clocks.window
            ));
        }
        // PERST#, in the window clock, for the DUT reset.
        if perst_resets_dut(plan) {
            out.push_str(&format!("    var pcie_perst: '{} logic;\n", clocks.window));
        }
        // The hard block clock. It gets a domain name, or what runs in it
        // would look like a crossing with an unknown domain. `user_reset` is
        // synchronous and active high, like the MIG `ui_clk_sync_rst`; it is
        // converted to the project polarity.
        out.push_str(&format!("    var pcie_user_clk: '{PCIE_DOMAIN} clock;\n"));
        out.push_str(&format!("    var pcie_user_srst: '{PCIE_DOMAIN} logic;\n"));
        out.push_str(&format!(
            "    var pcie_user_rst: '{PCIE_DOMAIN} {};\n",
            reset_type_name(reset)
        ));
        let released = if asserted(reset) == 0 { "~" } else { "" };
        out.push_str(&format!(
            "    assign pcie_user_rst = {released}pcie_user_srst as {};\n",
            reset_type_name(reset)
        ));
        // What the completer sees, counted by `hns::tlp_count` in the hard
        // block clock. Each count crosses to the window clock in Gray code.
        for (signal, width) in TLP_TAP {
            out.push_str(&format!(
                "    var pcie_{signal}: '{PCIE_DOMAIN} {};\n",
                logic(width)
            ));
        }
        for (name, _) in crate::regmap::PCIE_TLP_COUNTERS {
            out.push_str(&format!(
                "    var tlp_{name}_gray: '{PCIE_DOMAIN} logic<{WORD_BITS}>;\n"
            ));
            out.push_str(&format!(
                "    var tlp_{name}_sync: '{} logic<{WORD_BITS}>;\n",
                clocks.window
            ));
            out.push_str(&format!(
                "    var tlp_{name}: '{} logic<{WORD_BITS}>;\n",
                clocks.window
            ));
        }
        // The requester stream is in the hard block clock; `hns::tlp_cdc`
        // brings it from the harness side.
        let engine = !dma_registers(&plan.registers).is_empty();
        if engine {
            // In the window clock; the wrapper synchronizes them.
            out.push_str(&format!(
                "    var pcie_max_payload: '{} logic<3>;\n",
                clocks.window
            ));
            out.push_str(&format!(
                "    var pcie_max_read_req: '{} logic<3>;\n",
                clocks.window
            ));
            // In the hard block clock, Gray coded. `u_rqdrop_sync` below brings
            // it to the window clock.
            out.push_str(&format!(
                "    var pcie_rq_drops_gray: '{PCIE_DOMAIN} logic<16>;\n"
            ));
            out.push_str(&format!(
                "    var rq_drops_gray: '{} logic<16>;\n",
                clocks.window
            ));
            out.push_str(&format!(
                "    var rq_drops: '{} logic<16>;\n",
                clocks.window
            ));
            // TLPs with a gap inside. `hns::tlp_hold` counts them and
            // `u_rqgap_sync` carries the count.
            out.push_str(&format!(
                "    var pcie_rq_gaps_gray: '{PCIE_DOMAIN} logic<16>;\n"
            ));
            out.push_str(&format!(
                "    var rq_gaps_gray: '{} logic<16>;\n",
                clocks.window
            ));
            out.push_str(&format!("    var rq_gaps: '{} logic<16>;\n", clocks.window));
            for (signal, width) in RQ_STREAM {
                out.push_str(&format!(
                    "    var rq_{signal}: '{PCIE_DOMAIN} {};\n",
                    logic(width)
                ));
            }
            for (signal, width) in RC_STREAM {
                out.push_str(&format!(
                    "    var rc_{signal}: '{PCIE_DOMAIN} {};\n",
                    logic(width)
                ));
            }
        }
        out.push('\n');
        out.push_str("    inst u_pcie: $sv::hns_pcie_wrap #(\n");
        out.push_str(&format!("        BAR_ADDR_WIDTH: {bar_addr_width},\n"));
        // The PCIE3 block takes the subsystem vendor ID on a port.
        if plan.target().and_then(pcie_block) == Some(PcieBlock::Pcie3) {
            let vendor = plan
                .loaded
                .manifest
                .pcie
                .clone()
                .unwrap_or_default()
                .vendor_id;
            out.push_str(&format!("        SUBSYSTEM_VENDOR_ID: 16'h{vendor:04x},\n"));
        }
        out.push_str("    ) (\n");
        out.push_str("        i_pcie_refclk_p,\n");
        out.push_str("        i_pcie_refclk_n,\n");
        out.push_str("        i_pcie_reset_n,\n");
        out.push_str("        i_pcie_rx_p,\n");
        out.push_str("        i_pcie_rx_n,\n");
        out.push_str("        o_pcie_tx_p,\n");
        out.push_str("        o_pcie_tx_n,\n");
        out.push_str(&format!("        i_clk  : clk_{ident},\n"));
        out.push_str(&format!("        i_rst_n: rst_{ident},\n"));
        // Nothing reads the link state yet.
        out.push_str("        o_link_up: _,\n");
        out.push_str("        i_intx   : pcie_intx,\n");
        if plan.flrs.is_empty() {
            out.push_str("        o_flr    : _,\n");
            out.push_str("        i_flr_ack: 0,\n");
        } else {
            out.push_str("        o_flr    : pcie_flr,\n");
            out.push_str("        i_flr_ack: pcie_flr_ack,\n");
        }
        if perst_resets_dut(plan) {
            out.push_str("        o_perst  : pcie_perst,\n");
        } else {
            out.push_str("        o_perst  : _,\n");
        }
        out.push_str("        o_user_clk  : pcie_user_clk,\n");
        out.push_str("        o_user_reset: pcie_user_srst,\n");
        for (signal, _width) in TLP_TAP {
            out.push_str(&format!("        o_{signal}: pcie_{signal},\n"));
        }
        // Only the DMA engine drives RQ. Without it, RQ is tied to 0; left
        // open, the hard block `tvalid` would be an unconnected input.
        if engine {
            for (signal, _width) in RQ_STREAM {
                let d = if signal == "tready" { "o" } else { "i" };
                out.push_str(&format!("        {d}_rq_{signal}: rq_{signal},\n"));
            }
            // RC goes the other way: the hard block sends, the harness takes.
            for (signal, _width) in RC_STREAM {
                let d = if signal == "tready" { "i" } else { "o" };
                out.push_str(&format!("        {d}_rc_{signal}: rc_{signal},\n"));
            }
            // Sizes negotiated by the link. The wrapper already brings them
            // to the window clock.
            out.push_str("        o_max_payload: pcie_max_payload,\n");
            out.push_str("        o_max_read_req: pcie_max_read_req,\n");
            out.push_str("        o_rq_drops_gray: pcie_rq_drops_gray,\n");
        } else {
            out.push_str("        i_rq_tdata  : 0,\n");
            out.push_str("        i_rq_tkeep  : 0,\n");
            out.push_str("        i_rq_tlast  : 0,\n");
            out.push_str("        i_rq_tuser  : 0,\n");
            out.push_str("        i_rq_tvalid : 0,\n");
            out.push_str("        o_rq_tready : _,\n");
            // RC may be dropped when nobody takes it. No completion should
            // come, but a low `tready` would stall the hard block, so it
            // stays high.
            out.push_str("        o_rc_tdata  : _,\n");
            out.push_str("        o_rc_tkeep  : _,\n");
            out.push_str("        o_rc_tlast  : _,\n");
            out.push_str("        o_rc_tuser  : _,\n");
            out.push_str("        o_rc_tvalid : _,\n");
            out.push_str("        i_rc_tready : 1,\n");
            out.push_str("        o_max_payload: _,\n");
            out.push_str("        o_max_read_req: _,\n");
            out.push_str("        o_rq_drops_gray: _,\n");
        }
        for (signal, _width, from_slave) in AXI_SIGNALS {
            // The wrapper is the master: it takes the slave-driven signals
            // and drives the rest.
            let d = if from_slave { "i" } else { "o" };
            out.push_str(&format!("        {d}_{signal}: bar_{signal},\n"));
        }
        out.push_str("    );\n    }\n\n");

        // The TLP counts. Counted in the hard block clock, then each one to
        // the window clock: Gray code from a flop, so two stages per bit are
        // enough.
        out.push_str("    inst u_tlp_count: hns::tlp_count (\n");
        out.push_str("        i_clk: pcie_user_clk,\n");
        out.push_str("        i_rst: pcie_user_rst,\n");
        for (signal, _width) in TLP_TAP {
            out.push_str(&format!("        i_{signal}: pcie_{signal},\n"));
        }
        for (name, port) in crate::regmap::PCIE_TLP_COUNTERS {
            out.push_str(&format!("        {port}: tlp_{name}_gray,\n"));
        }
        out.push_str("    );\n");
        for (name, _) in crate::regmap::PCIE_TLP_COUNTERS {
            out.push_str("    unsafe (cdc) {\n");
            out.push_str(&format!(
                "        inst {}: $std::synchronizer_basic #(\n            WIDTH: {WORD_BITS},\n        ) (\n",
                tlp_sync_instance(name)
            ));
            out.push_str(&format!("            i_clk: clk_{ident},\n"));
            out.push_str(&format!("            i_rst: rst_{ident},\n"));
            out.push_str(&format!("            i_d: tlp_{name}_gray,\n"));
            out.push_str(&format!("            o_d: tlp_{name}_sync,\n"));
            out.push_str("        );\n    }\n");
            out.push_str(&format!(
                "    inst u_tlp_{}_bin: $std::gray_decoder #(\n        WIDTH: {WORD_BITS},\n    ) (\n",
                name.trim_start_matches("pcie_")
            ));
            out.push_str(&format!("        i_gray: tlp_{name}_sync,\n"));
            out.push_str(&format!("        o_bin : tlp_{name},\n"));
            out.push_str("    );\n");
        }
        out.push('\n');
        if engine {
            // The count of RQ TLPs dropped by the hard block, to the window
            // clock. It is Gray coded, so a two-stage synchronizer per bit is
            // enough: one increment changes one bit, and the wrapper drives it
            // from a flop.
            out.push_str("    unsafe (cdc) {\n");
            out.push_str(
                "        inst u_rqdrop_sync: $std::synchronizer_basic #(\n            WIDTH: 16,\n        ) (\n",
            );
            out.push_str(&format!("            i_clk: clk_{ident},\n"));
            out.push_str(&format!("            i_rst: rst_{ident},\n"));
            out.push_str("            i_d: pcie_rq_drops_gray,\n");
            out.push_str("            o_d: rq_drops_gray,\n");
            out.push_str("        );\n    }\n");
            out.push_str(
                "    inst u_rqdrop_bin: $std::gray_decoder #(\n        WIDTH: 16,\n    ) (\n",
            );
            out.push_str("        i_gray: rq_drops_gray,\n");
            out.push_str("        o_bin : rq_drops,\n");
            out.push_str("    );\n\n");
            // TLPs with a gap inside. Same shape (Gray code, two stages per bit).
            out.push_str("    unsafe (cdc) {\n");
            out.push_str(
                "        inst u_rqgap_sync: $std::synchronizer_basic #(\n            WIDTH: 16,\n        ) (\n",
            );
            out.push_str(&format!("            i_clk: clk_{ident},\n"));
            out.push_str(&format!("            i_rst: rst_{ident},\n"));
            out.push_str("            i_d: pcie_rq_gaps_gray,\n");
            out.push_str("            o_d: rq_gaps_gray,\n");
            out.push_str("        );\n    }\n");
            out.push_str(
                "    inst u_rqgap_bin: $std::gray_decoder #(\n        WIDTH: 16,\n    ) (\n",
            );
            out.push_str("        i_gray: rq_gaps_gray,\n");
            out.push_str("        o_bin : rq_gaps,\n");
            out.push_str("    );\n\n");
        }
    }

    // The AXI bus from the JTAG bridge.
    for signal in [
        ("awaddr", 32),
        ("awprot", 3),
        ("awvalid", 1),
        ("awready", 1),
        ("wdata", 32),
        ("wstrb", 4),
        ("wvalid", 1),
        ("wready", 1),
        ("bresp", 2),
        ("bvalid", 1),
        ("bready", 1),
        ("araddr", 32),
        ("arprot", 3),
        ("arvalid", 1),
        ("arready", 1),
        ("rdata", 32),
        ("rresp", 2),
        ("rvalid", 1),
        ("rready", 1),
    ] {
        out.push_str(&format!(
            "    var axi_{}: '{csr_ident} logic<{}>;\n",
            signal.0, signal.1
        ));
    }
    out.push('\n');

    // Our own JTAG bridge. Its CDC is inside `hns::dr`, marked with
    // `unsafe (cdc)` there, so no crossing is visible here.
    //
    // The bridge has no `awprot` / `arprot` (the window has no privilege
    // levels). They are driven to 0, not left floating.
    out.push_str("    assign axi_awprot = 3'b000;\n");
    out.push_str("    assign axi_arprot = 3'b000;\n\n");

    let addr_bits = bits_for(plan.registers.size_bytes());
    out.push_str(&format!(
        "    inst u_jtag: hns::bscan #(\n        AW: {addr_bits},\n    ) (\n"
    ));
    out.push_str(&format!("        i_clk: clk_{csr_ident},\n"));
    out.push_str(&format!("        i_rst: rst_{csr_ident},\n"));
    for signal in [
        "awaddr", "awvalid", "awready", "wdata", "wstrb", "wvalid", "wready", "bresp", "bvalid",
        "bready", "araddr", "arvalid", "arready", "rdata", "rresp", "rvalid", "rready",
    ] {
        // Directions seen from the bridge: slave-driven signals are inputs.
        let dir = match signal {
            "awready" | "wready" | "bresp" | "bvalid" | "arready" | "rdata" | "rresp"
            | "rvalid" => "i",
            _ => "o",
        };
        out.push_str(&format!("        {dir}_{signal}: axi_{signal},\n"));
    }
    out.push_str("    );\n\n");

    core_body(
        &mut out,
        plan,
        has_pcie(plan),
        prefixes,
        Some(clocks),
        Domain {
            clk: &format!("clk_{csr_ident}"),
            rst: &format!("rst_{csr_ident}"),
            tag: &format!("'{csr_ident} "),
        },
    );
    if has_pcie(plan) {
        // No `host_irq`: the line stays low, and the IP has no interrupt pin.
        let line = plan
            .irqs
            .first()
            .map_or("0".to_string(), |irq| format!("w_{}", irq.port));
        out.push_str(&format!("    assign pcie_intx = {line};\n"));
    }
    out.push_str("}\n");
    out
}

/// The DUT-side wires of a transfer-level port, `(port, width)`.
fn xfer_wires(xfer: &crate::terminator::XferPorts) -> Vec<(String, usize)> {
    let mut wires: Vec<(String, usize)> = Vec::new();
    if let Some(read) = &xfer.read {
        wires.extend([
            (read.cmd_valid.clone(), 1),
            (read.cmd_ready.clone(), 1),
            (read.cmd_addr.clone(), xfer.cmd_addr_width),
            (read.cmd_size.clone(), xfer.cmd_size_width),
            (read.valid.clone(), 1),
            (read.ready.clone(), 1),
            (read.data.clone(), xfer.data_width),
            (read.last.clone(), 1),
            (read.error.clone(), 1),
        ]);
    }
    if let Some(write) = &xfer.write {
        wires.extend([
            (write.cmd_valid.clone(), 1),
            (write.cmd_ready.clone(), 1),
            (write.cmd_addr.clone(), xfer.cmd_addr_width),
            (write.cmd_size.clone(), xfer.cmd_size_width),
            (write.valid.clone(), 1),
            (write.ready.clone(), 1),
            (write.data.clone(), xfer.data_width),
            (write.last.clone(), 1),
            (write.done_valid.clone(), 1),
            (write.done_error.clone(), 1),
        ]);
        if let Some(strb) = &write.strb {
            wires.push((strb.clone(), xfer.data_width / 8));
        }
    }
    wires
}

/// The DUT reset of the window's clock, as the DUT gets it. Without a DUT reset
/// there, the harness reset.
fn dut_reset_of(clocks: Option<&ClockPlan>, rst: &str) -> String {
    match clocks {
        None => "dut_rst".to_string(),
        Some(clocks) => {
            let ident = rst.strip_prefix("rst_").unwrap_or("c0");
            if clocks.resets.iter().any(|(_, owner)| owner == ident) {
                format!("drst_{ident}")
            } else {
                rst.to_string()
            }
        }
    }
}

/// `hns::corebus2axi`: a `dram` or `bram` whose DUT port is a pzcorebus memory
/// master. It drives `axi_<bundle>`, where an AXI4 DUT would be connected.
///
/// The module has plain wires, so they are joined to the interface members
/// here. The enums (`mcmd`, `sresp`) are bits in the module, and the beat count
/// is the interface's own function, which knows the unit size.
fn corebus_adapter(
    out: &mut String,
    clocks: Option<&ClockPlan>,
    clk: &str,
    rst: &str,
    axi_mem: &crate::terminator::AxiMemPlan,
    corebus: &crate::terminator::CorebusMaster,
) {
    let bundle = &axi_mem.bundle;
    let project = corebus
        .iface
        .split("::")
        .next()
        .expect("an interface path has a project");
    let dut_reset = dut_reset_of(clocks, rst);
    out.push_str(&format!(
        "    inst u_cb_{bundle}: hns::corebus2axi::<{}> #(\n        ID_W: {},\n        BURST_W: 16,\n    ) (\n",
        axi_mem.pkg, corebus.id_width
    ));
    out.push_str(&format!("        i_clk: {clk},\n"));
    out.push_str(&format!("        i_rst: {dut_reset},\n"));
    for (port, member) in [
        ("i_mcmd_valid", "mcmd_valid"),
        ("i_mid", "mid"),
        ("i_maddr", "maddr"),
        ("o_scmd_accept", "scmd_accept"),
        ("i_mdata_valid", "mdata_valid"),
        ("i_mdata", "mdata"),
        ("i_mdata_last", "mdata_last"),
        ("o_sdata_accept", "sdata_accept"),
        ("i_mresp_accept", "mresp_accept"),
        ("o_sresp_valid", "sresp_valid"),
        ("o_sid", "sid"),
        ("o_serror", "serror"),
        ("o_sdata", "sdata"),
    ] {
        out.push_str(&format!("        {port}: cb_{bundle}.{member},\n"));
    }
    out.push_str(&format!("        i_mcmd: cbw_{bundle}_mcmd,\n"));
    out.push_str(&format!("        i_mburst: cbw_{bundle}_burst,\n"));
    out.push_str(&format!("        i_mdata_byteen: cbw_{bundle}_byteen,\n"));
    out.push_str(&format!("        o_sresp_data: cbw_{bundle}_rdata,\n"));
    out.push_str(&format!("        o_sresp_last: cbw_{bundle}_rlast,\n"));
    out.push_str(&format!("        o_refused: t_{bundle}_refused,\n"));
    out.push_str(&format!("        axi: axi_{bundle},\n"));
    out.push_str("    );\n");
    out.push_str(&format!(
        "    assign cbw_{bundle}_mcmd = cb_{bundle}.mcmd as 4;\n"
    ));
    out.push_str(&format!(
        "    assign cbw_{bundle}_burst = cb_{bundle}.get_mburst_length() as 16;\n"
    ));
    // Without byte enables the bus has none, and the write is a full beat.
    if corebus.use_byte_enable {
        out.push_str(&format!(
            "    assign cbw_{bundle}_byteen = cb_{bundle}.mdata_byteen;\n"
        ));
    } else {
        out.push_str(&format!("    assign cbw_{bundle}_byteen = '1;\n"));
    }
    out.push_str(&format!(
        "    assign cb_{bundle}.sresp = if cbw_{bundle}_rdata ? {project}::pzcorebus_common_pkg::pzcorebus_sresp::RESPONSE_WITH_DATA : {project}::pzcorebus_common_pkg::pzcorebus_sresp::RESPONSE;\n"
    ));
    out.push_str(&format!(
        "    assign cb_{bundle}.sresp_last = if cbw_{bundle}_rlast ? '1 : '0;\n"
    ));
    out.push_str(&format!(
        "    assign cb_{bundle}.sresp_uniten = if cbw_{bundle}_rdata ? '1 : '0;\n"
    ));
    out.push_str(&format!("    assign cb_{bundle}.sinfo = 0;\n\n"));
}

/// `hns::xfer2axi`: a `dram` with a transfer-level port. It drives
/// `axi_<bundle>`, where an AXI4 DUT would be connected.
fn xfer_adapter(
    out: &mut String,
    clocks: Option<&ClockPlan>,
    clk: &str,
    rst: &str,
    axi_mem: &crate::terminator::AxiMemPlan,
    xfer: &crate::terminator::XferPorts,
) {
    let bundle = &axi_mem.bundle;
    let dut_reset = dut_reset_of(clocks, rst);
    let side = |port: Option<&String>, input: bool| -> String {
        match port {
            Some(name) => format!("w_{name}"),
            None if input => "0".to_string(),
            None => "_".to_string(),
        }
    };
    let read = xfer.read.as_ref();
    let write = xfer.write.as_ref();
    let has_strb = write.is_some_and(|w| w.strb.is_some());
    out.push_str(&format!(
        "    inst u_xfer_{bundle}: hns::xfer2axi::<{}> #(\n",
        axi_mem.pkg
    ));
    out.push_str(&format!("        HAS_STRB: {has_strb},\n"));
    out.push_str(&format!("        DEPTH: {},\n", axi_mem.depth));
    out.push_str(&format!("        ADDR_W: {},\n", xfer.cmd_addr_width));
    out.push_str(&format!("        SIZE_W: {},\n", xfer.cmd_size_width));
    out.push_str("    ) (\n");
    out.push_str(&format!("        i_clk: {clk},\n"));
    out.push_str(&format!("        i_rst: {dut_reset},\n"));
    for (port, chosen, input) in [
        ("i_rd_cmd_valid", read.map(|r| &r.cmd_valid), true),
        ("o_rd_cmd_ready", read.map(|r| &r.cmd_ready), false),
        ("i_rd_cmd_addr", read.map(|r| &r.cmd_addr), true),
        ("i_rd_cmd_size", read.map(|r| &r.cmd_size), true),
        ("o_rd_valid", read.map(|r| &r.valid), false),
        ("i_rd_ready", read.map(|r| &r.ready), true),
        ("o_rd_data", read.map(|r| &r.data), false),
        ("o_rd_last", read.map(|r| &r.last), false),
        ("o_rd_error", read.map(|r| &r.error), false),
        ("i_wr_cmd_valid", write.map(|w| &w.cmd_valid), true),
        ("o_wr_cmd_ready", write.map(|w| &w.cmd_ready), false),
        ("i_wr_cmd_addr", write.map(|w| &w.cmd_addr), true),
        ("i_wr_cmd_size", write.map(|w| &w.cmd_size), true),
        ("i_wr_valid", write.map(|w| &w.valid), true),
        ("o_wr_ready", write.map(|w| &w.ready), false),
        ("i_wr_data", write.map(|w| &w.data), true),
        ("i_wr_strb", write.and_then(|w| w.strb.as_ref()), true),
        ("i_wr_last", write.map(|w| &w.last), true),
        ("o_wr_done_valid", write.map(|w| &w.done_valid), false),
        ("o_wr_done_error", write.map(|w| &w.done_error), false),
    ] {
        out.push_str(&format!("        {port}: {},\n", side(chosen, input)));
    }
    out.push_str(&format!("        axi: axi_{bundle},\n"));
    out.push_str(&format!("        o_oor: t_{bundle}_oor,\n"));
    out.push_str("    );\n\n");
}

/// What to connect to one DUT port.
fn connection(
    plan: &Plan,
    clocks: Option<&ClockPlan>,
    clk: &str,
    rst: &str,
    port: &crate::dut::Port,
) -> String {
    use crate::dut::SignalRole;

    // Clocks and resets come from `hns_clk`. The simulation top has no MMCM,
    // so there the given port is connected as is.
    if port
        .signals
        .iter()
        .any(|signal| signal.role == SignalRole::Clock)
    {
        let Some(clocks) = clocks else {
            return clk.to_string();
        };
        let ident = clocks
            .outputs
            .iter()
            .find(|output| output.ports.contains(&port.name))
            .map(|output| output.ident.as_str())
            .unwrap_or("c0");
        return format!("clk_{ident}");
    }
    if port
        .signals
        .iter()
        .any(|signal| signal.role == SignalRole::Reset)
    {
        // A Debug Module's reset gets the harness reset, which `hio reset`
        // does not touch, so the debugger keeps its state.
        let debug_module = plan
            .apbs
            .iter()
            .any(|dmi| dmi.reset.as_deref() == Some(port.name.as_str()));
        // The DUT gets the DUT reset. Unlike the harness `rst_*`, the host
        // `dut_reset` also asserts it.
        let Some(clocks) = clocks else {
            return if debug_module { rst } else { "dut_rst" }.to_string();
        };
        let ident = clocks
            .resets
            .iter()
            .find(|(name, _)| name == &port.name)
            .map(|(_, ident)| ident.as_str())
            .unwrap_or("c0");
        return format!("{}_{ident}", if debug_module { "rst" } else { "drst" });
    }

    // [tie_off] / [leave_open].
    if let Some(entry) = plan
        .unconnected
        .iter()
        .find(|entry| entry.port == port.name)
    {
        return match &entry.kind {
            UnconnectedKind::Tie(value) => value.text.clone(),
            UnconnectedKind::Open => "_".to_string(),
            // A pin terminator passes through to a port of `hns_top`.
            UnconnectedKind::Pin { .. } => port.name.clone(),
        };
    }

    // axi_mem AXI4: connect the interface instance, not single wires.
    if let Some(axi_mem) = plan
        .axi_mems
        .iter()
        .find(|axi_mem| axi_mem.port == port.name)
    {
        // A pzcorebus port is joined to its own interface, and
        // `hns::corebus2axi` drives `axi_<bundle>` from it.
        let prefix = if axi_mem.corebus.is_some() {
            "cb"
        } else {
            "axi"
        };
        return format!("{prefix}_{}", axi_mem.bundle);
    }
    if let Some(axi) = plan.axi_slaves.iter().find(|axi| axi.port == port.name) {
        let prefix = if axi.corebus.is_some() { "cb" } else { "axi" };
        return format!("{prefix}_{}", axi.bundle);
    }

    // A `reg` register.
    format!("w_{}", port.name)
}

/// A pzcorebus `slave` of the DUT: the window drives it through
/// `hns::corebus_host`. The module has plain wires, so they are joined to the
/// members of the interface here. The command code and the response kind are
/// enums in the interface, and the module knows only their bits.
fn emit_corebus_host(
    out: &mut String,
    axi: &crate::terminator::AxiSlavePlan,
    corebus: &crate::terminator::CorebusSlave,
    domain: &str,
    clk: &str,
    rst: &str,
) {
    let bundle = &axi.bundle;
    let project = corebus
        .iface
        .split("::")
        .next()
        .expect("an interface path has a project");
    let word_addr = axi.word_addr_width();
    out.push_str(&format!(
        "    inst cb_{bundle}: {domain}{};\n",
        corebus.iface
    ));
    out.push_str(&format!("    var cbw_{bundle}_mcmd: {domain}logic<4>;\n"));
    out.push_str(&format!(
        "    inst u_hcb_{bundle}: hns::corebus_host #(\n        ADDR_W: {word_addr},\n        ID_W: {},\n        DATA_BYTES: {},\n        MEM: {},\n    ) (\n",
        corebus.id_width, axi.data_bytes, corebus.memory
    ));
    out.push_str(&format!("        i_clk: {clk},\n"));
    out.push_str(&format!("        i_rst: {rst},\n"));
    // A DUT slave may stop answering when it is reset.
    out.push_str("        i_hold: dut_fence,\n");
    out.push_str(&format!("        o_mcmd_valid: cb_{bundle}.mcmd_valid,\n"));
    out.push_str(&format!("        o_mcmd: cbw_{bundle}_mcmd,\n"));
    out.push_str(&format!("        o_mid: cb_{bundle}.mid,\n"));
    out.push_str(&format!("        o_maddr: cb_{bundle}.maddr,\n"));
    out.push_str(&format!(
        "        i_scmd_accept: cb_{bundle}.scmd_accept,\n"
    ));
    out.push_str(&format!("        o_mdata: cb_{bundle}.mdata,\n"));
    if corebus.memory {
        out.push_str(&format!(
            "        o_mdata_byteen: cb_{bundle}.mdata_byteen,\n"
        ));
        out.push_str(&format!(
            "        o_mdata_valid: cb_{bundle}.mdata_valid,\n"
        ));
        out.push_str(&format!(
            "        i_sdata_accept: cb_{bundle}.sdata_accept,\n"
        ));
    } else {
        out.push_str("        o_mdata_byteen: _,\n");
        out.push_str("        o_mdata_valid: _,\n");
        out.push_str("        i_sdata_accept: 1'b0,\n");
    }
    out.push_str(&format!(
        "        o_mresp_accept: cb_{bundle}.mresp_accept,\n"
    ));
    out.push_str(&format!(
        "        i_sresp_valid: cb_{bundle}.sresp_valid,\n"
    ));
    out.push_str(&format!("        i_sdata: cb_{bundle}.sdata,\n"));
    out.push_str(&format!("        i_haddr: t_{bundle}_maddr,\n"));
    out.push_str(&format!("        i_hwdata: t_{bundle}_mdata,\n"));
    out.push_str(&format!("        i_hwe: t_{bundle}_mwe,\n"));
    out.push_str(&format!("        i_hre: t_{bundle}_mre,\n"));
    out.push_str(&format!("        o_hrdata: t_{bundle}_mrdata,\n"));
    out.push_str(&format!("        o_hrvalid: t_{bundle}_mrvalid,\n"));
    out.push_str(&format!("        o_hwready: t_{bundle}_mwready,\n"));
    out.push_str("    );\n");
    // The rest of the master side: one beat, no extra info. A memory command
    // is one unit long (0 would mean the longest); a CSR one has no length.
    out.push_str(&format!(
        "    assign cb_{bundle}.mcmd = cbw_{bundle}_mcmd as {project}::pzcorebus_common_pkg::pzcorebus_mcmd;\n"
    ));
    let length = if corebus.memory { 1 } else { 0 };
    out.push_str(&format!("    assign cb_{bundle}.mlength = {length};\n"));
    for tied in ["mparam", "minfo"] {
        out.push_str(&format!("    assign cb_{bundle}.{tied} = 0;\n"));
    }
    if corebus.memory {
        out.push_str(&format!("    assign cb_{bundle}.mdata_last = 1;\n\n"));
    } else {
        out.push_str(&format!("    assign cb_{bundle}.mdata_byteen = '1;\n"));
        out.push_str(&format!("    assign cb_{bundle}.mdata_valid = 0;\n"));
        out.push_str(&format!("    assign cb_{bundle}.mdata_last = 0;\n\n"));
    }
}

/// Escapes an identifier that is a Veryl keyword with `r#`.
///
/// IP port names can be Veryl keywords. For example, clk_wiz with
/// `RESET_TYPE` ACTIVE_HIGH has a port named `reset`.
fn veryl_ident(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "always_comb",
        "always_ff",
        "as",
        "assign",
        "bit",
        "bool",
        "break",
        "case",
        "clock",
        "clock_posedge",
        "clock_negedge",
        "const",
        "converse",
        "default",
        "else",
        "embed",
        "enum",
        "export",
        "f32",
        "f64",
        "final",
        "for",
        "function",
        "i32",
        "i64",
        "if",
        "if_reset",
        "import",
        "in",
        "include",
        "initial",
        "inout",
        "input",
        "inside",
        "inst",
        "interface",
        "let",
        "logic",
        "lsb",
        "modport",
        "module",
        "msb",
        "output",
        "outside",
        "package",
        "param",
        "proto",
        "pub",
        "ref",
        "repeat",
        "reset",
        "reset_async_high",
        "reset_async_low",
        "reset_sync_high",
        "reset_sync_low",
        "return",
        "same",
        "signed",
        "step",
        "string",
        "struct",
        "switch",
        "tri",
        "type",
        "u32",
        "u64",
        "union",
        "unsafe",
        "var",
    ];
    if KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_string()
    }
}

/// `<out-dir>/Veryl.toml`: makes the output its own Veryl project.
///
/// Each part is derived, and a wrong one breaks silently:
///
/// - `[project] name` is `<DUT name>_<out-dir base name>`. It becomes the SV
///   prefix, so separate out-dirs keep several harnesses for one DUT apart.
/// - `[build]` is copied from the DUT. The top project's `reset_type` also
///   applies to its dependencies, so a mismatch emits the DUT SV with another
///   reset polarity (confirmed on hardware).
/// - The analysis limits and `[lint.portability]` are copied too: the harness
///   project analyses the DUT and everything it depends on again, so it fails
///   wherever the DUT needed a larger limit or allowed a dependency something.
/// - `[dependencies]` has `hns` (as the DUT declares it) and a path to the DUT.
pub fn veryl_toml(plan: &Plan, hns: &str, dut_path: &str, extra: &[(String, String)]) -> String {
    let build = &plan.metadata.build;
    let mut out = format!("# {}\n", crate::generate::MARKER);
    out.push_str("#\n# Generated by veryl-harness. DO NOT EDIT --\n");
    out.push_str("# `veryl harness gen` rewrites this file.\n\n");
    out.push_str(&format!(
        "[project]\nname    = \"{}\"\nversion = \"0.1.0\"\n\n",
        plan.harness
    ));
    out.push_str(&format!(
        "[build]\nreset_type = \"{}\"\n",
        reset_type_toml(build.reset_type)
    ));
    if build.omit_project_prefix {
        out.push_str("omit_project_prefix = true\n");
    }
    // Only what differs from Veryl's default, so a plain DUT gets a plain file.
    let default: veryl_metadata::Build =
        toml::from_str("").expect("every [build] key has a default");
    for (key, value, default) in [
        (
            "instance_depth_limit",
            build.instance_depth_limit,
            default.instance_depth_limit,
        ),
        (
            "instance_total_limit",
            build.instance_total_limit,
            default.instance_total_limit,
        ),
        (
            "function_instance_depth_limit",
            build.function_instance_depth_limit,
            default.function_instance_depth_limit,
        ),
        (
            "symbol_eval_depth_limit",
            build.symbol_eval_depth_limit,
            default.symbol_eval_depth_limit,
        ),
        (
            "evaluate_size_limit",
            build.evaluate_size_limit,
            default.evaluate_size_limit,
        ),
        (
            "evaluate_array_limit",
            build.evaluate_array_limit,
            default.evaluate_array_limit,
        ),
    ] {
        if value != default {
            out.push_str(&format!("{key} = {value}\n"));
        }
    }
    out.push('\n');
    out.push_str("[dependencies]\n");
    out.push_str(&format!("hns = {hns}\n"));
    out.push_str(&format!(
        "{} = {{ path = \"{dut_path}\" }}\n",
        plan.metadata.project.name
    ));
    for (name, line) in extra {
        out.push_str(&format!("{name} = {line}\n"));
    }
    let allowed = &plan.metadata.lint.portability.allow_in_dependencies;
    if !allowed.is_empty() {
        let items: Vec<String> = allowed.iter().map(|item| format!("\"{item}\"")).collect();
        out.push_str(&format!(
            "\n[lint.portability]\nallow_in_dependencies = [{}]\n",
            items.join(", ")
        ));
    }
    if plan.target().is_some_and(|target| target.is_sim()) {
        out.push_str(&format!("\n[[components]]\npath = \"{SIM_LINK_DIR}\"\n"));
    }
    out
}

/// The spelling of `[build] reset_type` as written in Veryl.toml.
fn reset_type_toml(reset: ResetType) -> &'static str {
    match reset {
        ResetType::AsyncLow => "async_low",
        ResetType::AsyncHigh => "async_high",
        ResetType::SyncLow => "sync_low",
        ResetType::SyncHigh => "sync_high",
    }
}

/// The top module name after emission, for the synthesis `-top`. Veryl adds
/// the project name as a prefix.
pub fn synth_top(plan: &Plan) -> String {
    format!("{}_top", plan.harness)
}

/// `hns/syn/board.xdc`: board pins and clocks, from the target.
pub fn board_xdc(plan: &Plan) -> String {
    let clocks = &plan.board().clocks;
    let mut out = header_tcl();
    out.push_str(
        "#\n# Board pins and clock sources, from the target description\n# (a static asset).\n\n",
    );

    let clock_port = if clocks.input.diff {
        "i_sys_clk_p"
    } else {
        "i_sys_clk"
    };
    if let (Some(pin), Some(standard)) = (&clocks.input.pin, &clocks.input.standard) {
        out.push_str(&format!(
            "set_property -dict {{ PACKAGE_PIN {pin} IOSTANDARD {standard} }} [get_ports {{ {clock_port} }}]\n"
        ));
    }
    if let (Some(pin), Some(standard)) = (&clocks.input.pin_n, &clocks.input.standard) {
        out.push_str(&format!(
            "set_property -dict {{ PACKAGE_PIN {pin} IOSTANDARD {standard} }} [get_ports {{ i_sys_clk_n }}]\n"
        ));
    }
    // The clock that the controller takes itself. It skips the MMCM, so its
    // pins are set here (the MIG XDC has only the memory pins). No period:
    // the UltraScale+ DDR4 IP puts `create_clock` on the same port in its own
    // XDC (`par/<ip>.xdc`). A second one overrides it (CRITICAL WARNING
    // 18-1056), as with the MMCM input clock below.
    if let Some(mig) = &clocks.controller_clock {
        let ports = controller_clock_ports(mig);
        let standard = mig
            .standard
            .as_deref()
            .expect("hns-targets checks `standard` on every board clock");
        for (port, pin) in ports.iter().zip([mig.pin.as_ref(), mig.pin_n.as_ref()]) {
            if let Some(pin) = pin {
                out.push_str(&format!(
                    "set_property -dict {{ PACKAGE_PIN {pin} IOSTANDARD {standard} }} [get_ports {{ {port} }}]\n"
                ));
            }
        }
        out.push('\n');
    }

    // No clock on the MMCM input. clk_wiz puts `create_clock` on the same port
    // in its own XDC (the frequency is `PRIM_IN_FREQ` in `mmcm.tcl`). A second
    // one overrides the IP definition (CRITICAL WARNING 18-1055 / 18-1056),
    // and the IP constraints on that clock (input jitter and so on) are lost.

    // The heartbeat UART pin.
    if let Some(heartbeat) = &plan.heartbeat {
        out.push_str(&format!(
            "# Heartbeat UART ({} baud).\nset_property -dict {{ PACKAGE_PIN {} IOSTANDARD {} }} [get_ports {{ o_{} }}]\n\n",
            heartbeat.actual_baud, heartbeat.pin, heartbeat.standard, heartbeat.resource
        ));
    }

    // Ports terminated by `[pin]`. The values come from the target.
    let pins = pin_ports(plan);
    if !pins.is_empty() {
        out.push_str("# Ports the manifest sent to board pins ([pin]).\n");
        for (port, resource, _width, _domain) in &pins {
            let Some(entry) = plan.unconnected.iter().find(|entry| &entry.port == port) else {
                continue;
            };
            let UnconnectedKind::Pin {
                pin: Some(pin),
                standard: Some(standard),
                ..
            } = &entry.kind
            else {
                continue;
            };
            out.push_str(&format!(
                "set_property -dict {{ PACKAGE_PIN {pin} IOSTANDARD {standard} }} [get_ports {{ {port} }}]  ;# {resource}\n"
            ));
        }
        out.push('\n');
    }

    // On a board whose board reset is PERST#, the PCIe constraints hold the pin.
    if let (Some(pin), Some(standard)) = (&clocks.reset.pin, &clocks.reset.standard)
        && !perst_is_board_reset(plan)
    {
        out.push_str(&format!(
            "set_property -dict {{ PACKAGE_PIN {pin} IOSTANDARD {standard} }} [get_ports {{ i_sys_rst }}]\n"
        ));
    }

    // Only a design with the memory controller has DCI I/O to reference.
    if !dram_pins(plan).is_empty()
        && let Some((master, slaves)) = plan
            .target()
            .and_then(|t| crate::target::dci_cascade(t).ok().flatten())
    {
        let slaves = slaves
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str("\n# The memory banks take their DCI reference from one bank. The MIG does\n");
        out.push_str(
            "# not write this, and a slave bank's own reference pins carry other signals.\n",
        );
        out.push_str(&format!(
            "set_property DCI_CASCADE {{{slaves}}} [get_iobanks {master}]\n"
        ));
    }
    // The memory controller takes the MMCM of the clock pin's region, so the
    // harness MMCM sits in the next region and is reached over the clock
    // backbone. Without this, placement stops (Place 30-575).
    if !dram_pins(plan).is_empty()
        && plan
            .target()
            .and_then(|t| t.table.get("vivado"))
            .and_then(|v| v.get("sys_clk_backbone"))
            .and_then(|v| v.as_bool())
            == Some(true)
    {
        out.push_str(
            "\n# The memory controller holds the MMCM in the board clock's region, so the\n",
        );
        out.push_str("# harness MMCM is one region away and takes the clock over the backbone.\n");
        out.push_str(
            "set_property CLOCK_DEDICATED_ROUTE BACKBONE [get_nets u_clk/u_mmcm/inst/clk_in1_hns_mmcm]\n",
        );
    }

    out.push_str("\n# Vivado derives generated clocks from the MMCM outputs on its own, so no\n");
    out.push_str("# create_generated_clock is written here.\n");
    if let Some(flash) = plan.target().and_then(hns_targets::flash) {
        out.push_str(&flash_xdc(&flash));
    }
    out
}

/// Bitstream settings for booting from the flash (`[flash]` in the target
/// description). They change nothing when the FPGA is configured over JTAG.
fn flash_xdc(flash: &hns_targets::Flash) -> String {
    let mut out = String::new();
    out.push_str("\n# Booting from the configuration flash ([flash] in the target description).\n");
    out.push_str(&format!(
        "set_property CFGBVS {} [current_design]\n",
        flash.cfgbvs
    ));
    out.push_str(&format!(
        "set_property CONFIG_VOLTAGE {} [current_design]\n",
        flash.config_voltage
    ));
    out.push_str(&format!(
        "set_property CONFIG_MODE {} [current_design]\n",
        flash.interface
    ));
    out.push_str(&format!(
        "set_property BITSTREAM.CONFIG.SPI_BUSWIDTH {} [current_design]\n",
        flash.bus_width()
    ));
    out.push_str(&format!(
        "set_property BITSTREAM.CONFIG.CONFIGRATE {} [current_design]\n",
        flash.config_rate_mhz
    ));
    out.push_str(&format!(
        "set_property BITSTREAM.CONFIG.SPI_FALL_EDGE {} [current_design]\n",
        if flash.spi_fall_edge { "Yes" } else { "No" }
    ));
    out.push_str("# Compressed: a smaller image boots sooner. A PCIe card has to be up about\n");
    out.push_str("# 100 ms after power-up for the host to find it.\n");
    out.push_str("set_property BITSTREAM.GENERAL.COMPRESS true [current_design]\n");
    out
}

/// The CDCs that the XDC bounds: instance path, the faster clock period, and
/// the synchronizer cell name.
///
/// The faster period is used because the hierarchy also holds paths inside
/// one clock; the slower period would relax them.
pub fn cdc_instances(plan: &Plan) -> Vec<(String, f64, &'static str)> {
    let Some(clocks) = plan.clocks() else {
        return Vec::new();
    };
    let dram_mhz = target_dram(plan).and_then(|d| d.ui_clk_mhz);
    let mut out = Vec::new();
    // A Zynq PS's memory is on the harness clock: nothing crosses.
    for m in plan
        .axi_mems
        .iter()
        .filter(|m| m.backing == crate::manifest::Backing::Dram && !is_ps7(plan))
    {
        let host_mhz = clocks.window_output().freq_mhz;
        let fastest = dram_mhz.map_or(host_mhz, |ui| ui.max(host_mhz));
        out.push((format!("u_cdc_{}", m.bundle), 1000.0 / fastest, "rg_reg"));
    }

    // The PCIe window also crosses. `axil_cdc` (borrowed from verilog-pcie)
    // sits between the user clock (250 MHz) and the harness clock. Its data
    // registers are read only after the handshake crosses, but Vivado does
    // not know that and fails timing without a bound (measured on VCU118 with
    // PCIe + DDR4: WNS -2.465 ns, all 56 violations here).
    //
    // A borrowed part gets the same treatment: bounded by the faster period,
    // not cut. The harness placed this synchronizer itself.
    if has_pcie(plan)
        && let Some(target) = plan.target()
    {
        let host_mhz = clocks.window_output().freq_mhz;
        let board = hns_targets::pcie(target);
        let (_, _, _, user_mhz) = board_link(&board);
        let fastest = host_mhz.max(f64::from(user_mhz));
        out.push((
            "axil_cdc_inst".to_string(),
            1000.0 / fastest,
            "flag_sync_reg",
        ));
        // The requester streams cross between the same two clocks
        // (`hns::tlp_cdc`). Inside is the same `std::async_fifo` as in
        // `hns::axi_cdc`, so the bound is the same.
        if !dma_registers(&plan.registers).is_empty() {
            out.push(("u_rq_cdc".to_string(), 1000.0 / fastest, "rg_reg"));
            out.push(("u_rc_cdc".to_string(), 1000.0 / fastest, "rg_reg"));
        }
    }
    out
}

/// The two-stage synchronizers that the XDC bounds, by `-to` only: a name
/// for the comment, the first-stage cells, and the period.
///
/// Unlike `hns::axi_cdc`, the source is outside the harness hierarchy (for
/// example inside the controller), so a bound inside one hierarchy misses it.
pub fn sync_instances(plan: &Plan) -> Vec<(String, String, f64)> {
    let Some(clocks) = plan.clocks() else {
        return Vec::new();
    };
    // `$std::synchronizer_basic` names its flops `rg`.
    let std_sync = |path: String, period_ns: f64| {
        let cells = format!("*{path}/*rg_reg*");
        (path, cells, period_ns)
    };
    let host_mhz = clocks.window_output().freq_mhz;
    let mut out: Vec<(String, String, f64)> = plan
        .axi_mems
        .iter()
        .filter(|m| m.backing == crate::manifest::Backing::Dram)
        .map(|m| std_sync(format!("u_calib_{}", m.bundle), 1000.0 / host_mhz))
        .collect();
    if has_pcie(plan)
        && !dma_registers(&plan.registers).is_empty()
        && let Some(target) = plan.target()
    {
        // RQ TLPs dropped by the hard block (Gray coded), to the window clock.
        out.push(std_sync("u_rqdrop_sync".to_string(), 1000.0 / host_mhz));
        out.push(std_sync("u_rqgap_sync".to_string(), 1000.0 / host_mhz));
        // The reset merge in `hns::tlp_cdc` (`MERGE_RESET`). The other side's
        // reset reaches the synchronizer's asynchronous clear, so that path is
        // bounded too. It ends on both sides, so the faster period is used.
        let board = hns_targets::pcie(target);
        let (_, _, _, user_mhz) = board_link(&board);
        let fastest = host_mhz.max(f64::from(user_mhz));
        for cdc in ["u_rq_cdc", "u_rc_cdc"] {
            out.push(std_sync(
                format!("{cdc}/u_fifo/u_reset_sync"),
                1000.0 / fastest,
            ));
        }
    }
    // The FLR request into the window clock, and the answer back. The
    // answer comes from a flop in `hns::flr_ctl`.
    if has_pcie(plan)
        && !plan.flrs.is_empty()
        && let Some(target) = plan.target()
    {
        let board = hns_targets::pcie(target);
        let (_, _, _, user_mhz) = board_link(&board);
        out.push((
            "u_pcie/flr_meta".to_string(),
            "*u_pcie/flr_meta_reg".to_string(),
            1000.0 / host_mhz,
        ));
        out.push((
            "u_pcie/flr_ack_meta".to_string(),
            "*u_pcie/flr_ack_meta_reg".to_string(),
            1000.0 / f64::from(user_mhz),
        ));
    }
    // The TLP counts (`hns::tlp_count`, Gray coded), to the window clock.
    if has_pcie(plan) && plan.target().is_some() {
        for (name, _) in crate::regmap::PCIE_TLP_COUNTERS {
            out.push(std_sync(tlp_sync_instance(name), 1000.0 / host_mhz));
        }
    }
    // The interrupt line, from the window clock into the hard block clock.
    // The wrapper registers it first, so the source is a flop.
    if has_pcie(plan)
        && !plan.irqs.is_empty()
        && let Some(target) = plan.target()
    {
        let board = hns_targets::pcie(target);
        let (_, _, _, user_mhz) = board_link(&board);
        out.push((
            "u_pcie/intx_meta".to_string(),
            "*u_pcie/intx_meta_reg".to_string(),
            1000.0 / f64::from(user_mhz),
        ));
    }
    out
}

/// `hns/syn/harness.xdc`: constraints for the circuits the harness inserts.
///
/// `tck_mhz` comes from `[jtag] max_tck_mhz` in the target. If the host
/// (`hns-host`) runs TCK faster, these constraints are wrong.
///
/// No false paths: cutting timing mechanically hides CDC bugs. Synchronizers
/// get `ASYNC_REG`, which does not cut timing; it packs the flops for MTBF.
///
/// This file must be read with `read_xdc -unmanaged` (`synth_tcl`). Plain
/// `read_xdc` does not accept Tcl control flow: it skips each line with `if`
/// with only `CRITICAL WARNING: [Designutils 20-1307]`. The `if` stays so that
/// a missing cell prints a warning instead of doing nothing silently.
pub fn harness_xdc(
    tck_mhz: f64,
    cdc_instances: &[(String, f64, &'static str)],
    sync_instances: &[(String, String, f64)],
) -> String {
    let mut out = header_tcl();
    out.push_str("#\n# Constraints for the circuits the harness itself inserted.\n#\n");
    out.push_str("# No false paths. Cutting timing mechanically would hide CDC\n");
    out.push_str("# bugs; all that lives here is the harness reset synchroniser.\n");
    out.push_str("# ASYNC_REG does not cut timing: it packs the flops together to buy MTBF.\n\n");
    out.push_str("set hns_sync_cells [get_cells -hier -filter {NAME =~ *rst_meta_* || NAME =~ *rst_sync_*}]\n");
    out.push_str("if {[llength $hns_sync_cells]} {\n");
    out.push_str("    set_property ASYNC_REG TRUE $hns_sync_cells\n");
    out.push_str("} else {\n");
    out.push_str(
        "    puts \"WARNING: harness reset synchroniser cells not found; ASYNC_REG not applied\"\n",
    );
    out.push_str("}\n\n");

    // AXI4 CDCs (`hns::axi_cdc`). Only synchronizers the harness inserted
    // itself are relaxed.
    //
    // Inside is `std::async_fifo`. It passes
    // Gray pointers from flops through 2FF synchronizers and reads the RAM
    // only when the read address is stable. Vivado does not know that and
    // fails timing on the crossing (measured on Arty: WNS -2.9 ns, all
    // violations inside the CDC).
    //
    // Neither `set_false_path` nor `set_clock_groups` is used. The first
    // ignores the paths; the second drops the whole clock pair. Both would
    // hide a crossing that skips the synchronizer.
    // `set_max_delay -datapath_only` bounds only this hierarchy, by the faster
    // period: the crossing is allowed, but a slow one fails.
    for (path, period_ns, sync_cells) in cdc_instances {
        out.push_str(&format!("# The crossing at {path}. Bounded, not cut.\n"));
        out.push_str(&format!(
            "set hns_cdc [get_cells -hier -filter {{IS_SEQUENTIAL && NAME =~ *{path}/*}}]\n"
        ));
        out.push_str("if {[llength $hns_cdc]} {\n");
        out.push_str(&format!(
            "    set_max_delay -datapath_only -from $hns_cdc -to $hns_cdc {period_ns:.3}\n"
        ));
        out.push_str(
            "    set_property ASYNC_REG TRUE [get_cells -hier -filter {IS_SEQUENTIAL && NAME =~ *",
        );
        out.push_str(path);
        out.push_str(&format!("/*{sync_cells}*}}]\n"));
        out.push_str("} else {\n");
        out.push_str(&format!(
            "    puts \"WARNING: {path} not found; the crossing is unconstrained\"\n"
        ));
        out.push_str("}\n\n");
    }

    // Two-stage synchronizers. The source is outside the harness, so only
    // `-to` is bounded.
    for (path, cells, period_ns) in sync_instances {
        out.push_str(&format!(
            "# The synchroniser at {path}. Bounded, not cut.\n"
        ));
        out.push_str(&format!(
            "set hns_sync [get_cells -hier -filter {{IS_SEQUENTIAL && NAME =~ {cells}}}]\n"
        ));
        out.push_str("if {[llength $hns_sync]} {\n");
        // `-from` is required (Vivado 18-540). The source is inside the
        // controller and has no name we can use. A synchronizer assumes any
        // source is asynchronous, so all clocks may be the start point. Only
        // paths into this synchronizer are relaxed.
        out.push_str(&format!(
            "    set_max_delay -datapath_only -from [get_clocks *] -to $hns_sync {period_ns:.3}\n"
        ));
        out.push_str("    set_property ASYNC_REG TRUE $hns_sync\n");
        out.push_str("} else {\n");
        out.push_str(&format!(
            "    puts \"WARNING: {path} not found; the crossing is unconstrained\"\n"
        ));
        out.push_str("}\n\n");
    }

    // TCK is asynchronous to the harness clock, and it stops while the host
    // sends nothing. The bridge synchronizes it with 2FF, so timing analysis
    // must treat it as a separate group.
    //
    // No `set_false_path`: it hides the paths. `set_clock_groups -asynchronous`
    // states that the two clocks have no phase relation, which is the intent.
    out.push_str(
        "# The bridge's TCK. **Vivado does not create this clock on its own** -- the one\n",
    );
    out.push_str("# seen with the jtag_axi IP came from its debug hub, which brought its own\n");
    out.push_str("# constraints. With our own BSCANE2 the TCK domain is unconstrained unless we\n");
    out.push_str("# say so, and then the 2FF synchroniser in hns::dr is never analysed.\n");
    out.push_str(&format!(
        "create_clock -name hns_tck -period {:.3} [get_pins -hier -filter {{NAME =~ *u_bscan/TCK}}]\n\n",
        1000.0 / tck_mhz
    ));
    out.push_str("# TCK is asynchronous to everything else, and stops whenever the host is not\n");
    out.push_str("# shifting. Not set_false_path: a clock group states that\n");
    out.push_str("# there is no phase relationship, it does not hide the path.\n");
    out.push_str("#\n");
    out.push_str("# **Do not enumerate the other clocks here.** The MMCM's generated clock does\n");
    out.push_str("# not exist yet when this file is read, so a `remove_from_collection` would\n");
    out.push_str(
        "# quietly leave it out -- the paths to and from it then fail timing. One group\n",
    );
    out.push_str(
        "# means \"asynchronous to every other clock\", which is both shorter and correct\n",
    );
    out.push_str("# whatever order the constraints are applied in.\n");
    out.push_str("set hns_tck [get_clocks -quiet hns_tck]\n");
    out.push_str("if {[llength $hns_tck]} {\n");
    out.push_str("    set_clock_groups -asynchronous -group $hns_tck\n");
    out.push_str("} else {\n");
    out.push_str(
        "    puts \"WARNING: hns_tck was not created; the bridge's CDC is unconstrained\"\n",
    );
    out.push_str("}\n");
    out
}

/// `hns/syn/ip.tcl`: IP generation. No `.xci` is kept; Tcl creates the IP.
pub fn ip_tcl(plan: &Plan) -> String {
    let part = device_part(plan);
    let mut out = header_tcl();
    out.push_str("#\n# IP generation. No .xci is kept; the IP is created from Tcl every time\n");
    out.push_str("# so differences between Vivado versions are absorbed here.\n\n");
    out.push_str(&format!("set part   {{{part}}}\n"));
    out.push_str("set ip_dir {output}\n\n");
    out.push_str("# `create_ip -dir` requires the directory to exist.\n");
    out.push_str("file mkdir $ip_dir\n");
    out.push_str("set_part $part\n");
    board_part_tcl(&mut out, plan);
    out.push_str("update_ip_catalog -rebuild\n\n");
    out.push_str("source mmcm.tcl\n");
    if has_pcie(plan) {
        out.push_str("source pcie.tcl\n");
    }
    if is_ps7(plan) {
        out.push_str("source ps7.tcl\n");
    } else if !dram_pins(plan).is_empty() {
        out.push_str("source mig.tcl\n");
    }
    out.push_str("# No out-of-context synthesis: the IP RTL is synthesised with the design, and\n");
    out.push_str("# `synth_ip` is therefore not called (it fails with 12-3437 in this mode).\n");
    out.push_str("foreach f [get_files -all {*.xci}] { set_property GENERATE_SYNTH_CHECKPOINT {false} -quiet $f }\n");
    out.push_str("generate_target all [get_ips]\n");
    if is_ps7(plan) {
        out.push_str(&format!(
            "\n# The PS setup `hio program` runs over JTAG, from the PS7 IP. It sits\n\
             # beside the bitstream, so `make zip` takes it along.\n\
             file copy -force [file join $ip_dir {PS7_MODULE} ps7_init.tcl] [file join $ip_dir ps7_init.tcl]\n"
        ));
    }
    out
}

/// `hns/syn/synth.tcl`: synthesis through the bitstream.
///
/// The RTL file list is read from the filelist `veryl build` writes. A list
/// by hand would silently miss new sources.
pub fn synth_tcl(plan: &Plan) -> String {
    let part = device_part(plan);
    let top = synth_top(plan);
    // The harness project writes the filelist. The DUT is a path dependency
    // and is listed in it too.
    let filelist = format!("{}.f", plan.harness);
    let mut out = header_tcl();
    out.push_str("#\n# Synthesis -> place and route -> bitstream.\n\n");
    out.push_str(&format!("set part     {{{part}}}\n"));
    out.push_str(&format!("set top      {{{top}}}\n"));
    out.push_str(&format!("set filelist {{../{filelist}}}\n"));
    out.push_str("set ip_dir   {output}\n");
    out.push_str("set output   {output}\n\n");
    out.push_str("set_part $part\n");
    board_part_tcl(&mut out, plan);
    out.push('\n');
    out.push_str("# IP, assuming ip.tcl already ran (the Makefile holds that dependency).\n");
    out.push_str("read_ip [glob -nocomplain -directory $ip_dir [file join * {*.xci}]]\n\n");
    // Borrowed Verilog is not in the Veryl filelist. The glob finds nothing
    // in a design that borrows none, so these lines are harmless there.
    out.push_str("# Borrowed Verilog, if any. It is not in the Veryl filelist because\n");
    out.push_str("# veryl build does not see it; the generator writes it next door.\n");
    out.push_str("foreach f [glob -nocomplain -directory ../vendor {*.v}] { read_verilog $f }\n\n");

    out.push_str("# RTL comes from the filelist veryl build writes. Never list files by hand.\n");
    out.push_str("set fh [open $filelist r]\n");
    out.push_str("foreach line [split [read $fh] \"\\n\"] {\n");
    out.push_str("    set line [string trim $line]\n");
    out.push_str("    if {$line ne \"\"} { read_verilog -sv $line }\n");
    out.push_str("}\nclose $fh\n\n");
    out.push_str("read_xdc board.xdc\n");
    if has_pcie(plan) {
        out.push_str("read_xdc pcie.xdc\n");
    }
    if dram_xdc(plan).is_some() {
        out.push_str("read_xdc dram.xdc\n");
    }
    out.push_str("# -unmanaged: harness.xdc is Tcl (it checks that the synchroniser cells\n");
    out.push_str("# exist before constraining them). Plain read_xdc drops control flow with\n");
    out.push_str("# CRITICAL WARNING Designutils 20-1307 and applies nothing.\n");
    out.push_str("read_xdc -unmanaged harness.xdc\n\n");
    out.push_str("# The directive of each step. `[vivado] tcl` in Harness.toml can set them.\n");
    for (var, _) in crate::manifest::DIRECTIVES {
        out.push_str(&format!("set {var:<18} Default\n"));
    }
    if plan.loaded.vivado_tcl.is_some() {
        out.push_str("source user.tcl\n");
    }
    out.push('\n');
    out.push_str("synth_design -top $top -part $part -directive $SYNTH_DIRECTIVE\n\n");
    // Move the debug hub away from the window.
    //
    // When an IP brings a debug core (the UltraScale+ DDR4 calibration
    // MicroBlaze does), Vivado inserts `dbg_hub`. Its default JTAG chain is 1,
    // which the harness bridge (`hns::bscan`, USER1) uses. Placement then
    // fails with "BSCAN1 is in use". The window matters more, so the hub
    // moves. Without a hub this does nothing (`-quiet`).
    out.push_str("# A debug hub, if some IP brought one, must not take USER1: that is where\n");
    out.push_str("# the harness bridge lives.\n");
    out.push_str("if {[llength [get_debug_cores -quiet dbg_hub]]} {\n");
    out.push_str("    set_property C_USER_SCAN_CHAIN 3 [get_debug_cores dbg_hub]\n");
    out.push_str("}\n\n");
    out.push_str("opt_design      -directive $OPT_DIRECTIVE\n");
    out.push_str("place_design    -directive $PLACE_DIRECTIVE\n");
    out.push_str("phys_opt_design -directive $PHYS_OPT_DIRECTIVE\n");
    out.push_str("route_design    -directive $ROUTE_DIRECTIVE\n\n");
    out.push_str("report_timing_summary            -file $output/timing.rpt -max_paths 10\n");
    out.push_str("report_utilization -hierarchical -file $output/util.rpt\n");
    // Harness area. `util.rpt` goes deep into the IP, so it is hard to compare
    // with the DUT. `area.tcl` counts only the instances under the top.
    out.push_str("source area.tcl\n");
    out.push_str("report_clocks                    -file $output/clock.rpt\n");
    out.push_str("report_drc                       -file $output/drc.rpt\n\n");
    out.push_str("write_checkpoint -force $output/$top\n");
    out.push_str("write_bitstream  -force $output/$top\n");
    out
}

/// `hns/syn/user.tcl`: the file `[vivado] tcl` names, copied. The marker
/// lets `gen` rewrite it, and remove it when the key goes away.
pub fn user_tcl(plan: &Plan) -> Option<String> {
    let text = plan.loaded.vivado_tcl.as_ref()?;
    let from = plan.loaded.manifest.vivado.tcl.as_ref()?;
    let mut out = header_tcl();
    out.push_str(&format!(
        "# Copied from {} (`[vivado] tcl` in Harness.toml)\n#\n",
        from.display()
    ));
    out.push_str(text);
    Some(out)
}

/// `hns/syn/area.tcl`: the harness area next to the DUT.
///
/// `util.rpt` walks the whole hierarchy, deep into the IP. The useful level is
/// just under the top: `u_dut` is the DUT, the rest is the harness.
///
/// It also runs alone on the checkpoint (`make area`), with no new synthesis.
pub fn area_tcl(plan: &Plan) -> String {
    let top = format!("{}_top", plan.harness);
    let mut out = header_tcl();
    out.push_str("#\n# Area by top-level instance: what the harness costs next to the DUT.\n#\n\n");
    out.push_str(&format!("set top {{{top}}}\n"));
    out.push_str("set output {output}\n\n");
    out.push_str("# Run on its own (`make area`) as well as after routing.\n");
    out.push_str("if {[llength [get_cells -quiet]] == 0} {\n");
    out.push_str("    open_checkpoint $output/$top.dcp\n");
    out.push_str("}\n\n");
    out.push_str("report_utilization -hierarchical -hierarchical_depth 1 -file $output/area.rpt\n");
    out
}

/// `hns/syn/Makefile`. After `gen`, `make` is the only step.
pub fn makefile(plan: &Plan) -> String {
    let top = synth_top(plan);
    let flash = plan.target().and_then(hns_targets::flash).is_some();
    let mut out = String::new();
    out.push_str(&format!("# {MARKER}\n#\n"));
    out.push_str("# Generated by veryl-harness. DO NOT EDIT --\n");
    out.push_str("# `veryl harness gen` rewrites this file.\n#\n");
    out.push_str("# make         bitstream\n");
    out.push_str("# make ip      generate the IP only\n");
    out.push_str("# make program program the device\n");
    out.push_str("# make svf     write the programming sequence out as SVF\n");
    if flash {
        out.push_str("# make mcs     make the flash image from the bitstream\n");
        out.push_str(
            "# make flash   erase the board's flash and write the image (Vivado Lab is enough)\n",
        );
    }
    out.push_str("# make area    what the harness costs next to the DUT\n");
    out.push_str("# make zip     pack what hio needs, for a machine without Vivado\n");
    out.push_str("# make rtl     veryl build only\n#\n");
    // No `make verify`: reading the window needs the USB probe details, which
    // are not known here. `hio id` does the same check.
    out.push_str("# Reading the window is not a target here: the harness reaches its own bridge\n");
    out.push_str("# over MPSSE, which Vivado cannot see, and doing so needs the probe's USB\n");
    out.push_str("# details. Use `hio id` -- it checks magic and the map hash against\n");
    out.push_str("# regs.json, which is what `make verify` used to do.\n\n");
    out.push_str("# Override with VIVADO_BIN / VERYL_BIN. The name `VIVADO` is avoided because\n");
    out.push_str("# Xilinx setup scripts sometimes export it as the INSTALL DIRECTORY, and `?=`\n");
    out.push_str("# would then pick that up and try to execute a directory.\n");
    out.push_str("VERYL_BIN  ?= veryl\n");
    out.push_str("VIVADO_BIN ?= vivado\n");
    out.push_str("ROOT       := ..\n");
    out.push_str(&format!("TOP        := {top}\n\n"));
    out.push_str(if flash {
        ".PHONY: all bit rtl ip program svf mcs flash area zip clean\n\n"
    } else {
        ".PHONY: all bit rtl ip program svf area zip clean\n\n"
    });
    out.push_str("all: bit\n\n");
    out.push_str("# RTL. Both the harness and the DUT are Veryl, so the SV appears here.\n");
    out.push_str("rtl:\n\tcd $(ROOT) && $(VERYL_BIN) build\n\n");
    out.push_str("ip: output/.ip.stamp\n\n");
    out.push_str("output/.ip.stamp: ip.tcl mmcm.tcl\n");
    out.push_str("\t$(VIVADO_BIN) -mode batch -source ip.tcl\n");
    out.push_str("\t@touch $@\n\n");
    out.push_str("bit: rtl ip\n\t$(VIVADO_BIN) -mode batch -source synth.tcl\n\n");
    out.push_str("# Programming goes through the standard JTAG configuration path, so it does\n");
    out.push_str("# not depend on how the harness window is reached.\n");
    out.push_str("program:\n\t$(VIVADO_BIN) -mode batch -source program.tcl\n\n");
    out.push_str("# The same sequence as SVF, so a machine without Vivado can replay it\n");
    out.push_str("# (`hio program output/<top>.svf`). Needed on devices with more than\n");
    out.push_str("# one SLR, whose .bit is one sub-bitstream per SLR.\n");
    out.push_str("svf:\n\t$(VIVADO_BIN) -mode batch -source svf.tcl\n\n");
    if flash {
        out.push_str(
            "# The board boots from its flash at power-up. `mcs` needs Vivado only; `flash`\n",
        );
        out.push_str(
            "# needs the board on this machine's cable, and **erases the whole flash**:\n",
        );
        out.push_str("#   make flash VIVADO_BIN=vivado_lab\n");
        out.push_str("mcs:\n\t$(VIVADO_BIN) -mode batch -source mcs.tcl\n\n");
        out.push_str("flash:\n\t$(VIVADO_BIN) -mode batch -source flash.tcl\n\n");
    }
    // `bit` also runs this, so it is rarely needed by hand.
    out.push_str("# Area by top-level instance, from the routed checkpoint.\n");
    out.push_str("area:\n\t$(VIVADO_BIN) -mode batch -source area.tcl\n\n");
    out.push_str(ZIP_RULE);
    out.push_str("clean:\n\t-rm -rf output *.jou *.log .Xil\n");
    out
}

/// `make zip`: what `hio` needs when the board is on another machine.
///
/// Paths in the zip start at the harness directory, as they are here: `hio`
/// looks for `regs.json` in the current directory and for the bitstream in
/// `syn/output/` beside it. So after unzipping, `cd` into the directory and
/// `hio program` and `hio id` work with no arguments. It only packs; it does
/// not build, because `bit` reruns synthesis.
const ZIP_RULE: &str = r#"# What hio needs on a machine without Vivado: the map, the bitstream, the
# SVF if `make svf` wrote one, and the reports. There:
#   unzip <top>.zip && cd <dir> && hio program && hio id
NAME := $(notdir $(abspath $(ROOT)))
SHIP := regs.json $(notdir $(wildcard $(ROOT)/regs.md)) syn/output/$(TOP).bit \
        $(addprefix syn/,$(wildcard output/$(TOP).svf output/$(TOP).mcs output/$(TOP).prm \
          output/ps7_init.tcl output/timing.rpt output/util.rpt))
zip:
	@test -f output/$(TOP).bit || { echo "no output/$(TOP).bit yet: run make first"; exit 1; }
	rm -f output/$(TOP).zip
	cd $(ROOT)/.. && zip -q $(NAME)/syn/output/$(TOP).zip $(addprefix $(NAME)/,$(SHIP))
	@echo "wrote output/$(TOP).zip: $(SHIP)"

"#;

/// `hns/syn/board.tcl`: board-specific data.
///
/// `program.tcl` only reads it and knows nothing else about the board.
pub fn board_tcl(plan: &Plan) -> String {
    let target = &plan.board().target;
    let top = synth_top(plan);
    let mut out = header_tcl();
    out.push_str("#\n# Board-specific data, from the target description.\n\n");
    out.push_str("namespace eval hns {}\n\n");
    out.push_str(&format!(
        "# Regex matched against the device names hw_manager lists. More than one FPGA\n\
         # may be connected, so never just take the first one.\n\
         set ::hns::device_pattern {{{}}}\n",
        target.head.device.hw_pattern()
    ));
    out.push_str(&format!(
        "set ::hns::part            {{{}}}\n",
        target.head.device.part
    ));
    out.push_str(&format!(
        "set ::hns::bitstream       {{output/{top}.bit}}\n"
    ));
    out.push_str(&format!(
        "\n# Engineering sample device? Programming one needs the bitstream version check\n\
         # relaxed (from [device] engineering_sample in the target description).\n\
         set ::hns::engineering_sample {}\n",
        if target.head.device.engineering_sample {
            1
        } else {
            0
        }
    ));
    if let Some(flash) = hns_targets::flash(target) {
        out.push_str(&format!(
            "\n# The flash the FPGA boots from ([flash] in the target description).\n\
             set ::hns::flash_part      {{{}}}\n\
             set ::hns::flash_size_mb   {}\n\
             set ::hns::flash_interface {{{}}}\n",
            flash.part, flash.size_mb, flash.interface
        ));
    }
    out
}

/// `hns/syn/mcs.tcl`: the image to write into the flash, made from the
/// bitstream. Only for a target with `[flash]`.
pub fn mcs_tcl() -> String {
    let mut out = header_tcl();
    out.push_str(
        r#"#
# Make the flash image from the bitstream. Needs Vivado, not the board.
#
#   vivado -mode batch -source mcs.tcl

source [file join [file dirname [info script]] board.tcl]

set mcs [file rootname $::hns::bitstream].mcs
write_cfgmem -force -format mcs -size $::hns::flash_size_mb     -interface $::hns::flash_interface     -loadbit "up 0x0 $::hns::bitstream" -checksum -file $mcs
puts "wrote: $mcs"
"#,
    );
    out
}

/// `hns/syn/flash.tcl`: writes the image into the board's flash. Only for a
/// target with `[flash]`.
pub fn flash_tcl() -> String {
    let mut out = header_tcl();
    out.push_str(
        r#"#
# Write the flash image (`make mcs`) into the board's flash, check it, and start
# the FPGA from it. The board must be on this machine's JTAG cable. Vivado Lab
# is enough:
#
#   vivado_lab -mode batch -source flash.tcl [-tclargs <device pattern>]
#
# **The whole flash is erased first**: an image left over from before could
# still be started by a design that asks for it, so nothing is kept. Read the
# flash back first if what is on it matters.

source [file join [file dirname [info script]] board.tcl]

set mcs [file rootname $::hns::bitstream].mcs
set prm [file rootname $::hns::bitstream].prm
if {![file exists $mcs]} { error "no $mcs yet: run `make mcs` first" }
set pattern [expr {[llength $argv] > 0 ? [lindex $argv 0] : $::hns::device_pattern}]

open_hw_manager
connect_hw_server
# More than one FPGA may be connected. Taking the first one blindly would
# erase the flash of the board next door.
set dev ""
set seen [list]
foreach t [get_hw_targets] {
    open_hw_target -quiet $t
    foreach d [get_hw_devices] {
        lappend seen "$t: $d"
        if {[regexp $pattern $d]} { set dev $d; break }
    }
    if {$dev ne ""} { break }
    close_hw_target
}
if {$dev eq ""} { error "no device matching '$pattern'. Devices seen:
  [join $seen "
  "]" }
current_hw_device $dev
refresh_hw_device -update_hw_probes false $dev

create_hw_cfgmem -hw_device $dev [lindex [get_cfgmem_parts $::hns::flash_part] 0]
set cm [get_property PROGRAM.HW_CFGMEM $dev]
set_property PROGRAM.FILES [list $mcs] $cm
set_property PROGRAM.PRM_FILE $prm $cm
set_property PROGRAM.ADDRESS_RANGE {entire_device} $cm
set_property PROGRAM.ERASE 1 $cm
set_property PROGRAM.BLANK_CHECK 0 $cm
set_property PROGRAM.CFG_PROGRAM 1 $cm
set_property PROGRAM.VERIFY 1 $cm
set_property PROGRAM.CHECKSUM 0 $cm
set_property PROGRAM.UNUSED_PIN_TERMINATION {pull-none} $cm
# Vivado reaches the flash through a bridge of its own, loaded into the FPGA
# first. What was running is gone after this.
create_hw_bitstream -hw_device $dev [get_property PROGRAM.HW_CFGMEM_BITFILE $dev]
program_hw_devices $dev
refresh_hw_device $dev
program_hw_cfgmem -hw_cfgmem $cm
# Start again from the flash, as at power-up.
boot_hw_device $dev
refresh_hw_device -update_hw_probes false $dev
puts "flashed: $dev <- $mcs (DONE [get_property {REGISTER.CONFIG_STATUS.BIT[14]_DONE_PIN} $dev])"
close_hw_target
"#,
    );
    out
}

/// `hns/syn/svf.tcl`: writes the programming sequence as SVF. It needs no
/// hardware.
///
/// For a device with several SLRs this is the only way `hio` can program it.
/// The `.bit` is one bitstream per SLR joined together, and Xilinx does not
/// publish where they split, so the tool cannot split it.
///
/// The synthesis machine has Vivado anyway. With the SVF, the machine at the
/// FPGA needs no Vivado: `hio program <svf>` replays it, including the status
/// checks Vivado does, for any device family.
pub fn svf_tcl() -> String {
    let mut out = header_tcl();
    out.push_str(
        r#"#
# Write the programming sequence out as SVF, without touching hardware.
#
#   vivado -mode batch -source svf.tcl
#
# Then, on the machine with the board and no Vivado:
#
#   hio --target <board> program output/<top>.svf

source [file join [file dirname [info script]] board.tcl]

set svf [file rootname $::hns::bitstream].svf

open_hw_manager
# A target that is not a probe: it records instead of driving pins. A
# hw_server that is still running keeps the last run's target, and creating
# it again fails. The server must be connected before its targets show.
connect_hw_server
foreach old [get_hw_targets -quiet *hns_svf] {
    delete_hw_target $old
}
create_hw_target hns_svf
open_hw_target [get_hw_targets *hns_svf]
create_hw_device -part $::hns::part
# On a Zynq-7000 Vivado puts the ARM DAP on the chain too, first in the list.
set dev ""
foreach d [get_hw_devices] {
    if {[regexp $::hns::device_pattern $d]} { set dev $d; break }
}
if {$dev eq ""} {
    error "no device matching '$::hns::device_pattern' on the SVF target: [get_hw_devices]"
}
set_property PROGRAM.FILE $::hns::bitstream $dev
program_hw_devices $dev
# The last run's SVF is replaced: it belongs to the last bitstream.
write_hw_svf -force $svf
close_hw_target
puts "wrote: $svf"
"#,
    );
    out
}

/// `hns/syn/program.tcl`: programs the bitstream. It is self-contained apart
/// from `board.tcl`, which holds the board data.
///
/// Programming does not depend on the transport. Configuration goes through
/// the standard JTAG path, so Vivado can do it whatever reaches the window.
pub fn program_tcl() -> String {
    let mut out = header_tcl();
    out.push_str(
        r#"#
# Program the bitstream.
#
# With more than one FPGA connected, the device is picked by the hw_pattern in
# the target description. Pass a different pattern as an argument:
#
#   vivado -mode batch -source program.tcl -tclargs <pattern>
#
# Reading the window afterwards is not done from here: the harness reaches its
# own bridge over MPSSE, which Vivado cannot see. Use `hio` for that.

source [file join [file dirname [info script]] board.tcl]

namespace eval hns {
    # More than one FPGA may be connected. Open each target in turn and look for
    # the device we want. Taking the first one blindly ends up programming the
    # board next door.
    #
    # The target that matched is left open.
    proc find_device {{pattern ""}} {
        if {$pattern eq ""} { set pattern $::hns::device_pattern }
        set seen [list]
        foreach t [get_hw_targets] {
            open_hw_target -quiet $t
            foreach d [get_hw_devices] {
                lappend seen "$t: $d"
                if {[regexp $pattern $d]} { return $d }
            }
            close_hw_target
        }
        error "no device matching '$pattern' was found. Devices seen:\n  [join $seen "\n  "]"
    }

    proc program {{pattern ""}} {
        open_hw_manager
        connect_hw_server
        set dev [find_device $pattern]
        current_hw_device $dev
        if {$::hns::engineering_sample} {
            # An engineering sample trips the bitstream version check. This is never
            # relaxed for production silicon: doing so would hide an ES/production mix-up.
            puts "note: engineering sample device; relaxing the bitstream version check"
            set_param xicom.use_bitstream_version_check false
        }
        set_property PROGRAM.FILE $::hns::bitstream $dev
        program_hw_devices $dev
        refresh_hw_device -update_hw_probes false $dev
        puts "programmed: $dev <- $::hns::bitstream"
    }
}

hns::program [lindex $argv 0]
"#,
    );
    out
}

fn device_part(plan: &Plan) -> String {
    plan.target()
        .map(|target| target.head.device.part.clone())
        .unwrap_or_default()
}

fn dut_port<'a>(dut: &'a Dut, name: &str) -> &'a crate::dut::Port {
    dut.ports
        .iter()
        .find(|port| port.name == name)
        .expect("registers come from DUT ports")
}

/// The window address width. The rule lives in `hns-regs`: it must equal the
/// DR length on the host side, so there is only one definition.
fn bits_for(count: usize) -> usize {
    hns_regs::addr_bits(count)
}

/// A Markdown table with aligned columns, so it reads well in `cat` or `less`.
///
/// Widths count characters. The output is English plus `—`, so this matches
/// the display width.
fn markdown_table(header: &[&str], rows: &[Vec<String>]) -> String {
    let width: Vec<usize> = header
        .iter()
        .enumerate()
        .map(|(column, name)| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .chain(std::iter::once(name.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let line = |cells: &[String]| {
        let mut out = String::from("|");
        for (column, cell) in cells.iter().enumerate() {
            let pad = width[column].saturating_sub(cell.chars().count());
            out.push_str(&format!(" {cell}{} |", " ".repeat(pad)));
        }
        out.push('\n');
        out
    };

    let mut out = line(
        &header
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>(),
    );
    out.push('|');
    for w in &width {
        out.push_str(&"-".repeat(w + 2));
        out.push('|');
    }
    out.push('\n');
    for row in rows {
        out.push_str(&line(row));
    }
    out
}

/// A byte count for people: `8 KB`, `2 GB`, or `48 bytes`.
fn bytes_text(n: u64) -> String {
    for (unit, shift) in [("GB", 30), ("MB", 20), ("KB", 10)] {
        if n >= 1 << shift && n.is_multiple_of(1 << shift) {
            return format!("{} {unit}", n >> shift);
        }
    }
    format!("{n} bytes")
}

/// `hns/regs.md`: the register table for people.
///
/// It comes from the same IR as `regs.json`. It is the same data formatted for
/// people, not per-DUT accessor code.
///
/// It is not part of the map hash. Otherwise every wording change would change
/// the hash and break the match against a programmed bitstream.
///
/// The notes are fixed text chosen by `role`. Hand-written notes per bundle
/// would make a second source of truth next to the map.
pub fn regs_md(plan: &Plan) -> String {
    let map = &plan.registers;
    let mut out = format!("<!-- {MARKER} -->\n");
    out.push_str("<!-- Generated by veryl-harness. DO NOT EDIT -- `veryl harness gen` rewrites this file. -->\n\n");
    out.push_str(&format!("# {} — harness register map\n\n", map.dut));

    // The identity header is at the end of the window. Take the offsets from
    // the map; never write them by hand.
    let offset_of = |name: &str| {
        map.registers
            .iter()
            .find(|register| register.name == name)
            .map(|register| register.offset)
            .expect("the header is always placed")
    };
    let mut summary = vec![
        vec!["DUT".to_string(), format!("`{}`", map.dut)],
        vec![
            "magic".to_string(),
            format!(
                "`0x{:08x}` (read at offset 0x{:x})",
                crate::regmap::MAGIC,
                offset_of(crate::regmap::MAGIC_NAME)
            ),
        ],
        vec![
            "map hash".to_string(),
            format!(
                "`0x{:08x}` (read at offset 0x{:x}; `hio id` compares it)",
                map.map_hash,
                offset_of(crate::regmap::MAP_HASH_NAME)
            ),
        ],
        vec!["window".to_string(), format!("{} bytes", map.size_bytes())],
        vec!["word".to_string(), format!("{WORD_BITS} bit")],
    ];
    if let Some(target) = plan.target() {
        summary.push(vec!["target".to_string(), format!("`{}`", target.name)]);
    }
    out.push_str(&markdown_table(&["", ""], &summary));
    out.push('\n');

    // The regions: memory or a DUT interface at a range of the window, not a
    // register. In address order, so the gaps and a fixed `window_base` show.
    if !map.regions.is_empty() {
        let mut regions: Vec<_> = map.regions.iter().collect();
        regions.sort_by_key(|region| region.window_base);
        let rows: Vec<Vec<String>> = regions
            .iter()
            .map(|region| {
                let declared = &plan.loaded.manifest.bundle[&region.bundle];
                let mut notes = Vec::new();
                if declared.window_base.is_some() {
                    notes.push("`window_base` in Harness.toml".to_string());
                }
                notes.push(match region.kind {
                    crate::regmap::RegionKind::Dut => "DUT slave interface".to_string(),
                    crate::regmap::RegionKind::Dmi => {
                        "Debug Module (DMI); one word per register, `hio cpu`".to_string()
                    }
                    crate::regmap::RegionKind::Memory => format!("`{}`", declared.backing),
                    crate::regmap::RegionKind::Trace => {
                        "trace ring, read only; `hio trace`".to_string()
                    }
                });
                if region.is_aperture() {
                    notes.push(format!(
                        "window into {}; moved by `{}_base_*`",
                        bytes_text(region.total_bytes),
                        region.bundle
                    ));
                }
                vec![
                    format!(
                        "`0x{:04x}`..`0x{:04x}`",
                        region.window_base,
                        region.window_base + region.size_bytes - 1
                    ),
                    format!("`{}`", region.bundle),
                    region.kind.as_str().to_string(),
                    bytes_text(region.size_bytes as u64),
                    notes.join("; "),
                ]
            })
            .collect();
        out.push_str("Ranges of the window that are memory or a DUT interface, not registers.\n");
        out.push_str("Addresses inside one count from its start (`hio read <name> <offset>`).\n\n");
        out.push_str(&markdown_table(
            &["range", "name", "kind", "size", "notes"],
            &rows,
        ));
        out.push('\n');
    }

    out.push_str("Word `i` of a register holds bits `[32i+31 : 32i]`. A register wider than\n");
    out.push_str("one word occupies consecutive words, low word first.\n\n");

    let rows: Vec<Vec<String>> = map
        .registers
        .iter()
        .map(|register| {
            vec![
                format!("`0x{:04x}`", register.offset),
                format!("`{}`", register.name),
                register.access.as_str().to_string(),
                register.width.to_string(),
                register
                    .bundle
                    .as_deref()
                    .map(|bundle| format!("`{bundle}`"))
                    .unwrap_or_else(|| "—".to_string()),
                register
                    .role
                    .map(|role| format!("`{role}`"))
                    .unwrap_or_else(|| "—".to_string()),
                register_notes(plan, register),
            ]
        })
        .collect();
    out.push_str(&markdown_table(
        &["offset", "name", "acc", "width", "bundle", "role", "notes"],
        &rows,
    ));
    out.push('\n');

    out.push_str("## Reaching these registers\n\n");
    out.push_str("The window is reached over the transport the harness was generated for. Over\n");
    out.push_str("JTAG that is `hio`, which talks to the harness's own bridge with MPSSE\n");
    out.push_str("(Vivado cannot see it, so hw_server is not involved):\n\n");
    out.push_str("```\nhio id                  # magic and map hash must match this file\nhio read  <name>\nhio write <name> <value>\nhio drain <bundle>\n```\n\n");
    out.push_str("It needs the probe's USB details as well; see the manual.\n\n");
    out.push_str("`regs.json` carries the same data for programs; this file is the same map\n");
    out.push_str("rendered for people. Neither is edited by hand -- both come from the IR.\n");
    out
}

/// What one TLP count counts, for regs.md.
fn tlp_counter_note(role: &str) -> &'static str {
    match role {
        "pcie_mrd" => "memory reads that reached the BAR, of any length",
        "pcie_mwr" => "memory writes that reached the BAR, of any length",
        "pcie_other" => "other requests that reached the BAR (I/O, atomics, messages)",
        "pcie_mwr_dropped" => {
            "posted requests the completer dropped without a trace: memory writes longer than one dword, and messages"
        }
        "pcie_cpl_sc" => "completions sent with Successful Completion",
        "pcie_cpl_ur" => "completions sent with Unsupported Request",
        "pcie_cpl_ca" => {
            "completions sent with Completer Abort; a read longer than one dword gets one"
        }
        _ => unreachable!("not a TLP count: {role}"),
    }
}

/// What all the TLP counts share, for regs.md.
const TLP_COUNTER_NOTE: &str = "All `pcie_*` counts saturate, and a link reset does not clear them. A request the PCIe block refuses itself (no BAR hit, Memory Space off) never reaches them: see the function's Device Status. A read over PCIe counts itself; read over JTAG to leave them alone";

/// The notes column. It is derived from `role`, so it cannot disagree with
/// the map.
fn register_notes(plan: &Plan, register: &crate::regmap::Register) -> String {
    let mut notes: Vec<String> = Vec::new();

    if let Some(value) = register.value {
        notes.push(format!("constant `0x{value:08x}`"));
    }
    // Self-clearing is noted only for DUT ports. For terminators the role text
    // below already says it.
    if let Some(clear) = &register.self_clearing
        && matches!(clear.source, ClearSource::DutPort(_))
    {
        let invert = if clear.invert { "!" } else { "" };
        notes.push(format!(
            "self-clearing: one write sends exactly one beat, and it returns to 0 once the {} (`{invert}{}`) takes it",
            clear.role,
            clear.on()
        ));
    }

    let bundle = register.bundle.as_deref().unwrap_or("");
    if register.name == crate::regmap::TIMEOUT_NAME {
        notes.push(
            "times the window answered for a terminator that did not; those reads returned 0. Write 1 to clear".to_string(),
        );
    }
    match register.role {
        Some("data") => notes.push(format!(
            "oldest entry; only meaningful while `{bundle}_level` is not 0. Reading does not consume it"
        )),
        Some("level") => notes.push("entries waiting to be read".to_string()),
        Some("irq_level") => notes.push(
            "the DUT's interrupt line now; each change is sent to the host as Assert_INTA or Deassert_INTA".to_string(),
        ),
        Some("depth") => notes.push("entries the terminator holds".to_string()),
        Some("run") => notes.push(
            "1 records into the ring; writing 1 after 0 starts over from an empty ring".to_string(),
        ),
        Some("pause") => notes.push(
            "1 holds the recording without starting over; `hio cpu` sets it while the hart is halted".to_string(),
        ),
        Some("count") => notes.push(format!(
            "records written since the start; wraps at 2^32, so `{bundle}_wrapped` tells a full ring"
        )),
        Some("arm") => notes.push(
            "1 at the start: the first beat that matches `_tmask` / `_tvalue` triggers (with no mask, the first beat)".to_string(),
        ),
        Some("after") => notes.push(
            "records kept after the trigger; then the ring freezes".to_string(),
        ),
        Some("tmask") => notes.push(
            "the bits of the trigger fields, then one \"not 0\" bit for each field wider than 1, that must match; `hio trace start --when` writes it".to_string(),
        ),
        Some("tvalue") => notes.push("what those bits must be".to_string()),
        Some("hit") => notes.push("1 once the trigger has matched".to_string()),
        Some("hit_at") => notes.push(
            "the record number of the trigger, counted from the start".to_string(),
        ),
        Some("wrapped") => notes.push(
            "1 once the ring is full: the oldest records are being overwritten".to_string(),
        ),
        Some("drops") => notes.push(
            "beats lost because the FIFO was full; saturates instead of wrapping".to_string(),
        ),
        Some("slverr") => notes.push(
            "APB transfers the DUT ended with `pslverr`; saturates instead of wrapping".to_string(),
        ),
        Some("pop") => notes.push(format!(
            "write 1 to take one entry; stays 1 while `{bundle}_level` is 0 and takes the next one that arrives"
        )),
        Some("maddr") => notes.push(format!(
            "address into the memory; advances by one when a `{bundle}_mdata` entry is committed, and never on a read"
        )),
        Some("mdata") => {
            notes.push(format!(
                "the word at `{bundle}_maddr`. Reading returns what the memory holds"
            ));
            if register.words > 1 {
                notes.push(format!(
                    "write the {} words low to high: the last one commits the entry and advances `{bundle}_maddr`",
                    register.words
                ));
            } else {
                notes.push(format!("writing it stores the entry and advances `{bundle}_maddr`"));
            }
        }
        Some("oor")
            if plan
                .axi_mems
                .iter()
                .any(|mem| Some(mem.bundle.as_str()) == register.bundle.as_deref()) =>
        {
            notes.push(
                "AXI4 transfers the DUT made outside the memory (below `dut_base` or past the end); they were answered with DECERR".to_string(),
            )
        }
        // Not a fixed-latency memory: do not talk about cycles holding an
        // address. A transfer-level port counts per command.
        Some("oor") if !plan
            .memories
            .iter()
            .any(|mem| Some(mem.bundle.as_str()) == register.bundle.as_deref()) =>
        {
            notes.push(
                "requests the DUT made beyond the memory; those reads return 0 and their writes are dropped".to_string(),
            )
        }
        Some("oor") => {
            let counts = plan
                .memories
                .iter()
                .find(|mem| Some(mem.bundle.as_str()) == register.bundle.as_deref())
                .is_some_and(|mem| mem.counts_accesses());
            notes.push(if counts {
                "accesses the DUT made at or above the depth; those reads return 0 and their writes are dropped. The address is never folded back into range. An enable held high counts every cycle, so read it as \"it happened\", not as a transaction count".to_string()
            } else {
                "cycles the DUT held an address at or above the depth (the port has no read enable, so an access cannot be told from an idle cycle); those reads return 0 and their writes are dropped. The address is never folded back into range".to_string()
            });
        }
        Some("refused") => notes.push(
            "pzcorebus commands the DUT made that are not a read or a write (broadcast, atomic, message). They were not run: their data was dropped, and a non-posted one was answered with an error".to_string(),
        ),
        Some("calib") if is_ps7(plan) => notes.push(
            "1 once the PS is set up (`hio program` runs ps7_init after the bitstream). Nothing reaches the memory before that".to_string(),
        ),
        Some("calib") => notes.push(
            "1 once the memory controller has calibrated. Nothing reaches the memory before that".to_string(),
        ),
        Some("base_jtag") | Some("base_pcie") => notes.push(format!(
            "where `{bundle}`'s window starts in the memory, for this transport. `hio` moves it for you"
        )),
        Some("delay") => notes.push(
            "cycles to hold each answer back, to make the DUT wait on purpose. 0 by default".to_string(),
        ),
        Some("jitter") => notes.push("1 varies the hold-back from answer to answer".to_string()),
        Some(role) if crate::regmap::PCIE_TLP_COUNTERS
            .iter()
            .any(|(name, _)| *name == role) =>
        {
            // The shared text once, on the first count.
            if role == crate::regmap::PCIE_TLP_COUNTERS[0].0 {
                notes.push(format!("{}. {TLP_COUNTER_NOTE}", tlp_counter_note(role)));
            } else {
                notes.push(format!(
                    "{}. Counted like `{}`",
                    tlp_counter_note(role),
                    crate::regmap::PCIE_TLP_COUNTERS[0].0
                ));
            }
        }
        Some(DUT_RESET) => notes.push(
            "1 holds the DUT in reset. The harness, memory contents and `reg` registers are not reset. Use `hio reset`".to_string(),
        ),
        Some(DUT_RESET_STATE) => notes.push(
            "1 while the DUT is in reset. After writing `dut_reset`, read this until it changes".to_string(),
        ),
        _ => {}
    }

    // The memory latency is part of the contract with the DUT.
    if let Some(mem) = plan
        .memories
        .iter()
        .find(|mem| Some(mem.bundle.as_str()) == register.bundle.as_deref())
        && register.role == Some("mdata")
    {
        notes.push(format!(
            "the DUT side reads it with a latency of {}{}",
            match mem.latency {
                0 => "0 cycles (combinational)".to_string(),
                1 => "1 cycle".to_string(),
                n => format!("{n} cycles"),
            },
            if mem.read_only {
                ", and never writes it"
            } else {
                ""
            }
        ));
    }

    if notes.is_empty() {
        "—".to_string()
    } else {
        notes.join("; ")
    }
}

fn header_veryl() -> String {
    format!(
        "/// {MARKER}\n///\n/// Generated by veryl-harness. DO NOT EDIT --\n/// `veryl harness gen` rewrites this file.\n"
    )
}

fn header_tcl() -> String {
    format!(
        "# {MARKER}\n#\n# Generated by veryl-harness. DO NOT EDIT --\n# `veryl harness gen` rewrites this file.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{ClockOutput, InputClock, InputReset};

    #[test]
    fn the_copied_mig_prj_carries_the_marker_after_the_declaration() {
        let xml = "<?xml version='1.0' encoding='UTF-8'?>\n<!-- MIG -->\n<Project/>\n";
        let marked = mark_xml(xml, "targets/a--b/mig.prj");
        let lines: Vec<&str> = marked.lines().collect();
        assert_eq!(lines[0], "<?xml version='1.0' encoding='UTF-8'?>");
        assert!(lines[1].contains(MARKER), "{marked}");
        assert!(!lines[1][4..lines[1].len() - 3].contains("--"), "{marked}");
        assert_eq!(&lines[2..], ["<!-- MIG -->", "<Project/>"]);

        // A byte order mark stays first, before the declaration.
        let marked = mark_xml(&format!("\u{feff}{xml}"), "mig.prj");
        assert!(marked.starts_with("\u{feff}<?xml"), "{marked}");
        assert!(marked.lines().nth(1).unwrap().contains(MARKER), "{marked}");
    }

    fn plan(outputs: Vec<ClockOutput>) -> ClockPlan {
        ClockPlan {
            window: outputs[0].ident.clone(),
            controller_clock: None,
            input: InputClock {
                name: "sys".to_string(),
                freq_mhz: 100.0,
                pin: Some("E3".to_string()),
                pin_n: None,
                standard: Some("LVCMOS33".to_string()),
                diff: false,
            },
            reset: InputReset {
                name: "sys".to_string(),
                pin: Some("C2".to_string()),
                standard: Some("LVCMOS33".to_string()),
                active_low: true,
            },
            outputs,
            resets: vec![("i_rst".to_string(), "c0".to_string())],
        }
    }

    fn output(ident: &str, domain: &str, freq: f64, ports: &[&str]) -> ClockOutput {
        ClockOutput {
            ports: ports.iter().map(|x| x.to_string()).collect(),
            freq_mhz: freq,
            domain: domain.to_string(),
            ident: ident.to_string(),
        }
    }

    #[test]
    fn the_generated_veryl_carries_the_marker() {
        let text = clock_module(
            &plan(vec![output("c0", "-", 200.0, &["i_clk"])]),
            ResetType::AsyncLow,
            "c0",
        );

        // Without the marker, `gen` cannot rewrite its own output.
        assert!(text.lines().next().unwrap().contains(MARKER));
    }

    /// The crossings are wrapped in `unsafe (cdc)`. The compiler rejects a
    /// missing wrap but not an extra one, so the count is fixed here.
    #[test]
    fn both_crossings_are_declared_unsafe_cdc() {
        let text = clock_module(
            &plan(vec![output("c0", "-", 200.0, &["i_clk"])]),
            ResetType::AsyncLow,
            "c0",
        );

        assert_eq!(text.matches("unsafe (cdc)").count(), 3);
        // The MMCM instance, and the harness and DUT reset synchronizers.
        assert!(text.contains("inst u_mmcm: $sv::hns_mmcm"));
        assert_eq!(text.matches("always_ff (o_clk_c0, i_sys_rst)").count(), 2);
        // Only the DUT reset follows the host request.
        assert!(
            text.contains("drst_meta_c0 = locked & ~i_dut_rst;"),
            "{text}"
        );
        assert!(text.contains("rst_meta_c0 = locked;"), "{text}");
    }

    #[test]
    fn two_domains_get_two_outputs_and_two_synchronizers() {
        let text = clock_module(
            &plan(vec![
                output("s", "'s", 100.0, &["is_clk"]),
                output("d", "'d", 50.0, &["id_clk"]),
            ]),
            ResetType::AsyncLow,
            "s",
        );

        assert!(text.contains("clk_out1: o_clk_s"));
        assert!(text.contains("clk_out2: o_clk_d"));
        // The MMCM, plus harness and DUT synchronizers per domain.
        assert_eq!(text.matches("unsafe (cdc)").count(), 5);
        assert!(text.contains("i_dut_rst: input 's logic"), "{text}");
        assert!(
            text.contains("o_drst_d: output 'd reset_async_low"),
            "{text}"
        );
        assert!(text.contains("o_clk_s: output 's clock"));
        assert!(text.contains("o_clk_d: output 'd clock"));
    }

    /// The board reset polarity comes from the target and is never inferred.
    #[test]
    fn the_reset_polarity_comes_from_the_target() {
        let mut board = plan(vec![output("c0", "-", 200.0, &["i_clk"])]);
        let text = clock_module(&board, ResetType::AsyncLow, "c0");
        assert!(
            text.contains("i_sys_rst: input 'sys reset_async_low"),
            "{text}"
        );
        assert!(text.contains("resetn: i_sys_rst"), "{text}");
        assert!(mmcm_tcl(&board).contains("CONFIG.RESET_TYPE {ACTIVE_LOW}"));

        // An active-high board reset does not change the DUT polarity.
        board.reset.active_low = false;
        let text = clock_module(&board, ResetType::AsyncLow, "c0");
        assert!(
            text.contains("i_sys_rst: input 'sys reset_async_high"),
            "{text}"
        );
        // The IP reset port name changes too (measured).
        assert!(text.contains("reset: i_sys_rst"), "{text}");
        // The outputs keep the DUT polarity.
        assert!(
            text.contains("o_rst_c0: output 'c0 reset_async_low"),
            "{text}"
        );
        assert!(mmcm_tcl(&board).contains("CONFIG.RESET_TYPE {ACTIVE_HIGH}"));
    }

    /// The Tcl has no M/D/O; Vivado solves them.
    #[test]
    fn the_tcl_asks_for_frequencies_not_dividers() {
        let text = mmcm_tcl(&plan(vec![
            output("s", "'s", 100.0, &["is_clk"]),
            output("d", "'d", 50.0, &["id_clk"]),
        ]));

        assert!(text.contains("CONFIG.PRIM_IN_FREQ {100.000}"));
        assert!(text.contains("CONFIG.CLKOUT1_REQUESTED_OUT_FREQ {100.000}"));
        assert!(text.contains("CONFIG.CLKOUT2_REQUESTED_OUT_FREQ {50.000}"));
        assert!(!text.contains("CLKFBOUT_MULT"));
        assert!(!text.contains("DIVCLK_DIVIDE"));
        assert!(!text.contains("CLKOUT1_DIVIDE"));
    }
}

#[cfg(test)]
mod pcie_emit_tests {
    use super::*;

    /// The `bar_bytes` written is the BAR the IP gets. A mismatch between
    /// `regs.json` and the real BAR0 makes `hio` reject the card.
    #[test]
    fn the_bar_given_to_the_ip_is_the_one_written() {
        let mut bytes = crate::manifest::MIN_BAR_BYTES;
        while bytes <= 1 << 30 {
            let (scale, size) = bar_scale(bytes);
            let unit = match scale {
                "Kilobytes" => 1024,
                "Megabytes" => 1024 * 1024,
                other => panic!("unknown scale {other}"),
            };
            assert_eq!(size * unit, bytes, "{bytes}");
            bytes <<= 1;
        }
    }

    fn board() -> hns_targets::Pcie {
        hns_targets::pcie(&hns_targets::resolve("xilinx/vcu118", &[]).unwrap())
    }

    /// Writes the generated XDC for a real Vivado run. It does nothing unless
    /// `HNS_DUMP_PCIE` is set.
    #[test]
    fn dump_for_a_real_vivado_run() {
        let Ok(dir) = std::env::var("HNS_DUMP_PCIE") else {
            return;
        };
        let b = board();
        std::fs::write(format!("{dir}/pcie.xdc"), pcie_xdc(&b)).unwrap();
    }

    /// The board facts reach the XDC unchanged. A mistake here still
    /// synthesizes, but the link never comes up, which is hard to debug.
    #[test]
    fn the_xdc_names_every_lane_once() {
        let b = board();
        let xdc = pcie_xdc(&b);
        assert!(xdc.contains("PACKAGE_PIN AC9"), "refclk missing:\n{xdc}");
        assert!(xdc.contains("PACKAGE_PIN AM17"), "reset missing");
        assert!(xdc.contains("create_clock -period 10.000"), "100MHz refclk");
        assert!(
            xdc.contains("set_false_path"),
            "reset arrives asynchronously"
        );
        for (i, pin) in b.rx_p.iter().enumerate() {
            assert!(
                xdc.contains(&format!(
                    "PACKAGE_PIN {pin} }} [get_ports {{ i_pcie_rx_p[{i}]"
                )),
                "lane {i} ({pin}) is not placed"
            );
        }
        assert_eq!(
            b.rx_p.len() + b.tx_p.len() + 2,
            xdc.matches("PACKAGE_PIN").count()
        );
    }
}

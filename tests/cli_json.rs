//! End-to-end tests for `check --json`.
//!
//! They pin the contract more than the content:
//!
//! - stdout is exactly one JSON document, with no lines for humans mixed in
//! - success and failure have the same shape; the exit code is 0 or 1
//! - a failure carries `code`, which agents use to branch

use std::fs;
use std::process::Command;

/// A CSR is a fixed-latency terminator with no handshake. `contract` must be
/// stated, because leaving it out asks for `valid_ready`.
const CSR_MANIFEST: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n";

/// Creates a Veryl project and a `Harness.toml` in a temporary directory.
fn fixture(harness_toml: &str) -> tempfile::TempDir {
    fixture_with_dut(harness_toml, None)
}

/// Like `fixture`, but the DUT source can be replaced. For tests that need
/// ports the shared DUT does not have, such as a transfer-level port.
fn fixture_with_dut(harness_toml: &str, dut: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Veryl.toml"),
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[build]\nreset_type = \"async_low\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("Harness.toml"), harness_toml).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    let source = dut.map(str::to_string).unwrap_or_else(|| {
        r#"
module dut_top (
    i_clk      : input  clock   ,
    i_rst      : input  reset   ,
    i_csr_wdata: input  logic<8>,
    o_csr_rdata: output logic<8>,
) {
    always_ff {
        if_reset {
            o_csr_rdata = 0;
        } else {
            o_csr_rdata = i_csr_wdata;
        }
    }
}
"#
        .to_string()
    });
    fs::write(dir.path().join("src").join("fixture.veryl"), source).unwrap();
    dir
}

fn check_json(dir: &tempfile::TempDir) -> (serde_json::Value, Option<i32>) {
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout is not a single JSON document ({err}):\n{stdout}"));
    (value, output.status.code())
}

#[test]
fn a_successful_check_is_one_json_document() {
    let dir = fixture(CSR_MANIFEST);
    let (value, code) = check_json(&dir);

    assert_eq!(code, Some(0));
    assert_eq!(value["status"], "ok");
    assert_eq!(value["format_version"], 14);
    assert_eq!(value["dut"]["module"], "dut_top");
    assert_eq!(value["bundles"][0]["name"], "csr");
    assert_eq!(value["bundles"][0]["matched_by"], "naming");
    assert_eq!(value["bundles"][0]["contract"], "fixed_latency");
    assert_eq!(value["bundles"][0]["contract_declared"], true);
    // A port the dictionary does not match is payload. It never becomes a
    // handshake signal.
    assert_eq!(value["bundles"][0]["roles"][0]["source"], "payload");
    // Clock and reset do not belong to a bundle.
    assert!(value["dut"]["ports"][0]["bundle"].is_null());

    // What was not checked is always listed, so that exit 0 is not read as
    // "a harness can be built".
    let not_checked = value["not_checked"].as_array().unwrap();
    assert!(!not_checked.is_empty());
    assert!(
        not_checked.iter().any(|item| item["id"] == "feasibility"),
        "not_checked was: {not_checked:?}"
    );
    assert!(!value["checked"].as_array().unwrap().is_empty());
}

#[test]
fn a_failing_check_returns_the_diagnostic_code() {
    // Points at a module that does not exist.
    let dir = fixture(
        "[dut]\nmodule = \"nope\"\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n",
    );
    let (value, code) = check_json(&dir);

    assert_eq!(code, Some(1));
    assert_eq!(value["status"], "error");
    assert_eq!(value["error"]["code"], "harness::dut::not_found");
    // The help says how to fix it.
    assert!(value["error"]["help"].as_str().unwrap().contains("dut_top"));
}

/// A rejection at the CLI level is JSON too. Otherwise the agent's parser
/// breaks only on errors.
#[test]
fn a_cli_level_rejection_is_reported_as_json_too() {
    let dir = fixture(CSR_MANIFEST);
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--target-patch", "p.toml"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::cli::patch_without_target");
}

/// An unknown target name is JSON too, and lists the targets that exist.
#[test]
fn an_unknown_target_is_reported_as_json_too() {
    let dir = fixture(CSR_MANIFEST);
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--target", "digilent/nope"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::target::not_found");
    assert!(
        value["error"]["help"]
            .as_str()
            .unwrap()
            .contains("digilent/arty-a7-35")
    );
}

/// A class code a host driver binds to passes, with a warning that names the
/// driver. The default class (and a class no driver claims) has none, and the
/// map carries the code either way.
#[test]
fn a_class_code_a_driver_binds_to_is_a_warning() {
    let run = |class_code: Option<&str>| {
        let mut manifest = CSR_MANIFEST.to_string();
        if let Some(code) = class_code {
            manifest.push_str(&format!("\n[pcie]\nclass_code = {code}\n"));
        }
        let dir = fixture(&manifest);
        let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
            .args([
                "check",
                "--json",
                "--target",
                "xilinx/vcu118",
                "--transport",
                "pcie",
            ])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{value:#}");
        value
    };

    let nvme = run(Some("0x010802"));
    let warning = &nvme["warnings"][0];
    assert_eq!(warning["id"], "pcie_class_driver");
    assert!(
        warning["what"].as_str().unwrap().contains("nvme"),
        "{warning}"
    );
    assert_eq!(nvme["registers"]["pcie"]["class_code"], 0x010802);

    for quiet in [None, Some("0x120000")] {
        let value = run(quiet);
        assert!(value.get("warnings").is_none(), "{quiet:?}: {value:#}");
    }
    assert_eq!(run(None)["registers"]["pcie"]["class_code"], 0xff0000);
}

/// A class code wider than 24 bits is refused.
#[test]
fn a_class_code_wider_than_24_bits_is_refused() {
    let dir = fixture(&format!("{CSR_MANIFEST}\n[pcie]\nclass_code = 0x1000000\n"));
    let (value, code) = check_json(&dir);
    assert_ne!(code, Some(0));
    assert_eq!(
        value["error"]["code"],
        "harness::manifest::class_code_too_wide"
    );
}

/// An installed binary finds the shipped targets. The descriptions are embedded
/// at build time, so this must work without knowing where the repository is.
#[test]
fn a_shipped_target_resolves_and_is_reported_as_verified() {
    let dir = fixture(CSR_MANIFEST);
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(value["target"]["name"], "digilent/arty-a7-35");
    assert_eq!(value["target"]["source"], "targets");
    assert_eq!(value["target"]["verified"], true);
    // The description itself is shown, untyped because its schema is not fixed yet.
    assert_eq!(
        value["target"]["description"]["device"]["part"],
        "xc7a35ticsg324-1L"
    );

    // With a target, the feasibility check runs. Arty has one transport, so
    // `--transport` is not needed.
    assert_eq!(value["feasibility"]["transport"], "jtag");
    assert_eq!(value["feasibility"]["bundles"][0]["bundle"], "csr");

    // The clock plan is shown too. It has no M/D/O values: Vivado solves them.
    assert_eq!(value["clocks"]["input"]["freq_mhz"], 100.0);
    assert_eq!(value["clocks"]["input"]["pin"], "E3");
    assert_eq!(value["clocks"]["outputs"][0]["freq_mhz"], 200.0);
    assert_eq!(value["clocks"]["outputs"][0]["ports"][0], "i_clk");
}

/// A clock port with no frequency is an error. No default is used.
#[test]
fn a_clock_port_without_a_frequency_is_rejected() {
    let dir = fixture(
        "[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::clock::missing_frequency");
    assert!(
        value["error"]["help"]
            .as_str()
            .unwrap()
            .contains("[clock.i_clk]"),
        "the message must show what to write"
    );
}

/// A combination that cannot work is rejected before anything is generated.
#[test]
fn an_impossible_combination_is_rejected_before_anything_is_generated() {
    // Asks for real host memory on Arty, which has only jtag. With `source`,
    // a BRAM could stand in; this test leaves `source` out on purpose.
    let dir = fixture_with_dut(
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n\
         [bundle.hmem]\nbacking = \"host_mem\"\n\
         ports = { rd_cmd_valid = \"o_cmd_valid\", rd_cmd_ready = \"i_cmd_ready\", \
         rd_cmd_addr = \"o_cmd_addr\", rd_cmd_size = \"o_cmd_nbytes\", \
         rd_valid = \"i_rd_valid\", rd_ready = \"o_rd_ready\", \
         rd_data = \"i_rd_data\", rd_last = \"i_rd_last\" }\n",
        Some(
            r#"
module dut_top (
    i_clk       : input  clock    ,
    i_rst       : input  reset    ,
    o_cmd_valid : output logic    ,
    i_cmd_ready : input  logic    ,
    o_cmd_addr  : output logic<64>,
    o_cmd_nbytes: output logic<32>,
    i_rd_valid  : input  logic    ,
    o_rd_ready  : output logic    ,
    i_rd_data   : input  logic<64>,
    i_rd_last   : input  logic    ,
) {
    assign o_cmd_valid  = 0;
    assign o_cmd_addr   = 0;
    assign o_cmd_nbytes = 0;
    assign o_rd_ready   = 0;
    let _unused: logic = i_clk | i_rst | i_cmd_ready | i_rd_valid | i_rd_data[0] | i_rd_last;
}
"#,
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    // Arty has only jtag, so a backing that needs PCIe fails here first.
    assert_eq!(value["error"]["code"], "harness::feasibility::needs_pcie");
    assert!(
        value["error"]["help"].as_str().unwrap().contains("bram"),
        "the message must say what can be used instead"
    );
}

/// A project whose DUT only sends a stream (`host_poll_fifo` terminates outputs).
fn stream_fixture(harness_toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Veryl.toml"),
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[build]\nreset_type = \"async_low\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("Harness.toml"), harness_toml).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk  : input  clock   ,
    i_rst  : input  reset   ,
    o_valid: output logic   ,
    o_data : output logic<8>,
) {
    var count: logic<8>;
    always_ff {
        if_reset {
            count = 0;
        } else {
            count = count + 1;
        }
    }
    assign o_valid = 1;
    assign o_data  = count;
}
"#,
    )
    .unwrap();
    dir
}

/// A lossy terminator is not rejected. It passes with a stated requirement.
#[test]
fn a_lossy_termination_passes_with_a_stated_requirement() {
    let dir = stream_fixture(
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.tx]\ncontract = \"valid_only\"\nbacking = \"host_poll_fifo\"\nports = { valid = \"o_valid\", data = \"o_data\" }\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "stdout: {value}");
    assert_eq!(
        value["feasibility"]["bundles"][0]["requirements"][0]["id"],
        "drop_counter"
    );
}

/// A patched target is reported as unverified.
#[test]
fn a_patched_target_is_reported_as_unverified() {
    let dir = fixture(CSR_MANIFEST);
    fs::write(
        dir.path().join("rev-c.toml"),
        "[provides.dram]\nchannels = 0\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args([
            "check",
            "--json",
            "--target",
            "digilent/arty-a7-35",
            "--target-patch",
            "rev-c.toml",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(value["target"]["verified"], false);
    assert!(
        !value["target"]["unverified_reasons"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // The patch took effect.
    assert_eq!(
        value["target"]["description"]["provides"]["dram"]["channels"],
        0
    );
}

/// The register map written by `--emit-regs` is the same as the one in
/// `check --json`. Two different forms would leave no single source of truth.
#[test]
fn the_register_map_is_the_same_in_json_and_in_the_emitted_file() {
    let dir = fixture(CSR_MANIFEST);
    let emitted = dir.path().join("regs.json");

    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--emit-regs", emitted.to_str().unwrap()])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0));

    let from_file: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&emitted).unwrap()).unwrap();
    assert_eq!(value["registers"], from_file);

    // reg ports start at offset 0 in declaration order. The direction sets `access`.
    let registers = from_file["registers"].as_array().unwrap();
    assert_eq!(registers[0]["name"], "i_csr_wdata");
    assert_eq!(registers[0]["offset"], 0);
    assert_eq!(registers[0]["access"], "rw");
    assert_eq!(registers[1]["name"], "o_csr_rdata");
    assert_eq!(registers[1]["access"], "ro");

    // The identity header is at the end of the window, so user registers can
    // start at offset 0. The host reads it to check that its map matches the
    // loaded bitstream.
    let size = from_file["size_bytes"].as_u64().unwrap();
    let last = registers.last().unwrap();
    assert_eq!(registers[registers.len() - 2]["name"], "harness_magic");
    assert_eq!(registers[registers.len() - 2]["offset"], size - 8);
    assert_eq!(last["name"], "harness_map_hash");
    assert_eq!(last["offset"], size - 4);
    assert_eq!(last["value"], from_file["map_hash"]);
}

/// A bundle whose shape no terminator accepts must not pass through. Otherwise
/// `gen` connects undeclared wires to the DUT and only `veryl build` fails.
/// `check` must stop it and say how to fix it.
#[test]
fn a_bundle_no_terminator_takes_is_refused_by_check() {
    const DUT: &str = r#"
module dut_top (
    i_clk      : input  clock    ,
    i_rst      : input  reset    ,
    o_mem_addr : output logic<8> ,
    i_mem_rdata: input  logic<32>,
    o_mem_valid: output logic    ,
    i_mem_ready: input  logic    ,
    i_csr_wdata: input  logic<8> ,
) {
    assign o_mem_addr  = 0;
    assign o_mem_valid = 0;
    let _unused: logic = i_clk | i_rst | i_mem_rdata[0] | i_mem_ready | i_csr_wdata[0];
}
"#;
    const HEAD: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n\
                        [bundle.csr]\nbacking = \"reg\"\n\n";
    let fixed = "addressing = \"word\"\nports = { addr = \"o_mem_addr\", rdata = \"i_mem_rdata\" }\n\
                 \n[leave_open]\nports = [\"o_mem_valid\"]\n\n[tie_off]\ni_mem_ready = 0\n";
    let shaken = "ports = { addr = \"o_mem_addr\", rdata = \"i_mem_rdata\", \
                  valid = \"o_mem_valid\", ready = \"i_mem_ready\" }\n";
    let cases = [
        // A fixed-latency BRAM with no `latency`. It must not default to 0.
        (
            format!("[bundle.mem]\nbacking = \"bram\"\n{fixed}"),
            "harness::terminator::latency_required",
            "latency = 1",
        ),
        // A handshake, but no transfer-level roles.
        (
            format!("[bundle.mem]\nbacking = \"bram\"\n{shaken}"),
            "harness::terminator::memory_contract",
            "rd_cmd_",
        ),
        // With no contract given, `latency` is checked against the contract
        // the ports imply.
        (
            format!("[bundle.mem]\nbacking = \"bram\"\nlatency = 1\n{shaken}"),
            "harness::contract::latency_not_allowed",
            "Remove `latency`",
        ),
        // A `slave` with no `addr`. The error must not talk about `rdata`.
        (
            "[bundle.mem]\nbacking = \"slave\"\nlatency = 1\n\
             ports = { rdata = \"i_mem_rdata\" }\n\n\
             [leave_open]\nports = [\"o_mem_addr\", \"o_mem_valid\"]\n\n[tie_off]\ni_mem_ready = 0\n"
                .to_string(),
            "harness::terminator::slave_needs_addr",
            "addr",
        ),
        // `dram` works only over AXI4 (here the ports are matched by name).
        (
            "[bundle.mem]\nbacking = \"dram\"\n".to_string(),
            "harness::terminator::dram_needs_axi4",
            "bram",
        ),
        // A key that has no effect is refused.
        (
            "[bundle.mem]\nbacking = \"reg\"\naddressing = \"byte\"\n\
             ports = [\"o_mem_addr\", \"i_mem_rdata\"]\n\n\
             [leave_open]\nports = [\"o_mem_valid\"]\n\n[tie_off]\ni_mem_ready = 0\n"
                .to_string(),
            "harness::terminator::key_not_applicable",
            "Remove `addressing`",
        ),
    ];
    for (bundle, code, help) in cases {
        let dir = fixture_with_dut(&format!("{HEAD}{bundle}"), Some(DUT));
        let (value, status) = check_json(&dir);
        assert_eq!(status, Some(1), "{bundle}\n{value:#}");
        assert_eq!(value["error"]["code"], code, "{bundle}\n{value:#}");
        let text = value["error"]["help"].as_str().unwrap_or_default();
        assert!(text.contains(help), "{bundle}\n{text}");
    }

    // A complete bundle passes.
    let dir = fixture_with_dut(
        &format!("{HEAD}[bundle.mem]\nbacking = \"bram\"\nlatency = 1\n{fixed}"),
        Some(DUT),
    );
    let (value, status) = check_json(&dir);
    assert_eq!(status, Some(0), "{value:#}");
}

/// `check --target` refuses what `gen` refuses. `exclude_std` matters only for
/// generation (the output's `hns` needs std), so plain `check` accepts it.
#[test]
fn check_with_a_target_refuses_what_gen_would() {
    let dir = fixture(CSR_MANIFEST);
    let veryl = dir.path().join("Veryl.toml");
    let text = fs::read_to_string(&veryl)
        .unwrap()
        .replace("[build]\n", "[build]\nexclude_std = true\n");
    fs::write(&veryl, text).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{value:#}");
    assert_eq!(value["error"]["code"], "harness::plan::exclude_std");

    let (value, code) = check_json(&dir);
    assert_eq!(code, Some(0), "{value:#}");
}

/// Every resolved plan appears in `--json`, including `slave` and the
/// heartbeat, so an agent can check what was resolved.
#[test]
fn every_resolved_plan_reaches_the_json() {
    const DUT: &str = r#"
module dut_top (
    i_clk      : input  clock    ,
    i_rst      : input  reset    ,
    i_regs_addr : input  logic<4> ,
    o_regs_rdata: output logic<32>,
) {
    assign o_regs_rdata = {28'b0, i_regs_addr};
    let _unused: logic = i_clk | i_rst;
}
"#;
    let dir = fixture_with_dut(
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n\
         [heartbeat]\npin = \"uart_tx\"\n\n\
         [bundle.regs]\nbacking = \"slave\"\nlatency = 1\n\
         ports = { addr = \"i_regs_addr\", rdata = \"o_regs_rdata\" }\n",
        Some(DUT),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value:#}");

    let slave = &value["bundles"][0]["slave"];
    assert_eq!(slave["addr_width"], 4, "{value:#}");
    assert_eq!(slave["width"], 32, "{value:#}");
    assert_eq!(slave["writable"], false, "{value:#}");
    assert_eq!(slave["latency"], 1, "{value:#}");

    let heartbeat = &value["heartbeat"];
    assert_eq!(heartbeat["resource"], "uart_tx", "{value:#}");
    assert_eq!(heartbeat["pin"], "D10", "{value:#}");
    assert_eq!(heartbeat["baud"], 115200, "{value:#}");
    assert_eq!(heartbeat["clocks_per_bit"], 868, "{value:#}");

    assert_eq!(value["registers"]["target_source"], "name", "{value:#}");
    // The JTAG host uses these to choose the TCK rate between scans.
    assert_eq!(value["registers"]["window_clock_mhz"], 100.0, "{value:#}");
    assert_eq!(value["registers"]["window_cycles"], 6, "{value:#}");
}

/// Transfer-level and slave ports are checked against the DUT declaration for
/// direction and width. Otherwise a mistake shows up only in `veryl build` or
/// on the board. A direction error names the port's own shape, not
/// `host_poll_fifo`.
#[test]
fn ports_in_the_wrong_direction_or_width_are_refused() {
    let head = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n";
    // Each case changes one DUT port.
    let dut = |ports: &str| {
        format!(
            "module dut_top (\n    i_clk: input clock,\n    i_rst: input reset,\n{ports}) {{\n    let _unused: logic = i_clk | i_rst;\n}}\n"
        )
    };
    let read_port = |cmd_valid: &str| {
        format!(
            "    {cmd_valid},\n    i_cmd_ready: input logic,\n    o_cmd_addr: output logic<32>,\n    o_cmd_size: output logic<16>,\n    i_rd_valid: input logic,\n    o_rd_ready: output logic,\n    i_rd_data: input logic<32>,\n    i_rd_last: input logic,\n"
        )
    };
    let read_roles = |cmd_valid: &str| {
        format!(
            "[bundle.hmem]\nbacking = \"bram\"\ndepth = 16\nports = {{ rd_cmd_valid = \"{cmd_valid}\", rd_cmd_ready = \"i_cmd_ready\", rd_cmd_addr = \"o_cmd_addr\", rd_cmd_size = \"o_cmd_size\", rd_valid = \"i_rd_valid\", rd_ready = \"o_rd_ready\", rd_data = \"i_rd_data\", rd_last = \"i_rd_last\" }}\n"
        )
    };
    let slave = |rdata: &str, extra: &str, roles: &str| {
        (
            dut(&format!(
                "    i_s_addr: input logic<4>,\n    {rdata},\n{extra}"
            )),
            format!(
                "[bundle.s]\nbacking = \"slave\"\nlatency = 1\nports = {{ addr = \"i_s_addr\", {roles} }}\n"
            ),
        )
    };
    let cases: Vec<(String, String, &str, &str)> = vec![
        // Transfer level: `rd_cmd_valid` is a DUT output.
        (
            dut(&read_port("i_cmd_valid: input logic")),
            read_roles("i_cmd_valid"),
            "harness::terminator::wrong_direction",
            "transfer-level",
        ),
        // Transfer level: the strobe has one bit per byte.
        (
            dut("    o_wc_valid: output logic,\n    i_wc_ready: input logic,\n    o_wc_addr: output logic<32>,\n    o_wc_size: output logic<16>,\n    o_w_valid: output logic,\n    i_w_ready: input logic,\n    o_w_data: output logic<32>,\n    o_w_strb: output logic<2>,\n    o_w_last: output logic,\n"),
            "[bundle.hmem]\nbacking = \"bram\"\ndepth = 16\nports = { wr_cmd_valid = \"o_wc_valid\", wr_cmd_ready = \"i_wc_ready\", wr_cmd_addr = \"o_wc_addr\", wr_cmd_size = \"o_wc_size\", wr_valid = \"o_w_valid\", wr_ready = \"i_w_ready\", wr_data = \"o_w_data\", wr_strb = \"o_w_strb\", wr_last = \"o_w_last\" }\n".to_string(),
            "harness::terminator::strobe_width",
            "4 bits wide",
        ),
        // Slave: the DUT answers on `rdata`.
        {
            let (d, m) = slave("i_s_rdata: input logic<32>", "", "rdata = \"i_s_rdata\"");
            (d, m, "harness::terminator::wrong_direction", "a `slave`")
        },
        // Slave: write data takes the 32-bit window word as is.
        {
            let (d, m) = slave(
                "o_s_rdata: output logic<32>",
                "    i_s_wdata: input logic<16>,\n    i_s_we: input logic,\n",
                "rdata = \"o_s_rdata\", wdata = \"i_s_wdata\", we = \"i_s_we\"",
            );
            (d, m, "harness::terminator::slave_width_not_a_word", "32 bits")
        },
        // Slave: too large for the window.
        (
            dut("    i_s_addr: input logic<40>,\n    o_s_rdata: output logic<32>,\n"),
            "[bundle.s]\nbacking = \"slave\"\nlatency = 1\nports = { addr = \"i_s_addr\", rdata = \"o_s_rdata\" }\n".to_string(),
            "harness::terminator::slave_too_large",
            "29 bits",
        ),
        // Fixed-latency memory: the memory answers on `rdata`. The error must
        // not talk about `host_poll_fifo`.
        (
            dut("    o_m_addr: output logic<4>,\n    o_m_rdata: output logic<32>,\n"),
            "[bundle.m]\nbacking = \"bram\"\nlatency = 1\naddressing = \"word\"\nports = { addr = \"o_m_addr\", rdata = \"o_m_rdata\" }\n".to_string(),
            "harness::terminator::wrong_direction",
            "memory answers on `rdata`",
        ),
    ];
    for (source, bundle, code, says) in cases {
        let dir = fixture_with_dut(&format!("{head}{bundle}"), Some(&source));
        let (value, status) = check_json(&dir);
        assert_eq!(status, Some(1), "{bundle}\n{value:#}");
        assert_eq!(value["error"]["code"], code, "{bundle}\n{value:#}");
        let text = format!(
            "{} {}",
            value["error"]["message"].as_str().unwrap_or_default(),
            value["error"]["help"].as_str().unwrap_or_default()
        );
        assert!(text.contains(says), "{bundle}\n{text}");
        assert!(!text.contains("host_poll_fifo"), "{bundle}\n{text}");
    }
}

/// A `$sv::` blackbox is named in `not_checked`: the generator does not read
/// Verilog, so a missing `include(inline, ...)` shows up only in synthesis.
/// With no blackbox, the item is absent.
#[test]
fn a_verilog_blackbox_is_named_as_not_checked() {
    let with_sv = r#"
module dut_top (
    i_clk      : input  clock   ,
    i_rst      : input  reset   ,
    i_csr_wdata: input  logic<8>,
    o_csr_rdata: output logic<8>,
) {
    inst u_core: $sv::core (
        i_clk: i_clk          ,
        i_d  : i_csr_wdata    ,
        o_q  : o_csr_rdata    ,
    );
    let _unused: logic = i_rst;
}
"#;
    let item = |dir: &tempfile::TempDir| {
        let (value, code) = check_json(dir);
        assert_eq!(code, Some(0), "{value:#}");
        value["not_checked"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "sv_blackbox_sources")
            .map(|item| item["what"].as_str().unwrap().to_string())
    };

    let what = item(&fixture_with_dut(CSR_MANIFEST, Some(with_sv)))
        .expect("a DUT with $sv:: has the item");
    assert!(what.contains("$sv::core"), "{what}");
    assert!(what.contains("include(inline"), "{what}");

    assert_eq!(item(&fixture(CSR_MANIFEST)), None);
}

/// `bram_preload` on an AXI4 port is not supported yet. `hns::axi_mem` accepts
/// DUT writes, so it cannot keep the memory read-only for the DUT.
#[test]
fn a_preloaded_memory_on_an_axi4_port_is_refused() {
    let dut = r#"
module dut_top (
    i_clk: input clock,
    i_rst: input reset,
    mem  : modport $std::axi4_if::<$std::axi4_pkg::<16, 4, 4, 1, 1, 1, 1, 1>>::master,
) {
    always_comb {
        mem.awvalid = 0;
    }
    let _unused: logic = i_clk | i_rst;
}
"#;
    let manifest = |backing: &str| {
        format!(
            "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n[bundle.mem]\nbacking = \"{backing}\"\ndepth = 256\nports = [\"mem\"]\n"
        )
    };
    let (value, code) = check_json(&fixture_with_dut(&manifest("bram_preload"), Some(dut)));
    assert_eq!(code, Some(1), "{value:#}");
    assert_eq!(
        value["error"]["code"], "harness::terminator::axi_preload_unsupported",
        "{value:#}"
    );
    assert!(
        value["error"]["help"]
            .as_str()
            .unwrap()
            .contains("backing = \"bram\""),
        "{value:#}"
    );

    let (value, code) = check_json(&fixture_with_dut(&manifest("bram"), Some(dut)));
    assert_eq!(code, Some(0), "{value:#}");
}

/// If a `$std::axi4_if` argument is not a number, `check` stops and says so,
/// instead of a misleading error about a missing `addr`.
#[test]
fn an_axi4_port_whose_widths_are_constants_is_named() {
    // The package must be in its own file. In the same file, Veryl (0.21.0, 0.22.0)
    // stops with `cyclic_file_dependency` against std's `axi_if.veryl`
    // (in `veryl check` too).
    let dut = r#"
module dut_top (
    i_clk: input clock,
    i_rst: input reset,
    mem  : modport $std::axi4_if::<$std::axi4_pkg::<widths::AW, 4, 4, 1, 1, 1, 1, 1>>::master,
) {
    always_comb {
        mem.awvalid = 0;
    }
    let _unused: logic = i_clk | i_rst;
}
"#;
    let dir = fixture_with_dut(
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n[bundle.mem]\nbacking = \"bram\"\ndepth = 256\nports = [\"mem\"]\n",
        Some(dut),
    );
    fs::write(
        dir.path().join("src").join("widths.veryl"),
        "package widths {\n    const AW: u32 = 16;\n}\n",
    )
    .unwrap();
    let (value, code) = check_json(&dir);
    assert_eq!(code, Some(1), "{value:#}");
    assert_eq!(
        value["error"]["code"], "harness::dut::axi4_args_unreadable",
        "{value:#}"
    );
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("`mem`") && message.contains("not a number"),
        "{message}"
    );
}

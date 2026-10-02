//! End-to-end tests for `gen`.
//!
//! They pin the safety promises more than the content of the output:
//!
//! - nothing is generated that `check` rejects (both share `plan`)
//! - a hand-written file is never overwritten
//! - no generated file is left out of the build
//! - gen can rewrite its own output any number of times

use std::fs;
use std::process::Command;

mod common;

fn fixture(veryl_toml: &str, harness_toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Veryl.toml"), veryl_toml).unwrap();
    fs::write(dir.path().join("Harness.toml"), harness_toml).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
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
"#,
    )
    .unwrap();
    dir
}

/// The fixture's Veryl.toml. It needs the `hns` package, because the output
/// refers to `hns::axil`, `hns::fifo` and `hns::mem`.
fn veryl_toml() -> String {
    format!(
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[build]\nreset_type = \"async_low\"\n\n[dependencies]\nhns = {{ path = \"{}/rtl/hns\" }}\n",
        env!("CARGO_MANIFEST_DIR")
    )
}

const HARNESS_TOML: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n";

/// Whether `command` is written as a command. It matches only at the start of a
/// line (after spaces), so a comment that mentions the name does not count.
fn has_command(text: &str, command: &str) -> bool {
    text.lines()
        .any(|line| line.trim_start().starts_with(command))
}

/// Collapses whitespace. The formatter aligns the output, so tests must not
/// depend on the exact spacing.
fn tight(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_gen(dir: &tempfile::TempDir, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["gen", "--target", "digilent/arty-a7-35"];
    args.extend_from_slice(extra);
    Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(args)
        .current_dir(dir.path())
        .output()
        .unwrap()
}

#[test]
fn gen_writes_the_register_map_and_can_rewrite_its_own_output() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);

    let first = run_gen(&dir, &[]);
    assert_eq!(first.status.code(), Some(0));
    let regs = dir.path().join("hns").join("regs.json");
    assert!(regs.is_file(), "hns/regs.json should exist");

    // gen can rewrite its own output: the marker lets it pass the overwrite guard.
    let second = run_gen(&dir, &[]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&regs).unwrap()).unwrap();
    assert_eq!(value["marker"], "veryl-harness:generated");
}

/// Without a target nothing can be decided. No default board is used.
#[test]
fn gen_without_a_target_is_rejected() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["gen", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::gen::target_required");
}

/// gen never overwrites a hand-written file.
#[test]
fn gen_refuses_to_overwrite_a_file_it_did_not_write() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    fs::create_dir_all(dir.path().join("hns")).unwrap();
    fs::write(
        dir.path().join("hns").join("regs.json"),
        "{ \"mine\": true }\n",
    )
    .unwrap();

    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::gen::would_overwrite");
    // The hand-written content is still there.
    let kept = fs::read_to_string(dir.path().join("hns").join("regs.json")).unwrap();
    assert!(kept.contains("mine"));
}

/// If gen refuses, it writes nothing. It prepares every file before writing
/// any, so a refusal on a late file cannot leave new files next to old ones.
#[test]
fn gen_writes_nothing_when_a_late_file_is_refused() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    // Make the Makefile, which is written late, a hand-written file.
    fs::create_dir_all(dir.path().join("hns/syn")).unwrap();
    fs::write(dir.path().join("hns/syn/Makefile"), "all:\n").unwrap();

    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::gen::would_overwrite");
    for early in ["harness.json", "Veryl.toml", "regs.json", "src/top.veryl"] {
        assert!(
            !dir.path().join("hns").join(early).exists(),
            "{early} was written before the refusal"
        );
    }
}

/// The output is a Veryl project of its own.
///
/// It works even when `sources = ["src"]` does not include the output
/// directory, because the output is not part of the DUT project. Veryl skips
/// a subdirectory that has its own `Veryl.toml`, so output under `src/` does
/// not mix into the DUT build either.
#[test]
fn the_output_is_a_project_of_its_own() {
    let veryl_toml = "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[build]\nreset_type = \"async_low\"\nsources = [\"src\"]\n\n[dependencies]\nhns = { path = \"HNS\" }\n"
        .replace("HNS", &format!("{}/rtl/hns", env!("CARGO_MANIFEST_DIR")));
    let dir = fixture(&veryl_toml, HARNESS_TOML);

    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let toml = fs::read_to_string(dir.path().join("hns/Veryl.toml")).unwrap();
    // The name is <DUT>_<base name of out-dir>, so it cannot clash with the
    // parts package `hns`.
    assert!(toml.contains("name    = \"fixture_hns\""), "{toml}");
    // `[build]` is copied from the DUT. If it differs, the DUT's SV is emitted
    // with a different reset polarity.
    assert!(toml.contains("reset_type = \"async_low\""), "{toml}");
    // The DUT is a path dependency.
    assert!(toml.contains("fixture = { path = \"..\" }"), "{toml}");
    assert!(toml.contains("hns = {"), "{toml}");
}

/// A different out-dir gives a different module name. This lets one DUT keep
/// harnesses for several boards side by side.
#[test]
fn a_different_out_dir_gives_a_different_module_name() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let output = run_gen(&dir, &["--out-dir", "harness_arty"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let synth = fs::read_to_string(dir.path().join("harness_arty/syn/synth.tcl")).unwrap();
    assert!(synth.contains("fixture_harness_arty_top"), "{synth}");
}

/// An out-dir name that is not a Veryl identifier is refused. Otherwise Veryl
/// fails to read the generated `Veryl.toml`, which is hard to understand.
#[test]
fn an_out_dir_that_cannot_be_a_project_name_is_refused() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let output = run_gen(&dir, &["--out-dir", "1-bad", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::plan::bad_out_dir");
}

/// `gen` rejects what `check` rejects (they share the resolution).
#[test]
fn gen_does_not_generate_what_check_would_reject() {
    let dir = fixture(
        &veryl_toml(),
        // No clock frequency.
        "[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n",
    );

    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::clock::missing_frequency");
    assert!(
        !dir.path().join("hns").exists(),
        "nothing should be written"
    );
}

/// The generated Veryl passes the analyzer.
///
/// It goes through the same path as `veryl build` (`dut::analyze`), so a CDC
/// that is not marked `unsafe (cdc)` fails here too.
#[test]
fn the_generated_veryl_passes_the_analyzer() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("hns/src/clk.veryl").is_file());
    assert!(dir.path().join("hns/syn/mmcm.tcl").is_file());

    let mut metadata = veryl_metadata::Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    harness::dut::analyze(&mut metadata).expect("the generated harness must compile");
}

/// The clock module declares only the crossings it intends.
#[test]
fn the_generated_clock_module_declares_its_crossings() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    run_gen(&dir, &[]);

    let text = fs::read_to_string(dir.path().join("hns/src/clk.veryl")).unwrap();
    // Three places: the MMCM instance and the harness and DUT reset synchronizers.
    assert_eq!(text.matches("unsafe (cdc)").count(), 3, "{text}");

    let tcl = fs::read_to_string(dir.path().join("hns/syn/mmcm.tcl")).unwrap();
    assert!(tcl.contains("CONFIG.CLKOUT1_REQUESTED_OUT_FREQ {200.000}"));
    // No M/D/O values: Vivado solves them.
    assert!(!tcl.contains("CLKFBOUT_MULT"));
}

/// The RTL and the host map come from the same IR.
///
/// The offsets in `regs.json` must match the word numbers the generated CSR
/// decodes. If they differ, the host accesses a different register than the
/// RTL has there.
#[test]
fn the_csr_decodes_the_offsets_the_register_map_publishes() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let output = run_gen(&dir, &[]);
    assert_eq!(output.status.code(), Some(0));

    let regs: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
            .unwrap();
    let csr = fs::read_to_string(dir.path().join("hns/src/csr.veryl")).unwrap();

    for register in regs["registers"].as_array().unwrap() {
        let word = register["offset"].as_u64().unwrap() / 4;
        let name = register["name"].as_str().unwrap();
        // The read case has that word number.
        assert!(
            tight(&csr)
                .lines()
                .any(|line| line.starts_with(&format!("{word} :"))),
            "word {word} ({name}) is not decoded:\n{csr}"
        );
    }

    // Constant registers have the same values in the RTL.
    let magic = regs["magic"].as_u64().unwrap();
    assert!(csr.contains(&format!("{magic:08x}")), "{csr}");
    let hash = regs["map_hash"].as_u64().unwrap();
    assert!(csr.contains(&format!("{hash:08x}")), "{csr}");
}

/// A backing with no terminator is an error, not silently dropped.
#[test]
fn gen_refuses_a_backing_it_cannot_terminate_yet() {
    let dir = fixture(
        &veryl_toml(),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"observe\"\n",
    );

    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::plan::no_terminator");
    // The help lists what can be built.
    let help = value["error"]["help"].as_str().unwrap();
    assert!(help.contains("reg"), "{help}");

    // `check --target` refuses it for the same reason, so `check` does not
    // pass what `gen` then refuses.
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::plan::no_terminator");

    // Without a target this is not about generation, so it passes.
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// A DUT with an output stream (for host_poll_fifo tests).
fn fifo_fixture(harness_toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Veryl.toml"), veryl_toml()).unwrap();
    fs::write(dir.path().join("Harness.toml"), harness_toml).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk  : input  clock   ,
    i_rst  : input  reset   ,
    i_run  : input  logic   ,
    o_valid: output logic   ,
    o_data : output logic<8>,
    i_ready: input  logic   ,
) {
    var count: logic<8>;
    always_ff {
        if_reset {
            count = 0;
        } else if i_run && i_ready {
            count = count + 1;
        }
    }
    assign o_valid = i_run;
    assign o_data  = count;
}
"#,
    )
    .unwrap();
    dir
}

const FIFO_HARNESS_TOML: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.tx]\ncontract = \"valid_ready\"\nbacking = \"host_poll_fifo\"\ndepth = 16\nports = { valid = \"o_valid\", ready = \"i_ready\", data = \"o_data\" }\n";

/// `host_poll_fifo` becomes a FIFO and its registers.
///
/// The contract can return `ready`, so nothing is lost and there is no drop
/// counter.
#[test]
fn a_host_poll_fifo_becomes_a_fifo_and_its_registers() {
    let dir = fifo_fixture(FIFO_HARNESS_TOML);
    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let map: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
            .unwrap();
    let registers = map["registers"].as_array().unwrap();
    let named = |name: &str| {
        registers
            .iter()
            .find(|register| register["name"] == name)
            .unwrap_or_else(|| panic!("{name} is missing from {registers:#?}"))
    };

    // Reading the data does not pop it. Pop is a separate register, so a
    // dump does not change the queue.
    assert_eq!(named("tx_data")["access"], "ro");
    assert_eq!(named("tx_data")["width"], 8);
    assert_eq!(named("tx_data")["role"], "data");
    assert_eq!(named("tx_level")["width"], 5); // $clog2(16) + 1
    // The depth can be read on the board, which also shows whether the
    // default was used.
    assert_eq!(named("tx_depth")["value"], 16);
    assert_eq!(named("tx_pop")["access"], "rw");
    assert_eq!(named("tx_pop")["self_clearing"]["from"], "terminator");
    assert_eq!(named("tx_pop")["self_clearing"]["on"], "tx_level");
    // Nothing can be lost, so there is no counter that is always 0.
    assert!(
        !registers
            .iter()
            .any(|register| register["name"] == "tx_drops"),
        "{registers:#?}"
    );

    let top = tight(&fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap());
    assert!(top.contains("inst u_fifo_tx: hns::fifo #("), "{top}");
    assert!(top.contains("WIDTH: 8"), "{top}");
    assert!(top.contains("DEPTH: 16"), "{top}");
    // The FIFO drives ready, and it reaches the DUT input.
    assert!(top.contains("assign w_i_ready = t_tx_ready;"), "{top}");

    let csr = tight(&fs::read_to_string(dir.path().join("hns/src/csr.veryl")).unwrap());
    // Pop clears when the FIFO is not empty (one write pops exactly one entry).
    assert!(
        csr.contains("if reg_tx_pop != 0 && (i_tx_level) != 0 {"),
        "{csr}"
    );

    // The generated Veryl compiles.
    let mut metadata = veryl_metadata::Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    harness::dut::analyze(&mut metadata).expect("the generated harness must compile");
}

/// A contract that cannot stall (`valid_only`) always gets a drop counter.
#[test]
fn a_valid_only_stream_gets_a_drop_counter() {
    let dir = fifo_fixture(
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[tie_off]\ni_ready = 1\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.tx]\ncontract = \"valid_only\"\nbacking = \"host_poll_fifo\"\nports = { valid = \"o_valid\", data = \"o_data\" }\n",
    );
    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let map: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
            .unwrap();
    let drops = map["registers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|register| register["name"] == "tx_drops")
        .expect("a lossy termination has to publish what it lost");
    assert_eq!(drops["role"], "drops");
    assert_eq!(drops["width"], 32);

    // With no depth given, the default is used, and it can be read on the board.
    let depth = map["registers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|register| register["name"] == "tx_depth")
        .unwrap();
    assert_eq!(depth["value"], harness::terminator::DEFAULT_DEPTH);

    let top = tight(&fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap());
    // Nobody reads ready, so it is left open. Losses are seen in the counter.
    assert!(top.contains("o_ready: _"), "{top}");
    assert!(top.contains("o_drops: t_tx_drops"), "{top}");

    let mut metadata = veryl_metadata::Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    harness::dut::analyze(&mut metadata).expect("the generated harness must compile");
}

/// The other direction (host to DUT) is not built. gen must not silently
/// generate something else.
#[test]
fn a_host_poll_fifo_that_feeds_the_dut_is_rejected() {
    let dir = fifo_fixture(
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[leave_open]\nports = [\"o_valid\", \"o_data\"]\n\n[bundle.rx]\ncontract = \"valid_only\"\nbacking = \"host_poll_fifo\"\nports = { valid = \"i_run\", data = \"i_ready\" }\n",
    );
    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        value["error"]["code"], "harness::terminator::wrong_direction",
        "{value}"
    );
    // The help says what to use instead.
    assert!(
        value["error"]["help"].as_str().unwrap().contains("reg"),
        "{value}"
    );
}

/// A depth that is not a power of two is an error, not rounded. Otherwise the
/// pointers would wrap early.
#[test]
fn a_depth_that_is_not_a_power_of_two_is_rejected() {
    let dir = fifo_fixture(&FIFO_HARNESS_TOML.replace("depth = 16", "depth = 100"));
    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        value["error"]["code"], "harness::terminator::depth_not_power_of_two",
        "{value}"
    );
    // The help gives the fix (the next power of two).
    assert!(
        value["error"]["help"].as_str().unwrap().contains("128"),
        "{value}"
    );
}

/// The map for humans comes from the same IR as the map for machines.
///
/// It must agree with `regs.json`, and it must state in words the rules that
/// follow from each role (the first thing a driver writer reads).
#[test]
fn the_markdown_map_says_the_same_thing_as_the_json() {
    let dir = mem_fixture(MEM_HARNESS_TOML);
    assert_eq!(run_gen(&dir, &[]).status.code(), Some(0));

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
            .unwrap();
    let md = fs::read_to_string(dir.path().join("hns/regs.md")).unwrap();

    // The marker, so the next gen can rewrite the file.
    assert!(md.contains("veryl-harness:generated"), "{md}");

    // The hash and the window size match the JSON.
    let hash = json["map_hash"].as_u64().unwrap();
    assert!(md.contains(&format!("0x{hash:08x}")), "{md}");
    assert!(
        md.contains(&format!("{} bytes", json["size_bytes"].as_u64().unwrap())),
        "{md}"
    );

    // The identity header offsets match the JSON (the end of the window).
    for (label, name) in [("magic", "harness_magic"), ("map hash", "harness_map_hash")] {
        let offset = json["registers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .and_then(|r| r["offset"].as_u64())
            .unwrap();
        assert!(offset > 4, "{name} at {offset}");
        let line = md
            .lines()
            .find(|line| line.contains(&format!("| {label} ")))
            .unwrap_or_else(|| panic!("no {label} line in:\n{md}"));
        assert!(
            line.contains(&format!("read at offset 0x{offset:x}")),
            "{line}"
        );
    }

    // Every register is listed at the same offset as in the JSON.
    for register in json["registers"].as_array().unwrap() {
        let name = register["name"].as_str().unwrap();
        let offset = register["offset"].as_u64().unwrap();
        assert!(
            md.contains(&format!("`0x{offset:04x}` | `{name}`")),
            "{name} at 0x{offset:04x} is missing from:\n{md}"
        );
    }

    // Tables are aligned for reading in a terminal with `cat`. Check that all
    // rows of one table have the same length.
    let mut block: Vec<usize> = Vec::new();
    for line in md.lines() {
        if line.starts_with('|') {
            block.push(line.chars().count());
        } else if !block.is_empty() {
            assert!(
                block.iter().all(|width| *width == block[0]),
                "a table is not aligned ({block:?}):\n{md}"
            );
            block.clear();
        }
    }

    // Rules from the roles, which the map alone does not show.
    assert!(
        md.contains("advances by one when a `ram_mdata` entry is committed"),
        "{md}"
    );
    assert!(md.contains("never on a read"), "{md}");
    assert!(md.contains("latency of 1 cycle"), "{md}");
}

/// A DUT with a memory port (for bram tests).
fn mem_fixture(harness_toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Veryl.toml"), veryl_toml()).unwrap();
    fs::write(dir.path().join("Harness.toml"), harness_toml).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk      : input  clock    ,
    i_rst      : input  reset    ,
    o_ram_addr : output logic<10>,
    i_ram_rdata: input  logic<32>,
    o_ram_wdata: output logic<32>,
    o_ram_we   : output logic    ,
) {
    var addr: logic<10>;
    always_ff {
        if_reset {
            addr = 0;
        } else {
            addr = addr + 1;
        }
    }
    assign o_ram_addr  = addr;
    assign o_ram_wdata = 0;
    assign o_ram_we    = 0;
}
"#,
    )
    .unwrap();
    dir
}

/// States the indirect access explicitly. The default is `region`, but the
/// indirect ports are still supported, and this fixture tests them.
const MEM_HARNESS_TOML: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.ram]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram\"\naddressing = \"word\"\naccess = \"indirect\"\n";

/// `access = "region"` shows up in the window layout.
///
/// User offsets start at 0. The region takes the low end of the window, and
/// the registers and the identity header sit above it. A region has no
/// `maddr` / `mdata`: each access carries its address, and those registers
/// would add shared state.
#[test]
fn a_region_takes_the_low_end_of_the_window_and_drops_the_indirect_ports() {
    let dir = mem_fixture(&format!(
        "{}access = \"region\"\ndepth = 256\n",
        MEM_HARNESS_TOML.replace("access = \"indirect\"\n", "")
    ));

    // `check` already shows the layout.
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--target", "digilent/arty-a7-35", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let map = &report["registers"];

    let regions = map["regions"].as_array().unwrap();
    assert_eq!(regions.len(), 1, "{regions:#?}");
    assert_eq!(regions[0]["name"], "ram");
    assert_eq!(regions[0]["base"], 0);
    assert_eq!(regions[0]["depth"], 256);
    // 256 entries x 4 bytes. Already a power of two.
    assert_eq!(regions[0]["size_bytes"], 1024);

    let registers = map["registers"].as_array().unwrap();
    let named = |name: &str| registers.iter().find(|r| r["name"] == name);
    // No indirect ports. The `depth` register stays.
    assert!(named("ram_maddr").is_none(), "{registers:#?}");
    assert!(named("ram_mdata").is_none(), "{registers:#?}");
    assert!(named("ram_depth").is_some(), "{registers:#?}");
    // Registers are above the region.
    assert!(
        named("ram_depth").unwrap()["offset"].as_u64().unwrap() >= 1024,
        "{registers:#?}"
    );
    // The identity header is at the end of the window.
    let size = map["size_bytes"].as_u64().unwrap();
    assert_eq!(named("harness_magic").unwrap()["offset"], size - 8);

    // Generation passes and emits the decode.
    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let top = tight(&fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap());
    // The region is selected by the upper bits (it is aligned, so a mask is enough).
    assert!(top.contains("assign sel_ram = bus_addr["), "{top}");
    // The address goes to the memory as is. The CSR is selected only outside the region.
    assert!(top.contains("assign t_ram_maddr = bus_addr["), "{top}");
    assert!(top.contains("assign csr_re = bus_re & ~(sel_ram)"), "{top}");
    // `rvalid` is a pulse. If it were held, it would never go low.
    assert!(top.contains("ans_ram = bus_re & sel_ram"), "{top}");
    assert!(
        top.contains("assign bus_rvalid = csr_rvalid | ans_ram"),
        "{top}"
    );
}

/// `bram` becomes a memory and its registers. The depth comes from the address width.
#[test]
fn a_bram_bundle_becomes_a_memory_and_its_window() {
    let dir = mem_fixture(MEM_HARNESS_TOML);
    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let map: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
            .unwrap();
    let registers = map["registers"].as_array().unwrap();
    let named = |name: &str| {
        registers
            .iter()
            .find(|register| register["name"] == name)
            .unwrap_or_else(|| panic!("{name} is missing from {registers:#?}"))
    };

    // Only two registers. The CSR space does not grow with the memory.
    assert_eq!(named("ram_maddr")["access"], "rw");
    assert_eq!(named("ram_maddr")["width"], 10);
    assert_eq!(named("ram_mdata")["access"], "rw");
    assert_eq!(named("ram_mdata")["width"], 32);
    // The depth covers the whole address width (2^10), so no access is out of range.
    assert_eq!(named("ram_depth")["value"], 1024);
    assert!(
        !registers
            .iter()
            .any(|register| register["name"] == "ram_oor"),
        "{registers:#?}"
    );

    let top = fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap();
    // Remove whitespace so the check does not depend on alignment.
    let squashed: String = top.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(squashed.contains("instu_mem_ram:hns::mem#("), "{top}");
    assert!(squashed.contains("DEPTH:1024"), "{top}");
    assert!(squashed.contains("LATENCY:1"), "{top}");

    let csr = tight(&fs::read_to_string(dir.path().join("hns/src/csr.veryl")).unwrap());
    // A data write advances the address. A read does not.
    assert!(csr.contains("reg_ram_maddr = reg_ram_maddr + 1;"), "{csr}");

    let mut metadata = veryl_metadata::Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    harness::dut::analyze(&mut metadata).expect("the generated harness must compile");
}

/// The DUT cannot write a `bram_preload`. A write port is an error.
#[test]
fn a_preload_memory_the_dut_writes_is_rejected() {
    let dir = mem_fixture(&MEM_HARNESS_TOML.replace("\"bram\"", "\"bram_preload\""));
    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        value["error"]["code"], "harness::terminator::preload_is_read_only",
        "{value}"
    );
    assert!(
        value["error"]["help"].as_str().unwrap().contains("bram"),
        "{value}"
    );
}

/// When the address space is too large, gen asks for a depth instead of using
/// a default.
#[test]
fn a_memory_with_a_huge_address_space_asks_for_a_depth() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Veryl.toml"), veryl_toml()).unwrap();
    fs::write(dir.path().join("Harness.toml"), MEM_HARNESS_TOML).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk      : input  clock    ,
    i_rst      : input  reset    ,
    o_ram_addr : output logic<32>,
    i_ram_rdata: input  logic<32>,
    o_ram_wdata: output logic<32>,
    o_ram_we   : output logic    ,
) {
    assign o_ram_addr  = 0;
    assign o_ram_wdata = 0;
    assign o_ram_we    = 0;
}
"#,
    )
    .unwrap();

    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        value["error"]["code"], "harness::terminator::address_space_too_large",
        "{value}"
    );
    // The help gives the fix (write `depth`) and says what happens out of range.
    let help = value["error"]["help"].as_str().unwrap();
    assert!(help.contains("depth ="), "{help}");
    assert!(help.contains("ram_oor"), "{help}");
}

/// A 64-bit address does not overflow. Real CPUs have this width.
///
/// Computing `1 << 64` would panic. With a depth given, gen passes, and since
/// some addresses cannot be reached, an out-of-range counter is added.
#[test]
fn a_memory_with_a_64_bit_address_does_not_overflow() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Veryl.toml"), veryl_toml()).unwrap();
    fs::write(
        dir.path().join("Harness.toml"),
        format!("{MEM_HARNESS_TOML}depth = 4096\n"),
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk      : input  clock    ,
    i_rst      : input  reset    ,
    o_ram_addr : output logic<64>,
    i_ram_rdata: input  logic<32>,
    o_ram_wdata: output logic<32>,
    o_ram_we   : output logic    ,
) {
    assign o_ram_addr  = 0;
    assign o_ram_wdata = 0;
    assign o_ram_we    = 0;
}
"#,
    )
    .unwrap();

    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let map: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
            .unwrap();
    // 4096 words do not cover a 64-bit address space, so out-of-range accesses are counted.
    assert!(
        map["registers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|register| register["name"] == "ram_oor"),
        "{map}"
    );
}

/// A memory that does not fit the board is rejected before synthesis.
#[test]
fn a_memory_that_does_not_fit_the_board_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Veryl.toml"), veryl_toml()).unwrap();
    fs::write(
        dir.path().join("Harness.toml"),
        format!("{MEM_HARNESS_TOML}depth = 1048576\n"),
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk      : input  clock    ,
    i_rst      : input  reset    ,
    o_ram_addr : output logic<24>,
    i_ram_rdata: input  logic<32>,
    o_ram_wdata: output logic<32>,
    o_ram_we   : output logic    ,
) {
    assign o_ram_addr  = 0;
    assign o_ram_wdata = 0;
    assign o_ram_we    = 0;
}
"#,
    )
    .unwrap();
    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        value["error"]["code"], "harness::feasibility::not_enough_bram",
        "{value}"
    );
}

/// The top connects the DUT in declaration order, and `[leave_open]` becomes `_`.
#[test]
fn the_top_connects_the_dut_and_leaves_open_ports_open() {
    let dir = fixture(
        &veryl_toml(),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\nports = [\"i_csr_wdata\"]\n\n[leave_open]\nports = [\"o_csr_rdata\"]\n",
    );
    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let top = tight(&fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap());
    // The DUT is in another project, so its name is qualified.
    assert!(top.contains("inst u_dut: fixture::dut_top"), "{top}");
    // A [leave_open] output connects to `_`.
    assert!(top.contains("o_csr_rdata: _"), "{top}");
    // The DUT clock and reset come from the generated clock module.
    assert!(top.contains("i_clk : clk_c0"), "{top}");
    assert!(top.contains("i_rst : rst_c0"), "{top}");

    // All of the output passes the analyzer.
    let mut metadata = veryl_metadata::Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    harness::dut::analyze(&mut metadata).expect("the generated harness must compile");
}

/// The transport closes the register window: a JTAG bridge and an AXI4-Lite slave.
#[test]
fn the_transport_closes_the_register_window() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let top = tight(&fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap());
    // The top exposes only the board clock and reset. The CSR bus stays inside.
    // The bridge is our own, not a Vivado IP.
    assert!(top.contains("inst u_jtag: hns::bscan"), "{top}");
    assert!(top.contains("inst u_axil: hns::axil"), "{top}");
    assert!(
        !top.contains("i_csr_addr"),
        "the CSR bus must not leak to the top:\n{top}"
    );

    // The JTAG CDC is inside `hns::dr`. The top has no crossing, so it needs
    // no `unsafe (cdc)` (the compiler does not warn about extra ones).
    assert_eq!(top.matches("unsafe (cdc)").count(), 0, "{top}");

    // No Vivado JTAG IP is generated.
    assert!(
        !dir.path().join("hns/syn/jtag_axi.tcl").exists(),
        "the Vivado JTAG IP is gone"
    );

    // TCK and the harness clock are asynchronous. This is stated with
    // `set_clock_groups -asynchronous`, not hidden with a false path.
    let xdc = fs::read_to_string(dir.path().join("hns/syn/harness.xdc")).unwrap();
    assert!(!has_command(&xdc, "set_false_path"), "{xdc}");

    // Vivado does not create the TCK clock by itself. Without it, the bridge
    // CDC is never analyzed.
    assert!(has_command(&xdc, "create_clock -name hns_tck"), "{xdc}");

    // Only one group is written. Listing the other clocks misses the MMCM
    // clocks, which do not exist yet when this XDC is read, and the paths to
    // them fail timing. A single group means "asynchronous to all others".
    assert!(
        has_command(&xdc, "set_clock_groups -asynchronous -group $hns_tck"),
        "{xdc}"
    );
    assert!(!has_command(&xdc, "set hns_others"), "{xdc}");

    // The AXI4-Lite slave is a part in the `hns` package, not a generated file.
    assert!(
        !dir.path().join("hns/src/hns_axil.veryl").exists(),
        "axil is a package part now, not a generated file"
    );

    let mut metadata = veryl_metadata::Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    harness::dut::analyze(&mut metadata).expect("the generated harness must compile");
}

/// The whole build flow is generated.
#[test]
fn the_build_flow_is_generated() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let output = run_gen(&dir, &[]);
    assert_eq!(output.status.code(), Some(0));

    for name in [
        "board.xdc",
        "board.tcl",
        "harness.xdc",
        "ip.tcl",
        "synth.tcl",
        "mmcm.tcl",
        "Makefile",
    ] {
        assert!(
            dir.path().join("hns/syn").join(name).is_file(),
            "hns/syn/{name} is missing"
        );
    }

    let board = fs::read_to_string(dir.path().join("hns/syn/board.xdc")).unwrap();
    // Pins come from the target description (Arty: clock on E3, reset on C2).
    assert!(board.contains("PACKAGE_PIN E3"), "{board}");
    assert!(board.contains("PACKAGE_PIN C2"), "{board}");
    // The MMCM IP defines the input clock period. Defining it here overrides
    // the IP (CRITICAL WARNING 18-1055).
    assert!(!board.contains("create_clock"), "{board}");
    let mmcm = fs::read_to_string(dir.path().join("hns/syn/mmcm.tcl")).unwrap();
    assert!(mmcm.contains("CONFIG.PRIM_IN_FREQ {100.000}"), "{mmcm}");

    // No false path. Synchronizers get only ASYNC_REG.
    let harness = fs::read_to_string(dir.path().join("hns/syn/harness.xdc")).unwrap();
    assert!(harness.contains("ASYNC_REG"), "{harness}");
    assert!(!has_command(&harness, "set_false_path"), "{harness}");

    let synth = fs::read_to_string(dir.path().join("hns/syn/synth.tcl")).unwrap();
    // The synthesis top is the emitted name (Veryl adds the project name as a prefix).
    assert!(synth.contains("set top      {fixture_hns_top}"), "{synth}");
    // RTL is read from the harness project's filelist, not listed by hand.
    assert!(synth.contains("fixture_hns.f"), "{synth}");

    let makefile = fs::read_to_string(dir.path().join("hns/syn/Makefile")).unwrap();
    // Some environments export `VIVADO` as the install directory.
    assert!(makefile.contains("VIVADO_BIN"), "{makefile}");
    assert!(!makefile.contains("VIVADO ?="), "{makefile}");

    let ip = fs::read_to_string(dir.path().join("hns/syn/ip.tcl")).unwrap();
    // No out-of-context synthesis, so synth_ip is not called. A comment in
    // the file mentions it, so check only the lines that run.
    assert!(
        !ip.lines()
            .any(|line| line.trim_start().starts_with("synth_ip")),
        "{ip}"
    );
    assert!(ip.contains("file mkdir $ip_dir"), "{ip}");
}

#[allow(dead_code)]
fn which_tclsh() -> Result<std::path::PathBuf, ()> {
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        for name in ["tclsh", "tclsh8.6"] {
            let path = std::path::Path::new(dir).join(name);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(())
}

/// One write is exactly one beat.
///
/// When the host drives `valid` of a `valid_ready` bundle, the CSR clears it
/// itself when it sees `ready`. JTAG works in ms and the DUT in ns, so without
/// this a single write of 1 sends hundreds of thousands of beats.
#[test]
fn a_host_driven_valid_clears_itself() {
    let dir = fixture(
        &veryl_toml(),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.csr]\ncontract = \"valid_ready\"\nbacking = \"reg\"\nports = { valid = \"i_csr_wdata\", ready = \"o_csr_rdata\" }\n",
    );
    let output = run_gen(&dir, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let csr = tight(&fs::read_to_string(dir.path().join("hns/src/csr.veryl")).unwrap());
    // The CSR clears valid itself once the transfer happens.
    assert!(csr.contains("reg_i_csr_wdata = 0;"), "{csr}");
    // The clear comes before the write. In the other order, a new beat from
    // the host would be lost.
    let clear_at = csr.find("reg_i_csr_wdata != 0").expect("no self-clear");
    let write_at = csr.find("if i_we").expect("no write decode");
    assert!(
        clear_at < write_at,
        "the clear must come before the write:\n{csr}"
    );

    // The map says so too, so the host can read back whether the beat was taken.
    let regs: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
            .unwrap();
    let valid = regs["registers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "i_csr_wdata")
        .unwrap();
    assert_eq!(valid["self_clearing"]["on"], "o_csr_rdata");
    assert_eq!(valid["self_clearing"]["role"], "ready");

    let mut metadata = veryl_metadata::Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    harness::dut::analyze(&mut metadata).expect("the generated harness must compile");
}

/// A differential clock and an active-high board reset (VCU118).
///
/// The board reset polarity and the DUT reset polarity are independent, and
/// the harness converts between them. On Arty both are active low, so Arty
/// alone cannot show this.
#[test]
fn a_differential_clock_and_an_active_high_board_reset() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        // VCU118 provides jtag and pcie, so one must be chosen. This test is
        // about the clock and reset, so jtag is enough.
        .args(["gen", "--target", "xilinx/vcu118", "--transport", "jtag"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let clk = tight(&fs::read_to_string(dir.path().join("hns/src/clk.veryl")).unwrap());
    // A differential clock comes in on two ports and goes to the IP's p/n.
    assert!(clk.contains("i_sys_clk_p: input 'sys clock"), "{clk}");
    assert!(clk.contains("clk_in1_p: i_sys_clk_p"), "{clk}");
    assert!(clk.contains("clk_in1_n: i_sys_clk_n"), "{clk}");
    // The board reset is active high. The DUT gets the project polarity (async_low).
    assert!(
        clk.contains("i_sys_rst : input 'sys reset_async_high"),
        "{clk}"
    );
    assert!(
        clk.contains("o_rst_c0 : output 'c0 reset_async_low"),
        "{clk}"
    );
    // `reset` is a Veryl keyword, so it is written as a raw identifier.
    assert!(clk.contains("r#reset : i_sys_rst"), "{clk}");

    let xdc = fs::read_to_string(dir.path().join("hns/syn/board.xdc")).unwrap();
    assert!(xdc.contains("PACKAGE_PIN AY24"), "{xdc}");
    assert!(xdc.contains("PACKAGE_PIN AY23"), "{xdc}");
    assert!(xdc.contains("PACKAGE_PIN L19"), "{xdc}");
    // The MMCM IP defines the input clock period, for a differential clock too.
    assert_eq!(xdc.matches("create_clock").count(), 0, "{xdc}");

    let mmcm = fs::read_to_string(dir.path().join("hns/syn/mmcm.tcl")).unwrap();
    assert!(mmcm.contains("CONFIG.PRIM_IN_FREQ {125.000}"), "{mmcm}");
    assert!(mmcm.contains("Differential_clock_capable_pin"), "{mmcm}");
    assert!(mmcm.contains("CONFIG.RESET_TYPE {ACTIVE_HIGH}"), "{mmcm}");

    let mut metadata = veryl_metadata::Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    harness::dut::analyze(&mut metadata).expect("the generated harness must compile");
}

/// A DUT port that is an unpacked array (`logic<8> [4]`) fails before anything
/// is written. Otherwise Vivado reports `type mismatch in port association`
/// only after it has elaborated the whole DUT.
#[test]
fn gen_rejects_an_unpacked_array_port_without_writing_anything() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk      : input  clock       ,
    i_rst      : input  reset       ,
    i_csr_wdata: input  logic<8>    ,
    o_csr_rdata: output logic<8> [4],
) {
    always_ff {
        if_reset {
            for i in 0..4 {
                o_csr_rdata[i] = 0;
            }
        } else {
            for i in 0..4 {
                o_csr_rdata[i] = i_csr_wdata;
            }
        }
    }
}
"#,
    )
    .unwrap();

    let out = run_gen(&dir, &[]);
    assert_ne!(
        out.status.code(),
        Some(0),
        "an array port must not generate"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("o_csr_rdata"), "{stderr}");
    assert!(stderr.contains("unpacked array"), "{stderr}");
    // The help gives the fix: the flattened width, or leaving the port open.
    assert!(stderr.contains("logic<32>"), "{stderr}");
    assert!(stderr.contains("leave_open"), "{stderr}");

    assert!(
        !dir.path().join("hns").exists(),
        "gen must not start writing what check rejects"
    );
}

/// `harness.xdc` is Tcl: it checks that the synchronizer cells exist before it
/// constrains them. A plain `read_xdc` does not accept control flow; it reports
/// `CRITICAL WARNING: [Designutils 20-1307]` and skips the file, so
/// `ASYNC_REG` is never applied. The content and the way it is read go
/// together, so they are tested together.
#[test]
fn a_tcl_harness_xdc_is_read_unmanaged() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    assert_eq!(run_gen(&dir, &[]).status.code(), Some(0));

    let syn = dir.path().join("hns").join("syn");
    let xdc = fs::read_to_string(syn.join("harness.xdc")).unwrap();
    let synth = fs::read_to_string(syn.join("synth.tcl")).unwrap();

    let is_tcl = xdc.contains("if {") || xdc.contains("foreach ");
    assert!(
        is_tcl,
        "harness.xdc is no longer Tcl; review this test:\n{xdc}"
    );
    assert!(
        synth.contains("read_xdc -unmanaged harness.xdc"),
        "an XDC with Tcl must be read with -unmanaged:\n{synth}"
    );
    assert!(
        !synth.contains("read_xdc harness.xdc"),
        "a plain read_xdc is still there:\n{synth}"
    );
    // board.xdc is plain XDC and does not need -unmanaged.
    assert!(synth.contains("read_xdc board.xdc"), "{synth}");
}

/// A port terminated with `[pin]` becomes a top port and gets an XDC
/// constraint. The pin number comes from the target description, so the
/// manifest holds only the resource name.
#[test]
fn a_pin_bound_port_reaches_the_top_and_the_xdc() {
    let dir = fixture(
        &veryl_toml(),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n\
         [pin]\no_uart_tx = \"uart_tx\"\n\n\
         [bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n",
    );
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk      : input  clock   ,
    i_rst      : input  reset   ,
    i_csr_wdata: input  logic<8>,
    o_csr_rdata: output logic<8>,
    o_uart_tx  : output logic   ,
) {
    always_ff {
        if_reset {
            o_csr_rdata = 0;
            o_uart_tx   = 0;
        } else {
            o_csr_rdata = i_csr_wdata;
            o_uart_tx   = i_csr_wdata[0];
        }
    }
}
"#,
    )
    .unwrap();

    let out = run_gen(&dir, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let top = fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap();
    assert!(
        tight(&top).contains("o_uart_tx: output 'c0 logic"),
        "the pin port should reach hns_top with its clock domain:\n{top}"
    );

    let xdc = fs::read_to_string(dir.path().join("hns/syn/board.xdc")).unwrap();
    // Arty's uart_tx is D10 / LVCMOS33 (from the board file).
    assert!(xdc.contains("PACKAGE_PIN D10"), "{xdc}");
    assert!(xdc.contains("IOSTANDARD LVCMOS33"), "{xdc}");
    assert!(xdc.contains("get_ports { o_uart_tx }"), "{xdc}");

    // The port is in no bundle but is terminated: a pin is a kind of terminator.
    let regs = fs::read_to_string(dir.path().join("hns/regs.json")).unwrap();
    assert!(
        !regs.contains("o_uart_tx"),
        "a pin port is not a register:\n{regs}"
    );
}

/// An unknown resource name fails and lists the resources the target has.
#[test]
fn gen_rejects_an_unknown_pin_resource() {
    let dir = fixture(
        &veryl_toml(),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n\
         [pin]\no_uart_tx = \"led0\"\n\n\
         [bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n",
    );
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk      : input  clock   ,
    i_rst      : input  reset   ,
    i_csr_wdata: input  logic<8>,
    o_csr_rdata: output logic<8>,
    o_uart_tx  : output logic   ,
) {
    assign o_csr_rdata = i_csr_wdata;
    assign o_uart_tx   = i_csr_wdata[0];
    let _unused: logic = i_clk | i_rst;
}
"#,
    )
    .unwrap();

    let out = run_gen(&dir, &[]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("led0"), "{stderr}");
    assert!(stderr.contains("uart_tx"), "{stderr}");
    assert!(
        !dir.path().join("hns").exists(),
        "nothing should be written"
    );
}

/// `[heartbeat]` adds a UART, outside the transport, that sends one line per
/// second. The magic and the map hash are fixed at generation, so the whole
/// line is a constant.
#[test]
fn a_heartbeat_emits_a_uart_that_repeats_the_identity_line() {
    let dir = fixture(
        &veryl_toml(),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n\
         [heartbeat]\npin = \"uart_tx\"\n\n\
         [bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n",
    );

    let out = run_gen(&dir, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let uart = fs::read_to_string(dir.path().join("hns/src/uart.veryl")).unwrap();
    // 100 MHz / 115200 = 868.
    assert!(tight(&uart).contains("const DIV: u32 = 868;"), "{uart}");
    // The line starts with "hns ", then the magic in hex.
    assert!(uart.contains("8'h68"), "'h' should be in the line:\n{uart}");
    assert!(
        uart.contains("8'h0a"),
        "the line should end with LF:\n{uart}"
    );

    let top = fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap();
    assert!(tight(&top).contains("o_uart_tx: output 'c0 logic"), "{top}");
    assert!(top.contains("inst u_uart: uart"), "{top}");

    let xdc = fs::read_to_string(dir.path().join("hns/syn/board.xdc")).unwrap();
    assert!(xdc.contains("PACKAGE_PIN D10"), "{xdc}");
    assert!(xdc.contains("get_ports { o_uart_tx }"), "{xdc}");
}

/// A baud rate too fast for the clock is rejected at generation.
#[test]
fn a_baud_the_clock_cannot_divide_is_rejected() {
    let dir = fixture(
        &veryl_toml(),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 1\n\n\
         [heartbeat]\npin = \"uart_tx\"\nbaud = 3000000\n\n\
         [bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n",
    );

    let out = run_gen(&dir, &[]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("clocks per bit"), "{stderr}");
    assert!(
        !dir.path().join("hns").exists(),
        "nothing should be written"
    );
}

/// The DUT project does not need to declare `hns`.
///
/// The output is a separate project, and gen writes its `Veryl.toml`. The
/// DUT does not use `hns` parts. If the DUT declares `hns`, that declaration
/// is used, so a local checkout or a pinned rev is kept.
#[test]
fn a_project_that_does_not_declare_hns_gets_the_default() {
    let plain = "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n\
                 [build]\nreset_type = \"async_low\"\n";
    let dir = fixture(plain, HARNESS_TOML);

    let out = run_gen(&dir, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let toml = fs::read_to_string(dir.path().join("hns/Veryl.toml")).unwrap();
    assert!(toml.contains("hns = { git ="), "{toml}");

    // Veryl must accept it. A git dependency without `version` stops
    // `veryl build` with `InvalidDependency: version is not specified`, although
    // gen passes. Check with Veryl's own types, not by spelling.
    let value: toml::Table = toml.parse().unwrap();
    let hns = value
        .get("dependencies")
        .and_then(|d| d.get("hns"))
        .expect("the generated project depends on hns");
    assert!(hns.get("git").is_some(), "{hns}");
    assert!(hns.get("version").is_some(), "{hns}");
    // The version is the one `hns` declares, so it is kept in one place.
    let package: toml::Table = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("rtl/hns/Veryl.toml"),
    )
    .unwrap()
    .parse()
    .unwrap();
    assert_eq!(
        hns.get("version").and_then(|v| v.as_str()),
        package["project"]["version"].as_str(),
        "{hns}"
    );
    // Veryl can load it as is.
    veryl_metadata::Metadata::load(dir.path().join("hns/Veryl.toml")).unwrap();

    // If the DUT declares `hns`, that declaration is used.
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    assert_eq!(run_gen(&dir, &[]).status.code(), Some(0));
    let toml = fs::read_to_string(dir.path().join("hns/Veryl.toml")).unwrap();
    assert!(toml.contains("hns = { path ="), "{toml}");
}

/// The TCK limit comes from the target description. If it is missing, gen
/// fails instead of using a default: if the host ran TCK faster, the timing
/// analysis would not describe the real design.
#[test]
fn gen_needs_the_target_to_say_how_fast_tck_may_run() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);

    // A patch removes `[jtag] max_tck_mhz` from the target.
    let patch = dir.path().join("no-tck.toml");
    fs::write(&patch, "[jtag]\nmax_tck_mhz = \"\"\n").unwrap();

    let out = run_gen(&dir, &["--target-patch", patch.to_str().unwrap()]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("max_tck_mhz"), "{stderr}");
    assert!(stderr.contains("constrains the bridge"), "{stderr}");
}

/// The BAR aperture width reaches the generated top.
///
/// The completer passes the request address as is, with the BAR base (set by
/// the BIOS) still in it. Only narrowing the master's address width to the BAR
/// size makes it BAR-relative. With the default 32 bits, every access fails the
/// window range check: it returns SLVERR, yet the host sees a plausible value
/// (seen on a real VCU118). The width comes from `bar_bytes`, so both the
/// default and changed values are tested.
#[test]
fn the_bar_aperture_reaches_the_generated_top() {
    for (bar_bytes, bits) in [(None, 12), (Some(8192), 13), (Some(1 << 20), 20)] {
        let mut harness_toml = HARNESS_TOML.to_string();
        if let Some(bytes) = bar_bytes {
            harness_toml.push_str(&format!("\n[pcie]\nbar_bytes = {bytes}\n"));
        }
        let dir = fixture(&veryl_toml(), &harness_toml);
        let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
            .args(["gen", "--target", "xilinx/vcu118", "--transport", "pcie"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let top = tight(&fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap());
        assert!(
            top.contains(&format!("BAR_ADDR_WIDTH: {bits},")),
            "bar_bytes {bar_bytes:?} should give {bits} address bits\n{top}"
        );
    }
}

/// The class code reaches the PCIe IP in the form each IP takes. On the PCIE3
/// IP `PF0_CLASS_CODE` cannot be set: Vivado ignored it with a warning, and
/// the KCU105 reported `058000`. That IP builds it from three parts.
#[test]
fn the_class_code_reaches_both_pcie_ips() {
    for (class_code, want) in [(None, "ff0000"), (Some("0x120000"), "120000")] {
        let mut harness_toml = HARNESS_TOML.to_string();
        if let Some(code) = class_code {
            harness_toml.push_str(&format!("\n[pcie]\nclass_code = {code}\n"));
        }
        let dir = fixture(&veryl_toml(), &harness_toml);
        for target in ["xilinx/vcu118", "xilinx/kcu105"] {
            let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
                .args(["gen", "--target", target, "--transport", "pcie"])
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let tcl = fs::read_to_string(dir.path().join("hns/syn/pcie.tcl")).unwrap();
            if target == "xilinx/vcu118" {
                assert!(
                    tcl.contains(&format!("CONFIG.PF0_CLASS_CODE {{{want}}}")),
                    "{tcl}"
                );
            } else {
                assert!(!tcl.contains("PF0_CLASS_CODE"), "{tcl}");
                let parts = [
                    ("base", &want[0..2]),
                    ("sub", &want[2..4]),
                    ("interface", &want[4..6]),
                ];
                for (part, value) in parts {
                    assert!(
                        tcl.contains(&format!("CONFIG.pf0_class_code_{part} {{{value}}}")),
                        "{part}\n{tcl}"
                    );
                }
            }
            let regs: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(dir.path().join("hns/regs.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                regs["pcie"]["class_code"].as_u64(),
                Some(u64::from_str_radix(want, 16).unwrap())
            );
        }
    }
}

/// `--transport` without a target is refused.
///
/// A transport has meaning only against the target's `provides.transport`.
/// Ignoring it silently would make an option that has no effect.
#[test]
fn a_transport_without_a_target_is_refused() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    for transport in ["pcie", "jtag", "nonsense"] {
        let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
            .args(["check", "--transport", transport])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(0), "{transport} was accepted");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("transport_needs_a_target"), "{stderr}");
        // The message says how to fix it.
        assert!(stderr.contains("--target"), "{stderr}");
    }

    // With a target, it passes.
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args([
            "check",
            "--target",
            "digilent/arty-a7-35",
            "--transport",
            "jtag",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// An AXI4 modport port passes with neither `ports` nor `contract`.
///
/// AXI4 has about 40 signals. The type gives direction and widths, and AXI4
/// defines its own flow control, so the manifest needs only `backing`.
///
/// `bram` says where the data lives, which is separate from the port shape.
/// `dram` would be refused here: this 32-bit port does not match the 128-bit
/// controller.
#[test]
fn an_axi4_bundle_needs_only_its_backing() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Veryl.toml"),
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[build]\nreset_type = \"async_low\"\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk: input clock,
    i_rst: input reset,
    mem  : modport $std::axi4_if::<$std::axi4_pkg::<32, 4, 4, 1, 1, 1, 1, 1>>::master,
) {
    always_comb {
        mem.awvalid = 0;
    }
}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("Harness.toml"),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.mem]\nbacking = \"bram\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--target", "digilent/arty-a7-35", "--verbose"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Shown decoded. The IR spelling `__axi4_pkg__32__4__4__1__1__1__1__1`
    // does not show widths or direction.
    assert!(
        stdout.contains("axi4 master addr=32 data=32 id=4"),
        "{stdout}"
    );
    // The role is `axi4`, not payload.
    assert!(stdout.contains("axi4"), "{stdout}");
    assert!(stdout.contains("backing=bram"), "{stdout}");
}

/// A DUT with one AXI4 master, backed by `dram`.
fn dram_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Veryl.toml"),
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[build]\nreset_type = \"async_low\"\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk: input clock,
    i_rst: input reset,
    mem  : modport $std::axi4_if::<$std::axi4_pkg::<28, 4, 8, 1, 1, 1, 1, 1>>::master,
) {
    always_comb {
        mem.awvalid = 0;
    }
}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("Harness.toml"),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n[bundle.mem]\nbacking = \"dram\"\ndepth = \"64M\"\naperture = 256\n",
    )
    .unwrap();
    dir
}

/// A clock only the memory controller takes has no DUT port, so `check` names
/// what it is for.
#[test]
fn check_names_the_clocks_of_the_memory_controller() {
    let dir = dram_fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["check", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = tight(&String::from_utf8_lossy(&output.stdout));
    assert!(
        stdout.contains("ok clock memory controller system clock"),
        "{stdout}"
    );
    assert!(
        stdout.contains("ok clock memory controller reference clock"),
        "{stdout}"
    );
}

/// The simulation stand-in does not take the real `depth`.
///
/// For `dram`, `depth` is how much of the chip is shown as a region, and can be
/// 256 MB. A stand-in that large does not start in the simulator (Veryl's
/// evaluation limit). Synthesis must keep the real size. Both are checked.
#[test]
fn the_simulation_stand_in_for_dram_does_not_carry_the_whole_chip() {
    let dir = dram_fixture();

    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args([
            "gen",
            "--target",
            "xilinx/vcu118",
            "--transport",
            "jtag",
            "-o",
            "hns",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let sim = fs::read_to_string(dir.path().join("hns").join("src").join("sim.veryl")).unwrap();
    // The stand-in covers only the start, and the output says how much of it.
    assert!(sim.contains("DEPTH: 4096"), "{sim}");
    assert!(
        sim.contains("stands in for the controller: the first 4096 entries of 67108864"),
        "{sim}"
    );
    assert!(!sim.contains("DEPTH: 67108864"), "{sim}");

    // Synthesis uses the real controller, so the stand-in size does not appear.
    let top = fs::read_to_string(dir.path().join("hns").join("src").join("top.veryl")).unwrap();
    assert!(!top.contains("DEPTH: 4096"), "{top}");
    assert!(top.contains("c0_ddr4_adr"), "{top}");

    // Only the pins of the controller clock are set here. The DDR4 IP sets the
    // period in its own XDC, and setting it here overrides that
    // (CRITICAL WARNING 18-1056).
    let board = fs::read_to_string(dir.path().join("hns/syn/board.xdc")).unwrap();
    assert!(board.contains("PACKAGE_PIN E12"), "{board}");
    assert!(board.contains("get_ports { i_mig_clk_p }"), "{board}");
    assert!(!board.contains("create_clock"), "{board}");
}

/// A PCIe design with `dram` gets a path where the card is the requester.
///
/// Reads through the window cannot go below 1.30 us per word, so to go faster
/// the card must write host memory itself. The `dma_*` registers and the
/// hardware (`hns::dma_gate` / `hns::dma_wr`) always come as a pair.
#[test]
fn a_pcie_design_with_a_controller_gets_a_requester() {
    let dir = tempfile::tempdir().unwrap();
    // `hns` is a path dependency. Otherwise the output has the default git
    // line, and `veryl check` below tries to clone it.
    fs::write(dir.path().join("Veryl.toml"), veryl_toml()).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    i_clk: input clock,
    i_rst: input reset,
    mem  : modport $std::axi4_if::<$std::axi4_pkg::<28, 4, 8, 1, 1, 1, 1, 1>>::master,
) {
    // Tie every master output. `veryl check` on the output analyzes the DUT
    // too, and an unassigned-signal warning fails it.
    always_comb {
        mem.awvalid = 0;
        mem.awaddr = 0;
        mem.awsize = 0;
        mem.awburst = 0;
        mem.awcache = 0;
        mem.awprot = 0;
        mem.awid = 0;
        mem.awlen = 0;
        mem.awlock = 0;
        mem.awqos = 0;
        mem.awregion = 0;
        mem.awuser = 0;
        mem.wvalid = 0;
        mem.wlast = 0;
        mem.wdata = 0;
        mem.wstrb = 0;
        mem.wuser = 0;
        mem.bready = 0;
        mem.arvalid = 0;
        mem.araddr = 0;
        mem.arsize = 0;
        mem.arburst = 0;
        mem.arcache = 0;
        mem.arprot = 0;
        mem.arid = 0;
        mem.arlen = 0;
        mem.arlock = 0;
        mem.arqos = 0;
        mem.arregion = 0;
        mem.aruser = 0;
        mem.rready = 0;
    }
}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("Harness.toml"),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n[bundle.mem]\nbacking = \"dram\"\ndepth = \"64M\"\naperture = 256\n\n[pcie]\nbar_bytes = 8192\n",
    )
    .unwrap();

    let run = |transport: &str, out_dir: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
            .args([
                "gen",
                "--target",
                "xilinx/vcu118",
                "--transport",
                transport,
                "-o",
                out_dir,
            ])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };

    run("pcie", "hns");
    // The output passes `veryl check` as is. gen builds many wires around the
    // DMA engine, and a wire used before its declaration would otherwise show
    // up only in synthesis.
    if let Err(why) = common::veryl_check(&dir.path().join("hns")) {
        panic!("the PCIe harness with a requester does not check:\n{why}");
    }
    let top = fs::read_to_string(dir.path().join("hns").join("src").join("top.veryl")).unwrap();
    // The gate, the borrowed DMA engines, and the only crossings.
    assert!(top.contains("inst u_dma_gate: hns::dma_gate"), "{top}");
    assert!(top.contains("inst u_dma_wr: hns::dma_wr"), "{top}");
    assert!(top.contains("inst u_dma_rd: hns::dma_rd"), "{top}");
    assert!(top.contains("inst u_rq_cdc: hns::tlp_cdc"), "{top}");
    assert!(top.contains("inst u_rc_cdc: hns::tlp_cdc"), "{top}");
    // Arbitration is on the wide side. The DMA engine's 256 bits are narrowed
    // to the controller width first (`hns::axi_dw` only widens).
    assert!(top.contains("inst u_ddw_mem: hns::axi_dw"), "{top}");
    assert!(top.contains("inst u_drr_mem: hns::axi_rr"), "{top}");
    // RQ is not tied to 0, and RC is not dropped.
    assert!(!top.contains("i_rq_tvalid : 0"), "{top}");
    assert!(!top.contains("i_rc_tready : 1"), "{top}");

    // `go` does not hold its value. A held register would keep issuing
    // descriptors after one write.
    let csr = fs::read_to_string(dir.path().join("hns").join("src").join("csr.veryl")).unwrap();
    assert!(!csr.contains("reg_dma_go"), "{csr}");
    assert!(csr.contains("assign o_dma_go = i_we &&"), "{csr}");

    let map: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join("hns").join("regs.json")).unwrap(),
    )
    .unwrap();
    let names: Vec<String> = map["registers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "dma_base"), "{names:?}");
    assert!(names.iter().any(|n| n == "dma_dir"), "{names:?}");
    assert!(names.iter().any(|n| n == "dma_mrrs"), "{names:?}");
    // Which memory the DMA engine reaches. The host must not use the DMA
    // engine for another bundle, or it reads and writes the wrong memory.
    assert_eq!(map["pcie"]["requester"], "mem", "{map}");
    // Count what is lost silently: RQ TLPs dropped by the hard block, and
    // completions dropped by the borrowed read engine.
    assert!(names.iter().any(|n| n == "dma_rq_drops"), "{names:?}");
    assert!(names.iter().any(|n| n == "dma_rc_cor"), "{names:?}");
    assert!(names.iter().any(|n| n == "dma_rc_uncor"), "{names:?}");
    assert!(top.contains("o_rq_drops_gray: pcie_rq_drops_gray"), "{top}");
    assert!(
        top.contains("inst u_rqdrop_sync: $std::synchronizer_basic"),
        "{top}"
    );
    // A whole TLP is buffered before it is sent. Dropping `tvalid` in the
    // middle makes the TLP nullified.
    assert!(top.contains("inst u_rq_hold: hns::tlp_hold"), "{top}");
    assert!(
        top.contains("inst u_rqgap_sync: $std::synchronizer_basic"),
        "{top}"
    );
    assert!(names.iter().any(|n| n == "dma_rq_gaps"), "{names:?}");
    // The formatter aligns before `:`, so remove whitespace.
    let flat: String = top.split_whitespace().collect();
    assert!(flat.contains("o_cor_count:t_dma_rc_cor,"), "{top}");
    // Crossings are constrained, not cut: the counter synchronizers and the
    // `tlp_cdc` reset synchronizers.
    let xdc = fs::read_to_string(dir.path().join("hns").join("syn").join("harness.xdc")).unwrap();
    assert!(xdc.contains("*u_rqdrop_sync/*rg_reg*"), "{xdc}");
    assert!(xdc.contains("*u_rqgap_sync/*rg_reg*"), "{xdc}");
    assert!(
        xdc.contains("*u_rq_cdc/u_fifo/u_reset_sync/*rg_reg*"),
        "{xdc}"
    );
    assert!(
        xdc.contains("*u_rc_cdc/u_fifo/u_reset_sync/*rg_reg*"),
        "{xdc}"
    );

    // JTAG gets none of this: the card has no host memory to write to.
    run("jtag", "hnsj");
    let jtag = fs::read_to_string(dir.path().join("hnsj").join("src").join("top.veryl")).unwrap();
    assert!(!jtag.contains("dma_gate"), "{jtag}");
}

/// The window watchdog limit is computed from the clock.
///
/// A fixed cycle count would be too long in real time on a slow clock, so the
/// PCIe completion timeout fires first. On a fast clock it would be too short
/// and cut normal accesses. Two clocks are tested, so a constant cannot pass.
#[test]
fn the_window_timeout_comes_from_the_clock() {
    for (mhz, cycles) in [(200, "2000"), (50, "500")] {
        let dir = fixture(
            &veryl_toml(),
            &format!(
                "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = {mhz}\n\n\
                 [bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n"
            ),
        );
        let out = run_gen(&dir, &[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let top = fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap();
        // The formatter aligns parameter names, so remove spaces.
        assert!(
            top.replace(' ', "").contains(&format!("TIMEOUT:{cycles},")),
            "{mhz} MHz should give TIMEOUT: {cycles} (10 us):\n{top}"
        );
    }
}

/// The host can read the timeout count. `SLVERR` does not reach the host over
/// PCIe, so this is the only way to know that a read got no answer.
#[test]
fn the_map_publishes_the_timeout_counter() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let out = run_gen(&dir, &[]);
    assert_eq!(out.status.code(), Some(0));

    let value: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join("hns").join("regs.json")).unwrap(),
    )
    .unwrap();
    let register = value["registers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "harness_timeout")
        .expect("regs.json should publish harness_timeout");
    // A read returns the count; a write clears it. Without a clear, every read
    // after the first timeout is non-zero, and a new timeout cannot be seen.
    assert_eq!(register["access"], "rw");

    // The window drives the count on a wire. It is not a constant, so unlike
    // the identity header it has no value.
    assert!(register.get("value").is_none() || register["value"].is_null());

    // The register does not store the written value. The write goes to the
    // window as a pulse.
    let csr = fs::read_to_string(dir.path().join("hns/src/csr.veryl")).unwrap();
    assert!(
        csr.contains("o_clear_timeouts"),
        "the write should reach the window as a pulse:\n{csr}"
    );
    assert!(
        !csr.contains("reg_harness_timeout"),
        "the count lives in hns::axil, not in a CSR register:\n{csr}"
    );
}

/// Only a PCIe design has the PCI identity in `regs.json`.
///
/// The host uses it to find the card, and the BAR size to catch the wrong
/// card. In a JTAG design, the host would look for a device that is not there.
#[test]
fn the_map_carries_the_pcie_identity_only_when_there_is_one() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let out = run_gen(&dir, &[]);
    assert_eq!(out.status.code(), Some(0));
    let jtag: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join("hns").join("regs.json")).unwrap(),
    )
    .unwrap();
    assert!(
        jtag.get("pcie").is_none(),
        "a jtag design should not name a PCI device: {jtag}"
    );

    // With PCIe, the map has the same values that were given to the IP.
    let dir = fixture(
        &veryl_toml(),
        &format!(
            "{HARNESS_TOML}\n[pcie]\nvendor_id = 0x10ee\ndevice_id = 0x903f\nbar_bytes = 8192\n"
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["gen", "--target", "xilinx/vcu118", "--transport", "pcie"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pcie: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join("hns").join("regs.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(pcie["pcie"]["vendor_id"], 0x10ee);
    assert_eq!(pcie["pcie"]["device_id"], 0x903f);
    assert_eq!(pcie["pcie"]["bar_bytes"], 8192);
}

/// A short target name is recorded in full. It may match two boards later.
#[test]
fn a_short_target_name_is_recorded_in_full() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let out = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["gen", "--target", "digilent/arty-a7-3"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    for file in ["harness.json", "regs.json"] {
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("hns").join(file)).unwrap())
                .unwrap();
        assert_eq!(value["target"], "digilent/arty-a7-35", "{file}");
    }
}

/// `update` repeats the generation with the recorded arguments.
///
/// If the user typed them again, a harness could be overwritten for another
/// board. The marker says only "generated", so it cannot prevent that.
#[test]
fn update_repeats_the_generation_that_made_the_directory() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    assert_eq!(run_gen(&dir, &[]).status.code(), Some(0));

    let record: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join("hns").join("harness.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["target"], "digilent/arty-a7-35");

    // It works with no arguments. The target comes from the record.
    let out = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .arg("update")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// With no record, `update` says what to do.
#[test]
fn update_without_a_record_says_to_generate_first() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let out = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["update", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "harness::update::no_record");
}

/// A file that is no longer generated is removed.
///
/// The output is its own project, so a stale file goes into the build. For
/// example, a removed UART would still be there.
#[test]
fn a_file_that_is_no_longer_generated_is_removed() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    // A stale file with the marker counts as generated.
    assert_eq!(run_gen(&dir, &[]).status.code(), Some(0));
    let stale = dir.path().join("hns").join("src").join("zz_old.veryl");
    fs::write(&stale, "/// veryl-harness:generated\nmodule zz_old {}\n").unwrap();
    // A file without the marker is not touched.
    let mine = dir.path().join("hns").join("src").join("mine.veryl");
    fs::write(&mine, "module mine {}\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .arg("update")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(!stale.exists(), "a stale generated file should be removed");
    assert!(mine.exists(), "a hand-written file must be left alone");
}

/// `update` regenerates from the recorded project.
///
/// The project search starts in the current directory. Run from elsewhere with
/// `-o`, it would overwrite the harness with that directory's DUT, which
/// silently writes a different design.
#[test]
fn update_uses_the_project_it_was_generated_from() {
    let mine = fixture(&veryl_toml(), HARNESS_TOML);
    assert_eq!(run_gen(&mine, &[]).status.code(), Some(0));
    let before = fs::read_to_string(mine.path().join("hns").join("regs.json")).unwrap();

    // Update that harness from a project with a different DUT.
    let other = fixture(
        &veryl_toml(),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n\
         [bundle.csr]\nbacking = \"reg\"\nports = [\"i_csr_wdata\"]\n\n\
         [leave_open]\nports = [\"o_csr_rdata\"]\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .arg("update")
        .arg("-o")
        .arg(mine.path().join("hns"))
        .current_dir(other.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = fs::read_to_string(mine.path().join("hns").join("regs.json")).unwrap();
    assert_eq!(
        before, after,
        "updating from another project must not rewrite this harness with that design"
    );
}

/// An output outside the project still resolves to one `hns` package.
///
/// Veryl tells dependencies apart by the written form of the path. If the DUT
/// writes a relative path and the output writes an absolute one, the package
/// comes in twice (`dependencies/hns` and `hns_0`), and the second one renames
/// modules to `hns_0_bscan`. That shows up only in synthesis, as a missing
/// module.
#[test]
fn an_out_of_tree_output_still_names_one_package() {
    // The DUT declares `hns` with a relative path, as the examples do.
    let hns = format!("{}/rtl/hns", env!("CARGO_MANIFEST_DIR"));
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let relative = pathdiff(dir.path(), std::path::Path::new(&hns));
    fs::write(
        dir.path().join("Veryl.toml"),
        format!(
            "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n\
             [build]\nreset_type = \"async_low\"\n\n\
             [dependencies]\nhns = {{ path = \"{relative}\" }}\n"
        ),
    )
    .unwrap();

    let out = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["gen", "--target", "digilent/arty-a7-35", "-o"])
        .arg(out.path().join("elsewhere"))
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let toml = fs::read_to_string(out.path().join("elsewhere/Veryl.toml")).unwrap();
    let line = toml
        .lines()
        .find(|line| line.starts_with("hns ="))
        .expect("the generated project names hns");
    // Same form: the DUT is relative, so this is relative too.
    assert!(
        !line.contains("path = \"/"),
        "the DUT declared it relative, so this must be too:\n{toml}"
    );
    // Same place: the same form pointing elsewhere would be worse.
    let written = line.split('"').nth(1).expect("a quoted path");
    let resolved = std::fs::canonicalize(out.path().join("elsewhere").join(written)).unwrap();
    assert_eq!(resolved, std::fs::canonicalize(&hns).unwrap());
}

/// The relative path from `from` to `to` (for tests).
fn pathdiff(from: &std::path::Path, to: &std::path::Path) -> String {
    let (from, to) = (
        std::fs::canonicalize(from).unwrap(),
        std::fs::canonicalize(to).unwrap(),
    );
    let common = from
        .components()
        .zip(to.components())
        .take_while(|(a, b)| a == b)
        .count();
    let mut path = std::path::PathBuf::new();
    for _ in 0..(from.components().count() - common) {
        path.push("..");
    }
    for part in to.components().skip(common) {
        path.push(part);
    }
    path.display().to_string()
}

/// The window runs on the clock of the terminated ports, not on the first
/// domain by name.
///
/// The reg bundle is in the second domain (`'b`, 50 MHz). The window timeout
/// must be counted at 50 MHz, not at the `'a` frequency (100 MHz).
#[test]
fn the_window_runs_on_the_clock_of_the_terminated_ports() {
    let dir = fixture(
        &veryl_toml(),
        "[dut]\nmodule = \"dut_top\"\n\n[clock.ia_clk]\nfreq_mhz = 100\n\n[clock.ib_clk]\nfreq_mhz = 50\n\n[bundle.csr]\nbacking = \"reg\"\n",
    );
    fs::write(
        dir.path().join("src").join("fixture.veryl"),
        r#"
module dut_top (
    ia_clk     : input  'a clock   ,
    ia_rst     : input  'a reset   ,
    ib_clk     : input  'b clock   ,
    ib_rst     : input  'b reset   ,
    i_csr_wdata: input  'b logic<8>,
    o_csr_rdata: output 'b logic<8>,
) {
    let _unused: 'a logic = ia_clk | ia_rst;
    always_ff (ib_clk, ib_rst) {
        if_reset {
            o_csr_rdata = 0;
        } else {
            o_csr_rdata = i_csr_wdata;
        }
    }
}
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["gen", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let top = tight(&fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap());
    // 10 us at 50 MHz. At 100 MHz it would be 1000.
    assert!(top.contains("TIMEOUT : 500,"), "{top}");
    assert!(top.contains("i_clk : clk_b ,"), "{top}");
    assert!(!top.contains("i_clk : clk_a"), "{top}");
}

/// One pin never gets two drivers. If `[heartbeat]` and `[pin]` take the same
/// resource, or make the same top port name (`o_<resource>` and the DUT port
/// name), gen refuses before generation. Otherwise only Vivado would fail.
#[test]
fn a_heartbeat_does_not_share_a_pin_or_a_port_name_with_pin() {
    const DUT: &str = r#"
module dut_top (
    i_clk      : input  clock   ,
    i_rst      : input  reset   ,
    i_csr_wdata: input  logic<8>,
    o_csr_rdata: output logic<8>,
    o_uart_tx  : output logic   ,
) {
    always_ff {
        if_reset {
            o_csr_rdata = 0;
            o_uart_tx   = 0;
        } else {
            o_csr_rdata = i_csr_wdata;
            o_uart_tx   = i_csr_wdata[0];
        }
    }
}
"#;
    let head = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n\
                [heartbeat]\npin = \"uart_tx\"\n\n\
                [bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n\n";
    for (pin, patch, code) in [
        ("uart_tx", None, "harness::heartbeat::pin_resource_taken"),
        (
            "led0",
            Some("[pins.led0]\npin = \"H5\"\nstandard = \"LVCMOS33\"\ndirection = \"output\"\n"),
            "harness::heartbeat::port_name_taken",
        ),
    ] {
        let dir = fixture(
            &veryl_toml(),
            &format!("{head}[pin]\no_uart_tx = \"{pin}\"\n"),
        );
        fs::write(dir.path().join("src").join("fixture.veryl"), DUT).unwrap();
        let mut args = vec!["--json".to_string()];
        if let Some(patch) = patch {
            let path = dir.path().join("led.toml");
            fs::write(&path, patch).unwrap();
            args.push("--target-patch".to_string());
            args.push(path.to_string_lossy().to_string());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_gen(&dir, &args);
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(out.status.code(), Some(1), "{value:#}");
        assert_eq!(value["error"]["code"], code, "{value:#}");
        assert!(
            !dir.path().join("hns").exists(),
            "nothing should be written"
        );
    }
}

/// A PCIe link whose stream width and user clock are unknown is refused, not
/// given a default. `check --target` stops for the same reason.
#[test]
fn a_pcie_link_the_generator_does_not_know_is_refused() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let patch = dir.path().join("x1.toml");
    fs::write(
        &patch,
        "[pcie]\nlanes = 1\nrx_p = [\"AA4\"]\ntx_p = [\"Y7\"]\n",
    )
    .unwrap();
    for command in ["gen", "check"] {
        let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
            .args([
                command,
                "--json",
                "--target",
                "xilinx/vcu118",
                "--transport",
                "pcie",
            ])
            .arg("--target-patch")
            .arg(&patch)
            .current_dir(dir.path())
            .output()
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{command}: {value:#}");
        assert_eq!(
            value["error"]["code"], "harness::plan::pcie_link_unsupported",
            "{command}: {value:#}"
        );
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("gen3 x1"),
            "{value:#}"
        );
    }
    assert!(
        !dir.path().join("hns").exists(),
        "nothing should be written"
    );
}

/// A map made with `--target-file` does not record the description's path.
///
/// A path depends on the machine, and the file can change after generation
/// without notice. The map records only that the target came from a file, in
/// `target_source`, and `hio` then asks for `--target-file` (see
/// `a_map_from_a_target_file_asks_for_the_file` in `crates/host`).
#[test]
fn a_map_from_a_target_file_records_the_origin_but_not_the_path() {
    let arty = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/crates/targets/targets/digilent/arty-a7-35/default.toml"
    );
    for (args, target, source) in [
        (vec!["--target-file", arty], None, "file"),
        (
            vec!["--target", "digilent/arty-a7-35"],
            Some("digilent/arty-a7-35"),
            "name",
        ),
    ] {
        let dir = fixture(&veryl_toml(), HARNESS_TOML);
        let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
            .arg("gen")
            .args(&args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let map: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
                .unwrap();
        assert_eq!(map["target"].as_str(), target, "{args:?}: {map:#}");
        assert_eq!(map["target_source"], source, "{args:?}: {map:#}");
        // The reader can parse it: `hns-regs` defines the contract.
        let parsed: hns_regs::RegisterMap =
            serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
                .unwrap();
        assert_eq!(parsed.target_source.as_deref(), Some(source));
    }
}

/// `gen --json` also says what was checked. `gen` goes through the same entry
/// as `check --target`, so it gives the same `checked` / `not_checked` lists.
#[test]
fn gen_json_says_what_was_checked() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let output = run_gen(&dir, &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value:#}");
    let ids = |key: &str| -> Vec<String> {
        value[key]
            .as_array()
            .unwrap_or_else(|| panic!("no {key}: {value:#}"))
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(
        ids("checked").contains(&"generatable".to_string()),
        "{value:#}"
    );
    assert!(!ids("not_checked").is_empty(), "{value:#}");
}

/// Generating for jtag after pcie removes the borrowed Verilog in `vendor/`.
/// Borrowed files cannot carry the marker. If they stayed, `synth.tcl` would
/// pick up `../vendor/*.v` and put unused PCIe Verilog into synthesis.
#[test]
fn switching_from_pcie_to_jtag_removes_the_borrowed_verilog() {
    let dir = fixture(&veryl_toml(), HARNESS_TOML);
    let generate = |transport: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
            .args(["gen", "--target", "xilinx/vcu118", "--transport", transport])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{transport}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let vendor = dir.path().join("hns/vendor");
    generate("pcie");
    assert!(
        fs::read_dir(&vendor).unwrap().count() > 0,
        "pcie writes the borrowed Verilog"
    );
    // A user file outside `vendor/` is not touched.
    fs::write(dir.path().join("hns/mine.txt"), "keep\n").unwrap();

    generate("jtag");
    let left: Vec<_> = fs::read_dir(&vendor)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "left behind: {left:?}");
    assert!(dir.path().join("hns/mine.txt").exists());
}

/// A DUT with an interrupt line beside the CSR. `o_irq` follows bit 0 of
/// what the host wrote.
const IRQ_DUT: &str = r#"
module dut_top (
    i_clk      : input  clock   ,
    i_rst      : input  reset   ,
    i_csr_wdata: input  logic<8>,
    o_csr_rdata: output logic<8>,
    o_irq      : output logic   ,
    o_irqs     : output logic<2>,
) {
    always_ff {
        if_reset {
            o_csr_rdata = 0;
        } else {
            o_csr_rdata = i_csr_wdata;
        }
    }
    assign o_irq  = o_csr_rdata[0];
    assign o_irqs = o_csr_rdata[2:1];
}
"#;

fn irq_fixture(bundles: &str) -> tempfile::TempDir {
    let harness_toml = format!(
        "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n\
         [bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\nports = [\"i_csr_wdata\", \"o_csr_rdata\"]\n\n{bundles}"
    );
    let dir = fixture(&veryl_toml(), &harness_toml);
    fs::write(dir.path().join("src").join("fixture.veryl"), IRQ_DUT).unwrap();
    dir
}

fn gen_pcie(dir: &tempfile::TempDir, target: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["gen", "--target", target, "--transport", "pcie"])
        .current_dir(dir.path())
        .output()
        .unwrap()
}

/// A `host_irq` line reaches the PCIe wrapper, the IP gets INTA, and the
/// window shows the level. Without one the IP has no interrupt pin, on both
/// IPs (the PCIE3 one would default to INTA).
#[test]
fn a_host_irq_line_becomes_inta() {
    const IRQ: &str = "[bundle.irq]\nbacking = \"host_irq\"\nports = [\"o_irq\"]\n\n\
                       [leave_open]\nports = [\"o_irqs\"]\n";
    for target in ["xilinx/vcu118", "xilinx/kcu105:dr"] {
        let dir = irq_fixture(IRQ);
        let out = gen_pcie(&dir, target);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{target}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let top = tight(&fs::read_to_string(dir.path().join("hns/src/top.veryl")).unwrap());
        assert!(top.contains("i_intx : pcie_intx ,"), "{top}");
        assert!(top.contains("assign pcie_intx = w_o_irq;"), "{top}");
        assert!(top.contains("assign t_irq_irq_level = w_o_irq;"), "{top}");
        let tcl = fs::read_to_string(dir.path().join("hns/syn/pcie.tcl")).unwrap();
        assert!(tcl.contains("CONFIG.PF0_INTERRUPT_PIN {INTA}"), "{tcl}");
        let xdc = fs::read_to_string(dir.path().join("hns/syn/harness.xdc")).unwrap();
        assert!(xdc.contains("*u_pcie/intx_meta_reg"), "{xdc}");
        let regs: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
                .unwrap();
        let level = regs["registers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "irq_level")
            .unwrap_or_else(|| panic!("no irq_level\n{regs}"));
        assert_eq!(level["access"], "ro");
        assert_eq!(level["width"], 1);
        common::veryl_check(&dir.path().join("hns")).unwrap();

        let plain = irq_fixture("[leave_open]\nports = [\"o_irq\", \"o_irqs\"]\n");
        let out = gen_pcie(&plain, target);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{target}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let top = tight(&fs::read_to_string(plain.path().join("hns/src/top.veryl")).unwrap());
        assert!(top.contains("assign pcie_intx = 0;"), "{top}");
        let tcl = fs::read_to_string(plain.path().join("hns/syn/pcie.tcl")).unwrap();
        assert!(tcl.contains("CONFIG.PF0_INTERRUPT_PIN {NONE}"), "{tcl}");
    }
}

/// INTx is one 1-bit line, and the card has one pin.
#[test]
fn a_host_irq_that_is_not_one_line_is_refused() {
    for (bundles, want) in [
        (
            "[bundle.irq]\nbacking = \"host_irq\"\nports = [\"o_irqs\"]\n\n[leave_open]\nports = [\"o_irq\"]\n",
            "`o_irqs` 2 bits wide",
        ),
        (
            "[bundle.irq]\nbacking = \"host_irq\"\nports = [\"o_irq\"]\n\n\
             [bundle.more]\nbacking = \"host_irq\"\nports = [\"o_irqs\"]\n",
            "are both host_irq",
        ),
    ] {
        let dir = irq_fixture(bundles);
        let out = gen_pcie(&dir, "xilinx/vcu118");
        assert_ne!(out.status.code(), Some(0));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(want), "{stderr}");
    }
    // JTAG has no way to interrupt the host.
    let dir = irq_fixture(
        "[bundle.irq]\nbacking = \"host_irq\"\nports = [\"o_irq\"]\n\n[leave_open]\nports = [\"o_irqs\"]\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["gen", "--target", "xilinx/vcu118", "--transport", "jtag"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("host_irq"), "{stderr}");
}

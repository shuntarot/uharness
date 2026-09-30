//! Applies one reference DUT to every shipped target.
//!
//! This is the only way to catch a target description that has gone stale:
//! for example a differential clock, an active-high board reset, or an IP port
//! name.
//!
//! The target list is not kept by hand. The test walks `target::list()`, so a
//! new target is covered automatically.
//!
//! The reference DUT wraps `std::fifo`. It is known to be correct, so a failure
//! points at the target description.

use std::fs;
use std::process::Command;

mod common;

/// The fixture's Veryl.toml. It uses std, so `exclude_std` is not set. It needs
/// the `hns` package, because the output refers to `hns::axil`, `hns::fifo`
/// and `hns::mem`.
fn veryl_toml() -> String {
    format!(
        "[project]\nname = \"reference\"\nversion = \"0.1.0\"\n\n[build]\nreset_type = \"async_low\"\n\n[dependencies]\nhns = {{ path = \"{}/rtl/hns\" }}\n",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// `std::fifo` in a wrapper with no parameters (a DUT must not have parameters).
const REFERENCE_DUT: &str = r#"
module dut_top (
    i_clk  : input  clock   ,
    i_rst  : input  reset   ,
    i_push : input  logic   ,
    i_data : input  logic<8>,
    o_full : output logic   ,
    i_pop  : input  logic   ,
    o_data : output logic<8>,
    o_empty: output logic   ,
) {
    inst u: $std::fifo #(
        WIDTH: 8 ,
        DEPTH: 16,
    ) (
        i_clk            ,
        i_rst            ,
        i_clear      : 0 ,
        o_empty          ,
        o_almost_full: _ ,
        o_full           ,
        o_word_count : _ ,
        i_push           ,
        i_data           ,
        i_pop            ,
        o_data           ,
    );
}
"#;

/// Puts both push and pop on CSRs, so both get the auto-clear logic.
const REFERENCE_MANIFEST: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n[bundle.push]\ncontract = \"valid_ready\"\nbacking = \"reg\"\nports = { valid = \"i_push\", ready = \"!o_full\", data = \"i_data\" }\n\n[bundle.pop]\ncontract = \"valid_ready\"\nbacking = \"reg\"\nports = { valid = \"i_pop\", ready = \"!o_empty\", data = \"o_data\" }\n";

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Veryl.toml"), veryl_toml()).unwrap();
    fs::write(dir.path().join("Harness.toml"), REFERENCE_MANIFEST).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src").join("dut_top.veryl"), REFERENCE_DUT).unwrap();
    dir
}

/// Names of the shipped targets, taken from the tool's own list.
fn shipped() -> Vec<String> {
    let names: Vec<String> = harness::target::list()
        .into_iter()
        .map(|listed| {
            listed
                .expect("a shipped target description must parse")
                .name
        })
        .collect();
    assert!(!names.is_empty(), "no target ships with the tool");
    names
}

/// Lists every transport a target provides.
///
/// If a board provides two, generation must pass for both. Testing only one
/// lets the other break without notice.
fn transports_of(name: &str) -> Vec<String> {
    let target = hns_targets::resolve(name, &[]).expect("a shipped target resolves");
    target
        .table
        .get("provides")
        .and_then(|v| v.get("transport"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| vec!["jtag".to_string()])
}

/// For every target and transport, generation passes and the output compiles.
#[test]
fn every_shipped_target_generates_a_harness_that_compiles() {
    let mut maps: Vec<(String, String)> = Vec::new();

    for name in shipped() {
        for transport in transports_of(&name) {
            let dir = fixture();
            let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
                .args(["gen", "--target", &name, "--transport", &transport])
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "gen failed for {name} over {transport}:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );

            // The output must pass `veryl check` with no warnings. It is its own
            // project, so running `gen` again does not analyze it.
            if let Err(why) = common::veryl_check(&dir.path().join("hns")) {
                panic!("the harness for {name} over {transport} does not check:\n{why}");
            }

            // gen can rewrite its own output. Files with the marker may be overwritten.
            let again = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
                .args(["gen", "--target", &name, "--transport", &transport])
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert_eq!(
                again.status.code(),
                Some(0),
                "gen could not rewrite its own output for {name} over {transport}:\n{}",
                String::from_utf8_lossy(&again.stderr)
            );

            // Board files exist, so the target really took effect.
            for file in ["board.xdc", "board.tcl", "mmcm.tcl", "Makefile"] {
                assert!(
                    dir.path().join("hns/syn").join(file).is_file(),
                    "{name}: hns/syn/{file} is missing"
                );
            }
            // Every file a Tcl script `source`s must exist. The link exists only
            // inside the generated Tcl, so nothing on the Rust side catches a
            // missing file.
            for entry in fs::read_dir(dir.path().join("hns/syn")).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().and_then(|e| e.to_str()) != Some("tcl") {
                    continue;
                }
                let text = fs::read_to_string(&path).unwrap();
                for line in text.lines() {
                    let Some(rest) = line
                        .trim()
                        .strip_prefix("source [file join [file dirname [info script]] ")
                    else {
                        continue;
                    };
                    let wanted = rest.trim_end_matches(']').trim();
                    assert!(
                        dir.path().join("hns/syn").join(wanted).is_file(),
                        "{name}: {} sources {wanted}, which is not generated",
                        path.file_name().unwrap().to_string_lossy()
                    );
                }
            }

            // Every Tcl script the Makefile runs must exist too.
            let makefile = fs::read_to_string(dir.path().join("hns/syn/Makefile")).unwrap();
            for line in makefile.lines() {
                let Some((_, rest)) = line.split_once("-source ") else {
                    continue;
                };
                let wanted = rest.split_whitespace().next().unwrap();
                assert!(
                    dir.path().join("hns/syn").join(wanted).is_file(),
                    "{name}: the Makefile runs {wanted}, which is not generated"
                );
            }

            // The MMCM IP defines the board clock. Defining it again in
            // `board.xdc` overrides the IP, and the IP's constraints on that
            // clock are ignored.
            let board = fs::read_to_string(dir.path().join("hns/syn/board.xdc")).unwrap();
            assert!(
                !board.contains("-name sys_clk"),
                "{name}: board.xdc redefines the MMCM input clock:\n{board}"
            );
            let mmcm = fs::read_to_string(dir.path().join("hns/syn/mmcm.tcl")).unwrap();
            assert!(
                mmcm.contains("CONFIG.PRIM_IN_FREQ"),
                "{name}: the MMCM is not told its input frequency:\n{mmcm}"
            );

            maps.push((
                format!("{name} ({transport})"),
                fs::read_to_string(dir.path().join("hns/regs.json")).unwrap(),
            ));
        }
    }

    // The window does not depend on the target. The host relies on this: the
    // same DUT gives the same registers on any board.
    //
    // Two fields do depend on it and are removed before comparing. `target`
    // records the board the bitstream was built for, so the host can omit
    // `--target`. `pcie` says how to find the card; it exists only in a PCIe
    // design, and in a JTAG design the host would look for a device that is
    // not there. The window itself (registers, size, hash) must match.
    let strip = |text: &str| {
        let mut v: serde_json::Value = serde_json::from_str(text).expect("regs.json is JSON");
        let object = v.as_object_mut().expect("an object");
        object.remove("target");
        object.remove("pcie");
        v
    };
    let (first_name, first_map) = &maps[0];
    for (name, map) in &maps[1..] {
        assert_eq!(
            strip(first_map),
            strip(map),
            "the window differs between {first_name} and {name}"
        );
    }

    // The hash matters most. The host uses it to check that its map matches
    // the loaded bitstream, so it must not change with the target.
    for (name, map) in &maps {
        let v: serde_json::Value = serde_json::from_str(map).unwrap();
        assert_eq!(
            v["map_hash"],
            strip(first_map)["map_hash"],
            "the map hash differs on {name}"
        );
        assert!(
            v["target"].is_string(),
            "{name}: regs.json must say which target it was generated for"
        );
    }
}

/// Runs synthesis. For a target with no board in CI, only this shows that its
/// description has gone stale.
///
/// It needs Vivado and takes 5 to 20 minutes per board, so it does not run by
/// default:
///
/// ```bash
/// cargo test --test targets -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs Vivado; minutes per target"]
fn every_shipped_target_synthesizes() {
    // Run once per transport, as in the generation test above. Without
    // `--transport` only the default (jtag) would be synthesized.
    for name in shipped() {
        for transport in transports_of(&name) {
            let dir = fixture();
            let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
                .args(["gen", "--target", &name, "--transport", &transport])
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "gen failed for {name} over {transport}:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );

            eprintln!("synthesising {name} over {transport} ...");
            let make = Command::new("make")
                .arg("bit")
                .current_dir(dir.path().join("hns/syn"))
                .output()
                .unwrap();
            assert!(
                make.status.success(),
                "{name} does not synthesise:\n{}",
                String::from_utf8_lossy(&make.stdout)
                    .lines()
                    .rev()
                    .take(40)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
}

/// The manual lists every manifest value.
///
/// The manual's tables are kept by hand, so a new value is easy to forget.
#[test]
fn the_manual_lists_every_backing_and_contract() {
    let manual =
        fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("doc/guide.md"))
            .unwrap();

    for backing in [
        "bram",
        "bram_preload",
        "host_mem",
        "dram",
        "reg",
        "host_irq",
        "host_poll_fifo",
        "observe",
    ] {
        assert!(
            manual.contains(backing),
            "doc/guide.md does not mention the `{backing}` backing"
        );
    }
    for contract in ["fixed_latency", "valid_ready", "valid_only"] {
        assert!(
            manual.contains(contract),
            "doc/guide.md does not mention the `{contract}` contract"
        );
    }
    // Removed names must not remain.
    for gone in ["`host_fifo`", "`req_ack`", "`tagged`"] {
        assert!(
            !manual.contains(gone),
            "doc/guide.md still mentions {gone}, which no longer exists"
        );
    }
}

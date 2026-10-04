//! `--target sim`: generating for the Veryl simulator, and serving the window
//! over TCP with `veryl harness sim`.
//!
//! The end-to-end test builds the socket component with cargo, which needs
//! `veryl-component` from crates.io (or the local cache).

use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use hns_host::sim::{Error, Link};

mod common;

const DUT: &str = r#"
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
"#;

const HARNESS_TOML: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\n";

fn fixture(dut: &str, harness_toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Veryl.toml"),
        format!(
            "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[build]\nreset_type = \"async_low\"\n\n[dependencies]\nhns = {{ path = \"{}/rtl/hns\" }}\n",
            env!("CARGO_MANIFEST_DIR")
        ),
    )
    .unwrap();
    fs::write(dir.path().join("Harness.toml"), harness_toml).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src").join("dut.veryl"), dut).unwrap();
    dir
}

fn harness(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn gen_for_the_simulator_writes_no_vivado_flow() {
    let dir = fixture(DUT, HARNESS_TOML);
    let out = harness(dir.path(), &["gen", "--target", "sim"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let hns = dir.path().join("hns");
    for file in [
        "regs.json",
        "src/sim.veryl",
        "src/sim_tb.veryl",
        "link/Cargo.toml",
        "link/src/lib.rs",
    ] {
        assert!(hns.join(file).is_file(), "{file} is missing");
    }
    for gone in ["syn", "src/top.veryl", "src/clk.veryl", "vendor"] {
        assert!(
            !hns.join(gone).exists(),
            "{gone} is written for a board only"
        );
    }
    let toml = fs::read_to_string(hns.join("Veryl.toml")).unwrap();
    assert!(toml.contains("[[components]]\npath = \"link\""), "{toml}");
    let regs: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(hns.join("regs.json")).unwrap()).unwrap();
    assert_eq!(regs["target"], "sim");

    // The output is one Veryl project, testbench and all.
    common::veryl_check(&hns).unwrap();
}

#[test]
fn the_simulator_refuses_systemverilog() {
    let dut = r#"
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
    let dir = fixture(dut, HARNESS_TOML);
    let out = harness(dir.path(), &["check", "--target", "sim"]);
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(text.contains("sim_sv_blackbox"), "{text}");
    assert!(text.contains("$sv::core"), "{text}");
}

/// Two clocks: each gets its own `clock_gen` with a period in picoseconds,
/// so they keep their ratio, and the DUT reset reaches both domains. Only
/// the generated project is checked here; running it takes a cargo build.
#[test]
fn the_simulator_drives_two_clocks() {
    let dut = r#"
module dut_top (
    i_clk_a    : input  'a clock   ,
    i_rst_a    : input  'a reset   ,
    i_clk_b    : input  'b clock   ,
    i_rst_b    : input  'b reset   ,
    i_csr_wdata: input  'a logic<8>,
    o_csr_rdata: output 'a logic<8>,
    o_b        : output 'b logic   ,
) {
    always_ff (i_clk_a, i_rst_a) {
        if_reset {
            o_csr_rdata = 0;
        } else {
            o_csr_rdata = i_csr_wdata;
        }
    }
    always_ff (i_clk_b, i_rst_b) {
        if_reset {
            o_b = 0;
        } else {
            o_b = ~o_b;
        }
    }
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk_a]\nfreq_mhz = 100\n\n[clock.i_clk_b]\nfreq_mhz = 50\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\nports = [\"i_csr_wdata\", \"o_csr_rdata\"]\n\n[leave_open]\nports = [\"o_b\"]\n";
    let dir = fixture(dut, manifest);
    let out = harness(dir.path(), &["gen", "--target", "sim"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let hns = dir.path().join("hns");
    let tb = fs::read_to_string(hns.join("src/sim_tb.veryl")).unwrap();
    assert!(tb.contains("period: 10000"), "{tb}");
    assert!(tb.contains("period: 20000"), "{tb}");
    let sim = fs::read_to_string(hns.join("src/sim.veryl")).unwrap();
    for each in ["drst_a", "drst_b"] {
        assert!(sim.contains(each), "{sim}");
    }
    common::veryl_check(&hns).unwrap();
}

#[test]
fn sim_refuses_a_harness_made_for_a_board() {
    let dir = fixture(DUT, HARNESS_TOML);
    let out = harness(dir.path(), &["gen", "--target", "digilent/arty-a7-35"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = harness(dir.path(), &["sim"]);
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(text.contains("not_a_sim_harness"), "{text}");
    assert!(text.contains("--target sim"), "{text}");
}

/// A sim harness kept inside the board one (`-o hns/sim`) stays when the
/// board one is generated again.
#[test]
fn a_harness_inside_another_survives_its_gen() {
    let dir = fixture(DUT, HARNESS_TOML);
    for args in [
        &["gen", "--target", "digilent/arty-a7-35"][..],
        &["gen", "--target", "sim", "-o", "hns/sim"],
        &["gen", "--target", "digilent/arty-a7-35"],
    ] {
        let out = harness(dir.path(), args);
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let sim = dir.path().join("hns").join("sim");
    for file in [
        "harness.json",
        "regs.json",
        "src/sim_tb.veryl",
        "link/src/lib.rs",
    ] {
        assert!(sim.join(file).is_file(), "{file} was removed");
    }
}

/// Inside a generated harness, `sim` takes that directory. Here it is a
/// board one, so it is refused by name.
#[test]
fn sim_inside_a_harness_takes_that_harness() {
    let dir = fixture(DUT, HARNESS_TOML);
    let out = harness(
        dir.path(),
        &["gen", "--target", "digilent/arty-a7-35", "-o", "board"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let out = harness(&dir.path().join("board"), &["sim"]);
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(text.contains("not_a_sim_harness"), "{text}");
    // The report wraps long lines, so only the start of the name.
    assert!(text.contains("digilent/arty"), "{text}");
}

/// Starts `veryl harness sim` and waits for its address. The first run
/// builds the component with cargo, so the wait is long.
fn start(dir: &Path) -> (Child, Link) {
    let addr = dir.join("hns").join("sim.addr");
    let child = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .arg("sim")
        .current_dir(dir)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    while !addr.is_file() {
        assert!(Instant::now() < deadline, "no sim.addr after 600 s");
        std::thread::sleep(Duration::from_millis(200));
    }
    // The file can be there before its line is.
    std::thread::sleep(Duration::from_millis(100));
    let link = Link::open(&dir.join("hns")).unwrap();
    (child, link)
}

#[test]
fn the_simulator_serves_the_window_over_tcp() {
    let dir = fixture(DUT, HARNESS_TOML);
    let out = harness(dir.path(), &["gen", "--target", "sim"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let hns = dir.path().join("hns");
    let regs: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(hns.join("regs.json")).unwrap()).unwrap();
    let offset = |name: &str| -> u32 {
        regs["registers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("no {name} in regs.json"))["offset"]
            .as_u64()
            .unwrap() as u32
    };

    let (mut child, mut link) = start(dir.path());
    assert_eq!(
        link.read32(offset("harness_magic")).unwrap() as u64,
        regs["magic"].as_u64().unwrap()
    );
    assert_eq!(
        link.read32(offset("harness_map_hash")).unwrap() as u64,
        regs["map_hash"].as_u64().unwrap()
    );

    // The DUT registers what the host wrote, one cycle later.
    let mut batch = hns_host::Batch::new();
    batch.write(offset("i_csr_wdata"), 0x5a);
    let read = batch.read(offset("o_csr_rdata"));
    assert_eq!(link.run(&batch).unwrap()[read], 0x5a);

    // Outside the window: an AXI error, not a value.
    let size = regs["size_bytes"].as_u64().unwrap() as u32;
    assert!(matches!(
        link.read32(size),
        Err(Error::Resp { resp: 2, .. })
    ));

    // hio connects once per command.
    drop(link);
    let link = Link::open(&hns).unwrap();
    link.finish().unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!hns.join("sim.addr").exists());

    // Ctrl-C ends it too, and takes the address with it.
    let (mut child, link) = start(dir.path());
    drop(link);
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    assert!(!child.wait().unwrap().success());
    assert!(!hns.join("sim.addr").exists());
    assert!(matches!(Link::open(&hns), Err(Error::NotRunning { .. })));
}

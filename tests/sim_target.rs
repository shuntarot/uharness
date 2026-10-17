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

/// `veryl harness sim`, killed when dropped. A test that fails before it
/// finishes the simulation would otherwise leave it running, holding the
/// test's output open.
struct Sim(Child);

impl std::ops::Deref for Sim {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Sim {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `veryl harness sim` and waits for its address. The first run
/// builds the component with cargo, so the wait is long.
fn start(dir: &Path) -> (Sim, Link) {
    let addr = dir.join("hns").join("sim.addr");
    let child = Sim(Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .arg("sim")
        .current_dir(dir)
        .spawn()
        .unwrap());
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

/// A DUT whose AXI4 master stays idle, so the host has the memory to itself.
const DRAM_DUT: &str = r#"
module dut_top (
    i_clk: input clock,
    i_rst: input reset,
    axi  : modport $std::axi4_if::<$std::axi4_pkg::<28, 4, 4, 1, 1, 1, 1, 1>>::master,
) {
    let _unused: logic = i_clk ^ i_rst;
    always_comb {
        axi.awvalid  = 0;
        axi.awaddr   = 0;
        axi.awsize   = 0;
        axi.awburst  = 0;
        axi.awcache  = 0;
        axi.awprot   = 0;
        axi.awid     = 0;
        axi.awlen    = 0;
        axi.awlock   = 0;
        axi.awqos    = 0;
        axi.awregion = 0;
        axi.awuser   = 0;
        axi.wvalid   = 0;
        axi.wlast    = 0;
        axi.wdata    = 0;
        axi.wstrb    = 0;
        axi.wuser    = 0;
        axi.bready   = 0;
        axi.arvalid  = 0;
        axi.araddr   = 0;
        axi.arsize   = 0;
        axi.arburst  = 0;
        axi.arcache  = 0;
        axi.arprot   = 0;
        axi.arid     = 0;
        axi.arlen    = 0;
        axi.arlock   = 0;
        axi.arqos    = 0;
        axi.arregion = 0;
        axi.aruser   = 0;
        axi.rready   = 0;
    }
}
"#;

const DRAM_HARNESS_TOML: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n[bundle.mem]\nbacking = \"dram\"\naperture = \"1M\"\nports = [\"axi\"]\n";

/// `dram` in the simulator is `$comp::hns_dram`, a sparse memory in the
/// testbench, not the 4096-entry stand-in a board's `hns_sim` carries. It is
/// as large as the sim target says (256 MB) and the far end is reachable.
#[test]
fn the_simulator_serves_dram_from_a_sparse_memory() {
    let dir = fixture(DRAM_DUT, DRAM_HARNESS_TOML);
    let out = harness(dir.path(), &["gen", "--target", "sim"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let hns = dir.path().join("hns");
    let sim = fs::read_to_string(hns.join("src/sim.veryl")).unwrap();
    assert!(!sim.contains("hns::axi_mem"), "{sim}");
    assert!(sim.contains("saxi_mem"), "{sim}");
    let tb = fs::read_to_string(hns.join("src/sim_tb.veryl")).unwrap();
    assert!(tb.contains("$comp::hns_dram"), "{tb}");
    assert!(hns.join("link/src/dram.rs").is_file());

    let regs: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(hns.join("regs.json")).unwrap()).unwrap();
    let region = &regs["regions"][0];
    assert_eq!(region["total_bytes"], 256 << 20);
    let window = region["size_bytes"].as_u64().unwrap() as u32;
    let base_reg = regs["registers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "mem_base_jtag")
        .expect("no mem_base_jtag")["offset"]
        .as_u64()
        .unwrap() as u32;
    let last_page = (256 << 20) / window - 1;

    let (mut child, mut link) = start(dir.path());
    // The first word, and the last word of the last page.
    let mut batch = hns_host::Batch::new();
    batch.write(base_reg, 0);
    batch.write(0, 0x1111_1111);
    batch.write(base_reg, last_page);
    batch.write(window - 4, 0x2222_2222);
    let top = batch.read(window - 4);
    let bottom_seen_from_top = batch.read(0);
    batch.write(base_reg, 0);
    let bottom = batch.read(0);
    let results = link.run(&batch).unwrap();
    assert_eq!(results[top], 0x2222_2222);
    // Memory never written reads as 0, and the top did not land on the
    // bottom.
    assert_eq!(results[bottom_seen_from_top], 0);
    assert_eq!(results[bottom], 0x1111_1111);

    link.finish().unwrap();
    assert!(child.wait().unwrap().success());
}

/// The size is the sim target's, so a target patch changes it. 4 GB is the
/// first size whose byte count does not fit in 32 bits.
#[test]
fn a_target_patch_sizes_the_simulated_dram() {
    let dir = fixture(DRAM_DUT, DRAM_HARNESS_TOML);
    let patch = dir.path().join("sim-4g.toml");
    fs::write(&patch, "[provides.dram]\naxi_addr_bits = 32\n").unwrap();
    let out = harness(
        dir.path(),
        &[
            "gen",
            "--target",
            "sim",
            "--target-patch",
            patch.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let hns = dir.path().join("hns");
    let regs: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(hns.join("regs.json")).unwrap()).unwrap();
    assert_eq!(regs["regions"][0]["total_bytes"], 4u64 << 30);
    // The DUT's 28-bit address is widened to the memory's 32.
    let sim = fs::read_to_string(hns.join("src/sim.veryl")).unwrap();
    assert!(sim.contains("hns::axi_aw"), "{sim}");
    common::veryl_check(&hns).unwrap();
}

/// A DMA engine on a transfer-level port: it writes `nbytes` of `seed + i`
/// to `addr`, or reads `nbytes` from `addr` and sums them.
const XFER_DUT: &str = r#"
module dut_top (
    i_clk   : input  clock    ,
    i_rst   : input  reset    ,
    i_go_rd : input  logic    ,
    i_go_wr : input  logic    ,
    i_addr  : input  logic<32>,
    i_nbytes: input  logic<32>,
    i_seed  : input  logic<32>,
    o_busy  : output logic    ,
    o_sum   : output logic<32>,
    o_beats : output logic<32>,
    o_dones : output logic<32>,

    o_rc_valid: output logic    ,
    i_rc_ready: input  logic    ,
    o_rc_addr : output logic<32>,
    o_rc_size : output logic<32>,
    i_r_valid : input  logic    ,
    o_r_ready : output logic    ,
    i_r_data  : input  logic<32>,
    i_r_last  : input  logic    ,
    o_wc_valid: output logic    ,
    i_wc_ready: input  logic    ,
    o_wc_addr : output logic<32>,
    o_wc_size : output logic<32>,
    o_w_valid : output logic    ,
    i_w_ready : input  logic    ,
    o_w_data  : output logic<32>,
    o_w_last  : output logic    ,
    i_w_done  : input  logic    ,
) {
    enum state_t: logic<3> {
        IDLE,
        RCMD,
        RDATA,
        WCMD,
        WDATA,
        WDONE,
    }
    var state: state_t  ;
    var rd_q : logic    ;
    var wr_q : logic    ;
    var left : logic<32>;
    var n    : logic<32>;

    assign o_busy     = state != state_t::IDLE;
    assign o_rc_valid = state == state_t::RCMD;
    assign o_rc_addr  = i_addr;
    assign o_rc_size  = i_nbytes;
    assign o_r_ready  = state == state_t::RDATA;
    assign o_wc_valid = state == state_t::WCMD;
    assign o_wc_addr  = i_addr;
    assign o_wc_size  = i_nbytes;
    assign o_w_valid  = state == state_t::WDATA;
    assign o_w_data   = i_seed + n;
    assign o_w_last   = left == 1;

    always_ff {
        if_reset {
            state   = state_t::IDLE;
            rd_q    = 0;
            wr_q    = 0;
            left    = 0;
            n       = 0;
            o_sum   = 0;
            o_beats = 0;
            o_dones = 0;
        } else {
            rd_q = i_go_rd;
            wr_q = i_go_wr;
            case state {
                state_t::IDLE: if i_go_rd && !rd_q {
                    state   = state_t::RCMD;
                    o_sum   = 0;
                    o_beats = 0;
                } else if i_go_wr && !wr_q {
                    state = state_t::WCMD;
                    left  = i_nbytes >> 2;
                    n     = 0;
                }
                state_t::RCMD: if i_rc_ready {
                    state = state_t::RDATA;
                }
                state_t::RDATA: if i_r_valid {
                    o_sum   += i_r_data;
                    o_beats += 1;
                    if i_r_last {
                        state = state_t::IDLE;
                    }
                }
                state_t::WCMD: if i_wc_ready {
                    state = state_t::WDATA;
                }
                state_t::WDATA: if i_w_ready {
                    n    += 1;
                    left -= 1;
                    if left == 1 {
                        state = state_t::WDONE;
                    }
                }
                state_t::WDONE: if i_w_done {
                    state   = state_t::IDLE;
                    o_dones += 1;
                }
                default: state = state_t::IDLE;
            }
        }
    }
}
"#;

const XFER_HARNESS_TOML: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 100\n\n\
[bundle.ctl]\nbacking = \"reg\"\nports = [\"i_go_rd\", \"i_go_wr\", \"i_addr\", \"i_nbytes\", \"i_seed\", \"o_busy\", \"o_sum\", \"o_beats\", \"o_dones\"]\n\n\
[bundle.mem]\nbacking = \"dram\"\naperture = \"1M\"\n\
ports = { rd_cmd_valid = \"o_rc_valid\", rd_cmd_ready = \"i_rc_ready\", rd_cmd_addr = \"o_rc_addr\", rd_cmd_size = \"o_rc_size\", \
rd_valid = \"i_r_valid\", rd_ready = \"o_r_ready\", rd_data = \"i_r_data\", rd_last = \"i_r_last\", \
wr_cmd_valid = \"o_wc_valid\", wr_cmd_ready = \"i_wc_ready\", wr_cmd_addr = \"o_wc_addr\", wr_cmd_size = \"o_wc_size\", \
wr_valid = \"o_w_valid\", wr_ready = \"i_w_ready\", wr_data = \"o_w_data\", wr_last = \"o_w_last\", wr_done_valid = \"i_w_done\" }\n";

/// A DUT built for `bram`'s transfer level runs on `dram` unchanged:
/// `hns::xfer2axi` turns its commands into AXI4 bursts. 300 beats from byte
/// 4000 cross a 4 KB edge and are longer than one burst; the host sees the
/// data through the same memory, and a read off the end is served short and
/// counted.
#[test]
fn a_transfer_level_dma_engine_runs_on_dram() {
    let dir = fixture(XFER_DUT, XFER_HARNESS_TOML);
    let out = harness(dir.path(), &["gen", "--target", "sim"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let hns = dir.path().join("hns");
    let sim = fs::read_to_string(hns.join("src/sim.veryl")).unwrap();
    assert!(sim.contains("hns::xfer2axi"), "{sim}");
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
    let total = regs["regions"][0]["total_bytes"].as_u64().unwrap() as u32;
    let window = regs["regions"][0]["size_bytes"].as_u64().unwrap() as u32;

    let (mut child, mut link) = start(dir.path());
    let wait_idle = |link: &mut Link| {
        for _ in 0..1000 {
            if link.read32(offset("o_busy")).unwrap() == 0 {
                return;
            }
        }
        panic!("the DMA engine never finished");
    };

    // Write 300 beats of 0x100 + i from byte 4000.
    let mut batch = hns_host::Batch::new();
    batch.write(offset("mem_base_jtag"), 0);
    batch.write(offset("i_addr"), 4000);
    batch.write(offset("i_nbytes"), 1200);
    batch.write(offset("i_seed"), 0x100);
    batch.write(offset("i_go_wr"), 1);
    batch.write(offset("i_go_wr"), 0);
    link.run(&batch).unwrap();
    wait_idle(&mut link);
    assert_eq!(link.read32(offset("o_dones")).unwrap(), 1);
    // The host sees it in the same memory, across the 4 KB edge.
    assert_eq!(link.read32(4000).unwrap(), 0x100);
    assert_eq!(link.read32(4096).unwrap(), 0x100 + 24);
    assert_eq!(link.read32(4000 + 299 * 4).unwrap(), 0x100 + 299);

    // Read them back: 300 beats, 300 * 0x100 + (0 + .. + 299).
    let mut batch = hns_host::Batch::new();
    batch.write(offset("i_go_rd"), 1);
    batch.write(offset("i_go_rd"), 0);
    link.run(&batch).unwrap();
    wait_idle(&mut link);
    assert_eq!(link.read32(offset("o_beats")).unwrap(), 300);
    assert_eq!(
        link.read32(offset("o_sum")).unwrap(),
        300 * 0x100 + 299 * 300 / 2
    );
    assert_eq!(link.read32(offset("mem_oor")).unwrap(), 0);

    // Off the end: 4 beats asked, 2 inside.
    let mut batch = hns_host::Batch::new();
    batch.write(offset("mem_base_jtag"), total / window - 1);
    batch.write(window - 8, 7);
    batch.write(window - 4, 8);
    batch.write(offset("i_addr"), total - 8);
    batch.write(offset("i_nbytes"), 16);
    batch.write(offset("i_go_rd"), 1);
    batch.write(offset("i_go_rd"), 0);
    link.run(&batch).unwrap();
    wait_idle(&mut link);
    assert_eq!(link.read32(offset("o_beats")).unwrap(), 2);
    assert_eq!(link.read32(offset("o_sum")).unwrap(), 15);
    assert_eq!(link.read32(offset("mem_oor")).unwrap(), 1);

    link.finish().unwrap();
    assert!(child.wait().unwrap().success());
}

/// A project that also holds firmware names its CPU in its own cargo config.
/// Veryl builds the socket component inside the project, so that config would
/// apply to it too; the component's own config keeps it on the host.
#[test]
fn a_cpu_target_in_the_project_does_not_reach_the_component() {
    let dir = fixture(DUT, HARNESS_TOML);
    fs::create_dir(dir.path().join(".cargo")).unwrap();
    fs::write(
        dir.path().join(".cargo").join("config.toml"),
        "[build]\ntarget = \"riscv32im-unknown-none-elf\"\n",
    )
    .unwrap();
    let out = harness(dir.path(), &["gen", "--target", "sim"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let config = fs::read_to_string(dir.path().join("hns/link/.cargo/config.toml")).unwrap();
    assert!(config.contains("host-tuple"), "{config}");

    let (mut child, link) = start(dir.path());
    link.finish().unwrap();
    assert!(child.wait().unwrap().success());
}

//! Tests that run the generated harness in a simulator.
//!
//! Passing the analyzer checks only the shape, and running on a board needs
//! the board. Simulation lets `cargo test` catch behaviour bugs, such as one
//! write sending many beats.
//!
//! The tests run `sim`, the top without the transport and the clock
//! generation. `top` has the MMCM and the JTAG bridge primitives, which are
//! Vivado blackboxes. Everything from the AXI4-Lite window down comes from the
//! same `core_body`, so what passes here is the same logic as on the board.

use std::fs;
use std::path::Path;
use std::process::Command;

use clap::Parser;

/// The fixture's Veryl.toml. It needs the `hns` package, because the output
/// refers to `hns::axil`, `hns::fifo` and `hns::mem`.
fn veryl_toml() -> String {
    format!(
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[build]\nreset_type = \"async_low\"\n\n[dependencies]\nhns = {{ path = \"{}/rtl/hns\" }}\n",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Runs `veryl test` as a library.
///
/// This does not call the `veryl` on `PATH`, so the test cannot pick up a
/// version different from the linked analyzer.
fn veryl_test(dir: &Path) -> bool {
    #[derive(Parser)]
    struct Args {
        #[command(flatten)]
        opt: veryl::OptTest,
    }

    let args = Args::parse_from(["test"]);
    let mut metadata = veryl_metadata::Metadata::load(dir.join("Veryl.toml")).unwrap();
    veryl::cmd_test::CmdTest::new(args.opt)
        .exec(&mut metadata)
        .expect("the simulator must run")
}

fn fixture(harness_toml: &str, dut: &str, tb: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Veryl.toml"), veryl_toml()).unwrap();
    fs::write(dir.path().join("Harness.toml"), harness_toml).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src").join("dut_top.veryl"), dut).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_veryl-harness"))
        .args(["gen", "--target", "digilent/arty-a7-35"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "gen failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The test bench is not generated, so it is written after gen. If `tb` is
    // empty, the caller writes it later, after reading the map offsets.
    //
    // It goes into the generated project, where `sim` is. The DUT project
    // cannot see `sim`.
    if !tb.is_empty() {
        write_tb(dir.path(), tb);
    }
    dir
}

/// Writes a test bench into the generated project.
fn write_tb(root: &Path, tb: impl AsRef<str>) {
    fs::write(root.join("hns").join("src").join("tb.veryl"), tb.as_ref()).unwrap();
}

/// Procedures that access the AXI4-Lite window, shared by every test bench.
const BUS: &str = r#"
    inst clk: $tb::clock_gen;
    inst rst: $tb::reset_gen(clk);

    var awaddr : logic<32>;
    var awvalid: logic    ;
    var awready: logic    ;
    var wdata  : logic<32>;
    var wstrb  : logic<4> ;
    var wvalid : logic    ;
    var wready : logic    ;
    var bresp  : logic<2> ;
    var bvalid : logic    ;
    var bready : logic    ;
    var araddr : logic<32>;
    var arvalid: logic    ;
    var arready: logic    ;
    var rdata  : logic<32>;
    var rresp  : logic<2> ;
    var rvalid : logic    ;
    var rready : logic    ;

    inst u: sim (
        i_clk    : clk    ,
        i_rst    : rst    ,
        i_awaddr : awaddr ,
        i_awvalid: awvalid,
        o_awready: awready,
        i_wdata  : wdata  ,
        i_wstrb  : wstrb  ,
        i_wvalid : wvalid ,
        o_wready : wready ,
        o_bresp  : bresp  ,
        o_bvalid : bvalid ,
        i_bready : bready ,
        i_araddr : araddr ,
        i_arvalid: arvalid,
        o_arready: arready,
        o_rdata  : rdata  ,
        o_rresp  : rresp  ,
        o_rvalid : rvalid ,
        i_rready : rready ,
    );

    // Wait for the response; do not count a fixed number of cycles. Registers
    // and BRAM regions answer in one cycle, but `dram` goes through AXI4 and
    // arbitration and takes longer. A fixed count returns the value of the
    // previous read, which looks plausible.
    function wr (
        addr: input logic<32>,
        data: input logic<32>,
    ) {
        awaddr  = addr;
        wdata   = data;
        wstrb   = 4'hf;
        awvalid = 1;
        wvalid  = 1;
        for _i in 0..64 {
            if awready {
                break;
            }
            clk.next(1);
        }
        clk.next(1);
        awvalid = 0;
        wvalid  = 0;
        for _i in 0..64 {
            if bvalid {
                break;
            }
            clk.next(1);
        }
        clk.next(1);
    }

    function rd (
        addr: input logic<32>,
    ) -> logic<32> {
        araddr  = addr;
        arvalid = 1;
        for _i in 0..64 {
            if arready {
                break;
            }
            clk.next(1);
        }
        clk.next(1);
        arvalid = 0;
        for _i in 0..64 {
            if rvalid {
                break;
            }
            clk.next(1);
        }
        return rdata;
    }

    function start () {
        awvalid = 0;
        wvalid  = 0;
        arvalid = 0;
        bready  = 1;
        rready  = 1;
        wstrb   = 4'hf;
        rst.assert(4);
        clk.next(4);
    }
"#;

/// A DUT with an output stream. It counts only when `i_run && i_ready`, so the
/// FIFO always holds 0, 1, 2, ... in order, and the expected values are known.
const STREAM_DUT: &str = r#"
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
"#;

const STREAM_MANIFEST: &str = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.tx]\ncontract = \"valid_ready\"\nbacking = \"host_poll_fifo\"\ndepth = 16\nports = { valid = \"o_valid\", ready = \"i_ready\", data = \"o_data\" }\n";

/// The `host_poll_fifo` rules hold in simulation.
#[test]
fn a_host_poll_fifo_pops_one_entry_per_write() {
    let dir = fixture(STREAM_MANIFEST, STREAM_DUT, "");
    let map = offsets(dir.path());
    let (magic, run, data, level, depth, pop) = (
        offset_of(&map, "harness_magic"),
        offset_of(&map, "i_run"),
        offset_of(&map, "tx_data"),
        offset_of(&map, "tx_level"),
        offset_of(&map, "tx_depth"),
        offset_of(&map, "tx_pop"),
    );
    let tb = format!(
        r#"
#[test(host_poll_fifo_window)]
module host_poll_fifo_window {{
{BUS}
    initial {{
        start();

        // Identity header.
        $assert(rd({magic}) == 32'h5648524e);

        // The depth is 16 as in the manifest. The host can read it, so it can
        // also tell whether the default was used.
        $assert(rd({depth}) == 16);
        $assert(rd({level}) == 0);

        // Run the DUT. When the FIFO is full, ready goes low and the DUT stops.
        wr({run}, 1);
        clk.next(40);
        $assert(rd({level}) == 16);

        // A read does not pop. Two reads give the same entry.
        $assert(rd({data}) == 0);
        $assert(rd({data}) == 0);
        $assert(rd({level}) == 16);

        // One pop write advances one entry. The order is 0, 1, 2, ...
        wr({pop}, 1);
        $assert(rd({data}) == 1);
        wr({pop}, 1);
        $assert(rd({data}) == 2);

        // Stop the DUT and drain: the level falls to 0.
        wr({run}, 0);
        clk.next(4);
        for i in 0..16 {{
            wr({pop}, 1);
        }}
        $assert(rd({level}) == 0);

        // An access outside the window returns SLVERR, not silence.
        var out_of_window: logic<32>;
        out_of_window = rd(32'h100);
        $assert(rresp == 2'b10);

        $finish();
    }}
}}
"#
    );

    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// `valid_only` counts what it drops.
///
/// The DUT has no `i_ready`, so it pushes every cycle. The 16-entry FIFO fills
/// quickly, and after that the drop counter increases.
#[test]
fn a_valid_only_stream_counts_what_it_drops() {
    let dut = r#"
module dut_top (
    i_clk  : input  clock   ,
    i_rst  : input  reset   ,
    i_run  : input  logic   ,
    o_valid: output logic   ,
    o_data : output logic<8>,
) {
    var count: logic<8>;
    always_ff {
        if_reset {
            count = 0;
        } else if i_run {
            count = count + 1;
        }
    }
    assign o_valid = i_run;
    assign o_data  = count;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.tx]\ncontract = \"valid_only\"\nbacking = \"host_poll_fifo\"\ndepth = 16\nports = { valid = \"o_valid\", data = \"o_data\" }\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (run, data, level, drops, pop) = (
        offset_of(&map, "i_run"),
        offset_of(&map, "tx_data"),
        offset_of(&map, "tx_level"),
        offset_of(&map, "tx_drops"),
        offset_of(&map, "tx_pop"),
    );
    let tb = format!(
        r#"
#[test(valid_only_drops)]
module valid_only_drops {{
{BUS}
    initial {{
        start();

        // Nothing is dropped before the run.
        $assert(rd({drops}) == 0);

        // Push for 32 cycles into 16 entries. The DUT cannot be stopped, so it overflows.
        wr({run}, 1);
        clk.next(32);
        wr({run}, 0);
        clk.next(4);

        $assert(rd({level}) == 16);
        $assert(rd({drops}) >: 0);

        // The order is kept after overflow: new entries are dropped, and
        // entries already stored are not pushed out.
        $assert(rd({data}) == 0);
        wr({pop}, 1);
        $assert(rd({data}) == 1);

        $finish();
    }}
}}
"#
    );

    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// Reads offsets from `regs.json`, so test benches do not hard-code addresses.
fn offsets(dir: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(dir.join("hns/regs.json")).unwrap()).unwrap()
}

fn region_base(map: &serde_json::Value, name: &str) -> u64 {
    map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|region| region["name"] == name)
        .unwrap_or_else(|| panic!("{name} is not a region"))["base"]
        .as_u64()
        .unwrap()
}

fn offset_of(map: &serde_json::Value, name: &str) -> u64 {
    map["registers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|register| register["name"] == name)
        .unwrap_or_else(|| panic!("{name} is missing from the map"))["offset"]
        .as_u64()
        .unwrap()
}

/// `bram_preload`: the host loads it, and the DUT reads it.
#[test]
fn a_preloaded_memory_is_read_by_the_dut() {
    let dut = r#"
module dut_top (
    i_clk       : input  clock    ,
    i_rst       : input  reset    ,
    i_run       : input  logic    ,
    o_imem_addr : output logic<8> ,
    i_imem_rdata: input  logic<32>,
    o_last      : output logic<32>,
) {
    var addr: logic<8>;
    always_ff {
        if_reset {
            addr   = 0;
            o_last = 0;
        } else if i_run {
            addr   = addr + 1;
            o_last = i_imem_rdata;
        }
    }
    assign o_imem_addr = addr;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.last]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"o_last\"]\n\n[bundle.imem]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram_preload\"\naddressing = \"word\"\naccess = \"indirect\"\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (run, last, maddr, mdata, depth) = (
        offset_of(&map, "i_run"),
        offset_of(&map, "o_last"),
        offset_of(&map, "imem_maddr"),
        offset_of(&map, "imem_mdata"),
        offset_of(&map, "imem_depth"),
    );

    let tb = format!(
        r#"
#[test(preloaded_memory)]
module preloaded_memory {{
{BUS}
    initial {{
        start();

        // The depth comes from the address width (2^8), and it can be read.
        $assert(rd({depth}) == 256);

        // Not running yet.
        $assert(rd({last}) == 0);

        // Load. The address advances on each write, so one word takes one
        // transaction.
        wr({maddr}, 0);
        for i in 0..8 {{
            wr({mdata}, 32'ha5a5_0000);
        }}
        // It advanced by the 8 writes.
        $assert(rd({maddr}) == 8);

        // Read back does not advance. Set the address, then read.
        wr({maddr}, 3);
        $assert(rd({mdata}) == 32'ha5a5_0000);
        $assert(rd({maddr}) == 3);

        // Let the DUT read. Stop before it leaves the loaded range.
        wr({run}, 1);
        clk.next(4);
        wr({run}, 0);
        $assert(rd({last}) == 32'ha5a5_0000);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);

    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// Loads entries wider than 32 bits through the window.
///
/// The window is 32 bits, so a 64-bit entry takes two writes. The entry is
/// committed when the top word is written, and only then the address advances
/// by one. It must not advance on each word.
#[test]
fn a_memory_wider_than_the_window_commits_one_entry_at_a_time() {
    let dut = r#"
module dut_top (
    i_clk       : input  clock    ,
    i_rst       : input  reset    ,
    i_run       : input  logic    ,
    o_imem_addr : output logic<8> ,
    i_imem_rdata: input  logic<64>,
    o_last      : output logic<64>,
) {
    var addr: logic<8>;
    always_ff {
        if_reset {
            addr   = 0;
            o_last = 0;
        } else if i_run {
            addr   = addr + 1;
            o_last = i_imem_rdata;
        }
    }
    assign o_imem_addr = addr;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.last]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"o_last\"]\n\n[bundle.imem]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram_preload\"\naddressing = \"word\"\naccess = \"indirect\"\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (run, last, maddr, mdata) = (
        offset_of(&map, "i_run"),
        offset_of(&map, "o_last"),
        offset_of(&map, "imem_maddr"),
        offset_of(&map, "imem_mdata"),
    );
    // A 64-bit entry takes two words.
    let mdata_hi = mdata + 4;
    let last_hi = last + 4;

    let tb = format!(
        r#"
#[test(wide_memory_window)]
module wide_memory_window {{
{BUS}
    initial {{
        start();

        // First entry: low word, then high word. Writing the high word commits it.
        wr({maddr}, 0);
        wr({mdata}, 32'h1111_2222);
        $assert(rd({maddr}) == 0);          // not advanced yet
        wr({mdata_hi}, 32'h3333_4444);
        $assert(rd({maddr}) == 1);          // advanced by one entry

        // Second entry.
        wr({mdata}, 32'haaaa_bbbb);
        wr({mdata_hi}, 32'hcccc_dddd);
        $assert(rd({maddr}) == 2);

        // Read back. Address 0 has the first entry, address 1 the second.
        wr({maddr}, 0);
        $assert(rd({mdata}) == 32'h1111_2222);
        $assert(rd({mdata_hi}) == 32'h3333_4444);
        wr({maddr}, 1);
        $assert(rd({mdata}) == 32'haaaa_bbbb);
        $assert(rd({mdata_hi}) == 32'hcccc_dddd);
        $assert(rd({maddr}) == 1);          // a read does not advance

        // Let the DUT read. The address advances while it runs (the `i_run`
        // write itself takes a few cycles), so fill the range with one value.
        wr({maddr}, 0);
        for i in 0..16 {{
            wr({mdata}, 32'h1111_2222);
            wr({mdata_hi}, 32'h3333_4444);
        }}
        wr({run}, 1);
        clk.next(2);
        wr({run}, 0);
        $assert(rd({last}) == 32'h1111_2222);
        $assert(rd({last_hi}) == 32'h3333_4444);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);

    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// `bram`: the host reads what the DUT wrote (the memory is dual-port).
#[test]
fn a_memory_the_dut_writes_is_read_back_by_the_host() {
    let dut = r#"
module dut_top (
    i_clk      : input  clock    ,
    i_rst      : input  reset    ,
    i_run      : input  logic    ,
    o_ram_addr : output logic<8> ,
    i_ram_rdata: input  logic<32>,
    o_ram_wdata: output logic<32>,
    o_ram_we   : output logic    ,
    o_seen     : output logic<32>,
) {
    var addr: logic<8>;
    always_ff {
        if_reset {
            addr   = 0;
            o_seen = 0;
        } else if i_run {
            addr   = addr + 1;
            o_seen = i_ram_rdata;
        }
    }
    assign o_ram_addr  = addr;
    assign o_ram_wdata = {{24'b0, addr}};
    assign o_ram_we    = i_run;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.seen]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"o_seen\"]\n\n[bundle.ram]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram\"\naddressing = \"word\"\naccess = \"indirect\"\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (run, maddr, mdata) = (
        offset_of(&map, "i_run"),
        offset_of(&map, "ram_maddr"),
        offset_of(&map, "ram_mdata"),
    );

    let tb = format!(
        r#"
#[test(dut_writes_memory)]
module dut_writes_memory {{
{BUS}
    initial {{
        start();

        // Let the DUT write a little. Address k gets k.
        wr({run}, 1);
        clk.next(8);
        wr({run}, 0);
        clk.next(2);

        // Read back from the host, on the other port of the same memory.
        wr({maddr}, 2);
        $assert(rd({mdata}) == 2);
        wr({maddr}, 5);
        $assert(rd({mdata}) == 5);

        // A host write can be read back at the same address.
        wr({maddr}, 100);
        wr({mdata}, 32'h1234);
        wr({maddr}, 100);
        $assert(rd({mdata}) == 32'h1234);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);

    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// Byte addresses and out-of-range accesses.
///
/// The DUT gives byte addresses, and an entry is 32 bits (4 bytes), so 4
/// addresses make one entry. Beyond the depth, the read returns 0 and `oor`
/// counts it. The address must not wrap around to another entry.
#[test]
fn a_byte_addressed_memory_counts_what_falls_outside() {
    let dut = r#"
module dut_top (
    i_clk       : input  clock    ,
    i_rst       : input  reset    ,
    i_addr      : input  logic<16>,
    o_imem_addr : output logic<16>,
    i_imem_rdata: input  logic<32>,
    o_last      : output logic<32>,
) {
    always_ff {
        if_reset {
            o_last = 0;
        } else {
            o_last = i_imem_rdata;
        }
    }
    assign o_imem_addr = i_addr;
}
"#;
    // The host sets the DUT address directly, so it can make an out-of-range access.
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.addr]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_addr\"]\n\n[bundle.last]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"o_last\"]\n\n[bundle.imem]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram_preload\"\naddressing = \"byte\"\ndepth = 16\nports = { addr = \"o_imem_addr\", rdata = \"i_imem_rdata\" }\naccess = \"indirect\"\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (addr, last, maddr, mdata, oor) = (
        offset_of(&map, "i_addr"),
        offset_of(&map, "o_last"),
        offset_of(&map, "imem_maddr"),
        offset_of(&map, "imem_mdata"),
        offset_of(&map, "imem_oor"),
    );

    let tb = format!(
        r#"
#[test(byte_addressed_memory)]
module byte_addressed_memory {{
{BUS}
    initial {{
        start();

        // Load 16 entries. Entry k gets k.
        wr({maddr}, 0);
        for i in 0..16 {{
            wr({mdata}, i);
        }}

        // 4 bytes per entry. Address 8 is entry 2.
        wr({addr}, 8);
        clk.next(4);
        $assert(rd({last}) == 2);

        // An offset inside an entry does not change the entry.
        wr({addr}, 11);
        clk.next(4);
        $assert(rd({last}) == 2);

        wr({addr}, 12);
        clk.next(4);
        $assert(rd({last}) == 3);

        // No out-of-range access so far.
        $assert(rd({oor}) == 0);

        // 16 entries = 64 bytes. Address 64 is out of range and must not wrap to entry 0.
        wr({addr}, 64);
        clk.next(4);
        $assert(rd({last}) == 0);
        $assert(rd({oor}) >: 0);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);

    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// Writes are whole lines, and reads are words.
///
/// A cache drain writes a 64-byte line at once, and later loads read it as 8
/// beats of 64 bits. One entry is one line, and a read selects the word in the
/// entry by the middle address bits. Bytes not enabled by the byte strobe do
/// not change.
#[test]
fn a_line_write_and_word_reads_share_one_memory() {
    let dut = r#"
module dut_top (
    i_clk       : input  clock     ,
    i_rst       : input  reset     ,
    i_addr      : input  logic<16> ,
    i_wen       : input  logic     ,
    i_wdata     : input  logic<128>,
    i_wstrb     : input  logic<16> ,
    o_dmem_addr : output logic<16> ,
    i_dmem_rdata: input  logic<32> ,
    o_dmem_wdata: output logic<128>,
    o_dmem_wstrb: output logic<16> ,
    o_dmem_wen  : output logic     ,
    o_last      : output logic<32> ,
) {
    always_ff {
        if_reset {
            o_last = 0;
        } else {
            o_last = i_dmem_rdata;
        }
    }
    assign o_dmem_addr  = i_addr;
    assign o_dmem_wdata = i_wdata;
    assign o_dmem_wstrb = i_wstrb;
    assign o_dmem_wen   = i_wen;
}
"#;
    // Entry = 128 bits (16 bytes), read = 32 bits. Four words per entry.
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.ctl]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_addr\", \"i_wen\", \"i_wdata\", \"i_wstrb\", \"o_last\"]\n\n[bundle.dmem]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram\"\naddressing = \"byte\"\ndepth = 16\nports = { addr = \"o_dmem_addr\", rdata = \"i_dmem_rdata\", wdata = \"o_dmem_wdata\", wstrb = \"o_dmem_wstrb\", we = \"o_dmem_wen\" }\naccess = \"indirect\"\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (addr, wen, wdata, wstrb, last) = (
        offset_of(&map, "i_addr"),
        offset_of(&map, "i_wen"),
        offset_of(&map, "i_wdata"),
        offset_of(&map, "i_wstrb"),
        offset_of(&map, "o_last"),
    );

    let tb = format!(
        r#"
#[test(line_write_word_read)]
module line_write_word_read {{
{BUS}
    initial {{
        start();

        // Write one line (4 words = 16 bytes) at once. i_wdata spans 4 window words.
        wr({wdata}, 32'h1111_1111);
        wr({wdata} + 4, 32'h2222_2222);
        wr({wdata} + 8, 32'h3333_3333);
        wr({wdata} + 12, 32'h4444_4444);
        wr({wstrb}, 32'h0000_ffff);   // all 16 bytes enabled
        wr({addr}, 0);
        wr({wen}, 1);
        clk.next(2);
        wr({wen}, 0);

        // Read inside the same entry, selected by the middle address bits.
        wr({addr}, 0);
        clk.next(4);
        $assert(rd({last}) == 32'h1111_1111);
        wr({addr}, 4);
        clk.next(4);
        $assert(rd({last}) == 32'h2222_2222);
        wr({addr}, 12);
        clk.next(4);
        $assert(rd({last}) == 32'h4444_4444);

        // Bytes not enabled by the strobe do not change.
        wr({wdata}, 32'haaaa_aaaa);
        wr({wdata} + 4, 32'hbbbb_bbbb);
        wr({wstrb}, 32'h0000_000f);   // only the first 4 bytes
        wr({addr}, 0);
        wr({wen}, 1);
        clk.next(2);
        wr({wen}, 0);

        wr({addr}, 0);
        clk.next(4);
        $assert(rd({last}) == 32'haaaa_aaaa);   // written
        wr({addr}, 4);
        clk.next(4);
        $assert(rd({last}) == 32'h2222_2222);   // not written, so unchanged

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);

    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// Read enable. The memory reads only in cycles where `en` is high, and holds
/// the data otherwise. Out-of-range accesses are counted per access, not per
/// cycle.
#[test]
fn a_read_enable_holds_the_data_and_counts_accesses() {
    let dut = r#"
module dut_top (
    i_clk       : input  clock    ,
    i_rst       : input  reset    ,
    i_addr      : input  logic<16>,
    i_en        : input  logic    ,
    o_imem_addr : output logic<16>,
    o_imem_en   : output logic    ,
    i_imem_rdata: input  logic<32>,
    o_last      : output logic<32>,
) {
    always_ff {
        if_reset {
            o_last = 0;
        } else {
            o_last = i_imem_rdata;
        }
    }
    assign o_imem_addr = i_addr;
    assign o_imem_en   = i_en;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.ctl]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_addr\", \"i_en\", \"o_last\"]\n\n[bundle.imem]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram_preload\"\naddressing = \"word\"\ndepth = 8\nports = { addr = \"o_imem_addr\", rdata = \"i_imem_rdata\", re = \"o_imem_en\" }\naccess = \"indirect\"\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (addr, en, last, maddr, mdata, oor) = (
        offset_of(&map, "i_addr"),
        offset_of(&map, "i_en"),
        offset_of(&map, "o_last"),
        offset_of(&map, "imem_maddr"),
        offset_of(&map, "imem_mdata"),
        offset_of(&map, "imem_oor"),
    );

    let tb = format!(
        r#"
#[test(read_enable)]
module read_enable {{
{BUS}
    initial {{
        start();

        wr({maddr}, 0);
        for i in 0..8 {{
            wr({mdata}, 32'hcafe_0000 + i);
        }}

        // Set enable and read entry 1.
        wr({addr}, 1);
        wr({en}, 1);
        clk.next(4);
        $assert(rd({last}) == 32'hcafe_0001);

        // With enable low, the data is held even if the address changes.
        wr({en}, 0);
        wr({addr}, 3);
        clk.next(8);
        $assert(rd({last}) == 32'hcafe_0001);

        // Set enable again, and the new address is read.
        wr({en}, 1);
        clk.next(4);
        $assert(rd({last}) == 32'hcafe_0003);

        // No out-of-range access so far.
        wr({en}, 0);
        $assert(rd({oor}) == 0);

        // Out-of-range is counted only when enable is high.
        wr({addr}, 100);
        clk.next(8);
        $assert(rd({oor}) == 0);      // enable is low, so not counted
        wr({en}, 1);
        clk.next(4);
        wr({en}, 0);
        $assert(rd({oor}) >: 0);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);

    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// One write is exactly one beat.
///
/// Without the auto-clear, the DUT keeps capturing from the moment the host
/// writes 1. This was seen on real hardware.
#[test]
fn a_host_driven_valid_sends_exactly_one_beat() {
    let dut = r#"
module dut_top (
    i_clk : input  clock   ,
    i_rst : input  reset   ,
    i_push: input  logic   ,
    i_data: input  logic<8>,
    o_full: output logic   ,
    o_data: output logic<8>,
) {
    var count: logic<4>;
    always_ff {
        if_reset {
            o_data = 0;
            count  = 0;
        } else if i_push {
            o_data = i_data;
            count  = count + 1;
        }
    }
    assign o_full = count == 15;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.push]\ncontract = \"valid_ready\"\nbacking = \"reg\"\nports = { valid = \"i_push\", ready = \"!o_full\", data = \"i_data\" }\n\n[bundle.result]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\nports = [\"o_data\"]\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (magic, push, data, result) = (
        offset_of(&map, "harness_magic"),
        offset_of(&map, "i_push"),
        offset_of(&map, "i_data"),
        offset_of(&map, "o_data"),
    );
    let tb = format!(
        r#"
#[test(one_write_one_beat)]
module one_write_one_beat {{
{BUS}
    initial {{
        start();

        $assert(rd({magic}) == 32'h5648524e);

        // No push yet, so the DUT has captured nothing.
        wr({data}, 32'h5a);
        $assert(rd({result}) == 0);

        // One write sends exactly one beat.
        wr({push}, 1);
        clk.next(4);
        $assert(rd({result}) == 32'h5a);

        // The CSR cleared push itself when the transfer happened. If it stayed
        // 1, one JTAG write would send hundreds of thousands of beats.
        $assert(rd({push}) == 0);

        // Push is low, so writing the next value does not push it.
        wr(12, 32'ha5);
        clk.next(4);
        $assert(rd({result}) == 32'h5a);

        $finish();
    }}
}}
"#
    );

    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// The heartbeat UART puts the right bytes on the wire.
///
/// There is no decoder in the test bench. With start-bit synchronization in
/// the bench, a bench error and an RTL bug look the same. Instead, Rust builds
/// the expected bit sequence and the bench compares it cycle by cycle.
#[test]
fn the_heartbeat_puts_the_identity_line_on_the_wire() {
    // 200 MHz / 25 Mbaud = 8 clocks per bit, to keep the test short.
    const DIV: usize = 8;
    let manifest = format!("{STREAM_MANIFEST}\n[heartbeat]\npin = \"uart_tx\"\nbaud = 25000000\n");
    let dir = fixture(&manifest, STREAM_DUT, "");

    let regs: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("hns/regs.json")).unwrap())
            .unwrap();
    let magic = regs["magic"].as_u64().unwrap() as u32;
    let hash = regs["map_hash"].as_u64().unwrap() as u32;
    let line = format!("hns {magic:08x} {hash:08x} dut_top\r\n");

    // 8N1: start (0), 8 data bits LSB first, stop (1). The first two bytes
    // cover both the framing and the content.
    let mut bits: Vec<char> = Vec::new();
    for byte in line.bytes().take(2) {
        bits.extend(std::iter::repeat_n('0', DIV));
        for i in 0..8 {
            let bit = if (byte >> i) & 1 == 1 { '1' } else { '0' };
            bits.extend(std::iter::repeat_n(bit, DIV));
        }
        bits.extend(std::iter::repeat_n('1', DIV));
    }

    // Align to the first rising edge, so the test does not depend on how many
    // cycles `start()` takes. The line always starts with 'h' (0x68), whose
    // bits 0..2 are 0, so the first 1 is at cycle 32 of the frame (bit 3).
    let align = DIV * 4;
    assert_eq!(line.as_bytes()[0], b'h');
    let checks: String = bits
        .iter()
        .skip(align)
        .map(|bit| format!("        $assert(tx == 1'b{bit});\n        clk.next(1);\n"))
        .collect();

    let bus = BUS.replace(
        "        i_rready : rready ,\n    );",
        "        i_rready : rready ,\n        o_uart_tx: tx     ,\n    );",
    );
    let tb = format!(
        r#"
#[test(heartbeat)]
module test_heartbeat {{
    var tx: logic;
{bus}
    initial {{
        start();
        // Advance to the first 1 (bit 3 of the leading 'h').
        for i in 0..1000 {{
            if tx == 1'b1 {{
                break;
            }}
            clk.next(1);
        }}
{checks}    }}
}}
"#
    );
    write_tb(dir.path(), tb);

    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// A DMA reads the `host_mem` stand-in through the window.
///
/// The host puts a pattern in the BRAM, runs the DMA, and checks the sum. The
/// sum matters: a beat count alone still matches when the latency is off by one.
#[test]
fn a_dma_reads_the_stand_in_for_host_memory() {
    let dut = r#"
module dut_top (
    i_clk       : input  clock    ,
    i_rst       : input  reset    ,
    i_start     : input  logic    ,
    i_addr      : input  logic<64>,
    i_nbytes    : input  logic<32>,
    o_busy      : output logic    ,
    o_sum       : output logic<64>,
    o_beats     : output logic<32>,
    o_cmd_valid : output logic    ,
    i_cmd_ready : input  logic    ,
    o_cmd_addr  : output logic<64>,
    o_cmd_nbytes: output logic<32>,
    i_rd_valid  : input  logic    ,
    o_rd_ready  : output logic    ,
    i_rd_data   : input  logic<64>,
    i_rd_last   : input  logic    ,
) {
    const S_IDLE: logic<2> = 2'd0;
    const S_CMD : logic<2> = 2'd1;
    const S_DATA: logic<2> = 2'd2;

    var state: logic<2> ;
    var addr : logic<64>;
    var nb   : logic<32>;
    var sum  : logic<64>;
    var beats: logic<32>;

    always_comb {
        o_cmd_valid  = state == S_CMD;
        o_cmd_addr   = addr;
        o_cmd_nbytes = nb;
        o_rd_ready   = state == S_DATA;
        o_busy       = state != S_IDLE;
        o_sum        = sum;
        o_beats      = beats;
    }

    always_ff {
        if_reset {
            state = S_IDLE;
            addr  = 0;
            nb    = 0;
            sum   = 0;
            beats = 0;
        } else {
            case state {
                S_IDLE: {
                    if i_start && i_nbytes != 0 {
                        addr  = i_addr;
                        nb    = i_nbytes;
                        sum   = 0;
                        beats = 0;
                        state = S_CMD;
                    }
                }
                S_CMD: {
                    if i_cmd_ready {
                        state = S_DATA;
                    }
                }
                S_DATA: {
                    if i_rd_valid {
                        sum   = sum + i_rd_data;
                        beats = beats + 1;
                        if i_rd_last {
                            state = S_IDLE;
                        }
                    }
                }
                default: {
                    state = S_IDLE;
                }
            }
        }
    }
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n\
         [bundle.ctl]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\n\
         ports = [\"i_start\", \"i_addr\", \"i_nbytes\", \"o_busy\", \"o_sum\", \"o_beats\"]\n\n\
         [bundle.hmem]\nbacking = \"bram\"\ndepth = 8\n\
         ports = { rd_cmd_valid = \"o_cmd_valid\", rd_cmd_ready = \"i_cmd_ready\", \
         rd_cmd_addr = \"o_cmd_addr\", rd_cmd_size = \"o_cmd_nbytes\", \
         rd_valid = \"i_rd_valid\", rd_ready = \"o_rd_ready\", \
         rd_data = \"i_rd_data\", rd_last = \"i_rd_last\" }\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (start, addr, nbytes, busy, sum, beats, maddr, mdata, depth, oor, delay, jitter) = (
        offset_of(&map, "i_start"),
        offset_of(&map, "i_addr"),
        offset_of(&map, "i_nbytes"),
        offset_of(&map, "o_busy"),
        offset_of(&map, "o_sum"),
        offset_of(&map, "o_beats"),
        offset_of(&map, "hmem_maddr"),
        offset_of(&map, "hmem_mdata"),
        offset_of(&map, "hmem_depth"),
        offset_of(&map, "hmem_oor"),
        offset_of(&map, "hmem_delay"),
        offset_of(&map, "hmem_jitter"),
    );

    let tb = format!(
        r#"
#[test(dma_over_the_window)]
module dma_over_the_window {{
{BUS}
    initial {{
        start();

        $assert(rd({depth}) == 8);
        $assert(rd({oor}) == 0);

        // Put a pattern. An entry is 64 bits: writing low then high commits
        // it and advances the address. Entry i gets i+1.
        wr({maddr}, 0);
        for i in 0..8 {{
            wr({mdata}, i + 1);
            wr({mdata} + 4, 0);
        }}
        // The address wraps at the depth. For 8 entries the address register
        // is 3 bits, so after 8 writes it is 0 again.
        $assert(rd({maddr}) == 0);

        // Read 4 entries (32 bytes) from the start. 1+2+3+4 = 10.
        wr({addr}, 0);
        wr({addr} + 4, 0);
        wr({nbytes}, 32);
        wr({start}, 1);
        wr({start}, 0);
        for _i in 0..200 {{
            if rd({busy}) == 0 {{
                break;
            }}
        }}
        $assert(rd({beats}) == 4);
        // Check the value too. A count alone matches even if one beat is off.
        $assert(rd({sum}) == 10);
        $assert(rd({sum} + 4) == 0);
        // All requests were in range, so the out-of-range count stays 0.
        $assert(rd({oor}) == 0);

        // Run the same transfer again with added stalls. The stand-in is fast
        // and regular, so without this, stalls would never be tested.
        wr({delay}, 12);
        wr({jitter}, 1);
        wr({start}, 1);
        wr({start}, 0);
        for _i in 0..400 {{
            if rd({busy}) == 0 {{
                break;
            }}
        }}
        // The result is the same.
        $assert(rd({beats}) == 4);
        $assert(rd({sum}) == 10);
        $assert(rd({oor}) == 0);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);

    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// Checks the write side through the backdoor.
///
/// The DUT writes 3 beats to the host memory stand-in, and the host reads them
/// back through the backdoor. Without the backdoor, there is no way to see
/// what the DUT wrote.
#[test]
fn a_dma_write_is_read_back_through_the_backdoor() {
    let dut = r#"
module dut_top (
    i_clk          : input  clock    ,
    i_rst          : input  reset    ,
    i_start        : input  logic    ,
    i_addr         : input  logic<64>,
    i_nbytes       : input  logic<32>,
    o_busy         : output logic    ,
    o_dones        : output logic<32>,
    o_cmd_valid    : output logic    ,
    i_cmd_ready    : input  logic    ,
    o_cmd_addr     : output logic<64>,
    o_cmd_nbytes   : output logic<32>,
    o_wr_valid     : output logic    ,
    i_wr_ready     : input  logic    ,
    o_wr_data      : output logic<64>,
    o_wr_strb      : output logic<8> ,
    o_wr_last      : output logic    ,
    i_wr_done_valid: input  logic    ,
) {
    const S_IDLE: logic<2> = 2'd0;
    const S_CMD : logic<2> = 2'd1;
    const S_DATA: logic<2> = 2'd2;

    var state: logic<2> ;
    var left : logic<32>;
    var n    : logic<32>;
    var bytes: logic<32>;
    var dones: logic<32>;

    always_comb {
        o_cmd_valid = state == S_CMD;
        o_cmd_addr  = i_addr;
        // **The port counts bytes, not beats.** Handing it the beat count asks
        // for one beat and the transfer ends after one.
        o_cmd_nbytes = bytes;
        o_wr_valid   = state == S_DATA;
        // Beat i writes 0xa50 + i, so each position has its own value.
        o_wr_data = {52'b0, 12'ha50} + {32'b0, n - left};
        // Beat 0 writes all bytes; later beats write only the low 4 bytes. If
        // the high bytes stay, the strobe works.
        o_wr_strb = if left == n ? 8'hff : 8'h0f;
        o_wr_last = left == 1;
        o_busy    = state != S_IDLE;
        o_dones   = dones;
    }

    always_ff {
        if_reset {
            state = S_IDLE;
            left  = 0;
            n     = 0;
            bytes = 0;
            dones = 0;
        } else {
            if i_wr_done_valid {
                dones = dones + 1;
            }
            case state {
                S_IDLE: {
                    if i_start && i_nbytes != 0 {
                        bytes = i_nbytes;
                        n     = i_nbytes >> 3;
                        left  = i_nbytes >> 3;
                        state = S_CMD;
                    }
                }
                S_CMD: {
                    if i_cmd_ready {
                        state = S_DATA;
                    }
                }
                S_DATA: {
                    if i_wr_ready {
                        left = left - 1;
                        if left == 1 {
                            state = S_IDLE;
                        }
                    }
                }
                default: {
                    state = S_IDLE;
                }
            }
        }
    }
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n\
         [bundle.ctl]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\n\
         ports = [\"i_start\", \"i_addr\", \"i_nbytes\", \"o_busy\", \"o_dones\"]\n\n\
         [bundle.hmem]\nbacking = \"bram\"\ndepth = 8\n\
         ports = { wr_cmd_valid = \"o_cmd_valid\", wr_cmd_ready = \"i_cmd_ready\", \
         wr_cmd_addr = \"o_cmd_addr\", wr_cmd_size = \"o_cmd_nbytes\", \
         wr_valid = \"o_wr_valid\", wr_ready = \"i_wr_ready\", \
         wr_data = \"o_wr_data\", wr_strb = \"o_wr_strb\", wr_last = \"o_wr_last\", \
         wr_done_valid = \"i_wr_done_valid\" }\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (start, addr, nbytes, busy, dones, maddr, mdata, oor) = (
        offset_of(&map, "i_start"),
        offset_of(&map, "i_addr"),
        offset_of(&map, "i_nbytes"),
        offset_of(&map, "o_busy"),
        offset_of(&map, "o_dones"),
        offset_of(&map, "hmem_maddr"),
        offset_of(&map, "hmem_mdata"),
        offset_of(&map, "hmem_oor"),
    );

    let tb = format!(
        r#"
#[test(dma_write_over_the_window)]
module dma_write_over_the_window {{
{BUS}
    initial {{
        start();

        // Prepare the strobe check: mark the high 4 bytes of entry 3. From
        // beat 1 on, the DUT writes only the low 4 bytes, so a full write
        // would erase the mark.
        wr({maddr}, 3);
        wr({mdata}, 32'h0000_0000);
        wr({mdata} + 4, 32'hdead_beef);

        // Let the DUT write 3 beats from entry 2.
        wr({addr}, 16);
        wr({addr} + 4, 0);
        wr({nbytes}, 24);
        wr({start}, 1);
        wr({start}, 0);
        for _i in 0..200 {{
            if rd({busy}) == 0 {{
                break;
            }}
        }}
        // A done came back, so the write side can finish the transfer.
        $assert(rd({dones}) != 0);

        // Read back through the backdoor.
        wr({maddr}, 2);
        $assert(rd({mdata}) == 32'ha50);
        wr({maddr}, 3);
        $assert(rd({mdata}) == 32'ha51);
        // The high 4 bytes are unchanged, so the strobe works.
        $assert(rd({mdata} + 4) == 32'hdead_beef);
        wr({maddr}, 4);
        $assert(rd({mdata}) == 32'ha52);
        // Nothing outside the transfer changed.
        wr({maddr}, 5);
        $assert(rd({mdata}) == 0);

        $assert(rd({oor}) == 0);
        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);

    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// An `access = "region"` window works in simulation.
///
/// Three checks: what is written to the region reads back from it, registers
/// in the same window are not affected, and the DUT sees the content. The
/// third one fails if the decode does not reach the right memory entry.
#[test]
fn a_region_reaches_the_same_memory_the_dut_reads() {
    let dut = r#"
module dut_top (
    i_clk       : input  clock    ,
    i_rst       : input  reset    ,
    i_run       : input  logic    ,
    o_imem_addr : output logic<8> ,
    i_imem_rdata: input  logic<32>,
    o_last      : output logic<32>,
) {
    var addr: logic<8>;
    always_ff {
        if_reset {
            addr   = 0;
            o_last = 0;
        } else if i_run {
            addr   = addr + 1;
            o_last = i_imem_rdata;
        }
    }
    assign o_imem_addr = addr;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.last]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"o_last\"]\n\n[bundle.imem]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram_preload\"\naddressing = \"word\"\ndepth = 256\naccess = \"region\"\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (base, run, last, depth) = (
        region_base(&map, "imem"),
        offset_of(&map, "i_run"),
        offset_of(&map, "o_last"),
        offset_of(&map, "imem_depth"),
    );
    // The region is at the low end of the window, and the registers are above
    // it. The checks below depend on this.
    assert_eq!(base, 0, "{map:#?}");
    assert!(run > base, "{map:#?}");

    let tb = format!(
        r#"
#[test(region_window)]
module region_window {{
{BUS}
    initial {{
        start();

        // The depth register is not affected by the region.
        $assert(rd({depth}) == 256);

        // Write to the region and read it back. The address is the window
        // offset. 16 entries are written so that the DUT, run below, stops
        // inside the written values wherever it stops. How far it goes
        // depends on bus timing, so the check does not depend on it.
        for i in 0..16 {{
            wr({base} + i * 4, 32'hc0de_0000 + i);
        }}
        for i in 0..16 {{
            $assert(rd({base} + i * 4) == 32'hc0de_0000 + i);
        }}
        $display("region[0] = %h  region[15] = %h", rd({base}), rd({base} + 60));

        // The host writes where the DUT reads. The DUT walks one entry per
        // cycle and puts the value on o_last. Check only the upper half.
        wr({run}, 1);
        clk.next(1);
        wr({run}, 0);
        clk.next(4);
        // A call result cannot be sliced, so store it first.
        var seen: logic<32>;
        seen = rd({last});
        $display("o_last = %h", seen);
        $assert(seen[31:16] == 16'hc0de);

        // Registers outside the region are not affected by the writes.
        $assert(rd({depth}) == 256);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// A region whose entry spans two window words.
///
/// A write covers only part of the entry, so the host says which bytes to
/// write (`i_hwstrb` of `hns::mem`). Without that, writing the low word
/// destroys the high word. This test catches it.
#[test]
fn a_region_whose_entry_spans_two_words_writes_only_the_word_asked_for() {
    let dut = r#"
module dut_top (
    i_clk       : input  clock    ,
    i_rst       : input  reset    ,
    i_run       : input  logic    ,
    o_imem_addr : output logic<8> ,
    i_imem_rdata: input  logic<64>,
    o_last      : output logic<64>,
) {
    var addr: logic<8>;
    always_ff {
        if_reset {
            addr   = 0;
            o_last = 0;
        } else if i_run {
            addr   = addr + 1;
            o_last = i_imem_rdata;
        }
    }
    assign o_imem_addr = addr;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.last]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"o_last\"]\n\n[bundle.imem]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram_preload\"\naddressing = \"word\"\ndepth = 64\naccess = \"region\"\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (base, run, last) = (
        region_base(&map, "imem"),
        offset_of(&map, "i_run"),
        offset_of(&map, "o_last"),
    );

    let tb = format!(
        r#"
#[test(wide_region_window)]
module wide_region_window {{
{BUS}
    initial {{
        start();

        // Write only the low word of entry 0. The high word is not touched.
        wr({base} + 0, 32'haaaa_1111);
        $assert(rd({base} + 0) == 32'haaaa_1111);

        // Write the high word. The low word must stay; if it changes, the
        // strobe does not work.
        wr({base} + 4, 32'hbbbb_2222);
        $assert(rd({base} + 0) == 32'haaaa_1111);
        $assert(rd({base} + 4) == 32'hbbbb_2222);

        // The next entry is independent too.
        wr({base} + 8, 32'hcccc_3333);
        wr({base} + 12, 32'hdddd_4444);
        $assert(rd({base} + 0) == 32'haaaa_1111);
        $assert(rd({base} + 4) == 32'hbbbb_2222);
        $assert(rd({base} + 8) == 32'hcccc_3333);
        $assert(rd({base} + 12) == 32'hdddd_4444);

        // The DUT reads all 64 bits. The window shows them as two words.
        wr({run}, 1);
        clk.next(1);
        wr({run}, 0);
        clk.next(4);
        $display("o_last = %h %h", rd({last} + 4), rd({last}));

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// An addressable slave interface in the DUT.
///
/// The window bus connects to the DUT directly. Two checks: a write lands at
/// that address inside the DUT, and the identity header in the same window is
/// not affected. The address is a word index, so the DUT gets the window
/// offset divided by 4.
#[test]
fn a_slave_interface_the_host_drives_reaches_the_dut() {
    let dut = r#"
module dut_top (
    i_clk      : input  clock    ,
    i_rst      : input  reset    ,
    i_csr_addr : input  logic<4> ,
    i_csr_wdata: input  logic<32>,
    i_csr_we   : input  logic    ,
    o_csr_rdata: output logic<32>,
) {
    var mem: logic<32> [16];
    always_ff {
        if i_csr_we {
            mem[i_csr_addr] = i_csr_wdata;
        }
        o_csr_rdata = mem[i_csr_addr];
    }
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.csr]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"slave\"\nports = { addr = \"i_csr_addr\", wdata = \"i_csr_wdata\", we = \"i_csr_we\", rdata = \"o_csr_rdata\" }\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (base, magic) = (region_base(&map, "csr"), offset_of(&map, "harness_magic"));
    // 16 words = 64 bytes, at the low end of the window. The header is at the end.
    assert_eq!(base, 0, "{map:#?}");
    assert!(magic > 60, "{map:#?}");

    let tb = format!(
        r#"
#[test(slave_window)]
module slave_window {{
{BUS}
    initial {{
        start();

        // Write and read. The address is the window offset.
        for i in 0..8 {{
            wr({base} + i * 4, 32'h5a5a_0000 + i);
        }}
        for i in 0..8 {{
            $assert(rd({base} + i * 4) == 32'h5a5a_0000 + i);
        }}
        $display("slave[0] = %h  slave[7] = %h", rd({base}), rd({base} + 28));

        // The identity header is not affected. This fails if the decode
        // reaches into the reserved area.
        $assert(rd({magic}) == 32'h5648524e);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// A DUT AXI4 master is terminated by FPGA memory. The host reaches the same
/// memory through an arbiter. With `backing = "dram"`, the shape stays and a
/// controller sits below it.
///
/// Two checks: AXI4 is really connected, and the DUT and the host see the
/// same memory. Without the second, the stand-in is useless for data.
#[test]
fn an_axi4_master_shares_its_memory_with_the_host() {
    let dut = r#"
module dut_top (
    i_clk : input  clock                                                              ,
    i_rst : input  reset                                                              ,
    i_run : input  logic                                                              ,
    o_last: output logic<32>                                                          ,
    axi   : modport $std::axi4_if::<$std::axi4_pkg::<32, 4, 4, 1, 1, 1, 1, 1>>::master,
) {
    // When run, it writes address 3, then reads address 1 into o_last.
    var phase: logic<3>;

    always_ff {
        if_reset {
            phase        = 0;
            o_last      = 0;
            axi.awvalid = 0;
            axi.wvalid  = 0;
            axi.arvalid = 0;
        } else {
            case phase {
                0: if i_run {
                    axi.awvalid = 1;
                    axi.wvalid  = 1;
                    phase        = 1;
                }
                1: if axi.awready {
                    axi.awvalid = 0;
                    phase        = 2;
                }
                2: if axi.wready {
                    axi.wvalid = 0;
                    phase       = 3;
                }
                3: if axi.bvalid {
                    axi.arvalid = 1;
                    phase        = 4;
                }
                4: if axi.arready {
                    axi.arvalid = 0;
                    phase        = 5;
                }
                5: if axi.rvalid {
                    o_last = axi.rdata;
                    phase   = 6;
                }
                default: {}
            }
        }
    }

    assign axi.awaddr   = 32'h0000_000c;
    assign axi.awsize   = 3'd2;
    assign axi.awburst  = 2'b01;
    assign axi.awcache  = 0;
    assign axi.awprot   = 0;
    assign axi.awid     = 0;
    assign axi.awlen    = 0;
    assign axi.awlock   = 0;
    assign axi.awqos    = 0;
    assign axi.awregion = 0;
    assign axi.awuser   = 0;
    assign axi.wlast    = 1;
    assign axi.wdata    = 32'hbeef_0003;
    assign axi.wstrb    = 4'hf;
    assign axi.wuser    = 0;
    assign axi.bready   = 1;
    assign axi.araddr   = 32'h0000_0004;
    assign axi.arsize   = 3'd2;
    assign axi.arburst  = 2'b01;
    assign axi.arcache  = 0;
    assign axi.arprot   = 0;
    assign axi.arid     = 0;
    assign axi.arlen    = 0;
    assign axi.arlock   = 0;
    assign axi.arqos    = 0;
    assign axi.arregion = 0;
    assign axi.aruser   = 0;
    assign axi.rready   = 1;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.last]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"o_last\"]\n\n[bundle.ddr]\nbacking = \"bram\"\ndepth = 64\nports = [\"axi\"]\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (base, run, last, depth) = (
        region_base(&map, "ddr"),
        offset_of(&map, "i_run"),
        offset_of(&map, "o_last"),
        offset_of(&map, "ddr_depth"),
    );
    assert_eq!(base, 0, "{map:#?}");

    let tb = format!(
        r#"
#[test(axi_mem_region)]
module axi_mem_region {{
{BUS}
    initial {{
        start();

        $assert(rd({depth}) == 64);

        // The DUT reads over AXI4 what the host put at address 1.
        wr({base} + 4, 32'ha5a5_0001);
        $assert(rd({base} + 4) == 32'ha5a5_0001);

        wr({run}, 1);
        clk.next(1);
        wr({run}, 0);
        clk.next(32);

        var seen: logic<32>;
        seen = rd({last});
        $display("o_last = %h", seen);
        $assert(seen == 32'ha5a5_0001);

        // The host reads from the region what the DUT wrote over AXI4.
        $display("mem[3] = %h", rd({base} + 12));
        $assert(rd({base} + 12) == 32'hbeef_0003);

        // The neighbours are not affected.
        $assert(rd({base} + 8) == 0);
        $assert(rd({base} + 4) == 32'ha5a5_0001);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// The host reaches the same memory while the DUT keeps running.
///
/// Through a backdoor this passes trivially, because the paths are separate.
/// Through an arbiter it does not: a wrong order gives one side's response to
/// the other. Without this test, such a bug shows up only on the board.
#[test]
fn an_axi4_region_answers_while_the_dut_keeps_the_bus_busy() {
    let dut = r#"
module dut_top (
    i_clk  : input  clock                                                             ,
    i_rst  : input  reset                                                             ,
    i_sweep: input  logic                                                             ,
    o_reads: output logic<32>                                                         ,
    o_last : output logic<32>                                                         ,
    axi    : modport $std::axi4_if::<$std::axi4_pkg::<32, 4, 4, 1, 1, 1, 1, 1>>::master,
) {
    // While i_sweep is high, keep reading addresses 0..3.
    var busy : logic   ;
    var sent : logic   ;
    var at   : logic<2>;

    always_comb {
        axi.awvalid  = 0;
        axi.awaddr   = 0;
        axi.awlen    = 0;
        axi.awsize   = 3'd2;
        axi.awburst  = 2'b01;
        axi.awcache  = 0;
        axi.awprot   = 0;
        axi.awid     = 0;
        axi.awlock   = 0;
        axi.awqos    = 0;
        axi.awregion = 0;
        axi.awuser   = 0;
        axi.wvalid   = 0;
        axi.wdata    = 0;
        axi.wstrb    = 0;
        axi.wlast    = 1;
        axi.wuser    = 0;
        axi.bready   = 1;

        axi.arvalid  = busy & ~sent;
        axi.araddr   = {28'b0, at, 2'b00};
        axi.arlen    = 0;
        axi.arsize   = 3'd2;
        axi.arburst  = 2'b01;
        axi.arcache  = 0;
        axi.arprot   = 0;
        axi.arid     = 0;
        axi.arlock   = 0;
        axi.arqos    = 0;
        axi.arregion = 0;
        axi.aruser   = 0;
        axi.rready   = 1;
    }

    always_ff {
        if_reset {
            busy    = 0;
            sent    = 0;
            at      = 0;
            o_reads = 0;
            o_last  = 0;
        } else {
            if !busy {
                if i_sweep {
                    busy = 1;
                    sent = 0;
                }
            } else {
                if axi.arvalid && axi.arready {
                    sent = 1;
                }
                if axi.rvalid && axi.rready {
                    busy    = 0;
                    at      = at + 1;
                    o_reads = o_reads + 1;
                    o_last  = axi.rdata;
                }
            }
        }
    }
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.ctl]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_sweep\", \"o_reads\", \"o_last\"]\n\n[bundle.ddr]\nbacking = \"bram\"\ndepth = 16\nports = [\"axi\"]\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (base, sweep, reads) = (
        region_base(&map, "ddr"),
        offset_of(&map, "i_sweep"),
        offset_of(&map, "o_reads"),
    );

    let tb = format!(
        r#"
#[test(dram_under_load)]
module dram_under_load {{
{BUS}
    initial {{
        start();

        // Put a known pattern.
        for i in 0..4 {{
            wr({base} + i * 4, 32'hc0de_0000 + i);
        }}

        // The host reads the same addresses while the DUT runs.
        wr({sweep}, 1);
        clk.next(16);
        var reads_a: logic<32>;
        reads_a = rd({reads});
        $assert(reads_a >: 0);

        for i in 0..4 {{
            $assert(rd({base} + i * 4) == 32'hc0de_0000 + i);
        }}
        // Writes work too.
        wr({base} + 8, 32'hbeef_0002);
        $assert(rd({base} + 8) == 32'hbeef_0002);

        var reads_b: logic<32>;
        reads_b = rd({reads});
        $display("dut reads: %h -> %h", reads_a, reads_b);
        // The DUT was not stalled. If the host held the bus, this would not grow.
        $assert(reads_b >: reads_a);

        wr({sweep}, 0);
        clk.next(16);
        $assert(rd({base}) == 32'hc0de_0000);

        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

/// A moving window reaches a memory bigger than the window.
///
/// A 64-entry memory is seen through a 64-byte (16-word) aperture.
/// `<bundle>_base_jtag` selects the page. Each master has its own base, so
/// moving the PCIe base does not change what JTAG sees.
#[test]
fn a_moving_window_reaches_a_memory_bigger_than_the_window() {
    let dut = r#"
module dut_top (
    i_clk       : input  clock    ,
    i_rst       : input  reset    ,
    i_run       : input  logic    ,
    o_imem_addr : output logic<6> ,
    i_imem_rdata: input  logic<32>,
    o_last      : output logic<32>,
) {
    var addr: logic<6>;
    always_ff {
        if_reset {
            addr   = 0;
            o_last = 0;
        } else if i_run {
            addr   = addr + 1;
            o_last = i_imem_rdata;
        }
    }
    assign o_imem_addr = addr;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.run]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"i_run\"]\n\n[bundle.last]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"o_last\"]\n\n[bundle.imem]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"bram_preload\"\naddressing = \"word\"\ndepth = 64\naccess = \"region\"\naperture = 64\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (base, page_j, page_p, last) = (
        region_base(&map, "imem"),
        offset_of(&map, "imem_base_jtag"),
        offset_of(&map, "imem_base_pcie"),
        offset_of(&map, "o_last"),
    );
    // Only the aperture is in the window. 64 entries x 4 bytes = 256, but the
    // region takes only 64 bytes.
    let size = map["regions"][0]["size_bytes"].as_u64().unwrap();
    let total = map["regions"][0]["total_bytes"].as_u64().unwrap();
    assert_eq!((size, total), (64, 256), "{map:#?}");

    let tb = format!(
        r#"
#[test(moving_window)]
module moving_window {{
{BUS}
    initial {{
        start();

        // There are 4 pages. Put a different pattern in each.
        for p in 0..4 {{
            wr({page_j}, p);
            for i in 0..16 {{
                wr({base} + i * 4, 32'hpage_0000 + p * 16 + i);
            }}
        }}

        // Go back to each page and read the pattern again.
        for p in 0..4 {{
            wr({page_j}, p);
            for i in 0..16 {{
                $assert(rd({base} + i * 4) == 32'hpage_0000 + p * 16 + i);
            }}
        }}

        // Moving the other master's base does not change this view. With one
        // shared base, a different page would appear here.
        wr({page_j}, 1);
        wr({page_p}, 3);
        $display("after the other master moved: %h", rd({base}));
        $assert(rd({base}) == 32'hpage_0000 + 16);

        // The DUT walks the whole memory, whatever the window shows.
        wr({page_j}, 0);
        clk.next(4);
        $finish();
    }}
}}
"#
    );
    let tb = tb.replace("32'hpage_0000", "32'hc0de_0000");
    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
    let _ = last;
}

/// A `dut_reset` in the middle of a burst does not block the host.
///
/// The DUT keeps writing 4-beat bursts with gaps between beats, so the reset
/// almost always hits the middle of a burst. If the fence did not complete the
/// rest, the arbiter would keep the path while it waits for B, and host
/// accesses to the region would stop.
///
/// This test cannot show that the DUT really resets and runs again. The
/// native simulator runs `if_reset` only on the test bench reset event, so a
/// reset made inside the harness has no effect. That is checked on the board.
#[test]
fn a_dut_reset_cuts_a_burst_without_blocking_the_host() {
    let dut = r#"
module dut_top (
    i_clk  : input  clock                                                              ,
    i_rst  : input  reset                                                              ,
    o_count: output logic<32>                                                          ,
    axi    : modport $std::axi4_if::<$std::axi4_pkg::<32, 4, 4, 1, 1, 1, 1, 1>>::master,
) {
    var phase: logic<2>;
    var beat : logic<2>;
    var gap  : logic<2>;

    always_ff {
        if_reset {
            phase       = 0;
            beat        = 0;
            gap         = 0;
            o_count     = 0;
            axi.awvalid = 0;
            axi.wvalid  = 0;
        } else {
            case phase {
                0: {
                    axi.awvalid = 1;
                    phase       = 1;
                }
                1: if axi.awready {
                    axi.awvalid = 0;
                    phase       = 2;
                }
                2: if axi.wvalid {
                    if axi.wready {
                        axi.wvalid = 0;
                        gap        = 2;
                        if beat == 3 {
                            beat  = 0;
                            phase = 3;
                        } else {
                            beat += 1;
                        }
                    }
                } else if gap != 0 {
                    gap -= 1;
                } else {
                    axi.wvalid = 1;
                }
                default: if axi.bvalid {
                    o_count += 1;
                    phase   =  0;
                }
            }
        }
    }

    assign axi.awaddr   = 32'h0000_0010;
    assign axi.awsize   = 3'd2;
    assign axi.awburst  = 2'b01;
    assign axi.awcache  = 0;
    assign axi.awprot   = 0;
    assign axi.awid     = 0;
    assign axi.awlen    = 3;
    assign axi.awlock   = 0;
    assign axi.awqos    = 0;
    assign axi.awregion = 0;
    assign axi.awuser   = 0;
    assign axi.wlast    = beat == 3;
    assign axi.wdata    = o_count;
    assign axi.wstrb    = 4'hf;
    assign axi.wuser    = 0;
    assign axi.bready   = 1;
    assign axi.araddr   = 0;
    assign axi.arsize   = 3'd2;
    assign axi.arburst  = 2'b01;
    assign axi.arcache  = 0;
    assign axi.arprot   = 0;
    assign axi.arid     = 0;
    assign axi.arlen    = 0;
    assign axi.arlock   = 0;
    assign axi.arqos    = 0;
    assign axi.arregion = 0;
    assign axi.aruser   = 0;
    assign axi.arvalid  = 0;
    assign axi.rready   = 1;
}
"#;
    let manifest = "[dut]\nmodule = \"dut_top\"\n\n[clock.i_clk]\nfreq_mhz = 200\n\n[bundle.count]\ncontract = \"fixed_latency\"\nlatency = 0\nbacking = \"reg\"\nports = [\"o_count\"]\n\n[bundle.ddr]\nbacking = \"bram\"\ndepth = 64\nports = [\"axi\"]\n";

    let dir = fixture(manifest, dut, "");
    let map = offsets(dir.path());
    let (base, count, reset, state) = (
        region_base(&map, "ddr"),
        offset_of(&map, "o_count"),
        offset_of(&map, "dut_reset"),
        offset_of(&map, "dut_reset_state"),
    );

    let tb = format!(
        r#"
#[test(dut_reset)]
module dut_reset {{
{BUS}
    initial {{
        start();
        $assert(rd({state}) == 0, "the DUT is held from the start");

        clk.next(200);
        $assert(rd({count}) != 0, "the DUT never finished a burst");

        // Assert it. After the write, poll until the state changes.
        wr({reset}, 1);
        var held: logic;
        held = 0;
        for _i in 0..64 {{
            if rd({state}) == 1 {{
                held = 1;
                break;
            }}
        }}
        $assert(held, "dut_reset_state never rose");

        // While held, the DUT finishes no burst.
        var seen: logic<32>;
        seen = rd({count});
        clk.next(100);
        $assert(rd({count}) == seen, "the DUT kept writing while held");

        // With the DUT held, the host still reaches the memory. If the rest
        // of the burst were not completed, the arbiter would be blocked here.
        wr({base} + 32, 32'h1234_5678);
        $assert(rd({base} + 32) == 32'h1234_5678, "the host lost the memory");

        // Release it.
        wr({reset}, 0);
        var released: logic;
        released = 0;
        for _i in 0..64 {{
            if rd({state}) == 0 {{
                released = 1;
                break;
            }}
        }}
        $assert(released, "dut_reset_state never fell");
        $finish();
    }}
}}
"#
    );
    write_tb(dir.path(), tb);
    assert!(
        veryl_test(&dir.path().join("hns")),
        "the simulation must pass"
    );
}

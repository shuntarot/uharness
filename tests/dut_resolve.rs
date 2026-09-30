//! Integration tests for `dut::resolve`. They run the real analyzer, so passing
//! them shows that the IR and the symbol table still give what we need.
//!
//! The symbol table is thread-local, so each test gets its own table and the
//! tests can run in parallel.

use std::fs;

use harness::bundle::{self, DirectionPrefixes, How};
use harness::dut::{self, DutError, SignalRole};
use harness::manifest::Manifest;
use veryl_metadata::Metadata;

/// Creates a Veryl project in a temporary directory and analyzes it.
///
/// `exclude_std = true` skips unpacking std into the cache directory. These
/// tests do not need std.
fn analyze(source: &str) -> (Metadata, veryl_analyzer::ir::Ir, tempfile::TempDir) {
    analyze_with(source, "")
}

/// `extra` is appended to `Veryl.toml` (used to test `[lint.naming]`).
fn analyze_with(
    source: &str,
    extra: &str,
) -> (Metadata, veryl_analyzer::ir::Ir, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Veryl.toml"),
        format!(
            "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[build]\nexclude_std = true\nreset_type = \"async_low\"\n{extra}"
        ),
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src").join("fixture.veryl"), source).unwrap();

    let mut metadata = Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    let ir = dut::analyze(&mut metadata).unwrap();
    (metadata, ir, dir)
}

const WRAPPER: &str = r#"
/// Has parameters (cannot be a harness DUT)
module inner #(
    param WIDTH: u32  = 8           ,
    param TYPE : type = logic<WIDTH>,
) (
    i_clk : input  clock,
    i_data: input  TYPE ,
    o_data: output TYPE ,
) {
    always_comb {
        o_data = i_data;
    }
}

/// No parameters (can be a harness DUT)
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
            count  = 0;
            o_data = 0;
        } else if i_push {
            count  = count + 1;
            o_data = i_data;
        }
    }

    assign o_full = count == 15;
}
"#;

#[test]
fn resolves_ports_in_declaration_order_with_resolved_widths() {
    let (metadata, ir, _dir) = analyze(WRAPPER);
    let dut = dut::resolve(&ir, &metadata.project.name, "dut_top").unwrap();

    // Declaration order. The IR's port_types is a HashMap, so taking the order
    // from it would make the output non-deterministic.
    let names: Vec<&str> = dut.ports.iter().map(|port| port.name.as_str()).collect();
    assert_eq!(
        names,
        ["i_clk", "i_rst", "i_push", "i_data", "o_full", "o_data"]
    );

    let width = |name: &str| {
        dut.ports
            .iter()
            .find(|port| port.name == name)
            .unwrap()
            .signals[0]
            .width
    };
    assert_eq!(width("i_clk"), Some(1));
    assert_eq!(width("i_data"), Some(8));
    assert_eq!(width("o_data"), Some(8));

    let role = |name: &str| {
        dut.ports
            .iter()
            .find(|port| port.name == name)
            .unwrap()
            .signals[0]
            .role
    };
    assert_eq!(role("i_clk"), SignalRole::Clock);
    assert_eq!(role("i_rst"), SignalRole::Reset);
    assert_eq!(role("i_data"), SignalRole::Data);

    assert!(dut.file.ends_with("src/fixture.veryl"));
}

/// Type parameters must not be missed. The IR's `Module.variables` drops
/// `param TYPE: type`, so a check on the IR alone misses the parameter that
/// sets the port width.
#[test]
fn a_parameterized_module_is_rejected_including_its_type_parameter() {
    let (metadata, ir, _dir) = analyze(WRAPPER);
    let err = dut::resolve(&ir, &metadata.project.name, "inner").unwrap_err();

    let DutError::HasParameters { count, names, .. } = &err else {
        panic!("expected HasParameters, got {err:?}");
    };
    assert_eq!(*count, 2, "names were: {names}");
    assert!(names.contains("WIDTH"), "names were: {names}");
    assert!(names.contains("TYPE"), "names were: {names}");

    // The message must say how to fix it.
    let rendered = format!("{:?}", miette::Report::new(err));
    assert!(rendered.contains("wrapper"), "rendered: {rendered}");
}

#[test]
fn an_unknown_module_lists_the_modules_that_do_exist() {
    let (metadata, ir, _dir) = analyze(WRAPPER);
    let err = dut::resolve(&ir, &metadata.project.name, "dut_topp").unwrap_err();

    let DutError::NotFound { candidates, .. } = &err else {
        panic!("expected NotFound, got {err:?}");
    };
    assert!(candidates.contains("dut_top"), "candidates: {candidates}");
    assert!(candidates.contains("inner"), "candidates: {candidates}");
}

/// A generic module has no single port boundary, so it is rejected.
#[test]
fn a_generic_module_is_rejected() {
    let source = r#"
proto module proto_x;

module generic_x::<X: proto_x> {
    inst u: X;
}
"#;
    let (metadata, ir, _dir) = analyze(source);
    let err = dut::resolve(&ir, &metadata.project.name, "generic_x").unwrap_err();

    assert!(matches!(err, DutError::HasGenerics { .. }), "got {err:?}");
}

/// A project that fails analysis must stop `check`, not pass silently.
#[test]
fn a_project_that_does_not_analyze_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Veryl.toml"),
        "[project]\nname = \"broken\"\nversion = \"0.1.0\"\n\n[build]\nexclude_std = true\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src").join("broken.veryl"),
        "module broken ( i_clk: input clock, ) { assign o_nope = 1; }\n",
    )
    .unwrap();

    let mut metadata = Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    assert!(dut::analyze(&mut metadata).is_err());
}

/// Direction prefixes come from `[lint.naming]`. In a project that does not use
/// the default `i` / `o`, naming-based matching must follow that project's rule.
#[test]
fn the_direction_prefix_comes_from_the_lint_config() {
    let source = r#"
module dut_top (
    in_clk       : input  clock   ,
    in_rst       : input  reset   ,
    in_csr_wdata : input  logic<8>,
    out_csr_rdata: output logic<8>,
) {
    always_ff {
        if_reset {
            out_csr_rdata = 0;
        } else {
            out_csr_rdata = in_csr_wdata;
        }
    }
}
"#;
    let extra = "\n[lint.naming]\nprefix_port_input  = \"in\"\nprefix_port_output = \"out\"\n";
    let (metadata, ir, _dir) = analyze_with(source, extra);
    let dut = dut::resolve(&ir, &metadata.project.name, "dut_top").unwrap();

    let prefixes = DirectionPrefixes::from_metadata(&metadata);
    assert_eq!(prefixes.input.as_deref(), Some("in"));
    assert_eq!(prefixes.output.as_deref(), Some("out"));

    let manifest: Manifest =
        toml::from_str("[dut]\nmodule = \"dut_top\"\n\n[bundle.csr]\nbacking = \"reg\"\n").unwrap();
    let bindings = bundle::resolve(&dut, &manifest, &prefixes, &[]).unwrap();

    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].bundle, "csr");
    assert_eq!(bindings[0].how, How::Naming);
    // Declaration order. Clock and reset are not included.
    assert_eq!(bindings[0].ports, ["in_csr_wdata", "out_csr_rdata"]);
}

/// The body of `examples/dump_ports.rs`. It is the tool for checking the
/// analyzer contract by eye, so it must still work when Veryl is upgraded.
///
/// It needs only `Veryl.toml`, with no manifest or bundles, so it shows
/// directly whether the IR still returns widths and domains.
#[test]
fn the_port_dump_shows_widths_directions_and_domains() {
    let (_metadata, ir, _dir) = analyze(WRAPPER);

    let all = harness::dut::describe_ports(&ir, None);
    assert!(all.contains("module dut_top"), "{all}");
    assert!(all.contains("module inner"), "{all}");

    let one = harness::dut::describe_ports(&ir, Some("dut_top"));
    assert!(
        !one.contains("module inner"),
        "filter should narrow it:\n{one}"
    );

    // Width, direction and kind are shown. If these turn into `UNRESOLVED`,
    // the analyzer no longer provides them.
    assert!(one.contains("width=8"), "{one}");
    assert!(one.contains("width=1"), "{one}");
    assert!(one.contains("input"), "{one}");
    assert!(one.contains("output"), "{one}");
    assert!(one.contains("kind=Clock"), "{one}");
    assert!(one.contains("kind=Reset"), "{one}");
    assert!(
        !one.contains("UNRESOLVED"),
        "nothing should be unresolved:\n{one}"
    );

    // Pin a known gap. The IR's `Module.variables` drops `param TYPE: type`,
    // so this tool does not show type parameters. The generator gets them from
    // the symbol table, but this tool shows what the IR returns.
    //
    // If Veryl adds type parameters to the IR, this assert fails.
    let inner = harness::dut::describe_ports(&ir, Some("inner"));
    assert!(inner.contains("WIDTH"), "{inner}");
    assert!(
        !inner.contains("TYPE"),
        "the IR started exposing type parameters -- good news, update this test \
         and see whether dut::resolve can stop using the symbol table:\n{inner}"
    );
}

/// A `std::axi4_if` modport port gives its direction and widths.
///
/// Interfaces do not appear in the IR, so the widths come from the generic
/// arguments written on the port, not from the interface body. Without this,
/// an AXI4 port would need about 40 lines in `ports`.
///
/// This test uses std, so it does not set `exclude_std`.
#[test]
fn an_axi4_modport_port_gives_up_its_direction_and_widths() {
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
    mem  : modport $std::axi4_if::<$std::axi4_pkg::<64, 8, 4, 1, 1, 1, 1, 1>>::master,
) {
    always_comb {
        mem.awvalid = 0;
    }
}
"#,
    )
    .unwrap();

    let mut metadata = Metadata::load(dir.path().join("Veryl.toml")).unwrap();
    let ir = harness::dut::analyze(&mut metadata).unwrap();
    let dut = harness::dut::resolve(&ir, "fixture", "dut_top").unwrap();

    let mem = dut.ports.iter().find(|p| p.name == "mem").unwrap();
    let axi4 = mem.axi4.as_ref().expect("mem is a std::axi4_if modport");
    // The modport name is the direction. If the DUT is master, the harness is slave.
    assert_eq!(axi4.modport, "master");
    assert!(axi4.is_master());
    // Widths come from the generic arguments. Data is given in bytes, so the
    // bit width is 8 times that.
    assert_eq!(axi4.addr_width(), 64);
    assert_eq!(axi4.data_bytes(), 8);
    assert_eq!(axi4.data_width(), 64);
    assert_eq!(axi4.id_width(), 4);

    // A port that is not a std AXI4 modport gets nothing.
    let clk = dut.ports.iter().find(|p| p.name == "i_clk").unwrap();
    assert!(clk.axi4.is_none());
}

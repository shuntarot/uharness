//! Runs the test benches of the parts package `rtl/hns` (`tb_*.veryl`).
//!
//! Without this, they run only when someone types `veryl test` in `rtl/hns`,
//! so a broken part would reach CI unnoticed.

use std::fs;
use std::path::Path;

use clap::Parser;

/// Runs `veryl test` as a library, like `tests/sim.rs`, so the version is the
/// linked one and not the `veryl` on `PATH`.
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

/// The package is copied first. Running in place would write the generated
/// SV into `rtl/hns/src` while other tests read the package as a dependency.
#[test]
fn every_part_passes_its_test_bench() {
    let hns = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("rtl")
        .join("hns");
    let dir = tempfile::tempdir().unwrap();
    fs::copy(hns.join("Veryl.toml"), dir.path().join("Veryl.toml")).unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    let mut benches = 0;
    for entry in fs::read_dir(hns.join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("veryl") {
            continue;
        }
        let name = path.file_name().unwrap();
        if name.to_string_lossy().starts_with("tb_") {
            benches += 1;
        }
        fs::copy(&path, dir.path().join("src").join(name)).unwrap();
    }
    // A wrong path would find nothing and pass.
    assert!(benches > 0, "no test bench found in {}", hns.display());

    // The simulator logs nothing here, so the message says where to look.
    assert!(
        veryl_test(dir.path()),
        "a part failed its test bench; run `veryl test` in rtl/hns to see which"
    );
}

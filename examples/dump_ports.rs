//! A spike: link the veryl compiler and show that parameter-resolved
//! port widths and clock domains really can be read out of the analyzer IR.
//!
//!     cargo run --example dump_ports -- <Veryl.toml> [module_name]
//!
//! What it prints is the boundary the whole design rests on: whatever the
//! analyzer resolves is taken from the source, and whatever it cannot (width
//! shown as UNRESOLVED) is what the manifest has to state instead.
//!
//! **The work is in `harness::dut::describe_ports`.** Kept in the library
//! rather than here, because code that lives only in an example cannot be
//! called from tests -- and would be found broken exactly when it is needed
//! (`tests/dut_resolve.rs` guards it).

use std::env;
use std::path::PathBuf;

use veryl::pipeline::{self, AnalyzeOptions};
use veryl_analyzer::ir::Ir;
use veryl_metadata::Metadata;

fn main() -> miette::Result<()> {
    let mut args = env::args().skip(1);
    let Some(toml) = args.next() else {
        eprintln!("usage: dump_ports <Veryl.toml> [module_name]");
        std::process::exit(2);
    };
    let filter = args.next();

    let mut metadata = Metadata::load(&toml)?;
    let no_files: Vec<PathBuf> = Vec::new();
    let paths = metadata.paths(&no_files, true, true)?;

    let options = AnalyzeOptions {
        defines: &[],
        emit_mode: false,
        incremental: false,
        fail_fast: false,
    };
    let mut ir = Ir::default();
    let _ = pipeline::analyze(&metadata, &paths, options, Some(&mut ir), None)?;

    print!("{}", harness::dut::describe_ports(&ir, filter.as_deref()));
    Ok(())
}

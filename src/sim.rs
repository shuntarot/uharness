//! `veryl harness sim`: runs a harness made with `gen --target sim`.
//!
//! This is `veryl test` on that directory, run from this binary. Plain
//! `veryl test` runs it too. This command adds what that lacks: it finds the
//! directory and refuses one not made for `sim`, removes `sim.addr` on Ctrl-C
//! (the component's `on_finish` does not run then), and uses the Veryl this
//! binary is built with, not the one on `PATH`.

use std::path::{Path, PathBuf};

use clap::Parser;

use crate::generate::Invocation;

/// Where the component writes the address it listens on. `hio` reads it.
pub const ADDR_FILE: &str = "sim.addr";

pub fn run(out_dir: Option<&Path>, wave: bool) -> miette::Result<()> {
    let out = match out_dir {
        Some(path) if path.is_relative() => std::env::current_dir()
            .map_err(|source| miette::miette!("cannot read the current directory: {source}"))?
            .join(path),
        Some(path) => path.to_path_buf(),
        // Inside a generated harness, run that one. Searching for `Veryl.toml`
        // from there finds the harness's own and looks for `hns/` inside it.
        None => {
            let here = std::env::current_dir()
                .map_err(|source| miette::miette!("cannot read the current directory: {source}"))?;
            if crate::generate::is_harness_dir(&here) {
                here
            } else {
                crate::generate::default_out_dir()?
            }
        }
    };

    let recorded = Invocation::read(&out)?;
    if recorded.target.as_deref() != Some(crate::target::SIM) {
        return Err(miette::miette!(
            code = "harness::sim::not_a_sim_harness",
            help = format!(
                "`sim` runs a harness generated for the simulator. Generate one inside it, and run that:\n\n    veryl harness gen --target sim --out-dir {0}\n    veryl harness sim -o {0}",
                out.join("sim").display()
            ),
            "`{}` was generated for {}, not for `--target sim`",
            out.display(),
            match &recorded.target {
                Some(target) => format!("`{target}`"),
                None => "a target file".to_string(),
            }
        ));
    }

    let addr = out.join(ADDR_FILE);
    // A file left by a run that was killed would send hio to a dead port.
    let _ = std::fs::remove_file(&addr);
    remove_on_interrupt(addr.clone())?;

    let mut metadata =
        veryl_metadata::Metadata::load(out.join("Veryl.toml")).map_err(|source| {
            miette::miette!(
                code = "harness::sim::no_project",
                "cannot read `{}`: {source}",
                out.join("Veryl.toml").display()
            )
        })?;
    std::env::set_current_dir(&out)
        .map_err(|source| miette::miette!("cannot enter `{}`: {source}", out.display()))?;

    println!("building the socket component with cargo, then waiting for hio");
    println!("stop with Ctrl-C");
    let passed = veryl::cmd_test::CmdTest::new(test_options(wave))
        .exec(&mut metadata)
        .map_err(|source| {
            miette::miette!(
                code = "harness::sim::failed",
                "the simulation stopped: {source}"
            )
        });
    let _ = std::fs::remove_file(&addr);
    if !passed? {
        return Err(miette::miette!(
            code = "harness::sim::failed",
            "the simulation stopped with a failure; see the output above"
        ));
    }
    Ok(())
}

/// `veryl test` options, built the way its own command line builds them.
/// Output is not buffered, so the component's log shows as it comes.
fn test_options(wave: bool) -> veryl::OptTest {
    #[derive(Parser)]
    struct Args {
        #[command(flatten)]
        opt: veryl::OptTest,
    }
    let mut argv = vec!["test", "--no-capture"];
    if wave {
        argv.push("--wave");
    }
    Args::parse_from(argv).opt
}

/// Ctrl-C does not reach the component's `on_finish`, so the address file is
/// removed here.
fn remove_on_interrupt(addr: PathBuf) -> miette::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM])
        .map_err(|source| miette::miette!("cannot catch Ctrl-C: {source}"))?;
    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            let _ = std::fs::remove_file(&addr);
            std::process::exit(128 + signal);
        }
    });
    Ok(())
}

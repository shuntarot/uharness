//! The `gen` subcommand: generates the harness.
//!
//! The module is `generate` because `gen` is reserved in Rust 2024.
//!
//! - All output goes under `hns/` (`hns/src` = Veryl, `hns/syn` =
//!   Tcl/XDC/Makefile).
//! - Nothing that `check` rejects is generated: both share `plan`, and `gen`
//!   only writes out the result.
//! - Hand-written files are never overwritten. Generated files carry a
//!   marker, and an existing file without it stops `gen` with an error.

use std::fs;
use std::path::{Path, PathBuf};

use miette::Diagnostic;
use thiserror::Error;

use crate::check::Format;
use crate::emit;
use crate::json;
use crate::plan;
use crate::target::Target;

/// Default output directory.
pub const OUT_DIR: &str = "hns";

/// Marks a generated file. A file without this string is never overwritten.
///
/// A "do not edit" note alone does not stop accidents, so the tool reads
/// this marker too.
pub const MARKER: &str = "veryl-harness:generated";

#[derive(Debug, Error, Diagnostic)]
pub enum GenError {
    #[error("`gen` needs a target")]
    #[diagnostic(
        code(harness::gen::target_required),
        help(
            "The harness depends on the board: its transport, its clock and the backings it can serve. Name one:\n\n    veryl harness gen --target <provider>/<board>\n\n`veryl harness targets` lists the shipped boards."
        )
    )]
    TargetRequired,

    #[error("`{}` was not written by veryl-harness", path.display())]
    #[diagnostic(
        code(harness::gen::would_overwrite),
        help(
            "Generated files carry a `{MARKER}` marker on their first lines, and this one does not. Move it away, or use another --out-dir."
        )
    )]
    WouldOverwrite { path: PathBuf },

    #[error("cannot write `{}`", path.display())]
    #[diagnostic(
        code(harness::gen::write_failed),
        help(
            "The output directory has to be writable. It is created if missing, so a failure here is usually a permission or a full disk."
        )
    )]
    WriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
pub struct Written {
    pub path: PathBuf,
    pub what: &'static str,
}

/// How `gen` was called. `update` reads it to generate the same thing again.
///
/// Paths are recorded as absolute paths. A relative path would point
/// somewhere else when `update` runs from another directory.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Invocation {
    pub marker: String,
    /// Absolute path to the `Veryl.toml` of the DUT project.
    ///
    /// Without it, `update` could write another design. The project search
    /// starts from the current directory, so `update -o` run from elsewhere
    /// would overwrite the harness with that directory's DUT.
    #[serde(default)]
    pub project: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub target: Option<String>,
    pub target_file: Option<PathBuf>,
    #[serde(default)]
    pub target_patch: Vec<PathBuf>,
    pub transport: Option<String>,
}

impl Invocation {
    pub fn path(out: &Path) -> PathBuf {
        out.join("harness.json")
    }

    /// Makes relative paths absolute and adds the marker.
    fn recorded(&self, project: &Path) -> Invocation {
        let absolute = |path: &PathBuf| -> PathBuf {
            if path.is_relative() {
                std::env::current_dir().unwrap_or_default().join(path)
            } else {
                path.clone()
            }
        };
        Invocation {
            marker: MARKER.to_string(),
            project: Some(project.to_path_buf()),
            config: self.config.as_ref().map(absolute),
            target: self.target.clone(),
            target_file: self.target_file.as_ref().map(absolute),
            target_patch: self.target_patch.iter().map(absolute).collect(),
            transport: self.transport.clone(),
        }
    }

    pub fn read(out: &Path) -> miette::Result<Invocation> {
        let path = Invocation::path(out);
        let text = std::fs::read_to_string(&path).map_err(|source| {
            miette::miette!(
                code = "harness::update::no_record",
                help = "`update` repeats the last `gen` in this directory, and needs the record `gen` leaves. Run `gen` once with the options you want:\n\n    veryl harness gen --target <name> --out-dir <dir>",
                "cannot read `{}`: {source}",
                path.display()
            )
        })?;
        serde_json::from_str(&text).map_err(|source| {
            miette::miette!(
                code = "harness::update::bad_record",
                "cannot read the record in `{}`: {source}",
                path.display()
            )
        })
    }
}

/// Whether `dir` holds a harness that `gen` wrote: a `harness.json` with the
/// marker.
pub fn is_harness_dir(dir: &Path) -> bool {
    Invocation::read(dir).is_ok_and(|recorded| recorded.marker == MARKER)
}

/// Output directory without `--out-dir`: `hns/` beside `Veryl.toml`.
///
/// `update` reads the record before it generates again, so it needs this
/// before the project is analyzed.
pub fn default_out_dir() -> miette::Result<PathBuf> {
    let metadata = veryl_metadata::Metadata::search_from_current().map_err(|source| {
        miette::miette!(
            code = "harness::update::no_project",
            help = "`update` looks for the harness beside Veryl.toml. Run it inside a Veryl project, or name the directory with --out-dir.",
            "cannot find a Veryl project: {source}"
        )
    })?;
    Ok(metadata
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(OUT_DIR))
}

pub fn run(
    invocation: &Invocation,
    target: Option<Target>,
    out_dir: Option<&Path>,
    format: Format,
) -> miette::Result<()> {
    let config = invocation.config.as_deref();
    let transport = invocation.transport.as_deref();
    // Nothing can be decided without a target. Never assume a default board.
    if target.is_none() {
        return Err(GenError::TargetRequired.into());
    }

    // The out-dir name becomes the project name; `plan::build` checks it, as
    // for `check`. Make the path absolute first, so that `--out-dir .` also
    // has a directory name.
    let out_dir = match out_dir {
        Some(path) if path.is_relative() => Some(
            std::env::current_dir()
                .map_err(|source| {
                    miette::miette!(
                        code = "harness::gen::no_current_dir",
                        "cannot read the current directory: {source}"
                    )
                })?
                .join(path),
        ),
        Some(path) => Some(path.to_path_buf()),
        None => None,
    };
    let out_dir_name = match &out_dir {
        Some(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        None => OUT_DIR.to_string(),
    };

    let plan = plan::build(
        config,
        target,
        transport,
        Some(&out_dir_name),
        |loaded, metadata, path| {
            if format == Format::Human {
                println!("manifest: {}", loaded.path.display());
                println!("project:  {} ({})", metadata.project.name, path.display());
                println!();
            }
        },
    )?;

    let out = out_dir.unwrap_or_else(|| plan.project_dir().join(OUT_DIR));

    let mut written = Vec::new();

    // Record the full target name. A short name that is unique today can
    // match two boards later, and `update` would then fail.
    let mut recorded = invocation.recorded(&plan.metadata_path);
    if recorded.target.is_some() {
        recorded.target = plan.target().map(|target| target.name.clone());
    }
    written.push(write(
        &Invocation::path(&out),
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&recorded).unwrap_or_default()
        ),
        "how this was generated",
    )?);

    // The output is its own Veryl project and depends on the DUT by path, so
    // the DUT project never contains it.
    written.push(write(
        &out.join("Veryl.toml"),
        &emit::veryl_toml(
            &plan,
            &hns_dependency(&plan, &out)?,
            &hop_to_project(&plan, &out),
        ),
        "Veryl project",
    )?);

    // The same file that `check --emit-regs` writes.
    written.push(write(
        &out.join("regs.json"),
        &format!(
            "{}\n",
            json::render_register_map(
                &plan.registers,
                plan.target(),
                plan.clocks(),
                plan.pcie_identity().as_ref()
            )
        ),
        "register map",
    )?);

    // The same map for people who write drivers, from the same IR as
    // `regs.json`.
    written.push(write(
        &out.join("regs.md"),
        &emit::regs_md(&plan),
        "register map (Markdown)",
    )?);

    // The window clock, as decided once by `clock::resolve`.
    let csr_ident = plan.board().clocks.window.clone();

    let sim = plan.target().is_some_and(|target| target.is_sim());
    if sim {
        written.extend(sim_files(&plan, &out)?);
    }

    // Always `Some`: `gen` has a target.
    if let Some(clocks) = plan.clocks().filter(|_| !sim) {
        // Formatting also parses our own output (`src/emit.rs`).
        let clock_veryl = emit::format(
            &emit::clock_module(clocks, plan.metadata.build.reset_type, &csr_ident),
            &plan.metadata,
        )?;
        written.push(write(
            &out.join("src").join("clk.veryl"),
            &clock_veryl,
            "clock generation (Veryl)",
        )?);
        written.push(write(
            &out.join("syn").join("mmcm.tcl"),
            &emit::mmcm_tcl(clocks),
            "MMCM IP (Tcl)",
        )?);
        if let Some(tcl) = emit::mig_tcl(&plan) {
            written.push(write(
                &out.join("syn").join("mig.tcl"),
                &tcl,
                "memory controller IP (Tcl)",
            )?);
        }
        if let Some(prj) = emit::mig_prj(&plan) {
            written.push(write(
                &out.join("syn").join("mig.prj"),
                &prj,
                "memory controller settings (board file)",
            )?);
        }

        let tck_mhz = crate::target::max_tck_mhz(&plan.board().target)
            .expect("plan::build checks max_tck_mhz");

        let prefixes = crate::bundle::DirectionPrefixes::from_metadata(&plan.metadata);
        let csr = emit::format(&emit::csr_module(&plan, &prefixes), &plan.metadata)?;
        written.push(write(
            &out.join("src").join("csr.veryl"),
            &csr,
            "reg terminator (Veryl)",
        )?);

        if let Some(heartbeat) = &plan.heartbeat {
            let uart = emit::format(&emit::uart_module(heartbeat), &plan.metadata)?;
            written.push(write(
                &out.join("src").join("uart.veryl"),
                &uart,
                "heartbeat UART (Veryl)",
            )?);
        }

        // Simulation top, only with one clock domain. With several, the test
        // bench would have to stand in for the MMCM, which is another design.
        if clocks.outputs.len() == 1 {
            let sim = emit::format(&emit::sim_module(&plan, &prefixes), &plan.metadata)?;
            written.push(write(
                &out.join("src").join("sim.veryl"),
                &sim,
                "simulation top (Veryl)",
            )?);
        }

        // The terminators (fifo / mem) and the window slave (axil) come from
        // the `hns` package. A part whose shape does not depend on the
        // manifest is not generated.

        let top = emit::format(
            &emit::top_module(&plan, &prefixes, &csr_ident),
            &plan.metadata,
        )?;
        written.push(write(
            &out.join("src").join("top.veryl"),
            &top,
            "harness top (Veryl)",
        )?);

        written.push(write(
            &out.join("syn").join("board.xdc"),
            &emit::board_xdc(&plan),
            "board pins and clock (XDC)",
        )?);

        // PCIe also writes the borrowed Verilog, so the output synthesizes on
        // its own.
        if emit::has_pcie(&plan) {
            let board = plan.target().map(hns_targets::pcie).unwrap_or_default();
            written.push(write(
                &out.join("syn").join("pcie.tcl"),
                &emit::pcie_tcl(&plan, &board),
                "PCIe hard block (Tcl)",
            )?);
            written.push(write(
                &out.join("syn").join("pcie.xdc"),
                &emit::pcie_xdc(&board),
                "PCIe pins (XDC)",
            )?);
            let block = plan
                .target()
                .and_then(emit::pcie_block)
                .expect("plan::generatable checks the family");
            let wrap = crate::vendor::pcie_wrap(block);
            for file in crate::vendor::PCIE
                .iter()
                .chain(std::iter::once(&wrap))
                .chain(crate::vendor::LICENSE)
            {
                written.push(write_verbatim(
                    &out.join("vendor").join(file.name),
                    file.text,
                    "borrowed, see vendor/COPYING",
                )?);
            }
        }
        written.push(write(
            &out.join("syn").join("harness.xdc"),
            &emit::harness_xdc(
                tck_mhz,
                &emit::cdc_instances(&plan),
                &emit::sync_instances(&plan),
            ),
            "harness constraints (XDC)",
        )?);
        written.push(write(
            &out.join("syn").join("ip.tcl"),
            &emit::ip_tcl(&plan),
            "IP generation (Tcl)",
        )?);
        written.push(write(
            &out.join("syn").join("synth.tcl"),
            &emit::synth_tcl(&plan),
            "synthesis (Tcl)",
        )?);
        written.push(write(
            &out.join("syn").join("area.tcl"),
            &emit::area_tcl(&plan),
            "area by instance (Tcl)",
        )?);
        written.push(write(
            &out.join("syn").join("board.tcl"),
            &emit::board_tcl(&plan),
            "board data for Tcl (data)",
        )?);
        // Programming always uses the JTAG configuration path, whatever the
        // transport.
        written.push(write(
            &out.join("syn").join("program.tcl"),
            &emit::program_tcl(),
            "bitstream programming (Tcl)",
        )?);
        written.push(write(
            &out.join("syn").join("svf.tcl"),
            &emit::svf_tcl(),
            "programming sequence as SVF (Tcl)",
        )?);
        written.push(write(
            &out.join("syn").join("Makefile"),
            &emit::makefile(&plan),
            "build flow (Makefile)",
        )?);
    }

    // Remove what is no longer generated. Without `[heartbeat]`, an old
    // `uart.veryl` would stay, and the output project would build it.
    let written = commit(written)?;
    let removed = remove_stale(&out, &written)?;

    match format {
        Format::Human => {
            for entry in &written {
                println!("wrote {}  ({})", entry.path.display(), entry.what);
            }
            for path in &removed {
                println!("removed {}  (no longer generated)", path.display());
            }
            println!();
            if let Some(target) = plan.target().filter(|t| t.head.board.untested) {
                println!(
                    "note: {} has not been run on real hardware yet",
                    target.name
                );
                println!();
            }
            if sim {
                print_sim_next_steps(&out);
            } else {
                print_next_steps(&out);
            }
        }
        Format::Json => {
            let project = json::Project {
                name: plan.metadata.project.name.clone(),
                manifest: plan.metadata_path.display().to_string(),
            };
            // Report the same checks as `check --target`: both run
            // `plan::build`.
            let mut output = json::ok(
                project,
                &plan,
                crate::check::checked_items(true),
                crate::check::not_checked_items(true, &plan.sv_blackboxes),
            );
            output.written = written
                .iter()
                .map(|entry| json::WrittenOutput {
                    path: entry.path.display().to_string(),
                    what: entry.what,
                })
                .collect();
            println!("{}", json::render(&output));
        }
    }

    Ok(())
}

/// The files of `--target sim`: the harness without its transport, a
/// testbench, and the component that serves the window over TCP. Nothing for
/// Vivado.
fn sim_files(plan: &plan::Plan, out: &Path) -> miette::Result<Vec<Pending>> {
    let prefixes = crate::bundle::DirectionPrefixes::from_metadata(&plan.metadata);
    let mut written = Vec::new();
    let csr = emit::format(&emit::csr_module(plan, &prefixes), &plan.metadata)?;
    written.push(write(
        &out.join("src").join("csr.veryl"),
        &csr,
        "reg terminator (Veryl)",
    )?);
    if let Some(heartbeat) = &plan.heartbeat {
        let uart = emit::format(&emit::uart_module(heartbeat), &plan.metadata)?;
        written.push(write(
            &out.join("src").join("uart.veryl"),
            &uart,
            "heartbeat UART (Veryl)",
        )?);
    }
    let sim = emit::format(&emit::sim_module(plan, &prefixes), &plan.metadata)?;
    written.push(write(
        &out.join("src").join("sim.veryl"),
        &sim,
        "simulation top (Veryl)",
    )?);
    let tb = emit::format(&emit::sim_testbench(plan), &plan.metadata)?;
    written.push(write(
        &out.join("src").join("sim_tb.veryl"),
        &tb,
        "testbench for veryl harness sim (Veryl)",
    )?);
    let link = out.join(emit::SIM_LINK_DIR);
    written.push(write(
        &link.join("Cargo.toml"),
        &emit::sim_link_cargo_toml(),
        "socket component package (Cargo)",
    )?);
    written.push(write(
        &link.join("veryl.manifest.json"),
        &emit::sim_link_manifest(),
        "socket component ports (Veryl)",
    )?);
    written.push(write(
        &link.join("src").join("lib.rs"),
        &emit::sim_link_source(),
        "socket component (Rust)",
    )?);
    Ok(written)
}

/// Removes generated files in the output that this run did not write.
///
/// Only files with the marker, and anything under `vendor/`. Borrowed files
/// cannot carry the marker (that would modify them), so the generator owns
/// all of `vendor/`. Files a user added have no marker and are kept; do not
/// put them in `vendor/`.
fn remove_stale(out: &Path, written: &[Written]) -> miette::Result<Vec<PathBuf>> {
    let kept: std::collections::BTreeSet<PathBuf> =
        written.iter().map(|entry| entry.path.clone()).collect();
    let mut removed = Vec::new();
    let mut stack = vec![out.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Build output (cargo, for `--target sim`) holds nothing of ours.
            if path == out.join("target") {
                continue;
            }
            if path.is_dir() {
                // Another harness inside this one (`-o hns/sim`) is not ours
                // to clean up; its own `gen` does that.
                if !is_harness_dir(&path) {
                    stack.push(path);
                }
                continue;
            }
            if kept.contains(&path) {
                continue;
            }
            // No marker check under `vendor/` (see `write_verbatim`). Otherwise,
            // after moving from pcie to jtag, `synth.tcl` would pick up the
            // unused `../vendor/*.v` files.
            let ours = path.starts_with(out.join("vendor"));
            if !ours {
                // A file that cannot be read is left alone.
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if !text.lines().take(4).any(|line| line.contains(MARKER)) {
                    continue;
                }
            }
            std::fs::remove_file(&path).map_err(|source| {
                miette::miette!(
                    code = "harness::gen::cannot_remove",
                    "cannot remove the stale `{}`: {source}",
                    path.display()
                )
            })?;
            removed.push(path);
        }
    }
    removed.sort();
    Ok(removed)
}

/// A file to write. Nothing touches the disk until `commit`.
struct Pending {
    path: PathBuf,
    contents: String,
    what: &'static str,
}

/// Queues a borrowed file as is, without the marker check.
///
/// `write` looks for the marker to protect hand-written files. A borrowed
/// file cannot carry one: that would modify it, and it could no longer be
/// compared with upstream. Instead, only names the
/// generator knows are written, and the generator owns all of `hns/vendor/`.
/// Do not put your own files there.
fn write_verbatim(path: &Path, contents: &str, what: &'static str) -> Result<Pending, GenError> {
    Ok(Pending {
        path: path.to_path_buf(),
        contents: contents.to_string(),
        what,
    })
}

/// Queues one file. Fails if an existing file there has no marker. That check
/// only reads, so it is done here, before `commit`.
fn write(path: &Path, contents: &str, what: &'static str) -> Result<Pending, GenError> {
    if path.exists() {
        let existing = fs::read_to_string(path).unwrap_or_default();
        // The marker is in the first lines: a "generator" field in JSON, a
        // comment in RTL.
        let head: String = existing.lines().take(5).collect::<Vec<_>>().join("\n");
        if !head.contains(MARKER) {
            return Err(GenError::WouldOverwrite {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(Pending {
        path: path.to_path_buf(),
        contents: contents.to_string(),
        what,
    })
}

/// Writes all queued files.
///
/// Writing starts only after every file is ready. Otherwise a failure in the
/// middle (in `emit::format`, say) would leave a new `regs.json` beside an
/// old `top.veryl`.
fn commit(pending: Vec<Pending>) -> Result<Vec<Written>, GenError> {
    pending
        .into_iter()
        .map(|entry| {
            if let Some(parent) = entry.path.parent() {
                fs::create_dir_all(parent).map_err(|source| GenError::WriteFailed {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::write(&entry.path, &entry.contents).map_err(|source| GenError::WriteFailed {
                path: entry.path.clone(),
                source,
            })?;
            Ok(Written {
                path: entry.path,
                what: entry.what,
            })
        })
        .collect()
}

/// What to run next to get a bitstream, and what is not generated yet.
fn print_next_steps(out: &Path) {
    println!("next:");
    println!("  make -C {}", out.join("syn").display());
    println!("      # veryl build -> IP generation -> synthesis -> bitstream");
    println!();
    println!("NOT GENERATED YET:");
    println!("  - host_mem / host_irq / observe terminators");
    println!();
    println!("The harness uses its own BSCANE2 bridge, which Vivado cannot see, so the");
    println!("window is reached with `hio` rather than hw_server. `make program`");
    println!("still goes through Vivado. Configuration uses a separate JTAG path.");
}

/// What to run next for `--target sim`.
fn print_sim_next_steps(out: &Path) {
    println!("next:");
    println!("  veryl harness sim -o {}", out.display());
    println!("      # builds the socket component with cargo, then waits for hio");
    println!("  hio id");
    println!();
    println!("Stop the simulation with Ctrl-C.");
}

/// Where `hns` comes from when the DUT project does not say.
const DEFAULT_HNS_GIT: &str = "https://github.com/shuntarot/uharness";

/// The `Veryl.toml` of the `hns` package. The version is read from it, so a
/// version bump of `hns` cannot leave a stale number in the generator.
const HNS_MANIFEST: &str = include_str!("../rtl/hns/Veryl.toml");

/// The `hns` dependency used when the DUT project does not declare one.
///
/// `version` is required. Without it, Veryl stops a git dependency with
/// `InvalidDependency: version is not specified`, and that error does not
/// point at the generator.
fn default_hns() -> String {
    format!(
        "{{ git = \"{DEFAULT_HNS_GIT}\", project = \"hns\", version = \"{}\" }}",
        hns_version()
    )
}

fn hns_version() -> String {
    HNS_MANIFEST
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| {
            t.get("project")?
                .get("version")?
                .as_str()
                .map(str::to_string)
        })
        .expect("the hns package states its version")
}

/// Relative path from the output directory back to the DUT project.
///
/// It must also work for a deeper out-dir such as `hns/arty`; a fixed `..`
/// fits only one level.
fn hop_to_project(plan: &crate::plan::Plan, out: &Path) -> String {
    relative_from(out, plan.project_dir())
}

/// Relative path from `from` to `to`.
///
/// Never an absolute path. Veryl tells dependencies apart by the path as
/// written. If the DUT says `../../../rtl/hns` and we write an absolute path,
/// the same package comes in twice (`dependencies/hns` and `hns_0`). The
/// second one renames modules to `hns_0_bscan`, which shows up only as a
/// missing module in synthesis.
fn relative_from(from: &Path, to: &Path) -> String {
    let (from, to) = (
        std::fs::canonicalize(from).unwrap_or_else(|_| from.to_path_buf()),
        std::fs::canonicalize(to).unwrap_or_else(|_| to.to_path_buf()),
    );
    let common = from
        .components()
        .zip(to.components())
        .take_while(|(a, b)| a == b)
        .count();
    let up = from.components().count() - common;
    let mut path = PathBuf::new();
    for _ in 0..up.max(1) {
        path.push("..");
    }
    for part in to.components().skip(common) {
        path.push(part);
    }
    path.display().to_string()
}

/// The `hns` dependency of the DUT project, rewritten for the generated
/// `Veryl.toml`.
///
/// A relative `path` must be recomputed from the output directory, which is
/// deeper. Otherwise `veryl build` cannot find the dependency, and the error
/// does not point at the generator.
fn hns_dependency(plan: &crate::plan::Plan, out: &Path) -> miette::Result<String> {
    let text = std::fs::read_to_string(&plan.metadata_path).map_err(|source| {
        miette::miette!(
            code = "harness::gen::unreadable_project",
            "cannot read `{}`: {source}",
            plan.metadata_path.display()
        )
    })?;
    let table: toml::Table = text.parse().map_err(|source| {
        miette::miette!(
            code = "harness::gen::unparsable_project",
            "cannot parse `{}`: {source}",
            plan.metadata_path.display()
        )
    })?;
    // Without a declaration the default is fine: only the harness project
    // uses the parts. A declaration is kept as is, so a local checkout or a
    // pinned rev is not overridden.
    let Some(mut value) = table
        .get("dependencies")
        .and_then(|deps| deps.get("hns"))
        .cloned()
    else {
        return Ok(default_hns());
    };

    if let Some(path) = value.get("path").and_then(|p| p.as_str()) {
        let relative = std::path::Path::new(path);
        if relative.is_relative() {
            // Keep it relative, never absolute (see `relative_from`).
            let moved = PathBuf::from(relative_from(out, &plan.project_dir().join(relative)));
            if let Some(table) = value.as_table_mut() {
                table.insert(
                    "path".to_string(),
                    toml::Value::String(moved.to_string_lossy().to_string()),
                );
            }
        }
    }
    // Make a one-line inline table. `toml::to_string` writes a document, which
    // would give `hns = path = "..."`.
    let inline = toml::to_string(&value)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("{{ {inline} }}"))
}

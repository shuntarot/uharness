//! The CLI, run by Veryl as an external subcommand.
//!
//! With `veryl-harness` on PATH, `veryl harness ...` calls it. Veryl passes
//! only argv (no env, no metadata), so the tool finds the project itself.
//!
//! `--info` must not run any analysis. For `veryl --list`, veryl calls
//! `--info` with a 500 ms timeout. It falls back to a placeholder unless the
//! output is one line of at most 160 characters, with no control characters,
//! and exit 0 (`veryl/crates/veryl/src/external_subcommand/help.rs`).
//!
//! Exit codes:
//!
//! | code | meaning |
//! |---|---|
//! | 0 | success |
//! | 1 | diagnostic (bad manifest, DUT not resolved, unsupported request) |
//! | 2 | usage error (from clap) |

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::check::Format;
use crate::target::{self, Target};
use miette::Diagnostic;
use thiserror::Error;

/// The one-line description in `veryl --list`: one line, at most 160
/// characters, no control characters. `--info` and clap's about share it.
pub const DESCRIPTION: &str = "Generate an FPGA harness for a single RTL block: terminate its ports as declared in Harness.toml, then emit harness RTL and a host register map";

/// Exit code for a failure with a diagnostic. Usage errors from clap use 2.
const EXIT_DIAGNOSTIC: u8 = 1;

#[derive(Debug, Parser)]
#[command(name = "veryl-harness", version, about = DESCRIPTION, long_about = None)]
pub struct Cli {
    /// Print the one-line description used by `veryl --list`, then exit.
    #[arg(long)]
    pub info: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check Harness.toml against the DUT source. Generates nothing.
    Check(CheckArgs),

    /// Generate the harness into hns/: RTL, IP scripts, constraints, a Makefile and the register map.
    Gen(CheckArgs),

    /// Re-generate a harness with the options it was generated with.
    Update(UpdateArgs),

    /// List the target descriptions that ship with this build.
    Targets,
}

/// `update` only repeats a generation, so it takes only the directory. The
/// options come from the record there (`harness.json`).
#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Which harness to update. Defaults to hns/ beside Veryl.toml.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub out_dir: Option<PathBuf>,

    /// Emit one JSON document on stdout instead of the human report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Path to Harness.toml. Skips the search entirely.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Target board: <provider>/<board>[:<config>], e.g. digilent/arty-a7-35.
    /// The start of the name is enough while only one board starts that way.
    #[arg(long, value_name = "NAME")]
    pub target: Option<String>,

    /// A target description of your own. Unofficial: the result is reported as unverified.
    #[arg(long, value_name = "PATH", conflicts_with = "target")]
    pub target_file: Option<PathBuf>,

    /// Layer a TOML patch over the target description. Repeatable; applied in order.
    #[arg(long, value_name = "PATH")]
    pub target_patch: Vec<PathBuf>,

    /// Transport: `pcie` or `jtag`. Without it, jtag, or the board's only transport.
    #[arg(long, value_name = "NAME")]
    pub transport: Option<String>,

    /// Write the register map to this path as JSON, without generating anything else.
    #[arg(long, value_name = "PATH")]
    pub emit_regs: Option<PathBuf>,

    /// Where `gen` writes. Defaults to hns/ beside Veryl.toml.
    ///
    /// The directory name becomes the Veryl project name, and Veryl puts that in front
    /// of every module it emits -- so a second directory is how two boards live side by side.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub out_dir: Option<PathBuf>,

    /// Emit one JSON document on stdout instead of the human report.
    #[arg(long)]
    pub json: bool,
    /// With `check`: also list every port and register, and what was and was not checked.
    #[arg(short, long)]
    pub verbose: bool,
}

/// Bad option combinations, and a broken shipped target description. An
/// option must never be ignored while the command still succeeds.
#[derive(Debug, Error, Diagnostic)]
pub enum CliError {
    #[error("--target-patch needs a target to patch")]
    #[diagnostic(
        code(harness::cli::patch_without_target),
        help(
            "A patch changes a target description, so name the target:\n\n    --target <provider>/<board> --target-patch <path>\n    --target-file <path> --target-patch <path>"
        )
    )]
    PatchWithoutTarget,

    #[error("a target description that ships with this build is broken")]
    #[diagnostic(
        code(harness::cli::broken_target),
        help(
            "This is a bug in the tool, not in your project. Please report it. To keep going, use --target-file with your own description."
        )
    )]
    BrokenTarget,
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    // Decided before running, so that failures are JSON too.
    let format = cli.format();

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(report) => {
            match format {
                // Errors are one JSON document on stdout too. An agent should
                // not have to parse two streams.
                Format::Json => println!("{}", crate::json::render(&crate::json::error(&report))),
                // miette's fancy handler prints labels with spans, and help.
                Format::Human => eprintln!("{report:?}"),
            }
            ExitCode::from(EXIT_DIAGNOSTIC)
        }
    }
}

impl Cli {
    /// `--json` on any subcommand makes all output JSON, errors included.
    fn format(&self) -> Format {
        let json = match &self.command {
            Some(Command::Check(args)) | Some(Command::Gen(args)) => args.json,
            Some(Command::Update(args)) => args.json,
            _ => false,
        };
        if json { Format::Json } else { Format::Human }
    }
}

fn run(cli: Cli) -> miette::Result<()> {
    if cli.info {
        // No analysis before this point. Past the 500 ms timeout,
        // `veryl --list` shows a fallback instead.
        println!("{DESCRIPTION}");
        return Ok(());
    }

    let Some(command) = cli.command else {
        // Treated like a clap usage error: help and exit 2, not a diagnostic.
        use clap::CommandFactory;
        let mut command = Cli::command();
        let _ = command.print_help();
        println!();
        std::process::exit(2);
    };

    match command {
        Command::Check(args) => {
            let format = if args.json {
                Format::Json
            } else {
                Format::Human
            };
            let target = resolve_target(&args)?;
            crate::check::run(
                args.config.as_deref(),
                target,
                args.transport.as_deref(),
                args.emit_regs.as_deref(),
                format,
                args.verbose,
            )
        }
        Command::Gen(args) => {
            let format = if args.json {
                Format::Json
            } else {
                Format::Human
            };
            let target = resolve_target(&args)?;
            crate::generate::run(&invocation(&args), target, args.out_dir.as_deref(), format)
        }
        // The options come from the record, not the command line. Otherwise
        // a harness could be overwritten for another board.
        Command::Update(args) => {
            let format = if args.json {
                Format::Json
            } else {
                Format::Human
            };
            // Make the path absolute now. The current directory changes to
            // the project below, and a relative path would then point
            // elsewhere.
            let out = match &args.out_dir {
                Some(path) if path.is_relative() => std::env::current_dir()
                    .map_err(|source| {
                        miette::miette!("cannot read the current directory: {source}")
                    })?
                    .join(path),
                Some(path) => path.clone(),
                None => crate::generate::default_out_dir()?,
            };
            let recorded = crate::generate::Invocation::read(&out)?;
            // Generate from the recorded project. The project search starts
            // from the current directory, so `-o` from elsewhere would
            // otherwise overwrite the harness with that directory's DUT.
            if let Some(project) = &recorded.project {
                let dir = project.parent().unwrap_or(std::path::Path::new("."));
                if !project.is_file() {
                    return Err(miette::miette!(
                        code = "harness::update::project_moved",
                        help = "`update` repeats the generation that made this directory, and that needs the project it was generated from. If it moved, generate again with `gen`.",
                        "the project this harness was generated from is gone: `{}`",
                        project.display()
                    ));
                }
                std::env::set_current_dir(dir).map_err(|source| {
                    miette::miette!("cannot enter `{}`: {source}", dir.display())
                })?;
            }
            let replay = CheckArgs {
                config: recorded.config.clone(),
                target: recorded.target.clone(),
                target_file: recorded.target_file.clone(),
                target_patch: recorded.target_patch.clone(),
                transport: recorded.transport.clone(),
                emit_regs: None,
                out_dir: Some(out.clone()),
                json: args.json,
                verbose: false,
            };
            let target = resolve_target(&replay)?;
            crate::generate::run(&invocation(&replay), target, Some(&out), format)
        }
        Command::Targets => list_targets(),
    }
}

fn invocation(args: &CheckArgs) -> crate::generate::Invocation {
    crate::generate::Invocation {
        marker: String::new(),
        // `generate::run` fills this in; the project is not resolved yet.
        project: None,
        config: args.config.clone(),
        target: args.target.clone(),
        target_file: args.target_file.clone(),
        target_patch: args.target_patch.clone(),
        transport: args.transport.clone(),
    }
}

/// Resolves `--target` / `--target-file` / `--target-patch` to one target.
/// Returns `None` without a target; the manifest checks still run then.
fn resolve_target(args: &CheckArgs) -> miette::Result<Option<Target>> {
    match (&args.target, &args.target_file) {
        (Some(name), None) => Ok(Some(target::resolve(name, &args.target_patch)?)),
        (None, Some(path)) => Ok(Some(target::load_file(path, &args.target_patch)?)),
        (Some(_), Some(_)) => unreachable!("clap rejects this"),
        (None, None) => {
            if args.target_patch.is_empty() {
                Ok(None)
            } else {
                Err(CliError::PatchWithoutTarget.into())
            }
        }
    }
}

fn list_targets() -> miette::Result<()> {
    let entries = target::list();
    if entries.is_empty() {
        println!("no target descriptions ship with this build");
        return Ok(());
    }

    let mut listed = Vec::new();
    for entry in entries {
        // A broken shipped description is not the user's fault; say so.
        listed
            .push(entry.map_err(|err| miette::Report::new(err).wrap_err(CliError::BrokenTarget))?);
    }

    // Shared with `hio targets`, so the two listings cannot drift apart.
    print!("{}", target::listing(&listed));

    println!();
    println!(
        "Not every target here has been tested end to end. The all-targets \
         CI is not running yet."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    /// What `veryl --list` accepts (external_subcommand/help.rs). A violation
    /// only shows up as a silent fallback in the listing.
    #[test]
    fn description_satisfies_the_veryl_list_contract() {
        assert!(!DESCRIPTION.trim().is_empty());
        assert!(
            DESCRIPTION.chars().count() <= 160,
            "{} chars",
            DESCRIPTION.chars().count()
        );
        assert!(!DESCRIPTION.chars().any(char::is_control));
        assert_eq!(DESCRIPTION.trim(), DESCRIPTION);
    }

    #[test]
    fn info_needs_no_subcommand() {
        let cli = Cli::try_parse_from(["veryl-harness", "--info"]).unwrap();

        assert!(cli.info);
        assert!(cli.command.is_none());
    }

    /// Both at once would leave unclear which one applies.
    #[test]
    fn target_and_target_file_are_exclusive() {
        let err = Cli::try_parse_from([
            "veryl-harness",
            "check",
            "--target",
            "digilent/arty-a7-35",
            "--target-file",
            "./my.toml",
        ])
        .unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn a_patch_without_a_target_is_rejected() {
        let cli =
            Cli::try_parse_from(["veryl-harness", "check", "--target-patch", "./p.toml"]).unwrap();
        let Some(Command::Check(args)) = cli.command else {
            panic!("expected check");
        };

        let err = resolve_target(&args).unwrap_err();
        assert!(
            format!("{err:?}").contains("patch_without_target"),
            "{err:?}"
        );
    }

    /// Patches apply in argument order.
    #[test]
    fn patches_keep_their_order() {
        let cli = Cli::try_parse_from([
            "veryl-harness",
            "check",
            "--target",
            "digilent/arty-a7-35",
            "--target-patch",
            "a.toml",
            "--target-patch",
            "b.toml",
        ])
        .unwrap();
        let Some(Command::Check(args)) = cli.command else {
            panic!("expected check");
        };

        assert_eq!(
            args.target_patch,
            [PathBuf::from("a.toml"), PathBuf::from("b.toml")]
        );
    }

    /// A human-readable error after `--json` would break an agent's parser.
    #[test]
    fn json_is_decided_before_the_command_runs() {
        let cli = Cli::try_parse_from(["veryl-harness", "gen", "--json"]).unwrap();
        assert_eq!(cli.format(), Format::Json);

        let cli = Cli::try_parse_from(["veryl-harness", "check"]).unwrap();
        assert_eq!(cli.format(), Format::Human);
    }

    #[test]
    fn check_takes_a_config_path() {
        let cli =
            Cli::try_parse_from(["veryl-harness", "check", "--config", "x/Harness.toml"]).unwrap();

        let Some(Command::Check(args)) = cli.command else {
            panic!("expected check");
        };
        assert_eq!(args.config, Some(PathBuf::from("x/Harness.toml")));
    }
}

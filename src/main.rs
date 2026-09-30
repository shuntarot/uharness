//! Entry point of `veryl-harness`. The work is in `harness::cli`.
//!
//! Keep startup light: veryl calls `--info` with a 500 ms timeout.

fn main() -> std::process::ExitCode {
    harness::cli::main()
}

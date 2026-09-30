//! Helpers shared by the tests.

use std::path::Path;

/// Runs `veryl check` on a generated harness (the output directory of `gen`).
///
/// The output is its own Veryl project, so analyzing the DUT project does not
/// see it. Running `gen` again does not compile the output either.
///
/// Warnings count as failures, as in `veryl check`.
///
/// This does not call the `veryl` on `PATH`, so the test cannot pick up a
/// version different from the linked analyzer. Each call runs on a new thread:
/// the analyzer keeps state per thread, and a second project on the same thread
/// fails with `duplicated_identifier`.
pub fn veryl_check(project: &Path) -> Result<(), String> {
    let toml = project.join("Veryl.toml");
    std::thread::spawn(move || {
        let mut metadata = veryl_metadata::Metadata::load(&toml)
            .map_err(|e| format!("{}: {e:?}", toml.display()))?;
        veryl::cmd_check::CmdCheck::new(veryl::OptCheck { files: Vec::new() })
            .exec(&mut metadata)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    })
    .join()
    .unwrap_or_else(|_| Err("veryl check panicked".to_string()))
}

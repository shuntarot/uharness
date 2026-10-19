//! Puts the commit into `--version`, so a binary says which source it was
//! built from.

use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    // Only a checkout of this repository says which commit it is. A package
    // unpacked inside some other repository would get that repository's.
    let ours = git(&["ls-files", "--error-unmatch", "Cargo.toml"]).is_some();
    let full = match git(&["log", "-1", "--format=%h %cs"]) {
        Some(commit) if ours && !commit.is_empty() => format!("{version} ({commit})"),
        _ => version,
    };
    println!("cargo:rustc-env=HNS_VERSION={full}");
    if let Some(dir) = git(&["rev-parse", "--git-dir"]).filter(|_| ours) {
        for path in ["HEAD", "refs", "packed-refs"] {
            println!("cargo:rerun-if-changed={dir}/{path}");
        }
    }
}

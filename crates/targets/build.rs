//! Target descriptions are embedded into this crate with rust-embed
//! (`src/lib.rs`).
//!
//! Adding or removing a description must trigger a rebuild. rust-embed only
//! tracks edits to files it already embedded. Without this script, tests
//! would pass against the old embedded set.
fn main() {
    // For a directory, cargo also watches files being added or removed.
    // `targets-private/` always exists because its README is tracked. A
    // missing path would count as "changed" on every build.
    println!("cargo:rerun-if-changed=targets");
    println!("cargo:rerun-if-changed=targets-private");
}

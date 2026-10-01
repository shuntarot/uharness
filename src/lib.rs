//! Errors carry a miette `NamedSource`, so they are large. Boxing them only
//! adds a level of indirection for the reader, so they are returned as is.
#![allow(clippy::result_large_err)]

//! veryl-harness: generates a harness that runs a single RTL block on real
//! hardware.

pub mod bundle;
pub mod check;
pub mod cli;
pub mod clock;
pub mod contract;
pub mod dut;
pub mod emit;
pub mod feasibility;
// `gen` is reserved in Rust 2024, so the module is `generate`. The subcommand
// is still `gen`.
pub mod generate;
pub mod heartbeat;
pub mod json;
pub mod manifest;
pub mod plan;
pub mod regmap;
pub mod sim;
/// Target resolution lives in `hns-targets` because the host reads the same
/// descriptions. This only keeps the old module name.
pub use hns_targets as target;
pub mod terminator;
pub mod unconnected;
pub mod vendor;

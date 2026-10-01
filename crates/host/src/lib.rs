//! Host side: reaches the harness window.
//!
//! There are two layers. Only the lower one depends on USB, so the upper one runs
//! in tests without a board.
//!
//! | Layer | Contents | Tested |
//! |---|---|---|
//! | `Bridge` / `Frame` | TAP walks, DR layout, pipeline semantics | in unit tests |
//! | `JtagIo` | "shift these bits and return TDO" | on real hardware only |
//!
//! The split lets tests catch mistakes that would otherwise show up only on
//! real hardware.

pub mod bitstream;
pub mod bridge;
pub mod config;
pub mod dma;
pub mod frame;
pub mod ftdi;
pub mod mpsse;
pub mod pcie;
pub mod sim;
pub mod svf;

#[cfg(test)]
mod model;

pub use bridge::{Batch, Bridge, Error, JtagIo, ReadHandle, Reads};
pub use frame::{Frame, Op, Response};
pub use ftdi::Ftdi;
pub use mpsse::{Chan, Mpsse, ProbeConfig};

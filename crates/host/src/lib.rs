//! Host side: reaches the harness window.
//!
//! Over JTAG there are two layers. Only the lower one depends on USB, so the upper
//! one runs in tests without a board.
//!
//! | Layer | Contents | Tested |
//! |---|---|---|
//! | `Bridge` / `Frame` | TAP walks, DR layout, pipeline semantics | in unit tests |
//! | `JtagIo` | "shift these bits and return TDO" | on real hardware only |
//!
//! The split lets tests catch mistakes that would otherwise show up only on
//! real hardware.

pub mod adi;
pub mod bitstream;
pub mod bridge;
pub mod config;
pub mod disasm;
pub mod dm;
pub mod dma;
pub mod elf;
pub mod frame;
pub mod ftdi;
pub mod hns2;
pub mod mpsse;
pub mod pcie;
pub mod ps7;
pub mod reprogram;
pub mod rtt;
pub mod semihosting;
pub mod svf;
pub mod sysmon;

#[cfg(test)]
mod model;

pub use bridge::{Batch, Bridge, Error, JtagIo, ReadHandle, Reads};
pub use frame::{Frame, Op, Response};
pub use ftdi::Ftdi;
pub use mpsse::{Chan, Mpsse, ProbeConfig};

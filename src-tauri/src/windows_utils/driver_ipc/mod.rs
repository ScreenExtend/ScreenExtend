mod client;
mod core;
mod driver_client;
pub mod host;
pub mod sync;
pub mod v2;

pub use client::{Client, ClosedWatch, Handshake};
pub use core::*;
/// Connect and handshake errors, so a caller can tell why a connection failed: a pipe whose
/// single instance is taken is a busy driver, not a dead one.
pub use driver_client::error;
pub use driver_client::DriverClient;

#[cfg(test)]
pub(crate) mod mock;

pub static DEFAULT_PIPE_NAME: &str = "virtualdisplaydriver";

mod client;
mod core;
mod driver_client;
pub mod sync;

pub use client::Client;
pub use core::*;
/// The connect/handshake errors, re-exported so a caller outside this module can tell *why* a
/// connection failed rather than only that it did. `virtual_display` needs the distinction: a
/// pipe whose single instance is already taken is a driver that is busy, not one that is dead.
pub use driver_client::error;
pub use driver_client::DriverClient;

#[cfg(test)]
mod mock;

pub static DEFAULT_PIPE_NAME: &str = "virtualdisplaydriver";

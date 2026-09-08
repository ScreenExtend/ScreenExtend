//! Windows.Graphics.Capture, vendored.
//!
//! A fork of the `windows-capture` crate cut down to what ScreenExtend uses: monitor capture into
//! a D3D11 texture. Provenance, the divergences from upstream and what was removed are recorded
//! in `FORK.md` next to this file.

// Upstream's crate-root lint policy is deliberately not carried over: `clippy::cargo` and
// `clippy::multiple_crate_versions` judge this crate's whole dependency graph, and
// `clippy::nursery` is unstable across toolchains. `missing_docs` is kept, since the module is
// documented and staying that way is cheap.
#![warn(missing_docs)]

/// Exported for the trait bounds
pub use windows::Graphics::Capture::GraphicsCaptureItem;

/// Contains safe wrapper for WinRT initialization.
pub(crate) mod winrt;

/// Contains the main capture functionality, including the
/// [`crate::capture::GraphicsCaptureApiHandler`] trait and related types.
pub mod capture;
/// Internal module for Direct3D 11 related functionality.
pub mod d3d11;
/// Image encoding for `Frame::save_as_image`.
pub mod encoder;
/// Contains the [`crate::frame::Frame`] struct and related types for representing captured frames.
pub mod frame;
/// Contains the types and functions related to the Graphics Capture API.
pub mod graphics_capture_api;
/// Contains functionality for working with monitors and screen information:
/// [`crate::monitor::Monitor`].
pub mod monitor;
/// Contains the [`crate::settings::Settings`] struct and related types for configuring capture
/// settings.
pub mod settings;

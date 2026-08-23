//! System-audio capture — macOS stub (PRD §7.10).
//!
//! Same public surface as the Windows backend so the Tauri command list and
//! `streamer::platform::start_audio_capture` are identical across OSes; returns a clear
//! "unsupported" error for now. The UI toggle is shown but disabled with an explanatory
//! tooltip, so the feature reads as unimplemented rather than broken.
//!
//! Eventual path: ScreenCaptureKit already captures system audio, and
//! `macos_utils/streamer/sck.rs` exists — an `SCStreamOutput` with `.audio` output type would
//! feed the same Opus encoder / transport. Not implemented now.

use anyhow::{bail, Result};

use crate::streamer::audio::AudioCapture;

pub fn start_capture() -> Result<AudioCapture> {
    bail!("system audio streaming is not supported on macOS yet")
}

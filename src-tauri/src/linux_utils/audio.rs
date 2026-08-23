//! System-audio capture — Linux stub (PRD §7.10).
//!
//! Same public surface as the Windows backend so the command list and
//! `streamer::platform::start_audio_capture` are identical across OSes; returns a clear
//! "unsupported" error. The UI toggle is shown but disabled with an explanatory tooltip.
//!
//! A future implementation would capture a PipeWire/PulseAudio monitor source and feed the
//! same Opus encoder / transport, mirroring how `linux_utils/streamer.rs` stubs the rest of
//! the pipeline.

use anyhow::{bail, Result};

use crate::streamer::audio::AudioCapture;

pub fn start_capture() -> Result<AudioCapture> {
    bail!("system audio streaming is not supported on Linux yet")
}

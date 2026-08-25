//! Windows system-audio capture + Opus encode (PRD §4). Mirrors the layout/conventions of
//! `windows_utils/streamer/`. Public surface: [`start_capture`], returning the cross-platform
//! [`crate::streamer::audio::AudioCapture`] (packet receiver + stop closure + format).
//!
//! The capture runs on a dedicated OS thread (COM MTA + MMCSS), driven by a silent render
//! companion, and Opus-encodes 5 ms frames — see `loopback.rs`, `silence.rs`, `AUDIO_NOTES.md`.

pub mod device;
pub mod format;
pub mod guards;
pub mod loopback;
pub mod silence;

// The Opus FFI shim (`opus_sys`) and encoder wrapper (`encoder`) used to live here; they moved
// to the OS-independent `crate::streamer::audio` so the macOS backend reuses them instead of
// duplicating the libopus binding (PRD macos §6).

#[cfg(test)]
mod test;

use anyhow::Result;

use crate::streamer::audio::AudioCapture;

/// Start the single host-wide system-audio capture. Called through
/// `streamer::platform::start_audio_capture`; the reference-counted `AudioHub` owns the result.
pub fn start_capture() -> Result<AudioCapture> {
    loopback::start()
}

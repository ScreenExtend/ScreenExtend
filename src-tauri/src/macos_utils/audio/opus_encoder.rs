//! Thin re-export of the shared libopus encoder (PRD §6).
//!
//! There is no macOS-specific Opus code: the FFI shim (`opus_sys`) and the encoder wrapper
//! (`encoder`) are OS-independent and live in `crate::streamer::audio`, shared with the Windows
//! WASAPI backend. libopus itself is cross-platform C; only the bundled library file name differs
//! (`libopus.dylib` vs `libopus.dll`), which the loader handles. This module just re-exports the
//! shared types so the macOS capture code reads against a local name, per the PRD's module layout.

pub use crate::streamer::audio::encoder::{OpusEncoder, OpusEncoderConfig, FRAME_SAMPLES};

/// Interleaved f32 samples in one 5 ms stereo Opus frame (`FRAME_SAMPLES` per channel × 2).
pub const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * super::format::OUT_CHANNELS;

pub use crate::streamer::audio::encoder::{OpusEncoder, OpusEncoderConfig, FRAME_SAMPLES};

pub const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * super::format::OUT_CHANNELS;

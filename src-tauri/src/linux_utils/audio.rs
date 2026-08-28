use anyhow::{bail, Result};

use crate::streamer::audio::AudioCapture;

pub fn start_capture() -> Result<AudioCapture> {
    bail!("system audio streaming is not supported on Linux yet")
}

use anyhow::{bail, Result};

use crate::streamer::audio::{AudioCapture, AudioSink};

pub fn start_capture(_sink: AudioSink) -> Result<AudioCapture> {
    bail!("system audio streaming is not supported on Linux yet")
}

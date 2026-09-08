pub mod device;
pub mod engine;
pub mod format;
pub mod guards;
pub mod loopback;
pub mod silence;

#[cfg(test)]
mod test;

use anyhow::Result;

use crate::streamer::audio::{AudioCapture, AudioSink};

pub fn start_capture(sink: AudioSink) -> Result<AudioCapture> {
    loopback::start(sink)
}

pub mod device;
pub mod format;
pub mod guards;
pub mod loopback;
pub mod silence;

#[cfg(test)]
mod test;

use anyhow::Result;

use crate::streamer::audio::AudioCapture;

pub fn start_capture() -> Result<AudioCapture> {
    loopback::start()
}

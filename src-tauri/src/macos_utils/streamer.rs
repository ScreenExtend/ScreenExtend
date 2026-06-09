use anyhow::{Result, bail};

use crate::streamer::platform::{BackendConfig, EncoderBackend};

pub struct VideoToolboxBackend;

impl EncoderBackend for VideoToolboxBackend {
    fn new(_config: BackendConfig) -> Result<Self> {
        bail!("macOS VideoToolbox encoder backend not implemented")
    }

    fn encode(&mut self, _force_idr: bool) -> Result<Vec<u8>> {
        bail!("macOS VideoToolbox encoder backend not implemented")
    }

    fn set_bitrate(&mut self, _bps: u32) -> Result<()> {
        bail!("macOS VideoToolbox encoder backend not implemented")
    }
}

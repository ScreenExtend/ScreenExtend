use anyhow::{Result, bail};

use crate::streamer::platform::{BackendConfig, EncoderBackend};

pub struct AmfBackend;

impl EncoderBackend for AmfBackend {
    fn new(_config: BackendConfig) -> Result<Self> {
        bail!("AMD AMF encoder backend not implemented")
    }

    fn encode(&mut self, _force_idr: bool) -> Result<Vec<u8>> {
        bail!("AMD AMF encoder backend not implemented")
    }

    fn set_bitrate(&mut self, _bps: u32) -> Result<()> {
        bail!("AMD AMF encoder backend not implemented")
    }
}

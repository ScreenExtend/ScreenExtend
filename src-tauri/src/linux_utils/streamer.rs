use anyhow::{Result, bail};

use crate::streamer::platform::{BackendConfig, EncoderBackend};

pub struct VaapiBackend;

impl EncoderBackend for VaapiBackend {
    fn new(_config: BackendConfig) -> Result<Self> {
        bail!("Linux VAAPI/NVENC encoder backend not implemented")
    }

    fn encode(&mut self, _force_idr: bool) -> Result<Vec<u8>> {
        bail!("Linux encoder backend not implemented")
    }

    fn set_bitrate(&mut self, _bps: u32) -> Result<()> {
        bail!("Linux encoder backend not implemented")
    }
}

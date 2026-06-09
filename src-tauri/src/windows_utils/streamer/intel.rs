use anyhow::{Result, bail};

use crate::streamer::platform::{BackendConfig, EncoderBackend};

pub struct QuickSyncBackend;

impl EncoderBackend for QuickSyncBackend {
    fn new(_config: BackendConfig) -> Result<Self> {
        bail!("Intel Quick Sync encoder backend not implemented")
    }

    fn encode(&mut self, _force_idr: bool) -> Result<Vec<u8>> {
        bail!("Intel Quick Sync encoder backend not implemented")
    }

    fn set_bitrate(&mut self, _bps: u32) -> Result<()> {
        bail!("Intel Quick Sync encoder backend not implemented")
    }
}

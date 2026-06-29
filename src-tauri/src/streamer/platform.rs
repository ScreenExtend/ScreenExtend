#![allow(dead_code, unused_imports)]

use anyhow::Result;

use super::config::Config;

pub fn set_dpi_awareness() {
    #[cfg(target_os = "windows")]
    crate::windows_utils::streamer::capture::set_dpi_awareness();
}

pub fn apply_process_tuning() {
    #[cfg(target_os = "windows")]
    crate::windows_utils::streamer::tuning::apply_process_tuning();
    #[cfg(target_os = "macos")]
    crate::macos_utils::streamer::tuning::apply_process_tuning();
}

pub fn tune_transport_thread() {
    #[cfg(target_os = "windows")]
    crate::windows_utils::streamer::tuning::tune_transport_thread();
    #[cfg(target_os = "macos")]
    crate::macos_utils::streamer::qos::pin_current_thread_user_initiated();
}

pub fn probe_capture(monitor: u32, path: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        crate::windows_utils::streamer::capture::probe_to_png(monitor, path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (monitor, path);
        anyhow::bail!("capture probe is only implemented on Windows")
    }
}

pub fn probe_dxgi(monitor: u32, path: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        crate::windows_utils::streamer::dxgi::probe_to_bmp(monitor, path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (monitor, path);
        anyhow::bail!("DXGI duplication probe is only implemented on Windows")
    }
}

pub fn probe_encode(config: &Config, path: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use crate::streamer::config::EncoderVendor;
        match config.encoder_vendor {
            EncoderVendor::Intel => {
                crate::windows_utils::streamer::intel::encoder::probe_encode(config, path)
            }
            EncoderVendor::Auto | EncoderVendor::Nvidia => {
                crate::windows_utils::streamer::nvidia::encoder::probe_encode(config, path)
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (config, path);
        anyhow::bail!("encode probe is only implemented on Windows")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BackendConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

pub trait EncoderBackend: Send {
    fn new(config: BackendConfig) -> Result<Self>
    where
        Self: Sized;

    fn encode(&mut self, force_idr: bool) -> Result<Vec<u8>>;

    fn set_bitrate(&mut self, bps: u32) -> Result<()>;
}

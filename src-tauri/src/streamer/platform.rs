#![allow(dead_code, unused_imports)]

use anyhow::Result;

use super::config::Config;

pub fn set_dpi_awareness() {
    #[cfg(target_os = "windows")]
    crate::windows_utils::streamer::capture::set_dpi_awareness();
}

pub fn max_display_dpr() -> f64 {
    #[cfg(target_os = "macos")]
    {
        2.0
    }
    #[cfg(not(target_os = "macos"))]
    {
        4.0
    }
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
        let vendor = if config.disable_gpu_encode {
            EncoderVendor::Software
        } else {
            config.encoder_vendor
        };
        match vendor {
            EncoderVendor::Intel => {
                crate::windows_utils::streamer::intel::encoder::probe_encode(config, path)
            }
            EncoderVendor::Software => {
                crate::windows_utils::streamer::x264::encoder::probe_encode(config, path)
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

/// Start the single host-wide system-audio capture, dispatching to the per-OS backend
/// (Windows: WASAPI loopback + Opus; macOS/Linux: an unsupported stub). Called by the
/// reference-counted `streamer::audio::AudioHub` on the first audio-enabled subscriber.
pub fn start_audio_capture() -> Result<crate::streamer::audio::AudioCapture> {
    #[cfg(target_os = "windows")]
    {
        crate::windows_utils::audio::start_capture()
    }
    #[cfg(target_os = "macos")]
    {
        crate::macos_utils::audio::start_capture()
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux_utils::audio::start_capture()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("system audio capture is not supported on this platform")
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

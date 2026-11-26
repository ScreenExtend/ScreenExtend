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

pub fn display_scale_steps() -> &'static [u32] {
    #[cfg(target_os = "windows")]
    {
        &crate::windows_utils::streamer::capture::DPI_PERCENT_VALUES
    }
    #[cfg(target_os = "macos")]
    {
        const MACOS: &[u32] = &[100, 200];
        MACOS
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        const ANY: &[u32] = &[];
        ANY
    }
}

pub fn display_dpr_ladder() -> Vec<f64> {
    let cap = max_display_dpr();
    let steps = display_scale_steps();
    if steps.is_empty() {
        let mut ratios = Vec::new();
        let mut r = 1.0_f64;
        while r <= cap + 1e-9 {
            ratios.push(r);
            r += 0.5;
        }
        return ratios;
    }
    steps
        .iter()
        .map(|&s| s as f64 / 100.0)
        .filter(|&r| r >= 1.0 && r <= cap + 1e-9)
        .collect()
}

pub fn snap_display_dpr(requested: f64) -> f64 {
    if !requested.is_finite() {
        return 1.0;
    }
    let cap = max_display_dpr();
    let want = requested.clamp(1.0, cap);
    let steps = display_scale_steps();
    if steps.is_empty() {
        return want;
    }
    steps
        .iter()
        .map(|&s| s as f64 / 100.0)
        .filter(|&r| r >= 1.0 && r <= cap + 1e-9 && r <= want + 1e-6)
        .fold(1.0_f64, f64::max)
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
    {
        use crate::macos_utils::streamer::{qos, tune};
        if tune::enabled("transport_user_interactive") {
            tune::record_rc(
                "transport_user_interactive",
                qos::pin_current_thread_user_interactive(),
            );
        } else if tune::enabled("transport_qos") {
            tune::record_rc("transport_qos", qos::pin_current_thread_user_initiated());
        } else {
            tune::record_skipped("transport_qos");
        }
    }
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
            EncoderVendor::Amd => {
                crate::windows_utils::streamer::amd::encoder::probe_encode(config, path)
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

pub fn start_audio_capture(
    sink: crate::streamer::audio::AudioSink,
) -> Result<crate::streamer::audio::AudioCapture> {
    #[cfg(target_os = "windows")]
    {
        crate::windows_utils::audio::start_capture(sink)
    }
    #[cfg(target_os = "macos")]
    {
        crate::macos_utils::audio::start_capture(sink)
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux_utils::audio::start_capture(sink)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("system audio capture is not supported on this platform")
    }
}

/// What an encoder did with a runtime bitrate request, so an ignored request is never logged
/// as a bitrate change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitrateApplied {
    Applied,
    /// The encoder is in explicit constant-QP mode, so bitrate is not a control input.
    IgnoredConstantQp,
    /// The engine cannot change its bitrate in a running session (NVENC without the
    /// dynamic-bitrate capability); the session keeps the rate it was opened with.
    #[cfg(windows)]
    Unsupported,
    /// Applied, but held to the encoder's own limit (Quick Sync's rate fields at the multiplier
    /// fixed at `Init`).
    #[cfg(windows)]
    Clamped {
        applied_bps: u32,
    },
}

/// What the local end of the nominated ICE pair is physically riding. The medium matters (Luna
/// Display publishes 11.3 ms mean over USB against 22.4 ms over Wi-Fi), and a USB-tethered phone
/// joins as an ordinary IP interface, so the address alone does not say which a session is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMedium {
    Wired,
    Wireless,
    Other,
}

impl LinkMedium {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkMedium::Wired => "wired",
            LinkMedium::Wireless => "wireless",
            LinkMedium::Other => "other",
        }
    }
}

/// The medium behind a local IP address, or `None` when the platform cannot say. Called once per
/// session, where the nominated candidate pair is read, never on a hot path.
pub fn link_medium_for_local_ip(ip: &str) -> Option<LinkMedium> {
    #[cfg(windows)]
    {
        crate::windows_utils::networking::link_medium_for_local_ip(ip)
    }
    #[cfg(target_os = "macos")]
    {
        crate::macos_utils::networking::link_medium_for_local_ip(ip)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = ip;
        None
    }
}

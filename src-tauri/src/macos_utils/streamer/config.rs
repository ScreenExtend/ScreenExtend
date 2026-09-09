use crate::streamer::config::{Config, H264Profile, ScalePercent};

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub profile: H264Profile,
    /// Explicit constant-QP rate control. Disables bitrate adaptation entirely.
    pub qp: Option<u8>,
    /// Quality ceiling under bitrate control: never spend bits to go below this QP.
    pub min_qp: Option<u8>,
    pub intra_refresh: bool,
    /// The GPU driving the captured display, from `display::metal_registry_id`. `None` means
    /// "let VideoToolbox choose", which is right on a single-GPU Mac and a coin toss on any other.
    pub gpu_registry_id: Option<u64>,
    /// The user's "software encode" setting. Until this landed, macOS read the flag nowhere at
    /// all - `streamer/platform.rs` consumes it under `cfg(windows)` only - so the Settings
    /// toggle and `serve --software-encode` were both inert on this platform.
    pub software_encode: bool,
}

pub fn scaled_dims(native_w: u32, native_h: u32, scale: ScalePercent) -> (u32, u32) {
    if scale.is_native() || native_w == 0 || native_h == 0 {
        return (native_w & !1, native_h & !1);
    }
    let w = scale.apply(native_w).max(2) & !1;
    let h = scale.apply(native_h).max(2) & !1;
    (w, h)
}

pub fn live_encoder_config(
    native_w: u32,
    native_h: u32,
    refresh_hz: u32,
    cfg: &Config,
) -> EncoderConfig {
    let fps = if let Some(f) = cfg.fps {
        f.clamp(15, 500)
    } else {
        // The display's own rate, not a floor of 60. A 30 Hz display or a client that asked for
        // 30 was being captured and encoded at 60 anyway: twice the frames, none of them new,
        // and twice the encoder work competing with the ones that were. The `refresh_hz == 0`
        // line above is what covers a display that reports nothing sensible.
        let refresh = if refresh_hz == 0 { 60 } else { refresh_hz };
        let max_fps = cfg.max_fps.clamp(15, 500);
        refresh.clamp(15, max_fps)
    };

    let (width, height) = scaled_dims(native_w, native_h, cfg.scale);

    let computed =
        ((width as u64 * height as u64 * fps as u64) / 10).clamp(6_000_000, 30_000_000) as u32;
    let bitrate = cfg
        .max_bitrate_kbps
        .map(|kbps| kbps.saturating_mul(1000))
        .unwrap_or(computed);

    EncoderConfig {
        width,
        height,
        fps,
        bitrate_bps: bitrate,
        max_bitrate_bps: bitrate,
        profile: cfg.h264_profile,
        qp: cfg.qp,
        min_qp: cfg.quality_qp,
        intra_refresh: cfg.intra_refresh,
        gpu_registry_id: None,
        // The live handle where a host exists, the snapshot where one does not — the probe
        // harness has no `AppState`, and `--software-encode` is the source of truth there. Read
        // here, at encoder-selection time, so the Settings toggle changes the *next* session's
        // encoder without every streamer having to be drained and rebuilt.
        software_encode: cfg.software_encode_now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_30hz_display_is_encoded_at_30() {
        let cfg = Config::default();
        assert_eq!(live_encoder_config(1920, 1080, 30, &cfg).fps, 30);
    }

    #[test]
    fn a_display_that_reports_nothing_falls_back_to_60() {
        let cfg = Config::default();
        assert_eq!(live_encoder_config(1920, 1080, 0, &cfg).fps, 60);
    }

    #[test]
    fn the_software_encode_flag_reaches_the_encoder() {
        let mut cfg = Config::default();
        assert!(!live_encoder_config(1920, 1080, 60, &cfg).software_encode);
        cfg.disable_gpu_encode = true;
        assert!(live_encoder_config(1920, 1080, 60, &cfg).software_encode);
    }

    #[test]
    fn a_live_toggle_beats_the_snapshot_the_config_was_built_with() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // The defect this closes: the flag was copied by value when the streamer was built, so a
        // user flipping the setting changed an atomic nothing downstream ever read again.
        let flag = Arc::new(AtomicBool::new(false));
        let cfg = Config {
            disable_gpu_encode: true,
            gpu_encode_toggle: Some(Arc::clone(&flag)),
            ..Config::default()
        };
        assert!(!live_encoder_config(1920, 1080, 60, &cfg).software_encode);
        flag.store(true, Ordering::Relaxed);
        assert!(live_encoder_config(1920, 1080, 60, &cfg).software_encode);
    }

    #[test]
    fn max_fps_still_caps_a_fast_display() {
        let cfg = Config {
            max_fps: 60,
            ..Config::default()
        };
        assert_eq!(live_encoder_config(1920, 1080, 120, &cfg).fps, 60);
    }
}

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
    fn max_fps_still_caps_a_fast_display() {
        let cfg = Config {
            max_fps: 60,
            ..Config::default()
        };
        assert_eq!(live_encoder_config(1920, 1080, 120, &cfg).fps, 60);
    }
}

//! Encoder-config derivation, mirroring `windows_utils::streamer::pipeline`'s
//! `live_encoder_config` / `scaled_dims` so the two platforms apply the same
//! scale/fps/bitrate policy from the shared [`Config`].

use crate::streamer::config::{Config, H264Profile, ScalePercent};

/// Everything the VideoToolbox [`super::encoder::Encoder`] needs to configure a
/// `VTCompressionSession` (PRD §13). The structural twin of the Windows
/// `nvidia::encoder::EncoderConfig`.
#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub profile: H264Profile,
    pub qp: Option<u8>,
    pub intra_refresh: bool,
}

/// Round to even dimensions and apply the scale percentage (H.264 wants even
/// width/height). Identical policy to the Windows backend.
///
/// We deliberately do NOT clamp tiny sizes up to the HW encoder's minimum: the
/// user's chosen (small) resolution is respected, and the encoder transparently
/// drops to the software H.264 encoder for sizes the hardware encoder won't serve
/// (below ~640×360 on the old Intel test box) — see `encoder::Encoder::new`.
///
/// The downscale itself is performed **at capture**, not in VideoToolbox: the
/// CGDisplayStream (and the SCK config) are created at these scaled dimensions, so
/// the window-server composites straight to the encode size and VideoToolbox
/// always receives a buffer already at its configured resolution. That is why the
/// encoder needs no `PixelTransferProperties` / `VTPixelTransferSession` resize
/// pass — adding one would only duplicate, on our side, work the compositor
/// already did for free.
pub fn scaled_dims(native_w: u32, native_h: u32, scale: ScalePercent) -> (u32, u32) {
    if scale.is_native() || native_w == 0 || native_h == 0 {
        return (native_w & !1, native_h & !1);
    }
    let w = scale.apply(native_w).max(2) & !1;
    let h = scale.apply(native_h).max(2) & !1;
    (w, h)
}

/// Derive the live encoder config from the native display geometry and the
/// shared session [`Config`]. Mirrors `windows_utils`'s `live_encoder_config`.
pub fn live_encoder_config(
    native_w: u32,
    native_h: u32,
    refresh_hz: u32,
    cfg: &Config,
) -> EncoderConfig {
    let fps = if let Some(f) = cfg.fps {
        f.clamp(15, 500)
    } else {
        let refresh = if refresh_hz == 0 { 60 } else { refresh_hz };
        let max_fps = cfg.max_fps.clamp(15, 500);
        refresh.clamp(60.min(max_fps), max_fps)
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
        intra_refresh: cfg.intra_refresh,
    }
}

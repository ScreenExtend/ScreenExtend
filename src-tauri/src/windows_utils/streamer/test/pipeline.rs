use crate::streamer::config::{Config, ScalePercent};
use crate::windows_capture::capture::GraphicsCaptureApiError;
use crate::windows_utils::streamer::pipeline::{
    live_encoder_config, scaled_dims, wgc_start_error, StaleMonitor,
};

/// A capture item that cannot be created from the monitor handle is the one WGC start failure
/// `start_on_monitor` retries before falling to DXGI; every other failure goes straight there.
#[test]
fn only_an_item_conversion_failure_is_classified_stale() {
    let stale = wgc_start_error(GraphicsCaptureApiError::ItemConvertFailed);
    assert!(stale.downcast_ref::<StaleMonitor>().is_some());
    assert!(format!("{stale:#}").contains("GraphicsCaptureItem"));

    let other = wgc_start_error(GraphicsCaptureApiError::FailedToInitWinRT);
    assert!(other.downcast_ref::<StaleMonitor>().is_none());
}

#[test]
fn scale_100_is_native_passthrough() {
    assert_eq!(
        scaled_dims(1920, 1080, ScalePercent::new(100)),
        (1920, 1080)
    );
    assert_eq!(
        scaled_dims(1921, 1081, ScalePercent::new(100)),
        (1920, 1080)
    );
}

#[test]
fn scale_90_of_1080p() {
    assert_eq!(scaled_dims(1920, 1080, ScalePercent::new(90)), (1728, 972));
}

#[test]
fn scale_rounds_to_even() {
    assert_eq!(scaled_dims(1920, 1080, ScalePercent::new(75)), (1440, 810));
    assert_eq!(scaled_dims(1366, 768, ScalePercent::new(50)), (682, 384));
}

#[test]
fn scale_clamps_out_of_range() {
    assert_eq!(ScalePercent::new(150).percent(), 100);
    assert_eq!(ScalePercent::new(0).percent(), ScalePercent::MIN);
    assert!(ScalePercent::new(100).is_native());
    assert!(!ScalePercent::new(99).is_native());
}

#[test]
fn scale_parse_accepts_percent_and_bare() {
    assert_eq!(ScalePercent::parse("90"), Some(ScalePercent::new(90)));
    assert_eq!(ScalePercent::parse(" 80% "), Some(ScalePercent::new(80)));
    assert_eq!(ScalePercent::parse("abc"), None);
}

#[test]
fn scale_never_produces_zero_dims() {
    let (w, h) = scaled_dims(8, 8, ScalePercent::new(ScalePercent::MIN));
    assert!(w >= 2 && h >= 2);
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn encoder_rate_follows_the_display_and_the_configured_ceiling() {
    let mut cfg = Config::default();
    cfg.fps = None;
    cfg.max_fps = 20;
    let enc = live_encoder_config(1920, 1080, 60, &cfg);
    assert_eq!(enc.fps, 20, "the ceiling wins over the display rate");

    cfg.max_fps = 144;
    let enc = live_encoder_config(1920, 1080, 120, &cfg);
    assert_eq!(enc.fps, 120);

    // A 30 Hz display composes 30 times a second; encoding 60 only produces duplicates.
    cfg.max_fps = 240;
    let enc = live_encoder_config(1920, 1080, 30, &cfg);
    assert_eq!(enc.fps, 30);

    // An unreported rate still assumes 60.
    let enc = live_encoder_config(1920, 1080, 0, &cfg);
    assert_eq!(enc.fps, 60);
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn quality_slider_is_a_ceiling_not_a_rate_control_mode() {
    // The per-device quality slider used to land in `Config::qp`, which put NVENC in CONSTQP
    // and turned every runtime bitrate change into a silent no-op.
    let mut cfg = Config::default();
    cfg.quality_qp = Some(28);
    let enc = live_encoder_config(1920, 1080, 60, &cfg);
    assert_eq!(enc.qp, None, "the slider must not select constant-QP");
    assert_eq!(enc.min_qp, Some(28), "the slider is a quality ceiling");
    assert!(enc.bitrate_bps > 0, "bitrate control stays active");
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn explicit_qp_still_selects_constant_qp() {
    let mut cfg = Config::default();
    cfg.qp = Some(26);
    let enc = live_encoder_config(1920, 1080, 60, &cfg);
    assert_eq!(enc.qp, Some(26));
    assert_eq!(enc.min_qp, None);
}

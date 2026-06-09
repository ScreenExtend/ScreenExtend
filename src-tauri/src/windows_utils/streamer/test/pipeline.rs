use crate::streamer::config::ScalePercent;
use crate::windows_utils::streamer::pipeline::scaled_dims;

#[test]
fn scale_100_is_native_passthrough() {
    assert_eq!(scaled_dims(1920, 1080, ScalePercent::new(100)), (1920, 1080));
    assert_eq!(scaled_dims(1921, 1081, ScalePercent::new(100)), (1920, 1080));
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

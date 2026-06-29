use std::time::{Duration, Instant};

use crate::streamer::bitrate::{BitrateController, estimate_from_loss};

const MIN: u32 = 1_000_000;
const MAX: u32 = 40_000_000;

fn t0() -> Instant {
    Instant::now()
}

#[test]
fn first_sample_is_emitted_and_clamped() {
    let mut c = BitrateController::new(MIN, MAX);
    let now = t0();
    let out = c.update(80_000_000, now).expect("first sample emits");
    assert_eq!(out, MAX, "above-max sample clamps to max");
    assert_eq!(c.current_target(), Some(MAX));
}

#[test]
fn clamps_below_min() {
    let mut c = BitrateController::new(MIN, MAX);
    let out = c.update(100_000, t0()).expect("first emits");
    assert_eq!(out, MIN, "below-min sample clamps to min");
}

#[test]
fn small_changes_are_suppressed_by_hysteresis() {
    let mut c = BitrateController::with_params(MIN, MAX, 1.0, 0.10, Duration::ZERO);
    let now = t0();
    assert_eq!(c.update(10_000_000, now), Some(10_000_000));
    assert_eq!(c.update(10_500_000, now + Duration::from_secs(1)), None);
    assert_eq!(
        c.update(11_200_000, now + Duration::from_secs(2)),
        Some(11_200_000)
    );
}

#[test]
fn rate_limit_blocks_rapid_changes() {
    let mut c = BitrateController::with_params(MIN, MAX, 1.0, 0.10, Duration::from_millis(500));
    let now = t0();
    assert_eq!(c.update(10_000_000, now), Some(10_000_000));
    assert_eq!(c.update(20_000_000, now + Duration::from_millis(100)), None);
    assert_eq!(
        c.update(20_000_000, now + Duration::from_millis(600)),
        Some(20_000_000)
    );
}

#[test]
fn ewma_smooths_a_spike() {
    let mut c = BitrateController::with_params(MIN, MAX, 0.4, 0.10, Duration::ZERO);
    let now = t0();
    assert_eq!(c.update(10_000_000, now), Some(10_000_000));
    let out = c
        .update(30_000_000, now + Duration::from_secs(1))
        .expect("crosses threshold");
    assert_eq!(out, 18_000_000, "EWMA dampens the spike");
}

#[test]
fn converges_on_steady_input() {
    let mut c = BitrateController::with_params(MIN, MAX, 0.5, 0.10, Duration::ZERO);
    let now = t0();
    let mut last = c.update(5_000_000, now).unwrap();
    for i in 1..20 {
        if let Some(v) = c.update(5_000_000, now + Duration::from_millis(i * 100)) {
            last = v;
        }
    }
    assert_eq!(last, 5_000_000, "steady input converges to that value");
}

#[test]
fn estimate_backs_off_on_heavy_loss() {
    let est = estimate_from_loss(10_000_000, 9_500_000, 0.20);
    assert!(est < 10_000_000, "heavy loss must reduce estimate, got {est}");
    assert_eq!(est, 9_000_000);
}

#[test]
fn estimate_probes_up_when_healthy() {
    let est = estimate_from_loss(10_000_000, 10_000_000, 0.0);
    assert!(est > 10_000_000, "healthy link should probe upward, got {est}");
    assert_eq!(est, 10_800_000);
}

#[test]
fn estimate_probe_is_capped_by_delivered_rate() {
    let est = estimate_from_loss(10_000_000, 2_000_000, 0.0);
    assert_eq!(est, 10_000_000, "probe capped near delivered rate");
}

#[test]
fn estimate_holds_in_neutral_band() {
    let est = estimate_from_loss(10_000_000, 10_000_000, 0.05);
    assert_eq!(est, 10_000_000, "moderate loss holds steady");
}

#[test]
fn cut_emits_when_symmetric_threshold_would_stall() {
    let mut c = BitrateController::new(MIN, MAX);
    let now = t0();
    let start = c.update(10_000_000, now).expect("first sample emits");
    assert_eq!(start, 10_000_000);
    let cut = c.update(8_500_000, now + Duration::from_millis(50));
    assert!(cut.is_some(), "cut must emit under the asymmetric threshold");
    assert!(cut.unwrap() < start, "emitted value must be a reduction: {cut:?}");
}

#[test]
fn upward_probe_still_hysteretic_under_asymmetric_cut() {
    let mut c = BitrateController::new(MIN, MAX);
    let now = t0();
    assert_eq!(c.update(10_000_000, now), Some(10_000_000));
    assert_eq!(c.update(10_300_000, now + Duration::from_secs(1)), None);
}

#[test]
fn upward_probe_still_rate_limited_under_asymmetric_cut() {
    let mut c = BitrateController::new(MIN, MAX);
    let now = t0();
    assert_eq!(c.update(10_000_000, now), Some(10_000_000));
    assert_eq!(c.update(40_000_000, now + Duration::from_millis(100)), None);
}

#[test]
fn end_to_end_degrade_then_recover() {
    let mut c = BitrateController::with_params(MIN, MAX, 0.5, 0.05, Duration::ZERO);
    let now = t0();
    let mut target = 10_000_000u32;
    c.update(target, now);

    for i in 1..8 {
        let est = estimate_from_loss(target, target, 0.30);
        if let Some(v) = c.update(est, now + Duration::from_millis(i * 100)) {
            target = v;
        }
    }
    assert!(target < 10_000_000, "target fell under sustained loss: {target}");
    let low = target;

    for i in 8..40 {
        let est = estimate_from_loss(target, MAX, 0.0);
        if let Some(v) = c.update(est, now + Duration::from_millis(i * 100)) {
            target = v;
        }
    }
    assert!(target > low, "target recovered after loss cleared: {low} -> {target}");
}

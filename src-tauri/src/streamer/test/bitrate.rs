use std::time::{Duration, Instant};

use crate::streamer::bitrate::{estimate_from_loss, BitrateController, RttCongestionMonitor};

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
    assert!(
        est < 10_000_000,
        "heavy loss must reduce estimate, got {est}"
    );
    assert_eq!(est, 9_000_000);
}

#[test]
fn estimate_probes_up_when_healthy() {
    let est = estimate_from_loss(10_000_000, 10_000_000, 0.0);
    assert!(
        est > 10_000_000,
        "healthy link should probe upward, got {est}"
    );
    assert_eq!(est, 10_800_000);
}

#[test]
fn estimate_probe_is_capped_by_delivered_rate() {
    let est = estimate_from_loss(10_000_000, 2_000_000, 0.0);
    assert_eq!(est, 10_000_000, "probe capped near delivered rate");
}

#[test]
fn estimate_reacts_across_the_old_dead_band() {
    // 2 % to 10 % loss used to be a literal no-op: a visibly broken picture with the rate held
    // flat. Every sample in that range must now move the estimate down, monotonically.
    let mut last = u32::MAX;
    for pct in [2u32, 3, 5, 8, 9] {
        let est = estimate_from_loss(10_000_000, 10_000_000, pct as f64 / 100.0);
        assert!(
            est < 10_000_000,
            "{pct}% loss must cut the estimate, got {est}"
        );
        assert!(est < last, "the response must be monotone in loss");
        last = est;
    }
}

#[test]
fn the_loss_response_is_continuous_where_the_branches_meet() {
    // At 10 % the gentle ramp and GCC's own `1 - 0.5 * loss` must agree, or the controller has
    // a step discontinuity exactly where loss becomes severe.
    let below = estimate_from_loss(10_000_000, 10_000_000, 0.0999);
    let at = estimate_from_loss(10_000_000, 10_000_000, 0.10);
    assert!(
        (below as i64 - at as i64).abs() < 10_000,
        "discontinuity at the severe-loss boundary: {below} vs {at}"
    );
    assert_eq!(at, 9_500_000, "severe loss still matches GCC");
}

#[test]
fn ordinary_wireless_background_loss_still_probes_upward() {
    // Below 1 % is the background of any wireless link and must not be read as congestion,
    // or a healthy 5 GHz session never reaches its ceiling.
    let est = estimate_from_loss(10_000_000, 10_000_000, 0.005);
    assert!(est > 10_000_000, "0.5% loss must not stop the probe: {est}");
}

#[test]
fn a_sub_deadband_cut_compounds_instead_of_stalling() {
    // The estimator runs on its own value, not the emitted target. A 5 % loss produces a ~2 %
    // cut, which the controller's 4 % cut deadband would swallow — so if the estimate were
    // recomputed from the unchanged target each interval, nothing would ever move. Feeding it
    // its own output is what makes moderate loss actually converge.
    let mut est = 10_000_000u32;
    for _ in 0..10 {
        est = estimate_from_loss(est, est, 0.05);
    }
    assert!(
        est < 8_500_000,
        "ten intervals of 5% loss must compound well past the deadband, got {est}"
    );
}

#[test]
fn cut_emits_when_symmetric_threshold_would_stall() {
    let mut c = BitrateController::new(MIN, MAX);
    let now = t0();
    let start = c.update(10_000_000, now).expect("first sample emits");
    assert_eq!(start, 10_000_000);
    let cut = c.update(8_500_000, now + Duration::from_millis(300));
    assert!(
        cut.is_some(),
        "cut must emit under the asymmetric threshold"
    );
    assert!(
        cut.unwrap() < start,
        "emitted value must be a reduction: {cut:?}"
    );
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
    assert!(
        target < 10_000_000,
        "target fell under sustained loss: {target}"
    );
    let low = target;

    for i in 8..40 {
        let est = estimate_from_loss(target, MAX, 0.0);
        if let Some(v) = c.update(est, now + Duration::from_millis(i * 100)) {
            target = v;
        }
    }
    assert!(
        target > low,
        "target recovered after loss cleared: {low} -> {target}"
    );
}

/// Replays `spawn_bitrate_driver`'s arithmetic without a peer connection: one RTT sample and
/// one loss sample per 120 ms poll, returning the final target.
fn drive(rtt_ms_series: &[f64], fraction_lost: f64, polls: usize) -> u32 {
    let mut controller = BitrateController::new(MIN, MAX);
    let mut monitor = RttCongestionMonitor::new();
    let mut target = MAX;
    let start = t0();
    for i in 0..polls {
        let rtt = rtt_ms_series[i % rtt_ms_series.len()];
        let mut estimate = estimate_from_loss(target, target, fraction_lost);
        if monitor.observe(rtt) {
            let clamped = (target as f64 * 0.85) as u32;
            if clamped < estimate {
                estimate = clamped;
            }
        }
        let now = start + Duration::from_millis(i as u64 * 120);
        if let Some(v) = controller.update(estimate, now) {
            target = v;
        }
    }
    target
}

#[test]
fn lan_rtt_never_cuts_the_target() {
    // 5 ms is a normal Wi-Fi LAN RTT. Before the unit fix this was multiplied by 1000,
    // crossed the 80 ms clamp on every poll, and walked the target to the 1 Mbps floor
    // within a couple of seconds.
    let target = drive(&[5.0], 0.0, 200);
    assert_eq!(
        target, MAX,
        "a 5 ms RTT must never clamp the bitrate; got {target}"
    );
}

#[test]
fn steady_wan_rtt_never_cuts_the_target() {
    // 150 ms steady is an ordinary cross-continent RTT and carries no congestion signal:
    // the old absolute 80 ms threshold pinned every such session at the floor.
    let target = drive(&[150.0, 152.0, 148.0, 151.0], 0.0, 200);
    assert_eq!(target, MAX, "steady high RTT is not congestion");
}

#[test]
fn queueing_above_baseline_still_clamps() {
    let mut m = RttCongestionMonitor::new();
    for _ in 0..20 {
        assert!(!m.observe(20.0), "baseline samples are not congestion");
    }
    assert!(!m.observe(120.0), "one excursion is not yet sustained");
    assert!(m.observe(130.0), "a sustained excursion must clamp");
}

#[test]
fn rtt_monitor_ignores_absent_samples() {
    let mut m = RttCongestionMonitor::new();
    assert!(
        !m.observe(0.0),
        "no RTCP report yet must not read as congestion"
    );
    assert!(!m.observe(f64::NAN));
    assert_eq!(m.baseline_ms(), None);
}

#[test]
fn cuts_are_rate_limited() {
    let mut c = BitrateController::new(MIN, MAX);
    let now = t0();
    assert_eq!(c.update(10_000_000, now), Some(10_000_000));
    assert_eq!(
        c.update(5_000_000, now + Duration::from_millis(120)),
        None,
        "a cut one poll after the last change must be held off"
    );
    assert!(
        c.update(5_000_000, now + Duration::from_millis(300))
            .is_some(),
        "the cut lands once the interval has elapsed"
    );
}

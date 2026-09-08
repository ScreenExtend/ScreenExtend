use std::time::{Duration, Instant};

pub const DEFAULT_MIN_BITRATE_BPS: u32 = 1_000_000;

#[derive(Debug, Clone)]
pub struct BitrateController {
    min_bps: u32,
    max_bps: u32,
    alpha: f64,
    change_threshold: f64,
    cut_threshold: f64,
    min_interval: Duration,
    cut_min_interval: Duration,
    ewma: Option<f64>,
    last_emitted: Option<u32>,
    last_change_at: Option<Instant>,
}

impl BitrateController {
    pub fn new(min_bps: u32, max_bps: u32) -> Self {
        Self::with_params(min_bps, max_bps, 0.4, 0.10, Duration::from_millis(500))
            .with_cut_threshold(0.04)
            .with_cut_min_interval(Duration::from_millis(250))
    }

    pub fn with_params(
        min_bps: u32,
        max_bps: u32,
        alpha: f64,
        change_threshold: f64,
        min_interval: Duration,
    ) -> Self {
        assert!(min_bps <= max_bps, "min_bps must be <= max_bps");
        assert!(alpha > 0.0 && alpha <= 1.0, "alpha must be in (0, 1]");
        Self {
            min_bps,
            max_bps,
            alpha,
            change_threshold,
            cut_threshold: change_threshold,
            min_interval,
            cut_min_interval: Duration::ZERO,
            ewma: None,
            last_emitted: None,
            last_change_at: None,
        }
    }

    pub fn with_cut_threshold(mut self, cut_threshold: f64) -> Self {
        assert!(
            cut_threshold > 0.0 && cut_threshold <= 1.0,
            "cut_threshold must be in (0, 1]"
        );
        self.cut_threshold = cut_threshold;
        self
    }

    /// Floor on how often a downward move may be emitted. Without one a sustained signal
    /// ratchets the target down once per BWE poll (120 ms) and walks to the floor in a
    /// couple of seconds.
    pub fn with_cut_min_interval(mut self, interval: Duration) -> Self {
        self.cut_min_interval = interval;
        self
    }

    pub fn current_target(&self) -> Option<u32> {
        self.last_emitted
    }

    pub fn update(&mut self, raw_bps: u32, now: Instant) -> Option<u32> {
        let raw = raw_bps as f64;
        let smoothed = match self.ewma {
            Some(prev) => self.alpha * raw + (1.0 - self.alpha) * prev,
            None => raw,
        };
        self.ewma = Some(smoothed);

        let candidate =
            (smoothed.round() as i64).clamp(self.min_bps as i64, self.max_bps as i64) as u32;

        let Some(last) = self.last_emitted else {
            self.emit(candidate, now);
            return Some(candidate);
        };

        let is_cut = candidate < last;
        let threshold = if is_cut {
            self.cut_threshold
        } else {
            self.change_threshold
        };

        let rel_delta = (candidate as f64 - last as f64).abs() / (last.max(1) as f64);
        if rel_delta < threshold {
            return None;
        }

        let interval = if is_cut {
            self.cut_min_interval
        } else {
            self.min_interval
        };
        if let Some(t) = self.last_change_at {
            if now.duration_since(t) < interval {
                return None;
            }
        }

        self.emit(candidate, now);
        Some(candidate)
    }

    fn emit(&mut self, target: u32, now: Instant) {
        self.last_emitted = Some(target);
        self.last_change_at = Some(now);
    }
}

/// How far above the observed baseline RTT counts as queuing delay.
pub const RTT_DELAY_THRESHOLD_MS: f64 = 40.0;

/// Consecutive polls that must exceed the delay threshold before a clamp fires. At the
/// 120 ms BWE poll rate this is ~240 ms of sustained queueing, which one late RTCP report
/// cannot fake.
const RTT_CONGESTION_POLLS: u32 = 2;

/// Delay-based congestion detector.
///
/// Absolute RTT is not a congestion signal: 80 ms is an ordinary uncongested RTT for a
/// cross-continent link, and clamping on it pins every WAN session at the floor forever.
/// What indicates a full queue is RTT *above the path's own baseline*, so this tracks a
/// rolling minimum and reports only sustained excursions above it.
#[derive(Debug, Clone, Default)]
pub struct RttCongestionMonitor {
    baseline_ms: Option<f64>,
    over: u32,
}

impl RttCongestionMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn baseline_ms(&self) -> Option<f64> {
        self.baseline_ms
    }

    /// Feed one RTT sample in **milliseconds**. Returns true while the path looks congested.
    pub fn observe(&mut self, rtt_ms: f64) -> bool {
        if !rtt_ms.is_finite() || rtt_ms <= 0.0 {
            return false;
        }
        let baseline = match self.baseline_ms {
            Some(b) if rtt_ms < b => rtt_ms,
            // Creep upward slowly so a genuinely re-routed path re-baselines instead of
            // staying pinned to a minimum it will never see again.
            Some(b) => b + (rtt_ms - b) * 0.002,
            None => rtt_ms,
        };
        self.baseline_ms = Some(baseline);

        if rtt_ms - baseline > RTT_DELAY_THRESHOLD_MS {
            self.over = self.over.saturating_add(1);
        } else {
            self.over = 0;
        }
        self.over >= RTT_CONGESTION_POLLS
    }
}

pub fn estimate_from_loss(
    current_target_bps: u32,
    measured_send_bps: u32,
    fraction_lost: f64,
) -> u32 {
    let cur = current_target_bps as f64;
    let loss = fraction_lost.clamp(0.0, 1.0);

    if loss > 0.10 {
        (cur * (1.0 - 0.5 * loss)).max(1.0) as u32
    } else if loss < 0.02 {
        let probe = cur * 1.08;
        let ceil = (measured_send_bps as f64) * 1.5;
        probe.min(ceil.max(cur)) as u32
    } else {
        current_target_bps
    }
}

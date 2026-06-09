use std::time::{Duration, Instant};

pub const DEFAULT_MIN_BITRATE_BPS: u32 = 1_000_000;

#[derive(Debug, Clone)]
pub struct BitrateController {
    min_bps: u32,
    max_bps: u32,
    alpha: f64,
    change_threshold: f64,
    min_interval: Duration,
    ewma: Option<f64>,
    last_emitted: Option<u32>,
    last_change_at: Option<Instant>,
}

impl BitrateController {
    pub fn new(min_bps: u32, max_bps: u32) -> Self {
        Self::with_params(min_bps, max_bps, 0.4, 0.10, Duration::from_millis(500))
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
            min_interval,
            ewma: None,
            last_emitted: None,
            last_change_at: None,
        }
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

        let candidate = (smoothed.round() as i64).clamp(self.min_bps as i64, self.max_bps as i64)
            as u32;

        let Some(last) = self.last_emitted else {
            self.emit(candidate, now);
            return Some(candidate);
        };

        let rel_delta = (candidate as f64 - last as f64).abs() / (last.max(1) as f64);
        if rel_delta < self.change_threshold {
            return None;
        }

        if let Some(t) = self.last_change_at {
            if now.duration_since(t) < self.min_interval {
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

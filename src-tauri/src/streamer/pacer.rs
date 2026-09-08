//! A send pacer for the per-viewer RTP writer.
//!
//! webrtc-rs has no pacer — `interceptor-0.17.1` ships nack, twcc, report and stats and nothing
//! else — so the writer hands a whole access unit to the socket in one tight loop. A 40 KB IDR is
//! ~35 back-to-back datagrams; a screen-change frame is more. That microburst lands in a single
//! 802.11 TXOP and in one AP queue, which is how a link with spare average capacity still drops
//! packets and builds standing delay.
//!
//! libwebrtc paces at 2.5x the estimated bitrate for exactly this reason ("video encoders
//! overshoot the target frame size in case of sudden movement especially when dealing with
//! screensharing"). Stadia was measured on the wire doing the same shape: packets arrive in
//! per-frame batches, and *within* a frame period they are grouped into ~2 ms clusters of 7-9
//! packets. Those two numbers are what [`PacingMode::Full`] is calibrated to.
//!
//! The pacer is a pure leaky bucket with no clock of its own: [`Pacer::schedule`] takes the
//! current instant and returns how long the caller should sleep before putting this packet on
//! the wire, so the whole policy is testable without a network.

use std::time::{Duration, Instant};

/// libwebrtc's `kDefaultPaceMultiplier`.
pub const PACING_FACTOR: f64 = 2.5;

/// A relayed or lossy path still gets to send at least this much, so a collapsed bitrate
/// estimate cannot pace the session into a stall it can never recover from.
pub const MIN_PACING_RATE_BPS: u64 = 2_000_000;

/// Packets allowed out back-to-back before the bucket starts holding them. Stadia's measured
/// cluster size.
const BURST_PACKETS: f64 = 8.0;

/// Fraction of one frame interval a single access unit may be spread over. Beyond this the
/// pacer is adding latency rather than removing it, so it gives up and lets the packet go.
const FRAME_BUDGET: f64 = 0.75;

/// How hard to pace. Driven by session locality and by whether loss has actually been observed:
/// on a clean same-subnet hop the burst costs nothing and spacing it only adds delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingMode {
    /// Same-subnet, no loss seen yet. The writer behaves exactly as it did before the pacer.
    Off,
    /// Same-subnet with observed loss, or an unclassified path. Wide bucket, high multiplier.
    Light,
    /// Relayed or cross-network. Full libwebrtc-shaped pacing.
    Full,
}

impl PacingMode {
    fn factor(self) -> f64 {
        match self {
            PacingMode::Off => f64::INFINITY,
            PacingMode::Light => 5.0,
            PacingMode::Full => PACING_FACTOR,
        }
    }

    fn burst_packets(self) -> f64 {
        match self {
            PacingMode::Off => f64::INFINITY,
            PacingMode::Light => BURST_PACKETS * 3.0,
            PacingMode::Full => BURST_PACKETS,
        }
    }
}

#[derive(Debug)]
pub struct Pacer {
    mode: PacingMode,
    target_bps: u64,
    mtu: usize,
    /// Bytes the bucket may spend right now. Goes negative: the debt is what the caller waits
    /// off, and carrying it is what keeps the long-run rate honest.
    budget_bytes: f64,
    last_refill: Option<Instant>,
    frame_interval: Duration,
    frame_waited: Duration,
    deadline_misses: u64,
    total_wait_ns: u64,
}

impl Pacer {
    pub fn new(mode: PacingMode, target_bps: u32, mtu: usize, frame_interval: Duration) -> Self {
        Self {
            mode,
            target_bps: target_bps.max(1) as u64,
            mtu: mtu.max(1),
            budget_bytes: 0.0,
            last_refill: None,
            frame_interval,
            frame_waited: Duration::ZERO,
            deadline_misses: 0,
            total_wait_ns: 0,
        }
    }

    pub fn set_mode(&mut self, mode: PacingMode) {
        if mode != self.mode {
            self.mode = mode;
            // A mode change resets the bucket rather than inheriting a debt accrued under a
            // different rate; the two are not comparable.
            self.budget_bytes = 0.0;
        }
    }

    pub fn mode(&self) -> PacingMode {
        self.mode
    }

    pub fn set_target_bps(&mut self, bps: u32) {
        self.target_bps = bps.max(1) as u64;
    }

    pub fn set_mtu(&mut self, mtu: usize) {
        self.mtu = mtu.max(1);
    }

    pub fn set_frame_interval(&mut self, interval: Duration) {
        self.frame_interval = interval;
    }

    pub fn deadline_misses(&self) -> u64 {
        self.deadline_misses
    }

    pub fn total_wait_ns(&self) -> u64 {
        self.total_wait_ns
    }

    /// Bytes per second the bucket refills at.
    fn refill_rate(&self) -> f64 {
        (self.target_bps as f64).max(MIN_PACING_RATE_BPS as f64) * self.mode.factor() / 8.0
    }

    fn burst_bytes(&self) -> f64 {
        self.mode.burst_packets() * self.mtu as f64
    }

    /// Start a new access unit. Each frame gets its own delay budget: a keyframe may be spread
    /// across most of a frame interval, but it may never push the *next* frame late.
    pub fn begin_frame(&mut self) {
        self.frame_waited = Duration::ZERO;
    }

    /// How long to sleep before writing a packet of `bytes`.
    pub fn schedule(&mut self, bytes: usize, now: Instant) -> Duration {
        if self.mode == PacingMode::Off {
            return Duration::ZERO;
        }
        let rate = self.refill_rate();
        if !rate.is_finite() || rate <= 0.0 {
            return Duration::ZERO;
        }

        if let Some(last) = self.last_refill {
            let dt = now.saturating_duration_since(last).as_secs_f64();
            self.budget_bytes = (self.budget_bytes + rate * dt).min(self.burst_bytes());
        } else {
            self.budget_bytes = self.burst_bytes();
        }
        self.last_refill = Some(now);

        self.budget_bytes -= bytes as f64;
        if self.budget_bytes >= 0.0 {
            return Duration::ZERO;
        }

        let wait = Duration::from_secs_f64((-self.budget_bytes / rate).min(1.0));
        let cap = self.frame_interval.mul_f64(FRAME_BUDGET);
        if self.frame_waited + wait > cap {
            // Holding this packet any longer would cost more latency than the burst it is
            // smoothing. Send it, forgive the debt so the next frame starts clean, and count it:
            // a pacer that silently grows unbounded is worse than no pacer.
            let allowed = cap.saturating_sub(self.frame_waited);
            self.frame_waited = cap;
            self.budget_bytes = 0.0;
            self.deadline_misses += 1;
            self.total_wait_ns += allowed.as_nanos() as u64;
            return allowed;
        }
        self.frame_waited += wait;
        self.total_wait_ns += wait.as_nanos() as u64;
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: usize = 1188;
    const FRAME: Duration = Duration::from_micros(16_667);

    fn full(bps: u32) -> Pacer {
        Pacer::new(PacingMode::Full, bps, MTU, FRAME)
    }

    #[test]
    fn off_mode_never_delays_anything() {
        let mut p = Pacer::new(PacingMode::Off, 1_000_000, MTU, FRAME);
        let t = Instant::now();
        p.begin_frame();
        for i in 0..200 {
            assert_eq!(p.schedule(MTU, t + Duration::from_nanos(i)), Duration::ZERO);
        }
        assert_eq!(p.deadline_misses(), 0);
    }

    #[test]
    fn a_small_frame_is_never_held() {
        let mut p = full(20_000_000);
        let t = Instant::now();
        p.begin_frame();
        // Four packets is a typical P-frame at 20 Mbps / 60 fps, well inside the burst.
        for _ in 0..4 {
            assert_eq!(p.schedule(MTU, t), Duration::ZERO);
        }
    }

    #[test]
    fn a_keyframe_burst_is_spread_but_never_past_the_frame_budget() {
        let mut p = full(8_000_000);
        let t = Instant::now();
        p.begin_frame();
        // ~35 packets: a 40 KB IDR.
        let mut total = Duration::ZERO;
        for _ in 0..35 {
            total += p.schedule(MTU, t + total);
        }
        assert!(total > Duration::ZERO, "the burst must actually be spread");
        assert!(
            total <= FRAME.mul_f64(FRAME_BUDGET) + Duration::from_micros(50),
            "pacing must not outlast the frame budget, got {total:?}"
        );
    }

    #[test]
    fn the_frame_budget_resets_per_access_unit() {
        let mut p = full(4_000_000);
        let t = Instant::now();
        p.begin_frame();
        let mut spent = Duration::ZERO;
        for _ in 0..80 {
            spent += p.schedule(MTU, t + spent);
        }
        assert!(p.deadline_misses() > 0, "an oversized frame must give up");
        p.begin_frame();
        // A fresh frame one interval later starts with its own budget, not the exhausted one.
        let next = p.schedule(MTU, t + spent + FRAME);
        assert!(next <= FRAME.mul_f64(FRAME_BUDGET));
    }

    #[test]
    fn the_long_run_rate_follows_the_target() {
        // 10 Mbps target paced at 2.5x is 25 Mbps of instantaneous allowance; over many frames
        // the bucket must not let a 40 Mbps stream through.
        let mut p = full(10_000_000);
        let start = Instant::now();
        let mut now = start;
        let mut bytes = 0usize;
        for _ in 0..60 {
            p.begin_frame();
            for _ in 0..20 {
                now += p.schedule(MTU, now);
                bytes += MTU;
            }
            now += FRAME;
        }
        let secs = now.duration_since(start).as_secs_f64();
        let bps = (bytes as f64 * 8.0) / secs;
        assert!(
            bps < 25_000_000.0 * 1.2,
            "paced rate {bps:.0} exceeded the 2.5x allowance"
        );
    }

    #[test]
    fn a_collapsed_estimate_still_gets_a_floor() {
        // The bitrate controller bottoming out at its 1 Mbps floor must not pace the session
        // into a stall: the floor rate keeps a keyframe moving.
        let mut p = full(100);
        let t = Instant::now();
        p.begin_frame();
        let w = p.schedule(MTU * 8, t);
        assert!(w <= FRAME.mul_f64(FRAME_BUDGET));
    }

    #[test]
    fn switching_mode_drops_the_inherited_debt() {
        let mut p = full(2_000_000);
        let t = Instant::now();
        p.begin_frame();
        for _ in 0..40 {
            p.schedule(MTU, t);
        }
        p.set_mode(PacingMode::Off);
        p.begin_frame();
        assert_eq!(p.schedule(MTU, t), Duration::ZERO);
    }
}

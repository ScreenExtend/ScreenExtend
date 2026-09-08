//! How often one session is allowed to ask the encoder for an IDR.
//!
//! Four separate paths in the transport reach for a keyframe: an RTCP PLI, an RTCP FIR, a
//! payloader error, and the local broadcast dropping frames on a viewer that fell behind. The
//! encoders run an infinite GOP, so an IDR is the largest frame a session will ever emit —
//! commonly 5-20x a P-frame. Answering each of those signals on its own turns a transient into a
//! storm: the keyframe makes the burst worse, the burst drops more packets, the client asks for
//! another keyframe.
//!
//! The gate breaks that loop by admitting at most one request per window. The window is derived
//! from the measured round trip because a request made now cannot possibly produce a frame the
//! client has reacted to before one round trip has passed — anything the client says in the
//! meantime is about the *previous* keyframe and is already being answered.

use std::sync::atomic::{AtomicU64, Ordering};

/// Never throttle harder than this: on a 1 ms LAN, 2x RTT is 2 ms, which is no gate at all.
pub const MIN_IDR_INTERVAL_MS: u64 = 200;

/// Never throttle softer than this. A client that genuinely lost its reference chain would
/// otherwise sit black for the whole window, and an infinite GOP gives it no other way out.
pub const MAX_IDR_INTERVAL_MS: u64 = 1000;

/// How many round trips a keyframe is given to take effect before another one is allowed.
const RTT_WINDOWS: f64 = 2.0;

#[derive(Debug)]
pub struct KeyframeGate {
    last_request_ns: AtomicU64,
    interval_ns: AtomicU64,
    suppressed: AtomicU64,
    admitted: AtomicU64,
}

impl Default for KeyframeGate {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeGate {
    pub fn new() -> Self {
        Self {
            last_request_ns: AtomicU64::new(0),
            interval_ns: AtomicU64::new(MIN_IDR_INTERVAL_MS * 1_000_000),
            suppressed: AtomicU64::new(0),
            admitted: AtomicU64::new(0),
        }
    }

    /// Re-derive the window from the path's round trip. Called from the BWE poll, which already
    /// reads RTT off the RTCP receiver reports.
    pub fn observe_rtt_ms(&self, rtt_ms: f64) {
        if !rtt_ms.is_finite() || rtt_ms <= 0.0 {
            return;
        }
        let want = (rtt_ms * RTT_WINDOWS) as u64;
        let clamped = want.clamp(MIN_IDR_INTERVAL_MS, MAX_IDR_INTERVAL_MS);
        self.interval_ns
            .store(clamped * 1_000_000, Ordering::Relaxed);
    }

    /// True when the caller may ask the encoder for an IDR, claiming the window if so.
    ///
    /// Lock-free because it is called from the RTCP read task, the RTP writer task and the
    /// connection-state callback, none of which may block one another.
    pub fn allow(&self, now_ns: u64) -> bool {
        let interval = self.interval_ns.load(Ordering::Relaxed);
        let mut last = self.last_request_ns.load(Ordering::Relaxed);
        loop {
            // 0 is the "never asked" sentinel, so the first request — the one that starts the
            // session on `Connected` — is always admitted.
            if last != 0 && now_ns.saturating_sub(last) < interval {
                self.suppressed.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            match self.last_request_ns.compare_exchange_weak(
                last,
                now_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.admitted.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                Err(actual) => last = actual,
            }
        }
    }

    pub fn suppressed(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }

    pub fn admitted(&self) -> u64 {
        self.admitted.load(Ordering::Relaxed)
    }

    pub fn interval_ms(&self) -> u64 {
        self.interval_ns.load(Ordering::Relaxed) / 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn the_first_request_is_always_admitted() {
        let g = KeyframeGate::new();
        assert!(g.allow(0), "the session-start IDR must never be gated");
        assert_eq!(g.admitted(), 1);
    }

    #[test]
    fn a_storm_collapses_to_one_request_per_window() {
        let g = KeyframeGate::new();
        assert!(g.allow(1_000 * MS));
        // Twenty PLIs arriving over the next 100 ms — the client's backlog, all describing the
        // same loss — must produce exactly zero further encoder work.
        for i in 1..=20 {
            assert!(
                !g.allow(1_000 * MS + i * 5 * MS),
                "request {i} slipped through"
            );
        }
        assert_eq!(g.suppressed(), 20);
        assert!(g.allow(1_000 * MS + MIN_IDR_INTERVAL_MS * MS));
        assert_eq!(g.admitted(), 2);
    }

    #[test]
    fn the_window_tracks_the_round_trip() {
        let g = KeyframeGate::new();
        assert_eq!(g.interval_ms(), MIN_IDR_INTERVAL_MS);
        g.observe_rtt_ms(150.0);
        assert_eq!(g.interval_ms(), 300, "two round trips");
        g.observe_rtt_ms(2.0);
        assert_eq!(g.interval_ms(), MIN_IDR_INTERVAL_MS, "clamped on a LAN");
        g.observe_rtt_ms(4_000.0);
        assert_eq!(g.interval_ms(), MAX_IDR_INTERVAL_MS, "clamped on a bad WAN");
    }

    #[test]
    fn a_missing_rtt_sample_does_not_move_the_window() {
        let g = KeyframeGate::new();
        g.observe_rtt_ms(400.0);
        let before = g.interval_ms();
        g.observe_rtt_ms(0.0);
        g.observe_rtt_ms(f64::NAN);
        g.observe_rtt_ms(-1.0);
        assert_eq!(g.interval_ms(), before);
    }
}

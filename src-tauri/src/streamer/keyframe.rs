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

/// The floor and cap for a path that is not same-subnet.
///
/// The window above is derived from RTT alone, and RTT is the wrong instrument for the thing that
/// actually hurts: on a relayed session whose ICE pair happens to be short, 2x RTT is a couple of
/// milliseconds, the clamp lands on [`MIN_IDR_INTERVAL_MS`], and the host will answer a struggling
/// client with **five 1080p IDRs a second** — measured at 3.2/s through the public relay, 365 IDRs
/// in 5092 frames, against 2 in 1352 on a direct LAN session of the same host. An IDR is 5-20x a
/// P-frame, so that is most of a constrained link's budget spent on the one frame type most likely
/// to overflow it, which drops more packets, which produces the next request. Latency is what a
/// short window buys, and a cross-network path has already given that up.
///
/// It is affordable to wait because rolling intra refresh is on for every shipped session
/// (`server.rs`): the refresh wave rebuilds the picture continuously without an IDR, so a keyframe
/// is for bootstrapping and for a loss the wave cannot cover, not for routine recovery.
const RELAY_MIN_IDR_INTERVAL_MS: u64 = 1000;
const RELAY_MAX_IDR_INTERVAL_MS: u64 = 3000;

/// Between the two: private, but a router hop away. Half the relay floor.
const UNKNOWN_MIN_IDR_INTERVAL_MS: u64 = 500;
const UNKNOWN_MAX_IDR_INTERVAL_MS: u64 = 2000;

// The ordering is the whole point of the tier, so it is checked by the compiler rather than by a
// test that could be deleted with the constant it guards.
const _: () = assert!(RELAY_MIN_IDR_INTERVAL_MS > UNKNOWN_MIN_IDR_INTERVAL_MS);
const _: () = assert!(UNKNOWN_MIN_IDR_INTERVAL_MS > MIN_IDR_INTERVAL_MS);
const _: () = assert!(RELAY_MAX_IDR_INTERVAL_MS >= RELAY_MIN_IDR_INTERVAL_MS);

#[derive(Debug)]
pub struct KeyframeGate {
    last_request_ns: AtomicU64,
    interval_ns: AtomicU64,
    /// The clamp `observe_rtt_ms` applies, in milliseconds. Fixed for the session, because the
    /// locality it comes from is decided before the handshake and does not change under it.
    floor_ms: u64,
    cap_ms: u64,
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
        Self::with_bounds(MIN_IDR_INTERVAL_MS, MAX_IDR_INTERVAL_MS)
    }

    /// The gate for a session on a known path.
    ///
    /// A same-subnet peer keeps the tight window — it can absorb the burst and it is the case
    /// where the extra latency of waiting would be felt. Everything else is throttled, because on
    /// those paths the IDR is what causes the next loss. See [`RELAY_MIN_IDR_INTERVAL_MS`].
    pub fn for_locality(locality: super::webrtc_session::SessionLocality) -> Self {
        use super::webrtc_session::SessionLocality::*;
        let (floor, cap) = match locality {
            SameNetwork => (MIN_IDR_INTERVAL_MS, MAX_IDR_INTERVAL_MS),
            Unknown => (UNKNOWN_MIN_IDR_INTERVAL_MS, UNKNOWN_MAX_IDR_INTERVAL_MS),
            CrossNetwork => (RELAY_MIN_IDR_INTERVAL_MS, RELAY_MAX_IDR_INTERVAL_MS),
        };
        Self::with_bounds(floor, cap)
    }

    fn with_bounds(floor_ms: u64, cap_ms: u64) -> Self {
        Self {
            last_request_ns: AtomicU64::new(0),
            interval_ns: AtomicU64::new(floor_ms * 1_000_000),
            floor_ms,
            cap_ms,
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
        let clamped = want.clamp(self.floor_ms, self.cap_ms);
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
    fn a_relayed_session_is_not_driven_to_five_idrs_a_second_by_a_short_rtt() {
        // The measured defect: through the public relay the nominated pair was short, `2 * RTT`
        // clamped to the 200 ms floor, and a client asking every ~300 ms was answered every time —
        // 365 IDRs in 5092 frames. RTT is the wrong instrument for a path whose problem is
        // capacity, so the floor is the locality's, not the LAN's.
        use super::super::webrtc_session::SessionLocality;
        let g = KeyframeGate::for_locality(SessionLocality::CrossNetwork);
        g.observe_rtt_ms(1.0);
        assert_eq!(
            g.interval_ms(),
            RELAY_MIN_IDR_INTERVAL_MS,
            "a short round trip must not unlock a relayed path's IDR rate"
        );

        // Ten seconds of the measured 3.2/s request storm. Started off the zero sentinel, which
        // `allow` treats as "never asked".
        let mut admitted = 0;
        for i in 0..=32 {
            if g.allow(1_000 * MS + i * 312 * MS) {
                admitted += 1;
            }
        }
        assert!(
            admitted <= 11,
            "a 3.2/s storm must cost about one IDR a second, not one per request; got {admitted} \
             in ten seconds"
        );
        // The other half of the requirement, and the reason this is a window and not a drop: a
        // client that genuinely lost its reference chain still gets a keyframe inside every
        // window. Suppressing a burst is not the same as refusing to answer it.
        assert!(
            admitted >= 8,
            "the gate must keep answering — a starved client sits black; got {admitted}"
        );
    }

    #[test]
    fn locality_sets_the_floor_and_the_cap() {
        use super::super::webrtc_session::SessionLocality;
        for (locality, floor, cap) in [
            (
                SessionLocality::SameNetwork,
                MIN_IDR_INTERVAL_MS,
                MAX_IDR_INTERVAL_MS,
            ),
            (
                SessionLocality::Unknown,
                UNKNOWN_MIN_IDR_INTERVAL_MS,
                UNKNOWN_MAX_IDR_INTERVAL_MS,
            ),
            (
                SessionLocality::CrossNetwork,
                RELAY_MIN_IDR_INTERVAL_MS,
                RELAY_MAX_IDR_INTERVAL_MS,
            ),
        ] {
            let g = KeyframeGate::for_locality(locality);
            assert_eq!(g.interval_ms(), floor, "{locality:?} starts at its floor");
            g.observe_rtt_ms(0.5);
            assert_eq!(
                g.interval_ms(),
                floor,
                "{locality:?} clamps up to its floor"
            );
            g.observe_rtt_ms(10_000.0);
            assert_eq!(g.interval_ms(), cap, "{locality:?} clamps down to its cap");
        }
    }

    #[test]
    fn a_lan_session_keeps_exactly_the_window_it_had() {
        // The tight window is what a same-subnet peer is for; this fix must not cost it anything.
        use super::super::webrtc_session::SessionLocality;
        let lan = KeyframeGate::for_locality(SessionLocality::SameNetwork);
        let default = KeyframeGate::new();
        assert_eq!(lan.interval_ms(), default.interval_ms());
        lan.observe_rtt_ms(150.0);
        default.observe_rtt_ms(150.0);
        assert_eq!(lan.interval_ms(), default.interval_ms());
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

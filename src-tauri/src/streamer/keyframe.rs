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
/// Note that rolling intra refresh is *off* on live joins (UX-GREEN in `docs/OPTIMIZATIONS.md`),
/// so nothing rebuilds the picture between keyframes: a relayed client that lost one waits for
/// this window to pass.
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

    /// Count a request something in front of the gate held back, so the telemetry total covers
    /// every suppressed request and not only the ones this window caught.
    pub fn note_suppressed(&self) {
        self.suppressed.fetch_add(1, Ordering::Relaxed);
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

/// The first window [`PliBackoff`] opens after an answered PLI keeps being followed by more.
pub const PLI_BACKOFF_FLOOR_MS: u64 = 200;

/// The widest it opens: one keyframe every two seconds for a receiver that never stops asking.
pub const PLI_BACKOFF_CAP_MS: u64 = 2000;

/// The cap while the client's own STATS frames say it is painting through WebCodecs.
///
/// That client decodes outside libwebrtc, so a PLI from it is libwebrtc's idle decoder and not
/// the page: a page that writes keyframes back to the receiver (G05) leaves one about every 3 s,
/// which the 2 s cap would still answer with an IDR every time. The page's own requests arrive
/// as evidence (its keyframe counter moves) and open the window at once.
pub const PLI_BACKOFF_CAP_HEALTHY_MS: u64 = 10_000;

/// A PLI-free stretch this long means whatever was asking has been satisfied.
const PLI_QUIET_RESET_MS: u64 = 2 * PLI_BACKOFF_CAP_MS;

/// Rate limit for keyframe requests that arrive as RTCP PLI/FIR, on top of [`KeyframeGate`].
///
/// A receiver whose frames are consumed by an encoded transform — every WebCodecs client page,
/// the relay's older copy and the pre-overhaul page included — never feeds libwebrtc's own
/// decoder, and libwebrtc answers a decoder that has had nothing decodable for 200 ms with a
/// keyframe request, forever. Measured on the Windows dev box: 117 PLIs in a 30 s static window,
/// all admitted by the 200 ms same-network gate, so the host emitted an IDR every ~280 ms for the
/// whole session. Nothing in the RTCP packet tells that receiver apart from one that really lost
/// its picture, so this goes by behaviour: a decoder an IDR fixes stops asking, and one that keeps
/// asking after being answered is not being fixed by IDRs. Each answered PLI that is followed by
/// another doubles the window, up to [`PLI_BACKOFF_CAP_MS`]; a quiet stretch closes it again.
///
/// Evidence that a request is real opens the window at once: transport-wide feedback or a NACK
/// reporting a lost packet, or the client's own stats showing its keyframe-request counter moved.
/// The client's out-of-band request (opcode 0x64) never comes through here at all.
#[derive(Debug)]
pub struct PliBackoff {
    window_ns: u64,
    last_admit_ns: Option<u64>,
    last_pli_ns: Option<u64>,
    evidence: bool,
    pub suppressed: u64,
}

impl Default for PliBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl PliBackoff {
    pub fn new() -> Self {
        Self {
            window_ns: 0,
            last_admit_ns: None,
            last_pli_ns: None,
            evidence: false,
            suppressed: 0,
        }
    }

    /// Something says the next request is genuine: a loss report, or the client counting one.
    pub fn note_evidence(&mut self) {
        self.evidence = true;
    }

    /// Whether this PLI/FIR may go on to the session's [`KeyframeGate`].
    pub fn allow(&mut self, now_ns: u64) -> bool {
        self.allow_with_cap(now_ns, PLI_BACKOFF_CAP_MS)
    }

    /// [`Self::allow`] with the cap chosen by the caller: [`PLI_BACKOFF_CAP_HEALTHY_MS`] while the
    /// client reports painting outside libwebrtc, [`PLI_BACKOFF_CAP_MS`] otherwise.
    pub fn allow_with_cap(&mut self, now_ns: u64, cap_ms: u64) -> bool {
        let quiet = self
            .last_pli_ns
            .is_none_or(|t| now_ns.saturating_sub(t) >= PLI_QUIET_RESET_MS * 1_000_000);
        self.last_pli_ns = Some(now_ns);
        if quiet || std::mem::take(&mut self.evidence) {
            self.window_ns = 0;
        }
        if let Some(t) = self.last_admit_ns {
            if now_ns.saturating_sub(t) < self.window_ns {
                self.suppressed += 1;
                return false;
            }
        }
        self.last_admit_ns = Some(now_ns);
        // Opened for the *next* request: a receiver satisfied by this answer never meets it.
        self.window_ns = (self.window_ns * 2).clamp(
            PLI_BACKOFF_FLOOR_MS * 1_000_000,
            cap_ms.max(PLI_BACKOFF_FLOOR_MS) * 1_000_000,
        );
        true
    }

    pub fn window_ms(&self) -> u64 {
        self.window_ns / 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// libwebrtc's starving-decoder cadence, 30 s of it.
    fn admitted_under_starvation(b: &mut PliBackoff, every_ms: u64, seconds: u64) -> u32 {
        let mut n = 0;
        let mut t = 1_000 * MS;
        let end = t + seconds * 1_000 * MS;
        while t < end {
            if b.allow(t) {
                n += 1;
            }
            t += every_ms * MS;
        }
        n
    }

    #[test]
    fn a_receiver_that_never_stops_asking_backs_off_to_the_cap() {
        let mut b = PliBackoff::new();
        let n = admitted_under_starvation(&mut b, 200, 30);
        // 200, 400, 800, 1600 then 2000 ms apart: about 18 in 30 s against the 150 that a
        // 200 ms gate admits.
        assert!(
            n <= 20,
            "starvation PLIs must collapse to the cap; got {n} in 30 s"
        );
        assert!(n >= 14, "and must still be answered at the cap; got {n}");
        assert_eq!(b.window_ms(), PLI_BACKOFF_CAP_MS);
    }

    #[test]
    fn a_painting_webcodecs_client_is_not_sent_an_idr_for_every_residual_pli() {
        // The page that hands keyframes back to the receiver leaves libwebrtc one PLI about every
        // 3 s. Under the 2 s cap every one of them was an IDR; under the healthy cap they settle
        // at one per 10 s.
        let mut b = PliBackoff::new();
        let mut n = 0;
        for i in 0..40u64 {
            if b.allow_with_cap(1_000 * MS + i * 3_000 * MS, PLI_BACKOFF_CAP_HEALTHY_MS) {
                n += 1;
            }
        }
        assert!(
            n <= 18,
            "120 s of residual PLIs must not be ~40 IDRs; got {n}"
        );
        let mut b2 = PliBackoff::new();
        let mut n2 = 0;
        for i in 0..40u64 {
            if b2.allow(1_000 * MS + i * 3_000 * MS) {
                n2 += 1;
            }
        }
        assert_eq!(
            n2, 40,
            "the default cap answers every one — the reason for the healthy cap"
        );
    }

    #[test]
    fn a_decoder_one_keyframe_fixes_is_answered_at_once_every_time() {
        let mut b = PliBackoff::new();
        // One request, a few seconds of health, another: each one is the first in a while.
        for i in 0..10u64 {
            assert!(
                b.allow(1_000 * MS + i * 5_000 * MS),
                "request {i} was delayed"
            );
        }
        assert_eq!(b.suppressed, 0);
    }

    #[test]
    fn evidence_of_loss_opens_the_window_immediately() {
        let mut b = PliBackoff::new();
        admitted_under_starvation(&mut b, 200, 10);
        // Keep the starvation cadence going until a request is actually held back.
        let mut t = 11_000 * MS;
        while b.allow(t) {
            t += 200 * MS;
        }
        b.note_evidence();
        assert!(
            b.allow(t + 200 * MS),
            "a PLI after reported loss must not wait out the backoff"
        );
    }

    #[test]
    fn the_first_request_of_a_session_is_never_held() {
        let mut b = PliBackoff::new();
        assert!(b.allow(0));
    }

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

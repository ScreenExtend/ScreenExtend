//! How often one session is allowed to ask the encoder for an IDR.
//!
//! Four paths reach for a keyframe: an RTCP PLI, an RTCP FIR, a payloader error, and a viewer
//! the broadcast lagged. With an infinite GOP an IDR is 5-20x a P-frame, so answering each
//! signal on its own turns a transient into a storm. The gate admits one request per window,
//! and the window comes from the measured round trip: whatever the client says within one
//! round trip of a request is about the previous keyframe.

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
/// RTT alone is the wrong instrument there: a relayed session with a short ICE pair clamps to
/// [`MIN_IDR_INTERVAL_MS`] and a struggling client is answered with up to five 1080p IDRs a
/// second (measured 3.2/s through the public relay: 365 IDRs in 5092 frames, against 2 in 1352
/// on a direct LAN session). Each one overflows the constrained link and causes the next request.
///
/// Rolling intra refresh is off on live joins, so nothing rebuilds the picture between
/// keyframes: a relayed client that lost one waits for this window to pass.
const RELAY_MIN_IDR_INTERVAL_MS: u64 = 1000;
const RELAY_MAX_IDR_INTERVAL_MS: u64 = 3000;

/// Between the two: private, but a router hop away. Half the relay floor.
const UNKNOWN_MIN_IDR_INTERVAL_MS: u64 = 500;
const UNKNOWN_MAX_IDR_INTERVAL_MS: u64 = 2000;

// the tiers only make sense in this order, so the compiler checks it
const _: () = assert!(RELAY_MIN_IDR_INTERVAL_MS > UNKNOWN_MIN_IDR_INTERVAL_MS);
const _: () = assert!(UNKNOWN_MIN_IDR_INTERVAL_MS > MIN_IDR_INTERVAL_MS);
const _: () = assert!(RELAY_MAX_IDR_INTERVAL_MS >= RELAY_MIN_IDR_INTERVAL_MS);

#[derive(Debug)]
pub struct KeyframeGate {
    last_request_ns: AtomicU64,
    interval_ns: AtomicU64,
    /// The clamp `observe_rtt_ms` applies, in milliseconds. Fixed for the session: the locality
    /// it comes from is decided before the handshake.
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
    /// A same-subnet peer keeps the tight window: it can absorb the burst and would feel the
    /// wait. Everything else is throttled, because on those paths the IDR is what causes the
    /// next loss. See [`RELAY_MIN_IDR_INTERVAL_MS`].
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
        self.allow_counting(now_ns, true)
    }

    /// The same, without counting a refusal as a suppressed request: for an ask that is retried
    /// until it gets through, which would otherwise inflate that count once per retry.
    pub fn allow_uncounted(&self, now_ns: u64) -> bool {
        self.allow_counting(now_ns, false)
    }

    /// Take the window whatever holds it, for the one request that may not wait.
    pub fn claim(&self, now_ns: u64) {
        self.last_request_ns.store(now_ns.max(1), Ordering::Relaxed);
        self.admitted.fetch_add(1, Ordering::Relaxed);
    }

    /// The packet distance's ask: the window if it is free, taken anyway when `may_force`.
    pub fn ask_for_distance(&self, now_ns: u64, may_force: bool) -> DistanceAsk {
        if self.allow_uncounted(now_ns) {
            DistanceAsk::Admitted
        } else if may_force {
            self.claim(now_ns);
            DistanceAsk::Forced
        } else {
            DistanceAsk::Refused
        }
    }

    fn allow_counting(&self, now_ns: u64, count_refusal: bool) -> bool {
        let interval = self.interval_ns.load(Ordering::Relaxed);
        let mut last = self.last_request_ns.load(Ordering::Relaxed);
        loop {
            // 0 means "never asked", so the first request (the one on `Connected`) is always
            // admitted
            if last != 0 && now_ns.saturating_sub(last) < interval {
                if count_refusal {
                    self.suppressed.fetch_add(1, Ordering::Relaxed);
                }
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
/// the page: about one every 3 s, and the IDRs that answer them fix nothing. At a 10 s cap a
/// real browser was sent 31 in 5 minutes, each up to ~2 MB and a ~250 ms pause in painting.
/// The page's own requests arrive as evidence (its keyframe counter moves) and open the window
/// at once, as do a NACK and transport-wide loss. `SE_PLI_HEALTHY_CAP_MS` overrides it for a
/// measurement ([`pli_backoff_cap_healthy_ms`]).
pub const PLI_BACKOFF_CAP_HEALTHY_MS: u64 = 60_000;

/// [`PLI_BACKOFF_CAP_HEALTHY_MS`], or `SE_PLI_HEALTHY_CAP_MS` when set to a number of
/// milliseconds (read once; values below the 200 ms floor are raised to it by the back-off).
pub fn pli_backoff_cap_healthy_ms() -> u64 {
    static CAP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        healthy_cap_from(crate::envcfg::var("SE_PLI_HEALTHY_CAP_MS").ok().as_deref())
    })
}

fn healthy_cap_from(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(PLI_BACKOFF_CAP_HEALTHY_MS)
}

/// A PLI-free stretch this long means whatever was asking has been satisfied.
const PLI_QUIET_RESET_MS: u64 = 2 * PLI_BACKOFF_CAP_MS;

/// Rate limit for keyframe requests that arrive as RTCP PLI/FIR, on top of [`KeyframeGate`].
///
/// A receiver whose frames are consumed by an encoded transform (every WebCodecs client page,
/// the relay's older copy included) never feeds libwebrtc's own decoder, and libwebrtc answers a
/// decoder that has had nothing decodable for 200 ms with a keyframe request, forever: 117 PLIs
/// measured in a 30 s static window, an IDR every ~280 ms. Nothing in the RTCP packet tells
/// that receiver from one that really lost its picture, so this goes by behaviour: each
/// answered PLI that is followed by another doubles the window, up to [`PLI_BACKOFF_CAP_MS`],
/// and a quiet stretch closes it again.
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
            // the cap in force now, not the one the window grew under: a client that stops
            // being healthy waits at most the ordinary cap
            let cap_ns = cap_ms.max(PLI_BACKOFF_FLOOR_MS) * 1_000_000;
            if now_ns.saturating_sub(t) < self.window_ns.min(cap_ns) {
                self.suppressed += 1;
                return false;
            }
        }
        self.last_admit_ns = Some(now_ns);
        // opened for the next request: a receiver this answer satisfies never meets it
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

/// How far a delta may get from its keyframe, in RTP sequence numbers, on the receivers that have
/// a limit.
///
/// Chrome before 141 receiving through the legacy encoded-streams transform stops handing frames
/// to the page once a delta is about 2^15 packets past the last keyframe, until the next
/// keyframe. Measured on Chrome 128 at 32,591 to 34,314 packets in four sessions out of four;
/// current Chrome ran 53,000 packets without a keyframe and did not stop.
pub const PACKET_DISTANCE_LIMIT: u32 = 1 << 15;

/// Where the writer starts asking for an IDR, in packets past the last one.
///
/// The room left under the limit has to hold what goes out while the ask waits: the gate's window
/// when another request has just claimed it, and the frames already in the encoder. So it is
/// `max(limit / 4, rate x (gate interval + 0.5 s))`, never leaving the bound under `limit / 8`.
/// At the 2,300 packets a second the Chrome 128 measurement ran at that is 24,576; at 10,000 a
/// second (100 Mbit/s) on a LAN gate of 1 s, 17,768; relayed with a 3 s window, the floor of 4,096.
pub fn packet_distance_bound(limit: u32, packets_per_s: u32, gate_interval_ms: u64) -> u32 {
    let held = u64::from(packets_per_s) * (gate_interval_ms + 500) / 1000;
    let margin = held.max(u64::from(limit / 4));
    (u64::from(limit).saturating_sub(margin) as u32).max(limit / 8)
}

/// Past this the ask may go round the gate: whatever holds the window, the IDR has to be on its
/// way with half a second of packets still to spare. It cannot become a storm: `on_frame` allows
/// it at most once per `limit` packets, and the requester takes the window only when the gated
/// ask it tries first is refused.
pub fn packet_distance_must(limit: u32, packets_per_s: u32, bound: u32) -> u32 {
    let spare = (packets_per_s / 2).max(limit / 32);
    limit.saturating_sub(spare).max(bound)
}

/// What one frame's count led to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistanceAsk {
    None,
    /// The gate admitted the ask.
    Admitted,
    /// The gate refused it; the count keeps running and the ask is retried.
    Refused,
    /// Near the limit with the gated ask refused: sent round the gate.
    Forced,
}

/// What `on_frame` lets the requester do this time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistanceRequest {
    /// Past the must point, and no force in the last `limit` packets: if the gate refuses, the
    /// window may be taken.
    pub may_force: bool,
    /// The first force of this run since the last IDR; later ones are not logged.
    pub first_force: bool,
}

/// Sequence numbers one viewer has been sent since the last IDR it was sent, and that viewer's
/// packet rate.
///
/// Owned by that viewer's RTP writer task and touched once per frame, so plain integers: no lock
/// and nothing per packet. The writer stamps every sequence number the receiver sees: there is no
/// RTX stream, and a NACK is answered by resending the packet under its original number, so the
/// writer's count is the receiver's.
#[derive(Debug)]
pub struct PacketDistance {
    limit: u32,
    since_idr: u32,
    /// Where the next gated ask is due once the bound has been passed: the bound moves with the
    /// rate, so before the first ask the bound itself decides.
    next_retry: Option<u32>,
    /// The count at the last ask that went round the gate, in this run.
    last_force_at: Option<u32>,
    window_start_ns: u64,
    window_packets: u64,
    rate: u32,
    short_start_ns: u64,
    short_packets: u64,
    short_rate: u32,
}

impl Default for PacketDistance {
    fn default() -> Self {
        Self::new()
    }
}

impl PacketDistance {
    pub fn new() -> Self {
        Self::with_limit(PACKET_DISTANCE_LIMIT)
    }

    /// The same mechanism against a smaller limit, so a test can reach it in seconds.
    pub fn with_limit(limit: u32) -> Self {
        Self {
            limit,
            since_idr: 0,
            next_retry: None,
            last_force_at: None,
            window_start_ns: 0,
            window_packets: 0,
            rate: 0,
            short_start_ns: 0,
            short_packets: 0,
            short_rate: 0,
        }
    }

    pub fn since_idr(&self) -> u32 {
        self.since_idr
    }

    /// The highest of: packets a second over the last full second, over the last full quarter
    /// second, and over the second in progress once a quarter of it has gone by. The quarter
    /// second is what sees a burst after a quiet stretch, which the second averages away.
    pub fn packets_per_s(&self, now_ns: u64) -> u32 {
        let elapsed = now_ns.saturating_sub(self.window_start_ns);
        let mut rate = self.rate.max(self.short_rate);
        if self.window_start_ns != 0 && elapsed >= 250_000_000 {
            rate = rate.max(per_s(self.window_packets, elapsed));
        }
        rate
    }

    fn note_rate(&mut self, packets: u32, now_ns: u64) {
        if self.window_start_ns == 0 {
            self.window_start_ns = now_ns.max(1);
            self.short_start_ns = now_ns.max(1);
        }
        self.window_packets += u64::from(packets);
        self.short_packets += u64::from(packets);
        let elapsed = now_ns.saturating_sub(self.window_start_ns);
        if elapsed >= 1_000_000_000 {
            self.rate = per_s(self.window_packets, elapsed);
            self.window_start_ns = now_ns;
            self.window_packets = 0;
        }
        let elapsed = now_ns.saturating_sub(self.short_start_ns);
        if elapsed >= 250_000_000 {
            self.short_rate = per_s(self.short_packets, elapsed);
            self.short_start_ns = now_ns;
            self.short_packets = 0;
        }
    }

    /// One frame went out as `packets` sequence numbers at `now_ns`. Any IDR, whatever asked for
    /// it, starts the count again, its own packets included. Past the bound, and every
    /// `limit / 32` packets after it until an IDR goes out, `applies` says whether this viewer
    /// needs the bound and `ask` makes the request, saying what it did.
    pub fn on_frame(
        &mut self,
        packets: u32,
        idr: bool,
        now_ns: u64,
        gate_interval_ms: u64,
        applies: impl FnOnce() -> bool,
        ask: impl FnOnce(DistanceRequest) -> DistanceAsk,
    ) -> DistanceAsk {
        self.note_rate(packets, now_ns);
        if idr {
            // assumed to reach the receiver: one lost for good leaves it nothing to decode, it
            // asks for a keyframe, and the IDR that answers resets the count again. The healthy
            // back-off cap does not hold that ask: a client with no keyframe reports no painting
            self.since_idr = packets;
            self.next_retry = None;
            self.last_force_at = None;
            return DistanceAsk::None;
        }
        self.since_idr = self.since_idr.saturating_add(packets);
        let rate = self.packets_per_s(now_ns);
        let bound = packet_distance_bound(self.limit, rate, gate_interval_ms);
        if self.since_idr < self.next_retry.unwrap_or(bound) {
            return DistanceAsk::None;
        }
        self.next_retry = Some(self.since_idr.saturating_add(self.limit / 32));
        if !applies() {
            return DistanceAsk::None;
        }
        // a full `limit` between two forces, or an encoder that never produces an IDR would be
        // forced at every retry
        let rested = self
            .last_force_at
            .is_none_or(|at| self.since_idr >= at.saturating_add(self.limit));
        let request = DistanceRequest {
            may_force: rested && self.since_idr >= packet_distance_must(self.limit, rate, bound),
            first_force: self.last_force_at.is_none(),
        };
        let asked = ask(request);
        if asked == DistanceAsk::Forced {
            self.last_force_at = Some(self.since_idr);
        }
        asked
    }
}

fn per_s(packets: u64, elapsed_ns: u64) -> u32 {
    (packets * 1_000_000_000 / elapsed_ns.max(1)).min(u64::from(u32::MAX)) as u32
}

/// Whether an RTP payload carries part of an IDR slice: a single NAL unit, the first or any later
/// fragment of one, or an aggregate holding one.
pub fn payload_carries_idr(payload: &[u8]) -> bool {
    const IDR: u8 = 5;
    const STAP_A: u8 = 24;
    const FU_A: u8 = 28;
    match payload.first().map(|b| b & 0x1f) {
        Some(IDR) => true,
        Some(FU_A) => payload.get(1).is_some_and(|h| h & 0x1f == IDR),
        Some(STAP_A) => {
            let mut i = 1usize;
            while i + 2 < payload.len() {
                let len = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                if payload[i + 2] & 0x1f == IDR {
                    return true;
                }
                i += 2 + len;
            }
            false
        }
        _ => false,
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
        // 200, 400, 800, 1600 then 2000 ms apart: about 18 in 30 s, against 150 for a 200 ms gate
        assert!(
            n <= 20,
            "starvation PLIs must collapse to the cap; got {n} in 30 s"
        );
        assert!(n >= 14, "and must still be answered at the cap; got {n}");
        assert_eq!(b.window_ms(), PLI_BACKOFF_CAP_MS);
    }

    #[test]
    fn a_painting_webcodecs_client_is_not_sent_an_idr_for_every_residual_pli() {
        // a residual PLI about every 3 s: the 2 s cap answers every one, the healthy cap one
        // per 60 s
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
            "the default cap answers every one (the reason for the healthy cap)"
        );
    }

    #[test]
    fn a_window_grown_under_the_healthy_cap_narrows_when_the_cap_does() {
        let mut b = PliBackoff::new();
        let mut t = 1_000 * MS;
        for _ in 0..600 {
            b.allow_with_cap(t, PLI_BACKOFF_CAP_HEALTHY_MS);
            t += 200 * MS;
        }
        assert!(b.window_ms() > 30_000);
        // the client stops painting: the next answers come at most 2 s apart, not 60
        let mut waited = 0;
        while !b.allow_with_cap(t, PLI_BACKOFF_CAP_MS) {
            t += 200 * MS;
            waited += 200;
            assert!(waited <= PLI_BACKOFF_CAP_MS, "{waited} ms");
        }
    }

    #[test]
    fn the_healthy_cap_is_a_minute_and_overridable() {
        assert_eq!(PLI_BACKOFF_CAP_HEALTHY_MS, 60_000);
        assert_eq!(healthy_cap_from(None), 60_000);
        assert_eq!(healthy_cap_from(Some("10000")), 10_000);
        assert_eq!(healthy_cap_from(Some(" 30000 ")), 30_000);
        assert_eq!(healthy_cap_from(Some("lots")), 60_000);
        // five minutes of residual PLIs every 3 s: the window opens to the cap and stays there,
        // so after the first ~100 s of doubling only one IDR a minute goes out
        let mut b = PliBackoff::new();
        let mut admitted = Vec::new();
        for i in 0..100u64 {
            let t = 1_000 * MS + i * 3_000 * MS;
            if b.allow_with_cap(t, PLI_BACKOFF_CAP_HEALTHY_MS) {
                admitted.push(t / MS);
            }
        }
        assert_eq!(b.window_ms(), PLI_BACKOFF_CAP_HEALTHY_MS);
        assert!(admitted.len() <= 12, "{admitted:?}");
        let late = admitted.iter().filter(|&&t| t >= 120_000).count();
        assert!(
            late <= 4,
            "after two minutes at most one a minute: {admitted:?}"
        );
        // the non-healthy cap and evidence are untouched
        let mut c = PliBackoff::new();
        for i in 0..20u64 {
            c.allow(1_000 * MS + i * 200 * MS);
        }
        assert_eq!(c.window_ms(), PLI_BACKOFF_CAP_MS);
        c.note_evidence();
        assert!(c.allow(5_200 * MS));
    }

    #[test]
    fn a_decoder_one_keyframe_fixes_is_answered_at_once_every_time() {
        let mut b = PliBackoff::new();
        // one request, a few seconds of health, another: each one is the first in a while
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
        // keep the starvation cadence going until a request is held back
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
        // twenty PLIs over the next 100 ms (the client's backlog, all describing the same
        // loss) must produce no further encoder work
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
        // RTT is the wrong instrument for a path whose problem is capacity, so the floor is
        // the locality's, not the LAN's
        use super::super::webrtc_session::SessionLocality;
        let g = KeyframeGate::for_locality(SessionLocality::CrossNetwork);
        g.observe_rtt_ms(1.0);
        assert_eq!(
            g.interval_ms(),
            RELAY_MIN_IDR_INTERVAL_MS,
            "a short round trip must not unlock a relayed path's IDR rate"
        );

        // ten seconds of a 3.2/s request storm, started off 0 because `allow` reads 0 as
        // "never asked"
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
        // a window, not a drop: a client that really lost its reference chain still gets a
        // keyframe inside every window
        assert!(
            admitted >= 8,
            "the gate must keep answering: a starved client sits black; got {admitted}"
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

    /// A viewer's writer, cut down to what the packet distance sees: frames of `packets`
    /// sequence numbers at 120 Hz, a shared gate, and an encoder that turns a request into an
    /// IDR a few frames later.
    struct Writer {
        distance: PacketDistance,
        legacy: bool,
        admitted: Vec<u32>,
        refused: Vec<u32>,
        forced: Vec<u32>,
        most: u32,
    }

    impl Writer {
        fn new(legacy: bool) -> Self {
            Self {
                distance: PacketDistance::new(),
                legacy,
                admitted: Vec::new(),
                refused: Vec::new(),
                forced: Vec::new(),
                most: 0,
            }
        }

        fn frame(&mut self, packets: u32, idr: bool, gate: &KeyframeGate, now_ns: u64) -> bool {
            let legacy = self.legacy;
            let since = self.distance.since_idr().saturating_add(packets);
            let asked = self.distance.on_frame(
                packets,
                idr,
                now_ns,
                gate.interval_ms(),
                || legacy,
                |request| gate.ask_for_distance(now_ns, request.may_force),
            );
            match asked {
                DistanceAsk::Admitted => self.admitted.push(since),
                DistanceAsk::Refused => self.refused.push(since),
                DistanceAsk::Forced => self.forced.push(since),
                DistanceAsk::None => {}
            }
            self.most = self.most.max(self.distance.since_idr());
            matches!(asked, DistanceAsk::Admitted | DistanceAsk::Forced)
        }
    }

    const FRAME_NS: u64 = 8_333_333;
    const LIMIT: u32 = PACKET_DISTANCE_LIMIT;

    /// Run `frames` frames through the viewers. A request becomes an IDR for every viewer
    /// `latency` frames later, as one shared encoder does; `hold(n, now)` may take the gate's
    /// window at frame `n` with something that produces no IDR (a long-term-reference repair).
    #[allow(clippy::too_many_arguments)]
    fn run(
        viewers: &mut [Writer],
        joins_at: &[u64],
        gate: &KeyframeGate,
        frames: u64,
        packets: u32,
        latency: u64,
        mut hold: impl FnMut(u64, u64),
    ) -> u64 {
        let mut idr_at: Option<u64> = None;
        let mut idrs = 0;
        for n in 0..frames {
            let now = 1 + n * FRAME_NS;
            hold(n, now);
            let idr = idr_at == Some(n);
            if idr {
                idr_at = None;
                idrs += 1;
            }
            for (v, at) in viewers.iter_mut().zip(joins_at) {
                if n >= *at && v.frame(packets, idr, gate, now) && idr_at.is_none() {
                    idr_at = Some(n + latency);
                }
            }
        }
        idrs
    }

    fn lan_gate(interval_ms: u64) -> KeyframeGate {
        let g = KeyframeGate::new();
        g.observe_rtt_ms(interval_ms as f64 / 2.0);
        g
    }

    fn relayed_gate() -> KeyframeGate {
        let g =
            KeyframeGate::for_locality(super::super::webrtc_session::SessionLocality::CrossNetwork);
        g.observe_rtt_ms(10_000.0);
        g
    }

    #[test]
    fn the_bound_leaves_room_for_the_gate_at_the_packet_rate() {
        assert_eq!(packet_distance_bound(LIMIT, 2_300, 200), 24_576);
        assert_eq!(packet_distance_bound(LIMIT, 10_000, 1_000), 17_768);
        assert_eq!(packet_distance_bound(LIMIT, 10_000, 3_000), 4_096);
        assert_eq!(packet_distance_bound(LIMIT, 0, 3_000), 24_576);
        assert_eq!(packet_distance_bound(4_096, 2_000, 200), 2_696);
    }

    #[test]
    fn the_rate_is_measured_from_what_went_out() {
        let mut d = PacketDistance::new();
        for n in 0..240u64 {
            d.on_frame(
                84,
                n == 0,
                1 + n * FRAME_NS,
                200,
                || false,
                |_| DistanceAsk::Refused,
            );
        }
        let rate = d.packets_per_s(1 + 240 * FRAME_NS);
        assert!((9_500..=10_600).contains(&rate), "{rate}");
    }

    #[test]
    fn any_idr_starts_the_count_again() {
        let gate = lan_gate(200);
        let mut w = Writer::new(true);
        for n in 0..1000 {
            w.frame(20, false, &gate, 1 + n * FRAME_NS);
        }
        assert_eq!(w.distance.since_idr(), 20_000);
        // an IDR a PLI asked for, or the session's first: its own packets start the count
        w.frame(150, true, &gate, 1 + 1000 * FRAME_NS);
        assert_eq!(w.distance.since_idr(), 150);
        for n in 1001..2200 {
            w.frame(20, false, &gate, 1 + n * FRAME_NS);
        }
        assert!(
            w.admitted.is_empty() && w.refused.is_empty(),
            "24,130 packets is under the bound"
        );
    }

    #[test]
    fn asks_once_at_the_bound_and_again_only_a_full_distance_later() {
        let gate = lan_gate(200);
        let mut v = [Writer::new(true)];
        // 120 Hz, 20 packets a frame (2,400 a second), an IDR 3 frames after the request
        let idrs = run(&mut v, &[0], &gate, 120 * 60, 20, 3, |_, _| {});
        let w = &v[0];
        assert!(w.refused.is_empty() && w.forced.is_empty());
        assert!(idrs >= 5, "one IDR about every 10 s over a minute: {idrs}");
        let bound = packet_distance_bound(LIMIT, 2_400, 200);
        for at in &w.admitted {
            assert!((bound..bound + 20).contains(at), "{at} against {bound}");
        }
        assert!(w.most < LIMIT, "{} reached", w.most);
    }

    /// The gate's window taken by something that sends no IDR just before the bound: every
    /// ask in that window is refused, and the IDR still lands under the limit.
    fn held_before_the_bound(gate: &KeyframeGate, packets: u32) -> Writer {
        let mut v = [Writer::new(true)];
        let rate = packets * 120;
        let bound = packet_distance_bound(LIMIT, rate, gate.interval_ms());
        let at = u64::from(bound / packets).saturating_sub(5);
        run(&mut v, &[0], gate, 120 * 40, packets, 3, |n, now| {
            if n == at {
                assert!(gate.allow(now));
            }
        });
        let [w] = v;
        assert!(!w.refused.is_empty(), "the window refused an ask");
        for pair in w.refused.windows(2).take_while(|p| p[1] > p[0]) {
            assert!(pair[1] - pair[0] >= LIMIT / 32, "{pair:?}");
        }
        assert!(
            w.most < LIMIT,
            "{} reached at {rate} packets a second",
            w.most
        );
        w
    }

    #[test]
    fn a_refused_ask_still_lands_under_the_limit_on_a_relayed_gate() {
        held_before_the_bound(&relayed_gate(), 20);
    }

    #[test]
    fn at_ten_thousand_packets_a_second_on_a_lan_gate_the_idr_lands_under_the_limit() {
        // the bound alone leaves room for the 1 s window; a retry that falls past the last safe
        // point takes the window instead of waiting for the next one
        let w = held_before_the_bound(&lan_gate(1_000), 84);
        assert!(!w.admitted.is_empty() || !w.forced.is_empty());
    }

    #[test]
    fn at_ten_thousand_packets_a_second_on_a_relayed_gate_the_last_ask_goes_round_it() {
        // a 3 s window is 30,000 packets here, more than any bound leaves: the ask goes round
        // the gate. Every cycle overlaps the window the one before took, so each one does,
        // about every 2.8 s, never more than once per cycle
        let w = held_before_the_bound(&relayed_gate(), 84);
        assert!(!w.forced.is_empty());
        assert!(w.forced.iter().all(|&at| at < LIMIT));
    }

    #[test]
    fn an_encoder_that_never_answers_is_forced_once_per_limit_and_never_holds_the_gate_longer() {
        // 10,000 packets a second, a relayed 3 s window, and an encoder that takes every request
        // and produces no IDR. A real request asks every frame, the way a stalled receiver does
        let gate = relayed_gate();
        let window_ns = gate.interval_ms() * 1_000_000;
        let mut v = [Writer::new(true)];
        let mut real: Vec<u64> = Vec::new();
        let seconds = 40u64;
        run(&mut v, &[0], &gate, 120 * seconds, 84, 1 << 40, |_, now| {
            if gate.allow(now) {
                real.push(now);
            }
        });
        let w = &v[0];
        let sent = 84 * 120 * seconds as u32;
        assert!(!w.forced.is_empty());
        assert!(
            w.forced.len() as u32 <= sent / LIMIT + 1,
            "{} forces in {sent} packets",
            w.forced.len()
        );
        for pair in w.forced.windows(2) {
            assert!(pair[1] - pair[0] >= LIMIT, "{pair:?}");
        }
        let longest = real.windows(2).map(|p| p[1] - p[0]).max().unwrap_or(0);
        assert!(
            longest <= 2 * window_ns + FRAME_NS,
            "a real request waited {} ms",
            longest / 1_000_000
        );
    }

    #[test]
    fn a_burst_after_a_quiet_stretch_is_seen_within_a_quarter_second() {
        let mut d = PacketDistance::new();
        let mut n = 0u64;
        // two seconds at about 2,000 packets a second, then 15,000
        while n < 240 {
            d.on_frame(
                17,
                n == 0,
                1 + n * FRAME_NS,
                200,
                || false,
                |_| DistanceAsk::Refused,
            );
            n += 1;
        }
        while n < 240 + 33 {
            d.on_frame(
                125,
                false,
                1 + n * FRAME_NS,
                200,
                || false,
                |_| DistanceAsk::Refused,
            );
            n += 1;
        }
        let rate = d.packets_per_s(1 + n * FRAME_NS);
        assert!(rate >= 14_000, "{rate} a quarter second into the burst");
    }

    #[test]
    fn refused_asks_are_not_counted_as_suppressed_requests() {
        let gate = relayed_gate();
        let before = gate.suppressed();
        held_before_the_bound(&gate, 20);
        assert_eq!(gate.suppressed(), before);
    }

    #[test]
    fn a_viewer_known_to_read_through_the_script_transform_is_never_the_one_asking() {
        let gate = lan_gate(200);
        let mut v = [Writer::new(false)];
        let idrs = run(&mut v, &[0], &gate, 120 * 60, 20, 3, |_, _| {});
        assert_eq!(idrs, 0);
        assert!(v[0].admitted.is_empty() && v[0].forced.is_empty());
    }

    #[test]
    fn two_viewers_one_joining_mid_stream_share_each_idr() {
        // one encoder: an IDR for one viewer is an IDR for both. The second joins 20,000 packets
        // in; its own count starts at zero, and the first viewer's IDR resets it too
        let gate = lan_gate(200);
        let mut v = [Writer::new(true), Writer::new(true)];
        let idrs = run(&mut v, &[0, 1000], &gate, 120 * 60, 20, 3, |_, _| {});
        assert!(idrs >= 5);
        assert!(v[0].most < LIMIT && v[1].most < LIMIT);
        let sent =
            v[0].admitted.len() + v[1].admitted.len() + v[0].forced.len() + v[1].forced.len();
        assert_eq!(sent as u64, idrs, "one request per IDR, not one per viewer");
        // a modern viewer beside a legacy one asks nothing, and its count still resets
        let gate = lan_gate(200);
        let mut v = [Writer::new(true), Writer::new(false)];
        run(&mut v, &[0, 0], &gate, 120 * 60, 20, 3, |_, _| {});
        assert!(v[1].admitted.is_empty() && v[1].most < LIMIT);
    }

    #[test]
    fn a_still_screen_is_never_asked() {
        // the 200 ms keepalive, a packet or two each, for ten minutes
        let gate = lan_gate(200);
        let mut w = Writer::new(true);
        for n in 0..(5 * 600u64) {
            w.frame(2, false, &gate, 1 + n * 200_000_000);
        }
        assert!(w.admitted.is_empty() && w.refused.is_empty() && w.forced.is_empty());
    }

    #[test]
    fn the_count_is_the_receivers_sequence_distance_across_a_wrap_and_resends() {
        use webrtc::rtp::sequence::{new_fixed_sequencer, Sequencer};
        // numbers stamped from just below the 16-bit wrap. A NACK is answered by resending the
        // packet under its original number, which stamps nothing and so counts nothing
        let seq = new_fixed_sequencer(65_000);
        let mut d = PacketDistance::new();
        let first = seq.next_sequence_number();
        d.on_frame(
            1,
            true,
            1,
            200,
            || true,
            |_| unreachable!("an IDR asks nothing"),
        );
        let mut last = first;
        for n in 0..300u64 {
            for _ in 0..10 {
                last = seq.next_sequence_number();
            }
            d.on_frame(
                10,
                false,
                1 + n * FRAME_NS,
                200,
                || true,
                |_| DistanceAsk::Admitted,
            );
        }
        let receiver_distance = u32::from(last.wrapping_sub(first)) + 1;
        assert_eq!(d.since_idr(), receiver_distance);
    }

    #[test]
    fn an_idr_is_recognised_in_every_payload_shape() {
        assert!(payload_carries_idr(&[0x65, 0x88]));
        assert!(payload_carries_idr(&[0x7c, 0x85, 0x00]), "FU-A start");
        assert!(payload_carries_idr(&[0x7c, 0x45, 0x00]), "FU-A end");
        assert!(
            !payload_carries_idr(&[0x7c, 0x81, 0x00]),
            "FU-A of a P slice"
        );
        assert!(!payload_carries_idr(&[0x41, 0x9a]));
        // STAP-A: an SPS, then an IDR slice
        assert!(payload_carries_idr(&[
            0x78, 0x00, 0x02, 0x67, 0x42, 0x00, 0x02, 0x65, 0x88
        ]));
        assert!(!payload_carries_idr(&[
            0x78, 0x00, 0x02, 0x67, 0x42, 0x00, 0x02, 0x68, 0xce
        ]));
        assert!(!payload_carries_idr(&[]));
    }
}

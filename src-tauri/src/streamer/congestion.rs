//! Send-side congestion control from transport-wide feedback.
//!
//! Before this the host had no delay signal at all. It advertised `transport-cc` and `goog-remb`
//! in the answer, ran `register_default_interceptors` — which installs the TWCC *receiver*, the
//! half a send-only peer has no use for — and drove the bitrate off RTCP receiver-report
//! `fraction_lost`, a number the browser refreshes about once a second. A loss-only controller
//! sampling a 1 Hz signal reacts only after the queue is already full, which on Wi-Fi (where
//! 802.11 link-layer ARQ hides loss until the queue is deep) is far too late.
//!
//! Two things changed. The writer now stamps the transport-wide sequence number itself, in the
//! same `write_rtp_with_extensions` call that already carries the playout-delay extension, so we
//! know exactly which transport sequence a given packet got — the thing
//! `interceptor::twcc::sender` will not tell us, because its counter is private. And this module
//! turns the resulting `TransportLayerCc` reports into the two signals that matter:
//!
//! - **Loss**, exactly and immediately: the status chunks name every sequence number the client
//!   did not receive, at the browser's ~50-100 ms feedback cadence rather than once a second.
//! - **Delay gradient**: pairing each feedback arrival time with the send time we recorded gives
//!   the one-way delay *variation* per packet, which is what rises while a queue is filling and
//!   before anything is dropped. The filter is libwebrtc's trendline estimator, reduced to its
//!   load-bearing parts and with its published constants.
//!
//! [`RembTracker`] stays alongside as a fallback: a client that ignores the extension makes
//! Chrome run its own receive-side estimator and report it as REMB, and that estimate is worth
//! having for free.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use webrtc::rtcp::transport_feedbacks::transport_layer_cc::{
    PacketStatusChunk, SymbolTypeTcc, TransportLayerCc,
};

/// TWCC reference time is in 1/64 s units.
const REFERENCE_TIME_UNIT_US: i64 = 64_000;

/// How many outstanding packets to remember send times for. At 20 Mbps and a 1188-byte payload
/// this is roughly two seconds of send history, comfortably more than a feedback round trip.
const SEND_HISTORY: usize = 4096;

/// One packet's departure, kept until its feedback arrives.
#[derive(Debug, Clone, Copy)]
struct SentPacket {
    seq: u16,
    send_us: i64,
    bytes: u32,
    /// Which access unit this packet belongs to, as a per-session ordinal.
    frame: u32,
}

/// How many in-flight frames to keep a per-frame tally for.
const FRAME_TALLIES: usize = 256;

/// One access unit's packets, until every one of them has been reported on.
///
/// Transport-wide feedback names every sequence number the client did and did not receive, so it
/// answers a question RTCP receiver reports cannot: *did this whole frame arrive?* That is exactly
/// the acknowledgement an encoder needs to use a long-term reference frame instead of an IDR for
/// loss recovery — VideoToolbox's `kVTEncodeFrameOptionKey_AcknowledgedLTRTokens` and NVENC's
/// reference invalidation both want it — and getting it here means it costs no client change, no
/// new control-channel opcode and no round trip beyond the one TWCC already pays.
#[derive(Debug, Clone, Copy)]
struct FrameTally {
    ordinal: u32,
    capture_ns: u64,
    sent: u32,
    received: u32,
    lost: u32,
}

impl FrameTally {
    fn decided(&self) -> bool {
        self.received + self.lost >= self.sent
    }

    fn intact(&self) -> bool {
        self.lost == 0 && self.received >= self.sent && self.sent > 0
    }
}

/// The transport-wide sequence numbers this session has put on the wire, with when.
#[derive(Debug)]
pub struct SendHistory {
    ring: VecDeque<SentPacket>,
    next_seq: u16,
    epoch: Instant,
    frames: VecDeque<FrameTally>,
}

impl SendHistory {
    pub fn new(epoch: Instant) -> Self {
        Self {
            ring: VecDeque::with_capacity(SEND_HISTORY),
            next_seq: 0,
            epoch,
            frames: VecDeque::with_capacity(FRAME_TALLIES),
        }
    }

    /// Claim the next transport-wide sequence number for a packet about to be written.
    ///
    /// `frame` is the writer's own per-session access-unit ordinal and `capture_ns` the host-clock
    /// instant that frame was captured — the identity the encoder already stamped on it, so an
    /// acknowledgement can be matched back to a reference frame without inventing a second one.
    pub fn record(&mut self, bytes: u32, now: Instant, frame: u32, capture_ns: u64) -> u16 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.ring.len() == SEND_HISTORY {
            self.ring.pop_front();
        }
        self.ring.push_back(SentPacket {
            seq,
            send_us: now.saturating_duration_since(self.epoch).as_micros() as i64,
            bytes,
            frame,
        });
        match self.frames.back_mut() {
            Some(t) if t.ordinal == frame => t.sent += 1,
            _ => {
                if self.frames.len() == FRAME_TALLIES {
                    self.frames.pop_front();
                }
                self.frames.push_back(FrameTally {
                    ordinal: frame,
                    capture_ns,
                    sent: 1,
                    received: 0,
                    lost: 0,
                });
            }
        }
        seq
    }

    /// Record one packet's fate against its frame.
    fn note(&mut self, frame: u32, received: bool) {
        if let Some(t) = self.frames.iter_mut().rev().find(|t| t.ordinal == frame) {
            if received {
                t.received += 1;
            } else {
                t.lost += 1;
            }
        }
    }

    /// The newest frame every packet of which the client has now reported receiving.
    ///
    /// A frame with even one lost packet is never acknowledged: the point of the signal is that
    /// the encoder may keep referencing that picture, and a decoder missing a slice of it cannot.
    /// Decided frames are dropped either way, so a lossy stretch does not accumulate tallies.
    fn take_decided_frames(&mut self, lost_out: &mut Vec<u64>) -> Option<(u32, u64)> {
        let mut newest: Option<(u32, u64)> = None;
        while let Some(front) = self.frames.front().copied() {
            if !front.decided() {
                break;
            }
            self.frames.pop_front();
            if front.intact() {
                newest = Some((front.ordinal, front.capture_ns));
            } else {
                lost_out.push(front.capture_ns);
            }
        }
        newest
    }

    fn lookup(&self, seq: u16) -> Option<SentPacket> {
        // `record` claims `next_seq` and increments by one, so the ring is dense: `ring[i].seq` is
        // always `ring[0].seq + i` modulo 2^16. That makes the position arithmetic rather than a
        // search, and the verification is the equality check on the slot it names — if the ring is
        // ever not dense the check fails and the scan below answers instead.
        //
        // It used to be only the scan, from the back, on the reasoning that "a linear scan from the
        // back finds a recent packet immediately". A TWCC report does not start at a recent packet:
        // it starts at `base_sequence_number`, the *oldest* sequence it has anything to say about,
        // so the first lookups walked most of the ring's 4096 entries — and `interpret` calls this
        // twice per reported symbol, with `feedback.history`'s blocking mutex held, on the same
        // mutex the RTP writer takes once per packet.
        let front = *self.ring.front()?;
        let idx = seq.wrapping_sub(front.seq) as usize;
        if let Some(p) = self.ring.get(idx) {
            if p.seq == seq {
                return Some(*p);
            }
        }
        self.ring.iter().rev().find(|p| p.seq == seq).copied()
    }

    /// Forget everything at or before `seq`, which the client has now reported on.
    fn retire_through(&mut self, seq: u16) {
        while let Some(front) = self.ring.front() {
            if front.seq.wrapping_sub(seq) < 0x8000 && front.seq != seq {
                break;
            }
            self.ring.pop_front();
        }
    }
}

/// What one `TransportLayerCc` report said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeedbackSummary {
    /// Packets the report covered and we had a send time for.
    pub matched: u32,
    /// Packets the report covered at all.
    pub reported: u32,
    /// Of those, how many the client did not receive.
    pub lost: u32,
    /// Bytes of the matched packets that arrived, over the span they arrived in.
    pub received_bps: u32,
    /// Host-clock capture time of the newest access unit the client received in full, when this
    /// report completed one. The encoder can use it as a long-term reference acknowledgement.
    pub acked_capture_ns: Option<u64>,
    /// Host-clock capture times of access units this report proved incomplete. Each one is a
    /// picture the encoder must stop referencing, or every frame predicted from it is undecodable
    /// on this client.
    pub lost_capture_ns: Vec<u64>,
}

impl FeedbackSummary {
    pub fn loss_fraction(&self) -> f64 {
        if self.reported == 0 {
            return 0.0;
        }
        self.lost as f64 / self.reported as f64
    }
}

/// Expand a report's status chunks into one symbol per covered sequence number.
fn expand_symbols(fb: &TransportLayerCc) -> Vec<SymbolTypeTcc> {
    let want = fb.packet_status_count as usize;
    let mut out = Vec::with_capacity(want);
    for chunk in &fb.packet_chunks {
        match chunk {
            PacketStatusChunk::RunLengthChunk(c) => {
                for _ in 0..c.run_length {
                    if out.len() == want {
                        break;
                    }
                    out.push(c.packet_status_symbol);
                }
            }
            PacketStatusChunk::StatusVectorChunk(c) => {
                for s in &c.symbol_list {
                    if out.len() == want {
                        break;
                    }
                    out.push(*s);
                }
            }
        }
        if out.len() == want {
            break;
        }
    }
    out
}

/// Turn one feedback report into loss, throughput and delay-variation samples.
///
/// `deltas_out` receives the per-packet one-way delay variation in milliseconds: positive means
/// the packet took longer to cross than its predecessor did, which is a queue filling.
pub fn interpret(
    fb: &TransportLayerCc,
    history: &mut SendHistory,
    deltas_out: &mut Vec<f64>,
) -> FeedbackSummary {
    deltas_out.clear();
    let symbols = expand_symbols(fb);
    if symbols.is_empty() {
        return FeedbackSummary::default();
    }

    let mut arrival_us = fb.reference_time as i64 * REFERENCE_TIME_UNIT_US;
    let mut delta_idx = 0usize;
    let mut summary = FeedbackSummary::default();

    let mut prev: Option<(i64, i64)> = None; // (send_us, arrival_us) of the last matched packet
    let mut first_arrival: Option<i64> = None;
    let mut last_arrival = 0i64;
    let mut bytes = 0u64;

    // Every symbol, received or not, is also a verdict on the frame its packet belonged to. That
    // is what makes a whole access unit acknowledgeable without a client change.
    let mut verdicts: Vec<(u32, bool)> = Vec::with_capacity(symbols.len());

    for (i, sym) in symbols.iter().enumerate() {
        let seq = fb.base_sequence_number.wrapping_add(i as u16);
        summary.reported += 1;
        let received = !matches!(sym, SymbolTypeTcc::PacketNotReceived);
        if let Some(p) = history.lookup(seq) {
            verdicts.push((p.frame, received));
        }
        match sym {
            SymbolTypeTcc::PacketNotReceived => {
                summary.lost += 1;
                continue;
            }
            SymbolTypeTcc::PacketReceivedWithoutDelta => {
                // Received, but the report carries no arrival time for it. It still counts
                // against loss; it just cannot contribute a gradient sample.
                continue;
            }
            _ => {}
        }
        let Some(d) = fb.recv_deltas.get(delta_idx) else {
            continue;
        };
        delta_idx += 1;
        arrival_us += d.delta;

        let Some(sent) = history.lookup(seq) else {
            continue;
        };
        summary.matched += 1;
        bytes += sent.bytes as u64;
        first_arrival.get_or_insert(arrival_us);
        last_arrival = arrival_us;

        if let Some((prev_send, prev_arrival)) = prev {
            let inter_departure = sent.send_us - prev_send;
            let inter_arrival = arrival_us - prev_arrival;
            // Reordered or duplicated feedback would otherwise inject a huge spurious gradient.
            if inter_departure >= 0 {
                deltas_out.push((inter_arrival - inter_departure) as f64 / 1000.0);
            }
        }
        prev = Some((sent.send_us, arrival_us));
    }

    if let Some(first) = first_arrival {
        let span_us = (last_arrival - first).max(0);
        if span_us > 0 {
            summary.received_bps = ((bytes as f64 * 8.0 * 1.0e6) / span_us as f64) as u32;
        }
    }

    // After the lookups, because `retire_through` drops the packets the verdicts came from.
    for (frame, received) in verdicts {
        history.note(frame, received);
    }
    summary.acked_capture_ns = history
        .take_decided_frames(&mut summary.lost_capture_ns)
        .map(|(_, capture_ns)| capture_ns);

    history.retire_through(
        fb.base_sequence_number
            .wrapping_add(symbols.len().saturating_sub(1) as u16),
    );
    summary
}

/// What the delay filter currently thinks the path is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkState {
    /// The queue is stable. Safe to probe upward.
    Normal,
    /// One-way delay is rising: a queue is filling and loss has not happened yet.
    Overuse,
    /// One-way delay is falling: a queue is draining. Hold, do not probe.
    Underuse,
}

/// libwebrtc's trendline estimator, reduced to the parts that decide the state.
///
/// The published constants are kept verbatim so the behaviour is comparable with the reference
/// implementation: smoothing 0.9, threshold gain 4.0, adaptive-threshold gains k_up 0.0087 and
/// k_down 0.039, an over-use dwell of 10 ms, and the threshold clamped to [6, 600] ms.
#[derive(Debug, Clone)]
pub struct DelayDetector {
    smoothed_ms: f64,
    threshold_ms: f64,
    overuse_since_ms: Option<f64>,
    now_ms: f64,
    last_update_ms: f64,
    state: NetworkState,
    samples: u64,
}

const TREND_SMOOTHING: f64 = 0.9;
const THRESHOLD_GAIN: f64 = 4.0;
const K_UP: f64 = 0.0087;
const K_DOWN: f64 = 0.039;
const OVERUSE_DWELL_MS: f64 = 10.0;
const THRESHOLD_MIN_MS: f64 = 6.0;
const THRESHOLD_MAX_MS: f64 = 600.0;
/// One sample must not move the adaptive threshold more than this, or a stalled feedback path
/// followed by a burst would re-baseline the detector in a single step.
const MAX_THRESHOLD_STEP_MS: f64 = 100.0;

impl Default for DelayDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayDetector {
    pub fn new() -> Self {
        Self {
            smoothed_ms: 0.0,
            threshold_ms: 12.5,
            overuse_since_ms: None,
            now_ms: 0.0,
            last_update_ms: 0.0,
            state: NetworkState::Normal,
            samples: 0,
        }
    }

    pub fn state(&self) -> NetworkState {
        self.state
    }

    pub fn trend_ms(&self) -> f64 {
        self.smoothed_ms
    }

    pub fn threshold_ms(&self) -> f64 {
        self.threshold_ms
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Feed one one-way delay variation sample, in milliseconds.
    pub fn observe(&mut self, delta_ms: f64) {
        if !delta_ms.is_finite() {
            return;
        }
        self.samples += 1;
        // The estimator's own clock advances with the samples it sees, which is what makes the
        // dwell time and the threshold adaptation independent of the feedback cadence.
        self.now_ms += delta_ms.abs().clamp(1.0, 50.0);
        self.smoothed_ms = TREND_SMOOTHING * self.smoothed_ms + (1.0 - TREND_SMOOTHING) * delta_ms;

        let modified = self.smoothed_ms * THRESHOLD_GAIN;
        if modified > self.threshold_ms {
            match self.overuse_since_ms {
                None => self.overuse_since_ms = Some(self.now_ms),
                Some(since) if self.now_ms - since >= OVERUSE_DWELL_MS => {
                    self.state = NetworkState::Overuse;
                }
                Some(_) => {}
            }
        } else {
            self.overuse_since_ms = None;
            self.state = if modified < -self.threshold_ms {
                NetworkState::Underuse
            } else {
                NetworkState::Normal
            };
        }

        self.adapt_threshold(modified);
    }

    fn adapt_threshold(&mut self, modified: f64) {
        let dt_ms = (self.now_ms - self.last_update_ms).clamp(0.0, MAX_THRESHOLD_STEP_MS);
        self.last_update_ms = self.now_ms;
        if modified.abs() > self.threshold_ms + 15.0 {
            // Way outside the band: this is a spike, not a trend. Do not let it drag the
            // threshold up, or the detector goes deaf.
            return;
        }
        let k = if modified.abs() < self.threshold_ms {
            K_DOWN
        } else {
            K_UP
        };
        self.threshold_ms += k * (modified.abs() - self.threshold_ms) * dt_ms;
        self.threshold_ms = self.threshold_ms.clamp(THRESHOLD_MIN_MS, THRESHOLD_MAX_MS);
    }
}

/// Multiplicative decrease applied on the first over-use, against the rate actually delivered.
const OVERUSE_DECREASE: f64 = 0.85;
/// Multiplicative increase per control interval while the path looks clean.
const NORMAL_INCREASE: f64 = 1.06;
/// Never let one control interval move the estimate below this fraction of its current value.
const MAX_SINGLE_CUT: f64 = 0.7;

/// AIMD rate control driven by [`DelayDetector`], the shape GCC uses.
#[derive(Debug, Clone)]
pub struct DelayBasedEstimate {
    estimate_bps: u32,
    min_bps: u32,
    max_bps: u32,
    last_decrease: Option<Instant>,
}

impl DelayBasedEstimate {
    pub fn new(start_bps: u32, min_bps: u32, max_bps: u32) -> Self {
        Self {
            estimate_bps: start_bps.clamp(min_bps, max_bps),
            min_bps,
            max_bps,
            last_decrease: None,
        }
    }

    pub fn current(&self) -> u32 {
        self.estimate_bps
    }

    /// Advance one control interval.
    ///
    /// `delivered_bps` is what the receiver actually got — the correct base for a multiplicative
    /// decrease, because cutting a fraction of a target we were never achieving does nothing.
    pub fn update(&mut self, state: NetworkState, delivered_bps: u32, now: Instant) -> u32 {
        let next = match state {
            NetworkState::Overuse => {
                // One decrease per round of over-use, not one per feedback packet.
                let too_soon = self
                    .last_decrease
                    .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_millis(250));
                if too_soon {
                    self.estimate_bps
                } else {
                    self.last_decrease = Some(now);
                    let base = if delivered_bps > 0 {
                        delivered_bps.min(self.estimate_bps)
                    } else {
                        self.estimate_bps
                    };
                    let floor = (self.estimate_bps as f64 * MAX_SINGLE_CUT) as u32;
                    ((base as f64 * OVERUSE_DECREASE) as u32).max(floor)
                }
            }
            NetworkState::Underuse => self.estimate_bps,
            NetworkState::Normal => (self.estimate_bps as f64 * NORMAL_INCREASE) as u32,
        };
        self.estimate_bps = next.clamp(self.min_bps, self.max_bps);
        self.estimate_bps
    }
}

/// The browser's own receive-side estimate, when it sends one.
///
/// Chrome picks between send-side and receive-side estimation per packet, on whether the
/// transport-wide sequence number is physically present. Once the writer stamps it, Chrome stops
/// emitting REMB — so this is the path for a client that does not negotiate the extension, and
/// its silence is not a fault.
#[derive(Debug, Clone, Default)]
pub struct RembTracker {
    last_bps: Option<u32>,
    reports: u64,
}

impl RembTracker {
    pub fn observe(&mut self, bitrate: f32) {
        if !bitrate.is_finite() || bitrate <= 0.0 {
            return;
        }
        self.reports += 1;
        self.last_bps = Some(bitrate as u32);
    }

    pub fn last_bps(&self) -> Option<u32> {
        self.last_bps
    }

    pub fn reports(&self) -> u64 {
        self.reports
    }
}

/// The one place the RTCP reader, the RTP writer and the bitrate driver meet.
///
/// Three tasks, three cadences: feedback arrives on the sender's RTCP read loop whenever the
/// client emits it, packets are stamped in the writer, and the controller runs on its own timer.
/// Everything crossing between them is an atomic or a short critical section — no task may ever
/// wait on another to make progress on the frame path.
#[derive(Debug, Default)]
pub struct SharedFeedback {
    /// Send times of packets still awaiting feedback. Written per packet by the writer, read per
    /// report by the RTCP task.
    pub history: std::sync::Mutex<Option<SendHistory>>,
    /// The delay filter. Only the RTCP task touches it; the driver reads the state out.
    pub detector: std::sync::Mutex<DelayDetector>,
    reported: std::sync::atomic::AtomicU64,
    lost: std::sync::atomic::AtomicU64,
    received_bps: std::sync::atomic::AtomicU32,
    remb_bps: std::sync::atomic::AtomicU32,
    twcc_reports: std::sync::atomic::AtomicU64,
    remb_reports: std::sync::atomic::AtomicU64,
    /// Times the local broadcast dropped frames on this viewer. A lagging viewer is an
    /// oversubscribed link, so the controller treats it as a congestion signal in its own right
    /// rather than only asking for a keyframe, which would make it worse.
    lag_events: std::sync::atomic::AtomicU64,
    /// Host-clock capture time of the newest access unit the client has confirmed receiving in
    /// full. Zero until one has been. This is the acknowledgement a long-term-reference encoder
    /// needs; nothing consumes it yet, which is why it is published rather than acted on.
    acked_capture_ns: std::sync::atomic::AtomicU64,
}

impl SharedFeedback {
    pub fn record_report(&self, s: &FeedbackSummary) {
        use std::sync::atomic::Ordering::Relaxed;
        self.reported.fetch_add(s.reported as u64, Relaxed);
        self.lost.fetch_add(s.lost as u64, Relaxed);
        if s.received_bps > 0 {
            self.received_bps.store(s.received_bps, Relaxed);
        }
        if let Some(ns) = s.acked_capture_ns {
            // Monotonic: a later report can only ever move this forward, and reports can be
            // reordered on the wire.
            let _ = self
                .acked_capture_ns
                .fetch_update(Relaxed, Relaxed, |cur| (ns > cur).then_some(ns));
        }
        self.twcc_reports.fetch_add(1, Relaxed);
    }

    /// The newest frame the client has confirmed receiving whole, by its host-clock capture time.
    ///
    /// Transport-wide feedback names every sequence number that did and did not arrive, so a whole
    /// access unit can be acknowledged from it with no client change, no new control-channel
    /// opcode and no round trip beyond the one TWCC already pays. That is exactly the input
    /// VideoToolbox's `kVTEncodeFrameOptionKey_AcknowledgedLTRTokens` and NVENC's reference
    /// invalidation want, and it is why the capture instant is the identity: the encoder already
    /// stamped it on the frame, so nothing new has to be carried to match it back.
    pub fn acked_frame_capture_ns(&self) -> Option<u64> {
        let ns = self
            .acked_capture_ns
            .load(std::sync::atomic::Ordering::Relaxed);
        (ns > 0).then_some(ns)
    }

    pub fn record_remb(&self, bps: u32) {
        use std::sync::atomic::Ordering::Relaxed;
        self.remb_bps.store(bps, Relaxed);
        self.remb_reports.fetch_add(1, Relaxed);
    }

    pub fn note_lag(&self) {
        self.lag_events
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Loss fraction since the last call, and how many packets it was measured over. Draining is
    /// deliberate: the controller wants "what happened in this interval", not a session average.
    pub fn take_loss(&self) -> Option<(f64, u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        let reported = self.reported.swap(0, Relaxed);
        let lost = self.lost.swap(0, Relaxed);
        if reported == 0 {
            return None;
        }
        Some((lost as f64 / reported as f64, reported))
    }

    pub fn take_lag_events(&self) -> u64 {
        self.lag_events
            .swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn received_bps(&self) -> u32 {
        self.received_bps.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn remb_bps(&self) -> Option<u32> {
        match self.remb_bps.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }

    pub fn twcc_reports(&self) -> u64 {
        self.twcc_reports.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn remb_reports(&self) -> u64 {
        self.remb_reports.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Which signal the session is actually flying on, for the capability-tier report.
    pub fn signal_name(&self) -> &'static str {
        if self.twcc_reports() > 0 {
            "twcc-delay-gradient"
        } else if self.remb_reports() > 0 {
            "remb"
        } else {
            "loss-only"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::rtcp::transport_feedbacks::transport_layer_cc::{
        RecvDelta, RunLengthChunk, StatusChunkTypeTcc, StatusVectorChunk, SymbolSizeTypeTcc,
    };

    fn epoch() -> Instant {
        Instant::now()
    }

    fn recv(delta_us: i64) -> RecvDelta {
        RecvDelta {
            type_tcc_packet: SymbolTypeTcc::PacketReceivedSmallDelta,
            delta: delta_us,
        }
    }

    #[test]
    fn send_history_hands_out_dense_sequence_numbers() {
        let e = epoch();
        let mut h = SendHistory::new(e);
        for i in 0..5u16 {
            assert_eq!(h.record(1200, e + Duration::from_millis(i as u64), 0, 0), i);
        }
    }

    #[test]
    fn send_history_wraps_and_stays_bounded() {
        let e = epoch();
        let mut h = SendHistory::new(e);
        for i in 0..(SEND_HISTORY + 100) {
            h.record(1200, e + Duration::from_micros(i as u64), 0, 0);
        }
        assert_eq!(h.ring.len(), SEND_HISTORY);
    }

    /// A clean run: every packet received, inter-arrival exactly matching inter-departure, so the
    /// gradient is flat and the detector must stay Normal.
    #[test]
    fn a_clean_path_yields_no_gradient_and_no_loss() {
        let e = epoch();
        let mut h = SendHistory::new(e);
        for i in 0..10 {
            h.record(1200, e + Duration::from_micros(i * 1000), 0, 0);
        }
        let fb = TransportLayerCc {
            sender_ssrc: 1,
            media_ssrc: 2,
            base_sequence_number: 0,
            packet_status_count: 10,
            reference_time: 100,
            fb_pkt_count: 0,
            packet_chunks: vec![PacketStatusChunk::RunLengthChunk(RunLengthChunk {
                type_tcc: StatusChunkTypeTcc::RunLengthChunk,
                packet_status_symbol: SymbolTypeTcc::PacketReceivedSmallDelta,
                run_length: 10,
            })],
            recv_deltas: (0..10).map(|_| recv(1000)).collect(),
        };
        let mut deltas = Vec::new();
        let s = interpret(&fb, &mut h, &mut deltas);
        assert_eq!(s.reported, 10);
        assert_eq!(s.lost, 0);
        assert_eq!(s.matched, 10);
        assert!(
            deltas.iter().all(|d| d.abs() < 1e-6),
            "matched pacing must produce a flat gradient, got {deltas:?}"
        );
        let mut det = DelayDetector::new();
        for d in &deltas {
            det.observe(*d);
        }
        assert_eq!(det.state(), NetworkState::Normal);
    }

    #[test]
    fn a_filling_queue_is_detected_before_any_loss() {
        let e = epoch();
        let mut h = SendHistory::new(e);
        for i in 0..20 {
            h.record(1200, e + Duration::from_micros(i * 1000), 0, 0);
        }
        // Sent 1 ms apart, arriving 4 ms apart: 3 ms of queue growth per packet, no loss.
        let fb = TransportLayerCc {
            sender_ssrc: 1,
            media_ssrc: 2,
            base_sequence_number: 0,
            packet_status_count: 20,
            reference_time: 100,
            fb_pkt_count: 0,
            packet_chunks: vec![PacketStatusChunk::RunLengthChunk(RunLengthChunk {
                type_tcc: StatusChunkTypeTcc::RunLengthChunk,
                packet_status_symbol: SymbolTypeTcc::PacketReceivedSmallDelta,
                run_length: 20,
            })],
            recv_deltas: (0..20).map(|_| recv(4000)).collect(),
        };
        let mut deltas = Vec::new();
        let s = interpret(&fb, &mut h, &mut deltas);
        assert_eq!(s.lost, 0, "the point is that this fires with zero loss");
        let mut det = DelayDetector::new();
        for d in &deltas {
            det.observe(*d);
        }
        assert_eq!(
            det.state(),
            NetworkState::Overuse,
            "trend {:.2} threshold {:.2}",
            det.trend_ms(),
            det.threshold_ms()
        );
    }

    #[test]
    fn a_status_vector_reports_loss_per_packet() {
        let e = epoch();
        let mut h = SendHistory::new(e);
        for i in 0..8 {
            h.record(1200, e + Duration::from_micros(i * 1000), 0, 0);
        }
        let mut symbols = vec![SymbolTypeTcc::PacketReceivedSmallDelta; 8];
        symbols[2] = SymbolTypeTcc::PacketNotReceived;
        symbols[5] = SymbolTypeTcc::PacketNotReceived;
        let fb = TransportLayerCc {
            sender_ssrc: 1,
            media_ssrc: 2,
            base_sequence_number: 0,
            packet_status_count: 8,
            reference_time: 10,
            fb_pkt_count: 0,
            packet_chunks: vec![PacketStatusChunk::StatusVectorChunk(StatusVectorChunk {
                type_tcc: StatusChunkTypeTcc::StatusVectorChunk,
                symbol_size: SymbolSizeTypeTcc::TwoBit,
                symbol_list: symbols,
            })],
            recv_deltas: (0..6).map(|_| recv(1000)).collect(),
        };
        let mut deltas = Vec::new();
        let s = interpret(&fb, &mut h, &mut deltas);
        assert_eq!(s.reported, 8);
        assert_eq!(s.lost, 2);
        assert!((s.loss_fraction() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn feedback_for_packets_we_never_sent_is_ignored_not_counted_as_delay() {
        let e = epoch();
        let mut h = SendHistory::new(e);
        h.record(1200, e, 0, 0);
        let fb = TransportLayerCc {
            sender_ssrc: 1,
            media_ssrc: 2,
            base_sequence_number: 5_000,
            packet_status_count: 4,
            reference_time: 1,
            fb_pkt_count: 0,
            packet_chunks: vec![PacketStatusChunk::RunLengthChunk(RunLengthChunk {
                type_tcc: StatusChunkTypeTcc::RunLengthChunk,
                packet_status_symbol: SymbolTypeTcc::PacketReceivedSmallDelta,
                run_length: 4,
            })],
            recv_deltas: (0..4).map(|_| recv(1000)).collect(),
        };
        let mut deltas = Vec::new();
        let s = interpret(&fb, &mut h, &mut deltas);
        assert_eq!(s.matched, 0);
        assert!(deltas.is_empty());
    }

    #[test]
    fn overuse_cuts_against_the_delivered_rate_not_the_wishful_target() {
        let now = Instant::now();
        let mut e = DelayBasedEstimate::new(20_000_000, 1_000_000, 40_000_000);
        // We wanted 20 Mbps; only 8 Mbps is arriving. The cut must land near 0.85 * 8 Mbps,
        // floored so one interval cannot collapse the session.
        let out = e.update(NetworkState::Overuse, 8_000_000, now);
        assert_eq!(out, 14_000_000, "a single cut is floored at 0.7x");
        let out = e.update(
            NetworkState::Overuse,
            8_000_000,
            now + Duration::from_millis(300),
        );
        assert_eq!(out, 9_800_000);
    }

    #[test]
    fn repeated_overuse_inside_one_window_only_cuts_once() {
        let now = Instant::now();
        let mut e = DelayBasedEstimate::new(20_000_000, 1_000_000, 40_000_000);
        let first = e.update(NetworkState::Overuse, 10_000_000, now);
        let second = e.update(
            NetworkState::Overuse,
            10_000_000,
            now + Duration::from_millis(50),
        );
        assert_eq!(first, second);
    }

    #[test]
    fn a_normal_path_climbs_back_to_the_ceiling() {
        let now = Instant::now();
        let mut e = DelayBasedEstimate::new(2_000_000, 1_000_000, 20_000_000);
        for i in 0..100 {
            e.update(
                NetworkState::Normal,
                2_000_000,
                now + Duration::from_millis(i * 120),
            );
        }
        assert_eq!(e.current(), 20_000_000);
    }

    #[test]
    fn underuse_holds_rather_than_probing() {
        let now = Instant::now();
        let mut e = DelayBasedEstimate::new(5_000_000, 1_000_000, 20_000_000);
        assert_eq!(e.update(NetworkState::Underuse, 5_000_000, now), 5_000_000);
    }

    #[test]
    fn shared_feedback_drains_loss_per_interval() {
        let f = SharedFeedback::default();
        assert_eq!(f.take_loss(), None, "no feedback yet is not zero loss");
        f.record_report(&FeedbackSummary {
            matched: 8,
            reported: 10,
            lost: 2,
            received_bps: 5_000_000,
            ..Default::default()
        });
        let (loss, n) = f.take_loss().unwrap();
        assert!((loss - 0.2).abs() < 1e-9);
        assert_eq!(n, 10);
        assert_eq!(f.take_loss(), None, "the interval is drained, not summed");
        assert_eq!(f.received_bps(), 5_000_000);
        assert_eq!(f.signal_name(), "twcc-delay-gradient");
    }

    #[test]
    fn shared_feedback_reports_the_signal_it_is_actually_flying_on() {
        let f = SharedFeedback::default();
        assert_eq!(f.signal_name(), "loss-only");
        f.record_remb(7_000_000);
        assert_eq!(f.signal_name(), "remb");
        assert_eq!(f.remb_bps(), Some(7_000_000));
    }

    #[test]
    fn remb_tracker_ignores_nonsense_and_counts_reports() {
        let mut r = RembTracker::default();
        r.observe(f32::NAN);
        r.observe(-1.0);
        assert_eq!(r.reports(), 0);
        assert_eq!(r.last_bps(), None);
        r.observe(6_500_000.0);
        assert_eq!(r.last_bps(), Some(6_500_000));
        assert_eq!(r.reports(), 1);
    }

    /// The acknowledgement signal, exercised through `SendHistory` directly: `interpret` needs a
    /// real `TransportLayerCc`, and what matters here is the tally, not the wire format.
    fn take_intact(h: &mut SendHistory) -> Option<(u32, u64)> {
        let mut lost = Vec::new();
        h.take_decided_frames(&mut lost)
    }

    #[test]
    fn a_frame_is_acknowledged_only_when_every_one_of_its_packets_arrived() {
        let e = Instant::now();
        let mut h = SendHistory::new(e);
        // Frame 1: three packets, all received. Frame 2: three packets, one lost.
        for _ in 0..3 {
            h.record(1200, e, 1, 1_000);
        }
        for _ in 0..3 {
            h.record(1200, e, 2, 2_000);
        }
        assert_eq!(take_intact(&mut h), None, "nothing decided yet");

        h.note(1, true);
        h.note(1, true);
        assert_eq!(take_intact(&mut h), None, "two of three is not the frame");
        h.note(1, true);
        assert_eq!(take_intact(&mut h), Some((1, 1_000)));

        h.note(2, true);
        h.note(2, false);
        h.note(2, true);
        assert_eq!(
            take_intact(&mut h),
            None,
            "a frame missing a packet must never be acknowledged"
        );
    }

    #[test]
    fn only_the_newest_intact_frame_is_reported_and_decided_frames_do_not_pile_up() {
        let e = Instant::now();
        let mut h = SendHistory::new(e);
        for frame in 1..=3u32 {
            h.record(1200, e, frame, frame as u64 * 1_000);
        }
        for frame in 1..=3u32 {
            h.note(frame, true);
        }
        assert_eq!(take_intact(&mut h), Some((3, 3_000)));
        assert_eq!(take_intact(&mut h), None, "the tallies were consumed");
    }

    #[test]
    fn the_published_acknowledgement_only_moves_forward() {
        let f = SharedFeedback::default();
        assert_eq!(f.acked_frame_capture_ns(), None);
        f.record_report(&FeedbackSummary {
            acked_capture_ns: Some(5_000),
            ..Default::default()
        });
        assert_eq!(f.acked_frame_capture_ns(), Some(5_000));
        // Reports can be reordered on the wire; an older one must not walk it backwards.
        f.record_report(&FeedbackSummary {
            acked_capture_ns: Some(1_000),
            ..Default::default()
        });
        assert_eq!(f.acked_frame_capture_ns(), Some(5_000));
        f.record_report(&FeedbackSummary {
            acked_capture_ns: Some(9_000),
            ..Default::default()
        });
        assert_eq!(f.acked_frame_capture_ns(), Some(9_000));
    }

    /// The indexed lookup has to agree with the scan it replaced on every entry of a full ring,
    /// across the `u16` wrap, and after a retire has moved the front — which is the only state in
    /// which index arithmetic can be wrong.
    #[test]
    fn the_indexed_lookup_finds_what_a_scan_would() {
        let e = Instant::now();
        let mut h = SendHistory::new(e);
        // Past the wrap and past the ring's capacity, so `next_seq` has wrapped and the front has
        // moved off zero.
        for i in 0..(SEND_HISTORY + 5000) {
            h.record(1200, e, (i / 10) as u32, i as u64);
        }
        assert_eq!(h.ring.len(), SEND_HISTORY);

        let scan = |seq: u16| h.ring.iter().rev().find(|p| p.seq == seq).copied();
        for p in h.ring.iter() {
            assert_eq!(h.lookup(p.seq), scan(p.seq), "seq {}", p.seq);
            assert_eq!(h.lookup(p.seq).map(|q| q.seq), Some(p.seq));
        }
        // A sequence the ring no longer holds must answer nothing, not the slot its index names.
        let front = *h.ring.front().unwrap();
        let gone = front.seq.wrapping_sub(1);
        assert_eq!(
            h.lookup(gone),
            None,
            "retired sequences are not in the ring"
        );
        let unsent = h.ring.back().unwrap().seq.wrapping_add(1);
        assert_eq!(h.lookup(unsent), None, "unsent sequences are not either");

        // And after a retire, when the front is no longer where it was.
        h.retire_through(front.seq.wrapping_add(100));
        for p in h.ring.iter() {
            assert_eq!(h.lookup(p.seq), scan(p.seq), "after retire, seq {}", p.seq);
        }
    }

    #[test]
    fn a_frame_the_client_did_not_receive_whole_is_reported_as_lost() {
        let e = Instant::now();
        let mut h = SendHistory::new(e);
        for _ in 0..2 {
            h.record(1200, e, 7, 7_000);
        }
        h.note(7, true);
        h.note(7, false);
        let mut lost = Vec::new();
        assert_eq!(h.take_decided_frames(&mut lost), None);
        assert_eq!(lost, vec![7_000], "the encoder must stop referencing it");
    }
}

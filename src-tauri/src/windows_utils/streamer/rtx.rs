//! Retransmission with a byte budget, for the per-viewer RTP writer.
//!
//! The stock webrtc-rs NACK responder resends every requested packet at once, however old and
//! however often it is asked. On a link that cannot carry the stream that is a storm: resends
//! are dropped at the bottleneck, requested again and resent again, and measured at a 1.5 Mbit/s
//! cap the host pushed 20 to 48 Mbit/s of them for ten seconds while the picture starved.
//!
//! Here the writer resends instead, through its own pacer, and only what passes three checks:
//! the packet is younger than a few round trips, it was not resent within the last one, and the
//! byte budget (half the recent media rate, libwebrtc's retransmission rate limiter in spirit)
//! has room for it. The stock responder keeps every non-video stream exactly as before.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use webrtc::api::interceptor_registry::{configure_rtcp_reports, configure_twcc_receiver_only};
use webrtc::api::media_engine::MediaEngine;
use webrtc::interceptor::nack::generator::Generator;
use webrtc::interceptor::nack::responder::Responder;
use webrtc::interceptor::registry::Registry;
use webrtc::interceptor::stream_info::StreamInfo;
use webrtc::interceptor::{
    Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader, RTPWriter,
};
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::NackPair;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::RTCPFeedback;

use crate::streamer::telemetry::ClientStats;

/// Packets kept for resending, a power of two. The stock responder's size: 1.5 s at 50 Mbit/s,
/// longer than the oldest packet [`MAX_AGE`] would ever let out.
const SLOTS: usize = 8192;

/// Resends may use this share of the recent media rate. libwebrtc allows the whole target; half
/// is enough to repair 5 % random loss twenty times over and caps a capped link's overshoot at
/// 1.5x the media instead of the tens of times measured.
const RTX_SHARE: f64 = 0.5;

/// The budget's burst: this much time at the resend rate may go out at once. Holds the repair of
/// a 100 ms outage at 10 Mbit/s, and at 1 Mbit/s is about a dozen packets, under the 100 ms queue
/// of the capped relay.
const BUDGET_WINDOW: Duration = Duration::from_millis(250);

/// Resend rate floor: a still screen sends almost nothing, and its occasional loss must still be
/// repaired.
const MIN_RTX_BPS: f64 = 300_000.0;

/// Burst floor: a dozen full packets, so one isolated loss is always repaired on first request.
const MIN_BUDGET_BYTES: f64 = 12.0 * 1200.0;

/// The media rate is measured over this many buckets of [`RATE_BUCKET`].
const RATE_BUCKETS: usize = 10;
const RATE_BUCKET: Duration = Duration::from_millis(100);

/// A packet older than this many round trips is not resent: the client has asked for a keyframe
/// or moved past it by then, and it is bandwidth the current picture needs.
const AGE_RTTS: u32 = 4;
/// At least this long whatever the round trip: Chrome re-asks about every 100 ms until it knows
/// the RTT, so a resend lost twice in a row still gets a third try.
const MIN_AGE: Duration = Duration::from_millis(400);
/// Never older than a second, the age libwebrtc's packet history keeps at least.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(1);

/// The same packet is not resent twice inside this much of one round trip. Bounded below by
/// timer and scheduling jitter, above so a stale RTT cannot hold a repair for long.
const MIN_RESEND_GAP: Duration = Duration::from_millis(10);
const MAX_RESEND_GAP: Duration = Duration::from_millis(250);

/// The round trip assumed before the client has reported one: half Chrome's own default, so a
/// genuine re-ask is never refused for being too soon.
const RTT_UNKNOWN: Duration = Duration::from_millis(50);

/// A client RTT older than this is not trusted.
const CLIENT_RTT_FRESH_NS: u64 = 3_000_000_000;

/// NACK batches queued between the RTCP reader and the writer. A full queue drops the batch,
/// which the client asks for again.
pub(crate) const NACK_QUEUE: usize = 256;

/// Why a requested packet was not resent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Never sent, or already overwritten.
    Missing,
    /// Older than the age limit.
    Stale,
    /// Resent less than a round trip ago.
    Recent,
    /// The byte budget had no room.
    Budget,
}

struct Slot<T> {
    seq: u16,
    sent: Instant,
    last_resent: Option<Instant>,
    bytes: u32,
    item: T,
}

/// Media bytes over the last second, in fixed buckets.
struct SendRate {
    epoch: Option<Instant>,
    head: u64,
    buckets: [u64; RATE_BUCKETS],
}

impl SendRate {
    fn new() -> Self {
        Self {
            epoch: None,
            head: 0,
            buckets: [0; RATE_BUCKETS],
        }
    }

    fn bucket_of(&mut self, now: Instant) -> u64 {
        let epoch = *self.epoch.get_or_insert(now);
        let b = (now.saturating_duration_since(epoch).as_nanos() / RATE_BUCKET.as_nanos()) as u64;
        if b > self.head {
            let clear = (b - self.head).min(RATE_BUCKETS as u64);
            for i in 0..clear {
                self.buckets[((b - i) % RATE_BUCKETS as u64) as usize] = 0;
            }
            self.head = b;
        }
        self.head
    }

    fn add(&mut self, bytes: u64, now: Instant) {
        let b = self.bucket_of(now);
        self.buckets[(b % RATE_BUCKETS as u64) as usize] += bytes;
    }

    fn bps(&mut self, now: Instant) -> f64 {
        let Some(epoch) = self.epoch else {
            return 0.0;
        };
        self.bucket_of(now);
        let span = now
            .saturating_duration_since(epoch)
            .min(RATE_BUCKET * RATE_BUCKETS as u32)
            .max(RATE_BUCKET);
        self.buckets.iter().sum::<u64>() as f64 * 8.0 / span.as_secs_f64()
    }
}

/// A token bucket in bytes whose rate and depth follow the media.
struct ByteBudget {
    tokens: f64,
    last: Option<Instant>,
}

impl ByteBudget {
    fn spend(&mut self, bytes: f64, rate_bytes: f64, depth: f64, now: Instant) -> bool {
        self.tokens = match self.last {
            Some(last) => (self.tokens
                + rate_bytes * now.saturating_duration_since(last).as_secs_f64())
            .min(depth),
            // a session starts able to repair
            None => depth,
        };
        self.last = Some(now);
        if self.tokens < bytes {
            return false;
        }
        self.tokens -= bytes;
        true
    }
}

/// What was sent, and the policy for sending it again. Owned by one writer task; `T` is what the
/// writer needs to rebuild the packet.
pub(crate) struct Retransmitter<T> {
    slots: Vec<Option<Slot<T>>>,
    rate: SendRate,
    budget: ByteBudget,
}

impl<T> Retransmitter<T> {
    pub(crate) fn new() -> Self {
        let mut slots = Vec::with_capacity(SLOTS);
        slots.resize_with(SLOTS, || None);
        Self {
            slots,
            rate: SendRate::new(),
            budget: ByteBudget {
                tokens: 0.0,
                last: None,
            },
        }
    }

    /// A media packet just went out.
    pub(crate) fn sent(&mut self, seq: u16, bytes: usize, now: Instant, item: T) {
        self.rate.add(bytes as u64, now);
        self.slots[seq as usize & (SLOTS - 1)] = Some(Slot {
            seq,
            sent: now,
            last_resent: None,
            bytes: bytes as u32,
            item,
        });
    }

    /// The client asked for `seq` again. On success the packet is charged to the budget and
    /// marked resent, and the caller must send it.
    ///
    /// `target_bps` is the rate the bitrate controller last set (0 when unknown): on a link the
    /// encoder cannot follow down, it is a better measure of what the path carries than what the
    /// encoder is actually putting out.
    pub(crate) fn request(
        &mut self,
        seq: u16,
        now: Instant,
        rtt: Duration,
        target_bps: u32,
    ) -> Result<(&T, u32), Refusal> {
        let media_bps = self.rate.bps(now);
        let slot = self.slots[seq as usize & (SLOTS - 1)]
            .as_mut()
            .filter(|s| s.seq == seq)
            .ok_or(Refusal::Missing)?;
        if now.saturating_duration_since(slot.sent) > age_limit(rtt) {
            return Err(Refusal::Stale);
        }
        if let Some(last) = slot.last_resent {
            if now.saturating_duration_since(last) < resend_gap(rtt) {
                return Err(Refusal::Recent);
            }
        }
        let basis = if target_bps > 0 {
            media_bps.min(target_bps as f64)
        } else {
            media_bps
        };
        let rate_bytes = (basis * RTX_SHARE).max(MIN_RTX_BPS) / 8.0;
        let depth = (rate_bytes * BUDGET_WINDOW.as_secs_f64()).max(MIN_BUDGET_BYTES);
        if !self.budget.spend(slot.bytes as f64, rate_bytes, depth, now) {
            return Err(Refusal::Budget);
        }
        slot.last_resent = Some(now);
        Ok((&slot.item, slot.bytes))
    }
}

impl<T> Retransmitter<T> {
    /// What the stock responder does with a request, for while the link side is not engaged:
    /// whatever is still in the history goes again, with no age limit, budget or spacing.
    pub(crate) fn request_unlimited(&mut self, seq: u16) -> Result<(&T, u32), Refusal> {
        let slot = self.slots[seq as usize & (SLOTS - 1)]
            .as_ref()
            .filter(|s| s.seq == seq)
            .ok_or(Refusal::Missing)?;
        Ok((&slot.item, slot.bytes))
    }
}

fn age_limit(rtt: Duration) -> Duration {
    (rtt * AGE_RTTS).clamp(MIN_AGE, MAX_AGE)
}

fn resend_gap(rtt: Duration) -> Duration {
    rtt.clamp(MIN_RESEND_GAP, MAX_RESEND_GAP)
}

/// The round trip the client last measured, or [`RTT_UNKNOWN`].
pub(crate) fn client_rtt(stats: Option<&ClientStats>, now_ns: u64) -> Duration {
    stats
        .filter(|s| s.rtt_us > 0 && now_ns.saturating_sub(s.received_host_ns) < CLIENT_RTT_FRESH_NS)
        .map_or(RTT_UNKNOWN, |s| Duration::from_micros(s.rtt_us as u64))
}

/// Every sequence number one NACK names.
pub(crate) fn nacked(pairs: &[NackPair]) -> impl Iterator<Item = u16> + '_ {
    pairs.iter().flat_map(|p| {
        std::iter::once(p.packet_id).chain(
            (0..16u16)
                .filter(move |i| p.lost_packets & (1 << i) != 0)
                .map(move |i| p.packet_id.wrapping_add(i + 1)),
        )
    })
}

/// What `status --json` shows under `retransmission`.
#[derive(Default)]
pub struct RtxCounters {
    pub active: AtomicBool,
    pub resent_packets: AtomicU64,
    pub resent_bytes: AtomicU64,
    pub refused_budget: AtomicU64,
    pub refused_age: AtomicU64,
    pub refused_recent: AtomicU64,
    pub refused_missing: AtomicU64,
    pub nack_batches_dropped: AtomicU64,
    /// Feedback packets the RTCP reader could not parse and read past.
    pub rtcp_unparsed: AtomicU64,
    /// The RTCP reader ended on an error that is not a normal close.
    pub rtcp_reader_failed: AtomicBool,
}

/// What the RTCP reader does with a read error.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RtcpReadError {
    /// One packet the rtcp crate rejected: the read already consumed it, so reading on cannot
    /// spin, and the session's feedback and resends go on.
    Skip,
    /// The sender or its session is closing.
    Closed,
    /// Anything else: not proven to consume its packet, so the reader ends, and says why.
    Fatal,
}

pub(crate) fn rtcp_read_error(e: &webrtc::Error) -> RtcpReadError {
    use webrtc::interceptor::Error as I;
    match e {
        webrtc::Error::Interceptor(I::Rtcp(_)) => RtcpReadError::Skip,
        webrtc::Error::ErrClosedPipe | webrtc::Error::Interceptor(I::ErrIoEOF) => {
            RtcpReadError::Closed
        }
        // a closed SRTP session or buffer arrives wrapped as text through the interceptor
        _ => {
            let text = e.to_string().to_ascii_lowercase();
            if text.contains("closed") || text.contains("eof") {
                RtcpReadError::Closed
            } else {
                RtcpReadError::Fatal
            }
        }
    }
}

impl RtxCounters {
    pub(crate) fn count(&self, verdict: Result<u32, Refusal>) {
        let c = match verdict {
            Ok(bytes) => {
                self.resent_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
                &self.resent_packets
            }
            Err(Refusal::Budget) => &self.refused_budget,
            Err(Refusal::Stale) => &self.refused_age,
            Err(Refusal::Recent) => &self.refused_recent,
            Err(Refusal::Missing) => &self.refused_missing,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn to_json(&self) -> Option<serde_json::Value> {
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let failed = self.rtcp_reader_failed.load(Ordering::Relaxed);
        if !self.active.load(Ordering::Relaxed) && r(&self.rtcp_unparsed) == 0 && !failed {
            return None;
        }
        Some(serde_json::json!({
            "resent_packets": r(&self.resent_packets),
            "resent_bytes": r(&self.resent_bytes),
            "refused_budget": r(&self.refused_budget),
            "refused_age": r(&self.refused_age),
            "refused_recent": r(&self.refused_recent),
            "refused_missing": r(&self.refused_missing),
            "nack_batches_dropped": r(&self.nack_batches_dropped),
            "rtcp_packets_unparsed": r(&self.rtcp_unparsed),
            "rtcp_reader_failed": failed,
        }))
    }
}

/// `register_default_interceptors` without video retransmission: the same feedback, generator,
/// reports and receive-side TWCC in the same order, and the stock responder for every stream but
/// video, which the writer answers itself.
pub(crate) fn register_interceptors(
    mut registry: Registry,
    media_engine: &mut MediaEngine,
) -> Result<Registry> {
    for parameter in ["", "pli"] {
        media_engine.register_feedback(
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: parameter.to_owned(),
            },
            RTPCodecType::Video,
        );
    }
    registry.add(Box::new(NonVideoResponderBuilder));
    registry.add(Box::new(Generator::builder()));
    registry = configure_rtcp_reports(registry);
    configure_twcc_receiver_only(registry, media_engine).context("register twcc receiver")
}

/// Whether the stock responder keeps a stream: everything except video.
fn responder_keeps(info: &StreamInfo) -> bool {
    !info.mime_type.to_ascii_lowercase().starts_with("video/")
}

struct NonVideoResponderBuilder;

impl InterceptorBuilder for NonVideoResponderBuilder {
    fn build(
        &self,
        id: &str,
    ) -> std::result::Result<Arc<dyn Interceptor + Send + Sync>, webrtc::interceptor::Error> {
        Ok(Arc::new(NonVideoResponder {
            inner: Responder::builder().build(id)?,
        }))
    }
}

/// The stock responder, never bound to a video stream. A NACK for an SSRC it holds no stream
/// for is ignored by it, so video NACKs reach the writer untouched.
struct NonVideoResponder {
    inner: Arc<dyn Interceptor + Send + Sync>,
}

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// written out by hand: the trait is `#[async_trait]` and this crate does not depend on it
impl Interceptor for NonVideoResponder {
    fn bind_rtcp_reader<'life0, 'async_trait>(
        &'life0 self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Boxed<'async_trait, Arc<dyn RTCPReader + Send + Sync>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.bind_rtcp_reader(reader)
    }

    fn bind_rtcp_writer<'life0, 'async_trait>(
        &'life0 self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Boxed<'async_trait, Arc<dyn RTCPWriter + Send + Sync>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.bind_rtcp_writer(writer)
    }

    fn bind_local_stream<'life0, 'life1, 'async_trait>(
        &'life0 self,
        info: &'life1 StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Boxed<'async_trait, Arc<dyn RTPWriter + Send + Sync>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        if responder_keeps(info) {
            self.inner.bind_local_stream(info, writer)
        } else {
            Box::pin(async move { writer })
        }
    }

    fn unbind_local_stream<'life0, 'life1, 'async_trait>(
        &'life0 self,
        info: &'life1 StreamInfo,
    ) -> Boxed<'async_trait, ()>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.unbind_local_stream(info)
    }

    fn bind_remote_stream<'life0, 'life1, 'async_trait>(
        &'life0 self,
        info: &'life1 StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Boxed<'async_trait, Arc<dyn RTPReader + Send + Sync>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.bind_remote_stream(info, reader)
    }

    fn unbind_remote_stream<'life0, 'life1, 'async_trait>(
        &'life0 self,
        info: &'life1 StreamInfo,
    ) -> Boxed<'async_trait, ()>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.unbind_remote_stream(info)
    }

    fn close<'life0, 'async_trait>(
        &'life0 self,
    ) -> Boxed<'async_trait, std::result::Result<(), webrtc::interceptor::Error>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.close()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use super::*;

    const PKT: usize = 1200;

    /// One feedback packet the rtcp crate rejects is read past; a closed sender ends the reader
    /// quietly; anything not known to consume its packet ends it with a line in the log.
    #[test]
    fn an_unparsable_feedback_packet_is_skipped_and_a_closed_sender_ends_the_reader() {
        use webrtc::interceptor::Error as I;
        for e in [
            webrtc::rtcp::Error::PacketTooShort,
            webrtc::rtcp::Error::PacketStatusChunkLength,
            webrtc::rtcp::Error::DeltaExceedLimit,
        ] {
            let e = webrtc::Error::Interceptor(I::Rtcp(e));
            assert_eq!(rtcp_read_error(&e), RtcpReadError::Skip, "{e}");
        }
        assert_eq!(
            rtcp_read_error(&webrtc::Error::ErrClosedPipe),
            RtcpReadError::Closed
        );
        assert_eq!(
            rtcp_read_error(&webrtc::Error::Interceptor(I::ErrIoEOF)),
            RtcpReadError::Closed
        );
        assert_eq!(
            rtcp_read_error(&webrtc::Error::Interceptor(I::Util(
                webrtc::util::Error::ErrBufferClosed
            ))),
            RtcpReadError::Closed
        );
        assert_eq!(
            rtcp_read_error(&webrtc::Error::ErrInterceptorNotBind),
            RtcpReadError::Fatal
        );
        assert_eq!(
            rtcp_read_error(&webrtc::Error::Interceptor(I::Util(
                webrtc::util::Error::ErrBufferShort
            ))),
            RtcpReadError::Fatal
        );
    }
    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn isolated_loss_on_a_healthy_link_is_repaired_on_the_first_request() {
        let mut r = Retransmitter::new();
        let t0 = Instant::now();
        // 10 Mbit/s for a second: ~1040 packets
        for seq in 0..1040u16 {
            r.sent(seq, PKT, t0 + Duration::from_micros(seq as u64 * 960), ());
        }
        let now = t0 + Duration::from_millis(1000);
        assert_eq!(
            r.request(1035, now, Duration::from_millis(5), 0)
                .map(|v| v.1),
            Ok(PKT as u32)
        );
    }

    #[test]
    fn a_session_with_no_rate_yet_still_repairs_its_first_loss() {
        let mut r = Retransmitter::new();
        let t0 = Instant::now();
        r.sent(7, PKT, t0, ());
        assert!(r.request(7, t0 + 3 * MS, RTT_UNKNOWN, 0).is_ok());
    }

    #[test]
    fn a_packet_older_than_the_age_limit_is_refused() {
        let mut r = Retransmitter::new();
        let t0 = Instant::now();
        r.sent(1, PKT, t0, ());
        r.sent(2, PKT, t0, ());
        let late = t0 + Duration::from_millis(600);
        // a 20 ms path: the limit is the 400 ms floor
        assert_eq!(
            r.request(1, late, Duration::from_millis(20), 0).err(),
            Some(Refusal::Stale)
        );
        // a 200 ms path allows four round trips, 800 ms
        assert!(r.request(2, late, Duration::from_millis(200), 0).is_ok());
        // and nothing ever past a second
        r.sent(3, PKT, t0, ());
        assert_eq!(
            r.request(
                3,
                t0 + Duration::from_millis(1100),
                Duration::from_secs(2),
                0
            )
            .err(),
            Some(Refusal::Stale)
        );
    }

    #[test]
    fn a_duplicate_nack_inside_one_round_trip_is_ignored() {
        let mut r = Retransmitter::new();
        let t0 = Instant::now();
        let rtt = Duration::from_millis(30);
        r.sent(40, PKT, t0, ());
        assert!(r.request(40, t0 + 5 * MS, rtt, 0).is_ok());
        assert_eq!(
            r.request(40, t0 + 20 * MS, rtt, 0).err(),
            Some(Refusal::Recent)
        );
        // a round trip later the resend itself may have been lost: send it again
        assert!(r.request(40, t0 + 36 * MS, rtt, 0).is_ok());
    }

    #[test]
    fn an_unknown_or_overwritten_packet_is_missing() {
        let mut r = Retransmitter::new();
        let t0 = Instant::now();
        assert_eq!(
            r.request(5, t0, RTT_UNKNOWN, 0).err(),
            Some(Refusal::Missing)
        );
        r.sent(5, PKT, t0, ());
        r.sent(5u16.wrapping_add(SLOTS as u16), PKT, t0, ());
        assert_eq!(
            r.request(5, t0, RTT_UNKNOWN, 0).err(),
            Some(Refusal::Missing)
        );
    }

    /// Before the link side engages the writer answers as the stock responder does: every
    /// packet still among the last `SLOTS` goes again, however old, however often, however many,
    /// and only one already overwritten is missing.
    #[test]
    fn an_unengaged_link_side_resends_what_the_stock_responder_would() {
        let t0 = Instant::now();
        let mut r = Retransmitter::new();
        for seq in 0..(SLOTS as u32 + 100) {
            r.sent(seq as u16, PKT, t0 + MS * seq, seq);
        }
        let late = t0 + Duration::from_secs(30);
        for _ in 0..3 {
            for seq in 100..(SLOTS as u32 + 100) {
                let got = r.request_unlimited(seq as u16).map(|(v, b)| (*v, b));
                assert_eq!(got, Ok((seq, PKT as u32)), "seq {seq}");
            }
        }
        assert_eq!(r.request_unlimited(50).err(), Some(Refusal::Missing));
        // the budgeted path refuses the same request on age
        assert_eq!(
            r.request(200, late, Duration::from_millis(20), 0).err(),
            Some(Refusal::Stale)
        );
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut r = Retransmitter::new();
        let t0 = Instant::now();
        for (i, seq) in (65530u16..=65535).chain(0..6).enumerate() {
            r.sent(seq, PKT, t0 + MS * i as u32, ());
        }
        let now = t0 + 20 * MS;
        assert!(r.request(65535, now, RTT_UNKNOWN, 0).is_ok());
        assert!(r.request(2, now, RTT_UNKNOWN, 0).is_ok());
    }

    #[test]
    fn nack_pairs_expand_to_every_named_sequence() {
        let pairs = [
            NackPair {
                packet_id: 65534,
                lost_packets: 0b101,
            },
            NackPair {
                packet_id: 10,
                lost_packets: 0,
            },
        ];
        let seqs: Vec<u16> = nacked(&pairs).collect();
        assert_eq!(seqs, vec![65534, 65535, 1, 10]);
    }

    #[test]
    fn the_stock_responder_keeps_audio_and_leaves_video_to_the_writer() {
        let info = |mime: &str| StreamInfo {
            mime_type: mime.to_owned(),
            ..Default::default()
        };
        assert!(responder_keeps(&info("audio/opus")));
        assert!(!responder_keeps(&info("video/H264")));
        assert!(!responder_keeps(&info("video/rtx")));
    }

    #[test]
    fn a_stale_client_rtt_falls_back_to_the_default() {
        let stats = ClientStats {
            rtt_us: 120_000,
            received_host_ns: 1_000_000_000,
            ..Default::default()
        };
        assert_eq!(
            client_rtt(Some(&stats), 2_000_000_000),
            Duration::from_millis(120)
        );
        assert_eq!(client_rtt(Some(&stats), 5_000_000_000), RTT_UNKNOWN);
        assert_eq!(client_rtt(None, 0), RTT_UNKNOWN);
    }

    struct Link {
        cap_bytes_per_ms: f64,
        queue_limit: usize,
        /// Forward loss, resends included.
        loss: f64,
        /// Loss of whole NACK batches on the way back.
        nack_loss: f64,
        outage: Option<(u64, u64)>,
    }

    /// The bottleneck: a token bucket in front of a drop-tail queue, plus random loss.
    struct Bottleneck {
        link: Link,
        rng: u64,
        queue: VecDeque<u16>,
        queued: usize,
        credit: f64,
    }

    impl Bottleneck {
        fn rand(&mut self) -> f64 {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            (self.rng >> 11) as f64 / (1u64 << 53) as f64
        }

        fn offer(&mut self, seq: u16, ms: u64) {
            let down = self.link.outage.is_some_and(|(a, b)| ms >= a && ms < b);
            let lost = self.rand() < self.link.loss;
            if down || lost || self.queued + PKT > self.link.queue_limit {
                return;
            }
            self.queue.push_back(seq);
            self.queued += PKT;
        }

        fn drain(&mut self, out: &mut Vec<u16>) {
            self.credit = (self.credit + self.link.cap_bytes_per_ms)
                .min(self.link.cap_bytes_per_ms.max(PKT as f64) * 2.0);
            while self.credit >= PKT as f64 {
                let Some(seq) = self.queue.pop_front() else {
                    break;
                };
                self.credit -= PKT as f64;
                self.queued -= PKT;
                out.push(seq);
            }
        }
    }

    struct Run {
        /// Bits per second entering the bottleneck, media and resends, per whole second.
        offered_bps: Vec<f64>,
        resent_bps: Vec<f64>,
        media_bps: Vec<f64>,
        /// Packets lost and never delivered before the client gave up on them.
        unrepaired: usize,
        /// The worst time from a lost packet's first send to its delivery.
        worst_repair: Duration,
    }

    /// A host, a bottleneck and a Chrome-like NACK generator, in 1 ms steps. `media_bps(ms)` is
    /// the encoder's output; `budgeted` false is the stock responder, which resends everything
    /// requested that is still cached.
    fn simulate(
        secs: u64,
        link: Link,
        media_bps: impl Fn(u64) -> f64,
        budgeted: bool,
        rtt: Duration,
    ) -> Run {
        const ONE_WAY_MS: u64 = 5;
        // Chrome's NackRequester: ask at once, then every 100 ms (its default RTT), ten times
        const RENACK_MS: u64 = 100;
        const RETRIES: u32 = 10;
        let t0 = Instant::now();
        let mut host: Retransmitter<()> = Retransmitter::new();
        let mut net = Bottleneck {
            link,
            rng: 0x9E37_79B9_7F4A_7C15,
            queue: VecDeque::new(),
            queued: 0,
            credit: 0.0,
        };
        let mut next_seq: u16 = 0;
        let mut media_acc = 0.0f64;
        let mut in_flight: VecDeque<(u64, u16)> = VecDeque::new();
        let mut nacks: VecDeque<(u64, Vec<u16>)> = VecDeque::new();
        let mut highest: Option<u16> = None;
        // seq -> (next ask at ms, asks so far)
        let mut missing: BTreeMap<u16, (u64, u32)> = BTreeMap::new();
        let mut first_sent: Vec<u64> = Vec::new();
        let mut run = Run {
            offered_bps: vec![0.0; secs as usize],
            resent_bps: vec![0.0; secs as usize],
            media_bps: vec![0.0; secs as usize],
            unrepaired: 0,
            worst_repair: Duration::ZERO,
        };
        let bits = (PKT * 8) as f64;
        let mut delivered = Vec::new();

        for ms in 0..secs * 1000 {
            let now = t0 + MS * ms as u32;
            let sec = (ms / 1000) as usize;
            // the encoder stops half a second early, so every loss has time to be repaired
            if ms + 500 < secs * 1000 {
                media_acc += media_bps(ms) / 8.0 / 1000.0;
            }
            while media_acc >= PKT as f64 {
                media_acc -= PKT as f64;
                let seq = next_seq;
                next_seq = next_seq.wrapping_add(1);
                host.sent(seq, PKT, now, ());
                first_sent.push(ms);
                run.media_bps[sec] += bits;
                run.offered_bps[sec] += bits;
                net.offer(seq, ms);
            }
            while nacks.front().is_some_and(|(at, _)| *at <= ms) {
                let (_, seqs) = nacks.pop_front().unwrap();
                for seq in seqs {
                    let resend = if budgeted {
                        host.request(seq, now, rtt, 0).is_ok()
                    } else {
                        host.slots[seq as usize & (SLOTS - 1)]
                            .as_ref()
                            .is_some_and(|s| s.seq == seq)
                    };
                    if resend {
                        run.resent_bps[sec] += bits;
                        run.offered_bps[sec] += bits;
                        net.offer(seq, ms);
                    }
                }
            }
            delivered.clear();
            net.drain(&mut delivered);
            for seq in &delivered {
                in_flight.push_back((ms + ONE_WAY_MS, *seq));
            }
            let mut ask = Vec::new();
            while in_flight.front().is_some_and(|(at, _)| *at <= ms) {
                let (_, seq) = in_flight.pop_front().unwrap();
                if missing.remove(&seq).is_some() {
                    let took = Duration::from_millis(ms - first_sent[seq as usize]);
                    run.worst_repair = run.worst_repair.max(took);
                    continue;
                }
                match highest {
                    Some(h) if seq.wrapping_sub(h) >= 0x8000 || seq == h => continue,
                    Some(h) => {
                        let mut s = h.wrapping_add(1);
                        while s != seq {
                            missing.insert(s, (ms + RENACK_MS, 1));
                            ask.push(s);
                            s = s.wrapping_add(1);
                        }
                    }
                    None => {}
                }
                highest = Some(seq);
            }
            for (seq, (next, asks)) in missing.iter_mut() {
                if *next <= ms && *asks < RETRIES {
                    *next = ms + RENACK_MS;
                    *asks += 1;
                    ask.push(*seq);
                }
            }
            let before = missing.len();
            missing.retain(|_, (next, asks)| *asks < RETRIES || *next > ms);
            run.unrepaired += before - missing.len();
            if !ask.is_empty() && net.rand() >= net.link.nack_loss {
                nacks.push_back((ms + ONE_WAY_MS, ask));
            }
        }
        run.unrepaired += missing.len();
        run
    }

    fn mbit(v: &[f64]) -> Vec<String> {
        v.iter().map(|b| format!("{:.2}", b / 1e6)).collect()
    }

    /// The measured 1.5 Mbit/s case: the encoder at 10 Mbit/s when the cap lands and down to
    /// 1.1 Mbit/s within three seconds, behind a 100 ms drop-tail queue.
    fn capped_media(ms: u64) -> f64 {
        let t = ms as f64 / 1000.0;
        if t < 3.0 {
            10e6 - (10e6 - 1.1e6) * t / 3.0
        } else {
            1.1e6
        }
    }

    fn capped_link() -> Link {
        Link {
            cap_bytes_per_ms: 1.5e6 / 8.0 / 1000.0,
            queue_limit: (1.5e6 / 8.0 * 0.1) as usize,
            loss: 0.0,
            nack_loss: 0.0,
            outage: None,
        }
    }

    #[test]
    fn the_budget_holds_under_a_sustained_nack_flood_on_a_capped_link() {
        // a full queue: the client measures about a 110 ms round trip
        let rtt = Duration::from_millis(110);
        let stock = simulate(12, capped_link(), capped_media, false, rtt);
        let ours = simulate(12, capped_link(), capped_media, true, rtt);
        eprintln!("offered Mbit/s, stock:  {:?}", mbit(&stock.offered_bps));
        eprintln!("offered Mbit/s, budget: {:?}", mbit(&ours.offered_bps));
        eprintln!("resent Mbit/s, budget:  {:?}", mbit(&ours.resent_bps));
        // the model reproduces the storm the stock responder makes
        let stock_peak = stock.offered_bps[3..].iter().cloned().fold(0.0, f64::max);
        assert!(
            stock_peak > 10e6,
            "the stock model should flood the link, peak {:.2}",
            stock_peak / 1e6
        );
        for s in 0..12 {
            // never more than half the media of the last second, plus one burst: the window
            // reaches back into the second before
            let media = ours.media_bps[s].max(ours.media_bps[s.saturating_sub(1)]);
            let allowed = (media * RTX_SHARE).max(MIN_RTX_BPS)
                + (media * RTX_SHARE * BUDGET_WINDOW.as_secs_f64()).max(MIN_BUDGET_BYTES * 8.0);
            assert!(
                ours.resent_bps[s] <= allowed,
                "second {s}: resent {:.2} over {:.2}",
                ours.resent_bps[s] / 1e6,
                allowed / 1e6
            );
        }
        // once the encoder fits, what enters the link stays near the cap
        for (s, offered) in ours.offered_bps.iter().enumerate().take(12).skip(4) {
            assert!(*offered < 1.8e6, "second {s}: offered {:.2}", offered / 1e6);
        }
    }

    fn healthy(loss: f64, nack_loss: f64, outage: Option<(u64, u64)>) -> Link {
        Link {
            // a 100 Mbit/s link carrying 10 Mbit/s
            cap_bytes_per_ms: 100e6 / 8.0 / 1000.0,
            queue_limit: 1 << 20,
            loss,
            nack_loss,
            outage,
        }
    }

    #[test]
    fn random_loss_on_an_uncapped_link_is_fully_repaired() {
        let rtt = Duration::from_millis(12);
        for loss in [0.01, 0.05] {
            let run = simulate(6, healthy(loss, 0.0, None), |_| 10e6, true, rtt);
            assert_eq!(run.unrepaired, 0, "loss {loss}: packets never repaired");
            // a resend lost again waits one re-ask (100 ms); allow three in a row
            assert!(
                run.worst_repair <= Duration::from_millis(330),
                "loss {loss}: worst repair {:?}",
                run.worst_repair
            );
        }
    }

    #[test]
    fn a_burst_outage_on_an_uncapped_link_is_repaired_at_once() {
        let rtt = Duration::from_millis(12);
        // 100 ms of nothing at 10 Mbit/s: about 104 packets named in one NACK
        let run = simulate(
            4,
            healthy(0.0, 0.0, Some((2000, 2100))),
            |_| 10e6,
            true,
            rtt,
        );
        assert_eq!(run.unrepaired, 0);
        assert!(
            run.worst_repair <= Duration::from_millis(130),
            "worst repair {:?}",
            run.worst_repair
        );
    }

    #[test]
    fn return_path_loss_only_delays_a_repair_by_one_re_ask() {
        let rtt = Duration::from_millis(12);
        let run = simulate(6, healthy(0.01, 0.05, None), |_| 10e6, true, rtt);
        assert_eq!(run.unrepaired, 0);
        assert!(
            run.worst_repair <= Duration::from_millis(330),
            "worst repair {:?}",
            run.worst_repair
        );
    }
}

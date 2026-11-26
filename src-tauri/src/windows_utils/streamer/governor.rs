//! What a constant-QP session does when the link cannot carry it: picture quality first, the frame
//! rate only after that, and never a queue.
//!
//! The transport's bitrate target cannot shrink a constant-QP stream, so [`CeilingGovernor`] (a
//! fixed ceiling, the macOS `QpGovernor`'s formula and constants) and [`LinkGovernor`] (a standing
//! queue or a delivered rate far under the sent one, read from transport-wide feedback) raise the
//! QP instead. A bitrate estimate is never evidence, and loss with no queue behind it is Wi-Fi,
//! not congestion. At [`QP_LINK_MAX`] frames are gated to a divisor of the rate, and recovery
//! undoes the divisor first.
//!
//! All three switches (`link_governor`, `qp_ceiling_governor`, `client_pace_governor`) are off
//! unless `SE_TUNE_ON` names them.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::Result;

use super::nvidia::encoder::EncoderConfig;
use crate::streamer::congestion::PathSample;
use crate::streamer::lock::LockExt;
use crate::streamer::telemetry::{SessionTelemetry, Slot};

const MS: u64 = 1_000_000;

// the fixed ceiling: same numbers as macos_utils/streamer/pipeline.rs, so both hosts answer
// incompressible content the same way

const QP_GOV_WINDOW_NS: u64 = 500 * MS;
const QP_GOV_SNAP_NS: u64 = 25 * MS;
const QP_GOV_FAST_FRAMES: u32 = 3;
const QP_GOV_FAST_K: u64 = 4;
const QP_GOV_FAST_COOLDOWN_NS: u64 = 100 * MS;
const QP_GOV_DOWN_RATIO: f64 = 0.6;
const QP_GOV_DOWN_HOLD_NS: u64 = 1_000 * MS;

/// `clamp(4 x clamp(w*h*fps/10, 6, 40 Mbps), 24, 160 Mbps)`: 1080p60 49.8 Mbps, 4K60 160 Mbps.
pub(crate) fn qp_ceiling_bps(width: u32, height: u32, fps: u32) -> u64 {
    let base = (width as u64 * height as u64 * fps as u64 / 10).clamp(6_000_000, 40_000_000);
    (4 * base).clamp(24_000_000, 160_000_000)
}

/// `SE_QP_CEILING_MBPS`, as on macOS: a number of Mbps replaces the ceiling, `0` turns it off.
fn qp_ceiling_override() -> Option<Option<u64>> {
    static V: std::sync::OnceLock<Option<Option<u64>>> = std::sync::OnceLock::new();
    *V.get_or_init(|| qp_ceiling_from(crate::envcfg::var("SE_QP_CEILING_MBPS").ok().as_deref()))
}

fn qp_ceiling_from(v: Option<&str>) -> Option<Option<u64>> {
    let mbps = v?.trim().parse::<f64>().ok().filter(|m| *m >= 0.0)?;
    Some((mbps > 0.0).then_some((mbps * 1e6) as u64))
}

/// Raises the QP while the encoded rate over a 500 ms window is above a fixed ceiling, in one
/// move of `ceil(6 * log2(rate / C))`, and steps down one a second under `0.6 * C`. Three
/// frames in a row over four per-frame budgets step at once. A line-for-line port of the macOS
/// governor, where each constant was measured.
pub(crate) struct CeilingGovernor {
    q_user: u8,
    q: u8,
    ceiling_bps: u64,
    snaps: VecDeque<(u64, u64)>,
    cooldown_until_ns: u64,
    fast_until_ns: u64,
    newest: [u64; 5],
    newest_n: usize,
    big_start_ns: u64,
    below_since_ns: Option<u64>,
    last_ns: Option<u64>,
    pub(crate) above_ns: u64,
    pub(crate) steps: u64,
    fps: u32,
    big_run: u32,
    big_bytes: u64,
}

impl CeilingGovernor {
    pub(crate) fn new(q_user: u8, ceiling_bps: u64, fps: u32) -> Self {
        CeilingGovernor {
            q_user,
            q: q_user,
            ceiling_bps,
            snaps: VecDeque::with_capacity(32),
            cooldown_until_ns: 0,
            fast_until_ns: 0,
            newest: [0; 5],
            newest_n: 0,
            big_start_ns: 0,
            below_since_ns: None,
            last_ns: None,
            above_ns: 0,
            steps: 0,
            fps: fps.max(1),
            big_run: 0,
            big_bytes: 0,
        }
    }

    pub(crate) fn q(&self) -> u8 {
        self.q
    }

    pub(crate) fn ceiling_bps(&self) -> u64 {
        self.ceiling_bps
    }

    /// One encoded real frame (repeats are not fed). `Some((q, why))` on a step.
    pub(crate) fn frame(&mut self, now_ns: u64, bytes: u64) -> Option<(u8, String)> {
        self.newest[self.newest_n % 5] = bytes;
        self.newest_n += 1;
        let budget = self.ceiling_bps / 8 / self.fps as u64;
        if bytes > QP_GOV_FAST_K * budget {
            if self.big_run == 0 {
                self.big_start_ns = now_ns;
                self.big_bytes = 0;
            }
            self.big_run += 1;
            self.big_bytes += bytes;
        } else {
            self.big_run = 0;
            self.big_bytes = 0;
        }
        if self.big_run < QP_GOV_FAST_FRAMES || now_ns < self.fast_until_ns || self.q >= 51 {
            return None;
        }
        let span = now_ns
            .saturating_sub(self.big_start_ns)
            .max((self.big_run as u64 - 1) * 1_000_000_000 / self.fps as u64);
        let per = self.big_bytes as f64 / self.big_run as f64;
        let rate = per * 8.0 * (self.big_run - 1) as f64 / (span as f64 / 1e9);
        if rate <= self.ceiling_bps as f64 {
            return None;
        }
        let c = self.ceiling_bps as f64;
        let delta = (6.0 * (rate / c).log2()).ceil().max(1.0) as u8;
        let from = self.q;
        self.q = self.q.saturating_add(delta).min(51);
        self.cooldown_until_ns = now_ns + QP_GOV_WINDOW_NS;
        self.fast_until_ns = now_ns + QP_GOV_FAST_COOLDOWN_NS;
        self.snaps.clear();
        self.below_since_ns = None;
        self.steps += 1;
        let n = self.big_run;
        self.big_run = 0;
        self.big_bytes = 0;
        Some((
            self.q,
            format!(
                "{n} frames in a row over {QP_GOV_FAST_K}x the {:.0} KB per-frame budget ({:.1} \
                 Mbps at {} fps) > the {:.1} Mbps ceiling: QP {from} -> {} at once",
                budget as f64 / 1e3,
                rate / 1e6,
                self.fps,
                c / 1e6,
                self.q
            ),
        ))
    }

    /// The encoded byte total at `now_ns`. `Some((q, why))` when the QP should change.
    pub(crate) fn observe(&mut self, now_ns: u64, total_bytes: u64) -> Option<(u8, String)> {
        if let Some(last) = self.last_ns.replace(now_ns) {
            if self.q > self.q_user {
                self.above_ns += now_ns.saturating_sub(last);
            }
        }
        if self
            .snaps
            .back()
            .is_none_or(|(t, _)| now_ns.saturating_sub(*t) >= QP_GOV_SNAP_NS)
        {
            self.snaps.push_back((now_ns, total_bytes));
        }
        while self.snaps.len() > 2 && now_ns.saturating_sub(self.snaps[1].0) >= QP_GOV_WINDOW_NS {
            self.snaps.pop_front();
        }
        let (t0, b0) = *self.snaps.front()?;
        let span = now_ns.saturating_sub(t0);
        if span < QP_GOV_WINDOW_NS * 4 / 5 {
            return None;
        }
        let rate = total_bytes.saturating_sub(b0) as f64 * 8.0 / (span as f64 / 1e9);
        let c = self.ceiling_bps as f64;
        if rate > c {
            self.below_since_ns = None;
            if now_ns < self.cooldown_until_ns || self.q >= 51 {
                return None;
            }
            let mut newest = self.newest;
            newest.sort_unstable();
            let now_rate = if self.newest_n >= 5 {
                newest[2] as f64 * 8.0 * self.fps as f64
            } else {
                0.0
            };
            let delta = (6.0 * (rate.max(now_rate) / c).log2()).ceil().max(1.0) as u8;
            let from = self.q;
            self.q = self.q.saturating_add(delta).min(51);
            self.cooldown_until_ns = now_ns + QP_GOV_WINDOW_NS;
            self.snaps.clear();
            self.steps += 1;
            return Some((
                self.q,
                format!(
                    "encoded {:.1} Mbps over {} ms > the {:.1} Mbps ceiling: QP {from} -> {}",
                    rate / 1e6,
                    QP_GOV_WINDOW_NS / MS,
                    c / 1e6,
                    self.q
                ),
            ));
        }
        if rate < QP_GOV_DOWN_RATIO * c && self.q > self.q_user {
            let since = *self.below_since_ns.get_or_insert(now_ns);
            if now_ns.saturating_sub(since) >= QP_GOV_DOWN_HOLD_NS {
                let from = self.q;
                self.q -= 1;
                self.below_since_ns = Some(now_ns);
                self.steps += 1;
                return Some((
                    self.q,
                    format!(
                        "encoded {:.1} Mbps, under {:.0} % of the ceiling for a second: QP {from} -> {}",
                        rate / 1e6,
                        QP_GOV_DOWN_RATIO * 100.0,
                        self.q
                    ),
                ));
            }
        } else {
            self.below_since_ns = None;
        }
        None
    }
}

/// The highest QP the link side asks for before it gates the frame rate. At 34 the 4x4 quantiser
/// step is about an eighth of a black-on-white glyph edge: text goes soft but small UI text still
/// reads; by 38 to 40 one-pixel stems break up.
pub(crate) const QP_LINK_MAX: u8 = 34;
/// 60 -> 30 -> 20 -> 15 at the most.
pub(crate) const MAX_DIVISOR: u32 = 4;
/// With the frame rate at a quarter and the link still not carrying the stream, the QP may go
/// past `QP_LINK_MAX` up to here, six steps a move. Recovery takes it back first. A capture that
/// cannot hold a gated frame never gets here: it stops at `QP_LINK_MAX`.
const QP_LAST_RESORT: u8 = 51;
const LAST_RESORT_STEP: f64 = 6.0;
/// A keyframe asked for while the governor is engaged is encoded this much coarser: it is the
/// largest thing the stream sends.
pub(crate) const KEYFRAME_BOOST: u8 = 6;

/// Each judged window covers at least this much send time and this many packets.
const EVAL_SPAN_US: i64 = 100_000;
const MIN_PACKETS: u32 = 8;
/// A queue this deep over the path's base delay is standing. Measured from each window's lowest
/// one-way delay, so jitter on a healthy link does not reach it.
const QUEUE_HIGH_US: i64 = 40_000;
/// Under this the queue is empty enough to probe upward.
const QUEUE_LOW_US: i64 = 20_000;
/// A queue that rose by this much in each of two consecutive `GROW_INTERVAL_NS` is filling: the
/// link takes less than goes in. One rise alone is a step in the path's delay, not a queue.
const GROW_US: i64 = 10_000;
const GROW_INTERVAL_NS: u64 = 300 * MS;
/// Byte delivery share (bytes that arrived over bytes sent, loss only): under this with a standing
/// queue in both windows, the queue is overflowing. A path delay with random loss can look the
/// same; the drain check after the raise tells them apart (`UNDO_*`).
const SHARE_OVERFLOW: f64 = 0.97;
/// Under this in both windows, the path is dropping too much to be anything but too slow (a
/// shallow buffer drops instead of queueing). No working Wi-Fi link loses 40 % at random.
const SHARE_SEVERE: f64 = 0.6;
/// A queue that has stood flat (within `FLAT_US`) for `REBASE_AFTER_NS` with nothing lost in any
/// window is the path's new base delay (a route or relay change, a cellular hop), not a queue: a
/// full drop-tail queue always loses something.
const FLAT_US: i64 = 10_000;
const REBASE_AFTER_NS: u64 = 1_500 * MS;
const SHARE_CLEAN: f64 = 0.995;
/// A raise after which the queue stood flat for `REBASE_AFTER_NS` although the encoded rate fell
/// to `UNDO_RATE` of what it was did not meet a queue: it is undone and the delay taken as the
/// path's. Looked for until `UNDO_WITHIN_NS` after the first raise of the episode.
const UNDO_RATE: f64 = 0.8;
/// ... and only once it sends less than this share of what the link delivered at the raise: a
/// queue still being overfilled cannot drain, whatever the rate did.
const UNDO_UNDER: f64 = 0.95;
const UNDO_WITHIN_NS: u64 = 4_000 * MS;
/// A probe upward needs arrivals not stretched against sends by more than this.
const SIGMA_CLEAR: f64 = 0.9;
const LOSS_CLEAR: f64 = 0.15;
/// The cut leaves room to drain what is queued: the target is the delivered rate times
/// `1 - queue / DRAIN_S`, between `TARGET_MIN` and `TARGET_MAX` of it.
const DRAIN_S: f64 = 1.0;
const TARGET_MAX: f64 = 0.85;
const TARGET_MIN: f64 = 0.5;
/// After a raise, long enough for two windows of packets at the new QP to be reported, plus
/// twice the queue standing at the time.
const RAISE_HOLD_NS: u64 = 400 * MS;
const RAISE_HOLD_MAX_NS: u64 = 1_000 * MS;
/// Clear for this long after a raise before the first step down, then `DOWN_STEP_NS` per step.
const FIRST_DOWN_NS: u64 = 1_000 * MS;
const DOWN_STEP_NS: u64 = 500 * MS;
/// A step down followed by congestion within this long failed; the next try at that level waits
/// twice as long, up to `DOWN_STEP_MAX_NS`.
const PROBE_FAIL_NS: u64 = 4_000 * MS;
const DOWN_STEP_MAX_NS: u64 = 10_000 * MS;
/// While the delivered rate measured at the last congestion is this recent, a stream sending under
/// `ACTIVE_SHARE` of it does not probe: it would learn nothing.
const CAP_MEMORY_NS: u64 = 5_000 * MS;
const ACTIVE_SHARE: f64 = 0.5;
/// A stream this light cannot congest a link by itself: sustained for `IDLE_RELEASE_NS`, the
/// governor lets go entirely, so a still screen is refined at the user's QP by the next repeat.
const IDLE_BPS: f64 = 300_000.0;
const IDLE_RELEASE_NS: u64 = 2_000 * MS;
/// The encoded rate a raise is sized from.
const ENC_WINDOW_NS: u64 = 500 * MS;
/// The response to a raise is read once its frames fill the rate window, and not after this: a
/// later reading mixes in whatever the content did since.
const SLOPE_READ_NS: u64 = 600 * MS;
const SLOPE_STALE_NS: u64 = 1_200 * MS;
const SLOPE_MIN: f64 = 6.0;
const SLOPE_MAX: f64 = 12.0;
const BASE_BUCKET_NS: u64 = 1_000 * MS;
const BASE_BUCKETS: usize = 10;
const SAMPLE_KEEP_NS: u64 = 2_000 * MS;
const SAMPLE_KEEP: usize = 128;
const QUEUE_HISTORY_NS: u64 = 5_000 * MS;
/// A queue that rose by `CREEP_US` over `CREEP_NS`, by at least `CREEP_THIRD_US` in each third of
/// it, never falling back by more than `FLAT_US`, is being filled slowly: a stream 1 to 3 % over
/// the link, which neither the growth rule's 300 ms intervals nor loss (until the buffer is full)
/// would ever call. A step in the path's delay puts its whole rise into one third.
const CREEP_NS: u64 = 3_000 * MS;
const CREEP_US: i64 = 25_000;
const CREEP_THIRD_US: i64 = 6_000;
/// A rebase is only for a step: the queue was under `QUEUE_LOW_US` no longer than this before
/// it went flat. A queue that crept up to where it stands is a queue, however flat it is now.
const STEP_WITHIN_NS: u64 = 600 * MS;

/// One judged window of feedback.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Window {
    pub valid: bool,
    pub packets: u32,
    /// Bytes that arrived over bytes sent: loss only.
    pub share: f64,
    /// Send span over arrival span of the packets that arrived: under 1, arrivals are stretched.
    pub sigma: f64,
    /// The window's lowest one-way delay over the path's base.
    pub queue_us: i64,
    pub delivered_bps: f64,
    pub sent_bps: f64,
    pub loss: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    NoData,
    Congested,
    /// A standing queue, not proven congestion: holds a probe back.
    Queued,
    /// The queue did not drain when the rate fell: the path's delay moved.
    NotAQueue,
    Clear,
    Neither,
}

/// A change the link side asks for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LinkMove {
    pub q: u8,
    pub divisor: u32,
    pub raise: bool,
    pub why: String,
}

pub(crate) struct LinkGovernor {
    q_user: u8,
    q: u8,
    q_max: u8,
    div: u32,
    /// Whether this capture path can gate frames without losing the last one of a burst.
    hold_frames: bool,
    samples: VecDeque<(u64, PathSample)>,
    /// Per-second minimum one-way delay, `(second + 1, min)`; 0 is empty.
    base: [(u64, i64); BASE_BUCKETS],
    /// `(when, queue, byte share)` of every valid newest window.
    queue_hist: VecDeque<(u64, i64, f64)>,
    /// The first raise of an episode: when, the level before it, the encoded and delivered rates
    /// then.
    undo: Option<(u64, (u8, u32), f64, f64)>,
    fresh: bool,
    hold_until_ns: u64,
    last_evidence_ns: u64,
    last_move_ns: u64,
    last_was_raise: bool,
    clear_since_ns: Option<u64>,
    idle_since_ns: Option<u64>,
    step_hold_ns: u64,
    /// The last step down: when, and the level it went to.
    last_down: Option<(u64, u8, u32)>,
    /// The lowest level a probe failed at; going below it again means the link changed.
    failed: Option<(u8, u32)>,
    cap_bps: f64,
    cap_at_ns: u64,
    enc: VecDeque<(u64, u64)>,
    enc_bytes: u64,
    enc_since_ns: Option<u64>,
    last_tick_ns: Option<u64>,
    no_step_before_ns: u64,
    /// The QP does not reach the stream: only the frame rate answers.
    pinned: bool,
    /// QP steps that halve the bits for this encoder and content.
    halving_qp: f64,
    /// The last QP raise: encoded rate before it, its size, and when.
    last_raise: Option<(f64, u8, u64)>,
    pub(crate) raises: u64,
    pub(crate) steps: u64,
    pub(crate) rebases: u64,
    pub(crate) above_ns: u64,
    pub(crate) last: Window,
}

impl LinkGovernor {
    pub(crate) fn new(q_user: u8) -> Self {
        LinkGovernor {
            q_user,
            q: q_user,
            q_max: QP_LINK_MAX.max(q_user),
            div: 1,
            hold_frames: true,
            samples: VecDeque::with_capacity(SAMPLE_KEEP),
            base: [(0, 0); BASE_BUCKETS],
            queue_hist: VecDeque::with_capacity(64),
            undo: None,
            fresh: false,
            hold_until_ns: 0,
            last_evidence_ns: 0,
            last_move_ns: 0,
            last_was_raise: false,
            clear_since_ns: None,
            idle_since_ns: None,
            step_hold_ns: DOWN_STEP_NS,
            last_down: None,
            failed: None,
            cap_bps: 0.0,
            cap_at_ns: 0,
            enc: VecDeque::with_capacity(256),
            enc_bytes: 0,
            enc_since_ns: None,
            last_tick_ns: None,
            no_step_before_ns: 0,
            pinned: false,
            halving_qp: SLOPE_MIN,
            last_raise: None,
            raises: 0,
            steps: 0,
            rebases: 0,
            above_ns: 0,
            last: Window::default(),
        }
    }

    pub(crate) fn q(&self) -> u8 {
        self.q
    }

    pub(crate) fn divisor(&self) -> u32 {
        self.div
    }

    pub(crate) fn halving_qp(&self) -> f64 {
        self.halving_qp
    }

    /// Whether the capture path can hold a gated frame for later delivery. Where it cannot, the
    /// frame rate is never cut and the QP answers alone.
    pub(crate) fn hold_frames(&mut self, yes: bool) {
        self.hold_frames = yes;
        if !yes {
            self.div = 1;
        }
    }

    /// The path changed under the session (a new ICE candidate pair): what was learnt about its
    /// delay belongs to the old one.
    pub(crate) fn path_changed(&mut self) {
        self.undo = None;
        self.base = [(0, 0); BASE_BUCKETS];
        self.queue_hist.clear();
        self.samples.clear();
        self.rebases += 1;
    }

    /// One transport-wide feedback report, received at `now_ns`.
    pub(crate) fn feedback(&mut self, now_ns: u64, s: &PathSample) {
        if s.reported == 0 {
            return;
        }
        if s.matched > 0 {
            let id = now_ns / BASE_BUCKET_NS + 1;
            let slot = &mut self.base[(id as usize) % BASE_BUCKETS];
            if slot.0 != id {
                *slot = (id, s.owd_min_us);
            } else if s.owd_min_us < slot.1 {
                slot.1 = s.owd_min_us;
            }
        }
        while self.samples.len() >= SAMPLE_KEEP
            || self
                .samples
                .front()
                .is_some_and(|(t, _)| now_ns.saturating_sub(*t) > SAMPLE_KEEP_NS)
        {
            self.samples.pop_front();
        }
        self.samples.push_back((now_ns, *s));
        self.fresh = true;
    }

    /// One emitted access unit. A keyframe is left out of the rate a raise is sized from.
    pub(crate) fn encoded(&mut self, now_ns: u64, bytes: u64, keyframe: bool) {
        self.enc_since_ns.get_or_insert(now_ns);
        if !keyframe {
            if self.enc.len() == self.enc.capacity() {
                if let Some((_, b)) = self.enc.pop_front() {
                    self.enc_bytes -= b;
                }
            }
            self.enc.push_back((now_ns, bytes));
            self.enc_bytes += bytes;
        }
    }

    fn enc_rate(&mut self, now_ns: u64) -> f64 {
        while let Some(&(t, b)) = self.enc.front() {
            if now_ns.saturating_sub(t) < ENC_WINDOW_NS {
                break;
            }
            self.enc.pop_front();
            self.enc_bytes -= b;
        }
        let Some(since) = self.enc_since_ns else {
            return 0.0;
        };
        let span = now_ns.saturating_sub(since).clamp(100 * MS, ENC_WINDOW_NS);
        self.enc_bytes as f64 * 8.0 / (span as f64 / 1e9)
    }

    fn base_owd(&self, now_ns: u64) -> Option<i64> {
        let cur = now_ns / BASE_BUCKET_NS + 1;
        self.base
            .iter()
            .filter(|(id, _)| *id != 0 && *id + BASE_BUCKETS as u64 > cur)
            .map(|(_, m)| *m)
            .min()
    }

    /// The newest window covering `EVAL_SPAN_US` of send time, and the one before it.
    fn windows(&self, now_ns: u64) -> (Window, Window) {
        let base = self.base_owd(now_ns);
        let mut it = self.samples.iter().rev().map(|(_, s)| s);
        let a = Self::window(&mut it, base);
        let b = Self::window(&mut it, base);
        (a, b)
    }

    fn window<'a>(it: &mut impl Iterator<Item = &'a PathSample>, base: Option<i64>) -> Window {
        let mut reported = 0u32;
        let mut lost = 0u32;
        let mut matched = 0u32;
        let mut sent = 0u64;
        let mut recv = 0u64;
        let mut send = (i64::MAX, i64::MIN);
        let mut arr = (i64::MAX, i64::MIN);
        let mut owd_min = i64::MAX;
        for s in it.by_ref() {
            reported += s.reported;
            lost += s.lost;
            sent += s.sent_bytes;
            if s.known > 0 {
                send = (send.0.min(s.first_send_us), send.1.max(s.last_send_us));
            }
            if s.matched > 0 {
                matched += s.matched;
                recv += s.recv_bytes;
                arr = (arr.0.min(s.first_arrival_us), arr.1.max(s.last_arrival_us));
                owd_min = owd_min.min(s.owd_min_us);
            }
            if send.0 != i64::MAX && send.1 - send.0 >= EVAL_SPAN_US {
                break;
            }
        }
        let send_span = send.1.saturating_sub(send.0);
        let arr_span = arr.1.saturating_sub(arr.0);
        if reported < MIN_PACKETS
            || matched < 2
            || sent == 0
            || send_span < EVAL_SPAN_US
            || arr_span <= 0
        {
            return Window::default();
        }
        Window {
            valid: true,
            packets: reported,
            share: recv as f64 / sent as f64,
            sigma: send_span as f64 / arr_span as f64,
            queue_us: match base {
                Some(b) if owd_min != i64::MAX => (owd_min - b).max(0),
                _ => 0,
            },
            delivered_bps: recv as f64 * 8.0 / (arr_span as f64 / 1e6),
            sent_bps: sent as f64 * 8.0 / (send_span as f64 / 1e6),
            loss: lost as f64 / reported.max(1) as f64,
        }
    }

    /// The queue at or before `ago` ago, from the history of newest windows.
    fn queue_at(&self, now_ns: u64, ago: u64) -> Option<i64> {
        let t = now_ns.checked_sub(ago)?;
        self.queue_hist
            .iter()
            .rev()
            .find(|(at, _, _)| *at <= t)
            .map(|(_, q, _)| *q)
    }

    /// Rising by `GROW_US` in each of the last two intervals: filling, not stepped.
    fn growing(&self, now_ns: u64, q: i64) -> bool {
        match (
            self.queue_at(now_ns, GROW_INTERVAL_NS),
            self.queue_at(now_ns, 2 * GROW_INTERVAL_NS),
        ) {
            (Some(q1), Some(q2)) => q > QUEUE_HIGH_US && q - q1 >= GROW_US && q1 - q2 >= GROW_US,
            _ => false,
        }
    }

    /// Rising slowly and steadily over `CREEP_NS`, never falling back: filled from a stream just
    /// over the link.
    fn creeping(&self, now_ns: u64, q: i64) -> bool {
        let (Some(q1), Some(q2), Some(q3)) = (
            self.queue_at(now_ns, CREEP_NS / 3),
            self.queue_at(now_ns, 2 * CREEP_NS / 3),
            self.queue_at(now_ns, CREEP_NS),
        ) else {
            return false;
        };
        if q <= QUEUE_LOW_US
            || q - q3 < CREEP_US
            || q - q1 < CREEP_THIRD_US
            || q1 - q2 < CREEP_THIRD_US
            || q2 - q3 < CREEP_THIRD_US
        {
            return false;
        }
        let since = now_ns.saturating_sub(CREEP_NS);
        let mut top = i64::MIN;
        self.queue_hist
            .iter()
            .filter(|(t, _, _)| *t >= since)
            .all(|(_, x, _)| {
                top = top.max(*x);
                *x >= top - FLAT_US
            })
    }

    /// The queue came up as a step: it was low just before the flat stretch began.
    fn stepped(&self, now_ns: u64) -> bool {
        let flat_from = now_ns.saturating_sub(REBASE_AFTER_NS);
        self.queue_hist
            .iter()
            .rev()
            .find(|(t, x, _)| *t < flat_from && *x < QUEUE_LOW_US)
            .is_some_and(|(t, _, _)| flat_from - t <= STEP_WITHIN_NS)
    }

    /// Flat for `REBASE_AFTER_NS`; and with `clean`, nothing lost in any window of it.
    fn flat(&self, now_ns: u64, q: i64, clean: bool) -> bool {
        let since = now_ns.saturating_sub(REBASE_AFTER_NS);
        self.queue_hist.front().is_some_and(|(t, _, _)| *t <= since)
            && self
                .queue_hist
                .iter()
                .filter(|(t, _, _)| *t >= since)
                .all(|(_, x, share)| (x - q).abs() < FLAT_US && (!clean || *share >= SHARE_CLEAN))
    }

    fn judge(&mut self, now_ns: u64, a: &Window, b: &Window) -> Verdict {
        if !a.valid {
            return Verdict::NoData;
        }
        self.queue_hist.push_back((now_ns, a.queue_us, a.share));
        while self
            .queue_hist
            .front()
            .is_some_and(|(t, _, _)| now_ns.saturating_sub(*t) > QUEUE_HISTORY_NS)
        {
            self.queue_hist.pop_front();
        }
        if let Some((at, _, enc_then, delivered_then)) = self.undo {
            let age = now_ns.saturating_sub(at);
            let enc = self.enc_rate(now_ns);
            if age > UNDO_WITHIN_NS {
                self.undo = None;
            } else if age >= REBASE_AFTER_NS
                && a.queue_us >= QUEUE_LOW_US
                && self.flat(now_ns, a.queue_us, false)
                && enc <= UNDO_RATE * enc_then
                && enc <= UNDO_UNDER * delivered_then
            {
                return Verdict::NotAQueue;
            }
        }
        let both = |f: &dyn Fn(&Window) -> bool| f(a) && b.valid && f(b);
        let overflow = both(&|w| w.queue_us > QUEUE_HIGH_US && w.share < SHARE_OVERFLOW);
        let severe = both(&|w| w.share < SHARE_SEVERE);
        if self.growing(now_ns, a.queue_us)
            || self.creeping(now_ns, a.queue_us)
            || overflow
            || severe
        {
            return Verdict::Congested;
        }
        if a.queue_us >= QUEUE_LOW_US && self.flat(now_ns, a.queue_us, true) && self.stepped(now_ns)
        {
            // a standing, flat, lossless queue is the path's new base delay
            self.path_changed();
            return Verdict::Neither;
        }
        if a.queue_us >= QUEUE_HIGH_US {
            return Verdict::Queued;
        }
        if a.queue_us < QUEUE_LOW_US && a.sigma > SIGMA_CLEAR && a.loss < LOSS_CLEAR {
            return Verdict::Clear;
        }
        Verdict::Neither
    }

    /// Decide. `q_eff` is the QP in force (the ceiling's may be above this side's).
    pub(crate) fn tick(&mut self, now_ns: u64, q_eff: u8) -> Option<LinkMove> {
        if let Some(last) = self.last_tick_ns.replace(now_ns) {
            if self.q > self.q_user || self.div > 1 {
                self.above_ns += now_ns.saturating_sub(last);
            }
        }
        if let Some((t, q, d)) = self.last_down {
            if now_ns.saturating_sub(t) >= PROBE_FAIL_NS {
                self.last_down = None;
                if self.failed.is_some_and(|f| !Self::below(f, (q, d))) {
                    // a probe into the level that failed before held: the link changed
                    self.failed = None;
                    self.step_hold_ns = DOWN_STEP_NS;
                }
            }
        }
        self.learn_slope(now_ns);
        if self.fresh {
            self.fresh = false;
            let (a, b) = self.windows(now_ns);
            if a.valid {
                self.last = a;
            }
            match self.judge(now_ns, &a, &b) {
                Verdict::Congested => {
                    self.last_evidence_ns = now_ns;
                    self.clear_since_ns = None;
                    self.idle_since_ns = None;
                    if now_ns >= self.hold_until_ns {
                        return self.raise(now_ns, q_eff, a);
                    }
                    return None;
                }
                Verdict::Queued => self.clear_since_ns = None,
                Verdict::NotAQueue => return self.undo_raise(now_ns, a),
                Verdict::Clear => {
                    self.clear_since_ns.get_or_insert(now_ns);
                }
                Verdict::NoData | Verdict::Neither => {}
            }
        }
        if let Some(m) = self.release_if_idle(now_ns) {
            return Some(m);
        }
        self.step_down(now_ns)
    }

    /// The response to the last raise, read once, from frames encoded after it only.
    fn learn_slope(&mut self, now_ns: u64) {
        let Some((before, delta, at)) = self.last_raise else {
            return;
        };
        let age = now_ns.saturating_sub(at);
        if age < SLOPE_READ_NS {
            return;
        }
        self.last_raise = None;
        if age > SLOPE_STALE_NS || delta < 4 {
            return;
        }
        let after = self.enc_rate(now_ns);
        if after > 0.0 && after < before {
            let h = delta as f64 / (before / after).log2();
            self.halving_qp = (0.5 * self.halving_qp + 0.5 * h).clamp(SLOPE_MIN, SLOPE_MAX);
        }
    }

    fn undo_raise(&mut self, now_ns: u64, a: Window) -> Option<LinkMove> {
        let (_, before, _, _) = self.undo.take()?;
        let from = (self.q, self.div);
        (self.q, self.div) = before;
        self.path_changed();
        self.hold_until_ns = 0;
        self.failed = None;
        self.last_down = None;
        self.step_hold_ns = DOWN_STEP_NS;
        self.last_move_ns = now_ns;
        self.last_was_raise = false;
        self.steps += 1;
        Some(LinkMove {
            q: self.q,
            divisor: self.div,
            raise: false,
            why: format!(
                "the {:.0} ms queue did not drain when the stream slowed: it is the path's delay; \
                 QP {} -> {}, frame rate 1/{} -> 1/{}",
                a.queue_us as f64 / 1e3,
                from.0,
                self.q,
                from.1,
                self.div
            ),
        })
    }

    fn release_if_idle(&mut self, now_ns: u64) -> Option<LinkMove> {
        if self.q <= self.q_user && self.div == 1 {
            self.idle_since_ns = None;
            return None;
        }
        if self.enc_rate(now_ns) >= IDLE_BPS {
            self.idle_since_ns = None;
            return None;
        }
        let since = *self.idle_since_ns.get_or_insert(now_ns);
        if now_ns.saturating_sub(since) < IDLE_RELEASE_NS {
            return None;
        }
        let from = (self.q, self.div);
        self.q = self.q_user;
        self.div = 1;
        self.idle_since_ns = None;
        self.failed = None;
        self.last_down = None;
        self.step_hold_ns = DOWN_STEP_NS;
        self.last_move_ns = now_ns;
        self.last_was_raise = false;
        self.steps += 1;
        Some(LinkMove {
            q: self.q,
            divisor: 1,
            raise: false,
            why: format!(
                "under {:.1} Mbps for {} s, which no link is congested by: QP {} -> {}, frame \
                 rate 1/{} -> 1/1",
                IDLE_BPS / 1e6,
                IDLE_RELEASE_NS / 1_000_000_000,
                from.0,
                self.q,
                from.1
            ),
        })
    }

    fn raise(&mut self, now_ns: u64, q_eff: u8, a: Window) -> Option<LinkMove> {
        self.cap_bps = a.delivered_bps;
        self.cap_at_ns = now_ns;
        if let Some((t, q, d)) = self.last_down.take() {
            if now_ns.saturating_sub(t) < PROBE_FAIL_NS {
                self.step_hold_ns = (self.step_hold_ns * 2).min(DOWN_STEP_MAX_NS);
                self.failed = Some((q, d));
            }
        }
        let drain = (1.0 - a.queue_us as f64 / 1e6 / DRAIN_S).clamp(TARGET_MIN, TARGET_MAX);
        let target = a.delivered_bps * drain;
        let enc = self.enc_rate(now_ns);
        if target <= 0.0 || enc <= target {
            // already under what the link delivers; what is queued drains by itself
            return None;
        }
        if self.last_raise.is_none() {
            // no fresh reading of the last response: let what was learnt fade toward the textbook
            self.halving_qp = SLOPE_MIN + (self.halving_qp - SLOPE_MIN) * 0.5;
        }
        let from = (q_eff.max(self.q), self.div);
        if self.undo.is_none() {
            self.undo = Some((now_ns, (self.q, self.div), enc, a.delivered_bps));
        }
        let sized = (self.halving_qp * (enc / target).log2()).ceil().max(1.0);
        if from.0 < self.q_max {
            self.q = from.0.saturating_add(sized as u8).min(self.q_max);
            self.last_raise = Some((enc, self.q - from.0, now_ns));
        } else if self.div < MAX_DIVISOR && self.hold_frames {
            // a picture that stands for more periods carries more change; taken as the square
            // root of the periods, the frame count has to fall by the square of the overshoot
            let want = (self.div as f64 * (enc / target).powi(2)).ceil() as u32;
            self.q = from.0;
            self.div = want.clamp(self.div + 1, MAX_DIVISOR);
        } else if self.hold_frames && !self.pinned && from.0 < QP_LAST_RESORT {
            // only after the frame rate is at a quarter: where frames cannot be held the QP stops
            // at the bound, which is set by legibility
            // a few steps at a time: a shallow queue drops bursts of sparse frames even when the
            // average fits, and the delivered rate read during those drops understates the link
            let step = sized.min(LAST_RESORT_STEP) as u8;
            self.q = from.0.saturating_add(step).min(QP_LAST_RESORT);
        } else {
            return None;
        }
        let queue_ns = a.queue_us.max(0) as u64 * 1_000;
        self.hold_until_ns = now_ns + (RAISE_HOLD_NS + 2 * queue_ns).min(RAISE_HOLD_MAX_NS);
        self.last_move_ns = now_ns;
        self.last_was_raise = true;
        self.raises += 1;
        Some(LinkMove {
            q: self.q,
            divisor: self.div,
            raise: true,
            why: format!(
                "link delivers {:.2} of {:.2} Mbps sent (queue {:.0} ms, byte share {:.2}, \
                 arrival stretch {:.2}), encoding {:.2} Mbps against {:.2}: QP {} -> {}, frame \
                 rate 1/{} -> 1/{}",
                a.delivered_bps / 1e6,
                a.sent_bps / 1e6,
                a.queue_us as f64 / 1e3,
                a.share,
                a.sigma,
                enc / 1e6,
                target / 1e6,
                from.0,
                self.q,
                from.1,
                self.div
            ),
        })
    }

    /// The encoder does not take a QP change: what is in force is `q`, and from now on the only
    /// answer left is the frame rate.
    pub(crate) fn pin_qp(&mut self, q: u8) {
        self.pinned = true;
        self.q = q;
        self.q_max = q;
        self.q_user = q;
        self.failed = None;
        self.last_down = None;
        self.step_hold_ns = DOWN_STEP_NS;
    }

    /// No step down before `until_ns`.
    pub(crate) fn defer_steps(&mut self, until_ns: u64) {
        self.no_step_before_ns = self.no_step_before_ns.max(until_ns);
    }

    /// `x` is a lower level (more quality) than `y`.
    fn below(x: (u8, u32), y: (u8, u32)) -> bool {
        (x.1, x.0) < (y.1, y.0)
    }

    fn step_down(&mut self, now_ns: u64) -> Option<LinkMove> {
        if self.q <= self.q_user && self.div == 1 {
            self.undo = None;
        }
        if (self.q <= self.q_user && self.div == 1) || now_ns < self.no_step_before_ns {
            return None;
        }
        let since = self.clear_since_ns?;
        let quiet_from = since.max(self.last_move_ns).max(self.last_evidence_ns);
        // the last resort first, then the frame rate, then the QP
        let next = if self.q > self.q_max {
            (self.q - 1, self.div)
        } else if self.div > 1 {
            (self.q, self.div - 1)
        } else {
            (self.q - 1, 1)
        };
        let mut need = if self.last_was_raise {
            FIRST_DOWN_NS
        } else {
            DOWN_STEP_NS
        };
        if self.failed.is_some_and(|f| !Self::below(f, next)) {
            // into a level that failed recently: wait out the backoff
            need = need.max(self.step_hold_ns);
        }
        if now_ns.saturating_sub(quiet_from) < need {
            return None;
        }
        let cap_known = self.cap_bps > 0.0 && now_ns.saturating_sub(self.cap_at_ns) < CAP_MEMORY_NS;
        let enc = self.enc_rate(now_ns);
        if cap_known && enc < ACTIVE_SHARE * self.cap_bps {
            return None;
        }
        let from = (self.q, self.div);
        (self.q, self.div) = next;
        if let Some(f) = self.failed {
            if Self::below((self.q, self.div), f) {
                // past the level that failed before: the link has changed, probe at full pace
                self.failed = None;
                self.step_hold_ns = DOWN_STEP_NS;
            }
        }
        self.last_down = Some((now_ns, self.q, self.div));
        self.last_move_ns = now_ns;
        self.last_was_raise = false;
        self.steps += 1;
        Some(LinkMove {
            q: self.q,
            divisor: self.div,
            raise: false,
            why: format!(
                "queue {:.0} ms, arrival stretch {:.2}, clear for {} ms: QP {} -> {}, frame rate \
                 1/{} -> 1/{}",
                self.last.queue_us as f64 / 1e3,
                self.last.sigma,
                now_ns.saturating_sub(quiet_from) / MS,
                from.0,
                self.q,
                from.1,
                self.div
            ),
        })
    }
}

/// Real frames after a raise within which it must show: the QP read back from the bitstream, or,
/// where nothing can be read, the encoded size.
const CONFIRM_FRAMES: u32 = 6;
/// The costly path (a re-init, a keyframe) at most once in this long.
const COSTLY_MIN_GAP_NS: u64 = 10_000 * MS;

/// Whether a raise reached the stream. `Ok(true)` from an encoder is a promise, not proof.
#[derive(Default)]
struct MoveCheck {
    /// Sizes of the newest real frames before the raise.
    recent: VecDeque<u64>,
    pending: Option<Pending>,
}

struct Pending {
    from: u8,
    to: u8,
    costly: bool,
    before: Option<u64>,
    after: Vec<u64>,
    seen_other: Option<u8>,
}

enum Check {
    Waiting,
    Confirmed,
    /// Not in the stream: the QP it carries (where it could be read), whether this was already
    /// the costly path, and whether the move was a raise.
    Unconfirmed(u8, bool, bool),
}

fn median(v: &mut [u64]) -> Option<u64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

impl MoveCheck {
    fn raised(&mut self, from: u8, to: u8, costly: bool) {
        let mut r: Vec<u64> = match self.pending.take() {
            // a move on top of one still being checked is judged from the same baseline
            Some(p) if p.before.is_some() => vec![p.before.unwrap_or(0)],
            _ => self.recent.iter().copied().collect(),
        };
        let base = if r.len() == 1 {
            r.first().copied()
        } else {
            None
        };
        self.pending = Some(Pending {
            from,
            to,
            costly,
            before: base.or(if r.len() >= 4 { median(&mut r) } else { None }),
            after: Vec::with_capacity(CONFIRM_FRAMES as usize),
            seen_other: None,
        });
    }

    fn frame(&mut self, bytes: u64, readback: Option<u8>, real: bool) -> Check {
        let Some(p) = self.pending.as_mut() else {
            if real {
                if self.recent.len() == 8 {
                    self.recent.pop_front();
                }
                self.recent.push_back(bytes);
            }
            return Check::Waiting;
        };
        match readback {
            Some(q) if q == p.to => {
                self.pending = None;
                self.recent.clear();
                return Check::Confirmed;
            }
            Some(q) => p.seen_other = Some(q),
            None => {}
        }
        if real {
            p.after.push(bytes);
        }
        if (p.after.len() as u32) < CONFIRM_FRAMES {
            return Check::Waiting;
        }
        let Some(p) = self.pending.take() else {
            return Check::Waiting;
        };
        self.recent.clear();
        let raise = p.to > p.from;
        if let Some(q) = p.seen_other {
            return Check::Unconfirmed(q, p.costly, raise);
        }
        // nothing readable: only a size that did not move at all counts against the change
        let mut after = p.after;
        match (p.before, median(&mut after)) {
            (Some(b), Some(a)) if (raise && a >= b) || (!raise && a <= b) => {
                Check::Unconfirmed(p.from, p.costly, raise)
            }
            _ => Check::Confirmed,
        }
    }
}

/// The two governors and the QP they put in force together. Pure; what the simulations drive.
pub(crate) struct Governors {
    pub q_user: u8,
    pub ceiling: Option<CeilingGovernor>,
    pub link: Option<LinkGovernor>,
    total_bytes: u64,
    pending: Option<String>,
    shown: (u8, u32),
    check: MoveCheck,
    costly_at: Option<u64>,
    /// The encoder ignores QP changes: only the frame rate is left.
    pub qp_dead: bool,
    pub unconfirmed: u64,
    pub confirmed: u64,
    pub costly: u64,
}

impl Governors {
    pub(crate) fn new(q_user: u8, ceiling_bps: Option<u64>, fps: u32, link: bool) -> Self {
        Governors {
            q_user,
            ceiling: ceiling_bps.map(|c| CeilingGovernor::new(q_user, c, fps)),
            link: link.then(|| LinkGovernor::new(q_user)),
            total_bytes: 0,
            pending: None,
            shown: (q_user, 1),
            check: MoveCheck::default(),
            costly_at: None,
            qp_dead: false,
            unconfirmed: 0,
            confirmed: 0,
            costly: 0,
        }
    }

    pub(crate) fn q(&self) -> u8 {
        let c = self.ceiling.as_ref().map_or(self.q_user, |g| g.q());
        let l = self.link.as_ref().map_or(self.q_user, |g| g.q());
        self.q_user.max(c).max(l)
    }

    pub(crate) fn divisor(&self) -> u32 {
        self.link.as_ref().map_or(1, |g| g.divisor())
    }

    /// Whether anything is engaged: the QP above the user's or the frame rate gated.
    pub(crate) fn engaged(&self) -> bool {
        self.q() > self.q_user || self.divisor() > 1
    }

    /// Whether the capture path can hold a gated frame (see [`LinkGovernor::hold_frames`]).
    pub(crate) fn hold_frames(&mut self, yes: bool) {
        if let Some(g) = self.link.as_mut() {
            g.hold_frames(yes);
        }
    }

    pub(crate) fn path_changed(&mut self) {
        if let Some(g) = self.link.as_mut() {
            g.path_changed();
        }
    }

    /// A raise is being checked, so every access unit's QP should be read.
    pub(crate) fn checking(&self) -> bool {
        self.check.pending.is_some()
    }

    /// An emitted access unit, with the QP its first slice carries where it could be read.
    /// Repeats count towards the ceiling's rate, not its per-frame path. `Some(q)`: the raise to
    /// `q` did not reach the stream; try the backend's costly path for it and report back with
    /// [`Self::costly_result`].
    pub(crate) fn emitted(
        &mut self,
        now_ns: u64,
        bytes: u64,
        repeat: bool,
        keyframe: bool,
        readback: Option<u8>,
    ) -> Option<(u8, u8)> {
        self.total_bytes += bytes;
        if let Some(g) = self.ceiling.as_mut() {
            if !repeat {
                if let Some((_, why)) = g.frame(now_ns, bytes) {
                    self.pending = Some(why);
                }
            }
        }
        if let Some(g) = self.link.as_mut() {
            g.encoded(now_ns, bytes, keyframe);
        }
        // a keyframe may carry the boosted QP; it says nothing about the move
        let readback = readback.filter(|_| !keyframe);
        match self.check.frame(bytes, readback, !repeat && !keyframe) {
            Check::Waiting => None,
            Check::Confirmed => {
                self.confirmed += 1;
                None
            }
            Check::Unconfirmed(in_force, costly_tried, raise) => {
                self.unconfirmed += 1;
                let to = self.q();
                let allowed = self
                    .costly_at
                    .is_none_or(|t| now_ns.saturating_sub(t) >= COSTLY_MIN_GAP_NS);
                if !costly_tried && allowed && !self.qp_dead {
                    self.costly_at = Some(now_ns);
                    return Some((to, in_force));
                }
                if raise {
                    self.give_up_qp(in_force);
                } else {
                    // a step down that did not land waits for the next costly turn; until then
                    // the QP reported is the one in the stream
                    self.revert_to(in_force);
                    let next = self.costly_at.map_or(0, |t| t + COSTLY_MIN_GAP_NS);
                    if let Some(g) = self.link.as_mut() {
                        g.defer_steps(next);
                    }
                }
                None
            }
        }
    }

    /// What the costly path for `q` did. `applied`: in effect, and checked like any move.
    pub(crate) fn costly_result(&mut self, q: u8, applied: bool, in_force: u8) {
        if applied {
            self.costly += 1;
            self.check.raised(in_force, q, true);
        } else {
            self.give_up_qp(in_force);
        }
    }

    fn revert_to(&mut self, in_force: u8) {
        let q = in_force.max(self.q_user);
        match (self.link.as_mut(), self.ceiling.as_mut()) {
            (Some(l), c) => {
                l.q = q;
                if let Some(c) = c {
                    c.q = c.q.min(q);
                }
            }
            (None, Some(c)) => c.q = q,
            (None, None) => {}
        }
        self.shown.0 = self.q();
    }

    pub(crate) fn give_up_qp(&mut self, in_force: u8) {
        if self.qp_dead {
            return;
        }
        self.qp_dead = true;
        self.ceiling = None;
        if let Some(g) = self.link.as_mut() {
            g.pin_qp(in_force.max(self.q_user));
        }
        self.pending = Some(format!(
            "the encoder does not take a QP change (the stream carries QP {in_force}): only the \
             frame rate answers the link from now on"
        ));
    }

    pub(crate) fn feedback(&mut self, now_ns: u64, s: &PathSample) {
        if let Some(g) = self.link.as_mut() {
            g.feedback(now_ns, s);
        }
    }

    /// Advance both. Returns why, when the QP or the divisor moved since the last answer.
    pub(crate) fn tick(&mut self, now_ns: u64) -> Option<String> {
        let mut why = self.pending.take();
        if let Some(g) = self.ceiling.as_mut() {
            if let Some((_, w)) = g.observe(now_ns, self.total_bytes) {
                why = Some(w);
            }
        }
        let q_eff = self.q();
        if let Some(g) = self.link.as_mut() {
            if let Some(m) = g.tick(now_ns, q_eff) {
                why = Some(m.why);
            }
        }
        let now = (self.q(), self.divisor());
        if now == self.shown {
            return None;
        }
        if now.0 != self.shown.0 && !self.qp_dead {
            self.check.raised(self.shown.0, now.0, false);
        }
        self.shown = now;
        Some(why.unwrap_or_default())
    }
}

/// What the page says about its own pace: the v4 tail of its stats frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientPace {
    /// When the host received the report.
    pub received_ns: u64,
    /// `video_late_state`: 0 in step, 1 skipping stale paints, 2 behind with skipping no help,
    /// 3 waiting for a catch-up keyframe.
    pub state: u8,
    /// Cumulative: decoded frames left unpainted because a newer one was ready.
    pub paints_skipped: u64,
    /// Cumulative: decoder resets to a keyframe because it fell behind.
    pub catch_ups: u64,
}

/// Where the newest [`ClientPace`] comes from.
pub(crate) trait ClientPaceSource {
    fn client_pace(&self) -> Option<ClientPace>;
}

impl ClientPaceSource for SessionTelemetry {
    fn client_pace(&self) -> Option<ClientPace> {
        let c = self.client.get()?;
        (c.version >= 4).then_some(ClientPace {
            received_ns: c.received_host_ns,
            state: c.video_late_state,
            paints_skipped: c.video_frames_stale as u64,
            catch_ups: c.video_catch_ups as u64,
        })
    }
}

/// Behind in this many consecutive reports before the rate is halved.
const PACE_BEHIND_REPORTS: u32 = 2;
/// Paints skipped since the previous report that count as behind on their own.
const PACE_SKIPS: u64 = 3;
/// After a cut, the next one waits this long (the client needs time to show it caught up).
const PACE_RAISE_HOLD_NS: u64 = 3_000 * MS;
/// In step this long before the rate is doubled again; doubled after a restore that failed.
const PACE_RESTORE_NS: u64 = 10_000 * MS;
const PACE_RESTORE_MAX_NS: u64 = 60_000 * MS;
/// A restore followed by falling behind within this long failed.
const PACE_FAIL_NS: u64 = 10_000 * MS;
/// Reports older than this are not about now.
const PACE_STALE_NS: u64 = 3_000 * MS;

/// Halves the frame rate for a client that keeps skipping paints or catching up (120 -> 60 ->
/// 30), and restores it once the client has been in step for a while. Separate from the link:
/// it never touches the QP, because neither QP nor bitrate makes a slow decoder faster, and a
/// keyframe request from a client waiting for one (state 3) is neither loss nor slowness.
pub(crate) struct ClientPaceGovernor {
    div: u32,
    last: Option<ClientPace>,
    behind_run: u32,
    in_step_since: Option<u64>,
    hold_until_ns: u64,
    restore_ns: u64,
    restored_at: Option<u64>,
    pub(crate) cuts: u64,
    pub(crate) restores: u64,
}

impl Default for ClientPaceGovernor {
    fn default() -> Self {
        ClientPaceGovernor {
            div: 1,
            last: None,
            behind_run: 0,
            in_step_since: None,
            hold_until_ns: 0,
            restore_ns: PACE_RESTORE_NS,
            restored_at: None,
            cuts: 0,
            restores: 0,
        }
    }
}

impl ClientPaceGovernor {
    pub(crate) fn divisor(&self) -> u32 {
        self.div
    }

    /// The newest report, at `now_ns`. Only a report not seen before counts. `Some(why)` when the
    /// divisor moved.
    pub(crate) fn observe(&mut self, now_ns: u64, p: Option<ClientPace>) -> Option<String> {
        let p = p?;
        if now_ns.saturating_sub(p.received_ns) > PACE_STALE_NS {
            return None;
        }
        let prev = self.last.replace(p);
        let prev = match prev {
            Some(prev) if prev.received_ns == p.received_ns => return None,
            Some(prev) => prev,
            // the first report only sets the baseline for the counters
            None => return None,
        };
        let skipped = p.paints_skipped.saturating_sub(prev.paints_skipped);
        let caught_up = p.catch_ups.saturating_sub(prev.catch_ups);
        match p.state {
            // waiting for a catch-up keyframe: says nothing about pace either way, and the
            // keyframe it asked for is not evidence of loss
            3 => None,
            s if s == 1 || s == 2 || skipped >= PACE_SKIPS || caught_up > 0 => {
                self.in_step_since = None;
                self.behind_run += 1;
                if self.behind_run < PACE_BEHIND_REPORTS
                    || now_ns < self.hold_until_ns
                    || self.div >= MAX_DIVISOR
                {
                    return None;
                }
                if self
                    .restored_at
                    .take()
                    .is_some_and(|t| now_ns.saturating_sub(t) < PACE_FAIL_NS)
                {
                    self.restore_ns = (self.restore_ns * 2).min(PACE_RESTORE_MAX_NS);
                }
                let from = self.div;
                self.div = (self.div * 2).min(MAX_DIVISOR);
                self.behind_run = 0;
                self.hold_until_ns = now_ns + PACE_RAISE_HOLD_NS;
                self.cuts += 1;
                Some(format!(
                    "the client cannot keep up (state {s}, {skipped} paints skipped, {caught_up} \
                     catch-ups since its last report): frame rate 1/{from} -> 1/{}",
                    self.div
                ))
            }
            _ => {
                self.behind_run = 0;
                let since = *self.in_step_since.get_or_insert(now_ns);
                if self.div == 1 || now_ns.saturating_sub(since) < self.restore_ns {
                    return None;
                }
                let from = self.div;
                self.div /= 2;
                self.in_step_since = Some(now_ns);
                self.restored_at = Some(now_ns);
                self.restores += 1;
                Some(format!(
                    "the client has been in step for {} s: frame rate 1/{from} -> 1/{}",
                    self.restore_ns / 1_000_000_000,
                    self.div
                ))
            }
        }
    }
}

// the QP the bitstream actually carries

#[derive(Clone, Copy, Debug, Default)]
struct SpsLite {
    separate_colour_plane: bool,
    log2_max_frame_num: u32,
    poc_type: u32,
    log2_max_poc_lsb: u32,
    delta_pic_order_always_zero: bool,
    frame_mbs_only: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct PpsLite {
    sps_id: u32,
    cabac: bool,
    bottom_field_pic_order: bool,
    weighted_pred: bool,
    weighted_bipred: u32,
    pic_init_qp: i32,
    redundant_pic_cnt: bool,
}

/// Exp-Golomb reader over a NAL payload that drops emulation-prevention bytes as it goes.
struct Bits<'a> {
    d: &'a [u8],
    i: usize,
    cur: u8,
    left: u8,
    zeros: u8,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bits {
            d,
            i: 0,
            cur: 0,
            left: 0,
            zeros: 0,
        }
    }

    fn bit(&mut self) -> Option<u32> {
        if self.left == 0 {
            let mut b = *self.d.get(self.i)?;
            self.i += 1;
            if self.zeros >= 2 && b == 3 {
                b = *self.d.get(self.i)?;
                self.i += 1;
            }
            self.zeros = if b == 0 { (self.zeros + 1).min(2) } else { 0 };
            self.cur = b;
            self.left = 8;
        }
        self.left -= 1;
        Some(((self.cur >> self.left) & 1) as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut z = 0;
        while self.bit()? == 0 {
            z += 1;
            if z > 31 {
                return None;
            }
        }
        Some(((1u64 << z) - 1 + self.bits(z)? as u64) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

/// Reads the slice QP of an access unit's first slice, remembering the SPS and PPS it needs from
/// whatever access unit carried them. Only the bytes up to the first slice header are touched.
#[derive(Default)]
pub(crate) struct SliceQp {
    sps: Vec<(u32, SpsLite)>,
    pps: Vec<(u32, PpsLite)>,
}

/// The index just past the next `00 00 01` at or after `from`.
fn find_start(au: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            return Some(i + 3);
        }
        i += 1;
    }
    None
}

impl SliceQp {
    /// Whether an SPS and a PPS have been seen, so a slice can be read.
    pub(crate) fn ready(&self) -> bool {
        !self.sps.is_empty() && !self.pps.is_empty()
    }

    /// What one access unit says. `(slice QP, is IDR)`: the QP is `None` when the AU has no
    /// slice this can read (no SPS/PPS seen yet, a cut-off header, or a feature none of these
    /// encoders uses). The first slice is read from its start and never scanned to its end.
    pub(crate) fn read(&mut self, au: &[u8], want_qp: bool) -> (Option<u8>, bool) {
        let mut pos = find_start(au, 0);
        while let Some(start) = pos {
            let Some(&h) = au.get(start) else { break };
            let ty = h & 0x1f;
            if ty == 1 || ty == 5 {
                let qp = if want_qp {
                    self.slice_qp(&au[start + 1..], ty == 5, (h >> 5) & 3)
                } else {
                    None
                };
                return (qp, ty == 5);
            }
            let next = find_start(au, start);
            // a four-byte start code leaves its leading zero on the previous unit
            let mut end = next.map_or(au.len(), |n| n - 3);
            while end > start + 1 && au[end - 1] == 0 && next.is_some() {
                end -= 1;
            }
            let body = au.get(start + 1..end).unwrap_or(&[]);
            match ty {
                7 => {
                    if let Some((id, sps)) = parse_sps(body) {
                        upsert(&mut self.sps, id, sps);
                    }
                }
                8 => {
                    if let Some((id, pps)) = parse_pps(body) {
                        upsert(&mut self.pps, id, pps);
                    }
                }
                _ => {}
            }
            pos = next;
        }
        (None, false)
    }

    fn slice_qp(&self, body: &[u8], idr: bool, nal_ref_idc: u8) -> Option<u8> {
        let mut r = Bits::new(body);
        r.ue()?; // first_mb_in_slice
        let slice_type = r.ue()? % 5;
        let pps = lookup(&self.pps, r.ue()?)?;
        let sps = lookup(&self.sps, pps.sps_id)?;
        let (p, b, i) = (
            slice_type == 0 || slice_type == 3,
            slice_type == 1,
            slice_type == 2,
        );
        if sps.separate_colour_plane {
            r.bits(2)?;
        }
        r.bits(sps.log2_max_frame_num)?;
        let mut field = false;
        if !sps.frame_mbs_only {
            field = r.flag()?;
            if field {
                r.bit()?;
            }
        }
        if idr {
            r.ue()?;
        }
        if sps.poc_type == 0 {
            r.bits(sps.log2_max_poc_lsb)?;
            if pps.bottom_field_pic_order && !field {
                r.se()?;
            }
        }
        if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
            r.se()?;
            if pps.bottom_field_pic_order && !field {
                r.se()?;
            }
        }
        if pps.redundant_pic_cnt {
            r.ue()?;
        }
        if b {
            r.bit()?;
        }
        // num_ref_idx_active_override: the counts do not change what follows up to the QP
        if (p || b) && r.flag()? {
            r.ue()?;
            if b {
                r.ue()?;
            }
        }
        for list in 0..2 {
            let applies = (list == 0 && !i && slice_type != 4) || (list == 1 && b);
            if applies && r.flag()? {
                loop {
                    match r.ue()? {
                        3 => break,
                        0..=2 => {
                            r.ue()?;
                        }
                        _ => return None,
                    }
                }
            }
        }
        if (pps.weighted_pred && p) || (pps.weighted_bipred == 1 && b) {
            return None;
        }
        if nal_ref_idc != 0 {
            if idr {
                r.bits(2)?;
            } else if r.flag()? {
                loop {
                    match r.ue()? {
                        0 => break,
                        1 | 2 | 4 | 6 => {
                            r.ue()?;
                        }
                        3 => {
                            r.ue()?;
                            r.ue()?;
                        }
                        5 => {}
                        _ => return None,
                    }
                }
            }
        }
        if pps.cabac && !i && slice_type != 4 {
            r.ue()?;
        }
        let qp = pps.pic_init_qp + r.se()?;
        (0..=51).contains(&qp).then_some(qp as u8)
    }
}

fn upsert<T>(v: &mut Vec<(u32, T)>, id: u32, x: T) {
    match v.iter_mut().find(|(i, _)| *i == id) {
        Some(slot) => slot.1 = x,
        None => v.push((id, x)),
    }
}

fn lookup<T: Copy>(v: &[(u32, T)], id: u32) -> Option<T> {
    v.iter().find(|(i, _)| *i == id).map(|(_, x)| *x)
}

fn parse_sps(body: &[u8]) -> Option<(u32, SpsLite)> {
    let mut r = Bits::new(body);
    let profile = r.bits(8)?;
    r.bits(16)?;
    let id = r.ue()?;
    let mut s = SpsLite::default();
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma = r.ue()?;
        if chroma == 3 {
            s.separate_colour_plane = r.flag()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.flag()? {
            for k in 0..if chroma == 3 { 12 } else { 8 } {
                if r.flag()? {
                    let size = if k < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    s.log2_max_frame_num = r.ue()? + 4;
    s.poc_type = r.ue()?;
    if s.poc_type == 0 {
        s.log2_max_poc_lsb = r.ue()? + 4;
    } else if s.poc_type == 1 {
        s.delta_pic_order_always_zero = r.flag()?;
        r.se()?;
        r.se()?;
        for _ in 0..r.ue()? {
            r.se()?;
        }
    }
    r.ue()?;
    r.bit()?;
    r.ue()?;
    r.ue()?;
    s.frame_mbs_only = r.flag()?;
    Some((id, s))
}

fn parse_pps(body: &[u8]) -> Option<(u32, PpsLite)> {
    let mut r = Bits::new(body);
    let id = r.ue()?;
    let mut p = PpsLite {
        sps_id: r.ue()?,
        cabac: r.flag()?,
        bottom_field_pic_order: r.flag()?,
        ..Default::default()
    };
    if r.ue()? != 0 {
        return None; // slice groups
    }
    r.ue()?;
    r.ue()?;
    p.weighted_pred = r.flag()?;
    p.weighted_bipred = r.bits(2)?;
    p.pic_init_qp = 26 + r.se()?;
    r.se()?;
    r.se()?;
    r.bit()?;
    r.bit()?;
    p.redundant_pic_cnt = r.flag()?;
    Some((id, p))
}

/// What the transport, the encode thread and the capture callback share. Lives in the session
/// telemetry, so every party already holds it.
#[derive(Default)]
pub struct LinkShared {
    /// Set once a governor with a link side is running; until then the transport queues nothing.
    listening: AtomicBool,
    feedback: Mutex<VecDeque<(u64, PathSample)>>,
    /// The frame-rate divisor the capture gate applies; 0 and 1 are the full rate.
    divisor: AtomicU32,
    /// The capture path can hold a gated frame and deliver it later (the ring's trailing edge).
    /// Without that the frame rate is never cut.
    frame_hold: AtomicBool,
    /// Bumped by the transport when ICE selects a new candidate pair.
    path_changes: AtomicU64,
    /// Which `QpControl` is current: a capture can be opened again on the same telemetry, and an
    /// older one dropped late must not silence the newer.
    generation: AtomicU64,
    user_qp: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
    fps: AtomicU32,
    // what `status --json` shows
    running: AtomicBool,
    link_on: AtomicBool,
    ceiling_on: AtomicBool,
    qp_unsupported: AtomicBool,
    qp_effective: AtomicU32,
    qp_link: AtomicU32,
    qp_max: AtomicU32,
    raises: AtomicU64,
    steps: AtomicU64,
    rebases: AtomicU64,
    above_ms: AtomicU64,
    halving_milli: AtomicU64,
    queue_us: AtomicI64,
    delivered_bps: AtomicU64,
    sent_bps: AtomicU64,
    share_milli: AtomicU64,
    sigma_milli: AtomicU64,
    loss_milli: AtomicU64,
    verified: AtomicU64,
    mismatches: AtomicU64,
    unread: AtomicU64,
    last_read: AtomicU32,
    set_qp_errors: AtomicU64,
    client_divisor: AtomicU32,
    client_cuts: AtomicU64,
    qp_change_unconfirmed: AtomicU64,
    qp_change_confirmed: AtomicU64,
    costly_applied: AtomicU64,
    qp_ineffective: AtomicBool,
    last_move: Slot<String>,
    state: Slot<String>,
}

const FEEDBACK_QUEUE: usize = 64;

impl LinkShared {
    /// The transport's half: one report's figures. A no-op until a governor listens.
    pub fn on_feedback(&self, s: &PathSample, now_ns: u64) {
        if !self.listening.load(Ordering::Relaxed) || s.reported == 0 {
            return;
        }
        let mut q = self.feedback.lock_recover();
        if q.len() >= FEEDBACK_QUEUE {
            q.pop_front();
        }
        q.push_back((now_ns, *s));
    }

    /// The transport's other half: ICE moved the session to another candidate pair.
    pub fn path_changed(&self) {
        self.path_changes.fetch_add(1, Ordering::Relaxed);
    }

    /// The capture gate's half.
    pub fn frame_divisor(&self) -> u32 {
        self.divisor.load(Ordering::Relaxed).clamp(1, MAX_DIVISOR)
    }

    pub fn to_json(&self) -> Option<serde_json::Value> {
        if !self.running.load(Ordering::Relaxed) {
            let state = self.state.get();
            return (!state.is_empty()).then(|| serde_json::json!({ "state": state }));
        }
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Some(serde_json::json!({
            "state": self.state.get(),
            "link": self.link_on.load(Ordering::Relaxed),
            "ceiling": self.ceiling_on.load(Ordering::Relaxed),
            "qp_unsupported": self.qp_unsupported.load(Ordering::Relaxed),
            "frame_hold": self.frame_hold.load(Ordering::Relaxed),
            "qp_user": self.user_qp.load(Ordering::Relaxed),
            "qp_effective": self.qp_effective.load(Ordering::Relaxed),
            "qp_link": self.qp_link.load(Ordering::Relaxed),
            "qp_max": self.qp_max.load(Ordering::Relaxed),
            "frame_divisor": self.frame_divisor(),
            "client_frame_divisor": self.client_divisor.load(Ordering::Relaxed).max(1),
            "client_cuts": r(&self.client_cuts),
            "raises": r(&self.raises),
            "steps": r(&self.steps),
            "path_rebases": r(&self.rebases),
            "halving_qp": r(&self.halving_milli) as f64 / 1e3,
            "ms_above_user_qp": r(&self.above_ms),
            "last_move": self.last_move.get(),
            "evidence": {
                "queue_ms": self.queue_us.load(Ordering::Relaxed) as f64 / 1e3,
                "delivered_mbps": r(&self.delivered_bps) as f64 / 1e6,
                "sent_mbps": r(&self.sent_bps) as f64 / 1e6,
                "byte_share": r(&self.share_milli) as f64 / 1e3,
                "arrival_stretch": r(&self.sigma_milli) as f64 / 1e3,
                "loss": r(&self.loss_milli) as f64 / 1e3,
            },
            "qp_readback": {
                "verified": r(&self.verified),
                "mismatches": r(&self.mismatches),
                "unread": r(&self.unread),
                "last": self.last_read.load(Ordering::Relaxed),
            },
            "set_qp_errors": r(&self.set_qp_errors),
            "qp_change_confirmed": r(&self.qp_change_confirmed),
            "qp_change_unconfirmed": r(&self.qp_change_unconfirmed),
            "costly_qp_changes": r(&self.costly_applied),
            "qp_ineffective": self.qp_ineffective.load(Ordering::Relaxed),
        }))
    }
}

/// Record the session's encoder settings where the encode thread will find them. Called where
/// a capture is opened, before its encode path is built.
pub(crate) fn arm(telemetry: &SessionTelemetry, config: &EncoderConfig) {
    let l = &telemetry.link;
    l.user_qp.store(
        config.qp.map_or(0, |q| q.clamp(1, 51) as u32),
        Ordering::Relaxed,
    );
    l.width.store(config.width, Ordering::Relaxed);
    l.height.store(config.height, Ordering::Relaxed);
    l.fps.store(config.fps, Ordering::Relaxed);
    l.frame_hold.store(false, Ordering::Relaxed);
}

/// What the capture answers to "can I deliver a gated frame later": asked on every frame it
/// gates, stored only when the answer changes.
pub(crate) fn set_frame_hold(telemetry: &SessionTelemetry, yes: bool) {
    let hold = &telemetry.link.frame_hold;
    if hold.load(Ordering::Relaxed) != yes {
        hold.store(yes, Ordering::Relaxed);
    }
}

/// The frame-rate divisor for the capture gate: 1 unless a governor gated it.
pub(crate) fn frame_divisor(telemetry: &SessionTelemetry) -> u32 {
    telemetry.link.frame_divisor()
}

/// What the governor needs from an encoder.
pub(crate) trait QpEncoder {
    /// Change the constant QP from the next frame without a keyframe. `Ok(false)`: cannot.
    fn set_qp(&mut self, qp: u8) -> Result<bool>;
    /// The backend's slower way to the same QP (a re-init, a keyframe), for when a `set_qp`
    /// that answered `Ok(true)` did not reach the stream. `Ok(false)`: there is none.
    fn set_qp_costly(&mut self, _qp: u8) -> Result<bool> {
        Ok(false)
    }
}

/// Verify the first frames after every change, then one in this many.
const VERIFY_EVERY: u32 = 120;
const VERIFY_AFTER_CHANGE: u32 = 2;
/// Enough of an access unit for its SPS, PPS and first slice header.
const PREFIX_BYTES: usize = 1024;
const PENDING_KEEP: usize = 16;

/// An access unit as it left, recorded after the send and looked at before the next submit.
struct Emitted {
    at_ns: u64,
    len: u64,
    repeat: bool,
    keyframe: bool,
    prefix: Option<Vec<u8>>,
}

/// The encode thread's part. Built once per encode path; `None` when the session is not at a
/// constant QP or every switch is off, and then nothing on the path changes.
pub(crate) struct QpControl {
    shared: std::sync::Arc<SessionTelemetry>,
    generation: u64,
    gov: Governors,
    q_user: u8,
    applied: u8,
    off: bool,
    /// The encoder answered `Ok(false)`: no QP change at all, the frame rate may still answer.
    qp_unsupported: bool,
    link: bool,
    ceiling_bps: Option<u64>,
    fps: u32,
    errors: u32,
    reader: SliceQp,
    verify_left: u32,
    since_verify: u32,
    drained: Vec<(u64, PathSample)>,
    pending: Vec<Emitted>,
    spare: Vec<Vec<u8>>,
    /// A raise that did not reach the stream: `(QP wanted, QP in force)`, for the costly path.
    costly: Option<(u8, u8)>,
    pace: Option<ClientPaceGovernor>,
    pace_polled_ns: u64,
    path_changes_seen: u64,
    /// The next submit is a keyframe; then, the last submit was.
    keyframe_next: bool,
    last_submit_key: bool,
}

impl QpControl {
    pub(crate) fn from_telemetry(telemetry: &std::sync::Arc<SessionTelemetry>) -> Option<Self> {
        Self::build(
            telemetry,
            super::tune::candidate("link_governor"),
            super::tune::candidate("qp_ceiling_governor"),
            super::tune::candidate("client_pace_governor"),
        )
    }

    pub(crate) fn build(
        telemetry: &std::sync::Arc<SessionTelemetry>,
        link: bool,
        ceiling: bool,
        pace: bool,
    ) -> Option<Self> {
        let l = &telemetry.link;
        let q = l.user_qp.load(Ordering::Relaxed);
        if q == 0 || !(link || ceiling || pace) {
            return None;
        }
        let q = q as u8;
        let (w, h, fps) = (
            l.width.load(Ordering::Relaxed),
            l.height.load(Ordering::Relaxed),
            l.fps.load(Ordering::Relaxed).max(1),
        );
        let ceiling_bps = match qp_ceiling_override() {
            Some(o) => o,
            None => Some(qp_ceiling_bps(w, h, fps)),
        }
        .filter(|_| ceiling);
        let generation = l.generation.fetch_add(1, Ordering::Relaxed) + 1;
        telemetry
            .qp_ceiling_bps
            .store(ceiling_bps.unwrap_or(0), Ordering::Relaxed);
        telemetry.qp_user.store(q as u64, Ordering::Relaxed);
        telemetry.qp_effective.store(q as u64, Ordering::Relaxed);
        l.link_on.store(link, Ordering::Relaxed);
        l.ceiling_on.store(ceiling_bps.is_some(), Ordering::Relaxed);
        l.qp_unsupported.store(false, Ordering::Relaxed);
        l.qp_effective.store(q as u32, Ordering::Relaxed);
        l.qp_link.store(q as u32, Ordering::Relaxed);
        l.qp_max.store(QP_LINK_MAX.max(q) as u32, Ordering::Relaxed);
        l.divisor.store(1, Ordering::Relaxed);
        l.state.set("armed".into());
        l.running.store(true, Ordering::Relaxed);
        l.listening.store(link, Ordering::Relaxed);
        tprintln!(
            "QP governor armed: QP {q}, link side {}, ceiling {}, client pace {}",
            if link { "on" } else { "off" },
            ceiling_bps.map_or("off".to_string(), |c| format!(
                "{:.1} Mbps over {} ms",
                c as f64 / 1e6,
                QP_GOV_WINDOW_NS / MS
            )),
            if pace { "on" } else { "off" },
        );
        Some(QpControl {
            shared: std::sync::Arc::clone(telemetry),
            generation,
            gov: Governors::new(q, ceiling_bps, fps, link),
            q_user: q,
            applied: q,
            off: false,
            qp_unsupported: false,
            link,
            ceiling_bps,
            fps,
            errors: 0,
            reader: SliceQp::default(),
            verify_left: VERIFY_AFTER_CHANGE,
            since_verify: 0,
            drained: Vec::with_capacity(FEEDBACK_QUEUE),
            pending: Vec::with_capacity(PENDING_KEEP),
            spare: Vec::new(),
            costly: None,
            pace: pace.then(ClientPaceGovernor::default),
            pace_polled_ns: 0,
            path_changes_seen: l.path_changes.load(Ordering::Relaxed),
            keyframe_next: false,
            last_submit_key: false,
        })
    }

    /// The backend was rebuilt under this path: whatever the old encoder could not do, the new
    /// one is asked again.
    pub(crate) fn encoder_replaced(&mut self) {
        if self.qp_unsupported {
            self.qp_unsupported = false;
            self.gov = Governors::new(self.q_user, self.ceiling_bps, self.fps, self.link);
            self.shared
                .link
                .qp_unsupported
                .store(false, Ordering::Relaxed);
        }
        self.applied = self.q_user;
    }

    /// Before every submit, frames and repeats alike: look at what left since the last one, take
    /// in the feedback that arrived, decide, and put the QP in force. `set_qp` with the QP
    /// already in force costs the encoder a compare.
    pub(crate) fn before_submit(&mut self, enc: &mut dyn QpEncoder) {
        if self.off {
            return;
        }
        self.take_pending();
        if let Some((q, in_force)) = self.costly.take() {
            let applied = match enc.set_qp_costly(q) {
                Ok(a) => a,
                Err(e) => {
                    teprintln!("QP governor: the slower QP path for {q} failed: {e:#}");
                    false
                }
            };
            self.gov.costly_result(q, applied, in_force);
            let l = &self.shared.link;
            l.costly_applied.store(self.gov.costly, Ordering::Relaxed);
            l.qp_ineffective.store(self.gov.qp_dead, Ordering::Relaxed);
            if applied {
                self.applied = q;
                tprintln!("QP governor: QP {q} applied through the encoder's slower path");
            }
        }
        let now = crate::streamer::audio::host_now_ns();
        let l = &self.shared.link;
        let hold = l.frame_hold.load(Ordering::Relaxed);
        self.gov.hold_frames(hold);
        let changes = l.path_changes.load(Ordering::Relaxed);
        if changes != self.path_changes_seen {
            self.path_changes_seen = changes;
            self.gov.path_changed();
        }
        if self.gov.link.is_some() {
            if let Ok(mut q) = l.feedback.try_lock() {
                self.drained.extend(q.drain(..));
            }
            for (t, s) in self.drained.drain(..) {
                self.gov.feedback(t, &s);
            }
        }
        let was_engaged = self.gov.engaged();
        if let Some(why) = self.gov.tick(now) {
            let what = if !self.gov.engaged() {
                "released"
            } else if !was_engaged {
                "engaged"
            } else {
                "moved"
            };
            tprintln!("QP governor {what}: {why}");
            l.state.set(what.into());
            l.last_move.set(why);
            self.publish_moves();
        }
        // the client's own pace, about once a STATS frame: its divisor never touches the QP, and
        // a path that cannot hold a gated frame does not cut the frame rate for it either
        if let Some(pace) = self.pace.as_mut().filter(|_| hold) {
            if now.saturating_sub(self.pace_polled_ns) >= 250 * MS {
                self.pace_polled_ns = now;
                if let Some(why) = pace.observe(now, self.shared.client_pace()) {
                    tprintln!("QP governor: {why}");
                    l.client_divisor.store(pace.divisor(), Ordering::Relaxed);
                    l.client_cuts.store(pace.cuts, Ordering::Relaxed);
                    l.last_move.set(why);
                }
            }
        }
        let div = if hold {
            self.gov
                .divisor()
                .max(self.pace.as_ref().map_or(1, |p| p.divisor()))
        } else {
            1
        };
        if l.divisor.load(Ordering::Relaxed) != div {
            l.divisor.store(div, Ordering::Relaxed);
        }
        if let Some(g) = self.gov.link.as_ref() {
            let w = g.last;
            l.queue_us.store(w.queue_us, Ordering::Relaxed);
            l.delivered_bps
                .store(w.delivered_bps as u64, Ordering::Relaxed);
            l.sent_bps.store(w.sent_bps as u64, Ordering::Relaxed);
            l.share_milli
                .store((w.share * 1e3) as u64, Ordering::Relaxed);
            l.sigma_milli
                .store((w.sigma * 1e3) as u64, Ordering::Relaxed);
            l.loss_milli.store((w.loss * 1e3) as u64, Ordering::Relaxed);
            l.rebases.store(g.rebases, Ordering::Relaxed);
            l.halving_milli
                .store((g.halving_qp() * 1e3) as u64, Ordering::Relaxed);
        }
        let key = std::mem::take(&mut self.keyframe_next);
        self.last_submit_key = key;
        if self.qp_unsupported {
            self.publish_above();
            return;
        }
        let mut q = self.gov.q();
        if key && self.gov.engaged() && !self.gov.qp_dead {
            q = q.saturating_add(KEYFRAME_BOOST).min(QP_LAST_RESORT);
        }
        match enc.set_qp(q) {
            Ok(true) => {
                if q != self.applied {
                    self.applied = q;
                    self.verify_left = VERIFY_AFTER_CHANGE;
                    self.shared.qp_effective.store(q as u64, Ordering::Relaxed);
                    l.qp_effective.store(q as u32, Ordering::Relaxed);
                }
            }
            Ok(false) => {
                self.qp_unsupported = true;
                self.gov.give_up_qp(self.q_user);
                l.qp_unsupported.store(true, Ordering::Relaxed);
                l.state
                    .set("this encoder cannot change its QP per frame".into());
                teprintln!(
                    "QP governor: the encoder cannot change its constant QP per frame; only the \
                     frame rate answers a slow link on this path"
                );
            }
            Err(e) => {
                self.errors += 1;
                l.set_qp_errors.fetch_add(1, Ordering::Relaxed);
                if self.errors.is_power_of_two() {
                    teprintln!(
                        "QP governor: set_qp({q}) failed ({} so far): {e:#}",
                        self.errors
                    );
                }
            }
        }
        self.publish_above();
    }

    fn publish_above(&self) {
        let above = self
            .gov
            .ceiling
            .as_ref()
            .map_or(0, |g| g.above_ns)
            .max(self.gov.link.as_ref().map_or(0, |g| g.above_ns));
        self.shared
            .qp_above_user_ms
            .store(above / MS, Ordering::Relaxed);
        self.shared
            .link
            .above_ms
            .store(above / MS, Ordering::Relaxed);
    }

    fn publish_moves(&self) {
        let l = &self.shared.link;
        if let Some(g) = self.gov.ceiling.as_ref() {
            self.shared
                .qp_ceiling_steps
                .store(g.steps, Ordering::Relaxed);
        }
        if let Some(g) = self.gov.link.as_ref() {
            l.qp_link.store(g.q() as u32, Ordering::Relaxed);
            l.raises.store(g.raises, Ordering::Relaxed);
            l.steps.store(g.steps, Ordering::Relaxed);
        }
    }

    /// Say before the submit that it will be a keyframe: while the governor is engaged it is
    /// encoded `KEYFRAME_BOOST` coarser, and the frame after it goes back.
    pub(crate) fn keyframe_next(&mut self, keyframe: bool) {
        self.keyframe_next = keyframe;
    }

    /// After an access unit has been handed to the transport. Records it and nothing more: the
    /// first bytes are copied only when a read is due (a keyframe, a move being checked, a
    /// periodic check), and everything else waits for the next submit.
    pub(crate) fn emitted(&mut self, au: &[u8], repeat: bool) {
        if self.off || au.is_empty() {
            return;
        }
        if self.pending.len() >= PENDING_KEEP {
            // nothing has been submitted for a while; the oldest records say nothing new
            let old = self.pending.remove(0);
            if let Some(p) = old.prefix {
                self.spare.push(p);
            }
        }
        self.since_verify += 1;
        let key = self.last_submit_key;
        let want = key
            || self.verify_left > 0
            || self.since_verify >= VERIFY_EVERY
            || self.gov.checking()
            || !self.reader.ready();
        let prefix = want.then(|| {
            let mut v = self.spare.pop().unwrap_or_default();
            v.clear();
            v.extend_from_slice(&au[..au.len().min(PREFIX_BYTES)]);
            v
        });
        self.pending.push(Emitted {
            at_ns: crate::streamer::audio::host_now_ns(),
            len: au.len() as u64,
            repeat,
            keyframe: key,
            prefix,
        });
    }

    fn take_pending(&mut self) {
        let mut pending = std::mem::take(&mut self.pending);
        for e in pending.drain(..) {
            self.look(e);
        }
        self.pending = pending;
    }

    fn look(&mut self, e: Emitted) {
        let (qp, idr) = match e.prefix.as_deref() {
            Some(p) => self.reader.read(p, true),
            None => (None, false),
        };
        let keyframe = e.keyframe || idr;
        let l = &self.shared.link;
        let unconfirmed = self.gov.unconfirmed;
        if let Some(c) = self.gov.emitted(e.at_ns, e.len, e.repeat, keyframe, qp) {
            self.costly = Some(c);
        }
        if self.gov.unconfirmed != unconfirmed {
            l.qp_change_unconfirmed
                .store(self.gov.unconfirmed, Ordering::Relaxed);
            l.qp_ineffective.store(self.gov.qp_dead, Ordering::Relaxed);
            if self.gov.unconfirmed.is_power_of_two() {
                teprintln!(
                    "QP governor: QP {} was set but the stream carries {} ({} such moves); {}",
                    self.applied,
                    qp.map_or("an unreadable QP".to_string(), |q| q.to_string()),
                    self.gov.unconfirmed,
                    if self.costly.is_some() {
                        "trying the encoder's slower path"
                    } else {
                        "only the frame rate is left"
                    }
                );
            }
        }
        l.qp_change_confirmed
            .store(self.gov.confirmed, Ordering::Relaxed);
        if let Some(p) = e.prefix {
            self.spare.push(p);
            // a keyframe carries the boosted QP, or on x264 its own I-frame offset: not a check
            if !keyframe {
                self.verify_left = self.verify_left.saturating_sub(1);
                self.since_verify = 0;
                match qp {
                    Some(q) => {
                        l.last_read.store(q as u32, Ordering::Relaxed);
                        if q == self.applied {
                            l.verified.fetch_add(1, Ordering::Relaxed);
                        } else {
                            let n = l.mismatches.fetch_add(1, Ordering::Relaxed) + 1;
                            if n.is_power_of_two() {
                                teprintln!(
                                    "QP governor: the bitstream carries QP {q} where {} was set \
                                     ({n} mismatches)",
                                    self.applied
                                );
                            }
                        }
                    }
                    None => {
                        l.unread.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

impl Drop for QpControl {
    fn drop(&mut self) {
        let l = &self.shared.link;
        if l.generation.load(Ordering::Relaxed) == self.generation {
            l.listening.store(false, Ordering::Relaxed);
            l.divisor.store(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // the ceiling: the macOS tests, against the port

    fn drive_ceiling(
        g: &mut CeilingGovernor,
        total: &mut u64,
        t0_ms: u64,
        secs: u64,
        fps: u64,
        bytes: &dyn Fn(u8, u64) -> u64,
    ) -> Vec<(u64, u8)> {
        let mut trace = Vec::new();
        for k in 0..secs * fps {
            let t = t0_ms * MS + k * 1_000_000_000 / fps;
            *total += bytes(g.q(), k);
            if let Some((q, _)) = g.observe(t, *total) {
                trace.push((t / MS, q));
            }
        }
        trace
    }

    #[test]
    fn the_ceiling_matches_the_macos_formula() {
        assert_eq!(qp_ceiling_bps(1920, 1080, 60), 49_766_400);
        assert_eq!(qp_ceiling_bps(800, 600, 60), 24_000_000);
        assert_eq!(qp_ceiling_bps(3840, 2160, 60), 160_000_000);
        assert_eq!(qp_ceiling_from(None), None);
        assert_eq!(qp_ceiling_from(Some("0")), Some(None));
        assert_eq!(qp_ceiling_from(Some("100")), Some(Some(100_000_000)));
        assert_eq!(qp_ceiling_from(Some("x")), None);
    }

    #[test]
    fn the_ceiling_never_touches_ordinary_content() {
        let mut g = CeilingGovernor::new(15, qp_ceiling_bps(1920, 1080, 60), 60);
        let mut total = 0;
        let bytes = |_q: u8, k: u64| -> u64 {
            if k.is_multiple_of(600) {
                2_000_000
            } else if (k / 60) % 10 == 3 {
                27_000_000 / 8 / 60
            } else {
                10_000_000 / 8 / 60
            }
        };
        let trace = drive_ceiling(&mut g, &mut total, 0, 60, 60, &bytes);
        assert!(trace.is_empty(), "{trace:?}");
    }

    #[test]
    fn noise_settles_under_the_ceiling_as_on_macos_and_comes_back() {
        let c = qp_ceiling_bps(800, 600, 60);
        let noise = |q: u8, _k: u64| -> u64 {
            (371e6 * 2f64.powf(-(q as f64 - 15.0) / 6.0) / 8.0 / 60.0) as u64
        };
        let mut g = CeilingGovernor::new(15, c, 60);
        let mut total = 0;
        let trace = drive_ceiling(&mut g, &mut total, 0, 30, 60, &noise);
        assert!(trace.first().expect("engaged").0 <= 1_000, "{trace:?}");
        let rate_at = |q: u8| 371e6 * 2f64.powf(-(q as f64 - 15.0) / 6.0);
        assert!(rate_at(g.q()) <= c as f64, "q {}", g.q());
        assert!(
            trace.len() <= 2 && trace.iter().all(|(t, _)| *t <= 1_500),
            "{trace:?}"
        );
        let calm = |q: u8, _k: u64| -> u64 {
            (10e6 * 2f64.powf(-(q as f64 - 15.0) / 6.0) / 8.0 / 60.0) as u64
        };
        let back = drive_ceiling(&mut g, &mut total, 30_000, 40, 60, &calm);
        assert_eq!(g.q(), 15, "{back:?}");
        assert!(back.windows(2).all(|w| w[1].1 + 1 == w[0].1), "{back:?}");
    }

    #[test]
    fn the_ceiling_fast_path_steps_on_three_big_frames_but_not_on_a_keyframe() {
        let mut g = CeilingGovernor::new(15, qp_ceiling_bps(1920, 1080, 60), 60);
        let t = |k: u64| k * 16_666_667;
        assert!(g.frame(t(0), 1_580_000).is_none());
        for k in 1..60 {
            assert!(g.frame(t(k), 20_000).is_none());
        }
        assert!(g.frame(t(62), 800_000).is_none());
        assert!(g.frame(t(63), 800_000).is_none());
        assert!(g.frame(t(64), 20_000).is_none());
        assert!(g.frame(t(65), 850_000).is_none());
        assert!(g.frame(t(66), 850_000).is_none());
        let (q, why) = g.frame(t(67), 850_000).expect("steps at the third frame");
        assert!(q > 15 && why.contains("at once"), "{q} {why}");
    }

    // the link, simulated. Content: `bytes(k)` at QP 15, scaled by 2^(-(q - 15) / 6) and by sqrt(d)
    // for a frame standing for d periods. Path, in the test relay's order: stall gate, Bernoulli
    // loss, delay with order-keeping jitter, FIFO drop-tail queue at the capped rate. Feedback: one
    // report per 50 ms, one propagation delay late, as `congestion::interpret` makes `PathSample`s

    const PKT: u64 = 1200;
    const FPS: u64 = 60;
    const PERIOD_NS: u64 = 1_000_000_000 / FPS;
    /// The client's arrival clock is unrelated to the host's.
    const CLOCK_OFFSET_US: i64 = 7_000_000_000;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[derive(Clone)]
    struct Path {
        /// Capacity at a time (ns), bits per second.
        cap: fn(u64) -> f64,
        /// Drop-tail queue: this much time at the capped rate (the relay's default is 100 ms).
        queue_s: f64,
        prop_us: i64,
        jitter_us: f64,
        loss: f64,
        /// A stall of `.1` ms every `.0` s.
        stall: Option<(u64, u64)>,
        /// From `.0` (ns) on, the path's one-way delay is `.1` us longer (a route change).
        delay_step: Option<(u64, i64)>,
        /// Each random loss takes this many more packets with it (a burst).
        loss_run: u32,
    }

    struct Pkt {
        send_us: i64,
        bytes: u64,
        arrival_us: Option<i64>,
    }

    /// How the simulated encoder answers `set_qp`.
    #[derive(Clone, Copy, PartialEq)]
    enum Model {
        Honest,
        /// Answers `Ok(true)` and keeps encoding at the QP it opened with; its costly path, when
        /// it has one, applies the QP with a keyframe.
        Ignores {
            costly_works: bool,
        },
    }

    struct Sim {
        /// QP steps that halve the bits: 6 by the textbook, ~16 for noise-dominated content
        /// (the real-encoder tests below measured x1.26 to x1.30 per six steps on such content).
        halving_qp: f64,
        model: Model,
        /// The QP the costly path put in force, for `Model::Ignores`.
        costly_q: Option<u8>,
        force_key: bool,
        /// When the costly path was taken (ms).
        costly_calls: Vec<u64>,
        path: Path,
        rng: Rng,
        pkts: Vec<Pkt>,
        busy_until_us: f64,
        last_arrival_us: i64,
        lose_next: u32,
        next_report_us: i64,
        reported_to: usize,
        reports: VecDeque<(u64, PathSample)>,
        /// (time ms, q, divisor) at every change.
        trace: Vec<(u64, u8, u32)>,
        /// Per frame: (send ms, queueing delay of its first packet in ms, None if it lost one).
        frames: Vec<(u64, Option<f64>)>,
    }

    impl Sim {
        fn new(path: Path, seed: u64) -> Self {
            Sim {
                halving_qp: 6.0,
                model: Model::Honest,
                costly_q: None,
                force_key: false,
                costly_calls: Vec::new(),
                path,
                rng: Rng(seed | 1),
                pkts: Vec::new(),
                busy_until_us: 0.0,
                last_arrival_us: 0,
                lose_next: 0,
                next_report_us: 50_000,
                reported_to: 0,
                reports: VecDeque::new(),
                trace: Vec::new(),
                frames: Vec::new(),
            }
        }

        fn send(&mut self, t_ns: u64, bytes: u64) {
            let n = bytes.div_ceil(PKT).max(1);
            let mut worst: Option<f64> = Some(0.0);
            let cap = (self.path.cap)(t_ns);
            for i in 0..n {
                let b = if i + 1 == n {
                    bytes - PKT * (n - 1)
                } else {
                    PKT
                }
                .max(40);
                let send_us = (t_ns / 1_000) as i64 + i as i64 * 10;
                let mut t = send_us as f64;
                if let Some((every_s, ms)) = self.path.stall {
                    let every = every_s as f64 * 1e6;
                    let phase = t % every;
                    if t >= every && phase < ms as f64 * 1e3 {
                        t += ms as f64 * 1e3 - phase;
                    }
                }
                let mut arrival = None;
                let prop = self.path.prop_us
                    + self
                        .path
                        .delay_step
                        .filter(|(at, _)| t_ns >= *at)
                        .map_or(0, |(_, us)| us);
                let lost = if self.lose_next > 0 {
                    self.lose_next -= 1;
                    true
                } else if self.rng.next() < self.path.loss {
                    self.lose_next = self.path.loss_run;
                    true
                } else {
                    false
                };
                if !lost {
                    let at = t + prop as f64 + self.rng.next() * self.path.jitter_us;
                    let backlog = (self.busy_until_us - at).max(0.0) / 1e6 * cap / 8.0;
                    let limit = (cap * self.path.queue_s / 8.0).max(3000.0);
                    if backlog + b as f64 <= limit {
                        let depart = self.busy_until_us.max(at) + b as f64 * 8.0 / cap * 1e6;
                        self.busy_until_us = depart;
                        let a = (depart as i64).max(self.last_arrival_us);
                        self.last_arrival_us = a;
                        arrival = Some(a);
                    }
                }
                match arrival {
                    // the first packet: how long the frame waited behind earlier traffic, not
                    // its own serialization (9 KB is 50 ms at 1.5 Mbps whatever the governor does)
                    Some(a) if i == 0 => {
                        let d = (a - send_us - prop) as f64 / 1e3;
                        worst = worst.map(|w| w.max(d));
                    }
                    Some(_) => {}
                    None => worst = None,
                }
                self.pkts.push(Pkt {
                    send_us,
                    bytes: b,
                    arrival_us: arrival,
                });
            }
            self.frames.push((t_ns / MS, worst));
        }

        /// Client reports due by `now_ns`, queued for the host.
        fn client_reports(&mut self, now_ns: u64) {
            let now_us = (now_ns / 1_000) as i64;
            while self.next_report_us + self.path.prop_us <= now_us {
                let r = self.next_report_us;
                self.next_report_us += 50_000;
                let mut last = None;
                for (i, p) in self.pkts.iter().enumerate().skip(self.reported_to) {
                    match p.arrival_us {
                        Some(a) if a <= r => last = Some(i),
                        Some(_) => break,
                        None if p.send_us > r => break,
                        None => {}
                    }
                }
                let Some(j) = last else { continue };
                let mut s = PathSample::default();
                for p in &self.pkts[self.reported_to..=j] {
                    s.reported += 1;
                    if s.known == 0 {
                        (s.first_send_us, s.last_send_us) = (p.send_us, p.send_us);
                    }
                    s.known += 1;
                    s.sent_bytes += p.bytes;
                    s.first_send_us = s.first_send_us.min(p.send_us);
                    s.last_send_us = s.last_send_us.max(p.send_us);
                    match p.arrival_us {
                        None => s.lost += 1,
                        Some(a) => {
                            let a = a + CLOCK_OFFSET_US;
                            let owd = a - p.send_us;
                            if s.matched == 0 {
                                (s.first_arrival_us, s.last_arrival_us) = (a, a);
                                s.owd_min_us = owd;
                            }
                            s.matched += 1;
                            s.recv_bytes += p.bytes;
                            s.last_arrival_us = s.last_arrival_us.max(a);
                            s.owd_min_us = s.owd_min_us.min(owd);
                            s.owd_last_us = owd;
                        }
                    }
                }
                self.reported_to = j + 1;
                self.reports
                    .push_back(((r + self.path.prop_us) as u64 * 1_000, s));
            }
        }

        /// Run `secs` of `content(k)` (bytes at QP 15, keyframe) through the governors.
        fn run(&mut self, g: &mut Governors, secs: u64, content: &dyn Fn(u64) -> (u64, bool)) {
            let mut shown = (g.q(), g.divisor());
            for k in 0..secs * FPS {
                let now = k * PERIOD_NS + 1;
                self.client_reports(now);
                while self.reports.front().is_some_and(|(t, _)| *t <= now) {
                    let (t, s) = self.reports.pop_front().unwrap();
                    g.feedback(t, &s);
                }
                g.tick(now);
                let (q, d) = (g.q(), g.divisor());
                if (q, d) != shown {
                    shown = (q, d);
                    self.trace.push((now / MS, q, d));
                }
                if k % d as u64 != 0 {
                    continue;
                }
                let (mut base, mut key) = content(k);
                if std::mem::take(&mut self.force_key) {
                    (base, key) = (400_000, true);
                }
                if base == 0 {
                    // nothing changed on screen and no repeat is due
                    continue;
                }
                let sq = match self.model {
                    Model::Honest => q,
                    Model::Ignores { .. } => self.costly_q.unwrap_or(15),
                };
                let scale = 2f64.powf(-(sq as f64 - 15.0) / self.halving_qp) * (d as f64).sqrt();
                let bytes = (base as f64 * scale) as u64;
                if let Some((want, in_force)) = g.emitted(now, bytes, false, key, Some(sq)) {
                    let works = self.model == Model::Ignores { costly_works: true };
                    if works {
                        self.costly_q = Some(want);
                        self.force_key = true;
                        self.costly_calls.push(now / MS);
                    }
                    g.costly_result(want, works, in_force);
                }
                self.send(now, bytes);
            }
        }

        fn moves(&self) -> usize {
            self.trace.len()
        }

        /// Frames sent in `[from, to)` ms that were late by more than `ms` or lost a packet.
        fn bad_frames(&self, from: u64, to: u64, ms: f64) -> usize {
            self.frames
                .iter()
                .filter(|(t, _)| (from..to).contains(t))
                .filter(|(_, d)| d.is_none_or(|d| d > ms))
                .count()
        }

        /// How long after `from` the frames stop being late by `ms` or broken, for `hold` ms.
        fn settled_after(&self, from: u64, ms: f64, hold: u64) -> Option<u64> {
            let mut since = None;
            for (t, d) in self.frames.iter().filter(|(t, _)| *t >= from) {
                if d.is_none_or(|d| d > ms) {
                    since = None;
                } else {
                    let s = *since.get_or_insert(*t);
                    if t - s >= hold {
                        return Some(s - from);
                    }
                }
            }
            None
        }

        fn sent_mbps(&self, from: u64, to: u64) -> f64 {
            let range = (from * 1_000) as i64..(to * 1_000) as i64;
            let b: u64 = self
                .pkts
                .iter()
                .filter(|p| range.contains(&p.send_us))
                .map(|p| p.bytes)
                .sum();
            b as f64 * 8.0 / ((to - from) as f64 / 1e3) / 1e6
        }

        fn q_at(&self, ms: u64) -> (u8, u32) {
            self.trace
                .iter()
                .take_while(|(t, _, _)| *t <= ms)
                .last()
                .map_or((15, 1), |(_, q, d)| (*q, *d))
        }
    }

    fn lan() -> Path {
        Path {
            cap: |_| 1e9,
            queue_s: 0.05,
            prop_us: 1_000,
            jitter_us: 1_000.0,
            loss: 0.0,
            stall: None,
            delay_step: None,
            loss_run: 0,
        }
    }

    fn governors() -> Governors {
        Governors::new(15, Some(qp_ceiling_bps(1920, 1080, 60)), 60, true)
    }

    /// A desktop: ~10 Mbps with frame-to-frame variation, a window drag for one second in ten at
    /// three times the average (the worst second of a recorded drag), and a 400 KB keyframe at
    /// the start and every 20 s.
    fn desktop(seed: u64) -> impl Fn(u64) -> (u64, bool) {
        move |k| {
            if k % (20 * FPS) == 0 {
                return (400_000, true);
            }
            let mut r = Rng((seed ^ k.wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1);
            let noise = 0.6 + 0.8 * r.next();
            let avg = if (k / FPS) % 10 == 4 { 30e6 } else { 10e6 };
            ((avg / 8.0 / FPS as f64 * noise) as u64, false)
        }
    }

    fn steady(mbps: f64) -> impl Fn(u64) -> (u64, bool) {
        move |k| {
            if k == 0 {
                return (400_000, true);
            }
            let mut r = Rng(k.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
            let v = 0.7 + 0.6 * r.next();
            ((mbps * 1e6 / 8.0 / FPS as f64 * v) as u64, false)
        }
    }

    #[test]
    fn a_healthy_lan_with_a_bursty_desktop_moves_nothing() {
        let mut g = governors();
        let mut sim = Sim::new(lan(), 1);
        sim.run(&mut g, 60, &desktop(1));
        assert_eq!(sim.moves(), 0, "{:?}", sim.trace);
        assert_eq!(g.q(), 15);
    }

    #[test]
    fn random_loss_without_a_capacity_limit_moves_nothing() {
        for (loss, jitter_ms) in [(0.01, 2.0), (0.05, 2.0), (0.05, 15.0)] {
            let mut g = governors();
            let path = Path {
                cap: |_| 200e6,
                queue_s: 0.2,
                loss,
                jitter_us: jitter_ms * 1e3,
                prop_us: 3_000,
                stall: None,
                delay_step: None,
                loss_run: 0,
            };
            let mut sim = Sim::new(path, 7);
            sim.run(&mut g, 60, &desktop(3));
            assert_eq!(sim.moves(), 0, "loss {loss}: {:?}", sim.trace);
        }
    }

    #[test]
    fn periodic_200_ms_stalls_leave_no_lasting_cut() {
        let mut g = governors();
        let path = Path {
            stall: Some((3, 200)),
            delay_step: None,
            loss_run: 0,
            ..lan()
        };
        let mut sim = Sim::new(path, 11);
        sim.run(&mut g, 40, &desktop(5));
        assert_eq!(sim.moves(), 0, "{:?}", sim.trace);
    }

    /// The relay's cap: a 100 ms drop-tail queue.
    fn capped(cap: fn(u64) -> f64) -> Path {
        Path {
            cap,
            queue_s: 0.1,
            prop_us: 2_000,
            jitter_us: 1_000.0,
            loss: 0.0,
            stall: None,
            delay_step: None,
            loss_run: 0,
        }
    }

    fn report(name: &str, sim: &Sim, onset_ms: u64) {
        eprintln!(
            "{name}: trace {:?}\n  settled {:?} ms after onset; late or broken frames: {} in the \
             first 2 s, {} in the 20 s after; sent {:.2} Mbps over the last 10 s",
            sim.trace,
            sim.settled_after(onset_ms, 60.0, 1_000),
            sim.bad_frames(onset_ms, onset_ms + 2_000, 60.0),
            sim.bad_frames(onset_ms + 2_000, onset_ms + 22_000, 60.0),
            sim.sent_mbps(onset_ms + 20_000, onset_ms + 30_000),
        );
    }

    #[test]
    fn a_step_down_to_5_mbps_fits_within_a_second_and_stays() {
        let mut g = governors();
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 5e6 }), 21);
        sim.run(&mut g, 40, &steady(12.0));
        report("cap5", &sim, 10_000);
        let settled = sim
            .settled_after(10_000, 60.0, 1_000)
            .expect("never fitted");
        assert!(settled <= 1_000, "settled {settled} ms after the cap");
        assert_eq!(g.divisor(), 1, "5 Mbps needs no frame-rate cut for 12 Mbps");
        let late = sim.bad_frames(12_000, 40_000, 60.0);
        assert!(late * 100 < 28 * FPS as usize * 3, "{late} late frames");
        let raises = g.link.as_ref().unwrap().raises;
        assert!(raises <= 6, "{raises} raises: {:?}", sim.trace);
        assert!(sim.sent_mbps(30_000, 40_000) > 3.5, "under-using the link");
    }

    #[test]
    fn a_step_down_to_1_5_mbps_fits_and_gates_the_frame_rate_only_past_the_qp_bound() {
        let mut g = governors();
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 1.5e6 }), 31);
        sim.run(&mut g, 40, &steady(20.0));
        report("cap1_5", &sim, 10_000);
        let settled = sim
            .settled_after(10_000, 60.0, 1_000)
            .expect("never fitted");
        assert!(settled <= 2_500, "settled {settled} ms after the cap");
        for w in sim.trace.windows(2) {
            if w[1].2 > w[0].2 {
                assert_eq!(w[0].1, QP_LINK_MAX, "{:?}", sim.trace);
            }
        }
        assert!(
            g.divisor() > 1,
            "20 Mbps at QP 34 is 2.2 Mbps: the rate must come down"
        );
    }

    #[test]
    fn a_cap_that_lifts_returns_to_the_users_qp() {
        let mut g = governors();
        let lifts = |t| {
            if (10_000 * MS..30_000 * MS).contains(&t) {
                5e6
            } else {
                1e9
            }
        };
        let mut sim = Sim::new(capped(lifts), 41);
        sim.run(&mut g, 50, &steady(12.0));
        report("cap lifts", &sim, 10_000);
        assert_eq!((g.q(), g.divisor()), (15, 1), "{:?}", sim.trace);
        let back = sim.trace.last().unwrap().0;
        eprintln!("cap lifted at 30000 ms, back at QP 15 at {back} ms");
        assert!(
            back <= 30_000 + 15_000,
            "back at the user's QP at {back} ms"
        );
    }

    #[test]
    fn the_cap_change_scenario_of_the_browser_harness() {
        // 8 -> 1.5 -> 8 Mbps at 7 s and 14 s, a 12 Mbps stream
        let mut g = governors();
        let change = |t| {
            if (7_000 * MS..14_000 * MS).contains(&t) {
                1.5e6
            } else {
                8e6
            }
        };
        let mut sim = Sim::new(capped(change), 51);
        sim.run(&mut g, 30, &steady(12.0));
        report("cap_change 8/1.5/8", &sim, 7_000);
        let (q8, _) = sim.q_at(6_900);
        let (q15, _) = sim.q_at(13_900);
        assert!(
            q8 > 15 && q8 < q15,
            "8 Mbps at QP {q8}, 1.5 Mbps at QP {q15}"
        );
        assert!(sim.q_at(30_000).0 < q15, "{:?}", sim.trace);
        assert!(sim.settled_after(7_000, 60.0, 1_000).unwrap() <= 1_500);
    }

    #[test]
    fn incompressible_content_on_a_lan_is_the_ceilings_job() {
        // 800x600 noise: 371 Mbps at QP 15, on a gigabit link that carries it
        let mut g = Governors::new(15, Some(qp_ceiling_bps(800, 600, 60)), 60, true);
        let mut sim = Sim::new(lan(), 61);
        sim.run(&mut g, 10, &steady(371.0));
        let c = g.ceiling.as_ref().unwrap();
        assert!(c.q() > 15, "the ceiling did not act");
        assert!(371e6 * 2f64.powf(-(c.q() as f64 - 15.0) / 6.0) <= c.ceiling_bps() as f64);
        assert_eq!(g.link.as_ref().unwrap().raises, 0, "the link carried it");
        assert!(sim.trace.first().unwrap().0 <= 1_000, "{:?}", sim.trace);
    }

    #[test]
    fn an_estimate_at_its_maximum_is_not_evidence() {
        // the stream sits exactly at what the link delivers, with no queue and no loss: nothing
        // here says the link cannot carry more, whatever an estimator would make of it
        let mut l = LinkGovernor::new(15);
        for k in 0..100u64 {
            let t = k * 50 * MS;
            let us = (t / 1_000) as i64;
            l.encoded(t, 12_000_000 / 8 / 20, false);
            let s = PathSample {
                reported: 60,
                known: 60,
                sent_bytes: 75_000,
                first_send_us: us,
                last_send_us: us + 49_000,
                matched: 60,
                recv_bytes: 75_000,
                first_arrival_us: us + 2_000,
                last_arrival_us: us + 51_000,
                owd_min_us: 2_000,
                owd_last_us: 2_000,
                ..Default::default()
            };
            l.feedback(t, &s);
            assert!(l.tick(t, 15).is_none(), "moved at {t}");
        }
    }

    #[test]
    fn a_weak_qp_response_still_fits_by_further_raises() {
        let mut g = governors();
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 5e6 }), 91);
        sim.halving_qp = 16.0;
        sim.run(&mut g, 40, &steady(12.0));
        report("cap5, x1.3 per six QP", &sim, 10_000);
        let settled = sim
            .settled_after(10_000, 60.0, 1_000)
            .expect("never fitted");
        assert!(settled <= 3_500, "settled {settled} ms after the cap");
        let late = sim.bad_frames(14_000, 40_000, 60.0);
        // detection waits for two windows of evidence on the window minimum, and each failed
        // probe runs that much longer into the cap
        assert!(late * 100 < 26 * FPS as usize * 8, "{late} late frames");
    }

    #[test]
    fn past_the_bound_and_a_quarter_of_the_frames_the_qp_is_the_last_resort() {
        // 40 Mbps of noise into 1.5 Mbps: QP 34 at a quarter of the frames is still 2.2 Mbps
        let mut g = governors();
        let lifts = |t| {
            if (10_000 * MS..30_000 * MS).contains(&t) {
                1.5e6
            } else {
                1e9
            }
        };
        let mut sim = Sim::new(capped(lifts), 111);
        sim.run(&mut g, 60, &steady(40.0));
        report("last resort", &sim, 10_000);
        let peak = sim.trace.iter().map(|t| t.1).max().unwrap();
        assert!(peak > QP_LINK_MAX, "{:?}", sim.trace);
        for w in sim.trace.windows(2) {
            if w[1].1 > QP_LINK_MAX && w[1].1 > w[0].1 {
                assert_eq!(
                    w[0].2, MAX_DIVISOR,
                    "past the bound before the frame rate: {:?}",
                    sim.trace
                );
            }
        }
        assert!(sim.settled_after(10_000, 60.0, 1_000).unwrap() <= 3_500);
        // after the lift the last resort goes first, then the frame rate, then the QP
        let after: Vec<_> = sim.trace.iter().filter(|t| t.0 > 30_000).collect();
        let first_div_step = after.iter().position(|t| t.2 < MAX_DIVISOR).unwrap();
        assert!(
            after[..first_div_step].iter().all(|t| t.1 >= QP_LINK_MAX),
            "{after:?}"
        );
        assert_eq!((g.q(), g.divisor()), (15, 1), "{:?}", sim.trace);
    }

    #[test]
    fn an_encoder_that_ignores_set_qp_falls_back_to_the_frame_rate() {
        let mut g = governors();
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 5e6 }), 71);
        sim.model = Model::Ignores {
            costly_works: false,
        };
        // 8 Mbps: a quarter of the frames fits 5 Mbps, so the frame rate alone can answer
        sim.run(&mut g, 40, &steady(8.0));
        report("ignores set_qp", &sim, 10_000);
        assert!(g.unconfirmed >= 1, "the ignored raise was not noticed");
        assert!(g.qp_dead, "{:?}", sim.trace);
        assert!(g.divisor() > 1, "{:?}", sim.trace);
        assert_eq!(g.q(), 15, "the QP it reports must be the one in the stream");
        let settled = sim
            .settled_after(10_000, 60.0, 1_000)
            .expect("never fitted");
        assert!(settled <= 3_000, "settled {settled} ms after the cap");
    }

    #[test]
    fn a_costly_qp_path_is_taken_once_and_never_storms() {
        let mut g = governors();
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 5e6 }), 81);
        sim.model = Model::Ignores { costly_works: true };
        sim.run(&mut g, 60, &steady(12.0));
        report("costly path", &sim, 10_000);
        eprintln!("costly path taken at {:?} ms", sim.costly_calls);
        assert!(!sim.costly_calls.is_empty(), "{:?}", sim.trace);
        for w in sim.costly_calls.windows(2) {
            assert!(w[1] - w[0] >= 10_000, "{:?}", sim.costly_calls);
        }
        assert!(sim.settled_after(10_000, 60.0, 1_000).unwrap() <= 3_000);
        // what the governor reports is what the stream carries
        assert_eq!(g.q(), sim.costly_q.unwrap_or(15), "{:?}", sim.trace);
    }

    // the client's pace: a scripted page, one report a second

    /// Feed `states` (one per second from `t0_s`) with `skips` paints skipped per report; returns
    /// the divisor after each.
    fn pace_run(
        g: &mut ClientPaceGovernor,
        t0_s: u64,
        states: &[u8],
        skips: u64,
        total: &mut u64,
    ) -> Vec<u32> {
        let mut out = Vec::new();
        for (i, &st) in states.iter().enumerate() {
            let t = (t0_s + i as u64) * 1_000 * MS;
            if st == 1 {
                *total += skips;
            }
            let p = ClientPace {
                received_ns: t,
                state: st,
                paints_skipped: *total,
                catch_ups: 0,
            };
            g.observe(t, Some(p));
            // a second look at the same report changes nothing
            g.observe(t + 300 * MS, Some(p));
            out.push(g.divisor());
        }
        out
    }

    #[test]
    fn a_client_in_step_is_left_alone_and_no_field_is_a_no_op() {
        let mut g = ClientPaceGovernor::default();
        let mut total = 0;
        assert!(pace_run(&mut g, 0, &[0; 60], 0, &mut total)
            .iter()
            .all(|d| *d == 1));
        let mut g = ClientPaceGovernor::default();
        for k in 0..100 {
            assert!(g.observe(k * 100 * MS, None).is_none());
        }
        let t = crate::streamer::telemetry::SessionTelemetry::default();
        assert_eq!(t.client_pace(), None);
    }

    #[test]
    fn a_client_that_cannot_keep_up_halves_the_rate_and_gets_it_back() {
        let mut g = ClientPaceGovernor::default();
        let mut total = 0;
        let mut script = vec![0u8; 3];
        script.extend([1u8; 8]);
        let d = pace_run(&mut g, 0, &script, 10, &mut total);
        // two reports behind -> 1/2; still behind after the 3 s hold -> 1/4; never past 4
        assert_eq!(d[4], 2, "{d:?}");
        assert_eq!(*d.last().unwrap(), 4, "{d:?}");
        // in step: one doubling per 10 s
        let d = pace_run(&mut g, 11, &[0; 25], 0, &mut total);
        assert_eq!(d[9], 4, "{d:?}");
        assert_eq!(d[11], 2, "{d:?}");
        assert_eq!(d[22], 1, "{d:?}");
        assert_eq!(g.restores, 2);
    }

    #[test]
    fn waiting_for_a_keyframe_is_not_slowness() {
        let mut g = ClientPaceGovernor::default();
        let mut total = 0;
        let d = pace_run(&mut g, 0, &[0, 3, 3, 3, 3, 3, 0, 0], 0, &mut total);
        assert!(d.iter().all(|d| *d == 1), "{d:?}");
        // behind with skipping no help is slowness
        let mut g = ClientPaceGovernor::default();
        let d = pace_run(&mut g, 0, &[0, 2, 2, 2], 0, &mut total);
        assert_eq!(*d.last().unwrap(), 2, "{d:?}");
    }

    #[test]
    fn a_restore_that_fails_waits_twice_as_long_next_time() {
        let mut g = ClientPaceGovernor::default();
        let mut total = 0;
        pace_run(&mut g, 0, &[0, 1, 1, 1], 10, &mut total);
        assert_eq!(g.divisor(), 2);
        let d = pace_run(&mut g, 4, &[0; 12], 0, &mut total);
        assert_eq!(*d.last().unwrap(), 1, "{d:?}");
        pace_run(&mut g, 16, &[1, 1, 1], 10, &mut total);
        assert_eq!(g.divisor(), 2);
        // now 20 s in step before the next restore
        let d = pace_run(&mut g, 19, &[0; 25], 0, &mut total);
        assert_eq!(d[15], 2, "{d:?}");
        assert_eq!(d[21], 1, "{d:?}");
    }

    #[test]
    fn a_slow_client_on_a_healthy_link_never_raises_the_qp() {
        let mut g = governors();
        let mut pace = ClientPaceGovernor::default();
        let mut sim = Sim::new(lan(), 101);
        sim.run(&mut g, 30, &desktop(9));
        let mut total = 0u64;
        for s in 0..30u64 {
            total += 20;
            let now = (s + 1) * 1_000 * MS;
            pace.observe(
                now,
                Some(ClientPace {
                    received_ns: now,
                    state: 1,
                    paints_skipped: total,
                    catch_ups: s,
                }),
            );
        }
        assert_eq!(g.q(), 15);
        assert_eq!(g.divisor(), 1, "the link side saw nothing");
        assert_eq!(pace.divisor(), MAX_DIVISOR);
    }

    #[test]
    fn the_divisor_never_gates_a_frame_the_path_cannot_hold() {
        use super::super::pipeline::{Admit, FrameGate};
        use std::time::{Duration, Instant};
        let t = SessionTelemetry::default();
        let period = Duration::from_micros(16_667);
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        for trailing in [false, true] {
            let mut g = FrameGate::new(period, false, false, false);
            g.divisor = 4;
            assert_eq!(
                g.admit(at(0), false, None, trailing, trailing, || None, &t),
                Admit::Stage
            );
            // 20 ms later is inside a quarter-rate window (62.5 ms) but past the undivided 12.5 ms
            let next = g.admit(at(20), false, None, trailing, trailing, || None, &t);
            if trailing {
                assert!(matches!(next, Admit::Hold { .. }), "{next:?}");
            } else {
                assert_eq!(next, Admit::Stage, "a classic path would lose this frame");
            }
        }
    }

    #[test]
    fn without_frame_hold_only_the_qp_answers_and_stops_at_the_bound() {
        let mut g = governors();
        g.hold_frames(false);
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 1.5e6 }), 131);
        sim.run(&mut g, 30, &steady(20.0));
        report("cap1_5 without frame hold", &sim, 10_000);
        assert!(sim.trace.iter().all(|t| t.2 == 1), "{:?}", sim.trace);
        assert_eq!(
            sim.trace.iter().map(|t| t.1).max(),
            Some(QP_LINK_MAX),
            "{:?}",
            sim.trace
        );
    }

    // the path's delay moving is not a queue

    fn stepped(seed: u64, jitter_ms: f64) -> Path {
        Path {
            cap: |_| 1e9,
            queue_s: 0.2,
            prop_us: 2_000,
            jitter_us: jitter_ms * 1e3,
            loss: 0.0,
            stall: None,
            delay_step: Some((10_000 * MS + seed * 37 * MS, 60_000)),
            loss_run: 0,
        }
    }

    #[test]
    fn a_lasting_rise_in_base_delay_moves_nothing() {
        for seed in 1..=5u64 {
            for jitter_ms in [0.0, 10.0, 15.0] {
                let mut g = governors();
                let mut sim = Sim::new(stepped(seed, jitter_ms), seed);
                sim.run(&mut g, 30, &steady(12.0));
                let l = g.link.as_ref().unwrap();
                assert_eq!(
                    l.raises, 0,
                    "seed {seed}, jitter {jitter_ms} ms: {:?}",
                    sim.trace
                );
                assert!(l.rebases >= 1, "the new base was never taken");
            }
        }
    }

    #[test]
    fn a_base_step_with_random_loss_leaves_no_lasting_cut() {
        for seed in 1..=3u64 {
            let mut g = governors();
            let mut path = stepped(seed, 10.0);
            path.loss = 0.03;
            let mut sim = Sim::new(path, seed);
            sim.run(&mut g, 30, &steady(12.0));
            let step_ms = 10_000 + seed * 37;
            let back = sim.q_at(step_ms + 6_000);
            assert_eq!(back, (15, 1), "seed {seed}: {:?}", sim.trace);
            assert_eq!(
                (g.q(), g.divisor()),
                (15, 1),
                "seed {seed}: {:?}",
                sim.trace
            );
        }
    }

    /// A stream a few percent over the link: the queue creeps up to the buffer and, on a deep
    /// one, would stand for hundreds of milliseconds if nothing called it.
    #[test]
    fn a_slowly_filling_queue_is_called_and_never_taken_for_the_base() {
        for (over, queue_s) in [(1.02, 0.1), (1.03, 0.5), (1.01, 0.5)] {
            let mut g = governors();
            let mut path = capped(|_| 5e6);
            path.queue_s = queue_s;
            let mut sim = Sim::new(path, 171);
            sim.run(&mut g, 40, &steady(5.0 * over));
            let l = g.link.as_ref().unwrap();
            eprintln!(
                "{:.0} % over, {} ms buffer: {:?}, rebases {}",
                (over - 1.0) * 100.0,
                queue_s * 1e3,
                sim.trace,
                l.rebases
            );
            assert!(l.raises >= 1, "never called: {:?}", sim.trace);
            assert_eq!(l.rebases, 0, "a creeping queue was taken for the base");
            let late = sim.bad_frames(30_000, 40_000, 100.0);
            assert!(
                late * 100 < 10 * FPS as usize * 5,
                "{late} frames over 100 ms late"
            );
        }
    }

    #[test]
    fn a_cap_after_a_base_step_is_still_found() {
        let mut g = governors();
        let mut path = capped(|t| if t < 15_000 * MS { 1e9 } else { 5e6 });
        path.delay_step = Some((5_000 * MS, 60_000));
        path.jitter_us = 10_000.0;
        let mut sim = Sim::new(path, 141);
        sim.run(&mut g, 30, &steady(12.0));
        report("cap after a base step", &sim, 15_000);
        assert!(sim.trace.iter().all(|t| t.0 >= 15_000), "{:?}", sim.trace);
        assert!(sim.settled_after(15_000, 60.0, 1_000).unwrap() <= 1_500);
    }

    #[test]
    fn a_new_candidate_pair_forgets_the_old_base() {
        let mut l = LinkGovernor::new(15);
        let s = PathSample {
            reported: 20,
            known: 20,
            sent_bytes: 24_000,
            last_send_us: 100_000,
            matched: 20,
            recv_bytes: 24_000,
            first_arrival_us: 2_000,
            last_arrival_us: 102_000,
            owd_min_us: 2_000,
            owd_last_us: 2_000,
            ..Default::default()
        };
        l.feedback(0, &s);
        assert_eq!(l.base_owd(0), Some(2_000));
        l.path_changed();
        assert_eq!(l.base_owd(0), None);
        assert_eq!(l.rebases, 1);
    }

    // clean links the relay can make: delay with jitter both ways, losses in bursts

    #[test]
    fn delay_jitter_and_burst_loss_move_nothing() {
        for (jitter_ms, loss, run) in [(20.0, 0.0, 0u32), (2.0, 0.01, 4), (10.0, 0.01, 4)] {
            let mut g = governors();
            let path = Path {
                cap: |_| 200e6,
                queue_s: 0.2,
                prop_us: 20_000,
                jitter_us: jitter_ms * 1e3,
                loss,
                stall: None,
                delay_step: None,
                loss_run: run,
            };
            let mut sim = Sim::new(path, 151);
            sim.run(&mut g, 60, &desktop(7));
            assert_eq!(
                sim.moves(),
                0,
                "jitter {jitter_ms} ms, loss {loss} x {}: {:?}",
                run + 1,
                sim.trace
            );
        }
    }

    #[test]
    fn a_still_screen_after_congestion_is_released() {
        let mut g = governors();
        // capped for the whole run, so no "clear" verdict will ever come from the link
        let mut sim = Sim::new(capped(|t| if t < 5_000 * MS { 1e9 } else { 5e6 }), 161);
        let content = |k: u64| -> (u64, bool) {
            if k < 15 * FPS {
                steady(12.0)(k)
            } else if k.is_multiple_of(12) {
                (300, false)
            } else {
                (0, false)
            }
        };
        sim.run(&mut g, 25, &content);
        assert!(sim.q_at(14_000).0 > 15, "never engaged: {:?}", sim.trace);
        assert_eq!(sim.q_at(18_500), (15, 1), "{:?}", sim.trace);
    }

    #[test]
    fn the_learnt_slope_is_bounded_read_in_time_and_fades() {
        let mut l = LinkGovernor::new(15);
        // content got busier just as the QP rose: the rate barely moved
        for k in 0..40u64 {
            l.encoded(k * 16 * MS, 20_600, false);
        }
        l.last_raise = Some((10.4e6, 9, 0));
        l.learn_slope(640 * MS);
        assert!(l.halving_qp() <= SLOPE_MAX, "{}", l.halving_qp());
        assert!(l.halving_qp() > SLOPE_MIN);
        let learnt = l.halving_qp();
        // a reading taken long after the raise is not taken at all
        l.last_raise = Some((10.4e6, 9, 0));
        l.learn_slope(5_000 * MS);
        assert_eq!(l.halving_qp(), learnt);
        // a raise with no fresh reading lets it fade toward six
        l.enc_since_ns = Some(0);
        l.halving_qp = SLOPE_MAX;
        let w = Window {
            valid: true,
            delivered_bps: 1e6,
            sent_bps: 2e6,
            share: 0.5,
            sigma: 1.0,
            ..Default::default()
        };
        for k in 0..30u64 {
            l.encoded(5_000 * MS + k * 16 * MS, 20_000, false);
        }
        l.raise(5_500 * MS, 15, w);
        assert!(l.halving_qp() < SLOPE_MAX, "{}", l.halving_qp());
    }

    #[test]
    fn stretched_arrivals_without_loss_are_not_loss() {
        let s = PathSample {
            reported: 20,
            known: 20,
            sent_bytes: 24_000,
            first_send_us: 0,
            last_send_us: 100_000,
            matched: 20,
            recv_bytes: 24_000,
            first_arrival_us: 0,
            last_arrival_us: 180_000,
            owd_min_us: 0,
            owd_last_us: 80_000,
            ..Default::default()
        };
        let w = LinkGovernor::window(&mut std::iter::once(&s), Some(0));
        assert!(w.valid);
        assert_eq!(w.share, 1.0);
        assert!(w.sigma < 0.6, "{}", w.sigma);
        // two such windows with no queue rise are neither loss nor growth
        let mut l = LinkGovernor::new(15);
        for k in 0..20u64 {
            let mut x = s;
            let off = (k * 100_000) as i64;
            x.first_send_us += off;
            x.last_send_us += off;
            x.first_arrival_us += off;
            x.last_arrival_us += off;
            l.encoded(k * 100 * MS, 150_000, false);
            l.feedback(k * 100 * MS, &x);
            assert!(l.tick(k * 100 * MS, 15).is_none(), "moved at {k}");
        }
    }

    // the encode thread's part

    fn armed(qp: u8) -> std::sync::Arc<SessionTelemetry> {
        let t = SessionTelemetry::new_shared();
        arm(&t, &cfg(qp));
        set_frame_hold(&t, true);
        t
    }

    struct Fake {
        set: Vec<u8>,
        ok: bool,
    }

    impl QpEncoder for Fake {
        fn set_qp(&mut self, qp: u8) -> Result<bool> {
            self.set.push(qp);
            Ok(self.ok)
        }
    }

    #[test]
    fn a_governor_dropped_late_does_not_silence_the_current_one() {
        let t = armed(18);
        let older = QpControl::build(&t, true, false, false).unwrap();
        let newer = QpControl::build(&t, true, false, false).unwrap();
        drop(older);
        assert!(t.link.listening.load(Ordering::Relaxed));
        drop(newer);
        assert!(!t.link.listening.load(Ordering::Relaxed));
    }

    #[test]
    fn an_encoder_without_qp_control_leaves_the_frame_rate_and_is_asked_again_after_a_rebuild() {
        let t = armed(18);
        let mut c = QpControl::build(&t, true, false, false).unwrap();
        let mut no = Fake {
            set: Vec::new(),
            ok: false,
        };
        c.before_submit(&mut no);
        c.before_submit(&mut no);
        assert_eq!(no.set, vec![18], "asked once, not on every frame");
        assert!(c.qp_unsupported && !c.off);
        assert!(c.gov.link.is_some(), "the frame rate can still answer");
        c.encoder_replaced();
        let mut yes = Fake {
            set: Vec::new(),
            ok: true,
        };
        c.before_submit(&mut yes);
        assert_eq!(yes.set, vec![18]);
        assert!(!c.qp_unsupported);
    }

    #[test]
    fn the_keyframe_boost_is_only_for_an_engaged_governor() {
        let t = armed(18);
        let mut c = QpControl::build(&t, true, false, false).unwrap();
        let mut f = Fake {
            set: Vec::new(),
            ok: true,
        };
        c.keyframe_next(true);
        c.before_submit(&mut f);
        c.gov.link.as_mut().unwrap().q = 25;
        c.keyframe_next(true);
        c.before_submit(&mut f);
        c.before_submit(&mut f);
        assert_eq!(f.set, vec![18, 25 + KEYFRAME_BOOST, 25]);
    }

    /// What the governor adds per frame, after the send and before the next submit, against a
    /// scan of the whole access unit.
    #[test]
    fn the_per_frame_cost_is_small_and_never_scales_with_the_frame() {
        crate::test_support::show_measurements();
        let t = armed(15);
        let mut c = QpControl::build(&t, true, false, false).unwrap();
        let mut f = Fake {
            set: Vec::new(),
            ok: true,
        };
        let mut p_frame = vec![0x55u8; 30_000];
        p_frame[..6].copy_from_slice(&[0, 0, 0, 1, 0x41, 0x9a]);
        let mut key = vec![0x55u8; 400_000];
        key[..6].copy_from_slice(&[0, 0, 0, 1, 0x65, 0x88]);
        let (mut after_send, mut before_submit, mut scan) = (Vec::new(), Vec::new(), Vec::new());
        for k in 0..6_000u32 {
            let keyframe = k.is_multiple_of(600);
            let au = if keyframe { &key } else { &p_frame };
            c.keyframe_next(keyframe);
            let t0 = std::time::Instant::now();
            c.before_submit(&mut f);
            before_submit.push(t0.elapsed().as_nanos() as u64);
            let t1 = std::time::Instant::now();
            c.emitted(au, false);
            after_send.push(t1.elapsed().as_nanos() as u64);
            assert!(c
                .pending
                .iter()
                .all(|e| e.prefix.as_ref().is_none_or(|p| p.len() <= PREFIX_BYTES)));
            let t2 = std::time::Instant::now();
            let mut n = 0;
            let mut pos = find_start(au, 0);
            while let Some(i) = pos {
                n += 1;
                pos = find_start(au, i);
            }
            std::hint::black_box(n);
            scan.push(t2.elapsed().as_nanos() as u64);
        }
        let pct = |v: &mut Vec<u64>, p: f64| {
            v.sort_unstable();
            v[((v.len() - 1) as f64 * p) as usize] as f64 / 1e3
        };
        let (a50, a99) = (pct(&mut after_send, 0.5), pct(&mut after_send, 0.99));
        let (b50, b99) = (pct(&mut before_submit, 0.5), pct(&mut before_submit, 0.99));
        let (s50, s99) = (pct(&mut scan, 0.5), pct(&mut scan, 0.99));
        tprintln!(
            "governor per frame, us p50/p99: after the send {a50:.2}/{a99:.2}, before the next \
             submit {b50:.2}/{b99:.2}; a whole-frame scan {s50:.2}/{s99:.2}"
        );
        assert!(
            a99 < s50,
            "the record after the send must not cost a frame scan"
        );
    }

    #[test]
    fn x264_keyframes_are_not_counted_as_read_back_mismatches() {
        struct X<'a>(&'a mut super::super::x264::encoder::X264Encoder);
        impl QpEncoder for X<'_> {
            fn set_qp(&mut self, qp: u8) -> Result<bool> {
                self.0.set_qp(qp)
            }
        }
        // not widened: the keyframe keeps libx264's own I-frame offset (QP 15 for 18)
        let mut e = match x264_open(cfg(18), false) {
            Ok(e) => e,
            Err(e) => return crate::test_support::skip("x264 keyframe read-back", e),
        };
        let t = armed(18);
        let mut c = QpControl::build(&t, true, false, false).unwrap();
        let mut buf = vec![0u8; 640 * 360 * 4];
        for k in 0..12u32 {
            fill(&mut buf, 640, 360, k);
            c.keyframe_next(k == 0);
            c.before_submit(&mut X(&mut e));
            let au = e.encode_bgra(&buf, k == 0).unwrap();
            c.emitted(&au, false);
        }
        c.before_submit(&mut X(&mut e));
        let l = &t.link;
        assert_eq!(l.mismatches.load(Ordering::Relaxed), 0);
        assert!(l.verified.load(Ordering::Relaxed) >= 2);
    }

    fn x264_open(
        config: EncoderConfig,
        wide: bool,
    ) -> Result<super::super::x264::encoder::X264Encoder> {
        use super::super::x264::encoder::{X264Encoder, X264Options};
        X264Encoder::with_options(
            config,
            X264Options {
                wide_qp_clamp: wide,
                ..X264Options::from_switches()
            },
        )
    }

    #[test]
    fn x264_without_a_switch_has_default_cqp_parameters_and_is_deterministic() {
        use super::super::x264::encoder::X264Options;
        // this process names no switch, so the options a session gets leave CQP alone
        assert!(!X264Options::from_switches().wide_qp_clamp);
        let run = |wide: bool| x264_open(cfg(18), wide).map(|e| e.effective_params());
        let (narrow, wide) = match (run(false), run(true)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => {
                return crate::test_support::skip("x264 CQP parameters", e)
            }
        };
        assert_eq!(narrow.rc.i_qp_constant, 18);
        assert_eq!(narrow.rc.f_ip_factor, 1.4);
        assert_eq!(narrow.rc.f_pb_factor, 1.3);
        // two encoders opened without it code the same pictures to the byte
        let encode = || -> Result<Vec<Vec<u8>>> {
            let mut e = x264_open(cfg(18), false)?;
            let mut buf = vec![0u8; 640 * 360 * 4];
            (0..20u32)
                .map(|k| {
                    fill(&mut buf, 640, 360, k);
                    e.encode_bgra(&buf, k == 0)
                })
                .collect()
        };
        assert_eq!(encode().unwrap(), encode().unwrap());
        assert_ne!(wide.rc.i_qp_constant, 18, "widened");
    }

    /// Widened for a governor, every user setting from 1 to 51 still codes its keyframes and its
    /// pictures at the QPs libx264 gives it unwidened, and the governor's range is reachable.
    #[test]
    fn x264_widened_keeps_every_setting_and_its_keyframe_and_reaches_the_governors_range() {
        let mut buf = vec![0u8; 640 * 360 * 4];
        for base in 1..=51u8 {
            let (mut narrow, mut wide) =
                match (x264_open(cfg(base), false), x264_open(cfg(base), true)) {
                    (Ok(a), Ok(b)) => (a, b),
                    (Err(e), _) | (_, Err(e)) => {
                        return crate::test_support::skip("x264 setting range", e)
                    }
                };
            let (_, top) = wide.qp_range().expect("widened");
            assert!(top >= QP_LINK_MAX.max(base), "setting {base}: top {top}");
            if base >= 15 {
                assert_eq!(top, 51, "setting {base}");
            }
            let (mut rn, mut rw) = (SliceQp::default(), SliceQp::default());
            for k in 0..3u32 {
                fill(&mut buf, 640, 360, k);
                let a = narrow.encode_bgra(&buf, k == 0).unwrap();
                let b = wide.encode_bgra(&buf, k == 0).unwrap();
                let (qa, qb) = (rn.read(&a, true), rw.read(&b, true));
                assert!(qa.0.is_some(), "setting {base}, frame {k}: unreadable");
                assert_eq!(qa, qb, "setting {base}, frame {k}");
            }
            for (i, q) in [QP_LINK_MAX.max(base), top, base].into_iter().enumerate() {
                assert!(wide.set_qp(q).unwrap());
                fill(&mut buf, 640, 360, 3 + i as u32);
                let au = wide.encode_bgra(&buf, false).unwrap();
                assert_eq!(
                    rw.read(&au, true),
                    (Some(q), false),
                    "setting {base}, QP {q}"
                );
            }
        }
    }

    #[test]
    fn a_repeat_at_the_users_qp_refines_a_coarse_still_picture() {
        let mut buf = vec![0u8; 640 * 360 * 4];
        fill(&mut buf, 640, 360, 3);
        let mut e = match x264_open(cfg(15), true) {
            Ok(e) => e,
            Err(e) => return crate::test_support::skip("x264 refinement", e),
        };
        e.encode_bgra(&buf, true).unwrap();
        e.set_qp(40).unwrap();
        e.encode_bgra(&buf, false).unwrap();
        // coarse content now: what a still screen would keep without a refinement
        let mut coarse = vec![0u8; 640 * 360 * 4];
        fill(&mut coarse, 640, 360, 9);
        e.set_qp(40).unwrap();
        e.encode_bgra(&coarse, false).unwrap();
        let same = e.encode_repeat(false).unwrap().len();
        e.set_qp(15).unwrap();
        let refined = e.encode_repeat(false).unwrap().len();
        tprintln!("x264 still picture: repeat at QP 40 {same} B, at QP 15 {refined} B");
        assert!(refined > 4 * same, "{refined} vs {same}");
        match super::super::nvidia::encoder::Encoder::new(cfg(15)) {
            Ok(mut n) => {
                n.encode_bgra(&buf, true).unwrap();
                n.set_qp(40).unwrap();
                n.encode_bgra(&coarse, false).unwrap();
                let same = n.encode_repeat(false).unwrap().len();
                n.set_qp(15).unwrap();
                let refined = n.encode_repeat(false).unwrap().len();
                tprintln!("NVENC still picture: repeat at QP 40 {same} B, at QP 15 {refined} B");
                assert!(refined > 4 * same, "{refined} vs {same}");
            }
            Err(e) => crate::test_support::skip("NVENC refinement", e),
        }
    }

    /// Two pictures in flight on the async ring when the QP changes: they keep the QP they were
    /// submitted with, the next one carries the new one, and nothing becomes a keyframe.
    fn nvenc_set_qp_in_flight(ltr: bool) {
        use super::super::nvidia::encoder::{
            fill_synthetic_bgra, Encoder, KEY_ENCODER, KEY_WRITER,
        };
        use windows::core::Interface as _;
        use windows::Win32::Graphics::Direct3D11::{ID3D11Device1, ID3D11Texture2D};
        use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
        let (w, h) = (640u32, 360u32);
        let mut enc = match Encoder::new_shared_ring(cfg(15), 3) {
            Ok(e) => e,
            Err(e) => return crate::test_support::skip("NVENC in-flight set_qp", e),
        };
        if enc.pipeline_depth() < 3 {
            return crate::test_support::skip_because(
                "NVENC in-flight set_qp",
                "this engine runs synchronously, so nothing is ever in flight",
            );
        }
        if ltr && enc.ltr_slots() == 0 {
            return crate::test_support::skip_because(
                "NVENC in-flight set_qp with LTR",
                "the encoder opened without long-term references",
            );
        }
        let device: ID3D11Device1 = enc.device().cast().unwrap();
        let context = unsafe { enc.device().GetImmediateContext() }.unwrap();
        let staged: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)> = enc
            .shared_handles()
            .into_iter()
            .map(|handle| {
                let tex: ID3D11Texture2D = unsafe { device.OpenSharedResource1(handle) }.unwrap();
                let mutex: IDXGIKeyedMutex = tex.cast().unwrap();
                (tex, mutex)
            })
            .collect();
        let mut frame = vec![0u8; (w * h * 4) as usize];
        let mut stage = |slot: usize, i: u32| {
            fill_synthetic_bgra(&mut frame, w, h, i);
            let (tex, mutex) = &staged[slot];
            crate::windows_utils::streamer::keyed::acquire(mutex, KEY_WRITER, 1000).unwrap();
            unsafe {
                context.UpdateSubresource(
                    tex,
                    0,
                    None,
                    frame.as_ptr() as *const std::ffi::c_void,
                    w * 4,
                    0,
                );
                context.Flush();
                mutex.ReleaseSync(KEY_ENCODER).unwrap();
            }
        };
        let mut reader = SliceQp::default();
        let mut round =
            |enc: &mut Encoder, first: u32, qp: Option<u8>| -> Vec<(Option<u8>, bool)> {
                stage(0, first);
                enc.submit_slot(0, first == 0).unwrap();
                stage(1, first + 1);
                enc.submit_slot(1, false).unwrap();
                if let Some(q) = qp {
                    assert!(enc.set_qp(q).unwrap());
                }
                stage(2, first + 2);
                enc.submit_slot(2, false).unwrap();
                (0..3)
                    .map(|slot| {
                        let au = enc.complete_slot(slot).unwrap();
                        reader.read(&au, true)
                    })
                    .collect()
            };
        crate::test_support::show_measurements();
        round(&mut enc, 0, None);
        let r = round(&mut enc, 3, Some(30));
        tprintln!(
            "NVENC in flight (async, depth {}, LTR slots {}): QP 30 set with two in flight; read back {:?}",
            enc.pipeline_depth(),
            enc.ltr_slots(),
            r
        );
        assert_eq!(
            r,
            vec![(Some(15), false), (Some(15), false), (Some(30), false)]
        );
        let r = round(&mut enc, 6, Some(15));
        tprintln!("NVENC in flight: QP 15 set with two in flight; read back {r:?}");
        assert_eq!(
            r,
            vec![(Some(30), false), (Some(30), false), (Some(15), false)]
        );
    }

    #[test]
    fn nvenc_set_qp_with_pictures_in_flight() {
        nvenc_set_qp_in_flight(false);
    }

    /// Run with `SE_NVENC_LTR=1` (it is read when the encoder opens); otherwise it says so.
    #[test]
    fn nvenc_set_qp_with_pictures_in_flight_and_long_term_references() {
        if crate::envcfg::var("SE_NVENC_LTR").as_deref() != Ok("1") {
            return crate::test_support::skip_because(
                "NVENC in-flight set_qp with LTR",
                "run with SE_NVENC_LTR=1",
            );
        }
        nvenc_set_qp_in_flight(true);
    }

    // each encoder: set_qp reaches the bitstream from the next frame, with no keyframe

    /// Static text-like rows with a band of fresh noise that moves every frame, so a P frame
    /// carries real residual and its size follows the QP.
    fn fill(buf: &mut [u8], w: u32, h: u32, f: u32) {
        let (w, h) = (w as usize, h as usize);
        let band = (f as usize * h / 16) % h;
        let mut r = Rng(0x1234_5678 ^ ((f as u64) << 20));
        for y in 0..h {
            let fresh = (y + h - band) % h < h / 8;
            for x in 0..w {
                let v = if fresh {
                    (r.next() * 255.0) as u8
                } else if (y / 12).is_multiple_of(2) && (x / 3 + y / 12) % 5 != 0 {
                    20
                } else {
                    235
                };
                let i = (y * w + x) * 4;
                buf[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
    }

    type EncodeFn<'a> = dyn FnMut(&[u8], Option<u8>) -> Result<Vec<u8>> + 'a;

    /// Encodes 40 frames, moving the QP at 10, 20 and 30, and checks every frame after the first
    /// against what was set before it. Prints bytes per QP so the six-steps-halve model can be
    /// read against this encoder.
    fn qp_follows(what: &str, base: u8, enc: &mut EncodeFn<'_>) {
        crate::test_support::show_measurements();
        let (w, h) = (640u32, 360u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut reader = SliceQp::default();
        let plan = |k: u32| match k {
            10 => Some(30),
            20 => Some(22),
            30 => Some(51),
            40 => Some(base),
            _ => None,
        };
        let mut want = base;
        let mut sizes: Vec<(u8, usize)> = Vec::new();
        for k in 0..50u32 {
            fill(&mut buf, w, h, k);
            let set = plan(k);
            if let Some(q) = set {
                want = q;
            }
            let au = enc(&buf, set).unwrap_or_else(|e| panic!("{what}: frame {k}: {e:#}"));
            let (qp, idr) = reader.read(&au, true);
            assert_eq!(idr, k == 0, "{what}: frame {k} keyframe={idr}");
            if k > 0 {
                assert_eq!(qp, Some(want), "{what}: frame {k}");
                if k % 10 > 1 {
                    sizes.push((want, au.len()));
                }
            }
        }
        let mean = |q: u8| {
            let v: Vec<f64> = sizes
                .iter()
                .filter(|s| s.0 == q)
                .map(|s| s.1 as f64)
                .collect();
            v.iter().sum::<f64>() / v.len().max(1) as f64
        };
        let (a, b) = (mean(22), mean(30));
        tprintln!(
            "{what}: P frame bytes at QP {base} {:.0}, 22 {a:.0}, 30 {b:.0}: six QP steps divide \
             the bits by {:.2}",
            mean(base),
            (a / b).powf(6.0 / 8.0)
        );
    }

    fn cfg(qp: u8) -> EncoderConfig {
        EncoderConfig {
            width: 640,
            height: 360,
            fps: 60,
            bitrate_bps: 6_000_000,
            max_bitrate_bps: 6_000_000,
            profile: crate::streamer::config::H264Profile::High,
            qp: Some(qp),
            min_qp: None,
            intra_refresh: false,
        }
    }

    #[test]
    fn x264_set_qp_reaches_the_next_frame_without_a_keyframe() {
        let mut e = match x264_open(cfg(15), true) {
            Ok(e) => e,
            Err(e) => return crate::test_support::skip("x264 set_qp read-back", e),
        };
        qp_follows("x264", 15, &mut |buf, q| {
            if let Some(q) = q {
                assert!(e.set_qp(q)?);
            }
            e.encode_bgra(buf, false)
        });
    }

    #[test]
    fn nvenc_set_qp_reaches_the_next_frame_without_a_keyframe() {
        let mut e = match super::super::nvidia::encoder::Encoder::new(cfg(15)) {
            Ok(e) => e,
            Err(e) => return crate::test_support::skip("NVENC set_qp read-back", e),
        };
        qp_follows("nvenc", 15, &mut |buf, q| {
            if let Some(q) = q {
                assert!(e.set_qp(q)?);
            }
            e.encode_bgra(buf, false)
        });
    }

    #[test]
    fn quick_sync_set_qp_reaches_the_next_frame_without_a_keyframe() {
        let mut e = match super::super::intel::encoder::Encoder::new(cfg(15)) {
            Ok(e) => e,
            Err(e) => return crate::test_support::skip("Quick Sync set_qp read-back", e),
        };
        qp_follows("quick sync", 15, &mut |buf, q| {
            if let Some(q) = q {
                assert!(e.set_qp(q)?);
            }
            e.encode_bgra(buf, false)
        });
    }

    #[test]
    fn set_qp_is_refused_outside_constant_qp() {
        let config = EncoderConfig {
            qp: None,
            ..cfg(18)
        };
        match super::super::x264::encoder::X264Encoder::new(config) {
            Ok(mut e) => assert!(!e.set_qp(30).unwrap()),
            Err(e) => crate::test_support::skip("x264 set_qp refusal", e),
        }
    }

    #[test]
    fn the_reader_stops_at_the_first_slice_behind_either_start_code() {
        // an access unit delimiter, an unparseable SPS, then an IDR slice: the slice is found
        // behind a four-byte start code and nothing after its first bytes is looked at
        let au = [
            0, 0, 0, 1, 9, 0xf0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x65, 0x88,
        ];
        assert_eq!(find_start(&au, 0), Some(4));
        assert_eq!(find_start(&au, 4), Some(9));
        assert_eq!(find_start(&au, 9), Some(16));
        let mut r = SliceQp::default();
        assert_eq!(r.read(&au, true), (None, true));
        assert!(!r.ready());
        let p = [0, 0, 1, 0x41, 0x9a];
        assert_eq!(r.read(&p, true), (None, false));
    }

    #[test]
    fn the_bit_reader_drops_emulation_prevention_bytes() {
        // 00 00 03 01 reads as 00 00 01
        let mut r = Bits::new(&[0, 0, 3, 1]);
        assert_eq!(r.bits(24), Some(1));
        assert_eq!(r.bit(), None);
    }
}

//! What a constant-QP session does when the link cannot carry it: picture quality first, the frame
//! rate only after that, and never a queue.
//!
//! The transport's bitrate target cannot shrink a constant-QP stream, so [`CeilingGovernor`] (a
//! fixed ceiling, the macOS `QpGovernor`'s formula and constants) and [`LinkGovernor`] (a standing
//! queue or a delivered rate far under the sent one, read from transport-wide feedback) raise the
//! QP instead. A bitrate estimate is never evidence, and loss with no queue behind it is Wi-Fi,
//! not congestion. At [`QP_LINK_MAX`] frames are gated to a divisor of the rate, a halving at a
//! time, and recovery undoes the divisor first. With `link_pace` (on under the link governor)
//! the divisor gives way to pacing: once the link is judged too slow, everything sent to the
//! viewer goes out at a share of what the link delivered, frames are skipped at the sender
//! against what waits there instead of being dropped in the network, keyframes are paced and
//! coarse, and recovery ramps the pace up and the QP down. While the link side is engaged,
//! keyframes are sized to the link and spaced out ([`KeyPolicy`]), except the ones a stuck
//! decoder needs.
//!
//! The link side ships on (`link_governor`) and does nothing until the link is shown to be the
//! limit: a shortfall that lasts, with a queue or stretched arrivals behind it, never loss or
//! jitter alone. `qp_ceiling_governor` and `client_pace_governor` ship off.

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

/// The highest QP the link side asks for before it gates the frame rate. Quality goes before
/// frames: by 38 to 40 one-pixel stems break up and small text goes soft, but large text and the
/// layout still read, and the six steps from 34 halve the bits again, which is as much as
/// quartering the frame rate buys (a picture that stands for four periods carries twice the
/// change). Past 40 text stops reading at all, so the frame rate goes next.
pub(crate) const QP_LINK_MAX: u8 = 40;
/// 60 -> 30 -> 20 -> 15 at the most.
pub(crate) const MAX_DIVISOR: u32 = 4;
/// With the frame rate at a quarter and the link still not carrying the stream, the QP may go
/// past `QP_LINK_MAX` up to here, six steps a move. Recovery takes it back first. Only a capture
/// that holds gated frames on its trailing edge gets here; any other stops at `QP_LINK_MAX`.
const QP_LAST_RESORT: u8 = 51;
const LAST_RESORT_STEP: f64 = 6.0;
/// A keyframe asked for while the governor is engaged is encoded at least this much coarser: it
/// is the largest thing the stream sends.
pub(crate) const KEYFRAME_BOOST: u8 = 6;
/// A keyframe on a governed link is never coded past this: the frame bound plus one boost. Past
/// it the picture the following frames predict from is too coarse to be worth the bytes saved.
pub(crate) const KEY_QP_MAX: u8 = QP_LINK_MAX + KEYFRAME_BOOST;
/// What a keyframe may take of the link at once: 80 ms of the rate it delivered, under the
/// shallowest buffer the stream was measured against (a 100 ms drop-tail queue), so a burst
/// queues instead of overflowing.
const KEY_BURST_S: f64 = 0.08;
/// While paced, what a keyframe may take of the link: a quarter second of the pace. Nothing is
/// lost to its size any more, but every frame behind it waits it out.
const KEY_PACED_S: f64 = 0.25;
/// While the link side is engaged, a keyframe nobody on the page asked for waits this long after
/// the last one at least: the widest window the PLI back-off opens without evidence. Loss opens
/// that window, so on a congested link every periodic PLI would otherwise be answered.
const KEY_GAP_MIN_NS: u64 = 2_000 * MS;
/// ... and no longer than this, whatever the last keyframe cost to drain.
const KEY_GAP_MAX_NS: u64 = 4_000 * MS;
/// The share of the link the governed stream leaves free (`1 - TARGET_MAX`): what drains a
/// keyframe's burst. The gap is twice that drain.
const KEY_DRAIN_SHARE: f64 = 1.0 - TARGET_MAX;
/// The frames after a coarse keyframe come back to the governed QP this many steps a frame: a
/// still region a finer P frame refines costs almost a keyframe of its own, spread this way over
/// a few frames instead of landing in the next one.
const KEY_RETURN_STEP: u8 = 3;
/// How a keyframe's size answers its QP, until two keyframes have shown it: weaker than a P
/// frame's (all intra, no skipped blocks).
const KEY_HALVING: f64 = 8.0;
const KEY_HALVING_MAX: f64 = 16.0;

/// Pacing to the link (`link_pace`): once the link is judged too slow, everything this viewer is
/// sent goes out at this share of what the link delivered, so the bottleneck queue never fills
/// and drops nothing; what the pacer cannot pass waits at the sender, and frames are skipped
/// there instead of being lost in the network.
pub(crate) const PACE_SHARE: f64 = 0.9;
/// The pacing rate never goes under this, so a collapsed reading cannot starve the session.
const PACE_MIN_BPS: f64 = 300_000.0;
/// Paced, the QP rises once this share of frames waits at the sender, by two to four steps
/// against how many wait, and the next raise waits this long: the skip share is read over half
/// a second, so a shorter hold raises again on frames that waited before the last raise.
const SKIP_RAISE_SHARE: f64 = 0.05;
const SKIP_RAISE_HOLD_NS: u64 = 600 * MS;
/// After a raise that frames waiting forced, the QP is not lowered for this long; after that, a
/// step down needs this long with nothing waiting and at the same QP, its heaviest half second
/// under the pace with room, and goes at most `LOWER_STEP_MAX` steps on the strongest response.
const LOWER_AFTER_RAISE_NS: u64 = 6_000 * MS;
const LOWER_EVIDENCE_NS: u64 = 2_000 * MS;
const LOWER_STEP_MAX: u8 = 2;
/// Paced at the bound, half the frames waiting lets the QP past it, toward `QP_LAST_RESORT`
/// (the frame rate is already halved by then); under a fifth waiting takes it back.
const LAST_RESORT_SKIP: f64 = 0.5;
const LAST_RESORT_LOWER_SKIP: f64 = 0.2;
/// ... held this long, and not in the first of it after pacing began, when what waits is the
/// engagement's own backlog; back a step every `LAST_RESORT_LOWER_NS`.
const LAST_RESORT_AFTER_NS: u64 = 1_500 * MS;
const LAST_RESORT_LOWER_NS: u64 = 1_000 * MS;
/// QP steps per halving the encoded rate is carried across QPs with while paced: between the
/// textbook's 6 and the 12 a raise assumes (the live content halved its bits every 10), so a
/// rate carried to a lower QP comes out a little high.
const FIT_HALVING: f64 = 9.0;
/// Recovery while paced is a ramp: every `RAMP_STEP_NS` the QP comes down `RAMP_QP_STEP` (one
/// more under `RAMP_ROOMY` of the pace) while the stream leaves room under the pace, and the pace
/// goes up while the stream fills `RAMP_FILLED` of it, by `RAMP_GAIN_NEAR` until it is
/// `RAMP_NEAR` over where it began and by `RAMP_GAIN` past that. Nothing depends on the content
/// filling a probe: a light stream walks the QP down, a heavy one walks the pace up, and with a
/// fifth of the frames waiting (`RAMP_SKIP`) the QP goes back up a step while the pace still
/// climbs.
const RAMP_STEP_NS: u64 = 120 * MS;
const RAMP_GAIN_NEAR: f64 = 1.05;
const RAMP_NEAR: f64 = 1.3;
const RAMP_GAIN: f64 = 1.25;
const RAMP_QP_STEP: u8 = 3;
const RAMP_FILLED: f64 = 0.85;
const RAMP_ROOMY: f64 = 0.5;
const RAMP_SKIP: f64 = 0.2;
/// The ramp walks the QP down no further than this many steps under the QP the last seconds fit
/// the pace at: twice the bits of the heaviest of them on the live content's response.
const RAMP_SPAN: u8 = 10;
/// The sender's skip share covers this long: one back-up per window.
const SKIP_WINDOW_NS: u64 = 500 * MS;
/// The ramp ends at the first queue rise of this much over where it started, or any loss: 5 %
/// a step past a link still at its old pace is a 10 ms rise within two steps, and the queue it
/// leaves is a few tens of ms, under the shallowest buffer, so a failed ramp costs a little
/// queue and no packet.
const RAMP_QUEUE_US: i64 = 10_000;
/// The stream at the user's QP passes this long under the ramp with no queue and nothing waiting,
/// and the link has been shown to take the heaviest half second of the last `DEMAND_SPAN_NS`
/// carried to that QP: pacing lets go entirely.
const RELEASE_PASS_NS: u64 = 600 * MS;
const DEMAND_SPAN_NS: u64 = 8_000 * MS;
/// ... to within this share: carried down twenty steps at `FIT_HALVING`, a rate comes out a
/// fifth over what the live content's response gives.
const RELEASE_DEMAND: f64 = 0.9;
/// A ramp starts once the queue has been clear this long, and no sooner than the hold after the
/// last one: `RAMP_HOLD_MIN_NS`, doubling after each that found no more room, to the max.
const RAMP_QUIET_NS: u64 = 500 * MS;
const RAMP_HOLD_MIN_NS: u64 = 1_000 * MS;
const RAMP_HOLD_MAX_NS: u64 = 2_000 * MS;
/// A ramp that left the pace this much higher found room.
const RAMP_PROGRESS: f64 = 1.05;
/// A lossless queue within this long of a ramp is the ramp's own.
const RAMP_SETTLE_NS: u64 = 1_000 * MS;
/// The encoded bytes, in buckets this long, kept over the demand span.
const ENC_BUCKET_NS: u64 = 250 * MS;
const ENC_BUCKETS: usize = 36;
/// The encoder is aimed at this share of the pace: frame sizes vary by a third either side, and
/// the sender's backlog is allowed `BACKLOG_PERIODS` frame periods before a frame waits.
pub(crate) const SENDER_SHARE: f64 = 0.8;
pub(crate) const BACKLOG_PERIODS: u64 = 2;

/// Engagement (see `LinkGovernor::may_engage`): a second of send time taken whole, delivered
/// under 85 % of sent with a standing queue or arrivals stretched to under 85 % of the send
/// time, for a second without a break. Loss alone is never the evidence: at 15 % random loss
/// the shortfall is the loss itself. A cap at half the stream crosses the shortfall within half
/// a second of arriving, so engagement comes about 1.5 s after a cap where a single window's
/// verdict took 0.6 s: the price of never reacting to anything shorter.
const ENGAGE_SPAN_US: i64 = 1_000_000;
const ENGAGE_SHORTFALL: f64 = 0.85;
const ENGAGE_STRETCH: f64 = 0.85;
const ENGAGE_HOLD_NS: u64 = 1_000 * MS;
/// ... or a queue under every packet of the span for this long, unless it came up as a step in
/// the path's delay (see `LinkGovernor::route_step`).
const ENGAGE_STANDING_NS: u64 = 2_000 * MS;

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
/// A queue that came up within `STEP_WITHIN_NS` and has stood flat (within `FLAT_US`) ever since,
/// for at least `REBASE_AFTER_NS`, losing no more than `SHARE_STEP` lets through on average, is
/// the path's new base delay (a route or relay change, a cellular hop), not a queue: a drop-tail
/// queue that fills that fast is overfilled by far more than that, and loses it.
const FLAT_US: i64 = 10_000;
const REBASE_AFTER_NS: u64 = 1_500 * MS;
const SHARE_STEP: f64 = 0.95;
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
/// A lossless queue rising within this long of a keyframe is the keyframe's own burst.
const KEY_SETTLE_NS: u64 = 1_500 * MS;
/// The link's rate is read from the windows it limited over this long: during a loss burst
/// resends and the stream share the link and feedback shows only the stream, so a single window
/// reads as little as half of what the link carries.
const LINK_EST_NS: u64 = 1_000 * MS;
/// Link-limited readings kept: four seconds of reports, enough to reach back to a cap's onset
/// from the moment the engagement evidence is complete.
const LINK_HIST_KEEP: usize = 80;
/// Paced, a window that arrived at this share of its send rate was carried: it says nothing
/// against the pace, whatever the verdict made of a queue still draining from before.
const PACE_CARRIED: f64 = 0.95;
/// For this long after pacing begins, what is reported was sent before it, into the queue and
/// the resends the engagement found: it does not lower the pace.
const PACE_SETTLE_NS: u64 = 1_000 * MS;
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

/// An attempt on the link while paced (see `LinkGovernor::ramp_step`).
#[derive(Clone, Copy, Debug)]
struct Ramp {
    from_pace: f64,
    from_q: u8,
    /// The queue when it began, and when the last step went.
    base_us: i64,
    stage_ns: u64,
    /// The most the link delivered under it with no queue rise, and whether one rose.
    carried: f64,
    rose: bool,
    since_ns: u64,
    /// When the QP last changed before it, and when it last backed the QP up.
    q_changed_ns: u64,
    backed_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    NoData,
    Congested,
    /// A standing queue, not proven congestion: holds a ramp back.
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
    /// Whether the QP may go past the bound once the frame rate is at a quarter.
    last_resort: bool,
    /// Pace the sender to the link instead of cutting the frame rate (`link_pace`).
    pacing: bool,
    /// The rate the sender is paced at; 0 while nothing is paced.
    pace_bps: f64,
    /// Share of frames the sender skipped over the last half second.
    skip_share: f64,
    /// Since when the engagement evidence has held without a break.
    short_since_ns: Option<u64>,
    standing_since_ns: Option<u64>,
    /// The best reading of the link among the windows that made the engagement evidence: the
    /// earliest of them, before the resends the loss calls for fill the link, read it best.
    engage_link_bps: f64,
    paced_since_ns: u64,
    pub(crate) engagements: u64,
    pub(crate) engaged_ns: u64,
    pub(crate) last_engagement: String,
    sender_waiting: bool,
    ramp: Option<Ramp>,
    ramp_hold_ns: u64,
    ramp_not_before_ns: u64,
    ramp_ended_ns: u64,
    lower_not_before_ns: u64,
    /// The last tick any frame, or a fifth of them, waited at the sender.
    last_skip_ns: u64,
    last_heavy_skip_ns: u64,
    half_waiting_since_ns: Option<u64>,
    /// Since when the stream at the user's QP has passed with nothing waiting.
    pass_since_ns: Option<u64>,
    q_changed_ns: u64,
    /// Encoded bytes per `ENC_BUCKETS_NS`: bucket + 1, the bytes carried to the user's QP frame by
    /// frame, the highest QP in it, and whether frames waited at the sender then (what was
    /// encoded is what the pace let through).
    enc_buckets: [(u64, f64, u8, bool); ENC_BUCKETS],
    /// Ramps begun.
    pub(crate) probes: u64,
    samples: VecDeque<(u64, PathSample)>,
    /// Per-second minimum one-way delay, `(second + 1, min)`; 0 is empty.
    base: [(u64, i64); BASE_BUCKETS],
    /// `(when, queue, byte share)` of every valid newest window.
    queue_hist: VecDeque<(u64, i64, f64)>,
    /// `(when, delivered)` of the windows the link limited (a queue stood or something was lost):
    /// only those say what the link carries.
    link_hist: VecDeque<(u64, f64)>,
    /// A keyframe went out until here: a lossless queue it builds is its own, not congestion.
    key_settle_ns: u64,
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
            last_resort: true,
            pacing: false,
            pace_bps: 0.0,
            skip_share: 0.0,
            short_since_ns: None,
            standing_since_ns: None,
            engage_link_bps: 0.0,
            paced_since_ns: 0,
            engagements: 0,
            engaged_ns: 0,
            last_engagement: String::new(),
            sender_waiting: false,
            ramp: None,
            ramp_hold_ns: RAMP_HOLD_MIN_NS,
            ramp_not_before_ns: 0,
            ramp_ended_ns: 0,
            lower_not_before_ns: 0,
            last_skip_ns: 0,
            last_heavy_skip_ns: 0,
            half_waiting_since_ns: None,
            pass_since_ns: None,
            q_changed_ns: 0,
            enc_buckets: [(0, 0.0, 0, false); ENC_BUCKETS],
            probes: 0,
            samples: VecDeque::with_capacity(SAMPLE_KEEP),
            base: [(0, 0); BASE_BUCKETS],
            queue_hist: VecDeque::with_capacity(64),
            link_hist: VecDeque::with_capacity(LINK_HIST_KEEP),
            key_settle_ns: 0,
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

    /// Whether the QP may go past the bound at a quarter of the frames (see `QP_LAST_RESORT`).
    pub(crate) fn last_resort(&mut self, yes: bool) {
        self.last_resort = yes;
    }

    /// What the link delivered when it was last judged congested, or the newest window's rate
    /// when it never was; 0 when nothing is known.
    pub(crate) fn link_bps(&self) -> f64 {
        if self.pace_bps > 0.0 {
            self.pace_bps / PACE_SHARE
        } else if self.cap_bps > 0.0 {
            self.cap_bps
        } else if self.last.valid {
            self.last.delivered_bps
        } else {
            0.0
        }
    }

    /// The encoder reaches no higher than `top`: the bound follows it.
    pub(crate) fn limit_qp(&mut self, top: u8) {
        self.q_max = self.q_max.min(top).max(self.q_user);
    }

    /// The link side itself has cut something (the ceiling's raises do not count).
    pub(crate) fn engaged(&self) -> bool {
        self.q > self.q_user || self.div > 1 || self.pace_bps > 0.0
    }

    /// Pace the sender to the link once it is judged too slow, instead of the frame divisor and
    /// the QP past the bound.
    pub(crate) fn set_pacing(&mut self, yes: bool) {
        self.pacing = yes;
        if !yes {
            self.pace_bps = 0.0;
        }
    }

    /// The rate the sender is paced at; 0 when it is not.
    pub(crate) fn pace_bps(&self) -> f64 {
        self.pace_bps
    }

    /// An attempt on the link is in progress.
    pub(crate) fn attempting(&self) -> bool {
        self.ramp.is_some()
    }

    /// The share of frames the sender skipped over the last half second (keyframe drains left
    /// out), and whether a keyframe is draining at the pace right now.
    pub(crate) fn sender(&mut self, skip_share: f64, draining: bool) {
        self.skip_share = skip_share.clamp(0.0, 1.0);
        self.sender_waiting = draining;
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
        if keyframe {
            self.key_settle_ns = now_ns + KEY_SETTLE_NS;
        }
        if !keyframe {
            if self.enc.len() == self.enc.capacity() {
                if let Some((_, b)) = self.enc.pop_front() {
                    self.enc_bytes -= b;
                }
            }
            self.enc.push_back((now_ns, bytes));
            self.enc_bytes += bytes;
            let id = now_ns / ENC_BUCKET_NS + 1;
            let slot = &mut self.enc_buckets[(id as usize) % ENC_BUCKETS];
            if slot.0 != id {
                *slot = (id, 0.0, 0, false);
            }
            slot.1 +=
                bytes as f64 * 2f64.powf(self.q.saturating_sub(self.q_user) as f64 / FIT_HALVING);
            slot.2 = slot.2.max(self.q);
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
        Self::window_over(it, base, EVAL_SPAN_US)
    }

    fn window_over<'a>(
        it: &mut impl Iterator<Item = &'a PathSample>,
        base: Option<i64>,
        span_us: i64,
    ) -> Window {
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
            if send.0 != i64::MAX && send.1 - send.0 >= span_us {
                break;
            }
        }
        let send_span = send.1.saturating_sub(send.0);
        let arr_span = arr.1.saturating_sub(arr.0);
        if reported < MIN_PACKETS
            || matched < 2
            || sent == 0
            || send_span < span_us
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

    /// The queue at `q` came up as a step and has stood flat since: it was under `QUEUE_LOW_US`,
    /// reached `q` within `STEP_WITHIN_NS`, and every window since is within `FLAT_US` of it and
    /// lost little on average. However old the step is, a queue that crept up is not one.
    fn route_step(&self, q: i64) -> bool {
        let Some(low) = self
            .queue_hist
            .iter()
            .rposition(|(_, x, _)| *x < QUEUE_LOW_US)
        else {
            return false;
        };
        let t_low = self.queue_hist[low].0;
        let Some(up) = self
            .queue_hist
            .iter()
            .skip(low + 1)
            .position(|(_, x, _)| (x - q).abs() < FLAT_US)
            .map(|i| low + 1 + i)
        else {
            return false;
        };
        if self.queue_hist[up].0.saturating_sub(t_low) > STEP_WITHIN_NS {
            return false;
        }
        let (mut n, mut share) = (0u32, 0.0);
        for (_, x, s) in self.queue_hist.iter().skip(up) {
            if (x - q).abs() >= FLAT_US {
                return false;
            }
            n += 1;
            share += s;
        }
        n > 0 && share / n as f64 >= SHARE_STEP
    }

    /// Flat for `REBASE_AFTER_NS`.
    fn flat(&self, now_ns: u64, q: i64) -> bool {
        let since = now_ns.saturating_sub(REBASE_AFTER_NS);
        self.queue_hist.front().is_some_and(|(t, _, _)| *t <= since)
            && self
                .queue_hist
                .iter()
                .filter(|(t, _, _)| *t >= since)
                .all(|(_, x, _)| (x - q).abs() < FLAT_US)
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
        // a queue, not loss alone: a window across the moment a cap arrives loses packets and
        // carries ones that arrived at the old rate
        if self.pacing && a.queue_us >= QUEUE_LOW_US {
            if self.link_hist.len() >= LINK_HIST_KEEP {
                self.link_hist.pop_front();
            }
            self.link_hist.push_back((now_ns, a.delivered_bps));
        }
        if let Some((at, _, enc_then, delivered_then)) = self.undo {
            let age = now_ns.saturating_sub(at);
            let enc = self.enc_rate(now_ns);
            if age > UNDO_WITHIN_NS {
                self.undo = None;
            } else if age >= REBASE_AFTER_NS
                && a.queue_us >= QUEUE_LOW_US
                && self.flat(now_ns, a.queue_us)
                && enc <= UNDO_RATE * enc_then
                && enc <= UNDO_UNDER * delivered_then
            {
                return Verdict::NotAQueue;
            }
        }
        let both = |f: &dyn Fn(&Window) -> bool| f(a) && b.valid && f(b);
        let overflow = both(&|w| w.queue_us > QUEUE_HIGH_US && w.share < SHARE_OVERFLOW);
        let severe = both(&|w| w.share < SHARE_SEVERE);
        // a keyframe's own burst queues on any path and drains by itself; with nothing lost it
        // is not the link (a relay's per-packet delay put 67 ms behind one on a jittery link)
        let own_burst = now_ns < self.key_settle_ns && both(&|w| w.share >= SHARE_CLEAN);
        if (!own_burst && (self.growing(now_ns, a.queue_us) || self.creeping(now_ns, a.queue_us)))
            || overflow
            || severe
        {
            return Verdict::Congested;
        }
        // (a step right behind our own move, or behind our own keyframe while a coarse one may
        // drain at a slow link's pace, is ours, not a new path; unengaged, the periodic PLI's
        // keyframes would otherwise hold the rebase off until the step is too old to tell)
        if a.queue_us >= QUEUE_LOW_US
            && (!self.engaged() || now_ns >= self.key_settle_ns)
            && now_ns.saturating_sub(self.last_move_ns) >= REBASE_AFTER_NS + STEP_WITHIN_NS
            && (self.last_evidence_ns == 0
                || now_ns.saturating_sub(self.last_evidence_ns) >= CAP_MEMORY_NS)
            && self.flat(now_ns, a.queue_us)
            && self.route_step(a.queue_us)
        {
            // a queue that stepped up and stands flat with little lost is the path's new base
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
            if self.engaged() {
                self.engaged_ns += now_ns.saturating_sub(last);
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
                if let Some(r) = self.ramp.as_mut() {
                    if a.queue_us >= r.base_us + RAMP_QUEUE_US || a.share < SHARE_CLEAN {
                        r.rose = true;
                    } else {
                        r.carried = r.carried.max(a.delivered_bps);
                    }
                }
            }
            let verdict = self.judge(now_ns, &a, &b);
            let was_engaged = self.engaged();
            let verdict = if was_engaged {
                verdict
            } else if self.may_engage(now_ns, verdict) {
                Verdict::Congested
            } else if verdict == Verdict::Congested {
                // a window's verdict alone never engages: that takes the sustained evidence
                Verdict::Queued
            } else {
                verdict
            };
            // a lossless queue under or right after our own ramp is the ramp's: it ends the ramp
            // and nothing else
            let own = verdict == Verdict::Congested
                && self.pace_bps > 0.0
                && a.share >= SHARE_CLEAN
                && (self.ramp.is_some()
                    || now_ns.saturating_sub(self.ramp_ended_ns) < RAMP_SETTLE_NS);
            if own {
                self.clear_since_ns = None;
                return self.end_ramp(now_ns, "a queue");
            }
            match verdict {
                Verdict::Congested => {
                    self.last_evidence_ns = now_ns;
                    self.clear_since_ns = None;
                    self.idle_since_ns = None;
                    if self.pacing {
                        self.pace_to(now_ns, &a);
                    }
                    let m = if now_ns >= self.hold_until_ns {
                        self.raise(now_ns, q_eff, a)
                    } else {
                        None
                    };
                    // an engagement counts once something was cut: a pace set or a QP raised
                    if !was_engaged && self.engaged() {
                        self.engagements += 1;
                    }
                    return m;
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
        if self.pacing && self.pace_bps > 0.0 {
            return self.paced(now_ns);
        }
        self.step_down(now_ns)
    }

    /// Whether the link side may engage, asked on every judged report while it is not engaged.
    ///
    /// The link is the limit when, over the last `ENGAGE_SPAN_US` of send time taken whole, the
    /// rate that arrived is under `ENGAGE_SHORTFALL` of the rate sent, and that shortfall comes
    /// with what only a bottleneck produces: a queue standing under every packet of the span (its
    /// lowest one-way delay `QUEUE_HIGH_US` over the base), or under every packet of the newest
    /// judged window when that queue did not come up as a step in the path's delay, or arrivals
    /// stretched to under `ENGAGE_STRETCH` of the send time, and all of that has held without a
    /// break for
    /// `ENGAGE_HOLD_NS`. Loss, however heavy, shortens what arrives and stretches nothing. A queue
    /// standing for `ENGAGE_STANDING_NS` engages too, unless it is a step in the path's delay
    /// (`route_step`), and so does a slowly filling one (the creep rule, itself three seconds of
    /// steady rise). Over a whole second, jitter, a stall or a keyframe's burst neither stretch
    /// arrivals by 15 % nor leave a queue under every packet, and none lasts a second more.
    fn may_engage(&mut self, now_ns: u64, verdict: Verdict) -> bool {
        let base = self.base_owd(now_ns);
        let mut it = self.samples.iter().rev().map(|(_, s)| s);
        let span = Self::window_over(&mut it, base, ENGAGE_SPAN_US);
        let short = span.valid
            && span.delivered_bps < ENGAGE_SHORTFALL * span.sent_bps
            && (span.queue_us >= QUEUE_HIGH_US
                || span.sigma < ENGAGE_STRETCH
                || (self.last.valid
                    && self.last.queue_us >= QUEUE_HIGH_US
                    && !self.route_step(self.last.queue_us)));
        if short {
            self.short_since_ns.get_or_insert(now_ns);
        } else {
            self.short_since_ns = None;
        }
        // a link just under the stream: no shortfall to speak of, but a queue under every packet
        // that has stood for seconds and did not come up as a step in the path's delay
        let newest = self.queue_hist.back().map_or(0, |(_, q, _)| *q);
        if span.valid && span.queue_us >= QUEUE_HIGH_US && !self.route_step(newest) {
            self.standing_since_ns.get_or_insert(now_ns);
        } else {
            self.standing_since_ns = None;
        }
        let standing = self
            .standing_since_ns
            .is_some_and(|t| now_ns.saturating_sub(t) >= ENGAGE_STANDING_NS);
        let creep = verdict == Verdict::Congested
            && self
                .queue_hist
                .back()
                .is_some_and(|(_, q, _)| self.creeping(now_ns, *q));
        let held = self
            .short_since_ns
            .is_some_and(|t| now_ns.saturating_sub(t) >= ENGAGE_HOLD_NS);
        if !held && !creep && !standing {
            return false;
        }
        let from = self
            .short_since_ns
            .into_iter()
            .chain(self.standing_since_ns)
            .min()
            .unwrap_or(now_ns)
            .saturating_sub(ENGAGE_SPAN_US as u64 * 1_000);
        self.engage_link_bps = self
            .link_hist
            .iter()
            .filter(|(t, _)| *t >= from)
            .map(|(_, d)| *d)
            .fold(0.0, f64::max);
        self.short_since_ns = None;
        self.standing_since_ns = None;
        self.last_engagement = if held {
            format!(
                "over {:.1} s the link delivered {:.2} of {:.2} Mbps sent, {:.1} % lost, a {:.0} ms \
                 queue under every packet, arrivals stretched to {:.2} of the send time, for \
                 {:.1} s without a break",
                ENGAGE_SPAN_US as f64 / 1e6,
                span.delivered_bps / 1e6,
                span.sent_bps / 1e6,
                span.loss * 100.0,
                span.queue_us as f64 / 1e3,
                span.sigma,
                ENGAGE_HOLD_NS as f64 / 1e9
            )
        } else if standing {
            format!(
                "a {:.0} ms queue stood under every packet for {:.0} s: a link just under the stream",
                span.queue_us as f64 / 1e3,
                ENGAGE_STANDING_NS as f64 / 1e9
            )
        } else {
            format!(
                "a queue rose steadily for {:.0} s to {:.0} ms: a link just under the stream",
                CREEP_NS as f64 / 1e9,
                self.queue_hist.back().map_or(0, |(_, q, _)| *q) as f64 / 1e3
            )
        };
        true
    }

    /// The link is too slow: pace to what it delivered. A ramp in progress ends where it is.
    fn pace_to(&mut self, now_ns: u64, a: &Window) {
        if self.ramp.is_some() {
            let _ = self.end_ramp(now_ns, "congestion");
        }
        self.ramp_not_before_ns = self.ramp_not_before_ns.max(now_ns + self.ramp_hold_ns);
        if self.pace_bps > 0.0
            && (a.delivered_bps >= PACE_CARRIED * a.sent_bps
                || now_ns.saturating_sub(self.paced_since_ns) < PACE_SETTLE_NS)
        {
            // the sender is what limits this window: the link carried it; or what is reported
            // was sent before the pace began, into the queue the engagement found
            return;
        }
        let since = now_ns.saturating_sub(LINK_EST_NS);
        let link = self
            .link_hist
            .iter()
            .filter(|(t, _)| *t >= since)
            .map(|(_, d)| *d)
            .fold(a.delivered_bps, f64::max)
            .max(std::mem::take(&mut self.engage_link_bps));
        if link > 0.0 {
            if self.pace_bps == 0.0 {
                self.paced_since_ns = now_ns;
                self.q_changed_ns = now_ns;
            }
            let pace = (PACE_SHARE * link).max(PACE_MIN_BPS);
            // the response to a link too slow for the pace only ever lowers it
            self.pace_bps = if self.pace_bps > 0.0 {
                self.pace_bps.min(pace)
            } else {
                pace
            };
        }
    }

    /// Encoded bits a second over the last `span_ns`, keyframes left out.
    fn enc_recent(&self, now_ns: u64, span_ns: u64) -> f64 {
        let from = now_ns.saturating_sub(span_ns);
        let bytes: u64 = self
            .enc
            .iter()
            .rev()
            .take_while(|(t, _)| *t >= from)
            .map(|(_, b)| *b)
            .sum();
        bytes as f64 * 8.0 / (span_ns as f64 / 1e9)
    }

    /// Every half second encoded from `from_ns` to `to_ns`: bits a second carried to the user's
    /// QP, the highest QP in it, and whether frames waited at the sender then.
    fn enc_halves(&self, from_ns: u64, to_ns: u64) -> impl Iterator<Item = (f64, u8, bool)> + '_ {
        let last = to_ns / ENC_BUCKET_NS + 1;
        let first = (from_ns / ENC_BUCKET_NS + 1).max(last.saturating_sub(ENC_BUCKETS as u64 - 2));
        let at = |id: u64| {
            let s = self.enc_buckets[(id as usize) % ENC_BUCKETS];
            (s.0 == id).then_some((s.1, s.2, s.3))
        };
        (first.max(1)..=last).filter_map(move |id| {
            let (b, q, w) = at(id)?;
            let (b2, q2, w2) = at(id + 1).unwrap_or((0.0, 0, false));
            let bps = (b + b2) * 8.0 / (2.0 * ENC_BUCKET_NS as f64 / 1e9);
            Some((bps, q.max(q2), w || w2))
        })
    }

    /// What the content asked for in its heaviest half second over `DEMAND_SPAN_NS`, carried to
    /// the user's QP.
    fn demand_at_user(&self, now_ns: u64) -> f64 {
        self.enc_halves(now_ns.saturating_sub(DEMAND_SPAN_NS), now_ns)
            .map(|(bps, _, _)| bps)
            .fold(0.0, f64::max)
    }

    /// The lowest QP at which every half second encoded over `DEMAND_SPAN_NS` up to `to_ns`,
    /// carried to it, fits `target`; `None` with nothing to go by.
    fn fit_q(&self, now_ns: u64, to_ns: u64, target: f64) -> Option<u8> {
        // where frames waited, the QP did not fit: it is a floor, whatever went through
        let need = self
            .enc_halves(now_ns.saturating_sub(DEMAND_SPAN_NS), to_ns)
            .filter(|(bps, _, _)| *bps > 0.0)
            .map(|(bps, q, waited)| {
                if waited {
                    q as f64
                } else {
                    self.q_user as f64 + FIT_HALVING * (bps / target).log2()
                }
            })
            .reduce(f64::max)?;
        Some((need.ceil().max(0.0) as u8).max(self.q_user))
    }

    /// Whether the stream at the user's QP has passed `RELEASE_PASS_NS` with nothing waiting at
    /// the sender, and the link has been shown to take what the content asked for lately.
    fn may_let_go(&mut self, now_ns: u64) -> bool {
        if self.q > self.q_user || self.skip_share > 0.0 || self.sender_waiting {
            self.pass_since_ns = None;
            return false;
        }
        let since = *self.pass_since_ns.get_or_insert(now_ns);
        now_ns.saturating_sub(since) >= RELEASE_PASS_NS
            && RELEASE_DEMAND * self.demand_at_user(now_ns) <= self.pace_bps
    }

    /// Pacing off and the user's QP: nothing of the episode is left.
    fn let_go(&mut self, now_ns: u64, why: String) -> LinkMove {
        self.q = self.q_user;
        self.div = 1;
        self.pace_bps = 0.0;
        self.ramp = None;
        self.ramp_hold_ns = RAMP_HOLD_MIN_NS;
        self.ramp_not_before_ns = 0;
        self.lower_not_before_ns = 0;
        self.pass_since_ns = None;
        self.hold_until_ns = 0;
        self.last_move_ns = now_ns;
        self.q_changed_ns = now_ns;
        self.last_was_raise = false;
        self.steps += 1;
        LinkMove {
            q: self.q,
            divisor: 1,
            raise: false,
            why,
        }
    }

    /// The ramp ends without letting go: paced at what the link took with no queue under it,
    /// and the QP that fitted before less what the pace gained. The next waits twice as long
    /// unless this one found more room.
    fn end_ramp(&mut self, now_ns: u64, cause: &str) -> Option<LinkMove> {
        let r = self.ramp.take()?;
        let tried = self.pace_bps;
        let pace = r.from_pace.max(PACE_SHARE * r.carried);
        let gained = pace > RAMP_PROGRESS * r.from_pace;
        let back = if gained {
            (FIT_HALVING * (pace / r.from_pace).log2()).floor() as u8
        } else {
            0
        };
        let from = self.q;
        // the QP the seconds before it fit at the new pace, never above the one it left: a
        // raise is for frames waiting to call for
        let fit = self
            .fit_q(now_ns, r.since_ns, pace * SENDER_SHARE)
            .unwrap_or(r.from_q.saturating_sub(back));
        self.q = fit.min(r.from_q).max(self.q_user);
        self.pace_bps = pace;
        self.ramp_hold_ns = if gained {
            RAMP_HOLD_MIN_NS
        } else {
            (self.ramp_hold_ns * 2).min(RAMP_HOLD_MAX_NS)
        };
        self.ramp_not_before_ns = now_ns + self.ramp_hold_ns;
        self.ramp_ended_ns = now_ns;
        self.pass_since_ns = None;
        // what waited at the sender during the ramp is its cost, not a reason to raise
        self.hold_until_ns = self.hold_until_ns.max(now_ns + SKIP_RAISE_HOLD_NS);
        self.last_move_ns = now_ns;
        self.q_changed_ns = if self.q == r.from_q {
            r.q_changed_ns
        } else {
            now_ns
        };
        self.last_was_raise = false;
        self.steps += 1;
        Some(LinkMove {
            q: self.q,
            divisor: self.div,
            raise: false,
            why: format!(
                "ramp ended by {cause} at {:.2} Mbps: paced at {:.2} (from {:.2}), QP {from} -> {}, \
                 next in {} ms",
                tried / 1e6,
                pace / 1e6,
                r.from_pace / 1e6,
                self.q,
                self.ramp_hold_ns / MS
            ),
        })
    }

    /// One step of the ramp: the QP down while the stream leaves room under the pace, the pace up
    /// while the stream fills it, until a queue rises or the stream at the user's QP passes.
    fn ramp_step(&mut self, now_ns: u64) -> Option<LinkMove> {
        let mut r = self.ramp?;
        if r.rose {
            return self.end_ramp(now_ns, "a queue");
        }
        if self.may_let_go(now_ns) {
            let why = format!(
                "the stream at the user's QP passed {} ms under a {:.2} Mbps ramp with no queue and \
                 nothing waiting: pacing off",
                RELEASE_PASS_NS / MS,
                self.pace_bps / 1e6
            );
            return Some(self.let_go(now_ns, why));
        }
        // a keyframe draining at the pace holds everything behind it: the ramp waits for it
        if self.sender_waiting || now_ns.saturating_sub(r.stage_ns) < RAMP_STEP_NS {
            return None;
        }
        r.stage_ns = now_ns;
        let recent = self.enc_recent(now_ns, 2 * RAMP_STEP_NS);
        // the pace goes up only once the link has delivered most of it with no queue: a step
        // taken on what the encoder offered alone could be several steps past the link by the
        // time a heavy second fills it
        let carried = r.carried >= RAMP_FILLED * self.pace_bps;
        let waiting = self.skip_share >= RAMP_SKIP;
        let back_up =
            waiting && self.q < r.from_q && now_ns.saturating_sub(r.backed_ns) >= SKIP_WINDOW_NS;
        if back_up {
            r.backed_ns = now_ns;
        }
        self.ramp = Some(r);
        if carried {
            self.pace_bps *= if self.pace_bps < RAMP_NEAR * r.from_pace {
                RAMP_GAIN_NEAR
            } else {
                RAMP_GAIN
            };
        }
        if back_up {
            // most frames waiting: back up a step, the pace still climbs on what goes through
            let from = self.q;
            self.q = (from + RAMP_QP_STEP).min(r.from_q);
            self.q_changed_ns = now_ns;
            self.last_move_ns = now_ns;
            return Some(LinkMove {
                q: self.q,
                divisor: self.div,
                raise: true,
                why: format!(
                    "ramp: {:.0} % of frames waiting under a {:.2} Mbps pace: QP {from} -> {}",
                    self.skip_share * 100.0,
                    self.pace_bps / 1e6,
                    self.q
                ),
            });
        }
        // no more than `RAMP_SPAN` under where the last seconds fit the pace, so an attempt on a
        // cap that stays dips the picture that far and no further; once the pace is well past
        // where it began with no queue, the cap has moved and nothing holds the QP up
        let floor = if self.pace_bps >= RAMP_NEAR * r.from_pace {
            self.q_user
        } else {
            self.fit_q(now_ns, now_ns, SENDER_SHARE * self.pace_bps)
                .map_or(self.q_user, |q| q.saturating_sub(RAMP_SPAN))
        };
        if self.q <= floor || recent >= RAMP_FILLED * self.pace_bps || self.skip_share > 0.0 {
            return None;
        }
        let step = if recent < RAMP_ROOMY * self.pace_bps {
            RAMP_QP_STEP + 1
        } else {
            RAMP_QP_STEP
        };
        let from = self.q;
        self.q = from.saturating_sub(step).max(floor);
        self.q_changed_ns = now_ns;
        self.last_move_ns = now_ns;
        Some(LinkMove {
            q: self.q,
            divisor: self.div,
            raise: false,
            why: format!(
                "ramp: {:.2} Mbps encoded under a {:.2} Mbps pace, no queue: QP {from} -> {}",
                recent / 1e6,
                self.pace_bps / 1e6,
                self.q
            ),
        })
    }

    /// While paced: hold the QP the heavy stretches fit with room and keep every frame, raise it
    /// at once when frames wait, lower it a step at a time on seconds of evidence, and look for
    /// a faster link with a ramp.
    fn paced(&mut self, now_ns: u64) -> Option<LinkMove> {
        let skip = self.skip_share;
        if skip > 0.0 {
            self.last_skip_ns = now_ns;
        }
        // (frames that wait within a skip window of a QP change waited at the QP before it)
        if skip >= SKIP_RAISE_SHARE && now_ns.saturating_sub(self.q_changed_ns) >= SKIP_WINDOW_NS {
            let id = now_ns / ENC_BUCKET_NS + 1;
            let slot = &mut self.enc_buckets[(id as usize) % ENC_BUCKETS];
            if slot.0 == id {
                slot.3 = true;
            }
        }
        if skip >= LAST_RESORT_LOWER_SKIP {
            self.last_heavy_skip_ns = now_ns;
        }
        if skip < LAST_RESORT_SKIP {
            self.half_waiting_since_ns = None;
        } else if self.q >= self.q_max {
            self.half_waiting_since_ns.get_or_insert(now_ns);
        }
        if self.ramp.is_some() {
            return self.ramp_step(now_ns);
        }
        let target = self.pace_bps * SENDER_SHARE;
        // past the bound only with half the frames waiting there, a few steps at a time
        let top = if self.q < self.q_max {
            self.q_max
        } else if self.last_resort
            && now_ns.saturating_sub(self.paced_since_ns) >= LAST_RESORT_AFTER_NS
            && self
                .half_waiting_since_ns
                .is_some_and(|t| now_ns.saturating_sub(t) >= LAST_RESORT_AFTER_NS)
        {
            QP_LAST_RESORT
        } else {
            self.q
        };
        if skip >= SKIP_RAISE_SHARE && now_ns >= self.hold_until_ns && self.q < top && !self.pinned
        {
            // sized on what waits, never on the encoded rate scaled up by it: with most frames
            // waiting that figure runs to several times what the content asks for
            let step = if skip < 0.2 {
                2
            } else if skip < 0.4 {
                3
            } else {
                4
            };
            let from = self.q;
            self.q = from.saturating_add(step).min(top);
            self.hold_until_ns = now_ns + SKIP_RAISE_HOLD_NS;
            self.lower_not_before_ns = now_ns + LOWER_AFTER_RAISE_NS;
            self.ramp_not_before_ns = self.ramp_not_before_ns.max(now_ns + RAMP_HOLD_MIN_NS);
            self.q_changed_ns = now_ns;
            self.last_move_ns = now_ns;
            self.last_was_raise = true;
            self.raises += 1;
            return Some(LinkMove {
                q: self.q,
                divisor: self.div,
                raise: true,
                why: format!(
                    "{:.0} % of frames waiting at the sender under a {:.2} Mbps pace: QP {from} -> \
                     {}, held {} s",
                    skip * 100.0,
                    self.pace_bps / 1e6,
                    self.q,
                    LOWER_AFTER_RAISE_NS / 1_000_000_000
                ),
            });
        }
        if self.may_let_go(now_ns) {
            let why = format!(
                "clear at the user's QP for {} ms, the heaviest stretch lately under the {:.2} Mbps \
                 pace: pacing off",
                RELEASE_PASS_NS / MS,
                self.pace_bps / 1e6
            );
            return Some(self.let_go(now_ns, why));
        }
        let settled = now_ns >= self.lower_not_before_ns
            && !self.sender_waiting
            && now_ns.saturating_sub(self.q_changed_ns) >= LOWER_EVIDENCE_NS;
        if self.q > self.q_max {
            // past the bound the frame rate is already the price: back toward it a step a
            // second once under a fifth of the frames wait
            if !self.sender_waiting
                && now_ns.saturating_sub(self.q_changed_ns) >= LAST_RESORT_LOWER_NS
                && now_ns.saturating_sub(self.last_heavy_skip_ns) >= LAST_RESORT_LOWER_NS
            {
                let from = self.q;
                self.q -= 1;
                self.q_changed_ns = now_ns;
                self.last_move_ns = now_ns;
                self.steps += 1;
                return Some(LinkMove {
                    q: self.q,
                    divisor: self.div,
                    raise: false,
                    why: format!(
                        "under a fifth of the frames waiting for {} ms: QP {from} -> {}",
                        LAST_RESORT_LOWER_NS / MS,
                        self.q
                    ),
                });
            }
        } else if settled
            && self.q > self.q_user
            && now_ns.saturating_sub(self.last_skip_ns) >= LOWER_EVIDENCE_NS
        {
            // down only to where every half second of the last seconds fits with room, two
            // steps at most
            if let Some(fit) = self
                .fit_q(now_ns, now_ns, target)
                .filter(|fit| *fit < self.q)
            {
                let from = self.q;
                self.q = from.saturating_sub(LOWER_STEP_MAX).max(fit);
                self.q_changed_ns = now_ns;
                self.last_move_ns = now_ns;
                self.last_was_raise = false;
                self.steps += 1;
                return Some(LinkMove {
                    q: self.q,
                    divisor: self.div,
                    raise: false,
                    why: format!(
                        "nothing waiting for {} s, and every half second of the last {} s fits \
                         the {:.2} Mbps the pace carries with room at QP {fit}: QP {from} -> {}",
                        LOWER_EVIDENCE_NS / 1_000_000_000,
                        DEMAND_SPAN_NS / 1_000_000_000,
                        target / 1e6,
                        self.q
                    ),
                });
            }
        }
        let quiet = self
            .clear_since_ns
            .is_some_and(|t| now_ns.saturating_sub(t) >= RAMP_QUIET_NS);
        if !quiet || now_ns < self.ramp_not_before_ns || self.sender_waiting {
            return None;
        }
        self.ramp = Some(Ramp {
            from_pace: self.pace_bps,
            from_q: self.q,
            base_us: self.last.queue_us.max(0),
            stage_ns: now_ns.saturating_sub(RAMP_STEP_NS),
            carried: 0.0,
            rose: false,
            since_ns: now_ns,
            q_changed_ns: self.q_changed_ns,
            backed_ns: 0,
        });
        self.probes += 1;
        self.ramp_step(now_ns)
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
        // frames waiting at the sender are not a light stream: a keyframe draining at the pace
        // holds everything behind it
        if !self.engaged()
            || (self.pace_bps > 0.0 && (self.skip_share > 0.0 || self.sender_waiting))
        {
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
        self.pace_bps = 0.0;
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
        // paced, frames wait at the sender until the QP is high enough: sized on the weakest
        // response, one raise gets there
        let h = if self.pacing {
            SLOPE_MAX
        } else {
            self.halving_qp
        };
        let sized = (h * (enc / target).log2()).ceil().max(1.0);
        if from.0 < self.q_max {
            self.q = from.0.saturating_add(sized as u8).min(self.q_max);
            self.last_raise = Some((enc, self.q - from.0, now_ns));
        } else if self.pacing {
            // the pacer and the skips at the sender take the frame rate down instead
            return None;
        } else if self.div < MAX_DIVISOR && self.hold_frames {
            // a picture that stands for more periods carries more change; taken as the square
            // root of the periods, the frame count has to fall by the square of the overshoot.
            // The smallest cut that does it, and never more than a halving at once
            let want = (self.div as f64 * (enc / target).powi(2)).ceil() as u32;
            self.q = from.0;
            self.div = want.clamp(self.div + 1, (2 * self.div).min(MAX_DIVISOR));
        } else if self.hold_frames && self.last_resort && !self.pinned && from.0 < QP_LAST_RESORT {
            // only after the frame rate is at a quarter, and only on the ring's trailing edge:
            // elsewhere the QP stops at the bound, which is set by legibility
            // a few steps at a time: a shallow queue drops bursts of sparse frames even when the
            // average fits, and the delivered rate read during those drops understates the link
            let step = sized.min(LAST_RESORT_STEP) as u8;
            self.q = from.0.saturating_add(step).min(QP_LAST_RESORT);
        } else {
            return None;
        }
        let queue_ns = a.queue_us.max(0) as u64 * 1_000;
        self.hold_until_ns = now_ns + (RAISE_HOLD_NS + 2 * queue_ns).min(RAISE_HOLD_MAX_NS);
        if self.pacing {
            self.lower_not_before_ns = now_ns + LOWER_AFTER_RAISE_NS;
            self.q_changed_ns = now_ns;
        }
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
    shown_pace: bool,
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
            shown_pace: false,
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

    pub(crate) fn set_pacing(&mut self, yes: bool) {
        if let Some(g) = self.link.as_mut() {
            g.set_pacing(yes);
        }
    }

    pub(crate) fn pace_bps(&self) -> f64 {
        self.link.as_ref().map_or(0.0, |g| g.pace_bps())
    }

    pub(crate) fn sender(&mut self, skip_share: f64, waiting: bool) {
        if let Some(g) = self.link.as_mut() {
            g.sender(skip_share, waiting);
        }
    }

    /// Whether the QP may go past the bound (see [`LinkGovernor::last_resort`]).
    pub(crate) fn last_resort(&mut self, yes: bool) {
        if let Some(g) = self.link.as_mut() {
            g.last_resort(yes);
        }
    }

    pub(crate) fn limit_qp(&mut self, top: u8) {
        if let Some(g) = self.link.as_mut() {
            g.limit_qp(top);
        }
    }

    /// The link side has cut something: what a keyframe nobody asked for waits behind.
    pub(crate) fn link_engaged(&self) -> bool {
        self.link.as_ref().is_some_and(|g| g.engaged())
    }

    pub(crate) fn link_bps(&self) -> f64 {
        self.link.as_ref().map_or(0.0, |g| g.link_bps())
    }

    pub(crate) fn halving_qp(&self) -> f64 {
        self.link.as_ref().map_or(SLOPE_MIN, |g| g.halving_qp())
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
        asked: u8,
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
        // a keyframe may carry the boosted QP, and a frame walking back from one was asked for
        // another QP than the governors': neither says anything about the move
        let aside = keyframe || asked != self.q();
        let readback = readback.filter(|_| !aside);
        match self.check.frame(bytes, readback, !repeat && !aside) {
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
        let pacing = self.pace_bps() > 0.0;
        if now == self.shown && pacing == self.shown_pace {
            return None;
        }
        self.shown_pace = pacing;
        if now.0 != self.shown.0 && !self.qp_dead {
            self.check.raised(self.shown.0, now.0, false);
        }
        self.shown = now;
        // the pace alone moved: say so, and why
        Some(why.unwrap_or_else(|| match self.link.as_ref() {
            Some(l) if pacing => format!(
                "paced at {:.2} Mbps: {}",
                l.pace_bps() / 1e6,
                l.last_engagement
            ),
            _ => "pacing off".to_string(),
        }))
    }
}

/// Keyframes on a governed link: how large, and how often.
///
/// Every request (a PLI, a lagging viewer, the packet distance) reaches the encoder as one flag,
/// and on a congested link each answer overflows the queue, the loss opens the PLI back-off, and
/// the next periodic PLI is answered too. While the link side is engaged a request is held until
/// the last keyframe has had time to drain, and the keyframe is coded to fit a burst the link can
/// queue. A request the page itself makes (its out-of-band keyframe request) is a decoder that
/// cannot go on, and goes at once. With the link side idle every request goes as it came.
#[derive(Debug)]
pub(crate) struct KeyPolicy {
    held: bool,
    held_urgent: bool,
    last_key_ns: Option<u64>,
    /// The newest keyframe that left: bytes, QP.
    last_key: Option<(u64, u8)>,
    key_halving: f64,
    pub held_requests: u64,
}

impl Default for KeyPolicy {
    fn default() -> Self {
        KeyPolicy {
            held: false,
            held_urgent: false,
            last_key_ns: None,
            last_key: None,
            key_halving: KEY_HALVING,
            held_requests: 0,
        }
    }
}

impl KeyPolicy {
    /// How long after the last keyframe the next unasked one may go.
    pub(crate) fn gap_ns(&self, link_bps: f64) -> u64 {
        let drain_s = match self.last_key {
            Some((bytes, _)) if link_bps > 0.0 => bytes as f64 * 8.0 / (KEY_DRAIN_SHARE * link_bps),
            _ => 0.0,
        };
        ((2.0 * drain_s * 1e9) as u64).clamp(KEY_GAP_MIN_NS, KEY_GAP_MAX_NS)
    }

    /// Whether this submit is a keyframe: `asked` is a request taken from the encoder's flag just
    /// now, `page` that the page asked for one since the last look, `engaged` that the link side
    /// has cut something. Idle, the answer is `asked` and nothing is held.
    pub(crate) fn admit(
        &mut self,
        now_ns: u64,
        asked: bool,
        page: bool,
        engaged: bool,
        link_bps: f64,
    ) -> bool {
        if !asked && !self.held {
            return false;
        }
        let open = self
            .last_key_ns
            .is_none_or(|t| now_ns.saturating_sub(t) >= self.gap_ns(link_bps));
        if !engaged || page || open {
            self.held = false;
            self.last_key_ns = Some(now_ns);
            return true;
        }
        if asked && !self.held {
            self.held_requests += 1;
        }
        self.held = true;
        false
    }

    /// While the sender is paced: an urgent request (a decoder that cannot go on or has not
    /// started) goes once the last keyframe has drained (`clear`); a routine one (a PLI, the
    /// packet distance) also waits for `routine`, so the browser's periodic PLI cannot keep a
    /// slow link draining keyframes. Until then it is held, never lost.
    pub(crate) fn admit_paced(
        &mut self,
        now_ns: u64,
        asked: bool,
        urgent: bool,
        clear: bool,
        routine: bool,
    ) -> bool {
        if !asked && !self.held {
            return false;
        }
        self.held_urgent |= asked && urgent;
        if clear && (self.held_urgent || routine) {
            self.held = false;
            self.held_urgent = false;
            self.last_key_ns = Some(now_ns);
            return true;
        }
        if asked && !self.held {
            self.held_requests += 1;
        }
        self.held = true;
        false
    }

    /// A held request may go now (for an idle path to know it should submit something).
    pub(crate) fn due(&self, now_ns: u64, engaged: bool, link_bps: f64) -> bool {
        self.held
            && (!engaged
                || self
                    .last_key_ns
                    .is_none_or(|t| now_ns.saturating_sub(t) >= self.gap_ns(link_bps)))
    }

    /// The QP for a keyframe on an engaged link at `q`: the boost at least, more when the last
    /// keyframe says it would not fit `KEY_BURST_S` of the link, never past `KEY_QP_MAX` and
    /// never under `q`.
    pub(crate) fn key_qp(&self, q: u8, link_bps: f64, paced: bool) -> u8 {
        let floor = q.saturating_add(KEYFRAME_BOOST).min(KEY_QP_MAX).max(q);
        let Some((bytes, kq)) = self.last_key.filter(|_| link_bps > 0.0) else {
            return floor;
        };
        let budget = link_bps * if paced { KEY_PACED_S } else { KEY_BURST_S } / 8.0;
        let need = kq as f64 + self.key_halving * (bytes as f64 / budget).log2();
        // paced, a keyframe's size is time every frame behind it waits: it may go to the last
        // resort, and the frames after it walk back
        let top = if paced { QP_LAST_RESORT } else { KEY_QP_MAX };
        let need = need.ceil().clamp(0.0, top as f64) as u8;
        need.max(floor).min(top).max(q)
    }

    /// A keyframe left: its size and QP. Two at different QPs show how size answers QP here.
    pub(crate) fn sent(&mut self, bytes: u64, qp: u8) {
        if let Some((b0, q0)) = self.last_key {
            if qp >= q0.saturating_add(4) && bytes > 0 && bytes < b0 {
                let h = (qp - q0) as f64 / (b0 as f64 / bytes as f64).log2();
                self.key_halving =
                    (0.5 * self.key_halving + 0.5 * h).clamp(SLOPE_MIN, KEY_HALVING_MAX);
            }
        }
        self.last_key = Some((bytes, qp));
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
    /// The capture path can hold a gated frame and deliver it later (the ring's trailing edge, or
    /// the classic path's held copy). Without that the frame rate is never cut.
    frame_hold: AtomicBool,
    /// ... on the ring's trailing edge, the one measured live: only there may the QP pass the bound.
    trailing_edge: AtomicBool,
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
    keyframes_held: AtomicU64,
    /// The rate the writer paces this viewer at; 0 while the link side is not pacing, and then the
    /// writer paces exactly as it always did.
    pace_bps: AtomicU64,
    /// The link side has cut something: loss is no longer news, and does not open the PLI back-off.
    engaged: AtomicBool,
    /// When what the writer holds will have left at the pacing rate (host ns), as the writer
    /// sees it, and as the encode thread sees it with the frames it has sent since.
    writer_until_ns: AtomicU64,
    send_until_ns: AtomicU64,
    /// Frames skipped at the sender for the backlog.
    skipped: AtomicU64,
    /// [`key_origin`] bits of the keyframe requests since the encoder last took the flag.
    key_origins: AtomicU32,
    keyframes_unrepairable: AtomicU64,
    keyframes_paced: AtomicU64,
    /// Keyframes sent, by the origins of the requests behind them.
    keyframes_by_origin: [AtomicU64; 6],
    engagements: AtomicU64,
    engaged_ms: AtomicU64,
    last_engagement: Slot<String>,
    /// PLIs from a painting page that were not answered.
    plis_ignored: AtomicU64,
    last_move: Slot<String>,
    state: Slot<String>,
}

/// Where a keyframe request came from, as bits.
pub mod key_origin {
    /// The page's own out-of-band request: its decoder cannot go on.
    pub const PAGE: u32 = 1;
    /// An RTCP PLI or FIR: on an encoded-transform page, mostly libwebrtc's idle decoder.
    pub const PLI: u32 = 2;
    /// A delta near the packet distance some receivers allow.
    pub const DISTANCE: u32 = 4;
    /// The broadcast dropped frames on this viewer.
    pub const LAG: u32 = 8;
    /// A resend refused for age or budget: that frame can never be completed.
    pub const UNREPAIRABLE: u32 = 16;
    /// The connection came up, the first RTP reached the wire, or nothing could be packetized.
    pub const START: u32 = 32;
    /// A decoder that cannot go on or has not started.
    pub const URGENT: u32 = PAGE | LAG | UNREPAIRABLE | START;
}

/// A token bucket at the link's pace for everything the writer sends one viewer, media and
/// resends alike. Unlike the transport's own pacer it never gives up on a frame: what it cannot
/// pass waits, and the sender skips frames against that wait instead of the network dropping
/// packets.
#[derive(Debug, Default)]
pub(crate) struct LinkPacer {
    rate: f64,
    tokens: f64,
    last: Option<std::time::Instant>,
}

/// The burst the link pacer lets out at once: a few milliseconds of the rate, at least two
/// packets.
const PACE_BURST_S: f64 = 0.004;
const PACE_BURST_MIN: f64 = 2.0 * 1_200.0;

impl LinkPacer {
    pub(crate) fn set_rate(&mut self, bps: u64) {
        self.rate = bps as f64 / 8.0;
    }

    fn burst(&self) -> f64 {
        (self.rate * PACE_BURST_S).max(PACE_BURST_MIN)
    }

    fn refill(&mut self, now: std::time::Instant) {
        self.tokens = match self.last {
            Some(t) => (self.tokens + self.rate * now.saturating_duration_since(t).as_secs_f64())
                .min(self.burst()),
            None => self.burst(),
        };
        self.last = Some(now);
    }

    /// How long to wait before writing `bytes`.
    pub(crate) fn schedule(
        &mut self,
        bytes: usize,
        now: std::time::Instant,
    ) -> std::time::Duration {
        if self.rate <= 0.0 {
            return std::time::Duration::ZERO;
        }
        self.refill(now);
        self.tokens -= bytes as f64;
        if self.tokens >= 0.0 {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::from_secs_f64((-self.tokens / self.rate).min(2.0))
    }

    /// How long until `more` bytes on top of what is owed have left.
    pub(crate) fn backlog(&mut self, more: usize, now: std::time::Instant) -> std::time::Duration {
        if self.rate <= 0.0 {
            return std::time::Duration::ZERO;
        }
        self.refill(now);
        let owed = more as f64 - self.tokens;
        std::time::Duration::from_secs_f64((owed / self.rate).clamp(0.0, 10.0))
    }
}

/// A keyframe goes no sooner than this after the previous one has drained at the pace: what
/// stops a storm, and the only limit left while the sender is paced.
const KEY_MARGIN_NS: u64 = 100 * MS;
/// A routine keyframe (a PLI, the packet distance) waits this many of the last keyframe's drain
/// times, and `KEY_GAP_MIN_NS` at least: keyframes then hold the link a quarter of the time at
/// most however slow it is.
const KEY_ROUTINE_DRAINS: u64 = 4;
const SKIP_BUCKET_NS: u64 = 125 * MS;
const SKIP_BUCKETS: usize = 4;

/// The page painted something within this long: it counts as painting.
const PAINT_RECENT_NS: u64 = 3_000 * MS;

/// A PLI from a page that is painting through its own decoder is libwebrtc's idle decoder, not a
/// picture anyone lost: with no other reason behind it, it is answered at the healthy cadence
/// only (`keyframe::PLI_BACKOFF_CAP_HEALTHY_MS`), paced link or not.
pub(crate) fn pli_is_noise(origins: u32, painting: bool, since_key_ns: u64) -> bool {
    painting
        && origins & key_origin::PLI != 0
        && origins & !key_origin::PLI == 0
        && since_key_ns < crate::streamer::keyframe::PLI_BACKOFF_CAP_HEALTHY_MS * MS
}

/// The sender's side of pacing: when what it has handed the writer will have left at the pace,
/// which frames it skipped against that, and when the last keyframe has drained.
#[derive(Debug, Default)]
pub(crate) struct SendGate {
    until_ns: u64,
    key_clear_ns: u64,
    key_routine_ns: u64,
    /// `(bucket, sent, skipped)`.
    buckets: [(u64, u32, u32); SKIP_BUCKETS],
}

impl SendGate {
    /// The writer holds more than `period_ns` of sending: a new frame would only wait behind it.
    pub(crate) fn busy(&self, now_ns: u64, writer_until_ns: u64, period_ns: u64) -> bool {
        self.until(writer_until_ns) > now_ns + period_ns
    }

    pub(crate) fn until(&self, writer_until_ns: u64) -> u64 {
        self.until_ns.max(writer_until_ns)
    }

    /// An access unit of `bytes` was handed to the writer.
    pub(crate) fn sent(
        &mut self,
        now_ns: u64,
        bytes: u64,
        pace_bps: f64,
        writer_until_ns: u64,
        keyframe: bool,
    ) {
        let start = now_ns.max(self.until(writer_until_ns));
        self.until_ns = start + (bytes as f64 * 8e9 / pace_bps.max(PACE_MIN_BPS)) as u64;
        if keyframe {
            self.key_clear_ns = self.until_ns + KEY_MARGIN_NS;
            let drain = self.until_ns - start;
            self.key_routine_ns = now_ns + (KEY_ROUTINE_DRAINS * drain).max(KEY_GAP_MIN_NS);
        }
        self.count(now_ns, 1, 0);
    }

    /// Frames that waited at the sender. Those behind a keyframe still draining are its cost,
    /// paid once, and say nothing about the QP.
    pub(crate) fn skipped(&mut self, now_ns: u64, n: u32) {
        if n > 0 && now_ns >= self.key_clear_ns {
            self.count(now_ns, 0, n);
        }
    }

    /// A keyframe is still draining at the pace, with the frames behind it waiting, or did so
    /// within the encoded-rate window: what was encoded meanwhile is not the content's rate.
    pub(crate) fn draining(&self, now_ns: u64) -> bool {
        now_ns < self.key_clear_ns + ENC_WINDOW_NS
    }

    fn count(&mut self, now_ns: u64, sent: u32, skipped: u32) {
        let id = now_ns / SKIP_BUCKET_NS + 1;
        let b = &mut self.buckets[(id as usize) % SKIP_BUCKETS];
        if b.0 != id {
            *b = (id, 0, 0);
        }
        b.1 += sent;
        b.2 += skipped;
    }

    /// Skipped over sent and skipped, the last half second.
    pub(crate) fn skip_share(&self, now_ns: u64) -> f64 {
        let id = now_ns / SKIP_BUCKET_NS + 1;
        let (sent, skipped) = self
            .buckets
            .iter()
            .filter(|b| b.0 != 0 && b.0 + SKIP_BUCKETS as u64 > id)
            .fold((0u32, 0u32), |(a, b), x| (a + x.1, b + x.2));
        if sent + skipped == 0 {
            0.0
        } else {
            skipped as f64 / (sent + skipped) as f64
        }
    }

    /// The last keyframe has drained, and a little more.
    pub(crate) fn key_clear(&self, now_ns: u64) -> bool {
        now_ns >= self.key_clear_ns
    }

    /// Long enough since the last keyframe for a routine one.
    pub(crate) fn key_routine(&self, now_ns: u64) -> bool {
        now_ns >= self.key_routine_ns
    }

    pub(crate) fn reset(&mut self) {
        *self = SendGate::default();
    }
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

    /// The writer's pacing rate for this viewer; 0 when it paces as it always did.
    pub fn pace_bps(&self) -> u64 {
        self.pace_bps.load(Ordering::Relaxed)
    }

    /// The link side has cut something.
    pub fn engaged(&self) -> bool {
        self.engaged.load(Ordering::Relaxed)
    }

    /// How long the link side has been engaged in all, for as long as this governor has run.
    pub fn engaged_ms(&self) -> u64 {
        self.engaged_ms.load(Ordering::Relaxed)
    }

    /// What the link delivered in the newest judged window, for the resend budget's basis.
    pub fn delivered_bps(&self) -> u64 {
        self.delivered_bps.load(Ordering::Relaxed)
    }

    /// Nothing paced, nothing engaged, no backlog and no request pending: what a governor that
    /// no longer runs left here would otherwise defer the next pipeline's frames, keep the writer
    /// pacing and refusing resends, and keep loss from reaching the PLI back-off.
    fn clear_link_state(&self) {
        self.pace_bps.store(0, Ordering::Relaxed);
        self.engaged.store(false, Ordering::Relaxed);
        self.writer_until_ns.store(0, Ordering::Relaxed);
        self.send_until_ns.store(0, Ordering::Relaxed);
        self.key_origins.store(0, Ordering::Relaxed);
        self.divisor.store(1, Ordering::Relaxed);
        self.feedback.lock_recover().clear();
    }

    /// The writer's half: when what it holds will have left.
    pub fn set_writer_until(&self, ns: u64) {
        self.writer_until_ns.store(ns, Ordering::Relaxed);
    }

    /// The capture side's half: the sender holds more than a frame period, so the frame waits
    /// (one relaxed load while nothing is paced).
    pub fn sender_busy(&self) -> bool {
        if self.pace_bps.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let period = 1_000_000_000 / self.fps.load(Ordering::Relaxed).max(1) as u64;
        self.busy_until_ns() > crate::streamer::audio::host_now_ns() + BACKLOG_PERIODS * period
    }

    /// When the sender's backlog will have left (host ns).
    pub fn busy_until_ns(&self) -> u64 {
        self.writer_until_ns
            .load(Ordering::Relaxed)
            .max(self.send_until_ns.load(Ordering::Relaxed))
    }

    /// A frame was skipped at the sender for the backlog.
    pub fn note_skip(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// A keyframe request, with where it came from.
    pub fn note_key_origin(&self, origin: u32) {
        self.key_origins.fetch_or(origin, Ordering::Relaxed);
        if origin & key_origin::UNREPAIRABLE != 0 {
            self.keyframes_unrepairable.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub(crate) fn key_origins_pending(&self) -> u32 {
        self.key_origins.load(Ordering::Relaxed)
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
        let pacing = serde_json::json!({
            "pace_mbps": r(&self.pace_bps) as f64 / 1e6,
            "frames_skipped_at_sender": r(&self.skipped),
            "keyframes_unrepairable": r(&self.keyframes_unrepairable),
            "keyframes_paced": r(&self.keyframes_paced),
            "plis_ignored_while_painting": r(&self.plis_ignored),
            "keyframes_by_origin": {
                "page": r(&self.keyframes_by_origin[0]),
                "pli": r(&self.keyframes_by_origin[1]),
                "distance": r(&self.keyframes_by_origin[2]),
                "lag": r(&self.keyframes_by_origin[3]),
                "unrepairable": r(&self.keyframes_by_origin[4]),
                "start": r(&self.keyframes_by_origin[5]),
            },
        });
        Some(serde_json::json!({
            "state": self.state.get(),
            "link": self.link_on.load(Ordering::Relaxed),
            "ceiling": self.ceiling_on.load(Ordering::Relaxed),
            "qp_unsupported": self.qp_unsupported.load(Ordering::Relaxed),
            "frame_hold": self.frame_hold.load(Ordering::Relaxed),
            "trailing_edge": self.trailing_edge.load(Ordering::Relaxed),
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
            "keyframes_held": r(&self.keyframes_held),
            "engagement": {
                "count": r(&self.engagements),
                "engaged_ms": r(&self.engaged_ms),
                "last": self.last_engagement.get(),
            },
            "pacing": pacing,
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
    l.trailing_edge.store(false, Ordering::Relaxed);
}

/// What the capture answers to "can I deliver a gated frame later", and whether that is the
/// ring's trailing edge: asked on every frame it gates, stored only when the answer changes.
pub(crate) fn set_frame_hold(telemetry: &SessionTelemetry, yes: bool, trailing_edge: bool) {
    let l = &telemetry.link;
    if l.frame_hold.load(Ordering::Relaxed) != yes {
        l.frame_hold.store(yes, Ordering::Relaxed);
    }
    let trailing_edge = yes && trailing_edge;
    if l.trailing_edge.load(Ordering::Relaxed) != trailing_edge {
        l.trailing_edge.store(trailing_edge, Ordering::Relaxed);
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
    /// The highest QP `set_qp` reaches (libx264 widened for a user setting under 4 stops at 39).
    fn qp_top(&self) -> u8 {
        51
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
    /// The QP it was submitted at.
    q: u8,
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
    keys: KeyPolicy,
    /// Pace the sender to the link once engaged (`link_pace`).
    link_pace: bool,
    gate: SendGate,
    skips_seen: u64,
    /// Origins of the requests behind the keyframe still to come.
    key_origins: u32,
    /// The page's last stats `(received, frames painted)`, and whether it was painting then.
    paint_seen: (u64, u32),
    paint_moved_ns: u64,
    painting: bool,
    paint_polled_ns: u64,
    /// The QP the frames after a coarse keyframe walk back from.
    walk: u8,
    /// The highest QP the encoder reaches; 0 until it has been asked.
    top: u8,
}

impl QpControl {
    pub(crate) fn from_telemetry(telemetry: &std::sync::Arc<SessionTelemetry>) -> Option<Self> {
        Self::build(
            telemetry,
            super::tune::on("link_governor"),
            super::tune::candidate("qp_ceiling_governor"),
            super::tune::candidate("client_pace_governor"),
        )
        .map(|c| c.with_link_pace(super::tune::on("link_pace")))
    }

    /// Pace the sender to the link once the link side is engaged, instead of the frame divisor.
    pub(crate) fn with_link_pace(mut self, yes: bool) -> Self {
        self.link_pace = yes;
        self.gov.set_pacing(yes);
        self
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
        l.clear_link_state();
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
            keys: KeyPolicy::default(),
            link_pace: false,
            gate: SendGate::default(),
            skips_seen: l.skipped.load(Ordering::Relaxed),
            key_origins: 0,
            paint_seen: (0, 0),
            paint_moved_ns: 0,
            painting: false,
            paint_polled_ns: 0,
            walk: 0,
            top: 0,
        })
    }

    /// The backend was rebuilt under this path: whatever the old encoder could not do, the new
    /// one is asked again.
    pub(crate) fn encoder_replaced(&mut self) {
        if self.qp_unsupported {
            self.qp_unsupported = false;
            self.gov = Governors::new(self.q_user, self.ceiling_bps, self.fps, self.link);
            self.gov.set_pacing(self.link_pace);
            self.shared
                .link
                .qp_unsupported
                .store(false, Ordering::Relaxed);
        }
        self.applied = self.q_user;
        self.top = 0;
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
        if self.top == 0 {
            self.top = enc.qp_top().max(1);
            self.gov.limit_qp(self.top);
        }
        let now = crate::streamer::audio::host_now_ns();
        let l = &self.shared.link;
        let hold = l.frame_hold.load(Ordering::Relaxed);
        // the classic path's held copy serves the link side only: the client's pace cuts the
        // frame rate where it always could, on the ring's trailing edge
        let trailing = l.trailing_edge.load(Ordering::Relaxed);
        self.gov.hold_frames(hold);
        self.gov.last_resort(trailing);
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
        if self.gov.pace_bps() > 0.0 && now.saturating_sub(self.paint_polled_ns) >= 500 * MS {
            self.paint_polled_ns = now;
            // painting through its own decoder within the last few seconds: a keyframe draining
            // at the pace pauses the picture longer than one stats interval, and is not a stall
            if let Some(c) = self
                .shared
                .client
                .get()
                .filter(|c| c.transport_tier != 0 && c.received_host_ns != self.paint_seen.0)
            {
                if self.paint_seen.0 != 0 && c.frames_painted != self.paint_seen.1 {
                    self.paint_moved_ns = c.received_host_ns;
                }
                self.paint_seen = (c.received_host_ns, c.frames_painted);
            }
            self.painting = self.paint_moved_ns != 0
                && now.saturating_sub(self.paint_moved_ns) < PAINT_RECENT_NS;
        }
        if self.link_pace {
            let skipped = l.skipped.load(Ordering::Relaxed);
            self.gate
                .skipped(now, skipped.saturating_sub(self.skips_seen) as u32);
            self.skips_seen = skipped;
            self.gov
                .sender(self.gate.skip_share(now), self.gate.draining(now));
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
        if let Some(pace) = self.pace.as_mut().filter(|_| trailing) {
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
        let client = if trailing {
            self.pace.as_ref().map_or(1, |p| p.divisor())
        } else {
            1
        };
        let div = if hold {
            self.gov.divisor().max(client)
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
            l.engaged_ms.store(g.engaged_ns / MS, Ordering::Relaxed);
            if l.engagements.load(Ordering::Relaxed) != g.engagements {
                l.engagements.store(g.engagements, Ordering::Relaxed);
                l.last_engagement.set(g.last_engagement.clone());
            }
            l.halving_milli
                .store((g.halving_qp() * 1e3) as u64, Ordering::Relaxed);
        }
        let pace = self.gov.pace_bps() as u64;
        if l.pace_bps.load(Ordering::Relaxed) != pace {
            l.pace_bps.store(pace, Ordering::Relaxed);
            if pace == 0 {
                self.gate.reset();
                l.writer_until_ns.store(0, Ordering::Relaxed);
                l.send_until_ns.store(0, Ordering::Relaxed);
            }
        }
        let engaged = self.gov.link_engaged();
        if l.engaged.load(Ordering::Relaxed) != engaged {
            l.engaged.store(engaged, Ordering::Relaxed);
        }
        let key = std::mem::take(&mut self.keyframe_next);
        self.last_submit_key = key;
        if self.qp_unsupported {
            self.publish_above();
            return;
        }
        let mut q = self.gov.q();
        if !self.gov.engaged() {
            // let go means let go: no frame walks back from a keyframe the governor coarsened
            self.walk = 0;
        }
        if key && self.gov.engaged() && !self.gov.qp_dead {
            if self.gov.link_engaged() {
                q = self
                    .keys
                    .key_qp(q, self.gov.link_bps(), self.gov.pace_bps() > 0.0)
                    .min(self.top.max(q));
                self.walk = q;
            } else {
                // the ceiling alone: boosted, and the frame after goes straight back, as it did
                q = q.saturating_add(KEYFRAME_BOOST).min(QP_LAST_RESORT);
                self.walk = 0;
            }
        } else if self.walk > q {
            self.walk = self.walk.saturating_sub(KEY_RETURN_STEP).max(q);
            q = self.walk;
            if self.walk == self.gov.q() {
                self.walk = 0;
            }
        } else {
            self.walk = 0;
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

    /// Whether the next submit is a keyframe, `asked` being the request just taken from the
    /// encoder's flag (see [`KeyPolicy`]). Called for every submit but a session's first. With
    /// the link side idle it answers `asked`.
    pub(crate) fn admit_keyframe(&mut self, asked: bool) -> bool {
        if self.off {
            return asked;
        }
        let l = &self.shared.link;
        let origins = if asked {
            l.key_origins.swap(0, Ordering::Relaxed)
        } else {
            0
        };
        let held = self.keys.held_requests;
        let now = crate::streamer::audio::host_now_ns();
        let since_key = self
            .keys
            .last_key_ns
            .map_or(u64::MAX, |t| now.saturating_sub(t));
        let paced = self.gov.pace_bps() > 0.0;
        let asked = if paced && asked && pli_is_noise(origins, self.painting, since_key) {
            l.plis_ignored.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            asked
        };
        if asked {
            self.key_origins |= origins;
        }
        let go = if paced {
            let go = self.keys.admit_paced(
                now,
                asked,
                origins & key_origin::URGENT != 0,
                self.gate.key_clear(now),
                self.gate.key_routine(now),
            );
            if go {
                l.keyframes_paced.fetch_add(1, Ordering::Relaxed);
            }
            go
        } else {
            self.keys.admit(
                now,
                asked,
                origins & key_origin::URGENT != 0,
                self.gov.link_engaged(),
                self.gov.link_bps(),
            )
        };
        if self.keys.held_requests != held {
            l.keyframes_held
                .store(self.keys.held_requests, Ordering::Relaxed);
        }
        if go {
            let o = std::mem::take(&mut self.key_origins);
            for (i, c) in l.keyframes_by_origin.iter().enumerate() {
                if o & (1 << i) != 0 {
                    c.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        go
    }

    /// A held keyframe request may go now: an idle path should submit something to carry it.
    pub(crate) fn keyframe_due(&self) -> bool {
        if self.off || !self.keys.held {
            return false;
        }
        let now = crate::streamer::audio::host_now_ns();
        if self.gov.pace_bps() > 0.0 {
            return self.gate.key_clear(now)
                && (self.keys.held_urgent || self.gate.key_routine(now));
        }
        self.keys
            .due(now, self.gov.link_engaged(), self.gov.link_bps())
    }

    /// After an access unit has been handed to the transport. Records it and nothing more: the
    /// first bytes are copied only when a read is due (a keyframe, a move being checked, a
    /// periodic check), and everything else waits for the next submit.
    pub(crate) fn emitted(&mut self, au: &[u8], repeat: bool) {
        if self.off || au.is_empty() {
            return;
        }
        let pace = self.gov.pace_bps();
        if pace > 0.0 {
            let l = &self.shared.link;
            let writer = l.writer_until_ns.load(Ordering::Relaxed);
            self.gate.sent(
                crate::streamer::audio::host_now_ns(),
                au.len() as u64,
                pace,
                writer,
                self.last_submit_key,
            );
            l.send_until_ns
                .store(self.gate.until(writer), Ordering::Relaxed);
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
            q: self.applied,
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
        if keyframe {
            self.keys.sent(e.len, qp.unwrap_or(e.q));
        }
        let l = &self.shared.link;
        let unconfirmed = self.gov.unconfirmed;
        if let Some(c) = self
            .gov
            .emitted(e.at_ns, e.len, e.repeat, keyframe, qp, e.q)
        {
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
            l.clear_link_state();
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

    /// How the simulated host answers a keyframe request.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Keys {
        /// A keyframe at the governed QP, at once (what the simulations always did).
        Plain,
        /// The code before keyframes were governed: at once, six steps coarser while engaged.
        Boosted,
        /// [`KeyPolicy`], with the walk back after a coarse keyframe, as `QpControl` does it.
        Governed,
    }

    /// A keyframe's size at QP 15: the desktop's.
    const KEY_BYTES: u64 = 400_000;

    struct Sim {
        /// QP steps that halve the bits: 6 by the textbook, ~16 for noise-dominated content
        /// (the real-encoder tests below measured x1.26 to x1.30 per six steps on such content).
        halving_qp: f64,
        keys: Keys,
        /// The same for a keyframe.
        key_halving: f64,
        /// The browser's PLIs: one every so many ms, through the PLI back-off (opened by any
        /// loss a report shows) and the keyframe gate, as the transport takes them.
        pli_every_ms: Option<u64>,
        /// The page's own keyframe requests, one every so many ms.
        page_every_ms: Option<u64>,
        /// A P frame finer than the picture it predicts from refines it: this share of a
        /// keyframe's bytes at the finer QP, less the coarser one's.
        refine: Option<f64>,
        policy: KeyPolicy,
        backoff: crate::streamer::keyframe::PliBackoff,
        gate: crate::streamer::keyframe::KeyframeGate,
        idr_flag: bool,
        page_asked: bool,
        level: u8,
        walk: u8,
        /// (send ms, bytes, QP) of every keyframe.
        keyframes: Vec<(u64, u64, u8)>,
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
        /// Loss starts here (ns); feedback reports lost on the way back.
        loss_from_ns: u64,
        loss_until_ns: u64,
        report_loss: f64,
        /// Per frame: (send ms, queueing delay of its first packet in ms, None if it lost one).
        frames: Vec<(u64, Option<f64>)>,
    }

    impl Sim {
        fn new(path: Path, seed: u64) -> Self {
            Sim {
                halving_qp: 6.0,
                keys: Keys::Plain,
                key_halving: 6.0,
                pli_every_ms: None,
                page_every_ms: None,
                refine: None,
                policy: KeyPolicy::default(),
                backoff: crate::streamer::keyframe::PliBackoff::new(),
                gate: crate::streamer::keyframe::KeyframeGate::new(),
                idr_flag: false,
                page_asked: false,
                level: 15,
                walk: 0,
                keyframes: Vec::new(),
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
                loss_from_ns: 0,
                loss_until_ns: u64::MAX,
                report_loss: 0.0,
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
                } else if self.rng.next()
                    < if (self.loss_from_ns..self.loss_until_ns).contains(&t_ns) {
                        self.path.loss
                    } else {
                        0.0
                    }
                {
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
                // feedback lost on the way back is never repeated
                if self.report_loss > 0.0 && self.rng.next() < self.report_loss {
                    continue;
                }
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
                    if s.lost > 0 {
                        self.backoff.note_evidence();
                    }
                    g.feedback(t, &s);
                }
                let tick = |every: Option<u64>| {
                    every.is_some_and(|ms| {
                        k > 0 && (k * 1_000 / FPS) / ms > ((k - 1) * 1_000 / FPS) / ms
                    })
                };
                if tick(self.pli_every_ms)
                    && self
                        .backoff
                        .allow_with_cap(now, crate::streamer::keyframe::PLI_BACKOFF_CAP_MS)
                    && self.gate.allow(now)
                {
                    self.idr_flag = true;
                }
                if tick(self.page_every_ms) {
                    self.page_asked = true;
                    if self.gate.allow(now) {
                        self.idr_flag = true;
                    }
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
                let asked = std::mem::take(&mut self.idr_flag);
                let forced = match self.keys {
                    Keys::Plain | Keys::Boosted => asked,
                    Keys::Governed => {
                        let page = std::mem::take(&mut self.page_asked);
                        self.policy
                            .admit(now, asked, page, g.link_engaged(), g.link_bps())
                    }
                };
                if std::mem::take(&mut self.force_key) || forced {
                    (base, key) = (KEY_BYTES, true);
                }
                if base == 0 {
                    // nothing changed on screen and no repeat is due
                    continue;
                }
                let mut sq = match self.model {
                    Model::Honest => q,
                    Model::Ignores { .. } => self.costly_q.unwrap_or(15),
                };
                if self.model == Model::Honest {
                    let engaged = g.engaged();
                    match self.keys {
                        Keys::Boosted if key && engaged => {
                            sq = sq.saturating_add(KEYFRAME_BOOST).min(QP_LAST_RESORT);
                        }
                        Keys::Governed if key && engaged => {
                            sq = if g.link_engaged() {
                                self.policy.key_qp(sq, g.link_bps(), false)
                            } else {
                                sq.saturating_add(KEYFRAME_BOOST).min(QP_LAST_RESORT)
                            };
                            self.walk = sq;
                        }
                        Keys::Governed if !key && self.walk > sq => {
                            self.walk = self.walk.saturating_sub(KEY_RETURN_STEP).max(sq);
                            sq = self.walk;
                        }
                        _ => {}
                    }
                }
                let at = |q: u8, h: f64| 2f64.powf(-(q as f64 - 15.0) / h);
                let mut bytes = if key {
                    (base as f64 * at(sq, self.key_halving)) as u64
                } else {
                    (base as f64 * at(sq, self.halving_qp) * (d as f64).sqrt()) as u64
                };
                if let Some(share) = self.refine {
                    if !key && sq < self.level {
                        let kh = self.key_halving;
                        bytes +=
                            (share * KEY_BYTES as f64 * (at(sq, kh) - at(self.level, kh))) as u64;
                    }
                    self.level = sq;
                }
                if key {
                    self.policy.sent(bytes, sq);
                    self.keyframes.push((now / MS, bytes, sq));
                }
                let asked = if self.model == Model::Honest { sq } else { q };
                if let Some((want, in_force)) = g.emitted(now, bytes, false, key, Some(sq), asked) {
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

        /// Keyframes sent in `[from, to)` ms.
        fn keyframes_in(&self, from: u64, to: u64) -> usize {
            self.keyframes
                .iter()
                .filter(|(t, _, _)| (from..to).contains(t))
                .count()
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
        // engagement takes a second of sustained shortfall on top of the first verdict
        assert!(settled <= 2_000, "settled {settled} ms after the cap");
        assert_eq!(g.divisor(), 1, "5 Mbps needs no frame-rate cut for 12 Mbps");
        let late = sim.bad_frames(12_000, 40_000, 60.0);
        assert!(late * 100 < 28 * FPS as usize * 3, "{late} late frames");
        let raises = g.link.as_ref().unwrap().raises;
        assert!(raises <= 6, "{raises} raises: {:?}", sim.trace);
        assert!(sim.sent_mbps(30_000, 40_000) > 3.5, "under-using the link");
    }

    #[test]
    fn a_step_down_to_1_5_mbps_fits_at_the_full_frame_rate() {
        // 20 Mbps at QP 40 is 1.1 Mbps: the QP range carries it, so no frame is withheld
        let mut g = governors();
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 1.5e6 }), 31);
        sim.run(&mut g, 40, &steady(20.0));
        report("cap1_5", &sim, 10_000);
        let settled = sim
            .settled_after(10_000, 60.0, 1_000)
            .expect("never fitted");
        assert!(settled <= 2_500, "settled {settled} ms after the cap");
        assert!(sim.trace.iter().all(|t| t.2 == 1), "{:?}", sim.trace);
    }

    /// Steady content into a cap the QP range can carry (`rate at QP_LINK_MAX` under what the
    /// governor aims for), from a light squeeze to 1 Mbps: the frame rate is never touched.
    #[test]
    fn for_any_cap_the_qp_range_can_carry_the_frame_rate_is_never_touched() {
        type Cap = (fn(u64) -> f64, f64);
        let caps: [Cap; 6] = [
            (|t| if t < 5_000 * MS { 1e9 } else { 8e6 }, 8e6),
            (|t| if t < 5_000 * MS { 1e9 } else { 5e6 }, 5e6),
            (|t| if t < 5_000 * MS { 1e9 } else { 3e6 }, 3e6),
            (|t| if t < 5_000 * MS { 1e9 } else { 2e6 }, 2e6),
            (|t| if t < 5_000 * MS { 1e9 } else { 1.5e6 }, 1.5e6),
            (|t| if t < 5_000 * MS { 1e9 } else { 1e6 }, 1e6),
        ];
        for (seed, (cap, bps)) in caps.into_iter().enumerate() {
            for mbps in [12.0, 20.0] {
                let at_bound = mbps * 1e6 * 2f64.powf(-((QP_LINK_MAX - 15) as f64) / 6.0);
                if at_bound > TARGET_MAX * bps * 0.95 {
                    continue;
                }
                let mut g = governors();
                let mut sim = Sim::new(capped(cap), 200 + seed as u64);
                sim.run(&mut g, 30, &steady(mbps));
                eprintln!("{mbps} Mbps into {:.1} Mbps: {:?}", bps / 1e6, sim.trace);
                assert!(
                    sim.trace.iter().all(|t| t.2 == 1),
                    "{mbps} Mbps into {bps}: {:?}",
                    sim.trace
                );
                assert!(sim.settled_after(5_000, 60.0, 1_000).is_some());
            }
        }
    }

    #[test]
    fn past_the_qp_bound_the_frame_rate_is_halved_before_it_is_quartered() {
        // 40 Mbps at QP 40 is 2.2 Mbps into 1.5: only now is the frame rate cut
        for (seed, last_resort) in [(32u64, true), (33, false)] {
            let mut g = governors();
            g.last_resort(last_resort);
            let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 1.5e6 }), seed);
            sim.run(&mut g, 40, &steady(40.0));
            report("cap1_5, 40 Mbps", &sim, 10_000);
            assert!(sim.trace.iter().any(|t| t.2 > 1), "{:?}", sim.trace);
            let mut prev = (15, 1);
            for &(_, q, d) in &sim.trace {
                if d > prev.1 {
                    assert_eq!(
                        prev.0, QP_LINK_MAX,
                        "frames before quality: {:?}",
                        sim.trace
                    );
                    assert!(
                        d <= 2 * prev.1,
                        "more than a halving at once: {:?}",
                        sim.trace
                    );
                }
                if !last_resort {
                    assert!(q <= QP_LINK_MAX, "past the bound off the trailing edge");
                }
                prev = (q, d);
            }
            assert!(sim.settled_after(10_000, 60.0, 1_000).unwrap() <= 3_500);
        }
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
        // and engagement waits for a second of sustained shortfall
        assert!(late * 100 < 26 * FPS as usize * 25, "{late} late frames");
    }

    #[test]
    fn past_the_bound_and_a_quarter_of_the_frames_the_qp_is_the_last_resort() {
        // 80 Mbps of noise into 1.5 Mbps: QP 40 at a quarter of the frames is still 2.2 Mbps
        let mut g = governors();
        let lifts = |t| {
            if (10_000 * MS..30_000 * MS).contains(&t) {
                1.5e6
            } else {
                1e9
            }
        };
        let mut sim = Sim::new(capped(lifts), 111);
        sim.run(&mut g, 60, &steady(80.0));
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
        // the link side lets go entirely; 80 Mbps is over the LAN ceiling, which keeps its own
        let l = g.link.as_ref().unwrap();
        assert_eq!((l.q(), l.divisor()), (15, 1), "{:?}", sim.trace);
        assert_eq!(g.q(), g.ceiling.as_ref().unwrap().q());
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
        assert!(settled <= 4_500, "settled {settled} ms after the cap");
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

    // keyframes on a governed link

    #[test]
    fn an_idle_link_answers_every_keyframe_request_as_it_came() {
        let mut p = KeyPolicy::default();
        let mut r = Rng(5);
        for k in 0..2_000u64 {
            let asked = r.next() < 0.3;
            let page = r.next() < 0.1;
            assert_eq!(
                p.admit(k * PERIOD_NS, asked, page, false, 5e6),
                asked,
                "frame {k}"
            );
            if asked {
                p.sent(400_000, 15);
            }
        }
        assert_eq!(p.held_requests, 0);
        assert!(!p.due(2_000 * PERIOD_NS, false, 5e6));
    }

    #[test]
    fn an_engaged_link_holds_unasked_keyframes_and_never_the_pages() {
        let mut p = KeyPolicy::default();
        let link = 5e6;
        assert!(p.admit(0, true, false, true, link), "the first one goes");
        p.sent(40_000, 34);
        // a PLI 300 ms later waits for the gap, and is not lost
        assert!(!p.admit(300 * MS, true, false, true, link));
        assert_eq!(p.held_requests, 1);
        assert!(!p.admit(1_000 * MS, false, false, true, link));
        assert!(!p.due(1_900 * MS, true, link));
        assert!(p.due(KEY_GAP_MIN_NS, true, link));
        assert!(
            p.admit(KEY_GAP_MIN_NS, false, false, true, link),
            "carried after the gap"
        );
        // the page asking goes at once, and takes a held request with it
        let t = KEY_GAP_MIN_NS;
        assert!(p.admit(t + 100 * MS, true, true, true, link));
        assert!(!p.admit(t + 200 * MS, true, false, true, link));
        assert!(p.admit(t + 250 * MS, false, true, true, link));
        // a link that lets go lets a held request go with it
        assert!(!p.admit(t + 300 * MS, true, false, true, link));
        assert!(p.admit(t + 316 * MS, false, false, false, link));
        assert_eq!(p.held_requests, 3);
    }

    #[test]
    fn the_keyframe_gap_follows_what_the_last_one_costs_to_drain_within_bounds() {
        let mut p = KeyPolicy::default();
        assert_eq!(p.gap_ns(0.0), KEY_GAP_MIN_NS);
        // 15 KB at 1.5 Mbps drains through the 15 % the stream leaves free in 0.53 s
        p.sent(15_000, 46);
        assert_eq!(p.gap_ns(1.5e6), KEY_GAP_MIN_NS);
        p.sent(45_000, 46);
        let g = p.gap_ns(1.5e6);
        assert!(g > KEY_GAP_MIN_NS && g < KEY_GAP_MAX_NS, "{g}");
        p.sent(400_000, 46);
        assert_eq!(p.gap_ns(1.5e6), KEY_GAP_MAX_NS);
    }

    #[test]
    fn a_keyframe_on_a_governed_link_is_sized_to_the_link_and_bounded() {
        let mut p = KeyPolicy::default();
        // nothing known yet: the boost
        assert_eq!(p.key_qp(24, 5e6, false), 24 + KEYFRAME_BOOST);
        // 400 KB at QP 15 into 5 Mbps: a 50 KB burst is three halvings, eight steps each
        p.sent(400_000, 15);
        assert_eq!(p.key_qp(24, 5e6, false), 39);
        // never past the bound, never under the frames' own QP
        assert_eq!(p.key_qp(30, 1.5e6, false), KEY_QP_MAX);
        assert_eq!(p.key_qp(48, 1.5e6, false), 48);
        // one that fits gets the boost and no more
        p.sent(20_000, 30);
        assert_eq!(p.key_qp(24, 5e6, false), 30);
        // two keyframes at different QPs show how size answers QP here
        let mut p = KeyPolicy::default();
        p.sent(400_000, 15);
        p.sent(100_000, 27);
        assert!(p.key_halving < KEY_HALVING, "{}", p.key_halving);
        assert!(p.key_halving >= SLOPE_MIN);
    }

    /// The browser's periodic PLIs (an encoded-transform page's idle decoder asks about every
    /// 280 ms, and loss opens the back-off) on a 5 Mbps cap: with keyframes answered at once the
    /// loop the live runs showed comes back every few seconds; governed, the stream fits and the
    /// frame rate is never touched.
    #[test]
    fn periodic_plis_on_a_5_mbps_cap_no_longer_cut_the_frame_rate() {
        let run = |keys: Keys| {
            let mut g = governors();
            let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 5e6 }), 301);
            sim.keys = keys;
            sim.key_halving = 10.0;
            sim.pli_every_ms = Some(280);
            sim.refine = Some(0.5);
            sim.run(&mut g, 40, &steady(11.0));
            sim
        };
        let before = run(Keys::Boosted);
        let after = run(Keys::Governed);
        for (name, sim) in [("answered at once", &before), ("governed", &after)] {
            report(name, sim, 10_000);
            eprintln!(
                "  {name}: {} keyframes in the capped 30 s, {} frame-rate moves, {} late or \
                 broken frames after the first 4 s",
                sim.keyframes_in(10_000, 40_000),
                sim.trace.iter().filter(|t| t.2 > 1).count(),
                sim.bad_frames(14_000, 40_000, 60.0),
            );
        }
        assert!(after.trace.iter().all(|t| t.2 == 1), "{:?}", after.trace);
        assert!(
            after.keyframes_in(12_000, 40_000) as u64 <= 28_000 * MS / KEY_GAP_MIN_NS + 1,
            "{:?}",
            after.keyframes
        );
        assert!(
            after.bad_frames(14_000, 40_000, 60.0) < before.bad_frames(14_000, 40_000, 60.0),
            "governed keyframes did not help"
        );
    }

    /// The same at 1.5 Mbps, where even a keyframe at the bound does not fit a burst: one per
    /// gap, and the frame rate is still never touched for it.
    #[test]
    fn periodic_plis_on_a_1_5_mbps_cap_leave_the_frame_rate_alone() {
        let mut g = governors();
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 1.5e6 }), 303);
        sim.keys = Keys::Governed;
        sim.key_halving = 10.0;
        sim.pli_every_ms = Some(280);
        sim.refine = Some(0.5);
        sim.run(&mut g, 40, &steady(11.0));
        report("cap1_5 with PLIs, governed", &sim, 10_000);
        eprintln!("  keyframes {:?}", sim.keyframes);
        assert!(sim.trace.iter().all(|t| t.2 == 1), "{:?}", sim.trace);
        assert!(sim.keyframes_in(12_000, 40_000) as u64 <= 28_000 * MS / KEY_GAP_MIN_NS + 1);
        assert!(sim
            .keyframes
            .iter()
            .filter(|k| k.0 >= 12_000)
            .all(|k| k.2 <= KEY_QP_MAX));
    }

    /// The page asks for a keyframe when its decoder cannot go on: on a governed link that is
    /// answered within a frame, every time.
    #[test]
    fn the_pages_own_keyframe_request_is_answered_at_once_on_a_governed_link() {
        let mut g = governors();
        let mut sim = Sim::new(capped(|t| if t < 5_000 * MS { 1e9 } else { 5e6 }), 311);
        sim.keys = Keys::Governed;
        sim.key_halving = 10.0;
        sim.page_every_ms = Some(700);
        sim.run(&mut g, 20, &steady(11.0));
        assert!(sim.q_at(19_000).0 > 15, "never engaged: {:?}", sim.trace);
        for n in 8..28u64 {
            let asked = n * 700;
            assert!(
                sim.keyframes
                    .iter()
                    .any(|k| k.0 + 1 >= asked && k.0 <= asked + 2 * PERIOD_NS / MS),
                "the request at {asked} ms waited: {:?}",
                sim.keyframes
            );
        }
    }

    /// On links that never give a congestion verdict (a clean LAN, random and burst loss, a
    /// jittery path) a governor with every switch on sends the same keyframes, at the same QP and
    /// the same moments, as no governor at all, and never moves the QP or the frame rate.
    #[test]
    fn with_no_congestion_verdict_keyframes_qp_and_frame_rate_are_as_without_a_governor() {
        let lossy = Path {
            cap: |_| 200e6,
            queue_s: 0.2,
            prop_us: 20_000,
            jitter_us: 10_000.0,
            loss: 0.02,
            stall: None,
            delay_step: None,
            loss_run: 3,
        };
        for (i, path) in [lan(), lossy].into_iter().enumerate() {
            let run = |keys: Keys, governed: bool| {
                let mut g = if governed {
                    governors()
                } else {
                    Governors::new(15, None, 60, false)
                };
                let mut sim = Sim::new(path.clone(), 401 + i as u64);
                sim.keys = keys;
                sim.pli_every_ms = Some(280);
                sim.page_every_ms = Some(1_300);
                sim.run(&mut g, 60, &desktop(4));
                sim
            };
            let bare = run(Keys::Plain, false);
            let gov = run(Keys::Governed, true);
            assert_eq!(gov.moves(), 0, "path {i}: {:?}", gov.trace);
            assert!(!bare.keyframes.is_empty());
            assert_eq!(gov.keyframes, bare.keyframes, "path {i}");
            assert_eq!(gov.frames, bare.frames, "path {i}");
        }
    }

    /// A classic capture holds its gated frames (frame hold) but is not the trailing edge: past
    /// the bound it cuts the frame rate, and the QP never goes past the bound.
    #[test]
    fn a_classic_path_cuts_the_frame_rate_after_the_bound_and_stops_the_qp_there() {
        let mut g = governors();
        g.hold_frames(true);
        g.last_resort(false);
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 1.5e6 }), 321);
        sim.run(&mut g, 40, &steady(80.0));
        report("classic, 80 Mbps into 1.5", &sim, 10_000);
        assert!(sim.trace.iter().any(|t| t.2 > 1), "{:?}", sim.trace);
        assert!(
            sim.trace.iter().all(|t| t.1 <= QP_LINK_MAX),
            "{:?}",
            sim.trace
        );
    }

    #[test]
    fn without_frame_hold_only_the_qp_answers_and_stops_at_the_bound() {
        let mut g = governors();
        g.hold_frames(false);
        let mut sim = Sim::new(capped(|t| if t < 10_000 * MS { 1e9 } else { 1.5e6 }), 131);
        sim.run(&mut g, 30, &steady(40.0));
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
            // each episode waits for the standing queue to be sustained before engaging
            assert!(
                late * 100 < 10 * FPS as usize * 8,
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
        assert!(sim.settled_after(15_000, 60.0, 1_000).unwrap() <= 2_500);
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
        set_frame_hold(&t, true, true);
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

    /// A pipeline rebuilt while the link is paced starts clean: its first frames are not held
    /// behind the old backlog, and the writer stops pacing, refusing resends and withdrawing
    /// loss for a governor that is gone. An older control dropped late clears nothing.
    #[test]
    fn a_rebuild_while_paced_leaves_nothing_of_the_old_governor() {
        let t = armed(18);
        let paced = |l: &LinkShared| {
            l.pace_bps.store(1_500_000, Ordering::Relaxed);
            l.engaged.store(true, Ordering::Relaxed);
            let later = crate::streamer::audio::host_now_ns() + 5_000 * MS;
            l.writer_until_ns.store(later, Ordering::Relaxed);
            l.send_until_ns.store(later, Ordering::Relaxed);
            l.key_origins.store(key_origin::LAG, Ordering::Relaxed);
            l.divisor.store(2, Ordering::Relaxed);
        };
        let clean = |l: &LinkShared| {
            l.pace_bps() == 0
                && !l.engaged()
                && !l.sender_busy()
                && l.busy_until_ns() == 0
                && l.key_origins.load(Ordering::Relaxed) == 0
                && l.frame_divisor() == 1
        };
        let old = QpControl::build(&t, true, false, false).unwrap();
        paced(&t.link);
        assert!(t.link.sender_busy());
        drop(old);
        assert!(clean(&t.link), "dropped while paced");
        // rebuilt before the old one is dropped: the build clears, the late drop leaves it be
        let old = QpControl::build(&t, true, false, false).unwrap();
        paced(&t.link);
        let new = QpControl::build(&t, true, false, false).unwrap();
        assert!(clean(&t.link), "built over a paced one");
        t.link.pace_bps.store(2_000_000, Ordering::Relaxed);
        drop(old);
        assert_eq!(
            t.link.pace_bps(),
            2_000_000,
            "a late drop touched the new one"
        );
        drop(new);
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
        c.before_submit(&mut f);
        c.before_submit(&mut f);
        // the frames after it walk back three steps a frame
        assert_eq!(f.set, vec![18, 25 + KEYFRAME_BOOST, 28, 25, 25]);
    }

    /// Under the other switches nothing is as the link side made it: with only the ceiling
    /// engaged the frame after a boosted keyframe goes straight back, and the client's pace
    /// cuts the frame rate only on the ring's trailing edge, never through the classic path's
    /// held copy.
    #[test]
    fn the_ceiling_and_the_client_pace_behave_as_before_the_link_side() {
        let t = armed(18);
        let mut c = QpControl::build(&t, false, true, false).unwrap();
        let mut f = Fake {
            set: Vec::new(),
            ok: true,
        };
        c.gov.ceiling.as_mut().unwrap().q = 25;
        c.keyframe_next(true);
        c.before_submit(&mut f);
        c.before_submit(&mut f);
        assert_eq!(f.set, vec![25 + KEYFRAME_BOOST, 25]);
        drop(c);
        let t = armed(18);
        let mut c = QpControl::build(&t, false, false, true).unwrap();
        c.pace.as_mut().unwrap().div = 2;
        for trailing in [false, true] {
            set_frame_hold(&t, true, trailing);
            c.before_submit(&mut f);
            assert_eq!(t.link.frame_divisor(), if trailing { 2 } else { 1 });
        }
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

    /// x264 widened under a governor, at a user QP of 5 and of 18: idle, a keyframe keeps
    /// libx264's own I-frame offset and every picture the user's QP; engaged, the keyframe carries
    /// the QP the governor chose and the frames after it walk back to the governed QP.
    #[test]
    fn x264_under_a_governor_codes_keyframes_as_asked_idle_and_engaged() {
        use super::super::x264::encoder::X264Encoder;
        struct X<'a>(&'a mut X264Encoder);
        impl QpEncoder for X<'_> {
            fn set_qp(&mut self, qp: u8) -> Result<bool> {
                self.0.set_qp(qp)
            }
            fn qp_top(&self) -> u8 {
                self.0.qp_range().map_or(51, |(_, top)| top)
            }
        }
        for base in [5u8, 18] {
            let mut e = match x264_open(cfg(base), true) {
                Ok(e) => e,
                Err(e) => return crate::test_support::skip("x264 under a governor", e),
            };
            let (key, top) = e.qp_range().expect("widened");
            let t = armed(base);
            let mut c = QpControl::build(&t, true, false, false).unwrap();
            let mut reader = SliceQp::default();
            let mut buf = vec![0u8; 640 * 360 * 4];
            let mut frame = |c: &mut QpControl, e: &mut X264Encoder, k: u32, keyframe: bool| {
                fill(&mut buf, 640, 360, k);
                c.keyframe_next(keyframe);
                c.before_submit(&mut X(e));
                let au = e.encode_bgra(&buf, keyframe).unwrap();
                c.emitted(&au, false);
                reader.read(&au, true)
            };
            assert_eq!(
                frame(&mut c, &mut e, 0, true),
                (Some(key), true),
                "QP {base}"
            );
            assert_eq!(frame(&mut c, &mut e, 1, false), (Some(base), false));
            assert_eq!(
                frame(&mut c, &mut e, 2, true),
                (Some(key), true),
                "QP {base}"
            );
            assert_eq!(frame(&mut c, &mut e, 3, false), (Some(base), false));
            c.gov.link.as_mut().unwrap().q = 30;
            assert_eq!(frame(&mut c, &mut e, 4, false), (Some(30), false));
            let want = (30 + KEYFRAME_BOOST).min(top);
            assert_eq!(
                frame(&mut c, &mut e, 5, true),
                (Some(want), true),
                "QP {base}"
            );
            let walk: Vec<_> = (6..9).map(|k| frame(&mut c, &mut e, k, false).0).collect();
            assert_eq!(walk, [Some(want - 3), Some(30), Some(30)], "QP {base}");
            assert_eq!(t.link.mismatches.load(Ordering::Relaxed), 0, "QP {base}");
        }
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
        // the link governor ships on, so a session opens x264 widened; without it CQP is left alone
        assert!(X264Options::from_switches().wide_qp_clamp);
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
            // under 4 libx264's keyframe offset leaves 39 above it, and the governor's bound
            // follows the encoder's top there (`QpEncoder::qp_top`)
            if base >= 4 {
                assert!(top >= QP_LINK_MAX.max(base), "setting {base}: top {top}");
            }
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
            for (i, q) in [QP_LINK_MAX.max(base).min(top), top, base]
                .into_iter()
                .enumerate()
            {
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

    // pacing to the link

    #[test]
    fn the_link_pacer_holds_its_rate_and_says_how_far_behind_it_is() {
        let t0 = std::time::Instant::now();
        let mut p = LinkPacer::default();
        assert_eq!(
            p.schedule(1_200, t0),
            std::time::Duration::ZERO,
            "no rate, no pacing"
        );
        p.set_rate(2_000_000);
        let mut now = t0;
        for _ in 0..500 {
            now += p.schedule(1_200, now);
        }
        // 600 KB at 2 Mbps is 2.4 s, less the opening burst
        let took = now.duration_since(t0).as_secs_f64();
        assert!((2.3..2.45).contains(&took), "{took}");
        // a 290 KB keyframe on top of an empty bucket waits 1.16 s to leave
        let later = now + std::time::Duration::from_secs(5);
        let left = p.backlog(290_000, later).as_secs_f64();
        assert!((1.14..1.17).contains(&left), "{left}");
    }

    #[test]
    fn the_send_gate_is_busy_while_a_frame_period_waits_and_counts_skips() {
        let mut g = SendGate::default();
        let period = 16_666_667;
        assert!(!g.busy(0, 0, period));
        // 290 KB at 1.35 Mbps: 1.7 s of sending
        g.sent(0, 290_000, 1.35e6, 0, true);
        assert!(g.busy(100 * MS, 0, period));
        assert!(!g.key_clear(1_000 * MS));
        assert!(!g.busy(1_710 * MS, 0, period));
        assert!(g.key_clear(1_820 * MS));
        // the writer may know better: it is still sending
        assert!(g.busy(1_710 * MS, 3_000 * MS, period));
        for k in 0..30u64 {
            if k % 3 == 0 {
                g.skipped(2_000 * MS + k * period, 1);
            } else {
                g.sent(2_000 * MS + k * period, 1_000, 1.35e6, 0, false);
            }
        }
        let share = g.skip_share(2_000 * MS + 30 * period);
        assert!((0.25..0.4).contains(&share), "{share}");
        assert_eq!(g.skip_share(10_000 * MS), 0.0, "old skips are forgotten");
    }

    /// Paced, a congested link sets the pace to what it delivered; the QP rises against what the
    /// sender skips, to the bound and no further while fewer than half the frames wait, past it
    /// toward the last resort once half do, and the divisor is never used.
    #[test]
    fn paced_the_qp_rises_against_skips_to_the_bound_and_past_it_only_at_half_the_frames() {
        for (skip, top) in [(0.3, QP_LINK_MAX), (0.5, QP_LAST_RESORT)] {
            let mut l = LinkGovernor::new(15);
            l.set_pacing(true);
            let w = Window {
                valid: true,
                packets: 100,
                share: 0.8,
                sigma: 1.0,
                queue_us: 60_000,
                delivered_bps: 1.5e6,
                sent_bps: 3e6,
                loss: 0.2,
            };
            for k in 0..30u64 {
                l.encoded(k * 16 * MS, 20_000, false);
            }
            l.pace_to(500 * MS, &w);
            assert!((l.pace_bps() - 1.35e6).abs() < 1.0, "{}", l.pace_bps());
            assert!(l.engaged());
            let mut t = 500 * MS;
            for _ in 0..40 {
                t += 500 * MS;
                for k in 0..30u64 {
                    l.encoded(t - 480 * MS + k * 16 * MS, 20_000, false);
                }
                l.sender(skip, true);
                l.paced(t);
            }
            assert_eq!(l.q(), top, "{skip}");
            assert_eq!(l.divisor(), 1);
            l.raise(t + 2_000 * MS, top, w);
            assert_eq!((l.q(), l.divisor()), (top, 1));
        }
    }

    /// A paced QP that frames waiting forced up is held: a light second right after it does
    /// not bring it down, and a step down later goes one or two steps on seconds of evidence.
    #[test]
    fn a_raise_that_skipping_forced_is_held_and_comes_down_a_step_at_a_time() {
        let mut l = LinkGovernor::new(15);
        l.set_pacing(true);
        l.q = 30;
        l.pace_bps = 4.5e6;
        l.clear_since_ns = Some(0);
        l.ramp_not_before_ns = u64::MAX;
        let mut t = 0;
        let feed = |l: &mut LinkGovernor, t: u64, frame: u64| {
            for k in 0..6u64 {
                l.encoded(t - 100 * MS + k * 16 * MS, frame, false);
            }
        };
        t += 100 * MS;
        feed(&mut l, t, 6_000);
        l.sender(0.3, false);
        let m = l.paced(t).expect("a raise");
        assert!(m.raise && m.q == 33, "{m:?}");
        // light from here on: nothing for six seconds, then single steps two seconds apart
        let mut downs = Vec::new();
        while t < 16_000 * MS {
            t += 100 * MS;
            feed(&mut l, t, 1_500);
            l.sender(0.0, false);
            if let Some(m) = l.paced(t) {
                assert!(!m.raise, "{m:?}");
                downs.push((t / MS, m.q));
            }
        }
        assert!(downs.first().is_some_and(|d| d.0 >= 6_100), "{downs:?}");
        assert!(
            downs
                .windows(2)
                .all(|w| w[1].0 - w[0].0 >= 2_000 && w[0].1 - w[1].1 <= 2),
            "{downs:?}"
        );
    }

    /// Paced on a clear link the ramp walks the QP down while the stream leaves room under the
    /// pace and the pace up while the stream fills it, and once the stream at the user's QP has
    /// passed with nothing waiting, pacing lets go.
    #[test]
    fn paced_recovery_ramps_the_qp_down_and_the_pace_up_and_lets_go() {
        let mut l = LinkGovernor::new(15);
        l.set_pacing(true);
        l.q = QP_LINK_MAX;
        l.pace_bps = 1.35e6;
        l.clear_since_ns = Some(0);
        let mut t = 0;
        let mut moves = 0;
        // 0.4 Mbps at QP 40, halving every 10 steps
        while t < 20_000 * MS && l.engaged() {
            t += 100 * MS;
            let frame = 800.0 * 2f64.powf((QP_LINK_MAX - l.q()) as f64 / 10.0);
            for k in 0..6u64 {
                l.encoded(t - 100 * MS + k * 16 * MS, frame as u64, false);
            }
            l.sender(0.0, false);
            if l.paced(t).is_some() {
                moves += 1;
            }
            // a link that carries all of it
            if let Some(r) = l.ramp.as_mut() {
                r.carried = l.pace_bps;
            }
        }
        assert!(!l.engaged(), "q {} pace {}", l.q(), l.pace_bps());
        assert!(t <= 4_000 * MS, "took {} ms over {moves} moves", t / MS);
        assert_eq!((l.q(), l.divisor()), (15, 1));
        assert_eq!(l.probes, 1);
    }

    /// A ramp that meets a queue goes back to the pace and the QP it left, and the next waits
    /// twice as long, up to the max; one that found room starts the hold again.
    #[test]
    fn a_ramp_that_meets_a_queue_goes_back_and_waits_twice_as_long_next_time() {
        let mut l = LinkGovernor::new(15);
        l.set_pacing(true);
        l.q = 34;
        l.pace_bps = 4.5e6;
        l.lower_not_before_ns = u64::MAX;
        let mut t = 0;
        let mut holds = Vec::new();
        for _ in 0..5 {
            t += 10_000 * MS;
            l.clear_since_ns = Some(t - RAMP_QUIET_NS);
            for k in 0..30u64 {
                l.encoded(t - 480 * MS + k * 16 * MS, 4_000, false);
            }
            l.sender(0.0, false);
            l.paced(t);
            assert!(l.attempting());
            l.ramp.as_mut().unwrap().rose = true;
            let m = l.paced(t + 100 * MS).expect("the ramp ends");
            // back to where the content fits at the old pace, never above where it left
            assert!(m.q <= 34 && l.pace_bps() == 4.5e6, "{m:?}");
            holds.push(l.ramp_not_before_ns - (t + 100 * MS));
        }
        assert_eq!(RAMP_HOLD_MAX_NS, 2 * RAMP_HOLD_MIN_NS);
        assert_eq!(holds, [RAMP_HOLD_MAX_NS; 5], "{holds:?}");
        // room found: paced at what was carried, QP back by what that gains, hold reset
        t += 10_000 * MS;
        l.clear_since_ns = Some(t - RAMP_QUIET_NS);
        l.paced(t);
        l.ramp.as_mut().unwrap().carried = 7e6;
        l.ramp.as_mut().unwrap().rose = true;
        let m = l.paced(t + 100 * MS).expect("the ramp ends");
        assert!((l.pace_bps() - 6.3e6).abs() < 1.0, "{}", l.pace_bps());
        assert!(m.q < 34, "{m:?}");
        assert_eq!(l.ramp_not_before_ns, t + 100 * MS + RAMP_HOLD_MIN_NS);
    }

    /// Paced, a keyframe goes once the last one has drained, whatever asked for it, and is held
    /// (never lost) until then.
    #[test]
    fn paced_keyframes_wait_only_for_the_last_one_to_drain() {
        let mut p = KeyPolicy::default();
        assert!(p.admit_paced(0, true, true, true, false));
        assert!(!p.admit_paced(10 * MS, true, true, false, true));
        assert!(
            !p.admit_paced(20 * MS, false, false, false, true),
            "held, not lost"
        );
        assert!(
            p.admit_paced(30 * MS, false, false, true, false),
            "urgent, held as such"
        );
        assert!(
            !p.admit_paced(40 * MS, false, false, true, true),
            "nothing asked"
        );
        // a routine request waits for its own gap as well
        assert!(!p.admit_paced(50 * MS, true, false, true, false));
        assert!(p.admit_paced(60 * MS, false, false, true, true));
        assert_eq!(p.held_requests, 2);
    }

    /// With `link_pace` on and nothing congested: nothing is paced, nothing waits at the sender,
    /// every keyframe request goes as it came, and the QP is the user's.
    #[test]
    fn with_link_pace_on_and_no_congestion_nothing_is_paced_skipped_or_held() {
        let t = armed(18);
        let mut c = QpControl::build(&t, true, true, true)
            .unwrap()
            .with_link_pace(true);
        let mut f = Fake {
            set: Vec::new(),
            ok: true,
        };
        let mut r = Rng(11);
        for _ in 0..600 {
            let asked = r.next() < 0.2;
            if r.next() < 0.1 {
                t.link.note_key_origin(key_origin::PLI);
            }
            let key = c.admit_keyframe(asked);
            assert_eq!(key, asked);
            c.keyframe_next(key);
            c.before_submit(&mut f);
            let au: &[u8] = if key {
                &[0, 0, 0, 1, 0x65, 0x88]
            } else {
                &[0, 0, 0, 1, 0x41, 0x9a]
            };
            c.emitted(au, false);
            assert!(!t.link.sender_busy());
            assert!(!c.keyframe_due());
        }
        assert!(f.set.iter().all(|q| *q == 18));
        assert_eq!(t.link.pace_bps(), 0);
        assert!(!t.link.engaged());
        assert_eq!(t.link.busy_until_ns(), 0);
        assert_eq!(t.link.frame_divisor(), 1);
    }

    // the onset, the periodic PLI, a keyframe's own queue

    fn queued(q: i64) -> Window {
        Window {
            valid: true,
            packets: 50,
            share: 1.0,
            sigma: 1.0,
            queue_us: q,
            delivered_bps: 20e6,
            sent_bps: 20e6,
            loss: 0.0,
        }
    }

    /// A queue rising by 10 ms twice in 600 ms is congestion; the same rise with nothing lost
    /// within 1.5 s of a keyframe is the keyframe's own burst draining (Quick Sync under a
    /// relay's per-packet jitter read 67 ms behind one).
    #[test]
    fn a_lossless_queue_behind_our_own_keyframe_is_not_congestion() {
        for keyframe in [false, true] {
            let mut l = LinkGovernor::new(15);
            l.queue_hist.push_back((0, 20_000, 1.0));
            l.queue_hist.push_back((300 * MS, 45_000, 1.0));
            if keyframe {
                l.encoded(100 * MS, 300_000, true);
            }
            let v = l.judge(600 * MS, &queued(67_000), &queued(60_000));
            if keyframe {
                assert_ne!(v, Verdict::Congested);
            } else {
                assert_eq!(v, Verdict::Congested);
            }
        }
        // loss behind the keyframe is still the link
        let mut l = LinkGovernor::new(15);
        l.encoded(100 * MS, 300_000, true);
        let lossy = Window {
            share: 0.5,
            ..queued(80_000)
        };
        assert_eq!(l.judge(600 * MS, &lossy, &lossy), Verdict::Congested);
    }

    /// The pace starts from the best reading of the link the last second gave (a window read
    /// during the loss burst undershoots by half), and a window the link carried in full never
    /// lowers it.
    #[test]
    fn the_pace_follows_the_link_and_never_a_sample_the_sender_limited() {
        let mut l = LinkGovernor::new(15);
        l.set_pacing(true);
        let w = |q: i64, delivered: f64, sent: f64| Window {
            delivered_bps: delivered,
            sent_bps: sent,
            share: 0.3,
            ..queued(q)
        };
        // the link delivered 5.1 Mbps in one window and 2.3 in the one that triggered
        l.judge(200 * MS, &w(60_000, 5.1e6, 11e6), &Window::default());
        l.pace_to(400 * MS, &w(80_000, 2.3e6, 11e6));
        assert!((l.pace_bps() - 0.9 * 5.1e6).abs() < 1.0, "{}", l.pace_bps());
        // paced, a window that arrived as fast as it was sent leaves the pace alone
        l.pace_to(900 * MS, &w(50_000, 1.9e6, 1.95e6));
        assert!((l.pace_bps() - 0.9 * 5.1e6).abs() < 1.0, "{}", l.pace_bps());
        // one the link did not carry lowers it, never raises it
        l.pace_to(1_000 * MS, &w(50_000, 4.0e6, 4.6e6));
        assert!((l.pace_bps() - 0.9 * 5.1e6).abs() < 1.0, "{}", l.pace_bps());
        l.link_hist.clear();
        l.pace_to(3_000 * MS, &w(50_000, 4.0e6, 4.6e6));
        assert!((l.pace_bps() - 3.6e6).abs() < 1.0, "{}", l.pace_bps());
    }

    #[test]
    fn a_pli_from_a_painting_page_is_not_a_reason_for_a_keyframe() {
        let s = 1_000 * MS;
        assert!(pli_is_noise(key_origin::PLI, true, 5 * s));
        assert!(
            !pli_is_noise(key_origin::PLI, false, 5 * s),
            "a page not painting"
        );
        assert!(
            !pli_is_noise(key_origin::PLI, true, 61 * s),
            "the healthy cadence"
        );
        assert!(!pli_is_noise(
            key_origin::PLI | key_origin::PAGE,
            true,
            5 * s
        ));
        assert!(!pli_is_noise(key_origin::UNREPAIRABLE, true, 5 * s));
        assert!(!pli_is_noise(0, true, 5 * s), "an untagged request");
    }

    // the governor stays out of the way: the uncapped scenarios of the browser harness

    /// Quick Sync's pattern: frames swing from a third to twice the average (sliced, with big
    /// refreshes), and a 400 KB keyframe every 5 s, as the jitter run had.
    fn quick_sync_like(seed: u64) -> impl Fn(u64) -> (u64, bool) {
        move |k| {
            if k % (5 * FPS) == 0 {
                return (400_000, true);
            }
            let mut r = Rng((seed ^ k.wrapping_mul(0xD6E8_FEB8_6659_FD93)) | 1);
            let x = r.next();
            let swing = if x < 0.1 { 2.2 } else { 0.35 + 1.1 * r.next() };
            ((14e6 / 8.0 / FPS as f64 * swing) as u64, false)
        }
    }

    /// NVENC's: the desktop's frame-to-frame variation and scene changes, and a keyframe every
    /// 5 s.
    fn nvenc_like(seed: u64) -> impl Fn(u64) -> (u64, bool) {
        let d = desktop(seed);
        move |k| {
            if k % (5 * FPS) == 0 {
                return (400_000, true);
            }
            d(k)
        }
    }

    /// Stall, random and burst loss, loss on the return path, delay jitter of +-10 and +-30 ms,
    /// a step in base delay and a loss step: on either encoder's pattern, with pacing on as it
    /// ships, the link side never engages, never moves anything, and every keyframe goes as asked.
    #[test]
    fn no_uncapped_scenario_ever_engages_the_link_side() {
        struct Case {
            name: &'static str,
            loss: f64,
            loss_run: u32,
            jitter_ms: f64,
            stall: Option<(u64, u64)>,
            step: bool,
            report_loss: f64,
            loss_from_s: u64,
        }
        let base = Case {
            name: "",
            loss: 0.0,
            loss_run: 0,
            jitter_ms: 1.0,
            stall: None,
            step: false,
            report_loss: 0.0,
            loss_from_s: 0,
        };
        let cases = [
            Case {
                name: "stall200",
                stall: Some((3, 200)),
                ..base
            },
            Case {
                name: "loss1",
                loss: 0.01,
                ..base
            },
            Case {
                name: "loss5",
                loss: 0.05,
                ..base
            },
            Case {
                name: "burst",
                loss: 0.01,
                loss_run: 4,
                ..base
            },
            Case {
                name: "loss5_up",
                report_loss: 0.05,
                ..base
            },
            Case {
                name: "delay_jitter 10",
                jitter_ms: 20.0,
                ..base
            },
            Case {
                name: "delay_jitter 30",
                jitter_ms: 60.0,
                ..base
            },
            Case {
                name: "delay_step",
                step: true,
                ..base
            },
            Case {
                name: "loss_step3",
                loss: 0.03,
                loss_from_s: 10,
                ..base
            },
        ];
        for (i, c) in cases.iter().enumerate() {
            for (pattern, content) in [
                (
                    "nvenc",
                    Box::new(nvenc_like(i as u64 + 3)) as Box<dyn Fn(u64) -> (u64, bool)>,
                ),
                ("quick sync", Box::new(quick_sync_like(i as u64 + 5))),
            ] {
                let mut g = governors();
                g.set_pacing(true);
                let path = Path {
                    cap: |_| 200e6,
                    queue_s: 0.2,
                    prop_us: 20_000,
                    jitter_us: c.jitter_ms * 1e3,
                    loss: c.loss,
                    stall: c.stall,
                    delay_step: c.step.then_some((10_000 * MS, 60_000)),
                    loss_run: c.loss_run,
                };
                let mut sim = Sim::new(path, 900 + i as u64);
                sim.report_loss = c.report_loss;
                sim.loss_from_ns = c.loss_from_s * 1_000 * MS;
                sim.keys = Keys::Governed;
                sim.pli_every_ms = Some(280);
                sim.run(&mut g, 40, &*content);
                let l = g.link.as_ref().unwrap();
                assert_eq!(
                    (l.engagements, sim.moves(), g.pace_bps()),
                    (0, 0, 0.0),
                    "{} on {pattern}: {} / {:?}",
                    c.name,
                    l.last_engagement,
                    sim.trace
                );
            }
        }
    }

    /// Heavy loss with no capacity limit behind it, and a route change with light loss: neither
    /// is a bottleneck, and on either encoder's pattern the link side never engages. At 15 % or
    /// more the shortfall is the loss itself, and a step in the path's delay stands flat.
    #[test]
    fn heavy_loss_and_a_route_change_with_light_loss_never_engage() {
        struct Case {
            name: String,
            loss: f64,
            loss_run: u32,
            from_s: u64,
            until_s: u64,
            step_us: Option<i64>,
        }
        let mut cases = Vec::new();
        for loss in [0.15, 0.2, 0.3] {
            cases.push(Case {
                name: format!("{:.0} % for 5 s", loss * 100.0),
                loss,
                loss_run: 0,
                from_s: 10,
                until_s: 15,
                step_us: None,
            });
        }
        // bursts of 30 packets: most of two seconds gone, as in a dead zone
        cases.push(Case {
            name: "an outage-like 2 s".into(),
            loss: 0.05,
            loss_run: 30,
            from_s: 10,
            until_s: 12,
            step_us: None,
        });
        for step_ms in [40, 60, 100] {
            for loss in [0.005, 0.01, 0.03] {
                cases.push(Case {
                    name: format!("a {step_ms} ms step at {:.1} % loss", loss * 100.0),
                    loss,
                    loss_run: 0,
                    from_s: 0,
                    until_s: 1_000,
                    step_us: Some(step_ms * 1_000),
                });
            }
        }
        let mut failed = Vec::new();
        for (i, c) in cases.iter().enumerate() {
            for (pattern, content) in [
                (
                    "nvenc",
                    Box::new(nvenc_like(i as u64 + 31)) as Box<dyn Fn(u64) -> (u64, bool)>,
                ),
                ("quick sync", Box::new(quick_sync_like(i as u64 + 37))),
            ] {
                let mut g = governors();
                g.set_pacing(true);
                let path = Path {
                    cap: |_| 200e6,
                    queue_s: 0.2,
                    prop_us: 20_000,
                    jitter_us: 1_000.0,
                    loss: c.loss,
                    stall: None,
                    delay_step: c.step_us.map(|us| (10_000 * MS, us)),
                    loss_run: c.loss_run,
                };
                let mut sim = Sim::new(path, 700 + i as u64);
                sim.loss_from_ns = c.from_s * 1_000 * MS;
                sim.loss_until_ns = c.until_s * 1_000 * MS;
                sim.keys = Keys::Governed;
                sim.pli_every_ms = Some(280);
                sim.run(&mut g, 30, &*content);
                let l = g.link.as_ref().unwrap();
                if (l.engagements, sim.moves(), g.pace_bps()) != (0, 0, 0.0) {
                    failed.push(format!("{} on {pattern}: {}", c.name, l.last_engagement));
                }
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// A cap behind a 100 ms buffer: the queue it overflows is short, and it still engages, with
    /// the queue and the loss both there.
    #[test]
    fn a_cap_behind_a_short_buffer_still_engages() {
        for (bps, pattern) in [(5e6, 0u64), (2e6, 1)] {
            let mut g = governors();
            g.set_pacing(true);
            let mut path = capped(if pattern == 0 {
                |t| if t < 5_000 * MS { 1e9 } else { 5e6 }
            } else {
                |t| if t < 5_000 * MS { 1e9 } else { 2e6 }
            });
            path.queue_s = 0.1;
            let mut sim = Sim::new(path, 811 + pattern);
            sim.keys = Keys::Governed;
            sim.run(&mut g, 15, &nvenc_like(41 + pattern));
            let l = g.link.as_ref().unwrap();
            assert_eq!(l.engagements, 1, "{bps}: {:?}", sim.trace);
            assert!(g.engaged(), "{bps}: {}", l.last_engagement);
        }
    }
}

#[cfg(test)]
#[path = "test/link_sim.rs"]
mod link_sim;

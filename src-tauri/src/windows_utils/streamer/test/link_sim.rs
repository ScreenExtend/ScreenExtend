//! A capped link end to end, a millisecond at a time: the encoder at the governed QP, the
//! sender (unpaced, or paced to the link with frames skipped against its backlog), a drop-tail
//! bottleneck with a 100 ms queue, transport-wide feedback, a receiver that asks for every lost
//! packet again, the budgeted resender, and a decoder that stops on any frame it can never
//! complete until a keyframe arrives whole. Sizes are the live content's: 10 to 13 Mbit/s at
//! QP 15, about 2.1 at QP 40, a keyframe about 290 KB at QP 46.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::*;
use crate::streamer::keyframe::{KeyframeGate, PliBackoff, PLI_BACKOFF_CAP_MS};
use crate::windows_utils::streamer::rtx::{Refusal, Retransmitter};

const PKT: u32 = 1_200;
const FPS: u64 = 60;
const PROP_MS: u64 = 2;
const CLOCK_OFFSET_US: i64 = 7_000_000_000;
/// P frames: this many bits a second at QP 15, halving every `P_HALVING` QP steps.
const P15_BPS: f64 = 12e6;
const P_HALVING: f64 = 10.0;
/// Keyframes: 290 KB at QP 46, halving every 16 steps.
const KEY46: f64 = 290_000.0;
const KEY_HALVING_LIVE: f64 = 16.0;
/// The receiver asks again this often, this many times, for a packet it is missing.
const RENACK_MS: u64 = 100;
const RENACK_TRIES: u32 = 10;
/// The idle libwebrtc decoder behind an encoded transform asks for a keyframe this often.
const IDLE_PLI_MS: u64 = 280;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Arm {
    /// Without link pacing: nothing paced, keyframes held 2 to 4 s, loss opens the
    /// PLI back-off.
    Before,
    /// Paced to the link, frames skipped at the sender, keyframes paced, one keyframe for an
    /// unrepairable loss, loss no news while engaged.
    After,
}

struct Pkt {
    frame: usize,
    bytes: u32,
    seq: u16,
    /// Left the sender (host ms, fractional).
    sent_ms: f64,
    arrival_ms: Option<f64>,
    dropped: bool,
    /// The media packet this is, or resends.
    of: usize,
    resend: bool,
}

struct Frame {
    key: bool,
    capture_ms: f64,
    media: Vec<usize>,
    complete_ms: Option<f64>,
}

struct Missing {
    of: usize,
    next_ms: u64,
    tries: u32,
}

#[derive(Default, Debug)]
struct Outcome {
    pub drops_settled: usize,
    pub packets_settled: usize,
    pub longest_freeze_ms: f64,
    pub painted_fps: f64,
    pub sent_fps: f64,
    pub p99_ms: f64,
    pub keyframes: usize,
}

struct LinkSim {
    arm: Arm,
    cap: Box<dyn Fn(u64) -> f64>,
    queue_s: f64,
    g: Governors,
    gate: SendGate,
    keys: KeyPolicy,
    rtx: Retransmitter<usize>,
    pli: PliBackoff,
    kgate: KeyframeGate,
    base: Instant,
    pkts: Vec<Pkt>,
    frames: Vec<Frame>,
    /// media packet -> earliest arrival of it or a resend
    got: Vec<Option<f64>>,
    /// media packet -> its index in `pkts`
    media_pkt: Vec<usize>,
    sendq: VecDeque<usize>,
    tokens: f64,
    /// When the bottleneck has sent everything queued (ms).
    link_free_ms: f64,
    arrivals: VecDeque<(f64, usize)>,
    highest_got: Option<usize>,
    missing: Vec<Missing>,
    nacks_in: VecDeque<(u64, usize)>,
    plis_in: VecDeque<u64>,
    reports_in: VecDeque<(u64, PathSample)>,
    reported_to: usize,
    next_seq: u16,
    idr_flag: bool,
    origins: u32,
    key_needed: bool,
    walk: u8,
    // the decoder
    next_decode: usize,
    last_paint_ms: Option<f64>,
    paints: Vec<(f64, f64)>,
    skipped: u32,
    t: u64,
    next_frame: u64,
    /// Per second: (q, divisor, pace Mbps) at its end.
    states: Vec<(u8, u32, f64)>,
    /// Per frame tick: (ms, skipped at the sender, below the QP bound with no keyframe draining,
    /// pace).
    ticks: Vec<(u64, bool, bool, f64)>,
    /// The content's weight against the base sizes, by host ms.
    content: Box<dyn Fn(u64) -> f64>,
    /// Bits this encoder spends against NVENC's at the same QP.
    gain: f64,
    /// The deepest bottleneck queue this second, and per second.
    qmax_ms: f64,
    queues: Vec<f64>,
    /// When each attempt on the link began (ms).
    attempts: Vec<u64>,
}

impl LinkSim {
    fn new(arm: Arm, cap: Box<dyn Fn(u64) -> f64>) -> Self {
        let mut g = Governors::new(15, None, FPS as u32, true);
        g.hold_frames(true);
        g.last_resort(true);
        g.set_pacing(arm == Arm::After);
        LinkSim {
            arm,
            cap,
            queue_s: 0.1,
            g,
            gate: SendGate::default(),
            keys: KeyPolicy::default(),
            rtx: Retransmitter::new(),
            pli: PliBackoff::new(),
            kgate: KeyframeGate::new(),
            base: Instant::now(),
            pkts: Vec::new(),
            frames: Vec::new(),
            got: Vec::new(),
            media_pkt: Vec::new(),
            sendq: VecDeque::new(),
            tokens: 0.0,
            link_free_ms: 0.0,
            arrivals: VecDeque::new(),
            highest_got: None,
            missing: Vec::new(),
            nacks_in: VecDeque::new(),
            plis_in: VecDeque::new(),
            reports_in: VecDeque::new(),
            reported_to: 0,
            next_seq: 0,
            idr_flag: false,
            origins: 0,
            key_needed: false,
            walk: 0,
            next_decode: 0,
            last_paint_ms: None,
            paints: Vec::new(),
            skipped: 0,
            t: 0,
            next_frame: 0,
            states: Vec::new(),
            ticks: Vec::new(),
            content: Box::new(|_| 1.0),
            gain: 1.0,
            qmax_ms: 0.0,
            queues: Vec::new(),
            attempts: Vec::new(),
        }
    }

    fn with_content(mut self, content: Box<dyn Fn(u64) -> f64>, gain: f64) -> Self {
        self.content = content;
        self.gain = gain;
        self
    }

    fn attempting(&self) -> bool {
        self.g.link.as_ref().is_some_and(|l| l.attempting())
    }

    fn at(&self, ms: f64) -> Instant {
        self.base + Duration::from_micros((ms * 1e3) as u64)
    }

    fn ask_keyframe(&mut self, now_ms: u64, origin: u32) -> bool {
        if !self.kgate.allow(now_ms * MS) {
            return false;
        }
        self.idr_flag = true;
        self.origins |= origin;
        true
    }

    fn note_tick(&mut self, now_ms: u64, skipped: bool) {
        // a probe pushes the sender past the pace on purpose, for 200 ms: its waits are its cost
        let probing = self.attempting();
        let below = self.g.q() < QP_LINK_MAX && self.gate.key_clear(now_ms * MS) && !probing;
        self.ticks.push((now_ms, skipped, below, self.paced()));
    }

    /// Of the frames due below the QP bound with no keyframe draining, the share skipped.
    fn skips_below_bound(&self, from: u64, to: u64) -> f64 {
        let (n, k) = self
            .ticks
            .iter()
            .filter(|t| (from..to).contains(&t.0) && t.2)
            .fold((0u32, 0u32), |(n, k), t| (n + 1, k + t.1 as u32));
        if n == 0 {
            0.0
        } else {
            k as f64 / n as f64
        }
    }

    /// The pace at `ms`.
    fn pace_at(&self, ms: u64) -> f64 {
        self.ticks
            .iter()
            .take_while(|t| t.0 <= ms)
            .last()
            .map_or(0.0, |t| t.3)
    }

    fn paced(&self) -> f64 {
        if self.arm == Arm::After {
            self.g.pace_bps()
        } else {
            0.0
        }
    }

    /// When what the sender holds will have left at the pace (ns).
    fn sender_until_ns(&self, now_ms: u64) -> u64 {
        let pace = self.paced();
        if pace <= 0.0 {
            return 0;
        }
        let held: u32 = self.sendq.iter().map(|&i| self.pkts[i].bytes).sum();
        let owed = (held as f64 - self.tokens).max(0.0);
        now_ms * MS + (owed * 8e9 / pace) as u64
    }

    fn frame_tick(&mut self, now_ms: u64, k: u64) {
        let now_ns = now_ms * MS;
        let period_ns = 1_000_000_000 / FPS;
        // the frame-rate divisor (round one's answer, still there without pacing)
        let d = self.g.divisor() as u64;
        if !k.is_multiple_of(d) {
            return;
        }
        if self.arm == Arm::After && self.g.pace_bps() > 0.0 {
            let writer = self.sender_until_ns(now_ms);
            if self.gate.busy(now_ns, writer, BACKLOG_PERIODS * period_ns) {
                self.skipped += 1;
                self.gate.skipped(now_ns, 1);
                self.note_tick(now_ms, true);
                return;
            }
        }
        self.note_tick(now_ms, false);
        let mut asked = std::mem::take(&mut self.idr_flag);
        let origins = std::mem::take(&mut self.origins);
        if self.arm == Arm::After && self.g.pace_bps() > 0.0 && asked {
            let painting = self
                .last_paint_ms
                .is_some_and(|p| now_ms as f64 - p < 3_000.0);
            let since = self
                .keys
                .last_key_ns
                .map_or(u64::MAX, |t| now_ns.saturating_sub(t));
            asked = !pli_is_noise(origins, painting, since);
        }
        let first = self.frames.is_empty();
        let key = first
            || match self.arm {
                Arm::After if self.g.pace_bps() > 0.0 => {
                    let clear = self.gate.key_clear(now_ns);
                    let routine = self.gate.key_routine(now_ns);
                    let urgent = origins & key_origin::URGENT != 0;
                    self.keys.admit_paced(now_ns, asked, urgent, clear, routine)
                }
                _ => self.keys.admit(
                    now_ns,
                    asked,
                    origins & key_origin::URGENT != 0,
                    self.g.link_engaged(),
                    self.g.link_bps(),
                ),
            };
        let mut q = self.g.q();
        if key && self.g.engaged() {
            q = self.keys.key_qp(q, self.g.link_bps(), self.paced() > 0.0);
            self.walk = q;
        } else if self.walk > q {
            self.walk = self.walk.saturating_sub(KEY_RETURN_STEP).max(q);
            q = self.walk;
        }
        let mut r = (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 11) as f64 / (1u64 << 53) as f64;
        r = 0.7 + 0.6 * r;
        let weight = (self.content)(now_ms);
        let bytes = if key {
            KEY46 * 2f64.powf(-(q as f64 - 46.0) / KEY_HALVING_LIVE) * self.gain
        } else {
            P15_BPS / 8.0 / FPS as f64
                * 2f64.powf(-(q as f64 - 15.0) / P_HALVING)
                * r
                * (d as f64).sqrt()
                * weight
                * self.gain
        } as u64;
        if key {
            self.keys.sent(bytes, q);
            self.key_needed = false;
        }
        let f = self.frames.len();
        let mut media = Vec::new();
        let n = bytes.div_ceil(PKT as u64).max(1);
        for i in 0..n {
            let b = if i + 1 == n {
                (bytes - PKT as u64 * (n - 1)).max(40)
            } else {
                PKT as u64
            } as u32;
            let idx = self.pkts.len();
            self.pkts.push(Pkt {
                frame: f,
                bytes: b,
                seq: self.next_seq,
                sent_ms: 0.0,
                arrival_ms: None,
                dropped: false,
                of: self.got.len(),
                resend: false,
            });
            self.next_seq = self.next_seq.wrapping_add(1);
            media.push(self.got.len());
            self.got.push(None);
            self.media_pkt.push(idx);
            self.sendq.push_back(idx);
        }
        self.frames.push(Frame {
            key,
            capture_ms: now_ms as f64,
            media,
            complete_ms: None,
        });
        let _ = self.g.emitted(now_ns, bytes, false, key, Some(q), q);
        let pace = self.paced();
        if pace > 0.0 {
            let writer = self.sender_until_ns(now_ms);
            self.gate.sent(now_ns, bytes, pace, writer, key);
        }
    }

    /// The sender's queue into the bottleneck: at once, or at the pace.
    /// The sender's queue into the bottleneck, at once or at the pace, and the bottleneck itself:
    /// a FIFO at the cap that drops what would wait past its buffer.
    fn send(&mut self, now_ms: u64) {
        let pace = self.paced();
        if pace > 0.0 {
            let burst = (pace / 8.0 * 0.004).max(2.0 * PKT as f64);
            self.tokens = (self.tokens + pace / 8.0 / 1000.0).min(burst);
        }
        let mut at = now_ms as f64;
        while let Some(&i) = self.sendq.front() {
            let b = self.pkts[i].bytes as f64;
            if pace > 0.0 {
                if self.tokens < b {
                    break;
                }
                self.tokens -= b;
                at += b * 8.0 / pace * 1e3;
            } else {
                at += 0.01;
            }
            self.sendq.pop_front();
            let sent = at.min(now_ms as f64 + 0.999);
            self.pkts[i].sent_ms = sent;
            if !self.pkts[i].resend {
                let (seq, of) = (self.pkts[i].seq, self.pkts[i].of);
                self.rtx.sent(seq, b as usize, self.at(sent), of);
            }
            let cap = (self.cap)(now_ms);
            let limit = (cap / 8.0 * self.queue_s).max(3_000.0);
            let backlog = (self.link_free_ms - sent).max(0.0) / 1e3 * cap / 8.0;
            if cap < 1e8 {
                self.qmax_ms = self.qmax_ms.max((self.link_free_ms - sent).max(0.0));
            }
            if backlog + b > limit {
                self.pkts[i].dropped = true;
                continue;
            }
            self.link_free_ms = self.link_free_ms.max(sent) + b * 8.0 / cap * 1e3;
            let arrival = self.link_free_ms + PROP_MS as f64;
            self.pkts[i].arrival_ms = Some(arrival);
            self.arrivals.push_back((arrival, i));
        }
    }

    fn receive(&mut self, now_ms: u64) {
        while self
            .arrivals
            .front()
            .is_some_and(|(t, _)| *t <= now_ms as f64)
        {
            let (t, i) = self.arrivals.pop_front().unwrap();
            let of = self.pkts[i].of;
            if self.got[of].is_none() {
                self.got[of] = Some(t);
                let f = self.pkts[i].frame;
                if self.frames[f].complete_ms.is_none()
                    && self.frames[f].media.iter().all(|&m| self.got[m].is_some())
                {
                    self.frames[f].complete_ms = Some(t);
                }
            }
            if self.highest_got.is_none_or(|h| of > h) {
                let from = self.highest_got.map_or(0, |h| h + 1);
                for m in from..of {
                    if self.got[m].is_none() {
                        self.missing.push(Missing {
                            of: m,
                            next_ms: now_ms,
                            tries: 0,
                        });
                    }
                }
                self.highest_got = Some(of);
            }
        }
        let got = &self.got;
        self.missing
            .retain(|m| got[m.of].is_none() && m.tries < RENACK_TRIES);
        for m in self.missing.iter_mut() {
            if m.next_ms <= now_ms {
                self.nacks_in.push_back((now_ms + PROP_MS, m.of));
                m.tries += 1;
                m.next_ms = now_ms + RENACK_MS;
            }
        }
        // the decoder: in order, a frame once it is whole and everything since the last keyframe
        // was; a keyframe that arrives whole past a gap starts it again
        while let Some(f) = self.frames.get(self.next_decode) {
            if let Some(c) = f.complete_ms {
                let paint = c.max(self.last_paint_ms.unwrap_or(0.0));
                self.paints.push((paint, f.capture_ms));
                self.last_paint_ms = Some(paint);
                self.next_decode += 1;
                continue;
            }
            let later_key = (self.next_decode + 1..self.frames.len())
                .find(|&j| self.frames[j].key && self.frames[j].complete_ms.is_some());
            match later_key {
                Some(j) => {
                    self.next_decode = j;
                    self.missing.clear();
                }
                None => break,
            }
        }
        if now_ms.is_multiple_of(IDLE_PLI_MS) {
            self.plis_in.push_back(now_ms + PROP_MS);
        }
        if now_ms.is_multiple_of(50) {
            self.report(now_ms);
        }
    }

    fn report(&mut self, now_ms: u64) {
        let r = now_ms as f64;
        let mut last = None;
        for (i, p) in self.pkts.iter().enumerate().skip(self.reported_to) {
            // a resend repeats its original's transport-wide sequence: feedback never shows it,
            // and it takes the link's capacity all the same
            if p.resend {
                continue;
            }
            if p.sent_ms == 0.0 && p.arrival_ms.is_none() && !p.dropped {
                break;
            }
            match p.arrival_ms {
                Some(a) if a <= r => last = Some(i),
                Some(_) => break,
                None if !p.dropped => break,
                None => {}
            }
        }
        let Some(j) = last else { return };
        let mut s = PathSample::default();
        for p in self.pkts[self.reported_to..=j].iter().filter(|p| !p.resend) {
            let send_us = (p.sent_ms * 1e3) as i64;
            s.reported += 1;
            if s.known == 0 {
                (s.first_send_us, s.last_send_us) = (send_us, send_us);
            }
            s.known += 1;
            s.sent_bytes += p.bytes as u64;
            s.first_send_us = s.first_send_us.min(send_us);
            s.last_send_us = s.last_send_us.max(send_us);
            match p.arrival_ms {
                None => s.lost += 1,
                Some(a) => {
                    let a = (a * 1e3) as i64 + CLOCK_OFFSET_US;
                    let owd = a - send_us;
                    if s.matched == 0 {
                        (s.first_arrival_us, s.last_arrival_us) = (a, a);
                        s.owd_min_us = owd;
                    }
                    s.matched += 1;
                    s.recv_bytes += p.bytes as u64;
                    s.last_arrival_us = s.last_arrival_us.max(a);
                    s.owd_min_us = s.owd_min_us.min(owd);
                    s.owd_last_us = owd;
                }
            }
        }
        self.reported_to = j + 1;
        self.reports_in.push_back((now_ms + PROP_MS, s));
    }

    fn host(&mut self, now_ms: u64) {
        let now_ns = now_ms * MS;
        let engaged = self.g.link_engaged();
        while self.reports_in.front().is_some_and(|(t, _)| *t <= now_ms) {
            let (_, s) = self.reports_in.pop_front().unwrap();
            if s.lost > 0 {
                self.pli.note_evidence();
                if self.arm == Arm::After && engaged {
                    self.pli.withdraw_evidence();
                }
            }
            self.g.feedback(now_ns, &s);
        }
        while self.nacks_in.front().is_some_and(|(t, _)| *t <= now_ms) {
            let (_, of) = self.nacks_in.pop_front().unwrap();
            self.pli.note_evidence();
            if self.arm == Arm::After && engaged {
                self.pli.withdraw_evidence();
            }
            let orig = self.media_pkt[of];
            let seq = self.pkts[orig].seq;
            let pace = self.paced();
            let target = if pace > 0.0 { pace / PACE_SHARE } else { 0.0 };
            if self.arm == Arm::After && pace > 0.0 && self.key_needed {
                // a keyframe is on its way: nothing before it is worth resending
                continue;
            }
            let at = self.at(now_ms as f64);
            let verdict = if engaged {
                self.rtx
                    .request(seq, at, Duration::from_millis(10), target as u32)
            } else {
                self.rtx.request_unlimited(seq)
            };
            match verdict {
                Ok((&m, bytes)) => {
                    let idx = self.pkts.len();
                    self.pkts.push(Pkt {
                        frame: self.pkts[orig].frame,
                        bytes,
                        seq,
                        sent_ms: 0.0,
                        arrival_ms: None,
                        dropped: false,
                        of: m,
                        resend: true,
                    });
                    self.sendq.push_back(idx);
                }
                Err(Refusal::Stale | Refusal::Budget)
                    if self.arm == Arm::After && pace > 0.0 && !self.key_needed =>
                {
                    self.key_needed = self.ask_keyframe(now_ms, key_origin::UNREPAIRABLE);
                }
                Err(_) => {}
            }
        }
        while self.plis_in.front().is_some_and(|t| *t <= now_ms) {
            self.plis_in.pop_front();
            // the page reports painting: the back-off's cap is the healthy one
            let painting = self
                .last_paint_ms
                .is_some_and(|p| now_ms as f64 - p < 3_000.0);
            let cap = if painting {
                crate::streamer::keyframe::PLI_BACKOFF_CAP_HEALTHY_MS
            } else {
                PLI_BACKOFF_CAP_MS
            };
            if self.pli.allow_with_cap(now_ns, cap) {
                self.ask_keyframe(now_ms, key_origin::PLI);
            }
        }
    }

    fn run(&mut self, secs: u64) {
        self.run_to(self.t + secs * 1_000);
    }

    fn run_to(&mut self, end_ms: u64) {
        let period = 1_000.0 / FPS as f64;
        while self.t < end_ms {
            let t = self.t;
            self.host(t);
            if t as f64 >= self.next_frame as f64 * period {
                let now_ns = t * MS;
                self.g
                    .sender(self.gate.skip_share(now_ns), self.gate.draining(now_ns));
                let tried = self.attempting();
                self.g.tick(now_ns);
                if !tried && self.attempting() {
                    self.attempts.push(t);
                }
                self.frame_tick(t, self.next_frame);
                self.next_frame += 1;
            }
            self.send(t);
            self.receive(t);
            self.t += 1;
            if self.t.is_multiple_of(1_000) {
                self.states
                    .push((self.g.q(), self.g.divisor(), self.g.pace_bps() / 1e6));
                self.queues.push(std::mem::take(&mut self.qmax_ms));
            }
        }
    }

    /// What happened in `[from, to)` ms.
    fn outcome(&self, from: u64, to: u64) -> Outcome {
        let (from_f, to_f) = (from as f64, to as f64);
        let settled: Vec<&Pkt> = self
            .pkts
            .iter()
            .filter(|p| p.sent_ms >= from_f && p.sent_ms < to_f)
            .collect();
        let mut longest = 0.0f64;
        let mut prev = from_f;
        let mut lat = Vec::new();
        let mut painted = 0usize;
        for &(paint, capture) in &self.paints {
            if paint < from_f || paint >= to_f {
                continue;
            }
            longest = longest.max(paint - prev);
            prev = paint;
            painted += 1;
            lat.push(paint - capture);
        }
        longest = longest.max(to_f - prev);
        lat.sort_by(|a, b| a.total_cmp(b));
        let secs = (to - from) as f64 / 1e3;
        Outcome {
            drops_settled: settled.iter().filter(|p| p.dropped).count(),
            packets_settled: settled.len(),
            longest_freeze_ms: longest,
            painted_fps: painted as f64 / secs,
            sent_fps: self
                .frames
                .iter()
                .filter(|f| f.capture_ms >= from_f && f.capture_ms < to_f)
                .count() as f64
                / secs,
            p99_ms: lat
                .get(lat.len().saturating_sub(1) * 99 / 100)
                .copied()
                .unwrap_or(0.0),
            keyframes: self
                .frames
                .iter()
                .filter(|f| f.key && f.capture_ms >= from_f && f.capture_ms < to_f)
                .count(),
        }
    }

    /// One line a second: state, frames sent and painted, keyframes, drops, longest freeze.
    fn seconds(&self) -> String {
        let mut out = String::new();
        for (s, st) in self.states.iter().enumerate() {
            let o = self.outcome(s as u64 * 1_000, (s as u64 + 1) * 1_000);
            out +=
                &format!(
                "  {s:>3} q {:>2} d {} pace {:>5.2} | sent {:>4.0} painted {:>4.0} key {} | drops \
                 {:>4} freeze {:>5.0} p99 {:>5.0} queue {:>4.0}\n",
                st.0, st.1, st.2, o.sent_fps, o.painted_fps, o.keyframes, o.drops_settled,
                o.longest_freeze_ms, o.p99_ms, self.queues.get(s).copied().unwrap_or(0.0)
            );
        }
        out
    }

    /// The median of the frames sent each whole second in `[from, to)` s.
    fn median_sent_fps(&self, from: u64, to: u64) -> f64 {
        let mut v: Vec<f64> = (from..to)
            .map(|s| self.outcome(s * 1_000, (s + 1) * 1_000).sent_fps)
            .collect();
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    }

    fn pace_mbps(&self) -> f64 {
        self.g.pace_bps() / 1e6
    }

    fn quality(&self) -> (u8, u32) {
        (self.g.q(), self.g.divisor())
    }
}

fn capped_between(onset_ms: u64, lift_ms: u64, bps: f64) -> Box<dyn Fn(u64) -> f64> {
    Box::new(move |t| {
        if (onset_ms..lift_ms).contains(&t) {
            bps
        } else {
            1e9
        }
    })
}

fn run(arm: Arm, bps: f64) -> (LinkSim, Outcome) {
    let mut sim = LinkSim::new(arm, capped_between(5_000, 1_000_000, bps));
    sim.run(30);
    let o = sim.outcome(8_000, 30_000);
    eprintln!(
        "{arm:?} {:.1} Mbps
{}",
        bps / 1e6,
        sim.seconds()
    );
    (sim, o)
}

/// The failure without pacing, reproduced: at every cap, before the change the picture freezes
/// for seconds (a lost packet nothing repairs, then a held or lost keyframe), and after it the
/// bottleneck drops next to nothing once the governor has settled, nothing freezes past the
/// time one keyframe takes to drain at the pace, and the frame rate gives way only past QP 40.
#[test]
fn hard_caps_freeze_before_and_keep_moving_after() {
    for bps in [5e6, 3e6, 2e6, 1.5e6] {
        let (_, before) = run(Arm::Before, bps);
        let (sim, after) = run(Arm::After, bps);
        let key_drain_ms = KEY46 * 8.0 / (PACE_SHARE * bps) * 1e3;
        eprintln!(
            "{:.1} Mbps\n  before: {before:?}\n  after:  {after:?}, pace {:.2} Mbps, quality {:?}",
            bps / 1e6,
            sim.pace_mbps(),
            sim.quality()
        );
        assert!(
            before.longest_freeze_ms > 1_000.0,
            "{bps}: the round-one failure did not reproduce: {before:?}"
        );
        assert!(
            after.drops_settled * 1_000 <= after.packets_settled.max(1),
            "{bps}: the bottleneck still drops: {after:?}"
        );
        assert!(
            after.longest_freeze_ms <= 300.0f64.max(key_drain_ms + 200.0),
            "{bps}: froze {} ms: {after:?}",
            after.longest_freeze_ms
        );
        assert!(after.painted_fps > before.painted_fps, "{bps}: {after:?}");
        assert!(
            after.painted_fps >= 0.9 * after.sent_fps - 1.0,
            "{bps}: {after:?}"
        );
        assert_eq!(
            sim.quality().1,
            1,
            "{bps}: the divisor is the pacer's job now"
        );
    }
}

/// A cap the QP range can carry keeps the full frame rate after the change; past it the frame
/// rate gives way only once the QP is at its bound.
#[test]
fn the_frame_rate_gives_way_only_past_the_qp_bound() {
    // a keyframe drains at the pace and the frames behind it wait: the typical second, not the
    // average, is the frame rate the link allows
    let (sim, o) = run(Arm::After, 5e6);
    assert!(
        sim.median_sent_fps(8, 30) >= 56.0,
        "5 Mbps carries QP 40 at the full rate: {o:?}"
    );
    assert!(sim.quality().0 < QP_LINK_MAX, "{:?}", sim.quality());
    let (sim, o) = run(Arm::After, 1.5e6);
    assert_eq!(sim.quality().0, QP_LINK_MAX, "{o:?}");
    let fps = sim.median_sent_fps(8, 30);
    assert!(fps < 55.0 && fps > 8.0, "{fps}: {o:?}");
}

/// The cap lifts: from the worst state back to the user's QP, the full rate and no pacing within
/// about five seconds, and a cap that stays is not probed into a freeze.
#[test]
fn recovery_takes_about_five_seconds_and_a_cap_that_stays_does_not_oscillate() {
    for bps in [1.5e6, 3e6] {
        let mut sim = LinkSim::new(Arm::After, capped_between(5_000, 20_000, bps));
        let mut back = None;
        for s in 0..20u64 {
            sim.run_to(20_000 + (s + 1) * 1_000);
            if back.is_none() && sim.quality() == (15, 1) && sim.pace_mbps() == 0.0 {
                back = Some(s + 1);
            }
        }
        eprintln!(
            "{:.1} Mbps lifted at 20 s: back after {back:?} s
{}",
            bps / 1e6,
            sim.seconds()
        );
        assert!(back.is_some_and(|s| s <= 8), "{bps}: back after {back:?} s");
        // once back, nothing about the episode is left
        let o = sim.outcome(20_000 + back.unwrap_or(20) * 1_000, 40_000);
        assert!(o.longest_freeze_ms < 300.0, "{o:?}");
        assert!(o.sent_fps > 58.0, "{o:?}");
    }
    // the browser's periodic PLI on a steady cap: keyframes hold the link a quarter of the time
    // at most
    let (_, o) = run(Arm::After, 2e6);
    let drain_s = KEY46 * 8.0 / (PACE_SHARE * 2e6);
    let most = (22.0 / (KEY_ROUTINE_DRAINS as f64 * drain_s)).ceil() as usize + 1;
    assert!(o.keyframes <= most, "{most}: {o:?}");
}

/// Four things a hard cap gets wrong: the first estimate read during the loss burst (resends
/// take the link and feedback never shows them), a pace that follows a delivered rate it limits
/// itself, the browser's periodic PLI turned into a keyframe every few seconds while the page
/// paints, and frames skipped while the QP is below its bound. After the change: the pace is
/// near the link within 1.5 s, PLIs from a painting page cost nothing, and below QP 40 next to
/// nothing waits at the sender.
#[test]
fn the_onset_the_periodic_pli_and_skips_below_the_bound() {
    let mut failed = Vec::new();
    for bps in [5e6, 3e6, 2e6, 1.5e6] {
        let mut sim = LinkSim::new(Arm::After, capped_between(5_000, 1_000_000, bps));
        sim.run(30);
        let pace = sim.pace_at(6_500);
        let o = sim.outcome(8_000, 30_000);
        let skips = sim.skips_below_bound(8_000, 30_000);
        let line = format!(
            "{:.1} Mbps: pace {:.2} at onset + 1.5 s, keyframes {} in 8-30 s, skips below the \
             bound {:.1} %, painted {:.1}, longest freeze {:.0} ms",
            bps / 1e6,
            pace / 1e6,
            o.keyframes,
            skips * 100.0,
            o.painted_fps,
            o.longest_freeze_ms
        );
        eprintln!("{line}");
        if (pace - PACE_SHARE * bps).abs() > 0.15 * bps || o.keyframes > 1 || skips >= 0.03 {
            failed.push(line);
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Desktop content whose rate moves by nearly a factor of three from one second to the next.
fn varying() -> Box<dyn Fn(u64) -> f64> {
    const W: [f64; 7] = [0.35, 0.8, 0.5, 1.0, 0.4, 0.9, 0.6];
    Box::new(|t| W[((t / 1_000) % 7) as usize])
}

/// The cap lifts while the content varies and is mostly lighter than a probe would ask: back
/// to the user's QP with nothing paced within about five seconds, and untouched from then on.
#[test]
fn a_cap_that_lifts_under_light_varying_content_is_let_go_within_five_seconds() {
    let mut failed = Vec::new();
    for bps in [5e6, 3e6, 1.5e6] {
        let mut sim = LinkSim::new(Arm::After, capped_between(5_000, 20_000, bps))
            .with_content(varying(), 1.0);
        let mut back = None;
        for s in 0..20u64 {
            sim.run_to(20_000 + (s + 1) * 1_000);
            if back.is_none() && sim.quality() == (15, 1) && sim.pace_mbps() == 0.0 {
                back = Some(s + 1);
            }
        }
        let engagements = sim.g.link.as_ref().map_or(0, |l| l.engagements);
        let touched = back.is_none_or(|b| {
            sim.states[(20 + b) as usize..]
                .iter()
                .any(|st| (st.0, st.1, st.2) != (15, 1, 0.0))
        });
        let line = format!(
            "{:.1} Mbps lifted at 20 s: back after {back:?} s, {engagements} engagements, touched \
             after {touched}",
            bps / 1e6
        );
        eprintln!("{line}\n{}", sim.seconds());
        if back.is_none_or(|b| b > 6) || engagements != 1 || touched {
            failed.push(line);
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// A cap that stays, under content whose rate varies threefold from second to second: the QP
/// sits where the heavy seconds fit, it is not lowered on a light one, and next to no frame
/// waits at the sender outside a keyframe's drain.
#[test]
fn a_steady_cap_under_content_varying_threefold_holds_the_qp_and_keeps_every_frame() {
    let mut failed = Vec::new();
    for bps in [5e6, 3e6] {
        let mut sim = LinkSim::new(Arm::After, capped_between(5_000, 1_000_000, bps))
            .with_content(varying(), 1.0);
        sim.run_to(12_000);
        let raises = sim.g.link.as_ref().map_or(0, |l| l.raises);
        sim.run_to(45_000);
        let raises = sim.g.link.as_ref().map_or(0, |l| l.raises) - raises;
        let skips = sim.skips_below_bound(12_000, 45_000);
        let fps = sim.median_sent_fps(12, 45);
        let line = format!(
            "{:.1} Mbps: skips outside drains {:.1} %, median sent fps {fps:.0}, raises {raises} \
             in 12-45 s, quality {:?}",
            bps / 1e6,
            skips * 100.0,
            sim.quality()
        );
        eprintln!("{line}\n{}", sim.seconds());
        if skips >= 0.02 || fps < 55.0 || raises > 6 {
            failed.push(line);
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Attempts on a cap that never lifts, behind a 100 ms bottleneck buffer: each costs a short,
/// shallow queue and never a packet, and they come further apart.
#[test]
fn attempts_on_a_cap_that_stays_cost_a_little_queue_and_never_a_packet() {
    let mut failed = Vec::new();
    for bps in [5e6, 3e6] {
        let mut sim = LinkSim::new(Arm::After, capped_between(5_000, 1_000_000, bps))
            .with_content(varying(), 1.0);
        sim.run(70);
        let o = sim.outcome(10_000, 70_000);
        let deepest = sim.queues[10..].iter().copied().fold(0.0, f64::max);
        let late: Vec<u64> = sim
            .attempts
            .iter()
            .copied()
            .filter(|t| *t >= 30_000)
            .collect();
        let (n, k) = sim
            .ticks
            .iter()
            .filter(|t| t.0 >= 10_000)
            .fold((0u32, 0u32), |(n, k), t| (n + 1, k + t.1 as u32));
        let line = format!(
            "{:.1} Mbps: drops {} of {}, deepest queue {deepest:.0} ms, {:.1} % of frames waited,              attempts at {:?}",
            bps / 1e6,
            o.drops_settled,
            o.packets_settled,
            k as f64 * 100.0 / n.max(1) as f64,
            sim.attempts
        );
        eprintln!("{line}\n{}", sim.seconds());
        if o.drops_settled > 0 || deepest > 60.0 || late.len() > 12 {
            failed.push(line);
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Quick Sync and x264 spend about twice NVENC's bits at a QP: at 2 and 1.5 Mbit/s the bound
/// alone left them under half of NVENC's frames (21 and 18 a second against 53 and 41); past
/// the bound once half the frames wait for long, they keep at least half.
#[test]
fn an_encoder_spending_twice_the_bits_at_low_caps() {
    let mut failed = Vec::new();
    for bps in [2e6, 1.5e6] {
        let mut fps = [0.0; 2];
        for (i, gain) in [1.0, 2.0].into_iter().enumerate() {
            let mut sim = LinkSim::new(Arm::After, capped_between(5_000, 1_000_000, bps))
                .with_content(Box::new(|_| 1.0), gain);
            sim.run(40);
            fps[i] = sim.outcome(12_000, 40_000).painted_fps;
            eprintln!(
                "{:.1} Mbps gain {gain}: quality {:?}\n{}",
                bps / 1e6,
                sim.quality(),
                sim.seconds()
            );
        }
        let line = format!(
            "{:.1} Mbps: painted {:.1} at NVENC's bits, {:.1} at twice",
            bps / 1e6,
            fps[0],
            fps[1]
        );
        eprintln!("{line}");
        if fps[1] < 0.5 * fps[0] {
            failed.push(line);
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

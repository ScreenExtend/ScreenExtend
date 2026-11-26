//! `pipe` on Windows: the production encode loop fed by the feeder, with no display, capture or
//! socket. Each session plays the capture handler: NVENC through `AsyncEncodeRing::stage` and its
//! encode thread, Quick Sync and x264 through `EncodeCore` and the idle repeater. Every emitted
//! frame goes through the production packetizer and `SessionTelemetry::record_frame`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{json, Value as Json};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use super::super::clip;
use super::super::metrics::summary;
use super::super::plan::{ProductionCfg, RunSpec, Workload};
use super::super::series::Sampler;
use super::feed::{self, FeedSession, FeedStats, Input, Pacer, Schedule};
use super::lab::{self, Enc, LabVendor, Session};
use super::recipe::{MixEntry, Val};
use super::sys;
use crate::streamer::packetizer::{access_unit_has_sps, H264Packetizer};
use crate::streamer::repair::RepairState;
use crate::streamer::telemetry::{FrameTiming, SessionTelemetry, SharedTelemetry};
use crate::streamer::webrtc_session::{next_video_rtp_ts, RTP_PAYLOAD_MTU_LAN};
use crate::windows_utils::streamer::nvidia::clock_hold;
use crate::windows_utils::streamer::pipeline::{
    hold_trailing_frame, is_transient_encode_error, spawn_repeater, Admit, AsyncEncodeRing,
    CoreHealth, EncodeCore, EncodedFrame, FrameGate, RingEncoder, RingTextures, TrailingEdge,
    BROADCAST_CAPACITY, CADENCE_DEN, CADENCE_NUM, MAX_TRANSIENT_ENCODE_DROPS,
};
use crate::windows_utils::streamer::{tune, tuning};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

const LATE_NS: u64 = 2_000_000;
const LATE_SHARE: f64 = 0.01;

pub const HELP: &str = "\
PIPE (Windows): the production encode loop fed at the recorded deadlines, then the production
  packetizer and session telemetry; no display, capture or socket
  NVENC: AsyncEncodeRing (stage, encode thread, keepalive); Quick Sync / x264: EncodeCore plus
  the idle repeater; AMF: skipped without an AMD adapter
  recipe fields:  as lab (vendor, production, set), plus
                  mix  [{label, vendor, qp, bitrate_bps, set, transform, fps, width, height,
                        offset_s}, ...]  one session each, all in one process
                  env  [[\"SE_...\", value], ...]  set in the child before anything starts
  group sessions N without a mix: N copies of the recipe, started 7 s apart in the source
  events:  force_idr | repair (falls back to an IDR when the encoder has no LTR) |
           set {key: bitrate_bps} | idle {value: seconds} | burst {value: seconds, 2x cadence}
                  gates  true: offer a take at its own callback cadence through the capture
                         handler's cadence and dirty-region gates and cursor exemption, deciding
                         each frame from its recorded flags; the workload's fps:N is the session
                         rate. auto_frame_rate  true arms the dirty-region gate
  several sessions on an uncompressed take read a compressed copy of the frames they need,
  made once in <out>/cache/ (reported as take_cache)
  builtin plans:  win-pipe-quick, win-pipe-sessions, win-pipe-gates
";

/// One session's description, before anything is built.
struct Want {
    label: String,
    vendor: LabVendor,
    production: ProductionCfg,
    set: Vec<(String, Val)>,
    workload: Workload,
    offset_s: f64,
}

fn wants(spec: &RunSpec) -> Result<Vec<Want>, String> {
    let r = &spec.recipe.recipe;
    let prod = spec.recipe.production.clone().unwrap_or_default();
    let parse_vendor = |v: Option<&str>| -> Result<LabVendor, String> {
        let v = v.ok_or("the recipe names no Windows vendor")?;
        LabVendor::parse(v).ok_or_else(|| format!("unknown vendor '{v}'"))
    };
    if let Some(mix) = &r.mix {
        return mix
            .iter()
            .enumerate()
            .map(|(k, m): (usize, &MixEntry)| {
                let mut w = spec.workload.clone();
                if let Some(t) = &m.transform {
                    w.transform = Some(t.clone());
                }
                if let Some(f) = m.fps {
                    w.fps = f;
                }
                if let Some(x) = m.width {
                    w.width = x;
                }
                if let Some(y) = m.height {
                    w.height = y;
                }
                let mut p = prod.clone();
                if m.qp.is_some() || m.bitrate_bps.is_some() {
                    p.qp = m.qp;
                    p.bitrate_bps = m.bitrate_bps;
                }
                Ok(Want {
                    label: m.label.clone().unwrap_or_else(|| format!("s{k}")),
                    vendor: parse_vendor(Some(&m.vendor))?,
                    production: p,
                    set: m.set.clone(),
                    workload: w,
                    offset_s: m.offset_s.unwrap_or(k as f64 * 7.0),
                })
            })
            .collect();
    }
    let v = parse_vendor(r.vendor.as_deref())?;
    Ok((0..spec.sessions.max(1) as usize)
        .map(|k| Want {
            label: format!("s{k}"),
            vendor: v,
            production: prod.clone(),
            set: r.set.clone(),
            workload: spec.workload.clone(),
            offset_s: k as f64 * 7.0,
        })
        .collect())
}

/// The production path a session drives.
enum Path {
    Ring(Box<AsyncEncodeRing>),
    Inline {
        core: Arc<Mutex<EncodeCore>>,
        stop: Arc<AtomicBool>,
    },
}

/// Everything one session owns while it runs.
struct Live {
    label: String,
    vendor: LabVendor,
    path: Path,
    session_meta: Json,
    telemetry: SharedTelemetry,
    tx: broadcast::Sender<EncodedFrame>,
    idr: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    wake: crossbeam_channel::Sender<()>,
    repair: Arc<RepairState>,
    last_frame_at: Arc<AtomicU64>,
    epoch: Instant,
    sched: Arc<Schedule>,
    fps: u32,
    width: u32,
    height: u32,
    gate: Option<SessionGate>,
}

// SAFETY: a session's COM objects are used only by its offer thread (and the production threads
// the production constructors started); the main thread only moves it there and back.
unsafe impl Send for Live {}

/// The capture handler's gates for one session, fed the take's recorded flags.
struct SessionGate {
    gate: FrameGate,
    cursor: Vec<bool>,
    dirty: Vec<Option<bool>>,
    trailing: Option<TrailingEdge>,
    epoch: Instant,
    gated_cadence: u64,
    gated_dirty: u64,
    cursor_exempted: u64,
    held: u64,
}

impl SessionGate {
    fn new(
        sched: &Schedule,
        fps: u32,
        flags: Option<&[u32]>,
        auto_frame_rate: bool,
        path: &Path,
        idr: &Arc<AtomicBool>,
        epoch: Instant,
    ) -> SessionGate {
        let (cursor, dirty) = recorded_flags(sched, flags);
        let ring = matches!(path, Path::Ring(_));
        let trailing = match path {
            Path::Ring(r) if tune::on("wgc_trailing_frame") => {
                Some(TrailingEdge::start(r, Arc::clone(idr), epoch))
            }
            _ => None,
        };
        SessionGate {
            gate: FrameGate::new(
                Duration::from_nanos(1_000_000_000 / fps.max(1) as u64),
                auto_frame_rate,
                flags.is_some(),
                ring && tune::on("wgc_cadence_counts_staged_only"),
            ),
            cursor,
            dirty,
            trailing,
            epoch,
            gated_cadence: 0,
            gated_dirty: 0,
            cursor_exempted: 0,
            held: 0,
        }
    }

    /// `true`: offer the frame. A held frame is staged into the ring here, for the timer.
    fn admit(
        &mut self,
        g: usize,
        capture: Instant,
        ring: Option<&mut AsyncEncodeRing>,
        tex: &ID3D11Texture2D,
        timing: FrameTiming,
        telemetry: &SessionTelemetry,
    ) -> Result<bool, String> {
        let cursor = self.cursor[g];
        let dirty = self.dirty[g];
        let timer = self
            .trailing
            .as_ref()
            .and_then(|t| t.delivered_at(self.epoch));
        let last = match (self.gate.last_delivered, timer) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if cursor
            && last.is_some_and(|l| {
                capture.duration_since(l) < self.gate.frame_period * CADENCE_NUM / CADENCE_DEN
            })
        {
            self.cursor_exempted += 1;
        }
        let ring_on = ring.is_some();
        match self.gate.admit(
            capture,
            cursor,
            timer,
            self.trailing.is_some(),
            ring_on,
            || dirty,
            telemetry,
        ) {
            Admit::Stage => {
                if let Some(t) = self.trailing.as_ref() {
                    t.discard_pending(telemetry);
                }
                Ok(true)
            }
            Admit::Hold { last } => {
                self.held += 1;
                hold_trailing_frame(
                    ring,
                    self.trailing.as_mut(),
                    &self.gate,
                    || dirty,
                    tex,
                    timing,
                    capture,
                    last,
                    telemetry,
                )
                .map_err(|e| format!("{e:#}"))?;
                Ok(false)
            }
            Admit::SkipCadence => {
                self.gated_cadence += 1;
                Ok(false)
            }
            Admit::SkipUnchanged => {
                self.gated_dirty += 1;
                Ok(false)
            }
        }
    }

    fn staged(&mut self, capture: Instant, staged: bool) {
        self.gate.staged(capture, staged);
    }

    fn to_json(&self) -> Json {
        json!({
            "gated_cadence": self.gated_cadence,
            "gated_dirty": self.gated_dirty,
            "cursor_exempted": self.cursor_exempted,
            "held": self.held,
            "trailing": self.trailing.is_some(),
            "trailing_delivered": self.trailing.as_ref().map_or(0, |t| t.delivered_count()),
            "auto_frame_rate": self.gate.auto_frame_rate,
            "dirty_known": self.gate.dirty_region_reporting,
            "cadence_counts_staged_only": self.gate.cadence_counts_staged_only,
        })
    }
}

/// Each scheduled frame's recorded cursor and dirty-region answers (none for a synthetic source).
fn recorded_flags(sched: &Schedule, flags: Option<&[u32]>) -> (Vec<bool>, Vec<Option<bool>>) {
    use super::clipfmt::{FLAG_CURSOR_MOVED, FLAG_DIRTY_EMPTY, FLAG_DIRTY_KNOWN};
    (0..sched.total())
        .map(|g| {
            let f = flags
                .and_then(|f| f.get(sched.src[g]).copied())
                .unwrap_or(0);
            let dirty = (f & FLAG_DIRTY_KNOWN != 0).then_some(f & FLAG_DIRTY_EMPTY == 0);
            (f & FLAG_CURSOR_MOVED != 0, dirty)
        })
        .unzip()
}

/// The workload without its `fps:N`, which with gates on is the session rate, not a decimation.
fn without_decimation(w: &Workload) -> Workload {
    let mut w = w.clone();
    w.transform = w.transform.as_deref().map(|t| {
        t.split(',')
            .map(str::trim)
            .filter(|p| !p.starts_with("fps:"))
            .collect::<Vec<_>>()
            .join(",")
    });
    w
}

/// `Pipeline::request_idr`.
fn request_idr(l: &Live) {
    l.idr.store(true, Ordering::Relaxed);
    let _ = l.wake.try_send(());
}

/// Build the production pipeline for a session around a lab-opened encoder.
fn build(
    w: &Want,
    s: Session,
    sched: Arc<Schedule>,
    fps: u32,
    bitrate: u32,
) -> Result<Live, String> {
    let telemetry = SessionTelemetry::new_shared();
    telemetry.capture_backend.set("bench-pipe".into());
    let (tx, _) = broadcast::channel::<EncodedFrame>(BROADCAST_CAPACITY);
    let idr = Arc::new(AtomicBool::new(false));
    let target_bitrate = Arc::new(AtomicU32::new(0));
    let (wake, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let repair = Arc::new(RepairState::new());
    let last_frame_at = Arc::new(AtomicU64::new(0));
    let epoch = Instant::now();
    let frame_duration = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
    let meta = json!({
        "backend": s.backend,
        "identity": s.identity,
        "session_create_ms": s.create_ms,
        "recipe_report": s.hooks.as_ref().map(|d| d.to_json()),
    });
    let Session { enc, ctx, .. } = s;
    let path = match enc {
        Enc::NvRing {
            enc, pairs, _hold, ..
        } => {
            tuning::raise_d3d11_gpu_priority(enc.device());
            let (textures, mutexes): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
            telemetry.encoder.set("nvenc-zero-copy-ring".into());
            let ring = AsyncEncodeRing::new(
                RingEncoder::Nvenc(*enc),
                RingTextures::Shared { textures, mutexes },
                ctx,
                None,
                None,
                tx.clone(),
                Arc::clone(&target_bitrate),
                bitrate,
                Arc::clone(&last_frame_at),
                Arc::clone(&idr),
                Arc::clone(&repair),
                wake_rx,
                frame_duration,
                epoch,
                Arc::clone(&telemetry),
            );
            // the ring took its own clock hold; the lab session's goes with it
            drop(_hold);
            Path::Ring(Box::new(ring))
        }
        Enc::AmfRing { enc, textures, .. } => {
            telemetry.encoder.set("amd-amf-ring".into());
            Path::Ring(Box::new(AsyncEncodeRing::new(
                RingEncoder::Amf(*enc),
                RingTextures::Local { textures },
                ctx,
                None,
                None,
                tx.clone(),
                Arc::clone(&target_bitrate),
                bitrate,
                Arc::clone(&last_frame_at),
                Arc::clone(&idr),
                Arc::clone(&repair),
                wake_rx,
                frame_duration,
                epoch,
                Arc::clone(&telemetry),
            )))
        }
        Enc::Inline {
            backend,
            scaler,
            reader,
        } => {
            telemetry.encoder.set(backend.name().into());
            let core = Arc::new(Mutex::new(EncodeCore {
                backend: *backend,
                scaler,
                reader,
                staging: None,
                target_bitrate: Arc::clone(&target_bitrate),
                idr_request: Arc::clone(&idr),
                current_bitrate: bitrate,
                bitrate_warned: false,
                have_frame: false,
                frame_index: 0,
                health: CoreHealth::new(Arc::clone(&telemetry)),
            }));
            let stop = Arc::new(AtomicBool::new(false));
            spawn_repeater(
                Arc::clone(&core),
                tx.clone(),
                Arc::clone(&idr),
                frame_duration,
                Arc::clone(&last_frame_at),
                epoch,
                Arc::clone(&stop),
                wake_rx,
                Arc::clone(&telemetry),
            );
            Path::Inline { core, stop }
        }
        Enc::None => return Err("the pipe stage needs an encoder (vendor none is lab-only)".into()),
    };
    Ok(Live {
        label: w.label.clone(),
        vendor: w.vendor,
        path,
        session_meta: meta,
        telemetry,
        tx,
        idr,
        target_bitrate,
        wake,
        repair,
        last_frame_at,
        epoch,
        width: sched.width as u32,
        height: sched.height as u32,
        sched,
        fps,
        gate: None,
    })
}

/// What the packetizer saw for one broadcast frame.
struct OutRec {
    timing: FrameTiming,
    capture_ns: u64,
    rtp_ts: u32,
    clamped: bool,
    au_bytes: usize,
    packets: usize,
    keyframe: bool,
}

/// The session's parameter-set accounting and the packetizer's own counters.
#[derive(Clone, Copy, Debug, Default)]
struct PacketizerSets {
    sets: ParamSets,
    /// Sets the MTU could not carry even fragmented (the packetizer's count).
    dropped_mtu: u64,
    /// Sets larger than the MTU, sent as FU-A (the packetizer's count).
    fragmented: u64,
}

/// What the packetizer did with parameter sets, summed over a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ParamSets {
    /// SPS and PPS units in the encoder's access units.
    in_access_units: u64,
    /// Of those, units a later unit of the same type and id in the same access unit replaced:
    /// only the last copy is sent.
    superseded: u64,
    /// SPS and PPS units on the wire: bare, inside a STAP-A, or as the first FU-A fragment.
    sent: u64,
    /// Sent units the access unit did not carry: stored sets put in front of a keyframe that
    /// lacked them.
    injected: u64,
    /// Units the access unit carried, not superseded, that never reached the wire.
    unaccounted: u64,
}

impl ParamSets {
    /// Fold one access unit and the payloads the packetizer made of it.
    fn add(&mut self, au: &[u8], payloads: &[Bytes], nals: &mut Vec<(usize, usize)>) {
        use super::avc::parameter_set_key;
        let is_ps = |t: u8| t == 7 || t == 8;
        super::avc::nal_ranges(au, nals);
        let keys: Vec<Option<(u8, u32)>> = nals
            .iter()
            .filter(|&&(s, _)| is_ps(au[s] & 0x1f))
            .map(|&(s, e)| parameter_set_key(&au[s..e]))
            .collect();
        let superseded = keys
            .iter()
            .enumerate()
            .filter(|(i, k)| k.is_some() && keys[i + 1..].contains(k))
            .count() as u64;
        let mut sent = 0u64;
        for p in payloads {
            let Some(&h) = p.first() else { continue };
            match h & 0x1f {
                t if is_ps(t) => sent += 1,
                24 => {
                    let mut i = 1;
                    while i + 2 <= p.len() {
                        let n = u16::from_be_bytes([p[i], p[i + 1]]) as usize;
                        if n > 0 && i + 2 < p.len() && is_ps(p[i + 2] & 0x1f) {
                            sent += 1;
                        }
                        i += 2 + n;
                    }
                }
                28 if p.len() > 1 && p[1] & 0x80 != 0 && is_ps(p[1] & 0x1f) => sent += 1,
                _ => {}
            }
        }
        let kept = keys.len() as u64 - superseded;
        self.in_access_units += keys.len() as u64;
        self.superseded += superseded;
        self.sent += sent;
        self.injected += sent.saturating_sub(kept);
        self.unaccounted += kept.saturating_sub(sent);
    }
}

/// The transport's half: subscribe, packetize at the LAN MTU, stamp, fold into the telemetry.
fn spawn_packetizer(
    l: &Live,
    expect: usize,
) -> std::thread::JoinHandle<(Vec<OutRec>, u64, PacketizerSets)> {
    let mut rx = l.tx.subscribe();
    let telemetry = Arc::clone(&l.telemetry);
    std::thread::Builder::new()
        .name("bench-packetize".into())
        .spawn(move || {
            crate::streamer::platform::tune_transport_thread();
            let mut pk = H264Packetizer::new();
            let mut payloads: Vec<Bytes> = Vec::with_capacity(256);
            let mut recs: Vec<OutRec> = Vec::with_capacity(expect + 256);
            let mut lagged = 0u64;
            let mut sets = ParamSets::default();
            let mut nals: Vec<(usize, usize)> = Vec::with_capacity(16);
            let mut last_rtp: Option<u32> = None;
            loop {
                match rx.blocking_recv() {
                    Ok(frame) => {
                        let mut timing = frame.timing;
                        pk.packetize(RTP_PAYLOAD_MTU_LAN, &frame.data, &mut payloads);
                        timing.mark_packetize_done();
                        let capture_ns = crate::streamer::audio::host_instant_to_ns(frame.capture);
                        let (rtp_ts, clamped) = next_video_rtp_ts(
                            last_rtp,
                            crate::streamer::audio::host_ns_to_rtp90k(capture_ns),
                        );
                        last_rtp = Some(rtp_ts);
                        if clamped {
                            telemetry.rtp_ts_clamped.fetch_add(1, Ordering::Relaxed);
                        }
                        let packets = payloads.len();
                        sets.add(&frame.data, &payloads, &mut nals);
                        payloads.clear();
                        telemetry
                            .bytes_sent
                            .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                        telemetry.record_frame(&timing);
                        recs.push(OutRec {
                            timing,
                            capture_ns,
                            rtp_ts,
                            clamped,
                            au_bytes: frame.data.len(),
                            packets,
                            keyframe: access_unit_has_sps(&frame.data),
                        });
                    }
                    Err(RecvError::Lagged(n)) => {
                        lagged += n;
                        telemetry
                            .degradation
                            .frames_dropped_broadcast
                            .fetch_add(n, Ordering::Relaxed);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            let counted = PacketizerSets {
                sets,
                dropped_mtu: pk.dropped_parameter_sets(),
                fragmented: pk.fragmented_parameter_sets(),
            };
            (recs, lagged, counted)
        })
        .expect("spawn packetizer")
}

/// Burst windows (deadlines compressed 2x) and idle windows (frames withheld), from the events.
struct Shape {
    idle: Vec<(u64, u64)>,
    bursts: Vec<(u64, u64)>,
}

fn shape_of(spec: &RunSpec) -> Shape {
    let mut s = Shape {
        idle: Vec::new(),
        bursts: Vec::new(),
    };
    for e in &spec.events {
        let (Some(at), Some(Val::Int(_) | Val::Float(_))) = (e.at_s, e.value.as_ref()) else {
            continue;
        };
        let secs = match e.value {
            Some(Val::Int(i)) => i as f64,
            Some(Val::Float(f)) => f,
            _ => 0.0,
        };
        let w = ((at * 1e9) as u64, ((at + secs) * 1e9) as u64);
        match e.action.as_str() {
            "idle" => s.idle.push(w),
            "burst" => s.bursts.push(w),
            _ => {}
        }
    }
    s
}

/// The source played at 2x cadence for `[T, T + S)` of run time: what was due over `[T, T + 2S)`
/// arrives in `S`, and everything after moves `S` earlier.
fn apply_bursts(at: &mut [u64], bursts: &[(u64, u64)]) {
    for &(t, end) in bursts {
        let s = end - t;
        for x in at.iter_mut() {
            if *x >= t + 2 * s {
                *x -= s;
            } else if *x >= t {
                *x = t + (*x - t) / 2;
            }
        }
    }
}

/// One session's offer-side record.
struct Offers {
    late_ns: Vec<u64>,
    late_by_encoder: usize,
    offer_ns: Vec<u64>,
    staged: Vec<bool>,
    delivered: usize,
    /// Frames the gates did not offer (cadence, unchanged or held).
    gated: usize,
    /// Per frame: 0 not reached, 1 offered, 2 gated, 3 withheld (idle).
    status: Vec<u8>,
    withheld_idle: usize,
    staged_n: usize,
    dropped_no_slot: usize,
    stage_errors: usize,
    busy: usize,
    errors: BTreeMap<String, usize>,
    underruns: usize,
    reader_gap: bool,
    stopped: Option<String>,
    feed: FeedStats,
    events: Vec<Json>,
    pacer: &'static str,
}

/// The capture handler's part: at each deadline hand the frame to the production path.
fn offer(l: &mut Live, fs: FeedSession, spec: &RunSpec, t_start: Instant, shape: &Shape) -> Offers {
    let _tuning = tuning::tune_current_thread();
    let pacer = Pacer::new();
    let total = l.sched.total();
    let mut o = Offers {
        late_ns: Vec::with_capacity(total),
        late_by_encoder: 0,
        offer_ns: vec![0; total],
        staged: vec![false; total],
        delivered: 0,
        gated: 0,
        status: vec![0; total],
        withheld_idle: 0,
        staged_n: 0,
        dropped_no_slot: 0,
        stage_errors: 0,
        busy: 0,
        errors: BTreeMap::new(),
        underruns: 0,
        reader_gap: false,
        stopped: None,
        feed: FeedStats::with_capacity(total),
        events: Vec::new(),
        pacer: pacer.kind(),
    };
    let fire = lab::event_schedule(&spec.events, &l.sched.at);
    let mut prev_end: Option<Instant> = None;
    let mut busy_streak = 0u32;
    for g in 0..total {
        let r = match fs.try_recv() {
            Some(r) => r,
            None => {
                o.underruns += 1;
                match fs.recv() {
                    Some(r) => r,
                    None => {
                        o.reader_gap = true;
                        break;
                    }
                }
            }
        };
        let due = l.sched.at[g];
        let deadline = t_start + Duration::from_nanos(due);
        let tex = match fs.acquire(&r) {
            Ok(t) => t.clone(),
            Err(e) => {
                o.stopped = Some(format!("frame {g}: {e}"));
                break;
            }
        };
        let miss = o.feed.note(g, deadline, &r.times);
        let late = pacer.wait_until(deadline);
        let feeder_late = matches!(miss, Some(c) if c != "consumer");
        if late > LATE_NS && !feeder_late && prev_end.is_some_and(|e| e > deadline) {
            o.late_by_encoder += 1;
        }
        o.late_ns.push(late);
        o.offer_ns[g] = Instant::now().saturating_duration_since(t_start).as_nanos() as u64;
        if let Some(evs) = fire.get(&g) {
            for &ei in evs {
                let ev = &spec.events[ei];
                let t_s = due as f64 / 1e9;
                let note = match (ev.action.as_str(), ev.key.as_deref()) {
                    ("force_idr", _) => {
                        request_idr(l);
                        json!("requested")
                    }
                    ("repair", _) => {
                        if l.repair.request_repair() {
                            let _ = l.wake.try_send(());
                            json!("repair requested")
                        } else {
                            request_idr(l);
                            json!("no reference to repair from (0 LTR slots): IDR requested, as the transport does")
                        }
                    }
                    ("set", Some("bitrate_bps")) => match &ev.value {
                        Some(Val::Int(b)) if *b > 0 => {
                            l.target_bitrate.store(*b as u32, Ordering::Relaxed);
                            let _ = l.wake.try_send(());
                            json!(format!("target {b} bps"))
                        }
                        _ => json!("bitrate_bps needs a positive integer"),
                    },
                    ("set", Some("qp")) => json!(
                        "not applicable: production fixes the QP when the session opens; a quality \
                         change is a new session"
                    ),
                    ("idle" | "burst", _) => json!("applied to the schedule"),
                    (a, _) => json!(format!("unknown action '{a}'")),
                };
                o.events.push(json!({ "session": l.label, "frame": g, "t_s": t_s, "action": ev.action, "key": ev.key, "result": note }));
            }
        }
        if shape.idle.iter().any(|&(a, b)| due >= a && due < b) {
            o.withheld_idle += 1;
            o.status[g] = 3;
            fs.release(r);
            continue;
        }
        let timing = FrameTiming::entering_capture();
        let capture = Instant::now();
        if let Some(sg) = l.gate.as_mut() {
            let ring = match &mut l.path {
                Path::Ring(ring) => Some(&mut **ring),
                Path::Inline { .. } => None,
            };
            match sg.admit(g, capture, ring, &tex, timing, &l.telemetry) {
                Ok(true) => {}
                Ok(false) => {
                    o.gated += 1;
                    o.status[g] = 2;
                    fs.release(r);
                    continue;
                }
                Err(e) => {
                    o.stopped = Some(format!("frame {g}: held frame: {e}"));
                    fs.release(r);
                    break;
                }
            }
        }
        o.delivered += 1;
        o.status[g] = 1;
        match &mut l.path {
            Path::Ring(ring) => match ring.stage(&tex, timing) {
                Ok(staged) => {
                    if let Some(sg) = l.gate.as_mut() {
                        sg.staged(capture, staged);
                    }
                    if staged {
                        o.staged_n += 1;
                        o.staged[g] = true;
                    } else {
                        o.dropped_no_slot += 1;
                    }
                }
                Err(e) => {
                    o.stage_errors += 1;
                    *o.errors
                        .entry(format!("{e:#}").chars().take(160).collect())
                        .or_default() += 1;
                }
            },
            Path::Inline { core, .. } => {
                // `on_frame_arrived`'s inline branch, with `encode_texture` for `encode_captured`
                // (the same dispatch without a WGC frame)
                let mut timing = timing;
                let res = {
                    let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
                    timing.mark_encode_submit();
                    c.encode_texture(&tex)
                };
                timing.mark_encode_done();
                match res {
                    Ok(au) => {
                        busy_streak = 0;
                        o.staged_n += 1;
                        o.staged[g] = true;
                        if !au.is_empty() {
                            l.last_frame_at
                                .store(l.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                            l.telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
                            let _ = l.tx.send(EncodedFrame {
                                data: Bytes::from(au),
                                capture,
                                timing,
                            });
                        }
                    }
                    Err(e)
                        if is_transient_encode_error(&e)
                            && busy_streak < MAX_TRANSIENT_ENCODE_DROPS =>
                    {
                        busy_streak += 1;
                        o.busy += 1;
                        l.telemetry
                            .degradation
                            .encoder_transient_errors
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        *o.errors
                            .entry(format!("{e:#}").chars().take(160).collect())
                            .or_default() += 1;
                        o.stopped = Some(format!(
                            "frame {g}: encode failed, the capture handler stops: {e:#}"
                        ));
                        fs.release(r);
                        break;
                    }
                }
            }
        }
        prev_end = Some(Instant::now());
        fs.release(r);
    }
    o
}

/// Samples host and GPU load once a second until `stop` is set.
fn load_sampler(stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Vec<Json>> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};
    fn ft(f: FILETIME) -> u64 {
        ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
    }
    let read = || -> Option<(u64, u64, u64)> {
        let (mut idle, mut kern, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let (mut c, mut e, mut pk, mut pu) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        unsafe {
            GetSystemTimes(Some(&mut idle), Some(&mut kern), Some(&mut user)).ok()?;
            GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut pk, &mut pu).ok()?;
        }
        // system kernel time includes idle
        Some((ft(kern) + ft(user), ft(idle), ft(pk) + ft(pu)))
    };
    std::thread::Builder::new()
        .name("bench-load".into())
        .spawn(move || {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
            let t0 = Instant::now();
            let mut out = Vec::new();
            let mut prev = read();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let now = read();
                if let (Some(a), Some(b)) = (prev, now) {
                    let total = (b.0 - a.0) as f64;
                    let busy = total - (b.1 - a.1) as f64;
                    out.push(json!({
                        "t_s": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
                        "system_cpu_pct": if total > 0.0 { (busy / total * 1000.0).round() / 10.0 } else { 0.0 },
                        "process_cpu_pct_of_machine": if total > 0.0 { ((b.2 - a.2) as f64 / (total / cores) / cores * 1000.0).round() / 10.0 } else { 0.0 },
                        "nvidia": sys::nvidia_smi(),
                        "clock_hold": clock_hold::wanted().0,
                    }));
                }
                prev = now;
            }
            out
        })
        .expect("spawn load sampler")
}

fn span(a: u64, b: u64) -> Option<f64> {
    (a != 0 && b != 0 && b >= a).then(|| (b - a) as f64 / 1e6)
}

/// What a session's result needs once its pipeline is torn down.
struct View {
    label: String,
    vendor: LabVendor,
    session_meta: Json,
    telemetry: SharedTelemetry,
    sched: Arc<Schedule>,
    fps: u32,
    width: u32,
    height: u32,
}

/// Per-frame rows for one session: every scheduled frame to `<id>.offers.csv` and every
/// broadcast access unit to `<id>.frames.csv`, appended, with a header when `first`.
fn write_per_frame(
    frames: &std::path::Path,
    label: &str,
    sched: &Schedule,
    o: &Offers,
    recs: &[OutRec],
    t_start: Instant,
    first: bool,
) -> std::io::Result<()> {
    use std::io::Write;
    let open = |p: &std::path::Path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map(std::io::BufWriter::new)
    };
    let ms = |a: u64, b: u64| span(a, b).map(|v| format!("{v:.3}")).unwrap_or_default();
    let t0 = crate::streamer::audio::host_instant_to_ns(t_start);
    let mut f = open(frames)?;
    if first {
        writeln!(f, "session,au,repeated,keyframe,capture_ms,offer_to_submit_ms,submit_to_encoded_ms,encoded_to_packetized_ms,offer_to_packetized_ms,bytes,packets,rtp_ts")?;
    }
    for (i, r) in recs.iter().enumerate() {
        let t = &r.timing;
        writeln!(
            f,
            "{label},{i},{},{},{:.3},{},{},{},{},{},{},{}",
            t.repeated as u8,
            r.keyframe as u8,
            r.capture_ns.saturating_sub(t0) as f64 / 1e6,
            ms(t.capture_cb_entry_ns, t.encode_submit_ns),
            ms(t.encode_submit_ns, t.encode_done_ns),
            ms(t.encode_done_ns, t.packetize_done_ns),
            ms(t.capture_cb_entry_ns, t.packetize_done_ns),
            r.au_bytes,
            r.packets,
            r.rtp_ts,
        )?;
    }
    f.flush()?;
    let offers = frames.with_file_name(
        frames
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("run.frames.csv")
            .replace(".frames.csv", ".offers.csv"),
    );
    let mut f = open(&offers)?;
    if first {
        writeln!(f, "session,frame,src,due_ms,offer_ms,status,staged")?;
    }
    for g in 0..sched.total() {
        let status = match o.status.get(g).copied().unwrap_or(0) {
            1 => "offered",
            2 => "gated",
            3 => "idle",
            _ => "not_reached",
        };
        writeln!(
            f,
            "{label},{g},{},{:.3},{:.3},{status},{}",
            sched.src[g],
            sched.at[g] as f64 / 1e6,
            o.offer_ns.get(g).copied().unwrap_or(0) as f64 / 1e6,
            o.staged.get(g).copied().unwrap_or(false) as u8,
        )?;
    }
    f.flush()
}

#[allow(clippy::too_many_arguments)]
fn session_json(
    l: &View,
    o: &Offers,
    recs: &[OutRec],
    lagged: u64,
    psets: PacketizerSets,
    t_start: Instant,
    shape: &Shape,
    feed_report: &feed::FeedReport,
) -> (Json, Vec<String>, Vec<(u64, u64)>) {
    let t0 = crate::streamer::audio::host_instant_to_ns(t_start);
    let rel = |ns: u64| ns.saturating_sub(t0);
    let mut st: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    let (mut real, mut repeats, mut clamped, mut keyframes) = (0usize, 0usize, 0usize, 0usize);
    let mut packets = Vec::new();
    let mut bytes = Vec::new();
    let mut emitted: Vec<u64> = Vec::new();
    let mut events: Vec<(u64, u64)> = Vec::new();
    for r in recs {
        let t = &r.timing;
        clamped += r.clamped as usize;
        keyframes += r.keyframe as usize;
        packets.push(r.packets as f64);
        bytes.push(r.au_bytes as f64);
        emitted.push(rel(t.packetize_done_ns));
        events.push((rel(t.packetize_done_ns), r.au_bytes as u64));
        if t.repeated {
            repeats += 1;
            if let Some(v) = span(t.encode_submit_ns, t.encode_done_ns) {
                st.entry("repeat_submit_to_encoded").or_default().push(v);
            }
            continue;
        }
        real += 1;
        let pairs = [
            ("offer_to_stage", t.capture_cb_entry_ns, t.scale_done_ns),
            ("offer_to_submit", t.capture_cb_entry_ns, t.encode_submit_ns),
            ("submit_to_encoded", t.encode_submit_ns, t.encode_done_ns),
            (
                "encoded_to_packetized",
                t.encode_done_ns,
                t.packetize_done_ns,
            ),
            ("offer_to_encoded", t.capture_cb_entry_ns, t.encode_done_ns),
            (
                "offer_to_packetized",
                t.capture_cb_entry_ns,
                t.packetize_done_ns,
            ),
            (
                "rtp_capture_to_packetized",
                r.capture_ns,
                t.packetize_done_ns,
            ),
        ];
        for (k, a, b) in pairs {
            if let Some(v) = span(a, b) {
                st.entry(k).or_default().push(v);
            }
        }
    }
    let rtp = (
        recs.first().map(|r| r.rtp_ts),
        recs.last().map(|r| r.rtp_ts),
    );
    emitted.sort_unstable();
    // achieved rate per whole second of the run, against what the source delivered in it
    let secs = (l.sched.timeline_ns / 1_000_000_000).max(1) as usize;
    let mut per_sec = vec![0usize; secs];
    for &t in &emitted {
        let s = (t / 1_000_000_000) as usize;
        if s < secs {
            per_sec[s] += 1;
        }
    }
    let mut due_sec = vec![0usize; secs];
    for (g, &a) in l.sched.at.iter().enumerate() {
        let s = (a / 1_000_000_000) as usize;
        if s < secs && o.staged.get(g).copied().unwrap_or(false) {
            due_sec[s] += 1;
        }
    }
    let worst = per_sec
        .iter()
        .enumerate()
        .skip(1)
        .min_by_key(|(_, n)| **n)
        .map(|(s, n)| json!({ "second": s, "frames": n, "staged_in_that_second": due_sec[s] }));
    // the longest gap between two emitted frames while the source was still offering; the
    // keepalive after the last offer is teardown, not a gap
    let last_offer =
        o.offer_ns.iter().copied().max().unwrap_or(0) + 1_000_000_000 / l.fps.max(1) as u64;
    let mut gap = (0u64, 0u64, 0u64);
    for w in emitted.windows(2) {
        if w[1] <= last_offer && w[1] - w[0] > gap.0 {
            gap = (w[1] - w[0], w[0], w[1]);
        }
    }
    let cause = if gap.0 == 0 {
        "none"
    } else {
        let (a, b) = (gap.1, gap.2);
        let in_gap: Vec<usize> = (0..l.sched.total())
            .filter(|&g| o.offer_ns[g] > a && o.offer_ns[g] < b)
            .collect();
        let staged = in_gap.iter().filter(|&&g| o.staged[g]).count();
        // a frame offered late because the feeder had not delivered it is the harness's fault,
        // not the pipeline's; the gap then ends at that late offer
        let feeder_late =
            o.feed.misses.iter().any(|&(g, c)| {
                c != "consumer" && o.offer_ns.get(g).is_some_and(|&t| t >= a && t <= b)
            });
        if feeder_late {
            "the feeder was late (disk, transform or upload): a harness fault"
        } else if in_gap.is_empty() {
            "the source delivered nothing (take cadence or an idle event)"
        } else if staged == 0 {
            "frames were offered but none staged (ring full or idle-withheld)"
        } else {
            "frames were staged but the encoder or the broadcast had not emitted them"
        }
    };
    // the first captured frame after each idle stretch: where a keepalive-to-motion spike shows
    let after_idle: Vec<Json> = shape
        .idle
        .iter()
        .map(|&(_, end)| {
            let first = recs
                .iter()
                .filter(|r| !r.timing.repeated)
                .find(|r| rel(r.timing.capture_cb_entry_ns) >= end);
            match first {
                Some(r) => json!({
                    "idle_end_s": end as f64 / 1e9,
                    "offer_to_packetized_ms": span(r.timing.capture_cb_entry_ns, r.timing.packetize_done_ns),
                    "submit_to_encoded_ms": span(r.timing.encode_submit_ns, r.timing.encode_done_ns),
                    "bytes": r.au_bytes,
                    "keyframe": r.keyframe,
                }),
                None => json!({ "idle_end_s": end as f64 / 1e9, "error": "no frame after it" }),
            }
        })
        .collect();
    let total_bytes: u64 = recs.iter().map(|r| r.au_bytes as u64).sum();
    let tel = l.telemetry.to_json();
    let enc_count = l.telemetry.frames_encoded.load(Ordering::Relaxed);
    let mut reasons = Vec::new();
    if lagged > 0 {
        reasons.push(format!(
            "{}: the packetizer lagged the broadcast by {lagged} frames",
            l.label
        ));
    }
    if enc_count as usize != recs.len() + lagged as usize {
        reasons.push(format!(
            "{}: telemetry counted {enc_count} encoded frames, the packetizer saw {}",
            l.label,
            recs.len()
        ));
    }
    let run_s = l.sched.timeline_ns as f64 / 1e9;
    let j = json!({
        "label": l.label,
        "vendor": l.vendor.name(),
        "encoder_size": [l.width, l.height],
        "fps": l.fps,
        "session": l.session_meta,
        "frames": {
            "scheduled": l.sched.total(),
            "delivered": o.delivered,
            "withheld_idle": o.withheld_idle,
            "gated_cadence": Json::Null,
            "gated_dirty": Json::Null,
            "exempted_cursor": Json::Null,
            "staged": o.staged_n,
            "dropped_no_slot": o.dropped_no_slot,
            "stage_errors": o.stage_errors,
            "device_busy": o.busy,
            "encoded": real,
            "repeats_and_keepalives": repeats,
            "packetized": recs.len(),
            "broadcast_lagged": lagged,
            "keyframes": keyframes,
            "rtp_ts_clamped": clamped,
            "rtp_ts_first_last": [rtp.0, rtp.1],
            "parameter_sets_dropped": psets.dropped_mtu + psets.sets.unaccounted,
            "parameter_sets": {
                "in_access_units": psets.sets.in_access_units,
                "sent": psets.sets.sent,
                "superseded_in_access_unit": psets.sets.superseded,
                "injected_before_keyframe": psets.sets.injected,
                "fragmented": psets.fragmented,
                "dropped_mtu": psets.dropped_mtu,
                "unaccounted": psets.sets.unaccounted,
            },
        },
        "errors": o.errors,
        "stopped": o.stopped,
        "stages_ms": st.iter().map(|(k, v)| (k.to_string(), summary(v))).collect::<serde_json::Map<_, _>>(),
        "packets_per_frame": summary(&packets),
        "frame_bytes": summary(&bytes),
        "avg_mbps": total_bytes as f64 * 8.0 / run_s.max(1e-9) / 1e6,
        "rate": {
            "encoded_fps": real as f64 / run_s.max(1e-9),
            "emitted_fps": recs.len() as f64 / run_s.max(1e-9),
            "per_second": per_sec,
            "worst_second": worst,
        },
        "longest_gap_ms": { "ms": gap.0 as f64 / 1e6, "from_s": gap.1 as f64 / 1e9, "cause": cause },
        "after_idle": after_idle,
        "telemetry": tel,
        "cross_check": {
            "telemetry_frames_encoded": enc_count,
            "packetized_plus_lagged": recs.len() as u64 + lagged,
            "telemetry_frames_dropped_ring": l.telemetry.degradation.frames_dropped_ring.load(Ordering::Relaxed),
            "offer_dropped_no_slot": o.dropped_no_slot,
        },
        "feeder": o.feed.to_json(&l.sched, (l.width * l.height * 4) as usize, feed_report, o.pacer),
    });
    (j, reasons, events)
}

pub(super) fn env_of(r: &super::recipe::Recipe) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (k, v) in &r.env {
        if !k.starts_with("SE_") {
            return Err(format!("env '{k}': only SE_* switches"));
        }
        std::env::set_var(k, v);
        out.push((k.clone(), v.clone()));
    }
    Ok(out)
}

/// The take the sessions read. Several feeders on one raw take each pull every picture off the
/// disk, so they read a compressed copy of the frames they need from the run's cache directory.
fn take_for_sessions(spec: &RunSpec, wants: &[Want]) -> Result<(Option<String>, Json), String> {
    let Some(p) = spec.clip_path.clone() else {
        return Ok((None, Json::Null));
    };
    let readers: Vec<&Want> = wants
        .iter()
        .filter(|w| !feed::renders_live(&w.workload))
        .collect();
    if readers.len() < 2 {
        return Ok((Some(p), Json::Null));
    }
    let Some(dir) = super::cache::cache_dir(spec) else {
        return Ok((
            Some(p),
            json!({ "used": false, "why": "no output directory" }),
        ));
    };
    let mut need = 0usize;
    for w in &readers {
        let (_, s, _) = feed::plan_at(&w.workload, Some(&p), 1, w.offset_s)?;
        need = need.max(s.src.iter().copied().max().unwrap_or(0) + 1);
    }
    let info = clip::scan(std::path::Path::new(&p))?;
    let (path, rep) = super::cache::compressed_copy(std::path::Path::new(&p), &info, need, &dir)?;
    Ok((Some(path.display().to_string()), rep))
}

/// The run's `gates` object: per-session counts summed, or why the gates were not run.
fn gates_json(sessions: &[Json]) -> Json {
    if sessions.is_empty() {
        return json!({
            "exercised": false,
            "why": "the recipe does not set `gates`: every frame the reader delivers is offered,                     and a rate below the take's is the reader's `fps:N` decimation",
        });
    }
    let sum = |k: &str| -> u64 { sessions.iter().filter_map(|s| s[k].as_u64()).sum() };
    json!({
        "exercised": true,
        "source": "the take's recorded cursor and dirty-region flags at its own callback cadence",
        "offered": sum("offered"),
        "gated_cadence": sum("gated_cadence"),
        "gated_dirty": sum("gated_dirty"),
        "cursor_exempted": sum("cursor_exempted"),
        "held": sum("held"),
        "trailing_delivered": sum("trailing_delivered"),
    })
}

pub fn run(spec: &RunSpec) -> Json {
    if let Some(f) = spec.recipe.test_fault.as_deref() {
        lab::fault(f);
    }
    let wall = Instant::now();
    let r = &spec.recipe.recipe;
    let env_set = match env_of(r) {
        Ok(e) => e,
        Err(e) => return lab::base(spec, "recipe_error", e),
    };
    if let Some(h) = r.gpu_clock_hold {
        std::env::set_var("SE_GPU_CLOCK_HOLD", if h { "1" } else { "0" });
    }
    if Input::parse(r.input.as_deref()) != Ok(Input::Bgra) {
        return lab::skipped(
            spec,
            "the pipe stage takes BGRA, as the capture handler does; NV12 is a lab axis".into(),
        );
    }
    let wants = match wants(spec) {
        Ok(w) => w,
        Err(e) => return lab::base(spec, "recipe_error", e),
    };
    tuning::apply_process_tuning();
    let sampler = Sampler::start();
    let shape = shape_of(spec);

    let (clip_path, cache_report) = match take_for_sessions(spec, &wants) {
        Ok(x) => x,
        Err(e) => return lab::base(spec, "setup_error", format!("take cache: {e}")),
    };

    // every session opens before any clock starts
    let mut lives: Vec<Live> = Vec::new();
    let mut feeders = Vec::new();
    let mut feeds: Vec<FeedSession> = Vec::new();
    for w in &wants {
        let read = if r.gates {
            without_decimation(&w.workload)
        } else {
            w.workload.clone()
        };
        let (source, mut sched, ops) =
            match feed::plan_at(&read, clip_path.as_deref(), spec.loops, w.offset_s) {
                Ok(x) => x,
                Err(e) => return lab::base(spec, "setup_error", format!("{}: {e}", w.label)),
            };
        let fps = lab::encode_fps(&w.workload, &source, &sched);
        let flags = match &source {
            feed::Source::Clip { info, .. } => Some(info.flags.clone()),
            feed::Source::Synthetic { .. } => None,
        };
        apply_bursts(&mut sched.at, &shape.bursts);
        let config =
            lab::production_config(&w.production, sched.width as u32, sched.height as u32, fps);
        let session = match lab::open_with(w.vendor, config, Input::Bgra, &w.set) {
            Ok(s) => s,
            Err((rejected, e, d)) => {
                let mut j = if rejected {
                    lab::base(
                        spec,
                        "rejected",
                        format!("{}: the encoder refused the recipe: {e}", w.label),
                    )
                } else {
                    lab::skipped(spec, format!("{} ({}): {e}", w.label, w.vendor.name()))
                };
                j["stage"] = json!("pipe");
                j["recipe_report"] = d.map(|d| d.to_json()).unwrap_or(Json::Null);
                return j;
            }
        };
        let dev = session.dev.clone();
        let sched = Arc::new(sched);
        let mut live = match build(w, session, Arc::clone(&sched), fps, config.bitrate_bps) {
            Ok(l) => l,
            Err(e) => return lab::base(spec, "setup_error", e),
        };
        if r.gates {
            live.gate = Some(SessionGate::new(
                &sched,
                fps,
                flags.as_deref(),
                r.auto_frame_rate,
                &live.path,
                &live.idr,
                live.epoch,
            ));
        }
        let (feeder, mut fs) = match feed::start(source, sched, ops, &[dev], Input::Bgra) {
            Ok(x) => x,
            Err(e) => return lab::base(spec, "setup_error", format!("feeder: {e}")),
        };
        lives.push(live);
        feeders.push(feeder);
        feeds.push(fs.remove(0));
    }
    let mut prefilled = true;
    for (f, s) in feeders.iter().zip(&feeds) {
        prefilled &= f.wait_prefill(
            std::slice::from_ref(s),
            feed::SLOTS,
            Duration::from_secs(60),
        );
    }

    let packetizers: Vec<_> = lives
        .iter()
        .map(|l| spawn_packetizer(l, l.sched.total()))
        .collect();
    let load_stop = Arc::new(AtomicBool::new(false));
    let load = load_sampler(Arc::clone(&load_stop));
    let t_start = Instant::now() + Duration::from_millis(30);
    let spec_arc = Arc::new(spec.clone());
    let shape = Arc::new(shape);
    struct Pair(Live, FeedSession);
    unsafe impl Send for Pair {}
    let handles: Vec<_> = lives
        .into_iter()
        .zip(feeds)
        .map(|(l, f)| {
            let spec = Arc::clone(&spec_arc);
            let shape = Arc::clone(&shape);
            let pair = Pair(l, f);
            std::thread::Builder::new()
                .name("bench-pipe-offer".into())
                .spawn(move || {
                    let pair = pair;
                    let Pair(mut l, f) = pair;
                    let o = offer(&mut l, f, &spec, t_start, &shape);
                    struct Back(Live, Offers);
                    unsafe impl Send for Back {}
                    Back(l, o)
                })
                .expect("spawn offer")
        })
        .collect();
    let mut done: Vec<(Live, Offers)> = Vec::new();
    for h in handles {
        match h.join() {
            Ok(b) => done.push((b.0, b.1)),
            Err(_) => return lab::base(spec, "setup_error", "an offer thread panicked".into()),
        }
    }
    let offer_end = Instant::now();
    // let the encoders drain, then stop them as a session's teardown does: the broadcast
    // closes once the session's own sender and the encode or repeater thread's are gone
    std::thread::sleep(Duration::from_millis(400));
    let feed_reports: Vec<feed::FeedReport> = feeders.into_iter().map(|f| f.finish()).collect();
    let mut results = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    for rep in &feed_reports {
        if let Some(e) = &rep.producer_error {
            reasons.push(format!("reader: {e}"));
        }
        if let Some(e) = &rep.uploader_error {
            reasons.push(format!("uploader: {e}"));
        }
    }
    let mut all_events: Vec<(u64, u64)> = Vec::new();
    let mut events_log = Vec::new();
    let mut lateness_total = (0usize, 0usize, 0usize);
    let mut gate_totals: Vec<Json> = Vec::new();
    for (((l, o), pk), rep) in done.into_iter().zip(packetizers).zip(&feed_reports) {
        let Live {
            label,
            vendor,
            path,
            session_meta,
            telemetry,
            tx,
            sched,
            fps,
            width,
            height,
            gate,
            ..
        } = l;
        // as in the capture handler's teardown: the trailing timer goes before the ring stops
        let gate_json = gate.as_ref().map(SessionGate::to_json);
        drop(gate);
        match path {
            Path::Ring(mut ring) => ring.stop(),
            Path::Inline { stop, .. } => stop.store(true, Ordering::Relaxed),
        }
        drop(tx);
        let live_view = View {
            label,
            vendor,
            session_meta,
            telemetry,
            sched,
            fps,
            width,
            height,
        };
        let (recs, lagged, psets) = pk.join().unwrap_or_default();
        let late = o.late_ns.iter().filter(|&&x| x > LATE_NS).count();
        let harness = late - o.late_by_encoder.min(late);
        lateness_total.0 += late;
        lateness_total.1 += harness;
        lateness_total.2 += o.late_ns.len();
        let hm: Vec<_> = o
            .feed
            .misses
            .iter()
            .filter(|(_, c)| *c != "consumer")
            .collect();
        if !hm.is_empty() {
            let mut causes: BTreeMap<&str, usize> = BTreeMap::new();
            for (_, c) in &hm {
                *causes.entry(*c).or_default() += 1;
            }
            reasons.push(format!(
                "{}: {} frames were not on the GPU by their deadline ({})",
                live_view.label,
                hm.len(),
                causes
                    .iter()
                    .map(|(c, n)| format!("{c} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if o.reader_gap {
            reasons.push(format!("{}: the feeder ended early", live_view.label));
        }
        if let Some(s) = &o.stopped {
            reasons.push(format!("{}: {s}", live_view.label));
        }
        if let Some(p) = spec.per_frame_path.as_deref() {
            let path = std::path::Path::new(p);
            if results.is_empty() {
                let _ = std::fs::remove_file(path);
                let _ = std::fs::remove_file(path.with_extension("").with_extension("offers.csv"));
            }
            if let Err(e) = write_per_frame(
                path,
                &live_view.label,
                &live_view.sched,
                &o,
                &recs,
                t_start,
                results.is_empty(),
            ) {
                reasons.push(format!("per-frame CSV: {e}"));
            }
        }
        let (mut j, rs, ev) =
            session_json(&live_view, &o, &recs, lagged, psets, t_start, &shape, rep);
        let lm: Vec<f64> = o.late_ns.iter().map(|&x| x as f64 / 1e6).collect();
        j["pacing"] = json!({
            "late_over_2ms": late,
            "late_over_2ms_encoder_blocked": o.late_by_encoder,
            "late_over_2ms_harness": harness,
            "late_ms": summary(&lm),
            "reader_underruns": o.underruns,
            "timer": o.pacer,
        });
        if let Some(mut g) = gate_json {
            g["offered"] = json!(o.delivered);
            g["not_offered"] = json!(o.gated);
            gate_totals.push(g.clone());
            j["gates"] = g;
        }
        reasons.extend(rs);
        all_events.extend(ev);
        events_log.extend(o.events);
        results.push(j);
    }
    load_stop.store(true, Ordering::Relaxed);
    let load = load.join().unwrap_or_default();
    if lateness_total.1 as f64 > LATE_SHARE * lateness_total.2.max(1) as f64 {
        reasons.push(format!(
            "{} of {} offers released > 2 ms late by the harness",
            lateness_total.1, lateness_total.2
        ));
    }
    if !prefilled {
        reasons.push("a feeder could not prefill within 60 s".into());
    }
    all_events.sort_unstable();
    let total_frames: u64 = results
        .iter()
        .filter_map(|j| j["frames"]["encoded"].as_u64())
        .sum();
    let mut out = json!({
        "id": spec.id,
        "result": "completed",
        "valid": reasons.is_empty(),
        "invalid_reasons": reasons,
        "stage": "pipe",
        "platform": "windows",
        "sessions_n": results.len(),
        "take_cache": cache_report,
        "gates": gates_json(&gate_totals),
        "aggregate": {
            "encoded": total_frames,
            "peak_1s_mbps": super::super::metrics::peak_rate_bps(&all_events, 1_000_000_000) / 1e6,
            "late_over_2ms_harness": lateness_total.1,
        },
        "per_session": results,
        "events": events_log,
        "load": load,
        "gpu_clock_hold": { "policy": clock_hold::wanted().0, "why": clock_hold::wanted().1 },
        "tuning": tune::report(),
        "tuning_unknown": tune::unknown(),
        "env_set": env_set,
        "offer_span_s": offer_end.saturating_duration_since(t_start).as_secs_f64(),
    });
    if let Some(first) = out["per_session"].get(0).cloned() {
        // the macOS pipe's top-level keys, from session 0
        out["frames"] = first["frames"].clone();
        out["stages_ms"] = first["stages_ms"].clone();
        out["telemetry"] = first["telemetry"].clone();
        out["avg_mbps"] = first["avg_mbps"].clone();
        out["pacing"] = first["pacing"].clone();
    }
    out["series"] = sampler.finish();
    out["wall_s"] = json!(wall.elapsed().as_secs_f64());
    out
}

#[cfg(test)]
mod tests {
    use super::super::super::plan::{Plan, WIN_PIPE_GATES, WIN_PIPE_QUICK, WIN_PIPE_SESSIONS};
    use super::*;

    /// The accounting describes the packetizer: every distinct set an access unit carries is
    /// sent (Quick Sync's several PPS ids included), a repeated type and id sends only its last
    /// copy, stored sets are put in front of a keyframe that lacks them, and a set larger than the
    /// MTU goes out fragmented and is still sent.
    #[test]
    fn parameter_set_accounting_matches_the_packetizer() {
        // SPS id 0 / PPS id n, with the ids written as exp-Golomb codes
        const SPS0: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0x80];
        const PPS: [&[u8]; 5] = [
            &[0x68, 0x80],
            &[0x68, 0x40],
            &[0x68, 0x60],
            &[0x68, 0x20],
            &[0x68, 0x28],
        ];
        const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x33];
        const P: &[u8] = &[0x41, 0x9a, 0x02];
        let au = |units: &[&[u8]]| -> Bytes {
            let mut v = Vec::new();
            for u in units {
                v.extend_from_slice(&[0, 0, 0, 1]);
                v.extend_from_slice(u);
            }
            Bytes::from(v)
        };
        let run = |mtu: usize, aus: &[Bytes]| -> (ParamSets, u64) {
            let mut pk = H264Packetizer::new();
            let (mut out, mut nals, mut s) = (Vec::new(), Vec::new(), ParamSets::default());
            for a in aus {
                pk.packetize(mtu, a, &mut out);
                s.add(a, &out, &mut nals);
            }
            (s, pk.fragmented_parameter_sets())
        };
        let ps = |in_au, superseded, sent, injected| ParamSets {
            in_access_units: in_au,
            superseded,
            sent,
            injected,
            unaccounted: 0,
        };
        // one pair: both sent
        let (s, _) = run(RTP_PAYLOAD_MTU_LAN, &[au(&[SPS0, PPS[0], IDR])]);
        assert_eq!(s, ps(2, 0, 2, 0));
        // Quick Sync's five PPS ids: all six sets sent, none superseded
        let mut qsv: Vec<&[u8]> = vec![SPS0];
        qsv.extend_from_slice(&PPS);
        qsv.push(IDR);
        let (s, _) = run(RTP_PAYLOAD_MTU_LAN, &[au(&qsv)]);
        assert_eq!(s, ps(6, 0, 6, 0));
        // the same pair twice in one access unit: the first copies are superseded
        let (s, _) = run(
            RTP_PAYLOAD_MTU_LAN,
            &[au(&[SPS0, PPS[0], SPS0, PPS[0], IDR])],
        );
        assert_eq!(s, ps(4, 2, 2, 0));
        // a keyframe without sets after one with them gets the stored pair injected; a P-frame
        // without sets gets nothing
        let (s, _) = run(
            RTP_PAYLOAD_MTU_LAN,
            &[au(&[SPS0, PPS[0], PPS[1], IDR]), au(&[P]), au(&[IDR])],
        );
        assert_eq!(s, ps(3, 0, 6, 3));
        // an MTU smaller than the SPS: it goes out as FU-A, still counted as sent, and the
        // packetizer's own counter records the fragmentation
        let (s, fragmented) = run(4, &[au(&[SPS0, PPS[0], IDR])]);
        assert_eq!(s, ps(2, 0, 2, 0));
        assert_eq!(fragmented, 1);
    }

    #[test]
    fn a_burst_plays_twice_the_source_in_its_window_and_pulls_the_rest_forward() {
        let mut at: Vec<u64> = (0..10).map(|k| k * 100).collect();
        apply_bursts(&mut at, &[(200, 400)]);
        assert_eq!(at, vec![0, 100, 200, 250, 300, 350, 400, 500, 600, 700]);
    }

    #[test]
    fn idle_and_burst_windows_come_from_the_events() {
        let spec: RunSpec = serde_json::from_value(json!({
            "id": "x", "group": 0, "repeat": 0, "loops": 1, "psnr": false, "per_frame": false,
            "recipe": { "name": "r", "vendor": "nvenc" },
            "workload": { "id": "w", "kind": "synthetic" },
            "events": [
                { "at_s": 2.0, "action": "idle", "value": 3 },
                { "at_s": 10.0, "action": "burst", "value": 1.5 },
                { "at_s": 1.0, "action": "force_idr" },
            ],
        }))
        .unwrap();
        let s = shape_of(&spec);
        assert_eq!(s.idle, vec![(2_000_000_000, 5_000_000_000)]);
        assert_eq!(s.bursts, vec![(10_000_000_000, 11_500_000_000)]);
    }

    #[test]
    fn the_pipe_plans_expand_and_every_mix_names_known_vendors() {
        let q = Plan::parse(WIN_PIPE_QUICK).unwrap();
        let qr = q.expand().unwrap();
        assert_eq!(qr.len(), 4);
        assert!(qr.iter().all(|r| r.stage == "pipe"));
        let s = Plan::parse(WIN_PIPE_SESSIONS).unwrap();
        let sr = s.expand().unwrap();
        assert_eq!(sr.len(), 12);
        for r in &sr {
            let w = wants(r).unwrap();
            assert!(!w.is_empty() && w.len() <= 4, "{}", r.id);
            let offsets: Vec<f64> = w.iter().map(|x| x.offset_s).collect();
            assert!(
                offsets.windows(2).all(|p| p[1] > p[0]),
                "{} sessions start apart",
                r.id
            );
        }
        let four = sr.iter().find(|r| r.recipe.recipe.name == "mix4").unwrap();
        let w = wants(four).unwrap();
        assert_eq!(
            w[1].workload.transform.as_deref(),
            Some("fps:120,tile2x2,crop:2560x1440")
        );
        assert_eq!(w[3].vendor, LabVendor::Qsv);
        let g = Plan::parse(WIN_PIPE_GATES).unwrap().expand().unwrap();
        assert_eq!(g.len(), 5);
        assert!(g.iter().all(|r| r.recipe.recipe.gates));
        let read = without_decimation(&g[0].workload);
        assert_eq!(read.transform.as_deref(), Some(""));
        assert_eq!(lab::transform_fps(&g[0].workload), Some(60.0));
    }

    #[test]
    fn recorded_flags_map_to_cursor_and_dirty_answers() {
        use super::super::clipfmt::{FLAG_CURSOR_MOVED, FLAG_DIRTY_EMPTY, FLAG_DIRTY_KNOWN};
        let sched = Schedule {
            at: vec![0, 1, 2, 3],
            src: vec![3, 2, 1, 0],
            force_idr: vec![false; 4],
            per_loop: 4,
            loops: 1,
            period_ns: 4,
            timeline_ns: 4,
            width: 16,
            height: 16,
            fps: 60.0,
        };
        let flags = [
            0,
            FLAG_DIRTY_KNOWN,
            FLAG_DIRTY_KNOWN | FLAG_DIRTY_EMPTY,
            FLAG_CURSOR_MOVED,
        ];
        let (c, d) = recorded_flags(&sched, Some(&flags));
        assert_eq!(c, vec![true, false, false, false]);
        assert_eq!(d, vec![None, Some(false), Some(true), None]);
        let (c, d) = recorded_flags(&sched, None);
        assert!(c.iter().all(|x| !x) && d.iter().all(Option::is_none));
    }
}

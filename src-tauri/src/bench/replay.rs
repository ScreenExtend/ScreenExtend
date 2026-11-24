//! The replay engine: one clip, one encoder configuration, real time, through the production
//! `Encoder` type. Runs inside a child process (`bench --child run`).
//!
//! Three threads: a read-ahead reader that copies records into IOSurface-backed `420f` buffers
//! from a `CVPixelBufferPool` (so disk and allocation never delay a submit), the submit loop on
//! the deadline scheduler with absolute-deadline pacing (sleep, then spin the last ~1.5 ms), and a
//! collector draining the encoder's output channel that only stamps and counts.
//!
//! `sessions: N` runs N encoders in the one child, each fed the same pixel buffer in turn on the
//! submit thread and drained by its own collector: N client devices sharing the machine's encoder
//! hardware. The top-level metrics are session 0's; `per_session` and `aggregate` hold the rest.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, RecvTimeoutError};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use serde_json::{json, Value as Json};

use crate::macos_utils::streamer::config::EncoderConfig;
use crate::macos_utils::streamer::encoder::{
    Encoder, EncoderIdentity, FrameTag, RecipeReport, SessionCreateFailed,
};
use crate::macos_utils::streamer::tune;
use crate::macos_utils::streamer::vt_recipe::{Codec, Val};
use crate::streamer::config::H264Profile;
use crate::streamer::telemetry::FrameTiming;

use super::cf;
use super::clip::{self, ClipInfo, Transform};
use super::metrics::{h264_sps_info, hevc_sps_info, peak_rate_bps, summary};
use super::plan::{Event, RunSpec};
use super::series::{self, Row, Sampler};

/// A run with more than this share of frames submitted > 2 ms late is not a measurement.
const LATE_NS: u64 = 2_000_000;
const LATE_SHARE: f64 = 0.01;

struct FrameIn {
    g: usize,
    pb: CFRetained<CVPixelBuffer>,
}
unsafe impl Send for FrameIn {}

struct Obs {
    ltr_token: Option<i64>,
    attach: Option<Json>,
    g: usize,
    at: Instant,
    status: i32,
    info: u32,
    bytes: u64,
    key: bool,
    picture: bool,
}

/// What one callback said, by source index: (time ns, status, info flags, bytes, keyframe).
type CbRow = Option<(u64, i32, u32, u64, bool)>;

struct Collected {
    obs: Vec<Obs>,
    params: Option<Vec<Vec<u8>>>,
    kept: Vec<(usize, CFRetained<CMSampleBuffer>)>,
}
unsafe impl Send for Collected {}

pub(super) fn fault(kind: &str) {
    eprintln!("[bench] test fault '{kind}' requested");
    match kind {
        // std installs its own SIGSEGV handler (stack-overflow detection) that returns for a
        // signal that is not a guard-page fault, so a bare `raise` is survived. Default it first.
        "crash" => unsafe {
            libc::signal(libc::SIGSEGV, libc::SIG_DFL);
            libc::raise(libc::SIGSEGV);
        },
        "abort" => std::process::abort(),
        "panic" => panic!("bench test fault"),
        "hang" => loop {
            std::thread::sleep(Duration::from_secs(1));
        },
        _ => {}
    }
}

pub(super) fn setup_error(spec: &RunSpec, msg: String) -> Json {
    json!({ "id": spec.id, "result": "setup_error", "valid": false, "invalid_reasons": [msg.clone()], "error": msg })
}

pub(super) fn production_config(spec: &RunSpec, w: u32, h: u32, fps: u32) -> EncoderConfig {
    let p = spec.recipe.production.clone().unwrap_or_default();
    let computed = ((w as u64 * h as u64 * fps as u64) / 10).clamp(6_000_000, 30_000_000) as u32;
    let bitrate = p.bitrate_bps.unwrap_or(computed);
    EncoderConfig {
        width: w,
        height: h,
        fps,
        bitrate_bps: bitrate,
        max_bitrate_bps: bitrate,
        profile: match p.profile.as_deref() {
            Some("main") => H264Profile::Main,
            Some("baseline") => H264Profile::Baseline,
            _ => H264Profile::High,
        },
        qp: p.qp,
        min_qp: p.min_qp,
        intra_refresh: false,
        gpu_registry_id: None,
        software_encode: p.software,
        recipe: if spec.recipe.production.is_some() {
            None
        } else {
            Some(Arc::new(spec.recipe.recipe.clone()))
        },
    }
}

pub(super) fn copy_into(
    pb: &CVPixelBuffer,
    src: &[u8],
    info: &ClipInfo,
    t: Transform,
    ow: usize,
    oh: usize,
) {
    let (w, h) = (info.width, info.height);
    let (ox, oy) = t.crop_origin(w, h);
    unsafe {
        CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(0));
        let y_dst = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *mut u8;
        let y_bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
        let c_dst = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *mut u8;
        let c_bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
        if !y_dst.is_null() && !c_dst.is_null() {
            t.copy_plane(&src[..w * h], w, h, y_dst, y_bpr, ow, oh, (ox, oy));
            t.copy_plane(
                &src[w * h..],
                w,
                h / 2,
                c_dst,
                c_bpr,
                ow,
                oh / 2,
                (ox, oy / 2),
            );
        }
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags(0));
    }
}

/// Frame index -> the events that fire on it.
pub(super) fn event_schedule(events: &[Event], at: &[u64]) -> BTreeMap<usize, Vec<usize>> {
    let total = at.len();
    let mut fire: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (ei, ev) in events.iter().enumerate() {
        let first = match (ev.at_frame, ev.at_s) {
            (Some(f), _) => f as usize,
            (None, Some(s)) => at.partition_point(|&t| (t as f64) < s * 1e9),
            _ => 0,
        };
        if first >= total {
            continue;
        }
        fire.entry(first).or_default().push(ei);
        if let Some(every) = ev.every_s.filter(|e| *e > 0.0) {
            let mut k = 1u64;
            loop {
                let t = at[first] as f64 + every * 1e9 * k as f64;
                let g = at.partition_point(|&x| (x as f64) < t);
                if g >= total {
                    break;
                }
                fire.entry(g).or_default().push(ei);
                k += 1;
            }
        }
    }
    fire
}

/// One encoder's state through the submit loop.
struct Sess {
    enc: Encoder,
    expected: Arc<AtomicUsize>,
    collector: Option<std::thread::JoinHandle<Collected>>,
    create_ms: f64,
    submit_at: Vec<Option<Instant>>,
    call_us: Vec<f64>,
    enc_status: Vec<i32>,
    flush_fail: (usize, Option<(usize, i32)>),
    submitted: usize,
    submitted_ok: usize,
    first_encode_err: Option<(usize, i32)>,
    max_in_flight: usize,
    ltr: Option<LtrState>,
}

/// The acknowledgement side of the LTR harness, per session.
struct LtrState {
    rx: crossbeam_channel::Receiver<(usize, i64)>,
    /// Frames a token waits; `None` = never acknowledged.
    delay: Option<u64>,
    pending: Vec<(usize, i64)>,
    seen: usize,
    acked: Vec<(usize, i64)>,
    frames_carrying_acks: usize,
    enable: Json,
    set_failures: Vec<Json>,
}

/// What one encoder left behind once it is gone.
struct SessDone {
    col: Collected,
    create_ms: f64,
    submit_at: Vec<Option<Instant>>,
    call_us: Vec<f64>,
    enc_status: Vec<i32>,
    flush_fail: (usize, Option<(usize, i32)>),
    submitted: usize,
    submitted_ok: usize,
    first_encode_err: Option<(usize, i32)>,
    max_in_flight: usize,
    flush_status: i32,
    output_drops: u64,
    identity: EncoderIdentity,
    recipe_report: Option<RecipeReport>,
    pending_after: Option<Json>,
    low_latency: bool,
    backend: &'static str,
    ltr: Option<LtrState>,
}

/// The shared facts every session's metrics are computed against.
struct Ctx<'a> {
    at: &'a [u64],
    total: usize,
    n_src: usize,
    loops: usize,
    t_start: Instant,
    timeline_ns: u64,
    codec: Codec,
}

struct SessOut {
    json: Json,
    rows: Vec<Row>,
    lat_ms: Vec<f64>,
    out_events: Vec<(u64, u64)>,
    reasons: Vec<String>,
    cb_at: Vec<CbRow>,
}

fn spawn_collector(
    out: crossbeam_channel::Receiver<crate::macos_utils::streamer::encoder::CompressedSample>,
    expected: Arc<AtomicUsize>,
    keep: bool,
    hevc: bool,
    total: usize,
    ltr_tx: Option<crossbeam_channel::Sender<(usize, i64)>>,
) -> std::thread::JoinHandle<Collected> {
    std::thread::Builder::new()
        .name("bench-collect".into())
        .spawn(move || {
            crate::macos_utils::streamer::qos::pin_current_thread_user_interactive();
            let mut c = Collected {
                obs: Vec::with_capacity(total),
                params: None,
                kept: Vec::new(),
            };
            let mut idle_since: Option<Instant> = None;
            loop {
                match out.recv_timeout(Duration::from_millis(20)) {
                    Ok(cs) => {
                        idle_since = None;
                        let g = cs
                            .tag
                            .as_ref()
                            .map_or(usize::MAX, |t| t.timing.capture_available_ns as usize);
                        let (bytes, key) = match cs.sample() {
                            Some(sb) if cs.status == 0 => {
                                (cf::sample_size(sb) as u64, cf::is_keyframe(sb))
                            }
                            _ => (0, false),
                        };
                        let picture = cs.status == 0 && !cs.is_rate_drop() && bytes > 0;
                        if picture && key && c.params.is_none() {
                            c.params = cs.sample().map(|sb| cf::parameter_sets(sb, hevc));
                        }
                        if keep && picture {
                            if let Some(sb) = cs.sample() {
                                c.kept
                                    .push((g, unsafe { CFRetained::retain(NonNull::from(sb)) }));
                            }
                        }
                        let (ltr_token, attach) = match (&ltr_tx, cs.sample()) {
                            (Some(tx), Some(sb)) if picture => {
                                let tok = cf::ltr_token(sb);
                                if let Some(t) = tok {
                                    let _ = tx.send((g, t));
                                }
                                (tok, Some(cf::attachments_json(sb)))
                            }
                            _ => (None, None),
                        };
                        c.obs.push(Obs {
                            ltr_token,
                            attach,
                            g,
                            at: cs.callback_at,
                            status: cs.status,
                            info: cs.info_flags,
                            bytes,
                            key,
                            picture,
                        });
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        let want = expected.load(Ordering::Acquire);
                        if want != usize::MAX {
                            if c.obs.len() >= want {
                                break;
                            }
                            let since = *idle_since.get_or_insert_with(Instant::now);
                            if since.elapsed() > Duration::from_secs(5) {
                                break;
                            }
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            c
        })
        .expect("spawn collector")
}

/// Everything one encoder produced, as JSON plus the raw rows the run-level views pool.
fn session_metrics(cx: &Ctx, d: &SessDone) -> SessOut {
    let (at, total, n_src, loops, t_start) = (cx.at, cx.total, cx.n_src, cx.loops, cx.t_start);
    let ns_since = |t: Instant| t.saturating_duration_since(t_start).as_nanos() as u64;
    let col = &d.col;
    let mut picture = vec![false; total];
    let mut errored_hist: BTreeMap<i32, usize> = BTreeMap::new();
    let mut first_cb_error: Option<Json> = None;
    let (mut dropped, mut errored, mut unknown_tag) = (0usize, 0usize, 0usize);
    let mut lat_ms: Vec<f64> = Vec::new();
    let mut lat_by_loop: Vec<Vec<f64>> = vec![Vec::new(); loops];
    let (mut bytes, mut key_bytes) = (Vec::<f64>::new(), Vec::<f64>::new());
    let mut out_events: Vec<(u64, u64)> = Vec::new();
    let mut pic_times: Vec<u64> = Vec::new();
    let mut total_bytes = 0u64;
    let mut cb_at: Vec<CbRow> = vec![None; total];
    let mut rows: Vec<Row> = Vec::with_capacity(total);
    let mut seen = vec![false; total];
    for o in &col.obs {
        if o.g >= total {
            unknown_tag += 1;
            continue;
        }
        seen[o.g] = true;
        cb_at[o.g] = Some((ns_since(o.at), o.status, o.info, o.bytes, o.key));
        let mut row = Row {
            t_ns: at[o.g],
            ..Default::default()
        };
        if o.status != 0 {
            errored += 1;
            *errored_hist.entry(o.status).or_default() += 1;
            if first_cb_error.is_none() {
                first_cb_error = Some(json!({
                    "frame": o.g, "status": o.status,
                    "submit_t_s": at[o.g] as f64 / 1e9,
                    "callback_t_s": ns_since(o.at) as f64 / 1e9,
                }));
            }
            row.errored = true;
            rows.push(row);
            continue;
        }
        if !o.picture {
            dropped += 1;
            row.dropped = true;
            rows.push(row);
            continue;
        }
        picture[o.g] = true;
        if let Some(s) = d.submit_at[o.g] {
            let l = o.at.saturating_duration_since(s).as_nanos() as f64 / 1e6;
            lat_ms.push(l);
            lat_by_loop[o.g / n_src].push(l);
            row.lat_ms = Some(l);
        }
        row.bytes = o.bytes;
        rows.push(row);
        bytes.push(o.bytes as f64);
        if o.key {
            key_bytes.push(o.bytes as f64);
        }
        total_bytes += o.bytes;
        out_events.push((ns_since(o.at), o.bytes));
        pic_times.push(ns_since(o.at));
    }
    // A frame submitted and never called back is an error in the time series too.
    for g in 0..total {
        if !seen[g] && d.enc_status[g] == 0 && d.submit_at[g].is_some() {
            rows.push(Row {
                t_ns: at[g],
                errored: true,
                ..Default::default()
            });
        }
    }
    out_events.sort_unstable();
    pic_times.sort_unstable();
    let gaps_ms: Vec<f64> = pic_times
        .windows(2)
        .map(|w| (w[1] - w[0]) as f64 / 1e6)
        .collect();
    let (mut run, mut longest) = (0usize, 0usize);
    for &p in picture.iter().take(d.submitted) {
        if p {
            run = 0;
        } else {
            run += 1;
            longest = longest.max(run);
        }
    }
    let missing = d.submitted_ok.saturating_sub(col.obs.len());
    let secs = cx.timeline_ns as f64 / 1e9;

    let mut reasons: Vec<String> = Vec::new();
    if d.output_drops > 0 {
        reasons.push(format!(
            "{} output-queue drops (collector fell behind)",
            d.output_drops
        ));
    }
    if d.submitted < total {
        reasons.push(format!("only {} of {total} frames submitted", d.submitted));
    }
    if missing > 0 {
        reasons.push(format!("{missing} callbacks never arrived"));
    }

    let params = col.params.clone().unwrap_or_default();
    let sps = if params.is_empty() {
        json!({ "error": "no keyframe emitted" })
    } else if cx.codec == Codec::Hevc {
        hevc_sps_info(&params)
    } else {
        h264_sps_info(&params)
    };
    let mut lat_by_10: BTreeMap<u64, Vec<f64>> = BTreeMap::new();
    for r in &rows {
        if let Some(l) = r.lat_ms {
            lat_by_10
                .entry(r.t_ns / 10_000_000_000)
                .or_default()
                .push(l);
        }
    }
    let first = d.first_encode_err;
    let json = json!({
        "session_create_status": 0,
        "session_create_ms": d.create_ms,
        "identity": {
            "encoder_id": d.identity.encoder_id,
            "hardware": d.identity.hardware,
            "gpu_registry_id": d.identity.gpu_registry_id,
            "backend": d.backend,
            "low_latency_spec": d.low_latency,
            "rate_tier": d.identity.rate_tier,
            "supports_base_frame_qp": d.identity.supports_base_frame_qp,
        },
        "recipe_report": d.recipe_report.as_ref().map(|r| json!(r)),
        "submitted": d.submitted,
        "submitted_ok": d.submitted_ok,
        "callbacks": col.obs.len(),
        "emitted": bytes.len(),
        "no_pictures": bytes.is_empty(),
        "dropped": dropped,
        "dropped_pct": if d.submitted > 0 { (dropped as f64 * 1e4 / d.submitted as f64).round() / 100.0 } else { 0.0 },
        "errored": errored,
        "errored_by_status": errored_hist.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "first_callback_error": first_cb_error,
        "missing_callbacks": missing,
        "unknown_tags": unknown_tag,
        "longest_drop_run": longest,
        "encode_call_status_first_failure": first.map(|(g, s)| json!({ "frame": g, "status": s, "t_s": at[g] as f64 / 1e9 })),
        "encode_call_failures": d.submitted - d.submitted_ok,
        "flush_each_failures": d.flush_fail.0,
        "flush_each_first_failure": d.flush_fail.1.map(|(g, s)| json!({ "frame": g, "status": s })),
        "final_flush_status": d.flush_status,
        "pending_after_flush": d.pending_after,
        "encode_call_us": summary(&d.call_us),
        "latency_ms": summary(&lat_ms),
        "latency_ms_by_loop": lat_by_loop.iter().map(|v| summary(v)).collect::<Vec<_>>(),
        "latency_ms_by_10s": lat_by_10.iter().map(|(b, v)| json!({ "from_s": b * 10, "stats": summary(v) })).collect::<Vec<_>>(),
        "buckets_10s": series::buckets(10, cx.timeline_ns, &rows),
        "buckets_60s": series::buckets(60, cx.timeline_ns, &rows),
        "output_gap_ms": summary(&gaps_ms),
        "frame_bytes": summary(&bytes),
        "keyframes": key_bytes.len(),
        "keyframe_bytes": summary(&key_bytes),
        "avg_mbps": total_bytes as f64 * 8.0 / secs.max(1e-9) / 1e6,
        "peak_1s_mbps": peak_rate_bps(&out_events, 1_000_000_000) / 1e6,
        "peak_100ms_mbps": peak_rate_bps(&out_events, 100_000_000) / 1e6,
        "sps": sps,
        "output_drops": d.output_drops,
        "max_in_flight": d.max_in_flight,
    });
    SessOut {
        json,
        rows,
        lat_ms,
        out_events,
        reasons,
        cb_at,
    }
}

/// Apply one event to every session. The log entry carries one result per session.
fn apply_event(
    sessions: &mut [Sess],
    ev: &Event,
    g: usize,
    t_s: f64,
    force: &mut bool,
    ltr_refresh: &mut bool,
    log: &mut Vec<Json>,
) {
    let one_or_all = |v: Vec<Json>| -> Json {
        if v.len() == 1 {
            v.into_iter().next().unwrap_or(Json::Null)
        } else {
            Json::Array(v)
        }
    };
    match ev.action.as_str() {
        "force_idr" => {
            *force = true;
            log.push(json!({ "frame": g, "t_s": t_s, "action": "force_idr" }));
        }
        "force_ltr_refresh" => {
            *ltr_refresh = true;
            log.push(json!({ "frame": g, "t_s": t_s, "action": "force_ltr_refresh" }));
        }
        "set" => {
            let key = ev.key.clone().unwrap_or_default();
            let val = ev.value.clone().unwrap_or(Val::Null);
            let r: Vec<Json> = sessions
                .iter_mut()
                .map(|s| json!(s.enc.set_property(&key, &val)))
                .collect();
            log.push(json!({ "frame": g, "t_s": t_s, "action": "set", "result": one_or_all(r) }));
        }
        "set_frame_opt" => {
            let key = ev.key.clone().unwrap_or_default();
            let r: Vec<Json> = sessions
                .iter_mut()
                .map(|s| match s.enc.set_frame_opt(&key, ev.value.clone()) {
                    Some(p) => json!(p),
                    None => json!({ "key": key, "removed": true }),
                })
                .collect();
            log.push(json!({
                "frame": g, "t_s": t_s, "action": "set_frame_opt", "key": key,
                "value": ev.value, "result": one_or_all(r),
            }));
        }
        other => log.push(json!({ "frame": g, "action": other, "error": "unknown action" })),
    }
}

pub fn run(spec: &RunSpec) -> Json {
    if let Some(f) = spec.recipe.test_fault.as_deref() {
        fault(f);
    }
    let wall = Instant::now();
    let sampler = Sampler::start();
    let (rss0, _) = cf::rss();
    let thermal0 = cf::thermal_state();
    let Some(path) = spec.clip_path.clone() else {
        return setup_error(spec, "no clip path".into());
    };
    let path = std::path::PathBuf::from(path);
    let info = match clip::scan(&path) {
        Ok(i) => i,
        Err(e) => return setup_error(spec, e),
    };
    let transform = match Transform::parse(spec.workload.transform.as_deref()) {
        Ok(t) => t,
        Err(e) => return setup_error(spec, e),
    };
    let (ow, oh) = match transform.out_dims(info.width, info.height) {
        Ok(d) => d,
        Err(e) => return setup_error(spec, e),
    };
    let n_src = spec
        .workload
        .max_frames
        .map_or(info.frames, |m| m.min(info.frames))
        .max(1);
    let loops = spec.loops.max(1) as usize;
    let speed = if spec.workload.speed > 0.0 {
        spec.workload.speed
    } else {
        1.0
    };
    let period_ns = info.times[n_src - 1] + info.median_gap_ns();
    let total = n_src * loops;
    let at: Vec<u64> = (0..total)
        .map(|g| {
            let raw = info.times[g % n_src] + (g / n_src) as u64 * period_ns;
            (raw as f64 / speed) as u64
        })
        .collect();
    let timeline_ns = (period_ns as f64 * loops as f64 / speed) as u64;
    let fps = ((spec.workload.fps as f64) * speed).round().max(1.0) as u32;
    let fire = event_schedule(&spec.events, &at);

    // The encoders.
    let cfg = production_config(spec, ow as u32, oh as u32, fps);
    let codec = if spec.recipe.production.is_some() {
        Codec::H264
    } else {
        spec.recipe.recipe.codec
    };
    let n_sess = spec.sessions.max(1) as usize;
    let mut sessions: Vec<Sess> = Vec::with_capacity(n_sess);
    let ltr_delay = match spec.ltr.as_ref().map(|l| l.ack.delay()).transpose() {
        Ok(d) => d,
        Err(e) => return setup_error(spec, e),
    };
    let mut ltr_txs: Vec<Option<crossbeam_channel::Sender<(usize, i64)>>> = Vec::new();
    for k in 0..n_sess {
        let create_t = Instant::now();
        match Encoder::new(cfg.clone()) {
            Ok(mut enc) => sessions.push(Sess {
                ltr: spec.ltr.as_ref().map(|_| {
                    let enable = json!(enc.set_property("EnableLTR", &Val::Bool(true)));
                    let (tx, rx) = crossbeam_channel::unbounded();
                    ltr_txs.push(Some(tx));
                    LtrState {
                        rx,
                        delay: ltr_delay.flatten(),
                        pending: Vec::new(),
                        seen: 0,
                        acked: Vec::new(),
                        frames_carrying_acks: 0,
                        enable,
                        set_failures: Vec::new(),
                    }
                }),
                enc,
                expected: Arc::new(AtomicUsize::new(usize::MAX)),
                collector: None,
                create_ms: create_t.elapsed().as_secs_f64() * 1e3,
                submit_at: vec![None; total],
                call_us: Vec::with_capacity(total),
                enc_status: vec![0; total],
                flush_fail: (0, None),
                submitted: 0,
                submitted_ok: 0,
                first_encode_err: None,
                max_in_flight: 0,
            }),
            Err(e) => {
                let (status, spec_report) = match e.downcast_ref::<SessionCreateFailed>() {
                    Some(f) => (Some(f.status), json!(f.spec)),
                    None => (None, Json::Null),
                };
                return json!({
                    "id": spec.id, "result": "create_failed", "valid": false,
                    "invalid_reasons": [format!("session {k} create failed")],
                    "session_index": k, "sessions_n": n_sess,
                    "session_create_status": status, "spec": spec_report, "error": format!("{e:#}"),
                    "encoder_size": [ow, oh],
                    "tuning": tune::report(),
                });
            }
        }
    }

    // Reader.
    let frame_bytes = ow * oh * 3 / 2;
    let depth = (256_000_000 / frame_bytes.max(1)).clamp(8, 64);
    let pool = match cf::Pool::new(ow, oh, depth + 8) {
        Ok(p) => p,
        Err(st) => return setup_error(spec, format!("CVPixelBufferPoolCreate failed: {st}")),
    };
    let (tx, rx) = bounded::<FrameIn>(depth);
    let reader = {
        let (path, info) = (path.clone(), info.clone());
        std::thread::Builder::new()
            .name("bench-reader".into())
            .spawn(move || -> Result<(), String> {
                crate::macos_utils::streamer::qos::pin_current_thread_user_initiated();
                let mut buf = Vec::new();
                for l in 0..loops {
                    let mut r = clip::ClipReader::open(&path, &info).map_err(|e| e.to_string())?;
                    for i in 0..n_src {
                        r.next(&mut buf)
                            .map_err(|e| format!("read frame {i}: {e}"))?;
                        let pb = pool.get().map_err(|st| format!("pool buffer: {st}"))?;
                        copy_into(&pb, &buf, &info, transform, ow, oh);
                        if tx
                            .send(FrameIn {
                                g: l * n_src + i,
                                pb,
                            })
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                }
                Ok(())
            })
            .expect("spawn reader")
    };

    // Collectors, one per encoder. Only session 0 keeps samples for the PSNR pass.
    for (k, s) in sessions.iter_mut().enumerate() {
        s.collector = Some(spawn_collector(
            s.enc.output(),
            Arc::clone(&s.expected),
            spec.psnr && k == 0,
            codec == Codec::Hevc,
            total,
            ltr_txs.get_mut(k).and_then(Option::take),
        ));
    }

    // Prefill the read-ahead before the clock starts.
    let pre = Instant::now();
    while rx.len() < depth.min(total)
        && !reader.is_finished()
        && pre.elapsed() < Duration::from_secs(60)
    {
        std::thread::sleep(Duration::from_millis(2));
    }

    // Submit loop.
    crate::macos_utils::streamer::qos::pin_current_thread_time_constraint(
        1_000_000_000 / fps as u64,
    );
    let t_start = Instant::now() + Duration::from_millis(20);
    let mut late_ns: Vec<u64> = Vec::with_capacity(total);
    // A late frame whose deadline passed while the previous `EncodeFrame` call was still running
    // was held up by the encoder (a blocking contract such as the non-low-latency encoder's), not
    // by this harness. Both are reported; only the harness's share can invalidate a run.
    let mut late_by_encoder = 0usize;
    let mut prev_call_end: Option<Instant> = None;
    let mut underruns = 0usize;
    let mut event_log: Vec<Json> = Vec::new();
    let mut reader_gap = false;
    let mut frames_offered = 0usize;
    let mut refresh_frames: Vec<usize> = Vec::new();
    for (g, &deadline_ns) in at.iter().enumerate() {
        let fr = match rx.try_recv() {
            Ok(f) => f,
            Err(_) => {
                underruns += 1;
                match rx.recv() {
                    Ok(f) => f,
                    Err(_) => {
                        reader_gap = true;
                        break;
                    }
                }
            }
        };
        debug_assert_eq!(fr.g, g);
        let deadline = t_start + Duration::from_nanos(deadline_ns);
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let left = deadline - now;
            if left > Duration::from_micros(2000) {
                std::thread::sleep(left - Duration::from_micros(1500));
            } else {
                std::hint::spin_loop();
            }
        }
        let late = Instant::now()
            .saturating_duration_since(deadline)
            .as_nanos() as u64;
        if late > LATE_NS && prev_call_end.is_some_and(|e| e > deadline) {
            late_by_encoder += 1;
        }
        late_ns.push(late);
        frames_offered += 1;

        let i = g % n_src;
        let mut force = g == 0 || (i != 0 && info.idr[i]);
        let mut ltr_refresh = false;
        if let Some(evs) = fire.get(&g) {
            for &ei in evs {
                let t_s = deadline_ns as f64 / 1e9;
                apply_event(
                    &mut sessions,
                    &spec.events[ei],
                    g,
                    t_s,
                    &mut force,
                    &mut ltr_refresh,
                    &mut event_log,
                );
            }
        }
        if ltr_refresh {
            refresh_frames.push(g);
        }

        for s in sessions.iter_mut() {
            // LTR: hand back the tokens that are due, and the refresh, on this frame only.
            let mut frame_keys: Vec<&str> = Vec::new();
            if let Some(l) = s.ltr.as_mut() {
                while let Ok(t) = l.rx.try_recv() {
                    l.seen += 1;
                    l.pending.push(t);
                }
                if let Some(delay) = l.delay {
                    let due: Vec<(usize, i64)> = l
                        .pending
                        .iter()
                        .copied()
                        .filter(|(tg, _)| *tg as u64 + delay <= g as u64)
                        .collect();
                    if !due.is_empty() {
                        l.pending.retain(|(tg, _)| *tg as u64 + delay > g as u64);
                        let list = Val::List(due.iter().map(|(_, t)| Val::Int(*t)).collect());
                        let r = s.enc.set_frame_opt("AcknowledgedLTRTokens", Some(list));
                        if !matches!(&r, Some(p) if p.outcome == crate::macos_utils::streamer::vt_recipe::Outcome::Included)
                            && l.set_failures.len() < 5
                        {
                            l.set_failures.push(json!({ "frame": g, "result": r }));
                        }
                        l.acked.extend(due);
                        l.frames_carrying_acks += 1;
                        frame_keys.push("AcknowledgedLTRTokens");
                    }
                }
                if ltr_refresh {
                    let r = s
                        .enc
                        .set_frame_opt("ForceLTRRefresh", Some(Val::Bool(true)));
                    if !matches!(&r, Some(p) if p.outcome == crate::macos_utils::streamer::vt_recipe::Outcome::Included)
                        && l.set_failures.len() < 5
                    {
                        l.set_failures.push(json!({ "frame": g, "result": r }));
                    }
                    frame_keys.push("ForceLTRRefresh");
                }
            }
            s.max_in_flight = s.max_in_flight.max(s.enc.frames_in_flight());
            let tag = FrameTag {
                capture: Instant::now(),
                hold_push: false,
                timing: FrameTiming {
                    // The bench's own tags: this field carries the source index, so a callback
                    // can be paired with its frame even when the encoder reorders.
                    capture_available_ns: g as u64,
                    ..FrameTiming::entering_capture()
                },
            };
            let t0 = tag.capture;
            let st = s.enc.submit_ex(&fr.pb, force, tag);
            let call_end = Instant::now();
            prev_call_end = Some(call_end);
            s.call_us
                .push(call_end.duration_since(t0).as_nanos() as f64 / 1e3);
            s.submit_at[g] = Some(t0);
            s.submitted += 1;
            s.enc_status[g] = st.encode;
            if st.encode == 0 {
                s.submitted_ok += 1;
            } else if s.first_encode_err.is_none() {
                s.first_encode_err = Some((g, st.encode));
            }
            if let Some(fs) = st.flush.filter(|x| *x != 0) {
                s.flush_fail.0 += 1;
                s.flush_fail.1.get_or_insert((g, fs));
            }
            for k in frame_keys {
                s.enc.set_frame_opt(k, None);
            }
        }
        drop(fr);
    }
    let submit_end = Instant::now();
    for s in sessions.iter_mut() {
        let st = s.enc.complete_frames_status();
        s.expected.store(s.submitted_ok, Ordering::Release);
        s.enc_status.push(st); // the final flush status rides at index `total`
    }
    let mut done: Vec<SessDone> = Vec::with_capacity(n_sess);
    for mut s in sessions {
        let col = s
            .collector
            .take()
            .map(|h| h.join().expect("collector"))
            .unwrap_or(Collected {
                obs: Vec::new(),
                params: None,
                kept: Vec::new(),
            });
        let flush_status = s.enc_status.pop().unwrap_or(0);
        let d = SessDone {
            output_drops: s.enc.output_drops(),
            identity: s.enc.identity().clone(),
            recipe_report: s.enc.recipe_report().cloned(),
            pending_after: s.enc.read_property("NumberOfPendingFrames").readback,
            low_latency: s.enc.low_latency(),
            backend: s.enc.backend_name(),
            ltr: s.ltr.take(),
            col,
            create_ms: s.create_ms,
            submit_at: s.submit_at,
            call_us: s.call_us,
            enc_status: s.enc_status,
            flush_fail: s.flush_fail,
            submitted: s.submitted,
            submitted_ok: s.submitted_ok,
            first_encode_err: s.first_encode_err,
            max_in_flight: s.max_in_flight,
            flush_status,
        };
        drop(s.enc);
        done.push(d);
    }
    let reader_result = reader
        .join()
        .unwrap_or_else(|_| Err("reader panicked".into()));

    // ---- metrics -------------------------------------------------------------------------
    let cx = Ctx {
        at: &at,
        total,
        n_src,
        loops,
        t_start,
        timeline_ns,
        codec,
    };
    let outs: Vec<SessOut> = done.iter().map(|d| session_metrics(&cx, d)).collect();
    let submitted = frames_offered;
    let late_count = late_ns.iter().filter(|&&l| l > LATE_NS).count();
    let late_harness = late_count - late_by_encoder;
    let late_ms: Vec<f64> = late_ns.iter().map(|&l| l as f64 / 1e6).collect();
    let secs = timeline_ns as f64 / 1e9;

    let mut reasons: Vec<String> = Vec::new();
    if late_harness as f64 > LATE_SHARE * submitted.max(1) as f64 {
        reasons.push(format!(
            "{late_harness} of {submitted} frames submitted > 2 ms late by the harness"
        ));
    }
    for (k, o) in outs.iter().enumerate() {
        for r in &o.reasons {
            reasons.push(if n_sess > 1 {
                format!("session {k}: {r}")
            } else {
                r.clone()
            });
        }
    }
    if let Err(e) = &reader_result {
        reasons.push(format!("reader: {e}"));
    }
    if reader_gap {
        reasons.push("reader ended early".into());
    }

    let (rss_end, rss_peak) = cf::rss();
    let mut out = json!({
        "id": spec.id,
        "result": "completed",
        "valid": reasons.is_empty(),
        "invalid_reasons": reasons,
        "stage": "lab",
        "encoder_size": [ow, oh],
        "fps": fps,
        "codec": codec,
        "frames_per_loop": n_src,
        "timeline_s": secs,
        "sessions_n": n_sess,
    });
    if let (Some(o), Json::Object(m)) = (out.as_object_mut(), outs[0].json.clone()) {
        o.extend(m);
    }
    let more = json!({
        "pacing": {
            "late_over_2ms": late_count,
            "late_over_2ms_encoder_blocked": late_by_encoder,
            "late_over_2ms_harness": late_harness,
            "late_ms": summary(&late_ms),
            "reader_underruns": underruns,
            "read_ahead_depth": depth,
            "submit_span_s": submit_end.saturating_duration_since(t_start).as_secs_f64(),
        },
        "events": event_log,
        "rss_mb": { "start": rss0 as f64 / 1e6, "end": rss_end as f64 / 1e6, "peak": rss_peak as f64 / 1e6 },
        "thermal_state": { "start": thermal0, "end": cf::thermal_state() },
        "tuning": tune::report(),
    });
    if let (Some(o), Json::Object(m)) = (out.as_object_mut(), more) {
        o.extend(m);
    }
    if n_sess > 1 {
        let mut lat: Vec<f64> = Vec::new();
        let mut rows: Vec<Row> = Vec::new();
        let mut ev: Vec<(u64, u64)> = Vec::new();
        for o in &outs {
            lat.extend_from_slice(&o.lat_ms);
            rows.extend_from_slice(&o.rows);
            ev.extend_from_slice(&o.out_events);
        }
        ev.sort_unstable();
        let sum = |k: &str| -> u64 { outs.iter().filter_map(|o| o.json[k].as_u64()).sum() };
        out["aggregate"] = json!({
            "sessions": n_sess,
            "submitted": sum("submitted"),
            "emitted": sum("emitted"),
            "dropped": sum("dropped"),
            "errored": sum("errored"),
            "missing_callbacks": sum("missing_callbacks"),
            "output_drops": sum("output_drops"),
            "latency_ms": summary(&lat),
            "avg_mbps": outs.iter().filter_map(|o| o.json["avg_mbps"].as_f64()).sum::<f64>(),
            "peak_1s_mbps": peak_rate_bps(&ev, 1_000_000_000) / 1e6,
            "buckets_10s": series::buckets(10, timeline_ns, &rows),
            "buckets_60s": series::buckets(60, timeline_ns, &rows),
        });
        out["per_session"] = Json::Array(
            outs.iter()
                .enumerate()
                .map(|(k, o)| {
                    let mut j = o.json.clone();
                    j["index"] = json!(k);
                    j
                })
                .collect(),
        );
    }

    if let Some(p) = spec.per_frame_path.as_deref() {
        let d = &done[0];
        let _ = write_per_frame(
            Path::new(p),
            &at,
            n_src,
            &d.submit_at,
            t_start,
            &late_ns,
            &d.call_us,
            &d.enc_status,
            &outs[0].cb_at,
        );
    }

    if spec.psnr {
        if done[0].output_drops > 0 {
            out["psnr"] = json!({ "error": "output-queue drops break the frame pairing" });
        } else {
            out["psnr"] = super::psnr::evaluate(
                &path,
                &info,
                n_src,
                loops,
                transform,
                &done[0].col.kept,
                &refresh_frames,
            );
        }
    }
    if let Some(l) = done[0].ltr.as_ref() {
        out["ltr"] = ltr_report(spec, &done[0], l, &refresh_frames, &at);
    }
    out["series"] = sampler.finish();
    out["wall_s"] = json!(wall.elapsed().as_secs_f64());
    out
}

/// Session 0's LTR record: what was enabled, what came back, what was acknowledged, and what
/// each forced refresh produced.
fn ltr_report(spec: &RunSpec, d: &SessDone, l: &LtrState, refresh: &[usize], at: &[u64]) -> Json {
    let tokens: Vec<i64> = d.col.obs.iter().filter_map(|o| o.ltr_token).collect();
    let mut distinct = tokens.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let frame = |g: usize| d.col.obs.iter().find(|o| o.g == g);
    let refreshes: Vec<Json> = refresh
        .iter()
        .map(|&g| match frame(g) {
            Some(o) => json!({
                "frame": g, "t_s": at.get(g).map(|t| *t as f64 / 1e9),
                "status": o.status, "bytes": o.bytes, "sync": o.key,
                "token": o.ltr_token, "attachments": o.attach,
                "next_frames_bytes": (g + 1..g + 6).map(|h| frame(h).map(|x| x.bytes)).collect::<Vec<_>>(),
            }),
            None => json!({ "frame": g, "error": "no callback" }),
        })
        .collect();
    let first_token_frame = d
        .col
        .obs
        .iter()
        .find(|o| o.ltr_token.is_some())
        .map(|o| o.g);
    json!({
        "ack": spec.ltr.as_ref().map(|x| json!(x.ack)),
        "enable_ltr": l.enable,
        "tokens_seen": tokens.len(),
        "distinct_tokens": distinct.len(),
        "first_token_frame": first_token_frame,
        "first_tokens": distinct.iter().take(8).collect::<Vec<_>>(),
        "tokens_received_by_submit": l.seen,
        "tokens_acked": l.acked.len(),
        "tokens_pending_at_end": l.pending.len(),
        "frames_carrying_acks": l.frames_carrying_acks,
        "frame_option_failures": l.set_failures,
        "refreshes": refreshes,
        "keyframes": d.col.obs.iter().filter(|o| o.key && o.picture).count(),
        "sample_attachments_first_token_frame": first_token_frame.and_then(frame).and_then(|o| o.attach.clone()),
    })
}

#[allow(clippy::too_many_arguments)]
fn write_per_frame(
    path: &Path,
    at: &[u64],
    n_src: usize,
    submit_at: &[Option<Instant>],
    t_start: Instant,
    late_ns: &[u64],
    call_us: &[f64],
    enc_status: &[i32],
    cb: &[CbRow],
) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(f, "frame,loop,src,due_ms,submit_ms,late_us,call_us,encode_status,callback_ms,cb_status,info_flags,bytes,keyframe,latency_ms")?;
    for g in 0..at.len() {
        let submit =
            submit_at[g].map(|s| s.saturating_duration_since(t_start).as_nanos() as f64 / 1e6);
        let (cbms, cst, inf, b, k) = match cb[g] {
            Some((t, s, i, b, k)) => (
                format!("{:.3}", t as f64 / 1e6),
                s.to_string(),
                i.to_string(),
                b.to_string(),
                (k as u8).to_string(),
            ),
            None => Default::default(),
        };
        let lat = match (cb[g], submit) {
            (Some((t, 0, _, b, _)), Some(s)) if b > 0 => format!("{:.3}", t as f64 / 1e6 - s),
            _ => String::new(),
        };
        writeln!(
            f,
            "{g},{},{},{:.3},{},{},{},{},{cbms},{cst},{inf},{b},{k},{lat}",
            g / n_src,
            g % n_src,
            at[g] as f64 / 1e6,
            submit.map(|s| format!("{s:.3}")).unwrap_or_default(),
            late_ns
                .get(g)
                .map(|l| (l / 1000).to_string())
                .unwrap_or_default(),
            call_us
                .get(g)
                .map(|c| format!("{c:.1}"))
                .unwrap_or_default(),
            enc_status[g],
        )?;
    }
    f.flush()
}

//! The `pipe` stage: a clip through the REAL production encode path.
//!
//! This stage plays the capture backend: frames are published into the production latest-frame
//! slot (`frame::frame_channel`) on the clip's schedule and `pipeline::spawn_pipeline` does the
//! rest as it does for a live session. A subscriber packetizes each `EncodedFrame` at the LAN MTU
//! and folds it into the session telemetry with `record_frame`. There is no network, so no socket
//! stage; `capture_available_ns` is the moment this stage offered the frame.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossbeam_channel::bounded;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetIOSurface};
use serde_json::{json, Value as Json};
use tokio::sync::broadcast::error::RecvError;

use crate::macos_utils::streamer::frame::{frame_channel, Backing, Frame, HeldSurface};
use crate::macos_utils::streamer::pipeline::spawn_pipeline;
use crate::macos_utils::streamer::{activity, tune, tuning};
use crate::streamer::packetizer::{access_unit_has_sps, H264Packetizer};
use crate::streamer::telemetry::{FrameTiming, SessionTelemetry};
use crate::streamer::webrtc_session::RTP_PAYLOAD_MTU_LAN;

use super::cf;
use super::clip::{self, Transform};
use super::metrics::{h264_sps_info, peak_rate_bps, summary};
use super::plan::RunSpec;
use super::replay::{copy_into, event_schedule, fault, production_config, setup_error};
use super::series::{self, Row, Sampler};

/// Harness lateness above which a run is not a measurement (the lab's rule).
const LATE_NS: u64 = 2_000_000;
const LATE_SHARE: f64 = 0.01;

struct FrameIn {
    pb: CFRetained<CVPixelBuffer>,
    /// Identical to the previous frame and `skip_unchanged` is on: not offered.
    unchanged: bool,
}
unsafe impl Send for FrameIn {}

/// What the packetizer saw for one broadcast frame.
struct OutRec {
    timing: FrameTiming,
    /// `EncodedFrame.capture` on the host clock: what the RTP timestamp is made from.
    capture_ns: u64,
    au_bytes: usize,
    packets: usize,
    payload_bytes: usize,
    keyframe: bool,
    params: Option<Vec<Vec<u8>>>,
}

fn span_ms(a: u64, b: u64) -> Option<f64> {
    (a != 0 && b != 0 && b >= a).then(|| (b - a) as f64 / 1e6)
}

/// SPS and PPS out of an Annex B access unit (the first keyframe's).
fn annexb_params(au: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(au.len(), |&n| n - 3);
        while e > s && au[e - 1] == 0 {
            e -= 1;
        }
        if s < e && matches!(au[s] & 0x1f, 7 | 8) {
            out.push(au[s..e].to_vec());
        }
    }
    out
}

/// `SE_BENCH_CLIENT_FRAME_MS`: the modelled client's per-frame cost (default 1 ms: a fast client).
fn client_frame_ms() -> f64 {
    std::env::var("SE_BENCH_CLIENT_FRAME_MS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(1.0)
}

/// A modelled client, so the hold push's client guard has STATS to read in a stage with no
/// browser: each packetized frame is "painted" FIFO at a fixed per-frame cost, and once a second
/// a STATS frame (arrival->paint p50 over the last 512 frames, painted count) goes into the
/// session telemetry where the real parser puts it. A client slower than twice the frame rate
/// queues under hold push, as a real one does.
struct SyntheticClient {
    /// `SE_BENCH_CLIENT_FRAME_MS=off|tier0|p50zero`: no STATS at all, STATS from a page that cannot
    /// measure paint, and STATS whose p50 is 0. All three must keep hold push out of immediate mode.
    off: bool,
    tier0: bool,
    p50zero: bool,
    frame_ns: u64,
    last_paint_ns: u64,
    window: std::collections::VecDeque<u64>,
    painted: u64,
    last_report_ns: u64,
}

impl SyntheticClient {
    fn new(frame_ms: f64) -> Self {
        let kind = |k: &str| std::env::var("SE_BENCH_CLIENT_FRAME_MS").as_deref() == Ok(k);
        SyntheticClient {
            off: kind("off"),
            tier0: kind("tier0"),
            p50zero: kind("p50zero"),
            frame_ns: (frame_ms * 1e6) as u64,
            last_paint_ns: 0,
            window: std::collections::VecDeque::with_capacity(512),
            painted: 0,
            last_report_ns: 0,
        }
    }

    fn frame(&mut self, telemetry: &SessionTelemetry, arrival_ns: u64) {
        if arrival_ns == 0 || self.off {
            return;
        }
        let paint = arrival_ns.max(self.last_paint_ns) + self.frame_ns;
        self.last_paint_ns = paint;
        if self.window.len() >= 512 {
            self.window.pop_front();
        }
        self.window.push_back(paint - arrival_ns);
        self.painted += 1;
        if arrival_ns.saturating_sub(self.last_report_ns) >= 1_000_000_000 {
            self.last_report_ns = arrival_ns;
            let mut v: Vec<u64> = self.window.iter().copied().collect();
            v.sort_unstable();
            let p50 = v[v.len() / 2];
            telemetry.set_client_stats(crate::streamer::telemetry::ClientStats {
                received_host_ns: crate::streamer::audio::host_now_ns(),
                version: 3,
                // a client that paints through WebCodecs: without these the host takes the frame
                // for a page that cannot measure paint, and hold push never goes immediate
                video_path: 1,
                transport_tier: u8::from(!self.tier0),
                arrival_to_paint_p50_us: if self.p50zero {
                    0
                } else {
                    (p50 / 1_000) as u32
                },
                frames_painted: self.painted as u32,
                ..Default::default()
            });
        }
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

    // what `Streamer::prepare` and `start_on_monitor` do around a live session
    tuning::apply_process_tuning();
    let activity = activity::begin_if_enabled();

    // reader: clip records into IOSurface-backed pool buffers, ahead of the clock
    let frame_bytes = ow * oh * 3 / 2;
    let depth = (256_000_000 / frame_bytes.max(1)).clamp(8, 64);
    // the encode loop holds the last frame for the keepalive and VideoToolbox holds up to the
    // stall bypass's bound, so the pool must outlast the read-ahead by more than that
    let pool = match cf::Pool::new(ow, oh, depth + 24) {
        Ok(p) => p,
        Err(st) => return setup_error(spec, format!("CVPixelBufferPoolCreate failed: {st}")),
    };
    let (ftx, frx) = bounded::<FrameIn>(depth);
    let skip_unchanged = spec.workload.skip_unchanged;
    let reader = {
        let (path, info) = (path.clone(), info.clone());
        std::thread::Builder::new()
            .name("bench-reader".into())
            .spawn(move || -> Result<(), String> {
                crate::macos_utils::streamer::qos::pin_current_thread_user_initiated();
                let mut buf = Vec::new();
                let mut prev: Vec<u8> = Vec::new();
                for _ in 0..loops {
                    let mut r = clip::ClipReader::open(&path, &info).map_err(|e| e.to_string())?;
                    for i in 0..n_src {
                        r.next(&mut buf)
                            .map_err(|e| format!("read frame {i}: {e}"))?;
                        let unchanged = skip_unchanged && !prev.is_empty() && prev == buf;
                        if skip_unchanged && !unchanged {
                            prev.clone_from(&buf);
                        }
                        let pb = pool.get().map_err(|st| format!("pool buffer: {st}"))?;
                        copy_into(&pb, &buf, &info, transform, ow, oh);
                        if ftx.send(FrameIn { pb, unchanged }).is_err() {
                            return Ok(());
                        }
                    }
                }
                Ok(())
            })
            .expect("spawn reader")
    };

    let cfg = production_config(spec, ow as u32, oh as u32, fps);
    let telemetry = SessionTelemetry::new_shared();
    telemetry.encoder.set("videotoolbox".into());
    telemetry.capture_backend.set("bench-pipe".into());
    let (sink, source) = frame_channel();
    let create_t = Instant::now();
    let (pipeline, stop, encode_thread) = match spawn_pipeline(cfg, source, Arc::clone(&telemetry))
    {
        Ok(p) => p,
        Err(e) => {
            drop(frx);
            let _ = reader.join();
            return json!({
                "id": spec.id, "result": "create_failed", "stage": "pipe", "valid": false,
                "invalid_reasons": ["spawn_pipeline failed"], "error": format!("{e:#}"),
                "encoder_size": [ow, oh], "tuning": tune::report(),
            });
        }
    };
    let create_ms = create_t.elapsed().as_secs_f64() * 1e3;

    let mut brx = pipeline.tx.subscribe();
    let client_frame_ms = client_frame_ms();
    let packetizer = {
        let telemetry = Arc::clone(&telemetry);
        std::thread::Builder::new()
            .name("bench-packetize".into())
            .spawn(move || {
                // the writer is a tokio worker in production: give this thread its tuning
                crate::streamer::platform::tune_transport_thread();
                let mut pk = H264Packetizer::new();
                let mut payloads: Vec<Bytes> = Vec::with_capacity(64);
                let mut recs: Vec<OutRec> = Vec::with_capacity(total + 64);
                let mut lagged = 0u64;
                let mut first_params: Option<Vec<Vec<u8>>> = None;
                let mut client = SyntheticClient::new(client_frame_ms);
                loop {
                    match brx.blocking_recv() {
                        Ok(frame) => {
                            let mut timing = frame.timing;
                            pk.packetize(RTP_PAYLOAD_MTU_LAN, &frame.data, &mut payloads);
                            timing.mark_packetize_done();
                            let packets = payloads.len();
                            let payload_bytes = payloads.iter().map(Bytes::len).sum();
                            payloads.clear();
                            let keyframe = access_unit_has_sps(&frame.data);
                            let params = if keyframe && first_params.is_none() {
                                first_params = Some(annexb_params(&frame.data));
                                first_params.clone()
                            } else {
                                None
                            };
                            telemetry
                                .bytes_sent
                                .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                            telemetry.record_frame(&timing);
                            client.frame(&telemetry, timing.packetize_done_ns);
                            recs.push(OutRec {
                                timing,
                                capture_ns: crate::streamer::audio::host_instant_to_ns(
                                    frame.capture,
                                ),
                                au_bytes: frame.data.len(),
                                packets,
                                payload_bytes,
                                keyframe,
                                params,
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
                (recs, lagged, pk.dropped_parameter_sets())
            })
            .expect("spawn packetizer")
    };

    // prefill the read-ahead before the clock starts
    let pre = Instant::now();
    while frx.len() < depth.min(total)
        && !reader.is_finished()
        && pre.elapsed() < Duration::from_secs(60)
    {
        std::thread::sleep(Duration::from_millis(2));
    }

    // this thread plays the capture callback on the clip's timing, so it is a deadline thread:
    // at USER_INTERACTIVE QoS alone (low-power mode) 413 of 600 offers were > 2 ms late, p50 3.4 ms
    crate::macos_utils::streamer::qos::pin_current_thread_time_constraint(
        1_000_000_000 / fps as u64,
    );
    let t_start = Instant::now() + Duration::from_millis(20);
    let t_start_ns = crate::streamer::audio::host_instant_to_ns(t_start);
    let mut offered_ns: Vec<u64> = vec![0; total];
    let mut late_ns: Vec<u64> = Vec::with_capacity(total);
    let mut underruns = 0usize;
    let mut reader_gap = false;
    let mut event_log: Vec<Json> = Vec::new();
    let mut idr_requests = 0usize;
    let mut offered = 0usize;
    let mut surface_missing = 0usize;
    let mut skipped_unchanged = 0usize;
    for (g, &deadline_ns) in at.iter().enumerate() {
        let fr = match frx.try_recv() {
            Ok(f) => f,
            Err(_) => {
                underruns += 1;
                match frx.recv() {
                    Ok(f) => f,
                    Err(_) => {
                        reader_gap = true;
                        break;
                    }
                }
            }
        };
        let deadline = t_start + Duration::from_nanos(deadline_ns);
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let left = deadline - now;
            if left > Duration::from_micros(2000) {
                // in slices: one long sleep (a 10 fps clip's ~98 ms) overslept by ~7 ms a frame,
                // because timer slack grows with the interval
                std::thread::sleep(
                    (left - Duration::from_micros(1500)).min(Duration::from_millis(10)),
                );
            } else {
                std::hint::spin_loop();
            }
        }
        let i = g % n_src;
        if g != 0 && i != 0 && info.idr[i] {
            // the recording's own keyframes were client requests: replay them as requests
            pipeline.request_idr();
            idr_requests += 1;
        }
        if let Some(evs) = fire.get(&g) {
            for &ei in evs {
                let ev = &spec.events[ei];
                let t_s = deadline_ns as f64 / 1e9;
                if ev.action == "force_idr" {
                    pipeline.request_idr();
                    idr_requests += 1;
                    event_log.push(json!({ "frame": g, "t_s": t_s, "action": "force_idr" }));
                } else {
                    event_log.push(json!({
                        "frame": g, "t_s": t_s, "action": ev.action,
                        "error": "the pipe stage takes force_idr only (the encoder belongs to the encode thread)",
                    }));
                }
            }
        }
        if fr.unchanged {
            // a capture backend delivers nothing for an unchanged screen
            skipped_unchanged += 1;
            continue;
        }
        let Some(surface) = (unsafe { CVPixelBufferGetIOSurface(Some(&fr.pb)) }) else {
            surface_missing += 1;
            continue;
        };
        let now = Instant::now();
        late_ns.push(now.saturating_duration_since(deadline).as_nanos() as u64);
        let ns = crate::streamer::audio::host_now_ns();
        offered_ns[g] = ns;
        sink.publish(Frame {
            width: ow,
            height: oh,
            arrived_mach: g as u64,
            captured_at: now,
            os_time_ns: ns,
            os_time_raw_ns: ns,
            backing: Backing::PixelBuffer {
                pixbuf: fr.pb,
                _surface: HeldSurface::hold(surface),
            },
        });
        offered += 1;
    }
    let offer_end = Instant::now();

    // let what is in flight come out, then stop the loop the way `SessionCapture` does
    let settle = Instant::now();
    loop {
        let sub = telemetry.frames_submitted.load(Ordering::Relaxed);
        let enc = telemetry.frames_encoded.load(Ordering::Relaxed);
        let rd = telemetry
            .degradation
            .encoder_rate_drops
            .load(Ordering::Relaxed);
        if (enc + rd >= sub && settle.elapsed() > Duration::from_millis(50))
            || settle.elapsed() > Duration::from_millis(1500)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = encode_thread.join();
    drop(sink);
    drop(pipeline);
    let (recs, lagged, dropped_params) = packetizer.join().unwrap_or_default();
    let reader_result = reader
        .join()
        .unwrap_or_else(|_| Err("reader panicked".into()));
    drop(activity);

    let by_offer: HashMap<u64, usize> = offered_ns
        .iter()
        .enumerate()
        .filter(|(_, &ns)| ns != 0)
        .map(|(g, &ns)| (ns, g))
        .collect();
    let deg = |name: &str| -> u64 {
        telemetry
            .degradation
            .each()
            .into_iter()
            .find(|(k, _)| *k == name)
            .map_or(0, |(_, v)| v)
    };
    let submitted = telemetry.frames_submitted.load(Ordering::Relaxed);
    let encoded = telemetry.frames_encoded.load(Ordering::Relaxed);
    let repeated = deg("frames_repeated");
    let placeholder = deg("capture_no_first_frame");
    let rate_drops = deg("encoder_rate_drops");
    let fresh_submitted = submitted.saturating_sub(repeated + placeholder);
    let coalesced = (offered as u64).saturating_sub(fresh_submitted);

    let (mut take, mut take_submit, mut offer_submit, mut submit_enc, mut enc_pk, mut e2e) = (
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let (mut rep_submit_enc, mut rep_enc_pk) = (Vec::new(), Vec::new());
    let (mut packets, mut au_bytes, mut key_bytes) = (Vec::new(), Vec::new(), Vec::new());
    let mut out_events: Vec<(u64, u64)> = Vec::new();
    let mut rows: Vec<Row> = Vec::with_capacity(total);
    let mut got = vec![false; total];
    let mut unknown = 0usize;
    let mut total_bytes = 0u64;
    let mut payload_total = 0u64;
    let mut params: Option<Vec<Vec<u8>>> = None;
    for r in &recs {
        let t = &r.timing;
        packets.push(r.packets as f64);
        au_bytes.push(r.au_bytes as f64);
        total_bytes += r.au_bytes as u64;
        payload_total += r.payload_bytes as u64;
        if r.keyframe {
            key_bytes.push(r.au_bytes as f64);
        }
        if params.is_none() {
            params = r.params.clone();
        }
        out_events.push((
            t.packetize_done_ns.saturating_sub(t_start_ns),
            r.au_bytes as u64,
        ));
        if t.repeated {
            rep_submit_enc.extend(span_ms(t.encode_submit_ns, t.encode_done_ns));
            rep_enc_pk.extend(span_ms(t.encode_done_ns, t.packetize_done_ns));
            continue;
        }
        let Some(&g) = by_offer.get(&t.capture_available_ns) else {
            unknown += 1;
            continue;
        };
        got[g] = true;
        take.extend(span_ms(t.capture_available_ns, t.capture_cb_entry_ns));
        take_submit.extend(span_ms(t.capture_cb_entry_ns, t.encode_submit_ns));
        offer_submit.extend(span_ms(t.capture_available_ns, t.encode_submit_ns));
        submit_enc.extend(span_ms(t.encode_submit_ns, t.encode_done_ns));
        enc_pk.extend(span_ms(t.encode_done_ns, t.packetize_done_ns));
        let l = span_ms(t.capture_available_ns, t.packetize_done_ns);
        e2e.extend(l);
        rows.push(Row {
            t_ns: at[g],
            lat_ms: l,
            bytes: r.au_bytes as u64,
            ..Default::default()
        });
    }
    // offered frames that never came out as a picture: coalesced in the slot, or rate-dropped
    for g in 0..total {
        if offered_ns[g] != 0 && !got[g] {
            rows.push(Row {
                t_ns: at[g],
                dropped: true,
                ..Default::default()
            });
        }
    }
    out_events.sort_unstable();
    // the RTP timestamp is the host clock at `EncodedFrame.capture`, so `packetized - capture` is
    // the host side of "host capture -> paint", per kind of frame; a non-increasing RTP timestamp
    // in output order is what the writer's clamp would fix
    let (mut stamp_real, mut stamp_rep) = (Vec::new(), Vec::new());
    let mut rtp_would_clamp = 0usize;
    let mut last_rtp: Option<u32> = None;
    for r in &recs {
        let lag = span_ms(r.capture_ns, r.timing.packetize_done_ns);
        if r.timing.repeated {
            stamp_rep.extend(lag);
        } else {
            stamp_real.extend(lag);
        }
        let ts = crate::streamer::audio::host_ns_to_rtp90k(r.capture_ns);
        if last_rtp.is_some_and(|l| (ts.wrapping_sub(l) as i32) <= 0) {
            rtp_would_clamp += 1;
        }
        last_rtp = Some(ts);
    }
    // repeats submitted after a real frame and before the next, within 190 ms of it (the keepalive
    // comes at 200 ms), are that frame's hold pushes; more than two would be a push of a push or
    // a third push, which the hold-push state machine must never do
    let mut by_submit: Vec<(u64, bool)> = recs
        .iter()
        .map(|r| (r.timing.encode_submit_ns, r.timing.repeated))
        .collect();
    by_submit.sort_unstable();
    let mut repeats_after_real: Vec<f64> = Vec::new();
    let mut last_real: Option<(u64, u32)> = None;
    for &(ns, repeated) in &by_submit {
        if !repeated {
            if let Some((_, n)) = last_real.take() {
                repeats_after_real.push(n as f64);
            }
            last_real = Some((ns, 0));
        } else if let Some((real_ns, n)) = last_real.as_mut() {
            if ns.saturating_sub(*real_ns) < 190_000_000 {
                *n += 1;
            }
        }
    }
    if let Some((_, n)) = last_real {
        repeats_after_real.push(n as f64);
    }
    let late_count = late_ns.iter().filter(|&&l| l > LATE_NS).count();
    let late_ms: Vec<f64> = late_ns.iter().map(|&l| l as f64 / 1e6).collect();
    let secs = timeline_ns as f64 / 1e9;

    let mut reasons: Vec<String> = Vec::new();
    if late_count as f64 > LATE_SHARE * offered.max(1) as f64 {
        reasons.push(format!(
            "{late_count} of {offered} frames offered > 2 ms late by the harness"
        ));
    }
    if offered + skipped_unchanged < total {
        reasons.push(format!("only {offered} of {total} frames offered"));
    }
    if surface_missing > 0 {
        reasons.push(format!("{surface_missing} pool buffers had no IOSurface"));
    }
    if lagged > 0 {
        reasons.push(format!(
            "the packetizer lagged the broadcast by {lagged} frame(s)"
        ));
    }
    if unknown > 0 {
        reasons.push(format!("{unknown} outputs matched no offered frame"));
    }
    if let Err(e) = &reader_result {
        reasons.push(format!("reader: {e}"));
    }
    if reader_gap {
        reasons.push("reader ended early".into());
    }
    let fresh_out = got.iter().filter(|&&x| x).count();
    let sps = match &params {
        Some(p) if !p.is_empty() => h264_sps_info(p),
        _ => json!({ "error": "no keyframe emitted" }),
    };
    let (rss_end, rss_peak) = cf::rss();
    json!({
        "id": spec.id,
        "result": "completed",
        "stage": "pipe",
        "valid": reasons.is_empty(),
        "invalid_reasons": reasons,
        "encoder_size": [ow, oh],
        "fps": fps,
        "frames_per_loop": n_src,
        "timeline_s": secs,
        "pipeline_create_ms": create_ms,
        "encoder_backend": telemetry.encoder.get(),
        "hold_push_mode": telemetry.hold_push_mode.get(),
        "synthetic_client_frame_ms": client_frame_ms,
        "frames": {
            "offered": offered,
            "submitted": submitted,
            "submitted_fresh": fresh_submitted,
            "repeated": repeated,
            "placeholder": placeholder,
            "coalesced": coalesced,
            "encoded": encoded,
            "rate_drops": rate_drops,
            "output_queue_drop_events": deg("encoder_output_drops"),
            "broadcast_lagged": lagged,
            "packetized": recs.len(),
            "packetized_fresh": fresh_out,
            "idrs_sent": deg("idrs_sent"),
            "skipped_unchanged": skipped_unchanged,
            "hold_pushed": deg("frames_hold_pushed"),
            "rtp_ts_would_clamp": rtp_would_clamp,
            "hold_push_bytes": telemetry.hold_push_bytes.load(Ordering::Relaxed),
            "repeats_after_real_max": repeats_after_real.iter().copied().fold(0.0, f64::max),
            "repeats_after_real_over_2": repeats_after_real.iter().filter(|&&n| n > 2.0).count(),
            "idr_requests": idr_requests,
            "transient_errors": deg("encoder_transient_errors"),
            "encoder_resets": deg("encoder_resets"),
        },
        // the lab's names, so the runner's one-line summary reads the same for both stages
        "submitted": submitted,
        "dropped": rate_drops,
        "emitted": recs.len(),
        "latency_ms": summary(&e2e),
        "stages_ms": {
            "offer_to_take": summary(&take),
            "take_to_submit": summary(&take_submit),
            "offer_to_submit": summary(&offer_submit),
            "submit_to_encoded": summary(&submit_enc),
            "encoded_to_packetized": summary(&enc_pk),
            "offer_to_packetized": summary(&e2e),
            "repeated_submit_to_encoded": summary(&rep_submit_enc),
            "repeated_encoded_to_packetized": summary(&rep_enc_pk),
            "rtp_capture_to_packetized_real": summary(&stamp_real),
            "rtp_capture_to_packetized_repeat": summary(&stamp_rep),
        },
        "buckets_10s": series::buckets(10, timeline_ns, &rows),
        "buckets_60s": series::buckets(60, timeline_ns, &rows),
        "packets_per_frame": summary(&packets),
        "frame_bytes": summary(&au_bytes),
        "keyframes": key_bytes.len(),
        "keyframe_bytes": summary(&key_bytes),
        "payload_bytes_total": payload_total,
        "packetizer_dropped_parameter_sets": dropped_params,
        "rtp_payload_mtu": RTP_PAYLOAD_MTU_LAN,
        "avg_mbps": total_bytes as f64 * 8.0 / secs.max(1e-9) / 1e6,
        "peak_1s_mbps": peak_rate_bps(&out_events, 1_000_000_000) / 1e6,
        "peak_100ms_mbps": peak_rate_bps(&out_events, 100_000_000) / 1e6,
        "sps": sps,
        "telemetry": telemetry.to_json(),
        "pacing": {
            "late_over_2ms": late_count,
            "late_ms": summary(&late_ms),
            "reader_underruns": underruns,
            "read_ahead_depth": depth,
            "offer_span_s": offer_end.saturating_duration_since(t_start).as_secs_f64(),
        },
        "events": event_log,
        "rss_mb": { "start": rss0 as f64 / 1e6, "end": rss_end as f64 / 1e6, "peak": rss_peak as f64 / 1e6 },
        "thermal_state": { "start": thermal0, "end": cf::thermal_state() },
        "tuning": tune::report(),
        "series": sampler.finish(),
        "wall_s": wall.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_sets_come_out_of_an_annex_b_keyframe() {
        let au = [
            0, 0, 0, 1, 0x67, 100, 0, 42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88, 0x84,
        ];
        let p = annexb_params(&au);
        assert_eq!(p, vec![vec![0x67, 100, 0, 42], vec![0x68, 0xce]]);
    }
}

//! The `churn` child: create a session, encode K frames, destroy it, M times. It answers whether
//! a session lifecycle leaks (memory, descriptors, mach ports, threads) and how long a create
//! takes when the hardware encoder is handed back and asked for again, which is what a
//! renegotiation or an encoder reset does in production.

use std::time::{Duration, Instant};

use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use serde_json::{json, Value as Json};

use crate::macos_utils::streamer::encoder::{Encoder, FrameTag, SessionCreateFailed};
use crate::macos_utils::streamer::tune;
use crate::streamer::telemetry::FrameTiming;

use super::cf;
use super::metrics::summary;
use super::plan::{Churn, RunSpec};
use super::replay::{fault, production_config, setup_error};
use super::series::Sampler;

/// A moving diagonal gradient, so every frame differs and the encoder does real work.
fn paint(pb: &CVPixelBuffer, w: usize, h: usize, tick: usize) {
    unsafe {
        CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(0));
        let y = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *mut u8;
        let ybpr = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
        let c = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *mut u8;
        let cbpr = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
        if !y.is_null() && !c.is_null() {
            for row in 0..h {
                let r = std::slice::from_raw_parts_mut(y.add(row * ybpr), w);
                for (x, p) in r.iter_mut().enumerate() {
                    *p = (x + row + tick * 7) as u8;
                }
            }
            for row in 0..h / 2 {
                std::ptr::write_bytes(c.add(row * cbpr), 128, w);
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags(0));
    }
}

pub fn run(spec: &RunSpec) -> Json {
    if let Some(f) = spec.recipe.test_fault.as_deref() {
        fault(f);
    }
    let wall = Instant::now();
    let sampler = Sampler::start();
    let c = spec.churn.clone().unwrap_or(Churn {
        cycles: 50,
        frames: 30,
    });
    let (w, h, fps) = (
        spec.workload.width,
        spec.workload.height,
        spec.workload.fps.max(1),
    );
    let cfg = production_config(spec, w as u32, h as u32, fps);
    let pool = match cf::Pool::new(w, h, 8) {
        Ok(p) => p,
        Err(st) => return setup_error(spec, format!("CVPixelBufferPoolCreate failed: {st}")),
    };
    let period = Duration::from_nanos(1_000_000_000 / fps as u64);
    let res_start = cf::resources();
    let mut res_samples: Vec<Json> = vec![json!({ "cycle": 0, "at": "start", "res": res_start })];
    let (mut create_ms, mut destroy_ms) = (Vec::<f64>::new(), Vec::<f64>::new());
    let mut create_failures: Vec<Json> = Vec::new();
    let mut encode_failures: Vec<Json> = Vec::new();
    let mut flush_failures: Vec<Json> = Vec::new();
    let mut callback_errors: Vec<Json> = Vec::new();
    let (mut emitted, mut dropped, mut missing) = (0u64, 0u64, 0u64);
    let mut identities: std::collections::BTreeMap<String, u32> = Default::default();
    let mut tick = 0usize;
    for cycle in 0..c.cycles as usize {
        let t = Instant::now();
        let mut enc = match Encoder::new(cfg.clone()) {
            Ok(e) => e,
            Err(e) => {
                let status = e.downcast_ref::<SessionCreateFailed>().map(|f| f.status);
                create_failures
                    .push(json!({ "cycle": cycle, "status": status, "error": format!("{e:#}") }));
                continue;
            }
        };
        create_ms.push(t.elapsed().as_secs_f64() * 1e3);
        *identities
            .entry(format!(
                "{} hw={:?}",
                enc.identity().encoder_id.as_deref().unwrap_or("?"),
                enc.identity().hardware
            ))
            .or_default() += 1;
        let out = enc.output();
        let mut ok = 0u64;
        let mut got = 0u64;
        let start = Instant::now();
        for f in 0..c.frames as usize {
            let due = start + period * f as u32;
            if let Some(left) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(left);
            }
            let pb = match pool.get() {
                Ok(p) => p,
                Err(st) => return setup_error(spec, format!("pool buffer: {st}")),
            };
            paint(&pb, w, h, tick);
            tick += 1;
            let tag = FrameTag {
                capture: Instant::now(),
                hold_push: false,
                timing: FrameTiming::entering_capture(),
            };
            let st = enc.submit_ex(&pb, f == 0, tag);
            if st.encode == 0 {
                ok += 1;
            } else if encode_failures.len() < 20 {
                encode_failures.push(json!({ "cycle": cycle, "frame": f, "status": st.encode }));
            }
            while let Ok(cs) = out.try_recv() {
                got += 1;
                if cs.status != 0 {
                    callback_errors.push(json!({ "cycle": cycle, "status": cs.status }));
                } else if cs.is_rate_drop() {
                    dropped += 1;
                } else {
                    emitted += 1;
                }
            }
        }
        let fs = enc.complete_frames_status();
        if fs != 0 {
            flush_failures.push(json!({ "cycle": cycle, "status": fs }));
        }
        let settle = Instant::now();
        while got < ok && settle.elapsed() < Duration::from_secs(2) {
            match out.recv_timeout(Duration::from_millis(20)) {
                Ok(cs) => {
                    got += 1;
                    if cs.status != 0 {
                        callback_errors.push(json!({ "cycle": cycle, "status": cs.status }));
                    } else if cs.is_rate_drop() {
                        dropped += 1;
                    } else {
                        emitted += 1;
                    }
                }
                Err(_) => continue,
            }
        }
        missing += ok.saturating_sub(got);
        let t = Instant::now();
        drop(out);
        drop(enc);
        destroy_ms.push(t.elapsed().as_secs_f64() * 1e3);
        if (cycle + 1) % 10 == 0 {
            res_samples.push(json!({ "cycle": cycle + 1, "res": cf::resources() }));
        }
    }
    // let VideoToolbox's own teardown finish before the closing count
    std::thread::sleep(Duration::from_millis(500));
    let res_end = cf::resources();
    res_samples.push(json!({ "cycle": c.cycles, "at": "end+500ms", "res": res_end }));
    let delta = |k: &str| -> Json {
        match (res_start[k].as_f64(), res_end[k].as_f64()) {
            (Some(a), Some(b)) => json!(((b - a) * 10.0).round() / 10.0),
            _ => Json::Null,
        }
    };
    let n_cb_err = callback_errors.len();
    callback_errors.truncate(20);
    let mut reasons: Vec<String> = Vec::new();
    if !create_failures.is_empty() {
        reasons.push(format!(
            "{} of {} creates failed",
            create_failures.len(),
            c.cycles
        ));
    }
    json!({
        "id": spec.id,
        "result": "completed",
        "stage": "churn",
        "valid": reasons.is_empty(),
        "invalid_reasons": reasons,
        "encoder_size": [w, h],
        "fps": fps,
        "cycles": c.cycles,
        "frames_per_cycle": c.frames,
        "creates_ok": create_ms.len(),
        "create_ms": summary(&create_ms),
        "destroy_ms": summary(&destroy_ms),
        "create_ms_first": create_ms.first(),
        "create_ms_last": create_ms.last(),
        "create_failures": create_failures,
        "encode_call_failures": encode_failures,
        "final_flush_failures": flush_failures,
        "callback_errors": n_cb_err,
        "callback_errors_first": callback_errors,
        "emitted": emitted,
        "dropped": dropped,
        "missing_callbacks": missing,
        "identities": identities,
        "resources": { "start": res_start, "end": res_end, "samples": res_samples },
        "resource_delta": {
            "rss_mb": delta("rss_mb"), "fds": delta("fds"),
            "mach_ports": delta("mach_ports"), "threads": delta("threads"),
        },
        "tuning": tune::report(),
        "series": sampler.finish(),
        "wall_s": wall.elapsed().as_secs_f64(),
    })
}

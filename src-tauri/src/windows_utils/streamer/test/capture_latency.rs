//! Windows capture-to-encoded latency, the counterpart to `macos_utils::streamer::pipeline`'s
//! `end_to_end_latency`.
//!
//! The primary shipping platform had no capture-to-encoded measurement at all: every
//! serialization point between the Desktop Duplication present and the encoded access unit was
//! unmeasured on Windows, so an encode-side improvement could never be attributed. This is that
//! number, produced by the same pieces the live pipeline uses — a real [`Duplicator`], the real
//! [`build_backend`] ladder, the real [`TextureReader`] — rather than by a synthetic frame
//! source, which is what makes it comparable with the macOS harness.
//!
//! Two things make it honest where the macOS one has to be asked nicely:
//!
//! * It paints its own [`WorkloadWindow`], so the display is guaranteed to be changing. The
//!   macOS harness asserts "no frames captured (idle screen? add activity)" and leaves the
//!   activity to the operator.
//! * The clock is `dup.last_present_host_ns()`, the OS's own presentation stamp for the frame,
//!   on the same `host_now_ns()` epoch the RTP timestamps ride. It is not "when our loop noticed".
//!
//! ```text
//! cargo test --release --lib windows_capture_to_encoded -- --ignored --nocapture
//! ```
//!
//! The summary line is shaped for `scripts/perf-gate.mjs`: `capture_to_encoded=` as
//! `p50/p90/p99/max` in milliseconds, plus `capture_encode_p95=<n>us`. The encode stage is
//! spelled differently from the x264 test's `p95_encode=` on purpose — this one is whichever
//! encoder the `Auto` ladder picked on the machine, so sharing a metric name with libx264 would
//! be comparing NVENC against a software baseline.

use std::time::{Duration, Instant};

use crate::streamer::audio::host_now_ns;
use crate::streamer::config::{EncoderVendor, H264Profile};
use crate::streamer::synthetic::Workload;
use crate::windows_utils::streamer::capture::{monitor_dimensions, monitor_rect, select_monitor};
use crate::windows_utils::streamer::dxgi::{Duplicator, PollStatus};
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;
use crate::windows_utils::streamer::pipeline::build_backend;
use crate::windows_utils::streamer::scaler::{ReadbackFormat, TextureReader};
use crate::windows_utils::streamer::workload_window::WorkloadWindow;

use super::{show_measurements, skip};
use crate::test_support::percentiles;

/// How long to hold the capture open. Long enough for the percentiles to mean something,
/// short enough that nobody minds an interactive desktop being covered for it.
const RUN: Duration = Duration::from_secs(6);
/// Frames below this and the numbers are startup transients, not a measurement.
const MIN_FRAMES: usize = 30;

#[test]
#[ignore = "requires an interactive desktop, covers the primary display with a workload window, \
            and drives a real encoder; run with --ignored --nocapture"]
fn windows_capture_to_encoded_latency() {
    show_measurements();

    let Ok((monitor, _info)) = select_monitor(1) else {
        skip("windows capture latency", "no monitor 1 to duplicate");
        return;
    };
    let Ok(device_name) = monitor.device_name() else {
        skip(
            "windows capture latency",
            "monitor reports no \\\\.\\DISPLAYn name",
        );
        return;
    };
    let Some((width, height)) = monitor_dimensions(&device_name) else {
        skip("windows capture latency", "monitor reports no dimensions");
        return;
    };
    // The encoder wants even dimensions; duplication hands us whatever the desktop is.
    let (enc_w, enc_h) = (width & !1, height & !1);
    let fps = 60u32;

    // Something has to be moving or Desktop Duplication reports Timeout forever and the harness
    // measures an idle screen. `Motion` is the pattern with the most inter-frame change.
    let workload = monitor_rect(&device_name).and_then(|(left, top, w, h)| {
        match WorkloadWindow::spawn(left, top, w, h, fps, Workload::Motion, true) {
            Ok(window) => Some(window),
            Err(e) => {
                eprintln!(
                    "workload window unavailable ({e:#}); capturing the live desktop instead"
                );
                None
            }
        }
    });

    let mut dup = match Duplicator::new(&device_name, width, height) {
        Ok(d) => d,
        Err(e) => {
            skip(
                "windows capture latency (Desktop Duplication unavailable)",
                e,
            );
            return;
        }
    };
    println!(
        "duplicating {device_name} {width}x{height} via {}",
        dup.api().name()
    );

    let config = EncoderConfig {
        width: enc_w,
        height: enc_h,
        fps,
        bitrate_bps: 12_000_000,
        max_bitrate_bps: 12_000_000,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    };

    // The real ladder, so the number describes the encoder this machine would actually stream
    // with rather than whichever one the test happened to name.
    let mut backend = match build_backend(
        config,
        EncoderVendor::Auto,
        width,
        height,
        dup.device(),
        dup.context(),
    ) {
        Ok(b) => b,
        Err(e) => {
            skip("windows capture latency (no encoder at all)", e);
            return;
        }
    };
    let backend_name = backend.name();
    let cpu_bridge = backend.is_cpu_bridge();
    println!("encoder backend: {backend_name} (cpu_bridge={cpu_bridge})");

    let mut reader = if cpu_bridge {
        match TextureReader::new(dup.device(), dup.context(), enc_w, enc_h) {
            Ok(r) => Some(r),
            Err(e) => {
                skip("windows capture latency (no staging texture)", e);
                return;
            }
        }
    } else {
        None
    };

    let mut end_to_end_us: Vec<u128> = Vec::new();
    let mut encode_us: Vec<u128> = Vec::new();
    let mut bytes = 0usize;
    let mut idr = true;
    let mut coalesced = 0u64;

    let deadline = Instant::now() + RUN;
    while Instant::now() < deadline {
        match dup.poll(8) {
            Ok(PollStatus::Dirty) => {}
            Ok(PollStatus::Timeout) => continue,
            Err(e) => {
                eprintln!("duplication stopped early: {e:?}");
                break;
            }
        }
        coalesced += dup.take_accumulated_drops();

        let present_ns = dup.last_present_host_ns();
        let tex = match dup.frame() {
            Ok(t) => t.clone(),
            Err(e) => {
                eprintln!("compose failed: {e:?}");
                break;
            }
        };

        let encode_start = Instant::now();
        let encoded = match reader.as_mut() {
            // `TextureReader` reads the captured texture's own format, which is BGRA — not
            // whatever the backend would have asked a scaler for.
            Some(reader) => match reader.read_back(&tex) {
                Ok((data, pitch)) => backend.encode_cpu(data, pitch, ReadbackFormat::Bgra8, idr),
                Err(e) => Err(e),
            },
            None => backend.encode_gpu(&tex, None, idr),
        };
        let encode_elapsed = encode_start.elapsed().as_micros();
        idr = false;

        let au = match encoded {
            Ok(au) => au,
            Err(e) => {
                eprintln!("encode failed: {e:?}");
                break;
            }
        };
        if au.is_empty() {
            continue;
        }
        bytes += au.len();
        encode_us.push(encode_elapsed);
        // `present_ns` is 0 until the OS has reported a presentation time for this frame; a
        // sample anchored on 0 would read as the whole uptime and poison the percentiles.
        if present_ns != 0 {
            end_to_end_us.push((host_now_ns().saturating_sub(present_ns) / 1000) as u128);
        }
    }

    drop(workload);

    assert!(
        encode_us.len() >= MIN_FRAMES,
        "only {} encoded frames in {}s — the display was not changing, or duplication stalled",
        encode_us.len(),
        RUN.as_secs()
    );
    assert!(
        !end_to_end_us.is_empty(),
        "duplication never reported a presentation time, so nothing could be anchored"
    );

    let e2e = percentiles(&end_to_end_us);
    let enc = percentiles(&encode_us);
    let ms = |us: u128| us as f64 / 1000.0;
    let budget_us = 1_000_000u128 / fps as u128;

    println!(
        "windows capture->encoded OK: backend={backend_name} {enc_w}x{enc_h}@{fps}, \
         {} frames, {bytes} bytes, coalesced={coalesced}, \
         capture_to_encoded={:.2}/{:.2}/{:.2}/{:.2} (p50/p90/p99/max ms), \
         capture_encode_p95={}us (budget {budget_us}us)",
        enc.count,
        ms(e2e.p50),
        ms(e2e.p90),
        ms(e2e.p99),
        ms(e2e.max),
        enc.p95,
    );

    // Not a performance assertion — a sanity one. A p50 above a quarter of a second means the
    // harness measured something other than a frame, and a silently absurd number in a baseline
    // is worse than a failure.
    assert!(
        ms(e2e.p50) < 250.0,
        "capture->encoded p50 of {:.1}ms is not a frame latency; the anchor is wrong",
        ms(e2e.p50)
    );
}

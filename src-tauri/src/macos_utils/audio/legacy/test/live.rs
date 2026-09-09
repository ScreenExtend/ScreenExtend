// Usage:
//   cargo test --lib legacy_live -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use crate::macos_utils::audio::legacy::{probe, LegacyVirtualDeviceSource};
use crate::macos_utils::audio::{ring, AudioFrameSink, AudioSource};
use crate::streamer::audio::AudioDiagnostics;

#[test]
#[ignore = "requires the ScreenExtend Audio driver loaded; takes over the default output device"]
fn legacy_live_capture_roundtrip() {
    if probe::device_present().is_none() {
        eprintln!("SKIP: ScreenExtend Audio device not present (driver not loaded)");
        return;
    }

    let (producer, consumer) = ring::ring(48_000 * 2);
    let diagnostics = Arc::new(AudioDiagnostics::default());
    let sink = AudioFrameSink {
        producer: Arc::new(producer),
        diagnostics: Arc::clone(&diagnostics),
        control_tx: None,
    };

    let mut src = LegacyVirtualDeviceSource::new();
    src.start(sink).expect("legacy backend start");
    let def = crate::macos_utils::audio::legacy::hal::default_output_device();
    let def_uid = crate::macos_utils::audio::legacy::hal::device_uid(def);
    eprintln!("after start: default_output id={def} uid={def_uid:?}");
    eprintln!("started; playing audio through the virtual device…");

    // drive the system mix with a TTS phrase (plays to the now-default virtual device)
    let _ = std::process::Command::new("say")
        .arg("integration test, one two three four five six seven eight nine ten, testing")
        .spawn();

    std::thread::sleep(Duration::from_secs(8));

    let avail = consumer.available();
    let mut buf = vec![0f32; avail.min(48_000)];
    let n = consumer.pop(&mut buf);
    let rms = if n > 0 {
        (buf[..n].iter().map(|x| x * x).sum::<f32>() / n as f32).sqrt()
    } else {
        0.0
    };
    let nonsilent = src.nonsilent_samples();
    eprintln!("captured {n} samples, rms={rms:.5}, nonsilent_total={nonsilent}");

    src.stop();
    eprintln!("stopped; default output restored");

    assert!(
        nonsilent > 0,
        "expected non-silent capture through the shipped Rust pipeline (rms={rms})"
    );
}

/// End-to-end through the shipped `start_capture()` path: how long a sample takes to get from the
/// driver to an Opus packet, and whether its stamp ever goes backwards.
///
/// Usage:
///   cargo test --lib legacy_live_latency -- --ignored --nocapture
#[test]
#[ignore = "requires the ScreenExtend Audio driver loaded; takes over the default output device"]
fn legacy_live_latency_and_monotonicity() {
    use crate::streamer::audio::host_now_ns;

    if probe::device_present().is_none() {
        eprintln!("SKIP: ScreenExtend Audio device not present (driver not loaded)");
        return;
    }

    // W8 replaced `AudioCapture::rx` with a broadcast `AudioSink` the backends publish into, so
    // subscribe before starting: a receiver created after the fact would miss the head of the run.
    let (tx, mut rx) = tokio::sync::broadcast::channel(64);
    let capture =
        crate::macos_utils::audio::start_capture(crate::streamer::audio::AudioSink::new(tx))
            .expect("start_capture");
    // Two phrases with a gap between them. The virtual device runs no IO cycle while nothing is
    // playing, so the gap is a real pause in the packet stream - the case where a sample-counter
    // timestamp would drift behind the hardware for good if it never re-seated.
    std::thread::spawn(|| {
        let _ = std::process::Command::new("say")
            .arg("measuring capture to packet latency, one two three four five")
            .status();
        std::thread::sleep(Duration::from_secs(3));
        let _ = std::process::Command::new("say")
            .arg("resuming after a silent gap, six seven eight nine ten")
            .status();
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(16);
    let mut lag_ns: Vec<i64> = Vec::new();
    let mut deltas: Vec<i64> = Vec::new();
    let mut regressions = 0usize;
    let mut last_ns: Option<u64> = None;
    let mut last_seq: Option<u32> = None;
    let mut seq_gaps = 0usize;
    let mut lagged = 0u64;

    while std::time::Instant::now() < deadline {
        let pkt = match rx.try_recv() {
            Ok(p) => p,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                lagged += n;
                continue;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        };
        lag_ns.push(host_now_ns() as i64 - pkt.capture_ns as i64);
        if let Some(prev) = last_ns {
            let d = pkt.capture_ns as i64 - prev as i64;
            if d <= 0 {
                regressions += 1;
            }
            deltas.push(d);
        }
        if let Some(p) = last_seq {
            if pkt.seq != p.wrapping_add(1) {
                seq_gaps += 1;
            }
        }
        last_ns = Some(pkt.capture_ns);
        last_seq = Some(pkt.seq);
    }

    let diag = capture.diagnostics.summary();
    let capture_discontinuities = capture
        .diagnostics
        .discontinuity_count
        .load(std::sync::atomic::Ordering::Relaxed);
    drop(capture);

    assert!(!lag_ns.is_empty(), "no packets arrived in 10 s");
    lag_ns.sort_unstable();
    let pct = |q: f64| lag_ns[((lag_ns.len() as f64 * q) as usize).min(lag_ns.len() - 1)];
    let ms = |v: i64| v as f64 / 1e6;
    eprintln!(
        "packets={} capture->packet ms: p50={:.2} p90={:.2} p99={:.2} min={:.2} max={:.2}",
        lag_ns.len(),
        ms(pct(0.50)),
        ms(pct(0.90)),
        ms(pct(0.99)),
        ms(lag_ns[0]),
        ms(*lag_ns.last().unwrap())
    );
    let odd = deltas.iter().filter(|&&d| d != 5_000_000).count();
    eprintln!(
        "capture_ns deltas: {} of {} are not exactly 5.000 ms, regressions={regressions}, \
         seq gaps={seq_gaps}",
        odd,
        deltas.len()
    );
    eprintln!("diagnostics: {diag}");
    assert_eq!(
        lagged, 0,
        "the test subscriber must keep up with the broadcast"
    );

    eprintln!(
        "clock resyncs after the gap: discontinuity_count={}",
        capture_discontinuities
    );
    assert_eq!(regressions, 0, "capture_ns must never go backwards");
    assert_eq!(seq_gaps, 0, "packet sequence must be contiguous");
}

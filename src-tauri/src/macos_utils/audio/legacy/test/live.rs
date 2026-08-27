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

    let (producer, consumer, consumer_thread_lock) = ring::ring(48_000 * 2);
    let diagnostics = Arc::new(AudioDiagnostics::default());
    let sink = AudioFrameSink {
        producer: Arc::new(producer),
        diagnostics: Arc::clone(&diagnostics),
        control_tx: None,
        consumer_thread: consumer_thread_lock,
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

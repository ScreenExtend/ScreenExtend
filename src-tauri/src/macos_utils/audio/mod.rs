pub mod format;
pub mod legacy;
pub mod opus_encoder;
pub mod process_tap;
pub mod ring;
pub mod sck_audio;

#[cfg(test)]
mod test;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use bytes::Bytes;

use crate::streamer::audio::{
    host_now_ns, AudioCapture, AudioDiagnostics, AudioFormat, AudioPacket, AudioStopFn,
    FLAG_DISCONTINUITY, FLAG_SILENT,
};
use opus_encoder::{OpusEncoder, OpusEncoderConfig, FRAME_INTERLEAVED};

const RING_CAPACITY: usize = 48_000 * 2 * 80 / 1000; // 7680 → rounds to 8192
const ENCODE_WINDOW: usize = 4096;
const DIAG_LOG_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackend {
    ProcessTap,
    ScreenCaptureKitAudio,
    VirtualDevice,
    NeedsDriverInstall,
    Unsupported,
}

impl AudioBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            AudioBackend::ProcessTap => "process_tap",
            AudioBackend::ScreenCaptureKitAudio => "screencapturekit",
            AudioBackend::VirtualDevice => "virtual_device",
            AudioBackend::NeedsDriverInstall => "needs_driver_install",
            AudioBackend::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AudioCaptureError {
    #[error("system audio capture unsupported: {0}")]
    Unsupported(String),
    #[error("audio capture setup failed: {0}")]
    Setup(String),
}

#[derive(Debug, Clone, Copy)]
pub enum ControlMsg {
    Reacquire,
}

#[derive(Clone)]
pub struct AudioFrameSink {
    pub producer: Arc<ring::Producer>,
    pub diagnostics: Arc<AudioDiagnostics>,
    pub control_tx: Option<crossbeam_channel::Sender<ControlMsg>>,
    pub consumer_thread: Arc<OnceLock<Thread>>,
}

pub trait AudioSource: Send {
    fn start(&mut self, sink: AudioFrameSink) -> Result<(), AudioCaptureError>;
    fn stop(&mut self);
    fn backend_name(&self) -> &'static str;
    fn reacquire(&mut self) -> Result<(), AudioCaptureError> {
        Ok(())
    }
    fn nonsilent_samples(&self) -> u64 {
        0
    }
}

pub fn probe_audio_backend() -> AudioBackend {
    static NATIVE: OnceLock<Option<AudioBackend>> = OnceLock::new();
    let native = *NATIVE.get_or_init(|| capable_native_backends().first().copied());
    if let Some(b) = native {
        return b;
    }

    if legacy::probe::eligible_os() {
        match legacy::probe::legacy_state() {
            legacy::probe::LegacyState::Ready => AudioBackend::VirtualDevice,
            _ => AudioBackend::NeedsDriverInstall,
        }
    } else {
        AudioBackend::Unsupported
    }
}

fn capable_native_backends() -> Vec<AudioBackend> {
    use crate::macos_utils::streamer::{macos_at_least, screencapturekit_available};
    let mut v = Vec::new();
    if macos_at_least(14, 2) && process_tap::runtime_available() {
        v.push(AudioBackend::ProcessTap);
    }
    if macos_at_least(13, 0)
        && screencapturekit_available()
        && sck_audio::SckAudioCapture::runtime_available()
    {
        v.push(AudioBackend::ScreenCaptureKitAudio);
    }
    v
}

fn capable_backends() -> Vec<AudioBackend> {
    let mut v = capable_native_backends();
    if v.is_empty() && legacy::probe::eligible_os() && legacy::probe::driver_healthy() {
        v.push(AudioBackend::VirtualDevice);
    }
    v
}

fn make_backend(kind: AudioBackend) -> Box<dyn AudioSource> {
    match kind {
        AudioBackend::ProcessTap => Box::new(process_tap::ProcessTapCapture::new()),
        AudioBackend::ScreenCaptureKitAudio => Box::new(sck_audio::SckAudioCapture::new()),
        AudioBackend::VirtualDevice => Box::new(legacy::LegacyVirtualDeviceSource::new()),
        AudioBackend::NeedsDriverInstall | AudioBackend::Unsupported => {
            unreachable!("capable_backends never yields a non-startable backend")
        }
    }
}

pub fn start_capture() -> Result<AudioCapture> {
    let candidates = capable_backends();
    if candidates.is_empty() {
        let ver = os_version();
        if legacy::probe::eligible_os() {
            bail!(
                "system audio on macOS {ver} needs the ScreenExtend Audio driver installed; \
                 enable it from the device's audio toggle to run the one-time install"
            );
        }
        bail!(
            "system audio capture is not supported on this macOS version ({ver}); \
             it requires macOS 13 (ScreenCaptureKit) or 14.2+ (Process Tap), or 10.15–12.x with \
             the ScreenExtend Audio driver"
        );
    }

    let diagnostics = Arc::new(AudioDiagnostics::default());
    let (producer, consumer, consumer_thread_lock) = ring::ring(RING_CAPACITY);
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::bounded::<ControlMsg>(8);
    let sink = AudioFrameSink {
        producer: Arc::new(producer),
        diagnostics: Arc::clone(&diagnostics),
        control_tx: Some(ctrl_tx),
        consumer_thread: consumer_thread_lock,
    };

    let (pkt_tx, pkt_rx) = crossbeam_channel::unbounded::<AudioPacket>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<i32>>();
    let stop = Arc::new(AtomicBool::new(false));

    let diag_thread = Arc::clone(&diagnostics);
    let stop_thread = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("macos-audio".to_string())
        .spawn(move || {
            worker(
                candidates,
                sink,
                consumer,
                ctrl_rx,
                pkt_tx,
                diag_thread,
                stop_thread,
                ready_tx,
            )
        })?;

    let lookahead_samples = match ready_rx.recv() {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => {
            let _ = join.join();
            return Err(e);
        }
        Err(_) => {
            let _ = join.join();
            bail!("audio worker exited during setup");
        }
    };

    let stop_flag = Arc::clone(&stop);
    let mut join_holder = Some(join);
    let stop_fn: AudioStopFn = Box::new(move || {
        stop_flag.store(true, Ordering::Relaxed);
        if let Some(j) = join_holder.take() {
            let _ = j.join();
        }
    });

    Ok(AudioCapture {
        rx: pkt_rx,
        stop: stop_fn,
        format: AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        },
        diagnostics,
        lookahead_samples,
    })
}

#[allow(clippy::too_many_arguments)]
fn worker(
    candidates: Vec<AudioBackend>,
    sink: AudioFrameSink,
    consumer: ring::Consumer,
    ctrl_rx: crossbeam_channel::Receiver<ControlMsg>,
    pkt_tx: crossbeam_channel::Sender<AudioPacket>,
    diagnostics: Arc<AudioDiagnostics>,
    stop: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<i32>>,
) {
    crate::macos_utils::streamer::qos::pin_current_thread_user_initiated();

    let mut encoder = match OpusEncoder::new(OpusEncoderConfig::default()) {
        Ok(e) => e,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    crate::tprintln!("audio: {} loaded", encoder.version());
    let lookahead = encoder.lookahead_samples();

    let mut backend: Box<dyn AudioSource> = 'pick: {
        for kind in &candidates {
            let mut b = make_backend(*kind);
            match b.start(sink.clone()) {
                Ok(()) => {
                    crate::tprintln!("audio: capture backend = {}", b.backend_name());
                    break 'pick b;
                }
                Err(e) => crate::teprintln!(
                    "audio: {} backend failed to start ({e}); trying next",
                    kind.as_str()
                ),
            }
        }
        let _ = ready_tx.send(Err(anyhow::anyhow!(
            "all capable audio backends failed to start"
        )));
        return;
    };
    let _ = ready_tx.send(Ok(lookahead));

    sink.producer.set_consumer_thread(std::thread::current());
    sink.consumer_thread.get_or_init(|| std::thread::current());

    let mut frame = [0.0f32; FRAME_INTERLEAVED];
    let mut seq: u32 = 0;
    let mut pending_flags: u8 = FLAG_DISCONTINUITY; // first frame starts a fresh timeline
    let mut encode_ns: Vec<u64> = Vec::with_capacity(ENCODE_WINDOW);
    let mut last_diag = Instant::now();
    let mut warned_silent = false;
    let start_instant = Instant::now();
    const FRAME_NS: u64 = 5_000_000; // 5 ms per Opus frame at 48 kHz
    let mut burst_t0: u64 = 0;
    let mut burst_i: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        while let Ok(ControlMsg::Reacquire) = ctrl_rx.try_recv() {
            diagnostics.device_changes.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = backend.reacquire() {
                crate::teprintln!("audio: re-acquire failed: {e}");
            }
            pending_flags |= FLAG_DISCONTINUITY;
        }

        let mut produced = false;
        // seed once per drain pass; frames in the pass are then spaced by FRAME_NS
        burst_t0 = host_now_ns();
        burst_i = 0;
        while consumer.available() >= FRAME_INTERLEAVED {
            if consumer.pop(&mut frame) < FRAME_INTERLEAVED {
                break;
            }
            let is_silent = frame.iter().all(|&x| x == 0.0);

            let t0 = Instant::now();
            let encoded = match encoder.encode_float(&frame) {
                Ok(pkt) => pkt,
                Err(e) => {
                    crate::teprintln!("audio: opus encode failed: {e}");
                    continue;
                }
            };
            let dt = t0.elapsed().as_nanos() as u64;
            if encode_ns.len() < ENCODE_WINDOW {
                encode_ns.push(dt);
            } else {
                encode_ns[(seq as usize) % ENCODE_WINDOW] = dt;
            }

            let mut flags = pending_flags;
            pending_flags = 0;
            if is_silent {
                flags |= FLAG_SILENT;
                diagnostics.silent_count.fetch_add(1, Ordering::Relaxed);
            }
            let pkt = AudioPacket {
                seq,
                capture_ns: burst_t0.saturating_add(burst_i * FRAME_NS),
                flags,
                data: Bytes::copy_from_slice(encoded),
            };
            burst_i += 1;
            if pkt_tx.send(pkt).is_err() {
                backend.stop();
                return;
            }
            seq = seq.wrapping_add(1);
            produced = true;
        }

        // ring overruns (bumped by the RT callback) surface as backpressure drops
        diagnostics
            .dropped_backpressure
            .store(consumer.overruns(), Ordering::Relaxed);

        if last_diag.elapsed() >= DIAG_LOG_INTERVAL {
            update_encode_percentiles(&encode_ns, &diagnostics);
            crate::tprintln!("[{}] {}", backend.backend_name(), diagnostics.summary());
            if !warned_silent
                && start_instant.elapsed() >= Duration::from_secs(5)
                && backend.nonsilent_samples() == 0
            {
                crate::teprintln!(
                    "audio: {} has produced only silence for 5s — if the host IS playing audio, \
                     this is the CATapDescription silent-samples failure mode \
                     (see AUDIO_NOTES_MACOS.md §4.1)",
                    backend.backend_name()
                );
                warned_silent = true;
            }
            last_diag = Instant::now();
        }

        if !produced && consumer.available() < FRAME_INTERLEAVED {
            std::thread::park_timeout(Duration::from_micros(125));
        }
    }

    backend.stop();
    crate::tprintln!("audio: macOS capture stopped");
}

fn update_encode_percentiles(samples: &[u64], diagnostics: &AudioDiagnostics) {
    if samples.is_empty() {
        return;
    }
    let mut sorted: Vec<u64> = samples.to_vec();
    sorted.sort_unstable();
    let p = |q: f64| sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)];
    diagnostics.encode_p50_ns.store(p(0.50), Ordering::Relaxed);
    diagnostics.encode_p99_ns.store(p(0.99), Ordering::Relaxed);
}

fn os_version() -> String {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

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
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use bytes::Bytes;

use crate::streamer::audio::{
    host_now_ns, AudioCapture, AudioDiagnostics, AudioFormat, AudioPacket, AudioSink, AudioStopFn,
    FLAG_DISCONTINUITY, FLAG_SILENT,
};
use opus_encoder::{OpusEncoder, OpusEncoderConfig, FRAME_INTERLEAVED};

const RING_CAPACITY: usize = 48_000 * 2 * 80 / 1000; // 7680 → rounds to 8192
const DIAG_LOG_INTERVAL: Duration = Duration::from_secs(2);

/// nanoseconds of audio in one interleaved stereo Opus frame (5 ms at 48 kHz)
const FRAME_NS: u64 = 5_000_000;
/// interleaved stereo samples per second (the ring's unit)
const RING_SAMPLES_PER_SEC: i128 = 48_000 * format::OUT_CHANNELS as i128;
/// how far the hardware clock may run ahead of the sample counter before the timeline is re-seated
const RESYNC_NS: u64 = 50_000_000;

/// Host-clock time of the sample at absolute ring index `pos`, extrapolated from `anchor`. The
/// rate is exact (every backend delivers 48 kHz stereo), so this is a sample-counter timestamp
/// seated on a hardware stamp, not a wall-clock reading at drain time.
fn anchored_ns(anchor: ring::Anchor, pos: u64) -> u64 {
    let delta = pos as i128 - anchor.pos as i128;
    let ns = anchor.host_ns as i128 + (delta * 1_000_000_000i128) / RING_SAMPLES_PER_SEC;
    if ns <= 0 {
        0
    } else {
        ns as u64
    }
}

/// Pick the capture time of the frame about to be encoded, and say whether the timeline jumped.
///
/// `next` is the sample counter's answer (one frame period after the last packet), `hw` the
/// capture hardware's stamp. The counter wins, so stamps are exactly one frame apart and never
/// regress; the hardware takes over only once it is more than [`RESYNC_NS`] ahead, which means
/// samples were lost.
fn choose_capture_ns(next: Option<u64>, hw: Option<u64>) -> (u64, bool) {
    match (next, hw) {
        (Some(n), Some(h)) if h > n.saturating_add(RESYNC_NS) => (h, true),
        (Some(n), _) => (n, false),
        (None, Some(h)) => (h, false),
        (None, None) => (host_now_ns(), false),
    }
}

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

/// What each capability probe answered, as plain data, so the ranking can be tested without the
/// probes' I/O (`dlsym`, class lookups, a driver stat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioProbes {
    /// `AudioHardwareCreateProcessTap` and `CATapDescription` both resolved (14.2+).
    pub process_tap: bool,
    /// The ScreenCaptureKit classes resolved (13.0+ for audio).
    pub sck: bool,
    /// The ScreenExtend Audio driver is installed *and* its device is present.
    pub driver_healthy: bool,
}

impl AudioProbes {
    /// Ask the machine. Native answers come from the `OnceLock` cache because a class cannot
    /// appear while the process runs; the driver answer is deliberately live, because an install
    /// finishes mid-session and the UI has to notice.
    fn probe() -> Self {
        Self {
            process_tap: native_backends().contains(&AudioBackend::ProcessTap),
            sck: native_backends().contains(&AudioBackend::ScreenCaptureKitAudio),
            driver_healthy: legacy::probe::driver_healthy(),
        }
    }
}

/// The tier this Mac reports, given what the probes answered. Pure.
pub(crate) fn rank_backend(probes: AudioProbes) -> AudioBackend {
    if probes.process_tap {
        return AudioBackend::ProcessTap;
    }
    if probes.sck {
        return AudioBackend::ScreenCaptureKitAudio;
    }
    if probes.driver_healthy {
        return AudioBackend::VirtualDevice;
    }
    // never `Unsupported`: below the native tiers the driver is the answer, and it covers the
    // app's 10.15 floor
    AudioBackend::NeedsDriverInstall
}

/// Every tier `start_capture` may try, best first. Pure.
pub(crate) fn rank_capable_backends(probes: AudioProbes) -> Vec<AudioBackend> {
    let mut v = Vec::new();
    if probes.process_tap {
        v.push(AudioBackend::ProcessTap);
    }
    if probes.sck {
        v.push(AudioBackend::ScreenCaptureKitAudio);
    }
    // The driver is a real fallback, not a below-13.0-only tier: on 13.x with Screen Recording
    // denied, SCK is selected and fails to start.
    if probes.driver_healthy {
        v.push(AudioBackend::VirtualDevice);
    }
    v
}

pub fn probe_audio_backend() -> AudioBackend {
    rank_backend(AudioProbes::probe())
}

/// The native tiers this Mac can run, best first.
///
/// Probed by capability alone (`dlsym` and class lookups), with no OS-version conjunct: a version
/// gate adds a `sw_vers` subprocess and a fail-open path to a question the class lookup already
/// answers. Cached, because neither answer can change while the process runs.
fn native_backends() -> &'static [AudioBackend] {
    static NATIVE: OnceLock<Vec<AudioBackend>> = OnceLock::new();
    NATIVE.get_or_init(|| {
        let mut v = Vec::new();
        if process_tap::runtime_available() {
            v.push(AudioBackend::ProcessTap);
        }
        if sck_audio::SckAudioCapture::runtime_available() {
            v.push(AudioBackend::ScreenCaptureKitAudio);
        }
        v
    })
}

fn capable_backends() -> Vec<AudioBackend> {
    rank_capable_backends(AudioProbes::probe())
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

/// Ceiling on how long `start_capture` will wait for a backend to come up.
///
/// `AudioHub::subscribe` keeps every other session's subscribe and unsubscribe waiting until
/// this call returns, so it must be bounded. Each backend bounds its own blocking calls; this
/// is the backstop for one that wedges somewhere none of them cover.
const SETUP_TIMEOUT: Duration = Duration::from_secs(15);

fn report_worker_panic(result: std::thread::Result<()>) {
    if let Err(payload) = result {
        let what = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown payload".to_string());
        crate::teprintln!("audio: capture worker panicked: {what}");
    }
}

pub fn start_capture(pkt_sink: AudioSink) -> Result<AudioCapture> {
    let candidates = capable_backends();
    if candidates.is_empty() {
        bail!(
            "system audio on macOS {} needs the ScreenExtend Audio driver installed; \
             enable it from the device's audio toggle to run the one-time install",
            os_version()
        );
    }

    let diagnostics = Arc::new(AudioDiagnostics::default());
    let (producer, consumer) = ring::ring(RING_CAPACITY);
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::bounded::<ControlMsg>(8);
    let sink = AudioFrameSink {
        producer: Arc::new(producer),
        diagnostics: Arc::clone(&diagnostics),
        control_tx: Some(ctrl_tx),
    };

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
                pkt_sink,
                diag_thread,
                stop_thread,
                ready_tx,
            )
        })?;

    let lookahead_samples = match ready_rx.recv_timeout(SETUP_TIMEOUT) {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => {
            report_worker_panic(join.join());
            return Err(e);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            stop.store(true, Ordering::Relaxed);
            report_worker_panic(join.join());
            bail!("audio capture setup did not finish within {SETUP_TIMEOUT:?}");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            report_worker_panic(join.join());
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
    pkt_sink: AudioSink,
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
    crate::tprintln!("{}", encoder.describe_state());
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

    let mut frame = [0.0f32; FRAME_INTERLEAVED];
    let mut seq: u32 = 0;
    let mut pending_flags: u8 = FLAG_DISCONTINUITY; // first frame starts a fresh timeline
    let mut last_diag = Instant::now();
    let mut warned_silent = false;
    let start_instant = Instant::now();
    // Host-clock time of the next sample to be popped: seeded from the capture hardware's stamp,
    // then advanced one frame at a time, so stamps are exactly 5 ms apart and never regress.
    let mut next_capture_ns: Option<u64> = None;
    let mut resyncs: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        while let Ok(ControlMsg::Reacquire) = ctrl_rx.try_recv() {
            diagnostics.device_changes.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = backend.reacquire() {
                crate::teprintln!("audio: re-acquire failed: {e}");
            }
            pending_flags |= FLAG_DISCONTINUITY;
            next_capture_ns = None; // new device, new timeline
        }

        let mut produced = false;
        while consumer.available() >= FRAME_INTERLEAVED {
            let pos = consumer.read_pos();
            if consumer.pop(&mut frame) < FRAME_INTERLEAVED {
                break;
            }
            let hw_ns = consumer
                .anchor()
                .map(|a| anchored_ns(a, pos))
                .filter(|&ns| ns != 0);
            let (capture_ns, jumped) = choose_capture_ns(next_capture_ns, hw_ns);
            if jumped {
                resyncs += 1;
                pending_flags |= FLAG_DISCONTINUITY;
                // a re-seat counts as a discontinuity in the summary too
                diagnostics
                    .discontinuity_count
                    .fetch_add(1, Ordering::Relaxed);
            }
            next_capture_ns = Some(capture_ns.saturating_add(FRAME_NS));
            let is_silent = frame.iter().all(|&x| x == 0.0);

            let t0 = Instant::now();
            let encoded = match encoder.encode_float(&frame) {
                Ok(pkt) => pkt,
                Err(e) => {
                    crate::teprintln!("audio: opus encode failed: {e}");
                    continue;
                }
            };
            diagnostics.record_encode_ns(t0.elapsed().as_nanos() as u64);

            let mut flags = pending_flags;
            pending_flags = 0;
            if is_silent {
                flags |= FLAG_SILENT;
                diagnostics.silent_count.fetch_add(1, Ordering::Relaxed);
            }
            let pkt = AudioPacket {
                seq,
                capture_ns,
                flags,
                data: Bytes::copy_from_slice(encoded),
            };
            diagnostics
                .capture_to_packet
                .record_span(pkt.capture_ns, host_now_ns());
            pkt_sink.send(pkt);
            seq = seq.wrapping_add(1);
            produced = true;
        }

        // `dropped_backpressure` is owned by the producers (capture callbacks and the legacy shm
        // reader both `fetch_add` their own losses). Do not `store(consumer.overruns())` here:
        // it would erase the legacy reader's count.

        if last_diag.elapsed() >= DIAG_LOG_INTERVAL {
            crate::tprintln!(
                "[{}] {}, clock_resyncs={resyncs}",
                backend.backend_name(),
                diagnostics.summary()
            );
            if !warned_silent
                && start_instant.elapsed() >= Duration::from_secs(5)
                && backend.nonsilent_samples() == 0
            {
                // not the classic tap-list UID mismatch (`noErr` and pure silence):
                // `process_tap.rs` builds the sub-tap dictionary from the description's own
                // `UUIDString`. That leaves consent and exclusivity.
                crate::teprintln!(
                    "audio: {} has produced only silence for 5s. If the host IS playing audio, \
                     check Screen Recording / audio permissions and whether another process \
                     holds an exclusive tap on the output device",
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

fn os_version() -> String {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

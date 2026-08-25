//! macOS system-audio capture (PRD-macos-audio.md §5). One shared interface (`AudioSource`) over
//! two backends, tiered at runtime by [`probe_audio_backend`]:
//!
//! * [`process_tap`] — Core Audio Process Tap (14.2+), preferred (audio-only, no screen-recording
//!   indicator).
//! * [`sck_audio`] — ScreenCaptureKit `capturesAudio` (13.0+), fallback.
//! * otherwise — `Unsupported` (a clear message; **no** virtual audio driver — §2, §3).
//!
//! Everything above the `AudioSource` line — Opus encode, DataChannel transport, the client — is
//! the OS-independent code shared with the Windows feature (`crate::streamer::audio`), written
//! once. This module only produces `AudioPacket`s into that pipeline via
//! [`crate::streamer::audio::AudioCapture`], exactly like `windows_utils/audio`.
//!
//! Threading (§9.2): the real-time capture callback (the Process Tap `AudioDeviceIOProc` or the
//! SCK sample handler) only converts to interleaved-stereo-f32 and pushes into a lock-free ring
//! ([`ring`]); a dedicated **worker thread** here drains the ring, Opus-encodes 5 ms frames, and
//! forwards `AudioPacket`s over `crossbeam-channel` to the `AudioHub`. The worker also polls a
//! control channel to re-acquire the tap when the default output device changes (§5.5).

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
    host_now_ns, AudioCapture, AudioDiagnostics, AudioFormat, AudioPacket, AudioStopFn,
    FLAG_DISCONTINUITY, FLAG_SILENT,
};
use opus_encoder::{OpusEncoder, OpusEncoderConfig, FRAME_INTERLEAVED};

/// Ring capacity in f32 samples (~340 ms of 48 kHz stereo). Rounded up to a power of two by
/// [`ring::ring`]. Big enough to absorb an encoder-thread scheduling hiccup without overrun.
const RING_CAPACITY: usize = 48_000 * 2 / 3;
const ENCODE_WINDOW: usize = 4096;
const DIAG_LOG_INTERVAL: Duration = Duration::from_secs(2);

/// Which capture backend is active / available (PRD §2, §8.4; legacy tier: PRD-macos-legacy-audio §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackend {
    ProcessTap,
    ScreenCaptureKitAudio,
    /// macOS 10.15–12.x, our virtual device installed and healthy (legacy tier).
    VirtualDevice,
    /// macOS 10.15–12.x, in range but the driver needs a one-time install (actionable, not a dead
    /// end — distinct from `Unsupported`, PRD-macos-legacy-audio §4).
    NeedsDriverInstall,
    Unsupported,
}

impl AudioBackend {
    /// Stable identifier surfaced to the frontend via `CompatibilityReport.audio_backend` and in
    /// diagnostics (§8.4, §10).
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

/// Control-thread messages posted from OS notification callbacks (default-output-device change,
/// stream invalidation). Kept off the real-time path (§5.5).
#[derive(Debug, Clone, Copy)]
pub enum ControlMsg {
    /// The default output device changed (or the stream was invalidated) — rebuild the capture.
    Reacquire,
}

/// The real-time hand-off a backend pushes into: the ring producer plus the shared diagnostics
/// and an optional control channel for device-change notifications. Cheap to clone (all
/// `Arc`/`Option`), so `reacquire` can rebuild against the same ring.
#[derive(Clone)]
pub struct AudioFrameSink {
    pub producer: Arc<ring::Producer>,
    pub diagnostics: Arc<AudioDiagnostics>,
    pub control_tx: Option<crossbeam_channel::Sender<ControlMsg>>,
}

/// One shared interface over both backends (PRD §5.1). The encode/transport code above is written
/// once against this, never against a concrete backend. Mirrors how `CaptureBackend` generalizes
/// `cgds` vs `sck` for video.
pub trait AudioSource: Send {
    fn start(&mut self, sink: AudioFrameSink) -> Result<(), AudioCaptureError>;
    fn stop(&mut self);
    fn backend_name(&self) -> &'static str;
    /// Rebuild capture against the current default device / after invalidation (§5.5).
    fn reacquire(&mut self) -> Result<(), AudioCaptureError> {
        Ok(())
    }
    /// Non-silent samples seen so far (diagnostic for the §4.1 silent-samples failure mode).
    fn nonsilent_samples(&self) -> u64 {
        0
    }
}

/// The best backend this host is capable of (best-first), cached so `check_system_requirements`
/// doesn't re-probe on every call. Drives the UI toggle + `CompatibilityReport.audio_backend`.
///
/// This is a **prompt-free capability** check (OS version + runtime symbol/class presence), *not* a
/// tap construction: creating a Process Tap fires the "System Audio Recording" TCC prompt, and this
/// runs at bootstrap, so it must not prompt for an off-by-default feature. The real
/// construct-and-fall-back — which is what actually catches a TCC denial or the silent-samples
/// mode — happens in [`start_capture`] when a device enables audio; that is the right moment for
/// §2.1's "attempt the higher-tier API and fall back on failure", and where a prompt is expected.
pub fn probe_audio_backend() -> AudioBackend {
    // The native tiers (Process Tap / SCK) are immutable for the life of the process, so cache
    // them. The legacy tier's availability flips at runtime (the user installs/uninstalls the
    // driver), so it must be recomputed on each call — a cheap device-list enumeration.
    static NATIVE: OnceLock<Option<AudioBackend>> = OnceLock::new();
    let native = *NATIVE.get_or_init(|| capable_native_backends().first().copied());
    if let Some(b) = native {
        return b;
    }

    // No native backend: on 10.15–12.x offer the virtual device (installed → VirtualDevice, else
    // the actionable NeedsDriverInstall); below 10.15 it's a genuine dead end.
    if legacy::probe::eligible_os() {
        match legacy::probe::legacy_state() {
            legacy::probe::LegacyState::Ready => AudioBackend::VirtualDevice,
            _ => AudioBackend::NeedsDriverInstall,
        }
    } else {
        AudioBackend::Unsupported
    }
}

/// The native backends this host is capable of, in strict preference order (Process Tap 14.2+ →
/// SCK audio 13.0+). Gated on OS version **and** the runtime presence of the version-specific
/// symbols/classes (prompt-free — see [`probe_audio_backend`]).
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

/// All backends that can actually be *started* right now, best-first. This is the native list plus
/// the legacy virtual device when (and only when) it's healthy on an in-range OS. `NeedsDriverInstall`
/// is never here — it is a UI state, not a startable backend. Note the legacy tier is gated on
/// `eligible_os()` (< 13.0), so it can never be selected on 13.0+ where native wins (PRD §4).
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

/// Start the single host-wide system-audio capture. Called through
/// `streamer::platform::start_audio_capture`; the reference-counted `AudioHub` owns the result.
pub fn start_capture() -> Result<AudioCapture> {
    let candidates = capable_backends();
    if candidates.is_empty() {
        let ver = os_version();
        if legacy::probe::eligible_os() {
            // In range for the virtual device, but it isn't installed/healthy yet — the UI should
            // have run the install flow first (PRD-macos-legacy-audio §9.2).
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
    let (producer, consumer) = ring::ring(RING_CAPACITY);
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::bounded::<ControlMsg>(8);
    let sink = AudioFrameSink {
        producer: Arc::new(producer),
        diagnostics: Arc::clone(&diagnostics),
        control_tx: Some(ctrl_tx),
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
    // The encoder thread is latency-sensitive but not hard-real-time (that's the OS callback);
    // give it an elevated QoS like the video transport thread.
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

    // Try each capable backend for real (construct + start); the first that succeeds wins, and we
    // fall back on failure — this is where §2.1's attempt-and-fall-back actually happens (at user
    // opt-in), and where a TCC denial / silent-samples rejection on the Process Tap defers to SCK.
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
    let mut encode_ns: Vec<u64> = Vec::with_capacity(ENCODE_WINDOW);
    let mut last_diag = Instant::now();
    let mut warned_silent = false;
    let start_instant = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // React to device-change / invalidation (§5.5), off the real-time path.
        while let Ok(ControlMsg::Reacquire) = ctrl_rx.try_recv() {
            diagnostics.device_changes.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = backend.reacquire() {
                crate::teprintln!("audio: re-acquire failed: {e}");
            }
            pending_flags |= FLAG_DISCONTINUITY;
        }

        let mut produced = false;
        while consumer.available() >= FRAME_INTERLEAVED {
            if consumer.pop(&mut frame) < FRAME_INTERLEAVED {
                break;
            }
            // Exact-zero frames are the silent buffers produced by silent tap/SCK delivery.
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
                capture_ns: host_now_ns(),
                flags,
                data: Bytes::copy_from_slice(encoded),
            };
            if pkt_tx.send(pkt).is_err() {
                // No subscribers / bridge dropped — capture is being torn down.
                backend.stop();
                return;
            }
            seq = seq.wrapping_add(1);
            produced = true;
        }

        // Surface the ring overrun counter (bumped by the RT callback) as backpressure drops.
        diagnostics
            .dropped_backpressure
            .store(consumer.overruns(), Ordering::Relaxed);

        if last_diag.elapsed() >= DIAG_LOG_INTERVAL {
            update_encode_percentiles(&encode_ns, &diagnostics);
            crate::tprintln!("[{}] {}", backend.backend_name(), diagnostics.summary());
            // The §4.1 guard: if the stream has run for a few seconds and delivered only silence,
            // warn once — it may be the documented silent-samples failure (or simply a quiet host).
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

        if !produced {
            // Nothing ready — briefly park. 500 µs is well under the jitter-buffer budget and
            // avoids a busy-spin. The RT callback keeps filling the ring meanwhile.
            std::thread::park_timeout(Duration::from_micros(500));
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

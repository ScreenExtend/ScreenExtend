//! Cross-platform audio transport glue: the packet type shared between the (per-OS) capture
//! backend and the WebRTC transport, host-side diagnostics counters, and the reference-counted
//! [`AudioHub`] that owns the single host-wide capture and fans it out to N client sessions.
//!
//! The actual capture + Opus encode lives in the per-OS backend (`windows_utils/audio`,
//! `macos_utils/audio`, `linux_utils/audio`) and is reached through
//! [`crate::streamer::platform::start_audio_capture`]. The wire format is in [`protocol`].

pub mod protocol;

// Opus encode is OS-independent (libopus is cross-platform C), so the FFI shim and encoder
// wrapper live here and are shared by every per-OS capture backend rather than duplicated
// (PRD macos §6). The bundled library differs per OS (`opus_sys::LIB_NAMES`); nothing else does.
pub mod encoder;
pub mod opus_sys;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::broadcast;

/// Opus's native rate and the mix-format fast path (see `AUDIO_NOTES.md`).
pub const AUDIO_SAMPLE_RATE: u32 = 48000;

/// One process-wide monotonic epoch. Both audio capture timestamps and the video RTP timestamps
/// reference this so the client can align A/V against a single host timebase (PRD §6.5): audio
/// carries `capture_ns` in its DataChannel header, and the video track stamps each frame's RTP
/// timestamp with [`host_ns_to_rtp90k`] of the same clock.
static HOST_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Nanoseconds since the shared host epoch. Comparable across threads (monotonic `Instant`).
pub fn host_now_ns() -> u64 {
    let epoch = HOST_EPOCH.get_or_init(std::time::Instant::now);
    epoch.elapsed().as_nanos() as u64
}

/// Nanoseconds of a host-monotonic [`Instant`] on the shared host epoch — the same timebase
/// [`host_now_ns`] returns. The video path converts each frame's `capture` instant this way so
/// audio and video share one capture clock (PRD §6.5). Saturates to 0 for instants before the
/// epoch (only reachable for the very first frame if capture predates epoch init).
pub fn host_instant_to_ns(instant: std::time::Instant) -> u64 {
    let epoch = *HOST_EPOCH.get_or_init(std::time::Instant::now);
    instant.saturating_duration_since(epoch).as_nanos() as u64
}

/// The video RTP clock rate (Hz). H.264 RTP always runs at 90 kHz; we stamp timestamps from the
/// host epoch at this rate so the client can invert them back to host-capture time.
pub const VIDEO_RTP_CLOCK_HZ: u64 = 90_000;

/// Map a host-epoch nanosecond instant to a 90 kHz RTP timestamp (wrapping `u32`), the value the
/// video track writes on the wire. The client inverts this and aligns audio (which carries the
/// same host clock as `capture_ns`) to the displayed video frame's host-capture time (PRD §6.5).
pub fn host_ns_to_rtp90k(ns: u64) -> u32 {
    ((ns as u128 * VIDEO_RTP_CLOCK_HZ as u128) / 1_000_000_000u128) as u32
}

/// Packet flag bits (mirrored on the wire in `protocol` and on the client).
pub const FLAG_SILENT: u8 = 1 << 0;
pub const FLAG_DISCONTINUITY: u8 = 1 << 1;

/// One encoded Opus frame plus the host-timebase capture instant, fanned out to every
/// subscribed session. Cheap to clone (`Bytes` is refcounted).
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub seq: u32,
    /// Host monotonic timebase, nanoseconds (shared with the video path for A/V sync, §6.5).
    pub capture_ns: u64,
    pub flags: u8,
    pub data: Bytes,
}

#[derive(Clone, Copy, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Host-side counters, surfaced through the log bus / Settings live log (PRD §9). Atomics so
/// the real-time capture thread and the async transport can both write without locking.
#[derive(Debug, Default)]
pub struct AudioDiagnostics {
    pub period_frames: AtomicU32,
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    pub discontinuity_count: AtomicU64,
    pub silent_count: AtomicU64,
    pub device_changes: AtomicU64,
    /// Encode time percentiles over a recent window, nanoseconds.
    pub encode_p50_ns: AtomicU64,
    pub encode_p99_ns: AtomicU64,
    /// Packets dropped by DataChannel backpressure (written by the transport).
    pub dropped_backpressure: AtomicU64,
}

impl AudioDiagnostics {
    pub fn summary(&self) -> String {
        format!(
            "audio diag: fmt={}Hz x{}ch period={}fr, encode p50={:.2}ms p99={:.2}ms, \
             discontinuity={}, silent={}, dev_changes={}, dc_drops={}",
            self.sample_rate.load(Ordering::Relaxed),
            self.channels.load(Ordering::Relaxed),
            self.period_frames.load(Ordering::Relaxed),
            self.encode_p50_ns.load(Ordering::Relaxed) as f64 / 1.0e6,
            self.encode_p99_ns.load(Ordering::Relaxed) as f64 / 1.0e6,
            self.discontinuity_count.load(Ordering::Relaxed),
            self.silent_count.load(Ordering::Relaxed),
            self.device_changes.load(Ordering::Relaxed),
            self.dropped_backpressure.load(Ordering::Relaxed),
        )
    }
}

/// Tears down a running capture (joins its OS thread(s)). May block briefly (~one period).
pub type AudioStopFn = Box<dyn FnOnce() + Send>;

/// What a per-OS backend returns from `start_capture`: the packet stream, a stop closure, the
/// negotiated format, live diagnostics, and the encoder look-ahead (for A/V-sync accounting).
pub struct AudioCapture {
    pub rx: crossbeam_channel::Receiver<AudioPacket>,
    pub stop: AudioStopFn,
    pub format: AudioFormat,
    pub diagnostics: Arc<AudioDiagnostics>,
    pub lookahead_samples: i32,
}

const BROADCAST_CAPACITY: usize = 128;

struct Running {
    tx: broadcast::Sender<AudioPacket>,
    stop: Option<AudioStopFn>,
    bridge: Option<std::thread::JoinHandle<()>>,
    format: AudioFormat,
    diagnostics: Arc<AudioDiagnostics>,
    lookahead_samples: i32,
}

struct HubInner {
    running: Option<Running>,
    refcount: usize,
}

/// One session's handle onto the shared capture: a fresh broadcast receiver plus the format /
/// diagnostics it needs. Obtained from [`AudioHub::subscribe`], balanced by
/// [`AudioHub::unsubscribe`].
pub struct AudioSubscription {
    pub rx: broadcast::Receiver<AudioPacket>,
    pub format: AudioFormat,
    pub diagnostics: Arc<AudioDiagnostics>,
    pub lookahead_samples: i32,
}

/// The single host-wide audio capture, reference-counted so it starts on the first
/// audio-enabled subscriber and stops when the last one leaves (PRD §7.5, §4.3). One WASAPI
/// loopback client + one Opus encoder fan out to every session — never N loopback clients.
#[derive(Default)]
pub struct AudioHub {
    inner: Mutex<Option<HubInner>>,
}

impl std::fmt::Debug for AudioHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (running, refcount) = self
            .inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|i| (i.running.is_some(), i.refcount)))
            .unwrap_or((false, 0));
        f.debug_struct("AudioHub")
            .field("running", &running)
            .field("refcount", &refcount)
            .finish()
    }
}

impl AudioHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Some(HubInner {
                running: None,
                refcount: 0,
            })),
        })
    }

    /// Ensure capture is running (starting it on the 0→1 transition) and return a new
    /// subscription. The critical section is sync end-to-end — no `.await` inside — so the
    /// blocking mutex is fine (§8.2).
    pub fn subscribe(&self) -> Result<AudioSubscription> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard.as_mut().expect("audio hub initialized");

        if inner.running.is_none() {
            let cap = crate::streamer::platform::start_audio_capture()?;
            let (tx, _rx0) = broadcast::channel::<AudioPacket>(BROADCAST_CAPACITY);

            // Bridge: the capture thread pushes over crossbeam (§8.3); a small OS thread
            // re-publishes into the broadcast for fan-out. It exits when the crossbeam sender
            // is dropped (i.e. after `stop()` joins the capture thread).
            let bridge_tx = tx.clone();
            let rx = cap.rx;
            let bridge = std::thread::Builder::new()
                .name("audio-bridge".to_string())
                .spawn(move || {
                    while let Ok(pkt) = rx.recv() {
                        let _ = bridge_tx.send(pkt);
                    }
                })?;

            crate::tprintln!(
                "audio: capture started ({}Hz x{}ch, encoder lookahead {} samples)",
                cap.format.sample_rate,
                cap.format.channels,
                cap.lookahead_samples
            );

            inner.running = Some(Running {
                tx,
                stop: Some(cap.stop),
                bridge: Some(bridge),
                format: cap.format,
                diagnostics: cap.diagnostics,
                lookahead_samples: cap.lookahead_samples,
            });
        }

        let running = inner.running.as_ref().expect("running set above");
        let sub = AudioSubscription {
            rx: running.tx.subscribe(),
            format: running.format,
            diagnostics: Arc::clone(&running.diagnostics),
            lookahead_samples: running.lookahead_samples,
        };
        inner.refcount += 1;
        crate::tprintln!("audio: subscriber added (refcount={})", inner.refcount);
        Ok(sub)
    }

    /// Balance a prior [`subscribe`]. On the 1→0 transition, stops capture and joins its
    /// threads. Safe to call more than the refcount (extra calls are ignored). May block
    /// briefly; call via `spawn_blocking` from async contexts, like the video stop path.
    pub fn unsubscribe(&self) {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard.as_mut().expect("audio hub initialized");
        if inner.refcount == 0 {
            return;
        }
        inner.refcount -= 1;
        crate::tprintln!("audio: subscriber removed (refcount={})", inner.refcount);
        if inner.refcount == 0 {
            if let Some(mut running) = inner.running.take() {
                if let Some(stop) = running.stop.take() {
                    stop();
                }
                if let Some(bridge) = running.bridge.take() {
                    let _ = bridge.join();
                }
                crate::tprintln!("audio: capture stopped (last subscriber left)");
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .map(|i| i.running.is_some())
            .unwrap_or(false)
    }

    pub fn diagnostics(&self) -> Option<Arc<AudioDiagnostics>> {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|i| i.running.as_ref())
            .map(|r| Arc::clone(&r.diagnostics))
    }
}

pub type SharedAudioHub = Arc<AudioHub>;

pub mod encoder;
pub mod opus_sys;
pub mod protocol;

#[cfg(test)]
mod opus_abi;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::broadcast;

pub const AUDIO_SAMPLE_RATE: u32 = 48000;

static HOST_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub fn host_now_ns() -> u64 {
    let epoch = HOST_EPOCH.get_or_init(std::time::Instant::now);
    epoch.elapsed().as_nanos() as u64
}

pub fn host_instant_to_ns(instant: std::time::Instant) -> u64 {
    let epoch = *HOST_EPOCH.get_or_init(std::time::Instant::now);
    instant.saturating_duration_since(epoch).as_nanos() as u64
}

pub const VIDEO_RTP_CLOCK_HZ: u64 = 90_000;

pub fn host_ns_to_rtp90k(ns: u64) -> u32 {
    ((ns as u128 * VIDEO_RTP_CLOCK_HZ as u128) / 1_000_000_000u128) as u32
}

pub const FLAG_SILENT: u8 = 1 << 0;
pub const FLAG_DISCONTINUITY: u8 = 1 << 1;
/// The body carries the previous frame's Opus payload after this one; see
/// [`protocol::build_datachannel_message`]. Only ever set for a client that asked for it.
pub const FLAG_REDUNDANT: u8 = 1 << 2;

#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub seq: u32,
    pub capture_ns: u64,
    pub flags: u8,
    pub data: Bytes,
}

#[derive(Clone, Copy, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// What the capture path publishes about itself. Every field is an atomic or a lock-free
/// histogram: the producer is a real-time thread (MMCSS "Pro Audio" on Windows, a HAL IOProc
/// worker on macOS) and must never allocate or lock to report.
#[derive(Debug, Default)]
pub struct AudioDiagnostics {
    /// Engine period in force, in frames: `IAudioClient::GetDevicePeriod` on Windows, not
    /// `GetBufferSize`, which is the whole engine buffer and several periods deep.
    pub period_frames: AtomicU32,
    /// The engine's total buffer capacity in frames (`GetBufferSize`).
    pub buffer_frames: AtomicU32,
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    pub discontinuity_count: AtomicU64,
    pub silent_count: AtomicU64,
    pub device_changes: AtomicU64,
    /// Times the mandatory silent render companion died and had to be restarted (Windows).
    pub companion_restarts: AtomicU64,
    /// Times the capture event went unsignalled long enough to be treated as a dead clock.
    pub event_starvations: AtomicU64,
    /// Packets a session's forwarder dropped because its DataChannel was backed up.
    pub dropped_backpressure: AtomicU64,
    /// Packets the broadcast dropped on a slow subscriber's behalf (`RecvError::Lagged`).
    pub lagged_packets: AtomicU64,
    /// Drains whose endpoint timestamp was in the future and was clamped to the host clock.
    /// Normal on a Windows loopback stream (`windows_utils::audio::loopback::drain`).
    pub capture_ts_clamped: AtomicU64,
    /// Opus encode duration per frame.
    pub encode: crate::streamer::telemetry::Histogram,
    /// Capture-thread wake-to-wake interval: the engine period as delivered, which shows
    /// whether a low-latency period request took effect.
    pub wake_interval: crate::streamer::telemetry::Histogram,
    /// `host_now_ns()` at hand-off to the transport minus the packet's `capture_ns`: the
    /// host-side half of the audio latency budget.
    pub capture_to_packet: crate::streamer::telemetry::Histogram,
    /// Capture tier in force, named by the backend at acquisition; empty until one reports.
    /// Not in [`Self::summary`], whose line each backend already extends. Atomic because the
    /// backend sets it from its real-time capture thread.
    tier: std::sync::atomic::AtomicPtr<&'static str>,
}

impl AudioDiagnostics {
    /// Hot path, once per encoded frame on the capture thread.
    #[inline]
    pub fn record_encode_ns(&self, ns: u64) {
        self.encode.record(ns);
    }

    /// Set at acquisition and re-acquisition, never per packet.
    pub fn set_tier(&self, tier: &'static &'static str) {
        let ptr = tier as *const &'static str as *mut &'static str;
        self.tier.store(ptr, Ordering::Relaxed);
    }

    pub fn tier(&self) -> &'static str {
        let ptr = self.tier.load(Ordering::Relaxed);
        if ptr.is_null() {
            return "";
        }
        // SAFETY: the only pointers ever stored come from `&'static &'static str` in `set_tier`,
        // so a non-null one is valid for the life of the program and never written through.
        unsafe { *ptr }
    }

    pub fn summary(&self) -> String {
        let enc = self.encode.snapshot();
        let wake = self.wake_interval.snapshot();
        let lat = self.capture_to_packet.snapshot();
        format!(
            "audio diag: fmt={}Hz x{}ch period={}fr buffer={}fr, encode={}ms, wake={}ms, \
             capture->packet={}ms (p50/p90/p99/max), discontinuity={}, silent={}, \
             dev_changes={}, companion_restarts={}, starvations={}, dc_drops={}, lagged={}, \
             ts_clamped={}",
            self.sample_rate.load(Ordering::Relaxed),
            self.channels.load(Ordering::Relaxed),
            self.period_frames.load(Ordering::Relaxed),
            self.buffer_frames.load(Ordering::Relaxed),
            enc.brief(),
            wake.brief(),
            lat.brief(),
            self.discontinuity_count.load(Ordering::Relaxed),
            self.silent_count.load(Ordering::Relaxed),
            self.device_changes.load(Ordering::Relaxed),
            self.companion_restarts.load(Ordering::Relaxed),
            self.event_starvations.load(Ordering::Relaxed),
            self.dropped_backpressure.load(Ordering::Relaxed),
            self.lagged_packets.load(Ordering::Relaxed),
            self.capture_ts_clamped.load(Ordering::Relaxed),
        )
    }
}

pub type AudioStopFn = Box<dyn FnOnce() + Send>;

/// Where a capture backend puts finished packets. `broadcast::Sender::send` is synchronous
/// and non-blocking (a slow receiver is told `Lagged`, there is no backpressure), so the
/// real-time capture thread calls it directly: a relay thread at default priority in
/// between is jitter the client has to buffer for.
#[derive(Clone)]
pub struct AudioSink {
    tx: broadcast::Sender<AudioPacket>,
}

impl AudioSink {
    pub fn new(tx: broadcast::Sender<AudioPacket>) -> Self {
        Self { tx }
    }

    /// Never blocks. "No receivers" is normal (every session can toggle its audio off) and is
    /// not a reason to tear capture down; the stop flag is.
    #[inline]
    pub fn send(&self, pkt: AudioPacket) {
        let _ = self.tx.send(pkt);
    }
}

pub struct AudioCapture {
    pub stop: AudioStopFn,
    pub format: AudioFormat,
    pub diagnostics: Arc<AudioDiagnostics>,
    pub lookahead_samples: i32,
}

/// Packets of fan-out buffer, at [`encoder::FRAME_MS`] each: 16 is 80 ms. A 10 ms engine
/// period delivers two 5 ms packets per wake, and a subscriber further behind than this is
/// holding stale audio it should drop, not catch up on.
const BROADCAST_CAPACITY: usize = 16;

struct Running {
    tx: broadcast::Sender<AudioPacket>,
    stop: Option<AudioStopFn>,
    format: AudioFormat,
    diagnostics: Arc<AudioDiagnostics>,
    lookahead_samples: i32,
}

#[derive(Default)]
struct HubInner {
    running: Option<Running>,
    refcount: usize,
    /// Set while a thread is inside a device open or close with the mutex released.
    busy: bool,
}

pub struct AudioSubscription {
    pub rx: broadcast::Receiver<AudioPacket>,
    pub format: AudioFormat,
    pub diagnostics: Arc<AudioDiagnostics>,
    pub lookahead_samples: i32,
}

/// One host-wide capture, fanned out to every session that wants audio.
///
/// The mutex guards only the refcount and the handle. Opening the OS capture device takes
/// tens to hundreds of milliseconds and closing it joins a real-time thread, so a thread
/// doing device work marks the hub `busy`, drops the guard, and re-takes the lock to publish
/// the result; everyone else waits on the condvar, not behind the device.
#[derive(Default)]
pub struct AudioHub {
    inner: Mutex<HubInner>,
    idle: Condvar,
}

impl std::fmt::Debug for AudioHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (running, refcount) = match self.inner.lock() {
            Ok(i) => (i.running.is_some(), i.refcount),
            Err(_) => (false, 0),
        };
        f.debug_struct("AudioHub")
            .field("running", &running)
            .field("refcount", &refcount)
            .finish()
    }
}

impl AudioHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Take the lock, waiting out any in-flight device open or close. Poison is recovered: the
    /// guarded state is a refcount and an `Option`, which a panicking holder cannot leave torn.
    fn lock_idle(&self) -> std::sync::MutexGuard<'_, HubInner> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while inner.busy {
            inner = self.idle.wait(inner).unwrap_or_else(|e| e.into_inner());
        }
        inner
    }

    pub fn subscribe(&self) -> Result<AudioSubscription> {
        let mut inner = self.lock_idle();

        if inner.running.is_none() {
            inner.busy = true;
            drop(inner);

            let (tx, _rx0) = broadcast::channel::<AudioPacket>(BROADCAST_CAPACITY);
            let started =
                crate::streamer::platform::start_audio_capture(AudioSink::new(tx.clone()));

            inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.busy = false;
            self.idle.notify_all();

            let cap = started?;
            crate::tprintln!(
                "audio: capture started ({}Hz x{}ch, encoder lookahead {} samples)",
                cap.format.sample_rate,
                cap.format.channels,
                cap.lookahead_samples
            );
            // `busy` kept every other opener out while the lock was released
            debug_assert!(
                inner.running.is_none(),
                "a second capture opened while busy"
            );
            if inner.running.is_some() {
                // release builds: stop the extra capture rather than leak it
                (cap.stop)();
            } else {
                inner.running = Some(Running {
                    tx,
                    stop: Some(cap.stop),
                    format: cap.format,
                    diagnostics: cap.diagnostics,
                    lookahead_samples: cap.lookahead_samples,
                });
            }
        }

        let running = inner
            .running
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("audio capture is not running"))?;
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

    pub fn unsubscribe(&self) {
        let mut inner = self.lock_idle();
        if inner.refcount == 0 {
            return;
        }
        inner.refcount -= 1;
        crate::tprintln!("audio: subscriber removed (refcount={})", inner.refcount);
        if inner.refcount != 0 {
            return;
        }
        let Some(mut running) = inner.running.take() else {
            return;
        };
        let Some(stop) = running.stop.take() else {
            return;
        };

        inner.busy = true;
        drop(inner);
        // joins the capture thread, outside the lock: a wedged backend must not stall the hub
        stop();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.busy = false;
        self.idle.notify_all();
        drop(inner);
        crate::tprintln!("audio: capture stopped (last subscriber left)");
    }

    pub fn is_running(&self) -> bool {
        match self.inner.lock() {
            Ok(i) => i.running.is_some(),
            Err(_) => false,
        }
    }

    pub fn diagnostics(&self) -> Option<Arc<AudioDiagnostics>> {
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.running.as_ref().map(|r| Arc::clone(&r.diagnostics)))
    }
}

pub type SharedAudioHub = Arc<AudioHub>;

pub mod encoder;
pub mod opus_sys;
pub mod protocol;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

/// What the capture path publishes about itself.
///
/// Every field is an atomic or a lock-free histogram, because the producer is a real-time
/// thread — MMCSS "Pro Audio" on Windows, a HAL IOProc worker on macOS — and must never
/// allocate or lock to report. The percentiles used to come from a 4096-element `Vec` that was
/// cloned and sorted on that thread every two seconds; `Histogram` gives the same numbers out
/// of a fixed array of atomics.
#[derive(Debug, Default)]
pub struct AudioDiagnostics {
    /// The engine period actually in force, in frames — `IAudioClient::GetDevicePeriod` on
    /// Windows, not `GetBufferSize`, which is the whole engine buffer and is several periods
    /// deep. This field carried the latter and was reported as if it were the former.
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
    /// Drains whose endpoint timestamp was in the future and had to be clamped to the host
    /// clock. On a Windows loopback stream this is the normal case, not an anomaly — see the
    /// comment at the anchor in `windows_utils::audio::loopback::drain`.
    pub capture_ts_clamped: AtomicU64,
    /// Opus encode duration per frame.
    pub encode: crate::streamer::telemetry::Histogram,
    /// Capture-thread wake-to-wake interval: the engine period as actually delivered, which is
    /// the number that says whether a low-latency period request took effect.
    pub wake_interval: crate::streamer::telemetry::Histogram,
    /// `host_now_ns()` when a packet is handed to the transport minus that packet's own
    /// `capture_ns` — the host-side half of the audio latency budget, end to end.
    pub capture_to_packet: crate::streamer::telemetry::Histogram,
}

impl AudioDiagnostics {
    /// Hot path, once per encoded frame on the capture thread.
    #[inline]
    pub fn record_encode_ns(&self, ns: u64) {
        self.encode.record(ns);
    }

    pub fn summary(&self) -> String {
        let enc = self.encode.snapshot();
        let wake = self.wake_interval.snapshot();
        let lat = self.capture_to_packet.snapshot();
        format!(
            "audio diag: fmt={}Hz x{}ch period={}fr buffer={}fr, encode={}ms, wake={}ms, \
             capture->packet={}ms (p50/p90/p99/max), discontinuity={}, silent={}, \
             dev_changes={}, companion_restarts={}, starvations={}, dc_drops={},              lagged={}, ts_clamped={}",
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

pub struct AudioSubscription {
    pub rx: broadcast::Receiver<AudioPacket>,
    pub format: AudioFormat,
    pub diagnostics: Arc<AudioDiagnostics>,
    pub lookahead_samples: i32,
}

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

    pub fn subscribe(&self) -> Result<AudioSubscription> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard.as_mut().expect("audio hub initialized");

        if inner.running.is_none() {
            let cap = crate::streamer::platform::start_audio_capture()?;
            let (tx, _rx0) = broadcast::channel::<AudioPacket>(BROADCAST_CAPACITY);

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

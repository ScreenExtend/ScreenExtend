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

/// Where a capture backend puts finished packets.
///
/// This used to be a `crossbeam_channel::Sender` whose receiver was drained by a dedicated
/// `audio-bridge` thread that did nothing but copy each packet into the broadcast. That hop
/// bought nothing: `tokio::sync::broadcast::Sender::send` is a synchronous, non-blocking call
/// — it takes the ring's tail lock, writes the slot and wakes waiters, and a slow receiver is
/// told `Lagged` rather than applying backpressure — so a real-time capture thread can call
/// it directly, exactly as it called the channel. The bridge also ran at default scheduler
/// priority between a Pro Audio thread and the network, which is the worst place in the path
/// for an unprioritised hop: every millisecond it was descheduled was jitter the client had
/// to absorb by holding a deeper buffer, so it cost latency twice.
#[derive(Clone)]
pub struct AudioSink {
    tx: broadcast::Sender<AudioPacket>,
}

impl AudioSink {
    pub fn new(tx: broadcast::Sender<AudioPacket>) -> Self {
        Self { tx }
    }

    /// Publish a packet. Never blocks, and never fails in a way the capture thread should act
    /// on: "no receivers right now" is normal — every session can toggle its audio off — and
    /// is not a reason to tear capture down. The stop flag is.
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

/// Packets of fan-out buffer, at [`encoder::FRAME_MS`] each.
///
/// This was 128 — **640 ms** of audio held for a subscriber that had stopped reading. A
/// subscriber that far behind is not recoverable on a real-time path: by the time it is
/// declared lagged it is already two thirds of a second late, and everything queued in
/// between is stale audio nobody wants. 16 packets is 80 ms, comfortably above the largest
/// burst the hardware produces (a 10 ms engine period delivers two 5 ms packets per wake, and
/// `probe --audio` measures subscriber inter-arrival p99 at one period) and below the point
/// where catching up stops being worth doing.
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
/// The refcounting is the easy part. The subtlety is *when the mutex is held*: opening the OS
/// capture device is tens to hundreds of milliseconds (a WASAPI client plus the mandatory
/// silent render companion on Windows; an aggregate device or an SCK stream on macOS) and
/// closing it joins a real-time thread. Both used to run under the mutex, so toggling audio on
/// one device stalled every other device's toggle — and a backend whose `stop` returned before
/// its producer had actually exited would have wedged the entire subsystem, `is_running` and
/// `diagnostics` included, with no way out but a restart.
///
/// The lock now guards only the refcount and the handle. A thread doing device work marks the
/// hub `busy`, drops the guard, does the work, and re-takes the lock to publish the result;
/// anyone else waits on the condvar rather than behind the device.
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

    /// Take the lock, waiting out any in-flight device open or close.
    ///
    /// A poisoned mutex is recovered rather than propagated. The guarded state is a refcount
    /// and an `Option`; a panicking holder cannot leave it torn, and killing audio for the
    /// rest of the process because one subscriber panicked is the worse outcome.
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
            if inner.running.is_some() {
                // Someone else won the race while the lock was released. Stop the loser's
                // capture rather than leaking it.
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
        // Joins the capture thread. Outside the lock, so a backend that is slow — or wedged —
        // on the way down cannot take the rest of the audio subsystem with it.
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

//! Lock-free, always-latest, drop-on-overflow frame hand-off (PRD §4.3–§4.4).
//!
//! This is the one piece of Part I that is pure, portable safe Rust, so it is
//! implemented in full here. The producer (the capture callback) never blocks;
//! the consumer (the encoder feed loop) parks until woken, then takes the
//! newest frame. Any frame the consumer did not grab is dropped — which, on
//! macOS, returns its backing `IOSurface` to WindowServer's pool immediately
//! (PRD invariant #2: "drop, don't buffer").
//!
//! The GPU-resident payload (the `IOSurface`-backed `CVPixelBuffer`) lives in
//! [`Frame::backing`]; [`Frame::pixel_buffer`] hands it straight to VideoToolbox
//! with zero copies (PRD §14.3).

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossbeam_channel::{Receiver, Sender, bounded};
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;
use objc2_io_surface::IOSurfaceRef;

/// GPU-resident frame backing (PRD §4.3): whatever must stay retained to keep
/// the `IOSurface`-backed pixels valid until the [`Frame`] is dropped.
///
/// For the streamer the canonical payload is the `IOSurface`-backed
/// `CVPixelBuffer` that VideoToolbox eats (PRD §14.3) — no Metal texture is
/// needed unless we add a local preview. The CGDisplayStream backend retains
/// the delivered `IOSurface` and wraps it once in a `CVPixelBuffer` (zero-copy:
/// the pixel buffer just references the surface).
pub enum Backing {
    /// Hand-off plumbing only (tests); carries no GPU memory.
    Empty,
    /// CGDisplayStream / SCK: retained surface + its zero-copy pixel-buffer
    /// wrapper. Both are released when the `Frame` drops, returning the surface
    /// to WindowServer's pool.
    PixelBuffer {
        pixbuf: CFRetained<CVPixelBuffer>,
        _surface: CFRetained<IOSurfaceRef>,
    },
}

/// A single captured frame: GPU-resident pixels plus the metadata the encoder
/// needs. Never CPU-touched (PRD invariant #1).
pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// `mach_absolute_time()` captured the instant the frame arrived in the
    /// capture callback (PRD §9). Drives the encoder PTS and the latency probe.
    pub arrived_mach: u64,
    /// Wall-clock `Instant` the frame arrived in the capture callback, in the same
    /// clock domain the encoded-frame consumer (`webrtc_session`) measures against.
    /// Carried through the encode FIFO so the emitted access unit's `capture` is
    /// the true capture time — driving accurate RTP frame `duration` (timestamp
    /// pacing) instead of the submit time it used to be stamped with.
    pub captured_at: std::time::Instant,
    /// Kept retained so the `IOSurface` backing stays alive until the consumer
    /// is done; dropped with the `Frame`.
    pub backing: Backing,
}

impl Frame {
    /// The `IOSurface`-backed `CVPixelBuffer` to hand straight to VideoToolbox
    /// (zero-copy). `None` for an `Empty` (test) frame.
    #[inline]
    pub fn pixel_buffer(&self) -> Option<&CVPixelBuffer> {
        match &self.backing {
            Backing::PixelBuffer { pixbuf, .. } => Some(pixbuf),
            Backing::Empty => None,
        }
    }
}

// The pixels are never read on the CPU, so there is no data race on contents;
// Metal/CF objects are safe to retain/release across threads (PRD §4.3).
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

/// Producer side, held by the capture callback. Lock-free `publish`.
pub struct FrameSink {
    latest: ArcSwapOption<Frame>,
    wake_tx: Sender<()>,
}

/// Consumer side, held by the encoder feed loop.
pub struct FrameSource {
    sink: Arc<FrameSink>,
    wake_rx: Receiver<()>,
}

/// Create a linked `(sink, source)` pair. The wakeup channel has capacity 1: it
/// is a pure "something changed" signal, never a queue of frames.
pub fn frame_channel() -> (Arc<FrameSink>, FrameSource) {
    let (wake_tx, wake_rx) = bounded(1);
    let sink = Arc::new(FrameSink { latest: ArcSwapOption::empty(), wake_tx });
    let source = FrameSource { sink: sink.clone(), wake_rx };
    (sink, source)
}

impl FrameSink {
    /// Called from the capture callback. Lock-free; at most one `Arc` alloc.
    ///
    /// Storing replaces (and drops) any previous unconsumed frame, which is the
    /// "drop old" policy: the old surface returns to the pool right away.
    #[inline]
    pub fn publish(&self, frame: Frame) {
        self.latest.store(Some(Arc::new(frame)));
        // Already-signaled is fine — the slot still holds the newest frame.
        let _ = self.wake_tx.try_send(());
    }
}

impl FrameSource {
    /// Block until a frame is available, then take the newest one. Returns
    /// `None` only if the wakeup channel is disconnected (capture stopped).
    pub fn next_blocking(&self) -> Option<Arc<Frame>> {
        self.wake_rx.recv().ok()?;
        self.sink.latest.swap(None)
    }

    /// Non-blocking peek+take, for a consumer driven by its own clock.
    pub fn try_take_latest(&self) -> Option<Arc<Frame>> {
        self.sink.latest.swap(None)
    }

    /// Peek whether a freshly-published frame is currently waiting in the slot,
    /// **without** taking it. Single-consumer, so a `true` here guarantees the
    /// next `try_take_latest` returns a frame at least this new.
    ///
    /// The encode loop uses this to decide whether the macOS 10.15 flush-copy is
    /// needed: if a newer frame is already queued, the next real submit will push
    /// the current frame out of VideoToolbox's one-frame hold for free, so the
    /// extra flush encode can be skipped (it would only double encoder/wire load
    /// under sustained motion).
    #[inline]
    pub fn peek_has_frame(&self) -> bool {
        self.sink.latest.load().is_some()
    }

    /// Park until a frame is published or `timeout` elapses; returns `true` if a
    /// publish woke us. Lets the encode loop react to a new frame within
    /// microseconds (no polling delay) while still ticking for keepalives.
    pub fn wait(&self, timeout: std::time::Duration) -> bool {
        self.wake_rx.recv_timeout(timeout).is_ok()
    }

    /// A clone of the wake sender so non-frame control events (a PLI/IDR request
    /// or a BWE bitrate update) can also fire the encode loop's park immediately,
    /// rather than waiting up to the idle tick to be noticed (2.2). The wake is a
    /// pure "re-check" signal — the loop re-reads the IDR/bitrate atomics on every
    /// iteration — so sharing the frame channel is correct.
    pub fn wake_sender(&self) -> Sender<()> {
        self.sink.wake_tx.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: u64) -> Frame {
        Frame {
            width: 1,
            height: 1,
            arrived_mach: tag,
            captured_at: std::time::Instant::now(),
            backing: Backing::Empty,
        }
    }

    #[test]
    fn keeps_only_the_latest_frame() {
        let (sink, source) = frame_channel();
        sink.publish(frame(1));
        sink.publish(frame(2));
        sink.publish(frame(3));
        let got = source.try_take_latest().expect("a frame");
        assert_eq!(got.arrived_mach, 3, "stale frames must be dropped");
        assert!(source.try_take_latest().is_none(), "slot drains to empty");
    }

    #[test]
    fn next_blocking_returns_after_publish() {
        let (sink, source) = frame_channel();
        sink.publish(frame(42));
        let got = source.next_blocking().expect("woken with a frame");
        assert_eq!(got.arrived_mach, 42);
    }
}

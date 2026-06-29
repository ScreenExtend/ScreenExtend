use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossbeam_channel::{Receiver, Sender, bounded};
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;
use objc2_io_surface::IOSurfaceRef;

pub enum Backing {
    Empty,
    PixelBuffer {
        pixbuf: CFRetained<CVPixelBuffer>,
        _surface: CFRetained<IOSurfaceRef>,
    },
}

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub arrived_mach: u64,
    pub captured_at: std::time::Instant,
    pub backing: Backing,
}

impl Frame {
    #[inline]
    pub fn pixel_buffer(&self) -> Option<&CVPixelBuffer> {
        match &self.backing {
            Backing::PixelBuffer { pixbuf, .. } => Some(pixbuf),
            Backing::Empty => None,
        }
    }
}

unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

pub struct FrameSink {
    latest: ArcSwapOption<Frame>,
    wake_tx: Sender<()>,
}

pub struct FrameSource {
    sink: Arc<FrameSink>,
    wake_rx: Receiver<()>,
}

pub fn frame_channel() -> (Arc<FrameSink>, FrameSource) {
    let (wake_tx, wake_rx) = bounded(1);
    let sink = Arc::new(FrameSink { latest: ArcSwapOption::empty(), wake_tx });
    let source = FrameSource { sink: sink.clone(), wake_rx };
    (sink, source)
}

impl FrameSink {
    #[inline]
    pub fn publish(&self, frame: Frame) {
        self.latest.store(Some(Arc::new(frame)));
        let _ = self.wake_tx.try_send(());
    }
}

impl FrameSource {
    pub fn next_blocking(&self) -> Option<Arc<Frame>> {
        self.wake_rx.recv().ok()?;
        self.sink.latest.swap(None)
    }

    pub fn try_take_latest(&self) -> Option<Arc<Frame>> {
        self.sink.latest.swap(None)
    }

    #[inline]
    pub fn peek_has_frame(&self) -> bool {
        self.sink.latest.load().is_some()
    }

    pub fn wait(&self, timeout: std::time::Duration) -> bool {
        self.wake_rx.recv_timeout(timeout).is_ok()
    }

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

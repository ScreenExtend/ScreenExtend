//! Lock-free SPSC ring buffer of interleaved-stereo f32 samples — the real-time hand-off from
//! the audio capture callback to the encoder thread (PRD §9.2).
//!
//! The producer is the Core Audio `AudioDeviceIOProc` (Process Tap) or the ScreenCaptureKit
//! `stream:didOutputSampleBuffer:ofType:` handler — real-time audio-thread code that must never
//! allocate, lock, or block. It only [`Producer::push`]es already-converted samples; on overrun
//! it drops the newest samples and bumps a counter rather than blocking. The consumer is the
//! encoder thread, which [`Consumer::pop`]s whole Opus frames. Single producer, single consumer,
//! so a plain pair of monotonic atomic counters over a fixed power-of-two buffer suffices — no
//! CAS loop, no lock, no allocation after construction.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

struct Shared {
    /// `capacity` (a power of two) cells; only the producer writes, only the consumer reads.
    buf: Box<[UnsafeCell<f32>]>,
    /// `capacity - 1`, for wrapping a monotonic counter into an index.
    mask: usize,
    /// Next slot the producer will write (monotonic, wraps naturally at usize::MAX).
    head: AtomicUsize,
    /// Next slot the consumer will read (monotonic).
    tail: AtomicUsize,
    /// Samples dropped because the ring was full (surfaced in diagnostics).
    overruns: AtomicU64,
}

// SAFETY: the `UnsafeCell<f32>` cells are only ever touched by exactly one thread at a time — the
// producer writes a cell strictly before it publishes `head` (Release), and the consumer reads a
// cell only after it observes that `head` (Acquire) and strictly before it publishes `tail`. So
// no cell is aliased across threads without the happens-before edge the atomics establish.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Producer half — held by the real-time capture callback.
pub struct Producer {
    shared: Arc<Shared>,
    /// Handle to the consumer (encoder) thread, set once via [`Producer::set_consumer_thread`].
    /// After a successful push the producer signals this handle (wait-free futex), eliminating
    /// the busy-spin on the consumer side. RT-safe: `unpark` never allocates or blocks.
    consumer_thread: Arc<OnceLock<std::thread::Thread>>,
}

/// Consumer half — held by the encoder thread.
pub struct Consumer {
    shared: Arc<Shared>,
}

/// Create an SPSC f32 ring holding `capacity` samples (rounded up to a power of two, min 2).
///
/// Returns `(producer, consumer, consumer_thread_handle)`. The caller should pass
/// `consumer_thread_handle` to [`Producer::set_consumer_thread`] from the consumer thread once
/// it has started, enabling event-driven wakeup instead of a timed park.
pub fn ring(capacity: usize) -> (Producer, Consumer, Arc<OnceLock<std::thread::Thread>>) {
    let cap = capacity.next_power_of_two().max(2);
    let mut cells = Vec::with_capacity(cap);
    cells.resize_with(cap, || UnsafeCell::new(0.0f32));
    let shared = Arc::new(Shared {
        buf: cells.into_boxed_slice(),
        mask: cap - 1,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        overruns: AtomicU64::new(0),
    });
    let consumer_thread: Arc<OnceLock<std::thread::Thread>> = Arc::new(OnceLock::new());
    (
        Producer {
            shared: Arc::clone(&shared),
            consumer_thread: Arc::clone(&consumer_thread),
        },
        Consumer { shared },
        consumer_thread,
    )
}

impl Producer {
    /// Register the consumer thread so that [`push`] can unpark it after each write.
    /// Call this once from the consumer thread itself (before entering the drain loop) so
    /// `std::thread::current()` captures the right handle. Wait-free on subsequent calls
    /// (OnceLock ignores them).
    pub fn set_consumer_thread(&self, t: std::thread::Thread) {
        self.consumer_thread.get_or_init(|| t);
    }

    /// Push as many of `data`'s samples as fit. Never blocks. Any samples that don't fit are
    /// dropped and added to the overrun counter (returns the number dropped). Real-time safe:
    /// no allocation, no locking.
    ///
    /// After a successful (non-zero) write the consumer thread is unparked via a wait-free futex
    /// signal, so the consumer does not need to spin between frames.
    pub fn push(&self, data: &[f32]) -> usize {
        let s = &*self.shared;
        let head = s.head.load(Ordering::Relaxed);
        let tail = s.tail.load(Ordering::Acquire);
        let free = s.buf.len() - head.wrapping_sub(tail);
        let n = data.len().min(free);
        for (i, &sample) in data.iter().take(n).enumerate() {
            let idx = head.wrapping_add(i) & s.mask;
            // SAFETY: this cell is in the free region [head, head+free); the consumer will not
            // read it until we publish the advanced `head` below.
            unsafe {
                *s.buf[idx].get() = sample;
            }
        }
        s.head.store(head.wrapping_add(n), Ordering::Release);
        // Wake the consumer thread (wait-free futex signal; RT-safe — no allocation, no blocking).
        if n > 0 {
            if let Some(t) = self.consumer_thread.get() {
                t.unpark();
            }
        }
        let dropped = data.len() - n;
        if dropped > 0 {
            s.overruns.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        dropped
    }

    /// Total samples ever dropped to overrun.
    pub fn overruns(&self) -> u64 {
        self.shared.overruns.load(Ordering::Relaxed)
    }
}

impl Consumer {
    /// Samples currently available to read.
    pub fn available(&self) -> usize {
        let s = &*self.shared;
        let head = s.head.load(Ordering::Acquire);
        let tail = s.tail.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
    }

    /// Pop up to `out.len()` samples into `out`; returns how many were written.
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let s = &*self.shared;
        let tail = s.tail.load(Ordering::Relaxed);
        let head = s.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail);
        let n = out.len().min(avail);
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            let idx = tail.wrapping_add(i) & s.mask;
            // SAFETY: index is in the readable region [tail, head); the producer will not
            // overwrite it until we publish the advanced `tail` below.
            *slot = unsafe { *s.buf[idx].get() };
        }
        s.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    pub fn overruns(&self) -> u64 {
        self.shared.overruns.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_in_order() {
        let (p, c, _t) = ring(8);
        assert_eq!(p.push(&[1.0, 2.0, 3.0]), 0);
        let mut out = [0.0f32; 4];
        assert_eq!(c.pop(&mut out), 3);
        assert_eq!(&out[..3], &[1.0, 2.0, 3.0]);
        assert_eq!(c.pop(&mut out), 0);
    }

    #[test]
    fn wraps_around_capacity() {
        let (p, c, _t) = ring(4); // capacity 4
        let mut out = [0.0f32; 4];
        for round in 0..10 {
            let a = round as f32;
            assert_eq!(p.push(&[a, a + 0.5]), 0);
            assert_eq!(c.pop(&mut out[..2]), 2);
            assert_eq!(&out[..2], &[a, a + 0.5]);
        }
    }

    #[test]
    fn counts_overruns_without_blocking() {
        let (p, c, _t) = ring(4); // rounds up to 4 usable slots
                                  // Push 6 into a 4-slot ring: 4 fit, 2 dropped.
        let dropped = p.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(dropped, 2);
        assert_eq!(p.overruns(), 2);
        assert_eq!(c.available(), 4);
    }
}

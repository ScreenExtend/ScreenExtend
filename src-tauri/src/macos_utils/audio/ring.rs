use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

struct Shared {
    buf: Box<[UnsafeCell<f32>]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
    overruns: AtomicU64,
}

unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// producer half, held by the RT capture callback
pub struct Producer {
    shared: Arc<Shared>,
}

/// consumer half, held by the encoder thread
pub struct Consumer {
    shared: Arc<Shared>,
}

pub fn ring(capacity: usize) -> (Producer, Consumer) {
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
    (
        Producer {
            shared: Arc::clone(&shared),
        },
        Consumer { shared },
    )
}

impl Producer {
    /// Write what fits and return how many samples had to be dropped.
    ///
    /// Every operation here is wait-free: this runs on a CoreAudio IOProc or the SCK delivery
    /// queue, where a blocking mutex acquisition against a lower-priority thread is a priority
    /// inversion. In particular there is deliberately no consumer wakeup — the encoder worker
    /// polls with a 125 us `park_timeout`, a quarter of one 5 ms Opus frame.
    pub fn push(&self, data: &[f32]) -> usize {
        let s = &*self.shared;
        let head = s.head.load(Ordering::Relaxed);
        let tail = s.tail.load(Ordering::Acquire);
        let free = s.buf.len() - head.wrapping_sub(tail);
        let n = data.len().min(free);
        for (i, &sample) in data.iter().take(n).enumerate() {
            let idx = head.wrapping_add(i) & s.mask;
            // SAFETY: this cell is in the free region [head, head+free)
            unsafe {
                *s.buf[idx].get() = sample;
            }
        }
        s.head.store(head.wrapping_add(n), Ordering::Release);
        let dropped = data.len() - n;
        if dropped > 0 {
            s.overruns.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        dropped
    }

    pub fn overruns(&self) -> u64 {
        self.shared.overruns.load(Ordering::Relaxed)
    }
}

impl Consumer {
    pub fn available(&self) -> usize {
        let s = &*self.shared;
        let head = s.head.load(Ordering::Acquire);
        let tail = s.tail.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
    }

    pub fn pop(&self, out: &mut [f32]) -> usize {
        let s = &*self.shared;
        let tail = s.tail.load(Ordering::Relaxed);
        let head = s.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail);
        let n = out.len().min(avail);
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            let idx = tail.wrapping_add(i) & s.mask;
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
        let (p, c) = ring(8);
        assert_eq!(p.push(&[1.0, 2.0, 3.0]), 0);
        let mut out = [0.0f32; 4];
        assert_eq!(c.pop(&mut out), 3);
        assert_eq!(&out[..3], &[1.0, 2.0, 3.0]);
        assert_eq!(c.pop(&mut out), 0);
    }

    #[test]
    fn wraps_around_capacity() {
        let (p, c) = ring(4);
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
        let (p, c) = ring(4);
        // 6 into a 4-slot ring: 4 fit, 2 dropped
        let dropped = p.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(dropped, 2);
        assert_eq!(p.overruns(), 2);
        assert_eq!(c.available(), 4);
    }
}

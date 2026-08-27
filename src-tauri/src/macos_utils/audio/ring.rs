use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

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
    consumer_thread: Arc<OnceLock<std::thread::Thread>>,
}

/// consumer half, held by the encoder thread
pub struct Consumer {
    shared: Arc<Shared>,
}

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
    pub fn set_consumer_thread(&self, t: std::thread::Thread) {
        self.consumer_thread.get_or_init(|| t);
    }

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
        let (p, c, _t) = ring(8);
        assert_eq!(p.push(&[1.0, 2.0, 3.0]), 0);
        let mut out = [0.0f32; 4];
        assert_eq!(c.pop(&mut out), 3);
        assert_eq!(&out[..3], &[1.0, 2.0, 3.0]);
        assert_eq!(c.pop(&mut out), 0);
    }

    #[test]
    fn wraps_around_capacity() {
        let (p, c, _t) = ring(4);
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
        let (p, c, _t) = ring(4);
        // 6 into a 4-slot ring: 4 fit, 2 dropped
        let dropped = p.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(dropped, 2);
        assert_eq!(p.overruns(), 2);
        assert_eq!(c.available(), 4);
    }
}

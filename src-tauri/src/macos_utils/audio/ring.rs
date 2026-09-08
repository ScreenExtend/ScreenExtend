use std::cell::UnsafeCell;
use std::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

struct Shared {
    buf: Box<[UnsafeCell<f32>]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
    overruns: AtomicU64,
    // seqlock-published capture-time anchor; see `Anchor`
    anchor_seq: AtomicU64,
    anchor_pos: AtomicU64,
    anchor_ns: AtomicU64,
}

unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Host-clock time of one identified sample in the stream.
///
/// Every capture callback is handed a hardware timestamp for the block it delivers. Publishing that
/// alongside the absolute ring index the block starts at lets the encoder worker date its packets
/// from when the samples were *captured* rather than from when it woke up to encode them. The pair
/// is guarded by a seqlock, not a mutex, because the writer is a real-time audio callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// absolute index, in ring samples, of the sample this time refers to
    pub pos: u64,
    /// `streamer::audio::host_now_ns()`-domain time of that sample
    pub host_ns: u64,
}

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
        anchor_seq: AtomicU64::new(0),
        anchor_pos: AtomicU64::new(0),
        anchor_ns: AtomicU64::new(0),
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
        self.write(data)
    }

    /// Push a block whose first sample was captured at `host_ns`, publishing that as the anchor.
    ///
    /// `host_ns == 0` means the backend had no usable hardware stamp for this block; the previous
    /// anchor is then left in place rather than replaced with a guess.
    pub fn push_at(&self, data: &[f32], host_ns: u64) -> usize {
        if host_ns != 0 {
            let s = &*self.shared;
            let head = s.head.load(Ordering::Relaxed) as u64;
            let seq = s.anchor_seq.load(Ordering::Relaxed);
            // seqlock write: an odd sequence marks the pair as in flux
            s.anchor_seq.store(seq.wrapping_add(1), Ordering::Relaxed);
            fence(Ordering::Release);
            s.anchor_pos.store(head, Ordering::Relaxed);
            s.anchor_ns.store(host_ns, Ordering::Relaxed);
            s.anchor_seq.store(seq.wrapping_add(2), Ordering::Release);
        }
        self.write(data)
    }

    fn write(&self, data: &[f32]) -> usize {
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

    /// Absolute index of the next sample [`Consumer::pop`] will return.
    pub fn read_pos(&self) -> u64 {
        self.shared.tail.load(Ordering::Relaxed) as u64
    }

    /// The most recently published capture-time anchor.
    ///
    /// `None` if the producer has never published one, or if the seqlock was caught mid-write on
    /// every attempt — the caller falls back to its own clock for that frame.
    pub fn anchor(&self) -> Option<Anchor> {
        let s = &*self.shared;
        for _ in 0..4 {
            let before = s.anchor_seq.load(Ordering::Acquire);
            if before == 0 {
                return None; // never published
            }
            if before & 1 != 0 {
                continue; // write in progress
            }
            let pos = s.anchor_pos.load(Ordering::Relaxed);
            let host_ns = s.anchor_ns.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if s.anchor_seq.load(Ordering::Relaxed) == before {
                return Some(Anchor { pos, host_ns });
            }
        }
        None
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

    #[test]
    fn no_anchor_until_one_is_published() {
        let (p, c) = ring(8);
        assert_eq!(c.anchor(), None);
        p.push(&[1.0, 2.0]);
        assert_eq!(c.anchor(), None);
    }

    #[test]
    fn an_anchor_names_the_first_sample_of_its_block() {
        let (p, c) = ring(64);
        p.push_at(&[1.0, 2.0], 7_000);
        assert_eq!(
            c.anchor(),
            Some(Anchor {
                pos: 0,
                host_ns: 7_000
            })
        );
        p.push_at(&[3.0, 4.0], 9_000);
        assert_eq!(
            c.anchor(),
            Some(Anchor {
                pos: 2,
                host_ns: 9_000
            })
        );
    }

    #[test]
    fn a_block_without_a_stamp_leaves_the_anchor_alone() {
        let (p, c) = ring(64);
        p.push_at(&[1.0, 2.0], 7_000);
        p.push_at(&[3.0, 4.0], 0);
        assert_eq!(
            c.anchor(),
            Some(Anchor {
                pos: 0,
                host_ns: 7_000
            })
        );
    }

    #[test]
    fn read_pos_tracks_what_has_been_popped() {
        let (p, c) = ring(64);
        p.push(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(c.read_pos(), 0);
        let mut out = [0.0f32; 3];
        c.pop(&mut out);
        assert_eq!(c.read_pos(), 3);
    }
}

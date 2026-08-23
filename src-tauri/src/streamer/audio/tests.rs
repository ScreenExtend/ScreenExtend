//! Mock capture source + jitter-buffer-policy tests (PRD §9).
//!
//! The real playback jitter buffer runs in `static/audio-worklet.js` (there's no JS test
//! harness in this repo); [`SeqGate`] below is the reference model for the client's reorder /
//! duplicate / late-drop policy over the unordered DataChannel, unit-tested here so the
//! algorithm is pinned. The mock capture lets the transport be exercised without audio
//! hardware, in the spirit of `windows_utils/driver_ipc/mock.rs`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use bytes::Bytes;

use super::{
    host_now_ns, AudioCapture, AudioDiagnostics, AudioFormat, AudioPacket, FLAG_DISCONTINUITY,
};

/// Build a mock [`AudioCapture`] that emits `n` synthetic packets immediately (no hardware, no
/// libopus), then closes the channel. Sequence numbers increment from 0; the first carries
/// `FLAG_DISCONTINUITY` (fresh timeline), matching the real capture thread.
fn mock_capture(n: u32) -> AudioCapture {
    let (tx, rx) = crossbeam_channel::unbounded::<AudioPacket>();
    let diagnostics = Arc::new(AudioDiagnostics::default());
    diagnostics.sample_rate.store(48000, Ordering::Relaxed);
    diagnostics.channels.store(2, Ordering::Relaxed);

    let join = std::thread::spawn(move || {
        for seq in 0..n {
            let flags = if seq == 0 { FLAG_DISCONTINUITY } else { 0 };
            let pkt = AudioPacket {
                seq,
                capture_ns: host_now_ns(),
                flags,
                // 5 ms @ 128 kbps ≈ 80 bytes; contents don't matter for transport tests.
                data: Bytes::from(vec![0xA5u8; 80]),
            };
            if tx.send(pkt).is_err() {
                break;
            }
        }
    });

    let mut join = Some(join);
    AudioCapture {
        rx,
        stop: Box::new(move || {
            if let Some(j) = join.take() {
                let _ = j.join();
            }
        }),
        format: AudioFormat {
            sample_rate: 48000,
            channels: 2,
        },
        diagnostics,
        lookahead_samples: 120,
    }
}

#[test]
fn mock_capture_emits_expected_packets() {
    let cap = mock_capture(5);
    let mut seqs = Vec::new();
    let mut first_flags = None;
    while let Ok(pkt) = cap.rx.recv() {
        if first_flags.is_none() {
            first_flags = Some(pkt.flags);
        }
        seqs.push(pkt.seq);
    }
    (cap.stop)();
    assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
    assert_eq!(first_flags, Some(FLAG_DISCONTINUITY));
    assert_eq!(cap.format.sample_rate, 48000);
    assert_eq!(cap.format.channels, 2);
}

#[test]
fn mock_capture_stop_is_idempotent_after_drain() {
    let cap = mock_capture(3);
    let got: Vec<u32> = cap.rx.iter().map(|p| p.seq).collect();
    assert_eq!(got.len(), 3);
    (cap.stop)(); // joins the producer thread; must not panic
}

// --- Reference jitter/reorder policy (mirrors static/audio-worklet.js + audio.js) ----------

/// Accept a packet only if its sequence number is newer than the last one played, tolerating
/// u32 wraparound. Duplicates and late (reordered-behind) packets are dropped — "late audio is
/// worse than missing audio" (§6.1/§6.2).
struct SeqGate {
    last: Option<u32>,
    dropped_late: u64,
    dropped_dup: u64,
    accepted: u64,
}

impl SeqGate {
    fn new() -> Self {
        Self {
            last: None,
            dropped_late: 0,
            dropped_dup: 0,
            accepted: 0,
        }
    }

    /// Newer means `(seq - last)` wraps forward within half the u32 space.
    fn accept(&mut self, seq: u32) -> bool {
        match self.last {
            None => {
                self.last = Some(seq);
                self.accepted += 1;
                true
            }
            Some(last) if seq == last => {
                self.dropped_dup += 1;
                false
            }
            Some(last) => {
                let newer = seq.wrapping_sub(last) < u32::MAX / 2;
                if newer {
                    self.last = Some(seq);
                    self.accepted += 1;
                    true
                } else {
                    self.dropped_late += 1;
                    false
                }
            }
        }
    }
}

#[test]
fn seqgate_accepts_in_order() {
    let mut g = SeqGate::new();
    for s in 0..10 {
        assert!(g.accept(s), "in-order seq {s} should be accepted");
    }
    assert_eq!(g.accepted, 10);
    assert_eq!(g.dropped_late + g.dropped_dup, 0);
}

#[test]
fn seqgate_drops_duplicates() {
    let mut g = SeqGate::new();
    assert!(g.accept(5));
    assert!(!g.accept(5), "duplicate seq must be dropped");
    assert_eq!(g.dropped_dup, 1);
}

#[test]
fn seqgate_drops_late_reordered() {
    let mut g = SeqGate::new();
    assert!(g.accept(10));
    assert!(g.accept(11));
    assert!(
        !g.accept(9),
        "a packet older than the last played must be dropped"
    );
    assert!(g.accept(12));
    assert_eq!(g.dropped_late, 1);
    assert_eq!(g.accepted, 3);
}

#[test]
fn seqgate_handles_forward_reorder_gaps() {
    // A gap (missing 6,7) is fine — we don't stall waiting; 8 is newer than 5, accept it.
    let mut g = SeqGate::new();
    assert!(g.accept(5));
    assert!(g.accept(8));
    assert_eq!(g.accepted, 2);
}

#[test]
fn seqgate_tolerates_u32_wraparound() {
    let mut g = SeqGate::new();
    assert!(g.accept(u32::MAX - 1));
    assert!(g.accept(u32::MAX));
    assert!(g.accept(0), "wraparound to 0 is newer, not late");
    assert!(g.accept(1));
    assert!(!g.accept(u32::MAX), "pre-wrap seq is now late");
    assert_eq!(g.accepted, 4);
    assert_eq!(g.dropped_late, 1);
}

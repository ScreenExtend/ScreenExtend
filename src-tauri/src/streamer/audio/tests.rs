use std::sync::atomic::Ordering;
use std::sync::Arc;

use bytes::Bytes;

use super::{
    host_now_ns, AudioCapture, AudioDiagnostics, AudioFormat, AudioPacket, AudioSink,
    BROADCAST_CAPACITY, FLAG_DISCONTINUITY,
};

/// A capture backend that publishes `n` packets into `sink` and then stops, shaped exactly
/// like the real ones: it owns a thread, it writes through the sink, and its `stop` joins.
fn mock_capture(sink: AudioSink, n: u32) -> AudioCapture {
    let diagnostics = Arc::new(AudioDiagnostics::default());
    diagnostics.sample_rate.store(48000, Ordering::Relaxed);
    diagnostics.channels.store(2, Ordering::Relaxed);

    let join = std::thread::spawn(move || {
        for seq in 0..n {
            let flags = if seq == 0 { FLAG_DISCONTINUITY } else { 0 };
            sink.send(AudioPacket {
                seq,
                capture_ns: host_now_ns(),
                flags,
                data: Bytes::from(vec![0xA5u8; 80]),
            });
        }
    });

    let mut join = Some(join);
    AudioCapture {
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
    let (tx, mut rx) = tokio::sync::broadcast::channel::<AudioPacket>(BROADCAST_CAPACITY);
    let cap = mock_capture(AudioSink::new(tx), 5);
    let mut seqs = Vec::new();
    let mut first_flags = None;
    for _ in 0..5 {
        let pkt = rx.blocking_recv().expect("packet");
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
fn capture_survives_having_no_subscribers() {
    // A session toggling audio off drops the last receiver. That is normal, and it must not
    // stop the host-wide capture: `AudioSink::send` swallows the "no receivers" error and the
    // stop flag remains the only teardown path.
    let (tx, rx) = tokio::sync::broadcast::channel::<AudioPacket>(BROADCAST_CAPACITY);
    drop(rx);
    let cap = mock_capture(AudioSink::new(tx), 64);
    (cap.stop)(); // joins the producer thread; must not panic or hang
}

#[test]
fn a_slow_subscriber_is_lagged_not_backpressured() {
    // The producer must never block on a reader that has stopped reading. Publish more than
    // the ring holds and confirm the writer finished and the reader is told it lost packets.
    let (tx, mut rx) = tokio::sync::broadcast::channel::<AudioPacket>(BROADCAST_CAPACITY);
    let cap = mock_capture(AudioSink::new(tx), BROADCAST_CAPACITY as u32 * 4);
    (cap.stop)(); // returns only if the producer never blocked
    assert!(matches!(
        rx.blocking_recv(),
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
}

// The five `seqgate_*` tests that used to live here exercised a `SeqGate` struct defined in
// this file and used nowhere else. The real sequence gate is the client's, in
// `streamer/static/audio-worker.js`, and the two had already drifted: the Rust copy treated a
// gap of exactly 0x7FFF_FFFF as late where the JS treats it as newer. A mirror nothing derives
// from the implementation is five green tests describing behaviour that may no longer exist,
// so they were moved to `scripts/test-client-audio.mjs`, which drives the shipped file.
//
// The host does not dedup: it is the producer. If a host-side gate is ever wanted, it belongs
// in `streamer/audio/` as production code, with these tests over that.

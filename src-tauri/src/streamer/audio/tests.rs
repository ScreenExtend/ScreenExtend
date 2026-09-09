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

// ---- the shared host clock: the one number audio and video both ride --------------------
//
// `host_ns_to_rtp90k` is the whole A/V sync contract on the host side. The video track is a raw
// `TrackLocalStaticRTP` — we packetize H.264 ourselves — precisely so each frame's RTP timestamp
// carries this conversion of the same monotonic epoch the audio `capture_ns` uses, and
// `transform-worker.js` inverts it on the client. A bug here is drift nobody can localise,
// because both halves still look internally consistent.

#[test]
fn the_rtp_clock_is_90_khz_and_the_conversion_is_exact_at_whole_ticks() {
    use super::{host_ns_to_rtp90k, VIDEO_RTP_CLOCK_HZ};
    assert_eq!(VIDEO_RTP_CLOCK_HZ, 90_000);
    assert_eq!(host_ns_to_rtp90k(0), 0);
    assert_eq!(host_ns_to_rtp90k(1_000_000_000), 90_000, "one second");
    assert_eq!(host_ns_to_rtp90k(1_000_000), 90, "one millisecond");
    // A 60 Hz frame is 16666666.67 ns, so the conversion lands one tick short of 1500 at the
    // truncated interval and on it at the real one. Both spellings appear in real timestamps.
    assert_eq!(host_ns_to_rtp90k(16_666_666), 1_499);
    assert_eq!(host_ns_to_rtp90k(16_666_667), 1_500);
    // One tick is 11111.11 ns. Sub-tick nanoseconds truncate rather than rounding, which is
    // what keeps the conversion monotonic — a rounding step would let two later stamps come
    // back in the wrong order.
    assert_eq!(host_ns_to_rtp90k(11_110), 0);
    assert_eq!(host_ns_to_rtp90k(11_112), 1);
}

#[test]
fn the_conversion_never_goes_backwards() {
    use super::host_ns_to_rtp90k;
    // Walk a millisecond cadence for ten seconds and require the 32-bit difference between
    // consecutive stamps to be the interval every time. A millisecond is a whole number of
    // ticks, so any deviation is a conversion bug rather than the truncation.
    const STEP_NS: u64 = 1_000_000;
    const STEP_TICKS: u32 = 90;
    let mut previous = host_ns_to_rtp90k(0);
    for i in 1..=10_000u64 {
        let now = host_ns_to_rtp90k(i * STEP_NS);
        assert_eq!(
            now.wrapping_sub(previous),
            STEP_TICKS,
            "step {i} advanced by {} ticks",
            now.wrapping_sub(previous)
        );
        previous = now;
    }
}

#[test]
fn the_32_bit_stamp_wraps_after_about_13_hours_of_host_uptime() {
    use super::{host_ns_to_rtp90k, VIDEO_RTP_CLOCK_HZ};
    // 2^32 ticks at 90 kHz is 47721.858 s. The client unwraps this with a signed 32-bit
    // difference; the host's only job is to keep the truncation clean so that difference stays
    // meaningful, which is what these three assertions check.
    const WRAP_NS: u128 = (1u128 << 32) * 1_000_000_000 / VIDEO_RTP_CLOCK_HZ as u128;
    let wrap_ns = WRAP_NS as u64;
    assert!(
        (47_720..47_723).contains(&(wrap_ns / 1_000_000_000)),
        "the wrap should be ~13.25 h, got {} s",
        wrap_ns / 1_000_000_000
    );

    // A stamp 10 ms either side of the wrap.
    let before = host_ns_to_rtp90k(wrap_ns - 10_000_000);
    let after = host_ns_to_rtp90k(wrap_ns + 10_000_000);
    assert!(before > u32::MAX - 10_000, "before the wrap: {before}");
    assert!(after < 10_000, "after the wrap: {after}");
    // Wrapping arithmetic still gives the real elapsed ticks, which is the property the client
    // relies on: 20 ms is 1800 ticks, +/- one from the truncation of the wrap point itself.
    let elapsed = after.wrapping_sub(before);
    assert!(
        (1_799..=1_801).contains(&elapsed),
        "the wrap broke the difference: {elapsed} ticks"
    );
}

#[test]
fn the_host_clock_is_monotonic_and_shared() {
    use super::{host_instant_to_ns, host_now_ns};
    let a = host_now_ns();
    let b = host_now_ns();
    assert!(b >= a, "host_now_ns went backwards: {a} then {b}");

    // `host_instant_to_ns` has to be the same epoch, or an audio packet stamped from a captured
    // `Instant` and a video frame stamped from `host_now_ns()` describe two different clocks.
    let now = std::time::Instant::now();
    let from_instant = host_instant_to_ns(now);
    let direct = host_now_ns();
    assert!(
        direct >= from_instant,
        "an Instant taken before host_now_ns() converted to a later stamp: {from_instant} vs {direct}"
    );
    assert!(
        direct - from_instant < 1_000_000_000,
        "the two conversions are a second apart, so they are not one epoch"
    );

    // An Instant from before the epoch saturates rather than underflowing into a huge stamp.
    let long_ago = now - std::time::Duration::from_secs(60 * 60 * 24 * 365);
    assert_eq!(host_instant_to_ns(long_ago), 0);
}

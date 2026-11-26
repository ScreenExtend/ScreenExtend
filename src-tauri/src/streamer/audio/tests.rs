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
    // a session toggling audio off drops the last receiver, which must not stop the
    // host-wide capture: the stop flag is the only teardown path
    let (tx, rx) = tokio::sync::broadcast::channel::<AudioPacket>(BROADCAST_CAPACITY);
    drop(rx);
    let cap = mock_capture(AudioSink::new(tx), 64);
    (cap.stop)(); // joins the producer thread; must not panic or hang
}

#[test]
fn a_slow_subscriber_is_lagged_not_backpressured() {
    // more than the ring holds: the writer must finish and the reader be told it lagged
    let (tx, mut rx) = tokio::sync::broadcast::channel::<AudioPacket>(BROADCAST_CAPACITY);
    let cap = mock_capture(AudioSink::new(tx), BROADCAST_CAPACITY as u32 * 4);
    (cap.stop)(); // returns only if the producer never blocked
    assert!(matches!(
        rx.blocking_recv(),
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
}

// no sequence-gate tests here: the host is the producer and does not dedup. The gate is the
// client's (`streamer/static/audio-worker.js`), tested by `scripts/test-client-audio.mjs`.

// `host_ns_to_rtp90k` is the host side of the A/V sync contract: video RTP timestamps and
// audio `capture_ns` ride the same monotonic epoch, and `transform-worker.js` inverts it.

#[test]
fn the_rtp_clock_is_90_khz_and_the_conversion_is_exact_at_whole_ticks() {
    use super::{host_ns_to_rtp90k, VIDEO_RTP_CLOCK_HZ};
    assert_eq!(VIDEO_RTP_CLOCK_HZ, 90_000);
    assert_eq!(host_ns_to_rtp90k(0), 0);
    assert_eq!(host_ns_to_rtp90k(1_000_000_000), 90_000, "one second");
    assert_eq!(host_ns_to_rtp90k(1_000_000), 90, "one millisecond");
    // a 60 Hz frame is 16666666.67 ns: one tick short of 1500 truncated, 1500 rounded up
    assert_eq!(host_ns_to_rtp90k(16_666_666), 1_499);
    assert_eq!(host_ns_to_rtp90k(16_666_667), 1_500);
    // one tick is 11111.11 ns; sub-tick ns truncate, which keeps the conversion monotonic
    assert_eq!(host_ns_to_rtp90k(11_110), 0);
    assert_eq!(host_ns_to_rtp90k(11_112), 1);
}

#[test]
fn the_conversion_never_goes_backwards() {
    use super::host_ns_to_rtp90k;
    // a millisecond is a whole number of ticks, so any deviation over 10 s is a conversion bug
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
    // 2^32 ticks at 90 kHz is 47721.858 s; the client unwraps with a signed 32-bit difference
    const WRAP_NS: u128 = (1u128 << 32) * 1_000_000_000 / VIDEO_RTP_CLOCK_HZ as u128;
    let wrap_ns = WRAP_NS as u64;
    assert!(
        (47_720..47_723).contains(&(wrap_ns / 1_000_000_000)),
        "the wrap should be ~13.25 h, got {} s",
        wrap_ns / 1_000_000_000
    );

    // 10 ms either side of the wrap
    let before = host_ns_to_rtp90k(wrap_ns - 10_000_000);
    let after = host_ns_to_rtp90k(wrap_ns + 10_000_000);
    assert!(before > u32::MAX - 10_000, "before the wrap: {before}");
    assert!(after < 10_000, "after the wrap: {after}");
    // 20 ms is 1800 ticks, +/- one from the truncation of the wrap point itself
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

    // must be the same epoch as `host_now_ns()`, or audio stamped from a captured `Instant`
    // and video stamped from `host_now_ns()` are on two clocks
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

    // an Instant from before the epoch saturates to 0 instead of underflowing
    let long_ago = now - std::time::Duration::from_secs(60 * 60 * 24 * 365);
    assert_eq!(host_instant_to_ns(long_ago), 0);
}

#[test]
fn the_capture_tier_is_reported_without_changing_the_summary_line() {
    let d = AudioDiagnostics::default();
    assert_eq!(d.tier(), "", "nothing is claimed before a backend reports");
    let before = d.summary();
    d.set_tier(&"process-loopback");
    assert_eq!(d.tier(), "process-loopback");
    assert_eq!(d.summary(), before);
}

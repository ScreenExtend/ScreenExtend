use crate::macos_utils::audio::ring::Anchor;
use crate::macos_utils::audio::{anchored_ns, choose_capture_ns, FRAME_NS, RESYNC_NS};

/// 5 ms of interleaved stereo at 48 kHz: 240 frames x 2 channels
const FRAME_SAMPLES: u64 = 480;

#[test]
fn one_frame_further_into_the_ring_is_one_frame_later() {
    let a = Anchor {
        pos: 0,
        host_ns: 1_000_000_000,
    };
    assert_eq!(anchored_ns(a, 0), 1_000_000_000);
    assert_eq!(anchored_ns(a, FRAME_SAMPLES), 1_000_000_000 + FRAME_NS);
    assert_eq!(
        anchored_ns(a, FRAME_SAMPLES * 4),
        1_000_000_000 + FRAME_NS * 4
    );
}

#[test]
fn a_position_behind_the_anchor_extrapolates_backwards() {
    let a = Anchor {
        pos: FRAME_SAMPLES * 10,
        host_ns: 1_000_000_000,
    };
    assert_eq!(anchored_ns(a, FRAME_SAMPLES * 9), 1_000_000_000 - FRAME_NS);
}

#[test]
fn a_pre_epoch_extrapolation_reads_as_absent() {
    let a = Anchor {
        pos: FRAME_SAMPLES * 10,
        host_ns: FRAME_NS,
    };
    assert_eq!(anchored_ns(a, 0), 0);
}

/// The defect this replaced: a burst of frames drained together all claimed the drain time, so a
/// short pass following a long one handed the client timestamps that went backwards.
#[test]
fn jittered_hardware_stamps_never_produce_a_decreasing_timestamp() {
    let jitter: [i64; 12] = [
        0,
        3_000_000,
        -4_000_000,
        1_000_000,
        -9_000_000,
        0,
        12_000_000,
        -2_000_000,
        500_000,
        -500_000,
        7_000_000,
        -11_000_000,
    ];
    let mut next: Option<u64> = None;
    let mut last: Option<u64> = None;
    for (i, j) in jitter.iter().enumerate() {
        let true_ns = 1_000_000_000 + i as u64 * FRAME_NS;
        let hw = (true_ns as i64 + j).max(0) as u64;
        let (capture, jumped) = choose_capture_ns(next, Some(hw));
        assert!(!jumped, "frame {i}: jitter must not trip a resync");
        if let Some(prev) = last {
            assert_eq!(
                capture,
                prev + FRAME_NS,
                "frame {i}: frames must be exactly one period apart"
            );
        }
        last = Some(capture);
        next = Some(capture + FRAME_NS);
    }
}

#[test]
fn a_gap_larger_than_the_resync_window_jumps_the_timeline_forward() {
    let hw = 1_000_000_000 + RESYNC_NS + FRAME_NS;
    let (capture, jumped) = choose_capture_ns(Some(1_000_000_000), Some(hw));
    assert!(jumped);
    assert_eq!(capture, hw);
}

#[test]
fn the_first_frame_takes_the_hardware_stamp_when_there_is_one() {
    let (capture, jumped) = choose_capture_ns(None, Some(42_000_000));
    assert!(!jumped);
    assert_eq!(capture, 42_000_000);
}

#[test]
fn a_transport_without_stamps_still_advances_by_exactly_one_frame() {
    let (first, _) = choose_capture_ns(None, None);
    let (second, jumped) = choose_capture_ns(Some(first + FRAME_NS), None);
    assert!(!jumped);
    assert_eq!(second, first + FRAME_NS);
}

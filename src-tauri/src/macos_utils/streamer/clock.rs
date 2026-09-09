//! `mach_absolute_time` → shared-host-clock conversion for capture timestamps.
//!
//! CGDisplayStream hands every frame a `displayTime` and ScreenCaptureKit a presentation
//! timestamp; both were being discarded, so the pipeline could only ever measure from the moment
//! its callback ran, hiding whatever the frame spent between the compositor and us.
//!
//! The rest of the streamer measures against `streamer::audio::host_now_ns()`, whose epoch is an
//! `Instant` taken at process start. On macOS `Instant` is `mach_absolute_time`-backed, so the two
//! differ by a constant, sampled once and cached.

use std::sync::OnceLock;

use super::mach::{mach_delta_ns, mach_now};

/// `mach_ns - host_ns`, from a single pair of reads of the two clocks.
fn mach_to_host_delta_ns() -> i128 {
    static DELTA: OnceLock<i128> = OnceLock::new();
    *DELTA.get_or_init(|| {
        let host = crate::streamer::audio::host_now_ns();
        let mach = mach_delta_ns(0, mach_now());
        mach as i128 - host as i128
    })
}

/// Convert a raw `mach_absolute_time` tick count to the shared host clock.
///
/// Returns 0 — which is how [`crate::streamer::telemetry::FrameTiming`] spells "no OS
/// timestamp" — for a missing stamp or one from before the host epoch, so such a sample is
/// dropped rather than folded in as a negative duration.
pub fn mach_ticks_to_host_ns(ticks: u64) -> u64 {
    if ticks == 0 {
        return 0;
    }
    host_ns_to_shared_ns(mach_delta_ns(0, ticks))
}

/// Convert a mach-clock timestamp that is **already in nanoseconds** to the shared host clock.
///
/// ScreenCaptureKit stamps its samples from `CMClockGetHostTimeClock`, which is the same clock
/// CGDisplayStream's `displayTime` comes from but expressed as a `CMTime` in seconds rather than
/// as a raw tick count. Running such a value through the tick conversion would scale it by the
/// timebase a second time and produce a plausible-looking, systematically wrong number — which is
/// worse than the honest zero it replaces.
/// The OS's frame time when it is usable as a capture instant, else the callback's arrival.
///
/// Two ways an OS stamp is not a capture instant. It can be absent — CGDisplayStream delivers a
/// `displayTime` of 0 on a handful of frames per session (measured: 2-3 in 200). And it can be in
/// the *future*: `displayTime` is when WindowServer scheduled the frame to be shown, which on the
/// 10.15.8 box is ahead of our callback on every single frame, by 15.5 ms at p50 with a 60 Hz
/// display. Taking either at face value understates every latency measured from it by up to a
/// frame period, and can produce a negative span.
///
/// Both callers pass the arrival stamp they just took, so the fallback costs nothing.
pub fn os_frame_time_or(arrived_host_ns: u64, os_host_ns: u64) -> u64 {
    if os_host_ns == 0 || os_host_ns > arrived_host_ns {
        arrived_host_ns
    } else {
        os_host_ns
    }
}

pub fn host_ns_to_shared_ns(mach_ns: u64) -> u64 {
    if mach_ns == 0 {
        return 0;
    }
    let host = mach_ns as i128 - mach_to_host_delta_ns();
    if host <= 0 {
        0
    } else {
        host as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_taken_now_lands_within_a_millisecond_of_the_host_clock() {
        let before = crate::streamer::audio::host_now_ns();
        let ticks = mach_now();
        let after = crate::streamer::audio::host_now_ns();
        let converted = mach_ticks_to_host_ns(ticks);
        assert!(
            converted + 1_000_000 >= before && converted <= after + 1_000_000,
            "converted {converted} outside [{before}, {after}] by more than 1 ms"
        );
    }

    #[test]
    fn a_missing_or_pre_epoch_stamp_reads_as_absent() {
        assert_eq!(mach_ticks_to_host_ns(0), 0);
        assert_eq!(mach_ticks_to_host_ns(1), 0);
    }

    #[test]
    fn an_os_stamp_in_the_past_is_the_one_that_is_used() {
        assert_eq!(os_frame_time_or(1_000_000, 990_000), 990_000);
    }

    #[test]
    fn an_absent_or_future_os_stamp_falls_back_to_the_arrival() {
        // 0 is CGDisplayStream delivering no `displayTime` at all, which it does a few times a
        // session. The future case is `displayTime` on every frame of a 60 Hz display: it is when
        // WindowServer scheduled the frame to be shown, ~15.5 ms after our callback runs.
        assert_eq!(os_frame_time_or(1_000_000, 0), 1_000_000);
        assert_eq!(os_frame_time_or(1_000_000, 1_015_500), 1_000_000);
    }
}

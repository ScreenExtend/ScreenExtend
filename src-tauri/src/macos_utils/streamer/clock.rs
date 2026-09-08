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
}

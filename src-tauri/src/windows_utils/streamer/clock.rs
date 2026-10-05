//! QPC → host-clock conversion for the timestamps the capture APIs hand us.
//!
//! DXGI duplication's `DXGI_OUTDUPL_FRAME_INFO::LastPresentTime` (raw QPC ticks) says when the
//! pixels were presented, so the difference between it and callback entry is the delivery latency
//! the host adds before doing any work at all.
//!
//! WGC's `Direct3D11CaptureFrame::SystemRelativeTime` is deliberately **not** converted. It is on
//! the same QPC timebase, but measured on the Windows dev box (2026-10-05, 120-frame blocks over a
//! 1080p session) it sat 3-14 ms (mean 8-11 ms) *after* our own callback entry on 2,879 of 2,880
//! frames: it is the composition's target present time, not the moment the frame existed. Used as
//! the origin it made `capture_delivery` drop every sample but the stale ones (3 of ~9,900) and
//! `capture_to_encode` / `capture_to_socket` drop ~89 % of frames and keep a biased remainder.
//!
//! The rest of the streamer measures against `streamer::audio::host_now_ns()`, whose epoch is an
//! `Instant` taken at process start. On Windows `Instant` is itself QPC-backed, so the two clocks
//! differ by a constant, sampled once and cached.

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

fn qpc_frequency() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe { QueryPerformanceFrequency(&mut f) }.ok();
        if f <= 0 {
            10_000_000
        } else {
            f
        }
    })
}

fn qpc_now_ticks() -> i64 {
    let mut t = 0i64;
    unsafe { QueryPerformanceCounter(&mut t) }.ok();
    t
}

/// Nanoseconds since boot for a raw QPC tick count.
pub fn qpc_ticks_to_boot_ns(ticks: i64) -> u64 {
    if ticks <= 0 {
        return 0;
    }
    ((ticks as i128 * 1_000_000_000i128) / qpc_frequency() as i128) as u64
}

/// `boot_ns - host_ns`, sampled once from a single pair of reads of the two clocks.
fn boot_to_host_delta_ns() -> i128 {
    static DELTA: OnceLock<i128> = OnceLock::new();
    *DELTA.get_or_init(|| {
        let host = crate::streamer::audio::host_now_ns();
        let boot = qpc_ticks_to_boot_ns(qpc_now_ticks());
        boot as i128 - host as i128
    })
}

/// Convert a since-boot nanosecond stamp to the shared host clock.
///
/// Returns 0 for a stamp that predates the host epoch — a frame captured before the process
/// started cannot be timed against it, and 0 is how [`crate::streamer::telemetry::FrameTiming`]
/// spells "no OS timestamp", so such a sample is dropped rather than folded in as a negative.
pub fn boot_ns_to_host_ns(boot_ns: u64) -> u64 {
    if boot_ns == 0 {
        return 0;
    }
    let host = boot_ns as i128 - boot_to_host_delta_ns();
    if host <= 0 {
        0
    } else {
        host as u64
    }
}

/// DXGI hands `LastPresentTime` as raw QPC ticks.
pub fn dxgi_present_time_to_host_ns(ticks: i64) -> u64 {
    boot_ns_to_host_ns(qpc_ticks_to_boot_ns(ticks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_taken_now_lands_within_a_millisecond_of_the_host_clock() {
        let host_before = crate::streamer::audio::host_now_ns();
        let boot = qpc_ticks_to_boot_ns(qpc_now_ticks());
        let host_after = crate::streamer::audio::host_now_ns();
        let converted = boot_ns_to_host_ns(boot);
        assert!(
            converted + 1_000_000 >= host_before && converted <= host_after + 1_000_000,
            "converted {converted} outside [{host_before}, {host_after}] by more than 1 ms"
        );
    }

    #[test]
    fn a_missing_or_pre_epoch_stamp_reads_as_absent() {
        assert_eq!(boot_ns_to_host_ns(0), 0);
        assert_eq!(dxgi_present_time_to_host_ns(0), 0);
        // A stamp from long before this process started must not wrap into a huge positive.
        assert_eq!(boot_ns_to_host_ns(1), 0);
    }

    #[test]
    fn ticks_convert_at_the_reported_frequency() {
        let f = qpc_frequency();
        assert!(f > 0);
        assert_eq!(qpc_ticks_to_boot_ns(f), 1_000_000_000);
        assert_eq!(qpc_ticks_to_boot_ns(0), 0);
    }
}

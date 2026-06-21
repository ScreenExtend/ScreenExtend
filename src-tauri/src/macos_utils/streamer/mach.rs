//! Mach-time latency instrumentation (PRD §9).
//!
//! Each frame is stamped with `mach_absolute_time()` the instant it arrives in
//! the capture callback; the consumer compares against "now" in the same clock
//! domain. We deliberately do NOT use SCK's presentation timestamps as an
//! absolute clock — they live in a different epoch and yield negative
//! "latencies" (PRD §9 gotcha).

use mach2::mach_time::{mach_absolute_time, mach_timebase_info, mach_timebase_info_data_t};
use std::sync::OnceLock;

fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut tb = mach_timebase_info_data_t { numer: 0, denom: 0 };
        // SAFETY: writes a small POD struct; always succeeds.
        unsafe { mach_timebase_info(&mut tb) };
        (tb.numer.max(1) as u64, tb.denom.max(1) as u64)
    })
}

/// Monotonic timestamp in mach ticks.
#[inline]
pub fn mach_now() -> u64 {
    // SAFETY: reads the monotonic clock.
    unsafe { mach_absolute_time() }
}

/// Milliseconds elapsed since a `mach_now()` stamp.
#[inline]
pub fn mach_age_ms(then: u64) -> f64 {
    let (numer, denom) = timebase();
    let now = mach_now();
    (now.saturating_sub(then) as f64) * (numer as f64 / denom as f64) / 1.0e6
}

/// Nanoseconds between two `mach_now()` stamps.
#[inline]
pub fn mach_delta_ns(from: u64, to: u64) -> u64 {
    let (numer, denom) = timebase();
    (to.saturating_sub(from)).saturating_mul(numer) / denom
}

/// Convert nanoseconds to mach absolute-time ticks — the inverse of the timebase
/// scaling `mach_delta_ns` applies (`ns = ticks · numer / denom`). The real-time
/// scheduling budgets `thread_policy_set` takes are expressed in these ticks.
#[inline]
pub fn ns_to_mach_ticks(ns: u64) -> u32 {
    let (numer, denom) = timebase();
    (ns.saturating_mul(denom) / numer).min(u32::MAX as u64) as u32
}

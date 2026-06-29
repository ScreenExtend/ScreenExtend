use mach2::mach_time::{mach_absolute_time, mach_timebase_info, mach_timebase_info_data_t};
use std::sync::OnceLock;

fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut tb = mach_timebase_info_data_t { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut tb) };
        (tb.numer.max(1) as u64, tb.denom.max(1) as u64)
    })
}

#[inline]
pub fn mach_now() -> u64 {
    unsafe { mach_absolute_time() }
}

#[inline]
pub fn mach_age_ms(then: u64) -> f64 {
    let (numer, denom) = timebase();
    let now = mach_now();
    (now.saturating_sub(then) as f64) * (numer as f64 / denom as f64) / 1.0e6
}

#[inline]
pub fn mach_delta_ns(from: u64, to: u64) -> u64 {
    let (numer, denom) = timebase();
    (to.saturating_sub(from)).saturating_mul(numer) / denom
}

#[inline]
pub fn ns_to_mach_ticks(ns: u64) -> u32 {
    let (numer, denom) = timebase();
    (ns.saturating_mul(denom) / numer).min(u32::MAX as u64) as u32
}

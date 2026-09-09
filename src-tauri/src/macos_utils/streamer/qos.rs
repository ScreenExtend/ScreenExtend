//! Scheduling for the two hot threads on the macOS encode path.
//!
//! Two schemes, one per thread, rather than four stacked on both:
//!
//! * the **encode** thread runs under `THREAD_TIME_CONSTRAINT_POLICY` and takes no QoS class,
//!   because a QoS class is the kernel's way of *managing* a thread's priority and a
//!   time-constraint reservation is the way of taking that management away. Setting both means
//!   the last writer wins and the log says nothing about which one that was.
//! * the **drain** thread is a plain `USER_INTERACTIVE` QoS thread. It does almost nothing per
//!   frame (a memcpy and a channel send) and has no deadline of its own.
//!
//! Every call's return value is checked. `thread_policy_set` returns `kern_return_t` and
//! `pthread_set_qos_class_self_np` returns 0/errno; both were previously discarded, so a policy
//! that never applied looked exactly like one that did.

use std::ffi::c_int;
use std::sync::atomic::{AtomicBool, Ordering};

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_init::mach_thread_self;
use mach2::thread_policy::{
    thread_policy_get, thread_policy_set, thread_precedence_policy_data_t,
    thread_time_constraint_policy_data_t, THREAD_PRECEDENCE_POLICY, THREAD_PRECEDENCE_POLICY_COUNT,
    THREAD_TIME_CONSTRAINT_POLICY, THREAD_TIME_CONSTRAINT_POLICY_COUNT,
};

use super::mach::{mach_ticks_to_ns, ns_to_mach_ticks};

const QOS_CLASS_USER_INTERACTIVE: c_int = 0x21;
const QOS_CLASS_USER_INITIATED: c_int = 0x19;

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: c_int, relative_priority: c_int) -> c_int;
}

/// Log a scheduling call that failed, once per process per call site.
///
/// A failing `thread_policy_set` is not fatal — the thread simply runs at the default priority —
/// but it changes what every latency number below it means, so it must not be silent. Repeating
/// it per session would spam the log bus without adding information.
fn report(what: &str, rc: i32, seen: &AtomicBool) {
    if rc == KERN_SUCCESS {
        return;
    }
    if seen.swap(true, Ordering::Relaxed) {
        return;
    }
    teprintln!("[qos] {what} failed (rc={rc}); this thread runs at the default priority");
}

pub fn pin_current_thread_user_interactive() {
    static SEEN: AtomicBool = AtomicBool::new(false);
    let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    report("pthread_set_qos_class_self_np(USER_INTERACTIVE)", rc, &SEEN);
}

pub fn pin_current_thread_user_initiated() {
    static SEEN: AtomicBool = AtomicBool::new(false);
    let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0) };
    report("pthread_set_qos_class_self_np(USER_INITIATED)", rc, &SEEN);
}

/// The share of the period the encode thread reserves as guaranteed CPU.
///
/// The old value was `period / 8` — 2.08 ms out of a 16.67 ms frame — which is under a third of
/// what a 1080p VideoToolbox submit plus the Annex B walk costs on this box, so the thread was
/// descheduled mid-frame as a matter of course. Chromium reserves ~0.75 of the period on its own
/// deadline threads (`base/threading/platform_thread_mac.mm`); a reservation smaller than the
/// work is worse than none, because the scheduler holds you to it.
const COMPUTATION_FRACTION: f64 = 0.75;

/// Put the calling thread on the real-time (deadline) scheduler.
///
/// `preemptible` stays true: a non-preemptible thread that overruns its computation can wedge
/// the machine, and nothing here is worth that.
pub fn pin_current_thread_time_constraint(period_ns: u64) {
    static SEEN: AtomicBool = AtomicBool::new(false);
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if period_ns == 0 {
        return;
    }
    let computation_ns = (period_ns as f64 * COMPUTATION_FRACTION) as u64;
    let mut data = thread_time_constraint_policy_data_t {
        period: ns_to_mach_ticks(period_ns),
        computation: ns_to_mach_ticks(computation_ns),
        constraint: ns_to_mach_ticks(period_ns),
        preemptible: 1,
    };
    let rc = unsafe {
        thread_policy_set(
            mach_thread_self(),
            THREAD_TIME_CONSTRAINT_POLICY,
            &mut data as *mut _ as *mut _,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    };
    report("thread_policy_set(TIME_CONSTRAINT)", rc, &SEEN);
    if rc == KERN_SUCCESS && !REPORTED.swap(true, Ordering::Relaxed) {
        if let Some(effective) = read_time_constraint() {
            tprintln!(
                "[qos] encode thread on the deadline scheduler: period={:.2}ms \
                 computation={:.2}ms constraint={:.2}ms preemptible={}",
                mach_ticks_to_ns(effective.period) as f64 / 1.0e6,
                mach_ticks_to_ns(effective.computation) as f64 / 1.0e6,
                mach_ticks_to_ns(effective.constraint) as f64 / 1.0e6,
                effective.preemptible != 0,
            );
        }
    }
}

/// Read the policy back rather than trusting the write. `thread_policy_get` can return the
/// defaults with `get_default = 1` even after a successful set, so the flag is cleared first.
fn read_time_constraint() -> Option<thread_time_constraint_policy_data_t> {
    let mut data = thread_time_constraint_policy_data_t {
        period: 0,
        computation: 0,
        constraint: 0,
        preemptible: 0,
    };
    let mut count = THREAD_TIME_CONSTRAINT_POLICY_COUNT;
    let mut get_default: mach2::boolean::boolean_t = 0;
    let rc = unsafe {
        thread_policy_get(
            mach_thread_self(),
            THREAD_TIME_CONSTRAINT_POLICY,
            &mut data as *mut _ as *mut _,
            &mut count,
            &mut get_default,
        )
    };
    (rc == KERN_SUCCESS).then_some(data)
}

const HOT_THREAD_IMPORTANCE: c_int = 63;

/// Raise the thread's importance within its band.
///
/// Only meaningful for a timeshare thread — a deadline thread's priority comes from its
/// reservation — so this is for the drain thread, not the encode thread.
pub fn raise_current_thread_precedence() {
    static SEEN: AtomicBool = AtomicBool::new(false);
    let mut data = thread_precedence_policy_data_t {
        importance: HOT_THREAD_IMPORTANCE,
    };
    let rc = unsafe {
        thread_policy_set(
            mach_thread_self(),
            THREAD_PRECEDENCE_POLICY,
            &mut data as *mut _ as *mut _,
            THREAD_PRECEDENCE_POLICY_COUNT,
        )
    };
    report("thread_policy_set(PRECEDENCE)", rc, &SEEN);
}

/// Ask the scheduler to keep the capture/encode threads in one cache affinity set.
///
/// Intel-only: `THREAD_AFFINITY_POLICY` is documented as unimplemented on Apple silicon, where
/// it returns `KERN_NOT_SUPPORTED` rather than doing nothing quietly.
#[cfg(target_arch = "x86_64")]
pub fn pin_current_thread_encode_affinity() {
    use mach2::thread_policy::{
        thread_affinity_policy_data_t, THREAD_AFFINITY_POLICY, THREAD_AFFINITY_POLICY_COUNT,
    };
    static SEEN: AtomicBool = AtomicBool::new(false);
    const ENCODE_AFFINITY_TAG: c_int = 1;
    let mut data = thread_affinity_policy_data_t {
        affinity_tag: ENCODE_AFFINITY_TAG,
    };
    let rc = unsafe {
        thread_policy_set(
            mach_thread_self(),
            THREAD_AFFINITY_POLICY,
            &mut data as *mut _ as *mut _,
            THREAD_AFFINITY_POLICY_COUNT,
        )
    };
    report("thread_policy_set(AFFINITY)", rc, &SEEN);
}

#[cfg(not(target_arch = "x86_64"))]
pub fn pin_current_thread_encode_affinity() {}

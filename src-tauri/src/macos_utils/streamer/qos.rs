//! Thread QoS pinning (PRD §7).
//!
//! macOS schedules `USER_INTERACTIVE` work onto the performance cores and ahead
//! of lower-priority threads, which trims wakeup jitter / tail latency on the
//! capture-consume and encode threads. You cannot pin to P-cores directly — QoS
//! is the lever. Uses the per-thread pthread API (not the `dispatch_*_qos_class`
//! variants, one of which aborts on 10.15).

use std::ffi::c_int;

use mach2::mach_init::mach_thread_self;
use mach2::thread_policy::{
    THREAD_TIME_CONSTRAINT_POLICY, THREAD_TIME_CONSTRAINT_POLICY_COUNT, thread_policy_set,
    thread_time_constraint_policy_data_t,
};

use super::mach::ns_to_mach_ticks;

/// `qos_class_t` value for `QOS_CLASS_USER_INTERACTIVE` (from `sys/qos.h`).
const QOS_CLASS_USER_INTERACTIVE: c_int = 0x21;

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: c_int, relative_priority: c_int) -> c_int;
}

/// Raise the calling thread to the top user QoS. Best-effort; ignores failure.
pub fn pin_current_thread_user_interactive() {
    // SAFETY: a plain libSystem call that only adjusts the current thread's QoS.
    unsafe {
        let _ = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

/// Promote the calling thread to a real-time **time-constraint** policy (PRD
/// Appendix B / 2.5) on top of the QoS bump. Where QoS only biases the thread
/// toward the performance cores, a time-constraint contract asks the Mach
/// scheduler to run the thread within `constraint` of becoming runnable for up to
/// `computation` time every `period` — bounding wakeup jitter for the periodic
/// encode thread, which is exactly the tail-latency the streamer targets.
///
/// `period_ns` is the frame interval (1/fps). We budget half the period for the
/// encode work and allow up to the full period as the deadline, and keep the
/// thread **preemptible** so a heavy frame (an IDR) that overruns degrades
/// gracefully to ordinary scheduling instead of starving the system — the
/// failure mode the TODO warns a wrong compute estimate can cause. Best-effort;
/// a failure just leaves the thread at its QoS level.
pub fn pin_current_thread_time_constraint(period_ns: u64) {
    if period_ns == 0 {
        return;
    }
    let period = ns_to_mach_ticks(period_ns);
    let mut data = thread_time_constraint_policy_data_t {
        period,
        computation: ns_to_mach_ticks(period_ns / 2),
        constraint: period,
        preemptible: 1,
    };
    // SAFETY: standard thread_policy_set with a correctly-sized policy struct for
    // the current thread; only adjusts this thread's scheduling.
    unsafe {
        let _ = thread_policy_set(
            mach_thread_self(),
            THREAD_TIME_CONSTRAINT_POLICY,
            &mut data as *mut _ as *mut _,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        );
    }
}

use std::ffi::{c_char, c_int, c_void};

use mach2::mach_init::mach_thread_self;
use mach2::thread_policy::{
    THREAD_PRECEDENCE_POLICY, THREAD_PRECEDENCE_POLICY_COUNT, THREAD_TIME_CONSTRAINT_POLICY,
    THREAD_TIME_CONSTRAINT_POLICY_COUNT, thread_policy_set, thread_precedence_policy_data_t,
    thread_time_constraint_policy_data_t,
};

use super::mach::ns_to_mach_ticks;

const QOS_CLASS_USER_INTERACTIVE: c_int = 0x21;
const QOS_CLASS_USER_INITIATED: c_int = 0x19;

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: c_int, relative_priority: c_int) -> c_int;
}

pub fn pin_current_thread_user_interactive() {
    unsafe {
        let _ = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

pub fn pin_current_thread_user_initiated() {
    unsafe {
        let _ = pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0);
    }
}

pub fn pin_current_thread_time_constraint(period_ns: u64) {
    if period_ns == 0 {
        return;
    }
    let period = ns_to_mach_ticks(period_ns);
    let mut data = thread_time_constraint_policy_data_t {
        period,
        computation: ns_to_mach_ticks(period_ns / 8),
        constraint: period,
        preemptible: 1,
    };
    unsafe {
        let _ = thread_policy_set(
            mach_thread_self(),
            THREAD_TIME_CONSTRAINT_POLICY,
            &mut data as *mut _ as *mut _,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        );
    }
}

const HOT_THREAD_IMPORTANCE: c_int = 63;

pub fn raise_current_thread_precedence() {
    let mut data = thread_precedence_policy_data_t {
        importance: HOT_THREAD_IMPORTANCE,
    };
    unsafe {
        let _ = thread_policy_set(
            mach_thread_self(),
            THREAD_PRECEDENCE_POLICY,
            &mut data as *mut _ as *mut _,
            THREAD_PRECEDENCE_POLICY_COUNT,
        );
    }
}

#[cfg(target_arch = "x86_64")]
pub fn pin_current_thread_encode_affinity() {
    use mach2::thread_policy::{
        THREAD_AFFINITY_POLICY, THREAD_AFFINITY_POLICY_COUNT, thread_affinity_policy_data_t,
    };
    const ENCODE_AFFINITY_TAG: c_int = 1;
    let mut data = thread_affinity_policy_data_t {
        affinity_tag: ENCODE_AFFINITY_TAG,
    };
    unsafe {
        let _ = thread_policy_set(
            mach_thread_self(),
            THREAD_AFFINITY_POLICY,
            &mut data as *mut _ as *mut _,
            THREAD_AFFINITY_POLICY_COUNT,
        );
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn pin_current_thread_encode_affinity() {}

#[allow(non_camel_case_types)]
type os_workgroup_t = *mut c_void;
#[allow(non_camel_case_types)]
type os_workgroup_attr_t = *mut c_void;

const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;

#[repr(C)]
#[derive(Clone, Copy)]
struct JoinToken([u8; 64]);

type CreateFn =
    unsafe extern "C" fn(*const c_char, u32, os_workgroup_attr_t) -> os_workgroup_t;
type JoinFn = unsafe extern "C" fn(os_workgroup_t, *mut JoinToken) -> c_int;
type LeaveFn = unsafe extern "C" fn(os_workgroup_t, *mut JoinToken);
type ReleaseFn = unsafe extern "C" fn(*mut c_void);

unsafe fn dlsym(name: &[u8]) -> Option<*mut c_void> {
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const c_char) };
    if p.is_null() { None } else { Some(p) }
}

pub struct FrameWorkgroup {
    wg: os_workgroup_t,
    token: Box<JoinToken>,
    leave: LeaveFn,
    release: Option<ReleaseFn>,
}

unsafe impl Send for FrameWorkgroup {}

impl FrameWorkgroup {
    pub fn join(_period_ns: u64) -> Option<Self> {
        unsafe {
            let create: CreateFn = std::mem::transmute(dlsym(b"os_workgroup_interval_create\0")?);
            let join: JoinFn = std::mem::transmute(dlsym(b"os_workgroup_join\0")?);
            let leave: LeaveFn = std::mem::transmute(dlsym(b"os_workgroup_leave\0")?);
            let release: Option<ReleaseFn> =
                dlsym(b"os_release\0").map(|p| std::mem::transmute::<_, ReleaseFn>(p));

            let name = b"screenextend-encode\0";
            let wg = create(name.as_ptr() as *const c_char, OS_CLOCK_MACH_ABSOLUTE_TIME, std::ptr::null_mut());
            if wg.is_null() {
                return None;
            }
            let mut token = Box::new(JoinToken([0u8; 64]));
            let rc = join(wg, token.as_mut() as *mut JoinToken);
            if rc != 0 {
                if let Some(rel) = release {
                    rel(wg);
                }
                teprintln!("[qos] os_workgroup_join failed (rc={rc}); using QoS + time-constraint only");
                return None;
            }
            tprintln!("[qos] joined real-time encode workgroup");
            Some(FrameWorkgroup { wg, token, leave, release })
        }
    }
}

impl Drop for FrameWorkgroup {
    fn drop(&mut self) {
        unsafe {
            (self.leave)(self.wg, self.token.as_mut() as *mut JoinToken);
            if let Some(rel) = self.release {
                rel(self.wg);
            }
        }
    }
}

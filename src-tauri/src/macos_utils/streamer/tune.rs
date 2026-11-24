//! Per-item switches over the macOS host tuning, for A/B measurement, and a record of what was
//! actually applied.
//!
//! * `SE_TUNE_OFF=<a,b,…>` disables individual **existing** items (all on by default).
//! * `SE_TUNE_ON=<a,b,…>` enables individual **candidates** (all off by default; none ships on).
//!
//! Both are read once per process. With neither set every call site behaves exactly as it did
//! before the switches existed. Every tuning call site records its outcome here — applied, failed
//! with a status, or skipped because it was switched off — and [`report`] returns that record, so
//! a measurement says what the process really got rather than what it asked for.
//! `docs/research/macos-tuning-audit.md` is the table of items and their Windows counterparts.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value as Json};

use crate::streamer::lock::LockExt;

/// Existing tuning, on unless named in `SE_TUNE_OFF`.
pub const EXISTING: &[&str] = &[
    // tuning.rs: process-wide NSActivity (UserInitiated, no automatic/sudden termination).
    "process_activity",
    // activity.rs: per-session NSActivity UserInitiated | LatencyCritical.
    "latency_activity",
    // power.rs: IOPM PreventUserIdleSystemSleep + PreventUserIdleDisplaySleep while encoding.
    "keep_awake",
    // qos.rs: the encode thread on THREAD_TIME_CONSTRAINT_POLICY.
    "encode_time_constraint",
    // qos.rs: THREAD_AFFINITY_POLICY tag on the encode and drain threads (Intel only).
    "encode_affinity",
    // qos.rs: the drain thread at QOS_CLASS_USER_INTERACTIVE.
    "drain_qos",
    // qos.rs: the drain thread's THREAD_PRECEDENCE_POLICY importance 63.
    "drain_precedence",
    // platform.rs: tokio transport workers at QOS_CLASS_USER_INITIATED.
    "transport_qos",
    // pipeline.rs: resubmit the newest frame to push it out of a one-frame-hold encoder
    // (behavioural probe; a no-op on an encoder that emits without a later submit).
    "hold_push",
    // task_policy_set(TASK_CATEGORY_POLICY, TASK_FOREGROUND_APPLICATION) while a session is live,
    // re-asserted about once a second from the encode loop (macOS resets the role on activation
    // changes). Measured on an M1 under 8 busy threads: offer->packetized p99 16.06 -> 6.92 ms.
    "task_role_foreground",
    // perf_hold.rs: the performance hold's DEFAULT POLICY (linger variant on with >= 2 perf
    // levels, AC power, Low Power Mode off). Switching it off disables the default; SE_PERF_HOLD
    // still wins over this.
    "perf_hold",
];

/// Candidates, off unless named in `SE_TUNE_ON`.
pub const CANDIDATES: &[&str] = &[
    // The drain thread on THREAD_TIME_CONSTRAINT_POLICY instead of QoS + precedence.
    "drain_time_constraint",
    // Transport workers (and the bench's packetize thread) at USER_INTERACTIVE, not INITIATED.
    "transport_user_interactive",
    // The encode thread's reservation shortened: computation min(2 ms, period/4), constraint
    // period/2, instead of 0.75 / 1.0 of the period.
    "encode_tc_short",
    // task_policy_set(TASK_BASE_QOS_POLICY, LATENCY_QOS_TIER_0 / THROUGHPUT_QOS_TIER_0).
    "task_qos_tier0",
    // Low-power mode (12+, respondsToSelector) and thermal state at session start, into the
    // log and the session telemetry.
    "power_report",
    // Log every NSProcessInfoThermalStateDidChangeNotification.
    "thermal_notify",
    // perf_hold.rs: force the always-spin variant (one USER_INTERACTIVE thread busy-spins while
    // any session is live), whatever the power source or core layout.
    "perf_hold_spin",
    // perf_hold.rs: force the linger variant (spins from each captured frame until 200 ms after
    // the newest one), whatever the power source or core layout.
    "perf_hold_inflight",
    // TEST HOOK (needs SE_TEST_HOOKS=1, which the bench runner sets with it): every session's
    // hold push starts in immediate mode (same as SE_HOLD_PUSH_START=immediate).
    "hold_push_start_immediate",
];

struct Switches {
    off: Vec<String>,
    on: Vec<String>,
}

fn parse_list(var: &str) -> Vec<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn switches() -> &'static Switches {
    static S: OnceLock<Switches> = OnceLock::new();
    S.get_or_init(|| {
        let off = parse_list("SE_TUNE_OFF");
        let on = parse_list("SE_TUNE_ON");
        for n in &off {
            if !EXISTING.contains(&n.as_str()) {
                teprintln!(
                    "[tune] SE_TUNE_OFF names unknown item '{n}' (existing: {})",
                    EXISTING.join(",")
                );
            }
        }
        for n in &on {
            if !CANDIDATES.contains(&n.as_str()) {
                teprintln!(
                    "[tune] SE_TUNE_ON names unknown item '{n}' (candidates: {})",
                    CANDIDATES.join(",")
                );
            }
        }
        if !off.is_empty() || !on.is_empty() {
            teprintln!(
                "[tune] A/B switches active: SE_TUNE_OFF=[{}] SE_TUNE_ON=[{}]",
                off.join(","),
                on.join(",")
            );
        }
        Switches { off, on }
    })
}

/// Whether a tuning item should run in this process.
pub fn enabled(name: &str) -> bool {
    let s = switches();
    if EXISTING.contains(&name) {
        !s.off.iter().any(|n| n == name)
    } else if CANDIDATES.contains(&name) {
        s.on.iter().any(|n| n == name)
    } else {
        debug_assert!(false, "unknown tuning item {name}");
        false
    }
}

fn applied() -> &'static Mutex<BTreeMap<String, Json>> {
    static A: OnceLock<Mutex<BTreeMap<String, Json>>> = OnceLock::new();
    A.get_or_init(Default::default)
}

/// Record one call site's outcome. Repeated calls (one per thread or per session) keep the last
/// detail and count the calls. Called once per thread or session start, never per frame.
pub fn record(name: &str, ok: bool, detail: Json) {
    let mut m = applied().lock_recover();
    let calls = m.get(name).and_then(|v| v["calls"].as_u64()).unwrap_or(0) + 1;
    m.insert(
        name.to_string(),
        json!({ "applied": ok, "calls": calls, "detail": detail }),
    );
}

/// Record a kernel/errno-style return code (0 = success).
pub fn record_rc(name: &str, rc: i32) {
    record(name, rc == 0, json!({ "rc": rc }));
}

/// Record that an item was skipped because it is switched off (existing) or not switched on.
pub fn record_skipped(name: &str) {
    let why = if EXISTING.contains(&name) {
        "SE_TUNE_OFF"
    } else {
        "not in SE_TUNE_ON"
    };
    record(name, false, json!({ "skipped": why }));
}

/// `{requested_off, requested_on, items: {name: {applied, calls, detail}}}`.
pub fn report() -> Json {
    let s = switches();
    let mut items = applied().lock_recover().clone();
    if let Some(item) = items.get_mut("task_role_foreground") {
        item["detail"]["asserts"] = json!(ROLE_ASSERTS.load(Relaxed));
        item["detail"]["last_rc"] = json!(ROLE_LAST_RC.load(Relaxed));
        item["detail"]["failures"] = json!(ROLE_FAILURES.load(Relaxed));
    }
    json!({
        "requested_off": s.off,
        "requested_on": s.on,
        "items": items,
    })
}

// ---------------------------------------------------------------------------------------------
// The foreground task role, while a session is live.
// ---------------------------------------------------------------------------------------------

static ROLE_ASSERTS: AtomicU64 = AtomicU64::new(0);
static ROLE_FAILURES: AtomicU64 = AtomicU64::new(0);
static ROLE_LAST_RC: AtomicI32 = AtomicI32::new(0);
static ROLE_FIRST: AtomicBool = AtomicBool::new(true);

/// `task_policy_set(TASK_CATEGORY_POLICY, TASK_FOREGROUND_APPLICATION)`, if `task_role_foreground`
/// is on. Called by the encode loop at session start and about once a second while streaming
/// (one mach call); nothing is restored at session end. The first call logs one line and records
/// the item; a failure is logged once and otherwise only counted. Returns whether it ran.
pub fn assert_task_role() -> bool {
    if !enabled("task_role_foreground") {
        if ROLE_FIRST.swap(false, Relaxed) {
            record_skipped("task_role_foreground");
        }
        return false;
    }
    let mut role = [TASK_FOREGROUND_APPLICATION];
    let rc = unsafe {
        task_policy_set(
            mach2::traps::mach_task_self(),
            TASK_CATEGORY_POLICY,
            role.as_mut_ptr(),
            1,
        )
    };
    ROLE_ASSERTS.fetch_add(1, Relaxed);
    ROLE_LAST_RC.store(rc, Relaxed);
    if rc != 0 && ROLE_FAILURES.fetch_add(1, Relaxed) == 0 {
        teprintln!(
            "[tune] task_policy_set(TASK_CATEGORY_POLICY, FOREGROUND_APPLICATION) failed: {rc}; \
             carrying on (logged once)"
        );
    }
    if ROLE_FIRST.swap(false, Relaxed) {
        tprintln!(
            "[tune] task role FOREGROUND_APPLICATION while streaming (rc {rc}); re-asserted about \
             once a second, not restored at session end"
        );
        record_rc("task_role_foreground", rc);
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Candidates that are process-wide.
// ---------------------------------------------------------------------------------------------

type MachPort = u32;
unsafe extern "C" {
    fn task_policy_set(task: MachPort, flavor: u32, policy_info: *mut i32, count: u32) -> i32;
}

const TASK_CATEGORY_POLICY: u32 = 1;
const TASK_BASE_QOS_POLICY: u32 = 8;
const TASK_FOREGROUND_APPLICATION: i32 = 1;
const LATENCY_QOS_TIER_0: i32 = (0xFF << 16) | 1;
const THROUGHPUT_QOS_TIER_0: i32 = (0xFE << 16) | 1;

/// The process-wide candidates. Called from `tuning::apply_process_tuning`.
pub fn apply_process_candidates() {
    if enabled("task_qos_tier0") {
        // struct task_qos_policy { latency tier, throughput tier }, TASK_QOS_POLICY_COUNT = 2.
        let mut policy = [LATENCY_QOS_TIER_0, THROUGHPUT_QOS_TIER_0];
        let rc = unsafe {
            task_policy_set(
                mach2::traps::mach_task_self(),
                TASK_BASE_QOS_POLICY,
                policy.as_mut_ptr(),
                2,
            )
        };
        tprintln!("[tune] task_policy_set(TASK_BASE_QOS_POLICY, tier 0/0) -> {rc}");
        record_rc("task_qos_tier0", rc);
    } else {
        record_skipped("task_qos_tier0");
    }
    if enabled("thermal_notify") {
        let ok = observe_thermal_state();
        record("thermal_notify", ok, json!({ "observer_installed": ok }));
    } else {
        record_skipped("thermal_notify");
    }
}

// ---------------------------------------------------------------------------------------------
// Power and thermal reporting.
// ---------------------------------------------------------------------------------------------

/// `NSProcessInfo.thermalState` (10.10.3): 0 nominal, 1 fair, 2 serious, 3 critical.
pub fn thermal_state() -> Option<i64> {
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{msg_send, sel};
    let cls = AnyClass::get(c"NSProcessInfo")?;
    unsafe {
        let pi: *mut AnyObject = msg_send![cls, processInfo];
        let pi = pi.as_ref()?;
        let responds: Bool = msg_send![pi, respondsToSelector: sel!(thermalState)];
        if !responds.as_bool() {
            return None;
        }
        let s: isize = msg_send![pi, thermalState];
        Some(s as i64)
    }
}

/// `NSProcessInfo.isLowPowerModeEnabled`, which exists on macOS 12+ only; probed with
/// `respondsToSelector:`, never with a version number.
pub fn low_power_mode() -> Option<bool> {
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{msg_send, sel};
    let cls = AnyClass::get(c"NSProcessInfo")?;
    unsafe {
        let pi: *mut AnyObject = msg_send![cls, processInfo];
        let pi = pi.as_ref()?;
        let responds: Bool = msg_send![pi, respondsToSelector: sel!(isLowPowerModeEnabled)];
        if !responds.as_bool() {
            return None;
        }
        let b: Bool = msg_send![pi, isLowPowerModeEnabled];
        Some(b.as_bool())
    }
}

pub fn thermal_name(s: Option<i64>) -> &'static str {
    match s {
        Some(0) => "nominal",
        Some(1) => "fair",
        Some(2) => "serious",
        Some(3) => "critical",
        Some(_) => "unknown",
        None => "unavailable",
    }
}

/// The power source: `ac`, `battery`, `ups` or `unknown`, from IOKit's
/// `IOPSCopyPowerSourcesInfo` + `IOPSGetProvidingPowerSourceType` (both 10.2+), resolved with
/// `dlsym` so nothing new is linked.
pub fn power_source() -> &'static str {
    type CopyInfo = unsafe extern "C" fn() -> *const std::ffi::c_void;
    type ProvidingType =
        unsafe extern "C" fn(*const std::ffi::c_void) -> *const objc2_core_foundation::CFString;
    let sym = |name: &str| -> Option<*mut std::ffi::c_void> {
        let c = std::ffi::CString::new(name).ok()?;
        let mut p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
        if p.is_null() {
            let lib = c"/System/Library/Frameworks/IOKit.framework/IOKit";
            let h = unsafe { libc::dlopen(lib.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
            if !h.is_null() {
                p = unsafe { libc::dlsym(h, c.as_ptr()) };
            }
        }
        (!p.is_null()).then_some(p)
    };
    let (Some(copy), Some(get)) = (
        sym("IOPSCopyPowerSourcesInfo"),
        sym("IOPSGetProvidingPowerSourceType"),
    ) else {
        return "unknown";
    };
    let copy: CopyInfo = unsafe { std::mem::transmute(copy) };
    let get: ProvidingType = unsafe { std::mem::transmute(get) };
    let info = unsafe { copy() };
    if info.is_null() {
        return "unknown";
    }
    let t = unsafe { get(info) };
    let out = match unsafe { t.as_ref() }.map(|s| s.to_string()) {
        Some(s) if s == "AC Power" => "ac",
        Some(s) if s == "Battery Power" => "battery",
        Some(s) if s == "UPS Power" => "ups",
        _ => "unknown",
    };
    // `IOPSCopyPowerSourcesInfo` follows the Copy rule; the type string is borrowed from it.
    unsafe {
        objc2_core_foundation::CFRetained::from_raw(std::ptr::NonNull::new_unchecked(
            info as *mut objc2_core_foundation::CFType,
        ))
    };
    out
}

/// One line, for the log and the telemetry slot: `power_source=… low_power_mode=… thermal=…`.
pub fn power_line() -> String {
    format!(
        "power_source={} low_power_mode={} thermal={}",
        power_source(),
        match low_power_mode() {
            Some(true) => "on",
            Some(false) => "off",
            None => "unavailable",
        },
        thermal_name(thermal_state())
    )
}

/// Install an observer for `NSProcessInfoThermalStateDidChangeNotification` (10.10.3; the name
/// is `dlsym`'d) that logs every change. The block runs on the posting thread; it only reads the
/// state and logs. Installed at most once per process.
fn observe_thermal_state() -> bool {
    use std::ptr::NonNull;

    use block2::RcBlock;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{msg_send, sel};

    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let Some(name_ptr) =
            super::vt_recipe::dlsym_ptr("NSProcessInfoThermalStateDidChangeNotification")
        else {
            return false;
        };
        let name = unsafe { *(name_ptr as *const *mut AnyObject) };
        if name.is_null() {
            return false;
        }
        let Some(cls) = AnyClass::get(c"NSNotificationCenter") else {
            return false;
        };
        let block = RcBlock::new(|_note: NonNull<AnyObject>| {
            tprintln!(
                "[tune] thermal state changed: {} ({})",
                thermal_name(thermal_state()),
                power_line()
            );
        });
        unsafe {
            let center: *mut AnyObject = msg_send![cls, defaultCenter];
            let Some(center) = center.as_ref() else {
                return false;
            };
            let _ = sel!(addObserverForName:object:queue:usingBlock:);
            let token: *mut AnyObject = msg_send![
                center,
                addObserverForName: name,
                object: std::ptr::null_mut::<AnyObject>(),
                queue: std::ptr::null_mut::<AnyObject>(),
                usingBlock: &*block
            ];
            if token.is_null() {
                return false;
            }
            // The token (and through it the block) lives for the process: retain it once more
            // so the autoreleased return value is never the last reference.
            let _: *mut AnyObject = msg_send![token, retain];
        }
        std::mem::forget(block);
        tprintln!("[tune] observing thermal-state changes ({})", power_line());
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_has_one_home() {
        for n in EXISTING {
            assert!(
                !CANDIDATES.contains(n),
                "{n} is both existing and candidate"
            );
        }
        // With neither variable set (the test environment) existing items run, candidates do not.
        if std::env::var("SE_TUNE_OFF").is_err() && std::env::var("SE_TUNE_ON").is_err() {
            assert!(EXISTING.iter().all(|n| enabled(n)));
            assert!(CANDIDATES.iter().all(|n| !enabled(n)));
        }
    }
}

//! The performance hold: one `QOS_CLASS_USER_INTERACTIVE` thread of this process busy-spins so
//! the encode runs on a woken performance cluster. M1: ~6.5 ms a frame with it, ~8.1 ms without
//! (the fallback `ave.avc` encoder: 7.4 vs 10.5 ms). An external low-priority spinner does not
//! help, and on the 2012 Intel MacBook Pro nothing changes, so there it would be pure cost.
//!
//! [`decide`] is the policy; it runs on this module's own thread, never on the encode thread.
//! The parked thread is woken by `Thread::unpark` from the encode loop's real-frame submit
//! (~11 us mean, ~25 us max to running).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::streamer::lock::LockExt;

/// Default linger after the newest captured frame: long enough that sparse but active content
/// (typing, a cursor at ~10 fps) stays warm.
pub const LINGER: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Spin while any session is live.
    Spin,
    /// Spin from a captured frame until the linger after the newest one.
    Linger,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Spin => "spin",
            Variant::Linger => "linger",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerSource {
    Ac,
    Battery,
    Ups,
    Unknown,
}

impl PowerSource {
    pub fn from_name(s: &str) -> PowerSource {
        match s {
            "ac" => PowerSource::Ac,
            "battery" => PowerSource::Battery,
            "ups" => PowerSource::Ups,
            _ => PowerSource::Unknown,
        }
    }
}

/// `NSProcessInfoThermalStateSerious`: a machine already throttling is not given more to do.
const THERMAL_SERIOUS: i64 = 2;

/// What the decision is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inputs {
    /// `hw.nperflevels` (`None`: the sysctl does not exist).
    pub perf_levels: Option<i64>,
    pub power: PowerSource,
    /// `NSProcessInfo.isLowPowerModeEnabled` (`None`: not readable).
    pub low_power_mode: Option<bool>,
    /// `NSProcessInfo.thermalState` (0 nominal .. 3 critical; `None`: not readable).
    pub thermal: Option<i64>,
    /// `SE_PERF_HOLD`.
    pub env: Option<String>,
    /// Plan switches: `perf_hold` switched off, `perf_hold_spin` / `perf_hold_inflight` on.
    pub tune_off: bool,
    pub tune_spin: bool,
    pub tune_linger: bool,
}

/// The decision and the reason, for the log line and the tuning record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub variant: Option<Variant>,
    pub why: String,
}

/// The policy. Pure: precedence is `SE_PERF_HOLD`, then the plan switches, then the default
/// (linger on with >= 2 performance levels, AC power and Low Power Mode off).
pub fn decide(i: &Inputs) -> Decision {
    let d = |variant, why: String| Decision { variant, why };
    match i.env.as_deref().map(str::trim) {
        Some("0") => return d(None, "SE_PERF_HOLD=0".into()),
        Some("1") => return d(Some(Variant::Linger), "SE_PERF_HOLD=1".into()),
        Some("spin") => return d(Some(Variant::Spin), "SE_PERF_HOLD=spin".into()),
        _ => {}
    }
    if i.tune_spin {
        return d(Some(Variant::Spin), "tune_on perf_hold_spin".into());
    }
    if i.tune_linger {
        return d(Some(Variant::Linger), "tune_on perf_hold_inflight".into());
    }
    if i.tune_off {
        return d(None, "SE_TUNE_OFF perf_hold".into());
    }
    let levels = match i.perf_levels {
        Some(n) if n >= 2 => n,
        Some(n) => {
            return d(
                None,
                format!("{n} perf level(s): no efficiency cores to avoid"),
            )
        }
        None => return d(None, "hw.nperflevels absent: one kind of core".into()),
    };
    if i.power != PowerSource::Ac {
        return d(
            None,
            format!("{levels} perf levels, but on {:?} power", i.power).to_lowercase(),
        );
    }
    if i.low_power_mode == Some(true) {
        return d(
            None,
            format!("{levels} perf levels, AC power, but Low Power Mode is on"),
        );
    }
    if let Some(t) = i.thermal.filter(|t| *t >= THERMAL_SERIOUS) {
        return d(
            None,
            format!("{levels} perf levels, AC power, but thermal state {t} (serious or worse)"),
        );
    }
    d(
        Some(Variant::Linger),
        format!("{levels} perf levels, AC power"),
    )
}

/// The linger state: only a captured frame starts or extends it. Pure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Linger {
    last_real_ns: u64,
}

impl Linger {
    /// A captured frame was submitted at `now_ns`.
    pub fn real(&mut self, now_ns: u64) {
        self.last_real_ns = self.last_real_ns.max(now_ns);
    }

    /// Whether to be spinning at `now_ns`. A repeat or a hold push never calls [`Linger::real`],
    /// so a still screen parks `linger` after its last real frame.
    pub fn should_spin(&self, now_ns: u64, linger_ns: u64) -> bool {
        self.last_real_ns != 0 && now_ns < self.last_real_ns.saturating_add(linger_ns)
    }
}

static SESSIONS: AtomicUsize = AtomicUsize::new(0);
/// 0 off, 1 spin, 2 linger: the running variant, decided by the thread.
static ACTIVE: AtomicU8 = AtomicU8::new(0);
static LAST_REAL_NS: AtomicU64 = AtomicU64::new(0);
static SPIN_NS: AtomicU64 = AtomicU64::new(0);
static WAKES: AtomicU64 = AtomicU64::new(0);
static WAKE_NS_SUM: AtomicU64 = AtomicU64::new(0);
static WAKE_NS_MAX: AtomicU64 = AtomicU64::new(0);
static UNPARKED_AT: AtomicU64 = AtomicU64::new(0);
/// A session just started: the thread decides now instead of at its next once-a-second look.
static DECIDE_NOW: AtomicBool = AtomicBool::new(false);
static STARTED_ONCE: AtomicBool = AtomicBool::new(false);

fn thread_handle() -> &'static OnceLock<Thread> {
    static T: OnceLock<Thread> = OnceLock::new();
    &T
}

struct Live {
    started: Option<Instant>,
    last: Option<(Decision, Inputs)>,
    changes: u64,
}

fn live() -> &'static Mutex<Live> {
    static L: OnceLock<Mutex<Live>> = OnceLock::new();
    L.get_or_init(|| {
        Mutex::new(Live {
            started: None,
            last: None,
            changes: 0,
        })
    })
}

fn linger_ns() -> u64 {
    static L: OnceLock<u64> = OnceLock::new();
    *L.get_or_init(|| {
        super::vt_recipe::hook_var("SE_PERF_HOLD_LINGER_MS")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(LINGER.as_nanos() as u64, |ms| ms * 1_000_000)
    })
}

fn perf_levels() -> Option<i64> {
    let mut v: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    let name = c"hw.nperflevels";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut v as *mut i32).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(v as i64)
}

/// Read every input now (power source and Low Power Mode are slow-path reads).
pub fn read_inputs() -> Inputs {
    use super::tune;
    Inputs {
        perf_levels: perf_levels(),
        power: PowerSource::from_name(tune::power_source()),
        low_power_mode: tune::low_power_mode(),
        thermal: tune::thermal_state(),
        env: crate::envcfg::var("SE_PERF_HOLD")
            .ok()
            .filter(|v| !v.is_empty()),
        tune_off: !tune::enabled("perf_hold"),
        tune_spin: tune::enabled("perf_hold_spin"),
        tune_linger: tune::enabled("perf_hold_inflight"),
    }
}

/// A live session; dropping it ends the session.
pub struct Session;

/// Start of a streaming session (the encode loop). Starts the hold's thread on first use; the
/// thread makes and re-makes the decision.
pub fn begin() -> Session {
    let _l = live().lock_recover();
    if SESSIONS.fetch_add(1, Relaxed) == 0 {
        let mut l = _l;
        l.started = Some(Instant::now());
        l.changes = 0;
        l.last = None;
        drop(l);
        SPIN_NS.store(0, Relaxed);
        WAKES.store(0, Relaxed);
        WAKE_NS_SUM.store(0, Relaxed);
        WAKE_NS_MAX.store(0, Relaxed);
        let t = thread_handle();
        if t.get().is_none() {
            match std::thread::Builder::new()
                .name("perf-hold".into())
                .spawn(run)
            {
                Ok(h) => {
                    let _ = t.set(h.thread().clone());
                }
                Err(e) => teprintln!("[tune] perf_hold: could not start its thread: {e}"),
            }
        }
        // the decision reads IOKit and ObjC, so the hold's own thread makes it
        DECIDE_NOW.store(true, Relaxed);
        if let Some(t) = t.get() {
            t.unpark();
        }
    }
    Session
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut l = live().lock_recover();
        if SESSIONS.fetch_sub(1, Relaxed) != 1 {
            return;
        }
        let was = ACTIVE.swap(0, Relaxed);
        if let Some(t) = thread_handle().get() {
            t.unpark();
        }
        let live_ns = l
            .started
            .take()
            .map_or(0, |s| s.elapsed().as_nanos() as u64);
        let last = l.last.take();
        let changes = l.changes;
        drop(l);
        // let the thread book its last spin interval
        std::thread::sleep(Duration::from_millis(2));
        if was != 0 {
            tprintln!("[tune] performance hold off: the last session ended");
        }
        let spin = SPIN_NS.load(Relaxed);
        let wakes = WAKES.load(Relaxed);
        let (variant, why, inputs) = match &last {
            Some((d, i)) => (d.variant, d.why.clone(), Some(i.clone())),
            None => (None, "no decision".into(), None),
        };
        super::tune::record(
            "perf_hold",
            variant.is_some(),
            json!({
                "variant": variant.map(Variant::as_str),
                "why": why,
                "decision_changes": changes,
                "inputs": inputs.map(|i| json!({
                    "perf_levels": i.perf_levels,
                    "power_source": format!("{:?}", i.power).to_lowercase(),
                    "low_power_mode": i.low_power_mode,
                    "thermal": i.thermal,
                    "env": i.env,
                    "tune_off": i.tune_off,
                    "tune_spin": i.tune_spin,
                    "tune_linger": i.tune_linger,
                })),
                "linger_ms": linger_ns() as f64 / 1e6,
                "live_s": live_ns as f64 / 1e9,
                "spin_s": spin as f64 / 1e9,
                "spin_fraction": if live_ns > 0 { spin as f64 / live_ns as f64 } else { 0.0 },
                "wakes": wakes,
                "wake_us_mean": if wakes > 0 {
                    WAKE_NS_SUM.load(Relaxed) as f64 / wakes as f64 / 1e3
                } else { 0.0 },
                "wake_us_max": WAKE_NS_MAX.load(Relaxed) as f64 / 1e3,
            }),
        );
    }
}

/// The encode loop submitted a *captured* frame (never a repeat or a hold push): starts or
/// extends the linger. Two relaxed atomic ops, plus an unpark while the linger variant runs.
#[inline]
pub fn real_frame_submitted() {
    let now = crate::streamer::audio::host_now_ns();
    LAST_REAL_NS.fetch_max(now, Relaxed);
    if ACTIVE.load(Relaxed) == 2 {
        if let Some(t) = thread_handle().get() {
            let _ = UNPARKED_AT.compare_exchange(0, now, Relaxed, Relaxed);
            t.unpark();
        }
    }
}

/// Make the decision again (session start, and about once a second on the hold's thread).
fn reevaluate() {
    if SESSIONS.load(Relaxed) == 0 {
        return;
    }
    let inputs = read_inputs();
    let d = decide(&inputs);
    let code = match d.variant {
        None => 0,
        Some(Variant::Spin) => 1,
        Some(Variant::Linger) => 2,
    };
    let prev = ACTIVE.swap(code, Relaxed);
    let mut l = live().lock_recover();
    let first = l.last.is_none();
    if first || prev != code {
        if !first {
            l.changes += 1;
        }
        match d.variant {
            Some(v) => tprintln!("[tune] performance hold on ({}): {}", v.as_str(), d.why),
            None if !first || STARTED_ONCE.load(Relaxed) => {
                tprintln!("[tune] performance hold off: {}", d.why)
            }
            None => tprintln!("[tune] performance hold not started: {}", d.why),
        }
        STARTED_ONCE.store(true, Relaxed);
    }
    l.last = Some((d, inputs));
}

fn run() {
    let rc = super::qos::pin_current_thread_user_interactive();
    if rc != 0 {
        teprintln!("[tune] perf_hold: USER_INTERACTIVE QoS refused (rc {rc})");
    }
    let mut checked = Instant::now();
    loop {
        if SESSIONS.load(Relaxed) == 0 {
            std::thread::park();
            checked = Instant::now();
            continue;
        }
        if DECIDE_NOW.swap(false, Relaxed) || checked.elapsed() >= Duration::from_secs(1) {
            checked = Instant::now();
            reevaluate();
        }
        let spin = match ACTIVE.load(Relaxed) {
            1 => true,
            2 => Linger {
                last_real_ns: LAST_REAL_NS.load(Relaxed),
            }
            .should_spin(crate::streamer::audio::host_now_ns(), linger_ns()),
            _ => false,
        };
        if !spin {
            UNPARKED_AT.store(0, Relaxed);
            std::thread::park_timeout(Duration::from_millis(250));
            let at = UNPARKED_AT.swap(0, Relaxed);
            if at != 0 {
                let d = crate::streamer::audio::host_now_ns().saturating_sub(at);
                WAKES.fetch_add(1, Relaxed);
                WAKE_NS_SUM.fetch_add(d, Relaxed);
                WAKE_NS_MAX.fetch_max(d, Relaxed);
            }
            continue;
        }
        // short slices, so the session, the decision and the linger are re-read often
        let t0 = Instant::now();
        for _ in 0..256 {
            std::hint::spin_loop();
        }
        SPIN_NS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_serious_thermal_state_turns_the_hold_off() {
        let at = |t| {
            let i = Inputs {
                thermal: t,
                ..inputs(Some(2), PowerSource::Ac, Some(false))
            };
            decide(&i).variant
        };
        assert_eq!(at(None), Some(Variant::Linger));
        assert_eq!(at(Some(1)), Some(Variant::Linger), "fair");
        assert_eq!(at(Some(2)), None, "serious");
        assert_eq!(at(Some(3)), None, "critical");
        let forced = Inputs {
            env: Some("1".into()),
            thermal: Some(3),
            ..inputs(Some(2), PowerSource::Ac, Some(false))
        };
        assert_eq!(decide(&forced).variant, Some(Variant::Linger), "forced");
    }

    fn inputs(levels: Option<i64>, power: PowerSource, lpm: Option<bool>) -> Inputs {
        Inputs {
            perf_levels: levels,
            power,
            low_power_mode: lpm,
            thermal: Some(0),
            env: None,
            tune_off: false,
            tune_spin: false,
            tune_linger: false,
        }
    }

    #[test]
    fn the_default_policy_wants_efficiency_cores_ac_power_and_no_low_power_mode() {
        use PowerSource::*;
        let on = Some(Variant::Linger);
        // M1 on the charger
        assert_eq!(decide(&inputs(Some(2), Ac, Some(false))).variant, on);
        assert_eq!(
            decide(&inputs(Some(2), Ac, Some(false))).why,
            "2 perf levels, AC power"
        );
        // Low Power Mode unreadable (before macOS 12) counts as off
        assert_eq!(decide(&inputs(Some(2), Ac, None)).variant, on);
        // each condition alone turns it off
        assert_eq!(decide(&inputs(Some(2), Battery, Some(false))).variant, None);
        assert_eq!(decide(&inputs(Some(2), Ups, Some(false))).variant, None);
        assert_eq!(decide(&inputs(Some(2), Unknown, Some(false))).variant, None);
        assert_eq!(decide(&inputs(Some(2), Ac, Some(true))).variant, None);
        assert_eq!(decide(&inputs(Some(1), Ac, Some(false))).variant, None);
        // Intel: no hw.nperflevels at all
        assert_eq!(decide(&inputs(None, Ac, None)).variant, None);
        assert!(decide(&inputs(Some(2), Battery, None))
            .why
            .contains("battery"));
    }

    #[test]
    fn the_overrides_take_precedence_in_order() {
        use PowerSource::*;
        let with = |env: Option<&str>, off: bool, spin: bool, linger: bool, base: Inputs| Inputs {
            env: env.map(str::to_string),
            tune_off: off,
            tune_spin: spin,
            tune_linger: linger,
            ..base
        };
        let eligible = inputs(Some(2), Ac, Some(false));
        let ineligible = inputs(None, Battery, Some(true));
        // SE_PERF_HOLD wins over everything
        assert_eq!(
            decide(&with(Some("0"), false, true, true, eligible.clone())).variant,
            None
        );
        assert_eq!(
            decide(&with(Some("1"), true, false, false, ineligible.clone())).variant,
            Some(Variant::Linger)
        );
        assert_eq!(
            decide(&with(Some("spin"), true, false, false, ineligible.clone())).variant,
            Some(Variant::Spin)
        );
        // an unknown value is ignored
        assert_eq!(
            decide(&with(Some("yes"), false, false, false, eligible.clone())).variant,
            Some(Variant::Linger)
        );
        // then the plan switches: a forced variant runs even where the default would not...
        assert_eq!(
            decide(&with(None, false, true, false, ineligible.clone())).variant,
            Some(Variant::Spin)
        );
        assert_eq!(
            decide(&with(None, false, false, true, ineligible.clone())).variant,
            Some(Variant::Linger)
        );
        // ...and `perf_hold` in the off list disables the default
        assert_eq!(
            decide(&with(None, true, false, false, eligible)).variant,
            None
        );
    }

    #[test]
    fn only_a_captured_frame_starts_or_extends_the_linger() {
        const MS: u64 = 1_000_000;
        let linger = 200 * MS;
        let mut l = Linger::default();
        // nothing captured yet (a still screen from the start, keepalive only): never spins
        for t in [0, 100, 1_000, 5_000] {
            assert!(!l.should_spin(t * MS, linger));
        }
        // a captured frame starts it
        l.real(1_000 * MS);
        assert!(l.should_spin(1_000 * MS, linger));
        assert!(l.should_spin(1_199 * MS, linger));
        // repeats and pushes do not call `real`: it parks 200 ms after the last captured frame
        assert!(!l.should_spin(1_200 * MS, linger));
        // sparse but active content (10 fps) keeps it warm throughout
        for k in 0..20u64 {
            let t = 2_000 * MS + k * 100 * MS;
            l.real(t);
            assert!(l.should_spin(t + 99 * MS, linger));
        }
        // an out-of-order (older) stamp does not shorten it
        l.real(2_500 * MS);
        assert!(l.should_spin(4_000 * MS, linger));
        assert!(!l.should_spin(4_100 * MS, linger));
    }
}

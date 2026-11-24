#![allow(non_snake_case)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObject};
use objc2::{extern_class, extern_methods, sel, AnyThread};
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop, CGPoint, CGSize};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayCopyAllDisplayModes, CGDisplayMode,
    CGDisplayPixelsHigh, CGDisplayPixelsWide, CGDisplayRegisterReconfigurationCallback,
    CGDisplaySetDisplayMode, CGError, CGGetActiveDisplayList, CGMainDisplayID,
};
use objc2_foundation::{NSArray, NSString};

use crate::streamer::session::{SharedVirtualDisplay, VirtualDisplayController};

extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut AnyObject;
}
const DISPATCH_QUEUE_PRIORITY_HIGH: isize = 2;
const DEFAULT_PPI: f64 = 96.0;
/// MAC-DISPLAY-4K. The declared pixel density of a 4K-class panel (both axes at or above
/// 3840x2160). At 96 PPI such a panel is a 40-inch screen, and macOS gives it a scaled "looks
/// like" default — 3840x2160 came up at 1920x1080, 4096x2160 at 1920x1012 — whatever mode list
/// it registered, and the native mode can then only be selected through
/// `CGDisplaySetDisplayMode`, whose mode list macOS 26 withholds for every display after a
/// process's first. Declared at 200 PPI the same panel comes up at 3840x2160, first display and
/// later ones alike (measured on the M1, macOS 26.7.1; 60 PPI behaves as 96).
const LARGE_PANEL_PPI: f64 = 200.0;

fn panel_ppi(width: u32, height: u32) -> f64 {
    if width >= 3840 && height >= 2160 {
        LARGE_PANEL_PPI
    } else {
        DEFAULT_PPI
    }
}

/// A pre-registered mode is normally visible on the first `CGDisplayCopyAllDisplayModes`, so the
/// find loop should not need a second pass at all. The deadline stays as a backstop.
const MODE_FIND_TIMEOUT: Duration = Duration::from_millis(1500);
const MODE_SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Poll tightly at first — a settle that takes 20 ms should cost 20 ms, not 50.
const FAST_POLL_WINDOW: Duration = Duration::from_millis(250);
const FAST_POLL: Duration = Duration::from_millis(5);
const SLOW_POLL: Duration = Duration::from_millis(25);
/// How long the settle wait sleeps between geometry reads once WindowServer's reconfiguration
/// events are reaching us. Deliberately much longer than `SLOW_POLL`: the event is what wakes it,
/// and this is only the ceiling on the cost of one that never arrives.
const MODE_SETTLE_WAKE: Duration = Duration::from_millis(100);
const REMOVAL_CONFIRM_TIMEOUT: Duration = Duration::from_millis(1000);

extern_class!(
    #[unsafe(super(NSObject))]
    #[derive(Debug)]
    pub struct CGVirtualDisplayDescriptor;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[derive(Debug)]
    pub struct CGVirtualDisplayMode;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[derive(Debug)]
    pub struct CGVirtualDisplaySettings;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[derive(Debug)]
    pub struct CGVirtualDisplay;
);

impl CGVirtualDisplayDescriptor {
    extern_methods!(
        #[unsafe(method(init))]
        #[unsafe(method_family = init)]
        unsafe fn init(this: Allocated<Self>) -> Retained<Self>;

        #[unsafe(method(setName:))]
        unsafe fn setName(&self, name: &NSString);

        #[unsafe(method(setMaxPixelsWide:))]
        unsafe fn setMaxPixelsWide(&self, v: u32);

        #[unsafe(method(setMaxPixelsHigh:))]
        unsafe fn setMaxPixelsHigh(&self, v: u32);

        #[unsafe(method(setSizeInMillimeters:))]
        unsafe fn setSizeInMillimeters(&self, size: CGSize);

        #[unsafe(method(setVendorID:))]
        unsafe fn setVendorID(&self, v: u32);

        #[unsafe(method(setProductID:))]
        unsafe fn setProductID(&self, v: u32);

        #[unsafe(method(setSerialNum:))]
        unsafe fn setSerialNum(&self, v: u32);

        #[unsafe(method(setRedPrimary:))]
        unsafe fn setRedPrimary(&self, p: CGPoint);

        #[unsafe(method(setGreenPrimary:))]
        unsafe fn setGreenPrimary(&self, p: CGPoint);

        #[unsafe(method(setBluePrimary:))]
        unsafe fn setBluePrimary(&self, p: CGPoint);

        #[unsafe(method(setWhitePoint:))]
        unsafe fn setWhitePoint(&self, p: CGPoint);

        #[unsafe(method(setQueue:))]
        unsafe fn setQueue(&self, queue: *mut AnyObject);

        #[unsafe(method(setTerminationHandler:))]
        unsafe fn setTerminationHandler(&self, handler: &block2::Block<dyn Fn()>);
    );
}

impl CGVirtualDisplayMode {
    extern_methods!(
        #[unsafe(method(initWithWidth:height:refreshRate:))]
        #[unsafe(method_family = init)]
        unsafe fn initWithWidth_height_refreshRate(
            this: Allocated<Self>,
            width: u32,
            height: u32,
            refreshRate: f64,
        ) -> Retained<Self>;
    );
}

impl CGVirtualDisplaySettings {
    extern_methods!(
        #[unsafe(method(init))]
        #[unsafe(method_family = init)]
        unsafe fn init(this: Allocated<Self>) -> Retained<Self>;

        #[unsafe(method(setModes:))]
        unsafe fn setModes(&self, modes: &NSArray<CGVirtualDisplayMode>);

        #[unsafe(method(setHiDPI:))]
        unsafe fn setHiDPI(&self, v: u32);
    );
}

impl CGVirtualDisplay {
    extern_methods!(
        #[unsafe(method(initWithDescriptor:))]
        #[unsafe(method_family = init)]
        unsafe fn initWithDescriptor(
            this: Allocated<Self>,
            descriptor: &CGVirtualDisplayDescriptor,
        ) -> Option<Retained<Self>>;

        #[unsafe(method(applySettings:))]
        unsafe fn applySettings(&self, settings: &CGVirtualDisplaySettings) -> bool;

        #[unsafe(method(displayID))]
        unsafe fn displayID(&self) -> u32;
    );
}

/// What we last handed to `applySettings:`. `width`/`height` are always **backing pixels**; the
/// registered modes are in **points**, which is the same thing at `hidpi == 0` and half at
/// `hidpi == 1`.
#[derive(Clone, Debug, PartialEq)]
struct AppliedSettings {
    width: u32,
    height: u32,
    refresh: f64,
    hidpi: u32,
    /// The registered mode list, in points. A geometry change that lands inside this list is a
    /// plain `CGDisplaySetDisplayMode`; one that does not needs a fresh `applySettings:`.
    modes: Vec<(u32, u32)>,
}

impl AppliedSettings {
    fn logical(&self) -> (u32, u32) {
        to_logical((self.width, self.height), self.hidpi)
    }
}

fn to_logical((width, height): (u32, u32), hidpi: u32) -> (u32, u32) {
    if hidpi == 1 {
        (width / 2, height / 2)
    } else {
        (width, height)
    }
}

/// The mode list to register at `applySettings:` time, in points.
///
/// macOS is not Windows here: `hiDPI` is one flag over the whole settings object, so the DPR
/// ladder cannot be expressed as a mode list the way the IDD does it. What a mode list *does* buy
/// on macOS is every geometry the session can reach without another `applySettings:` — the primary
/// mode, its transpose (so an orientation flip is a pure mode switch), and the ladder's other
/// resolutions with their transposes.
fn mode_list(width: u32, height: u32, hidpi: u32, extra_backing: &[(u32, u32)]) -> Vec<(u32, u32)> {
    fn push(modes: &mut Vec<(u32, u32)>, m: (u32, u32)) {
        if m.0 >= 2 && m.1 >= 2 && !modes.contains(&m) {
            modes.push(m);
        }
    }

    let mut modes: Vec<(u32, u32)> = Vec::new();
    let primary = to_logical((width, height), hidpi);
    push(&mut modes, primary);
    push(&mut modes, (primary.1, primary.0));
    for &entry in extra_backing {
        let logical = to_logical(entry, hidpi);
        push(&mut modes, logical);
        push(&mut modes, (logical.1, logical.0));
    }
    modes
}

unsafe fn make_settings(
    modes: &[(u32, u32)],
    refresh_rate: f64,
    hidpi: u32,
) -> Retained<CGVirtualDisplaySettings> {
    let settings = CGVirtualDisplaySettings::init(CGVirtualDisplaySettings::alloc());
    settings.setHiDPI(hidpi);
    let objects: Vec<Retained<CGVirtualDisplayMode>> = modes
        .iter()
        .map(|&(w, h)| {
            CGVirtualDisplayMode::initWithWidth_height_refreshRate(
                CGVirtualDisplayMode::alloc(),
                w,
                h,
                refresh_rate,
            )
        })
        .collect();
    settings.setModes(&NSArray::from_retained_slice(&objects));
    settings
}

/// `refreshDeadline`, `rotation` and `isReference` were added to `CGVirtualDisplaySettings`
/// somewhere between 10.14.6 and the present. Log what this machine has, once, so a report from
/// the field says which levers were even available. Nothing is set: `refreshDeadline` is the only
/// latency-named knob on the WindowServer side of this boundary, but its units and semantics are
/// unpublished and it cannot be measured on the 10.15 floor, where the property does not exist.
fn report_settings_surface() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        if !cg_virtual_display_available() {
            return;
        }
        let settings = unsafe { CGVirtualDisplaySettings::init(CGVirtualDisplaySettings::alloc()) };
        let responds = |selector| -> bool {
            unsafe { objc2::msg_send![&*settings, respondsToSelector: selector] }
        };
        tprintln!(
            "[display] CGVirtualDisplaySettings: refreshDeadline {}, rotation {}, isReference {}",
            if responds(sel!(setRefreshDeadline:)) {
                "present"
            } else {
                "absent"
            },
            if responds(sel!(setRotation:)) {
                "present"
            } else {
                "absent"
            },
            if responds(sel!(setIsReference:)) {
                "present"
            } else {
                "absent"
            }
        );
    });
}

/// WindowServer can tear a virtual display down under us — a log out, a fast user switch, a real
/// display hot-plug that forces a topology rebuild. The descriptor has always been given a
/// dispatch queue for exactly this callback; until now nothing was registered on it, so the
/// registry kept a live handle to a display that no longer existed and the session kept streaming
/// a display id that resolved to nothing.
unsafe fn install_termination_handler(desc: &CGVirtualDisplayDescriptor, token: u32) {
    let responds: bool = objc2::msg_send![desc, respondsToSelector: sel!(setTerminationHandler:)];
    if !responds {
        return;
    }
    let handler = RcBlock::new(move || {
        let removed = {
            let mut guard = registry().lock().unwrap();
            let id = guard
                .displays
                .iter()
                .find(|(_, entry)| entry.token == token)
                .map(|(id, _)| *id);
            id.and_then(|id| guard.displays.remove(&id).map(|_| id))
        };
        match removed {
            Some(id) => teprintln!(
                "[display] WindowServer terminated virtual display {id}; dropping its handle"
            ),
            None => teprintln!(
                "[display] WindowServer terminated a virtual display we had already released"
            ),
        }
    });
    desc.setTerminationHandler(&handler);
    // The handler outlives every Rust owner: WindowServer may run it at any point up to and past
    // the display's own teardown, so the block is deliberately never freed. It is one small
    // allocation per display created.
    std::mem::forget(handler);
}

/// The display's serial number, derived from its name so that the same client device gets the same
/// display identity every time.
///
/// macOS keys per-display state — arrangement position, colour profile, resolution preference — on
/// vendor/product/serial. The serial used to be a process-local counter starting at 1, so the
/// *first* device to join after every launch inherited whatever arrangement the previous run's
/// first device had left behind, and a returning device never got its own back. `name` is
/// `"ScreenExtend - <device name>"`, which is the most stable per-device identity that reaches this
/// layer.
///
/// FNV-1a, folded away from 0 because a serial of 0 reads as "unset".
///
/// MAC-DISPLAY-ORIGIN: the identity is the device name AND the size it is created at
/// ([`display_identity`]). WindowServer restores the geometry it remembers for an identity, and
/// with one identity per device a size change came up at the old size and, once re-applied, at the
/// old origin — 1920 wide at x = -800 over the built-in panel — where no origin change could move
/// it (WindowServer still snaps a left neighbour to the remembered width: -1920 is read back as
/// -800, and on every display after a process's first `CGConfigureDisplayOrigin` fails outright).
/// Per device and size, what is remembered is always a geometry of exactly this size, so the
/// user's arrangement for it comes back and can never overlap.
fn display_serial(name: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash | 1
}

/// The string a display's serial is derived from: the device's display name and the size.
fn display_identity(name: &str, width: u32, height: u32) -> String {
    format!("{name} @ {width}x{height}")
}

#[cfg(test)]
unsafe fn create_cg_virtual_display(
    name: &str,
    width: u32,
    height: u32,
    refresh_rate: f64,
    hidpi: u32,
    extra_modes: &[(u32, u32)],
) -> Result<(Retained<CGVirtualDisplay>, AppliedSettings, u32), String> {
    let identity = display_identity(name, width, height);
    create_with_identity(
        name,
        &identity,
        width,
        height,
        refresh_rate,
        hidpi,
        extra_modes,
    )
}

unsafe fn create_with_identity(
    name: &str,
    identity: &str,
    width: u32,
    height: u32,
    refresh_rate: f64,
    hidpi: u32,
    extra_modes: &[(u32, u32)],
) -> Result<(Retained<CGVirtualDisplay>, AppliedSettings, u32), String> {
    report_settings_surface();

    let modes = mode_list(width, height, hidpi, extra_modes);
    let settings = make_settings(&modes, refresh_rate, hidpi);

    let pixel_factor = if hidpi == 1 { 2 } else { 1 };
    let max_wide = modes.iter().map(|m| m.0).max().unwrap_or(width) * pixel_factor;
    let max_high = modes.iter().map(|m| m.1).max().unwrap_or(height) * pixel_factor;

    let desc = CGVirtualDisplayDescriptor::init(CGVirtualDisplayDescriptor::alloc());
    desc.setQueue(dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0));
    desc.setName(&NSString::from_str(name));

    desc.setWhitePoint(CGPoint {
        x: 0.3125,
        y: 0.3291,
    });
    desc.setBluePrimary(CGPoint {
        x: 0.1494,
        y: 0.0557,
    });
    desc.setGreenPrimary(CGPoint {
        x: 0.2559,
        y: 0.6983,
    });
    desc.setRedPrimary(CGPoint {
        x: 0.6797,
        y: 0.3203,
    });

    desc.setMaxPixelsWide(max_wide);
    desc.setMaxPixelsHigh(max_high);
    #[cfg(test)]
    let ppi = std::env::var("SE_EXP_PPI")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(panel_ppi(width, height));
    #[cfg(not(test))]
    let ppi = panel_ppi(width, height);
    desc.setSizeInMillimeters(CGSize {
        width: 25.4 * width as f64 / ppi,
        height: 25.4 * height as f64 / ppi,
    });
    desc.setSerialNum(display_serial(identity));
    desc.setProductID(OUR_PRODUCT);
    desc.setVendorID(OUR_VENDOR);
    // Not the descriptor's serial: two clients can carry the same device name, and the termination
    // handler has to name exactly one registry entry.
    static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    install_termination_handler(&desc, token);

    let display = CGVirtualDisplay::initWithDescriptor(CGVirtualDisplay::alloc(), &desc)
        .ok_or_else(|| {
            "CGVirtualDisplay initWithDescriptor returned nil — the private API is \
             unavailable or the app is missing the com.apple.CG.virtual-display \
             entitlement"
                .to_string()
        })?;

    if !display.applySettings(&settings) {
        return Err(
            "CGVirtualDisplay applySettings returned NO — the requested mode is \
             invalid or the display was already destroyed"
                .to_string(),
        );
    }

    let applied = AppliedSettings {
        width,
        height,
        refresh: refresh_rate,
        hidpi,
        modes,
    };
    Ok((display, applied, token))
}

fn poll_interval(started: Instant) -> Duration {
    if started.elapsed() < FAST_POLL_WINDOW {
        FAST_POLL
    } else {
        SLOW_POLL
    }
}

/// Bumped once per completed WindowServer reconfiguration. A generation counter rather than a bare
/// notify: the waiter reads it *before* it reads the geometry, so a reconfiguration that lands
/// between the two cannot be lost.
struct ReconfigureSignal {
    generation: Mutex<u64>,
    changed: Condvar,
}

fn reconfigure_signal() -> &'static ReconfigureSignal {
    static SIGNAL: OnceLock<ReconfigureSignal> = OnceLock::new();
    SIGNAL.get_or_init(|| ReconfigureSignal {
        generation: Mutex::new(0),
        changed: Condvar::new(),
    })
}

/// What WindowServer has told this process about one display id. Ids are reused — on 10.15 every
/// display a process creates gets the SAME id — and for some milliseconds after a create CoreGraphics
/// still answers for the display that was just removed, so a reading is evidence about a display
/// only after an Add event newer than the create (MAC-DISPLAY-FRESH).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct IdEvents {
    adds: u64,
    removes: u64,
}

fn id_events() -> &'static Mutex<HashMap<u32, IdEvents>> {
    static EVENTS: OnceLock<Mutex<HashMap<u32, IdEvents>>> = OnceLock::new();
    EVENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn events_of(id: u32) -> IdEvents {
    use crate::streamer::lock::LockExt;
    id_events()
        .lock_recover()
        .get(&id)
        .copied()
        .unwrap_or_default()
}

fn note_event(map: &mut HashMap<u32, IdEvents>, id: u32, added: bool, removed: bool) {
    let e = map.entry(id).or_default();
    e.removes += u64::from(removed);
    e.adds += u64::from(added);
}

/// Every reconfiguration callback this process has had, Add/Remove or not.
static CALLBACKS: AtomicU32 = AtomicU32::new(0);

/// What says that readings for an id are about the display just created.
///
/// Add events are the direct evidence but do not reach every process (none did in the bench host
/// on macOS 26.7.1). The serial always can: it is ours and encodes device and size
/// ([`display_identity`]), so while CoreGraphics still answers for a predecessor of another size
/// it answers with that one's serial; and a predecessor with the same serial has the same size,
/// where a stale reading changes nothing. An id this process never had before has no
/// predecessor.
#[derive(Clone, Copy, Debug)]
struct Freshness {
    before: IdEvents,
    id_used_before: bool,
    serial: u32,
}

impl Freshness {
    fn is_fresh(&self, now: IdEvents, read_serial: u32) -> bool {
        now.adds > self.before.adds || read_serial == self.serial || !self.id_used_before
    }
}

unsafe extern "C-unwind" fn on_display_reconfigured(
    display: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    _user_info: *mut std::ffi::c_void,
) {
    // The "begin" pass fires before anything has moved. Only the completion pass means the
    // geometry is readable.
    if flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
        return;
    }
    CALLBACKS.fetch_add(1, Ordering::Relaxed);
    {
        use crate::streamer::lock::LockExt;
        let added = flags.contains(CGDisplayChangeSummaryFlags::AddFlag);
        let removed = flags.contains(CGDisplayChangeSummaryFlags::RemoveFlag);
        if added || removed {
            note_event(&mut id_events().lock_recover(), display, added, removed);
        }
    }
    let signal = reconfigure_signal();
    if let Ok(mut generation) = signal.generation.lock() {
        *generation += 1;
    }
    signal.changed.notify_all();
}

/// Whether WindowServer's reconfiguration events are reaching us.
///
/// `CGDisplayRegisterReconfigurationCallback` is 10.3+, but it delivers on the run loop of the
/// thread that registered it — and the join path runs on a `spawn_blocking` worker with no run
/// loop of its own. So the registration lives on a thread that exists to run one. The answer is
/// only ever an accelerator: `wait_for_settle` re-reads the geometry on a bounded timeout whether
/// or not an event ever arrives, so a machine where the callback never fires is no slower than the
/// poll it replaced.
fn reconfiguration_events_available() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("cg-display-reconfig".to_string())
            .spawn(move || {
                let err = unsafe {
                    CGDisplayRegisterReconfigurationCallback(
                        Some(on_display_reconfigured),
                        std::ptr::null_mut(),
                    )
                };
                let _ = tx.send(err == CGError::Success);
                if err != CGError::Success {
                    return;
                }
                let mode = unsafe { kCFRunLoopDefaultMode };
                loop {
                    // A run loop with no sources left returns immediately, so the sleep is what
                    // keeps an unexpectedly empty one from spinning a core.
                    CFRunLoop::run_in_mode(mode, 1.0e9, false);
                    std::thread::sleep(Duration::from_millis(250));
                }
            })
            .is_ok();
        let registered = spawned && rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false);
        if !registered {
            teprintln!(
                "[display] CGDisplayRegisterReconfigurationCallback did not take; falling back to \
                 polling for mode changes to settle"
            );
        }
        registered
    })
}

/// Wait for `display_id` to report `width x height` points.
///
/// Event-driven where WindowServer will tell us, with the previous poll cadence as the ceiling on
/// how long a missed event can cost.
fn wait_for_settle(display_id: u32, width: u32, height: u32) -> Result<(), String> {
    wait_for_settle_until(
        display_id,
        width,
        height,
        Instant::now() + MODE_SETTLE_TIMEOUT,
    )
}

fn wait_for_settle_until(
    display_id: u32,
    width: u32,
    height: u32,
    deadline: Instant,
) -> Result<(), String> {
    let started = Instant::now();
    let events = reconfiguration_events_available();
    let signal = reconfigure_signal();

    loop {
        let seen = *signal.generation.lock().unwrap();

        let w = unsafe { CGDisplayPixelsWide(display_id) } as u32;
        let h = unsafe { CGDisplayPixelsHigh(display_id) } as u32;
        if w == width && h == height {
            return Ok(());
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(format!(
                "display {display_id} did not settle to {width}x{height} (still {w}x{h})"
            ));
        }

        if events {
            // A reconfiguration that completed while we were reading the geometry has already
            // bumped the counter, so this never sleeps through one.
            let guard = signal.generation.lock().unwrap();
            if *guard == seen {
                let _unused = signal
                    .changed
                    .wait_timeout(guard, MODE_SETTLE_WAKE.min(deadline - now))
                    .unwrap();
            }
        } else {
            std::thread::sleep(poll_interval(started).min(deadline - now));
        }
    }
}

pub(crate) fn activate_mode(display_id: u32, width: u32, height: u32) -> Result<(), String> {
    if unsafe { CGDisplayPixelsWide(display_id) } as u32 == width
        && unsafe { CGDisplayPixelsHigh(display_id) } as u32 == height
    {
        return Ok(());
    }

    let started = Instant::now();
    let mut nil_polls = 0u32;
    let switched = unsafe {
        let find_deadline = started + MODE_FIND_TIMEOUT;
        loop {
            // A display created a moment ago is not online yet: `CGDisplayCopyAllDisplayModes`
            // answers nil until WindowServer has brought it up. That is "not yet", exactly like a
            // mode missing from the list, and gets the same bounded retry. Failing at the first
            // nil left the display in whatever mode macOS restored for this display identity —
            // the last size that identity had — instead of the one the client asked for.
            let Some(modes) = CGDisplayCopyAllDisplayModes(display_id, None) else {
                nil_polls += 1;
                if Instant::now() < find_deadline {
                    std::thread::sleep(poll_interval(started));
                    continue;
                }
                return Err(format!(
                    "CGDisplayCopyAllDisplayModes({display_id}) still nil after {} ms \
                     ({nil_polls} polls)",
                    started.elapsed().as_millis()
                ));
            };
            if nil_polls > 0 {
                tprintln!(
                    "[display] display {display_id}'s mode list appeared after {:.1} ms \
                     ({nil_polls} nil poll(s))",
                    started.elapsed().as_secs_f64() * 1e3
                );
                nil_polls = 0;
            }
            let count = modes.count();
            let mut chosen: Option<&CGDisplayMode> = None;
            for i in 0..count {
                let ptr = modes.value_at_index(i) as *const CGDisplayMode;
                if ptr.is_null() {
                    continue;
                }
                let mode = &*ptr;
                if CGDisplayMode::width(Some(mode)) as u32 == width
                    && CGDisplayMode::height(Some(mode)) as u32 == height
                {
                    chosen = Some(mode);
                    break;
                }
            }
            match chosen {
                Some(mode) => {
                    let err = CGDisplaySetDisplayMode(display_id, Some(mode), None);
                    if err != CGError::Success {
                        return Err(format!(
                            "CGDisplaySetDisplayMode({display_id}, {width}x{height}) -> CGError {}",
                            err.0
                        ));
                    }
                    break;
                }
                None if Instant::now() < find_deadline => {
                    std::thread::sleep(poll_interval(started));
                    continue;
                }
                None => {
                    return Err(format!(
                        "no {width}x{height} mode available on display {display_id}"
                    ));
                }
            }
        }
        true
    };

    if switched {
        return wait_for_settle(display_id, width, height);
    }
    Ok(())
}

struct DisplayEntry {
    display: Retained<CGVirtualDisplay>,
    applied: AppliedSettings,
    /// The DPR ladder this display was created with, in backing pixels. Kept so a later
    /// `applySettings:` can re-register the same set of reachable geometries instead of shrinking
    /// the mode list to the one mode it happens to be changing to.
    ladder: Vec<(u32, u32)>,
    token: u32,
}

struct DisplayRegistry {
    displays: HashMap<u32, DisplayEntry>,
}

unsafe impl Send for DisplayRegistry {}

fn registry() -> &'static Mutex<DisplayRegistry> {
    static REGISTRY: OnceLock<Mutex<DisplayRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(DisplayRegistry {
            displays: HashMap::new(),
        })
    })
}

/// The geometry we last applied to a display we created, in backing pixels, plus its refresh and
/// HiDPI flag. `CGDisplayPixelsWide` reports *points*, so a caller that wants to re-apply a
/// geometry cannot read it back from CoreGraphics without halving it a second time on every HiDPI
/// round trip.
pub fn applied_backing(device_name: &str) -> Option<(u32, u32, u32, u32)> {
    let id: u32 = device_name.trim().parse().ok()?;
    let guard = registry().lock().unwrap();
    let entry = guard.displays.get(&id)?;
    Some((
        entry.applied.width,
        entry.applied.height,
        entry.applied.refresh.round() as u32,
        entry.applied.hidpi,
    ))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisplayBackend {
    CgVirtualDisplay,
    AirPlayFallback,
    Unsupported,
}

impl DisplayBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            DisplayBackend::CgVirtualDisplay => "CGVirtualDisplay",
            DisplayBackend::AirPlayFallback => "AirPlayFallback",
            DisplayBackend::Unsupported => "Unsupported",
        }
    }
}

pub fn cg_virtual_display_available() -> bool {
    use objc2::runtime::AnyClass;
    AnyClass::get(c"CGVirtualDisplay").is_some()
        && AnyClass::get(c"CGVirtualDisplayDescriptor").is_some()
        && AnyClass::get(c"CGVirtualDisplaySettings").is_some()
        && AnyClass::get(c"CGVirtualDisplayMode").is_some()
}

pub fn cd_virtual_display_available() -> bool {
    let name = std::ffi::CString::new("CDVirtualDisplayCreate").unwrap();
    !unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }.is_null()
}

pub fn probe_display_backend() -> DisplayBackend {
    static CACHED: OnceLock<DisplayBackend> = OnceLock::new();
    *CACHED.get_or_init(|| {
        if cg_virtual_display_available() {
            return DisplayBackend::CgVirtualDisplay;
        }
        if cd_virtual_display_available() {
            tprintln!(
                "[display] CGVirtualDisplay is absent but CoreDisplay exports CDVirtualDisplayCreate \
                 — a direct C path may be possible on this OS; please report this"
            );
        }
        DisplayBackend::AirPlayFallback
    })
}

pub fn new_shared_controller() -> Option<SharedVirtualDisplay> {
    match probe_display_backend() {
        DisplayBackend::CgVirtualDisplay => MacosVirtualDisplay::new_shared(),
        DisplayBackend::AirPlayFallback => {
            tprintln!(
                "[display] using the AirPlay fallback to create displays (CGVirtualDisplay {})",
                if cg_virtual_display_available() {
                    "is present but overridden"
                } else {
                    "is absent on this macOS version"
                }
            );
            super::airplay::AirPlayVirtualDisplay::new_shared()
        }
        DisplayBackend::Unsupported => None,
    }
}

/// Flip a display we created between 1x and HiDPI without changing its geometry.
///
/// The caller used to compute this from `CGDisplayPixelsWide`, which reports points — so asking
/// for HiDPI on a display that was already HiDPI halved it again. Working from the geometry we
/// remember applying makes the call idempotent, which in turn makes it free on the join path now
/// that the display is born at the client's DPR.
pub fn set_display_hidpi(device_name: &str, hidpi: u32) -> Result<(), String> {
    let Some((width, height, refresh, current)) = applied_backing(device_name) else {
        // Not a display we created — the AirPlay tier owns its own mode list.
        return Ok(());
    };
    if current == hidpi {
        return Ok(());
    }
    reconfigure_display(device_name, width, height, refresh, hidpi)
}

pub fn reconfigure_display(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
    hidpi: u32,
) -> Result<(), String> {
    let id: u32 = device_name
        .trim()
        .parse()
        .map_err(|_| format!("invalid display id {device_name:?}"))?;
    let refresh = if refresh == 0 { 60.0 } else { refresh as f64 };

    if id == unsafe { CGMainDisplayID() } && !super::airplay::owns_display(id) {
        return Err(format!(
            "refusing to reconfigure display {id}: it is this Mac's main display, not a display ScreenExtend created"
        ));
    }

    if super::airplay::owns_display(id) {
        let (logical_w, logical_h) = to_logical((width, height), hidpi);
        // Deliberately not `activate_mode`. That is `CGDisplaySetDisplayMode`,
        // which scopes the change to the calling process and makes macOS hold
        // the display open until that process exits — measured at 48 s past the
        // end of a session, which is exactly why `airplay::topology` routes
        // every mode change through a `kCGConfigurePermanently` configuration
        // instead. Calling it here, on the display that module had just
        // created, undid the precaution on every single join.
        return super::airplay::topology::set_mode_permanently(id, logical_w, logical_h).map_err(|e| {
            let ladder = super::airplay::topology::available_modes(id)
                .iter()
                .map(|(w, h, _)| format!("{w}x{h}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{e}. This display was created over AirPlay, so its geometry is whatever macOS negotiated — the only modes macOS published for it are: {ladder}"
            )
        });
    }

    // Clone the handle out and drop the guard before messaging ObjC: `applySettings:` is a
    // synchronous WindowServer round trip, and holding the registry across it serializes every
    // other client's join and leave behind one client's settings change.
    let (display, applied, ladder) = {
        let guard = registry().lock().unwrap();
        let entry = guard
            .displays
            .get(&id)
            .ok_or_else(|| format!("display {id} is not a known virtual display"))?;
        (
            entry.display.clone(),
            entry.applied.clone(),
            entry.ladder.clone(),
        )
    };

    let (logical_w, logical_h) = to_logical((width, height), hidpi);
    // SCREENS-GUARD: before our display is touched.
    let screens = foreign_screens();

    // A geometry already covered by the registered mode list at the same HiDPI is a plain mode
    // switch — no mode-list rewrite, no WindowServer settings round trip.
    let covered = applied.hidpi == hidpi
        && applied.refresh == refresh
        && applied.modes.contains(&(logical_w, logical_h));
    if covered {
        if applied.logical() != (logical_w, logical_h) {
            let mut guard = registry().lock().unwrap();
            if let Some(entry) = guard.displays.get_mut(&id) {
                entry.applied.width = width;
                entry.applied.height = height;
            }
        }
    } else {
        let modes = mode_list(width, height, hidpi, &ladder);
        let settings = unsafe { make_settings(&modes, refresh, hidpi) };
        if !unsafe { display.applySettings(&settings) } {
            return Err(format!(
                "applySettings({width}x{height}@{refresh}) returned NO for display {id}"
            ));
        }
        let mut guard = registry().lock().unwrap();
        if let Some(entry) = guard.displays.get_mut(&id) {
            entry.applied = AppliedSettings {
                width,
                height,
                refresh,
                hidpi,
                modes,
            };
        }
    }

    let result = ensure_geometry(
        id,
        logical_w,
        logical_h,
        Instant::now() + ATTEMPT_BUDGET,
        &mut Steps::new(),
    );
    watch_foreign_screens("resized", screens);
    result
}

/// How long a display may take to come online (in the active list with a real size) before the
/// geometry step gives up and leaves it to the caller's own attach wait.
const ONLINE_TIMEOUT: Duration = Duration::from_millis(3000);
/// One create attempt, from its first call to a settled display.
const ATTEMPT_BUDGET: Duration = Duration::from_millis(4000);
/// How long the arrangement may take to show a configured origin.
const ORIGIN_SETTLE_TIMEOUT: Duration = Duration::from_millis(1000);

/// A display is online when it is in the active list with a non-degenerate geometry. A 10.15
/// display that has just been created reads 1x1 while it attaches — that is "not up yet", never
/// "at a remembered size" (measured: a re-apply on such a display left it never attaching, a
/// failed join, 1 of 20 runs).
fn is_online(id: u32) -> bool {
    display_is_active(id)
        && unsafe { CGDisplayPixelsWide(id) >= 64 && CGDisplayPixelsHigh(id) >= 64 }
}

/// Wait until `id` is online AND what CoreGraphics says about it is about this display (see
/// [`IdEvents`]). `Ok(true)` = both; `Ok(false)` = online, but no evidence the readings are not a
/// predecessor's (the events never came): usable, and nothing may be corrected on them.
fn wait_online(id: u32, deadline: Instant, want: Freshness) -> Result<bool, String> {
    let started = Instant::now();
    let mut online_since = None;
    loop {
        let serial = unsafe { objc2_core_graphics::CGDisplaySerialNumber(id) };
        let fresh = want.is_fresh(events_of(id), serial);
        let online = is_online(id) && !offline_for_test();
        if online {
            online_since.get_or_insert_with(Instant::now);
        } else {
            online_since = None;
        }
        if online && fresh {
            return Ok(true);
        }
        let unverified = online_since.is_some_and(|t| t.elapsed() >= UNVERIFIED_WAIT);
        if unverified || Instant::now() >= deadline {
            return if online {
                Ok(false)
            } else {
                Err(format!(
                    "display {id} not online after {} ms ({}x{}, readings {})",
                    started.elapsed().as_millis(),
                    unsafe { CGDisplayPixelsWide(id) },
                    unsafe { CGDisplayPixelsHigh(id) },
                    if fresh { "its own" } else { "not its own" },
                ))
            };
        }
        std::thread::sleep(poll_interval(started));
    }
}

/// How long an online display is given to show that its readings are its own.
const UNVERIFIED_WAIT: Duration = Duration::from_millis(800);

/// Test only: how many more attempts must see their display as never coming online.
#[cfg(test)]
static OFFLINE_ATTEMPTS: AtomicU32 = AtomicU32::new(0);
fn offline_for_test() -> bool {
    #[cfg(test)]
    return OFFLINE_ATTEMPTS.load(Ordering::Relaxed) > 0;
    #[cfg(not(test))]
    false
}

/// Run the calling thread's run loop for up to `seconds`.
///
/// On the main thread this is what keeps CoreGraphics' picture of the displays current: WindowServer's
/// changes are delivered to the main run loop, and a process up for a few seconds without one can
/// stop seeing the displays it creates (0x0 for good, every attempt, while every other process
/// sees them online). The desktop app always has one (wry); `serve` parked its main thread
/// (MAC-DISPLAY-STALE).
/// Opt-in, unproven (`SE_MAIN_PUMP=1`): run the main thread's run loop in `serve` and the bench. One batch
/// of fresh processes stopped seeing their displays with main blocked and not with it running; a second
/// showed no difference, and the bench's 1440p teardown hung with it on (MAC-DISPLAY-STALE).
pub fn main_pump_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("SE_MAIN_PUMP").as_deref() == Ok("1"))
}

pub fn pump_main_run_loop(seconds: f64) {
    if !main_pump_enabled() {
        std::thread::sleep(Duration::from_secs_f64(seconds));
        return;
    }
    let mode = unsafe { kCFRunLoopDefaultMode };
    let t = Instant::now();
    CFRunLoop::run_in_mode(mode, seconds, true);
    // A run loop with nothing to wait on returns at once; do not spin on it.
    if t.elapsed().as_secs_f64() < seconds / 10.0 {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// When this process last removed a display (host ns, 0 = never).
static LAST_REMOVE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Everything this process's CoreGraphics says around `id`, for a failed attempt's log line.
fn describe_state(id: u32) -> String {
    use crate::streamer::lock::LockExt;
    use objc2_core_graphics::{CGDisplayIsActive, CGDisplayIsOnline, CGGetOnlineDisplayList};
    let (mut online, mut n) = ([0u32; 32], 0u32);
    let (mut active, mut m) = ([0u32; 32], 0u32);
    let (is_active, is_online) = unsafe {
        CGGetOnlineDisplayList(32, online.as_mut_ptr(), &mut n);
        CGGetActiveDisplayList(32, active.as_mut_ptr(), &mut m);
        (CGDisplayIsActive(id), CGDisplayIsOnline(id))
    };
    let list = &online[..(n as usize).min(32)];
    let used = used_ids().lock_recover();
    let live = registry().lock().unwrap();
    let ours = list.iter().filter(|d| used.contains(*d) && **d != id);
    let stale_ours = ours.filter(|d| !live.displays.contains_key(*d)).count();
    let last = LAST_REMOVE_NS.load(Ordering::Relaxed);
    let since = if last == 0 {
        "never".to_string()
    } else {
        let ns = crate::streamer::audio::host_now_ns().saturating_sub(last);
        format!("{} ms ago", ns / 1_000_000)
    };
    let e = events_of(id);
    format!(
        "active={is_active} online={is_online} online_list={n} {list:?} active_list={m} \
         add_events={} callbacks={} our_removed_displays_still_listed={stale_ours} last_remove={since}",
        e.adds,
        CALLBACKS.load(Ordering::Relaxed)
    )
}

/// Bench: what this process's CoreGraphics says about `id`.
#[cfg(feature = "bench")]
pub(crate) fn bench_state(id: u32) -> String {
    describe_state(id)
}

/// Bench: reconfiguration callbacks this process has had.
#[cfg(feature = "bench")]
pub(crate) fn bench_callbacks() -> u32 {
    CALLBACKS.load(Ordering::Relaxed)
}

/// Bench: register the reconfiguration callback on the CALLING thread.
#[cfg(feature = "bench")]
pub(crate) fn bench_register_here() -> bool {
    unsafe {
        CGDisplayRegisterReconfigurationCallback(
            Some(on_display_reconfigured),
            std::ptr::null_mut(),
        ) == CGError::Success
    }
}

/// Run a WindowServer call that can block on a thread of its own and give up waiting for it at
/// `limit`. The call itself cannot be cancelled: it is left to finish, its result is dropped, and
/// until it has, no further call of this kind is started (`None` at once).
fn bounded<T: 'static>(what: &str, limit: Duration, f: impl FnOnce() -> T + 'static) -> Option<T> {
    /// ObjC handles are only moved to the worker and back; nothing is shared while it runs.
    struct Carry<T>(T);
    unsafe impl<T> Send for Carry<T> {}
    impl<T> Carry<T> {
        fn into_inner(self) -> T {
            self.0
        }
    }
    static STUCK: AtomicU32 = AtomicU32::new(0);
    // Creating and tearing down must always be tried: skipping either would fail a join or leak a
    // display. Only corrections wait for an earlier call.
    let correction = !what.starts_with("display ");
    if correction && STUCK.load(Ordering::Acquire) > 0 {
        teprintln!("[display] {what} skipped: an earlier WindowServer call has not returned yet");
        return None;
    }
    let (tx, rx) = std::sync::mpsc::sync_channel::<Carry<T>>(1);
    let f = Carry(f);
    let timed_out = Arc::new(Mutex::new((false, false))); // (caller gave up, call finished)
    let state = Arc::clone(&timed_out);
    let label = what.to_string();
    let started = Instant::now();
    let spawned = std::thread::Builder::new()
        .name("cg-display-call".into())
        .spawn(move || {
            let v = f.into_inner()();
            let gave_up = {
                let mut st = state.lock().unwrap();
                st.1 = true;
                st.0
            };
            if gave_up {
                STUCK.fetch_sub(1, Ordering::Release);
                teprintln!(
                    "[display] {label} returned after {} ms, too late to be used",
                    started.elapsed().as_millis()
                );
            }
            let _ = tx.send(Carry(v));
        });
    if spawned.is_err() {
        return None;
    }
    match rx.recv_timeout(limit) {
        Ok(v) => Some(v.into_inner()),
        Err(_) => {
            {
                let mut st = timed_out.lock().unwrap();
                if !st.1 {
                    st.0 = true;
                    STUCK.fetch_add(1, Ordering::Release);
                }
            }
            teprintln!(
                "[display] {what} has not returned after {} ms; not waiting for it",
                limit.as_millis()
            );
            None
        }
    }
}

/// The steps of one attach and how long each took, for the timeline line.
struct Steps {
    started: Instant,
    last: Instant,
    parts: Vec<String>,
}

impl Steps {
    fn new() -> Self {
        let now = Instant::now();
        Steps {
            started: now,
            last: now,
            parts: Vec::new(),
        }
    }
    fn mark(&mut self, what: &str) {
        let now = Instant::now();
        self.parts.push(format!(
            "{what} {:.0}",
            now.duration_since(self.last).as_secs_f64() * 1e3
        ));
        self.last = now;
    }
}

/// Bring a display that is online, with fresh readings, to `width x height` (points) with a valid
/// arrangement. Every step is bounded by `deadline`:
///
/// 1. **Size.** Only a size that has read valid-but-wrong for `WRONG_SIZE_FOR` is corrected:
///    apply the display's settings again with the requested mode first (MAC-DISPLAY-MODE), then
///    the mode list as a fallback.
/// 2. **Arrangement.** See [`fix_arrangement`]; on the join path it only runs when the display
///    really overlaps another or is mirrored.
fn ensure_geometry(
    id: u32,
    width: u32,
    height: u32,
    deadline: Instant,
    steps: &mut Steps,
) -> Result<(), String> {
    let left = || deadline.saturating_duration_since(Instant::now());
    let wait_wrong = Instant::now();
    while !at_size(id, width, height) && wait_wrong.elapsed() < WRONG_SIZE_FOR {
        std::thread::sleep(FAST_POLL);
    }
    let mut result = Ok(());
    if !at_size(id, width, height) {
        let before = size_of(id);
        steps.mark("size-wait");
        let reapplied = bounded("applySettings", left(), move || {
            reapply_settings(id, width, height)
        })
        .unwrap_or(false);
        steps.mark("re-apply");
        let by = deadline.min(Instant::now() + MODE_SETTLE_TIMEOUT);
        let settled = reapplied && wait_for_settle_until(id, width, height, by).is_ok();
        steps.mark("settle");
        tprintln!(
            "[display] display {id} came up at {}x{}, not {width}x{height}; re-applied its \
             settings -> {}",
            before.0,
            before.1,
            if settled {
                "at size"
            } else {
                "still not at size"
            }
        );
        if !settled {
            let r = bounded("mode switch", left(), move || {
                activate_mode(id, width, height)
            })
            .unwrap_or_else(|| Err("the mode switch did not return in time".into()));
            steps.mark("mode-switch");
            if let Err(e) = r {
                let (w, h) = size_of(id);
                result = Err(format!(
                    "{e}; it is running at {w}x{h}, which is what the client gets"
                ));
            }
        }
    } else {
        steps.mark("size-ok");
    }
    fix_arrangement(id, deadline);
    steps.mark("arrangement");
    result
}

/// How long a size must read valid-but-wrong before it is believed.
const WRONG_SIZE_FOR: Duration = Duration::from_millis(150);

fn size_of(id: u32) -> (u32, u32) {
    unsafe {
        (
            CGDisplayPixelsWide(id) as u32,
            CGDisplayPixelsHigh(id) as u32,
        )
    }
}

fn at_size(id: u32, width: u32, height: u32) -> bool {
    unsafe { CGDisplayPixelsWide(id) as u32 == width && CGDisplayPixelsHigh(id) as u32 == height }
}

/// `applySettings:` again with what the display was given, the target mode (points) first.
fn reapply_settings(id: u32, width: u32, height: u32) -> bool {
    let (display, settings) = {
        let reg = registry().lock().unwrap();
        let Some(e) = reg.displays.get(&id) else {
            return false;
        };
        let a = &e.applied;
        let mut modes = a.modes.clone();
        modes.retain(|m| *m != (width, height));
        modes.insert(0, (width, height));
        (e.display.clone(), unsafe {
            make_settings(&modes, a.refresh, a.hidpi)
        })
    };
    unsafe { display.applySettings(&settings) }
}

/// `(id, x, y, w, h)` of every active display, in global points.
fn display_rects() -> Vec<(u32, i32, i32, i32, i32)> {
    let mut ids = [0u32; 16];
    let mut n: u32 = 0;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) }
        != CGError::Success
    {
        return Vec::new();
    }
    ids[..n as usize]
        .iter()
        .map(|&d| {
            let b = unsafe { objc2_core_graphics::CGDisplayBounds(d) };
            (
                d,
                b.origin.x.round() as i32,
                b.origin.y.round() as i32,
                b.size.width.round() as i32,
                b.size.height.round() as i32,
            )
        })
        .collect()
}

/// Bench only: move display `id` to the left of the main display and make WindowServer remember
/// it there, which is how a user's own arrangement teaches a display
/// identity a left-hand origin. Returns the CGError code.
#[cfg(feature = "bench")]
pub(crate) fn teach_origin_left(id: u32) -> i32 {
    use objc2_core_graphics::{
        CGBeginDisplayConfiguration, CGCompleteDisplayConfiguration, CGConfigureDisplayOrigin,
        CGConfigureOption, CGDisplayConfigRef,
    };
    let w = unsafe { objc2_core_graphics::CGDisplayBounds(id) }
        .size
        .width as i32;
    unsafe {
        let mut cfg: CGDisplayConfigRef = std::ptr::null_mut();
        if CGBeginDisplayConfiguration(&mut cfg) != CGError::Success {
            return -1;
        }
        let _ = CGConfigureDisplayOrigin(cfg, id, -w, 0);
        CGCompleteDisplayConfiguration(cfg, CGConfigureOption::ForSession).0
    }
}

/// The active displays whose rectangles overlap display `id`'s, and whether `id` is in a mirror
/// set — what a reader of the arrangement (the bench's live stage) checks a run against.
pub(crate) fn arrangement_conflicts(id: u32) -> (Vec<u32>, bool) {
    let rects = display_rects();
    let Some(&(_, x, y, w, h)) = rects.iter().find(|r| r.0 == id) else {
        return (Vec::new(), false);
    };
    let hits = rects
        .iter()
        .filter(|o| o.0 != id && x < o.1 + o.3 && o.1 < x + w && y < o.2 + o.4 && o.2 < y + h)
        .map(|o| o.0)
        .collect();
    (hits, unsafe {
        objc2_core_graphics::CGDisplayIsInMirrorSet(id)
    })
}

/// The origin that takes `me` off every display it overlaps while keeping its side: left of
/// them stays left (x = their leftmost edge − width), otherwise to their right (x = their
/// rightmost edge). `None` when it overlaps nothing. Pure.
pub(crate) fn non_overlapping_origin(
    me: (i32, i32, i32, i32),
    others: &[(i32, i32, i32, i32)],
) -> Option<(i32, i32)> {
    let (x, y, w, h) = me;
    let hits: Vec<&(i32, i32, i32, i32)> = others
        .iter()
        .filter(|o| x < o.0 + o.2 && o.0 < x + w && y < o.1 + o.3 && o.1 < y + h)
        .collect();
    if hits.is_empty() {
        return None;
    }
    let left = x < 0 || hits.iter().all(|o| x < o.0);
    let nx = if left {
        hits.iter().map(|o| o.0).min().unwrap_or(0) - w
    } else {
        hits.iter().map(|o| o.0 + o.2).max().unwrap_or(0)
    };
    Some((nx, y))
}

/// Repair the arrangement around display `id` after a size change: out of any mirror set, and off
/// any display it overlaps (moving only `id`), in one configuration transaction;
/// then wait (bounded) until `CGDisplayBounds` shows it, so every reader after this sees the
/// settled origin.
fn fix_arrangement(id: u32, deadline: Instant) {
    use objc2_core_graphics::{
        CGBeginDisplayConfiguration, CGCompleteDisplayConfiguration,
        CGConfigureDisplayMirrorOfDisplay, CGConfigureDisplayOrigin, CGConfigureOption,
        CGDisplayConfigRef, CGDisplayIsInMirrorSet,
    };
    let rects = display_rects();
    let Some(&(_, x, y, w, h)) = rects.iter().find(|r| r.0 == id) else {
        return;
    };
    let others: Vec<(i32, i32, i32, i32)> = rects
        .iter()
        .filter(|r| r.0 != id)
        .map(|r| (r.1, r.2, r.3, r.4))
        .collect();
    let mirrored = unsafe { CGDisplayIsInMirrorSet(id) };
    let target = non_overlapping_origin((x, y, w, h), &others);
    if target.is_none() && !mirrored {
        return;
    }
    let t0 = Instant::now();
    let limit = deadline.saturating_duration_since(t0);
    if limit < Duration::from_millis(50) {
        teprintln!("[display] display {id}: no time left to correct its arrangement");
        return;
    }
    let call = move || unsafe {
        let mut cfg: CGDisplayConfigRef = std::ptr::null_mut();
        if CGBeginDisplayConfiguration(&mut cfg) != CGError::Success {
            return CGError::Failure;
        }
        if mirrored {
            let _ = CGConfigureDisplayMirrorOfDisplay(cfg, id, 0);
        }
        if let Some((nx, ny)) = target {
            let _ = CGConfigureDisplayOrigin(cfg, id, nx, ny);
        }
        // ForSession, never Permanently. A permanent write saves the WHOLE configuration of the
        // set of connected displays, including the modes of the user's own screens, and macOS
        // re-applies it every time that set comes back (SCREENS-GUARD). On the M1 (26.7.1) a
        // ForSession origin on a virtual display returns success and is not honoured; the display
        // identity (name + size) is what keeps the arrangement valid, so this is a best effort.
        CGCompleteDisplayConfiguration(cfg, CGConfigureOption::ForSession)
    };
    let Some(err) = bounded(
        "CGCompleteDisplayConfiguration",
        limit.min(ORIGIN_SETTLE_TIMEOUT),
        call,
    ) else {
        return;
    };
    let mut settled = false;
    while t0.elapsed() < ORIGIN_SETTLE_TIMEOUT.min(limit) {
        let b = unsafe { objc2_core_graphics::CGDisplayBounds(id) };
        let at = (b.origin.x.round() as i32, b.origin.y.round() as i32);
        if target.is_none_or(|t| t == at) && !unsafe { CGDisplayIsInMirrorSet(id) } {
            settled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    tprintln!(
        "[display] display {id} {w}x{h} at ({x},{y}){}{}: rearranged -> {:?} (CGError {}), {} in \
         {:.0} ms",
        if target.is_some() {
            " overlapped another display"
        } else {
            ""
        },
        if mirrored { ", was mirrored" } else { "" },
        target,
        err.0,
        if settled { "settled" } else { "NOT settled" },
        t0.elapsed().as_secs_f64() * 1e3
    );
}

// ---------------------------------------------------------------------------------------------
// SCREENS-GUARD. Creating, resizing or removing one of our displays makes macOS re-plan the whole
// arrangement, and a saved arrangement for that set of displays can carry a different mode for the
// user's own screens (seen: a built-in display flipping 1680x1050 <-> 1440x900 every time one test
// display came and went). A host must never leave the user's screens different from how it found
// them, so every such operation remembers the mode of every screen that is not ours and puts it
// back if it moved.
// ---------------------------------------------------------------------------------------------

/// What a display mode is, comparably.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModeSig {
    width: usize,
    height: usize,
    pixel_width: usize,
    pixel_height: usize,
    millihertz: u64,
}

/// Our displays carry these (`create_cg_virtual_display`), so a screen with both is never "theirs".
const OUR_VENDOR: u32 = 0x3456;
const OUR_PRODUCT: u32 = 0x1234;

/// A screen that is not ours and the mode it was in.
struct ForeignScreen {
    id: u32,
    sig: ModeSig,
    mode: objc2_core_foundation::CFRetained<CGDisplayMode>,
}

/// Display modes are immutable CF objects, safe to read and to hand to WindowServer from any
/// thread; the snapshot only ever moves to the watcher.
unsafe impl Send for ForeignScreen {}

/// How long after an operation a change is still put down to it. Past this, a changed mode is the
/// user's own choice and is left alone.
const SCREENS_WATCH: Duration = Duration::from_secs(4);
/// At most this many corrections per operation: a guard that fights is worse than no guard.
const SCREENS_MAX_RESTORES: u32 = 2;

fn mode_sig(mode: &CGDisplayMode) -> ModeSig {
    unsafe {
        ModeSig {
            width: CGDisplayMode::width(Some(mode)),
            height: CGDisplayMode::height(Some(mode)),
            pixel_width: CGDisplayMode::pixel_width(Some(mode)),
            pixel_height: CGDisplayMode::pixel_height(Some(mode)),
            millihertz: (CGDisplayMode::refresh_rate(Some(mode)) * 1000.0).round() as u64,
        }
    }
}

/// `SE_SCREENS_GUARD=0` turns the guard off (a measurement switch).
fn screens_guard_enabled() -> bool {
    std::env::var("SE_SCREENS_GUARD").as_deref() != Ok("0")
}

/// Every online display that is not one of ours, with its current mode. Empty when the guard is
/// off or WindowServer does not answer, in which case nothing is ever restored.
fn foreign_screens() -> Vec<ForeignScreen> {
    if !screens_guard_enabled() {
        return Vec::new();
    }
    let mut ids = [0u32; 32];
    let mut n = 0u32;
    let err = unsafe {
        objc2_core_graphics::CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n)
    };
    if err != CGError::Success {
        return Vec::new();
    }
    let ours: std::collections::HashSet<u32> = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .displays
        .keys()
        .copied()
        .collect();
    ids[..(n as usize).min(ids.len())]
        .iter()
        .copied()
        .filter(|id| {
            !ours.contains(id)
                && unsafe {
                    objc2_core_graphics::CGDisplayVendorNumber(*id) != OUR_VENDOR
                        || objc2_core_graphics::CGDisplayModelNumber(*id) != OUR_PRODUCT
                }
        })
        .filter_map(|id| {
            let mode = unsafe { objc2_core_graphics::CGDisplayCopyDisplayMode(id) }?;
            Some(ForeignScreen {
                id,
                sig: mode_sig(&mode),
                mode,
            })
        })
        .collect()
}

/// The screens of `before` whose mode is now different: `now` is `(id, mode)` for the displays
/// still online. A screen that has gone (unplugged) is not a change.
fn changed_screens(before: &[(u32, ModeSig)], now: &[(u32, ModeSig)]) -> Vec<u32> {
    before
        .iter()
        .filter(|(id, sig)| now.iter().any(|(i, s)| i == id && s != sig))
        .map(|(id, _)| *id)
        .collect()
}

/// Put one screen back into the mode it had, for this session only.
fn restore_screen(screen: &ForeignScreen) -> CGError {
    use objc2_core_graphics::{
        CGBeginDisplayConfiguration, CGCompleteDisplayConfiguration,
        CGConfigureDisplayWithDisplayMode, CGConfigureOption, CGDisplayConfigRef,
    };
    unsafe {
        let mut cfg: CGDisplayConfigRef = std::ptr::null_mut();
        let begun = CGBeginDisplayConfiguration(&mut cfg);
        if begun != CGError::Success {
            return begun;
        }
        let set = CGConfigureDisplayWithDisplayMode(cfg, screen.id, Some(&screen.mode), None);
        if set != CGError::Success {
            return set;
        }
        // ForSession: never Permanently. This undoes a side effect; it must not become a setting.
        CGCompleteDisplayConfiguration(cfg, CGConfigureOption::ForSession)
    }
}

/// Watch the screens of `before` for [`SCREENS_WATCH`] after `operation`, and put back any whose
/// mode moved. Runs on its own thread: nothing on the join or teardown path waits for it.
fn watch_foreign_screens(operation: &'static str, before: Vec<ForeignScreen>) {
    if before.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("screens-guard".into())
        .spawn(move || {
            let sigs: Vec<(u32, ModeSig)> = before.iter().map(|s| (s.id, s.sig)).collect();
            let deadline = Instant::now() + SCREENS_WATCH;
            let mut restores = 0u32;
            while Instant::now() < deadline && restores < SCREENS_MAX_RESTORES {
                std::thread::sleep(Duration::from_millis(100));
                let now: Vec<(u32, ModeSig)> = sigs
                    .iter()
                    .filter_map(|(id, _)| {
                        let mode = unsafe { objc2_core_graphics::CGDisplayCopyDisplayMode(*id) }?;
                        Some((*id, mode_sig(&mode)))
                    })
                    .collect();
                for id in changed_screens(&sigs, &now) {
                    let Some(screen) = before.iter().find(|s| s.id == id) else {
                        continue;
                    };
                    let (from, to) = (now.iter().find(|n| n.0 == id).map(|n| n.1), screen.sig);
                    let err = restore_screen(screen);
                    restores += 1;
                    teprintln!(
                        "[display] screen {id} changed mode after our display was {operation} \
                         ({from:?} instead of {to:?}); put back for this session (CGError {})",
                        err.0
                    );
                }
            }
        });
    if let Err(e) = spawned {
        teprintln!("[display] could not start the screens guard: {e}");
    }
}

pub struct MacosVirtualDisplay;

impl std::fmt::Debug for MacosVirtualDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = registry().lock().map(|r| r.displays.len()).unwrap_or(0);
        f.debug_struct("MacosVirtualDisplay")
            .field("displays", &count)
            .finish()
    }
}

impl MacosVirtualDisplay {
    pub fn new_shared() -> Option<SharedVirtualDisplay> {
        unsafe {
            let main = CGMainDisplayID();
            let _ = CGDisplayPixelsWide(main);
            let _ = CGDisplayPixelsHigh(main);
        }

        registry().lock().unwrap().displays.clear();
        Some(Arc::new(Self))
    }
}

/// The DPR the join asked for, recovered from the ladder.
///
/// `extra_modes` is `dpr_mode_ladder(css_w, css_h)` — the client's CSS size multiplied by every
/// ratio in `platform::display_dpr_ladder()`, sorted — so its smallest entry is the 1x size and
/// `width / ladder[0].0` is the requested ratio. macOS's ladder is {1.0, 2.0} because HiDPI there
/// is a binary flag, so this only has to tell 1x from 2x.
fn hidpi_from_ladder(width: u32, extra_modes: &[(u32, u32)]) -> u32 {
    match extra_modes.iter().map(|(w, _)| *w).min() {
        Some(base) if base > 0 && width * 2 >= base * 3 => 1,
        _ => 0,
    }
}

fn display_is_active(id: u32) -> bool {
    let mut ids = [0u32; 16];
    let mut count: u32 = 0;
    let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        return false;
    }
    ids[..count as usize].contains(&id)
}

/// Ids this process has had a display under (a reused id is what can read stale).
fn used_ids() -> &'static Mutex<std::collections::HashSet<u32>> {
    static USED: OnceLock<Mutex<std::collections::HashSet<u32>>> = OnceLock::new();
    USED.get_or_init(Default::default)
}

/// One create: make the display, wait for it to be online with readings of its own, settle its
/// geometry. `Err` (with the display destroyed) when it never came online.
#[allow(clippy::too_many_arguments)]
fn attempt_create(
    name: &str,
    identity: &str,
    width: u32,
    height: u32,
    refresh: f64,
    hidpi: u32,
    extra_modes: &[(u32, u32)],
    attempt: u32,
) -> Result<u32, String> {
    let mut steps = Steps::new();
    let deadline = steps.started + ATTEMPT_BUDGET;
    // Before the display exists, so its Add event cannot be missed.
    reconfiguration_events_available();
    let before = id_events()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let (n, i, extra) = (name.to_string(), identity.to_string(), extra_modes.to_vec());
    let (display, applied, token) = bounded("display create", ONLINE_TIMEOUT, move || unsafe {
        create_with_identity(&n, &i, width, height, refresh, hidpi, &extra)
    })
    .unwrap_or_else(|| Err("CGVirtualDisplay creation did not return in time".into()))?;
    let id = unsafe { display.displayID() };
    if id == 0 {
        return Err("CGVirtualDisplay reported displayID 0 after applySettings".to_string());
    }
    let (logical_w, logical_h) = applied.logical();
    tprintln!(
        "[display] virtual display {id} created at {width}x{height} backing \
         ({logical_w}x{logical_h} points, hiDPI={hidpi}) with {} registered modes",
        applied.modes.len()
    );
    registry().lock().unwrap().displays.insert(
        id,
        DisplayEntry {
            display,
            applied,
            ladder: extra_modes.to_vec(),
            token,
        },
    );
    let used = !used_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id);
    steps.mark("create");
    let was = before.get(&id).copied().unwrap_or_default();
    let want = Freshness {
        before: was,
        id_used_before: used,
        serial: display_serial(identity),
    };
    let online = wait_online(id, deadline.min(Instant::now() + ONLINE_TIMEOUT), want);
    steps.mark("online");
    let outcome = match &online {
        Err(e) => {
            teprintln!(
                "[display] attempt {attempt}, display {id}: {}",
                describe_state(id)
            );
            if CALLBACKS.load(Ordering::Relaxed) == 0 {
                teprintln!(
                    "[display] no display notification has ever reached this process, so its view \
                     of the displays may be stuck (its main run loop is not running) even though \
                     the display is online"
                );
            }
            destroy_display(id);
            steps.mark("destroy");
            format!("FAILED: {e}")
        }
        Ok(false) => {
            "online, but its readings were never shown to be its own: nothing corrected".to_string()
        }
        Ok(true) => match ensure_geometry(id, logical_w, logical_h, deadline, &mut steps) {
            Ok(()) => "ok".to_string(),
            Err(e) => {
                teprintln!("[display] display {id} is not at {logical_w}x{logical_h}: {e}");
                "wrong size".to_string()
            }
        },
    };
    tprintln!(
        "[display] attach timeline id={id} attempt={attempt} reused_id={used} serial_ok={} \
         add_seen={} callbacks={}: {} = {:.0} ms ({outcome})",
        unsafe { objc2_core_graphics::CGDisplaySerialNumber(id) } == want.serial,
        events_of(id).adds > was.adds,
        CALLBACKS.load(Ordering::Relaxed),
        steps.parts.join(", "),
        steps.started.elapsed().as_secs_f64() * 1e3
    );
    online.map(|_| id)
}

/// Drop a display and wait, bounded, until WindowServer has let go of it: out of the active list
/// and its Remove event seen. `false` for an id that is not ours.
fn destroy_display(id: u32) -> bool {
    let screens = foreign_screens();
    let gone = destroy_display_now(id);
    if gone {
        watch_foreign_screens("removed", screens);
    }
    gone
}

fn destroy_display_now(id: u32) -> bool {
    let was = events_of(id);
    // Taken out first and dropped with the registry unlocked: the teardown is a WindowServer round
    // trip and may run the termination handler, which takes the same lock.
    let Some(entry) = registry().lock().unwrap().displays.remove(&id) else {
        return false;
    };
    let t0 = Instant::now();
    LAST_REMOVE_NS.store(crate::streamer::audio::host_now_ns(), Ordering::Relaxed);
    let _ = bounded("display teardown", REMOVAL_CONFIRM_TIMEOUT, move || {
        drop(entry)
    });
    let events = reconfiguration_events_available();
    let deadline = t0 + REMOVAL_CONFIRM_TIMEOUT;
    let mut inactive_since = None;
    loop {
        let removed = events_of(id).removes > was.removes;
        if display_is_active(id) {
            inactive_since = None;
        } else {
            let since = *inactive_since.get_or_insert_with(Instant::now);
            // The list alone is good enough after a grace: the event can fail to arrive.
            // ...and never waited for in a process no such event has ever reached.
            let ever = CALLBACKS.load(Ordering::Relaxed) > 0;
            if removed || !events || !ever || since.elapsed() >= REMOVE_EVENT_GRACE {
                tprintln!(
                    "[display] display {id} removed in {} ms (remove event {})",
                    t0.elapsed().as_millis(),
                    if removed { "seen" } else { "NOT seen" }
                );
                return true;
            }
        }
        if Instant::now() >= deadline {
            teprintln!(
                "[display] virtual display {id} not confirmed removed {REMOVAL_CONFIRM_TIMEOUT:?} \
                 after its last reference was dropped (active={}, remove event seen={removed})",
                display_is_active(id)
            );
            return true;
        }
        std::thread::sleep(FAST_POLL);
    }
}

/// Between a failed create (its display gone) and the second one.
const RETRY_PAUSE: Duration = Duration::from_millis(1000);

/// How long an inactive display's Remove event is waited for.
const REMOVE_EVENT_GRACE: Duration = Duration::from_millis(200);

/// Take everything out of a locked map and hand it back with the lock released.
fn take_all<R, T>(
    lock: &Mutex<R>,
    map: impl FnOnce(&mut R) -> &mut HashMap<u32, T>,
) -> HashMap<u32, T> {
    let mut guard = lock.lock().unwrap();
    std::mem::take(map(&mut guard))
}

impl VirtualDisplayController for MacosVirtualDisplay {
    fn create_display(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Result<u32, String> {
        self.create_display_with_modes(name, width, height, refresh_rate, &[])
    }

    fn create_display_with_modes(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
        extra_modes: &[(u32, u32)],
    ) -> Result<u32, String> {
        let refresh = if refresh_rate == 0 {
            60.0
        } else {
            refresh_rate as f64
        };
        let hidpi = hidpi_from_ladder(width, extra_modes);
        // SCREENS-GUARD: before our display exists.
        let screens = foreign_screens();

        // MAC-DISPLAY-RETRY: a display WindowServer never brings online is destroyed and created
        // once more under a salted identity, which bypasses anything remembered for the first.
        let identity = display_identity(&name, width, height);
        let mut why = String::new();
        for attempt in 1..=2u32 {
            let identity = if attempt == 1 {
                identity.clone()
            } else {
                format!("{identity} #retry")
            };
            let r = attempt_create(
                &name,
                &identity,
                width,
                height,
                refresh,
                hidpi,
                extra_modes,
                attempt,
            );
            #[cfg(test)]
            if OFFLINE_ATTEMPTS.load(Ordering::Relaxed) > 0 {
                OFFLINE_ATTEMPTS.fetch_sub(1, Ordering::Relaxed);
            }
            match r {
                Ok(id) => {
                    watch_foreign_screens("created", screens);
                    return Ok(id);
                }
                Err(e) => {
                    teprintln!("[display] create attempt {attempt} of 2 failed: {e}");
                    if attempt == 1 {
                        // The removal is confirmed; give WindowServer a moment of its own too.
                        std::thread::sleep(RETRY_PAUSE);
                    }
                    why = e;
                }
            }
        }
        watch_foreign_screens("created and removed again", screens);
        Err(why)
    }

    fn created_geometry(&self, id: u32) -> Option<(u32, u32)> {
        let applied = registry()
            .lock()
            .unwrap()
            .displays
            .get(&id)?
            .applied
            .clone();
        let (w, h) = applied.logical();
        at_size(id, w, h).then_some((applied.width, applied.height))
    }

    fn display_device_name(&self, id: u32) -> Option<String> {
        crate::macos_utils::streamer::display::display_by_name(&id.to_string())
            .map(|id| id.to_string())
    }

    fn remove_display(&self, id: u32) {
        if !destroy_display(id) {
            teprintln!("virtual_display: remove_display({id}) — unknown display id");
        }
    }

    fn remove_all_displays(&self) {
        // Dropped with the registry unlocked: the teardown may run the termination handler, which
        // takes the same lock.
        let screens = foreign_screens();
        let all = take_all(registry(), |r| &mut r.displays);
        let _ = bounded("display teardown", REMOVAL_CONFIRM_TIMEOUT, move || {
            drop(all)
        });
        watch_foreign_screens("removed", screens);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streamer::session::VirtualDisplayController;

    fn sig(w: usize, h: usize) -> ModeSig {
        ModeSig {
            width: w,
            height: h,
            pixel_width: 2 * w,
            pixel_height: 2 * h,
            millihertz: 60_000,
        }
    }

    /// SCREENS-GUARD: only a screen still there in another mode counts, and nothing is ever
    /// "restored" that was not seen before our operation.
    #[test]
    fn only_a_screen_that_moved_is_put_back() {
        let before = [(1, sig(1680, 1050)), (2, sig(1920, 1080))];
        assert!(changed_screens(&before, &before).is_empty());
        // The built-in display flipping is the case this exists for.
        assert_eq!(
            changed_screens(&before, &[(1, sig(1440, 900)), (2, sig(1920, 1080))]),
            vec![1]
        );
        // An unplugged screen and a new one are not changes.
        assert!(changed_screens(&before, &[(1, sig(1680, 1050)), (7, sig(800, 600))]).is_empty());
        // A different refresh rate is a different mode.
        let mut hz = sig(1680, 1050);
        hz.millihertz = 120_000;
        assert_eq!(
            changed_screens(&before, &[(1, hz), (2, sig(1920, 1080))]),
            vec![1]
        );
    }

    #[test]
    fn our_own_displays_are_never_among_the_screens_to_protect() {
        // The descriptor is created with these very constants, so a display of ours carries them.
        assert_ne!((OUR_VENDOR, OUR_PRODUCT), (0, 0));
    }

    #[test]
    fn a_display_name_resolves_only_to_a_live_display() {
        // CoreGraphics reports no displays at all to a process outside a logged-in console
        // session, and ssh is how this crate is usually tested on the 10.15 box. The assertion is
        // about a live display, so with none present there is nothing to assert.
        if crate::macos_utils::streamer::display::active_displays().is_empty() {
            crate::test_support::skip_because(
                "macos display name correlation",
                "CoreGraphics reports no displays outside a console session (ssh?)",
            );
            return;
        }
        let vd = MacosVirtualDisplay;
        let main = unsafe { CGMainDisplayID() };
        assert_eq!(vd.display_device_name(main), Some(main.to_string()));
        assert_eq!(vd.display_device_name(0xDEAD_BEEF), None);
    }

    #[test]
    fn a_grown_display_is_moved_off_its_neighbour_keeping_its_side() {
        let main = (0, 0, 1680, 1050);
        // The measured case: 800x600 remembered on the left, re-applied to 1920x1080 at x = -800.
        assert_eq!(
            non_overlapping_origin((-800, 0, 1920, 1080), &[main]),
            Some((-1920, 0))
        );
        // On the right it grows away from the panel: nothing to do.
        assert_eq!(non_overlapping_origin((1680, 0, 1920, 1080), &[main]), None);
        // Overlapping from the right (origin inside the panel): pushed to its right edge.
        assert_eq!(
            non_overlapping_origin((100, 0, 1920, 1080), &[main]),
            Some((1680, 0))
        );
        // Two displays to its right: left of the leftmost one it overlaps.
        let right = (1680, 0, 1280, 720);
        assert_eq!(
            non_overlapping_origin((-800, 0, 2560, 1440), &[main, right]),
            Some((-2560, 0))
        );
        // Touching edges are not an overlap.
        assert_eq!(
            non_overlapping_origin((-1920, 0, 1920, 1080), &[main]),
            None
        );
    }

    #[test]
    fn a_reused_id_is_only_fresh_on_its_own_evidence() {
        let mut m = HashMap::new();
        note_event(&mut m, 7, true, false); // the predecessor (another size) came up
        let want = Freshness {
            before: m[&7],
            id_used_before: true,
            serial: 0xB,
        };
        // Same id, active and readable, but every reading is still the predecessor's.
        assert!(!want.is_fresh(m[&7], 0xA));
        note_event(&mut m, 7, false, true); // its removal arrives late
        assert!(!want.is_fresh(m[&7], 0xA));
        // The new display's own serial, or its own Add.
        assert!(want.is_fresh(m[&7], 0xB));
        note_event(&mut m, 7, true, false);
        assert!(want.is_fresh(m[&7], 0xA));
        // An id never used before has no predecessor to read.
        let first = Freshness {
            id_used_before: false,
            ..want
        };
        assert!(first.is_fresh(IdEvents::default(), 0));
    }

    #[test]
    fn displays_are_dropped_with_the_registry_unlocked() {
        // The teardown of one may run the termination handler, which takes the registry lock.
        static LOCK: Mutex<Option<HashMap<u32, Probe>>> = Mutex::new(None);
        static FREE: AtomicU32 = AtomicU32::new(0);
        struct Probe;
        impl Drop for Probe {
            fn drop(&mut self) {
                FREE.fetch_add(u32::from(LOCK.try_lock().is_ok()), Ordering::Relaxed);
            }
        }
        let fill = || *LOCK.lock().unwrap() = Some(HashMap::from([(1, Probe), (2, Probe)]));
        fill();
        drop(take_all(&LOCK, |m| m.as_mut().unwrap()));
        assert_eq!(FREE.swap(0, Ordering::Relaxed), 2);
        // The old ordering (`lock().displays.clear()`) held it through both drops.
        fill();
        LOCK.lock().unwrap().as_mut().unwrap().clear();
        assert_eq!(FREE.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_blocking_call_is_not_waited_for_past_its_limit() {
        let t = Instant::now();
        let r = bounded("test", Duration::from_millis(30), || {
            std::thread::sleep(Duration::from_millis(200));
            1
        });
        assert!(r.is_none() && t.elapsed() < Duration::from_millis(150));
        assert_eq!(
            bounded("test", Duration::from_secs(1), || 2),
            None,
            "still stuck"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(bounded("test", Duration::from_secs(1), || 3), Some(3));
    }

    /// One fresh process, one display: does this process's CoreGraphics ever see it? When it does
    /// not while another process (`SE_EXP_PROBE`) does, try what might refresh it, one at a time.
    #[test]
    #[ignore = "creates a virtual display on the live desktop; run in a loop, one process each"]
    fn investigate_stale_process() {
        use objc2_core_graphics::{
            CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGDisplayIsActive,
            CGDisplayIsOnline, CGGetOnlineDisplayList,
        };
        let state = |id: u32| unsafe {
            let (mut ids, mut n, mut m) = ([0u32; 16], 0u32, 0u32);
            CGGetOnlineDisplayList(16, ids.as_mut_ptr(), &mut n);
            CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut m);
            format!(
                "{:?} active={} online={} lists={n}/{m} cb={}",
                size_of(id),
                CGDisplayIsActive(id),
                CGDisplayIsOnline(id),
                CALLBACKS.load(Ordering::Relaxed)
            )
        };
        let ext = || {
            let p = std::env::var("SE_EXP_PROBE").unwrap_or_default();
            let o = std::process::Command::new(p).output();
            o.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        let name = "ScreenExtend - stale";
        let (d, _, _) =
            unsafe { create_cg_virtual_display(name, 1920, 1080, 60.0, 0, &[]) }.expect("create");
        let id = unsafe { d.displayID() };
        let t = Instant::now();
        while !is_online(id) && t.elapsed() < Duration::from_millis(2000) {
            std::thread::sleep(FAST_POLL);
        }
        if is_online(id) {
            println!(
                "SEEN {id} after {} ms {}",
                t.elapsed().as_millis(),
                state(id)
            );
            return;
        }
        println!("STALE {id}: {} | outside: {}", state(id), ext());
        let check = |what: &str| {
            std::thread::sleep(Duration::from_millis(300));
            println!("  after {what}: {} | outside: {}", state(id), ext());
        };
        check("nothing");
        let mode = unsafe { kCFRunLoopDefaultMode };
        CFRunLoop::run_in_mode(mode, 0.3, false);
        check("pumping this thread's run loop");
        unsafe {
            let mut cfg = std::ptr::null_mut();
            CGBeginDisplayConfiguration(&mut cfg);
            CGCancelDisplayConfiguration(cfg);
        }
        check("an empty configuration begun and cancelled");
        reconfiguration_events_available();
        check("registering the reconfiguration callback");
        let r =
            unsafe { create_cg_virtual_display("ScreenExtend - stale 2", 1280, 720, 60.0, 0, &[]) };
        check("creating a second display");
        println!(
            "  second: {:?}",
            r.map(|r| (
                unsafe { r.0.displayID() },
                state(unsafe { r.0.displayID() })
            ))
        );
        println!("  fresh 5 s later: {}", {
            std::thread::sleep(Duration::from_secs(5));
            state(id)
        });
    }

    /// A display that never comes online is created again (live: real displays).
    #[test]
    #[ignore = "creates virtual displays on the live desktop"]
    fn a_display_that_stays_offline_is_retried_once() {
        let vd = MacosVirtualDisplay;
        let make =
            || vd.create_display_with_modes("ScreenExtend - retry".into(), 1280, 720, 60, &[]);
        OFFLINE_ATTEMPTS.store(1, Ordering::Relaxed);
        let t = Instant::now();
        let id = make().expect("the second attempt");
        println!("retry: {} ms", t.elapsed().as_millis());
        assert!(t.elapsed() < Duration::from_secs(8) && size_of(id) == (1280, 720));
        vd.remove_display(id);
        OFFLINE_ATTEMPTS.store(2, Ordering::Relaxed);
        let t = Instant::now();
        assert!(make().is_err());
        println!("give up: {} ms", t.elapsed().as_millis());
        assert!(t.elapsed() < Duration::from_secs(9));
        assert!(registry().lock().unwrap().displays.is_empty());
    }

    #[test]
    fn the_same_device_name_always_gets_the_same_display_serial() {
        let ipad = display_serial("ScreenExtend - Sarvesh's iPad");
        assert_eq!(ipad, display_serial("ScreenExtend - Sarvesh's iPad"));
        assert_ne!(ipad, display_serial("ScreenExtend - Pixel 7"));
        assert_ne!(display_serial(""), 0, "a serial of 0 reads as unset");
        // One identity per device and size: a size change never inherits another size's geometry.
        let a = display_serial(&display_identity("ScreenExtend - iPad", 1920, 1080));
        assert_eq!(
            a,
            display_serial(&display_identity("ScreenExtend - iPad", 1920, 1080))
        );
        assert_ne!(
            a,
            display_serial(&display_identity("ScreenExtend - iPad", 800, 600))
        );
    }

    /// The settle wait is event-driven only where the registration takes. Over ssh it may not, and
    /// the poll fallback is what then has to keep working — so this asserts that the probe answers
    /// and caches, not which answer it gives.
    #[test]
    fn the_reconfiguration_watcher_reports_whether_it_registered() {
        let available = reconfiguration_events_available();
        println!("CGDisplayRegisterReconfigurationCallback registered: {available}");
        assert_eq!(available, reconfiguration_events_available());
    }

    /// The size sequence that exposed MAC-DISPLAY-MODE: one process, one display name (so one
    /// display identity, which macOS remembers geometry for), joins at 1280x720 -> 2560x1440 ->
    /// 1920x1080 -> 800x600 -> 1920x1080, each created with the production DPR ladder and removed
    /// before the next. Logs what the display looked like while it came up and asserts each one
    /// reached its requested size.
    #[test]
    #[ignore = "creates real virtual displays; needs a console session and the encoder lock; run with --ignored --nocapture"]
    fn a_sequence_of_sizes_each_reaches_its_requested_mode() {
        if !cg_virtual_display_available() {
            crate::test_support::skip_because("display size sequence", "no CGVirtualDisplay");
            return;
        }
        let vd = MacosVirtualDisplay;
        let ladder = |w: u32, h: u32| -> Vec<(u32, u32)> {
            let even = |v: f64| (v.round() as u32).clamp(2, 16384) & !1;
            let mut m: Vec<(u32, u32)> = crate::streamer::platform::display_dpr_ladder()
                .into_iter()
                .map(|r| (even(w as f64 * r), even(h as f64 * r)))
                .collect();
            m.sort_unstable();
            m.dedup();
            m
        };
        let mut results = Vec::new();
        for (w, h) in [
            (1280, 720),
            (2560, 1440),
            (1920, 1080),
            (800, 600),
            (1920, 1080),
        ] {
            let t0 = Instant::now();
            let id = vd
                .create_display_with_modes(
                    "ScreenExtend - size sequence".into(),
                    w,
                    h,
                    60,
                    &ladder(w, h),
                )
                .expect("create");
            let created_ms = t0.elapsed().as_secs_f64() * 1e3;
            let (aw, ah) = unsafe {
                (
                    CGDisplayPixelsWide(id) as u32,
                    CGDisplayPixelsHigh(id) as u32,
                )
            };
            let modes: Vec<(u32, u32)> = unsafe {
                CGDisplayCopyAllDisplayModes(id, None)
                    .map(|l| {
                        (0..l.count())
                            .filter_map(|i| {
                                let p = l.value_at_index(i) as *const CGDisplayMode;
                                p.as_ref().map(|m| {
                                    (
                                        CGDisplayMode::width(Some(m)) as u32,
                                        CGDisplayMode::height(Some(m)) as u32,
                                    )
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            println!(
                "requested {w}x{h} -> display {id} at {aw}x{ah} after {created_ms:.0} ms; modes {modes:?}"
            );
            results.push(((w, h), (aw, ah)));
            vd.remove_display(id);
            std::thread::sleep(Duration::from_millis(500));
        }
        for (req, got) in &results {
            assert_eq!(req, got, "requested vs actual: {results:?}");
        }
    }

    /// MAC-DISPLAY-MODE investigation: create/remove a sequence of sizes under three regimes and
    /// log, per display, when the mode list stops being nil and what the geometry is over time.
    /// `SE_EXP=same|gap|unique` (same name 0.5 s apart / same name 4 s apart / a new name each).
    #[test]
    #[ignore = "creates real virtual displays; console session + encoder lock; --ignored --nocapture"]
    fn investigate_mode_list_after_create() {
        if !cg_virtual_display_available() {
            return;
        }
        let exp = std::env::var("SE_EXP").unwrap_or_else(|_| "same".into());
        let _vd = MacosVirtualDisplay;
        for (k, (w, h)) in [
            (1280u32, 720u32),
            (2560, 1440),
            (1920, 1080),
            (800, 600),
            (1920, 1080),
        ]
        .into_iter()
        .enumerate()
        {
            let name = if exp == "unique" {
                format!("ScreenExtend - exp {k}")
            } else {
                "ScreenExtend - exp".to_string()
            };
            let t0 = Instant::now();
            let (display, applied, _token) =
                unsafe { create_cg_virtual_display(&name, w, h, 60.0, 0, &[]).expect("create") };
            let id = unsafe { display.displayID() };
            let mut first_list = None;
            let mut trace = Vec::new();
            while t0.elapsed() < Duration::from_millis(8000) {
                let ms = t0.elapsed().as_millis();
                let list = unsafe { CGDisplayCopyAllDisplayModes(id, None) };
                let px = unsafe { (CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id)) };
                if list.is_some() && first_list.is_none() {
                    first_list = Some(ms);
                }
                if trace.last().map(|(_, p, l)| (*p, *l)) != Some((px, list.is_some())) {
                    trace.push((ms, px, list.is_some()));
                }
                if first_list.is_some() && ms > first_list.unwrap() + 300 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let px_now = unsafe {
                (
                    CGDisplayPixelsWide(id) as u32,
                    CGDisplayPixelsHigh(id) as u32,
                )
            };
            if exp == "reapply" && px_now != (w, h) {
                // Re-apply the same settings (requested mode first) and watch the geometry.
                let modes = mode_list(w, h, 0, &[]);
                let ok = unsafe { display.applySettings(&make_settings(&modes, 60.0, 0)) };
                let t1 = Instant::now();
                let mut after = Vec::new();
                while t1.elapsed() < Duration::from_millis(2000) {
                    let px = unsafe { (CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id)) };
                    if after.last().map(|(_, p)| *p) != Some(px) {
                        after.push((t1.elapsed().as_millis(), px));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                println!("[reapply] applySettings again -> {ok}; geometry (ms, pixels) {after:?}");
            }
            if exp == "reapply2" && px_now != (w, h) {
                // A different mode first, then the requested one: force WindowServer to move.
                let other = mode_list(w + 2, h + 2, 0, &[]);
                let a = unsafe { display.applySettings(&make_settings(&other, 60.0, 0)) };
                std::thread::sleep(Duration::from_millis(300));
                let modes = mode_list(w, h, 0, &[]);
                let b = unsafe { display.applySettings(&make_settings(&modes, 60.0, 0)) };
                let t1 = Instant::now();
                let mut after = Vec::new();
                while t1.elapsed() < Duration::from_millis(2000) {
                    let px = unsafe { (CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id)) };
                    if after.last().map(|(_, p)| *p) != Some(px) {
                        after.push((t1.elapsed().as_millis(), px));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                println!(
                    "[reapply2] applySettings other {a} then requested {b}; geometry {after:?}"
                );
            }
            let _ = applied;
            println!(
                "[{exp}] {w}x{h} id {id}: mode list first non-nil at {first_list:?} ms; trace (ms, pixels, list?) {trace:?}"
            );
            drop(display);
            let deadline = Instant::now() + Duration::from_millis(2000);
            while display_is_active(id) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(if exp == "gap" { 4000 } else { 500 }));
        }
    }

    /// Every active display as `(id, x, y, w, h, in_mirror_set, mirrors)`, in points.
    fn arrangement() -> Vec<(u32, i32, i32, u32, u32, bool, u32)> {
        let mut ids = [0u32; 16];
        let mut n = 0u32;
        unsafe {
            objc2_core_graphics::CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n);
        }
        ids[..n as usize]
            .iter()
            .map(|&id| unsafe {
                let b = objc2_core_graphics::CGDisplayBounds(id);
                (
                    id,
                    b.origin.x as i32,
                    b.origin.y as i32,
                    b.size.width as u32,
                    b.size.height as u32,
                    objc2_core_graphics::CGDisplayIsInMirrorSet(id),
                    objc2_core_graphics::CGDisplayMirrorsDisplay(id),
                )
            })
            .collect()
    }

    fn overlaps(a: &[(u32, i32, i32, u32, u32, bool, u32)]) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for (i, x) in a.iter().enumerate() {
            for y in &a[i + 1..] {
                let ix = x.1 < y.1 + y.3 as i32 && y.1 < x.1 + x.3 as i32;
                let iy = x.2 < y.2 + y.4 as i32 && y.2 < x.2 + x.4 as i32;
                if ix && iy {
                    out.push((x.0, y.0));
                }
            }
        }
        out
    }

    /// `NSScreen.screens` frames as `(screen number, x, y, w, h)`.
    fn ns_screens() -> Vec<(u32, f64, f64, f64, f64)> {
        use objc2::msg_send;
        use objc2::runtime::{AnyClass, AnyObject};
        let Some(cls) = AnyClass::get(c"NSScreen") else {
            return Vec::new();
        };
        unsafe {
            let screens: *mut AnyObject = msg_send![cls, screens];
            let Some(screens) = screens.as_ref() else {
                return Vec::new();
            };
            let count: usize = msg_send![screens, count];
            (0..count)
                .map(|i| {
                    let sc: *mut AnyObject = msg_send![screens, objectAtIndex: i];
                    let f: objc2_core_foundation::CGRect = msg_send![sc, frame];
                    let desc: *mut AnyObject = msg_send![sc, deviceDescription];
                    let key = NSString::from_str("NSScreenNumber");
                    let num: *mut AnyObject = msg_send![desc, objectForKey: &*key];
                    let id: u32 = if num.is_null() {
                        0
                    } else {
                        msg_send![num, unsignedIntValue]
                    };
                    (id, f.origin.x, f.origin.y, f.size.width, f.size.height)
                })
                .collect()
        }
    }

    fn configure_origin(id: u32, x: i32, y: i32, permanently: bool) -> i32 {
        use objc2_core_graphics::{
            CGBeginDisplayConfiguration, CGCompleteDisplayConfiguration, CGConfigureDisplayOrigin,
            CGConfigureOption, CGDisplayConfigRef,
        };
        unsafe {
            let mut cfg: CGDisplayConfigRef = std::ptr::null_mut();
            if CGBeginDisplayConfiguration(&mut cfg) != CGError::Success {
                return -1;
            }
            let e = CGConfigureDisplayOrigin(cfg, id, x, y);
            // Never Permanently, not even in an investigation: it saves the user's screens too.
            let _ = permanently;
            let opt = CGConfigureOption::ForSession;
            let c = CGCompleteDisplayConfiguration(cfg, opt);
            e.0 * 1000 + c.0
        }
    }

    /// MAC-DISPLAY-ORIGIN / MAC-4K investigation. `SE_EXP=left`: teach an identity 800x600 on the
    /// LEFT of the main display, then re-create it at 1920x1080 and 2560x1440 and log the whole
    /// arrangement after the settings re-apply at 0 / 50 / 500 / 2000 ms. `SE_EXP=4k`: what a
    /// fresh identity reaches at 3840x2160 (hiDPI off and on) and at nearby large sizes.
    #[test]
    #[ignore = "creates real virtual displays and moves one; console session + encoder lock"]
    fn investigate_arrangement_and_large_modes() {
        if !cg_virtual_display_available() {
            return;
        }
        let exp = std::env::var("SE_EXP").unwrap_or_else(|_| "left".into());
        let log = |tag: &str| {
            let a = arrangement();
            println!("  {tag}: CG {a:?} overlaps {:?}", overlaps(&a));
            println!("  {tag}: NSScreen {:?}", ns_screens());
        };
        if exp == "live" {
            // A live resize of one display (the `/reconfig` path): 800x600 on the left, then
            // its settings changed to 1920x1080 in place, then the arrangement repair.
            let ext0 = || {
                let o =
                    std::process::Command::new(std::env::var("SE_EXP_PROBE").unwrap_or_default())
                        .output();
                o.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default()
            };
            let name = format!("ScreenExtend - live resize {}", std::process::id());
            let (d, _, _) =
                unsafe { create_cg_virtual_display(&name, 800, 600, 60.0, 0, &[]) }.unwrap();
            let id = unsafe { d.displayID() };
            std::thread::sleep(Duration::from_millis(500));
            let rc = configure_origin(id, -800, 0, false);
            std::thread::sleep(Duration::from_millis(500));
            println!("live {id} 800x600 moved left rc {rc}: {}", ext0());
            for (w, h) in [(1920u32, 1080u32), (2560, 1440), (800, 600)] {
                let modes = mode_list(w, h, 0, &[]);
                let ok = unsafe { d.applySettings(&make_settings(&modes, 60.0, 0)) };
                std::thread::sleep(Duration::from_millis(500));
                println!("  resized to {w}x{h} ({ok}): {}", ext0());
                fix_arrangement(id, Instant::now() + ATTEMPT_BUDGET);
                std::thread::sleep(Duration::from_millis(300));
                println!("  after fix_arrangement: {}", ext0());
            }
            drop(d);
        } else if exp == "left" {
            let name = "ScreenExtend - origin test";
            let (d, _, _) =
                unsafe { create_cg_virtual_display(name, 800, 600, 60.0, 0, &[]) }.unwrap();
            let id = unsafe { d.displayID() };
            std::thread::sleep(Duration::from_millis(500));
            let rc = configure_origin(id, -800, 0, true);
            std::thread::sleep(Duration::from_millis(500));
            println!("taught {id} 800x600 at (-800,0): rc {rc}");
            log("taught");
            drop(d);
            std::thread::sleep(Duration::from_millis(1500));
            for (w, h) in [(1920u32, 1080u32), (2560, 1440)] {
                let (d, applied, _) =
                    unsafe { create_cg_virtual_display(name, w, h, 60.0, 0, &[]) }.unwrap();
                let id = unsafe { d.displayID() };
                std::thread::sleep(Duration::from_millis(100));
                println!("created {id} for {w}x{h}");
                log("before re-apply");
                let ext0 = || {
                    let o = std::process::Command::new(
                        std::env::var("SE_EXP_PROBE").unwrap_or_default(),
                    )
                    .output();
                    o.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_default()
                };
                if let Ok(px) = std::env::var("SE_EXP_PRE") {
                    let x: i32 = if px == "w" {
                        -(w as i32)
                    } else {
                        px.parse().unwrap_or(1680)
                    };
                    let rc = configure_origin(id, x, 0, false);
                    std::thread::sleep(Duration::from_millis(300));
                    println!("  pre-move to x={x} rc {rc}: EXTERNAL {}", ext0());
                }
                let ok = unsafe { d.applySettings(&make_settings(&applied.modes, 60.0, 0)) };
                let t0 = Instant::now();
                let ext = |tag: &str| {
                    let o = std::process::Command::new(
                        std::env::var("SE_EXP_PROBE").unwrap_or_default(),
                    )
                    .output();
                    println!(
                        "  {tag}: EXTERNAL {}",
                        o.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                            .unwrap_or_default()
                    );
                };
                for at in [0u64, 500] {
                    while t0.elapsed() < Duration::from_millis(at) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    log(&format!("re-apply {ok} +{at} ms"));
                    ext(&format!("re-apply +{at} ms"));
                }
                let mode = std::env::var("SE_EXP_FIX").unwrap_or_else(|_| "session".into());
                let rc = match mode.as_str() {
                    "perm" => configure_origin(id, -(w as i32), 0, true),
                    "right" => configure_origin(id, 1680, 0, false),
                    "rightleft" => {
                        let a = configure_origin(id, 1680, 0, false);
                        std::thread::sleep(Duration::from_millis(700));
                        ext("after right step");
                        a * 10 + configure_origin(id, -(w as i32), 0, false)
                    }
                    "far" => configure_origin(id, -(w as i32) - 2000, 0, false),
                    "leftup" => configure_origin(id, -(w as i32), -100, false),
                    "small" => configure_origin(id, -1000, 0, false),
                    "below" => configure_origin(id, 0, 1050, false),
                    _ => configure_origin(id, -(w as i32), 0, false),
                };
                let t1 = Instant::now();
                for at in [50u64, 500, 2000] {
                    while t1.elapsed() < Duration::from_millis(at) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    log(&format!("fix {mode} rc {rc} +{at} ms"));
                    ext(&format!("fix {mode} +{at} ms"));
                }
                drop(d);
                std::thread::sleep(Duration::from_millis(1500));
            }
        } else {
            let sizes: Vec<(u32, u32, u32)> = std::env::var("SE_EXP_SIZES")
                .ok()
                .map(|v| {
                    v.split(',')
                        .filter_map(|t| {
                            let p: Vec<u32> = t.split('x').filter_map(|n| n.parse().ok()).collect();
                            (p.len() == 3).then(|| (p[0], p[1], p[2]))
                        })
                        .collect()
                })
                .unwrap_or_else(|| {
                    vec![
                        (3840, 2160, 0),
                        (3840, 2160, 1),
                        (3200, 1800, 0),
                        (3008, 1692, 0),
                        (2880, 1620, 0),
                    ]
                });
            for (k, (w, h, hidpi)) in sizes.into_iter().enumerate() {
                let name = format!("ScreenExtend - large {k}");
                let r = unsafe { create_cg_virtual_display(&name, w, h, 60.0, hidpi, &[]) };
                let Ok((d, applied, _)) = r else {
                    println!("{w}x{h} hidpi {hidpi}: create failed: {:?}", r.err());
                    continue;
                };
                let id = unsafe { d.displayID() };
                std::thread::sleep(Duration::from_millis(300));
                let px = unsafe { (CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id)) };
                let ok = unsafe { d.applySettings(&make_settings(&applied.modes, 60.0, hidpi)) };
                std::thread::sleep(Duration::from_millis(1000));
                let px2 = unsafe { (CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id)) };
                let mode = unsafe {
                    objc2_core_graphics::CGDisplayCopyDisplayMode(id).map(|m| {
                        (
                            CGDisplayMode::width(Some(&m)),
                            CGDisplayMode::height(Some(&m)),
                            CGDisplayMode::pixel_width(Some(&m)),
                            CGDisplayMode::pixel_height(Some(&m)),
                        )
                    })
                };
                let act = activate_mode(id, w, h);
                let px3 = unsafe { (CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id)) };
                println!("{w}x{h} hidpi {hidpi}: activate_mode -> {act:?}, now {px3:?}");
                println!(
                    "{w}x{h} hidpi {hidpi}: id {id} after create {px:?}; re-apply {ok} -> {px2:?}; current mode (w,h,pw,ph) {mode:?}; registered {:?}",
                    applied.modes
                );
                drop(d);
                std::thread::sleep(Duration::from_millis(800));
            }
        }
    }

    /// A/B of the Retina join sequence on real hardware.
    ///
    /// **Run it from a Terminal window in the console session.** Over SSH the process has no
    /// WindowServer connection, and `initWithDescriptor:` / `applySettings:` block indefinitely
    /// rather than failing — measured: the test hangs and has to be killed, even though the
    /// read-only CoreGraphics calls in the rest of the suite work fine there.
    ///
    /// ```text
    /// cargo test --lib macos_utils::virtual_display::tests::time_a_retina_join -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "creates a real Retina virtual display; needs a console session (it hangs over SSH); run with --ignored --nocapture"]
    fn time_a_retina_join() {
        if !cg_virtual_display_available() {
            println!("CGVirtualDisplay is not available on this Mac");
            return;
        }
        // A 800x600 CSS client at dpr 2, exactly as `dpr_mode_ladder` would produce it.
        let (w, h, refresh) = (1600u32, 1200u32, 60.0);
        let ladder = [(800u32, 600u32), (1600u32, 1200u32)];

        // Before: born at 1x with one registered mode, then a full applySettings + mode switch to
        // reach HiDPI, which is what `apply_display_scale(200)` did on every Retina join.
        let started = Instant::now();
        let old = unsafe { create_cg_virtual_display("SE-old", w, h, refresh, 0, &[]) };
        let Ok((display, applied, token)) = old else {
            println!("could not create a virtual display: {:?}", old.err());
            return;
        };
        let id = unsafe { display.displayID() };
        registry().lock().unwrap().displays.insert(
            id,
            DisplayEntry {
                display,
                applied,
                ladder: Vec::new(),
                token,
            },
        );
        let _ = activate_mode(id, w, h);
        let old_create = started.elapsed();
        let _ = reconfigure_display(&id.to_string(), w, h, refresh as u32, 1);
        let old_total = started.elapsed();
        registry().lock().unwrap().displays.remove(&id);
        std::thread::sleep(Duration::from_millis(500));

        // After: born at the client's DPR with the ladder and both transposes registered, and the
        // scale call that follows the join is a no-op.
        let started = Instant::now();
        let vd = MacosVirtualDisplay;
        let Ok(id) =
            vd.create_display_with_modes("SE-new".to_string(), w, h, refresh as u32, &ladder)
        else {
            println!("could not create the ladder display");
            return;
        };
        let new_create = started.elapsed();
        let _ = set_display_hidpi(&id.to_string(), 1);
        let new_total = started.elapsed();

        // And an orientation flip, which the transposed mode should make a pure mode switch.
        let rotated = Instant::now();
        let _ = reconfigure_display(&id.to_string(), h, w, refresh as u32, 1);
        let rotate = rotated.elapsed();
        vd.remove_display(id);

        println!("before: create {old_create:?}, create + HiDPI reconfigure {old_total:?}");
        println!("after : create {new_create:?}, create + HiDPI reconfigure {new_total:?}");
        println!("after : orientation flip {rotate:?}");
    }

    #[test]
    fn the_ladder_decides_hidpi() {
        // A 390x844 phone at dpr 2: backing 780x1688, ladder [(390,844), (780,1688)].
        let ladder = [(390, 844), (780, 1688)];
        assert_eq!(hidpi_from_ladder(780, &ladder), 1);
        // The same phone at dpr 1: backing 390x844, same ladder.
        assert_eq!(hidpi_from_ladder(390, &ladder), 0);
        // No ladder at all (the plain create_display path) never claims HiDPI.
        assert_eq!(hidpi_from_ladder(1920, &[]), 0);
    }

    #[test]
    fn modes_carry_the_transpose_and_the_ladder() {
        let modes = mode_list(780, 1688, 1, &[(390, 844), (780, 1688)]);
        assert_eq!(modes[0], (390, 844), "the primary mode comes first");
        assert!(modes.contains(&(844, 390)), "the transpose is registered");
        assert!(
            modes.contains(&(195, 422)),
            "the 1x ladder entry is registered"
        );

        let flat = mode_list(1920, 1080, 0, &[(1920, 1080)]);
        assert_eq!(flat[0], (1920, 1080));
        assert!(flat.contains(&(1080, 1920)));
        assert_eq!(flat.len(), 2, "duplicates are collapsed");
    }
}

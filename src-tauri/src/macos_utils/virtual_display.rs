#![allow(non_snake_case)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObject};
use objc2::{extern_class, extern_methods, sel, AnyThread};
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_core_graphics::{
    CGDisplayCopyAllDisplayModes, CGDisplayMode, CGDisplayPixelsHigh, CGDisplayPixelsWide,
    CGDisplaySetDisplayMode, CGError, CGGetActiveDisplayList, CGMainDisplayID,
};
use objc2_foundation::{NSArray, NSString};

use crate::streamer::session::{SharedVirtualDisplay, VirtualDisplayController};

extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut AnyObject;
}
const DISPATCH_QUEUE_PRIORITY_HIGH: isize = 2;
const DEFAULT_PPI: f64 = 96.0;

/// A pre-registered mode is normally visible on the first `CGDisplayCopyAllDisplayModes`, so the
/// find loop should not need a second pass at all. The deadline stays as a backstop.
const MODE_FIND_TIMEOUT: Duration = Duration::from_millis(1500);
const MODE_SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Poll tightly at first — a settle that takes 20 ms should cost 20 ms, not 50.
const FAST_POLL_WINDOW: Duration = Duration::from_millis(250);
const FAST_POLL: Duration = Duration::from_millis(5);
const SLOW_POLL: Duration = Duration::from_millis(25);
const REMOVAL_CONFIRM_TIMEOUT: Duration = Duration::from_millis(500);

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
unsafe fn install_termination_handler(desc: &CGVirtualDisplayDescriptor, serial: u32) {
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
                .find(|(_, entry)| entry.serial == serial)
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

unsafe fn create_cg_virtual_display(
    name: &str,
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
    desc.setSizeInMillimeters(CGSize {
        width: 25.4 * width as f64 / DEFAULT_PPI,
        height: 25.4 * height as f64 / DEFAULT_PPI,
    });
    static NEXT_SERIAL: AtomicU32 = AtomicU32::new(1);
    let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
    desc.setSerialNum(serial);
    desc.setProductID(0x1234);
    desc.setVendorID(0x3456);
    install_termination_handler(&desc, serial);

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
    Ok((display, applied, serial))
}

fn poll_interval(started: Instant) -> Duration {
    if started.elapsed() < FAST_POLL_WINDOW {
        FAST_POLL
    } else {
        SLOW_POLL
    }
}

pub(crate) fn activate_mode(display_id: u32, width: u32, height: u32) -> Result<(), String> {
    if unsafe { CGDisplayPixelsWide(display_id) } as u32 == width
        && unsafe { CGDisplayPixelsHigh(display_id) } as u32 == height
    {
        return Ok(());
    }

    let started = Instant::now();
    let switched = unsafe {
        let find_deadline = started + MODE_FIND_TIMEOUT;
        loop {
            let modes = CGDisplayCopyAllDisplayModes(display_id, None).ok_or_else(|| {
                format!("CGDisplayCopyAllDisplayModes({display_id}) returned nil")
            })?;
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
        let settle_started = Instant::now();
        let deadline = settle_started + MODE_SETTLE_TIMEOUT;
        loop {
            let w = unsafe { CGDisplayPixelsWide(display_id) } as u32;
            let h = unsafe { CGDisplayPixelsHigh(display_id) } as u32;
            if w == width && h == height {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "display {display_id} did not settle to {width}x{height} (still {w}x{h})"
                ));
            }
            std::thread::sleep(poll_interval(settle_started));
        }
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
    serial: u32,
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
        return activate_mode(id, logical_w, logical_h).map_err(|e| {
            format!(
                "{e}. This display was created over AirPlay, so its geometry is whatever macOS negotiated — only the modes macOS published for it can be selected."
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

    activate_mode(id, logical_w, logical_h)
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

        let (display, applied, serial) = unsafe {
            create_cg_virtual_display(&name, width, height, refresh, hidpi, extra_modes)?
        };
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
                serial,
            },
        );

        if let Err(e) = activate_mode(id, logical_w, logical_h) {
            teprintln!(
                "[display] activate_mode({id}, {logical_w}x{logical_h}) failed: {e}; the client \
                 will get whatever geometry the display settled on"
            );
        }

        Ok(id)
    }

    fn display_device_name(&self, id: u32) -> Option<String> {
        crate::macos_utils::streamer::display::display_by_name(&id.to_string())
            .map(|id| id.to_string())
    }

    fn remove_display(&self, id: u32) {
        if registry().lock().unwrap().displays.remove(&id).is_none() {
            teprintln!("virtual_display: remove_display({id}) — unknown display id");
            return;
        }
        // Teardown is only the `Retained` drop above; nothing in the private API acknowledges it.
        // Confirm the display actually left the active list, so a leak is visible here rather than
        // surfacing later as a phantom monitor in Displays preferences.
        let deadline = Instant::now() + REMOVAL_CONFIRM_TIMEOUT;
        while display_is_active(id) {
            if Instant::now() >= deadline {
                teprintln!(
                    "[display] virtual display {id} is still in CGGetActiveDisplayList \
                     {REMOVAL_CONFIRM_TIMEOUT:?} after its last reference was dropped"
                );
                return;
            }
            std::thread::sleep(SLOW_POLL);
        }
    }

    fn remove_all_displays(&self) {
        registry().lock().unwrap().displays.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streamer::session::VirtualDisplayController;

    #[test]
    fn a_display_name_resolves_only_to_a_live_display() {
        let vd = MacosVirtualDisplay;
        let main = unsafe { CGMainDisplayID() };
        assert_eq!(vd.display_device_name(main), Some(main.to_string()));
        assert_eq!(vd.display_device_name(0xDEAD_BEEF), None);
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
    #[ignore]
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
        let Ok((display, applied, serial)) = old else {
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
                serial,
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

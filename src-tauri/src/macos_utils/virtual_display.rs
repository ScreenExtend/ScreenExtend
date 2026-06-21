//! macOS virtual display backend.
//!
//! Implements [`VirtualDisplayController`] on top of the **private** CoreGraphics
//! `CGVirtualDisplay` Objective-C API — the same API used by Sidecar,
//! DisplayLink and BetterDisplay to create software-only displays that macOS
//! treats exactly like a physical monitor.
//!
//! The four private classes (`CGVirtualDisplayDescriptor`, `CGVirtualDisplayMode`,
//! `CGVirtualDisplaySettings`, `CGVirtualDisplay`) are declared here by hand with
//! `objc2`'s `extern_class!`/`extern_methods!` — they do not appear in any public
//! Apple header. Available since macOS 10.14, reliable on macOS 11+.
//!
//! A `CGVirtualDisplay` lives for exactly as long as the Rust `Retained` handle
//! to it is kept alive; dropping the handle removes the display from macOS. We
//! therefore stash each live display in a **module-global** registry keyed by the
//! `CGDirectDisplayID` macOS assigns to it, so both the controller and the
//! `pipeline` device-setting helpers (which only get the display id, not the
//! handle) can reach it. `remove_display` simply drops the handle.
//!
//! ## Activating the mode
//!
//! A freshly created `CGVirtualDisplay` comes up at a default **1×1** mode even
//! though `applySettings:` succeeds — exactly the trap the Chromium reference
//! avoids with its `EnsureDisplayWithResolution`. A 1×1 display cannot be
//! captured (CGDisplayStream returns a NULL stream). So after `applySettings:`
//! we must explicitly switch the display to the real mode via
//! [`CGDisplaySetDisplayMode`]. The same routine powers in-place resolution /
//! orientation changes (the macOS analogue of Windows' `ChangeDisplaySettingsExW`).

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObject};
use objc2::{extern_class, extern_methods, AnyThread};
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_core_graphics::{
    CGDisplayCopyAllDisplayModes, CGDisplayMode, CGDisplayPixelsHigh, CGDisplayPixelsWide,
    CGDisplaySetDisplayMode, CGError, CGMainDisplayID,
};
use objc2_foundation::{NSArray, NSString};

use crate::streamer::session::{SharedVirtualDisplay, VirtualDisplayController};

// ── libdispatch ─────────────────────────────────────────────────────────────
// The descriptor wants a dispatch_queue_t on which it delivers the (optional)
// termination handler. We hand it the shared high-priority global queue, exactly
// like Chromium's reference implementation.
extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut AnyObject;
}
const DISPATCH_QUEUE_PRIORITY_HIGH: isize = 2;

/// Generic desktop pixel density used to derive the descriptor's physical size
/// (`mm = 25.4 * pixels / ppi`). The exact value only affects the reported
/// physical dimensions, not the rendered resolution.
const DEFAULT_PPI: f64 = 96.0;

// ── private CoreGraphics class declarations ─────────────────────────────────

extern_class!(
    /// Physical description of the virtual display (name, mm size, color primaries).
    #[unsafe(super(NSObject))]
    #[derive(Debug)]
    pub struct CGVirtualDisplayDescriptor;
);

extern_class!(
    /// A single `width × height @ refreshRate` resolution entry.
    #[unsafe(super(NSObject))]
    #[derive(Debug)]
    pub struct CGVirtualDisplayMode;
);

extern_class!(
    /// Wraps the list of modes plus the HiDPI flag; passed to `applySettings:`.
    #[unsafe(super(NSObject))]
    #[derive(Debug)]
    pub struct CGVirtualDisplaySettings;
);

extern_class!(
    /// The live virtual display. Dropping the handle removes it from macOS.
    #[unsafe(super(NSObject))]
    #[derive(Debug)]
    pub struct CGVirtualDisplay;
);

// ── method bindings ─────────────────────────────────────────────────────────

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

        /// `dispatch_queue_t` used for termination notifications.
        #[unsafe(method(setQueue:))]
        unsafe fn setQueue(&self, queue: *mut AnyObject);
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

        /// Activate the display; call immediately after `initWithDescriptor:`.
        /// Returns `false` if the display is already gone or the modes are invalid.
        #[unsafe(method(applySettings:))]
        unsafe fn applySettings(&self, settings: &CGVirtualDisplaySettings) -> bool;

        /// The `CGDirectDisplayID` macOS assigned to this virtual display.
        #[unsafe(method(displayID))]
        unsafe fn displayID(&self) -> u32;
    );
}

/// Build a `CGVirtualDisplaySettings` carrying a single `width × height @ refresh`
/// mode. For SDR (`hidpi == 0`) the mode dimensions equal the pixel dimensions;
/// for HiDPI (`hidpi == 1`) the *logical* mode dimensions are half the pixels.
///
/// # Safety
/// Calls the private CGVirtualDisplay Objective-C API directly.
unsafe fn make_settings(
    width: u32,
    height: u32,
    refresh_rate: f64,
    hidpi: u32,
) -> Retained<CGVirtualDisplaySettings> {
    let settings = CGVirtualDisplaySettings::init(CGVirtualDisplaySettings::alloc());
    settings.setHiDPI(hidpi);

    let (mode_w, mode_h) = if hidpi == 1 {
        (width / 2, height / 2)
    } else {
        (width, height)
    };
    let mode = CGVirtualDisplayMode::initWithWidth_height_refreshRate(
        CGVirtualDisplayMode::alloc(),
        mode_w,
        mode_h,
        refresh_rate,
    );
    settings.setModes(&NSArray::from_retained_slice(&[mode]));
    settings
}

/// Build and activate a virtual display at the requested SDR resolution.
///
/// # Safety
/// Calls the private CGVirtualDisplay Objective-C API directly.
unsafe fn create_cg_virtual_display(
    name: &str,
    width: u32,
    height: u32,
    refresh_rate: f64,
) -> Result<Retained<CGVirtualDisplay>, String> {
    // Settings object first (empty); the mode is added *after* initWithDescriptor,
    // matching the proven Chromium ordering. Building the mode before init left
    // the display wedged on macOS 10.15 (every later CGDisplay* call blocked).
    let settings = CGVirtualDisplaySettings::init(CGVirtualDisplaySettings::alloc());
    settings.setHiDPI(0);

    // Descriptor: physical characteristics of the display.
    let desc = CGVirtualDisplayDescriptor::init(CGVirtualDisplayDescriptor::alloc());
    desc.setQueue(dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0));
    desc.setName(&NSString::from_str(name));

    // Standard sRGB / Apple-native color primaries (CIE 1931 xy).
    desc.setWhitePoint(CGPoint { x: 0.3125, y: 0.3291 });
    desc.setBluePrimary(CGPoint { x: 0.1494, y: 0.0557 });
    desc.setGreenPrimary(CGPoint { x: 0.2559, y: 0.6983 });
    desc.setRedPrimary(CGPoint { x: 0.6797, y: 0.3203 });

    desc.setMaxPixelsWide(width);
    desc.setMaxPixelsHigh(height);
    desc.setSizeInMillimeters(CGSize {
        width: 25.4 * width as f64 / DEFAULT_PPI,
        height: 25.4 * height as f64 / DEFAULT_PPI,
    });
    // Unique serial per display. With vendor/product/serial all zero every
    // virtual display is identical, so macOS recycles the *same* CGDirectDisplayID
    // — and if a prior display leaked (e.g. its owner was force-killed) the new
    // one collides with the zombie and queries on the id block. A distinct serial
    // gives each display its own identity and id.
    static NEXT_SERIAL: AtomicU32 = AtomicU32::new(1);
    desc.setSerialNum(NEXT_SERIAL.fetch_add(1, Ordering::Relaxed));
    desc.setProductID(0x1234);
    desc.setVendorID(0x3456);

    let display = CGVirtualDisplay::initWithDescriptor(CGVirtualDisplay::alloc(), &desc)
        .ok_or_else(|| {
            "CGVirtualDisplay initWithDescriptor returned nil — the private API is \
             unavailable or the app is missing the com.apple.CG.virtual-display \
             entitlement"
                .to_string()
        })?;

    // SDR: the mode dimensions equal the pixel dimensions. Built here, after init.
    let mode = CGVirtualDisplayMode::initWithWidth_height_refreshRate(
        CGVirtualDisplayMode::alloc(),
        width,
        height,
        refresh_rate,
    );
    settings.setModes(&NSArray::from_retained_slice(&[mode]));

    if !display.applySettings(&settings) {
        return Err(
            "CGVirtualDisplay applySettings returned NO — the requested mode is \
             invalid or the display was already destroyed"
                .to_string(),
        );
    }

    Ok(display)
}

/// Switch `display_id` to the active CoreGraphics mode whose *logical* size is
/// `width × height` (Chromium's `EnsureDisplayWithResolution`). A virtual display
/// boots at a 1×1 default; without this it cannot be captured. After
/// `applySettings:` the new mode appears in the display's mode list only once the
/// `com.apple.VirtualDisplayListener` queue has processed it, so we retry the
/// list lookup over a short deadline, then poll until the window server reports
/// the new size.
fn activate_mode(display_id: u32, width: u32, height: u32) -> Result<(), String> {
    // Already there? (CGDisplayPixelsWide reports the current mode's points.)
    // SAFETY: plain CoreGraphics getters keyed by display id.
    if unsafe { CGDisplayPixelsWide(display_id) } as u32 == width
        && unsafe { CGDisplayPixelsHigh(display_id) } as u32 == height
    {
        return Ok(());
    }

    // SAFETY: copies the display's mode list, reads each mode's size, and selects
    // the matching one. The array (and its borrowed modes) live for each attempt.
    // `applySettings` registers the new mode asynchronously, so retry the lookup.
    let switched = unsafe {
        let find_deadline = Instant::now() + Duration::from_millis(1500);
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
                    std::thread::sleep(Duration::from_millis(50));
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
        // The switch is applied asynchronously by the window server; wait for the
        // reported geometry to catch up so capture sees the real size.
        let deadline = Instant::now() + Duration::from_millis(1500);
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
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}

/// Live virtual displays, keyed by the `CGDirectDisplayID` macOS assigned them.
struct DisplayRegistry {
    displays: HashMap<u32, Retained<CGVirtualDisplay>>,
}

// SAFETY: the `Retained<CGVirtualDisplay>` handles are never accessed
// concurrently — every read, insert and drop happens while holding the global
// `Mutex`, which serializes all use across the background threads
// (`spawn_blocking` / `thread::spawn`) that drive the controller and the
// device-setting helpers. The objects are created and released under that lock.
unsafe impl Send for DisplayRegistry {}

fn registry() -> &'static Mutex<DisplayRegistry> {
    static REGISTRY: OnceLock<Mutex<DisplayRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(DisplayRegistry {
            displays: HashMap::new(),
        })
    })
}

/// Re-apply settings to an existing virtual display so it offers a new
/// `width × height @ refresh` mode, then make that mode active. This is the
/// macOS analogue of Windows' `set_display_mode` (`ChangeDisplaySettingsExW`):
/// it changes a *live* virtual display's resolution / orientation in place,
/// keeping the same `CGDirectDisplayID` (so the streamer's per-display
/// bookkeeping survives the change).
///
/// `device_name` is the `CGDirectDisplayID` rendered as a decimal string.
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

    {
        let guard = registry().lock().unwrap();
        let display = guard
            .displays
            .get(&id)
            .ok_or_else(|| format!("display {id} is not a known virtual display"))?;
        // SAFETY: re-applying settings on a live virtual display under the lock.
        let settings = unsafe { make_settings(width, height, refresh, hidpi) };
        if !unsafe { display.applySettings(&settings) } {
            return Err(format!(
                "applySettings({width}x{height}@{refresh}) returned NO for display {id}"
            ));
        }
    }

    // Activate outside the lock — the window server callback runs on its own
    // queue and we only need the display id here. For HiDPI the active mode (and
    // the geometry CoreGraphics reports) is in *logical* points, i.e. half the
    // pixel size, so target the logical dims.
    let (logical_w, logical_h) = if hidpi == 1 {
        (width / 2, height / 2)
    } else {
        (width, height)
    };
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
        // Warm up SkyLight (CGS) *before* any virtual display exists. SkyLight
        // initializes lazily inside a `dispatch_once` on the first CGDisplay*
        // call; on macOS 10.15 doing that first init while a virtual-display
        // creation notification is in flight deadlocks on an internal SkyLight
        // lock (the `com.apple.VirtualDisplayListener` queue holds it). Forcing
        // the init now — against the main display, with nothing pending — makes
        // every later geometry query non-blocking.
        // SAFETY: plain CoreGraphics getters against the main display id.
        unsafe {
            let main = CGMainDisplayID();
            let _ = CGDisplayPixelsWide(main);
            let _ = CGDisplayPixelsHigh(main);
        }

        // Start from a clean slate so a relaunch never leaves orphan displays.
        registry().lock().unwrap().displays.clear();
        Some(Arc::new(Self))
    }
}

impl VirtualDisplayController for MacosVirtualDisplay {
    fn create_display(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Result<u32, String> {
        let refresh = if refresh_rate == 0 {
            60.0
        } else {
            refresh_rate as f64
        };

        let display = unsafe { create_cg_virtual_display(&name, width, height, refresh)? };
        let id = unsafe { display.displayID() };
        if id == 0 {
            return Err("CGVirtualDisplay reported displayID 0 after applySettings".to_string());
        }

        registry().lock().unwrap().displays.insert(id, display);

        // Switch off the 1×1 boot mode to the real resolution so the display is
        // capturable. Non-fatal if it lags: the server re-issues set_display_mode.
        if let Err(e) = activate_mode(id, width, height) {
            teprintln!("virtual_display: activate_mode({id}, {width}x{height}) failed: {e}");
        }

        Ok(id)
    }

    fn remove_display(&self, id: u32) {
        // Dropping the retained handle releases the ObjC object, which removes
        // the display from the macOS display list.
        if registry().lock().unwrap().displays.remove(&id).is_none() {
            teprintln!("virtual_display: remove_display({id}) — unknown display id");
        }
    }

    fn remove_all_displays(&self) {
        registry().lock().unwrap().displays.clear();
    }
}

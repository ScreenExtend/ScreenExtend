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

extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut AnyObject;
}
const DISPATCH_QUEUE_PRIORITY_HIGH: isize = 2;
const DEFAULT_PPI: f64 = 96.0;

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

unsafe fn create_cg_virtual_display(
    name: &str,
    width: u32,
    height: u32,
    refresh_rate: f64,
) -> Result<Retained<CGVirtualDisplay>, String> {
    let settings = CGVirtualDisplaySettings::init(CGVirtualDisplaySettings::alloc());
    settings.setHiDPI(0);

    let desc = CGVirtualDisplayDescriptor::init(CGVirtualDisplayDescriptor::alloc());
    desc.setQueue(dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0));
    desc.setName(&NSString::from_str(name));

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

fn activate_mode(display_id: u32, width: u32, height: u32) -> Result<(), String> {
    if unsafe { CGDisplayPixelsWide(display_id) } as u32 == width
        && unsafe { CGDisplayPixelsHigh(display_id) } as u32 == height
    {
        return Ok(());
    }

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

struct DisplayRegistry {
    displays: HashMap<u32, Retained<CGVirtualDisplay>>,
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
        let settings = unsafe { make_settings(width, height, refresh, hidpi) };
        if !unsafe { display.applySettings(&settings) } {
            return Err(format!(
                "applySettings({width}x{height}@{refresh}) returned NO for display {id}"
            ));
        }
    }

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
        unsafe {
            let main = CGMainDisplayID();
            let _ = CGDisplayPixelsWide(main);
            let _ = CGDisplayPixelsHigh(main);
        }

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

        if let Err(e) = activate_mode(id, width, height) {
            teprintln!("virtual_display: activate_mode({id}, {width}x{height}) failed: {e}");
        }

        Ok(id)
    }

    fn remove_display(&self, id: u32) {
        if registry().lock().unwrap().displays.remove(&id).is_none() {
            teprintln!("virtual_display: remove_display({id}) — unknown display id");
        }
    }

    fn remove_all_displays(&self) {
        registry().lock().unwrap().displays.clear();
    }
}

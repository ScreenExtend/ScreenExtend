//! The live stage's workload: a borderless window covering the virtual display, whose single
//! layer shows one of a small ring of BGRA IOSurfaces, swapped once per workload frame.
//!
//! AppKit and Core Animation are reached through the runtime only (`AnyClass::get` +
//! `msg_send!`): no `objc2-app-kit` / `objc2-quartz-core` dependency and no link-time reference
//! to a class or symbol newer than 10.15 (everything used here is 10.5-era API). The window is
//! created and closed on the main thread; the per-frame swap runs on a presenter thread inside an
//! explicit `CATransaction` that it commits and flushes itself, so nothing depends on the main
//! run loop spinning (it does not: the live child runs inside Tauri's `setup`).
//!
//! The window is placed by CoreGraphics' global display bounds converted to Cocoa coordinates,
//! not through `NSScreen`, whose list AppKit only refreshes from its event loop. Where it actually
//! landed is read back from the window server (`CGWindowListCopyWindowInfo`, no permission
//! needed) and reported.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString};
use objc2_core_foundation::{CGPoint, CGRect};

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    static kIOSurfaceWidth: &'static CFString;
    static kIOSurfaceHeight: &'static CFString;
    static kIOSurfaceBytesPerElement: &'static CFString;
    static kIOSurfaceBytesPerRow: &'static CFString;
    static kIOSurfacePixelFormat: &'static CFString;
    fn IOSurfaceCreate(props: *const c_void) -> *mut c_void;
    fn IOSurfaceLock(s: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(s: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(s: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(s: *mut c_void) -> usize;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGMainDisplayID() -> u32;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> *const CFArray;
    fn CGRectMakeWithDictionaryRepresentation(dict: *const c_void, rect: *mut CGRect) -> bool;
    static kCGWindowBounds: *const c_void;
}

unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFArrayGetCount(a: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(a: *const c_void, i: isize) -> *const c_void;
}

unsafe extern "C-unwind" {
    // Same signature as the other declarations in the crate, or rustc warns about the clash.
    fn CFDictionaryGetValue(the_dict: *const CFDictionary, key: *const c_void) -> *const c_void;
}

const PIXEL_BGRA: u32 = 0x4247_5241; // 'BGRA'
/// `NSWindowStyleMaskBorderless`.
const STYLE_BORDERLESS: usize = 0;
/// `NSBackingStoreBuffered`.
const BACKING_BUFFERED: usize = 2;
/// `NSScreenSaverWindowLevel`: above the menu bar and the Dock, so the whole display is ours.
const LEVEL_SCREEN_SAVER: isize = 1000;
/// `canJoinAllSpaces | stationary | ignoresCycle`.
const COLLECTION: usize = (1 << 0) | (1 << 4) | (1 << 6);
/// `kCGWindowListOptionIncludingWindow`.
const WINDOW_LIST_INCLUDING: u32 = 1 << 3;

pub fn is_main_thread() -> bool {
    unsafe { libc::pthread_main_np() == 1 }
}

/// One BGRA IOSurface the presenter puts on screen.
pub struct Surface {
    raw: NonNull<c_void>,
    pub width: usize,
    pub height: usize,
}

// An IOSurface is a kernel object with its own locking; the handle may move between threads.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    pub fn new(width: usize, height: usize) -> Result<Surface, String> {
        let w = CFNumber::new_isize(width as isize);
        let h = CFNumber::new_isize(height as isize);
        let bpe = CFNumber::new_isize(4);
        let bpr = CFNumber::new_isize((width * 4) as isize);
        let fmt = CFNumber::new_i32(PIXEL_BGRA as i32);
        let props: CFRetained<CFDictionary> = unsafe {
            super::cf::dict(&[
                (kIOSurfaceWidth, &w),
                (kIOSurfaceHeight, &h),
                (kIOSurfaceBytesPerElement, &bpe),
                (kIOSurfaceBytesPerRow, &bpr),
                (kIOSurfacePixelFormat, &fmt),
            ])
        };
        let raw = unsafe { IOSurfaceCreate(CFRetained::as_ptr(&props).as_ptr().cast()) };
        let raw = NonNull::new(raw).ok_or("IOSurfaceCreate returned NULL")?;
        Ok(Surface { raw, width, height })
    }

    /// Copy a tightly packed BGRA frame (`width * height * 4`) in.
    pub fn write(&self, bgra: &[u8]) {
        unsafe {
            if IOSurfaceLock(self.raw.as_ptr(), 0, ptr::null_mut()) != 0 {
                return;
            }
            let base = IOSurfaceGetBaseAddress(self.raw.as_ptr()) as *mut u8;
            let bpr = IOSurfaceGetBytesPerRow(self.raw.as_ptr());
            let row = self.width * 4;
            if !base.is_null() && bgra.len() >= row * self.height {
                if bpr == row {
                    ptr::copy_nonoverlapping(bgra.as_ptr(), base, row * self.height);
                } else {
                    for y in 0..self.height {
                        ptr::copy_nonoverlapping(
                            bgra.as_ptr().add(y * row),
                            base.add(y * bpr),
                            row,
                        );
                    }
                }
            }
            IOSurfaceUnlock(self.raw.as_ptr(), 0, ptr::null_mut());
        }
    }

    fn as_object(&self) -> *mut AnyObject {
        // IOSurfaceRef is toll-free bridged to the `IOSurface` class, which is what
        // `CALayer.contents` accepts.
        self.raw.as_ptr().cast()
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe { CFRelease(self.raw.as_ptr()) };
    }
}

/// The layer the presenter swaps contents on. Only ever messaged inside an explicit
/// `CATransaction`, which Core Animation allows from any thread.
#[derive(Clone, Copy)]
pub struct LayerRef(NonNull<AnyObject>);
unsafe impl Send for LayerRef {}

/// The workload window. Created and closed on the main thread.
pub struct Window {
    win: NonNull<AnyObject>,
    layer: NonNull<AnyObject>,
    pub window_number: isize,
    /// Where the window was asked to go, in CoreGraphics global points (x, y, w, h).
    pub target_cg: (f64, f64, f64, f64),
}

fn class(name: &std::ffi::CStr) -> Result<&'static AnyClass, String> {
    AnyClass::get(name).ok_or_else(|| format!("class {} not found", name.to_string_lossy()))
}

/// Whether AppKit's screen list has `display` yet, and its frame there (Cocoa coordinates).
fn appkit_screen_frame(display: u32) -> Option<CGRect> {
    let ns_screen = AnyClass::get(c"NSScreen")?;
    unsafe {
        let screens: *mut AnyObject = msg_send![ns_screen, screens];
        let screens = screens.as_ref()?;
        let n: usize = msg_send![screens, count];
        let key = objc2_foundation::NSString::from_str("NSScreenNumber");
        for i in 0..n {
            let scr: *mut AnyObject = msg_send![screens, objectAtIndex: i];
            let Some(scr) = scr.as_ref() else { continue };
            let desc: *mut AnyObject = msg_send![scr, deviceDescription];
            let Some(desc) = desc.as_ref() else { continue };
            let num: *mut AnyObject = msg_send![desc, objectForKey: &*key];
            let Some(num) = num.as_ref() else { continue };
            let id: u32 = msg_send![num, unsignedIntValue];
            if id == display {
                let f: CGRect = msg_send![scr, frame];
                return Some(f);
            }
        }
    }
    None
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: u8) -> i32;
}

/// Run the main thread's run loop for up to `seconds` (main thread only; elsewhere it runs that
/// thread's loop, which is harmless). CoreGraphics' per-process display list and AppKit's screen
/// list are both refreshed from callbacks delivered here.
pub fn pump_run_loop(seconds: f64) {
    unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, 1) };
}

/// AppKit's own event loop for up to `seconds`: what `[NSApp run]` does, in slices.
pub fn app_events(seconds: f64) {
    unsafe {
        let (Some(app_c), Some(date_c)) =
            (AnyClass::get(c"NSApplication"), AnyClass::get(c"NSDate"))
        else {
            return;
        };
        let app: *mut AnyObject = msg_send![app_c, sharedApplication];
        let until: *mut AnyObject = msg_send![date_c, dateWithTimeIntervalSinceNow: seconds];
        let mode: *const AnyObject = kCFRunLoopDefaultMode.cast();
        let ev: *mut AnyObject = msg_send![
            app, nextEventMatchingMask: usize::MAX, untilDate: until, inMode: mode,
            dequeue: objc2::runtime::Bool::YES
        ];
        if !ev.is_null() {
            let _: () = msg_send![app, sendEvent: ev];
        }
    }
}

/// Let AppKit hear about a display that was just created. Its screen list is refreshed from a
/// reconfiguration callback delivered on the main run loop, which nothing runs while the live
/// child sits inside Tauri's `setup`. A window placed on a screen AppKit does not know about is
/// moved onto one it does — the user's own display.
fn wait_for_appkit_screen(display: u32, timeout: std::time::Duration) -> Option<CGRect> {
    let start = std::time::Instant::now();
    loop {
        if let Some(f) = appkit_screen_frame(display) {
            return Some(f);
        }
        if start.elapsed() > timeout {
            return None;
        }
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, 1) };
    }
}

impl Window {
    /// A borderless window exactly covering `display`. Main thread only. It stays fully
    /// transparent until the window server confirms it covers that display, and is closed again
    /// (an error) if it does not, so nothing is ever drawn over the user's own screen.
    pub fn open(display: u32) -> Result<Window, String> {
        if !is_main_thread() {
            return Err("not on the main thread; AppKit windows cannot be created here".into());
        }
        let ns_window = class(c"NSWindow")?;
        let ns_view = class(c"NSView")?;
        let ca_layer = class(c"CALayer")?;
        let ca_tx = class(c"CATransaction")?;
        let b = unsafe { CGDisplayBounds(display) };
        if b.size.width < 1.0 || b.size.height < 1.0 {
            return Err(format!("display {display} has empty bounds"));
        }
        let main = unsafe { CGDisplayBounds(CGMainDisplayID()) };
        let Some(screen) = wait_for_appkit_screen(display, std::time::Duration::from_secs(3))
        else {
            return Err(format!(
                "AppKit never listed display {display} as a screen; refusing to place a window"
            ));
        };
        // Cocoa's global origin is the bottom-left of the main display, y up.
        let cocoa_cg = CGRect {
            origin: CGPoint {
                x: b.origin.x,
                y: main.size.height - (b.origin.y + b.size.height),
            },
            size: b.size,
        };
        let cocoa = screen;
        if (cocoa.origin.x - cocoa_cg.origin.x).abs() > 1.0
            || (cocoa.origin.y - cocoa_cg.origin.y).abs() > 1.0
        {
            teprintln!(
                "[live] AppKit frame for display {display} ({:?}) differs from CoreGraphics' ({:?}); using AppKit's",
                cocoa, cocoa_cg
            );
        }
        let local = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: b.size,
        };
        unsafe {
            let alloc: *mut AnyObject = msg_send![ns_window, alloc];
            let win: *mut AnyObject = msg_send![
                alloc,
                initWithContentRect: cocoa,
                styleMask: STYLE_BORDERLESS,
                backing: BACKING_BUFFERED,
                defer: Bool::NO,
            ];
            let win = NonNull::new(win).ok_or("NSWindow init returned nil")?;
            let w = win.as_ref();
            let _: () = msg_send![w, setReleasedWhenClosed: Bool::NO];
            let _: () = msg_send![w, setAlphaValue: 0.0f64];
            let _: () = msg_send![w, setLevel: LEVEL_SCREEN_SAVER];
            let _: () = msg_send![w, setOpaque: Bool::YES];
            let _: () = msg_send![w, setHasShadow: Bool::NO];
            let _: () = msg_send![w, setIgnoresMouseEvents: Bool::YES];
            let _: () = msg_send![w, setCollectionBehavior: COLLECTION];

            let valloc: *mut AnyObject = msg_send![ns_view, alloc];
            let view: *mut AnyObject = msg_send![valloc, initWithFrame: local];
            let view = NonNull::new(view).ok_or("NSView init returned nil")?;
            let layer: *mut AnyObject = msg_send![ca_layer, new];
            let layer = NonNull::new(layer).ok_or("CALayer new returned nil")?;
            let gravity = objc2_foundation::NSString::from_str("resize");
            let _: () = msg_send![layer.as_ref(), setContentsGravity: &*gravity];
            let _: () = msg_send![layer.as_ref(), setOpaque: Bool::YES];
            // A layer-hosting view: the view adopts our layer instead of drawing its own.
            let _: () = msg_send![view.as_ref(), setLayer: layer.as_ref()];
            let _: () = msg_send![view.as_ref(), setWantsLayer: Bool::YES];
            let _: () = msg_send![w, setContentView: view.as_ref()];
            // The window retains its content view.
            let _: () = msg_send![view.as_ref(), release];
            let _: () = msg_send![w, setFrame: cocoa, display: Bool::YES];
            let _: () = msg_send![w, orderFrontRegardless];
            let _: () = msg_send![w, displayIfNeeded];
            let _: () = msg_send![ca_tx, flush];
            let number: isize = msg_send![w, windowNumber];
            let me = Window {
                win,
                layer,
                window_number: number,
                target_cg: (b.origin.x, b.origin.y, b.size.width, b.size.height),
            };
            let placed = me.server_bounds();
            let covers = placed.is_some_and(|(x, y, ww, hh)| {
                (x - b.origin.x).abs() < 2.0
                    && (y - b.origin.y).abs() < 2.0
                    && (ww - b.size.width).abs() < 2.0
                    && (hh - b.size.height).abs() < 2.0
            });
            if !covers {
                me.close();
                return Err(format!(
                    "the window server placed the window at {placed:?}, not over display {display} \
                     at ({}, {}, {}, {}); closed it before drawing anything",
                    b.origin.x, b.origin.y, b.size.width, b.size.height
                ));
            }
            let _: () = msg_send![w, setAlphaValue: 1.0f64];
            let _: () = msg_send![ca_tx, flush];
            Ok(me)
        }
    }

    pub fn layer(&self) -> LayerRef {
        LayerRef(self.layer)
    }

    /// The window's bounds as the window server has them (CG global points), if it lists it.
    pub fn server_bounds(&self) -> Option<(f64, f64, f64, f64)> {
        unsafe {
            let list: *const c_void =
                CGWindowListCopyWindowInfo(WINDOW_LIST_INCLUDING, self.window_number as u32).cast();
            if list.is_null() {
                return None;
            }
            let mut out = None;
            if CFArrayGetCount(list) > 0 {
                let d = CFArrayGetValueAtIndex(list, 0);
                let bd = CFDictionaryGetValue(d.cast(), kCGWindowBounds);
                let mut r = CGRect::default();
                if !bd.is_null() && CGRectMakeWithDictionaryRepresentation(bd, &mut r) {
                    out = Some((r.origin.x, r.origin.y, r.size.width, r.size.height));
                }
            }
            CFRelease(list);
            out
        }
    }

    /// Order out and close, without the close animation, and wait (bounded) until the window server
    /// has taken it off the screen: the animation needs this thread's run loop, which nothing runs
    /// afterwards, and a window whose display is then removed is moved to the user's own screen.
    /// Main thread only.
    pub fn close(self) {
        unsafe {
            let w = self.win.as_ref();
            // NSWindowAnimationBehaviorNone
            let _: () = msg_send![w, setAnimationBehavior: 2isize];
            let _: () = msg_send![w, setAlphaValue: 0.0f64];
            let _: () = msg_send![w, orderOut: ptr::null::<AnyObject>()];
            let _: () = msg_send![w, close];
            if let Some(tx) = AnyClass::get(c"CATransaction") {
                let _: () = msg_send![tx, flush];
            }
            let t = std::time::Instant::now();
            while self.server_bounds().is_some() && t.elapsed().as_millis() < 500 {
                pump_run_loop(0.02);
            }
            if self.server_bounds().is_some() {
                eprintln!("[live] the window server still lists the closed window");
            }
            let _: () = msg_send![self.layer.as_ref(), release];
            let _: () = msg_send![w, release];
        }
    }
}

/// Put `surface` on screen: one explicit transaction, committed and flushed by this thread.
pub fn present(layer: LayerRef, surface: &Surface) {
    let Some(tx) = AnyClass::get(c"CATransaction") else {
        return;
    };
    unsafe {
        let _: () = msg_send![tx, begin];
        let _: () = msg_send![tx, setDisableActions: Bool::YES];
        let _: () = msg_send![layer.0.as_ref(), setContents: surface.as_object()];
        let _: () = msg_send![tx, commit];
        let _: () = msg_send![tx, flush];
    }
}

/// Whether the AppKit/Core Animation pieces this module messages exist (always, on 10.5+; a
/// recorded check rather than an assumption).
pub fn available() -> bool {
    ["NSWindow", "NSView", "CALayer", "CATransaction"]
        .iter()
        .all(|n| {
            let c = std::ffi::CString::new(*n).unwrap_or_default();
            AnyClass::get(&c).is_some()
        })
        && AnyClass::get(c"CALayer")
            .map(|c| {
                let r: Bool =
                    unsafe { msg_send![c, instancesRespondToSelector: sel!(setContents:)] };
                r.as_bool()
            })
            .unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// The frame stamp: a frame counter barcode the receiver reads back out of the decoded picture.
// ---------------------------------------------------------------------------------------------

/// Bits of frame index carried (about 4.8 hours at 60 fps).
const STAMP_BITS: u32 = 20;
/// 1 guard (white) + 20 index bits + 4 check bits + 1 guard (black).
pub const STAMP_CELLS: usize = 26;

/// Cell geometry in display pixels: (x0, y0, cell size). Below the `draw_counter` text.
pub fn stamp_geometry(width: usize) -> (usize, usize, usize) {
    let scale = (width / 160).max(3);
    let cell = (width / 80).max(12);
    (scale, scale * 22, cell)
}

fn check4(idx: u32) -> u32 {
    (idx ^ (idx >> 4) ^ (idx >> 8) ^ (idx >> 12) ^ (idx >> 16) ^ 0xA) & 0xF
}

fn stamp_bits(idx: u32) -> [bool; STAMP_CELLS] {
    let idx = idx & ((1 << STAMP_BITS) - 1);
    let mut b = [false; STAMP_CELLS];
    b[0] = true;
    for i in 0..STAMP_BITS as usize {
        b[1 + i] = (idx >> (STAMP_BITS as usize - 1 - i)) & 1 == 1;
    }
    let c = check4(idx);
    for i in 0..4 {
        b[1 + STAMP_BITS as usize + i] = (c >> (3 - i)) & 1 == 1;
    }
    b[STAMP_CELLS - 1] = false;
    b
}

/// Draw the barcode for `idx` into a tightly packed BGRA frame.
pub fn draw_stamp(bgra: &mut [u8], width: usize, height: usize, idx: u32) {
    let (x0, y0, cell) = stamp_geometry(width);
    if y0 + cell > height || x0 + cell * STAMP_CELLS > width {
        return;
    }
    for (i, on) in stamp_bits(idx).iter().enumerate() {
        let v = if *on { 255 } else { 0 };
        for y in y0..y0 + cell {
            let row = y * width * 4;
            for x in x0 + i * cell..x0 + (i + 1) * cell {
                let o = row + x * 4;
                bgra[o] = v;
                bgra[o + 1] = v;
                bgra[o + 2] = v;
                bgra[o + 3] = 255;
            }
        }
    }
}

/// Read the barcode back out of a luma plane of `w x h` (a picture of the `disp_w`-wide display,
/// possibly scaled). `None` when the guards or the check bits disagree.
pub fn read_stamp(
    luma: *const u8,
    bpr: usize,
    w: usize,
    h: usize,
    disp_w: usize,
    disp_h: usize,
) -> Option<u32> {
    if luma.is_null() || disp_w == 0 || disp_h == 0 {
        return None;
    }
    let (x0, y0, cell) = stamp_geometry(disp_w);
    let sx = w as f64 / disp_w as f64;
    let sy = h as f64 / disp_h as f64;
    let mut bits = [false; STAMP_CELLS];
    let mut lumas = [0u8; STAMP_CELLS];
    for (i, bit) in bits.iter_mut().enumerate() {
        let cx = (x0 as f64 + (i as f64 + 0.5) * cell as f64) * sx;
        let cy = (y0 as f64 + 0.5 * cell as f64) * sy;
        let r = ((cell as f64 * sx.min(sy)) / 4.0).max(1.0) as isize;
        let (mut sum, mut n) = (0u64, 0u64);
        for dy in -r..=r {
            for dx in -r..=r {
                let x = cx as isize + dx;
                let y = cy as isize + dy;
                if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
                    continue;
                }
                sum += unsafe { *luma.add(y as usize * bpr + x as usize) } as u64;
                n += 1;
            }
        }
        if n == 0 {
            return None;
        }
        *bit = sum / n > 128;
        lumas[i] = (sum / n) as u8;
    }
    if !bits[0] || bits[STAMP_CELLS - 1] {
        // Once per run: what is where the barcode should be.
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| tprintln!("[live] stamp guards unreadable; cell luma {lumas:?}"));
        return None;
    }
    let mut idx = 0u32;
    for i in 0..STAMP_BITS as usize {
        idx = (idx << 1) | bits[1 + i] as u32;
    }
    let mut c = 0u32;
    for i in 0..4 {
        c = (c << 1) | bits[1 + STAMP_BITS as usize + i] as u32;
    }
    (c == check4(idx)).then_some(idx)
}

/// Full-range BT.709 NV12 -> BGRA (for a `--recording` workload), into a tightly packed buffer of
/// `dw x dh`; the source is cropped or padded (black) to fit.
pub fn nv12_to_bgra(nv12: &[u8], sw: usize, sh: usize, out: &mut [u8], dw: usize, dh: usize) {
    out.iter_mut().for_each(|b| *b = 0);
    let (w, h) = (sw.min(dw), sh.min(dh));
    let (yp, uv) = nv12.split_at(sw * sh);
    for y in 0..h {
        for x in 0..w {
            let yy = yp[y * sw + x] as f32;
            let ci = (y / 2) * sw + (x & !1);
            let u = uv[ci] as f32 - 128.0;
            let v = uv[ci + 1] as f32 - 128.0;
            let r = yy + 1.5748 * v;
            let g = yy - 0.1873 * u - 0.4681 * v;
            let b = yy + 1.8556 * u;
            let o = (y * dw + x) * 4;
            out[o] = b.clamp(0.0, 255.0) as u8;
            out[o + 1] = g.clamp(0.0, 255.0) as u8;
            out[o + 2] = r.clamp(0.0, 255.0) as u8;
            out[o + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_reads_back_at_full_and_half_size() {
        let (w, h) = (1920usize, 1080usize);
        let mut bgra = vec![40u8; w * h * 4];
        for idx in [0u32, 1, 599, 123_456, (1 << 20) - 1] {
            draw_stamp(&mut bgra, w, h, idx);
            // luma ~ green channel for grey/white/black
            let luma: Vec<u8> = bgra.chunks(4).map(|p| p[1]).collect();
            assert_eq!(read_stamp(luma.as_ptr(), w, w, h, w, h), Some(idx));
            let half: Vec<u8> = (0..h / 2)
                .flat_map(|y| (0..w / 2).map(move |x| (x, y)))
                .map(|(x, y)| luma[(y * 2) * w + x * 2])
                .collect();
            assert_eq!(
                read_stamp(half.as_ptr(), w / 2, w / 2, h / 2, w, h),
                Some(idx)
            );
        }
        let blank = vec![0u8; w * h];
        assert_eq!(read_stamp(blank.as_ptr(), w, w, h, w, h), None);
    }
}

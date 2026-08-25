//! Event-tap volume-key fallback (PRD-macos-legacy-audio.md §6.3).
//!
//! Layer 1 (device Volume/Mute controls) makes macOS handle the hardware volume keys natively —
//! including the on-screen HUD — while our virtual device is the default output. That was verified
//! working on 10.15, so Layer 1 is the default and this tap is **opt-in**
//! (`SCREENEXTEND_LEGACY_VOLUME_TAP=1`), kept as a belt-and-braces backstop for any OS version where
//! Layer 1 turns out not to re-enable OS handling: a `CGEventTap` that intercepts the F10/F11/F12
//! volume keys directly, drives our device's volume/mute itself, and **consumes** the event so there
//! is never a "denied" beep and never a double-adjustment with the OS. The cost of consuming — and
//! the reason it is not the default — is that the native volume HUD does not appear.
//!
//! Because it consumes the key, it works regardless of whether Layer 1 re-enabled OS handling on a
//! given version — which is exactly the guarantee we want. Setting our device's volume scalar reuses
//! the whole Layer 2 chain (the volume proxy observes it → applies the monitor gain → mirrors to the
//! real device), so this file only has to move one property.
//!
//! Requires Accessibility permission (an active event tap does). ScreenExtend already requests it
//! for remote input injection; if it isn't granted, `CGEventTapCreate` returns null and we fall back
//! to Layer 1 alone and say so. The one cost of consuming the key is that the on-screen volume HUD
//! may not appear on versions where Layer 1 would have shown it.
//!
//! CoreGraphics' event-tap / run-loop API is not exposed by `objc2-core-graphics`, so — exactly like
//! the remote-input-injection code (`streamer/input/macos.rs`) — it is declared directly against the
//! CoreGraphics + CoreFoundation frameworks.

use std::ffi::c_void;
use std::sync::mpsc;
use std::thread::JoinHandle;

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_core_audio::AudioObjectID;

use super::hal;

type CFTypeRef = *mut c_void;
type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CGEventTapCallBack =
    extern "C-unwind" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFTypeRef;
    fn CGEventTapEnable(tap: CFTypeRef, enable: bool);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(
        alloc: *const c_void,
        port: CFTypeRef,
        order: isize,
    ) -> CFTypeRef;
    fn CFRunLoopGetCurrent() -> CFTypeRef;
    fn CFRunLoopAddSource(rl: CFTypeRef, source: CFTypeRef, mode: CFTypeRef);
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: CFTypeRef);
    fn CFRunLoopSourceInvalidate(source: CFTypeRef);
    fn CFMachPortInvalidate(port: CFTypeRef);
    fn CFRelease(cf: *const c_void);
    static kCFRunLoopCommonModes: CFTypeRef;
}

// CGEventTapLocation / Placement / Options and the NX system-defined event type.
const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const NX_SYSDEFINED: u32 = 14;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

// NSSystemDefined aux-button subtype and the three volume key codes.
const AUX_CONTROL_SUBTYPE: i16 = 8;
const NX_KEYTYPE_SOUND_UP: i64 = 0;
const NX_KEYTYPE_SOUND_DOWN: i64 = 1;
const NX_KEYTYPE_MUTE: i64 = 7;

/// macOS moves output volume in sixteenths per key press.
const VOLUME_STEP: f32 = 1.0 / 16.0;

struct TapCtx {
    our_device: AudioObjectID,
    /// The tap's Mach port, stored so the callback can re-enable it if the system disables it.
    port: CFTypeRef,
}

fn nudge_volume(dev: AudioObjectID, delta: f32) {
    let cur = hal::output_volume_scalar(dev).unwrap_or(1.0);
    let _ = hal::set_output_volume_scalar(dev, (cur + delta).clamp(0.0, 1.0));
}

fn toggle_mute(dev: AudioObjectID) {
    let muted = hal::output_mute(dev).unwrap_or(false);
    let _ = hal::set_output_mute(dev, !muted);
}

extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if user_info.is_null() {
        return event;
    }
    // SAFETY: user_info is our TapCtx, alive for the tap thread's lifetime.
    let ctx = unsafe { &*(user_info as *const TapCtx) };

    // The system disables a slow/interrupted tap; re-enable and pass the event through.
    if etype == TAP_DISABLED_BY_TIMEOUT || etype == TAP_DISABLED_BY_USER_INPUT {
        // SAFETY: re-enabling our own tap port.
        unsafe { CGEventTapEnable(ctx.port, true) };
        return event;
    }
    if etype != NX_SYSDEFINED {
        return event;
    }

    // Decode the aux-button event via NSEvent (the reliable bridge for system-defined data).
    let mut is_volume_key = false;
    let mut act_on_down = false;
    let mut key_code = -1i64;
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: +[NSEvent eventWithCGEvent:] returns an autoreleased NSEvent (or nil).
        unsafe {
            let ns: *mut AnyObject = msg_send![class!(NSEvent), eventWithCGEvent: event];
            if ns.is_null() {
                return;
            }
            let subtype: i16 = msg_send![ns, subtype];
            if subtype != AUX_CONTROL_SUBTYPE {
                return;
            }
            let data1: isize = msg_send![ns, data1];
            key_code = ((data1 & 0xFFFF_0000) >> 16) as i64;
            let key_flags = (data1 & 0xFFFF) as i64;
            // 0x0A == key down, 0x0B == key up.
            act_on_down = ((key_flags & 0xFF00) >> 8) == 0x0A;
            is_volume_key = matches!(
                key_code,
                NX_KEYTYPE_SOUND_UP | NX_KEYTYPE_SOUND_DOWN | NX_KEYTYPE_MUTE
            );
        }
    });

    if !is_volume_key {
        return event; // not ours — let it through untouched.
    }
    // It's a volume key: act on the key-down edge, and consume BOTH down and up so the OS never
    // sees a half event and never plays the "denied" beep.
    if act_on_down {
        match key_code {
            NX_KEYTYPE_SOUND_UP => nudge_volume(ctx.our_device, VOLUME_STEP),
            NX_KEYTYPE_SOUND_DOWN => nudge_volume(ctx.our_device, -VOLUME_STEP),
            NX_KEYTYPE_MUTE => toggle_mute(ctx.our_device),
            _ => {}
        }
    }
    std::ptr::null_mut() // consume
}

/// RAII owner of the volume-key event tap. Dropping it stops the run loop and joins the tap thread.
pub struct VolumeKeyTap {
    run_loop: CFTypeRef,
    join: Option<JoinHandle<()>>,
}

// SAFETY: `run_loop` is only ever used with CFRunLoopStop (documented thread-safe) and is not
// dereferenced in Rust; the tap thread owns and frees the CF objects.
unsafe impl Send for VolumeKeyTap {}

impl VolumeKeyTap {
    /// Start the tap for `our_device`. Returns `None` if the tap can't be created (no Accessibility
    /// permission) — the caller then relies on the Layer 1 device controls alone.
    pub fn start(our_device: AudioObjectID) -> Option<VolumeKeyTap> {
        let (ready_tx, ready_rx) = mpsc::channel::<Option<usize>>();
        let join = std::thread::Builder::new()
            .name("se-audio-volkeys".into())
            .spawn(move || tap_thread(our_device, ready_tx))
            .ok()?;

        match ready_rx.recv() {
            Ok(Some(rl)) => Some(VolumeKeyTap {
                run_loop: rl as CFTypeRef,
                join: Some(join),
            }),
            _ => {
                let _ = join.join();
                None
            }
        }
    }
}

impl Drop for VolumeKeyTap {
    fn drop(&mut self) {
        if !self.run_loop.is_null() {
            // SAFETY: CFRunLoopStop is thread-safe; the tap thread is blocked in CFRunLoopRun.
            unsafe { CFRunLoopStop(self.run_loop) };
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The tap thread: create the tap, run a CFRunLoop until stopped, then tear everything down.
fn tap_thread(our_device: AudioObjectID, ready_tx: mpsc::Sender<Option<usize>>) {
    let ctx = Box::into_raw(Box::new(TapCtx {
        our_device,
        port: std::ptr::null_mut(),
    }));
    let mask: u64 = 1u64 << NX_SYSDEFINED;

    // SAFETY: create an active session-level tap for system-defined events; user_info is our live
    // ctx box. Returns null without Accessibility permission.
    let port = unsafe {
        CGEventTapCreate(
            K_CG_SESSION_EVENT_TAP,
            K_CG_HEAD_INSERT_EVENT_TAP,
            K_CG_EVENT_TAP_OPTION_DEFAULT,
            mask,
            tap_callback,
            ctx as *mut c_void,
        )
    };
    if port.is_null() {
        // SAFETY: nothing took ownership of ctx; reclaim it.
        drop(unsafe { Box::from_raw(ctx) });
        let _ = ready_tx.send(None);
        return;
    }
    // SAFETY: store the port for the callback's re-enable path.
    unsafe { (*ctx).port = port };

    // SAFETY: wire the tap into this thread's run loop and enable it.
    let (source, run_loop) = unsafe {
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
        let run_loop = CFRunLoopGetCurrent();
        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
        (source, run_loop)
    };

    let _ = ready_tx.send(Some(run_loop as usize));

    // SAFETY: blocks until CFRunLoopStop() is called from Drop.
    unsafe { CFRunLoopRun() };

    // Teardown after the run loop stops.
    // SAFETY: disable + invalidate + release the CF objects we created, then free ctx (no callback
    // can fire after the source is invalidated and the run loop has returned).
    unsafe {
        CGEventTapEnable(port, false);
        CFRunLoopSourceInvalidate(source);
        CFMachPortInvalidate(port);
        CFRelease(source);
        CFRelease(port);
        drop(Box::from_raw(ctx));
    }
}

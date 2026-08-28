use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
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

// CGEventTapLocation / Placement / Options and the NX system-defined event type
const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const NX_SYSDEFINED: u32 = 14;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

// NSSystemDefined aux-button subtype and the three volume key codes
const AUX_CONTROL_SUBTYPE: i16 = 8;
const NX_KEYTYPE_SOUND_UP: i64 = 0;
const NX_KEYTYPE_SOUND_DOWN: i64 = 1;
const NX_KEYTYPE_MUTE: i64 = 7;

// macOS moves output volume in sixteenths per key press
const VOLUME_STEP: f32 = 1.0 / 16.0;

struct TapCtx {
    our_device: AudioObjectID,
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
    let ctx = unsafe { &*(user_info as *const TapCtx) };

    if etype == TAP_DISABLED_BY_TIMEOUT || etype == TAP_DISABLED_BY_USER_INPUT {
        unsafe { CGEventTapEnable(ctx.port, true) };
        return event;
    }
    if etype != NX_SYSDEFINED {
        return event;
    }

    let mut is_volume_key = false;
    let mut act_on_down = false;
    let mut key_code = -1i64;
    objc2::rc::autoreleasepool(|_| {
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
            // 0x0A == key down, 0x0B == key up
            act_on_down = ((key_flags & 0xFF00) >> 8) == 0x0A;
            is_volume_key = matches!(
                key_code,
                NX_KEYTYPE_SOUND_UP | NX_KEYTYPE_SOUND_DOWN | NX_KEYTYPE_MUTE
            );
        }
    });

    if !is_volume_key {
        return event;
    }
    if act_on_down {
        match key_code {
            NX_KEYTYPE_SOUND_UP => nudge_volume(ctx.our_device, VOLUME_STEP),
            NX_KEYTYPE_SOUND_DOWN => nudge_volume(ctx.our_device, -VOLUME_STEP),
            NX_KEYTYPE_MUTE => toggle_mute(ctx.our_device),
            _ => {}
        }
    }
    std::ptr::null_mut()
}

pub struct VolumeKeyTap {
    run_loop: CFTypeRef,
    join: Option<JoinHandle<()>>,
}

unsafe impl Send for VolumeKeyTap {}

impl VolumeKeyTap {
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
            unsafe { CFRunLoopStop(self.run_loop) };
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn tap_thread(our_device: AudioObjectID, ready_tx: mpsc::Sender<Option<usize>>) {
    let ctx = Box::into_raw(Box::new(TapCtx {
        our_device,
        port: std::ptr::null_mut(),
    }));
    let mask: u64 = 1u64 << NX_SYSDEFINED;

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
        drop(unsafe { Box::from_raw(ctx) });
        let _ = ready_tx.send(None);
        return;
    }
    unsafe { (*ctx).port = port };

    let (source, run_loop) = unsafe {
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
        let run_loop = CFRunLoopGetCurrent();
        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
        (source, run_loop)
    };

    let _ = ready_tx.send(Some(run_loop as usize));

    unsafe { CFRunLoopRun() };

    unsafe {
        CGEventTapEnable(port, false);
        CFRunLoopSourceInvalidate(source);
        CFMachPortInvalidate(port);
        CFRelease(source);
        CFRelease(port);
        drop(Box::from_raw(ctx));
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<Manager> = Mutex::new(Manager {
    device: 0,
    tap: None,
});

struct Manager {
    device: AudioObjectID,
    tap: Option<VolumeKeyTap>,
}

fn env_forced() -> bool {
    std::env::var_os("SCREENEXTEND_LEGACY_VOLUME_TAP").is_some()
}

fn desired() -> bool {
    ENABLED.load(Ordering::Relaxed) || env_forced()
}

fn reconcile(m: &mut Manager) {
    let want = desired() && m.device != 0;
    if want && m.tap.is_none() {
        m.tap = VolumeKeyTap::start(m.device);
    } else if !want {
        m.tap = None;
    }
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    reconcile(&mut STATE.lock().unwrap());
}

pub fn bind_device(device: AudioObjectID) {
    let mut m = STATE.lock().unwrap();
    m.device = device;
    m.tap = None;
    reconcile(&mut m);
}

pub fn unbind() {
    let mut m = STATE.lock().unwrap();
    m.device = 0;
    m.tap = None;
}

pub fn is_active() -> bool {
    STATE.lock().unwrap().tap.is_some()
}

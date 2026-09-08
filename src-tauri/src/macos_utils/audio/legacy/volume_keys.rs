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

/// What a volume key press asks for. Sent to the worker; never acted on in the tap callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeAction {
    Up,
    Down,
    ToggleMute,
}

struct TapCtx {
    tx: crossbeam_channel::Sender<VolumeAction>,
    port: CFTypeRef,
}

fn nudge_volume(dev: AudioObjectID, delta: f32) {
    let Some(cur) = hal::output_volume_scalar(dev) else {
        crate::teprintln!(
            "audio(legacy): device {dev} has no readable output volume; the volume keys cannot \
             move it"
        );
        return;
    };
    let st = hal::set_output_volume_scalar(dev, (cur + delta).clamp(0.0, 1.0));
    if st != 0 {
        crate::teprintln!("audio(legacy): device {dev} refused a volume change (OSStatus {st})");
    }
}

fn toggle_mute(dev: AudioObjectID) {
    let Some(muted) = hal::output_mute(dev) else {
        crate::teprintln!("audio(legacy): device {dev} has no readable mute control");
        return;
    };
    let st = hal::set_output_mute(dev, !muted);
    if st != 0 {
        crate::teprintln!("audio(legacy): device {dev} refused a mute change (OSStatus {st})");
    }
}

/// Drains the actions the tap callback classified and performs the HAL round trips they need.
///
/// This exists so the callback does not. It is an *active* session-level event tap with
/// `kCGEventTapOptionDefault`, so every keyboard and mouse event for the whole login session waits
/// behind whatever it does — and what it used to do was two synchronous `AudioObject*` round trips
/// to `coreaudiod`. When `coreaudiod` is busy (a hot-plug, a format change, another app
/// enumerating devices) that overruns `kCGEventTapDisabledByTimeout` and macOS disables the tap,
/// which is why the re-enable branch in `tap_callback` exists at all.
fn action_worker(dev: AudioObjectID, rx: crossbeam_channel::Receiver<VolumeAction>) {
    while let Ok(action) = rx.recv() {
        match action {
            VolumeAction::Up => nudge_volume(dev, VOLUME_STEP),
            VolumeAction::Down => nudge_volume(dev, -VOLUME_STEP),
            VolumeAction::ToggleMute => toggle_mute(dev),
        }
    }
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
        let action = match key_code {
            NX_KEYTYPE_SOUND_UP => Some(VolumeAction::Up),
            NX_KEYTYPE_SOUND_DOWN => Some(VolumeAction::Down),
            NX_KEYTYPE_MUTE => Some(VolumeAction::ToggleMute),
            _ => None,
        };
        // try_send, never send: the whole point is that this callback cannot block. A full queue
        // means the worker is behind on a burst of key repeats, and dropping one of those is
        // invisible where stalling every input event in the session is not.
        if let Some(a) = action {
            let _ = ctx.tx.try_send(a);
        }
    }
    std::ptr::null_mut()
}

pub struct VolumeKeyTap {
    run_loop: CFTypeRef,
    join: Option<JoinHandle<()>>,
    /// dropping this ends `action_worker`'s `recv` loop
    action_tx: Option<crossbeam_channel::Sender<VolumeAction>>,
    worker: Option<JoinHandle<()>>,
}

unsafe impl Send for VolumeKeyTap {}

/// Queue depth for classified key presses.
///
/// A key repeat runs at roughly 30 Hz and each action is one pair of HAL round trips, so a handful
/// of slots absorbs a burst; past that the presses are stale anyway and dropping them beats making
/// the session's input path wait.
const ACTION_QUEUE_DEPTH: usize = 16;

impl VolumeKeyTap {
    pub fn start(our_device: AudioObjectID) -> Option<VolumeKeyTap> {
        let (action_tx, action_rx) = crossbeam_channel::bounded::<VolumeAction>(ACTION_QUEUE_DEPTH);
        let worker = std::thread::Builder::new()
            .name("se-audio-volkeys-io".into())
            .spawn(move || action_worker(our_device, action_rx))
            .ok()?;

        let (ready_tx, ready_rx) = mpsc::channel::<Option<usize>>();
        let tap_tx = action_tx.clone();
        let join = match std::thread::Builder::new()
            .name("se-audio-volkeys".into())
            .spawn(move || tap_thread(tap_tx, ready_tx))
        {
            Ok(j) => j,
            Err(e) => {
                crate::teprintln!("audio(legacy): volume-key tap thread spawn failed: {e}");
                drop(action_tx);
                let _ = worker.join();
                return None;
            }
        };

        match ready_rx.recv() {
            Ok(Some(rl)) => Some(VolumeKeyTap {
                run_loop: rl as CFTypeRef,
                join: Some(join),
                action_tx: Some(action_tx),
                worker: Some(worker),
            }),
            _ => {
                let _ = join.join();
                drop(action_tx);
                let _ = worker.join();
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
        // the tap thread holds the other sender, so this only closes the channel once it is gone
        self.action_tx = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn tap_thread(tx: crossbeam_channel::Sender<VolumeAction>, ready_tx: mpsc::Sender<Option<usize>>) {
    let ctx = Box::into_raw(Box::new(TapCtx {
        tx,
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

/// A poisoned `STATE` must not take the volume keys down with it.
///
/// The guard is held across `VolumeKeyTap`'s `Drop`, which stops a run loop and joins two threads;
/// a panic anywhere under it would otherwise turn every later `set_enabled`/`is_active` call into
/// a panic of its own.
fn state() -> std::sync::MutexGuard<'static, Manager> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bring the tap into line with `desired()`, starting or stopping it as needed.
///
/// `VolumeKeyTap::start` blocks on a handshake with the tap thread, so it runs with no lock held;
/// the guard is taken only to read the intent and again to install the result. Two concurrent
/// callers can therefore both build a tap, and the loser's is dropped — cheaper than serialising
/// every caller behind a thread handshake.
fn reconcile() {
    let (want, device) = {
        let m = state();
        (desired() && m.device != 0, m.device)
    };
    if !want {
        state().tap = None;
        return;
    }
    if state().tap.is_some() {
        return;
    }
    let tap = VolumeKeyTap::start(device);
    let mut m = state();
    if m.device == device && desired() && m.tap.is_none() {
        m.tap = tap;
    }
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    reconcile();
}

pub fn bind_device(device: AudioObjectID) {
    {
        let mut m = state();
        m.device = device;
        m.tap = None;
    }
    reconcile();
}

pub fn unbind() {
    let mut m = state();
    m.device = 0;
    m.tap = None;
}

pub fn is_active() -> bool {
    state().tap.is_some()
}

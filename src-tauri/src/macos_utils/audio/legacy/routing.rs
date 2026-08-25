//! Default-output-device routing state machine (PRD-macos-legacy-audio.md §8.3).
//!
//! Enabling capture means making `ScreenExtend Audio` the system default output so the OS mixes
//! every app into it. That is a user-visible, crash-sensitive change, so this module:
//!   * persists the previous default (UID + "we changed it" flag + timestamp) before touching it,
//!   * restores it on disable / quit,
//!   * restores it on the NEXT launch if we died while active (crash recovery), and
//!   * watches for the user switching output themselves, and does not fight them.
//!
//! The save/switch/restore/recover logic is written against the [`DefaultDevicePort`] trait so it
//! is unit-tested without Core Audio (`test/routing.rs`); the real port and the HAL listeners are
//! at the bottom of the file.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, AudioObjectAddPropertyListener,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertyListenerProc,
    AudioObjectRemovePropertyListener,
};
use serde::{Deserialize, Serialize};

use super::hal;

/// Abstraction over "read / set the system default output device by UID". Real impl talks to the
/// HAL; the tests use an in-memory fake to exercise the crash-recovery paths.
pub trait DefaultDevicePort {
    fn current_default_uid(&self) -> Option<String>;
    fn set_default_uid(&self, uid: &str) -> bool;
    /// A real (non-virtual) output device UID to restore to when we have no better memory — used
    /// only for the edge case where we are already the default at activation. Never returns our own
    /// device. Default `None`.
    fn fallback_output_uid(&self) -> Option<String> {
        None
    }
}

/// Persisted across launches so a crash while active can be undone (PRD §8.3, crash recovery).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RoutingState {
    /// UID of the output device we replaced (the one to restore).
    pub saved_uid: String,
    /// True while `ScreenExtend Audio` is (believed to be) the default because we set it.
    pub changed: bool,
    /// Unix seconds when we last set it (diagnostics only).
    pub timestamp: u64,
}

impl RoutingState {
    pub fn load(path: &Path) -> RoutingState {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, bytes);
        }
    }

    pub fn clear(path: &Path) {
        RoutingState::default().save(path);
    }
}

/// What a default-output-device change means for us.
#[derive(Debug, PartialEq, Eq)]
pub enum DefaultChange {
    /// Nothing relevant changed (still us, or we caused it).
    Ignore,
    /// The user switched to a different device — stop, restore, don't re-assert (PRD §8.3).
    UserSwitchedAway { new_uid: String },
}

/// The routing state machine. Generic over the port so it is testable.
pub struct Router<P: DefaultDevicePort> {
    port: P,
    our_uid: String,
    state_path: PathBuf,
    saved_uid: Option<String>,
    active: bool,
}

impl<P: DefaultDevicePort> Router<P> {
    pub fn new(port: P, our_uid: String, state_path: PathBuf) -> Self {
        Self {
            port,
            our_uid,
            state_path,
            saved_uid: None,
            active: false,
        }
    }

    /// Save the current default and set ourselves as the default output. Idempotent-ish: if we are
    /// already the default (e.g. re-enable after a manual re-select) we still record and persist.
    pub fn activate(&mut self, now_secs: u64) -> Result<(), String> {
        let current = self.port.current_default_uid();
        // Never "save" ourselves as the device to restore to — that would strand the user on a
        // silent device. If we're already the default (e.g. a prior crash), keep any prior memory,
        // else fall back to a real output device.
        let saved: Option<String> = match current {
            Some(uid) if uid != self.our_uid => Some(uid),
            _ => self
                .saved_uid
                .clone()
                .or_else(|| self.port.fallback_output_uid()),
        };
        self.saved_uid = saved.clone();
        if !self.port.set_default_uid(&self.our_uid) {
            return Err("failed to set ScreenExtend Audio as default output".into());
        }
        self.active = true;
        RoutingState {
            saved_uid: saved.unwrap_or_default(),
            changed: true,
            timestamp: now_secs,
        }
        .save(&self.state_path);
        Ok(())
    }

    /// Restore the saved default and clear the persisted flag. Safe to call when not active.
    pub fn restore(&mut self) {
        if !self.active {
            return;
        }
        if let Some(saved) = self.saved_uid.clone() {
            if saved != self.our_uid {
                self.port.set_default_uid(&saved);
            }
        }
        self.active = false;
        RoutingState::clear(&self.state_path);
    }

    /// Handle a default-output-device change notification. Returns the decision for the caller.
    pub fn on_default_changed(&mut self) -> DefaultChange {
        if !self.active {
            return DefaultChange::Ignore;
        }
        match self.port.current_default_uid() {
            Some(uid) if uid == self.our_uid => DefaultChange::Ignore,
            Some(new_uid) => {
                // The user (or the OS) moved output elsewhere. Respect it: stop asserting, treat
                // the new device as the one to keep, and clear our persisted flag.
                self.active = false;
                self.saved_uid = Some(new_uid.clone());
                RoutingState::clear(&self.state_path);
                DefaultChange::UserSwitchedAway { new_uid }
            }
            None => DefaultChange::Ignore,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn saved_uid(&self) -> Option<&str> {
        self.saved_uid.as_deref()
    }

    /// Test hook: simulate the OS/user changing the default output device out from under us.
    #[cfg(test)]
    pub fn set_default_for_test(&self, uid: &str) {
        self.port.set_default_uid(uid);
    }
}

/// Crash recovery — run once, early, on every launch (PRD §8.3). If we persisted that we were the
/// default output and we still are, restore the saved device and clear the flag. Pure logic over
/// the port so it is testable; `recover_on_launch` wires in the real port + path.
pub fn recover<P: DefaultDevicePort>(port: &P, our_uid: &str, state_path: &Path) -> bool {
    let state = RoutingState::load(state_path);
    if !state.changed {
        return false;
    }
    let is_us = port.current_default_uid().as_deref() == Some(our_uid);
    if is_us && !state.saved_uid.is_empty() && state.saved_uid != our_uid {
        port.set_default_uid(&state.saved_uid);
    }
    RoutingState::clear(state_path);
    is_us
}

// ── Real Core Audio port + persistence path ─────────────────────────────────────────────────────

/// Talks to the HAL for the default-output device.
pub struct HalDefaultDevicePort;

impl DefaultDevicePort for HalDefaultDevicePort {
    fn current_default_uid(&self) -> Option<String> {
        let dev = hal::default_output_device();
        if dev == 0 {
            None
        } else {
            hal::device_uid(dev)
        }
    }

    fn set_default_uid(&self, uid: &str) -> bool {
        match hal::device_by_uid(uid) {
            Some(dev) => hal::set_default_output_device(dev) == 0,
            None => false,
        }
    }

    fn fallback_output_uid(&self) -> Option<String> {
        hal::all_devices().into_iter().find_map(|d| {
            let uid = hal::device_uid(d)?;
            if uid != super::branding::DEVICE_UID && hal::has_output_channels(d) {
                Some(uid)
            } else {
                None
            }
        })
    }
}

/// `~/Library/Application Support/app.screenextend.desktop/legacy_audio_routing.json`.
pub fn default_state_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library")
        .join("Application Support")
        .join("app.screenextend.desktop")
        .join("legacy_audio_routing.json")
}

/// Crash-recovery entry point for app launch (PRD §8.3). Restores the user's output device if we
/// died while holding it. Cheap and side-effect-free when there is nothing to recover.
pub fn recover_on_launch() {
    let path = default_state_path();
    if recover(&HalDefaultDevicePort, super::branding::DEVICE_UID, &path) {
        crate::tprintln!(
            "audio(legacy): crash recovery restored the previous default output device"
        );
    }
}

/// True if a *different* ScreenExtend process is running (so the main app, not us, owns recovery).
fn another_instance_running() -> bool {
    let self_pid = std::process::id();
    std::process::Command::new("pgrep")
        .args(["-x", "ScreenExtend"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .any(|pid| pid != self_pid)
        })
        .unwrap_or(false)
}

/// Watchdog entry point invoked by the launch agent (`ScreenExtend audio-recover`, §8.3). Unlike
/// [`recover_on_launch`], it first checks the main app isn't running — if it is, the app owns the
/// device and will restore it itself, so we must not stomp a live session.
pub fn watchdog_recover() {
    if another_instance_running() {
        return;
    }
    recover_on_launch();
}

const WATCHDOG_LABEL: &str = "app.screenextend.desktop.audiowatchdog";

fn watchdog_plist_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{WATCHDOG_LABEL}.plist"))
}

/// Install (idempotently) a per-user LaunchAgent that runs `audio-recover` periodically, so a crash
/// is undone even if the user never relaunches ScreenExtend (PRD §8.3). Best-effort, no root
/// required (it's a user agent).
pub fn install_watchdog_agent() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{WATCHDOG_LABEL}</string>
  <key>ProgramArguments</key><array>
    <string>{}</string>
    <string>audio-recover</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>StartInterval</key><integer>30</integer>
  <key>ProcessType</key><string>Background</string>
</dict></plist>
"#,
        exe.display()
    );
    let path = watchdog_plist_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&path, plist).is_err() {
        return;
    }
    // Load it into the current GUI session (harmless if already loaded).
    // SAFETY: getuid() is always safe.
    let uid = format!("gui/{}", unsafe { libc::getuid() });
    let _ = std::process::Command::new("launchctl")
        .args(["bootstrap", &uid, &path.to_string_lossy()])
        .output();
}

// ── HAL listeners for default-device + device-list changes ──────────────────────────────────────

/// Events posted from HAL notification callbacks to the legacy control thread (PRD §8.3: callbacks
/// arrive on Core Audio threads we don't own, so we only `try_send` and never block them).
#[derive(Debug, Clone, Copy)]
pub enum RoutingEvent {
    DefaultOutput,
    DeviceList,
    /// The playthrough device changed its sample rate in place (e.g. AirPods entering call mode) —
    /// re-sync the playthrough format even though the device id is unchanged (§8.3).
    PlaythroughFormat,
}

struct ListenerCtx {
    tx: crossbeam_channel::Sender<RoutingEvent>,
    event: RoutingEvent,
}

extern "C-unwind" fn routing_listener(
    _obj: AudioObjectID,
    _n: u32,
    _addrs: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    // SAFETY: client_data is our Box<ListenerCtx>, alive until we remove the listener.
    let ctx = unsafe { &*(client_data as *const ListenerCtx) };
    let _ = ctx.tx.try_send(ctx.event);
    0
}

fn system_addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// RAII registration of the default-output-device and device-list listeners. Removes them on drop.
pub struct RoutingListeners {
    entries: Vec<(AudioObjectPropertyAddress, *mut ListenerCtx)>,
}

// SAFETY: the ctx pointers are owned solely by this struct and freed on drop.
unsafe impl Send for RoutingListeners {}

impl RoutingListeners {
    pub fn register(tx: crossbeam_channel::Sender<RoutingEvent>) -> RoutingListeners {
        let mut entries = Vec::new();
        for (selector, event) in [
            (
                kAudioHardwarePropertyDefaultOutputDevice,
                RoutingEvent::DefaultOutput,
            ),
            (kAudioHardwarePropertyDevices, RoutingEvent::DeviceList),
        ] {
            let addr = system_addr(selector);
            let ctx = Box::into_raw(Box::new(ListenerCtx {
                tx: tx.clone(),
                event,
            }));
            let listener: AudioObjectPropertyListenerProc = Some(routing_listener);
            // SAFETY: register on the system object; ctx outlives the registration (freed on drop).
            let st = unsafe {
                AudioObjectAddPropertyListener(
                    kAudioObjectSystemObject as AudioObjectID,
                    NonNull::from(&addr),
                    listener,
                    ctx as *mut c_void,
                )
            };
            if st == 0 {
                entries.push((addr, ctx));
            } else {
                // SAFETY: registration failed; reclaim the box.
                drop(unsafe { Box::from_raw(ctx) });
                crate::teprintln!("audio(legacy): routing listener register failed ({st})");
            }
        }
        RoutingListeners { entries }
    }
}

impl Drop for RoutingListeners {
    fn drop(&mut self) {
        let listener: AudioObjectPropertyListenerProc = Some(routing_listener);
        for (addr, ctx) in self.entries.drain(..) {
            // SAFETY: remove the listener we registered with the same address + clientData, then
            // free the ctx.
            unsafe {
                let _ = AudioObjectRemovePropertyListener(
                    kAudioObjectSystemObject as AudioObjectID,
                    NonNull::from(&addr),
                    listener,
                    ctx as *mut c_void,
                );
                drop(Box::from_raw(ctx));
            }
        }
    }
}

/// Listens for a nominal-sample-rate change on ONE device (the current playthrough target). Unlike
/// the device-list listener, this catches an in-place format change (AirPods call mode), so we can
/// re-sync the playthrough IOProc to the new rate. Re-registered whenever the target changes.
pub struct FormatListener {
    device: AudioObjectID,
    addr: AudioObjectPropertyAddress,
    ctx: *mut ListenerCtx,
}

// SAFETY: the ctx pointer is owned solely here and freed on drop.
unsafe impl Send for FormatListener {}

impl FormatListener {
    pub fn register(
        device: AudioObjectID,
        tx: crossbeam_channel::Sender<RoutingEvent>,
    ) -> Option<FormatListener> {
        if device == 0 {
            return None;
        }
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyNominalSampleRate,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let ctx = Box::into_raw(Box::new(ListenerCtx {
            tx,
            event: RoutingEvent::PlaythroughFormat,
        }));
        let listener: AudioObjectPropertyListenerProc = Some(routing_listener);
        // SAFETY: register on `device`; ctx outlives the registration (freed on drop).
        let st = unsafe {
            AudioObjectAddPropertyListener(
                device,
                NonNull::from(&addr),
                listener,
                ctx as *mut c_void,
            )
        };
        if st == 0 {
            Some(FormatListener { device, addr, ctx })
        } else {
            // SAFETY: registration failed; reclaim the box.
            drop(unsafe { Box::from_raw(ctx) });
            None
        }
    }
}

impl Drop for FormatListener {
    fn drop(&mut self) {
        let listener: AudioObjectPropertyListenerProc = Some(routing_listener);
        // SAFETY: remove the listener we registered, then free the ctx.
        unsafe {
            let _ = AudioObjectRemovePropertyListener(
                self.device,
                NonNull::from(&self.addr),
                listener,
                self.ctx as *mut c_void,
            );
            drop(Box::from_raw(self.ctx));
        }
    }
}

/// Best output device to send playthrough to right now: the persisted saved device if still
/// present, else the current system default that isn't us (PRD §8.3, headphone/Bluetooth re-point).
pub fn preferred_playthrough_device(
    our_uid: &str,
    saved_uid: Option<&str>,
) -> Option<AudioObjectID> {
    if let Some(uid) = saved_uid {
        if let Some(dev) = hal::device_by_uid(uid) {
            if hal::has_output_channels(dev) {
                return Some(dev);
            }
        }
    }
    let dev = hal::default_output_device();
    if dev != 0 && hal::device_uid(dev).as_deref() != Some(our_uid) && hal::has_output_channels(dev)
    {
        return Some(dev);
    }
    None
}

/// Monotonic-ish wall clock in unix seconds for the persisted timestamp (best-effort).
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

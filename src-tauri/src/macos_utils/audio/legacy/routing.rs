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

pub trait DefaultDevicePort {
    fn current_default_uid(&self) -> Option<String>;
    fn set_default_uid(&self, uid: &str) -> bool;
    fn fallback_output_uid(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RoutingState {
    pub saved_uid: String,
    pub changed: bool,
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

#[derive(Debug, PartialEq, Eq)]
pub enum DefaultChange {
    Ignore,
    UserSwitchedAway { new_uid: String },
}

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

    pub fn activate(&mut self, now_secs: u64) -> Result<(), String> {
        let current = self.port.current_default_uid();
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

    pub fn on_default_changed(&mut self) -> DefaultChange {
        if !self.active {
            return DefaultChange::Ignore;
        }
        match self.port.current_default_uid() {
            Some(uid) if uid == self.our_uid => DefaultChange::Ignore,
            Some(new_uid) => {
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

    #[cfg(test)]
    pub fn set_default_for_test(&self, uid: &str) {
        self.port.set_default_uid(uid);
    }
}

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

/// `~/Library/Application Support/app.screenextend.desktop/legacy_audio_routing.json`
pub fn default_state_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library")
        .join("Application Support")
        .join("app.screenextend.desktop")
        .join("legacy_audio_routing.json")
}

pub fn recover_on_launch() {
    let path = default_state_path();
    if recover(&HalDefaultDevicePort, super::branding::DEVICE_UID, &path) {
        crate::tprintln!(
            "audio(legacy): crash recovery restored the previous default output device"
        );
    }
}

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
    let uid = format!("gui/{}", unsafe { libc::getuid() });
    let _ = std::process::Command::new("launchctl")
        .args(["bootstrap", &uid, &path.to_string_lossy()])
        .output();
}

#[derive(Debug, Clone, Copy)]
pub enum RoutingEvent {
    DefaultOutput,
    DeviceList,
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

pub struct RoutingListeners {
    entries: Vec<(AudioObjectPropertyAddress, *mut ListenerCtx)>,
}

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

pub struct FormatListener {
    device: AudioObjectID,
    addr: AudioObjectPropertyAddress,
    ctx: *mut ListenerCtx,
}

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
            drop(unsafe { Box::from_raw(ctx) });
            None
        }
    }
}

impl Drop for FormatListener {
    fn drop(&mut self) {
        let listener: AudioObjectPropertyListenerProc = Some(routing_listener);
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

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

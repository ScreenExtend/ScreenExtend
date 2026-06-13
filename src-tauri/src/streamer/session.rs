use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Default)]
pub struct LeaveSignal {
    pub left: AtomicBool,
    pub notify: Notify,
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub ip: String,
    pub name: String,
    pub os: String,
    pub screen_size: String,
    pub refresh_rate: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceOverride {
    pub scale: u32,
    pub orientation_portrait: bool,
    pub refresh_rate: u32,
    pub video_scale: u32,
    pub video_quality: u8,
}

pub trait DeviceReporter: Send + Sync + std::fmt::Debug {
    fn report_join(&self, device: DeviceInfo);
    fn report_remove(&self, ip: String);
}

pub type SharedDeviceReporter = Arc<dyn DeviceReporter>;

pub type SharedDeviceOverrides = Arc<Mutex<HashMap<String, DeviceOverride>>>;

pub type SharedDisconnectGrace = Arc<std::sync::atomic::AtomicU64>;

pub const DEFAULT_DISCONNECT_GRACE_SECS: u64 = 10;
pub const MIN_DISCONNECT_GRACE_SECS: u64 = 0;
pub const MAX_DISCONNECT_GRACE_SECS: u64 = 600;

pub fn new_shared_disconnect_grace() -> SharedDisconnectGrace {
    Arc::new(std::sync::atomic::AtomicU64::new(DEFAULT_DISCONNECT_GRACE_SECS))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveDisplay {
    pub display_id: u32,
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
    pub scale: u32,
    pub portrait: bool,
}

impl LiveDisplay {
    pub fn display_params(&self) -> (u32, u32, u32, u32, bool) {
        (self.width, self.height, self.refresh, self.scale, self.portrait)
    }
}

pub type CaptureStopper = Box<dyn FnOnce() + Send>;

#[derive(Default)]
pub struct DeviceSessionState {
    pub reconfig_epoch: u64,
    pub kick_epoch: u64,
    pub session_seq: u64,
    pub live_display: Option<LiveDisplay>,
    pub leave: Option<Arc<LeaveSignal>>,
    pub active_capture: Option<(u64, CaptureStopper)>,
}

impl std::fmt::Debug for DeviceSessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSessionState")
            .field("reconfig_epoch", &self.reconfig_epoch)
            .field("kick_epoch", &self.kick_epoch)
            .field("session_seq", &self.session_seq)
            .field("live_display", &self.live_display)
            .field("leave_armed", &self.leave.is_some())
            .field("active_capture_seq", &self.active_capture.as_ref().map(|(s, _)| *s))
            .finish()
    }
}

pub type SharedSessions = Arc<Mutex<HashMap<String, DeviceSessionState>>>;

pub fn arm_leave(sessions: &SharedSessions, ip: &str) -> Arc<LeaveSignal> {
    let signal = Arc::new(LeaveSignal::default());
    sessions.lock().unwrap().entry(ip.to_string()).or_default().leave = Some(signal.clone());
    signal
}

pub fn signal_leave(sessions: &SharedSessions, ip: &str) {
    let signal = sessions.lock().unwrap().get(ip).and_then(|s| s.leave.clone());
    if let Some(s) = signal {
        s.left.store(true, Ordering::SeqCst);
        s.notify.notify_waiters();
    }
}

pub fn get_live_display(sessions: &SharedSessions, ip: &str) -> Option<LiveDisplay> {
    sessions.lock().unwrap().get(ip).and_then(|s| s.live_display.clone())
}

pub fn set_live_display(sessions: &SharedSessions, ip: &str, display: LiveDisplay) {
    sessions.lock().unwrap().entry(ip.to_string()).or_default().live_display = Some(display);
}

pub fn take_live_display(sessions: &SharedSessions, ip: &str) -> Option<LiveDisplay> {
    sessions.lock().unwrap().get_mut(ip).and_then(|s| s.live_display.take())
}

pub fn bump_reconfig_epoch(sessions: &SharedSessions, ip: &str) {
    let mut map = sessions.lock().unwrap();
    map.entry(ip.to_string()).or_default().reconfig_epoch += 1;
}

pub fn reconfig_epoch(sessions: &SharedSessions, ip: &str) -> u64 {
    sessions.lock().unwrap().get(ip).map(|s| s.reconfig_epoch).unwrap_or(0)
}

pub fn bump_kick_epoch(sessions: &SharedSessions, ip: &str) {
    let mut map = sessions.lock().unwrap();
    map.entry(ip.to_string()).or_default().kick_epoch += 1;
}

pub fn kick_epoch(sessions: &SharedSessions, ip: &str) -> u64 {
    sessions.lock().unwrap().get(ip).map(|s| s.kick_epoch).unwrap_or(0)
}

pub fn set_active_capture(sessions: &SharedSessions, ip: &str, seq: u64, stop: CaptureStopper) {
    sessions.lock().unwrap().entry(ip.to_string()).or_default().active_capture = Some((seq, stop));
}

pub fn take_active_capture(sessions: &SharedSessions, ip: &str) -> Option<CaptureStopper> {
    sessions
        .lock()
        .unwrap()
        .get_mut(ip)
        .and_then(|s| s.active_capture.take())
        .map(|(_, stop)| stop)
}

pub fn take_active_capture_if(sessions: &SharedSessions, ip: &str, seq: u64) -> Option<CaptureStopper> {
    let mut map = sessions.lock().unwrap();
    let state = map.get_mut(ip)?;
    match &state.active_capture {
        Some((s, _)) if *s == seq => state.active_capture.take().map(|(_, stop)| stop),
        _ => None,
    }
}

pub fn next_session_seq(sessions: &SharedSessions, ip: &str) -> u64 {
    let mut map = sessions.lock().unwrap();
    let entry = map.entry(ip.to_string()).or_default();
    entry.session_seq += 1;
    entry.session_seq
}

pub fn is_current_session(sessions: &SharedSessions, ip: &str, seq: u64) -> bool {
    sessions.lock().unwrap().get(ip).map(|s| s.session_seq == seq).unwrap_or(false)
}

#[derive(Clone, Debug, Default)]
pub struct SessionAuth {
    pub session_id: Arc<Mutex<String>>,
    pub otp: Arc<Mutex<String>>,
}

impl SessionAuth {
    pub fn validate(&self, session_id: &str, otp: &str) -> bool {
        let want_session = self.session_id.lock().unwrap();
        let want_otp = self.otp.lock().unwrap();
        !want_session.is_empty()
            && !want_otp.is_empty()
            && want_session.as_str() == session_id
            && want_otp.as_str() == otp
    }
}

pub trait VirtualDisplayController: Send + Sync + std::fmt::Debug {
    fn create_display(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Result<u32, String>;

    fn remove_display(&self, id: u32);

    fn remove_all_displays(&self);
}

pub type SharedVirtualDisplay = Arc<dyn VirtualDisplayController>;

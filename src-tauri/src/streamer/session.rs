use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

#[derive(Default)]
pub struct LeaveSignal {
    pub left: AtomicBool,
    pub notify: Notify,
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub ip: String,
    pub token: String,
    pub name: String,
    pub os: String,
    pub screen_size: String,
    pub refresh_rate: u32,
    pub portrait: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceOverride {
    pub scale: u32,
    pub orientation_portrait: bool,
    pub refresh_rate: u32,
    pub video_scale: u32,
    pub video_quality: u8,
    pub control_enabled: bool,
    /// System audio streaming for this device. Default **false** — audio is privacy-relevant
    /// (it captures everything the host plays) and must be opted in per device (PRD §7.1).
    pub audio_enabled: bool,
}

pub trait DeviceReporter: Send + Sync + std::fmt::Debug {
    fn report_join(&self, device: DeviceInfo);
    fn report_remove(&self, ip: String);
    fn report_join_attempts_paused(&self, _retry_after_secs: u64) {}
}

pub type SharedDeviceReporter = Arc<dyn DeviceReporter>;

pub type SharedDeviceOverrides = Arc<Mutex<HashMap<String, DeviceOverride>>>;

pub type SharedLocalIps = Arc<Mutex<Vec<IpAddr>>>;

pub fn new_shared_local_ips() -> SharedLocalIps {
    Arc::new(Mutex::new(Vec::new()))
}

pub fn mint_device_token() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub type SharedBannedDevices = Arc<Mutex<std::collections::HashSet<String>>>;

pub fn new_shared_banned_devices() -> SharedBannedDevices {
    Arc::new(Mutex::new(std::collections::HashSet::new()))
}

pub fn is_device_banned(banned: &SharedBannedDevices, token: &str, ip: &str) -> bool {
    let set = banned.lock().unwrap();
    (!token.is_empty() && set.contains(token)) || set.contains(ip)
}

pub type SharedApprovedDevices = Arc<Mutex<std::collections::HashSet<String>>>;

pub fn new_shared_approved_devices() -> SharedApprovedDevices {
    Arc::new(Mutex::new(std::collections::HashSet::new()))
}

pub fn is_device_approved(approved: &SharedApprovedDevices, token: &str) -> bool {
    !token.is_empty() && approved.lock().unwrap().contains(token)
}

pub type SharedDisconnectGrace = Arc<std::sync::atomic::AtomicU64>;

pub const DEFAULT_DISCONNECT_GRACE_SECS: u64 = 10;
pub const MIN_DISCONNECT_GRACE_SECS: u64 = 0;
pub const MAX_DISCONNECT_GRACE_SECS: u64 = 600;

pub fn new_shared_disconnect_grace() -> SharedDisconnectGrace {
    Arc::new(std::sync::atomic::AtomicU64::new(
        DEFAULT_DISCONNECT_GRACE_SECS,
    ))
}

pub const DEFAULT_HTTP_PORT: u16 = 8080;
pub const DEFAULT_HTTPS_PORT: u16 = 8443;

#[derive(Debug)]
pub struct ServerPortState {
    pub http: AtomicU16,
    pub https: AtomicU16,
}

pub type SharedServerPorts = Arc<ServerPortState>;

pub fn new_shared_server_ports() -> SharedServerPorts {
    Arc::new(ServerPortState {
        http: AtomicU16::new(DEFAULT_HTTP_PORT),
        https: AtomicU16::new(DEFAULT_HTTPS_PORT),
    })
}

impl ServerPortState {
    pub fn get(&self) -> (u16, u16) {
        (
            self.http.load(Ordering::Relaxed),
            self.https.load(Ordering::Relaxed),
        )
    }

    pub fn set(&self, http: u16, https: u16) {
        self.http.store(http, Ordering::Relaxed);
        self.https.store(https, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserTurnConfig {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

pub type SharedTurnConfig = Arc<Mutex<UserTurnConfig>>;

pub fn new_shared_turn_config() -> SharedTurnConfig {
    Arc::new(Mutex::new(UserTurnConfig::default()))
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
        (
            self.width,
            self.height,
            self.refresh,
            self.scale,
            self.portrait,
        )
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
            .field(
                "active_capture_seq",
                &self.active_capture.as_ref().map(|(s, _)| *s),
            )
            .finish()
    }
}

pub type SharedSessions = Arc<Mutex<HashMap<String, DeviceSessionState>>>;

pub fn arm_leave(sessions: &SharedSessions, ip: &str) -> Arc<LeaveSignal> {
    let signal = Arc::new(LeaveSignal::default());
    sessions
        .lock()
        .unwrap()
        .entry(ip.to_string())
        .or_default()
        .leave = Some(signal.clone());
    signal
}

pub fn signal_leave(sessions: &SharedSessions, ip: &str) {
    let signal = sessions
        .lock()
        .unwrap()
        .get(ip)
        .and_then(|s| s.leave.clone());
    if let Some(s) = signal {
        s.left.store(true, Ordering::SeqCst);
        s.notify.notify_waiters();
    }
}

pub fn get_live_display(sessions: &SharedSessions, ip: &str) -> Option<LiveDisplay> {
    sessions
        .lock()
        .unwrap()
        .get(ip)
        .and_then(|s| s.live_display.clone())
}

pub fn set_live_display(sessions: &SharedSessions, ip: &str, display: LiveDisplay) {
    sessions
        .lock()
        .unwrap()
        .entry(ip.to_string())
        .or_default()
        .live_display = Some(display);
}

pub fn take_live_display(sessions: &SharedSessions, ip: &str) -> Option<LiveDisplay> {
    sessions
        .lock()
        .unwrap()
        .get_mut(ip)
        .and_then(|s| s.live_display.take())
}

pub fn bump_reconfig_epoch(sessions: &SharedSessions, ip: &str) {
    let mut map = sessions.lock().unwrap();
    map.entry(ip.to_string()).or_default().reconfig_epoch += 1;
}

pub fn reconfig_epoch(sessions: &SharedSessions, ip: &str) -> u64 {
    sessions
        .lock()
        .unwrap()
        .get(ip)
        .map(|s| s.reconfig_epoch)
        .unwrap_or(0)
}

pub fn bump_kick_epoch(sessions: &SharedSessions, ip: &str) {
    let mut map = sessions.lock().unwrap();
    map.entry(ip.to_string()).or_default().kick_epoch += 1;
}

pub fn kick_epoch(sessions: &SharedSessions, ip: &str) -> u64 {
    sessions
        .lock()
        .unwrap()
        .get(ip)
        .map(|s| s.kick_epoch)
        .unwrap_or(0)
}

pub fn set_active_capture(sessions: &SharedSessions, ip: &str, seq: u64, stop: CaptureStopper) {
    sessions
        .lock()
        .unwrap()
        .entry(ip.to_string())
        .or_default()
        .active_capture = Some((seq, stop));
}

pub fn take_active_capture(sessions: &SharedSessions, ip: &str) -> Option<CaptureStopper> {
    sessions
        .lock()
        .unwrap()
        .get_mut(ip)
        .and_then(|s| s.active_capture.take())
        .map(|(_, stop)| stop)
}

pub fn take_active_capture_if(
    sessions: &SharedSessions,
    ip: &str,
    seq: u64,
) -> Option<CaptureStopper> {
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
    sessions
        .lock()
        .unwrap()
        .get(ip)
        .map(|s| s.session_seq == seq)
        .unwrap_or(false)
}

#[derive(Clone, Debug, Default)]
pub struct SessionAuth {
    pub session_id: Arc<Mutex<String>>,
    pub otp: Arc<Mutex<String>>,
}

impl SessionAuth {
    pub fn validate(&self, session_id: &str, otp: &str) -> bool {
        use subtle::ConstantTimeEq;
        let want_session = self.session_id.lock().unwrap();
        let want_otp = self.otp.lock().unwrap();
        if want_session.is_empty() || want_otp.is_empty() {
            return false;
        }
        let session_ok = want_session.as_bytes().ct_eq(session_id.as_bytes());
        let otp_ok = want_otp.as_bytes().ct_eq(otp.as_bytes());
        (session_ok & otp_ok).into()
    }
}

/// Max failed OTP attempts allowed (per device) before a lockout kicks in.
pub const MAX_OTP_ATTEMPTS: u32 = 5;
/// How long a device is locked out after exhausting its attempts.
pub const OTP_LOCKOUT: Duration = Duration::from_secs(60);

pub const MAX_GLOBAL_OTP_ATTEMPTS: u32 = 20;
pub const GLOBAL_OTP_WINDOW: Duration = Duration::from_secs(60);
pub const GLOBAL_OTP_PAUSE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, Default)]
struct OtpAttempt {
    failures: u32,
    locked_until: Option<Instant>,
}

#[derive(Debug, Default)]
struct GlobalOtpState {
    failures: u32,
    window_start: Option<Instant>,
    paused_until: Option<Instant>,
}

pub enum OtpOutcome {
    Rejected { remaining: u32 },
    LockedOut { retry_after: Duration },
}

#[derive(Debug, Default)]
pub struct OtpLimiter {
    attempts: Mutex<HashMap<String, OtpAttempt>>,
    global: Mutex<GlobalOtpState>,
}

pub type SharedOtpLimiter = Arc<OtpLimiter>;

pub fn new_shared_otp_limiter() -> SharedOtpLimiter {
    Arc::new(OtpLimiter::new())
}

impl OtpLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global_paused(&self) -> Option<Duration> {
        let mut g = self.global.lock().unwrap();
        match g.paused_until {
            Some(until) => {
                let now = Instant::now();
                if now < until {
                    Some(until - now)
                } else {
                    *g = GlobalOtpState::default();
                    None
                }
            }
            None => None,
        }
    }

    pub fn note_global_failure(&self) -> Option<Duration> {
        let mut g = self.global.lock().unwrap();
        let now = Instant::now();

        if let Some(until) = g.paused_until {
            if now < until {
                return None;
            }
            *g = GlobalOtpState::default();
        }

        match g.window_start {
            Some(start) if now.duration_since(start) < GLOBAL_OTP_WINDOW => {}
            _ => {
                g.window_start = Some(now);
                g.failures = 0;
            }
        }

        g.failures += 1;
        if g.failures >= MAX_GLOBAL_OTP_ATTEMPTS {
            g.paused_until = Some(now + GLOBAL_OTP_PAUSE);
            Some(GLOBAL_OTP_PAUSE)
        } else {
            None
        }
    }

    pub fn reset(&self) {
        self.attempts.lock().unwrap().clear();
        *self.global.lock().unwrap() = GlobalOtpState::default();
    }

    pub fn locked_for(&self, key: &str) -> Option<Duration> {
        let mut map = self.attempts.lock().unwrap();
        let entry = map.get_mut(key)?;
        match entry.locked_until {
            Some(until) => {
                let now = Instant::now();
                if now < until {
                    Some(until - now)
                } else {
                    *entry = OtpAttempt::default();
                    None
                }
            }
            None => None,
        }
    }

    pub fn record_failure(&self, key: &str) -> OtpOutcome {
        let mut map = self.attempts.lock().unwrap();
        let entry = map.entry(key.to_string()).or_default();

        if let Some(until) = entry.locked_until {
            if Instant::now() >= until {
                *entry = OtpAttempt::default();
            }
        }

        entry.failures += 1;
        if entry.failures >= MAX_OTP_ATTEMPTS {
            entry.locked_until = Some(Instant::now() + OTP_LOCKOUT);
            OtpOutcome::LockedOut {
                retry_after: OTP_LOCKOUT,
            }
        } else {
            OtpOutcome::Rejected {
                remaining: MAX_OTP_ATTEMPTS - entry.failures,
            }
        }
    }

    pub fn record_success(&self, key: &str) {
        self.attempts.lock().unwrap().remove(key);
        *self.global.lock().unwrap() = GlobalOtpState::default();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_key_lockout_still_applies() {
        let lim = OtpLimiter::new();
        let key = "1.2.3.4";
        for _ in 0..MAX_OTP_ATTEMPTS - 1 {
            assert!(matches!(
                lim.record_failure(key),
                OtpOutcome::Rejected { .. }
            ));
            assert!(lim.locked_for(key).is_none());
        }
        assert!(matches!(
            lim.record_failure(key),
            OtpOutcome::LockedOut { .. }
        ));
        assert!(lim.locked_for(key).is_some());
    }

    #[test]
    fn rotating_keys_trips_the_global_guard() {
        let lim = OtpLimiter::new();
        let mut tripped = false;
        for i in 0..MAX_GLOBAL_OTP_ATTEMPTS {
            let key = format!("10.0.0.{i}");
            assert!(matches!(
                lim.record_failure(&key),
                OtpOutcome::Rejected { .. }
            ));
            if lim.note_global_failure().is_some() {
                tripped = true;
            }
        }
        assert!(tripped, "global guard should trip once the window fills");
        assert!(
            lim.global_paused().is_some(),
            "new joins should be paused after the guard trips"
        );
    }

    #[test]
    fn success_resets_per_key_and_global() {
        let lim = OtpLimiter::new();
        let key = "192.168.1.5";
        for _ in 0..MAX_OTP_ATTEMPTS - 1 {
            lim.record_failure(key);
        }
        for _ in 0..MAX_GLOBAL_OTP_ATTEMPTS - 1 {
            assert!(lim.note_global_failure().is_none());
        }
        lim.record_success(key);
        assert!(lim.locked_for(key).is_none());
        assert!(lim.global_paused().is_none());
        assert!(lim.note_global_failure().is_none());
    }

    #[test]
    fn session_auth_matches_and_rejects() {
        let auth = SessionAuth::default();
        assert!(!auth.validate("ABCDEFGHJKLM", "123456"));
        *auth.session_id.lock().unwrap() = "ABCDEFGHJKLM".to_string();
        *auth.otp.lock().unwrap() = "123456".to_string();
        assert!(auth.validate("ABCDEFGHJKLM", "123456"));
        assert!(!auth.validate("ABCDEFGHJKLM", "123457")); // wrong otp
        assert!(!auth.validate("XBCDEFGHJKLM", "123456")); // wrong session
        assert!(!auth.validate("ABCDEFGHJKLM", "12345")); // short otp
    }

    #[test]
    fn reset_clears_lockouts_and_pause() {
        let lim = OtpLimiter::new();
        for _ in 0..MAX_OTP_ATTEMPTS {
            lim.record_failure("1.1.1.1");
        }
        for _ in 0..MAX_GLOBAL_OTP_ATTEMPTS {
            lim.note_global_failure();
        }
        assert!(lim.locked_for("1.1.1.1").is_some());
        assert!(lim.global_paused().is_some());
        lim.reset();
        assert!(lim.locked_for("1.1.1.1").is_none());
        assert!(lim.global_paused().is_none());
    }
}

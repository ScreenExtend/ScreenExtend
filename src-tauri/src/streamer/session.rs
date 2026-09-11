use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::lock::LockExt;

#[derive(Default)]
pub struct LeaveSignal {
    pub left: AtomicBool,
    pub notify: Notify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByeReason {
    Kicked,
    HostExit,
}

impl ByeReason {
    pub fn code(self) -> u8 {
        match self {
            ByeReason::Kicked => crate::streamer::input::protocol::bye::KICKED,
            ByeReason::HostExit => crate::streamer::input::protocol::bye::HOST_EXIT,
        }
    }
}

pub struct ControlMsg {
    pub reason: ByeReason,
    pub sent: Option<std::sync::mpsc::Sender<()>>,
}

pub type ControlSender = tokio::sync::mpsc::UnboundedSender<ControlMsg>;
pub type ControlReceiver = tokio::sync::mpsc::UnboundedReceiver<ControlMsg>;

#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    /// The session this row is. One per browser tab; a device with two tabs open produces two.
    pub join_id: String,
    /// Where the device was when it joined. A label — it may change under a live session and it
    /// is recycled between devices, so nothing addresses it.
    pub ip: String,
    /// The device this join belongs to, and the key every per-device setting is stored under.
    pub token: String,
    pub name: String,
    pub os: String,
    pub screen_size: String,
    pub refresh_rate: u32,
    pub portrait: bool,
    pub dpr: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceOverride {
    pub scale: u32,
    pub orientation_portrait: bool,
    pub refresh_rate: u32,
    pub video_scale: u32,
    pub video_quality: u8,
    pub control_enabled: bool,
    pub dpr: f64,
    pub audio_enabled: bool,
    /// Where a pointer-locked client's relative motion may go. Read at join, like
    /// `control_enabled`.
    pub pointer_scope: crate::streamer::input::PointerScope,
    /// Whether the pipeline may drop below `refresh_rate` on its own.
    ///
    /// Off — the default — means `refresh_rate` is a promise, not a ceiling: the stream is
    /// delivered at exactly the rate the user picked, an unchanged frame is repeated at that
    /// cadence rather than skipped, and no controller may lower it. On, the pipeline is free to
    /// let the rate fall (a static screen costs nothing to leave alone; a congested link is
    /// better served by fewer, better frames). The user asked for a fixed rate to be fixed, so
    /// the adaptive behaviour is opt-in per device.
    ///
    /// Read by `windows_utils::streamer::pipeline` through `Config::auto_frame_rate`, which
    /// `server.rs` copies out of this at join.
    pub auto_frame_rate: bool,
}

/// What one settings edit costs the stream the device is already watching.
///
/// The desktop UI writes the whole override on every save, so the interesting question is never
/// "was `set_device_override` called" but "did any field that the live pipeline reads actually
/// move". Each variant names a class of field, and the variants are ordered by how much of the
/// session they disturb so a run of fields folds with `max`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReconfigAction {
    /// Classes (a), (b) and (c): the name and its labels, remote control, the pointer scope,
    /// system audio on/off and the output device. None of them is an encoder-session parameter —
    /// the injector and the audio fan-out read them live — so the stream is told nothing at all.
    #[default]
    Nothing,
    /// Display scale alone. The extended desktop's DPI changes under a stream whose geometry is
    /// already right, so the host applies it to the live display and the client never hears about
    /// it.
    ScaleLive,
    /// Classes (d) and the rest of (e): geometry, refresh rate, DPR, the encoder's input scale
    /// and its quality target. These are fixed when the encoder session is built, so the pipeline
    /// restarts at the new mode and the client renegotiates **in place** — same join id, same
    /// display, the last frame held until the new one paints.
    Rebuild,
}

impl DeviceOverride {
    /// Which class the difference between two saved overrides falls into.
    ///
    /// This is the whole rule in one place, and the unit test beside it is the field→action
    /// table. Anything not named here is cosmetic by construction: adding a field to
    /// `DeviceOverride` without adding it to this match means "changing it does nothing to the
    /// stream", which is the safe default and the one the caller can see.
    pub fn reconfig_action(old: &Self, new: &Self) -> ReconfigAction {
        let rebuild = old.orientation_portrait != new.orientation_portrait
            || old.refresh_rate != new.refresh_rate
            || old.video_scale != new.video_scale
            || old.video_quality != new.video_quality
            || old.auto_frame_rate != new.auto_frame_rate
            || (old.dpr - new.dpr).abs() > f64::EPSILON;
        if rebuild {
            ReconfigAction::Rebuild
        } else if old.scale != new.scale {
            ReconfigAction::ScaleLive
        } else {
            ReconfigAction::Nothing
        }
    }

    /// The values the join path would have used, for the single-field commands
    /// (`set_device_pointer_scope`, `set_device_auto_frame_rate`) when nothing is saved for a
    /// device yet. Writing one field must not silently pin a resolution or a refresh rate.
    pub fn join_defaults() -> Self {
        Self {
            scale: 100,
            orientation_portrait: false,
            refresh_rate: 60,
            video_scale: 100,
            video_quality: 15,
            control_enabled: true,
            dpr: 1.0,
            audio_enabled: false,
            pointer_scope: crate::streamer::input::PointerScope::default(),
            auto_frame_rate: false,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct AudioOutput {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct AudioOutputsReport {
    pub supported: bool,
    pub outputs: Vec<AudioOutput>,
    /// What the host last asked this device to play on.
    pub selected: String,
    /// What the device reports it is *actually* playing on. The two diverge the moment the
    /// user changes the output in the browser's own UI, which only the client can see.
    /// `None` means this client never said — an older one, or one that has not posted yet —
    /// which is a different answer from `Some("")`, the device's default output.
    pub current: Option<String>,
    /// Whether the labels beside the ids above are the operating system's or this page's own.
    /// `Some(false)` means the browser withheld the real names and the client numbered them
    /// ("Output 1", "Output 2") rather than spend a microphone prompt on them, so they must
    /// not be presented as device names. `None` means the client never said.
    pub labelled: Option<bool>,
}

pub trait DeviceReporter: Send + Sync + std::fmt::Debug {
    fn report_join(&self, device: DeviceInfo);
    /// The *same* join, offering again after a display change.
    ///
    /// A renegotiation is not an arrival: the row is already on the dashboard, the display is
    /// still up and the user did nothing but change a setting. Reporting it as a join made the
    /// desktop app raise "a device joined" every time a refresh rate or a DPR moved, which is the
    /// shape of noise the whole reuse path exists to remove. The default forwards, so a reporter
    /// that does not care about the distinction keeps its old behaviour.
    fn report_modify(&self, device: DeviceInfo) {
        self.report_join(device);
    }
    /// One join ended. The join id is the identity — the device may still hold other joins, and
    /// its IP is only ever a label.
    fn report_remove(&self, join_id: String, token: String);
    fn report_join_attempts_paused(&self, _retry_after_secs: u64) {}
    fn report_audio_outputs(&self, _join_id: String, _token: String, _report: AudioOutputsReport) {}
}

pub type SharedDeviceReporter = Arc<dyn DeviceReporter>;

/// Per-device settings, keyed by the device **token** — never the IP.
///
/// An address is recycled: a phone that leaves and a laptop that picks up its DHCP lease are the
/// same string and a different device, and the settings of the first must not land on the second.
/// The token is minted once per device on its first successful OTP join and presented on every
/// join after that, so it survives a roam, a lease renewal and a NAT rebind.
pub type SharedDeviceOverrides = Arc<Mutex<HashMap<String, DeviceOverride>>>;

/// The audio output the host has asked a device to play on, keyed by device token.
///
/// Per device rather than per join: two tabs of one browser share one set of speakers, and the
/// choice has to outlive every join so it can be replayed from `config.json` into a host with
/// nothing connected yet.
pub type SharedAudioSinks = Arc<Mutex<HashMap<String, String>>>;

pub fn new_shared_audio_sinks() -> SharedAudioSinks {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn set_device_audio_sink(sinks: &SharedAudioSinks, token: &str, sink: Option<String>) {
    if token.is_empty() {
        return;
    }
    let mut map = sinks.lock_recover();
    match sink {
        Some(s) if !s.is_empty() => {
            map.insert(token.to_string(), s);
        }
        _ => {
            map.remove(token);
        }
    }
}

pub fn device_audio_sink(sinks: &SharedAudioSinks, token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    sinks.lock_recover().get(token).cloned()
}

pub type SharedLocalIps = Arc<Mutex<Vec<IpAddr>>>;

pub fn new_shared_local_ips() -> SharedLocalIps {
    Arc::new(Mutex::new(Vec::new()))
}

pub fn mint_device_token() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The identity of one live session — one browser tab — minted by the host at `POST /whep` and
/// handed back as the WHEP resource id.
///
/// 128 bits from the same source as the device token, because it is the capability that
/// `PATCH`/`DELETE /whep/{id}`, `/reconfig`, `/leave` and `/audio-outputs` are addressed by. It
/// deliberately carries nothing about the peer: an address changes under a session (a Wi-Fi roam,
/// a DHCP renewal, a NAT rebind) and is recycled between devices, so it can be a label and never
/// a name.
pub fn mint_join_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub type SharedBannedDevices = Arc<Mutex<std::collections::HashSet<String>>>;

pub fn new_shared_banned_devices() -> SharedBannedDevices {
    Arc::new(Mutex::new(std::collections::HashSet::new()))
}

/// Whether this **device** is banned.
///
/// By token only. An IP ban used to be honoured too, and it is the one shape of ban that can hit
/// the wrong machine: the address a banned phone had this morning is the address the owner's
/// laptop gets this afternoon, and the laptop is then locked out with nothing in the UI to
/// explain it. A device that has never completed an OTP join has no token, and there is nothing
/// to ban it by — it has never been seen.
pub fn is_device_banned(banned: &SharedBannedDevices, token: &str) -> bool {
    !token.is_empty() && banned.lock_recover().contains(token)
}

pub type SharedApprovedDevices = Arc<Mutex<std::collections::HashSet<String>>>;

pub fn new_shared_approved_devices() -> SharedApprovedDevices {
    Arc::new(Mutex::new(std::collections::HashSet::new()))
}

pub fn is_device_approved(approved: &SharedApprovedDevices, token: &str) -> bool {
    !token.is_empty() && approved.lock_recover().contains(token)
}

/// Whether a token the client presented is one **this host already keeps settings under**.
///
/// The join path mints a fresh token on every successful OTP, which is right for a device the
/// host has never seen and wrong for one it has: a per-device setting is keyed by the token, so
/// re-minting renames the device and orphans the row that was just written for it. That is what a
/// settings change looks like from the client — the epoch bumps, the session rebuilds, the rejoin
/// is handed a new name, and the display comes back with the *defaults* the user had just
/// changed. Under `serve` it happens on every single reconfigure, because nothing there ever
/// writes the approval the desktop UI writes from its `deviceJoin` handler.
///
/// Continuity is granted only for a token the host itself issued and still has state for —
/// never for an arbitrary string a client offers, because the trust maps are keyed by this value
/// and a device that could choose its own name could choose one that is already approved.
pub fn is_known_device(overrides: &SharedDeviceOverrides, token: &str) -> bool {
    is_device_token(token) && overrides.lock_recover().contains_key(token)
}

/// The shape [`mint_device_token`] produces: 128 bits as lowercase hex.
///
/// A client presents this string, so it is parsed before it is compared: a length or alphabet
/// this host never issues cannot be one of ours whatever a map happens to contain.
pub fn is_device_token(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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

/// The STUN servers offered to a peer that is not already known to be on this subnet.
///
/// `DEFAULT_STUN_URL` was a compile-time constant with no way to change or disable it, and the
/// only writer outside `Config::default` was `--stun` in the developer probe harness, which is not
/// wired into the shipping binary. A same-subnet peer is no longer offered STUN at all, so the
/// common case costs nothing — but a cross-network or unclassified join still contacted a Google
/// endpoint, with no setting and no way to point it at a private server.
pub type SharedStunConfig = Arc<Mutex<Vec<String>>>;

pub fn new_shared_stun_config() -> SharedStunConfig {
    Arc::new(Mutex::new(vec![
        crate::streamer::config::DEFAULT_STUN_URL.to_string()
    ]))
}

/// Parse a comma- or whitespace-separated STUN list the way the TURN surface parses its own.
///
/// An empty string means "no STUN at all", which is a legitimate setting: on a LAN-only host it
/// removes the last reason for the join path to touch the public internet.
pub fn parse_stun_urls(raw: &str) -> Vec<String> {
    raw.split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveDisplay {
    pub display_id: u32,
    pub device_name: String,
    /// The OS's stable name for the *monitor*, as opposed to the slot it occupies.
    ///
    /// On Windows `device_name` is a `\\.\DISPLAYn`, which the OS hands straight back out to the
    /// next display to attach — so a name recorded when this display was created can address a
    /// different monitor, a physical one included, by the time a reconfigure writes a mode or a
    /// DPI to it. This is the monitor's device-interface path, and every write re-derives the
    /// name from it. Empty means "the backend has no such identity", which on Windows refuses
    /// every write that cannot otherwise prove its target is one of ours.
    pub device_path: String,
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
    pub scale: u32,
    pub portrait: bool,
}

/// What a display write is addressed to: an identity, and the name it last answered to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayTarget {
    pub device_path: String,
    pub device_name: String,
}

impl LiveDisplay {
    pub fn target(&self) -> DisplayTarget {
        DisplayTarget {
            device_path: self.device_path.clone(),
            device_name: self.device_name.clone(),
        }
    }

    pub fn display_params(&self) -> (u32, u32, u32, u32, bool) {
        (
            self.width,
            self.height,
            self.refresh,
            self.scale,
            self.portrait,
        )
    }

    pub fn swap_axes(&self) -> bool {
        self.portrait != (self.height > self.width)
    }
}

pub type CaptureStopper = Box<dyn FnOnce() + Send>;

/// Everything one live join owns.
///
/// The map this lives in is keyed by the **join id**, so two tabs of one browser — same device,
/// same token, same address — are two entries with two displays, and neither can reach the
/// other's. `device_token` is the device this join belongs to, and it is what every per-device
/// lookup (settings, trust, the audio sink) is keyed by. `ip` is a label for the UI and the
/// logs: it is refreshed from whichever request arrives last, and nothing is ever addressed by
/// it.
#[derive(Default)]
pub struct DeviceSessionState {
    pub device_token: String,
    /// The peer's address as of the most recent request on this session. A display hint only.
    pub ip: String,
    /// The cloud relay's id for this client, when the join arrived through the tunnel. The relay
    /// announces a departure by that id and nothing else, so it is the only way to map its
    /// `client_gone` onto the joins it ended.
    pub relay_client: Option<String>,
    pub device_name: String,
    pub screen_size: String,
    pub reconfig_epoch: u64,
    pub kick_epoch: u64,
    pub session_seq: u64,
    pub live_display: Option<LiveDisplay>,
    /// The class-(b) input settings this session's injector is reading right now. Kept here so a
    /// per-device command can reach every tab the device has open without going through the
    /// transport.
    pub live_input: Option<crate::streamer::input::SharedLiveInput>,
    pub leave: Option<Arc<LeaveSignal>>,
    pub control: Option<ControlSender>,
    pub active_capture: Option<(u64, CaptureStopper)>,
    pub host_ip: Option<String>,
    pub audio_outputs: Vec<AudioOutput>,
    pub audio_outputs_supported: bool,
    pub current_audio_output: Option<String>,
    pub audio_outputs_labelled: Option<bool>,
}

impl std::fmt::Debug for DeviceSessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSessionState")
            .field("device_token", &self.device_token)
            .field("ip", &self.ip)
            .field("reconfig_epoch", &self.reconfig_epoch)
            .field("kick_epoch", &self.kick_epoch)
            .field("session_seq", &self.session_seq)
            .field("live_display", &self.live_display)
            .field("leave_armed", &self.leave.is_some())
            .field("control_armed", &self.control.is_some())
            .field(
                "active_capture_seq",
                &self.active_capture.as_ref().map(|(s, _)| *s),
            )
            .finish()
    }
}

/// Live sessions, keyed by join id. Never by address.
pub type SharedSessions = Arc<Mutex<HashMap<String, DeviceSessionState>>>;

/// Open the session for `join`, creating it if the join is new.
///
/// Every write below goes through this rather than `entry().or_default()` on its own, so a state
/// that a settings command creates before the client has connected still carries the device it
/// belongs to.
pub fn open_session(sessions: &SharedSessions, join: &str, token: &str, ip: &str) {
    open_session_via(sessions, join, token, ip, None)
}

pub fn open_session_via(
    sessions: &SharedSessions,
    join: &str,
    token: &str,
    ip: &str,
    relay_client: Option<&str>,
) {
    let mut map = sessions.lock_recover();
    let entry = map.entry(join.to_string()).or_default();
    if !token.is_empty() {
        entry.device_token = token.to_string();
    }
    if !ip.is_empty() {
        entry.ip = ip.to_string();
    }
    if let Some(relay) = relay_client {
        entry.relay_client = Some(relay.to_string());
    }
}

/// Every live join the cloud relay announced under one `client_id`.
pub fn joins_for_relay_client(sessions: &SharedSessions, client_id: &str) -> Vec<String> {
    sessions
        .lock_recover()
        .iter()
        .filter(|(_, s)| s.relay_client.as_deref() == Some(client_id))
        .map(|(join, _)| join.clone())
        .collect()
}

/// Record what the UI shows beside a join: the name the client chose and the size it asked for.
pub fn set_session_labels(sessions: &SharedSessions, join: &str, name: &str, screen_size: &str) {
    let mut map = sessions.lock_recover();
    let entry = map.entry(join.to_string()).or_default();
    entry.device_name = name.to_string();
    entry.screen_size = screen_size.to_string();
}

/// Refresh the address label after a request from a session that is already open.
///
/// A roam or a lease renewal moves a live session to a new address; nothing about the session
/// changes, so this only keeps the dashboard row honest.
pub fn note_session_ip(sessions: &SharedSessions, join: &str, ip: &str) {
    if ip.is_empty() {
        return;
    }
    let mut map = sessions.lock_recover();
    if let Some(state) = map.get_mut(join) {
        if state.ip != ip {
            state.ip = ip.to_string();
        }
    }
}

/// Forget a join entirely. Called once, when its display is gone for good.
///
/// The map used to be keyed by address, so it was bounded by the number of devices that had ever
/// connected and nothing had to clean up. Keyed by join, every tab that is ever opened is an
/// entry, so one has to.
pub fn remove_session(sessions: &SharedSessions, join: &str) {
    sessions.lock_recover().remove(join);
}

pub fn device_token_for(sessions: &SharedSessions, join: &str) -> Option<String> {
    sessions
        .lock_recover()
        .get(join)
        .map(|s| s.device_token.clone())
        .filter(|t| !t.is_empty())
}

pub fn session_ip(sessions: &SharedSessions, join: &str) -> Option<String> {
    sessions
        .lock_recover()
        .get(join)
        .map(|s| s.ip.clone())
        .filter(|ip| !ip.is_empty())
}

/// Every live join belonging to one device.
pub fn joins_for_token(sessions: &SharedSessions, token: &str) -> Vec<String> {
    if token.is_empty() {
        return Vec::new();
    }
    sessions
        .lock_recover()
        .iter()
        .filter(|(_, s)| s.device_token == token)
        .map(|(join, _)| join.clone())
        .collect()
}

/// The device token of whatever is connected from `ip` right now.
///
/// A convenience for the CLI and for a caller that still has only an address: it resolves to a
/// real identity when exactly one device is there, and refuses when more than one is, because an
/// address is a place and not a device. Never used on a request path.
pub fn token_for_ip(sessions: &SharedSessions, ip: &str) -> Option<String> {
    let map = sessions.lock_recover();
    let mut found: Option<String> = None;
    for state in map.values() {
        if state.ip != ip || state.device_token.is_empty() {
            continue;
        }
        match &found {
            Some(t) if t != &state.device_token => return None,
            Some(_) => {}
            None => found = Some(state.device_token.clone()),
        }
    }
    found
}

pub fn arm_leave(sessions: &SharedSessions, join: &str) -> Arc<LeaveSignal> {
    let signal = Arc::new(LeaveSignal::default());
    sessions
        .lock_recover()
        .entry(join.to_string())
        .or_default()
        .leave = Some(signal.clone());
    signal
}

pub fn signal_leave(sessions: &SharedSessions, join: &str) {
    let signal = sessions
        .lock_recover()
        .get(join)
        .and_then(|s| s.leave.clone());
    if let Some(s) = signal {
        s.left.store(true, Ordering::SeqCst);
        s.notify.notify_waiters();
    }
}

pub fn set_control_sender(sessions: &SharedSessions, join: &str, tx: ControlSender) {
    sessions
        .lock_recover()
        .entry(join.to_string())
        .or_default()
        .control = Some(tx);
}

pub fn clear_control_sender(sessions: &SharedSessions, join: &str, tx: &ControlSender) {
    let mut map = sessions.lock_recover();
    if let Some(state) = map.get_mut(join) {
        if state.control.as_ref().is_some_and(|c| c.same_channel(tx)) {
            state.control = None;
        }
    }
}

pub fn send_bye(sessions: &SharedSessions, join: &str, reason: ByeReason) -> bool {
    send_bye_acked(sessions, join, reason, None)
}

fn send_bye_acked(
    sessions: &SharedSessions,
    join: &str,
    reason: ByeReason,
    sent: Option<std::sync::mpsc::Sender<()>>,
) -> bool {
    let tx = sessions
        .lock_recover()
        .get(join)
        .and_then(|s| s.control.clone());
    match tx {
        Some(tx) => tx.send(ControlMsg { reason, sent }).is_ok(),
        None => false,
    }
}

pub fn broadcast_bye(sessions: &SharedSessions, reason: ByeReason, timeout: Duration) {
    let joins: Vec<String> = sessions.lock_recover().keys().cloned().collect();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut queued = 0usize;
    for join in &joins {
        if send_bye_acked(sessions, join, reason, Some(tx.clone())) {
            queued += 1;
        }
    }
    drop(tx);
    if queued == 0 {
        return;
    }
    let deadline = Instant::now() + timeout;
    for _ in 0..queued {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        if rx.recv_timeout(left).is_err() {
            break;
        }
    }
}

pub fn get_live_display(sessions: &SharedSessions, join: &str) -> Option<LiveDisplay> {
    sessions
        .lock_recover()
        .get(join)
        .and_then(|s| s.live_display.clone())
}

pub fn set_live_display(sessions: &SharedSessions, join: &str, display: LiveDisplay) {
    sessions
        .lock_recover()
        .entry(join.to_string())
        .or_default()
        .live_display = Some(display);
}

pub fn take_live_display(sessions: &SharedSessions, join: &str) -> Option<LiveDisplay> {
    sessions
        .lock_recover()
        .get_mut(join)
        .and_then(|s| s.live_display.take())
}

/// Hand this session's injector the handle it reads remote control and the pointer scope from.
pub fn set_live_input(
    sessions: &SharedSessions,
    join: &str,
    live: crate::streamer::input::SharedLiveInput,
) {
    sessions
        .lock_recover()
        .entry(join.to_string())
        .or_default()
        .live_input = Some(live);
}

/// Apply a class-(b) input change to every live join of one device. Returns how many it reached.
///
/// No epoch, no renegotiation, no display work: the injector reads both values per event, so the
/// change lands on the next pointer move.
pub fn apply_live_input_for_device(
    sessions: &SharedSessions,
    token: &str,
    control_enabled: bool,
    scope: crate::streamer::input::PointerScope,
) -> usize {
    if token.is_empty() {
        return 0;
    }
    let handles: Vec<crate::streamer::input::SharedLiveInput> = sessions
        .lock_recover()
        .values()
        .filter(|s| s.device_token == token)
        .filter_map(|s| s.live_input.clone())
        .collect();
    for live in &handles {
        live.set_control_enabled(control_enabled);
        live.set_pointer_scope(scope);
    }
    handles.len()
}

pub fn bump_reconfig_epoch(sessions: &SharedSessions, join: &str) {
    let mut map = sessions.lock_recover();
    map.entry(join.to_string()).or_default().reconfig_epoch += 1;
}

/// Bump every live join of one device. What a per-device settings change means, now that a
/// device can hold more than one.
pub fn bump_reconfig_epoch_for_device(sessions: &SharedSessions, token: &str) -> usize {
    let joins = joins_for_token(sessions, token);
    for join in &joins {
        bump_reconfig_epoch(sessions, join);
    }
    joins.len()
}

pub fn reconfig_epoch(sessions: &SharedSessions, join: &str) -> u64 {
    sessions
        .lock_recover()
        .get(join)
        .map(|s| s.reconfig_epoch)
        .unwrap_or(0)
}

pub fn bump_kick_epoch(sessions: &SharedSessions, join: &str) {
    let mut map = sessions.lock_recover();
    map.entry(join.to_string()).or_default().kick_epoch += 1;
}

pub fn kick_epoch(sessions: &SharedSessions, join: &str) -> u64 {
    sessions
        .lock_recover()
        .get(join)
        .map(|s| s.kick_epoch)
        .unwrap_or(0)
}

/// End one join: tell the client, move its kick epoch, and release the session's waiter.
pub fn kick_join(sessions: &SharedSessions, join: &str, reason: ByeReason) {
    send_bye(sessions, join, reason);
    bump_kick_epoch(sessions, join);
    signal_leave(sessions, join);
}

/// End every join of one device. Returns how many there were.
pub fn kick_device(sessions: &SharedSessions, token: &str, reason: ByeReason) -> usize {
    let joins = joins_for_token(sessions, token);
    for join in &joins {
        kick_join(sessions, join, reason);
    }
    joins.len()
}

pub fn set_active_capture(sessions: &SharedSessions, join: &str, seq: u64, stop: CaptureStopper) {
    sessions
        .lock_recover()
        .entry(join.to_string())
        .or_default()
        .active_capture = Some((seq, stop));
}

pub fn take_active_capture(sessions: &SharedSessions, join: &str) -> Option<CaptureStopper> {
    sessions
        .lock_recover()
        .get_mut(join)
        .and_then(|s| s.active_capture.take())
        .map(|(_, stop)| stop)
}

pub fn take_active_capture_if(
    sessions: &SharedSessions,
    join: &str,
    seq: u64,
) -> Option<CaptureStopper> {
    let mut map = sessions.lock_recover();
    let state = map.get_mut(join)?;
    match &state.active_capture {
        Some((s, _)) if *s == seq => state.active_capture.take().map(|(_, stop)| stop),
        _ => None,
    }
}

pub fn next_session_seq(sessions: &SharedSessions, join: &str) -> u64 {
    let mut map = sessions.lock_recover();
    let entry = map.entry(join.to_string()).or_default();
    entry.session_seq += 1;
    entry.session_seq
}

pub fn is_current_session(sessions: &SharedSessions, join: &str, seq: u64) -> bool {
    sessions
        .lock_recover()
        .get(join)
        .map(|s| s.session_seq == seq)
        .unwrap_or(false)
}

pub fn set_audio_outputs(
    sessions: &SharedSessions,
    join: &str,
    supported: bool,
    outputs: Vec<AudioOutput>,
    current: Option<String>,
    labelled: Option<bool>,
) {
    let mut map = sessions.lock_recover();
    let entry = map.entry(join.to_string()).or_default();
    entry.audio_outputs_supported = supported;
    entry.audio_outputs = outputs;
    // A client that does not report one must not erase what a newer one did.
    if current.is_some() {
        entry.current_audio_output = current;
    }
    if labelled.is_some() {
        entry.audio_outputs_labelled = labelled;
    }
}

/// What one join reports about its outputs, with the device's chosen sink beside it.
pub fn audio_outputs_report(
    sessions: &SharedSessions,
    sinks: &SharedAudioSinks,
    join: &str,
) -> AudioOutputsReport {
    let map = sessions.lock_recover();
    let Some(s) = map.get(join) else {
        return AudioOutputsReport::default();
    };
    let token = s.device_token.clone();
    let report = AudioOutputsReport {
        supported: s.audio_outputs_supported,
        outputs: s.audio_outputs.clone(),
        selected: String::new(),
        current: s.current_audio_output.clone(),
        labelled: s.audio_outputs_labelled,
    };
    drop(map);
    AudioOutputsReport {
        selected: device_audio_sink(sinks, &token).unwrap_or_default(),
        ..report
    }
}

/// What the UI asks for: one device's outputs, taken from whichever of its joins has reported.
///
/// Two tabs of one browser enumerate the same speakers, so the first join that answered is as
/// good as any; a join that never reported (`supported == false` with an empty list) is skipped
/// so it cannot mask one that did.
pub fn device_audio_outputs_report(
    sessions: &SharedSessions,
    sinks: &SharedAudioSinks,
    token: &str,
) -> AudioOutputsReport {
    let best = {
        let map = sessions.lock_recover();
        map.values()
            .filter(|s| s.device_token == token)
            .max_by_key(|s| (s.supported_rank(), s.audio_outputs.len()))
            .map(|s| AudioOutputsReport {
                supported: s.audio_outputs_supported,
                outputs: s.audio_outputs.clone(),
                selected: String::new(),
                current: s.current_audio_output.clone(),
                labelled: s.audio_outputs_labelled,
            })
    };
    AudioOutputsReport {
        selected: device_audio_sink(sinks, token).unwrap_or_default(),
        ..best.unwrap_or_default()
    }
}

impl DeviceSessionState {
    fn supported_rank(&self) -> u8 {
        u8::from(self.audio_outputs_supported)
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionAuth {
    pub session_id: Arc<Mutex<String>>,
    pub otp: Arc<Mutex<String>>,
}

impl SessionAuth {
    pub fn validate(&self, session_id: &str, otp: &str) -> bool {
        use subtle::ConstantTimeEq;
        let want_session = self.session_id.lock_recover();
        let want_otp = self.otp.lock_recover();
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
        let mut g = self.global.lock_recover();
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
        let mut g = self.global.lock_recover();
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
        self.attempts.lock_recover().clear();
        *self.global.lock_recover() = GlobalOtpState::default();
    }

    pub fn locked_for(&self, key: &str) -> Option<Duration> {
        let mut map = self.attempts.lock_recover();
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
        let mut map = self.attempts.lock_recover();
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
        self.attempts.lock_recover().remove(key);
        *self.global.lock_recover() = GlobalOtpState::default();
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

    fn create_display_with_modes(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
        extra_modes: &[(u32, u32)],
    ) -> Result<u32, String> {
        let _ = extra_modes;
        self.create_display(name, width, height, refresh_rate)
    }

    fn display_device_name(&self, id: u32) -> Option<String> {
        let _ = id;
        None
    }

    /// The geometry the backend actually landed on, when it knows it.
    ///
    /// `create_display` returns a display id and nothing else, so the server always follows a
    /// successful create with a forced `set_display_mode` and a settle poll — a whole extra
    /// reconfiguration on a path that has already done several. That is unavoidable on a backend
    /// that is *commanded* (the Windows IDD registers a mode list and the OS picks from it, so it
    /// genuinely does not know), but wrong on one that *negotiates*: the macOS AirPlay tier
    /// computes the closest mode from the ladder macOS published and then discards the answer, and
    /// the forced set is guaranteed to fail whenever the requested size is off that ladder.
    ///
    /// Returning `None` means "ask the OS", which is the old behaviour and the default.
    fn created_geometry(&self, id: u32) -> Option<(u32, u32)> {
        let _ = id;
        None
    }

    /// How many displays this backend can drive at once, when it is capped at all.
    ///
    /// Read it through [`concurrent_display_cap`], never directly: the cap is the input to a
    /// user-visible refusal, and exactly one backend is allowed to produce one.
    fn max_concurrent_displays(&self) -> Option<usize> {
        None
    }
}

/// The only capped display backend in the product is the macOS AirPlay tier, which macOS itself
/// limits to one display because macOS owns that display. Every other backend is uncapped, and
/// the product rule is that a host is never told it is full: it never evicts a connected device,
/// and it never serves `at-capacity.html`.
///
/// So this is the one place the cap is read, and it recognises exactly one value. A backend that
/// reports any other finite number is a bug — a capacity nobody designed, arriving as a refusal
/// the user cannot act on — so it is logged once and treated as uncapped, and the join that
/// would have been turned away proceeds and reports whatever really goes wrong.
pub fn concurrent_display_cap(client: &dyn VirtualDisplayController) -> Option<usize> {
    match client.max_concurrent_displays() {
        None => None,
        Some(1) => Some(1),
        Some(other) => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                teprintln!(
                    "[display] {client:?} reports a concurrent-display cap of {other}; only the \
                     one-at-a-time AirPlay tier may cap, so this host is treated as uncapped"
                );
            });
            None
        }
    }
}

/// [`concurrent_display_cap`] for the shared handle the host actually carries.
pub fn shared_display_cap(client: &SharedVirtualDisplay) -> Option<usize> {
    concurrent_display_cap(client.as_ref())
}

pub fn set_host_ip(sessions: &SharedSessions, join: &str, host_ip: Option<String>) {
    sessions
        .lock_recover()
        .entry(join.to_string())
        .or_default()
        .host_ip = host_ip;
}

pub fn display_holder_host_ip(sessions: &SharedSessions) -> Option<String> {
    sessions
        .lock_recover()
        .values()
        .find(|s| s.live_display.is_some())
        .and_then(|s| s.host_ip.clone())
}

pub fn live_displays_except(sessions: &SharedSessions, join: &str) -> Vec<LiveDisplay> {
    sessions
        .lock_recover()
        .iter()
        .filter(|(key, _)| key.as_str() != join)
        .filter_map(|(_, s)| s.live_display.clone())
        .collect()
}

/// Every client address this host currently has a session from.
///
/// The one place that can answer "has anything actually associated to our hosted network and
/// joined?", which is the difference between an AP that is advertised and an AP that works. It
/// reads the label each session carries, not the map's keys — those are join ids now.
pub fn client_ips(sessions: &SharedSessions) -> Vec<String> {
    let mut ips: Vec<String> = sessions
        .lock_recover()
        .values()
        .map(|s| s.ip.clone())
        .filter(|ip| !ip.is_empty())
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// One row per live join, for `devices list` and `status --json`.
pub struct JoinSummary {
    pub join_id: String,
    pub device_token: String,
    pub ip: String,
    pub name: String,
    pub screen_size: String,
    pub display: Option<LiveDisplay>,
}

pub fn join_summaries(sessions: &SharedSessions) -> Vec<JoinSummary> {
    let mut rows: Vec<JoinSummary> = sessions
        .lock_recover()
        .iter()
        .map(|(join, s)| JoinSummary {
            join_id: join.clone(),
            device_token: s.device_token.clone(),
            ip: s.ip.clone(),
            name: s.device_name.clone(),
            screen_size: s.screen_size.clone(),
            display: s.live_display.clone(),
        })
        .collect();
    rows.sort_by(|a, b| a.join_id.cmp(&b.join_id));
    rows
}

pub fn live_display_count(sessions: &SharedSessions) -> usize {
    sessions
        .lock_recover()
        .values()
        .filter(|s| s.live_display.is_some())
        .count()
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
        *auth.session_id.lock_recover() = "ABCDEFGHJKLM".to_string();
        *auth.otp.lock_recover() = "123456".to_string();
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

    // ---- device trust: keyed on the minted token, never the IP -----------------------

    #[test]
    fn a_minted_device_token_is_128_bits_of_lowercase_hex() {
        // The token is the whole device-trust key: a short or predictable one is an auto-join
        // for anyone who guesses it.
        let a = mint_device_token();
        let b = mint_device_token();
        assert_eq!(a.len(), 32, "128 bits as hex");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b, "two mints must not collide");
    }

    #[test]
    fn approval_is_by_token_and_an_empty_token_is_never_approved() {
        // The IP is a display hint. A device that presents no token has never completed an OTP
        // join, so it must go through one — an empty token matching an empty entry would hand
        // every unknown device an auto-join.
        let approved = new_shared_approved_devices();
        let token = mint_device_token();
        assert!(!is_device_approved(&approved, &token));
        approved.lock().unwrap().insert(token.clone());
        assert!(is_device_approved(&approved, &token));
        assert!(!is_device_approved(&approved, ""));
        approved.lock().unwrap().insert(String::new());
        assert!(
            !is_device_approved(&approved, ""),
            "an empty token is never approved"
        );
    }

    #[test]
    fn a_ban_holds_by_token_and_never_by_address() {
        // An address is recycled. Honouring an IP ban locks out whichever device picks up the
        // banned one's lease next, with nothing in the UI to explain it — the exact failure the
        // token exists to prevent.
        let banned = new_shared_banned_devices();
        let token = mint_device_token();
        assert!(!is_device_banned(&banned, &token));
        banned.lock().unwrap().insert(token.clone());
        assert!(is_device_banned(&banned, &token));
        assert!(!is_device_banned(&banned, "other"));
        banned.lock().unwrap().insert("10.0.0.9".to_string());
        assert!(
            !is_device_banned(&banned, ""),
            "a device with no token has never joined and cannot be banned"
        );
    }

    // ---- epoch bookkeeping: how a live settings change reaches the client -------------

    #[test]
    fn an_unknown_join_reads_epoch_zero_rather_than_failing() {
        // The client adopts whatever the first `/reconfig` returns, so a join the host has no
        // state for must read a stable 0 and not a value that moves on its own.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        assert_eq!(reconfig_epoch(&sessions, "join-a"), 0);
        assert_eq!(kick_epoch(&sessions, "join-a"), 0);
        assert_eq!(reconfig_epoch(&sessions, "join-a"), 0);
    }

    #[test]
    fn each_bump_is_visible_exactly_once_and_only_to_that_join() {
        // The client compares the epoch it holds against the one it polls. A bump that moved two
        // joins would rebuild a session nobody changed; a bump that never landed leaves a tab on
        // the old settings forever.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        bump_reconfig_epoch(&sessions, "join-a");
        assert_eq!(reconfig_epoch(&sessions, "join-a"), 1);
        assert_eq!(reconfig_epoch(&sessions, "join-b"), 0, "a bump is per join");
        for expected in 2..=5 {
            bump_reconfig_epoch(&sessions, "join-a");
            assert_eq!(reconfig_epoch(&sessions, "join-a"), expected);
        }
    }

    #[test]
    fn a_per_device_change_reaches_every_tab_that_device_has_open() {
        // Two tabs of one browser share a device token and therefore share every setting. A
        // change made once in the UI has to land on both, and on nobody else's.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let mine = mint_device_token();
        let theirs = mint_device_token();
        open_session(&sessions, "join-1", &mine, "192.168.1.5");
        open_session(&sessions, "join-2", &mine, "192.168.1.5");
        open_session(&sessions, "join-3", &theirs, "192.168.1.6");

        assert_eq!(bump_reconfig_epoch_for_device(&sessions, &mine), 2);
        assert_eq!(reconfig_epoch(&sessions, "join-1"), 1);
        assert_eq!(reconfig_epoch(&sessions, "join-2"), 1);
        assert_eq!(
            reconfig_epoch(&sessions, "join-3"),
            0,
            "another device must not be rebuilt"
        );
    }

    #[test]
    fn the_two_epochs_are_independent() {
        // A settings change must not read as a kick: the client tears the session down and shows
        // "you have been removed" on a kick, and silently rebuilds on a reconfig.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        bump_reconfig_epoch(&sessions, "join-a");
        bump_reconfig_epoch(&sessions, "join-a");
        assert_eq!(
            kick_epoch(&sessions, "join-a"),
            0,
            "a reconfig is not a kick"
        );
        bump_kick_epoch(&sessions, "join-a");
        assert_eq!(kick_epoch(&sessions, "join-a"), 1);
        assert_eq!(
            reconfig_epoch(&sessions, "join-a"),
            2,
            "a kick is not a reconfig"
        );
    }

    // ---- identity: the address is a label, never a name -------------------------------

    #[test]
    fn a_device_that_inherits_a_disconnected_devices_address_inherits_nothing_else() {
        // The owner's case: a phone leaves, the router hands its address to a laptop. Keyed by
        // address the laptop would have walked into the phone's session and its settings.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let overrides: SharedDeviceOverrides = Arc::new(Mutex::new(HashMap::new()));
        let banned = new_shared_banned_devices();
        let sinks = new_shared_audio_sinks();

        let phone = mint_device_token();
        let phone_join = mint_join_id();
        open_session(&sessions, &phone_join, &phone, "192.168.1.42");
        overrides.lock_recover().insert(
            phone.clone(),
            DeviceOverride {
                scale: 175,
                ..DeviceOverride::join_defaults()
            },
        );
        set_device_audio_sink(&sinks, &phone, Some("phone-speaker".into()));
        banned.lock_recover().insert(phone.clone());
        bump_kick_epoch(&sessions, &phone_join);

        // The phone goes away and its session is forgotten.
        remove_session(&sessions, &phone_join);

        // The laptop arrives on the same address with its own token.
        let laptop = mint_device_token();
        let laptop_join = mint_join_id();
        open_session(&sessions, &laptop_join, &laptop, "192.168.1.42");

        assert_ne!(laptop_join, phone_join);
        assert!(
            !is_device_banned(&banned, &laptop),
            "the phone's ban must not reach the laptop"
        );
        assert!(
            overrides.lock_recover().get(&laptop).is_none(),
            "the phone's settings must not reach the laptop"
        );
        assert_eq!(
            device_audio_sink(&sinks, &laptop),
            None,
            "the phone's speaker choice must not reach the laptop"
        );
        assert_eq!(
            kick_epoch(&sessions, &laptop_join),
            0,
            "the laptop starts on a fresh epoch, not the phone's"
        );
        assert!(joins_for_token(&sessions, &phone).is_empty());
        assert_eq!(joins_for_token(&sessions, &laptop), vec![laptop_join]);
    }

    #[test]
    fn a_session_survives_its_peer_changing_address() {
        // A roam or a DHCP renewal moves the peer; the session is named by its join id, so the
        // only thing that changes is the label the dashboard shows.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let token = mint_device_token();
        let join = mint_join_id();
        open_session(&sessions, &join, &token, "192.168.1.9");
        bump_reconfig_epoch(&sessions, &join);
        note_session_ip(&sessions, &join, "10.0.0.4");
        assert_eq!(session_ip(&sessions, &join).as_deref(), Some("10.0.0.4"));
        assert_eq!(
            reconfig_epoch(&sessions, &join),
            1,
            "the session is unchanged by the move"
        );
        assert_eq!(device_token_for(&sessions, &join).as_deref(), Some(&*token));
    }

    #[test]
    fn an_address_resolves_to_a_device_only_while_it_is_unambiguous() {
        // The CLI convenience lookup. One device there resolves; two do not, because an address
        // is a place and the answer would be a coin toss.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let a = mint_device_token();
        let b = mint_device_token();
        open_session(&sessions, "join-1", &a, "192.168.1.5");
        open_session(&sessions, "join-2", &a, "192.168.1.5");
        assert_eq!(token_for_ip(&sessions, "192.168.1.5").as_deref(), Some(&*a));
        open_session(&sessions, "join-3", &b, "192.168.1.5");
        assert_eq!(
            token_for_ip(&sessions, "192.168.1.5"),
            None,
            "two devices behind one address cannot be told apart by it"
        );
        assert_eq!(token_for_ip(&sessions, "192.168.1.9"), None);
    }

    #[test]
    fn a_bump_creates_the_session_entry_rather_than_being_dropped() {
        // Settings can be changed before the device has ever connected — the CLI's `devices set`
        // writes through the same path — and a bump that landed nowhere would be silently lost.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        bump_kick_epoch(&sessions, "join-7");
        assert!(sessions.lock().unwrap().contains_key("join-7"));
        assert_eq!(kick_epoch(&sessions, "join-7"), 1);
    }

    // ---- session sequence numbers: which capture owns the display ---------------------

    #[test]
    fn a_stale_session_never_stops_the_capture_that_replaced_it() {
        // A reconnect on the same join id mints a new sequence; the old session's teardown then
        // runs and must not stop the capture the new one just started. That is what the
        // conditional take is for.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let first = next_session_seq(&sessions, "join-3");
        let second = next_session_seq(&sessions, "join-3");
        assert!(second > first, "each session gets a fresh sequence");
        assert!(!is_current_session(&sessions, "join-3", first));
        assert!(is_current_session(&sessions, "join-3", second));

        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        set_active_capture(
            &sessions,
            "join-3",
            second,
            Box::new(move || flag.store(true, Ordering::SeqCst)),
        );
        assert!(
            take_active_capture_if(&sessions, "join-3", first).is_none(),
            "the stale session must not claim the live capture"
        );
        assert!(take_active_capture_if(&sessions, "join-3", second).is_some());
        assert!(!stopped.load(Ordering::SeqCst), "taking is not stopping");
    }

    #[test]
    fn two_tabs_of_one_device_hold_two_independent_displays() {
        // The owner's report: two tabs on one machine used to collapse into one session, so the
        // first froze and was replaced by the second.
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let token = mint_device_token();
        let (a, b) = (mint_join_id(), mint_join_id());
        let display = |id: u32| LiveDisplay {
            display_id: id,
            device_name: format!("\\\\.\\DISPLAY{id}"),
            device_path: format!("path-{id}"),
            width: 1920,
            height: 1080,
            refresh: 60,
            scale: 100,
            portrait: false,
        };
        open_session(&sessions, &a, &token, "192.168.1.5");
        open_session(&sessions, &b, &token, "192.168.1.5");
        set_live_display(&sessions, &a, display(4));
        set_live_display(&sessions, &b, display(5));

        assert_eq!(live_display_count(&sessions), 2);
        assert_eq!(get_live_display(&sessions, &a).unwrap().display_id, 4);
        assert_eq!(get_live_display(&sessions, &b).unwrap().display_id, 5);
        assert_eq!(joins_for_token(&sessions, &token).len(), 2);

        // One tab closes; the other keeps its display.
        let _ = take_live_display(&sessions, &a);
        remove_session(&sessions, &a);
        assert_eq!(live_display_count(&sessions), 1);
        assert_eq!(get_live_display(&sessions, &b).unwrap().display_id, 5);
    }

    // ---- disconnect grace --------------------------------------------------------------

    #[test]
    fn the_disconnect_grace_default_sits_inside_its_own_range() {
        // The UI and the CLI both clamp to this range; a default outside it would be unreachable
        // from either. Compared through a runtime binding so clippy sees a comparison rather
        // than a constant it can fold — the point is that the three constants agree, and a
        // future edit that broke it would be a compile-time-visible fact only by accident.
        let (lo, hi) = (MIN_DISCONNECT_GRACE_SECS, MAX_DISCONNECT_GRACE_SECS);
        assert!((lo..=hi).contains(&DEFAULT_DISCONNECT_GRACE_SECS));
        let grace = new_shared_disconnect_grace();
        assert_eq!(grace.load(Ordering::Relaxed), DEFAULT_DISCONNECT_GRACE_SECS);
    }

    // ---- display geometry --------------------------------------------------------------

    #[test]
    fn display_params_report_what_was_recorded() {
        let d = LiveDisplay {
            display_id: 4,
            device_name: "4".into(),
            device_path: "p4".into(),
            width: 1920,
            height: 1080,
            refresh: 120,
            scale: 150,
            portrait: false,
        };
        assert_eq!(d.display_params(), (1920, 1080, 120, 150, false));
    }

    // ---- device identity across a settings change ---------------------------------------

    /// The reconfigure a settings change triggers must not rename the device.
    ///
    /// This is the shape of the reported fault: the user changes the refresh rate and the pixel
    /// ratio, the host writes the override under the device's token and bumps the reconfig
    /// epoch, the client tears its session down and joins again — presenting the same token and,
    /// because the page still holds them, the same credentials. If the OTP path mints here, the
    /// rejoin is a *different* device, the override that was just written belongs to nobody, and
    /// the display comes back at the settings the user had just changed away from.
    #[test]
    fn a_device_the_host_has_settings_for_keeps_its_token() {
        let overrides: SharedDeviceOverrides = Arc::new(Mutex::new(HashMap::new()));
        let token = mint_device_token();
        assert!(
            !is_known_device(&overrides, &token),
            "a token with no settings behind it is not continuity, it is a claim"
        );

        overrides
            .lock_recover()
            .insert(token.clone(), DeviceOverride::join_defaults());
        assert!(is_known_device(&overrides, &token));
    }

    /// Continuity is granted on the host's own record, never on the client's word: every trust
    /// map is keyed by this string, so a device that could name itself could name itself
    /// something already approved.
    #[test]
    fn an_unknown_or_malformed_token_is_never_honoured() {
        let overrides: SharedDeviceOverrides = Arc::new(Mutex::new(HashMap::new()));
        let stranger = mint_device_token();
        assert!(!is_known_device(&overrides, &stranger));
        assert!(!is_known_device(&overrides, ""));

        // Even present in the map, a string this host could not have minted is refused: the
        // parse runs first, so a row planted by a corrupt config cannot become an identity.
        for bogus in ["../../etc", "NOTHEX", &"a".repeat(64), &"g".repeat(32)] {
            overrides
                .lock_recover()
                .insert(bogus.to_string(), DeviceOverride::join_defaults());
            assert!(
                !is_known_device(&overrides, bogus),
                "{bogus:?} is not a shape this host mints"
            );
        }
    }

    #[test]
    fn a_minted_token_is_recognised_as_one() {
        for _ in 0..32 {
            let t = mint_device_token();
            assert!(is_device_token(&t), "{t}");
        }
        assert!(!is_device_token(""));
        assert!(!is_device_token(&"0".repeat(31)));
        assert!(!is_device_token(&"0".repeat(33)));
        assert!(
            !is_device_token(&"A".repeat(32)),
            "minted tokens are lowercase hex; accepting uppercase would make one device two"
        );
    }

    /// The field → action table, as one test.
    ///
    /// The owner's report was that editing something as inert as a device name disconnected the
    /// client. The cause was that the desktop UI writes the *whole* override on every save and
    /// the host read "a save happened" as "something changed", so every field was effectively in
    /// the most expensive class. This asserts the classes are what the classes say they are, one
    /// field at a time: move exactly one, and the verdict is exactly its class.
    #[test]
    fn each_field_costs_the_stream_what_its_class_costs() {
        use crate::streamer::input::PointerScope;
        let base = DeviceOverride::join_defaults();
        let cases: &[(&str, DeviceOverride, ReconfigAction)] = &[
            // (a)/(b)/(c): nothing the pipeline or the client reads.
            (
                "remote control",
                DeviceOverride {
                    control_enabled: !base.control_enabled,
                    ..base
                },
                ReconfigAction::Nothing,
            ),
            (
                "pointer scope",
                DeviceOverride {
                    pointer_scope: PointerScope::Display,
                    ..base
                },
                ReconfigAction::Nothing,
            ),
            (
                "system audio",
                DeviceOverride {
                    audio_enabled: !base.audio_enabled,
                    ..base
                },
                ReconfigAction::Nothing,
            ),
            ("nothing at all", base, ReconfigAction::Nothing),
            // (e) display scale: applied to the live display, the client is never told.
            (
                "display scale",
                DeviceOverride {
                    scale: base.scale + 25,
                    ..base
                },
                ReconfigAction::ScaleLive,
            ),
            // (d) stream parameters and the rest of (e): an encoder session is built from these.
            (
                "orientation",
                DeviceOverride {
                    orientation_portrait: !base.orientation_portrait,
                    ..base
                },
                ReconfigAction::Rebuild,
            ),
            (
                "refresh rate",
                DeviceOverride {
                    refresh_rate: base.refresh_rate + 30,
                    ..base
                },
                ReconfigAction::Rebuild,
            ),
            (
                "dpr",
                DeviceOverride {
                    dpr: base.dpr + 1.0,
                    ..base
                },
                ReconfigAction::Rebuild,
            ),
            (
                "video scale",
                DeviceOverride {
                    video_scale: 50,
                    ..base
                },
                ReconfigAction::Rebuild,
            ),
            (
                "video quality",
                DeviceOverride {
                    video_quality: base.video_quality + 5,
                    ..base
                },
                ReconfigAction::Rebuild,
            ),
            (
                "automatic frame rate",
                DeviceOverride {
                    auto_frame_rate: !base.auto_frame_rate,
                    ..base
                },
                ReconfigAction::Rebuild,
            ),
        ];
        for (what, next, want) in cases {
            assert_eq!(
                DeviceOverride::reconfig_action(&base, next),
                *want,
                "moving {what} should cost the stream {want:?}"
            );
        }
    }

    /// A rebuild must win when both moved: the display is re-driven at the new mode either way,
    /// so applying the scale live as well would be a second, redundant reconfiguration.
    #[test]
    fn a_rebuild_absorbs_a_scale_change_that_rides_with_it() {
        let base = DeviceOverride::join_defaults();
        let both = DeviceOverride {
            scale: base.scale + 25,
            refresh_rate: base.refresh_rate + 30,
            ..base
        };
        assert_eq!(
            DeviceOverride::reconfig_action(&base, &both),
            ReconfigAction::Rebuild
        );
    }

    /// The owner's own reproduction. The name is not on `DeviceOverride` at all, so a save that
    /// only moved it submits an identical row — and the session, its epochs and its display must
    /// all come out the other side untouched.
    #[test]
    fn a_rename_moves_no_epoch_and_ends_no_session() {
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let token = mint_device_token();
        let join = mint_join_id();
        open_session(&sessions, &join, &token, "10.0.0.9");
        set_live_display(
            &sessions,
            &join,
            LiveDisplay {
                display_id: 4,
                device_name: "DISPLAY7".to_string(),
                device_path: "p4".into(),
                width: 1920,
                height: 1080,
                refresh: 60,
                scale: 100,
                portrait: false,
            },
        );
        set_session_labels(&sessions, &join, "old name", "1920x1080");
        let before = reconfig_epoch(&sessions, &join);

        // What the desktop sheet submits when only the name was edited: the same row back.
        let saved = DeviceOverride::join_defaults();
        let submitted = saved;
        assert_eq!(
            DeviceOverride::reconfig_action(&saved, &submitted),
            ReconfigAction::Nothing,
            "a rename is not a stream change"
        );
        set_session_labels(&sessions, &join, "new name", "1920x1080");

        assert_eq!(
            reconfig_epoch(&sessions, &join),
            before,
            "a rename must not move the reconfig epoch"
        );
        assert_eq!(
            kick_epoch(&sessions, &join),
            0,
            "a rename must not kick the session"
        );
        assert!(
            get_live_display(&sessions, &join).is_some(),
            "a rename must not take the display down"
        );
        assert_eq!(
            joins_for_token(&sessions, &token),
            vec![join],
            "and the join it was watching on is the join it is still watching on"
        );
    }

    /// A device-wide bump reaches every tab and leaves every display alone. This is what a
    /// display-affecting change does instead of a leave-and-rejoin.
    #[test]
    fn a_stream_change_moves_every_tab_without_touching_the_display() {
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let token = mint_device_token();
        let other = mint_device_token();
        let (a, b, c) = (mint_join_id(), mint_join_id(), mint_join_id());
        open_session(&sessions, &a, &token, "10.0.0.9");
        open_session(&sessions, &b, &token, "10.0.0.9");
        open_session(&sessions, &c, &other, "10.0.0.11");
        for join in [&a, &b, &c] {
            set_live_display(
                &sessions,
                join,
                LiveDisplay {
                    display_id: 1,
                    device_name: "d".to_string(),
                    device_path: "pd".into(),
                    width: 800,
                    height: 600,
                    refresh: 60,
                    scale: 100,
                    portrait: false,
                },
            );
        }

        let moved = bump_reconfig_epoch_for_device(&sessions, &token);
        assert_eq!(moved, 2, "both of this device's tabs, and only those");
        assert_eq!(reconfig_epoch(&sessions, &a), 1);
        assert_eq!(reconfig_epoch(&sessions, &b), 1);
        assert_eq!(
            reconfig_epoch(&sessions, &c),
            0,
            "another device's session is not this device's business"
        );
        for join in [&a, &b, &c] {
            assert!(
                get_live_display(&sessions, join).is_some(),
                "an epoch bump is a renegotiation, not a teardown"
            );
            assert_eq!(kick_epoch(&sessions, join), 0);
        }
    }

    /// Class (b) reaches the injector that is running, not the next one.
    #[test]
    fn remote_control_and_the_pointer_scope_apply_to_a_live_session() {
        use crate::streamer::input::{LiveInput, PointerScope};
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let token = mint_device_token();
        let other = mint_device_token();
        let (a, b) = (mint_join_id(), mint_join_id());
        open_session(&sessions, &a, &token, "10.0.0.9");
        open_session(&sessions, &b, &other, "10.0.0.11");
        let live_a = Arc::new(LiveInput::new(true, PointerScope::All));
        let live_b = Arc::new(LiveInput::new(true, PointerScope::All));
        set_live_input(&sessions, &a, Arc::clone(&live_a));
        set_live_input(&sessions, &b, Arc::clone(&live_b));

        let reached = apply_live_input_for_device(&sessions, &token, false, PointerScope::Display);
        assert_eq!(reached, 1);
        assert!(!live_a.control_enabled());
        assert_eq!(live_a.pointer_scope(), PointerScope::Display);
        assert!(
            live_b.control_enabled(),
            "another device's injector is untouched"
        );
        assert_eq!(live_b.pointer_scope(), PointerScope::All);
        assert_eq!(
            reconfig_epoch(&sessions, &a),
            0,
            "class (b) costs no renegotiation"
        );
    }
}

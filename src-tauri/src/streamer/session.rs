use std::collections::{BTreeMap, HashMap};
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
    /// Where the device was when it joined. A label: it may change under a live session and it
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
    /// Off (the default), `refresh_rate` is a promise, not a ceiling: an unchanged frame is
    /// repeated at that cadence rather than skipped, and no controller may lower it. On, the
    /// rate may fall on a static screen or a congested link.
    ///
    /// Read by `windows_utils::streamer::pipeline` through `Config::auto_frame_rate`, which
    /// `server.rs` copies out of this at join.
    pub auto_frame_rate: bool,
    /// Quality: off (the default) codes every picture at `video_quality` as a constant QP; on,
    /// the slider is a QP *floor* under CBR and the loss controller owns the bitrate. Read by
    /// `server.rs` at join.
    pub adaptive_quality: bool,
    /// Pixel ratio: off renders at `dpr`; on (the default), at whatever ratio the device reports
    /// on each join, so a browser zoom or a different screen is followed rather than pinned.
    pub native_dpr: bool,
    /// Orientation: off honours the saved orientation for the life of a session (a physical
    /// rotation is still believed); on (the default), the display always follows the way the
    /// device is held.
    pub auto_orientation: bool,
}

/// What one settings edit costs the stream the device is already watching.
///
/// The desktop UI writes the whole override on every save, so the question is whether any field
/// the live pipeline reads moved. Each variant names a class of field, ordered by how much of the
/// session it disturbs, so a run of fields folds with `max`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReconfigAction {
    /// Classes (a), (b) and (c): the name and its labels, remote control, the pointer scope,
    /// system audio on/off and the output device. None is an encoder-session parameter (the
    /// injector and the audio fan-out read them live), so the stream is told nothing.
    #[default]
    Nothing,
    /// Display scale alone. The extended desktop's DPI changes under a stream whose geometry is
    /// already right, so the host applies it to the live display and the client is told nothing.
    ScaleLive,
    /// Classes (d) and the rest of (e): geometry, refresh rate, DPR, the encoder's input scale
    /// and its quality target. These are fixed when the encoder session is built, so the pipeline
    /// restarts at the new mode and the client renegotiates **in place**: same join id, same
    /// display, the last frame held until the new one paints.
    Rebuild,
}

impl DeviceOverride {
    /// Which class the difference between two saved overrides falls into.
    ///
    /// The whole rule lives here, and the unit test beside it is the field-to-action table. A
    /// field of `DeviceOverride` not named in this match is cosmetic by construction: changing it
    /// does nothing to the stream.
    pub fn reconfig_action(old: &Self, new: &Self) -> ReconfigAction {
        // with `auto_orientation` on, the join path believes the device and ignores the saved
        // orientation, so moving it must not rebuild the stream (a ~0.4 s gap for nothing)
        let orientation_moved =
            old.orientation_portrait != new.orientation_portrait && !new.auto_orientation;
        let rebuild = orientation_moved
            || old.refresh_rate != new.refresh_rate
            || old.video_scale != new.video_scale
            || old.video_quality != new.video_quality
            || old.auto_frame_rate != new.auto_frame_rate
            || old.adaptive_quality != new.adaptive_quality
            || old.native_dpr != new.native_dpr
            || old.auto_orientation != new.auto_orientation
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
            adaptive_quality: false,
            native_dpr: true,
            auto_orientation: true,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct AudioOutput {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct AudioOutputsReport {
    // `false` only when the device's page said its browser cannot move audio off the default
    // output: the desktop UI shows "can't switch speakers" for exactly that value. "Not reported
    // yet" is `true` with no outputs, which the UI shows as waiting for the device. (A line
    // comment, not a doc comment: specta copies doc comments into the generated `bindings.ts`.)
    pub supported: bool,
    pub outputs: Vec<AudioOutput>,
    /// What the host last asked this device to play on.
    pub selected: String,
    /// What the device reports it is *actually* playing on. The two diverge the moment the
    /// user changes the output in the browser's own UI, which only the client can see.
    /// `None` means this client never said (an older one, or one that has not posted yet),
    /// which is a different answer from `Some("")`, the device's default output.
    pub current: Option<String>,
    /// Whether the labels beside the ids above are the operating system's or this page's own.
    /// `Some(false)` means the browser withheld the real names and the client numbered them
    /// ("Output 1", "Output 2") rather than spend a microphone prompt on them, so they must
    /// not be presented as device names. `None` means the client never said.
    pub labelled: Option<bool>,
    // What each output-discovery API answered at the join, keyed by API (`selectAudioOutput`,
    // `enumerateDevices`, `getUserMedia`, ...) with `ok` / `unsupported` / `denied` / `empty` /
    // `error` / `timeout`. `None` from a page that predates it. Not part of the desktop UI's
    // type (`specta(skip)`): the sheet does not read it, `status --json` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(skip)]
    pub discovery: Option<BTreeMap<String, String>>,
}

impl Default for AudioOutputsReport {
    fn default() -> Self {
        Self {
            supported: true,
            outputs: Vec::new(),
            selected: String::new(),
            current: None,
            labelled: None,
            discovery: None,
        }
    }
}

pub trait DeviceReporter: Send + Sync + std::fmt::Debug {
    fn report_join(&self, device: DeviceInfo);
    /// The *same* join, offering again after a display change.
    ///
    /// A renegotiation is not an arrival: the row is already on the dashboard and the display is
    /// still up. Reported as a join, it raises "a device joined" every time a refresh rate or a
    /// DPR moves. The default forwards, for a reporter that does not care about the distinction.
    fn report_modify(&self, device: DeviceInfo) {
        self.report_join(device);
    }
    /// One join ended. The join id is the identity: the device may still hold other joins, and
    /// its IP is only ever a label.
    fn report_remove(&self, join_id: String, token: String);
    fn report_join_attempts_paused(&self, _retry_after_secs: u64) {}
    fn report_audio_outputs(&self, _join_id: String, _token: String, _report: AudioOutputsReport) {}
    /// The link governor has held this join's stream down for `engaged_secs` of the last
    /// `window_secs`: its network cannot carry the stream.
    #[cfg(windows)]
    fn report_slow_connection(&self, _device: DeviceInfo, _engaged_secs: u32, _window_secs: u32) {}
}

pub type SharedDeviceReporter = Arc<dyn DeviceReporter>;

/// Per-device settings, keyed by the device **token**, never the IP.
///
/// An address is recycled: a phone that leaves and a laptop that picks up its DHCP lease are the
/// same string and a different device. The token is minted once per device on its first
/// successful OTP join and presented on every join after that, so it survives a roam, a lease
/// renewal and a NAT rebind.
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

/// The identity of one live session (one browser tab), minted by the host at `POST /whep` and
/// handed back as the WHEP resource id.
///
/// 128 bits from the same source as the device token, because it is the capability that
/// `PATCH`/`DELETE /whep/{id}`, `/reconfig`, `/leave` and `/audio-outputs` are addressed by. It
/// carries nothing about the peer: an address moves under a session and is recycled between
/// devices, so it can be a label and never a name.
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
/// By token only, never by IP: the address a banned phone had this morning is the address the
/// owner's laptop gets this afternoon. A device that has never completed an OTP join has no
/// token, so there is nothing to ban it by.
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
/// The join path mints a fresh token on every successful OTP, which is wrong for a device the
/// host already knows: per-device settings are keyed by the token, so re-minting orphans the row
/// just written and a reconfigure comes back with the *defaults*. Under `serve` nothing else
/// vouches for the device, because only the desktop UI writes an approval.
///
/// Continuity is granted only for a token the host itself issued and still has state for, never
/// for an arbitrary string a client offers: the trust maps are keyed by this value, and a device
/// that could choose its own name could choose one that is already approved.
pub fn is_known_device(overrides: &SharedDeviceOverrides, token: &str) -> bool {
    is_device_token(token) && overrides.lock_recover().contains_key(token)
}

/// Devices whose override row the join path wrote and nobody has saved since.
///
/// The join path seeds a row for a device the host holds no settings for, so the sheet's first
/// save has something to compare against (`DeviceOverride::reconfig_action`). The refresh rate in
/// a seeded row is what the client reported *at that join*: a detection, not a setting, until a
/// save makes it one. Read back as a setting, it pins a device that first joined at 60 Hz to 60
/// whatever it reports afterwards.
static JOIN_SEEDED: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

pub fn mark_join_seeded(token: &str) {
    JOIN_SEEDED
        .lock_recover()
        .get_or_insert_with(Default::default)
        .insert(token.to_string());
}

/// A save makes every value in the row the user's own.
pub fn clear_join_seeded(token: &str) {
    if let Some(set) = JOIN_SEEDED.lock_recover().as_mut() {
        set.remove(token);
    }
}

pub fn is_join_seeded(token: &str) -> bool {
    JOIN_SEEDED
        .lock_recover()
        .as_ref()
        .is_some_and(|set| set.contains(token))
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

pub const DEFAULT_DISCONNECT_GRACE_SECS: u64 = 3;
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
/// A same-subnet peer is offered none. This is a setting so that a cross-network or unclassified
/// join can be pointed at a private server, or at nothing, instead of the default Google endpoint.
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
    /// next display to attach, so a name recorded when this display was created can address a
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
/// The map this lives in is keyed by the **join id**, so two tabs of one browser (same device,
/// same token, same address) are two entries with two displays, and neither can reach the
/// other's. `device_token` is what every per-device lookup (settings, trust, the audio sink) is
/// keyed by. `ip` is a label for the UI and the logs: it is refreshed from whichever request
/// arrives last, and nothing is ever addressed by it.
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
    /// `None` until this join's page has said. Only `Some(false)` means "this browser cannot
    /// switch speakers"; a join that never reported (a relayed client, whose relay does not
    /// tunnel `/audio-outputs`, or one that has not posted yet) is unknown, not unsupported.
    pub audio_outputs_supported: Option<bool>,
    pub current_audio_output: Option<String>,
    pub audio_outputs_labelled: Option<bool>,
    pub audio_discovery: Option<BTreeMap<String, String>>,
    /// The page load this join came from (the client's `attemptId`), when it sent one.
    pub attempt_id: String,
    /// Whether any picture has ever reached this join's client. A join that never connected
    /// is the one a lost answer leaves behind, and the only kind an `attemptId` may take over.
    pub ever_connected: bool,
    /// Whether the dashboard has been told this join arrived. A retry that adopts a join whose
    /// first attempt never got that far reports the arrival, not a change.
    pub join_reported: bool,
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
/// Every write goes through this, never a bare `entry().or_default()`, so a state a settings
/// command creates before the client connects still carries the device it belongs to.
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

/// The join a relay client is streaming on right now.
///
/// A relayed page that predates join ids names nothing, so the relay's `client_id` is all there
/// is, and one client can hold two joins: the one it watches, and after a renegotiation an older
/// one waiting out its disconnect grace. The page would read the dead join's epoch as another
/// reconfigure, so prefer the join whose capture is running, then one with a display, then any.
pub fn live_join_for_relay_client(sessions: &SharedSessions, client_id: &str) -> Option<String> {
    let map = sessions.lock_recover();
    let mine = || {
        map.iter()
            .filter(move |(_, s)| s.relay_client.as_deref() == Some(client_id))
    };
    mine()
        .find(|(_, s)| s.active_capture.is_some())
        .or_else(|| mine().find(|(_, s)| s.live_display.is_some()))
        .or_else(|| mine().next())
        .map(|(join, _)| join.clone())
}

/// Record the page load (`attemptId`) a join was opened from.
pub fn set_session_attempt(sessions: &SharedSessions, join: &str, attempt: &str) {
    if attempt.is_empty() {
        return;
    }
    if let Some(state) = sessions.lock_recover().get_mut(join) {
        state.attempt_id = attempt.to_string();
    }
}

/// A picture reached this join's client; from now on no `attemptId` may take it over.
pub fn mark_session_connected(sessions: &SharedSessions, join: &str) {
    if let Some(state) = sessions.lock_recover().get_mut(join) {
        state.ever_connected = true;
    }
}

/// The join a page load opened and never received, as `(join, device_token)`.
///
/// The lost-answer case: the host answered, the page never saw the answer, gave up after its
/// timeout and offered again from the same page load with no join id and (on a first join) no
/// device token. Adopting the join keeps the retry from being a second session and a second
/// display. Only a join that **never connected** and already has its display qualifies, and only
/// for the same device when the retry presents a token at all: an attempt id can never reach a
/// session somebody is watching, or another device's.
pub fn pending_join_for_attempt(
    sessions: &SharedSessions,
    attempt: &str,
    presented_token: &str,
) -> Option<(String, String)> {
    if !is_device_token(attempt) {
        return None;
    }
    sessions
        .lock_recover()
        .iter()
        .find(|(_, s)| {
            s.attempt_id == attempt
                && !s.ever_connected
                && s.live_display.is_some()
                && !s.device_token.is_empty()
                && (presented_token.is_empty() || presented_token == s.device_token)
        })
        .map(|(join, s)| (join.clone(), s.device_token.clone()))
}

/// Whether a join opened from this page load is still being built: same attempt, never
/// connected, no display recorded yet, and (when the retry presents one) the same device.
///
/// A retry that arrives while the first attempt is still creating its display has to wait for
/// it rather than start a second display beside it.
pub fn attempt_in_flight(sessions: &SharedSessions, attempt: &str, presented_token: &str) -> bool {
    if !is_device_token(attempt) {
        return false;
    }
    sessions.lock_recover().values().any(|s| {
        s.attempt_id == attempt
            && !s.ever_connected
            && s.live_display.is_none()
            && (presented_token.is_empty()
                || s.device_token.is_empty()
                || presented_token == s.device_token)
    })
}

/// Bump one join's reconfig epoch only while `seq` is still its current attempt. Never creates
/// an entry: a session removed meanwhile stays removed.
pub fn bump_reconfig_epoch_if_current(sessions: &SharedSessions, join: &str, seq: u64) -> bool {
    match sessions.lock_recover().get_mut(join) {
        Some(state) if state.session_seq == seq => {
            state.reconfig_epoch += 1;
            true
        }
        _ => false,
    }
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
/// Keyed by join, the map gains an entry for every tab ever opened, so each has to be removed.
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

/// Register a join's control channel and leave signal, only while `seq` still owns it, in the one
/// lock that checks it: an attempt overtaken in between cannot overwrite the newer attempt's.
/// `None` when it no longer owns the join.
pub fn register_attempt(
    sessions: &SharedSessions,
    join: &str,
    seq: u64,
    tx: ControlSender,
) -> Option<Arc<LeaveSignal>> {
    let mut map = sessions.lock_recover();
    let state = map.get_mut(join).filter(|s| s.session_seq == seq)?;
    let signal = Arc::new(LeaveSignal::default());
    state.leave = Some(signal.clone());
    state.control = Some(tx);
    Some(signal)
}

/// Whether this join's arrival has yet to be reported, marking it reported.
pub fn take_join_report(sessions: &SharedSessions, join: &str) -> bool {
    match sessions.lock_recover().get_mut(join) {
        Some(state) => !std::mem::replace(&mut state.join_reported, true),
        None => true,
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

/// Bump every live join of one device: a per-device settings change, and a device can hold more
/// than one join.
pub fn bump_reconfig_epoch_for_device(sessions: &SharedSessions, token: &str) -> usize {
    let joins = joins_for_token(sessions, token);
    for join in &joins {
        bump_reconfig_epoch(sessions, join);
    }
    joins.len()
}

/// Bump every live join on the host: a host-wide setting a session reads only when it is built.
pub fn bump_reconfig_epoch_all(sessions: &SharedSessions) -> usize {
    let mut map = sessions.lock_recover();
    let mut moved = 0usize;
    for state in map.values_mut() {
        if state.live_display.is_some() {
            state.reconfig_epoch += 1;
            moved += 1;
        }
    }
    moved
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
    supported: Option<bool>,
    outputs: Vec<AudioOutput>,
    current: Option<String>,
    labelled: Option<bool>,
    discovery: Option<BTreeMap<String, String>>,
) {
    let mut map = sessions.lock_recover();
    let entry = map.entry(join.to_string()).or_default();
    // a body without the field says nothing about the browser: it must not turn a join that
    // did say into "cannot switch"
    if supported.is_some() {
        entry.audio_outputs_supported = supported;
    }
    entry.audio_outputs = outputs;
    // a client that does not report one must not erase what a newer one did
    if current.is_some() {
        entry.current_audio_output = current;
    }
    if labelled.is_some() {
        entry.audio_outputs_labelled = labelled;
    }
    if discovery.is_some() {
        entry.audio_discovery = discovery;
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
        supported: s.audio_outputs_supported.unwrap_or(true),
        outputs: s.audio_outputs.clone(),
        selected: String::new(),
        current: s.current_audio_output.clone(),
        labelled: s.audio_outputs_labelled,
        discovery: s.audio_discovery.clone(),
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
/// good as any; a join that never reported ranks below one that did, so it cannot mask it. With
/// no join at all (the device is offline, or every join is relayed) the answer is "not reported"
/// (`supported: true` with no outputs), never "cannot switch".
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
                supported: s.audio_outputs_supported.unwrap_or(true),
                outputs: s.audio_outputs.clone(),
                selected: String::new(),
                current: s.current_audio_output.clone(),
                labelled: s.audio_outputs_labelled,
                discovery: s.audio_discovery.clone(),
            })
    };
    AudioOutputsReport {
        selected: device_audio_sink(sinks, token).unwrap_or_default(),
        ..best.unwrap_or_default()
    }
}

impl DeviceSessionState {
    /// A join that reported a usable browser, then one that reported an unusable one, then one
    /// that never said.
    fn supported_rank(&self) -> u8 {
        match self.audio_outputs_supported {
            Some(true) => 2,
            Some(false) => 1,
            None => 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionAuth {
    pub session_id: Arc<Mutex<String>>,
    pub otp: Arc<Mutex<String>>,
}

impl SessionAuth {
    /// The live pair, or the permanent one when the user has made one.
    pub fn validate(&self, session_id: &str, otp: &str) -> bool {
        let live = {
            let want_session = self.session_id.lock_recover();
            let want_otp = self.otp.lock_recover();
            pair_matches(&want_session, &want_otp, session_id, otp)
        };
        // both compared every time, so the answer takes as long whichever pair matched
        let permanent = permanent_link_matches(session_id, otp);
        live | permanent
    }
}

fn pair_matches(want_session: &str, want_otp: &str, session_id: &str, otp: &str) -> bool {
    use subtle::ConstantTimeEq;
    if want_session.is_empty() || want_otp.is_empty() {
        return false;
    }
    let session_ok = want_session.as_bytes().ct_eq(session_id.as_bytes());
    let otp_ok = want_otp.as_bytes().ct_eq(otp.as_bytes());
    (session_ok & otp_ok).into()
}

/// A session id and OTP that do not rotate, for a link a device can keep. Accepted beside the
/// live pair, never instead of it. Process-wide like the host itself; None when turned off.
static PERMANENT_LINK: Mutex<Option<(String, String)>> = Mutex::new(None);

fn permanent_link_matches(session_id: &str, otp: &str) -> bool {
    match PERMANENT_LINK.lock_recover().as_ref() {
        Some((want_session, want_otp)) => pair_matches(want_session, want_otp, session_id, otp),
        None => pair_matches("", "", session_id, otp),
    }
}

/// The client page's join form takes exactly this: a 12-character id from the minters'
/// alphabet and a 6-digit OTP. A pair it cannot type is a link that cannot connect.
pub fn validate_permanent_link(session_id: &str, otp: &str) -> Result<(), String> {
    const ALPHABET: &str = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    if session_id.len() != 12 || !session_id.chars().all(|c| ALPHABET.contains(c)) {
        return Err("a permanent session id is 12 characters from the session id alphabet".into());
    }
    if otp.len() != 6 || !otp.chars().all(|c| c.is_ascii_digit()) {
        return Err("a permanent OTP is 6 digits".into());
    }
    Ok(())
}

/// Turns the permanent link on with this pair, or off with `None`. Takes effect for the next
/// join; a revoked pair is refused at once.
pub fn apply_permanent_link(pair: Option<(String, String)>) -> Result<(), String> {
    if let Some((session_id, otp)) = &pair {
        validate_permanent_link(session_id, otp)?;
    }
    let on = pair.is_some();
    *PERMANENT_LINK.lock_recover() = pair;
    tprintln!("[session] permanent link {}", if on { "on" } else { "off" });
    Ok(())
}

/// Settings > Permanent link. `None` for either turns it off.
#[tauri::command]
#[specta::specta]
pub fn set_permanent_link(session_id: Option<String>, otp: Option<String>) -> Result<(), String> {
    apply_permanent_link(session_id.zip(otp))
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
    /// `None` (the default) means "ask the OS": the server follows the create with a forced
    /// `set_display_mode` and a settle poll. That is unavoidable on a backend that is *commanded*
    /// (the Windows IDD registers a mode list and the OS picks from it), but wrong on one that
    /// *negotiates*: the macOS AirPlay tier has already picked the closest mode from the ladder
    /// macOS published, and a forced set fails whenever the requested size is off that ladder.
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

/// The one place the display cap is read. Only the macOS AirPlay tier is capped (macOS owns that
/// display and allows one); every other backend is uncapped, never evicts a connected device and
/// never serves `at-capacity.html`.
///
/// Only `Some(1)` is recognised. Any other finite number is a bug in the backend, so it is
/// logged once and treated as uncapped.
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

/// [`concurrent_display_cap`] for the shared handle the host carries.
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
/// reads the label each session carries, not the map's keys, which are join ids.
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
    /// What the join's page last posted to `/audio-outputs`, for `status --json`.
    pub audio_outputs_supported: Option<bool>,
    pub audio_outputs: usize,
    pub audio_outputs_labelled: Option<bool>,
    pub audio_discovery: Option<BTreeMap<String, String>>,
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
            audio_outputs_supported: s.audio_outputs_supported,
            audio_outputs: s.audio_outputs.len(),
            audio_outputs_labelled: s.audio_outputs_labelled,
            audio_discovery: s.audio_discovery.clone(),
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

    /// "This device's browser can't switch speakers" is the device sheet's reading of
    /// `supported == false`, so only a page that said so may produce it. An offline device, a
    /// join that has not posted, a relayed join (the relay does not tunnel `/audio-outputs`)
    /// and a body without the field are all "not reported": `true` with no outputs.
    #[test]
    fn only_a_reported_false_reads_as_cannot_switch() {
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        let sinks = new_shared_audio_sinks();
        let token = "tok-a";
        let none = |j: &str| audio_outputs_report(&sessions, &sinks, j);

        // nothing connected at all
        assert!(AudioOutputsReport::default().supported);
        assert!(device_audio_outputs_report(&sessions, &sinks, token).supported);
        assert!(none("missing").supported);

        // a join that exists and has never posted
        {
            let mut map = sessions.lock_recover();
            map.entry("j1".into()).or_default().device_token = token.into();
            map.entry("j2".into()).or_default().device_token = token.into();
        }
        assert!(none("j1").supported);
        let r = device_audio_outputs_report(&sessions, &sinks, token);
        assert!(r.supported && r.outputs.is_empty());

        // a body without the field leaves it unsaid
        set_audio_outputs(&sessions, "j1", None, vec![], None, None, None);
        assert!(none("j1").supported);

        // a page that says its browser cannot route: the one legitimate `false`, and it outranks
        // a sibling join that never said
        set_audio_outputs(&sessions, "j1", Some(false), vec![], None, None, None);
        assert!(!none("j1").supported);
        assert!(!device_audio_outputs_report(&sessions, &sinks, token).supported);
        // and a later body without the field does not erase it
        set_audio_outputs(&sessions, "j1", None, vec![], None, None, None);
        assert!(!none("j1").supported);

        // a join that reported a usable browser wins over one that reported none
        let speakers = AudioOutput {
            id: "abc".into(),
            label: "Speakers (Boom Audio)".into(),
        };
        set_audio_outputs(
            &sessions,
            "j2",
            Some(true),
            vec![speakers],
            None,
            None,
            None,
        );
        let r = device_audio_outputs_report(&sessions, &sinks, token);
        assert!(r.supported);
        assert_eq!(r.outputs.len(), 1);

        // a later post from the same join replaces an earlier one, in either direction
        set_audio_outputs(&sessions, "j1", Some(true), vec![], None, None, None);
        assert!(none("j1").supported);

        // the per-API discovery results ride along, and a body without them keeps the last
        let mut d = BTreeMap::new();
        d.insert("enumerateDevices".to_string(), "empty".to_string());
        set_audio_outputs(&sessions, "j1", Some(true), vec![], None, None, Some(d));
        set_audio_outputs(&sessions, "j1", Some(true), vec![], None, None, None);
        let r = none("j1");
        assert_eq!(
            r.discovery
                .as_ref()
                .and_then(|m| m.get("enumerateDevices"))
                .map(String::as_str),
            Some("empty")
        );
        // the desktop UI's type does not carry it, and an absent map is not serialised
        let v = serde_json::to_value(AudioOutputsReport::default()).unwrap();
        assert!(v.get("discovery").is_none());
    }

    #[test]
    fn a_seeded_refresh_rate_is_a_detection_until_saved() {
        let token = mint_device_token();
        assert!(!is_join_seeded(&token));
        mark_join_seeded(&token);
        assert!(is_join_seeded(&token));
        clear_join_seeded(&token);
        assert!(!is_join_seeded(&token));
    }

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
    fn a_permanent_pair_is_checked_like_the_live_one() {
        assert!(pair_matches(
            "ABCDEFGHJKLM",
            "123456",
            "ABCDEFGHJKLM",
            "123456"
        ));
        assert!(!pair_matches(
            "ABCDEFGHJKLM",
            "123456",
            "ABCDEFGHJKLM",
            "123457"
        ));
        assert!(!pair_matches(
            "ABCDEFGHJKLM",
            "123456",
            "ZBCDEFGHJKLM",
            "123456"
        ));
        // a turned-off link has no pair, and no input can match an empty one
        assert!(!pair_matches("", "", "", ""));
    }

    #[test]
    fn either_pair_joins_and_a_revoked_permanent_pair_does_not() {
        // a pair no other test uses, since the permanent link is process-wide
        let (pid, potp) = ("PQRSTUVWXYZP".to_string(), "975310".to_string());
        let auth = SessionAuth::default();
        *auth.session_id.lock_recover() = "ABCDEFGHJKLM".to_string();
        *auth.otp.lock_recover() = "123456".to_string();
        assert!(!auth.validate(&pid, &potp));
        apply_permanent_link(Some((pid.clone(), potp.clone()))).unwrap();
        assert!(auth.validate(&pid, &potp), "the permanent pair joins");
        assert!(
            auth.validate("ABCDEFGHJKLM", "123456"),
            "the live pair still joins"
        );
        assert!(!auth.validate(&pid, "123456"), "pairs do not mix");
        assert!(!auth.validate("ABCDEFGHJKLM", &potp), "pairs do not mix");
        apply_permanent_link(None).unwrap();
        assert!(
            !auth.validate(&pid, &potp),
            "a revoked pair is refused at once"
        );
        assert!(auth.validate("ABCDEFGHJKLM", "123456"));
        assert!(apply_permanent_link(Some(("short".into(), "1".into()))).is_err());
    }

    #[test]
    fn a_permanent_pair_must_be_one_the_join_form_can_type() {
        assert!(validate_permanent_link("23456789ABCD", "012345").is_ok());
        assert!(validate_permanent_link("23456789ABC", "012345").is_err()); // short id
        assert!(validate_permanent_link("23456789ABC1", "012345").is_err()); // 1 is not in the alphabet
        assert!(validate_permanent_link("23456789abcd", "012345").is_err()); // lower case
        assert!(validate_permanent_link("23456789ABCD", "01234a").is_err());
        assert!(validate_permanent_link("23456789ABCD", "0123456").is_err());
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

    #[test]
    fn a_minted_device_token_is_128_bits_of_lowercase_hex() {
        // the token is the whole device-trust key: a short or predictable one is an auto-join
        // for anyone who guesses it
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
        // an empty token matching an empty entry would hand every unknown device an auto-join
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
        // an IP ban would lock out whichever device picks up the banned one's lease next
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

    #[test]
    fn an_unknown_join_reads_epoch_zero_rather_than_failing() {
        // the client adopts whatever the first `/reconfig` returns, so a join the host has no
        // state for must read a stable 0
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        assert_eq!(reconfig_epoch(&sessions, "join-a"), 0);
        assert_eq!(kick_epoch(&sessions, "join-a"), 0);
        assert_eq!(reconfig_epoch(&sessions, "join-a"), 0);
    }

    #[test]
    fn each_bump_is_visible_exactly_once_and_only_to_that_join() {
        // a bump that moved two joins would rebuild a session nobody changed, and one that
        // never landed leaves a tab on the old settings forever
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
        // a settings change must not read as a kick: the client tears the session down and shows
        // "you have been removed" on a kick, and silently rebuilds on a reconfig
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

    #[test]
    fn a_device_that_inherits_a_disconnected_devices_address_inherits_nothing_else() {
        // a phone leaves and the router hands its address to a laptop: keyed by address, the
        // laptop would walk into the phone's session and its settings
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

        remove_session(&sessions, &phone_join);

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
        // a roam or a DHCP renewal moves the peer: the session is named by its join id, so the
        // only thing that changes is the label the dashboard shows
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
        // the CLI convenience lookup: one device there resolves, two do not, because an address
        // is a place and the answer would be a coin toss
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
        // settings can be changed before the device has ever connected (the CLI's `devices set`
        // writes through the same path), and a bump that landed nowhere would be silently lost
        let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
        bump_kick_epoch(&sessions, "join-7");
        assert!(sessions.lock().unwrap().contains_key("join-7"));
        assert_eq!(kick_epoch(&sessions, "join-7"), 1);
    }

    #[test]
    fn a_stale_session_never_stops_the_capture_that_replaced_it() {
        // a reconnect on the same join id mints a new sequence, and the old session's teardown
        // must not stop the capture the new one just started: hence the conditional take
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
        // keyed by anything coarser than the join id, the second tab would replace the first
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

        // one tab closes, the other keeps its display
        let _ = take_live_display(&sessions, &a);
        remove_session(&sessions, &a);
        assert_eq!(live_display_count(&sessions), 1);
        assert_eq!(get_live_display(&sessions, &b).unwrap().display_id, 5);
    }

    #[test]
    fn the_disconnect_grace_default_sits_inside_its_own_range() {
        // the UI and the CLI both clamp to this range, so a default outside it would be
        // unreachable from either; compared through a runtime binding so clippy sees a
        // comparison rather than a constant it can fold
        let (lo, hi) = (MIN_DISCONNECT_GRACE_SECS, MAX_DISCONNECT_GRACE_SECS);
        assert!((lo..=hi).contains(&DEFAULT_DISCONNECT_GRACE_SECS));
        let grace = new_shared_disconnect_grace();
        assert_eq!(grace.load(Ordering::Relaxed), DEFAULT_DISCONNECT_GRACE_SECS);
    }

    #[test]
    fn an_overtaken_attempt_cannot_register_over_the_newer_one() {
        let sessions: SharedSessions = Default::default();
        let join = mint_join_id();
        open_session(&sessions, &join, "tok", "10.0.0.5");
        let first = next_session_seq(&sessions, &join);
        let second = next_session_seq(&sessions, &join);
        let (newer, _rx) = tokio::sync::mpsc::unbounded_channel();
        let newer_leave = register_attempt(&sessions, &join, second, newer.clone()).expect("owner");
        let (older, _rx2) = tokio::sync::mpsc::unbounded_channel();
        assert!(register_attempt(&sessions, &join, first, older).is_none());
        let map = sessions.lock_recover();
        let state = map.get(&join).unwrap();
        assert!(state.control.as_ref().unwrap().same_channel(&newer));
        assert!(Arc::ptr_eq(state.leave.as_ref().unwrap(), &newer_leave));
        drop(map);
        assert!(register_attempt(&sessions, &mint_join_id(), 1, newer).is_none());
        assert_eq!(
            sessions.lock_recover().len(),
            1,
            "no entry made for an unknown join"
        );
    }

    #[test]
    fn a_join_is_reported_as_arriving_once() {
        let sessions: SharedSessions = Default::default();
        let join = mint_join_id();
        open_session(&sessions, &join, "tok", "10.0.0.5");
        assert!(
            take_join_report(&sessions, &join),
            "the first report is the arrival"
        );
        assert!(
            !take_join_report(&sessions, &join),
            "every later one is a change"
        );
        let other = mint_join_id();
        open_session(&sessions, &other, "tok", "10.0.0.5");
        assert!(take_join_report(&sessions, &other));
    }

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

    /// The reconfigure a settings change triggers must not rename the device: if the OTP path
    /// mints a token for the rejoin, the override just written belongs to nobody and the display
    /// comes back at the settings the user had just changed away from.
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

        // even present in the map, a string this host could not have minted is refused: the
        // parse runs first, so a row planted by a corrupt config cannot become an identity
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

    /// The field-to-action table, as one test: move exactly one field, and the verdict is exactly
    /// its class. The desktop UI writes the *whole* override on every save, so "a save happened"
    /// must never be read as "something changed".
    #[test]
    fn each_field_costs_the_stream_what_its_class_costs() {
        use crate::streamer::input::PointerScope;
        let base = DeviceOverride::join_defaults();
        let cases: &[(&str, DeviceOverride, ReconfigAction)] = &[
            // (a)/(b)/(c): nothing the pipeline or the client reads
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
            // (e) display scale: applied to the live display, the client is never told
            (
                "display scale",
                DeviceOverride {
                    scale: base.scale + 25,
                    ..base
                },
                ReconfigAction::ScaleLive,
            ),
            // (d) stream parameters and the rest of (e): an encoder session is built from these
            // (orientation only while the device is not followed)
            (
                "orientation (auto-orientation on)",
                DeviceOverride {
                    orientation_portrait: !base.orientation_portrait,
                    ..base
                },
                ReconfigAction::Nothing,
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
            (
                "adaptive quality",
                DeviceOverride {
                    adaptive_quality: !base.adaptive_quality,
                    ..base
                },
                ReconfigAction::Rebuild,
            ),
            (
                "native pixel ratio",
                DeviceOverride {
                    native_dpr: !base.native_dpr,
                    ..base
                },
                ReconfigAction::Rebuild,
            ),
            (
                "automatic orientation",
                DeviceOverride {
                    auto_orientation: !base.auto_orientation,
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
        // with the device not followed, the saved orientation is what the display is built at
        let fixed = DeviceOverride {
            auto_orientation: false,
            ..base
        };
        for portrait in [true, false] {
            let next = DeviceOverride {
                orientation_portrait: portrait,
                ..fixed
            };
            let old = DeviceOverride {
                orientation_portrait: !portrait,
                ..fixed
            };
            assert_eq!(
                DeviceOverride::reconfig_action(&old, &next),
                ReconfigAction::Rebuild,
                "a fixed orientation moved to portrait={portrait}"
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

    /// The name is not on `DeviceOverride` at all, so a save that only moved it submits an
    /// identical row, and the session, its epochs and its display must all come out untouched.
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

        // what the desktop sheet submits when only the name was edited: the same row back
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

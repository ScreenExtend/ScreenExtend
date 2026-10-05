//use rand::Rng;
use serde::{Deserialize, Serialize};
use specta::Type;
#[cfg(debug_assertions)]
use specta_typescript::Typescript;
use tauri::path::BaseDirectory;
//use tauri::Emitter;
use tauri::Manager;
use tauri_plugin_cli::CliExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_shell::ShellExt;
use tauri_specta::{collect_commands, collect_events, Builder, Event};

#[macro_use]
mod logbus;
mod cli;
mod data_dir;
mod exit_reason;
mod single_instance;
mod streamer;
#[cfg(test)]
mod test_support;

#[cfg(target_os = "windows")]
mod windows_utils;
#[cfg(target_os = "windows")]
use windows_utils::*;

#[cfg(target_os = "macos")]
mod macos_utils;
#[cfg(target_os = "macos")]
use macos_utils::*;

#[cfg(target_os = "linux")]
mod linux_utils;
#[cfg(target_os = "linux")]
use linux_utils::*;

pub use streamer::session::{AudioOutput, AudioOutputsReport};

pub const BYE_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DeviceAudioOutputs {
    /// The join that reported this list. One browser tab.
    #[serde(rename = "joinId")]
    pub join_id: String,
    /// The device the join belongs to, and what the UI keys its row by.
    pub token: String,
    pub supported: bool,
    pub outputs: Vec<AudioOutput>,
    /// What the host last asked this device to play on.
    pub selected: String,
    /// What the device says it is actually playing on right now. `None` when it never said,
    /// which is not the same as `Some("")` — the device's default output.
    pub current: Option<String>,
    /// `Some(false)` when the labels beside the ids are the client page's own numbering rather
    /// than the operating system's names; `None` when the client never said.
    pub labelled: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DeviceJoin(Device);

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DeviceModify(Device);

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DeviceRemove(Device);

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct NetworkChange;

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct HostedNetworkNoPassword;

/// macOS revoked Screen Recording while a session was live. The watchdog in
/// `macos_utils::start_session_watchdog` ends the sessions on a definite
/// `Some(true) -> Some(false)` edge; this is the half that tells the person at the host,
/// who would otherwise only find it in the log. Windows and Linux never emit it.
#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct ScreenRecordingRevoked {
    /// The permission key the UI should re-check, matching `PermissionStatus.key`.
    pub key: String,
    /// How many live sessions were ended because of it.
    pub sessions_ended: u32,
}

/// Everything the UI can say about the hosted network without opening the log terminal.
///
/// `state` is the tri-state both platforms already collect and neither could report:
/// - `notStarted` — nothing is being advertised.
/// - `advertised` — the platform accepted the start, and that is all that is known. An AP
///   in this state is visible to a phone and may still be unjoinable, because nothing is
///   serving DHCP.
/// - `verified` — the AP interface holds a routable IPv4, i.e. something is serving
///   addresses, so a client that associates can actually get on.
///
/// `secured` is what item 6 of the UI recommendations needs: on macOS the HostAP SPI can
/// refuse every secured `securityType` and the fallback brings up an *unencrypted* AP,
/// which stays up for the session while the join URL, session id and OTP are readable by
/// anyone in range who associates. A toast cannot carry that; a badge on the card can.
#[derive(Serialize, Deserialize, Debug, Clone, Default, Type)]
pub struct HostedNetworkReport {
    pub state: String,
    pub backend: Option<String>,
    pub ssid: String,
    pub channel: Option<i32>,
    pub band: Option<String>,
    /// The security type as it was logged, e.g. `WPA2` or `securityType 2 (open — no
    /// encryption)`. `None` when nothing is up.
    pub security: Option<String>,
    /// False means the network carries no encryption at all.
    pub secured: bool,
    pub host_ipv4: Option<String>,
    /// A client seen on the AP's own subnet. macOS only; `None` elsewhere.
    pub peer: Option<String>,
}

impl HostedNetworkReport {
    pub const NOT_STARTED: &'static str = "notStarted";
    pub const ADVERTISED: &'static str = "advertised";
    pub const VERIFIED: &'static str = "verified";
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct CloudStatusChange {
    pub state: String,
    pub detail: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct PermissionStatus {
    pub key: String,
    pub name: String,
    pub description: String,
    /// `granted` | `denied` | `unknown`. Tri-state on purpose: a probe that could
    /// not learn the answer must not be reported as a refusal, because the caller
    /// treats a refusal as a reason not to start.
    pub state: String,
    /// What the probe actually concluded, in the user's words.
    pub detail: String,
    pub required: bool,
    /// macOS binds some grants to the process that asked for them, so a permission
    /// the user has just ticked can only reach a freshly launched copy.
    pub relaunch_required: bool,
}

/// The host's join credentials changed under the desktop UI, which mirrors both values.
///
/// Two things emit it: the cloud relay rotating an id it found taken, and `ScreenExtend session
/// new` replacing the pair through the single-instance control socket. An **empty `otp` means
/// the OTP did not change** — the rotation case — and the frontend must leave its copy alone
/// rather than clearing it. Without the field the UI would show the new id beside the old code
/// and then push that stale pair back through `set_session_credentials`, undoing the half of the
/// change it could not see.
#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct SessionIdChange {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub otp: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct JoinAttemptsPaused {
    #[serde(rename = "retryAfterSecs")]
    pub retry_after_secs: u32,
}

/// The severities a capability note can carry.
///
/// `blocking` is the only one that takes "Continue anyway" off the startup gate, so it is
/// reserved for the **bare minimum**: a host that cannot create a virtual display, cannot
/// capture, or cannot encode at all. Everything a user could plausibly live without — the
/// hosted network, any audio tier, remote input, a hardware GPU, the preferred one of two
/// working capture backends — is `optional`. `installable` is a row the app can fix by itself;
/// the bootstrap remediates it and re-probes before deciding anything.
pub const SEVERITY_BLOCKING: &str = "blocking";
pub const SEVERITY_INSTALLABLE: &str = "installable";
pub const SEVERITY_OPTIONAL: &str = "optional";

/// Stable ids for the rows the frontend and `doctor` branch on. The prose beside them is for
/// humans and may be reworded at any time; these may not.
pub mod api_key {
    /// No capture backend at all.
    pub const CAPTURE: &str = "capture";
    /// The preferred capture backend is missing but a working fallback was probed.
    pub const CAPTURE_PREFERRED: &str = "capture-preferred";
    /// No way to create a display for a joining device.
    pub const DISPLAY: &str = "display";
    /// No encoder at all — not even the software one.
    pub const ENCODER: &str = "encoder";
    /// The virtual display driver is not installed. Installable.
    pub const DISPLAY_DRIVER: &str = "display-driver";
    /// The virtual display driver is installed but not answering. Installable (reinstall).
    pub const DISPLAY_DRIVER_UNRESPONSIVE: &str = "display-driver-unresponsive";
    /// The virtual display driver is installed and healthy, and another process of ours is
    /// already using it. Deliberately **not** installable: there is nothing to install, and
    /// reinstalling a driver whose pipe is merely occupied is what produced the loop of
    /// elevation prompts this key exists to end.
    pub const DISPLAY_DRIVER_BUSY: &str = "display-driver-busy";
    /// Displays work, but this backend can only drive one at a time.
    pub const DISPLAY_CAPPED: &str = "display-capped";
    pub const GPU_ADAPTER: &str = "gpu-adapter";
    pub const AUDIO: &str = "audio";
    pub const WIFI_RADIOS: &str = "wifi-radios";
    pub const HOSTED_NETWORK: &str = "hosted-network";
    pub const WMI: &str = "wmi";
    pub const INPUT_ACCESSIBILITY: &str = "input-accessibility";
}

/// The three capabilities that make a host usable at all: it must be able to create a display,
/// capture it, and encode the result. Missing any one of them means there is nothing to offer a
/// device that joins, and only those three may produce a `blocking` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BareMinimum {
    Display,
    Capture,
    Encoder,
}

impl BareMinimum {
    pub fn key(self) -> &'static str {
        match self {
            Self::Display => api_key::DISPLAY,
            Self::Capture => api_key::CAPTURE,
            Self::Encoder => api_key::ENCODER,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct UnsupportedApi {
    /// Stable id from [`api_key`]. The severity says how bad it is; this says what it is.
    pub key: String,
    pub name: String,
    pub description: String,
    pub required_version: String,
    pub severity: String,
}

impl UnsupportedApi {
    /// A row that stops startup. Taking [`BareMinimum`] rather than a string is the audit: a
    /// capability that is not one of the three cannot be given this severity by accident.
    pub fn blocking(
        what: BareMinimum,
        name: impl Into<String>,
        description: impl Into<String>,
        required_version: impl Into<String>,
    ) -> Self {
        Self {
            key: what.key().to_string(),
            name: name.into(),
            description: description.into(),
            required_version: required_version.into(),
            severity: SEVERITY_BLOCKING.to_string(),
        }
    }

    /// A row the app can fix by itself, before it decides anything.
    pub fn installable(
        key: &str,
        name: impl Into<String>,
        description: impl Into<String>,
        required_version: impl Into<String>,
    ) -> Self {
        Self {
            key: key.to_string(),
            name: name.into(),
            description: description.into(),
            required_version: required_version.into(),
            severity: SEVERITY_INSTALLABLE.to_string(),
        }
    }

    /// Something is missing, and the host still streams without it.
    pub fn optional(
        key: &str,
        name: impl Into<String>,
        description: impl Into<String>,
        required_version: impl Into<String>,
    ) -> Self {
        Self {
            key: key.to_string(),
            name: name.into(),
            description: description.into(),
            required_version: required_version.into(),
            severity: SEVERITY_OPTIONAL.to_string(),
        }
    }

    pub fn is_blocking(&self) -> bool {
        self.severity == SEVERITY_BLOCKING
    }

    pub fn is_installable(&self) -> bool {
        self.severity == SEVERITY_INSTALLABLE
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct CompatibilityReport {
    pub os_name: String,
    pub os_version: String,
    pub min_os_version: String,
    pub os_supported: bool,
    pub unsupported_apis: Vec<UnsupportedApi>,
    pub audio_backend: String,
    pub display_backend: String,
    pub capture_backend: String,
    pub encoder_backend: String,
    pub hosted_network_backend: String,
    pub input_backend: String,
    /// The pid holding the display driver, when `display_backend` is `DriverBusy` and we could
    /// name one. The startup gate shows it and offers to quit it; everywhere else it is `None`.
    pub display_driver_holder_pid: Option<u32>,
    /// An id that changes when this machine reboots, or empty where the platform does not
    /// report one.
    ///
    /// It is here rather than behind a command of its own because the one thing that needs it
    /// asks for this report at exactly the same moment: the startup gate's repair budget is
    /// "one driver install, one reinstall and one relaunch per boot", and a restart is the
    /// advice it gives when that budget runs out — so the restart has to clear it.
    pub boot_id: String,
}

impl CompatibilityReport {
    /// Whether this host is missing something it cannot stream without.
    ///
    /// This is the one question the startup gate asks: a `true` here is the only reason the
    /// user is not offered "Continue anyway". `installable` rows are deliberately not counted —
    /// they are remediated and re-probed first, and only what survives that can be missing.
    pub fn bare_minimum_missing(&self) -> bool {
        !self.os_supported
            || self
                .unsupported_apis
                .iter()
                .any(UnsupportedApi::is_blocking)
    }

    /// The rows the app can fix by itself, in the order they were probed.
    pub fn installable(&self) -> impl Iterator<Item = &UnsupportedApi> {
        self.unsupported_apis.iter().filter(|a| a.is_installable())
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, specta::Type)]
pub struct DisplayCapacity {
    pub max: Option<u32>,
    pub in_use: u32,
    pub full: bool,
    pub backend: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct Device {
    /// The live session this row is — one browser tab. A device with two tabs open produces two
    /// rows with one `token` between them. Empty on a row that describes only a device.
    #[serde(rename = "joinId")]
    pub join_id: String,
    /// Where the device was when it last spoke. A label: it changes under a live session and it
    /// is recycled between devices, so nothing is keyed by it.
    pub ip: String,
    /// The device. Every per-device setting, and the `config.json` row, is keyed by this.
    pub token: String,
    pub name: String,
    pub scale: u32,
    pub orientation: String,
    #[serde(rename = "refreshRate")]
    pub refresh_rate: u32,
    #[serde(rename = "videoScale")]
    pub video_scale: u32,
    #[serde(rename = "videoQuality")]
    pub video_quality: u32,
    #[serde(rename = "remoteControl")]
    pub control_enabled: bool,
    /// `"display"` or `"all"` — how far a pointer-locked client may drive this host's cursor.
    /// See `streamer::input::PointerScope`.
    #[serde(rename = "pointerScope")]
    pub pointer_scope: String,
    /// Whether the stream may fall below `refresh_rate` on its own. Off means the chosen rate
    /// is a promise: see `streamer::session::DeviceOverride::auto_frame_rate`.
    #[serde(rename = "autoFrameRate")]
    pub auto_frame_rate: bool,
    /// The slider is a QP floor under CBR rather than a constant QP. Off is the fixed form.
    #[serde(rename = "adaptiveQuality")]
    pub adaptive_quality: bool,
    /// Follow the ratio the device reports instead of `dpr`. Off is the fixed form.
    #[serde(rename = "nativeDpr")]
    pub native_dpr: bool,
    /// Follow the way the device is held instead of `orientation`. Off is the fixed form.
    #[serde(rename = "autoOrientation")]
    pub auto_orientation: bool,
    #[serde(rename = "systemAudio")]
    pub audio_enabled: bool,
    pub os: String,
    #[serde(rename = "screenSize")]
    pub screen_size: String,
    pub dpr: f64,
    #[serde(rename = "maxDpr")]
    pub max_dpr: f64,
}

impl Device {
    pub fn defaults(info: crate::streamer::session::DeviceInfo) -> Self {
        // The *saved* rate is capped the same way the join path caps the reported one, and for
        // the same reason: a device row written from a 360 Hz browser pinned the host to a 360 Hz
        // virtual display for every later join, long after the session that measured it. The
        // slider still goes to `MAX_REFRESH_RATE`; this is only what a device is given without
        // anyone choosing it.
        let refresh_rate = if info.refresh_rate == 0 {
            60
        } else {
            info.refresh_rate.clamp(
                crate::streamer::server::MIN_REFRESH_RATE,
                crate::streamer::server::MAX_REFRESH_RATE,
            )
        };
        let native_dpr = crate::streamer::platform::snap_display_dpr(info.dpr);
        const SLIDER_DPR_CEILING: f64 = 3.0;
        let max_dpr = crate::streamer::platform::snap_display_dpr(info.dpr.max(SLIDER_DPR_CEILING));
        Self {
            join_id: info.join_id,
            ip: info.ip,
            token: info.token,
            name: info.name,
            scale: 100,
            orientation: if info.portrait {
                "Portrait"
            } else {
                "Landscape"
            }
            .to_string(),
            refresh_rate,
            video_scale: 100,
            video_quality: 15,
            control_enabled: true,
            pointer_scope: streamer::input::PointerScope::default()
                .as_str()
                .to_string(),
            auto_frame_rate: false,
            adaptive_quality: false,
            native_dpr: true,
            auto_orientation: true,
            audio_enabled: false,
            os: info.os,
            screen_size: info.screen_size,
            dpr: native_dpr,
            max_dpr,
        }
    }
}

#[tauri::command]
#[specta::specta]
fn exit_app(app: tauri::AppHandle) {
    exit_reason::record("exit_app: the desktop UI asked the host to quit");
    if let Some(state) = app.try_state::<AppState>() {
        streamer::session::broadcast_bye(
            &state.sessions,
            streamer::session::ByeReason::HostExit,
            BYE_FLUSH_TIMEOUT,
        );
        remove_all_displays(&state.virtual_display);
    }
    app.exit(0);
}

/// An id that changes when this machine reboots.
///
/// Derived rather than read from anywhere authoritative: wall clock now, minus the monotonic
/// uptime, bucketed to ten seconds so the tick granularity and a small clock correction do not
/// change it inside one boot. A large clock step would, and what that costs is one extra driver
/// repair attempt — which is why nothing but the repair budget may key on this.
#[cfg(windows)]
pub fn boot_id() -> String {
    let uptime_ms = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    format!("b{}", now_ms.saturating_sub(uptime_ms) / 10_000)
}

/// Empty off Windows: the repair budget exists for the bundled display driver, and no other
/// platform has one to install.
#[cfg(not(windows))]
pub fn boot_id() -> String {
    String::new()
}

/// Ask the other ScreenExtend instance that is holding the display driver's pipe to quit.
///
/// Only ever reaches an instance answering on the single-instance control socket — a desktop
/// host or a `serve` — and never this process, whose own control server may well be the one
/// that file names by the time the desktop UI asks. A `probe --live` run, or an instance that
/// is already on its way out, has no socket to ask: the answer there is `false` and the user is
/// told to close it themselves rather than being handed a button that silently does nothing.
#[tauri::command]
#[specta::specta]
fn quit_driver_holder(app: tauri::AppHandle) -> bool {
    let Ok(dir) = crate::data_dir::local_data_dir(&app) else {
        return false;
    };
    let ctrl_path = dir.join(single_instance::CTRL_FILE);
    match single_instance::running_instance_pid_at(&ctrl_path) {
        Some(pid) if pid != std::process::id() => {
            tprintln!(
                "[drivers] asking the instance holding the display driver (pid {pid}) to quit"
            );
            single_instance::signal_running_instance(&ctrl_path, single_instance::Command::Quit)
        }
        _ => {
            teprintln!(
                "[drivers] the process holding the display driver does not answer on the \
                 control socket; it has to be closed by hand"
            );
            false
        }
    }
}

#[tauri::command]
#[specta::specta]
fn set_window_zoom(window: tauri::WebviewWindow, factor: f64) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if !webview_zoom_supported() {
        return Err("this WebKit has no page zoom; it arrived with Safari 14".to_string());
    }
    window.set_zoom(factor).map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
fn webview_zoom_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        objc2::runtime::AnyClass::get(c"WKWebView")
            .is_some_and(|class| class.responds_to(objc2::sel!(setPageZoom:)))
    })
}

#[tauri::command]
#[specta::specta]
fn get_username() -> String {
    whoami::username().unwrap_or_else(|_| "".to_string())
}

fn avatar_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = crate::data_dir::data_dir(app).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("avatar.png"))
}

fn write_avatar(app: &tauri::AppHandle, bytes: &[u8]) -> bool {
    match avatar_path(app).and_then(|path| std::fs::write(&path, bytes).map_err(|e| e.to_string()))
    {
        Ok(()) => true,
        Err(e) => {
            teprintln!("[avatar] failed to save avatar: {e}");
            false
        }
    }
}

fn read_avatar(app: &tauri::AppHandle) -> Option<Vec<u8>> {
    let path = avatar_path(app).ok()?;
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            teprintln!("[avatar] failed to read avatar: {e}");
            None
        }
    }
}

// Base64 rather than `Vec<u8>`, which tauri-specta maps to `number[]`: a 256x256 PNG then
// crossed the bridge as a JSON array of decimal integers -- three to four bytes of JSON
// per image byte -- to be parsed into an array of tens of thousands of boxed numbers, walked
// into a `Uint8Array`, wrapped in a `Blob` and run through a `FileReader` to become the
// base64 data URL the `<img>` wanted in the first place. This is the same payload the
// webview needs, one third the size of the array encoding, with no conversion at all.
#[tauri::command]
#[specta::specta]
fn set_avatar(app: tauri::AppHandle, bytes: String) -> bool {
    use base64::Engine as _;
    match base64::engine::general_purpose::STANDARD.decode(bytes.as_bytes()) {
        Ok(decoded) => write_avatar(&app, &decoded),
        Err(e) => {
            teprintln!("[avatar] failed to decode the avatar payload: {e}");
            false
        }
    }
}

#[tauri::command]
#[specta::specta]
fn get_avatar(app: tauri::AppHandle) -> Option<String> {
    use base64::Engine as _;
    read_avatar(&app).map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[tauri::command]
#[specta::specta]
fn remove_avatar(app: tauri::AppHandle) -> bool {
    let path = match avatar_path(&app) {
        Ok(path) => path,
        Err(e) => {
            teprintln!("[avatar] failed to resolve avatar path: {e}");
            return false;
        }
    };
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            teprintln!("[avatar] failed to remove avatar: {e}");
            false
        }
    }
}

#[cfg(target_os = "macos")]
fn build_menu(handle: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{AboutMetadata, MenuBuilder, PredefinedMenuItem, SubmenuBuilder};

    let about = PredefinedMenuItem::about(
        handle,
        Some("About ScreenExtend"),
        Some(AboutMetadata {
            name: Some("ScreenExtend".into()),
            ..Default::default()
        }),
    )?;
    let quit = PredefinedMenuItem::quit(handle, Some("Quit ScreenExtend"))?;
    let app_menu = SubmenuBuilder::new(handle, "ScreenExtend")
        .item(&about)
        .separator()
        .item(&quit)
        .build()?;
    MenuBuilder::new(handle).item(&app_menu).build()
}

#[cfg(not(target_os = "macos"))]
fn build_menu(handle: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    tauri::menu::MenuBuilder::new(handle).build()
}

// The legacy `installdrivers` / `removedrivers` / `hostednetwork` subcommands.
//
// They stay because installers from earlier releases invoke them by these exact names
// (`windows/hooks.nsh`, `windows/fragment.wxs`), so an old uninstaller must keep working.
// `drivers install|remove` and `network start` are the modern equivalents.
//
// Every path resolution and every shell call here used to be an `.unwrap()`. A missing
// `nefconc`, a shell-ACL mismatch against `capabilities/default.json` or an unresolvable
// resource directory therefore crashed the process — during an uninstall, where the caller
// wants a non-zero exit code and a reason, not a crash dialog.

/// Spawn one legacy external command. A non-zero exit is reported but not fatal: `certutil
/// -delstore` for a certificate that is already gone is the normal uninstall case, and the
/// previous code ignored the status entirely. Only a failure to run at all is an error.
async fn legacy_shell(
    app: &tauri::AppHandle,
    program: &str,
    args: &[&str],
    cwd: &std::path::Path,
) -> Result<(), String> {
    match app
        .shell()
        .command(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(out) => {
            if !out.status.success() {
                teprintln!(
                    "[legacy-cli] {program} {} exited with {:?}: {}",
                    args.join(" "),
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        }
        Err(e) => Err(format!("could not run {program}: {e}")),
    }
}

fn legacy_resource(app: &tauri::AppHandle, file: &str) -> Result<String, String> {
    app.path()
        .resolve(file, BaseDirectory::Resource)
        .map_err(|e| format!("could not resolve bundled resource {file}: {e}"))?
        .into_os_string()
        .into_string()
        .map_err(|_| format!("bundled resource path for {file} is not valid UTF-8"))
}

fn legacy_resource_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .resource_dir()
        .map_err(|e| format!("could not resolve the resource directory: {e}"))
}

const VDD_HARDWARE_ID: &str = "Root\\VirtualDisplayDriver";
const VDD_CLASS_GUID: &str = "4D36E968-E325-11CE-BFC1-08002BE10318";

async fn legacy_install_drivers(app: &tauri::AppHandle) -> Result<(), String> {
    let dir = legacy_resource_dir(app)?;
    let cert = legacy_resource(app, "resources/ScreenExtend.cer")?;
    let inf = legacy_resource(app, "resources/VirtualDisplayDriver.inf")?;
    legacy_shell(app, "certutil", &["-addstore", "-f", "root", &cert], &dir).await?;
    legacy_shell(
        app,
        "certutil",
        &["-addstore", "-f", "TrustedPublisher", &cert],
        &dir,
    )
    .await?;
    legacy_shell(
        app,
        "nefconc",
        &[
            "--remove-device-node",
            "--hardware-id",
            VDD_HARDWARE_ID,
            "--class-guid",
            VDD_CLASS_GUID,
        ],
        &dir,
    )
    .await?;
    legacy_shell(
        app,
        "nefconc",
        &[
            "--create-device-node",
            "--class-name",
            "Display",
            "--class-guid",
            VDD_CLASS_GUID,
            "--hardware-id",
            VDD_HARDWARE_ID,
        ],
        &dir,
    )
    .await?;
    legacy_shell(
        app,
        "nefconc",
        &["--install-driver", "--inf-path", &inf],
        &dir,
    )
    .await
}

async fn legacy_remove_drivers(app: &tauri::AppHandle) -> Result<(), String> {
    let dir = legacy_resource_dir(app)?;
    let inf = legacy_resource(app, "resources/VirtualDisplayDriver.inf")?;
    legacy_shell(
        app,
        "nefconc",
        &[
            "--remove-device-node",
            "--hardware-id",
            VDD_HARDWARE_ID,
            "--class-guid",
            VDD_CLASS_GUID,
        ],
        &dir,
    )
    .await?;
    legacy_shell(
        app,
        "nefconc",
        &["--uninstall-driver", "--inf-path", &inf],
        &dir,
    )
    .await?;
    legacy_shell(
        app,
        "certutil",
        &["-delstore", "root", "ScreenExtend"],
        &dir,
    )
    .await?;
    legacy_shell(
        app,
        "certutil",
        &["-delstore", "TrustedPublisher", "ScreenExtend"],
        &dir,
    )
    .await
}

async fn legacy_hosted_network(
    app: &tauri::AppHandle,
    ssid: &str,
    password: &str,
) -> Result<(), String> {
    if ssid.is_empty() {
        return Err("hostednetwork needs an SSID".to_string());
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    legacy_shell(
        app,
        "netsh",
        &[
            "wlan",
            "set",
            "hostednetwork",
            "mode=allow",
            &format!("ssid={ssid}"),
            &format!("key={password}"),
        ],
        &cwd,
    )
    .await?;
    legacy_shell(app, "netsh", &["wlan", "start", "hostednetwork"], &cwd).await
}

/// Run one legacy branch and turn its result into the process exit code the installer reads.
fn finish_legacy(app: &tauri::AppHandle, result: Result<(), String>) {
    match result {
        Ok(()) => app.exit(0),
        Err(e) => {
            teprintln!("[legacy-cli] {e}");
            eprintln!("error: {e}");
            app.exit(1);
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    cli::fast_path();
    #[cfg(target_os = "macos")]
    macos_utils::objc_exception::install_logger();

    let builder = Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            compatibility::check_system_requirements,
            permissions::check_permissions,
            permissions::request_permission,
            permissions::open_permission_settings,
            permissions::relaunch_app,
            setup,
            set_session_credentials,
            register_cloud_session,
            unregister_cloud_session,
            get_cloud_status,
            exit_app,
            set_window_zoom,
            get_username,
            set_avatar,
            get_avatar,
            remove_avatar,
            networking::get_network_adapters,
            networking::watch_for_network_changes,
            hosted_network::start_hosted_network,
            hosted_network::stop_hosted_network,
            hosted_network::is_hosted_network,
            hosted_network::get_hosted_network_status,
            hosted_network::is_wifi_on,
            hosted_network::turn_on_wifi,
            install_drivers,
            remove_drivers,
            quit_driver_holder,
            install_audio_driver,
            uninstall_audio_driver,
            audio_driver_status,
            set_legacy_volume_key_proxy,
            set_device_override,
            set_device_pointer_scope,
            set_device_auto_frame_rate,
            set_device_adaptive_modes,
            refresh_device_stream,
            disconnect_session,
            set_device_audio_output,
            get_device_audio_outputs,
            remove_device_override,
            set_device_banned,
            set_device_approved,
            set_disconnect_grace,
            get_disconnect_grace,
            get_display_capacity,
            set_turn_config,
            get_turn_config,
            set_server_ports,
            get_server_ports,
            set_disable_gpu_encode,
            get_disable_gpu_encode,
            logbus::get_log_backlog,
            logbus::set_log_streaming,
            logbus::log_frontend_error
        ])
        .events(collect_events![
            DeviceJoin,
            DeviceAudioOutputs,
            DeviceModify,
            DeviceRemove,
            NetworkChange,
            HostedNetworkNoPassword,
            ScreenRecordingRevoked,
            CloudStatusChange,
            SessionIdChange,
            JoinAttemptsPaused,
            logbus::LogLine
        ]);

    // Debug build, no CLI arguments: that is `pnpm tauri dev`, and `cargo run -- serve` is
    // deliberately excluded. The path is resolved from the crate directory rather than from
    // `..`, so it lands on the right file whatever the working directory is -- a relative
    // path here wrote somewhere else, or panicked, depending on where the binary started.
    //
    // "no arguments" is still only a proxy for "this is the current build", so a stale debug
    // binary can revert the whole surface. `scripts/check-bindings.mjs` is what catches that:
    // a static cross-check against the macro lists above, run in CI.
    #[cfg(debug_assertions)]
    if std::env::args_os().nth(1).is_none() {
        let bindings_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/lib/bindings.ts");
        builder
            .export(Typescript::default(), &bindings_path)
            .expect("error while exporting typescript bindings");
        let contents = std::fs::read_to_string(&bindings_path)
            .expect("error while reading typescript bindings");
        // The generated file declares a `TAURI_CHANNEL` helper it does not always use, which
        // `noUnusedLocals` rejects; that one artifact is why the whole file is unchecked.
        std::fs::write(&bindings_path, format!("// @ts-nocheck\n{contents}"))
            .expect("error while writing typescript bindings");
        println!("exported bindings to {}", bindings_path.display());
    }

    tauri::Builder::default()
        .plugin(
            tauri_plugin_updater::Builder::new()
                .default_version_comparator(|current, update| update.version != current)
                .build(),
        )
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_cli::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .menu(build_menu)
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            let _ = crate::cli::dispatch(app.handle());
            crate::streamer::input::prime();
            permissions::prime();
            match app.cli().matches().ok().and_then(|m| m.subcommand) {
                Some(command) if command.name == "hostednetwork" => {
                    let mut ssid = String::new();
                    let mut password = String::new();
                    for (key, arg_data) in &command.matches.args {
                        match key.as_str() {
                            "ssid" => ssid = arg_data.value.as_str().unwrap_or("").to_string(),
                            "password" => {
                                password = arg_data.value.as_str().unwrap_or("").to_string()
                            }
                            _ => {}
                        }
                    }
                    let handle = app.handle().clone();
                    let result = tauri::async_runtime::block_on(legacy_hosted_network(
                        &handle, &ssid, &password,
                    ));
                    finish_legacy(&handle, result);
                }
                Some(command) if command.name == "installdrivers" => {
                    let handle = app.handle().clone();
                    let result = tauri::async_runtime::block_on(legacy_install_drivers(&handle));
                    finish_legacy(&handle, result);
                }
                Some(command) if command.name == "removedrivers" => {
                    let handle = app.handle().clone();
                    let result = tauri::async_runtime::block_on(legacy_remove_drivers(&handle));
                    finish_legacy(&handle, result);
                }
                _ => {
                    // This runs before the window exists, so a failure here has no UI to report
                    // itself in other than a dialog. It used to be an `.unwrap()` on the normal
                    // startup path of every user.
                    let lock_dir = match crate::data_dir::local_data_dir(app) {
                        Ok(dir) => dir,
                        Err(e) => {
                            app.dialog()
                                .message(format!(
                                    "ScreenExtend cannot resolve its data directory:
{e}"
                                ))
                                .title("ScreenExtend")
                                .blocking_show();
                            std::process::exit(1);
                        }
                    };
                    let _ = std::fs::create_dir_all(&lock_dir);
                    // Before anything can exit: every deliberate exit route writes its reason
                    // here, and this is what reads the previous instance's back into the log.
                    exit_reason::set_dir(lock_dir.clone());
                    exit_reason::report_previous(&lock_dir);
                    let lock_path = lock_dir.join(single_instance::LOCK_FILE);
                    let ctrl_path = lock_dir.join(single_instance::CTRL_FILE);

                    let mut lock = single_instance::acquire_lock(&lock_path);

                    // An unwritable or unopenable lock file is not another instance. Offering
                    // "Show running instance" for one that does not exist is how that used to
                    // present.
                    if let single_instance::LockOutcome::Unavailable(e) = &lock {
                        app.dialog()
                            .message(format!(
                                "ScreenExtend could not open its single-instance lock at
{}

{e}",
                                lock_path.display()
                            ))
                            .title("ScreenExtend")
                            .blocking_show();
                        std::process::exit(1);
                    }

                    if matches!(lock, single_instance::LockOutcome::HeldByOther) {
                        let quit_other = app
                            .dialog()
                            .message("ScreenExtend is already running.")
                            .title("ScreenExtend")
                            .buttons(MessageDialogButtons::OkCancelCustom(
                                "Quit running instance".into(),
                                "Show running instance".into(),
                            ))
                            .blocking_show();

                        if quit_other {
                            single_instance::signal_running_instance(
                                &ctrl_path,
                                single_instance::Command::Quit,
                            );
                            lock = single_instance::wait_for_lock(
                                &lock_path,
                                std::time::Duration::from_secs(10),
                            );
                            if !matches!(lock, single_instance::LockOutcome::Acquired(_)) {
                                std::process::exit(0);
                            }
                        } else {
                            single_instance::signal_running_instance(
                                &ctrl_path,
                                single_instance::Command::Focus,
                            );
                            std::process::exit(0);
                        }
                    }

                    if let single_instance::LockOutcome::Acquired(file) = lock {
                        std::mem::forget(file);
                    }

                    logbus::attach(app.handle().clone());
                    let window = tauri::WebviewWindowBuilder::new(
                        app,
                        "main".to_string(),
                        tauri::WebviewUrl::App("index.html".into()),
                    )
                    .min_inner_size(1050.0, 650.0)
                    .inner_size(1200.0, 675.0)
                    .title("ScreenExtend")
                    .resizable(true)
                    .maximized(true)
                    .build()?;

                    focus_main_window(&window);

                    if let Err(e) = single_instance::start_control_server(
                        app.handle().clone(),
                        ctrl_path,
                        focus_main_window,
                        single_instance::quit_main_window,
                        cli::reload_config_into_state,
                    ) {
                        teprintln!("[single-instance] failed to start control server: {e}");
                    }
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        // The run loop is where an exit nobody asked for becomes visible. Tauri's default is to
        // end the process once the last window is gone, so a `WM_CLOSE` from anywhere — a
        // restart manager replacing the binary, a session logoff, a stray Alt+F4 — produced a
        // clean exit(0) with no crash dump, no Application-log event and no line anywhere saying
        // what happened. The window's own close request and the run loop's `ExitRequested` are
        // recorded here, so the reason survives the process that had it.
        .run(|_app, event| match &event {
            tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { .. },
                ..
            } => {
                exit_reason::record(format!(
                    "window `{label}` was asked to close (the title-bar button, Alt+F4, or \
                     another process sending WM_CLOSE)"
                ));
            }
            tauri::RunEvent::ExitRequested { code, .. } => {
                exit_reason::record(format!(
                    "the run loop was asked to exit (code {code:?}) with no window left and no \
                     route of ours having named a reason"
                ));
            }
            _ => {}
        });
}

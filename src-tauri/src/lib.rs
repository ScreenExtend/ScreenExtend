use serde::{Deserialize, Serialize};
use specta::Type;
#[cfg(debug_assertions)]
use specta_typescript::Typescript;
use tauri::path::BaseDirectory;
use tauri::Manager;
use tauri_plugin_cli::CliExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_shell::ShellExt;
use tauri_specta::{collect_commands, collect_events, Builder, Event};

#[macro_use]
mod logbus;
#[cfg(feature = "bench")]
mod bench;
mod cli;
mod data_dir;
mod envcfg;
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
    /// which is not the same as `Some("")` (the device's default output).
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
/// - `notStarted`: nothing is being advertised.
/// - `advertised`: the platform accepted the start, and that is all that is known. An AP
///   in this state is visible to a phone and may still be unjoinable, because nothing is
///   serving DHCP.
/// - `verified`: the AP interface holds a routable IPv4, i.e. something is serving
///   addresses, so a client that associates can actually get on.
///
/// `secured` is there because on macOS the HostAP SPI can
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
    /// The security type as it was logged, e.g. `WPA2` or `securityType 2 (open, no
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
/// the OTP did not change** (the rotation case) and the frontend must leave its copy alone
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

/// The link governor kept lowering a device's picture to keep its stream moving, so its network
/// is what holds it back. Never sent for a device already on the hosted network. Only Windows
/// emits it; it is registered everywhere so the bindings match.
#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct SlowConnection {
    #[serde(rename = "joinId")]
    pub join_id: String,
    pub token: String,
    pub name: String,
    /// A label, as on every device row.
    pub ip: String,
    /// How long the governor was engaged within the last `window_secs`.
    #[serde(rename = "engagedSecs")]
    pub engaged_secs: u32,
    #[serde(rename = "windowSecs")]
    pub window_secs: u32,
}

/// The severities a capability note can carry.
///
/// `blocking` is the only one that takes "Continue anyway" off the startup gate, so it is
/// reserved for the **bare minimum**: a host that cannot create a virtual display, capture, or
/// encode. Anything a user could live without (the hosted network, audio, remote input, a
/// hardware GPU, the preferred capture backend) is `optional`. `installable` is a row the app
/// can fix by itself: the bootstrap remediates it and re-probes before deciding anything.
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
    /// No encoder at all, not even the software one.
    pub const ENCODER: &str = "encoder";
    /// The virtual display driver is not installed. Installable.
    pub const DISPLAY_DRIVER: &str = "display-driver";
    /// The virtual display driver is installed but not answering. Installable (reinstall).
    pub const DISPLAY_DRIVER_UNRESPONSIVE: &str = "display-driver-unresponsive";
    /// The virtual display driver is installed and healthy, and another process of ours is
    /// already using it. Deliberately **not** installable: reinstalling a driver whose pipe is
    /// merely occupied only loops elevation prompts.
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

/// The three capabilities a host cannot do without: create a display, capture it, and encode
/// the result. Only these may produce a `blocking` row.
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
    /// A row that stops startup. Takes [`BareMinimum`], not a string, so only one of the three
    /// capabilities can be given this severity.
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
    /// advice it gives when that budget runs out, so the restart has to clear it.
    pub boot_id: String,
}

impl CompatibilityReport {
    /// Whether this host is missing something it cannot stream without.
    ///
    /// A `true` here is the only reason the startup gate withholds "Continue anyway".
    /// `installable` rows are not counted: they are remediated and re-probed first.
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

/// What the virtual display driver's host process has built up, read without elevation.
///
/// The bundled IddCx driver leaks ~47-124 MB of commit, ~37 handles and ~5 threads per display
/// creation or mode change and never gives it back; at ~36 GB / ~3,200 threads it stops attaching
/// displays at all. Only a device restart (elevated) or a reboot frees it, so the startup gate
/// reads this before `setup` and offers the restart.
#[derive(Serialize, Deserialize, Debug, Clone, Default, Type)]
pub struct DisplayDriverHealth {
    /// `false` where there is no such driver (macOS, Linux). Nothing else here means anything then.
    pub supported: bool,
    /// The host is over a threshold and the driver is installed: restart it before creating
    /// displays.
    pub needs_restart: bool,
    /// The driver host's pid, when one was found.
    pub pid: Option<u32>,
    /// How that pid was chosen: `pipeServer` (the process serving the driver's own named pipe,
    /// exact), `largestWudfHost` (a guess, used only when the pipe could not name its server),
    /// `none` (no driver host found) or `unsupported`.
    pub identified_by: String,
    /// Private commit of the host process, MB (what Task Manager calls "Commit size").
    pub commit_mb: u32,
    pub threads: u32,
    pub handles: u32,
    pub threshold_mb: u32,
    /// `0` when the thread criterion is switched off (see `SE_DRIVER_RESTART_THRESHOLD_MB`).
    pub threshold_threads: u32,
    /// One line for the log and `doctor`.
    pub detail: String,
}

/// What an elevated `drivers restart` came back with.
#[derive(Serialize, Deserialize, Debug, Clone, Default, Type)]
pub struct DriverRestartOutcome {
    pub ok: bool,
    /// `restarted`, `declined` (the UAC prompt was dismissed), `timeout`, `refused` (a host is
    /// streaming or holds displays), `notElevated`, `needsReboot`, `notInstalled`, `failed` or
    /// `unsupported`. The desktop UI keys its message on this.
    pub reason: String,
    /// The subcommand's own sentence, when it wrote one.
    pub detail: String,
    pub commit_mb_before: Option<u32>,
    pub commit_mb_after: Option<u32>,
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
    /// The live session this row is: one browser tab. A device with two tabs open produces two
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
    /// `"display"` or `"all"`: how far a pointer-locked client may drive this host's cursor.
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
        // the saved default gets the same clamp the join path applies to the reported rate
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
    #[cfg(windows)]
    HOST_TORN_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
    app.exit(0);
}

/// Set once the host's teardown has run, so the run loop's `Exit` does not repeat it.
#[cfg(windows)]
static HOST_TORN_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The desktop host's teardown, from the run loop's `Exit` (Windows).
///
/// The webview's `onCloseRequested` path (stop the hosted network, then [`exit_app`]) does not
/// run when the renderer crashed or hung, or on a logoff or shutdown (tao turns `WM_ENDSESSION`
/// into `Exit`), and attached virtual displays and a Mobile Hotspot are OS state that outlives
/// the process. Idempotent, and a no-op when the JavaScript path already ran or `setup` never
/// built an `AppState`.
#[cfg(windows)]
fn windows_exit_teardown(app: &tauri::AppHandle) {
    use streamer::lock::LockExt;
    if HOST_TORN_DOWN.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    exit_reason::record("the run loop exited before the desktop UI's own shutdown had run");
    streamer::session::broadcast_bye(
        &state.sessions,
        streamer::session::ByeReason::HostExit,
        BYE_FLUSH_TIMEOUT,
    );
    remove_all_displays(&state.virtual_display);
    // only a network this process started: the stopper is registered by a successful start
    let started_here = state.stop_hosted_network.lock_recover().is_some();
    if started_here {
        let stopped = windows_utils::hosted_network::stop_hosted_network_blocking(app, &state);
        tprintln!("[exit] hosted network stopped on exit: {stopped}");
    }
}

/// An id that changes when this machine reboots.
///
/// The kernel's own per-boot counter when it can be read ([`windows_boot_counter`]). Otherwise
/// derived: wall clock minus monotonic uptime, bucketed to ten seconds. The derived value can
/// flip inside one boot (time sync slews the wall clock but not the tick count, and a reading
/// can land on either side of a bucket edge). That costs one extra driver repair attempt, so
/// nothing but the repair budget may key on this. The two forms carry different prefixes, so
/// neither can equal the other.
#[cfg(windows)]
pub fn boot_id() -> String {
    let uptime_ms = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    boot_id_from(windows_boot_counter(), now_ms, uptime_ms)
}

/// `BootId` under the prefetcher's parameters: a counter the kernel advances once per boot (a
/// Fast Startup resume is not a boot, and does not advance it, which matches the driver host's
/// lifetime: it is hibernated with the kernel session). Readable unelevated.
#[cfg(windows)]
fn windows_boot_counter() -> Option<u32> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters",
        )
        .ok()?
        .get_value::<u32, _>("BootId")
        .ok()
}

#[cfg(windows)]
fn boot_id_from(counter: Option<u32>, now_ms: u64, uptime_ms: u64) -> String {
    match counter {
        Some(id) => format!("k{id}"),
        None => format!("b{}", now_ms.saturating_sub(uptime_ms) / 10_000),
    }
}

#[cfg(all(windows, test))]
mod boot_id_tests {
    use super::*;

    /// The driver-repair budget keys on this, so it must not move inside one boot. With the
    /// counter readable it cannot; the derived fallback can.
    #[test]
    fn the_boot_id_does_not_follow_the_wall_clock() {
        // two readings 2 ms apart in uptime, the second after a 2-minute clock correction;
        // then two that straddle a ten-second bucket edge
        let uptime: u64 = 3 * 24 * 3600 * 1000;
        let a = boot_id_from(Some(13), 1_700_000_009_999, uptime);
        let b = boot_id_from(Some(13), 1_700_000_010_001 + 120_000, uptime + 2);
        assert_eq!(a, b);
        assert_ne!(
            boot_id_from(None, 1_700_000_009_999, uptime),
            boot_id_from(None, 1_700_000_010_001, uptime),
            "the fallback is the old derivation, edge flip and all"
        );
        assert_ne!(boot_id_from(Some(14), 0, 0), a, "a new boot is a new id");
    }

    #[test]
    fn this_machine_answers_with_its_boot_counter() {
        if windows_boot_counter().is_none() {
            crate::test_support::skip_because(
                "this_machine_answers_with_its_boot_counter",
                "the BootId registry value is not readable here",
            );
            return;
        }
        let id = boot_id();
        assert!(id.starts_with('k'), "{id}");
        assert_eq!(id, boot_id());
    }
}

/// Empty off Windows: the repair budget exists for the bundled display driver, and no other
/// platform has one to install.
#[cfg(not(windows))]
pub fn boot_id() -> String {
    String::new()
}

/// Ask the other ScreenExtend instance that is holding the display driver's pipe to quit.
///
/// Only ever reaches an instance answering on the single-instance control socket (a desktop
/// host or a `serve`) and never this process, whose own control server may well be the one
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

// base64, not `Vec<u8>`: tauri-specta maps that to `number[]`, three to four bytes of JSON per
// image byte, and the `<img>` wants a base64 data URL anyway
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

// The legacy `installdrivers` / `removedrivers` / `hostednetwork` subcommands. Installers already
// shipped invoke them by these exact names (`windows/hooks.nsh`, `windows/fragment.wxs`), so
// they must keep working; `drivers install|remove` and `network start` are the modern forms.
// Nothing here may panic: a missing `nefconc`, a shell-ACL mismatch against
// `capabilities/default.json` or an unresolvable resource directory has to reach the caller as
// a non-zero exit code and a reason, not a crash dialog in the middle of an uninstall.

/// Spawn one legacy external command. A non-zero exit is logged but not fatal (`certutil
/// -delstore` for a certificate that is already gone is the normal uninstall case), except for
/// the steps `legacy_step_is_required` names on Windows. A failure to run at all is an error.
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
                #[cfg(windows)]
                if legacy_step_is_required(program, args, out.status.code()) {
                    return Err(format!(
                        "{program} {} failed (exit {:?}); the driver is not installed",
                        args.first().copied().unwrap_or_default(),
                        out.status.code()
                    ));
                }
            }
            Ok(())
        }
        Err(e) => Err(format!("could not run {program}: {e}")),
    }
}

/// Windows: whether a non-zero exit from this legacy step means there is no driver.
///
/// Only creating the device node and installing the driver package are fatal: without them
/// `installdrivers` must not exit 0 (a refused signature, a missing devnode). Removing a node,
/// a package or a certificate that is already gone is the ordinary uninstall case. 3010 and
/// 1641 are the installer convention for "succeeded, a reboot is required / was started".
#[cfg(windows)]
fn legacy_step_is_required(program: &str, args: &[&str], code: Option<i32>) -> bool {
    if matches!(code, Some(3010) | Some(1641)) {
        return false;
    }
    program == "nefconc"
        && args
            .first()
            .is_some_and(|a| *a == "--create-device-node" || *a == "--install-driver")
}

#[cfg(all(windows, test))]
mod legacy_step_tests {
    use super::legacy_step_is_required;

    #[test]
    fn only_the_two_install_steps_are_fatal() {
        assert!(legacy_step_is_required(
            "nefconc",
            &["--create-device-node", "--class-name", "Display"],
            Some(1)
        ));
        assert!(legacy_step_is_required(
            "nefconc",
            &["--install-driver", "--inf-path", "x.inf"],
            Some(-1)
        ));
        assert!(!legacy_step_is_required(
            "nefconc",
            &["--install-driver", "--inf-path", "x.inf"],
            Some(3010)
        ));
        for args in [
            &["--remove-device-node", "--hardware-id", "x"][..],
            &["--uninstall-driver", "--inf-path", "x.inf"][..],
        ] {
            assert!(!legacy_step_is_required("nefconc", args, Some(1)));
        }
        assert!(!legacy_step_is_required(
            "certutil",
            &["-delstore", "root", "ScreenExtend"],
            Some(1)
        ));
        assert!(!legacy_step_is_required(
            "netsh",
            &["wlan", "start", "hostednetwork"],
            Some(1)
        ));
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

/// The elevated half of the legacy hosted network when the passphrase came in a file: it is on
/// no command line, this process's or netsh's, because the key goes through the WLAN service.
#[cfg(windows)]
async fn legacy_hosted_network_from_file(
    app: &tauri::AppHandle,
    ssid: &str,
    password_file: &std::path::Path,
) -> Result<(), String> {
    let password = crate::windows_utils::hosted_network::take_passphrase_file(password_file)?;
    if ssid.is_empty() {
        return Err("hostednetwork needs an SSID".to_string());
    }
    // validate before anything changes: a passphrase the WLAN service would refuse must not
    // leave the SSID rewritten and no network started
    crate::windows_utils::hosted_network::validate_passphrase(&password)?;
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
        ],
        &cwd,
    )
    .await?;
    crate::windows_utils::hosted_network::set_legacy_passphrase(&password)?;
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
            display_driver_health,
            restart_display_driver,
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
            streamer::config::set_disable_link_governor,
            streamer::config::get_disable_link_governor,
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
            SlowConnection,
            logbus::LogLine
        ]);

    // Debug build, no CLI arguments: that is `pnpm tauri dev`; `cargo run -- serve` is
    // deliberately excluded. The path comes from the crate directory, not the working directory.
    // "No arguments" is only a proxy for "this is the current build": a stale debug binary can
    // revert the whole surface, which `scripts/check-bindings.mjs` catches.
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
                    #[cfg(windows)]
                    let mut password_file = String::new();
                    for (key, arg_data) in &command.matches.args {
                        match key.as_str() {
                            "ssid" => ssid = arg_data.value.as_str().unwrap_or("").to_string(),
                            "password" => {
                                password = arg_data.value.as_str().unwrap_or("").to_string()
                            }
                            #[cfg(windows)]
                            "password-file" => {
                                password_file = arg_data.value.as_str().unwrap_or("").to_string()
                            }
                            _ => {}
                        }
                    }
                    let handle = app.handle().clone();
                    #[cfg(windows)]
                    let result = if password_file.is_empty() {
                        tauri::async_runtime::block_on(legacy_hosted_network(
                            &handle, &ssid, &password,
                        ))
                    } else {
                        tauri::async_runtime::block_on(legacy_hosted_network_from_file(
                            &handle,
                            &ssid,
                            std::path::Path::new(&password_file),
                        ))
                    };
                    #[cfg(not(windows))]
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
                    // runs before the window exists: a dialog is the only way to report a failure
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
                    // before anything can exit: every deliberate exit route writes its reason
                    // here, and this reads the previous instance's back into the log
                    exit_reason::set_dir(lock_dir.clone());
                    exit_reason::report_previous(&lock_dir);
                    let lock_path = lock_dir.join(single_instance::LOCK_FILE);
                    let ctrl_path = lock_dir.join(single_instance::CTRL_FILE);

                    let mut lock = single_instance::acquire_lock(&lock_path);

                    // an unwritable or unopenable lock file is not another instance: do not
                    // offer "Show running instance" for it
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
        // Tauri ends the process once the last window is gone, so a `WM_CLOSE` from anywhere (a
        // restart manager replacing the binary, a session logoff, a stray Alt+F4) is a clean
        // exit(0) with no crash dump and no Application-log event. Record the window's close
        // request and the run loop's `ExitRequested` so the reason survives the process.
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
                // nothing of ours asked to exit, so on macOS this is a Quit from outside, such as
                // the "Quit Now" macOS offers once Screen Recording is granted
                #[cfg(target_os = "macos")]
                if exit_reason::recorded().is_none() {
                    macos_utils::permissions::reopen_if_quit_for_a_grant();
                }
                exit_reason::record(format!(
                    "the run loop was asked to exit (code {code:?}) with no window left and no \
                     route of ours having named a reason"
                ));
            }
            #[cfg(windows)]
            tauri::RunEvent::Exit => windows_exit_teardown(_app),
            _ => {}
        });
}

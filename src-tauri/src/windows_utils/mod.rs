pub mod audio;
pub mod com;
pub mod compatibility;
pub mod device_reporter;
pub mod display_layout;
pub mod driver_health;
pub mod driver_ipc;
pub mod hosted_network;
pub mod networking;
pub mod permissions;
#[cfg(test)]
pub(crate) mod process_handles;
pub mod streamer;
pub mod virtual_display;
#[allow(warnings, clippy::all)]
pub mod windows_capture;

use crate::streamer::cloud::{
    CloudClient, CloudConfig, CloudState, CloudStatusSink, SharedCloudStatusSink,
};
use crate::streamer::lock::LockExt;
use crate::streamer::session::{
    self, DeviceOverride, ReconfigAction, SessionAuth, SharedApprovedDevices, SharedBannedDevices,
    SharedDeviceOverrides, SharedDeviceReporter, SharedOtpLimiter, SharedServerPorts,
    SharedSessions, SharedTurnConfig, SharedVirtualDisplay, UserTurnConfig,
};
use crate::streamer::{Config, Streamer};
use crate::DisplayCapacity;
use device_reporter::TauriDeviceReporter;
use networking::NetworkInfo;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;
use tauri::State;
use virtual_display::WindowsVirtualDisplay;

/// One adapter's server.
pub struct StreamerHandle {
    handle: axum_server::Handle,
    /// Set when this server is being stopped on purpose, so its thread stops retrying a bind.
    stopping: Arc<AtomicBool>,
}

impl StreamerHandle {
    fn stop(&self, grace: Duration) {
        self.stopping.store(true, Ordering::Release);
        self.handle.graceful_shutdown(Some(grace));
    }
}

/// How long an adapter's server waits before trying its bind again after the last attempt
/// failed: 1 s, doubling, capped at 30 s.
///
/// Bind failures are usually transient: a port change whose old socket outlived the release
/// wait, or another program that lets the port go later.
pub fn rebind_delay(failures: u32) -> Duration {
    let secs = 1u64 << failures.saturating_sub(1).min(5);
    Duration::from_secs(secs.min(30))
}

pub struct AppState {
    pub virtual_display: SharedVirtualDisplay,
    pub stop_hosted_network: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pub hosted_network_running: Mutex<bool>,
    pub network_adapters: Mutex<Vec<NetworkInfo>>,
    pub local_ips: session::SharedLocalIps,
    pub streamers: Mutex<HashMap<String, StreamerHandle>>,
    pub session_auth: SessionAuth,
    pub device_reporter: SharedDeviceReporter,
    pub device_overrides: SharedDeviceOverrides,
    /// The audio output each device has been asked to play on, keyed by device token. Per device
    /// rather than per join, and it outlives every join so `serve` can replay a saved choice.
    pub audio_sinks: session::SharedAudioSinks,
    pub sessions: SharedSessions,
    pub disconnect_grace: session::SharedDisconnectGrace,
    pub user_turn: SharedTurnConfig,
    pub stun_urls: session::SharedStunConfig,
    pub banned_devices: SharedBannedDevices,
    pub approved_devices: SharedApprovedDevices,
    pub otp_limiter: SharedOtpLimiter,
    pub server_ports: SharedServerPorts,
    pub disable_gpu_encode: Arc<std::sync::atomic::AtomicBool>,
    /// Which encoder the pipelines ask for, from `serve --encoder`. A process-lifetime CLI
    /// choice, not a user setting: not persisted, no Tauri command.
    pub encoder_vendor: Arc<Mutex<crate::streamer::config::EncoderVendor>>,
    pub cloud: Mutex<Option<CloudClient>>,
    pub cloud_status: Arc<Mutex<(String, String)>>,
    pub audio_hub: crate::streamer::audio::SharedAudioHub,
}

pub type SharedCloudStatus = Arc<Mutex<(String, String)>>;

#[derive(Debug)]
pub struct TauriCloudStatusSink {
    app: tauri::AppHandle,
    status: SharedCloudStatus,
}

impl TauriCloudStatusSink {
    pub fn new_shared(app: tauri::AppHandle, status: SharedCloudStatus) -> SharedCloudStatusSink {
        Arc::new(Self { app, status })
    }
}

impl CloudStatusSink for TauriCloudStatusSink {
    fn report(&self, state: CloudState, detail: String) {
        use tauri_specta::Event;
        *self.status.lock_recover() = (state.as_str().to_string(), detail.clone());
        let payload = crate::CloudStatusChange {
            state: state.as_str().to_string(),
            detail,
        };
        if let Err(e) = payload.emit(&self.app) {
            teprintln!("[cloud] failed to emit status event: {e:?}");
        }
    }

    fn session_rotated(&self, session_id: String) {
        use tauri_specta::Event;
        // only the id moved (the relay found ours taken): an empty `otp` tells the UI to keep
        // the code it is showing
        let payload = crate::SessionIdChange {
            session_id,
            otp: String::new(),
        };
        if let Err(e) = payload.emit(&self.app) {
            teprintln!("[cloud] failed to emit session id change event: {e:?}");
        }
    }
}

/// Give each attached ScreenExtend display that has no desktop of its own a source of its own.
///
/// Called by the join's attach poll when its display has not shown up as a new `\\.\DISPLAYn`,
/// under the name the macOS backend shares. It activates only our own driver's paths and passes
/// every other display's path through unchanged: never `SDC_TOPOLOGY_EXTEND` on the whole
/// machine.
pub fn set_display_topology_extend() -> bool {
    virtual_display::activate_driver_paths()
}

/// Check the driver's leak every ten minutes for as long as the host runs, saying so in the log
/// when it crosses the threshold (and again on every later check while it stays over).
fn watch_driver_health() {
    const EVERY: Duration = Duration::from_secs(600);
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("se-driver-health".into())
        .spawn(|| loop {
            std::thread::sleep(EVERY);
            driver_health::warn_if_over_threshold("periodic check");
        });
    if let Err(e) = spawned {
        teprintln!("[driver-health] could not start the periodic check: {e}");
    }
}

pub fn focus_main_window(window: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic,
        SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };

    let hwnd = match window.hwnd() {
        Ok(h) => HWND(h.0 as _),
        Err(e) => {
            teprintln!("[window] focus: failed to get hwnd: {e}");
            return;
        }
    };

    unsafe {
        let cmd = if IsIconic(hwnd).as_bool() {
            SW_RESTORE
        } else {
            SW_SHOW
        };
        let _ = ShowWindow(hwnd, cmd);

        let foreground = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(foreground, None);
        let this_thread = GetCurrentThreadId();
        let attach = fg_thread != 0
            && fg_thread != this_thread
            && AttachThreadInput(this_thread, fg_thread, true).as_bool();

        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);

        if attach {
            let _ = AttachThreadInput(this_thread, fg_thread, false);
        }
    }
}

/// The capture's one-time probes, paid at host start instead of inside the first join.
///
/// `wgc_features` is a process-wide `OnceLock` whose first caller runs the WinRT capability
/// queries while every other caller waits (50 s measured on a first join under CPU load). The
/// DWM check and the monitor enumeration keep no cache, but their first call loads what later
/// ones reuse. On its own thread, so neither `setup` nor a join waits for it.
fn warm_capture_probes() -> Option<std::thread::JoinHandle<()>> {
    let spawned = std::thread::Builder::new()
        .name("capture-warmup".into())
        .spawn(|| {
            let _com = com::ensure_for_this_thread("capture warm-up", com::Apartment::Mta);
            let started = std::time::Instant::now();
            let features = windows_capture::graphics_capture_api::wgc_features();
            let dwm = streamer::capture::check_dwm_composition().is_ok();
            let monitors = streamer::capture::monitor_device_names().len();
            tprintln!(
                "[setup] capture probes ready in {} ms (dwm composition: {dwm}, monitors: \
                 {monitors}, {})",
                started.elapsed().as_millis(),
                features.summary()
            );
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            teprintln!("[setup] could not start the capture warm-up thread ({e}); the first join pays for it");
            None
        }
    }
}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    if app_handle.try_state::<AppState>().is_some() {
        return true;
    }
    // give back a Mobile Hotspot a crashed run left configured, whichever way the host started
    hosted_network::spawn_crash_recovery(&app_handle);
    if let Some(dir) = hosted_network::handoff_dir(&app_handle) {
        drop(tauri::async_runtime::spawn_blocking(move || {
            hosted_network::sweep_passphrase_files(&dir)
        }));
    }
    let _ = warm_capture_probes();
    // the displays as they were before the host touched anything: the only "before" a later
    // report of a display that changed under the user can be checked against
    let _ = tauri::async_runtime::spawn_blocking(|| {
        virtual_display::log_display_inventory("at startup")
    })
    .await;

    // The user's own topology is logged and left alone: forcing `SDC_TOPOLOGY_EXTEND` switches
    // on monitors they turned off and undoes a duplicate desktop they chose. Our displays get a
    // desktop of their own when they attach (`set_display_topology_extend`).
    let _ = tauri::async_runtime::spawn_blocking(virtual_display::log_topology_at_startup).await;

    // read the driver's leak here because `serve` has no startup gate, then watch it: every
    // join and mode change grows it
    let _ = tauri::async_runtime::spawn_blocking(|| {
        driver_health::read_health();
        watch_driver_health();
    })
    .await;

    // made before the display backend, which reads the encoder choice a session will use
    let disable_gpu_encode = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let encoder_vendor = Arc::new(Mutex::new(crate::streamer::config::EncoderVendor::default()));
    let encoder = virtual_display::EncoderSource {
        vendor: encoder_vendor.clone(),
        software: disable_gpu_encode.clone(),
    };
    let virtual_display = tauri::async_runtime::spawn_blocking(move || {
        WindowsVirtualDisplay::new_shared_following(Some(encoder))
    })
    .await;

    let virtual_display = match virtual_display {
        Ok(Ok(vd)) => vd,
        Ok(Err(e)) => {
            teprintln!("[setup] cannot start: {e}");
            return false;
        }
        Err(e) => {
            teprintln!("[setup] the virtual-display task panicked: {e}");
            return false;
        }
    };

    let device_overrides: SharedDeviceOverrides = Arc::new(Mutex::new(HashMap::new()));
    let device_reporter: SharedDeviceReporter =
        TauriDeviceReporter::new_shared(app_handle.clone(), device_overrides.clone());
    let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));

    let state = AppState {
        virtual_display,
        stop_hosted_network: Mutex::new(None),
        hosted_network_running: Mutex::new(false),
        network_adapters: Mutex::new(Vec::new()),
        local_ips: session::new_shared_local_ips(),
        streamers: Mutex::new(HashMap::new()),
        session_auth: SessionAuth::default(),
        device_reporter,
        device_overrides,
        audio_sinks: session::new_shared_audio_sinks(),
        sessions,
        disconnect_grace: session::new_shared_disconnect_grace(),
        user_turn: session::new_shared_turn_config(),
        stun_urls: session::new_shared_stun_config(),
        banned_devices: session::new_shared_banned_devices(),
        approved_devices: session::new_shared_approved_devices(),
        otp_limiter: session::new_shared_otp_limiter(),
        server_ports: session::new_shared_server_ports(),
        disable_gpu_encode,
        encoder_vendor,
        cloud: Mutex::new(None),
        cloud_status: Arc::new(Mutex::new(("connecting".to_string(), String::new()))),
        audio_hub: crate::streamer::audio::AudioHub::new(),
    };
    // The saved STUN list, from the store the CLI writes. Absent keeps the built-in default; an
    // empty list is a deliberate "no STUN at all" (a LAN-only host).
    if let Some(urls) = crate::cli::saved_stun_urls(&app_handle) {
        if urls.is_empty() {
            tprintln!("[ice] STUN disabled by configuration; host candidates only");
        } else {
            tprintln!("[ice] STUN servers from configuration: {}", urls.join(", "));
        }
        *state.stun_urls.lock_recover() = urls;
    }

    app_handle.manage(state);
    true
}

#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub fn set_device_override(
    state: State<'_, AppState>,
    device: String,
    scale: u32,
    orientation: String,
    refresh_rate: u32,
    video_scale: u32,
    video_quality: u32,
    control_enabled: bool,
    dpr: f64,
    audio_enabled: bool,
) {
    use crate::streamer::config::ScalePercent;
    use crate::streamer::platform::max_display_dpr;
    use crate::streamer::server::{
        MAX_DISPLAY_SCALE, MAX_REFRESH_RATE, MIN_DISPLAY_SCALE, MIN_REFRESH_RATE,
    };

    let dpr = if dpr.is_finite() {
        dpr.clamp(1.0, max_display_dpr())
    } else {
        1.0
    };
    // the carried-over fields are read under ONE guard, before the struct literal: two
    // `lock_recover()` temporaries inside one literal both live to the end of the statement, and
    // the second deadlocks on the non-reentrant mutex
    let Some(token) = resolve_device(&state, &device) else {
        return;
    };
    let (carried_scope, carried_auto_frame_rate, carried_modes) = {
        let overrides = state.device_overrides.lock_recover();
        match overrides.get(&token) {
            Some(o) => (
                o.pointer_scope,
                o.auto_frame_rate,
                (o.adaptive_quality, o.native_dpr, o.auto_orientation),
            ),
            None => {
                let d = DeviceOverride::join_defaults();
                (
                    Default::default(),
                    false,
                    (d.adaptive_quality, d.native_dpr, d.auto_orientation),
                )
            }
        }
    };
    let next = DeviceOverride {
        scale: scale.clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE),
        orientation_portrait: orientation == "Portrait",
        refresh_rate: refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE),
        video_scale: ScalePercent::new(video_scale).percent(),
        video_quality: video_quality.clamp(1, 51) as u8,
        control_enabled,
        dpr,
        audio_enabled,
        // not arguments here (specta's ceiling is ten): their own commands set them, and
        // carrying them over stops an unrelated display change from resetting them
        pointer_scope: carried_scope,
        auto_frame_rate: carried_auto_frame_rate,
        adaptive_quality: carried_modes.0,
        native_dpr: carried_modes.1,
        auto_orientation: carried_modes.2,
    };
    // The action comes from comparing the saved row with the submitted one, not from the save:
    // the desktop UI writes every field on every save, and an epoch bump makes the client rebuild.
    // No saved row means no live session (the join path seeds one), so `Rebuild` reaches nothing.
    let action = {
        let mut overrides = state.device_overrides.lock_recover();
        let action = match overrides.get(&token) {
            Some(old) => DeviceOverride::reconfig_action(old, &next),
            None => ReconfigAction::Rebuild,
        };
        overrides.insert(token.clone(), next);
        action
    };
    // a save makes the refresh rate a setting, no longer the join's detection
    session::clear_join_seeded(&token);
    // Class (b), whatever else this edit did: remote control is read per event, so it lands on
    // the pointer that is moving now rather than on the next connection.
    session::apply_live_input_for_device(
        &state.sessions,
        &token,
        next.control_enabled,
        next.pointer_scope,
    );
    match action {
        // classes (a), (b) and (c): nothing reaches the wire, the dashboard or the client
        ReconfigAction::Nothing => {
            tprintln!("device {token} settings saved; nothing the live stream reads has changed");
        }
        // the extended desktop's DPI changes under a stream whose geometry is already correct
        ReconfigAction::ScaleLive => {
            let sessions = state.sessions.clone();
            let effective = ((next.dpr * next.scale as f64).round() as u32).clamp(
                MIN_DISPLAY_SCALE,
                crate::streamer::server::MAX_EFFECTIVE_SCALE,
            );
            tauri::async_runtime::spawn(async move {
                let applied = crate::streamer::server::apply_scale_live_for_device(
                    &sessions, &token, effective,
                )
                .await;
                if applied == 0 {
                    // the next join reads it from the override map
                    tprintln!(
                        "display scale for device {token} saved; no live display to apply it to"
                    );
                }
            });
        }
        // every tab the device has open (settings are per device); each renegotiates on its
        // own join id, so its display survives
        ReconfigAction::Rebuild => {
            let moved = session::bump_reconfig_epoch_for_device(&state.sessions, &token);
            tprintln!(
                "device {token} changed a stream parameter; {moved} live session(s) will \
                 renegotiate in place"
            );
        }
    }
}

/// Resolve a device token, or an address the caller happens to know, to the token the host
/// stores settings under.
///
/// An address is a convenience for the CLI and for a caller holding an old row: it resolves only
/// while exactly one device is connected from it. Anything else is refused and logged, never
/// guessed, or one device ends up with another's settings.
pub fn resolve_device(state: &AppState, key: &str) -> Option<String> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    if is_device_token(key) {
        return Some(key.to_string());
    }
    match session::token_for_ip(&state.sessions, key) {
        Some(token) => {
            tprintln!(
                "[devices] {key} is an address, not a device; resolved to the device connected \
                 from it. Pass the token to be sure which device you mean"
            );
            Some(token)
        }
        None => {
            teprintln!(
                "[devices] {key} names no device: an address only identifies one while exactly \
                 one device is connected from it, and addresses are recycled between devices"
            );
            None
        }
    }
}

/// What [`crate::streamer::session::mint_device_token`] produces: 128 bits as lowercase hex.
fn is_device_token(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Where a pointer-locked client may drive this host's cursor: `"all"` (every screen, including
/// the host's real monitors, the default) or `"display"` (only the screen it is streaming).
///
/// Its own command rather than another `set_device_override` argument: that one is at specta's
/// ten-argument ceiling, and this needs none of its machinery: no clamp, no rebuild verdict,
/// no live display to touch. The injector reads the scope when the session is built, so a
/// change applies to the next connection, exactly like remote control itself.
#[tauri::command]
#[specta::specta]
pub fn set_device_pointer_scope(state: State<'_, AppState>, device: String, scope: String) {
    let Some(token) = resolve_device(&state, &device) else {
        return;
    };
    let parsed = crate::streamer::input::PointerScope::from_str(&scope);
    let mut overrides = state.device_overrides.lock_recover();
    match overrides.get_mut(&token) {
        Some(existing) => existing.pointer_scope = parsed,
        None => {
            overrides.insert(
                token.clone(),
                DeviceOverride {
                    pointer_scope: parsed,
                    ..DeviceOverride::join_defaults()
                },
            );
        }
    }
    let control_enabled = overrides
        .get(&token)
        .map(|o| o.control_enabled)
        .unwrap_or(true);
    drop(overrides);
    // class (b): straight into every live injector this device drives. No epoch: nothing the
    // encoder or the client reads has moved.
    let reached =
        session::apply_live_input_for_device(&state.sessions, &token, control_enabled, parsed);
    tprintln!(
        "[input] pointer scope for device {token} is now {} ({reached} live session(s))",
        parsed.as_str()
    );
}

/// Whether this device's stream may drop below its chosen refresh rate on its own.
///
/// Off is the default: the rate the user picked is the rate the stream
/// runs at, an unchanged frame is repeated rather than skipped, and no controller lowers it.
/// A separate command for the same reason as `set_device_pointer_scope` (`set_device_override`
/// is at specta's ten-argument ceiling) and read by the pipeline when the session is built, so
/// a change applies to the next connection.
#[tauri::command]
#[specta::specta]
pub fn set_device_auto_frame_rate(state: State<'_, AppState>, device: String, enabled: bool) {
    let Some(token) = resolve_device(&state, &device) else {
        return;
    };
    let mut overrides = state.device_overrides.lock_recover();
    let changed = match overrides.get_mut(&token) {
        Some(existing) => {
            let moved = existing.auto_frame_rate != enabled;
            existing.auto_frame_rate = enabled;
            moved
        }
        None => {
            overrides.insert(
                token.clone(),
                DeviceOverride {
                    auto_frame_rate: enabled,
                    ..DeviceOverride::join_defaults()
                },
            );
            enabled != DeviceOverride::join_defaults().auto_frame_rate
        }
    };
    drop(overrides);
    // class (d): read when the session is built (the display's compose rate is chosen from it),
    // so it takes a rebuild, but only when it moved
    if changed {
        let moved = session::bump_reconfig_epoch_for_device(&state.sessions, &token);
        tprintln!(
            "[stream] automatic frame rate for device {token} is now {}; {moved} live \
             session(s) will renegotiate in place",
            if enabled { "on" } else { "off" }
        );
    } else {
        tprintln!(
            "[stream] automatic frame rate for device {token} is already {}",
            if enabled { "on" } else { "off" }
        );
    }
}

/// The three adaptive switches, one per setting that has a fixed form and an adaptive one:
/// quality (CBR with the slider as a QP floor, instead of constant QP at the slider), pixel
/// ratio (the ratio the device reports at each join, instead of the slider) and orientation
/// (whichever way the device is held, always, instead of the saved value). Off is the default
/// for all three and means "fixed at what the sheet shows", the same rule as
/// `set_device_auto_frame_rate`. Their own command for the same reason as that one:
/// `set_device_override` is at specta's ten-argument ceiling. Each is read when the session is
/// built, so a change takes a rebuild (only when something actually moved).
#[tauri::command]
#[specta::specta]
pub fn set_device_adaptive_modes(
    state: State<'_, AppState>,
    device: String,
    adaptive_quality: bool,
    native_dpr: bool,
    auto_orientation: bool,
) {
    let Some(token) = resolve_device(&state, &device) else {
        return;
    };
    let mut overrides = state.device_overrides.lock_recover();
    let changed = match overrides.get_mut(&token) {
        Some(existing) => {
            let moved = existing.adaptive_quality != adaptive_quality
                || existing.native_dpr != native_dpr
                || existing.auto_orientation != auto_orientation;
            existing.adaptive_quality = adaptive_quality;
            existing.native_dpr = native_dpr;
            existing.auto_orientation = auto_orientation;
            moved
        }
        None => {
            let defaults = DeviceOverride::join_defaults();
            overrides.insert(
                token.clone(),
                DeviceOverride {
                    adaptive_quality,
                    native_dpr,
                    auto_orientation,
                    ..defaults
                },
            );
            adaptive_quality != defaults.adaptive_quality
                || native_dpr != defaults.native_dpr
                || auto_orientation != defaults.auto_orientation
        }
    };
    drop(overrides);
    // class (d): all three are read when the session is built
    if changed {
        let moved = session::bump_reconfig_epoch_for_device(&state.sessions, &token);
        tprintln!(
            "[stream] adaptive modes for device {token}: quality={adaptive_quality} \
             pixel-ratio={native_dpr} orientation={auto_orientation}; {moved} live session(s) \
             will renegotiate in place"
        );
    } else {
        tprintln!("[stream] adaptive modes for device {token} are unchanged");
    }
}

#[tauri::command]
#[specta::specta]
pub fn refresh_device_stream(
    state: State<'_, AppState>,
    device: String,
    join_id: Option<String>,
) -> bool {
    // a join id refreshes that one tab; without one, every tab the device has open
    let joins: Vec<String> = match join_id.filter(|j| !j.trim().is_empty()) {
        Some(join) => vec![join],
        None => match resolve_device(&state, &device) {
            Some(token) => session::joins_for_token(&state.sessions, &token),
            None => Vec::new(),
        },
    };
    let mut refreshed = 0usize;
    for join in &joins {
        if session::get_live_display(&state.sessions, join).is_none() {
            tprintln!("stream refresh skipped: join {join} has no live display attached");
            continue;
        }
        session::bump_reconfig_epoch(&state.sessions, join);
        tprintln!("stream refresh requested for join {join}; it renegotiates on its next poll");
        refreshed += 1;
    }
    refreshed > 0
}

/// End one live session without touching the device's settings or its trust.
///
/// The per-join counterpart to `remove_device_override`: a device with two tabs open can have one
/// of them disconnected.
#[tauri::command]
#[specta::specta]
pub fn disconnect_session(state: State<'_, AppState>, join_id: String) -> bool {
    if session::get_live_display(&state.sessions, &join_id).is_none()
        && session::device_token_for(&state.sessions, &join_id).is_none()
    {
        tprintln!("disconnect skipped: no session named {join_id}");
        return false;
    }
    session::kick_join(&state.sessions, &join_id, session::ByeReason::Kicked);
    tprintln!("join {join_id} disconnected by the host");
    true
}

#[tauri::command]
#[specta::specta]
pub fn set_device_audio_output(state: State<'_, AppState>, device: String, device_id: String) {
    let Some(token) = resolve_device(&state, &device) else {
        return;
    };
    let sel = if device_id.trim().is_empty() {
        None
    } else {
        Some(device_id)
    };
    // Per device: two tabs of one browser share one set of speakers, and the choice outlives
    // every join so `serve` can replay it into a host with nothing connected yet.
    // Class (c), so no epoch: `audioSink` rides in every `/reconfig` body (the client applies it
    // on its next poll, about 1 s), and the device sheet sends this on every save, moved or not.
    session::set_device_audio_sink(&state.audio_sinks, &token, sel);
}

#[tauri::command]
#[specta::specta]
pub fn get_device_audio_outputs(
    state: State<'_, AppState>,
    device: String,
) -> crate::streamer::session::AudioOutputsReport {
    match resolve_device(&state, &device) {
        Some(token) => {
            session::device_audio_outputs_report(&state.sessions, &state.audio_sinks, &token)
        }
        None => Default::default(),
    }
}

#[tauri::command]
#[specta::specta]
pub fn remove_device_override(state: State<'_, AppState>, device: String) {
    let Some(token) = resolve_device(&state, &device) else {
        return;
    };
    state.device_overrides.lock_recover().remove(&token);
    state.audio_sinks.lock_recover().remove(&token);
    let ended = session::kick_device(&state.sessions, &token, session::ByeReason::Kicked);
    tprintln!("device {token} removed; {ended} live session(s) ended");
}

#[tauri::command]
#[specta::specta]
pub fn set_device_banned(state: State<'_, AppState>, token: String, ip: String, banned: bool) {
    // by token, never by address: a banned phone's address is handed to the next device that
    // asks for one. `ip` is a label for the log and the last-resort lookup for an old row.
    let Some(key) = resolve_device(&state, if token.trim().is_empty() { &ip } else { &token })
    else {
        teprintln!("[devices] cannot ban {ip}: it has never joined, so there is no device to ban");
        return;
    };
    if banned {
        state.banned_devices.lock_recover().insert(key.clone());
        let ended = session::kick_device(&state.sessions, &key, session::ByeReason::Kicked);
        tprintln!("device {key} banned; {ended} live session(s) kicked");
    } else {
        state.banned_devices.lock_recover().remove(&key);
        tprintln!("device {key} unbanned");
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_device_approved(state: State<'_, AppState>, token: String, approved: bool) {
    if token.trim().is_empty() {
        return;
    }
    if approved {
        state.approved_devices.lock_recover().insert(token);
        tprintln!("device token approved for auto-join");
    } else {
        state.approved_devices.lock_recover().remove(&token);
        tprintln!("device token auto-join approval revoked");
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_disconnect_grace(state: State<'_, AppState>, seconds: u32) {
    let secs = (seconds as u64).clamp(
        session::MIN_DISCONNECT_GRACE_SECS,
        session::MAX_DISCONNECT_GRACE_SECS,
    );
    state
        .disconnect_grace
        .store(secs, std::sync::atomic::Ordering::Relaxed);
    tprintln!("disconnect grace set to {secs}s");
}

#[tauri::command]
#[specta::specta]
pub fn get_disconnect_grace(state: State<'_, AppState>) -> u32 {
    state
        .disconnect_grace
        .load(std::sync::atomic::Ordering::Relaxed) as u32
}

#[tauri::command]
#[specta::specta]
pub fn get_display_capacity(state: State<'_, AppState>) -> DisplayCapacity {
    // `shared_display_cap` caps only the macOS AirPlay tier, so `full` is always false here
    let max = session::shared_display_cap(&state.virtual_display);
    let in_use = session::live_display_count(&state.sessions);
    DisplayCapacity {
        max: max.map(|m| m as u32),
        in_use: in_use as u32,
        full: max.is_some_and(|m| in_use >= m),
        // cached: a sync command on the UI's main thread, polled by the dashboard, and a fresh
        // probe of an unreachable driver is a connect plus a 5 s handshake
        backend: virtual_display::probe_display_backend_cached()
            .as_str()
            .to_string(),
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, specta::Type)]
pub struct TurnConfig {
    pub urls: String,
    pub username: String,
    pub credential: String,
}

#[tauri::command]
#[specta::specta]
pub fn set_turn_config(
    state: State<'_, AppState>,
    urls: String,
    username: String,
    credential: String,
) {
    let urls: Vec<String> = urls
        .split(',')
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect();
    let enabled = !urls.is_empty();
    *state.user_turn.lock_recover() = UserTurnConfig {
        urls,
        username: username.trim().to_string(),
        credential: credential.trim().to_string(),
    };
    if enabled {
        tprintln!("TURN relay configured from settings");
    } else {
        tprintln!("TURN relay cleared (none configured)");
    }
}

#[tauri::command]
#[specta::specta]
pub fn get_turn_config(state: State<'_, AppState>) -> TurnConfig {
    let cfg = state.user_turn.lock_recover();
    TurnConfig {
        urls: cfg.urls.join(","),
        username: cfg.username.clone(),
        credential: cfg.credential.clone(),
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, specta::Type)]
pub struct ServerPorts {
    pub http: u16,
    pub https: u16,
}

#[tauri::command]
#[specta::specta]
pub fn get_server_ports(state: State<'_, AppState>) -> ServerPorts {
    let (http, https) = state.server_ports.get();
    ServerPorts { http, https }
}

/// Run a blocking body that `#[tauri::command(async)]` has placed on a worker of Tauri's async
/// runtime, telling the runtime first so it hands that worker's other tasks to the rest.
///
/// The attribute calls a sync command inside an `async move` block on the runtime, so a body that
/// blocks for seconds (an elevated install waits up to `DRIVER_COMMAND_TIMEOUT`) would park a
/// worker other IPC futures are queued behind. Outside a multi-thread runtime (the CLI and
/// `doctor` call these synchronously, and `block_in_place` panics on a current-thread runtime)
/// the body simply runs.
pub(crate) fn blocking_off_the_workers<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// `async` so the restart below (up to 500 ms of graceful shutdown and 2 s waiting for the
/// ports) runs on the async runtime, not on the UI's main thread.
#[tauri::command(async)]
#[specta::specta]
pub fn set_server_ports(
    state: State<'_, AppState>,
    http_port: u16,
    https_port: u16,
) -> ServerPorts {
    blocking_off_the_workers(|| set_server_ports_blocking(&state, http_port, https_port))
}

fn set_server_ports_blocking(state: &AppState, http_port: u16, https_port: u16) -> ServerPorts {
    let http = if http_port == 0 {
        session::DEFAULT_HTTP_PORT
    } else {
        http_port
    };
    let https = if https_port == 0 {
        session::DEFAULT_HTTPS_PORT
    } else {
        https_port
    };

    let (cur_http, cur_https) = state.server_ports.get();
    if http == https {
        teprintln!("[streamer] rejecting server port change: HTTP and HTTPS must differ ({http})");
        return ServerPorts {
            http: cur_http,
            https: cur_https,
        };
    }
    if http == cur_http && https == cur_https {
        return ServerPorts { http, https };
    }

    state.server_ports.set(http, https);
    tprintln!(
        "[streamer] server ports changed to HTTP :{http}, HTTPS :{https}; restarting streamers"
    );

    restart_streamers(state, "port change");

    ServerPorts { http, https }
}

/// Give a stopping server this long to release its listening socket before starting the new one.
const PORT_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// Stop every per-adapter server and start it again, for a setting a live server cannot take
/// (the listening ports).
fn restart_streamers(state: &AppState, reason: &str) {
    // bye before the socket dies, so a client sees a reason, not a dead transport
    session::broadcast_bye(
        &state.sessions,
        session::ByeReason::HostExit,
        Duration::from_millis(500),
    );

    {
        let mut streamers = state.streamers.lock_recover();
        for (ip, streamer) in streamers.drain() {
            tprintln!("[streamer] stopping streamer bound to {ip} for {reason}");
            streamer.stop(Duration::from_millis(500));
        }
    }

    let (http, https) = state.server_ports.get();
    if !wait_for_ports_free(state, http, https, PORT_RELEASE_TIMEOUT) {
        teprintln!(
            "[streamer] ports {http}/{https} were still held after {PORT_RELEASE_TIMEOUT:?}; \
             starting anyway, watch for a bind failure below"
        );
    }
    sync_streamers(state);
}

/// Poll the adapter addresses about to be bound until they accept a listener again. A fixed
/// sleep is either wasted or, silently, too short.
fn wait_for_ports_free(state: &AppState, http: u16, https: u16, timeout: Duration) -> bool {
    use std::net::TcpListener;

    let ips: Vec<Ipv4Addr> = state
        .network_adapters
        .lock()
        .unwrap()
        .iter()
        .flat_map(|adapter| adapter.ip_addresses.iter())
        .filter_map(|ip| ip.parse::<Ipv4Addr>().ok())
        .collect();
    if ips.is_empty() {
        return true;
    }

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let free = ips.iter().all(|ip| {
            TcpListener::bind((*ip, http)).is_ok() && TcpListener::bind((*ip, https)).is_ok()
        });
        if free {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[tauri::command]
#[specta::specta]
pub fn get_disable_gpu_encode(state: State<'_, AppState>) -> bool {
    state
        .disable_gpu_encode
        .load(std::sync::atomic::Ordering::Relaxed)
}

#[tauri::command]
#[specta::specta]
pub fn set_disable_gpu_encode(state: State<'_, AppState>, disabled: bool) {
    let prev = state
        .disable_gpu_encode
        .swap(disabled, std::sync::atomic::Ordering::Relaxed);
    if prev == disabled {
        return;
    }
    // the pipeline reads this flag live (`Config::software_encode_now`) when it builds a
    // session, so no server restart (it would drop every session): each live join renegotiates
    // in place, same join and display, new encoder
    let moved = session::bump_reconfig_epoch_all(&state.sessions);
    tprintln!(
        "[streamer] GPU video encoding {}; {moved} live session(s) will renegotiate in place",
        if disabled {
            "disabled (software x264 only)"
        } else {
            "re-enabled"
        }
    );
}

/// Re-invoke ourselves elevated to run one of the legacy driver subcommands, and wait for it.
///
/// Not through `elevated-command`: its Windows backend is a bare `ShellExecuteW("runas")` that
/// neither waits for the child nor returns its exit code, so a reinstall would run two `nefconc`
/// on one devnode. Exit 0 from the legacy subcommand is the only success.
fn run_elevated_driver_command(app: &tauri::AppHandle, subcommand: &str) -> bool {
    // parent the prompt to the window, when there is one, so it comes up in front of it
    let owner = app
        .get_webview_window("main")
        .and_then(|w| w.hwnd().ok())
        .map(|h| h.0 as isize);
    tprintln!("[drivers] asking for elevation to run `{subcommand}`");
    match driver_health::run_elevated(
        subcommand,
        owner,
        driver_health::DRIVER_COMMAND_TIMEOUT,
        None,
    ) {
        driver_health::Elevated::Exited(0) => {
            tprintln!("[drivers] {subcommand} finished");
            true
        }
        driver_health::Elevated::Exited(code) => {
            teprintln!("[drivers] {subcommand} exited with {code}");
            false
        }
        driver_health::Elevated::Declined => {
            teprintln!("[drivers] {subcommand}: the administrator prompt was dismissed");
            false
        }
        driver_health::Elevated::Failed(e) => {
            teprintln!("[drivers] {subcommand} could not run elevated: {e}");
            false
        }
        driver_health::Elevated::Busy => {
            teprintln!(
                "[drivers] {subcommand} not started: an earlier elevated driver operation from \
                 this app is still running"
            );
            false
        }
        driver_health::Elevated::TimedOut => {
            teprintln!(
                "[drivers] {subcommand} did not finish within {:?}; it is still running and is \
                 being watched to the end",
                driver_health::DRIVER_COMMAND_TIMEOUT
            );
            false
        }
    }
}

/// How long [`install_drivers`] gives the freshly installed driver to start serving its pipe.
/// The devnode is created and the driver bound inside the elevated child; the UMDF host and the
/// pipe come up asynchronously after it exits.
const DRIVER_READY_WAIT: Duration = Duration::from_secs(10);

/// Install the driver elevated, wait for the install, and report whether the driver now answers.
///
/// `async` in the Tauri sense: it runs on the async runtime, not on the UI's main thread, which
/// would otherwise be frozen for the whole elevated install. Still a plain function for the CLI.
#[tauri::command(async)]
#[specta::specta]
pub fn install_drivers(app: tauri::AppHandle) -> bool {
    blocking_off_the_workers(|| install_drivers_blocking(&app))
}

fn install_drivers_blocking(app: &tauri::AppHandle) -> bool {
    if !run_elevated_driver_command(app, "installdrivers") {
        return false;
    }
    // report what the machine now has; a fresh probe, since this process's "connected" cache
    // predates the install
    let deadline = std::time::Instant::now() + DRIVER_READY_WAIT;
    let backend = loop {
        let backend = virtual_display::probe_display_backend_fresh();
        if backend == virtual_display::DisplayBackend::IddDriver
            || std::time::Instant::now() >= deadline
        {
            break backend;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    tprintln!(
        "[drivers] after install, display backend is {}",
        backend.as_str()
    );
    backend == virtual_display::DisplayBackend::IddDriver
}

/// Remove the driver elevated and wait for the removal, so an install that follows cannot race
/// it. Runs off the UI's main thread, like [`install_drivers`].
#[tauri::command(async)]
#[specta::specta]
pub fn remove_drivers(app: tauri::AppHandle) -> bool {
    blocking_off_the_workers(|| run_elevated_driver_command(&app, "removedrivers"))
}

/// How much the virtual display driver's host has leaked, read unelevated. The startup gate
/// asks this before `setup` and offers [`restart_display_driver`] when it is over threshold.
#[tauri::command]
#[specta::specta]
pub fn display_driver_health() -> crate::DisplayDriverHealth {
    driver_health::read_health()
}

/// Run `ScreenExtend drivers restart` elevated (one UAC prompt), wait for it, and say how it
/// went. Off the main thread: the prompt and the restart take seconds.
#[tauri::command]
#[specta::specta]
pub async fn restart_display_driver(app: tauri::AppHandle) -> crate::DriverRestartOutcome {
    // parent the prompt to the window so it comes up in front of it, not behind
    let owner = app
        .get_webview_window("main")
        .and_then(|w| w.hwnd().ok())
        .map(|h| h.0 as isize);
    match tauri::async_runtime::spawn_blocking(move || driver_health::restart_elevated(owner)).await
    {
        Ok(outcome) => outcome,
        Err(e) => crate::DriverRestartOutcome {
            reason: "failed".to_string(),
            detail: format!("the restart task did not complete: {e}"),
            ..Default::default()
        },
    }
}

#[tauri::command]
#[specta::specta]
pub fn install_audio_driver(_app: tauri::AppHandle) -> String {
    "unsupported".to_string()
}

#[tauri::command]
#[specta::specta]
pub fn uninstall_audio_driver() -> String {
    "unsupported".to_string()
}

#[tauri::command]
#[specta::specta]
pub fn audio_driver_status() -> String {
    "unsupported".to_string()
}

#[tauri::command]
#[specta::specta]
pub fn set_legacy_volume_key_proxy(_enabled: bool) {}

pub fn remove_all_displays(client: &SharedVirtualDisplay) {
    let client = client.clone();
    let _ = std::thread::spawn(move || client.remove_all_displays()).join();
}

#[tauri::command]
#[specta::specta]
pub fn set_session_credentials(state: State<'_, AppState>, session_id: String, otp: String) {
    *state.session_auth.session_id.lock_recover() = session_id;
    *state.session_auth.otp.lock_recover() = otp;
    state.otp_limiter.reset();
}

#[tauri::command]
#[specta::specta]
pub fn register_cloud_session(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) {
    let server_config = Config {
        virtual_display: Some(state.virtual_display.clone()),
        session_auth: Some(state.session_auth.clone()),
        device_reporter: Some(state.device_reporter.clone()),
        device_overrides: Some(state.device_overrides.clone()),
        sessions: Some(state.sessions.clone()),
        audio_sinks: Some(state.audio_sinks.clone()),
        disconnect_grace: Some(state.disconnect_grace.clone()),
        user_turn: Some(state.user_turn.clone()),
        stun_urls: state.stun_urls.lock_recover().clone(),
        banned_devices: Some(state.banned_devices.clone()),
        approved_devices: Some(state.approved_devices.clone()),
        otp_limiter: Some(state.otp_limiter.clone()),
        audio_hub: Some(state.audio_hub.clone()),
        disable_gpu_encode: state
            .disable_gpu_encode
            .load(std::sync::atomic::Ordering::Relaxed),
        gpu_encode_toggle: Some(state.disable_gpu_encode.clone()),
        encoder_vendor: *state.encoder_vendor.lock_recover(),
        ..Config::default()
    };
    // The old client is taken out under the lock and stopped outside it: `stop` joins its thread,
    // whose runtime waits for outstanding blocking tasks (a relayed join's attach poll), and
    // `live_session_info` on the control thread takes this same lock. It is stopped before the
    // new one starts, so two relay clients never register at once.
    let previous = state.cloud.lock_recover().take();
    if let Some(mut prev) = previous {
        prev.stop();
    }
    *state.cloud_status.lock_recover() = ("connecting".to_string(), String::new());
    let sink = TauriCloudStatusSink::new_shared(app, state.cloud_status.clone());
    let client = CloudClient::spawn(CloudConfig::new(session_id), server_config, sink);
    let displaced = state.cloud.lock_recover().replace(client);
    if let Some(mut other) = displaced {
        // a concurrent registration got in between the two locks; keep the newest
        other.stop();
    }
}

#[tauri::command]
#[specta::specta]
pub fn unregister_cloud_session(state: State<'_, AppState>) {
    let previous = state.cloud.lock_recover().take();
    if let Some(mut prev) = previous {
        prev.stop();
        tprintln!("[cloud] public sessions disabled; relay client stopped");
    }
    *state.cloud_status.lock_recover() = (CloudState::Disabled.as_str().to_string(), String::new());
}

#[tauri::command]
#[specta::specta]
pub fn get_cloud_status(state: State<'_, AppState>) -> crate::CloudStatusChange {
    let (status, detail) = state.cloud_status.lock_recover().clone();
    crate::CloudStatusChange {
        state: status,
        detail,
    }
}

pub fn sync_streamers(state: &AppState) {
    let (desired, fresh_local_ips) = {
        let adapters = state.network_adapters.lock_recover();
        let desired: Vec<(String, Ipv4Addr)> = adapters
            .iter()
            .flat_map(|adapter| adapter.ip_addresses.iter())
            .filter_map(|ip| ip.parse::<Ipv4Addr>().ok().map(|addr| (ip.clone(), addr)))
            .collect();
        // IPv4 only: say so for an adapter that has addresses but no IPv4 one
        for adapter in adapters.iter() {
            if !adapter.ip_addresses.is_empty()
                && !adapter
                    .ip_addresses
                    .iter()
                    .any(|ip| ip.parse::<Ipv4Addr>().is_ok())
            {
                tprintln!(
                    "[streamer] adapter '{}' has only IPv6 addresses ({:?}); no server will be bound on it",
                    adapter.network_name,
                    adapter.ip_addresses
                );
            }
        }
        let fresh_local_ips = crate::streamer::server::collect_local_ips(
            adapters
                .iter()
                .flat_map(|adapter| adapter.ip_addresses.iter().map(String::as_str)),
        );
        (desired, fresh_local_ips)
    };

    *state.local_ips.lock_recover() = fresh_local_ips;

    let (http_port, https_port) = state.server_ports.get();

    let mut streamers = state.streamers.lock_recover();

    streamers.retain(|ip, streamer| {
        if desired.iter().any(|(desired_ip, _)| desired_ip == ip) {
            true
        } else {
            tprintln!("[streamer] stopping streamer bound to {ip}");
            streamer.stop(Duration::from_secs(1));
            false
        }
    });

    for (ip, addr) in desired {
        if streamers.contains_key(&ip) {
            continue;
        }

        let handle = axum_server::Handle::new();
        let config = Config {
            bind_ip: IpAddr::V4(addr),
            lan_ip: Some(ip.clone()),
            local_ips: Some(state.local_ips.clone()),
            port: http_port,
            https_port,
            virtual_display: Some(state.virtual_display.clone()),
            session_auth: Some(state.session_auth.clone()),
            device_reporter: Some(state.device_reporter.clone()),
            device_overrides: Some(state.device_overrides.clone()),
            sessions: Some(state.sessions.clone()),
            audio_sinks: Some(state.audio_sinks.clone()),
            disconnect_grace: Some(state.disconnect_grace.clone()),
            user_turn: Some(state.user_turn.clone()),
            stun_urls: state.stun_urls.lock_recover().clone(),
            banned_devices: Some(state.banned_devices.clone()),
            approved_devices: Some(state.approved_devices.clone()),
            otp_limiter: Some(state.otp_limiter.clone()),
            audio_hub: Some(state.audio_hub.clone()),
            disable_gpu_encode: state
                .disable_gpu_encode
                .load(std::sync::atomic::Ordering::Relaxed),
            gpu_encode_toggle: Some(state.disable_gpu_encode.clone()),
            encoder_vendor: *state.encoder_vendor.lock_recover(),
            ..Config::default()
        };

        let thread_handle = handle.clone();
        let ip_for_log = ip.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_stopping = stopping.clone();
        // one thread per adapter, all driving the one shared transport runtime
        // (`streamer::transport_runtime`)
        let spawned = std::thread::Builder::new()
            .name(format!("se-adapter-{ip}"))
            .spawn(move || {
                let mut failures = 0u32;
                loop {
                    // the same handle every time: it remembers a shutdown requested while no
                    // server was listening, so the next attempt stops at once
                    let result =
                        Streamer::new(config.clone()).run_with_handle(thread_handle.clone());
                    if thread_stopping.load(Ordering::Acquire) {
                        return;
                    }
                    let Err(e) = result else {
                        return;
                    };
                    failures = failures.saturating_add(1);
                    let wait = rebind_delay(failures);
                    teprintln!(
                        "[streamer] streamer bound to {ip_for_log} exited: {e}; trying again in \
                         {wait:?} (attempt {})",
                        failures + 1
                    );
                    let until = std::time::Instant::now() + wait;
                    while std::time::Instant::now() < until {
                        if thread_stopping.load(Ordering::Acquire) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            });
        if let Err(e) = spawned {
            teprintln!("[streamer] could not start a streamer for {ip}: {e}");
            continue;
        }

        tprintln!("[streamer] started streamer bound to {ip}");
        streamers.insert(ip, StreamerHandle { handle, stopping });
    }
}

#[cfg(test)]
mod blocking_tests {
    use super::blocking_off_the_workers;
    use std::time::Duration;

    #[test]
    fn the_capture_warm_up_runs_off_the_caller_and_finishes() {
        let handle = super::warm_capture_probes().expect("a warm-up thread");
        assert_ne!(handle.thread().id(), std::thread::current().id());
        handle.join().expect("the warm-up does not panic");
    }

    /// The body runs wherever a command is called from: plain (the CLI), inside a
    /// current-thread runtime (where `block_in_place` would panic) and on a multi-thread worker.
    #[test]
    fn a_blocking_command_body_runs_from_anywhere() {
        assert_eq!(blocking_off_the_workers(|| 1), 1);
        let current = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        assert_eq!(
            current.block_on(async { blocking_off_the_workers(|| 2) }),
            2
        );
    }

    /// A body that blocks on a worker of a multi-thread runtime must not hold up the tasks queued
    /// behind it. One worker, and the blocked body waits on a task that needs a worker to run:
    /// without handing the worker off this can only time out.
    #[test]
    fn a_blocking_command_body_does_not_hold_a_worker() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let got = rt.block_on(async {
            let (tx, rx) = std::sync::mpsc::channel::<u32>();
            let body = tokio::spawn(async move {
                blocking_off_the_workers(|| rx.recv_timeout(Duration::from_secs(5)).ok())
            });
            tokio::spawn(async move {
                let _ = tx.send(7);
            });
            body.await.expect("the body task")
        });
        assert_eq!(got, Some(7));
    }
}

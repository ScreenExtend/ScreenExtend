pub mod audio;
pub mod com;
pub mod compatibility;
pub mod device_reporter;
pub mod driver_ipc;
pub mod hosted_network;
pub mod networking;
pub mod permissions;
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
use elevated_command::Command;
use networking::NetworkInfo;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::path::BaseDirectory;
use tauri::Manager;
use tauri::State;
use virtual_display::WindowsVirtualDisplay;

pub struct StreamerHandle {
    handle: axum_server::Handle,
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
    /// Which encoder the pipelines should ask for, from `serve --encoder`. A process-lifetime
    /// CLI choice rather than a user setting, so it is not persisted and has no Tauri command;
    /// the two places that build a streaming `Config` read it when they build one.
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
        *self.status.lock().unwrap() = (state.as_str().to_string(), detail.clone());
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
        // Only the id moved: the relay found this one taken and picked another. An empty `otp`
        // says so, and the UI keeps the code it is already showing.
        let payload = crate::SessionIdChange {
            session_id,
            otp: String::new(),
        };
        if let Err(e) = payload.emit(&self.app) {
            teprintln!("[cloud] failed to emit session id change event: {e:?}");
        }
    }
}

pub fn set_display_topology_extend() -> bool {
    use windows::Win32::Devices::Display::{
        GetDisplayConfigBufferSizes, QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_PATH_INFO, QDC_ALL_PATHS, SDC_APPLY, SDC_TOPOLOGY_EXTEND,
    };

    const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;

    fn source_key(p: &DISPLAYCONFIG_PATH_INFO) -> (u32, i32, u32) {
        (
            p.sourceInfo.adapterId.LowPart,
            p.sourceInfo.adapterId.HighPart,
            p.sourceInfo.id,
        )
    }
    fn target_key(p: &DISPLAYCONFIG_PATH_INFO) -> (u32, i32, u32) {
        (
            p.targetInfo.adapterId.LowPart,
            p.targetInfo.adapterId.HighPart,
            p.targetInfo.id,
        )
    }

    let mut path_count = 0u32;
    let mut mode_count = 0u32;
    if let Err(e) =
        unsafe { GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut path_count, &mut mode_count) }.ok()
    {
        teprintln!("[display] extend: GetDisplayConfigBufferSizes failed ({e})");
        return false;
    }

    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    if let Err(e) = unsafe {
        QueryDisplayConfig(
            QDC_ALL_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    }
    .ok()
    {
        teprintln!("[display] extend: QueryDisplayConfig failed ({e})");
        return false;
    }
    paths.truncate(path_count as usize);

    let active: Vec<&DISPLAYCONFIG_PATH_INFO> = paths
        .iter()
        .filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0)
        .collect();

    let active_targets: std::collections::HashSet<_> =
        active.iter().map(|p| target_key(p)).collect();
    let active_sources: std::collections::HashSet<_> =
        active.iter().map(|p| source_key(p)).collect();
    let is_cloned = active_targets.len() >= 2 && active_sources.len() < active_targets.len();

    let has_inactive_display = paths.iter().any(|p| {
        let inactive = p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0;
        let available = p.targetInfo.targetAvailable.as_bool();
        inactive && available && !active_targets.contains(&target_key(p))
    });

    if !is_cloned && !has_inactive_display {
        tprintln!(
            "[display] already extended ({} monitor(s), {} source(s)); nothing to do",
            active_targets.len(),
            active_sources.len()
        );
        return false;
    }

    let reason = if is_cloned {
        "duplicate/clone topology"
    } else {
        "an inactive display"
    };
    tprintln!(
        "[display] {reason} detected ({} active monitor(s)); applying extend topology",
        active_targets.len()
    );

    let result = unsafe {
        SetDisplayConfig(
            None::<&[DISPLAYCONFIG_PATH_INFO]>,
            None::<&[DISPLAYCONFIG_MODE_INFO]>,
            SDC_TOPOLOGY_EXTEND | SDC_APPLY,
        )
    };
    if result == 0 {
        tprintln!("[display] extend topology applied");
        return true;
    }
    teprintln!("[display] failed to apply extend topology (SetDisplayConfig win32 error {result})");
    false
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

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    if app_handle.try_state::<AppState>().is_some() {
        return true;
    }
    // What was on this machine before the host touched anything. One `QueryDisplayConfig`, and it
    // is the only "before" a later report of a display that changed under the user can be checked
    // against — the alternative is a snapshot taken afterwards and a guess at what preceded it.
    let _ = tauri::async_runtime::spawn_blocking(|| {
        virtual_display::log_display_inventory("at startup")
    })
    .await;

    // Extend the desktop *before* touching the driver. These are two concurrent reconfigurations
    // of the same OS state otherwise: `set_display_topology_extend` decides what to apply from a
    // snapshot of the current paths, while `new_shared_reported` removes every virtual monitor.
    match tauri::async_runtime::spawn_blocking(set_display_topology_extend).await {
        Ok(true) => tprintln!("[display] desktop topology set to extend"),
        Ok(false) => {}
        Err(e) => teprintln!("[display] topology task failed: {e}"),
    }

    let virtual_display =
        tauri::async_runtime::spawn_blocking(WindowsVirtualDisplay::new_shared_reported).await;

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
        disable_gpu_encode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        encoder_vendor: Arc::new(Mutex::new(crate::streamer::config::EncoderVendor::default())),
        cloud: Mutex::new(None),
        cloud_status: Arc::new(Mutex::new(("connecting".to_string(), String::new()))),
        audio_hub: crate::streamer::audio::AudioHub::new(),
    };
    // The saved STUN list, read from the same store the CLI writes, so `stun set`, `serve` and
    // the desktop app all agree. Absent means "keep the built-in default"; an empty list is the
    // deliberate "no STUN at all", which a LAN-only host has every reason to choose.
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
    // Both of the carried-over fields are read under **one** guard, before the struct literal.
    // Two `lock_recover()` calls inside one struct literal are two guards alive at the same time
    // on the same non-reentrant mutex — the temporaries live to the end of the statement, not to
    // the end of their line — so the second one deadlocked the caller. `serve` calls this once
    // per saved device at startup, which is why a host with any device in `config.json` hung
    // before it ever printed a join URL.
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
            None => (Default::default(), false, (false, false, false)),
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
        // Neither of these is an argument here: `set_device_override` is already at specta's
        // ten-argument ceiling, and unlike everything above them they need no rebuild verdict.
        // `set_device_pointer_scope` and `set_device_auto_frame_rate` own them, and a device
        // with no saved value keeps the default rather than being reset by an unrelated
        // display change.
        pointer_scope: carried_scope,
        auto_frame_rate: carried_auto_frame_rate,
        adaptive_quality: carried_modes.0,
        native_dpr: carried_modes.1,
        auto_orientation: carried_modes.2,
    };
    // What this edit costs the stream, decided by comparing the saved row against the one the
    // sheet just submitted rather than by the fact that a save happened at all. The desktop UI
    // writes every field on every save, so without the comparison renaming a device — or moving
    // anything the pipeline never reads — bumped the epoch, and the client answers an epoch bump
    // by rebuilding. `ReconfigAction` is the whole rule; `session.rs` holds it and the field
    // table that tests it.
    //
    // A device with no saved row is the one case that cannot be compared, and the join path now
    // seeds one, so in practice this is only ever a device that is not connected — where a bump
    // reaches zero sessions anyway.
    let action = {
        let mut overrides = state.device_overrides.lock_recover();
        let action = match overrides.get(&token) {
            Some(old) => DeviceOverride::reconfig_action(old, &next),
            None => ReconfigAction::Rebuild,
        };
        overrides.insert(token.clone(), next);
        action
    };
    // Class (b), whatever else this edit did: remote control is read per event, so it lands on
    // the pointer that is moving now rather than on the next connection.
    session::apply_live_input_for_device(
        &state.sessions,
        &token,
        next.control_enabled,
        next.pointer_scope,
    );
    match action {
        // Classes (a), (b) and (c). Nothing on the wire, nothing on the dashboard, nothing to
        // the client: the name moved, or control did, or the audio sink, and the picture the
        // user is looking at is already the right picture.
        ReconfigAction::Nothing => {
            tprintln!("device {token} settings saved; nothing the live stream reads has changed");
        }
        // The extended desktop's DPI changes under a stream whose geometry is already correct.
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
                    // Nothing live to touch, so there is nothing to apply it to; the next join
                    // reads it back out of the override map anyway.
                    tprintln!(
                        "display scale for device {token} saved; no live display to apply it to"
                    );
                }
            });
        }
        // Every tab this device has open, because the settings are the device's. The client
        // renegotiates on its own join id, so the display it is watching survives the change.
        ReconfigAction::Rebuild => {
            let moved = session::bump_reconfig_epoch_for_device(&state.sessions, &token);
            tprintln!(
                "device {token} changed a stream parameter; {moved} live session(s) will \
                 renegotiate in place"
            );
        }
    }
}

/// Resolve whatever the caller had — a device token, or an address it happens to know — to the
/// one identity the host stores settings under.
///
/// A token is passed straight through. An address is a convenience for the CLI and for a caller
/// holding an old row: it resolves only while exactly one device is connected from it, because an
/// address is a place and not a device. Anything else is refused with a line in the log rather
/// than guessed at, since guessing is how one device ends up wearing another's settings.
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
                 from it — pass the token to be sure which device you mean"
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
/// the host's real monitors — the default) or `"display"` (only the screen it is streaming).
///
/// Its own command rather than another `set_device_override` argument: that one is at specta's
/// ten-argument ceiling, and this needs none of its machinery — no clamp, no rebuild verdict,
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
    // Class (b): straight into every live injector this device is driving. No epoch, because
    // nothing the encoder or the client reads has moved.
    let reached =
        session::apply_live_input_for_device(&state.sessions, &token, control_enabled, parsed);
    tprintln!(
        "[input] pointer scope for device {token} is now {} ({reached} live session(s))",
        parsed.as_str()
    );
}

/// Whether this device's stream may drop below its chosen refresh rate on its own.
///
/// Off is the default and the owner's ruling: the rate the user picked is the rate the stream
/// runs at, an unchanged frame is repeated rather than skipped, and no controller lowers it.
/// A separate command for the same reason as `set_device_pointer_scope` — `set_device_override`
/// is at specta's ten-argument ceiling — and read by the pipeline when the session is built, so
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
    // Class (d): the pipeline copies this out of the override map when the session is built and
    // the display's compose rate is chosen from it, so it takes a rebuild — but only when it
    // actually moved. It used to take neither, which left the setting inert until the next join.
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
/// for all three and means "fixed at what the sheet shows", the same ruling as
/// `set_device_auto_frame_rate`. Their own command for the same reason as that one:
/// `set_device_override` is at specta's ten-argument ceiling. Each is read when the session is
/// built, so a change takes a rebuild — only when something actually moved.
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
    // Class (d): all three are read when the session is built.
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
    // A join id refreshes exactly that tab; without one, every tab the device has open, which is
    // what "restart this device's stream" means when a device can hold several.
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
    // Per device: two tabs of one browser share one set of speakers, and the choice has to
    // outlive every join so `serve` can replay it into a host with nothing connected yet.
    //
    // Class (c), so no epoch. `audioSink` rides in *every* `/reconfig` body and the client
    // applies it on its next poll, about a second away; bumping as well meant that choosing an
    // output device tore the session down — and it did so on every save of the device sheet,
    // because the sheet pushes this command whether or not the sink moved.
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
    state.device_overrides.lock().unwrap().remove(&token);
    state.audio_sinks.lock().unwrap().remove(&token);
    let ended = session::kick_device(&state.sessions, &token, session::ByeReason::Kicked);
    tprintln!("device {token} removed; {ended} live session(s) ended");
}

#[tauri::command]
#[specta::specta]
pub fn set_device_banned(state: State<'_, AppState>, token: String, ip: String, banned: bool) {
    // By token, never by address: the address a banned phone had this morning is the address
    // somebody's laptop gets this afternoon. `ip` is kept as a label for the log and as the
    // last-resort lookup for a caller holding an old row.
    let Some(key) = resolve_device(&state, if token.trim().is_empty() { &ip } else { &token })
    else {
        teprintln!("[devices] cannot ban {ip}: it has never joined, so there is no device to ban");
        return;
    };
    if banned {
        state.banned_devices.lock().unwrap().insert(key.clone());
        let ended = session::kick_device(&state.sessions, &key, session::ByeReason::Kicked);
        tprintln!("device {key} banned; {ended} live session(s) kicked");
    } else {
        state.banned_devices.lock().unwrap().remove(&key);
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
        state.approved_devices.lock().unwrap().insert(token);
        tprintln!("device token approved for auto-join");
    } else {
        state.approved_devices.lock().unwrap().remove(&token);
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
    // Through `shared_display_cap`, so `full` can only ever be true on the one-at-a-time
    // AirPlay tier — which does not exist on Windows, and so `full` is always false here.
    let max = session::shared_display_cap(&state.virtual_display);
    let in_use = session::live_display_count(&state.sessions);
    DisplayCapacity {
        max: max.map(|m| m as u32),
        in_use: in_use as u32,
        full: max.is_some_and(|m| in_use >= m),
        backend: virtual_display::probe_display_backend()
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
    *state.user_turn.lock().unwrap() = UserTurnConfig {
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
    let cfg = state.user_turn.lock().unwrap();
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

#[tauri::command]
#[specta::specta]
pub fn set_server_ports(
    state: State<'_, AppState>,
    http_port: u16,
    https_port: u16,
) -> ServerPorts {
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

    restart_streamers(&state, "port change");

    ServerPorts { http, https }
}

/// Give a stopping server this long to release its listening socket before starting the new one.
const PORT_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// Stop every per-adapter server and start it again. Used by the two settings that cannot be
/// applied to a live server: the listening ports and the encoder selection.
fn restart_streamers(state: &AppState, reason: &str) {
    // Say goodbye before the socket dies, so a client sees a reason instead of a dead transport.
    session::broadcast_bye(
        &state.sessions,
        session::ByeReason::HostExit,
        Duration::from_millis(500),
    );

    {
        let mut streamers = state.streamers.lock().unwrap();
        for (ip, streamer) in streamers.drain() {
            tprintln!("[streamer] stopping streamer bound to {ip} for {reason}");
            streamer
                .handle
                .graceful_shutdown(Some(Duration::from_millis(500)));
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

/// Poll the adapter addresses we are about to bind until they accept a listener again. This
/// replaces a fixed sleep that either wasted the wait or was not long enough, silently.
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
    tprintln!(
        "[streamer] GPU video encoding {}; restarting streamers",
        if disabled {
            "disabled (software x264 only)"
        } else {
            "re-enabled"
        }
    );
    restart_streamers(&state, "encoder change");
}

/// Re-invoke ourselves elevated to run one of the legacy driver subcommands.
///
/// Every step here is ordinary-user-reachable: cancelling the UAC prompt, a profile path that is
/// not valid UTF-8, a dev tree with no bundled resources. All of them used to `unwrap()`, and the
/// result was then discarded in favour of a hardcoded `true`, so the UI reported a successful
/// install for a driver that was never installed.
fn run_elevated_driver_command(app: &tauri::AppHandle, subcommand: &str) -> bool {
    let exe_path = match std::env::current_exe().map(|p| p.into_os_string().into_string()) {
        Ok(Ok(path)) => path,
        Ok(Err(_)) => {
            teprintln!("[drivers] {subcommand}: the executable path is not valid UTF-8");
            return false;
        }
        Err(e) => {
            teprintln!("[drivers] {subcommand}: could not resolve the executable path: {e}");
            return false;
        }
    };

    let mut cmd = StdCommand::new(exe_path);
    cmd.arg(subcommand);
    let mut admincmd = Command::new(cmd);
    let mut fincmd = admincmd.name("ScreenExtend".to_string());
    if let Ok(icon) = app
        .path()
        .resolve("icons/icon.icns", BaseDirectory::Resource)
        .map_err(|e| e.to_string())
        .and_then(|p| std::fs::read(p).map_err(|e| e.to_string()))
    {
        fincmd = fincmd.icon(icon);
    }

    match fincmd.output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            teprintln!(
                "[drivers] {subcommand} exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            false
        }
        Err(e) => {
            // The ordinary case here is the user dismissing the elevation prompt.
            teprintln!("[drivers] {subcommand} could not run elevated: {e}");
            false
        }
    }
}

#[tauri::command]
#[specta::specta]
pub fn install_drivers(app: tauri::AppHandle) -> bool {
    if !run_elevated_driver_command(&app, "installdrivers") {
        return false;
    }
    // Report what the machine now actually has, not what we asked for.
    let backend = virtual_display::probe_display_backend();
    tprintln!(
        "[drivers] after install, display backend is {}",
        backend.as_str()
    );
    backend == virtual_display::DisplayBackend::IddDriver
}

#[tauri::command]
#[specta::specta]
pub fn remove_drivers(app: tauri::AppHandle) -> bool {
    run_elevated_driver_command(&app, "removedrivers")
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
    *state.session_auth.session_id.lock().unwrap() = session_id;
    *state.session_auth.otp.lock().unwrap() = otp;
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
    *state.cloud_status.lock().unwrap() = ("connecting".to_string(), String::new());
    let sink = TauriCloudStatusSink::new_shared(app, state.cloud_status.clone());
    let client = CloudClient::spawn(CloudConfig::new(session_id), server_config, sink);
    let mut guard = state.cloud.lock().unwrap();
    if let Some(mut prev) = guard.take() {
        prev.stop();
    }
    *guard = Some(client);
}

#[tauri::command]
#[specta::specta]
pub fn unregister_cloud_session(state: State<'_, AppState>) {
    let mut guard = state.cloud.lock().unwrap();
    if let Some(mut prev) = guard.take() {
        prev.stop();
        tprintln!("[cloud] public sessions disabled; relay client stopped");
    }
    *state.cloud_status.lock().unwrap() =
        (CloudState::Disabled.as_str().to_string(), String::new());
}

#[tauri::command]
#[specta::specta]
pub fn get_cloud_status(state: State<'_, AppState>) -> crate::CloudStatusChange {
    let (status, detail) = state.cloud_status.lock().unwrap().clone();
    crate::CloudStatusChange {
        state: status,
        detail,
    }
}

pub fn sync_streamers(state: &AppState) {
    let (desired, fresh_local_ips) = {
        let adapters = state.network_adapters.lock().unwrap();
        let desired: Vec<(String, Ipv4Addr)> = adapters
            .iter()
            .flat_map(|adapter| adapter.ip_addresses.iter())
            .filter_map(|ip| ip.parse::<Ipv4Addr>().ok().map(|addr| (ip.clone(), addr)))
            .collect();
        // We bind IPv4 only. An adapter that carries addresses but no IPv4 one is invisible to
        // the streamer, and used to be so without a word.
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

    *state.local_ips.lock().unwrap() = fresh_local_ips;

    let (http_port, https_port) = state.server_ports.get();

    let mut streamers = state.streamers.lock().unwrap();

    streamers.retain(|ip, streamer| {
        if desired.iter().any(|(desired_ip, _)| desired_ip == ip) {
            true
        } else {
            tprintln!("[streamer] stopping streamer bound to {ip}");
            streamer
                .handle
                .graceful_shutdown(Some(Duration::from_secs(1)));
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
        // One thread per adapter, but they all drive the single shared transport runtime
        // (`streamer::transport_runtime`) rather than each building one of their own.
        let spawned = std::thread::Builder::new()
            .name(format!("se-adapter-{ip}"))
            .spawn(move || {
                if let Err(e) = Streamer::new(config).run_with_handle(thread_handle) {
                    teprintln!("[streamer] streamer bound to {ip_for_log} exited: {e}");
                }
            });
        if let Err(e) = spawned {
            teprintln!("[streamer] could not start a streamer for {ip}: {e}");
            continue;
        }

        tprintln!("[streamer] started streamer bound to {ip}");
        streamers.insert(ip, StreamerHandle { handle });
    }
}

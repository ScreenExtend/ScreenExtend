pub mod airplay;
pub mod audio;
pub mod compatibility;
pub mod device_reporter;
pub mod hosted_network;
pub mod networking;
pub mod objc_exception;
pub mod permissions;
pub mod streamer;
pub mod virtual_display;

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
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::path::BaseDirectory;
use tauri::Manager;
use tauri::State;
use virtual_display::new_shared_controller;

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

pub fn focus_main_window(window: &tauri::WebviewWindow) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    unsafe {
        let ns_app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if !ns_app.is_null() {
            let _: () = msg_send![ns_app, activateIgnoringOtherApps: true];
        }
    }

    match window.ns_window() {
        Ok(ptr) if !ptr.is_null() => unsafe {
            let ns_window = ptr as *mut AnyObject;
            let nil: *mut AnyObject = std::ptr::null_mut();
            let _: () = msg_send![ns_window, makeKeyAndOrderFront: nil];
        },
        Ok(_) => {}
        Err(e) => teprintln!("[window] focus: failed to get ns_window: {e}"),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    if app_handle.try_state::<AppState>().is_some() {
        return true;
    }

    audio::legacy::routing::recover_on_launch();
    let virtual_display = tauri::async_runtime::spawn_blocking(new_shared_controller).await;

    // `setup` returns a bare bool, so the only place the reason can survive is the log backlog —
    // and until now `Ok(None)` and a panicking `new_shared_controller` were indistinguishable, with
    // the panic payload discarded.
    let virtual_display = match virtual_display {
        Ok(Some(vd)) => vd,
        Ok(None) => {
            teprintln!(
                "[setup] no virtual-display backend could be constructed on this Mac \
                 (display tier: {}). Nothing else in setup ran.",
                virtual_display::probe_display_backend().as_str()
            );
            return false;
        }
        Err(e) => {
            teprintln!("[setup] building the virtual-display controller panicked: {e}");
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
    start_session_watchdog(app_handle.clone());
    true
}

/// How often the watchdog wakes. Cheap enough to be frequent: the AirPlay latch is an
/// atomic read and the screen-recording read is the window-list probe, 0.6 ms warm.
const WATCHDOG_TICK: Duration = Duration::from_secs(5);

/// How often authorization is actually re-read while a session is streaming.
const TCC_REPROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Watch two things that can change under a live session and that nothing else notices.
///
/// **Screen-recording consent.** macOS 15 re-asks roughly every 30 days and there is no
/// supported way to suppress it. A host left running as a permanent second monitor -- the
/// product's whole use case -- will meet that with a device connected, and if the user
/// dismisses the dialog the capture APIs go on delivering frames: they just contain
/// wallpaper. So the host reports a healthy stream while the client shows a blank desktop.
/// `ensure_screen_recording_access` catches this at pipeline start, which is every join
/// and every refresh, and nowhere in between -- this is the in-between. Only a definite
/// `Some(true) -> Some(false)` transition acts; `None` means "could not read" and must
/// never be treated as a refusal.
///
/// **AirPlay geometry.** On that display tier macOS owns the display and moves it under
/// us; the sender re-sends codec data when it does, which `MirrorStats` latches. Bumping
/// the reconfig epoch hands that to the client on its next `/reconfig` poll instead of
/// waiting for a size poll to notice.
fn start_session_watchdog(app_handle: tauri::AppHandle) {
    std::thread::Builder::new()
        .name("se-session-watchdog".into())
        .spawn(move || {
            let airplay_tier = virtual_display::probe_display_backend()
                == virtual_display::DisplayBackend::AirPlayFallback;
            let mut authorized: Option<bool> = None;
            let mut last_probe = Instant::now();
            loop {
                std::thread::sleep(WATCHDOG_TICK);
                let Some(state) = app_handle.try_state::<AppState>() else {
                    continue;
                };

                if airplay_tier && airplay::take_geometry_changed() {
                    let joins: Vec<String> =
                        state.sessions.lock().unwrap().keys().cloned().collect();
                    if !joins.is_empty() {
                        tprintln!(
                            "[airplay] macOS changed the display geometry; asking {} session(s) \
                             to renegotiate",
                            joins.len()
                        );
                    }
                    for join in joins {
                        session::bump_reconfig_epoch(&state.sessions, &join);
                    }
                }

                let streaming = {
                    let sessions = state.sessions.lock().unwrap();
                    sessions.values().any(|s| s.live_display.is_some())
                };
                if !streaming {
                    // Nothing to protect, and the next join re-reads authorization anyway.
                    authorized = None;
                    continue;
                }
                if last_probe.elapsed() < TCC_REPROBE_INTERVAL {
                    continue;
                }
                last_probe = Instant::now();
                let now = permissions::screen_recording_live();
                if authorized == Some(true) && now == Some(false) {
                    teprintln!(
                        "[tcc] Screen Recording authorization was revoked while streaming. \
                         macOS 15 and later re-ask about every 30 days; capture would keep \
                         delivering frames of the wallpaper, so the sessions are being ended \
                         instead. Re-approve ScreenExtend under System Settings -> Privacy & \
                         Security -> Screen Recording, then reconnect."
                    );
                    crate::streamer::session::broadcast_bye(
                        &state.sessions,
                        session::ByeReason::Kicked,
                        crate::BYE_FLUSH_TIMEOUT,
                    );
                    let joins: Vec<String> =
                        state.sessions.lock().unwrap().keys().cloned().collect();
                    let ended = joins.len() as u32;
                    for join in joins {
                        session::bump_kick_epoch(&state.sessions, &join);
                        session::signal_leave(&state.sessions, &join);
                    }
                    // The log line above is the record; this is what puts it in front of
                    // the person at the host, who is otherwise left to work out why a
                    // device dropped. `App.tsx` answers it by re-opening the permission
                    // gate over whatever route is mounted.
                    use tauri_specta::Event as _;
                    if let Err(e) = (crate::ScreenRecordingRevoked {
                        key: permissions::KEY_SCREEN_RECORDING.to_string(),
                        sessions_ended: ended,
                    })
                    .emit(&app_handle)
                    {
                        teprintln!("[tcc] could not emit ScreenRecordingRevoked: {e}");
                    }
                }
                if now.is_some() {
                    authorized = now;
                }
            }
        })
        .ok();
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
///
/// **UNVERIFIED on macOS**: written to keep the command surface identical across the three
/// `*_utils` modules and never compiled on a Mac in this workstream. The macOS pipeline's half
/// of the ruling is not implemented at all.
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
    // Through `shared_display_cap`, so `full` can only ever be true on the AirPlay tier, which
    // macOS itself limits to one display.
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
    app: tauri::AppHandle,
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

    // Tell the connected clients why their stream is about to end, rather than dropping the
    // transport under them.
    let connected: Vec<String> = state.sessions.lock().unwrap().keys().cloned().collect();
    for join in connected {
        session::send_bye(&state.sessions, &join, session::ByeReason::HostExit);
    }

    {
        let mut streamers = state.streamers.lock().unwrap();
        for (ip, streamer) in streamers.drain() {
            tprintln!("[streamer] stopping streamer bound to {ip} for port change");
            streamer
                .handle
                .graceful_shutdown(Some(Duration::from_millis(500)));
        }
    }
    // Waiting for the old listeners to release their ports used to be a 300 ms
    // `std::thread::sleep` inside a synchronous command, i.e. on the main thread, with
    // `sync_streamers` and its per-adapter runtime spin-up behind it. The command now returns
    // immediately and the restart finishes on its own thread.
    std::thread::spawn(move || {
        use tauri::Manager;
        std::thread::sleep(Duration::from_millis(300));
        match app.try_state::<AppState>() {
            Some(state) => sync_streamers(&state),
            None => teprintln!("[streamer] port change: app state went away before the restart"),
        }
    });

    ServerPorts { http, https }
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
    state
        .disable_gpu_encode
        .store(disabled, std::sync::atomic::Ordering::Relaxed);
}

#[tauri::command]
#[specta::specta]
pub fn install_drivers(_app: tauri::AppHandle) -> bool {
    true
}

#[tauri::command]
#[specta::specta]
pub fn remove_drivers(_app: tauri::AppHandle) -> bool {
    true
}

#[tauri::command]
#[specta::specta]
pub fn install_audio_driver(app: tauri::AppHandle) -> String {
    let pkg = app
        .path()
        .resolve("resources/ScreenExtendAudio.pkg", BaseDirectory::Resource);
    match pkg {
        Ok(path) => {
            if !path.exists() {
                // `resolve` only joins a path, so an app bundle built without the package
                // staged reaches `install_pkg` and fails there with nothing naming the cause.
                teprintln!(
                    "[audio] this build does not carry the audio driver package ({} is \
                     missing). Build it with macos/ScreenExtendAudio/scripts/release.sh \
                     before `tauri build`, or the Install button can only fail.",
                    path.display()
                );
            }
            audio::legacy::installer::install_pkg(&path)
                .as_str()
                .to_string()
        }
        Err(e) => {
            teprintln!("[audio] could not resolve installer package: {e}");
            "failed".to_string()
        }
    }
}

#[tauri::command]
#[specta::specta]
pub fn uninstall_audio_driver() -> String {
    audio::legacy::installer::uninstall().as_str().to_string()
}

#[tauri::command]
#[specta::specta]
pub fn audio_driver_status() -> String {
    use audio::legacy::probe::LegacyState;
    match audio::legacy::installer::health() {
        LegacyState::Ready => "ready".to_string(),
        LegacyState::NeedsInstall => "needs_install".to_string(),
        LegacyState::InstalledButUnhealthy => "installed_but_unhealthy".to_string(),
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_legacy_volume_key_proxy(enabled: bool) {
    audio::legacy::volume_keys::set_enabled(enabled);
}

pub fn remove_all_displays(client: &SharedVirtualDisplay) {
    client.remove_all_displays();
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

fn display_slot_holder(state: &AppState) -> Option<String> {
    let max = session::shared_display_cap(&state.virtual_display)?;
    if session::live_display_count(&state.sessions) < max {
        return None;
    }
    session::display_holder_host_ip(&state.sessions)
}

/// Whether an IPv6 address is one a client can reach by typing it into a browser.
///
/// Link-local (`fe80::/10`) is not: it needs a zone index (`%en0`), and no URL carries one. The
/// loopback and unspecified addresses are not either. Global unicast and unique-local are, and on
/// an IPv6-only network they are the only way to host at all.
fn servable_v6(ip: &std::net::Ipv6Addr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified() && ip.segments()[0] & 0xffc0 != 0xfe80
}

pub fn sync_streamers(state: &AppState) {
    let (desired, unusable, fresh_local_ips) = {
        let adapters = state.network_adapters.lock().unwrap();
        let all = || {
            adapters
                .iter()
                .flat_map(|adapter| adapter.ip_addresses.iter())
        };

        // IPv4 first, then whatever IPv6 a browser can actually reach. An IPv6-only network used
        // to produce no listener at all and no diagnostic, because every v6 address failed
        // `parse::<Ipv4Addr>()` and was dropped with `.ok()`.
        let mut desired: Vec<(String, IpAddr)> = Vec::new();
        let mut unusable: Vec<String> = Vec::new();
        for ip in all() {
            match ip.parse::<IpAddr>() {
                Ok(addr @ IpAddr::V4(_)) => desired.push((ip.clone(), addr)),
                Ok(IpAddr::V6(v6)) if servable_v6(&v6) => {
                    desired.push((ip.clone(), IpAddr::V6(v6)))
                }
                _ => unusable.push(ip.clone()),
            }
        }
        desired.sort_by_key(|(_, addr)| addr.is_ipv6());

        let fresh_local_ips = crate::streamer::server::collect_local_ips(all().map(String::as_str));
        (desired, unusable, fresh_local_ips)
    };

    *state.local_ips.lock().unwrap() = fresh_local_ips;

    if !unusable.is_empty() {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let first_time = !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed);
        if first_time || desired.is_empty() {
            teprintln!(
                "[streamer] {} address(es) are not being served ({}): a link-local IPv6 address                  needs a zone index that no browser URL can carry{}",
                unusable.len(),
                unusable.join(", "),
                if desired.is_empty() {
                    ", and no other address is available, so nothing is listening and no device can join"
                } else {
                    ""
                }
            );
        }
    }

    let desired = match display_slot_holder(state) {
        Some(host_ip) => {
            let kept: Vec<_> = desired
                .into_iter()
                .filter(|(ip, _)| *ip == host_ip)
                .collect();
            if kept.is_empty() {
                tprintln!(
                    "[streamer] display slot is taken by a client on an adapter that is gone; serving nobody until it frees up"
                );
            }
            kept
        }
        None => desired,
    };

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
            bind_ip: addr,
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
            gpu_encode_toggle: Some(state.disable_gpu_encode.clone()),
            encoder_vendor: *state.encoder_vendor.lock_recover(),
            ..Config::default()
        };

        let thread_handle = handle.clone();
        let ip_for_log = ip.clone();
        // One thread per adapter, but they all drive the single shared transport runtime
        // (`streamer::transport_runtime`) rather than each building one of their own.
        // A discarded `JoinHandle` also discarded the spawn error, so a thread that never
        // started left `streamers` claiming an adapter was served with nothing listening.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn only_reachable_ipv6_addresses_are_served() {
        // The one this dev box reports, and the reason an IPv6-only network still cannot host
        // from it: fe80::/10 needs a zone index no URL can carry.
        assert!(!servable_v6(
            &"fe80::844:907:64ca:8224".parse::<Ipv6Addr>().unwrap()
        ));
        assert!(!servable_v6(&Ipv6Addr::LOCALHOST));
        assert!(!servable_v6(&Ipv6Addr::UNSPECIFIED));
        // Global unicast and unique-local are what an IPv6-only network actually hands out.
        assert!(servable_v6(&"2001:db8::1".parse::<Ipv6Addr>().unwrap()));
        assert!(servable_v6(&"fd00::1".parse::<Ipv6Addr>().unwrap()));
    }
}

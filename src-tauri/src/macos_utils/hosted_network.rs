//! macOS hosted-network (Wi-Fi hotspot) command surface.
//!
//! Signatures mirror `windows_utils::hosted_network` exactly so the generated
//! frontend bindings are identical across platforms.
//!
//! macOS brings up a hotspot through CoreWLAN's *private* Host-AP API
//! (`-[CWInterface startHostAPModeWithSSID:securityType:channel:password:error:]`
//! and `-stopHostAPMode`). Those selectors are absent from the public framework
//! headers, so they are invoked here via `msg_send!`. The behaviour is ported
//! from the reference `hostap.m`.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2_core_wlan::{CWChannel, CWInterface, CWInterfaceMode, CWWiFiClient};
use objc2_foundation::{NSData, NSError, NSString};
use tauri::{AppHandle, State};
use tauri_specta::Event;

use crate::macos_utils::AppState;

/// CoreWLAN security type for an open (no-password) Host-AP network.
const SECURITY_OPEN: u64 = 2;
/// CoreWLAN security type for a WEP-secured Host-AP network.
const SECURITY_WEP: u64 = 16;
/// Preferred 2.4 GHz channel for the hotspot (matches `hostap.m`).
const PREFERRED_CHANNEL: isize = 11;

fn wifi_interface() -> Option<Retained<CWInterface>> {
    unsafe { CWWiFiClient::sharedWiFiClient().interface() }
}

#[tauri::command]
#[specta::specta]
pub fn is_wifi_on() -> bool {
    match wifi_interface() {
        Some(interface) => unsafe { interface.powerOn() },
        None => false,
    }
}

#[tauri::command]
#[specta::specta]
pub fn turn_on_wifi() -> bool {
    let Some(interface) = wifi_interface() else {
        return false;
    };
    unsafe { interface.setPower_error(true) }.is_ok()
}

/// Picks the preferred Host-AP channel, falling back to any supported channel.
fn pick_channel(interface: &CWInterface) -> Option<Retained<CWChannel>> {
    let channels = unsafe { interface.supportedWLANChannels() }?;
    let mut fallback: Option<Retained<CWChannel>> = None;
    for channel in channels.iter() {
        if unsafe { channel.channelNumber() } == PREFERRED_CHANNEL {
            return Some(channel);
        }
        fallback.get_or_insert(channel);
    }
    fallback
}

/// Attempts to bring up Host-AP mode on `interface`/`channel`. Passing
/// `password = None` starts an open network. Returns `true` on success.
fn try_start_host_ap(
    interface: &CWInterface,
    channel: &CWChannel,
    ssid: &str,
    password: Option<&str>,
) -> bool {
    let ssid_data = NSData::with_bytes(ssid.as_bytes());
    let (security_type, password_obj) = match password {
        Some(pw) => (SECURITY_WEP, Some(NSString::from_str(pw))),
        None => (SECURITY_OPEN, None),
    };
    let password_ptr: *const NSString = match &password_obj {
        Some(pw) => Retained::as_ptr(pw),
        None => std::ptr::null(),
    };

    let mut error: *mut NSError = std::ptr::null_mut();
    let success: bool = unsafe {
        msg_send![
            interface,
            startHostAPModeWithSSID: &*ssid_data,
            securityType: security_type,
            channel: channel,
            password: password_ptr,
            error: &mut error,
        ]
    };

    if !success {
        let detail = unsafe { error.as_ref() }
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_else(|| "unknown error".to_string());
        teprintln!("[hosted-network] startHostAPMode (secured={}) failed: {detail}", password.is_some());
    }
    success
}

/// Tears down Host-AP mode if the interface is currently hosting one.
fn stop_host_ap_mode() {
    if let Some(interface) = wifi_interface() {
        unsafe {
            let _: () = msg_send![&*interface, stopHostAPMode];
        }
    }
}

#[tauri::command]
#[specta::specta]
pub fn start_hosted_network(
    app: AppHandle,
    state: State<'_, AppState>,
    name: &str,
    password: &str,
) -> bool {
    let Some(interface) = wifi_interface() else {
        *state.hosted_network_running.lock().unwrap() = false;
        return false;
    };
    let Some(channel) = pick_channel(&interface) else {
        *state.hosted_network_running.lock().unwrap() = false;
        return false;
    };

    // First try with the requested password. macOS Host-AP is flaky and some
    // adapters/drivers reject the secured path, so on failure retry as an open
    // network and let the UI warn the user that the password was dropped.
    let had_password = !password.is_empty();
    let mut fell_back = false;
    let started = if had_password
        && try_start_host_ap(&interface, &channel, name, Some(password))
    {
        true
    } else {
        let started_open = try_start_host_ap(&interface, &channel, name, None);
        fell_back = started_open && had_password;
        started_open
    };

    if started {
        *state.stop_hosted_network.lock().unwrap() = Some(Box::new(stop_host_ap_mode));
    }
    if fell_back {
        let _ = crate::HostedNetworkNoPassword.emit(&app);
    }
    *state.hosted_network_running.lock().unwrap() = started;
    started
}

#[tauri::command]
#[specta::specta]
pub fn stop_hosted_network(_app: AppHandle, state: State<'_, AppState>) -> bool {
    // Prefer the stored teardown hook; fall back to a direct stop so a hotspot
    // left over from a previous run is still cleared.
    match state.stop_hosted_network.lock().unwrap().take() {
        Some(stop) => stop(),
        None => stop_host_ap_mode(),
    }
    *state.hosted_network_running.lock().unwrap() = false;
    true
}

#[tauri::command]
#[specta::specta]
pub fn is_hosted_network(_app: AppHandle, state: State<'_, AppState>) -> bool {
    let live = wifi_interface()
        .map(|interface| unsafe { interface.interfaceMode() } == CWInterfaceMode::HostAP)
        .unwrap_or(false);
    live || *state.hosted_network_running.lock().unwrap()
}

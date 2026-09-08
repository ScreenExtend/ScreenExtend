use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_store::StoreExt;
use tauri_specta::Event;
use wmi::{COMLibrary, WMIConnection};

use super::AppState;

/// How long to keep collecting change notifications before re-enumerating, so a burst
/// (an adapter appearing usually fires several) costs one WMI round trip.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Backstop re-enumeration for anything iphlpapi does not notify us about (a connection
/// profile renamed, a hosted network coming up). Two WMI ticks a minute instead of 180.
const SAFETY_POLL: Duration = Duration::from_secs(30);

/// An empty adapter list tears down every streamer, so demand agreement before believing it.
const EMPTY_CONFIRMATIONS: u32 = 3;
const EMPTY_RECHECK: Duration = Duration::from_millis(500);

#[derive(Deserialize, Debug)]
struct NetAdapter {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "InterfaceIndex")]
    interface_index: Option<u32>,
    #[serde(rename = "DriverDescription")]
    driver_description: Option<String>,
}

#[derive(Deserialize, Debug)]
struct NetIPAddress {
    #[serde(rename = "InterfaceIndex")]
    interface_index: Option<u32>,
    #[serde(rename = "IPAddress")]
    ip_address: Option<String>,
    #[serde(rename = "AddressFamily")]
    address_family: Option<u16>,
}

#[derive(Deserialize, Debug)]
struct NetConnectionProfile {
    #[serde(rename = "InterfaceIndex")]
    interface_index: Option<u32>,
    #[serde(rename = "Name")]
    name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event, PartialEq)]
pub struct NetworkInfo {
    pub network_name: String,
    pub interface_index: u32,
    pub ip_addresses: Vec<String>,
}

const ADAPTER_QUERY: &str = "SELECT Name, InterfaceIndex, DriverDescription FROM MSFT_NetAdapter \
WHERE EndPointInterface = False \
AND (NdisPhysicalMedium = 1 OR NdisPhysicalMedium = 9 OR NdisPhysicalMedium = 14) \
AND OperationalStatusDownMediaDisconnected = False";

const IP_QUERY: &str = "SELECT InterfaceIndex, IPAddress, AddressFamily FROM MSFT_NetIPAddress \
WHERE AddressFamily = 2 OR AddressFamily = 23";

const PROFILE_QUERY: &str = "SELECT InterfaceIndex, Name FROM MSFT_NetConnectionProfile";

const WIFI_DIRECT_DRIVER: &str = "Microsoft Wi-Fi Direct Virtual Adapter";

fn hosted_network_name(app: &AppHandle) -> String {
    let stored_name = app.store("config.json").ok().and_then(|config| {
        let name = config
            .get("hostedNetworkCredentials")?
            .get("name")?
            .as_str()?
            .to_string();
        Some(name)
    });

    stored_name.unwrap_or_else(|| "ScreenExtend".to_string())
}

/// COM must be live on the calling thread before any WMI call. Tauri's worker threads are
/// not initialized by anything in this crate, so initialize here and only fall back to the
/// "somebody else already did it" assumption when `CoInitializeEx` refuses (an STA thread).
fn com_for_this_thread() -> COMLibrary {
    match COMLibrary::new() {
        Ok(com) => com,
        Err(e) => {
            teprintln!("[network] CoInitializeEx failed ({e}); assuming COM is already up");
            unsafe { COMLibrary::assume_initialized() }
        }
    }
}

/// Enumerate the host's usable network adapters. `hosted_network_label` names the Wi-Fi Direct
/// virtual adapter when our own hosted network is up; passing `None` labels it generically.
///
/// Returns `Err` rather than an empty list when the enumeration itself failed: an empty list is
/// load-bearing (it stops every streamer) and must not be produced by a transient WMI hiccup.
fn enumerate_adapters(hosted_network_label: Option<&str>) -> Result<Vec<NetworkInfo>, String> {
    let com_con = com_for_this_thread();
    let wmi_con = WMIConnection::with_namespace_path("root\\StandardCimv2", com_con)
        .map_err(|e| format!("WMI connect to root\\StandardCimv2 failed: {e}"))?;

    let adapters: Vec<NetAdapter> = wmi_con
        .raw_query(ADAPTER_QUERY)
        .map_err(|e| format!("MSFT_NetAdapter query failed: {e}"))?;
    let ip_addresses: Vec<NetIPAddress> = wmi_con
        .raw_query(IP_QUERY)
        .map_err(|e| format!("MSFT_NetIPAddress query failed: {e}"))?;
    let connection_profiles: Vec<NetConnectionProfile> = wmi_con
        .raw_query(PROFILE_QUERY)
        .map_err(|e| format!("MSFT_NetConnectionProfile query failed: {e}"))?;

    let mut ip_map: HashMap<u32, Vec<&NetIPAddress>> = HashMap::new();
    for ip in &ip_addresses {
        if let Some(interface_index) = ip.interface_index {
            ip_map.entry(interface_index).or_default().push(ip);
        }
    }

    let mut profile_map: HashMap<u32, &NetConnectionProfile> = HashMap::new();
    for profile in &connection_profiles {
        if let Some(interface_index) = profile.interface_index {
            profile_map.insert(interface_index, profile);
        }
    }

    Ok(adapters
        .iter()
        .filter_map(|adapter| {
            let interface_index = adapter.interface_index?;

            let is_wifi_direct = adapter.driver_description.as_deref() == Some(WIFI_DIRECT_DRIVER);
            let network_name = if is_wifi_direct {
                hosted_network_label
                    .unwrap_or("Personal Hotspot")
                    .to_string()
            } else if let Some(profile) = profile_map.get(&interface_index) {
                profile.name.as_deref().unwrap_or("Unknown").to_string()
            } else {
                adapter.name.as_deref().unwrap_or("Unknown").to_string()
            };

            let ip_addresses = ip_map.get(&interface_index).map_or_else(Vec::new, |ips| {
                let by_family = |family: u16| {
                    ips.iter()
                        .filter(move |ip| ip.address_family == Some(family))
                        .filter_map(|ip| ip.ip_address.clone())
                };
                by_family(2).chain(by_family(23)).collect()
            });

            Some(NetworkInfo {
                network_name,
                interface_index,
                ip_addresses,
            })
        })
        .collect())
}

fn hosted_label(app: &AppHandle) -> Option<String> {
    let running = app
        .try_state::<AppState>()
        .map(|state| *state.hosted_network_running.lock().unwrap())
        .unwrap_or(false);
    running.then(|| hosted_network_name(app))
}

#[tauri::command]
#[specta::specta]
pub fn get_network_adapters(app: AppHandle, state: State<'_, AppState>) -> Vec<NetworkInfo> {
    let label = (*state.hosted_network_running.lock().unwrap()).then(|| hosted_network_name(&app));
    match enumerate_adapters(label.as_deref()) {
        Ok(adapters) => adapters,
        Err(e) => {
            teprintln!("[network] adapter enumeration failed: {e}");
            Vec::new()
        }
    }
}

pub fn cli_adapters() -> Vec<NetworkInfo> {
    match enumerate_adapters(None) {
        Ok(adapters) => adapters,
        Err(e) => {
            teprintln!("[network] adapter enumeration failed: {e}");
            Vec::new()
        }
    }
}

/// IPv4 addresses currently held by the virtual adapter Windows creates for a hosted network
/// (both the Wi-Fi Direct legacy AP and the Mobile Hotspot ride "Microsoft Wi-Fi Direct Virtual
/// Adapter"). An AP that came up but is not serving DHCP has none, which is how "we asked for a
/// hotspot" is told apart from "a client can actually get an address".
pub fn hosted_network_ipv4() -> Vec<String> {
    const ANY_ADAPTER: &str = "SELECT InterfaceIndex, Name, DriverDescription FROM MSFT_NetAdapter";

    let com_con = com_for_this_thread();
    let wmi_con = match WMIConnection::with_namespace_path("root\\StandardCimv2", com_con) {
        Ok(connection) => connection,
        Err(e) => {
            teprintln!("[hosted-network] WMI connect failed while checking the AP address: {e}");
            return Vec::new();
        }
    };

    let adapters: Vec<NetAdapter> = match wmi_con.raw_query(ANY_ADAPTER) {
        Ok(rows) => rows,
        Err(e) => {
            teprintln!("[hosted-network] MSFT_NetAdapter query failed: {e}");
            return Vec::new();
        }
    };
    let indices: Vec<u32> = adapters
        .iter()
        .filter(|adapter| {
            adapter
                .driver_description
                .as_deref()
                .is_some_and(|d| d.contains("Wi-Fi Direct Virtual Adapter"))
        })
        .filter_map(|adapter| adapter.interface_index)
        .collect();
    if indices.is_empty() {
        return Vec::new();
    }

    let ips: Vec<NetIPAddress> = match wmi_con
        .raw_query("SELECT InterfaceIndex, IPAddress, AddressFamily FROM MSFT_NetIPAddress WHERE AddressFamily = 2")
    {
        Ok(rows) => rows,
        Err(e) => {
            teprintln!("[hosted-network] MSFT_NetIPAddress query failed: {e}");
            return Vec::new();
        }
    };

    ips.iter()
        .filter(|ip| ip.interface_index.is_some_and(|i| indices.contains(&i)))
        .filter_map(|ip| ip.ip_address.clone())
        // 169.254/16 is what Windows assigns when the AP came up but DHCP did not.
        .filter(|ip| !ip.starts_with("169.254."))
        .collect()
}

static WATCHER_STARTED: AtomicBool = AtomicBool::new(false);
static WAKE: OnceLock<crossbeam_channel::Sender<()>> = OnceLock::new();

/// iphlpapi delivers these on a system thread-pool thread. Do nothing here but poke the
/// watcher: it must never block, allocate or take one of our locks.
unsafe extern "system" fn on_ip_interface_change(
    _context: *const c_void,
    _row: *const windows::Win32::NetworkManagement::IpHelper::MIB_IPINTERFACE_ROW,
    _notification_type: windows::Win32::NetworkManagement::IpHelper::MIB_NOTIFICATION_TYPE,
) {
    if let Some(tx) = WAKE.get() {
        let _ = tx.try_send(());
    }
}

unsafe extern "system" fn on_unicast_address_change(
    _context: *const c_void,
    _row: *const windows::Win32::NetworkManagement::IpHelper::MIB_UNICASTIPADDRESS_ROW,
    _notification_type: windows::Win32::NetworkManagement::IpHelper::MIB_NOTIFICATION_TYPE,
) {
    if let Some(tx) = WAKE.get() {
        let _ = tx.try_send(());
    }
}

/// Register for OS interface/address change notifications. Returns false if either
/// registration failed, in which case the watcher falls back to the safety poll alone.
fn register_change_notifications() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::NetworkManagement::IpHelper::{
        NotifyIpInterfaceChange, NotifyUnicastIpAddressChange,
    };
    use windows::Win32::Networking::WinSock::AF_UNSPEC;

    // The cancel handles are deliberately leaked: the watcher lives as long as the process.
    let mut interface_handle = HANDLE::default();
    let mut address_handle = HANDLE::default();

    let interface_rc = unsafe {
        NotifyIpInterfaceChange(
            AF_UNSPEC,
            Some(on_ip_interface_change),
            None,
            false,
            &mut interface_handle,
        )
    };
    if interface_rc != windows::Win32::Foundation::NO_ERROR {
        teprintln!("[network-watcher] NotifyIpInterfaceChange failed: {interface_rc:?}");
        return false;
    }

    let address_rc = unsafe {
        NotifyUnicastIpAddressChange(
            AF_UNSPEC,
            Some(on_unicast_address_change),
            None,
            false,
            &mut address_handle,
        )
    };
    if address_rc != windows::Win32::Foundation::NO_ERROR {
        teprintln!("[network-watcher] NotifyUnicastIpAddressChange failed: {address_rc:?}");
        return false;
    }

    true
}

#[tauri::command]
#[specta::specta]
pub fn watch_for_network_changes(app: AppHandle) {
    if WATCHER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    let _ = WAKE.set(tx);
    let event_driven = register_change_notifications();
    tprintln!(
        "[network-watcher] starting ({}), safety poll every {}s",
        if event_driven {
            "event-driven"
        } else {
            "poll-only, OS notifications unavailable"
        },
        SAFETY_POLL.as_secs()
    );

    std::thread::spawn(move || {
        let _com = com_for_this_thread();

        let mut previous = match enumerate_adapters(hosted_label(&app).as_deref()) {
            Ok(adapters) => adapters,
            Err(e) => {
                teprintln!("[network-watcher] initial enumeration failed: {e}");
                Vec::new()
            }
        };
        tprintln!("[network-watcher] initial network adapters: {previous:?}");
        apply(&app, previous.clone());

        loop {
            // Wait for a change notification, or wake on the safety poll.
            let _ = rx.recv_timeout(SAFETY_POLL);
            // Coalesce the burst that follows any single real change.
            while rx.recv_timeout(DEBOUNCE).is_ok() {}

            let label = hosted_label(&app);
            let current = match enumerate_adapters(label.as_deref()) {
                Ok(adapters) => adapters,
                Err(e) => {
                    teprintln!("[network-watcher] enumeration failed, keeping last known set: {e}");
                    continue;
                }
            };

            // Losing every adapter stops every streamer. Confirm it before believing it.
            let current = if current.is_empty() && !previous.is_empty() {
                match confirm_empty(label.as_deref()) {
                    Some(confirmed) => confirmed,
                    None => continue,
                }
            } else {
                current
            };

            if current != previous {
                tprintln!("[network-watcher] network adapters changed: {current:?}");
                previous = current.clone();
                apply(&app, current);
            }
        }
    });
}

/// Re-enumerate a few times before accepting "no adapters at all". Returns `Some(list)` when
/// the machine really did lose everything (or came back with something), `None` to skip the tick.
fn confirm_empty(label: Option<&str>) -> Option<Vec<NetworkInfo>> {
    for attempt in 1..EMPTY_CONFIRMATIONS {
        std::thread::sleep(EMPTY_RECHECK);
        match enumerate_adapters(label) {
            Ok(adapters) if adapters.is_empty() => continue,
            Ok(adapters) => {
                tprintln!(
                    "[network-watcher] empty adapter list was transient (recovered on retry {attempt})"
                );
                return Some(adapters);
            }
            Err(e) => {
                teprintln!("[network-watcher] enumeration failed while confirming empty set: {e}");
                return None;
            }
        }
    }
    teprintln!(
        "[network-watcher] no usable network adapters after {EMPTY_CONFIRMATIONS} attempts; stopping all streamers"
    );
    Some(Vec::new())
}

fn apply(app: &AppHandle, adapters: Vec<NetworkInfo>) {
    use tauri_specta::Event;
    let state = app.state::<AppState>();
    *state.network_adapters.lock().unwrap() = adapters;
    super::sync_streamers(&state);
    if let Err(e) = crate::NetworkChange.emit(app) {
        teprintln!("[network-watcher] failed to emit NetworkChange: {e:?}");
    }
}

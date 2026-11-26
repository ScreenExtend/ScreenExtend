use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_store::StoreExt;
use tauri_specta::Event;
use wmi::{COMLibrary, WMIConnection};

use super::{com, AppState};

/// How long to keep collecting change notifications before re-enumerating, so a burst
/// (an adapter appearing usually fires several) costs one WMI round trip.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Backstop re-enumeration for anything iphlpapi does not notify about (a connection profile
/// renamed, a hosted network coming up).
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
    #[serde(rename = "ComponentID")]
    component_id: Option<String>,
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

const ADAPTER_QUERY: &str = "SELECT Name, InterfaceIndex, DriverDescription, ComponentID \
FROM MSFT_NetAdapter \
WHERE EndPointInterface = False \
AND (NdisPhysicalMedium = 1 OR NdisPhysicalMedium = 9 OR NdisPhysicalMedium = 14) \
AND OperationalStatusDownMediaDisconnected = False";

const IP_QUERY: &str = "SELECT InterfaceIndex, IPAddress, AddressFamily FROM MSFT_NetIPAddress \
WHERE AddressFamily = 2 OR AddressFamily = 23";

const PROFILE_QUERY: &str = "SELECT InterfaceIndex, Name FROM MSFT_NetConnectionProfile";

/// The hardware id prefix of the adapters Windows puts a hosted network on, from the inbox
/// `netvwifimp.inf`: `...\vwifimp_wfd` is the Wi-Fi Direct virtual adapter both the Mobile
/// Hotspot and the Wi-Fi Direct legacy AP ride (seen on Windows 11 26200), `...\vwifimp` the
/// older hosted-network adapter of the netsh tier. A hardware id is never localized; the driver
/// description, an INF display string, is.
const HOSTED_AP_COMPONENT_PREFIX: &str = "{5d624f94-8850-40c3-a3fa-a4fd2080baf3}\\vwifimp";

/// The English description, kept only for an adapter whose component id did not come back.
const WIFI_DIRECT_DRIVER: &str = "Wi-Fi Direct Virtual Adapter";

fn is_hosted_ap_adapter(component_id: Option<&str>, driver_description: Option<&str>) -> bool {
    match component_id.filter(|id| !id.is_empty()) {
        Some(id) => id
            .get(..HOSTED_AP_COMPONENT_PREFIX.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(HOSTED_AP_COMPONENT_PREFIX)),
        None => driver_description.is_some_and(|d| d.contains(WIFI_DIRECT_DRIVER)),
    }
}

fn hosted_network_name(app: &AppHandle) -> String {
    let stored_name = app
        .store(crate::data_dir::config_store())
        .ok()
        .and_then(|config| {
            let name = config
                .get("hostedNetworkCredentials")?
                .get("name")?
                .as_str()?
                .to_string();
            Some(name)
        });

    stored_name.unwrap_or_else(|| "ScreenExtend".to_string())
}

/// COM must be live on the calling thread before any WMI call, and this is reached from three
/// kinds of thread: a Tauri command (already in Tauri's STA), the watcher thread the app spawns
/// (no apartment), and a `spawn_blocking` worker under `link_medium_for_local_ip` (likewise).
///
/// Never `COMLibrary::new()`: its unconditional `CoInitializeEx(MTA)` is refused on a Tauri
/// thread with `RPC_E_CHANGED_MODE` (0x80010106). `windows_utils::com` asks first.
fn com_for_this_thread(site: &'static str) -> Result<(com::ComScope, COMLibrary), String> {
    com::wmi_for_this_thread(site).map_err(|e| format!("{e:#}"))
}

/// Enumerate the host's usable network adapters. `hosted_network_label` names the Wi-Fi Direct
/// virtual adapter when our own hosted network is up; passing `None` labels it generically.
///
/// Returns `Err` rather than an empty list when the enumeration itself failed: an empty list is
/// load-bearing (it stops every streamer) and must not be produced by a transient WMI hiccup.
fn enumerate_adapters(hosted_network_label: Option<&str>) -> Result<Vec<NetworkInfo>, String> {
    // `_com` is declared first so it outlives the connection: locals drop in reverse, and the
    // apartment must not be released under a live WMI proxy
    let (_com, com_con) = com_for_this_thread("network")?;
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

            let is_wifi_direct = is_hosted_ap_adapter(
                adapter.component_id.as_deref(),
                adapter.driver_description.as_deref(),
            );
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

/// Three WMI queries: on the blocking pool, not on the UI's main thread, where a sync command
/// runs and where the pages polling it stalled the window.
#[tauri::command]
#[specta::specta]
pub async fn get_network_adapters(app: AppHandle) -> Vec<NetworkInfo> {
    let enumerate = move || {
        let label = hosted_label(&app);
        match enumerate_adapters(label.as_deref()) {
            Ok(adapters) => adapters,
            Err(e) => {
                teprintln!("[network] adapter enumeration failed: {e}");
                Vec::new()
            }
        }
    };
    match tauri::async_runtime::spawn_blocking(enumerate).await {
        Ok(adapters) => adapters,
        Err(e) => {
            teprintln!("[network] adapter enumeration failed on its worker thread: {e}");
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

/// IPv4 addresses held by the virtual adapter Windows creates for a hosted network (the Wi-Fi
/// Direct legacy AP and the Mobile Hotspot both ride "Microsoft Wi-Fi Direct Virtual Adapter").
/// An AP that came up but is not serving DHCP has none.
pub fn hosted_network_ipv4() -> Vec<String> {
    const ANY_ADAPTER: &str =
        "SELECT InterfaceIndex, Name, DriverDescription, ComponentID FROM MSFT_NetAdapter";

    let (_com, com_con) = match com_for_this_thread("hosted-network") {
        Ok(pair) => pair,
        Err(e) => {
            teprintln!("[hosted-network] {e}");
            return Vec::new();
        }
    };
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
            is_hosted_ap_adapter(
                adapter.component_id.as_deref(),
                adapter.driver_description.as_deref(),
            )
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
        // 169.254/16 is what Windows assigns when the AP came up but DHCP did not
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

/// Returns false if either registration failed: the watcher then runs on the safety poll alone.
fn register_change_notifications() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::NetworkManagement::IpHelper::{
        NotifyIpInterfaceChange, NotifyUnicastIpAddressChange,
    };
    use windows::Win32::Networking::WinSock::AF_UNSPEC;

    // cancel handles are leaked on purpose: the watcher lives as long as the process
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
    // give back a Mobile Hotspot a crashed run left configured (no-op without a breadcrumb)
    super::hosted_network::spawn_crash_recovery(&app);

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
        // the app's own thread, so it owns the apartment: every `enumerate_adapters` below
        // finds it already up
        let _com = match com::ensure_for_this_thread("network-watcher", com::Apartment::Mta) {
            Ok(scope) => scope,
            Err(e) => {
                teprintln!("[network-watcher] {e}");
                return;
            }
        };

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
            let _ = rx.recv_timeout(SAFETY_POLL);
            // coalesce the burst that follows a single change
            while rx.recv_timeout(DEBOUNCE).is_ok() {}

            let label = hosted_label(&app);
            let current = match enumerate_adapters(label.as_deref()) {
                Ok(adapters) => adapters,
                Err(e) => {
                    teprintln!("[network-watcher] enumeration failed, keeping last known set: {e}");
                    continue;
                }
            };

            // losing every adapter stops every streamer: confirm it before believing it
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

/// Physical medium of the adapter holding a local address, from `MSFT_NetAdapter`'s
/// `NdisPhysicalMedium`.
///
/// The values that matter are `NdisPhysicalMedium802_3` (14) and
/// `NdisPhysicalMediumNative802_11` (9), the pair `ADAPTER_QUERY` filters on. USB tethering and
/// RNDIS both present as 802.3, so a phone on a cable reads as wired.
#[derive(Deserialize, Debug)]
struct NetAdapterMedium {
    #[serde(rename = "InterfaceIndex")]
    interface_index: Option<u32>,
    #[serde(rename = "NdisPhysicalMedium")]
    ndis_physical_medium: Option<u32>,
}

const MEDIUM_QUERY: &str =
    "SELECT InterfaceIndex, NdisPhysicalMedium FROM MSFT_NetAdapter WHERE EndPointInterface = False";

/// Which physical medium a local IP is reached over, or `None` when nothing matched.
pub fn link_medium_for_local_ip(ip: &str) -> Option<crate::streamer::platform::LinkMedium> {
    use crate::streamer::platform::LinkMedium;

    let (_com, com_con) = com_for_this_thread("link-medium").ok()?;
    let wmi_con = WMIConnection::with_namespace_path("root\\StandardCimv2", com_con).ok()?;
    let ips: Vec<NetIPAddress> = wmi_con.raw_query(IP_QUERY).ok()?;
    let index = ips.iter().find_map(|entry| {
        (entry.ip_address.as_deref() == Some(ip))
            .then_some(entry.interface_index)
            .flatten()
    })?;
    let media: Vec<NetAdapterMedium> = wmi_con.raw_query(MEDIUM_QUERY).ok()?;
    let medium = media
        .iter()
        .find(|m| m.interface_index == Some(index))
        .and_then(|m| m.ndis_physical_medium)?;
    Some(match medium {
        14 => LinkMedium::Wired,
        1 | 9 => LinkMedium::Wireless,
        _ => LinkMedium::Other,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn hosted_ap_adapter_is_recognised_by_component_id() {
        // as `Get-NetAdapter -IncludeHidden` reports it on Windows 11 26200
        let wfd = Some(r"{5d624f94-8850-40c3-a3fa-a4fd2080baf3}\vwifimp_wfd");
        assert!(is_hosted_ap_adapter(
            wfd,
            Some("Microsoft Wi-Fi Direct Virtual Adapter")
        ));
        // a localized description does not matter
        assert!(is_hosted_ap_adapter(
            wfd,
            Some("Adaptateur virtuel Microsoft Wi-Fi Direct")
        ));
        assert!(is_hosted_ap_adapter(
            Some(r"{5D624F94-8850-40C3-A3FA-A4FD2080BAF3}\VWIFIMP_WFD"),
            None
        ));
        // the older hosted-network virtual adapter of the netsh tier
        assert!(is_hosted_ap_adapter(
            Some(r"{5d624f94-8850-40c3-a3fa-a4fd2080baf3}\vwifimp"),
            Some("Microsoft Hosted Network Virtual Adapter")
        ));
        // the physical Wi-Fi card and a VM adapter are not
        assert!(!is_hosted_ap_adapter(
            Some(r"PCI\VEN_8086&DEV_51F0&SUBSYS_00948086"),
            Some("Intel(R) Wi-Fi 6E AX211 160MHz")
        ));
        assert!(!is_hosted_ap_adapter(
            Some("*VMnetAdapter8"),
            Some("VMware Virtual Ethernet Adapter for VMnet8")
        ));
        // a component id that is present decides alone
        assert!(!is_hosted_ap_adapter(
            Some(r"ROOT\SOMETHING"),
            Some("Microsoft Wi-Fi Direct Virtual Adapter")
        ));
        // with no component id, the English description is the fallback
        assert!(is_hosted_ap_adapter(
            None,
            Some("Microsoft Wi-Fi Direct Virtual Adapter #2")
        ));
        assert!(is_hosted_ap_adapter(
            Some(""),
            Some("Microsoft Wi-Fi Direct Virtual Adapter")
        ));
        assert!(!is_hosted_ap_adapter(None, None));
    }
}

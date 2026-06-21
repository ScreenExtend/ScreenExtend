use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_store::StoreExt;
use tauri_specta::Event;
use wmi::{COMLibrary, WMIConnection};

use super::hosted_network::is_hosted_network;
use super::AppState;

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

fn hosted_network_name(app: &AppHandle, state: &State<'_, AppState>) -> String {
    let current_user = state.current_user.lock().unwrap().clone();

    let stored_name = app.store("config.json").ok().and_then(|config| {
        let user_data = config.get(&current_user)?;
        let name = user_data
            .get("hostedNetworkCredentials")?
            .get("name")?
            .as_str()?;
        Some(name.to_string())
    });

    stored_name.unwrap_or_else(|| {
        if current_user.is_empty() {
            "ScreenExtend".to_string()
        } else {
            format!("ScreenExtend-{current_user}")
        }
    })
}

#[tauri::command]
#[specta::specta]
pub fn get_network_adapters(app: AppHandle, state: State<'_, AppState>) -> Vec<NetworkInfo> {
    let com_con = unsafe { COMLibrary::assume_initialized() };
    let wmi_con = match WMIConnection::with_namespace_path("root\\StandardCimv2", com_con) {
        Ok(connection) => connection,
        Err(_) => return Vec::new(),
    };

    let adapter_query = "SELECT * FROM MSFT_NetAdapter\nWHERE EndPointInterface = False\nAND (NdisPhysicalMedium = 1 OR NdisPhysicalMedium = 9 OR NdisPhysicalMedium = 14)\nAND OperationalStatusDownMediaDisconnected = False";
    let adapters: Vec<NetAdapter> = match wmi_con.raw_query(adapter_query) {
        Ok(query) => query,
        Err(_) => return Vec::new(),
    };

    let ip_addresses: Vec<NetIPAddress> = match wmi_con.raw_query("SELECT * FROM MSFT_NetIPAddress") {
        Ok(query) => query,
        Err(_) => return Vec::new(),
    };

    let connection_profiles: Vec<NetConnectionProfile> =
        match wmi_con.raw_query("SELECT * FROM MSFT_NetConnectionProfile") {
            Ok(query) => query,
            Err(_) => return Vec::new(),
        };

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

    adapters
        .iter()
        .filter_map(|adapter| {
            let interface_index = adapter.interface_index?;

            let is_wifi_direct = adapter.driver_description.as_deref()
                == Some("Microsoft Wi-Fi Direct Virtual Adapter");
            let network_name = if is_wifi_direct {
                if is_hosted_network(app.clone(), state.clone()) {
                    hosted_network_name(&app, &state)
                } else {
                    "Personal Hotspot".to_string()
                }
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
        .collect()
}

#[tauri::command]
#[specta::specta]
pub fn watch_for_network_changes(app: AppHandle) {
    std::thread::spawn(move || {
        let _com = match COMLibrary::new() {
            Ok(com) => com,
            Err(error) => {
                tprintln!("[network-watcher] failed to initialize COM: {error:?}");
                return;
            }
        };

        let mut previous = get_network_adapters(app.clone(), app.state::<AppState>());
        tprintln!("[network-watcher] initial network adapters: {previous:?}");
        apply(&app, previous.clone());

        loop {
            std::thread::sleep(Duration::from_secs(1));
            let current = get_network_adapters(app.clone(), app.state::<AppState>());
            if current != previous {
                tprintln!("[network-watcher] network adapters changed: {current:?}");
                previous = current.clone();
                apply(&app, current);
            }
        }
    });
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

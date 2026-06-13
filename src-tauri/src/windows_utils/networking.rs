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

    let ip_query = "SELECT * FROM MSFT_NetIPAddress";
    let ip_addresses: Vec<NetIPAddress> = match wmi_con.raw_query(ip_query) {
        Ok(query) => query,
        Err(_) => return Vec::new(),
    };

    let profile_query = "SELECT * FROM MSFT_NetConnectionProfile";
    let connection_profiles: Vec<NetConnectionProfile> = match wmi_con.raw_query(profile_query) {
        Ok(query) => query,
        Err(_) => return Vec::new(),
    };

    let mut ip_map: HashMap<u32, Vec<&NetIPAddress>> = HashMap::new();
    for ip in &ip_addresses {
        if let Some(interface_index) = ip.interface_index {
            ip_map
                .entry(interface_index)
                .or_insert_with(Vec::new)
                .push(ip);
        }
    }

    let mut profile_map: HashMap<u32, &NetConnectionProfile> = HashMap::new();
    for profile in &connection_profiles {
        if let Some(interface_index) = profile.interface_index {
            profile_map.insert(interface_index, profile);
        }
    }

    let network_infos: Vec<NetworkInfo> = adapters
        .iter()
        .filter_map(|adapter| {
            if let Some(interface_index) = adapter.interface_index {
                let network_name = if adapter.driver_description.as_deref() == Some("Microsoft Wi-Fi Direct Virtual Adapter") {
                    if is_hosted_network(app.clone(), state.clone()) {
                        let temporary_name = match app.store("config.json") {
                            Ok(config) => {
                                if let Some(user_data) =
                                    config.get(state.current_user.lock().unwrap().clone())
                                {
                                    if let Some(credentials) =
                                        user_data.get("hostedNetworkCredentials")
                                    {
                                        if let Some(name) = credentials.get("name") {
                                            name.as_str().unwrap_or("Unknown").to_string()
                                        } else {
                                            "Unknown".to_string()
                                        }
                                    } else {
                                        "Unknown".to_string()
                                    }
                                } else {
                                    "Unknown".to_string()
                                }
                            }
                            Err(_) => "Unknown".to_string(),
                        };
                        if temporary_name == "Unknown" {
                            if state.current_user.lock().unwrap().len() > 0 {
                                "ScreenExtend".to_string() + &state.current_user.lock().unwrap()
                            } else {
                                "ScreenExtend".to_string()
                            }
                        } else {
                            temporary_name
                        }
                    } else {
                        "Personal Hotspot".to_string()
                    }
                } else {
                    if let Some(profile) = profile_map.get(&interface_index) {
                        profile.name.as_deref().unwrap_or("Unknown").to_string()
                    } else {
                        adapter.name.as_deref().unwrap_or("Unknown").to_string()
                    }
                };
                let ip_addresses = if let Some(ips) = ip_map.get(&interface_index) {
                    let ipv4_addresses: Vec<String> = ips
                        .iter()
                        .filter(|ip| ip.address_family == Some(2))
                        .filter_map(|ip| ip.ip_address.clone())
                        .collect();
                    let ipv6_addresses: Vec<String> = ips
                        .iter()
                        .filter(|ip| ip.address_family == Some(23))
                        .filter_map(|ip| ip.ip_address.clone())
                        .collect();
                    [ipv4_addresses, ipv6_addresses].concat()
                } else {
                    Vec::new()
                };
                Some(NetworkInfo {
                    network_name,
                    interface_index,
                    ip_addresses,
                })
            } else {
                None
            }
        })
        .collect();

    network_infos
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

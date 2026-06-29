use libc::{
    freeifaddrs, getifaddrs, ifaddrs, AF_INET, AF_INET6, IFF_LOOPBACK, IFF_RUNNING, IFF_UP,
};
use objc2_core_wlan::CWWiFiClient;
use objc2_foundation::NSString;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::mem::size_of;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_store::StoreExt;
use tauri_specta::Event;

use super::hosted_network::is_hosted_network;
use super::AppState;

const IFM_AVALID: i32 = 0x1;
const IFM_ACTIVE: i32 = 0x2;
const IFRTYPE_FUNCTIONAL_WIRED: i32 = 2;
const IFRTYPE_FUNCTIONAL_WIFI_INFRA: i32 = 3;

const fn iowr(num: u64, size: usize) -> u64 {
    0xc000_0000 | (((size as u64) & 0x1fff) << 16) | ((b'i' as u64) << 8) | num
}
const SIOCGIFMEDIA: u64 = iowr(56, size_of::<ifmediareq>());
const SIOCGIFFUNCTIONALTYPE: u64 = iowr(173, size_of::<ifreq>());

#[repr(C, packed(4))]
struct ifmediareq {
    ifm_name: [libc::c_char; libc::IFNAMSIZ],
    ifm_current: libc::c_int,
    ifm_mask: libc::c_int,
    ifm_status: libc::c_int,
    ifm_active: libc::c_int,
    ifm_count: libc::c_int,
    ifm_ulist: *mut libc::c_int,
}

#[repr(C)]
struct ifreq {
    name: [libc::c_char; libc::IFNAMSIZ],
    data: [u8; 16],
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event, PartialEq)]
pub struct NetworkInfo {
    pub network_name: String,
    pub interface_index: u32,
    pub ip_addresses: Vec<String>,
}

fn name_into(buf: &mut [libc::c_char], name: &str) {
    for (slot, &b) in buf.iter_mut().zip(name.as_bytes()).take(libc::IFNAMSIZ - 1) {
        *slot = b as libc::c_char;
    }
}

fn media_status(name: &str) -> Option<i32> {
    unsafe {
        let sock = libc::socket(AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            return None;
        }
        let mut req: ifmediareq = std::mem::zeroed();
        name_into(&mut req.ifm_name, name);
        let ret = libc::ioctl(sock, SIOCGIFMEDIA as _, &mut req as *mut _);
        libc::close(sock);
        (ret >= 0).then_some(req.ifm_status)
    }
}

fn functional_type(name: &str) -> Option<i32> {
    unsafe {
        let sock = libc::socket(AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            return None;
        }
        let mut req: ifreq = std::mem::zeroed();
        name_into(&mut req.name, name);
        let ret = libc::ioctl(sock, SIOCGIFFUNCTIONALTYPE as _, &mut req as *mut _);
        libc::close(sock);
        (ret >= 0).then(|| i32::from_ne_bytes(req.data[..4].try_into().unwrap()))
    }
}

fn interface_index(name: &str) -> Option<u32> {
    let c = CString::new(name).ok()?;
    match unsafe { libc::if_nametoindex(c.as_ptr()) } {
        0 => None,
        index => Some(index),
    }
}

fn wifi_ssid(name: &str) -> Option<String> {
    unsafe {
        let client = CWWiFiClient::sharedWiFiClient();
        let interface = client.interfaceWithName(Some(&NSString::from_str(name)))?;
        interface.ssid().map(|ssid| ssid.to_string())
    }
}

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

#[tauri::command]
#[specta::specta]
pub fn get_network_adapters(app: AppHandle, state: State<'_, AppState>) -> Vec<NetworkInfo> {
    let mut flags: BTreeMap<String, i32> = BTreeMap::new();
    let mut v4: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut v6: BTreeMap<String, Vec<String>> = BTreeMap::new();

    unsafe {
        let mut ifap: *mut ifaddrs = std::ptr::null_mut();
        if getifaddrs(&mut ifap) != 0 {
            return Vec::new();
        }

        let mut ifa = ifap;
        while !ifa.is_null() {
            let name = CStr::from_ptr((*ifa).ifa_name).to_string_lossy().into_owned();
            flags.insert(name.clone(), (*ifa).ifa_flags as i32);

            let addr = (*ifa).ifa_addr;
            if !addr.is_null() {
                match (*addr).sa_family as i32 {
                    AF_INET => {
                        let sin = addr as *const libc::sockaddr_in;
                        let ip = Ipv4Addr::from((*sin).sin_addr.s_addr.to_ne_bytes());
                        v4.entry(name).or_default().push(ip.to_string());
                    }
                    AF_INET6 => {
                        let sin6 = addr as *const libc::sockaddr_in6;
                        let ip = Ipv6Addr::from((*sin6).sin6_addr.s6_addr);
                        v6.entry(name).or_default().push(ip.to_string());
                    }
                    _ => {}
                }
            }

            ifa = (*ifa).ifa_next;
        }
        freeifaddrs(ifap);
    }

    flags
        .into_iter()
        .filter_map(|(name, fl)| {
            let want = IFF_UP | IFF_RUNNING;
            if fl & (want | IFF_LOOPBACK) != want {
                return None;
            }
            let status = media_status(&name)?;
            if status & (IFM_AVALID | IFM_ACTIVE) != (IFM_AVALID | IFM_ACTIVE) {
                return None;
            }
            let media = match functional_type(&name)? {
                IFRTYPE_FUNCTIONAL_WIFI_INFRA => "Wi-Fi",
                IFRTYPE_FUNCTIONAL_WIRED => "Ethernet",
                _ => return None,
            };
            let interface_index = interface_index(&name)?;

            let network_name = if media == "Wi-Fi" && is_hosted_network(app.clone(), state.clone()) {
                hosted_network_name(&app)
            } else if media == "Wi-Fi" {
                wifi_ssid(&name).unwrap_or_else(|| "Wi-Fi".to_string())
            } else {
                "Ethernet".to_string()
            };

            let mut ip_addresses = v4.remove(&name).unwrap_or_default();
            ip_addresses.extend(v6.remove(&name).unwrap_or_default());

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

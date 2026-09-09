use libc::{
    freeifaddrs, getifaddrs, ifaddrs, AF_INET, AF_INET6, IFF_LOOPBACK, IFF_RUNNING, IFF_UP,
};
use objc2_core_wlan::CWWiFiClient;
use objc2_foundation::NSString;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString};
use std::mem::size_of;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Mutex, OnceLock};
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

impl NetworkInfo {
    /// What the streamers actually bind to. `network_name` is display metadata: it flips whenever
    /// a `CWInterface.ssid` read comes back nil (which needs Location authorization on 10.15+) and
    /// a name-only flip must not tear the servers down and rebuild them.
    fn topology(&self) -> (u32, &[String]) {
        (self.interface_index, &self.ip_addresses)
    }
}

fn same_topology(a: &[NetworkInfo], b: &[NetworkInfo]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.topology() == y.topology())
}

fn name_into(buf: &mut [libc::c_char], name: &str) {
    for (slot, &b) in buf.iter_mut().zip(name.as_bytes()).take(libc::IFNAMSIZ - 1) {
        *slot = b as libc::c_char;
    }
}

/// One `AF_INET` socket serves the whole enumeration; the previous code opened and closed two per
/// interface per second.
struct IoctlSocket(libc::c_int);

impl IoctlSocket {
    fn open() -> Option<Self> {
        let fd = unsafe { libc::socket(AF_INET, libc::SOCK_DGRAM, 0) };
        (fd >= 0).then_some(Self(fd))
    }

    fn media_status(&self, name: &str) -> Option<i32> {
        unsafe {
            let mut req: ifmediareq = std::mem::zeroed();
            name_into(&mut req.ifm_name, name);
            let ret = libc::ioctl(self.0, SIOCGIFMEDIA as _, &mut req as *mut _);
            (ret >= 0).then_some(req.ifm_status)
        }
    }

    fn functional_type(&self, name: &str) -> Option<i32> {
        unsafe {
            let mut req: ifreq = std::mem::zeroed();
            name_into(&mut req.name, name);
            let ret = libc::ioctl(self.0, SIOCGIFFUNCTIONALTYPE as _, &mut req as *mut _);
            (ret >= 0).then(|| i32::from_ne_bytes(req.data[..4].try_into().unwrap()))
        }
    }
}

impl Drop for IoctlSocket {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// An interface's functional type does not change while the interface exists, so it is read once
/// per name rather than once per poll.
fn functional_type_cached(sock: &IoctlSocket, name: &str) -> Option<i32> {
    static CACHE: OnceLock<Mutex<HashMap<String, i32>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&cached) = cache.lock().unwrap().get(name) {
        return Some(cached);
    }
    let value = sock.functional_type(name)?;
    cache.lock().unwrap().insert(name.to_string(), value);
    Some(value)
}

fn interface_index(name: &str) -> Option<u32> {
    let c = CString::new(name).ok()?;
    match unsafe { libc::if_nametoindex(c.as_ptr()) } {
        0 => None,
        index => Some(index),
    }
}

/// `CWInterface.ssid` returns nil on macOS 10.15+ unless the process holds Location Services
/// authorization.
///
/// **ScreenExtend does not ask for Location, deliberately.** The SSID is a label on a dashboard
/// row; a Location prompt is one of the heaviest consents macOS shows, and asking for it to
/// decorate a row that already carries the adapter's addresses is not a trade worth making. So on
/// a Mac that has never granted it the row reads "Wi-Fi", on purpose rather than by accident, and
/// the reason is said once in the log instead of looking like a bug next to the Windows dashboard.
///
/// The last name we *did* read is still remembered, so a machine that has the authorization does
/// not flap to the placeholder on a single nil read.
fn wifi_ssid(name: &str) -> Option<String> {
    static LAST: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let last = LAST.get_or_init(|| Mutex::new(HashMap::new()));

    let live = unsafe {
        let client = CWWiFiClient::sharedWiFiClient();
        client
            .interfaceWithName(Some(&NSString::from_str(name)))
            .and_then(|interface| interface.ssid())
            .map(|ssid| ssid.to_string())
    };

    match live {
        Some(ssid) => {
            last.lock().unwrap().insert(name.to_string(), ssid.clone());
            Some(ssid)
        }
        None => {
            let cached = last.lock().unwrap().get(name).cloned();
            if cached.is_none() {
                static EXPLAINED: OnceLock<()> = OnceLock::new();
                EXPLAINED.get_or_init(|| {
                    tprintln!(
                        "[network] the Wi-Fi network name is unavailable: macOS treats an SSID as \
                         location data and only hands it to a process holding Location Services \
                         authorization, which ScreenExtend does not request. Adapters will be \
                         labelled \"Wi-Fi\"; their addresses are unaffected."
                    );
                });
            }
            cached
        }
    }
}

/// Whether AWDL — Apple Wireless Direct Link, the radio AirDrop, Handoff and Sidecar run on — is
/// up on this Mac, and which channel the Wi-Fi interface is associated to.
///
/// Both matter to latency and neither is something ScreenExtend can fix, so they are reported
/// once rather than acted on. A single-radio Mac time-shares between the infrastructure link and
/// AWDL's social channels (6, 44 and 149), which is measured elsewhere as periodic 50-200 ms
/// stalls on a stream — a jitter source the *user* can act on by narrowing AirDrop's
/// discoverability, and one that will otherwise be blamed on us.
fn awdl_is_up() -> bool {
    unsafe {
        let mut ifap: *mut ifaddrs = std::ptr::null_mut();
        if getifaddrs(&mut ifap) != 0 {
            return false;
        }
        let mut up = false;
        let mut ifa = ifap;
        while !ifa.is_null() {
            let name = CStr::from_ptr((*ifa).ifa_name).to_string_lossy();
            if name.starts_with("awdl") && (*ifa).ifa_flags as i32 & IFF_UP != 0 {
                up = true;
                break;
            }
            ifa = (*ifa).ifa_next;
        }
        freeifaddrs(ifap);
        up
    }
}

/// AWDL's three default social channels. Every nearby Apple device parks its AWDL interface on one
/// of these for discovery.
const AWDL_SOCIAL_CHANNELS: [isize; 3] = [6, 44, 149];

fn report_radio_conditions() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let channel = unsafe {
            CWWiFiClient::sharedWiFiClient()
                .interface()
                .and_then(|i| i.wlanChannel())
                .map(|c| c.channelNumber())
        };
        let awdl = awdl_is_up();
        match channel {
            Some(number) if AWDL_SOCIAL_CHANNELS.contains(&number) => teprintln!(
                "[network] this Mac's Wi-Fi is associated on channel {number}, one of AWDL's \
                 social channels (6, 44, 149){}. Expect periodic stalls while nearby Apple \
                 devices discover each other on it; a different channel on the router, or \
                 narrowing AirDrop discoverability, is the fix",
                if awdl { ", and awdl0 is up" } else { "" }
            ),
            Some(number) if awdl => tprintln!(
                "[network] Wi-Fi channel {number}, awdl0 up. A single-radio Mac retunes to AWDL's \
                 social channels periodically, which shows up as jitter on the stream; setting \
                 AirDrop to Contacts Only or Receiving Off reduces it"
            ),
            Some(number) => tprintln!("[network] Wi-Fi channel {number}, awdl0 down"),
            None if awdl => tprintln!("[network] awdl0 is up; Wi-Fi channel unavailable"),
            None => {}
        }
    });
}

/// The kernel's routing socket: how BSD announces interface and address changes.
///
/// `PF_ROUTE` predates macOS, needs no framework, no run loop and no entitlement, and it carries
/// exactly the events that change what the streamers bind — `RTM_NEWADDR`, `RTM_DELADDR`,
/// `RTM_IFINFO`. The previous watcher re-ran the whole `getifaddrs` + `ioctl` + CoreWLAN
/// enumeration once a second forever, whether or not anything had moved.
struct RouteSocket(libc::c_int);

impl RouteSocket {
    fn open() -> Option<Self> {
        let fd = unsafe { libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, 0) };
        (fd >= 0).then_some(Self(fd))
    }

    fn drain(&self) {
        let mut buf = [0u8; 2048];
        loop {
            let n = unsafe {
                libc::recv(
                    self.0,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                return;
            }
        }
    }

    /// Blocks until the kernel reports something or `timeout` elapses. The payload is not read for
    /// meaning: the enumeration below is the source of truth, and this only decides when to run it.
    fn wait(&self, timeout: Duration) -> bool {
        let mut fds = libc::pollfd {
            fd: self.0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        if unsafe { libc::poll(&mut fds, 1, ms) } <= 0 {
            return false;
        }
        self.drain();
        true
    }
}

impl Drop for RouteSocket {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// A Wi-Fi switch produces a burst of routing messages — link down, address removed, link up,
/// address added. One enumeration should cover the lot.
const ROUTE_DEBOUNCE: Duration = Duration::from_millis(250);
/// Re-enumerate this often even with a silent routing socket, so a message we somehow miss costs
/// at most this much rather than the life of the process.
const ROUTE_BACKSTOP: Duration = Duration::from_secs(30);
/// Only used when the routing socket cannot be opened at all.
const FALLBACK_POLL: Duration = Duration::from_secs(1);

/// How many full adapter enumerations the watcher has run. The whole point of the routing socket
/// is that this stops growing when nothing changes, so it is worth being able to read.
static ENUMERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn watch_enumerations() -> u64 {
    ENUMERATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The host's own routable IPv4 on one interface. Used to tell a hosted network that is merely
/// advertised from one that is actually serving DHCP.
pub fn interface_ipv4(name: &str) -> Option<Ipv4Addr> {
    unsafe {
        let mut ifap: *mut ifaddrs = std::ptr::null_mut();
        if getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut found = None;
        let mut ifa = ifap;
        while !ifa.is_null() {
            let this = CStr::from_ptr((*ifa).ifa_name).to_string_lossy();
            let addr = (*ifa).ifa_addr;
            if this == name && !addr.is_null() && (*addr).sa_family as i32 == AF_INET {
                let sin = addr as *const libc::sockaddr_in;
                let ip = Ipv4Addr::from((*sin).sin_addr.s_addr.to_ne_bytes());
                if !ip.is_link_local() && !ip.is_unspecified() {
                    found = Some(ip);
                    break;
                }
            }
            ifa = (*ifa).ifa_next;
        }
        freeifaddrs(ifap);
        found
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

/// The whole `getifaddrs` walk, once. `hosted_name` is `Some` only when the caller has already
/// established that this host is running its own AP — the previous code asked CoreWLAN that
/// question again for every Wi-Fi interface on every poll.
fn enumerate_adapters(hosted_name: Option<&str>) -> Vec<NetworkInfo> {
    let mut flags: BTreeMap<String, i32> = BTreeMap::new();
    let mut v4: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut v6: BTreeMap<String, Vec<String>> = BTreeMap::new();

    unsafe {
        let mut ifap: *mut ifaddrs = std::ptr::null_mut();
        if getifaddrs(&mut ifap) != 0 {
            teprintln!(
                "[network] getifaddrs failed ({}); reporting no adapters, which stops every \
                 streamer",
                std::io::Error::last_os_error()
            );
            return Vec::new();
        }

        let mut ifa = ifap;
        while !ifa.is_null() {
            let name = CStr::from_ptr((*ifa).ifa_name)
                .to_string_lossy()
                .into_owned();
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

    let Some(sock) = IoctlSocket::open() else {
        teprintln!("[network] could not open a socket for SIOCGIFMEDIA; reporting no adapters");
        return Vec::new();
    };

    flags
        .into_iter()
        .filter_map(|(name, fl)| {
            let want = IFF_UP | IFF_RUNNING;
            if fl & (want | IFF_LOOPBACK) != want {
                return None;
            }
            let status = sock.media_status(&name)?;
            if status & (IFM_AVALID | IFM_ACTIVE) != (IFM_AVALID | IFM_ACTIVE) {
                return None;
            }
            let media = match functional_type_cached(&sock, &name)? {
                IFRTYPE_FUNCTIONAL_WIFI_INFRA => "Wi-Fi",
                IFRTYPE_FUNCTIONAL_WIRED => "Ethernet",
                _ => return None,
            };
            let interface_index = interface_index(&name)?;

            let network_name = match (media, hosted_name) {
                ("Wi-Fi", Some(hosted)) => hosted.to_string(),
                ("Wi-Fi", None) => wifi_ssid(&name).unwrap_or_else(|| "Wi-Fi".to_string()),
                _ => "Ethernet".to_string(),
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
pub fn get_network_adapters(app: AppHandle, state: State<'_, AppState>) -> Vec<NetworkInfo> {
    ENUMERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let hosted = is_hosted_network(app.clone(), state).then(|| hosted_network_name(&app));
    enumerate_adapters(hosted.as_deref())
}

pub fn cli_adapters() -> Vec<NetworkInfo> {
    enumerate_adapters(None)
}

#[tauri::command]
#[specta::specta]
pub fn watch_for_network_changes(app: AppHandle) {
    // A Tauri command the frontend can call more than once. It used to start another identical
    // thread every time, each one enumerating and each one able to call `sync_streamers`.
    static WATCHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WATCHING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }

    std::thread::spawn(move || {
        report_radio_conditions();

        let mut previous = get_network_adapters(app.clone(), app.state::<AppState>());
        tprintln!("[network-watcher] initial network adapters: {previous:?}");
        apply(&app, previous.clone());

        let route = RouteSocket::open();
        match &route {
            Some(_) => tprintln!(
                "[network-watcher] watching the kernel routing socket; adapters are re-enumerated \
                 when something changes rather than once a second"
            ),
            None => teprintln!(
                "[network-watcher] could not open a PF_ROUTE socket ({}); falling back to polling \
                 once a second",
                std::io::Error::last_os_error()
            ),
        }

        loop {
            match &route {
                Some(sock) => {
                    if sock.wait(ROUTE_BACKSTOP) {
                        // Let the rest of a Wi-Fi switch's burst land before reading the world.
                        std::thread::sleep(ROUTE_DEBOUNCE);
                        sock.drain();
                    }
                }
                None => std::thread::sleep(FALLBACK_POLL),
            }

            let current = get_network_adapters(app.clone(), app.state::<AppState>());
            if same_topology(&current, &previous) {
                previous = current;
                continue;
            }
            tprintln!("[network-watcher] network adapters changed: {current:?}");
            previous = current.clone();
            apply(&app, current);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(index: u32, name: &str, ips: &[&str]) -> NetworkInfo {
        NetworkInfo {
            network_name: name.to_string(),
            interface_index: index,
            ip_addresses: ips.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_name_only_change_is_not_a_topology_change() {
        let before = [info(4, "Wi-Fi", &["192.168.1.20"])];
        let after = [info(4, "HomeNet", &["192.168.1.20"])];
        assert!(same_topology(&before, &after));
        assert_ne!(before[0], after[0]);
    }

    #[test]
    fn an_address_change_is_a_topology_change() {
        let before = [info(4, "Wi-Fi", &["192.168.1.20"])];
        let after = [info(4, "Wi-Fi", &["192.168.1.21"])];
        assert!(!same_topology(&before, &after));
    }

    #[test]
    fn the_awdl_social_channels_are_the_published_three() {
        assert_eq!(AWDL_SOCIAL_CHANNELS, [6, 44, 149]);
    }

    #[test]
    fn a_routing_socket_can_be_opened() {
        assert!(
            RouteSocket::open().is_some(),
            "PF_ROUTE is unavailable: {}",
            std::io::Error::last_os_error()
        );
    }

    /// What the watcher costs at idle, before and after.
    ///
    /// The old shape ran the full `getifaddrs` + `SIOCGIFMEDIA` + CoreWLAN enumeration once a
    /// second forever. The new one blocks on the routing socket. Both halves are measured over the
    /// same window on the same machine so the comparison is not against a remembered number.
    ///
    /// ```text
    /// cargo test --lib macos_utils::networking::tests::measure_adapter_watch_cost -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_adapter_watch_cost() {
        use std::time::Instant;
        const WINDOW: Duration = Duration::from_secs(30);

        let started = Instant::now();
        let mut polled = 0u32;
        while started.elapsed() < WINDOW {
            let _ = enumerate_adapters(None);
            polled += 1;
            std::thread::sleep(Duration::from_secs(1));
        }
        let interfaces = enumerate_adapters(None).len();

        let Some(sock) = RouteSocket::open() else {
            println!("PF_ROUTE unavailable; only the poll half was measured");
            return;
        };
        sock.drain();
        let started = Instant::now();
        let mut woken = 0u32;
        while started.elapsed() < WINDOW {
            if sock.wait(Duration::from_secs(1)) {
                woken += 1;
                std::thread::sleep(ROUTE_DEBOUNCE);
                sock.drain();
            }
        }

        println!(
            "over {}s at idle with {interfaces} reported adapters: poll shape {polled} \
             enumerations, routing-socket shape {woken}",
            WINDOW.as_secs()
        );
    }

    /// What this Mac will actually report, and why. Prints rather than asserts: the answers are
    /// properties of the machine's Location authorization and its radio, not of the code.
    ///
    /// ```text
    /// cargo test --lib macos_utils::networking::tests::report_wifi_naming_and_radio -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn report_wifi_naming_and_radio() {
        for adapter in enumerate_adapters(None) {
            println!(
                "adapter index {} named {:?} with {:?}",
                adapter.interface_index, adapter.network_name, adapter.ip_addresses
            );
        }
        println!("awdl0 up: {}", awdl_is_up());
        let channel = unsafe {
            CWWiFiClient::sharedWiFiClient()
                .interface()
                .and_then(|i| i.wlanChannel())
                .map(|c| c.channelNumber())
        };
        println!("associated channel: {channel:?}");
        report_radio_conditions();
    }
}

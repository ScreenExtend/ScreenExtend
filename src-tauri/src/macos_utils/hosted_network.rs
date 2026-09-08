use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{msg_send, sel};
use objc2_core_wlan::{CWChannel, CWChannelBand, CWInterface, CWInterfaceMode, CWWiFiClient};
use objc2_foundation::{NSData, NSError, NSString};
use tauri::{AppHandle, State};
use tauri_specta::Event;

use crate::macos_utils::AppState;

/// `securityType:` values for the private HostAP SPI.
///
/// 2 (open) and 16 (WEP) are the only two values ever confirmed, and they come from a 2012 sweep
/// that only covered [-127, 127] — so nothing at or above 128 has ever been tried on any macOS.
/// The candidates below are guesses ordered most-preferred first; the first one that both returns
/// YES and brings the radio up is remembered for the rest of the process. Override the whole
/// ladder with `SCREENEXTEND_HOSTAP_SECURITY=<n>` to sweep by hand on a machine we do not have.
const SECURITY_OPEN: u64 = 2;
const SECURITY_WEP: u64 = 16;
const SECURED_CANDIDATES: &[u64] = &[1024, 512, 256, 128, 64, 32, SECURITY_WEP];

/// `CWIBSSModeSecurity`: the deprecated public IBSS API offers open, WEP-40 and WEP-104 and
/// nothing else.
const IBSS_SECURITY_NONE: u64 = 0;
const IBSS_SECURITY_WEP104: u64 = 2;

/// 5 GHz first, because a 1080p60 stream does not fit comfortably in a crowded HT20 2.4 GHz
/// channel. 44 and 149 are omitted deliberately: with 6 they are AWDL's three social channels,
/// where every nearby Apple device parks its radio for AirDrop/Handoff discovery.
const PREFERRED_5GHZ_CHANNELS: [isize; 6] = [36, 40, 48, 153, 157, 161];
/// 2.4 GHz fallback. 11 rather than 6 for the same reason.
const PREFERRED_CHANNEL: isize = 11;

/// WPA-class passphrases are 8-63 characters; WEP-104 wants 13 ASCII. We accept the WPA floor and
/// let the SPI reject what it cannot use.
const MIN_PASSWORD_LEN: usize = 8;

const MODE_SETTLE_TIMEOUT: Duration = Duration::from_millis(2500);
const DHCP_SETTLE_TIMEOUT: Duration = Duration::from_millis(6000);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const WATCH_INTERVAL: Duration = Duration::from_secs(5);
/// How long an AP may stay up with nothing associated before saying so. Long enough for a person
/// to pick the network on a phone, type the passphrase and be handed a lease.
const ASSOCIATION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostedNetworkBackend {
    /// The private CoreWLAN `startHostAPModeWithSSID:securityType:channel:password:error:`.
    HostApSpi,
    /// The deprecated public `startIBSSModeWithSSID:security:channel:password:error:`.
    /// 2.4 GHz, 802.11g rates, open or WEP only.
    Ibss,
    /// Neither exists on this Mac: the user has to bring their own router.
    Unsupported,
}

impl HostedNetworkBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            HostedNetworkBackend::HostApSpi => "HostAP",
            HostedNetworkBackend::Ibss => "IBSS",
            HostedNetworkBackend::Unsupported => "Unsupported",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HostedNetworkStatus {
    pub backend: Option<&'static str>,
    pub ssid: String,
    pub channel: Option<isize>,
    pub band: Option<&'static str>,
    pub security: Option<String>,
    /// The platform accepted the start request.
    pub advertised: bool,
    /// The host holds a routable IPv4 on the AP interface, i.e. `bootpd` is serving.
    pub host_ipv4: Option<Ipv4Addr>,
    /// A client that actually associated, took a DHCP lease from us and reached the streamer.
    /// This is the only field that distinguishes an AP that works from one that merely exists.
    pub associated_peer: Option<Ipv4Addr>,
}

fn status() -> &'static Mutex<HostedNetworkStatus> {
    static STATUS: OnceLock<Mutex<HostedNetworkStatus>> = OnceLock::new();
    STATUS.get_or_init(|| Mutex::new(HostedNetworkStatus::default()))
}

pub fn hosted_network_status() -> HostedNetworkStatus {
    status().lock().unwrap().clone()
}

/// Bumped on every start and stop; the liveness watcher exits when its generation is stale.
static WATCH_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The `securityType:` value that last brought a secured AP up on this machine, so the guessing
/// ladder is walked at most once per process.
static KNOWN_SECURED_TYPE: OnceLock<u64> = OnceLock::new();

fn sel_start_host_ap() -> Sel {
    sel!(startHostAPModeWithSSID:securityType:channel:password:error:)
}

fn sel_stop_host_ap() -> Sel {
    sel!(stopHostAPMode)
}

fn sel_start_ibss() -> Sel {
    sel!(startIBSSModeWithSSID:security:channel:password:error:)
}

fn sel_stop_ibss() -> Sel {
    sel!(stopIBSSMode)
}

fn responds(interface: &CWInterface, selector: Sel) -> bool {
    unsafe { msg_send![interface, respondsToSelector: selector] }
}

fn wifi_interface() -> Option<Retained<CWInterface>> {
    unsafe { CWWiFiClient::sharedWiFiClient().interface() }
}

fn interface_bsd_name(interface: &CWInterface) -> Option<String> {
    unsafe { interface.interfaceName() }.map(|n| n.to_string())
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

/// Which mechanism this Mac can actually use to create a network, decided by asking the live
/// `CWInterface` whether it implements the selector — never by an OS version. A bare `msg_send!`
/// to a selector the class does not implement raises an Obj-C exception through a non-unwinding
/// Rust frame, which aborts the process, so this probe is load-bearing rather than cosmetic.
pub fn probe_hosted_network_backend() -> HostedNetworkBackend {
    static CACHED: OnceLock<HostedNetworkBackend> = OnceLock::new();
    if let Some(cached) = CACHED.get() {
        return *cached;
    }
    // No interface yet (radio hard-off, no Wi-Fi hardware) is not a verdict — do not cache it.
    let Some(interface) = wifi_interface() else {
        return HostedNetworkBackend::Unsupported;
    };
    let host_ap = responds(&interface, sel_start_host_ap());
    let ibss = responds(&interface, sel_start_ibss());
    let backend = if host_ap {
        HostedNetworkBackend::HostApSpi
    } else if ibss {
        HostedNetworkBackend::Ibss
    } else {
        HostedNetworkBackend::Unsupported
    };
    let backend = *CACHED.get_or_init(|| backend);
    tprintln!(
        "[hosted-network] backend probe: {} (startHostAPMode… {}, startIBSSMode… {})",
        backend.as_str(),
        if host_ap { "present" } else { "absent" },
        if ibss { "present" } else { "absent" }
    );
    backend
}

fn channel_band(channel: &CWChannel) -> &'static str {
    match unsafe { channel.channelBand() } {
        CWChannelBand::Band5GHz => "5 GHz",
        CWChannelBand::Band2GHz => "2.4 GHz",
        _ => "unknown band",
    }
}

fn channel_override() -> Option<isize> {
    std::env::var("SCREENEXTEND_HOSTAP_CHANNEL")
        .ok()?
        .trim()
        .parse::<isize>()
        .ok()
}

fn channel_numbered(interface: &CWInterface, number: isize) -> Option<Retained<CWChannel>> {
    let channels = unsafe { interface.supportedWLANChannels() }?;
    channels
        .iter()
        .find(|c| unsafe { c.channelNumber() } == number)
}

fn pick_channel(interface: &CWInterface) -> Option<Retained<CWChannel>> {
    if let Some(forced) = channel_override() {
        match channel_numbered(interface, forced) {
            Some(channel) => {
                tprintln!(
                    "[hosted-network] using channel {forced} from SCREENEXTEND_HOSTAP_CHANNEL"
                );
                return Some(channel);
            }
            None => teprintln!(
                "[hosted-network] SCREENEXTEND_HOSTAP_CHANNEL={forced} is not supported by this \
                 interface; falling back to the preferred list"
            ),
        }
    }

    let channels = unsafe { interface.supportedWLANChannels() }?;

    for wanted in PREFERRED_5GHZ_CHANNELS {
        for channel in channels.iter() {
            let is_5ghz = unsafe { channel.channelBand() } == CWChannelBand::Band5GHz;
            if is_5ghz && unsafe { channel.channelNumber() } == wanted {
                return Some(channel);
            }
        }
    }

    let mut fallback: Option<Retained<CWChannel>> = None;
    for channel in channels.iter() {
        if unsafe { channel.channelNumber() } == PREFERRED_CHANNEL {
            return Some(channel);
        }
        fallback.get_or_insert(channel);
    }

    fallback
}

fn security_override() -> Option<u64> {
    std::env::var("SCREENEXTEND_HOSTAP_SECURITY")
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

fn open_fallback_allowed() -> bool {
    !matches!(
        std::env::var("SCREENEXTEND_HOSTAP_ALLOW_OPEN").as_deref(),
        Ok("0") | Ok("false") | Ok("no")
    )
}

fn send_start_host_ap(
    interface: &CWInterface,
    channel: &CWChannel,
    ssid: &str,
    security_type: u64,
    password: Option<&str>,
) -> bool {
    if !responds(interface, sel_start_host_ap()) {
        teprintln!("[hosted-network] startHostAPMode… is not implemented on this Mac");
        return false;
    }

    let ssid_data = NSData::with_bytes(ssid.as_bytes());
    let password_obj = password.map(NSString::from_str);
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
        tprintln!("[hosted-network] securityType {security_type} rejected: {detail}");
    }
    success
}

fn send_start_ibss(
    interface: &CWInterface,
    channel_number: u64,
    ssid: &str,
    security: u64,
    password: Option<&str>,
) -> bool {
    if !responds(interface, sel_start_ibss()) {
        return false;
    }

    let ssid_data = NSData::with_bytes(ssid.as_bytes());
    let password_obj = password.map(NSString::from_str);
    let password_ptr: *const NSString = match &password_obj {
        Some(pw) => Retained::as_ptr(pw),
        None => std::ptr::null(),
    };

    let mut error: *mut NSError = std::ptr::null_mut();
    let success: bool = unsafe {
        msg_send![
            interface,
            startIBSSModeWithSSID: &*ssid_data,
            security: security,
            channel: channel_number,
            password: password_ptr,
            error: &mut error,
        ]
    };

    if !success {
        let detail = unsafe { error.as_ref() }
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_else(|| "unknown error".to_string());
        tprintln!("[hosted-network] IBSS start rejected: {detail}");
    }
    success
}

fn stop_host_ap_mode() {
    WATCH_GENERATION.fetch_add(1, Ordering::Relaxed);
    if let Some(interface) = wifi_interface() {
        if responds(&interface, sel_stop_host_ap()) {
            unsafe {
                let _: () = msg_send![&*interface, stopHostAPMode];
            }
        }
        if responds(&interface, sel_stop_ibss()) {
            unsafe {
                let _: () = msg_send![&*interface, stopIBSSMode];
            }
        }
    }
    *status().lock().unwrap() = HostedNetworkStatus::default();
}

fn interface_mode_is_host_ap(interface: &CWInterface) -> bool {
    let mode = unsafe { interface.interfaceMode() };
    mode == CWInterfaceMode::HostAP || mode == CWInterfaceMode::IBSS
}

/// A start that returned YES has told us nothing about the radio: wait for the interface to
/// actually enter AP mode. `start_hosted_network` is a synchronous Tauri command, so this is the
/// only wait it is allowed to make the user sit through.
fn wait_for_ap_mode(interface: &CWInterface) -> bool {
    let deadline = Instant::now() + MODE_SETTLE_TIMEOUT;
    loop {
        if interface_mode_is_host_ap(interface) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// AP mode with no address on the interface means nothing is serving DHCP: clients will see the
/// network and fail to join. This is the difference between advertised and working, and it settles
/// a second or two after the start, so it is watched rather than waited on.
fn wait_for_ipv4(name: &str, timeout: Duration) -> Option<Ipv4Addr> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(ip) = super::networking::interface_ipv4(name) {
            return Some(ip);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Finish the verification the command thread could not afford to wait for, then keep watching.
///
/// Phase one asks whether the host got a routable IPv4 on the AP interface and says plainly which
/// of the two outcomes happened. Phase two notices an AP that goes away on its own — a radio reset,
/// a Wi-Fi mode change from the menu bar, or the OS reclaiming the interface — which is what
/// otherwise leaves the dashboard with a green light on a dead AP.
fn watch_liveness(app: AppHandle, generation: u64, ssid: String, iface: Option<String>) {
    std::thread::spawn(move || {
        use tauri::Manager;

        let host_ipv4 = iface
            .as_deref()
            .and_then(|name| wait_for_ipv4(name, DHCP_SETTLE_TIMEOUT));
        if WATCH_GENERATION.load(Ordering::Relaxed) != generation {
            return;
        }
        {
            let mut guard = status().lock().unwrap();
            guard.host_ipv4 = host_ipv4;
            match (host_ipv4, guard.channel, guard.band, guard.security.clone()) {
                (Some(ip), Some(channel), Some(band), Some(security)) => tprintln!(
                    "[hosted-network] '{ssid}' verified — channel {channel} ({band}), {security}, \
                     host address {ip}"
                ),
                (None, ..) => teprintln!(
                    "[hosted-network] '{ssid}' is advertised but the interface never got an IPv4 \
                     address; clients will see the network and fail to join because nothing is \
                     handing out DHCP leases"
                ),
                _ => {}
            }
        }

        let since = Instant::now();
        let mut announced_association = false;
        let mut announced_silence = false;

        loop {
            std::thread::sleep(WATCH_INTERVAL);
            if WATCH_GENERATION.load(Ordering::Relaxed) != generation {
                return;
            }

            // "Started" has never meant "a client can join". Look for one.
            if let (Some(host), false) = (host_ipv4, announced_association) {
                if let Some(peer) = associated_peer(&app, host) {
                    status().lock().unwrap().associated_peer = Some(peer);
                    tprintln!(
                        "[hosted-network] '{ssid}' carried a client: {peer} associated, took a                          lease and joined"
                    );
                    announced_association = true;
                }
            }
            if !announced_association
                && !announced_silence
                && since.elapsed() >= ASSOCIATION_TIMEOUT
            {
                announced_silence = true;
                match host_ipv4 {
                    Some(host) => teprintln!(
                        "[hosted-network] '{ssid}' has been up for {}s on {host} with nothing                          associated. The usual causes, in order: the client refused the security                          type this Mac accepted; the client's radio does not support the band or                          channel in use; or the passphrase was mistyped. Check what the network                          looks like from the client before assuming the host is at fault",
                        ASSOCIATION_TIMEOUT.as_secs()
                    ),
                    None => teprintln!(
                        "[hosted-network] '{ssid}' has been up for {}s with no address on the                          interface and nothing associated; nothing is serving DHCP, so a client                          that does associate still cannot join",
                        ASSOCIATION_TIMEOUT.as_secs()
                    ),
                }
            }

            let still_up = wifi_interface()
                .map(|i| interface_mode_is_host_ap(&i))
                .unwrap_or(false);
            if still_up {
                continue;
            }
            if WATCH_GENERATION.load(Ordering::Relaxed) != generation {
                return;
            }
            teprintln!(
                "[hosted-network] '{ssid}' is no longer in AP mode — the OS or the radio dropped it"
            );
            *status().lock().unwrap() = HostedNetworkStatus::default();
            if let Some(state) = app.try_state::<AppState>() {
                *state.hosted_network_running.lock().unwrap() = false;
                *state.stop_hosted_network.lock().unwrap() = None;
            }
            return;
        }
    });
}

/// A client of *our* AP, as opposed to one that reached us over the wired LAN.
///
/// The hosted network hands out a /24 around the host's own address, so a session whose client IP
/// shares that /24 came in over the AP. This is the promotion from "advertised" to "verified":
/// nothing before it observes a client at all.
fn associated_peer(app: &AppHandle, host: Ipv4Addr) -> Option<Ipv4Addr> {
    use tauri::Manager;
    let state = app.try_state::<AppState>()?;
    let sessions = state.sessions.clone();
    crate::streamer::session::client_ips(&sessions)
        .into_iter()
        .filter_map(|ip| ip.parse::<Ipv4Addr>().ok())
        .find(|peer| peer.octets()[..3] == host.octets()[..3] && *peer != host)
}

struct Attempt {
    security: u64,
    label: String,
    password: Option<String>,
}

fn secured_attempts(password: &str) -> Vec<Attempt> {
    if let Some(forced) = security_override() {
        return vec![Attempt {
            security: forced,
            label: format!("securityType {forced} (SCREENEXTEND_HOSTAP_SECURITY)"),
            password: Some(password.to_string()),
        }];
    }
    if let Some(&known) = KNOWN_SECURED_TYPE.get() {
        return vec![Attempt {
            security: known,
            label: format!("securityType {known} (previously accepted)"),
            password: Some(password.to_string()),
        }];
    }
    SECURED_CANDIDATES
        .iter()
        .map(|&security| Attempt {
            security,
            label: if security == SECURITY_WEP {
                "securityType 16 (WEP — crackable, and modern phones warn about it)".to_string()
            } else {
                format!("securityType {security} (unconfirmed)")
            },
            password: Some(password.to_string()),
        })
        .collect()
}

struct StartOutcome {
    channel: isize,
    band: &'static str,
    security: String,
}

/// Try one (channel, security) pair and only report success once the radio agrees.
fn try_attempt(
    interface: &CWInterface,
    channel: &CWChannel,
    ssid: &str,
    attempt: &Attempt,
) -> bool {
    let number = unsafe { channel.channelNumber() };
    tprintln!(
        "[hosted-network] trying '{ssid}' on channel {number} ({}) with {}",
        channel_band(channel),
        attempt.label
    );
    if !send_start_host_ap(
        interface,
        channel,
        ssid,
        attempt.security,
        attempt.password.as_deref(),
    ) {
        return false;
    }
    if !wait_for_ap_mode(interface) {
        teprintln!(
            "[hosted-network] {} returned YES but the interface never entered AP mode; \
             stopping and moving on",
            attempt.label
        );
        stop_host_ap_mode();
        return false;
    }
    true
}

fn try_ibss(interface: &CWInterface, ssid: &str, password: Option<&str>) -> Option<u64> {
    let (security, pw) = match password {
        Some(pw) => (IBSS_SECURITY_WEP104, Some(pw)),
        None => (IBSS_SECURITY_NONE, None),
    };
    tprintln!(
        "[hosted-network] falling back to IBSS on 2.4 GHz channel {PREFERRED_CHANNEL}. IBSS is \
         802.11g-rate and open/WEP only, so the link tops out near 20 Mbit/s of TCP goodput — \
         enough for a scaled-down stream, not for 1080p60"
    );
    if !send_start_ibss(interface, PREFERRED_CHANNEL as u64, ssid, security, pw) {
        return None;
    }
    if !wait_for_ap_mode(interface) {
        stop_host_ap_mode();
        return None;
    }
    Some(security)
}

fn start_via_host_ap(
    interface: &CWInterface,
    name: &str,
    password: &str,
    had_password: bool,
    app: &AppHandle,
) -> Option<StartOutcome> {
    let primary = pick_channel(interface)?;
    let fallback = if unsafe { primary.channelNumber() } == PREFERRED_CHANNEL {
        None
    } else {
        channel_numbered(interface, PREFERRED_CHANNEL)
    };
    let channels: Vec<Retained<CWChannel>> = std::iter::once(primary).chain(fallback).collect();

    if had_password {
        for (index, channel) in channels.iter().enumerate() {
            if index > 0 {
                std::thread::sleep(Duration::from_millis(250));
            }
            for attempt in secured_attempts(password) {
                if try_attempt(interface, channel, name, &attempt) {
                    let _ = KNOWN_SECURED_TYPE.set(attempt.security);
                    return Some(StartOutcome {
                        channel: unsafe { channel.channelNumber() },
                        band: channel_band(channel),
                        security: attempt.label,
                    });
                }
            }
        }
        if !open_fallback_allowed() {
            teprintln!(
                "[hosted-network] no secured start succeeded and the open fallback is disabled by \
                 SCREENEXTEND_HOSTAP_ALLOW_OPEN"
            );
            return None;
        }
        teprintln!(
            "[hosted-network] this Mac accepted none of the secured security types, so the only \
             network it can create is unencrypted"
        );
    }

    let open = Attempt {
        security: SECURITY_OPEN,
        label: "securityType 2 (open — no encryption)".to_string(),
        password: None,
    };
    for (index, channel) in channels.iter().enumerate() {
        if index > 0 {
            std::thread::sleep(Duration::from_millis(250));
        }
        if try_attempt(interface, channel, name, &open) {
            if had_password {
                let _ = crate::HostedNetworkNoPassword.emit(app);
            }
            return Some(StartOutcome {
                channel: unsafe { channel.channelNumber() },
                band: channel_band(channel),
                security: open.label,
            });
        }
    }
    None
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
        teprintln!("[hosted-network] no Wi-Fi interface — this Mac cannot host a network");
        *state.hosted_network_running.lock().unwrap() = false;
        return false;
    };

    let backend = probe_hosted_network_backend();
    if backend == HostedNetworkBackend::Unsupported {
        teprintln!(
            "[hosted-network] this Mac exposes neither the HostAP SPI nor IBSS, so ScreenExtend \
             cannot create a network here. Connect both devices to the same Wi-Fi router instead."
        );
        *state.hosted_network_running.lock().unwrap() = false;
        return false;
    }

    let had_password = !password.is_empty();
    if had_password && password.len() < MIN_PASSWORD_LEN {
        teprintln!(
            "[hosted-network] password is {} characters; Wi-Fi passphrases must be at least \
             {MIN_PASSWORD_LEN}",
            password.len()
        );
        *state.hosted_network_running.lock().unwrap() = false;
        let _ = crate::HostedNetworkNoPassword.emit(&app);
        return false;
    }

    // Start from a known state, but only if there is something to stop: sending stopHostAPMode to
    // an interface that is associated as a station is unverified territory, and this Mac's Wi-Fi is
    // very likely the link the user is about to stream over.
    if interface_mode_is_host_ap(&interface) {
        tprintln!("[hosted-network] the interface is already in AP mode; stopping it first");
        stop_host_ap_mode();
    }

    let outcome = if backend == HostedNetworkBackend::HostApSpi {
        start_via_host_ap(&interface, name, password, had_password, &app)
    } else {
        let secured = if had_password { Some(password) } else { None };
        try_ibss(&interface, name, secured)
            .or_else(|| {
                if had_password && open_fallback_allowed() {
                    let started = try_ibss(&interface, name, None)?;
                    let _ = crate::HostedNetworkNoPassword.emit(&app);
                    Some(started)
                } else {
                    None
                }
            })
            .map(|security| StartOutcome {
                channel: PREFERRED_CHANNEL,
                band: "2.4 GHz",
                security: format!("IBSS security {security}"),
            })
    };

    let Some(outcome) = outcome else {
        teprintln!("[hosted-network] every start attempt failed; the Wi-Fi interface is unchanged");
        *state.hosted_network_running.lock().unwrap() = false;
        *status().lock().unwrap() = HostedNetworkStatus::default();
        return false;
    };

    tprintln!(
        "[hosted-network] '{name}' is up on {} — channel {} ({}), {}; checking for a DHCP address",
        backend.as_str(),
        outcome.channel,
        outcome.band,
        outcome.security
    );

    *status().lock().unwrap() = HostedNetworkStatus {
        backend: Some(backend.as_str()),
        ssid: name.to_string(),
        channel: Some(outcome.channel),
        band: Some(outcome.band),
        security: Some(outcome.security),
        advertised: true,
        host_ipv4: None,
        associated_peer: None,
    };

    *state.stop_hosted_network.lock().unwrap() = Some(Box::new(stop_host_ap_mode));
    *state.hosted_network_running.lock().unwrap() = true;
    watch_liveness(
        app,
        WATCH_GENERATION.load(Ordering::Relaxed),
        name.to_string(),
        interface_bsd_name(&interface),
    );
    true
}

#[tauri::command]
#[specta::specta]
pub fn stop_hosted_network(_app: AppHandle, state: State<'_, AppState>) -> bool {
    match state.stop_hosted_network.lock().unwrap().take() {
        // We started it, so stop it unconditionally — the radio may not have followed.
        Some(stop) => stop(),
        // We did not. Only send the SPI if there is actually an AP to stop; otherwise this is a
        // private selector aimed at the interface the user is currently associated through.
        None => {
            let in_ap_mode = wifi_interface()
                .map(|i| interface_mode_is_host_ap(&i))
                .unwrap_or(false);
            if in_ap_mode {
                stop_host_ap_mode();
            }
        }
    }
    *state.hosted_network_running.lock().unwrap() = false;

    let stopped = !wifi_interface()
        .map(|i| interface_mode_is_host_ap(&i))
        .unwrap_or(false);
    if !stopped {
        teprintln!("[hosted-network] stop requested but the interface is still in AP mode");
    }
    stopped
}

#[tauri::command]
#[specta::specta]
pub fn is_hosted_network(_app: AppHandle, state: State<'_, AppState>) -> bool {
    let live = wifi_interface()
        .map(|interface| interface_mode_is_host_ap(&interface))
        .unwrap_or(false);
    live || *state.hosted_network_running.lock().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_channels_avoid_the_awdl_social_channels() {
        for social in [6, 44, 149] {
            assert!(
                !PREFERRED_5GHZ_CHANNELS.contains(&social),
                "channel {social} is an AWDL social channel"
            );
            assert_ne!(PREFERRED_CHANNEL, social);
        }
    }

    #[test]
    fn wep_is_the_last_secured_candidate() {
        assert_eq!(SECURED_CANDIDATES.last(), Some(&SECURITY_WEP));
        assert!(!SECURED_CANDIDATES.contains(&SECURITY_OPEN));
    }

    /// Read-only inventory of what this Mac's Wi-Fi interface offers. Safe to run anywhere — it
    /// starts nothing and changes nothing:
    ///
    /// ```text
    /// cargo test --lib macos_utils::hosted_network::tests::report_hostap_surface -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn report_hostap_surface() {
        let Some(interface) = wifi_interface() else {
            println!("no Wi-Fi interface");
            return;
        };
        println!("interface        : {:?}", interface_bsd_name(&interface));
        println!("interfaceMode    : {:?}", unsafe {
            interface.interfaceMode()
        });
        println!("powerOn          : {}", unsafe { interface.powerOn() });
        for (label, selector) in [
            (
                "startHostAPModeWithSSID:securityType:channel:password:error:",
                sel_start_host_ap(),
            ),
            ("stopHostAPMode", sel_stop_host_ap()),
            ("startHostAPMode:", sel!(startHostAPMode:)),
            ("enableHostAPMode", sel!(enableHostAPMode)),
            ("disableHostAPMode", sel!(disableHostAPMode)),
            (
                "startIBSSModeWithSSID:security:channel:password:error:",
                sel_start_ibss(),
            ),
            ("stopIBSSMode", sel_stop_ibss()),
        ] {
            println!(
                "responds         : {:<62} {}",
                label,
                responds(&interface, selector)
            );
        }
        println!(
            "backend          : {}",
            probe_hosted_network_backend().as_str()
        );
        if let Some(channels) = unsafe { interface.supportedWLANChannels() } {
            let mut five: Vec<isize> = Vec::new();
            let mut two: Vec<isize> = Vec::new();
            for channel in channels.iter() {
                let n = unsafe { channel.channelNumber() };
                match unsafe { channel.channelBand() } {
                    CWChannelBand::Band5GHz => five.push(n),
                    CWChannelBand::Band2GHz => two.push(n),
                    _ => {}
                }
            }
            five.sort_unstable();
            two.sort_unstable();
            println!("5 GHz channels   : {five:?}");
            println!("2.4 GHz channels : {two:?}");
        }
        println!(
            "would pick       : {:?}",
            pick_channel(&interface).map(|c| unsafe { c.channelNumber() })
        );
    }

    /// One bounded run of exactly what shipping code does, with every number the ledger wants:
    /// which securityType the SPI accepts, on which channel and band, how long the radio takes to
    /// reach AP mode, and whether anything then serves DHCP.
    ///
    /// **This takes the Wi-Fi interface.** On a Mac whose only network is that interface — which
    /// is this project's dev box — it drops every remote session, so it has to be run detached
    /// with a watchdog that restores the radio. `docs/manual-tests/macos-hosted-network.md` has
    /// the script.
    ///
    /// ```text
    /// cargo test --lib macos_utils::hosted_network::tests::measure_hostap_start -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "takes the Wi-Fi interface; see docs/manual-tests/macos-hosted-network.md"]
    fn measure_hostap_start() {
        let Some(interface) = wifi_interface() else {
            println!("no Wi-Fi interface");
            return;
        };
        println!("backend: {:?}", probe_hosted_network_backend());
        let Some(channel) = pick_channel(&interface) else {
            println!("no usable channel");
            return;
        };
        let number = unsafe { channel.channelNumber() };
        println!("channel {number} ({})", channel_band(&channel));
        let name = interface_bsd_name(&interface);
        println!("interface {name:?}");

        let ssid = "ScreenExtend-Measure";
        let password = "screenextend";
        for attempt in secured_attempts(password).into_iter().chain([Attempt {
            security: SECURITY_OPEN,
            label: "securityType 2 (open)".to_string(),
            password: None,
        }]) {
            let t0 = Instant::now();
            let accepted = send_start_host_ap(
                &interface,
                &channel,
                ssid,
                attempt.security,
                attempt.password.as_deref(),
            );
            let accepted_at = t0.elapsed();
            let mode_ok = accepted && wait_for_ap_mode(&interface);
            let mode_at = t0.elapsed();
            let ip = mode_ok
                .then(|| name.clone())
                .flatten()
                .and_then(|n| wait_for_ipv4(&n, DHCP_SETTLE_TIMEOUT));
            let ip_at = t0.elapsed();
            println!(
                "{}: accepted={accepted} in {accepted_at:?}, ap_mode={mode_ok} at {mode_at:?},                  ip={ip:?} at {ip_at:?}",
                attempt.label
            );
            stop_host_ap_mode();
            std::thread::sleep(Duration::from_millis(400));
            if mode_ok {
                break;
            }
        }
        println!("stopped; the radio should rejoin its known network on its own");
    }

    /// The security-type sweep the tree has been carrying as a 2012 comment. Run it on a real Mac
    /// with a phone nearby:
    ///
    /// ```text
    /// cargo test --lib macos_utils::hosted_network::tests::sweep_security_types -- --ignored --nocapture
    /// ```
    ///
    /// It records, for every candidate, whether the SPI returned YES and whether the radio then
    /// actually entered AP mode. Scan from a second device to see what each accepted value
    /// advertises.
    #[test]
    #[ignore = "takes the Wi-Fi interface; see docs/manual-tests/macos-hosted-network.md"]
    fn sweep_security_types() {
        let Some(interface) = wifi_interface() else {
            println!("no Wi-Fi interface");
            return;
        };
        if !responds(&interface, sel_start_host_ap()) {
            println!("startHostAPModeWithSSID:… is absent on this Mac");
            return;
        }
        let Some(channel) = pick_channel(&interface) else {
            println!("no usable channel");
            return;
        };
        let ssid = "ScreenExtend-Sweep";
        let password = "screenextend";
        let mut candidates: Vec<u64> = (0..=64).collect();
        candidates.extend([96, 128, 160, 192, 256, 384, 512, 768, 1024, 2048]);
        for security in candidates {
            let accepted = send_start_host_ap(&interface, &channel, ssid, security, Some(password));
            let mode_ok = accepted && wait_for_ap_mode(&interface);
            let ip = mode_ok
                .then(|| interface_bsd_name(&interface))
                .flatten()
                .and_then(|name| wait_for_ipv4(&name, DHCP_SETTLE_TIMEOUT));
            println!("securityType {security:5}: accepted={accepted} ap_mode={mode_ok} ip={ip:?}");
            stop_host_ap_mode();
            std::thread::sleep(Duration::from_millis(400));
        }
    }
}

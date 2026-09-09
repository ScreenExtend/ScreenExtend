//! The host-created Wi-Fi network, as a probed ladder of backends.
//!
//! Newest first: **Mobile Hotspot** (`NetworkOperatorTetheringManager`, the only one with band
//! control), **Wi-Fi Direct** legacy AP (`WiFiDirectAdvertisementPublisher`, no band control),
//! **netsh legacy** (`netsh wlan hostednetwork`, deprecated since Windows 10 1607), then
//! unsupported. Each tier is selected by attempting the capability, never by a version number.
//!
//! "Started" here means the radio is up *and* the virtual adapter has a routable IPv4 — an AP
//! that never serves DHCP is a failure the user would otherwise experience as a wrong OTP.

use crate::windows_utils::AppState;
use elevated_command::Command;
use std::process::Command as StdCommand;
use std::sync::mpsc::channel;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::AppHandle;
use tauri::State;
use tauri_plugin_shell::ShellExt;
use windows::core::{Result, HSTRING};
use windows::Devices::Radios::{Radio, RadioAccessStatus, RadioKind, RadioState};
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementPublisher, WiFiDirectAdvertisementPublisherStatus,
    WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
};
use windows::Foundation::TypedEventHandler;
use windows::Security::Credentials::PasswordCredential;

use windows::Networking::{Connectivity::*, NetworkOperators::*};

/// How long to wait for the Wi-Fi Direct publisher to report a terminal status.
const PUBLISHER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the AP's virtual adapter to pick up a routable IPv4 after the radio
/// reports it is up. Windows normally assigns 192.168.137.1 within a second or two.
const ADDRESS_TIMEOUT: Duration = Duration::from_secs(8);
const ADDRESS_POLL: Duration = Duration::from_millis(400);

/// 802.11 requires a WPA2 passphrase of 8..=63 ASCII characters.
const MIN_PASSPHRASE: usize = 8;
const MAX_PASSPHRASE: usize = 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedNetworkBackend {
    MobileHotspot,
    WiFiDirect,
    NetshLegacy,
    Unsupported,
}

impl HostedNetworkBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MobileHotspot => "MobileHotspot",
            Self::WiFiDirect => "WiFiDirect",
            Self::NetshLegacy => "NetshLegacy",
            Self::Unsupported => "Unsupported",
        }
    }
}

/// What we know about the hosted network right now. `advertised` means the platform accepted
/// the start; `host_ipv4` being set is the evidence that a client could actually associate.
#[derive(Clone, Debug, Default)]
pub struct HostedNetworkStatus {
    pub backend: Option<HostedNetworkBackend>,
    pub ssid: String,
    pub band: Option<&'static str>,
    pub advertised: bool,
    pub host_ipv4: Option<String>,
}

/// `NetworkOperatorTetheringManager` is not agile, so it cannot be parked in a `static` or in
/// `AppState`; it is cheap to rebuild from a connection profile, and that is what stop and
/// status do. The Wi-Fi Direct publisher *is* agile and must be kept, since dropping it is the
/// only way the advertisement ends.
enum Active {
    MobileHotspot,
    WiFiDirect(WiFiDirectAdvertisementPublisher),
    NetshLegacy,
}

fn active() -> &'static Mutex<Option<Active>> {
    static ACTIVE: OnceLock<Mutex<Option<Active>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(None))
}

fn status() -> &'static Mutex<HostedNetworkStatus> {
    static STATUS: OnceLock<Mutex<HostedNetworkStatus>> = OnceLock::new();
    STATUS.get_or_init(|| Mutex::new(HostedNetworkStatus::default()))
}

pub fn hosted_network_status() -> HostedNetworkStatus {
    status().lock().unwrap().clone()
}

// -- tier probes ------------------------------------------------------------------------------

/// Any connection profile will do to reach the tethering manager — Microsoft's own sample uses
/// the internet profile, but `CreateFromConnectionProfile` succeeds on an Ethernet or even a
/// saved-but-not-connected Wi-Fi profile, and the whole point of a hosted network is that there
/// may be no internet at all.
fn any_connection_profile() -> Option<ConnectionProfile> {
    if let Ok(profile) = NetworkInformation::GetInternetConnectionProfile() {
        return Some(profile);
    }
    let profiles = NetworkInformation::GetConnectionProfiles().ok()?;
    profiles.into_iter().next()
}

fn capability_name(capability: TetheringCapability) -> &'static str {
    match capability {
        TetheringCapability::Enabled => "Enabled",
        TetheringCapability::DisabledByGroupPolicy => "DisabledByGroupPolicy",
        TetheringCapability::DisabledByHardwareLimitation => "DisabledByHardwareLimitation",
        TetheringCapability::DisabledByOperator => "DisabledByOperator",
        TetheringCapability::DisabledBySku => "DisabledBySku",
        TetheringCapability::DisabledByRequiredAppNotInstalled => {
            "DisabledByRequiredAppNotInstalled"
        }
        TetheringCapability::DisabledDueToUnknownCause => "DisabledDueToUnknownCause",
        TetheringCapability::DisabledBySystemCapability => "DisabledBySystemCapability",
        _ => "Unknown",
    }
}

/// The real Mobile Hotspot probe. `DisabledBySystemCapability` is Microsoft's documented signal
/// that the caller lacks the `wiFiControl` manifest capability — measured `Enabled` from this
/// unpackaged binary on Windows 11 26200, so unpackaged desktop apps are not blocked by it.
pub fn tethering_capability() -> Option<TetheringCapability> {
    let profile = any_connection_profile()?;
    NetworkOperatorTetheringManager::GetTetheringCapabilityFromConnectionProfile(&profile).ok()
}

fn tethering_manager() -> Option<NetworkOperatorTetheringManager> {
    let profile = any_connection_profile()?;
    match NetworkOperatorTetheringManager::CreateFromConnectionProfile(&profile) {
        Ok(manager) => Some(manager),
        Err(e) => {
            teprintln!("[hosted-network] CreateFromConnectionProfile failed: {e}");
            None
        }
    }
}

/// Constructive Wi-Fi Direct probe: the type exists on every Windows 10 build, so build a
/// publisher and read its legacy settings, which is what actually fails on machines without one.
fn wifi_direct_available() -> bool {
    let probe = || -> Result<()> {
        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        publisher.Advertisement()?.LegacySettings()?;
        Ok(())
    };
    match probe() {
        Ok(()) => true,
        Err(e) => {
            tprintln!("[hosted-network] Wi-Fi Direct unavailable: {e}");
            false
        }
    }
}

/// RAII wrapper for a WLAN client handle, so every early return closes it.
struct WlanHandle(windows::Win32::Foundation::HANDLE);

impl WlanHandle {
    /// Open a WLAN client session, negotiating the highest version this OS offers.
    fn open() -> Option<Self> {
        use windows::Win32::NetworkManagement::WiFi::WlanOpenHandle;
        let mut negotiated = 0u32;
        let mut handle = windows::Win32::Foundation::HANDLE::default();
        // Version 2 is Vista+; every OS this project supports negotiates it or better.
        let rc = unsafe { WlanOpenHandle(2, None, &mut negotiated, &mut handle) };
        (rc == 0).then_some(Self(handle))
    }
}

impl Drop for WlanHandle {
    fn drop(&mut self) {
        use windows::Win32::NetworkManagement::WiFi::WlanCloseHandle;
        unsafe { WlanCloseHandle(self.0, None) };
    }
}

/// Ask the WLAN service, per interface, whether its driver can host a network.
///
/// `wlan_intf_opcode_hosted_network_capable` returns a `BOOL` and is the answer `netsh wlan show
/// drivers` prints as "Hosted network supported". Reading it directly is locale-proof; the
/// string was not, and on a German, French, Japanese or Chinese Windows the phrase never appears
/// at all, so the probe returned "no" unconditionally.
///
/// `None` means the question could not be asked (no WLAN service, no interfaces, an opcode this
/// build of Windows does not know) — distinct from a definite "no", which is what makes the
/// `netsh` fallback below worth keeping.
fn wlan_hosted_network_capable() -> Option<bool> {
    use windows::Win32::NetworkManagement::WiFi::{
        wlan_intf_opcode_hosted_network_capable, WlanEnumInterfaces, WlanFreeMemory,
        WlanQueryInterface, WLAN_INTERFACE_INFO_LIST, WLAN_OPCODE_VALUE_TYPE,
    };

    let handle = WlanHandle::open()?;
    let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
    if unsafe { WlanEnumInterfaces(handle.0, None, &mut list) } != 0 || list.is_null() {
        return None;
    }
    // Everything past here must reach the free, so the answer is computed and then returned.
    let mut answer: Option<bool> = None;
    unsafe {
        let count = (*list).dwNumberOfItems as usize;
        for i in 0..count {
            let info = (*list).InterfaceInfo.as_ptr().add(i);
            let mut size = 0u32;
            let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut value_type = WLAN_OPCODE_VALUE_TYPE::default();
            let rc = WlanQueryInterface(
                handle.0,
                &(*info).InterfaceGuid,
                wlan_intf_opcode_hosted_network_capable,
                None,
                &mut size,
                &mut data,
                Some(&mut value_type),
            );
            if rc != 0 || data.is_null() || size < 4 {
                continue;
            }
            let capable = *(data as *const i32) != 0;
            WlanFreeMemory(data);
            // Any interface that can host is enough; the tier only needs one.
            answer = Some(answer.unwrap_or(false) || capable);
            if capable {
                break;
            }
        }
        WlanFreeMemory(list as *const std::ffi::c_void);
    }
    answer
}

/// The netsh legacy tier's capability. A static property of the installed WLAN driver, so it is
/// computed at most once per process — it used to be re-derived by spawning `netsh` on every
/// start/stop/status call, and the adapter enumeration reached it once a second.
fn supports_legacy_hosted_network(app: &AppHandle) -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        if let Some(capable) = wlan_hosted_network_capable() {
            tprintln!(
                "[hosted-network] WlanQueryInterface(hosted_network_capable) => {capable} \
                 (locale-independent)"
            );
            return capable;
        }
        tprintln!(
            "[hosted-network] the WLAN service could not answer hosted_network_capable; \
             falling back to parsing `netsh wlan show drivers`"
        );
        let output = tauri::async_runtime::block_on(async {
            app.shell()
                .command("netsh")
                .args(["wlan", "show", "drivers"])
                .output()
                .await
        });
        let Ok(output) = output else {
            teprintln!("[hosted-network] could not run `netsh wlan show drivers`");
            return false;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        if !text.contains("Hosted network supported") {
            // Localized Windows never prints the English phrase. Treat that as unknown rather
            // than as "no": the tier below this one is "unsupported" either way, and the two
            // modern tiers are tried first.
            tprintln!(
                "[hosted-network] `netsh wlan show drivers` did not name hosted-network support \
                 (localized Windows?); assuming the legacy tier is unavailable"
            );
            return false;
        }
        let supported = text
            .split("Hosted network supported")
            .any(|s| s.trim().starts_with(": Yes"));
        tprintln!("[hosted-network] legacy netsh hosted network supported: {supported}");
        supported
    })
}

/// Whether the legacy hosted network is running right now, read from the WLAN service instead
/// of from a localized `netsh` status line.
///
/// `WLAN_HOSTED_NETWORK_STATE` distinguishes `active` from `idle` and `unavailable` with no
/// string involved, which is exactly the readback the old code got wrong: on localized Windows a
/// *successfully started* hosted network was reported as not running.
fn wlan_hosted_network_active() -> Option<bool> {
    use windows::Win32::NetworkManagement::WiFi::{
        wlan_hosted_network_active, WlanFreeMemory, WlanHostedNetworkQueryStatus,
        WLAN_HOSTED_NETWORK_STATUS,
    };

    let handle = WlanHandle::open()?;
    let mut status: *mut WLAN_HOSTED_NETWORK_STATUS = std::ptr::null_mut();
    let rc = unsafe { WlanHostedNetworkQueryStatus(handle.0, &mut status, None) };
    if rc != 0 || status.is_null() {
        return None;
    }
    let active = unsafe { (*status).HostedNetworkState } == wlan_hosted_network_active;
    unsafe { WlanFreeMemory(status as *const std::ffi::c_void) };
    Some(active)
}

/// The best backend this machine can actually run, newest first.
pub fn probe_backend(app: Option<&AppHandle>) -> HostedNetworkBackend {
    if tethering_capability() == Some(TetheringCapability::Enabled) {
        return HostedNetworkBackend::MobileHotspot;
    }
    if wifi_direct_available() {
        return HostedNetworkBackend::WiFiDirect;
    }
    if app.is_some_and(supports_legacy_hosted_network) {
        return HostedNetworkBackend::NetshLegacy;
    }
    HostedNetworkBackend::Unsupported
}

// -- radios -----------------------------------------------------------------------------------

fn wifi_radios() -> Vec<Radio> {
    let access = match Radio::RequestAccessAsync().and_then(|op| op.join()) {
        Ok(status) => status,
        Err(_) => return Vec::new(),
    };
    if access != RadioAccessStatus::Allowed {
        return Vec::new();
    }
    let radios = match Radio::GetRadiosAsync().and_then(|op| op.join()) {
        Ok(radios) => radios,
        Err(_) => return Vec::new(),
    };
    radios
        .into_iter()
        .filter(|r| r.Kind().map(|k| k == RadioKind::WiFi).unwrap_or(false))
        .collect()
}

#[tauri::command]
#[specta::specta]
pub fn is_wifi_on() -> bool {
    wifi_radios()
        .iter()
        .any(|r| r.State().map(|s| s == RadioState::On).unwrap_or(false))
}

#[tauri::command]
#[specta::specta]
pub fn turn_on_wifi() -> bool {
    let radios = wifi_radios();
    if radios.is_empty() {
        return false;
    }
    let mut any_on = false;
    for radio in radios {
        if radio.State().map(|s| s == RadioState::On).unwrap_or(false) {
            any_on = true;
            continue;
        }
        if let Ok(RadioAccessStatus::Allowed) =
            radio.SetStateAsync(RadioState::On).and_then(|op| op.join())
        {
            any_on = true
        }
    }
    any_on
}

// -- Mobile Hotspot ---------------------------------------------------------------------------

fn band_name(band: TetheringWiFiBand) -> &'static str {
    match band {
        TetheringWiFiBand::SixGigahertz => "6 GHz",
        TetheringWiFiBand::FiveGigahertz => "5 GHz",
        TetheringWiFiBand::TwoPointFourGigahertz => "2.4 GHz",
        _ => "auto",
    }
}

/// 6 GHz has no AWDL traffic and no legacy contention; 5 GHz gives VHT80. 2.4 GHz is what a
/// hotspot lands on by default and is the worst band for a 1080p60 stream.
fn preferred_band(config: &NetworkOperatorTetheringAccessPointConfiguration) -> TetheringWiFiBand {
    for band in [
        TetheringWiFiBand::SixGigahertz,
        TetheringWiFiBand::FiveGigahertz,
    ] {
        if config.IsBandSupported(band).unwrap_or(false) {
            return band;
        }
    }
    TetheringWiFiBand::Auto
}

fn operation_status_name(status: TetheringOperationStatus) -> &'static str {
    match status {
        TetheringOperationStatus::Success => "Success",
        TetheringOperationStatus::MobileBroadbandDeviceOff => "MobileBroadbandDeviceOff",
        TetheringOperationStatus::WiFiDeviceOff => "WiFiDeviceOff",
        TetheringOperationStatus::EntitlementCheckTimeout => "EntitlementCheckTimeout",
        TetheringOperationStatus::EntitlementCheckFailure => "EntitlementCheckFailure",
        TetheringOperationStatus::OperationInProgress => "OperationInProgress",
        TetheringOperationStatus::BluetoothDeviceOff => "BluetoothDeviceOff",
        TetheringOperationStatus::NetworkLimitedConnectivity => "NetworkLimitedConnectivity",
        TetheringOperationStatus::AlreadyOn => "AlreadyOn",
        TetheringOperationStatus::RadioRestriction => "RadioRestriction",
        TetheringOperationStatus::BandInterference => "BandInterference",
        _ => "Unknown",
    }
}

/// Start the Mobile Hotspot. Returns the band it was configured for.
fn start_mobile_hotspot(name: &str, password: &str) -> Result<&'static str> {
    let manager = tethering_manager().ok_or_else(|| {
        windows::core::Error::new(windows::core::HRESULT(-1), "no tethering manager")
    })?;

    if manager.TetheringOperationalState()? == TetheringOperationalState::On {
        // Reconfiguring a running hotspot is not allowed; take it down first, as Microsoft
        // recommends, so our SSID and band actually apply.
        let _ = manager.StopTetheringAsync().and_then(|op| op.join());
    }

    let config = manager.GetCurrentAccessPointConfiguration()?;
    config.SetSsid(&HSTRING::from(name))?;
    config.SetPassphrase(&HSTRING::from(password))?;
    let band = preferred_band(&config);
    if band != TetheringWiFiBand::Auto {
        if let Err(e) = config.SetBand(band) {
            tprintln!("[hosted-network] could not pin the hotspot band: {e}");
        }
    }
    manager
        .ConfigureAccessPointAsync(&config)
        .and_then(|op| op.join())?;

    let mut last: Option<TetheringOperationStatus> = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
        }
        let result = manager.StartTetheringAsync().and_then(|op| op.join())?;
        let op_status = result.Status()?;
        if op_status == TetheringOperationStatus::Success
            || op_status == TetheringOperationStatus::AlreadyOn
        {
            *active().lock().unwrap() = Some(Active::MobileHotspot);
            return Ok(band_name(band));
        }
        let detail = result.AdditionalErrorMessage().unwrap_or_default();
        tprintln!(
            "[hosted-network] StartTetheringAsync attempt {} returned {} {}",
            attempt + 1,
            operation_status_name(op_status),
            detail
        );
        last = Some(op_status);
        // These will not improve with another try.
        if matches!(
            op_status,
            TetheringOperationStatus::WiFiDeviceOff | TetheringOperationStatus::RadioRestriction
        ) {
            break;
        }
    }

    Err(windows::core::Error::new(
        windows::core::HRESULT(-1),
        format!(
            "mobile hotspot did not start ({})",
            last.map(operation_status_name).unwrap_or("no status")
        ),
    ))
}

// -- Wi-Fi Direct -----------------------------------------------------------------------------

fn start_wifi_direct_(
    name: &str,
    password: &str,
    success_tx: Sender<bool>,
) -> Result<WiFiDirectAdvertisementPublisher> {
    // Mobile Hotspot and Wi-Fi Direct cannot run at once, and Mobile Hotspot wins. Refuse if it
    // is already up — but never let the absence of a tethering manager (no internet, no profile)
    // block this path, which needs no connection profile of its own.
    if let Some(manager) = tethering_manager() {
        if manager.TetheringOperationalState()? != TetheringOperationalState::Off {
            let _ = success_tx.send(false);
            return Err(windows::core::Error::new(
                windows::core::HRESULT(-1),
                "the Mobile Hotspot is on; it takes precedence over Wi-Fi Direct",
            ));
        }
    }

    let publisher = WiFiDirectAdvertisementPublisher::new()?;

    let ssid = HSTRING::from(name);
    let password_credential = PasswordCredential::new()?;
    password_credential.SetPassword(&HSTRING::from(password))?;

    let publisher_status_changed_callback = TypedEventHandler::<
        WiFiDirectAdvertisementPublisher,
        WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
    >::new(move |_sender, args| {
        if let Some(args) = args.as_ref() {
            match args.Status()? {
                WiFiDirectAdvertisementPublisherStatus::Started => {
                    let _ = success_tx.send(true);
                }
                WiFiDirectAdvertisementPublisherStatus::Aborted
                | WiFiDirectAdvertisementPublisherStatus::Stopped => {
                    let _ = success_tx.send(false);
                }
                other => {
                    tprintln!("[hosted-network] Wi-Fi Direct publisher status: {other:?}");
                }
            }
        }
        Ok(())
    });
    publisher.StatusChanged(&publisher_status_changed_callback)?;

    let advertisement = publisher.Advertisement()?;
    advertisement.SetIsAutonomousGroupOwnerEnabled(true)?;

    let legacy_settings = advertisement.LegacySettings()?;
    legacy_settings.SetIsEnabled(true)?;
    legacy_settings.SetSsid(&ssid)?;
    legacy_settings.SetPassphrase(&password_credential)?;

    publisher.Start()?;

    Ok(publisher)
}

fn stop_publisher(publisher: &WiFiDirectAdvertisementPublisher) {
    if let Ok(status) = publisher.Status() {
        if status == WiFiDirectAdvertisementPublisherStatus::Started {
            let _ = publisher.Stop();
        }
    }
}

// -- verification -----------------------------------------------------------------------------

/// Wait for the AP's virtual adapter to hold a routable IPv4. Without one there is no DHCP and
/// no client will ever reach us, however cheerful the radio's own status is.
fn wait_for_ap_address() -> Option<String> {
    let deadline = std::time::Instant::now() + ADDRESS_TIMEOUT;
    loop {
        if let Some(ip) = super::networking::hosted_network_ipv4().into_iter().next() {
            return Some(ip);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(ADDRESS_POLL);
    }
}

fn validate_passphrase(password: &str) -> std::result::Result<(), String> {
    let len = password.chars().count();
    if !(MIN_PASSPHRASE..=MAX_PASSPHRASE).contains(&len) {
        return Err(format!(
            "the hotspot password must be {MIN_PASSPHRASE}-{MAX_PASSPHRASE} characters (got {len})"
        ));
    }
    if !password.is_ascii() {
        return Err("the hotspot password must be ASCII".to_string());
    }
    Ok(())
}

// -- commands ---------------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub fn start_hosted_network(
    app: AppHandle,
    state: State<'_, AppState>,
    name: &str,
    password: &str,
) -> bool {
    if let Err(e) = validate_passphrase(password) {
        teprintln!("[hosted-network] {e}");
        record(state, None, name, None, false, None);
        return false;
    }

    let mut started: Option<(HostedNetworkBackend, Option<&'static str>)> = None;

    if tethering_capability() == Some(TetheringCapability::Enabled) {
        match start_mobile_hotspot(name, password) {
            Ok(band) => {
                tprintln!("[hosted-network] Mobile Hotspot started on {band}");
                started = Some((HostedNetworkBackend::MobileHotspot, Some(band)));
            }
            Err(e) => {
                teprintln!("[hosted-network] Mobile Hotspot failed ({e}); trying Wi-Fi Direct")
            }
        }
    } else if let Some(capability) = tethering_capability() {
        tprintln!(
            "[hosted-network] Mobile Hotspot unavailable: {}",
            capability_name(capability)
        );
    }

    if started.is_none() {
        let (success_tx, success_rx) = channel::<bool>();
        match start_wifi_direct_(name, password, success_tx) {
            Ok(publisher) => match success_rx.recv_timeout(PUBLISHER_TIMEOUT) {
                Ok(true) => {
                    tprintln!("[hosted-network] Wi-Fi Direct legacy AP started (no band control)");
                    *active().lock().unwrap() = Some(Active::WiFiDirect(publisher));
                    started = Some((HostedNetworkBackend::WiFiDirect, None));
                }
                Ok(false) => {
                    teprintln!("[hosted-network] the Wi-Fi Direct publisher aborted");
                    stop_publisher(&publisher);
                }
                Err(_) => {
                    teprintln!(
                        "[hosted-network] the Wi-Fi Direct publisher never reported a status \
                         within {}s",
                        PUBLISHER_TIMEOUT.as_secs()
                    );
                    stop_publisher(&publisher);
                }
            },
            Err(e) => teprintln!("[hosted-network] Wi-Fi Direct failed: {e}"),
        }
    }

    if started.is_none()
        && supports_legacy_hosted_network(&app)
        && start_legacy(&app, name, password)
    {
        tprintln!("[hosted-network] legacy netsh hosted network started");
        *active().lock().unwrap() = Some(Active::NetshLegacy);
        started = Some((HostedNetworkBackend::NetshLegacy, None));
    }

    let Some((backend, band)) = started else {
        teprintln!("[hosted-network] no hosted-network backend on this machine could start an AP");
        record(state, None, name, None, false, None);
        return false;
    };

    let host_ipv4 = wait_for_ap_address();
    if host_ipv4.is_none() {
        teprintln!(
            "[hosted-network] '{name}' is advertised on {} but its adapter never got an IPv4 \
             address; clients will be able to see it and not join (no DHCP)",
            backend.as_str()
        );
    } else {
        tprintln!(
            "[hosted-network] '{name}' verified on {} at {}",
            backend.as_str(),
            host_ipv4.as_deref().unwrap_or("-")
        );
    }

    register_stopper(&state);
    record(state, Some(backend), name, band, true, host_ipv4);
    true
}

fn start_legacy(app: &AppHandle, name: &str, password: &str) -> bool {
    let exe_path = match std::env::current_exe() {
        Ok(exe_path) => match exe_path.into_os_string().into_string() {
            Ok(path) => path,
            Err(_) => {
                teprintln!("[hosted-network] the executable path is not valid UTF-8");
                return false;
            }
        },
        Err(e) => {
            teprintln!("[hosted-network] could not resolve the executable path: {e}");
            return false;
        }
    };
    let mut cmd = StdCommand::new(exe_path);
    cmd.arg("hostednetwork");
    cmd.arg(name);
    cmd.arg(password);
    if let Err(e) = Command::new(cmd).output() {
        teprintln!("[hosted-network] elevated `hostednetwork` failed: {e}");
        return false;
    }
    legacy_started(app)
}

fn legacy_started(app: &AppHandle) -> bool {
    if let Some(active) = wlan_hosted_network_active() {
        return active;
    }
    // Only if the WLAN service declined to answer at all. This branch is English-only and known
    // to be: on a localized Windows a running hosted network reads as stopped here.
    let output = tauri::async_runtime::block_on(async {
        app.shell()
            .command("netsh")
            .args(["wlan", "show", "hostednetwork"])
            .output()
            .await
    });
    output.is_ok_and(|output| {
        let text = String::from_utf8_lossy(&output.stdout);
        text.contains("Status")
            && text
                .split("Status")
                .any(|s| s.trim().starts_with(": Started"))
    })
}

fn register_stopper(state: &State<'_, AppState>) {
    let mut stop_func = state.stop_hosted_network.lock().unwrap();
    *stop_func = Some(Box::new(move || {
        if let Some(active) = active().lock().unwrap().take() {
            match active {
                Active::MobileHotspot => {
                    if let Some(manager) = tethering_manager() {
                        let _ = manager.StopTetheringAsync().and_then(|op| op.join());
                    }
                }
                Active::WiFiDirect(publisher) => stop_publisher(&publisher),
                Active::NetshLegacy => {}
            }
        }
    }));
}

fn record(
    state: State<'_, AppState>,
    backend: Option<HostedNetworkBackend>,
    ssid: &str,
    band: Option<&'static str>,
    advertised: bool,
    host_ipv4: Option<String>,
) {
    *status().lock().unwrap() = HostedNetworkStatus {
        backend,
        ssid: ssid.to_string(),
        band,
        advertised,
        host_ipv4,
    };
    *state.hosted_network_running.lock().unwrap() = advertised;
}

#[tauri::command]
#[specta::specta]
pub fn stop_hosted_network(app: AppHandle, state: State<'_, AppState>) -> bool {
    let was_legacy = matches!(*active().lock().unwrap(), Some(Active::NetshLegacy));

    let stopped = if was_legacy {
        let status = tauri::async_runtime::block_on(async {
            app.shell()
                .command("netsh")
                .args(["wlan", "stop", "hostednetwork"])
                .status()
                .await
        });
        match status {
            Ok(status) => status.success(),
            Err(e) => {
                teprintln!("[hosted-network] `netsh wlan stop hostednetwork` failed: {e}");
                false
            }
        }
    } else {
        let stopper = state.stop_hosted_network.lock().unwrap().is_some();
        if stopper {
            if let Some(ref stop_func) = *state.stop_hosted_network.lock().unwrap() {
                stop_func();
            }
            true
        } else {
            false
        }
    };

    if stopped {
        *active().lock().unwrap() = None;
        *status().lock().unwrap() = HostedNetworkStatus::default();
    }
    *state.hosted_network_running.lock().unwrap() = !stopped;
    stopped
}

/// Whether the hosted network is up. Reads the platform object we already hold — it must not
/// spawn a process, because the UI and the adapter list ask often.
#[tauri::command]
#[specta::specta]
pub fn is_hosted_network(_app: AppHandle, state: State<'_, AppState>) -> bool {
    let live = match &*active().lock().unwrap() {
        Some(Active::MobileHotspot) => Some(tethering_manager().is_some_and(|manager| {
            manager
                .TetheringOperationalState()
                .map(|s| s == TetheringOperationalState::On)
                .unwrap_or(false)
        })),
        Some(Active::WiFiDirect(publisher)) => Some(
            publisher
                .Status()
                .map(|s| s == WiFiDirectAdvertisementPublisherStatus::Started)
                .unwrap_or(false),
        ),
        // The legacy tier's only readback is `netsh`, and this is a hot path.
        Some(Active::NetshLegacy) => None,
        None => Some(false),
    };

    match live {
        Some(running) => {
            *state.hosted_network_running.lock().unwrap() = running;
            status().lock().unwrap().advertised = running;
            running
        }
        None => *state.hosted_network_running.lock().unwrap(),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn passphrase_bounds_follow_802_11() {
        assert!(validate_passphrase("1234567").is_err());
        assert!(validate_passphrase("12345678").is_ok());
        assert!(validate_passphrase(&"a".repeat(63)).is_ok());
        assert!(validate_passphrase(&"a".repeat(64)).is_err());
        assert!(validate_passphrase("pässwörd").is_err());
    }

    #[test]
    fn backend_names_are_stable() {
        assert_eq!(
            HostedNetworkBackend::MobileHotspot.as_str(),
            "MobileHotspot"
        );
        assert_eq!(HostedNetworkBackend::Unsupported.as_str(), "Unsupported");
    }

    /// Records what this machine actually answers. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "read-only report of this machine's tethering capability; run with --ignored --nocapture"]
    fn report_tethering_capability() {
        match tethering_capability() {
            Some(capability) => println!(
                "GetTetheringCapabilityFromConnectionProfile => {} ({})",
                capability_name(capability),
                capability.0
            ),
            None => println!("no connection profile to probe"),
        }
    }

    /// The locale-independent legacy probe, next to the localized string it replaces. Run with
    /// `--ignored --nocapture`; needs no elevation.
    #[test]
    #[ignore = "read-only report of the legacy WLAN hosted-network capability; run with --ignored --nocapture"]
    fn report_wlan_hosted_network_capability() {
        println!(
            "WlanQueryInterface(hosted_network_capable) => {:?}",
            wlan_hosted_network_capable()
        );
        println!(
            "WlanHostedNetworkQueryStatus(active) => {:?}",
            wlan_hosted_network_active()
        );
    }
}

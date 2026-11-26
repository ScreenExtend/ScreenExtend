//! The host-created Wi-Fi network, as a probed ladder of backends, newest first: Mobile Hotspot
//! (`NetworkOperatorTetheringManager`, the only tier with band control), Wi-Fi Direct legacy AP
//! (`WiFiDirectAdvertisementPublisher`), netsh legacy (deprecated since Windows 10 1607). Each
//! tier is selected by attempting the capability, never by a version number.
//!
//! "Started" means the radio is up and the virtual adapter has a routable IPv4: an AP that never
//! serves DHCP is a failure. The Mobile Hotspot configuration is the user's, so a start
//! snapshots it to `hotspot-restore.json`, and a stop (or the next launch after a crash) puts it
//! back. Commands are `async` and work on the blocking pool: a sync Tauri command runs on the
//! UI's main thread, and a failing start can take half a minute.

use crate::windows_utils::AppState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Manager};
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

/// How long the legacy tier waits for its elevated `hostednetwork` child. The UAC prompt is
/// answered inside `ShellExecuteExW`, before this starts; what it covers is a whole Tauri process
/// starting up and running `netsh wlan set` and `netsh wlan start`.
const LEGACY_ELEVATED_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a child that outlived `LEGACY_ELEVATED_TIMEOUT` is still watched for. An AP it
/// brings up in that time is recorded as ours, so it shows on the network card and a stop
/// reaches it.
const LEGACY_LATE_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a stopped hotspot may take to read as off before the stop is called failed.
const HOTSPOT_STOP_SETTLE: Duration = Duration::from_millis(1000);

/// 802.11 requires a WPA2 passphrase of 8..=63 ASCII characters.
const MIN_PASSPHRASE: usize = 8;
const MAX_PASSPHRASE: usize = 63;

/// The Mobile Hotspot snapshot, in the app's local data directory. It holds the user's hotspot
/// passphrase, which any process running as this user can already read from
/// `GetCurrentAccessPointConfiguration`, so a per-user directory is protection enough.
const HOTSPOT_RESTORE_FILE: &str = "hotspot-restore.json";

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

    /// The authentication each tier negotiates. None of them can be asked to drop the
    /// passphrase, so Windows never brings up an open network. Only the legacy tier can be read
    /// back (`netsh wlan show hostednetwork` prints `Authentication : WPA2-Personal`); the WinRT
    /// tiers expose no property, so theirs is the documented and observed value.
    pub fn security(self) -> Option<&'static str> {
        match self {
            Self::MobileHotspot => Some("WPA2-Personal"),
            Self::WiFiDirect => Some("WPA2-Personal (Wi-Fi Direct legacy AP)"),
            Self::NetshLegacy => Some("WPA2-Personal (netsh hosted network)"),
            Self::Unsupported => None,
        }
    }
}

/// What is known about the hosted network right now. `advertised` means the platform accepted
/// the start; `host_ipv4` being set is the evidence that a client could actually associate.
#[derive(Clone, Debug, Default)]
pub struct HostedNetworkStatus {
    pub backend: Option<HostedNetworkBackend>,
    pub ssid: String,
    pub band: Option<&'static str>,
    pub advertised: bool,
    pub host_ipv4: Option<String>,
}

/// The user's Mobile Hotspot configuration as it was before this app first changed it, plus the
/// SSID the app put in its place. `ours` is how a later reader tells "still the app's settings"
/// from "the user has changed them since", and only the former is ever overwritten.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HotspotSnapshot {
    ssid: String,
    passphrase: String,
    /// `TetheringWiFiBand`, when this Windows exposes it.
    band: Option<i32>,
    /// `TetheringWiFiAuthenticationKind`, when this Windows exposes it.
    authentication: Option<i32>,
    /// The user's own hotspot was running when the app took the radio over.
    was_on: bool,
    ours: String,
}

/// A started Mobile Hotspot and how to give the user's configuration back.
struct HotspotRestore {
    snapshot: HotspotSnapshot,
    breadcrumb: Option<PathBuf>,
}

/// `NetworkOperatorTetheringManager` is not agile, so it cannot be parked in a `static` or in
/// `AppState`; it is cheap to rebuild from a connection profile, and that is what stop and
/// status do. The Wi-Fi Direct publisher *is* agile and must be kept, since dropping it is the
/// only way the advertisement ends.
enum Active {
    MobileHotspot(HotspotRestore),
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

/// Serialises start, stop and crash recovery: each reads and then rewrites the one system-wide
/// hotspot configuration, and a stop racing a start would restore half of one. Held only on
/// blocking-pool threads, never across an `.await`.
fn hotspot_op() -> MutexGuard<'static, ()> {
    static OP: Mutex<()> = Mutex::new(());
    OP.lock().unwrap_or_else(|e| e.into_inner())
}

/// Set once a start in this process has taken over (or consumed) the snapshot on disk, after
/// which a crash recovery still queued from startup must not act on it.
static HOTSPOT_TAKEN_OVER: AtomicBool = AtomicBool::new(false);

pub fn hosted_network_status() -> HostedNetworkStatus {
    status().lock().unwrap().clone()
}

/// Run a command body on the blocking pool. `fallback` is what the UI gets if it panicked.
async fn on_blocking_pool<T: Send + 'static>(
    what: &'static str,
    fallback: T,
    body: impl FnOnce() -> T + Send + 'static,
) -> T {
    match tauri::async_runtime::spawn_blocking(body).await {
        Ok(value) => value,
        Err(e) => {
            teprintln!("[hosted-network] {what} failed on its worker thread: {e}");
            fallback
        }
    }
}

// -- tier probes ------------------------------------------------------------------------------

/// Any connection profile reaches the tethering manager: `CreateFromConnectionProfile` succeeds
/// on an Ethernet or a saved-but-not-connected Wi-Fi profile, and a hosted network may have no
/// internet at all.
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
/// that the caller lacks the `wiFiControl` manifest capability; this unpackaged binary reads
/// `Enabled` on Windows 11 26200, so unpackaged desktop apps are not blocked by it.
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
    fn open() -> Option<Self> {
        use windows::Win32::NetworkManagement::WiFi::WlanOpenHandle;
        let mut negotiated = 0u32;
        let mut handle = windows::Win32::Foundation::HANDLE::default();
        // client version 2 is Vista and later
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

/// Ask the WLAN service, per interface, whether its driver can host a network: the `BOOL` that
/// `netsh wlan show drivers` prints as "Hosted network supported", read without the localized
/// string. `None` means the question could not be asked (no WLAN service, no interfaces, an
/// opcode this Windows does not know), not a definite "no": the `netsh` fallback is for that.
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
    // no early return past here: everything must reach the free
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
            // on success the service allocated `data`: free it whatever its size
            if rc != 0 || data.is_null() {
                continue;
            }
            let value = (size >= 4).then(|| *(data as *const i32));
            WlanFreeMemory(data);
            let Some(value) = value else {
                continue;
            };
            let capable = value != 0;
            // one interface that can host is enough
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
/// computed at most once per process.
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
            // localized Windows never prints the English phrase: unknown, not a "no", but the
            // tier below this one is "unsupported" either way
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

/// Whether the legacy hosted network is running right now, read from the WLAN service
/// (`WLAN_HOSTED_NETWORK_STATE`), not from a `netsh` status line, which is localized.
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
pub async fn is_wifi_on() -> bool {
    on_blocking_pool("is_wifi_on", false, || {
        wifi_radios()
            .iter()
            .any(|r| r.State().map(|s| s == RadioState::On).unwrap_or(false))
    })
    .await
}

/// Off the main thread: a sync command runs on the webview's thread, and a radio that is slow
/// to answer froze the window for as long as it took.
#[tauri::command]
#[specta::specta]
pub async fn turn_on_wifi() -> bool {
    on_blocking_pool("turn_on_wifi", false, turn_on_wifi_blocking).await
}

/// The body of [`turn_on_wifi`], for `ScreenExtend network wifi-on`, which has no runtime to
/// await on.
pub fn turn_on_wifi_blocking() -> bool {
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

/// The bands worth pinning, in the order to try them. 2.4 GHz, where a hotspot lands by default,
/// is the worst band for a 1080p60 stream. 6 GHz is the cleanest (no AWDL, no legacy contention),
/// but most phones and tablets cannot see it and Windows moves a 6 GHz hotspot to WPA3, which
/// older clients lack: so 5 GHz (VHT80) goes first unless `hotspot_prefer_5ghz` is switched off.
fn band_preference(prefer_5ghz: bool) -> [TetheringWiFiBand; 2] {
    if prefer_5ghz {
        [
            TetheringWiFiBand::FiveGigahertz,
            TetheringWiFiBand::SixGigahertz,
        ]
    } else {
        [
            TetheringWiFiBand::SixGigahertz,
            TetheringWiFiBand::FiveGigahertz,
        ]
    }
}

fn preferred_band(
    config: &NetworkOperatorTetheringAccessPointConfiguration,
    prefer_5ghz: bool,
) -> TetheringWiFiBand {
    for band in band_preference(prefer_5ghz) {
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

fn started_ok(status: TetheringOperationStatus) -> bool {
    status == TetheringOperationStatus::Success || status == TetheringOperationStatus::AlreadyOn
}

/// How the Mobile Hotspot tier picks and gives up on a band. Asked once per start.
#[derive(Clone, Copy, Debug)]
struct BandPolicy {
    /// `hotspot_prefer_5ghz` (on): 5 GHz before 6 GHz. Off: 6 GHz first.
    prefer_5ghz: bool,
    /// `hotspot_band_auto_fallback` (on): `BandInterference` is not retried on the same band,
    /// and a pinned band that fails gets one more start on auto. Off: three tries on the pinned
    /// band, `BandInterference` included, and then the next tier.
    auto_fallback: bool,
}

impl BandPolicy {
    fn from_switches() -> Self {
        use crate::windows_utils::streamer::tune;
        Self {
            prefer_5ghz: tune::on("hotspot_prefer_5ghz"),
            auto_fallback: tune::on("hotspot_band_auto_fallback"),
        }
    }
}

/// Whether another start on the *same* configuration can do better. A radio that is off or
/// restricted stays so. With the auto fallback on, `BandInterference` is not retried either:
/// the station's association on another band does not move in the 1.5 s the retries take.
fn worth_retrying(status: TetheringOperationStatus, auto_fallback: bool) -> bool {
    !started_ok(status)
        && !matches!(
            status,
            TetheringOperationStatus::WiFiDeviceOff | TetheringOperationStatus::RadioRestriction
        )
        && !(auto_fallback && status == TetheringOperationStatus::BandInterference)
}

/// After a pinned band failed, one more start with the band on auto, unless the radio itself is
/// the problem. Auto is what the Wi-Fi Direct tier below would get anyway, with no band control
/// at all, so this never lands on anything worse than leaving the tier did.
fn fall_back_to_auto(
    enabled: bool,
    band: TetheringWiFiBand,
    last: TetheringOperationStatus,
) -> bool {
    enabled
        && band != TetheringWiFiBand::Auto
        && !started_ok(last)
        && !matches!(
            last,
            TetheringOperationStatus::WiFiDeviceOff | TetheringOperationStatus::RadioRestriction
        )
}

fn start_once(
    manager: &NetworkOperatorTetheringManager,
    label: &str,
) -> Result<TetheringOperationStatus> {
    let result = manager.StartTetheringAsync().and_then(|op| op.join())?;
    let op_status = result.Status()?;
    if !started_ok(op_status) {
        let detail = result.AdditionalErrorMessage().unwrap_or_default();
        tprintln!(
            "[hosted-network] StartTetheringAsync {label} returned {} {detail}",
            operation_status_name(op_status)
        );
    }
    Ok(op_status)
}

/// Up to three starts on the configuration as it stands; returns the last status.
fn start_attempts(
    manager: &NetworkOperatorTetheringManager,
    auto_fallback: bool,
) -> Result<TetheringOperationStatus> {
    let mut last = TetheringOperationStatus::Success;
    for attempt in 0..3u32 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
        }
        last = start_once(manager, &format!("attempt {}", attempt + 1))?;
        if !worth_retrying(last, auto_fallback) {
            break;
        }
    }
    Ok(last)
}

/// Reconfigure the hotspot to ours and start it. Returns the band it came up on.
fn configure_and_start(
    manager: &NetworkOperatorTetheringManager,
    name: &str,
    password: &str,
    policy: BandPolicy,
) -> Result<&'static str> {
    if manager.TetheringOperationalState()? == TetheringOperationalState::On {
        // a running hotspot cannot be reconfigured, so stop it first (as Microsoft recommends);
        // the snapshot already says it was on
        let _ = manager.StopTetheringAsync().and_then(|op| op.join());
    }

    let config = manager.GetCurrentAccessPointConfiguration()?;
    config.SetSsid(&HSTRING::from(name))?;
    config.SetPassphrase(&HSTRING::from(password))?;
    let mut band = preferred_band(&config, policy.prefer_5ghz);
    if band != TetheringWiFiBand::Auto {
        if let Err(e) = config.SetBand(band) {
            tprintln!("[hosted-network] could not pin the hotspot band: {e}");
            band = config.Band().unwrap_or(TetheringWiFiBand::Auto);
        }
    }
    manager
        .ConfigureAccessPointAsync(&config)
        .and_then(|op| op.join())?;

    let mut last = start_attempts(manager, policy.auto_fallback)?;
    if started_ok(last) {
        return Ok(band_name(band));
    }
    if fall_back_to_auto(policy.auto_fallback, band, last) {
        tprintln!(
            "[hosted-network] the hotspot would not start on {} ({}); retrying once with the \
             band on auto",
            band_name(band),
            operation_status_name(last)
        );
        config.SetBand(TetheringWiFiBand::Auto)?;
        manager
            .ConfigureAccessPointAsync(&config)
            .and_then(|op| op.join())?;
        last = start_once(manager, "on auto band")?;
        if started_ok(last) {
            return Ok(band_name(TetheringWiFiBand::Auto));
        }
    }

    Err(windows::core::Error::new(
        windows::core::HRESULT(-1),
        format!(
            "mobile hotspot did not start ({})",
            operation_status_name(last)
        ),
    ))
}

/// The hotspot configuration as Windows holds it now. `ours` is left empty for the caller.
fn read_hotspot(manager: &NetworkOperatorTetheringManager) -> Result<HotspotSnapshot> {
    let config = manager.GetCurrentAccessPointConfiguration()?;
    Ok(HotspotSnapshot {
        ssid: config.Ssid()?.to_string_lossy(),
        passphrase: config.Passphrase()?.to_string_lossy(),
        band: config.Band().ok().map(|b| b.0),
        authentication: config.AuthenticationKind().ok().map(|a| a.0),
        was_on: manager.TetheringOperationalState()? == TetheringOperationalState::On,
        ours: String::new(),
    })
}

/// The snapshot a start should keep. An earlier one (from a start this process never stopped,
/// or a crashed run's breadcrumb) is kept while the hotspot still carries the SSID that run set:
/// a fresh one would record the app's own settings as the user's and lose theirs for good.
/// Anything else is stale.
fn snapshot_for_start(
    earlier: Option<HotspotSnapshot>,
    current: HotspotSnapshot,
    ours: &str,
) -> HotspotSnapshot {
    match earlier {
        Some(mut kept) if kept.ours == current.ssid => {
            kept.ours = ours.to_string();
            kept
        }
        _ => HotspotSnapshot {
            ours: ours.to_string(),
            ..current
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RestorePlan {
    stop: bool,
    reconfigure: bool,
    restart: bool,
}

/// What giving the user's hotspot back takes, given what the radio holds now.
///
/// Only a configuration that still carries our SSID is rewritten: a user who renamed the
/// hotspot in Settings since (or after a crash) has made their choice. A running hotspot is
/// stopped when it is ours (by SSID, or because this process started it and is stopping it),
/// and the user's own hotspot is brought back up only when it was on before and their
/// configuration is back in place.
fn restore_plan(
    snapshot: &HotspotSnapshot,
    current_ssid: &str,
    current_on: bool,
    started_here: bool,
) -> RestorePlan {
    let still_ours = current_ssid == snapshot.ours;
    RestorePlan {
        stop: current_on && (still_ours || started_here),
        reconfigure: still_ours,
        restart: still_ours && snapshot.was_on,
    }
}

/// Put the user's hotspot configuration (and running state) back. True when nothing is left to
/// do, so the breadcrumb can go; false when the tethering manager could not be reached, a
/// stop it needed did not take, or the configuration could not be written.
fn restore_hotspot(snapshot: &HotspotSnapshot, started_here: bool) -> bool {
    let Some(manager) = tethering_manager() else {
        teprintln!(
            "[hosted-network] cannot reach the tethering manager to restore the Mobile Hotspot \
             settings; keeping the snapshot for the next start"
        );
        return false;
    };
    let restore = || -> Result<RestorePlan> {
        let config = manager.GetCurrentAccessPointConfiguration()?;
        let current_ssid = config.Ssid()?.to_string_lossy();
        let current_on = manager.TetheringOperationalState()? == TetheringOperationalState::On;
        let plan = restore_plan(snapshot, &current_ssid, current_on, started_here);
        if plan.stop {
            let _ = manager.StopTetheringAsync().and_then(|op| op.join());
            let still_on = || {
                manager
                    .TetheringOperationalState()
                    .map(|s| s == TetheringOperationalState::On)
                    .unwrap_or(true)
            };
            let deadline = std::time::Instant::now() + HOTSPOT_STOP_SETTLE;
            let mut on = still_on();
            while on && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
                on = still_on();
            }
            if on {
                // our SSID may still be on the air: the caller keeps the snapshot and the
                // record, so a stop can be retried and the UI does not show it off
                return Err(windows::core::Error::new(
                    windows::core::HRESULT(-1),
                    "the hotspot is still on after StopTetheringAsync",
                ));
            }
        }
        if plan.reconfigure {
            config.SetSsid(&HSTRING::from(snapshot.ssid.as_str()))?;
            config.SetPassphrase(&HSTRING::from(snapshot.passphrase.as_str()))?;
            if let Some(band) = snapshot.band {
                if let Err(e) = config.SetBand(TetheringWiFiBand(band)) {
                    tprintln!("[hosted-network] could not restore the hotspot band: {e}");
                }
            }
            if let Some(kind) = snapshot.authentication {
                if let Err(e) = config.SetAuthenticationKind(TetheringWiFiAuthenticationKind(kind))
                {
                    tprintln!("[hosted-network] could not restore the hotspot authentication: {e}");
                }
            }
            manager
                .ConfigureAccessPointAsync(&config)
                .and_then(|op| op.join())?;
        }
        if plan.restart {
            let status = start_once(&manager, "restoring the user's hotspot")?;
            if !started_ok(status) {
                teprintln!(
                    "[hosted-network] the user's own hotspot was on before and did not come back \
                     ({})",
                    operation_status_name(status)
                );
            }
        }
        Ok(plan)
    };
    match restore() {
        Ok(plan) => {
            tprintln!(
                "[hosted-network] Mobile Hotspot handed back: settings {}, {}",
                if plan.reconfigure {
                    "restored"
                } else {
                    "left alone (changed since)"
                },
                if plan.restart {
                    "the user's hotspot restarted"
                } else if plan.stop {
                    "stopped"
                } else {
                    "nothing running"
                }
            );
            true
        }
        Err(e) => {
            teprintln!(
                "[hosted-network] restoring the Mobile Hotspot settings failed ({e}); keeping \
                 the snapshot for the next start"
            );
            false
        }
    }
}

fn breadcrumb_path(app: &AppHandle) -> Option<PathBuf> {
    crate::data_dir::local_data_dir(app)
        .ok()
        .map(|dir| dir.join(HOTSPOT_RESTORE_FILE))
}

fn read_breadcrumb(path: &Path) -> Option<HotspotSnapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(snapshot) => Some(snapshot),
        Err(e) => {
            teprintln!(
                "[hosted-network] {} is unreadable ({e}); ignoring it",
                path.display()
            );
            None
        }
    }
}

/// Written before the radio is touched, through a temporary file and a rename, so a crash at
/// any point leaves either no breadcrumb or a whole one.
fn write_breadcrumb(path: &Path, snapshot: &HotspotSnapshot) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(snapshot).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn remove_breadcrumb(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => teprintln!("[hosted-network] could not remove {}: {e}", path.display()),
    }
}

/// Stop a Mobile Hotspot this process started and give the user's configuration back. The
/// breadcrumb goes only when that worked; true when it did.
fn hand_back(restore: &HotspotRestore) -> bool {
    let done = restore_hotspot(&restore.snapshot, true);
    if done {
        if let Some(path) = &restore.breadcrumb {
            remove_breadcrumb(path);
        }
    }
    done
}

/// After a failed start has handed the hotspot back, an earlier start's record of it is spent:
/// the hotspot running now (if any) is the user's, and a later stop would stop it as ours
/// (`started_here`). Kept when the hand-back failed, so a stop can retry it.
fn release_handed_back(active: &mut Option<Active>, restored: bool) {
    if restored && matches!(active, Some(Active::MobileHotspot(_))) {
        *active = None;
    }
}

/// Start the Mobile Hotspot; returns the band it was configured for. The user's configuration is
/// snapshotted to disk before anything is changed, and a start that fails gives it straight back.
fn start_mobile_hotspot(
    name: &str,
    password: &str,
    breadcrumb: Option<&Path>,
    policy: BandPolicy,
) -> Result<&'static str> {
    let manager = tethering_manager().ok_or_else(|| {
        windows::core::Error::new(windows::core::HRESULT(-1), "no tethering manager")
    })?;

    let current = read_hotspot(&manager)?;
    HOTSPOT_TAKEN_OVER.store(true, Ordering::SeqCst);
    let earlier = match &*active().lock().unwrap() {
        Some(Active::MobileHotspot(running)) => Some(running.snapshot.clone()),
        _ => None,
    }
    .or_else(|| breadcrumb.and_then(read_breadcrumb));
    let snapshot = snapshot_for_start(earlier, current, name);
    if let Some(path) = breadcrumb {
        if let Err(e) = write_breadcrumb(path, &snapshot) {
            teprintln!(
                "[hosted-network] could not save the Mobile Hotspot snapshot to {} ({e}); a \
                 crash would leave the app's settings in place",
                path.display()
            );
        }
    }
    let restore = HotspotRestore {
        snapshot,
        breadcrumb: breadcrumb.map(Path::to_path_buf),
    };

    match configure_and_start(&manager, name, password, policy) {
        Ok(band) => {
            *active().lock().unwrap() = Some(Active::MobileHotspot(restore));
            Ok(band)
        }
        Err(e) => {
            let restored = hand_back(&restore);
            release_handed_back(&mut active().lock().unwrap(), restored);
            Err(e)
        }
    }
}

/// Give back a Mobile Hotspot a previous run of the app configured and never restored: it
/// crashed, was killed, or exited without the desktop UI's stop. Called once at startup; the
/// work runs on its own thread, and only when a breadcrumb exists.
pub fn spawn_crash_recovery(app: &AppHandle) {
    let Some(path) = breadcrumb_path(app) else {
        return;
    };
    if !path.exists() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("hotspot-restore".into())
        .spawn(move || {
            let _op = hotspot_op();
            if HOTSPOT_TAKEN_OVER.load(Ordering::SeqCst) {
                // a start in this process got there first and took the snapshot over
                return;
            }
            let Some(snapshot) = read_breadcrumb(&path) else {
                remove_breadcrumb(&path);
                return;
            };
            tprintln!(
                "[hosted-network] a previous run left the Mobile Hotspot configured as '{}'; \
                 restoring the user's settings",
                snapshot.ours
            );
            if restore_hotspot(&snapshot, false) {
                remove_breadcrumb(&path);
            }
        });
    if let Err(e) = spawned {
        teprintln!("[hosted-network] could not start the hotspot restore thread: {e}");
    }
}

// -- Wi-Fi Direct -----------------------------------------------------------------------------

fn start_wifi_direct_(
    name: &str,
    password: &str,
    success_tx: Sender<bool>,
) -> Result<WiFiDirectAdvertisementPublisher> {
    // Mobile Hotspot and Wi-Fi Direct cannot run at once, and Mobile Hotspot wins. A missing
    // tethering manager (no internet, no profile) must not block this tier: it needs no profile.
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
/// no client can join, whatever the radio's own status says.
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

/// The one passphrase rule, for every tier and for both ends of the elevated hand-over: a WPA2
/// passphrase is 8 to 63 printable ASCII characters.
pub fn validate_passphrase(password: &str) -> std::result::Result<(), String> {
    let len = password.chars().count();
    if !(MIN_PASSPHRASE..=MAX_PASSPHRASE).contains(&len) {
        return Err(format!(
            "the hotspot password must be {MIN_PASSPHRASE}-{MAX_PASSPHRASE} characters (got {len})"
        ));
    }
    if !password.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err("the hotspot password must be printable ASCII".to_string());
    }
    Ok(())
}

// -- commands ---------------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn start_hosted_network(app: AppHandle, name: String, password: String) -> bool {
    on_blocking_pool("start_hosted_network", false, move || {
        let state = app.state::<AppState>();
        start_hosted_network_blocking(&app, &state, &name, &password)
    })
    .await
}

fn start_hosted_network_blocking(
    app: &AppHandle,
    state: &AppState,
    name: &str,
    password: &str,
) -> bool {
    let _op = hotspot_op();
    if let Err(e) = validate_passphrase(password) {
        teprintln!("[hosted-network] {e}");
        record(state, None, name, None, false, None);
        return false;
    }

    let mut started: Option<(HostedNetworkBackend, Option<&'static str>)> = None;

    if tethering_capability() == Some(TetheringCapability::Enabled) {
        let policy = BandPolicy::from_switches();
        let breadcrumb = breadcrumb_path(app);
        match start_mobile_hotspot(name, password, breadcrumb.as_deref(), policy) {
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

    if started.is_none() && supports_legacy_hosted_network(app) && start_legacy(app, name, password)
    {
        tprintln!("[hosted-network] legacy netsh hosted network started");
        *active().lock().unwrap() = Some(Active::NetshLegacy);
        started = Some((HostedNetworkBackend::NetshLegacy, None));
    }

    let Some((backend, band)) = started else {
        teprintln!("[hosted-network] no hosted-network backend on this machine could start an AP");
        if active().lock().unwrap().is_none() {
            // nothing of ours is left running, so a stop has nothing to do
            state.stop_hosted_network.lock().unwrap().take();
        }
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

    register_stopper(state);
    record(state, Some(backend), name, band, true, host_ipv4);
    true
}

/// Quote one argument the way `CommandLineToArgvW` and the Rust runtime's own parser read it
/// back: as exactly one argument, byte for byte. `ShellExecute` takes the parameters as a single
/// string, so an unquoted SSID such as `Living Room` reaches the elevated child as two.
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        // backslashes are literal unless a quote follows: before one, each is doubled and
        // the quote itself escaped
        let run = if c == '"' {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        out.extend(std::iter::repeat_n('\\', run));
        backslashes = 0;
        out.push(c);
    }
    // the closing quote follows any trailing backslashes, so they are doubled too
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// The elevated child's command line. It names the passphrase file, never the passphrase:
/// a process's command line is readable by every other process on the machine.
fn hostednetwork_parameters(name: &str, password_file: &std::path::Path) -> String {
    format!(
        "hostednetwork {} --password-file {}",
        quote_windows_arg(name),
        quote_windows_arg(&password_file.to_string_lossy())
    )
}

const HANDOFF_PREFIX: &str = "screenextend-hostednetwork-";

/// A file whose owner may still be waiting at its elevation prompt is never swept; one this old
/// is swept whatever its name says, in case its owner's process id has been reused.
const HANDOFF_STALE: Duration = Duration::from_secs(24 * 60 * 60);

/// Where the passphrase waits for the elevated child: the app's own local data directory, which
/// is per user (user, SYSTEM and Administrators only), follows `SCREENEXTEND_DATA_DIR` so
/// isolated hosts never share it, and goes with the uninstaller. `None` refuses the hand-over:
/// there is no fallback to the shared temporary directory.
pub fn handoff_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    crate::data_dir::local_data_dir(app)
        .ok()
        .map(|d| d.join("handoff"))
}

/// The passphrase file, deleted when this is dropped: after the parent's wait whatever came of
/// it, on every early return, and on a panic.
pub(crate) struct PassphraseFile(std::path::PathBuf);

impl PassphraseFile {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for PassphraseFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write the passphrase where the elevated child can read it and nobody else's command-line
/// listing can, under a name nothing else guesses that also names the process waiting on it.
/// The child deletes it after reading, and the parent's guard deletes it in any case.
fn write_passphrase_file(dir: &std::path::Path, password: &str) -> std::io::Result<PassphraseFile> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!(
        "{HANDOFF_PREFIX}{}-{}.key",
        std::process::id(),
        crate::streamer::session::mint_device_token()
    ));
    let guard = PassphraseFile(path);
    std::fs::write(guard.path(), password)?;
    Ok(guard)
}

/// Delete hand-over files a crashed run left behind. A file whose owner is still running (a CLI
/// `network start` waiting at its elevation prompt) is left alone. Blocking; called at host start
/// and before every hand-over.
pub fn sweep_passphrase_files(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(owner) = handoff_owner(&name.to_string_lossy()) else {
            continue;
        };
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > HANDOFF_STALE);
        if stale || !process_alive(owner) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The process a hand-over file belongs to, from its name.
fn handoff_owner(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(HANDOFF_PREFIX)?.strip_suffix(".key")?;
    rest.split_once('-')?.0.parse().ok()
}

fn process_alive(pid: u32) -> bool {
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    if pid == std::process::id() {
        return true;
    }
    // SAFETY: a query-only open of a process id; the handle is closed below.
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    let mut code = 0u32;
    // SAFETY: `handle` is the open process handle from above.
    let alive = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok() && code == STILL_ACTIVE;
    // SAFETY: closed once, here.
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
    alive
}

/// Read and delete the file [`write_passphrase_file`] wrote. Deleted even when unreadable.
pub fn take_passphrase_file(path: &std::path::Path) -> std::result::Result<String, String> {
    let read = std::fs::read_to_string(path);
    let _ = std::fs::remove_file(path);
    read.map_err(|e| format!("could not read the passphrase file {}: {e}", path.display()))
}

/// Set the legacy hosted network's passphrase through the WLAN service, which takes it in
/// memory: `netsh wlan set hostednetwork key=` would put it on netsh's command line.
pub fn set_legacy_passphrase(passphrase: &str) -> std::result::Result<(), String> {
    use windows::Win32::NetworkManagement::WiFi::{
        WlanHostedNetworkSetSecondaryKey, WLAN_HOSTED_NETWORK_REASON,
    };
    let key = passphrase_key_bytes(passphrase)?;
    let handle = WlanHandle::open().ok_or("could not open the WLAN service")?;
    let mut reason = WLAN_HOSTED_NETWORK_REASON::default();
    // SAFETY: `handle` is an open WLAN client handle and `key` outlives the call.
    let rc = unsafe {
        WlanHostedNetworkSetSecondaryKey(handle.0, &key, true, true, Some(&mut reason), None)
    };
    if rc != 0 {
        return Err(format!(
            "the WLAN service refused the hosted network passphrase (error {rc}, reason {})",
            reason.0
        ));
    }
    Ok(())
}

/// What the WLAN service takes for a passphrase: 8 to 63 printable ASCII characters, followed by
/// the terminating NUL its length counts.
fn passphrase_key_bytes(passphrase: &str) -> std::result::Result<Vec<u8>, String> {
    validate_passphrase(passphrase)?;
    let mut key = passphrase.as_bytes().to_vec();
    key.push(0);
    Ok(key)
}

/// What became of the elevated `hostednetwork` child.
#[derive(Debug, PartialEq, Eq)]
pub enum ElevatedRun {
    Exited(u32),
    Declined,
    TimedOut,
    Failed(String),
}

/// The elevated child's process handle, closed on drop. Kept as an integer so a watcher thread
/// can own it: `HANDLE` is a raw pointer and not `Send`.
struct ElevatedChild(isize);

impl ElevatedChild {
    fn handle(&self) -> windows::Win32::Foundation::HANDLE {
        windows::Win32::Foundation::HANDLE(self.0 as *mut std::ffi::c_void)
    }

    /// `None` when the child is still running at the end of `timeout`.
    fn wait(&self, timeout: Duration) -> Option<ElevatedRun> {
        use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        let waited = unsafe { WaitForSingleObject(self.handle(), ms) };
        if waited == WAIT_TIMEOUT {
            return None;
        }
        if waited != WAIT_OBJECT_0 {
            return Some(ElevatedRun::Failed(format!(
                "waiting for the elevated child failed: {}",
                windows::core::Error::from_thread()
            )));
        }
        let mut code = 0u32;
        Some(
            match unsafe { GetExitCodeProcess(self.handle(), &mut code) } {
                Ok(()) => ElevatedRun::Exited(code),
                Err(e) => ElevatedRun::Failed(format!("GetExitCodeProcess: {e}")),
            },
        )
    }
}

impl Drop for ElevatedChild {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle()) };
    }
}

/// Launch `ScreenExtend hostednetwork <ssid> <password>` elevated, without waiting for it.
///
/// Not `elevated-command`: it is a bare `ShellExecuteW("runas")` that returns no handle and no
/// exit code, so a readback would run while the child (a whole Tauri process) is still starting.
/// `ShellExecuteExW` with `SEE_MASK_NOCLOSEPROCESS` gives the caller a process to wait on, the
/// same route as `driver_health::restart_elevated`.
///
/// The passphrase goes in a file under [`handoff_dir`]; the command line carries only its path.
fn launch_elevated_hostednetwork(
    app: &AppHandle,
    name: &str,
    password: &str,
) -> std::result::Result<(ElevatedChild, PassphraseFile), ElevatedRun> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
        SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let exe = std::env::current_exe()
        .map_err(|e| ElevatedRun::Failed(format!("could not resolve the executable: {e}")))?;
    let file = HSTRING::from(exe.as_os_str());
    let dir = handoff_dir(app).ok_or_else(|| {
        ElevatedRun::Failed(
            "no private directory to hand the passphrase over in; refusing".to_string(),
        )
    })?;
    sweep_passphrase_files(&dir);
    let key_file = write_passphrase_file(&dir, password)
        .map_err(|e| ElevatedRun::Failed(format!("could not hand the passphrase over: {e}")))?;
    let params = HSTRING::from(hostednetwork_parameters(name, key_file.path()));
    let verb = HSTRING::from("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        hwnd: HWND::default(),
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        if e.code() == ERROR_CANCELLED.to_hresult() {
            return Err(ElevatedRun::Declined);
        }
        return Err(ElevatedRun::Failed(format!("ShellExecuteExW: {e}")));
    }
    if info.hProcess.is_invalid() {
        return Err(ElevatedRun::Failed(
            "the elevated child returned no process handle".to_string(),
        ));
    }
    Ok((ElevatedChild(info.hProcess.0 as isize), key_file))
}

/// Run `ScreenExtend hostednetwork <ssid> <password>` elevated and **wait** for it, up to
/// `timeout`.
pub fn run_elevated_hostednetwork(
    app: &AppHandle,
    name: &str,
    password: &str,
    timeout: Duration,
) -> ElevatedRun {
    match launch_elevated_hostednetwork(app, name, password) {
        // `_key` deletes the hand-over file once the wait is over, whatever it returned
        Ok((child, _key)) => child.wait(timeout).unwrap_or(ElevatedRun::TimedOut),
        Err(run) => run,
    }
}

fn start_legacy(app: &AppHandle, name: &str, password: &str) -> bool {
    let run = match launch_elevated_hostednetwork(app, name, password) {
        Ok((child, key)) => match child.wait(LEGACY_ELEVATED_TIMEOUT) {
            Some(run) => run,
            None => {
                teprintln!(
                    "[hosted-network] elevated `hostednetwork` did not finish within {} s; \
                     watching it for {} s more so an AP it brings up is recorded and can be \
                     stopped",
                    LEGACY_ELEVATED_TIMEOUT.as_secs(),
                    LEGACY_LATE_TIMEOUT.as_secs()
                );
                // the hand-over file stays while the child is watched (it may still be at its
                // elevation prompt) and goes when the watch ends
                watch_late_legacy(app.clone(), child, key, name.to_string());
                return false;
            }
        },
        Err(run) => run,
    };
    match run {
        ElevatedRun::Exited(code) => {
            if code != 0 {
                teprintln!("[hosted-network] elevated `hostednetwork` exited with {code}");
            }
            // the child exits 0 once netsh has run, whatever netsh said: the WLAN service's
            // own state is the verdict
            legacy_started(app)
        }
        ElevatedRun::Declined => {
            teprintln!(
                "[hosted-network] the administrator prompt for the legacy tier was dismissed"
            );
            false
        }
        ElevatedRun::TimedOut => false,
        ElevatedRun::Failed(e) => {
            teprintln!("[hosted-network] elevated `hostednetwork` failed: {e}");
            false
        }
    }
}

/// Whether an AP the slow elevated child brought up after the start gave up on it should be
/// recorded as ours: only when the WLAN service says it is up and no other tier has been
/// started since (that one owns the record now).
fn adopt_late_legacy(another_tier_active: bool, legacy_up: Option<bool>) -> bool {
    !another_tier_active && legacy_up == Some(true)
}

/// Wait out a child that outlived `LEGACY_ELEVATED_TIMEOUT`, on its own thread, and record the
/// AP if it came up. The start has already reported failure; what this buys is that the AP is
/// then on the network card and a stop reaches it, rather than running unowned until reboot.
fn watch_late_legacy(app: AppHandle, child: ElevatedChild, key: PassphraseFile, name: String) {
    let spawned = std::thread::Builder::new()
        .name("hostednetwork-late".into())
        .spawn(move || {
            let run = child.wait(LEGACY_LATE_TIMEOUT);
            drop(key);
            drop(child);
            let _op = hotspot_op();
            let another_tier_active = active().lock().unwrap().is_some();
            let legacy_up = legacy_hosted_network_active();
            if !adopt_late_legacy(another_tier_active, legacy_up) {
                tprintln!(
                    "[hosted-network] the slow elevated `hostednetwork` finished ({run:?}); \
                     legacy AP up: {legacy_up:?}, another tier active: {another_tier_active}; \
                     not recorded"
                );
                return;
            }
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            *active().lock().unwrap() = Some(Active::NetshLegacy);
            let host_ipv4 = super::networking::hosted_network_ipv4().into_iter().next();
            register_stopper(&state);
            record(
                &state,
                Some(HostedNetworkBackend::NetshLegacy),
                &name,
                None,
                true,
                host_ipv4,
            );
            teprintln!(
                "[hosted-network] the legacy netsh hosted network '{name}' came up after the \
                 start gave up on it ({run:?}); recorded so it can be stopped"
            );
        });
    if let Err(e) = spawned {
        teprintln!("[hosted-network] could not watch the slow elevated child: {e}");
    }
}

/// Whether the legacy hosted network is up, read after the elevated child has finished.
pub fn legacy_hosted_network_active() -> Option<bool> {
    wlan_hosted_network_active()
}

fn legacy_started(app: &AppHandle) -> bool {
    if let Some(active) = legacy_hosted_network_active() {
        return active;
    }
    // Reached only when the WLAN service did not answer. English-only: on a localized Windows a
    // running hosted network reads as stopped here.
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

/// Marks a network of ours as registered for `stop_hosted_network`. That stop dispatches on the
/// tier itself, so it can see each tier's result and keep the record when a stop fails; the
/// closure is the same hand-back for any other caller, and must not take the operation lock.
fn register_stopper(state: &AppState) {
    let mut stop_func = state.stop_hosted_network.lock().unwrap();
    *stop_func = Some(Box::new(move || {
        if let Some(active) = active().lock().unwrap().take() {
            match active {
                Active::MobileHotspot(restore) => {
                    hand_back(&restore);
                }
                Active::WiFiDirect(publisher) => stop_publisher(&publisher),
                Active::NetshLegacy => {}
            }
        }
    }));
}

fn record(
    state: &AppState,
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

/// What a stop reports and what `hosted_network_running` becomes.
///
/// `registered` is whether this process had a network of its own to stop. With nothing
/// registered the stop reports success and clears the flag: left set, it labels the user's own
/// Wi-Fi Direct adapter with the app's SSID. A stop that failed leaves the flag as it was.
fn stop_outcome(registered: bool, stopped: bool, was_running: bool) -> (bool, bool) {
    match (registered, stopped) {
        (false, _) => (true, false),
        (true, true) => (true, false),
        (true, false) => (false, was_running),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn stop_hosted_network(app: AppHandle) -> bool {
    on_blocking_pool("stop_hosted_network", false, move || {
        let state = app.state::<AppState>();
        stop_hosted_network_blocking(&app, &state)
    })
    .await
}

pub(crate) fn stop_hosted_network_blocking(app: &AppHandle, state: &AppState) -> bool {
    let _op = hotspot_op();
    // Taken, not borrowed: a stopper that has run has nothing left to stop. Both go back if the
    // stop fails.
    let stopper = state.stop_hosted_network.lock().unwrap().take();
    let running_tier = active().lock().unwrap().take();
    let registered = running_tier.is_some() || stopper.is_some();

    let stopped = match running_tier {
        Some(Active::NetshLegacy) => {
            let status = tauri::async_runtime::block_on(async {
                app.shell()
                    .command("netsh")
                    .args(["wlan", "stop", "hostednetwork"])
                    .status()
                    .await
            });
            let ok = match status {
                Ok(status) => status.success(),
                Err(e) => {
                    teprintln!("[hosted-network] `netsh wlan stop hostednetwork` failed: {e}");
                    false
                }
            };
            if !ok {
                // still ours and still up: keep it for the next attempt
                *active().lock().unwrap() = Some(Active::NetshLegacy);
                *state.stop_hosted_network.lock().unwrap() = stopper;
            }
            ok
        }
        Some(Active::MobileHotspot(restore)) => {
            let ok = hand_back(&restore);
            if !ok {
                // our SSID may still be on the air, and the snapshot is this process's only
                // copy of the user's configuration: keep both for the next attempt
                *active().lock().unwrap() = Some(Active::MobileHotspot(restore));
                *state.stop_hosted_network.lock().unwrap() = stopper;
            }
            ok
        }
        Some(Active::WiFiDirect(publisher)) => {
            stop_publisher(&publisher);
            true
        }
        // a stopper with nothing recorded behind it has nothing to do
        None => true,
    };

    if stopped {
        *status().lock().unwrap() = HostedNetworkStatus::default();
    }
    let mut running = state.hosted_network_running.lock().unwrap();
    let (reported, now_running) = stop_outcome(registered, stopped, *running);
    *running = now_running;
    reported
}

/// Everything the network card can show: the tri-state, the backend, the band and the
/// authentication. It re-reads liveness through `is_hosted_network` first so a hotspot the
/// OS took down on its own is not still reported as advertised.
#[tauri::command]
#[specta::specta]
pub async fn get_hosted_network_status(app: AppHandle) -> crate::HostedNetworkReport {
    on_blocking_pool(
        "get_hosted_network_status",
        not_started_report(),
        move || {
            let state = app.state::<AppState>();
            hosted_network_report(&state)
        },
    )
    .await
}

fn not_started_report() -> crate::HostedNetworkReport {
    crate::HostedNetworkReport {
        state: crate::HostedNetworkReport::NOT_STARTED.to_string(),
        backend: None,
        ssid: String::new(),
        channel: None,
        band: None,
        security: None,
        secured: false,
        host_ipv4: None,
        peer: None,
    }
}

fn hosted_network_report(state: &AppState) -> crate::HostedNetworkReport {
    let live = is_hosted_network_now(state);
    let status = hosted_network_status();
    let advertised = live && status.advertised;
    crate::HostedNetworkReport {
        state: if !advertised {
            crate::HostedNetworkReport::NOT_STARTED.to_string()
        } else if status.host_ipv4.is_some() {
            crate::HostedNetworkReport::VERIFIED.to_string()
        } else {
            crate::HostedNetworkReport::ADVERTISED.to_string()
        },
        backend: advertised
            .then(|| status.backend.map(|b| b.as_str().to_string()))
            .flatten(),
        ssid: if advertised {
            status.ssid
        } else {
            String::new()
        },
        // neither WinRT tier exposes the channel it picked, and asking `netsh` would put a
        // process spawn on a path the UI polls
        channel: None,
        band: advertised
            .then(|| status.band.map(str::to_string))
            .flatten(),
        security: advertised
            .then(|| {
                status
                    .backend
                    .and_then(|b| b.security())
                    .map(str::to_string)
            })
            .flatten(),
        secured: advertised,
        host_ipv4: if advertised { status.host_ipv4 } else { None },
        peer: None,
    }
}

/// Whether the hosted network is up. Reads the platform object we already hold: it must not
/// spawn a process, because the UI and the adapter list ask often.
#[tauri::command]
#[specta::specta]
pub async fn is_hosted_network(app: AppHandle) -> bool {
    on_blocking_pool("is_hosted_network", false, move || {
        let state = app.state::<AppState>();
        is_hosted_network_now(&state)
    })
    .await
}

fn is_hosted_network_now(state: &AppState) -> bool {
    let live = match &*active().lock().unwrap() {
        Some(Active::MobileHotspot(_)) => Some(tethering_manager().is_some_and(|manager| {
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
        // the legacy tier's only readback is `netsh`, and this is a hot path
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

    /// The locale-independent legacy probes; needs no elevation.
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

    fn snapshot(ssid: &str, was_on: bool, ours: &str) -> HotspotSnapshot {
        HotspotSnapshot {
            ssid: ssid.to_string(),
            passphrase: format!("{ssid}-secret"),
            band: Some(TetheringWiFiBand::TwoPointFourGigahertz.0),
            authentication: Some(TetheringWiFiAuthenticationKind::Wpa2.0),
            was_on,
            ours: ours.to_string(),
        }
    }

    #[test]
    fn a_start_keeps_the_users_snapshot_while_the_hotspot_is_still_ours() {
        let users = snapshot("Kitchen", true, "ScreenExtend");
        let now = snapshot("ScreenExtend", false, "");
        let kept = snapshot_for_start(Some(users.clone()), now, "ScreenExtend 2");
        assert_eq!(kept.ssid, "Kitchen");
        assert_eq!(kept.passphrase, "Kitchen-secret");
        assert!(kept.was_on);
        assert_eq!(kept.ours, "ScreenExtend 2");
    }

    #[test]
    fn a_start_takes_a_fresh_snapshot_when_the_user_changed_it_since() {
        let stale = snapshot("Kitchen", true, "ScreenExtend");
        let now = snapshot("Garage", false, "");
        let fresh = snapshot_for_start(Some(stale), now.clone(), "ScreenExtend");
        assert_eq!(fresh.ssid, "Garage");
        assert!(!fresh.was_on);
        assert_eq!(fresh.ours, "ScreenExtend");
        let first = snapshot_for_start(None, now, "ScreenExtend");
        assert_eq!(first.ssid, "Garage");
        assert_eq!(first.ours, "ScreenExtend");
    }

    #[test]
    fn restore_plan_gives_back_only_what_is_still_ours() {
        let was_on = snapshot("Kitchen", true, "ScreenExtend");
        let was_off = snapshot("Kitchen", false, "ScreenExtend");
        // our stop, our SSID, running: stop, restore, and bring the user's hotspot back
        assert_eq!(
            restore_plan(&was_on, "ScreenExtend", true, true),
            RestorePlan {
                stop: true,
                reconfigure: true,
                restart: true
            }
        );
        // theirs was off: stop and restore, nothing restarted
        assert_eq!(
            restore_plan(&was_off, "ScreenExtend", true, true),
            RestorePlan {
                stop: true,
                reconfigure: true,
                restart: false
            }
        );
        // crash recovery, still our SSID and still advertising: the radio is ours to stop
        assert_eq!(
            restore_plan(&was_off, "ScreenExtend", true, false),
            RestorePlan {
                stop: true,
                reconfigure: true,
                restart: false
            }
        );
        // crash recovery after the user renamed and restarted their own hotspot: hands off
        assert_eq!(
            restore_plan(&was_on, "Garage", true, false),
            RestorePlan {
                stop: false,
                reconfigure: false,
                restart: false
            }
        );
        // our own stop after the user renamed it mid-session: stop what this process
        // started, keep their new settings
        assert_eq!(
            restore_plan(&was_on, "Garage", true, true),
            RestorePlan {
                stop: true,
                reconfigure: false,
                restart: false
            }
        );
        // a failed start (nothing running): restore, and restart theirs if it was on
        assert_eq!(
            restore_plan(&was_on, "ScreenExtend", false, true),
            RestorePlan {
                stop: false,
                reconfigure: true,
                restart: true
            }
        );
    }

    #[test]
    fn breadcrumb_round_trips_and_is_removed() {
        let dir = std::env::temp_dir().join(format!(
            "screenextend-hotspot-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let path = dir.join(HOTSPOT_RESTORE_FILE);
        let written = snapshot("Kitchen \"5G\"", true, "ScreenExtend");
        write_breadcrumb(&path, &written).expect("write");
        assert_eq!(read_breadcrumb(&path), Some(written));
        assert!(!path.with_extension("json.tmp").exists());
        remove_breadcrumb(&path);
        assert!(!path.exists());
        assert_eq!(read_breadcrumb(&path), None);
        remove_breadcrumb(&path); // a second removal is quiet
        std::fs::write(&path, b"{not json").expect("write garbage");
        assert_eq!(read_breadcrumb(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn band_preference_puts_5ghz_first_unless_switched_off() {
        assert_eq!(
            band_preference(true),
            [
                TetheringWiFiBand::FiveGigahertz,
                TetheringWiFiBand::SixGigahertz
            ]
        );
        assert_eq!(
            band_preference(false),
            [
                TetheringWiFiBand::SixGigahertz,
                TetheringWiFiBand::FiveGigahertz
            ]
        );
    }

    #[test]
    fn band_interference_falls_back_to_auto_instead_of_retrying() {
        use TetheringOperationStatus as S;
        assert!(!worth_retrying(S::BandInterference, true));
        assert!(!worth_retrying(S::WiFiDeviceOff, true));
        assert!(!worth_retrying(S::RadioRestriction, true));
        assert!(!worth_retrying(S::Success, true));
        assert!(worth_retrying(S::OperationInProgress, true));

        let five = TetheringWiFiBand::FiveGigahertz;
        assert!(fall_back_to_auto(true, five, S::BandInterference));
        assert!(fall_back_to_auto(true, five, S::OperationInProgress));
        assert!(!fall_back_to_auto(true, five, S::WiFiDeviceOff));
        assert!(!fall_back_to_auto(true, five, S::RadioRestriction));
        assert!(!fall_back_to_auto(true, five, S::Success));
        assert!(!fall_back_to_auto(
            true,
            TetheringWiFiBand::Auto,
            S::BandInterference
        ));
    }

    /// `SE_TUNE_OFF=hotspot_band_auto_fallback`: `BandInterference` is retried on the pinned band
    /// like any other failure, and there is no start on auto.
    #[test]
    fn band_auto_fallback_switched_off_is_the_old_behaviour() {
        use TetheringOperationStatus as S;
        assert!(worth_retrying(S::BandInterference, false));
        assert!(worth_retrying(S::OperationInProgress, false));
        assert!(!worth_retrying(S::WiFiDeviceOff, false));
        assert!(!worth_retrying(S::RadioRestriction, false));
        assert!(!worth_retrying(S::AlreadyOn, false));
        let five = TetheringWiFiBand::FiveGigahertz;
        assert!(!fall_back_to_auto(false, five, S::BandInterference));
        assert!(!fall_back_to_auto(false, five, S::OperationInProgress));
    }

    #[test]
    fn a_failed_start_drops_the_record_it_handed_back() {
        let recorded = || {
            Some(Active::MobileHotspot(HotspotRestore {
                snapshot: snapshot("Kitchen", true, "ScreenExtend"),
                breadcrumb: None,
            }))
        };
        let mut active = recorded();
        release_handed_back(&mut active, true);
        assert!(active.is_none());

        // the hand-back failed: the record stays, so a stop can try again
        let mut active = recorded();
        release_handed_back(&mut active, false);
        assert!(matches!(active, Some(Active::MobileHotspot(_))));

        // another tier's record is not the hotspot's to drop
        let mut active = Some(Active::NetshLegacy);
        release_handed_back(&mut active, true);
        assert!(matches!(active, Some(Active::NetshLegacy)));
    }

    #[test]
    fn a_late_legacy_ap_is_adopted_only_when_up_and_unclaimed() {
        assert!(adopt_late_legacy(false, Some(true)));
        assert!(!adopt_late_legacy(true, Some(true)));
        assert!(!adopt_late_legacy(false, Some(false)));
        assert!(!adopt_late_legacy(false, None));
    }

    #[test]
    fn stop_never_marks_a_network_running() {
        // nothing of ours: success, and the flag is clear
        assert_eq!(stop_outcome(false, false, false), (true, false));
        assert_eq!(stop_outcome(false, false, true), (true, false));
        // ours and stopped
        assert_eq!(stop_outcome(true, true, true), (true, false));
        // ours and the stop failed: the flag stays as it was
        assert_eq!(stop_outcome(true, false, true), (false, true));
        assert_eq!(stop_outcome(true, false, false), (false, false));
    }

    /// `CommandLineToArgvW` is the reference parser the Rust runtime's argv follows.
    fn parse_like_windows(params: &str) -> Vec<String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::UI::Shell::CommandLineToArgvW;
        let line = HSTRING::from(format!("ScreenExtend.exe {params}"));
        let mut count = 0i32;
        let argv = unsafe { CommandLineToArgvW(PCWSTR(line.as_ptr()), &mut count) };
        assert!(!argv.is_null(), "CommandLineToArgvW failed");
        let out = (0..count as usize)
            .map(|i| unsafe { (*argv.add(i)).to_string().expect("utf-16") })
            .collect();
        unsafe { LocalFree(Some(HLOCAL(argv as *mut core::ffi::c_void))) };
        out
    }

    /// The SSID and the passphrase file reach the elevated child as exactly one argument each,
    /// whatever they contain, and the passphrase itself is not on the command line at all.
    #[test]
    fn elevated_parameters_survive_spaces_quotes_and_backslashes() {
        // unquoted, the SSID splits in two
        assert_eq!(
            parse_like_windows("hostednetwork Living Room 12345678").len(),
            5
        );
        let cases = [
            ("ScreenExtend", r"C:\Users\a\AppData\Local\Temp\k.key"),
            ("Living Room", r"C:\Users\Some One\AppData\Local\Temp\k.key"),
            ("say \"hi\"", r"C:\Temp\"),
            ("ends in a backslash\\", r"\\server\share\k.key"),
            ("quote after slashes\\\"", "k.key"),
            ("tab\there", " leading and trailing "),
            ("Café Wi-Fi", r"C:\Temp\naïve.key"),
            ("", "k.key"),
            ("-dash", "--also"),
        ];
        for (name, file) in cases {
            assert_eq!(
                parse_like_windows(&hostednetwork_parameters(name, std::path::Path::new(file))),
                vec![
                    "ScreenExtend.exe".to_string(),
                    "hostednetwork".to_string(),
                    name.to_string(),
                    "--password-file".to_string(),
                    file.to_string()
                ],
                "{name:?} / {file:?}"
            );
        }
    }

    /// A directory of its own for each test; the real one is the app's local data directory.
    fn handoff_test_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "se-handoff-{}",
            crate::streamer::session::mint_device_token()
        ))
    }

    #[test]
    fn the_passphrase_travels_in_a_file_that_is_gone_once_read() {
        let dir = handoff_test_dir();
        let key = write_passphrase_file(&dir, "correct horse battery").expect("writable");
        let path = key.path().to_path_buf();
        assert!(!hostednetwork_parameters("ScreenExtend", &path).contains("correct horse"));
        assert_eq!(
            take_passphrase_file(&path).as_deref(),
            Ok("correct horse battery")
        );
        assert!(!path.exists());
        assert!(take_passphrase_file(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The parent's guard deletes the file whatever happened to the child: it never ran (UAC
    /// dismissed, launch failed), timed out, or died before reading.
    #[test]
    fn the_handoff_file_is_gone_once_the_parent_is_done_with_it() {
        let dir = handoff_test_dir();
        let key = write_passphrase_file(&dir, "correct horse battery").expect("writable");
        let path = key.path().to_path_buf();
        assert!(path.exists());
        assert!(path.starts_with(&dir));
        assert_eq!(
            handoff_owner(&path.file_name().unwrap().to_string_lossy()),
            Some(std::process::id())
        );
        drop(key);
        assert!(!path.exists());

        // an early return or a panic between write and launch drops it the same way
        let path = std::panic::catch_unwind(|| {
            let key = write_passphrase_file(&dir, "correct horse battery").expect("writable");
            let path = key.path().to_path_buf();
            std::panic::panic_any(path);
        })
        .unwrap_err()
        .downcast::<std::path::PathBuf>()
        .expect("the path");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A run that was killed before its guard could run leaves a file and the next sweep takes
    /// it; a file whose owner is still running (waiting at its elevation prompt) is left alone.
    #[test]
    fn the_sweep_takes_only_files_whose_owner_has_gone() {
        let dir = handoff_test_dir();
        std::fs::create_dir_all(&dir).expect("writable");
        let file_of = |pid: u32| {
            let f = dir.join(format!(
                "{HANDOFF_PREFIX}{pid}-{}.key",
                crate::streamer::session::mint_device_token()
            ));
            std::fs::write(&f, "secret").expect("writable");
            f
        };
        let mut gone = std::process::Command::new("cmd")
            .args(["/c", "exit"])
            .spawn()
            .expect("cmd");
        let gone_pid = gone.id();
        gone.wait().expect("exited");
        drop(gone);
        let mut waiting = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .spawn()
            .expect("cmd");

        let crashed = file_of(gone_pid);
        let in_flight = file_of(waiting.id());
        let own = file_of(std::process::id());
        let other = dir.join("unrelated.txt");
        std::fs::write(&other, "keep").expect("writable");
        sweep_passphrase_files(&dir);
        let _ = waiting.kill();
        let _ = waiting.wait();

        assert!(!crashed.exists(), "its owner is gone");
        assert!(in_flight.exists(), "its owner is still waiting on it");
        assert!(own.exists());
        assert!(other.exists(), "nothing but hand-over files is touched");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Parent and child refuse the same passphrases, so the child never finds out late.
    #[test]
    fn parent_and_child_agree_on_what_a_passphrase_is() {
        for p in [
            "12345678",
            "correct horse battery",
            "tab\there!!",
            "short",
            "naive-pass\u{7f}",
        ] {
            assert_eq!(
                validate_passphrase(p).is_ok(),
                passphrase_key_bytes(p).is_ok(),
                "{p:?}"
            );
        }
        assert!(validate_passphrase("tab\there!!").is_err());
    }

    #[test]
    fn only_a_passphrase_the_wlan_service_takes_is_passed_to_it() {
        assert_eq!(passphrase_key_bytes("12345678"), Ok(b"12345678\0".to_vec()));
        assert!(passphrase_key_bytes("1234567").is_err());
        assert!(passphrase_key_bytes(&"a".repeat(64)).is_err());
        assert!(passphrase_key_bytes("naïve-pass").is_err());
    }
}

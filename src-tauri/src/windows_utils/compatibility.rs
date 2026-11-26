use windows::core::HSTRING;
use windows::Foundation::Metadata::ApiInformation;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE,
    DXGI_ERROR_NOT_FOUND,
};
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;
use wmi::WMIConnection;

use super::hosted_network::{probe_backend, HostedNetworkBackend};
use super::streamer::{select_vendor, Vendor};
use super::virtual_display::{probe_display_backend, DisplayBackend};
use crate::{api_key, BareMinimum, CompatibilityReport, UnsupportedApi};

/// Windows 10 20H1. Kept for the "minimum required" line the compatibility panel shows; the
/// decision of whether this host is supported is made by probing the capture backends.
const MIN_BUILD: u32 = 19041;

/// The first Windows 11 build. `ProductName` in the registry still reads "Windows 10" on
/// Windows 11, so the build number is the only honest source for the name.
const WIN11_BUILD: u32 = 22000;

struct Probes {
    wgc: bool,
    dxgi: bool,
    wifi_radios: bool,
    wmi: bool,
    audio_backend: &'static str,
    capture_backend: &'static str,
    encoder: EncoderProbe,
    display_backend: DisplayBackend,
    hosted_network: HostedNetworkBackend,
}

impl Probes {
    /// Used only when the probe thread itself could not run: a machine is never told it is
    /// unsupported because we failed to ask, and never told it works either.
    ///
    /// The capture flags stay `true` because they are the verdict (`os_supported`) and a probe
    /// that did not run is not evidence against the machine; everything else reads `unknown`.
    /// The display driver is still asked: that probe needs no COM, and its answer decides whether
    /// the bootstrap offers the install.
    fn fallback(why: &str) -> Self {
        teprintln!(
            "[compatibility] the capability probe did not run ({why}); reporting what could not \
             be probed as unknown"
        );
        Self {
            wgc: true,
            dxgi: true,
            wifi_radios: true,
            wmi: true,
            audio_backend: "unknown",
            capture_backend: "unknown",
            encoder: EncoderProbe {
                backend: "unknown".to_string(),
                software_adapter_only: false,
            },
            display_backend: probe_display_backend(),
            hosted_network: HostedNetworkBackend::Unsupported,
        }
    }
}

struct EncoderProbe {
    /// The encoder the pipeline would reach for first on this hardware. Owned rather than
    /// `&'static str` because the AMD arm can name the reason AMF was not available.
    backend: String,
    /// Every DXGI adapter on this machine is a software rasteriser (WARP / Basic Render Driver).
    software_adapter_only: bool,
}

fn read_os_info() -> (String, u32) {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let nt = match hklm.open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion") {
        Ok(nt) => nt,
        Err(_) => return ("Windows".to_string(), 0),
    };
    let product_name: String = nt.get_value("ProductName").unwrap_or_default();
    let display_version: String = nt.get_value("DisplayVersion").unwrap_or_default();
    let build: String = nt.get_value("CurrentBuild").unwrap_or_default();
    let build_num = build.parse::<u32>().unwrap_or(0);

    let product = if build_num >= WIN11_BUILD {
        "Windows 11".to_string()
    } else if product_name.is_empty() {
        "Windows".to_string()
    } else {
        product_name
    };
    let display = if display_version.is_empty() {
        String::new()
    } else {
        format!(" {display_version}")
    };
    let os_version = if build.is_empty() {
        format!("{product}{display}")
    } else {
        format!("{product}{display} (build {build})")
    };
    (os_version, build_num)
}

/// Every probe runs on a thread of this function's own: the WinRT and WMI calls want an MTA (a
/// blocking `join()` on a Tauri STA has no message pump behind it), and an app-spawned thread is
/// the only kind this crate may initialize. The caller's apartment cannot reach the answer, as
/// `com::test::the_compatibility_report_does_not_depend_on_the_callers_apartment` asserts.
fn run_probes() -> Probes {
    std::thread::spawn(|| -> Probes {
        let (_com, com_con) = match super::com::wmi_for_this_thread("compatibility-probe") {
            Ok(pair) => pair,
            Err(e) => return Probes::fallback(&format!("COM/WMI could not start: {e}")),
        };

        let wmi = WMIConnection::with_namespace_path("root\\StandardCimv2", com_con).is_ok();

        let wgc = wgc_supported();
        let dxgi = dxgi_supported();

        Probes {
            wgc,
            dxgi,
            wifi_radios: type_present("Windows.Devices.Radios.Radio"),
            wmi,
            audio_backend: probe_audio_backend(),
            capture_backend: if wgc {
                "WindowsGraphicsCapture"
            } else if dxgi {
                "DxgiDesktopDuplication"
            } else {
                "none"
            },
            encoder: probe_encoder(),
            display_backend: probe_display_backend(),
            hosted_network: probe_backend(None),
        }
    })
    .join()
    .unwrap_or_else(|_| Probes::fallback("the probe thread panicked"))
}

/// The report and the capture path must never disagree about whether WGC is available, so this
/// is a thin call into the one predicate the capture path itself gates on. A second copy that
/// lost a conjunct (upstream's `UniversalApiContract 8`, where that gate binds) would make the
/// report claim WGC on a host where capture would refuse to start.
fn wgc_supported() -> bool {
    crate::windows_capture::graphics_capture_api::wgc_supported()
}

/// Whether the DXGI Desktop Duplication *fallback* can actually run here.
///
/// `CreateDXGIFactory1().is_ok()` is true on every Windows since 7 and says nothing about
/// duplication: on a hybrid-graphics laptop `DuplicateOutput` returns `DXGI_ERROR_UNSUPPORTED`
/// on the primary display. So duplication is created and immediately dropped, once, on the
/// probe thread.
fn dxgi_supported() -> bool {
    if unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.is_err() {
        return false;
    }
    match super::streamer::dxgi::probe_duplication() {
        Ok(api) => {
            tprintln!(
                "dxgi: duplication works on at least one attached output (via {}); whether it \
                 works on a virtual display is decided when one is captured",
                api.name()
            );
            true
        }
        Err(e) => {
            teprintln!(
                "dxgi: duplication fallback is NOT available on this machine ({e:#}); \
                 Windows.Graphics.Capture is the only capture path here"
            );
            false
        }
    }
}

fn type_present(type_name: &str) -> bool {
    ApiInformation::IsTypePresent(&HSTRING::from(type_name)).unwrap_or(false)
}

/// System audio is WASAPI loopback, in one of two tiers, and the report names which.
///
/// `wasapi-process` is process loopback (the faster tier), `wasapi-endpoint` the default render
/// endpoint's loopback. Process loopback is probed by attempting the activation, as capture
/// does (`process_loopback::available`), and `SE_ENDPOINT_LOOPBACK=1` forces the endpoint tier
/// here as it does there. With no default output device neither tier has anything to loop back.
fn probe_audio_backend() -> &'static str {
    match probe_default_endpoint() {
        "none" => "none",
        _ => audio_tier_name(
            crate::envcfg::var("SE_ENDPOINT_LOOPBACK").as_deref() == Ok("1"),
            super::audio::process_loopback::available,
        ),
    }
}

/// Which tier the capture path will pick, by its own rule (`loopback.rs::acquire`).
fn audio_tier_name(
    endpoint_forced: bool,
    process_available: impl FnOnce() -> bool,
) -> &'static str {
    if !endpoint_forced && process_available() {
        "wasapi-process"
    } else {
        "wasapi-endpoint"
    }
}

fn probe_default_endpoint() -> &'static str {
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    let probe = || -> windows::core::Result<()> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
        unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }?;
        Ok(())
    };
    match probe() {
        // process loopback is tried first and the gate learns from the first real capture, so
        // before any capture this names the tier that will be tried (no activation here)
        Ok(()) if super::audio::process_loopback::refused() => "wasapi-endpoint",
        Ok(()) => "wasapi-process",
        Err(e) => {
            tprintln!("[compatibility] no default audio output endpoint: {e}");
            "none"
        }
    }
}

/// Which encoder the pipeline would reach for. `EncoderVendor::Auto` tries NVENC, then AMF,
/// then Quick Sync, then x264.
///
/// Adapters are classified by **PCI vendor id**, not description string: descriptions are
/// localized and OEM-rebranded, and WARP's names no vendor while reporting Microsoft's `0x1414`.
/// `select_vendor` is the fallback for an adapter whose id is unknown.
fn probe_encoder() -> EncoderProbe {
    let mut has_nvidia = false;
    let mut has_intel = false;
    let mut has_amd = false;
    // the first hardware adapter's vendor: the one the capture device lands on
    let mut first_hardware: Option<Vendor> = None;
    let mut hardware_adapters = 0usize;
    let mut software_adapters = 0usize;

    let factory = match unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
        Ok(factory) => factory,
        Err(_) => {
            return EncoderProbe {
                backend: "x264".to_string(),
                software_adapter_only: false,
            }
        }
    };

    for index in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(_) => break,
        };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..end]);

        if DXGI_ADAPTER_FLAG(desc.Flags as i32) & DXGI_ADAPTER_FLAG_SOFTWARE != DXGI_ADAPTER_FLAG(0)
            || name.contains("Basic Render Driver")
        {
            software_adapters += 1;
            continue;
        }
        hardware_adapters += 1;
        let vendor = match Vendor::from_pci_id(desc.VendorId) {
            Vendor::Unknown => select_vendor(&name),
            known => known,
        };
        first_hardware.get_or_insert(vendor);
        match vendor {
            Vendor::Nvidia => has_nvidia = true,
            Vendor::Intel => has_intel = true,
            Vendor::Amd => has_amd = true,
            Vendor::Unknown => {}
        }
    }

    // each hardware tier is asked the way the pipeline asks it, by opening an encoder: adapter
    // presence would report `nvenc` for a GeForce MX with no NVENC block or a rejected API version
    let tiers = EncoderTiers {
        nvenc: has_nvidia.then(nvenc_opens),
        quicksync: has_intel.then(quicksync_opens),
        amd: has_amd.then(|| amd_backend_name(first_hardware == Some(Vendor::Amd))),
        x264: x264_loads(),
    };

    EncoderProbe {
        backend: encoder_backend_name(&tiers),
        software_adapter_only: hardware_adapters == 0 && software_adapters > 0,
    }
}

/// What each encoder tier said when it was tried. `None` for a vendor with no adapter here.
struct EncoderTiers {
    nvenc: Option<Result<(), String>>,
    quicksync: Option<Result<(), String>>,
    /// [`amd_backend_name`]'s answer, which already names an AMF refusal.
    amd: Option<String>,
    x264: Result<(), String>,
}

/// The first tier of `EncoderVendor::Auto`'s chain (NVENC, AMF, Quick Sync, x264) that opened,
/// naming why a present vendor's hardware tier did not. `none` when not even x264 loads.
fn encoder_backend_name(t: &EncoderTiers) -> String {
    if let Some(Ok(())) = t.nvenc {
        return "nvenc".to_string();
    }
    if let Some(amd) = t.amd.as_deref().filter(|name| name.starts_with("amf")) {
        return amd.to_string();
    }
    if let Some(Ok(())) = t.quicksync {
        return "quicksync".to_string();
    }
    if let Err(e) = &t.x264 {
        tprintln!("[compatibility] x264 does not load either: {e}");
        return "none".to_string();
    }
    let why = [
        t.nvenc
            .as_ref()
            .and_then(|r| r.as_ref().err())
            .map(|e| format!("NVENC unavailable: {e}")),
        t.quicksync
            .as_ref()
            .and_then(|r| r.as_ref().err())
            .map(|e| format!("Quick Sync unavailable: {e}")),
    ]
    .into_iter()
    .flatten()
    .next();
    match (why, t.amd.as_deref()) {
        (Some(why), _) => format!("x264 ({why})"),
        // already `x264 (AMF unavailable: ...)`
        (None, Some(amd)) => amd.to_string(),
        (None, None) => "x264".to_string(),
    }
}

/// Small and legal everywhere: nothing is encoded, so the geometry only has to pass the
/// encoder's own range check.
fn probe_config() -> super::streamer::nvidia::encoder::EncoderConfig {
    super::streamer::nvidia::encoder::EncoderConfig {
        width: 1280,
        height: 720,
        fps: 60,
        bitrate_bps: 6_000_000,
        max_bitrate_bps: 6_000_000,
        profile: crate::streamer::config::H264Profile::High,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    }
}

/// An `anyhow` chain flattened to one short clause, so it can sit inside the card's string.
fn one_line(e: &anyhow::Error) -> String {
    const MAX: usize = 140;
    let s = format!("{e:#}")
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if s.chars().count() > MAX {
        s.chars().take(MAX - 1).collect::<String>() + "…"
    } else {
        s
    }
}

/// Open an NVENC session on the NVIDIA adapter and close it again.
///
/// Once per process. The report is asked for by the bootstrap, by `doctor` and again by the
/// compatibility card, and on a hybrid laptop each open wakes the discrete GPU; the answer only
/// changes with a driver update, which restarts the app anyway.
fn nvenc_opens() -> Result<(), String> {
    static ANSWER: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    ANSWER
        .get_or_init(|| {
            super::streamer::nvidia::encoder::Encoder::new(probe_config())
                .map(drop)
                .map_err(|e| one_line(&e))
        })
        .clone()
}

/// Open a Quick Sync session on the Intel adapter and close it again. Once per process, like
/// [`nvenc_opens`].
fn quicksync_opens() -> Result<(), String> {
    static ANSWER: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    ANSWER
        .get_or_init(|| {
            super::streamer::intel::encoder::Encoder::new(probe_config())
                .map(drop)
                .map_err(|e| one_line(&e))
        })
        .clone()
}

/// Whether the software fallback can load at all. Once per process.
fn x264_loads() -> Result<(), String> {
    static ANSWER: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    ANSWER
        .get_or_init(|| {
            super::streamer::x264::x264_sys::X264Api::load()
                .map(drop)
                .map_err(|e| one_line(&e))
        })
        .clone()
}

/// The AMD arm of [`probe_encoder`], split out so the AMF probe is one call.
///
/// `same_adapter` is whether the first hardware adapter (the one the capture device is created
/// on) is the AMD one. When it is not, `Backend::Amf` refuses ("AMD AMF requires an AMD D3D11
/// device") and the pipeline falls to `Backend::AmfCpu`, a readback per frame, which the name
/// reports.
fn amd_backend_name(same_adapter: bool) -> String {
    use super::streamer::amd::encoder::{probe_availability, AmfAvailability};
    match probe_availability() {
        AmfAvailability::Hardware if same_adapter => "amf".to_string(),
        AmfAvailability::Hardware => "amf (cpu bridge)".to_string(),
        AmfAvailability::Unavailable(reason) => format!("x264 (AMF unavailable: {reason})"),
    }
}

/// Turn what was probed into the capability notes the report carries.
///
/// Pure, and separate from [`check_system_requirements`], so the severity policy (which missing
/// capability may stop startup) can be tested on a machine that does not have the fault.
fn capability_notes(probes: &Probes) -> Vec<UnsupportedApi> {
    let mut unsupported: Vec<UnsupportedApi> = Vec::new();

    // With neither backend there is no picture to send: blocking. With one missing the other
    // still streams, so that is `optional` and the user is offered "Continue anyway".
    if !probes.wgc && !probes.dxgi {
        unsupported.push(UnsupportedApi::blocking(
            BareMinimum::Capture,
            "Screen Capture (Windows Graphics Capture / DXGI Desktop Duplication)",
            "Captures the screen to stream to connected devices.",
            "Windows 10 20H1 (build 19041)",
        ));
    } else if !probes.wgc {
        unsupported.push(UnsupportedApi::optional(
            api_key::CAPTURE_PREFERRED,
            "Windows Graphics Capture",
            "Preferred screen capture backend; falling back to DXGI Desktop Duplication.",
            "Windows 10 20H1 (build 19041)",
        ));
    }

    match probes.display_backend {
        DisplayBackend::IddDriver => {}
        // The app can fix a missing driver itself: the bootstrap runs the elevated installer and
        // retries (every first launch goes through this). `installable` keeps the row visible to
        // `doctor` and `status` without the desktop UI treating it as an unsupported OS.
        DisplayBackend::DriverNotInstalled => unsupported.push(UnsupportedApi::installable(
            api_key::DISPLAY_DRIVER,
            "Virtual Display Driver",
            "Not installed. Each connected device needs one; install the driver to continue.",
            "Windows 10 20H1 (build 19041)",
        )),
        // Installed, healthy, and held by another of our own processes. `optional`, not
        // `installable`: the bootstrap must not run the elevated installer against a working
        // driver. Not `blocking` either: quitting that process fixes it.
        DisplayBackend::DriverBusy { holder } => unsupported.push(UnsupportedApi::optional(
            api_key::DISPLAY_DRIVER_BUSY,
            "Virtual Display Driver",
            match holder {
                Some(pid) => format!(
                    "Installed and working, but another ScreenExtend instance (pid {pid}) is \
                     already using it. Quit that one, then try again."
                ),
                None => "Installed and working, but another ScreenExtend instance is already \
                         using it. Quit that one, then try again."
                    .to_string(),
            },
            "Windows 10 20H1 (build 19041)",
        )),
        DisplayBackend::DriverNotResponding => unsupported.push(UnsupportedApi::installable(
            api_key::DISPLAY_DRIVER_UNRESPONSIVE,
            "Virtual Display Driver",
            "Installed but not answering. Reinstall the driver, or restart this machine.",
            "Windows 10 20H1 (build 19041)",
        )),
    }

    // not even x264 loads and no hardware encoder opened: nothing can encode a frame
    if probes.encoder.backend == "none" {
        unsupported.push(UnsupportedApi::blocking(
            BareMinimum::Encoder,
            "H.264 encoder",
            "No hardware encoder opened and the bundled software encoder (libx264) could not be \
             loaded; reinstall ScreenExtend.",
            "any",
        ));
    }

    // a WARP-only machine is slow, not incapable: x264 encodes what WARP composes
    if probes.encoder.software_adapter_only {
        unsupported.push(UnsupportedApi::optional(
            api_key::GPU_ADAPTER,
            "Hardware graphics adapter",
            "Only a software rasteriser (WARP) was found; expect high latency and high CPU use.",
            "any GPU",
        ));
    }

    if probes.audio_backend == "none" {
        unsupported.push(UnsupportedApi::optional(
            api_key::AUDIO,
            "System Audio Capture",
            "No default audio output device, so there is nothing to capture.",
            "Windows 10",
        ));
    }

    if !probes.wifi_radios {
        unsupported.push(UnsupportedApi::optional(
            api_key::WIFI_RADIOS,
            "WinRT Wi-Fi Radios",
            "Detecting and toggling the Wi-Fi radio state.",
            "Windows 10",
        ));
    }

    if probes.hosted_network == HostedNetworkBackend::Unsupported {
        unsupported.push(UnsupportedApi::optional(
            api_key::HOSTED_NETWORK,
            "Wi-Fi Direct / Mobile Hotspot",
            "Hosting a Wi-Fi hotspot for devices to join.",
            "Windows 10",
        ));
    }

    if !probes.wmi {
        unsupported.push(UnsupportedApi::optional(
            api_key::WMI,
            "WMI (Windows Management Instrumentation)",
            "Enumerating network adapters and IP addresses.",
            "Windows",
        ));
    }

    unsupported
}

/// `async` in the Tauri sense: it runs on the async runtime rather than the UI's main thread,
/// which every probe below (the driver handshake (5 s deadline), WMI, DXGI duplication, the
/// encoder sessions) used to freeze. Still a plain function for `doctor` and the tests.
#[tauri::command(async)]
#[specta::specta]
pub fn check_system_requirements() -> CompatibilityReport {
    super::blocking_off_the_workers(check_system_requirements_blocking)
}

fn check_system_requirements_blocking() -> CompatibilityReport {
    let (os_version, build_num) = read_os_info();

    let probes = run_probes();
    // supported means a capture backend works here, not that the build number is high enough
    let os_supported = probes.wgc || probes.dxgi;
    let unsupported = capability_notes(&probes);

    if build_num != 0 && build_num < MIN_BUILD {
        tprintln!(
            "[compatibility] build {build_num} is below the stated minimum {MIN_BUILD}, but the \
             capture backends probed available"
        );
    }

    CompatibilityReport {
        os_name: "Windows".to_string(),
        os_version,
        min_os_version: "Windows 10 20H1 (build 19041) / Server 20H2 (build 19042)".to_string(),
        os_supported,
        unsupported_apis: unsupported,
        audio_backend: probes.audio_backend.to_string(),
        display_backend: probes.display_backend.as_str().to_string(),
        capture_backend: probes.capture_backend.to_string(),
        encoder_backend: super::streamer::pipeline::started_backend()
            .map(str::to_string)
            .unwrap_or_else(|| probes.encoder.backend.to_string()),
        hosted_network_backend: probes.hosted_network.as_str().to_string(),
        input_backend: crate::streamer::input::probe_backend().as_str().to_string(),
        display_driver_holder_pid: probes.display_backend.holder(),
        boot_id: crate::boot_id(),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn healthy() -> Probes {
        Probes {
            wgc: true,
            dxgi: true,
            wifi_radios: true,
            wmi: true,
            audio_backend: "wasapi-process",
            capture_backend: "WindowsGraphicsCapture",
            encoder: EncoderProbe {
                backend: "nvenc".to_string(),
                software_adapter_only: false,
            },
            display_backend: DisplayBackend::IddDriver,
            hosted_network: HostedNetworkBackend::WiFiDirect,
        }
    }

    fn keys(notes: &[UnsupportedApi], severity: &str) -> Vec<String> {
        notes
            .iter()
            .filter(|a| a.severity == severity)
            .map(|a| a.key.clone())
            .collect()
    }

    #[test]
    fn a_healthy_host_has_nothing_to_report() {
        assert!(capability_notes(&healthy()).is_empty());
    }

    /// Only a host that cannot create a display, cannot capture, or cannot encode may be
    /// stopped. Everything else is a degraded feature and the user gets "Continue anyway", so
    /// no combination of the optional faults may produce a blocking row.
    #[test]
    fn no_optional_fault_can_stop_startup() {
        let cases: Vec<(&str, Probes)> = vec![
            (
                "WGC absent, DXGI present",
                Probes {
                    wgc: false,
                    ..healthy()
                },
            ),
            (
                "no default audio endpoint",
                Probes {
                    audio_backend: "none",
                    ..healthy()
                },
            ),
            (
                "no Wi-Fi radios",
                Probes {
                    wifi_radios: false,
                    ..healthy()
                },
            ),
            (
                "no hosted network",
                Probes {
                    hosted_network: HostedNetworkBackend::Unsupported,
                    ..healthy()
                },
            ),
            (
                "no WMI",
                Probes {
                    wmi: false,
                    ..healthy()
                },
            ),
            (
                "WARP only",
                Probes {
                    encoder: EncoderProbe {
                        backend: "x264".to_string(),
                        software_adapter_only: true,
                    },
                    ..healthy()
                },
            ),
            (
                "the display driver is in use by another instance",
                Probes {
                    display_backend: DisplayBackend::DriverBusy {
                        holder: Some(71848),
                    },
                    ..healthy()
                },
            ),
            (
                "every optional fault at once",
                Probes {
                    wgc: false,
                    wifi_radios: false,
                    wmi: false,
                    audio_backend: "none",
                    hosted_network: HostedNetworkBackend::Unsupported,
                    encoder: EncoderProbe {
                        backend: "x264".to_string(),
                        software_adapter_only: true,
                    },
                    ..healthy()
                },
            ),
        ];
        for (what, probes) in cases {
            let notes = capability_notes(&probes);
            assert!(
                !notes.is_empty(),
                "{what}: a degraded capability must still be reported"
            );
            assert!(
                keys(&notes, crate::SEVERITY_BLOCKING).is_empty(),
                "{what}: produced a blocking row {:?}; only the bare minimum may block",
                keys(&notes, crate::SEVERITY_BLOCKING)
            );
        }
    }

    /// The two driver states are the app's own to fix, so they are `installable` and the
    /// bootstrap remediates them before it decides anything. They must stay distinguishable:
    /// a missing driver is installed, one that is not answering is reinstalled.
    #[test]
    fn the_driver_states_are_installable_and_distinct() {
        let missing = capability_notes(&Probes {
            display_backend: DisplayBackend::DriverNotInstalled,
            ..healthy()
        });
        assert_eq!(
            keys(&missing, crate::SEVERITY_INSTALLABLE),
            vec![api_key::DISPLAY_DRIVER.to_string()]
        );
        assert!(keys(&missing, crate::SEVERITY_BLOCKING).is_empty());

        let mute = capability_notes(&Probes {
            display_backend: DisplayBackend::DriverNotResponding,
            ..healthy()
        });
        assert_eq!(
            keys(&mute, crate::SEVERITY_INSTALLABLE),
            vec![api_key::DISPLAY_DRIVER_UNRESPONSIVE.to_string()]
        );
    }

    /// A driver whose pipe is merely taken is the one display-driver state that must **not** be
    /// remediated: as an `installable` row the bootstrap would remove and reinstall a healthy
    /// signed driver, one elevation prompt per pass, and still find the pipe held.
    #[test]
    fn a_busy_driver_is_reported_but_never_installed() {
        for holder in [Some(71848u32), None] {
            let notes = capability_notes(&Probes {
                display_backend: DisplayBackend::DriverBusy { holder },
                ..healthy()
            });
            assert_eq!(
                notes.iter().map(|a| a.key.as_str()).collect::<Vec<_>>(),
                vec![api_key::DISPLAY_DRIVER_BUSY],
                "the busy driver must be reported, and be the only row"
            );
            assert!(
                keys(&notes, crate::SEVERITY_INSTALLABLE).is_empty(),
                "a busy driver is not installable: there is nothing to install"
            );
            assert!(
                keys(&notes, crate::SEVERITY_BLOCKING).is_empty(),
                "a busy driver is not a verdict either: quitting the other instance fixes it"
            );
            if let Some(pid) = holder {
                assert!(
                    notes[0].description.contains(&pid.to_string()),
                    "the row must name the process to quit: {}",
                    notes[0].description
                );
            }
        }
    }

    /// `DriverBusy` travels in an existing string field, and the other side compares the value
    /// against a literal.
    #[test]
    fn the_busy_backend_reports_its_holder_separately() {
        let backend = DisplayBackend::DriverBusy {
            holder: Some(71848),
        };
        assert_eq!(backend.as_str(), "DriverBusy");
        assert_eq!(backend.holder(), Some(71848));
    }

    #[test]
    fn no_encoder_at_all_is_the_bare_minimum() {
        let notes = capability_notes(&Probes {
            encoder: EncoderProbe {
                backend: "none".to_string(),
                software_adapter_only: false,
            },
            ..healthy()
        });
        assert_eq!(
            keys(&notes, crate::SEVERITY_BLOCKING),
            vec![api_key::ENCODER.to_string()]
        );
    }

    /// The encoder row names the tier that actually opens, not the vendor whose adapter is present.
    #[test]
    fn the_encoder_row_names_the_tier_that_opened() {
        let tiers = |nvenc, quicksync, amd: Option<&str>, x264| EncoderTiers {
            nvenc,
            quicksync,
            amd: amd.map(str::to_string),
            x264,
        };
        let refused = |why: &str| Some(Err(why.to_string()));
        assert_eq!(
            encoder_backend_name(&tiers(Some(Ok(())), None, None, Ok(()))),
            "nvenc"
        );
        // an NVIDIA adapter without a working NVENC on a hybrid laptop: Quick Sync is next
        assert_eq!(
            encoder_backend_name(&tiers(refused("no NVENC"), Some(Ok(())), None, Ok(()))),
            "quicksync"
        );
        assert_eq!(
            encoder_backend_name(&tiers(refused("no NVENC"), None, None, Ok(()))),
            "x264 (NVENC unavailable: no NVENC)"
        );
        assert_eq!(
            encoder_backend_name(&tiers(None, None, Some("amf"), Ok(()))),
            "amf"
        );
        // a Radeon beside an Intel iGPU: AMF comes before Quick Sync
        assert_eq!(
            encoder_backend_name(&tiers(None, Some(Ok(())), Some("amf"), Ok(()))),
            "amf"
        );
        // and Quick Sync is what is left when AMF does not open
        assert_eq!(
            encoder_backend_name(&tiers(
                None,
                Some(Ok(())),
                Some("x264 (AMF unavailable: no runtime)"),
                Ok(())
            )),
            "quicksync"
        );
        assert_eq!(
            encoder_backend_name(&tiers(
                None,
                None,
                Some("x264 (AMF unavailable: no runtime)"),
                Ok(())
            )),
            "x264 (AMF unavailable: no runtime)"
        );
        assert_eq!(
            encoder_backend_name(&tiers(None, None, None, Ok(()))),
            "x264"
        );
        assert_eq!(
            encoder_backend_name(&tiers(refused("x"), None, None, Err("gone".into()))),
            "none"
        );
    }

    /// The two WASAPI tiers are told apart, by the same rule capture uses.
    #[test]
    fn the_audio_row_names_the_loopback_tier() {
        assert_eq!(audio_tier_name(false, || true), "wasapi-process");
        assert_eq!(audio_tier_name(false, || false), "wasapi-endpoint");
        assert_eq!(
            audio_tier_name(true, || panic!("a forced endpoint tier must not probe")),
            "wasapi-endpoint"
        );
    }

    #[test]
    fn losing_every_capture_backend_is_the_bare_minimum() {
        let notes = capability_notes(&Probes {
            wgc: false,
            dxgi: false,
            capture_backend: "none",
            ..healthy()
        });
        assert_eq!(
            keys(&notes, crate::SEVERITY_BLOCKING),
            vec![api_key::CAPTURE.to_string()]
        );
    }

    /// `bare_minimum_missing` is what the startup gate asks, so an `installable` row must
    /// never answer it: that is the whole point of the severity.
    #[test]
    fn an_installable_row_is_not_a_verdict() {
        let mut report = CompatibilityReport {
            os_name: "Windows".into(),
            os_version: "test".into(),
            min_os_version: "test".into(),
            os_supported: true,
            unsupported_apis: capability_notes(&Probes {
                display_backend: DisplayBackend::DriverNotInstalled,
                ..healthy()
            }),
            audio_backend: "wasapi-process".into(),
            display_backend: "none".into(),
            capture_backend: "WindowsGraphicsCapture".into(),
            encoder_backend: "nvenc".into(),
            hosted_network_backend: "WiFiDirect".into(),
            input_backend: "SendInput".into(),
            display_driver_holder_pid: None,
            boot_id: String::new(),
        };
        assert!(!report.bare_minimum_missing());
        assert_eq!(report.installable().count(), 1);
        report.os_supported = false;
        assert!(report.bare_minimum_missing());
    }

    /// The compatibility report and the capture path must answer the same question the same way.
    ///
    /// This passes trivially while both go through
    /// `windows_capture::graphics_capture_api::wgc_supported`, which is the point: it fails the
    /// moment somebody adds a term (such as `UniversalApiContract 8`) to one side only.
    #[test]
    fn the_report_and_the_capture_path_share_one_wgc_predicate() {
        let report = wgc_supported();
        let capture =
            crate::windows_capture::graphics_capture_api::GraphicsCaptureApi::is_supported()
                .unwrap_or(false);
        assert_eq!(
            report, capture,
            "compatibility::wgc_supported() and GraphicsCaptureApi::is_supported() disagree; \
             they must be the same predicate or the report can promise a backend that refuses \
             to start"
        );

        // `wgc_features()` gates on the same answer, so an unsupported host must report every
        // knob absent rather than a mix
        let features = crate::windows_capture::graphics_capture_api::wgc_features();
        if !report {
            assert_eq!(
                features.summary(),
                "-cursor -border -secondary-windows -min-update-interval -dirty-region \
                 -free-threaded-pool",
                "WGC is unsupported here, so no optional capture property may read present"
            );
        }
    }

    /// Prints every probed backend on this machine.
    #[test]
    #[ignore = "read-only report of this machine's probed backends; run with --ignored --nocapture"]
    fn report_backends() {
        let report = check_system_requirements();
        println!("os:             {}", report.os_version);
        println!("supported:      {}", report.os_supported);
        println!("capture:        {}", report.capture_backend);
        println!("encoder:        {}", report.encoder_backend);
        println!("display:        {}", report.display_backend);
        println!("audio:          {}", report.audio_backend);
        println!("hosted network: {}", report.hosted_network_backend);
        println!("input:          {}", report.input_backend);
        for api in &report.unsupported_apis {
            println!("  [{}] {} - {}", api.severity, api.name, api.description);
        }
    }
}

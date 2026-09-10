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
    /// Used only when the probe thread itself could not run, so that a machine is never told it
    /// is unsupported because we failed to ask.
    fn optimistic() -> Self {
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
            display_backend: DisplayBackend::IddDriver,
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

/// Every probe below runs on a thread of this function's own, and that is load-bearing twice
/// over: the WinRT and WMI calls want an MTA (a blocking `join()` on a Tauri STA has no message
/// pump behind it), and an app-spawned thread is the only kind this crate is allowed to
/// initialize. The caller's apartment therefore cannot reach the answer — which is what
/// `com::test::the_compatibility_report_does_not_depend_on_the_callers_apartment` asserts.
fn run_probes() -> Probes {
    std::thread::spawn(|| -> Probes {
        let Ok((_com, com_con)) = super::com::wmi_for_this_thread("compatibility-probe") else {
            return Probes::optimistic();
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
    .unwrap_or_else(|_| Probes::optimistic())
}

/// The report and the capture path must never disagree about whether WGC is available, so this
/// is a thin call into the one predicate the capture path itself gates on. It used to be a second
/// copy that dropped upstream's `UniversalApiContract 8` conjunct while
/// `GraphicsCaptureApi::is_supported()` kept it — which on a build where that gate binds made the
/// report claim WGC on a host where capture would refuse to start.
fn wgc_supported() -> bool {
    crate::windows_capture::graphics_capture_api::wgc_supported()
}

/// Whether the DXGI Desktop Duplication *fallback* can actually run here.
///
/// This used to be `CreateDXGIFactory1().is_ok()`, which is true on every Windows since 7 and
/// says nothing about duplication. On a hybrid-graphics laptop `DuplicateOutput` returns
/// `DXGI_ERROR_UNSUPPORTED` on the primary display, so the report claimed a fallback that could
/// never start. Duplication is created and immediately dropped, once, on the probe thread.
fn dxgi_supported() -> bool {
    if unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.is_err() {
        return false;
    }
    match super::streamer::dxgi::probe_duplication() {
        Ok(api) => {
            tprintln!("dxgi: duplication fallback available via {}", api.name());
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

/// System audio is WASAPI loopback on a render endpoint. With no default output device there
/// is nothing to loop back, and the audio toggle can never work.
fn probe_audio_backend() -> &'static str {
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
        Ok(()) => "wasapi",
        Err(e) => {
            tprintln!("[compatibility] no default audio output endpoint: {e}");
            "none"
        }
    }
}

/// Which encoder the pipeline would reach for. `EncoderVendor::Auto` tries NVENC, then Quick
/// Sync, then AMF, then x264.
///
/// Adapters are classified by **PCI vendor id**, not by their description string: descriptions are
/// localized and OEM-rebranded, and WARP's contains no vendor name at all while reporting
/// Microsoft's `0x1414`. `select_vendor` stays as the fallback for an adapter whose id is not one
/// we know, which is the only case a name can still help with.
///
/// The AMD arm goes further than the other two and actually asks AMF whether it would open here.
/// It used to report `"x264 (no AMD hardware encoder)"` unconditionally, which stopped being true
/// the day the AMF backend landed, and that string is shown in the compatibility card and read by
/// `doctor --json` — so an AMD user was being told the product could not use their GPU.
fn probe_encoder() -> EncoderProbe {
    let mut has_nvidia = false;
    let mut has_intel = false;
    let mut has_amd = false;
    // The first hardware adapter's vendor, which is the one the capture device lands on.
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

    let backend = if has_nvidia {
        "nvenc".to_string()
    } else if has_intel {
        "quicksync".to_string()
    } else if has_amd {
        amd_backend_name(first_hardware == Some(Vendor::Amd))
    } else {
        "x264".to_string()
    };

    EncoderProbe {
        backend,
        software_adapter_only: hardware_adapters == 0 && software_adapters > 0,
    }
}

/// The AMD arm of [`probe_encoder`], split out so the AMF probe is one call.
///
/// `same_adapter` is whether the first hardware adapter — the one the capture device is created
/// on — is the AMD one. When it is not, `Backend::Amf` refuses ("AMD AMF requires an AMD D3D11
/// device") and the pipeline falls to `Backend::AmfCpu`, which is a readback per frame and worth
/// saying out loud.
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
/// Pure, and separate from [`check_system_requirements`], because the severities are a policy
/// question — *which* missing capability is allowed to stop startup — and a policy is only
/// worth anything if it can be tested against a machine that does not have the fault.
fn capability_notes(probes: &Probes) -> Vec<UnsupportedApi> {
    let mut unsupported: Vec<UnsupportedApi> = Vec::new();

    // The only bare-minimum row this platform can produce: with neither backend there is no
    // picture to send, and no fallback left to try. One of the two missing is not a verdict —
    // the other one streams — so it is `optional` and the user is offered "Continue anyway".
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
        // The driver is the one "unsupported API" the app can fix by itself: the bootstrap
        // runs the elevated installer and retries, which is the flow every first launch (and
        // every fresh install, whose pre-uninstall hook removes the previous driver) goes
        // through. `installable` keeps the row visible to `doctor` and `status` without the
        // desktop UI treating a missing driver as an unsupported operating system.
        DisplayBackend::DriverNotInstalled => unsupported.push(UnsupportedApi::installable(
            api_key::DISPLAY_DRIVER,
            "Virtual Display Driver",
            "Not installed. Each connected device needs one; install the driver to continue.",
            "Windows 10 20H1 (build 19041)",
        )),
        DisplayBackend::DriverNotResponding => unsupported.push(UnsupportedApi::installable(
            api_key::DISPLAY_DRIVER_UNRESPONSIVE,
            "Virtual Display Driver",
            "Installed but not answering. Reinstall the driver, or restart this machine.",
            "Windows 10 20H1 (build 19041)",
        )),
    }

    // A WARP-only machine is slow, not incapable: x264 encodes what WARP composes, so this
    // costs latency and CPU rather than the ability to stream.
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

#[tauri::command]
#[specta::specta]
pub fn check_system_requirements() -> CompatibilityReport {
    let (os_version, build_num) = read_os_info();

    let probes = run_probes();
    // Supported means "a capture backend actually works here", not "the build number is high
    // enough". The build number stays as the text we show.
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
        encoder_backend: probes.encoder.backend.to_string(),
        hosted_network_backend: probes.hosted_network.as_str().to_string(),
        input_backend: crate::streamer::input::probe_backend().as_str().to_string(),
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
            audio_backend: "wasapi",
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

    /// The rule the owner set: only a host that cannot create a display, cannot capture, or
    /// cannot encode may be stopped. Everything else is a degraded feature and the user gets
    /// "Continue anyway", so no combination of the optional faults may produce a blocking row.
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

    /// No capture backend at all is the one thing that genuinely cannot be worked around.
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
    /// never answer it — that is the whole point of the severity.
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
            audio_backend: "wasapi".into(),
            display_backend: "none".into(),
            capture_backend: "WindowsGraphicsCapture".into(),
            encoder_backend: "nvenc".into(),
            hosted_network_backend: "WiFiDirect".into(),
            input_backend: "SendInput".into(),
        };
        assert!(!report.bare_minimum_missing());
        assert_eq!(report.installable().count(), 1);
        report.os_supported = false;
        assert!(report.bare_minimum_missing());
    }

    /// The compatibility report and the capture path must answer the same question the same way.
    ///
    /// They used to be two copies of the predicate that disagreed by one conjunct
    /// (`UniversalApiContract 8`), so a host where that gate bound was told WGC works while
    /// `GraphicsCaptureApi::new` returned `Unsupported` and every optional knob read absent. This
    /// passes trivially while both go through
    /// `windows_capture::graphics_capture_api::wgc_supported`, which is the point: it fails the
    /// moment somebody re-adds a term to one side only.
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
        // knob absent rather than a mix.
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

    /// Prints every probed backend on this machine. Run with `--ignored --nocapture`.
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

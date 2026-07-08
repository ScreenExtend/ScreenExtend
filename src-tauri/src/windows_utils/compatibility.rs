use windows::core::HSTRING;
use windows::Foundation::Metadata::ApiInformation;
use windows::Graphics::Capture::GraphicsCaptureSession;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;
use wmi::{COMLibrary, WMIConnection};

use crate::{CompatibilityReport, UnsupportedApi};

const MIN_BUILD: u32 = 19041;

struct Probes {
    wgc: bool,
    dxgi: bool,
    wifi_radios: bool,
    wifi_direct: bool,
    tethering: bool,
    wmi: bool,
}

impl Probes {
    fn optimistic() -> Self {
        Self {
            wgc: true,
            dxgi: true,
            wifi_radios: true,
            wifi_direct: true,
            tethering: true,
            wmi: true,
        }
    }
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

    let product = if product_name.is_empty() {
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
    let build_num = build.parse::<u32>().unwrap_or(0);
    (os_version, build_num)
}

fn run_probes() -> Probes {
    std::thread::spawn(|| -> Probes {
        let _com = match COMLibrary::new() {
            Ok(c) => c,
            Err(_) => return Probes::optimistic(),
        };

        let wmi = WMIConnection::with_namespace_path(
            "root\\StandardCimv2",
            unsafe { COMLibrary::assume_initialized() },
        )
        .is_ok();

        Probes {
            wgc: wgc_supported(),
            dxgi: dxgi_supported(),
            wifi_radios: type_present("Windows.Devices.Radios.Radio"),
            wifi_direct: type_present("Windows.Devices.WiFiDirect.WiFiDirectAdvertisementPublisher"),
            tethering: type_present("Windows.Networking.NetworkOperators.NetworkOperatorTetheringManager"),
            wmi,
        }
    })
    .join()
    .unwrap_or_else(|_| Probes::optimistic())
}

fn wgc_supported() -> bool {
    ApiInformation::IsApiContractPresentByMajor(
        &HSTRING::from("Windows.Foundation.UniversalApiContract"),
        8,
    )
    .unwrap_or(false)
        && GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

fn dxgi_supported() -> bool {
    unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.is_ok()
}

fn type_present(type_name: &str) -> bool {
    ApiInformation::IsTypePresent(&HSTRING::from(type_name)).unwrap_or(false)
}

#[tauri::command]
#[specta::specta]
pub fn check_system_requirements() -> CompatibilityReport {
    let (os_version, build_num) = read_os_info();
    let os_supported = build_num >= MIN_BUILD;

    let probes = run_probes();
    let mut unsupported: Vec<UnsupportedApi> = Vec::new();

    if !probes.wgc && !probes.dxgi {
        unsupported.push(UnsupportedApi {
            name: "Screen Capture (Windows Graphics Capture / DXGI Desktop Duplication)"
                .to_string(),
            description: "Captures the screen to stream to connected devices.".to_string(),
            required_version: "Windows 10 20H1 (build 19041)".to_string(),
            severity: "blocking".to_string(),
        });
    } else if !probes.wgc {
        unsupported.push(UnsupportedApi {
            name: "Windows Graphics Capture".to_string(),
            description: "Preferred screen capture backend; falling back to DXGI Desktop Duplication.".to_string(),
            required_version: "Windows 10 20H1 (build 19041)".to_string(),
            severity: "optional".to_string(),
        });
    }

    if !probes.wifi_radios {
        unsupported.push(UnsupportedApi {
            name: "WinRT Wi-Fi Radios".to_string(),
            description: "Detecting and toggling the Wi-Fi radio state.".to_string(),
            required_version: "Windows 10".to_string(),
            severity: "optional".to_string(),
        });
    }

    if !probes.wifi_direct || !probes.tethering {
        unsupported.push(UnsupportedApi {
            name: "Wi-Fi Direct / Mobile Hotspot".to_string(),
            description: "Hosting a Wi-Fi hotspot for devices to join.".to_string(),
            required_version: "Windows 10".to_string(),
            severity: "optional".to_string(),
        });
    }

    if !probes.wmi {
        unsupported.push(UnsupportedApi {
            name: "WMI (Windows Management Instrumentation)".to_string(),
            description: "Enumerating network adapters and IP addresses.".to_string(),
            required_version: "Windows".to_string(),
            severity: "optional".to_string(),
        });
    }

    CompatibilityReport {
        os_name: "Windows".to_string(),
        os_version,
        min_os_version: "Windows 10 20H1 (build 19041) / Server 20H2 (build 19042)".to_string(),
        os_supported,
        unsupported_apis: unsupported,
    }
}

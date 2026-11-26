//! Env stage on Windows: machine facts and capability probes by trial, in the macOS `env.json`
//! shape plus the Windows sections. Every probe is a read or a trial that leaves nothing behind.

use std::path::Path;

use serde_json::{json, Value as Json};
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

use super::sys;

/// Variables that change what the host, the encoders or the bench do. The runner clears them for
/// every child; `env` records what the parent had.
pub const CLEARED_VARS: &[&str] = &[
    // shared plans can set these through the runner
    "SE_VT_RECIPE",
    "SE_VT_QP_BPS",
    "SE_VT_SIM_HOLD",
    "SE_HOLD_PUSH_START",
    "SE_TEST_SOURCE",
    "SE_PERF_HOLD",
    "SE_PERF_HOLD_LINGER_MS",
    "SE_VT_SIM_HOLD_DELAY_MS",
    "SE_VT_SIM_HOLD_COST_MS",
    "SE_PLI_HEALTHY_CAP_MS",
    "SE_QP_CEILING_MBPS",
    "SE_BENCH_CLIENT_FRAME_MS",
    "SE_RAW_RECORD",
    "SE_FRAME_TRACE",
    "SE_INTRA_REFRESH",
    "SE_REQUIRE_HW",
    "SE_VT_DENY",
    "SE_TEST_HOOKS",
    "SE_TUNE_OFF",
    "SE_TUNE_ON",
    "SE_SCK_QUEUE_DEPTH",
    "SE_SCK_MIN_INTERVAL",
    "SE_SCK_PIXEL_FORMAT",
    "SE_SCK_SHOWS_CURSOR",
    "SE_CGDS_QUEUE_DEPTH",
    "SE_CGDS_MIN_FRAME_TIME",
    "SE_BENCH_DENY_SCREEN_RECORDING",
    "SCREENEXTEND_DATA_DIR",
    // Windows host switches
    "SE_CURSOR_OVERLAY",
    "SE_DRIVER_RESTART_THRESHOLD_MB",
    "SE_DRIVER_RESTART_THRESHOLD_THREADS",
    "SE_ENDPOINT_LOOPBACK",
    "SE_GPU_CLOCK_HOLD",
    "SE_GPU_SCHED_CLASS",
    "SE_INPUT_TRACE",
    "SE_INTEL_NO_PER_FRAME_FLUSH",
    "SE_LEAK_CYCLES",
    "SE_NVENC_LTR",
    "SE_NVENC_NO_ASYNC",
    "SE_TRANSPORT_WORKERS",
    "SE_ENC_RECIPE",
];

fn reg_string(path: &str, name: &str) -> Option<String> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(path)
        .ok()?
        .get_value::<String, _>(name)
        .ok()
        .map(|s| s.trim().to_string())
}

fn reg_u32(path: &str, name: &str) -> Option<u32> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(path)
        .ok()?
        .get_value::<u32, _>(name)
        .ok()
}

const NT_CURRENT: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
const BIOS: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
const CPU0: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";

fn memory_bytes() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe { GlobalMemoryStatusEx(&mut m) }.ok()?;
    Some(m.ullTotalPhys)
}

/// The active power scheme's GUID (read only).
fn power_scheme() -> Option<String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::System::Power::PowerGetActiveScheme;
    let mut guid: *mut windows::core::GUID = std::ptr::null_mut();
    if unsafe { PowerGetActiveScheme(None, &mut guid) }.0 != 0 || guid.is_null() {
        return None;
    }
    let s = format!("{:?}", unsafe { *guid });
    unsafe {
        let _ = LocalFree(Some(HLOCAL(guid.cast())));
    }
    Some(s)
}

/// Every active display path and its adapter, from the display-configuration API: DXGI lists an
/// output under the adapter that composes it, which on a hybrid machine is not the one driving
/// the panel. Which GPU composes an indirect display only its driver knows: reported as unknown.
fn display_paths(primary_device: Option<&str>) -> Json {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let paths = match crate::windows_utils::virtual_display::active_display_paths_checked() {
        Ok(p) => p,
        Err(e) => return json!({ "error": e }),
    };
    let mut by_luid: Vec<((u32, i32), String)> = Vec::new();
    if let Ok(factory) = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
        for i in 0.. {
            let Ok(a) = (unsafe { factory.EnumAdapters1(i) }) else {
                break;
            };
            if let Ok(d) = unsafe { a.GetDesc1() } {
                let end = d.Description.iter().position(|&c| c == 0).unwrap_or(128);
                by_luid.push((
                    (d.AdapterLuid.LowPart, d.AdapterLuid.HighPart),
                    String::from_utf16_lossy(&d.Description[..end]),
                ));
            }
        }
    }
    Json::Array(
        paths
            .iter()
            .map(|p| {
                let instance =
                    crate::windows_utils::virtual_display::adapter_instance_id(&p.target_adapter);
                let indirect = instance
                    .as_deref()
                    .is_some_and(|i| i.len() > 5 && i[..5].eq_ignore_ascii_case("ROOT\\"));
                let owner = by_luid
                    .iter()
                    .find(|(l, _)| *l == p.adapter_id)
                    .map(|(_, d)| d.clone());
                json!({
                    "device": p.gdi_name,
                    "monitor": p.friendly,
                    "primary": primary_device == Some(p.gdi_name.as_str()),
                    "adapter": owner,
                    "adapter_luid": format!("0x{:X}", ((p.adapter_id.1 as i64) << 32) | p.adapter_id.0 as i64),
                    "adapter_instance": instance,
                    "indirect": indirect,
                    "composing_adapter": if indirect { json!("unknown") } else { json!(owner) },
                })
            })
            .collect(),
    )
}

/// Every DXGI adapter with its LUID, memory, user-mode driver version and outputs.
fn adapters(primary_device: Option<&str>) -> Json {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };
    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(f) => f,
        Err(e) => return json!({ "error": e.to_string() }),
    };
    let mut out = Vec::new();
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        let Ok(d) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        let luid = (i64::from(d.AdapterLuid.HighPart) << 32) | i64::from(d.AdapterLuid.LowPart);
        let umd = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
            .ok()
            .map(|v| {
                let v = v as u64;
                format!(
                    "{}.{}.{}.{}",
                    v >> 48,
                    (v >> 32) & 0xFFFF,
                    (v >> 16) & 0xFFFF,
                    v & 0xFFFF
                )
            });
        let mut outputs = Vec::new();
        for j in 0.. {
            let Ok(o) = (unsafe { adapter.EnumOutputs(j) }) else {
                break;
            };
            if let Ok(od) = unsafe { o.GetDesc() } {
                let name = {
                    let end = od.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
                    String::from_utf16_lossy(&od.DeviceName[..end])
                };
                let r = od.DesktopCoordinates;
                outputs.push(json!({
                    "device": name,
                    "attached": od.AttachedToDesktop.as_bool(),
                    "desktop": [r.left, r.top, r.right, r.bottom],
                    "rotation": od.Rotation.0,
                    "primary": primary_device == Some(name.as_str()),
                }));
            }
        }
        let desc = {
            let end = d.Description.iter().position(|&c| c == 0).unwrap_or(128);
            String::from_utf16_lossy(&d.Description[..end])
        };
        out.push(json!({
            "index": i,
            "description": desc,
            "vendor_id": format!("0x{:04X}", d.VendorId),
            "device_id": format!("0x{:04X}", d.DeviceId),
            "subsys_id": format!("0x{:08X}", d.SubSysId),
            "revision": d.Revision,
            "luid": format!("0x{luid:X}"),
            "dedicated_video_mb": d.DedicatedVideoMemory / (1 << 20),
            "dedicated_system_mb": d.DedicatedSystemMemory / (1 << 20),
            "shared_system_mb": d.SharedSystemMemory / (1 << 20),
            "software": d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || d.VendorId == 0x1414,
            "umd_version": umd,
            "outputs": outputs,
        }));
    }
    Json::Array(out)
}

/// What the NVENC GPU clock hold would do right now: `SE_GPU_CLOCK_HOLD=0` off, `=1` on,
/// otherwise on while on AC power.
fn clock_hold_state(ac: Option<bool>) -> Json {
    let var = std::env::var("SE_GPU_CLOCK_HOLD").ok();
    let would_hold = match var.as_deref().map(str::trim) {
        Some("0") => Some(false),
        Some("1") => Some(true),
        _ => ac,
    };
    json!({ "env": var, "would_hold_on_an_nvenc_session": would_hold })
}

fn capabilities() -> Json {
    use crate::windows_utils::windows_capture::graphics_capture_api::{
        wgc_features, wgc_supported,
    };
    let f = wgc_features();
    // the audio and compatibility probes need COM; this is the child's own thread, not Tauri's
    let _com = crate::windows_utils::com::ensure_for_this_thread(
        "bench-env",
        crate::windows_utils::com::Apartment::Mta,
    );
    let loopback = crate::windows_utils::audio::process_loopback::available();
    let report = crate::windows_utils::compatibility::check_system_requirements();
    json!({
        "wgc": {
            "supported": wgc_supported(),
            "summary": f.summary(),
            "cursor_toggle": f.cursor_toggle,
            "border_toggle": f.border_toggle,
            "secondary_windows": f.secondary_windows,
            "min_update_interval": f.min_update_interval,
            "dirty_region": f.dirty_region,
            "free_threaded_pool": f.free_threaded_pool,
        },
        "process_loopback_audio": loopback,
        "compatibility_report": serde_json::to_value(&report).unwrap_or(Json::Null),
        "driver_health": serde_json::to_value(crate::windows_utils::driver_health::read_health())
            .unwrap_or(Json::Null),
        "timer_resolution": sys::timer_resolution(),
    })
}

pub fn collect(out_dir: &Path, parent_env: &Json) -> Json {
    let build = reg_string(NT_CURRENT, "CurrentBuild")
        .or_else(|| reg_string(NT_CURRENT, "CurrentBuildNumber"));
    let ubr = reg_u32(NT_CURRENT, "UBR");
    let product_version = build.as_ref().map(|b| match ubr {
        Some(u) => format!("10.0.{b}.{u}"),
        None => format!("10.0.{b}"),
    });
    let (ac, battery) = sys::ac_and_battery();
    let primary = sys::primary_display_mode();
    let model = match (
        reg_string(BIOS, "SystemManufacturer"),
        reg_string(BIOS, "SystemProductName"),
    ) {
        (Some(m), Some(p)) => Some(format!("{m} {p}")),
        (m, p) => m.or(p),
    };
    json!({
        "hostname": sys::hostname(),
        "collected_at": sys::local_stamp(),
        "os": {
            "product_version": product_version,
            "build": build,
            "ubr": ubr,
            "product_name": reg_string(NT_CURRENT, "ProductName"),
            "display_version": reg_string(NT_CURRENT, "DisplayVersion"),
            "edition": reg_string(NT_CURRENT, "EditionID"),
            "kernel": Json::Null,
        },
        "hw": {
            "model": model,
            "cpu_brand": reg_string(CPU0, "ProcessorNameString"),
            "cpu_mhz_nominal": reg_u32(CPU0, "~MHz"),
            "logical_cpu": std::thread::available_parallelism().ok().map(|n| n.get()),
            "memsize_bytes": memory_bytes(),
        },
        "process": {
            "arch": std::env::consts::ARCH,
            "translated": 0,
            "translated_sysctl_present": false,
        },
        "power": {
            "ac_line": ac,
            "battery_pct": battery,
            "power_scheme": power_scheme(),
            "gpu_clock_hold": clock_hold_state(ac),
            "nvidia_smi": sys::nvidia_smi(),
            "package_power_mw": sys::package_power(),
            "low_power_mode": Json::Null,
            "thermal_state": Json::Null,
        },
        "displays": {
            "primary": primary,
            "paths": display_paths(primary.as_ref().map(|p| p.device.as_str())),
        },
        "adapters": adapters(primary.as_ref().map(|p| p.device.as_str())),
        "capabilities": capabilities(),
        "counters": sys::probe(),
        "disk_free_bytes": sys::free_disk_bytes(out_dir),
        "build": {
            "git_describe": option_env!("SE_BENCH_GIT_DESCRIBE"),
            "package_version": env!("CARGO_PKG_VERSION"),
            "features": {
                "bench": cfg!(feature = "bench"),
                "probe": cfg!(feature = "probe"),
            },
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "target_arch": std::env::consts::ARCH,
            "executable": std::env::current_exe().ok().map(|p| p.display().to_string()),
        },
        "parent_env_cleared_for_children": parent_env,
    })
}

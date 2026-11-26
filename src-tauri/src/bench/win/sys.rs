//! System counters for the Windows bench: the `cf.rs` equivalents (host name, stamp, free disk)
//! and the per-process and machine counters each series row samples. A counter this machine does
//! not have is `null`, never 0; [`probe`] says which are present.

use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value as Json};
use windows::core::{s, w, PCWSTR};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::windows_utils::driver_health;

fn mb(bytes: u64) -> f64 {
    (bytes as f64 / 1e5).round() / 10.0
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// A function in an already-loaded system DLL, by name. `None` when it is not exported.
fn proc_addr(
    module: PCWSTR,
    name: windows::core::PCSTR,
) -> Option<unsafe extern "system" fn() -> isize> {
    let m = unsafe { GetModuleHandleW(module) }.ok()?;
    unsafe { GetProcAddress(m, name) }
}

/// The DNS host name, reduced to `[A-Za-z0-9-]` like the macOS one (it names the results file).
pub fn hostname() -> String {
    use windows::core::PWSTR;
    use windows::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
    let mut buf = [0u16; 256];
    let mut n = buf.len() as u32;
    let ok = unsafe {
        GetComputerNameExW(
            ComputerNameDnsHostname,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut n,
        )
    }
    .is_ok();
    let s = if ok {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown-host".into())
    };
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Local time as `YYYYMMDD-HHMMSS`.
pub fn local_stamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// Free bytes available to this user on the volume holding `path` (or its nearest existing
/// ancestor).
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    type F = unsafe extern "system" fn(PCWSTR, *mut u64, *mut u64, *mut u64) -> i32;
    let f = proc_addr(w!("kernel32.dll"), s!("GetDiskFreeSpaceExW"))?;
    let f: F = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, F>(f) };
    let mut dir = if path.as_os_str().is_empty() {
        std::env::current_dir().ok()?
    } else {
        path.to_path_buf()
    };
    if dir.is_relative() {
        dir = std::env::current_dir().ok()?.join(dir);
    }
    while !dir.exists() {
        dir = dir.parent()?.to_path_buf();
    }
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { f(PCWSTR(wide.as_ptr()), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some(avail)
}

/// Working set and peak working set, bytes: the Windows reading of the macOS `rss()`.
pub fn rss() -> (u64, u64) {
    match memory() {
        Some(m) => (m.working_set, m.peak_working_set),
        None => (0, 0),
    }
}

struct Memory {
    private: u64,
    working_set: u64,
    peak_working_set: u64,
}

fn memory() -> Option<Memory> {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            c.cb,
        )
    }
    .ok()?;
    Some(Memory {
        private: c.PrivateUsage as u64,
        working_set: c.WorkingSetSize as u64,
        peak_working_set: c.PeakWorkingSetSize as u64,
    })
}

fn handle_count() -> Option<u32> {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut n = 0u32;
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) }.ok()?;
    Some(n)
}

/// GDI and USER objects. `GetGuiResources` cannot tell "none" from a failure, and a process with
/// no user32 objects really owns none, so 0 stands.
fn gui_objects() -> (u32, u32) {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS, GR_USEROBJECTS,
    };
    unsafe {
        (
            GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS),
            GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS),
        )
    }
}

/// Rows in an IP Helper "owner pid" table that this process owns. `query(buf, size)` is the
/// table call; the row's pid sits at `pid_offset` within each `row_size`-byte row. With `port`
/// (the row's `dwLocalPort` offset and the port), only rows bound to that local port.
fn owned_rows(
    query: impl Fn(Option<*mut core::ffi::c_void>, &mut u32) -> u32,
    row_size: usize,
    pid_offset: usize,
    port: Option<(usize, u16)>,
) -> Option<usize> {
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let me = std::process::id();
    let mut size = 0u32;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..4 {
        let ptr = (!buf.is_empty()).then(|| buf.as_mut_ptr().cast::<core::ffi::c_void>());
        match query(ptr, &mut size) {
            0 if !buf.is_empty() => {
                let bytes =
                    unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) };
                let n = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
                // rows start after the u32 count, padded to the row's alignment (4 for these)
                let base = 4;
                let mut count = 0;
                for i in 0..n {
                    let o = base + i * row_size + pid_offset;
                    if o + 4 > bytes.len() {
                        return None;
                    }
                    if u32::from_le_bytes(bytes[o..o + 4].try_into().ok()?) != me {
                        continue;
                    }
                    if let Some((at, want)) = port {
                        // the port is in network byte order in the low two bytes
                        let q = base + i * row_size + at;
                        if u16::from_be_bytes([bytes[q], bytes[q + 1]]) != want {
                            continue;
                        }
                    }
                    count += 1;
                }
                return Some(count);
            }
            0 | ERROR_INSUFFICIENT_BUFFER => {
                buf = vec![0u64; (size as usize).div_ceil(8) + 64];
                size = (buf.len() * 8) as u32;
            }
            _ => return None,
        }
    }
    None
}

fn udp_sockets() -> Option<usize> {
    udp_sockets_on(None)
}

fn udp_sockets_on(port: Option<u16>) -> Option<usize> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedUdpTable, MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID, UDP_TABLE_OWNER_PID,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    let v4 = owned_rows(
        |p, size| unsafe {
            GetExtendedUdpTable(p, size, false, AF_INET.0 as u32, UDP_TABLE_OWNER_PID, 0)
        },
        std::mem::size_of::<MIB_UDPROW_OWNER_PID>(),
        std::mem::offset_of!(MIB_UDPROW_OWNER_PID, dwOwningPid),
        port.map(|p| (std::mem::offset_of!(MIB_UDPROW_OWNER_PID, dwLocalPort), p)),
    )?;
    let v6 = owned_rows(
        |p, size| unsafe {
            GetExtendedUdpTable(p, size, false, AF_INET6.0 as u32, UDP_TABLE_OWNER_PID, 0)
        },
        std::mem::size_of::<MIB_UDP6ROW_OWNER_PID>(),
        std::mem::offset_of!(MIB_UDP6ROW_OWNER_PID, dwOwningPid),
        port.map(|p| (std::mem::offset_of!(MIB_UDP6ROW_OWNER_PID, dwLocalPort), p)),
    )?;
    Some(v4 + v6)
}

fn tcp_sockets() -> Option<usize> {
    tcp_sockets_on(None)
}

fn tcp_sockets_on(port: Option<u16>) -> Option<usize> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    let v4 = owned_rows(
        |p, size| unsafe {
            GetExtendedTcpTable(p, size, false, AF_INET.0 as u32, TCP_TABLE_OWNER_PID_ALL, 0)
        },
        std::mem::size_of::<MIB_TCPROW_OWNER_PID>(),
        std::mem::offset_of!(MIB_TCPROW_OWNER_PID, dwOwningPid),
        port.map(|p| (std::mem::offset_of!(MIB_TCPROW_OWNER_PID, dwLocalPort), p)),
    )?;
    let v6 = owned_rows(
        |p, size| unsafe {
            GetExtendedTcpTable(
                p,
                size,
                false,
                AF_INET6.0 as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        },
        std::mem::size_of::<MIB_TCP6ROW_OWNER_PID>(),
        std::mem::offset_of!(MIB_TCP6ROW_OWNER_PID, dwOwningPid),
        port.map(|p| (std::mem::offset_of!(MIB_TCP6ROW_OWNER_PID, dwLocalPort), p)),
    )?;
    Some(v4 + v6)
}

/// This process's video memory on every hardware adapter, MB.
fn gpu_memory() -> Option<Json> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
        DXGI_QUERY_VIDEO_MEMORY_INFO,
    };
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut out = Vec::new();
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || desc.VendorId == 0x1414 {
            continue;
        }
        let luid =
            (i64::from(desc.AdapterLuid.HighPart) << 32) | i64::from(desc.AdapterLuid.LowPart);
        let read = |a: &IDXGIAdapter3, g| {
            let mut m = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            unsafe { a.QueryVideoMemoryInfo(0, g, &mut m) }
                .ok()
                .map(|()| m)
        };
        let (local, nonlocal) = match adapter.cast::<IDXGIAdapter3>() {
            Ok(a3) => (
                read(&a3, DXGI_MEMORY_SEGMENT_GROUP_LOCAL),
                read(&a3, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL),
            ),
            Err(_) => (None, None),
        };
        out.push(json!({
            "adapter": wide_to_string(&desc.Description),
            "vendor_id": format!("0x{:04X}", desc.VendorId),
            "luid": format!("0x{luid:X}"),
            "local_mb": local.map(|m| mb(m.CurrentUsage)),
            "local_budget_mb": local.map(|m| mb(m.Budget)),
            "nonlocal_mb": nonlocal.map(|m| mb(m.CurrentUsage)),
        }));
    }
    Some(Json::Array(out))
}

/// The driver's pipe server pid, asked once per process (the ask opens one client handle).
fn driver_pipe_server() -> driver_health::PipeProbe {
    static PIPE: OnceLock<driver_health::PipeProbe> = OnceLock::new();
    *PIPE.get_or_init(driver_health::probe_pipe_server)
}

/// `(threads of this process, the driver host)` from one process-list read.
fn process_list_counters() -> (Option<u32>, Option<Json>) {
    let Ok(procs) = driver_health::process_list() else {
        return (None, None);
    };
    let me = std::process::id();
    let threads = procs.iter().find(|p| p.pid == me).map(|p| p.threads);
    let pipe = driver_pipe_server();
    let host = match pipe {
        driver_health::PipeProbe::Absent => None,
        driver_health::PipeProbe::Server(pid) => {
            driver_health::select_driver_host(&procs, Some(pid))
        }
        driver_health::PipeProbe::Present => driver_health::select_driver_host(&procs, None),
    }
    .map(|(p, how)| {
        json!({
            "pid": p.pid,
            "identified_by": how.as_str(),
            "commit_mb": p.commit_mb(),
            "threads": p.threads,
            "handles": p.handles,
        })
    });
    (threads, host)
}

/// The per-process counters and the driver host's, for leak checks.
pub fn resources() -> Json {
    let m = memory();
    let (gdi, user) = gui_objects();
    let (threads, driver_host) = process_list_counters();
    json!({
        "private_mb": m.as_ref().map(|m| mb(m.private)),
        "rss_mb": m.as_ref().map(|m| mb(m.working_set)),
        "rss_peak_mb": m.as_ref().map(|m| mb(m.peak_working_set)),
        "handles": handle_count(),
        "gdi": gdi,
        "user": user,
        "threads": threads,
        "udp_sockets": udp_sockets(),
        "tcp_sockets": tcp_sockets(),
        "gpu_mem": gpu_memory(),
        "driver_host": driver_host,
    })
}

/// `GetSystemPowerStatus`: `(on AC, battery percent)`, each `None` when the OS says "unknown".
pub fn ac_and_battery() -> (Option<bool>, Option<u8>) {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut s) }.is_err() {
        return (None, None);
    }
    let ac = match s.ACLineStatus {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    // 128 = no system battery; 255 = unknown
    let pct =
        (s.BatteryFlag & 128 == 0 && s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent);
    (ac, pct)
}

/// `NtQueryTimerResolution`, ms: `{coarsest, finest, current}`. `finest` is what a process can
/// reach; `current` is what is in force for this process right now.
pub fn timer_resolution() -> Option<Json> {
    type F = unsafe extern "system" fn(*mut u32, *mut u32, *mut u32) -> i32;
    let f = proc_addr(w!("ntdll.dll"), s!("NtQueryTimerResolution"))?;
    let f: F = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, F>(f) };
    let (mut coarsest, mut finest, mut current) = (0u32, 0u32, 0u32);
    let st = unsafe { f(&mut coarsest, &mut finest, &mut current) };
    (st == 0).then(|| {
        json!({
            "coarsest_ms": coarsest as f64 / 1e4,
            "finest_ms": finest as f64 / 1e4,
            "current_ms": current as f64 / 1e4,
        })
    })
}

/// A display mode as `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DisplayMode {
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// The primary display device's current mode. `None` when no device carries the primary flag or
/// its mode cannot be read.
pub fn primary_display_mode() -> Option<DisplayMode> {
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayDevicesW, EnumDisplaySettingsW, DEVMODEW, DISPLAY_DEVICEW,
        DISPLAY_DEVICE_PRIMARY_DEVICE, ENUM_CURRENT_SETTINGS,
    };
    for i in 0..64 {
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
            break;
        }
        if !dd.StateFlags.contains(DISPLAY_DEVICE_PRIMARY_DEVICE) {
            continue;
        }
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !unsafe {
            EnumDisplaySettingsW(
                PCWSTR(dd.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dm,
            )
        }
        .as_bool()
        {
            return None;
        }
        return Some(DisplayMode {
            device: wide_to_string(&dd.DeviceName),
            width: dm.dmPelsWidth,
            height: dm.dmPelsHeight,
            refresh_hz: dm.dmDisplayFrequency,
        });
    }
    None
}

/// Compare the primary display's mode before and after a run. A width or height change
/// invalidates the run and is printed on stderr; a refresh change alone is reported only, since
/// adding a virtual display can lower the panel's refresh rate.
pub fn display_mode_check(before: Option<&DisplayMode>, after: Option<&DisplayMode>) -> Json {
    let size_changed = match (before, after) {
        (Some(b), Some(a)) => (b.width, b.height) != (a.width, a.height) || b.device != a.device,
        (Some(_), None) | (None, Some(_)) => true,
        (None, None) => false,
    };
    let refresh_changed =
        matches!((before, after), (Some(b), Some(a)) if b.refresh_hz != a.refresh_hz);
    if size_changed {
        eprintln!(
            "PRIMARY DISPLAY MODE CHANGED DURING THE RUN: before {:?}, after {:?}. \
             The run is invalid.",
            before, after
        );
    }
    json!({
        "before": before,
        "after": after,
        "size_changed": size_changed,
        "refresh_changed": refresh_changed,
        "valid": !size_changed,
    })
}

/// `nvidia-smi`, when it runs: one object per GPU with `power_w`, `pstate`, `graphics_mhz`.
/// `None` when the tool is not present (probed once) or this call failed.
pub fn nvidia_smi() -> Option<Json> {
    static PRESENT: OnceLock<bool> = OnceLock::new();
    let run = || -> Option<Json> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = Command::new("nvidia-smi")
            .args([
                "--query-gpu=index,name,power.draw,pstate,clocks.gr",
                "--format=csv,noheader,nounits",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let num = |s: &str| s.trim().parse::<f64>().ok();
        let rows: Vec<Json> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let f: Vec<&str> = l.split(',').collect();
                json!({
                    "index": f.first().and_then(|s| s.trim().parse::<u32>().ok()),
                    "name": f.get(1).map(|s| s.trim()),
                    "power_w": f.get(2).and_then(|s| num(s)),
                    "pstate": f.get(3).map(|s| s.trim()),
                    "graphics_mhz": f.get(4).and_then(|s| num(s)),
                })
            })
            .collect();
        (!rows.is_empty()).then_some(Json::Array(rows))
    };
    if !*PRESENT.get_or_init(|| run().is_some()) {
        return None;
    }
    run()
}

/// The `Energy Meter` performance counters (RAPL package, core and DRAM power), probed once.
struct EnergyMeter {
    query: windows::Win32::System::Performance::PDH_HQUERY,
    counter: windows::Win32::System::Performance::PDH_HCOUNTER,
}

// SAFETY: PDH handles are process-wide and PDH serialises calls internally; access goes through a
// Mutex here anyway.
unsafe impl Send for EnergyMeter {}

fn energy_meter() -> &'static Mutex<Option<EnergyMeter>> {
    static M: OnceLock<Mutex<Option<EnergyMeter>>> = OnceLock::new();
    M.get_or_init(|| {
        use windows::Win32::System::Performance::{
            PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhOpenQueryW, PDH_HCOUNTER,
            PDH_HQUERY,
        };
        let mut query = PDH_HQUERY::default();
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
            return Mutex::new(None);
        }
        let mut counter = PDH_HCOUNTER::default();
        let st = unsafe {
            PdhAddEnglishCounterW(query, w!("\\Energy Meter(*)\\Power"), 0, &mut counter)
        };
        if st != 0 || unsafe { PdhCollectQueryData(query) } != 0 {
            unsafe {
                let _ = PdhCloseQuery(query);
            }
            return Mutex::new(None);
        }
        // `Power` is a rate: the first value after opening reads 0, so prime it
        std::thread::sleep(std::time::Duration::from_secs(1));
        if unsafe { PdhCollectQueryData(query) } != 0 {
            unsafe {
                let _ = PdhCloseQuery(query);
            }
            return Mutex::new(None);
        }
        Mutex::new(Some(EnergyMeter { query, counter }))
    })
}

/// `{instance: milliwatts}` from `\Energy Meter(*)\Power`.
pub fn package_power() -> Option<Json> {
    use windows::Win32::System::Performance::{
        PdhCollectQueryData, PdhGetFormattedCounterArrayW, PDH_FMT_COUNTERVALUE_ITEM_W,
        PDH_FMT_DOUBLE,
    };
    const PDH_MORE_DATA: u32 = 0x8000_07D2;
    let guard = energy_meter().lock().unwrap_or_else(|e| e.into_inner());
    let em = guard.as_ref()?;
    if unsafe { PdhCollectQueryData(em.query) } != 0 {
        return None;
    }
    let (mut size, mut count) = (0u32, 0u32);
    let st = unsafe {
        PdhGetFormattedCounterArrayW(em.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None)
    };
    if st != PDH_MORE_DATA || size == 0 {
        return None;
    }
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
    let st = unsafe {
        PdhGetFormattedCounterArrayW(
            em.counter,
            PDH_FMT_DOUBLE,
            &mut size,
            &mut count,
            Some(items),
        )
    };
    if st != 0 {
        return None;
    }
    let mut out = serde_json::Map::new();
    for i in 0..count as usize {
        let it = unsafe { &*items.add(i) };
        let name = unsafe { it.szName.to_string() }.unwrap_or_default();
        let v = (it.FmtValue.CStatus == 0).then_some(unsafe { it.FmtValue.Anonymous.doubleValue });
        out.insert(name, json!(v));
    }
    Some(Json::Object(out))
}

/// The machine side of a sample: power, timer resolution, the primary display's mode.
pub fn machine() -> Json {
    let (ac, battery) = ac_and_battery();
    json!({
        "ac_line": ac,
        "battery_pct": battery,
        "gpu_power": nvidia_smi(),
        "package_power_mw": package_power(),
        "timer_resolution_ms": timer_resolution().map(|t| t["current_ms"].clone()),
        "primary_display": primary_display_mode(),
    })
}

/// One series row: the macOS keys (`t_s`, `rss_mb`, `rss_peak_mb`, `thermal`, `low_power`, the
/// last two always `null` here) and then every counter above.
pub fn sample(t0: Instant) -> Json {
    let mut v = json!({
        "t_s": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        "thermal": Json::Null,
        "low_power": Json::Null,
    });
    for part in [resources(), machine()] {
        if let (Some(dst), Json::Object(src)) = (v.as_object_mut(), part) {
            dst.extend(src);
        }
    }
    v
}

/// Which counters this machine has, and why the others are absent. Recorded once by `env`.
pub fn probe() -> Json {
    let has = |present: bool, why: &str| -> Json {
        if present {
            json!({ "present": true })
        } else {
            json!({ "present": false, "why": why })
        }
    };
    let (ac, _) = ac_and_battery();
    let (threads, host) = process_list_counters();
    json!({
        "process_memory": has(memory().is_some(), "GetProcessMemoryInfo failed"),
        "handles": has(handle_count().is_some(), "GetProcessHandleCount failed"),
        "gui_objects": has(true, ""),
        "threads": has(threads.is_some(), "NtQuerySystemInformation process list unreadable"),
        "udp_sockets": has(udp_sockets().is_some(), "GetExtendedUdpTable failed"),
        "tcp_sockets": has(tcp_sockets().is_some(), "GetExtendedTcpTable failed"),
        "gpu_mem": has(gpu_memory().is_some(), "no DXGI factory"),
        "driver_host": has(host.is_some(), match driver_pipe_server() {
            driver_health::PipeProbe::Absent => "the display driver's pipe does not exist (driver not installed or not running)",
            _ => "no WUDFHost.exe found",
        }),
        "gpu_power": has(nvidia_smi().is_some(), "nvidia-smi is not on PATH or did not answer"),
        "package_power": has(package_power().is_some() || energy_meter().lock().map(|g| g.is_some()).unwrap_or(false),
            "no \\Energy Meter performance counters on this machine (CPU package power needs a driver otherwise)"),
        "ac_line": has(ac.is_some(), "GetSystemPowerStatus reports the AC state as unknown"),
        "timer_resolution": has(timer_resolution().is_some(), "NtQueryTimerResolution not exported or failed"),
        "primary_display": has(primary_display_mode().is_some(), "no display device carries the primary flag"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_basic_process_counters_read_on_any_windows() {
        let (ws, peak) = rss();
        assert!(ws > 0 && peak >= ws);
        assert!(handle_count().unwrap() > 0);
        assert!(!hostname().is_empty());
        assert_eq!(local_stamp().len(), 15);
        assert!(free_disk_bytes(Path::new(".")).unwrap() > 0);
        let r = resources();
        assert!(r["threads"].as_u64().unwrap() >= 1, "{r}");
        assert!(r["udp_sockets"].is_u64(), "{r}");
        let s = sample(Instant::now());
        assert!(s["t_s"].is_number() && s["thermal"].is_null(), "{s}");
    }

    #[test]
    fn a_socket_this_process_opens_is_counted() {
        // counted by its own port, so sockets other tests open meanwhile do not move the answer
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        assert_eq!(udp_sockets_on(Some(port)), Some(1));
        assert!(udp_sockets().unwrap() >= 1);
        drop(sock);
        assert_eq!(udp_sockets_on(Some(port)), Some(0));
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert_eq!(tcp_sockets_on(Some(port)), Some(1));
        assert!(tcp_sockets().unwrap() >= 1);
        drop(l);
        assert_eq!(tcp_sockets_on(Some(port)), Some(0));
    }

    #[test]
    fn a_display_mode_change_of_size_invalidates_and_refresh_alone_does_not() {
        let m = |w, h, hz| DisplayMode {
            device: "\\\\.\\DISPLAY1".into(),
            width: w,
            height: h,
            refresh_hz: hz,
        };
        let a = m(1920, 1080, 360);
        let same = display_mode_check(Some(&a), Some(&a));
        assert_eq!(same["valid"], json!(true));
        let hz = display_mode_check(Some(&a), Some(&m(1920, 1080, 60)));
        assert_eq!(hz["valid"], json!(true));
        assert_eq!(hz["refresh_changed"], json!(true));
        let size = display_mode_check(Some(&a), Some(&m(1280, 720, 360)));
        assert_eq!(size["valid"], json!(false));
        assert_eq!(display_mode_check(Some(&a), None)["valid"], json!(false));
        assert_eq!(display_mode_check(None, None)["valid"], json!(true));
    }
}

//! The display arrangement on Windows, through CCD (`QueryDisplayConfig` / `SetDisplayConfig`).
//!
//! Only `sourceMode.position` is ever written. Every other field of the queried paths and modes
//! goes back exactly as it was read, and `SDC_ALLOW_CHANGES` is never passed, so Windows may
//! refuse a layout but may not change a mode to make it fit.

use std::sync::Mutex;
use std::time::Duration;

use windows::Win32::Devices::Display::{
    GetDisplayConfigBufferSizes, QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO, QDC_ONLY_ACTIVE_PATHS,
    QDC_VIRTUAL_MODE_AWARE, QDC_VIRTUAL_REFRESH_RATE_AWARE, QUERY_DISPLAY_CONFIG_FLAGS, SDC_APPLY,
    SDC_SAVE_TO_DATABASE, SDC_USE_SUPPLIED_DISPLAY_CONFIG, SDC_VALIDATE, SDC_VIRTUAL_MODE_AWARE,
    SDC_VIRTUAL_REFRESH_RATE_AWARE, SET_DISPLAY_CONFIG_FLAGS,
};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    ERROR_SUCCESS, POINTL, WIN32_ERROR,
};

use super::virtual_display::{is_owned_path, target_name, wide_to_string};
use crate::display_layout::{ArrangedDisplay, DisplayArranger, DisplayLayout, DisplayPlacement};

const PATH_ACTIVE: u32 = 0x1;
const PATH_SUPPORT_VIRTUAL_MODE: u32 = 0x8;
const IDX16_INVALID: u32 = 0xFFFF;
const IDX32_INVALID: u32 = 0xFFFF_FFFF;

pub struct WindowsArranger;

impl DisplayArranger for WindowsArranger {
    fn read() -> Result<DisplayLayout, String> {
        read_layout()
    }

    fn apply(placements: &[DisplayPlacement]) -> Result<DisplayLayout, String> {
        apply_layout(placements)
    }
}

/// The source-mode index a path's source or target union holds.
///
/// With `DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE` set the union is two 16-bit halves, low first
/// (`cloneGroupId`/`desktopModeInfoIdx`, then the mode index); otherwise it is one 32-bit index.
fn decode_mode_index(path_flags: u32, raw: u32, modes: usize) -> Option<usize> {
    let (idx, invalid) = if path_flags & PATH_SUPPORT_VIRTUAL_MODE != 0 {
        (raw >> 16, IDX16_INVALID)
    } else {
        (raw, IDX32_INVALID)
    };
    (idx != invalid)
        .then_some(idx as usize)
        .filter(|&i| i < modes)
}

/// Whether this machine accepts the refresh-rate-aware query, asked once.
fn refresh_rate_aware() -> bool {
    static PROBED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PROBED.get_or_init(|| {
        let (mut np, mut nm) = (0u32, 0u32);
        let flags = QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE | QDC_VIRTUAL_REFRESH_RATE_AWARE;
        let rc = unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) };
        rc == ERROR_SUCCESS
    })
}

struct Ccd {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
    refresh_aware: bool,
}

fn query(refresh_aware: bool) -> Result<Ccd, WIN32_ERROR> {
    let mut flags: QUERY_DISPLAY_CONFIG_FLAGS = QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE;
    if refresh_aware {
        flags |= QDC_VIRTUAL_REFRESH_RATE_AWARE;
    }
    for _ in 0..8 {
        let (mut np, mut nm) = (0u32, 0u32);
        let sized = unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) };
        if sized != ERROR_SUCCESS {
            return Err(sized);
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        let rc = unsafe {
            QueryDisplayConfig(
                flags,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                modes.as_mut_ptr(),
                None,
            )
        };
        if rc == ERROR_INSUFFICIENT_BUFFER {
            // the topology changed between the two calls
            continue;
        }
        if rc != ERROR_SUCCESS {
            return Err(rc);
        }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        return Ok(Ccd {
            paths,
            modes,
            refresh_aware,
        });
    }
    Err(ERROR_INSUFFICIENT_BUFFER)
}

/// One desktop region: every active path sharing a source mode.
struct Source {
    mode: usize,
    /// Device paths of the monitors showing it, sorted; the first is the region's id.
    device_paths: Vec<String>,
    name: String,
    /// A laptop panel or another display wired inside the machine.
    built_in: bool,
    clone_group: Option<u32>,
    refresh_hz: Option<f64>,
}

impl Ccd {
    fn source_mode(&self, p: &DISPLAYCONFIG_PATH_INFO) -> Option<usize> {
        let raw = unsafe { p.sourceInfo.Anonymous.modeInfoIdx };
        let i = decode_mode_index(p.flags, raw, self.modes.len())?;
        (self.modes[i].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE).then_some(i)
    }

    fn position(&self, mode: usize) -> POINTL {
        unsafe { self.modes[mode].Anonymous.sourceMode.position }
    }

    fn size(&self, mode: usize) -> (u32, u32) {
        let m = unsafe { self.modes[mode].Anonymous.sourceMode };
        (m.width, m.height)
    }

    fn sources(&self) -> Vec<Source> {
        let mut out: Vec<Source> = Vec::new();
        for p in self.paths.iter().filter(|p| p.flags & PATH_ACTIVE != 0) {
            let Some(mode) = self.source_mode(p) else {
                continue;
            };
            let (friendly, device_path) =
                target_name(p.targetInfo.adapterId, p.targetInfo.id).unwrap_or_default();
            let built_in = is_built_in(p.targetInfo.outputTechnology);
            let rate = p.targetInfo.refreshRate;
            let refresh_hz = (rate.Denominator != 0 && rate.Numerator != 0)
                .then(|| f64::from(rate.Numerator) / f64::from(rate.Denominator));
            match out.iter_mut().find(|s| s.mode == mode) {
                Some(s) => {
                    s.device_paths.push(device_path);
                    if s.name.is_empty() {
                        s.name = friendly;
                    }
                    s.built_in |= built_in;
                    s.refresh_hz = s.refresh_hz.or(refresh_hz);
                }
                None => out.push(Source {
                    mode,
                    device_paths: vec![device_path],
                    name: friendly,
                    built_in,
                    clone_group: None,
                    refresh_hz,
                }),
            }
        }
        let mut group = 0u32;
        for s in &mut out {
            s.device_paths.sort();
            if s.device_paths.len() > 1 {
                s.clone_group = Some(group);
                group += 1;
            }
        }
        out
    }

    fn layout(&self) -> DisplayLayout {
        let displays = self
            .sources()
            .into_iter()
            .enumerate()
            .map(|(n, s)| {
                let pos = self.position(s.mode);
                let (width, height) = self.size(s.mode);
                let id = s.device_paths.first().cloned().unwrap_or_default();
                ArrangedDisplay {
                    ours: s.device_paths.iter().any(|d| is_owned_path(d)),
                    // a laptop panel usually reports no EDID name
                    name: match (s.name.is_empty(), s.built_in) {
                        (false, _) => s.name,
                        (true, true) => "Built-in display".to_string(),
                        (true, false) => format!("Display {}", n + 1),
                    },
                    id,
                    x: pos.x,
                    y: pos.y,
                    width,
                    height,
                    primary: pos.x == 0 && pos.y == 0,
                    mirror_group: s.clone_group,
                    refresh_hz: s.refresh_hz,
                    // the command layer links ours to their joins
                    join_id: None,
                }
            })
            .collect();
        let remote = is_remote_session();
        DisplayLayout {
            displays,
            units: "pixels".to_string(),
            can_apply: !remote,
            reason: remote.then(|| REMOTE_REASON.to_string()),
        }
    }
}

fn is_built_in(
    tech: windows::Win32::Devices::Display::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
) -> bool {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
    };
    [
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
    ]
    .contains(&tech)
}

const REMOTE_REASON: &str =
    "Displays cannot be rearranged from a Remote Desktop session; use the machine's own console.";

fn is_remote_session() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

fn refusal(code: WIN32_ERROR) -> Option<&'static str> {
    if code == ERROR_ACCESS_DENIED {
        Some(REMOTE_REASON)
    } else if code == ERROR_NOT_SUPPORTED {
        Some("This display driver does not let applications arrange displays.")
    } else {
        None
    }
}

fn read_layout() -> Result<DisplayLayout, String> {
    match query(refresh_rate_aware()) {
        Ok(ccd) => Ok(ccd.layout()),
        Err(code) => match refusal(code) {
            // read-only: what the desktop looks like, without the means to change it
            Some(reason) => Ok(DisplayLayout {
                displays: gdi_layout(),
                units: "pixels".to_string(),
                can_apply: false,
                reason: Some(reason.to_string()),
            }),
            None => Err(format!(
                "QueryDisplayConfig failed (win32 error {})",
                code.0
            )),
        },
    }
}

/// The desktop as GDI sees it: names are `\\.\DISPLAYn` slots, so ids here are not stable.
pub(crate) fn gdi_layout() -> Vec<ArrangedDisplay> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
    };
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> BOOL {
        let out = unsafe { &mut *(out.0 as *mut Vec<ArrangedDisplay>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(m, &mut info.monitorInfo) }.as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let name = wide_to_string(&info.szDevice);
            out.push(ArrangedDisplay {
                id: name.clone(),
                name,
                x: r.left,
                y: r.top,
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                primary: r.left == 0 && r.top == 0,
                ours: false,
                mirror_group: None,
                refresh_hz: None,
                join_id: None,
            });
        }
        true.into()
    }
    let mut out: Vec<ArrangedDisplay> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(each),
            LPARAM(&mut out as *mut Vec<ArrangedDisplay> as isize),
        );
    }
    out
}

/// The last layout read or applied, so the watcher reports only real changes and never the
/// echo of our own apply.
static LAST_SEEN: Mutex<Option<DisplayLayout>> = Mutex::new(None);

fn remember(layout: &DisplayLayout) {
    *LAST_SEEN.lock().unwrap_or_else(|e| e.into_inner()) = Some(layout.clone());
}

fn apply_layout(placements: &[DisplayPlacement]) -> Result<DisplayLayout, String> {
    if is_remote_session() {
        return Err(REMOTE_REASON.to_string());
    }
    let mut refresh_aware = refresh_rate_aware();
    loop {
        let mut ccd = query(refresh_aware).map_err(|code| {
            refusal(code)
                .map(str::to_string)
                .unwrap_or_else(|| format!("QueryDisplayConfig failed (win32 error {})", code.0))
        })?;
        let sources = ccd.sources();
        let rects: Vec<Rect> = sources
            .iter()
            .map(|s| {
                let p = ccd.position(s.mode);
                let (w, h) = ccd.size(s.mode);
                Rect {
                    x: p.x,
                    y: p.y,
                    w: w as i32,
                    h: h as i32,
                }
            })
            .collect();
        let mut wanted = rects.clone();
        let mut moved = vec![false; rects.len()];
        for pl in placements {
            let Some(i) = sources.iter().position(|s| s.device_paths.contains(&pl.id)) else {
                return Err(format!(
                    "no connected display has the id {:?}; read the layout again",
                    pl.id
                ));
            };
            wanted[i].x = pl.x;
            wanted[i].y = pl.y;
            moved[i] = true;
        }
        let Some(primary) = rects.iter().position(|r| r.x == 0 && r.y == 0) else {
            return Err("no display is at the desktop origin".to_string());
        };
        let target = arrange(&wanted, &moved, primary)?;
        if target == rects {
            let layout = ccd.layout();
            remember(&layout);
            return Ok(layout);
        }
        for (s, r) in sources.iter().zip(&target) {
            // writing one union field is safe; only reads need the type checked
            ccd.modes[s.mode].Anonymous.sourceMode.position = POINTL { x: r.x, y: r.y };
        }
        let mut base: SET_DISPLAY_CONFIG_FLAGS =
            SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VIRTUAL_MODE_AWARE;
        if ccd.refresh_aware {
            base |= SDC_VIRTUAL_REFRESH_RATE_AWARE;
        }
        let validated =
            unsafe { SetDisplayConfig(Some(&ccd.paths), Some(&ccd.modes), SDC_VALIDATE | base) };
        if validated == ERROR_INVALID_PARAMETER.0 as i32 && ccd.refresh_aware {
            // the query took the flag but the set does not: ask again without it
            refresh_aware = false;
            continue;
        }
        if validated != 0 {
            return Err(set_error("Windows refused this arrangement", validated));
        }
        // our own apply arrives back as a display change: record what it will look like first
        remember(&ccd.layout());
        let applied = unsafe {
            SetDisplayConfig(
                Some(&ccd.paths),
                Some(&ccd.modes),
                SDC_APPLY | SDC_SAVE_TO_DATABASE | base,
            )
        };
        if applied != 0 {
            return Err(set_error(
                "Windows could not apply this arrangement",
                applied,
            ));
        }
        let layout = read_layout()?;
        remember(&layout);
        return Ok(layout);
    }
}

fn set_error(what: &str, code: i32) -> String {
    match refusal(WIN32_ERROR(code as u32)) {
        Some(reason) => reason.to_string(),
        None => format!("{what} (win32 error {code})"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    fn overlaps(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }

    /// Shares at least one pixel of edge with `o`, without overlapping it.
    fn touches(&self, o: &Rect) -> bool {
        if self.overlaps(o) {
            return false;
        }
        let x_run = self.x < o.x + o.w && o.x < self.x + self.w;
        let y_run = self.y < o.y + o.h && o.y < self.y + self.h;
        ((self.x + self.w == o.x || o.x + o.w == self.x) && y_run)
            || ((self.y + self.h == o.y || o.y + o.h == self.y) && x_run)
    }
}

/// The layout Windows is given: the primary at (0, 0), every display touching the rest, none
/// overlapping, each as close as possible to where it was asked to go.
///
/// Displays that were not moved are placed first, nearest the primary first, so a layout the
/// user did not touch stays as it was; then the moved ones, in request order.
pub(crate) fn arrange(
    wanted: &[Rect],
    moved: &[bool],
    primary: usize,
) -> Result<Vec<Rect>, String> {
    let n = wanted.len();
    if n == 0 || primary >= n {
        return Err("no displays to arrange".to_string());
    }
    if wanted.iter().any(|r| r.w <= 0 || r.h <= 0) {
        return Err("a display has no size".to_string());
    }
    let (ox, oy) = (wanted[primary].x, wanted[primary].y);
    let shifted: Vec<Rect> = wanted
        .iter()
        .map(|r| Rect {
            x: r.x - ox,
            y: r.y - oy,
            ..*r
        })
        .collect();
    let mut order: Vec<usize> = (0..n).filter(|&i| i != primary && !moved[i]).collect();
    order.sort_by_key(|&i| {
        let r = shifted[i];
        (r.x as i64).pow(2) + (r.y as i64).pow(2)
    });
    order.extend((0..n).filter(|&i| i != primary && moved[i]));

    let mut placed: Vec<Option<Rect>> = vec![None; n];
    placed[primary] = Some(shifted[primary]);
    for i in order {
        let here: Vec<Rect> = placed.iter().flatten().copied().collect();
        placed[i] = Some(snap(shifted[i], &here));
    }
    let out: Vec<Rect> = placed.into_iter().flatten().collect();
    for (a, ra) in out.iter().enumerate() {
        if out.iter().skip(a + 1).any(|rb| ra.overlaps(rb)) {
            return Err("the arrangement still overlaps after snapping".to_string());
        }
    }
    Ok(out)
}

/// `r` itself when it already touches `placed` without overlapping, otherwise the nearest
/// position against one of the placed displays' edges that overlaps none of them.
fn snap(r: Rect, placed: &[Rect]) -> Rect {
    if !placed.iter().any(|p| p.overlaps(&r)) && placed.iter().any(|p| p.touches(&r)) {
        return r;
    }
    let mut best: Option<(i64, Rect)> = None;
    for p in placed {
        // along a vertical edge the y range keeps at least one pixel of contact; likewise x
        let y = r.y.clamp(p.y - r.h + 1, p.y + p.h - 1);
        let x = r.x.clamp(p.x - r.w + 1, p.x + p.w - 1);
        let candidates = [
            Rect {
                x: p.x + p.w,
                y,
                ..r
            },
            Rect {
                x: p.x - r.w,
                y,
                ..r
            },
            Rect {
                x,
                y: p.y + p.h,
                ..r
            },
            Rect {
                x,
                y: p.y - r.h,
                ..r
            },
        ];
        for c in candidates {
            if placed.iter().any(|q| q.overlaps(&c)) {
                continue;
            }
            let d = (c.x as i64 - r.x as i64).pow(2) + (c.y as i64 - r.y as i64).pow(2);
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, c));
            }
        }
    }
    // every edge blocked: beyond the right of everything, top aligned with the primary
    best.map(|(_, c)| c).unwrap_or_else(|| Rect {
        x: placed.iter().map(|p| p.x + p.w).max().unwrap_or(0),
        y: placed.first().map_or(0, |p| p.y),
        ..r
    })
}

/// A running display-change watcher; dropping it stops the thread.
pub struct DisplayWatcher {
    hwnd: isize,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for DisplayWatcher {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const DEBOUNCE: Duration = Duration::from_millis(300);
const TIMER_ID: usize = 1;

type OnChange = Box<dyn Fn(DisplayLayout) + Send>;

/// Calls `on_change` with the new layout after any display change that moved something, at
/// most once per burst of system notifications. Changes this module applied itself are not
/// reported: the caller already has that layout from [`DisplayArranger::apply`].
///
/// Runs its own hidden top-level window on its own thread: message-only windows do not get the
/// display broadcasts. Nothing here initializes COM.
pub fn watch(on_change: impl Fn(DisplayLayout) + Send + 'static) -> Result<DisplayWatcher, String> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<isize, String>>();
    let callback: OnChange = Box::new(on_change);
    let thread = std::thread::Builder::new()
        .name("display-layout-watcher".into())
        .spawn(move || run_watcher(callback, tx))
        .map_err(|e| format!("could not start the display watcher thread: {e}"))?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(hwnd)) => Ok(DisplayWatcher {
            hwnd,
            thread: Some(thread),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the display watcher window did not start".to_string()),
    }
}

fn run_watcher(callback: OnChange, ready: std::sync::mpsc::Sender<Result<isize, String>>) {
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, MSG,
        WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPED,
    };
    let class = w!("ScreenExtendDisplayLayoutWatcher");
    let instance = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(format!("GetModuleHandleW failed: {e}")));
            return;
        }
    };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(watcher_proc),
        hInstance: instance.into(),
        lpszClassName: class,
        ..Default::default()
    };
    // a second watcher finds the class already registered, which is fine
    unsafe { RegisterClassW(&wc) };
    let state = Box::into_raw(Box::new(callback));
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            Some(state as *const _),
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            drop(unsafe { Box::from_raw(state) });
            let _ = ready.send(Err(format!("the display watcher window failed: {e}")));
            return;
        }
    };
    let _ = ready.send(Ok(hwnd.0 as isize));
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn watcher_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, GetWindowLongPtrW, KillTimer, PostQuitMessage, SetTimer, SetWindowLongPtrW,
        CREATESTRUCTW, GWLP_USERDATA, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_NCCREATE,
        WM_NCDESTROY, WM_SETTINGCHANGE, WM_TIMER,
    };
    let handled = std::panic::catch_unwind(|| unsafe {
        match msg {
            WM_NCCREATE => {
                let cs = &*(lp.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                None
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
                // restarting the timer on every message coalesces a burst into one read
                SetTimer(Some(hwnd), TIMER_ID, DEBOUNCE.as_millis() as u32, None);
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TIMER_ID => {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const OnChange;
                if let (Some(cb), Ok(layout)) = (state.as_ref(), read_layout()) {
                    if changed(&layout) {
                        cb(layout);
                    }
                }
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                Some(LRESULT(0))
            }
            WM_NCDESTROY => {
                let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut OnChange;
                if !p.is_null() {
                    drop(Box::from_raw(p));
                }
                None
            }
            _ => None,
        }
    });
    match handled {
        Ok(Some(r)) => r,
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Whether `layout`'s geometry differs from the last one read or applied; records it.
fn changed(layout: &DisplayLayout) -> bool {
    let mut last = LAST_SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let same = last.as_ref().is_some_and(|l| same_geometry(l, layout));
    *last = Some(layout.clone());
    !same
}

fn same_geometry(a: &DisplayLayout, b: &DisplayLayout) -> bool {
    let key = |l: &DisplayLayout| {
        let mut v: Vec<_> = l
            .displays
            .iter()
            .map(|d| (d.id.clone(), d.x, d.y, d.width, d.height, d.mirror_group))
            .collect();
        v.sort();
        v
    };
    key(a) == key(b) && a.can_apply == b.can_apply
}

#[cfg(test)]
#[path = "test/display_layout.rs"]
mod tests;

/// The name the shared command layer reaches every OS by.
pub type Arranger = WindowsArranger;

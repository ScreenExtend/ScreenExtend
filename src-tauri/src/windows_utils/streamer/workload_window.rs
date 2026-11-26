//! A borderless window that paints a scripted workload onto a real display.
//!
//! `--synthetic-pattern` feeds the generated buffer straight to the encoder, which leaves the
//! `capture_delivery` and `scale` histograms empty, and `--live` cannot be scripted or repeated.
//! Painting the same generated frames in a top-most window lets the real capture backend pick
//! them up: repeatable content, measured from `capture_delivery` onwards.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetDC, GetMonitorInfoW, MonitorFromPoint, ReleaseDC, StretchDIBits,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
    PostQuitMessage, RegisterClassW, SetLayeredWindowAttributes, SetWindowPos, ShowWindow,
    TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG, PM_REMOVE, SWP_NOACTIVATE, SW_SHOWNOACTIVATE,
    WM_DESTROY, WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};

use crate::streamer::synthetic::{self, Workload};

const CLASS_NAME: PCWSTR = w!("ScreenExtendWorkload");

/// A developer hook (honoured only with `SE_TEST_HOOKS=1`) that lets the window cover the
/// primary display, for a harness run on a machine nobody is using.
const ALLOW_PRIMARY_VAR: &str = "SE_WORKLOAD_ALLOW_PRIMARY";

/// Whether two `(left, top, right, bottom)` rectangles share any pixel. Right and bottom are
/// exclusive, as in a Win32 `RECT`.
pub(crate) fn rects_overlap(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// The primary monitor's rectangle in desktop coordinates. The primary display always has its
/// top-left corner at the desktop origin, so `(0, 0)` finds it.
fn primary_monitor_rect() -> Option<(i32, i32, i32, i32)> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
    }
    let r = info.rcMonitor;
    Some((r.left, r.top, r.right, r.bottom))
}

/// A running workload window. Dropping it closes the window and joins its thread.
pub struct WorkloadWindow {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl WorkloadWindow {
    /// Cover `(left, top, width, height)`, in desktop coordinates, with a window painting
    /// `workload` at `fps`.
    ///
    /// The window gets its own thread: a Win32 window belongs to the thread that created it and
    /// needs that thread's message loop, and the probe's thread is busy draining encoded frames.
    pub fn spawn(
        left: i32,
        top: i32,
        width: u32,
        height: u32,
        fps: u32,
        workload: Workload,
        counter: bool,
    ) -> Result<Self> {
        if width < 2 || height < 2 {
            return Err(anyhow!("workload window needs a real display rect"));
        }
        // Never cover the user's own screen unasked: a missing or mistyped `--monitor` makes
        // `select_monitor` fall back to display 0. An unreadable primary rect is refused too.
        let rect = (
            left,
            top,
            left.saturating_add(width.min(i32::MAX as u32) as i32),
            top.saturating_add(height.min(i32::MAX as u32) as i32),
        );
        let on_primary = primary_monitor_rect().is_none_or(|p| rects_overlap(rect, p));
        if on_primary && super::tune::hook_var(ALLOW_PRIMARY_VAR).is_none() {
            return Err(anyhow!(
                "refusing to cover the primary display with the workload window \
                 ({width}x{height} at ({left},{top})); point it at a virtual display, or set \
                 {ALLOW_PRIMARY_VAR}=1 with SE_TEST_HOOKS=1 on a machine nobody is using"
            ));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let join = std::thread::Builder::new()
            .name("workload-window".into())
            .spawn(move || {
                let report = ready_tx.clone();
                if let Err(e) = run_window(
                    left,
                    top,
                    width,
                    height,
                    fps,
                    workload,
                    counter,
                    &thread_stop,
                    ready_tx,
                ) {
                    let _ = report.send(Err(format!("{e:#}")));
                }
            })?;
        // the window has to exist before capture starts, or the first frames measure an empty
        // desktop
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(anyhow!("workload window: {e}")),
            Err(_) => return Err(anyhow!("workload window did not start within 5 s")),
        }
        Ok(Self {
            stop,
            join: Some(join),
        })
    }
}

impl Drop for WorkloadWindow {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // the render loop paints; answering the paint request keeps Windows from asking again
        // on every iteration
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Destroys the window on drop, however the render loop exits.
struct Window(HWND);

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_window(
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    fps: u32,
    workload: Workload,
    counter: bool,
    stop: &AtomicBool,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        // the `RegisterClassW` result is ignored: the class is process-wide, so a second probe
        // run in this process gets ERROR_CLASS_ALREADY_EXISTS
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassW(&class);

        // `WS_EX_LAYERED | WS_EX_TRANSPARENT` makes it click-through: input goes to whatever is
        // underneath
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            CLASS_NAME,
            w!("ScreenExtend workload"),
            WS_POPUP | WS_VISIBLE,
            left,
            top,
            width as i32,
            height as i32,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let window = Window(hwnd);
        // a layered window shows nothing until it is given its attributes; fully opaque, so the
        // pixels captured are exactly the pixels painted
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        // `WS_EX_NOACTIVATE` keeps focus where it was; this keeps the window in front of
        // whatever already had it
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            left,
            top,
            width as i32,
            height as i32,
            SWP_NOACTIVATE,
        );
        let _ = ready.send(Ok(()));

        let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
        // negative height: a top-down DIB, matching the row order `synthetic::fill` writes and
        // the order the capture backends hand the encoders
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let interval = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
        let started = Instant::now();
        let mut frame = 0u32;
        let mut next = Instant::now();
        let mut msg = MSG::default();
        while !stop.load(Ordering::Relaxed) {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let now = Instant::now();
            if now < next {
                std::thread::sleep((next - now).min(interval));
                continue;
            }
            next = now + interval;

            synthetic::fill(&mut buf, width, height, frame, workload);
            if counter {
                synthetic::draw_counter(
                    &mut buf,
                    width,
                    height,
                    frame,
                    started.elapsed().as_millis() as u64,
                );
            }
            frame = frame.wrapping_add(1);

            let hdc = GetDC(Some(hwnd));
            if !hdc.is_invalid() {
                StretchDIBits(
                    hdc,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    Some(buf.as_ptr() as *const _),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
                ReleaseDC(Some(hwnd), hdc);
            }
        }
        drop(window);
    }
    Ok(())
}

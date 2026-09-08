//! A borderless window that paints a scripted workload onto a real display.
//!
//! `--workload` implies `--synthetic-pattern`, which feeds the generated buffer straight to the
//! encoder: no `Duplicator`, no WGC frame pool, no GPU texture, no scaler. That measures encode,
//! packetize and socket, and by construction leaves the `capture_delivery` and `scale`
//! histograms empty — so the four workloads, whose whole point is that different content
//! stresses *capture* and encode differently, only ever exercised the encode half.
//!
//! `--live` captures the real desktop, which does go through the whole path but takes whatever
//! happens to be on screen and cannot be scripted or repeated.
//!
//! This closes the gap the cheap way: put the same generated frames on the actual display, in a
//! borderless top-most window covering it, and let the real capture backend pick them up. The
//! content stays repeatable, and every stage from `capture_delivery` onwards is now measured.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetDC, ReleaseDC, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
    PostQuitMessage, RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, MSG,
    PM_REMOVE, SWP_NOACTIVATE, SW_SHOWNOACTIVATE, WM_DESTROY, WM_PAINT, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

use crate::streamer::synthetic::{self, Workload};

const CLASS_NAME: PCWSTR = w!("ScreenExtendWorkload");

/// A running workload window. Dropping it closes the window and joins its thread.
pub struct WorkloadWindow {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl WorkloadWindow {
    /// Cover `(left, top, width, height)` — desktop coordinates — with a window painting
    /// `workload` at `fps`.
    ///
    /// The window is created on its own thread because a Win32 window belongs to the thread that
    /// created it and needs that thread's message loop; the probe's own thread is busy draining
    /// encoded frames.
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
        // The window has to exist before capture starts, or the first frames measure an empty
        // desktop and the histograms open with a transient nobody asked for.
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
        // Painting happens from the render loop, which owns the frame counter. Answering the
        // paint request keeps Windows from asking again on every iteration.
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

/// Marks the window's lifetime; dropping it is what the render loop's exit does.
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
        // Registering twice returns ERROR_CLASS_ALREADY_EXISTS, which is fine — the class is
        // process-wide and a second probe run in the same process would hit it.
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassW(&class);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
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
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        // `WS_EX_NOACTIVATE` keeps focus where it was; this keeps the window in front of
        // whatever already had it.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            left,
            top,
            width as i32,
            height as i32,
            SWP_NOACTIVATE,
        );
        // The window exists and is on screen; capture can start.
        let _ = ready.send(Ok(()));

        let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
        // Negative height: a top-down DIB, matching the row order `synthetic::fill` writes and
        // the order the capture backends hand the encoders.
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

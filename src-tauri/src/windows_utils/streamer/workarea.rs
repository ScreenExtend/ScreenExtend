//! When the Windows shell has finished taking a new monitor in.
//!
//! A monitor this host has just attached refuses capture items until the shell commits that
//! monitor's work area, which it announces to every top-level window as `WM_SETTINGCHANGE` with
//! `SPI_SETWORKAREA`. Measured, the item becomes creatable 0.15 to 1.05 s after that broadcast, and
//! the shell takes from under 2 s to over 14 s to send it. The broadcast goes only to top-level
//! windows, so this keeps one, hidden, on a thread of its own that does nothing but pump.

use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    TranslateMessage, MSG, WM_SETTINGCHANGE, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_POPUP,
};

const CLASS_NAME: PCWSTR = w!("ScreenExtendWorkArea");
/// `SPI_SETWORKAREA`, the `wParam` of the broadcast that commits a work area.
const SPI_SETWORKAREA: usize = 0x002F;

struct Changes {
    seen: Mutex<(u64, Option<Instant>)>,
    changed: Condvar,
}

fn changes() -> &'static Changes {
    static CHANGES: OnceLock<Changes> = OnceLock::new();
    CHANGES.get_or_init(|| Changes {
        seen: Mutex::new((0, None)),
        changed: Condvar::new(),
    })
}

/// Record one work-area broadcast. Split from the window so the waiting side can be tested.
pub(crate) fn note_change() {
    let c = changes();
    let mut seen = c.seen.lock().unwrap_or_else(|p| p.into_inner());
    seen.0 += 1;
    seen.1 = Some(Instant::now());
    c.changed.notify_all();
}

/// How many work-area broadcasts this process has received, starting the listener on first use.
pub fn generation() -> u64 {
    start();
    changes().seen.lock().unwrap_or_else(|p| p.into_inner()).0
}

/// Wait at most `timeout` for a broadcast after `seen`. Returns the generation now and, when one
/// came, when the latest one arrived.
pub fn wait_after(seen: u64, timeout: Duration) -> (u64, Option<Instant>) {
    let c = changes();
    let guard = c.seen.lock().unwrap_or_else(|p| p.into_inner());
    let (guard, _) = c
        .changed
        .wait_timeout_while(guard, timeout, |s| s.0 == seen)
        .unwrap_or_else(|p| p.into_inner());
    if guard.0 == seen {
        (guard.0, None)
    } else {
        (guard.0, guard.1)
    }
}

/// Whether a window message is the shell committing a work area.
pub(crate) fn is_work_area_change(msg: u32, wparam: WPARAM) -> bool {
    msg == WM_SETTINGCHANGE && wparam.0 == SPI_SETWORKAREA
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if is_work_area_change(msg, wparam) {
        note_change();
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Start the listener once for the life of the process. A failure is logged and leaves the
/// waits to the capture-item poll alone, which is still correct, only slower to notice.
fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("se-workarea".into())
            .spawn(|| {
                if let Err(e) = listen() {
                    teprintln!("[display] work-area listener unavailable: {e}");
                }
            });
        if let Err(e) = spawned {
            teprintln!("[display] work-area listener thread failed to start: {e}");
        }
    });
}

fn listen() -> windows::core::Result<()> {
    // SAFETY: plain Win32 window creation and a message loop on the thread that owns the window.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        // top level (a message-only window gets no broadcasts), never shown, never activated
        let _hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            CLASS_NAME,
            w!("ScreenExtend work area"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let mut msg = MSG::default();
        // 0 is WM_QUIT and -1 an error; either ends the loop
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_committed_work_area_counts() {
        assert!(is_work_area_change(
            WM_SETTINGCHANGE,
            WPARAM(SPI_SETWORKAREA)
        ));
        assert!(!is_work_area_change(WM_SETTINGCHANGE, WPARAM(0)));
        assert!(!is_work_area_change(
            WM_SETTINGCHANGE + 1,
            WPARAM(SPI_SETWORKAREA)
        ));
    }

    #[test]
    fn a_broadcast_wakes_a_waiter_with_its_time() {
        let seen = changes().seen.lock().unwrap().0;
        let (now, at) = wait_after(seen, Duration::from_millis(20));
        assert_eq!((now, at), (seen, None), "nothing came");
        let waker = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            note_change();
        });
        let (now, at) = wait_after(seen, Duration::from_secs(5));
        waker.join().unwrap();
        assert!(now > seen && at.is_some());
    }
}

//! RAII guards for the unsafe handles the audio threads hold (PRD §8.6): COM apartment, the
//! WASAPI event handle, and the MMCSS task registration. Each reverts on drop so an early
//! return or `?` can never leak them.

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};

/// Initializes COM MTA on the current thread; uninitializes on drop. Per PRD §4.1 the audio
/// threads run MTA and must not touch the Tauri main thread's apartment.
pub struct ComGuard;

impl ComGuard {
    pub fn init_mta() -> Result<Self> {
        // SAFETY: initializing COM on this thread; balanced by CoUninitialize in Drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
            .context("CoInitializeEx(MTA) on audio thread")?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances the CoInitializeEx in `init_mta`, same thread.
        unsafe { CoUninitialize() };
    }
}

/// Owns a Win32 auto-reset event; closes it on drop.
pub struct EventHandle(HANDLE);

impl EventHandle {
    pub fn new_auto_reset() -> Result<Self> {
        // SAFETY: creating an unnamed auto-reset, initially-unsignaled event.
        let h = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .context("CreateEventW for audio")?;
        Ok(Self(h))
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// Wait up to `timeout_ms`. Returns true if the event was signaled, false on timeout.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: `self.0` is a live event handle owned by this guard.
        let r: WAIT_EVENT = unsafe { WaitForSingleObject(self.0, timeout_ms) };
        r == WAIT_OBJECT_0
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: closing a handle we created and still own.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Registers the current thread with MMCSS "Pro Audio"; reverts on drop. Without this the
/// scheduler hands the capture thread periodic 10 ms+ stalls (PRD §4.1).
pub struct MmcssGuard(HANDLE);

impl MmcssGuard {
    pub fn register_pro_audio() -> Result<Self> {
        let mut task_index: u32 = 0;
        // SAFETY: "Pro Audio" is a valid MMCSS task name; `task_index` receives the index.
        let handle = unsafe {
            AvSetMmThreadCharacteristicsW(PCWSTR::from_raw(PRO_AUDIO.as_ptr()), &mut task_index)
        }
        .context("AvSetMmThreadCharacteristicsW(\"Pro Audio\")")?;
        Ok(Self(handle))
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: reverting the MMCSS registration created above, same thread.
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(self.0);
            }
        }
    }
}

// UTF-16, NUL-terminated "Pro Audio".
const PRO_AUDIO: &[u16] = &[
    b'P' as u16,
    b'r' as u16,
    b'o' as u16,
    b' ' as u16,
    b'A' as u16,
    b'u' as u16,
    b'd' as u16,
    b'i' as u16,
    b'o' as u16,
    0,
];

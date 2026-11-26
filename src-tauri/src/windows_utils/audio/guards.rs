use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::WAVEFORMATEX;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};

/// Owns a `WAVEFORMATEX` that `IAudioClient::GetMixFormat` allocated with `CoTaskMemAlloc`, so
/// an early return cannot leak it (`acquire` retries every 200 ms).
pub struct MixFormatPtr(*mut WAVEFORMATEX);

impl MixFormatPtr {
    /// # Safety
    /// `ptr` must be a live `CoTaskMemAlloc`'d `WAVEFORMATEX` that nothing else will free.
    pub unsafe fn from_raw(ptr: *mut WAVEFORMATEX) -> Self {
        Self(ptr)
    }

    pub fn as_ptr(&self) -> *const WAVEFORMATEX {
        self.0
    }
}

impl Drop for MixFormatPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
            self.0 = std::ptr::null_mut();
        }
    }
}

/// The MTA the WASAPI threads run in.
///
/// Both callers (the MMCSS "Pro Audio" capture thread and the silent render companion) are
/// threads this crate spawned, the one kind that may initialize COM. It still goes through
/// `windows_utils::com`, which asks `CoGetApartmentType` first: on a Tauri thread the guard is
/// inert instead of failing with `RPC_E_CHANGED_MODE` or uninitializing an apartment it never
/// took.
pub struct ComGuard(crate::windows_utils::com::ComScope);

impl ComGuard {
    pub fn init_mta() -> Result<Self> {
        use crate::windows_utils::com::{ensure_for_this_thread, Apartment};
        ensure_for_this_thread("audio", Apartment::Mta)
            .map(Self)
            .context("COM (MTA) on audio thread")
    }
}

pub struct EventHandle(HANDLE);

// SAFETY: a Win32 event handle is a process-wide kernel object reference; `SetEvent`,
// `WaitForSingleObject` and `CloseHandle` may be called on it from any thread. Sharers (the
// process-loopback completion handler) hold it through an `Arc`, so it stays open until the last
// one lets go.
unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

impl EventHandle {
    pub fn new_auto_reset() -> Result<Self> {
        let h = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .context("CreateEventW for audio")?;
        Ok(Self(h))
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }

    pub fn wait(&self, timeout_ms: u32) -> bool {
        let r: WAIT_EVENT = unsafe { WaitForSingleObject(self.0, timeout_ms) };
        r == WAIT_OBJECT_0
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

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
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(self.0);
            }
        }
    }
}

// UTF-16, NUL-terminated "Pro Audio"
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

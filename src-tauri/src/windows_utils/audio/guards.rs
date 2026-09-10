use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::WAVEFORMATEX;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};

/// Owns a `WAVEFORMATEX` that `IAudioClient::GetMixFormat` allocated with `CoTaskMemAlloc`.
///
/// Every early return between the `GetMixFormat` and the matching `CoTaskMemFree` used to leak
/// it — and `acquire` sits on a 200 ms retry loop, so a format the parser rejects leaked about
/// five times a second for as long as the condition lasted.
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
/// Both call sites — the MMCSS "Pro Audio" capture thread and the silent render companion — are
/// threads this crate spawned itself, so they are the one kind that may initialize COM. It still
/// goes through `windows_utils::com`, which asks `CoGetApartmentType` first: if a future caller
/// ever reaches this from a Tauri thread the guard becomes inert rather than either failing with
/// `RPC_E_CHANGED_MODE` or, worse, uninitializing an apartment it never took.
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

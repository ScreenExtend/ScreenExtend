use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};

pub struct ComGuard;

impl ComGuard {
    pub fn init_mta() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
            .context("CoInitializeEx(MTA) on audio thread")?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
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

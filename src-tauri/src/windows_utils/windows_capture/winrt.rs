use windows::Win32::Foundation::S_FALSE;
use windows::Win32::System::Com::{CoDecrementMTAUsage, CoIncrementMTAUsage, CO_MTA_USAGE_COOKIE};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

use crate::windows_utils::com::{apartment_of_this_thread, keep_process_com_alive};

/// Panic safe wrapper around `CoIncrementMTAUsage`.
struct WinMTACookie {
    cookie: CO_MTA_USAGE_COOKIE,
}

impl WinMTACookie {
    /// Increments the current threads MTA usage.
    pub fn new() -> windows::core::Result<Self> {
        Ok(Self {
            cookie: unsafe { CoIncrementMTAUsage()? },
        })
    }
}

impl Drop for WinMTACookie {
    fn drop(&mut self) {
        let _ = unsafe { CoDecrementMTAUsage(self.cookie) };
    }
}

/// Panic safe wrapper for WinRT api initialization.
pub struct WinRT {
    _cookie: WinMTACookie,
    /// Whether this wrapper is what initialized the thread. False on a thread that already had an
    /// apartment, where `RoUninitialize` would decrement a reference this type never took.
    owned: bool,
}

impl WinRT {
    /// Initializes WinRT apis on the current thread.
    ///
    /// **Fork divergence:** upstream calls `RoInitialize(RO_INIT_MULTITHREADED)` unconditionally.
    /// `Handler::start` runs on the *caller's* thread, and in this app that caller can be one
    /// Tauri already put in an STA — where an unconditional multithreaded initialize returns
    /// `RPC_E_CHANGED_MODE` and takes the whole capture down with `FailedToInitWinRT`. So ask
    /// first, the same way `windows_utils::com` does everywhere else: an app-spawned thread (the
    /// `start_free_threaded` path, which is what the live pipeline uses) still gets the MTA it
    /// wants, and a thread that already has an apartment keeps the one it was given.
    ///
    /// The second divergence is [`keep_process_com_alive`]. This type's `CoIncrementMTAUsage` is
    /// balanced on drop, so on a host where capture is the only COM user the last stop tears the
    /// MTA down — and `windows-core` caches WGC's agile activation factories in process-wide raw
    /// pointers that outlive it. The next capture start would then read a dead factory.
    pub fn new() -> windows::core::Result<Self> {
        keep_process_com_alive();
        let cookie = WinMTACookie::new()?;

        let owned = if apartment_of_this_thread().is_explicit() {
            false
        } else {
            if let Err(e) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
                if e.code() != S_FALSE {
                    return Err(e);
                }
            }
            true
        };

        Ok(Self {
            _cookie: cookie,
            owned,
        })
    }
}

impl Drop for WinRT {
    fn drop(&mut self) {
        if self.owned {
            unsafe { RoUninitialize() };
        }
    }
}

//! `IDXGIKeyedMutex::AcquireSync` with its real outcome.
//!
//! `AcquireSync` reports a timeout and an abandoned mutex as SUCCESS codes: `WAIT_TIMEOUT`
//! (0x102) and `WAIT_ABANDONED` (0x80) are positive `HRESULT`s, and the `windows` crate's
//! wrapper maps every non-negative `HRESULT` to `Ok(())`, so `mutex.AcquireSync(key, ms)?`
//! goes on to use a texture it does not own. Every keyed-mutex acquire in the Windows backend
//! goes through [`acquire`], which calls the vtable entry itself.

use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

/// `WAIT_TIMEOUT` as `AcquireSync` returns it.
const WAIT_TIMEOUT_HR: i32 = 0x0000_0102;
/// `WAIT_ABANDONED` as `AcquireSync` returns it.
const WAIT_ABANDONED_HR: i32 = 0x0000_0080;

/// What an `AcquireSync` call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acquired {
    /// The caller owns the mutex and must `ReleaseSync` it.
    Owned,
    /// Nobody released the key within the timeout. The caller does NOT own the mutex and must
    /// neither touch the texture nor call `ReleaseSync`.
    TimedOut,
    /// The previous owner went away without releasing. Per the DXGI documentation the shared
    /// surface and the mutex are no longer consistent and both must be recreated; the caller
    /// must not use the texture.
    Abandoned,
}

/// Classify the raw `HRESULT` of an `AcquireSync` call.
pub fn classify(hr: windows::core::HRESULT) -> windows::core::Result<Acquired> {
    match hr.0 {
        WAIT_TIMEOUT_HR => Ok(Acquired::TimedOut),
        WAIT_ABANDONED_HR => Ok(Acquired::Abandoned),
        _ => hr.ok().map(|()| Acquired::Owned),
    }
}

/// Acquire `key` on `mutex`, waiting at most `timeout_ms`, and say what happened.
///
/// `Err` is a real failure (`E_FAIL` when the device was removed, for example). `Ok` carries
/// the three outcomes the plain wrapper folds into one.
pub fn acquire(
    mutex: &IDXGIKeyedMutex,
    key: u64,
    timeout_ms: u32,
) -> windows::core::Result<Acquired> {
    // SAFETY: `mutex` is a live COM interface for the duration of the call, and the vtable
    // entry is called with exactly the arguments the generated wrapper would pass.
    let hr = unsafe {
        (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), key, timeout_ms)
    };
    classify(hr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HRESULT;

    #[test]
    fn success_codes_that_are_not_ownership_are_told_apart() {
        assert_eq!(classify(HRESULT(0)).unwrap(), Acquired::Owned);
        assert_eq!(classify(HRESULT(0x102)).unwrap(), Acquired::TimedOut);
        assert_eq!(classify(HRESULT(0x80)).unwrap(), Acquired::Abandoned);
        // the plain wrapper reads both as success
        assert!(HRESULT(0x102).ok().is_ok());
        assert!(HRESULT(0x80).ok().is_ok());
    }

    #[test]
    fn a_failure_stays_a_failure() {
        // E_FAIL, what a removed device returns
        assert!(classify(HRESULT(0x8000_4005_u32 as i32)).is_err());
    }
}

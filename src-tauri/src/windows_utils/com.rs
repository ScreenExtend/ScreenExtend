//! COM apartments on this process's threads, and the one way first-party code may join in.
//!
//! Tauri (wry) initializes COM on the main thread and on the threads it hands to commands and
//! event callbacks. A second `CoInitializeEx`/`RoInitialize` there with a different model returns
//! `RPC_E_CHANGED_MODE` and initializes nothing, so [`ensure_for_this_thread`] asks first: a
//! thread that has an apartment is left alone (and never `CoUninitialize`d), and only a thread
//! the app spawned itself is initialized, then released by the returned [`ComScope`].

use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once, OnceLock};

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, RPC_E_TOO_LATE};
use windows::Win32::System::Com::{
    CoGetApartmentType, CoIncrementMTAUsage, CoInitializeEx, CoInitializeSecurity, CoUninitialize,
    APTTYPE, APTTYPEQUALIFIER, APTTYPEQUALIFIER_IMPLICIT_MTA, APTTYPE_MAINSTA, APTTYPE_MTA,
    APTTYPE_NA, APTTYPE_STA, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IMPERSONATE,
};

/// The apartment an *app-spawned* thread should join. Nothing here can change the apartment of a
/// thread that already has one, so this is a request, not an override.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Apartment {
    /// Multi-threaded: what every blocking, non-UI worker wants (WASAPI, WMI, WinRT activation).
    Mta,
    /// Single-threaded, for a thread that owns a window. Unused in this crate today.
    Sta,
}

/// What `CoGetApartmentType` says about the calling thread *right now*.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadApartment {
    /// No apartment, and no process MTA to fall into. Only an app-spawned thread can be here.
    Uninitialized,
    /// A single-threaded apartment: what wry gives the main thread for WebView2.
    Sta,
    /// The process's *main* STA.
    MainSta,
    /// An explicit multi-threaded apartment.
    Mta,
    /// No explicit initialization, but the process has an MTA, so a COM call from this thread
    /// joins it (Windows 8 and later). Usable, but not ours to uninitialize.
    ImplicitMta,
    /// The neutral apartment: a thread executing inside an NA object's call.
    Neutral,
}

impl ThreadApartment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uninitialized => "none",
            Self::Sta => "STA",
            Self::MainSta => "main STA",
            Self::Mta => "MTA",
            Self::ImplicitMta => "implicit MTA",
            Self::Neutral => "neutral",
        }
    }

    /// Whether a COM or WinRT call made from this thread will find an apartment. Everything
    /// except [`Self::Uninitialized`] will.
    pub fn is_usable(self) -> bool {
        self != Self::Uninitialized
    }

    /// Whether the thread has an apartment **of its own**: one somebody called `CoInitializeEx`
    /// for, which belongs to whoever made that call.
    ///
    /// The implicit MTA is deliberately not one: it is a process-wide floor any uninitialized
    /// thread falls into. Counting it would make the answer for the capture and audio threads
    /// depend on whether some other subsystem touched COM first.
    pub fn is_explicit(self) -> bool {
        !matches!(self, Self::Uninitialized | Self::ImplicitMta)
    }
}

/// Counts `CoInitializeEx` calls this crate made that were refused with `RPC_E_CHANGED_MODE`.
/// [`ensure_for_this_thread`] asks first, so only a race between the question and the answer can
/// increment it; the tests assert it stays at zero.
static CHANGED_MODE: AtomicU64 = AtomicU64::new(0);

/// See [`CHANGED_MODE`].
pub fn changed_mode_refusals() -> u64 {
    CHANGED_MODE.load(Ordering::Relaxed)
}

/// Read the calling thread's apartment without touching it.
pub fn apartment_of_this_thread() -> ThreadApartment {
    let mut kind = APTTYPE::default();
    let mut qualifier = APTTYPEQUALIFIER::default();
    // SAFETY: both out-parameters are live stack locals of the right type.
    if unsafe { CoGetApartmentType(&mut kind, &mut qualifier) }.is_err() {
        // the only documented failure for a well-formed call is `CO_E_NOTINITIALIZED`
        return ThreadApartment::Uninitialized;
    }
    match kind {
        APTTYPE_MTA if qualifier == APTTYPEQUALIFIER_IMPLICIT_MTA => ThreadApartment::ImplicitMta,
        APTTYPE_MTA => ThreadApartment::Mta,
        APTTYPE_MAINSTA => ThreadApartment::MainSta,
        APTTYPE_STA => ThreadApartment::Sta,
        APTTYPE_NA => ThreadApartment::Neutral,
        _ => ThreadApartment::Uninitialized,
    }
}

/// A thread's COM apartment, held for as long as the COM work lasts.
///
/// It uninitializes on drop **only** when this scope is what initialized the thread. A scope
/// taken on a Tauri thread is inert: it keeps call sites uniform and gets the apartment logged.
///
/// Declare it *before* any COM object it covers: locals drop in reverse declaration order, and
/// the other way round releases the apartment out from under a live proxy.
pub struct ComScope {
    owned: bool,
    apartment: ThreadApartment,
    /// COM apartments are per thread, so a scope must not be sent to another one.
    _not_send: PhantomData<*mut ()>,
}

impl ComScope {
    /// The apartment this thread is in while the scope is alive.
    pub fn apartment(&self) -> ThreadApartment {
        self.apartment
    }

    /// Whether this scope initialized the thread, and will therefore uninitialize it on drop.
    pub fn owns_initialization(&self) -> bool {
        self.owned
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balanced against the `CoInitializeEx` this scope made, on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// Make COM usable on this thread, asking before initializing anything.
///
/// `site` names the caller in the log ("network", "compatibility-probe", ...); the one-shot
/// report is deduplicated on it and the apartment observed.
pub fn ensure_for_this_thread(site: &'static str, want: Apartment) -> Result<ComScope> {
    keep_process_com_alive();
    let observed = apartment_of_this_thread();
    // an implicit MTA is not this thread's own (see `is_explicit`), so it gets initialized,
    // except when the caller wants an STA, which a thread on the implicit MTA cannot become
    let leave_alone = observed.is_explicit() || (observed.is_usable() && want == Apartment::Sta);
    if leave_alone {
        report(site, observed, false);
        return Ok(ComScope {
            owned: false,
            apartment: observed,
            _not_send: PhantomData,
        });
    }

    let flags = match want {
        Apartment::Mta => COINIT_MULTITHREADED,
        Apartment::Sta => COINIT_APARTMENTTHREADED,
    };
    // SAFETY: no reserved pointer, and the thread was just observed to have no apartment of its
    // own. Explicitly initializing a thread sitting on the implicit MTA is allowed and pins it.
    let hr = unsafe { CoInitializeEx(None, flags) };
    if hr == RPC_E_CHANGED_MODE {
        CHANGED_MODE.fetch_add(1, Ordering::Relaxed);
        return Err(anyhow!(
            "[com] {site}: CoInitializeEx was refused with RPC_E_CHANGED_MODE on a thread that \
             reported no apartment; another thread initialized it in between"
        ));
    }
    hr.ok()
        .map_err(|e| anyhow!("[com] {site}: CoInitializeEx failed: {e}"))?;

    let apartment = apartment_of_this_thread();
    report(site, apartment, true);
    Ok(ComScope {
        owned: true,
        apartment,
        _not_send: PhantomData,
    })
}

/// COM plus the `wmi` crate's "this thread is initialized" marker.
///
/// Use this, never `COMLibrary::new()`: that calls `CoInitializeEx(MTA)` unconditionally, which a
/// Tauri thread refuses. Here the marker is `assume_initialized()`, backed by the returned scope.
///
/// Hold the scope for at least as long as the `WMIConnection` built from the marker.
pub fn wmi_for_this_thread(site: &'static str) -> Result<(ComScope, wmi::COMLibrary)> {
    let scope = ensure_for_this_thread(site, Apartment::Mta)?;
    init_security_once();
    // SAFETY: `scope` is live and guarantees this thread has an apartment.
    let marker = unsafe { wmi::COMLibrary::assume_initialized() };
    Ok((scope, marker))
}

/// Hold the process's COM/WinRT infrastructure open for as long as the process runs.
///
/// `windows-core`'s `FactoryCache` keeps every **agile** WinRT activation factory it resolves in
/// a process-wide `AtomicPtr` and never releases it. Those pointers are valid only while the
/// apartment behind them is: a thread that uninitializes can drop the process's last COM
/// reference and leave them dangling, and the fault (`STATUS_ACCESS_VIOLATION`) lands on another
/// thread at the next capture start. `windows_capture::winrt::WinRT` balances its
/// `CoIncrementMTAUsage` on drop, so stopping capture with no other MTA thread alive is enough.
///
/// `CoIncrementMTAUsage` with no matching decrement is the documented way to hold that floor, and
/// what `windows-core` itself does when it finds COM uninitialized. It costs one idle MTA thread.
pub fn keep_process_com_alive() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: the cookie is deliberately never decremented; see the doc comment.
        match unsafe { CoIncrementMTAUsage() } {
            Ok(_cookie) => tprintln!("[com] process MTA pinned for the lifetime of the process"),
            Err(e) => teprintln!("[com] CoIncrementMTAUsage failed: {e}"),
        }
    });
}

/// Sets the process-wide COM security defaults once, with the parameters `wmi`'s
/// `COMLibrary::new()` would use, since that call is never made here.
///
/// `RPC_E_TOO_LATE` means COM already marshalled something and picked its own defaults (WebView2
/// usually gets there first), which is not a failure.
fn init_security_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: all-default security; no authentication services or principal are supplied.
        let hr = unsafe {
            CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_DEFAULT,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
                None,
            )
        };
        match hr {
            Ok(()) => tprintln!("[com] process COM security initialized (impersonate)"),
            Err(e) if e.code() == RPC_E_TOO_LATE => {
                tprintln!("[com] process COM security was already set by the host process")
            }
            Err(e) => teprintln!("[com] CoInitializeSecurity failed: {e}"),
        }
    });
}

/// One line per (call site, apartment) pair, which is the thread map. A site called from two
/// different kinds of thread reports twice, on purpose.
fn report(site: &'static str, apartment: ThreadApartment, initialized: bool) {
    static SEEN: OnceLock<Mutex<BTreeSet<(&'static str, &'static str)>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(BTreeSet::new()));
    let fresh = {
        let mut guard = seen.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert((site, apartment.as_str()))
    };
    if !fresh {
        return;
    }
    if initialized {
        tprintln!(
            "[com] {site}: thread had no apartment of its own; initialized {} for this \
             app-spawned thread",
            apartment.as_str()
        );
    } else {
        tprintln!(
            "[com] {site}: thread is already in the {} apartment; leaving it alone",
            apartment.as_str()
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_support::skip;

    /// Run `f` on a fresh OS thread whose apartment is `start`, and hand back what it returned.
    ///
    /// `None` means "leave the thread as the OS made it", which is the state every worker the app
    /// spawns starts in.
    fn on_thread<T: Send + 'static>(
        start: Option<Apartment>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        std::thread::spawn(move || {
            // the harness tears its apartments down, which the app never does: pin the process
            // floor first, or the first STA teardown takes `windows-core`'s cached WinRT
            // factories with it and the next test faults
            keep_process_com_alive();
            let owned = start.map(|want| {
                let flags = match want {
                    Apartment::Mta => COINIT_MULTITHREADED,
                    Apartment::Sta => COINIT_APARTMENTTHREADED,
                };
                // SAFETY: a brand new thread; nothing else has initialized it.
                unsafe { CoInitializeEx(None, flags) }
                    .ok()
                    .expect("test thread apartment");
            });
            let out = f();
            if owned.is_some() {
                // SAFETY: balanced against the initialization just above.
                unsafe { CoUninitialize() };
            }
            out
        })
        .join()
        .expect("test thread panicked")
    }

    /// A thread Tauri already initialized (an STA, which is what wry gives the main thread) is
    /// observed, reported and left alone.
    #[test]
    fn a_thread_that_already_has_an_apartment_is_never_re_initialized() {
        let before = changed_mode_refusals();
        let (owned, apartment) = on_thread(Some(Apartment::Sta), || {
            let scope = ensure_for_this_thread("test-sta", Apartment::Mta)
                .expect("asking on an STA thread must not fail");
            (scope.owns_initialization(), scope.apartment())
        });
        assert!(
            !owned,
            "the helper initialized a thread that already had an apartment; \
             dropping that scope would CoUninitialize somebody else's reference"
        );
        assert!(
            matches!(apartment, ThreadApartment::Sta | ThreadApartment::MainSta),
            "expected the STA the test set up, got {apartment:?}"
        );
        assert_eq!(
            changed_mode_refusals(),
            before,
            "asking first must never produce RPC_E_CHANGED_MODE"
        );

        let (owned_mta, apartment_mta) = on_thread(Some(Apartment::Mta), || {
            let scope = ensure_for_this_thread("test-mta", Apartment::Mta).expect("mta");
            (scope.owns_initialization(), scope.apartment())
        });
        assert!(!owned_mta);
        assert_eq!(apartment_mta, ThreadApartment::Mta);
        assert_eq!(changed_mode_refusals(), before);
    }

    /// A thread the app spawned itself has no apartment of its own: the helper gives it an
    /// explicit MTA and hands it back, leaving the process floor as it found it.
    #[test]
    fn an_app_spawned_thread_is_initialized_and_released() {
        let (before, owned, apartment, released) = on_thread(None, || {
            let before = apartment_of_this_thread();
            let (owned, apartment) = {
                let scope = ensure_for_this_thread("test-bare", Apartment::Mta).expect("bare");
                (scope.owns_initialization(), scope.apartment())
            };
            (before, owned, apartment, apartment_of_this_thread())
        });
        // `on_thread` pins the process MTA first, so a bare thread sits on the implicit floor
        // rather than on nothing; the floor is not its own apartment (`is_explicit`)
        assert_eq!(before, ThreadApartment::ImplicitMta);
        assert!(!before.is_explicit());
        assert!(
            owned,
            "a thread with no apartment of its own must be initialized by the scope"
        );
        assert_eq!(apartment, ThreadApartment::Mta);
        assert_eq!(
            released,
            ThreadApartment::ImplicitMta,
            "the scope must give the explicit apartment back and leave the process floor alone"
        );
    }

    /// Call sites nest an outer scope for the thread's lifetime and an inner one per operation.
    /// Only the outer one may release the apartment.
    #[test]
    fn only_the_outermost_scope_owns_the_apartment() {
        on_thread(None, || {
            let outer = ensure_for_this_thread("test-outer", Apartment::Mta).expect("outer");
            let inner = ensure_for_this_thread("test-inner", Apartment::Mta).expect("inner");
            assert!(
                !inner.owns_initialization(),
                "an inner scope must never claim ownership of the apartment"
            );
            assert_eq!(inner.apartment(), outer.apartment());
        });
    }

    /// Adapter enumeration is reached from a Tauri command (STA), the app's watcher thread (no
    /// apartment) and a `spawn_blocking` worker. All three must see the same machine.
    #[test]
    fn adapter_enumeration_agrees_across_apartments() {
        let before = changed_mode_refusals();
        let indices = |list: Vec<super::super::networking::NetworkInfo>| {
            let mut v: Vec<u32> = list.into_iter().map(|a| a.interface_index).collect();
            v.sort_unstable();
            v
        };
        let sta = indices(on_thread(Some(Apartment::Sta), || {
            super::super::networking::cli_adapters()
        }));
        if sta.is_empty() {
            skip(
                "adapter enumeration",
                "this machine reported no usable network adapters",
            );
            return;
        }
        let mta = indices(on_thread(Some(Apartment::Mta), || {
            super::super::networking::cli_adapters()
        }));
        let bare = indices(on_thread(None, super::super::networking::cli_adapters));
        assert_eq!(
            sta, mta,
            "WMI enumeration differed between an STA and an MTA"
        );
        assert_eq!(
            sta, bare,
            "WMI enumeration differed between a Tauri thread and an app-spawned one"
        );
        assert_eq!(
            changed_mode_refusals(),
            before,
            "an enumeration produced RPC_E_CHANGED_MODE"
        );
    }

    /// `GraphicsCaptureSession::IsSupported()` answers `false` on a thread with no apartment, the
    /// same as on a machine without WGC, and `wgc_features()` caches the answer process-wide.
    /// Under a scope it must not depend on the thread.
    #[test]
    fn the_capture_and_hosted_network_probes_agree_across_apartments() {
        use super::super::hosted_network::probe_backend;
        use crate::windows_capture::graphics_capture_api::wgc_supported;

        let probe = || {
            let _scope = ensure_for_this_thread("test-probe", Apartment::Mta).expect("scope");
            (wgc_supported(), probe_backend(None))
        };
        let sta = on_thread(Some(Apartment::Sta), probe);
        let mta = on_thread(Some(Apartment::Mta), probe);
        let bare = on_thread(None, probe);
        assert_eq!(sta, mta, "WGC / hosted-network probe differed on an MTA");
        assert_eq!(
            sta, bare,
            "WGC / hosted-network probe differed on an app-spawned thread"
        );
    }

    /// The compatibility report (what `doctor --json` and the dashboard read) runs its probes on
    /// a thread of its own, so the caller's apartment must not reach the answer.
    #[test]
    fn the_compatibility_report_does_not_depend_on_the_callers_apartment() {
        let before = changed_mode_refusals();
        let summary = |r: crate::CompatibilityReport| {
            (
                r.os_supported,
                r.audio_backend,
                r.display_backend,
                r.capture_backend,
                r.encoder_backend,
                r.hosted_network_backend,
                r.input_backend,
            )
        };
        let sta = summary(on_thread(Some(Apartment::Sta), || {
            super::super::compatibility::check_system_requirements()
        }));
        let mta = summary(on_thread(Some(Apartment::Mta), || {
            super::super::compatibility::check_system_requirements()
        }));
        let bare = summary(on_thread(None, || {
            super::super::compatibility::check_system_requirements()
        }));
        assert_eq!(sta, mta, "the probed backends differed between STA and MTA");
        assert_eq!(
            sta, bare,
            "the probed backends differed between a Tauri thread and an app-spawned one"
        );
        assert_eq!(
            changed_mode_refusals(),
            before,
            "a compatibility probe produced RPC_E_CHANGED_MODE"
        );
    }

    /// Prints the apartment of a few representative threads.
    #[test]
    #[ignore = "read-only report of this machine's COM apartments; run with --ignored --nocapture"]
    fn report_apartments() {
        println!("test main thread: {:?}", apartment_of_this_thread());
        println!(
            "freshly spawned:  {:?}",
            on_thread(None, apartment_of_this_thread)
        );
        println!(
            "after a scope:    {:?}",
            on_thread(None, || {
                let scope = ensure_for_this_thread("report", Apartment::Mta).unwrap();
                (scope.apartment(), scope.owns_initialization())
            })
        );
        println!("changed-mode refusals: {}", changed_mode_refusals());
    }
}

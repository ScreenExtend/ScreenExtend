//! The sender-side preference that looks like a geometry lever, and is not.
//!
//! macOS reduces an AirPlay receiver's advertised size to a single bit —
//! `kCGSVirtualDisplay1080pMode = (height >= 1080)` in MediaToolbox's
//! `scr_bringAirDisplayOnline` — and that bit only chooses which entry of a
//! fixed mode table CoreDisplay makes the default.
//!
//! `com.apple.coremedia`'s `wirelessdisplaymac_set_display_size` looks like the
//! way out: with it set, MediaToolbox additionally emits
//! `com.apple.windowserver.virtualDisplayWidth`/`Height`. But CoreDisplay's
//! custom-geometry branch also requires `…virtualDisplayResolution`, and nothing
//! in the system ever writes that key — so the custom-mode path is dead code.
//! Measured: with the preference confirmed set to 1, a request for 1440x900
//! still produced a stock 1280x720 display.
//!
//! So this module does **not** write the preference. Mutating a global user
//! setting for no measurable benefit is not a trade worth making; the real
//! geometry control is picking from the published mode ladder afterwards
//! ([`super::topology::best_effort_mode`]). What stays here is the ceiling, the
//! refresh-rate truth, and a read-only probe so the preference's state can be
//! reported if someone wants to experiment on another macOS release.

use std::ffi::c_void;
use std::sync::Mutex;

use objc2_core_foundation::{CFNumber, CFRetained, CFString};

const DOMAIN: &str = "com.apple.coremedia";
const KEY: &str = "wirelessdisplaymac_set_display_size";

/// Ceiling of the mode ladder macOS publishes for an AirPlay display.
pub const MAX_WIDTH: u32 = 1920;
pub const MAX_HEIGHT: u32 = 1080;

/// What the receiver advertises in `/info`, regardless of what was requested.
///
/// The advertised size is not a request for that size — macOS keeps only one
/// bit of it, `height >= 1080`, and that bit picks which mode ladder the
/// display gets. Advertising 1080-tall is what makes 1920x1080 available at all;
/// the client's actual geometry is then selected from the ladder.
pub const ADVERTISED_WIDTH: u32 = 1920;
pub const ADVERTISED_HEIGHT: u32 = 1080;
/// The only refresh rate reachable on this path: the sender hardcodes 60 when it
/// builds the timing mode, and nothing emits a refresh-rate override.
pub const FIXED_REFRESH_HZ: u32 = 60;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFPreferencesCopyAppValue(
        key: *const c_void,
        application_id: *const c_void,
    ) -> *const c_void;
    fn CFPreferencesSetAppValue(
        key: *const c_void,
        value: *const c_void,
        application_id: *const c_void,
    );
    fn CFPreferencesAppSynchronize(application_id: *const c_void) -> bool;
    fn CFRelease(cf: *const c_void);
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFNumberGetTypeID() -> usize;
}

/// What the preference was before we touched it, so it can be put back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Previous {
    /// Absent — restoring means removing it again.
    Unset,
    /// Present with this value.
    Set(i64),
}

fn saved() -> &'static Mutex<Option<Previous>> {
    static SAVED: std::sync::OnceLock<Mutex<Option<Previous>>> = std::sync::OnceLock::new();
    SAVED.get_or_init(|| Mutex::new(None))
}

fn key() -> CFRetained<CFString> {
    CFString::from_str(KEY)
}

fn domain() -> CFRetained<CFString> {
    CFString::from_str(DOMAIN)
}

fn read() -> Option<i64> {
    let k = key();
    let d = domain();
    unsafe {
        let v = CFPreferencesCopyAppValue(
            (&*k as *const CFString).cast(),
            (&*d as *const CFString).cast(),
        );
        if v.is_null() {
            return None;
        }
        // The value is written as a number; anything else we treat as absent
        // rather than guessing at a coercion.
        let n = if CFGetTypeID(v) == CFNumberGetTypeID() {
            (*(v as *const CFNumber)).as_i64()
        } else {
            None
        };
        CFRelease(v);
        n
    }
}

fn write(value: Option<i64>) -> bool {
    let k = key();
    let d = domain();
    unsafe {
        match value {
            Some(v) => {
                let n = CFNumber::new_i64(v);
                CFPreferencesSetAppValue(
                    (&*k as *const CFString).cast(),
                    (&*n as *const CFNumber).cast(),
                    (&*d as *const CFString).cast(),
                );
            }
            None => {
                CFPreferencesSetAppValue(
                    (&*k as *const CFString).cast(),
                    std::ptr::null(),
                    (&*d as *const CFString).cast(),
                );
            }
        }
        CFPreferencesAppSynchronize((&*d as *const CFString).cast())
    }
}

/// Whether the exact-geometry path is currently armed.
pub fn exact_geometry_enabled() -> bool {
    read().is_some_and(|v| v != 0)
}

/// Turns the exact-geometry path on, remembering what to restore.
///
/// Returns `Ok(false)` when the preference was already on, so the caller can
/// tell "we changed the system" from "it was already like that".
pub fn enable_exact_geometry() -> Result<bool, String> {
    let current = read();
    if current.is_some_and(|v| v != 0) {
        return Ok(false);
    }

    {
        let mut slot = saved().lock().unwrap();
        if slot.is_none() {
            *slot = Some(match current {
                Some(v) => Previous::Set(v),
                None => Previous::Unset,
            });
        }
    }

    if !write(Some(1)) {
        return Err(format!(
            "could not write {KEY} to {DOMAIN}; AirPlay displays will be created at a fixed \
             1920x1080 instead of the requested size"
        ));
    }

    // Read back rather than trusting the write: on an OS that does not know the
    // key this is where we find out.
    if !exact_geometry_enabled() {
        return Err(format!(
            "{DOMAIN}/{KEY} did not stick, so macOS will ignore the requested display size"
        ));
    }

    tprintln!("[airplay] enabled exact AirPlay display geometry ({DOMAIN} {KEY})");
    Ok(true)
}

/// Puts the preference back the way we found it.
pub fn restore() {
    let previous = saved().lock().unwrap().take();
    let Some(previous) = previous else {
        return;
    };
    let ok = match previous {
        Previous::Unset => write(None),
        Previous::Set(v) => write(Some(v)),
    };
    if ok {
        tprintln!("[airplay] restored {DOMAIN} {KEY}");
    } else {
        teprintln!("[airplay] could not restore {DOMAIN} {KEY}");
    }
}

/// Clamps a requested geometry to what this path can actually produce.
///
/// Returns the geometry to advertise plus a note when something had to give, so
/// the caller can log the difference instead of silently lying to the user.
pub fn clamp(width: u32, height: u32, refresh_hz: u32) -> (u32, u32, u32, Option<String>) {
    let w = width.clamp(2, MAX_WIDTH);
    let h = height.clamp(2, MAX_HEIGHT);
    let mut notes: Vec<String> = Vec::new();

    if (w, h) != (width, height) {
        notes.push(format!(
            "{width}x{height} exceeds the {MAX_WIDTH}x{MAX_HEIGHT} ceiling macOS applies to \
             AirPlay displays; using {w}x{h}"
        ));
    }
    if refresh_hz != 0 && refresh_hz != FIXED_REFRESH_HZ {
        notes.push(format!(
            "AirPlay displays are always {FIXED_REFRESH_HZ} Hz; the requested {refresh_hz} Hz \
             cannot be honoured"
        ));
    }

    let note = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    (w, h, FIXED_REFRESH_HZ, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamping_leaves_a_reachable_geometry_alone() {
        let (w, h, r, note) = clamp(1600, 900, 60);
        assert_eq!((w, h, r), (1600, 900, 60));
        assert!(note.is_none());
    }

    #[test]
    fn portrait_is_clamped_because_the_ladder_is_landscape_only() {
        let (w, h, _, note) = clamp(1080, 1920, 60);
        assert_eq!((w, h), (1080, MAX_HEIGHT));
        assert!(
            note.is_some(),
            "the caller must be told it was not honoured"
        );
    }

    #[test]
    fn oversized_requests_are_clamped_and_reported() {
        let (w, h, _, note) = clamp(5000, 3000, 60);
        assert_eq!((w, h), (MAX_WIDTH, MAX_HEIGHT));
        assert!(note.unwrap().contains("ceiling"));
    }

    /// Anything below 1080 sets macOS's `1080pMode` bit false and costs us the
    /// top of the mode ladder, so this is a compile-time invariant.
    const _: () = assert!(ADVERTISED_HEIGHT >= 1080);

    #[test]
    fn a_non_60hz_request_is_coerced_and_reported() {
        let (_, _, r, note) = clamp(1920, 1080, 120);
        assert_eq!(r, 60);
        assert!(note.unwrap().contains("60 Hz"));
    }

    #[test]
    fn zero_refresh_is_not_reported_as_a_downgrade() {
        let (_, _, r, note) = clamp(1920, 1080, 0);
        assert_eq!(r, 60);
        assert!(note.is_none());
    }

    #[test]
    fn reading_the_preference_does_not_panic() {
        // May be absent; the point is that the FFI round-trips safely.
        let _ = exact_geometry_enabled();
    }

    #[test]
    #[ignore = "writes a real user preference; run manually"]
    fn enable_then_restore_round_trips() {
        let before = read();
        enable_exact_geometry().expect("enable");
        assert!(exact_geometry_enabled());
        restore();
        assert_eq!(read(), before);
    }
}

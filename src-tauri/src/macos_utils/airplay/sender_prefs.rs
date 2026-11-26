//! The sender-side preference that looks like a geometry lever, and is not.
//!
//! macOS keeps one bit of an AirPlay receiver's advertised size (`kCGSVirtualDisplay1080pMode`,
//! `height >= 1080`, set in MediaToolbox), which picks the default entry of a fixed mode table.
//! `wirelessdisplaymac_set_display_size` does not change that: CoreDisplay's custom-geometry
//! branch also needs a `virtualDisplayResolution` key nothing writes (with the preference at 1,
//! a 1440x900 request still produced 1280x720). So the preference is only read here: do not
//! write a global user setting that buys nothing.

use objc2_core_foundation::{CFNumber, CFRetained, CFString};

use super::cf_sys::{CFGetTypeID, CFNumberGetTypeID, CFPreferencesCopyAppValue, CFRelease};

const DOMAIN: &str = "com.apple.coremedia";
const KEY: &str = "wirelessdisplaymac_set_display_size";

/// Ceiling of the mode ladder macOS publishes for an AirPlay display.
pub const MAX_WIDTH: u32 = 1920;
pub const MAX_HEIGHT: u32 = 1080;

/// What the receiver advertises in `/info`, regardless of what was requested. macOS keeps only
/// `height >= 1080` of it, and advertising 1080-tall is what makes 1920x1080 available at all.
pub const ADVERTISED_WIDTH: u32 = 1920;
pub const ADVERTISED_HEIGHT: u32 = 1080;
/// The only refresh rate reachable on this path: the sender hardcodes 60 when it
/// builds the timing mode, and nothing emits a refresh-rate override.
pub const FIXED_REFRESH_HZ: u32 = 60;

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
        // a non-number is treated as absent
        let n = if CFGetTypeID(v) == CFNumberGetTypeID() {
            (*(v as *const CFNumber)).as_i64()
        } else {
            None
        };
        CFRelease(v);
        n
    }
}

/// Whether the exact-geometry path is currently armed.
pub fn exact_geometry_enabled() -> bool {
    read().is_some_and(|v| v != 0)
}

/// Clamps a requested geometry to what this path can produce. The note says what had to give,
/// for the caller to log.
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
        // may be absent: the point is that the FFI round-trips safely
        let _ = exact_geometry_enabled();
    }
}

//! Latency-critical activity assertion (PRD §7).
//!
//! Holding this token for the lifetime of a capture/encode session disables App
//! Nap and timer coalescing for the process, so the OS does not throttle or
//! batch the capture-delivery and encode threads — which is what trims tail
//! latency/jitter. Dropping the token ends the assertion.

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

/// RAII token; keep it alive while streaming.
pub struct LatencyActivity {
    _token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

// The token is just a retained Obj-C object; safe to move to the encode thread.
unsafe impl Send for LatencyActivity {}

/// Begin a `UserInteractive` (UserInitiated + LatencyCritical) activity. Safe to
/// call once per session; the returned token must be held until streaming stops.
pub fn begin_latency_critical_activity() -> LatencyActivity {
    let pi = NSProcessInfo::processInfo();
    let reason = NSString::from_str("ScreenExtend desktop capture/encode");
    // SAFETY: NSProcessInfo is always available; beginActivity returns a token
    // whose lifetime controls the assertion.
    let token = unsafe {
        pi.beginActivityWithOptions_reason(NSActivityOptions::UserInteractive, &reason)
    };
    LatencyActivity { _token: token }
}

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

pub struct LatencyActivity {
    _token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

unsafe impl Send for LatencyActivity {}

/// The per-session activity unless `SE_TUNE_OFF=latency_activity`; the outcome is recorded.
pub fn begin_if_enabled() -> Option<LatencyActivity> {
    if super::tune::enabled("latency_activity") {
        super::tune::record("latency_activity", true, serde_json::Value::Null);
        Some(begin_latency_critical_activity())
    } else {
        super::tune::record_skipped("latency_activity");
        None
    }
}

pub fn begin_latency_critical_activity() -> LatencyActivity {
    let pi = NSProcessInfo::processInfo();
    let reason = NSString::from_str("ScreenExtend desktop capture/encode");
    let options = NSActivityOptions::UserInitiated | NSActivityOptions::LatencyCritical;
    let token = unsafe { pi.beginActivityWithOptions_reason(options, &reason) };
    LatencyActivity { _token: token }
}

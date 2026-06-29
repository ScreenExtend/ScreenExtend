use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

pub struct LatencyActivity {
    _token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

unsafe impl Send for LatencyActivity {}

pub fn begin_latency_critical_activity() -> LatencyActivity {
    let pi = NSProcessInfo::processInfo();
    let reason = NSString::from_str("ScreenExtend desktop capture/encode");
    let token = unsafe {
        pi.beginActivityWithOptions_reason(NSActivityOptions::UserInteractive, &reason)
    };
    LatencyActivity { _token: token }
}

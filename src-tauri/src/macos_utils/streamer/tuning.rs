use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

pub fn apply_process_tuning() {
    static TOKEN: OnceLock<ProcessTuning> = OnceLock::new();
    let _ = TOKEN.set(ProcessTuning::begin());
}

struct ProcessTuning {
    _activity: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

unsafe impl Send for ProcessTuning {}
unsafe impl Sync for ProcessTuning {}

impl ProcessTuning {
    fn begin() -> Self {
        let pi = NSProcessInfo::processInfo();
        let reason = NSString::from_str("ScreenExtend background streaming host");
        let opts = NSActivityOptions::UserInitiated
            | NSActivityOptions::AutomaticTerminationDisabled
            | NSActivityOptions::SuddenTerminationDisabled;
        let _activity = unsafe { pi.beginActivityWithOptions_reason(opts, &reason) };
        tprintln!("[tuning] process-wide App-Nap + termination hardening applied");
        // No `setpriority(PRIO_PROCESS, -10)`: a negative nice value needs root, so for every
        // real user it printed an EACCES line and changed nothing. Darwin expresses process
        // priority through QoS classes and NSProcessInfo activities, both of which are in use
        // here and on the encode threads.
        ProcessTuning { _activity }
    }
}

use std::sync::Arc;

use tauri::AppHandle;
use tauri_specta::Event;

use crate::streamer::lock::LockExt;
use crate::streamer::session::{
    AudioOutputsReport, DeviceInfo, DeviceReporter, SharedDeviceOverrides,
};
use crate::{
    AudioOutput, Device, DeviceAudioOutputs, DeviceJoin, DeviceModify, DeviceRemove,
    JoinAttemptsPaused,
};

#[derive(Debug)]
pub struct TauriDeviceReporter {
    app: AppHandle,
    overrides: SharedDeviceOverrides,
}

impl TauriDeviceReporter {
    pub fn new_shared(app: AppHandle, overrides: SharedDeviceOverrides) -> Arc<Self> {
        Arc::new(Self { app, overrides })
    }
}

impl TauriDeviceReporter {
    /// The dashboard row for one live join, with the device's saved settings folded in.
    ///
    /// Settings are the device's, not the join's: two tabs of one browser read the same row.
    fn row(&self, info: DeviceInfo) -> Device {
        device_row(&self.overrides, info)
    }
}

/// [`TauriDeviceReporter::row`], without the app handle it does not need.
///
/// The overrides map is shared by every adapter's server, so it is taken with `lock_recover`: a
/// poisoned lock would fail every later join with a 500 and nothing in the log to say why.
fn device_row(overrides: &SharedDeviceOverrides, info: DeviceInfo) -> Device {
    {
        let token = info.token.clone();
        let mut device = Device::defaults(info);
        if let Some(o) = overrides.lock_recover().get(&token) {
            device.scale = o.scale;
            device.refresh_rate = o.refresh_rate;
            device.video_scale = o.video_scale;
            device.video_quality = o.video_quality as u32;
            device.control_enabled = o.control_enabled;
            device.pointer_scope = o.pointer_scope.as_str().to_string();
            device.auto_frame_rate = o.auto_frame_rate;
            device.adaptive_quality = o.adaptive_quality;
            device.native_dpr = o.native_dpr;
            device.auto_orientation = o.auto_orientation;
            device.dpr = o.dpr.min(device.max_dpr);
            device.audio_enabled = o.audio_enabled;
        }
        device
    }
}

impl DeviceReporter for TauriDeviceReporter {
    fn report_join(&self, info: DeviceInfo) {
        // logged because `serve` builds the same reporter: a headless run then shows whether a
        // display change moved the row or removed it and put a new one back
        tprintln!(
            "[dashboard] device joined: join={} token={} ({})",
            info.join_id,
            info.token,
            info.ip
        );
        if let Err(e) = DeviceJoin(self.row(info)).emit(&self.app) {
            teprintln!("[device-reporter] emit DeviceJoin failed: {e:?}");
        }
    }

    fn report_modify(&self, info: DeviceInfo) {
        tprintln!(
            "[dashboard] device updated in place: join={} token={} ({})",
            info.join_id,
            info.token,
            info.ip
        );
        if let Err(e) = DeviceModify(self.row(info)).emit(&self.app) {
            teprintln!("[device-reporter] emit DeviceModify failed: {e:?}");
        }
    }

    fn report_remove(&self, join_id: String, token: String) {
        tprintln!("[dashboard] device removed: join={join_id} token={token}");
        let device = Device::defaults(DeviceInfo {
            join_id,
            token,
            ..DeviceInfo::default()
        });
        if let Err(e) = DeviceRemove(device).emit(&self.app) {
            teprintln!("[device-reporter] emit DeviceRemove failed: {e:?}");
        }
    }

    fn report_join_attempts_paused(&self, retry_after_secs: u64) {
        let event = JoinAttemptsPaused {
            retry_after_secs: retry_after_secs as u32,
        };
        if let Err(e) = event.emit(&self.app) {
            teprintln!("[device-reporter] emit JoinAttemptsPaused failed: {e:?}");
        }
    }

    fn report_audio_outputs(&self, join_id: String, token: String, report: AudioOutputsReport) {
        let ev = DeviceAudioOutputs {
            join_id,
            token,
            supported: report.supported,
            outputs: report
                .outputs
                .into_iter()
                .map(|o| AudioOutput {
                    id: o.id,
                    label: o.label,
                })
                .collect(),
            selected: report.selected,
            current: report.current,
            labelled: report.labelled,
        };
        if let Err(e) = ev.emit(&self.app) {
            teprintln!("[device-reporter] emit DeviceAudioOutputs failed: {e:?}");
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::streamer::session::DeviceOverride;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[test]
    fn a_poisoned_overrides_map_still_reports_the_device() {
        let overrides: SharedDeviceOverrides = Arc::new(Mutex::new(HashMap::new()));
        let mut saved = DeviceOverride::join_defaults();
        saved.refresh_rate = 120;
        overrides.lock_recover().insert("tok".to_string(), saved);
        let poisoner = overrides.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the overrides map");
        })
        .join();
        assert!(overrides.is_poisoned());

        let row = device_row(
            &overrides,
            DeviceInfo {
                token: "tok".to_string(),
                ..DeviceInfo::default()
            },
        );
        assert_eq!(row.refresh_rate, 120);
    }
}

use std::sync::Arc;

use tauri::AppHandle;
use tauri_specta::Event;

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
        let token = info.token.clone();
        let mut device = Device::defaults(info);
        if let Some(o) = self.overrides.lock().unwrap().get(&token) {
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
        // What the dashboard is being told, on the record. A display change must move a row, not
        // remove it and put a new one back, and this is the only place that question can be
        // answered without the desktop UI in front of you — `serve` builds the same reporter, so
        // a headless run says exactly what the app's device list would have done.
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

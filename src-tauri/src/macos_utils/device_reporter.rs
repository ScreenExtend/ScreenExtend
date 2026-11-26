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

    /// Only a backend that caps concurrent displays (the macOS AirPlay tier) needs this: a join or
    /// a leave changes nothing else `sync_streamers` reads, so the other tiers skip the thread.
    fn resync_streamers(&self) {
        use tauri::Manager;
        let Some(state) = self.app.try_state::<super::AppState>() else {
            return;
        };
        if crate::streamer::session::shared_display_cap(&state.virtual_display).is_none() {
            return;
        }
        let app = self.app.clone();
        std::thread::spawn(move || {
            if let Some(state) = app.try_state::<super::AppState>() {
                super::sync_streamers(&state);
            }
        });
    }
}

impl TauriDeviceReporter {
    /// The dashboard row for one live join, with the device's saved settings folded in.
    fn row(&self, info: DeviceInfo) -> Device {
        let token = info.token.clone();
        let mut device = Device::defaults(info);
        // settings are the device's, not the join's: two tabs of one browser read the same row.
        // Bound first: a guard in the `if let` scrutinee would live to the end of the block
        let saved = self.overrides.lock().unwrap().get(&token).copied();
        if let Some(o) = saved {
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
        tprintln!(
            "[dashboard] device joined: join={} token={} ({})",
            info.join_id,
            info.token,
            info.ip
        );
        if let Err(e) = DeviceJoin(self.row(info)).emit(&self.app) {
            teprintln!("[device-reporter] emit DeviceJoin failed: {e:?}");
        }
        self.resync_streamers();
    }

    /// UNVERIFIED (not compiled): mirrors the Windows sibling.
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
        // no `resync_streamers`: a renegotiation neither takes a display slot nor frees one
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
        self.resync_streamers();
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

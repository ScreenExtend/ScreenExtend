use std::sync::Arc;

use tauri::AppHandle;
use tauri_specta::Event;

use crate::streamer::session::{DeviceInfo, DeviceReporter, SharedDeviceOverrides};
use crate::{Device, DeviceJoin, DeviceRemove, JoinAttemptsPaused};

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

impl DeviceReporter for TauriDeviceReporter {
    fn report_join(&self, info: DeviceInfo) {
        let ip = info.ip.clone();
        let mut device = Device::defaults(info);
        if let Some(o) = self.overrides.lock().unwrap().get(&ip) {
            device.scale = o.scale;
            device.refresh_rate = o.refresh_rate;
            device.video_scale = o.video_scale;
            device.video_quality = o.video_quality as u32;
            device.control_enabled = o.control_enabled;
            device.dpr = o.dpr.min(device.max_dpr);
            device.audio_enabled = o.audio_enabled;
        }
        if let Err(e) = DeviceJoin(device).emit(&self.app) {
            teprintln!("[device-reporter] emit DeviceJoin failed: {e:?}");
        }
    }

    fn report_remove(&self, ip: String) {
        let device = Device::defaults(DeviceInfo {
            ip,
            token: String::new(),
            name: String::new(),
            os: String::new(),
            screen_size: String::new(),
            refresh_rate: 0,
            portrait: false,
            dpr: 1.0,
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
}

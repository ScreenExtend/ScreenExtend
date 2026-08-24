use std::sync::{Arc, Mutex};

use crate::driver_ipc::{sync::DriverClient, Mode, Monitor};

use crate::streamer::session::{SharedVirtualDisplay, VirtualDisplayController};

#[derive(Debug)]
pub struct WindowsVirtualDisplay {
    client: Mutex<DriverClient>,
}

impl WindowsVirtualDisplay {
    pub fn new_shared() -> Option<SharedVirtualDisplay> {
        let mut client = DriverClient::new().ok()?;
        client.remove_all();
        let _ = client.notify();
        Some(Arc::new(Self {
            client: Mutex::new(client),
        }))
    }
}

impl VirtualDisplayController for WindowsVirtualDisplay {
    fn create_display(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Result<u32, String> {
        self.create_display_with_modes(name, width, height, refresh_rate, &[])
    }

    fn create_display_with_modes(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
        extra_modes: &[(u32, u32)],
    ) -> Result<u32, String> {
        let mut client = self.client.lock().unwrap();
        client.refresh_state();
        let id = client
            .new_id(None)
            .ok_or_else(|| "no free display id".to_string())?;

        let mut dims: Vec<(u32, u32)> = Vec::new();
        let mut candidates: Vec<(u32, u32)> = Vec::with_capacity(1 + extra_modes.len());
        candidates.push((width, height));
        candidates.extend_from_slice(extra_modes);
        for (w, h) in candidates {
            if w < 2 || h < 2 {
                continue;
            }
            if !dims.contains(&(w, h)) {
                dims.push((w, h));
            }
            if w != h && !dims.contains(&(h, w)) {
                dims.push((h, w));
            }
        }
        let modes: Vec<Mode> = dims
            .into_iter()
            .map(|(w, h)| Mode {
                width: w,
                height: h,
                refresh_rates: vec![refresh_rate],
            })
            .collect();

        let monitor = Monitor {
            id,
            enabled: true,
            name: Some(name),
            modes,
        };
        client
            .add(monitor)
            .map_err(|e| format!("add monitor: {e}"))?;
        client.notify().map_err(|e| format!("notify driver: {e}"))?;
        Ok(id)
    }

    fn remove_display(&self, id: u32) {
        let mut client = self.client.lock().unwrap();
        client.refresh_state();
        client.remove(&[id]);
        if let Err(e) = client.notify() {
            teprintln!("virtual_display: notify after remove({id}) failed: {e:?}");
        }
    }

    fn remove_all_displays(&self) {
        let mut client = self.client.lock().unwrap();
        client.remove_all();
        if let Err(e) = client.notify() {
            teprintln!("virtual_display: notify after remove_all failed: {e:?}");
        }
    }
}

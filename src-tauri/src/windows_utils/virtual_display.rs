use std::sync::{Arc, Mutex};

use crate::driver_ipc::{Mode, Monitor, sync::DriverClient};

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
        let mut client = self.client.lock().unwrap();
        client.refresh_state();
        let id = client.new_id(None).ok_or_else(|| "no free display id".to_string())?;
        let mut modes = vec![Mode { width, height, refresh_rates: vec![refresh_rate] }];
        if width != height {
            modes.push(Mode { width: height, height: width, refresh_rates: vec![refresh_rate] });
        }
        let monitor = Monitor { id, enabled: true, name: Some(name), modes };
        client.add(monitor).map_err(|e| format!("add monitor: {e}"))?;
        client.notify().map_err(|e| format!("notify driver: {e}"))?;
        Ok(id)
    }

    fn remove_display(&self, id: u32) {
        let mut client = self.client.lock().unwrap();
        client.refresh_state();
        client.remove(&[id]);
        if let Err(e) = client.notify() {
            eprintln!("virtual_display: notify after remove({id}) failed: {e:?}");
        }
    }

    fn remove_all_displays(&self) {
        let mut client = self.client.lock().unwrap();
        client.remove_all();
        if let Err(e) = client.notify() {
            eprintln!("virtual_display: notify after remove_all failed: {e:?}");
        }
    }
}

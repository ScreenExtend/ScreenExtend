use std::sync::{Arc, Mutex};

use crate::driver_ipc::{sync::DriverClient, Mode, Monitor};

use crate::streamer::session::{SharedVirtualDisplay, VirtualDisplayController};

const COMMON_REFRESH_RATES: [u32; 2] = [60, 120];

#[derive(Debug)]
struct Inner {
    client: DriverClient,
    owned: Vec<Monitor>,
}

impl Inner {
    fn commit(&mut self, monitors: Vec<Monitor>) -> Result<(), String> {
        self.client
            .set_monitors(&monitors)
            .map_err(|e| format!("set monitors: {e}"))?;
        self.owned = monitors;
        self.client
            .notify()
            .map_err(|e| format!("notify driver: {e}"))
    }
}

#[derive(Debug)]
pub struct WindowsVirtualDisplay {
    inner: Mutex<Inner>,
}

impl WindowsVirtualDisplay {
    pub fn new_shared() -> Option<SharedVirtualDisplay> {
        let mut client = DriverClient::new().ok()?;
        client.remove_all();
        let _ = client.notify();
        Some(Arc::new(Self {
            inner: Mutex::new(Inner {
                client,
                owned: Vec::new(),
            }),
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
        let mut inner = self.inner.lock().unwrap();
        let mut monitors = inner.owned.clone();
        let id = (0u32..)
            .find(|candidate| !monitors.iter().any(|m| m.id == *candidate))
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

        let mut refresh_rates = vec![refresh_rate];
        for rate in COMMON_REFRESH_RATES {
            if !refresh_rates.contains(&rate) {
                refresh_rates.push(rate);
            }
        }

        let modes: Vec<Mode> = dims
            .into_iter()
            .map(|(w, h)| Mode {
                width: w,
                height: h,
                refresh_rates: refresh_rates.clone(),
            })
            .collect();

        let name = if monitors
            .iter()
            .any(|m| m.name.as_deref() == Some(name.as_str()))
        {
            format!("{name} ({id})")
        } else {
            name
        };

        monitors.push(Monitor {
            id,
            enabled: true,
            name: Some(name),
            modes,
        });
        inner.commit(monitors)?;
        Ok(id)
    }

    fn remove_display(&self, id: u32) {
        let mut inner = self.inner.lock().unwrap();
        let mut monitors = inner.owned.clone();
        monitors.retain(|m| m.id != id);
        if let Err(e) = inner.commit(monitors) {
            teprintln!("virtual_display: remove({id}) failed: {e}");
        }
    }

    fn remove_all_displays(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.client.remove_all();
        inner.owned.clear();
        if let Err(e) = inner.client.notify() {
            teprintln!("virtual_display: notify after remove_all failed: {e:?}");
        }
    }
}

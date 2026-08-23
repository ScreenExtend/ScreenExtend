//! Default-render-endpoint enumeration and change notifications (PRD §4.5).
//!
//! `IMMNotificationClient::OnDefaultDeviceChanged` fires on a COM thread we don't own. We must
//! not block it and must not take a lock the capture thread holds — so the callback only posts
//! a message to the capture thread over `crossbeam-channel` (the existing dependency). Unplug
//! of the *active* endpoint is caught in the capture loop via `AUDCLNT_E_DEVICE_INVALIDATED`
//! (matching OBS's `win-wasapi.cpp`), so `OnDeviceStateChanged` stays a no-op to avoid
//! spurious re-acquires on unrelated devices.

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eConsole, eRender, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

/// Message posted from the COM notification thread to the capture thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceEvent {
    /// The user switched the default render endpoint (headphones, HDMI, …). Re-acquire.
    DefaultRenderChanged,
}

pub fn create_enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: standard COM object creation; MTA must already be initialized on this thread.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .context("CoCreateInstance(MMDeviceEnumerator)")
}

pub fn default_render_endpoint(
    enumerator: &IMMDeviceEnumerator,
) -> Result<windows::Win32::Media::Audio::IMMDevice> {
    // SAFETY: `enumerator` is a live COM interface.
    unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .context("GetDefaultAudioEndpoint(eRender, eConsole)")
}

#[windows::core::implement(IMMNotificationClient)]
struct DeviceNotifier {
    tx: crossbeam_channel::Sender<DeviceEvent>,
}

impl IMMNotificationClient_Impl for DeviceNotifier_Impl {
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _default_device_id: &PCWSTR,
    ) -> windows::core::Result<()> {
        // Only care about the render/console endpoint we capture from.
        if flow == eRender && role == eConsole {
            // Non-blocking send; never block the COM callback thread.
            let _ = self.tx.send(DeviceEvent::DefaultRenderChanged);
        }
        Ok(())
    }

    fn OnDeviceStateChanged(
        &self,
        _device_id: &PCWSTR,
        _new_state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        // Handled reactively in the capture loop via AUDCLNT_E_DEVICE_INVALIDATED.
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _device_id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// RAII registration: unregisters the notification callback on drop.
pub struct NotifierRegistration {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl NotifierRegistration {
    pub fn register(
        enumerator: &IMMDeviceEnumerator,
        tx: crossbeam_channel::Sender<DeviceEvent>,
    ) -> Result<Self> {
        let client: IMMNotificationClient = DeviceNotifier { tx }.into();
        // SAFETY: `enumerator` and `client` are live COM interfaces.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client) }
            .context("RegisterEndpointNotificationCallback")?;
        Ok(Self {
            enumerator: enumerator.clone(),
            client,
        })
    }
}

impl Drop for NotifierRegistration {
    fn drop(&mut self) {
        // SAFETY: unregistering a callback we registered; both interfaces are still live.
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.client);
        }
    }
}

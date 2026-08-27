use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eConsole, eRender, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceEvent {
    DefaultRenderChanged,
}

pub fn create_enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .context("CoCreateInstance(MMDeviceEnumerator)")
}

pub fn default_render_endpoint(
    enumerator: &IMMDeviceEnumerator,
) -> Result<windows::Win32::Media::Audio::IMMDevice> {
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
        if flow == eRender && role == eConsole {
            let _ = self.tx.send(DeviceEvent::DefaultRenderChanged);
        }
        Ok(())
    }

    fn OnDeviceStateChanged(
        &self,
        _device_id: &PCWSTR,
        _new_state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
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
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.client);
        }
    }
}

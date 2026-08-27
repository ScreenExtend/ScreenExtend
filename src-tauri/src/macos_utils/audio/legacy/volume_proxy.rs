use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioDevicePropertyMute, kAudioDevicePropertyVolumeScalar, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeOutput, AudioObjectAddPropertyListener, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertyListenerProc, AudioObjectRemovePropertyListener,
};

use super::hal;
use super::playthrough::MonitorGain;

#[inline]
pub fn compute_gain(scalar: f32, muted: bool) -> f32 {
    if muted {
        0.0
    } else {
        scalar.clamp(0.0, 1.0)
    }
}

pub fn apply(our_device: AudioObjectID, real_device: AudioObjectID, gain: &MonitorGain) {
    let scalar = hal::output_volume_scalar(our_device).unwrap_or(1.0);
    let muted = hal::output_mute(our_device).unwrap_or(false);
    gain.set(compute_gain(scalar, muted));

    if real_device != 0 {
        let _ = hal::set_output_volume_scalar(real_device, scalar);
        let _ = hal::set_output_mute(real_device, muted);
    }
}

#[derive(Debug, Clone, Copy)]
pub enum VolumeEvent {
    Changed,
}

struct ListenerCtx {
    tx: crossbeam_channel::Sender<VolumeEvent>,
}

extern "C-unwind" fn volume_listener(
    _obj: AudioObjectID,
    _n: u32,
    _addrs: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    let ctx = unsafe { &*(client_data as *const ListenerCtx) };
    let _ = ctx.tx.try_send(VolumeEvent::Changed);
    0
}

fn output_addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMain,
    }
}

pub struct VolumeListeners {
    device: AudioObjectID,
    entries: Vec<(AudioObjectPropertyAddress, *mut ListenerCtx)>,
}

unsafe impl Send for VolumeListeners {}

impl VolumeListeners {
    pub fn register(
        device: AudioObjectID,
        tx: crossbeam_channel::Sender<VolumeEvent>,
    ) -> VolumeListeners {
        let mut entries = Vec::new();
        for selector in [kAudioDevicePropertyVolumeScalar, kAudioDevicePropertyMute] {
            let addr = output_addr(selector);
            let ctx = Box::into_raw(Box::new(ListenerCtx { tx: tx.clone() }));
            let listener: AudioObjectPropertyListenerProc = Some(volume_listener);
            let st = unsafe {
                AudioObjectAddPropertyListener(
                    device,
                    NonNull::from(&addr),
                    listener,
                    ctx as *mut c_void,
                )
            };
            if st == 0 {
                entries.push((addr, ctx));
            } else {
                drop(unsafe { Box::from_raw(ctx) });
                crate::teprintln!("audio(legacy): volume listener register failed ({st})");
            }
        }
        VolumeListeners { device, entries }
    }
}

impl Drop for VolumeListeners {
    fn drop(&mut self) {
        let listener: AudioObjectPropertyListenerProc = Some(volume_listener);
        for (addr, ctx) in self.entries.drain(..) {
            unsafe {
                let _ = AudioObjectRemovePropertyListener(
                    self.device,
                    NonNull::from(&addr),
                    listener,
                    ctx as *mut c_void,
                );
                drop(Box::from_raw(ctx));
            }
        }
    }
}

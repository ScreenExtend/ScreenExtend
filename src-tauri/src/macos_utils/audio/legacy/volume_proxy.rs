//! Volume proxy (PRD-macos-legacy-audio.md §6.2, layer 2).
//!
//! Layer 1 (in the driver) exposes Volume + Mute controls on our device so macOS re-enables the
//! volume keys. Layer 2 (here) makes them *do something*: we observe our device's volume scalar +
//! mute via HAL property listeners and
//!   * apply the resulting gain to the playthrough / monitor path (so the speakers actually get
//!     quieter — smoothed in the IOProc, §5.2b), and
//!   * mirror the value to the real output device's own volume/mute where the hardware supports it,
//!     so the OSD and the state we hand back stay consistent.
//!
//! Design (documented in the PRD): volume/mute affect **local monitoring only**. The streamed audio
//! is tapped in the driver *before* any gain, so muting locally keeps streaming to the remote
//! device — usually what someone extending a display wants.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioDevicePropertyMute, kAudioDevicePropertyVolumeScalar, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeOutput, AudioObjectAddPropertyListener, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertyListenerProc, AudioObjectRemovePropertyListener,
};

use super::hal;
use super::playthrough::MonitorGain;

/// Linear gain to apply given the device's volume scalar and mute state. Pure — unit-tested.
#[inline]
pub fn compute_gain(scalar: f32, muted: bool) -> f32 {
    if muted {
        0.0
    } else {
        scalar.clamp(0.0, 1.0)
    }
}

/// Read our device's current volume + mute, push the gain to the monitor path, and mirror to the
/// real output device (best-effort). Called on the control thread whenever a volume/mute
/// notification fires (never on the RT audio thread).
pub fn apply(our_device: AudioObjectID, real_device: AudioObjectID, gain: &MonitorGain) {
    let scalar = hal::output_volume_scalar(our_device).unwrap_or(1.0);
    let muted = hal::output_mute(our_device).unwrap_or(false);
    gain.set(compute_gain(scalar, muted));

    // Mirror to the real device so the hardware OSD + the value we eventually hand back match the
    // user's intent. Both are best-effort: many devices expose one but not the other.
    if real_device != 0 {
        let _ = hal::set_output_volume_scalar(real_device, scalar);
        let _ = hal::set_output_mute(real_device, muted);
    }
}

/// Events posted from the HAL notification callback to the legacy control thread.
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
    // SAFETY: client_data is our Box<ListenerCtx>, alive until we remove the listener.
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

/// RAII registration of volume + mute listeners on OUR device. Removes them on drop.
pub struct VolumeListeners {
    device: AudioObjectID,
    entries: Vec<(AudioObjectPropertyAddress, *mut ListenerCtx)>,
}

// SAFETY: ctx pointers are owned solely here and freed on drop.
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
            // SAFETY: register on our device; ctx outlives the registration (freed on drop).
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
                // SAFETY: registration failed; reclaim the box.
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
            // SAFETY: remove the listener we registered, then free the ctx.
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

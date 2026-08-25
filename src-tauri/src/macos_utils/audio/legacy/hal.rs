//! Small typed wrappers over the Core Audio HAL property API, shared by the legacy virtual-device
//! backend's routing, playthrough, volume-proxy, and probe code (PRD-macos-legacy-audio.md §8).
//!
//! These are the same `objc2-core-audio` calls the Process Tap backend uses
//! (`macos_utils/audio/process_tap.rs`), factored out so each legacy file reads as intent rather
//! than as `AudioObjectGetPropertyData` boilerplate. Everything here is floor-present on 10.15.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_core_audio::{
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyMute, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyVolumeScalar,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectSetPropertyData,
};
use objc2_core_audio_types::AudioBufferList;
use objc2_core_foundation::{CFRetained, CFString};

/// Build a property address. `scope`/`element` default to global/main for the common case.
pub fn addr(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: element,
    }
}

fn global(selector: u32) -> AudioObjectPropertyAddress {
    addr(
        selector,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    )
}

/// Read a fixed-size POD property. Returns `None` on any non-zero `OSStatus`.
pub fn get_pod<T: Copy>(obj: AudioObjectID, address: &AudioObjectPropertyAddress) -> Option<T> {
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: `out` points at a `T` of `size` bytes; these selectors take no qualifier.
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(out.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if st == 0 {
        // SAFETY: HAL wrote `size` bytes into `out` on success.
        Some(unsafe { out.assume_init() })
    } else {
        None
    }
}

/// Write a fixed-size POD property. Returns the `OSStatus`.
pub fn set_pod<T: Copy>(
    obj: AudioObjectID,
    address: &AudioObjectPropertyAddress,
    value: &T,
) -> i32 {
    let mut v = *value;
    // SAFETY: setting a `T`-sized property; `v` outlives the call.
    unsafe {
        AudioObjectSetPropertyData(
            obj,
            NonNull::from(address),
            0,
            ptr::null(),
            std::mem::size_of::<T>() as u32,
            NonNull::new(&mut v as *mut T as *mut c_void).unwrap(),
        )
    }
}

/// The current system default output device (`0` if unavailable).
pub fn default_output_device() -> AudioObjectID {
    get_pod(
        kAudioObjectSystemObject as AudioObjectID,
        &global(kAudioHardwarePropertyDefaultOutputDevice),
    )
    .unwrap_or(0)
}

/// Set the system default output device. Returns the `OSStatus`.
pub fn set_default_output_device(dev: AudioObjectID) -> i32 {
    set_pod(
        kAudioObjectSystemObject as AudioObjectID,
        &global(kAudioHardwarePropertyDefaultOutputDevice),
        &dev,
    )
}

/// A device's persistent UID string (`None` if it has none / call fails).
pub fn device_uid(dev: AudioObjectID) -> Option<String> {
    let mut cf: *const CFString = ptr::null();
    let address = global(kAudioDevicePropertyDeviceUID);
    let mut size = std::mem::size_of::<*const CFString>() as u32;
    // SAFETY: kAudioDevicePropertyDeviceUID writes a +1-retained CFStringRef into `cf`.
    let st = unsafe {
        AudioObjectGetPropertyData(
            dev,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(&mut cf as *mut *const CFString as *mut c_void).unwrap(),
        )
    };
    if st != 0 || cf.is_null() {
        return None;
    }
    // SAFETY: own the returned +1 CFString and convert.
    let s = unsafe { CFRetained::from_raw(NonNull::new_unchecked(cf as *mut CFString)) };
    Some(s.to_string())
}

/// All audio device IDs currently known to the HAL.
pub fn all_devices() -> Vec<AudioObjectID> {
    let address = global(kAudioHardwarePropertyDevices);
    let mut size: u32 = 0;
    // SAFETY: query the property data size first (variable-length array of AudioObjectID).
    let st = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if st != 0 || size == 0 {
        return Vec::new();
    }
    let count = size as usize / std::mem::size_of::<AudioObjectID>();
    let mut ids = vec![0 as AudioObjectID; count];
    let mut io_size = size;
    // SAFETY: `ids` has room for `count` AudioObjectIDs == `size` bytes.
    let st = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut io_size),
            NonNull::new(ids.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if st != 0 {
        return Vec::new();
    }
    ids.truncate(io_size as usize / std::mem::size_of::<AudioObjectID>());
    ids
}

/// Find a device by its UID, if present and responding.
pub fn device_by_uid(uid: &str) -> Option<AudioObjectID> {
    all_devices()
        .into_iter()
        .find(|&d| device_uid(d).as_deref() == Some(uid))
}

/// Whether the device has at least one output channel (used to pick a playthrough target).
pub fn has_output_channels(dev: AudioObjectID) -> bool {
    stream_channel_count(dev, kAudioObjectPropertyScopeOutput) > 0
}

/// Total channel count across a device's streams in the given scope.
pub fn stream_channel_count(dev: AudioObjectID, scope: u32) -> u32 {
    let address = addr(
        kAudioDevicePropertyStreamConfiguration,
        scope,
        kAudioObjectPropertyElementMain,
    );
    let mut size: u32 = 0;
    // SAFETY: size query for a variable-length AudioBufferList.
    let st = unsafe {
        AudioObjectGetPropertyDataSize(
            dev,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if st != 0 || size == 0 {
        return 0;
    }
    let mut buf = vec![0u8; size as usize];
    let mut io_size = size;
    // SAFETY: `buf` is `size` bytes; the HAL fills it with an AudioBufferList.
    let st = unsafe {
        AudioObjectGetPropertyData(
            dev,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut io_size),
            NonNull::new(buf.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if st != 0 {
        return 0;
    }
    // SAFETY: `buf` starts with an AudioBufferList header; walk its `mNumberBuffers` buffers.
    let list = unsafe { &*(buf.as_ptr() as *const AudioBufferList) };
    let nbuf = list.mNumberBuffers as usize;
    if nbuf == 0 {
        return 0;
    }
    // SAFETY: mBuffers is a flexible array of `nbuf` AudioBuffers within `buf`.
    let bufs = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), nbuf) };
    bufs.iter().map(|b| b.mNumberChannels).sum()
}

/// Read a device's nominal sample rate.
pub fn nominal_sample_rate(dev: AudioObjectID) -> Option<f64> {
    get_pod(dev, &global(kAudioDevicePropertyNominalSampleRate))
}

/// Volume scalar (0.0–1.0) on the output scope's main element, if the device exposes one.
pub fn output_volume_scalar(dev: AudioObjectID) -> Option<f32> {
    get_pod(
        dev,
        &addr(
            kAudioDevicePropertyVolumeScalar,
            kAudioObjectPropertyScopeOutput,
            kAudioObjectPropertyElementMain,
        ),
    )
}

/// Set the output-scope volume scalar. Returns the `OSStatus` (non-zero if unsupported).
pub fn set_output_volume_scalar(dev: AudioObjectID, value: f32) -> i32 {
    set_pod(
        dev,
        &addr(
            kAudioDevicePropertyVolumeScalar,
            kAudioObjectPropertyScopeOutput,
            kAudioObjectPropertyElementMain,
        ),
        &value.clamp(0.0, 1.0),
    )
}

/// Output-scope mute state, if the device exposes one.
pub fn output_mute(dev: AudioObjectID) -> Option<bool> {
    get_pod::<u32>(
        dev,
        &addr(
            kAudioDevicePropertyMute,
            kAudioObjectPropertyScopeOutput,
            kAudioObjectPropertyElementMain,
        ),
    )
    .map(|v| v != 0)
}

/// Set the output-scope mute state. Returns the `OSStatus`.
pub fn set_output_mute(dev: AudioObjectID, muted: bool) -> i32 {
    set_pod::<u32>(
        dev,
        &addr(
            kAudioDevicePropertyMute,
            kAudioObjectPropertyScopeOutput,
            kAudioObjectPropertyElementMain,
        ),
        &u32::from(muted),
    )
}

// Buffer-frame-size selectors ('fsiz' / 'fsz#'), defined locally as four-char codes so this doesn't
// depend on whether the pinned objc2-core-audio exports them.
const SEL_BUFFER_FRAME_SIZE: u32 = u32::from_be_bytes(*b"fsiz");
const SEL_BUFFER_FRAME_SIZE_RANGE: u32 = u32::from_be_bytes(*b"fsz#");

/// `AudioValueRange` (min/max as f64), the payload of `kAudioDevicePropertyBufferFrameSizeRange`.
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioValueRange {
    minimum: f64,
    maximum: f64,
}

/// The device's supported I/O buffer-frame-size range, in frames.
pub fn buffer_frame_size_range(dev: AudioObjectID) -> Option<(u32, u32)> {
    let r: AudioValueRange = get_pod(dev, &global(SEL_BUFFER_FRAME_SIZE_RANGE))?;
    Some((r.minimum as u32, r.maximum as u32))
}

/// The device's current I/O buffer frame size.
pub fn buffer_frame_size(dev: AudioObjectID) -> Option<u32> {
    get_pod(dev, &global(SEL_BUFFER_FRAME_SIZE))
}

/// Set the device's I/O buffer frame size (§5.3, driving the capture period down). Returns OSStatus.
pub fn set_buffer_frame_size(dev: AudioObjectID, frames: u32) -> i32 {
    set_pod(dev, &global(SEL_BUFFER_FRAME_SIZE), &frames)
}

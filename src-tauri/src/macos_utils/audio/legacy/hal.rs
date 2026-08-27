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

pub fn get_pod<T: Copy>(obj: AudioObjectID, address: &AudioObjectPropertyAddress) -> Option<T> {
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>() as u32;
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
        Some(unsafe { out.assume_init() })
    } else {
        None
    }
}

pub fn set_pod<T: Copy>(
    obj: AudioObjectID,
    address: &AudioObjectPropertyAddress,
    value: &T,
) -> i32 {
    let mut v = *value;
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

/// `0` if unavailable
pub fn default_output_device() -> AudioObjectID {
    get_pod(
        kAudioObjectSystemObject as AudioObjectID,
        &global(kAudioHardwarePropertyDefaultOutputDevice),
    )
    .unwrap_or(0)
}

pub fn set_default_output_device(dev: AudioObjectID) -> i32 {
    set_pod(
        kAudioObjectSystemObject as AudioObjectID,
        &global(kAudioHardwarePropertyDefaultOutputDevice),
        &dev,
    )
}

pub fn device_uid(dev: AudioObjectID) -> Option<String> {
    let mut cf: *const CFString = ptr::null();
    let address = global(kAudioDevicePropertyDeviceUID);
    let mut size = std::mem::size_of::<*const CFString>() as u32;
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
    let s = unsafe { CFRetained::from_raw(NonNull::new_unchecked(cf as *mut CFString)) };
    Some(s.to_string())
}

pub fn all_devices() -> Vec<AudioObjectID> {
    let address = global(kAudioHardwarePropertyDevices);
    let mut size: u32 = 0;
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

pub fn device_by_uid(uid: &str) -> Option<AudioObjectID> {
    all_devices()
        .into_iter()
        .find(|&d| device_uid(d).as_deref() == Some(uid))
}

pub fn has_output_channels(dev: AudioObjectID) -> bool {
    stream_channel_count(dev, kAudioObjectPropertyScopeOutput) > 0
}

pub fn stream_channel_count(dev: AudioObjectID, scope: u32) -> u32 {
    let address = addr(
        kAudioDevicePropertyStreamConfiguration,
        scope,
        kAudioObjectPropertyElementMain,
    );
    let mut size: u32 = 0;
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
    let list = unsafe { &*(buf.as_ptr() as *const AudioBufferList) };
    let nbuf = list.mNumberBuffers as usize;
    if nbuf == 0 {
        return 0;
    }
    let bufs = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), nbuf) };
    bufs.iter().map(|b| b.mNumberChannels).sum()
}

pub fn nominal_sample_rate(dev: AudioObjectID) -> Option<f64> {
    get_pod(dev, &global(kAudioDevicePropertyNominalSampleRate))
}

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

const SEL_BUFFER_FRAME_SIZE: u32 = u32::from_be_bytes(*b"fsiz");
const SEL_BUFFER_FRAME_SIZE_RANGE: u32 = u32::from_be_bytes(*b"fsz#");

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioValueRange {
    minimum: f64,
    maximum: f64,
}

pub fn buffer_frame_size_range(dev: AudioObjectID) -> Option<(u32, u32)> {
    let r: AudioValueRange = get_pod(dev, &global(SEL_BUFFER_FRAME_SIZE_RANGE))?;
    Some((r.minimum as u32, r.maximum as u32))
}

pub fn buffer_frame_size(dev: AudioObjectID) -> Option<u32> {
    get_pod(dev, &global(SEL_BUFFER_FRAME_SIZE))
}

pub fn set_buffer_frame_size(dev: AudioObjectID, frames: u32) -> i32 {
    set_pod(dev, &global(SEL_BUFFER_FRAME_SIZE), &frames)
}

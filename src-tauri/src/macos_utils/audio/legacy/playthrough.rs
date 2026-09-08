use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioObjectID,
};
use objc2_core_audio_types::AudioBufferList;

use super::hal;
use crate::macos_utils::audio::ring;

const GAIN_SMOOTH: f32 = 1.0 / 48.0;

#[derive(Default)]
pub struct MonitorGain {
    target_bits: AtomicU32,
}

impl MonitorGain {
    pub fn new() -> Arc<MonitorGain> {
        let g = Arc::new(MonitorGain::default());
        g.set(1.0);
        g
    }
    pub fn set(&self, gain: f32) {
        self.target_bits
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    #[inline]
    fn target(&self) -> f32 {
        f32::from_bits(self.target_bits.load(Ordering::Relaxed))
    }
}

#[inline]
pub fn ramp(current: f32, target: f32) -> f32 {
    current + (target - current) * GAIN_SMOOTH
}

/// Output IO periods of monitor backlog the drift loop steers toward.
///
/// The setpoint *is* the steady-state monitoring latency: the loop's fixed point is that many
/// frames sitting in the monitor ring, on top of the output device's own IOProc latency. On this
/// tier the virtual device is the system default output, so every sound the Mac makes goes through
/// here before reaching the speakers — the fixed 4096 frames this replaces was 85.3 ms at 48 kHz,
/// well past where video on the host screen visibly leads its own audio and where UI click
/// feedback feels broken. Three periods is enough to ride out a late callback.
const SETPOINT_PERIODS: usize = 3;
/// Floor and ceiling on that, in frames. 128 frames is 2.7 ms; the ceiling is the old fixed value,
/// now only the point past which raising it further is not buying anything.
const MIN_SETPOINT_FRAMES: usize = 128;
const MAX_SETPOINT_FRAMES: usize = 4096;
/// How much headroom one measured underrun buys. Adaptive rather than pessimistic: start low and
/// only pay for what this machine turns out to need.
const UNDERRUN_BUMP_FRAMES: usize = 64;

/// Integrator gain on the resampler ratio, per frame of error per callback.
///
/// 25x the value that went with the 4096-frame setpoint, against errors that are now roughly an
/// order of magnitude smaller: a typical error reaches [`DRIFT_TRIM_LIMIT`] in about two seconds
/// instead of half a minute, which is what makes a low setpoint reachable rather than merely
/// declared. The limit itself is unchanged — 0.3% is about five cents of pitch, inaudible.
const DRIFT_GAIN: f64 = 5.0e-8;
const DRIFT_TRIM_LIMIT: f64 = 0.003;

#[inline]
pub fn drift_trim(trim: f64, avail_frames: usize, setpoint: usize) -> f64 {
    let err = avail_frames as f64 - setpoint as f64;
    (trim + err * DRIFT_GAIN).clamp(-DRIFT_TRIM_LIMIT, DRIFT_TRIM_LIMIT)
}

/// Backlog to steer toward, given this device's IO period and what underruns have cost so far.
#[inline]
pub fn setpoint_frames(dst_frames: usize, bump: usize) -> usize {
    let base = dst_frames
        .saturating_mul(SETPOINT_PERIODS)
        .clamp(MIN_SETPOINT_FRAMES, MAX_SETPOINT_FRAMES);
    base.saturating_add(bump).min(MAX_SETPOINT_FRAMES)
}

struct IoCtx {
    monitor: Arc<ring::Consumer>,
    gain: Arc<MonitorGain>,
    base_ratio: f64,
    cur_gain: UnsafeCell<f32>,
    resamp_pos: UnsafeCell<f64>,
    trim: UnsafeCell<f64>,
    hist: UnsafeCell<[f32; 2]>,
    src_scratch: UnsafeCell<Box<[f32]>>,
    underruns: AtomicU32,
    /// extra setpoint frames bought by underruns; only ever read and written by the IOProc, but an
    /// atomic so the control thread can report it without a lock on the real-time path
    setpoint_bump: AtomicUsize,
}

unsafe impl Send for IoCtx {}
unsafe impl Sync for IoCtx {}

extern "C-unwind" fn playthrough_ioproc(
    _dev: AudioObjectID,
    _now: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    _in_input_data: NonNull<AudioBufferList>,
    _in_time: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    out_output_data: NonNull<AudioBufferList>,
    _out_time: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    let ctx = unsafe { &*(client_data as *const IoCtx) };
    let out_list = unsafe { out_output_data.as_ref() };
    let nbuf = out_list.mNumberBuffers as usize;
    if nbuf == 0 {
        return 0;
    }
    let out_bufs = unsafe { std::slice::from_raw_parts(out_list.mBuffers.as_ptr(), nbuf) };

    let b0 = &out_bufs[0];
    if b0.mData.is_null() {
        return 0;
    }
    let dst_channels = b0.mNumberChannels.max(1) as usize;
    let interleaved = nbuf == 1;
    let dst_frames = if interleaved {
        (b0.mDataByteSize as usize) / 4 / dst_channels
    } else {
        (b0.mDataByteSize as usize) / 4
    };
    if dst_frames == 0 {
        return 0;
    }

    let avail_frames = ctx.monitor.available() / 2;
    let bump = ctx.setpoint_bump.load(Ordering::Relaxed);
    let trim = unsafe { &mut *ctx.trim.get() };
    *trim = drift_trim(*trim, avail_frames, setpoint_frames(dst_frames, bump));
    let ratio_eff = ctx.base_ratio * (1.0 + *trim);

    let src_frames_needed = ((dst_frames as f64) * ratio_eff).ceil() as usize + 2;
    let scratch = unsafe { &mut *ctx.src_scratch.get() };
    let cap_frames = scratch.len() / 2;
    let want = src_frames_needed.min(cap_frames);
    let got = ctx.monitor.pop(&mut scratch[..want * 2]) / 2;
    if got < want {
        for s in scratch[got * 2..want * 2].iter_mut() {
            *s = 0.0;
        }
        ctx.underruns.fetch_add(1, Ordering::Relaxed);
        // Buy a little more headroom, up to the ceiling. Two atomic ops, no allocation, no lock.
        if bump < MAX_SETPOINT_FRAMES {
            ctx.setpoint_bump
                .store(bump + UNDERRUN_BUMP_FRAMES, Ordering::Relaxed);
        }
    }

    let cur = unsafe { &mut *ctx.cur_gain.get() };
    let pos = unsafe { &mut *ctx.resamp_pos.get() };
    let hist = unsafe { &mut *ctx.hist.get() };
    let target = ctx.gain.target();

    let src_at = |idx: isize, scratch: &[f32]| -> (f32, f32) {
        if idx < 0 {
            (hist[0], hist[1])
        } else {
            let i = idx as usize;
            if i * 2 + 1 < scratch.len() {
                (scratch[i * 2], scratch[i * 2 + 1])
            } else {
                (0.0, 0.0)
            }
        }
    };

    // Reconstruct the destination slices once, not per frame.
    let mut out_il: &mut [f32] = &mut [];
    let mut out_l: &mut [f32] = &mut [];
    let mut out_r: &mut [f32] = &mut [];
    if interleaved {
        out_il = unsafe {
            std::slice::from_raw_parts_mut(b0.mData as *mut f32, dst_frames * dst_channels)
        };
    } else {
        out_l = unsafe { std::slice::from_raw_parts_mut(b0.mData as *mut f32, dst_frames) };
        if nbuf >= 2 && !out_bufs[1].mData.is_null() {
            out_r = unsafe {
                std::slice::from_raw_parts_mut(out_bufs[1].mData as *mut f32, dst_frames)
            };
        }
    }

    for f in 0..dst_frames {
        let p = *pos;
        let i0 = p.floor() as isize;
        let frac = (p - p.floor()) as f32;
        let (l0, r0) = src_at(i0 - 1, scratch);
        let (l1, r1) = src_at(i0, scratch);
        let l = l0 + (l1 - l0) * frac;
        let r = r0 + (r1 - r0) * frac;

        *cur = ramp(*cur, target);
        let g = *cur;
        let (lg, rg) = (l * g, r * g);
        *pos = p + ratio_eff;

        if interleaved {
            let base = f * dst_channels;
            if dst_channels == 1 {
                out_il[base] = (lg + rg) * 0.5;
            } else {
                out_il[base] = lg;
                out_il[base + 1] = rg;
                for c in 2..dst_channels {
                    out_il[base + c] = 0.0;
                }
            }
        } else {
            if let Some(slot) = out_l.get_mut(f) {
                *slot = lg;
            }
            if let Some(slot) = out_r.get_mut(f) {
                *slot = rg;
            }
        }
    }

    // The interleaved branch zeroes channels 3+ per frame; the non-interleaved one writes only
    // buffers 0 and 1, and CoreAudio does not promise the buffers handed to an IOProc are zeroed —
    // many drivers recycle them. On a multichannel non-interleaved output (a USB or Thunderbolt
    // interface) that would emit stale garbage on channels 3-8 while ScreenExtend is the default
    // output. One memset per buffer per callback, not per frame.
    if !interleaved {
        for b in out_bufs.iter().skip(2) {
            if !b.mData.is_null() {
                unsafe { ptr::write_bytes(b.mData as *mut u8, 0, b.mDataByteSize as usize) };
            }
        }
    }

    let consumed = pos.floor() as isize;
    if consumed >= 1 {
        let (lh, rh) = src_at(consumed - 1, scratch);
        *hist = [lh, rh];
    }
    *pos -= pos.floor();

    0
}

pub struct Playthrough {
    device: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    ctx: *mut IoCtx,
    started: bool,
    gain: Arc<MonitorGain>,
}

unsafe impl Send for Playthrough {}

impl Playthrough {
    pub fn start(
        device: AudioObjectID,
        monitor: Arc<ring::Consumer>,
        gain: Arc<MonitorGain>,
    ) -> Option<Playthrough> {
        if device == 0 {
            return None;
        }
        let dst_rate = hal::nominal_sample_rate(device).unwrap_or(48_000.0);
        let ratio = if dst_rate > 0.0 {
            48_000.0 / dst_rate
        } else {
            1.0
        };
        let scratch = vec![0.0f32; 8192 * 2].into_boxed_slice();
        let ctx = Box::into_raw(Box::new(IoCtx {
            monitor,
            gain: Arc::clone(&gain),
            base_ratio: ratio,
            cur_gain: UnsafeCell::new(1.0),
            resamp_pos: UnsafeCell::new(0.0),
            trim: UnsafeCell::new(0.0),
            hist: UnsafeCell::new([0.0, 0.0]),
            src_scratch: UnsafeCell::new(scratch),
            underruns: AtomicU32::new(0),
            setpoint_bump: AtomicUsize::new(0),
        }));

        let mut proc_id: AudioDeviceIOProcID = None;
        let proc_fn: objc2_core_audio::AudioDeviceIOProc = Some(playthrough_ioproc);
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                device,
                proc_fn,
                ctx as *mut c_void,
                NonNull::from(&mut proc_id),
            )
        };
        if st != 0 || proc_id.is_none() {
            drop(unsafe { Box::from_raw(ctx) });
            crate::teprintln!("audio(legacy): playthrough IOProc create failed (OSStatus {st})");
            return None;
        }
        let st = unsafe { AudioDeviceStart(device, proc_id) };
        if st != 0 {
            unsafe {
                let _ = AudioDeviceDestroyIOProcID(device, proc_id);
                drop(Box::from_raw(ctx));
            }
            crate::teprintln!("audio(legacy): playthrough AudioDeviceStart failed (OSStatus {st})");
            return None;
        }
        crate::tprintln!(
            "audio(legacy): playthrough → device {device} @ {dst_rate}Hz (ratio {ratio:.4})"
        );
        Some(Playthrough {
            device,
            proc_id,
            ctx,
            started: true,
            gain,
        })
    }

    /// Times the monitor ring ran dry, and the extra setpoint frames those underruns have bought.
    ///
    /// The counter used to be incremented and never read, which made the one signal that says the
    /// monitoring latency is set too low invisible. The control thread reports it.
    pub fn underruns(&self) -> (u32, usize) {
        if self.ctx.is_null() {
            return (0, 0);
        }
        let ctx = unsafe { &*self.ctx };
        (
            ctx.underruns.load(Ordering::Relaxed),
            ctx.setpoint_bump.load(Ordering::Relaxed),
        )
    }
}

impl Drop for Playthrough {
    fn drop(&mut self) {
        unsafe {
            if self.started {
                let _ = AudioDeviceStop(self.device, self.proc_id);
            }
            if self.proc_id.is_some() {
                let _ = AudioDeviceDestroyIOProcID(self.device, self.proc_id);
            }
            if !self.ctx.is_null() {
                drop(Box::from_raw(self.ctx));
                self.ctx = ptr::null_mut();
            }
        }
    }
}

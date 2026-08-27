use std::cell::UnsafeCell;
use std::ffi::{c_void, CString};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioObjectID,
};
use objc2_core_audio_types::AudioBufferList;

use crate::macos_utils::audio::ring;
use crate::macos_utils::audio::ControlMsg;
use crate::streamer::audio::AudioDiagnostics;

use super::branding::SHM_NAME;
const SHM_MAGIC: u32 = 0x3141_4553; // 'SEA1' little-endian
const LAYOUT_VERSION: u32 = 1;
const RING_CAPACITY: usize = 131_072; // 2^17 f32 samples
const HEADER_BYTES: usize = 64;
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_SAMPLE_RATE: usize = 8;
const OFF_CHANNELS: usize = 12;
const OFF_CAPACITY: usize = 20;
const OFF_WRITE_POS: usize = 32;
const OFF_GENERATION: usize = 40;
const SHM_TOTAL_BYTES: usize = HEADER_BYTES + RING_CAPACITY * 4;
const DRAIN_CHUNK: usize = 4096;

pub(crate) fn clamp_read(read_pos: u64, write_pos: u64, capacity: u64) -> (u64, u64, bool) {
    let avail = write_pos.wrapping_sub(read_pos);
    if avail > capacity {
        (write_pos.wrapping_sub(capacity), capacity, true)
    } else {
        (read_pos, avail, false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Shm,
    HalInput,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Shm => "shm",
            Transport::HalInput => "hal_input",
        }
    }
}

#[derive(Clone)]
pub struct CaptureTargets {
    pub encoder: Arc<ring::Producer>,
    pub monitor: Arc<ring::Producer>,
    pub diagnostics: Arc<AudioDiagnostics>,
    pub control_tx: Option<crossbeam_channel::Sender<ControlMsg>>,
    pub nonsilent: Arc<AtomicU64>,
}

/// silence floor (~−90 dBFS), matching the Process Tap backend
const SILENCE_THRESHOLD: f32 = 1.0 / 32768.0;

impl CaptureTargets {
    #[inline]
    fn publish(&self, samples: &[f32]) {
        let mut nonsilent = 0u64;
        for &x in samples {
            if x.abs() > SILENCE_THRESHOLD {
                nonsilent += 1;
            }
        }
        if nonsilent > 0 {
            self.nonsilent.fetch_add(nonsilent, Ordering::Relaxed);
        }
        let dropped = self.encoder.push(samples);
        if dropped > 0 {
            self.diagnostics
                .dropped_backpressure
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        let _ = self.monitor.push(samples);
    }
}

struct ShmMap {
    base: *mut c_void,
}

unsafe impl Send for ShmMap {}

impl ShmMap {
    fn open() -> Option<ShmMap> {
        let cname = CString::new(SHM_NAME).ok()?;
        let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0) };
        if fd < 0 {
            return None;
        }
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                SHM_TOTAL_BYTES,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd) };
        if base == libc::MAP_FAILED {
            return None;
        }
        let map = ShmMap { base };
        if !map.header_valid() {
            return None;
        }
        Some(map)
    }

    #[inline]
    fn u32_at(&self, off: usize) -> u32 {
        unsafe { ptr::read_unaligned((self.base as *const u8).add(off) as *const u32) }
    }

    #[inline]
    fn atomic_u64_at(&self, off: usize) -> &AtomicU64 {
        unsafe { &*((self.base as *const u8).add(off) as *const AtomicU64) }
    }

    #[inline]
    fn ring_sample(&self, index: usize) -> f32 {
        unsafe {
            ptr::read_unaligned((self.base as *const u8).add(HEADER_BYTES + index * 4) as *const f32)
        }
    }

    fn header_valid(&self) -> bool {
        self.u32_at(OFF_MAGIC) == SHM_MAGIC
            && self.u32_at(OFF_VERSION) == LAYOUT_VERSION
            && self.u32_at(OFF_CAPACITY) as usize == RING_CAPACITY
            && self.u32_at(OFF_CHANNELS) == 2
            && self.u32_at(OFF_SAMPLE_RATE) == 48_000
    }
}

impl Drop for ShmMap {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base, SHM_TOTAL_BYTES);
        }
    }
}

pub struct Reader {
    transport: Transport,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    hal_input: Option<HalInputProc>,
}

impl Reader {
    pub fn transport(&self) -> Transport {
        self.transport
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.hal_input = None; // HalInputProc tears down in its own Drop
    }
}

pub fn start(device_id: AudioObjectID, targets: CaptureTargets) -> Reader {
    if let Some(map) = ShmMap::open() {
        crate::tprintln!("audio(legacy): capture transport = shm ({SHM_NAME})");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("se-audio-shm".into())
            .spawn(move || shm_reader_loop(map, targets, stop_thread))
            .ok();
        return Reader {
            transport: Transport::Shm,
            stop,
            join,
            hal_input: None,
        };
    }

    crate::tprintln!(
        "audio(legacy): shared memory unavailable (sandbox?), falling back to HAL input"
    );
    let hal_input = HalInputProc::start(device_id, targets);
    Reader {
        transport: Transport::HalInput,
        stop: Arc::new(AtomicBool::new(false)),
        join: None,
        hal_input,
    }
}

fn shm_reader_loop(map: ShmMap, targets: CaptureTargets, stop: Arc<AtomicBool>) {
    crate::macos_utils::streamer::qos::pin_current_thread_time_constraint(5_000_000);

    let mask = RING_CAPACITY - 1;
    let write_pos = map.atomic_u64_at(OFF_WRITE_POS);
    let generation = map.atomic_u64_at(OFF_GENERATION);

    let mut read_pos = write_pos.load(Ordering::Acquire);
    let mut last_gen = generation.load(Ordering::Acquire);
    let mut scratch = vec![0.0f32; DRAIN_CHUNK];

    while !stop.load(Ordering::Relaxed) {
        let gen = generation.load(Ordering::Acquire);
        if gen != last_gen {
            last_gen = gen;
            read_pos = write_pos.load(Ordering::Acquire);
            if let Some(tx) = &targets.control_tx {
                let _ = tx.try_send(ControlMsg::Reacquire);
            }
            continue;
        }

        let w0 = write_pos.load(Ordering::Acquire);
        if w0 == read_pos {
            std::thread::park_timeout(Duration::from_micros(500));
            continue;
        }
        let (new_read, avail, lapped) = clamp_read(read_pos, w0, RING_CAPACITY as u64);
        if lapped {
            targets
                .diagnostics
                .dropped_backpressure
                .fetch_add(new_read.wrapping_sub(read_pos), Ordering::Relaxed);
            read_pos = new_read;
        }

        let n = (avail as usize).min(DRAIN_CHUNK);
        for (i, slot) in scratch.iter_mut().take(n).enumerate() {
            *slot = map.ring_sample((read_pos.wrapping_add(i as u64) as usize) & mask);
        }

        let w1 = write_pos.load(Ordering::Acquire);
        if w1.wrapping_sub(read_pos) > RING_CAPACITY as u64 {
            read_pos = w1;
            continue;
        }
        read_pos = read_pos.wrapping_add(n as u64);
        targets.publish(&scratch[..n]);
    }
}

struct HalInputCtx {
    targets: CaptureTargets,
    scratch: UnsafeCell<Box<[f32]>>,
}

unsafe impl Send for HalInputCtx {}
unsafe impl Sync for HalInputCtx {}

extern "C-unwind" fn input_ioproc(
    _dev: AudioObjectID,
    _now: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    in_input_data: NonNull<AudioBufferList>,
    _in_time: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    _out_data: NonNull<AudioBufferList>,
    _out_time: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    let ctx = unsafe { &*(client_data as *const HalInputCtx) };
    let list = unsafe { in_input_data.as_ref() };
    if list.mNumberBuffers == 0 {
        return 0;
    }
    let b0 = unsafe { &*list.mBuffers.as_ptr() };
    if b0.mData.is_null() {
        return 0;
    }
    let n = (b0.mDataByteSize as usize) / 4;
    let src = unsafe { std::slice::from_raw_parts(b0.mData as *const f32, n) };
    let scratch = unsafe { &mut *ctx.scratch.get() };
    let m = n.min(scratch.len());
    scratch[..m].copy_from_slice(&src[..m]);
    ctx.targets.publish(&scratch[..m]);
    0
}

struct HalInputProc {
    device: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    ctx: *mut HalInputCtx,
    started: bool,
}

unsafe impl Send for HalInputProc {}

impl HalInputProc {
    fn start(device: AudioObjectID, targets: CaptureTargets) -> Option<HalInputProc> {
        if device == 0 {
            return None;
        }
        let scratch = vec![0.0f32; 8192 * 2].into_boxed_slice();
        let ctx = Box::into_raw(Box::new(HalInputCtx {
            targets,
            scratch: UnsafeCell::new(scratch),
        }));
        let mut proc_id: AudioDeviceIOProcID = None;
        let proc_fn: objc2_core_audio::AudioDeviceIOProc = Some(input_ioproc);
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
            crate::teprintln!("audio(legacy): HAL-input IOProc create failed (OSStatus {st})");
            return None;
        }
        let st = unsafe { AudioDeviceStart(device, proc_id) };
        if st != 0 {
            unsafe {
                let _ = AudioDeviceDestroyIOProcID(device, proc_id);
                drop(Box::from_raw(ctx));
            }
            crate::teprintln!("audio(legacy): HAL-input AudioDeviceStart failed (OSStatus {st})");
            return None;
        }
        Some(HalInputProc {
            device,
            proc_id,
            ctx,
            started: true,
        })
    }
}

impl Drop for HalInputProc {
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

#[cfg(test)]
pub(crate) mod layout {
    pub const MAGIC: u32 = super::SHM_MAGIC;
    pub const VERSION: u32 = super::LAYOUT_VERSION;
    pub const CAPACITY: usize = super::RING_CAPACITY;
    pub const HEADER_BYTES: usize = super::HEADER_BYTES;
    pub const OFF_WRITE_POS: usize = super::OFF_WRITE_POS;
    pub const OFF_GENERATION: usize = super::OFF_GENERATION;
    pub const TOTAL_BYTES: usize = super::SHM_TOTAL_BYTES;
}

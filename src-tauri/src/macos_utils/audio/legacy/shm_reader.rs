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

use crate::macos_utils::audio::format;
use crate::macos_utils::audio::ring;
use crate::macos_utils::audio::ControlMsg;
use crate::streamer::audio::AudioDiagnostics;

use super::branding::SHM_NAME;

// Hard ABI contract with `src-tauri/macos/ScreenExtendAudio/src/shm_ring.hpp` — change both
// together. `legacy/test/ring.rs` scrapes that header and fails if the two drift.
const SHM_MAGIC: u32 = 0x3141_4553; // 'SEA1' little-endian
const LAYOUT_VERSION: u32 = 2;
const RING_CAPACITY: usize = 131_072; // 2^17 f32 samples
const HEADER_BYTES: usize = 128;
const FORMAT_F32_INTERLEAVED: u32 = 1;
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_SAMPLE_RATE: usize = 8;
const OFF_CHANNELS: usize = 12;
const OFF_SAMPLE_FORMAT: usize = 16;
const OFF_CAPACITY: usize = 20;
const OFF_HEADER_BYTES: usize = 24;
const OFF_WRITE_POS: usize = 32;
const OFF_GENERATION: usize = 40;
const OFF_READ_POS: usize = 56;
const OFF_ANCHOR_SEQ: usize = 64;
const OFF_ANCHOR_POS: usize = 72;
const OFF_ANCHOR_HOST_TIME: usize = 80;
const SHM_TOTAL_BYTES: usize = HEADER_BYTES + RING_CAPACITY * 4;
const DRAIN_CHUNK: usize = 4096;
/// interleaved stereo samples per second — the ring's unit
const RING_SAMPLES_PER_SEC: i128 = 96_000;

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
    /// `host_ns` is the capture time of `samples[0]`, or 0 when this transport has no stamp.
    #[inline]
    fn publish(&self, samples: &[f32], host_ns: u64) {
        let mut nonsilent = 0u64;
        for &x in samples {
            if x.abs() > SILENCE_THRESHOLD {
                nonsilent += 1;
            }
        }
        if nonsilent > 0 {
            self.nonsilent.fetch_add(nonsilent, Ordering::Relaxed);
        }
        let dropped = self.encoder.push_at(samples, host_ns);
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
    /// false when the segment could only be opened read-only, so `readPos` cannot be published and
    /// the driver's overrun accounting stays off
    writable: bool,
}

unsafe impl Send for ShmMap {}

impl ShmMap {
    fn open() -> Option<ShmMap> {
        let cname = CString::new(SHM_NAME).ok()?;
        // Read-write first, so the driver can be told how far behind we are; a segment created
        // under a restrictive coreaudiod umask may still grant read-only, which works for capture.
        let (fd, writable) = unsafe {
            match libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0) {
                d if d >= 0 => (d, true),
                _ => (libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0), false),
            }
        };
        if fd < 0 {
            return None;
        }
        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                SHM_TOTAL_BYTES,
                prot,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd) };
        if base == libc::MAP_FAILED {
            return None;
        }
        let map = ShmMap { base, writable };
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

    /// Copy `out.len()` samples starting at ring index `start`, in at most two spans.
    #[inline]
    fn copy_ring(&self, start: usize, out: &mut [f32]) {
        let n = out.len();
        let first = (RING_CAPACITY - start).min(n);
        // SAFETY: the mapping is page-aligned and the ring begins at a 16-byte offset, so every
        // f32 is aligned; `start` and `start + first` are both within [0, RING_CAPACITY]
        unsafe {
            let ring = (self.base as *const u8).add(HEADER_BYTES) as *const f32;
            ptr::copy_nonoverlapping(ring.add(start), out.as_mut_ptr(), first);
            if first < n {
                ptr::copy_nonoverlapping(ring, out.as_mut_ptr().add(first), n - first);
            }
        }
    }

    /// The driver's latest (sample index, `mach_absolute_time` ticks) pair, read through its
    /// seqlock. `None` if the driver has published none or the lock was caught mid-write.
    fn anchor(&self) -> Option<(u64, u64)> {
        let seq = self.atomic_u64_at(OFF_ANCHOR_SEQ);
        for _ in 0..4 {
            let before = seq.load(Ordering::Acquire);
            if before == 0 {
                return None;
            }
            if before & 1 != 0 {
                continue;
            }
            let pos = self.atomic_u64_at(OFF_ANCHOR_POS).load(Ordering::Relaxed);
            let ticks = self
                .atomic_u64_at(OFF_ANCHOR_HOST_TIME)
                .load(Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) == before {
                return Some((pos, ticks));
            }
        }
        None
    }

    /// Host-clock time of the sample at absolute index `pos`, or 0 if the driver has not stamped
    /// anything yet.
    fn host_ns_at(&self, pos: u64) -> u64 {
        let Some((anchor_pos, ticks)) = self.anchor() else {
            return 0;
        };
        let base = crate::macos_utils::streamer::clock::mach_ticks_to_host_ns(ticks);
        if base == 0 {
            return 0;
        }
        let delta = pos as i128 - anchor_pos as i128;
        let ns = base as i128 + (delta * 1_000_000_000i128) / RING_SAMPLES_PER_SEC;
        if ns <= 0 {
            0
        } else {
            ns as u64
        }
    }

    /// Tell the driver how far we have consumed, so it can count what it overwrites unread.
    #[inline]
    fn publish_read_pos(&self, pos: u64) {
        if self.writable {
            self.atomic_u64_at(OFF_READ_POS)
                .store(pos, Ordering::Release);
        }
    }

    fn header_valid(&self) -> bool {
        let checks = [
            ("magic", self.u32_at(OFF_MAGIC), SHM_MAGIC),
            ("version", self.u32_at(OFF_VERSION), LAYOUT_VERSION),
            (
                "header_bytes",
                self.u32_at(OFF_HEADER_BYTES),
                HEADER_BYTES as u32,
            ),
            ("capacity", self.u32_at(OFF_CAPACITY), RING_CAPACITY as u32),
            ("channels", self.u32_at(OFF_CHANNELS), 2),
            ("sample_rate", self.u32_at(OFF_SAMPLE_RATE), 48_000),
            (
                "sample_format",
                self.u32_at(OFF_SAMPLE_FORMAT),
                FORMAT_F32_INTERLEAVED,
            ),
        ];
        for (name, got, want) in checks {
            if got != want {
                crate::teprintln!(
                    "audio(legacy): shared segment header rejected: {name} is {got}, expected \
                     {want} (driver and host layouts differ — reinstall the audio driver)"
                );
                return false;
            }
        }
        true
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

pub fn start(device_id: AudioObjectID, mut targets: CaptureTargets) -> Reader {
    if let Some(map) = ShmMap::open() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let targets_for_fallback = targets.clone();
        match std::thread::Builder::new()
            .name("se-audio-shm".into())
            .spawn(move || shm_reader_loop(map, targets, stop_thread))
        {
            Ok(join) => {
                crate::tprintln!("audio(legacy): capture transport = shm ({SHM_NAME})");
                return Reader {
                    transport: Transport::Shm,
                    stop,
                    join: Some(join),
                    hal_input: None,
                };
            }
            Err(e) => {
                // Discarding this used to leave `Transport::Shm` reported with no thread behind
                // it: the success line already printed, and no audio produced, forever.
                crate::teprintln!(
                    "audio(legacy): shm reader thread spawn failed ({e}); falling back to HAL input"
                );
                targets = targets_for_fallback;
            }
        }
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

/// How long the reader sleeps when the shared ring is empty.
///
/// A pure polling interval, not a wait: the producer is `coreaudiod` in another process writing to
/// shared memory, so there is no cross-process wakeup to be had and nothing ever unparks this
/// thread. It is a quarter of the driver's own 2.67 ms IO cycle (128 frames at 48 kHz), so a block
/// waits at most a fraction of the period that produced it.
const POLL_INTERVAL: Duration = Duration::from_micros(250);

fn shm_reader_loop(map: ShmMap, targets: CaptureTargets, stop: Arc<AtomicBool>) {
    // Not a Mach time-constraint policy. That declares a periodic deadline the scheduler holds the
    // thread to, and this thread then sleeps for an interval unrelated to it — a contract it
    // cannot honour, whose usual reward is being demoted out of the real-time band anyway. The
    // work here is a memcpy out of shared memory and a ring push; `user_interactive` is what it
    // actually needs, and is what the SCK audio delivery queue asks for on the native tiers.
    crate::macos_utils::streamer::qos::pin_current_thread_user_interactive();

    let mask = RING_CAPACITY - 1;
    let write_pos = map.atomic_u64_at(OFF_WRITE_POS);
    let generation = map.atomic_u64_at(OFF_GENERATION);

    let mut read_pos = write_pos.load(Ordering::Acquire);
    let mut last_gen = generation.load(Ordering::Acquire);
    let mut scratch = vec![0.0f32; DRAIN_CHUNK];
    map.publish_read_pos(read_pos);

    while !stop.load(Ordering::Relaxed) {
        let gen = generation.load(Ordering::Acquire);
        if gen != last_gen {
            last_gen = gen;
            read_pos = write_pos.load(Ordering::Acquire);
            map.publish_read_pos(read_pos);
            if let Some(tx) = &targets.control_tx {
                let _ = tx.try_send(ControlMsg::Reacquire);
            }
            continue;
        }

        let w0 = write_pos.load(Ordering::Acquire);
        if w0 == read_pos {
            std::thread::sleep(POLL_INTERVAL);
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
        // when the driver wrote this block, not when this thread got to it
        let block_ns = map.host_ns_at(read_pos);
        map.copy_ring((read_pos as usize) & mask, &mut scratch[..n]);

        let w1 = write_pos.load(Ordering::Acquire);
        if w1.wrapping_sub(read_pos) > RING_CAPACITY as u64 {
            read_pos = w1;
            map.publish_read_pos(read_pos);
            continue;
        }
        read_pos = read_pos.wrapping_add(n as u64);
        map.publish_read_pos(read_pos);
        targets.publish(&scratch[..n], block_ns);
    }
}

struct HalInputCtx {
    targets: CaptureTargets,
    scratch: UnsafeCell<Box<[f32]>>,
    /// what the device actually said it delivers, read once at start
    desc: format::AudioFormatDesc,
}

unsafe impl Send for HalInputCtx {}
unsafe impl Sync for HalInputCtx {}

extern "C-unwind" fn input_ioproc(
    _dev: AudioObjectID,
    _now: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    in_input_data: NonNull<AudioBufferList>,
    in_time: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    _out_data: NonNull<AudioBufferList>,
    _out_time: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    let ctx = unsafe { &*(client_data as *const HalInputCtx) };
    let list = unsafe { in_input_data.as_ref() };
    let nbuf = list.mNumberBuffers as usize;
    if nbuf == 0 {
        return 0;
    }
    // SAFETY: `mBuffers` is a C flexible array of `mNumberBuffers` `AudioBuffer`s
    let bufs = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), nbuf) };
    let b0 = &bufs[0];
    if b0.mData.is_null() {
        return 0;
    }
    let desc = &ctx.desc;
    let bps = match desc.kind {
        format::SampleKind::F32 | format::SampleKind::I32 => 4,
        format::SampleKind::I16 => 2,
    };
    let scratch = unsafe { &mut *ctx.scratch.get() };

    // Run the delivered layout through the same converter the native backends use, rather than
    // reinterpreting whatever arrives as interleaved stereo f32. This is the path a host takes
    // when the driver could not create its shared segment — the case where least is known about
    // the running configuration, and the last place to be guessing at the format.
    let written = if desc.non_interleaved {
        let ch = nbuf.min(8);
        let mut planes: [&[u8]; 8] = [&[][..]; 8];
        let frames = b0.mDataByteSize as usize / bps;
        for (i, p) in planes.iter_mut().enumerate().take(ch) {
            let b = &bufs[i];
            if b.mData.is_null() {
                return 0;
            }
            // SAFETY: `mData` points at `mDataByteSize` bytes of channel `i`'s plane
            *p = unsafe {
                std::slice::from_raw_parts(b.mData as *const u8, b.mDataByteSize as usize)
            };
        }
        format::convert_planar(&planes[..ch], frames, desc, scratch)
    } else {
        let stride = desc.channels as usize * bps;
        if stride == 0 {
            return 0;
        }
        let frames = b0.mDataByteSize as usize / stride;
        // SAFETY: `mData` points at `mDataByteSize` bytes of interleaved frames
        let src =
            unsafe { std::slice::from_raw_parts(b0.mData as *const u8, b0.mDataByteSize as usize) };
        format::convert_interleaved(src, frames, desc, scratch)
    };
    if written == 0 {
        return 0;
    }
    ctx.targets.publish(
        &scratch[..written],
        crate::macos_utils::audio::process_tap::block_host_ns(in_time),
    );
    0
}

/// `kAudioDevicePropertyStreamFormat`, input scope, parsed.
fn read_input_format(device: AudioObjectID) -> Option<format::AudioFormatDesc> {
    const SEL_STREAM_FORMAT: u32 = u32::from_be_bytes(*b"sfmt");
    let addr = super::hal::addr(
        SEL_STREAM_FORMAT,
        objc2_core_audio::kAudioObjectPropertyScopeInput,
        objc2_core_audio::kAudioObjectPropertyElementMain,
    );
    let asbd: objc2_core_audio_types::AudioStreamBasicDescription =
        super::hal::get_pod(device, &addr)?;
    match format::parse_asbd(&asbd) {
        Ok(d) => Some(d),
        Err(e) => {
            crate::teprintln!("audio(legacy): HAL input stream format unusable on {device}: {e}");
            None
        }
    }
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
        let desc = read_input_format(device)?;
        // Nothing in the macOS audio path resamples, and the ring's whole contract is 48 kHz — a
        // rate mismatch here would encode at the wrong speed rather than fail, so refuse instead.
        if desc.sample_rate != 48_000 {
            crate::teprintln!(
                "audio(legacy): HAL input on device {device} is {} Hz, not 48000; refusing the \
                 fallback transport rather than encoding at the wrong speed",
                desc.sample_rate
            );
            return None;
        }
        crate::tprintln!(
            "audio(legacy): HAL input format {} Hz x{} {:?}{}",
            desc.sample_rate,
            desc.channels,
            desc.kind,
            if desc.non_interleaved {
                " planar"
            } else {
                " interleaved"
            }
        );
        let scratch = vec![0.0f32; 8192 * 2].into_boxed_slice();
        let ctx = Box::into_raw(Box::new(HalInputCtx {
            targets,
            scratch: UnsafeCell::new(scratch),
            desc,
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
    pub const SAMPLE_FORMAT: u32 = super::FORMAT_F32_INTERLEAVED;
    pub const TOTAL_BYTES: usize = super::SHM_TOTAL_BYTES;
    /// every named field offset, as `(name in shm_ring.hpp, byte offset)`
    pub const OFFSETS: &[(&str, usize)] = &[
        ("magic", super::OFF_MAGIC),
        ("version", super::OFF_VERSION),
        ("sampleRate", super::OFF_SAMPLE_RATE),
        ("channels", super::OFF_CHANNELS),
        ("sampleFormat", super::OFF_SAMPLE_FORMAT),
        ("capacity", super::OFF_CAPACITY),
        ("headerBytes", super::OFF_HEADER_BYTES),
        ("writePos", super::OFF_WRITE_POS),
        ("generation", super::OFF_GENERATION),
        ("readPos", super::OFF_READ_POS),
        ("anchorSeq", super::OFF_ANCHOR_SEQ),
        ("anchorPos", super::OFF_ANCHOR_POS),
        ("anchorHostTime", super::OFF_ANCHOR_HOST_TIME),
    ];
}

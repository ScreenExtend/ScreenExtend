//! `ScreenExtend bench record`: a take of one monitor exactly as the encoder would receive it
//! (WGC BGRA8, pointer composited, no scaling), every delivered frame up to a rate cap. The
//! capture callback only copies into a free staging texture and never waits; a frame that finds
//! none is counted SKIPPED. Frames are LZ4-compressed in parallel into container v3 unless
//! `--raw`, and written with unbuffered sequential writes.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;

use super::clipfmt::{
    compress_frame, record_header, FileHeader, FrameWord, IndexBuilder, PixelFormat, Reorder,
    VarFileHeader, CODEC_LZ4, FILE_CLOSED, FILE_HEADER, FLAG_CURSOR_MOVED, FLAG_DIRTY_EMPTY,
    FLAG_DIRTY_KNOWN, FLAG_OS_TIME, HEADER, INDEX_ALIGN, INDEX_ENTRY, INDEX_HEADER,
    VAR_RECORD_HEAD,
};
use super::sys;
use crate::streamer::audio::host_now_ns;
use crate::windows_utils::windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use crate::windows_utils::windows_capture::frame::Frame;
use crate::windows_utils::windows_capture::graphics_capture_api::{
    wgc_features, InternalCaptureControl,
};
use crate::windows_utils::windows_capture::monitor::Monitor;
use crate::windows_utils::windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

/// Refuse a take that would leave less than this free.
const SPARE_BYTES: u64 = 50 * 1_000_000_000;
/// One disk write. A multiple of every sector size in use.
const CHUNK: usize = 32 << 20;
/// Chunks in flight between the mapper and the disk thread.
const CHUNKS: usize = 8;
const SECTOR: usize = 4096;
/// Staging textures: about this many bytes of frames between the callback and the mapper.
const STAGING_BUDGET: usize = 320 << 20;

struct Opts {
    monitor: Option<usize>,
    seconds: f64,
    fps: u32,
    out: Option<PathBuf>,
    countdown: u32,
    /// Feed the writer from a paced generator instead of WGC; captures nothing.
    synthetic: bool,
    content: Content,
    /// The v2 fixed-size raw container.
    raw: bool,
    /// LZ4 worker threads (compressed takes).
    compress_threads: usize,
    /// `--verify <file>`: read and check a take instead of recording one.
    verify: Option<PathBuf>,
}

/// What `--synthetic-source` paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content {
    /// Flat regions, text-like detail and a few moving rectangles: compresses like a desktop.
    Desktop,
    /// Every pixel random: the worst case, LZ4 gains nothing and frames are stored raw.
    Noise,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        monitor: None,
        seconds: 10.0,
        fps: 120,
        out: None,
        countdown: 5,
        synthetic: false,
        content: Content::Desktop,
        raw: false,
        compress_threads: 4,
        verify: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--monitor" => {
                let v = val("--monitor")?;
                o.monitor = if v == "primary" {
                    None
                } else {
                    Some(v.parse::<usize>().ok().filter(|n| *n >= 1).ok_or(format!(
                        "--monitor: 'primary' or a 1-based index, not '{v}'"
                    ))?)
                };
            }
            "--seconds" => {
                o.seconds = val("--seconds")?
                    .parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && *s > 0.0 && *s <= 3600.0)
                    .ok_or("--seconds: a number of seconds, 0 < N <= 3600")?
            }
            "--fps" => {
                o.fps = val("--fps")?
                    .parse::<u32>()
                    .ok()
                    .filter(|f| (1..=1000).contains(f))
                    .ok_or("--fps: 1..=1000")?
            }
            "--out" => o.out = Some(PathBuf::from(val("--out")?)),
            "--countdown" => {
                o.countdown = val("--countdown")?
                    .parse::<u32>()
                    .ok()
                    .filter(|c| *c <= 60)
                    .ok_or("--countdown: 0..=60 seconds")?
            }
            "--synthetic-source" => o.synthetic = true,
            "--synthetic-content" => {
                o.content = match val("--synthetic-content")?.as_str() {
                    "desktop" => Content::Desktop,
                    "noise" => Content::Noise,
                    v => return Err(format!("--synthetic-content: desktop | noise, not '{v}'")),
                }
            }
            "--raw" => o.raw = true,
            "--compress-threads" => {
                o.compress_threads = val("--compress-threads")?
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=32).contains(n))
                    .ok_or("--compress-threads: 1..=32")?
            }
            "--verify" => o.verify = Some(PathBuf::from(val("--verify")?)),
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown record option '{other}'")),
        }
    }
    Ok(o)
}

fn default_out() -> Result<PathBuf, String> {
    let dir = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir.join(format!("take-{}.seraw", sys::local_stamp())))
}

/// Did the pointer move on, onto or off the captured monitor since the previous frame? Mirrors
/// the capture pipeline's own test.
struct CursorProbe {
    monitor: isize,
    handle: isize,
    x: i32,
    y: i32,
    on_monitor: bool,
    broken: bool,
}

impl CursorProbe {
    fn new(monitor: isize) -> Self {
        Self {
            monitor,
            handle: 0,
            x: i32::MIN,
            y: i32::MIN,
            on_monitor: false,
            broken: false,
        }
    }

    fn changed(&mut self) -> bool {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTONULL};
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};
        if self.broken {
            return false;
        }
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            self.broken = true;
            return false;
        }
        const CURSOR_SUPPRESSED: u32 = 0x2;
        let showing = info.flags.0 & CURSOR_SHOWING.0 != 0 && info.flags.0 & CURSOR_SUPPRESSED == 0;
        let pt = POINT {
            x: info.ptScreenPos.x,
            y: info.ptScreenPos.y,
        };
        let mine = HMONITOR(self.monitor as *mut core::ffi::c_void);
        let on_monitor = showing && unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONULL) } == mine;
        let handle = info.hCursor.0 as isize;
        let changed = if on_monitor {
            !self.on_monitor || handle != self.handle || pt.x != self.x || pt.y != self.y
        } else {
            self.on_monitor
        };
        self.on_monitor = on_monitor;
        if on_monitor {
            self.handle = handle;
            self.x = pt.x;
            self.y = pt.y;
        }
        changed
    }
}

#[derive(Default)]
struct Shared {
    /// Frames are recorded only while armed (after the countdown) and not stopped.
    armed: AtomicBool,
    stop: AtomicBool,
    /// The capture thread has stopped; once the queue is empty the mapper is done.
    capture_done: AtomicBool,
    delivered: AtomicU64,
    recorded: AtomicU64,
    skipped: AtomicU64,
    pre_roll: AtomicU64,
    dirty_empty: AtomicU64,
    cursor_moved: AtomicU64,
    /// Bytes the disk thread has written (sector-padded tail excluded).
    written: AtomicU64,
    /// Nanoseconds the disk thread spent inside `WriteFile`.
    write_ns: AtomicU64,
    stale: Arc<AtomicU32>,
    error: Mutex<Option<String>>,
    /// Callback-entry host times of every delivered frame while armed (recorded or skipped).
    delivered_ns: Mutex<Vec<u64>>,
    /// `SystemRelativeTime` (on the host clock) minus callback entry, ns, per recorded frame.
    srt_minus_entry: Mutex<Vec<i64>>,
    /// Uncompressed picture bytes and stored record bytes packed so far (for the ratio).
    raw_bytes: AtomicU64,
    stored_bytes: AtomicU64,
    /// Frames handed to the compress workers / written by the packer, and the most that were
    /// between the two at once (the worker queue's high-water mark).
    submitted: AtomicU64,
    packed: AtomicU64,
    queue_hw: AtomicU64,
}

impl Shared {
    fn fail(&self, e: String) {
        let mut g = self.error.lock().unwrap_or_else(|p| p.into_inner());
        if g.is_none() {
            *g = Some(e);
        }
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A staged frame on its way to the mapper.
struct Staged {
    slot: usize,
    tex: ID3D11Texture2D,
    header: [u8; HEADER],
}

struct Init {
    shared: Arc<Shared>,
    tx: Sender<Staged>,
    free_tx: Sender<usize>,
    free_rx: Receiver<usize>,
    /// Hands the capture device's context and adapter label to the mapper, once.
    setup_tx: Sender<(ID3D11DeviceContext, String)>,
    monitor: isize,
    width: u32,
    height: u32,
    slots: usize,
}

struct Recorder {
    init: Init,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Vec<ID3D11Texture2D>,
    cursor: CursorProbe,
    _tuning: crate::windows_utils::streamer::tuning::ThreadTuning,
}

impl Recorder {
    /// Staging textures shaped like the first delivered frame.
    fn make_staging(&mut self, desc: &D3D11_TEXTURE2D_DESC) -> Result<(), String> {
        let d = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        for i in 0..self.init.slots {
            let mut t = None;
            unsafe { self.device.CreateTexture2D(&d, None, Some(&mut t)) }
                .map_err(|e| format!("staging texture {i}: {e}"))?;
            self.staging
                .push(t.ok_or("staging texture: none returned")?);
            let _ = self.init.free_tx.send(i);
        }
        Ok(())
    }
}

impl GraphicsCaptureApiHandler for Recorder {
    type Flags = Init;
    type Error = anyhow::Error;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        // the mapper maps on the immediate context from its own thread
        if let Ok(mt) = ctx.device_context.cast::<ID3D11Multithread>() {
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }
        let _ = ctx
            .flags
            .setup_tx
            .send((ctx.device_context.clone(), ctx.adapter.label()));
        Ok(Recorder {
            cursor: CursorProbe::new(ctx.flags.monitor),
            init: ctx.flags,
            device: ctx.device,
            context: ctx.device_context,
            staging: Vec::new(),
            // the capture thread: tuned as a session's capture thread is
            _tuning: crate::windows_utils::streamer::tuning::tune_current_thread(),
        })
    }

    fn frame_pool_buffers(&self) -> u32 {
        3
    }

    fn stale_frame_counter(&self) -> Option<Arc<AtomicU32>> {
        Some(Arc::clone(&self.init.shared.stale))
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let entry = host_now_ns();
        let sh = Arc::clone(&self.init.shared);
        // asked on every delivered frame so "moved" is relative to the previous delivery
        let moved = self.cursor.changed();
        if sh.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if self.staging.is_empty() {
            let desc = *frame.desc();
            if (desc.Width, desc.Height) != (self.init.width, self.init.height) {
                sh.fail(format!(
                    "WGC delivered {}x{} but the monitor reports {}x{}",
                    desc.Width, desc.Height, self.init.width, self.init.height
                ));
                return Ok(());
            }
            if let Err(e) = self.make_staging(&desc) {
                sh.fail(e);
                return Ok(());
            }
        }
        if !sh.armed.load(Ordering::Relaxed) {
            sh.pre_roll.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        sh.delivered.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut v) = sh.delivered_ns.lock() {
            v.push(entry);
        }
        if (frame.width(), frame.height()) != (self.init.width, self.init.height) {
            sh.fail(format!(
                "the monitor changed size during the take ({}x{} -> {}x{})",
                self.init.width,
                self.init.height,
                frame.width(),
                frame.height()
            ));
            return Ok(());
        }
        let Ok(slot) = self.init.free_rx.try_recv() else {
            sh.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let tex = self.staging[slot].clone();
        unsafe {
            self.context.CopyResource(&tex, frame.as_raw_texture());
            self.context.Flush();
        }
        let mut flags = 0;
        if moved {
            flags |= FLAG_CURSOR_MOVED;
            sh.cursor_moved.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(regions) = frame.dirty_regions() {
            flags |= FLAG_DIRTY_KNOWN;
            if regions.is_empty() {
                flags |= FLAG_DIRTY_EMPTY;
                sh.dirty_empty.fetch_add(1, Ordering::Relaxed);
            }
        }
        // `SystemRelativeTime` is QPC in 100 ns units
        let os_ns = match frame.timestamp() {
            Ok(ts) if ts.Duration > 0 => {
                flags |= FLAG_OS_TIME;
                let host = crate::windows_utils::streamer::clock::boot_ns_to_host_ns(
                    ts.Duration as u64 * 100,
                );
                if let Ok(mut v) = sh.srt_minus_entry.lock() {
                    v.push(host as i64 - entry as i64);
                }
                host
            }
            _ => 0,
        };
        let header = record_header(entry, os_ns, frame.width(), frame.height(), false, flags);
        if self.init.tx.try_send(Staged { slot, tex, header }).is_err() {
            // slots bound the queue, so this should not happen; count it if it does
            let _ = self.init.free_tx.send(slot);
            sh.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        sh.recorded.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.init
            .shared
            .fail("the capture item closed (monitor disconnected?)".into());
        Ok(())
    }
}

/// A sector-aligned buffer for unbuffered writes.
struct Chunk {
    ptr: *mut u8,
    /// Logical bytes in use.
    len: usize,
}

// SAFETY: the buffer is exclusively owned and moves between threads whole.
unsafe impl Send for Chunk {}

impl Chunk {
    fn layout() -> std::alloc::Layout {
        std::alloc::Layout::from_size_align(CHUNK, SECTOR).expect("chunk layout")
    }

    fn new() -> Chunk {
        let ptr = unsafe { std::alloc::alloc_zeroed(Self::layout()) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(Self::layout());
        }
        Chunk { ptr, len: 0 }
    }

    fn spare(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(self.len), CHUNK - self.len) }
    }

    /// The logical bytes rounded up to a whole sector; the pad is truncated at close.
    fn padded(&mut self) -> &[u8] {
        let n = self.len.div_ceil(SECTOR) * SECTOR;
        unsafe {
            std::ptr::write_bytes(self.ptr.add(self.len), 0, n - self.len);
            std::slice::from_raw_parts(self.ptr, n)
        }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, Self::layout()) };
    }
}

/// Packs records into chunks and hands full ones to the disk thread.
struct Sink {
    cur: Option<Chunk>,
    full_tx: Sender<Chunk>,
    empty_rx: Receiver<Chunk>,
}

impl Sink {
    fn append(&mut self, mut bytes: &[u8]) -> Result<(), String> {
        while !bytes.is_empty() {
            let cur = match self.cur.as_mut() {
                Some(c) => c,
                None => {
                    let c = self
                        .empty_rx
                        .recv()
                        .map_err(|_| "the disk writer stopped".to_string())?;
                    self.cur.insert(c)
                }
            };
            let spare = cur.spare();
            let n = spare.len().min(bytes.len());
            spare[..n].copy_from_slice(&bytes[..n]);
            cur.len += n;
            bytes = &bytes[n..];
            if cur.len == CHUNK {
                let full = self.cur.take().expect("current chunk");
                self.full_tx
                    .send(full)
                    .map_err(|_| "the disk writer stopped".to_string())?;
            }
        }
        Ok(())
    }

    fn finish(mut self) {
        if let Some(c) = self.cur.take() {
            if c.len > 0 {
                let _ = self.full_tx.send(c);
            }
        }
    }
}

/// One frame on its way through the compress workers. Jobs come from a fixed pool and are reused,
/// so nothing is allocated per frame once warm.
struct Job {
    seq: u64,
    header: [u8; HEADER],
    raw: Vec<u8>,
    out: Vec<u8>,
    word: FrameWord,
    /// The bytes to store are `out[..stored_len]` (LZ4) rather than `raw` (stored raw).
    in_out: bool,
    compress_ns: u64,
}

/// Where the mapper (or the synthetic packer) puts each picture.
enum Feeder {
    /// `--raw`: straight into the v2 fixed-size record stream.
    Raw(Sink, Vec<u8>),
    /// Into a pooled job for the compress workers. Waiting for a job holds a staging texture, so
    /// backpressure reaches the capture callback as a skip, never as a wait.
    Compressed {
        seq: u64,
        pool_rx: Receiver<Job>,
        work_tx: Sender<Job>,
        shared: Arc<Shared>,
    },
}

impl Feeder {
    /// One picture: `rows` rows of `row` bytes, `pitch` apart in `src`.
    fn push(
        &mut self,
        header: &[u8; HEADER],
        src: &[u8],
        pitch: usize,
        row: usize,
        rows: usize,
    ) -> Result<(), String> {
        match self {
            Feeder::Raw(sink, packed) => {
                if pitch == row {
                    sink.append(header)
                        .and_then(|_| sink.append(&src[..row * rows]))
                } else {
                    packed.resize(row * rows, 0);
                    for y in 0..rows {
                        packed[y * row..(y + 1) * row]
                            .copy_from_slice(&src[y * pitch..y * pitch + row]);
                    }
                    sink.append(header).and_then(|_| sink.append(packed))
                }
            }
            Feeder::Compressed {
                seq,
                pool_rx,
                work_tx,
                shared,
            } => {
                let mut job = pool_rx
                    .recv()
                    .map_err(|_| "the compress pool closed".to_string())?;
                job.raw.resize(row * rows, 0);
                if pitch == row {
                    job.raw.copy_from_slice(&src[..row * rows]);
                } else {
                    for y in 0..rows {
                        job.raw[y * row..(y + 1) * row]
                            .copy_from_slice(&src[y * pitch..y * pitch + row]);
                    }
                }
                job.header = *header;
                job.seq = *seq;
                *seq += 1;
                let n = shared.submitted.fetch_add(1, Ordering::Relaxed) + 1;
                let in_flight = n.saturating_sub(shared.packed.load(Ordering::Relaxed));
                shared.queue_hw.fetch_max(in_flight, Ordering::Relaxed);
                work_tx
                    .send(job)
                    .map_err(|_| "the compress workers stopped".to_string())
            }
        }
    }

    fn finish(self) {
        match self {
            Feeder::Raw(sink, _) => sink.finish(),
            // dropping the work sender lets the workers drain and exit, then the packer
            Feeder::Compressed { work_tx, .. } => drop(work_tx),
        }
    }
}

/// What the packer reports at the end.
struct Packed {
    frames: u64,
    index_sector: u32,
    /// Logical end of the file (after the index).
    end: u64,
    compress_ns: Vec<u64>,
}

/// Start `threads` compress workers and the packer. `jobs` bounds what is in flight between the
/// mapper and the packer.
fn spawn_compression(
    shared: &Arc<Shared>,
    sink: Sink,
    threads: usize,
    jobs: usize,
    payload: usize,
    max_frames: usize,
) -> std::io::Result<(Feeder, std::thread::JoinHandle<Packed>)> {
    let (pool_tx, pool_rx) = bounded::<Job>(jobs);
    for _ in 0..jobs {
        let _ = pool_tx.send(Job {
            seq: 0,
            header: [0; HEADER],
            raw: vec![0; payload],
            out: vec![0; lz4_flex::block::get_maximum_output_size(payload)],
            word: FrameWord::default(),
            in_out: false,
            compress_ns: 0,
        });
    }
    let (work_tx, work_rx) = bounded::<Job>(jobs);
    let (done_tx, done_rx) = bounded::<Job>(jobs);
    for i in 0..threads {
        let work_rx = work_rx.clone();
        let done_tx = done_tx.clone();
        std::thread::Builder::new()
            .name(format!("record-lz4-{i}"))
            .spawn(move || {
                for mut job in work_rx {
                    let t = Instant::now();
                    let (word, in_out) = compress_frame(&job.raw, &mut job.out);
                    job.compress_ns = t.elapsed().as_nanos() as u64;
                    job.word = word;
                    job.in_out = in_out;
                    if done_tx.send(job).is_err() {
                        return;
                    }
                }
            })?;
    }
    drop(done_tx);
    let packer = {
        let shared = Arc::clone(shared);
        std::thread::Builder::new()
            .name("record-pack".into())
            .spawn(move || run_packer(shared, sink, done_rx, pool_tx, jobs, max_frames))?
    };
    Ok((
        Feeder::Compressed {
            seq: 0,
            pool_rx,
            work_tx,
            shared: Arc::clone(shared),
        },
        packer,
    ))
}

/// Writes finished jobs in capture order, builds the index and appends it at the end.
fn run_packer(
    shared: Arc<Shared>,
    mut sink: Sink,
    done_rx: Receiver<Job>,
    pool_tx: Sender<Job>,
    jobs: usize,
    max_frames: usize,
) -> Packed {
    let mut order: Reorder<Job> = Reorder::new(jobs);
    let mut index = IndexBuilder::new(max_frames);
    let mut compress_ns = Vec::with_capacity(max_frames);
    let mut failed = false;
    'outer: for job in done_rx {
        if let Err(job) = order.insert(job.seq, job) {
            shared.fail(format!(
                "frame {} arrived outside the reorder window (next {})",
                job.seq,
                order.next_seq()
            ));
            failed = true;
            break;
        }
        while let Some(job) = order.pop() {
            let stored: &[u8] = if job.in_out {
                &job.out[..job.word.stored_len as usize]
            } else {
                &job.raw
            };
            let r = sink
                .append(&job.header)
                .and_then(|_| sink.append(&job.word.to_bytes()))
                .and_then(|_| sink.append(stored));
            if let Err(e) = r {
                shared.fail(e);
                failed = true;
                break 'outer;
            }
            let submit = u64::from_le_bytes(job.header[0..8].try_into().unwrap_or([0; 8]));
            index.add(submit, job.word.stored_len);
            compress_ns.push(job.compress_ns);
            shared
                .raw_bytes
                .fetch_add(job.raw.len() as u64, Ordering::Relaxed);
            shared.stored_bytes.fetch_add(
                (VAR_RECORD_HEAD + job.word.stored_len as usize) as u64,
                Ordering::Relaxed,
            );
            shared.packed.fetch_add(1, Ordering::Relaxed);
            let _ = pool_tx.send(job);
        }
    }
    let frames = index.records.len() as u64;
    let mut index_sector = 0;
    let mut end = index.pos;
    if !failed {
        let mut tail = Vec::new();
        match index.tail(&mut tail) {
            Ok(sector) => match sink.append(&tail) {
                Ok(()) => {
                    index_sector = sector;
                    end = sector as u64 * INDEX_ALIGN
                        + INDEX_HEADER as u64
                        + frames * INDEX_ENTRY as u64;
                }
                Err(e) => shared.fail(e),
            },
            Err(e) => shared.fail(e),
        }
    }
    sink.finish();
    Packed {
        frames,
        index_sector,
        end,
        compress_ns,
    }
}

/// The mapper: waits for the GPU copy, then copies the picture rows (pitch padding removed) and
/// the record header into the sink. Returns the slot to the free list.
fn run_mapper(
    shared: Arc<Shared>,
    rx: Receiver<Staged>,
    free_tx: Sender<usize>,
    setup_rx: Receiver<(ID3D11DeviceContext, String)>,
    mut feeder: Feeder,
    width: u32,
    height: u32,
) {
    let Ok((context, _)) = setup_rx.recv() else {
        feeder.finish();
        return;
    };
    let row = width as usize * 4;
    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(m) => m,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if shared.capture_done.load(Ordering::Relaxed) && rx.is_empty() {
                    break;
                }
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let started = Instant::now();
        let ok = loop {
            match unsafe {
                context.Map(
                    &msg.tex,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            } {
                Ok(()) => break true,
                Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                    if started.elapsed() > Duration::from_secs(2) {
                        shared.fail("a staging copy did not finish within 2 s".into());
                        break false;
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                Err(e) => {
                    shared.fail(format!("Map: {e}"));
                    break false;
                }
            }
        };
        if !ok {
            let _ = free_tx.send(msg.slot);
            continue;
        }
        let pitch = mapped.RowPitch as usize;
        let src = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * height as usize)
        };
        let result = feeder.push(&msg.header, src, pitch, row, height as usize);
        unsafe { context.Unmap(&msg.tex, 0) };
        let _ = free_tx.send(msg.slot);
        if let Err(e) = result {
            shared.fail(e);
            break;
        }
    }
    feeder.finish();
}

/// The disk thread: sequential unbuffered writes of whole sectors.
fn run_disk(
    shared: Arc<Shared>,
    mut file: File,
    full_rx: Receiver<Chunk>,
    empty_tx: Sender<Chunk>,
) -> std::io::Result<u64> {
    let mut logical = 0u64;
    for mut c in full_rx {
        let len = c.len;
        let t = Instant::now();
        let r = file.write_all(c.padded());
        shared
            .write_ns
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if let Err(e) = r {
            shared.fail(format!("write: {e}"));
            return Err(e);
        }
        logical += len as u64;
        shared.written.store(logical, Ordering::Relaxed);
        c.len = 0;
        let _ = empty_tx.send(c);
    }
    Ok(logical)
}

enum Source {
    Wgc(crate::windows_utils::windows_capture::capture::CaptureControl<Recorder, anyhow::Error>),
    Synthetic(std::thread::JoinHandle<()>),
}

#[allow(clippy::too_many_arguments)]
/// The synthetic source: one frame per period into a free slot, or SKIPPED when none is free.
fn spawn_generator(
    shared: Arc<Shared>,
    tx: Sender<(usize, [u8; HEADER])>,
    free_rx: Receiver<usize>,
    bufs: Arc<Vec<Mutex<Vec<u8>>>>,
    width: u32,
    height: u32,
    fps: u32,
    content: Content,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("record-synthetic".into())
        .spawn(move || {
            let mut pic = vec![0u8; width as usize * height as usize * 4];
            match content {
                Content::Noise => crate::streamer::synthetic::fill(
                    &mut pic,
                    width,
                    height,
                    0,
                    crate::streamer::synthetic::Workload::Noise,
                ),
                Content::Desktop => desktop_base(&mut pic, width as usize, height as usize),
            }
            let period = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
            while !shared.armed.load(Ordering::Relaxed) {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let t0 = Instant::now();
            let mut k = 0u32;
            while !shared.stop.load(Ordering::Relaxed) {
                let due = t0 + period * k;
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                k += 1;
                let entry = host_now_ns();
                shared.delivered.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut v) = shared.delivered_ns.lock() {
                    v.push(entry);
                }
                let Ok(slot) = free_rx.try_recv() else {
                    shared.skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                if let Ok(mut b) = bufs[slot].lock() {
                    b.copy_from_slice(&pic);
                    if content == Content::Desktop {
                        desktop_motion(&mut b, width as usize, height as usize, k);
                    }
                    // a frame number in the first pixel so consecutive frames differ
                    b[..4].copy_from_slice(&k.to_le_bytes());
                }
                let h = record_header(entry, 0, width, height, false, 0);
                if tx.try_send((slot, h)).is_err() {
                    shared.skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                shared.recorded.fetch_add(1, Ordering::Relaxed);
            }
        })
}

fn fill_rect(buf: &mut [u8], w: usize, h: usize, r: (usize, usize, usize, usize), bgra: [u8; 4]) {
    let (x0, y0, rw, rh) = r;
    for y in y0..(y0 + rh).min(h) {
        for x in x0..(x0 + rw).min(w) {
            buf[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&bgra);
        }
    }
}

/// A deterministic still desktop: wallpaper, taskbar, and three windows of glyph-like text.
fn desktop_base(buf: &mut [u8], w: usize, h: usize) {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    fill_rect(buf, w, h, (0, 0, w, h), [0x7A, 0x4B, 0x2E, 0xFF]);
    let bar = 48.min(h / 8);
    fill_rect(buf, w, h, (0, h - bar, w, bar), [0x20, 0x20, 0x20, 0xFF]);
    let windows = [
        (w * 5 / 100, h * 6 / 100, w / 2, h * 6 / 10),
        (w * 40 / 100, h * 20 / 100, w * 55 / 100, h * 55 / 100),
        (w * 10 / 100, h * 55 / 100, w * 35 / 100, h * 30 / 100),
    ];
    // a small glyph set, so text repeats like real text does
    let glyphs: Vec<[u16; 12]> = (0..64)
        .map(|_| {
            let mut g = [0u16; 12];
            for row in g.iter_mut().skip(2).take(9) {
                *row = (rnd() as u16) & 0x3E & (rnd() as u16 | 0x14);
            }
            g
        })
        .collect();
    for &(x, y, ww, wh) in &windows {
        fill_rect(buf, w, h, (x, y, ww, wh), [0xFF, 0xFF, 0xFF, 0xFF]);
        fill_rect(buf, w, h, (x, y, ww, 30.min(wh)), [0xE6, 0xE1, 0xDC, 0xFF]);
        let mut ty = y + 40;
        while ty + 12 < (y + wh).min(h) {
            let mut tx = x + 10;
            let line_end = (x + ww).saturating_sub(10 + (rnd() % (ww as u64 / 3 + 1)) as usize);
            while tx + 7 < line_end.min(w) {
                let word = 2 + (rnd() % 8) as usize;
                for _ in 0..word {
                    if tx + 7 >= line_end.min(w) {
                        break;
                    }
                    let g = &glyphs[(rnd() % 64) as usize];
                    for (gy, bits) in g.iter().enumerate() {
                        for gx in 0..7 {
                            if bits & (1 << gx) != 0 {
                                let o = ((ty + gy) * w + tx + gx) * 4;
                                buf[o..o + 4].copy_from_slice(&[0x22, 0x22, 0x22, 0xFF]);
                            }
                        }
                    }
                    tx += 7;
                }
                tx += 7;
            }
            ty += 18;
        }
    }
}

/// Three rectangles moving over the desktop on fixed paths.
fn desktop_motion(buf: &mut [u8], w: usize, h: usize, k: u32) {
    let colors = [
        [0x30, 0x90, 0xE0, 0xFF],
        [0x40, 0xC0, 0x50, 0xFF],
        [0xD0, 0x50, 0x90, 0xFF],
    ];
    for (i, c) in colors.iter().enumerate() {
        let (rw, rh) = (w / 10, h / 9);
        let t = k as f64 * 0.021 * (i as f64 + 1.0);
        let x = ((w - rw) as f64 * 0.5 * (1.0 + (t + i as f64).sin())) as usize;
        let y = ((h - rh) as f64 * 0.5 * (1.0 + (t * 1.3 + i as f64 * 2.0).cos())) as usize;
        fill_rect(buf, w, h, (x, y, rw, rh), *c);
        fill_rect(
            buf,
            w,
            h,
            (x + 4, y + 4, rw - 8, 2),
            [0xFF, 0xFF, 0xFF, 0xFF],
        );
    }
}

/// The packer for the synthetic source: `run_mapper` without the GPU map.
fn run_synthetic_packer(
    shared: Arc<Shared>,
    rx: Receiver<(usize, [u8; HEADER])>,
    free_tx: Sender<usize>,
    bufs: Arc<Vec<Mutex<Vec<u8>>>>,
    mut feeder: Feeder,
    row: usize,
    rows: usize,
) {
    for (slot, header) in rx {
        let r = match bufs[slot].lock() {
            Ok(b) => feeder.push(&header, &b, row, row, rows),
            Err(_) => Err("slot lock poisoned".to_string()),
        };
        let _ = free_tx.send(slot);
        if let Err(e) = r {
            shared.fail(e);
            break;
        }
    }
    feeder.finish();
}

fn gap_stats(times: &[u64]) -> (usize, f64, f64) {
    let gaps: Vec<u64> = times
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .collect();
    if gaps.is_empty() {
        return (0, 0.0, 0.0);
    }
    let mean = gaps.iter().sum::<u64>() as f64 / gaps.len() as f64 / 1e6;
    let max = *gaps.iter().max().unwrap_or(&0) as f64 / 1e6;
    (gaps.len(), mean, max)
}

pub fn main(args: &[String]) -> i32 {
    let o = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("error: {e}");
            }
            eprintln!("usage: ScreenExtend bench record [--monitor primary|<n>] [--seconds N] [--fps N] [--out <file>] [--countdown N] [--raw] [--compress-threads N] [--synthetic-source [--synthetic-content desktop|noise]] | --verify <file>");
            return if e.is_empty() { 0 } else { 2 };
        }
    };
    if let Some(p) = &o.verify {
        return match verify_file(p) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        };
    }
    match record(o) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn record(o: Opts) -> Result<(), String> {
    // before any monitor geometry is read, as a session does; all of it is per-process
    crate::streamer::platform::set_dpi_awareness();
    crate::streamer::platform::apply_process_tuning();

    let monitor = match o.monitor {
        None => Monitor::primary(),
        Some(n) => Monitor::from_index(n),
    }
    .map_err(|e| format!("monitor: {e}"))?;
    let width = monitor.width().map_err(|e| format!("monitor width: {e}"))?;
    let height = monitor
        .height()
        .map_err(|e| format!("monitor height: {e}"))?;
    let hz = monitor.refresh_rate().unwrap_or(0);
    let name = monitor.device_name().unwrap_or_default();
    let out = match o.out {
        Some(p) => p,
        None => default_out()?,
    };
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        if !parent.is_dir() {
            return Err(format!("{} does not exist", parent.display()));
        }
    }

    let header = FileHeader::new(PixelFormat::Bgra, width, height, o.fps);
    let var_header = VarFileHeader::new(PixelFormat::Bgra, width, height, o.fps, CODEC_LZ4);
    let rec = header.rec_size;
    let max_frames = (o.seconds * o.fps as f64).ceil() as u64 + o.fps as u64;
    // worst case: incompressible frames are stored raw, plus the frame words and the index
    let need = if o.raw {
        FILE_HEADER as u64 + rec * max_frames
    } else {
        FILE_HEADER as u64
            + (rec + 16) * max_frames
            + INDEX_ALIGN
            + INDEX_HEADER as u64
            + INDEX_ENTRY as u64 * max_frames
    };
    let free =
        sys::free_disk_bytes(&out).ok_or("cannot read the free space on the target volume")?;
    if free < need + SPARE_BYTES {
        return Err(format!(
            "the take could need {:.1} GB and the volume has {:.1} GB free; refusing to leave less \
             than {} GB",
            need as f64 / 1e9,
            free as f64 / 1e9,
            SPARE_BYTES / 1_000_000_000
        ));
    }

    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    use std::os::windows::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(&out)
        .map_err(|e| format!("create {}: {e}", out.display()))?;
    // reserving up front lets the file grow by extent rather than per write
    let _ = file.set_len(need.div_ceil(SECTOR as u64) * SECTOR as u64);

    let shared = Arc::new(Shared::default());
    if let Ok(mut v) = shared.delivered_ns.lock() {
        v.reserve(max_frames as usize * 2);
    }
    if let Ok(mut v) = shared.srt_minus_entry.lock() {
        v.reserve(max_frames as usize * 2);
    }
    let frame_bytes = width as usize * height as usize * 4;
    let slots = (STAGING_BUDGET / frame_bytes.max(1)).clamp(4, 64);
    let (tx, rx) = bounded::<Staged>(slots);
    let (free_tx, free_rx) = bounded::<usize>(slots);
    let (setup_tx, setup_rx) = bounded(1);
    let (full_tx, full_rx) = bounded::<Chunk>(CHUNKS);
    let (empty_tx, empty_rx) = bounded::<Chunk>(CHUNKS + 1);
    for _ in 0..CHUNKS {
        let _ = empty_tx.send(Chunk::new());
    }
    // the header is rewritten with the frame count at close
    let mut sink = Sink {
        cur: None,
        full_tx,
        empty_rx,
    };
    if o.raw {
        sink.append(&header.to_bytes())?;
    } else {
        sink.append(&var_header.to_bytes())?;
    }

    let disk = {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("record-disk".into())
            .spawn(move || run_disk(shared, file, full_rx, empty_tx))
            .map_err(|e| e.to_string())?
    };
    // one job per worker, one queued behind each, and slack for the frame the packer awaits
    let jobs = (o.compress_threads * 2 + 2).max(4);
    let (feeder, packer) = if o.raw {
        (Feeder::Raw(sink, Vec::new()), None)
    } else {
        let (f, p) = spawn_compression(
            &shared,
            sink,
            o.compress_threads,
            jobs,
            frame_bytes,
            max_frames as usize,
        )
        .map_err(|e| e.to_string())?;
        (f, Some(p))
    };
    println!(
        "[record] {}",
        if o.raw {
            "container v2, raw BGRA (--raw)".to_string()
        } else {
            format!(
                "container v3, LZ4 per frame, {} workers, {} jobs in flight at most ({:.0} MB)",
                o.compress_threads,
                jobs,
                (jobs * (frame_bytes + lz4_flex::block::get_maximum_output_size(frame_bytes)))
                    as f64
                    / 1e6
            )
        }
    );
    let source;
    let mapper;
    if o.synthetic {
        drop((tx, rx, free_rx, setup_tx, setup_rx));
        println!(
            "[record] SYNTHETIC SOURCE ({:?} content): no capture; a paced generator feeds the \
             same slot ring ({slots} slots, skip when full), packer and unbuffered disk writer",
            o.content
        );
        let (stx, srx) = bounded::<(usize, [u8; HEADER])>(slots);
        let bufs: Arc<Vec<Mutex<Vec<u8>>>> = Arc::new(
            (0..slots)
                .map(|_| Mutex::new(vec![0u8; frame_bytes]))
                .collect(),
        );
        let (ftx, frx) = bounded::<usize>(slots);
        for i in 0..slots {
            let _ = ftx.send(i);
        }
        mapper = {
            let shared = Arc::clone(&shared);
            let bufs = Arc::clone(&bufs);
            let ftx = ftx.clone();
            let row = width as usize * 4;
            let rows = height as usize;
            std::thread::Builder::new()
                .name("record-map".into())
                .spawn(move || run_synthetic_packer(shared, srx, ftx, bufs, feeder, row, rows))
                .map_err(|e| e.to_string())?
        };
        source = Source::Synthetic(
            spawn_generator(
                Arc::clone(&shared),
                stx,
                frx,
                bufs,
                width,
                height,
                o.fps,
                o.content,
            )
            .map_err(|e| e.to_string())?,
        );
        drop(free_tx);
    } else {
        mapper = {
            let shared = Arc::clone(&shared);
            let free_tx = free_tx.clone();
            std::thread::Builder::new()
                .name("record-map".into())
                .spawn(move || run_mapper(shared, rx, free_tx, setup_rx, feeder, width, height))
                .map_err(|e| e.to_string())?
        };

        let features = wgc_features();
        let interval = Duration::from_nanos(1_000_000_000 / o.fps as u64);
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Custom(interval),
            DirtyRegionSettings::ReportOnly,
            ColorFormat::Bgra8,
            Init {
                shared: Arc::clone(&shared),
                tx,
                free_tx,
                free_rx,
                setup_tx,
                monitor: monitor.as_raw_hmonitor() as isize,
                width,
                height,
                slots,
            },
        )
        .with_gpu_preference(crate::windows_utils::streamer::capture_gpu_preference(
            crate::streamer::config::EncoderVendor::Auto,
        ));

        println!(
            "[record] monitor {name} {width}x{height} @ {hz} Hz; cap {} fps (MinUpdateInterval \
             {:.2} ms{}); {:.1} s into {}",
            o.fps,
            interval.as_secs_f64() * 1e3,
            if features.min_update_interval {
                ""
            } else {
                ", NOT SUPPORTED on this build: uncapped"
            },
            o.seconds,
            out.display()
        );
        println!(
            "[record] WGC features: {}; {} staging textures ({:.0} MB); up to {:.1} GB on disk \
             ({:.0} GB free)",
            features.summary(),
            slots,
            (slots * frame_bytes) as f64 / 1e6,
            need as f64 / 1e9,
            free as f64 / 1e9
        );
        source = Source::Wgc(
            Recorder::start_free_threaded(settings).map_err(|e| format!("WGC start: {e}"))?,
        );
    }

    for i in (1..=o.countdown).rev() {
        println!("[record] starting in {i}...");
        std::thread::sleep(Duration::from_secs(1));
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
    }
    let t0 = Instant::now();
    shared.armed.store(true, Ordering::Relaxed);
    println!("[record] RECORDING");
    let mut next = 1u64;
    let ratio_of = |raw: u64, stored: u64| {
        if stored > 0 {
            raw as f64 / stored as f64
        } else {
            0.0
        }
    };
    while t0.elapsed().as_secs_f64() < o.seconds && !shared.stop.load(Ordering::Relaxed) {
        let left = o.seconds - t0.elapsed().as_secs_f64();
        std::thread::sleep(Duration::from_secs_f64(left.clamp(0.0, 0.05)));
        if t0.elapsed().as_secs() >= next {
            println!(
                "[record] {next:>4} s  frames={} skipped={} written={:.2} GB ratio={:.2}",
                shared.recorded.load(Ordering::Relaxed),
                shared.skipped.load(Ordering::Relaxed),
                shared.written.load(Ordering::Relaxed) as f64 / 1e9,
                if o.raw {
                    1.0
                } else {
                    ratio_of(
                        shared.raw_bytes.load(Ordering::Relaxed),
                        shared.stored_bytes.load(Ordering::Relaxed),
                    )
                }
            );
            next += 1;
        }
    }
    shared.stop.store(true, Ordering::Relaxed);
    let took = t0.elapsed().as_secs_f64();
    println!("[record] stopping");
    let stop_result = match source {
        Source::Wgc(control) => control.stop().map_err(|e| e.to_string()),
        Source::Synthetic(h) => h.join().map_err(|_| "the generator panicked".to_string()),
    };
    shared.capture_done.store(true, Ordering::Relaxed);
    let _ = mapper.join();
    let packed = match packer {
        Some(p) => Some(p.join().map_err(|_| "the packer panicked".to_string())?),
        None => None,
    };
    let logical = match disk.join() {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(format!("write: {e}")),
        Err(_) => return Err("the disk thread panicked".into()),
    };

    // cut off the sector pad and the reserve, then write the final header
    let (frames, header_bytes, expect) = match &packed {
        None => {
            let frames = shared.recorded.load(Ordering::Relaxed);
            let mut fh = header;
            fh.frame_count = frames;
            fh.flags = FILE_CLOSED;
            (frames, fh.to_bytes(), FILE_HEADER as u64 + frames * rec)
        }
        Some(p) => {
            let mut vh = var_header;
            vh.base.frame_count = p.frames;
            vh.base.flags = FILE_CLOSED;
            vh.index_sector = p.index_sector;
            (p.frames, vh.to_bytes(), p.end)
        }
    };
    let finalize = || -> std::io::Result<()> {
        let mut f = OpenOptions::new().write(true).open(&out)?;
        f.set_len(logical)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&header_bytes)?;
        f.sync_all()
    };
    finalize().map_err(|e| format!("finalize {}: {e}", out.display()))?;

    let error = shared
        .error
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let delivered = shared
        .delivered_ns
        .lock()
        .map(|v| v.clone())
        .unwrap_or_default();
    let (_, mean_gap, max_gap) = gap_stats(&delivered);
    let skipped = shared.skipped.load(Ordering::Relaxed);
    let write_s = shared.write_ns.load(Ordering::Relaxed) as f64 / 1e9;
    let mut srt = shared
        .srt_minus_entry
        .lock()
        .map(|v| v.clone())
        .unwrap_or_default();
    srt.sort_unstable();
    let pct = |p: f64| -> f64 {
        if srt.is_empty() {
            f64::NAN
        } else {
            srt[((srt.len() - 1) as f64 * p).round() as usize] as f64 / 1e6
        }
    };
    if !o.synthetic {
        println!(
            "[record] WGC SystemRelativeTime minus callback entry: min {:.2} ms, p50 {:.2}, max \
             {:.2} ({} frames); pre-roll frames {}; pool stale-dropped {}; dirty-empty {}; \
             cursor-moved {}",
            pct(0.0),
            pct(0.5),
            pct(1.0),
            srt.len(),
            shared.pre_roll.load(Ordering::Relaxed),
            shared.stale.load(Ordering::Relaxed),
            shared.dirty_empty.load(Ordering::Relaxed),
            shared.cursor_moved.load(Ordering::Relaxed),
        );
    }
    if logical != expect {
        eprintln!(
            "[record] WARNING: {logical} bytes written, {expect} expected for {frames} records"
        );
    }
    if let Err(e) = stop_result {
        eprintln!("[record] capture stop: {e}");
    }
    let raw_bytes = frames * frame_bytes as u64;
    let (ratio, cmp) = match &packed {
        None => (1.0, "compress_ms=-".to_string()),
        Some(p) => {
            let mut ns = p.compress_ns.clone();
            ns.sort_unstable();
            let at = |q: f64| -> f64 {
                if ns.is_empty() {
                    0.0
                } else {
                    ns[((ns.len() - 1) as f64 * q).round() as usize] as f64 / 1e6
                }
            };
            let mean = if ns.is_empty() {
                0.0
            } else {
                ns.iter().sum::<u64>() as f64 / ns.len() as f64 / 1e6
            };
            (
                ratio_of(raw_bytes, logical),
                format!(
                    "compress_ms_mean={mean:.2} compress_ms_p99={:.2} compress_ms_max={:.2} \
                     queue_hw={}",
                    at(0.99),
                    at(1.0),
                    shared.queue_hw.load(Ordering::Relaxed)
                ),
            )
        }
    };
    println!(
        "RESULT frames={frames} skipped={skipped} delivered={} seconds={took:.3} fps={:.1} \
         raw_bytes={raw_bytes} stored_bytes={logical} ratio={ratio:.2} {cmp} \
         disk_MBps={:.0} disk_busy_MBps={:.0} mean_gap_ms={mean_gap:.3} max_gap_ms={max_gap:.3} \
         size={width}x{height} file={}",
        shared.delivered.load(Ordering::Relaxed),
        frames as f64 / took.max(1e-9),
        logical as f64 / took.max(1e-9) / 1e6,
        if write_s > 0.0 {
            logical as f64 / write_s / 1e6
        } else {
            0.0
        },
        out.display()
    );
    match error {
        Some(e) => Err(format!("the take ended early: {e}")),
        None => Ok(()),
    }
}

/// `bench record --verify <file>`: read every frame of any container, check every CRC-32 and
/// (v3) the index against a walk of the records. `Err` on any mismatch.
fn verify_file(path: &Path) -> Result<(), String> {
    use super::super::clip::{scan, ClipReader};
    let t = Instant::now();
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    let info = scan(path)?;
    let mut problems: Vec<String> = Vec::new();
    let mut container = if info.layout.v2 {
        "v2 raw".to_string()
    } else {
        "legacy (macOS NV12)".to_string()
    };
    let mut index = "none (fixed-size records)".to_string();
    if let Some(table) = &info.table {
        container = format!(
            "v3 codec {} ({} LZ4, {} stored raw)",
            if table.codec == CODEC_LZ4 {
                "LZ4"
            } else {
                "none"
            },
            table.stored_lz4,
            table.stored_raw
        );
        index = if table.from_index {
            "present".to_string()
        } else {
            table
                .index_note
                .clone()
                .unwrap_or_else(|| "absent".to_string())
        };
        match super::clipfmt::scan_records(path, false) {
            Ok(walk) => {
                if table.from_index {
                    if walk.table.records != table.records {
                        let first = walk
                            .table
                            .records
                            .iter()
                            .zip(&table.records)
                            .position(|(a, b)| a != b)
                            .unwrap_or(walk.table.records.len().min(table.records.len()));
                        problems.push(format!(
                            "the index ({} frames) and a walk of the records ({}) disagree from \
                             frame {first}",
                            table.records.len(),
                            walk.table.records.len()
                        ));
                    } else {
                        index = format!("present, matches a walk of all {} records", info.frames);
                    }
                }
            }
            Err(e) => problems.push(format!("walking the records: {e}")),
        }
    }
    match info.layout.declared_frames {
        Some(n) if n as usize != info.frames => problems.push(format!(
            "the header declares {n} frames, {} were found",
            info.frames
        )),
        Some(_) => {}
        None if info.layout.v2 => index.push_str("; the take was never closed"),
        None => {}
    }
    let mut r = ClipReader::open(path, &info).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut read = 0usize;
    for i in 0..info.frames {
        match r.next(&mut buf) {
            Ok(()) => read += 1,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                problems.push(e.to_string());
            }
            Err(e) => {
                problems.push(format!("frame {i}: {e}"));
                break;
            }
        }
    }
    let raw_bytes = info.frames as u64 * info.layout.payload as u64;
    let secs = t.elapsed().as_secs_f64();
    println!(
        "VERIFY file={} container={container} size={}x{} frames={} read_ok={read} \
         declared={} index={index} raw_bytes={raw_bytes} stored_bytes={len} ratio={:.2} \
         seconds={secs:.1} read_MBps={:.0} problems={}",
        path.display(),
        info.width,
        info.height,
        info.frames,
        info.layout
            .declared_frames
            .map_or("-".to_string(), |n| n.to_string()),
        raw_bytes as f64 / len.max(1) as f64,
        len as f64 / secs.max(1e-9) / 1e6,
        problems.len()
    );
    for p in problems.iter().take(20) {
        println!("  PROBLEM {p}");
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} problem(s) in {}",
            problems.len(),
            path.display()
        ))
    }
}

/// Check that `path` is a complete, closed take.
pub fn verify(path: &Path) -> Result<super::super::clip::ClipInfo, String> {
    let info = super::super::clip::scan(path)?;
    match info.layout.declared_frames {
        Some(n) if n as usize == info.frames => Ok(info),
        Some(n) => Err(format!(
            "{} complete records but the header declares {n}",
            info.frames
        )),
        None => Err("the take was never closed (no frame count in its header)".into()),
    }
}

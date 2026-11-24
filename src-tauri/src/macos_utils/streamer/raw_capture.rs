//! Diagnostic: record the exact frames handed to the encoder, so one real interaction can be
//! replayed in real time against any encoder configuration.
//!
//! Off unless `SE_RAW_RECORD=<path>` is set, and even then nothing is written until
//! `<path>.arm` exists; removing that file stops the take. The file holds raw screen contents, so
//! it is created owner-only and is never sent anywhere.
//!
//! Records are little-endian: `submit_ns u64, os_ns u64, width u32, height u32, force_idr u32,
//! repeated u32`, then a tightly packed 4:2:0 biplanar picture (`w*h*3/2` bytes).
//!
//! A take is replayed by the bench harness (`ScreenExtend bench --recording <file>`, built with
//! `--features bench`; `src/bench/clip.rs` reads this format), which replaced the ad-hoc
//! `replay_recorded_take` test that used to live here.
//!
//! **Compiled only with `--features bench`**: a shipped build has no raw-frame recorder at all,
//! so no environment variable can make it write screen contents to disk. Even in a bench build
//! the take is opened with `O_CREAT | O_EXCL | O_NOFOLLOW` (an existing file or a symlink at the
//! path is refused, never truncated or followed), mode 0600, and only while a regular
//! `<path>.arm` file owned by this process's effective uid exists.
#![cfg(feature = "bench")]

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Duration;

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};

use crate::streamer::telemetry::FrameTiming;

pub const HEADER: usize = 32;
/// Frames that may wait for the disk: about 400 MB at 1080p.
const QUEUE: usize = 128;

static ARMED: AtomicBool = AtomicBool::new(false);
static SKIPPED: AtomicU64 = AtomicU64::new(0);

fn writer() -> Option<&'static Sender<Vec<u8>>> {
    static TX: OnceLock<Option<Sender<Vec<u8>>>> = OnceLock::new();
    TX.get_or_init(|| {
        let path = std::env::var("SE_RAW_RECORD")
            .ok()
            .filter(|p| !p.is_empty())?;
        let (tx, rx) = bounded::<Vec<u8>>(QUEUE);
        let spawned = std::thread::Builder::new()
            .name("raw-record".into())
            .spawn(move || {
                let arm = format!("{path}.arm");
                let mut file: Option<std::fs::File> = None;
                let mut frames = 0u64;
                let mut bytes = 0u64;
                let mut open_failed = false;
                loop {
                    let want = arm_is_ours(std::path::Path::new(&arm));
                    if want && file.is_none() {
                        match std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .custom_flags(libc::O_NOFOLLOW)
                            .mode(0o600)
                            .open(&path)
                        {
                            Ok(f) => {
                                file = Some(f);
                                (frames, bytes) = (0, 0);
                                SKIPPED.store(0, Relaxed);
                                tprintln!("[raw-record] recording");
                            }
                            Err(e) => {
                                // Once per arming, not once per 50 ms poll.
                                if !open_failed {
                                    open_failed = true;
                                    teprintln!(
                                        "[raw-record] cannot create the take (it must not exist \
                                         yet, and must not be a symlink): {e}"
                                    );
                                }
                            }
                        }
                    }
                    if !want {
                        open_failed = false;
                    }
                    ARMED.store(want && file.is_some(), Relaxed);
                    // Drain for one polling interval, or to empty once disarmed.
                    let until = std::time::Instant::now() + Duration::from_millis(50);
                    loop {
                        match rx.recv_timeout(
                            until.saturating_duration_since(std::time::Instant::now()),
                        ) {
                            Ok(buf) => {
                                if let Some(f) = file.as_mut() {
                                    if let Err(e) = f.write_all(&buf) {
                                        teprintln!("[raw-record] write failed: {e}");
                                        ARMED.store(false, Relaxed);
                                        return;
                                    }
                                    frames += 1;
                                    bytes += buf.len() as u64;
                                }
                                if want && std::time::Instant::now() >= until {
                                    break;
                                }
                            }
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    if !want && file.is_some() && rx.is_empty() {
                        file = None;
                        tprintln!(
                            "[raw-record] stopped frames={frames} bytes={bytes} skipped={}",
                            SKIPPED.load(Relaxed)
                        );
                    }
                }
            });
        spawned.ok().map(|_| tx)
    })
    .as_ref()
}

/// The arm file counts only if it is a regular file (not followed if it is a symlink) owned by
/// this process's effective uid: another local user must not be able to start a recording of
/// this user's screen by dropping a file next to the take.
fn arm_is_ours(arm: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(arm)
        .is_ok_and(|m| m.file_type().is_file() && m.uid() == unsafe { libc::geteuid() })
}

/// Copy one encoder input. Costs a relaxed load unless a take is running.
pub fn record(pb: &CVPixelBuffer, force_idr: bool, timing: &FrameTiming) {
    let Some(tx) = writer() else { return };
    if !ARMED.load(Relaxed) {
        return;
    }
    if tx.is_full() || unsafe { CVPixelBufferGetPlaneCount(pb) } != 2 {
        SKIPPED.fetch_add(1, Relaxed);
        return;
    }
    let (w, h) = unsafe { (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb)) };
    let mut buf = Vec::with_capacity(HEADER + w * h * 3 / 2);
    buf.extend_from_slice(&timing.encode_submit_ns.to_le_bytes());
    buf.extend_from_slice(&timing.capture_available_ns.to_le_bytes());
    for v in [w as u32, h as u32, force_idr as u32, timing.repeated as u32] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    let ok = unsafe {
        CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) == 0 && {
            for (plane, rows) in [(0, h), (1, h / 2)] {
                let base = CVPixelBufferGetBaseAddressOfPlane(pb, plane) as *const u8;
                let bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, plane);
                if !base.is_null() {
                    for y in 0..rows {
                        buf.extend_from_slice(std::slice::from_raw_parts(base.add(y * bpr), w));
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
            buf.len() == HEADER + w * (h + h / 2)
        }
    };
    if !ok || tx.try_send(buf).is_err() {
        SKIPPED.fetch_add(1, Relaxed);
    }
}

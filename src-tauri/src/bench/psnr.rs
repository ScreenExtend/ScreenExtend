//! Quality, after the timed part: decode what was emitted with `VTDecompressionSession` and
//! compare luma against the source.
//!
//! - `sharp`: each emitted picture against its own source frame.
//! - `shown`: at every source frame, the latest picture decoded at or before it against that
//!   source, so a dropped frame is charged for being stale.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::time::Instant;

use objc2_core_foundation::{CFNumber, CFRetained};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use serde_json::{json, Value as Json};

use super::cf;
use super::clip::{ClipInfo, ClipReader, Transform};
use super::metrics::low_summary;

#[repr(C)]
struct CallbackRecord {
    cb: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        i32,
        u32,
        *mut CVPixelBuffer,
        CMTime,
        CMTime,
    ),
    refcon: *mut c_void,
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        alloc: *const c_void,
        fmt: *const c_void,
        spec: *const c_void,
        attrs: *const c_void,
        cb: *const CallbackRecord,
        out: *mut *mut c_void,
    ) -> i32;
    fn VTDecompressionSessionDecodeFrame(
        s: *mut c_void,
        sample: *const c_void,
        flags: u32,
        refcon: *mut c_void,
        info: *mut u32,
    ) -> i32;
    fn VTDecompressionSessionWaitForAsynchronousFrames(s: *mut c_void) -> i32;
    fn VTDecompressionSessionInvalidate(s: *mut c_void);
    fn VTDecompressionSessionCanAcceptFormatDescription(s: *mut c_void, d: *const c_void) -> u8;
}

unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// Where the decode callback writes: the luma plane and its size.
struct Out {
    luma: Vec<u8>,
    w: usize,
    h: usize,
    ok: bool,
}

unsafe extern "C" fn decoded(
    _r: *mut c_void,
    out: *mut c_void,
    status: i32,
    _f: u32,
    img: *mut CVPixelBuffer,
    _pts: CMTime,
    _dur: CMTime,
) {
    let dst = &mut *(out as *mut Out);
    if status != 0 || img.is_null() {
        return;
    }
    let pb = &*img;
    CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
    let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
    let base = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *const u8;
    let bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
    if !base.is_null() {
        dst.luma.clear();
        for y in 0..h {
            dst.luma
                .extend_from_slice(std::slice::from_raw_parts(base.add(y * bpr), w));
        }
        dst.w = w;
        dst.h = h;
        dst.ok = true;
    }
    CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
}

pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let sse: u64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| {
            let d = *x as i32 - *y as i32;
            (d * d) as u64
        })
        .sum();
    let mse = sse as f64 / a.len().max(1) as f64;
    if mse < 0.065 {
        60.0
    } else {
        10.0 * (65025.0 / mse).log10()
    }
}

struct Decoder {
    session: *mut c_void,
    fmt: *const c_void,
}

impl Decoder {
    fn ensure(&mut self, d: *const c_void) -> Result<(), i32> {
        if !self.session.is_null()
            && (d == self.fmt
                || unsafe { VTDecompressionSessionCanAcceptFormatDescription(self.session, d) }
                    != 0)
        {
            return Ok(());
        }
        self.close();
        let cb = CallbackRecord {
            cb: decoded,
            refcon: ptr::null_mut(),
        };
        let num = CFNumber::new_i32(cf::PIXEL_420F as i32);
        let attrs = cf::dict(&[(unsafe { kCVPixelBufferPixelFormatTypeKey }, &num)]);
        let st = unsafe {
            VTDecompressionSessionCreate(
                ptr::null(),
                d,
                ptr::null(),
                CFRetained::as_ptr(&attrs).as_ptr().cast_const().cast(),
                &cb,
                &mut self.session,
            )
        };
        if st != 0 {
            self.session = ptr::null_mut();
            return Err(st);
        }
        self.fmt = d;
        Ok(())
    }

    fn close(&mut self) {
        if !self.session.is_null() {
            unsafe {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
            }
            self.session = ptr::null_mut();
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.close();
    }
}

/// `emitted`: (global source index, sample) in callback (decode) order.
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    clip: &Path,
    info: &ClipInfo,
    n_src: usize,
    loops: usize,
    transform: Transform,
    emitted: &[(usize, CFRetained<CMSampleBuffer>)],
    watch: &[usize],
) -> Json {
    let watched = |g: usize| watch.iter().any(|&w| g + 2 >= w && g <= w + 8);
    let mut watch_out: Vec<Json> = Vec::new();
    let total = n_src * loops;
    let mut pos_of: Vec<Option<usize>> = vec![None; total];
    for (p, (g, _)) in emitted.iter().enumerate() {
        if *g < total {
            pos_of[*g] = Some(p);
        }
    }
    let (ow, oh) = transform
        .out_dims(info.width, info.height)
        .unwrap_or((info.width, info.height));
    let mut dec = Decoder {
        session: ptr::null_mut(),
        fmt: ptr::null(),
    };
    let mut out = Out {
        luma: Vec::new(),
        w: 0,
        h: 0,
        ok: false,
    };
    let mut pictures: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    let (mut sharp, mut shown) = (Vec::new(), Vec::new());
    let (mut failed, mut size_mismatch, mut create_fail) = (0usize, 0usize, None::<i32>);
    let (mut decode_ns, mut decoded_n, mut not_shown) = (0u128, 0usize, 0usize);
    let mut next = 0usize;
    let (mut src, mut src_luma) = (Vec::new(), Vec::new());
    let started = Instant::now();

    for l in 0..loops {
        let mut reader = match ClipReader::open(clip, info) {
            Ok(r) => r,
            Err(e) => return json!({ "error": format!("reopen clip: {e}") }),
        };
        for i in 0..n_src {
            let g = l * n_src + i;
            if reader.next(&mut src).is_err() {
                return json!({ "error": format!("clip ended at frame {i}") });
            }
            transform.luma(
                &src[..info.width * info.height],
                info.width,
                info.height,
                &mut src_luma,
            );
            if let Some(p) = pos_of[g] {
                while next <= p {
                    let (gi, sb) = &emitted[next];
                    next += 1;
                    let d = cf::format_description(sb);
                    if let Err(st) = dec.ensure(d) {
                        create_fail.get_or_insert(st);
                        failed += 1;
                        continue;
                    }
                    out.ok = false;
                    let t = Instant::now();
                    let mut flags = 0u32;
                    let st = unsafe {
                        VTDecompressionSessionDecodeFrame(
                            dec.session,
                            cf::sb_ptr(sb),
                            0,
                            (&mut out as *mut Out).cast(),
                            &mut flags,
                        )
                    };
                    unsafe { VTDecompressionSessionWaitForAsynchronousFrames(dec.session) };
                    decode_ns += t.elapsed().as_nanos();
                    decoded_n += 1;
                    if st != 0 || !out.ok {
                        failed += 1;
                        continue;
                    }
                    if (out.w, out.h) != (ow, oh) {
                        size_mismatch += 1;
                        continue;
                    }
                    pictures.insert(*gi, std::mem::take(&mut out.luma));
                }
            }
            if let Some(pic) = pictures.get(&g) {
                let p = psnr(pic, &src_luma);
                sharp.push(p);
                if watched(g) {
                    watch_out.push(json!({ "frame": g, "sharp": (p * 100.0).round() / 100.0 }));
                }
            } else if watched(g) {
                watch_out.push(json!({ "frame": g, "sharp": Json::Null }));
            }
            let latest = pictures.range(..=g).next_back().map(|(k, _)| *k);
            match latest {
                Some(k) => {
                    shown.push(psnr(&pictures[&k], &src_luma));
                    let stale: Vec<usize> = pictures.range(..k).map(|(k, _)| *k).collect();
                    for s in stale {
                        pictures.remove(&s);
                    }
                }
                None => not_shown += 1,
            }
        }
    }
    json!({
        "sharp": low_summary(&sharp),
        "shown": low_summary(&shown),
        "decode_failed": failed,
        "decoder_create_status": create_fail,
        "size_mismatch": size_mismatch,
        "frames_before_first_picture": not_shown,
        "decoded": decoded_n,
        "decode_ms_mean": if decoded_n > 0 { decode_ns as f64 / decoded_n as f64 / 1e6 } else { 0.0 },
        "watch": watch_out,
        "wall_s": started.elapsed().as_secs_f64(),
    })
}

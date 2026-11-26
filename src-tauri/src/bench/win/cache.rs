//! A compressed copy of the part of a take a run needs, under the run's own cache directory, so
//! several feeders can read one take without each pulling raw pictures off the disk. Every frame
//! is LZ4-compressed on its own (container v3), with its record header, flags and times kept.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use serde_json::{json, Value as Json};

use super::super::clip::{self, ClipInfo};
use super::super::plan::RunSpec;
use super::clipfmt::{
    compress_frame, record_header, IndexBuilder, VarFileHeader, CODEC_LZ4, FILE_CLOSED,
};

/// Frames compressed together, in parallel, between two sequential writes.
const CHUNK: usize = 24;

/// `<out>/cache/take`, from the run's result path (`<out>/work/<id>.result.json`). A directory of
/// its own, so the runner's end-of-plan clip cleanup leaves it for the next plan in that `--out`.
pub fn cache_dir(spec: &RunSpec) -> Option<PathBuf> {
    let r = Path::new(spec.result_path.as_deref()?);
    Some(r.parent()?.parent()?.join("cache").join("take"))
}

/// Seconds a first `compressed_copy` may add to a run, for the watchdog. A multi-session pipe run
/// on a raw take is the only user.
pub fn estimate_s(r: &RunSpec, rec: Option<&ClipInfo>) -> f64 {
    let sessions = r
        .recipe
        .recipe
        .mix
        .as_ref()
        .map_or(r.sessions as usize, |m| m.len());
    let Some(info) = rec else { return 0.0 };
    if r.stage != "pipe" || r.workload.kind != "recording" || sessions < 2 || info.table.is_some() {
        return 0.0;
    }
    let fps = (info.frames as f64 / (info.duration_ns().max(1) as f64 / 1e9)).max(1.0);
    let n = r.workload.max_frames.unwrap_or(info.frames) as f64 + 7.0 * fps * sessions as f64;
    // about 1 GB/s of raw pictures read, compressed and written
    n.min(info.frames as f64) * info.layout.payload as f64 / 1e9
}

fn name_for(src: &Path, info: &ClipInfo) -> String {
    let len = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let key = format!("{}|{len}|{}", src.display(), info.frames);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("take-{h:016x}.seraw")
}

/// The first `frames` records of `src` as a v3 LZ4 file in `dir`, made once and reused by any
/// later run that needs no more frames than it holds. A take that is already compressed is
/// returned as it is.
pub fn compressed_copy(
    src: &Path,
    info: &ClipInfo,
    frames: usize,
    dir: &Path,
) -> Result<(PathBuf, Json), String> {
    if info.table.is_some() {
        return Ok((
            src.to_path_buf(),
            json!({ "used": false, "why": "the take is already compressed" }),
        ));
    }
    let frames = frames.clamp(1, info.frames);
    let out = dir.join(name_for(src, info));
    if let Ok(have) = clip::scan(&out) {
        if have.frames >= frames && have.table.is_some() {
            return Ok((
                out.clone(),
                json!({ "used": true, "reused": true, "frames": have.frames, "file": out }),
            ));
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cache dir: {e}"))?;
    let t0 = Instant::now();
    let tmp = out.with_extension("tmp");
    let lay = info.layout;
    let mut header = VarFileHeader::new(
        lay.format,
        info.width as u32,
        info.height as u32,
        lay.nominal_fps,
        CODEC_LZ4,
    );
    let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    f.write_all(&header.to_bytes()).map_err(|e| e.to_string())?;
    let mut reader =
        clip::ClipReader::open(src, info).map_err(|e| format!("open {}: {e}", src.display()))?;
    let mut idx = IndexBuilder::new(frames);
    let mut raw: Vec<Vec<u8>> = (0..CHUNK).map(|_| vec![0u8; lay.payload]).collect();
    let mut packed: Vec<Vec<u8>> = (0..CHUNK).map(|_| Vec::new()).collect();
    let (mut bytes_in, mut bytes_out) = (0u64, 0u64);
    let mut i = 0usize;
    while i < frames {
        let n = CHUNK.min(frames - i);
        for buf in raw.iter_mut().take(n) {
            reader
                .next_into(buf)
                .map_err(|e| format!("read record {i}: {e}"))?;
        }
        let words: Vec<_> = raw[..n]
            .par_iter()
            .zip(packed[..n].par_iter_mut())
            .map(|(r, out)| compress_frame(r, out))
            .collect();
        for (k, (word, in_out)) in words.into_iter().enumerate() {
            let g = i + k;
            let stored: &[u8] = if in_out {
                &packed[k][..word.stored_len as usize]
            } else {
                &raw[k]
            };
            let head = record_header(
                info.times[g],
                info.os_ns[g],
                info.width as u32,
                info.height as u32,
                info.idr[g],
                info.flags[g],
            );
            f.write_all(&head).map_err(|e| e.to_string())?;
            f.write_all(&word.to_bytes()).map_err(|e| e.to_string())?;
            f.write_all(stored).map_err(|e| e.to_string())?;
            idx.add(info.times[g], word.stored_len);
            bytes_in += lay.payload as u64;
            bytes_out += stored.len() as u64;
        }
        i += n;
    }
    let mut tail = Vec::new();
    header.index_sector = idx.tail(&mut tail)?;
    f.write_all(&tail).map_err(|e| e.to_string())?;
    header.base.frame_count = frames as u64;
    header.base.flags |= FILE_CLOSED;
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    f.write_all(&header.to_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, &out).map_err(|e| format!("rename cache: {e}"))?;
    let secs = t0.elapsed().as_secs_f64();
    Ok((
        out.clone(),
        json!({
            "used": true,
            "reused": false,
            "frames": frames,
            "file": out,
            "bytes_in": bytes_in,
            "bytes_out": bytes_out,
            "ratio": if bytes_out > 0 { bytes_in as f64 / bytes_out as f64 } else { 0.0 },
            "seconds": secs,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::clipfmt::{FileHeader, PixelFormat};
    use super::*;

    /// A small v2 take through the cache and back: same pictures, times, flags and IDR marks.
    #[test]
    fn a_compressed_copy_reads_back_identical() {
        let dir = std::env::temp_dir().join(format!(
            "se-bench-cache-{}-{}",
            std::process::id(),
            crate::streamer::audio::host_now_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("take.seraw");
        let (w, h, n) = (64u32, 32u32, 50usize);
        let mut hdr = FileHeader::new(PixelFormat::Bgra, w, h, 120);
        hdr.frame_count = n as u64;
        hdr.flags = FILE_CLOSED;
        let mut f = File::create(&src).unwrap();
        f.write_all(&hdr.to_bytes()).unwrap();
        let mut pics = Vec::new();
        for i in 0..n {
            // flat bands with a moving edge: compresses, and differs per frame
            let pic: Vec<u8> = (0..(w * h * 4) as usize)
                .map(|b| {
                    if (b / 4) % w as usize > i {
                        200
                    } else {
                        (i * 3) as u8
                    }
                })
                .collect();
            let flags = (i as u32 % 3) << 1;
            f.write_all(&record_header(
                i as u64 * 8_333_333,
                7,
                w,
                h,
                i == 10,
                flags,
            ))
            .unwrap();
            f.write_all(&pic).unwrap();
            pics.push(pic);
        }
        drop(f);
        let info = clip::scan(&src).unwrap();
        let (copy, rep) = compressed_copy(&src, &info, 40, &dir.join("cache")).unwrap();
        assert_eq!(rep["used"], true);
        assert!(rep["ratio"].as_f64().unwrap() > 1.0, "{rep}");
        let c = clip::scan(&copy).unwrap();
        assert_eq!(c.frames, 40);
        assert!(c.table.is_some());
        assert_eq!(c.times[..], info.times[..40]);
        assert_eq!(c.flags[..], info.flags[..40]);
        assert_eq!(c.idr[..], info.idr[..40]);
        let mut r = clip::ClipReader::open(&copy, &c).unwrap();
        let mut buf = vec![0u8; c.layout.payload];
        for (i, pic) in pics.iter().take(40).enumerate() {
            r.next_into(&mut buf).unwrap();
            assert_eq!(&buf, pic, "frame {i}");
        }
        // seek and stored reads agree with the sequential ones
        r.seek_frame(17).unwrap();
        let mut stored = Vec::new();
        let word = r.next_stored(&mut stored).unwrap().unwrap();
        let mut dec = Vec::new();
        super::super::clipfmt::decode_frame(17, &word, &stored, c.layout.payload, &mut dec)
            .unwrap();
        assert_eq!(dec, pics[17]);
        let (again, rep2) = compressed_copy(&src, &info, 30, &dir.join("cache")).unwrap();
        assert_eq!(again, copy);
        assert_eq!(rep2["reused"], true);
        let (bigger, rep3) = compressed_copy(&src, &info, 45, &dir.join("cache")).unwrap();
        assert_eq!(bigger, copy);
        assert_eq!(rep3["reused"], false);
        assert_eq!(clip::scan(&bigger).unwrap().frames, 45);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

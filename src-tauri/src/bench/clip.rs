//! Clips: the raw record format written by `macos_utils::streamer::raw_capture`, read back here,
//! plus deterministic synthetic clips rendered into the same format so a Mac with nothing but
//! this executable can run the same engine.
//!
//! Record (little-endian): `submit_ns u64, os_ns u64, width u32, height u32, force_idr u32,
//! repeated u32`, then `w*h` luma and `w*h/2` interleaved CbCr ('420f', full range).

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::macos_utils::streamer::raw_capture::HEADER;

use super::plan::Workload;

/// The schedule of a clip, read from its headers without touching the pictures.
#[derive(Clone, Debug)]
pub struct ClipInfo {
    pub width: usize,
    pub height: usize,
    pub frames: usize,
    pub rec_size: u64,
    /// Recorded submit times, ns, relative to the first record.
    pub times: Vec<u64>,
    pub idr: Vec<bool>,
    pub repeated: Vec<bool>,
}

impl ClipInfo {
    pub fn duration_ns(&self) -> u64 {
        self.times.last().copied().unwrap_or(0)
    }

    pub fn median_gap_ns(&self) -> u64 {
        let mut g: Vec<u64> = self.times.windows(2).map(|w| w[1] - w[0]).collect();
        if g.is_empty() {
            return 16_666_667;
        }
        g.sort_unstable();
        g[g.len() / 2].max(1)
    }
}

fn header(h: &[u8]) -> (u64, usize, usize, bool, bool) {
    let u64at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
    let u32at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
    (
        u64at(0),
        u32at(16) as usize,
        u32at(20) as usize,
        u32at(24) != 0,
        u32at(28) != 0,
    )
}

pub fn scan(path: &Path) -> Result<ClipInfo, String> {
    let mut f = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    let mut h = [0u8; HEADER];
    f.read_exact(&mut h)
        .map_err(|e| format!("first header: {e}"))?;
    let (t0, w, ht, _, _) = header(&h);
    if w == 0 || ht == 0 || w % 2 != 0 || ht % 2 != 0 || w > 16384 || ht > 16384 {
        return Err(format!("implausible picture size {w}x{ht}"));
    }
    let rec_size = (HEADER + w * ht * 3 / 2) as u64;
    let frames = (len / rec_size) as usize;
    let mut info = ClipInfo {
        width: w,
        height: ht,
        frames,
        rec_size,
        times: Vec::with_capacity(frames),
        idr: Vec::with_capacity(frames),
        repeated: Vec::with_capacity(frames),
    };
    let mut last = t0;
    for i in 0..frames {
        f.seek(SeekFrom::Start(i as u64 * rec_size))
            .map_err(|e| e.to_string())?;
        f.read_exact(&mut h)
            .map_err(|e| format!("header {i}: {e}"))?;
        let (t, w2, h2, idr, rep) = header(&h);
        if (w2, h2) != (w, ht) {
            return Err(format!(
                "record {i} is {w2}x{h2}, not {w}x{ht}: variable-size clips are not supported"
            ));
        }
        // Monotonic by construction; a clock step in a recording is clamped, not trusted.
        let t = t.max(last);
        last = t;
        info.times.push(t - t0);
        info.idr.push(idr);
        info.repeated.push(rep);
    }
    if frames == 0 {
        return Err("clip has no complete records".into());
    }
    Ok(info)
}

/// What a workload's `transform` does to each picture on its way into the encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transform {
    None,
    /// Four copies side by side: 2w x 2h.
    Tile2x2,
    /// A centred crop.
    Crop(usize, usize),
}

impl Transform {
    pub fn parse(s: Option<&str>) -> Result<Transform, String> {
        let Some(s) = s.map(str::trim).filter(|s| !s.is_empty() && *s != "none") else {
            return Ok(Transform::None);
        };
        if s == "tile2x2" {
            return Ok(Transform::Tile2x2);
        }
        if let Some(dims) = s.strip_prefix("crop:") {
            let (w, h) = dims
                .split_once('x')
                .ok_or_else(|| format!("bad crop '{s}'"))?;
            let w: usize = w.parse().map_err(|_| format!("bad crop '{s}'"))?;
            let h: usize = h.parse().map_err(|_| format!("bad crop '{s}'"))?;
            return Ok(Transform::Crop(w & !1, h & !1));
        }
        Err(format!("unknown transform '{s}'"))
    }

    pub fn out_dims(self, w: usize, h: usize) -> Result<(usize, usize), String> {
        match self {
            Transform::None => Ok((w, h)),
            Transform::Tile2x2 => Ok((w * 2, h * 2)),
            Transform::Crop(cw, ch) if cw >= 16 && ch >= 16 && cw <= w && ch <= h => Ok((cw, ch)),
            Transform::Crop(cw, ch) => Err(format!("crop {cw}x{ch} does not fit {w}x{h}")),
        }
    }

    /// Copy one plane (`rows` rows of `row_bytes` source bytes) into a destination plane.
    /// `src_off_rows`/`src_off_bytes` are the crop origin in this plane's units.
    ///
    /// # Safety
    /// `dst` must have `dst_rows` rows of at least `dst_row_bytes` at `dst_stride`.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn copy_plane(
        self,
        src: &[u8],
        src_row_bytes: usize,
        src_rows: usize,
        dst: *mut u8,
        dst_stride: usize,
        dst_row_bytes: usize,
        dst_rows: usize,
        crop_origin: (usize, usize),
    ) {
        match self {
            Transform::None => {
                for y in 0..dst_rows.min(src_rows) {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().add(y * src_row_bytes),
                        dst.add(y * dst_stride),
                        src_row_bytes.min(dst_row_bytes),
                    );
                }
            }
            Transform::Tile2x2 => {
                for y in 0..dst_rows {
                    let s = src.as_ptr().add((y % src_rows) * src_row_bytes);
                    let d = dst.add(y * dst_stride);
                    std::ptr::copy_nonoverlapping(s, d, src_row_bytes);
                    std::ptr::copy_nonoverlapping(s, d.add(src_row_bytes), src_row_bytes);
                }
            }
            Transform::Crop(..) => {
                let (ox, oy) = crop_origin;
                for y in 0..dst_rows {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().add((y + oy) * src_row_bytes + ox),
                        dst.add(y * dst_stride),
                        dst_row_bytes,
                    );
                }
            }
        }
    }

    /// Crop origin (x, y) in luma pixels, even.
    pub fn crop_origin(self, w: usize, h: usize) -> (usize, usize) {
        match self {
            Transform::Crop(cw, ch) => (((w - cw) / 2) & !1, ((h - ch) / 2) & !1),
            _ => (0, 0),
        }
    }

    /// The luma plane after the transform, tightly packed (`ow*oh`), for PSNR.
    pub fn luma(self, src: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
        let (ow, oh) = self.out_dims(w, h).unwrap_or((w, h));
        out.resize(ow * oh, 0);
        let origin = self.crop_origin(w, h);
        unsafe { self.copy_plane(src, w, h, out.as_mut_ptr(), ow, ow, oh, origin) };
    }
}

/// Sequential reader over a clip's records.
pub struct ClipReader {
    f: std::io::BufReader<File>,
    pub width: usize,
    pub height: usize,
}

impl ClipReader {
    pub fn open(path: &Path, info: &ClipInfo) -> std::io::Result<ClipReader> {
        Ok(ClipReader {
            f: std::io::BufReader::with_capacity(8 << 20, File::open(path)?),
            width: info.width,
            height: info.height,
        })
    }

    /// Read the next record's picture into `buf` (`w*h*3/2` bytes).
    pub fn next(&mut self, buf: &mut Vec<u8>) -> std::io::Result<()> {
        let mut h = [0u8; HEADER];
        self.f.read_exact(&mut h)?;
        buf.resize(self.width * self.height * 3 / 2, 0);
        self.f.read_exact(buf)
    }
}

// ---------------------------------------------------------------------------------------------
// Synthetic clips.
// ---------------------------------------------------------------------------------------------

/// Upper bound for one rendered clip on disk.
pub const CLIP_BUDGET_BYTES: u64 = 2_500_000_000;

pub fn synthetic_bytes(w: &Workload) -> u64 {
    let frames = (w.seconds * w.fps as f64).round().max(1.0) as u64;
    frames * (HEADER as u64 + (w.width * w.height * 3 / 2) as u64)
}

/// Full-range BT.709 BGRA -> NV12.
pub fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize, out: &mut [u8]) {
    let (yp, uv) = out.split_at_mut(w * h);
    for y in 0..h {
        let row = &bgra[y * w * 4..(y + 1) * w * 4];
        for x in 0..w {
            let (b, g, r) = (
                row[x * 4] as f32,
                row[x * 4 + 1] as f32,
                row[x * 4 + 2] as f32,
            );
            yp[y * w + x] = (0.2126 * r + 0.7152 * g + 0.0722 * b + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (mut r, mut g, mut b) = (0f32, 0f32, 0f32);
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let o = ((cy * 2 + dy) * w + cx * 2 + dx) * 4;
                b += bgra[o] as f32;
                g += bgra[o + 1] as f32;
                r += bgra[o + 2] as f32;
            }
            let (r, g, b) = (r / 4.0, g / 4.0, b / 4.0);
            let yy = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let cb = (b - yy) / 1.8556 + 128.0;
            let cr = (r - yy) / 1.5748 + 128.0;
            uv[cy * w + cx * 2] = (cb + 0.5).clamp(0.0, 255.0) as u8;
            uv[cy * w + cx * 2 + 1] = (cr + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

fn hash2(x: usize, y: usize, seed: u64) -> u32 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B1) ^ (y as u64).wrapping_mul(0x85EB_CA77) ^ seed;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    (h ^ (h >> 15)) as u32
}

fn put(buf: &mut [u8], o: usize, r: u8, g: u8, b: u8) {
    buf[o] = b;
    buf[o + 1] = g;
    buf[o + 2] = r;
    buf[o + 3] = 255;
}

/// The detailed desktop: a noisy photographic-ish wallpaper, a grid of icons, a menu bar of text.
pub(super) fn stress_desktop(w: usize, h: usize, seed: u64) -> Vec<u8> {
    let mut buf = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let n = (hash2(x, y, seed) & 63) as i32 - 32;
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let r = (60.0 + 120.0 * fx + 30.0 * (fy * 9.0).sin()) as i32 + n;
            let g = (80.0 + 90.0 * fy + 25.0 * (fx * 13.0).cos()) as i32 + n;
            let b = (140.0 + 60.0 * (1.0 - fx) * fy) as i32 + n;
            put(
                &mut buf,
                (y * w + x) * 4,
                r.clamp(0, 255) as u8,
                g.clamp(0, 255) as u8,
                b.clamp(0, 255) as u8,
            );
        }
    }
    // Icons down the right edge.
    let icon = (h / 16).max(24);
    let mut rng = Rng(seed ^ 0x1C0);
    let mut y0 = 40;
    while y0 + icon < h {
        let x0 = w.saturating_sub(icon + 24);
        let (cr, cg, cb) = (rng.next() as u8, rng.next() as u8, rng.next() as u8);
        for y in y0..y0 + icon {
            for x in x0..x0 + icon {
                let n = (hash2(x, y, seed ^ 7) & 31) as u8;
                put(&mut buf, (y * w + x) * 4, cr ^ n, cg ^ n, cb);
            }
        }
        y0 += icon * 3 / 2;
    }
    // Menu bar with text-like glyphs.
    let bar = (h / 45).max(12);
    for y in 0..bar {
        for x in 0..w {
            let on = (8..bar - 4).contains(&(y % bar))
                && hash2(x / 2, y / 2, seed ^ 0xBA2) & 3 == 0
                && (x / 70) % 3 != 2;
            let v = if on { 30 } else { 236 };
            put(&mut buf, (y * w + x) * 4, v, v, v);
        }
    }
    buf
}

/// The window being dragged: a title bar and a dense page of coloured "text".
pub(super) fn stress_window(w: usize, h: usize, seed: u64) -> Vec<u8> {
    let mut buf = vec![0u8; w * h * 4];
    let title = 28.min(h / 8).max(8);
    let line = 18;
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 4;
            if y < title {
                let v = 210 - (y as i32 * 2) as u8;
                put(&mut buf, o, v, v, v);
                continue;
            }
            if x < 4 || x >= w - 4 || y >= h - 4 {
                put(&mut buf, o, 120, 120, 120);
                continue;
            }
            let ly = (y - title) / line;
            let in_line = (y - title) % line < 13;
            // Words: runs of glyph cells with gaps, per line a different length.
            let cell = x / 7;
            let word_gap = hash2(cell / 6, ly, seed ^ 0x77) & 7 == 0;
            let line_len = 40 + (hash2(0, ly, seed) % (w as u32 / 7)) as usize;
            let glyph = in_line && !word_gap && cell < line_len && hash2(x, y, seed ^ 0x6c) % 5 < 2;
            if glyph {
                let tint = hash2(cell / 6, ly, seed ^ 0x99) % 4;
                let (r, g, b) = match tint {
                    0 => (180, 40, 40),
                    1 => (30, 90, 180),
                    2 => (20, 130, 50),
                    _ => (20, 20, 20),
                };
                put(&mut buf, o, r, g, b);
            } else {
                put(&mut buf, o, 252, 252, 250);
            }
        }
    }
    // An embedded photo-like block of noise in the lower right quarter.
    let (px, py) = (w * 3 / 5, h * 3 / 5);
    for y in py..h.saturating_sub(12) {
        for x in px..w.saturating_sub(12) {
            let n = hash2(x, y, seed ^ 0xF0) as u8;
            put(&mut buf, (y * w + x) * 4, n, n.rotate_left(3), 255 - n);
        }
    }
    buf
}

/// A closed path of fast drags with pauses, `frames` long, returning to its start so the clip
/// loops seamlessly. Positions are even so the NV12 blit stays chroma-aligned.
pub(super) fn drag_path(
    frames: usize,
    fps: f64,
    span_x: usize,
    span_y: usize,
    seed: u64,
) -> Vec<(usize, usize)> {
    let mut rng = Rng(seed ^ 0xD8A6);
    let start = (span_x as f64 * 0.15, span_y as f64 * 0.2);
    let mut segs: Vec<(usize, (f64, f64))> = Vec::new(); // (frames, target)
    let mut used = 0usize;
    let mut cur = start;
    let pause_last = (0.3 * fps) as usize;
    loop {
        let mv = (rng.range(0.25, 0.7) * fps) as usize;
        let pause = (rng.range(0.1, 0.5) * fps) as usize;
        let back = (0.5 * fps) as usize;
        if used + mv + pause + back + pause_last > frames {
            break;
        }
        let target = (
            rng.range(0.0, 1.0) * span_x as f64,
            rng.range(0.0, 1.0) * span_y as f64,
        );
        segs.push((mv, target));
        segs.push((pause, target));
        used += mv + pause;
        cur = target;
    }
    let _ = cur;
    // Return home with whatever is left, ending on a short pause.
    let rest = frames - used;
    let back = rest.saturating_sub(pause_last).max(1);
    segs.push((back, start));
    segs.push((rest - back, start));

    let mut out = Vec::with_capacity(frames);
    let mut from = start;
    for (n, to) in segs {
        for i in 0..n {
            let t = (i + 1) as f64 / n as f64;
            let e = t * t * (3.0 - 2.0 * t); // smoothstep: accelerate, coast, settle
            let x = from.0 + (to.0 - from.0) * e;
            let y = from.1 + (to.1 - from.1) * e;
            out.push(((x as usize) & !1, (y as usize) & !1));
        }
        from = to;
    }
    out.truncate(frames);
    while out.len() < frames {
        out.push(((start.0 as usize) & !1, (start.1 as usize) & !1));
    }
    out
}

/// Render a synthetic workload into a raw clip at `path`.
pub fn render(w: &Workload, path: &Path) -> Result<ClipInfo, String> {
    let (width, height) = (w.width & !1, w.height & !1);
    if width < 64 || height < 64 {
        return Err("synthetic clips must be at least 64x64".into());
    }
    let frames = (w.seconds * w.fps as f64).round().max(1.0) as usize;
    let pattern = w.pattern.as_deref().unwrap_or("window_drag_stress");
    let seed = w.seed;
    let gap_ns = 1_000_000_000f64 / w.fps as f64;
    let tmp = path.with_extension("partial");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|e| format!("create {}: {e}", tmp.display()))?;
    let mut out = BufWriter::with_capacity(8 << 20, file);
    let pic = width * height * 3 / 2;
    let mut nv12 = vec![0u8; pic];

    let write_rec = |out: &mut BufWriter<File>, i: usize, nv12: &[u8]| -> Result<(), String> {
        let mut h = [0u8; HEADER];
        h[0..8].copy_from_slice(&((i as f64 * gap_ns).round() as u64).to_le_bytes());
        h[16..20].copy_from_slice(&(width as u32).to_le_bytes());
        h[20..24].copy_from_slice(&(height as u32).to_le_bytes());
        h[24..28].copy_from_slice(&u32::from(i == 0).to_le_bytes());
        out.write_all(&h).map_err(|e| e.to_string())?;
        out.write_all(nv12).map_err(|e| e.to_string())
    };

    // `stress_then_noise`: the drag, then every pixel new from five seconds in.
    let noise_from = (pattern == "stress_then_noise").then_some(5 * w.fps as usize);
    if pattern == "window_drag_stress" || noise_from.is_some() {
        let ww = (width * 55 / 100) & !1;
        let wh = (height * 60 / 100) & !1;
        let mut desk = vec![0u8; pic];
        bgra_to_nv12(
            &stress_desktop(width, height, seed),
            width,
            height,
            &mut desk,
        );
        let mut win = vec![0u8; ww * wh * 3 / 2];
        bgra_to_nv12(&stress_window(ww, wh, seed), ww, wh, &mut win);
        let path_xy = drag_path(frames, w.fps as f64, width - ww, height - wh, seed);
        let mut bgra = vec![0u8; width * height * 4];
        for (i, &(px, py)) in path_xy.iter().enumerate() {
            if noise_from.is_some_and(|n| i >= n) {
                let wl = crate::streamer::synthetic::Workload::Noise;
                let (w, h) = (width as u32, height as u32);
                crate::streamer::synthetic::fill(&mut bgra, w, h, i as u32, wl);
                bgra_to_nv12(&bgra, width, height, &mut nv12);
                write_rec(&mut out, i, &nv12)?;
                continue;
            }
            nv12.copy_from_slice(&desk);
            for y in 0..wh {
                let d = (py + y) * width + px;
                nv12[d..d + ww].copy_from_slice(&win[y * ww..(y + 1) * ww]);
            }
            let (duv, suv) = (width * height, ww * wh);
            for y in 0..wh / 2 {
                let d = duv + (py / 2 + y) * width + px;
                let s = suv + y * ww;
                nv12[d..d + ww].copy_from_slice(&win[s..s + ww]);
            }
            write_rec(&mut out, i, &nv12)?;
        }
    } else {
        let wl = crate::streamer::synthetic::Workload::parse(pattern)
            .ok_or_else(|| format!("unknown synthetic pattern '{pattern}'"))?;
        let mut bgra = vec![0u8; width * height * 4];
        for i in 0..frames {
            crate::streamer::synthetic::fill(&mut bgra, width as u32, height as u32, i as u32, wl);
            bgra_to_nv12(&bgra, width, height, &mut nv12);
            write_rec(&mut out, i, &nv12)?;
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    drop(out);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    scan(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drag_path_is_closed_and_even() {
        let p = drag_path(600, 60.0, 800, 400, 1);
        assert_eq!(p.len(), 600);
        assert_eq!(p[0].0 % 2, 0);
        let last = p[599];
        // ends where it started (within the final pause)
        assert!(
            last.0.abs_diff(120) <= 2 && last.1.abs_diff(80) <= 2,
            "{last:?}"
        );
        assert!(p.iter().all(|&(x, y)| x <= 800 && y <= 400));
    }

    #[test]
    fn transforms_have_the_promised_shapes() {
        assert_eq!(
            Transform::parse(Some("tile2x2"))
                .unwrap()
                .out_dims(1920, 1080)
                .unwrap(),
            (3840, 2160)
        );
        let c = Transform::parse(Some("crop:1280x720")).unwrap();
        assert_eq!(c.out_dims(1920, 1080).unwrap(), (1280, 720));
        assert_eq!(c.crop_origin(1920, 1080), (320, 180));
        let src: Vec<u8> = (0..16u8).collect(); // 4x4 luma
        let mut out = Vec::new();
        Transform::Tile2x2.luma(&src, 4, 4, &mut out);
        assert_eq!(out.len(), 64);
        assert_eq!(&out[0..8], &[0, 1, 2, 3, 0, 1, 2, 3]);
    }

    #[test]
    fn grey_converts_to_neutral_chroma() {
        let bgra = vec![128u8; 4 * 4 * 4];
        let mut out = vec![0u8; 24];
        bgra_to_nv12(&bgra, 4, 4, &mut out);
        assert!(out[..16].iter().all(|&y| y == 128));
        assert!(out[16..].iter().all(|&c| c == 128));
    }
}

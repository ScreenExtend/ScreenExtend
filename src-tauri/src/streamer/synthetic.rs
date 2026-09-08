//! Scripted synthetic workloads for the probe harness.
//!
//! Encoder and transport numbers only mean something next to the content that produced them: a
//! static desktop and a full-frame 60 fps wipe are different problems for the same encoder, and
//! a single "moving box" pattern measures neither. These four are the shapes that actually show
//! up, chosen so the bitrate and the encode cost separate:
//!
//! - [`Workload::Static`] — nothing changes. Exercises the idle path, the keepalive repeater and
//!   whatever floor the rate control refuses to go below.
//! - [`Workload::Scroll`] — text-like rows sliding vertically. Small per-frame residual, high
//!   spatial detail: the case intra-refresh and QP choice show up in.
//! - [`Workload::Motion`] — full-frame movement every frame. The worst case; if a pipeline keeps
//!   up here it keeps up anywhere.
//! - [`Workload::Drag`] — a rigid block translating over a static background, which is what
//!   dragging a window looks like to an encoder. Almost pure motion vectors.
//!
//! [`Counter`] paints a large frame number and a millisecond clock over any of them. That is the
//! target for the camera method in `docs/BASELINE.md`: film the host display and the client
//! screen in one shot at 240 fps and subtract the two readings.
//!
//! Output is BGRA8, four bytes per pixel, matching what the capture backends hand the encoders.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Workload {
    /// The historical pattern: a drifting box and a sweeping bar over a gradient.
    #[default]
    Mixed,
    Static,
    Scroll,
    Motion,
    Drag,
}

impl Workload {
    pub fn parse(s: &str) -> Option<Workload> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mixed" => Some(Workload::Mixed),
            "static" => Some(Workload::Static),
            "scroll" => Some(Workload::Scroll),
            "motion" => Some(Workload::Motion),
            "drag" => Some(Workload::Drag),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Workload::Mixed => "mixed",
            Workload::Static => "static",
            Workload::Scroll => "scroll",
            Workload::Motion => "motion",
            Workload::Drag => "drag",
        }
    }
}

#[inline]
fn put(buf: &mut [u8], o: usize, b: u8, g: u8, r: u8) {
    buf[o] = b;
    buf[o + 1] = g;
    buf[o + 2] = r;
    buf[o + 3] = 255;
}

/// Render one frame of `workload` into `buf` (BGRA, `width * height * 4` bytes).
pub fn fill(buf: &mut [u8], width: u32, height: u32, frame: u32, workload: Workload) {
    let w = width as usize;
    let h = height as usize;
    if buf.len() < w * h * 4 {
        return;
    }
    let f = frame as usize;

    match workload {
        // Byte-identical every frame. Anything the pipeline reports here is its own overhead.
        Workload::Static => {
            for y in 0..h {
                let row = y * w * 4;
                for x in 0..w {
                    let o = row + x * 4;
                    let shade = ((x / 32 + y / 32) % 2) as u8 * 24 + 32;
                    put(buf, o, shade, shade, shade);
                }
            }
        }

        // Text-like: dense high-contrast rows scrolling upward one line per frame. Cheap in
        // motion vectors, expensive in residual detail, which is the combination that separates
        // a well-chosen QP from a badly chosen one.
        Workload::Scroll => {
            let line_h = (h / 30).max(8);
            let offset = (f * 3) % line_h.max(1);
            for y in 0..h {
                let row = y * w * 4;
                let ly = (y + offset) / line_h;
                let in_line = ((y + offset) % line_h) < line_h * 2 / 3;
                for x in 0..w {
                    let o = row + x * 4;
                    // Deterministic per-cell "glyph" so every line differs from its neighbours.
                    let cell = x / 9;
                    let on = in_line
                        && ((cell.wrapping_mul(2654435761) ^ ly.wrapping_mul(40503)) & 3) != 0;
                    if on {
                        put(buf, o, 235, 235, 235);
                    } else {
                        put(buf, o, 18, 18, 18);
                    }
                }
            }
        }

        // Every pixel changes every frame: a diagonal colour wipe plus per-pixel noise the
        // encoder cannot predict away. The upper bound on encode cost.
        Workload::Motion => {
            for y in 0..h {
                let row = y * w * 4;
                for x in 0..w {
                    let o = row + x * 4;
                    let phase = (x + y * 2 + f * 24) as u32;
                    let noise =
                        ((x.wrapping_mul(1103515245) ^ y.wrapping_mul(12345) ^ f) >> 7) as u8;
                    put(
                        buf,
                        o,
                        (phase & 0xff) as u8 ^ (noise >> 3),
                        ((phase >> 1) & 0xff) as u8,
                        ((phase >> 2) & 0xff) as u8 ^ (noise >> 4),
                    );
                }
            }
        }

        // A rigid block over a fixed background: what dragging a window looks like to the
        // encoder. Should compress to almost nothing but motion vectors.
        Workload::Drag => {
            let bw = w / 3;
            let bh = h / 3;
            let span_x = w.saturating_sub(bw).max(1);
            let span_y = h.saturating_sub(bh).max(1);
            // Ping-pong rather than wrap, so the block never teleports across the frame.
            let px = f * 6 % (span_x * 2);
            let py = f * 4 % (span_y * 2);
            let bx = if px < span_x { px } else { span_x * 2 - px };
            let by = if py < span_y { py } else { span_y * 2 - py };
            for y in 0..h {
                let row = y * w * 4;
                let in_row = y >= by && y < by + bh;
                for x in 0..w {
                    let o = row + x * 4;
                    if in_row && x >= bx && x < bx + bw {
                        let d = ((x - bx) / 16 + (y - by) / 16) % 2;
                        let s = if d == 0 { 220u8 } else { 160u8 };
                        put(buf, o, s / 2, s, s);
                    } else {
                        let shade = ((x / 64 + y / 64) % 2) as u8 * 16 + 40;
                        put(buf, o, shade, shade, shade + 8);
                    }
                }
            }
        }

        Workload::Mixed => {
            let box_w = w / 6;
            let box_h = h / 6;
            let span_x = w.saturating_sub(box_w).max(1);
            let span_y = h.saturating_sub(box_h).max(1);
            let box_x = (f * 11) % span_x;
            let box_y = (f * 7) % span_y;
            let bar_w = (w / 60).max(2);
            let bar_x = (f * (w / 90).max(1)) % w;
            let bar_h = h / 12;
            for y in 0..h {
                let row = y * w * 4;
                for x in 0..w {
                    let o = row + x * 4;
                    let in_box = x >= box_x && x < box_x + box_w && y >= box_y && y < box_y + box_h;
                    let in_bar = y < bar_h && x >= bar_x && x < bar_x + bar_w;
                    if in_bar {
                        put(buf, o, 255, 255, 255);
                    } else if in_box {
                        put(buf, o, 32, 32, 240);
                    } else {
                        put(
                            buf,
                            o,
                            ((x + f * 3) & 0xff) as u8,
                            ((y + f * 5) & 0xff) as u8,
                            ((x + y + f * 2) & 0xff) as u8,
                        );
                    }
                }
            }
        }
    }
}

/// 5x7 bitmap glyphs, one `u8` per row, high bit leftmost. Hand-rolled because the client page
/// and this harness must both stay dependency-free, and a font crate for ten digits is absurd.
const GLYPHS: [(char, [u8; 7]); 13] = [
    ('0', [0x1E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E]),
    ('1', [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E]),
    ('2', [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F]),
    ('3', [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E]),
    ('4', [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02]),
    ('5', [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E]),
    ('6', [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E]),
    ('7', [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08]),
    ('8', [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E]),
    ('9', [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C]),
    (':', [0x00, 0x06, 0x06, 0x00, 0x06, 0x06, 0x00]),
    ('.', [0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x06]),
    (' ', [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
];

fn glyph(c: char) -> Option<[u8; 7]> {
    GLYPHS.iter().find(|(g, _)| *g == c).map(|(_, rows)| *rows)
}

/// Paint `text` at (`x0`, `y0`) scaled by `scale`, on a black plate so it reads on any content.
fn draw_text(
    buf: &mut [u8],
    width: u32,
    height: u32,
    x0: usize,
    y0: usize,
    scale: usize,
    text: &str,
) {
    let w = width as usize;
    let h = height as usize;
    let cell_w = 6 * scale;
    let plate_h = 7 * scale + 2 * scale;
    let plate_w = text.chars().count() * cell_w + 2 * scale;

    for y in y0..(y0 + plate_h).min(h) {
        let row = y * w * 4;
        for x in x0..(x0 + plate_w).min(w) {
            put(buf, row + x * 4, 0, 0, 0);
        }
    }

    for (i, c) in text.chars().enumerate() {
        let Some(rows) = glyph(c) else { continue };
        for (ry, bits) in rows.iter().enumerate() {
            for bx in 0..5 {
                if bits & (1 << (4 - bx)) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    let y = y0 + scale + ry * scale + sy;
                    if y >= h {
                        continue;
                    }
                    let row = y * w * 4;
                    for sx in 0..scale {
                        let x = x0 + scale + i * cell_w + bx * scale + sx;
                        if x >= w {
                            continue;
                        }
                        put(buf, row + x * 4, 0, 255, 0);
                    }
                }
            }
        }
    }
}

/// Overlay a frame number and a millisecond clock, sized to be legible on camera.
///
/// The clock is the elapsed host time in `SSS.mmm`, not a wall clock: only the *difference*
/// between the host reading and the client reading matters, and an elapsed counter cannot be
/// confused with a time zone.
pub fn draw_counter(buf: &mut [u8], width: u32, height: u32, frame: u32, elapsed_ms: u64) {
    let scale = ((width / 160).max(3)) as usize;
    let secs = elapsed_ms / 1000;
    let ms = elapsed_ms % 1000;
    draw_text(
        buf,
        width,
        height,
        scale,
        scale,
        scale,
        &format!("{frame:08}"),
    );
    draw_text(
        buf,
        width,
        height,
        scale,
        scale * 12,
        scale,
        &format!("{:04}.{:03}", secs % 10_000, ms),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32) -> Vec<u8> {
        vec![0u8; (w * h * 4) as usize]
    }

    fn differing_bytes(a: &[u8], b: &[u8]) -> usize {
        a.iter().zip(b).filter(|(x, y)| x != y).count()
    }

    #[test]
    fn static_workload_is_byte_identical_across_frames() {
        let (w, h) = (320, 180);
        let mut a = frame(w, h);
        let mut b = frame(w, h);
        fill(&mut a, w, h, 0, Workload::Static);
        fill(&mut b, w, h, 900, Workload::Static);
        assert_eq!(differing_bytes(&a, &b), 0);
    }

    #[test]
    fn motion_changes_far_more_than_drag() {
        let (w, h) = (320, 180);
        let mut m0 = frame(w, h);
        let mut m1 = frame(w, h);
        fill(&mut m0, w, h, 10, Workload::Motion);
        fill(&mut m1, w, h, 11, Workload::Motion);
        let mut d0 = frame(w, h);
        let mut d1 = frame(w, h);
        fill(&mut d0, w, h, 10, Workload::Drag);
        fill(&mut d1, w, h, 11, Workload::Drag);
        let motion = differing_bytes(&m0, &m1);
        let drag = differing_bytes(&d0, &d1);
        assert!(
            motion > drag * 3,
            "motion changed {motion} bytes, drag {drag}; they must not be comparable or the \
             workloads measure the same thing"
        );
    }

    #[test]
    fn scroll_moves_without_touching_every_pixel() {
        let (w, h) = (320, 180);
        let mut a = frame(w, h);
        let mut b = frame(w, h);
        fill(&mut a, w, h, 0, Workload::Scroll);
        fill(&mut b, w, h, 1, Workload::Scroll);
        let changed = differing_bytes(&a, &b);
        assert!(changed > 0, "scroll must actually scroll");
        assert!(
            changed < a.len() * 9 / 10,
            "scroll changed {changed}/{} bytes — that is a full-frame workload, not a scroll",
            a.len()
        );
    }

    #[test]
    fn every_workload_fills_the_whole_buffer_opaque() {
        let (w, h) = (64, 48);
        for wl in [
            Workload::Mixed,
            Workload::Static,
            Workload::Scroll,
            Workload::Motion,
            Workload::Drag,
        ] {
            let mut buf = frame(w, h);
            fill(&mut buf, w, h, 7, wl);
            assert!(
                buf.chunks_exact(4).all(|px| px[3] == 255),
                "{} left transparent pixels",
                wl.name()
            );
        }
    }

    #[test]
    fn a_short_buffer_is_left_alone_rather_than_panicking() {
        let mut buf = vec![0u8; 10];
        fill(&mut buf, 64, 48, 0, Workload::Motion);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn the_counter_paints_and_changes_with_the_frame_number() {
        let (w, h) = (640, 360);
        let mut a = frame(w, h);
        let mut b = frame(w, h);
        fill(&mut a, w, h, 0, Workload::Static);
        fill(&mut b, w, h, 0, Workload::Static);
        draw_counter(&mut a, w, h, 1, 1_000);
        draw_counter(&mut b, w, h, 2, 1_100);
        assert!(differing_bytes(&a, &b) > 0, "the counter never changed");
    }

    #[test]
    fn the_counter_stays_inside_a_small_frame() {
        let (w, h) = (64, 32);
        let mut buf = frame(w, h);
        draw_counter(&mut buf, w, h, 12_345_678, 99_999);
    }

    #[test]
    fn workload_names_round_trip() {
        for wl in [
            Workload::Mixed,
            Workload::Static,
            Workload::Scroll,
            Workload::Motion,
            Workload::Drag,
        ] {
            assert_eq!(Workload::parse(wl.name()), Some(wl));
        }
        assert_eq!(Workload::parse("nonsense"), None);
    }
}

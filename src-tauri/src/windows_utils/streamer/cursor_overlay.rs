//! Host-side cursor compositing for the WGC capture path.
//!
//! # Why this exists
//!
//! `IsCursorCaptureEnabled` asks the compositor to draw the pointer into the captured surface,
//! and on a *direct* display that is exactly right. On an **indirect** display — which is every
//! display this app creates — it is not, because DWM does not produce a new frame for a pointer
//! that moved over an otherwise static desktop. Measured on the Windows dev box with the pointer
//! swept continuously across an empty virtual display: **2–10 real frames a second**, in both the
//! pre-engagement build and the current one (`docs/RESULTS.md`, "User-facing stream quality").
//! Everything else the stream emits in that scene is a keepalive repeat of the last captured
//! surface, and that surface carries the pointer wherever WGC last composited it. The user's
//! word for it is "the cursor freezes and is painted on stale frames", and they are right.
//!
//! So the pointer stops being something the compositor owes us:
//!
//! 1. WGC is asked for the desktop **without** the cursor;
//! 2. every frame staged into a ring slot gets the pointer drawn onto it here, on the GPU;
//! 3. when the pointer moves and no new frame arrives, the encode thread redraws it on the slot
//!    it is already holding and re-encodes — a few hundred bytes of P-frame — so pointer motion
//!    runs at the stream's frame rate rather than the compositor's mood.
//!
//! The DXGI backend has always worked this way (`dxgi/mod.rs`, `frame()`); this is the WGC
//! equivalent, and it reuses the same sprite builder and the same quad renderer so both backends
//! draw the identical pointer.
//!
//! # The fallback, and why the tier is probed rather than assumed
//!
//! Everything here needs a render-target view on the encoder's own input textures, which is only
//! true of the async ring's slots. If any part of the setup fails, the overlay is simply not
//! built, [`LiveCapture::cursor_capture_override`] returns `None`, and WGC keeps compositing the
//! pointer exactly as before. There is no state in which both draw it, and none in which neither
//! does.
//!
//! # Threading
//!
//! Two threads reach this type: the WGC callback (a fresh frame) and the ring encode thread (a
//! pointer that moved with no fresh frame). D3D11 makes *individual* context calls thread-safe,
//! not a set-state-then-draw sequence, so the whole thing lives behind one `std::sync::Mutex`
//! that both sides take around their GPU work. Neither side blocks on the other while holding
//! it, and the encode thread only ever takes it on a tick where no frame arrived.

use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{bail, Context as _, Result};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D, D3D11_BOX,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITORINFO};
use windows::Win32::UI::WindowsAndMessaging::HCURSOR;

use super::dxgi::cursor::{
    build_sprite, quad_verts, shape_from_hcursor, CursorSprite, QuadRenderer,
};
use super::nvidia::encoder::KEY_WRITER;
use crate::streamer::telemetry::SharedTelemetry;

/// Ceiling on the undo texture, in destination pixels.
///
/// Windows' largest cursor at 400 % is 256 px square; a display scaled past that would need more,
/// and this is the point at which saving-and-restoring stops being cheaper than not bothering.
const MAX_UNDO_DIM: u32 = 512;

/// How often the monitor rectangle and the pointer's DPI are re-read.
///
/// Both change on a `/reconfig` that resizes or rescales the virtual display. One `GetMonitorInfoW`
/// and one `GetDpiForMonitor` a second is nothing; doing it per frame at 120 Hz is not.
const GEOMETRY_REFRESH: std::time::Duration = std::time::Duration::from_secs(1);

/// How many cursor shapes to keep converted at once. Windows' whole standard set is fourteen and
/// a session uses a handful; each entry is one or two textures of at most 256 px square.
const MAX_CACHED_SPRITES: usize = 8;

/// How long [`CursorOverlay::recompose`] waits for the slot it was told is free.
///
/// The held slot's key was released by the encoder and nobody else can hold it, so this should
/// never wait at all. `KEY_TIMEOUT_MS` (1 s) is the wrong ceiling here: the WGC callback wants
/// this same lock for the *next* frame, and a pointer redraw that misses one tick costs a tick.
const RECOMPOSE_KEY_TIMEOUT_MS: u32 = 8;

/// Smallest gap between the "no pointer drawn" lines. See [`CursorOverlay::note_absent`].
const ABSENT_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Everything about the pointer that decides what gets drawn where. Two equal values draw the
/// same picture, which is exactly the test the encode thread's idle tick needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pointer {
    x: i32,
    y: i32,
    /// `HCURSOR` as an integer, so the struct stays `Copy` and comparable.
    shape: isize,
}

/// A rectangle in destination pixels, already clipped to the target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// The three coordinate spaces the pointer has to cross, in one value so the arithmetic that
/// crosses them can be tested without a GPU.
///
/// Desktop pixels (what `GetCursorInfo` reports) → this display's pixels (subtract the origin) →
/// the encoder's pixels (multiply by the video scale). The hotspot is subtracted in *display*
/// pixels, after the DPI correction and before the video scale, because that is the space the
/// cursor bitmap itself is measured in.
#[derive(Clone, Copy)]
struct Geometry {
    origin_x: i32,
    origin_y: i32,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    dpi_scale: f32,
}

/// Where the sprite goes, in destination pixels, hotspot applied. May be negative or run off the
/// edge; the rasterizer clips the draw and [`clip_to`] clips the undo rectangle.
fn place_sprite(g: &Geometry, px: i32, py: i32, sprite: &CursorSprite) -> (i32, i32, u32, u32) {
    let kx = g.dst_w as f32 / g.src_w.max(1) as f32;
    let ky = g.dst_h as f32 / g.src_h.max(1) as f32;
    let s = g.dpi_scale;
    let left = (px - g.origin_x) as f32 - sprite.hotspot_x as f32 * s;
    let top = (py - g.origin_y) as f32 - sprite.hotspot_y as f32 * s;
    (
        (left * kx).round() as i32,
        (top * ky).round() as i32,
        ((sprite.width as f32 * s * kx).round() as u32).max(1),
        ((sprite.height as f32 * s * ky).round() as u32).max(1),
    )
}

/// The part of the sprite's box that lands on the target, or `None` when none of it does.
fn clip_to(x: i32, y: i32, w: u32, h: u32, dst_w: u32, dst_h: u32) -> Option<Rect> {
    let left = x.clamp(0, dst_w as i32) as u32;
    let top = y.clamp(0, dst_h as i32) as u32;
    let right = x.saturating_add(w as i32).clamp(0, dst_w as i32) as u32;
    let bottom = y.saturating_add(h as i32).clamp(0, dst_h as i32) as u32;
    (right > left && bottom > top).then_some(Rect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    })
}

struct Slot {
    rtv: ID3D11RenderTargetView,
    /// The pixels the sprite was drawn over, so it can be erased without a full-frame copy.
    undo: Option<ID3D11Texture2D>,
    undo_w: u32,
    undo_h: u32,
    /// Where the sprite currently sits in this slot, if it is drawn at all.
    drawn: Option<Rect>,
    /// The pointer state this slot's pixels currently show.
    shows: Option<Pointer>,
}

pub struct CursorOverlay {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    renderer: QuadRenderer,
    textures: Vec<ID3D11Texture2D>,
    /// Empty for the AMF ring, whose slots live on the capture device and need no key.
    mutexes: Vec<IDXGIKeyedMutex>,
    slots: Vec<Slot>,

    /// `HMONITOR` of the captured display, as an integer so the struct stays `Send`.
    monitor: isize,
    origin_x: i32,
    origin_y: i32,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    /// Sprite pixels to source pixels: a cursor bitmap handed out at the process's DPI drawn onto
    /// a display running at a different one.
    dpi_scale: f32,
    geometry_checked: Instant,

    /// Built sprites, most recently used first, keyed by `HCURSOR`.
    ///
    /// A cache rather than a single slot because a pointer crossing a text field alternates
    /// arrow and I-beam several times a second, and rebuilding a sprite is two `GetDIBits` and a
    /// texture upload. System cursor handles are stable for the life of the process, so a handle
    /// is a sound key.
    sprites: Vec<(isize, CursorSprite)>,
    /// Shapes that would not convert. Kept so a cursor we cannot read is not re-attempted sixty
    /// times a second.
    bad_shapes: Vec<isize>,
    shape_errors: u32,
    absent_logged: u32,
    absent_logged_at: Instant,

    telemetry: SharedTelemetry,
}

// The COM pointers here are all created from, and only ever used with, the one capture device,
// and every call into them happens under the `Mutex` this type is stored in. Same argument, and
// the same shape, as `Scaler` and the encoders next door.
unsafe impl Send for CursorOverlay {}

impl CursorOverlay {
    /// Builds the overlay for one session's ring slots, or fails — in which case the caller must
    /// leave WGC compositing the pointer.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        textures: &[ID3D11Texture2D],
        mutexes: &[IDXGIKeyedMutex],
        monitor: isize,
        native: (u32, u32),
        dst: (u32, u32),
        telemetry: SharedTelemetry,
    ) -> Result<Self> {
        if textures.is_empty() {
            bail!("cursor overlay needs at least one ring slot");
        }
        if !mutexes.is_empty() && mutexes.len() != textures.len() {
            bail!(
                "cursor overlay: {} slot textures against {} keyed mutexes",
                textures.len(),
                mutexes.len()
            );
        }
        let renderer =
            QuadRenderer::new(device, context).context("cursor overlay quad renderer")?;

        let mut slots = Vec::with_capacity(textures.len());
        for (i, tex) in textures.iter().enumerate() {
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            unsafe { device.CreateRenderTargetView(tex, None, Some(&mut rtv)) }
                .with_context(|| format!("CreateRenderTargetView (ring slot {i})"))?;
            slots.push(Slot {
                rtv: rtv.context("ring slot render target view was null")?,
                undo: None,
                undo_w: 0,
                undo_h: 0,
                drawn: None,
                shows: None,
            });
        }

        // The pointer is reported in desktop coordinates, so the display's own origin is the
        // difference between drawing it where the user is pointing and drawing it a whole screen
        // away. A virtual display normally sits at a negative x.
        let rect = monitor_rect(monitor);
        if rect.is_none() {
            teprintln!(
                "[cursor] GetMonitorInfoW({monitor:#x}) failed; assuming this display starts at \
                 the desktop origin, which is wrong for any display but the primary"
            );
        }
        let (origin_x, origin_y, rect_w, rect_h) = rect.unwrap_or((0, 0, native.0, native.1));
        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            renderer,
            textures: textures.to_vec(),
            mutexes: mutexes.to_vec(),
            slots,
            monitor,
            origin_x,
            origin_y,
            src_w: rect_w.max(1),
            src_h: rect_h.max(1),
            dst_w: dst.0.max(1),
            dst_h: dst.1.max(1),
            dpi_scale: cursor_dpi_scale(monitor),
            geometry_checked: Instant::now(),
            sprites: Vec::with_capacity(MAX_CACHED_SPRITES),
            bad_shapes: Vec::new(),
            shape_errors: 0,
            absent_logged: 0,
            // Far enough in the past that the first absence reports immediately. `checked_sub`
            // because `Instant - Duration` panics on a machine that booted seconds ago.
            absent_logged_at: Instant::now()
                .checked_sub(ABSENT_LOG_INTERVAL)
                .unwrap_or_else(Instant::now),
            telemetry,
        })
    }

    /// The geometry, for the one line the log gets when a session starts compositing.
    pub fn describe(&self) -> String {
        format!(
            "display {}x{} at {},{} (hmonitor {:#x}) -> encode {}x{}, cursor dpi factor {:.2}",
            self.src_w,
            self.src_h,
            self.origin_x,
            self.origin_y,
            self.monitor,
            self.dst_w,
            self.dst_h,
            self.dpi_scale
        )
    }

    /// Draw the pointer onto a slot the capture callback has just overwritten with a fresh frame.
    ///
    /// The caller already owns the slot — for the NVENC ring that means it holds `KEY_WRITER` —
    /// and flushes afterwards, so this only issues draws.
    pub fn compose_fresh(&mut self, slot: usize) {
        let started = Instant::now();
        let want = self.read_pointer();
        if let Some(s) = self.slots.get_mut(slot) {
            // A fresh frame overwrote the whole slot: nothing is drawn on it and nothing under it
            // is worth keeping.
            s.drawn = None;
            s.shows = None;
        }
        if let Err(e) = self.render(slot, want) {
            teprintln!("cursor overlay: composing slot {slot} failed: {e:#}");
        }
        self.note_cost(started);
    }

    /// Redraw the pointer on a slot the encode thread is holding, if it has moved or changed.
    ///
    /// Returns `true` when the slot's pixels changed, which is the encode thread's signal that
    /// re-encoding it is worth a frame rather than a repeat. Takes the slot's keyed mutex itself:
    /// unlike [`Self::compose_fresh`], nobody else is holding it.
    pub fn recompose(&mut self, slot: usize) -> bool {
        let want = self.read_pointer();
        let shows = self.slots.get(slot).and_then(|s| s.shows);
        if want == shows {
            return false;
        }
        let started = Instant::now();
        let held = match self.mutexes.get(slot) {
            Some(m) => {
                // The held slot sits at `KEY_WRITER`: `encode_input_slot` releases it there and
                // `encode_repeat_slot` takes and returns it there.
                if let Err(e) = unsafe { m.AcquireSync(KEY_WRITER, RECOMPOSE_KEY_TIMEOUT_MS) } {
                    teprintln!("cursor overlay: AcquireSync(writer) on slot {slot} failed: {e}");
                    return false;
                }
                Some(m.clone())
            }
            None => None,
        };

        let drawn = self.render(slot, want);
        unsafe { self.context.Flush() };
        if let Some(m) = held {
            let _ = unsafe { m.ReleaseSync(KEY_WRITER) };
        }
        self.note_cost(started);

        match drawn {
            Ok(changed) => changed,
            Err(e) => {
                teprintln!("cursor overlay: recomposing slot {slot} failed: {e:#}");
                false
            }
        }
    }

    fn note_cost(&self, started: Instant) {
        let d = &self.telemetry.degradation;
        d.cursor_composites.fetch_add(1, Ordering::Relaxed);
        d.cursor_composite_us
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    /// Erase whatever is drawn on this slot, then draw the pointer where it is now.
    ///
    /// Returns whether any pixel was touched.
    fn render(&mut self, slot: usize, want: Option<Pointer>) -> Result<bool> {
        if slot >= self.slots.len() {
            bail!("cursor overlay: slot {slot} out of range");
        }

        let mut touched = false;
        if let Some(r) = self.slots[slot].drawn.take() {
            self.restore(slot, r)?;
            touched = true;
        }
        self.slots[slot].shows = want;

        let Some(p) = want else {
            return Ok(touched);
        };
        if !self.ensure_sprite(p.shape) {
            return Ok(touched);
        }

        // The geometry is read out and the borrow dropped before `save_undo` needs `&mut self`.
        // `ensure_sprite` has just put the one we want at the front.
        let (x, y, w, h) = match self.sprites.first() {
            Some((_, sprite)) => place_sprite(&self.geometry(), p.x, p.y, sprite),
            None => return Ok(touched),
        };
        let Some(clip) = clip_to(x, y, w, h, self.dst_w, self.dst_h) else {
            // Entirely off the destination: the pointer is on this monitor but its sprite lands
            // outside the encoded area, which a video-scale below 100 % makes reachable.
            return Ok(touched);
        };
        self.save_undo(slot, clip)?;

        let verts = quad_verts(x, y, w, h, self.dst_w, self.dst_h);
        let (dst_w, dst_h) = (self.dst_w, self.dst_h);
        // Split the borrow: `draw` takes `&self.renderer` while the sprite and the slot's view
        // are borrowed out of the same struct.
        let Self {
            renderer,
            sprites,
            slots,
            ..
        } = self;
        let Some((_, sprite)) = sprites.first() else {
            return Ok(touched);
        };
        for (srv, blend) in &sprite.passes {
            renderer.draw(&slots[slot].rtv, srv, *blend, &verts, dst_w, dst_h)?;
        }
        self.slots[slot].drawn = Some(clip);
        Ok(true)
    }

    fn geometry(&self) -> Geometry {
        Geometry {
            origin_x: self.origin_x,
            origin_y: self.origin_y,
            src_w: self.src_w,
            src_h: self.src_h,
            dst_w: self.dst_w,
            dst_h: self.dst_h,
            dpi_scale: self.dpi_scale,
        }
    }

    fn save_undo(&mut self, slot: usize, r: Rect) -> Result<()> {
        if r.w > MAX_UNDO_DIM || r.h > MAX_UNDO_DIM {
            // Refuse rather than allocate without bound. The sprite is still drawn; it is the
            // *erase* that will not happen, so the next recompose leaves a trail — which is why
            // this also says so once.
            bail!(
                "cursor sprite is {}x{} destination pixels, past the {MAX_UNDO_DIM} px undo cap",
                r.w,
                r.h
            );
        }
        let (have, undo_w, undo_h) = {
            let s = &self.slots[slot];
            (s.undo.is_some(), s.undo_w, s.undo_h)
        };
        if !have || undo_w < r.w || undo_h < r.h {
            let w = r.w.max(undo_w).min(MAX_UNDO_DIM);
            let h = r.h.max(undo_h).min(MAX_UNDO_DIM);
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: 0,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                .context("CreateTexture2D (cursor undo)")?;
            let s = &mut self.slots[slot];
            s.undo = Some(tex.context("cursor undo texture was null")?);
            s.undo_w = w;
            s.undo_h = h;
        }
        let undo = self.slots[slot]
            .undo
            .as_ref()
            .expect("undo texture created above");
        let src = D3D11_BOX {
            left: r.x,
            top: r.y,
            front: 0,
            right: r.x + r.w,
            bottom: r.y + r.h,
            back: 1,
        };
        unsafe {
            self.context.CopySubresourceRegion(
                undo,
                0,
                0,
                0,
                0,
                &self.textures[slot],
                0,
                Some(&src),
            );
        }
        Ok(())
    }

    fn restore(&mut self, slot: usize, r: Rect) -> Result<()> {
        let s = &self.slots[slot];
        let Some(undo) = s.undo.as_ref() else {
            bail!("cursor overlay: slot {slot} has a drawn rectangle but no saved pixels");
        };
        let src = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: r.w,
            bottom: r.h,
            back: 1,
        };
        unsafe {
            self.context.CopySubresourceRegion(
                &self.textures[slot],
                0,
                r.x,
                r.y,
                0,
                undo,
                0,
                Some(&src),
            );
        }
        Ok(())
    }

    /// Makes sure a usable sprite for this cursor is at the front of the cache.
    fn ensure_sprite(&mut self, shape: isize) -> bool {
        if let Some(i) = self.sprites.iter().position(|(h, _)| *h == shape) {
            self.sprites[..=i].rotate_right(1);
            return true;
        }
        if self.bad_shapes.contains(&shape) {
            return false;
        }
        let built = shape_from_hcursor(HCURSOR(shape as *mut core::ffi::c_void))
            .and_then(|s| build_sprite(&self.device, &s.info, &s.data));
        match built {
            Ok(sprite) => {
                self.sprites.truncate(MAX_CACHED_SPRITES - 1);
                self.sprites.insert(0, (shape, sprite));
                true
            }
            Err(e) => {
                // Remembered so a cursor we cannot read is not re-attempted 60 times a second,
                // and counted so a host drawing no pointer at all is visible in `status --json`.
                if self.bad_shapes.len() < MAX_CACHED_SPRITES {
                    self.bad_shapes.push(shape);
                }
                self.shape_errors += 1;
                self.telemetry
                    .degradation
                    .cursor_shape_errors
                    .fetch_add(1, Ordering::Relaxed);
                if self.shape_errors <= 3 {
                    teprintln!("cursor overlay: cannot read cursor shape {shape:#x}: {e:#}");
                }
                false
            }
        }
    }

    /// The pointer, if it is showing *and* over the display being captured.
    ///
    /// The bounds test is what keeps a pointer that has wandered onto the host's own screen from
    /// being painted onto the client's — the per-device pointer scope defaults to `all`, so it
    /// leaves this display all the time. It is a rectangle test against the monitor's own bounds
    /// rather than `MonitorFromPoint`, matching `dxgi/mod.rs::refresh_cursor_state`: the two agree
    /// wherever the rectangle is known, and where it is not the rectangle test still draws a
    /// pointer at a slightly wrong offset, where the handle test draws none at all.
    fn read_pointer(&mut self) -> Option<Pointer> {
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};

        // Before the bounds test, not after: the rectangle it tests against is what a `/reconfig`
        // moves, and testing against the old one puts the pointer on the wrong display for a
        // second.
        self.refresh_geometry();
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            self.note_absent("GetCursorInfo failed", 0, 0);
            return None;
        }
        if (info.flags.0 & CURSOR_SHOWING.0) == 0 || info.hCursor.is_invalid() {
            self.note_absent("the pointer is hidden or suppressed", 0, 0);
            return None;
        }
        let (x, y) = (info.ptScreenPos.x, info.ptScreenPos.y);
        let inside = x >= self.origin_x
            && x < self.origin_x + self.src_w as i32
            && y >= self.origin_y
            && y < self.origin_y + self.src_h as i32;
        if !inside {
            self.note_absent("the pointer is on another display", x, y);
            return None;
        }
        Some(Pointer {
            x,
            y,
            shape: info.hCursor.0 as isize,
        })
    }

    /// Says, a handful of times and never more often than every few seconds, why no pointer is
    /// being drawn.
    ///
    /// A host drawing no pointer because the user's mouse is on their own screen and a host
    /// drawing no pointer because its geometry is wrong look identical from the outside, and the
    /// second one is the whole failure this module exists to avoid.
    fn note_absent(&mut self, why: &str, x: i32, y: i32) {
        const MAX_LINES: u32 = 3;
        if self.absent_logged >= MAX_LINES || self.absent_logged_at.elapsed() < ABSENT_LOG_INTERVAL
        {
            return;
        }
        self.absent_logged += 1;
        self.absent_logged_at = Instant::now();
        teprintln!(
            "[cursor] no pointer drawn: {why} (pointer {x},{y}; this display {}x{} at {},{}, \
             hmonitor {:#x})",
            self.src_w,
            self.src_h,
            self.origin_x,
            self.origin_y,
            self.monitor
        );
    }

    fn refresh_geometry(&mut self) {
        if self.geometry_checked.elapsed() < GEOMETRY_REFRESH {
            return;
        }
        self.geometry_checked = Instant::now();
        if let Some((x, y, w, h)) = monitor_rect(self.monitor) {
            self.origin_x = x;
            self.origin_y = y;
            self.src_w = w.max(1);
            self.src_h = h.max(1);
        }
        self.dpi_scale = cursor_dpi_scale(self.monitor);
    }
}

/// `(left, top, width, height)` of a monitor in desktop pixels.
fn monitor_rect(monitor: isize) -> Option<(i32, i32, u32, u32)> {
    use windows::Win32::Graphics::Gdi::GetMonitorInfoW;

    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let hmon = HMONITOR(monitor as *mut core::ffi::c_void);
    if !unsafe { GetMonitorInfoW(hmon, &mut mi) }.as_bool() {
        return None;
    }
    let r = mi.rcMonitor;
    Some((
        r.left,
        r.top,
        (r.right - r.left).max(1) as u32,
        (r.bottom - r.top).max(1) as u32,
    ))
}

/// How much bigger the pointer should be drawn than the bitmap `GetIconInfoExW` handed over.
///
/// The cursor bitmap comes out at the DPI of the thread asking for it; the display being captured
/// may be running at another one, because the client picks its own scale. Comparing the two
/// `SM_CXCURSOR` metrics is self-correcting: where the two DPIs agree the ratio is exactly 1 and
/// nothing is scaled, and where they do not it is the ratio Windows would itself have used. Any
/// failure returns 1, which is the old behaviour rather than a wrong one.
fn cursor_dpi_scale(monitor: isize) -> f32 {
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetSystemMetricsForDpi, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXCURSOR};

    let base = unsafe { GetSystemMetrics(SM_CXCURSOR) };
    if base <= 0 {
        return 1.0;
    }
    let hmon = HMONITOR(monitor as *mut core::ffi::c_void);
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    if unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_err()
        || dpi_x == 0
    {
        return 1.0;
    }
    let want = unsafe { GetSystemMetricsForDpi(SM_CXCURSOR, dpi_x) };
    if want <= 0 {
        return 1.0;
    }
    (want as f32 / base as f32).clamp(0.25, 8.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sprite with no GPU behind it. Only the four numbers [`place_sprite`] reads are used.
    fn sprite(w: u32, h: u32, hx: i32, hy: i32) -> CursorSprite {
        CursorSprite {
            passes: Vec::new(),
            width: w,
            height: h,
            hotspot_x: hx,
            hotspot_y: hy,
        }
    }

    fn geom(origin: (i32, i32), src: (u32, u32), dst: (u32, u32), dpi: f32) -> Geometry {
        Geometry {
            origin_x: origin.0,
            origin_y: origin.1,
            src_w: src.0,
            src_h: src.1,
            dst_w: dst.0,
            dst_h: dst.1,
            dpi_scale: dpi,
        }
    }

    /// The hotspot is the pixel the user is pointing at, so it must land exactly on the pointer's
    /// own coordinate — not the sprite's corner. Getting this wrong is the classic "the cursor is
    /// drawn but clicks land somewhere else" bug, and it is invisible on an arrow whose hotspot is
    /// (0,0); every other shape (the I-beam, the resize handles, the hand) has a centred one.
    #[test]
    fn the_hotspot_lands_on_the_pointer() {
        let g = geom((0, 0), (1920, 1080), (1920, 1080), 1.0);
        let (x, y, w, h) = place_sprite(&g, 400, 300, &sprite(32, 32, 16, 16));
        assert_eq!((x, y, w, h), (384, 284, 32, 32));
        assert_eq!(x + 16, 400);
        assert_eq!(y + 16, 300);
    }

    /// The display's own origin is subtracted before anything else: a virtual display sits beside
    /// the host's panel in desktop coordinates, so a pointer at desktop x=2000 is at x=80 on a
    /// display whose left edge is 1920.
    #[test]
    fn the_display_origin_is_subtracted() {
        let g = geom((1920, 0), (1920, 1080), (1920, 1080), 1.0);
        let (x, y, ..) = place_sprite(&g, 2000, 40, &sprite(32, 32, 0, 0));
        assert_eq!((x, y), (80, 40));
    }

    /// A video scale below 100 % encodes fewer pixels than the display has, and the pointer has to
    /// shrink and move with them or it drifts further from the truth the further right it goes.
    #[test]
    fn the_video_scale_moves_and_shrinks_the_sprite() {
        let g = geom((0, 0), (1920, 1080), (960, 540), 1.0);
        let (x, y, w, h) = place_sprite(&g, 1920, 1080, &sprite(32, 32, 0, 0));
        assert_eq!((x, y, w, h), (960, 540, 16, 16));
    }

    /// A display at 200 % gets a cursor bitmap twice the size Windows handed us, so the sprite is
    /// drawn twice as large and its hotspot moves with it.
    #[test]
    fn the_dpi_factor_scales_the_sprite_and_its_hotspot() {
        let g = geom((0, 0), (1920, 1080), (1920, 1080), 2.0);
        let (x, y, w, h) = place_sprite(&g, 500, 500, &sprite(32, 32, 16, 16));
        assert_eq!((w, h), (64, 64));
        assert_eq!((x + 32, y + 32), (500, 500));
    }

    /// The undo rectangle must never run off the target: `CopySubresourceRegion` with a box past
    /// the edge is undefined, and the pointer sits half off the edge constantly.
    #[test]
    fn the_undo_rectangle_is_clipped_to_the_target() {
        assert_eq!(
            clip_to(-10, -10, 32, 32, 1920, 1080),
            Some(Rect {
                x: 0,
                y: 0,
                w: 22,
                h: 22
            })
        );
        assert_eq!(
            clip_to(1900, 1060, 32, 32, 1920, 1080),
            Some(Rect {
                x: 1900,
                y: 1060,
                w: 20,
                h: 20
            })
        );
        assert_eq!(clip_to(-40, 10, 32, 32, 1920, 1080), None);
        assert_eq!(clip_to(1920, 10, 32, 32, 1920, 1080), None);
        assert_eq!(clip_to(10, 1080, 32, 32, 1920, 1080), None);
    }

    /// Two pointer states that draw the same picture must compare equal, or the encode thread
    /// re-encodes a frame per tick for a pointer that has not moved.
    #[test]
    fn an_unmoved_pointer_compares_equal() {
        let a = Pointer {
            x: 10,
            y: 20,
            shape: 0x1234,
        };
        assert_eq!(a, a);
        assert_ne!(a, Pointer { x: 11, ..a });
        assert_ne!(a, Pointer { shape: 0x5678, ..a });
    }
}

# F — `windows_utils/windows_capture/`: the vendored `windows-capture` fork

Territory: every file in `src-tauri/src/windows_utils/windows_capture/`.
Read in full, then diffed against upstream.

## Upstream identification and diff method

- `git log --follow` on the directory shows three commits: `ad598b2` "refactor(windows): vendor
  windows-capture crate in-tree" (2026-06-30, the vendoring), `37c0f82` "build: add tauri updater
  plugin" (2026-08-01, a whole-directory rustfmt reflow to `max_width = 100` plus let-chain
  desugaring — no semantic change), and `5ba1183` "perf(windows): decouple NVENC capture from encode
  via a texture ring" (2026-08-28, one line).
- Upstream version: **`windows-capture` 2.0.0**, from `git show ad598b2^:src-tauri/Cargo.lock`
  (checksum `0f9460c7b82f6b7d314d85b45610481f26a1159a42dadf1e13a329041553e060`). Nothing in the tree
  records this; the vendoring commit deleted the previous record,
  `src-tauri/vendor/windows-capture.patch`.
- No copy exists under `~/.cargo/registry/src/`. I fetched
  `https://static.crates.io/crates/windows-capture/windows-capture-2.0.0.crate` and verified the
  sha256 matches the lockfile byte for byte.
- Diff method: copied both trees, applied `rustfmt --edition 2024 --config max_width=100` to each
  file, stripped CRs from the upstream copy, then `diff -u` per file. This removes the `37c0f82`
  reflow noise so only semantic divergence remains. Upstream is edition 2024 and uses let-chains;
  this crate is edition 2021, which is why the fork desugars them.
- Upstream `src/lib.rs` -> fork `mod.rs`: **byte-identical** after normalization.

## File-by-file divergence from upstream 2.0.0

| file | fork LOC | divergence |
|---|---|---|
| `mod.rs` | 193 | none (upstream `lib.rs` verbatim, crate-level attributes and all) |
| `settings.rs` | 181 | `crate::` -> `super::` only |
| `graphics_capture_picker.rs` | 171 | `crate::` -> `super::` only |
| `monitor.rs` | 520 | `crate::` -> `super::` only |
| `encoder.rs` | 1481 | `crate::` -> `super::`, let-chain desugar. **No semantic change.** |
| `winrt.rs` | 49 | two panics removed; `new()` returns `Result` |
| `window.rs` | 402 | `EnumChildWindows(GetDesktopWindow())` -> `EnumWindows`; title-bar height `.max(0)` before `as u32` |
| `graphics_capture_api.rs` | 545 | frame-pool buffer count 1 -> 3; `expect` removed; `stop_capture`/`Drop` share `cleanup()` |
| `capture.rs` | 541 | message-loop `-1` handling; `GetThreadId == 0`; `yield_now`; 6 panics -> typed errors; helper extraction |
| `d3d11.rs` | 341 | `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` + fallback; `StagingTexture`/`MappedStagingTexture` RAII; `from_raw_checked` mask fix; `unwrap` -> `UnexpectedNullResult` |
| `frame.rs` | 500 | staging texture now RAII-unmapped; `has_padding()` uses real bpp; `capacity()` -> `len()`; format taken from `desc.Format` not `color_format` |
| `dxgi_duplication_api.rs` | 951 | same RAII rework; 4 formats **removed**; `DuplicateOutput` -> `DuplicateOutput1` with a 3-format allow-list; `Drop` releases a held frame; helper extraction |

Detail on the load-bearing ones:

**`d3d11.rs` — the reason the fork exists.** `create_d3d_device()` (d3d11.rs:176-233) tries
`D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT` first and falls back to
BGRA-only on any error. `VIDEO_SUPPORT` is what lets the same `ID3D11Device` be handed to NVENC /
oneVPL; without it the zero-copy GPU encode paths cannot share the capture device. This was the
original out-of-tree `.patch` before vendoring. It also adds a reusable `StagingTexture` (:251-341)
and an RAII `MappedStagingTexture` that unmaps on `Drop` (:74-165) — upstream mapped and never
unmapped.

**`frame.rs`/`dxgi_duplication_api.rs` — three upstream bugs fixed.** Upstream's `FrameBuffer` held a
raw `&mut [u8]` into a mapped texture that was never unmapped and whose texture was dropped at the
end of the producing function (a dangling map plus a live COM object leak). Upstream's
`has_padding()` compared `width * 4` against the row pitch, wrong for `Rgba16F` (8 bpp). Upstream's
`as_nopadding_buffer` resized on `buffer.capacity() < frame_size`, which can leave `len()` short and
hand back an under-length slice. The fork fixes all three (frame.rs:297-316, 414-416, 440-442).

**`graphics_capture_api.rs:196-198`** — the only performance-motivated divergence:
`Direct3D11CaptureFramePool::Create(&direct3d_device, pixel_format, 3, item.Size()?)`, raised from
upstream's `1` by `5ba1183`. **The matching `Recreate` at :286-291 still passes `1`.**

## 1. Files and purpose

- **`mod.rs`** (193) — upstream's crate root, vendored verbatim: module declarations, the
  `GraphicsCaptureItem` re-export, a long crate-level doc example, and four `#![warn(...)]`
  attributes (`clippy::nursery`, `clippy::cargo`, `clippy::multiple_crate_versions`, `missing_docs`)
  that now apply from inside a module of a much larger crate.
- **`winrt.rs`** (49) — RAII WinRT init. `WinMTACookie` wraps `CoIncrementMTAUsage`/`CoDecrementMTAUsage`;
  `WinRT` additionally calls `RoInitialize(RO_INIT_MULTITHREADED)` (tolerating `S_FALSE`) and
  `RoUninitialize` on drop. Both fork-hardened to return `Result` instead of panicking.
- **`settings.rs`** (181) — the configuration value types: `GraphicsCaptureItemType`
  (Monitor/Window/Unknown, each carrying the WinRT `GraphicsCaptureItem`), `ColorFormat`
  (`Rgba16F`/`Rgba8`/`Bgra8`, discriminants are `DXGI_FORMAT` values), and five tri-state option
  enums (`CursorCaptureSettings`, `DrawBorderSettings`, `SecondaryWindowSettings`,
  `MinimumUpdateIntervalSettings`, `DirtyRegionSettings`) plus the `Settings<Flags, T>` bag.
- **`capture.rs`** (541) — the public entry point. `GraphicsCaptureApiHandler` (the trait ScreenExtend
  implements), `Context<Flags>`, `CaptureControl<T, E>`, and the two starters: `start` (takes over
  the calling thread) and `start_free_threaded` (spawns one). Both build a `DispatcherQueueController`
  on the capture thread and run a Win32 message loop; shutdown is `PostThreadMessageW(WM_QUIT)`.
- **`graphics_capture_api.rs`** (545) — the WGC session itself: capability probing, frame-pool
  creation, the `FrameArrived` and `Closed` event handlers, the five optional-setting appliers, and
  `cleanup()`. This is where every knob the brief asks about lives.
- **`frame.rs`** (500) — `Frame<'a>` (a borrowed view over one `Direct3D11CaptureFrame`: its
  `IDirect3DSurface`, the `ID3D11Texture2D` behind it, the device + immediate context, the
  `D3D11_TEXTURE2D_DESC`, and a pre-computed title-bar height) plus `FrameBuffer<'a>` (a CPU
  mapping) and `DirtyRegion`.
- **`d3d11.rs`** (341) — device creation, `SendDirectX<T>` (an unconditional `unsafe impl Send`
  wrapper), `StagingTexture`, `MappedStagingTexture`, `unmap_staging_texture`.
- **`monitor.rs`** (520) — `Monitor` (a `HMONITOR` newtype, `unsafe impl Send`) with enumeration,
  index/name/device-name lookup via `GetMonitorInfoW` + `QueryDisplayConfig`, geometry and refresh
  rate via `EnumDisplaySettingsW`, and `TryInto<GraphicsCaptureItemType>` via
  `IGraphicsCaptureItemInterop::CreateForMonitor`. **Used by ScreenExtend.**
- **`window.rs`** (402) — the same for `HWND`. **Unused by ScreenExtend.**
- **`graphics_capture_picker.rs`** (171) — shows the system capture-picker dialog behind a hidden
  off-screen tool window at (-69000, -69000), pumping messages until the async op completes.
  **Unused by ScreenExtend** (it is a headless host; there is nobody to click it).
- **`dxgi_duplication_api.rs`** (951) — a complete DXGI Desktop Duplication wrapper.
  **Unused by ScreenExtend**, which has its own in `windows_utils/streamer/dxgi/`.
- **`encoder.rs`** (1481) — two unrelated things. `ImageEncoder` (:95-192, WIC `BitmapEncoder` to
  PNG/JPEG/GIF/TIFF/BMP/JPEG-XR) is **used**, via `Frame::save_as_image` on the `--probe-capture`
  path. `VideoEncoder` (:659-1481) is upstream's Media Foundation file encoder —
  `MediaStreamSource` + `MediaTranscoder` + `MediaEncodingProfile`, writing H.264/HEVC into an MP4
  with optional AAC audio, samples pulled by a `SampleRequested` handler that offloads to the WinRT
  `ThreadPool`. It is **dead code**: ScreenExtend never writes a file, uses NVENC/QSV/x264 directly,
  and nothing imports it.

## 2. Function catalog

### `winrt.rs`
- `WinMTACookie::new` — `CoIncrementMTAUsage`; fork returns `Result` (upstream `expect`ed).
- `WinMTACookie::drop` — `CoDecrementMTAUsage`, result discarded.
- `WinRT::new` — cookie + `RoInitialize(RO_INIT_MULTITHREADED)`; treats `S_FALSE` (already
  initialized, different mode) as success.
- `WinRT::drop` — `RoUninitialize`.

### `settings.rs`
- `Settings::new` — `const`, eight arguments, no validation whatsoever.
- `Settings::{item, cursor_capture, draw_border, color_format, flags}` — accessors. Note there is
  **no** accessor for `secondary_window_settings`, `minimum_update_interval_settings`, or
  `dirty_region_settings` — an upstream omission carried over.
- `ColorFormat::default` — `Rgba8`. ScreenExtend always passes `Bgra8` explicitly.

### `capture.rs`
- `dispatcher_queue_options` (:27) — `const`, `DQTYPE_THREAD_CURRENT` + `DQTAT_COM_NONE`. Fork
  extraction; upstream inlined it with a `u32::try_from(...).unwrap()`.
- `run_message_loop<E>` (:35) — `GetMessageW` loop. `-1` -> `FailedToRunMessageLoop`, `0` -> `Ok`.
  **Fork bug fix**: upstream's `while GetMessageW(..).as_bool()` treats the `-1` error return as
  `true` and spins forever.
- `join_capture_thread<E>` (:50) — join + flatten. Fork extraction.
- `CaptureControl::{new, is_finished, into_thread_handle, halt_handle, callback, wait, stop}`
  (:102-223) — `stop` sets `halt`, resolves the thread id, posts `WM_QUIT` in a retry loop that
  yields on `ERROR_INVALID_THREAD_ID`, then joins. Fork adds the `thread_id == 0` branch (:186) and
  the `yield_now` (:211).
- `GraphicsCaptureApiHandler::start` (:300) — blocking: WinRT init, dispatcher queue, D3D device,
  handler construction, `GraphicsCaptureApi::new`, `start_capture`, message loop, queue shutdown,
  second message loop, `stop_capture`, handler-error check.
- `GraphicsCaptureApiHandler::start_free_threaded` (:391) — the same on a spawned thread, publishing
  the halt handle and the callback `Arc` back over two `std::sync::mpsc` channels. **This is the one
  ScreenExtend's live path uses** (pipeline.rs:349).
- `GraphicsCaptureApiHandler::{new, on_frame_arrived, on_closed}` — the trait surface.

### `graphics_capture_api.rs`
- `InternalCaptureControl::{new, stop}` (:79-92) — a one-shot `Arc<AtomicBool>` the handler can set
  to request teardown. A fresh one is allocated **per frame** (:314).
- `GraphicsCaptureApi::new` (:126) — the whole session build-up. Order: five capability gates,
  title-bar pre-compute, `create_direct3d_device`, frame-pool `Create` with **3** buffers,
  `CreateCaptureSession`, `Closed` handler, `FrameArrived` handler, then the five setting appliers.
- `GraphicsCaptureApi::start_capture` (:436) — `AlreadyStarted` guard then `session.StartCapture()`.
- `GraphicsCaptureApi::stop_capture` (:451) / `Drop` (:541) — both call `cleanup()`.
- `GraphicsCaptureApi::halt_handle` (:462).
- `GraphicsCaptureApi::is_supported` (:468) — `UniversalApiContract` major **8** present **and**
  `GraphicsCaptureSession::IsSupported()`.
- `is_cursor_settings_supported` / `is_border_settings_supported` /
  `is_secondary_windows_supported` / `is_minimum_update_interval_supported` /
  `is_dirty_region_supported` (:477-518) — all five are `ApiInformation::IsPropertyPresent` on
  `Windows.Graphics.Capture.GraphicsCaptureSession` for `IsCursorCaptureEnabled`, `IsBorderRequired`,
  `IncludeSecondaryWindows`, `MinUpdateInterval`, `DirtyRegionMode` respectively, `&&`'d with
  `is_supported()`. Each re-queries `is_supported()`, so `new()` performs on the order of a dozen
  `ApiInformation` calls at session start (startup cost only).
- `cleanup` (:520) — `RemoveFrameArrived`, `Close` the pool, `Close` the session, `RemoveClosed` on
  the item, `active = false`. Every result discarded with `let _`.

### `frame.rs`
- `Frame::new` (:86) — `const`, stores the eight fields.
- `Frame::{width, height, color_format, as_raw_surface, as_raw_texture, device, device_context, desc}`
  — accessors; `as_raw_texture()` is what the pipeline hands to NVENC/the scaler/the ring.
- `Frame::dirty_regions` (:116) — maps `Direct3D11CaptureFrame::DirtyRegions()`. **No callers.**
- `Frame::timestamp` (:139) — `SystemRelativeTime()` (QPC-based). **No callers in ScreenExtend**;
  the pipeline stamps its own `Instant::now()` in `on_frame_arrived`.
- `Frame::buffer` (:187) — new staging texture + `CopyResource` + `Map`. **Per-frame allocation.**
- `Frame::buffer_crop` (:213) — same with `CopySubresourceRegion`.
- `Frame::buffer_without_title_bar` (:270) — crop or passthrough.
- `Frame::save_as_image` (:284) — `buffer()` then `FrameBuffer::save_as_image`.
- `FrameBufferBacking::{as_slice, as_mut_slice}` (:302-316) — fork addition, dispatches
  Borrowed/Mapped.
- `FrameBuffer::{new, from_mapped, width, height, row_pitch, depth_pitch, color_format,
  has_padding, as_raw_buffer, as_nopadding_buffer, save_as_image, bytes_per_pixel}` (:339-499).
  `as_nopadding_buffer` de-pitches with a **rayon `into_par_iter` over rows** using raw pointers
  laundered through `usize`.

### `d3d11.rs`
- `SendDirectX<T>::new` + `unsafe impl<T> Send` (:38-50) — a blanket `Send` for any COM object.
  Used to move `IDirect3DDevice` into the `FrameArrived` closure and `IDirect3DSurface` into the
  encoder's channel.
- `StagingTextureHandle::{texture, set_mapped}` (:52-71) — Owned/Borrowed dispatch.
- `MappedStagingTexture::{map_owned, map_borrowed, map, as_slice, as_mut_slice, row_pitch,
  depth_pitch}` + `Drop` (:74-155) — `Drop` calls `Unmap`. `as_slice(rows)` builds a slice of
  `rows * RowPitch` bytes from `mapped.pData` — unchecked, trusts the caller's row count.
- `unmap_staging_texture` (:158).
- `create_d3d_device` (:176) — hardware driver type, 7 feature levels, `VIDEO_SUPPORT` then fallback,
  rejects below FL 11.0.
- `create_direct3d_device` (:242) — `IDXGIDevice` cast + `CreateDirect3D11DeviceFromDXGIDevice`.
- `StagingTexture::{new, texture, desc, is_mapped, set_mapped, from_raw_checked}` (:257-340).

### `monitor.rs`
- `Monitor::primary` (:122) — `MonitorFromPoint((0,0), MONITOR_DEFAULTTONULL)`.
- `Monitor::from_index` (:141) — 1-based index into `enumerate()`.
- `Monitor::index` (:164) — parses the trailing digits of `\\.\DISPLAYn`.
- `Monitor::name` (:177) — `GetDisplayConfigBufferSizes` + `QueryDisplayConfig` +
  `DisplayConfigGetDeviceInfo`, matched back to `device_name()`; `NameNotFound` if no path matches.
- `Monitor::device_name` (:271) / `device_string` (:313) — `GetMonitorInfoW` / `EnumDisplayDevicesW`.
- `Monitor::{refresh_rate, width, height}` (:376, :405, :434) — three near-identical
  `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` calls, each of which first re-derives
  `device_name()` (so `width()` + `height()` + `refresh_rate()` = 3 `GetMonitorInfoW` + 3
  `EnumDisplaySettingsW`). Startup-path only.
- `Monitor::enumerate` (:460) + `enum_monitors_callback` (:494) — `EnumDisplayMonitors` pushing into
  a `Vec` through an `LPARAM` raw pointer.
- `Monitor::{from_raw_hmonitor, as_raw_hmonitor}` (:479, :488).
- `TryInto<GraphicsCaptureItemType> for Monitor` (:512) — `IGraphicsCaptureItemInterop::CreateForMonitor`.

### `window.rs` (unused)
- `foreground` (:89), `from_name` (:106), `from_contains_name` (:125), `title` (:145),
  `process_id` (:170), `process_name` (:190) (opens the process, `GetModuleBaseNameW`),
  `monitor` (:222), `rect` (:240), `title_bar_height` (:256, DPI-corrected, fork adds `.max(0)`),
  `is_valid` (:287), `enumerate` (:330, fork switched to `EnumWindows`), `width`/`height`
  (:348, :358), `from_raw_hwnd`/`as_raw_hwnd` (:366, :373), `enum_windows_callback` (:379),
  `TryInto<GraphicsCaptureItemType>` (:394).

### `graphics_capture_picker.rs` (unused)
- `wnd_proc` (:32), `HwndGuard::drop` (:47, destroys the window then drains **all** thread messages
  with `PeekMessageW(PM_REMOVE)` — it will eat messages that belong to other windows on that thread),
  `PickedGraphicsCaptureItem::size` (:69), `GraphicsCapturePicker::pick_item` (:94),
  `TryInto<GraphicsCaptureItemType>` (:168).

### `dxgi_duplication_api.rs` (unused)
- `enable_per_monitor_dpi_awareness` (:133) — `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`,
  treats `E_ACCESSDENIED` (already set) as success. **This mutates process-wide DPI state as a side
  effect of constructing a duplication session.**
- `find_output_for_monitor` (:141) — `EnumOutputs` loop matched on `HMONITOR`.
- `map_supported_formats` (:165) — always appends BGRA8.
- `DxgiDuplicationApi::{release_frame_if_needed (:183), new (:202), new_options (:233),
  recreate (:264), recreate_options (:280), device, device_context, duplication, duplication_desc,
  dxgi_device, output, width, height, format, refresh_rate, acquire_next_frame (:398)}` + `Drop` (:448).
- `DxgiDuplicationFrame::{width, height, format, device, device_context, duplication, texture,
  texture_desc, frame_info (:532), buffer (:544), buffer_crop (:570), buffer_with (:631),
  buffer_crop_with (:669), save_as_image (:734)}`.
- `DxgiDuplicationFrameBuffer::{new, from_mapped, width, height, row_pitch, depth_pitch, format,
  has_padding, as_nopadding_buffer, as_raw_buffer, save_as_image, bytes_per_pixel}`.

### `encoder.rs`
- **Used:** `ImageEncoder::new` (:124, resolves a WIC encoder GUID + `BitmapPixelFormat`),
  `ImageEncoder::encode` (:162, `InMemoryRandomAccessStream` -> `BitmapEncoder::CreateAsync` ->
  `SetPixelData(Premultiplied)` -> `FlushAsync` -> `DataReader::ReadBytes`; four blocking `.join()`
  calls on WinRT async ops).
- **Dead:** `VideoSettingsBuilder` (:273-348), `AudioSettingsBuilder` (:360-421),
  `ContainerSettingsBuilder` (:432-450), the three `to_hstring` subtype tables (:508, :585, :642),
  `VideoEncoder::create_cached_surface` (:727), `attach_sample_requested_handlers` (:775),
  `new` (:894), `new_from_stream` (:1052), `build_padded_surface` (:1175), `send_frame` (:1245),
  `send_frame_with_audio` (:1283), `send_frame_buffer` (:1345), `send_audio_buffer` (:1384),
  `finish` (:1418), `Drop` (:1455), `unsafe impl Send for VideoEncoder` (:1481).

## 3. Threads

The fork spawns exactly two kinds of thread, and only the first is used by ScreenExtend.

1. **The capture thread** — `thread::spawn` in `start_free_threaded` (capture.rs:401). Default
   priority, default stack, no QoS/MMCSS of its own (ScreenExtend applies its tuning separately
   inside the handler via `windows_utils/streamer/tuning.rs`). It owns: the `WinRT` MTA
   registration, the `DispatcherQueueController`, the `ID3D11Device` + **immediate**
   `ID3D11DeviceContext`, the `GraphicsCaptureApi` (and therefore the frame pool and session), and
   the `Arc<Mutex<T>>` handler. Lifetime: from `start_free_threaded` until `WM_QUIT` is posted
   (either by `CaptureControl::stop`, by the `Closed` handler, or by the `FrameArrived` handler when
   the user callback errors or calls `InternalCaptureControl::stop`). It runs **two** message loops:
   one for the session, then one to drain the dispatcher-queue shutdown. `start` (the blocking
   variant, used by `--probe-capture`) does the same work on the caller's thread.
   **All `FrameArrived` callbacks — and therefore the entire ScreenExtend encode on the classic path
   — run on this one thread**, because the pool is built with `Create`, not `CreateFreeThreaded`.
2. **`VideoEncoder`'s transcode thread** (encoder.rs:1020, dead code) — `thread::spawn` running
   `TranscodeAsync().join()`, plus WinRT `ThreadPool` work items per sample request
   (`WorkItemPriority::Normal`). Never instantiated.

Not threads but worth noting: `FrameBuffer::as_nopadding_buffer` and its DXGI twin dispatch one
rayon task **per pixel row** onto the global rayon pool (frame.rs:447, dxgi_duplication_api.rs:891).
For a 4K frame that is 2160 tasks. Only the `--probe-capture` PNG path reaches it in ScreenExtend;
the live CPU-bridge path uses `as_raw_buffer()` and lets x264 deal with the pitch.

## 4. Locks

| lock | guards | taken by | hot path? | notes |
|---|---|---|---|---|
| `Arc<parking_lot::Mutex<T>>` (the handler) | the user's `GraphicsCaptureApiHandler` | the `FrameArrived` closure (graphics_capture_api.rs:318), the `Closed` closure (:220), and anyone holding the clone from `CaptureControl::callback()` (capture.rs:143) | **yes** | Held for the *entire* `on_frame_arrived`, i.e. across the whole encode on the classic path. Uncontended today (no `.callback()` callers) — see `speculative-wgc-handler-mutex-is-held-for-the-whole-encode.md`. |
| `Arc<parking_lot::Mutex<Option<E>>>` (`result`) | the last handler error | both event closures (:222, :325) and the thread epilogue (capture.rs:381, 493) | on error only | Trivial critical section. |
| `Arc<AtomicBool>` `halt` | "capture is finished" | set by `Closed` (:217), by `FrameArrived` on error (:328), by `CaptureControl::stop` (capture.rs:179); read at the top of every `FrameArrived` (:256) | yes | All `Ordering::Relaxed`. Relaxed is fine for the flag itself, but note it is the *only* synchronization between `stop()` and the callback — there is no acquire/release pairing with any data. |
| `Arc<AtomicBool>` `stop` (`InternalCaptureControl`) | per-frame stop request | allocated fresh each frame (:314), read at :323 | yes | One `Arc` allocation + deallocation **per frame**. Small, but it is a heap allocation on the capture thread inside the frame callback. |
| `Arc<(AtomicI32, AtomicI32)>` `last_size` | last observed content size | `FrameArrived` only (:282-298) | yes | Single-threaded in practice; the `Arc<(Atomic, Atomic)>` is pure overhead. |
| `Arc<parking_lot::Mutex<mpsc::Receiver<..>>>` x2 (encoder.rs:38-40) | the sample channels | WinRT thread-pool work items | n/a | Dead code. |

**No `std::sync::Mutex` and no `.await` anywhere in this module**, so CLAUDE.md's "never hold a guard
across an `.await`" rule cannot be violated here. The `if let` scrutinee temporary-lifetime pattern
the brief asks about **does** appear, at graphics_capture_api.rs:318-320 — the guard from
`callback_frame_pool.lock()` is a temporary that lives to the end of the statement, which is the
whole user callback. There is no `.await` inside (the callback is sync), so it is a blocking-duration
concern, not a deadlock one. `frame.rs`/`d3d11.rs` have no locks at all.

## 5. Buffers and copies

Following one frame on ScreenExtend's live WGC path:

1. **DWM -> frame pool texture.** WGC writes into one of the pool's 3 `Direct3D11CaptureFrame`
   surfaces. No copy on our side; the surface is GPU-resident.
2. **Surface -> `ID3D11Texture2D`.** `frame_surface.cast::<IDirect3DDxgiInterfaceAccess>()` +
   `GetInterface::<ID3D11Texture2D>()` (graphics_capture_api.rs:273-275). **Zero-copy**, a COM
   QueryInterface (two per frame: the cast and the interface fetch, each an AddRef).
3. **`GetDesc`** (:279) — one D3D call per frame to fill `D3D11_TEXTURE2D_DESC` (128-ish bytes,
   stack).
4. **`Frame` construction** (:302-311) — stack struct holding two `&` and three cloned COM
   interfaces (`Direct3D11CaptureFrame`, `IDirect3DSurface`, `ID3D11Texture2D`). Three AddRefs, no
   pixel copy.
5. **Handoff to the pipeline.** Three possibilities, all outside this module but driven by
   `as_raw_texture()`:
   - **Ring path (NVENC):** `ring.stage(frame.as_raw_texture())` (pipeline.rs:1702) —
     one **GPU->GPU** `CopyResource` into a shared texture, then the handler returns.
   - **GPU path (NVENC/QSV direct):** `backend.encode_gpu(frame.as_raw_texture(), scaler, ...)`
     (pipeline.rs:925) — **zero-copy** into the encoder, or GPU->GPU through the scaler.
   - **CPU-bridge path (x264 / IntelCpu / NVENC-CpuBridge):** either
     `scaler.scale(as_raw_texture())` + `scaler.read_back()` (GPU->GPU then **GPU->CPU readback**),
     or `frame.buffer()` (pipeline.rs:920) which is
     **`CreateTexture2D` (allocation, per frame) + `CopyResource` (GPU->CPU staging) + `Map`**.
     See `major-wgc-frame-buffer-allocates-a-staging-texture-per-frame.md`.
6. **Teardown.** `Frame` drops -> three Releases; the `Direct3D11CaptureFrame` Release is what
   returns the buffer to the pool.

Allocations per frame inside this module: one `Arc<AtomicBool>` (the `InternalCaptureControl`), plus
— on the CPU-bridge-without-scaler path — one full-resolution staging texture. Everything else is
COM refcount traffic.

`FrameBuffer::as_nopadding_buffer` is the only CPU memcpy in the module: `width * bpp` bytes per row,
`height` rows, in parallel, into a caller-supplied `Vec`. Not on ScreenExtend's live path.

`SendDirectX<T>` (d3d11.rs:38-50) is an unconditional `unsafe impl Send` over any `T`. It is used to
smuggle the `IDirect3DDevice` into the `FrameArrived` closure (:252). The device is agile
(free-threaded), so this is sound in practice, but the wrapper is blanket and would silently launder
a non-agile apartment-threaded object too.

## 6. Queues

- **The WGC frame pool itself** — depth **3** (graphics_capture_api.rs:197), depth **1** after any
  resize (:289, the bug). Bounded by construction. Back-pressure: when full, WGC drops or overwrites
  rather than blocking the compositor. `TryGetNextFrame` dequeues the **oldest**, and the handler
  calls it exactly once with no drain loop — see
  `major-wgc-framepool-never-drains-to-the-newest-frame.md`.
- **The thread's Win32 message queue / DispatcherQueue** — unbounded, OS-managed. Every
  `FrameArrived` is one queued item. A slow handler backs this up in lockstep with the pool.
- **`std::sync::mpsc` x2 in `start_free_threaded`** (capture.rs:398-399) — unbounded, used exactly
  once each at startup to publish the halt handle and the callback `Arc`. The parent blocks on
  `recv()` (:502, :512); if the thread died first, `recv` errors and the parent joins to recover the
  real error. Fork-hardened (upstream `unwrap`ed).
- **`VideoEncoder`'s two `mpsc` channels** (encoder.rs:697-698) — unbounded, dead code. Worth noting
  the shape anyway: producers never block, and the WinRT thread-pool consumer does a blocking
  `recv()` while holding the `parking_lot` mutex on the receiver.

No ring buffers, no `crossbeam` channels, no `tokio` anything in this module.

## 7. Syscall / FFI boundaries

All Win32/WinRT via the `windows` crate 0.62 (+ `windows-future` 0.3.2); no hand-written FFI, no
versioned structs beyond the two `cbSize`/`dmSize`/`cb` fields below. No macOS/dyld exposure — this
module is `cfg(windows)` only.

- **WinRT activation/apartment:** `CoIncrementMTAUsage`, `CoDecrementMTAUsage`,
  `RoInitialize(RO_INIT_MULTITHREADED)`, `RoUninitialize` (winrt.rs).
- **DispatcherQueue:** `CreateDispatcherQueueController(DispatcherQueueOptions { dwSize:
  size_of::<DispatcherQueueOptions>(), DQTYPE_THREAD_CURRENT, DQTAT_COM_NONE })` (capture.rs:27-33).
  `dwSize` is a **versioned-struct field** — computed from `size_of`, correct.
- **Win32 messaging:** `GetCurrentThreadId`, `GetThreadId`, `GetMessageW`, `TranslateMessage`,
  `DispatchMessageW`, `PostThreadMessageW`, `PostQuitMessage`.
- **WinRT Graphics.Capture:** `Direct3D11CaptureFramePool::Create` / `Recreate` / `FrameArrived` /
  `RemoveFrameArrived` / `Close`, `CreateCaptureSession`, `StartCapture`, `Close`,
  `SetIsCursorCaptureEnabled`, `SetIsBorderRequired`, `SetIncludeSecondaryWindows`,
  `SetMinUpdateInterval`, `SetDirtyRegionMode`, `GraphicsCaptureItem::{Size, Closed, RemoveClosed}`,
  `Direct3D11CaptureFrame::{TryGetNextFrame, ContentSize, Surface, SystemRelativeTime, DirtyRegions}`.
- **Capability probing:** `ApiInformation::IsApiContractPresentByMajor("Windows.Foundation.UniversalApiContract", 8)`
  and five `ApiInformation::IsPropertyPresent(...)` calls (graphics_capture_api.rs:468-518).
- **WinRT<->D3D interop:** `IDirect3DDxgiInterfaceAccess::GetInterface::<ID3D11Texture2D>`,
  `CreateDirect3D11DeviceFromDXGIDevice`, `CreateDirect3D11SurfaceFromDXGISurface` (encoder, dead).
- **D3D11:** `D3D11CreateDevice` (`D3D11_SDK_VERSION`, 7 feature levels, hardware driver type),
  `CreateTexture2D`, `CopyResource`, `CopySubresourceRegion`, `Map`/`Unmap`, `GetDesc`,
  `CreateRenderTargetView`/`ClearRenderTargetView`/`Flush` (dead code).
- **DXGI (dead module):** `IDXGIDevice4::GetAdapter`, `EnumOutputs`, `IDXGIOutput6::DuplicateOutput1`,
  `AcquireNextFrame`, `ReleaseFrame`, `GetDesc`.
- **GDI/display (monitor.rs, window.rs):** `MonitorFromPoint`, `EnumDisplayMonitors`,
  `GetMonitorInfoW` (`MONITORINFOEXW.cbSize`), `EnumDisplayDevicesW` (`DISPLAY_DEVICEW.cb`),
  `EnumDisplaySettingsW` (`DEVMODEW.dmSize`, a `u16`), `GetDisplayConfigBufferSizes`,
  `QueryDisplayConfig`, `DisplayConfigGetDeviceInfo`, `GetDpiForWindow`, `EnumWindows`,
  `GetWindowLongPtrW`, `GetWindowTextW`, `OpenProcess`/`GetModuleBaseNameW`.
- **Process-wide side effect:** `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`
  (dxgi_duplication_api.rs:134). Dead here, but ScreenExtend sets DPI awareness itself in
  `streamer::prepare()` — two owners of one process-global.
- **WIC / Media Foundation (encoder.rs):** `BitmapEncoder`, `InMemoryRandomAccessStream`,
  `DataReader`, `CryptographicBuffer`, `MediaStreamSource`, `MediaTranscoder`, `StorageFile`,
  `ThreadPool`. All the Media Foundation ones are dead but still force the `Media_*`, `Storage*`,
  `Security_Cryptography` and `Graphics_Imaging` features on the `windows` dependency.
- **Callback ABI:** two `unsafe extern "system"` callbacks (`enum_monitors_callback`,
  `enum_windows_callback`) plus `wnd_proc`, each reconstructing a `&mut Vec<T>` from an `LPARAM`.

## 8. Panics

Runtime-reachable, in the code ScreenExtend actually compiles:

- `capture.rs:131` — `CaptureControl::into_thread_handle` does `self.thread_handle.unwrap()`. Panics
  if called after `wait()`/`stop()` took the handle. **No callers in ScreenExtend.**
- `dxgi_duplication_api.rs:364` and `:489` — `_ => unreachable!()` in the two `format()` matches.
  Reachable if DXGI ever reports a format outside the fork's three (the fork deleted the four
  10-bit/sRGB variants upstream had). Module is dead; filed as
  `minor-dxgi-vendored-wrapper-dropped-hdr-formats-behind-an-unreachable.md`.
- `monitor.rs:274, 316, 334, 378, 407, 436` — `u32::try_from(mem::size_of::<...>()).unwrap()` /
  `u16::try_from(...)`. Compile-time-constant sizes; cannot fail in practice.
- `window.rs:152, 311, 314` — `usize::try_from(len).unwrap()` on a window-title length, and two
  `isize::try_from(WS_*)`. `GetWindowTextLengthW` returns `i32`; a negative return would panic.
  Module is dead.
- `encoder.rs:753, 789, 948, 1104, 1200, 1273, 1335, 1374, 1409, 1441` — ten `expect`s in
  `VideoEncoder`, including `"CreateTexture2D returned None"` and four
  `"Failed to join transcode thread"` (which panic on a *panicking* transcode thread). All dead code.
- `frame.rs`, `d3d11.rs`, `graphics_capture_api.rs`, `winrt.rs`, `settings.rs`, `mod.rs` — **no
  panics at all.** Every one upstream had was converted to a typed error by the fork. This is the
  single biggest quality delta between the fork and 2.0.0.

Not a panic but adjacent: `MappedStagingTexture::as_slice(rows)` (d3d11.rs:123) builds a slice of
`rows * RowPitch` bytes with no validation that the mapping is that large. Every caller passes the
texture's own height, so it holds, but it is an unchecked `from_raw_parts` reachable from safe code.

## 9. Silent fallbacks and swallowed errors

- **`create_d3d_device` silently drops `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`** (d3d11.rs:218-222):
  `Err(_) => try_create(D3D11_CREATE_DEVICE_BGRA_SUPPORT)?`. The error is discarded and nothing is
  logged. A device created without `VIDEO_SUPPORT` cannot be shared with NVENC/oneVPL, so the
  fallback quietly forces the pipeline onto the CPU-bridge or a separate-device path. This is the
  fork's own patch, and it is the one place in the module where a *performance* tier degrades with
  no trace. It should `tprintln!` which flags succeeded.
- **`cleanup()` discards five results** (graphics_capture_api.rs:521-536): `RemoveFrameArrived`,
  `Close` (pool), `Close` (session), `RemoveClosed`. Defensible in a `Drop`-shared path, but
  `stop_capture()` also routes here, so a genuine teardown failure is invisible.
- **`WinMTACookie::drop`** discards `CoDecrementMTAUsage` (winrt.rs:21).
- **`recreate`/`recreate_options`** discard `release_frame_if_needed()` with `let _`
  (dxgi_duplication_api.rs:265, :286); `Drop` does the same (:450). Dead code.
- **`FrameArrived` returns `Ok(())` on a null pool argument** (graphics_capture_api.rs:261-263) —
  the fork's replacement for upstream's `expect`. Safer, but a frame silently vanishes with no
  counter and no log.
- **The `halt` early-return** (:256-258) drops frames silently by design during teardown.
- The big one is outside this module but caused by it: `GraphicsCaptureApi::new` returning
  `MinimumUpdateIntervalUnsupported`/`BorderConfigUnsupported` makes `start_on_monitor` fall through
  to DXGI duplication with only a `teprintln!` — already filed by another territory as
  `blocker-wgc-min-update-interval-forces-dxgi.md`. What this territory adds: the hard-fail is
  **upstream 2.0.0 behaviour, unmodified by the fork**, and the fork already owns the five
  `is_*_supported()` probes needed to downgrade instead. The fix is a fork patch, not a caller
  workaround.

## 10. TODO / FIXME / HACK / XXX

`grep -rn "TODO\|FIXME\|HACK\|XXX"` over the whole module: **none.**

## 11. Latency path

On the glass-to-glass path (Windows, WGC): `graphics_capture_api.rs` (the `FrameArrived` handler),
`frame.rs` (the `Frame` wrapper and, on the CPU-bridge path, `buffer()`), `d3d11.rs` (device +
staging), and `capture.rs` (the thread and its message loop). `monitor.rs` is startup-only.
`settings.rs`, `mod.rs`, `winrt.rs` are configuration/lifetime. `window.rs`,
`graphics_capture_picker.rs`, `dxgi_duplication_api.rs`, and `encoder.rs::VideoEncoder` are not on
any path.

Per-frame cost inside this module, in order (no measurements available on this machine — these are
structural, not timings):

| stage | cost | notes |
|---|---|---|
| DWM signal -> DispatcherQueue -> `GetMessageW` -> `DispatchMessageW` -> handler | one thread wake + scheduler jitter | avoidable with `CreateFreeThreaded`; see `minor-wgc-frame-delivery-goes-through-a-win32-message-pump.md` |
| `halt.load` | negligible | |
| `TryGetNextFrame` | returns the **oldest** pooled frame | with 3 buffers and a slow handler this alone can be 2 frames (~33 ms @ 60 Hz) of pure age |
| `ContentSize` + `Surface` + 2 COM casts + `GetDesc` | ~5 cross-ABI calls | |
| resize check (2 atomic loads) | negligible | |
| `Arc::new(AtomicBool)` for `InternalCaptureControl` | one heap alloc | per frame, on the capture thread |
| `callback.lock()` | uncontended today | held across everything below |
| **user callback** | dominant | ring: one `CopyResource`; classic GPU: full encode; classic CPU-bridge: `CreateTexture2D` + `CopyResource` + `Map` + full encode |
| `Frame` drop (3 COM Releases) | returns the buffer to the pool | |

**Serialization points** (where one stage waits on another):

1. **The capture thread is the encode thread** on the classic path. WGC cannot deliver frame N+1
   while frame N is being encoded, because the handler runs on the thread that pumps the messages
   that deliver frames. The `5ba1183` texture ring exists specifically to break this — and it only
   applies to the NVENC ring path (`pipeline.rs:1699-1707`); every other backend still serializes.
2. **The handler mutex** (graphics_capture_api.rs:318) serializes `FrameArrived` against `Closed`
   and against any future `CaptureControl::callback()` user.
3. **`Map` is a GPU sync point.** On the CPU-bridge path, `MappedStagingTexture::map` blocks the CPU
   until the preceding `CopyResource` retires. There is no double-buffered staging texture and no
   `D3D11_MAP_FLAG_DO_NOT_WAIT` retry — the capture thread stalls on the GPU once per frame.
4. **`ImageEncoder::encode`** does four blocking `.join()`s on WinRT async operations
   (encoder.rs:164-181). Probe path only.

## 12. Stale references

- `mod.rs:23-33` — "Add this library to your `Cargo.toml`: `windows-capture = "2.0.0"` ... or run
  `cargo add windows-capture`". This is a module inside `screenextend`; the crate is no longer a
  dependency (it was removed from `Cargo.toml` by `ad598b2`).
- `mod.rs:37-156`, `encoder.rs:101-115` and `:672-691`, `dxgi_duplication_api.rs:8-28`,
  `monitor.rs:81-105`, `window.rs:65-74` — ```no_run``` doc examples that `use windows_capture::...`,
  a crate path that does not resolve here. Filed as
  `minor-build-vendored-doctests-reference-the-external-crate.md`.
- `mod.rs:157-160` — `#![warn(clippy::nursery)] / (clippy::cargo) / (clippy::multiple_crate_versions)
  / (missing_docs)`: upstream's crate-root policy, now governing a module of an unrelated crate.
  Filed as `speculative-build-module-level-clippy-cargo-lints-in-vendored-fork.md`.
- `graphics_capture_api.rs:105` — the doc comment on the `frame_pool` field still says "The frame
  pool that provides frames for the capture session" with no mention that the count is now 3 and why.
  The `// Create frame pool` comment that used to sit above it was deleted by `5ba1183` and replaced
  with nothing, so the one line the fork changed for performance reasons carries **zero** explanation.
- `dxgi_duplication_api.rs:228-232` — the `new_options` doc was rewritten by the fork ("lets callers
  prefer any subset of the crate-supported DXGI formats") but the type it documents no longer
  supports the formats the upstream sentence was about; the rewrite is accurate, the surrounding
  module docs still describe upstream's HDR-capable behaviour.
- No references to `PRD`, `AUDIO_NOTES*`, `docs/airplay-fallback-design.md`, `§`, `M1/M2`, or any AI
  tooling anywhere in the module.
- **CLAUDE.md vs code:** CLAUDE.md says `windows_capture/` "is a vendored fork of the
  `windows-capture` crate" and to "keep its `parking_lot` locks and style". Accurate. It does not say
  which version, and the version is now only recoverable from git history — see
  `minor-build-vendored-fork-has-no-provenance-record.md`. CLAUDE.md also states the current version
  is 0.5.4; `src-tauri/Cargo.toml` says **0.6.3** (unrelated to this territory, but noting the
  discrepancy as instructed).

## 13. Dead code candidates

Evidence for each: `grep -rn "windows_capture" src-tauri/src` shows exactly two consumers
(`windows_utils/streamer/capture.rs` and `windows_utils/streamer/pipeline.rs`) and the full list of
symbols they import is `capture::{CaptureControl, Context, GraphicsCaptureApiHandler}`,
`encoder::ImageFormat`, `frame::Frame`, `graphics_capture_api::InternalCaptureControl`,
`monitor::Monitor`, `settings::{ColorFormat, CursorCaptureSettings, DirtyRegionSettings,
DrawBorderSettings, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings}`.

- **`dxgi_duplication_api.rs`** (951 lines) — no importers. ScreenExtend's real duplication backend
  is `windows_utils/streamer/dxgi/`, which is strictly more capable (it reads `AccumulatedFrames`,
  `LastPresentTime`, `GetFrameDirtyRects`, `GetFrameMoveRects` — dxgi/mod.rs:352-417 — none of which
  the vendored wrapper uses even though it stores `DXGI_OUTDUPL_FRAME_INFO` and exposes it via
  `frame_info()`).
- **`graphics_capture_picker.rs`** (171 lines) — no importers; a modal picker dialog is meaningless
  for a headless host. Kept structurally alive only by `GraphicsCaptureItemType::Unknown(_, HwndGuard)`.
- **`window.rs`** (402 lines) — no importers; ScreenExtend only ever captures monitors. Kept alive by
  `GraphicsCaptureItemType::Window` and by `graphics_capture_api::Error::WindowError`.
- **`encoder.rs::VideoEncoder` and its builders/subtype tables** (~1,290 of 1,481 lines) — no
  importers. Only `ImageEncoder`/`ImageFormat`/`ImageEncoderPixelFormat` are live, and only via
  `Frame::save_as_image` on `--probe-capture`.
- **`Frame::dirty_regions`** (frame.rs:116) and **`Frame::timestamp`** (:139) — no callers.
  `timestamp()` is notable: it is the QPC-based capture time WGC stamps on the frame, and the
  pipeline instead uses its own `Instant::now()` taken at handler entry (pipeline.rs:1694). Using
  `SystemRelativeTime` would give a strictly earlier and more accurate capture instant for A/V sync.
- **`Frame::buffer_crop`, `buffer_without_title_bar`** — no callers.
- **`CaptureControl::{is_finished, into_thread_handle, halt_handle, callback, wait}`** — no callers;
  only `stop()` is used (pipeline.rs:242).
- **`Settings::{item, cursor_capture, draw_border, color_format, flags}`** — no callers.
- **`clap`** (`Cargo.toml:141`) — added by the vendoring commit for upstream's `examples/cli.rs`,
  which was not vendored. `grep -rn clap src-tauri/src` returns nothing.
- `[lints.rust] dead_code = "allow"` (Cargo.toml:23-24) is why none of the above warns.

Filed as `minor-build-vendored-fork-carries-2000-lines-of-dead-code.md`.

## 14. Capability probing

**This module is the good example in the codebase.** Every optional WGC feature is selected by
*probing the capability by name*, never by an OS version number:

- `is_supported()` — `IsApiContractPresentByMajor("Windows.Foundation.UniversalApiContract", 8)` **and**
  `GraphicsCaptureSession::IsSupported()` (graphics_capture_api.rs:468-473). Two independent probes.
- `is_cursor_settings_supported` / `is_border_settings_supported` / `is_secondary_windows_supported`
  / `is_minimum_update_interval_supported` / `is_dirty_region_supported` — one
  `ApiInformation::IsPropertyPresent` each, on the exact property name
  (`IsCursorCaptureEnabled`, `IsBorderRequired`, `IncludeSecondaryWindows`, `MinUpdateInterval`,
  `DirtyRegionMode`) (:477-518).
- `create_d3d_device` probes `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` by *attempting the call* and
  falling back — probing by behaviour, the strongest form.
- There is no `GetVersionEx`, no `RtlGetVersion`, no build-number comparison anywhere in the module.

**The defect is not the probe — it is what happens after it.** `GraphicsCaptureApi::new` treats
"probe says no" as a fatal error rather than as "leave this setting at the system default"
(:148-176, and again at :345-413). The tier below the missing capability is missing: there is a
`Supported` tier and an `Error` tier, and no `Degraded` tier. That is what turns a Windows-11-only
knob into a total loss of the WGC path (already filed as
`blocker-wgc-min-update-interval-forces-dxgi.md`; this territory establishes that the behaviour is
inherited verbatim from upstream 2.0.0 and that the fix belongs in the fork, where all five probes
already live).

Second gap: the *reverse* direction is never probed. `DirtyRegionSettings` has a probe, a setter and
a `Frame::dirty_regions()` reader, and the caller opts out with `Default` — a capability that is
present on new hosts and deliberately unused (`speculative-wgc-dirty-regions-captured-but-never-used.md`).

## 15. Questions and cross-cutting concerns

1. **Who owns the frame-pool buffer count?** `5ba1183` raised it to 3 for the NVENC ring, but the
   constant is global to the module and the classic/CPU-bridge path pays the queueing cost without
   the pipelining benefit. Should the count be threaded through `Settings` so the ring path asks for
   3 and everything else asks for 1? (Cross-cuts territory D/E — NVENC ring and pipeline.)
2. **Is the vendored fork in scope for modification?** CLAUDE.md says keep its `parking_lot` locks
   and style; it does not say frozen, and `5ba1183` already modified it. Three of my findings (the
   `Recreate` count, `Frame::buffer_with`, the drain-to-newest loop) are one-file patches inside the
   fork. Confirming the fork is editable unblocks all three.
3. **Should the fork be pruned or re-based?** ~2,400 of 5,900 lines are dead, and the fork contains
   at least six genuine upstream bug fixes (leaked texture maps, the `GetMessageW == -1` infinite
   loop, `has_padding` for 16F, `capacity()` vs `len()`, `EnumChildWindows`, negative title-bar
   height). Those should go upstream; the dead code should go away. Both need a PROVENANCE record
   first.
4. **`SystemRelativeTime` vs `Instant::now()`.** The pipeline timestamps each frame at handler entry
   (pipeline.rs:1694) and the whole A/V sync story rides
   `streamer::audio::host_now_ns()`. WGC already hands us a QPC capture instant via
   `Frame::timestamp()`, which is strictly earlier and excludes the message-pump hop. Is there a
   reason it is unused? If not, plumbing it through would tighten both the reported display lag and
   A/V sync. (Cross-cuts the WebRTC/audio-clock territory.)
5. **Two owners of process DPI awareness.** `dxgi_duplication_api.rs:134` calls
   `SetProcessDpiAwarenessContext` as a side effect of constructing a duplication session, and
   `streamer::prepare()` sets it too. Dead today; a live hazard if the vendored wrapper is ever
   revived. Which layer should own it?
6. **`unsafe impl<T> Send for SendDirectX<T>`** is a blanket escape hatch over any COM type
   (d3d11.rs:49-50). It is sound for the agile objects it is used on today. Worth a note in whatever
   audit tracks `unsafe` in this repo.
7. **Frame size vs encoder size after a resize.** The `FrameArrived` resize branch recreates the pool
   but hands the *already-fetched* frame (old `desc`) to the callback in the same invocation
   (graphics_capture_api.rs:282-311). ScreenExtend's encoders are configured for a fixed geometry.
   I could not determine from this territory whether the pipeline validates `frame.width()/height()`
   against the encoder config before `encode_gpu`. Worth checking from the pipeline side.
8. **Doctests.** If `cargo test` is meant to be part of the local gate (CLAUDE.md says run it
   locally since CI is off), the vendored ```no_run``` examples likely break `cargo test --doc`
   today. One command on a Windows host settles it.

## Findings written

`findings/` gained eleven files from this territory:

- `major-wgc-framepool-recreate-drops-to-one-buffer.md`
- `major-wgc-frame-buffer-allocates-a-staging-texture-per-frame.md`
- `major-wgc-framepool-never-drains-to-the-newest-frame.md`
- `minor-wgc-frame-delivery-goes-through-a-win32-message-pump.md`
- `minor-build-vendored-fork-carries-2000-lines-of-dead-code.md`
- `minor-build-vendored-fork-has-no-provenance-record.md`
- `minor-build-vendored-doctests-reference-the-external-crate.md`
- `minor-dxgi-vendored-wrapper-dropped-hdr-formats-behind-an-unreachable.md`
- `speculative-build-module-level-clippy-cargo-lints-in-vendored-fork.md`
- `speculative-wgc-dirty-regions-captured-but-never-used.md`
- `speculative-wgc-handler-mutex-is-held-for-the-whole-encode.md`

(The Windows-10 WGC hard-fail was already
filed by another territory as `blocker-wgc-min-update-interval-forces-dxgi.md`; I did not duplicate
it, and instead recorded above what this territory adds to it.)

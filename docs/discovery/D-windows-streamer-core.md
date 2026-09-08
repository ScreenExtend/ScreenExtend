# D — Windows streamer core (capture, encode selection, tuning, DXGI duplication)

Territory, read in full: `src-tauri/src/windows_utils/streamer/mod.rs`, `pipeline.rs` (1888 lines),
`capture.rs`, `scaler.rs`, `tuning.rs`, `dxgi/mod.rs`, `dxgi/cursor.rs`. Supporting reads (for
correctness of claims, not part of the territory): `nvidia/encoder.rs`, `intel/encoder.rs`,
`windows_capture/d3d11.rs`, `windows_capture/capture.rs`, `windows_capture/graphics_capture_api.rs`,
`windows_capture/frame.rs`, `streamer/server.rs` (join path), `streamer/webrtc_session.rs`,
`streamer/config.rs`, `streamer/platform.rs`.

---

## 1. Files and purpose

**`mod.rs` (60 lines).** Module root for the Windows encode/capture backends plus a two-function
vendor helper. `Vendor` is a four-way enum (`Nvidia`/`Amd`/`Intel`/`Unknown`), `select_vendor`
classifies a DXGI adapter description string by substring match, and `device_vendor` pulls the
description off an `ID3D11Device` via `IDXGIDevice::GetAdapter().GetDesc()` and logs it. Despite
appearances this is **not** the backend selector — see §15.1.

**`pipeline.rs` (1888 lines).** The centre of the Windows latency path. Owns `Pipeline` (the handle
handed to `webrtc_session`: a `broadcast::Sender<EncodedFrame>`, an IDR flag, a target-bitrate cell,
a wake channel), the two capture entry points (`start` for probes, `start_on_monitor` for real
sessions), both capture drivers (the WGC `LiveCapture` handler and the `dxgi_capture_thread`), the
encoder-backend abstraction (`Backend`, `EncodePath`, `EncodeCore`), the decoupled NVENC texture ring
(`AsyncNvencRing` + `ring_encode_thread`), the idle frame repeater, and the synthetic-pattern probe
loop.

**`capture.rs` (526 lines).** Display plumbing rather than pixel plumbing. DPI-awareness opt-in,
monitor enumeration/selection (by index and by GDI device name), monitor geometry
(`EnumDisplayMonitors` callback), mode setting (`ChangeDisplaySettingsExW` with a 1 s retry loop and
a refresh-rate-drop fallback), orientation setting, and per-monitor DPI scale via the undocumented
`DISPLAYCONFIG_DEVICE_INFO_GET_DPI`/`SET_DPI` info types. Also holds `probe_to_png`, a
developer-harness one-shot WGC capture.

**`scaler.rs` (324 lines).** Two small GPU utilities. `Scaler` wraps a D3D11 `VideoProcessorBlt`
downscale (BGRA→BGRA) with cached input/output views keyed on the raw texture pointer, an optional
staging texture for CPU readback, and a retained readback `Vec`. `TextureReader` is a bare
GPU→CPU readback (`CopyResource` into a staging texture, `Map`, memcpy). Both are `unsafe impl Send`.

**`tuning.rs` (271 lines).** Process- and thread-level scheduling knobs: the high-performance-GPU
registry preference, `timeBeginPeriod(1)`, two `SetProcessInformation(ProcessPowerThrottling)` calls
(execution-speed exemption and Win11 timer-resolution opt-in), `HIGH_PRIORITY_CLASS`, MMCSS
registration with an RAII revert guard (`ThreadTuning`), `SetThreadExecutionState` keep-awake
(`KeepAwake`), the tokio-worker tuner (`tune_transport_thread`), and
`IDXGIDevice::SetGPUThreadPriority(5)`.

**`dxgi/mod.rs` (661 lines).** The custom Desktop Duplication capture. `Duplicator` finds the
`IDXGIOutput1` matching a GDI device name, creates a D3D11 device *on that output's adapter*,
establishes duplication (with a mode-settle loop and retries), and exposes `poll(timeout_ms)` →
`PollStatus::{Dirty, Timeout}` plus `frame()` → composited `ID3D11Texture2D`. Handles rotation
un-rotate, cursor sprite acquisition, cursor compositing, the unchanged-frame skip added in commit
a56eedc, and re-duplication after `DXGI_ERROR_ACCESS_LOST`. Also `probe_to_bmp` and a BMP writer for
the dev harness.

**`dxgi/cursor.rs` (386 lines).** A minimal D3D11 quad renderer (an inline HLSL vertex+pixel shader
compiled at runtime with `D3DCompile`, a dynamic constant buffer holding four vertices, a point
sampler, three blend states) and the DXGI pointer-shape → SRV conversion for all three shape types
(monochrome AND/XOR, masked colour, straight colour). Used both for the cursor sprite and for the
rotation un-rotate blit.

---

## 2. Function catalog

### `mod.rs`
- `select_vendor(&str) -> Vendor` — uppercase substring match on an adapter description
  ("NVIDIA" / "INTEL" / "AMD"|"RADEON"|"ADVANCED MICRO DEVICES"), else `Unknown`. `Vendor::Amd` is
  produced but nothing acts on it.
- `device_vendor(&ID3D11Device) -> Vendor` — casts to `IDXGIDevice`, `GetAdapter().GetDesc()`,
  UTF-16 → String, logs `capture device adapter: name=..., vendor=...`, returns the classification.
  On any failure returns `Vendor::Unknown` **without logging**. Only caller:
  `IntelEncoder::new_on_device`.

### `pipeline.rs`
- `Pipeline::request_idr` — sets the `AtomicBool` and `try_send`s the wake channel (both ineffective
  on the ring path; see findings).
- `Pipeline::set_target_bitrate` — stores into `AtomicU32`, `try_send`s the wake.
- `apply_pending_bitrate(&mut Backend, &AtomicU32, &mut u32)` — swaps the atomic to 0, calls
  `Backend::set_bitrate`, logs. Loses the pending value if the call fails.
- `is_transient_encode_error(&anyhow::Error) -> bool` — **string match** on `format!("{e:#}")`
  containing `"device busy"`.
- `live_encoder_config(native_w, native_h, refresh_hz, &Config) -> EncoderConfig` — fps =
  `cfg.fps` clamped 15..500, else `refresh.clamp(min(60, max_fps), max_fps)`; dims from
  `scaled_dims`; bitrate = `cfg.max_bitrate_kbps * 1000` else `w*h*fps/10` clamped to 6..30 Mbps;
  `max_bitrate_bps == bitrate_bps`.
- `scaled_dims(w, h, ScalePercent) -> (u32, u32)` — native passthrough (rounded to even) or scaled,
  min 2, rounded to even.
- `effective_vendor(&Config)` — `Software` if `disable_gpu_encode`, else `cfg.encoder_vendor`.
- `start(&Config) -> Result<Pipeline>` — probe-harness entry. Synthetic-pattern branch spawns
  `synthetic_pattern_loop`; otherwise checks DWM composition, `select_monitor(cfg.monitor)`, tries
  WGC then DXGI. Leaks the control handle in both arms (`mem::forget` / `drop` of a `Drop`-less type).
- `DxgiControl::stop` — sets the stop flag and joins. No `Drop` impl.
- `SessionCapture::stop` — dispatches to WGC `CaptureControl::stop()` or `DxgiControl::stop()`.
- `start_on_monitor(&Config, device_name) -> Result<SessionCapture>` — the real session entry.
  Checks DWM, `select_monitor_by_device_name`, tries WGC, falls back to DXGI.
- `start_live_capture(...) -> Result<(Pipeline, CaptureControl<LiveCapture, _>)>` — builds
  `EncoderConfig`, the wake channel, the `Pipeline`, and the WGC `Settings`
  (`WithCursor`, `WithoutBorder`, `SecondaryWindowSettings::Default`,
  `MinimumUpdateIntervalSettings::Custom(frame_duration * 2/3)`, `DirtyRegionSettings::Default`,
  `ColorFormat::Bgra8`), then `LiveCapture::start_free_threaded`.
- `start_dxgi_capture(...) -> Result<(Pipeline, DxgiControl)>` — spawns `dxgi-capture` and blocks on
  a `std::sync::mpsc` ready handshake.
- `dxgi_capture_thread(DxgiThreadArgs, ready_tx)` — the DXGI capture+encode loop. Tunes the thread,
  asserts keep-awake, builds `Duplicator` + `Backend` + optional `Scaler` + optional `TextureReader`,
  raises GPU thread priority on both devices, wraps everything in `Arc<Mutex<EncodeCore>>`, spawns the
  repeater, then loops: `poll` → pace against `next_due` → `frame()` → lock → `encode_texture` →
  `tx.send`. Logs an `encode-path latency:` summary every 60 frames.
- `probe_bitrate`, `probe_live`, `is_keyframe` — dev harness (NAL scan for type 5 or 7).
- `EncodePath::name`, `Backend::name/device/set_bitrate/wants_prescale/is_cpu_bridge/encode_gpu/
  encode_cpu/encode_repeat` — the backend dispatch surface. `wants_prescale` is
  `!matches!(self, Backend::Intel)`; `is_cpu_bridge` covers `Nvenc{CpuBridge}`, `IntelCpu`, `X264`.
- `EncodeCore::take_force_idr` — `frame_index == 0 || idr_request.swap(false)`.
- `EncodeCore::encode_captured(&mut Frame)` — WGC classic path: cpu-bridge → (scaler ? scale+readback
  : `frame.buffer()`) → `encode_cpu`; otherwise `encode_gpu`.
- `EncodeCore::encode_texture(&ID3D11Texture2D)` — DXGI path, same shape but uses `TextureReader`
  when there is no scaler.
- `EncodeCore::encode_repeat` — re-encodes the last input (returns `None` before the first frame).
- `build_zero_copy(config, device, context)` — `Encoder::new_shared` + `OpenSharedResource1` +
  `IDXGIKeyedMutex` cast.
- `build_zero_copy_ring(config, device, context, K)` — same for K shared handles. Ignores `context`.
- `AsyncNvencRing::new(...)` — unzips the (texture, mutex) pairs, creates two bounded crossbeam
  channels of depth K, prefills `free_slots`, spawns `nvenc-ring-encode`.
- `AsyncNvencRing::stage(&ID3D11Texture2D) -> Result<bool>` — `try_recv` a free slot (returns
  `Ok(false)` if none), `AcquireSync(KEY_WRITER, 1000)`, scale-into or `CopyResource`, `Flush`,
  `ReleaseSync(KEY_ENCODER)`, compute `force_idr`, `send(ReadyMsg)`, stamp `last_frame_at`.
- `AsyncNvencRing::stop` / `Drop` — set the flag, join.
- `ring_encode_thread(...)` — tunes the thread; loop on `recv_timeout(200ms)`: skip poisoned slots,
  apply pending bitrate, `encode_input_slot`, `tx.send`, `release_and_hold` (returns the *previous*
  slot, keeps the current one); on timeout, `encode_repeat_slot(held)` as a keepalive; on
  `Poisoned`, add to the poison set and bail if unrecoverable.
- `apply_pending_bitrate_atomic` — the `Encoder`-typed twin of `apply_pending_bitrate`.
- `build_backend(config, vendor, native_w, native_h, device, context) -> Result<Backend>` — the
  fallback chain. `try_nvenc` = zero-copy → NVENC CPU bridge; `try_intel` = same-adapter →
  own-device CPU bridge; `try_x264`. `Software` → x264; `Nvidia` → nvenc → x264;
  `Intel` → intel → x264; `Auto` → nvenc → intel → x264.
- `spawn_repeater(core, tx, idr_request, frame_duration, last_frame_at, epoch, stop, wake_rx)` —
  spawns `nvenc-repeat`: waits `tick = max(frame_duration, 8ms)` on the wake channel, drains it,
  skips if a real frame landed within `max(2 * frame_duration, 34ms)`, then emits only if an IDR is
  pending or 200 ms have elapsed since the last repeat.
- `LiveCapture::new(Context<CaptureFlags>)` — tunes the thread, asserts keep-awake, and (for
  `Nvidia`/`Auto`) tries the K=3 ring first; on failure falls back to `build_backend` + `EncodeCore`
  + `spawn_repeater`.
- `LiveCapture::on_frame_arrived(&mut Frame, InternalCaptureControl)` — ring: `stage()` and return.
  Classic: lock the core, `encode_captured`, drop-on-transient-busy up to 60 in a row, `tx.send`,
  60-frame latency summary.
- `LiveCapture::on_closed` — logs `capture item closed (display disconnected?)`, returns `Ok`.
- `LiveCapture::Drop` — sets `stop`, stops the ring.
- `synthetic_pattern_loop`, `fill_synthetic_pattern` — dev harness (a 1280x720 gradient with a moving
  box and bar, CPU-filled per frame).

### `capture.rs`
- `set_dpi_awareness()` — `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`; logs on failure.
- `check_dwm_composition()` — `DwmIsCompositionEnabled`; bails only when explicitly disabled; an API
  error is logged and treated as OK.
- `select_monitor(u32)` — enumerates, logs every display, falls back to index 0 with a log.
- `monitor_device_names()`, `monitor_dimensions(name)`, `monitor_rect(name)` — enumeration helpers;
  `monitor_rect` uses an `EnumDisplayMonitors` callback with a raw `*mut Ctx` in the `LPARAM`.
- `set_display_resolution` → `set_display_mode(name, w, h, refresh, portrait)` — seeds a `DEVMODEW`
  from `ENUM_CURRENT_SETTINGS`, sets pels+frequency+orientation, retries for 1 s at 50 ms, then
  retries once without `DM_DISPLAYFREQUENCY` and reports partial success as `Ok`.
- `set_display_orientation(name, portrait)` — no-op when already correct; swaps pels and applies.
- `source_path_for_device(name) -> (LUID, u32)` — `QueryDisplayConfig` walk matching the GDI name.
- `ensure_dpi_at(name, target_idx) -> Result<bool>` — undocumented GET_DPI/SET_DPI; returns `true`
  when already at target.
- `set_display_scale(name, percent)` — snaps to `DPI_PERCENT_VALUES`, loops for 2 s requiring five
  consecutive confirmations; returns `Ok` on most timeout paths.
- `select_monitor_by_device_name(name)` — linear scan.
- `ProbeHandler` (`new`, `on_frame_arrived`), `probe_to_png` — dev harness.

### `scaler.rs`
- `Scaler::new(device, context, src_w, src_h, dst_w, dst_h)` — casts to video device/context, creates
  the enumerator, processor, destination texture (`RENDER_TARGET | SHADER_RESOURCE`) and output view.
- `Scaler::scale(&src) -> &dst` / `scale_into(&src, &dst)` — blt to the owned or an external target.
- `Scaler::input_view` / `output_view_for` — one-entry caches keyed on `as_raw()` pointer identity.
- `Scaler::run_blt` — builds a `D3D11_VIDEO_PROCESSOR_STREAM` with a `ManuallyDrop` input surface,
  calls `VideoProcessorBlt`, then manually drops the surface. No filter is configured; the driver's
  default is used.
- `Scaler::read_back() -> (&[u8], row_pitch)` — lazily creates the staging texture, `CopyResource`,
  `Map(READ)`, memcpy into a retained `Vec`, `Unmap`.
- `TextureReader::new` / `read_back(&src)` — the same readback without the scale.

### `tuning.rs`
- `apply_process_tuning()` — calls the five process-level helpers in order.
- `prefer_high_performance_gpu()` — writes `GpuPreference=2;` under
  `HKCU\...\DirectX\UserGpuPreferences`, keyed on the exe path; skips if already set.
- `raise_timer_resolution()` — `timeBeginPeriod(1)`, checked and logged.
- `exempt_from_power_throttling()` / `honor_timer_resolution_in_background()` —
  `SetProcessInformation(ProcessPowerThrottling)` with `EXECUTION_SPEED` and
  `IGNORE_TIMER_RESOLUTION`; both checked and logged (the second logs failure at info level, since
  it is Win11-only).
- `raise_process_priority()` — `SetPriorityClass(HIGH_PRIORITY_CLASS)`, checked.
- `tune_current_thread() -> ThreadTuning` — `register_mmcss()` + `raise_current_thread_priority()`.
- `ThreadTuning::Drop` — `AvRevertMmThreadCharacteristics` (from whatever thread does the drop).
- `KeepAwake::begin` / `Drop` — `SetThreadExecutionState(CONTINUOUS|SYSTEM|DISPLAY)` and restore.
- `tune_transport_thread()` — MMCSS "Playback" + `THREAD_PRIORITY_ABOVE_NORMAL`; **both results
  discarded, handle leaked**.
- `register_mmcss() -> isize` — tries "Pro Audio", "Games", "Playback" in order; on success raises to
  `AVRT_PRIORITY_CRITICAL`; returns the handle as `isize` or 0.
- `raise_current_thread_priority()` — `THREAD_PRIORITY_TIME_CRITICAL` +
  `SetThreadPriorityBoost(t, true)` (i.e. boost **disabled**).
- `raise_d3d11_gpu_priority(&ID3D11Device)` — `IDXGIDevice::SetGPUThreadPriority(5)`, checked and
  logged; silently returns if the cast fails.

### `dxgi/mod.rs`
- `wide_to_string(&[u16])`, `find_output(name) -> (IDXGIAdapter1, IDXGIOutput1, DXGI_OUTPUT_DESC)` —
  nested adapter/output enumeration; logs the adapter the output was found on.
- `create_device_on(&IDXGIAdapter1)` — tries `BGRA|VIDEO`, retries `BGRA` only on failure; sets
  `ID3D11Multithread::SetMultithreadProtected(true)` (result discarded).
- `duplicate_with_retry(output, device, attempts)` — `DuplicateOutput` with 100 ms backoff.
- `make_target(device, w, h)` — texture + SRV + RTV.
- `blit_verts(rotation) -> QuadVerts` — UV permutation for the four rotations.
- `Duplicator::new(device_name, logical_w, logical_h)` — find output, create device, loop until the
  duplication mode matches the expected logical size (3 s deadline), allocate `desktop` (physical
  size) and `composite` (logical size), build the `QuadRenderer`.
- `Duplicator::device` / `context` — accessors used by the pipeline to build the backend/scaler.
- `Duplicator::redup()` — re-establish duplication after access loss; bails if the mode changed or
  after `REDUP_MAX_RETRIES = 40`.
- `Duplicator::present_region_is_empty(dup, info) -> bool` — the a56eedc skip logic (see findings).
- `Duplicator::poll(timeout_ms) -> Result<PollStatus>` — `AcquireNextFrame`; maps
  `WAIT_TIMEOUT` → `Timeout`, `ACCESS_LOST` → drop the duplication and `Timeout`; on a real present
  (and not skipped) `CopyResource` into `desktop`; fetches a new pointer shape if
  `PointerShapeBufferSize > 0`; tracks cursor visibility/position; `ReleaseFrame` (result discarded).
- `Duplicator::refresh_cursor_state()` — `GetCursorInfo`, bounds-check against the output rect,
  rewrite `cursor_pos` as (screen − origin − hotspot).
- `Duplicator::frame() -> Result<&ID3D11Texture2D>` — fast path returns `&desktop` when rotation is
  identity and the cursor is off; otherwise composites (full copy or un-rotate blit) and draws the
  cursor passes.
- `Duplicator::cursor_drawn()` — probe-only accessor.
- `probe_to_bmp`, `write_bmp` — dev harness.

### `dxgi/cursor.rs`
- `quad_verts(x, y, w, h, target_w, target_h) -> QuadVerts` — pixel rect → NDC quad with UVs.
- `compile(entry, target) -> Vec<u8>` — runtime `D3DCompile` of the inline HLSL, with error-blob text.
- `make_blend(device, src, dst)` — a blend state with `SrcBlendAlpha = ZERO`, `DestBlendAlpha = ONE`.
- `QuadRenderer::new` — compiles `vs_4_0`/`ps_4_0`, creates the dynamic cbuffer, a **point** sampler,
  and the alpha/multiply/xor blend states.
- `QuadRenderer::draw(rtv, srv, blend, verts, w, h)` — maps the cbuffer with `WRITE_DISCARD`, memcpys
  the four vertices, sets viewport/RTV/blend/topology/shaders/SRV/sampler, `Draw(4, 0)`, then unbinds
  the SRV and the RTV.
- `make_srv(device, w, h, &[u32])` — immutable BGRA texture + SRV from a CPU pixel buffer.
- `build_sprite(device, &DXGI_OUTDUPL_POINTER_SHAPE_INFO, data) -> CursorSprite` — three cases:
  monochrome (two passes, AND with `Multiply` then XOR with `Xor`), masked colour (base `Alpha` +
  `Xor`), colour (single `Alpha` pass). Bails on an unknown type.

---

## 3. Threads

Per **session** (one virtual display = one client):

| Thread | Spawned at | Priority | Lifetime | Owns |
|---|---|---|---|---|
| WGC dispatcher/capture thread | `windows_capture/capture.rs:401` (`start_free_threaded`) | MMCSS ("Pro Audio"→"Games"→"Playback") at `AVRT_PRIORITY_CRITICAL` + `THREAD_PRIORITY_TIME_CRITICAL`, boost disabled — set inside `LiveCapture::new` (pipeline.rs:1544) | until `CaptureControl::stop()` or `WM_QUIT` | the WinRT apartment, the `DispatcherQueueController`, the D3D11 device+context, the frame pool (3 buffers), and — on the classic path — the inline encode |
| `nvenc-ring-encode` | pipeline.rs:1114-1129 | same tuning (pipeline.rs:1228) | until `AsyncNvencRing::stop`/`Drop` | the NVENC `Encoder`, the held slot, `poisoned_slots`, the bitrate state |
| `nvenc-repeat` | pipeline.rs:1473-1525 | same tuning (pipeline.rs:1476) | until `stop` is set | nothing exclusively; borrows `Arc<Mutex<EncodeCore>>` |
| `dxgi-capture` | pipeline.rs:407-410 | same tuning (pipeline.rs:460) | until `stop` is set or a fatal poll/encode error | the `Duplicator`, `KeepAwake`, and the `Arc<Mutex<EncodeCore>>` |

Notes:
- The ring path spawns **no** repeater (its keepalive lives in `ring_encode_thread`'s 200 ms
  `recv_timeout` branch). The classic WGC path and the DXGI path each spawn one.
- `KeepAwake::begin()` is asserted on the WGC capture thread (pipeline.rs:1545) and on the DXGI thread
  (pipeline.rs:461). `SetThreadExecutionState` is per-thread, so the assertion lives exactly as long as
  that thread — which is the intent, but it means the ring encode and repeater threads do not assert it.
- `nvenc-encode` (pipeline.rs:167-170) is the synthetic-pattern probe thread; never spawned in a real
  session.
- Every tokio worker in the streamer runtime additionally runs `tune_transport_thread` at
  `on_thread_start` (`streamer/mod.rs:41, 50, 64`).

---

## 4. Locks

| Lock | Guards | Takers | Hot path? | Across `.await`/callback? |
|---|---|---|---|---|
| `Arc<Mutex<EncodeCore>>` (std) | backend, scaler, reader, bitrate state, `have_frame`, `frame_index` | WGC classic `on_frame_arrived` (pipeline.rs:1716), `dxgi_capture_thread` (pipeline.rs:599), `spawn_repeater` (pipeline.rs:1504) | **yes** — held across the entire encode, including NVENC/oneVPL/x264 FFI and any GPU readback | never across `.await` (no async here), but **held across an FFI callback boundary** on the WGC path: the guard lives inside the WinRT `TypedEventHandler`. A panic there unwinds through `extern "system"`. |
| `parking_lot::Mutex<LiveCapture>` (vendored) | the `GraphicsCaptureApiHandler` | the WGC FrameArrived handler (`graphics_capture_api.rs:318-320`) and the Closed handler (:220) | yes, once per frame | callback only |
| `Arc<Mutex<Option<ProbeResult>>>` | probe output | `ProbeHandler::on_frame_arrived` (capture.rs:483), `probe_to_png` (capture.rs:518) | probe only | `.unwrap()` on both |
| `idr_request: Arc<AtomicBool>` | pending-IDR flag | `Pipeline::request_idr` (any tokio task), `EncodeCore::take_force_idr`, `AsyncNvencRing::stage`, `spawn_repeater` | yes | atomics, `Relaxed` throughout |
| `target_bitrate: Arc<AtomicU32>` | pending bitrate | `Pipeline::set_target_bitrate`, both `apply_pending_bitrate*` | yes | `Relaxed`, swap-to-0 sentinel |
| `last_frame_at: Arc<AtomicU64>` | ms since epoch of the last real frame | writers: `dxgi_capture_thread`, `AsyncNvencRing::stage`, `LiveCapture::on_frame_arrived`; reader: `spawn_repeater` | yes | `Relaxed` |
| `stop: Arc<AtomicBool>` | shutdown | all four thread kinds | per-iteration | `Relaxed` |
| `IDXGIKeyedMutex` (per ring slot) | the shared NVENC input texture | capture side `AcquireSync(KEY_WRITER, 1000)`, encode side `AcquireSync(KEY_ENCODER, 1000)` | **yes, per frame** | a 1000 ms timeout on the capture thread — see `minor-keyed-mutex-one-second-timeout-stalls-the-capture-thread.md` |
| `ID3D11Multithread::SetMultithreadProtected(true)` | the DXGI duplication device | driver-internal | implicit per D3D call | result discarded (dxgi/mod.rs:146) |

The territory contains **no `.await`**, so the "never hold a `MutexGuard` across `.await`" rule is
vacuously satisfied here. The `if let` scrutinee temporary-lifetime pattern does appear but always in
sync code: `Scaler::input_view` / `output_view_for` (`if let Some((cached_key, view)) =
&self.cached_in_view`, scaler.rs:141, 168) and `spawn_repeater`'s `if let Some(slot) = held_slot`
(pipeline.rs:1299) — none of these enclose an await point today, but the ring/repeater is the most
likely place for a future async refactor to introduce one.

---

## 5. Buffers and copies

Per frame, WGC + NVENC **ring** path (the "fast" path):
1. WGC frame pool texture (3 buffers) — **GPU-resident**, borrowed as `frame.as_raw_texture()`.
2. `AsyncNvencRing::stage` → `scale_into(raw, dst)` (**GPU→GPU** `VideoProcessorBlt`) *or*
   `igpu_context.CopyResource(dst, raw)` (**GPU→GPU full-frame copy**) into the keyed-mutex shared
   texture. So "zero-copy" means zero *CPU* copies; there is always one full-frame GPU copy.
3. `igpu_context.Flush()` — forces a GPU submit every frame (needed for the cross-adapter share).
4. NVENC reads the shared texture directly (`nvEncMapInputResource` on a registered DX resource) —
   **no copy**.
5. `nvEncLockBitstream` → `slice::from_raw_parts(...).to_vec()` — **one heap allocation + memcpy per
   frame** (nvidia/encoder.rs:789-795). See
   `minor-per-frame-allocation-of-the-encoded-access-unit.md`.
6. `Bytes::from(au)` — zero-copy ownership transfer.
7. `broadcast::Sender::send` — clones `Bytes` per receiver (refcount only).

WGC classic path, cpu-bridge backend (NVENC CPU bridge / IntelCpu / x264):
- with a scaler: GPU→GPU blt, then `Scaler::read_back` = `CopyResource` to staging + `Map` +
  **GPU→CPU readback memcpy into a retained `Vec`** (no per-frame alloc after warm-up), then the
  encoder re-uploads.
- without a scaler: `frame.buffer()` maps the vendored `windows_capture` staging texture — same
  GPU→CPU cost.

DXGI path, every frame:
- `poll`: `CopyResource(desktop, dup_tex)` — **one full-frame GPU→GPU copy, unconditional**.
- `frame()`: if the cursor is visible or the display is rotated, a **second** full-frame operation
  (`CopyResource(composite, desktop)` or an un-rotate blit) plus 1-2 cursor quad draws.
- then the same encoder-input path as above (a third copy into the NVENC shared texture, or a
  readback).

Retained/reused buffers (no per-frame allocation): `Scaler::readback`, `TextureReader::readback`
(both grown with `resize`), `Duplicator::shape_buf`, `Duplicator::meta_buf`,
`Encoder::bitstream_buf` (Intel), `synthetic_pattern_loop::frame_buf`.

Per-event (not per-frame) allocations: `build_sprite` allocates one or two `Vec<u32>` of
`w*h` on each cursor **shape change** (dxgi/cursor.rs:294, 341, 372), plus one or two immutable
textures + SRVs, and the old `CursorSprite` is dropped. A rapidly animating cursor (a spinner) makes
this per-frame in practice.

---

## 6. Queues

| Queue | Type / depth | Blocking | Back-pressure |
|---|---|---|---|
| `Pipeline::tx` | `tokio::sync::broadcast<EncodedFrame>`, capacity **4** (`BROADCAST_CAPACITY`, pipeline.rs:43) | `send` never blocks | oldest frame evicted; slow receivers get `RecvError::Lagged`, which the viewer task answers with a **forced IDR** (webrtc_session.rs:550-553). See `minor-broadcast-capacity-four-forces-idr-storms.md`. |
| `wake` | `crossbeam_channel::bounded::<()>(1)` | `try_send` (dropped when full) | the repeater drains it with `while wake_rx.try_recv().is_ok() {}` (pipeline.rs:1488). **Unused on the ring path** — `wake_rx` is dropped, so `request_idr`'s poke goes nowhere. |
| `free_slots` | `crossbeam_channel::bounded::<usize>(K=3)` | capture side uses `try_recv` (non-blocking) | when empty the frame is **silently dropped** (`Ok(false)`, pipeline.rs:1151). One slot is permanently held back by `release_and_hold`, so effective depth is 2. |
| `ready` | `crossbeam_channel::bounded::<ReadyMsg>(K=3)` | `send` (would block if full, but cannot be, since a slot must be free to produce a message) | bounded by `free_slots` |
| DXGI setup handshake | `std::sync::mpsc::channel::<Result<()>>` | `ready_rx.recv()` blocks the caller | one-shot |
| WGC start handshake | two `std::sync::mpsc` channels | `halt_receiver.recv()` + `callback_receiver.recv()` block the caller | one-shot; this is what makes `start_on_monitor` blocking |

---

## 7. Syscall / FFI boundaries

**D3D11 / DXGI (`windows` crate, `extern "system"`):** `D3D11CreateDevice` (dxgi/mod.rs:118,
windows_capture/d3d11.rs:203, nvidia/encoder.rs:885), `CreateDXGIFactory1`, `EnumAdapters1`,
`EnumOutputs`, `GetDesc`/`GetDesc1`, `DuplicateOutput`, `AcquireNextFrame`, `GetFrameMoveRects`,
`GetFrameDirtyRects`, `GetFramePointerShape`, `ReleaseFrame`, `CreateTexture2D`,
`CreateShaderResourceView`, `CreateRenderTargetView`, `CreateBlendState`, `CreateSamplerState`,
`CreateBuffer`, `CopyResource`, `Map`/`Unmap`, `Flush`, `OpenSharedResource1`,
`IDXGIKeyedMutex::AcquireSync`/`ReleaseSync`, `IDXGIDevice::SetGPUThreadPriority`,
`ID3D11Multithread::SetMultithreadProtected`, the full `ID3D11VideoDevice`/`ID3D11VideoContext`
surface (`CreateVideoProcessorEnumerator`, `CreateVideoProcessor`, `CreateVideoProcessor*View`,
`VideoProcessorBlt`).

**Shader compilation:** `D3DCompile` from `D3DCompiler_47.dll` at runtime (dxgi/cursor.rs:77) — an
implicit dependency on the D3D compiler redistributable being present. It ships with Windows, so this
is safe, but it is worth knowing about.

**GDI / display config:** `EnumDisplayMonitors` + a raw `unsafe extern "system"` callback with a
`*mut Ctx` smuggled through `LPARAM` (capture.rs:115-151), `GetMonitorInfoW` with a
`MONITORINFOEXW` whose `cbSize` is set by hand, `EnumDisplaySettingsW`, `ChangeDisplaySettingsExW`,
`GetDisplayConfigBufferSizes`, `QueryDisplayConfig`, `DisplayConfigGetDeviceInfo`,
`DisplayConfigSetDeviceInfo`.

**Undocumented / hand-rolled versioning structs:** `DisplayConfigGetDpi` and `DisplayConfigSetDpi`
(capture.rs:280-295) are `#[repr(C)]` structs written by hand against the undocumented
`DISPLAYCONFIG_DEVICE_INFO_TYPE` values **-3 (GET_DPI)** and **-4 (SET_DPI)**. `header.size` is set
from `size_of::<Self>()`, and `DisplayConfigSetDeviceInfo` is called through a
`.cast::<DISPLAYCONFIG_DEVICE_INFO_HEADER>()` pointer. If Microsoft changes the layout this fails at
runtime, and `set_display_scale` swallows the failure (see
`minor-set-display-scale-reports-success-after-failing.md`).

**Scheduling / power:** `AvSetMmThreadCharacteristicsW`, `AvSetMmThreadPriority`,
`AvRevertMmThreadCharacteristics`, `SetThreadPriority`, `SetThreadPriorityBoost`, `SetPriorityClass`,
`SetProcessInformation(ProcessPowerThrottling)` with `PROCESS_POWER_THROTTLING_CURRENT_VERSION`
(a real versioning struct, correctly filled), `timeBeginPeriod`, `SetThreadExecutionState`,
`SetProcessDpiAwarenessContext`.

**Registry:** `winreg` `HKEY_CURRENT_USER\Software\Microsoft\DirectX\UserGpuPreferences`.

**WinRT (via the vendored `windows_capture`):** `Direct3D11CaptureFramePool::Create`,
`CreateCaptureSession`, `TypedEventHandler` callbacks, `ApiInformation::IsApiContractPresentByMajor`
and `IsPropertyPresent` (a genuine capability probe, see §14),
`CreateDispatcherQueueController`, `CreateDirect3D11DeviceFromDXGIDevice`,
`IDirect3DDxgiInterfaceAccess::GetInterface`.

**NVENC / oneVPL / libx264** are reached through `Backend` but live outside this territory.

macOS/dyld exposure: none — this whole territory is `#[cfg(windows)]`.

---

## 8. Panics

Reachable at runtime:
- `pipeline.rs:1105` `free_tx.send(slot).expect("prefill free_slots")` — cannot fail as written
  (capacity == K), but it is an `expect` inside a constructor.
- `pipeline.rs:1128` `.expect("spawn nvenc ring encode thread")`, `pipeline.rs:1525`
  `.expect("spawn repeater thread")`, `pipeline.rs:170` `.expect("spawn encode thread")` — thread
  spawn failure under resource exhaustion aborts the process instead of failing the session.
- `pipeline.rs:1714` `.expect("classic path must have an EncodeCore")` — an invariant, but asserted
  inside a WinRT callback.
- **`pipeline.rs:1716` `core.lock().expect("encode core mutex poisoned")`** — the one that matters:
  a panic anywhere in `EncodeCore::encode_repeat` on the repeater thread poisons the mutex and then
  panics the WGC callback, unwinding through `extern "system"`. See
  `minor-encode-core-mutex-expect-panics-the-capture-thread.md`.
- `scaler.rs:241` `self.staging.as_ref().unwrap()` — guarded by the assignment immediately above.
- `dxgi/mod.rs:548` `self.sprite.as_ref().unwrap()` — guarded by `cursor_on`.
- `capture.rs:483, 518` `.lock().unwrap()` — probe only.
- `dxgi/mod.rs:658` `data[start..start + row]` in `write_bmp` — probe only; would panic if the
  staging pitch were smaller than `w*4`.
- Slice indexing in `fill_synthetic_pattern` (`buf[o+3]`) — probe only, sized by construction.
- `nvidia/encoder.rs` uses `.expect("<fn> present")` on every NVENC function pointer
  (e.g. :774, :785, :801). If a driver ships an older function table, that is a panic on the encode
  thread rather than a graceful fallback.

`is_multiple_of` (pipeline.rs:636, 1748) requires Rust >= 1.87.

---

## 9. Silent fallbacks and swallowed errors

**Silent degradations (no log at all):**
- `AsyncNvencRing::stage` returning `Ok(false)` — a dropped frame with no counter and no log
  (pipeline.rs:1151, consumed as `Ok(_staged) => {}` at :1703).
- `Encoder::set_bitrate` returning `Ok(())` when `qp.is_some()` (nvidia/encoder.rs:461-463) — the
  caller then logs a bitrate change that did not happen.
- `device_vendor` returning `Vendor::Unknown` on any COM failure (mod.rs:58) with no log.
- `raise_d3d11_gpu_priority` returning early when the `IDXGIDevice` cast fails (tuning.rs:263-266).
- `tune_transport_thread` discarding both `AvSetMmThreadCharacteristicsW` and `SetThreadPriority`
  results (tuning.rs:206-209).
- `let _ = SetThreadPriorityBoost(t, true)` (tuning.rs:254).
- `let _ = unsafe { mt.SetMultithreadProtected(true) }` (dxgi/mod.rs:146).
- `let _ = unsafe { dup.ReleaseFrame() }` (dxgi/mod.rs:478).
- `let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) }` (nvidia/encoder.rs:320, 348) — a failed release
  wedges a ring slot permanently.
- `let _ = EnumDisplaySettingsW(...)` in `set_display_mode` (capture.rs:189) — a failed seed leaves a
  zeroed `DEVMODEW`.
- `let _ = EnumDisplayMonitors(...)` (capture.rs:150).
- `let _ = tx.send(EncodedFrame { .. })` at pipeline.rs:623, 1266, 1311, 1517, 1738 — correct
  (a broadcast with no receivers), but it also hides "every viewer disconnected".
- `set_display_scale` returning `Ok(())` on most timeout paths (capture.rs:428-433).
- `check_dwm_composition` treating an API error as success (capture.rs:40-43).

**Logged degradations (the perf cliff is announced but not surfaced to the user):**
- WGC → DXGI fallback (pipeline.rs:198-202, 274-277).
- NVENC zero-copy → NVENC CPU bridge (pipeline.rs:1366-1368).
- NVENC ring → classic backend (pipeline.rs:1612-1614).
- NVENC → Intel → x264 (pipeline.rs:1435, 1440, 1445, 1450).
- Intel same-adapter → Intel CPU bridge (pipeline.rs:1400-1402).
- `create_device_on` dropping `VIDEO_SUPPORT` (dxgi/mod.rs:141).
- `set_display_mode` dropping the requested refresh rate (capture.rs:222-226).
- `redup` retrying up to 40 times, one `tprintln!` only on success (dxgi/mod.rs:332).

None of these reach the UI. `CompatibilityReport` carries `display_backend` and `audio_backend`
tiers on macOS but nothing equivalent for the Windows capture/encode tier.

---

## 10. TODO / FIXME / HACK / XXX

None. `grep -n "TODO\|FIXME\|HACK\|XXX"` over the whole territory returns nothing.

---

## 11. Latency path

Every file in this territory is on the glass-to-glass path. Estimated per-stage cost at 1080p60
(estimates, not measured — nothing in this territory is instrumented finely enough to attribute
within a stage):

| Stage | Where | Est. cost | Serialization |
|---|---|---|---|
| Present → capture delivery | WGC frame pool / `AcquireNextFrame` | up to 1 frame period; unmeasured because the source timestamp is discarded (see `major-capture-timestamp-is-callback-arrival-not-frame-time.md`) | WGC: throttled by `MinUpdateInterval = frame_duration * 2/3`. DXGI: `poll(timeout_ms)` where the timeout is `next_due − now` when dirty, else a flat **8 ms** (pipeline.rs:559-564) — so an idle→active transition costs up to 8 ms of extra wait. |
| Duplication copy | `CopyResource(desktop, tex)` dxgi/mod.rs:426 | ~0.5-1 ms at 1080p, scales with resolution | DXGI only; unconditional |
| Composite + cursor | `Duplicator::frame()` dxgi/mod.rs:534-567 | a second full-frame copy (~0.5-1 ms) + 1-2 quad draws whenever the cursor is on screen | DXGI only |
| Downscale | `Scaler::scale`/`scale_into` | GPU blt, sub-ms | only when `videoScale < 100` and `wants_prescale` |
| Stage into encoder input | `CopyResource` + `Flush` into the keyed-mutex texture (pipeline.rs:850, 1168-1172) | one full-frame GPU copy + a forced submit | **the capture thread waits on `AcquireSync(KEY_WRITER, 1000ms)` here** |
| Encode | `nvEncEncodePicture` + `nvEncLockBitstream` | 2-6 ms NVENC P1 ULTRA_LOW_LATENCY; tens of ms for x264 | **synchronous**, one picture in flight (`enableEncodeAsync = 0`, single bitstream buffer). On the classic and DXGI paths this runs **on the capture thread**, so capture N+1 waits for encode N. |
| Bitstream copy | `.to_vec()` nvidia/encoder.rs:789 | one alloc + memcpy, ~25 KB typical | on the encode thread |
| Fan-out | `broadcast` depth 4 | refcount only | overrun → `Lagged` → forced IDR |

**Serialization points, explicitly:**
1. Ring path: capture thread and encode thread are decoupled by `free_slots`/`ready` — capture of
   N+1 *does* overlap encode of N, up to 2 slots deep. But the encoder itself is serial.
2. Classic WGC path and DXGI path: **no overlap at all** — the encode happens inline under the
   `EncodeCore` mutex on the capture thread.
3. `EncodeCore` mutex: the repeater can hold it while the capture thread wants it, and vice versa.
4. Keyed mutex: a 1 s timeout on the capture thread.
5. `start_on_monitor` blocks a tokio worker for the whole capture bring-up
   (see `major-start-capture-blocks-a-tokio-worker.md`).
6. Frame pacing: the DXGI loop enforces `next_due = capture + frame_duration` (pipeline.rs:629), so a
   frame that arrives just after `next_due` waits a full period. The `was_idle` handling
   (pipeline.rs:567-572) resets `next_due` on the idle→active edge, which is the right instinct, but
   the 8 ms idle poll timeout still applies first.

**Input path:** none of these files touch remote input. **Audio path:** none, except that
`EncodedFrame.capture` is the video half of the shared A/V clock.

---

## 12. Stale references

- **`pipeline.rs:335`** — the log line reads `"pipeline: starting live monitor capture (WGC -> NVENC;
  zero-copy if available)"`, printed unconditionally by `start_live_capture` before the backend is
  chosen. If the host has no NVIDIA GPU it still says NVENC, and the subsequent
  `pipeline: live capture ready -- SOFTWARE x264 CPU path` contradicts it.
- **`pipeline.rs:1040`** — `let _ = igpu_context; // context is passed for symmetry with
  build_zero_copy / used by caller`. The comment describes an intent, not the code; the parameter is
  genuinely unused.
- **`pipeline.rs:828-831`** — `debug_assert!(scaler.is_none(), "Intel fuses scaling in VPP; no
  prescaler expected")` is correct today only because `wants_prescale()` returns `false` for
  `Backend::Intel`; the invariant is enforced 20 lines away in a different function.
- **Naming, throughout the NVENC paths** — `igpu_device`, `igpu_context`, `shared_igpu`, `igpu_mutex`
  (pipeline.rs:753-756, 1011-1012, 1070-1072) all encode an assumption that the capture device is an
  integrated GPU. Nothing enforces that; see
  `speculative-wgc-device-lands-on-the-wrong-adapter.md`.
- **`amd.md`** sits next to a stub `amd.rs` that nothing constructs.
- No references to `PRD`, `AUDIO_NOTES*`, `docs/airplay-fallback-design.md`, `§`, `M1`/`M2`, or to AI
  tooling anywhere in this territory.
- `CLAUDE.md` says "Current version: **0.5.4**" while `git log` shows
  `build(release): bump version to 0.6.3` — a doc/code disagreement outside this territory but worth
  recording.

---

## 13. Dead code candidates

| Item | Evidence |
|---|---|
| `nvidia::Encoder::request_idr` (nvidia/encoder.rs:493) | `grep -rn "\.request_idr()"` across `src-tauri/src` matches only `Pipeline::request_idr` call sites. Consequently `self.idr_requested` is always `false` and every read of it is a constant. **This is not benign** — it is the mechanism by which the ring keepalive would emit an IDR. |
| `intel::Encoder::request_idr` (intel/encoder.rs:501) | same |
| `Pipeline::frame_duration` (pipeline.rs:48) | `pub`, assigned at pipeline.rs:301 and :365, never read anywhere (`grep -rn "\.frame_duration"` finds no reads). |
| `AmfBackend` (amd.rs:5) | never constructed; `Backend` has no AMD variant. |
| `select_vendor`'s `Vendor::Amd` arm (mod.rs:29) | produced but never matched on. |
| `build_zero_copy_ring`'s `igpu_context` parameter (pipeline.rs:1038) | explicitly discarded at :1040. |
| `Duplicator::cursor_drawn` (dxgi/mod.rs:572) | only `probe_to_bmp`. |
| `Duplicator::raw_rotation` | used only in `redup`'s mode-change check (dxgi/mod.rs:321) — live, listed for completeness. |
| `probe_to_png`, `probe_to_bmp`, `probe_live`, `probe_bitrate`, `synthetic_pattern_loop`, `fill_synthetic_pattern`, `start()` | all reachable only from the standalone developer probe harness (`streamer/cli.rs` + `Config::from_args`), which `main.rs` does not wire up. Not dead in the strict sense, but not shipped-facing either. |
| `MonitorInfo.gpu` | read only in log lines. |

---

## 14. Capability probing

**Done correctly:**
- The vendored WGC layer probes by *property presence*, not by build number:
  `ApiInformation::IsApiContractPresentByMajor("Windows.Foundation.UniversalApiContract", 8)` plus
  `IsPropertyPresent("...GraphicsCaptureSession", "IsCursorCaptureEnabled" | "IsBorderRequired" |
  "IncludeSecondaryWindows" | "MinUpdateInterval" | "DirtyRegionMode")`
  (graphics_capture_api.rs:466-518). This is exactly the tier doctrine.
- `honor_timer_resolution_in_background` probes by *calling* and treating failure as "pre-Win11"
  (tuning.rs:140-143) rather than checking a version.
- `create_device_on` probes `VIDEO_SUPPORT` by trying and retrying without it (dxgi/mod.rs:137-143).
- `IntelEncoder::new_on_device` probes the capture device's `GetCreationFlags()` for
  `VIDEO_SUPPORT` and builds a bridge if absent (intel/encoder.rs:210-225).
- `select_vendor`/`device_vendor` probe by adapter description string, which is stringly-typed but is
  the only identification DXGI offers short of PCI vendor IDs.

**Defects:**
1. **The probe result is used to fail, not to downgrade.** `GraphicsCaptureApi::new` returns
   `Error::BorderConfigUnsupported` / `MinimumUpdateIntervalUnsupported` when a requested optional
   setting is absent (graphics_capture_api.rs:148-176). Since `start_live_capture` requests
   `WithoutBorder` and a custom `MinUpdateInterval` unconditionally, the whole WGC tier is unavailable
   on Windows 10 and pre-24H2 Windows 11. This is the single largest compatibility finding — see
   `blocker-wgc-min-update-interval-forces-dxgi.md`.
2. **A missing tier: AMD.** The backend ladder is NVENC → Intel → x264. There is no AMF tier, so
   every AMD host falls to software. See `major-amd-hosts-get-no-hardware-encoder.md`.
3. **A missing tier report.** There is no `CompatibilityReport` field telling the UI which capture
   path and which encoder tier were selected, so a user on the slow path has no way to know.
4. **`select_vendor` uses PCI vendor *strings*.** `desc.VendorId` is available in
   `DXGI_ADAPTER_DESC1` and is unambiguous (0x10DE NVIDIA, 0x8086 Intel, 0x1002/0x1022 AMD). The
   string match is fragile for localized or unusual descriptions and will classify the IDD adapter
   as `Unknown`.
5. **No probe of the duplication adapter's video capability before building the `Scaler`** —
   the `ID3D11VideoDevice` cast is the probe, but its failure is fatal to the session rather than a
   downgrade. See `major-scaler-requires-video-device-on-the-duplication-adapter.md`.

---

## 15. Questions and cross-cutting concerns for the orchestrator

1. **Backend selection is not what the brief assumed.** `select_vendor` does **not** drive backend
   choice. `build_backend` (pipeline.rs:1432-1456) is a pure try-in-order ladder; `select_vendor` is
   reachable only through `device_vendor`, whose only caller is the Intel same-adapter guard
   (intel/encoder.rs:201). The adapter description is consulted in exactly two places: that guard,
   and `create_nvidia_d3d11_device`'s search for a description containing "NVIDIA"
   (nvidia/encoder.rs:871). So "the wrong adapter can be chosen" is a real risk, but it is about
   *which adapter the capture D3D device lands on* (`D3D11CreateDevice(None, HARDWARE, ...)`,
   windows_capture/d3d11.rs:203), not about vendor misclassification. On the hybrid dev box this is
   genuinely unresolved and needs one measurement — the instrumentation plan is in
   `speculative-wgc-device-lands-on-the-wrong-adapter.md`.
2. **The Windows 10 question dominates everything else here.** If the team cares about Windows 10
   hosts at all, the WGC hard-fail is the first thing to fix; every other Windows latency finding is
   downstream of "which path are we actually on". Someone should confirm the target OS floor before
   the rest of the plan is sequenced.
3. **Cross-territory: `cfg.qp` and the bitrate controller.** `server.rs:827` sets `cfg.qp` for every
   device with an override, which silently disables the entire loss-based BWE in
   `webrtc_session.rs`. This spans territories D (encoder), the server territory, and the WebRTC
   territory — it should be owned by one of them rather than fixed three times.
4. **Cross-territory: the shared A/V clock.** `EncodedFrame.capture` is stamped with `Instant::now()`
   at three different points depending on path, and the true frame time (WGC `SystemRelativeTime`,
   DXGI `LastPresentTime`) is discarded. Whoever owns `streamer/audio`'s `host_now_ns` epoch should
   sign off on the QPC→epoch conversion before it is implemented.
5. **Instrumentation gap.** The only latency number this territory emits is
   `encode-path latency: path=..., avg_ms=..., max_ms=..., frames=...` every 60 frames, which lumps
   capture-callback→broadcast-send into one figure and is absent from the ring path's staging half.
   Before optimising anything, the overhaul needs per-stage timings (delivery, copy, stage, encode,
   send) and a dropped-frame counter. This looks like the first work item, not a side quest.
6. **`is_transient_encode_error` matches the literal string `"device busy"`** (pipeline.rs:84-86).
   Any encoder rewording its error message silently turns a recoverable drop into a session-killing
   error. Worth a typed error variant across all three encoder backends.
7. **`start()` vs `start_on_monitor()` leak their capture control** (`mem::forget` at pipeline.rs:186;
   `drop` of a `Drop`-less `DxgiControl` at :208). Only the probe harness uses `start()`, so this is
   currently harmless, but if anything ever calls `start()` in-process it will leak a capture thread
   and a virtual display per call.

---

## Findings written (24)

Blockers: `blocker-wgc-min-update-interval-forces-dxgi`,
`blocker-nvenc-ring-ignores-idr-on-static-desktop`,
`blocker-nvenc-constqp-disables-bitrate-adaptation`.

Major: `major-amd-hosts-get-no-hardware-encoder`,
`major-dxgi-path-serializes-capture-and-encode`,
`major-start-capture-blocks-a-tokio-worker`,
`major-nvenc-ring-drops-frames-silently`,
`major-nvenc-sync-single-bitstream-defeats-ring`,
`major-dxgi-cursor-costs-a-full-frame-copy`,
`major-capture-timestamp-is-callback-arrival-not-frame-time`,
`major-wgc-path-has-no-unchanged-frame-skip`,
`major-scaler-requires-video-device-on-the-duplication-adapter`.

Minor: `minor-tuning-gpu-preference-applies-next-launch`,
`minor-tune-transport-thread-leaks-mmcss-and-ignores-errors`,
`minor-time-critical-thread-count-scales-with-clients`,
`minor-encode-core-mutex-expect-panics-the-capture-thread`,
`minor-pending-bitrate-target-is-lost-when-set-fails`,
`minor-dead-code-in-windows-streamer-core`,
`minor-keyed-mutex-one-second-timeout-stalls-the-capture-thread`,
`minor-per-frame-allocation-of-the-encoded-access-unit`,
`minor-broadcast-capacity-four-forces-idr-storms`,
`minor-dxgi-dirty-rects-fetched-then-discarded`,
`minor-set-display-scale-reports-success-after-failing`.

Speculative: `speculative-wgc-device-lands-on-the-wrong-adapter`.

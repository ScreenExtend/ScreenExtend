# E2 — Intel Quick Sync (oneVPL), x264 software fallback, AMD AMF stub

Territory: `src-tauri/src/windows_utils/streamer/intel/` (`encoder.rs` 1174 L, `intel_sys.rs` 702 L,
`mod.rs`), `src-tauri/src/windows_utils/streamer/x264/` (`encoder.rs` 540 L, `x264_sys.rs` 515 L,
`mod.rs`), `amd.rs` (19 L), `amd.md` (198 L). Every file read in full. Supporting reads for context
only: `windows_utils/streamer/mod.rs`, `pipeline.rs` (backend selection, encode core, readback),
`scaler.rs`, `streamer/platform.rs`, `streamer/config.rs`, `test/{intel_layout,intel_encoder,x264_encoder}.rs`.

Headline: three findings dominate. (1) The x264 "BGRA direct" fast path almost certainly emits
**High 4:4:4 Predictive** H.264, which browsers cannot decode — and the software encoder is the
fallback every AMD host lands on. (2) Every Intel bitrate change calls `MFXVideoENCODE_Reset` with
`NumExtParam = 0`, discarding the entire low-latency configuration seconds into a session.
(3) `libvpl.dll` is not bundled, so Quick Sync may never engage at all on real Intel hosts.

---

## 1. Files and purpose

**`intel/mod.rs`** (2 L) — `pub mod encoder; pub mod intel_sys;`. Nothing else.

**`intel/intel_sys.rs`** (702 L) — hand-written oneVPL FFI. Primitive typedefs, status codes,
implementation/acceleration/handle/resource enums, codec and rate-control constants, and `#[repr(C)]`
translations of `mfxVariant`, `mfxExtBuffer`, `mfxBitstream`, `mfxFrameId`, `mfxFrameInfo`,
`mfxFrameData`, `mfxFrameSurface1`, `mfxFrameSurfaceInterface` (the vtable), `mfxInfoMFX`,
`mfxInfoVPP`, `mfxVideoParam`, `mfxExtCodingOption`/`2`/`3`, `mfxEncodeCtrl`. Then a `libloading`
dispatcher (`struct Vpl`) that resolves 24 `MFX*` entry points from `libvpl.dll`, plus `check` /
`check_strict` status helpers. Unions in the C headers are flattened to the variant the code uses,
each with a comment naming the aliases; `mfxInfoUnion` is sized for the larger `mfxInfoVPP`.

**`intel/encoder.rs`** (1174 L) — the Quick Sync backend. `InputBridge` (a same-adapter
keyed-mutex shared texture used when the capture device lacks `VIDEO_SUPPORT`), `Encoder` (session +
VPP + ENCODE, BGRA→NV12 conversion and scaling fused into one VPP pass), D3D11 Intel-adapter
selection by LUID then by name, a binary-search frame-rate capability probe, and a synthetic
`probe_encode` for the developer harness.

**`x264/mod.rs`** (2 L) — `pub mod encoder; pub mod x264_sys;`.

**`x264/x264_sys.rs`** (515 L) — hand-written libx264 FFI pinned to `X264_BUILD 164`: csp / picture
type / rate-control / log-level / NAL constants, the full `x264_param_t` (and its `vui`, `analyse`,
`rc`, `crop_rect`, `mastering_display`, `content_light_level` sub-structs), `x264_picture_t`,
`x264_nal_t`, and a `libloading` loader (`X264Api`) that searches `resources/`, the exe dir, PATH and
build-suffixed names, resolving the versioned `x264_encoder_open_<build>` symbol.

**`x264/encoder.rs`** (540 L) — the CPU fallback encoder. `ultrafast` + `zerolatency` preset, an
optional "BGRA direct" input path, a rayon-parallel scalar BT.601 BGRA→I420 converter, runtime
bitrate reconfigure, a drain-on-drop, and a synthetic `probe_encode`.

**`amd.rs`** (19 L) — `struct AmfBackend` implementing `EncoderBackend` with three `bail!("AMD AMF
encoder backend not implemented")` methods. Nothing constructs it.

**`amd.md`** (198 L) — a complete written spec for the AMF backend: binding strategy, data path,
a 21-row latency-driven property table, dynamic controls, seven milestones, a test method, and a
traps list. Summarised in full in §16 below.

---

## 2. Function catalog

### `intel/intel_sys.rs`
- `make_fourcc(a,b,c,d) -> u32` (43) — const FourCC packer.
- `Vpl::load() -> Result<Self>` (645) — `libloading::Library::new("libvpl.dll")` (bare name, no path
  search), resolves 24 symbols through the `resolve!` macro, moves the `Library` into the struct so
  the copied fn pointers stay valid. `unsafe impl Send/Sync for Vpl` (595-596).
- `check(status, what) -> Result<()>` (687) — `status >= 0` is `Ok`; **every warning is swallowed
  silently**.
- `check_strict(status, what) -> Result<()>` (696) — only `MFX_ERR_NONE` is `Ok`.

### `intel/encoder.rs`
- `align_up_u16(v, a) -> u16` (43) — round up to a multiple, cast to u16.
- `InputBridge::new(enc_device, cap_device, cap_context, w, h)` (57) — creates a
  `SHARED_NTHANDLE | SHARED_KEYEDMUTEX` BGRA texture on the encoder device, `CreateSharedHandle`,
  `OpenSharedResource1` on the capture device, keeps both keyed mutexes.
- `InputBridge::upload(src, w, h)` (117) — `AcquireSync(KEY_WRITER)`, `CopySubresourceRegion` from
  the capture texture, `Flush`, `ReleaseSync(KEY_ENCODER)`. The release result is discarded.
- `InputBridge::drop` (155) — `CloseHandle(shared_handle)`, result discarded.
- `Encoder::new(config)` (189) — creates its own Intel D3D11 device, then `new_on_device` with
  native == config dims. This is the "CPU bridge" constructor.
- `Encoder::new_on_device(config, native_w, native_h, device, context)` (194) — **bails if the
  capture device is not Intel** ("cross-adapter Quick Sync bridge is not implemented", 200-206);
  picks same-device or bridge based on `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`; loads oneVPL, sets
  multithread-protected (result ignored), creates the session, sets the D3D11 handle, probes and
  caps fps, computes 16-aligned coded dims, calls `init_vpp` then `init_encode`, logs the requested
  config as achieved.
- `Encoder::device()` (317) — accessor used by `tuning::raise_d3d11_gpu_priority`.
- `Encoder::frame_info(fourcc, coded_w, coded_h, crop_w, crop_h)` (321) — builds an `mfxFrameInfo`
  (progressive, 4:2:0, `FrameRateExtN = fps`, `FrameRateExtD = 1`).
- `Encoder::init_vpp()` (349) — `AsyncDepth = 1`, `IOPattern = IN_VIDEO | OUT_VIDEO`, In = RGB4 at
  src dims, Out = NV12 at config dims (so scale + colour convert fuse), `MFXVideoVPP_Init`.
- `Encoder::init_encode()` (381) — the full `mfxVideoParam` build (see §"oneVPL parameter audit"),
  `MFXVideoENCODE_Init`, then `GetVideoParam` **with the ext buffers cleared**, sizing
  `bitstream_buf` from the effective `BufferSizeInKB`.
- `Encoder::request_idr()` (500) — sets a flag. `#[allow(dead_code)]`; no caller.
- `Encoder::set_bitrate(bps)` (504) — no-op under CQP; otherwise patches `TargetKbps`/`MaxKbps` on
  the saved param and calls `MFXVideoENCODE_Reset` with `ExtParam = null, NumExtParam = 0`.
- `Encoder::encode_texture(src, force_idr)` (527) — `vpp_convert` → `encode_surface` →
  `replace_last_nv12`.
- `Encoder::encode_bgra(bgra, force_idr)` (540) — tight-pitch wrapper.
- `Encoder::encode_bgra_padded(data, row_pitch, force_idr)` (544) — validates length/pitch,
  `UpdateSubresource` into a cached upload texture, then `encode_texture`.
- `Encoder::encode_repeat(force_idr)` (572) — re-encodes the retained NV12 surface; empty `Vec` if
  none.
- `Encoder::ensure_upload_texture()` (585) — lazily creates and caches a `DEFAULT`/`SHADER_RESOURCE`
  BGRA texture at config dims.
- `Encoder::vpp_convert(src)` (612) — gets a VPP-in surface, copies into it, gets a VPP-out surface,
  `RunFrameVPPAsync` with a 1 ms sleep retry on `DEVICE_BUSY` (max 30), releases the input, returns
  the NV12 surface. **The VPP sync point is never synchronised.**
- `Encoder::copy_into_surface(surf, src)` (657) — `FrameInterface::GetNativeHandle`, asserts
  `MFX_RESOURCE_DX11_TEXTURE`, then either a direct `CopySubresourceRegion` + `Flush` (no bridge) or
  bridge-upload + a second keyed-mutex `CopySubresourceRegion` + `Flush`.
- `Encoder::encode_surface(surf, force_idr)` (739) — builds `mfxEncodeCtrl` (`I|IDR|REF`) only when
  forcing, points `mfxBitstream` at the reusable `bitstream_buf`, `EncodeFrameAsync` with the same
  busy-retry, returns empty on `MFX_ERR_MORE_DATA`, then `SyncOperation(100 ms)` in a loop bounded at
  2000 ms, and finally **copies** the coded bytes out into a fresh `Vec`.
- `Encoder::get_surface(kind)` (808) — `MFXMemory_GetSurfaceFor{VPP,VPPOut,Encode}`; `Encode` is
  never requested in practice.
- `Encoder::release_surface(surf)` (831) — `FrameInterface::Release`, result discarded.
- `Encoder::replace_last_nv12(surf)` (845) — releases the previous retained surface.
- `Encoder::drop` (854) — `ENCODE_Close`, `VPP_Close`, `MFXClose`. **No `MFXUnload`.**
- `create_session(vpl)` (865) — `MFXLoad`, two `MFXSetConfigFilterProperty` filters (HARDWARE,
  VIA_D3D11), `MFXCreateSession`, logs the API version. Unloads the loader only on failure.
- `create_intel_d3d11_device()` (916) / `create_intel_d3d11_device_for(capture)` (923) /
  `create_intel_d3d11_device_inner(prefer_luid)` (929) — enumerate adapters, skip software adapters,
  prefer a LUID match, else the first adapter whose description contains "INTEL"; create with
  `BGRA_SUPPORT | VIDEO_SUPPORT` at FL 11.1/11.0.
- `adapter_name(desc)` (998), `adapter_luid(device)` (1007), `luid_eq(a,b)` (1015) — helpers.
- `query_fps_supported(vpl, session, config, fps)` (1019) — builds a *different* param set (no ext
  buffers, no `IdrInterval`) and calls `MFXVideoENCODE_Query`, accepting only if the returned
  `FrameRateExtN` is unchanged.
- `query_max_supported_fps(vpl, session, config, requested)` (1078) — try requested; else probe 30;
  if 30 fails, silently return `min(requested, 60)`; else binary-search 30..requested.
- `probe_encode(config, path)` (1105) — 300 synthetic 1080p60 frames to an Annex-B file.
- `fill_synthetic_bgra(buf, w, h, frame)` (1147) — moving-box test pattern.

### `x264/x264_sys.rs`
- `open_library() -> Result<Library>` (401) — `resources/` under the exe, then the exe dir, then bare
  names on the OS path, then `libx264-<175..150>.dll`.
- `resolve_encoder_open(lib)` (449) — `x264_encoder_open_164`, then any `_<175..150>`, then bare.
- `X264Api::load()` (467) — resolves nine symbols plus the versioned open. `unsafe impl Send/Sync`.

### `x264/encoder.rs`
- `X264Encoder::new(config)` (37) — loads libx264, rejects odd/zero dims, `param_default_preset
  (ultrafast, zerolatency)`, sets geometry/latency/RC/GOP/bitstream/VUI, applies the H.264 profile,
  **then copies the param, flips `i_csp` to BGRA and tries opening that first**, falling back to the
  I420 param only if the BGRA open returns null.
- `X264Encoder::encode_bgra(bgra, force_idr)` (156) — tight-pitch wrapper.
- `X264Encoder::encode_bgra_padded(data, row_pitch, force_idr)` (160) — BGRA path: copy the frame
  into `repeat_bgra`, then `encode_with_bgra_direct`. I420 path: `convert_bgra_to_i420` then
  `encode_converted`.
- `X264Encoder::encode_repeat(force_idr)` (178) — BGRA path clones `repeat_bgra` and re-encodes it
  **with a recomputed `width*4` pitch**; otherwise re-encodes the existing I420 planes.
- `X264Encoder::set_bitrate(bps)` (190) — no-op under QP; else updates `i_bitrate`,
  `i_vbv_max_bitrate`, `i_vbv_buffer_size` and calls `x264_encoder_reconfig`.
- `X264Encoder::encode_with_bgra_direct(data, row_pitch, force_idr)` (207) — one-plane packed BGRA
  picture, `X264_TYPE_IDR` or `AUTO`, `x264_encoder_encode`, copies `size` bytes from the first NAL's
  payload pointer.
- `X264Encoder::encode_converted(force_idr)` (255) — three-plane I420 picture, same encode + copy.
- `X264Encoder::convert_bgra_to_i420(bgra, row_pitch)` (309) — validates, then a rayon
  `par_chunks_mut` over row pairs in a dedicated pool.
- `X264Encoder::drop` (342) — drains up to 64 delayed frames, `x264_encoder_close`.
- `build_convert_pool()` (372) — a `OnceLock` rayon pool of `(cores/2).clamp(1,4)` threads named
  `x264-csc-N`, each raised to `THREAD_PRIORITY_ABOVE_NORMAL`; falls back to a 1-thread pool with
  `.expect(...)`.
- `encode_thread_count()` (394) — `available_parallelism().clamp(1, 8)`.
- `bitrate_kbps(bps)` (401) — `(bps/1000).max(64)`.
- `vbv_buffer_kbit(kbps, fps)` (405) — 2 frames' worth of kbit.
- `convert_row_pair(...)` (411), `bt601_y/u/v` (440/445/450), `clamp8` (455) — scalar BT.601 limited
  range.
- `probe_encode(config, path)` (459), `fill_synthetic_bgra(...)` (509) — harness.

### `amd.rs`
- `AmfBackend::new / encode / set_bitrate` (7-19) — all `bail!`.

---

## 3. Threads

Neither the Intel nor the x264 encoder spawns a thread of its own. They are *driven* by threads owned
elsewhere:

| Thread | Spawned at | Priority / QoS | Lifetime | Owns |
|---|---|---|---|---|
| DXGI/WGC capture+encode thread | `pipeline.rs:444` (`dxgi_capture_thread`) and the WGC callback path | `tuning::tune_current_thread()` (`THREAD_PRIORITY_TIME_CRITICAL`, tuning.rs) + `KeepAwake` | one capture session | the `Duplicator`, the `Arc<Mutex<EncodeCore>>` (and hence `Encoder` / `X264Encoder`), the `Scaler`/`TextureReader` |
| repeater thread | `pipeline.rs::spawn_repeater` (1456) | default | one capture session | calls `EncodeCore::encode_repeat` under the same mutex, on a `frame_duration.max(8ms)` tick |
| `x264-csc-0..N` rayon pool | `build_convert_pool` (x264/encoder.rs:372), lazily on first I420 conversion | `THREAD_PRIORITY_ABOVE_NORMAL` (379-384) | **process lifetime** (`OnceLock`) | nothing; borrows the caller's slices via `install()` |
| libx264's own slice threads | inside `x264_encoder_open` | libx264 default | encoder lifetime | `i_threads` = `min(cores, 8)` sliced threads |
| oneVPL runtime threads | inside the driver | driver's | session lifetime | opaque |

Notable: the CSC pool is process-wide and shared across concurrent client sessions, and it is sized
`(cores/2).clamp(1,4)` independently of `encode_thread_count()`'s `clamp(1,8)` — so on an 8-core box
the two pools request 12 threads between them, with the CSC pool at the higher priority.

---

## 4. Locks

Neither file declares a mutex, rwlock or atomic. All synchronisation is external or GPU-level:

- `Arc<Mutex<EncodeCore>>` (`std::sync::Mutex`, pipeline.rs:504) — guards the whole encoder. Taken by
  the capture thread and by the repeater thread. **Hot path** (once per frame). No `.await` anywhere
  near it; the capture thread is not async. The `if let` temporary-lifetime hazard does not arise
  here — `EncodeCore` is destructured by value after the guard is taken.
- `IDXGIKeyedMutex` on the input bridge (`AcquireSync(KEY_WRITER, 1000ms)` /
  `ReleaseSync(KEY_ENCODER)`, intel/encoder.rs:118-150, and the mirror pair at 723-733) — a real
  cross-device GPU lock on the hot path, two acquire/release round trips per frame in bridge mode.
  `KEY_TIMEOUT_MS = 1000` (intel/encoder.rs:32): a stuck peer blocks the encode thread for a full
  second. Both `ReleaseSync` results are discarded (`let _ =`).
- `ID3D11Multithread::SetMultithreadProtected(true)` on the **encoder** context only
  (intel/encoder.rs:228-231) — the cast failure and the call result are both swallowed. The bridge
  drives the *capture* device's immediate context from the encode thread; see
  `speculative-intel-bridge-uses-capture-context-from-encode-thread.md`.
- `CONVERT_POOL: OnceLock<rayon::ThreadPool>` (x264/encoder.rs:370) — one-time init, then lock-free.
- Raw-pointer state without a lock: `Encoder::last_nv12: *mut mfxFrameSurface1`
  (intel/encoder.rs:176) is a refcounted surface held across frames, protected only by the outer
  `EncodeCore` mutex. `unsafe impl Send for Encoder` (180) and `unsafe impl Send for X264Encoder`
  (x264/encoder.rs:34) are what make that sound.

---

## 5. Buffers and copies

### Intel, same-adapter GPU path (`Backend::Intel`)
1. Capture texture (BGRA, capture device) — **no copy** into the encoder.
2. `copy_into_surface`: `CopySubresourceRegion` capture-texture → VPP input surface, then `Flush()`
   — **GPU→GPU full-frame copy, every frame** (intel/encoder.rs:695-716). Not zero-copy.
3. VPP pass: BGRA→NV12 + downscale in one `RunFrameVPPAsync` — **GPU→GPU, fused, the good part**.
4. Encode reads the NV12 surface — **zero-copy**.
5. `bitstream_buf` (a reused `Vec<u8>` sized from `BufferSizeInKB`, often 8 MB) receives the coded
   bytes, then `self.bitstream_buf[start..end].to_vec()` — **one CPU allocation + copy per frame**,
   of the compressed size only (intel/encoder.rs:801-805).
6. `last_nv12` retains one NV12 surface for `encode_repeat` — **held allocation**, refcounted.

### Intel, bridge path (capture device lacks `VIDEO_SUPPORT`)
Adds, before step 2: `CopySubresourceRegion` capture-texture → shared bridge texture + `Flush`, under
a keyed mutex — a **second GPU→GPU full-frame copy** plus two mutex round trips
(intel/encoder.rs:118-152).

### Intel, CPU bridge (`Backend::IntelCpu`) — the worst path
1. `Scaler::scale` (GPU) → 2. `CopyResource` into staging (GPU→GPU) → 3. blocking
`Map(MAP_READ)` + `copy_nonoverlapping` of `RowPitch*height` into a `Vec` (**GPU→CPU readback,
8.29 MB at 1080p**, scaler.rs:243-259) → 4. `UpdateSubresource` into the Intel upload texture
(**CPU→GPU**, intel/encoder.rs:561-570) → 5. `CopySubresourceRegion` into the VPP surface
(**GPU→GPU**) → 6. VPP → 7. encode → 8. bitstream `to_vec`.
Three full-frame copies and two bus traversals per frame.

### x264
1. Same readback as above → a borrowed `&[u8]` at the driver's `RowPitch`.
2. BGRA path: `repeat_bgra.extend_from_slice(data)` — **full-frame CPU memcpy per frame** (8.29 MB
   at 1080p), then libx264 copies it again internally. `encode_repeat` adds a `clone()` —
   **another full-frame allocation + copy per idle tick**.
3. I420 path: `convert_bgra_to_i420` writes into the preallocated `i420: Vec<u8>` (allocated once in
   `new`, x264/encoder.rs:129) — **no per-frame allocation**, but a full scalar CSC pass.
4. Output: `slice::from_raw_parts((*nals).p_payload, size).to_vec()` — one allocation + copy of the
   coded bytes per frame (x264/encoder.rs:251, 305). This relies on libx264 laying all NALs
   contiguously in `h->nal_buffer`, which it does, but the code does not walk `n_nal`.

---

## 6. Queues

Neither file owns a channel or ring. The relevant ones are upstream:
- `broadcast::Sender<EncodedFrame>` (`BROADCAST_CAPACITY`, pipeline.rs) — the only queue the encoded
  bytes enter. Lagging receivers drop.
- `crossbeam_channel::bounded::<()>(1)` wake channel to the repeater (pipeline.rs:436).
- **Intel's `AsyncDepth = 1`** on both VPP (356) and ENCODE (388) is a deliberate depth-1 "queue":
  submit one, sync, submit the next. This is the right latency choice and it means the encoder has no
  pipelining to exploit — the encode thread blocks in `SyncOperation` for the GPU's full encode time
  (intel/encoder.rs:784-798).
- **x264's `i_sync_lookahead = 0`, `rc.i_lookahead = 0`, `i_bframe = 0`, `b_sliced_threads = 1`** —
  no frame queue at all; frame in, packet out. Verified by the smoke test's
  `assert_eq!(outputs, FRAMES)` (test/x264_encoder.rs:116-119). **Frame threading is genuinely off.**
- The oneVPL `DEVICE_BUSY` retry loops (max 30 × 1 ms sleep) act as an unbounded-ish back-pressure
  stall on the encode thread.

---

## 7. Syscall / FFI boundaries

**oneVPL (`libvpl.dll`, `extern "C"` / `MFX_CDECL` — cdecl, correct on x64 and x86):**
`MFXLoad`, `MFXUnload`, `MFXCreateConfig`, `MFXSetConfigFilterProperty` (takes `mfxVariant` **by
value**), `MFXCreateSession`, `MFXClose`, `MFXQueryVersion`, `MFXQueryIMPL`,
`MFXVideoCORE_SetHandle`, `MFXVideoCORE_SyncOperation`, `MFXVideoENCODE_{Init,Query,Reset,
GetVideoParam,Close,EncodeFrameAsync}`, `MFXVideoVPP_{Init,Query,Reset,Close,RunFrameVPPAsync}`,
`MFXMemory_GetSurfaceFor{VPP,VPPOut,Encode}` (intel_sys.rs:555-597, 648-685). Plus the
runtime-provided vtable `mfxFrameSurfaceInterface::{Release, GetNativeHandle}`
(intel_sys.rs:281-320, called at intel/encoder.rs:663-674, 835-840).

*Struct-version handling:* there is none, by design — oneVPL 2.x structs carry `mfxStructVersion`
fields the code zeroes, and the runtime validates `mfxExtBuffer::BufferSz` against its own `sizeof`.
The bindings are therefore size-critical, and `test/intel_layout.rs` pins them: `mfxFrameInfo` 68,
`mfxInfoMFX` 136, `mfxInfoVPP` 168, `mfxVideoParam` 208, `mfxBitstream` 72, `mfxEncodeCtrl` 48,
`mfxVariant` 16, `CO` 64, `CO2` 68, `CO3` 512, plus key offsets (`InitialDelayInKB` 122,
`TargetKbps` 126, `MaxKbps` 128, `ExtParam` 192, `CO3.LowDelayBRC` 166). **I re-derived all of these
by hand against the oneVPL headers and they are correct**, including the tricky ones: the
`mfxBitstream` leading union is 24 bytes (two pointers + a `u16`, padded) so `DecodeTimeStamp` lands
at 24 and `Data` at 40; the `mfxFrameInfo` frame/buffer union is 12 bytes either way;
`mfxInfoMFX`'s trailing union is exactly 13 × `u16`. Rust `#[repr(C)]` without `packed` matches the
headers' `pack(4)`/`pack(8)` here because no member's natural alignment exceeds the pack value.
Constant values also check out: `MFX_HANDLE_D3D11_DEVICE = 3`, `MFX_RESOURCE_DX11_TEXTURE = 5`,
`MFX_VARIANT_TYPE_U32 = 5`, `MFX_SCENARIO_DISPLAY_REMOTING = 1`, `MFX_TARGETUSAGE_BEST_SPEED = 7`,
`CODINGOPTION_ON/OFF = 0x10/0x20`, `MFX_PROFILE_AVC_CONSTRAINED_BASELINE = 66 + 0x200`.
*The gap is the API version*: no `mfxImplDescription.ApiVersion` filter is set and `MFXQueryVersion`'s
status is discarded (intel/encoder.rs:900-908), so a 1.x runtime passes init and dies at frame 1 in
`MFXMemory_GetSurfaceForVPP`.

**Direct3D 11 / DXGI (COM, stdcall via the `windows` crate):** `D3D11CreateDevice`,
`CreateDXGIFactory1`, `IDXGIFactory1::EnumAdapters1`, `IDXGIAdapter1::GetDesc1`,
`ID3D11Device::{CreateTexture2D, GetCreationFlags}`, `ID3D11Device1::OpenSharedResource1`,
`IDXGIResource1::CreateSharedHandle`, `IDXGIKeyedMutex::{AcquireSync, ReleaseSync}`,
`ID3D11DeviceContext::{CopySubresourceRegion, UpdateSubresource, Flush}`,
`ID3D11Multithread::SetMultithreadProtected`, `CloseHandle`.

**libx264 (`extern "C"`, cdecl):** `x264_param_default_preset`, `x264_param_apply_profile`,
`x264_encoder_open_<build>`, `x264_encoder_headers`, `x264_encoder_encode`, `x264_encoder_reconfig`,
`x264_encoder_intra_refresh`, `x264_encoder_delayed_frames`, `x264_encoder_close`,
`x264_picture_init` (x264_sys.rs:363-394, 466-514). *Version handling:* `X264_BUILD 164` pins the
`x264_param_t` layout, but the loader accepts builds 150-175 and an unversioned symbol — an ABI
hazard, see the findings.

**Win32:** `SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL)` in the CSC pool's
start handler (x264/encoder.rs:379-384).

**macOS/dyld exposure:** none. This territory is `windows_utils/` only; `linux_utils/streamer.rs`
carries the mirror `VaapiBackend` stub.

---

## 8. Panics

- `x264/encoder.rs:390` — `.expect("1-thread fallback rayon pool")` in `build_convert_pool`'s
  fallback. Reachable only if rayon cannot build a 1-thread pool (thread-creation failure under
  exhaustion), and it runs on the encode thread.
- `x264/encoder.rs:246-249, 300-303` — `debug_assert_eq!(pic_out.i_dts, pic_out.i_pts, ...)`.
  Debug-only; would fire if libx264 ever reordered despite `i_bframe = 0`.
- `x264/encoder.rs:332-334` — `&bgra[top*row_pitch .. top*row_pitch + min_stride]` inside the rayon
  closure. Guarded by the length check at 313-319 and by the even-height check in `new` (42-44); a
  panic here would poison a rayon worker.
- `x264/encoder.rs:411-437` — `convert_row_pair`'s indexing. The worst chroma index is
  `(w-2)*4 + 6 = 4w - 2 < 4w = min_stride`, so it is in bounds.
- `intel/encoder.rs` — no `unwrap`/`expect`/`unreachable` on the hot path; every failure is a `bail!`
  or a `context`. The raw-pointer dereferences (`(*surf).FrameInterface`, `(*iface).Release`) are
  null-checked (831-841, 662-666).
- `scaler.rs:241` — `self.staging.as_ref().unwrap()`, safe by construction but on the path that feeds
  both CPU-bridge encoders.
- Arithmetic: `(f * 13) % w.saturating_sub(box_w).max(1)` in both `fill_synthetic_bgra`
  implementations is guarded; `(config.height / 256).clamp(4, 8)` cannot divide by zero.

---

## 9. Silent fallbacks and swallowed errors

**Intel**
1. `check()` accepts every positive `mfxStatus` and logs nothing — including
   `MFX_WRN_PARTIAL_ACCELERATION` (software fallback inside the runtime) and
   `MFX_WRN_INCOMPATIBLE_VIDEO_PARAM` (the runtime changed your parameters). Applied to `VPP_Init`,
   `ENCODE_Init`, `GetVideoParam`, `Reset`, `RunFrameVPPAsync`, `EncodeFrameAsync`, `SyncOperation`
   (intel_sys.rs:687-694). **The single largest observability hole in this territory.**
2. `let _ = (vpl.MFXQueryVersion)(session, &mut ver);` (900) — status discarded; a failed query
   prints "api_version=0.0" and proceeds.
3. `if let Ok(mt) = enc_context.cast::<ID3D11Multithread>() { let _ = ...SetMultithreadProtected }`
   (228-231) — both the cast failure and the call result are swallowed.
4. `let _ = unsafe { self.cap_mutex.ReleaseSync(KEY_ENCODER) };` (149) and
   `let _ = unsafe { bridge.enc_mutex.ReleaseSync(KEY_WRITER) };` (732) — a failed release leaves the
   texture locked and the next `AcquireSync` will time out after 1 s.
5. `let _ = CloseHandle(...)` (100, 157), `let _ = release(surf)` (839) — resource cleanup errors
   dropped.
6. `query_max_supported_fps` returns `min(requested, 60)` with **no log** when the 30 fps probe fails
   (1085-1087).
7. `adapter_luid` uses `.ok()?` three times (1008-1010), so a failed LUID lookup silently degrades
   adapter selection to name matching.
8. `set_bitrate` returns `Ok(())` and does nothing under CQP (505-507) — no log.
9. `Encoder::new_on_device`'s bail → `build_backend` catches it and falls to the CPU bridge with only
   a `teprintln!` (pipeline.rs:1399-1412).

**x264**
10. `let bgra_handle = ...; if !bgra_handle.is_null() { (bgra_handle, true) } else { ... }`
    (113-125) — the encoder silently selects a completely different colour space and profile family
    based on whether one call returned null. The only trace is `csp=BGRA` in one log line.
11. `set_bitrate` returns `Ok(())` under QP (191-193); `config.qp` is otherwise never read.
12. `if size == 0 || n_nal == 0 || nals.is_null() { return Ok(Vec::new()) }` (243, 297) — an empty
    frame is indistinguishable from a dropped one to the caller.
13. `Drop` breaks out of the drain loop on `s <= 0` and after 64 iterations without reporting
    (351-363).
14. `build_convert_pool` falls back from an N-thread pool to a 1-thread pool with no log (386-391).

**Vendor chain** — `build_backend`'s three `teprintln!` fallbacks (pipeline.rs:1364, 1400, 1434-1452)
are logged but only to the log bus; nothing surfaces in the UI, so a host silently running x264 looks
identical to one running NVENC. (`major-compat-win-backends-hardcoded-not-probed.md` covers the UI
side.)

---

## 10. TODO / FIXME / HACK / XXX

None. `grep -n "TODO\|FIXME\|HACK\|XXX"` over `intel/`, `x264/`, `amd.rs` and `amd.md` returns
nothing. The closest things to markers are prose:
- `intel/encoder.rs:204` — `"(cross-adapter Quick Sync bridge is not implemented)"`.
- `amd.rs:8,12,16` — `"AMD AMF encoder backend not implemented"` ×3.
- `x264/x264_sys.rs:461-463` — `"library build likely incompatible with the vendored bindings"`,
  in an error path that is only reached *after* every incompatible build has been tried.
- `x264/encoder.rs` (via commit 450778a) — the comment "x264 runs its own SIMD BGRA->I420 convert
  internally", which is factually wrong; the current file kept the behaviour but dropped the comment.

---

## 11. Latency path

Both encoders sit squarely on the glass-to-glass path; neither touches input or audio.

**Intel same-adapter (best case):**

| Stage | Where | Cost |
|---|---|---|
| capture → VPP-in copy + `Flush` | intel/encoder.rs:695-716 | full-frame GPU blit + a forced submit; unmeasured |
| `RunFrameVPPAsync` (BGRA→NV12 + scale) | 617-648 | one fused VPP pass; unmeasured (**not** synchronised — good) |
| `EncodeFrameAsync` + `SyncOperation` | 739-798 | **blocking**, `AsyncDepth = 1`, so the thread waits the full GPU encode |
| bitstream `to_vec` | 801-805 | one small allocation |
| **serialisation points** | | the copy must finish before VPP; `SyncOperation` blocks the thread before the next frame can even be captured; `DEVICE_BUSY` adds 1-30 ms sleeps |

**Intel bridge:** + one GPU blit, + two keyed-mutex round trips (1 s timeout each), + one `Flush`.

**Intel CPU bridge / x264 (worst case):** + `CopyResource` to staging, + **blocking `Map(MAP_READ)`
= a full CPU↔GPU sync**, + an 8.29 MB memcpy (1080p), all serially on the encode thread before the
encoder starts. Then for x264: + a full scalar CSC (2.07 M luma + 0.52 M chroma samples) on ≤4
threads *or*, on the BGRA path, + another 8.29 MB memcpy; then libx264's own frame copy and encode.
The `spawn_repeater` tick adds an 8.29 MB `clone()` per idle frame on the BGRA path.

**Serialisation summary:** every stage in both backends is strictly sequential on one thread. There
is no pipelining anywhere — capture, copy, convert, encode and sync all happen inside one frame slot,
by design (`AsyncDepth = 1`, `i_sync_lookahead = 0`). That is the right call for latency; the
consequence is that every avoidable copy is directly additive to glass-to-glass, which is why the
readback stall, the extra VPP-input copy and the `repeat_bgra` memcpy all matter more here than they
would in a pipelined design.

---

## 12. Stale references

- `intel/intel_sys.rs:648-651` — "loading libvpl.dll (Intel oneVPL dispatcher; **ships with the
  Intel GPU driver**)". The dispatcher is normally application-redistributed; what the driver ships
  is the runtime. The app bundles no `libvpl.dll` (`tauri.conf.json:22-29`). See
  `major-intel-libvpl-dll-not-bundled.md`.
- `x264/encoder.rs` behaviour vs. commit 450778a's comment "x264 runs its own SIMD BGRA->I420 convert
  internally" — libx264 does no such conversion; RGB csps are encoded as 4:4:4.
- `intel/intel_sys.rs:653-654` — "`Symbol::into_raw` keeps the pointer valid as long as the library
  is alive". `into_raw` is never used; the `resolve!` macro (636-644) dereferences the `Symbol` to
  copy the fn pointer out and then moves the `Library` into the struct. The behaviour is sound, the
  comment describes a different implementation.
- `amd.md:164` — "the project's §16 photographic method"; `amd.md:126` — "the project's M-gating";
  `amd.md:190-192` — "Vendored **AMF headers** (`AMF/amf/public/include/...`)" (there is no `AMF/`
  directory); `amd.md:193` — "(APIs get hallucinated otherwise; see CLAUDE.md)", an AI-tooling
  reference in a source-tree document, pointing at guidance CLAUDE.md no longer contains. Filed as
  `minor-docs-amd-md-references-missing-documents-and-ai-tooling.md`.
- `CLAUDE.md` vs. code: CLAUDE.md says `pipeline.rs` picks "`Intel` tries the same-adapter path, then
  an own-device 'CPU bridge' (`Backend::IntelCpu`), then x264" — accurate. It does **not** mention
  that `Backend::Intel` still performs a per-frame copy into the VPP surface, nor that the AMD path
  has no backend at all. Version drift: CLAUDE.md says "Current version: **0.5.4**" while
  `package.json`/`Cargo.toml` are at 0.6.3 (out of this territory, noted for the orchestrator).

---

## 13. Dead code candidates

| Item | Location | Evidence nothing selects it |
|---|---|---|
| `AmfBackend` | amd.rs:5-19 | `grep -rn "AmfBackend"` finds only the definition. `Backend` has no AMD variant (pipeline.rs:770-775); `EncoderVendor` has no `Amd` variant. Every method `bail!`s. |
| `Encoder::request_idr` | intel/encoder.rs:500-502 | Marked `#[allow(dead_code)]`. IDR is forced through the `force_idr` argument instead. |
| `SurfaceKind::Encode` / `MFXMemory_GetSurfaceForEncode` | intel/encoder.rs:39, 819-822 | `get_surface` is only ever called with `VppIn`/`VppOut` (618, 620). The encode surface always comes from VPP output. |
| `X264Api::encoder_headers` | x264_sys.rs:387, 476-478 | Resolved, never called. `b_repeat_headers = 1` makes it unnecessary. |
| `X264Api::encoder_intra_refresh` | x264_sys.rs:389, 485-487 | Resolved, never called — which is why `b_intra_refresh` is inert (see `major-x264-intra-refresh-is-a-noop.md`). |
| `Vpl::MFXQueryIMPL`, `MFXVideoVPP_Query`, `MFXVideoVPP_Reset` | intel_sys.rs:614, 624-625 | Resolved, never called. `MFXQueryIMPL` would answer the "did I actually get hardware?" question that §9 item 1 leaves open. |
| Unused constants | intel_sys.rs (`MFX_MAP_READ/WRITE`, `MFX_CHROMAFORMAT_YUV444`, `MFX_RATECONTROL_VBR`, `MFX_GOP_CLOSED/STRICT`, `MFX_SCENARIO_REMOTE_GAMING`, `MFX_INFINITE`, most status codes); x264_sys.rs (`X264_CSP_NV12`, `X264_TYPE_I/P`, `X264_RC_CQP/CRF`, all `NAL_*`, `x264_zone_t`, `x264_sei_*`, `x264_hrd_t`) | Whole modules carry `#![allow(dead_code)]`. Keeping the full enum tables is reasonable for an FFI shim; the *functions* above are the real dead weight. |
| `x264_sys.rs`'s 150..175 build fallbacks | 432-439, 452-453 | Not dead, but actively harmful — see `major-x264-abi-accepts-mismatched-builds.md`. |

---

## 14. Capability probing

**Done by probing (good):**
- Intel adapter selection probes the DXGI adapter list by LUID then by description, skipping software
  adapters (intel/encoder.rs:929-984) — no hardcoded index.
- `capture_has_video = creation_flags & D3D11_CREATE_DEVICE_VIDEO_SUPPORT` (intel/encoder.rs:212-214)
  — a real capability probe that selects the bridge.
- `query_max_supported_fps` binary-searches the real `MFXVideoENCODE_Query` (1078-1103) rather than
  assuming a cap — though it probes the wrong parameter set (see findings).
- x264's `resolve_encoder_open` probes exported symbols rather than assuming a build — the right
  instinct, wrong policy (it should reject, not accept, a mismatch).
- The `libvpl.dll` load itself is a probe: absence cleanly fails over.

**Selected by assumption rather than probing (defects):**
- **Vendor is decided by string matching on the adapter description** (`select_vendor`,
  `windows_utils/streamer/mod.rs:22-34`: "NVIDIA" / "INTEL" / "AMD"|"RADEON"|"ADVANCED MICRO
  DEVICES"). A rebranded, virtualised or localised description falls to `Vendor::Unknown` and the
  host silently loses hardware encode. `Vendor::Amd` is computed and then never used.
- **`LowPower = ON` is asserted, never verified** (intel/encoder.rs:410). Low-power (VDEnc) is not
  available on every SKU or for every profile; the runtime may quietly turn it off and return
  `MFX_WRN_INCOMPATIBLE_VIDEO_PARAM`, which `check()` swallows. The init log then claims
  "low_power=on".
- **No oneVPL API-version gate** — 2.x-only entry points are used unconditionally
  (`minor-intel-no-onevpl-api-version-gate.md`).
- **`MFXQueryIMPL` is resolved but never called**, so "am I on hardware?" is never asked directly.
- **The BGRA csp is probed by side effect** — `encoder_open` returning non-null is treated as "this
  build supports packed BGRA input" when what it actually means is "this build will encode 4:4:4".
  This is a probe measuring the wrong property (`blocker-x264-bgra-input-emits-444-stream.md`).
- **No AMD tier at all.** The tier doctrine used for macOS audio/display (probe, pick the best
  available, report the chosen tier to the UI) has no Windows-encoder equivalent: there is no
  `encoder_backend` field in `CompatibilityReport`, so the user is never told which of NVENC /
  Quick Sync / Quick Sync-CPU-bridge / x264 they got. See
  `major-compat-win-backends-hardcoded-not-probed.md`.
- **Missing tier:** AMD AMF (`amd.rs` is a stub) and, on the Intel side, cross-adapter Quick Sync.
  Both are documented and both are unimplemented.

---

## 15. oneVPL parameter audit (the requested focus)

Every field `init_encode` sets (intel/encoder.rs:381-497), with a verdict:

| Field | Value | Verdict |
|---|---|---|
| `AsyncDepth` | 1 (388, and 356 for VPP) | correct for latency; means the thread blocks on `SyncOperation` |
| `IOPattern` | `IN_VIDEO_MEMORY` (encode) / `IN|OUT_VIDEO_MEMORY` (VPP) | correct |
| `CodecId` | `MFX_CODEC_AVC` | correct |
| `CodecProfile` | Constrained Baseline / Main / High from `H264Profile` (403-407) | correct; default is Constrained Baseline, matching `profile-level-id=42e01f` |
| `LowPower` | `ON` (410) | requested, never verified — see §14 |
| `TargetUsage` | `BEST_SPEED` (7) (411) | correct |
| `GopPicSize` | `0xFFFF` (412) | ≈65535 frames (18 min at 60 fps), effectively no periodic IDR — correct intent, but not literally infinite |
| `GopRefDist` | 1 (413) | correct: no B-frames, no reorder delay |
| `IdrInterval` | 0 (414) | every I is an IDR — correct |
| `NumSlice` | `(height/256).clamp(4,8)` (415) | evaluates to 4 for all heights < 1280 and 8 for ≥2048; the divisor makes it look adaptive when it is two-valued |
| `NumRefFrame` | 1 (416) | correct |
| `BufferSizeInKB` | `coded_w*coded_h*4/1024` clamped 512..65000 (394-396, 417) | **wrong** — this is the CPB/VBV size in CBR, and 8160 KB ≈ 6.5 s at 10 Mbps. It is being sized as an output allocation. `major-intel-buffersizeinkb-is-a-multi-second-vbv.md` |
| `RateControlMethod` | `CBR` (or `CQP` when `config.qp`) (419-428) | correct |
| `TargetKbps` / `MaxKbps` | equal in CBR when driven from `set_bitrate`; at init `MaxKbps = max_bitrate_bps` (392-393, 425-426) | correct (peak == target prevents a frame ballooning) |
| `InitialDelayInKB` | 0 in CBR (427) | reads as "runtime default", commonly half the CPB — with the oversized CPB this is a real pre-fill delay |
| CQP aliasing | `InitialDelayInKB = QPI`, `TargetKbps = QPP`, `MaxKbps = QPB` (420-423) | correct use of the flattened union; matches the layout test's offsets |
| `CO.MaxDecFrameBuffering` | 1 (434) | correct — no decoder reorder buffer |
| `CO.NalHrdConformance` / `VuiNalHrdParameters` / `VuiVclHrdParameters` | `OFF` (435-437) | correct — no HRD to conform to |
| `CO.PicTimingSEI` / `AUDelimiter` | `OFF` (438-439) | correct — fewer bytes |
| `CO2.RepeatPPS` | `ON` (444) | correct — in-band parameter sets on every IDR |
| `CO2.BRefType` | `B_REF_OFF` (445) | correct (moot with `GopRefDist = 1`) |
| `CO2.AdaptiveI` / `AdaptiveB` | `OFF` (446-447) | correct — no scene-cut IDR spikes |
| `CO2.IntRefType` / `IntRefCycleSize` / `IntRefQPDelta` | `HORIZONTAL` / `fps` / 0, only when `config.intra_refresh` (448-451) | correct: a 1-second refresh wave |
| `CO2.MaxFrameSize` | **not set** (0) | superseded by `CO3.MaxFrameSizeI/P`, which is finer — fine |
| `CO2.MaxSliceSize` / `NumMbPerSlice` | **not set** | slices are not MTU-bounded; RTP FU-A fragments them. An opportunity for loss resilience |
| `CO2.ExtBRC` / `MBBRC` / `LookAheadDepth` / `Trellis` | **not set** (0 = unknown) | correct to leave off; `ExtBRC` in particular would add a BRC callback |
| `CO3.ScenarioInfo` | `DISPLAY_REMOTING` (457) | correct and well-chosen |
| `CO3.GPB` | `OFF` (458) | correct |
| `CO3.IntRefCycleDist` | `fps * 4` when refreshing (459-461) | 4 s between refresh waves |
| `CO3.LowDelayBRC` | `ON` (464) | correct — the key CBR latency switch |
| `CO3.WinBRCSize` / `WinBRCMaxAvgKbps` | 1 / `max_kbps` (465-466) | correct — a one-frame sliding rate window |
| `CO3.MaxFrameSizeP` / `MaxFrameSizeI` | 2× / 6× the average frame bytes (467-470) | correct and the main defence against the oversized CPB |
| **all three ext buffers on `Reset`** | **dropped** (`NumExtParam = 0`, 512-513) | **the blocker** — every row above marked CO/CO2/CO3 reverts to default at the first bitrate change |

**Same-adapter vs. CPU bridge.** `new_on_device` (194) refuses a non-Intel capture device outright
(200-206). When the capture device *is* Intel but lacks `VIDEO_SUPPORT`, an `InputBridge` is built on
a second Intel device on the same adapter (207-225): a `SHARED_NTHANDLE|SHARED_KEYEDMUTEX` BGRA
texture created on the encode device, opened on the capture device, written under `KEY_WRITER` and
read under `KEY_ENCODER`. That is one extra GPU→GPU full-frame copy plus two mutex round trips per
frame — but it is still all on the GPU. The `Backend::IntelCpu` "CPU bridge" is a different and much
worse thing: it is what `build_backend` falls back to when `new_on_device` *fails*
(pipeline.rs:1399-1412), and it routes the whole frame through system memory
(`Scaler::read_back` → `UpdateSubresource` → `CopySubresourceRegion` → VPP). What it copies and why
is spelled out in §5; the "why" is simply that the cross-adapter case is unimplemented.

**Session init.** `MFXLoad` → two filter properties (`Impl = HARDWARE`,
`AccelerationMode = VIA_D3D11`) → `MFXCreateSession(loader, 0, &session)` →
`MFXVideoCORE_SetHandle(MFX_HANDLE_D3D11_DEVICE, device)` (intel/encoder.rs:236-247, 865-914). The
D3D11 surface is imported the other way round from what a zero-copy design would do: rather than
wrapping the app's texture, the code asks the runtime for a surface
(`MFXMemory_GetSurfaceForVPP`), extracts its native `ID3D11Texture2D` via
`FrameInterface::GetNativeHandle`, checks `MFX_RESOURCE_DX11_TEXTURE`, and copies into it.

**x264 parameter audit** (x264/encoder.rs:47-111): `param_default_preset("ultrafast",
"zerolatency")`; `i_csp = I420` (then possibly flipped to BGRA); `b_vfr_input = 0`,
`i_fps_num/den = fps/1`, `i_timebase_num/den = 1/fps`; `i_bframe = 0`, `b_sliced_threads = 1`,
`i_threads = min(cores, 8)`, `rc.i_lookahead = 0`, `i_sync_lookahead = 0` — **frame threading is
genuinely off, verified by the frame-in/packet-out assertion in the smoke test**;
`rc.i_rc_method = ABR` with `i_bitrate = i_vbv_max_bitrate = kbps` and
`i_vbv_buffer_size = 2 × kbps/fps` (units check out: kbit, i.e. two frames' worth);
`i_keyint_max = 1<<30`, `i_scenecut_threshold = 0`, `b_intra_refresh` from config (inert, see
findings); `b_repeat_headers = 1`, `b_annexb = 1`, `i_log_level = ERROR`; VUI limited-range SMPTE
170M on all three of colorprim/transfer/colmatrix; `param_apply_profile` for the negotiated profile.
`i_slice_max_size` is never set, so slices are bounded only by the sliced-thread count.
`config.qp` is never read.

---

## 16. `amd.md` — full summary of its prescriptions

The document is the AMD mirror of NVENC's rationale block and is explicitly labelled "the spec for
`amd.rs`". Its north star: "minimize glass-to-glass latency. Period" — D3D11-native, zero-copy, one
frame in flight, ~1-frame VBV, no B-frames, no lookahead, no pre-analysis; "if a knob trades 1 ms for
a little PSNR, we spend the PSNR."

**§1 Binding strategy (amd.md:15-31).** No SDK at build time. The AMF runtime ships with the Radeon
driver as `amfrt64.dll`; load it with `libloading` and resolve exactly two C entry points,
`AMFInit(version, **ppFactory)` and `AMFQueryVersion(*pVersion)`. Everything else comes from the
returned `AMFFactory` through vtable calls on COM-like `AMFInterface` objects (`AMFFactory`,
`AMFContext`, `AMFComponent`, `AMFSurface`, `AMFBuffer`, `AMFData`, `AMFVariant`). Vendor only the
MIT-licensed AMF headers and hand-translate the structs/enums/vtables touched into an `amf_sys/`
module mirroring `nvidia/nvenc_sys/` — no `amfrt64.lib`, no toolkit, no LIBCLANG. Version-gate on
`AMFQueryVersion >= AMF_FULL_VERSION`; log and fall through to the next vendor if the driver is too
old (ULL + intra-refresh need a recent driver). Layout: `amd_sys/mod.rs` (loader + vtable typedefs)
plus hand-maintained enum/const files; keep a rationale note at the top like NVENC.
*(Note the internal inconsistency: §1 names the module `amf_sys/` in one sentence and `amd_sys/` in
the next.)*

**§2 Architecture (amd.md:35-73).** Data path: WGC capture (BGRA `ID3D11Texture2D`) → same-adapter
zero copy *or* cross-adapter `CopyResource` into a `SHARED_NTHANDLE|KEYEDMUTEX` texture (reusing
`pipeline.rs::build_zero_copy`) → D3D11 VideoProcessor (`scaler.rs`, extended) doing BGRA→**NV12**
plus optional downscale **in one pass** → `AMFSurface` wrapping that NV12 texture via
`context->CreateSurfaceFromDX11Native` (zero copy) → `AMFComponent AMFVideoEncoderVCE_AVC`
(`SubmitInput`/`QueryOutput`) → `AMFBuffer` of Annex-B H.264 → `Bytes` →
`broadcast::Sender<EncodedFrame>` (transport unchanged). Four architectural decisions: (a) share the
capture `ID3D11Device` via `context->InitDX11(device, AMF_DX11_1)` so surface import is a pointer
wrap — never let AMF create its own device; (b) same-adapter is the common AMD case and the fastest,
so detect it and skip the shared-texture dance entirely, falling back to the keyed-mutex copy only
when the adapters genuinely differ; (c) colour convert is mandatory (VCE wants NV12, WGC gives BGRA)
and must be done in the D3D11 VideoProcessor as a single GPU pass — not on the CPU, and not via AMF's
own `AMFVideoConverter` as a second pass; (d) exactly one frame in flight — submit one surface, query
its output before submitting the next, with `AMF_VIDEO_ENCODER_QUERY_TIMEOUT = 0` and a tight bounded
poll. Output is Annex-B with SPS/PPS forced in-band on every IDR.

**§3 The latency table (amd.md:76-111).** Set before `Init` unless marked per-frame:
`USAGE = ULTRA_LOW_LATENCY` (**set this first** — other USAGE values re-default the encoder and
re-introduce buffering); `QUALITY_PRESET = SPEED`; `LOWLATENCY_MODE = true` (probe-and-set, older
drivers lack it); `RATE_CONTROL_METHOD = CBR`; `RATE_CONTROL_PREANALYSIS_ENABLE = OFF`;
`PREENCODE_ENABLE = OFF`; `B_PIC_PATTERN = 0`; `MAX_NUM_REFRAMES = 1`;
`VBV_BUFFER_SIZE = bitrate / fps` (≈1 frame — "the single biggest CBR latency lever");
`INITIAL_VBV_BUFFER_FULLNESS = 0`; `FILLER_DATA_ENABLE = OFF`; `TARGET_BITRATE == PEAK_BITRATE`;
`IDR_PERIOD` very large; `GOP_SIZE` very large; `INTRA_REFRESH_NUM_MBS_PER_SLOT` sized for a ~1 s
refresh wave; `SLICES_PER_FRAME = 4-8`; `HEADER_INSERTION_SPACING` = SPS/PPS on every IDR;
`PROFILE = CONSTRAINED_BASELINE` (or MAIN) to avoid CABAC and keep receiver decode cheap;
`FRAMERATE` = the real fps num/den; `QUERY_TIMEOUT = 0`; `DE_BLOCKING_FILTER` left on (free on the
encoder, not a latency cost). Per-frame, on the `AMFSurface` before `SubmitInput`:
`FORCE_PICTURE_TYPE = IDR` on join / inbound PLI / FIR (the *only* keyframe trigger, wired to the
existing `idr_request` flag), and `INSERT_SPS` / `INSERT_PPS = true` on each forced IDR.

**§4 Dynamic controls (amd.md:114-121).** `set_bitrate` sets `TARGET_BITRATE` **and** `PEAK_BITRATE`
live on the running component (AMF applies without a rebuild), driven from the existing controller
via `Pipeline::set_target_bitrate`. **Never** change resolution at runtime — surfaces are pinned at
init exactly as with NVENC, and a resize is a teardown plus an IDR spike. Force-IDR uses the
per-surface property, with NVENC's `force_idr()` semantics.

**§5 Milestones (amd.md:125-154), gated one at a time, each must compile, run and pass:**
- **M-AMD0 load & device** — load `amfrt64.dll`/`AMFInit`, create factory + context, `InitDX11` on a
  throwaway device, create + `Init` + destroy the `AMFVideoEncoderVCE_AVC` component. Test: no error,
  logs AMF version + adapter, no leaks over 100 iterations.
- **M-AMD1 NV12 convert** — extend `scaler.rs` to emit NV12; dump a converted test texture. Test:
  `ffplay -f rawvideo -pix_fmt nv12` shows the right image; bytes match a checksum.
- **M-AMD2 encode to file (ULL)** — wrap the NV12 texture as an `AMFSurface`, encode 300 synthetic
  frames with the §3 settings. Test: `ffprobe` shows AVC with `has_b_frames=0`, SPS/PPS present,
  4-8 slices; plays; encode time/frame logged and ≤ NVENC's ~8 ms at the same resolution (sanity).
- **M-AMD3 zero-copy import** — replace any staging copy with `CreateSurfaceFromDX11Native`; assert
  no CPU readback. Test: a debug counter shows zero `Map`/`memcpy` per frame; PIX/GPUView shows no
  round trip.
- **M-AMD4 same-adapter live** — WGC → VP(NV12) → AMF → track. Test: `--probe-live` writes 150 clean
  frames; the WHEP self-test asserts RTP flows.
- **M-AMD5 cross-adapter fallback** — keyed-mutex shared texture, reusing
  `pipeline.rs::build_zero_copy`; log the chosen path. Test: force it, 150 clean frames, a single
  GPU→GPU copy with no CPU bounce.
- **M-AMD6 dynamic bitrate + IDR** — live TARGET/PEAK updates and forced IDR on PLI. Test: an AMD
  analogue of `--probe-bitrate` shows the stream following within a few frames with no resolution
  change and no IDR storm; an injected PLI produces exactly one IDR.

**§6 How to test latency (amd.md:158-168).** `ffprobe -show_frames` → only I/P, `has_b_frames=0`,
SPS/PPS at each IDR. Reuse the per-60-frame avg/max encode-time log; AMD must land in NVENC's band
(single-digit ms at 1080p, low teens at 4K). Glass-to-glass by the photographic stopwatch method,
AMD vs NVENC on the same box; confirm hardware decode in `chrome://media-internals`. Run 5+ minutes
and watch the encode-time **tail**, not the average — "the tail is where latency bugs hide".

**§7 AMF-specific traps (amd.md:172-185).** Setting `USAGE` to anything but ULL, or setting it
*after* other RC properties (it re-defaults them); letting AMF allocate its own D3D11 device instead
of sharing capture's (a hidden cross-context copy per frame); submitting more than one surface before
querying output (an encode queue forms and wall-clock latency grows by queue depth × frame time even
though throughput looks fine); leaving PREANALYSIS/PREENCODE/B-frames on because a preset enabled
them; a VBV larger than ~1 frame or `FILLER_DATA` on; CPU NV12 conversion, or AMF's converter as a
second GPU pass when the VideoProcessor can do scale+convert in one.

**§8 References (amd.md:189-198).** The vendored AMF headers
(`components/VideoEncoderVCE.h` for every `AMF_VIDEO_ENCODER_*` property and enum,
`components/VideoConverter.h`, `core/Factory.h`, `core/Context.h`, `core/Surface.h`) as the
authoritative names; `nvidia/encoder.rs` as the working ULL reference to mirror property-for-property;
`scaler.rs` to extend for BGRA→NV12; `pipeline.rs`'s `build_zero_copy` and latest-wins encode loop to
reuse verbatim, "the encoder is the only swapped part."

**Implementer's note from this review:** two of amd.md's prescriptions have already been proven out
elsewhere in this tree and can be lifted rather than reinvented — the keyed-mutex shared texture
(`intel/encoder.rs::InputBridge`, 47-152, and `pipeline.rs::build_zero_copy`, 1010-1032) and the
one-frame-in-flight discipline (`AsyncDepth = 1` in the Intel backend). The one thing amd.md asks for
that does **not** exist yet is an NV12-capable `scaler.rs`: today `Scaler` hardcodes
`DXGI_FORMAT_B8G8R8A8_UNORM` for its destination, staging and views (scaler.rs:77, 224, 284). That
extension is also the highest-leverage fix for the x264 CSC cost and the CPU-bridge readback size, so
M-AMD1 is worth doing whether or not the AMF backend follows.

---

## 17. Questions and cross-cutting concerns for the orchestrator

1. **Confirm the 4:4:4 blocker on hardware before anything else.** One `ffprobe` on a
   `--probe-encode --software-encode` output settles it. If confirmed, it is the highest-priority fix
   in this territory and it changes the risk profile of every AMD host. It cannot be verified from
   this Windows tree without running the binary.
2. **The Intel `Reset` ext-buffer loss spans territories.** The trigger is the loss-based bitrate
   driver in `webrtc_session.rs` (120 ms poll) — coordinate with the WebRTC agent on how often
   `set_bitrate` actually fires in practice, and with the NVENC agent on whether their
   reconfigure path has the same shape. Several findings already filed against the bitrate controller
   (`blocker-webrtc-rtt-unit-mismatch-collapses-bitrate.md`,
   `blocker-nvenc-constqp-disables-adaptive-bitrate.md`) suggest the controller's behaviour is itself
   unsettled; fixing the controller may change how urgent this is.
3. **NV12 from the D3D11 VideoProcessor is the single highest-leverage change I found.** It serves
   x264 (kills the scalar CSC and shrinks the readback by 62%), the Intel CPU bridge, and a future
   AMF backend (amd.md requires it). It belongs to `scaler.rs`, which is the capture agent's
   territory — please assign it deliberately rather than letting it fall between reports.
4. **Is `libvpl.dll` present on real Intel hosts?** I cannot determine this from the tree. If it is
   not, Quick Sync has never actually run in production and every "Intel path" measurement in this
   report is theoretical. This should be checked before anyone invests in Intel tuning.
5. **Nothing in this territory is measured.** There is no per-stage timing for the VPP-input copy,
   the readback, the CSC, or the encode itself — only a coarse per-60-frame encode-time log in
   `pipeline.rs`. Several of my findings say "measure first" because the code offers no way to rank
   them. A shared per-stage timing harness would pay for itself across all the encoder reports.
6. **The `EncoderVendor` enum has no `Amd` variant**, so `--encoder amd` cannot even be requested
   from the probe harness. Implementing AMF means touching `streamer/config.rs`, `select_vendor`,
   `build_backend` and the CLI parser — worth scoping alongside the backend itself.
7. **Duplicate risk:** I filed `major-cpu-bridge-readback-stalls-encode-thread.md` against
   `scaler.rs`, which is shared with the capture territory. Deduplicate if E1 filed the same thing.
   I deliberately did **not** re-file the AMD-has-no-hardware-encoder finding, which already exists
   as `major-amd-hosts-get-no-hardware-encoder.md`.

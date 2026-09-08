# E1 — NVENC encoder and the hand-translated NVENC FFI

Territory (read in full):
- `src-tauri/src/windows_utils/streamer/nvidia/encoder.rs` (1061 lines)
- `src-tauri/src/windows_utils/streamer/nvidia/mod.rs` (2 lines)
- `src-tauri/src/windows_utils/streamer/nvidia/nvenc_sys/mod.rs` (14 lines)
- `src-tauri/src/windows_utils/streamer/nvidia/nvenc_sys/version.rs` (43 lines)
- `src-tauri/src/windows_utils/streamer/nvidia/nvenc_sys/guid.rs` (233 lines)
- `src-tauri/src/windows_utils/streamer/nvidia/nvenc_sys/nvEncodeAPI.rs` (5318 lines)

Read as supporting context because the brief names `AsyncNvencRing`, `apply_pending_bitrate_atomic`,
`encode_repeat` and `is_transient_encode_error`, none of which live in the territory:
`src-tauri/src/windows_utils/streamer/pipeline.rs` (1888 lines — NVENC-relevant regions read in full),
plus targeted reads of `streamer/server.rs`, `streamer/config.rs`, `windows_utils/streamer/tuning.rs`,
`windows_utils/streamer/mod.rs`, `windows_utils/streamer/test/`, `lib.rs`.

---

## 1. Files and purpose

**`nvidia/mod.rs`** — two `pub mod` lines (`encoder`, `nvenc_sys`). Nothing else.

**`nvidia/encoder.rs`** — the entire NVENC backend. It owns a private D3D11 device created on an
NVIDIA adapter, loads `nvEncodeAPI64.dll` at runtime through `libloading`, opens an NVENC session,
builds `NV_ENC_INITIALIZE_PARAMS` + `NV_ENC_CONFIG` + `NV_ENC_CONFIG_H264` from the
`P1 / ULTRA_LOW_LATENCY` preset, and exposes three input styles: a CPU upload path
(`encode_bgra`, `encode_bgra_padded` via `UpdateSubresource`), a single shared-texture zero-copy path
(`new_shared` + `encode_input`), and a K-slot shared-texture ring (`new_shared_ring` +
`encode_input_slot`/`encode_repeat_slot`). It also carries the live bitrate reconfigure
(`set_bitrate` → `nvEncReconfigureEncoder`), the synthetic-pattern probe (`probe_encode`,
`fill_synthetic_bgra`) and an RAII `Drop` that unregisters resources, closes shared handles and
destroys the bitstream buffer and encoder.

**`nvenc_sys/mod.rs`** — module glue with `#![allow(non_snake_case, non_camel_case_types,
non_upper_case_globals, dead_code)]`, an `#[allow(warnings)] #[rustfmt::skip]` on the bindgen module,
and blanket re-exports. Its doc comment names the upstream source
(`https://github.com/ViliamVadocz/nvidia-video-codec-sdk`) but records no SDK version.

**`nvenc_sys/version.rs`** — **hand-written**, not generated. Defines
`NVENCAPI_STRUCT_VERSION(ver) = NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)` and 30-odd
`NV_ENC_*_VER` constants. This file is the single point where the struct revision numbers the driver
validates are asserted, and it is decoupled from whatever header produced `nvEncodeAPI.rs`.

**`nvenc_sys/guid.rs`** — hand-written GUID constants that bindgen emits incorrectly
(rust-bindgen#1888): codec GUIDs (H.264/HEVC/AV1), the eight H.264 profile GUIDs, the three HEVC and
one AV1 profile GUIDs, and presets `P1`–`P7`. Only `NV_ENC_CODEC_H264_GUID`,
`NV_ENC_H264_PROFILE_{BASELINE,MAIN,HIGH}_GUID` and `NV_ENC_PRESET_P1_GUID` are used.

**`nvenc_sys/nvEncodeAPI.rs`** — bindgen 0.65.1 output for `nvEncodeAPI.h`: ~65 type aliases, ~45
`PNVENC*` function-pointer typedefs, ~50 `#[repr(C)]` structs and 5 unions, ~30 `#[repr(i32)]` enums,
the `__BindgenBitfieldUnit` machinery, and mechanical `impl Default` (zero-fill) plus bitfield
getter/setter impls for every bitfielded struct. Lines 5313-5318 carry a hand-added note that the
trailing `extern "C" { pub fn NvEnc* }` static-link block was deleted, because the crate resolves
everything through `NV_ENCODE_API_FUNCTION_LIST` at runtime — so there is no `nvencodeapi.lib` link
dependency and no import-table entry for NVENC in the shipped binary.

---

## 2. Function catalog

### `nvidia/encoder.rs`

Types: `SlotEncodeError` (enum `Poisoned`/`Encode`, both wrapping `anyhow::Error`),
`EncoderConfig` (`width, height, fps, bitrate_bps, max_bitrate_bps, profile, qp, intra_refresh` — `Copy`),
`Ring` (`input_textures`, `registered_inputs`, `keyed_mutexes`, `shared_handles`, all `Vec` of length K),
`Encoder`. Constants `KEY_WRITER = 0`, `KEY_ENCODER = 1`, `KEY_TIMEOUT_MS = 1000`,
`DXGI_SHARED_RESOURCE_RW = 0x8000_0000 | 0x1`.

- `impl Debug for SlotEncodeError` (:36) — manual, because `anyhow::Error` is not `Debug`-derivable in an enum here.
- `Encoder::new` (:89) — `new_inner(config, shared = false)`, CPU-upload encoder.
- `Encoder::new_shared` (:93) — `new_inner(config, shared = true)`, one shared keyed-mutex input texture.
- `Encoder::new_shared_ring` (:97) — asserts `ring >= 1`; creates the device, loads the DLL, opens the
  session, creates K shared textures, `initialize()`, `create_bitstream()`, then registers each texture
  and creates its `CreateSharedHandle` + `IDXGIKeyedMutex`. Logs a one-line summary.
- `Encoder::new_inner` (:197) — the non-ring constructor; same shape, one texture, plus `setup_shared`.
- `Encoder::setup_shared` (:265) — casts the input texture to `IDXGIResource1`, `CreateSharedHandle`,
  casts to `IDXGIKeyedMutex`, stores both.
- `Encoder::shared_handle` (:282) / `shared_handles` (:286) — hand the NT handles to the capture side.
- `Encoder::encode_input_slot` (:293) — ring hot path. Clones the slot's mutex, `AcquireSync(KEY_ENCODER,
  1000)` (failure ⇒ `Poisoned`), encodes, always `ReleaseSync(KEY_WRITER)`, bumps `frame_index`.
- `Encoder::encode_repeat_slot` (:329) — ring keepalive. `AcquireSync(KEY_WRITER)` on the held slot,
  re-encodes the same texture, `ReleaseSync(KEY_WRITER)`. **Takes no `force_idr` argument** and reads the
  dead `self.idr_requested`.
- `Encoder::device` (:357) — `&ID3D11Device`, used only for `raise_d3d11_gpu_priority`.
- `Encoder::encode_bgra` (:361) — length check, `UpdateSubresource`, `encode_mapped`. CPU→GPU upload.
- `Encoder::encode_bgra_padded` (:392) — same with an explicit row pitch (the DXGI/CPU-bridge path).
- `Encoder::encode_input` (:425) — single-shared-texture path; acquire `KEY_ENCODER`, encode, release `KEY_WRITER`.
- `Encoder::encode_repeat` (:442) — re-encode the last input. If a keyed mutex exists it acquires
  `KEY_WRITER`; otherwise it encodes straight away. This is the classic path's idle keepalive.
- `Encoder::set_bitrate` (:460) — **early-returns `Ok(())` when `qp.is_some()`**. Otherwise copies
  `encode_config`, rewrites `averageBitRate`/`maxBitRate`/`vbvBufferSize`/`vbvInitialDelay`, copies
  `init_params` into `NV_ENC_RECONFIGURE_PARAMS.reInitEncodeParams`, points `encodeConfig` at the stack
  local, calls `nvEncReconfigureEncoder`, then writes both back and re-aliases the self-pointer.
  It never sets `resetEncoder` or `forceIDR` on the reconfigure params.
- `Encoder::request_idr` (:493) — sets `idr_requested`. **No callers.**
- `Encoder::sequence_params` (:497) — `nvEncGetSequenceParams` into a 512-byte buffer. **No callers.**
- `Encoder::check` (:517) — status check that appends `nvEncGetLastErrorString`.
- `Encoder::initialize` (:540) — the whole configuration. Detailed in §"Configuration inventory" below.
- `Encoder::register_input` (:673) / `register_texture` (:679) — `nvEncRegisterResource` with
  `resourceType = DIRECTX`, `pitch = 0`, `bufferFormat = ARGB`, `bufferUsage = NV_ENC_INPUT_IMAGE`.
- `Encoder::create_bitstream` (:699) — one `nvEncCreateBitstreamBuffer`, regardless of ring depth.
- `Encoder::encode_mapped` (:713) — `encode_mapped_resource(self.registered_input, …)`.
- `Encoder::encode_mapped_resource` (:718) — `nvEncMapInputResource` → `encode_locked` →
  `nvEncUnmapInputResource` (whose status is only checked when the encode succeeded).
- `Encoder::encode_locked` (:749) — fills `NV_ENC_PIC_PARAMS`, `nvEncEncodePicture`,
  `nvEncLockBitstream`, `to_vec()`, `nvEncUnlockBitstream`.
- `impl Drop for Encoder` (:808) — unregisters ring resources, closes ring handles, closes the single
  shared handle, unregisters the single input, destroys the bitstream buffer, destroys the encoder. Each
  status goes through `log_drop_status`.
- `create_nvidia_d3d11_device` (:853) — enumerates DXGI adapters, logs each, picks the first non-software
  adapter whose uppercased description contains `"NVIDIA"`, creates a D3D11 device at FL 11_1/11_0 with
  `BGRA_SUPPORT`.
- `create_input_texture` (:904) — `B8G8R8A8_UNORM`, `USAGE_DEFAULT`, `RENDER_TARGET | SHADER_RESOURCE`,
  `CPUAccessFlags = 0`, and when shared `SHARED_NTHANDLE | SHARED_KEYEDMUTEX`.
- `open_session` (:939) — `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS { deviceType: DIRECTX, device,
  apiVersion: NVENCAPI_VERSION }`; on failure calls `nvEncDestroyEncoder` on the (possibly null) handle
  and returns an error without the driver error string.
- `zeroed<T>` (:966) — `MaybeUninit::zeroed().assume_init()`.
- `check` free fn (:970) — takes `_encoder` and ignores it, so no `nvEncGetLastErrorString` detail.
- `log_drop_status` (:977) — `teprintln!` on non-success during drop.
- `probe_encode` (:983) — 1920x1080@60, 300 frames, 10 Mbps, writes Annex-B to a file.
- `fill_synthetic_bgra` (:1030) — moving-box gradient test pattern.

### `nvenc_sys/version.rs`
- `NVENCAPI_STRUCT_VERSION(ver) -> u32` (:8) — `const fn`, `NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)`.
- 30 `pub const NV_ENC_*_VER` values. The ones the code actually sends:
  `NV_ENCODE_API_FUNCTION_LIST_VER = SV(2)`, `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER = SV(1)`,
  `NV_ENC_PRESET_CONFIG_VER = SV(4)|1<<31`, `NV_ENC_CONFIG_VER = SV(8)|1<<31`,
  `NV_ENC_RC_PARAMS_VER = SV(1)`, `NV_ENC_INITIALIZE_PARAMS_VER = SV(6)|1<<31`,
  `NV_ENC_RECONFIGURE_PARAMS_VER = SV(1)|1<<31`, `NV_ENC_REGISTER_RESOURCE_VER = SV(4)`,
  `NV_ENC_CREATE_BITSTREAM_BUFFER_VER = SV(1)`, `NV_ENC_MAP_INPUT_RESOURCE_VER = SV(4)`,
  `NV_ENC_PIC_PARAMS_VER = SV(6)|1<<31`, `NV_ENC_LOCK_BITSTREAM_VER = SV(1)|1<<31`,
  `NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER = SV(1)`.

### `nvenc_sys/nvEncodeAPI.rs`
Entirely generated. Public API surface consists of type aliases, function-pointer typedefs, `#[repr(C)]`
structs/unions, `#[repr(i32)]` enums, and per-struct `Default` (zero-fill) plus bitfield accessors
(`get_x`/`set_x`/`new_bitfield_1`). Notable constants: `NVENCAPI_MAJOR_VERSION = 12` (:1088),
`NVENCAPI_MINOR_VERSION = 1` (:1089), `NVENCAPI_VERSION = 16777228` (:1090, `= 12 | (1<<24)`),
`NVENC_INFINITE_GOPLENGTH = 0xFFFF_FFFF` (:1091), `NV_MAX_SEQ_HDR_LEN = 512` (:1092).
`__BindgenBitfieldUnit::{new, get_bit, set_bit, get, set}` (:1574-1651) are bit-at-a-time loops —
correct but not vectorised; they run only at configuration time, never per frame.

### `pipeline.rs` (NVENC-relevant only)
- `apply_pending_bitrate` (:68) — generic backend version, `target_bitrate.swap(0)`.
- `is_transient_encode_error` (:84) — **`format!("{e:#}").contains("device busy")`**. A string match on
  the formatted error chain. Nothing in `nvidia/encoder.rs` ever produces the substring "device busy";
  it comes from the Intel/x264 backends. So on the NVENC path this predicate is always false, and
  `MAX_TRANSIENT_ENCODE_DROPS = 60` (:82) is dead for NVENC.
- `live_encoder_config` (:88) — fps from `cfg.fps` or `refresh.clamp(60.min(max_fps), max_fps)`; bitrate
  = `cfg.max_bitrate_kbps * 1000` or `(w*h*fps)/10` clamped to 6-30 Mbps; carries `qp`, `profile`,
  `intra_refresh` straight through.
- `build_zero_copy` (:1009) / `build_zero_copy_ring` (:1034) — create the encoder and open its shared
  handle(s) on the capture device via `ID3D11Device1::OpenSharedResource1`.
- `ReadyMsg` (:1063) — `{ slot: usize, force_idr: bool, capture: Instant }`.
- `AsyncNvencRing` (:1069) / `::new` (:1087) / `::stage` (:1148) / `::stop` (:1203) / `Drop` (:1211).
- `ring_encode_thread` (:1218) — the dedicated encode thread loop.
- `apply_pending_bitrate_atomic` (:1328) — `target_bitrate.swap(0, Relaxed)`; skips when 0 or unchanged;
  calls `Encoder::set_bitrate`; logs success or failure. Because `set_bitrate` returns `Ok(())` for a
  CONSTQP encoder, this logs `"adapting bitrate: X -> Y bps"` for a change that did not happen.
- `build_backend` (:1346) — the classic fallback ladder (`try_nvenc` → `try_intel` → `try_x264`).
- `spawn_repeater` (:1460) — classic-path idle repeater. **Not spawned on the ring path.**
- `LiveCapture::new` (:1532) / `on_frame_arrived` (:1693) / `on_closed` (:1765).
- `synthetic_pattern_loop` (:1771) — `--synthetic-pattern` probe driver, uses `Encoder::new`.

---

## 3. Threads

| Thread | Spawned at | Priority / tuning | Lifetime | Owns |
|---|---|---|---|---|
| `nvenc-ring-encode` | `pipeline.rs:1114-1128` (`std::thread::Builder::spawn`) | `tuning::tune_current_thread()` at `pipeline.rs:1228` — MMCSS registration + `raise_current_thread_priority` (`tuning.rs:72-79`) | Created with `AsyncNvencRing`, joined in `AsyncNvencRing::stop` (`pipeline.rs:1203-1208`), which is called from `Drop` (`:1211`) and from `LiveCapture::drop` (`:1003-1005`) | The `Encoder` by value (and therefore the NVENC session, the D3D11 device/context, the K registered resources, the single bitstream buffer, all NT handles). Also `held_slot`, `poisoned_slots`, `current_bitrate` |
| WGC capture delivery thread | owned by `windows_capture`; enters this territory at `on_frame_arrived` (`pipeline.rs:1693`) | tuned once in `LiveCapture::new` via `tuning::tune_current_thread()` (`pipeline.rs:1544`), held in `_thread_tuning` | For the capture session | Calls `AsyncNvencRing::stage`, so it owns the producer half of the keyed mutexes and the `free_slots`/`ready` channel endpoints |
| `nvenc-repeat` | `pipeline.rs:1473-1525` | `tuning::tune_current_thread()` (`:1476`) | classic path only | `Arc<Mutex<EncodeCore>>`; **not used on the NVENC ring path** |
| `dxgi-capture` / synthetic-pattern loop | `pipeline.rs` (outside the ring) | `tune_current_thread` | probe/fallback only | its own `Encoder` |

There is **no thread owned by `nvidia/encoder.rs` itself**; the `Encoder` is a single-owner object moved
onto whichever thread will drive it. `unsafe impl Send for Encoder {}` (`encoder.rs:86`) is what makes
that legal to the compiler; see §"Panics"/findings for the self-referential-pointer caveat.

`enableEncodeAsync = 0` means the NVENC driver has no completion event registered
(`nvEncRegisterAsyncEvent` is never called), so there are no driver-side callback threads that this code
observes; `nvEncLockBitstream` blocks the calling thread instead.

---

## 4. Locks

**No `std::sync::Mutex` is taken anywhere in `nvidia/encoder.rs`.** The encoder is `&mut self`
throughout, single-owner. That is deliberate and good.

The locks that surround it:

| Lock | Guards | Taken by | Hot path? | Across `.await`? |
|---|---|---|---|---|
| `IDXGIKeyedMutex` per ring slot (`Ring.keyed_mutexes`, `encoder.rs:63`; producer copies in `AsyncNvencRing.igpu_mutex`, `pipeline.rs:1071`) | the shared BGRA texture for that slot | producer: `stage()` `AcquireSync(KEY_WRITER, 1000)` (`pipeline.rs:1157`) / `ReleaseSync(KEY_ENCODER)` (`:1175`). consumer: `encode_input_slot` `AcquireSync(KEY_ENCODER, 1000)` (`encoder.rs:314`) / `ReleaseSync(KEY_WRITER)` (`:320`); keepalive `encode_repeat_slot` acquires and releases `KEY_WRITER` (`encoder.rs:344, 348`) | **Yes — once per frame on each side** | No — neither side is async. But `AcquireSync` on the *capture callback thread* can block up to `KEY_TIMEOUT_MS = 1000` |
| `Encoder::input_keyed_mutex` (single-texture path, `encoder.rs:74`) | the one shared input texture | `encode_input` (`:429`), `encode_repeat` (`:445`) | Yes (classic zero-copy path) | No |
| `Arc<Mutex<EncodeCore>>` (`pipeline.rs:984`) | the classic backend + scaler | `on_frame_arrived` `.lock().expect("encode core mutex poisoned")` (`:1716`), dxgi loop (`:599`), repeater (`:1504`) | Yes, classic path only | No — `LiveCapture` is a sync callback handler, not a task. **`None` on the ring path** |
| `Arc<AtomicU32> target_bitrate` | pending bitrate | `Pipeline::set_target_bitrate` `store` (`:63`), `apply_pending_bitrate_atomic` `swap(0)` (`:1333`) | per frame + per keepalive tick | n/a |
| `Arc<AtomicBool> idr_request` | pending keyframe | `Pipeline::request_idr` `store` (`:58`), `AsyncNvencRing::stage` `swap(false)` (`:1187`) | per frame | n/a. **Not visible to `ring_encode_thread`** |
| `Arc<AtomicBool> stop` | shutdown | `LiveCapture::drop` (`:1002`), `AsyncNvencRing::stop` (`:1204`), read at `ring_encode_thread` top (`:1249`) | per keepalive tick | n/a |
| `Arc<AtomicU64> last_frame_at` | freshness clock for the classic repeater | `stage()` store (`:1198`); **explicitly discarded** in `ring_encode_thread` (`let _ = last_frame_at;`, `:1230`) | per frame | n/a |

`if let` temporary-lifetime hazard: the two candidates in this territory are safe.
`Encoder::encode_repeat` uses `if let Some(mutex) = self.input_keyed_mutex.clone()` (`encoder.rs:444`) —
the scrutinee is an owned clone, so no guard outlives the block. `encode_input_slot`/`encode_repeat_slot`
deliberately scope the `ring.as_ref()` borrow into a block that ends before any FFI call
(`encoder.rs:298-312`, `:330-342`), cloning the `IDXGIKeyedMutex` and copying the
`NV_ENC_REGISTERED_PTR` out first. That is the correct pattern and looks intentional.
None of these are `MutexGuard`s and none of these code paths are `async`, so the
"never hold a guard across `.await`" rule is not at risk here.

---

## 5. Buffers and copies

Ring (default NVIDIA path), per frame:

1. WGC delivers `frame.as_raw_texture()` — GPU texture on the **capture** device. *(no copy)*
2. `stage()` → `Scaler::scale_into(raw, dst)` or `igpu_context.CopyResource(dst, raw)` into the
   slot's shared texture (`pipeline.rs:1164-1174`) — **GPU→GPU**, on the capture device.
3. `igpu_context.Flush()` (`pipeline.rs:1172`) — submission, no copy, but a serialisation point.
4. Keyed-mutex handoff `KEY_WRITER → KEY_ENCODER` — **zero-copy**; the same DXGI resource is opened on
   the NVENC device via `OpenSharedResource1` (`pipeline.rs:1053`). If the two adapters differ, the
   *driver* performs a cross-adapter transfer here; the code cannot see or measure it.
5. `nvEncMapInputResource` (`encoder.rs:726-732`) — **zero-copy**, per frame.
6. `nvEncEncodePicture` — NVENC reads the ARGB surface directly. The RGB→YUV conversion happens inside
   the encoder; `NV_ENC_BUFFER_FORMAT_ARGB` is used both at registration (`encoder.rs:687`) and in
   `pic.bufferFmt` (`encoder.rs:761`). **No CPU-side colour conversion anywhere.**
7. `nvEncLockBitstream` (`encoder.rs:778-787`) — maps the driver's output buffer.
8. **`to_vec()` (`encoder.rs:789-795`) — the one forced CPU copy on this path**: a fresh heap
   allocation plus a memcpy out of (typically write-combined) driver memory, performed *inside* the
   bitstream lock.
9. `nvEncUnlockBitstream` (`encoder.rs:797-803`).
10. `Bytes::from(au)` (`pipeline.rs:1267`) — **takes ownership, no copy**.
11. `broadcast::Sender::send` — clones the `Bytes` handle per subscriber, refcount only, **no copy**.

Non-ring paths that force additional CPU copies:
- `encode_bgra` / `encode_bgra_padded` (`encoder.rs:361-423`) — `ID3D11DeviceContext::UpdateSubresource`
  from a CPU slice into a `USAGE_DEFAULT` texture: a **full CPU→GPU upload of `w*h*4` bytes per frame**
  (8.3 MB at 1080p, 500 MB/s at 60 fps). This is the `Backend::Nvenc { path: CpuBridge }` route and the
  DXGI-duplication route.
- `sequence_params` (`encoder.rs:498`) allocates a 512-byte `Vec` per call — dead code.
- `probe_encode` allocates one 8.3 MB frame buffer once (`encoder.rs:1006`) and reuses it — fine.

Allocation per frame in steady state: exactly one (`to_vec`). Everything else is reused.

---

## 6. Queues

| Queue | Depth | Bounded | Blocking | Back-pressure behaviour |
|---|---|---|---|---|
| `free_slots` (`crossbeam_channel::bounded::<usize>(k)`, `pipeline.rs:1102`) | K = 3 (`pipeline.rs:1553`), prefilled with 0..K at `:1104-1106` | Yes | Producer uses **`try_recv`** (`:1149`) — never blocks | **Silently drops the frame** (`return Ok(false)`), and the caller ignores the bool (`:1703`). No log, no counter |
| `ready` (`crossbeam_channel::bounded::<ReadyMsg>(k)`, `pipeline.rs:1103`) | K = 3 | Yes | Producer uses blocking `send` (`:1194`) | Cannot actually block: a `ReadyMsg` only exists once a slot was taken out of `free_slots`, so at most K are outstanding. Consumer `recv_timeout(200 ms)` (`:1252`) |
| Effective encoder queue depth | **1** | — | — | One bitstream buffer + `enableEncodeAsync = 0` ⇒ strictly one picture in flight. One slot is also permanently withheld by `release_and_hold` (`:1235-1246`) for keepalive, so the usable producer pool is 2 |
| `broadcast::Sender<EncodedFrame>` | `BROADCAST_CAPACITY = 4` (`pipeline.rs:43`) | Yes | `let _ = tx.send(...)` (`:1266`, `:1311`) | Errors ignored; slow subscribers see `RecvError::Lagged` |
| `wake` (`crossbeam_channel`) | — | — | `try_send` | **Unused on the ring path** — `wake_rx` is dropped when the ring branch returns at `:1594` |

---

## 7. Syscall / FFI boundaries

Dynamic loading (no link-time dependency, no import-table entry):
- `libloading::Library::new("nvEncodeAPI64.dll")` (`encoder.rs:101`, `:200`) — unqualified name, so it
  resolves via the standard DLL search order. Absent ⇒ `Err`, which is the primary "no NVIDIA driver"
  detection.
- `lib.get(b"NvEncodeAPICreateInstance\0")` → `unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS`
  (`encoder.rs:46`, `:107-114`). The `_lib: libloading::Library` field (`encoder.rs:83`) keeps the module
  loaded for the encoder's lifetime; note it is declared *last* in the struct, so it is dropped last —
  which is correct, because `Drop for Encoder` calls into the DLL.
- All subsequent calls go through `NV_ENCODE_API_FUNCTION_LIST`, i.e. `unsafe extern "C"` (the C calling
  convention; on x86-64 Windows there is only one, so `stdcall`/`cdecl` divergence is not a risk here).

Versioned structs — every one is zero-filled with `zeroed()` then given a `version` word:
`NV_ENCODE_API_FUNCTION_LIST_VER`, `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER` (+ `apiVersion = NVENCAPI_VERSION`),
`NV_ENC_PRESET_CONFIG_VER` + inner `NV_ENC_CONFIG_VER`, `NV_ENC_CONFIG_VER`, `NV_ENC_RC_PARAMS_VER`,
`NV_ENC_INITIALIZE_PARAMS_VER`, `NV_ENC_REGISTER_RESOURCE_VER`, `NV_ENC_CREATE_BITSTREAM_BUFFER_VER`,
`NV_ENC_MAP_INPUT_RESOURCE_VER`, `NV_ENC_PIC_PARAMS_VER`, `NV_ENC_LOCK_BITSTREAM_VER`,
`NV_ENC_RECONFIGURE_PARAMS_VER`, `NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER`.

NVENC entry points actually called: `NvEncodeAPICreateInstance`, `nvEncOpenEncodeSessionEx`,
`nvEncGetEncodePresetConfigEx`, `nvEncInitializeEncoder`, `nvEncRegisterResource`,
`nvEncCreateBitstreamBuffer`, `nvEncMapInputResource`, `nvEncEncodePicture`, `nvEncLockBitstream`,
`nvEncUnlockBitstream`, `nvEncUnmapInputResource`, `nvEncReconfigureEncoder`, `nvEncGetSequenceParams`
(dead), `nvEncGetLastErrorString`, `nvEncUnregisterResource`, `nvEncDestroyBitstreamBuffer`,
`nvEncDestroyEncoder`.
**Never called:** `nvEncGetEncodeCaps`, `nvEncGetEncodeGUIDs`/`GUIDCount`,
`nvEncGetEncodeProfileGUIDs`, `nvEncGetInputFormats`/`FormatCount`, `nvEncGetEncodePresetGUIDs`,
`nvEncRegisterAsyncEvent`/`Unregister`, `nvEncInvalidateRefFrames`, `nvEncGetEncodeStats`,
`nvEncGetSequenceParamEx`, `nvEncSetIOCudaStreams`, `nvEncRestoreEncoderState`,
`nvEncLookaheadPicture`, `nvEncCreateInputBuffer`/`Lock`/`Unlock`, the ME-only family.

Windows APIs crossed: `CreateDXGIFactory1`, `IDXGIFactory1::EnumAdapters1`, `IDXGIAdapter1::GetDesc1`,
`D3D11CreateDevice`, `ID3D11Device::CreateTexture2D`, `ID3D11DeviceContext::UpdateSubresource`,
`IDXGIResource1::CreateSharedHandle`, `IDXGIKeyedMutex::AcquireSync`/`ReleaseSync`, `CloseHandle`,
`ID3D11Device1::OpenSharedResource1` and `ID3D11DeviceContext::{CopyResource, Flush}` (in `pipeline.rs`),
`IDXGIDevice::SetGPUThreadPriority` (in `tuning.rs`).

macOS/dyld exposure: none — this whole territory is `#[cfg(windows)]` by virtue of living under
`windows_utils/`. There are no `extern static`s and no link-time NVENC symbols
(`nvEncodeAPI.rs:5313-5318` documents the removal of the static-link block), so nothing here can affect
the macOS 10.15 dyld floor.

---

## 8. Panics

- `assert!(ring >= 1, "ring size must be >= 1")` (`encoder.rs:98`) — caller-controlled, currently the
  const `RING = 3`. Not runtime-reachable.
- `input_textures[0].clone()` (`encoder.rs:131`) — safe given the assert.
- `ring.registered_inputs[slot]` (`encoder.rs:310`, `:340`) — **unchecked index**. The neighbouring
  `keyed_mutexes.get(slot)` *is* checked and returns an error, so this is inconsistent. Only reachable
  if the four `Ring` vectors ever diverge in length; they are built together, so it is currently
  unreachable but fragile.
- Twelve `.expect("nvEnc… present")` on `Option<fn>` from the driver function list
  (`encoder.rs:482, 509, 551, 662, 692, 705, 729, 739, 773, 784, 800, 953`). A driver that populates
  `nvEncOpenEncodeSessionEx` (the only entry checked, `:116`/`:215`) but omits another entry panics —
  and on the ring path that panic lands on the detached `nvenc-ring-encode` thread whose join result is
  discarded (`let _ = join.join()`, `pipeline.rs:1206`), so the message never surfaces.
- `MaybeUninit::<T>::zeroed().assume_init()` (`encoder.rs:967`) — sound for these `#[repr(C)]` POD
  structs (all fields are integers, raw pointers, `#[repr(i32)]` enums or arrays thereof). But note the
  enums are `#[repr(i32)]` Rust enums, and `0` is a valid discriminant for every enum used
  (`NV_ENC_PARAMS_RC_CONSTQP = 0`, `NV_ENC_BUFFER_FORMAT_UNDEFINED = 0`, `NV_ENC_TUNING_INFO_UNDEFINED = 0`,
  `NV_ENC_DEVICE_TYPE_DIRECTX = 0`, …) — except `NV_ENC_PARAMS_FRAME_FIELD_MODE` and
  `NV_ENC_PIC_STRUCT`, whose lowest discriminant is **1**. `config.frameFieldMode` comes from the preset
  (not zeroed) and `pic.pictureStruct` is explicitly set to `FRAME` (`encoder.rs:762`), so no invalid
  discriminant is currently constructed — but `zeroed()` on any struct containing those enums is UB by
  the letter of the rules and is worth a `MaybeUninit`-plus-explicit-write rewrite if the surface grows.
- In `pipeline.rs` on the classic path only: `core.lock().expect("encode core mutex poisoned")`
  (`:1716`), `self.core.as_ref().expect("classic path must have an EncodeCore")` (`:1711-1714`),
  `free_tx.send(slot).expect("prefill free_slots")` (`:1105`), `.expect("spawn nvenc ring encode thread")`
  (`:1128`).
- `Drop for Encoder` calls into the DLL. If a panic unwinds through it while the encoder is mid-encode,
  `nvEncDestroyEncoder` runs on a session with an outstanding picture — undefined by the SDK. Not
  currently reachable because nothing panics between `nvEncEncodePicture` and `nvEncUnlockBitstream`,
  but the `.expect`s at `:784` and `:800` are exactly there.

---

## 9. Silent fallbacks and swallowed errors

- **`set_bitrate` returns `Ok(())` without doing anything when `qp.is_some()`** (`encoder.rs:460-463`).
  `apply_pending_bitrate_atomic` then logs `"adapting bitrate: X -> Y bps"` and updates its cached
  `current` (`pipeline.rs:1337-1341`) — an outright false success line in the log. This is the single
  most misleading swallow in the territory.
- `let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) }` (`encoder.rs:320`, `:348`, `:433`, `:448`) — a
  failed release is discarded on all four paths. A failed release wedges the slot forever.
- `encode_mapped_resource` (`encoder.rs:736-745`): `nvEncUnmapInputResource`'s status is checked
  **only when the encode succeeded**. If the encode failed, an unmap failure is silently dropped,
  leaking the mapping.
- `open_session` (`encoder.rs:955-960`): on failure it calls `nvEncDestroyEncoder(encoder)` on a handle
  that may still be null, discards that status (`let _ = d(encoder)`), and returns an error built by
  `anyhow!` **without** consulting `nvEncGetLastErrorString`.
- The free `check()` (`encoder.rs:970-975`) takes `_encoder` and ignores it, so
  `NvEncodeAPICreateInstance` and `open_session` failures lose the driver's error text that
  `Encoder::check` (`:517-538`) would have attached.
- `Drop` (`encoder.rs:808-851`): every teardown status is routed to `log_drop_status`, which is at least
  logged (`teprintln!`) — this one is handled well. But `let _ = CloseHandle(h)` (`:821`, `:826`) is
  discarded.
- `pipeline.rs:1611-1616` — an NVENC ring construction failure logs
  `"NVENC zero-copy ring unavailable (…); falling back to classic backend path"` and then re-tries the
  whole ladder. Logged, but it means a single unsupported flag can silently downgrade a user all the way
  to x264 (`pipeline.rs:1443-1455`).
- `pipeline.rs:1149-1152` / `:1703` — the dropped-frame path, discussed above: no log at all.
- `pipeline.rs:1206` — `let _ = join.join()` discards a panic payload from the encode thread.
- `pipeline.rs:1230` — `let _ = last_frame_at;` discards the freshness clock.
- `pipeline.rs:1040` — `let _ = igpu_context;` with the comment
  `// context is passed for symmetry with build_zero_copy / used by caller`.
- `is_transient_encode_error` (`pipeline.rs:84`) matches the substring `"device busy"`, which the NVENC
  backend never emits, so NVENC encode errors are never treated as transient. On the ring path they are
  logged and the slot recycled (`:1290-1295`); on the classic path they abort the capture (`:1732`).

---

## 10. TODO / FIXME / HACK / XXX

Grepped across the whole territory. Verbatim, with location:

- `src-tauri/src/windows_utils/streamer/nvidia/nvenc_sys/nvEncodeAPI.rs:2` —
  `// automatically generated by rust-bindgen 0.65.1`
- `src-tauri/src/windows_utils/streamer/nvidia/nvenc_sys/nvEncodeAPI.rs:5313-5318` —
  ```
  // ---------------------------------------------------------------------------
  // NOTE: the trailing `extern "C" { pub fn NvEnc* }` block of static-link function
  // declarations was removed. This crate loads `nvEncodeAPI64.dll` at runtime via
  // `libloading` (see ../encoder.rs) and dispatches through NV_ENCODE_API_FUNCTION_LIST,
  // so no import library (`nvencodeapi.lib`) is needed at build time.
  // ---------------------------------------------------------------------------
  ```
- `src-tauri/src/windows_utils/streamer/pipeline.rs:1040` —
  `let _ = igpu_context; // context is passed for symmetry with build_zero_copy / used by caller`

**No `TODO`, `FIXME`, `HACK` or `XXX` appears anywhere in the territory.**

---

## 11. Configuration inventory (the brief's specific ask)

Every field set in `Encoder::initialize` (`encoder.rs:540-671`), what it is set to, and whether the
driver's acceptance is verified.

**Preset / tuning selection** (`encoder.rs:541-558`):
- `codec = NV_ENC_CODEC_H264_GUID`; `preset = NV_ENC_PRESET_P1_GUID` (highest performance);
  `tuning = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`.
- `nvEncGetEncodePresetConfigEx(encoder, codec, P1, ULL, &mut preset_config)` **is** called and its
  status **is** checked. The returned `preset_config.presetCfg` becomes the base `NV_ENC_CONFIG`
  (`:558`). Nothing else in the config is verified — `NvEncGetEncodeCaps` is never called anywhere in
  the codebase.

**`NV_ENC_CONFIG`** (`encoder.rs:558-590`):

| Field | Value | Verified? |
|---|---|---|
| `version` | `NV_ENC_CONFIG_VER` (`SV(8) \| 1<<31`) | by `nvEncInitializeEncoder` status only |
| `profileGUID` | `BASELINE` / `MAIN` / `HIGH` from `EncoderConfig.profile`; **default is Baseline** | no cap query |
| `gopLength` | `NVENC_INFINITE_GOPLENGTH` (0xFFFFFFFF) — **no periodic keyframe** | no |
| `frameIntervalP` | `1` — no B-frames (correct for low latency) | `NV_ENC_CAPS_NUM_MAX_BFRAMES` not queried |
| `monoChromeEncoding`, `frameFieldMode`, `mvPrecision` | **not set** — inherited from the P1/ULL preset | no |
| `encodeCodecConfig` | the H.264 union, see below | — |

**`NV_ENC_RC_PARAMS`** (`encoder.rs:569-590`):

| Field | Value | Verified? |
|---|---|---|
| `version` | `NV_ENC_RC_PARAMS_VER` | no |
| `rateControlMode` | `NV_ENC_PARAMS_RC_CONSTQP` **when `qp.is_some()`**, else `NV_ENC_PARAMS_RC_CBR` | `NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES` not queried |
| `constQP` | `{qpInterP: q, qpInterB: q, qpIntra: q}` (CONSTQP branch only) | no |
| `averageBitRate` | `bitrate_bps` (CBR branch only; **unset in CONSTQP**) | no |
| `maxBitRate` | `max_bitrate_bps` (CBR only; equals `bitrate_bps` in practice — `live_encoder_config` sets both the same, `pipeline.rs:115-116`) | `NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE` not queried |
| `vbvBufferSize` | `bitrate_bps / fps` — exactly one frame (CBR only) | `NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE` not queried |
| `vbvInitialDelay` | same as `vbvBufferSize` | no |
| `enableLookahead` | `set_enableLookahead(0)` | — |
| `lookaheadDepth` | `0` | — |
| `enableAQ` | `set_enableAQ(0)` | — |
| `enableTemporalAQ` | `set_enableTemporalAQ(0)` | `NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ` not queried (irrelevant while off) |
| `multiPass` | `NV_ENC_MULTI_PASS_DISABLED` | — |
| `zeroReorderDelay` | `set_zeroReorderDelay(1)` — output order == input order | — |
| `lowDelayKeyFrameScale` | **not set** — inherited from preset, never logged, value unknown to this codebase | no |
| `enableNonRefP` | **not set** — inherited from preset | `NV_ENC_CAPS_SUPPORT_HIERARCHICAL_PFRAMES` not queried |
| `minQP`/`maxQP`/`initialRCQP` + their enable bits, `targetQuality`, `qpMapMode`, `aqStrength`, `strictGOPTarget`, `disableIadapt`/`disableBadapt`, `enableExtLookahead`, `temporallayerIdxMask`, `alphaLayerBitrateRatio`, the QP index offsets | **all unset** — inherited from preset | no |

**`NV_ENC_CONFIG_H264`** (`encoder.rs:592-640`):

| Field | Value | Verified? |
|---|---|---|
| `idrPeriod` | `NVENC_INFINITE_GOPLENGTH` | no |
| `enableIntraRefresh` | `1` **only when `EncoderConfig.intra_refresh`**, which defaults `false` and is unreachable from any shipping path | `NV_ENC_CAPS_SUPPORT_INTRA_REFRESH` (25) and `SINGLE_SLICE_INTRA_REFRESH` (50) not queried |
| `intraRefreshPeriod` | `(fps * 4).max(2)` — 240 at 60 fps | no |
| `intraRefreshCnt` | `(fps / 2).max(1)` — 30 at 60 fps | no |
| `sliceMode` / `sliceModeData` | `0` / `0` ⇒ **one slice per picture**. This *overrides* the preset (which supplies `3`/`1`) | `NV_ENC_CAPS_SUPPORT_DYNAMIC_SLICE_MODE` (27) not queried |
| `maxNumRefFrames` | `1` | `NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES` (47) not queried |
| `numRefL0` | `NV_ENC_NUM_REF_FRAMES_1` | no |
| `numRefL1` | `NV_ENC_NUM_REF_FRAMES_1` — set even though `frameIntervalP = 1` means no B-frames, so L1 is unused. Harmless, but unnecessary | no |
| `repeatSPSPPS` | `set_repeatSPSPPS(1)` — SPS/PPS on every IDR. Correct for WebRTC | no |
| `enableFillerDataInsertion` | `0` | — |
| `outputBufferingPeriodSEI` | `0` | — |
| `outputPictureTimingSEI` | `0` | — |
| `outputAUD` | `0` | — |
| `entropyCodingMode` | `CAVLC` for Baseline; `CABAC` for Main/High | `NV_ENC_CAPS_SUPPORT_CABAC` (7) not queried |
| `adaptiveTransformMode` | `DISABLE` for Baseline/Main; `ENABLE` for High | `NV_ENC_CAPS_SUPPORT_ADAPTIVE_TRANSFORM` (8) not queried |
| `enableConstrainedEncoding` | `1` for Baseline only | `NV_ENC_CAPS_SUPPORT_CONSTRAINED_ENCODING` (24) not queried |
| `enableLTR` / `ltrNumFrames` / `ltrTrustMode` | **never set** (0). `nvEncInvalidateRefFrames` never called | `NV_ENC_CAPS_NUM_MAX_LTR_FRAMES` (40) not queried |
| `level` | **not set** (0 = `NV_ENC_LEVEL_AUTOSELECT`) | `NV_ENC_CAPS_LEVEL_MAX`/`LEVEL_MIN` not queried |
| `chromaFormatIDC`, `separateColourPlaneFlag`, `disableDeblockingFilterIDC`, `numTemporalLayers`, `spsId`/`ppsId`, `fmoMode`, `bdirectMode`, `stereoMode`, `useBFramesAsRef`, `maxTemporalLayers` | **all unset** — inherited from preset | no |
| `enableVFR`, `singleSliceIntraRefresh`, `enableTimeCode`, `useConstrainedIntraPred`, `enableScalabilityInfoSEI`, `disableSPSPPS`, `hierarchicalPFrames`/`BFrames`, `enableTemporalSVC`, `enableStereoMVC` | **all unset** (0) | no |

**H.264 VUI** (`encoder.rs:632-640`) — all explicitly set:
`videoSignalTypePresentFlag = 1`, `videoFormat = UNSPECIFIED`, **`videoFullRangeFlag = 1`**,
`colourDescriptionPresentFlag = 1`, `colourPrimaries = BT709`, `transferCharacteristics = BT709`,
`colourMatrix = BT709`. Note the full-range flag: the input is BGRA, so full-range YUV is the right
choice, and it is signalled — good. `timingInfoPresentFlag`/`numUnitInTicks`/`timeScale` and
`bitstreamRestrictionFlag` are **not** set, so the stream carries no VUI timing and no
`max_num_reorder_frames = 0` hint; some decoders use the latter to shorten their reorder buffer, which
is directly a client-side latency lever worth investigating.

**`NV_ENC_INITIALIZE_PARAMS`** (`encoder.rs:642-657`):

| Field | Value | Verified? |
|---|---|---|
| `version` | `NV_ENC_INITIALIZE_PARAMS_VER` (`SV(6) \| 1<<31`) | status only |
| `encodeGUID` | H.264 | `nvEncGetEncodeGUIDs` never called |
| `presetGUID` | P1 | `nvEncGetEncodePresetGUIDs` never called |
| `encodeWidth`/`encodeHeight` | client-driven (`server.rs:833-835`, clamped only to 2..=16384) | `NV_ENC_CAPS_WIDTH_MAX/HEIGHT_MAX/WIDTH_MIN/HEIGHT_MIN` not queried |
| `darWidth`/`darHeight` | same as encode dims (square pixels) | — |
| `frameRateNum`/`frameRateDen` | `fps` / `1` | — |
| `enablePTD` | `1` — the driver decides picture types | — |
| `enableEncodeAsync` | **`0`** — synchronous; no completion event | `NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT` (30) not queried |
| `tuningInfo` | `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY` | — |
| `maxEncodeWidth`/`maxEncodeHeight` | equal to `encodeWidth`/`encodeHeight` ⇒ **dynamic resolution change is impossible without a full re-init** | `NV_ENC_CAPS_SUPPORT_DYN_RES_CHANGE` (19) not queried |
| `encodeConfig` | `&mut config` (stack local at call time; re-aliased into `self` afterwards) | — |
| `enableSubFrameWrite` | **not set** (0) | `NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK` (23) not queried |
| `enableOutputInVidmem` | **not set** (0) | — |
| `enableWeightedPrediction` | **not set** (0) | `NV_ENC_CAPS_SUPPORT_WEIGHTED_PREDICTION` (41) not queried |
| `bufferFormat` | **not set** (0 = `UNDEFINED`) — only meaningful for D3D12 / output-in-vidmem, so correct | — |
| `reportSliceOffsets`, `enableExternalMEHints`, `enableMEOnlyMode`, `splitEncodeMode`, `enableReconFrameOutput`, `enableOutputStats`, `numStateBuffers`, `outputStatsLevel`, `privData`/`privDataSize`, `maxMEHintCountsPerBlock` | **all unset** (0) | — |

`outputBufferingCount` — the brief asks about this; **it does not exist as a field in this SDK
revision's `NV_ENC_INITIALIZE_PARAMS`**. The nearest analogue is `numStateBuffers` (`nvEncodeAPI.rs:711`),
which is left 0. Output buffering here is instead determined by the single `create_bitstream()` call
(`encoder.rs:699-711`), i.e. an effective buffering count of **1**.

**`NV_ENC_REGISTER_RESOURCE`** (`encoder.rs:679-697`): `resourceType = DIRECTX`, `width`/`height` from
config, `pitch = 0`, `resourceToRegister = texture.as_raw()`, `bufferFormat = ARGB`,
`bufferUsage = NV_ENC_INPUT_IMAGE`. `subResourceIndex` left 0. Called **once per texture at
construction**, never per frame.

**`NV_ENC_MAP_INPUT_RESOURCE`** (`encoder.rs:723-732`): `registeredResource` only. Called **per frame**.

**`NV_ENC_PIC_PARAMS`** (`encoder.rs:754-768`): `inputWidth`/`inputHeight`,
`inputPitch = width * 4` (ignored for DirectX resources), `inputBuffer = mapped`,
`outputBitstream = self.bitstream`, `bufferFmt = ARGB`, `pictureStruct = FRAME`,
`inputTimeStamp = frame_index`, `frameIdx = frame_index as u32`, and on a forced IDR
`encodePicFlags = FORCEIDR | OUTPUT_SPSPPS`. `completionEvent` left null (consistent with
`enableEncodeAsync = 0`). `inputDuration`, `qpDeltaMap`, `meExternalHints`, `codecPicParams` all zero.
Note `inputTimeStamp` is a frame counter, **not** the host clock — so the RTP timestamps the WebRTC
layer stamps (`host_ns_to_rtp90k`) come from `EncodedFrame.capture`, not from anything NVENC returns.

**`NV_ENC_LOCK_BITSTREAM`** (`encoder.rs:778-787`): only `version` and `outputBitstream`.
`doNotWait` left 0 ⇒ blocking. `getRCStats` left 0 ⇒ `frameAvgQP`, `frameSatd`, `hwEncodeStatus`,
`intraMBCount`/`interMBCount` and `pictureType` are all **available and never read** — a free source of
encoder telemetry that is currently discarded.

**`NV_ENC_RECONFIGURE_PARAMS`** (`encoder.rs:467-488`): `version`, `reInitEncodeParams` (a full copy of
`init_params` with `encodeConfig` repointed at a fresh stack config). `resetEncoder` and `forceIDR` are
**not** set, which is correct for a bitrate-only change (no IDR, no session reset).

---

## 12. Latency path

Stages on the glass-to-glass path that live in or are driven by this territory (ring path, the default
on NVIDIA):

| # | Stage | Where | Est. cost | Serialisation |
|---|---|---|---|---|
| 1 | WGC frame delivery | `pipeline.rs:1693` | throttled by `MinimumUpdateIntervalSettings::Custom(frame_duration * 2/3)` (`pipeline.rs:319`) | Frames only arrive on change — a static desktop produces none |
| 2 | Slot acquisition | `pipeline.rs:1149` `try_recv` | ~0 | **Non-blocking; drops the frame if no slot.** Bounded by the encoder's throughput |
| 3 | `AcquireSync(KEY_WRITER)` | `pipeline.rs:1157` | ~0 normally | Waits on the encoder having released the slot. Up to **1000 ms** if the slot is wedged |
| 4 | Scale/copy + `Flush()` | `pipeline.rs:1164-1174` | GPU copy of a full frame + a full immediate-context flush | Runs **on the capture callback thread**; blocks frame N+1's delivery |
| 5 | `ReleaseSync(KEY_ENCODER)` + `ready.send` | `pipeline.rs:1175, 1194` | ~0 | Thread handoff to `nvenc-ring-encode` |
| 6 | `apply_pending_bitrate_atomic` | `pipeline.rs:1263` | `nvEncReconfigureEncoder` when a change is pending; **a no-op today** because CONSTQP short-circuits | Synchronous on the encode thread |
| 7 | `AcquireSync(KEY_ENCODER)` | `encoder.rs:314` | ~0 | Waits on stage 5 |
| 8 | `nvEncMapInputResource` | `encoder.rs:726` | µs | Per frame |
| 9 | `nvEncEncodePicture` | `encoder.rs:770` | submit only | — |
| 10 | **`nvEncLockBitstream`** | `encoder.rs:781` | **the full frame encode time** — single slice, no sub-frame write, `doNotWait = 0` | **The dominant serialisation point.** One bitstream buffer + `enableEncodeAsync = 0` ⇒ exactly one picture in flight; frame N+1 cannot even be submitted |
| 11 | `to_vec()` | `encoder.rs:789-795` | one alloc + a memcpy from write-combined memory, **inside the lock** | Extends stage 10's critical section |
| 12 | `nvEncUnlockBitstream` + `Unmap` + `ReleaseSync(KEY_WRITER)` | `encoder.rs:797-803, 736-745`, `:320` | µs | Frees the slot for the producer |
| 13 | `Bytes::from` + `broadcast::send` | `pipeline.rs:1266-1269` | ~0 (refcount) | — |

Serialisation points, ranked:
1. **Stage 10** — no encoder pipelining at all. The K=3 ring decouples *capture* from encode and buys
   nothing on the encode side.
2. **Stage 4** — a full `Flush()` on the capture callback thread, per frame.
3. **Stage 11** — the WC-memory memcpy sits inside the bitstream lock.
4. **Stage 3** — normally free, but a 1 s worst case on a callback thread.

Idle path: with no frames arriving, `ring_encode_thread`'s `recv_timeout(200 ms)`
(`pipeline.rs:1252, 1231`) re-encodes the held slot as a P-frame every 200 ms to keep the WebRTC
connection alive. This is the path that cannot produce an IDR.

Not on this path: the input path (`streamer/input/`) and the audio path
(`streamer/audio/`, `windows_utils/audio/`) touch none of these files.

**Nothing in the ring path is instrumented.** No timing, no drop counter, no
`encode-path latency:` line. Any latency claim about this territory is currently unmeasured.

---

## 13. Stale references

- `nvenc_sys/mod.rs:1` cites `https://github.com/ViliamVadocz/nvidia-video-codec-sdk` as the source of
  the bindings but records **no SDK version and no upstream commit**, which is the root of the
  version-provenance problem in §14.
- `nvEncodeAPI.rs:2` claims "automatically generated by rust-bindgen 0.65.1" for a file that has since
  been hand-edited (the static-link block removal at `:5313-5318`, and the `pub use super::{guid::*, version::*};`
  prepended at `:1`). It is no longer regenerable by re-running bindgen alone.
- `encoder.rs:183` and `:251` log `"(D3D11 / ULL)"` and `rc={}` where the `rc` string is derived as
  `if config.qp.is_some() { "constqp" } else { "cbr" }` — accurate, but it is the *only* place the
  CONSTQP switch is visible, and it is a one-shot init line rather than anything the operator would
  correlate with a stalled stream.
- No references to `PRD`, `AUDIO_NOTES*`, `docs/airplay-fallback-design.md`, `§`, or `M1`/`M2` anywhere
  in the territory. No references to AI tooling.
- `CLAUDE.md` says "Current version: **0.5.4**" while `package.json`/`Cargo.toml` are at 0.6.3
  (`git log` shows `4b745d1 build(release): bump version to 0.6.3`) — a doc/code disagreement outside
  this territory, noted per the brief's "where they disagree, the code wins" instruction.

---

## 14. Dead code candidates

| Item | Location | Evidence nothing selects it |
|---|---|---|
| `Encoder::request_idr` | `encoder.rs:493-495` | grep for `request_idr` over `src-tauri/src` returns only `Pipeline::request_idr` (windows + macos), `IntelEncoder::request_idr`, and their callers. Nothing calls the NVENC method |
| `Encoder::idr_requested` field + 5 read sites | `encoder.rs:82`, read at `:318, 346, 383, 416, 443` | only writer is `request_idr` (above), so permanently `false`. **This is the mechanism behind the ring IDR blocker** |
| `Encoder::sequence_params` | `encoder.rs:497-515` | grep for `sequence_params` returns only the definition. Out-of-band SPS/PPS is unnecessary because `repeatSPSPPS(1)` + `NV_ENC_PIC_FLAG_OUTPUT_SPSPPS` put them inline |
| `Encoder::new_shared` / `build_zero_copy` / `EncodePath::ZeroCopy` | `encoder.rs:93`, `pipeline.rs:1009-1032` | only reachable via `build_backend`, which the ring path bypasses entirely on NVIDIA (`pipeline.rs:1554-1609`). Live only as the fallback after a ring failure |
| `Encoder::encode_bgra` | `encoder.rs:361-390` | called only by `probe_encode` (`:1010`) and `synthetic_pattern_loop` (`pipeline.rs:1814`) — probe-only |
| `is_transient_encode_error` on the NVENC path | `pipeline.rs:84` | matches `"device busy"`, a string no NVENC error produces; `MAX_TRANSIENT_ENCODE_DROPS`, `LiveCapture::busy_streak` and the retry branch at `:1721-1731` are unreachable for NVENC |
| `LiveCapture::{frames_sent, timing_sum_ns, timing_count, timing_max_ns, path_name}` on the ring path | `pipeline.rs:990, 995-997, 989` | the ring branch returns at `:1708`, before the block at `:1742-1761` that is their only reader |
| `wake_rx` on the ring path | `pipeline.rs:1541-1542` | dropped when the ring branch returns at `:1594`; `Pipeline::request_idr`/`set_target_bitrate` still `try_send` into a channel with no receiver |
| ~95% of `nvEncodeAPI.rs` | HEVC, AV1, MEONLY, D3D12 fence points, OpenGL, CUDA-stream, film-grain, external-ME-hint, time-code, restore-state and lookahead types | `#![allow(dead_code)]` in `nvenc_sys/mod.rs` suppresses the warning. This is normal and correct for a vendored FFI header — do **not** prune it, it is the contract with the driver |
| `nvenc_layout.rs` (6167 lines) | `windows_utils/streamer/test/nvenc_layout.rs` | Runs, but asserts the Rust struct's own offsets against constants from the same bindgen run. It is self-referential and cannot detect a wrong-SDK header — its value is limited to catching hand-edits |

---

## 15. Capability probing

**The one rule this territory breaks the hardest: there is no capability probe at all.**

- `nvEncGetEncodeCaps` — present in the bindings (`nvEncodeAPI.rs:1050`), 55 caps enumerated
  (`nvEncodeAPI.rs:1387-1444`), **zero call sites in `src-tauri/src`**. Same for
  `nvEncGetEncodeGUIDs`, `nvEncGetEncodeProfileGUIDs`, `nvEncGetInputFormats` and
  `nvEncGetEncodePresetGUIDs`. Every field in §11 marked "not queried" is asserted blind.
- What the code does instead is **attempt-and-fallback**, which is the right *doctrine* but far too
  coarse a *mechanism*: a single rejected flag makes `nvEncInitializeEncoder` fail, which fails
  `build_zero_copy_ring`, which logs `"NVENC zero-copy ring unavailable"` (`pipeline.rs:1611-1616`) and
  drops through `build_backend` → NVENC → Intel → x264 (`pipeline.rs:1443-1455`). A user with a
  perfectly good NVENC engine that merely lacks, say, `SINGLE_SLICE_INTRA_REFRESH` can end up on
  software x264.
- **Version-number gating: none, and that is correct.** Grepping `src-tauri/src/windows_utils` for
  `driver_version`, `NvAPI`, `nvml`, or a driver-version literal returns nothing. Availability is
  detected by `libloading::Library::new("nvEncodeAPI64.dll")` failing (`encoder.rs:101`) and by
  `fns.nvEncOpenEncodeSessionEx.is_none()` (`encoder.rs:116`). Both are true probes.
- **But there is an implicit version gate**: `params.apiVersion = NVENCAPI_VERSION` (`encoder.rs:947`,
  = 12.1) is a hard floor — NVENC rejects an `apiVersion` newer than the installed driver. No message
  tells the user "your driver is older than X"; they just silently land on x264.
- **Hardware selection is a string match**, not a probe: `name.to_uppercase().contains("NVIDIA")`
  (`encoder.rs:871`) and `d.contains("NVIDIA")` (`windows_utils/streamer/mod.rs:25`). No `VendorId`
  check, no NVENC-engine-count check, no relationship to which adapter owns the captured display.
- **Missing tier**: there is no "NVENC present but degraded" tier. The ladder is
  NVENC-ring → NVENC-classic → Intel → x264, all-or-nothing at each step. A per-feature degrade
  (drop intra refresh, drop CABAC, drop custom VBV, but keep NVENC) does not exist.
- **The version-constant provenance is itself unverified**: `NVENCAPI_MAJOR_VERSION = 12` /
  `MINOR = 1` (`nvEncodeAPI.rs:1088-1089`) sits alongside a function list and struct set that carry
  post-12.1 members (`nvEncRestoreEncoderState`, `nvEncLookaheadPicture`, `numStateBuffers`,
  `outputStatsLevel`, `splitEncodeMode`, `NV_ENC_CAPS_EXPOSED_COUNT = 55`), and the struct revision
  numbers are hand-asserted in `version.rs`. See
  `findings/major-nvenc-sdk-version-vs-struct-layout-mismatch.md`.

---

## Questions and cross-cutting concerns for the orchestrator

1. **`video_quality` semantics is a product decision, not just an encoder one.** Wiring the slider to
   `cfg.qp` turns off rate control entirely for every device with a saved override. Whoever owns the
   Settings UI needs to decide whether the slider means "quality target under a bitrate cap"
   (→ `rcParams.targetQuality` + min/max QP, keeps CBR and the BWE loop live) or "fixed QP, no cap".
   This spans E1, the session/server territory and the desktop UI. **Cross-territory.**
2. **The client's `profile-level-id` is never read.** Profile negotiation belongs in the WebRTC/session
   territory, but all three encoder backends consume the result. Someone should own the mapping.
3. **VUI timing and `bitstreamRestrictionFlag`/`max_num_reorder_frames` are not signalled.** That is a
   host-side one-line change with a client-side decoder-latency payoff. It needs a client-territory
   owner to measure the effect in `transform-worker.js`.
4. **Ordering dependency.** Nothing in this territory can be tuned until
   `major-nvenc-ring-path-emits-no-latency-telemetry.md` lands — the primary NVIDIA path currently
   emits no numbers at all. Suggested order: telemetry → IDR blocker → CONSTQP blocker → encoder
   pipelining → sub-frame readback.
5. **Hardware access.** Everything here is Windows + NVIDIA only, and the dev box in `CLAUDE.md` is an
   Intel Catalina MacBook. None of these findings can be verified without an NVIDIA Windows host; the
   engagement needs one, plus ideally a second GPU config (Optimus laptop, dual-NVIDIA desktop) for the
   adapter-selection finding.
6. **Duplicate findings.** Four of my conclusions were filed independently by a concurrent agent
   (`blocker-nvenc-constqp-disables-bitrate-adaptation.md`,
   `blocker-nvenc-ring-ignores-idr-on-static-desktop.md`,
   `major-nvenc-ring-drops-frames-silently.md`,
   `major-nvenc-sync-single-bitstream-defeats-ring.md`). I deleted my near-identical copies and kept
   theirs. Treat the overlap as independent corroboration.
7. **Open question I could not resolve from the tree:** which NVENC SDK version `nvEncodeAPI.rs` was
   actually generated from. Resolving it requires the upstream crate version or the original header;
   until then the `version.rs` constants should be treated as unverified.

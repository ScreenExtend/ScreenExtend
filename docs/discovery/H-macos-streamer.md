# H — macOS streamer (`src-tauri/src/macos_utils/streamer/`)

Territory: every file in `src-tauri/src/macos_utils/streamer/` read in full —
`mod.rs` (169), `pipeline.rs` (736), `encoder.rs` (895), `sck.rs` (791),
`cgds.rs` (423), `qos.rs` (177), `frame.rs` (125), `display.rs` (83),
`config.rs` (57), `power.rs` (77), `tuning.rs` (38), `mach.rs` (35),
`gpu.rs` (15), `activity.rs` (17). 3638 lines total. There are no other files in
the directory (no `test/` subdirectory; tests are inline `#[cfg(test)]`, all of
them `#[ignore]`d except `frame.rs`'s two and `sck.rs`'s one).

Line numbers are from the working tree at commit `bef5f89`.

---

## 1. Files and purpose

**`mod.rs`** — the module root and the backend selector. Declares
`CaptureError`, the `CaptureBackend` trait (`start`/`stop`), `CaptureConfig`,
the two pixel-format constants (`PIXEL_FORMAT_420F` = `'420f'`,
`PIXEL_FORMAT_BGRA` = `'BGRA'`), the OS-version helper `macos_at_least`, the TCC
helpers, and `start_capture()`, which tries ScreenCaptureKit when
`screencapturekit_available()` and falls back to CGDisplayStream, reporting
`FallbackFailed` only when both fail. `#![allow(dead_code)]` covers the whole
module, which is why none of the dead surface here warns.

**`pipeline.rs`** — the macOS half of the streaming pipeline. Owns
`EncodedFrame`, `Pipeline` (the handle the WebRTC layer holds: a
`broadcast::Sender`, the frame duration, an IDR-request flag, a target-bitrate
cell, a wake sender, the max bitrate and the profile), `SessionCapture` (the
RAII session), `spawn_pipeline`, `run_encode_loop` (the encode thread body),
`submit_one`, the two entry points `start` / `start_on_monitor`, the probe
harness entry points `probe_bitrate` / `probe_live`, and the display-control
surface (`set_display_resolution`, `set_display_mode`,
`set_display_orientation`, `set_display_scale`, `monitor_*`) that forwards to
`macos_utils::virtual_display::reconfigure_display`.

**`encoder.rs`** — the VideoToolbox H.264 compression session. `Encoder` wraps a
`VTCompressionSession`, a `crossbeam` receiver of `CompressedSample`, and the
boxed output context. Handles session creation over a three-entry
encoder-specification ladder, all property configuration, rate control (ABR or
QP), frame submission with an optional force-keyframe dictionary, the
asynchronous output callback, and AVCC→Annex B conversion with SPS/PPS
in-band insertion on keyframes.

**`sck.rs`** — the ScreenCaptureKit capture backend (12.3+). Notable for using
**runtime interop only** — `AnyClass::get`, `msg_send!`, a `ClassBuilder`-built
`SCStreamOutput` delegate, and `dlsym`'d SCK CFString constants — so the binary
has no dyld dependency on ScreenCaptureKit and still loads on 10.15. Bridges the
two async SCK APIs (`getShareableContentWithCompletionHandler:`,
`startCaptureWithCompletionHandler:`) to synchronous calls with channels and 5 s
timeouts.

**`cgds.rs`** — the CGDisplayStream fallback (`#![allow(deprecated)]`). Builds
the stream property dictionary, installs an `RcBlock` frame handler on a
UserInteractive serial dispatch queue, wraps each delivered IOSurface in a fresh
CVPixelBuffer, and publishes it. Also carries `probe_screen_recording()`, the
64×64 700 ms TCC probe.

**`frame.rs`** — the capture→encode hand-off. `Frame` (dimensions, an
`arrived_mach` tick stamp, a `captured_at: Instant`, and a `Backing` that is
either `Empty` or a retained `CVPixelBuffer` + retained `IOSurfaceRef`), and a
single-slot latest-wins channel: `FrameSink` (an `ArcSwapOption<Frame>` plus a
`bounded(1)` wake sender) and `FrameSource`.

**`qos.rs`** — thread scheduling. `pthread_set_qos_class_self_np` wrappers for
USER_INTERACTIVE and USER_INITIATED, a `THREAD_TIME_CONSTRAINT_POLICY` setter,
a `THREAD_PRECEDENCE_POLICY` setter, an x86_64-only `THREAD_AFFINITY_POLICY`
setter, and `FrameWorkgroup` — a dlsym'd `os_workgroup_interval_create` +
`os_workgroup_join`.

**`display.rs`** — CoreGraphics display enumeration: `CGGetActiveDisplayList`,
`CGMainDisplayID`, `CGDisplayPixelsWide/High`, `CGDisplayCopyDisplayMode` for
refresh rate, and a hand-declared `CGDisplayBounds`. Display "names" are just the
`CGDirectDisplayID` rendered as a decimal string.

**`config.rs`** — `EncoderConfig` and the two functions that derive it:
`scaled_dims` (percentage scale, rounded down to even) and
`live_encoder_config` (fps, dimensions, bitrate).

**`power.rs`** — `KeepAwake`, an RAII pair of IOPMAssertions
(`PreventUserIdleSystemSleep`, `PreventUserIdleDisplaySleep`) created by name.

**`tuning.rs`** — process-wide tuning: one `NSProcessInfo` activity
(UserInitiated | AutomaticTerminationDisabled | SuddenTerminationDisabled) held
in a `OnceLock`, plus a `setpriority(PRIO_PROCESS, 0, -10)`.

**`activity.rs`** — `LatencyActivity`, a second `NSProcessInfo` activity
(UserInitiated | LatencyCritical) taken per capture session.

**`mach.rs`** — `mach_absolute_time` plus a cached `mach_timebase_info`, and the
three conversions built on it (`mach_age_ms`, `mach_delta_ns`,
`ns_to_mach_ticks`).

**`gpu.rs`** — a 15-line empty stub. `pub struct Gpu { _private: () }`. The two
commented-out fields (`MTLDevice`, `CVMetalTextureCache`) were never written.

---

## 2. Function catalog

### `mod.rs`
- `macos_at_least(major, minor) -> bool` (61) — forks `sw_vers -productVersion`,
  parses `major.minor`, compares. **No caching; fails open (returns `true`).**
- `screencapturekit_available() -> bool` (80) — `macos_at_least(12, 3)`. Marked
  `#[inline]`, which hides that it forks a process.
- `ensure_screen_recording_access() -> bool` (84) — `probe_screen_recording()`
  first; on failure `dlsym`s `CGRequestScreenCaptureAccess` and calls it,
  treating a missing symbol as granted.
- `probe_screen_recording() -> bool` (107) — SCK probe on 12.3+, else the cgds
  probe.
- `start_capture(display, gpu, sink, cfg) -> Box<dyn CaptureBackend>` (116) —
  SCK then CGDS; logs the SCK failure and merges both errors into
  `FallbackFailed`.
- `start_sck` (149) / `start_cgds` (160) — construct + `start()`.

### `pipeline.rs`
- `Pipeline::request_idr` (44) — sets an `AtomicBool`, pokes the wake channel.
- `Pipeline::set_target_bitrate(bps)` (49) — stores into an `AtomicU32`, pokes
  the wake channel. `0` is the sentinel for "nothing pending".
- `SessionCapture::stop` (63) / `shutdown` (65) / `Drop` (79) — set the stop
  flag, stop the backend, join the encode thread.
- `spawn_pipeline(enc, source)` (82) — builds the broadcast channel, the atomics
  and `frame_duration`; constructs the `Encoder` **on the calling thread**;
  spawns the `videotoolbox-encode` thread.
- `run_encode_loop(...)` (126) — applies four thread policies, joins a
  workgroup, takes the keep-awake assertions, spawns the `videotoolbox-drain`
  thread, then loops: wait ≤4 ms → swap the IDR flag → swap the pending bitrate →
  take the latest frame and submit it, or (if none) re-submit `last_frame` when
  an IDR is pending or 200 ms have passed.
- `submit_one(...)` (226) — pushes `captured_at` onto `pending_ts`, calls
  `Encoder::submit`, pops it back on error. Returns `false` on error, which the
  caller turns into a permanent `break`.
- `start(cfg) -> Pipeline` (245) — the single-display entry point.
  `mem::forget`s the backend and the activity, and drops the stop flag and the
  join handle.
- `start_on_monitor(cfg, device_name) -> SessionCapture` (267) — the production
  entry point, called from `streamer/server.rs:1038`.
- `probe_bitrate(cfg)` (295) / `probe_live(cfg, path)` (353) — developer probes.
- `make_synthetic_bgra(w, h, tick)` (411) — a CPU-filled BGRA test buffer.
- `select_display` (455), `display_by_name` (459), `display_geometry` (464),
  `monitor_device_names` (468), `monitor_dimensions` (472), `monitor_rect` (476),
  `pixel_dims` (481) — thin wrappers over `display.rs`.
- `set_display_resolution` (492), `set_display_mode` (507),
  `set_display_orientation` (525), `set_display_scale` (534) — forward to
  `virtual_display::reconfigure_display`; `set_display_scale` maps ≥150 % to the
  binary HiDPI flag.
- `set_display_topology_extend() -> bool` (544) — hardcoded `false`.

### `encoder.rs`
- `CompressedSample::to_annexb(&mut Vec<u8>) -> bool` (45) — returns whether the
  AU was a keyframe.
- `Encoder::new(cfg)` (68) — the spec ladder, session creation, `configure`,
  `prepare_to_encode_frames`.
- `Encoder::vt_session` (142) — reinterprets the compression session as a
  `VTSession`.
- `Encoder::configure(cfg)` (146) — every compression property (see §7).
- `Encoder::configure_rate_control(cfg)` (245) — QP mode (Quality +
  Max/MinAllowedFrameQP) or ABR (AverageBitRate + DataRateLimits +
  MaxAllowedFrameQP 45).
- `Encoder::submit(image_buffer, force_idr)` (268) — builds PTS/duration from a
  monotonically increasing `frame_index` at the `fps` timescale, optionally
  builds the ForceKeyFrame dictionary, calls `encode_frame`. Asynchronous.
- `Encoder::output()` (309) — clones the receiver.
- `Encoder::flush` (313) / `complete_pending` (316) — both
  `VTCompressionSessionCompleteFrames(kCMTimeInvalid)`, i.e. **blocking**.
- `Encoder::set_bitrate(bps)` (330) — no-op in QP mode; otherwise rewrites
  AverageBitRate and DataRateLimits.
- `Encoder::low_latency()` (348) — whether the low-latency spec was accepted.
- `Encoder::log_hw_acceleration()` (355) — reads
  `UsingHardwareAcceleratedVideoEncoder` and logs it. **Nothing consumes it.**
- `Encoder::drop` (373) — complete frames, invalidate, free the refcon.
- `output_callback(...)` (386) — VT's async output callback; retains the sample
  and sends it, or sends `sample: None` on a null sample / non-zero status.
- `sample_to_annexb_into(sample, out)` (413) — two passes over the AVCC buffer:
  first to detect a type-5 NAL, then to emit. Prepends the SPS/PPS parameter
  sets on keyframes; drops AUDs (type 9).
- `build_encoder_spec(low_latency, require_hw)` (499) — Require- vs
  Enable-HardwareAcceleratedVideoEncoder, plus the `EnableLowLatencyRateControl`
  literal.
- `create_session(cfg, spec, refcon)` (517) — `VTCompressionSession::create`.
- `cfbool` (553), `VTSessionCopyProperty`/`CFBooleanGetValue`/`CFRelease`
  externs (556), `vt_optional_cfstring(symbol)` (566 area) — `dlsym` of a
  `CFStringRef` constant.
- `set_optional` (566) / `_str` (576) / `_i32` (581) / `_f64` (586) /
  `_bool` (591) — set only if the symbol resolves; **return value never
  checked**.
- `dict_from_pairs` (595), `data_rate_limits(max_bytes, window_secs)` (617) —
  both `.expect(...)` on the CF constructor.
- `set_cftype` (635), `set_bool` (642), `set_i32` (646), `set_f64` (651).

### `sck.rs`
- `sck_optional_cfstring(symbol)` (117) — `dlsym` of an SCK `NSString *const`.
- `SckBackend::new` (155) / `frames_captured` (~196).
- `sck_class(name)` (~205) — `AnyClass::get`, mapped to
  `StreamCreateFailed`.
- `retain_new(obj)` (~215) — wrap a `+1` pointer.
- `delegate_class()` (~220) — `OnceLock`-cached `ClassBuilder` registration of
  `ScreenExtendSckOutput` with a `ctx` ivar, the
  `stream:didOutputSampleBuffer:ofType:` IMP and best-effort `SCStreamOutput`
  protocol conformance.
- `class_nsobject()` (266).
- `SckBackend::start` (272) — class checks, `resolve_display`, `SCContentFilter`,
  `SCStreamConfiguration` + `configure_stream`, attachment-key resolution,
  delegate + `SCStream`, the UserInteractive queue,
  `addStreamOutput:type:sampleHandlerQueue:error:`, then the bridged
  `startCaptureWithCompletionHandler:`.
- `SckBackend::stop` (~416) — bridged `stopCaptureWithCompletionHandler:` with a
  5 s wait, then releases the retained objects and frees the boxed context.
- `SckBackend::drop` (~449).
- `set_ctx_ivar` (~458) / `get_ctx_ivar` (~477).
- `resolve_display(cls_content, display_id)` (~486) — async→sync bridge, 5 s
  timeout; leaks a `+1` retain through a `usize` across the channel.
- `find_display(content, display_id)` (~532) — linear scan of
  `content.displays` comparing `displayID`.
- `configure_stream(config, cfg)` (552) — the seven property setters (see §7).
- `did_output_sample_buffer(...)` (583) — the hot-path IMP, `extern "C-unwind"`.
- `frame_is_complete_and_dirty(sample, ctx)` (~626) — reads the per-sample
  attachment dictionary; requires status == Complete; skips when the dirty-rects
  array is present and empty.
- `publish_sample(sample, ctx)` (678) — image buffer → IOSurface → a **fresh**
  CVPixelBuffer → `Frame` → `sink.publish`.
- `nserror_description(error)` (~727).
- `probe_screen_recording() -> Option<bool>` (761) — `AnyClass::get` on
  `SCShareableContent`, then a 5 s bridged `getShareableContent…`.

### `cgds.rs`
- `CgDisplayStreamBackend::new` (47) / `frames_captured` (64).
- `make_cg_properties(fps)` (69) — the five-key property dictionary.
- `handle_frame(...)` (102) — status gate, dirty-rect gate, IOSurface retain,
  CVPixelBuffer wrap, publish.
- `CgDisplayStreamBackend::start` (159) — build properties, create the
  UserInteractive serial queue, build the `RcBlock`,
  `CGDisplayStreamCreateWithDispatchQueue`, `CGDisplayStreamStart`.
- `CgDisplayStreamBackend::stop` (211) — `CGDisplayStreamStop` and drop.
- `probe_screen_recording() -> bool` (221) — a 64×64 stream, 700 ms wait for one
  frame.

### `frame.rs`
- `Frame::pixel_buffer()` (27), `frame_channel()` (48),
  `FrameSink::publish` (63), `FrameSource::next_blocking` (70) /
  `try_take_latest` (75) / `peek_has_frame` (80) / `wait(timeout)` (84) /
  `wake_sender` (88).

### `qos.rs`
- `pin_current_thread_user_interactive` (19), `pin_current_thread_user_initiated`
  (25), `pin_current_thread_time_constraint(period_ns)` (31),
  `raise_current_thread_precedence` (58),
  `pin_current_thread_encode_affinity` (73 / 92 no-op off x86_64),
  `dlsym(name)` (110), `FrameWorkgroup::join(_period_ns)` (128),
  `FrameWorkgroup::drop` (168).

### others
- `display.rs`: `active_displays` (15), `select_display` (26),
  `display_by_name` (37), `display_name` (42), `display_geometry` (46),
  `display_bounds` (54), `display_refresh_hz` (64), `device_names` (75),
  `dimensions_by_name` (79).
- `config.rs`: `scaled_dims` (15), `live_encoder_config` (24).
- `power.rs`: `KeepAwake::begin` (34), `KeepAwake::drop` (65),
  `cfstr_ptr` (75).
- `tuning.rs`: `apply_process_tuning` (7), `ProcessTuning::begin` (20).
- `activity.rs`: `begin_latency_critical_activity` (11).
- `mach.rs`: `timebase` (4), `mach_now` (14), `mach_age_ms` (19),
  `mach_delta_ns` (26), `ns_to_mach_ticks` (32).
- `gpu.rs`: `Gpu::new` (12).

---

## 3. Threads

Per streaming session (one client):

| Thread | Spawned at | QoS / policy | Lifetime | Owns |
|---|---|---|---|---|
| `videotoolbox-encode` | `pipeline.rs:106-119` | `pipeline.rs:133-136`: QOS USER_INTERACTIVE, then TIME_CONSTRAINT (period = frame duration, computation = period/8, constraint ≤4 ms, preemptible), then PRECEDENCE importance 63, then AFFINITY tag 1 (x86_64). Also joins `FrameWorkgroup` (137) and takes `KeepAwake` (138). | until `stop` is set, the submit fails, or the frame channel closes | the `Encoder`, `last_frame`, the two IOPMAssertions, the workgroup token |
| `videotoolbox-drain` | `pipeline.rs:143-167` | `pipeline.rs:147-149`: QOS USER_INTERACTIVE, PRECEDENCE 63, AFFINITY tag 1. **No** time constraint. | until the encoder's output channel closes (i.e. the `Encoder` is dropped) | the AnnexB conversion, the broadcast sender |
| CGDisplayStream delivery | `cgds.rs:162-165` — a serial `DispatchQueue` named `com.screenextend.cgds` targeting the global `UserInteractive` queue | dispatch QoS USER_INTERACTIVE | the stream's lifetime | wraps and publishes each IOSurface |
| SCK delivery | `sck.rs:347-350` (approx) — a serial `DispatchQueue` named `com.screenextend.sck`, same target | dispatch QoS USER_INTERACTIVE | the stream's lifetime | the same |
| VideoToolbox output | VT-internal | Apple's | the session's | invokes `output_callback` |

Transient: the TCC probe queue `com.screenextend.tcc-probe` (`cgds.rs:233`), and
the SCK completion blocks, which run on SCK's own queues and post to `mpsc`
channels that the *calling* thread (a tokio worker — see §11) blocks on.

Process-wide: `tuning::apply_process_tuning` takes one `NSProcessInfo` activity
in a `OnceLock` (`tuning.rs:7-10`), called from `streamer/platform.rs:80`.
`tune_transport_thread` pins each tokio worker to USER_INITIATED
(`platform.rs:84-89`).

Note there is **no display-link pacing anywhere**. The pipeline is entirely
event-driven off the capture callback, with a 4 ms poll fallback.

---

## 4. Locks

- **`pending_ts: Arc<Mutex<VecDeque<Instant>>>`** (`pipeline.rs:140`). Guards
  the capture-timestamp queue that reunites an encoded AU with the frame it came
  from. Taken by the encode thread (`pipeline.rs:233`, `:236`) and by the drain
  thread (`pipeline.rs:151`). **Hot path — once per frame on each side.** Never
  crosses an `.await` (neither thread is async), but it is a plain
  `std::sync::Mutex` between a TIME_CONSTRAINT thread and a USER_INTERACTIVE
  thread, i.e. an unprotected priority inversion. All three call sites
  `.unwrap()`. → `major-vt-pending-ts-mutex-priority-inversion.md`
- **`FrameSink::latest: ArcSwapOption<Frame>`** (`frame.rs:39`) — lock-free.
  Written by the capture callback (`publish`), swapped out by the encode loop.
  This is the correct shape.
- **Atomics**: `frames: Arc<AtomicU64>` in both backends (Relaxed, counter only);
  `idr_request: AtomicBool` (Relaxed, `swap`); `target_bitrate: AtomicU32`
  (Relaxed, `swap`, `0` as sentinel); `stop: AtomicBool` (Relaxed).
- **`OnceLock`s**: `mach::timebase` (`mach.rs:5`), `sck::delegate_class`'s `CLS`
  (`sck.rs:~221`), `tuning::TOKEN` (`tuning.rs:8`, whose `set` result is
  discarded).
- **Guard-across-`.await`**: none in this territory — nothing here is `async`.
  The `if let` scrutinee temporary-lifetime pattern does appear
  (`pipeline.rs:189`, `:204`, `sck.rs` attachment reads) but never with an
  `.await` inside, so it is currently benign. It is worth noting that
  `pipeline.rs:203-217`'s `if let Some(frame) = last_frame.as_ref()` holds an
  `Arc<Frame>` borrow across the *entire* submit + `complete_pending()` block —
  which is the mechanism by which a second IOSurface stays pinned (§5).
- **Cross-territory**: `start_on_monitor` is called from inside an async fn on a
  tokio worker (`server.rs:1038`) and blocks on `mpsc::recv_timeout`. No mutex is
  held, but the worker is.

---

## 5. Buffers and copies

The video path is genuinely zero-copy for pixels. The costs are object
allocations and surface pinning, not memcpys.

| Stage | Classification |
|---|---|
| Display → IOSurface | Done by WindowServer/SCK. The requested `width`/`height` are the **already-scaled** encode dimensions (`pipeline.rs:253-258`, `:277-282`), so the compositor also does the downscale and the RGB→`'420f'` conversion. GPU-side, outside our process. |
| IOSurface → `CVPixelBuffer` | **Zero-copy wrap, one CF object allocated per frame.** `cgds.rs:136`, `sck.rs:706` — `CVPixelBufferCreateWithIOSurface`. SCK additionally already *has* a `CVImageBuffer` from the sample (`sck.rs:681`) and deliberately re-wraps (`sck.rs:697-702`). |
| `Frame` → `FrameSink` | **One `Arc::new` allocation per frame** inside the capture callback (`frame.rs:64`). Ownership moves into the single latest-wins slot; the previous `Frame` is dropped there, releasing its IOSurface. |
| `FrameSink` → encode loop | Zero-copy `ArcSwapOption::swap`. The taken `Arc<Frame>` is then **held in `last_frame`** (`pipeline.rs:202`) until the next frame replaces it — so **two IOSurfaces are pinned in steady state**. |
| Encode-loop → VideoToolbox | Zero-copy: the `CVImageBuffer` reference is passed straight to `encode_frame` (`encoder.rs:~294`). |
| VT → `CompressedSample` | Zero-copy: `CFRetained::retain` of the output `CMSampleBuffer` (`encoder.rs:408`). |
| `CompressedSample` → Annex B | **CPU memcpy of the encoded AU** — `sample_to_annexb_into` walks the AVCC buffer and `extend_from_slice`s each NAL plus a 4-byte start code (`encoder.rs:~455-495`), preceded by the SPS/PPS on keyframes. Two passes over the data. |
| Annex B → `Bytes` | **One `Vec<u8>` allocated per encoded frame** (`pipeline.rs:156`, capacity 0 so `out.reserve` allocates exactly once), then `Bytes::from(au)` (`pipeline.rs:162`) which is a free move. |
| `Bytes` → broadcast | Clone per subscriber, refcount only. |

There is **no GPU→CPU readback and no Metal stage** — `gpu.rs` is empty, so
nothing in this territory ever maps a surface for CPU access outside the two
synthetic-frame test helpers (`pipeline.rs:411`, `encoder.rs:~668`).

Per-frame allocations, summarised: 1 CF object (capture callback), 1 `Arc`
(capture callback), 1 `Vec<u8>` (drain thread). →
`minor-vt-per-frame-allocations-in-hot-path.md`

---

## 6. Queues

| Queue | Where | Depth | Bounded | Blocking | Back-pressure |
|---|---|---|---|---|---|
| CGDisplayStream surface queue | `cgds.rs:71` `kCGDisplayStreamQueueDepth = 2` | 2 | yes, by the OS | the stream stalls when all surfaces are retained | **the pipeline pins 2 of the 2** → `blocker-cgds-queue-depth-starvation.md` |
| SCK surface queue | `sck.rs:566` `setQueueDepth: 3` | 3 | yes | same semantics | 2 of 3 pinned |
| Frame slot | `frame.rs:39` `ArcSwapOption` | **1** | yes | never blocks | latest-wins: an unread frame is dropped. Correct for latency. |
| Frame wake | `frame.rs:49` `bounded(1)` | 1 | yes | `try_send`, dropped when full | coalescing. Also carries `request_idr` / `set_target_bitrate` pokes (`pipeline.rs:44-52`) — one slot for three producers. |
| VT output | `encoder.rs:71` `crossbeam::unbounded` | **∞** | **no** | never blocks | **none** → `major-vt-unbounded-encoder-output-channel.md` |
| `pending_ts` | `pipeline.rs:140` `VecDeque` | ∞ | no | mutex | none; relies on a strict 1:1 submit/emit |
| Encoded broadcast | `pipeline.rs:27`/`:86` `broadcast::channel(4)` | 4 | yes | drops oldest, signals `Lagged` | the consumer's response is `request_idr` (`webrtc_session.rs:550-553`) → `speculative-vt-broadcast-lag-to-idr-feedback-loop.md` |
| SCK/TCC async bridges | `sck.rs:519`, `:379`, `cgds.rs:265` | 1 | yes | `recv_timeout` 5 s / 5 s / 700 ms | blocks the caller |

---

## 7. Syscall / FFI boundaries

### CGDisplayStream (`cgds.rs`) — every property actually set
`make_cg_properties` (`cgds.rs:69-100`) builds a 5-key CFDictionary:

| Key | Value | Line |
|---|---|---|
| `kCGDisplayStreamMinimumFrameTime` | `(1.0 / fps) * 0.95` seconds | 70 |
| `kCGDisplayStreamQueueDepth` | `2` | 71 |
| `kCGDisplayStreamShowCursor` | `true` | 72 |
| `kCGDisplayStreamPreserveAspectRatio` | `false` | 73 |
| `kCGDisplayStreamYCbCrMatrix` | `ITU_R_709_2` | 81/88 |

**Not set:** `kCGDisplayStreamColorSpace`, `kCGDisplayStreamSourceRect`,
`kCGDisplayStreamDestinationRect`. Output pixel format is the `cfg.pixel_format`
argument to `CGDisplayStreamCreateWithDispatchQueue` (`cgds.rs:184`), which
production always sets to `PIXEL_FORMAT_420F` (`pipeline.rs:257`, `:281`, `:366`)
— `'420f'`, i.e. **full-range** biplanar YCbCr. The absence of SourceRect /
DestinationRect means the stream scales the whole display into the requested
width×height, in WindowServer.

Queue: a serial `DispatchQueue` targeting the global UserInteractive queue
(`cgds.rs:162-165`).

Handler, per frame (`handle_frame`, `cgds.rs:102-156`):
1. `status != FrameComplete` → return (114). Note `FrameBlank`, `FrameIdle`
   and `Stopped` are all treated identically and silently.
2. `CGDisplayStreamUpdate::rects(update, DirtyRects, &mut count)`; `count == 0` →
   return (118-127). `RefreshedRects` and `MovedRects` are never queried and the
   returned rect array is never used for anything else. →
   `major-cgds-dirty-rect-skip-can-blank-a-new-client.md`
3. `CFRetained::retain(surface)` (134).
4. `CVPixelBufferCreateWithIOSurface` (136) — the zero-copy wrap.
5. `frames.fetch_add(1, Relaxed)`, `sink.publish(Frame { … })` (144-154).

**No colour conversion happens on the CPU.** The `displayTime` block parameter is
bound as `_display_time` and discarded (`cgds.rs:172`). →
`major-cgds-capture-timestamp-is-callback-arrival-not-display-time.md`

### ScreenCaptureKit (`sck.rs`) — every property actually set
`configure_stream` (`sck.rs:552-571`), all via `msg_send!`:

| Selector | Value |
|---|---|
| `setWidth:` / `setHeight:` | the scaled encode dimensions (isize) |
| `setMinimumFrameInterval:` | `CMTime { value: 1, timescale: fps, Valid }` |
| `setPixelFormat:` | `cfg.pixel_format` (`'420f'`) |
| `setQueueDepth:` | `3` |
| `setShowsCursor:` | `true` |
| `setCapturesAudio:` | `false` |

**Not set:** `colorMatrix`, `colorSpaceName`, `backgroundColor`, `scalesToFit`,
`preservesAspectRatio`, `sourceRect`, `destinationRect`, `captureResolution`
(14.0), `streamName`. The colour divergence from cgds is
`minor-sck-color-config-diverges-from-cgds.md`.

Delegate queue: a serial `DispatchQueue` targeting the global UserInteractive
queue, attached with `addStreamOutput:type:sampleHandlerQueue:error:`.

**Dyld-safety mechanism** — this is the part the file gets right, and it is
load-bearing:
- classes via `AnyClass::get` (`sck_class`, `sck.rs:~205`; also re-checked
  defensively in `start` at `sck.rs:273-276`);
- messages via `msg_send!`, never a typed `extern_class!`;
- the delegate class built at runtime with `ClassBuilder`, so the
  `SCStreamOutput` protocol type is never referenced (protocol conformance is
  added only if `AnyProtocol::get(c"SCStreamOutput")` succeeds);
- `SCStreamFrameInfoStatus` / `SCStreamFrameInfoDirtyRects` resolved with
  `libc::dlsym(RTLD_DEFAULT, …)` (`sck_optional_cfstring`, `sck.rs:117-128`),
  never as `extern static`s;
- the enum values that would otherwise be 12.3+ symbols are inlined as
  constants (`SC_FRAME_STATUS_COMPLETE = 0`, `SC_OUTPUT_TYPE_SCREEN = 0`).
- `CFDictionaryGetValue` / `CFNumberGetValue` are hand-declared (`sck.rs:~750`)
  because they are floor-present.

Verified by reading: there is no `objc2-screen-capture-kit` import and no
`#[link]` of a post-10.15 framework in this module.

### VideoToolbox (`encoder.rs`) — every property, and how it is set

Encoder specification (`build_encoder_spec`, `encoder.rs:499-512`):
- `kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder` = true,
  **or** `…_EnableHardwareAcceleratedVideoEncoder` = true. Both are linked
  `extern static`s — both are 10.9-era, so dyld-safe.
- `"EnableLowLatencyRateControl"` = true — built as a **string literal**
  (`encoder.rs:505`), not the 11.0 `extern static`. Correct.

Compression properties (`configure`, `encoder.rs:146-243`):

| Property | Value | How set | Version |
|---|---|---|---|
| `RealTime` | `true` | linked static, status logged if rejected | 10.9 |
| `AllowFrameReordering` | `false` | linked static | 10.9 |
| `ProfileLevel` | High / Main / Baseline `_AutoLevel`, preferring the `Constrained*` variants via `vt_optional_cfstring` when the low-latency spec was taken | linked static + `dlsym` fallback | 10.9 |
| `ColorPrimaries` | `"ITU_R_709_2"` | **`dlsym`** (`set_optional_str`) | — |
| `TransferFunction` | `"ITU_R_709_2"` | **`dlsym`** | — |
| `YCbCrMatrix` | `"ITU_R_709_2"` | **`dlsym`** | — |
| `ExpectedFrameRate` | `cfg.fps` | linked static | 10.9 |
| `MaxKeyFrameInterval` | `fps * 60` | linked static | 10.9 |
| `MaxKeyFrameIntervalDuration` | `10.0` s | linked static | 10.9 |
| `MaxFrameDelayCount` | `1` | linked static, status logged if rejected | 10.9 |
| `MaximizePowerEfficiency` | `false` | **`dlsym`** | 10.14 |
| `PrioritizeEncodingSpeedOverQuality` | `true` | **`dlsym`** | 11.0 |
| `Quality` / `MaxAllowedFrameQP` / `MinAllowedFrameQP` | QP mode only | **`dlsym`** | 11.0+ |
| `AverageBitRate` | `cfg.bitrate_bps` | linked static | 10.9 |
| `DataRateLimits` | `[bitrate*1.5/8 bytes, 0.5 s]` | linked static | 10.9 |
| `MaxAllowedFrameQP` | `45` (ABR mode) | **`dlsym`** | 11.0 |

**Not set:** `BaseLayerFrameRate` (no temporal SVC), `ConstantBitRate`,
`AllowOpenGOP`, `PixelTransferProperties`, `H264EntropyMode` (so CABAC/CAVLC is
whatever VT picks for the profile), `MaxH264SliceBytes`,
`MoreFramesBeforeStart`/`AfterEnd`, `EncoderID`.

**Linked symbols newer than 10.15: none found.** Every post-10.15 key
(`PrioritizeEncodingSpeedOverQuality`, `EnableLowLatencyRateControl`, the QP
keys) is either `dlsym`'d or a string literal, exactly as CLAUDE.md requires.
`kVTCompressionPropertyKey_MaximizePowerEfficiency` (10.14) is also `dlsym`'d,
which is belt-and-braces.

Session shape: **asynchronous**. `create_session` installs `output_callback`
(`encoder.rs:531-551`); `submit` returns as soon as `encode_frame` accepts the
frame (`encoder.rs:~294-306`). The callback retains the `CMSampleBuffer` and
pushes it to the unbounded channel — no copy, no parse, on VT's own thread.

`kVTEncodeFrameOptionKey_ForceKeyFrame` path: `submit(image_buffer, force_idr)`
builds a fresh single-entry CFDictionary **per forced frame**
(`encoder.rs:283-289`, via `dict_from_pairs`, which `.expect`s) and passes it as
`frameProperties`. `force_idr` originates from `Pipeline::request_idr`, which is
called on PLI/FIR, on payloader failure, on broadcast lag, and once at
negotiation.

### Other FFI
- CoreGraphics: `CGGetActiveDisplayList`, `CGMainDisplayID`,
  `CGDisplayPixelsWide/High`, `CGDisplayCopyDisplayMode`,
  `CGDisplayMode::refresh_rate`, and a hand-declared `CGDisplayBounds`
  (`display.rs:8-11`).
- IOKit: `IOPMAssertionCreateWithName` / `IOPMAssertionRelease`, declared
  by hand (`power.rs:9-18`). Return codes: `Create` **is** checked
  (`power.rs:48`); `Release` is not (`power.rs:68`).
- Mach: `mach_absolute_time`, `mach_timebase_info`, `thread_policy_set`,
  `mach_thread_self` (via `mach2`). All `thread_policy_set` results discarded.
- pthread: `pthread_set_qos_class_self_np` hand-declared (`qos.rs:16`), result
  discarded.
- libSystem via `dlsym`: `os_workgroup_interval_create`, `os_workgroup_join`,
  `os_workgroup_leave`, `os_release` (`qos.rs:131-135`);
  `CGRequestScreenCaptureAccess` (`mod.rs:90-96`).
- Foundation: `NSProcessInfo.beginActivityWithOptions:reason:` in both
  `tuning.rs:26` and `activity.rs:15`.
- **Subprocess**: `sw_vers -productVersion` (`mod.rs:63-66`) — see
  `major-compat-mac-capabilities-gated-by-a-sw-vers-subprocess.md` and
  `minor-vt-low-latency-version-gate-is-redundant-with-the-spec-ladder.md`.
- `virtual_display::reconfigure_display` (called from `pipeline.rs:492`, `:522`,
  `:531`, `:542`) — another territory's boundary.

Versioning structs: none in this module. `thread_time_constraint_policy_data_t`
and friends come from `mach2` and are fixed-layout. The `JoinToken([u8; 64])`
(`qos.rs:103`) is a hand-sized opaque buffer for `os_workgroup_join_token_s`;
64 bytes is larger than the real struct, so it is safe, but the size is a guess
with no static assertion.

---

## 8. Panics

Full inventory in `minor-vt-panics-reachable-at-runtime.md`. Summary of the ones
a runtime condition can reach:

- `cgds.rs:98` `.expect("CFDictionaryCreate for CGDisplayStream properties")` —
  every capture start and every TCC probe.
- `encoder.rs:613` `.expect("CFDictionaryCreate")` in `dict_from_pairs` — used
  for the encoder spec **and for the per-frame ForceKeyFrame dictionary**.
- `encoder.rs:631` `.expect("CFArrayCreate for DataRateLimits")` — used by
  `configure_rate_control` and by **`set_bitrate`**, i.e. on every update from
  the loss-based bitrate driver.
- `pipeline.rs:167` `.expect("spawn encode-drain thread")`.
- `pipeline.rs:151`, `:233`, `:236` — `pending_ts.lock().unwrap()`; a panic in
  one thread poisons the other.
- `sck.rs:228` `.expect("ClassBuilder for SCK output delegate")`,
  `sck.rs:266` `.expect("NSObject is always present")`,
  `sck.rs:464` `.expect("SCK delegate must have the ctx ivar")`,
  `sck.rs:522` and `sck.rs:545` `Retained::…().unwrap()`.
- `mod.rs:90` `CString::new(…).unwrap()`.
- Test-only: `partial_cmp(…).unwrap()` in the latency sorts
  (`cgds.rs:407`, `pipeline.rs:~672`, `:~715`).

`sck.rs:583`'s IMP is declared `extern "C-unwind"` specifically so a panic
crossing the ObjC frame is not UB — evidence the authors already knew panics
were reachable on the hot path.

---

## 9. Silent fallbacks and swallowed errors

1. **`set_optional*` returns a `bool` no caller reads** (`encoder.rs:566-593`).
   Ten properties can vanish without a word. →
   `minor-vt-set-optional-results-discarded.md`
2. **`set_cftype` logs the OSStatus but not the key** (`encoder.rs:635-640`), so
   a rejected property is un-diagnosable.
3. **The encoder-spec ladder degrades silently to a hinted (possibly software)
   encoder.** Only the third rung logs (`encoder.rs:105-114`). →
   `major-vt-silent-software-encoder.md`
4. **`UsingHardwareAcceleratedVideoEncoder` is read and only printed**
   (`encoder.rs:355-370`).
5. **Every `thread_policy_set` and `pthread_set_qos_class_self_np` result is
   `let _ =`** (`qos.rs:21`, `:27`, `:47`, `:63`, `:82`). →
   `major-vt-qos-thread-policy-returns-ignored.md`
6. **`IOPMAssertionRelease` result discarded** (`power.rs:68`).
   `KeepAwake::begin` at least logs when *all* assertions fail
   (`power.rs:52-59`) — but a partial failure (1 of 2) is reported as success.
7. **`tuning.rs:9` `let _ = TOKEN.set(...)`** — a second call silently does
   nothing, and the "process-wide App-Nap … applied" line (`tuning.rs:27`) is
   printed *before* the `setpriority` attempt that follows it. →
   `minor-vt-setpriority-fails-for-unprivileged-users.md`
8. **Both capture backends return silently on a failed CVPixelBuffer wrap**
   (`cgds.rs:137-142`, `sck.rs:704-713`) and leak on one branch. →
   `minor-cgds-pixelbuffer-leaked-on-nonzero-cvreturn.md`
9. **Non-`FrameComplete` statuses are dropped with no counter**
   (`cgds.rs:114`): `FrameBlank`, `FrameIdle` and `Stopped` are
   indistinguishable from an idle screen.
10. **Empty dirty-rect frames dropped with no counter** (`cgds.rs:124`,
    `sck.rs:~668`). → `major-cgds-dirty-rect-skip-can-blank-a-new-client.md`
11. **`macos_at_least` fails open** (`mod.rs:75`, `None => true`) — an
    unreadable version claims the newest OS.
12. **`ensure_screen_recording_access` treats a missing
    `CGRequestScreenCaptureAccess` as "granted"** (`mod.rs:92-93`).
13. **`set_display_orientation` returns `Ok(())` without acting** when the
    display is already in the requested orientation (`pipeline.rs:527-530`) —
    correct, but indistinguishable from a real change by the caller.
14. **`pipeline::start` `mem::forget`s the backend and the activity** and drops
    the stop flag and join handle (`pipeline.rs:260-264`). →
    `minor-vt-pipeline-start-leaks-backend-and-thread.md`
15. **A pending bitrate target is lost if `set_bitrate` fails**: it was already
    `swap`ped to 0 (`pipeline.rs:~183`) before the call, so the error is logged
    and the target never retried.
16. **The encode loop `break`s permanently on one submit failure** and nothing
    downstream notices. → `major-vt-encode-loop-exits-silently-on-first-submit-error.md`
17. `sck.rs` swallows the `NSError` out-parameter from
    `addStreamOutput:…error:` — `add_err` is written but never read.
18. `probe_screen_recording` in `sck.rs:761` returns `None` on timeout, which
    `mod.rs:109-112` then treats as "fall through to the cgds probe" — a
    reasonable design, but the SCK timeout itself is never logged.

---

## 10. TODO / FIXME / HACK / XXX

No `TODO`, `FIXME`, `HACK` or `XXX` markers exist in this territory. What does
exist is a set of references to documents and milestones that are not in the
tree (see §12):

```
gpu.rs:6    // M1: device: Retained<ProtocolObject<dyn MTLDevice>>,
gpu.rs:7    // M1: tex_cache: CFRetained<CVMetalTextureCache>,
mod.rs:37   #[error("{0} not yet implemented (M1/M2): see PRD {1}")]
sck.rs:1    //! ScreenCaptureKit backend (primary, macOS 12.3+) — PRD §5.
sck.rs:3    //! ## Why this file links and loads on macOS 10.15 (resolves TODO 5.1 by construction)
sck.rs:24   //! historical "weak-link / dlopen ScreenCaptureKit" build task (TODO 5.1) moot:
sck.rs:34   //! ## The flow (PRD §5)
```
plus §-tagged comments at `sck.rs:35, 39, 41, 46, 48, 50, 53, 55, 282, 285, 302,
310, 335, 347, 371, 486, 554, 580, 617, 622, 628, 662`.

---

## 11. Latency path

Every file except `display.rs`, `config.rs` and the probe halves of
`pipeline.rs` sits on the glass-to-glass path. Stage by stage, with the
serialization points marked **S**:

| # | Stage | Files | Cost (estimated; unmeasured on this hardware) |
|---|---|---|---|
| 1 | Composite + scale + RGB→`'420f'` | WindowServer / SCK, driven by `cgds.rs:179-189` / `sck.rs:552-571` | Outside our process. Scales with resolution. `speculative-cgds-windowserver-does-the-downscale-with-no-source-rect.md` |
| **S1** | **Surface-queue starvation** | `cgds.rs:71` (depth 2) vs. 2 pinned surfaces | Potentially a full frame period or worse, every frame. **The most likely single cause of the reported symptom.** `blocker-cgds-queue-depth-starvation.md` |
| 2 | Delivery to our dispatch queue | serial UserInteractive queue | Unmeasured, and **unmeasurable today** because `displayTime` is discarded. `major-cgds-capture-timestamp-is-callback-arrival-not-display-time.md` |
| 3 | Callback work | `cgds.rs:102-156` / `sck.rs:583-723` | 1 CF alloc + 1 `Arc` alloc; the dirty-rect query. Sub-ms unless malloc contends. |
| **S2** | **Frame slot + wake** | `frame.rs:63-66`, `pipeline.rs:179` | 0 when the encode thread is running; up to 4 ms when the wake token was consumed out of order. `speculative-vt-encode-loop-4ms-idle-and-wake-token-race.md` |
| **S3** | **`pending_ts` mutex** | `pipeline.rs:151`, `:233` | 0 uncontended; unbounded under priority inversion. `major-vt-pending-ts-mutex-priority-inversion.md` |
| 4 | `VTCompressionSessionEncodeFrame` | `encoder.rs:268-307` | Hardware: ~2-6 ms at 1080p. **Software: 15-40 ms and CPU-bound.** `major-vt-silent-software-encoder.md` |
| **S4** | **`complete_pending()` on the keepalive path** | `pipeline.rs:209-215` | A blocking encoder flush on the real-time thread, on every pre-11.0 host. `major-vt-complete-frames-blocks-the-realtime-thread.md` |
| **S5** | **Unbounded VT output queue** | `encoder.rs:71` | 0 when drained promptly; grows without limit when not. `major-vt-unbounded-encoder-output-channel.md` |
| 5 | AVCC→Annex B | `encoder.rs:413-497` | Two passes + one alloc over ~10-100 KB. Sub-ms. |
| **S6** | **Broadcast(4)** | `pipeline.rs:86` | 66 ms of slack at 60 fps; overflow triggers an IDR. `speculative-vt-broadcast-lag-to-idr-feedback-loop.md` |
| 6 | RTP payload + `write_rtp` | `webrtc_session.rs:502-540` (other territory) | one `.await` per ~1188-byte packet |

Additional, off the steady-state path but on the **join** path:
`start_on_monitor` runs 3-5 `sw_vers` forks, a 700 ms TCC probe, and two 5 s
SCK bridges **synchronously on a tokio worker**
(`major-session-start-on-monitor-blocks-a-tokio-worker.md`).

A structural note on the fps floor: `live_encoder_config`
(`config.rs:30-36`) raises any refresh below 60 to 60, and `cap_fps` then takes
`max(enc.fps, refresh)` (`pipeline.rs:276`). Every stage above therefore runs at
≥60 Hz even when the client asked for 30. →
`major-vt-encoder-fps-floored-at-60.md`

**No display-link pacing** is used anywhere; the encode loop is purely reactive
to the capture callback with a 4 ms poll floor. Given the latest-wins frame slot
this is the right shape, and adding pacing would only help if S1 is fixed first.

---

## 12. Stale references

- **`PRD §…`** — 23 references in `sck.rs` plus one embedded in a user-facing
  error string at `mod.rs:37`. **No PRD exists in the tree.** The only design
  document present is `docs/airplay-fallback-design.md`.
- **`M1` / `M1/M2`** — `gpu.rs:6`, `gpu.rs:7`, `mod.rs:37`. A milestone plan that
  is not in the repo.
- **`TODO 5.1`** — `sck.rs:3`, `sck.rs:24`. A task tracker that is not in the
  repo (the comment is otherwise excellent and self-contained).
- **`CaptureError::NotImplemented(&str, &str)`** (`mod.rs:37`) — the variant is
  never constructed, so the stale PRD reference is in dead code.
- **`gpu.rs`'s commented-out fields** describe a Metal path that does not exist;
  CLAUDE.md's architecture section states that `gpu.rs` provides a "Metal device
  + `CVMetalTextureCache`", which the code contradicts. (Already filed by another
  territory as `minor-vt-gpu-struct-is-an-empty-stub-claude-md-says-otherwise.md`.)
- **`cgds.rs:199`'s log line hardcodes "(queueDepth=2)"** and `sck.rs:~390`'s
  hardcodes "(queueDepth=3)" — they will not track a change to the constants.
- No AI-tooling references were found in this territory.

Filed as `minor-docs-stale-prd-and-milestone-references.md`.

---

## 13. Dead code candidates

- **`gpu.rs` entirely.** `Gpu` carries no state; the `Arc<Gpu>` parameter is
  `_gpu` in `cgds.rs:49` and `#[allow(dead_code)] gpu` in `sck.rs:148-150`.
  Nothing reads it. Consequently `CaptureError::NoMetalDevice` and
  `CaptureError::TextureCache(i32)` (`mod.rs:23-26`) are unconstructible.
- **`CaptureError::NotImplemented`** (`mod.rs:37`) and
  **`CaptureError::PermissionDenied`** (`mod.rs:35`) — grep finds no
  construction site for either.
- **`FrameSource::next_blocking`** (`frame.rs:70`) and
  **`FrameSource::peek_has_frame`** (`frame.rs:80`) — used only by
  `frame.rs`'s own tests. The production loop uses `wait` + `try_take_latest`.
- **`pipeline::start`** (`pipeline.rs:245`) — re-exported through
  `streamer/pipeline.rs:4`/`:21` but no production caller; only
  `start_on_monitor` is used (`server.rs:1038`).
- **`pipeline::set_display_resolution`** (`pipeline.rs:492`) — superseded by
  `set_display_mode`; check `server.rs` before removing.
- **`pipeline::set_display_topology_extend`** (`pipeline.rs:544`) — returns a
  hardcoded `false`.
- **`qos::pin_current_thread_time_constraint`,
  `raise_current_thread_precedence`, `pin_current_thread_encode_affinity`,
  `FrameWorkgroup`** — *called*, but plausibly inert (see
  `major-vt-qos-thread-policy-returns-ignored.md` and
  `major-vt-workgroup-created-but-never-driven.md`). Not dead by grep; possibly
  dead in effect. Needs measurement before removal.
- **`display::display_name`** (`display.rs:42`) — `id.to_string()`; used, but
  the abstraction adds nothing.
- **`mach::mach_delta_ns`** (`mach.rs:26`) — no non-test caller found. Would
  become live if the `displayTime` fix in
  `major-cgds-capture-timestamp-is-callback-arrival-not-display-time.md` lands.
- **`Backing::Empty`** (`frame.rs:10`) — only constructed in tests.
- **`config::scaled_dims`'s native branch** — reachable, listed only because it
  silently truncates an odd dimension by one pixel.

`mod.rs:1`'s `#![allow(dead_code)]` suppresses the compiler warnings that would
otherwise flag most of this.

---

## 14. Capability probing

Where behaviour is selected by version number rather than by probing — each one
a defect under the tier doctrine ("Probe by class lookup, never by version
number", CLAUDE.md):

1. **`screencapturekit_available()` → `macos_at_least(12, 3)`** (`mod.rs:80-82`),
   which gates `start_capture`'s choice of backend (`mod.rs:122`) and
   `probe_screen_recording`'s choice of probe (`mod.rs:108`). The correct probe
   is one line away and already written twice in the same crate:
   `sck.rs:761`'s `AnyClass::get(c"SCShareableContent")` and `SckBackend::start`'s
   own re-check at `sck.rs:273-276`.
2. **`macos_at_least(11, 0)`** (`encoder.rs:73`) gating the low-latency encoder
   spec — redundant, because the spec ladder already falls through on rejection.
   → `minor-vt-low-latency-version-gate-is-redundant-with-the-spec-ladder.md`
3. **`macos_at_least` itself fails open** (`mod.rs:75`): an unreadable version
   claims the newest OS, so a host that cannot spawn a subprocess is tiered to
   ScreenCaptureKit and Process Tap.
   (Filed by another territory as
   `major-compat-mac-capabilities-gated-by-a-sw-vers-subprocess.md`.)

Where a tier is **missing**:

4. **No hardware/software encoder tier.** The encoder ladder degrades to
   software with no tier identity, no UI report, and no adjustment to
   resolution or fps — unlike the audio and display subsystems, which both
   report their chosen tier through `CompatibilityReport`. →
   `major-vt-silent-software-encoder.md`
5. **No capture-backend tier report.** `CompatibilityReport` carries
   `audio_backend` and `display_backend` but nothing for *capture*: a host that
   fell back from SCK to CGDisplayStream looks identical in the UI to one that
   is on SCK, even though the two have different colour behaviour, different
   queue depths and different dirty-rect semantics.
6. **No probe for `os_workgroup_interval_*`'s usefulness** — `FrameWorkgroup`
   probes for the *symbols* (correct) but never verifies the join did anything.
7. **CGDisplayStream is itself deprecated and removed on recent macOS.** There is
   no third tier below it, so a future macOS that drops CGDisplayStream while
   something breaks SCK leaves `start_capture` with nothing. Worth noting as a
   compatibility floor/ceiling question, not filed.

Done correctly, and worth preserving: `sck.rs`'s entire runtime-interop
construction; `vt_optional_cfstring`'s `dlsym` of every post-10.15 VideoToolbox
key; `qos.rs`'s `dlsym` of the workgroup API; `mod.rs`'s `dlsym` of
`CGRequestScreenCaptureAccess`. `nm -u` on the built binary should show no
post-10.15 undefined symbols from this module.

---

## 15. Questions and cross-cutting concerns

1. **Is the encoder hardware or software on the affected Mac?** This is the
   single measurement that would most change the diagnosis, and the log line
   already exists (`encoder.rs:365`). Everything else is secondary if the answer
   is "software".
2. **Does CGDisplayStream actually stall at queueDepth 2 with two surfaces
   pinned?** I believe it does, from the API contract, but this must be measured
   on the Mac. If it does, `blocker-cgds-queue-depth-starvation.md` is the fix
   with the largest expected effect and the smallest diff.
3. **What scheduling policy do the encode and drain threads actually end up
   with?** Four policy calls are stacked and every return is discarded
   (`qos.rs`). This needs a `thread_policy_get` readback before any further QoS
   work is attempted.
4. **The 60 fps floor is cross-platform.** `windows_utils/streamer/pipeline.rs:99`
   has the identical clamp and
   `windows_utils/streamer/test/pipeline.rs:62` asserts it. Whoever owns the
   Windows pipeline should be consulted before changing `config.rs:30-36`.
5. **Timestamp semantics are cross-territory.** `EncodedFrame.capture` feeds
   `host_ns_to_rtp90k` (`webrtc_session.rs:512`), which the client inverts for
   A/V sync and display-lag telemetry. Audio uses a genuine capture instant while
   video uses a callback-arrival instant, so the two are not on the same footing
   even though they share the epoch. Coordinate the fix with the audio and client
   territories.
6. **The `sourceFrameRefCon` slot is free** (`encoder.rs:299` passes null). Using
   it would delete the `pending_ts` mutex, the VecDeque, and the 1:1 submit/emit
   assumption in one change. This is the same opportunity on Windows if the
   Windows encoders expose an equivalent.
7. **Nobody reports the capture backend to the UI.** Adding
   `CompatibilityReport.capture_backend` (SCK / CGDisplayStream, hardware /
   software encoder) would make several of these findings self-diagnosing from a
   user's log.
8. **`pipeline::start` vs `start_on_monitor`.** `start` looks like an orphaned
   probe entry point with `mem::forget`s papering over its lifetime. Confirm with
   whoever owns `streamer/cli.rs` before deleting.
9. **Colour range.** `'420f'` (full range) is captured, 709 is tagged, and no
   full-range flag is set. The client territory should confirm what the browser
   decoder assumes before anyone changes the pixel format.
10. **AirPlay is faster on the same Mac.** That is the user's own A/B and it is
    worth taking literally: AirPlay does its capture, scale, encode and pacing
    entirely inside WindowServer/AVFoundation with the hardware encoder and a
    display-link cadence. The gap is most consistent with (a) a software encoder,
    (b) capture starvation at queueDepth 2, or (c) a mis-set thread policy —
    in roughly that order of expected effect.

---

## Findings written

25 files under `findings/`, all with `Status: open`:

**blocker (1)**
- `blocker-cgds-queue-depth-starvation.md`

**major (11)**
- `major-vt-silent-software-encoder.md`
- `major-vt-qos-thread-policy-returns-ignored.md`
- `major-vt-workgroup-created-but-never-driven.md`
- `major-vt-encoder-fps-floored-at-60.md`
- `major-vt-unbounded-encoder-output-channel.md`
- `major-vt-pending-ts-mutex-priority-inversion.md`
- `major-vt-complete-frames-blocks-the-realtime-thread.md`
- `major-vt-encode-loop-exits-silently-on-first-submit-error.md`
- `major-cgds-capture-timestamp-is-callback-arrival-not-display-time.md`
- `major-cgds-dirty-rect-skip-can-blank-a-new-client.md`
- `major-session-start-on-monitor-blocks-a-tokio-worker.md`

**minor (10)**
- `minor-sck-color-config-diverges-from-cgds.md`
- `minor-vt-set-optional-results-discarded.md`
- `minor-vt-low-latency-version-gate-is-redundant-with-the-spec-ladder.md`
- `minor-vt-periodic-idr-every-10-seconds.md`
- `minor-vt-per-frame-allocations-in-hot-path.md`
- `minor-vt-panics-reachable-at-runtime.md`
- `minor-vt-pipeline-start-leaks-backend-and-thread.md`
- `minor-vt-setpriority-fails-for-unprivileged-users.md`
- `minor-cgds-pixelbuffer-leaked-on-nonzero-cvreturn.md`
- `minor-docs-stale-prd-and-milestone-references.md`

**speculative (3)**
- `speculative-cgds-windowserver-does-the-downscale-with-no-source-rect.md`
- `speculative-vt-encode-loop-4ms-idle-and-wake-token-race.md`
- `speculative-vt-broadcast-lag-to-idr-feedback-loop.md`

Two drafts were deleted as duplicates of findings other territories had already
filed: the `sw_vers` version-gating one is covered by
`major-compat-mac-capabilities-gated-by-a-sw-vers-subprocess.md` (my only unique
addition, the redundant 11.0 gate in `encoder.rs`, was split out as
`minor-vt-low-latency-version-gate-is-redundant-with-the-spec-ladder.md`), and
the `gpu.rs` stub one by
`minor-vt-gpu-struct-is-an-empty-stub-claude-md-says-otherwise.md`.

# LATENCY MAP — what every stage costs

Companion to `docs/DATAFLOW.md`, which describes the same stages structurally. This document
attaches a number to each one.

**Every figure in the ESTIMATED column is derived from code, from an API contract, or from a
cited public measurement. None of them is a measurement of this system.** The MEASURED column
is deliberately empty; it is filled from `docs/BASELINE.md` once the probe harness and the
camera procedure described there have been run. Two prerequisites gate that:

1. `major-client-no-glass-to-glass-telemetry.md` — the client's `videoDelayEmaMs`
   (`transform-worker.js:57-93`) subtracts a **host monotonic epoch** (`HOST_EPOCH`,
   `streamer/audio/mod.rs:17-33`) from a **client Unix epoch**
   (`performance.timeOrigin + performance.now()`). The absolute value is nonsense; only its
   first difference is usable. Every "measure `videoDelayEmaMs` before/after" instruction is
   blocked on this.
2. `major-webrtc-no-per-stage-encode-latency-metric.md` and
   `major-pipeline-win-one-aggregate-timer-and-no-stage-breakdown.md` — the host emits one
   aggregate `encode-path latency:` line every 60 frames, on two of its eight video paths, and
   **nothing at all on the primary NVIDIA ring path**. Until the stages are split
   (frame-stamp → callback → staged → submit → bitstream → broadcast) there is no number for
   any recommendation to move.

**Already fixed in the working tree, and therefore not counted below:** the RTT unit mismatch
that discovery report A §15.1 ranked as its highest-value finding. `webrtc_session.rs:900-911`
now carries an explicit comment recording that `RemoteInboundRTPStats::round_trip_time` is
already milliseconds in webrtc-rs, reads it unscaled, and feeds an `RttMonitor`
(`bitrate.rs:120`, `:144-162`) that fires only when RTT exceeds its own rolling **baseline** by
`RTT_DELAY_THRESHOLD_MS = 40`. The flat `> 80 ms → ×0.85` clamp that pinned every session at
the 1 Mbps floor is gone, and so is the finding file.

Reference workload throughout: **1920×1080, 60 fps, one viewer, LAN**. Frame period = 16.7 ms.

---

# 1. Per-stage cost, per path

## 1.1 Windows · WGC · NVENC texture ring

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| 1 | DWM → pool surface | — | 0–16.7 ms | — | Compositor cadence; not ours |
| 2 | Pool queueing | `graphics_capture_api.rs:197` | **0–33 ms** | — | Pool of 3 with `TryGetNextFrame` **oldest-first** and **no drain loop**: a consumer one frame behind serves a 2-frame-old picture forever = up to 2 frames = 33 ms at 60 Hz. Sunshine drains implicitly; OBS uses 2 buffers. |
| 3 | `MinUpdateInterval` hold-off | `pipeline.rs:342` — `frame_duration * 2/3` | **0–11.1 ms** | — | 11.1 ms at 60 fps. `windows-capture#190` measured 1/60 s producing ~45 fps on a 60 Hz panel, so the interval behaves as a floor on the inter-frame gap and must sit well *below* the target period. Sunshine uses a flat 4 ms. |
| 4 | Message-pump hop | `capture.rs:35`, `:409` | **0.1–0.5 ms + scheduler jitter** | — | `Create` (not `CreateFreeThreaded`) ⇒ DispatcherQueue enqueue → thread wake → `GetMessageW` return → `DispatchMessageW`. One scheduler-mediated hop that `CreateFreeThreaded` removes. |
| 5 | COM unwrap + `GetDesc` + `Arc::new` | `graphics_capture_api.rs:273-314` | **< 50 µs** | — | 5 cross-ABI calls + one heap alloc |
| 6 | Slot acquire | `pipeline.rs:1149` | **0 or ∞** | — | `try_recv` on `bounded(3)` with one slot permanently withheld ⇒ effective depth 2. Never blocks; on empty the **frame is dropped silently**. At 60 fps with a 6 ms encode there is headroom; at 4K or on x264 there is not. |
| 7 | `AcquireSync(KEY_WRITER)` | `pipeline.rs:1185` | **~0 normally, 1000 ms worst** | — | `KEY_TIMEOUT_MS = 1000` on a `THREAD_PRIORITY_TIME_CRITICAL` callback thread. A GPU hiccup becomes a one-second stall. |
| 8 | Stage into slot | `pipeline.rs:1164-1174` | **0.3–0.8 ms** | — | One full-frame `CopyResource` or `VideoProcessorBlt` at 1080p. 8.29 MB at typical intra-GPU bandwidth. |
| 9 | `Flush()` | `pipeline.rs:1172` | **0.1–0.5 ms** | — | Forced GPU submit **every frame**, on the capture callback thread, blocking delivery of frame N+1 |
| 10 | Thread handoff | `:1194` → `:1252` | **10–60 µs** | — | crossbeam send + thread wake, both MMCSS-tuned |
| 11 | `nvEncMapInputResource` | `nvidia/encoder.rs:726` | **< 50 µs** | — | Resource registered once at construction; map/unmap per frame is the SDK-sanctioned pattern |
| 12 | **`nvEncLockBitstream`** | `nvidia/encoder.rs:781` | **2–6 ms** | — | **The dominant serialisation point.** `doNotWait = 0`, `enableEncodeAsync = 0`, `sliceMode = 0` (one slice), `enableSubFrameWrite` unset, **one** bitstream buffer ⇒ exactly one picture in flight. NVENC P1 + ULTRA_LOW_LATENCY at 1080p is 2–6 ms; the K=3 ring decouples *capture* from encode and buys nothing on the encode side. |
| 13 | `to_vec()` inside the lock | `nvidia/encoder.rs:789-795` | **20–60 µs** | — | ~25 KB memcpy out of write-combined driver memory (WC reads run ~1 GB/s), **extending stage 12's critical section** |
| 14 | Broadcast | `pipeline.rs:1266` | **< 5 µs** | — | Refcount clone per subscriber. Capacity 4 = 66 ms of slack at 60 fps before `Lagged`. |
| — | Idle keepalive | `pipeline.rs:1252` | **200 ms** | — | `recv_timeout(200ms)` re-encodes the held slot as a **P-frame**. It **cannot produce an IDR**, because `encode_repeat_slot` reads the dead `Encoder::idr_requested` — `blocker-nvenc-ring-ignores-idr-on-static-desktop.md`. A client joining a static desktop waits for motion. |

**Path subtotal, capture callback → broadcast: ≈ 3–8 ms steady state, 0–44 ms of queueing in
front of it.**

## 1.2 Windows · WGC · NVENC CPU bridge

Stages 1–5 as above, then:

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| 6 | `EncodeCore` mutex | `pipeline.rs:1716` | **0–(one encode)** | — | Contended with `nvenc-repeat` (`:1504`). Held across the whole encode inside a WinRT `extern "system"` callback. |
| 7a | Scaler blt | `scaler.rs:~150` | 0.3–0.8 ms | — | G→G |
| 7b | **`Map(MAP_READ)` readback** | `scaler.rs:243-259` | **2–6 ms** | — | 8.29 MB at 1080p over PCIe *plus* a hard CPU↔GPU sync — the CPU blocks until the preceding `CopyResource` retires. No double-buffered staging, no `MAP_FLAG_DO_NOT_WAIT` retry. |
| 7c | `Frame::buffer()` (no scaler) | `windows_capture/frame.rs:187` | **+0.5–2 ms** | — | `CreateTexture2D` **per frame** on top of the readback |
| 8 | `UpdateSubresource` upload | `nvidia/encoder.rs:392` | **2–5 ms** | — | 8.29 MB CPU→GPU, ~500 MB/s of sustained bus traffic at 60 fps |
| 9 | Encode | as §1.1 stage 12 | 2–6 ms | — | — |
| — | **Serialisation** | — | **capture N+1 blocked by encode N** | — | The encode runs inline on the WGC capture thread, so the frame rate is capped at `1 / (copy + readback + upload + encode)` ≈ **1/(10–19 ms) ≈ 50–100 fps**, and every capture is delayed by the previous encode |

**Path subtotal: ≈ 8–20 ms, all serial.** This is roughly **3× the ring path**, and the only
announcement is one `teprintln!`.

## 1.3 Windows · DXGI · NVENC — *the path that actually runs on most hosts*

`GraphicsCaptureApi::new` turns an absent `MinUpdateInterval` property into a hard `Err`
(`graphics_capture_api.rs:148-176`). That property is **build 26100 (Win11 24H2)**, so **every
Windows 10 host and every Windows 11 host before 24H2 runs this path**, not §1.1.

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| 1 | Idle poll timeout | `pipeline.rs:583-589` | **0–8 ms** | — | Flat **8 ms** when not dirty. `AcquireNextFrame` is a wait, not a poll, so the timeout costs wakeups — except on the idle→active edge, where the current wait must expire first. |
| 2 | Change gate | `dxgi/mod.rs:349-395` | < 100 µs | — | `GetFrameMoveRects` + `GetFrameDirtyRects` fetched into reused buffers and **reduced to one boolean**. This is the one thing the DXGI path does better than WGC. |
| 3 | Duplication copy | `dxgi/mod.rs:426` | **0.5–1.0 ms** | — | `CopyResource(desktop, dup_tex)`, full frame, **unconditional** |
| 4 | Frame pacing | `pipeline.rs:611`, `:629` | **0–16.7 ms** | — | `next_due = capture + frame_duration`; a frame arriving just after `next_due` waits a **full period**. The `was_idle` reset (`:567-572`) is the right instinct but applies only after the 8 ms idle wait. |
| 5 | Composite + cursor | `dxgi/mod.rs:534-567` | **+0.5–1.0 ms + 1–2 quad draws** | — | A **second** full-frame operation whenever the cursor is visible or the display is rotated — i.e. essentially always. `major-dxgi-cursor-costs-a-full-frame-copy.md`. |
| 6 | Cursor sprite rebuild | `dxgi/cursor.rs:294,341,372` | **0.2–1 ms on a shape change** | — | 1–2 `Vec<u32>` of `w*h` + immutable textures + SRVs. An animated spinner makes this per-frame. |
| 7 | Encoder input | `pipeline.rs:599` | 0.3–0.8 ms (GPU) or **2–6 ms** (readback) | — | A *third* full-frame copy into the NVENC shared texture on the GPU path |
| 8 | Encode | as §1.1 stage 12 | 2–6 ms | — | **Inline on the capture thread. No capture/encode overlap at all.** `major-dxgi-path-serializes-capture-and-encode.md` |
| — | Access loss | `dxgi/mod.rs:~300-340` | **100 ms × up to 40** | — | `redup()` backs off 100 ms per attempt, `REDUP_MAX_RETRIES = 40`. Only the success is logged. |

**Path subtotal: ≈ 4–10 ms of work with 0–25 ms of queueing/pacing in front of it,
fully serial.**

## 1.4 Windows · Intel Quick Sync, same adapter

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| 1 | Copy into VPP surface | `intel/encoder.rs:695-716` | **0.3–0.8 ms** (+ the same again and 2 keyed-mutex round trips with an `InputBridge`) | — | Full-frame `CopySubresourceRegion` + `Flush`. `major-intel-extra-full-frame-copy-per-frame.md` |
| 2 | VPP (BGRA→NV12 + scale) | `intel/encoder.rs:612-648` | **0.5–1.5 ms** | — | One fused pass — the good part. The sync point is never synchronised, so this overlaps the encode submit. |
| 3 | `DEVICE_BUSY` retry | `:640`, `:770` | **0–30 ms** | — | `sleep(1 ms)` × 30 on a hot path. `minor-intel-device-busy-sleeps-1ms-on-hot-path.md` |
| 4 | `SyncOperation` | `intel/encoder.rs:784-798` | **3–8 ms** | — | **Blocking, `AsyncDepth = 1`.** The thread waits the full GPU encode; there is no pipelining to hide it. Bounded at 2000 ms. |
| 5 | Bitstream `to_vec` | `:801-805` | 20–60 µs | — | Compressed size only |
| — | **VBV** | `:394-396`, `:417` | **potentially seconds** | — | `BufferSizeInKB = coded_w*coded_h*4/1024` clamped 512..65000. At 1080p that is ~8160 KB ≈ **6.5 s of CPB at 10 Mbps**. It is being sized as an output allocation, not as a VBV. `major-intel-buffersizeinkb-is-a-multi-second-vbv.md` |
| — | **First bitrate change** | `:512-513` | **catastrophic** | — | `MFXVideoENCODE_Reset` with `NumExtParam = 0` drops `LowDelayBRC`, `WinBRC*`, `MaxFrameSizeI/P`, `MaxDecFrameBuffering = 1`, `RepeatPPS`, `ScenarioInfo` — **the entire low-latency configuration reverts to default seconds into a session.** `major-intel-init-warnings-swallowed.md` compounds it: every `MFX_WRN_*` is swallowed, so a runtime software fallback is invisible. |
| — | `LowDelayBRC` / `MaxFrameSize*` | `:464-470` | **0** | — | Per the oneVPL reference these are defined for VBR/QVBR/VCM, **not CBR** — the three lines are almost certainly no-ops today, while the init log claims them. |

## 1.5 Windows · Intel CPU bridge — the worst GPU path

| # | Stage | `file:line` | ESTIMATED | MEASURED |
|---|---|---|---|---|
| 1 | Scaler blt | `scaler.rs:~150` | 0.3–0.8 ms | — |
| 2 | Staging copy | `scaler.rs:243` | 0.3–0.8 ms | — |
| 3 | **`Map(MAP_READ)`** | `scaler.rs:243-259` | **2–6 ms** | — |
| 4 | `UpdateSubresource` | `intel/encoder.rs:561-570` | **2–5 ms** | — |
| 5 | Into VPP surface | `intel/encoder.rs:695` | 0.3–0.8 ms | — |
| 6–8 | VPP → encode → sync | as §1.4 | 4–10 ms | — |
| | **Total** | | **≈ 9–24 ms, serial** | — |

Three full-frame copies and two bus traversals per frame. Reached whenever the capture device
is not Intel at all — `"cross-adapter Quick Sync bridge is not implemented"`
(`intel/encoder.rs:200-206`), announced with one `teprintln!`.

## 1.6 Windows · x264

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| 1 | Readback | `scaler.rs:243-259` | **2–6 ms** | — | as above |
| 2a | BGRA memcpy | `x264/encoder.rs:~163` | **1.5–3 ms** | — | A full-frame 8.29 MB `extend_from_slice`, then libx264 copies it again internally |
| 2b | I420 CSC | `x264/encoder.rs:309-437` | **2–6 ms** | — | Scalar BT.601 over 2.07 M luma + 0.52 M chroma samples on `(cores/2).clamp(1,4)` rayon threads. `minor-x264-colour-convert-is-scalar-on-at-most-four-threads.md` |
| 3 | Encode | `x264_encoder_encode` | **8–15 ms at 1080p** | — | `ultrafast` + `zerolatency`, `b_sliced_threads = 1`, `i_threads = min(cores, 8)`. Frame threading genuinely off (verified by the smoke test's frame-in/packet-out assertion). |
| 4 | AU copy | `:251`, `:305` | 20–60 µs | — | — |
| — | Idle repeat | `:178` | **+1.5–3 ms per tick** | — | The BGRA path `clone()`s the full 8.29 MB frame on every repeater tick |
| — | **Correctness** | `:113-125` | — | — | The BGRA fast path is selected by `x264_encoder_open` returning non-null, which means "this build accepts packed BGRA", not "this build will emit 4:2:0". A 4:4:4-capable libx264 emits **High 4:4:4 Predictive**, which browsers cannot decode. `blocker-x264-bgra-input-emits-444-stream.md` |

**Path subtotal: ≈ 12–27 ms, serial.** This is the tier **every AMD host lands on**, because
there is no AMF backend (`major-amd-hosts-get-no-hardware-encoder.md`).

## 1.7 macOS · CGDisplayStream · VideoToolbox

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| 1 | Composite + scale + `'420f'` | WindowServer, driven by `cgds.rs:179-189` | scales with resolution | — | The requested dims are the **already-scaled** encode dims and no `SourceRect`/`DestinationRect` is set, so WindowServer does the downscale. Outside our process. |
| 2 | **`MinimumFrameTime` hold-off** | `cgds.rs:70` — `(1/fps) * 0.95` | **uniform 0–15.8 ms, mean ≈ 8 ms** at 60 fps | — | A **hold-off**, not a rate cap: after a delivery the stream will not deliver another for that long, so an update landing just after a delivery waits up to one interval. At 30 fps the mean is ~17 ms. `minor-cgds-minimum-frame-time-coalescing.md` |
| 3 | **Queue-depth starvation** | `cgds.rs:71` (depth **2**) vs `pipeline.rs:202` (2 pinned) | **potentially a full frame period or worse, every frame** | — | The pipeline holds the taken `Arc<Frame>` in `last_frame` *and* the newly published one sits in the `ArcSwapOption`, so **both** of the two queued IOSurfaces are retained in steady state and the stream has nothing free to write into. `blocker-cgds-queue-depth-starvation.md` — **the most likely single cause of the reported macOS symptom, and the smallest diff.** |
| 4 | Delivery to our queue | `cgds.rs:162-165` | unmeasurable today | — | `displayTime` is bound as `_display_time` and discarded (`:172`), so stage 1→3 has no ground truth. `major-cgds-capture-timestamp-is-callback-arrival-not-display-time.md` |
| 5 | Callback work | `cgds.rs:102-156` | **< 0.5 ms** unless malloc contends | — | 1 CF alloc + 1 `Arc` alloc + the dirty-rect query |
| 6 | Frame slot + wake | `frame.rs:63-66`, `pipeline.rs:181` | **0 or 4 ms** | — | 0 when the encode thread is running; up to the 4 ms `idle` poll when the single wake token was consumed out of order by `request_idr`/`set_target_bitrate`. `speculative-vt-encode-loop-4ms-idle-and-wake-token-race.md` |
| 7 | `pending_ts` mutex | `pipeline.rs:151`, `:233` | **0 uncontended, unbounded inverted** | — | A plain `std::sync::Mutex` between a `THREAD_TIME_CONSTRAINT` thread and a `USER_INTERACTIVE` thread — an unprotected priority inversion. The free `sourceFrameRefCon` slot (`encoder.rs:299` passes null) would delete it. `major-vt-pending-ts-mutex-priority-inversion.md` |
| 8 | **VT encode** | `encoder.rs:268-307` | **2–6 ms hardware / 15–40 ms software** | — | Asynchronous submit, so the *submit* is cheap; the dwell shows up as output-callback latency. The spec ladder is Require-HW → Enable-HW → hinted, and **only the third rung logs** (`encoder.rs:105-114`). `UsingHardwareAcceleratedVideoEncoder` is read and only printed (`:355-370`). **`major-vt-silent-software-encoder.md` — the single measurement that would most change the macOS diagnosis.** |
| 9 | `complete_pending()` on the keepalive path | `pipeline.rs:209-215` | **a full encoder flush** | — | `VTCompressionSessionCompleteFrames(kCMTimeInvalid)` — blocking, **on the real-time thread**, on every pre-11.0 host. `major-vt-complete-frames-blocks-the-realtime-thread.md` |
| 10 | VT output queue | `encoder.rs:71` | 0 drained promptly, **unbounded otherwise** | — | `crossbeam::unbounded`, no back-pressure at all. `major-vt-unbounded-encoder-output-channel.md` |
| 11 | AVCC→Annex B | `encoder.rs:413-497` | **< 0.5 ms** | — | Two passes + one `Vec` alloc over 10–100 KB |
| 12 | Broadcast(4) | `pipeline.rs:86` | 66 ms of slack | — | Overflow → `Lagged` → forced IDR. `major-webrtc-lagged-viewer-requests-idr-and-makes-it-worse.md` |
| — | **fps floor** | `config.rs:30-36`, `pipeline.rs:276` | **every stage runs at ≥60 Hz** | — | `live_encoder_config` raises any refresh below 60 to 60 and `cap_fps` then takes `max(enc.fps, refresh)` — a client that asked for 30 still drives the whole pipeline at 60. `major-vt-encoder-fps-floored-at-60.md` |
| — | **Periodic IDR** | `encoder.rs:~167` | **every 10 s** | — | `MaxKeyFrameIntervalDuration = 10.0` s produces an unrequested keyframe spike twice a minute. `minor-vt-periodic-idr-every-10-seconds.md` |

## 1.8 macOS · ScreenCaptureKit · VideoToolbox

Identical from stage 5 onward. Differences:

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| 2' | `minimumFrameInterval` | `sck.rs:~558` | **drops frames rather than limiting them** | — | Set to *exactly* `CMTime{1, fps}`. Apple's semantics are a minimum interval, so a frame arriving one scheduling tick early is discarded outright. `major-sck-minimum-frame-interval-exactly-one-over-fps-drops-frames.md` |
| 3' | Queue depth | `sck.rs:566` — **3** | 2 of 3 pinned | — | Less starvation pressure than cgds, same mechanism. Apple's documented default is higher — `major-sck-queue-depth-3-contradicts-apples-documented-default.md`. |
| 5' | Re-wrap | `sck.rs:697-702` | **+1 CF alloc/frame** | — | SCK already hands us a `CVImageBuffer` and the code deliberately extracts the IOSurface and builds a **fresh** `CVPixelBuffer` |
| — | First frame | `sck.rs:~600` | **one frame lost at join** | — | `major-sck-first-frame-after-start-is-discarded.md` |
| — | Tier gate | `macos_utils/streamer/mod.rs:61-82` | **3–5 subprocess forks per probe** | — | `screencapturekit_available()` is `macos_at_least(12, 3)`, which **forks `sw_vers`** and **fails open** (`:75`, `None => true`). SCK is enabled at 12.3 but is known broken before 13.0 — `major-sck-tier-enabled-at-12.3-but-known-broken-before-13.0.md`. |

## 1.9 Transport (all video paths)

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| T1 | Broadcast dwell | `pipeline.rs:44` | **0–66 ms at 60 fps, 0–33 ms at 120 Hz** | 0 lagged events in 26 self-test runs (idle desktop) | Capacity 4. Overflow yields `Lagged`; the response is no longer an unconditional `request_idr` — it goes through the per-session keyframe gate and cuts the rate at the same time, and is counted as `frames_dropped_broadcast`. Whether 4 is the right capacity is still unmeasured: `minor-pipeline-broadcast-capacity-is-four-frames-and-untested-under-loss.md` |
| T2 | Payloading | `webrtc_session.rs:516` | **P-frame ~20 µs; 300 KB IDR ~250 allocs + 300 KB copied** | — | Per viewer. `minor-webrtc-fua-fragmentation-allocates-and-copies-every-frame-per-viewer.md` |
| T3 | Paced burst | `webrtc_session.rs` (writer) + `streamer/pacer.rs` | **10–40 ms of induced queueing delay on congested Wi-Fi, before pacing** | `pace_wait` p50/p99 = 0/0 ms on a same-subnet session, where the pacer is deliberately `Off` | A leaky bucket at libwebrtc's 2.5× factor with Stadia's measured ~8-packet burst, capped at 75% of one frame interval. Three modes by locality: `Off` on a same-subnet hop (where spacing only adds delay and `socket_write` p50 is unchanged at 2.10 ms), `Light` unclassified, `Full` relayed. The benefit is unmeasurable on loopback — there is no contention to smooth — and needs a busy AP. |
| T3b | RTP MTU | `webrtc_session.rs` | — | — | 1188 bytes same-subnet, **1012 relayed** — a relayed datagram carries TURN encapsulation, SRTP auth and outer IP/UDP on top, which can exceed a tunnelled path MTU or the 1280-byte IPv6 minimum. Moonlight caps remote streams at 1024; Sunshine defaults to it. |
| T4 | SRTP + socket | webrtc-rs | **µs/packet** | — | Not a factor |
| T5 | LAN | — | **1–5 ms** | — | — |
| T6 | Client depacketize | UA | **< 1 ms** | — | — |
| T7 | `EncodedVideoChunk` copy | `transform-worker.js:252-256` | **< 100 µs + 1 alloc** | — | Spec-mandated |
| T8 | Client decode | UA | **2–8 ms hardware / 10–30 ms software** | — | `optimizeForLatency: true` set; **no `hardwareAcceleration` hint** — `major-client-isconfigsupported-probes-a-different-config.md` |
| T9 | `drawImage` + composite | `transform-worker.js:116` | **1–3 ms** | — | 5-argument destination-rect form, so a scale is applied even when the sizes match. Painting inline in the decoder callback rather than under `rAF` saves one compositor frame — the best decision in the client. |

### Join chain, measured

The join was a serial chain of independent waits with no instrumentation. It now logs its own
breakdown (`join stages (<locality>): display=… pipeline=… handshake=…`) and the WHEP self-test
reports `whep_answer_ms` and `first_rtp_ms`. On Windows 11 26200 / RTX 3080 Ti, loopback,
1280x720, over 12 runs:

| Stage | Before | After |
|---|---|---|
| Host WHEP handshake (PC + SDP + ICE gather) | 77–221 ms | **12–20 ms** |
| Virtual display create/attach/mode/settle | not instrumented | 700–1300 ms |
| `start_on_monitor` | not instrumented | 540–1020 ms |
| `/whep` POST → answer returned | not instrumented | 1250–2130 ms |

The handshake collapse is the same-subnet peer no longer being offered a STUN server, so host-only
gathering completes almost immediately instead of waiting on a round trip to the public internet.
What remains is dominated by display creation and pipeline start, neither of which is transport
work — and because the handshake is now ~1% of the join on this path, overlapping the two is worth
far less than it looks. See `minor-webrtc-handshake-does-not-overlap-display-creation.md`, which
carries the numbers, and `major-whep-no-trickle-ice.md` for the larger change.

**Steady-state glass-to-glass, best case (Windows WGC+NVENC ring, LAN, WebCodecs client):**
capture queueing 0–33 ms + stage 3–8 ms + transport 2–6 ms + decode/paint 3–11 ms
≈ **8–58 ms**, with the spread dominated by queueing, not by work.

**Worst realistic case (Windows 10 → DXGI + x264 → `<video>` fallback):**
8 + 17 + 27 + 40 (burst) + 90 (jitter buffer) ≈ **180 ms+**.

---

# 2. Audio, per-stage

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| **W1** | WASAPI engine period | `loopback.rs:307-322` | **+5 ms mean, +10 ms worst** | — | Both duration params are 0 ⇒ the engine grants its default, typically 10 ms. **The period is never queried**; the value logged as "period" is `GetBufferSize()` (`:355`). Documented floor on small-buffer drivers is 128 samples = **2.67 ms**, reachable by having the *silent companion* request it via `IAudioClient3::InitializeSharedAudioStream` — `major-audio-win-engine-period-lowerable-via-render-companion.md`. Measured on the dev box: `default = fundamental = min = max = 480` (10.000 ms), i.e. no headroom on that hardware. |
| **W2** | Accumulator residue | `loopback.rs:222`, `:255-257` | **0–4.99 ms** | — | The engine period is not a multiple of the 5 ms Opus frame |
| W3 | Convert to stereo f32 | `format.rs:134-141` | **low single-digit µs** | — | ~960 load/clamp/push iterations per wake. Not a memcpy. |
| W4 | Opus encode | `loopback.rs:227` | **< 1 ms** (measured into `encode_p50_ns` / `encode_p99_ns`) | — | CELT-only `RESTRICTED_LOWDELAY`, complexity 5, 5 ms frames |
| W5 | RT-thread diagnostics | `loopback.rs:259-263`, `:458-466` | **a stall every 2 s** | — | A 32 KiB alloc + a 4096-element sort + the logbus mutex + a Tauri IPC emit, **on the "Pro Audio" thread**. `minor-audio-win-realtime-thread-allocates-sorts-and-logs.md` |
| **M1** | Process Tap IO cycle | `process_tap.rs` (never set) | **+5.3 ms mean, +10.67 ms worst** | — | `kAudioDevicePropertyBufferFrameSize` appears nowhere in the file; a tap aggregate typically inherits 512 frames. Chromium runs **256** frames on *output* and its `kMinAudioBufferSize` is **128**; a capture IOProc has none of the render-side under-run exposure Chromium guards against. The reference implementation (AudioCap) also never sets it. `major-audio-mac-tap-aggregate-buffer-frame-size-never-set.md` |
| M2 | 5 CPU passes over the samples | `format.rs`, `ring.rs:60`, `mod.rs:262`, `:290` | **50–150 µs** | — | `AudioFormatDesc::is_fast_path()` exists and is **never consulted**, so 48 kHz stereo f32 interleaved — the case the pipeline is designed around — takes the same byte-wise reassembly + BS.775 downmix as an 8-channel i16 source |
| M3 | **`unpark` in the RT callback** | `ring.rs:68-72` | **unbounded** | — | Rust's macOS parker takes a `pthread_mutex` in `unpark` whenever the target is parked, and the consumer parks on **every idle gap** (`mod.rs:351-353`). A blocking, non-priority-inheriting lock acquisition on a real-time thread. `major-audio-mac-unpark-inside-realtime-callback.md` |
| M4 | Worker idle poll | `mod.rs:351-353` | **0–125 µs** | — | `park_timeout(125 µs)` |
| L1 | Legacy driver period | `legacy/mod.rs:70-79` | **+1.3 ms mean, +2.67 ms worst** | — | 128 frames — **the smallest buffer in the system, and the only tier that negotiates its own** |
| L2 | shm poll | `shm_reader.rs:236` | **0–500 µs** | — | `park_timeout(500 µs)`; **nothing can unpark it**, the producer is another process |
| L3 | Playthrough monitor | `playthrough.rs` | **85.3 ms** | — | `DRIFT_SETPOINT_FRAMES = 4096` of monitor backlog is what the *host itself* hears. `major-audio-mac-playthrough-85ms-monitor-latency.md`. `IoCtx::underruns` is incremented and never read — precisely the number needed to lower it safely. |
| **S1** | Opus frame + lookahead | `encoder.rs:6-7`, `:100` | **7.5 ms** (5 + 2.5) | — | `OPUS_GET_LOOKAHEAD` reads back 120 samples. `.unwrap_or(0)` on failure feeds the client a false zero. |
| S2 | Bridge-thread hop | `streamer/audio/mod.rs:156-162` | **10–60 µs, unbounded under load** | — | A plain `std::thread` at **default priority** doing a pure copy loop — a removable type-adapter hop between two real-time-ish stages. `major-audio-core-bridge-thread-is-a-removable-hop.md` |
| S3 | Broadcast(128) | `streamer/audio/mod.rs:93` | **0–640 ms** | — | 128 packets × 5 ms. `Lagged` is silently `continue`d and **`FLAG_DISCONTINUITY` is not set on the next packet**, so the client never learns there was a hole. `major-audio-core-broadcast-lag-swallowed-without-discontinuity.md` |
| S4 | Forwarder double copy | `webrtc_session.rs:~784-792` | **< 1 µs + 1 alloc** | — | The reused `scratch` is defeated by the final `Bytes::copy_from_slice` |
| S5 | DataChannel backpressure | `webrtc_session.rs:46`, `:778` | **≈1 s of audio before a drop** | — | 16 KiB of `buffered_amount`, checked **every 4th packet**; the drop is silent to the client |
| C1 | Client decode | `audio-worker.js` | **sub-ms** | — | Unverified |
| **C2** | Client jitter depth | `audio.js:6-7`, `:426-451` | **10–400 ms, commanded** | — | `videoDelay − preEnqueue − outputLatency`, clamped. `outputLatencyMs()` treats a missing `AudioContext.outputLatency` as **0** — Safari < 18.4, Chrome < 102, every WebView on the 10.15 floor. On a Bluetooth sink that is a **100–200 ms** systematic A/V error. `major-client-outputlatency-absent-breaks-avsync-on-safari-and-bluetooth.md` |
| **C3** | Underrun re-prime | `audio-worklet.js:178-188` | **one lost 5 ms packet ⇒ 10–40 ms of digital silence** | — | `priming = true` after *any* underrun, outputting silence until `avail >= effTarget`. There is no gap detection and no PLC in the worker either. **The largest audible-quality lever in the client.** `major-client-one-lost-packet-costs-a-full-jitter-buffer.md` |
| C4 | `AudioContext` output | platform | **3–10 ms desktop / 20+ ms iOS / 100–200 ms Bluetooth** | — | Floor |

**Audio total, best case today (Windows, wired sink):**
10 (period) + 2.5 (residue) + 7.5 (Opus) + <1 (encode) + ~0.1 (hops) + 2 (LAN) + 10 (jitter
floor) + 5 (output) ≈ **≈ 37 ms**. With the two buffer-size fixes and no other change:
**≈ 27 ms**.

**A/V sync note:** `capture_ns` is taken at **encode time** on Windows (`loopback.rs:245`) and
**synthesised forward from the drain wake-up** on macOS (`mod.rs:284-315`), while the video RTP
timestamp rides `EncodedFrame.capture`, itself a **callback-arrival** `Instant` rather than the
true frame time. Three of the four numbers in the sync equation are the wrong instant.
`major-audio-mac-capture-ns-is-drain-time.md`, `major-audio-win-capture-timestamp-taken-at-encode-time.md`,
`major-pipeline-win-capture-timestamp-is-callback-arrival-not-frame-time.md`,
`major-cgds-capture-timestamp-is-callback-arrival-not-display-time.md`.

---

# 3. Input, per-stage

| # | Stage | `file:line` | ESTIMATED | MEASURED | Reasoning |
|---|---|---|---|---|---|
| 1 | Client coalescing | `input.js:279-320` | **0–1 task (~0–4 ms)** | — | The first move of each task is sent immediately; the rest coalesce. Good design. `getCoalescedEvents()` is used **only for pen** (`:333`), so a 240 Hz touch digitiser is decimated to one sample per task. |
| 2 | `fast` drop | `input.js:74-78` | **the event is gone** | — | `bufferedAmount > 8192` ⇒ silent, uncounted drop |
| 3 | SCTP + `on_message` | `webrtc_session.rs:425-437` | **~100 ns + 1 `Box::pin` alloc per message** | — | 3 `Arc` clones + a heap allocation on every input event |
| 4 | Parse | `input/protocol.rs:235` | **< 1 µs** | — | `SmolStr` keeps the common key event allocation-free |
| 5 | Route | `input/mod.rs:63-76` | **~100 ns** | — | `try_send`. On a full 512-deep fast queue the event is dropped and only an atomic nobody reads is bumped. |
| 6 | **Channel wait + thread wakeup** | `input/mod.rs:103-112` | **10–60 µs typical, unbounded under load** | — | The single biggest avoidable hop on the input path. `crossbeam::select!` picks **randomly** among ready operations, so a key-down has no precedence over a pointer move and cross-channel order is not preserved. |
| 7 | Fast-queue backlog | `input/mod.rs:52` | **up to 2.5 s, replayed IN ORDER** | — | 512 entries at a ~5 ms cadence. When the injector falls behind, the backlog is not skipped — it is replayed. `major-input-fast-queue-512-deep-silent-drop.md` |
| 8 | `refresh_geometry` | `windows.rs:196` / `macos.rs:325` | **~50 ns cached; 5 `GetSystemMetrics` + a monitor lookup every 500 ms** | — | Called **once per sample**, not once per batch |
| 9a | Windows injection | `windows.rs:850` | **1–5 µs** (`SendInput`) | — | Reused `Vec<INPUT>` scratch, one syscall per event |
| 9b | **Windows batch** | `windows.rs:117` | **×N samples** | — | `pointer_batch` loops per sample, **one OS call each** — a 40-sample batch is 40 syscalls. `major-input-batch-replayed-one-syscall-per-sample.md` |
| 9c | macOS injection | `macos.rs:~494` | **20–100 µs** | — | `CGEventPost` to `kCGHIDEventTap` is Mach IPC to WindowServer, plus **one CoreFoundation allocation per event** — there is no reuse path and the API has no batch form |
| 10 | OS → app | win32k / WindowServer | **1 frame** | — | Not ours |

**Input total: ≈ 0.1–5 ms of controllable cost on top of the network RTT**, dominated by the
thread hop and, on batches, by the per-sample syscall.

---

# 4. Serialization points

Every place one stage waits on another that need not. Ordered by expected win.

| # | Where | `file:line` | What waits on what | Expected win from decoupling |
|---|---|---|---|---|
| 1 | **NVENC has no encoder pipelining** | `nvidia/encoder.rs:781`, `:699`, `:~647` (`enableEncodeAsync = 0`) | Frame N+1 cannot even be *submitted* until frame N's bitstream is locked, copied and unlocked. One bitstream buffer, synchronous mode, one slice, no sub-frame write. | **2–6 ms per frame** off the critical path. The SDK's own guide says `NvEncEncodePicture` "is non-blocking" and that async mode is supported on Windows 10+. `major-nvenc-sync-single-bitstream-defeats-ring.md`, `major-nvenc-single-slice-no-subframe-readback.md` |
| 2 | **DXGI and WGC-classic run encode on the capture thread** | `pipeline.rs:599`, `:1716` | Capture of N+1 waits for encode of N, under `Arc<Mutex<EncodeCore>>`, inside a WinRT callback on the WGC path | **The whole encode time, every frame** — 4–20 ms depending on backend. This is exactly what the K=3 ring exists to break, and it applies only to NVENC. `major-dxgi-path-serializes-capture-and-encode.md` |
| 3 | **WGC pool is never drained to the newest frame** | `graphics_capture_api.rs:264` | Every consumer serves the oldest of up to 3 buffered frames | **up to 2 frames = 33 ms** of pure age. Loop `TryGetNextFrame` until null, keep the last, close the rest. `major-wgc-framepool-never-drains-to-the-newest-frame.md` |
| 4 | **CGDisplayStream queue starvation** | `cgds.rs:71` vs `pipeline.rs:202` | The capture stream waits for a free surface that the pipeline is holding | **potentially a full frame period every frame.** Release `last_frame` before taking the next, or raise the queue depth. `blocker-cgds-queue-depth-starvation.md` |
| 5 | **`Flush()` on the WGC capture callback thread** | `pipeline.rs:1172` | Frame N+1's delivery waits on a forced GPU submit for frame N | **0.1–0.5 ms per frame**, and it is on the thread that pumps the messages that deliver frames |
| 6 | **`Map(MAP_READ)` readback** | `scaler.rs:243-259` | The CPU blocks until the preceding `CopyResource` retires | **2–6 ms per frame** on every cpu-bridge path. A double-buffered staging texture plus `MAP_FLAG_DO_NOT_WAIT` removes most of it. `major-pipeline-win-cpu-bridge-readback-stalls-encode-thread.md` |
| 7 | **Intel `SyncOperation`** | `intel/encoder.rs:784-798` | The encode thread waits the full GPU encode; `AsyncDepth = 1` leaves nothing to overlap | **3–8 ms per frame**, but the fix trades against latency — `AsyncDepth = 1` is the *correct* low-latency value. The real win is #1's shape (a second surface in flight), not raising `AsyncDepth`. |
| 8 | **`complete_pending()` on the macOS RT thread** | `pipeline.rs:209-215` | The real-time encode thread blocks on an encoder flush during the idle keepalive | **a full encoder flush**, on every pre-11.0 host. `major-vt-complete-frames-blocks-the-realtime-thread.md` |
| 9 | **`pending_ts` mutex across a QoS boundary** | `pipeline.rs:151`, `:233` | A `TIME_CONSTRAINT` thread waits on a `USER_INTERACTIVE` thread | Unbounded under inversion. Use the free `sourceFrameRefCon` slot (`encoder.rs:299`) and delete the mutex, the `VecDeque` and the 1:1 assumption in one change. |
| 10 | **`start_on_monitor` blocks a tokio worker** | `server.rs:1038` | Every other session's RTP writer on that adapter waits for D3D11 device creation, encoder session setup and capture start — with a full DXGI fallback behind it. On macOS it also runs 3–5 `sw_vers` forks, a 700 ms TCC probe and two 5 s SCK bridges. | **tens to hundreds of ms** of stolen worker time at every join. `major-session-blocking-os-calls-on-tokio-workers.md`, `major-session-blocking-os-calls-on-tokio-workers.md` |
| 11 | **`DISPLAY_CORRELATION_LOCK` is process-wide** | `server.rs:~928` (a `tokio::sync::Mutex<()>`) | Concurrent joins serialise on new-display creation, **held across `spawn_blocking` awaits and a ≤5 s poll loop** | **up to 5 s per concurrent join.** `minor-session-display-correlation-lock-serializes-all-joins-for-up-to-5s.md` |
| 12 | **The RTP writer awaits each fragment** | `webrtc_session.rs:527-545` | Each of ~250 IDR fragments waits on the previous socket write | Not a delay so much as a burst — see the pacer discussion. The fix is spacing, not concurrency. |
| 13 | **Pong awaited inside the receive handler** | `webrtc_session.rs:435` | Every subsequent reliable-channel message waits on an SCTP send | Head-of-line blocking on the channel that carries key events. `minor-input-datachannel-handler-allocates-and-sends-pong-inline.md` |
| 14 | **The audio bridge thread** | `streamer/audio/mod.rs:156-162` | The broadcast waits on a default-priority thread wake between two real-time-ish stages | **10–60 µs typical, unbounded under load.** Pure type adaptation; deletable. |
| 15 | **`AudioHub` mutex across device open and thread join** | `streamer/audio/mod.rs:147`, `:194` | Every session's audio subscribe/unsubscribe serialises behind a device open (`start_capture` blocks on `ready_rx.recv()` with **no timeout**) and a teardown that is **up to 5 s on the SCK backend** | Multi-second serialisation of audio subscription. `major-audio-core-hub-mutex-held-across-device-open-and-thread-join.md` |
| 16 | **The injector `select!` is a hard serializer** | `input/mod.rs:103-112` | All input from one client passes through one thread, and the select is **random**, not priority-ordered | A dropped precedence guarantee more than a latency cost, but the 512-deep backlog replay is real |
| 17 | **`wait_for_display_settle` always sleeps** | `server.rs:~1209` | The join path sleeps ≥75 ms even when nothing changed, because `last` starts as `None` | **≥75 ms per join** |
| 18 | **`driver_ipc` runs on a one-worker runtime** | `driver_ipc/sync.rs:12` | Every driver operation in the process serialises through a single tokio worker, and `WindowsVirtualDisplay.inner` is a `std::sync::Mutex` **held across `block_on` of a named-pipe round trip with no write timeout** | Serialises display create/remove across all devices. `minor-vdisplay-win-block-on-held-under-std-mutex.md` |

---

# 5. Startup / join path — client POST to first painted frame

Every stage runs to completion before the next. **Nothing overlaps.**

| # | Step | `file:line` | ESTIMATED | MEASURED | Note |
|---|---|---|---|---|---|
| 0a | `initSpeakers` + `needsMicPermission` | `index.html:1397` | **ms – 100s of ms** | — | `permissions.query` + `enumerateDevices` |
| 0b | **Microphone modal + browser prompt + `getUserMedia`** | `index.html:1399`, `audio.js:123-149` | **0–12 000 ms** | — | Needed only for audio-output **labels**, a feature unrelated to streaming, and it gates everything after it. `major-client-mic-prompt-gates-the-stream.md` |
| 0c | `prepare()` — `AudioContext` + `addModule('/audio-worklet.js')` | `audio.js:465-492` | **1 RTT + compile** | — | Called twice on the join path (`index.html:1385` and `:821`); the `state.prepared` latch is set only at the end, so both callers proceed and the second orphans the first `AudioContext` |
| 1 | `fetch('/ice-config')` | `index.html:808` | **1 RTT** | — | — |
| 2 | `new RTCPeerConnection` + `createOffer` + `setLocalDescription` | `index.html:816`, `:948-949` | **ms** | — | — |
| 3 | `fetch('/transform-worker.js')` + blob + `new Worker` | `index.html:789-802` | **1 RTT + worker spawn** | — | The blob URL is memoised, the worker is not, so every reconfigure repeats it |
| 4 | **`waitForIceGathering`** | `index.html:950` | **0–3000 ms (6000 with TURN)** | — | Non-trickle. Google STUN is always in the list (`config.rs` default), so this is **never zero even on a pure LAN join**. `major-client-join-blocks-on-ice-gathering.md`, `major-session-google-stun-on-every-lan-join.md` |
| 5 | `POST /whep` → capacity, ICE assembly, JSON parse, size caps | `server.rs:589-642` | **µs** | — | — |
| 6 | Ban → pre-approval → global pause → per-key lockout → `SessionAuth::validate` → mint token | `server.rs:655-747` | **several mutex round trips** | — | — |
| 7 | **Stop and await the previous capture** | `server.rs:785-797` | **a full encoder + capture teardown** | — | `spawn_blocking(stop).await` on **any** rejoin — and a settings change *is* a rejoin |
| 8 | Compute backing size, snap DPR, resolve overrides, decide orientation | `server.rs:799-887` | µs | — | — |
| 9a | *Existing display, params changed:* `set_display_mode` → `wait_for_display_settle` → `apply_display_scale` | `server.rs:892-916` | **≥75 ms + 3 `spawn_blocking` round trips** | — | `wait_for_display_settle` **always sleeps at least once** |
| 9b | *New display:* acquire `DISPLAY_CORRELATION_LOCK` → `monitor_device_names` (blocking, **on the async thread**) → `create_display_with_modes` → `display_device_name` else `wait_for_new_monitor` | `server.rs:927-971` | **0–5000 ms** (`DISPLAY_ATTACH_TIMEOUT`) | — | Topology nudge at 600 ms, 100 ms poll. **Process-wide lock**, so concurrent joins serialise. If the Windows IDD backend does not implement `display_device_name`, every new-display join takes the slow diff-and-poll path. |
| 10 | `set_display_mode` → settle → scale (+ `restore_displays` if the topology reset) | `server.rs:973-1003` | **≥75 ms** | — | On Windows a mode change **unplugs the monitor** — `major-vdisplay-win-mode-change-unplugs-the-monitor.md` |
| 11 | **`pipeline::start_on_monitor`** | `server.rs:1038` | **tens to hundreds of ms**, on a tokio worker | — | D3D11 device + scaler + encoder session + capture start, with the whole DXGI fallback behind it. On macOS: `sw_vers` forks + a 700 ms TCC probe + two 5 s SCK bridges. |
| 12 | `monitor_rect` for a log line | `server.rs:1043` | blocking, on the async thread | — | For a log line |
| 13 | `handle_whep_offer` — API, PC, data channels, `add_track`, spawn 4 tasks | `webrtc_session.rs:367-562` | **ms** | — | — |
| 14 | `set_remote_description` → **`request_idr()`** → `create_answer` | `webrtc_session.rs:639-645` | — | — | **This IDR is thrown away**: `write_rtp` with no bindings returns `Ok(0)` silently. `major-webrtc-first-idr-discarded-before-track-bound.md` |
| 15 | **Wait for host ICE gathering** | `webrtc_session.rs:654-655` | **0–1500 ms** (`GATHER_MAX_WAIT`) | — | Vanilla ICE — the client cannot start connectivity checks until the answer lands |
| 16 | Answer returned; reporter notified; leave armed; teardown task spawned | `server.rs:1076-1186` | µs | — | — |
| 17 | Client applies the answer → ICE connectivity → DTLS → SRTP | client | **RTT-dependent; DTLS ~200 ms per Firefox telemetry** | — | — |
| 18 | **Track binds; first frames are P-frames referencing a keyframe the client never saw; client emits PLI; host produces an IDR** | `webrtc_session.rs:472-483` | **one PLI round trip + the browser's PLI-generation latency** | — | The client's own PLI is the **only** thing that starts a session, because of the infinite GOP + the discarded IDR + the silently-dropped SPS/PPS. On the NVENC **ring** path with a static desktop, even the PLI cannot produce an IDR (`blocker-nvenc-ring-ignores-idr-on-static-desktop.md`). |
| 19 | **`refineRefreshRate()` may tear the whole thing down and repeat it** | `index.html:301`, `:1119-1123` | **the entire chain again** | — | Fires on any ≥5 Hz difference — i.e. on **every 90/120 Hz phone**, immediately after the stream goes live. `major-client-refresh-rate-renegotiates-right-after-live.md` |

**Worst case before DTLS even starts:** 12 000 (mic) + 3 000 (client gather) + 5 000 (display
attach) + ~200 (display settle) + pipeline + 1 500 (host gather) ≈ **22 s**.

**Typical LAN case:** dominated by 3 000 ms (client STUN gather) + 1 500 ms (host gather)
unless STUN resolves promptly, plus ~200 ms of display work and the PLI round trip.

**Theoretical minimum with the same architecture:** trickle ICE or a LAN-only candidate policy
removes both gather waits; a pre-created display removes the attach timeout; a host-side
keyframe on `Connected` removes the PLI round trip; the mic prompt runs in parallel. That
leaves 1 RTT for `/ice-config`, 1 RTT for the POST, one DTLS handshake, and one frame period —
**≈ 250–400 ms on a LAN**, against a current typical of 4–6 seconds.

**And every settings change replays all of it.** `applyReconfig` → `reconfigure()` →
`teardownSession(); start(true)` (`index.html:356-380`). CLAUDE.md's "settings changes apply
live via epoch bumps rather than tearing down the display" is true of the *display* and false
of the WebRTC session, the encoder, the capture and the injector thread.

---

# 6. Fallback cliffs

Every point where the path silently degrades, with the cost of the cliff.

| # | Cliff | Trigger | `file:line` | Latency cost | Visible? |
|---|---|---|---|---|---|
| 1 | **WGC → DXGI** | `MinUpdateInterval` absent — **build < 26100**, i.e. every Win10 and every Win11 before 24H2 | `graphics_capture_api.rs:148-176` → `pipeline.rs:274-277` | Loses the ring's capture/encode decoupling entirely on the classic path; gains a dirty-rect skip. Net: **+4 to +20 ms** and a full serialisation. | one `teprintln!`, nothing in the UI |
| 2 | **NVENC ring → NVENC classic** | any rejected NVENC flag makes `nvEncInitializeEncoder` fail | `pipeline.rs:1611-1616` | **+encode time onto the capture thread** (2–6 ms) plus the loss of pipelining | logged |
| 3 | **NVENC → Intel → x264** | the same single rejected flag falls all the way through the ladder | `pipeline.rs:1443-1455` | **+10 to +20 ms.** A perfectly good NVENC engine that merely lacks, say, `SINGLE_SLICE_INTRA_REFRESH` can land on software. There is no per-feature degrade tier. `major-nvenc-no-capability-probing.md` | logged |
| 4 | **Zero-copy → CPU bridge** | `create_d3d_device` silently drops `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` (`d3d11.rs:218-222`), or the capture device lands on a different adapter than the encoder | `d3d11.rs:218`, `intel/encoder.rs:200-206` | **+4 to +11 ms** (readback + upload) and a full CPU↔GPU sync per frame | **the `VIDEO_SUPPORT` drop is logged nowhere at all** |
| 5 | **Intel same-adapter → IntelCpu** | `new_on_device` bails (most often: the capture device is not Intel) | `pipeline.rs:1399-1412` | **+4 to +11 ms** | one `teprintln!` |
| 6 | **Any AMD host → x264** | there is no AMF backend; `amd.rs` is three `bail!`s | `windows_utils/streamer/amd.rs` | **+10 to +20 ms**, on every AMD machine | `major-amd-hosts-get-no-hardware-encoder.md` |
| 7 | **Hardware VT → software VT** | the encoder-specification ladder falls to the hinted rung | `encoder.rs:105-114` | **+13 to +34 ms** (2–6 → 15–40 ms) and a CPU-bound host | **only the third rung logs**; `UsingHardwareAcceleratedVideoEncoder` is read and only printed |
| 8 | **SCK → CGDisplayStream** | `screencapturekit_available()` false, or SCK start fails | `macos_utils/streamer/mod.rs:116` | Queue depth 3 → **2** (with 2 pinned ⇒ starvation), a `MinimumFrameTime` hold-off appears (**+8 ms mean**), different colour and dirty-rect semantics | logged; **no `CompatibilityReport.capture_backend` field exists** |
| 9 | **WebCodecs → `<video>`** | missing `RTCRtpScriptTransform`/`createEncodedStreams`, missing `VideoDecoder`, missing `transferControlToOffscreen`, a 5 s watchdog, a render stall, a `createEncodedStreams` throw, or any setup error | `index.html:166-169`, `:736-742`, `:868`, `:912`, `:995-1000` | **+~90 ms** — the full libwebrtc receive path with its jitter buffer. Multi measured exactly this as their single biggest win. On WebKit `jitterBufferTarget`/`playoutDelayHint` are **no-ops**, so the fallback runs at Safari's default. | `console.warn` only; **the host is never told** |
| 10 | **`RTCRtpScriptTransform` → `createEncodedStreams`** | Chromium takes the legacy branch | `index.html:862` vs `:866` | 0 ms directly — but `requestKey()` is a **no-op unless `transformer` exists** (`transform-worker.js:182`), so **the majority client can never ask for a keyframe out of band** | invisible |
| 11 | **SAB → postMessage audio ring** | `crossOriginIsolated` false (i.e. the HTTP path, or a browser without SAB) | `audio.js:500-507`, `:556` | **two main-thread hops per 5 ms packet** and **400 allocations/second**. There is no middle tier (a `MessageChannel` worker↔worklet would be one). | one `console.log` |
| 12 | **`prepare()` fails ⇒ no audio at all** | `AudioContext` or `addModule` failure | `audio.js:488-491`, `:540-544` | Total audio loss, because the Opus-track fallback was **already excluded from the SDP** by the time this is known | two `console.warn`s |
| 13 | **DataChannel audio → NetEQ Opus track** | client reports `!(webcodecsOpus && worklet)` | `webrtc_session.rs:697-714` | Reintroduces NetEQ's own buffering, and `forward_track` **discards `capture_ns`** so the shared timebase guarantee no longer holds | logged host-side |
| 14 | **Unordered/lossy → ordered/reliable input** | never happens by design, but `fast` drops above 8 KiB buffered | `input.js:74-78` | The event is simply gone, silently and uncounted | no |
| 15 | **Pen/touch → mouse (Windows)** | `CreateSyntheticPointerDevice` fails | `windows.rs:140-147` | Loses pressure, tilt, twist and multi-touch permanently; never re-probed | **warns once** — the best-behaved cliff in the codebase, and the model the others should follow |
| 16 | **All input → nothing (macOS)** | Accessibility not granted | `macos.rs:265-270` | Every `CGEventPost` is a **silent no-op forever**. No tier, no `CompatibilityReport` field, no re-probe. | one warning at construction, telling the user to relaunch |
| 17 | **shm → HAL input → silence (macOS legacy)** | `shm_open` fails, then the input IOProc also fails | `shm_reader.rs:200-202`, `:330-343` | `Reader` is constructed anyway and the tier **reports success while producing silence forever** | the `errno` is discarded |
| 18 | **Optimistic compatibility report** | COM init or the probe thread fails | `compatibility.rs:68`, `:90` | `Probes::optimistic()` — **"assume everything works"**, precisely when nothing could be checked | no |

---

# 7. Constants that should depend on locality

`SessionLocality` is computed 500 ms after Connected (`webrtc_session.rs:~588`), classified
`SameNetwork` / `CrossNetwork` / `Unknown`, formatted into a log line, and **thrown away**
(`major-webrtc-session-locality-is-computed-then-thrown-away.md`). The information needed to
classify is already available at `process_whep` time (`AppState::local_ips`), and the cloud
relay knows by construction that it *is* the relay. This is the largest single architectural
lever for the LAN goal, because almost every transport constant below is a compromise between
two cases that never occur together.

| Constant | `file:line` | Today | Should be, LAN | Should be, relayed | Why |
|---|---|---|---|---|---|
| `RTP_PAYLOAD_MTU` | `webrtc_session.rs:505` | **1188** everywhere | 1188 (or higher on a jumbo-frame LAN) | **~988** | A relayed path adds an SRTP tag + TURN ChannelData + outer UDP/IP on top of the 1200-byte payload. Moonlight caps `packetSize` at **1024** for remote streams "to avoid MTU-related issues like packet loss and fragmentation"; Sunshine's default is 1024. One lost IP fragment kills the whole datagram. `major-webrtc-rtp-mtu-fixed-at-1188-on-relayed-paths.md` |
| Send pacing | `webrtc_session.rs:527-545` | **none** | arguably none needed on a clean wired LAN | **leaky bucket at ~2.5× the target** | The burst is harmless into a switch and is exactly the queue that produces the tail on congested Wi-Fi. `major-webrtc-no-send-pacer.md` |
| STUN servers | `config.rs:~150` | **Google STUN always in the list** | **none** — host candidates only | STUN + TURN | A same-subnet join pays a 3 s (client) + 1.5 s (host) gather wait for candidates it will never use. `major-session-google-stun-on-every-lan-join.md` |
| `waitForIceGathering` timeout | `index.html:950` | 3000 / 6000 ms | **~200 ms** (host candidates are immediate) | 3000–6000 ms | — |
| `GATHER_MAX_WAIT` | `webrtc_session.rs:654` | 1500 ms | **~100 ms** | 1500 ms | — |
| Loss-cut threshold | `bitrate.rs:113-119` | cut only above **10 %**, probe below 2 %, hold in between | cut from ~1 % — 5 % loss on Wi-Fi is already a broken picture | same, plus a delay-gradient term | The 2–10 % dead band is where a Wi-Fi link actually lives |
| `RTT_DELAY_THRESHOLD_MS` | `bitrate.rs:120` | **40 ms above a rolling baseline** | ~10–15 ms — a LAN baseline is 1–3 ms, so 40 ms of excursion is already a broken picture | 40 ms is reasonable | The baseline-relative shape (added since discovery) is correct; the constant is a single compromise |
| `BWE_POLL_INTERVAL` | `webrtc_session.rs:43` | 120 ms, a full `get_stats` walk | can be slower | should be event-driven from TWCC | `speculative-webrtc-get_stats-polled-8-times-a-second-per-session.md` |
| Video broadcast depth | `pipeline.rs:44` | **4** (66 ms at 60 fps) | 2 — a LAN viewer that is 2 frames behind is already broken | 4 | Overflow triggers an IDR, which makes it worse |
| Audio broadcast depth | `streamer/audio/mod.rs:93` | **128** (640 ms) | **16** (80 ms) | 32–64 | 640 ms of audio backlog is not a buffer, it is a bug waiting to be heard |
| Audio backpressure | `webrtc_session.rs:46` | 16 KiB ≈ **1 s of audio** | 2–4 KiB | 16 KiB | — |
| Client jitter floor | `audio.js:6` | `MIN_TARGET_MS = 10` | 10 | 20–30 | Selkies warns that pinning to 0 causes stuttering; 10 ms is already a reasonable LAN floor |
| FEC | absent | none needed — NACK+retransmit at 1–3 ms RTT is cheaper than 20 % parity | **~20 % Reed–Solomon** (Sunshine's default) | A NACK round trip is unaffordable when RTT is high. `speculative-webrtc-no-fec-on-relayed-paths.md` |
| `playout-delay` extension | `webrtc_session.rs:505-507` | `(0,0)` unconditionally | `(0,0)` | possibly a small non-zero max | Stamped unconditionally and silently dropped by peers that did not negotiate it, with no log of which happened |
| Encoder bitrate ceiling | `pipeline.rs:~115` | `w*h*fps/10` clamped 6–30 Mbps | can be high | must be low — Parsec's own docs say "higher bitrates increase latency" | Serialization delay on the last hop dominates |
| Wired vs wireless | not detected | — | skip pacing, raise the ceiling | — | Every one of Sidecar, Luna, Duet, spacedesk and Steam Link treats wired as a first-class low-latency tier; Luna's own bench is 11.3 ms USB vs 22.4 ms Wi-Fi. `minor-session-usb-tether-is-an-undocumented-low-latency-tier.md` |

---

# 8. The twenty largest estimated contributors to glass-to-glass latency

Ranked by expected contribution to a real session on real hardware, combining magnitude with
how often the path is taken. **Rank is by estimate, not measurement** — that is the point of
`docs/BASELINE.md`.

| # | Contributor | Est. cost | Path(s) affected | `findings/` |
|---|---|---|---|---|
| 1 | **The `<video>` fallback's jitter buffer** — the WebCodecs path needs three separate APIs and any one of six triggers demotes to it, silently, with the host never told | **~90 ms** | any client below Safari 16.4 / any stall | `major-client-video-fallback-jitter-buffer.md`, `major-client-no-glass-to-glass-telemetry.md` |
| 2 | **The join chain is a fully serial multi-second sequence** — mic prompt, two ICE gather waits, a 5 s display attach, and a refresh-rate renegotiation that replays all of it | **4–22 s to first frame**, and again on every settings change | all | `major-session-join-is-a-fully-serial-multi-second-chain.md`, `major-client-join-blocks-on-ice-gathering.md`, `major-client-mic-prompt-gates-the-stream.md`, `major-client-refresh-rate-renegotiates-right-after-live.md`, `major-session-google-stun-on-every-lan-join.md` |
| 3 | **WGC is unreachable below Win11 24H2**, so most Windows hosts silently run the fully-serialised DXGI path | **+4 to +20 ms**, on the majority of hosts | Windows | `blocker-wgc-min-update-interval-forces-dxgi.md` |
| 4 | **NVENC has no encoder pipelining** — sync mode, one bitstream buffer, one slice, no sub-frame write; the K=3 ring buys nothing on the encode side | **2–6 ms every frame** | Windows/NVIDIA | `major-nvenc-sync-single-bitstream-defeats-ring.md`, `major-nvenc-single-slice-no-subframe-readback.md`, `major-nvenc-no-capability-probing.md` |
| 5 | **The WGC frame pool is never drained to the newest frame** (3 buffers, oldest-first, one `TryGetNextFrame` per callback) | **up to 33 ms of pure age** | Windows/WGC | `major-wgc-framepool-never-drains-to-the-newest-frame.md`, `major-wgc-framepool-recreate-drops-to-one-buffer.md` |
| 6 | **CGDisplayStream queue-depth starvation** — the pipeline pins 2 of the 2 queued surfaces | **potentially a full frame period, every frame** | macOS/cgds | `blocker-cgds-queue-depth-starvation.md` |
| 7 | **The bandwidth estimator is blind between RTCP reports** — `transport-cc` is advertised in the SDP answer but the TWCC *sender* interceptor is never registered (`register_default_interceptors` installs `configure_twcc_receiver_only`), so **the browser cannot generate a single TWCC feedback packet** and the host's only congestion signal is a ~1 Hz Receiver Report. `estimate_from_loss` then does nothing between 2 % and 10 % loss. | **the difference between 20 ms and 200 ms of standing queue** on a shared link | all | `major-webrtc-transport-cc-advertised-but-never-stamped.md`, `major-webrtc-bitrate-control-is-slower-and-slacker-than-the-field.md`, `major-webrtc-remb-feedback-is-received-and-discarded.md` |
| 8 | **No send pacer** — ~250 back-to-back awaited writes for an IDR into a Wi-Fi TXOP | **10–40 ms of induced queueing, and the loss that starts the PLI→IDR spiral** | all, worst on Wi-Fi | `major-webrtc-no-send-pacer.md` |
| 9 | **Capture and encode are serialised on one thread** on DXGI and on every WGC-classic backend | **the whole encode time, every frame** | Windows non-ring | `major-dxgi-path-serializes-capture-and-encode.md` |
| 10 | **The CPU bridge's `Map(MAP_READ)` readback + `UpdateSubresource` upload** — two full-frame bus traversals plus a hard GPU sync | **4–11 ms every frame** | every cpu-bridge path, i.e. every hybrid laptop that lands on the wrong adapter | `major-pipeline-win-cpu-bridge-readback-stalls-encode-thread.md`, `major-intel-cross-adapter-unimplemented-forces-cpu-bridge.md`, `major-pipeline-win-adapter-selection-has-no-runtime-policy.md` |
| 11 | **Every AMD host runs x264** | **+10 to +20 ms**, on a whole GPU vendor | Windows/AMD | `major-amd-hosts-get-no-hardware-encoder.md` |
| 12 | **The silent software VideoToolbox encoder** — the ladder degrades with no tier identity and no UI report | **+13 to +34 ms** when it happens | macOS | `major-vt-silent-software-encoder.md`, `major-vt-qos-thread-policy-returns-ignored.md` |
| 13 | **`CGDisplayStreamMinimumFrameTime` coalescing** — a hold-off, not a rate cap | **~8 ms mean, ~17 ms worst** at 60 fps | macOS/cgds | `minor-cgds-minimum-frame-time-coalescing.md` |
| 14 | **IDR-only loss recovery with a one-frame VBV** — every PLI, FIR, `Lagged` and payloader failure produces a 5–20× frame-size spike, which at 20 Mbps is 40–100 ms of extra serialization for that frame plus everything behind it. Intra-refresh is implemented on two backends and unreachable from any shipping path. | **40–100 ms per event**, and the events are self-inflicted | all | `major-nvenc-idr-only-loss-recovery.md`, `speculative-nvenc-intra-refresh-is-unreachable-from-any-shipping-path.md`, `major-webrtc-lagged-viewer-requests-idr-and-makes-it-worse.md`, `major-client-decodequeuesize-is-the-wrong-backpressure-signal.md` |
| 15 | **The first IDR is discarded before the track binds**, and SPS/PPS are silently dropped if the STAP-A exceeds the MTU — the client's own PLI is the only thing that starts a session | **one PLI round trip minimum; a permanent black screen at worst** | all | `major-webrtc-first-idr-discarded-before-track-bound.md`, `major-webrtc-sps-pps-silently-dropped-when-stap-a-exceeds-mtu.md`, `blocker-nvenc-ring-ignores-idr-on-static-desktop.md` |
| 16 | **Audio: the capture period is never negotiated** — 10 ms on Windows (never queried), ~10.67 ms on the macOS Process Tap (never set), while the legacy tier proves 2.67 ms is reachable | **+5 to +8 ms**, on the preferred tier of both platforms | all audio | `major-audio-win-engine-period-lowerable-via-render-companion.md`, `major-audio-mac-tap-aggregate-buffer-frame-size-never-set.md` |
| 17 | **Audio: one lost packet costs a full jitter buffer** — the worklet re-primes into digital silence after any underrun, and there is no gap detection or PLC in the worker | **10–40 ms of silence per lost 5 ms packet** | all audio | `major-client-one-lost-packet-costs-a-full-jitter-buffer.md`, `major-audio-core-no-fec-on-an-unreliable-datachannel.md` |
| 18 | **A/V sync rides three wrong instants** — `capture_ns` at encode/drain time, video `capture` at callback-arrival time, and `outputLatency` treated as 0 when absent | **a systematic offset of one capture block plus, on Bluetooth, 100–200 ms** | all | `major-audio-mac-capture-ns-is-drain-time.md`, `major-audio-win-capture-timestamp-taken-at-encode-time.md`, `major-pipeline-win-capture-timestamp-is-callback-arrival-not-frame-time.md`, `major-client-outputlatency-absent-breaks-avsync-on-safari-and-bluetooth.md` |
| 19 | **Intel's first bitrate change discards the entire low-latency configuration** (`Reset` with `NumExtParam = 0`), and its `BufferSizeInKB` is a multi-second VBV | **seconds of VBV**, arriving a few seconds into every Quick Sync session | Windows/Intel | `major-intel-buffersizeinkb-is-a-multi-second-vbv.md`, `major-intel-lowdelaybrc-and-maxframesize-are-noops-under-cbr.md`, `major-intel-init-warnings-swallowed.md` |
| 20 | **`SessionLocality` is computed and thrown away**, so MTU, pacing, STUN, gather waits, loss thresholds and queue depths are all a single compromise between LAN and relay | **compounds 8, 2 and 14** | all | `major-webrtc-session-locality-is-computed-then-thrown-away.md`, `major-webrtc-rtp-mtu-fixed-at-1188-on-relayed-paths.md`, `major-net-direct-link-lowlatency-mode.md` |

**Honourable mentions that are not latency but block measuring it:**
`major-webrtc-no-per-stage-encode-latency-metric.md`,
`major-pipeline-win-one-aggregate-timer-and-no-stage-breakdown.md`,
`major-client-no-glass-to-glass-telemetry.md`. Every competitor in the field publishes a
per-stage breakdown (Parsec: encode-ms / decode-ms / network; Moonlight: host processing /
network / decode / render / frame-queue delay; Steam Link: input vs display). ScreenExtend
emits one aggregate line every 60 frames on two of its eight video paths, and none at all on
the primary NVIDIA one. **Nothing above rank 7 can be verified until that is fixed.**

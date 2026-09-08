# DATAFLOW — every byte, end to end

Traced from source at commit `bef5f89`. Line numbers drift by a few lines against the
discovery reports where the tree moved after they were written; where I re-read the file
myself the current number is used and marked. Nothing here is measured — this document says
*what happens*, `docs/LATENCY-MAP.md` says *what it costs*.

## Legend

Every buffer transfer is classified. These five labels are used consistently:

| Label | Meaning |
|---|---|
| **ZC** | Zero-copy. A pointer, a refcount bump, or an `ArcSwap` store. No bytes move. |
| **G→G** | GPU→GPU copy. `CopyResource`, `CopySubresourceRegion`, `VideoProcessorBlt`, `RunFrameVPPAsync`. Full-frame unless noted. |
| **G→C** | GPU→CPU readback. `Map(D3D11_MAP_READ)` + memcpy. A hard CPU↔GPU sync point. |
| **C→G** | CPU→GPU upload. `UpdateSubresource`. |
| **CPU** | CPU memcpy or per-sample loop within one address space. |
| **ALLOC** | A heap allocation on the per-frame / per-packet path. |

Queue notation: `type(depth) — behaviour on full`.

Thread notation names the OS thread or the executor, and its priority class where the code
sets one.

---

# PART 1 — THE PIXEL PATH

## 1.0 The shared spine (all eight video paths converge here)

Every capture/encode backend terminates in the same three objects, so they are described once.

```
  [encode backend]                            [transport]                    [client]
        |                                          |                             |
        v                                          |                             |
  EncodedFrame{ data: Bytes, capture: Instant }    |                             |
        |                                          |                             |
        +--> tokio::sync::broadcast(4) ------------+                             |
                  |  no backpressure               |                             |
                  |  oldest evicted -> Lagged      |                             |
                  v                                v                             v
             RTP writer task (one per viewer) --> SRTP/DTLS/UDP --> browser depacketizer
```

| # | Stage | Function (`file:line`) | Thread | Buffers | Queues / locks |
|---|---|---|---|---|---|
| S1 | Encoded AU lands | per-backend, see below | encode thread | `Bytes::from(Vec<u8>)` — **ZC** move of an already-allocated Vec | — |
| S2 | Fan-out | `tx.send(EncodedFrame)` — `windows_utils/streamer/pipeline.rs:44` (`BROADCAST_CAPACITY = 4`), `:1266`; `macos_utils/streamer/pipeline.rs:28`, `:86` | encode / drain thread | `Bytes` clone per subscriber = refcount only — **ZC** | `tokio::sync::broadcast<EncodedFrame>(4)` — **never blocks**; a receiver >4 frames behind loses the oldest and gets `RecvError::Lagged`. **No back-pressure on the encoder at all.** |
| S3 | Writer wakes | `webrtc_session.rs:500-561` (spawned per WHEP offer) | tokio worker on the per-adapter runtime; `on_thread_start(platform::tune_transport_thread)` → Windows MMCSS "Playback" + `THREAD_PRIORITY_ABOVE_NORMAL` (`tuning.rs:206-209`, both results discarded), macOS `pin_current_thread_user_initiated` | — | `Lagged` → `pipeline.request_idr()` (`:550-553`) — a positive-feedback loop |
| S4 | RTP timestamp | `host_ns_to_rtp90k(host_instant_to_ns(frame.capture))` `webrtc_session.rs:511-512` | same | — | reads the process-wide `HOST_EPOCH: OnceLock<Instant>` (`streamer/audio/mod.rs:17-33`) — the **one clock** audio `capture_ns` and video RTP both ride |
| S5 | Payloading | `H264Payloader::payload(RTP_PAYLOAD_MTU, &frame.data)` `webrtc_session.rs:505` (`1200-12 = 1188`), `:516` | same | NAL ≤ MTU: `nalu.clone()`, a `Bytes` slice — **ZC**. NAL > MTU: `BytesMut::with_capacity(2+frag)` per fragment — **ALLOC + CPU** ~1186 B each. SPS/PPS: `Vec::with_capacity` + two `extend_from_slice` — **ALLOC + CPU**. `payloads` is `vec![]` with no capacity hint, so it reallocs as it grows. | A 16 KB P-frame → ~14 allocs + 16 KB copied. A 300 KB IDR → ~250 allocs + 300 KB copied, **per viewer** (the payloader is constructed inside the per-session writer task at `:504`). |
| S6 | Per-packet write | `write_rtp_with_extensions(&pkt, &[PlayoutDelay(0,0)])` `webrtc_session.rs:527-545` | same | `let mut pkt = p.clone()` + a header-extension `Vec` — **ALLOC** per packet per binding (`webrtc-0.17.1/.../track_local_static_rtp.rs:98`, `:131-150`) | **Awaited per fragment. No pacer anywhere.** A 300 KB IDR is ~250 back-to-back awaited sends into the SRTP/DTLS/UDP path with no spacing. Writing with **no bindings returns `Ok(0)`** silently (`track_local_static_rtp.rs:100-123`). |
| S7 | Interceptors | `register_default_interceptors` `webrtc_session.rs:~175` | same | NACK responder retains 8192 packets (~10 MB of send history) | TWCC **receiver only** — `configure_twcc_receiver_only`; the sender interceptor is never added, so `transport-cc` is advertised and never stamped |
| S8 | SRTP + socket | inside `webrtc-rs` | same | one more buffer + AES — **ALLOC + CPU** | UDP `send_to` |

### Client, video (both paths)

```
UDP -> DTLS/SRTP -> browser depacketizer -> complete access unit
                                                  |
              +-----------------------------------+-------------------------+
              | fast path (WebCodecs)                                        | fallback
              v                                                             v
  RTCRtpScriptTransform (Safari) / createEncodedStreams (Chromium)   video.srcObject
              |  transferred ReadableStream                                 |
              v                                                     libwebrtc jitter buffer
      transform-worker.js  (dedicated worker thread)                        |
              |                                                             v
        new EncodedVideoChunk({data})  <-- ALLOC + CPU, spec-mandated copy   <video> element
              |
        decoder.decode()  (VideoDecoder, optimizeForLatency: true)
              |
        output callback -> ctx.drawImage(frame,0,0,w,h)  <-- 5-arg scaled form
              |            (OffscreenCanvas 2D, {alpha:false, desynchronized:true})
              |            NO requestAnimationFrame, NO requestVideoFrameCallback
        frame.close()  (in a `finally`, so no VideoFrame leak)
              v
          compositor
```

| # | Stage | Function (`file:line`) | Thread | Buffers | Queues |
|---|---|---|---|---|---|
| C1 | Transform attach | `receiver.transform = new RTCRtpScriptTransform(...)` `index.html:862` **or** `receiver.createEncodedStreams()` `:866` | page main | — | — |
| C2 | Latency hints | `receiver.playoutDelayHint = 0` `index.html:832`; `receiver.jitterBufferTarget = 0` `:874` | page main | — | both feature-guarded, both **no-ops on WebKit** |
| C3 | Chunk build | `new EncodedVideoChunk({ data: encodedFrame.data })` `transform-worker.js:252-256` | transform worker | **ALLOC + CPU**, one per frame (the spec copies here) | — |
| C4 | Backlog gate | `startPump` `transform-worker.js:199`, `:229-243` | transform worker | — | `decoder.decodeQueueSize > max(3, fps*0.05)` twice ⇒ stop decoding, latch `waitingForKey`, request a key. `requestKey()` is a **no-op unless `transformer` exists** (`:182`) — i.e. dead on the Chromium `createEncodedStreams` branch. |
| C5 | Decode | `makeDecoder()` `transform-worker.js:105`, `ensureConfigured()` `:135-157` | UA decoder threads | — | `isConfigSupported` probed first; `configure({codec, optimizeForLatency:true})`. **No `hardwareAcceleration` hint.** |
| C6 | Paint | output callback → `ctx.drawImage(...)` `transform-worker.js:110-126` | transform worker | canvas size latched on first frame | Painting inline in the decoder callback, off the main thread, is the single best latency decision in the client. |
| C7 | A/V sync report | `reportAvSync(tsTicks)` `transform-worker.js:57-93` | transform worker | — | unwraps the 32-bit RTP timestamp, asymmetric min-tracking EMA, posts `avsync` at most every 200 ms. **Compares a host monotonic epoch against a client Unix epoch — the absolute value is not a latency.** |
| C8 | Fallback | `fallbackToVideo(reason)` `index.html:736-742` | page main | `video.srcObject = e.streams[0]` `:921` — the UA owns everything | Reintroduces the full libwebrtc receive path including the jitter buffer. One-way (`usingFallback` latch). |

---

## 1.1 Windows · WGC · NVENC texture ring (the default NVIDIA path)

```
 DWM/compositor
      |
      v  writes into one of 3 pool surfaces (GPU)
 Direct3D11CaptureFramePool (numberOfBuffers = 3)
      |  FrameArrived -> DispatcherQueue -> thread msg queue -> GetMessageW -> DispatchMessageW
      v
 [WGC capture thread]  MMCSS ProAudio/Games/Playback @ AVRT_PRIORITY_CRITICAL
                       + THREAD_PRIORITY_TIME_CRITICAL, boost disabled
      |
      |  TryGetNextFrame()  -> dequeues the OLDEST pooled frame; called ONCE, no drain loop
      |  2x COM cast + GetDesc + Arc::new(AtomicBool)  [ALLOC, per frame]
      |  parking_lot handler mutex held for the whole callback
      v
 AsyncNvencRing::stage()
      |  free_slots.try_recv()  -- bounded(3), NON-BLOCKING; empty => frame dropped silently
      |  AcquireSync(KEY_WRITER, 1000ms)          <-- keyed mutex, on the capture thread
      |  scale_into() [G->G blt]  OR  CopyResource() [G->G full frame]
      |  igpu_context.Flush()                     <-- forced GPU submit, every frame
      |  ReleaseSync(KEY_ENCODER)
      |  ready.send(ReadyMsg{slot, force_idr, capture})   -- bounded(3)
      v
 [nvenc-ring-encode thread]  same MMCSS tuning
      |  ready.recv_timeout(200ms)                <-- 200ms keepalive re-encode on timeout
      |  apply_pending_bitrate_atomic()           <-- NO-OP when qp.is_some() (CONSTQP)
      |  AcquireSync(KEY_ENCODER, 1000ms)
      |  nvEncMapInputResource()                  [ZC]
      |  nvEncEncodePicture()                     <-- submit
      |  nvEncLockBitstream()                     <-- BLOCKS for the whole encode
      |  slice::to_vec()                          [ALLOC + CPU, from write-combined memory,
      |                                             INSIDE the bitstream lock]
      |  nvEncUnlockBitstream / Unmap / ReleaseSync(KEY_WRITER)
      |  release_and_hold()  -- returns the PREVIOUS slot; keeps the current one for keepalive
      v
 Bytes::from(au) -> broadcast(4)  [see 1.0]
```

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Queue / lock |
|---|---|---|---|---|---|
| 1 | Pool alloc | `Direct3D11CaptureFramePool::Create(&device, fmt, 3, item.Size())` `windows_capture/graphics_capture_api.rs:197` | session start | 3 GPU surfaces, owned by WinRT | bounded(3), FIFO. `Recreate` on a size change passes **1** (`:286-291`) — a latent one-buffer regression |
| 2 | Delivery | `TypedEventHandler` → `DispatcherQueue` → `run_message_loop` `windows_capture/capture.rs:35`, `:409` | WGC capture thread (spawned `capture.rs:401`) | — | Win32 message queue, unbounded, OS-managed |
| 3 | Dequeue | `TryGetNextFrame()` `graphics_capture_api.rs:264` | WGC capture thread | **ZC** — returns the *oldest* buffered frame; **called once, no drain loop** | — |
| 4 | Surface unwrap | `cast::<IDirect3DDxgiInterfaceAccess>()` + `GetInterface::<ID3D11Texture2D>()` `graphics_capture_api.rs:273-275`; `GetDesc` `:279` | WGC capture thread | **ZC** — 2 QueryInterface (2 AddRef) + 1 D3D call | — |
| 5 | Per-frame control | `Arc::new(AtomicBool)` for `InternalCaptureControl` `graphics_capture_api.rs:314` | WGC capture thread | **ALLOC** — one heap alloc per frame inside the callback | — |
| 6 | Handler lock | `callback_frame_pool.lock()` `graphics_capture_api.rs:318` | WGC capture thread | — | `parking_lot::Mutex<LiveCapture>`, **held for the entire callback** |
| 7 | Capture stamp | `Instant::now()` `pipeline.rs:1694` | WGC capture thread | — | The WGC `SystemRelativeTime()` QPC stamp is available (`frame.rs:139`) and **never read** |
| 8 | Slot acquire | `free_tx/free_rx.try_recv()` `pipeline.rs:1149` | WGC capture thread | — | `crossbeam_channel::bounded(K=3)` (`pipeline.rs:1102`, `RING=3` at `:1593`), prefilled 0..K. **`try_recv`, so on empty the frame is dropped and `Ok(false)` returned — the caller ignores the bool (`:1703`). No log, no counter.** `release_and_hold` permanently withholds one slot, so the effective producer pool is **2**. |
| 9 | Writer lock | `mutex.AcquireSync(KEY_WRITER, KEY_TIMEOUT_MS)` `pipeline.rs:1185` (`KEY_TIMEOUT_MS = 1000`) | WGC capture thread | — | `IDXGIKeyedMutex` per slot. A wedged slot blocks a TIME_CRITICAL callback thread for a full second. |
| 10 | Stage into slot | `scaler.scale_into(raw, dst)` **or** `igpu_context.CopyResource(dst, raw)` `pipeline.rs:1164-1174` | WGC capture thread | **G→G**, full frame, unconditional. "Zero-copy" here means zero *CPU* copies. | — |
| 11 | Submit | `igpu_context.Flush()` `pipeline.rs:1172` | WGC capture thread | — | Forces a GPU submit every frame (needed for the cross-adapter share) |
| 12 | Handoff | `ReleaseSync(KEY_ENCODER)` `:1175`; `idr_request.swap(false)` `:1187`; `ready.send(ReadyMsg)` `:1194`; `last_frame_at.store` `:1198` | WGC capture thread | `ReadyMsg { slot: usize, force_idr: bool, capture: Instant }` `pipeline.rs:1063` | `crossbeam_channel::bounded(K=3)`; cannot block (bounded by `free_slots`) |
| 13 | Encode wake | `ready.recv_timeout(200ms)` `pipeline.rs:1252` | `nvenc-ring-encode` (`pipeline.rs:1114-1128`), MMCSS-tuned at `:1228` | — | On timeout: `encode_repeat_slot(held)` — a P-frame keepalive that **cannot produce an IDR**, because it reads the dead `Encoder::idr_requested` (`encoder.rs:82`, only writer is the callerless `request_idr` at `:493`) |
| 14 | Bitrate | `apply_pending_bitrate_atomic` `pipeline.rs:1328` | ring encode thread | — | `AtomicU32::swap(0)` sentinel. **`Encoder::set_bitrate` early-returns `Ok(())` when `qp.is_some()` (`encoder.rs:460-463`)**, and the caller then logs a bitrate change that did not happen. |
| 15 | Encoder lock | `AcquireSync(KEY_ENCODER, 1000)` `nvidia/encoder.rs:314` | ring encode thread | — | `IDXGIKeyedMutex`, the other half of stage 9 |
| 16 | Map | `nvEncMapInputResource` `nvidia/encoder.rs:726-732` | ring encode thread | **ZC** — the resource was registered once at construction (`:679`, `bufferFormat = ARGB`, `pitch = 0`) | per frame |
| 17 | Encode | `nvEncEncodePicture` `nvidia/encoder.rs:770` | ring encode thread + GPU | **ZC** — NVENC reads the ARGB surface directly and does RGB→YUV internally. **No CPU colour conversion anywhere.** | `enableEncodeAsync = 0`, `completionEvent = null`, **one** bitstream buffer (`create_bitstream`, `:699`) ⇒ **exactly one picture in flight** |
| 18 | Readback | `nvEncLockBitstream` `nvidia/encoder.rs:781` | ring encode thread | — | `doNotWait = 0` ⇒ **blocks for the full frame encode**. `sliceMode = 0` (one slice), `enableSubFrameWrite` unset, `reportSliceOffsets` unset. `getRCStats` unset, so `frameAvgQP` / `pictureType` / MB counts are available and discarded. |
| 19 | AU copy | `slice::from_raw_parts(...).to_vec()` `nvidia/encoder.rs:789-795` | ring encode thread | **ALLOC + CPU** — one heap alloc + a memcpy out of (typically write-combined) driver memory, **inside the bitstream lock** | ~25 KB typical |
| 20 | Release | `nvEncUnlockBitstream` `:797`, `nvEncUnmapInputResource` `:736`, `ReleaseSync(KEY_WRITER)` `:320` | ring encode thread | — | All four `ReleaseSync` results are `let _ =` discarded; a failed release wedges the slot forever |
| 21 | Slot recycle | `release_and_hold` `pipeline.rs:1235-1246` | ring encode thread | — | Returns the *previous* slot, keeps the current one for the 200 ms keepalive |
| 22 | Publish | `Bytes::from(au)` `pipeline.rs:1267` → `tx.send` `:1266-1269` | ring encode thread | **ZC** ownership move | broadcast(4) — see §1.0 |

**Locks held on this path:** the `parking_lot` handler mutex (stage 6, whole callback), two
`IDXGIKeyedMutex` acquisitions per frame (stages 9 and 15, 1 s timeout each). The
`Arc<Mutex<EncodeCore>>` that guards the classic path is **`None` on the ring path**
(`pipeline.rs:1554-1609` returns before it is built).

**Instrumentation:** none. The `LiveCapture::{frames_sent, timing_sum_ns, timing_max_ns}`
counters and the `encode-path latency:` line (`pipeline.rs:1742-1761`) are unreachable — the
ring branch returns at `:1708`, before their only reader.

---

## 1.2 Windows · WGC · NVENC CPU bridge (`Backend::Nvenc { path: CpuBridge }`)

Reached when `build_zero_copy_ring` fails and `build_zero_copy` also fails
(`pipeline.rs:1611-1616` → `build_backend` → `try_nvenc`). Stages 1–7 are identical to §1.1.
From there:

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Queue / lock |
|---|---|---|---|---|---|
| 8 | Core lock | `core.lock().expect("encode core mutex poisoned")` `pipeline.rs:1716` | **WGC capture thread** | — | `Arc<std::sync::Mutex<EncodeCore>>` (`pipeline.rs:984`), **held across the entire encode, inside a WinRT `extern "system"` callback**. Contended with the `nvenc-repeat` thread (`:1504`). A panic under it poisons the lock and unwinds through the FFI boundary. |
| 9a | *With a scaler* | `Scaler::scale(raw)` `scaler.rs:~150` | WGC capture thread | **G→G** `VideoProcessorBlt` into an owned destination texture, cached views keyed on raw pointer identity | — |
| 9b | | `Scaler::read_back()` `scaler.rs:241-259` | WGC capture thread | **G→C** — `CopyResource` into a lazily-created staging texture, `Map(D3D11_MAP_READ)`, memcpy into a **retained** `Vec` (no per-frame alloc after warm-up). 8.29 MB at 1080p. | `Map` is a **hard GPU sync point**: the CPU blocks until the preceding copy retires. No double-buffered staging, no `MAP_FLAG_DO_NOT_WAIT` retry. |
| 9c | *Without a scaler* | `frame.buffer()` `windows_capture/frame.rs:187` | WGC capture thread | **ALLOC + G→C** — `CreateTexture2D` **per frame** + `CopyResource` + `Map` | Same sync point, plus a full-resolution texture allocation every frame |
| 10 | Upload | `Encoder::encode_bgra_padded` → `UpdateSubresource` `nvidia/encoder.rs:392-423` | WGC capture thread | **C→G** — a full `w*h*4` upload into a `USAGE_DEFAULT` texture. 8.29 MB at 1080p, ~500 MB/s at 60 fps. | — |
| 11 | Encode | `encode_mapped` → stages 16–20 of §1.1 | **WGC capture thread** | as §1.1 | Encode runs inline on the capture thread ⇒ **capture of frame N+1 cannot begin until encode of frame N completes** |
| 12 | Transient retry | `is_transient_encode_error(&e)` `pipeline.rs:84-86` | WGC capture thread | — | Matches the literal substring `"device busy"`. **NVENC never emits it**, so `MAX_TRANSIENT_ENCODE_DROPS = 60` (`:103`) and the whole retry branch are dead for NVENC; an encode error aborts the capture (`:1732`). |
| 13 | Publish | `tx.send` `pipeline.rs:1738` + the 60-frame `encode-path latency:` summary `:1742-1761` | WGC capture thread | as §1.0 | This is the **only** latency line the Windows backend emits, and it lumps callback→broadcast into one aggregate |
| ↺ | Idle keepalive | `spawn_repeater` `pipeline.rs:1460-1525` (`nvenc-repeat` thread) | own thread, MMCSS-tuned | `EncodeCore::encode_repeat` re-encodes the last input | Waits `max(frame_duration, 8ms)` on `crossbeam_channel::bounded(())(1)`, drains it, skips if a real frame landed within `max(2*frame_duration, 34ms)`, emits only on a pending IDR or after 200 ms |

---

## 1.3 Windows · DXGI Desktop Duplication · NVENC

The path that actually runs on **every host below Windows 11 build 26100**, because
`GraphicsCaptureApi::new` turns an absent `MinUpdateInterval` property into a hard `Err`
(`graphics_capture_api.rs:148-176`) and `start_on_monitor` falls through
(`pipeline.rs:274-277`).

```
 [dxgi-capture thread]  MMCSS-tuned (pipeline.rs:460) + KeepAwake (:461)
      |
      |  timeout_ms = dirty ? (next_due - now) : 8      <-- flat 8 ms idle poll
      v
 Duplicator::poll(timeout_ms)                  dxgi/mod.rs:397-482
      |  AcquireNextFrame  (a WAIT, not a poll)
      |  LastPresentTime == 0 ?  -> Timeout
      |  present_region_is_empty()  <- GetFrameMoveRects + GetFrameDirtyRects, reduced to a bool
      |  CopyResource(desktop, dup_tex)         [G->G FULL FRAME, UNCONDITIONAL]
      |  GetFramePointerShape() if PointerShapeBufferSize > 0
      |  ReleaseFrame()  (result discarded)
      v
 pace against next_due  (pipeline.rs:629)      <-- a frame just past next_due waits a FULL period
      |
      v
 Duplicator::frame()                           dxgi/mod.rs:534-567
      |  fast path: return &desktop        (rotation identity AND cursor off)
      |  else: CopyResource(composite, desktop) or un-rotate blit   [G->G SECOND FULL FRAME]
      |        + 1-2 cursor quad draws (D3DCompile'd HLSL, point sampler, 3 blend states)
      v
 core.lock()  ->  EncodeCore::encode_texture()   pipeline.rs:599
      |  cpu-bridge?  scaler.read_back() [G->C]  or TextureReader::read_back() [G->C]
      |  else encode_gpu -> NVENC as in 1.1 stages 16-20
      v
 tx.send  (pipeline.rs:623)
```

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Queue / lock |
|---|---|---|---|---|---|
| 1 | Wait | `dup.poll(timeout_ms)` `pipeline.rs:583-589` → `dxgi/mod.rs:397` | `dxgi-capture` (spawned `pipeline.rs:407-410`) | — | `AcquireNextFrame` is a blocking wait. Timeout is `next_due - now` when a frame is pending, else a **flat 8 ms** — so an idle→active transition costs up to 8 ms of extra wait. `was_idle` resets `next_due` on the edge (`pipeline.rs:567-572`), but only after that first 8 ms elapses. |
| 2 | Change gate | `present_region_is_empty(dup, info)` `dxgi/mod.rs:349-395` | dxgi-capture | move-rect + dirty-rect arrays are fetched into reused `meta_buf` and **reduced to one boolean** | `AccumulatedFrames > 1` ⇒ "not empty". `RectsCoalesced` and `ProtectedContentMaskedOut` are never read. |
| 3 | Duplication copy | `CopyResource(desktop, dup_tex)` `dxgi/mod.rs:426` | dxgi-capture + GPU | **G→G**, full frame, **unconditional** | — |
| 4 | Cursor shape | `GetFramePointerShape` into reused `shape_buf` `dxgi/mod.rs:~440` | dxgi-capture | reused buffer; on a *shape change* `build_sprite` allocates 1–2 `Vec<u32>` of `w*h` plus immutable textures + SRVs (`dxgi/cursor.rs:294`, `:341`, `:372`) — **ALLOC**, per-shape-change, which an animated spinner makes per-frame | — |
| 5 | Release | `let _ = dup.ReleaseFrame()` `dxgi/mod.rs:478` | dxgi-capture | — | Result discarded. MSDN's guidance is to release immediately before the next acquire; this code does. |
| 6 | Pacing | `if now < next_due { … }` `pipeline.rs:611`, `next_due = now + frame_duration` `:629` | dxgi-capture | — | A frame that arrives just *after* `next_due` waits a **full frame period** |
| 7 | Composite | `Duplicator::frame()` `dxgi/mod.rs:534-567` | dxgi-capture + GPU | **G→G** second full-frame copy (`CopyResource(composite, desktop)`) or an un-rotate blit, **whenever the cursor is visible or the display is rotated** — i.e. essentially always | plus 1–2 quad draws |
| 8 | Core lock | `core.lock()` `pipeline.rs:599` | dxgi-capture | — | `Arc<Mutex<EncodeCore>>`, held across the whole encode, contended with `nvenc-repeat` |
| 9 | Encoder input | `EncodeCore::encode_texture` — `TextureReader::read_back` when there is no scaler (`scaler.rs:~300`) | dxgi-capture | **G→C** on any cpu-bridge backend; **ZC**→NVENC-shared-texture on the GPU path (a *third* full-frame copy into the keyed-mutex texture) | — |
| 10 | Encode + publish | as §1.1 stages 16–22, but **inline on the capture thread** | dxgi-capture | — | **No capture/encode overlap at all.** Capture N+1 waits for encode N. |
| — | Telemetry | `encode-path latency: path=…, avg_ms=…, max_ms=…, frames=…` every 60 frames `pipeline.rs:631-649` | dxgi-capture | — | One aggregate number covering poll→broadcast |
| — | Loss of duplication | `Duplicator::redup()` `dxgi/mod.rs:~300-340` | dxgi-capture | — | `DXGI_ERROR_ACCESS_LOST` → drop the duplication, report `Timeout`, re-establish with 100 ms backoff, up to `REDUP_MAX_RETRIES = 40`. Only the success is logged. |

The DXGI device is created **on the adapter that enumerates the output**
(`find_output` `dxgi/mod.rs:73-108`, `create_device_on` `:118`), because duplication is
adapter-local. It tries `BGRA|VIDEO` and silently retries `BGRA` only (`:137-143`); losing
`VIDEO_SUPPORT` here is what forces the scaler and the CPU bridge.

---

## 1.4 Windows · Intel Quick Sync, same adapter (`Backend::Intel`)

`wants_prescale()` returns **false** for `Backend::Intel` (`pipeline.rs:~111`), so no external
scaler runs — the downscale is fused into the VPP pass. Capture is WGC or DXGI as above.

```
 capture texture (BGRA, capture device)
      |
      v  MFXMemory_GetSurfaceForVPP -> FrameInterface::GetNativeHandle -> ID3D11Texture2D
 copy_into_surface()                              intel/encoder.rs:657
      |  [no bridge]   CopySubresourceRegion + Flush            [G->G FULL FRAME]
      |  [bridge]      AcquireSync(KEY_WRITER,1000) + CopySubresourceRegion + Flush
      |                + ReleaseSync(KEY_ENCODER), then a SECOND CopySubresourceRegion [G->G x2]
      v
 RunFrameVPPAsync()   BGRA -> NV12 + downscale, ONE fused pass   intel/encoder.rs:612-648
      |  1 ms sleep retry on MFX_ERR_DEVICE_BUSY, max 30
      |  ** the VPP sync point is NEVER synchronised **
      v
 EncodeFrameAsync()   AsyncDepth = 1                            intel/encoder.rs:739
      |  same busy-retry
      v
 SyncOperation(100 ms) in a loop bounded at 2000 ms             <-- BLOCKING, full GPU encode
      |
      v
 bitstream_buf[start..end].to_vec()   [ALLOC + CPU]             intel/encoder.rs:801-805
      |
      v  replace_last_nv12()  <-- retains ONE NV12 surface for encode_repeat
 Bytes -> broadcast(4)
```

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Queue / lock |
|---|---|---|---|---|---|
| 1 | Surface fetch | `get_surface(VppIn)` → `MFXMemory_GetSurfaceForVPP` `intel/encoder.rs:808` | capture thread | runtime-owned, refcounted via `mfxFrameSurfaceInterface` | The import is backwards from a zero-copy design: rather than wrapping our texture, the code asks the runtime for a surface and copies into it |
| 2 | Copy in | `copy_into_surface` `intel/encoder.rs:657-733` | capture thread | **G→G** full frame + `Flush`. With an `InputBridge` (capture device is Intel but lacks `VIDEO_SUPPORT`): **G→G ×2** plus **two keyed-mutex round trips**, 1 s timeout each | `IDXGIKeyedMutex`, `KEY_TIMEOUT_MS = 1000` (`intel/encoder.rs:32`); both `ReleaseSync` results `let _ =` discarded |
| 3 | VPP | `RunFrameVPPAsync` `intel/encoder.rs:612-648` | capture thread + GPU | **G→G**, colour convert + scale **fused into one pass** — the good part of this backend | `AsyncDepth = 1` on both VPP and ENCODE. Busy retry: `sleep(1ms)` × 30. |
| 4 | Encode | `EncodeFrameAsync` `intel/encoder.rs:739` | capture thread + GPU | **ZC** — encode reads the VPP output NV12 surface | `mfxEncodeCtrl{I|IDR|REF}` built only when forcing |
| 5 | Sync | `MFXVideoCORE_SyncOperation(100ms)` loop, 2000 ms cap `intel/encoder.rs:784-798` | capture thread | — | **Blocks the thread for the full GPU encode.** `AsyncDepth = 1` means there is no pipelining to hide it. |
| 6 | AU copy | `self.bitstream_buf[start..end].to_vec()` `:801-805` | capture thread | **ALLOC + CPU**, compressed size only; `bitstream_buf` is a reused `Vec` sized from the effective `BufferSizeInKB` (often ~8 MB) | — |
| 7 | Retain | `replace_last_nv12(surf)` `:845` | capture thread | one NV12 surface held across frames, refcounted; protected only by the outer `EncodeCore` mutex | — |
| — | Bitrate change | `Encoder::set_bitrate` `intel/encoder.rs:504` | BWE task → capture thread | — | Calls `MFXVideoENCODE_Reset` with **`ExtParam = null, NumExtParam = 0`** (`:512-513`) — every `CodingOption`/`CO2`/`CO3` setting (`LowDelayBRC`, `WinBRC*`, `MaxFrameSize*`, `MaxDecFrameBuffering`, `RepeatPPS`, `ScenarioInfo`, …) **reverts to default at the first bitrate change**. |
| — | Warnings | `check(status, what)` `intel/intel_sys.rs:687` | — | — | `status >= 0` is `Ok`. **Every positive `mfxStatus` warning is swallowed silently**, including `MFX_WRN_PARTIAL_ACCELERATION` (software fallback inside the runtime) and `MFX_WRN_INCOMPATIBLE_VIDEO_PARAM` (the runtime changed your parameters). |

---

## 1.5 Windows · Intel CPU bridge (`Backend::IntelCpu`)

Reached when `Encoder::new_on_device` **fails** — most commonly because the capture device is
not Intel at all ("cross-adapter Quick Sync bridge is not implemented",
`intel/encoder.rs:200-206`). `build_backend` catches the bail and falls through with one
`teprintln!` (`pipeline.rs:1399-1412`).

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership |
|---|---|---|---|---|
| 1 | Scale | `Scaler::scale` `scaler.rs:~150` | capture thread + GPU | **G→G** `VideoProcessorBlt` |
| 2 | Stage | `CopyResource` into the staging texture `scaler.rs:243` | capture thread + GPU | **G→G** |
| 3 | Readback | `Map(D3D11_MAP_READ)` + `copy_nonoverlapping` of `RowPitch*height` `scaler.rs:243-259` | capture thread | **G→C**, 8.29 MB at 1080p, into a retained `Vec`. **A full CPU↔GPU sync.** |
| 4 | Upload | `ensure_upload_texture()` + `UpdateSubresource` `intel/encoder.rs:561-570`, `:585` | capture thread | **C→G**, 8.29 MB |
| 5 | Into VPP | `CopySubresourceRegion` `intel/encoder.rs:695-716` | capture thread + GPU | **G→G** |
| 6–9 | VPP → encode → sync → `to_vec` | as §1.4 stages 3–6 | capture thread | — |

**Three full-frame copies and two bus traversals per frame**, all serial on one thread.

---

## 1.6 Windows · x264 software fallback

Reached by `EncoderVendor::Software` (the `disableGpuEncode` toggle) or as the last rung of the
`Auto` ladder. Input is the same **G→C** readback as §1.5 stages 1–3.

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Notes |
|---|---|---|---|---|---|
| 1 | Readback | `Scaler::read_back` / `TextureReader::read_back` / `Frame::buffer` | capture thread | **G→C**, 8.29 MB, blocking | — |
| 2a | *BGRA path* | `repeat_bgra.extend_from_slice(data)` `x264/encoder.rs:~163` | capture thread | **CPU**, a **full-frame 8.29 MB memcpy per frame**, then libx264 copies it again internally | Selected by trying `x264_encoder_open` with `i_csp = X264_CSP_BGRA` first and falling back on null (`:113-125`). **A 4:4:4-capable libx264 will emit High 4:4:4 Predictive from this input, which browsers cannot decode** — `blocker-x264-bgra-input-emits-444-stream.md`. |
| 2b | *I420 path* | `convert_bgra_to_i420` `x264/encoder.rs:309` | `x264-csc-0..N` rayon pool | writes into a preallocated `i420: Vec<u8>` — **no per-frame alloc**, but a full scalar BT.601 pass (2.07 M luma + 0.52 M chroma samples at 1080p) | Pool is `(cores/2).clamp(1,4)` threads at `THREAD_PRIORITY_ABOVE_NORMAL`, `OnceLock`, **process-wide and shared across concurrent sessions** (`:372-391`). It is sized independently of `encode_thread_count()`'s `clamp(1,8)`, so an 8-core box requests 12 threads between the two. |
| 3 | Encode | `x264_encoder_encode` `x264/encoder.rs:207`/`:255` | capture thread + libx264 slice threads | — | `ultrafast` + `zerolatency`: `i_bframe = 0`, `rc.i_lookahead = 0`, `i_sync_lookahead = 0`, `b_sliced_threads = 1`, `i_threads = min(cores, 8)`. **Frame threading is genuinely off** — frame in, packet out. |
| 4 | AU copy | `slice::from_raw_parts((*nals).p_payload, size).to_vec()` `:251`, `:305` | capture thread | **ALLOC + CPU** | Relies on libx264 laying all NALs contiguously in `h->nal_buffer` (it does), but the code never walks `n_nal`. |
| ↺ | Idle repeat | `encode_repeat` `x264/encoder.rs:178` | `nvenc-repeat` thread | BGRA path does `repeat_bgra.clone()` — **another 8.29 MB ALLOC + CPU per idle tick** | — |
| — | Unused | `nalu_process` (`x264_sys.rs:295`), `i_slice_max_size` / `i_slice_count` (`:286-290`), `i_nal_hrd` (`:207`) | — | — | `nalu_process` is x264's free sub-frame write and is never set. |

---

## 1.7 macOS · CGDisplayStream · VideoToolbox

```
 WindowServer composites, SCALES to the requested w x h, converts to '420f' (full-range 4:2:0)
      |  (no kCGDisplayStreamSourceRect / DestinationRect, so the whole display is scaled)
      |  kCGDisplayStreamMinimumFrameTime = (1/fps) * 0.95   <-- HOLD-OFF, not a rate cap
      |  kCGDisplayStreamQueueDepth = 2
      v
 [com.screenextend.cgds]  serial DispatchQueue targeting the global UserInteractive queue
      |
      |  handle_frame()                              cgds.rs:102-156
      |    status != FrameComplete            -> return (silent, no counter)
      |    DirtyRects count == 0              -> return (silent, no counter)
      |    CFRetained::retain(IOSurface)      [ZC, +1 retain]
      |    CVPixelBufferCreateWithIOSurface   [ZC wrap, 1 CF object ALLOC per frame]
      |    displayTime block parameter        -> DISCARDED
      |    sink.publish(Frame{ arrived_mach, captured_at: Instant::now(), backing })  [1 Arc ALLOC]
      v
 FrameSink: ArcSwapOption<Frame>  (depth 1, latest-wins, LOCK-FREE) + crossbeam bounded(1) wake
      |
      v
 [videotoolbox-encode]  QOS_USER_INTERACTIVE
                        + THREAD_TIME_CONSTRAINT(period = frame_duration,
                                                 computation = period/8,
                                                 constraint <= 4 ms, preemptible)
                        + THREAD_PRECEDENCE importance 63
                        + THREAD_AFFINITY tag 1 (x86_64 only)
                        + os_workgroup_join (dlsym'd)
                        + KeepAwake (2 IOPMAssertions)
      |  source.wait(4 ms)                            <-- 4 ms poll floor
      |  idr_request.swap / target_bitrate.swap
      |  try_take_latest() -> Arc<Frame>, HELD in `last_frame`
      |     ** => 2 of the 2 queueDepth surfaces are pinned in steady state **
      |  pending_ts.lock().unwrap().push_back(captured_at)
      |  Encoder::submit(image_buffer, force_idr)     [ZC into VTCompressionSessionEncodeFrame]
      |     ASYNCHRONOUS: returns as soon as VT accepts the frame
      |  if no frame and (IDR pending or 200 ms elapsed): re-submit last_frame,
      |     then complete_pending() -> VTCompressionSessionCompleteFrames(kCMTimeInvalid)
      |     ** a BLOCKING encoder flush on the real-time thread **
      v
 [VideoToolbox internal thread]
      |  output_callback()                            encoder.rs:386
      |    CFRetained::retain(CMSampleBuffer)         [ZC]
      |    crossbeam::unbounded send                  <-- NO BOUND, NO BACKPRESSURE
      v
 [videotoolbox-drain]  QOS_USER_INTERACTIVE + PRECEDENCE 63 + AFFINITY; NO time constraint
      |  pending_ts.lock().unwrap().pop_front()
      |  sample_to_annexb_into()                      encoder.rs:413-497
      |    two passes over the AVCC buffer; extend_from_slice per NAL + a 4-byte start code
      |    SPS/PPS prepended on keyframes; AUD (type 9) dropped        [CPU memcpy of the AU]
      |  Vec<u8> ALLOC per frame -> Bytes::from(au)   [ZC move]
      v
 broadcast(4)
```

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Queue / lock |
|---|---|---|---|---|---|
| 1 | Compositor | `make_cg_properties(fps)` `cgds.rs:69-100`; `CGDisplayStreamCreateWithDispatchQueue` `:184` with `cfg.pixel_format = PIXEL_FORMAT_420F` | WindowServer | The requested `width`/`height` are the **already-scaled encode dims** (`pipeline.rs:253-258`), so WindowServer does the downscale and the RGB→YCbCr conversion | `kCGDisplayStreamQueueDepth = 2` (`cgds.rs:71`); `kCGDisplayStreamMinimumFrameTime = (1/fps)*0.95` (`:70`); `ShowCursor = true`; `PreserveAspectRatio = false`; `YCbCrMatrix = ITU_R_709_2` |
| 2 | Delivery | `RcBlock` handler on a serial `DispatchQueue` `cgds.rs:162-165` | `com.screenextend.cgds`, dispatch QoS UserInteractive | — | The `displayTime` parameter is bound as `_display_time` and discarded (`:172`), which is why stage 1→2 is **unmeasurable today** |
| 3 | Status gate | `cgds.rs:114` | cgds queue | — | `FrameBlank`, `FrameIdle` and `Stopped` are all treated identically and silently |
| 4 | Dirty gate | `CGDisplayStreamUpdate::rects(update, DirtyRects, &mut count)` `cgds.rs:118-127` | cgds queue | the rect array is fetched and never used for anything but the count | `count == 0` ⇒ drop the frame, no counter. `RefreshedRects` / `MovedRects` never queried. |
| 5 | Wrap | `CFRetained::retain(surface)` `:134`; `CVPixelBufferCreateWithIOSurface` `:136` | cgds queue | **ZC** wrap; **ALLOC** of one CF object per frame | On a non-zero `CVReturn` the pixel buffer leaks on one branch (`:137-142`) |
| 6 | Publish | `sink.publish(Frame{…})` `frame.rs:63-66` | cgds queue | **ALLOC** — one `Arc::new(Frame)` per frame | `ArcSwapOption<Frame>` (depth **1**, latest-wins, lock-free) + `crossbeam bounded(1)` wake, `try_send`, dropped when full |
| 7 | Encode wake | `source.wait(idle)` `pipeline.rs:181`, `idle = 4 ms` `:170` | `videotoolbox-encode` | — | One wake slot for three producers (capture publish, `request_idr`, `set_target_bitrate` — `pipeline.rs:44-52`) |
| 8 | Take frame | `try_take_latest()` `frame.rs:75`, stored in `last_frame` `pipeline.rs:202` | videotoolbox-encode | **ZC** `ArcSwapOption::swap`. The taken `Arc<Frame>` is **held** until the next frame replaces it, so **two IOSurfaces are pinned in steady state — out of a queue depth of two.** | — |
| 9 | Timestamp queue | `pending_ts.lock().unwrap().push_back` `pipeline.rs:233` | videotoolbox-encode | `VecDeque<Instant>`, unbounded | `std::sync::Mutex` shared between a **TIME_CONSTRAINT** thread and a **USER_INTERACTIVE** thread — an unprotected priority inversion. All three call sites `.unwrap()`. The free `sourceFrameRefCon` slot (`encoder.rs:299` passes null) would delete this entirely. |
| 10 | Submit | `Encoder::submit` `encoder.rs:268-307` | videotoolbox-encode | **ZC** — the `CVImageBuffer` reference goes straight to `VTCompressionSessionEncodeFrame` | **Asynchronous**; returns on accept. PTS from a monotonically increasing `frame_index` at the `fps` timescale. A forced IDR builds a **fresh single-entry CFDictionary per frame** (`:283-289`, via `dict_from_pairs`, which `.expect`s). |
| 11 | Keepalive | `pipeline.rs:203-217` | videotoolbox-encode | `if let Some(frame) = last_frame.as_ref()` holds the `Arc<Frame>` borrow across the whole submit + flush block | `complete_pending()` = `VTCompressionSessionCompleteFrames(kCMTimeInvalid)` — **a blocking encoder flush on the real-time thread** |
| 12 | Output | `output_callback` `encoder.rs:386-412` | VT internal | **ZC** `CFRetained::retain(CMSampleBuffer)` (`:408`) | `crossbeam::unbounded` (`encoder.rs:71`) — **no bound, no back-pressure** |
| 13 | Annex B | `sample_to_annexb_into` `encoder.rs:413-497` | `videotoolbox-drain` | **CPU** — two passes over the AVCC buffer; SPS/PPS prepended on keyframes; AUDs dropped | — |
| 14 | Publish | `Vec<u8>` `pipeline.rs:156` (capacity 0, so `reserve` allocates once) → `Bytes::from(au)` `:162` → `tx.send` | videotoolbox-drain | **ALLOC** per frame, then **ZC** | broadcast(4) |

**Encoder configuration** (`encoder.rs:146-243`): `RealTime = true`, `AllowFrameReordering = false`,
`MaxFrameDelayCount = 1`, `ExpectedFrameRate = fps`, `MaxKeyFrameInterval = fps*60`,
`MaxKeyFrameIntervalDuration = 10 s`, 709 primaries/transfer/matrix (all `dlsym`'d),
`MaximizePowerEfficiency = false` (dlsym, 10.14), `PrioritizeEncodingSpeedOverQuality = true`
(dlsym, 11.0), `AverageBitRate`, `DataRateLimits = [bitrate*1.5/8 bytes, 0.5 s]`,
`MaxAllowedFrameQP = 45` (dlsym, 11.0). The encoder specification ladder is
`Require`-hardware → `Enable`-hardware → hinted, with `"EnableLowLatencyRateControl"` built as a
**string literal** rather than the 11.0 `extern static` — dyld-safe. **Only the third rung logs**
(`encoder.rs:105-114`), so a silent drop to a software encoder is invisible.

---

## 1.8 macOS · ScreenCaptureKit · VideoToolbox

Structurally identical to §1.7 from stage 6 onward. The differences are all in capture:

| # | Stage | Function (`file:line`) | Thread | Difference from CGDisplayStream |
|---|---|---|---|---|
| 1 | Config | `configure_stream` `sck.rs:552-571`, all via `msg_send!` | session start | `setWidth:`/`setHeight:` = scaled encode dims; **`setMinimumFrameInterval: CMTime{value:1, timescale:fps}`** — *exactly* 1/fps, which drops frames rather than merely limiting them; `setPixelFormat: '420f'`; **`setQueueDepth: 3`**; `setShowsCursor: true`; `setCapturesAudio: false`. **Not set:** `colorMatrix`, `colorSpaceName`, `sourceRect`, `destinationRect`, `scalesToFit`, `preservesAspectRatio`, `captureResolution`. |
| 2 | Delivery | `addStreamOutput:type:sampleHandlerQueue:error:` onto a serial `DispatchQueue` `com.screenextend.sck` targeting the global UserInteractive queue | SCK delivery queue | The `NSError` out-parameter of `addStreamOutput:…error:` is written and never read |
| 3 | IMP | `did_output_sample_buffer` `sck.rs:583`, declared `extern "C-unwind"` | SCK queue | Built at runtime with `ClassBuilder`; `SCStreamOutput` protocol conformance added only if `AnyProtocol::get(c"SCStreamOutput")` succeeds |
| 4 | Gate | `frame_is_complete_and_dirty` `sck.rs:~626` | SCK queue | Reads the per-sample attachment dictionary via `dlsym`'d `SCStreamFrameInfoStatus` / `SCStreamFrameInfoDirtyRects` CFString constants; requires status `Complete`; skips when the dirty-rects array is present and **empty**. The **first frame after start is discarded**. |
| 5 | Wrap | `publish_sample` `sck.rs:678`, `:697-713` | SCK queue | SCK already hands us a `CVImageBuffer` (`:681`) and the code deliberately extracts the IOSurface and **re-wraps it into a fresh `CVPixelBuffer`** — one avoidable CF allocation per frame |
| 6 | Pinning | `pipeline.rs:202` | videotoolbox-encode | **2 of 3** surfaces pinned (vs 2 of 2 on cgds) — less starvation pressure, same mechanism |

**Dyld-safety (load-bearing):** classes via `AnyClass::get`, messages via `msg_send!`, the
delegate class built with `ClassBuilder`, the two 12.3+ CFString constants resolved with
`libc::dlsym(RTLD_DEFAULT, …)` (`sck.rs:117-128`), and the 12.3+ enum values inlined as
constants. There is **no `objc2-screen-capture-kit` dependency**. This is the part of the file
that must not regress.

**Backend selection** (`macos_utils/streamer/mod.rs:116`): `start_capture` tries SCK when
`screencapturekit_available()` and falls back to CGDisplayStream, reporting `FallbackFailed`
only when both fail. `screencapturekit_available()` is **`macos_at_least(12, 3)`**
(`mod.rs:80-82`) — a `sw_vers` **subprocess fork**, not a class lookup, and it **fails open**
(`:75`, `None => true`).

---

# PART 2 — THE AUDIO PATH

## 2.0 The shared spine (all four capture tiers converge here)

```
 [capture backend]
      |  AudioPacket{ seq: u32, capture_ns: u64, flags: u8, data: Bytes }
      v
 crossbeam::unbounded          <-- UNBOUNDED, no back-pressure (both platforms)
      |
      v
 [audio-bridge thread]  plain std::thread, DEFAULT PRIORITY, no boost
      |  streamer/audio/mod.rs:156-162 — a pure copy loop, a removable type-adapter hop
      v
 tokio::sync::broadcast<AudioPacket>(128)      = 640 ms of slack
      |  Lagged(n) -> `continue`, n discarded, no counter, FLAG_DISCONTINUITY NOT set
      v
 forward_datachannel()  (one task per session)      webrtc_session.rs:~750-800
      |  overrides.lock() per drained batch (audio_enabled_now)
      |  scratch.resize(HDR + len)        <-- reused Vec
      |  protocol::write_message(...)     [CPU memcpy #1]
      |  Bytes::copy_from_slice(&scratch) [ALLOC + CPU memcpy #2]  <-- defeats the reused scratch
      |  every 4th packet: dc.buffered_amount().await > 16 KiB -> DROP the packet, silently
      v
 RTCDataChannel "audio"  — negotiated UNORDERED, maxRetransmits 0
      v
 SCTP -> DTLS -> UDP
```

Fallback: `forward_track` (`webrtc_session.rs:~830`) writes an Opus `TrackLocalStaticSample`
with a fixed 5 ms duration and **discards `capture_ns` entirely**, so the shared-timebase
guarantee does not hold on the NetEQ path.

Wire format (`streamer/audio/protocol.rs:3`): **13 bytes LE** — `seq: u32`, `capture_ns: u64`,
`flags: u8` — followed by the raw Opus payload.

Opus (`streamer/audio/encoder.rs`, shared by every backend): `OPUS_APPLICATION_RESTRICTED_LOWDELAY`
(CELT-only), 48 kHz stereo, **240 samples = 5 ms**, 128 kbps, complexity 5, `SIGNAL_MUSIC`,
DTX 0, **INBAND_FEC 0**, VBR 1, **VBR_CONSTRAINT 0** (unconstrained). Encodes into a reused
4000-byte scratch and returns a borrow — **no per-frame allocation in the encoder itself**.
`OPUS_GET_LOOKAHEAD` reads back **120 samples = 2.5 ms** and is propagated to the client and
only logged. In-band FEC and DTX are both LPC-layer-only and therefore **structurally inert** in
this configuration.

## 2.1 Windows · WASAPI loopback

```
 apps render -> WASAPI shared engine mixes the eRender/eConsole endpoint
      |  Initialize(SHARED, LOOPBACK|EVENTCALLBACK, 0, 0, pwfx, None)   loopback.rs:307-322
      |  both duration params are 0 => the engine grants its DEFAULT period (10 ms typical)
      |  the period is NEVER QUERIED; the field named `period_frames` is GetBufferSize()
      |
 [audio-silence thread]  silence.rs — MANDATORY, the clock source
      |  IAudioRenderClient on the same endpoint, refills
      |  (buffer_frames - GetCurrentPadding()) with AUDCLNT_BUFFERFLAGS_SILENT
      |  the buffer is NEVER WRITTEN -> genuinely zero-copy
      |  measured: 0/20 loopback events without it, 20/20 with it
      v
 [audio-loopback thread]  MMCSS "Pro Audio" + COM MTA
      |  session.event.wait(100)                    <-- 100 ms starvation fallback
      v
 drain()                                            loopback.rs:274-342
      |  loop { GetNextPacketSize -> GetBuffer -> ... -> ReleaseBuffer } until empty
      |    GetBuffer returns a pointer INTO the engine buffer          [ZC]
      |    convert_to_stereo_f32()                  format.rs:133-141
      |      "fast path" is NOT a memcpy: ~960 iterations of
      |      f32::from_le_bytes(...).clamp(-1,1) + push  into a REUSED accum: Vec<f32>  [CPU]
      |    AUDCLNT_BUFFERFLAGS_SILENT -> accum.resize(len+n, 0.0)      [memset]
      |    devpos / qpcpos requested and NEVER READ
      v
 cut into 5 ms frames: encoder.encode_float(&accum[start..start+480])  loopback.rs:227
      |  capture_ns: host_now_ns()                  loopback.rs:245
      |  ** taken at ENCODE time, not capture time **
      |  Bytes::copy_from_slice(encoded)            loopback.rs:247  [ALLOC + CPU, 200/s]
      |  accum.drain(0..full*480)                   loopback.rs:255  [memmove of 0-479 f32]
      v
 pkt_tx.send(AudioPacket)  -> crossbeam UNBOUNDED  loopback.rs:39
```

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Queue / lock |
|---|---|---|---|---|---|
| 1 | Companion | `SilenceCompanion::start()` `silence.rs:21`, `setup` `:80`, `pump` `:123` | `audio-silence`, COM MTA, **no MMCSS** | **ZC** — buffer never written | Blocks the caller on a `std::sync::mpsc` ready handshake with **no timeout**. Three unlogged `break`s end the thread; nothing restarts it and the capture loop's only symptom is a permanently un-signalled event whose `bool` return is discarded (`loopback.rs:197`). |
| 2 | Event wait | `session.event.wait(100)` `loopback.rs:197` | `audio-loopback`, MMCSS "Pro Audio" | — | `WaitForSingleObject` on an auto-reset event |
| 3 | Drain | `drain(&Session, &mut accum, …)` `loopback.rs:274` | audio-loopback | **ZC** in, **CPU** per-sample out into a reused `Vec<f32>` (capacity `FRAME_INTERLEAVED*16` = 7680 f32 = 30 KiB) | `AUDCLNT_E_DEVICE_INVALIDATED` → `DrainError::DeviceInvalidated` from all three call sites |
| 4 | Encode | `encoder.encode_float` `loopback.rs:227` | audio-loopback | encoder-internal reused scratch — **no ALLOC** | Encode failure logs and `continue`s, **silently dropping the frame without setting `FLAG_DISCONTINUITY` on the next one** |
| 5 | Packetise | `Bytes::copy_from_slice(encoded)` `:247` | audio-loopback | **ALLOC + CPU** — 200 allocations/second of a few dozen bytes each | — |
| 6 | Compact | `accum.drain(0..full*480)` `:255-257` | audio-loopback | **CPU** memmove of the 0–479 f32 residue, every iteration | — |
| 7 | Publish | `pkt_tx.send(pkt)` `:249` | audio-loopback | **ZC** move of the `Bytes` | `crossbeam_channel::unbounded` — the producer never blocks, so a stalled consumer converts directly into latency |
| 8 | Diagnostics | `update_encode_percentiles` `:458-466`, called `:259-263` | **audio-loopback (the RT thread)** | `samples.to_vec()` — a **32 KiB allocation** + `sort_unstable` of 4096 elements + a `format!` + the logbus mutex + a Tauri IPC event, **every 2 seconds on the Pro Audio thread** | — |
| — | Device change | `DeviceNotifier::OnDefaultDeviceChanged` `device.rs:33` | COM notification thread (not ours) | — | Non-blocking `let _ = tx.send(...)` into `crossbeam unbounded`; `try_recv` once per capture loop (`:179`), each event triggering a full `reacquire()`. **This callback correctly does nothing else.** |

**Format negotiation** (`acquire` `loopback.rs:344-431`): the companion starts **first**, then
the mix format is taken verbatim if it is already 48 kHz / 2 ch / f32 (`:358-360`), else a
hardcoded 48 kHz f32 stereo target with `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` (`:26-27`).
Both outcomes satisfy `is_fast_path()`, which is why `format.rs`'s BS.775 downmix and integer
sample paths are **unreachable in production**.

## 2.2 macOS · Core Audio Process Tap (14.2+, preferred)

```
 HAL mixes -> private aggregate device (main sub-device = the default output,
              tap list entry's kAudioSubTapUIDKey == the CATapDescription's own UUID)
      |  ** kAudioDevicePropertyBufferFrameSize is NEVER SET **  (typically 512 fr = 10.67 ms)
      v
 [CoreAudio HAL IOProc thread]  real-time, HAL-owned
      |  tap_ioproc()                                process_tap.rs:~90-160
      |    read AudioBufferList                      [ZC, kernel memory]
      |    in_input_time (the hardware timestamp)    -> DISCARDED
      |    convert_interleaved / convert_planar      format.rs  [CPU pass 1]
      |      ** AudioFormatDesc::is_fast_path() is NEVER CONSULTED **, so even 48 kHz
      |         stereo f32 interleaved goes through byte-wise reassembly + BS.775 downmix
      |      destination: a preallocated 16384-f32 (64 KiB) Box<[f32]> — no ALLOC
      |    silence scan (abs() > 1/32768)            [CPU pass 2]
      |    Producer::push(&scratch)                  [CPU pass 3, element-wise via UnsafeCell]
      |      ** Thread::unpark() inside the RT callback (ring.rs:68-72) **
      |         takes a pthread_mutex whenever the consumer is parked — every idle gap
      v
 SPSC ring: Box<[UnsafeCell<f32>]>, 8192 f32 = 4096 frames = 85.3 ms
      |  drop-NEWEST on overflow, overruns counter, never blocks
      v
 [macos-audio worker]  QOS_CLASS_USER_INITIATED
      |  drain ControlMsg (bounded(8), try_send, drops)
      |  Consumer::pop into a [f32; 960] stack array [CPU pass 4]
      |  silence check: frame.iter().all(|&x| x == 0.0)  [CPU pass 5]
      |     ** exact zero, while the callbacks use -90 dBFS — the two definitions disagree **
      |  encoder.encode_float()
      |  capture_ns SYNTHESISED FORWARD in 5 ms steps from the worker's wake-up (mod.rs:284-315)
      |     ** drain time, not capture time **
      |  Bytes::copy_from_slice()                    mod.rs:317  [ALLOC]
      |  pkt_tx.send()                               -> crossbeam UNBOUNDED
      |  park_timeout(125 us) when idle              mod.rs:351-353
```

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Queue / lock |
|---|---|---|---|---|---|
| 1 | Tap create | `build_and_start` `process_tap.rs:~270-365` | caller (a tokio worker via `AudioHub::subscribe`) | — | Order: resolve `dlsym`'d fns → `AnyClass::get(c"CATapDescription")` → `initStereoGlobalTapButExcludeProcesses:[]` → set UUID / muteBehavior 0 / private / name → create tap → read tap ASBD → read the default **system** output device + its UID → **`set_nominal_sample_rate(output_dev, 48_000.0)` on the user's device, status ignored, never restored** (`:320`) → `create_aggregate_device` → re-read ASBD → reject non-48 kHz → `AudioDeviceCreateIOProcID` → `AudioDeviceStart` |
| 2 | Callback | `tap_ioproc` | HAL RT thread | 3 CPU passes over 1024–2048 samples into preallocated scratch; **no ALLOC** | The `unpark` (stage 3) is the one unbounded-blocking risk |
| 3 | Ring push | `Producer::push` `ring.rs:60-72` | HAL RT thread | **CPU** element-wise through `UnsafeCell` | SPSC, relaxed-load head / acquire-load tail / release-store head. `Thread::unpark()` at `:68-72` acquires a `pthread_mutex` on a real-time thread. |
| 4 | Worker | `worker(...)` `mod.rs:~220-360` | `macos-audio`, `pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED)` | 2 more CPU passes + one `Bytes` ALLOC per 5 ms | `park_timeout(125 µs)` when the ring is short of a frame |
| 5 | Device change | `default_device_listener` `process_tap.rs:~199` | HAL property-listener thread | — | `try_send(ControlMsg::Reacquire)` into `bounded(8)`; drops on full. Correct — these are edge notifications. |
| — | Teardown | `TapResources::drop` `process_tap.rs:~159` | — | — | RAII in the documented order: stop → `AudioDeviceDestroyIOProcID` → free ctx → `AudioHardwareDestroyAggregateDevice` → `destroy_tap`, so a leaked tap cannot persist a hidden capture |

## 2.3 macOS · ScreenCaptureKit audio (13.0–14.1)

SCK has no audio-only mode, so this tier rides a **2×2 pixel, 1 fps dummy video stream** with
`capturesAudio` on — which lights the screen-recording indicator.

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership |
|---|---|---|---|---|
| 1 | Start | `build_and_start` `sck_audio.rs:~90-250` | caller | resolve classes → `resolve_any_display` (**5 s blocking bridge**) → content filter → config (`capturesAudio`, 48 kHz, 2 ch, exclude own audio, 2×2, 1 fps) → delegate → `SCStream` → UserInteractive serial queue → `addStreamOutput:type:1` → `startCaptureWithCompletionHandler` bridged to a **5 s `recv_timeout`** |
| 2 | Sample | `handle_audio_sample(sample, ctx)` `sck_audio.rs:~370-460` | SCK serial queue `com.screenextend.sck.audio`, dispatch QoS UserInteractive | `CMSampleBufferGetFormatDescription` → `CMAudioFormatDescriptionGetStreamBasicDescription` → `parse_asbd` → `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` into a **4 KiB scratch** → planar or interleaved convert into a 64 KiB scratch → silence scan → `Producer::push`. The block buffer is retained for the duration of the handler (`:376-379`). The `CMSampleBuffer` PTS is **discarded**. |
| 3–5 | ring → worker → Opus → channel | as §2.2 | `macos-audio` | identical |
| — | Stop | `AudioSource::stop` `sck_audio.rs:~230-260` | caller | Blocking stop with a **5 s timeout**, then release + **free the delegate ctx immediately afterwards**, while a handler may still be in flight (`major-audio-mac-sck-stop-frees-ctx-with-handler-possibly-in-flight.md`) |

Delivery chunk size is whatever SCK chooses and is not documented — larger than a HAL period.

## 2.4 macOS · legacy virtual audio device (10.15–12.x)

```
 [ScreenExtendAudio AudioServerPlugIn, in coreaudiod]
      |  set as the system default output device by Router::activate
      |  host requests kAudioDevicePropertyBufferFrameSize = 128 frames = 2.67 ms
      |  ** the smallest buffer in the system **                    legacy/mod.rs:70-79
      |
      |  OnProcessMixedOutput()
      |    ShmRing::Write()  element-wise store into the mapped ring  shm_ring.cpp:77-79
      |      never checks the reader, never blocks, OVERWRITES; `overruns` never incremented
      |    PLUS a second element-wise write into the driver's own 65536-frame loopback ring
      |      (device.cpp:46-51) on EVERY callback, used or not
      v
 POSIX shared memory: 64-byte header + 131072 f32 = 65536 frames = 1.365 s
      |  ABI contract: macos/ScreenExtendAudio/src/shm_ring.hpp <-> legacy/shm_reader.rs
      v
 [se-audio-shm thread]  THREAD_TIME_CONSTRAINT, period 5 ms       shm_reader.rs:213
      |  poll writePos (relaxed/acquire atomics over a PROT_READ mapping)
      |  clamp_read() -> lap detection; a lapped reader JUMPS FORWARD
      |  drain into a 4096-f32 scratch via element-wise ptr::read_unaligned  [CPU]
      |  park_timeout(500 us)   <-- NOTHING CAN UNPARK IT; the producer is another process
      v
 CaptureTargets::publish(&[f32])                                   shm_reader.rs:82,88
      |  push the SAME slice into TWO rings:
      |    encoder ring  (8192 f32  = 85.3 ms)   -> the worker, as in 2.2
      |    monitor ring  (65536 f32 = 683 ms)    -> local playthrough
      v
 [playthrough IOProc]  real-time, HAL-owned                        playthrough.rs:~120-200
      |  measure backlog -> drift_trim (integral controller, +/-0.3 %)
      |  target DRIFT_SETPOINT_FRAMES = 4096 = 85.3 ms of monitor backlog
      |  pop ceil(dst*ratio)+2 source frames into a 16384-f32 scratch
      |  linear interpolate 48 kHz -> device rate, apply ramped gain
      |  write interleaved or planar; the destination slice is reconstructed INSIDE the per-frame loop
      v
 the real output device — this is what the HOST hears
```

Fallback transport when the shared segment cannot be mapped: `HalInputProc`
(`shm_reader.rs:322-343`) — a plain input IOProc on the virtual device's input stream, which
**assumes f32 stereo without checking**. If it also fails, `Reader` is constructed anyway and
the tier reports success while producing silence forever.

Side machinery on this tier, all on the start path: `Router::activate` (saves the current
default output, writes `legacy_audio_routing.json` synchronously, sets ours),
`install_watchdog_agent()` (writes a plist and forks `launchctl bootstrap` on **every** start),
`VolumeListeners` + `volume_proxy` (mirrors the virtual device's Volume/Mute onto `MonitorGain`
and onto the real device), and `volume_keys` (a `CGEventTap` on `NX_SYSDEFINED` behind a
process-global `Mutex<Manager>` whose `bind_device`/`set_enabled` hold the lock **across a
blocking thread handshake with no timeout**).

## 2.5 Client, audio

```
 SCTP (unordered, maxRetransmits 0) -> dc.onmessage on the PAGE MAIN THREAD
      |  an RTCDataChannel cannot be handed to a worker, so the page must receive it;
      |  but nothing else happens there
      |  postMessage(buf, [buf])                    audio.js:551   [ZC transfer]
      v
 [audio decode worker]                              audio-worker.js
      |  handlePacket()  :121
      |    parse the 13-byte header (seq u32 LE, capture_ns u64 LE, flags u8)
      |    dedup + late-drop by seq  :126-129     ** no gap detection, no PLC **
      |    byte 12 (FLAG_SILENT / FLAG_DISCONTINUITY) is NEVER READ
      |  new EncodedAudioChunk({data})              [ALLOC + CPU]
      |  AudioDecoder.decode()   (config: {codec:'opus', sampleRate:48000, numberOfChannels:2},
      |                           no `description` — correct for raw Opus)
      |  onDecoded()  :65
      |    two AudioData.copyTo into reused scratchL/scratchR  [2 CPU memcpys, planar]
      |    ad.close()
      |  writeShared()  :36     [SAB path]
      |    Atomics.load(W), Atomics.load(R)
      |    two TypedArray.set into the PLANAR ring (4 if the write wraps)  [CPU]
      |    Atomics.store(W)
      |    ** on short free space: writes a PARTIAL packet and Atomics.add(OVERRUNS);
      |       the tail is silently dropped MID-PACKET **
      |  postSamples()  :57     [non-SAB path]
      |    TWO fresh Float32Arrays PER PACKET  [400 ALLOC/s] + 2 memcpys
      |    -> transferred to the page -> re-transferred to the worklet (audio.js:525)
      |    ** two main-thread hops per 5 ms packet **
      v
 SharedArrayBuffer ring: 64-byte Int32Array control block (W=0, R=1, OVERRUNS=2, 13 unused)
                         + two Float32Array regions of CAP_FRAMES = 65536 (= 1.365 s)
                         absolute uint32 positions, power-of-two capacity, no CAS, no Atomics.wait
      v
 [AudioWorklet render thread]  every 128 frames = 2.67 ms at 48 kHz
      |  AudioJitterProcessor.process()             audio-worklet.js:125
      |    available() via Atomics
      |    hard drop: advanceRead(avail - effTarget) when avail > effTarget + 250 ms  :138-142
      |    dequeueInto: two `set` memcpys into the output block  :113-123
      |    ** underrun => priming = true => DIGITAL SILENCE until avail >= effTarget  :178-188
      |       one lost 5 ms packet costs a FULL target depth (10-40 ms) of silence **
      |    drift corrected only at silence boundaries (SILENCE_PEAK = 0.0035)
      |    stats posted every 96 quanta -> stored in state.stats -> read only by getSyncInfo(),
      |       which nothing calls automatically
      v
 GainNode -> AudioContext destination (setSinkId / iOS audioSession category as configured)
```

Depth is commanded by `maybeUpdateTarget` (`audio.js:426-451`):
`videoDelay − preEnqueue − outputLatency`, wrapped modulo the RTP rollover period
`P_MS = 2³²/90`, clamped to **[10, 400] ms**, with the unachievable remainder recorded in
`residualOffsetMs` and never acted on. `outputLatencyMs()` (`audio.js:418-424`) is
`(baseLatency ?? 0) + (outputLatency ?? 0)` — a **missing `AudioContext.outputLatency` is
treated as 0**, which is Safari < 18.4, Chrome < 102, and every WebView on the 10.15 floor.

Ring ABI is a **three-file contract**: `audio.js` owns the allocation size (`CAP_FRAMES = 65536`,
`CTRL_BYTES = 64`, `HDR_BYTES = 13`), `audio-worker.js` writes it, `audio-worklet.js` reads it.

---

# PART 3 — THE INPUT PATH

## 3.0 Client → wire (shared)

```
 pointerdown / pointermove / pointerrawupdate / wheel / keydown / ... listeners
      |  installListeners()  input.js:405 ; 'onpointerrawupdate' in window  :411
      v
 gate + coalesce                                    input.js:279-320
      |  the FIRST move of each task is sent immediately;
      |  subsequent ones are coalesced into pendingMoves / pendingStrokes Maps
      |  drained by a MessageChannel flush (one macrotask)
      |  getCoalescedEvents() is used ONLY for pen (:333) — high-rate TOUCH samples are
      |    decimated to one per task
      v
 encode into module-level PREALLOCATED DataViews    input.js:43-52
      |  moveBuf 40 B, batchBuf 970 B, wheelBuf 15 B, deltaBuf 9 B, resizeBuf 9 B,
      |  lifeBuf 2 B, pingBuf 9 B, zoomBuf 5 B, keyScratch 1024 B
      |  ** genuinely allocation-free on the hot path **
      v
 channelFor(op)  input.js:57
      |  `fast`     — unordered, maxRetransmits 0 : MOVE, MOVE_BATCH, ENTER, LEAVE, OVER,
      |                                            OUT, WHEEL, ZOOM, MOUSE_DELTA
      |  `reliable` — ordered, reliable          : keys, text, clipboard, lifecycle, PING/PONG, BYE
      |  `bulk`     —                            : file drops
      v
 fastSend()  input.js:74
      |  bufferedAmount > FAST_BUFFER_MAX (8192) => DROP the message, silently, UNCOUNTED
      |  every send() throw is swallowed (:70-73)
      v
 SCTP -> DTLS -> UDP
```

## 3.1 Host → OS injection, Windows

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Queue / lock |
|---|---|---|---|---|---|
| 1 | Receive | `dc.on_message` closure `webrtc_session.rs:425-437` | webrtc-rs SCTP task, a tokio worker | `msg.data: Bytes` — **ZC** from SCTP. **3 `Arc` clones + one `Box::pin` heap ALLOC per message.** | — |
| 2 | Parse | `protocol::parse(b)` `input/protocol.rs:235` | same | **CPU**, tiny. `SmolStr` keeps `code`/`key` inline for ≤22 bytes, so the common key event allocates nothing. `PointerBatch` ALLOCs one `Vec<MoveSample>` (≤40×24 = 960 B); `Text`/`Clipboard`/`Drop` ALLOC by size. | `None` ⇒ a bare `return` — no log, no counter (`:429-431`) |
| 3 | Pong | `let _ = dc_pong.send(build_pong(t_ns)).await` `webrtc_session.rs:435` | same | **ALLOC** of a 9-byte reply | **Awaited inside the receive handler** — head-of-line blocks the reliable channel. The RTT it enables is computed on the client (`input.js:530`) and **read by nothing**. |
| 4 | Route | `InjectorTx::route(ev, hot)` `input/mod.rs:63-76` | same | `InputEvent` **moved** into the channel slot (largest variant ~48 B memcpy); heap contents not copied | `hot` ⇒ `crossbeam bounded(FAST_CAP = 512)` (`input/mod.rs:52`, `:92`), `try_send`; **on full the event is DROPPED and only an atomic nobody reads is incremented**. Otherwise `crossbeam unbounded`, `send` result discarded. |
| 5 | Injector wake | `crossbeam::select!` `input/mod.rs:103-112` | `injector` OS thread, **one per WHEP session** (`webrtc_session.rs:394`), `THREAD_PRIORITY_TIME_CRITICAL` (`windows.rs:69-73`) | — | **`select!` picks RANDOMLY among ready operations** — a reliable-channel key-down has no precedence over a fast-channel move, and cross-channel arrival order is not preserved. The thread exits when *either* channel disconnects. |
| 6 | Geometry | `Injector::refresh_geometry()` `windows.rs:196` | injector | — | ~50 ns cached; **5 `GetSystemMetrics` + a `monitor_rect` lookup at most every 500 ms**. Called at the top of **every** `dispatch` — i.e. **once per sample, not per batch**. |
| 7a | Mouse / key / text | `mouse_pointer` / `key` / `text` → `send_inputs` `windows.rs:850` | injector | Packed into the **reused** `self.scratch: Vec<INPUT>` (`with_capacity(16)`) — **no steady-state ALLOC**. One `SendInput` copies into kernel space. `size_of::<INPUT>()` passed explicitly. | A partial `SendInput` warns but **does not retry the remainder** (`:851-853`) |
| 7b | Batch | `pointer_batch(source, id, buttons, samples)` `windows.rs:117` | injector | — | **Loops per sample, one OS call each.** A 40-sample batch is 40 syscalls. |
| 7c | Pen / touch | `pen_info` / `touch_info` → `InjectSyntheticPointerInput` | injector | `POINTER_TYPE_INFO` built with `..Default::default()`; touch uses the reused `touch_scratch`, moved in and out with `std::mem::take` (a pointer swap, not a copy). Touch re-emits the **whole contact frame** on every change — correct for the API. | Devices come from `CreateSyntheticPointerDevice` (pen 1 contact, touch 10). Creation failure **warns once** then permanently degrades pen/touch to mouse — a good probe, an invisible degradation. `DestroySyntheticPointerDevice` is **never called**. |
| 7d | Clipboard | `set_clipboard` `windows.rs:866-880` | injector | `GlobalAlloc` + UTF-16 copy | If it returns false, **nothing is logged at all**. `EmptyClipboard` / `CloseClipboard` results discarded. |
| 7e | File drop | `drop_files` `windows.rs:624-690` | injector | `std::fs::write` to `%TEMP%\input-bridge-drop`, then a `DROPFILES` `GlobalAlloc` block — **two extra full copies**, cold path | `WindowFromPoint` + `ScreenToClient` + `PostMessageW(WM_DROPFILES)` |
| 8 | Delivery | win32k → target window | OS | — | `mouse_delta` is **unclamped**, so a pointer-locked client can drive the host's real monitors. `Lifecycle::PointerLock` is **ignored** on Windows. |

## 3.2 Host → OS injection, macOS

Stages 1–6 are identical (the injector thread is pinned with
`macos_utils::streamer::qos::pin_current_thread_user_interactive()`, whose return is discarded).
From stage 7:

| # | Stage | Function (`file:line`) | Thread | Buffer / ownership | Notes |
|---|---|---|---|---|---|
| 7 | Event build | `post_mouse` / `post_key` `macos.rs:~481`, `:596` | injector | **One CoreFoundation object ALLOCATED PER EVENT** (`CGEventCreateMouseEvent` / `CGEventCreateKeyboardEvent`), released immediately after posting. **There is no reuse path; the API has no batch form.** | Every `CGEventCreate*` null check is a bare `return`/`continue` with no log |
| 8 | Modifiers | `key(down, code, key)` `macos.rs:~560`, `recompute_mods()` | injector | — | `code_to_keycode` **hardcodes ControlLeft→Command and MetaLeft→Control, unconditionally — including for Apple clients**. A modifier's own bit is deliberately excluded from its own key-down. |
| 9 | Post | `CGEventPost(kCGHIDEventTap, ev)` | injector → Mach IPC → WindowServer | — | The `CGEventSource` is created with the local-events suppression interval set to **0** (correct — otherwise synthetic moves suppress the real mouse for 0.25 s). |
| 10 | Degradations | `pen`, `touch`, `pointer_batch` `macos.rs:~640` | injector | — | All three degrade to `mouse_pointer`. `mouse_delta` is clamped to the whole desktop, not the target display. `Lifecycle::PointerLock` **is** honoured here. |
| — | Scroll | `CGEventCreateScrollWheelEvent` `macos.rs:33-38` | injector | — | **Mis-declared**: `wheel1` is inside the variadic part, but the real prototype has it as a *named* parameter. On arm64 Darwin named args go in registers and variadic args go on the stack, so the callee reads `wheel1` from a register the caller never wrote — `blocker-input-cgscroll-variadic-abi-arm64.md`. |
| — | Accessibility | `Injector::new` `macos.rs:265-270` | injector | — | `accessibility_trusted()` is checked, **one warning is logged, and the code proceeds anyway**. Every subsequent `CGEventPost` is then a silent no-op forever, with no tier, no `CompatibilityReport` field, and no re-probe. |

---

# Appendix — every queue in the system, in one table

| Queue | Where | Type | Depth | Bounded | On full | Back-pressure |
|---|---|---|---|---|---|---|
| WGC frame pool | `graphics_capture_api.rs:197` | WinRT | **3** (1 after `Recreate`) | yes | WGC drops/overwrites | `TryGetNextFrame` dequeues the **oldest**; no drain loop |
| WGC message queue | `capture.rs:35`, `:409` | Win32 | unbounded | no | — | backs up in lockstep with the pool |
| NVENC ring free slots | `pipeline.rs:1102` | `crossbeam bounded` | 3 (effective **2**) | yes | **frame silently dropped**, `Ok(false)` ignored | none |
| NVENC ring ready | `pipeline.rs:1103` | `crossbeam bounded` | 3 | yes | cannot fill | bounded by free_slots |
| NVENC bitstream buffers | `nvidia/encoder.rs:699` | NVENC | **1** | yes | — | `enableEncodeAsync = 0` ⇒ one picture in flight |
| Repeater wake | `pipeline.rs:~436` | `crossbeam bounded` | 1 | yes | `try_send` dropped | coalescing by design; **unused on the ring path** (`wake_rx` is dropped at `:1594`) |
| DXGI setup handshake | `pipeline.rs:407` | `std::sync::mpsc` | 1 | — | — | blocks the caller |
| CGDisplayStream surfaces | `cgds.rs:71` | OS | **2** | yes | stream stalls | **the pipeline pins 2 of the 2** |
| SCK surfaces | `sck.rs:566` | OS | 3 | yes | stream stalls | 2 of 3 pinned |
| macOS frame slot | `frame.rs:39` | `ArcSwapOption` | **1** | yes | never blocks | latest-wins; correct for latency |
| macOS frame wake | `frame.rs:49` | `crossbeam bounded` | 1 | yes | `try_send` dropped | one slot, three producers |
| VT output | `encoder.rs:71` | `crossbeam unbounded` | **∞** | **no** | grows | **none** |
| `pending_ts` | `pipeline.rs:140` | `VecDeque` under a `std::sync::Mutex` | ∞ | no | — | relies on a strict 1:1 submit/emit |
| Encoded video fan-out | `pipeline.rs:44` / `macos .../pipeline.rs:28` | `tokio broadcast` | **4** (= 66 ms at 60 fps) | yes | oldest evicted → `Lagged` | **none**; the viewer answers `Lagged` with a forced IDR |
| Audio capture → hub | `loopback.rs:39`, `macos .../mod.rs:161` | `crossbeam unbounded` | **∞** | **no** | grows | **none** |
| Audio hub fan-out | `streamer/audio/mod.rs:93` | `tokio broadcast` | **128** (= 640 ms) | yes | `Lagged(n)` | dropped count discarded, no `FLAG_DISCONTINUITY` |
| Audio DataChannel | `webrtc_session.rs:46` | SCTP send buffer | — | — | `buffered_amount > 16 KiB` ⇒ packet dropped | sampled 1-in-4, silent |
| macOS audio SPSC ring | `macos .../mod.rs:25` | hand-rolled SPSC | 8192 f32 = **85.3 ms** | yes | drop **newest** + `overruns` | none |
| macOS monitor ring | `legacy/mod.rs:27` | SPSC | 65536 f32 = **683 ms** | yes | drop newest, silently | none |
| macOS legacy shm ring | `shm_ring.hpp:17` | cross-process | 131072 f32 = **1.365 s** | yes | producer **overwrites** | reader detects the lap and jumps forward |
| Client audio ring | `audio.js:11` | SAB (or private) | 65536 frames = **1.365 s** | yes | **partial write** + `OVERRUNS`; the tail is dropped mid-packet | none |
| Client worklet hard drop | `audio-worklet.js:138-142` | — | `effTarget + 250 ms` | — | `advanceRead(avail - effTarget)` in one step | — |
| Client decoder queue | `transform-worker.js:229-243` | UA | unbounded | no | ours: `> max(3, fps*0.05)` twice ⇒ stop and demand a keyframe | — |
| Input fast | `input/mod.rs:92` | `crossbeam bounded` | **512** (≈2.5 s of stale pointer history, **replayed in order**) | yes | **event dropped**, uncounted | none |
| Input reliable | `input/mod.rs:93` | `crossbeam unbounded` | ∞ | no | — | none |
| Client fast DataChannel | `input.js:74-78` | SCTP | `bufferedAmount > 8192` | soft | **message dropped**, silent, uncounted | correct policy for an unordered lossy channel |
| BYE control | `server.rs:1048` | `tokio mpsc unbounded` | ∞ | no | — | never blocks |
| Relay outbound | `cloud.rs:327` | `tokio mpsc unbounded` | ∞ | no | — | the writer breaks on the first send error; subsequent sends are silently dropped |
| Driver IPC broadcast | `driver_ipc/client.rs:68` | `tokio broadcast` | 10 | yes | `Lagged(n)` **discarded** | leaves the cached monitor state stale with no signal |

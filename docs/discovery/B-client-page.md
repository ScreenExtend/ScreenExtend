# B — The client web page (`src-tauri/src/streamer/static/`)

Territory: every file in `src-tauri/src/streamer/static/`, read in full.
Read-only pass. Line references are against the working tree at the time of reading.

Note on scope: the brief listed nine files; the directory also contains a tenth,
`at-capacity.html`, which is served by `server.rs::at_capacity_page()`. It is covered below.

---

## 1. Files and purpose

**`index.html` (1406 lines)** — the whole client application. It is one HTML document with a small
inline `<style>` for the microphone modal, the join form, the video stage, five modals, and a
single ~1250-line inline `<script>` that owns everything: capability detection, the WHEP
handshake, the WebCodecs↔`<video>` path selection and fallback, fullscreen/pointer-lock/keyboard-lock,
the `/reconfig` poll, geometry and refresh-rate reporting, kick handling, the compatibility gate,
the HTTPS upsell, the same-device warning, and the microphone permission flow. The Rust server
substitutes `__SAME_DEVICE_FLAG__` (`index.html:38`) at serve time and adds the cross-origin
isolation headers (`server.rs:367-397`). Everything in the inline script shares one scope, so a
top-level throw anywhere aborts the rest of it — see §8.

**`transform-worker.js` (264 lines)** — the WebCodecs video path, in a dedicated worker. Receives
either an `RTCRtpScriptTransform` (`self.onrtctransform`) or a transferred `ReadableStream` from
`createEncodedStreams()`, pumps encoded H.264 frames into a `VideoDecoder`, and paints each output
frame directly to a transferred `OffscreenCanvas` 2D context inside the decoder's output callback.
Also derives an `estimatedFps` and a `videoDelayEmaMs` from the RTP timestamps, and posts both
back to the page.

**`audio.js` (632 lines)** — the page-side audio controller, an IIFE exposing `window.SEAudio`.
Two responsibilities that are only loosely related: (a) the fast audio path — `AudioContext`,
`AudioWorkletNode`, the decode worker, the SharedArrayBuffer ring, and the A/V sync target
computation; and (b) the audio-*output* selection feature — `enumerateDevices`, the microphone
permission dance needed to get device labels, `setSinkId`, the iOS earpiece/loudspeaker category
switch, and the `/audio-outputs` POST loop. It is by some margin the most stateful file
(the `speakers` object alone has 27 fields, `audio.js:54-81`).

**`audio-worker.js` (158 lines)** — Opus decode off the main thread. Parses the 13-byte host
header, dedups and rejects late packets by sequence number, feeds a WebCodecs `AudioDecoder`,
deinterleaves the decoded planar float into scratch, and writes into either the shared ring
(`Atomics`) or a postMessage pair of `Float32Array`s. Also computes the audio-side
capture→enqueue delay EMA used for A/V sync.

**`audio-worklet.js` (210 lines)** — the jitter buffer, running on the audio render thread. A
planar ring (separate L/R regions, power-of-two capacity, absolute uint32 positions) read either
from shared memory or from a private copy fed by `postMessage`. Holds audio back to a commanded
depth, corrects drift only at silence boundaries, adapts its own target when uncommanded, and
posts stats every 96 quanta.

**`input.js` (532 lines)** — remote input capture and the binary wire protocol. Creates three data
channels (`fast` unordered/0-retransmit, `reliable`, `bulk`), captures pointer/wheel/gesture/key/
composition/clipboard/drag-drop events, coalesces moves through a `MessageChannel` flush, and
encodes everything into pre-allocated `DataView`s. Exposes `window.RemoteInput`.

**`nosleep.js` (1 line, 16 KB)** — NoSleep.js v0.12.0 (Rich Tibbett, MIT), a vendored minified
webpack UMD bundle, compiled to ES5. Uses `navigator.wakeLock` when present, otherwise plays a
hidden looping base64 video. The only third-party code on the page.

**`styles.css` (514 lines)** — all page styling. Deliberately avoids flex `gap` (sibling spacing is
done with `> * + *` margin rules throughout), but does depend on the `inset` shorthand for every
full-bleed layer (see §14).

**`logo.svg` (7 lines)** — three coloured rounded rectangles, the favicon and the brand mark.
Static, no issues.

**`at-capacity.html` (80 lines)** — a standalone page returned with HTTP 503 when
`state.display_slot_unavailable()` is true (`server.rs:381-405`), i.e. the macOS AirPlay display
tier which can only drive one display. Self-contained, polls `/` with `XMLHttpRequest` every 4 s
and reloads on a 200 (`at-capacity.html:69-78`).

---

## 2. Function catalog

### `index.html` (inline script)

| line | function | purpose / notable behavior |
|---|---|---|
| 206 | `nowMs()` | `performance.now()` with a `Date.now()` fallback. |
| 208 | `showLinkStatus(msg)` | Toggles the `#linkStatus` chip and the `.stalled` class that dims the picture. |
| 214 | `markAlive()` | Resets `lastAliveAt`/`refusedStreak` and clears the degraded chip. Called from four independent liveness sources. |
| 223 | `enterDegraded()` | Shows "Reconnecting…" or "Offline". Nothing actually reconnects — recovery is the `/reconfig` poll succeeding. |
| 229 | `evaluateLink()` | Escalates degraded → `linkLost` after `REFUSED_STREAK` (3) refusals or `DROP_AFTER_MS` (5000) of silence. |
| 242 | `linkLost(msg)` | Tears down and returns to the join form with an error. |
| 254 | `window.SEHealth` | `{ alive, bye(code) }` — the hook `input.js` calls on `PONG` and on the `BYE` opcode. |
| 263 | `showError(msg)` | Shows the error, stops the lock, and un-hides the join form. |
| 276/277 | `showCanvas()` / `showVideo()` | The canvas↔video swap: pure `style.display` toggling. |
| 279 | `freshCanvas()` | Replaces the `#canvas` element — required because a canvas whose control was transferred to an OffscreenCanvas cannot be reused. |
| 289 | `onStreamLive()` | First-frame handler: clears the watchdog, hides the join form, starts the lock and the reconfig poll, and calls `refineRefreshRate()`. |
| 304 | `startReconfigPoll()` | Fetches the initial epoch pair, then arms the probe. |
| 316 | `armProbe()` | `setTimeout(probeTick, suspect ? 300 : 1000)`. |
| 322 | `probeTick()` | `fetch('/reconfig')`; classifies a fast failure (<400 ms) as "connection refused" versus a timeout. |
| 346 | `applyReconfig(j)` | Applies `audioSink`, compares `kick` and `epoch` against the stored values. |
| 364 | `kickDevice(msg)` | Terminal: sets `kicked`, tears down, shows the message. |
| 374 | `reconfigure()` | Full teardown + `start(true)`. The single most expensive operation the client performs. |
| 386/387 | `onViewportSettled(cb)` / `emitViewportSettled()` | A tiny subscriber list; the only subscriber is the viewport-mode resizer, plus a hard-coded call into `RemoteInput`. |
| 392 | `readOrientationPortrait()` | Four-tier fallback: `screen.orientation.type` → `window.orientation` → `matchMedia` → viewport aspect. |
| 410 | `engineSwapsScreenOnRotation()` | Returns false only for iOS, detected by `navigator.platform` + `maxTouchPoints`. |
| 415 | `getDpr()` | `devicePixelRatio`, else a `matchMedia` `min-resolution` ladder from 4 down to 1 in 0.5 steps. |
| 428 | `screenGeom()` | `screen.width/height`, transposed to match the detected orientation. |
| 441/442 | `geomKey(g)` / `relayGeomChange()` | Change detection; a changed key calls `reconfigure()`. |
| 453 | `onGeomChange()` | Debounced settle loop, 120 ms × up to 6 tries, then relay + emit. |
| 486 | `liveEl()` | Returns whichever of canvas/video is currently displayed. |
| 498 | `enterFullscreen(el)` | Four vendor prefixes plus the iOS `video.webkitEnterFullscreen`. **Only the unprefixed branch returns a Promise** — see §8. |
| 506/509 | `currentFullscreenEl()` / `fullscreenTarget()` | Prefix-tolerant readers; the target is `#stage` when it supports any request method, else the live element. |
| 516/522 | `sizeToViewport()` / `clearViewportSize()` | Non-fullscreen ("viewport") mode sizing. |
| 526 | `acquireKeyboardLock()` | `navigator.keyboard.lock()` with no argument list, in a try. |
| 531 | `acquirePointerLock(el)` | Four prefixes; if none exist, falls back to hiding the cursor with CSS. |
| 542/558 | `installGestureHandler()` / `removeGestureHandler()` | A capture-phase passive listener on nine event types that re-enters fullscreen / re-acquires pointer lock on any user gesture. |
| 567/571 | `enterViewportMode()` / `exitViewportMode()` | |
| 575/579 | `disableNoSleep()` / `enableNoSleep()` | Both fully wrapped in try/catch; `enable()`'s rejection is swallowed. |
| 587 | `startLock()` | The whole second-screen mode: gesture handler, focus, visibility/beforeunload/pageshow handlers, NoSleep, fullscreen, pointer lock, keyboard lock. |
| 646 | `stopLock()` | Unwinds all of the above. |
| 677 | `sendLeave()` | `navigator.sendBeacon('/leave')` + `pc.close()`, on `pagehide` only. |
| 685 | `getIceServers()` | `fetch('/ice-config')`, `[]` on any failure. |
| 693 | `hasTurnServer(iceServers)` | Regex `^turns?:` over `urls`/`url`, array or scalar. |
| 700 | `waitForIceGathering(pc, timeoutMs)` | Waits for `iceGatheringState === 'complete'` or the timeout. Non-trickle. |
| 714 | `tryPlay()` | `video.play()`; on rejection reveals the "Tap to start playback" button. `<video>` path only. |
| 724 | `teardownSession()` | Closes input, all four timers, the worker, `SEAudio`, and the peer connection. |
| 736 | `fallbackToVideo(reason)` | One-way (guarded by `usingFallback`), logs to console, full renegotiation. |
| 744 | `handleRenderCount(n)` | The render-stall watchdog: compares rendered-frame count against `inbound-rtp.bytesReceived`; two stalls with bytes still arriving ⇒ fall back. |
| 770 | `negotiatedProfileLevelId(sdp)` | Parses the answer SDP for the H.264 `profile-level-id`, passed to the worker as a codec hint. |
| 789 | `makeTransformWorker()` | Fetches the worker source and builds a blob worker; falls back to a direct URL. Memoised in `__workerBlobUrl`. |
| 805 | `start(preferWebCodecs)` | The entire connection setup — see §11. |
| 1003 | `httpsUrl()` | Builds the HTTPS URL from `/net-config`, carrying id/name/otp/fs in the query string. |
| 1020/1038 | `offerHttpsUpgrade()` / `offerHttpsOnLoad()` | The two entry points to the HTTPS modal. |
| 1032 | `beginStreaming()` | HTTP + no WebCodecs ⇒ offer HTTPS; else `start(true)`. |
| 1072 | `detectOs()` | UA/platform sniff, iPadOS-aware. |
| 1083/1091 | `medianRate(times)` / `getRefreshRate(samples=60)` | 60-sample `requestAnimationFrame` median, with a `samples*40+1000` ms timeout. |
| 1106/1107 | `clampRefresh(r)` / `screenRefreshRate()` | Rounds to 10 Hz; `screen.refreshRate` is non-standard and effectively always absent. |
| 1111/1119 | `relayRefreshRate(rate)` / `refineRefreshRate()` | ≥5 Hz difference ⇒ `reconfigure()`. |
| 1125 | `validate()` | Enables Connect on a valid 12-char session id + name + 6 digits. |
| 1130 | `prefillFromUrl()` (IIFE) | Reads `id/name/otp/fs`, then `history.replaceState`s the secrets out of the URL. |
| 1200/1203 | `compatPrefHidden()` / `setCompatPrefHidden(h)` | `se.compat.hideLimited` in localStorage. |
| 1210 | `checkBrowserRequirements()` | The capability report — see §14. |
| 1268 | `renderCompatModal(apis, blocking)` | Builds the modal with `createElement`/`textContent` (no innerHTML — good). |
| 1303/1332 | `compatGate()` / `sameDeviceGate()` (IIFEs) | Blocking vs optional gating; the same-device warning. |
| 1343 | `showPermModal(onContinue, onSkip)` | The microphone explainer. |
| 1354 | (submit listener) | Builds `joinData`, then the microphone flow, then `beginStreaming()`. |

### `transform-worker.js`

| line | function | purpose / notable behavior |
|---|---|---|
| 39 | `self.onmessage` | Handles `canvas` (gets a 2D context with `{alpha:false, desynchronized:true}`), `codechint`, `stats`, `rendercount`, `encodedstream`. |
| 57 | `reportAvSync(tsTicks)` | Unwraps the 32-bit RTP timestamp, maintains an asymmetric (min-tracking) EMA of the paint delay, estimates fps from tick deltas, posts `avsync` at most every 200 ms. |
| 95 | `postStats()` | Debug-only counters. |
| 105 | `makeDecoder()` | `VideoDecoder` whose `output` callback sizes the canvas once and `drawImage`s immediately; `error` resets and requests a key. |
| 135 | `ensureConfigured()` | Probes candidates with `isConfigSupported`, then `configure({ codec, optimizeForLatency: true })`. |
| 168 | `resetAndRequestKey()` | Closes the decoder, latches `waitingForKey`, forces a key request. |
| 178 | `requestKey(force)` | Rate-limited to 250 ms. **No-op unless `transformer` exists** (ScriptTransform path only). |
| 189 | `self.onrtctransform` | ScriptTransform entry point; the only assignment of `transformer`. |
| 199 | `startPump(readable)` | The frame loop: drop deltas while waiting for a key, backlog check, `decode()`. |

### `audio.js`

`detectCapabilities` (13), `sharedMemoryOk` (23), `detectIOS` (83), `detectHasEarpiece` (91),
`initSpeakers` (95), `micWanted` (118), `primeMicPermission` (123), `micReady` (143),
`releasePrimedStream` (151), `unlockAndEnumerate` (157), `needsMicPermission` (181),
`listOutputs` (200), `buildOutputs` (211), `applySession` (223), `acquireMicHold` (228),
`releaseMicHold` (242), `revertSink` (248), `ensureCategoryBridge` (262), `startDeviceWatch` (284),
`reEnumerateAndPost` (307), `postOutputs` (323), `setSpeaker` (341), `setSpeakerIdentity` (413),
`outputLatencyMs` (418), `maybeUpdateTarget` (426), `setVideoDelay` (453), `setMuted` (459),
`prepare` (465), `resume` (494), `makeSharedRing` (500), `startWorker` (509), `stopWorker` (534),
`attachDataChannel` (540), `attachFallbackStream` (559), `activePath` (578), `teardown` (582),
`getSyncInfo` (594). Exported surface at 610-631.

Notable: `maybeUpdateTarget` (426) is the A/V sync core — `videoDelay − preEnqueue − outputLatency`,
wrapped modulo the RTP rollover period `P_MS = 2³²/90 ms`, clamped to [10, 400] ms, with the
unachievable remainder recorded in `residualOffsetMs` and never acted on.

### `audio-worker.js`

`setupShared` (27), `writeShared` (36), `postSamples` (57), `onDecoded` (65), `makeDecoder` (102),
`handlePacket` (121), `self.onmessage` (145).

### `audio-worklet.js`

`AudioJitterProcessor` — `constructor` (13), `onMessage` (54), `available` (68), `advanceRead` (73),
`enqueue` (78), `peekPeak` (99), `dequeueInto` (113), `process` (125), `maybePostStats` (194).

### `input.js`

`applyMouseCurve` (21), `refreshSize` (38), `clamp01`/`clampI16` (54-55), `channelFor` (57),
`rawSend` (70), `fastSend` (74), `send` (79), `modMask` (84), `sourceByte` (97), `normX`/`normY`
(100-101), `sendPointer` (103), `sendPointerBatch` (119), `sendZoom` (142), `sendWheel` (148),
`sendMouseDelta` (157), `sendKey` (167), `sendText` (182), `sendClipboard` (193), `sendDrag` (206),
`sendDrop` (215), `sendState` (245), `sendResize` (246), `sendPing` (253), `onReliableMessage` (260),
`armFlush` (286), `flushStroke` (287), `flushMoves` (291), `coalescedOf` (299), `onPointerMove` (303),
`onPointerDown` (339), `endPointer` (345), `pinchStart` (355), `updatePinch` (360), `pinchEnd` (371),
`twoFingerDist` (372), `isComposingKey` (378), `onKeyDown` (379), `onKeyUp` (384), `clearHeld` (390),
`ndx`/`ndy` (398-399), `on` (401), `installListeners` (405), `forwardClipboard` (483), `setup` (490),
`teardown` (514).

---

## 3. Threads

The browser gives this page five execution contexts. Priorities are the UA's, not ours.

1. **Page main thread** — the inline script, `input.js`, `audio.js`, all DOM and all `fetch`.
   Also relays audio samples worker→worklet on the non-SAB path (`audio.js:524-526`). Lifetime:
   the document.
2. **Transform worker** (`index.html:789-802` / `index.html:836`) — owns the `VideoDecoder`, the
   `OffscreenCanvas` and its 2D context, `framesIn/Decoded/Rendered`, `estimatedFps`,
   `videoDelayEmaMs`. Created per `start()` when WebCodecs is used, `terminate()`d in
   `teardownSession()` (`index.html:731`). Because the blob URL is memoised but the worker is not,
   every reconfigure gets a fresh worker with fresh module globals.
3. **Audio decode worker** (`audio.js:509-532`, `new Worker('/audio-worker.js')`) — owns the
   `AudioDecoder`, `lastSeq`, the scratch planes, `preEnqueueEmaMs`, and the *write* half of the
   ring. Created on `attachDataChannel`, terminated in `teardown()`.
4. **AudioWorklet render thread** (`audio.js:472-479`) — `AudioJitterProcessor`, the *read* half of
   the ring, all buffer-depth policy. Real-time; runs every 128 frames (2.67 ms at 48 kHz). It
   allocates nothing per block and takes no locks other than `Atomics` on the control block.
5. **WebRTC internal threads** — not ours, but the receiver's depacketizer feeds the transform
   worker's readable and the `<video>` element.

Two threads are created *lazily on the join path* (the transform worker via a `fetch` +
`URL.createObjectURL`, `index.html:789-802`) which adds a round trip to the first frame.

## 4. Locks

There is no mutual exclusion in the JavaScript sense; the equivalents are:

- **`Atomics` on the shared control block** (`Int32Array`, 16 slots, 64 bytes). `W = 0`, `R = 1`,
  `OVERRUNS = 2`; slots 3-15 unused. Writer: `audio-worker.js:36-55` (`Atomics.load` W and R,
  `Atomics.store` W, `Atomics.add` OVERRUNS). Reader: `audio-worklet.js:68-76`, `:36`, `:62-63`,
  `:197`. This is a single-producer/single-consumer ring with no CAS, so it is lock-free and
  correct provided exactly one writer and one reader — which holds. **Hot path** (200 writes/s,
  375 reads/s). No `Atomics.wait` anywhere, so the render thread never blocks.
- **`state.prepared`** (`audio.js:466`) — a plain boolean idempotence latch on `prepare()`. Because
  `prepare()` is `async` and the flag is only set at the end (`audio.js:486`), two concurrent
  callers both proceed and the second overwrites `state.ctx`/`state.node`/`state.gain`, orphaning
  the first `AudioContext`. `prepare()` is in fact called twice on the join path —
  `index.html:1385` and `index.html:821` — so this race is live, though in practice the first
  usually completes first.
- **Boolean re-entrancy guards**: `usingFallback` (`index.html:736`), `reconfiguring`
  (`index.html:374`), `kicked` (`index.html:364`), `leaveSent` (`index.html:677`), `installed` /
  `active` (`input.js:32-33`), `speakers.inited` / `speakers.watching` (`audio.js:96`, `:285`),
  `waitingForKey` / `configured` / `sizedCanvas` / `renderedOnce` (`transform-worker.js:5-8`).
- **Gates masquerading as locks**: `mouseGateOpen` / `absGateOpen` / `flushTimer`
  (`input.js:279-286`) serialise move sends to one immediate send plus one coalesced flush per
  task.

Nothing here can deadlock. The equivalent of "a guard held across an await" is the
`await`-then-mutate pattern in `prepare()` above, and the `sizedCanvas` latch in §"buffers".

## 5. Buffers and copies

**Video, per frame (WebCodecs path):**
1. Depacketized H.264 access unit → `RTCEncodedVideoFrame.data` (`ArrayBuffer`). Owned by the UA.
2. `new EncodedVideoChunk({ data: encodedFrame.data })` (`transform-worker.js:252-256`) — the spec
   copies the buffer here. **One CPU copy per frame, one allocation per frame.**
3. `decoder.decode(...)` → a `VideoFrame` in the output callback. GPU- or CPU-backed depending on
   the decoder; not observable.
4. `ctx.drawImage(frame, 0, 0, canvas.width, canvas.height)` (`transform-worker.js:116`) — the
   only paint. **The 5-argument destination-rect form**, so a scale is applied even when the sizes
   match. Then `frame.close()` in a `finally` (`:124-126`) — correctly, so no `VideoFrame` leak.

There is no `requestAnimationFrame` and no `requestVideoFrameCallback` anywhere in the render loop:
paint happens inline in the decoder callback, on the worker thread. That is the right choice for
latency (it costs at most one compositor frame instead of two) and it is the single best thing
about this code path.

**Video, `<video>` path:** `video.srcObject = e.streams[0]` (`index.html:921`) — the UA owns
everything, and the browser's jitter buffer is reintroduced.

**Audio, per 5 ms packet (SAB path):** DataChannel `ArrayBuffer` → transferred (zero-copy) to the
worker (`audio.js:551`, `postMessage(buf, [buf])`) → `EncodedAudioChunk` (a copy) → decoded
`AudioData` → **two `copyTo` calls into reused scratch** (`audio-worker.js:81-83`, planar, so two
memcpys) → `ad.close()` → **two `TypedArray.set` memcpys into the ring** (`audio-worker.js:48-52`,
split into two more if the write wraps) → the worklet's `dequeueInto` does **two more `set`
memcpys** into the output block (`audio-worklet.js:113-123`). So four to eight memcpys of ≤240
frames each, and **zero allocations** steady-state (`scratchL`/`scratchR` are grown-once,
`audio-worker.js:74-77`).

**Audio, per packet (non-SAB path):** as above, plus `postSamples` allocating **two fresh
`Float32Array`s per packet** (`audio-worker.js:58-59`) plus two more memcpys, transferred to the
page, then re-transferred to the worklet (`audio.js:525`), then `enqueue`'s two `set` memcpys
(`audio-worklet.js:90-95`). 400 allocations/s and two main-thread hops per packet.

**Input:** all fixed-size messages use module-level pre-allocated `ArrayBuffer`s and `DataView`s
(`input.js:43-52`) — `moveBuf` 40 B, `wheelBuf` 15 B, `deltaBuf` 9 B, `resizeBuf` 9 B, `lifeBuf`
2 B, `pingBuf` 9 B, `zoomBuf` 5 B, `keyScratch` 1024 B, `batchBuf` 970 B. Genuinely
allocation-free on the hot path. The variable-length messages (`sendText`, `sendClipboard`,
`sendDrag`, `sendDrop`) allocate per call, which is fine given their frequency — except
`sendDrop`, which reads whole files into memory (`input.js:221`) and concatenates them into one
buffer (§8).

## 6. Queues

| queue | depth | bounded | blocking | back-pressure |
|---|---|---|---|---|
| Shared audio ring | `CAP_FRAMES = 65536` frames = 1.365 s | yes, power-of-two | never (no `Atomics.wait`) | Writer computes `free` and, if short, **writes a partial packet and increments `OVERRUNS`** (`audio-worker.js:41-45`); the tail is silently dropped mid-packet. |
| Private (postMessage) audio ring | same | yes | never | Same partial-write behaviour in `enqueue` (`audio-worklet.js:81-87`), counting into `this.overruns`. |
| Worklet hard-drop | `effTarget + hardSlackFrames` where `hardSlack = 250 ms` | — | — | `advanceRead(avail - effTarget)` — discards everything above the target in one step (`audio-worklet.js:138-142`). |
| `VideoDecoder` internal queue | `decodeQueueSize`, unbounded by the UA | no | no | Ours: >`max(3, fps*0.05)` twice ⇒ stop decoding and demand a keyframe (`transform-worker.js:229-243`). |
| `fast` DataChannel | `bufferedAmount > FAST_BUFFER_MAX (8192)` ⇒ **drop the message** (`input.js:74-78`) | soft | no | Silent drop. Correct policy for an unordered lossy channel, but uncounted. |
| `reliable` / `bulk` DataChannels | unbounded | no | no | None; `send()` throws are swallowed (`input.js:70-73`). |
| `pendingMoves` / `pendingStrokes` Maps | one entry per live pointer id | effectively | no | Drained every task via the `MessageChannel` flush (`input.js:283-298`). |
| `viewportSettledSubs` | 1 subscriber | — | — | — |

## 7. Syscall / FFI boundaries

Browser-API boundaries, i.e. every point where the page leaves JavaScript:

- **WebRTC**: `RTCPeerConnection` (`index.html:816`), `addTransceiver`, `createOffer`,
  `setLocalDescription`, `setRemoteDescription`, `createDataChannel` (`input.js:493-495`),
  `getStats` (`index.html:748`, `:892`), and the two insertable-streams APIs —
  `receiver.transform = new RTCRtpScriptTransform(...)` (`index.html:862`) and
  `receiver.createEncodedStreams()` (`index.html:866`). Also the two latency knobs,
  `receiver.playoutDelayHint` (Chromium-only) and `receiver.jitterBufferTarget`, both
  feature-detected with `in` and both wrapped in try/catch (`index.html:831-833`, `:873-875`,
  `:922-923`).
- **WebCodecs**: `VideoDecoder` + `VideoDecoder.isConfigSupported` + `EncodedVideoChunk`
  (`transform-worker.js`), `AudioDecoder` + `EncodedAudioChunk` + `AudioData.copyTo`
  (`audio-worker.js`). All secure-context-gated, which is what makes the HTTP path fall back.
- **Web Audio**: `AudioContext({latencyHint, sampleRate})`, `audioWorklet.addModule`,
  `AudioWorkletNode`, `createGain`, `createMediaStreamDestination`, `ctx.setSinkId`,
  `ctx.baseLatency` / `ctx.outputLatency` (`audio.js:418-424`), `AudioWorkletProcessor` /
  `registerProcessor`.
- **Media devices**: `getUserMedia`, `enumerateDevices`, `mediaDevices.selectAudioOutput` (probed
  but never called — `audio.js:104`), `HTMLMediaElement.setSinkId`, `navigator.audioSession.type`
  (iOS), `navigator.permissions.query({name:'microphone'})` (`audio.js:186`, throws on Safari and
  is caught).
- **Screen/lock**: `requestFullscreen` ×4 prefixes, `requestPointerLock` ×3 prefixes,
  `navigator.keyboard.lock/unlock`, `navigator.wakeLock` (via NoSleep), `screen.orientation`.
- **Threading/transfer**: `Worker`, `MessageChannel`, `OffscreenCanvas` via
  `transferControlToOffscreen`, `SharedArrayBuffer` + `Atomics`, `URL.createObjectURL` + `Blob`.
- **Storage/network**: `localStorage` (four keys, every access in a try), `fetch`,
  `navigator.sendBeacon`, `history.replaceState`, `XMLHttpRequest` (only in `at-capacity.html`).
- **Encoding**: `TextEncoder` (`input.js:13`), `DataView` including `getBigUint64`/`setBigUint64`
  (BigInt — see §14), `URL`/`URLSearchParams`.

The one *link-time* hazard analogous to the macOS dyld rule is **BigInt literal syntax**:
`audio-worker.js:137` contains `1000n`. On a browser without BigInt this is a **parse error**,
which kills the whole worker file rather than one call. It is safe today only because
`audio-worker.js` is never loaded unless `AudioDecoder` exists (`index.html:170-171`,
`audio.js:509-517`), which implies a browser new enough to have BigInt. `input.js` — which *is*
loaded unconditionally on every page view — uses `BigInt(...)` as a *call* and
`setBigUint64`/`getBigUint64` as *methods* (`input.js:221`, `:238-239`, `:254-257`, `:266`), which
degrade to runtime errors inside existing try/catch rather than parse errors. That distinction is
load-bearing and undocumented.

## 8. Panics (the JS equivalent: unguarded throws and unhandled rejections)

- **`index.html:490` `const noSleep = new NoSleep();`** — top-level in the inline script, no guard.
  If `/nosleep.js` fails to load, this throws `ReferenceError` and **everything declared after
  line 490 never executes**, including the entire join form wiring and the submit handler. Every
  other optional global on the page is guarded (`window.SEAudio &&`, `window.RemoteInput &&`,
  `'RTCRtpReceiver' in window`). → finding `minor-client-nosleep-unguarded-constructor.md`.
- **`index.html:554` and `index.html:633`** — `.catch()` on the return of `enterFullscreen()`,
  which is `undefined` on any browser without unprefixed `requestFullscreen`. At :633 this rejects
  `startLock()`, which is called un-awaited from `onStreamLive()` (`index.html:301`), so pointer
  lock and keyboard lock are silently skipped. → finding
  `major-client-fullscreen-catch-on-undefined.md`.
- **`transform-worker.js:116`** `ctx.drawImage(...)` and `:110-113` `canvas.width` — inside a
  `try { } finally { frame.close(); }` with **no `catch`**, so a throw propagates out of the
  decoder output callback. In practice the UA reports it and continues, but `framesRendered` stops
  advancing and the render watchdog eventually demotes to `<video>`.
- **`transform-worker.js:145`** `VideoDecoder.isConfigSupported` is called on a codec string
  without validating it; `preferredCodec` is regex-validated first (`:44`) so this is safe.
- **`input.js:373-374` `twoFingerDist()`** — `a.x` / `b.x` where `a`/`b` come from
  `pinchPts.values()`. Guarded by `pinchPts.size === 2` at both call sites (`:358`, `:363`), so it
  cannot fire today, but the guard is at the caller, not in the function.
- **`input.js:262` `new DataView(ev.data)`** — throws if `binaryType` ever yields a Blob. Wrapped
  in try/catch (`:261`, `:272`).
- **`audio-worker.js:131` `new Uint8Array(buf, HDR_BYTES)`** — safe, length is checked at `:122`.
- **`audio-worklet.js:22-24`** — the SAB views are constructed in a try/catch that falls back to
  the private ring; a size mismatch degrades silently rather than throwing (§9).
- Everything in `audio.js`'s `speakers` machinery is wrapped; the exception is
  `needsMicPermission`'s `await listOutputs()` at `:190`, which is outside the try but is itself
  internally guarded.

## 9. Silent fallbacks and swallowed errors

This is the dominant characteristic of the file set. Every degradation below is invisible to the
host, and most are invisible to the user.

| # | degradation | where | logged? | host sees it? |
|---|---|---|---|---|
| 1 | WebCodecs → `<video>` (5 s watchdog, render stall, `createEncodedStreams` throw, any setup error) | `index.html:736-742`, `:868`, `:912`, `:995-1000` | `console.warn` | **no** |
| 2 | Keyframe request is a no-op on the legacy path | `transform-worker.js:182` | no | **no** |
| 3 | Decoder backlog ⇒ discard until keyframe | `transform-worker.js:232-238` | `console.warn`, and the message text is wrong ("dropping to freshest frame" — it does not) | **no** |
| 4 | SharedArrayBuffer unavailable ⇒ postMessage ring | `audio.js:500-507`, `:556` | one `console.log` | **no** |
| 5 | `prepare()` fails ⇒ **no audio at all**, and the Opus-track fallback was never negotiated | `audio.js:488-491`, `:540-544` | `console.warn` ×2 | **no** |
| 6 | `AudioContext.resume()` refused (autoplay policy) | `audio.js:494-498` | no — empty catch | **no** |
| 7 | `setSinkId` unsupported / throws ⇒ output stays on default | `audio.js:365-410` — one `try` wrapping the whole function with `catch (_) {}` | no | Host is told the sink was *offered*, never that it failed |
| 8 | `getUserMedia` declined ⇒ unlabelled outputs; "Not now" ⇒ **empty** output list reported as `supported: true` | `audio.js:136-139`, `:325` | no | Reported as "no outputs", not "denied" |
| 9 | Wake lock refused / NoSleep video `play()` rejected | `index.html:583` `p.then(ok, () => {})` | NoSleep's own `console.error` only | **no** |
| 10 | Fullscreen request rejected | `index.html:554`, `:600`, `:628`, `:633` — four sites, all swallowed | no | **no** |
| 11 | Pointer lock / keyboard lock refused | `index.html:526-540` | no | **no** |
| 12 | Every DataChannel `send()` throw (message too large, channel closing) | `input.js:70-73`, `:77` — empty catch | no | **no** |
| 13 | `fast` channel drop above 8 KB buffered | `input.js:76` | no, and uncounted | **no** |
| 14 | `sendBeacon('/leave')` failure | `index.html:680` | no | **no** |
| 15 | `localStorage` unavailable (private mode) — device token, fullscreen pref, compat pref all silently non-persistent | `index.html:952-953`, `:970`, `:1059-1062`, `:1200-1208`; `input.js:16-20` | no | **no** |
| 16 | `/ice-config`, `/net-config`, `/reconfig` fetch failures | `index.html:685-689`, `:993-997`, `:308-311` | no | partially — the host sees the poll stop |
| 17 | `/audio-outputs` POST failure | `audio.js:337` `.catch(() => {})` | no | **no** |
| 18 | Transform-worker blob creation failure ⇒ direct-URL worker | `index.html:798-801` | `console.warn` | **no** |
| 19 | Audio decoder error ⇒ silently rebuilt, `lastSeq` reset | `audio-worker.js:106-112` | `console.warn` | **no** |
| 20 | `AudioData.copyTo` throws ⇒ packet dropped | `audio-worker.js:84-87` | no | **no** |
| 21 | Worklet ring overrun / underrun | `audio-worker.js:42`, `audio-worklet.js:84`, `:182` | counted, posted to the page every 256 ms, and **read by nothing** | **no** |
| 22 | `emitViewportSettled` subscriber throw | `index.html:388` | no | **no** |

Grep totals for the swallowing idiom in this directory: `catch (_) {}` appears **63** times across
`index.html`, `input.js` and `audio.js`.

## 10. TODO / FIXME / HACK / XXX

None. `grep -n "TODO\|FIXME\|HACK\|XXX"` over all nine files returns nothing. The comment density
is very low throughout — `transform-worker.js` and `audio-worklet.js` have essentially none, and
the two hardest pieces of reasoning in the territory (the RTP-rollover modulo in
`audio.js:431-433` and the asymmetric EMA in `transform-worker.js:71`) are both uncommented.

## 11. Latency path

Every file in this territory is on a latency path. Stage costs below are structural (what the code
makes unavoidable), not measured — nothing in this territory measures anything usefully (§9 #21,
and finding `major-client-no-glass-to-glass-telemetry.md`).

### Join path (submit → first painted frame) — fully serial

| stage | where | cost |
|---|---|---|
| `initSpeakers` + `needsMicPermission` (`permissions.query`, `enumerateDevices`) | `index.html:1397` | ms–100s of ms |
| microphone modal + browser prompt + `getUserMedia` | `index.html:1399`, `audio.js:123-149` | **0–12 000 ms** |
| `prepare()` — `AudioContext` + `addModule('/audio-worklet.js')` fetch | `index.html:1385`, `audio.js:465-492` | 1 RTT + compile |
| `fetch('/ice-config')` | `index.html:808` | 1 RTT |
| `new RTCPeerConnection` + `createOffer` + `setLocalDescription` | `index.html:816`, `:948-949` | ms |
| `fetch('/transform-worker.js')` + blob + `new Worker` | `index.html:789-802` | 1 RTT + worker spawn |
| **`waitForIceGathering`** | `index.html:950` | **0–3000 ms (6000 with TURN)** |
| `POST /whep` — host creates the virtual display, up to `DISPLAY_ATTACH_TIMEOUT` 5 s | `index.html:955-970` | **0–5000 ms** |
| `setRemoteDescription` + ICE connectivity + first keyframe | | 100s of ms |
| **then** `refineRefreshRate()` may tear the whole thing down and repeat it | `index.html:301`, `:1119-1123` | **the whole chain again** on any 90/120 Hz client |

Serialisation points, explicitly: the microphone prompt gates ICE gathering; ICE gathering gates
the WHEP POST; the WHEP POST gates display creation. None of these overlap, and two of the three
have multi-second timeouts.

### Steady-state video (per frame)

RTP arrival → UA depacketize → transform readable → `EncodedVideoChunk` copy →
`decoder.decode()` → decoder queue (target ≤3 frames) → `output` callback → `drawImage` →
compositor. The client-controllable knobs are `jitterBufferTarget = 0` and
`playoutDelayHint = 0` (`index.html:832`, `:874`), `optimizeForLatency: true`
(`transform-worker.js:157`), and painting in the decoder callback rather than under `rAF`. Those
are the right choices. The costs left on the table are the per-frame chunk copy, the scaled
`drawImage`, and the absence of a hardware-acceleration preference
(`speculative-client-decoder-no-hardware-acceleration-hint.md`).

The `<video>` fallback path reintroduces the browser jitter buffer and is a step change, not a
gradient — which is why the aggressiveness of the fallback triggers (§9 #1, #3) matters as much
as their existence.

### Steady-state audio

DataChannel → transfer → worker decode → ring → worklet holds to the commanded depth → output.
The commanded depth is the dominant term and is set entirely by the A/V sync arithmetic
(`audio.js:426-451`), clamped to ≥10 ms. Everything else is memcpys. On the non-SAB transport,
two main-thread hops per 5 ms packet are inserted.

### Input

Event → gate/coalesce → `MessageChannel` flush (one task) → `fast` (unordered, 0 retransmits) or
`reliable` channel. The design is good: the first move of each task is sent immediately and only
subsequent ones are coalesced (`input.js:309-320`). `getCoalescedEvents()` is used **only** for
pen (`input.js:333`), not for mouse or touch — so high-rate touch samples are decimated to one per
task.

## 12. Stale references

- `server.rs:369` (immediately upstream of this territory, and the comment that explains this
  page's cross-origin isolation): `// Cross-origin isolation (§6.4): ...` — a section reference to
  a design document that does not exist in the tree.
- `index.html:105` — "Switching to the **sucre** version", plus "choose to More Info/Advanced" and
  a raw `-->` arrow in body copy. → finding `minor-client-https-modal-typo-and-arrow.md`.
- `index.html:845-846` — `'[webcodecs] decoder backlog (' + m.data.size + '); dropping to freshest
  frame'`. The code does not drop to the freshest frame; it stops decoding entirely until a
  keyframe arrives (`transform-worker.js:235-238`).
- `input.js:17` — `localStorage.getItem('rib.settings')`. `rib.` is an orphaned naming scheme
  (every other key on this page is `se.`), and nothing in the repository ever writes it.
- CLAUDE.md vs code: "`transform-worker.js` … reports the display lag" — it reports a number that
  is offset by the client's Unix epoch and is not a lag (§ finding
  `major-client-no-glass-to-glass-telemetry.md`). CLAUDE.md also describes the ring ABI as a
  two-file contract; it is a three-file contract, because `audio.js` owns the allocation size.
- `at-capacity.html:63-67` tells the user to "use a Mac running macOS 10.15 Catalina or later",
  but per CLAUDE.md 10.15 is already the supported floor and the one-display AirPlay tier only
  applies below 10.14.4 — so this page should be unreachable on any supported host. Either the
  tiering or the copy is wrong.
- No AI-tooling references anywhere in this territory.

## 13. Dead code candidates

| item | where | evidence |
|---|---|---|
| `LOCK_KEYS` | `index.html:488` | `grep -n LOCK_KEYS index.html` returns only the declaration. |
| `TICK_MS` | `input.js:275` | Declared, never referenced. Also misdescribes the flush, which is a `MessageChannel` task, not a 4 ms tick. |
| `mouseRelative` | `input.js:14` | `const … = true`, never reassigned; the `&&` at `:305` is a no-op. |
| `RemoteInput.rtt` | `input.js:530` | `lastRttMs` is computed every second and has no reader anywhere in `static/`. |
| `OP.STATS` (`0x62`) | `input.js:10`, `protocol.rs:31` | Declared on both sides, never sent or handled. |
| `rib.settings` / `settings.sensitivity` / `settings.accel` / `applyMouseCurve` | `input.js:15-28` | Nothing in the repo writes the key, so the curve is permanently identity. |
| `mediaDevices.selectAudioOutput` | `audio.js:104` | Probed into `speakers.hasSelectOut`, which is never read. |
| `speakers.canCategory` earpiece path on non-iPhone | `audio.js:91-92`, `:110` | Gated on `/iPhone/` in the UA, so unreachable on every other device. |
| `AudioWorklet` stats postMessage | `audio-worklet.js:194-207` → `audio.js:480-482` | Stored in `state.stats`, read only by `getSyncInfo()`, which nothing calls automatically. |
| `FLAG_SILENT` / `FLAG_DISCONTINUITY` | `audio-worker.js:121-143` | Byte 12 of every packet is never read. |
| `getSyncInfo()` | `audio.js:594-608` | Exported, no caller. Intended as a console-debug hook. |
| `postStats` / `'stats'` message | `transform-worker.js:95-103`, `index.html:889-909` | Only under `?debug`. |

Collected in finding `minor-client-dead-code-and-unused-constants.md`.

## 14. Capability probing

This territory is, on the whole, a **good** example of capability probing: it selects by feature,
not by version, in almost every case.

Probed correctly (feature/API presence, with a working fallback):
`RTCRtpReceiver.prototype.transform` / `createEncodedStreams` / `VideoDecoder` /
`transferControlToOffscreen` (`index.html:166-169`); `AudioContext.prototype.audioWorklet`,
`AudioDecoder`, `EncodedAudioChunk`, `AudioData` (`audio.js:13-21`); `crossOriginIsolated` +
`SharedArrayBuffer` + `Atomics` (`audio.js:23-27`); `setSinkId` on both `HTMLMediaElement` and
`AudioContext` (`audio.js:102-103`); `navigator.audioSession` (`:106`); `'playoutDelayHint' in
receiver` and `'jitterBufferTarget' in receiver` (`index.html:831-833`, `:873-875`);
`VideoDecoder.isConfigSupported` before `configure` (`transform-worker.js:141`);
`getCoalescedEvents` (`input.js:300`); `'onpointerrawupdate' in window` (`input.js:411`); the four
fullscreen and three pointer-lock prefixes; `screen.orientation` / `window.orientation` /
`matchMedia` / aspect (`index.html:392-408`); `devicePixelRatio` with a `matchMedia` ladder
(`index.html:415-427`). `checkBrowserRequirements()` (`index.html:1210-1266`) turns several of
these into a user-facing report with `blocking` and `optional` severities.

Selected by **UA sniffing rather than capability** (defects per the tier doctrine):
- `detectIOS()` (`audio.js:83-89`) and `detectHasEarpiece()` (`audio.js:91-92`) — the earpiece
  routing tier is gated on `/iPhone/` in the user-agent string. There is no probe for "this device
  has an earpiece"; arguably there cannot be one, but the code does not say so.
- `engineSwapsScreenOnRotation()` (`index.html:410-413`) — the screen-geometry transpose rule is
  chosen by `navigator.platform` matching `iPad|iPhone|iPod` or `Mac` + touch. A rendering-engine
  behaviour is being inferred from a brand string.
- `detectOs()` (`index.html:1072-1081`) — reported to the host and stored in the device config;
  UA-derived, though this one is genuinely just a label.

**Missing tiers:**
- There is no tier between "WebCodecs" and "`<video>`". A browser with `VideoDecoder` but no
  insertable streams (or vice versa) drops straight to the full-jitter-buffer path.
- There is no tier between the SharedArrayBuffer ring and the double-hop postMessage ring; a
  `MessageChannel` between worker and worklet would be an obvious middle tier (finding
  `minor-client-postmessage-audio-ring-crosses-main-thread.md`).
- If `prepare()` fails there is no audio tier at all, because the Opus-track fallback was already
  excluded from the SDP (finding `major-client-no-audio-fallback-when-prepare-fails.md`).

**Version-implicit hazards (the JS analogue of the macOS dyld rule):** parse-time features are the
ones that matter, because a syntax error kills a whole file rather than one call. Survey of all
six JS/HTML script files:

- Optional chaining `?.`, nullish coalescing `??`, logical assignment `||= &&= ??=`, private class
  fields `#x`, class instance fields, top-level `await`, optional catch binding `catch {}`:
  **none present.** Verified by grep.
- `.at()`, `replaceAll`, `flatMap`, `Object.fromEntries`, `globalThis`, `structuredClone`,
  `Promise.allSettled`, `queueMicrotask`, `padStart`, `requestIdleCallback`, `ResizeObserver`,
  `IntersectionObserver`, `AbortController`, `requestVideoFrameCallback`, `createImageBitmap`:
  **none present.**
- Present and ES6-or-earlier, safe on the intended floor: arrow functions, `async`/`await`,
  destructuring, array spread, default parameters (`index.html:1091`), object shorthand methods,
  `for…of` over `Map`/`Set`, getters in object literals, template-free string concatenation
  (the code consistently uses `+`).
- **One parse-time hazard: `1000n` at `audio-worker.js:137`.** Safe only because that file is
  never loaded on a browser without `AudioDecoder`. This is an undocumented invariant.
- **CSS is the real compatibility gap**, not JS: `inset: 0` is used for all six full-bleed layers
  with no long-hand fallback (finding `major-client-css-inset-breaks-older-webkit.md`). The CSS
  correctly avoids flex `gap`, so the constraint is clearly known — `inset` was just missed.
  `accent-color`, `text-wrap: balance` and unprefixed `backdrop-filter` are present but degrade
  harmlessly.

## 15. Questions and cross-cutting concerns

1. **Should the client be allowed to request a keyframe out of band?** The single highest-impact
   finding in this territory is that on Chrome/Edge — the majority client — the WebCodecs path can
   never ask for one, because it takes the legacy `createEncodedStreams` branch and only
   `RTCRtpScriptTransform` has `sendKeyFrameRequest()`. This spans my territory and the WebRTC/
   pipeline territory: either the client flips its transport preference, or the host grows a
   keyframe keepalive, or we add an opcode on the `reliable` channel (`OP.STATS = 0x62` is
   already reserved and unused). Someone needs to own the decision.
2. **There is no clock sync between host and client, and the RTP timestamp epoch is a host
   `Instant`, not a wall clock.** `src-tauri/src/streamer/audio/mod.rs:17-27`. That is fine for A/V
   sync (the offset cancels) and fatal for latency measurement. If this engagement is going to
   optimise glass-to-glass, the first deliverable should be an offset estimate — the `PING`/`PONG`
   exchange on the `reliable` channel already exists and its RTT is already computed and thrown
   away.
3. **Nothing the client learns ever reaches the host.** Twenty-two distinct silent degradations
   (§9), a per-second RTT measurement, worklet underrun counters, decoder backlog counts — all
   console-only. Any latency or compatibility regression on a browser family we do not personally
   test is invisible. I would treat "add a client→host telemetry frame" as a prerequisite for the
   rest of the overhaul, not a nice-to-have. It also spans territories: the host side needs to log
   it (`tprintln!`) and probably surface it in device details.
4. **Three independent liveness mechanisms and four polling loops.** `/reconfig` at 1 Hz,
   `/audio-outputs` + `enumerateDevices` at 0.25 Hz, `PING` at 1 Hz, `getStats` at 0.5 Hz. The
   `/reconfig` poll in particular could move onto the `reliable` data channel, which already
   carries `BYE`. This overlaps the session/epoch design owned by another territory.
5. **`reconfigure()` is a sledgehammer and it is invoked for small things.** Refresh-rate
   refinement, any orientation change, and any host settings epoch bump all cause a full teardown
   and renegotiation, which also takes the host's `DISPLAY_CORRELATION_LOCK`. The refresh-rate one
   is gratuitous (the measurement is available at submit time) and fires on every high-refresh
   phone. Worth asking whether the epoch mechanism could distinguish "resize the display" from
   "renegotiate the transport".
6. **The audio-output-selection feature costs the join path up to 12 seconds** for a feature
   unrelated to streaming. It should be parallelised. Related question for the host/UI territory:
   what does the device-details panel show today when a client reports `supported: true` with zero
   outputs — because that is what "Not now" produces.
7. **`sendDrop` cannot work for any realistic file** (one SCTP message, no chunking, throw
   swallowed). Is drag-and-drop a shipped feature or an unfinished one? If shipped, it needs
   chunking; if not, the code is dead weight on the `bulk` channel.
8. **No test infrastructure exists for this page at all**, and it has no build step, no linter and
   no type checking — ESLint is scoped to `src/`. Every finding here was found by reading. A
   headless smoke test that loads the page against a running `serve` and asserts a first painted
   frame would catch the entire class of "top-level throw kills the script" bugs (§8).

---

**Findings written:** 25 files under `findings/` — 10 `major-client-*`, 14 `minor-client-*`,
1 `speculative-client-*`. (Other agents have also written `client`-prefixed findings; the ones
belonging to this report are listed inline above.)

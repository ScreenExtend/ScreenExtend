# Client page review 1 — correctness and control flow

Branch `worktree-agent-a5109a99927714a08` against `latency-overhaul`, scope
`src-tauri/src/streamer/static/`. Read cold: the diff and the full post-change files first,
then the commit subjects and `docs/TELEMETRY.md`. Line numbers are **post-change**.

Angle: exception paths, promise ordering, races between WHEP/ICE/transform/configure/first
frame, decoder queue and `VideoFrame` ownership, the audio ring producer/consumer math, the
stats v2 byte layout, the PING/PONG clock arithmetic, the letterbox transform, the reconfig
teardown rules, and every path that can end in a black screen or silent audio with no
indication.

---

## Defects

### 1. `waitForIceGathering` is never told it needs a relay candidate — cross-network joins ship an offer with no relay

`index.html:1456`

```js
await waitForIceGathering(pc, hasTurnServer(iceServers) ? 6000 : 3000);
```

The function signature is `waitForIceGathering(pc, timeoutMs, needRelay)` (`index.html:897`).
The third argument is never passed, so `needRelay` is always `undefined`. The candidate
handler therefore always takes the LAN branch:

```js
if (needRelay) { if (type === 'relay') arm(ICE_RELAY_GRACE_MS); }
else if (type === 'host' || type === 'srflx') arm(ICE_HOST_GRACE_MS);   // 200 ms
```

Host candidates are produced in the first few milliseconds, so the wait resolves ~200 ms in.
A TURN allocate is a full round trip to the relay and routinely lands later than that. WHEP is
a single POST with no trickle (`index.html:888-894` says so), so a relay candidate that has not
been gathered by the time the offer is serialised can never be used.

This is a **regression**: the base `waitForIceGathering` (`latency-overhaul:index.html:704`)
waited for `iceGatheringState === 'complete'` or the full 6 s TURN timeout. The code even
computes `hasTurnServer(iceServers)` for the timeout and then drops it on the floor.

**Failing scenario.** A cloud/cross-network session with a configured TURN server on a link
where the relay candidate arrives >200 ms after the first host candidate. The offer carries
host candidates only, none of which is reachable from the host, ICE fails, and the page shows
"Connection failed." (`index.html:1443`). Nothing in the page distinguishes this from a real
network failure.

**Fix.** Compute the flag once and pass it:

```js
const needRelay = hasTurnServer(iceServers);
await waitForIceGathering(pc, needRelay ? 6000 : 3000, needRelay);
```

Consider also arming the host grace *in addition to* the relay grace when `needRelay`, so a
purely local cross-network fallback still gets its 200 ms path.

---

### 2. The out-of-band keyframe request (`0x64`) has no host handler, and the client reports it as a working tier

`input.js:11`, `input.js:386-395`; `index.html:1303-1313`; `transform-worker.js:541-545`

`OP.KEYFRAME_REQUEST = 0x64` is sent on the reliable channel. `git grep 0x64` /
`KEYFRAME_REQUEST` over `src-tauri/src` on this branch finds **no host-side parse arm** —
`streamer/input/protocol.rs` knows `PING/PONG/STATS/BYE` and stops. The comment at
`input.js:380-383` ("A host that does not know this opcode returns None from its parser and
ignores the frame, which is why this is safe to send unconditionally") is describing the
*only* behaviour that exists: the host always ignores it.

The problem is not the wasted 2 bytes, it is that three separate decisions are made on the
assumption that the request worked:

- `requestKeyframe()` returns `true` whenever the channel is merely open (`input.js:394`).
- `index.html:1335` posts `{type:'outofbandkey', available:true}` unconditionally, so
  `transform-worker.js:243` sets `keyTier = KEYREQ.outOfBand (2)` — a tier
  `docs/TELEMETRY.md` documents as "`KEYFRAME_REQUEST` (`0x64`) on the input channel".
- `index.html:1313` `if (!m.data.keyTier) fallbackToVideo('no keyframe and no way to ask for
  one')` — with `keyTier === 2` the demotion never fires; and `index.html:1178`
  `if (w && w.keyTier === 0) degrade(DEG.KEYFRAME_REQUEST_IMPOSSIBLE)` never sets the bit that
  would tell the host.

**Failing scenario.** Any client on the legacy `createEncodedStreams` transport (Chrome
94–140, and every client demoted by defect 3) loses a keyframe. The worker asks every 250 ms
forever, the host never hears, `nokeyframe` fires every 8 s and is ignored because
`keyTier === 2`, and the picture stays frozen. It recovers only through
`handleRenderCount` (`index.html:1044`), which needs `renderStalls >= 4` at 2 s per tick — and
because `asked` is `true`, it takes the *slower* of the two thresholds. So a frozen picture
sits for ~8 s and then demotes to `<video>`, and the host telemetry says the client had a
working keyframe path the whole time.

**Fix.** Either implement the opcode on the host (parse `0x64` on the reliable channel and
route it to the same `request_keyframe` path the PLI handler uses) or stop claiming the tier:
have `RemoteInput.requestKeyframe()` return `false`, drop the unconditional
`{outofbandkey:true}`, and let `keyTier` fall to 0 so the existing demotion and degradation
bits fire. Do not ship the tier byte as 2 until the host arm exists — `TELEMETRY.md` currently
documents a host feature that is not there.

---

### 3. The 2500 ms transform watchdog is armed before the offer, so any slow join is spuriously demoted off `RTCRtpScriptTransform`

`index.html:1344-1359`

```js
videoTransceiver.receiver.transform = new RTCRtpScriptTransform(worker, { kind: 'video' });
...
transformWatchdog = setTimeout(() => {
    if (!transformStarted) retryWithLegacyTransform('RTCRtpScriptTransform never fired');
}, 2500);
```

This runs inside `if (webcodecs) { ... }`, which is at `index.html:1286` — **before**
`await pc.createOffer()` (1454), `await pc.setLocalDescription()` (1455),
`await waitForIceGathering(...)` (1456, budget 3 s or 6 s), `await fetch('/whep')` (1460),
`await resp.text()` (1481) and `await pc.setRemoteDescription()` (1482).

`self.onrtctransform` (`transform-worker.js:551`) cannot fire until the receiver actually has
an encoded frame, which requires all of the above plus ICE connectivity plus the first RTP
packet. The watchdog therefore measures "time since we installed the transform", not "time
since the transform should have started", and 2500 ms is well inside the normal setup budget
the same function allots to ICE alone.

**Failing scenario.** Cross-network join, or any join where the host takes >2.5 s to create
the virtual display and answer. The watchdog fires mid-`await`, calls
`retryWithLegacyTransform` → `teardownSession()` + `start(true)` with
`forceLegacyTransform = true`, and the session lands on `createEncodedStreams` — permanently
losing `sendKeyFrameRequest()`, which combined with defect 2 leaves it with no keyframe
mechanism at all. `DEG.TRANSFORM_FELL_BACK_TO_LEGACY` is set, so the host sees the demotion
but not that it was spurious.

**Fix.** Arm the watchdog from the first sign that packets should be flowing — start it in
`pc.ontrack`, or on `iceConnectionState === 'connected'`, or on the first `inbound-rtp`
`bytesReceived > 0` — not at transform installation. 2500 ms from the *first arriving byte* is
the right measurement.

---

### 4. `start()` has no generation guard, and two timers can re-enter it mid-await

`index.html:1256-1517`, with `retryWithLegacyTransform` (967) and `fallbackToVideo` (979)

`start()` awaits six times and reads and writes the module-level `pc`, `worker`,
`usingFallback` and `reconfiguring` across every one of them. `retryWithLegacyTransform` (from
the 2500 ms `transformWatchdog`) and `fallbackToVideo` (from the 5 s `watchdog`) both call
`teardownSession()` — which sets `pc = null` and terminates `worker` — and then call `start()`
again, without the in-flight `start()` knowing anything happened.

The ICE handlers guard themselves with `const localPc = pc; if (pc !== localPc) return;`
(`index.html:1436-1438`), so the author was aware of the hazard; the body of `start()` was not
given the same treatment.

**Failing scenario.** Defect 3's watchdog fires while the first `start()` is parked on
`await waitForIceGathering(...)`. The second `start()` builds a fresh `pc`. The first
`start()` resumes at `index.html:1473` and reads `pc.localDescription.sdp` — now the *second*
connection's SDP — POSTs a second `/whep` for the same device (the host runs display
correlation and creation again, taking `DISPLAY_CORRELATION_LOCK`), and then calls
`pc.setRemoteDescription` on a connection that is already mid-negotiation from the other
invocation. If the teardown lands in the other order, `pc` is `null` at line 1473 and the
`TypeError` is swallowed into the generic catch, which then calls `fallbackToVideo` for a
reason that is not the real one.

**Fix.** Take a token at the top and check it after every await:

```js
let startGen = 0;
async function start(preferWebCodecs) {
    const gen = ++startGen;
    ...
    const iceServers = await getIceServers();
    if (gen !== startGen) return;
    ...
}
```

and have `teardownSession()` bump `startGen` so any in-flight `start()` retires itself.

---

### 5. `presentFrame` prefers an orphaned track-generator writer forever — black screen, no error, and a `VideoFrame` leak

`transform-worker.js:192-215`, `index.html:993-1013`

```js
function presentFrame(frame) {
  if (genWriter) { ... return true; }      // the writable owns the frame from here
  if (!ctx || !canvas) return false;
  ...
}
```

`genWriter` is set in `setupTrackGenerator()` (`transform-worker.js:167-187`) and is **never
cleared**. Both of `attachTrackGenerator`'s failure paths leave it set:

- `index.html:995-1001` (the worker reported `track: null`, the common case on anything but
  Chrome and Safari 18): the page transfers a fresh canvas to the worker and calls
  `showCanvas()`. `setupCanvas` sets `ctx` and `renderTier`, but `genWriter` still wins in
  `presentFrame`.
- `index.html:1008-1012` (`video.srcObject = new MediaStream([track])` threw): the page calls
  `showCanvas()` and does **not** transfer a canvas at all, so the worker has neither a live
  sink nor a canvas.

In both cases every decoded frame is written to a writable whose track is attached to nothing.
`handedOff` is `true`, so `onDecodedFrame` does not close the frame; the writable holds each
one. `framesRendered++` still runs, so `rendered` is posted, `onStreamLive()` clears the 5 s
watchdog, and `handleRenderCount` sees renders advancing and never demotes.

**Failing scenario.** `?present=track` on Firefox or older Safari: permanent black canvas, no
error message, no host-visible degradation, and `VideoFrame`s accumulating until the decoder
runs out of buffers. This is exactly the "no video and no error" case.

Reachable only through the opt-in query flag documented for the manual A/B, which is why this
is major and not blocker — but the null-track branch is the *expected* outcome of that flag on
most engines, so the A/B itself cannot be run correctly today.

**Fix.** Clear the generator when the page tells the worker to use a canvas:

```js
} else if (e.data.type === 'canvas') {
    if (genWriter) { try { genWriter.close(); } catch (_) { note(); } }
    generator = null; genWriter = null;
    setupCanvas(e.data.canvas);
}
```

and make `index.html:1008-1012`'s catch transfer a canvas the way the null branch does.

---

### 6. `decoder-failed` nulls the worker but leaves the DataChannel handler pointing at it

`audio.js:748-756`, `audio.js:779-783`

```js
} else if (d.type === 'decoder-failed') {
    state.decoderFailed = true;
    stopWorker();                                   // state.worker = null
    if (state.fallbackStream) attachFallbackStream(state.fallbackStream);
}
```

`state.dc.onmessage` is still the closure installed at `audio.js:779`, which does
`state.worker.postMessage(buf, [buf])` with no null check. `teardown()` clears
`state.dc.onmessage` (`audio.js:816`); this path does not.

**Failing scenario.** After four Opus decode errors (`audio-worker.js:163`), every subsequent
audio packet — 200/s at the host's 5 ms framing — throws
`Cannot read properties of null (reading 'postMessage')` out of the DataChannel message
handler. Worse, the recovery is conditional on `state.fallbackStream`, and a client that
claimed the fast path told the host `audioCapabilities.webcodecsOpus = true`, so the host put
**no audio track in the answer** (`index.html:1271-1281`) and `state.fallbackStream` is
`null`. The result is permanent silence, a per-packet exception storm, and nothing on screen.
`DEG.AUDIO_OPUS_UNSUPPORTED` reaches the host but the *user* is told nothing.

**Fix.** In the `decoder-failed` branch, detach the channel first
(`if (state.dc) { state.dc.onmessage = null; }`) and, when there is no fallback stream, surface
it — the page already has `showLinkStatus()` for exactly this kind of "still connected, one
subsystem is gone" message.

---

### 7. The new worklet concealment never runs in the case it was written for

`audio-worklet.js:194-208` vs the new code at `219-232`

The commanded-target branch short-circuits before the underrun handling:

```js
if (commanded) {
    const err = avail - effTarget;
    ...
    } else if (err < -this.slackFrames && (avail === 0 || this.peekPeak(need) < SILENCE_PEAK)) {
        outL.fill(0); if (outR) outR.fill(0);
        this.corrections++;
        this.maybePostStats();
        return true;                       // <-- returns here
    }
}
```

`commanded` is true on the whole WebCodecs path, because `audio.js:624` posts a target as soon
as `videoDelayMs` and `preEnqueueEmaMs` are both known. `effTarget` is at least
`MIN_TARGET_MS` = 10 ms = 480 frames and `slackFrames` = 240, so on a total starvation
(`avail === 0`) `err = -effTarget < -slackFrames` is always true and `avail === 0` satisfies
the second clause unconditionally. The function returns digital silence **before** reaching
`conceal()`, `this.underruns++`, `this.starvedBlocks++` and the new
`REPRIME_AFTER_STARVED` hysteresis.

So the change's headline behaviour ("conceal a lost packet instead of re-priming the jitter
buffer") only applies when the ring is partially full *and* the pending audio is above
`SILENCE_PEAK`. The most common underrun — the ring at zero — still emits a square-edged
silent quantum, which is the artefact the commit set out to remove.

The second consequence is a telemetry hole: `this.underruns` is what `adaptSafetyMargin`
(`audio.js:579-597`) differentiates. With underruns never incremented on this path,
`fresh` is always 0, `safetyMs` only ever decays, and the adaptive margin cannot grow on the
link that needs it. The host's `client_jitter_underruns` reads 0 while the user hears dropouts.

The early return itself predates this branch, but it is what makes the new code dead, and the
change is what makes it matter.

**Fix.** Restrict the silence-insertion clause to the case it is actually for — growing the
buffer at a *silence boundary* — and let a starved ring fall through:

```js
} else if (err < -this.slackFrames && avail > 0 && this.peekPeak(need) < SILENCE_PEAK) {
```

`avail === 0` then reaches the `else` branch, conceals, counts the underrun, and re-primes only
after `REPRIME_AFTER_STARVED` blocks — which is the documented intent.

---

### 8. The WHEP POST has no timeout, and on the reconfigure path a hang is invisible

`index.html:1460-1476`, `reconfigure()` at `445-451`

`reconfigure()` sets `reconfiguring = true`, tears the session down and calls `start(true)`.
`reconfiguring` is cleared in exactly two places: `onStreamLive()` (`index.html:361`) and the
`catch` in `start()` (`1511`). `fetch('/whep')` carries no `AbortController` and no timeout, so
a host that accepts the TCP connection and never answers — a wedged display-correlation lock,
a paused VM, a black-holed route — parks `start()` there forever.

While `reconfiguring` is true: `armProbe()` returns immediately (`388`), `probeTick` returns
(`395`), `evaluateLink()` returns (`290`), `onconnectionstatechange` returns (`1447`), and
`sendLeave()` returns (`866`). Nothing shows the loading spinner on this path either —
`loadingEl.classList.add('show')` only happens in the form submit handler.

**Failing scenario.** An orientation change on a phone (`relayGeomChange` → `reconfigure`)
against a host that stops answering `/whep`. The user sees the last decoded frame frozen on a
canvas, no spinner, no link status, no error, and no reconnect — indefinitely. The same hang
on the initial join at least leaves the spinner up.

**Fix.** Give the WHEP POST a deadline (`AbortSignal.timeout` where available, an
`AbortController` + `setTimeout` otherwise, ~10 s) and treat the abort as a normal `start()`
error. Independently, arm a watchdog when `reconfiguring` is set that calls
`linkLost('Disconnected, reconnect to continue.')` if the flag has not cleared within the same
budget.

---

### 9. `ensureConfigured()` can run twice concurrently and leak a `VideoDecoder`

`transform-worker.js:438-489`, called at `563` and `620`

```js
function startPump(readable) {
  const reader = readable.getReader();
  ensureConfigured().then(() => requestKey(true)).catch(note);   // not awaited
  (async function pump() { ... await ensureConfigured(); ... })();
}
```

The early-out `if (configured && decoder && decoder.state !== 'closed') return true;` is
evaluated only on entry, and `configured` is not set until after
`await VideoDecoder.isConfigSupported(...)` (up to eight candidates, each an await). The
un-awaited call at 563 and the pump's own call at 620 therefore overlap on the first frame:
both run the probe loop, both do `decoder = makeDecoder(); decoder.configure(chosen)`, and the
second assignment orphans the first decoder with no `close()`.

**Failing scenario.** Every WebCodecs session start. A configured-but-unreferenced
`VideoDecoder` is left holding a decode session — on Chromium that is a GPU-process decoder
instance — until GC, and GC is not required to run. On a device that reconfigures often (an
orientation-flipping phone) these accumulate.

**Fix.** Memoise the in-flight promise:

```js
let configuring = null;
function ensureConfigured() {
  if (configured && decoder && decoder.state !== 'closed') return Promise.resolve(true);
  if (!configuring) configuring = doConfigure().finally(() => { configuring = null; });
  return configuring;
}
```

---

### 10. `framesRendered` counts frames that were never painted

`transform-worker.js:365-367`

```js
handedOff = presentFrame(frame);
framesRendered++;
```

`presentFrame` returns `false` both when it painted to the canvas *and* when it did nothing at
all (`if (!ctx || !canvas) return false;` at line 204). The counter is incremented either way,
and `framesRendered` is the value behind `rendered` (which fires `onStreamLive` and cancels the
5 s no-frame watchdog), behind `rendercount` (which drives `handleRenderCount`'s stall
detection), and behind the `frames painted` field of the stats frame.

**Failing scenario.** Any window in which the worker is decoding without a sink — the
round trip in `attachTrackGenerator`'s null branch (`index.html:995-1001`), or defect 5's
permanent version. The page believes it is live and the host is told the client painted frames
it never saw.

**Fix.** Have `presentFrame` distinguish the three outcomes (`'handed-off' | 'painted' |
'no-sink'`) and only increment `framesRendered`, `wArrivalToPaint`, `wHostToPaint` and
`renderedOnce` for the first two.

---

### 11. The audio worker's decoder-reset budget is never replenished

`audio-worker.js:150-176`

`decoderResets` is incremented in the decoder error callback and reset only in the `init`
handler (`audio-worker.js:263`). There is no equivalent of the video worker's
`decoderResets = 0` on a successful keyframe (`transform-worker.js:625`).

**Failing scenario.** A four-hour session with one transient Opus decode error per hour — a
corrupt packet on a lossy link, a device change — hits `MAX_DECODER_RESETS` and posts
`decoder-failed`, taking audio down permanently (and, via defect 6, into the exception storm).
The errors were unrelated and each one had already been recovered from.

**Fix.** Reset the counter on a successful decode, e.g. `decoderResets = 0;` at the top of
`onDecoded` once `frames > 0`, so the budget means "four failures in a row" as it does on the
video side.

---

### 12. `renderFromTs` is an unwrapped RTP timestamp, so an RTP wrap drops every frame until the session demotes

`transform-worker.js:383` and `624`

`renderFromTs = encodedFrame.timestamp` is stored raw, and the output gate is
`if (ts >= renderFromTs)`. The timestamp is the 32-bit 90 kHz RTP value — the same one
`reportAvSync` explicitly unwraps at `280-285`. `renderFromTs` is only rewritten inside
`if (waitingForKey && type === 'key')`, so once the stream is running it is never refreshed.

**Failing scenario.** After the RTP counter wraps (2^32 / 90000 ≈ 13.25 h from the host process
epoch), every subsequent `ts` is small and `renderFromTs` is near 2^32. Every decoded frame is
dropped at output, silently — the drop path is the `if` in `onDecodedFrame`, which has no
counter. `framesRendered` stops, `handleRenderCount` sees bytes advancing with no renders,
asks for a keyframe (which does not refresh `renderFromTs`, because `waitingForKey` is false),
and after four 2 s ticks demotes the whole session to `<video>` — an unexplained latency
regression 13 hours into a session.

**Fix.** Compare against the same unwrapped timeline `reportAvSync` maintains, or simply treat
a backward jump larger than 2^31 as a wrap and reset `renderFromTs`.

---

### 13. The new ring ABI assertions throw into a void

`audio-worklet.js:26-28` and `33-36`; `audio.js:672-697`

The two new `throw`s are inside `AudioJitterProcessor`'s constructor, which runs on the audio
rendering thread. A constructor that throws there does **not** reject the `AudioWorkletNode`
constructor on the main thread — it fires `onprocessorerror` on the node and the node outputs
silence. `attachWorklet()` has no `onprocessorerror` handler, so its `try` block completes,
`state.prepared = true`, `beginFromGesture()` resolves `true`, and the page tells the host
`worklet: true`.

**Failing scenario.** Any future divergence between `CAP_FRAMES` in `audio.js` and
`capacityFrames` in the node options — precisely the drift the assertion was added to catch —
produces permanent silence that is invisible to the user, to `SEAudio.getSyncInfo()` (which
reports `contextState: 'running'`), and to the host (`DEG.AUDIO_PREPARE_FAILED` is not set,
because `state.prepared` is true).

**Fix.**

```js
state.node.onprocessorerror = () => {
    state.prepared = false;
    console.error('[audio] worklet processor failed');
};
```

and gate `state.prepared = true` on a first `stats` message from the processor rather than on
the constructor returning.

---

### 14. `showError(err.message)` with a falsy message leaves the join screen dead

`index.html:323-335` and `1514`

```js
function showError(msg) {
    errEl.textContent = msg || '';
    errEl.classList.toggle('show', !!msg);
    if (msg) { stopLock(); isLive = false; loadingEl.classList.remove('show');
               joinEl.style.display = ''; ...; connectBtn.disabled = false; validate(); }
}
```

Everything that recovers the UI — hiding the spinner, restoring the join form, re-enabling
Connect — is inside `if (msg)`. `start()`'s catch calls `showError(err.message)`. A `throw`
whose value has no `.message`, or a `DOMException` with an empty message (`AbortError` from a
cancelled `setRemoteDescription`, some `InvalidStateError`s), passes `undefined`/`''` and the
page keeps the spinner up with a disabled button and no text, forever.

`/Invalid session ID or OTP/.test(err.message)` on the line above has the same shape but is
harmless (`test(undefined)` stringifies).

**Fix.** `showError((err && err.message) || String(err) || 'Connection failed.')`, or move the
recovery out of the `if (msg)` guard and keep only the text assignment inside it.

---

### 15. The picture rect is unknown for the first second of every WebCodecs session

`index.html:1224-1229` and `1326-1330`; `input.js:66-82`

On the canvas path the only caller of `RemoteInput.setPictureSize` is the `videostats` message
handler, and `startVideoStatsPoll` is a plain `setInterval(..., 1000)` whose first tick is a
full second after the stream starts. Until then `srcW === srcH === 0` and
`recomputePictureRect` takes its degenerate branch:

```js
if (!(srcW > 0 && srcH > 0 && cssW > 0 && cssH > 0)) { picX = 0; picY = 0; picW = cssW; picH = cssH; return; }
```

so `normX`/`normY` (`input.js:144-145`) normalise against the whole stage while the picture is
letterboxed inside it by `object-fit: contain` (`styles.css:23-31`).

**Failing scenario.** A 16:9 phone in portrait against a 16:9 host display: the picture occupies
a band roughly a third of the stage height, and for the first second every tap is mapped to a
host coordinate off by up to a third of the screen. The same gap reopens after every
`reconfigure()`, because `teardownSession()` nulls `videoWorkerStats` (`index.html:954`).

The letterbox maths itself is correct once `srcW/srcH` land — `scale = min(cssW/srcW,
cssH/srcH)` with centred bars matches `object-fit: contain`, and mixing device-pixel `srcW/srcH`
with CSS-pixel `cssW/cssH` is fine because only the ratio is used, so all DPRs and both
orientations are handled.

**Fix.** Have the worker post `videostats` (or a dedicated `size` message) once, unprompted,
from `presentFrame` the first time `canvas.width`/`height` are set, and post one immediately
from `startVideoStatsPoll` rather than waiting a full interval.

---

## Lower-confidence concerns

1. **The static-asset ETag is the crate version** (`server.rs`, `asset_etag()` →
   `env!("CARGO_PKG_VERSION")`, `Cache-Control: no-cache`). The client page has no build step
   and is edited directly, so editing `transform-worker.js` without bumping the version makes
   every browser that has loaded the page revalidate, receive `304`, and run the *old* file.
   That is a trap aimed squarely at this branch's own workflow. A content hash
   (`const_fnv1a_hash` over the `include_str!`s, computed once in a `OnceLock`) would be exact
   and just as cheap.

2. **The refresh-rate deadline may be shorter than the measurement.**
   `REFRESH_DEADLINE_MS = 400` (`index.html:1635`) races `refreshRatePromise`, which is
   `getRefreshRate(60)` — 60 rAF callbacks, i.e. ~1000 ms at 60 Hz and ~500 ms at 120 Hz. The
   promise is started at page load (1726) so it has usually resolved by the time the user taps
   Connect, which is why I am not calling this a defect. But a QR-prefilled join tapped
   quickly, or a page loaded in a background tab where rAF is throttled, will send
   `refreshRate: 0`, and the refinement then only lands "at the next reconfigure that happens
   for some other reason" — which may be never. Worth measuring on a real 120 Hz phone before
   trusting the comment at 1627-1634.

3. **`getStats()` mutates degradation state** (`index.html:1177-1180`). `SEVideo.getStats()` is
   documented as the console read-side, and `input.js:323` calls it once a second, but it also
   sets `DEG.DECODER_HW_UNAVAILABLE`, `DEG.KEYFRAME_REQUEST_IMPOSSIBLE` and
   `DEG.COALESCED_EVENTS_UNAVAILABLE`. Bits are latched and never cleared, so a device that is
   briefly on a software decoder reports `DECODER_HW_UNAVAILABLE` for the rest of the session.
   A reader typing `SEVideo.getStats()` in the console also permanently changes what the host
   is told.

4. **Coalesced `movementX` under pointer lock** (`input.js:517-520`). The summed-coalesced path
   only falls back to `e.movementX` when `c.length === 1 && c[0] === e`. If an engine returns
   several coalesced entries with `movementX === 0` (they are only guaranteed populated for the
   dispatched event), the summed delta is 0 and the host cursor does not move. Worth an
   explicit check that the sum is non-zero before preferring it over `e.movementX`.

5. **The worklet has no cross-check that the writer agreed on shared mode.** If
   `audio-worker.js::setupShared` bails (`:58`, `:62`) while the worklet's own constructor
   succeeded, the worker posts `samples` messages, the worklet's `enqueue` writes into the
   SharedArrayBuffer views and advances `this.writePos`, but `available()` reads
   `Atomics.load(ctrl, W)` which nobody advances — permanent priming, permanent silence. Both
   sides check the same two constants so this is not reachable today, but the ABI comment says
   "change all three together" and nothing enforces it at runtime. A one-word handshake
   (the worker posts `{type:'mode', shared}` and the worklet asserts it matches) would close it.

6. **`state.safetyMs` and `state.cleanBlocks` survive `teardown()`** (`audio.js:814-824`), so a
   reconfigured session inherits the previous link's adaptive margin. Probably desirable
   (same device, same network) but it is not stated anywhere, and combined with defect 7 the
   margin can only ever be inherited downward.

7. **`clampRefresh` rounds to the nearest 10 Hz** (`index.html:1622`), so a 144 Hz panel asks
   the host for 140 and a 165 Hz panel for 170. Pre-existing, not part of this change, but it
   sits directly on the path this branch reworked and is worth confirming against the host's
   mode table.

---

## Verified correct

- **Stats v2 byte layout.** `sendStats()` (`input.js:339-371`) matches `docs/TELEMETRY.md`
  field for field: opcode/version at 0-1, path bytes at 2-3, `u32` LE flags at 4, the nine
  percentile `u32`s at 8..40, counters at 44..68, and the v2 tail — tier bytes 72-75,
  suppressed at 76, degradation at 80, audio target/concealed/output-latency at 84/88/92 — for
  96 bytes total. Bytes 0..72 are byte-identical to v1 and `parse_client_stats`
  (`input/protocol.rs:465`) reads exactly that prefix with a `< CLIENT_STATS_LEN` floor, so the
  "length is a floor, never an equality" contract holds on both sides. `node
  scripts/test-client-stats.mjs` passes on this branch (30 assertions including the tier bytes,
  the 72-byte floor and the clock arithmetic).
- **The `us()`/`u32()` coercions.** Non-numbers, non-finite values and negatives all map to 0
  ("not measured"), and both saturate at `0xffffffff`, so no field can wrap or write a negative
  through `setUint32`. `degradeFlags` currently tops out at bit 21, safely clear of the signed
  bitwise-OR boundary at bit 31.
- **PING/PONG clock offset.** `onPong` (`input.js:419-433`) computes
  `offsetMs = hostMs − (timeOrigin + (t_send + t_recv)/2)`, exactly the sign and form
  `TELEMETRY.md` documents, and both consumers invert it consistently:
  `transform-worker.js:275` and `hostCaptureAbsMsOf`, `audio-worker.js:129`, and
  `SEClock.hostNsToAbsMs` all use `absMs = hostMs − offsetMs`. The min-RTT selection with a
  30 s TTL (`424`) is right — the least-queued exchange gives the tightest midpoint — and the
  17-byte length check plus `onReliableMessage`'s `>= 9` guard means a legacy 9-byte PONG is
  read for liveness and ignored for the clock, which the test asserts.
- **The u64 halves.** `setU64`/`getU64` (`input.js:20-28`) are byte-identical to
  `setBigUint64`/`getBigUint64` little-endian and stay exact below 2^53, which every stamp is.
  `readU64` in `audio-worker.js:20` matches, and `build_pong` (`input/protocol.rs:503`) writes
  both halves LE.
- **RTP unwrapping in the video worker.** `reportAvSync` (`transform-worker.js:278-285`) and
  the non-mutating `hostCaptureAbsMsOf` (`267-276`) use the same `d < -0x80000000` /
  `d > 0x80000000` test on `uint32` differences, which is correct in both directions, and the
  ordering (read the estimate, then advance the state) is what the comment claims.
- **`VideoFrame` ownership.** `onDecodedFrame` (`transform-worker.js:357-397`) reads
  `frame.timestamp` *before* handing the frame to the writable, and the `finally` closes the
  frame on every path that did not hand it off — the early-drop path (`ts < renderFromTs`), the
  paint-error path, and the normal canvas path. The one residual leak is a `genWriter.write()`
  that rejects (`196`), where the writable may not close the chunk; that only matters under
  defect 5.
- **The decoder in-flight accounting is balanced on every path I can reach from a throw**:
  `inFlight++` immediately before `decode()`, `inFlight--` at the top of the output callback
  before anything that can throw, `inFlight--` in the submit catch, and `inFlight = 0` in both
  `ensureConfigured` and `resetAndRequestKey`. `ondequeue` only clears `backlogStreak` and never
  gates submission, so it cannot deadlock on its own.

  **Correction.** My first pass concluded there was no wedge because the
  `inFlight > MAX_IN_FLIGHT` gate exempts keyframes, so an IDR would drain the count. That is
  wrong, and `findings/major-client-inflight-counter-leaks-and-wedges-the-decoder.md` (filed
  independently) has it right. A keyframe both increments and decrements: `inFlight` goes
  N → N+1 → N, so a count leaked by a decode the browser accepted and silently discarded (no
  output, no error — a delta whose reference was lost) is never repaid. Every subsequent delta
  is dropped, the backlog handler sets `waitingForKey` and asks for another IDR, and the session
  settles into a keyframe-only slideshow. Because keyframes *do* render, `framesRendered`
  advances and `handleRenderCount` never demotes. See that finding, not this line.
- **Bounded `pending` map.** `notePending` evicts the oldest entry at `PENDING_MAX`, so a
  decoder that stops emitting cannot turn the arrival-time map into a leak.
- **Decoder rebuild is bounded.** `MAX_DECODER_RESETS = 3` with `decoderResets = 0` on a
  successful keyframe (`transform-worker.js:625`) gives "three failures in a row", and the
  `decoderdead` message reaches a page handler that demotes to `<video>` — a real terminal
  state rather than a spin.
- **`ensureConfigured` probes the configuration it installs.** `decoderConfig()` builds one
  object used for both `isConfigSupported` and `configure`, closing the old
  probe-one-thing/configure-another gap; `withAcceleration(..., 'prefer-hardware')` is used only
  for the telemetry answer and never for the actual configure, which is the right call given
  Chromium's GPU-only factory.
- **iOS gesture ordering.** `beginFromGesture()` (`audio.js:653-670`) constructs the
  `AudioContext` and calls `resume()` synchronously, and the join handler calls it before its
  first `await` (`index.html:1926-1927`) with a comment saying why. `attachWorklet()`'s
  `addModule` await happens after the resume has been issued. This is correct, and it is the
  bug the commit set out to fix.
- **The fast-path claim is verified before it is sent.** `verifyCapabilities()` runs
  `AudioDecoder.isConfigSupported({codec:'opus'})` rather than sniffing constructors, the
  worklet-prepare result is folded in with a 2 s deadline, and `audioFast` is recomputed before
  `beginStreaming()` — so a browser that has the constructors but cannot decode Opus now tells
  the host `false` and gets an audio track in the answer instead of silence.
- **Ring producer/consumer arithmetic.** Positions are absolute `uint32` frame counters on both
  sides: the writer stores `(w + n) >>> 0` into an `Int32Array` and the reader recovers it with
  `Atomics.load(...) >>> 0`; the reader stores `readPos | 0` and the writer recovers it the same
  way. `free = capFrames - ((w - rd) >>> 0)` and `available() = (w - readPos) >>> 0` are both
  correct across the 2^32 wrap, and `pos & (cap-1)` is valid because `CAP_FRAMES = 65536` — now
  asserted on all three sides. The planar split at `CTRL_BYTES` and `CTRL_BYTES + cap*4` agrees
  byte for byte between `audio.js:22-23`, `audio-worker.js:66-68` and `audio-worklet.js:38-40`,
  and the allocation `CTRL_BYTES + CAP_FRAMES*4*2` matches the `need` both readers check.
- **Reset semantics across a reconfigure.** `teardown()` posts `{type:'reset'}`, the worklet
  sets `readPos = W` and publishes it, and the fresh worker's `setupShared` deliberately does
  not zero `W` — so the two stay consistent and no stale samples are replayed.
- **Sequence-gap detection.** `handlePacket` (`audio-worker.js:226-231`) rejects duplicates,
  rejects late packets with the `>= 0x80000000` half-space test, and computes
  `gap = ((seq - lastSeq) >>> 0) - 1` — all correct across the `u32` wrap. Concealment is capped
  at `MAX_CONCEAL_PACKETS` and suppressed when the host has flagged a real discontinuity, which
  is the right split of responsibility.
- **No scratch-buffer aliasing in the audio worker.** `concealPackets` writes into
  `scratchL/scratchR` while reading `lastDecodedL/lastDecodedR`, which `rememberDecoded` keeps
  as separate arrays; and it is called synchronously from `handlePacket`, never interleaved with
  the decoder's `onDecoded` callback.
- **Reconfig / kick epoch handling.** `applyReconfig` (`index.html:417-433`) seeds both epochs
  on the first successful poll (both start `null`, so neither fires a spurious kick or
  reconfigure), and `reconfigure()`'s `if (reconfiguring) return;` guard prevents overlapping
  teardowns from the geometry watcher and the poll. `probeTick`'s refused/unreachable
  classification and the `armProbe` fast/slow cadence are self-consistent, and every timer it
  owns is cleared in `teardownSession()`.
- **The letterbox transform** itself, once sized: correct for both orientations and all DPRs
  (see defect 15 for the timing gap), and consistent between the single-pointer path
  (`input.js:144-145`) and the coalesced batch path (`174-175`). `#stage` is
  `position: fixed; top/right/bottom/left: 0` (`styles.css:14-21`) so `clientX` and the stage's
  content box share an origin, and `#video, #canvas` are `pointer-events: none` so events always
  target the stage.
- **`enterFullscreen` returns a Promise on every branch** (`index.html:599-616`), including the
  Safari-below-16.4 `webkitRequestFullscreen`-returns-undefined case, so the `.catch()` at each
  call site can no longer throw a `TypeError` that silently skips pointer lock, keyboard lock
  and the wake lock. The `.then(..., ...)` also latches and clears `DEG.FULLSCREEN_DENIED`
  correctly in both directions.
- **The `styles.css` `inset` → longhand rewrite** is a faithful one-for-one substitution (5
  sites, all `inset: 0`), and the added `-webkit-backdrop-filter` is correctly paired.
- **`permModalIfVisible`** (`index.html:1910-1917`) does the right thing: a modal raised behind a
  fullscreen stage would be invisible and its promise would never settle, so it calls `onSkip`
  and reports `AUDIO_OUTPUT_LABELS_UNAVAILABLE` instead of deadlocking `enumerateLazily`'s
  `await new Promise(...)`.
- **`sendStats` is exception-isolated.** `SEVideo.getStats()` is wrapped in its own try/catch
  that returns rather than throws, and the interval body wraps `sendPing()` and `sendStats()`
  separately (`input.js:719-722`) — a throw in either can no longer kill the once-a-second timer,
  which is the failure mode `docs/TELEMETRY.md` calls out as otherwise invisible.

---

## Findings filed

`findings/` is shared with the other reviewers on this branch, so two of my defects were
already on disk when I went to write them and I did not duplicate the files.

| # | Defect | File |
|---|---|---|
| 1 | ICE gathering never waits for a relay | `major-client-ice-gathering-needrelay-never-passed.md` (pre-existing; I would raise it to **blocker** — it breaks every TURN join, and it is a regression from the base) |
| 2 | `0x64` has no host handler | `major-client-outofband-keyframe-disables-video-demotion.md` and `blocker-client-out-of-band-keyframe-tier-is-declared-live-unconditionally.md` (pre-existing) |
| 3 | Transform watchdog armed before negotiation | `major-client-transform-watchdog-armed-before-negotiation.md` (new) |
| 4 | `start()` re-entrancy | `major-client-start-reentrancy-no-generation-guard.md` (new) |
| 5 | Orphaned track-generator writer | `major-client-trackgen-writer-orphans-the-canvas.md` (new) |
| 6 | `decoder-failed` leaves the DataChannel handler | `major-client-audio-decoder-failed-leaves-datachannel-handler.md` (new) |
| 7 | Worklet concealment unreachable | `major-client-worklet-conceal-unreachable-on-commanded-path.md` (new) |
| 8 | WHEP POST has no timeout | `major-client-whep-post-has-no-timeout.md` (new) |
| 9 | `ensureConfigured` race | `minor-client-ensureconfigured-races-and-leaks-a-decoder.md` (new) |
| 10 | `framesRendered` counts unpainted frames | `major-client-framesrendered-counts-unpainted-frames.md` (pre-existing, filed as major — agreed) |
| 11 | Audio reset budget never replenished | `minor-client-audio-decoder-reset-budget-never-replenished.md` (new) |
| 12 | `renderFromTs` not unwrapped | `minor-client-renderfromts-not-unwrapped-across-rtp-wrap.md` (new) |
| 13 | Worklet ABI assertions throw into a void | `minor-client-worklet-abi-assert-throws-into-a-void.md` (new) |
| 14 | `showError` with a falsy message | `minor-client-showerror-with-a-falsy-message-strands-the-join-screen.md` (new) |
| 15 | Picture rect unknown for the first second | `minor-client-picture-rect-unknown-for-the-first-second.md` (new) |
| C1 | Static-asset ETag is the crate version | `major-build-static-asset-etag-is-the-crate-version.md` (new) |
| C3 | `getStats()` mutates degradation state | `speculative-client-getstats-mutates-degradation-state.md` (new) |
| C5 | Ring shared-mode handshake | `speculative-client-audio-ring-shared-mode-not-handshaken.md` (new) |

Concerns C2 (refresh-rate deadline), C4 (coalesced `movementX`), C6 (`safetyMs` survives
teardown) and C7 (`clampRefresh` 10 Hz rounding) are already covered by
`minor-client-refresh-rate-refinement-never-reaches-the-host.md` and
`major-client-pointerlock-coalesced-movement-can-be-zero.md`, or are too weak to file.

# Client web page — review 4: regressions against current behavior and the host contract

Branch `worktree-agent-a5109a99927714a08` vs `latency-overhaul`.
Scope: `src-tauri/src/streamer/static/` (7 files, +1594/−325), read cold against both sides in
full, plus the host side on `latency-overhaul` (`server.rs`, `input/protocol.rs`,
`webrtc_session.rs`, `audio/protocol.rs`).

Line references are `new-file:line` unless prefixed `old`.

**Important context for the whole review:** `src-tauri/src/streamer/input/protocol.rs` is
**unchanged on this branch**. The only Rust file the branch touches is `server.rs`, and only to
add ETag/`no-cache` revalidation to the eight static assets. So "the new host" and "the old
host" are the *same parser* for every wire message below.

---

## 1. Regressions

### R1 — `needRelay` is never passed, so a TURN session ships host-only candidates (major)

`waitForIceGathering(pc, timeoutMs, needRelay)` — new:897 — replaces the old
gathering-complete wait (old:704) with an early exit: a grace timer armed by candidate type,
`ICE_HOST_GRACE_MS = 200` for `host`/`srflx`, `ICE_RELAY_GRACE_MS = 400` for `relay`, chosen by
`needRelay`.

The only call site does not pass it:

```js
// new:1456
await waitForIceGathering(pc, hasTurnServer(iceServers) ? 6000 : 3000);
```

`needRelay` is `undefined` → falsy → `onCandidate` takes the `else if (type === 'host' || type
=== 'srflx')` branch and arms the **200 ms** timer. A host candidate lands within a few
milliseconds of `setLocalDescription`, so gathering is cut ~200 ms in — typically before the
STUN round trip finishes and reliably before a TURN `Allocate` completes.

**Scenario.** Cross-network / cloud-relay join with a TURN server configured. WHEP is a single
POST (`/whep`, new:1461) with no PATCH to trickle candidates later, so whatever is in
`pc.localDescription.sdp` at that instant is the whole offer. The offer ships host candidates
only; the host has no reflexive or relay address to pair with and the session never connects.
Before this change the same join waited for `icegatheringstatechange === 'complete'` (or 6 s),
which is exactly when the relay candidate had arrived.

The 6000 ms deadline is still computed and still passed — it is simply unreachable now,
because the 200 ms grace always fires first.

**Fix.** `await waitForIceGathering(pc, needRelay ? 6000 : 3000, needRelay)` with
`const needRelay = hasTurnServer(iceServers);` hoisted above.

`findings/major-client-ice-gathering-needrelay-never-passed.md`

---

### R2 — the client stops demoting on a lost keyframe, trusting an opcode the host drops (major)

The worker now asks for an IDR out of band when the receiver has no
`transformer.sendKeyFrameRequest` (`transform-worker.js:541-544`), the page relays it
(`index.html:1305-1309`), and `input.js:386-395` sends **opcode `0x64`** on the reliable
channel. `protocol.rs::parse` has no `0x64` arm and falls through to `_ => return None`, so the
frame is parsed as unknown and discarded — on *both* hosts. That host gap is already tracked as
`findings/major-host-keyframe-request-opcode-unhandled.md`; what is new here is that the client
now **changes its own recovery behavior** on the strength of a request that does nothing.

Two places:

```js
// new:1041-1046  (old:771 was: if (++renderStalls >= 2) fallbackToVideo(...))
try { asked = !!(window.RemoteInput && RemoteInput.requestKeyframe && RemoteInput.requestKeyframe()); } ...
const keyTier = (videoWorkerStats && videoWorkerStats.keyTier) || 0;
const canAsk = asked || keyTier === 1;
if (renderStalls >= (canAsk ? 4 : 2)) { fallbackToVideo(...); }
```

```js
// new:1309-1313
} else if (m.data.type === 'nokeyframe') {
    if (!m.data.keyTier) fallbackToVideo('no keyframe and no way to ask for one');
}
```

`requestKeyframe()` returns `true` whenever the reliable channel is open — it has no way to
know the host ignored the frame — so `asked` is `true` and `renderStalls` must reach **4**
instead of 2. `liveWatch` ticks every 2 s (new:1417-1420), so a frozen picture on the legacy
`createEncodedStreams` path (every Chrome/Edge 94–140) now takes ~8 s to demote instead of ~4 s.
And the worker's own give-up signal is disarmed entirely: it fires with `keyTier === 2`
(`KEYREQ.outOfBand`, set at `transform-worker.js:242-243` from the unconditional
`{type:'outofbandkey', available:true}` at `index.html:1360`), so `!m.data.keyTier` is false and
nothing happens.

Telemetry compounds it: `SEVideo.getStats` only sets `DEG.KEYFRAME_REQUEST_IMPOSSIBLE` when
`w.keyTier === 0` (new:1178), so the host is told the device *can* ask for keyframes when it
demonstrably cannot.

**Fix.** Gate `outOfBandKey` on a host capability the page can actually observe (a version or
feature field on the WHEP answer / `/net-config`), or — until `0x64` is parsed — do not post
`{type:'outofbandkey', available:true}` and leave `keyTier` at 0 so both recovery paths keep
their old behavior.

`findings/major-client-outofband-keyframe-disables-video-demotion.md`

---

### R3 — the new `inFlight` back-pressure counter has no resync path (major)

`transform-worker.js` replaces the browser-owned `decoder.decodeQueueSize > max(3, fps*0.05)`
check (old:352-353) with a client-maintained counter:

```js
inFlight++;                                   // new:630, before every decode()
decoder.decode(new EncodedVideoChunk({...}));
...
if (inFlight > 0) inFlight--;                 // new:359, only in onDecodedFrame
...
if (inFlight > MAX_IN_FLIGHT) { /* drop the frame, ask for a key */ }   // new:603, MAX_IN_FLIGHT = 2
```

The decrement happens **only** when the decoder emits an output. A `VideoDecoder` that accepts a
chunk and then silently discards it — a delta whose reference frame was lost, a frame dropped
after a corrupt NAL — never decrements. The error path resets to 0 (`resetAndRequestKey`,
new:504) and so does `ensureConfigured` (new:487), but a *silent* drop hits neither.

**Scenario.** Three silently-dropped frames over a session put `inFlight` permanently above 2.
From then on every delta frame takes the `new:603` branch, `backlogStreak` reaches 2,
`setWaitingForKey(true)` fires, and the stream becomes keyframe-only: each keyframe decodes and
decrements once, the first delta after it re-trips the check. Frame rate collapses to the
keyframe rate with a continuous keyframe-request storm, and no watchdog catches it because
`framesRendered` is still advancing.

The `ondequeue` handler already has the ingredients and does not use them:

```js
// new:413-415
d.ondequeue = function () {
  if (d.decodeQueueSize === 0 && inFlight <= MAX_IN_FLIGHT) backlogStreak = 0;
};
```

**Fix.** In that handler, resync rather than only clear the streak:
`if (d.decodeQueueSize === 0 && inFlight > MAX_IN_FLIGHT) inFlight = 0;` — an empty accept queue
with the counter above the cap can only mean leaked decodes. On engines without `ondequeue`
(`hasDequeueEvent` false), age the counter out of the pump instead.

`findings/major-client-inflight-counter-leaks-and-wedges-the-decoder.md`

---

### R4 — pointer-lock motion is summed from coalesced deltas that some engines leave at zero (major)

```js
// new:513-521 (old:410 read e.movementX directly)
let mx = 0, my = 0;
const c = coalescedOf(e);
for (let i = 0; i < c.length; i++) { mx += c[i].movementX || 0; my += c[i].movementY || 0; }
if (c.length === 1 && c[0] === e) { mx = e.movementX || 0; my = e.movementY || 0; }
```

`movementX`/`movementY` are not populated on the events returned by `getCoalescedEvents()` on
every engine — Firefox reports 0 on coalesced pointer events, and Chromium had the same defect
(crbug 802067) into its 70s. The guard on the next line only rescues the case where exactly one
sample came back *and it is the parent event*; the moment the browser coalesces two raw updates
into one dispatch, `mx`/`my` sum to 0 and `sendMouseDelta(0, 0, buttons)` goes to the host.

**Scenario.** Fullscreen + pointer lock on a browser whose coalesced events carry no movement:
the remote cursor stops moving entirely, or moves only on the frames where a single sample
happened to arrive — a stutter that reads as a dead pointer. This is the primary remote-control
path.

**Fix.** Fall back on an all-zero sum:
`if (mx === 0 && my === 0) { mx = e.movementX || 0; my = e.movementY || 0; }` — replacing the
`c.length === 1 && c[0] === e` guard, which that condition subsumes.

`findings/major-client-pointerlock-coalesced-movement-can-be-zero.md`

---

### R5 — a frame counts as rendered even when nothing painted it (major)

```js
// new:363-370   (old:216-218 had framesRendered++ inside `if (ctx && frame.timestamp >= renderFromTs)`)
if (ts >= renderFromTs) {
  handedOff = presentFrame(frame);
  framesRendered++;
  ...
  if (!renderedOnce) { renderedOnce = true; self.postMessage({ type: 'rendered' }); }
}
```

`presentFrame` (new:186-206) returns early with `false` when there is neither a `genWriter` nor a
`ctx`/`canvas`. Nothing is drawn, but the frame is counted and `{type:'rendered'}` still fires,
which drives `onStreamLive()` (new:1289) — the join screen is dismissed onto a black stage. Worse,
`handleRenderCount`'s stall detector compares `framesRendered` (new:1035); it keeps advancing, so
`fallbackToVideo` is never reached and the session cannot recover.

Reachable today on `?present=track` when `trackGeneratorCtor()` returns null: the worker posts
`{type:'trackgen', track:null}`, and `attachTrackGenerator` (new:993-1002) posts a replacement
canvas back **asynchronously**, so every frame decoded in that window paints nowhere and counts.

**Fix.** `if (presentFrame(frame) || paintedToCanvas) framesRendered++;` — have `presentFrame`
report whether it actually presented (distinct from whether it took ownership), and gate both
the counter and the `rendered` message on it.

`findings/major-client-framesrendered-counts-unpainted-frames.md`

> A concurrent reviewer filed the same defect as
> `findings/minor-client-framesrendered-counts-unpainted-frames.md`. The two should be merged;
> I rate it major rather than minor because the black stage is unrecoverable — the watchdog that
> would demote the session watches the very counter that is wrong.

---

### R6 — the measured refresh rate no longer reaches the host (minor)

Old: `onStreamLive()` → `refineRefreshRate()` (old:305, old:1266) measured the rate, and
`relayRefreshRate` forced a `reconfigure()` when it differed by ≥5 Hz, so a 120 Hz client always
ended up with a 120 Hz host display.

New: `resolveRefreshRate()` (new:1637-1645) races the measurement against
`REFRESH_DEADLINE_MS = 400`, and the post-live refinement only writes `joinData` —

```js
// new:372
refreshRatePromise.then((r) => noteRefinedRefreshRate(clampRefresh(r)), noteSuppressed);
// new:1647-1653 — "will apply at the next reconfigure"
```

`getRefreshRate()` (new:1595) collects **60 rAF samples**, which is ~1 s at 60 Hz and ~0.5 s at
120 Hz — both past the 400 ms deadline. `screen.refreshRate` is non-standard and absent on
essentially every client browser, so `resolveRefreshRate()` returns 0 for the overwhelming
majority of joins, and the correct rate is then applied only if some *unrelated* reconfigure
(orientation change, host setting) happens later.

Removing a renegotiation ~1 s after connect is a real win and the trade is stated in the comment
— but the cost is that high-refresh clients now run the host display at the default rate for the
whole session, which is a fidelity regression, not a neutral change.

**Fix.** Either lower the sample count so the measurement fits inside 400 ms
(`getRefreshRate(24)` is ~0.4 s at 60 Hz), or keep the forced reconfigure but arm it once, late
(e.g. 3 s after live), so it cannot collide with the connect path.

`findings/minor-client-refresh-rate-refinement-never-reaches-the-host.md`

---

### R7 — one failed worklet load disables audio permanently (minor)

```js
// new:701-705
function prepare() {
    if (state.prepared) return Promise.resolve(true);
    if (preparePromise) return preparePromise;
    return beginFromGesture();
}
```

`preparePromise` is assigned once (new:668) and never cleared on failure — `attachWorklet`'s
`catch` returns `false` and leaves the resolved-false promise in place. The old `prepare()` was
not memoized: `if (state.prepared) return true;` and then a fresh attempt, so a transient
failure (a `/audio-worklet.js` fetch that lost a race with the reconnect, a momentarily
unavailable `AudioContext`) was retried on the next call.

**Scenario.** The first `audioWorklet.addModule('/audio-worklet.js')` fails. `attachDataChannel`
awaits `prepare()` (audio.js:769) on this join *and on every reconfigure for the rest of the
page's life* and gets `false` each time, so the fast audio path stays dead — and because the
client already told the host `worklet: true` in `audioCapabilities`, the host put no audio track
in the SDP, so there is nothing to fall back to. Total silence.

**Fix.** In `attachWorklet`'s catch, `preparePromise = null` before returning false.

`findings/minor-client-prepare-memoizes-audio-failure-forever.md`

---

### R8 — the microphone modal can be raised into a fullscreen subtree and stick there (minor)

The mic prompt moved from before the join (old:1546 — `showPermModal(startAudioAndStream, ...)`
gated `beginStreaming`) to after it (new:1993-1996, `SEAudio.enumerateLazily(permModalIfVisible)`
issued right after `beginStreaming()`). Moving it off the join path is right; the guard is not
quite sufficient:

```js
// new:1910-1918
function permModalIfVisible(onContinue, onSkip) {
    if (currentFullscreenEl()) { degrade(...); onSkip(); return; }
    showPermModal(onContinue, onSkip);
}
```

`currentFullscreenEl()` is sampled once, at call time. `startLock()` enters fullscreen
asynchronously off `onStreamLive()`, so a modal raised a few hundred milliseconds earlier is
still `display: flex` when the fullscreen transition happens. A fullscreen element renders only
its own subtree, so `#permModal` becomes invisible, its promise never settles (`enumerateLazily`
awaits it at audio.js:246-248), and the element **reappears over the live picture** the moment
the user leaves fullscreen — a modal with no context, blocking the stage.

**Fix.** Re-check on `fullscreenchange` while the modal is up and resolve it as skipped, or
simply defer `enumerateLazily` until after `startLock()` has settled.

`findings/minor-client-perm-modal-can-be-orphaned-by-fullscreen.md`

---

### R9 — drag/drop coordinates were not migrated to the picture rect (minor)

`normX`/`normY` now normalise against the letterboxed picture (input.js:143-144), which is the
correct fix for `object-fit: contain`. `ndx`/`ndy`, used by every `dragenter`/`dragover`/
`dragleave`/`drop` handler (input.js:657-664), were left on the old divisor:

```js
// new:596-597 — identical to old:501-502
function ndx(e) { return clamp01(e.clientX / (cssW || window.innerWidth)); }
function ndy(e) { return clamp01(e.clientY / (cssH || window.innerHeight)); }
```

So in exactly the case the pointer fix exists for — a stage whose aspect ratio differs from the
host display's, i.e. non-fullscreen viewport mode — a file dropped under the cursor lands
somewhere else on the host desktop, offset by half the bar.

**Fix.** Route `ndx`/`ndy` through the same `picX/picY/picW/picH` arithmetic as `normX`/`normY`.

`findings/minor-client-drag-drop-coords-not-letterbox-corrected.md`

---

### R10 — `requestKeyframe()` arms its rate limiter before it can send (minor)

```js
// new:386-395
const now = performance.now();
if (now - lastKeyReqAt < 200) return false;
lastKeyReqAt = now;                                   // armed here…
if (!reliable || reliable.readyState !== 'open') return false;   // …but we never sent
```

A request attempted while the reliable channel is closed or reconnecting burns the 200 ms
window, so the next real request — which may be the one that unfreezes the picture — is
suppressed. Move `lastKeyReqAt = now` below the readyState check.

`findings/minor-client-keyframe-request-ratelimit-armed-before-send.md`

---

### R11 — `?present=track` is immediately undone by `ontrack` (minor)

`attachTrackGenerator` sets `showVideo()` (new:1010), but `pc.ontrack` unconditionally calls
`showCanvas()` on the WebCodecs path (new:1389) — after the guarded `if (PRESENT_PREF !== 'track')
showCanvas();` at new:1378 correctly skipped it. The track-generator tier therefore always ends
up displaying an empty canvas. Debug-flag only (`?present=track`), so it costs only the manual
A/B in `docs/manual-tests/client-page.md`.

`findings/minor-client-present-track-undone-by-ontrack-showcanvas.md`

---

## 2. Host-contract table

Every message the page sends, against the *unchanged* `protocol.rs` / `server.rs` parsers.

| Message | Shape sent by the new client | Old host | New host | Note |
|---|---|---|---|---|
| `POST /whep` JSON | `sessionId, otp, deviceToken, deviceName, os, refreshRate, width, height, dpr, sdp, audioCapabilities{webcodecsOpus,worklet}` | OK | OK | Field set byte-identical to old (new:1462-1475 vs old:1104-1117). `refreshRate` now usually `0` — see R6. `audioCapabilities` now the *verified* caps, same shape. |
| `X-Device-Token` (response) | read, stored under `se.deviceToken` | OK | OK | Unchanged (new:1481-1482). |
| SDP offer | `recvonly` video transceiver; audio transceiver only when `!audioFast`; 3 DataChannels | OK | OK | **But** the candidate set is now truncated on TURN sessions — R1. |
| `POST /leave` | `navigator.sendBeacon('/leave')`, no body | OK | OK | Unchanged (new:875). |
| `GET /reconfig` | polled 1000 ms / 300 ms when suspect; reads `epoch`, `kick`, `audioSink` | OK | OK | Cadence, epoch handling and kick handling byte-identical (new:378-441 vs old:311-374). |
| `GET /ice-config` | reads `iceServers` | OK | OK | Unchanged. |
| `GET /net-config` | reads `httpsPort` | OK | OK | Unchanged. |
| `POST /audio-outputs` | `{sessionId, deviceToken, supported, outputs[], selected}` | OK | OK | Shape unchanged. Now posted once *early* (audio.js:238) before `setSpeakerIdentity`, with empty identity — harmless, the host keys on the peer IP (`server.rs:527-548`) and a correct post follows. |
| `__SAME_DEVICE_FLAG__` | `window.__SAME_DEVICE__` at index.html:41 | OK | OK | Substitution token intact; `sameDeviceGate` unchanged (new:1885-1894). |
| Input opcodes `0x01`–`0x54` | unchanged layouts; `BigInt` writes replaced by two LE u32 halves | OK | OK | `setU64`/`getU64` (input.js:20-28) are byte-identical to `setBigUint64`/`getBigUint64`. Verified against `parse_drop`/`parse_pointer`. |
| `PING 0x60` | 9 bytes, `t_ns` u64 LE via `setU64` | OK | OK | Same bytes as before. |
| `PONG 0x61` (host→client) | read via `getU64`, 17-byte check retained | OK | OK | Same. |
| `STATS 0x62` | **96 bytes, version = 2** | **OK** | **OK** | `parse_client_stats` checks `b.len() < CLIENT_STATS_LEN (72)` — a floor, not equality — and reads fixed offsets 0..72. Bytes 0..72 are byte-identical to v1 (verified field-by-field against `scripts/test-client-stats.mjs`). The v2 tail is read by neither host; that is the pre-existing `findings/major-telemetry-client-stats-v2-tail-is-discarded.md`. |
| `BYE 0x63` (host→client) | `SEHealth.bye(code)`, `BYE_HOST_EXIT = 0x02` | OK | OK | Unchanged (new:262-268, input.js:442-444). Both reasons from `ab37665` still handled. |
| **`KEYFRAME_REQUEST 0x64`** | 2 bytes on `reliable` | **dropped** | **dropped** | No `0x64` arm in `protocol.rs::parse`; falls to `_ => return None`. Wire-safe (no misparse, no desync) but **inert** — and the client now changes its recovery behavior as if it worked. R2. |
| Audio DataChannel (host→client) | 13-byte header `seq u32 / capture_ns u64 / flags u8`, read as two u32 halves | OK | OK | `FLAG_DISCONTINUITY = 1<<1` is now honoured (audio-worker.js:5, 194-198) and the host does set it (`audio/mod.rs:36`, `audio/tests.rs:18`). Channel is unordered/no-retransmit on both ends, which is what the new gap concealment assumes. Correct. |

**Nothing the new client sends would be rejected or misparsed by an old host.** The only
contract defect is one-directional: `0x64` is sent and silently discarded.

**Host messages the new client no longer handles:** none. Kick epochs, reconfig epochs,
`audioSink` push, and both `BYE` reasons from `ab37665` are all still handled, with the same
code.

---

## 3. Lower-confidence concerns

1. **The HTTPS modal copy is now more wrong on iPhone, not less.** The compat gate's
   `httpsCouldHelp = !window.isSecureContext` (new:1758) is the right *available* signal — on
   HTTP you genuinely cannot distinguish "absent because insecure" from "absent because
   unimplemented". But on HTTP the behavior is unchanged (`httpsCouldHelp` is true, the audio
   row is still suppressed, `offerHttpsOnLoad` still fires), while the modal's new copy asserts
   *"Your browser only unlocks the low-latency video and audio paths on a secure one"* — a
   claim that is false for WebCodecs audio on iOS at any protocol, and stronger than the copy it
   replaced. The fix lands only on HTTPS/localhost, where the accurate description now shows.
   Verified: the upsell is **not** suppressed anywhere it was previously offered.
2. **`hasFinePointer()` (input.js:681-686) widens `imeSink.focus()` to touch devices.**
   `(any-pointer: fine)` matches an iPad with a Pencil or trackpad and an Android tablet with a
   stylus or mouse. Focusing a hidden input does not raise the soft keyboard without a gesture
   on iOS, but on Android it can — and a soft keyboard resizes the visual viewport, which trips
   `onGeomChange` → `relayGeomChange` → `reconfigure()`, i.e. a full host display rebuild. Worth
   a manual check on an Android tablet with a stylus.
3. **`nosleep.js` muting looks safe but is unverified on the tier that needs it.** The bundled
   v0.12.0 only creates `noSleepVideo` when `"wakeLock" in navigator` is false, so the
   `noSleepImpl.noSleepVideo.muted = true` at new:578 applies exactly to iOS < 16.4 — where a
   muted inline video still plays and still holds the idle timer, and muting *relaxes* the
   autoplay rule that would otherwise refuse `play()` outside a gesture (and `enableNoSleep()` is
   called from `onStreamLive`/`visibilitychange`/`pageshow`, none of which are gestures). So this
   reads as a strict improvement. The lazy construction is unambiguously a fix: the old
   top-level `const noSleep = new NoSleep()` (old:494) threw a `ReferenceError` and killed the
   entire inline script if `/nosleep.js` failed to load. Needs a real iOS 15/16 device to
   confirm the screen still stays awake.
4. **New per-event allocation on the pointer-lock path.** `coalescedOf(e)` (input.js:517) now
   runs for a locked mouse, allocating a `getCoalescedEvents()` array per `pointerrawupdate` —
   up to 1000/s on a high-rate mouse, on a path that previously allocated nothing (old:410 read
   `e.movementX` directly). Also `st.samples.splice(...)` (input.js:506) allocates a discard
   array on every batch overflow. Both are small, but they are the hottest input path.
5. **Asset ETag is the crate version** (`server.rs::asset_etag`, `env!("CARGO_PKG_VERSION")`).
   With `no-cache` the client revalidates every load, but two builds sharing a version string
   are indistinguishable, so any change to `index.html`/`input.js`/`audio.js` that does not bump
   the version serves a **304 against stale client code**. Fine for releases; a footgun during
   development and for any hot-fix that reuses a version. A content hash would cost nothing.
6. **`pushStroke` latches a pointer into batch mode for the rest of its stroke** (input.js:489-508).
   Once a coalesced burst creates `st`, every later move for that `pointerId` goes as a
   `POINTER_MOVE_BATCH` sample even when only one event arrived — and the 24-byte batch sample
   has no room for contact width/height or per-sample buttons, which the full `POINTER_MOVE`
   carries. Touch and unlocked mouse both reach this now (they took the full-pointer path
   before, old:419-431). Contact geometry is dropped for the remainder of a drag.
7. **`verifyCapabilities()` runs after the compat modal has already rendered** (new:1969 vs the
   `compatGate()` IIFE at new:1873). The modal is drawn from the unverified constructor sniff, so
   a browser that has `AudioDecoder` but refuses `opus` shows a modal claiming audio is fine and
   is never corrected. Cosmetic; the *wire* answer to the host is correctly the verified one.

---

## 4. Verified unchanged

**Appearance.** The entire `<body>` markup was diffed literally: **one hunk, one paragraph** —
the HTTPS modal's `<p>`. Everything else is byte-identical:

- element structure, every `id` and `class`, and the DOM order of all four modals;
- z-order: `#modal` 9999, `#compatModal` 9998, `#sameDeviceModal` 9997, `#permModal` 9996
  (inline styles, all unchanged);
- the join form — session/name inputs with their `maxlength` and placeholders, the six OTP
  boxes with their `inputmode`/`autocomplete` attributes, the `.err` region, the `.foot` link;
- the fullscreen toggle: `#fullscreenMode` checkbox, its `.fs-toggle-title`/`.fs-toggle-desc`
  copy, `se.fullscreenMode` persistence, and the `?fs=0|1` URL override — all identical;
- the stage: `#canvas`, `#video` and its full attribute list, `#linkStatus`, `#unmute`,
  `#imeSink` and its inline offscreen style;
- `styles.css`: no rule added or removed, no selector, size, color or layout value changed.
  The only edits are five `inset: 0` → `top/right/bottom/left: 0` expansions (plus the same in
  the `#permModal` inline `<style>`) and one `-webkit-backdrop-filter` added alongside the
  existing `backdrop-filter` on `.loading`. Both are Safari-13 compatibility fixes; the second
  restores the intended blur on old WebKit rather than changing the design.

Two *content* deltas inside otherwise-unchanged elements, both intentional:
- the HTTPS modal paragraph (rewritten; fixes the "sucre" typo — see concern 1);
- the compat modal's generated list — "WebCodecs audio and AudioWorklet" is now two rows, and a
  new "Cross-origin isolation" row can appear (new:1785-1792). The host's COOP/COEP headers make
  that row rare on HTTPS.

**Behavior.** `/reconfig` cadence and both epochs; kick handling and its message; device-token
storage under `se.deviceToken` and its presentation on rejoin; the OTP flow end to end
(`VALID_RE`, per-box input/keydown/paste handlers, `validate()`, URL prefill and the
`history.replaceState` scrub); the same-device gate and its hand-off to `offerHttpsOnLoad`; the
remote-control permission modal's own markup and handlers; fullscreen-toggle semantics
(`fullscreenMode` → viewport mode vs fullscreen, the gesture handler, the `beforeunload`/
`pageshow`/`visibilitychange` hygiene); clipboard forwarding (`copy`/`cut`/`paste`, opcode `0x30`);
file drop framing (only `BigInt` → paired u32 writes, byte-identical).

**Performance.** No new per-frame main-thread work: `presentFrame` allocates nothing;
`avsync` is still gated to 200 ms (`transform-worker.js:313`), so `getOutputTimestamp()` via
`maybeUpdateTarget` runs ~5/s, not per frame. All wire buffers stay preallocated
(`statsBuf` is one 96-byte `ArrayBuffer`, `keyReqBuf` one 2-byte). No new polling loop —
`videoStatsPoll` (1 s) and the audio `devicechange` poll (4 s) both pre-date this branch;
`transformWatchdog` is a one-shot cleared in `teardownSession` (new:947). Listener hygiene
improved: `waitForIceGathering` now removes both of its listeners and its timers in `finish()`
(new:906-914), where the old version left the `icegatheringstatechange` listener and the timeout
behind on every connect. The two new `video` listeners (new:345-346) are module-scope and
bounded.

---

## 5. Summary

11 regressions: **5 major**, **6 minor**. The three worst are R1 (TURN sessions ship host-only
candidates because `needRelay` is never passed), R2 (the client stops demoting to `<video>` on a
lost keyframe because it trusts an opcode no host parses), and R3 (the `inFlight` counter can
leak and wedge the decoder into a keyframe-only stream).

The appearance surface is clean — one paragraph of copy, and the CSS changes are pure
Safari-13 compatibility with no design impact. The host contract is intact in both directions
except for the one-way `0x64`, which is wire-safe but inert.

# Client page review 2 — feature detection and fallback paths

Branch `worktree-agent-a5109a99927714a08` against `latency-overhaul`, restricted to
`src-tauri/src/streamer/static/` (plus the `server.rs` ETag change and
`scripts/test-client-stats.mjs` that ship in the same range).

Angle: for every modern API the page reaches for, (a) is the detection a real capability
probe, (b) does it probe exactly what is used, (c) does the fallback actually *trigger* on
failure, (d) is the floor behind each tier no worse than today's behaviour, (e) is the tier
that ended up live reported to the host, (f) does any ladder skip a rung.

Support facts below are taken from `@mdn/browser-compat-data` (`api/*.json`, fetched
2026-09-08), not from memory. Line numbers are on the post-change files.

---

## 1. Summary

The branch is a large net improvement on (a), (b) and (d): every probe I checked is a real
capability test rather than a UA sniff (the one exception is `detectHasEarpiece`, §6 D14),
the `VideoDecoder` probe now includes `optimizeForLatency` instead of probing a config it
does not install, `verifyCapabilities()` replaces a bare `typeof AudioDecoder` sniff with
`AudioDecoder.isConfigSupported`, `enterFullscreen()` finally normalises the old-WebKit
`undefined` return into a Promise, `new NoSleep()` is lazy so a failed `/nosleep.js` no
longer kills the script, and `inset:` was replaced with longhands so the CSS still parses on
the iOS 12/13 floor.

Where it is weak is (c), (e) and (f):

- Three ladders do not step down when the rung they are on *fails* as opposed to being
  *absent*: pointer lock (D7), `sendKeyFrameRequest` (D8), `AudioContext.setSinkId` (D12).
- Two tiers are declared live from the existence of a constructor or a hard-coded `true`
  rather than from anything working: the out-of-band keyframe request (D2) and the
  track-generator presentation path (D3, D4).
- One silent-failure path was added, not removed: `AudioData.copyTo` failure is swallowed
  frame by frame with no counter and no demotion (D5).
- (e) is currently answered **no** end to end: the client writes four tier bytes and a
  22-bit degradation field into a 96-byte v2 stats frame, and the host's parser reads the
  72-byte v1 prefix and drops the rest (already tracked, see §7).

18 defects: 2 blocker, 10 major, 6 minor. 7 lower-confidence concerns.

Three of them (D1, D3, D4) were independently found by a concurrent reviewer working the same
files and already have `findings/` entries; I kept them in the numbering because the tier
table depends on them, and cite the existing files rather than writing duplicates.

---

## 2. Per-browser expected tier table

Codes as the client defines them:
transport `1 = RTCRtpScriptTransform`, `2 = createEncodedStreams`, `0 = none (<video>)`;
render `1 = canvas2d`, `2 = trackGenerator`, `3 = <video>`;
key `1 = sendKeyFrameRequest`, `2 = out-of-band 0x64`, `0 = none`;
audio `1 = worklet + SAB`, `2 = worklet + postMessage`, `3 = Opus track (NetEQ)`.

Audio 1 vs 2 is decided by `crossOriginIsolated`, i.e. by HTTPS: on the HTTP origin the page
serves no COOP/COEP, so every "1" below becomes "2".

| Client | transport | render | key | audio | Path taken, and what is wrong |
|---|---|---|---|---|---|
| Chrome/Edge desktop 120 | 2 | 1 | 2 (fiction) | 1 | `RTCRtpReceiver.transform` is Chrome **141**, so `hasScriptTransform` is false and the legacy `createEncodedStreams` (Chrome 86) path is taken. `keyTier` reports 2 but the host has no `0x64` arm — **D2**. Stall budget is silently doubled to ~8 s. `DEG.SCRIPT_TRANSFORM_UNAVAILABLE` set, correctly. |
| Chrome/Edge desktop 141+ | 1 | 1 | 1 | 1 | The intended path. `hasLegacyEncodedStreams` is also true, so the 2.5 s `transformWatchdog` arms and a transform that never fires steps down one rung (good — this is (f) done right). |
| Chrome Android 141+ | 1 | 1 | 1 | 1 | Same. `navigator.keyboard.lock()` exists and rejects; the rejection is swallowed and `FLAG.KEYBOARD_LOCK` still reads 1 (LC6). Pointer lock is a no-op on Android. |
| Chrome Android ≤140 | 2 | 1 | 2 (fiction) | 1 | As Chrome 120. |
| Samsung Internet (current ≈ Chrome 130) | 2 | 1 | 2 (fiction) | 1 | As Chrome 120. Will move to transport 1 only when Samsung rebases past Chromium 141. |
| Firefox 130+ desktop | 1 | 1 | 1 | 1 | `RTCRtpScriptTransform` 117 + `VideoDecoder` 130 + `transferControlToOffscreen` 105. `createEncodedStreams` is **absent** in Firefox, so `retryWithLegacyTransform()` degenerates straight to `fallbackToVideo()` and no `transformWatchdog` is armed — that is correct, not a defect. `navigator.keyboard` absent → DEG bit 21. FF 130/131 lack `requestVideoFrameCallback` (132) → DEG bit 20, which only matters after a demotion. `selectAudioOutput` (116) is present and `pickOutput()` is never called — **D13**. |
| Firefox 117–129 | 0 | 3 | 0 | 3 | Has the transform but not `VideoDecoder`, so `canWebCodecs` is false and the `<video>` floor is taken. Correct. |
| Firefox Android | 0 | 3 | 0 | 3 | `VideoDecoder` is `false` on firefox_android. Floor. `getCoalescedEvents` exists (79) but is always empty; `coalescedOf` handles that. |
| Safari macOS/iOS 15.0–15.3 | 0 | 3 | 0 | 3 | No transform (15.4), no WebCodecs. Floor, unchanged from today. |
| Safari macOS/iOS 15.4–16.3 | 0 | 3 | 0 | 3 | `RTCRtpScriptTransform` yes, `VideoDecoder` no (16.4) → `canWebCodecs` false. Floor. |
| Safari iOS 16.4–17.x | 1 | 1 | 1 | 3 | Video fast path. `AudioDecoder` is Safari **26**, so audio stays on NetEQ — correctly reported. `navigator.wakeLock` is iOS **18.4**, so DEG `WAKE_LOCK_API_ABSENT` and the nosleep shim; correct tiering. No pointer lock on iOS at all (`Element.requestPointerLock` safari_ios `false`), so `acquirePointerLock` takes the cursor-hiding branch. `getCoalescedEvents` is 18.2 → DEG bit 14. `HTMLMediaElement.setSinkId` is 18.4 → `canSink` false, so the output list is the `audioSession` Speaker/Earpiece pair only, gated by a UA sniff (**D14**). |
| Safari iOS 18 / 18.4 | 1 | 1 (2 only with `?present=track`) | 1 | 3 | `VideoTrackGenerator` lands at Safari 18 — this is the only engine with the *new* name. With `?present=track` the page hits **D3/D4** and shows a blank canvas. 18.4 adds wake lock, `setSinkId`, `outputLatency` and `unadjustedMovement`. |
| Safari iOS/macOS 26 | 1 | 1 | 1 | 1 | First Safari with `AudioDecoder`, so the audio fast path opens. This is precisely where **D5** bites: `isConfigSupported` says yes, and if `copyTo(..., 'f32-planar')` then refuses, every packet is dropped in a bare `catch` and the client reports `audioPath = 1`. |
| iOS 12/13 WebKit | 0 | 3 | 0 | 3 | No transform, no WebCodecs, no `AudioWorklet` (14.1) → `<video>` + NetEQ. I grepped the four changed JS files for ES2020+ syntax (`?.`, `??`, `\|\|=`, `.at`, `flatMap`, `replaceAll`, `matchAll`, `globalThis`, `structuredClone`, BigInt): none. `inset:` in `styles.css` was converted to longhands. No regression against the floor. |

Two structural observations from the table:

- **Chrome/Edge/Samsung below 141 is the majority of the install base and it is the one row
  whose keyframe recovery is a fiction.** That is D2, and it is why I rated it blocker.
- The two tiers that are *new* in this branch (track generator, out-of-band keyframe) are the
  two that do not work. The tiers that were already there got better.

---

## 3. (a) Is the detection a real probe?

Yes everywhere I checked, with one exception.

Verified real probes: `('transform' in RTCRtpReceiver.prototype) && typeof
RTCRtpScriptTransform !== 'undefined'` (index.html:169); `typeof
RTCRtpReceiver.prototype.createEncodedStreams === 'function'` (:170); `typeof
canvas.transferControlToOffscreen === 'function'` (:171); `navigator.wakeLock &&
typeof navigator.wakeLock.request === 'function'` (:582); `navigator.keyboard &&
navigator.keyboard.lock` (:228); `typeof video.requestVideoFrameCallback === 'function'`
(:229, :1107); `'playoutDelayHint' in receiver` / `'jitterBufferTarget' in receiver`
(:1283, :1376); `window.PointerEvent && ('getCoalescedEvents' in PointerEvent.prototype)`
(input.js:475); `'onpointerrawupdate' in window` (input.js:609); `matchMedia('(any-pointer:
fine)')` (input.js:683); `'audioWorklet' in AC.prototype` (audio.js:30); `self.crossOriginIsolated
=== true && typeof SharedArrayBuffer === 'function' && typeof Atomics === 'object'`
(audio.js:61-65); `'setSinkId' in HTMLMediaElement.prototype` and `'setSinkId' in
AC.prototype` (audio.js:150-151); `typeof md.selectAudioOutput === 'function'` (:152);
`'audioSession' in navigator` (:154); `typeof VideoTrackGenerator === 'function'` /
`typeof MediaStreamTrackGenerator === 'function'` (transform-worker.js:161-165);
`'ondequeue' in d` (transform-worker.js:411).

The exception is `detectHasEarpiece()` (audio.js:140), `/iPhone/.test(navigator.userAgent)`,
which gates the entire `SE_LOUDSPEAKER`/`SE_EARPIECE` output list — D14.

`engineSwapsScreenOnRotation()` (index.html:481) and `detectOs()` (:1588) are also UA/platform
sniffs, but they are unchanged and there is no capability to probe there.

## 4. (b) Does the probe test what is used?

Improved and now mostly correct.

- `VideoDecoder.isConfigSupported(decoderConfig(c))` (transform-worker.js:456) probes the
  same object `configure()` is given, including `optimizeForLatency` — this fixes a real
  latent bug called out in the code comment. The separate `prefer-hardware` probe (:472) is
  telemetry only and the install still uses `no-preference`, which is right.
- `AudioDecoder.isConfigSupported({codec:'opus', sampleRate:48000, numberOfChannels:2})`
  (audio.js:48) matches `decoder.configure` in audio-worker.js:170 exactly.
- **But** the codec hint and coded size the probe is supposed to use arrive after the
  decoder is already configured — D9. And neither audio probe covers `AudioData.copyTo` with
  format conversion, which is what actually moves the samples — D5.
- `hasLegacyEncodedStreams` tests the method's existence, but `createEncodedStreams()` only
  works when the `RTCPeerConnection` was built with `encodedInsertableStreams: true`; the
  page does set that whenever it is going to use the legacy path (index.html:1264-1267), and
  the actual call is wrapped with a `fallbackToVideo` on throw (:1362-1367). Correct.

## 5. (c) Does the fallback trigger on failure?

This is the weak axis. Handled correctly: `enterFullscreen` normalises `undefined` and
catches synchronous throws (index.html:599-616); `makeTransformWorker` falls back from the
blob worker to the direct URL and then to `fallbackToVideo` via the outer catch (:1240-1254);
`RTCRtpScriptTransform` construction throwing steps down one rung, not all the way
(:1344-1350); `configerror`/`decoderdead` from the worker demote to `<video>`
(:1296-1300); `makeSharedRing` returns null on throw and the postMessage ring takes over
(audio.js:714-726); `AudioWorkletNode` construction failure resolves `prepare()` false and
the client does not claim the fast path (audio.js:672-697 + index.html:1970-1981).

Not handled: D5 (`copyTo`), D7 (pointer lock throw), D8 (`sendKeyFrameRequest` throw/reject),
D12 (`ctx.setSinkId` reject), D3 (track transfer throw leaves the writer live), D10
(`getContext('2d')` returning null).

## 6. Numbered defects

### D1 — blocker — `waitForIceGathering` is never told a relay is needed
`index.html:1456` calls `waitForIceGathering(pc, hasTurnServer(iceServers) ? 6000 : 3000)`,
but the function signature is `(pc, timeoutMs, needRelay)` (`index.html:897`). `needRelay` is
always `undefined`, so the `if (needRelay) { if (type === 'relay') arm(ICE_RELAY_GRACE_MS); }`
branch at :924-925 is dead and every candidate takes the `else` branch: gathering is cut
**200 ms after the first host or srflx candidate**, which on any LAN is the first few
milliseconds. `ICE_RELAY_GRACE_MS` is unreferenced.

Scenario: a cross-network (cloud-relay) join with TURN configured. On `latency-overhaul` the
client waited for `iceGatheringState === 'complete'` or 6 s, so relay candidates made it into
the single WHEP offer. Now the offer is posted with host and srflx candidates only, ~200 ms
in, before the TURN allocation has returned. WHEP has no PATCH to trickle them later (see
`findings/major-whep-no-trickle-ice.md`), so the relay pair can never form from the client
side. This is a regression, and it also invalidates that finding's claim that "with TURN
configured it still waits for a relay candidate".

Fix: pass the third argument — `await waitForIceGathering(pc, needRelay ? 6000 : 3000,
needRelay)` with `const needRelay = hasTurnServer(iceServers)`. Consider also not arming the
host grace at all while `needRelay` is set, so a fast srflx cannot pre-empt the relay wait.

### D2 — blocker — the out-of-band keyframe tier is declared live unconditionally
`index.html:1335` posts `{type:'outofbandkey', available: true}` to the worker as a
constant, before any DataChannel exists, and `transform-worker.js:241-243` promotes
`keyTier` to `KEYREQ.outOfBand` (2) on the spot. Nothing ever demotes it.

The request itself is `OP.KEYFRAME_REQUEST = 0x64` (input.js:11, :386-395). The host has no
arm for `0x64` — `src-tauri/src/streamer/input/protocol.rs` `op` stops at `BYE = 0x63` and
`parse` falls through to `_ => return None` (tracked host-side as
`findings/major-host-keyframe-request-opcode-unhandled.md`). The client-side half is a
separate defect, because the client makes three tiering decisions on the strength of it:

1. `transform-worker.js:591` posts `nokeyframe` with `keyTier`, and `index.html:1313` only
   demotes `if (!m.data.keyTier)`. `keyTier` is never 0, so the "we asked by every means and
   the host still sent no keyframe" rung is unreachable.
2. `index.html:1178` `if (w && w.keyTier === 0) degrade(DEG.KEYFRAME_REQUEST_IMPOSSIBLE)` can
   never fire, so the host is never told, even once it can read byte 74.
3. `index.html:1041-1044`: `asked` is true whenever the reliable channel is open, so
   `canAsk` raises the stall threshold from 2 to 4 ticks — 4 s to 8 s — before
   `fallbackToVideo`. On Chrome/Edge/Chrome-Android ≤140 and Samsung Internet, which have no
   `sendKeyFrameRequest`, a single lost IDR now freezes the picture for roughly twice as long
   as it did on `latency-overhaul`.

Fix: make `available` conditional and revocable — post it from `reliable.onopen`, and have
`RemoteInput.requestKeyframe()` return false until the host has acknowledged the opcode (a
version byte in the WHEP response, or an ack frame). Until the host arm exists, `keyTier`
should be 0 on the legacy transport, which restores the 2-tick demotion.

### D3 — major — a failed track transfer leaves the worker writing to an orphaned writable
`transform-worker.js:217-232`: when `self.postMessage({type:'trackgen', track}, [track])`
throws — transferring a `MediaStreamTrack` is not universally supported, which is exactly why
the `try` is there — the worker posts `track: null` but leaves `generator`, `genWriter` and
`renderTier` set from `setupTrackGenerator()` (:167-187). The page then calls
`attachTrackGenerator(null)` (index.html:993-1001), transfers a fresh OffscreenCanvas and
posts `{type:'canvas'}`; `setupCanvas` assigns `ctx` and resets `renderTier` to `canvas2d`.
But `presentFrame` (:192-203) tests `if (genWriter)` **first**, so every decoded frame is
written into the orphaned writable and the canvas is never drawn.

Consequence: `framesRendered` still increments, the `rendered` message still fires,
`onStreamLive()` clears the 5 s watchdog and `handleRenderCount` sees renders advancing — so
nothing demotes. The session is "live" on a permanently black canvas, and the host is told
`renderTier = 1`.

Fix: in the catch, tear the generator down before posting null —
`try { generator.track && generator.track.stop(); } catch (_) {}`, then
`generator = null; genWriter = null; renderTier = RENDER.none;`.

### D4 — major — `ontrack` hides the track-generator video behind the canvas
`index.html:1378` correctly guards the first `showCanvas()` with `if (PRESENT_PREF !==
'track')`, but `index.html:1389`, inside `pc.ontrack`'s `if (webcodecs)` branch, calls
`showCanvas()` unconditionally. `attachTrackGenerator()` runs on a worker message
(immediately after the `trackgen` post), while `ontrack` fires after `setRemoteDescription`,
i.e. after a network round trip — so `showCanvas()` always wins and hides the `<video>` that
`attachTrackGenerator` just attached the generator track to.

Scenario: `?present=track` on Chrome (legacy `MediaStreamTrackGenerator`) or Safari 18+ (new
`VideoTrackGenerator`) — the only two engines where the tier exists. Result is a blank canvas
over a working video element. Combined with D3 this means the presentation ladder's second
rung has never worked.

Fix: `if (PRESENT_PREF !== 'track') showCanvas();` at :1389, mirroring :1378.

### D5 — major — `AudioData.copyTo` failure is swallowed per frame, with no counter and no demotion
`audio-worker.js:118-125`:

```js
try {
    ad.copyTo(l, { planeIndex: 0, format: 'f32-planar' });
    if (chs >= 2) ad.copyTo(r, { planeIndex: 1, format: 'f32-planar' });
    else r.set(l);
} catch (e) {
    ad.close();
    return;
}
```

`e` is unused. Nothing is counted, no message is posted, `decoderResets` is untouched, and
the page's `decoder-failed` path is never reached. Every packet is dropped and the client
keeps reporting `audioPath = workletSab` and `audioUnderruns` climbing with no cause.

Scenario: an engine whose `AudioDecoder` answers `supported: true` for `opus` but whose
`copyTo` does not implement format conversion away from its native output layout. The two
engines that just shipped `AudioDecoder` — Safari 26 and Firefox 130, the exact pair the
branch's own comment (`audio.js:38-41`) says made this urgent — are where that risk lives,
and neither is covered by `isConfigSupported`.

Fix: count the failures, post `decoder-failed` after a small number of them so the existing
demotion path runs, and extend `verifyCapabilities()` to decode one known Opus frame and
`copyTo` it before claiming the fast path — that is the only probe that tests what is used.

### D6 — major — the audio ladder has no rung under "the fast path failed"
`audio.js:748-756` handles `decoder-failed` by calling
`attachFallbackStream(state.fallbackStream)`, but `state.fallbackStream` is only ever set
from `pc.ontrack` for an audio track, and the host adds no audio track precisely when the
client claimed the fast path. So in the only case this handler exists for, the fallback
stream is null and the result is permanent silence.

Host-side mitigation is tracked in
`findings/major-host-audio-track-absent-when-client-claims-fast-path.md`. The client-side
half is still missing: on `decoder-failed` the page should set `audioFast = false` and call
`reconfigure()`, so the next WHEP offer carries a recvonly audio transceiver and the host
answers with a track. Nothing today does that.

### D7 — major — a throwing `requestPointerLock({unadjustedMovement})` loses pointer lock entirely
`index.html:648-682`:

```js
let p = null;
try { p = el.requestPointerLock({ unadjustedMovement: true }); } catch (_) { p = null; }
if (p && typeof p.then === 'function') { … return; }
// No Promise came back, so the options bag was ignored
degrade(DEG.UNADJUSTED_MOVEMENT_UNAVAILABLE);
pointerLockUnadjusted = false;
```

The comment above the function names this exact trap, but the code does not distinguish "the
call threw" from "the call returned undefined". In the throw case **no pointer lock was ever
requested** and there is no retry — the plain `el.requestPointerLock()` retry exists only
inside the promise-rejection branch at :671. Relative movement is then dead for the whole
session: `onPointerMove` (input.js:513) checks `document.pointerLockElement`, finds nothing,
and routes mouse motion down the absolute-coordinate path.

Fix: record whether it threw and, if so, call `el.requestPointerLock()` with no argument and
treat its return the same way (promise → catch → `POINTER_LOCK_DENIED`, undefined → done).

### D8 — major — `sendKeyFrameRequest` failure does not fall through to the out-of-band rung
`transform-worker.js:525-547`: `requestKey()` sets `keyTier = 1` and returns as soon as
`typeof transformer.sendKeyFrameRequest === 'function'`. A synchronous throw is `note()`d and
a rejected promise is `note()`d — in both cases the function has already returned, so the
`outOfBandKey` rung below it is unreachable. The out-of-band request is only ever tried when
the *method is absent*, never when it *fails*.

Scenario: an engine that exposes `sendKeyFrameRequest` on the transformer but rejects it for
a receiver in this state. The client keeps reporting `keyTier = 1`, `handleRenderCount` keeps
the relaxed 4-tick budget because `keyTier === 1` sets `canAsk`, and no keyframe is ever
requested by any means.

Fix: on throw or rejection, clear `keyTier` back to `outOfBand`/`none` and issue the
out-of-band request.

### D9 — major — the codec hint and coded size arrive after the decoder is configured
`transform-worker.js:560-563` — `startPump()` calls `ensureConfigured()` immediately, and
`startPump` is reached from `onrtctransform` (:551, fired when the transform is assigned at
index.html:1346) or from the `encodedstream` message (:257, posted at index.html:1368). Both
happen **before** `await pc.setRemoteDescription(...)`. The `codechint` message carrying
`profileLevelId` and `width`/`height` is posted at index.html:1498-1503, after the WHEP POST
round trip.

`ensureConfigured()` early-returns `if (configured && decoder && decoder.state !== 'closed')`
(:439), so `preferredCodec`, `hintWidth` and `hintHeight` are only consulted after a decoder
reset. In practice every client probes and configures `avc1.640034` (High 5.2) with no
`codedWidth`/`codedHeight` — which is the behaviour the commit "probe the config it is
actually given" set out to remove, and it is also the case where `isConfigSupported` is least
able to answer honestly.

Fix: have `startPump` await the first `codechint` behind a short deadline (or defer the
configure to the first encoded frame, by which time the hint has landed), and fall back to
the current blind configure if it never arrives.

### D10 — major — `setupCanvas` does not check that it got a 2D context
`transform-worker.js:155-159` assigns `ctx = canvas.getContext('2d', {alpha:false})` and sets
`renderTier = RENDER.canvas2d` unconditionally. If `getContext` returns null, `presentFrame`
(:204) returns false without drawing while `onDecodedFrame` still increments
`framesRendered` and posts `rendered` — so `onStreamLive()` clears the watchdog,
`handleRenderCount` sees renders advancing, and the session is live on a black canvas with
`renderTier` reported as 1. If `getContext` *throws*, it throws out of `self.onmessage` with
no handler at all.

Fix: `if (!ctx) { self.postMessage({type:'configerror', message:'no 2d context'}); return; }`
so the page's existing `configerror → fallbackToVideo` path runs.

### D11 — major — the v2 tier bytes are written, undocumented, and dropped by the host
`input.js:297-372` writes a 96-byte v2 frame whose tail carries `transportTier`,
`renderTier`, `keyTier`, `sinkTier`, the suppressed-error count and the 22-bit `degraded`
field. `docs/TELEMETRY.md` on this branch still documents "version (currently 1)", a
72-byte frame and a nine-bit flag table, with no tier or degradation section;
`transform-worker.js:1161`'s "Tier codes, mirrored in docs/TELEMETRY.md" is false, and
`scripts/test-client-stats.mjs:109` asserts "the 96 bytes docs/TELEMETRY.md documents for
v2" against a document that documents 72. The host-side half (the parser reading only the v1
prefix) is tracked in `findings/major-telemetry-client-stats-v2-tail-is-discarded.md`.

This is the answer to review question (e): today, **no**. The client computes every tier
correctly and nothing downstream can see any of it.

Fix: update `docs/TELEMETRY.md` with the v2 layout, the four tier enumerations and the DEG
bit table, in the same change as the Rust parser.

### D12 — major — the `setSinkId` ladder does not fall through on rejection
`audio.js:447-500`: the `ctx.setSinkId` branch (:463-472) and the
`MediaStreamDestination` + `<audio>.setSinkId` branch (:474-499) sit inside one
`try { … } catch (_) { note(); }`. A rejected `await state.ctx.setSinkId(id)` — `NotFoundError`
on a device id that has since gone away, or `NotAllowedError` — aborts to the shared catch and
the element rung below is never tried, even though the page has both capabilities on Chrome
110+. `speakers.appliedSink` and `sinkTier` are left at their previous values, so the host is
told the old sink is still in force.

Fix: give the ctx attempt its own try/catch and fall through to the element path on failure;
set `sinkTier` only after the await resolves.

### D13 — minor — the `selectAudioOutput` tier is unreachable
`audio.js:506-517` implements `pickOutput()` and `SINK_TIER.selectAudioOutput = 4`, and the
comment correctly notes it needs transient activation. Nothing in `index.html` calls it —
there is no button, no gesture handler, no reference. `MediaDevices.selectAudioOutput` is
Firefox-116-only, so this is the one engine with a permission-free output picker and the page
never offers it. `sinkTier === 4` can never be reported.

### D14 — minor — the earpiece/loudspeaker tier is gated on a UA sniff
`audio.js:139-141` `detectHasEarpiece()` is `/iPhone/.test(navigator.userAgent)`, and
`initSpeakers` (:158-159) makes it the sole gate on `canCategory`, hence on whether the host
sees Speaker/Earpiece at all. An iPhone with "Request Desktop Website" enabled reports a
Macintosh UA and silently loses the choice. `detectIOS()` right above it already handles the
iPadOS `MacIntel` + `maxTouchPoints` case and is not reused. There is no true capability
probe for "this device has an earpiece", but `detectIOS() && !isIpad` would at least be
consistent with the file's own other detector.

### D15 — minor — two disagreeing definitions of "fullscreen"
`index.html:617-619` `currentFullscreenEl()` checks `fullscreenElement ||
mozFullScreenElement || webkitFullscreenElement`; `index.html:1157-1159` `isFullscreen()`,
which is what feeds `FLAG.FULLSCREEN` in the stats frame, checks only `fullscreenElement ||
webkitFullscreenElement`. On a Firefox old enough to need the moz prefix the page believes it
is fullscreen and tells the host it is not. Collapse `isFullscreen()` into
`!!currentFullscreenEl()`.

### D16 — minor — stale document references introduced by this branch
`transform-worker.js:150` and `index.html:187` both cite
`docs/manual-tests/client-page.md` for the presentation-tier A/B. That file does not exist on
this branch (`docs/manual-tests/` contains only `macos-permissions.md`). Combined with the
`docs/TELEMETRY.md` mismatch in D11, the two documents the new tier machinery points a reader
at are one missing and one stale.

### D17 — minor — the keyboard lock is requested before fullscreen is re-entered
`index.html:783-789`: the visibility handler does `await acquireKeyboardLock();` and only
then `if (!currentFullscreenEl()) await enterFullscreen(...)`. `navigator.keyboard.lock()` is
specified to reject outside fullscreen, so on the return-from-background path the lock is
requested at the one moment it cannot succeed, the rejection is swallowed at :639, and the
lock is only recovered by the subsequent `fullscreenchange` handler (:806-813). Swap the two.

### D18 — minor — a returned Promise is taken as proof `unadjustedMovement` was accepted
`index.html:664-666` sets `pointerLockUnadjusted = true` and clears the degradation bit
*before* the promise settles, purely because a promise came back. Two ways that is wrong:

- Chrome 88–110 accepts and honours `unadjustedMovement` while `requestPointerLock` still
  returns `undefined`; those builds fall into the `else` at :679 and are reported as *not*
  unadjusted when they are.
- An engine that returns a promise but ignores unknown dictionary members resolves normally
  and is reported as unadjusted when it is not. Firefox is the live example: `requestPointerLock`
  has been there since 50 and `options_unadjustedMovement_parameter` only lands in **152**.

Fix: set the flag in the fulfilment handler, not before it, and treat an `undefined` return as
"unknown" rather than "unavailable".

## 7. Lower-confidence concerns

- **LC1 — the two ends of the audio ring decide `shared` independently.**
  `audio-worklet.js:37-44` silently sets `this.shared = false` if the typed-array views over
  the SAB cannot be constructed, while `audio-worker.js:57-72` sets `shared = true` on its own
  validation. A divergence means the worker writes into the SAB and the worklet reads a
  private ring that nothing fills — silence — and `getSyncInfo().transport` (audio.js:830)
  still reports `'shared'` because it only looks at `state.sab`. The worklet does post its own
  `shared` in `maybePostStats` and nobody reads it. Low probability (both sides validate the
  same size and power-of-two), but there is no handshake.
- **LC2 — `pointerrawupdate` replaces `pointermove` outright.** `input.js:609-613` installs
  one or the other. `pointerrawupdate` is Chromium-only and is not dispatched while the
  document lacks focus; nothing verifies that at least one move source is producing events. A
  cheap guard would be to fall back to `pointermove` if no raw update has arrived within the
  first second of an active pointer.
- **LC3 — the compat modal is built from the unverified capability snapshot.**
  `compatGate()` (index.html:1856) runs `checkBrowserRequirements()` at load, against the
  `detectCapabilities()` sniff; `verifyCapabilities()` only runs at submit
  (:1966). A browser whose `AudioDecoder` fails the real probe is never told why its audio is
  on the slow path.
- **LC4 — `DEG.REFRESH_RATE_MEASURED` is set on essentially every client.**
  `screenRefreshRate()` (index.html:1623) reads `window.screen.refreshRate`, which is not a
  standard property in any shipping engine, so `resolveRefreshRate()` almost always takes the
  measured branch and sets bit 18. A flag that is always on carries no signal.
- **LC5 — the `prefer-hardware` telemetry probe is on the join path.**
  `transform-worker.js:472` runs a second `isConfigSupported` purely to fill
  `decoderHardware`. On some builds `prefer-hardware` instantiates and tears down a GPU
  decoder factory. Worth moving off the first-frame path.
- **LC6 — `FLAG.KEYBOARD_LOCK` reports availability, not liveness.** `index.html:1190` is
  `!!(navigator.keyboard && navigator.keyboard.lock)`; `acquireKeyboardLock` swallows the
  rejection, so a Chrome client that never got the lock is indistinguishable from one that
  did. `docs/TELEMETRY.md` does say "keyboard lock available", so it is honest — but it is
  the only flag in the frame that is not liveness, and D17 makes the rejection likely.
- **LC7 — the `AudioContext` is constructed on every join and never closed.**
  `beginFromGesture()` (audio.js:653) runs from the submit handler regardless of whether the
  host will send audio, and `teardown()` (:814) does not `close()` it. On a device the host
  has audio disabled for, that is a running audio graph for the life of the tab.

## 8. Findings files

| # | `findings/` file |
|---|---|
| D1 | `major-client-ice-gathering-needrelay-never-passed.md` (already written by a concurrent reviewer; I would rate it blocker) |
| D2 | `blocker-client-out-of-band-keyframe-tier-is-declared-live-unconditionally.md` |
| D3 | `major-client-trackgen-writer-orphans-the-canvas.md` (already written) |
| D4 | `minor-client-present-track-undone-by-ontrack-showcanvas.md` (already written) |
| D5 | `major-client-audiodata-copyto-failure-is-swallowed-per-frame.md` |
| D6 | `major-client-no-rung-below-a-failed-webcodecs-audio-path.md` |
| D7 | `major-client-pointer-lock-lost-when-the-options-bag-throws.md` |
| D8 | `major-client-sendkeyframerequest-failure-does-not-fall-through.md` |
| D9 | `major-client-codec-hint-arrives-after-the-decoder-is-configured.md` |
| D10 | `major-client-offscreen-2d-context-null-is-never-checked.md` |
| D11 | `major-telemetry-client-stats-v2-tail-is-discarded.md` (host half, already written) + `minor-docs-client-tier-machinery-points-at-a-missing-and-a-stale-document.md` |
| D12 | `major-client-setsinkid-ladder-does-not-fall-through-on-rejection.md` |
| D13 | `minor-client-selectaudiooutput-tier-is-unreachable.md` |
| D14 | `minor-client-earpiece-tier-gated-on-a-useragent-sniff.md` |
| D15 | `minor-client-two-disagreeing-definitions-of-fullscreen.md` |
| D16 | `minor-docs-client-tier-machinery-points-at-a-missing-and-a-stale-document.md` |
| D17 | `minor-client-keyboard-lock-requested-before-fullscreen-is-re-entered.md` |
| D18 | `minor-client-unadjusted-movement-inferred-from-a-promise-return.md` |

Related host-side entries that this review depends on:
`major-host-keyframe-request-opcode-unhandled.md` (D2),
`major-host-audio-track-absent-when-client-claims-fast-path.md` (D6),
`major-whep-no-trickle-ice.md` (D1 — note that its claim "with TURN configured it still waits
for a relay candidate" is false, for the reason D1 gives).

## 9. What I verified, and how

- Read the full post-change `index.html`, `input.js`, `audio.js`, `audio-worker.js`,
  `audio-worklet.js`, `transform-worker.js` and the `styles.css` diff, then the diff against
  `latency-overhaul` for each.
- Support claims are from `@mdn/browser-compat-data` `api/*.json` fetched from
  `raw.githubusercontent.com/mdn/browser-compat-data/main` on 2026-09-08 and parsed, not from
  memory: `RTCRtpScriptTransform` (chrome 141 / firefox 117 / safari 15.4),
  `RTCRtpScriptTransformer.sendKeyFrameRequest` (same), `RTCRtpReceiver.transform` (same),
  `RTCRtpReceiver.createEncodedStreams` (chrome 86, firefox/safari false), `VideoDecoder`
  (94 / 130 / 16.4, firefox_android false), `AudioDecoder` and `AudioData.copyTo` (94 / 130 /
  26, firefox_android false), `VideoTrackGenerator` (safari 18 only),
  `MediaStreamTrackGenerator` (chrome 94 only), `HTMLCanvasElement.transferControlToOffscreen`
  (69 / 105 / 16.4), `HTMLVideoElement.requestVideoFrameCallback` (83 / 132 / 15.4),
  `Navigator.wakeLock` (84 / 126 / 16.4 desktop, 18.4 iOS), `Keyboard.lock` (chrome 68 only),
  `Element.requestPointerLock` (37 / 50 / 10.1, iOS false) and its
  `options_unadjustedMovement_parameter` (88 / 152 / 18.4), `Element.requestFullscreen`
  (71 / 64 / 16.4), `PointerEvent.getCoalescedEvents` (58 / 59 / 18.2),
  `AudioContext.outputLatency` (102 / 70 / 18.4), `baseLatency` and `getOutputTimestamp`
  (57-58 / 70 / 14.1), `AudioContext.setSinkId` (chrome 110 only),
  `HTMLMediaElement.setSinkId` (49 / 116 / 18.4), `MediaDevices.selectAudioOutput`
  (firefox 116 only), `Navigator.audioSession` (safari 16.4 only).
- Confirmed the host has no `0x64` arm by reading the `op` module and `parse` in
  `src-tauri/src/streamer/input/protocol.rs` on the branch.
- Confirmed `parse_client_stats` reads only `CLIENT_STATS_LEN = 72` with
  `CLIENT_STATS_VERSION = 1` on the branch, and that `docs/TELEMETRY.md` still documents v1.
- Grepped all four changed JS files for ES2020+ syntax against the iOS 12/13 floor: clean.
- Confirmed `docs/manual-tests/client-page.md` is absent from `git ls-tree` on the branch.
- **Not verified:** no browser was run. Whether `MediaStreamTrack` transfer to the window
  actually throws on a given build (D3 is a defect either way — the failure path is wrong),
  whether any shipping engine throws rather than ignores the pointer-lock options bag (D7,
  same), and whether Safari 26 / Firefox 130 `AudioData.copyTo` refuses `f32-planar` (D5,
  same — the swallowed catch is the defect). My web-search budget was exhausted before I
  could check those three directly; the BCD data above was fetched over plain HTTP instead.

---

*Note added after writing: an uncommitted edit in the `main` working tree is already adding a
v2 section (version 2, 96 bytes, tier rows) to `docs/TELEMETRY.md`. D11's and D16's
documentation half may therefore be resolved by the time this is read; the statements above
describe `worktree-agent-a5109a99927714a08` as reviewed. The host-parser half of D11 is
unaffected.*

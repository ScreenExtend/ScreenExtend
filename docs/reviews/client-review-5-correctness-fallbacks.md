# Client page review 5 — correctness, control flow, and fallback integrity

Branch `worktree-agent-a8dc3a207f40d3925` against `latency-overhaul`. Scope:
`src-tauri/src/streamer/static/`, `src-tauri/src/streamer/server.rs`, `scripts/test-client-*.mjs`
(~3,300 lines, 11 files). Read cold: the diff, then every post-change file in full, then the
host side of each contract (`streamer/input/protocol.rs`, `streamer/audio/protocol.rs`,
`streamer/webrtc_session.rs`, `streamer/server.rs`), `docs/TELEMETRY.md`,
`docs/INPUT-PROTOCOL.md`, and the four earlier reviews. Line numbers are **post-change**.

Angle, in priority order: start/teardown generations and promise ordering; every feature
detection and the floor behind it; the byte-level host contract; and anything that leaves a
device slow or silent with no error.

Counts: **0 blockers, 5 major, 10 minor.** Findings files are in `findings/`, tagged
`Workstream: W11`.

---

## Major

### 1 — `needRelay` means "a TURN server is configured", not "a relay is needed", so LAN joins now stall behind the TURN allocation

`index.html:1406`, `index.html:1650`, `index.html:943-949`, `index.html:970-973`;
host side `server.rs:242-259`.

Review 1 §1 / review 4 §R1 correctly reported that `needRelay` was never passed. It is passed
now — but it is derived from `hasTurnServer(iceServers)`, i.e. *"the ICE list contains a
`turn:`/`turns:` URL"*. That is not the same question.

`AppState::ice_json_for` (`server.rs:242-256`) strips **STUN** for a `SessionLocality::SameNetwork`
peer but deliberately keeps every TURN server:

```rust
if locality == SessionLocality::SameNetwork {
    for turn in [user_turn_ice_server(&self.config), ephemeral_turn_ice_server(&self.config)]
```

So on any host with a TURN server configured — which `CLAUDE.md` treats as the normal setup for
anyone who wants cross-network joins — a same-subnet client computes `needRelay === true` and
`iceGraceFor` returns 0 for every host and reflexive candidate:

```js
function iceGraceFor(type, needRelay) {
    if (needRelay) return type === 'relay' ? ICE_RELAY_GRACE_MS : 0;
```

The wait can then only end on a relay candidate (+400 ms), on `iceGatheringState === 'complete'`,
or on the 6 s deadline. Before this branch it ended 200 ms after the first host candidate. A
misconfigured or unreachable TURN server does not fail fast — libwebrtc retransmits the ALLOCATE
on the RFC 5389 schedule (~7.9 s) — so the client burns the full **6 s before the WHEP POST is
even sent**, on a LAN where the host candidate was always going to be the nominated pair. The
whole engagement is about time to first frame; this is 6 s of black screen added to it, and
`ICE_GATHERING_DEADLINE` is the only trace.

Fix: make the host answer the question it already knows. `/ice-config` computes `locality`
already; add `"needRelay": locality != SameNetwork` to the JSON and have the client read that
instead of sniffing the URL list. Client-only stopgap: keep the relay wait but bound it — once a
host candidate has arrived, arm a second, longer grace (≈1200 ms) so a LAN join is not held to
the full deadline. `scripts/test-client-ice.mjs` should then assert that a LAN-shaped ICE list
does not take the relay branch.

### 2 — the codec hint's coded size is what the client *asked for*, not what the host negotiated, and an over-large hint can fail every candidate and demote the session

`index.html:1711-1724`, `transform-worker.js:533-542`, `transform-worker.js:566-603`;
host side `server.rs:995`, `server.rs:1000-1002`.

The page posts the coded size as `Math.round(joinData.width * getDpr())`. The host does not build
that display. `server.rs:1001` snaps the ratio (`platform::snap_display_dpr`), `server.rs:1002`
rounds the backing size to an even number and clamps it, an `override_for_ip` can replace the
DPR outright, and `server.rs:995` applies a per-device `video_scale` that shrinks the *encoded*
frame further. The number the worker is given is therefore routinely larger — often much larger —
than the stream it will decode.

`doConfigure` puts that number into **every** probe:

```js
let chosen = decoderConfig(candidates[0]);          // carries codedWidth/codedHeight
for (const c of candidates) { const cfg = decoderConfig(c); ... isConfigSupported(cfg) ... }
if (!anySupported) { configError = 'no supported H.264 decoder configuration'; ... throw }
```

`isConfigSupported` is allowed to answer on resolution. A phone reporting `devicePixelRatio` 3 on
a 1440 CSS-px screen hands the worker 4320 px wide; if the UA's H.264 ladder tops out below that,
**every candidate returns `supported: false`**, `configerror` reaches `index.html:1460-1462`, and
`fallbackToVideo()` demotes a session whose real 1280×720 stream the device would have decoded in
hardware. The symptom is exactly "still laggy, no error": `DEMOTED_TO_VIDEO` in the degradation
field and nothing else.

The commit rationale ("the host was asked for exactly this size") is not what the host does.
Fix, smallest first: if the sized probe finds nothing supported, retry the loop with the size
omitted before declaring failure. Better: have `/whep`'s answer carry the encoded dimensions the
host actually chose (it knows them at `server.rs:1002`/`995`) and hint with those; or drop
`codedWidth`/`codedHeight` and let the first keyframe size the decoder, which is the pre-change
behaviour and never demoted anything.

### 3 — `wantsFallbackTrack` asks for a spare Opus track the fast-path offer has no `m=audio` section to carry

`index.html:1415-1425`, `audio.js:130`, `audio.js:1025-1030`, `index.html:1564-1571`;
host side `webrtc_session.rs:1206-1221`, `webrtc_session.rs:1150`, `webrtc_session.rs:1157`.

`verifyCapabilities` sets `caps.wantsFallbackTrack = webcodecsOpus && worklet` and the host
honours it by calling `pc.add_track(...opus...)` at `webrtc_session.rs:1218`. But the client only
adds an audio transceiver on the **slow** path:

```js
if (audioFast) { pc.ondatachannel = ...; }
else { pc.addTransceiver('audio', { direction: 'recvonly' }); }
```

On the fast path the offer contains `m=video` and the DTLS/SCTP section and nothing else. The
host applies that offer at `webrtc_session.rs:1150` and answers at `1157`; an answer may not add
an m-line the offer did not have (RFC 3264 §6), and webrtc-rs builds `create_answer`'s media
sections from the remote description, so the unmatched sendonly audio transceiver is dropped.

Consequence: `pc.ontrack` never fires for audio, `state.fallbackStream` stays null, and
`abandonWebCodecsAudio()` (`audio.js:991-997`) always takes the `SEVideo.onAudioFastLost()` branch
— a full teardown and renegotiation — instead of the instant switch the comment at `audio.js:1020-1024`
describes. The rung that was added specifically because "WHEP has no renegotiation verb" is the
one rung that is not there. The host also pays for a transceiver that never reaches the wire, and
`docs/TELEMETRY.md` documents the mechanism as live.

Fix (one line, client side): when `audioFast && audioCaps.wantsFallbackTrack`, also
`pc.addTransceiver('audio', { direction: 'recvonly' })` so the answer has somewhere to put the
spare track. Verify by reading the answer SDP on a fast-path join: it must contain `m=audio`.

### 4 — the audio decode worker failing to start is permanent silence with no rung below it

`audio.js:935-970`, `audio.js:964`, `audio.js:1000-1005`, `audio.js:985-998`.

Three failures on the audio fast path exit without engaging the ladder:

```js
w.onerror = (e) => { note(); console.warn('[audio] decoder worker error:', e); };   // 964
...
if (!startWorker()) return;                                                        // 1005
if (!(await prepare())) { console.warn('[audio] cannot prepare WebCodecs path'); return; } // 1001-1004
```

`worker.onerror` fires for a script that fails to load or throws at top level — the exact case
`index.html:1442-1450` reasons about for the *video* worker and answers with `fallbackToVideo`.
Here it only logs. `dc.onmessage` is never installed (it is set at `audio.js:1008`, after these
returns), so every packet is dropped, `abandonWebCodecsAudio` is never called, `onAudioFastLost`
never runs, and the host — which was told `webcodecsOpus: true, worklet: true` and therefore put
no fed audio track in the answer — has nothing to switch to. Silent audio for the whole session,
visible only as `suppressed` ticking up.

Fix: call `abandonWebCodecsAudio()` from `w.onerror` and from both early returns in
`attachDataChannel`. Belt and braces: the worker's `init` handler already replies
`{type:'mode'}` (`audio-worker.js:374`) — arm a short deadline in `startWorker()` and abandon if
that reply never arrives, which also catches a worker that loads but wedges.

### 5 — two degradation bits latch from the pre-configure stats tick, so they are set on every session

`index.html:1497-1500`, `index.html:1334-1342`; `transform-worker.js:112`,
`transform-worker.js:159`, `transform-worker.js:597-602`.

```js
if (m.data.stats.decoderHardware === false) degrade(DEG.DECODER_HW_UNAVAILABLE);
if (m.data.stats.keyTier === 0)             degrade(DEG.KEYFRAME_REQUEST_IMPOSSIBLE);
```

`degrade()` is a latch (`degradeFlags |= bit`) with no matching `undegrade()`, and
`startVideoStatsPoll()` deliberately fires its first tick immediately ("A plain setInterval's
first tick is a full second out"). That tick is posted at `index.html:1516`, *before* the
transform is installed at `1519`/`1533`, so the worker answers from its initial state:
`keyTier = KEYREQ.none` (`transform-worker.js:112`) and `decoderHardware = false`
(`transform-worker.js:159`, only assigned inside `doConfigure`, which itself awaits the codec
hint for up to 600 ms).

So bit 11 (`no way to request a keyframe`) and bit 17 (`no hardware decoder`) are set on
essentially every session, including sessions that then report `keyTier: 1` and hardware decode
in the same 104-byte frame. Two of the twenty-two degradation bits are now noise, and they
contradict the tier bytes beside them — which is precisely the failure `docs/TELEMETRY.md`
("A degradation nobody can see is a degradation nobody fixes") exists to prevent.

Fix: make both a set/clear pair driven by the reported value, the way `FULLSCREEN_DENIED` and
`UNADJUSTED_MOVEMENT_UNAVAILABLE` already are —
`if (s.keyTier === 0) degrade(...); else undegrade(...);` — or gate the latch on the worker
having reported `configured: true`.

---

## Minor

### 6 — `SEAudio.resume()` is exported and never called; a suspended `AudioContext` is reported and never recovered

`audio.js:870-875` (exported at `audio.js:1118`), `index.html:842-851`. The base branch called it
once at join (`latency-overhaul:index.html:1532`); `beginFromGesture` now resumes synchronously
inside the gesture, which is better, but nothing resumes the context after the browser suspends it
on a background/foreground cycle — the case iOS and Android hit routinely. `degradeFlags` reports
`AUDIO_CONTEXT_SUSPENDED` (`audio.js:1104`) so the host can see it, and then nothing acts on it.
Fix: call `SEAudio.resume()` from `startLock`'s `visibilityHandler` alongside `enableNoSleep()`.

### 7 — the new MessagePort sample transport has no telemetry code and reports as the slow path

`audio.js:1075`, `index.html:1234-1241`, `docs/TELEMETRY.md` "Path codes". `getSyncInfo()`
distinguishes `'shared' | 'port' | 'postMessage'`, but `activeAudioPath()` collapses everything
that is not `'shared'` to `AUDIO_PATH.workletPost` (2). A device on the direct worker→worklet port
— the whole point of commit `9087343` — is indistinguishable on the host from one relaying every
5 ms packet through the main thread. `CLAUDE.md` states the rule: "a new tier should get a byte".
Fix: give the audio sink/transport its own tier byte in the v3 tail, or split path code 2.

### 8 — after a reconfigure the private-ring MessagePort is never re-offered, so the page relay returns for good

`audio.js:890-899`, `audio.js:915-919`, `audio.js:972-977`, `audio.js:793-860`. `offerRingPort()`
is called only from `attachWorklet()`, which runs once per page (`state.prepared` /
`preparePromise`). `stopWorker()` calls `releaseRingPort()`, which nulls `ringPortForWorker`. The
next session's worker therefore never gets a port, and samples go worker → page → worklet again,
200 times a second, on the main thread that also runs the reconfig fetch and every pointer event —
the exact cost the change was written to remove. Fix: call `offerRingPort()` from `startWorker()`
(it already no-ops when `state.sab` is set), not only from `attachWorklet()`.

### 9 — audio-sink tier 4 (`selectAudioOutput`) is unreachable

`audio.js:623-634`, exported at `audio.js:1136`. `pickOutput()` is never called from `index.html`
(grep: the only references are its definition and its export), so `sinkTier` can never take the
value `SINK_TIER.selectAudioOutput`. `docs/TELEMETRY.md` documents byte 75 = 4 as a live tier.
This is review 2 §D13, still open. Fix: either wire it to a gesture in the page or drop the tier
from the table.

### 10 — packet concealment is written into the ring synchronously while decoded audio arrives asynchronously

`audio-worker.js:243-265`, `audio-worker.js:324-337`, `audio-worker.js:133-197`. `concealPackets`
calls `writeShared`/`postSamples` inline from `handlePacket`, but real samples only reach the ring
from the `AudioDecoder` output callback, which is a separate task. When the worker is behind — the
only time concealment matters — the task order is `msg(N)`, `msg(N+2)`, `output(N)`, so the
concealment for the missing `N+1` lands in the ring *before* `N`. A 5 ms swap, audible as a
stutter rather than the smooth fill the code intends. `scripts/test-client-audio.mjs:36-42` hides
this: `FakeAudioDecoder.decode` calls `output` synchronously. Fix: emit concealment from the
output callback (track the outstanding decode count), or serialise both through one small
in-worker queue.

### 11 — `useCanvasInstead()` can throw out of a worker message handler and leave no sink at all

`index.html:1074-1080`, reached from `index.html:1081-1099` and `index.html:1482-1483`.
`freshCanvas().transferControlToOffscreen()` is unguarded; every other call of it
(`index.html:1512`) sits inside `start()`'s try. Here it runs in `worker.onmessage`, so a throw is
an uncaught exception, `showCanvas()` never runs, and the page shows black with the worker writing
into nothing. Only reachable under `?present=track`, which is why it is minor. Fix: wrap it and
fall through to `fallbackToVideo('no presentation sink')`.

### 12 — the `POINTER_MOVE_BATCH` tail flags byte is written by the client, documented as host-read, and discarded

`input.js:230-234`; host `input/protocol.rs:412-419`, `input/protocol.rs:81-87`;
`docs/INPUT-PROTOCOL.md:185`. The client writes `evFlags()` at `need + 8` and the host reads only
`b[need + STAMP_LEN + 1]` (the predicted count) — `parse_pointer_batch` never touches the flags
byte, although `evflag`'s doc comment and the protocol doc both say it applies to
`POINTER_MOVE_BATCH`. Harmless on the wire, but it is also semantically wrong on the client side:
`evFlags()` reports pointer-lock `unadjustedMovement`, which cannot mean anything for a batch of
absolute normalised coordinates. Fix: either read it host-side or write a constant 0 and correct
the two doc comments.

### 13 — `handleRenderCount` has no generation guard and can demote a session that has already been rebuilt

`index.html:1101-1136`. The function awaits `pc.getStats()`; a `reconfigure()` or
`retryWithLegacyTransform()` during that await tears the session down and starts a new one, and
the resumed call still increments the module-level `renderStalls` and can reach
`fallbackToVideo('webcodecs stalled')`. That silently pins the *new* session to the `<video>`
element for a stall that belonged to the old one. Every other async path in the page took a
generation token in this branch (`startGen` / `stale()`, `index.html:1018-1023`); this one did
not. Fix: capture `startGen` on entry and return if it moved.

### 14 — `audioCaps.redundancy` stays `true` on the Opus-track path

`index.html:2250-2259`, `audio.js:38`. `detectCapabilities()` returns `redundancy: true` as a
property of the build, and `docs/TELEMETRY.md` states "`index.html` overwrites it with `false` on
the Opus-track path, which has no DataChannel to carry a redundant body at all". It does so only
in `SEVideo.onAudioFastLost` (`index.html:1276-1279`); the join-time branch at `2250-2259` sets
`wantsFallbackTrack` but leaves `redundancy` true, so a client that failed the Opus probe still
tells the host it can split a body it will never receive. Inert today (the host only sets the flag
on the DataChannel), but it is a false capability claim and the doc disagrees with the code.
Fix: `audioCaps.redundancy = audioFast;` beside line 2258.

### 15 — `docs/INPUT-PROTOCOL.md` still describes the stats frame as v2/96 bytes

`docs/INPUT-PROTOCOL.md:231` — "96 bytes in v2, 72 in v1" — while the client now sends 104 bytes
at `STATS_VERSION = 3` (`input.js:419-420`) and `docs/TELEMETRY.md` documents v3 in full. A reader
who takes INPUT-PROTOCOL.md as the opcode authority will size the frame wrong. Fix: add the v3 row
and point at TELEMETRY.md.

---

## Verified correct

Traced and found sound — do not redo these.

**Host contract, byte for byte**

- `PONG`: host `build_pong` emits 19 bytes, caps `u16` LE at 17 (`input/protocol.rs:682-689`);
  client `noteHostCaps` requires `byteLength >= 19` and reads `getUint16(17, true)`, else 0
  (`input.js:521-523`). `onPong` requires `>= 17` for the host stamp at 9 (`input.js:571-573`).
  A 9-byte and a 17-byte PONG both degrade correctly; asserted by `test-client-stats.mjs:154-208`.
- Capability bits: client `HOST_CAP` (`input.js:507-513`) is bit-identical to `hostcap`
  (`input/protocol.rs:654-669`), including the reserved `UNRELIABLE_MOVE_CHANNEL` neither side uses.
- `KEYFRAME_REQUEST` `0x64`: 2 bytes on the reliable channel, gated on bit 0
  (`input.js:531-543`); host parses it (`input/protocol.rs:356-358`), counts it
  (`webrtc_session.rs:726-733`) and routes it through the one rate-limited requester
  (`webrtc_session.rs:735-741`). The worker's tier is revocable, not a latch
  (`transform-worker.js:318-326`), and `teardown()` withdraws it with `setHostCaps(0)`
  (`input.js:907`).
- Optional input tails, all five: `POINTER_*` stamp at 40 = `POINTER_LEN`
  (`input.js:203` / `protocol.rs:329`); `WHEEL` at 15 (`input.js:253` / `protocol.rs:336`);
  `MOUSE_DELTA` flags at 8 and stamp at 9, with `DELTA_LEN = 9` on the client and
  `MOUSE_DELTA_LEN = 8` + 1 on the host (`input.js:256-270` / `protocol.rs:448-462`);
  `KEY` stamp past the variable `code`/`key` strings (`input.js:284` / `protocol.rs:475`);
  `BATCH` stamp at `10 + n*24` and predicted count at +9 (`input.js:230-234` /
  `protocol.rs:412-419`). All gated on `INPUT_TIMESTAMP`, so the first second of every session is
  byte-identical v1.
- `STATS` v3: 104 bytes, offsets 0..96 identical to v2, the two new `u32`s at 96 and 100
  (`input.js:419-493`). Host `parse_client_stats` takes the tail only when all 96 bytes arrived
  and ignores 96..104 (`protocol.rs:589-643`) — the floor rule working, and documented as such in
  `docs/TELEMETRY.md`.
- `FLAG_REDUNDANT` body split: client `u16` LE primary length at 13, primary, then previous
  (`audio-worker.js:302-309`) matches `build_datachannel_message` /`split_payloads`
  (`audio/protocol.rs:54-103`) exactly, including the truncated-body rejection and the legal
  zero-length previous payload. Sequence handling is wrap-safe and de-duplicates
  (`audio-worker.js:311-337`), which also makes it repair reordering on the host's
  `ordered: false, max_retransmits: 0` channel (`webrtc_session.rs:1234-1240`).
- Host redundancy invariant: `prev_payload` is cleared on every producer-side skip — backpressure
  drop, broadcast lag, resubscribe (`webrtc_session.rs:1288-1300`, `1336-1341`, `1373-1379`) — so
  the trailing copy really is `seq - 1`.
- 13-byte audio header (`seq` u32, `capture_ns` u64, `flags` u8) read as two u32 halves with no
  `BigInt` (`audio-worker.js:26-29`), matching `audio/protocol.rs:22-37`.

**Control flow**

- `start()` generation token: `startGen` is bumped by `teardownSession()` and re-taken at
  `index.html:1395`, and `stale()` is checked after all seven suspension points including the WHEP
  fetch and `setRemoteDescription` (`index.html:1402-1724`), and again in the catch (`1729`).
  Review 1 §4 is fixed.
- The transform watchdog is armed from `iceconnectionstate`/`connectionstate` connected, not from
  the offer (`index.html:1553-1562`, `1626-1644`), and both handlers are guarded by
  `pc !== localPc`. Review 1 §3 fixed.
- `ensureConfigured()` memoises the in-flight configure and can no longer orphan a `VideoDecoder`
  (`transform-worker.js:555-564`). Review 1 §9 fixed.
- `inFlight` has three independent resyncs — `ondequeue` with an empty accept queue
  (`transform-worker.js:520-529`), a 500 ms no-output staleness check
  (`transform-worker.js:762-766`), and a reset to 0 on configure/reset — plus a decrement in the
  decode catch. It cannot wedge over the cap. Review 4 §R3 fixed.
- `PRESENT.noSink` is a third outcome, so a frame that reached no sink no longer clears the
  watchdog or blinds the stall detector (`transform-worker.js:255-288`, `457-506`), and
  `releaseTrackGenerator()` stops an orphaned writable from winning in `presentFrame`. Review 1 §5
  and §10 fixed. The `VideoFrame` timestamp is read before hand-off (`transform-worker.js:464`).
- `beforeRenderStart()` uses a signed 32-bit difference, so the ~13 h RTP wrap no longer drops
  every frame (`transform-worker.js:378-381`); `advanceRtpUnwrap` maintains the wrap count and
  `hostCaptureAbsMsOf` reads it without mutating (`transform-worker.js:350-372`). Review 1 §12 fixed.
- `waitingSince` is stamped in `startPump`, not left at 0, so the 8 s give-up is measured from the
  stream and not from page load (`transform-worker.js:620-625`, `715`). Review 1 §7-adjacent fix
  verified.
- The WHEP POST has a 10 s `AbortController` deadline with a distinguishable message
  (`index.html:1676-1693`), and the reconfigure path has its own 15 s deadline
  (`index.html:460-478`). Review 1 §8 fixed.
- `errorText()` never lets an empty `DOMException.message` leave the join screen dead
  (`index.html:329-345`). Review 1 §14 fixed.
- The `/reconfig` poll, `armProbe`, `probeTick`, `applyReconfig`, `kickDevice` and `linkLost`
  re-check `kicked || reconfiguring || !isLive` after every await; `teardownSession()` clears every
  timer it owns (`index.html:1025-1040`).

**Feature detection and fallbacks**

- Audio fast path: `detectCapabilities` → `AudioDecoder.isConfigSupported` → a real 3-byte Opus
  packet decoded and `copyTo`'d to `f32-planar` (`audio.js:60-133`). Only a produced-then-failed
  `copyTo` or an unconfigurable decoder demotes; a decode error is treated as inconclusive, with
  the per-packet `copyFailures` counter as the net (`audio-worker.js:152-166`). Review 2 §D5 fixed.
- `AudioWorkletNode.onprocessorerror` → `abandonWebCodecsAudio()` (`audio.js:829-835`) catches a
  processor constructor that throws on the render thread, which does not reject `new
  AudioWorkletNode`. Review 1 §13 fixed.
- The ring is validated independently on both sides and the two exchange a `mode` handshake, with
  a `mode-mismatch` reply that re-inits the writer without the buffer (`audio.js:808-822`,
  `audio-worker.js:355-375`, `audio-worklet.js:98-118`). Capacity is asserted to be a power of two
  in all three (`audio.js:923`, `audio-worker.js:74`, `audio-worklet.js:26`).
- `Atomics` usage on the SAB ring: absolute `uint32` positions, `& (cap-1)` indexing,
  `>>> 0` on every read, `| 0` on the store — the round trip through `Int32Array` is exact and the
  2^32-frame (~24.9 h) wrap is handled by unsigned differences (`audio-worker.js:90-109`,
  `audio-worklet.js:138-167`).
- The MessagePort handshake is offer → worklet ack → hand-off, and an engine that will not
  transfer a port into an `AudioWorkletGlobalScope` simply never acks, leaving the page relay
  unchanged (`audio.js:887-919`, `audio-worklet.js:119-128`). No side is left half-migrated.
- The worklet's silence-boundary correction now requires `avail > 0`, so a starved ring reaches
  `conceal()` / `underruns++` / the re-prime hysteresis instead of returning a square-edged silent
  quantum (`audio-worklet.js:240-246`). Review 1 §7 fixed, and it is what makes `adaptSafetyMargin`
  able to grow at all.
- `RTCRtpScriptTransform` → `createEncodedStreams` → `<video>` is a real ladder: the constructor
  throw and the watchdog both go to `retryWithLegacyTransform`, which itself falls to
  `fallbackToVideo` when there is no legacy path (`index.html:1048-1067`, `1517-1534`).
  `worker.onerror` covers a transform worker that loads but never posts (`index.html:1447-1450`).
- `setupCanvas` fails closed when `getContext('2d')` returns nothing: `renderTier` drops to none
  and a `configerror` is posted rather than counting every frame as rendered
  (`transform-worker.js:186-206`). Review 2 §D10 fixed.
- `sendKeyFrameRequest()` existing is not treated as it working: two failures drop to the
  out-of-band rung and a later success restores it (`transform-worker.js:672-695`). Review 2 §D8 fixed.
- Pointer lock: a throwing `requestPointerLock({unadjustedMovement:true})` retries plainly rather
  than losing the lock entirely, and the raw-movement claim is set on *fulfilment* only, withdrawn
  on `pointerlockchange` (`index.html:686-741`, `input.js:834-841`). Reviews 2 §D7/§D18 and 4 §R4 fixed.
- The `setSinkId` ladder falls through on rejection instead of aborting with `sinkTier` reporting a
  sink it did not set (`audio.js:574-616`).
- Wake Lock → `nosleep.js` shim, with `NoSleep` constructed lazily so a failed `/nosleep.js` fetch
  cannot take the page down (`index.html:593-607`, `810-832`).
- `performance.timeOrigin` has a computed fallback in `input.js:41-43` (Safari < 15);
  `TextEncoder` is constructed lazily inside `enc()` (`input.js:34-37`); `Blob.arrayBuffer` has a
  `FileReader` fallback (`input.js:324-332`); `window.isSecureContext` is type-checked before use
  (`index.html:2006-2008`). Review 3 defects 1, 3, 5 and 6 fixed. `index.html:1200-1204` reads
  `performance.timeOrigin` raw, but the only consumer is the rVFC path (Chrome 83+/Safari 15.4+)
  and a `NaN` there fails the `lag >= 0` test and drops the sample rather than poisoning the window.
- The mic-permission modal is not raised into a fullscreen subtree, and a fullscreen transition
  during its lifetime resolves it as skipped rather than leaving a promise pending forever
  (`index.html:2154-2189`). Review 4 §R8 fixed.
- `?present=track` is no longer undone by `ontrack` (`index.html:1574-1578`). Review 4 §R11 fixed.
- The measured refresh rate is folded into `joinData` before the offer, with a bounded 500 ms race
  and a one-shot later refinement (`index.html:1863-1896`, `2248-2260`). Review 4 §R6 fixed.
- `requestKeyframe()` arms its 200 ms limiter only when the send will happen
  (`input.js:531-543`); asserted by `test-client-stats.mjs:221-233`. Review 4 §R10 fixed.

**server.rs**

- The ETag is FNV-1a over each asset's own `include_str!` bytes, folded in a `const fn`, so an
  edit to any client file invalidates that file's cache and only that file
  (`server.rs:509-560`). `If-None-Match` is split on `,` and trimmed, and the 304 repeats the ETag
  and `Cache-Control: no-cache`. `index` stays uncached because of the `__SAME_DEVICE_FLAG__`
  substitution, and keeps its COOP `same-origin` + COEP `require-corp` headers
  (`server.rs:483-497`) — the assets need no `Cross-Origin-Resource-Policy` header because
  same-origin responses pass the CORP check by default.

**Test scripts**

- `test-client-ice.mjs` lifts the three ICE helpers out of `index.html` by name and asserts the
  full grace table, listener cleanup, the already-complete short circuit and the deadline bit. It
  does not (and by construction cannot) assert finding 1, because it never exercises a LAN-shaped
  ICE list.
- `test-client-audio.mjs` covers the redundancy split end to end, including the discard-on-no-loss
  case that would otherwise double the audio, the duplicate, the late packet, the discontinuity
  and the truncated body. Its `FakeAudioDecoder` is synchronous, which is what hides finding 10.
- `test-client-stats.mjs` asserts the 104-byte v3 frame field by field, the 72/96-byte floors, the
  clock midpoint arithmetic, the capability gate, and every optional input tail length. All three
  assertions match the Rust parsers I checked them against.

# Manual test: the client web page

Nothing in `src-tauri/src/streamer/static/` can be exercised on either development machine.
The Windows box is the host, so a browser on it is the same-device case; the Mac is the 10.15
floor and its Safari is 13.1, below every fast path. Every tier in
`docs/CAPABILITY-TIERS.md` whose subsystem starts with "Client" is therefore `UNVERIFIED`, and
this is the script that closes them.

It needs a phone in your hand and about forty minutes. Work through §1 once per device; §2-§8
are the individual claims, each of which names the field that proves it. §9 is the catalogue of
every fallback and the one line that proves which rung is live — read it before §1 if you only
have time for one section, because most of what can go wrong here is a fast path that failed
*quietly* and looked identical to one that worked.

## What changed, and therefore what most needs a device in front of it

Everything below is new or newly conditional. If your time is short, test in this order.

1. **Keyframe requests.** The page prefers `RTCRtpScriptTransform` (which has
   `sendKeyFrameRequest`) over the legacy `createEncodedStreams`, and where neither can ask it
   relays the request to the host as `KEYFRAME_REQUEST` (`0x64`) — but only to a host that has
   said on the `PONG` that it parses it. Highest-risk device: **Chrome/Edge/Samsung Internet
   below 141**, which has no `sendKeyFrameRequest` at all. §2.
2. **The join chain.** The microphone prompt no longer gates ICE; ICE waits for a relay
   candidate only when the session is actually relayed; the WHEP POST has a deadline; the
   transform watchdog is armed when packets start rather than when the page loads. Highest-risk
   devices: **a cross-network join through TURN** and **any slow phone**. §3.
3. **Audio.** The WebCodecs path is claimed only after a real Opus frame has been decoded *and*
   `copyTo`'d, and it can now step down mid-session. Highest-risk devices: **Safari 26 and
   Firefox 130**, the two engines that only just shipped `AudioDecoder` and so are the ones the
   old constructor sniff got wrong. §5.
4. **Input.** Coordinates come from the letterboxed picture rect, pointer lock survives an
   options bag that throws, and the keyboard lock is requested after fullscreen is re-entered.
   Highest-risk device: **a phone in portrait against a 16:9 host**, where the letterbox is
   largest. §6.
5. **Presentation.** Frames now count as rendered only when something painted them. A black
   canvas that used to satisfy every watchdog now demotes. Highest-risk device: **anything with
   an unusual GPU or a blocked OffscreenCanvas**. §4, §9.

A tester-facing side effect worth knowing: each static asset's `ETag` is now a hash of its own
bytes rather than the crate version, so editing `index.html` or a worker and restarting the host
is enough for the device to pick it up. A hard reload is no longer part of the loop, and a
device that *does* keep serving stale JavaScript is now a real finding.

## 0. Setup, and the one console command you need

Start the host with telemetry on:

```
ScreenExtend serve -v
```

Join from the device under test, then open its browser console and run:

```js
SEVideo.getStats()
```

Everything below is a field of that object, or of `SEAudio.getSyncInfo()`. On iOS Safari the
console is on a Mac: Safari → Develop → *your iPhone* → the page. On Android Chrome it is
`chrome://inspect` from a desktop Chrome over USB. Where you cannot attach a console at all
(Samsung Internet on a device with no debugging), read the same values on the **host** instead:
they ride the once-a-second stats frame, and `ScreenExtend status --json` prints them.

Two numbers matter more than the rest and should be checked on every device:

- `videoPath` — 1 = WebCodecs into a canvas (the fast path), 2 = WebCodecs into a `<video>`
  element, 3 = the plain `<video>` element (**the slow path**; libwebrtc's whole receive path
  is back, about 90 ms of it), 0 = nothing is playing.
- `degraded` — the degradation bitfield. `0` is a device with nothing to report. Decode it
  against the table in `docs/TELEMETRY.md`; `degraded.toString(2)` is the quickest way to read
  it.

Also useful: `?debug` in the URL turns on the per-second `inbound-rtp` console dump, and
`?present=track` selects the experimental presentation tier (§4).

## 1. Per-device sweep

For each device: join, confirm the picture and the mouse work, then record the row.

| # | Device / browser | Expect `videoPath` | Expect `transportTier` | Expect `keyTier` | Expect `audioPath` | Notes to check |
|---|---|---|---|---|---|---|
| 1 | Chrome desktop (Windows or macOS), current | 1 | **1** | **1** | 1 | The single most important row: before this change every Chrome client took transport tier 2 and `keyTier` 0. If `transportTier` is 2 here, `RTCRtpScriptTransform` is missing or the 2.5 s watchdog fired — check `degraded` bit 19. |
| 2 | Edge desktop, current | 1 | 1 | 1 | 1 | Same engine; the row exists because Edge's WebRTC field-trial set differs. |
| 3 | Chrome desktop, an older build (≤ 140 if you can get one) | 1 | **2** | **2** against a host that parses `0x64`, else **0** | 1 | The legacy-transform path. `keyTier` is 2 only once the host has advertised capability bit 0 on the `PONG`; against a host without the arm it is 0 by design, and the `<video>` demotion stays armed — see §2. |
| 4 | Chrome on Android | 1 | 1 | 1 | 1 or 2 | `audioPath` 2 (postMessage ring) is expected unless the page is HTTPS: Android WebView has no `SharedArrayBuffer`. Check `sab` and `crossOriginIsolated`. |
| 5 | Safari, macOS 15 or later | 1 | 1 | 1 | 1 on Safari 26, else 3 | `audioPath` 3 on Safari 25 and below is correct, not a bug: `AudioDecoder` shipped in Safari 26. |
| 6 | Safari, iOS/iPadOS 18 | 1 | 1 | 1 | 3 | Expect `degraded` bit 6 (no Opus decode) and, on 18.4+, *not* bit 8 (`outputLatency` arrived in 18.4). |
| 7 | Safari, iOS/iPadOS 17 | 1 | 1 | 1 | 3 | Expect bits 6 and 8. Fullscreen: on **iPad** the stage goes fullscreen; on **iPhone** it cannot, so expect `fullscreen: false` and bit 0, with the picture filling the viewport instead. That is the designed behaviour, not a failure. |
| 8 | Safari, iOS/iPadOS 16 | 1 on 16.4+, else **3** | 1 | 1 | 3 | 16.4 is the floor for `VideoDecoder` and `transferControlToOffscreen`. On 16.0-16.3 expect `videoPath` 3 and bit 13 — and confirm the picture still moves and the touch still lands. |
| 9 | Safari, iOS 15 | **3** | — | — | 3 | `RTCRtpScriptTransform` exists (15.4) but WebCodecs does not, so this is the `<video>` fallback by design. The row is here to prove the fallback is *usable*, not to prove the fast path. |
| 10 | Firefox desktop, current | 1 | 1 | 1 | 1 on 130+ | Firefox is the one engine with `selectAudioOutput()`: expect **no microphone prompt at all**, and `sinkTier` 1 after the host changes the output. |
| 11 | Samsung Internet, current | 1 | 1 or 2 | 1 or 2 | 1 or 2 | Chromium-based but its version trails; record what you get rather than expecting a value. |
| 12 | An old iOS 12 or 13 device | **3** | — | — | 3 | The parse-safety row. What is being tested is that the page **loads at all**: the join form renders, the OTP boxes accept input, Connect works. A syntax error anywhere in `index.html`, `input.js` or `audio.js` would show as a blank or half-rendered page. If the console is reachable, confirm zero uncaught `SyntaxError`. |

Row 12 is the one that cannot be replaced by reasoning: the whole client page is one shared
scope, so a single unsupported token anywhere in it takes the join form down with it, and no
static check catches a construct nobody thought to add to the list.

## 2. Keyframe requests actually reach the host

The change this workstream exists for. On row 1 (Chrome desktop):

1. `SEVideo.getStats().keyTier` should be **1**.
2. On the host, watch `ScreenExtend serve -v` for `RTCP PLI received, requesting IDR`.
3. Provoke a loss: on the host, briefly saturate the link (a large file copy over the same
   Wi-Fi), or disable and re-enable the client's Wi-Fi for about a second.
4. **Expected:** the picture recovers within roughly one round trip, and `keyRequests` in
   `getStats()` has gone up. `videoPath` stays 1.
5. **The old behaviour, which must not reappear:** the picture freezes for about four seconds
   and then `videoPath` becomes 3 for the rest of the session.

On row 3 (Chrome ≤ 140) the answer depends on the host, and the point of the row is that the
client tells the truth about which one it is talking to:

- **Host without the `0x64` arm** (`findings/major-host-keyframe-request-opcode-unhandled.md`
  not yet landed): `SEVideo.getStats().keyTier` must be **0**, `keyRequests` must stay flat —
  the client sends nothing it knows will be dropped — and `degraded` bit 11
  (`KEYFRAME_REQUEST_IMPOSSIBLE`) must be set. Provoke a loss and confirm the picture demotes
  to `videoPath` 3 within about four seconds rather than sitting frozen for eight.
- **Host with the arm**: `keyTier` is **2**, `keyRequests` climbs, and the picture recovers
  within roughly one round trip without demoting. Re-run this row after the host side lands;
  a `keyTier` of 2 against a host that does not parse `0x64` is the exact bug this gate exists
  to prevent.

`node scripts/test-client-stats.mjs` asserts the gate itself (17-byte `PONG` → no capability,
no `0x64` on the wire; 19-byte `PONG` with bit 0 → the request goes out), so the manual run only
has to confirm the two end-to-end behaviours above.

## 3. Join time

Time from pressing Connect to the first frame, with a stopwatch, three runs, take the median.

| Case | What to check |
|---|---|
| First join on a device that has never granted the microphone | **There must be no microphone prompt before the picture.** If one appears at all it should be after the stream is live. This was a 12-second cap in front of ICE. |
| Rejoin on a LAN with no TURN configured | ICE should not cost seconds. `degraded` bit 15 set means gathering ran to its deadline rather than settling — on a LAN that points at a network problem, not at this code. |
| Join from a 90, 120 or 144 Hz device | **The session must not renegotiate about a second after it goes live.** Watch the host log for a second `creating virtual display` for the same client. The measured rate should already be in the first join: check the host's device panel shows the right refresh rate immediately. |
| Join with TURN configured, from another network | Gathering waits for a relay candidate plus a 400 ms grace. Expect this to be the slowest case and note the number. **It must connect at all** — before this pass the relay flag was never passed to the wait, so a cross-network join shipped an offer with no relay candidate and could only ever end in "Connection failed." If it fails, check the offer in the console for an `a=candidate:` line with `typ relay`. |
| Join against a host that accepts the TCP connection and never answers | Simulate it by suspending the host process (`Ctrl+Break` in its console, or the debugger) between pressing Connect and the answer. The page must reach the normal **"Disconnected"** state within about ten seconds, not sit on a spinner forever. On the *reconfigure* path (change the scale from the host's device panel while suspended) the same applies within fifteen: a frozen frame with no spinner and no status was the old behaviour. |
| Join twice quickly, or force a watchdog while joining | Reload the page during the join, or let the 2.5 s transform watchdog fire on a slow link. Watch the host log: there must be **one** `POST /whep` per join attempt that survives. Two peer connections for one device — a second `creating virtual display` line with no intervening leave — means the generation guard on `start()` is not holding. |

## 4. Presentation tier A/B (optional, and the only thing here that is a genuine experiment)

The default is the OffscreenCanvas in the worker. `?present=track` routes decoded frames
through a `VideoTrackGenerator` into the `<video>` element instead, which *may* win a hardware
overlay on some platforms. No public measurement says it does, which is exactly why this is a
manual A/B rather than a default.

Run on Chrome desktop and on Safari 18 (the only two engines with a track generator, under
different names):

1. Join normally. Record `hostToPaint.p50` and `.p99` from `getStats()` after 60 s of moving
   content on the host.
2. Rejoin with `?present=track` appended to the URL. Confirm `renderTier` is **2**; if it is 1
   the generator was unavailable and the page fell back, which is itself the expected
   behaviour everywhere else.
3. Record the same two numbers.
4. If the p99 improves by more than a few milliseconds on both engines, say so in
   `docs/OPTIMIZATIONS.md` row 30 and the default can change. If it does not, record that too
   — a negative result here retires the idea.

The photographic version, if you want a real number: point a 240 fps camera at the host screen
and the client screen together, display a millisecond counter on the host, and difference the
two readings.

## 5. Audio

With the per-device Audio toggle on in the host's device panel, and something playing on the
host.

| Check | How | Expected |
|---|---|---|
| Lip sync | Play a video with visible speech | Mouth and sound agree. `SEAudio.getSyncInfo().residualOffsetMs` near 0 means the sync target was achievable; a persistent value above 30 ms means it was clamped and something upstream is wrong. |
| Output latency source | `SEAudio.getSyncInfo().outputLatencySource` | `measured` on Chrome 102+/Firefox 70+/Safari 18.4+, `estimated` below that. `unavailable` on a browser with `getOutputTimestamp` would be a bug. |
| **Bluetooth** | Pair Bluetooth headphones, then reconnect the session | Audio must still line up. This is the case the estimator exists for: before it, a Bluetooth sink played 100-200 ms early. Check `outputLatencyMs` is now in the hundreds, not 0. |
| Packet loss | Saturate the Wi-Fi while audio plays | A brief roughness, not a gap of silence. `getSyncInfo().concealed` should climb. `underruns` climbing while `concealed` stays 0 means concealment is not engaging. |
| **Packet redundancy** | Induce 2 % and then 5 % uniform loss (`clumsy` on Windows, `netem` on Linux) with audio playing, and read `SEAudio.getSyncInfo()` | Above the host's loss threshold, `redundantUsed` climbs and `concealed` stays far lower than it does without redundancy — a single lost packet should be inaudible and only bursts of two or more audible. `redundantDiscarded` climbing with `redundantUsed` at zero is payload spent for nothing and points at the host's threshold. **Listen for doubled audio**: a copy played when its own frame also arrived would emit the same 5 ms twice, which is the failure the sequence de-duplication exists to prevent. |
| Sink selection | Change the device's audio output from the host's device panel | The sound moves. `sinkTier` says which API did it; 0 after a successful change is a bug. |
| iOS resume | On iPhone, join and let the screen lock and unlock | Audio resumes. `getSyncInfo().contextState` should be `running`, never `suspended`. |
| Safari before 26 | `audioPath` | 3, and audio still plays over the browser's own track. Silence here is the failure this workstream's `isConfigSupported` probe exists to prevent. |

## 6. Input

| Check | How | Expected |
|---|---|---|
| Letterbox coordinates | Use a device whose aspect ratio differs from the host display (a phone in portrait against a 16:9 desktop). Tap a specific icon near the top of the picture. | The click lands on that icon. Before this change it landed high by half the bar. Also tap **inside the black bar**: nothing should happen on the host. |
| Coalesced samples | Drag slowly in a drawing app on the host, from a touchscreen | A smooth line, not a polyline of straight segments. `degraded` bit 14 means the browser has no `getCoalescedEvents` and one sample per frame is expected. |
| Pointer lock, raw movement | On a laptop client, enter fullscreen and move the mouse across the whole host desktop | The cursor should feel like the host's own, not double-accelerated. `pointerLockUnadjusted: true` in `getStats()` confirms raw deltas; `degraded` bit 2 means the platform refused and the two curves are compounding. |
| Keyboard lock | In fullscreen on Chrome, press Escape | The key reaches the host rather than leaving fullscreen. `keyboardLock: false` is expected on Safari and Firefox, which have no such API. |
| Fullscreen on iPhone | Join with fullscreen mode on | `fullscreen: false` and `degraded` bit 0, and the picture fills the viewport. iPhone Safari cannot make an element fullscreen at all; this is designed behaviour. |

## 7. Session survival

| Check | How | Expected |
|---|---|---|
| Wake lock | Leave the client untouched for longer than its screen timeout | The screen stays on. `wakeLock: true`. `degraded` bit 3 means the Wake Lock API is missing and `nosleep.js` is carrying it, which is normal below iOS 18.4. |
| No audible nosleep | On a device on bit 3, listen carefully with the stream's own audio muted | Silence. `nosleep.js` plays a looping clip and it is now muted; if you hear anything, that is a regression. |
| Backgrounding | Switch apps and come back | The picture resumes, the wake lock is re-acquired, and the session is not dropped. |
| Missing `nosleep.js` | In devtools, block `/nosleep.js`, then reload | The join form still renders and Connect still works. This used to be a top-level `new NoSleep()` that took the whole script down. |

## 8. Compatibility copy

| Check | Expected |
|---|---|
| Join over **http://** from a phone | The HTTPS modal appears. Its text should not have "sucre" in it or a raw `-->`. |
| Join over **https://** from an iPhone on iOS 17 | The limited-support modal lists **WebCodecs audio** and does *not* offer HTTPS as the fix — the page is already secure and that Safari has never had an `AudioDecoder`. The description should say audio uses a standard track and the picture is unaffected. |
| Join over https:// from Chrome desktop | No modal at all. |

## 9. The fallback catalogue

Every fast path on this page has a working floor under it, and every step down is reported. This
is the table to read when something "works but feels wrong": find the row, read the field, and
you know which rung you are on without guessing. **A blank field where a number should be is
itself the answer** — it means the page never got far enough to measure.

How to force each one, so you can see the floor rather than trust it:

| Fast path | Floor | How to force the floor | What you should see | The line that proves it |
|---|---|---|---|---|
| **WebCodecs decode → canvas** | the `<video>` element | Join from Safari on iOS 15 (no `VideoDecoder`), or block `/transform-worker.js` in devtools | Picture still moves, mouse still lands, latency visibly worse — libwebrtc's receive path is back, roughly 90 ms of it | `videoPath` **3**; `degraded` bit 13 (`DEMOTED_TO_VIDEO`). Blocking the worker should demote **immediately**, not after a five-second black screen — that is the worker `onerror` this pass added |
| `RTCRtpScriptTransform` | `createEncodedStreams` | Chrome or Edge below 141 | No visible difference until a packet is lost, then §2 | `transportTier` **2**. If a *current* Chrome reads 2, the 2.5 s watchdog fired: `degraded` bit 19 |
| Either transform | no transform at all | Safari below 15.4 | `<video>` element | `transportTier` **0** with `videoPath` 3 |
| `sendKeyFrameRequest()` | `0x64` on the input channel, then `<video>` | Chrome ≤ 140 (§2) | Recovery within a round trip *if* the host parses `0x64`; otherwise a four-second freeze and a demotion, which is the honest outcome | `keyTier` **1** / **2** / **0**; bit 11 when 0 |
| **OffscreenCanvas in the worker** | a canvas drawn on the page, then `<video>` | Block `transferControlToOffscreen` (`Object.defineProperty(HTMLCanvasElement.prototype,'transferControlToOffscreen',{value:undefined})` before joining) | Picture still paints | `renderTier` **1** normally, **3** on the element; `degraded` bit 12 |
| Track generator (`?present=track`) | the canvas | Any engine without `VideoTrackGenerator`/`MediaStreamTrackGenerator` | The canvas, silently — this is the expected result nearly everywhere | `renderTier` **1** instead of **2** |
| **`SharedArrayBuffer` audio ring** | the same ring, private, samples posted worker → page → worklet | Join over **http://**, or from Android Chrome without the isolation headers | Audio plays. On a phone under load expect more dropouts: this path crosses the main thread 200 times a second | `audioPath` **2** (fast path is **1**); `sab` and `crossOriginIsolated` both false in `getStats()`. `SEAudio.getSyncInfo().transport` is `shared`, `port` or `postMessage`: `port` means the samples go worker → worklet over a transferred `MessagePort` and the main thread is out of the path, `postMessage` means the page is relaying every packet. Either plays; `port` is the one to expect on a browser that transfers a port into an `AudioWorkletGlobalScope`. The shared/private choice is also handshaken now — a disagreement used to be permanent silence with both halves believing they worked |
| **WebCodecs Opus decode** | the browser's own Opus track (NetEQ) | Any Safari below 26; or stub `window.AudioDecoder = undefined` in the console before pressing Connect | Audio plays over a normal WebRTC audio track, sync a little looser | `audioPath` **3**; `degraded` bit 6. **Silence here is the failure this pass exists to prevent** — the page now decodes a real Opus frame and `copyTo`s it before claiming the fast path |
| WebCodecs Opus decode, failing *mid-session* | same, without a rejoin | Hard to force by hand; if it happens, `SEAudio.getSyncInfo().decoderFailed` is true | Audio drops out for one renegotiation and comes back | `audioPath` moves 1 → 3 during the session, bit 6 sets. Audio that never comes back is a real defect |
| `AudioContext.setSinkId` | `HTMLMediaElement.setSinkId` → `navigator.audioSession` → nothing | Change the output from the host panel on iOS (no `setSinkId` below 18.4) and on Firefox | The sound moves, or the output list is empty and the host's panel says so | `sinkTier` **1**/**2**/**3**/**4**, `setSinkId` flag in `getStats()`. `sinkTier` **0** after a change the user asked for is a bug; an empty output list on iOS 17 is not |
| Output-latency reporting | estimate from `getOutputTimestamp()` | Chrome below 102, Safari below 18.4 | Sync still holds, including on Bluetooth | `getSyncInfo().outputLatencySource` = `estimated`; `degraded` bit 8 |
| **Screen Wake Lock API** | `nosleep.js` | Any iOS below 18.4, or deny the permission | The screen stays on either way, and the shim is **silent** | `wakeLock` true with `degraded` bit 3 set = on the shim. Bit 4 = the request was refused outright, and then the screen *will* sleep — that is the one case to report rather than accept |
| Pointer lock with `unadjustedMovement` | plain pointer lock → absolute coordinates | macOS Safari, or any engine that rejects the options bag | The cursor works but feels doubly accelerated | `pointerLock` true with `degraded` bit 2 = two pointer curves compounding. `pointerLock` **false** with bit 1 = the lock was refused and every move is going down the absolute path — usable, but a different feel |
| Keyboard Lock | the browser keeps Escape and the platform shortcuts | Safari, Firefox, or any non-fullscreen session | Escape leaves fullscreen instead of reaching the host | `keyboardLock` false; `degraded` bit 21 |
| Fullscreen | the picture fills the viewport, no fullscreen element | iPhone Safari (which cannot make an element fullscreen at all) | The stage fills the screen, browser chrome stays | `fullscreen` false with `degraded` bit 0 |
| `getCoalescedEvents` | one sample per frame | Older Safari | A drawn line becomes a polyline of straight segments | `degraded` bit 14 |
| Clock offset from `PONG` | offset 0, so host-relative numbers are a difference and not an absolute | An older host that sends a 9- or 17-byte `PONG`; or Safari below 15, where `performance.timeOrigin` is missing | Everything works; `hostToPaint` reads 0 | `clockValid` false. `hostToPaint.p50` of 0 **with** `clockValid` true would be the bug |

Two rules that apply to the whole table:

- **A degradation bit set is not a failure**, it is the page telling the truth about a device.
  What is a failure is a bit that *should* be set and is not — that is the class of bug this
  whole pass was about, where a fast path was claimed on a device that could not run it.
- Read `degraded` as binary (`SEVideo.getStats().degraded.toString(2)`) and decode it against
  the table in `docs/TELEMETRY.md`. Bits are latched for the session and never cleared, so a
  bit set once means "this happened", not "this is happening now".

## Recording the result

Fill in the `Status` column of the "Client …" rows in `docs/CAPABILITY-TIERS.md` with
`VERIFIED (<device>, <browser version>)` for each tier you actually saw live, and put the
`hostToPaint` percentiles from §4 into `docs/OPTIMIZATIONS.md` rows 28-37. A tier you did not
reach stays `UNVERIFIED` — an untested tier and a working one must not look the same in the
ledger, which is the whole reason the telemetry exists.

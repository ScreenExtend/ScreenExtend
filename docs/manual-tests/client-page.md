# Manual test: the client web page

Nothing in `src-tauri/src/streamer/static/` can be exercised on either development machine.
The Windows box is the host, so a browser on it is the same-device case; the Mac is the 10.15
floor and its Safari is 13.1, below every fast path. Every tier in
`docs/CAPABILITY-TIERS.md` whose subsystem starts with "Client" is therefore `UNVERIFIED`, and
this is the script that closes them.

It needs a phone in your hand and about forty minutes. Work through §1 once per device; §2-§8
are the individual claims, each of which names the field that proves it.

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
| Join with TURN configured, from another network | Gathering waits for a relay candidate plus a 400 ms grace. Expect this to be the slowest case and note the number. |

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

## Recording the result

Fill in the `Status` column of the "Client …" rows in `docs/CAPABILITY-TIERS.md` with
`VERIFIED (<device>, <browser version>)` for each tier you actually saw live, and put the
`hostToPaint` percentiles from §4 into `docs/OPTIMIZATIONS.md` rows 28-37. A tier you did not
reach stays `UNVERIFIED` — an untested tier and a working one must not look the same in the
ledger, which is the whole reason the telemetry exists.

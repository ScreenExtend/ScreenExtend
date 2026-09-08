# Client page review 6 — regressions, old-browser compatibility, no-build constraint

**Scope.** `git diff latency-overhaul...HEAD` restricted to `src-tauri/src/streamer/static/`,
`src-tauri/src/streamer/server.rs` and `scripts/test-client-*.mjs`, in the worktree
`.claude/worktrees/agent-a8dc3a207f40d3925`. Merge base `d83808d`. 3,323 changed lines across
`index.html`, `audio.js`, `audio-worker.js`, `audio-worklet.js`, `transform-worker.js`,
`input.js`, `styles.css`, `server.rs`, and three Node test scripts. Read-only: nothing was
built, nothing was run in a browser. The three `scripts/test-client-*.mjs` harnesses *were*
executed under Node (they are pure `node:fs`/`node:vm` and only read the page) — all 58
assertions pass.

Both sides were read: `git show latency-overhaul:<path>` for the pre-change behavior and the
full post-change files. The four earlier reviews (`client-review-{1,2,3,4}`) were read and
**verified against HEAD, not trusted**: essentially every defect they name has since been fixed
on this branch (`needRelay` is now threaded into `waitForIceGathering`, `start()` has a
generation token, the WHEP POST has a 10 s `AbortController` deadline, `ensureConfigured()` is
memoised, `framesUnpainted` is counted separately, `renderFromTs` is compared through a signed
32-bit difference, the ring ABI throws now reach `onprocessorerror`, `errorText()` guards an
empty `DOMException`, the picture rect is pushed from `presentFrame`, `permModalIfVisible`
watches for a fullscreen transition, `requestKeyframe()` arms its window only when it will send,
`?present=track` is no longer undone by `ontrack`, drag/drop uses `normX`/`normY`, `inFlight`
has two resync paths, coalesced pointer-lock deltas fall back to the dispatched event,
`setupCanvas` checks its 2D context, the `setSinkId` ladder falls through, `performance.timeOrigin`
is computed the long way in `input.js`, the transform worker has an `onerror`, file drop has a
`FileReader` fallback, the `AudioContext` is closed when `prepare()` fails, `TextEncoder` is
lazy, and `window.isSecureContext` is guarded). The findings below are what remains, plus what
this diff newly introduces.

**Counts: 0 blocker, 2 major, 9 minor.**

---

## Syntax-compatibility verdict (the hard constraint)

Every changed script was swept for the constructs listed in the brief. **No file introduces
syntax that an older parser rejects.** Verdict per file:

| File | Parse floor | New syntax added by this diff | Verdict |
|---|---|---|---|
| `index.html` (inline script) | ES2017 (`async`/`await`, default params, one trailing comma in a call) | none newer than the file already used | **PASS** |
| `audio.js` | ES2017 (`async`/`await`, shorthand methods) | none | **PASS** |
| `audio-worker.js` | ES2017 | **improved** — `getBigUint64`/`1n` replaced with a two-`u32` `readU64()`; the file no longer needs BigInt (Safari 14 / Chrome 67) | **PASS** |
| `audio-worklet.js` | ES2015 class + `Atomics` (inherent to `AudioWorkletProcessor`) | none — `this.X = …` in the constructor, not class fields | **PASS** |
| `transform-worker.js` | ES2017; `'use strict'` newly added | `'use strict'` (behavior, not syntax); `[...].concat()` replaced a spread — a compat *improvement* | **PASS** |
| `input.js` | ES2017 | **improved** — `setBigUint64`/`BigInt()` replaced with `setU64`/`getU64`; `performance.timeOrigin` now has a `Date.now() - performance.now()` fallback | **PASS** |
| `styles.css` + the inline `<style>` | — | **improved** — every `inset: 0` expanded to `top/right/bottom/left` and `-webkit-backdrop-filter` added | **PASS** |

Specifically checked and **absent everywhere**: `?.`, `??`, `??=`/`||=`/`&&=`, class fields,
`#private`, top-level `await`, `for await`, numeric separators, regex lookbehind or named
groups, BigInt literals, optional catch binding (`catch {`), object/array spread,
`Array.prototype.at`, `Object.fromEntries`, `Promise.allSettled`/`any`, `globalThis`,
`structuredClone`, `String.prototype.replaceAll`, `queueMicrotask`, `**`, generators,
`import()`, `new.target`.

One **trailing comma in a call argument list** is new (`index.html:2187`, the
`showPermModal(a, b,)` call). Trailing commas in calls are ES2017 (Safari 10, Chrome 58,
Firefox 52) and `transform-worker.js:802` already had one before this branch, so the parse
floor does not move. Noted, not counted as a regression.

Every **runtime** API this diff newly reaches for is guarded before use, in the same file:
`navigator.wakeLock` (`index.html:609`), `AbortController` (`typeof … === 'function'`),
`MessageChannel` in `audio.js`, `VideoTrackGenerator`/`MediaStreamTrackGenerator` (`typeof`),
`AudioDecoder.isConfigSupported` / `VideoDecoder.isConfigSupported` (`typeof`),
`'ondequeue' in decoder`, `AudioContext.getOutputTimestamp` (`typeof`),
`navigator.mediaDevices.selectAudioOutput` (`typeof`), `matchMedia('(any-pointer: fine)')`
(inside `window.matchMedia &&` and a `try`), `Blob.prototype.arrayBuffer` (`typeof`, with a
`FileReader` fallback), `requestPointerLock({unadjustedMovement})` (inside its own `try`, with a
plain retry), `getCoalescedEvents` (feature-tested once at module scope), `window.isSecureContext`
(`typeof … === 'boolean'`). The worklet and the two workers were checked as independent parses.

**No build step, no dependencies.** Nothing is imported from a CDN or from npm in any served
file. No new routes were added; `server.rs`'s route table is byte-identical. The three
`scripts/test-client-*.mjs` files import only `node:fs` and `node:vm`, are never served, and are
not wired into `package.json` — they must be run by hand.

---

## Defects

### 1 — major — the transform-start watchdog can lose the race to the 5 s full demotion
`src-tauri/src/streamer/static/index.html:1554` (`armTransformWatchdog`),
`index.html:1389` (`TRANSFORM_START_TIMEOUT_MS = 2500`), `index.html:1603` (the 5 s
`fallbackToVideo` watchdog), `index.html:180` (`useScriptTransformNow`).

**Pre:** `useScriptTransform = hasScriptTransform && !hasLegacyEncodedStreams` — a browser that
had both (Chrome/Edge, which has had `createEncodedStreams` since 86 and
`RTCRtpScriptTransform` since 141) always took the **legacy** path. There was no
transform-start watchdog and no way for this failure mode to occur.

**Post:** `RTCRtpScriptTransform` is preferred wherever it exists, so Chrome 141+ — the largest
client population — moves to a path that has never carried a frame on this branch. The safety
net is a 2500 ms watchdog that demotes one rung (to `createEncodedStreams`) instead of all the
way to `<video>`. But the two watchdogs are started from different origins:

- `watchdog` (5000 ms → `fallbackToVideo`) is armed inside `pc.ontrack`, i.e. during
  `setRemoteDescription`.
- `transformWatchdog` (2500 ms → `retryWithLegacyTransform`) is armed only once ICE reports
  `connected`/`completed`.

Any join where ICE takes longer than 2.5 s — a cross-network/TURN session, a phone on a slow
Wi-Fi association, a host still finishing `DISPLAY_CORRELATION_LOCK` — makes the 5 s watchdog
fire first. The session is then demoted to the `<video>` element (`usingFallback = true`, which
is permanent for the session) and both the WebCodecs jitter-buffer saving and the keyframe
request are lost, when one rung down would have kept them. This is exactly the cross-network
case the comment above `armTransformWatchdog` says it exists to protect.

**Affected:** Chrome/Edge 141+ on any join whose ICE connect exceeds ~2.5 s — cross-network
(cloud relay) joins in particular.

**Fix:** make the demotion ladder ordered rather than racing. Either (a) arm the 5 s
`fallbackToVideo` watchdog from the same event that arms `armTransformWatchdog` instead of from
`ontrack`, or (b) have the 5 s watchdog call `retryWithLegacyTransform('no frame within 5 s')`
rather than `fallbackToVideo` whenever `webcodecs && useScriptTransform && hasLegacyEncodedStreams
&& !transformStarted` — `retryWithLegacyTransform` already falls through to `fallbackToVideo`
when there is no rung below it.

---

### 2 — major — audio-output labels are lost for the session when fullscreen pre-empts the microphone modal
`src-tauri/src/streamer/static/index.html:2159` (`permModalIfVisible`), `index.html:2267-2271`
(the submit tail), `audio.js:322` (`enumerateLazily`, `lazyEnumStarted`).

**Pre:** the submit handler asked `needsMicPermission()` and, when true, showed the permission
modal **before** `beginStreaming()`. The user always got the prompt, and a grant made
`enumerateDevices()` return labelled outputs, which are what the host's device panel lists for
"audio output".

**Post:** the join starts first and `enumerateLazily(permModalIfVisible)` raises the modal
afterwards. `permModalIfVisible` installs a `fullscreenchange` watcher and, the moment the page
enters fullscreen, hides the card and resolves it as **skipped** — permanently, because
`enumerateLazily` is one-shot (`lazyEnumStarted`) and nothing re-offers it. Fullscreen mode is
the default (`fullscreenModeInput` is checked unless `se.fullscreenMode === '0'`), and
`installGestureHandler` enters fullscreen on the *first user gesture after the stream goes
live* — which includes the tap the user makes on the modal itself, and any tap they make while
reading it.

The result: `speakers.labelsUnavailable` stays true, `_cache.outputs` carry empty labels, and
the user cannot pick an output device by name for the rest of the session. On iOS below 18.4
(no `HTMLMediaElement.setSinkId`) the same grant is what backs the Speaker/Earpiece control.

**Affected:** every client that joins with the fullscreen toggle on (the default) and does not
answer the card before the stream goes live — most phones and tablets. Desktop Chrome/Edge and
Safari where `navigator.mediaDevices.selectAudioOutput` is absent (i.e. not Firefox 116+).

**Fix:** do not treat "we entered fullscreen" as a decline. Either defer the modal until the
page is out of fullscreen and re-offer it there (drop `lazyEnumStarted`'s latch for the
auto-skip case and set a `pendingPermPrompt` flag that a later `fullscreenchange`-to-windowed
re-raises), or leave the card up and let it render *inside* the fullscreen element by appending
`#permModal` to `#stage` while fullscreen is active. At minimum, make the auto-skip
non-terminal: keep `speakers.labelsUnavailable = true` but allow `enumerateLazily` to be called
again.

---

### 3 — minor — the HTTPS-upsell modal copy was rewritten
`src-tauri/src/streamer/static/index.html:108`.

**Pre:** "This page is on an insecure (HTTP) connection. Switching to the sucre version ensures
data is exchanged quicker and privately. Your browser may warn about the self-signed certificate
once, which is expected; choose to More Info/Advanced --> Proceed to site."
**Post:** a full rewrite (three sentences, different wording, an em dash).

The page may not change in appearance. Part of this is a genuine fix — "sucre" is a typo for
"secure" — but the rest is a rewrite of user-visible copy, and it is the only markup change in
the whole diff besides the `inset` longhands. **Affected:** every client that opens the page over
HTTP. **Fix:** if the intent is a typo fix, change `sucre` → `secure` and leave the rest; if the
new copy is wanted, get it signed off as a deliberate copy change rather than shipping it inside
a latency branch.

---

### 4 — minor — the limited-browser-support modal now lists three audio rows instead of one, with new copy
`src-tauri/src/streamer/static/index.html:2019` ("WebCodecs audio"), `:2029` ("AudioWorklet"),
`:2037` ("Cross-origin isolation").

**Pre:** one row, "WebCodecs audio and AudioWorklet", shown when `!audioFast`.
**Post:** up to three rows with different names and descriptions, and `httpsFixable` is now
computed from `window.isSecureContext` instead of being hard-coded `true`.

The `httpsFixable` change is a correctness fix (telling an iPhone that HTTPS will restore an
`AudioDecoder` its Safari never shipped was wrong). The row split and the new descriptions are a
visible content change to one of the four modals the brief calls out. In practice the third row
is close to unreachable — WebCodecs requires a secure context, and on a secure context the
`isolation_headers()` in `server.rs` make `crossOriginIsolated` true — so it mostly costs review
surface rather than user-visible noise. **Affected:** Safari 16.4–17 and Firefox 130+ clients,
which now see two rows where they saw one. **Fix:** keep the `httpsFixable` correction; consider
collapsing the two WebCodecs-audio rows back into the original single row to leave the modal
unchanged.

---

### 5 — minor — touch and unlocked-mouse moves now travel as `POINTER_MOVE_BATCH` and lose contact size
`src-tauri/src/streamer/static/input.js:642` (`pushStroke`), `input.js:686-691`
(`onPointerMove`), host side `src-tauri/src/streamer/input/windows.rs:434`.

**Pre:** `onPointerMove` sent touch and unlocked-mouse moves through `pendingMoves` as full
40-byte `POINTER_MOVE` frames, which carry per-event `buttons` (u16) and normalised contact
`width`/`height`. Only pen went through the batch.
**Post:** touch and unlocked mouse both go through `pushStroke`, which sends a single
`POINTER_MOVE` only while there is exactly one coalesced sample and no stroke in flight;
otherwise it builds a `POINTER_MOVE_BATCH`. The 24-byte batch sample has no room for
`width`/`height`, and the Windows injector consequently calls
`self.touch(id, s.x, s.y, s.pressure, 0.0, 0.0, Phase::Move)` — contact size zero — for every
batched touch move. `buttons` also becomes stroke-level rather than per-sample.

**Affected:** Android Chrome and desktop Chrome/Edge (which populate `getCoalescedEvents`);
Safari below 18.2 and Firefox Android return one/no coalesced events, so they keep the old
behavior. **Fix:** either keep touch on the single-frame path (`if (src === SRC.touch) { … }`
before `pushStroke`), or carry `width`/`height` in the batch by bumping the sample size, or have
the injector reuse the last known contact size when a batch sample reports zero.

---

### 6 — minor — the hidden IME input is now focused on stylus and touchscreen tablets
`src-tauri/src/streamer/static/input.js:847` (`hasFinePointer`), `input.js:879`.

**Pre:** `if (imeSink && navigator.maxTouchPoints === 0)` — the off-screen `#imeSink` input was
focused only on devices with no touch at all.
**Post:** `hasFinePointer()` = `matchMedia('(any-pointer: fine)').matches`. That matches a
**pen** as well as a mouse, so an iPad with an Apple Pencil, a Surface with its stylus, or an
Android tablet with an active digitiser now gets a real `<input>` focused as soon as the reliable
channel opens. Focusing a text input on a touch device is what raises the software keyboard, and
`#imeSink` sits at `left: -9999px` with `opacity: 0`, so the keyboard would appear over the live
stage with nothing visible to explain it.

The stated intent — touchscreen laptops, which have a real keyboard — is correct; `any-pointer:
fine` is just wider than that intent. **Affected:** stylus tablets. Devices with a hardware
keyboard attached will not show the software keyboard, so the practical blast radius is a pen
tablet with no keyboard. **Fix:** require a mouse rather than any fine pointer, e.g.
`matchMedia('(any-hover: hover) and (any-pointer: fine)')`, or keep `hasFinePointer()` but also
require `navigator.maxTouchPoints === 0 || matchMedia('(any-hover: hover)').matches`.

---

### 7 — minor — the `nosleep.js` shim is never used on any browser that merely *has* `navigator.wakeLock`
`src-tauri/src/streamer/static/index.html:609`, `:813` (`enableNoSleep`), `:824`
(`enableNoSleepShim`).

**Pre:** every browser used the looping-video shim (`new NoSleep()` at module scope,
`noSleep.enable()`).
**Post:** `if (hasWakeLockApi) { acquireWakeLock().then(ok => ok ? … : enableNoSleepShim(), …) }`
— the shim runs only when `navigator.wakeLock.request('screen')` **rejects**. The comment
directly above the function says "On iOS below 18.4 (and in a standalone home-screen app even at
18.4) that is still the only thing that works", but `navigator.wakeLock` exists from Safari 16.4,
and there the request resolves. A sentinel that resolves and then does not actually hold the
screen awake leaves no path back to the shim.

**Affected:** iOS/iPadOS 16.4–18.3, and any 18.4+ device running the page as a home-screen web
app — exactly the population the comment names. **Fix:** on iOS, or whenever the wake lock
resolves but the page is not in fullscreen, arm the shim as well as the sentinel (the shim is
already idempotent via `ns.isEnabled`, and `disableNoSleep()` already tears both down); or add a
watchdog that re-checks `wakeLockSentinel.released` and falls through to `enableNoSleepShim()`.

---

### 8 — minor — `performance.timeOrigin` is still unguarded on the `<video>` fallback frame stats
`src-tauri/src/streamer/static/index.html:1202`.

The branch added a `TIME_ORIGIN` fallback (`Date.now() - performance.now()`) in `input.js:41`
because `performance.timeOrigin` is Safari 15+, and the same expression in `index.html`'s
`startFallbackFrameStats` was not migrated. That code path is the `<video>` element path — the
one old browsers actually take. On Safari 13.1/14 (the 10.15 WKWebView floor and older iOS)
`paintMs` is `NaN`, `lag` is `NaN`, the `lag >= 0 && lag < 10000` test is false, and every sample
is silently discarded, so `hostToPaint` p50/p90/p99 read 0 in the STATS frame for precisely the
devices whose latency most needs measuring. Nothing crashes.

**Affected:** Safari ≤ 14 / iOS ≤ 14 clients on the `<video>` path. **Fix:** export the
`input.js` value (`SEClock.timeOrigin`) or recompute the same fallback in `index.html` and use it
at `:1202`, `transform-worker.js:161`/`:386` and `audio-worker.js:173` (the workers only run on
WebCodecs-capable engines, so those two are latent).

---

### 9 — minor — a render stall posts `{type:'stats'}` to the worker and logs it outside debug mode
`src-tauri/src/streamer/static/index.html:1125`, and the `m.data.type === 'stats'` arm at
`index.html:~1508` (`console.log('[webcodecs] stats', m.data)`).

`handleRenderCount` is driven by `liveWatch`, a 2 s interval. On a stall it now sends
`{type:'stats'}`, whose reply the page unconditionally `console.log`s. The `statsPoll` that used
to be the only source of that message is gated behind `if (DEBUG)`; this one is not. So a stalled
session prints a stats object to the console every 2 seconds on an ordinary user's device.

**Affected:** all clients, during a stall. **Fix:** either drop the `postMessage({type:'stats'})`
(`videostats` is already polled once a second and is what supplies `videoWorkerStats.keyTier`,
the only field read here), or gate the `'stats'` console.log behind `DEBUG`.

---

### 10 — minor — the refresh-rate "refinement" is inert, and its comment misdescribes the old code
`src-tauri/src/streamer/static/index.html:1865` (`resolveRefreshRate`), `:1884`
(`noteRefinedRefreshRate`), `:1969` (`REFRESH_SAMPLES = 24`), and `:~226` in `onStreamLive`.

`onStreamLive` refines the rate with `refreshRatePromise.then(r => noteRefinedRefreshRate(clampRefresh(r)))`
— but `resolveRefreshRate()` already awaited **that same promise** (raced against a 500 ms
deadline) to fill `joinData.refreshRate`. Whenever the measurement finished inside 500 ms — the
normal case, since 24 rAF frames at 60 Hz is ~383 ms — the refinement receives the identical
value, `Math.abs(rate - cur) < 5` is true, and it returns. The safety net the comment describes
("the safety net for a join-time measurement that was wrong … the page spent its first
half-second in a background tab or behind a layout storm") cannot fire for the case it names,
because a throttled measurement produces the same wrong number both times. It only fires when the
500 ms deadline won, i.e. when the join shipped `0`.

Two secondary points in the same block: the comment claims the old code was "400 ms against a
60-sample measurement, i.e. ~1 s at 60 Hz — so `resolveRefreshRate` returned 0 on essentially
every join". That is not what the pre-change file did: `refineRefreshRate()` (pre,
`index.html:1266`) awaited `refreshRatePromise` in full with no deadline and applied it by
calling `reconfigure()`. And `REFRESH_SAMPLES` dropped 60 → 24, which halves the median window.

**Affected:** any client whose first ~400 ms of `requestAnimationFrame` is throttled. **Fix:**
give the refinement an independent measurement (`getRefreshRate(REFRESH_SAMPLES)` started after
`onStreamLive`) rather than re-reading the promise the join already consumed, and correct the
comment.

---

### 11 — minor — STATS v3 is emitted but nothing on the host reads its two new fields
`src-tauri/src/streamer/static/input.js:419-420` (`STATS_LEN = 104`, `STATS_VERSION = 3`),
`input.js:487-489`; host `src-tauri/src/streamer/input/protocol.rs:587`
(`CLIENT_STATS_VERSION = 2`, `CLIENT_STATS_V2_LEN = 96`).

This branch changes **no Rust outside `server.rs`**. `parse_client_stats` accepts any non-zero
version and takes only `b[72..96]`, so the frame is not rejected — but `audioRedundantUsed` and
`audioRedundantDiscarded` at offsets 96 and 100 are written to the wire and dropped, and the
client advertises a version the host does not know. That is 8 bytes per client per second buying
nothing, and it makes the "what the redundancy actually bought" measurement the comment promises
unobtainable.

**Affected:** every session, telemetry only — no user-visible effect. **Fix:** either land the
matching host-side parse (`CLIENT_STATS_V3_LEN = 104`, two more `tail_u32` reads, a
`hostcap::STATS_V3` bit) or revert `STATS_VERSION`/`STATS_LEN` to 2/96 until the host is ready.

---

## Verified unchanged

Compared line-by-line against `git show latency-overhaul:<path>` and found equivalent:

**Markup / appearance.** The entire `<body>` of `index.html` is byte-identical except the two
items above (defect 3, and `inset: 0` → longhands in the inline `#permModal` style). No element
was added, removed, reordered, renamed, or re-classed. All four modals (`#modal` HTTPS upsell,
`#compatModal`, `#sameDeviceModal`, `#permModal`) keep their ids, roles, inline `z-index`
(9999/9998/9997/9996), buttons and structure. `#join`, `.join-wrap`, the session/name fields, the
six `#otp` inputs, `#fullscreenMode`, `#connect`, `#errorMessage`, `#stage`, `#canvas`,
`#video`, `#linkStatus`, `#imeSink` and the footer are untouched.

**`styles.css`.** The only changes are `inset: 0` → `top/right/bottom/left: 0` on `#stage`,
`#video, #canvas`, `#modal, #compatModal, #sameDeviceModal`, `#join` and `.loading`, plus a
`-webkit-backdrop-filter` beside the existing `backdrop-filter`. Both are pure fixes for broken
rendering: `inset` is Safari 14.1+, so on the 10.15 WKWebView floor and on older iOS the whole
declaration was dropped and those fixed/absolute elements collapsed to auto offsets. No colour,
size, spacing, font, radius or z-index changed. No flex `gap` exists anywhere in the served CSS
(the CLAUDE.md rule still holds), and no `:is()`, `:where()`, `aspect-ratio`, `clamp()`,
`dvh/svh/lvh` or `color-mix()` was introduced.

**Join form and OTP.** `validate()`, `otpValue()`, the per-digit input/keydown/paste handlers,
`prefillFromUrl` (including `id`/`name`/`otp`/`fs` consumption and `history.replaceState`),
`VALID_RE`, the 20-char device-name slice and `FULLSCREEN_MODE_KEY` persistence are unchanged
apart from `catch (_) {}` → `catch (_) { noteSuppressed(); }`.

**Kick handling and `/reconfig` polling.** `startReconfigPoll`, `armProbe`, `applyReconfig`
(including the `kickEpoch` branch and `handleKick`), `markAlive`, `enterDegraded`, `linkLost`
and `reconfigEpoch` bookkeeping are behaviorally identical; the only additions are
`clearReconfigWatchdog()` in `onStreamLive` and the new 15 s `reconfigWatchdog`, which only fires
where the old code hung indefinitely.

**Orientation / DPR / viewport reporting.** `readOrientationPortrait`, `screenGeom`, `getDpr`,
`emitViewportSettled`, `onViewportSettled`, `enterViewportMode`/`exitViewportMode`,
`sizeToViewport`/`clearViewportSize` and the `/whep` body's `width`/`height`/`dpr`/`orientation`
fields are unchanged.

**`<video>` ↔ `<canvas>` swap.** `showCanvas()`/`showVideo()`/`liveEl()` are unchanged; the new
`if (PRESENT_PREF !== 'track')` guards are no-ops on the default `?present=canvas`, so the
canvas remains the default tier and the `<video>` element remains the fallback.

**Same-device modal.** `window.__SAME_DEVICE__` and its modal are untouched, and
`server.rs::index` still performs the `__SAME_DEVICE_FLAG__` substitution
(`server.rs:495`) and still returns `isolation_headers()` — COOP `same-origin` + COEP
`require-corp` — unchanged (`server.rs:470-481`), so `crossOriginIsolated` and the
`SharedArrayBuffer` ring behave as before.

**Clipboard.** `forwardClipboard`, `sendClipboard`, the copy/cut/paste listeners and the wire
layout are unchanged; only `te.encode` became `enc` (the lazily-constructed `TextEncoder`).

**Microphone permission declined.** Traced end to end: `permModalIfVisible`'s skip path calls
`onSkip` → `enumerateLazily` resolves `false` → returns `speakers._cache` (already POSTed
without labels). `primeMicPermission()` never rejects (`req.then(ok, () => null)`), `listOutputs()`
never rejects, and the enumeration promise is consumed under a `.catch(() => {})`. Declining
costs nothing beyond the labels, and — unlike pre-change — no longer delays the join at all.

**`server.rs`.** The route table (`/`, `/health`, `/whep`, `/leave`, `/transform-worker.js`,
`/input.js`, `/audio.js`, `/audio-worklet.js`, `/audio-worker.js`, `/nosleep.js`, `/logo.svg`,
`/styles.css`, `/ice-config`, `/net-config`, `/reconfig`, `/audio-outputs`) is unchanged;
content types are unchanged; `index` and `at_capacity_page` are unchanged. The new
`asset()` helper adds a compile-time FNV-1a ETag plus `Cache-Control: no-cache`. Because the
ETag is folded from each asset's own `include_str!` bytes, an edit to any static file changes
its tag, so an app update cannot serve a stale asset — the previously-noted failure mode of a
version-string ETag does not exist here. `index` is deliberately left uncached (it carries a
per-request substitution). `asset_etag()` allocates a small `String` per request, which mildly
contradicts the "no request pays for the hash" comment but costs nothing measurable.

**Node harnesses.** `scripts/test-client-ice.mjs` (19 assertions),
`scripts/test-client-audio.mjs` (16) and `scripts/test-client-stats.mjs` (23) all pass against
HEAD. They use only `node:fs` and `node:vm`, lift the functions under test out of the served
files by name, and add no dependency to the client page.

**Audio fallback wiring.** `noteFallbackStream` gates on `verified.webcodecsOpus &&
verified.worklet`, and `verifyCapabilities()` returns the same object the submit handler mutates
(`audioCaps.worklet = false` on a failed `prepare()`), so a client that could not build its
worklet still attaches the host's Opus track. `abandonWebCodecsAudio()` clears
`verified.webcodecsOpus` before falling back, so the ladder cannot latch itself off. The
`mode`/`mode-mismatch` handshake between `audio-worker.js` and `audio-worklet.js` converges in
both directions and cannot leave the two halves in disagreeing ring modes.

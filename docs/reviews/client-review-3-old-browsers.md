# Client web page review 3 — old-browser compatibility

Branch `worktree-agent-a5109a99927714a08` against `latency-overhaul`, territory
`src-tauri/src/streamer/static/` (7 files, +1594 / -325).

Angle: the page has no build step, no dependencies, and one unsupported token kills the whole
file silently. The question is whether every file still **parses** on iOS 12/13 Safari and
Android 8-era Chrome/WebView, whether the new fast paths are feature-detected, and whether a
failed worker can take the join form with it.

**Headline: there is no parse-time defect anywhere in the changed tree, and the branch removes
two real ones that were in `latency-overhaul`.** Six runtime defects remain, all minor or
speculative.

---

## 1. Method — what was actually verified

- `node --check` on all six JS files: clean. (Node accepts ES2024, so this only proves the
  files are not malformed; it is not the compatibility test.)
- **Empirical parse-level probe.** The two inline `<script>` blocks were extracted from
  `index.html` and every file was re-parsed with `acorn@8.15.0` at successively higher
  `ecmaVersion` values until it succeeded. This is a real answer, not a guess, because acorn's
  `ecmaVersion` gates syntax exactly the way an engine's parser does.
- Targeted greps for each construct in the brief, over all seven files including `styles.css`
  and the inline `<style>` in `index.html`.
- Full read of all seven post-change files (`git show <branch>:<path>`), plus the base
  versions of the lines each defect touches, to separate new from pre-existing.
- Manual guard-tracing of every risky global and method that appears in the added lines.

## 2. Oldest engine each file parses on

| File | Min. `ecmaVersion` (acorn) | Governing construct | Oldest engine that **parses** |
|---|---|---|---|
| `index.html` inline @41 | 5 | assignment only | Any (IE9+) |
| `index.html` inline @157 (main script) | **8 (ES2017)** | `async function` / `await` (`startReconfigPoll` :375, submit handler :1918) | Safari 10.1 · Chrome 55 · FF 52 · Edge 15 |
| `input.js` | **8 (ES2017)** | `async function sendDrop` :259, `await Promise.all` :261 | Safari 10.1 · Chrome 55 · FF 52 |
| `audio.js` | **8 (ES2017)** | `async function verifyCapabilities` :42 and 13 others | Safari 10.1 · Chrome 55 · FF 52 |
| `audio-worker.js` | **6 (ES2015)** | `const` :3, arrow `self.onmessage` :253 | Safari 10 · Chrome 49 · FF 45 |
| `audio-worklet.js` | **6 (ES2015)** | `class … extends` :21, arrow :81 | Safari 10 · Chrome 49 · FF 45 |
| `transform-worker.js` | **8 (ES2017)** | `async function ensureConfigured` :428; trailing comma in a call :634 | Safari 10.1 · Chrome 58 · FF 52 |
| `nosleep.js` (vendored, unchanged) | 5 | — | Any |
| `styles.css` | — | see §4 | Safari 9 · Chrome 76 (for `accent-color` cosmetics only) |

Every file is comfortably below the stated floor (iOS 12 = Safari 12; Android 8 ships
WebView/Chrome 60+ and auto-updates). **Nothing in the tree requires ES2018 or later.**
`transform-worker.js:4` claims "nothing here may use syntax newer than ES2017" — verified true.

## 3. Construct sweep — every item in the brief

`✗` = not present anywhere. `✓ guarded` = present and correctly feature-detected.

| Construct | Result | Where / how guarded |
|---|---|---|
| Optional chaining `?.` | ✗ | zero matches in any file |
| Nullish `??` / `??=` | ✗ | zero matches |
| Logical assignment `\|\|=` `&&=` | ✗ | zero matches |
| Class fields (public) | ✗ | `audio-worklet.js` assigns every field in the constructor |
| Private `#` fields | ✗ | the only `#` hits are the CSS selector `#permModal` (`index.html:12-38`) |
| Static blocks | ✗ | zero matches |
| Top-level `await` | ✗ | zero matches; no `import`/`export` statements |
| Numeric separators `1_000` | ✗ | zero matches |
| BigInt literals `123n` | ✗ | **removed by this branch** — see §5 |
| `getBigUint64` / `setBigUint64` | ✗ | **removed by this branch** — see §5 |
| `import()` in a classic script | ✗ | zero matches |
| Regex lookbehind `(?<=` `(?<!` | ✗ | zero matches |
| Regex named groups `(?<name>` | ✗ | zero matches |
| Regex `s` / `v` flag | ✗ | zero matches; every literal uses only `i`/`g` |
| Object rest/spread `{...x}` | ✗ | zero matches |
| Optional catch binding `catch {}` | ✗ | every site binds, e.g. `catch (_)` |
| `Array.prototype.at` | ✗ | zero matches |
| `.flat()` / `.flatMap()` | ✗ | zero matches |
| `Object.fromEntries` | ✗ | no `Object.*` call anywhere |
| `String.replaceAll` / `matchAll` | ✗ | zero matches |
| `Promise.allSettled` / `.any` | ✗ | only `resolve`/`reject`/`all`/`race` (ES2015) |
| `globalThis` | ✗ | zero matches; workers use `self`, page uses `window` |
| `structuredClone` | ✗ | zero matches |
| `queueMicrotask` | ✗ | `input.js:462` uses a `MessageChannel` instead |
| `AbortSignal.timeout` / `AbortController` | ✗ | zero matches; deadlines are `Promise.race` + `setTimeout` |
| `ResizeObserver` | ✗ | zero matches; resize is `window`/`visualViewport` events |
| `IntersectionObserver` | ✗ | zero matches |
| `matchMedia().addEventListener` | ✓ guarded | `index.html:550-551` — `if (mq.addEventListener) … else if (mq.addListener)`. Same pattern for `screen.orientation` at `:544-547` |
| `crypto.randomUUID` | ✗ | zero matches |
| `Intl.*` | ✗ | zero matches |
| `TextEncoder` in a worker | ✗ | only in `input.js:30` (page scope) — see defect 5 |
| `performance.now()` in a worklet | ✗ | **`audio-worklet.js` contains no `performance` reference at all**; its stats cadence is `blockCount % 96` (`:271`). Correct — `performance` is absent from `AudioWorkletGlobalScope` on old Safari |
| `console` in a worklet | ✗ | **`audio-worklet.js` contains no `console` call**. Errors are `throw new Error(...)` (`:26`, `:41`) |
| `SharedArrayBuffer` unguarded | ✓ guarded | always behind `typeof`: `audio.js:63`, `audio.js:721` (via `sharedMemoryOk()`), `index.html:1185`. No bare reference anywhere |
| `Atomics.waitAsync` | ✗ | only `load`/`store`/`add`, and only when `this.shared` / `shared` is true |
| `Atomics` unguarded | ✓ guarded | `audio.js:64` `typeof Atomics === 'object'`; `audio-worker.js:75-92` gated by `shared`; `audio-worklet.js:99,108,114,273` gated by `this.shared` |
| `OffscreenCanvas` unguarded | ✓ guarded | the identifier is never referenced; the capability is probed as `typeof canvas.transferControlToOffscreen === 'function'` (`index.html:171`, `:227`), and every call site (`:997`, `:1339`) is reached only when `canWebCodecs` |

### Runtime APIs added by this diff, and their guards

| API | Site | Guard |
|---|---|---|
| `navigator.wakeLock.request` | `index.html:689` | `hasWakeLockApi` (`:582`), plus `DEG.WAKE_LOCK_API_ABSENT` and the nosleep shim |
| `navigator.keyboard.lock/unlock` | `:639`, `:845` | `navigator.keyboard && navigator.keyboard.lock` at `:228`, `:638`, `:845`, `:1190` |
| `video.requestVideoFrameCallback` | `:1105`, `:1127-1129` | `typeof … !== 'function'` at `:1105`, `:1133`; `DEG.RVFC_UNAVAILABLE` at `:229` |
| `RTCRtpScriptTransform` | `:1346` | `typeof RTCRtpScriptTransform !== 'undefined'` at `:169` → `useScriptTransformNow()` |
| `createEncodedStreams` | `:1363` | `typeof RTCRtpReceiver.prototype.createEncodedStreams === 'function'` at `:170` |
| `VideoTrackGenerator` / `MediaStreamTrackGenerator` | `transform-worker.js:167,172` | `typeof … === 'function'` in `trackGeneratorCtor()` `:157-161`, plus try/catch |
| `AudioContext.setSinkId` | `audio.js:328`, `:465` | `speakers.hasCtxSink` (`:151`) |
| `HTMLMediaElement.setSinkId` | `audio.js:331`, `:491` | `speakers.hasElemSink` (`:150`), `index.html:1191` uses `typeof HTMLMediaElement !== 'undefined'` |
| `mediaDevices.selectAudioOutput` | `audio.js:508` | `speakers.hasSelectOut` (`:152`) |
| `navigator.audioSession` | `audio.js:162`, `:298` | `'audioSession' in navigator` (`:154`) + try/catch |
| `AudioContext.outputLatency` | `audio.js:549` | `typeof c.outputLatency === 'number' && > 0` |
| `getOutputTimestamp()` | `audio.js:536` | `typeof c.getOutputTimestamp !== 'function'` early return |
| `getCoalescedEvents` | `input.js:482` | `hasCoalesced` (`:475`) = `window.PointerEvent && ('getCoalescedEvents' in PointerEvent.prototype)`, plus try/catch |
| `pc.addTransceiver` | `index.html:1276` | **blocking** compat gate at `:1741` refuses to connect without it — the right answer, since iOS <12.2 and Chrome <69 lack it |
| `navigator.sendBeacon` | `index.html:868` | try/catch |
| `new NoSleep()` | `index.html:568` | `typeof NoSleep === 'function'` + try/catch + **lazy** |

## 4. CSS

`styles.css` and the inline `<style>` are clean against the whole list:

- **`inset` shorthand — removed.** This branch replaces all five `inset: 0` in `styles.css`
  (`#stage`, `#video, #canvas`, the three modals, `#join`, `.loading`) and the one in the
  `index.html` inline `<style>` (`#permModal`) with `top/right/bottom/left` longhands. `inset`
  is Safari 14.1 / Chrome 87, so on iOS 12/13 the stage, the video, the canvas, the join
  screen, all four modals and the loading overlay were previously `position:fixed`/`absolute`
  with no offsets at all — i.e. collapsed to content size at the static position. **This was a
  layout-destroying bug on the exact target engines and the branch fixes it.**
- `-webkit-backdrop-filter: blur(4px)` added ahead of the unprefixed form (`styles.css:500`),
  so the `.loading` scrim now blurs on Safari 9-17 rather than doing nothing.
- No `gap` / `row-gap` / `column-gap` anywhere — flex containers space with margins
  (`#permModal .row > * { margin: .3rem; }`, `index.html:36`), matching the Safari <14.1
  constraint in `CLAUDE.md`.
- No `:is()`, `:where()`, `:has()`, `aspect-ratio`, logical properties (`margin-inline` etc.),
  `env()`, `dvh`/`svh`/`lvh`, `clamp()`/`min()`/`max()`, `color-mix()`, `@supports`,
  `@container`, `@layer`, or custom properties (`var(--…)`).
- `accent-color` (`:192`, `:406`) is Safari 15.4 / Chrome 93 but degrades to the native
  checkbox colour — cosmetic only.
- `user-select` is paired with `-webkit-user-select` (`:35-36`, `:393-394`); `object-fit`
  (`:31`) is Safari 10+.

## 5. What this branch fixed (worth recording)

1. **BigInt eliminated from `audio-worker.js`.** `dv.getBigUint64(4, true)` and
   `Number(captureNs / 1000n)` became `readU64(dv, 4)` (two little-endian u32 halves) and
   `Math.round(captureNs / 1000)`. BigInt is Safari 14 / Chrome 67. `input.js:14-18` already
   carried the same rule; the worker now matches it and says so at `:16-18`.
2. **`inset` shorthand eliminated** (§4).
3. **`enterFullscreen()` (`index.html:601-616`) now tolerates a non-Promise return.** Safari
   below 16.4 returns `undefined` from `webkitRequestFullscreen`, so the previous `.catch()`
   at each call site threw a `TypeError`; because `startLock()` is invoked un-awaited from
   `onStreamLive()`, that rejection silently skipped pointer lock, keyboard lock **and** the
   wake lock for the entire session on every old Safari.
4. **`acquirePointerLock()` (`:646-681`) handles the same shape** for
   `requestPointerLock({unadjustedMovement:true})`, which returns `undefined` below Chrome 92 /
   Safari 18.4, and reports `DEG.UNADJUSTED_MOVEMENT_UNAVAILABLE` instead of guessing.
5. **`new NoSleep()` moved out of module scope** into a lazy `getNoSleep()` with try/catch
   (`:560-576`). A 404 or parse failure of `/nosleep.js` used to take the inline script — and
   therefore the whole page — down with it.
6. Wake Lock is now tiered (real API → nosleep video shim), and `matchMedia`/`screen.orientation`
   listeners fall back to `addListener`/`.onchange`.

## 6. Worker isolation and script order

`index.html` loads `/input.js`, `/audio.js`, `/nosleep.js` as three separate classic scripts
(`:153-156`) **before** the inline script (`:157`). A parse error or 404 in any of them cannot
reach the others, and the inline script gates every use behind `window.RemoteInput &&`,
`window.SEAudio &&`, `window.SEClock &&`, `typeof NoSleep === 'function'`. `input.js` in turn
gates its own callback into the page with `window.SEVideo && SEVideo.noteSuppressed`
(`:744-746`). Verified: **no script-order hazard.**

The three worker/worklet files are all loaded lazily and after the join, so none of them can
affect the join form:

- `/transform-worker.js` — fetched inside `start()` (`:1226-1241`), only when `canWebCodecs`.
  Fetch failure falls back to `new Worker('/transform-worker.js')`; a throw there rejects into
  `start()`'s catch → `fallbackToVideo`. **But there is no `worker.onerror`** — defect 2.
- `/audio-worker.js` — `new Worker` inside try (`audio.js:747-752`), with `w.onerror` set
  (`:758`).
- `/audio-worklet.js` — `await ctx.audioWorklet.addModule(...)` inside try (`audio.js:673-696`);
  a missing `audioWorklet` throws a `TypeError` that the same catch absorbs and reports as
  `DEG.AUDIO_PREPARE_FAILED`.

## 7. Numbered defects

All six are runtime, not parse-time. Numbers 1, 3, 5 and 6 are **pre-existing** in
`latency-overhaul` and survive unchanged into this branch; 2 and 4 are properties of code this
branch added or restructured.

### 1 — `performance.timeOrigin` is unguarded on the all-browsers clock path
`src-tauri/src/streamer/static/input.js:426` (and the consumer at
`src-tauri/src/streamer/static/index.html:1233`)

**Engine that breaks:** Safari < 15 — i.e. iOS 12, 13 and 14 Safari, and macOS Safari up to
14.1. `performance.timeOrigin` shipped in Safari 15.0; Chrome has had it since 49.

`input.js` is loaded by *every* client, and `onPong` computes
`const midAbsMs = performance.timeOrigin + (echoedNs / 1e6 + nowMs) / 2;`. With `timeOrigin`
`undefined` this is `NaN`, so `clock.offsetMs = NaN` and `notifyClock()` publishes it. The two
workers and `audio.js:630` all screen it with `isFinite`, but `index.html:1233` assigns it
straight into `hostClockOffsetMs`, and `:1195` reports `clockValid: hostClockOffsetMs !== null`
— `true` for a clock that is `NaN`. `input.js:348` then sets the `CLOCK_VALID` telemetry flag,
so the host records a valid clock offset for a device that has none.
`SEClock.hostNsToAbsMs()` (`input.js:764`) likewise returns `NaN` rather than `null`.

**Fix:** compute the origin once with a fallback that is exactly what `timeOrigin` means —
`const TIME_ORIGIN = (typeof performance.timeOrigin === 'number' && isFinite(performance.timeOrigin)) ? performance.timeOrigin : (Date.now() - performance.now());`
— and use it at `input.js:426`. Independently, harden the consumer:
`index.html:1233` should be `if (typeof offsetMs === 'number' && isFinite(offsetMs)) hostClockOffsetMs = offsetMs;`,
matching the guard every other consumer already applies.

### 2 — the transform worker has no `error` handler
`src-tauri/src/streamer/static/index.html:1288`

**Engine that breaks:** not version-specific. It is the recovery path for any client where
`/transform-worker.js` fails to load or parse — a 404, a CSP that blocks both the blob worker
and the direct URL, or a future edit that introduces a token the client's engine rejects.

`worker.onmessage` is assigned at `:1288`; no `onerror` is. A worker whose script throws at
load time is created successfully and then simply never posts anything. The page therefore
waits for a timeout instead of demoting immediately: `transformWatchdog` (2500 ms, `:1355`)
only exists when `hasLegacyEncodedStreams`, which is false on Safari, so on Safari the only
net is `watchdog = setTimeout(() => fallbackToVideo('no WebCodecs frame within 5s'), 5000)`
inside `pc.ontrack` (`:1409`). Five seconds of black screen where a callback would give an
instant fallback. `audio.js:758` already sets `w.onerror` on its worker; this one is the
inconsistency.

**Fix:** immediately after `:1288`, add
`worker.onerror = (ev) => { noteSuppressed(); fallbackToVideo('transform worker failed to load: ' + ((ev && ev.message) || 'unknown')); };`

### 3 — file drop uses `Blob.prototype.arrayBuffer()` with no fallback
`src-tauri/src/streamer/static/input.js:264` (call site `:661`)

**Engine that breaks:** Safari < 14 (iOS 12/13/14.0), Chrome < 76, Firefox < 69.
`Blob.prototype.arrayBuffer` is Chrome 76 / Safari 14 / Firefox 69.

`sendDrop` does `data: new Uint8Array(await f.arrayBuffer())`. On an older engine `f.arrayBuffer`
is `undefined`, the call throws a `TypeError` inside an `async` function, and the call site
`if (…) sendDrop(ndx(e), ndy(e), e.dataTransfer.files);` never attaches a rejection handler —
so a file drop is silently discarded and surfaces only as an unhandled rejection in the
console. Nothing in the compat gate mentions it.

**Fix:** a two-line `FileReader` shim, used when the method is missing:

    function fileBytes(f) {
      if (typeof f.arrayBuffer === 'function') return f.arrayBuffer();
      return new Promise((res, rej) => {
        const fr = new FileReader();
        fr.onload = () => res(fr.result); fr.onerror = () => rej(fr.error);
        fr.readAsArrayBuffer(f);
      });
    }

and wrap the `sendDrop(...)` call at `:661` in a `.catch(noteSuppressed)`.

### 4 — the `AudioContext` is created and left running on every browser that cannot use it
`src-tauri/src/streamer/static/audio.js:660` / `:693-695`

**Engine that breaks:** any engine without `AudioWorklet` — Safari < 14.1 (so all of iOS 12/13/14.0),
Chrome < 66, Firefox < 76. Also any engine where `addModule('/audio-worklet.js')` 404s.

`beginFromGesture()` is called unconditionally from the join handler (`index.html:1922`). It
constructs `new AC({ latencyHint: 'interactive', sampleRate: SR })` and calls `resume()`, then
`attachWorklet()` fails at `await state.ctx.audioWorklet.addModule(...)` because `audioWorklet`
is `undefined`. The catch at `:693` logs and returns `false` — correctly — but `state.ctx` is
never closed, by that catch or by `teardown()` (`:814-825`). The session then plays audio
through the `<audio>` element (netEQ path) while a second, empty, *running* `AudioContext`
holds the output device open for the whole session. On iOS that is a real battery and
audio-session cost for zero benefit, and it is paid only by the old browsers.

**Fix:** in `attachWorklet()`'s catch, release it —
`try { if (state.ctx) state.ctx.close(); } catch (_) { note(); } state.ctx = null; state.node = null; state.gain = null;`
— then set `preparePromise = null` so a later `prepare()` is not permanently poisoned.
Every reader of `state.ctx` already handles `null` (`:328`, `:449`, `:465`, `:836`, `audio.js`
`degradeFlags` `:868`).

### 5 — `new TextEncoder()` runs at `input.js` module scope with no guard
`src-tauri/src/streamer/static/input.js:30`

**Engine that breaks:** Safari < 10.1, Chrome < 38, IE11 — all below the stated floor, hence
speculative rather than a live bug.

The construction is the third statement of the top-level IIFE. If it throws, the entire IIFE
aborts, `window.RemoteInput` and `window.SEClock` are never defined, and the page loses remote
control, the clock sync and the whole telemetry channel — **silently**, because
`checkBrowserRequirements()` (`index.html:1751-1826`) probes data channels and Pointer Events
but never `TextEncoder`, so the user is not told and the connect button stays enabled.

**Fix:** either construct lazily (`let te = null; function enc(s) { if (!te) te = new TextEncoder(); return te.encode(s); }`)
or add `typeof TextEncoder === 'undefined'` to `checkBrowserRequirements()` as a `blocking`
entry so the failure is at least visible.

### 6 — `window.isSecureContext` is read without a fallback
`src-tauri/src/streamer/static/index.html:1760`

**Engine that breaks:** Safari < 11.1, Chrome < 47 — below the floor, hence speculative.

`const httpsCouldHelp = !window.isSecureContext;` evaluates to `true` where the property does
not exist, including on an https:// page. The `onHttp` filter at `:1846` suppresses the
`httpsFixable` entries only on `http:`, so on https:// the WebCodecs-audio bullet takes the
"Enables the lowest-latency audio path." branch (`:1777`) rather than the accurate "this
browser cannot decode Opus with WebCodecs" text. Cosmetic, but it is exactly the misleading
message the comment at `:1755-1758` says it wants to avoid.

**Fix:** `const httpsCouldHelp = (typeof window.isSecureContext === 'boolean') ? !window.isSecureContext : (location.protocol === 'http:');`

## 8. Defect count

| Severity | Count | Numbers |
|---|---|---|
| Blocker | 0 | — |
| Major | 0 | — |
| Minor | 4 | 1, 2, 3, 4 |
| Speculative | 2 | 5, 6 |

Findings files, all tagged `Workstream: W11`:

- `findings/minor-client-performance-timeorigin-unguarded-below-safari-15.md`
- `findings/minor-client-transform-worker-has-no-onerror-handler.md`
- `findings/minor-client-file-drop-uses-blob-arraybuffer-with-no-fallback.md`
- `findings/minor-client-audiocontext-left-running-when-worklet-unavailable.md`
- `findings/speculative-client-textencoder-constructed-at-input-js-module-scope.md`
- `findings/speculative-client-issecurecontext-read-without-a-fallback.md`

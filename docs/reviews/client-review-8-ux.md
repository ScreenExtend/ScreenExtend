# Client review 8 — the `ux-client` UX pass

Cold review of branch `ux-client` (2 commits on top of `latency-overhaul` @ `38724f7`:
`44f3f6d` "mic prompt before the join, and nothing interrupts a live session" and `246f8cb`
"serialise the enumeration behind the picker"). Read-only; nothing was built or run.

Scope reviewed in one pass: correctness/control flow, fallback integrity, host-contract
agreement, regressions against reviews 5–7, and old-WebKit parse safety.

Files: `src-tauri/src/streamer/static/{index.html,audio.js,input.js,styles.css}`,
`src-tauri/src/streamer/{server.rs,session.rs,telemetry.rs}`, `src-tauri/src/lib.rs`,
`src-tauri/src/{windows,macos}_utils/device_reporter.rs`, `src/lib/bindings.ts`,
`scripts/test-client-reconfig.mjs`, and the four docs the branch touches.

**Counts: 1 high · 5 medium · 12 low.** Line numbers are post-change.

---

## High

### H-1 — the queued output picker re-arms on every 1 Hz poll tick, so it reopens on every tap for the rest of the session

`src-tauri/src/streamer/static/index.html:517-536` (`applyReconfig`), with
`index.html:1139-1169` (`queueOutputPicker`).

`process_reconfig` (`server.rs:774-780`) puts `audioSink` in **every** `/reconfig` body —
`selected_audio_output(...).unwrap_or_default()` — so `applyReconfig` calls
`SEAudio.setSpeaker(j.audioSink)` on every probe tick, about once a second. When the engine
cannot route to the host's choice, `setSpeaker` returns `false` (`audio.js:685`) on *every*
tick, because `speakers.appliedSink` is never updated and the early-out at `audio.js:594`
never fires. Each of those ticks calls `queueOutputPicker(j.audioSink)`.

Two consequences:

* Where `canPickOutput()` is true (Firefox, flagged Chromium), the user opens the picker,
  dismisses it or picks something the engine still will not route to, `fire()` disarms — and
  the next tick, ≤ 1 s later, arms it again. **The next tap or keypress on the stage opens the
  picker again, forever.** The stage becomes unusable: every gesture is swallowed by
  `pickerGestureArmed` (`index.html:882`) instead of re-entering fullscreen/pointer lock, and
  a browser chooser is thrown in the user's face on each one. This is the "nothing interrupts
  a live session" ruling inverted — the owner's ruling (4) says a refused host change "arms a
  re-offer", singular.
* Where `canPickOutput()` is false, `queueOutputPicker` reaches
  `console.warn('[audio] the host asked for an output this browser will not route to')`
  (`index.html:1145`) — once per second for the rest of the session.

**Fix.** Latch the offer per requested id. Keep the last id the picker was armed or spent on
(`lastOfferedSinkId`), and in `applyReconfig` only call `queueOutputPicker(j.audioSink)` when
`j.audioSink !== lastOfferedSinkId`; clear the latch on `teardownSession`/`showError`
alongside `cancelQueuedPicker()`. The `console.warn` should be behind the same latch (or
behind `DEBUG`).

---

## Medium

### M-1 — `current` is plumbed end to end and then dropped; the manual test asserts a UI behaviour that cannot happen

`src/App.tsx:377`, `src/components/pages/device-details.tsx:187`, against
`docs/manual-tests/client-page.md:196`.

The field is correct on the wire (`server.rs:717-721` `#[serde(default)] Option<String>`),
correct in `session.rs:449-462` (a client that omits it does not erase a newer one's value),
correct in `AudioOutputsReport`/`DeviceAudioOutputs`, and correct in the regenerated
`bindings.ts`. But both TS consumers rebuild `AudioOutputsInfo` as
`{ supported, outputs, selected }` and drop `current`, and the device-details picker still
renders `selected` only. The new manual-test row says "The host's device panel follows within
a few seconds" — it will not, on any device, so that row can only ever fail.

**Fix.** Either add `current` to `AudioOutputsInfo` and show it in `device-details.tsx`
(a "playing on …" hint beside the picker when `current !== selected`), or reword the manual
test to say the value is observable only over the wire / in the event payload.

### M-2 — dialogs are revealed synchronously after asynchronous lock exits, with no confirmation and no fallback timer

`src-tauri/src/streamer/static/index.html:1087-1120`.

`beginModal()` fires `exitPointerLockNow()`, `navigator.keyboard.unlock()` and
`exitFullscreenNow()` and returns immediately; `withModal(fn)` then runs `fn(done)`, which
sets `.show` / `display:flex` in the same task. Pointer-lock exit is delivered on
`pointerlockchange` and fullscreen exit on `fullscreenchange`, both later. So the dialog is
shown **before** either has landed, and — critically — a page modal is a sibling of `#stage`,
so while the fullscreen element is still `#stage` the modal is not rendered at all.

The old code had exactly this net and this branch deleted it with the functions that carried
it: `permModalIfVisible` watched `FULLSCREEN_EVENTS` for the life of the card, and
`offerPermModalNow` had a 1500 ms "an exit that never lands must not strand the caller"
timeout. `exitFullscreenNow` swallows a rejection into `p.catch(noteSuppressed)` and Safari's
`webkitExitFullscreen` returns `undefined`, so there is no signal either way. The result of a
refused exit is now an invisible dialog with input suspended (`modalSuspend` gates the gesture
handler at `:882`, the visibility handler at `:973` and the fullscreen handler at `:997`) and
nothing that ever un-suspends it.

Today's only mid-session `withModal` is the native picker, which the browser draws itself, so
the practical exposure is small — but the net is gone for the next page modal, and the ruling
asked for it explicitly.

**Fix.** In `beginModal`, resolve when `!currentFullscreenEl() && !document.pointerLockElement`
(watching `fullscreenchange`/`pointerlockchange`) with a ~1500 ms deadline, and have
`withModal` reveal the dialog from that resolution. Failing that, at minimum give `endModal`
a watchdog so `modalSuspend` cannot outlive a dialog nobody can see.

### M-3 — `applyPendingSink` claims `sinkTier` 4 for sinks the picker never chose

`src-tauri/src/streamer/static/audio.js:703-713`.

`applyPendingSink()` sets `sinkTier = SINK_TIER.selectAudioOutput` for **any** pending sink
that lands, but `speakers.pendingSink` is also written by `setSpeaker` itself when the graph
does not exist yet (`audio.js:629`) — i.e. by a host-commanded change that arrived before
`attachWorklet` finished. `attachWorklet` then drains it (`audio.js:962`) and the host is told
tier 4 although `selectAudioOutput()` was never called. That directly contradicts the
paragraph this branch added to `docs/TELEMETRY.md` ("claimed only once the picked device is
actually routed") and is the class of silent mis-report `sinkTier` exists to prevent.

**Fix.** Tag the provenance — e.g. `speakers.pendingSink = { id, fromPicker: true|false }`, or
a separate `speakers.pickerSink` — and only promote the tier when the pending sink came from
`pickOutput`.

### M-4 — the join has no deadline on `selectAudioOutput()`, and a picker that never settles wedges the join *and* leaves `modalSuspend` set

`src-tauri/src/streamer/static/index.html:2843-2860` and `:2924-2940`.

Every other promise on the join path is bounded — `AUDIO_PREPARE_DEADLINE_MS`,
`REFRESH_DEADLINE_MS`, `WHEP_TIMEOUT_MS`, `ENUM_TIMEOUT_MS`. `pickPromise` is not, and
`enumPromise` is chained behind it, so `await enumPromise` at `:2940` is the last gate before
`beginStreaming()`. A chooser that never settles (a tab backgrounded while it is open, an
engine that resolves neither way) leaves the page on the spinner with `connectBtn.disabled`
and no error — and because `endModal()` only runs from the two settle handlers,
`modalSuspend` stays `true`, so even a later session would never take fullscreen or pointer
lock.

The mic card is deliberately unbounded — the owner ruled the join waits for the user's answer —
but that card has two buttons and cannot be left unanswered by accident. A browser chooser can.

**Fix.** `Promise.race` the picker against a generous deadline (10–15 s) that resolves `null`
and calls `endModal()`, and note the timeout with `noteSuppressed()`.

### M-5 — the keystroke that opens the queued picker is forwarded to the host first

`src-tauri/src/streamer/static/input.js:880` vs `index.html:1167-1168`.

`input.js` registers `on(window, 'keydown', onKeyDown, true)` during `setup(pc)`;
`queueOutputPicker` adds its `fire` to `window` with capture later. Same target, same type,
same phase, so registration order decides: `onKeyDown` runs first, `suspended` is still
`false`, and the key is sent to the host. `beginModal` → `setSuspended(true)` only lands
afterwards. So the key that summons the dialog also lands on the host's desktop — the exact
thing the suspension exists to stop, and it can be a modifier or a destructive key.

`pointerdown` and `touchend` are safe (input.js binds those on `#stage` in the bubble phase,
so the window-capture `fire` wins).

**Fix.** Either drop `keydown` from `PICKER_GESTURES` (a tap is a sufficient gesture and the
stage is the thing being tapped), or arm the picker on `keyup` so the matching `keydown` was
already delivered as a pair, or have `fire` call `e.preventDefault()`/`stopImmediatePropagation()`
— the last only works if `fire` is registered first, which would need `queueOutputPicker` to
re-register `input.js`'s handler.

---

## Low

### L-1 — `enumerateBeforeJoin` overwrites the cache without the `_lastEnum` guard its sibling has

`audio.js:342-344`. `reEnumerateAndPost` (`audio.js:536-543`) falls back to `_lastEnum` when a
fresh enumeration comes back empty or unlabelled; `enumerateBeforeJoin` assigns
`speakers._cache` unconditionally. On the picker path the enumeration runs **after**
`pickOutput` has already cached a labelled list (`:2931`), so a second `enumerateDevices()`
that returns fewer labels silently downgrades what the host is shown.

### L-2 — `pickOutput`'s pre-selected fallback cannot work

`audio.js:731-742`. When `selectAudioOutput({deviceId})` rejects with anything other than
`NotAllowedError`/`AbortError` (e.g. `NotFoundError` for an id this browser does not know),
the code falls through to a bare `md.selectAudioOutput()`. That second call needs transient
activation the first one already consumed, so it can only reject. Either return `null` on any
rejection, or make the pre-selected call the only call.

### L-3 — `httpsFixable: true` now hides the WebCodecs rows on `http://localhost`

`index.html:2666`, `:2680`, filtered at `:2767`. The deleted `httpsCouldHelp` used
`isSecureContext`, which is **true** on `http://localhost` / `http://127.0.0.1` — the
same-device join. On https the change is a no-op (the `onHttp &&` guard already covered it),
but on a localhost join the two optional rows are now suppressed and `offerHttpsOnLoad()`
still offers a switch to HTTPS that fixes nothing, which is precisely the misleading copy the
removed comment existed to avoid. Cheap fix: keep the row unless `location.protocol === 'http:'
&& !window.isSecureContext`.

### L-4 — the ICE hold makes auto-fullscreen less likely

`index.html:423-473`. `startLock()` runs from `onStreamLive`, which is now held until the
trickle sender settles (up to `ICE_SETTLE_DEADLINE_MS` = 8 s). `requestFullscreen()` and
`navigator.keyboard.lock()` need transient activation, and Chromium's window is 5 s from the
Join click — which `selectAudioOutput()` has already spent on the engines that have it. Expect
`degraded` bit 0 (`FULLSCREEN_DENIED`) more often, and one extra tap before the session goes
fullscreen. The gesture handler is the net, so this is a UX cost, not a break; it is worth a
line in `docs/manual-tests/client-page.md` §7 so a tester does not report it as a defect.

### L-5 — `#unmute` is an interactive affordance over the stage that does not go through `beginModal`

`index.html:101`, shown from `tryPlay()` at `:1492-1500`. While pointer lock is held, clicks
target the lock element and the cursor is hidden, so "Tap to start playback" cannot be reached
with a mouse — the same failure class as the cards this branch fixed. In practice the element
is `muted`, so `play()` rarely rejects, which is why it has survived; but the ruling asked for
every affordance to be traced, and this one is not.

### L-6 — `setSuspended` writes `suspended` before its `!active` guard

`input.js:823-828`. A card raised on the join form sets `suspended = true` while `active` is
false. Today every such card is balanced before `RemoteInput.setup()` runs, so nothing leaks —
but if a modal ever outlives a `setup()` (a picker still open across a reconfigure), the new
session forwards input with `modalSuspend` still true, and the page's two notions of
suspension disagree. Move the assignment below the guard, or re-assert
`RemoteInput.setSuspended(modalSuspend)` at the end of `setup()`.

### L-7 — a tab closed during the ICE hold sends no `/leave`

`index.html:1172` (`if (leaveSent || !isLive || reconfiguring) return`) against `:452`, where
the held branch returns before `isLive = true`. The host falls back to the disconnect grace,
so this is a delay rather than a leak, but it is a new window that did not exist before the
hold.

### L-8 — `postOutputs` sends `current` identical to `selected`

`audio.js:558-568`. Both are `speakers.appliedSink`. The separation is real only because the
host's `selected` comes from `set_device_audio_output` and the handler ignores the posted
`selected` entirely (`server.rs:728-758`) — so the behaviour is right, but the client-side
comment claims a distinction the client does not make. Either post the raw
`readCurrentSink()` result as `current` and the last host-commanded id as `selected`, or
reword the comment.

### L-9 — bit 15 now has two meanings and `docs/TELEMETRY.md` was not amended

`index.html:430` degrades `DEG.ICE_GATHERING_DEADLINE` when the *trickle settle* deadline
expires. `docs/TELEMETRY.md:455` still reads "ICE gathering ran to its deadline instead of
settling", which is true of the tier-2 wait and only loosely true of the new case (nothing
waited for gathering at all). A host reading bit 15 can no longer tell them apart.

### L-10 — the new mechanisms have no harness coverage

`scripts/test-client-reconfig.mjs:65` replaces the `offerPermModalNow` stub with
`queueOutputPicker`, but no case in the file drives it — the payloads carry no `audioSink`, so
the whole new branch at `index.html:517-536` (including H-1) is unasserted. Nothing lifts
`holdStageForIce`/`iceSettled`/`onStreamLive`, so the stage-hold contract that the ICE section
of `docs/CAPABILITY-TIERS.md` now advertises is untested; `scripts/test-client-ice.mjs:50-62`
still calls `startTrickle(pc)` / `startTrickle(pc, cb)` and never passes `onSettled`. Both
harnesses still lift real source and both still pass — the gap is coverage, not breakage.
A cheap addition: lift `applyReconfig` with a `SEAudio` stub whose `setSpeaker` resolves
`false` and assert `queueOutputPicker` is called **once** across ten ticks.

### L-11 — two rows in the fallback-ladder table have five cells in a four-column table

`docs/manual-tests/client-page.md:277-284`. The header is
`| Fast path | Floor | How to force the floor | The line that proves it |`; the rows added at
`:283` and `:284` carry five cells, so their last cell ("`sinkTier` **4** only where the picked
device actually routed", "`videoPath` must be unchanged either side") is dropped by every
Markdown renderer. The lead-in at `:275` also still says "Four rungs" for six rows.

### L-12 — `CLAUDE.md` names a "remote-control permission" modal that the page does not have

`CLAUDE.md`, "The client web page" section. `index.html` ships four dialogs — HTTPS upsell
(`#modal`), limited-browser (`#compatModal`), same-device (`#sameDeviceModal`) and the
microphone card (`#permModal`). There is no remote-control permission modal. Worth correcting
in the same pass, because the "trace every dialog" instruction was written from that list.

---

## Verified correct

**Ruling 1 — the microphone card is pre-join, terminal, and never shown where the picker exists.**
`enumerateBeforeJoin(micCard)` is chained into `enumPromise` (`index.html:2924-2936`) and
awaited at `:2940`, before `beginStreaming()`; the card is a plain `withModal` around
`#permModal` with only Continue/Not now, so the join genuinely waits for an answer rather than
a timer. `enumStarted` (`audio.js:333`) is a page-lifetime latch and `offerOutputLabels` /
`enumerateLazily` / `permModalIfVisible` / `offerPermModalNow` are all gone, so there is
exactly one call site and no re-offer path — including from `applyReconfig`, which now applies
sinks silently. A decline degrades bit 9 twice over (`micCard` at `:2820` and
`degradeFlags`'s `labelsUnavailable` at `audio.js:1267`), which is belt-and-braces, not a bug.
`needsMicPermission()` returns `false` for `hasSelectOut` at `audio.js:306`, so Firefox and
flagged Chromium never reach the card — the comment's claim that the two never both raise UI
holds.

**Ruling 2 — nothing the page decides on its own renegotiates behind a picture.**
`noteRefinedRefreshRate` (`index.html:2536-2542`) only writes `joinData`;
`SEVideo.onAudioFastLost` (`:1813-1828`) drops the claim and logs instead of calling
`reconfigure()`. The trickle gate is sound: `holdStageForIce` before `startTrickle`,
`onStreamLive` parking on `liveHeld`, `iceSettled` replaying it, `releaseIceHold` in
`teardownSession` (`:1511`) so a rebuild cannot inherit a stale hold, `stale()` on the
`onSettled` callback (`:2267`), and `settle()` reached from all four exits — `stop()`, the
404 branch, `dead()` (after `onDead`, as the comment says), and the 8 s deadline. I traced
every ordering I could construct in which `dead()` could fire *after* a settle: `quiesce()`
sets `closed`, `flush()` and the ack timer both bail on it, and only one PATCH is in flight at
a time, so a post-settle rejoin is unreachable. Both failure paths land in front of the join
form, because `#join` (z-index 30) is only hidden from the non-held branch of `onStreamLive`.
The stale-canvas swap is correct in both directions: `insertBefore(replacement, canvas)` puts
the old element later in document order so it paints on top, `.stale` in `styles.css:41-63`
reproduces `#canvas`'s box exactly (long-hand `top/right/bottom/left`, no `inset`) and matches
the existing `#stage.stalled` opacity rule, and every exit drops it — `onStreamLive`,
`showVideo`, `showError`, and `freshCanvas` itself.

**Ruling 3 — "Switch to HTTPS" always offered.** `offerHttpsOnLoad` is gated only on
`location.protocol === 'http:'` and `!compatBlocked`, and both fast-path rows now carry
`httpsFixable: true` so neither is listed on an http page. Behaviour on real http and real
https is unchanged from the previous `httpsCouldHelp`; only the localhost corner moved (L-3).

**Ruling 4 — the picker runs inside the Join click's activation.** `beginFromGesture()`,
`canPickOutput()` and `pickOutput()` all run before the handler's first `await`
(`index.html:2833-2854`), and `beginModal()` — which only calls exit/unlock APIs — does not
consume activation. Fullscreen and the keyboard lock are requested from `startLock()` in a
much later task, so there is no ordering conflict with the picker. `canPickOutput()` is
`typeof navigator.mediaDevices.selectAudioOutput === 'function'` (`audio.js:254`), so
Chromium without the flag and every Safari take the no-op branch. A dismissal rejects and is
answered with `null` and a `noteSuppressed()`, not an error, and the join continues.
`sinkchange` (`audio.js:522-530`, on a context that `teardown()` never destroys, so the
`sinkWatched` latch is right) and the device poll (`reEnumerateAndPost` now leads with
`syncCurrentSink()`) both report browser-side changes.

**Ruling 5 — every dialog goes through the suspension.** HTTPS upsell (both call sites,
`:2407` and `:2431`), the optional compat modal (`:2769`), the same-device modal (`:2785`) and
the microphone card (`:2794`) are all `withModal`; the blocking compat modal takes a
deliberate unbalanced `beginModal()` (`:2761`) with `connectBtn.disabled = true` beside it, so
nothing is being held back from a session that cannot exist; the join-click picker uses raw
`begin`/`endModal` balanced on both settle paths; the queued picker uses `withModal` with
`close` on both. Kick and error are not modals — `kickDevice` → `showError` → `stopLock()`
does the full release. `withModal`'s `catch` calls `done()`, so a throwing body cannot leak a
level, and `endModal` refuses to go below zero. `input.js`'s `on()` wrapper is the single
choke point for every forwarded listener, so `setSuspended(true)` genuinely stops all of them,
and it sends `FOCUS_STATE false`, a synthetic `POINTERLOCK_STATE false` (correct — the real
`pointerlockchange` arrives in a later task, by which time the listener is gated off) and
`clearHeld()`, then restores focus, pointer-lock state and the size on the way out. Bit 23 is
set only when `setSuspended` reports it actually suspended something, so a pre-join card
reports nothing, as `docs/TELEMETRY.md:463-469` says.

**Host contract.** `AudioOutputsRequest.current` is `#[serde(default)] Option<String>`, so an
older client still parses; `set_audio_outputs` only overwrites on `Some`, so an older client
cannot erase a newer one's value; the string is truncated to `MAX_AUDIO_OUTPUT_STR` like every
other attacker-controllable field on that unauthenticated endpoint. `AudioOutputsReport` and
`DeviceAudioOutputs` gain a plain `String` (empty = never said), both `device_reporter`s were
updated, and `bindings.ts` carries the Rust doc comments verbatim in specta's emission style —
consistent with a regeneration rather than a hand edit. `CLIENT_DEGRADED_NAMES` grew to 24 in
lock step with `DEG.INPUT_SUSPENDED_FOR_MODAL = 1 << 23`.

**Old-WebKit parse safety.** No `?.`, `??`, `??=`, class fields, `#private`, top-level `await`
or BigInt literal in `index.html`, `input.js` or `audio.js` (the only `#`-prefixed hits are CSS
id selectors in the inline `<style>`). The inline `#permModal` block and the new
`canvas.stale` rule both use `top/right/bottom/left` long-hands, never `inset`. Everything the
branch adds is ES2017-or-older: function declarations, arrows, `const`/`let`, object shorthand
methods, `Promise.resolve().then`, `forEach`. Both Node harnesses still lift the real shipped
source by name (`takeFunction`/`takeConst`), and `startTrickle`'s new third parameter is
optional (`if (onSettled)`), so `test-client-ice.mjs`'s two- and one-argument call sites are
unaffected.

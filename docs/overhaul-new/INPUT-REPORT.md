# INPUT-REPORT.md: the input path, read and not run

Step 5 (§9) of the overhaul-new job. **Everything here comes from reading the code. Nothing was
run that injects, captures or measures input**: no `SendInput`, no `SetCursorPos`, no CDP
`Input.*`, no synthetic DOM events, no `scripts/measure-input.ps1` / `measure-stream.ps1` /
`test-startup-gate.mjs`, no host and no browser. The only things executed were `git` (read
only) and two small Node scripts that do arithmetic on the absolute-coordinate formula (§4.2).
`src-tauri/src/streamer/input/` and `src-tauri/src/streamer/static/input.js` are unchanged.
Every claim below is either cited to code or marked **UNVERIFIED**.

Base: `latency-overhaul-new` at `80e00a4`. The input files there are byte-identical to
`latency-overhaul` (`git diff latency-overhaul HEAD -- <input paths>` is empty). The old app is
`bef5f89`.

Contents

1. The commit set (43 commits, reconciled)
2. Per-commit ledger
3. Today's input path, end to end, compared with the old app
4. Hypotheses for each symptom, ranked
5. Changes that look wrong or risky from reading alone
6. Proposed changes (not applied)
7. Manual test plan the owner can run by hand
8. Instrumented plan for a later job that is allowed to inject input
9. Findings in other groups' areas (capture, transport, client page)
10. Unverified, and questions for the owner

---

## 1. The commit set

`commit-groups.json` marks 43 commits `touches_input`: the 16 in G11 plus 27 others. Checked
against git:

* `git log bef5f89..latency-overhaul --no-merges -- src-tauri/src/streamer/input src-tauri/src/streamer/static/input.js`
  returns **39**. Default history simplification hides **`5d10607`**. That commit changes a
  `windows.rs` line on the Mac-side branch, and the merge that brought it in was TREESAME for
  that path. With `--full-history` the same query returns **40**.
* The remaining 3 (`cabf63a`, `afb64e6`, `af59c4a`) touch only `scripts/measure-input.ps1`.
  `cabf63a` also touches `windows_utils/streamer/pipeline.rs` (the cursor cadence window). The
  classifier marked them `touches_input` because they belong to the input harness.
* 40 + 3 = 43. No commit after `latency-overhaul` touches input
  (`git log latency-overhaul..latency-overhaul-new -- <input paths>` is empty).

All 43 are covered below. For each one I read the input-file diff where it was small and the
final code plus the commit message where the diff was large: `protocol.rs` tests, telemetry
plumbing and macOS bodies. I read every hunk that decides how an event moves or is dropped
(`windows.rs`, `mod.rs`, `protocol.rs` parse/route, `input.js` send and flush paths, and the
lock and modal code in `index.html`) in full, both in the current tree and in `bef5f89`.

---

## 2. Per-commit ledger

Risk is judged from reading only. **H** means a plausible direct cause of the reported
symptoms. **M** means it changes behaviour on the hot path in a way that could hurt. **L** means
input-neutral or a clear fix. "Later" names a commit that altered or undid this one.

| # | Commit | Date | Group | What changed (input files) | What it claims | Later | Risk |
|---|---|---|---|---|---|---|---|
| 1 | `6ab8e40` | 09-08 03:12 | G05 | PONG grows 9→17 bytes with `host_now_ns`. `input.js` gets an NTP-style offset estimate and `window.SEClock` (written with `BigInt` at first) | Correct display-lag / A/V-sync clock | `d5cfe79` removed BigInt; `2becec0` added a `timeOrigin` fallback; `8e5d3c8` grew PONG to 19 | L |
| 2 | `a3e3e08` | 03:38 | G08 | `OP::STATS` 72-byte client→host frame once a second on `reliable`. Parse in `protocol.rs`; no-op dispatch arms in `windows.rs`/`macos.rs` | Host sees the client's stages and tiers | v2 `d5cfe79`/`2208f4d`, v3 `4c4c070`/`ea04926` | L |
| 3 | `d5cfe79` | 04:57 | G11 | `input.js`: coordinates normalised against the letterboxed picture rect; `getCoalescedEvents()` for mouse and touch, **capped at the newest 8 per flush**; the locked path **sums coalesced `movementX/Y`** instead of using the dispatched event's; a new unlocked-mouse batch path (`pushStroke`/`pendingStrokes`); `KEYFRAME_REQUEST` 0x64; stats v2; BigInt removed; fast drops counted | Taps land correctly under letterboxing; high-rate fidelity; legacy-transform clients can ask for an IDR | `2becec0` (zero-sum fallback), `72c4b11` (touch stops batching). The "host replays one injection per sample" comment went stale with `7ce9d34` | **H**: truncates locked motion (H3); unlocked mouse batches are not flushed or discarded at `pointerdown` (H4) |
| 4 | `b19736b` | 05:01 | G12 | macOS scroll/zoom through the non-variadic `CGEventCreateScrollWheelEvent2` | Fixes arm64 scroll deltas | — | L (macOS) |
| 5 | `664d26e` | 05:07 | G11 | `positions_cursor()` added; Down/Up position the cursor unless locked; `mouse_relative` **starts false** (was true); Windows gets a `POINTERLOCK_STATE` arm; **locked deltas switch from relative `MOUSEEVENTF_MOVE` to absolute injection from a tracked `rel_pos`, so no Windows curve applies** | Clicks land where the frame says; one pointer curve | `f957022` (clamp scope), `420d76f` (relative again for unadjusted+All), `301230d` (`rel_pos` → `GetCursorPos`) | **H**: introduced the tracked-absolute path (H1/H2) and the initial-false state behind H6b |
| 6 | `f1b3134` | 05:08 | G11 | `Drop for Injector` destroys the synthetic pen/touch devices | Stops a per-session handle leak | — | L (real fix) |
| 7 | `ef921fc` | 05:10 | G07 | `#imeSink` focused when `(any-pointer: fine)` instead of `maxTouchPoints===0`; `rtt` getter removed | Touchscreen laptops get IME input | `72c4b11` added the hover test | L |
| 8 | `7ce9d34` | 05:15 | G11 | Host injects a mouse batch as one `SendInput` array; pen/touch stay one call per sample; predicted samples dropped for ink; batch tail with a predicted count; macOS collapses a mouse batch to its last sample | 20–37 % less injector time per 40-sample batch | — | L–M: the batch's last sample still carries the button word (H4 pathway) |
| 9 | `35986dd` | 05:27 | G11 | Telemetry stages `input_recv/queue/inject` and drop counters; optional client stamp tails; MOUSE_DELTA flags byte; **`FAST_CAP` 512→64 with drop-oldest**; **a 50 ms age gate that drops positional events, `MOUSE_DELTA` included**; PONG moved off the receive handler; `input::tune_process` deleted; `parse_drop` bounded | Stop replaying a stale queue; the input path becomes measurable | `da2c2ec` made the gate source-aware | **M**: dropping a relative delta loses that motion for good (H10) |
| 10 | `eff862a` | 05:38 | G03 | `KEYFRAME_REQUEST` parsed; transport forces an IDR with a 200 ms floor | Legacy-transform clients recover from a lost IDR | — | M (cross-group: a possible keyframe-storm driver, §9) |
| 11 | `2208f4d` | 05:40 | G08 | v2 stats tail parsed | Tiers visible host-side | — | L |
| 12 | `8e5d3c8` | 05:49 | G11 | PONG grows to 19 bytes with a `u16` capability bitfield | Client can feature-detect | — | L (but capabilities only arrive with the first PONG, ≈1 s in: H1a) |
| 13 | `71fd9df` | 05:55 | G05 | Client claims the out-of-band keyframe tier only when the capability bit is set; limiter armed only on a real send | Legacy-transform watchdog stays honest | — | L |
| 14 | `2becec0` | 06:07 | G11 | Lock: retry a plain lock when the options form throws; set unadjusted on fulfilment; zero-sum coalesced fallback to `e.movementX`; drop letterboxed; `timeOrigin` fallback; lazy `TextEncoder` | Pointer lock recovers; clock not NaN | — | L–M (keeps the 8-sample truncation) |
| 15 | `4c4c070` | 06:16 | G06 | Stats v3 audio counters | Audio redundancy telemetry | `ea04926` | L |
| 16 | `34cfafb` | 06:16 | G11 | `ClientProfile { device_name, platform }`; macOS Ctrl/Command remap per client platform; Windows only signature changes | Correct modifiers for Apple clients | — | L (Windows) |
| 17 | `d03efe5` | 13:26 | G11 | Client sends stamps, the **MOUSE_DELTA flags byte** and a predicted count of 0, **all gated on capability bits from the first PONG** | Host can measure the wire; exactly one curve | — | **M**: the host's curve decision now depends on state the client lacks for the first second (H1a) |
| 18 | `9087343` | 13:33 | G06 | `rawSend` names a throw once; file drops screened against `pc.sctp.maxMessageSize` | No silent drop failures | — | L |
| 19 | `803e88f` | 13:51 | G11 | `teardown()` clears `hostCaps` and **`lockIsUnadjusted`**; losing the lock clears it too | The claim cannot leak into the next connection | — | **H**: every rebuild clears the claim while the lock is still held, and nothing sets it again (H1b) |
| 20 | `da2c2ec` | 14:28 | G11 | Age gate source-aware (pen and touch never dropped); `ParseError`; the fast-channel `u16` sequence and reorder gate (**dropped late deltas too**); per-client pen id | Ink is never dropped; parse errors visible; no backward cursor walk | `39f00cb` exempted deltas from the reorder gate | M |
| 21 | `72c4b11` | 14:36 | G11 | Touch stops batching (full 40-byte frame); `imeSink` focus needs `(any-hover: hover)` | Touch keeps contact size; no surprise soft keyboard | — | L |
| 22 | `ea04926` | 14:50 | G08 | v3 tail parsed; batch flags byte is a reserved 0 | Telemetry correctness | — | L |
| 23 | `d3ce813` | 15:27 | G09 | `InputBackend` probe tier; injector diagnostics move to the log bus; macOS Accessibility re-probed every 500 ms | A silent no-op on macOS becomes visible | — | L |
| 24 | `c5e9db3` | 15:52 | G08 | macOS injector constructor binds the tier | — | — | L (macOS) |
| 25 | `ea0aa36` | 16:02 | G11 | `INPUT_SEQUENCE` capability bit; a clipboard write failure is logged | Client can tell; "paste does nothing" is diagnosable | — | L |
| 26 | `e01b4a1` | 17:36 | G08 | Reason string on an `#[ignore]` in `windows.rs` | Test hygiene | — | none |
| 27 | `4280b13` | 17:44 | G11 | Client writes the fast-channel sequence on pointer, batch, wheel and delta frames | Activates the host reorder gate | — | L–M |
| 28 | `39f00cb` | 18:31 | G03 | Reorder gate never drops `MOUSE_DELTA`; a late delta's button word is rewritten to the **last delta's** word; the high-water mark only advances | Late deltas are motion, not noise | — | **M**: the rewritten word can be older than a reliable `Down` already injected, which synthesises a spurious button-up (H4b) |
| 29 | `45129d9` | 19:04 | G08 | Golden client-frame vectors + `protocol.rs` tests | Client and host cannot drift apart | — | none |
| 30 | `5d10607` | 19:17 | G08 | `#[ignore]` reason in `windows.rs` (hidden by default git history simplification) | — | — | none |
| 31 | `b266e75` | 19:27 | G14 | Comment and log text in `linux.rs` / `windows.rs` | Docs resolve | — | none |
| 32 | `f957022` | 09-09 18:17 | G09 | `PointerScope {Display, All}`, **default Display**; tracked position clamped to `pointer_bounds()` | Confine a locked pointer to its display | `d42f0df` flipped the default to All; `f569e41` made it live | M |
| 33 | `44f3f6d` | 18:29 | G07 | `setSuspended()`: every listener stands down while a dialog or picker is up; sends `FOCUS_STATE false` + `POINTERLOCK_STATE false` and resends both on resume | A modal is reachable; held modifiers released | — | M (by design it drops lock, keyboard lock and fullscreen for any dialog, including a mid-session output-picker re-offer: H6) |
| 34 | `d42f0df` | 21:04 | G04 | `PointerScope` **default All**; only the explicit string `"display"` restricts | Owner ruling: unrestricted reach | — | M (pointer leaves the virtual display and vanishes from the stream: H5) |
| 35 | `0db7fc0` | 09-10 11:23 | G03 | Injector thread gets a `stop` one-shot; bounded join | A clean leave no longer deadlocks display removal | — | L (real fix) |
| 36 | `f569e41` | 20:28 | G04 | `LiveInput` atomics for control and scope, read per event; input-only edits no longer renegotiate | A cosmetic or input edit costs the stream nothing | — | L–M (good: fewer rebuilds) |
| 37 | `a6331c9` | 22:35 | G08 | `SE_INPUT_TRACE=1` per-event host line; `measure-input.ps1` | The client→host leg becomes measurable | `cabf63a`, `afb64e6`, `af59c4a` | L (trace off by default) |
| 38 | `420d76f` | 22:38 | G11 | `lets_os_curve(unadjusted)`: **raw deltas under scope All go back to relative `MOUSEEVENTF_MOVE` (Windows curve); otherwise the tracked-absolute path** | Exactly one curve, applied by whichever side holds raw counts | `301230d` | **M**: splits behaviour on a flag that is often wrong (H1) |
| 39 | `cabf63a` | 23:05 | G08 | Harness restores config on every exit; **`pipeline.rs` cursor cadence window ½ → ⅔ period** | Harness safety; cursor fps | `6faa359` replaced the window with an exemption | L for input |
| 40 | `e6467d4` | 23:36 | G08 | Test and attribute fixes in `windows.rs` | Gate passes | — | none |
| 41 | `afb64e6` | 09-11 00:08 | G08 | Harness `-Load` uses half the CPUs | Harness survives | — | none |
| 42 | `af59c4a` | 01:01 | G08 | Harness becomes DPI-aware; self-test checks for *movement* | Harness correctness. Notes "the cursor is pinned at the primary display's centre … something else running here" | — | none (see §10 Q9) |
| 43 | `301230d` | 09-14 16:48 | G11 | Tracked path: **base = `GetCursorPos()` on every delta** (the remembered `rel_pos` is removed); macOS mirrors it with `CGEventGetLocation` (uncompiled) | Host trackpad motion between client frames no longer snaps back | — | **H** (H2: rounding feedback, and possibly a stale read under bursts) |

Related commits outside the 43 that the analysis leans on: `b0183dd` (09-09 23:18, added the
WGC cadence gate), `d848234` (09-10 02:18, host draws the pointer), `8e65d90` (merge of it),
`a871ec7` (09-10 22:37, WGC composites the pointer again), `6faa359` (09-10 23:56, a moved
cursor is exempt from the cadence gate), `932e0bc` (09-08 06:14, one shared runtime, 4
workers), `3791155` (09-10 22:37, back to 16 workers).

**Timeline of the locked-pointer gain.** It is the single most important thing in this table,
and it moved four times:

| Window | Locked delta injection | Curve(s) applied |
|---|---|---|
| `bef5f89` (old app) | relative `MOUSEEVENTF_MOVE`; plain `requestPointerLock()` | **two**: client OS + Windows |
| `664d26e` → `420d76f` (09-08 05:07 → 09-10 22:38) | tracked absolute from `rel_pos`; client asks for `unadjustedMovement` | **none** for Chrome on Windows/macOS (raw counts, no host curve) |
| `420d76f` → `301230d` | relative if `unadjusted && scope==All`, else tracked from `rel_pos` | one, or none when the flag is false |
| `301230d` → today | same split; tracked base = `GetCursorPos` per delta | one, or none + rounding drift when the flag is false |

Even when everything works, the new app is **slower per hand movement than the old app** for
anyone used to it, because the old app accelerated twice. That is a gain change, not latency,
but it is the first thing a user calls "lag" (see §10 Q1).

---

## 3. Today's input path, end to end

Notation: **[T]** = a thread hop. **[Q]** = a queue or buffer. **[D]** = a place where an event
can be dropped. **[R]** = a place where events can be reordered. **[old]** = how `bef5f89` did
the same stage.

### 3.1 A pointer move (mouse)

1. **OS → browser.** The client OS delivers the mouse report to the browser process, then the
   renderer compositor thread, then the renderer main thread **[T]**.
   * On a secure context (HTTPS, or the cloud page), Chromium exposes `pointerrawupdate`
     (`input.js:851`), which fires per raw report and is not rAF-aligned.
   * On a non-secure context it is not exposed, so `pointermove` is used. It is rAF-aligned:
     one dispatched event per animation frame, with the rest in `getCoalescedEvents()`.
   * **The LAN QR code is `http://…:8080`** (`src/lib/utils.ts:87`), so the default LAN join is
     on the `pointermove` path. **[old]** identical selection.
2. **`input.js` `onPointerMove`** (`input.js:711`).
   * **Locked** (pointer lock held), mouse:
     * `coalescedOf(e)` returns at most the **newest 8** coalesced samples
       (`MAX_COALESCED_PER_FLUSH`, `input.js:672-681`) **[D]**.
     * Their `movementX/Y` are summed. If the sum is 0, it falls back to `e.movementX`.
     * `applyMouseCurve()` is the identity at the defaults (`sensitivity 1.0, accel 0`, overridable
       from `localStorage['rib.settings']`).
     * The result is added to `rel`. If the gate is open, `sendMouseDelta()` sends inline;
       otherwise it waits for the `MessageChannel` flush (next task) **[Q]**.
     * `clampI16(v|0)` **truncates** any fractional part, and the remainder is discarded, not
       carried **[D]**.
     * **[old]** used `e.movementX/Y` of the dispatched event (the browser's own sum) and had no
       8-sample cap.
   * **Unlocked**, mouse: `pushStroke()`.
     * A single sample goes as a full 40-byte `POINTER_MOVE` (`queueSingleMove`).
     * Several samples go as a `POINTER_MOVE_BATCH` of up to 40, newest 8 per flush
       (`pendingStrokes`). The first is sent inline, the rest at the flush **[Q]**.
     * **[old]** sent only `POINTER_MOVE` for the mouse (no batch), and `pendingMoves` was
       cleared on `pointerdown`. **The batch queue is not cleared on `pointerdown`** (see H4).
   * Coordinates are normalised against the letterboxed picture rect (`picX/picY/picW/picH`).
     **[old]** used the stage, which was wrong under letterboxing.
3. **Send.** `fastSend()` goes to the `fast` DataChannel: `ordered:false, maxRetransmits:0,
   priority:'high'` (`input.js:956`).
   * If `bufferedAmount > 8192` the frame is dropped and counted (`fastDropped`) **[D]**.
   * Tails are added only after the first PONG has delivered the capability bits: an 8-byte
     stamp, a flags byte (MOUSE_DELTA), and a `u16` sequence.
   * The first PING goes out **1 s after `reliable.onopen`** (`setInterval(…,1000)`, no
     immediate ping), so the first ≈1 s + RTT of every connection carries plain v1 frames with
     **flags = 0**.
   * The browser's network thread (dcsctp) then sends DTLS over UDP **[T][Q]**.
4. **Network.** SCTP over DTLS over ICE.
   * The `fast` channel is unordered with no retransmission: a lost packet is gone **[D]**, and
     packets can arrive in any order **[R]**.
   * `reliable` and `bulk` are ordered and reliable. A loss holds every later message on that
     stream until it is retransmitted (head-of-line blocking), and the recovery time is RTT plus
     either fast retransmit or an RTO.
   * The receiver is webrtc-sctp 0.17.1 (`AckMode::Normal`): the first DATA packet after idle
     starts a **200 ms delayed-SACK timer** (`association_internal.rs:1034-1043`,
     `ACK_INTERVAL = 200 ms`), and the second packet triggers an immediate SACK.
   * **[old]** same channel layout and same library behaviour.
5. **Host receive.** UDP mux / ICE read task → DTLS → SCTP association → the data channel's read
   loop → the `on_message` closure (`webrtc_session.rs:1193`). Each step is a tokio task on the
   **shared transport runtime** **[T][Q]**.
   * Width is `min(available_parallelism, 16)`, overridable with `SE_TRANSPORT_WORKERS`
     (`streamer/mod.rs:64`).
   * Tokio has no dedicated I/O thread: an arriving datagram waits for a parked worker or for a
     busy worker's `event_interval` (61 polls).
   * History: `932e0bc` narrowed it to 4 (measured: 779/1000 frames of a 1 kHz burst reached
     `SendInput`); `3791155` restored 16 (1000/1000).
   * **[old]** one runtime per network adapter, each `available_parallelism` wide.
   * `protocol::parse` reads the opcode, so `fast` is decided by opcode, not by the channel
     the message arrived on.
   * PING, STATS and KEYFRAME_REQUEST are handled here. Everything else goes to `route()` if
     `live.control_enabled()` (per message).
6. **`InjectorTx::route`** (`mod.rs:434`).
   * Stamps `host_recv_ns` **after** the scheduler wait above. A runtime stall is therefore
     invisible to every host stage histogram (`docs/OPTIMIZATIONS.md` UX-INPUT says so).
   * Positional (fast) events go to a crossbeam `bounded(64)`. When it is full, the oldest is
     popped through `fast_relief` and the new one pushed **[Q][D]**.
   * Everything else goes to an unbounded `reliable` queue **[Q]**.
   * **[old]** `bounded(512)`, dropping the *newest* when full.
7. **Injector thread** (`mod.rs:489`; OS thread, `THREAD_PRIORITY_TIME_CRITICAL`) **[T]**.
   * `select!` over `stop`, `reliable` and `fast` **picks randomly among ready channels**
     **[R]**: a reliable `Down` can be injected before or after a fast move sent just before it.
     **[old]** same random select.
   * **Age gate**: an event picked up more than **50 ms** after `host_recv_ns` is dropped if
     it is supersedable, meaning a mouse `Pointer` Move/Enter/Over, a mouse `PointerBatch`, or
     **any `MouseDelta`** **[D]**.
   * **Reorder gate**: a fast frame whose `seq` is behind the highest injected is dropped if it
     is an absolute mouse position **[D]**. A late `MouseDelta` is kept, but its button word is
     replaced with the last delta's.
   * **[old]** had neither gate.
8. **`Injector::dispatch`** (`windows.rs:213`).
   * `refresh_geometry()` re-reads `SM_*VIRTUALSCREEN` and `monitor_rect(device_name)` every
     500 ms. That lookup is by the cached `\\.\DISPLAYn` **slot name**.
   * **Absolute** (unlocked): `abs()` maps through `pixel()` into the target rect, then
     normalises to 0..65535 over the virtual desktop with `round()`. The result is
     `MOUSEEVENTF_MOVE|ABSOLUTE|VIRTUALDESK`, then `button_transitions(old_word, frame_word)`.
     A batch is one `SendInput` array (`7ce9d34`). **[old]** one `SendInput` per sample, same
     mapping.
   * **Locked delta** (`mouse_delta`, `windows.rs:381`):
     * If `unadjusted && scope == All`: relative `MOUSEEVENTF_MOVE(dx,dy)`, and Windows applies
       pointer speed and "Enhance pointer precision". This is the **relative path**.
     * Otherwise: `advance_rel()` reads `GetCursorPos`, adds dx/dy, clamps to `pointer_bounds()`,
       normalises with `round(u/W*65535)` and sends it absolute. This is the **tracked path**,
       with no host curve.
     * Then `button_transitions(old_word, frame_word)`.
     * **[old]** always relative, so always curved twice.
   * `PointerScope` default: `Display` at `f957022`, `All` since `d42f0df`. It is read per event
     from `LiveInput` (`f569e41`).
9. **Windows.** `SendInput` → win32k / raw-input thread → cursor position update → DWM. Whether
   `GetCursorPos` reflects an injected move synchronously is **UNVERIFIED** (H2b).
10. **Capture** (WGC, `windows_utils/streamer/pipeline.rs`).
    * `CursorCaptureSettings::WithCursor` by default. `SE_CURSOR_OVERLAY=1` switches to
      `WithoutCursor` plus the host-drawn overlay.
    * `on_frame_arrived` calls `CursorProbe::changed()` (`GetCursorInfo` + `MonitorFromPoint`)
      **before** two gates:
      * the cadence gate (drop a frame within ¾ of a period of the last), from which a
        cursor-moved frame is **exempt** (`6faa359`);
      * the dirty-region gate, consulted only when `autoFrameRate` is on (off by default).
    * Cursor history: `b0183dd` added the cadence gate, which also discarded cursor frames;
      `d848234` made the host draw the pointer (polled); `a871ec7` gave it back to WGC; `cabf63a`
      tried a ⅔ window; `6faa359` made it exempt. Measured painted fps for a pointer sweep:
      59 → 113 (UX-INPUT).
    * **[old]** WGC with cursor and `MinimumUpdateInterval = ⅔ frame`, no post-delivery gate.
11. **Encode → packetize → pace → send**, then the client's decode and paint (other groups).
    Under pointer lock the browser hides the local cursor, so **the only cursor the user sees is
    the composited one**, and motion-to-photon for it is the whole host→client latency plus
    stages 1–9. Unlocked, the local OS cursor and the composited one are both visible (§4.4).

### 3.2 Keyboard

* `keydown`/`keyup` are listened for on `window` in the capture phase. `sendKey` sends inline on
  `reliable` (ordered, reliable) with a stamp after the first PONG, and calls `preventDefault()`.
* Host: the unbounded reliable queue. Keys are never age- or reorder-dropped. `key()` sends a
  scancode `SendInput`, or a Unicode `text()` for unmapped keys. `down_keys` is tracked for
  `release_all`, which runs on `FOCUS_STATE false`, `VISIBILITY false`, PC
  `Disconnected/Failed/Closed`, and injector exit.
* Risks:
  * Head-of-line blocking behind a lost reliable packet, so key-ups are late. Browser key
    auto-repeat keeps arriving meanwhile, which looks like a runaway key.
  * Keys in a rebuild gap are lost (§4.3).
* **[old]** same path, without stamps.
* **Pre-existing**: `preventDefault()` on `keydown` cancels the browser's own `paste` event, so
  the `paste`-driven clipboard forwarding never fires from Ctrl/Cmd+V. The host pastes **its
  own** clipboard (UNVERIFIED in a browser; this is how Chromium specifies it).

### 3.3 Wheel and zoom

* Sent on `fast` (unreliable), with stamp and sequence once capabilities are known.
* Host: not supersedable and not reorder-droppable. Scaling is `deltaMode 0 → 120/100` per
  pixel, `1 → 120` per line, `2 → 360` per page.
* Lost on any packet loss; reordering commutes. **[old]** same.

### 3.4 Touch and pen

* Touch: full 40-byte frames on `fast` (`72c4b11`), with Down/Up on `reliable`. The host injects
  through a synthetic touch device (10 contacts).
* Pen: batches on `fast`.
* Host: neither is ever age-dropped or reorder-dropped, so **a reordered touch or pen sample is
  injected out of order**, which puts a zig-zag in the ink. Drop-oldest on a full queue can still
  discard them.
* **[old]** touch and pen samples were never reordered on purpose, but there was no sequence
  either.

### 3.5 Clipboard and drop

* `bulk` channel (ordered, reliable) → the host's reliable queue → `set_clipboard`
  (`OpenClipboard`…) **on the injector thread**. Op 2 adds Ctrl+V.
* Drops: files are written to `%TEMP%` **on the injector thread**, then `WM_DROPFILES` is posted.
* Any long operation here stalls every queued pointer event, and those older than 50 ms are
  then age-dropped (H10).

---

## 4. Hypotheses, ranked

Symptoms: **cursor glitches**, **input glitches**, **input lag**, **cursor being defocussed**.
Ranking is by likelihood × how many users each one reaches, from reading alone.

### 4.1 The ranked list

| Rank | Hypothesis | Symptoms | Where | Introduced by | Likelihood |
|---|---|---|---|---|---|
| 1 | **H1. Locked motion goes down the tracked-absolute path far more often than intended.** The path was meant for already-curved deltas; it takes over in five cases (below) | lag ("slower"), speed changes mid-session, glitches combined with H2 | `input.js` `teardown()` / `onopen` / ping timing; `windows.rs::lets_os_curve` | `664d26e`, `d03efe5`, `803e88f`, `420d76f` | **High** |
| 2 | **H2. Tracked path: `GetCursorPos` base + `round()` normalisation feeds Windows' pixel rounding back into the next move.** Holds under the truncating conversion (§4.2) | glitches (1 px right/down moves lost, left/up doubled, drift), sticky precise aiming | `windows.rs:381-453` (`advance_rel`, `mouse_delta`) | `301230d` on top of `664d26e` | **High if Windows truncates** (UNVERIFIED which model) |
| 3 | **H4. Spurious button transitions from positional frames.** A move, batch or delta sent before a click but injected after it restates the old button word; `button_transitions` turns that into an up (then a down again) | input glitches: double-clicks, a drag that drops, text selection jumping | `windows.rs::button_transitions` callers; `mod.rs:540-545`; `input.js` `onPointerDown` / flush order | pre-existing, widened by `d5cfe79` (mouse batches) and `39f00cb` (word rewrite) | **Medium-high** |
| 4 | **H6. Pointer lock or focus is lost, or the session is rebuilt, and input comes back wrong** | cursor defocussed; first click after an edit lands in the wrong place | `index.html` `beginModal`, `queueOutputPicker`, `teardownSession`; `input.js` `setup/onopen`; `windows.rs` `mouse_relative: false` | `44f3f6d`, `664d26e`, `803e88f`; the 5 s demotion defect | **Medium-high** |
| 5 | **H3. Locked motion truncated to the newest 8 coalesced samples** on every engine that exposes `getCoalescedEvents()` on the `pointermove` path | lag ("pointer slower than my hand"), inconsistent speed with frame rate or jank | `input.js:668-681, 714-725` | `d5cfe79` | **Medium** (depends on whether the owner's browser exposes coalesced events on `http://`, §10 Q4) |
| 6 | **H5. The pointer leaves the virtual display** (scope All + relative path; and an off-by-one at the right/bottom edge on the absolute path) and disappears from the stream; clicks land on the host's own monitors | cursor defocussed / vanishes | `mod.rs::PointerScope` default; `windows.rs::pixel()` | `d42f0df` (default All), `pixel()` pre-existing | **Medium** (by design for All; the edge bug is real) |
| 7 | **H7. Input is dead across every rebuild**, and the known 5 s demotion forces one per affected session | input glitches; dropped keys or drags around 5 s after connect | `index.html::fallbackToVideo / reconfigure / teardownSession` | client-page territory | **Medium** |
| 8 | **H9. Cursor-exempt frames run the stream at the compositor's rate** (113 fps measured). A weak client decoder falls behind, drops to the freshest frame and asks for a keyframe, which feeds the IDR storm while the mouse moves | cursor stutter and lag on slower clients | `pipeline.rs:3432-3442`; `transform-worker.js` resync | `6faa359` | **Medium-low** (unmeasured on a real client) |
| 9 | **H10. Deltas are dropped for age or overflow**, so motion is lost permanently whenever the injector stalls (clipboard, drop, low-level hooks) | lag, then the cursor "jumps short" | `mod.rs:382-393, 444-455, 521` | `35986dd` | **Low-medium** |
| 10 | **H8. Transport starvation**: runtime width (fixed at 16), Wi-Fi airtime shared with IDR bursts, reliable head-of-line blocking under loss, 200 ms delayed SACK for lone packets | lag spikes on keys and clicks | runtime, SCTP | `932e0bc` (fixed by `3791155`) | **Low** on wired or same-machine; **medium** on lossy Wi-Fi |

**Double acceleration today:** none on the relative path (raw counts + Windows curve) and none
on the tracked path (already-curved counts + no host curve). The only remaining case is a
browser that resolves the `{unadjustedMovement:true}` promise but delivers OS-curved deltas;
`2becec0`'s comment says this is not expected. The opposite failure, **no curve at all**, is the
live one: H1 puts Chrome's raw counts on the tracked path.

### 4.2 H2 worked through: the rounding feedback in `advance_rel`

The code (`windows.rs:390-398, 439-453`), with `left`/`W` = `SM_XVIRTUALSCREEN` /
`SM_CXVIRTUALSCREEN`:

```
base = GetCursorPos()                     // integer pixel P
T    = clamp(P + dx)                      // target pixel
n    = round_f32( (T - left) / W * 65535 )
SendInput(MOUSEEVENTF_MOVE | ABSOLUTE | VIRTUALDESK, n)
```

What pixel Windows lands on for `n` is the whole question. Two models:

* **Truncating**: `x = left + floor(n * W / 65536)`. This is the commonly reported behaviour
  (and what ReactOS's re-implementation does with `>> 16`), and is why the usual advice is to
  add 1 or round up.
* **Nearest**: `x = left + round(n * W / 65536)`.

**Which one this Windows build uses is UNVERIFIED.** It can only be settled by injecting (§8).

Let `u = T - left`. Then `n ≈ u·65535/W`, so
`n·W/65536 = u − u/65536 + e·W/65536`, where `e ∈ [−½, ½]` is the rounding error of `n`. The
mapped value sits a few hundredths of a pixel **below** `u` for most `u`:

* Under **truncation** it lands at `u−1`.
* Under **nearest** it lands exactly on `u`.

Computed exactly. The script re-implements the Rust's f32 normalisation and `round`, applies each
model to every pixel, and was run from the job's scratchpad, not the repo; it is pure
arithmetic.

| Geometry | Truncating: pixels that land 1 short | Nearest: pixels that land wrong |
|---|---|---|
| 1920 wide, single monitor (left 0) | 1643 / 1920 (85.6 %), every `u ≥ 961` | 0 |
| 1080 tall | 939 / 1080 (86.9 %), every `u ≥ 541` | 0 |
| 2560 wide, single monitor | 2096 / 2560 (81.9 %), every `u ≥ 1286` | 0 |
| 1440 tall | 1247 / 1440 (86.6 %), every `u ≥ 721` | 0 |
| Virtual desktop: 1920 at x = −1920 plus 2560 primary (left −1920, W 4480) | 3884 / 4480 (86.7 %), every `u ≥ 2241` | 0 |
| 2560 primary plus 1920 to the right (left 0, W 4480) | 3884 / 4480 (86.7 %) | 0 |

Worked single steps under truncation (P = cursor before, d = delta):

| Case | n | n·W/65536 | lands | moved | ought to move |
|---|---|---|---|---|---|
| 1920 wide, P = 1000, d = +1 | 34167 | 1000.986 | 1000 | **0** | +1 |
| 1920 wide, P = 1000, d = −1 | 34099 | 998.994 | 998 | **−2** | −1 |
| 1080 tall, P = 700, d = +1 | 42537 | 700.988 | 700 | **0** | +1 |
| 2560 wide, P = 1280, d = +1 | 32793 | 1280.977 | 1280 | **0** | +1 |
| 2560 wide, P = 1280, d = −1 | 32742 | 1278.984 | 1278 | **−2** | −1 |
| 1440 tall, P = 1000, d = −1 | 45465 | 998.987 | 998 | **−2** | −1 |
| vdesk L = −1920, W = 4480, P = 320, d = +1 | 32782 | 2240.957 (+L) | 320 | **0** | +1 |
| vdesk L = −1920, W = 4480, P = 320, d = −1 | 32753 | 2238.975 (+L) | 318 | **−2** | −1 |
| vdesk L = −1920, W = 4480, P = −1000, d = +1 | 13473 | 921.006 (+L) | −999 | +1 | +1 (one of the ~13 % that land right) |
| vdesk L = −1920, W = 4480, P = −1000, d = −1 | 13443 | 918.955 (+L) | −1002 | **−2** | −1 |

Simulated runs (each step re-reads the landed position as the next base, which is exactly what
`GetCursorPos` does):

| Run (1920 wide, start mid-screen) | Truncating | Nearest |
|---|---|---|
| 100 × (+1) | moved **0** | +100 |
| 100 × (−1) | moved **−188** (−200 from start x ≥ 1720) | −100 |
| 50 × (+3) | moved **+100** | +150 |
| 50 × (−3) | moved **−188** | −150 |

The same shape holds for 2560, 1080, 1440 and both virtual-desktop layouts. **The negative
origin changes nothing**: the arithmetic is in `u = T − left`, which is always non-negative.

**Verdict on the other agent's hypothesis:**

* **It holds exactly as stated under a truncating conversion.** One-pixel moves right or down are
  lost, one-pixel moves left or up overshoot by one, and any delta of size k moves k−1 right/down
  and k+1 left/up across 82–87 % of the screen, from the middle onwards, on every geometry
  checked.
* **It does not hold at all under round-to-nearest.**
* The remembered-position design it replaced (`rel_pos`, `664d26e`…`420d76f`) did not have the
  feedback: the remembered position was the exact target, and rounding never accumulated.
* Reach:
  * It affects **only the tracked path**: scope Display, deltas without the unadjusted flag, and
    every H1 case.
  * The unlocked absolute path (`abs()`) has the same 1-pixel shortfall but does not feed it
    back, so it is a constant one-pixel offset up and left, not a drift.
* **Fix that is correct under both models** (P2): `n = ceil(u·65536/W)`. Verified exhaustively
  for W ∈ {1080, 1440, 1920, 2560, 4480, 7680}: zero mismatches under either model, and n never
  exceeds 65535.

**H2b (also UNVERIFIED): `GetCursorPos` may lag `SendInput`.** If the raw-input thread applies
the injected move asynchronously, two deltas injected back to back (a 1 kHz mouse, or a burst
released after a network stall) both read the same stale base. The second overwrites the first,
so motion is lost in proportion to the burst size. A low-level mouse hook in another process
(PowerToys Mouse Utilities, AutoHotkey, gaming or overlay software) would widen that window.
The fix in P2 (track the target, resynchronise only when someone else moved the cursor) removes
both H2 and H2b.

### 4.3 H1: the five ways a Chrome client ends up on the tracked path

`lets_os_curve(unadjusted) = unadjusted && scope == All` (`windows.rs:411`). The flag comes
from `input.js` `evFlags()`, which is sent only when `wantEvFlags()`, i.e. once a PONG has
delivered the capability bits.

1. **First ≈1 s + RTT of every connection.** `hostCaps = 0` until the first PONG, and the first
   PING is sent by `setInterval(…, 1000)` (`input.js:977`), not on open. So the flags byte is 0,
   deltas go down the tracked path, and when the PONG lands they flip to the relative path.
   **The pointer's gain changes about one second into every connection.**
2. **After every rebuild while the lock is held.** That covers a host-initiated reconfigure
   (epoch bump), demotion to `<video>`, `retryWithLegacyTransform`, and a trickle latch.
   `teardown()` sets `lockIsUnadjusted = false` (`input.js:1000`, `803e88f`). Pointer lock
   lives on `#stage` and survives the rebuild, so **no `pointerlockchange` fires** and nothing
   sets the flag again. **The rest of the session is on the tracked path**: no Windows curve
   (a 2–6× gain cut, by `420d76f`'s own estimate) plus H2. This lines up with the owner's
   "changing a device's settings…" reports, and with the known **5 s demotion defect**, which
   would put every affected session on the tracked path 5 s after connecting.
3. **Browsers without `unadjustedMovement`.** The page's comment says Safari < 18.4, Linux
   Chrome, and Firefox before 152. The owner's Mac is on 10.15, where Safari tops out at 15.x,
   so **a Safari client from that Mac is on the tracked path for every move** (§10 Q2).
4. **`PointerScope::Display`** ("Restrict cursor to this display"): by design, always tracked.
5. **A plain-lock retry** (`lockPlain`) after the options form threw or was rejected.

Cases 3–5 are intended, but they inherit H2. Cases 1 and 2 are defects.

### 4.4 The other hypotheses in detail

**H3, truncated locked motion.**

* Under lock, `onPointerMove` sums `coalescedOf(e)`, which keeps only the newest 8 samples.
* Where `pointermove` carries N > 8 coalesced reports, (N−8)/N of the motion is discarded:
  * a 1000 Hz mouse at a 60 Hz display gives about 16 per event, so **≈50 % lost**;
  * at 120 Hz about 8, borderline, and lost whenever a frame is late.
* With `pointerrawupdate` (secure contexts) the coalesced list is normally 1–2 samples, so it
  bites only under main-thread jank.
* Whether Chromium exposes `getCoalescedEvents` on non-secure `http://` pages decides how
  common this is. If it is `[SecureContext]` there, `hasCoalesced` is false on the LAN page and
  H3 is limited to HTTPS-with-jank and to non-Chromium engines. **UNVERIFIED, §10 Q4.**
* Separately, `clampI16(v|0)` discards fractional deltas every send. That matters only if
  `movementX` is fractional (high-DPR clients) or `rib.settings.sensitivity ≠ 1`.
* The old app used `e.movementX`, the browser's own sum, with none of this.

**H4, spurious button transitions.** Three routes.

* (a) **Unlocked, `d5cfe79`.**
  * A mouse moving with several coalesced samples goes into `pendingStrokes`.
  * `onPointerDown` deletes `pendingMoves` but **not `pendingStrokes`**, then sends `Down`
    (buttons = 1) on `reliable` immediately.
  * The flush later sends the stale batch (buttons = 0) on `fast`.
  * The host injects `Down` (press), then the batch:
    `push_mouse_pointer → button_transitions(1, 0)` gives **LEFTUP**. The next move with
    buttons = 1 gives **LEFTDOWN**.
  * One click becomes press–release–press. That is a double-click in most apps, or a drag that
    starts, drops and restarts.
* (b) **Locked, pre-existing.**
  * `rel` holds a pending delta (buttons = 0) when `pointerdown` arrives.
  * `Down` goes on `reliable` at once; the delta (buttons = 0) is flushed after it on `fast`.
  * The same LEFTUP / LEFTDOWN follows.
  * `39f00cb`'s rewrite makes this worse for a *late* delta: its word is replaced with the last
    **delta's** word, which predates a reliable `Down` already injected, so the rewrite itself
    synthesises the up.
* (c) **The random `select!`** between the reliable and fast queues reorders a `Down` against a
  fast frame sent just before it, even on a perfect network.
* All three exist because positional frames carry authority over button state. They should not
  (P4).
* `[old]` had (b) and (c), but not (a): the old unlocked mouse never batched.

**H5, the pointer leaving the display.**

* With scope All (the default since `d42f0df`) and the relative path, the locked cursor goes
  wherever Windows takes it, including the host's real monitors.
* There it disappears from the stream: `CursorProbe` reports it "left", and WGC is not
  capturing that monitor. The user sees a stream with no cursor, and clicks land on the host's
  own desktop.
* That is the owner's ruling, but it reads exactly like "the cursor got defocussed".
* The old app behaved the same way (relative, unclamped). Between `f957022` and `d42f0df` it
  was clamped.
* **Edge bug (pre-existing, in both trees)**: `pixel()` maps `nx = 1.0` to `r.left + r.width`,
  one column **past** the display.
  * Under nearest rounding, a tap or move at the client's right or bottom edge puts the host
    cursor on the neighbouring monitor.
  * Under truncation, H2's shortfall happens to cancel it.

**H6, lock, focus and rebuild interactions.**

* (a) **First click after any rebuild lands at the frozen lock point.**
  * The new session's `Injector` starts `mouse_relative: false` (`664d26e`).
  * The client does not resend `POINTERLOCK_STATE` on the new `reliable.onopen` (only
    `pointerlockchange` and `setSuspended(false)` send it).
  * A `POINTER_DOWN` that arrives before the first `MOUSE_DELTA` is treated as unlocked, and
    `positions_cursor(false, Down)` moves the cursor to the down event's `clientX/Y`, which the
    browser froze at the moment the lock was taken. The cursor teleports and the click lands
    there.
  * `[old]` started `mouse_relative: true` and did not have this.
* (b) **A mid-session dialog drops the lock, the keyboard lock and fullscreen** (`44f3f6d`). The
  one reachable mid-session is the **output-picker re-offer**:
  * when the host commands a sink the engine refuses, `queueOutputPicker` arms on
    `pointerdown`;
  * the user's next click on the stage opens the picker through `withModal`, which drops lock,
    keyboard lock and fullscreen.
  * Getting back needs another click, and that click is itself forwarded as a real click on the
    host before the lock is granted (the gesture handler runs in the capture phase and the
    `Down` is unlocked at that moment). `[old]` had the same re-lock-by-click behaviour.
* (c) **Alt-Tab / blur / Esc** drop the lock (browser behaviour). Re-entry needs a gesture, as
  above.
* (d) **The `<video>` swap does not drop the lock**: the lock target is `#stage`, not the media
  element. But a rebuild runs H1(2) and H6(a).
* (e) **Stuck keys and buttons across teardown are handled.** The old injector's `release_all`
  runs on thread exit and on PC `Closed`. One race: if the old PC's `Closed` arrives after the
  new session has injected a press, the old injector's `LEFTUP` releases it (global
  `SendInput`). Low likelihood.

**H7, the input gap per rebuild.**

* `teardown()` closes the channels at once. The new ones open after a full WHEP round trip.
* Everything sent in between is silently dropped (`rawSend` returns on `readyState !== 'open'`).
* A drag in progress is broken by the old injector's `release_all`.
* If the 5 s demotion fires on many sessions, this is a "glitch around five seconds in", every
  session.

**H9, cursor frames at the compositor's rate.**

* With the exemption, a moving cursor delivers every composed frame: 113 fps measured on a 60 Hz
  client.
* On a slow decoder the client's "drop to freshest and request a keyframe" rule fires. The
  request reaches the host as RTCP PLI or as `KEYFRAME_REQUEST` over the input channel
  (`eff862a`, 200 ms floor), and IDR bursts follow while the user is moving the mouse.
* The `CursorProbe` reads the cursor at **callback** time, not compose time. When the cursor
  stops, the frame that actually contains the final position can see "no change" and be
  cadence-dropped. The final position then waits for the next cadence frame (≤ 1 period with
  `autoFrameRate` off). Minor.

**H10, deltas dropped for age or overflow.**

* `is_supersedable(MouseDelta) = true`, so a delta queued more than 50 ms is discarded.
* `FAST_CAP` drop-oldest also discards deltas.
* Deltas are increments, so this is motion lost, not superseded: the same argument `39f00cb`
  made for the reorder gate.
* Triggers: clipboard and drop work on the injector thread, a slow `SendInput` under a
  low-level hook, a stalled machine.

**H8, transport.**

* At 16 workers the measured 1 kHz burst reached `SendInput` in full. Re-check with
  `SE_TRANSPORT_WORKERS=4` versus 16 only as a discriminator.
* Keys and clicks on `reliable` suffer head-of-line blocking under Wi-Fi loss. Recovery is fast
  while moves keep flowing (their SACK gaps trigger fast retransmit) and slow (RTO) when only
  keys are flowing.
* The 200 ms delayed SACK for lone packets mostly affects the client's congestion accounting,
  not delivery. Low.

### 4.5 The cursor drawn twice, or not at all

* **Twice**:
  * Unlocked (fullscreen unticked, or before the first lock, or a touch client with a mouse),
    the local browser cursor floats over the stream and the composited host cursor trails it by
    the full glass-to-glass latency. Same in the old app; nothing hides the local cursor unless
    pointer lock is unavailable (`acquirePointerLock` sets `cursor:none` only in that case).
  * `SE_CURSOR_OVERLAY=1` on a Windows build without the WGC cursor property could composite
    both. Not default.
* **Not at all**:
  * the cursor on a host monitor (H5);
  * Windows hides the pointer after touch input until the next mouse move ("touch mode"), and
    after typing with "hide pointer while typing";
  * `CursorProbe` treats `flags != CURSOR_SHOWING` (e.g. `CURSOR_SUPPRESSED`) as not showing,
    which affects only the gating decision, not what WGC draws.

---

## 5. Changes that look wrong or risky by reading alone

1. **`input.js` `teardown()` clears `lockIsUnadjusted` while the lock is still held** (`803e88f`).
   Its stated purpose ("a re-entry by a route that did not grant it cannot inherit the claim") is
   already served by clearing on `pointerlockchange` → unlocked.
2. **No PING on `reliable.onopen`; capability-gated flags** (`d03efe5` + `8e5d3c8`). The host's
   curve decision depends on a byte the client cannot send for the first second.
3. **Tracked path re-reads `GetCursorPos` and quantises with `round()`** (`301230d`). This
   produces drift under a truncating conversion, and a lost-motion race if the read is stale.
4. **`MouseDelta` is supersedable for age and overflow** (`35986dd`, kept by `da2c2ec`). The
   reorder gate was fixed for exactly this reason (`39f00cb`); the age gate and the queue were
   not.
5. **The late-delta button rewrite uses the last delta's word**, not the injector's current word
   (`39f00cb`, `mod.rs:540-545`).
6. **Positional frames carry button authority** (all backends, pre-existing). Combined with two
   queues and a random select, this is how a click becomes two.
7. **Unlocked mouse batching has no `pointerdown` barrier** (`d5cfe79`).
8. **The 8-sample cap is applied to the locked sum** (`d5cfe79`). The cap's rationale ("the host
   replays a batch one injection per sample") was obsolete an hour later (`7ce9d34`) and never
   applied to deltas.
9. **The new injector starts unlocked and the client never restates the lock** (`664d26e`
   together with the client's `setup`).
10. **`refresh_geometry` resolves the target by `\\.\DISPLAYn` name** every 500 ms (pre-existing).
    `CLAUDE.md` forbids addressing a display by that slot name for writes, because Windows hands
    it straight back out. If the name is reused or fails to resolve, `target = None` and absolute
    input maps across the **whole virtual desktop**, so a client's taps can land on the host's
    real monitors until the next refresh. Also, after a resolution or orientation change, the
    rect is stale for up to 500 ms.
11. **Injector work that can block** (clipboard, file drop) shares the thread with the
    latency-critical pointer path.
12. **Touch and pen are never reorder-dropped**, so reordered ink is injected out of order.
13. **`SE_INPUT_TRACE` does not log the delta path, the flag, or button transitions.** It cannot
    tell the tracked path from the relative one, which is the first thing to know (P8).

---

## 6. Proposed changes (not applied)

Each one gives what it should fix and what it could break. Input code is off limits in this job,
so these are for a later job (§8) to apply **one at a time, measured**.

### P1: keep the lock claim with the lock, restate it on every connection, ask for capabilities at once (client)

Fixes H1 cases 1 and 2, and H6(a).

```js
// input.js teardown(): stop clearing the lock claim; it belongs to the lock, not the connection.
-    setHostCaps(0);
-    lockIsUnadjusted = false;
+    setHostCaps(0);
+    if (!document.pointerLockElement) lockIsUnadjusted = false;

// input.js reliable.onopen: restate lock and focus for the new injector, and PING now.
     reliable.onopen = () => {
       refreshSize();
       sendResize();
+      sendState(OP.FOCUS_STATE, document.hasFocus ? document.hasFocus() : true);
+      sendState(OP.POINTERLOCK_STATE, !!document.pointerLockElement);
+      try { sendPing(); } catch (_) { noteSuppressed(); }
       ...
```

* Could break:
  * a lock lost *during* the rebuild gap (listeners are inactive then) would keep a stale `true`
    claim. Mitigated by the `pointerLockElement` test at open.
  * `FOCUS_STATE true` is harmless to the host (no arm acts on it).
* Golden vectors and `test-client-stats.mjs` assert the first frames carry no tail; those tests
  need updating, not the protocol.

**P1b (host, belt and braces):** treat `unadjusted` as sticky per injector once seen, so a
connection's first-second deltas use the relative path whenever the previous connection of the
same device had it. More state. Optional.

### P2: exact absolute conversion, plus a tracked target that resyncs only when someone else moved the cursor (host)

Fixes H2 and H2b, and makes `abs()` exact.

```rust
/// Normalised 0..=65535 coordinate that Windows maps back to exactly `px`, whether it
/// truncates or rounds: ceil(u * 65536 / W).
fn norm(px: i32, origin: i32, extent: i32) -> i32 {
    let u = (px - origin).clamp(0, extent - 1) as i64;
    let w = extent.max(1) as i64;
    (((u << 16) + w - 1) / w).min(65535) as i32
}

// In Injector: last_target: Option<POINT> (where we sent it), prev_read: Option<POINT>
// (what GetCursorPos said just before that injection).
fn advance_rel(&mut self, dx: i32, dy: i32) -> POINT {
    let now = cursor_pos();                         // GetCursorPos
    // The cursor is "ours" if it is where we put it (the injection landed) or still where it
    // was before we moved it (the injection has not been applied yet, H2b). Anything else
    // means the host's own mouse or another session moved it, and the real position wins,
    // which is what 301230d wanted.
    let ours = matches!(self.last_target, Some(t) if now == t)
        || matches!(self.prev_read, Some(p) if now == p);
    let base = match self.last_target { Some(t) if ours => t, _ => now };
    let b = self.pointer_bounds();
    let p = POINT { x: (base.x + dx).clamp(b.left, b.left + b.width as i32 - 1),
                    y: (base.y + dy).clamp(b.top,  b.top  + b.height as i32 - 1) };
    self.prev_read = Some(now);
    self.last_target = Some(p);
    p
}
// mouse_delta / abs(): replace `(v.clamp(0,1) * 65535).round()` with norm(px, virt.left, virt.width).
// Clear both fields on PointerLock(false) and whenever the relative (OS-curve) path is taken.
```

* With `norm` exact, a landed injection reads back as exactly `last_target`. The "not landed
  yet" arm covers a stale read (H2b), so neither rounding nor lag can feed back into the next
  move.
* Could break:
  * a host-side move that happens to end exactly on `last_target` or on `prev_read` is
    ignored. That costs one step of base and is harmless.
  * Mixing the relative and tracked paths in one session needs the two fields cleared on every
    switch, as noted in the code comment above.
* Needs the PointerScope clamp tests updated, plus a new pure test of `norm` against both
  models (the table in §4.2).

### P3: locked motion from the dispatched event, with the remainder carried (client)

Fixes H3.

```js
-      let mx = 0, my = 0;
-      const c = coalescedOf(e);
-      for (let i = 0; i < c.length; i++) { mx += c[i].movementX || 0; my += c[i].movementY || 0; }
-      if (mx === 0 && my === 0) { mx = e.movementX || 0; my = e.movementY || 0; }
+      // The dispatched event's movement is already the sum of everything it coalesced.
+      let mx = e.movementX || 0, my = e.movementY || 0;
...
-  function sendMouseDelta(dx, dy, buttons) { ... clampI16(dx) ... }
+  // carry the fractional remainder so sub-pixel motion accumulates instead of vanishing
+  let fracX = 0, fracY = 0;
+  function sendMouseDelta(dx, dy, buttons) {
+    const tx = dx + fracX, ty = dy + fracY;
+    const ix = clampI16(Math.trunc(tx)), iy = clampI16(Math.trunc(ty));
+    fracX = tx - ix; fracY = ty - iy;
+    ...
```

* Could break: an engine whose dispatched `movementX` is *not* the coalesced sum. None is known;
  `pointerrawupdate` and `pointermove` both define it as the sum. Keep the coalesced-sum path
  behind a probe if one appears.
* Update `test-client-input.mjs`.

### P4: buttons belong to Down/Up/Cancel only; a pointerdown barrier on the client

Fixes H4 (a), (b) and (c).

* Host, all backends: `mouse_delta`, `pointer_batch` and `mouse_pointer` for Move/Enter/Over
  stop calling `button_transitions`. Only `Down`/`Up`/`Cancel` (and `release_all`) change
  `self.buttons`. Delete the `last_delta_buttons` rewrite in `mod.rs`.
* Client, `onPointerDown`/`endPointer`: before sending the button frame, flush `rel` (locked)
  and the pointer's `pendingStrokes` entry (unlocked) with the **pre-transition** button word,
  then send `Down`/`Up`.
* Could break:
  * a client that loses a `pointerup` (no pointer capture) leaves the host button down until
    the next Down/Up/blur. Pointer capture is set on down, and `release_all` runs on blur, so
    this is bounded.
  * The relayed old client page (cloud relay) still sends Down/Up, so it is unaffected.

### P5: never drop motion; coalesce it (host)

Fixes H10.

* `is_supersedable(MouseDelta) → false` for the age gate.
* Instead, when an old delta is picked up, fold it into a pending `(dx, dy)` and inject the sum
  with the next delta (or on the next idle tick).
* On a full fast queue, drop the oldest *absolute* move, never a delta. If only deltas remain,
  merge the two oldest.
* Could break: a long stall followed by a large jump, which is correct motion but surprising.
  The 50 ms cutoff could stay as "inject the sum at once" rather than "drop".

### P6: `pixel()` maps into the display, not one past it (host)

Fixes the edge escape in H5.

```rust
- x: r.left + (nx.clamp(0.0, 1.0) * r.width as f32).round() as i32,
+ x: r.left + (nx.clamp(0.0, 1.0) * (r.width.max(1) - 1) as f32).round() as i32,
```

Same for y and for the `None` branch. Could break: nothing visible (a sub-pixel scale change).

### P7: client clipboard and drop work off the injector thread (host)

Fixes H10 triggers and §5.11.

Give the reliable queue's `Clipboard`/`Drop` arms to a small `bulk` worker thread (or a
`spawn_blocking`). `paste_hotkey` must still be ordered after the clipboard set, so the bulk
worker sends the Ctrl+V back through the reliable queue.

Could break: ordering between a paste and keys typed after it. Use a sequence or a completion
channel.

### P8: make the path visible (diagnostics only)

* Add `delta_path=relative|tracked` and `flag=unadjusted` to the `SE_INPUT_TRACE` line, and log
  `button` transitions as `btn=down/up`.
* Count `tracked_deltas`, `relative_deltas` and `spurious_button_up` in telemetry.
* Log one line per session whenever the delta path flips.

Costs nothing when the trace is off. This is the change to make **first** in the later job: it
turns H1, H2 and H4 from inference into a log line.

### P9: resolve the input target by the display's stable identity (host; G04 territory)

`refresh_geometry` should resolve through the same device-interface path that
`pick_owned_display` uses, and keep the last good rect (not `None`) on a failed lookup. Fixes
§5.10.

### P10 (owner's call): hide the local cursor over the stage when the stream carries the host cursor

```css
#stage.live:not(.locked) { cursor: none; }
```

Removes the doubled cursor in unlocked mode. **This changes how the page looks** and removes the
zero-latency local cursor, so it needs an owner decision.

### P11 (capture, G01; measure first): bound cursor-exempt frames by the client's refresh rate

Cap the exemption at the client's advertised refresh (or a window just under its period) rather
than the compositor's rate, so a 60 Hz client is not fed 113 fps. `6faa359`'s data says a
bounded window *below* the compose interval was worse, so the bound must be at the client's
period, not under it. Measure painted fps and decoder backlog on a slow client.

---

## 7. Manual test plan (the owner, by hand)

Ordered so the **first ten minutes** separate the top hypotheses. Nothing here needs code
changes.

**Common setup**

* Host: the GUI or `ScreenExtend serve -v` built from `latency-overhaul-new`, with
  `SE_INPUT_TRACE=1` set in the host's environment.
  * PowerShell: `$env:SE_INPUT_TRACE='1'; & .\ScreenExtend.exe serve -v`
  * The trace lines show in `serve -v` output or `ScreenExtend logs`.
* Client: **a second device** (a laptop) with a physical mouse, ideally a 1000 Hz gaming mouse
  with a polling-rate switch, in **Chrome**. Join with the **fullscreen box ticked** so pointer
  lock engages.
* Device settings: default (pointer scope All, i.e. "Restrict cursor to this display"
  unticked), audio off.
* On the virtual display: open **Paint**, zoom to 800 %, and **Notepad**.
* Optional: a tiny local HTML page on the host that logs `mousedown`/`mouseup`/`dblclick` with
  timestamps. Opening a page is fine; it injects nothing.
* Keep a terminal with `ScreenExtend status --json` handy. The fields to read are
  `degradation.input_queue_drops`, `input_stale_drops`, `input_reorder_drops`,
  `input_parse_errors`, `keyframe_requests`, `client_keyframe_requests`,
  `frames_skipped_cadence`, the `input_recv/queue/inject` stages, and the client stats `flags`
  (bit 3 = pointer lock).

### First ten minutes

| Min | Test | Do | Watch for | Means |
|---|---|---|---|---|
| 0–2 | **T1: speed at connect and after an edit (H1)** | Join. Within the first second, sweep the mouse a fixed hand distance (e.g. 10 cm against a ruler). Wait 5 s and repeat. Change **video quality** on the dashboard (a stream-class edit, so a rebuild). When the picture is back, repeat the sweep. | The cursor covers a different screen distance for the same hand distance: a jump at ~1 s, then slower after the edit | Different at ~1 s: H1 case 1. Slower after the edit and staying slow until you press Esc and re-lock: **H1 case 2 confirmed**. Discriminator: on the host, move **Settings → Mouse → pointer speed** from 10 to 18 and repeat. If the remote cursor changes speed, you are on the **relative** path; if not, the **tracked** path |
| 2–4 | **T2: one-pixel drift (H2)** | Tick "Restrict cursor to this display" (forces the tracked path). In Paint at 800 %, creep the mouse slowly **right**, then slowly **left**, then draw slow small circles. | Rightward or downward creep does not move the cursor while leftward or upward creep does; circles drift up-left | **H2 under the truncating model.** If the motion is symmetric, Windows rounds to nearest and H2 is ruled out; untick and move on |
| 4–6 | **T3: lost motion under lock (H3)** | Untick restrict. Join over `http://` (the QR) and sweep 10 cm; rejoin over `https://…:8443` and sweep 10 cm. With a polling switch, repeat at 125 Hz and 1000 Hz. | `http` travels markedly less than `https` at 1000 Hz but not at 125 Hz | **H3** (coalesced-sample truncation). Equal: H3 does not apply to this browser (§10 Q4) |
| 6–8 | **T4: clicks while moving (H4)** | In the click-logger page, or in Notepad on a line of words, click **while the mouse is moving**, 20 times, locked. Then untick fullscreen (unlocked mode) and repeat with a 1000 Hz mouse. | Extra down/up pairs, `dblclick` events, words getting selected, a drag that starts on a plain click | **H4.** Locked only: route (b). Unlocked with a fast mouse only: route (a), i.e. `d5cfe79` |
| 8–9 | **T5: first click after an edit (H6a)** | Change refresh rate on the dashboard. When the picture returns, **do not move the mouse**; click once. | The cursor jumps (to where the lock was taken, often mid-screen) and clicks there | **H6(a)** |
| 9–10 | **T6: the edges (H5)** | Locked, scope All: push the cursor past each edge of the virtual display. Then unlocked (no fullscreen): move to the extreme right and bottom edge of the picture and click. | The cursor vanishes from the stream and the host's own monitor reacts; the unlocked edge click lands on the neighbouring monitor | Locked vanish: H5 (by design for All). Unlocked edge spill: the `pixel()` off-by-one |

### Then, in any order

* **T7: defocus sources (H6 b/c).** Note every time the lock drops by itself. Check whether an
  output picker or dialog appeared, whether the audio output was changed on the host just
  before, and whether the client window lost focus (notifications, Alt-Tab). With `?debug` on
  the client URL, the console logs `[audio] output re-selected` and `falling back to …`.
* **T8: the 5 s demotion (H7).** Join with `?debug` and keep the DevTools console open. If
  `falling back to <video>: no WebCodecs frame within 5s` appears, note whether input stuttered
  at that moment and whether T1's speed changed then. Both are H1(2) + H7.
* **T9: keyboard.** In Notepad, type fast for 30 s, hold a key for 3 s, and use Shift/Ctrl
  combinations. Check for missing characters, repeats after release, and stuck modifiers after
  Alt-Tab on the client. Then press Ctrl+V with text copied on the client: the **host's**
  clipboard is pasted (pre-existing, §3.2).
* **T10: wheel.** Scroll a long document, wheel notches and a trackpad. Lost notches on Wi-Fi
  are the unreliable channel.
* **T11: same machine vs second device.** Repeat T1 and T4 joining from a browser on the host
  itself. Same-machine removes the network. If T4 still fails, it is H4(a)/(c), not the network.
* **T12: capture-side cursor.**
  * Sweep the cursor over a static desktop and over a playing video. Watch for stutter or the
    cursor arriving late when you stop.
  * Restart the host with `SE_CURSOR_OVERLAY=1` and repeat. If the overlay is smoother over
    static content but worse over video, that matches UX-INPUT's numbers.
  * With `status --json`, compare `frames_skipped_cadence` and `keyframe_requests` before and
    after a 10 s sweep. Keyframe requests rising during sweeps, mostly as
    `client_keyframe_requests`, points to H9.
* **T13: runtime width.** Restart with `SE_TRANSPORT_WORKERS=4`, then `=16`, and repeat T1's
  sweep at 1000 Hz. A visible difference means transport starvation is still in play (H8);
  `input_queue_drops` may stay 0, because the stall is before `route()` stamps.
* **T14: lossy Wi-Fi.** On a busy 2.4 GHz network, type and click while a large window animates
  (an IDR-heavy scene). Key-ups arriving in bursts means reliable head-of-line blocking (H8).
* **T15: the owner's Mac** (10.15). Join from Safari and from Chrome if installed. Safari has
  no `unadjustedMovement`, so it is on the tracked path for every move: run T2 there without
  ticking restrict.

**Switches available today:**

* Host:
  * `SE_INPUT_TRACE=1` (per-event line);
  * `SE_CURSOR_OVERLAY=1` (host-drawn pointer);
  * `SE_TRANSPORT_WORKERS=N` (runtime width);
  * per-device "Restrict cursor to this display" (forces the tracked path);
  * "Remote control" (live);
  * `autoFrameRate` (enables the dirty-region gate).
* Client:
  * the fullscreen checkbox (lock on/off; URL `fs=0/1`);
  * `?debug` (console diagnostics);
  * `?present=track`;
  * `localStorage['rib.settings'] = '{"sensitivity":…, "accel":…}'` (the client-side curve on
    the tracked path; useful to compensate H1 by hand as a confirmation).

---

## 8. Instrumented plan for a later job that may inject input

**Precondition: the owner must explicitly lift the ban on injecting input before any of this
runs, on a machine nobody is using.**

### 8.1 What `scripts/measure-input.ps1` does, and what it lacks

* Takes the host's single-instance lock. It **identifies and stops the owner's GUI** and
  relaunches it at the end.
* Backs up and rewrites `devices` in the owner's `config.json` (restored by trap since
  `cabf63a`).
* Launches Chrome with CDP and joins.
* Phases:
  1. **hold**: DOM event → `RTCDataChannel.send`, via a hooked `send` and a capture-phase
     listener;
  2. **forward**: client send → host `SendInput`, paired against `SE_INPUT_TRACE` wall-clock
     lines;
  3. **frames**: painted fps on a static desktop for no motion, a client-pointer sweep and a
     host-pointer sweep (the host pointer is moved by `SendInput` from PowerShell);
  4. **keys**: dispatched into the page.
* Needs: the binary path, ports 8080/8443 free, DPI-aware PowerShell (since `af59c4a`), an idle
  machine, `-Load` optional.
* **It never exercises pointer lock, `MOUSE_DELTA`, either curve path, button correctness or
  motion accuracy.** Every H1–H6 case is outside its reach. It also cannot run beside the
  owner's GUI. Use the handler instead.

### 8.2 Additions to `scripts/test-handler/`

The handler currently refuses any request that mentions input (`README.md`), and
`lib/cdp.mjs` refuses `Input.*`. Add these behind an explicit `--allow-input` flag that the
owner sets.

**`input_path` request: the client→host leg and its correctness, without a browser event.**

* Client: an injected test hook writes protocol frames **directly onto the page's own `fast` and
  `reliable` channels**. It uses the real `RemoteInput` buffers, so it is byte-identical to the
  shipped encoders, and it scripts:
  * N × `MOUSE_DELTA(+1,0)`, N × `(−1,0)`, N × `(+3,0)`, N × `(−3,0)`, each with `flags = 0`
    and `flags = UNADJUSTED`, under scope All and scope Display;
  * a 1 kHz burst of 1000;
  * click-while-moving sequences: delta(buttons 0), Down, delta(buttons 0), delta(buttons 1),
    Up;
  * the same with absolute `POINTER_MOVE`/batches.
* Host: run with the P8 trace (path, flag, button transitions) and sample `GetCursorPos` (a
  read, not an injection) after each scripted step.
* Report:
  * displacement error per direction and path (px);
  * motion lost per burst;
  * spurious button transitions;
  * forward latency p50/p99 (client send stamp vs host trace wall clock);
  * `input_*` counters.
* **Pass:**
  * displacement error **0 px** over 1000 single-pixel steps in each direction on both paths
    (tracked path compared against the target, relative path compared against the curve with
    the curve turned off by `SPI_SETMOUSE` acceleration off for the test, restored after);
  * 1000/1000 of a 1 kHz burst injected, with no motion lost;
  * **zero** spurious button transitions;
  * forward p50 ≤ 2 ms and p99 ≤ 5 ms same-machine, ≤ RTT + 5 ms over LAN.
* Run it on the current head first, then with each of P1–P5 applied alone.

**`pointer_lock` request: the page's own lock state machine.**

* Headless Chrome cannot hold a real pointer lock, so drive it in **headful Chrome on a
  dedicated desktop** with a real `Input.dispatchMouseEvent` click (permitted under the lifted
  rule) to grant the lock.
* Then trigger, in turn: a reconfigure, the `<video>` demotion, an output-picker re-offer
  (command a refused sink), and Esc.
* After each: read `document.pointerLockElement`, the next `MOUSE_DELTA`'s flags byte (from the
  hooked `send`), and whether `POINTERLOCK_STATE` was sent on the new channel.
* **Pass:**
  * the flags byte equals the granted state on the first delta of every connection;
  * `POINTERLOCK_STATE` restated within one RTT of every `reliable.onopen`;
  * the first click after a rebuild does not move the cursor.

**`cursor_capture` request: motion-to-photon for the real cursor.**

* Host: move the cursor with `SetCursorPos` along a scripted path on the test's virtual display
  (sweep, isolated moves, stop-and-hold) at 60/125/1000 Hz, stamping each move's host time.
* Client: the existing paint hook reads the painted frame and **locates the cursor sprite** by
  template match against the default arrow (or a custom high-contrast cursor installed for the
  test and restored after). It records position over time.
* Report:
  * cursor motion-to-photon p50/p99;
  * cursor painted fps during a sweep;
  * frames between "stop" and the final position being painted;
  * any frame with **two** cursors (local plus composited, in headful unlocked mode) or **none**
    while the cursor is on the display.
* Run with the default and with `SE_CURSOR_OVERLAY=1`, over a static and an animated
  background, and at a 60 Hz and a 120 Hz client.
* **Pass:**
  * painted fps during a sweep ≥ min(client refresh, 60);
  * motion-to-photon p50 within one frame of the moving-window stand-in's p50 in the same run;
  * final position painted within one client frame period of the stop;
  * zero double or missing cursors.

**`input_load` request.** Repeat `input_path` while the standard "large area changing every
frame" scenario runs and IDRs are frequent, at `SE_TRANSPORT_WORKERS` 4 and 16. **Pass:** no
change in forward p99 beyond the noise figure.

---

## 9. Findings in other groups' areas

* **Client page (G05/G07):**
  * Every rebuild path (`reconfigure`, `fallbackToVideo`, `retryWithLegacyTransform`,
    `trickleFailed`) costs input correctness: H1 case 2, H6(a) and H7, through `input.js`
    `teardown()`/`setup()`. Fixing the **5 s demotion** defect removes one rebuild from every
    affected session.
  * The **output-picker re-offer** (`queueOutputPicker`, armed on `pointerdown` after a refused
    sink) is a mid-session modal that drops lock, keyboard lock and fullscreen on the user's
    next click.
  * The first PING waits a full second.
* **Capture (G01):**
  * The cursor-moved exemption (`6faa359`) lets cursor motion drive the stream at the
    compositor's rate (113 fps measured) regardless of the client's refresh (H9).
  * `CursorProbe` samples the cursor at callback time, so a frame carrying the cursor's final
    resting position can be cadence-dropped.
  * `flags == CURSOR_SHOWING` is an equality test against a bitfield.
* **Transport (G03):**
  * `route()` stamps `host_recv_ns` after the scheduler wait, so a starved runtime is invisible
    to every host stage. A blind spot already noted in UX-INPUT, but still unaddressed.
  * The **IDR-every-~250 ms** storm: compare `client_keyframe_requests` with `keyframe_requests`
    in `status --json`. If most are client requests over the input channel (`eff862a`, 200 ms
    floor, plus the gate), the storm is the client's resync rule and not a host bug.
  * webrtc-sctp's 200 ms delayed SACK for a lone packet (`AckMode::Normal`) is worth a look for
    key-only traffic.
* **Session and display (G04):** the injector resolves its target by the `\\.\DISPLAYn` slot
  name every 500 ms. That is the slot-name pattern `CLAUDE.md` forbids for writes, and a failed
  lookup maps input across the whole desktop (§5.10, P9).

---

## 10. Unverified, and questions for the owner

Nothing was asked during the job. Each question gives the assumption I made instead.

**Unverified (needs injection or a real client to settle):**

1. Whether Windows' absolute-mouse conversion truncates or rounds on this build (decides H2;
   §4.2). Assumed: truncates. That is commonly reported, but not confirmed here.
2. Whether `GetCursorPos` reflects a `SendInput` move synchronously (H2b). Assumed: not
   guaranteed.
3. Whether Chromium exposes `getCoalescedEvents()` on non-secure `http://` pages, and how many
   coalesced samples `pointerrawupdate` carries under jank (decides how common H3 is).
4. Whether Chrome's `movementX` is fractional on high-DPR clients (the truncation in
   `clampI16`).
5. Whether Chromium fires `pointerover`/`pointerenter` while locked. If it does, `mouse_pointer`
   sets `mouse_relative = false` and the next locked `Down` positions the cursor at the frozen
   point, the same symptom as H6(a).
6. All capture-side cursor behaviour (H9, double or missing cursor) on a real second device. The
   moving-window stand-in does not exercise `CursorProbe`.
7. Every macOS input change (`7ce9d34`, `34cfafb`, `f957022`, `d42f0df`, `301230d` macOS halves)
   is uncompiled on this machine.
8. That the 5 s `<video>` demotion fires on the owner's real sessions (given to me as a known
   defect; not observed here).

**Questions I would have asked:**

* **Q1.** "Laggy / slow cursor": does it mean the cursor *trails* the hand in time, or needs
  *more hand movement* than the old app? The old app curved twice, so even a correct single
  curve feels slower. Assumed: both, and both are covered (H1, H3 for distance; H8, H9, H10 for
  time).
* **Q2.** Which client browser and OS do you mostly use: the 10.15 Mac with Safari or Chrome, a
  Windows laptop, a tablet? Assumed: Chrome on a laptop, with the Mac as a second client. A
  Safari client is always on the tracked path (H1 case 3).
* **Q3.** Do you join over the `http://` QR link or over HTTPS / the cloud link? Assumed:
  `http://` on the LAN (the QR default). This changes which pointer event fires (H3).
* **Q4.** Do you use fullscreen (pointer lock) or the windowed mode? Assumed: fullscreen. In
  windowed mode, H1–H3 do not apply and H4(a), H5's edge bug and the doubled cursor do.
* **Q5.** Does "cursor being defocussed" mean (a) the pointer lock or fullscreen dropping on the
  client, (b) the cursor disappearing off the edge of the virtual display, or (c) keyboard focus
  leaving a window on the host? Assumed: any of the three; H5, H6 and T6/T7 cover them.
* **Q6.** Is pointer scope "All" still the wanted default, given that a locked cursor can wander
  onto the host's monitors and vanish from the client's view? Assumed: yes (your ruling in
  `d42f0df`). P6 fixes only the unlocked edge bug.
* **Q7.** May the client page hide its own cursor over the stage in windowed mode (P10)? It
  changes how the page looks. Assumed: no, until asked.
* **Q8.** Do you run PowerToys, AutoHotkey, a gaming-mouse utility or anything else with a
  global mouse hook on the host? It would widen H2b and H10. Assumed: unknown.
* **Q9.** `af59c4a` (09-11) recorded that on this machine "the cursor is pinned at the primary
  display's centre and genuinely does not move, which is something else running here, not the
  build". Was something else holding the cursor at that time (another tool or session)? If it
  recurs it would look like an input glitch. Assumed: a transient from concurrent agent work.
* **Q10.** Should clipboard paste from the client (Ctrl/Cmd+V) carry the client's clipboard? It
  never has (§3.2): the key's `preventDefault` cancels the paste event. Assumed: wanted, but out
  of scope.
* **Q11.** When you change a setting mid-session, do you notice the pointer speed changing
  afterwards (T1)? If yes, H1 case 2 is confirmed with no injection at all.

**What I assumed in place of measuring:** that the arithmetic in §4.2 reflects the code exactly
(it re-implements the f32 normalisation and Rust's `round`); that crossbeam's `select!` is
random among ready channels (its documented behaviour, which both the code comments and
`INPUT-PROTOCOL.md` rely on); and that webrtc-sctp 0.17.1's delayed-ACK logic is as read from
the registry source.

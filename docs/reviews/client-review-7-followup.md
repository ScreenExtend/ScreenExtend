# Client review 7 — `w11-followup` (trickle ICE + fast-channel sequence)

Cold review, single pass, all angles. Read-only: nothing was modified, nothing was built, no
browser was driven. Line numbers are from the worktree
`.claude/worktrees/w11-followup` at `eec5357`.

Scope reviewed:

```
git diff 3a137a7e...HEAD -- src-tauri/src/streamer/static/ \
    src-tauri/src/streamer/webrtc_session.rs src-tauri/src/streamer/server.rs \
    scripts/test-client-*.mjs
```

Two features, derived from the code: (a) trickle ICE — the WHEP offer is posted the moment
`setLocalDescription` resolves and candidates follow over `PATCH /whep/{id}`; (b) a `u16`
reorder sequence appended past the stamp on fast-channel input frames. The host halves of both
came in with the `w1-closeout` merge; this branch is the client adopting them.

Counts: **0 blockers · 1 major · 7 minor · 5 nits.**

Both Node harnesses were executed and pass (33 ICE assertions, 28 stats assertions), and both
lift the real functions out of the shipped source rather than restating them.

---

## Findings

### 1. [MAJOR] A systematically rejected `PATCH` silently kills the session, with no latch and no fallback

`src-tauri/src/streamer/static/index.html:1176-1198`

`failStreak` is reset only by a `2xx` (`:1191`). `requeue()` requeues on the first failure and,
from the second consecutive failure onward, returns without requeueing (`:1178`). After that the
counter never comes back down, so **every** subsequent batch is PATCHed once, fails, and is
dropped — permanently, for the rest of the session.

Because `canTrickle` skipped `waitForIceGathering` entirely (`:1943-1954`), the offer that was
posted carries **no candidates at all**. If PATCH is systematically rejected — a reverse proxy
that 405s the verb, a captive portal, a WAF that blocks a non-standard `Content-Type`, a host
whose `/whep/{id}` route is unreachable for any reason — the host ends up with zero remote
candidates and ICE can never connect. The only exit is `pc.onconnectionstatechange === 'failed'`
→ `linkLost('Disconnected, reconnect to continue.')` (`:1932`). `trickleOff` is **not** set on
this path, so a manual reconnect retries trickle and fails identically. The page is bricked on a
network where the pre-change client worked.

Why this is a real asymmetry rather than a theoretical one: the sibling failure mode — the host
advertising `trickleIce` and returning no `Location` — *is* latched and rejoined once
(`:2013-2018`). The author clearly identified the "host lied about trickle" class; this is the
same class arriving one round trip later, and it is unhandled. `docs/manual-tests/client-page.md:144`
even names "no PATCH at all against a host whose `/net-config` says `trickleIce: true`" as a
finding, but there is no code that turns it into one.

**Fix.** In `requeue()`'s give-up branch (and immediately on a status that means "this verb will
never work" — 405, 501, 403), set a flag the page can read; in `start()`, treat it the way the
missing-`Location` branch is treated: `trickleOff = true`,
`degrade(DEG.TRICKLE_ICE_UNAVAILABLE)`, `teardownSession()`, one `start(preferWebCodecs)`. The
`trickleOff` latch already guarantees that cannot loop. A cheaper stopgap: arm a timer when
`open()` is called and, if `sentCount === 0` after ~1.5 s, take the same rejoin.

### 2. [MINOR] Degradation bit 22 has no name on the host, so it never reaches the log

`src-tauri/src/streamer/telemetry.rs:457`, `:483`; `index.html:219`

The client added `TRICKLE_ICE_UNAVAILABLE: 1 << 22`. Host-side, `CLIENT_DEGRADED_NAMES` is
`[&str; 22]` — indices 0..=21 — and `client_degraded_list` builds its output by
`.iter().enumerate()` over that array. Bit 22 is therefore filtered out and never appears in the
`degraded=…` line the session summary prints (`telemetry.rs:685-688`).

`docs/TELEMETRY.md:456` documents bit 22, and `docs/manual-tests/client-page.md:143-145,245`
tells the tester to read bit 22 to decide whether trickle engaged. As shipped, the host log
cannot answer that question — only the browser console can. `telemetry.rs` is not in this
branch's diff at all, so the two halves were landed without the name table being widened.

**Fix.** Append `"no_trickle_ice"` and widen the array to `[&str; 23]`.

### 3. [MINOR] The trickle rejoin throws away the device token the accepted join just issued, and abandons a fully materialised host session without `/leave`

`index.html:2003-2019` vs `:2021-2022`

The missing-`Location` branch `return`s at `:2018`, two lines before `X-Device-Token` is read.
The first POST was **accepted** — the host validated the OTP, minted a token
(`server.rs:980`), created the virtual display, ran the whole pipeline and `activate_whep`. The
client discards the token and rejoins presenting the *old* `storedToken` (often empty) plus the
same OTP.

That happens to work only because `SessionAuth::validate` (`server.rs:932`) is not single-use;
if the OTP ever becomes one-shot, this branch turns into a hard `401 Invalid session ID or OTP`.

Separately, `teardownSession()` (`:1232-1248`) closes `pc` but sends no `/leave`, so the
abandoned first session sits on the host until `disconnect_grace` (default 10 s) expires, and
the whole display-create/pipeline-start cost is paid twice. The existing
`retryWithLegacyTransform`/`fallbackToVideo` paths have the same shape, so this is not a new
class — but those never had an accepted server-side join behind them, and this one does. The
host now exposes `DELETE /whep/{id}`, which exists precisely for this.

**Fix.** Hoist the `X-Device-Token` read above the `localTrickle` block, and fire
`navigator.sendBeacon('/leave')` (or a `DELETE` to `resourceUrl` when there is one) before
`teardownSession()`.

### 4. [MINOR] `DELETE /whep/{id}` ignores its own resource id

`src-tauri/src/streamer/server.rs:846-853`

The handler binds `Path(_id)` and discards it, calling `process_leave(&state, &peer.ip())`.
Consequences: `DELETE /whep/<any-string-at-all>` tears down the caller's own session; a session
keyed by a cloud `client_id` rather than an IP can never be addressed by this route at all; and
the resource id — the whole point of RFC 9725 §4.4 — carries no authority. Nothing in the client
calls it (the page uses `POST /leave`, `index.html:968`), so this is untested surface that reads
as if it were doing an id lookup.

**Fix.** Resolve the session from `id` through the same registry `whep_patch` uses and reject a
mismatch with `404`, rather than trusting the peer IP.

### 5. [MINOR] `PATCH /whep/{id}` is the only route with no peer binding and no rate limit

`server.rs:829-842`, `webrtc_session.rs:2295-2298`

`whep_patch` takes neither `State` nor `ConnectInfo`. The sole guard is that
`mint_resource_id()` is 128 bits of `rand::random()`, i.e. a capability URL — but it is handed
out in a `Location` header that travels in cleartext on the HTTP listener (port 8080), which is
the listener most LAN joins actually use. Anyone who observes or guesses it can add ICE
candidates to a live session, which is a media-redirection primitive, and can do so unbounded:
there is no per-id counter, no attempt limiter (contrast `OtpLimiter` on `/whep`), and one 64 KiB
body can carry several hundred candidate lines, each awaited individually
(`webrtc_session.rs:2372-2382`).

**Fix.** Record the joining peer's normalised IP alongside the `Weak` in `TRICKLE_SESSIONS` and
compare it in `whep_patch`; cap candidates per fragment and fragments per session.

### 6. [MINOR] A transient `/net-config` failure is cached for the life of the page

`index.html:996-1012`

`getNetConfig()` memoises `netConfigPromise` including its *failure* result — both the network
rejection (`:1003-1006`) and `!r.ok` (`:1000-1002`) resolve to `{}` and that `{}` is cached
forever. Two consequences: trickle is permanently off for the page (`:1639`), and `httpsUrl()`
is permanently pinned to the 8443 default (`:2072`). The previous code fetched `/net-config`
fresh inside `httpsUrl()` on every call, so this is a small behavioural regression on a flaky
first load — and the prime now runs at module scope (`:1012`), i.e. at the moment the page is
most likely to race the server coming up.

**Fix.** Null `netConfigPromise` in the rejection and `!r.ok` branches so the next caller retries;
keep the memo only for a successful body.

### 7. [MINOR] The Rust "byte for byte" fragment test is a reimplementation of the client body, not the client body

`src-tauri/src/streamer/webrtc_session.rs:2244-2262`

`the_client_fragment_shape_parses_whole` documents itself as "the body `trickleFragment()` in
`static/index.html` actually writes, byte for byte" and then hard-codes a string literal. It
reads nothing from `static/index.html`, so the client can change its body — a different `m=`
line, a moved `a=mid:`, a missing `\r\n` — and this test stays green. It is the same defect
already logged as `findings/minor-tests-audio-seqgate-tests-a-reimplementation.md`.

Credit where due: the *client* side is genuinely pinned. `scripts/test-client-ice.mjs:44-58`
lifts `TRICKLE_MAX_BATCH`, `localIceUfrag`, `trickleFragment` and `startTrickle` out of
`index.html` by name and runs the real source in a `vm` context, asserting the actual request
URL, method, `Content-Type` and body. Only the Rust half restates.

**Fix.** `include_str!("static/index.html")` in the test and extract `trickleFragment`'s expected
output the way the harness does, or have the Node harness emit its generated body to a fixture the
Rust test parses.

### 8. [MINOR] Reorder-gating `MOUSE_DELTA` discards motion that reordering could not have harmed

`src-tauri/src/streamer/input/mod.rs:229-240`, `:338-348`; enabled by `input.js:145`, `:304`

`is_supersedable` returns `true` for `MouseDelta`, so the injector drops a delta whose sequence
is behind the high-water mark (`mod.rs:341`). A delta's position term is a *relative increment* —
`windows.rs:350-369` accumulates it into a tracked position — and increments commute, so applying
a reordered delta was already exactly correct. Dropping it loses that motion permanently, under
pointer lock, which is the mode where the user is least able to compensate.

The gate is defensible only for the frame's other term: `windows.rs:370` also runs
`button_transitions(self.buttons, buttons, …)`, and a stale button word could synthesise a
spurious release. So this is a tradeoff rather than a plain bug — but it is a tradeoff that only
becomes live *because of this branch*: before `input.js` started stamping deltas
(`input.js:304`), `parsed.seq` was always `None` and the gate never fired on a delta.

**Fix.** Split the terms: apply `dx/dy` unconditionally and gate only the button transition; or
drop `MouseDelta` from `is_supersedable` and rely on the existing staleness drop
(`mod.rs:325-331`).

Related, same file, lower confidence: `mod.rs:346` sets `highest_seq = Some(seq)` for
*non*-supersedable frames unconditionally, including when `seq` is older than the current mark —
a late `WHEEL` can walk the mark backwards. The effect is only that the gate briefly loosens, so
it is not harmful, but `if seq_is_newer(...)` would be the intent.

---

## Nits

9. **Head-only dedupe.** `index.html:1166` —
   `while (queue.length && posted.indexOf(queue[0].candidate) >= 0) queue.shift();` stops at the
   first non-duplicate, so a duplicate sitting behind a fresh candidate is trickled again. It is
   sound *today* only because `localDescription` accumulates candidates in gathering order, which
   is the order the queue holds, so all duplicates precede all fresh ones. That is an invariant of
   the browser, not of this code. Filter the batch instead of shifting the head.

10. **`a=end-of-candidates` is parsed and then dropped.** `webrtc_session.rs:2383-2392` only logs
    `end_of_candidates`; the host never calls `add_ice_candidate(None)`, so its ICE agent is never
    told remote gathering finished and cannot fail the checklist early.

11. **Comment contradicts code, and the status is non-standard.** `server.rs:839-840` reads
    "Gone rather than Not Found" and returns `StatusCode::NOT_FOUND`. The client special-cases
    exactly `404` to stop (`index.html:1190`), so acting on the comment would break the stop path —
    the coupling deserves a note on both sides. Separately `server.rs:991` answers `200 OK` with a
    `Location`; RFC 9725 §4.1 specifies `201 Created` for a WHEP resource. Harmless for our own
    client, wrong for any third-party one.

12. **Misplaced doc comment.** `webrtc_session.rs:728-736` — `abandon`'s three-line doc block now
    sits above `resource_id()`, whose own one-line doc is appended to it.

13. **Dead accessor.** `index.html:1211` `count()` has no caller in the page; only
    `scripts/test-client-ice.mjs` reads it.

---

## Verified correct

**Trickle ICE — the two probes.**
- `/net-config` returns `{"httpsPort":…,"trickleIce":true}` (`server.rs:781-784`) and the client
  requires the literal `=== true` (`index.html:1639`). An older host omits the key → `canTrickle`
  false → the gather-then-post shape is byte-identical to before.
- **A relayed join where `/net-config` 404s falls back correctly, by construction.** The cloud
  tunnel dispatches only `POST /whep`, `GET /reconfig` and `POST /leave` (`cloud.rs:526-551`);
  everything else is 404. `getNetConfig()` maps `!r.ok` → `{}` (`index.html:1000-1002`), so
  `canTrickle` is false and bit 22 is raised. The relay also drops `ProcessedResponse.resource`
  (it never reads the field), so no stray `Location` can reach a relayed client.
- The `Location` value is a relative reference `/whep/{hex}` (`server.rs:815-819`). `fetch` does
  not follow `Location` on a 2xx, and `headers.get('Location')` is fully exposed same-origin, so
  the client reads it raw and resolves it against the page base. **No CORS preflight**: the PATCH
  is same-origin, so the custom `Content-Type` and the non-simple method cost nothing, and the
  COOP/COEP isolation headers (`server.rs:508-516`) are irrelevant to a same-origin request.
- `axum = "0.8"` (`Cargo.toml:69`, lock 0.8.9), so `"/whep/{id}"` is genuine path-parameter
  syntax, not a literal segment. Under axum 0.7 it would have been a literal and every PATCH
  would have 404'd.

**Trickle ICE — the `trickleOff` latch.** It cannot loop and cannot double-join:
`trickleOff = true` is set *before* the recursive `start()` (`:2013` vs `:2017`), is never
cleared anywhere in the file, and `canTrickle` reads it first (`:1639`). `start()` opens with
`teardownSession()` and then `++startGen` (`:1626-1628`), and the outer call `return`s
immediately (`:2018`), so exactly one generation is live. The old peer connection is closed by
`teardownSession()`; the OTP survives the second POST because `validate` is not single-use.

**Trickle ICE — ordering, teardown, retry.**
- The candidate listener is installed *before* `offerSdp` is snapshotted
  (`:1948-1955`), so a candidate produced between `setLocalDescription` resolving and the POST is
  buffered, not lost. `open()` is what releases the buffer (`:1205-1209`); `flush()` no-ops while
  `!url` (`:1162`).
- The PATCH fires before `setRemoteDescription(answer)` on the client, which is harmless — and on
  the host the offer is already the remote description (`webrtc_session.rs:850-852`) and
  `register_trickle` runs before the answer is returned (`:919-922`), so a PATCH can never beat
  its own registration.
- Teardown while candidates are in flight is clean: `teardownSession()` stops the sender
  (`index.html:1244`), `stop()` is idempotent, clears the queue and removes the listener
  (`:1148-1153`), and the in-flight `.then` re-enters `flush()` which returns on `closed`.
  The `catch` also calls `localTrickle.stop()` unconditionally (`:2052`), correctly ignoring
  `stale()`.
- `404` → `stop()` rather than retry (`:1190`) is right: the session is over.
- Host-side registry lifetime is right — `Weak` entries, swept on the next `register_trickle`
  (`webrtc_session.rs:2302`), explicitly removed by both `SessionHandle::close` (`:672`) and
  `PendingWhep::abandon` (`:739`), and the id is minted only *after* the fallible negotiation
  block so a failed join leaves nothing behind (`:919-922`).

**Fast-channel sequence — every offset agrees with the host parser.**

| frame | client writes | host reads | agree |
|---|---|---|---|
| pointer | `input.js:224`, after `POINTER_LEN+STAMP_LEN` = **48** | `protocol.rs:448` `POINTER_LEN + STAMP_LEN` = 48 | ✓ |
| batch | `input.js:262`, `10 + n*24 + 8 + 2` | `protocol.rs:551` `need + BATCH_TAIL_LEN` = `10 + n*24 + 10` | ✓ |
| wheel | `input.js:285`, **23** | `protocol.rs:455` `WHEEL_LEN + STAMP_LEN` = 23 | ✓ |
| mouse delta | `input.js:304`, **17** | `protocol.rs:461` `MOUSE_DELTA_LEN + 1 + STAMP_LEN` = 17 | ✓ |

Buffer sizing is exact, not lucky: `batchBuf` is `10 + 40*24 + 10 + 2 = 982` and the largest
write ends at 982 (`input.js:111`). `rd_u16` is bounds-checked and returns `None` past the end
(`protocol.rs:357-359`), so a client that sends the stamp but not the sequence reads as `None`
and simply is not gated.

**Fast-channel sequence — gating, wraparound, old hosts.**
- `wantSeq()` requires `wantStamp()` **and** bit 5 (`input.js:145`), and every write site is
  nested inside `if (wantStamp())`. `HOST_CAPABILITIES` always sets bit 3 alongside bit 5
  (`protocol.rs:850-853`), so the "sequence without stamp" state is unreachable from both ends.
- A host without bit 5 receives byte-identical v1/v2 frames: `hostCaps` is 0 until the first
  PONG and a 17-byte PONG decodes to 0 (`input.js:560-561`). Confirmed on the wire by
  `test-client-stats.mjs` — MOUSE_DELTA stays 17 bytes and WHEEL 23 until bit 5 arrives.
- Only fast-channel frames carry it. `sendPointer` checks `(ch || channelFor(op)) === fast`
  (`input.js:223`), so `POINTER_DOWN`/`UP`, which are sent with an explicit `reliable`
  (`:750`, `:757`), stay 48 bytes — asserted on the wire in the harness. Wheel, delta and batch
  need no such check because `channelFor` routes all three to `fast` unconditionally
  (`:152-154`).
- Wraparound is correct on both sides: the client masks with `& 0xffff` (`input.js:146`), the
  host compares the signed difference `(seq.wrapping_sub(last) as i16) > 0` (`protocol.rs:126`),
  which is exact across 65535→0 for anything under half a space — several minutes of continuous
  motion at one frame per pointer sample.
- "Never reset" is the right choice and the comment's reasoning holds: the host's `highest_seq`
  is per injector thread and starts at `None` (`mod.rs:311`), so a monotone client counter is
  correct on both sides of a reconnect. Gaps from `fastSend` backpressure drops
  (`input.js:177`) are harmless to a high-water comparison.

**Old-browser parse safety.** Nothing added in this diff uses `?.`, `??`, class fields,
`#private`, top-level `await`, numeric separators, lookbehind, `BigInt` literals, optional
`catch` binding, object spread, `.at()`, `Object.fromEntries`, `Promise.allSettled`,
`globalThis`, `structuredClone`, `replaceAll` or `queueMicrotask`. The new trickle code is
plain ES5 plus `const`/`let` and one pre-existing arrow — `function` expressions throughout,
`Array#splice`/`unshift`/`indexOf`, `String#indexOf`, a `RegExp` with no lookbehind
(`index.html:1114`). `node --check` on `input.js` and a `new Function()` parse of both inline
script blocks succeed. `setBigUint64` appears only in `scripts/test-client-stats.mjs`, which runs
in Node.

**No visual or markup drift.** The `index.html` diff touches only the inline `<script>` region —
no element, attribute, class or CSS rule changed. The join flow's observable behaviour is
unchanged except where it is the feature: the spinner is never hidden or re-shown by the rejoin,
`showError` text is untouched, and the only new latency on the join path is `await getNetConfig()`
(`:1636`), which is normally already resolved because it is primed at load (`:1012`) — and the
prime is guarded on `typeof fetch === 'function'` so a browser without `fetch` still gets the
join form rather than a module-scope throw.

**Host-side hygiene inside the reviewed diff.** `same_device_page` (`server.rs:519-535`) is a
behaviour-preserving change — two `OnceLock`s replacing a per-request `String::replace` over the
largest asset in the tree. `whep_patch` rejects non-UTF-8 with `400` and bodies over
`MAX_SDP_LEN` (64 KiB) with `413` before parsing (`server.rs:830-835`). `parse_trickle_fragment`
tolerates junk, a bare `candidate:` prefix, multiple `m=` sections and an empty body without
panicking, and is covered by five unit tests.

**The harnesses assert real bytes, not a reimplementation.** `test-client-stats.mjs` loads the
shipped `input.js` and reads the actual sent buffers at the actual offsets (48 / 23 / 17 /
`10+n*24+10`), including the negative cases (17 and 23 bytes before bit 5 arrives, 48 bytes on
`reliable`). Its two new stubs are real fixes, not conveniences: `send()` now `.slice()`s because
`input.js` reuses one `ArrayBuffer` per frame type, and the `MessageChannel` stub now delivers
its post so the coalescing gate reopens. `test-client-ice.mjs` runs the shipped `startTrickle`
and asserts the request URL, method, `Content-Type` and body. Both were run for this review and
both pass. The one restated artefact is finding 7.

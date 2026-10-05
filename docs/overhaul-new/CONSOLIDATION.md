# Consolidation (Step 4, §8) — `consolidate` on `latency-overhaul-new` `5e20e52`

All nine group branches were already merged into `latency-overhaul-new` (head `5e20e52`). This
step fixed the cross-group defects the merges left, re-read the seams where groups touched the
same code, ran the full gate, built the merged tree and took the final three-way measurement.

## A. Cross-group fixes (branch `consolidate`)

| commit | what | evidence | status |
|---|---|---|---|
| `fb6d08b` | **Telemetry unregistered by the wrong session** (G03's find). Both transports of a renegotiated join publish under one join id; the superseded session's teardown called `telemetry::unregister(&join)` and removed the live successor's handle — right after every renegotiation since `1f85421` closes the superseded transport promptly. Now `telemetry::unregister_if(key, &own)` (`Arc::ptr_eq`). | unit test `superseded_session_does_not_unregister_its_successor`; end to end see §D | COMPILE-CHECKED + unit test (end to end: §D) |
| `9433732` | **Blocking `session.stop()` on async workers** (G03's D-02 candidate). The four inline stops in `start_session` (failed handshake, failed activation, the two "superseded" stand-downs) and the no-session-map teardown branch now go through `discard_capture()` = `spawn_blocking(stop).await`, so ordering is unchanged (the capture still stops before `abandon_failed_attempt` removes the display, `f789b76`, and before the pending handshake/transport is abandoned). It also withdraws the failed attempt's own telemetry entry, which used to stay registered. | compiles; server tests pass (incl. `f789b76`'s three) | COMPILE-CHECKED. Not shown to be D-02's cause (no reproduction with a stack) |
| `d5ea62a`, `29e75d9` | **Client audio path 4** (MessagePort ring, G06) reported `"unknown"`; now `"worklet+port"`. Test `every_client_audio_path_has_a_name` parses `AUDIO_PATH` out of the shipped `index.html`. | unit test | COMPILE-CHECKED + unit test |
| `d1b494c` | **Docs**: `CLAUDE.md` — Windows audio tier order (process loopback first, endpoint the fallback, `SE_ENDPOINT_LOOPBACK=1`), ten client harnesses, `PliBackoff`, `attemptId`; `docs/CAPABILITY-TIERS.md` audio row reordered with G06's numbers. The WGC minimum-update-interval rule and `SCREENEXTEND_DATA_DIR` were already in `CLAUDE.md`. | read against the code | — |
| `7bf24ae` | **`docs/OPTIMIZATIONS.md`**: 33 entries for every kept change of G01–G09 plus the three above, statuses as the ledgers give them (where a ledger gave no status word, the entry says what its status rests on). | — | — |
| `844c35f` | `index.html` comment over `setupFailureAction` said the host could not recognise a first join's lost-answer retry; G04b's `7584269` does. Comment only. | `pnpm test:client` | — |

**Other blocking calls on the join / renegotiate path (read, `server.rs`):** none left inline.
Every OS call (`start_on_monitor`, `create_display_with_modes`, `monitor_device_names`,
`attached_display_paths`, `display_device_name`, `claim_display_identity`, `current_mode_of`,
`set_display_mode_owned`, `set_display_scale_owned`, `monitor_dimensions_owned`,
`set_display_topology_extend`, `remove_display`) is `spawn_blocking`; the previous capture's
stop on a continuing join and the teardown stopper already were. What remains inline is short:
std-mutex map reads/writes (`session::*`, `forget_display_identity`'s set), and
`client.created_geometry` (returns a cached value / `None` on Windows). `monitor_rect` is
spawned and not awaited (log line only). `DISPLAY_CORRELATION_LOCK` is still held across the
≤5 s attach poll, by design. Inside `start_on_monitor` (already on the blocking pool) G01's
stale-monitor retry sleeps up to 5 × 200 ms — on the blocking pool, so harmless to the workers.

### Seams read (groups that touched the same code; all merges were textually clean except `f47f088`'s handler.mjs)

- **`index.html`** — G05's watchdog latch (`webcodecsPainted`, reset in `teardownSession`,
  checked by `armFrameWatchdog` both when arming and when firing) is intact under G07's
  same-tier retry (`setupFailureAction` → `start(preferWebCodecs, true)`), which tears the
  session down first, so the latch and the watchdog are per session. G05's keyframe hand-back
  sends the `writable` with the `readable` on the legacy path and falls back to readable-only on
  a `DataCloneError`. G07's `warmJoinPath` (one `enumerateDevices()` + one throwaway
  `RTCPeerConnection` at load) prompts nothing; G06 removed the microphone card from the join —
  `primeMicPermission` (the only `getUserMedia`) is reachable only from
  `requestOutputChoice`, i.e. only when the host operator picks "choose on this device"; the
  join's output enumeration is `enumerateBeforeJoin` and not awaited. `attemptId` (G07) is in
  every POST body and the host half (G04b) reads it. Stale comment fixed (`844c35f`).
  `pnpm test:client`: all ten harnesses pass on the merged page.
- **`webrtc_session.rs`** (G03 + G06) — G03's `AUDIO_SACK_ALLOWANCE_MS` changes only
  `backpressure_limit_bytes`; G06's spare-track feeding and RTP clock live in other functions.
  G03's D-05 close of the superseded transport (`register_trickle` returns the replaced PC,
  closed on the new one's `Connected`) also covers G04b's attempt continuation: the lost-answer
  attempt's PC is the one replaced. The superseded teardown then sleeps the disconnect grace
  before noticing it is superseded (harmless, a parked task) and keeps the display.
- **`server.rs` / `session.rs`** (G04, G04b, G03's D-05) — `attempt_continuation` requires
  `!ever_connected` + a display + the same token, and `process_whep`'s `retried` branch uses the
  same lookup to keep the lost answer's token, so the two agree. A continuing (attempt or
  renegotiation) join that fails keeps its display for the retry (`f789b76`); the old capture
  was already stopped at the top of `start_session`. Fixes 1 and 2 above are the two defects
  found at this seam.
- **`pipeline.rs`** (G01 + G08) — G01's stale-monitor retry loop, cursor-flag bitfield and
  `wgc_min_update_interval`, and G08's removal of WGC `SystemRelativeTime` as a capture time and
  the keepalive's encode stamps touch different functions; the merged code has each as its
  group left it.
- **Input and desktop UI untouched**: `git diff latency-overhaul HEAD -- src-tauri/src/streamer/input src-tauri/src/streamer/static/input.js` is empty; `git diff latency-overhaul HEAD --stat -- src` is empty.

## B. The full gate on the merged tree (`consolidate`, after every fix above)

Target dir `C:\Users\sarve\.cargo\shared_target\ScreenExtend-consol` (seeded from `-g03`), every
cargo run under the build lock, `--offline`.

| gate | result |
|---|---|
| `cargo fmt --check` | **pass** (after `29e75d9`; the first run flagged one line of my own test) |
| `cargo clippy --all-targets -- -D warnings` | **pass** (run twice; the last on the final code tree) |
| `cargo test` | **pass**: lib 489 passed, 0 failed, 18 ignored; other targets 0 passed / 4 ignored. No `SKIP` lines on stderr |
| `SE_REQUIRE_HW=1 cargo test` | **1 failure, pre-existing and by configuration**: 488 passed, 1 failed (`nvenc_ltr_repair_is_cheaper_than_an_idr`), 18 ignored. The test skips when the encoder opens without long-term references, and LTR is off by default since UX-GREEN (opt-in `SE_NVENC_LTR=1`), so `SE_REQUIRE_HW=1` turns the skip into a panic. With `SE_REQUIRE_HW=1 SE_NVENC_LTR=1` that test **passes** on this RTX 3080 Ti. Not a regression of the merge; left as is (question below) |
| `pnpm lint` | **pass**: 0 errors, 33 warnings (the known non-blocking set) |
| `pnpm build` | **pass** |
| `pnpm test:client` | **pass**: all 10 harnesses (run before and after the `index.html` comment change) |
| `pnpm test:config` | **pass** |
| `pnpm test:gate` | **pass**. The brief lists `scripts/test-startup-gate.mjs` among scripts never to run; the consolidation task said to check it first. Read in full: it imports `src/lib/startup-gate.ts` (pure functions) and reads `src/i18n/locales/en.json`, and nothing else — no process, no GUI, no input (`'SendInput'` appears only as a fixture string). So it was run |
| `node scripts/check-lockfile.mjs` | **pass** (5 packages with optionalDependencies, all recorded) |
| `node scripts/check-bindings.mjs` | **pass** (54 commands, 11 events) |
| `node scripts/check-versions.mjs` | **pass** (0.6.3 in all four files) |
| `node scripts/check-legacy-webkit.mjs` | **pass** (no unpatched post-floor APIs in `dist/` or the client page) |

`pnpm install --frozen-lockfile --prefer-offline` was needed in the worktree; the lockfile is
unchanged. `src-tauri/gen/schemas/*.json` show as modified after a build (line endings only) and
were not committed.

## C. The build

**Thin LTO, not the real release profile.** Commit headroom when the build was due was
9.63 GB, below the 10 GB the task set for a fat-LTO build, so the build is
`CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build --release --offline`
(Idle priority, `CARGO_BUILD_JOBS=4`, under the build lock; 5 min 09 s). **Every "new" number in
§D is therefore from a thin build, while "prev" (`d4d8be9`) is a fat-LTO build** and "old"
(`bef5f89`) is whatever its directory holds. A fat-LTO build of this tree is still owed.

- Code at `29e75d9` (`consolidate`; the later commits are docs only).
- Exe (stable copy, with `libopus.dll` and `libx264-164.dll` from `src-tauri/resources/` beside
  it): `C:\Users\sarve\.cargo\shared_target\ScreenExtend-consol\final\ScreenExtend.exe`
  (SHA-256 starts `4F8CBA4D746DFCC2`). The cargo output itself is
  `...\ScreenExtend-consol\release\ScreenExtend.exe`.
- `pnpm tauri build` (the installer bundle) was not run: the measurement only needs the exe, and
  bundling adds nothing the handler reads.

## D. The final three-way measurement

### Setup

- Builds: **old** `bef5f89` (`ScreenExtend-old\release`), **prev** `d4d8be9` (`ScreenExtend\release`,
  fat LTO), **new** = this branch, thin LTO (`ScreenExtend-consol\final`). All three were run in
  **exclusive mode** (`force_exclusive: true` for new as well), so all three ran the same way:
  slot-1 ports, the real data directory, `config.json` backed up and restored.
- The owner's installed app was not running before any run. `config.json` SHA-256 was
  `176B9DAD7C9C4985…` before and after every one of the 14 runs (`ownerConfig.identical` true in
  each result, and checked by my runner around each).
- Request (each run): `{"scenarios":["small_window","large_area","hires_small_window"],"runs":2,"duration":20,"stamp_band":"center","force_exclusive":true}` —
  one 1080p join (small window ×2, large area ×2) and one hi-res join (2560×1440 screen, DPR 2,
  a real 120 Hz, fresh device token) per run. Three rounds, interleaved old, prev, new, old…
  (09:00–09:37 on 10/5). Load gate on; **no window was marked `loaded`** (window CPU 21–47 %).
  Commit headroom 7.1–9.7 GB and free physical memory 2.6–4.7 GB across the windows (never
  under the 1.5 GB "suspect" line). Every join had `offers: 1`.
- Then, once per build, `{"scenarios":[],"audio":true,"duration":15,"stamp_band":"center","force_exclusive":true}`;
  then on new only the D-05 check and `slow_answer` n=5 (below).
- Display creations spent: 9 × (one 1080p + one hi-res, counted double) = 27, audio 4 (old
  restarts its host and rejoins), D-05 1, slow_answer 5: **37 of the 40 budgeted.** The driver
  host's commit went 34,584 → 36,714 MB (+2.1 GB, threads 2,950 → 3,131) over Part D,
  141–215 MB per standard run.

### Like for like (from each run's `serve.log`)

| | old | prev | new |
|---|---|---|---|
| 1080p join | 1920×1080@60, NVENC ring, **Baseline**, CBR 12.44 Mbps, qp None | 1920×1080@60, High (negotiated), CBR 12.44 Mbps, qp None | same as prev |
| hi-res join | 2560×1440@120 (registered and set), Baseline, CBR 30 Mbps | 2560×1440@120, High level 0x34, CBR 30 Mbps | same as prev |
| saved override on the hi-res join | none (fresh token; old keys overrides by IP and the owner's config has no row for the test address) | none (fresh token) | none (fresh token) |

So rate control is identical (CBR, no constant QP) on every build and every join; the only
encoder difference is the profile — the old app always answers Baseline, prev and new negotiate
High from Chrome's offer. That is a property of the builds, not forced equal. On new, one hi-res
join hit G01's stale-monitor case (`WGC could not open \\.\DISPLAY9 … re-resolving`) right after
the mode set and recovered on WGC in 0.8 s (`48d3807` working).

### The three-way table (stamp metric: host draw → client paint call, ms)

Valid windows: not `loaded`, the join's `offers` = 1, and a real sample. **Discarded:** new's
round-2 1080p join (4 windows), see "Unexplained" below.

| scenario | build | windows | p50 median | p50 min–max (range) | p99 median (min–max) | painted fps median (min–max) | duplicate frames / 20 s window |
|---|---|---|---|---|---|---|---|
| small window 1080p60 | old | 6 | **25.63** | 24.90–26.29 (1.39) | 34.7 (33.9–35.2) | 54.3 (53.5–55.1) | 0 |
| | prev | 6 | **27.02** | 25.71–27.53 (1.82) | 35.0 (34.2–35.9) | 54.5 (52.3–55.2) | 50–58 |
| | new | 4 | **25.90** | 24.06–26.58 (2.52) | 34.4 (33.9–34.7) | 54.0 (53.7–54.7) | 1–6 |
| large area 1080p60 | old | 6 | **28.92** | 28.03–29.29 (1.26) | 36.6 (36.0–37.0) | 53.7 (52.3–54.9) | 0–1 |
| | prev | 6 | **29.20** | 28.48–29.99 (1.51) | 38.4 (36.6–40.1) | 53.0 (52.5–53.5) | 59–65 |
| | new | 4 | **28.41** | 27.16–29.33 (2.17) | 36.2 (35.4–36.8) | 52.9 (52.7–53.7) | 2–3 |
| hi-res small window 2560×1440 @120 | old | 6 | **34.86** | 32.70–38.15 (5.45) | 53.7 (43.7–92.6) | 86.0 (74.4–88.5) | 627–857 |
| | prev | 6 | **34.98** | 34.67–35.52 (0.85) | 50.1 (47.0–52.4) | 81.2 (79.2–85.2) | 330–427 |
| | new | 6 | **34.30** | 33.88–35.05 (1.17) | 48.0 (46.4–48.8) | 82.5 (80.3–84.4) | 294–364 |

| join (3 per build per kind) | old | prev | new |
|---|---|---|---|
| 1080p (first join of a host): submit → answer, ms | 7324 / 7674 / 7731 | 4861 / 4898 / 4574 | **2978 / 2763 / 2678** |
| 1080p: submit → first stamped picture, ms | 7397 / 10605 / 10505 | 7970 / 7688 / 7206 | **3165 / 2998 / 2919** |
| hi-res (second join): answer, ms | 6200 / 5921 / 6100 | 2971 / 2822 / 3144 | 3343 / 2738 / 2304 |
| hi-res: first picture, ms | 9475 / 9118 / 9058 | 3055 / 2901 / 3254 | 3471 / 3035 / 2858 |

| audio (one run per build; click → client, ms; includes winmm buffering, compare only) | old | prev | new |
|---|---|---|---|
| clicks matched | **0 of 15** (the old page cannot play: known COEP worker defect) | 15 of 15 | 15 of 15 |
| → decoded p50 | — | 80.8 | **57.3** |
| → rendered p50 (min–max) | — | 92.3 (91.0–99.4) | **62.8 (62.3–64.2)** |
| → output p50 | — | 148.7 | **111.1** |
| path / transport | webcodecs / shared (silent) | webcodecs / shared | webcodecs / shared (page on the host machine starts muted, `24f75e8`; the handler unmutes it) |

### Verdict per scenario (keep rule's noise test: a difference counts only if larger than the larger run-to-run p50 range)

- **Small window 1080p: level.** New vs old +0.27 ms, vs prev −1.12 ms; the larger range is 2.52.
  p99 and fps not worse. Duplicate frames: prev's ~50 per window are gone on new (1–6).
- **Large area 1080p: level.** New vs old −0.51, vs prev −0.79; larger range 2.17. p99 36.2 vs
  prev 38.4 (not worse).
- **Hi-res 2560×1440 @ 120 Hz: level.** New vs old −0.56 (old's range 5.45), vs prev −0.68
  (range 1.17). p99 48.0 vs 53.7 / 50.1, painted fps 82.5 vs 86.0 / 81.2 — fps inside old's
  range (74.4–88.5). **The hi-res regression BASELINE measured (40.1 vs 31.9 ms) is not present**
  with like-for-like rate control (CBR on every build), a real 120 Hz and a fresh token.
- **Join: new beats both, by more than the noise.** First 1080p picture 2.9–3.2 s against
  7.2–8.0 s (prev) and 7.4–10.6 s (old); answer 2.7–3.0 s against 4.6–4.9 s and 7.3–7.7 s. The
  hi-res (second) join is level with prev (≈2.9–3.5 s) and ~6 s faster than old.
- **Audio: new beats prev by ~30 ms click → rendered (62.8 vs 92.3 ms), one run each**, matching
  G06's −29 ms for process loopback first. Old plays nothing.
- Summary: on latency the merged build is **level with the old app and with the previous head**
  in every video scenario on this machine (none is worse), with a much faster join, working audio
  ~30 ms sooner than prev, fewer duplicate frames than prev, and the defects the groups fixed.

### Unexplained (new build, 1 of 3 1080p joins)

`std-r2-new` (run dir `…\runs\2026-10-05T16-20-59-114Z-slot1-86820`): the 1080p join answered
in 2.76 s and painted its first stamped frame at 3.0 s (stamp 72 ms old), but in all four timed
windows that followed **every decoded stamp was "implausible"** (older than 5 s or in the
future: 881–1,231 per window, 0–11 plausible). Meanwhile the host encoded 56–60 fps from WGC,
the page reported `videoPath` 1, 70 painted fps and its own `hostToPaint` p50 15.7 ms, and the
stage drew 4,857 stamps in 27 s on `\\.\DISPLAY9` (found by driver id). The hi-res join of the
same run was normal. So the stream was live by the build's own counters while the picture it
carried showed a stamp that was not current — a frozen/stale composition of the stage window,
or a harness fault; not established which. Not seen on old or prev in these 9 runs (and not in
the other two new runs). Request that would settle it (6 joins at 1080p, ~6 creations):
`{"scenarios":["small_window"],"runs":1,"duration":15,"stamp_band":"center","force_exclusive":true,"slow_answer":{"n":5},"join_probe":"JSON.stringify(window.SEVideo&&SEVideo.getStats())"}`
on new, and a screencast of a window with `collectStampsScreencast` to see what the client shows.

### Extra checks on the new build (budget allowing)

- **G01 `ea821a9` hi-res 120 Hz question** (one run on `d7a448a` gave p50 73.5 ms and 810
  duplicate frames): **not reproduced.** Six hi-res 120 Hz windows on the merged build: p50
  33.9–35.1 ms, 294–364 duplicate frames per 20 s window (old: 627–857), painted fps 80–84.
  Not an A/B against `ea821a9`'s parent, so it says the merged build is healthy there, not what
  that one run was.
- **D-05 after the fix + Part A fix 1**, request
  `{"scenarios":["small_window"],"duration":15,"runs":1,"stamp_band":"center","force_exclusive":true,"settings_change":true,"settings_only":["video quality"],"custom":{"js":"1","host_commands":[["status","--json"]]}}`:
  one encoder-only edit (`--video-quality 30`) — same join id and display id, 1 display before
  and after, no device row removed or added, 1 new peer connection, picture back in 855 ms
  (largest gap 509 ms), the new encoder at `rc=constqp qp=30`. The host log: `peer connection
  state changed: Connected` → `closing the transport this renegotiation replaced` **in the same
  millisecond**, the superseded session's teardown ran 1.5 s later (`live transport sessions:
  1`), and `status --json` taken after that still lists the join's telemetry
  (`live_transport_sessions: 1`, `sessions` holds the join id with the new session's counters).
  **`1f85421` and `fb6d08b` are VERIFIED end to end (one run).**
- **`slow_answer` n=5** (`{"scenarios":[],"slow_answer":{"n":5},"stamp_band":"center","force_exclusive":true}`):
  **0 flagged**; answers 1.98–2.21 s, first picture 2.38–2.52 s, one offer each. (Five quiet-machine
  joins; D-02 was seen about one join in five under load, so this does not prove fix 2 removed it.)

## Not run (with the request)

- **The real release profile** (fat LTO, one CGU) build and the three-way on it: headroom was
  9.6 GB < 10 GB. Build `cargo build --release --offline` under the lock, then three rounds of
  the standard request above, interleaved.
- **Three-way with five runs** (§8 asks for five): three rounds were what the display budget
  allowed. Two more rounds of the standard request = 12 creations.
- G04's third A/B round, G08's two telemetry fixes through the handler, G07 K3/K4, G05's D1 live
  confirmation (tens of joins), G02's x264 / async / adaptive-quality requests, G03's §7
  ablations (pacer, BWE, profile, packetizer, runtime width, socket tier), the 2-display
  confirmation of `ea821a9`, the noise figure with `stamp_band: "center"`: each ledger keeps its
  request; none was run here (budget).
- `07ec295` (audio SACK allowance) A/B: not run as an A/B; the new build's single audio run had
  15/15 clicks and no silence.

## Questions for the owner

- `SE_REQUIRE_HW=1 cargo test` fails on `nvenc_ltr_repair_is_cheaper_than_an_idr` because LTR is
  off by default (UX-GREEN). Should that test require `SE_NVENC_LTR=1` explicitly (so a strict
  run on a capable machine is green), or is the strict-mode failure the intended reminder? Assumed:
  left as is.
- The consolidation build is thin LTO. Is a fat-LTO re-measure wanted before calling the numbers
  final (it needs ≥10 GB commit headroom, i.e. probably a reboot first)?
- The old app always streams H.264 **Baseline**; prev/new negotiate **High**. The comparison was
  left like that (it is what each build does). Assumed fine.
- About 10 % of 1080p frames still fail the stamp checksum with `stamp_band: "center"` on all three
  builds (90–150 per window); the handler README says `center` fixes them. Treat the README claim
  as unconfirmed.

## Unverified / untested

- `9433732` (blocking stops off the workers): COMPILE-CHECKED; not shown to cure D-02 (5 clean
  joins on a quiet machine prove nothing about a 1-in-5-under-load defect).
- The new build's one stale-stamp 1080p join (above): unexplained.
- Everything every group ledger lists as unverified stays unverified; nothing here touched input,
  macOS, AMD/Intel or the desktop UI.

## Machine state at hand-back

No ScreenExtend host, test Chrome, stage window or virtual display of mine left (every result's
`cleanup` empty; `handler --status` all slots free, no lock; no `ScreenExtend.exe` running). No
build lock held. `config.json` SHA-256 `176B9DAD7C9C4985…` (unchanged). Driver host commit at the
end of Part D 36,714 MB (3,131 threads); system commit headroom 7.9 GB.

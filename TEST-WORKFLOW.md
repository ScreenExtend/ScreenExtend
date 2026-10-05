# TEST-WORKFLOW.md — find the fast app inside 442 commits and consolidate it

You are the orchestrator of this job. This file is your whole brief; nothing else has been said to
you. Read it to the end before doing anything.

## 1. The situation

ScreenExtend is a Tauri 2 desktop app (repo: `C:\Users\sarve\Downloads\funnyscnew\ScreenExtend-workspace\main`).
A Rust host creates a virtual display per client, captures and GPU-encodes it, and streams it over
WebRTC (WHEP, H.264) to a dependency-free browser page. `CLAUDE.md` at the repo root describes the
architecture and its rules; it binds you and every agent you start.

The old app was fast but not the fastest. The owner gave a frontier model (Claude Fable 5.1) the
task of optimising it. It produced **442 non-merge commits** between `bef5f89` (the old app, the
last commit before that work) and the head of `latency-overhaul` (`d4d8be9` when this was written).
The work started well, and the model is an expert, so real optimisations are in there. But
optimisations were added, partly reverted, re-fixed and layered on each other, and the app that
came out the far end is, by the owner's account, worse than the old one:

- slower, with cursor glitches
- sound does not work
- changing a device's settings makes two new devices appear with two virtual screens
- a microphone permission prompt appears on desktop even where speaker outputs need no permission
- logs do not come through
- the desktop UI is odd (out of scope here, see §3)

**That list is rough and incomplete. Finding the rest is part of the job.**

**The goal:** somewhere in those 442 commits is a version of the app that is far faster than the
current one. Find it. Consolidate every change that genuinely lowers latency or fixes a defect into
one master build on a new branch, `latency-overhaul-new`, and leave out or repair every change that
does not. Every claim is decided by measurement on this machine, never by a commit message.

## 2. What already exists (use it, do not redo it)

- `docs/commit-groups/COMMIT-GROUPS.md` and `commit-groups.json` — all 442 commits sorted into 14
  groups, each with what changed, the latency it claims, its risk, whether it changed a default, the
  switch that toggles it, and which earlier commit it reverts or supersedes. **These are leads, not
  facts:** the classifier read every commit message but almost no diffs. Read the diff before
  trusting a row.
- `TEST-GUIDE.md` and `scripts/e2e-headless/` (`e2e.ps1`, `run.mjs`, `mover.ps1`, `tone.ps1`) — a
  working end-to-end audio+video test: host from the CLI, a fresh headless Chrome driven over its
  debugging protocol, measurements, clean shutdown. It is the seed of the test handler in §5 and
  already encodes several hard-won facts (§5.6).
- `docs/OPTIMIZATIONS.md` (every change attempted, with numbers and a status, including rejected
  ones), `docs/FINAL-REPORT.md` (§15.x, one section per later fix), `docs/BASELINE.md`,
  `docs/LATENCY-MAP.md`, `docs/TELEMETRY.md`, `docs/DATAFLOW.md`, `docs/CAPABILITY-TIERS.md`.
  Take their findings into account: they say what was tried, what was measured and why things were
  reverted. Treat their numbers as claims to re-measure.
- Reference numbers for the current head, measured the day this was written (same machine, Chrome
  154 headless, 800×600 virtual display, a 48 px window moving on it): host capture → client paint
  p50 12.2–12.8 ms and p99 19.8–20.5 ms; decode p50 0.7–0.8 ms; 58–61 painted fps; join → first
  paint about 3.1 s; audio on the fast path, 200 packets/s, 0 underruns. Those come from the client
  page's own counters, which the old app may not have (§5.3).

These files are **untracked** in the working tree: `TEST-WORKFLOW.md`, `TEST-GUIDE.md`, `TEST.md`,
`scripts/e2e-headless/`, `docs/commit-groups/`. Step 0 commits them so agent worktrees can see them.

## 3. Hard rules (for you and, verbatim, in every agent brief)

1. **Input is off limits.** The owner's words: "do NOT manipulate my cursor. displays is fine. if
   you want to simulate a cursor, paint a toiny moving window. do not hijack the cursor". No agent
   may test an input module or inject input by any route: no `SendInput`, `SetCursorPos`,
   `mouse_event`, keyboard injection, no browser-protocol `Input.dispatchMouseEvent` /
   `dispatchKeyEvent` / `dispatchTouchEvent`, no synthetic DOM pointer/keyboard/wheel events on the
   client page (the page forwards them to the host, which moves the real cursor). Page actions are
   DOM method calls (`.click()`, `requestSubmit()`) through `Runtime.evaluate`. Motion on a display
   is a small moving window. Never run `scripts/measure-input.ps1`, `scripts/measure-stream.ps1` or
   `scripts/test-startup-gate.mjs`. Input code is not experimented on: at the end,
   `git diff latency-overhaul latency-overhaul-new -- src-tauri/src/streamer/input src-tauri/src/streamer/static/input.js`
   must be empty. Input gets a written report instead (§9), and a brief for a later job that may test it (§11).
2. **Never push.** Nothing on any remote may change.
3. **Only Windows + NVIDIA is testable here.** macOS, AMD and Intel code cannot be run on this
   machine. Leave it exactly as it is on `latency-overhaul`. If a shared change forces an edit in a
   macOS/AMD/Intel file to keep it compiling, make the smallest mirror and mark it
   `UNVERIFIED (not compiled)` or `UNVERIFIED (not run)`.
4. **The desktop UI (`src/`) is out of scope** and will be done later. Do not change how it or the
   client page looks. Client page *behaviour* is in scope.
5. **All code goes to `latency-overhaul-new`.** Do not commit to `latency-overhaul` or `main`.
6. **Models:** every agent you start for this job runs on Opus 5.5.
7. **The owner's machine is in use.** Kill only processes you started, by PID — never by image name
   (the owner's own Chrome is running). Never write the owner's
   `%APPDATA%\app.screenextend.desktop\config.json`; the handler isolates or restores it (§5.2).
   Before claiming a clean finish, confirm no host, no test browser and no virtual display of yours
   is left.
8. **Report faithfully.** A test that could not run says so. A change that was not measured is not
   called a win. Unexplained results are written down as unexplained.

## 4. The plan

| Step | What | Who |
|---|---|---|
| 0 | Create `latency-overhaul-new` from `latency-overhaul`; commit the untracked files listed in §2 | you |
| 1 | Build the test handler (§5) and prove it on the current head | one agent |
| 2 | Baselines and bug census: old app vs current head (§6) | one agent, or you |
| 3 | One agent per commit group (§7), each producing kept changes and a ledger | nine agents |
| 4 | Consolidate group by group into `latency-overhaul-new`, re-measuring after each merge (§8) | you |
| 5 | Input report (§9) and final report (§10) | one agent + you |
| 6 | Write `FUTURE-TEST-WORKFLOW.md` for everything this job could not test (§11) — the last thing you do | you |

## 5. Step 1 — the test handler

Build `scripts/test-handler/` (Node, no new npm dependencies; PowerShell helpers where Windows
needs them). Every measurement in this job goes through it, so that every number is produced the
same way.

### 5.1 Interface

```
node scripts/test-handler/handler.mjs --exe <path-to-ScreenExtend.exe> [--request <file.json | inline JSON>]
```

It runs that build's **CLI host** (`serve`), connects a fresh headless Chrome, runs the standard
end-to-end latency test plus whatever the request asks for, shuts everything down, and prints one
JSON document on stdout (exit 0 = ran to completion, whatever the numbers; non-zero = the harness
itself failed, with the reason in the JSON). The caller is an agent: make the JSON complete and
self-describing, and put bulky artefacts (logs, raw samples) in a per-run directory whose path is in
the JSON.

### 5.2 Concurrency and isolation

- **At most four tests run at once**, enforced machine-wide (slot files under
  `%TEMP%\screenextend-test-handler\`, with stale-slot recovery when the owning PID is gone). A fifth
  caller waits in a queue.
- **Latency is measured one test at a time.** Four hosts share one GPU, one encoder and one audio
  mix, so concurrent latency numbers contaminate each other. A machine-wide measurement lock covers
  the timed window of every test; setup, joins, functional checks and teardown may overlap. Any
  test that plays or analyses audio also takes the lock.
- Each slot has its own HTTP/HTTPS ports, browser debugging port, Chrome profile, working directory
  and **app data directory**. The host has a single-instance lock and one `config.json`, so
  concurrent hosts need a small test-only isolation switch in the app (for example an environment
  variable that redirects the app-data and local-data directories and so separates the lock file,
  the control socket and the config). Add it as the first commit of this job, keep it inert when
  unset, and document it.
- **Builds without that switch** (the old app and any revision older than your commit) cannot be
  isolated. The handler must detect this (the host does not come up on the slot's ports) and fall
  back to **exclusive mode**: wait for all slots to be free, hold them all, run on the default ports
  with the real data directory, back up `config.json` first and restore it byte-for-byte afterwards.
  Where you build an old revision yourself, prefer cherry-picking the isolation commit onto it.
- The handler adapts to what a build offers and reports what it had to do: credentials by
  environment variable where supported (never on the command line when avoidable), otherwise parsed
  from the `serve` output; the control verbs (`status --json`, `stop`, `devices set`) where they
  exist, otherwise a PID-scoped stop. Check what `bef5f89`'s CLI actually has before relying on any
  of it.

### 5.3 The primary metric must not depend on the build

The current client page exposes `SEVideo.getStats()`; the old page may not, and counters the code
under test maintains can themselves be wrong. So the handler measures latency from outside:

- **Video, end to end.** A borderless, never-focused, topmost window on the test's virtual display
  draws the host's wall-clock time as a machine-readable pattern (for example a row of large
  black/white cells encoding milliseconds, with a guard pattern and a checksum), redrawn every
  frame. In the page, a script the harness injects reads the pixels the client actually painted
  (from the `<video>` or the canvas), decodes the stamp and subtracts it from the client's clock.
  Host and client are one machine, so the clocks agree. This covers capture, encode, send, receive,
  decode and paint for any build. Report p50/p90/p99/max/min, the sample count, the number of
  distinct stamps seen per second (true painted fps), duplicated and skipped stamps, and the longest
  gap between new stamps (freezes). Calibrate first: measure the method's own floor (pattern shown
  in a local page with no stream) and report it with every result, and state the sampling quantum
  (a readback loop tied to a 60 Hz callback adds up to one period of uniform delay — the same for
  every build, so comparisons hold).
- **Audio, end to end.** Play a train of short clicks or tone bursts at known host times; detect
  each onset at the client's audio output with an analyser tapped before the destination; report
  the delay distribution, whether the signal arrived at all, its level, and silence before/after.
  Mute Chrome's real output (`--mute-audio`) so the host does not capture the client's playback.
- **Secondary data, when the build has it:** `SEVideo.getStats()`, `SEAudio.getSyncInfo()`, WebRTC
  `getStats()` (inbound video/audio, data channels, candidate pair), host `status --json`
  telemetry, which video and audio path the client ended on, and every page console warning/error.
- **Join timing:** form submit → answer received → first painted frame, separately.
- **Machine load:** sample CPU and GPU utilisation (`nvidia-smi` is available) before and during
  the timed window and include it, so a noisy run can be recognised.

### 5.4 Standard scenarios

Every test runs these unless the request narrows them. Set the client's screen size, device pixel
ratio and window size explicitly through the browser protocol, because headless defaults (800×600)
are not what users have:

| Scenario | Purpose |
|---|---|
| static desktop, 30 s | idle cost, keepalive cadence, no flicker (painted frames must be pixel-stable; flag green or banded frames) |
| small moving window | pointer-like motion (the stand-in for a cursor) |
| large area changing every frame | sustained full-motion load |
| 1920×1080 at 60 Hz, DPR 1 | the common client |
| a high-resolution, high-refresh client (for example 2560×1440, DPR 2, 120 Hz) | the display's own 360 Hz panel and a self-reported high refresh have caused collapses before (`b0183dd`, `170138e`) |

### 5.5 Special requests

The request JSON selects extra checks and conditions. Implement at least these; make it easy to add
more, and include a `custom` hook (a JS snippet evaluated in the client page, and a host CLI command
list, with results returned) so an agent can test something nobody listed:

| Request | What it does and returns |
|---|---|
| `host_env`, `host_args`, `chrome_flags`, `client` (size, DPR, refresh, HTTP vs HTTPS), `duration`, `scenarios`, `runs` | conditions; `runs: N` repeats the timed window and returns per-run and aggregate numbers |
| `mic_prompt` | joins with a chosen set of granted permissions and reports: was a microphone permission requested (wrap `getUserMedia` and the Permissions API), did the page's own microphone card appear, did `enumerateDevices` return output labels without it, does `selectAudioOutput` exist. Run it with speaker access already available and with nothing granted. The defect to catch: a mic prompt when outputs are already available |
| `audio` | enables system audio for the test device, runs the audio measurement, reports the path taken and why if it fell back |
| `settings_change` | mid-session, changes one device setting at a time through the CLI (each class: name; audio toggle; quality; video scale; refresh rate; DPR; orientation; display scale). After each: number of live joins, join id, number of attached displays, display id, whether the device row was removed and re-added, time until the picture recovered, whether the stream was rebuilt. The defect to catch: two devices and two virtual screens after one edit |
| `two_clients` | two browser instances on one host; change a setting on one; assert the other's stream is untouched (no gap in its stamps) |
| `leave_rejoin` | close the tab; assert the display is removed after the grace period; rejoin inside the grace period and assert the same display is reused |
| `slow_answer` | repeats the join N times and reports any join where the answer took over 5 s, the page gave up, or a second session/display appeared (seen once: "The host did not answer in time" → fallback → a second session and display) |
| `auto_rejoin` | restarts the host under a live page and reports what the page does |
| `logs` | `ScreenExtend logs` and `status --json` return data while a session is live |
| `fallback_paths` | forces each client tier (WebCodecs off, script transform off, cross-origin isolation off via plain HTTP, hardware decode off) and reports latency on each |
| `loss` | only if it can be done without admin rights or input; otherwise report it as not run |

Reject any request that asks for input injection, and say why.

### 5.6 Facts already learned (do not rediscover them)

- Session ids must match `/^[23456789ABCDEFGHJKLMNPQRSTUVWXYZ]{12}$/`; the page rejects others.
- The host's certificate is self-signed: `--ignore-certificate-errors`.
- `Browser.grantPermissions` **denies every permission it does not name**. Naming only the
  microphone is what made Wake Lock fail in an earlier run.
- The page raises a `beforeunload` dialog; unhandled it hangs the run. Accept it with
  `Page.handleJavaScriptDialog`.
- Joining from the host machine shows a same-device card that must be dismissed.
- `devices set <ip>` refuses when two devices share an address; address the device by token (from
  `status --json`). Enabling audio that way also saves a full override row, which triggers one
  renegotiation the first time.
- A ScreenExtend tab left open in the owner's browser rejoins by itself when a host appears on the
  default ports, adding a display and a stream. Non-default ports avoid it; in exclusive mode,
  detect the extra join and report it instead of measuring through it.
- Headless Chrome on Windows needs no GPU flags for WebCodecs, hardware decode, encoded transforms,
  `AudioWorklet` or `SharedArrayBuffer`.
- A worker script on a cross-origin-isolated page needs its own
  `Cross-Origin-Embedder-Policy` header, and an encoded transform must be installed in the same task
  as `addTransceiver` (both fixed at `d4d8be9`; older revisions fall to slower paths — that is a
  finding about those revisions, not a harness fault).
- A release build takes 12–25 minutes cold. `67e36c6` turned on fat LTO with one codegen unit
  (244 s → 698 s). Each concurrent builder needs its own `CARGO_TARGET_DIR`
  (`C:\Users\sarve\.cargo\shared_target\ScreenExtend-<name>`); watch free disk. The client page is
  compiled into the binary, so a page edit needs a rebuild. `tauri_build` needs `dist/` (run
  `pnpm build` once, or copy it).

### 5.7 Acceptance

The handler is done when: four simultaneous invocations against the current head all complete with
their latency windows serialised; a fifth queues; an exclusive-mode run against a build without the
isolation switch completes and leaves `config.json` byte-identical; five consecutive standard runs
on the current head are reported with their spread (this spread is the noise figure everyone uses);
and after every run no host, test browser or virtual display remains. Write `scripts/test-handler/README.md`.

## 6. Step 2 — baselines and the bug census

Build the old app (`bef5f89`) and the current head. Run the full standard test and every special
request against both, five runs each, interleaved (old, new, old, new…) so drift on a busy machine
cancels. Write `docs/overhaul-new/BASELINE.md`: one table of old vs current for every metric and
scenario, and a **bug census** — every defect either build shows, each with a reproduction through
the handler. Start from the owner's list in §1 and these, then add what you find:

- the second session and display after a slow answer; a display left behind after a leave
- intermittent green or banded frames on a static desktop (history: `d996635`, `f66a5a7`, `9ffd3ad`, `abd1d2c`)
- collapse at high refresh or high resolution
- join time (the page once waited on the microphone card and on the end of ICE gathering: `44f3f6d`, `5f6042c`)
- audio missing, late or falling back to the plain track
- the client being demoted to `<video>`
- anything the fallback tiers do worse than the old app did

Cursor glitches cannot be reproduced without moving the cursor. Use the small moving window as the
stand-in for capture behaviour, analyse the cursor-compositing code by reading it (`b0183dd`,
`d848234`, `a871ec7`, `6faa359`), and record what remains for the owner to check by hand (§9).

## 7. Step 3 — one agent per commit group

Start one Opus 5.5 agent for each of **G01–G09** from `docs/commit-groups/COMMIT-GROUPS.md`:

| Group | Area | Commits |
|---|---|---|
| G01 | Windows capture: WGC, DXGI, cursor compositing, scaler, frame gates, thread tuning | 24 |
| G02 | Windows encode: NVENC, the async ring, x264, encoder settings | 12 |
| G03 | Transport: packetizer, pacer, congestion control, keyframe gate, repair, ICE, runtime width | 22 |
| G04 | Session and display: join, reconfigure, leave, identities, virtual display, DPR | 21 |
| G05 | Client video path | 11 |
| G06 | Audio: Windows capture, Opus, client audio, output picker, microphone card | 23 |
| G07 | Client page: join form, modals, reconfigure polling, client ICE | 11 |
| G08 | Telemetry, probe, tests (measure-only code: confirm it costs nothing on the hot path, otherwise leave) | 51 |
| G09 | Host core: CLI, lifecycle, single instance, log bus, config, relay, COM | 30 |

Not given an experimenting agent: **G10** desktop UI (later), **G11** input (§9), **G12** macOS,
**G13** AMD/Intel, **G14** docs/CI/build. Their code stays as it is on `latency-overhaul`. One
exception inside G14: the release profile (`67e36c6`) is worth one measured comparison.

Each agent works in its own git worktree branched from `latency-overhaul-new`, with its own
`CARGO_TARGET_DIR`, and measures only through the handler. Run as many agents at once as the
machine can build for (three concurrent release builds is a sensible ceiling); the handler already
keeps their measurements apart.

**The method each agent follows** (put this, §3, §5.6 and the group's rows in its brief):

1. **Read first.** Every commit in the group: the diff, not just the row. Then the matching
   `docs/OPTIMIZATIONS.md` entries and `docs/FINAL-REPORT.md` sections. List what each commit
   claims, what it changes by default, and which later commit undid or altered it. Note
   optimisations that were lost: something that measured well and was later reverted or disabled
   for a side effect.
2. **Baseline.** Measure the group as it stands (all its commits included, the branch head): the
   standard test plus every special request that exercises a feature the group touches, 3–5 runs.
   Write down every defect seen. This is the baseline whether it is faster or slower than the old
   app.
3. **Change one thing at a time, slowly.** Candidates: remove one commit (or one behaviour of a
   multi-part commit); flip a switch a commit added; restore the old app's version of one function;
   repair a defect you diagnosed; bring back a lost optimisation in a form that avoids the side
   effect it was reverted for. For each candidate: build, measure 3–5 runs interleaved with the
   current best, decide, record.
4. **Keep only what is better.** A change is kept only if it lowers end-to-end latency (or raises
   painted fps, or shortens the join) by more than the noise, or fixes a defect — and makes nothing
   else worse. "More than the noise" means: across 3–5 interleaved runs each, the candidate's median
   beats the current best's median by more than the larger of the two run-to-run ranges, on p50
   without a worse p99. If the ranges overlap, run up to five each; if they still overlap, it is not
   a win. Everything not kept is reverted and **documented** with its numbers and the reason.
5. **Stay in your group, and off input.** Do not experiment on another group's files; if your
   change needs one, say so in the report. A commit that also touches input files: experiment on
   the non-input part only and leave the input files byte-identical.
6. **Gates before handing back:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test` (no host running), `pnpm test:client`, and `node scripts/check-legacy-webkit.mjs`
   if the client page changed. A regression test for each defect fixed, where one is cheap.
7. **Hand back:** the branch with one commit per kept change (the message carrying before/after
   numbers), and a ledger `docs/overhaul-new/<group>.md`: a table of **every** experiment — what
   was changed, runs, numbers, verdict (kept / discarded / inconclusive), why — then the defects
   found, fixed and still open, the lost optimisations recovered or ruled out, and anything
   unverified.

Leads from the classification pass, to check early (unverified):

- **G01:** `2ad6ca3` drops WGC frames with empty dirty regions (active on this OS build, never
  tested against a stall); `d071133` pins real-time threads to efficiency-class CPU sets on this
  hybrid CPU and drops the 60 fps floor; `408099e` reworks adapter matching (could be a real win on
  hybrid graphics); the cursor history `b0183dd` → `d848234` → `a871ec7` → `6faa359` (59 → 113 fps
  claimed for a pointer sweep); `3bcd1e8` widens where WGC is attempted.
- **G02:** `f66a5a7` (references 1→4, long-term references, intra-refresh wave, async encode;
  "async is a wash" by its own numbers) later undone piecemeal by `9ffd3ad` and `abd1d2c`, so a loss
  is now answered with a keyframe; `e9c521b` and `83cc915` move quality between constant QP and a
  QP floor under CBR; `9c4499a` honours a keyframe request on a static desktop (a real fix).
- **G03:** `8feb63c` (own packetizer, measured 1.65× slower than stock at the time; forced
  High/Main profile; a socket tier later turned off by `fca80af`; scan fixed by `948e312`);
  `42d364a` wires the pacer, the delay-based estimator and keyframe suppression into every session;
  `932e0bc` narrowed the runtime to 4 workers and `3791155` widened it to 16; `f5d0e27` fixed an RTT
  unit bug that pinned sessions at 1 Mbps; `34ae41d` cuts bitrate continuously from 1 % loss;
  `214e5d6` (transport-cc only when offered, p50 3.00 → 2.10 ms claimed); `d996635` keyframe window
  by path; `de7de8b`/`94adab7` net to zero.
- **G04:** `babdd0c` (sessions keyed by join id, with follow-ups `0db7fc0`, `f569e41`, `670dbb7`,
  `65fe249`) is the first place to look for the two-devices-two-screens defect; `0ca3f3c` overlaps
  the handshake with display creation; `2dba8b9` replaces a 100 ms poll; `8682d0e` applies display
  scale live; `c0bdd73` moves blocking join calls off the async workers; `56fe583` forced 120 Hz
  and turned intra refresh on; `170138e` removed the 60 Hz refresh cap `b0183dd` had added.
- **G05:** `5f6042c` (large: script transform, microphone prompt off the join path, shorter ICE
  gather, fullscreen handling); `fd2c1d0` changes when frames are dropped and keyframes requested;
  `d660b84` (the transform fix, already in).
- **G06:** `7438858` removed the bridge thread and cut broadcast capacity 128 → 16; `dfd5dc9` added
  about 15 ms of audio delay that `f36f2ec` removed; audio is off by default per device; the
  microphone card.
- **G07:** `44f3f6d` puts the microphone card back before the join with no timeout and holds the
  stage until ICE gathering ends (its message admits the join-time cost); `a38ad21` adds modal and
  fullscreen timers; `451cff5` adds a rejoin path with a 2 s deadline.
- **G09:** `b8afb8f` reroutes COM initialisation for capture and audio threads; `logbus` and the
  `logs` path, for "logs do not come through".

## 8. Step 4 — consolidation

Merge the groups' kept changes into `latency-overhaul-new` one group at a time, in dependency order
(capture and encode first, then transport, session, client, audio, host core). After each merge run
the full standard test and the special requests, five runs, against the pre-merge state. A win
measured alone that disappears or reverses in combination is investigated before moving on: changes
interact (a faster capture can expose an encoder queue, a narrower runtime can starve input), so
re-test the pairs that touch the same stage. Resolve conflicts by measurement.

Finish with: the full gate (`cargo fmt --check`, clippy, `cargo test`, `SE_REQUIRE_HW=1 cargo test`,
`pnpm lint`, `pnpm build`, `pnpm test:client`, `pnpm test:config`, `pnpm test:gate`, the four
`scripts/check-*.mjs`), a `pnpm tauri build` with the real release profile, and a last five-run,
three-way interleaved comparison: old app vs `latency-overhaul` head vs `latency-overhaul-new`.

## 9. Step 5 — the input report

One agent, **reading only**, writes `docs/overhaul-new/INPUT-REPORT.md`. It runs nothing that
injects, captures or measures input. It covers the 16 commits in G11 and the 27 other commits that
touch input files (43 in all; `commit-groups.json` marks them `touches_input`): what each changed,
what it claims, what could cause the cursor glitches the owner reports (queue depth and shedding in
`35986dd`, the pointer curve and base position in `420d76f` / `301230d`, batching in `7ce9d34`, the
pointer-scope default flips in `d42f0df` / `f957022`, and the capture-side cursor path from §6),
which changes look risky by reading alone, and a step-by-step **manual test plan the owner can run
by hand**. It proposes changes; it makes none.

## 10. What you hand back

`docs/overhaul-new/REPORT.md`:

1. The verdict in one paragraph: is `latency-overhaul-new` faster than the old app and than the
   previous head, by how much, on which scenarios.
2. The three-way table (old / previous head / new) for every metric and scenario, with run counts
   and spreads.
3. Every change kept, with its measured effect and the commit it came from or replaced.
4. Every change tried and not kept, with numbers.
5. The bug census: fixed (with the test that proves it), still open, and not testable here.
6. What was left untouched and unverified: input, macOS, AMD/Intel, desktop UI.
7. Anything unexplained.

## 11. Step 6 — `FUTURE-TEST-WORKFLOW.md`, written last

This job leaves real defects untested, because its rules forbid the only ways to reach them. When
everything above is finished, write `FUTURE-TEST-WORKFLOW.md` at the repo root and commit it to
`latency-overhaul-new`. It is the brief for a later job run under different rules (the owner
present, input allowed, a second device available), and like this file it will be handed to an
agent **byte for byte as its whole prompt** — so it must stand alone: no "as discussed", no
reference to this conversation, every path, commit and number written out.

**What goes in it: everything this job did not or could not test.** Keep a running list from the
first day (every agent's ledger has an "unverified" section; collect them) so nothing is
reconstructed from memory at the end. At minimum:

- **Cursor glitches** — the composited pointer stuttering, lagging, jumping, vanishing or being
  drawn twice; behaviour on a static desktop, over video, at high refresh, across two displays.
- **Input glitches and input lag** — pointer, keyboard, wheel, touch, clipboard; dropped or
  reordered events; stuck keys and buttons; the pointer curve feeling wrong (too slow, too fast,
  doubly accelerated); click-to-photon and motion-to-photon latency; behaviour under a 1 kHz mouse.
- **The cursor or the stage being defocused** — pointer lock or keyboard lock dropping by itself,
  focus leaving the client stage or the host window, fullscreen exiting, a modal that cannot be
  clicked, the pointer escaping the display it is scoped to, what happens after alt-tab, a
  reconfigure or a reconnect.
- **Desktop UI errors** — every page of the Tauri window: the dashboard device list, device
  settings, the settings page and its config editor, the log panel ("logs do not come through"),
  onboarding, notifications, anything that renders wrong, errors, or shows stale state.
- **Everything this machine cannot run** — macOS (every capture, audio and display tier), AMD and
  Intel encoders, real client devices (phones, tablets, Safari, Firefox, older browsers), real
  Wi-Fi with loss and roaming, the cross-network relay and TURN, the hosted network, the installer
  and auto-update, and the join path below Windows 11 24H2.
- **Anything a group agent marked unverified, inconclusive or unexplained**, and every change this
  job kept whose benefit rests on something it could only measure indirectly (for example, a
  capture change justified by the moving-window stand-in rather than by a real cursor).

**For each item, write:**

1. What the defect or risk is, in one or two sentences, and who reported or suspected it (the
   owner's complaint, a ledger entry, a reading of the code).
2. Why this job could not test it (which rule or missing hardware).
3. The commits and files most likely involved, from `docs/commit-groups/`, the input report and
   the group ledgers, with what each is suspected of.
4. What this job learned about it without testing: findings from reading the code, related
   measurements, hypotheses ranked by likelihood.
5. **How to test it** — the exact procedure, the equipment and permissions it needs (for example
   "the owner's explicit permission to inject input, on a machine nobody is using", "a 240 fps
   camera", "an Intel Mac on macOS 12.3+"), what to measure, the tool to measure it with, and the
   number or observation that counts as a pass. Say which checks can be automated by extending
   `scripts/test-handler/` and which need a human watching a screen.
6. What to do with the result: which change to try first if it fails, and how to decide whether to
   keep it (the same keep-only-if-better rule as §7, with the same run counts).

**Also include**, so the next agent starts where this one stopped:

- The state of the code: the branch, the head commit, what `latency-overhaul-new` contains relative
  to the old app and to `latency-overhaul`, and where the reports and ledgers are.
- The test handler: how to run it, what it measures, its noise figure, and exactly which requests
  would have to be added for the future tests (an input-latency request, a cursor-capture request,
  a GUI-driving request), with a design sketch for each.
- The rules that still hold (never push; the owner's machine and config; probe capabilities, never
  version numbers; no change to how the UI looks unless asked) and the rules that the future job
  must have **explicitly lifted by the owner before it starts** — above all the ban on touching
  input. Write that as a precondition at the top: if the owner has not lifted it, the future agent
  must not inject input either, and must stop and ask.
- An ordering: which items are most likely to matter to a user, which are cheapest to check, and a
  suggested sequence.

Do not test any of it yourself to fill the file in. If you catch yourself needing to move the
cursor or open the desktop UI to finish a sentence, the sentence is "not tested; here is how".

Then stop. Do not push, do not merge into `latency-overhaul` or `main`, and leave the machine with
no host, no test browser and no extra display running.

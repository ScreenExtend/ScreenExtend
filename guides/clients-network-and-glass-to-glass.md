# Clients, network conditions and the glass-to-glass rig

**A standalone autonomous directive.** You are a fresh Claude Code instance with no access to the
conversation that produced this file. Everything you need is in here and in the repository you are
sitting on. Read this file to the end before you touch anything.

ScreenExtend is a Tauri 2 desktop app that turns any device with a web browser into a wireless
second monitor. It runs on the **host** (the PC being extended); a **client** (phone, tablet,
laptop) joins by opening a URL, with no install. The host creates a real virtual display per
client, GPU-encodes it, and streams H.264 over WebRTC with WHEP signalling. The client is a
dependency-free browser page (`src-tauri/src/streamer/static/`) served by the host's own axum
server and `include_str!`'d into the binary — no bundler, no build step, no npm.

The project has two development machines: a Windows 11 build 26200 laptop with an RTX 3080 Ti, and
a macOS 10.15.8 Intel MacBook reached over SSH. Between them they have measured the host pipeline
to the socket in detail. **Neither has ever run the client page.** The Windows box is the host, so
a browser on it is the same-device case; the Mac's Safari is 13.1, below every fast path the page
has. Every capability tier in `docs/CAPABILITY-TIERS.md` whose subsystem name starts with
"Client" is `UNVERIFIED`. Every network condition that is not a quiet LAN is unmeasured. Nobody has
ever pointed a camera at a host screen and a client screen at the same time.

**This guide closes all of that.** It is the only guide in `guides/` whose constraint is the client
and the link rather than the host, and it carries 34 inventory items plus a hyper-optimisation
sweep. It is written to be usable two ways at once: with a phone in your hand, working through a
device matrix; and as an autonomous optimisation directive, where **every fallback that silently
activates on a real device is a defect to fix and every millisecond found is to be shaved**.

---

## 0. The machine contract

### What you must have

**Host — one of these two, no substitutes:**

| Option | Minimum | Why |
|---|---|---|
| Windows | build **26100** or newer (24H2 / 25H2), with an **NVIDIA GPU** carrying NVENC | 26100 is where `GraphicsCaptureSession.MinUpdateInterval` and `DirtyRegionMode` exist, so the Windows.Graphics.Capture path is the one already measured; NVENC is the only backend with long-term reference frames wired up (item C-25), and its numbers are the ones `docs/BASELINE.md` holds |
| macOS | **14.2** or newer | ScreenCaptureKit video (12.3+), SCK audio (13.0+) and the Core Audio Process Tap (14.2+) all exist, so the host half is a configuration the project has verified rather than a variable |

The host must be a configuration this project has already characterised, because the whole point
of this guide is that a bad number is attributable to the **client or the link**. If your host is
an AMD box, a Windows 10 machine or an Intel Mac on 12.x, stop — those are three other guides
(`windows-amd.md`, `windows-legacy.md`, `intel-mac.md`) and running this one there produces numbers
nobody can interpret.

**No maximum OS.** A Windows 11 26200+ or a macOS 15/26 host is fine and preferred.

**Clients — the matrix.** More is better; the rows in bold are the ones that decide items on their
own and cannot be substituted:

- **iOS/iPadOS ≥ 18.4** (Safari) — `outputLatency`, `unadjustedMovement`, `setSinkId` all arrive here
- **iOS/iPadOS ≤ 17** (Safari) — the "old WebKit" arm: no `outputLatency`, no `AudioDecoder`
- **an iPhone specifically** as well as an iPad — fullscreen behaves differently and only the iPhone has an earpiece
- **Android Chrome ≥ 141** and **Android Chrome 86–140** (or Samsung Internet, whose version trails)
- **desktop Chrome/Edge ≥ 141** and **desktop Chrome ≤ 140**
- **desktop Firefox ≥ 130** and **Firefox ≤ 129** (130 is where `AudioDecoder` shipped)
- **desktop Safari 26** if you can reach one (`AudioDecoder` shipped there), and an older Safari
- **one constrained-baseline-only H.264 decoder** — an old Android, a smart TV browser, a cheap tablet
- **two pen-capable devices** — iPad + Pencil, a Surface, an Android with an S Pen
- **a Bluetooth audio sink** paired to at least three different client OSes
- an **iOS 12/13-era device** if one exists, for the parse-safety row

**Network:**

- a **Wi-Fi 5 (802.11ac)** AP and a **Wi-Fi 6 (802.11ax)** AP, ideally the same SSID on both bands
- a **second network** that cannot route to the first, plus a **real TURN server** you control
  (coturn on a VPS is enough; `ScreenExtend turn set` takes the URLs)
- an **IPv6-capable network** (a v6-only segment if you can build one)
- a way to put the host on **wired Ethernet** and an **Android phone on a USB cable** for tethering
- `iperf3` on host and on at least one client-side machine
- **`clumsy`** on Windows, or **`dnctl` + `pfctl`** on macOS, for injected loss/latency/reorder

**Measurement:**

- a **240 fps camera** — a recent iPhone or Android in slow-motion mode is enough. This quantises
  to 4.17 ms and that number must be stated with every result.
- the competitor apps for the side-by-side: **spacedesk**, **Deskreen**, **Duet Display**,
  **Parsec**, **Moonlight + Sunshine**, **Steam Link**, and **AirPlay to an iPad** if the host is a
  Mac. Free/trial tiers are fine.

### Preflight — prove the machine qualifies before you start

Run these first and paste the output into `docs/discovery/G5-machine.md` (create it).

```sh
# --- Windows host ---
[System.Environment]::OSVersion.Version            # Build must be >= 26100
(Get-CimInstance Win32_VideoController).Name       # an NVIDIA part must be listed
nvidia-smi --query-gpu=name,driver_version --format=csv

# --- macOS host ---
sw_vers                                            # ProductVersion must be >= 14.2
system_profiler SPHardwareDataType | head -20

# --- both: build the binary this guide measures ---
cd src-tauri
cargo build --release --features probe
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

# --- both: prove the host half works before you blame the client ---
./target/release/ScreenExtend doctor --json
./target/release/ScreenExtend probe --whep-selftest --duration 30 --encoder auto
# expect: PASSED, a four-digit RTP packet count, and a per-stage summary on exit

# --- the offline client harnesses must pass before any client change ---
cd ..
node scripts/test-client-stats.mjs
node scripts/test-client-audio.mjs
node scripts/test-client-ice.mjs
```

If `probe --whep-selftest` does not pass, **stop and fix that first** — every client number you
take afterwards is measured against a host that is not working, and you will spend days blaming
browsers.

> **Measurement trap, learned the hard way.** Leave **≥ 15 s between probe runs**. A probe that
> exits without tearing down leaves its virtual display attached, and the next run attaches to the
> stale display and streams nothing. This corrupted 4 of 12 runs during the host measurement pass;
> every one of them opened with `[display] already extended (2 monitor(s), 2 source(s))`.

---

## 1. Model and orchestration directives

**Every subagent you spawn runs on Claude Opus 5.** No exceptions, no "this one is simple". If a
subagent spawns its own subagents, those are Opus 5 too.

**You are running fully autonomously. There is nobody to ask.** The user is not watching this run
and cannot answer a question. Therefore:

- **Never ask a question and wait. Decide.** Where anything here says "propose" or "consider",
  that instruction is superseded: make the best call on the evidence, act, and write down what you
  decided and why. Every such decision goes in the final report under a clearly-labelled
  **Decisions Made Without Consultation** section so it can be reviewed and reversed.
- **When in doubt, build it.** A feature that turns out to be unwanted is a one-minute revert; a
  deferred idea costs a future agent a full re-research cycle. You are holding the context. Spend it.
- **Self-unblock.** Missing information is a research task, not a blocker. Missing tooling is an
  install decision you make yourself. A failed approach is data — record it and try the next one.
- **Leave the tree reviewable.** No half-finished refactors, no debug scaffolding in shipping paths,
  no commented-out experiments. Anything incomplete is finished, reverted, or isolated on a clearly
  named local branch and called out in the report.

**Parallelise hard, with one caveat this guide has that the others do not.** The device work is
serialised on *you holding the phone*. So partition the way §8.7 sets out: agents run the code
changes, the research, the analysis, the ledger writing and the cold reviews concurrently, while
the physical sweeps happen in batches — join once per device and collect every measurement that
device can give in that one session, rather than joining eleven times for eleven items.

**Token and time cost are not constraints.** The engagement standard, verbatim from the user:

> "Even if there's some obscure fix for exactly one version of an operating system or weird browser
> that shaves off exactly 0.1 millisecond, I want that. I want absolutely every possible item
> optimized. Never assume anything is impossible unless web search and actually trying it. If even
> one app — closed source, paid, whatever — has figured it out, there is a way and we must figure it
> out as well."

Take that literally. "Good enough", "diminishing returns", "minor" and "speculative" are not
reasons to stop. A speculative idea is resolved by trying it and measuring, or by a **recorded
rejection with a reason and a number** in `docs/OPTIMIZATIONS.md` — never by being skipped.

---

## 2. Absolute rules

### 2.1 Git

- **Work on a local branch `guide/clients-network-and-glass-to-glass`**, cut from `latency-overhaul`.
- **NEVER `git push`. Never touch `origin` in any way.** No push, no force-push, no tags, no pull
  requests, no issue or release edits, not even a `--dry-run` you then "just try for real". `origin`
  is read-only to you. The user reviews everything and pushes personally, afterwards. There is no
  situation in this guide where pushing is the right call.
- Locally you are unconstrained: branch, commit, stash, worktree, rewrite history freely.
- Commits are small and single-purpose. **A performance commit states its measured before/after in
  the message.** Commit at least every 30 minutes so a lost session costs minutes, not hours.
- Every commit message ends with:

  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: <the URL of your own session>
  ```

### 2.2 No visual change

**The client page and the desktop UI keep their current appearance.** Element for element, class
for class, word for word. No control added or removed, no layout moved, no copy reworded, no colour
changed. This is a constraint on *appearance*, not on performance — everything behind the pixels is
explicitly in scope and is the point of this guide.

If a fix you want requires a visible change, **do not make it.** Implement the fastest version that
preserves the current appearance and write the visible alternative up in `docs/OPTIMIZATIONS.md`
under "UI-visible recommendations (G5)" with its measured upside, so the user can approve it later.
There is precedent: `SEAudio.pickOutput()` in `audio.js` is a complete, correct implementation of
Firefox's permission-free `selectAudioOutput()` picker, and **nothing calls it**, because calling it
needs transient activation and the page has no control to hang a click on. Adding one is a visible
change. It stays uncalled, and the recommendation stays written down. Do the same with anything you
find in that shape.

### 2.3 The scope fence

Everything outside the list below **has already been audited, optimised and verified on the two
development machines. Do not modify it.** A change outside the fence is a merge conflict against
work you cannot see. You may *read* anything, and you may *measure* anything.

**Yours to change freely:**

```
src-tauri/src/streamer/static/**        the whole client page
  index.html  transform-worker.js  audio.js  audio-worker.js  audio-worklet.js
  input.js    nosleep.js  styles.css  logo.svg  at-capacity.html
scripts/test-client.mjs                 the runner: `pnpm test:client`
scripts/test-client-stats.mjs           input.js wire format + telemetry
scripts/test-client-input.mjs           input.js golden vectors, cross-checked by the Rust parser
scripts/test-client-audio.mjs           audio-worker.js decode, redundancy, the sequence gate
scripts/test-client-worklet.mjs         audio-worklet.js jitter buffer + the planar ring ABI
scripts/test-client-transform.mjs       transform-worker.js WebCodecs path + A/V sync
scripts/test-client-ice.mjs             index.html ICE gathering
scripts/test-client-reconfig.mjs        index.html /reconfig epochs and kick
docs/manual-tests/client-page.md        this guide's own test script
docs/manual-tests/transport.md
docs/BASELINE.md  docs/RESULTS.md  docs/OPTIMIZATIONS.md  docs/CAPABILITY-TIERS.md
docs/TELEMETRY.md  docs/INPUT-PROTOCOL.md   (the wire-format docs the client half defines)
findings/**
```

**Yours to *tune and defect-fix*, not to restructure** — these are the host↔client contract and the
transport shaping whose constants exist to be set by exactly the measurements in this guide:

```
src-tauri/src/streamer/input/protocol.rs     HOST_CAPABILITIES bits, the optional tails, parse
src-tauri/src/streamer/telemetry.rs          parse_client_stats, CLIENT_DEGRADED_NAMES, counters
src-tauri/src/streamer/server.rs             routes, /net-config, /ice-config, locality_for
src-tauri/src/streamer/webrtc_session.rs     MTU, playout-delay, redundancy_now, BWE loop
src-tauri/src/streamer/negotiate.rs          H.264 profile/level selection
src-tauri/src/streamer/sockets.rs            DSCP / qWAVE / SO_NET_SERVICE_TYPE
src-tauri/src/streamer/pacer.rs              pacing tiers and their thresholds
src-tauri/src/streamer/congestion.rs         TWCC / REMB / receiver-report tiering
src-tauri/src/streamer/bitrate.rs            estimate_from_loss, the driver's step response
src-tauri/src/streamer/keyframe.rs           the keyframe gate window
src-tauri/src/streamer/repair.rs             LTR repair policy
src-tauri/src/streamer/cloud.rs              the relay control channel
src-tauri/src/streamer/session.rs            per-session state the client's join populates
src-tauri/src/streamer/audio/protocol.rs     the 13-byte DataChannel header and FLAG_REDUNDANT
```

**Off limits entirely:** every capture backend, every encoder backend, the virtual display and its
driver, the audio capture backends, `src/` (the React desktop UI), `cli.rs`, `lib.rs`, the build
configuration, the installer, `windows_utils/`, `macos_utils/`, `linux_utils/`.

### 2.4 The invariants that bind this area

From `CLAUDE.md`, which you should read in full during Phase 1. These are the ones you can break:

1. **The client page has no build step and no dependencies.** It is `include_str!`'d verbatim into
   the binary and served as-is. It must stay dependency-free, and it must keep **parsing** on old
   mobile WebKit — the whole page is one shared scope, so a single unsupported token anywhere kills
   the join form itself, silently, with the Connect button still enabled. The current floor is
   measured and documented: every file parses at **ES2017 (acorn `ecmaVersion: 8`)** or below,
   governed by `async`/`await`; `audio-worker.js` and `audio-worklet.js` are ES2015. Nothing in the
   tree requires ES2018 or later. **Keep it that way**, and re-verify with the acorn method in
   `docs/reviews/client-review-3-old-browsers.md` §1 after every change, not with `node --check`
   (Node accepts ES2024 and proves nothing).
   Forbidden constructs, currently at zero occurrences and to stay there: `?.`, `??`, `||=`, class
   fields, `#private`, top-level `await`, numeric separators, BigInt literals,
   `getBigUint64`/`setBigUint64`, `import()`, regex lookbehind / named groups / `s` flag, object
   rest-spread, optional catch binding, `Array.prototype.at`, `.flat`, `Object.fromEntries`,
   `String.replaceAll`, `Promise.allSettled`, `globalThis`, `structuredClone`, `queueMicrotask`.
   In CSS: flex containers may not use `gap` (Safari 13.1 has it for grid only) and `inset:` must
   stay expanded to its four long-hands.
2. **Probe capabilities, never version numbers.** Every tier is selected by attempting the real
   capability and falling back at runtime without crashing, and the selected tier announces itself.
   `typeof AudioDecoder === 'function'` is not a probe — Safari 26 and Firefox 130 pass it and then
   refuse `opus`, which is exactly why `verifyCapabilities()` decodes a real 3-byte Opus frame and
   `copyTo`s it before claiming the fast path. Hold that standard for anything you add.
3. **`std::sync::Mutex` is the one blocking mutex type in first-party Rust, and a `MutexGuard` is
   never held across an `.await`.** Lock, copy what you need, drop the guard, then await. Watch the
   `if let` / `match` scrutinee temporary-lifetime footgun: the guard lives to the end of the block.
   The only two intentional async locks are `DISPLAY_CORRELATION_LOCK` in `streamer/server.rs` and
   `receive_error` in `windows_utils/driver_ipc/client.rs`.
4. **Real-time threads take no locks and do not allocate.** Audio capture threads and video capture
   callbacks talk to the async world over `crossbeam-channel` only.
5. **`tprintln!` / `teprintln!`, never `println!` / `eprintln!`,** in the Rust core.
6. **On macOS: no link-time references to APIs newer than 10.15.** `dlsym` / `AnyClass::get` /
   `msg_send!` only; `extern static` constants count. dyld binds every non-weak undefined symbol
   before `main()`, so one such reference means the app will not launch on Catalina at all. If you
   touch anything that could add one, run
   `scripts/check-macos-dyld.sh target/release/ScreenExtend --floor 10.15` and `nm -u`.
7. A new Tauri command goes into **all three** `*_utils` modules and `collect_commands!`; never
   hand-edit `src/lib/bindings.ts`. (You almost certainly need neither.)
8. **No AI-tooling references in code comments or docs.** No model names, no "generated by", no
   agent-directed instructions left in source. Commit messages read as ordinary engineering commits.

---

## 3. Norms of research and measurement

**Research before you conclude anything is impossible.** Load web search and Deep Research and lean
on them hard. Prefer primary sources: W3C and WHATWG specs, WebRTC and WebCodecs explainers, RFCs,
Chromium and WebKit and Gecko source and bug trackers, browser release notes, MDN's BCD data, the
source of Moonlight, Sunshine, OBS, FFmpeg and webrtc-rs, and vendor engineering blogs. **If one
shipping product demonstrably does the thing, the thing is possible** — "impossible" is only
allowed after a search found nobody doing it *and* you tried it and measured.

**Measure before and after every change, as p50 / p90 / p99 / max, never a bare mean.** Jitter is
felt far more than the average: a pipeline at 20 ms mean and 60 ms p99 feels worse than one at
30 ms mean and 33 ms p99. Nanosecond stamps come off the shared host clock
(`streamer::audio::host_now_ns`), which is the same epoch the video RTP timestamps ride, which is
what makes the host half and the client half add up.

**Never trust a flag without a read-back.** Every property set is read back and logged; every tier
selected announces itself. On the client this discipline already exists and you must extend it: a
tier byte in the stats frame, a degradation bit, or both. A capability that cannot be read back
from a running session is a capability you cannot verify.

**Measurement hygiene.**

- ≥ 3 trials per cell, report the median with p50/p90/p99/max and say how many trials.
- State whether the host was quiet. The host measurement pass found percentiles moving by a factor
  of two purely from concurrent compilers; every number here is worthless without that context.
- 15 s between probe runs (see §0).
- On a Wi-Fi measurement, record the band, the channel, the channel width, the AP model, and
  whether anything else was on the AP. A number without those is not reproducible.
- A degradation bit being **set is not a failure** — it is the page telling the truth about a
  device. What is a failure is a bit that *should* be set and is not, and a fast path claimed on a
  device that cannot run it.

---

## 4. The territory: repo map, harness, ledgers, findings, gates

### 4.1 The client page, file by file

`src-tauri/src/streamer/static/` — plain HTML/JS/CSS, served by `streamer/server.rs`, no bundler.

| File | Lines | What it owns |
|---|---:|---|
| `index.html` | ~2460 | the join form, the video stage (`<canvas>` / `<video>` swap), four modals (HTTPS upsell, limited-browser-support, same-device, microphone permission), the whole WebRTC/WHEP join, the transport ladder and its two watchdogs, the `/reconfig` poll, viewport/orientation/DPR reporting, fullscreen + pointer lock + wake lock, kick handling, `window.SEVideo` |
| `transform-worker.js` | ~840 | the WebCodecs video path: the RTP encoded transform, `VideoDecoder` configuration and probing, the outstanding-output backpressure, the OffscreenCanvas / track-generator presentation tiers, per-frame stage timing, display-lag recovery from the RTP timestamp |
| `audio.js` | ~1210 | the `AudioContext`, the worklet node, the decoder worker, the NetEQ track fallback, output-device enumeration and `setSinkId` tiering, the A/V sync target, `window.SEAudio` |
| `audio-worker.js` | ~415 | Opus decode off the main thread: header parse, dedup, `FLAG_REDUNDANT` split, concealment, `AudioDecoder`, deinterleave, ring write |
| `audio-worklet.js` | ~325 | the jitter buffer, drift correction only at silence boundaries, concealment, underrun/overrun counters |
| `input.js` | ~985 | the 24-opcode input wire protocol, pointer/keyboard/clipboard capture, the three DataChannels, PING/PONG clock sync, the once-a-second stats frame, `window.RemoteInput` and `window.SEClock` |
| `nosleep.js` | 1 (minified) | vendored NoSleep.js v0.12.0 (Rich Tibbett, MIT), the wake-lock floor |
| `styles.css` | ~530 | all of it; Safari-13-safe |
| `at-capacity.html` | 80 | the capacity-gate page, `include_str!`'d by `server.rs:550` |

### 4.2 The two console accessors

Everything the page knows about itself is behind two globals, and every number in this guide's
device sweeps is a field of one of them:

```js
SEVideo.getStats()      // videoPath, audioPath, transportTier, renderTier, keyTier, sinkTier,
                        // degraded, suppressed, hostToPaint {p50,p90,p99,max,n}, arrivalToPaint,
                        // decode, framesPainted, framesDropped, decodeErrors, keyRequests,
                        // sab, crossOriginIsolated, wakeLock, pointerLock, pointerLockUnadjusted,
                        // keyboardLock, fullscreen, setSinkId, decoderHardware, clockValid, rttMs
SEAudio.getSyncInfo()   // path, transport ('shared'|'port'|'postMessage'), targetMs, depthMs,
                        // underruns, overruns, corrections, concealed, redundantUsed,
                        // redundantDiscarded, safetyMs, outputLatencyMs, outputLatencySource,
                        // residualOffsetMs, contextState, decoderFailed, sinkTier
```

`?debug` in the URL turns on a per-second `inbound-rtp` console dump. `?present=track` selects the
experimental presentation tier. `?resetprefs` clears the "don't show again" compat preference.

How to reach a console on each device: **iOS Safari** — Safari on a Mac → Develop → *your iPhone* →
the page. **Android Chrome** — `chrome://inspect` from a desktop Chrome over USB. **Samsung
Internet on a device with no debugging** — you cannot; read the same values on the host instead,
because they ride the once-a-second stats frame and `ScreenExtend status --json` prints them.

### 4.3 The measurement harness

```sh
# whole host path, real WHEP join over loopback, per-stage telemetry on exit
ScreenExtend probe --whep-selftest --duration 30 --encoder <auto|nvidia|intel|software> \
                   --capture-backend <wgc|dxgi|auto>          # needs --features probe

# real content through the real capture path, to a file
ScreenExtend probe --live out.h264 --duration 30 --workload <mixed|static|scroll|motion|drag>

# the glass-to-glass overlay: a large frame number + elapsed-ms clock painted into the encoded frames
ScreenExtend probe --live --duration 60 --synthetic-pattern --workload motion --counter

# single-frame captures, encoder smoke, adaptive-bitrate path
ScreenExtend probe --capture cap.png | --encode out.h264 | --bitrate

# system audio: packet rate, subscriber inter-arrival, capture→subscriber latency, capture_ns skew
ScreenExtend probe --audio --duration 15

# from a running host
ScreenExtend serve -v            # one summary line per session every 5 s, with tier names
ScreenExtend status --json       # the same counters, per session, from another shell
ScreenExtend doctor --json       # capability report + per-probe timings
ScreenExtend logs --lines 400    # the 2000-line ring backlog

# offline client harnesses — no browser needed, and no substitute for one
node scripts/test-client-stats.mjs
node scripts/test-client-audio.mjs
node scripts/test-client-ice.mjs
```

`ScreenExtend serve -v` is your primary instrument for a device sweep: it names the four client
tiers in words and lists the set degradation bits by name, once every five seconds per session.
`status --json` carries the raw codes plus the decoded names plus the audio numbers.

**`--counter` caveat, and it matters for item C-34.** The overlay is painted into the *synthetic*
generator, which bypasses screen capture entirely. The camera number therefore **excludes capture
delivery**, and you must add the measured `capture_delivery` p50 for the backend under test to
compare like with like. On the reference Windows box that is 0.98 ms static / 6.29 ms live. Item
C-34 asks you to close this properly (see §8.6).

### 4.4 The ledgers, and how to write to them

- **`docs/OPTIMIZATIONS.md`** — one table row per change attempted:
  `| # | Area | Change | Hypothesis | Before | After | Status | Kept? | Notes |`. Failures are
  recorded on purpose; a reverted change with a number attached is information. Status vocabulary:
  **`VERIFIED`** (executed and measured on your machine), **`COMPILE-CHECKED`** (builds and passes
  static tests, never executed), **`UNVERIFIED`** (written from research, no runtime signal). Rows
  are numbered; the file currently runs to row 305. Add new rows at the end, and **edit existing
  rows in place** when your run flips their status — that is most of this guide's job.
- **`docs/CAPABILITY-TIERS.md`** — one row per subsystem × tier:
  `| Subsystem | Tier (newest first) | Probe | Era / platform | Expected delta | Status |`. The
  "Client …" rows are yours. Fill each Status with `VERIFIED (<device>, <browser version>)` for
  each tier you actually saw live. **A tier you did not reach stays `UNVERIFIED`** — an untested
  tier and a working one must not look the same in the ledger, which is the entire reason the
  telemetry exists.
- **`docs/BASELINE.md`** — the pre-change numbers, the competitor table, and the glass-to-glass
  procedure. Its "What could not be measured" table names *"Client arrival → paint, and
  glass-to-glass"* as the largest hole. You fill it.
- **`docs/RESULTS.md`** — **does not exist yet.** Create it with your own sections rather than
  attempting a whole-engagement results document: `# Results — clients, network and glass-to-glass`
  and then the sections §12 asks for.
- **`docs/TELEMETRY.md`** — the authority on the stats frame layout, the degradation bit table and
  the clock arithmetic. If you add a bit or a byte, this file changes in the same commit.
- **`docs/INPUT-PROTOCOL.md`** — the authority on the input opcodes, the optional client stamp, the
  event-flags byte, the reorder sequence and the host capability bits.

### 4.5 The findings queue

`findings/` at the repo root is the work queue and the crash-recovery point. It is excluded from git
through `.git/info/exclude` (not `.gitignore`). **Every problem, opportunity, suspicion and idea
becomes one file**, named `<severity>-<component>-<slug>.md` with severity in
`blocker | major | minor | speculative`, and this schema:

```
# <Title — one line>
Status: open
Severity: blocker | major | minor | speculative
Component: <subsystem>, <files>, <platform>
Evidence: what you observed, with numbers and file:line references
Hypothesis: what you think is going on, and what you are unsure about
Proposed fix: concrete enough that someone else could implement it
Verification: how to prove it worked; what to measure before and after
Context: anything a fresh agent would otherwise have to re-derive
```

**One finding per file. A file is deleted the moment its finding is resolved** — implemented and
verified, or deliberately rejected with the reasoning recorded in `docs/OPTIMIZATIONS.md` *first*.
Deleting is how you mark it done: do not archive, do not move to `done/`, do not comment it out. The
lifecycle is empty → very full → empty, and **a non-empty `findings/` at the end means the work is
not done.** Write the file when you find the thing, not at the end of a session — its whole value is
being accurate at any arbitrary moment you might be cut off.

**`findings/` is essentially empty when you start, and that is deliberate.** The nine client-scoped
findings that were open when this guide's inventory was taken were all fixed before it shipped;
§5.4 lists what each became and which C-item now regression-tests it. Read whatever is there once at
the start — files under `findings/_out-of-scope/` (Linux) are never yours, and `findings/_briefs/`
and `findings/_STATUS.md` are historical orchestration notes rather than a queue.

### 4.6 Gates

Run all of these before every commit, from the repo root unless noted:

```sh
cd src-tauri && cargo fmt --check
cd src-tauri && cargo clippy --all-targets -- -D warnings
cd src-tauri && cargo test
pnpm test:client                              # scripts/test-client.mjs — all seven harnesses
node scripts/check-legacy-webkit.mjs          # after a `pnpm build`; it also scans the client page
node scripts/check-bindings.mjs
node scripts/check-versions.mjs
```

`pnpm test:client` runs every harness in one go and exits non-zero if **any** of them did, after
running all of them, so one failure does not hide the rest. Each harness loads the shipped `.js`
files with `vm` and the smallest possible browser stubs and leaves
`src-tauri/src/streamer/static/` byte-identical. They are the only automated coverage the client
page has and **extending them is part of every change you make**, not a follow-up.

**Skipping convention on the Rust side.** A hardware- or OS-gated test never returns silently: it
calls `crate::test_support::skip(what, reason)` or `skip_because(what, reason)`, which prints
`SKIP <what>: <reason>` on stderr, and **`SE_REQUIRE_HW=1` turns a skip into a failure**. Set it on
this host for every capability it is supposed to have. Anything you add follows the same pattern,
with `#[ignore = "<reason>"]` so the harness prints the reason rather than a bare `ignored`.

`check-legacy-webkit.mjs` scans `src-tauri/src/streamer/static/` under a stricter contract than the
desktop UI and currently reports six client-page hazards as known debt while exiting 0. If your
change adds a seventh, that is a regression — the script tells you.

**Additionally, for any client change: the acorn parse-floor check.** There is no packaged script
for it; run it inline and add one if you find yourself doing it twice:

```sh
npx --yes acorn@8 --ecma2017 --silent src-tauri/src/streamer/static/input.js
# and for index.html, extract the two inline <script> bodies first
```

**Final production build gate**, immediately before the report:

```sh
pnpm tauri build                                             # Windows host
pnpm tauri build --config src-tauri/tauri.macos.conf.json    # macOS host
```

**The one failure you may accept is the Tauri updater signing-key error** — the private key is not
on your machine and that is expected. Zero cargo errors, zero TypeScript errors, zero linker
errors, zero bundling errors otherwise. Do not write the report over a broken build.

---

## 5. Current state of this area

### 5.1 The host half is measured; the client half is a blank column

`docs/BASELINE.md` holds the host path in detail. The numbers you are extending, from the reference
Windows 11 26200 / RTX 3080 Ti box, `probe --whep-selftest --duration 30`, real capture of a
1280×720@60 virtual display, WHEP over loopback, **capture → socket in ms, p50 / p90 / p99 / max**:

| Encoder / capture | Before the host work | After, quietest pass (load 2–4 builds) |
|---|---|---|
| NVENC zero-copy ring + WGC | 1.57 / 8.39 / 16.78 / 18.00, 67 real frames / 30 s | **1.84 / 3.93 / 6.82 / 7.08**, **206** real frames / 30 s |
| x264 software + WGC | 2.36 / 5.24 / 23.07 / 24.82, 81 real frames | 4.19–8.39 / 7.34–12.58 / 9.44–27.26 / 11.05–109.67, 159–450 real frames |
| Quick Sync + WGC | 11.53 / 16.78 / 25.17 / 26.05 (`intel-cpu-bridge`) | 8.39–11.53 / 13.63–15.73 / 16.78–20.97 (`intel-same-adapter`) |

On the macOS side, the 10.15.8 dev box measured capture → encoded at **13.61 / 15.06 / 20.64 ms**
(p50/p99/max) and is explicitly *the slowest configuration the code supports*. A 14.2+ Mac should
beat every one of those numbers, and if it does not, that is a finding.

**The client column is empty.** `docs/BASELINE.md` "What could not be measured" says so plainly:
*"Client arrival → paint, and glass-to-glass — needs a real browser, and browser automation was out
of scope for this run. The apparatus is in place and tested; the numbers arrive the moment a device
joins."* That sentence is your brief in one line.

### 5.2 What has landed on the client, and what it is worth
**A note on the `W…` labels you will meet in the ledgers, in the risk register and in this file.**
The engagement that produced this repository's current state ran as numbered parallel workstreams:
`W1` transport · `W3a`/`W3b` Windows capture and encode · `W4`/`W4b` AMD · `W5` macOS capture and
encode · `W6` macOS permissions · `W7` macOS audio · `W8` Windows audio · `W9` Windows platform ·
`W10` input · `W11` the client page · `W12` the desktop UI · `W13a`/`W13b` build, CI and hygiene ·
`W14` AirPlay · `W15` tests. They are **provenance labels only** — they tell you which change series
a row came from so a ledger note reads in context. They are not people, none of them is still
running, nothing is waiting on one, and **every one of them has merged into `latency-overhaul`.**


Ledger rows 28–37 and 171–177 are the client page's own work; rows 48–57, 179–187 and 204–213 are
the host half of the same contract. In summary, all currently `UNVERIFIED` or `COMPILE-CHECKED`
except where noted:

- **Row 28** — `RTCRtpScriptTransform` preferred over `createEncodedStreams` (Chrome 141+ has both,
  and the old condition `hasScriptTransform && !hasLegacyEncodedStreams` sent every Chrome and Edge
  down the legacy branch); a 2.5 s watchdog steps *down one rung* rather than to `<video>`; the new
  `KEYFRAME_REQUEST` (`0x64`) on the reliable input channel when neither transform can ask.
- **Row 29** — decoder backpressure moved from `decodeQueueSize` (which counts acceptance, not
  completion) to outstanding decoder **outputs**, target 1, with `ondequeue` clearing a stale
  backlog; `isConfigSupported` given the exact configuration `configure()` receives plus the real
  coded size; `hardwareAcceleration` probed `no-preference` then separately `prefer-hardware` for
  telemetry only; bounded decoder resets (3).
- **Row 30** — three-argument `drawImage`; canvas re-sized on every frame-size change;
  `desynchronized` removed (it is inert on an OffscreenCanvas in Chromium); the opt-in
  `VideoTrackGenerator` tier behind `?present=track`.
- **Row 31** — the microphone prompt removed from the join path; ICE gathering ends a grace period
  after the last *useful* candidate (200 ms after a host/srflx candidate on a LAN, 400 ms after a
  relay candidate on a relayed session) rather than at the deadline; the measured refresh rate
  folded into the join instead of applied by a post-live renegotiation.
- **Row 32** — the audio fast path chosen from an awaited `AudioDecoder.isConfigSupported` **plus a
  real decode-and-`copyTo` probe** rather than constructor existence.
- **Row 33** — underrun no longer re-primes on the first starved block (four consecutive); both
  worker and worklet conceal instead of outputting silence; sequence gaps detected in the worker.
- **Row 34** — `outputLatency` estimated from `getOutputTimestamp()` where the browser does not
  report it, low-passed; an adaptive safety margin that grows on a real underrun (4 ms steps to
  60 ms) and decays on a clean link (1 ms per ~2 s).
- **Row 35** — coordinates normalised against the `object-fit: contain` picture rect;
  `getCoalescedEvents()` extended from pen to touch and mouse, capped at 8 per flush; pointer lock
  asks for `unadjustedMovement`.
- **Row 36** — Screen Wake Lock as a tier above `nosleep.js`; `nosleep.js` constructed lazily and
  muted; `enterFullscreen()` always returns a Promise.
- **Row 37** — stats frame v2 (96 bytes, first 72 byte-identical to v1): four tier bytes, a
  degradation bitfield, the swallowed-error count, three audio numbers. Now v3 (104 bytes) with the
  two redundancy counters.
- **Rows 171–177** — the optional input tails gated on host capability bits; `wantsFallbackTrack`
  and `verified` in the join body; the `MessagePort` audio ring; the `/audio-outputs` poll at 30 s
  where `devicechange` exists; dropped-file screening against `pc.sctp.maxMessageSize`; the second
  60-sample refresh-rate measurement; `detectHasEarpiece` by screen short edge.
- **Row 179** — H.264 profile negotiation, High > Main > Baseline from the client's own
  `profile-level-id`, with the answer's level derived from the negotiated geometry.
- **Row 180** — `streamer/sockets.rs` and the per-session muxed ICE socket. **This one shipped
  switched off** and §8.4 item C-20 is the measurement that decides whether it ships on.
- **Rows 204/285** — trickle ICE. **Read §5.3 before you touch this.**
- **Row 287** — repair from a long-term reference instead of an IDR. One repair measures
  **23,698 B against a 28,423 B IDR** on a synthetic worst case where every pixel changes every
  frame. The *rate* — how many IDRs a lossy minute stops producing — is unmeasured.

### 5.3 The two halves that were on a branch have merged

The client half of trickle ICE and of the fast-channel reorder sequence — the work the ledgers,
`docs/TELEMETRY.md`, `docs/CAPABILITY-TIERS.md` and `docs/manual-tests/client-page.md` describe — is
**in `latency-overhaul`**. Items C-06 and C-14 are therefore *verification* items, not
implement-from-scratch ones. Confirm it in thirty seconds, and paste the output into the report:

```sh
grep -c trickleIce            src-tauri/src/streamer/static/index.html   # expect >= 1
grep -c 'trickle-ice-sdpfrag' src-tauri/src/streamer/static/index.html   # expect >= 1
grep -c INPUT_SEQUENCE        src-tauri/src/streamer/static/input.js     # expect >= 1
grep -n 'CLIENT_DEGRADED_NAMES: \[&str;' src-tauri/src/streamer/telemetry.rs
#   expect [&str; 23], ending in "no_trickle_ice"
```

If any of the first three returns **0**, you are on a tree older than this guide: implement the
client half to the description in `docs/CAPABILITY-TIERS.md` rows *Client ICE signalling* and
*Client fast-channel sequence* and in `docs/TELEMETRY.md`. The host half has been in
`latency-overhaul` throughout: `server.rs` serves `POST /whep` → `Location: /whep/{id}`,
`PATCH /whep/{id}` taking `application/trickle-ice-sdpfrag`, and `DELETE /whep/{id}`; `/net-config`
answers `{"httpsPort":…,"trickleIce":true}`; `input/protocol.rs` defines `INPUT_SEQUENCE = 1 << 5`
and the parser reads the sequence past the stamp.

**And the host now has a name for bit 22.** `telemetry.rs` declares
`CLIENT_DEGRADED_NAMES: [&str; 23]`, ending in `"no_trickle_ice"`, so
`TRICKLE_ICE_UNAVAILABLE` reaches the `degraded=` suffix on the session summary line — the host's
only view of client degradation — instead of being silently dropped by `client_degraded_list`'s
`enumerate()` filter. `docs/TELEMETRY.md` and `docs/manual-tests/client-page.md` both tell the
tester to read bit 22, and it is now readable. **Confirm that on a real join** (C-06): the bit must
be clear on a direct trickle join and set when trickle was unavailable or the host advertised it and
returned no `Location`. If your `serve -v` line stops one name short, the array is not the one this
guide describes.

### 5.4 The client findings are closed — here is what they became

Nine client-scoped defects were open when this guide's inventory was taken, found by seven
independent cold reviews of the client diff. **All nine were fixed before the guide shipped**, so
`findings/` is essentially empty when you start. They are listed here for two reasons: an older
document may point at a filename you cannot find, and **each one is a regression you can now test
for** — several of them are the reason a C-item below says what it says.

| Was | What it was, and where it went |
|---|---|
| `major-client-trickle-patch-failure-has-no-fallback.md` | With trickle on the offer carries **no ICE candidates at all**, so a systematically failing `PATCH` meant zero remote candidates and ICE never connecting, with no `trickleOff` latch on that path (unlike its sibling, where the host advertises trickle and returns no `Location`). Fixed: the latch now fires and rejoins once with a gathered offer. **C-06 reproduces it** by blocking `PATCH /whep/*`. |
| `major-client-web-page-has-no-test-infrastructure-at-all.md` | Fixed: `pnpm test:client` (`scripts/test-client.mjs`) runs **seven** Node harnesses over the shipped files with `vm` and minimal browser stubs, leaving `static/` byte-identical. See §4.6. |
| `minor-client-degrade-bit-22-has-no-host-name.md` | Fixed: `CLIENT_DEGRADED_NAMES` is 23 entries (§5.3). |
| `minor-client-net-config-failure-is-cached-forever.md` | `getNetConfig()` memoised `{}` on a failed fetch primed at module scope — exactly when it is most likely to race the host's HTTP server coming up. Fixed; **C-05 should confirm a first join on a cold host still gets `trickleIce`.** |
| `minor-client-trickle-rejoin-discards-device-token.md` | The rejoin `return`ed above the `X-Device-Token` store, so it re-presented the previous token and the same OTP, and sent no `/leave` — leaving a fully materialised host session to expire on `disconnect_grace`, paying the display-create cost twice for one join. Fixed; **C-06 verifies both halves.** |
| `minor-client-whep-delete-ignores-its-resource-id.md` | Fixed. |
| `minor-client-whep-patch-has-no-peer-binding.md` | `PATCH /whep/{id}` was the only route with no peer binding and no rate limit. Fixed: bound to the peer address that created the resource, and capped. **C-06 confirms a foreign peer cannot PATCH someone else's resource.** |
| `minor-client-mouse-delta-reorder-gate-drops-additive-motion.md` | `MOUSE_DELTA` is `is_supersedable`, so a reordered frame was dropped — but its position term is a **relative increment**, and increments commute, so dropping one lost that motion permanently. **C-14 still owns the measurement** (cursor-travel error under 3 % reorder), because nobody has ever induced a reorder on this project's hardware. |
| `minor-client-fragment-test-is-a-reimplementation.md` | The Rust "byte for byte" trickle-fragment test reimplemented the client body instead of using it. Fixed with golden vectors shared by both sides. |

The full reviews are in `docs/reviews/client-review-1-correctness.md` through
`client-review-7-followup.md` — seven independent cold reads, about 200 KB in total. Review 7's
"Findings" section is the list above; reviews 1–6 also carry "Verified correct" sections telling you
what has already been reasoned through and does not need re-deriving.

### 5.5 What was already tried and rejected here, with the numbers

Do not re-try any of these blindly. Each may be re-opened, but only against the number that stopped
it.

**Transport:**

- **FEC is not built, because webrtc-rs ships none of it.** `interceptor-0.17.1/src/` contains
  `chain, error, lib, mock, nack, noop, registry, report, stats, stream_info, stream_reader, twcc`
  and nothing else. `rtp-0.17.1/src/codecs/` contains `av1, g7xx, h264, h265, opus, vp8, vp9` and
  nothing else. The one occurrence of `ulpfec` in the whole of `webrtc-0.17.1` is a bare **codec
  registration** at `api/media_engine/mod.rs:367` with no packetizer behind it. FEC here means
  writing RFC 5109 or FlexFEC-03 from scratch with its own repair SSRC and sequence space. The six
  cheaper alternatives all landed first (rolling intra refresh, the keyframe gate, the send pacer,
  TWCC + trendline BWE, the relayed MTU, audio `FLAG_REDUNDANT`). **The bar for revisiting is
  exactly what you are about to produce: an instrumented relayed or lossy-AP session that still
  shows PLI-driven keyframe storms after all six.** If your item C-24/C-25 runs show that, FEC
  becomes in scope and you build it.
- **HEVC is not negotiated, because no host backend can encode it.** The RTP payloader exists
  (`rtp-0.17.1/src/codecs/h265/mod.rs`, a complete RFC 7798 `HevcPayloader`) and Chrome 136+ and
  Safari offer an H.265 payload type. What landed instead is the **detection**:
  `negotiate::read_offer` records whether the offer carried an H.265 payload type and the join logs
  it, so the share of clients that could take it becomes a measured number. **Collecting that
  number across your whole device matrix is an explicit deliverable** (item C-19).
- **WebTransport is not built.** Chrome has not shipped `sendOrder`/`createSendGroup()`, there is no
  session `getStats()`, RFC 9221 puts a second congestion controller under ours, there is no
  transport-level fragmentation (so a 200 KB keyframe means reimplementing RTP without NACK/RTX),
  and there is no WHEP-over-WebTransport spec. Revisit only when Chrome ships stream prioritisation
  **and** a LAN glass-to-glass comparison beats the WebRTC path *after* the pacer and the BWE work.
- **Sender RTX was rejected twice** (ledger rows 62 and 269): webrtc-rs binds the NACK responder by
  `info.ssrc` for streams whose `rtcp_feedback` declares nack, which the RTX registration does not.
- **Sub-frame / slice-mode output rejected** on NVENC, AMF and x264 against a measured **~2 ms**
  ceiling for a whole frame's encode. Slice-level delivery cannot save more than that here.

**The client page:**

- **`getPredictedEvents()` on the pointer batch.** The byte is in the wire format and is
  deliberately written as 0. Prediction trades correctness for latency and the size of both halves
  is a per-engine, per-device number **you can now obtain**. Enabling it is a client-only change.
  Re-open it (§9 item 4).
- **Chunking a dropped file across SCTP messages** — needs a continuation opcode, a transfer id and
  reassembly on the host. What landed is the client-side half: refuse before reading, and say so.
- **Moving the `/reconfig` poll onto the reliable DataChannel** — the client cannot do it alone;
  `server.rs` would have to push the epoch, and the client would still need the HTTP path as the
  fallback for a session with no data channel. In your scope fence `server.rs` is tunable, so this
  is now *reachable* — but weigh it against the liveness note below.
- **Consolidating the three liveness signals** (the reconfig poll, the `PONG`, and the render/bytes
  advance, all feeding one `markAlive()`) — rejected because they fail independently and collapsing
  them makes liveness only as good as the weakest transport, for a saving of two comparisons.
- **A/B-ing `jitterBufferTarget`.** All three jitter-buffer knobs are **inert on the WebCodecs
  path**: an encoded transform takes the frame from `RtpVideoStreamReceiver2::OnAssembledFrame`
  *instead of* it entering the reference finder and the frame buffer, and the frame buffer is what
  they configure. The measurement is only executable on the `<video>` fallback — and that same
  branch is why **every receive-side timing field in `getStats()` reads zero on the fast path**.
  Any measurement plan that says "compare `jitterBufferDelay` before and after" is wrong on the fast
  path. This is item C-04's and C-22's central complication; do not lose an afternoon to it.

**macOS host, if that is your host:** one shared VideoToolbox session between clients at the same
resolution was rejected as a session-lifecycle change; mid-session SCK → CGDisplayStream fallback
was rejected (the tier is committed only after a delivered frame, with a 750 ms first-frame
deadline that synthesizes a black IDR and counts `capture_no_first_frame`); a Metal convert/scale
stage was rejected because WindowServer delivers the frame already scaled and already in `420f`;
`kVTCompressionPropertyKey_ConstantBitRate` was rejected because CBR spends bits on a still screen.

### 5.6 The numbers to beat

From `docs/BASELINE.md` and `docs/research/competitors-closed-source.md`. **None of these was run
on your hardware.** They say what "good" looks like; producing the side-by-side is item C-34.

| Product | Glass-to-glass (ms) | Basis |
|---|---|---|
| Parsec | 4–8 (LAN, 240 fps) | *vendor*, not independently reproduced |
| Steam Link | 13–16 display latency at < 1 ms ping | user reports |
| **Luna Display (LIQUID 3.0)** | **11.3 mean USB, 22.4 mean Wi-Fi** (50 samples) | *vendor* bench — the same job as ScreenExtend, and the target |
| Moonlight / GameStream | 20–35 on a good LAN | community measurement |
| Apple Sidecar | "consistently below 30" wired USB-C | reported |
| Meta Air Link | 25–50 claimed; 59.1 measured | review |
| Air Display | 52.9 USB / 131.4 Wi-Fi | *adversarial* bench (Astropad) |
| Duet Display | 95 USB / 108 Wi-Fi at 60 fps | *adversarial* bench (Astropad) |
| **Hopp (browser WebRTC remote control)** | **98** (good net, 1080p) after tuning libwebrtc, from 159 | published measurement — the same *transport*, and therefore the honest ceiling |
| Miracast / Wi-Fi Display | 80–100 optimised; **spec ceiling 250** | certification spec |
| Stadia | RTT 10–15 typical, **jitter buffer 45.35 ms** at 1080p | academic traffic analysis |

The two that matter for positioning are **Luna Display** (the same job — a tablet as a second
display over Wi-Fi, 22.4 ms) and **Hopp** (the same transport — browser WebRTC — at 98 ms). The
Hopp teardown is the most useful data point in the table because it shows where the milliseconds
live in *our* architecture: **encode/decode processing and the receiver jitter buffer, not the
network** (processing delay 232 → 51 ms, jitter buffer 189 → 46 ms).

ScreenExtend's host half is **3.93 ms at p90** on the fast path, which leaves most of a Luna-class
budget for network and client. Whether the client actually spends it inside that budget is the
question this entire guide exists to answer.

---

## 6. Phase 1 — read first, then discover what the hardware exposes

### 6.1 Read these in full before changing anything

Partition across Opus 5 agents; every file below must be read in full by at least one agent, and
you must hold the synthesis.

**The client page (read line by line — this is the territory):**
`static/index.html`, `static/transform-worker.js`, `static/audio.js`, `static/audio-worker.js`,
`static/audio-worklet.js`, `static/input.js`, `static/styles.css`, `static/at-capacity.html`.

**The host↔client contract:**
`streamer/server.rs` (routes, the COOP/COEP headers, ETag-per-asset, `locality_for`, `ice_json_for`,
the clamp constants, the join path), `streamer/webrtc_session.rs` (WHEP negotiation, ICE, the MTU
constants, playout delay, the audio DataChannel, `redundancy_now`, the BWE poll loop, trickle
registry), `streamer/input/protocol.rs` (all 24 opcodes, the optional tails, `HOST_CAPABILITIES`),
`streamer/telemetry.rs` (`parse_client_stats`, the histograms, `CLIENT_DEGRADED_NAMES`),
`streamer/negotiate.rs`, `streamer/sockets.rs`, `streamer/pacer.rs`, `streamer/congestion.rs`,
`streamer/bitrate.rs`, `streamer/keyframe.rs`, `streamer/repair.rs`, `streamer/cloud.rs`,
`streamer/session.rs`, `streamer/audio/protocol.rs`, `streamer/synthetic.rs`.

**The docs:**
`CLAUDE.md` (all of it), `docs/dev-workflow.md`, `docs/TELEMETRY.md` (all of it — this is the
authority on every field you will read), `docs/INPUT-PROTOCOL.md`, `docs/BASELINE.md`,
`docs/manual-tests/client-page.md`, `docs/manual-tests/transport.md`,
`docs/manual-tests/core-closeout.md` §1/§4/§7, the "Client …" rows of `docs/CAPABILITY-TIERS.md`,
ledger rows 28–37, 44, 46–48, 52–61, 65, 74–75, 171–177, 179–187, 192, 204–215, 271 and 285–288 of
`docs/OPTIMIZATIONS.md` plus its note sections "Considered and rejected on the transport (W1 close-out)",
"W11 decisions made without consultation", "W11 rejections (recorded, not applied)",
"W11 UI-visible recommendations", and "The client does not call `DELETE /whep/{id}`, and the trickle
tier is a bit rather than a byte" — §5.2's glossary says what the `W…` labels in those headings are.

**The research** (do not redo it; mine it):
`docs/research/client-browser.md` (49 KB — the per-engine WebCodecs, decoder-hold, presentation and
audio analysis, with a tier table and a verification plan whose "cannot be verified on the dev
boxes" list is essentially your job list), `docs/research/competitors-closed-source.md`,
`docs/research/competitors-open-source.md`, `docs/research/transport-codecs-network.md`.

**The seven cold reviews:** `docs/reviews/client-review-1-correctness.md` through `-7-followup.md`.
Read at least reviews 2 (fallbacks), 3 (old browsers — its §2 table is the parse floor you must
preserve) and 7 (the open defects).

**`docs/RISK-REGISTER.md`** entries R-244 (every client-browser matrix claim), R-245, R-247
(cross-network / TURN / cloud relay).

### 6.2 Hardware and browser discovery — write it down

Create `docs/discovery/G5-machine.md` and fill it before you measure anything. It is what makes
every later number reproducible.

1. **Host facts.** OS build, CPU, GPU and driver version, monitor refresh rate(s), whether the IDD
   virtual-display driver is installed, `ScreenExtend doctor --json` in full.
2. **The device inventory.** For every client you have: model, OS version, browser and its exact
   version string, screen CSS size and `devicePixelRatio`, `screen.refreshRate` if the browser
   reports it, whether it has a pen, whether it can pair a Bluetooth sink, and whether you can
   attach a console to it.
3. **The network inventory.** Both APs: model, firmware, band, channel, channel width, whether WMM
   is on, whether the admin UI exposes a DSCP/WMM mapping table. Wired link speed. The TURN server's
   host, region, and measured RTT from both networks. Whether the second network is genuinely
   unroutable to the first (prove it with `ping`, not with an assumption).
4. **The camera.** Model, the exact slow-motion frame rate it records (many "240 fps" phones drop to
   120 in low light — check the file's metadata, not the marketing), and the light source you will
   use. A 240 fps camera quantises to 4.17 ms; state that with every result.
5. **Baseline link quality.** `iperf3 -c <host> -u -b 200M -t 30` and `-R` on each AP band and on
   the wire, plus a 300-packet ping histogram. You need to know the link is healthy before you
   attribute anything to the app.

---

## 7. Phase 2 — instrumentation and the baseline matrix on *this* hardware

Do not change a line of client code until this matrix has numbers in it. Every claim in Phase 3
references it.

### 7.1 The one-device baseline

Pick the strongest client you have (a current desktop Chrome on the same 5 GHz AP is ideal because
it has a console and is the least likely to be the bottleneck) and take a full reading:

```sh
# host
ScreenExtend serve -v --no-cloud
# in another shell, once the device has joined and settled for 60 s of moving content
ScreenExtend status --json > baseline-chrome-desktop.json
```

```js
// client console, after 60 s of moving content on the host
JSON.stringify(SEVideo.getStats(), null, 1)
JSON.stringify(SEAudio.getSyncInfo(), null, 1)
```

Record `hostToPaint` p50/p90/p99/max — **this is the client half of glass-to-glass and it is the
number nobody in this project has ever seen.** It is only populated once the clock offset has
arrived (about a second into the session); before that the client reports 0 and clears
`CLOCK_VALID`, so a `hostToPaint.p50` of 0 **with** `clockValid: true` is a bug and worth its own
finding.

### 7.2 The matrix to fill

Rows are devices, columns are conditions. Fill **every** cell or say why it is empty. Each cell is
`videoPath / transportTier / renderTier / keyTier / audioPath / sinkTier`, plus `degraded` in
binary, plus `hostToPaint` p50/p90/p99.

| Device | LAN Wi-Fi 5 | LAN Wi-Fi 6 | Wired host / Wi-Fi client | Hosted network | Cross-network via TURN | 1 % loss | 3 % loss | 5 % loss | +40 ms RTT |
|---|---|---|---|---|---|---|---|---|---|
| Chrome desktop current | | | | | | | | | |
| Edge desktop current | | | | | | | | | |
| Chrome desktop ≤ 140 | | | | | | | | | |
| Chrome Android ≥ 141 | | | | | | | | | |
| Chrome Android 86–140 | | | | | | | | | |
| Samsung Internet | | | | | | | | | |
| Firefox desktop ≥ 130 | | | | | | | | | |
| Firefox desktop ≤ 129 | | | | | | | | | |
| Firefox Android | | | | | | | | | |
| Safari macOS 26 | | | | | | | | | |
| Safari macOS ≤ 25 | | | | | | | | | |
| Safari iPadOS ≥ 18.4 | | | | | | | | | |
| Safari iOS ≥ 18.4 | | | | | | | | | |
| Safari iOS 17 | | | | | | | | | |
| Safari iOS 16 | | | | | | | | | |
| Safari iOS 15 | | | | | | | | | |
| iOS 12/13 device | | | | | | | | | |
| constrained-baseline decoder | | | | | | | | | |

Not every cell needs every condition — a sensible reduction is: all devices on LAN Wi-Fi 5, the
three or four most representative on every other column. **Say which reduction you took and why.**

The expected values per device are already written out in `docs/manual-tests/client-page.md` §1,
row by row, including the ones that are correct-but-surprising (iPhone `fullscreen: false` with
degradation bit 0 is designed behaviour, not a failure; `audioPath` 3 on Safari 25 and below is
correct because `AudioDecoder` shipped in Safari 26). **Compare against that table and treat every
disagreement as a defect until proven otherwise.**

### 7.3 Inducing the network conditions

- **Windows host — `clumsy`.** Filter on the client's IP, then use Lag (40 ms), Drop (1/3/5 %),
  Out of order, and Throttle in turn. Run one impairment at a time; a combined impairment produces
  a number you cannot attribute.
- **macOS host — `dnctl` + `pfctl`.** The shape:
  ```sh
  sudo dnctl pipe 1 config bw 0 delay 40 plr 0.03
  echo 'dummynet out proto udp from any to <client-ip> pipe 1' | sudo pfctl -f - -e
  # undo
  sudo pfctl -d; sudo dnctl -q flush
  ```
- **Reorder** specifically: `clumsy`'s Out-of-order, or the TURN relay path, which reorders
  naturally. This is the only way `input_reorder_drops` can move off zero.
- **Contention**: an `iperf3 -u -b 300M` from a second station on the same AP, and separately a
  4K YouTube stream, which is a more realistic traffic shape.

### 7.4 Instrumentation you may need to add

The client already measures three stages (`arrivalToPaint`, `decode`, `hostToPaint`) as 512-sample
sliding windows in `transform-worker.js`. If a Phase 3 item needs a stage that is not there, add it
to `StatWindow`, expose it in `collectStats()`, and — only if the host needs it — append a field to
the stats frame. **Appending is safe and the protocol is designed for it**: the length is a floor,
never an equality; bytes 0..72 are frozen as v1 and 0..96 as v2, and each tail is read whole or not
at all. A v4 tail means bumping `STATS_VERSION`, widening `STATS_LEN`, writing the field in
`sendStats()`, reading it in `parse_client_stats`, and documenting it in `docs/TELEMETRY.md` — all
five in one commit, plus an assertion in `scripts/test-client-stats.mjs`.

Note the standing decision this replaces: the trickle tier took **degradation bit 22 rather than a
new byte**, because the degradation field at offset 80 is inside the v2 tail the shipped host
already parses, while a new byte means a v4 frame — and v3's own two `u32`s at 96 and 100 are
already written and dropped by hosts built against v2. If you do extend to v4, promote the trickle
tier to a byte at the same time.

---

## 8. Phase 3 — the work

Thirty-four numbered items. For each: **what to verify, what to fix, how to measure, what "done"
looks like.** Done is always one of:

- the ledger row flipped to **`VERIFIED`** with numbers and the device/browser that produced them,
  and the `docs/CAPABILITY-TIERS.md` Status filled as `VERIFIED (<device>, <version>)`; **or**
- the change **reverted** with the reason and the number that killed it recorded in
  `docs/OPTIMIZATIONS.md`; **or**
- the item **still `UNVERIFIED`** with an explicit statement of what hardware or condition was
  missing. No item may be silently absent from the report.

**The standing rule for every item in this phase: a fallback that silently activates on a real
device is a defect.** If a current Chrome reads `transportTier: 2`, if an Android reads `audioPath: 2`
on an HTTPS page, if a desktop reads `renderTier: 3` — do not record it as an observation and move
on. Find out why, fix it, and re-measure.

### 8.1 Workstream A — the client page's tiers and fallbacks

#### C-01 — Client video transport tier
`static/index.html`, `static/transform-worker.js`. Tiers: `RTCRtpScriptTransform` → `createEncodedStreams` → `<video>`.

**Verify:** on every device in the matrix, `SEVideo.getStats().transportTier` matches the expected
value in `docs/manual-tests/client-page.md` §1. The single most important row is Chrome desktop
current, which must read **1**: before ledger row 28 every Chrome and Edge client took tier 2,
because `useScriptTransform` was `hasScriptTransform && !hasLegacyEncodedStreams` and Chrome 141+
ships both. **A current Chrome reading 2 means the 2.5 s watchdog fired — check `degraded` bit 19.**

**Verify the ladder steps rather than jumps:** the watchdog is armed from ICE-connected (not from
page load, and not from `ontrack`, which is *earlier* because it fires during
`setRemoteDescription`). Both watchdogs share that origin deliberately: when they did not, a join
whose ICE took more than ~2.5 s after the answer had the 5 s demotion fire first and skip the
ladder entirely, landing on `<video>` for the whole session instead of stepping one rung. Force it:
Chrome 141+ over the cloud relay with `receiver.transform` stubbed to never fire → expect
`transportTier` **2**, not 0.

**Fix targets:** any device where tier 1 exists and is not taken. Any device where the step-down
takes more than one rung.

**Measure:** `hostToPaint` p50/p90/p99 at tier 1 vs tier 2 vs tier 3 on the same device and link.
Tier 3 reinstates libwebrtc's whole receive path — estimated at ~90 ms and **never measured**.
Measuring that delta on real hardware is one of the highest-value single numbers in this guide,
because it is what the entire WebCodecs path is worth.

#### C-02 — Client keyframe request
`static/index.html`, `static/transform-worker.js`, `static/input.js`, `streamer/input/protocol.rs`, `streamer/keyframe.rs`.
Tiers: `sendKeyFrameRequest()` → `KEYFRAME_REQUEST` (`0x64`) on the reliable channel → none.

**Highest-risk device: Chrome/Edge/Samsung Internet below 141**, which has no `sendKeyFrameRequest`
at all.

**Verify, on Chrome desktop current (`keyTier` must be 1):**
1. Watch `serve -v` for `RTCP PLI received, requesting IDR`.
2. Provoke loss — saturate the link, or disable and re-enable the client's Wi-Fi for a second.
3. **Expect** recovery within roughly one round trip, `keyRequests` climbing, `videoPath` staying 1.
4. **The old behaviour that must not reappear:** a ~4 s freeze followed by `videoPath` 3 for the
   rest of the session.

**Verify, on Chrome ≤ 140 (`keyTier` must be 2 against this host):** `client_keyframe_requests` in
`status --json` counts them separately from RTCP PLIs, so the two sources stay distinguishable.
Then verify the *gate*: against a host that sends a 17-byte `PONG` (no capability bits), `keyTier`
must be **0**, `keyRequests` must stay flat, and degradation bit 11 must be set — the client sends
nothing it knows will be dropped. A `keyTier` of 2 against a host that does not parse `0x64` is the
exact bug the gate exists to prevent. `node scripts/test-client-stats.mjs` already asserts the gate
itself; the manual run confirms the two end-to-end behaviours.

**Measure:** time from injected loss burst to recovered picture, and `<video>` demotions per hour,
both with and without the tier available. Also: `keyframe_requests_suppressed` against
`keyframe_requests` — a large suppressed ratio is **healthy**, because a decoder that has lost its
reference chain emits a PLI for every frame in its backlog and answering each one on an infinite-GOP
encoder produces the largest frame the session can send.

#### C-03 — Client decode
`static/transform-worker.js`.

**Verify:** the backpressure counts outstanding decoder **outputs** (`MAX_IN_FLIGHT = 2`, target 1),
not `decodeQueueSize`, which drops as soon as the control message is handed toward the codec and can
read zero with frames in flight. Confirm the two resynchronisation paths both work: `ondequeue` with
an empty accept queue (Chrome 106+, Firefox 130+, Safari 16.4+ — check `hasDequeueEvent` in the
worker stats), and the `IN_FLIGHT_STALE_MS = 500` timeout for engines without it or that stop
firing it. Without both, three silently-discarded decodes put the counter permanently over the cap,
every subsequent delta is dropped, and the stream collapses into a keyframe-only slideshow that
nothing demotes because keyframes still render.

**Verify:** `isConfigSupported` is given the same configuration `configure()` receives, including
`optimizeForLatency: true`, and the coded-size hint. Force the hint's floor: set a per-device video
scale of 50 % on the host so the hinted size is twice the stream. `videoPath` must stay **1** —
a demotion here is a bug, because the hint is the client's *request* and the host snaps, evens,
clamps and scales it. `sizeHintRejected` in the worker stats says the hint was the thing refused.

**Measure:** decoder hold — the median difference between `decoder.decode()` and the output
callback, keyed by timestamp — per device. `SEVideo.getStats().decode` already reports it as
p50/p90/p99/max. `docs/research/client-browser.md` §2.1 predicts ≈ one frame interval given that
libwebrtc rewrites the SPS's `max_num_reorder_frames` to 0; **verify that prediction on at least
three engines**, because if it is wrong the whole reorder-delay analysis inverts.

#### C-04 — Client render, and the presentation A/B
`static/transform-worker.js`. Tiers: OffscreenCanvas 2D in the worker (default) → `VideoTrackGenerator`/`MediaStreamTrackGenerator` → `<video>`.

**Verify:** `renderTier` 1 everywhere with `transferControlToOffscreen`. Force the floor by
stubbing it (`Object.defineProperty(HTMLCanvasElement.prototype,'transferControlToOffscreen',{value:undefined})`
before joining) and confirm the page still paints, with degradation bit 12 set.

**Verify:** frames count as rendered only when something painted them. `setupCanvas` refuses the
tier when `getContext('2d')` returns nothing, and `presentFrame` returns `PRESENT.noSink` rather
than counting a frame that reached no sink. A black canvas that used to satisfy every watchdog now
demotes; confirm it does.

**The A/B — this is the only genuine experiment in the guide, and no public measurement settles
it.** `?present=track` routes decoded frames through a track generator into the `<video>` element,
which *may* win a hardware overlay on some platforms. Chrome has the legacy name, Safari 18 the new
one, nobody has both.

1. Join normally; record `hostToPaint.p50` and `.p99` after 60 s of moving content.
2. Rejoin with `?present=track`; confirm `renderTier` is **2** (a 1 means the generator was
   unavailable and the page fell back, which is the expected result nearly everywhere).
3. Record the same two numbers.
4. **Run it on Chrome desktop, Safari 18+, and in fullscreen as well as windowed** — the overlay,
   if it exists, is a fullscreen-compositor behaviour.
5. If p99 improves by more than a few milliseconds on both engines, say so in `docs/OPTIMIZATIONS.md`
   row 30 and **change the default**. If it does not, record that — a negative result retires the
   idea permanently and that is worth as much.
6. Do the photographic version too: the camera method of §8.6 on both tiers.

#### C-05 — Client join chain
`static/index.html`.

**Verify, three stopwatch runs each, median:**

| Case | What must be true |
|---|---|
| First join on a device that has never granted the microphone | **No microphone prompt before the picture.** If one appears at all it is after the stream is live. This used to be a 12 s cap in front of ICE, which gated WHEP, which gated the host creating the display. |
| Rejoin on a LAN with no TURN configured | ICE must not cost seconds. `degraded` bit 15 means gathering ran to its deadline rather than settling — on a LAN that points at the network, not at this code. |
| Join from a 90 / 120 / 144 Hz device | **The session must not renegotiate about a second after it goes live.** Watch for a second `creating virtual display` for the same client. The measured rate must already be in the first join; check the host's device panel shows the right refresh rate immediately. |
| Join with TURN configured, from another network | Gathering waits for a relay candidate plus a 400 ms grace. Expect the slowest case; **it must connect at all.** Before this pass the relay flag was never passed to the wait, so a cross-network join shipped an offer with no relay candidate and could only end in "Connection failed". If it fails, look for `a=candidate:` with `typ relay` in the offer. |
| Host accepts the TCP connection and never answers | Suspend the host process between Connect and the answer. The page must reach the normal **"Disconnected"** state within ~10 s (`WHEP_TIMEOUT_MS = 10000`), not spin forever. On the *reconfigure* path the same within 15 s (`RECONFIG_DEADLINE_MS`). |
| Join twice quickly, or force a watchdog mid-join | **One** `POST /whep` per surviving join attempt. Two peer connections for one device — a second `creating virtual display` with no intervening leave — means the `startGen` generation guard is not holding. |

**Measure and record:** time-to-first-frame per device per link, p50 of three runs. Break it down
using the host's `join stages (<locality>): display=…ms pipeline=…ms handshake=…ms (overlapped)`
line. Time-to-first-frame is part of "feels slow" and nobody has ever measured it on a real device.

`node scripts/test-client-ice.mjs` is the offline half and already asserts the candidate/grace
table, that a relayed wait is not ended by a host or reflexive candidate, the relay grace,
gathering completion, listener cleanup, and the deadline setting bit 15.

#### C-06 — Client trickle ICE, end to end
`static/index.html`, `streamer/server.rs`. **Read §5.3 and §5.4 first** — both halves are in your
tree, and three of the nine closed findings are on exactly this path.

**Verify the whole flow against a real browser:** `POST /whep` → `Location: /whep/{id}` → `PATCH`
with `application/trickle-ice-sdpfrag` → `a=end-of-candidates`. Host log shows
`trickle: added N remote candidate(s)`. Degradation bit 22 clear on a direct join, set when trickle
was unavailable or the host advertised it and returned no `Location` — and it now **has a host-side
name** (`no_trickle_ice`, the 23rd entry in `CLIENT_DEGRADED_NAMES`), so it reaches the `degraded=`
suffix on the session summary line. Confirm it does.

The wait removed was measured offline at **201 ms on a LAN** and **713 ms relayed** (with TURN
answering in 300 ms). **Measure it for real:** time-to-first-frame with trickle on and with
`trickleOff` forced, three runs each, on a LAN and through TURN. Note that the **cloud relay tunnels
`/whep`, `/reconfig` and `/leave` and 404s `/net-config`**, so a relayed client takes the
gather-then-post tier by construction — confirm that is what happens rather than a hang.

**Then break it deliberately — three fixes on this path have never been exercised against a real
browser, only reasoned about** (§5.4):

- **The PATCH-failure latch.** With trickle on, the offer carries **no candidates at all**, so a
  systematically failing `PATCH` means the host has zero remote candidates and ICE never connects.
  There is now a `trickleOff` latch on that path — there was not, while the sibling case (host
  advertises trickle, returns no `Location`) was already latched. Reproduce the failure by blocking
  `PATCH /whep/*` in devtools, or with a proxy that 405s it, and confirm: the latch fires, the page
  degrades, and it rejoins **once** with a gathered offer that connects. A second rejoin, or a spin,
  is a regression.
- **The rejoin's device token.** The rejoin used to `return` two lines above the `X-Device-Token`
  store, so it re-presented the *previous* token and the same OTP — working only because the OTP is
  not single-use — and sent no `/leave`, leaving a fully materialised host session (display created,
  pipeline started) to expire on `disconnect_grace` and paying the create cost twice for one join.
  Confirm the rejoin stores the token the accepted join issued, and that the abandoned session is
  released rather than left to expire: watch for a second `creating virtual display` with no
  intervening leave.
- **`PATCH /whep/{id}`'s peer binding and rate limit.** It was the only route with neither; a
  resource id is not an authenticator. Confirm a PATCH from a different peer address is refused and
  that a flood is capped.

#### C-07 — Client audio decode tier
`static/audio.js`, `static/audio-worker.js`, `static/index.html`. Tiers: WebCodecs `AudioDecoder` + `AudioWorklet` → the browser's own Opus track (NetEQ).

**The probe is the point.** `verifyCapabilities()` awaits `AudioDecoder.isConfigSupported({codec:'opus', sampleRate:48000, numberOfChannels:2})`
**and then decodes a real 3-byte CELT-only Opus frame (`0xf8 0xff 0xfe`) and `copyTo`s the result as
`f32-planar`**, because `isConfigSupported` answers about a configuration, not about the call that
actually moves the samples. The verdict is deliberately asymmetric: only a decoder that *produced*
an `AudioData` whose `copyTo` threw, or one that could not be configured, demotes; a decode error or
no output within 400 ms is `inconclusive`, because the probe packet would then be the suspect and a
wrong verdict disables the fast audio path on every browser at once.

**Highest-risk devices: Safari 26 and Firefox 130** — the two engines that only just shipped
`AudioDecoder` and that the old constructor sniff got wrong. **Verify on both**, and on Firefox 129
and Safari 25 where the answer must be tier 2 with degradation bit 6.

**Verify the mid-session step-down:** `SEVideo.onAudioFastLost()` drops the claim, rebuilds the
session, and the next answer carries a real Opus track — once only, because a second round would be
a loop. `SEAudio.getSyncInfo().decoderFailed` reports it. Force it by stubbing
`window.AudioDecoder = undefined` after the join, or by making the worker throw. **Audio that never
comes back is a real defect.**

**Verify silence is impossible:** the host reads `audioCapabilities` once; if the client claims the
fast path the host sends Opus on a DataChannel and puts **no audio track in the SDP at all**. Every
exit from `attachDataChannel` must take the ladder. Confirm by blocking `/audio-worker.js` in
devtools and joining: `audioPath` must become **3** within about three seconds (a worker that 404s
is constructed successfully and never answers, so the `WORKER_READY_DEADLINE_MS = 3000` deadline is
the only thing that catches it).

#### C-08 — Client audio ring tier
`static/audio.js`, `static/audio-worker.js`, `static/audio-worklet.js`, `streamer/server.rs`.
Tiers: `SharedArrayBuffer` + `Atomics` → a transferred `MessagePort` → the page relaying every packet.

**The question nobody has answered: does a self-signed HTTPS join still yield `crossOriginIsolated`?**
`server.rs` sends COOP `same-origin` and COEP `require-corp` on the index, and the runtime-generated
self-signed certificate is what a real join uses. Verify directly: `SEVideo.getStats().sab` and
`.crossOriginIsolated`, and `SEAudio.getSyncInfo().transport` reading `shared`. **Test this on every
platform**, because the answer may differ — an Android WebView with no `SharedArrayBuffer` is
expected to read `port` or `postMessage`, but a desktop Chrome on HTTPS reading anything but
`shared` is a defect.

**Verify the handshake, because a disagreement used to be permanent silence with both halves
believing they worked:** the worklet acknowledges (`ringport-ok`) before the worker gets the other
end of the `MessagePort`; a worklet that could not map the shared ring posts `mode-mismatch` and
both sides fall back to the postMessage ring.

**Verify the ring ABI holds.** It is a hard contract between three files: control block 16 `Int32`
at bytes 0..64 (`W` = absolute write position in frames, `R` = read position, `OVERRUNS`), then
`cap*4` bytes of left channel and `cap*4` of right, planar. `CAP_FRAMES = 65536` and **must stay a
power of two** because positions are absolute `uint32` frame counters indexed by `pos & (cap-1)`.
If you change one file, change all three, and add an assertion to
`scripts/test-client-audio.mjs`.

**Measure:** underruns and `concealed` per minute on each transport tier, on a loaded phone. Tier 3
crosses the main thread 200 times a second at the host's 5 ms framing, so the expected difference is
a dropout rate under load, not a mean latency.

#### C-09 — Client audio output latency, and the Bluetooth case
`static/audio.js`, `static/audio-worklet.js`. Tiers: `AudioContext.outputLatency` → a low-passed `getOutputTimestamp()` estimate → `baseLatency` → unknown.

**This is the item a Bluetooth sink exists for.** Treating a missing `outputLatency` as zero made
the commanded jitter depth too large by exactly the output latency, so audio played **early** — by
3–10 ms on a wired output and **100–200 ms on Bluetooth**, which is the difference between lip-sync
and obviously wrong.

**Verify on Chrome/Android, Safari/iOS and Chrome/Windows, each with a Bluetooth sink and with a
wired one:**
- `SEAudio.getSyncInfo().outputLatencySource` reads `measured` on Chrome 102+/Firefox 70+/Safari
  18.4+ and `estimated` below that. **`unavailable` on a browser that has `getOutputTimestamp` is a
  bug.**
- `outputLatencyMs` on Bluetooth must be **in the hundreds, not 0**.
- Degradation bit 8 set exactly when the source is not `measured`.
- The A/V offset **by camera**: play a video with visible speech, film the client's screen and
  listen, or use the 240 fps rig with a clap/flash. `residualOffsetMs` near 0 means the sync target
  was achievable; a persistent value above 30 ms means it was clamped and something upstream is
  wrong.

**Verify the re-measurement on sink change:** `state.outputLatencyEmaMs` is cleared and
`maybeUpdateTarget()` re-runs whenever the sink changes, because a different device has a different
buffer depth.

**Optimise:** the adaptive safety margin (`SAFETY_STEP_MS = 4`, `SAFETY_MAX_MS = 60`,
`SAFETY_DECAY_MS = 1` per ~2 s of clean blocks). Measure the converged `safetyMs` on a clean 5 GHz
link — the design intent is that a good network converges to **no margin at all** rather than paying
a fixed worst-case buffer forever. If it does not converge to zero on a quiet link, find out why.
Then measure it on a busy AP and on the relay, and tune the three constants on the numbers.

#### C-10 — Client audio sink selection, concealment and underruns
`static/audio.js`, `static/audio-worker.js`, `static/audio-worklet.js`. Tiers 1–5: `selectAudioOutput()` → `AudioContext.setSinkId` → `HTMLMediaElement.setSinkId` via a MediaStreamDestination bridge → `navigator.audioSession` category (iPhone earpiece) → none.

**Verify:** change the device's audio output from the host's device panel and confirm the sound
moves. `sinkTier` says which API did it; **`sinkTier` 0 after a change the user asked for is a
bug**, while an empty output list on iOS 17 is not. Tier 1 is currently **unreachable** by design
(§2.2) — confirm it stays 0 and do not add a control.

**Verify the earpiece path on an actual iPhone**, including one with "Request Desktop Website" on,
which sends a Macintosh UA — `detectHasEarpiece()` handles it via `maxTouchPoints` and a screen
short edge ≤ 500 CSS px. An iPad must report no earpiece.

**Verify concealment:** four consecutive starved blocks before re-priming (`REPRIME_AFTER_STARVED = 4`);
the worklet repeats the last quantum with a fade rather than outputting digital silence; the worker
detects sequence gaps and conceals up to `MAX_CONCEAL_PACKETS = 6` beyond which a gap is a stall.
Saturate the Wi-Fi with audio playing: expect **a brief roughness, not a gap of silence**.
`concealed` should climb. **`underruns` climbing while `concealed` stays 0 means concealment is not
engaging** — that is a defect.

#### C-11 — Client screen wake
`static/index.html`, `static/nosleep.js`. Tiers: Screen Wake Lock → the `nosleep.js` looping-video shim → none.

**Verify the case that has no feature detection:** a **standalone home-screen app on iOS**, where
`navigator.wakeLock.request('screen')` *fulfils* and the screen sleeps anyway. `standaloneApp()`
detects it (`navigator.standalone === true` or `(display-mode: standalone)`) and runs the shim
**beneath** the wake lock there. Add the page to the home screen on an iPhone, join, and leave it
untouched past the screen timeout. It must stay awake.

**Verify the shim is silent.** `nosleep.js` plays a looping clip; the page mutes it
(`noSleepImpl.noSleepVideo.muted = true`), which also relaxes the autoplay rules that would refuse
it. Mute the stream's own audio and listen carefully on a device on degradation bit 3. **Any sound
is a regression.**

**Verify the lazy construction:** block `/nosleep.js` in devtools and reload — the join form must
still render and Connect must still work. A top-level `new NoSleep()` used to take the whole script
down.

**Verify:** bit 3 = the API is absent and the shim is carrying it (normal below iOS 18.4); bit 4 =
the request was refused outright, **and then the screen will sleep** — that is the one case to
report rather than accept.

#### C-12 — Client pointer input
`static/input.js`, `static/index.html`. Tiers: pointer lock with `{unadjustedMovement: true}` → plain → prefixed → hidden cursor.

**Verify:** on a laptop client, enter fullscreen and move the mouse across the whole host desktop.
`pointerLockUnadjusted: true` confirms raw deltas; **degradation bit 2 means the platform refused
and the client's OS pointer curve is compounding with the host's**, which feels like double
acceleration. Tier 1 exists on Chrome 88+ (Windows, macOS 10.15.1+, ChromeOS — not Linux),
Firefox 152+ and Safari 18.4+.

**Verify the three failure shapes are distinguished**, because they were not: an options bag that
*throws* (no lock requested at all — the plain retry is what saves relative movement for the
session), an options bag that *rejects* (retry plain), and a call that returns `undefined` (Chrome
88–110 honours `unadjustedMovement` and still returns undefined, so this is genuinely unknown; the
page degrades rather than over-report raw deltas).

**Verify letterbox coordinates.** Use a device whose aspect ratio differs from the host display —
a phone in portrait against a 16:9 desktop, where the bar is largest. Tap a specific icon near the
top of the picture: **the click lands on that icon** (before the fix it landed high by half the
bar). Tap **inside the black bar**: nothing happens on the host. Do this at several DPRs and in both
orientations.

**Verify coalesced events:** drag slowly in a drawing app from a touchscreen — a smooth line, not a
polyline of straight segments. Degradation bit 14 means the browser has no `getCoalescedEvents` and
one sample per frame is expected. **Firefox Android has the method and always returns `[]`** —
confirm the page handles that (it falls back to `[e]`), and confirm the bit reflects reality.
The cap is `MAX_COALESCED_PER_FLUSH = 8`, keeping the newest samples, because the host replays a
batch one injection per sample.

**Verify `#imeSink` does not raise the software keyboard** on a tablet with only a stylus attached.
`hasFinePointer()` requires both `(any-pointer: fine)` **and** `(any-hover: hover)` for exactly this
reason: a pen matches the first and not the second.

#### C-13 — Client fullscreen
`static/index.html`, `static/styles.css`. Tiers: `Element.requestFullscreen` on the stage → prefixed → `video.webkitEnterFullscreen` → viewport-sized elements.

**Verify on iPhone:** `fullscreen: false`, degradation bit 0, and **the picture fills the viewport
anyway**. iPhone Safari cannot make an element fullscreen at all; this is designed behaviour, not a
failure. On **iPad** the stage does go fullscreen.

**Verify the regression that must not return:** only the unprefixed form is specified to return a
Promise. Safari below 16.4 returns `undefined` from `webkitRequestFullscreen`, so a `.catch()` on it
threw a `TypeError` — and because `startLock()` is called un-awaited from `onStreamLive()`, that
rejection silently skipped **pointer lock, keyboard lock and the wake lock for the whole session**.
`enterFullscreen()` now always returns a Promise. Confirm on a Safari 15/16.0 device that all three
still come up.

**Verify keyboard lock:** in fullscreen on Chrome, press Escape — the key must reach the host rather
than leaving fullscreen. `keyboardLock: false` is expected on Safari and Firefox (degradation bit
21). The lock is requested **after** fullscreen is re-entered, because `navigator.keyboard.lock()` is
specified to reject outside it.

#### C-14 — Client input tails and the fast-channel sequence
`static/input.js`, `streamer/input/protocol.rs`. **Read §5.3 first** — the client half is in your
tree, so this is a verification item.

Three optional tails, each gated on a host capability bit read off the `PONG` at offset 17:
`INPUT_TIMESTAMP` (bit 3) adds a `u64` client stamp (`performance.now() * 1e6`, the same timebase
the PING carries); `INPUT_EVENT_FLAGS` (bit 4) turns `MOUSE_DELTA` byte 8 — always a zero pad — into
a flags byte whose bit 0 says the deltas are raw device counts with no OS pointer curve;
`INPUT_SEQUENCE` (bit 5) appends a `u16` reorder sequence past the stamp on `POINTER_*` (offset 48),
`POINTER_MOVE_BATCH` (`10 + n*24 + 10`), `WHEEL` (23) and `MOUSE_DELTA` (17).

**Verify:** `ScreenExtend status --json` — the `input_recv` histogram is **structurally empty** and
`input_reorder_drops` **structurally 0** until a client sends these. Making both non-empty is the
whole item. `input_recv` reports *excess one-way delay* (the frame's `host_recv − client_send` minus
the rolling 30 s minimum), so it reads 0 on the best sample and grows with queueing — a relative
number by construction, which is all one direction can honestly give.

**Verify the reorder drop actually fires** — this needs an induced-reorder link (`clumsy`'s
out-of-order, or the TURN relay). Then settle the one open question on this path (§5.4):
`MOUSE_DELTA` is `is_supersedable`, so a reordered frame is dropped — but its **position term is a
relative increment, and increments commute**, so a reordered delta's motion was safe to apply and is
lost permanently instead. The reason the frame is gated at all is the **button word** in it, which
*is* order-sensitive: the frame mixes an order-insensitive term with an order-sensitive one and the
code sacrifices the first to protect the second. **The fix is to split them** — apply the motion
unconditionally and gate only the button transitions, or carry the buttons on a separate ordered
frame. **Measure the cursor-travel error with and without, under 3 % reorder**, then implement the
winner; this is the one item in the guide where the defect is understood and only the number is
missing.

**Verify the capability gate itself:** a session's first second is plain v1 frames, because
`hostCaps` is 0 until the first `PONG` — which is also what an older host gets for the whole
session, and is correct for both. `teardown()` resets `hostCaps` to 0 so a reconfigure onto a
different adapter cannot inherit the last host's answer.

#### C-15 — Client stats frame v3 end to end
`static/index.html`, `static/audio.js`, `static/input.js`, `streamer/telemetry.rs`.

**Verify** the full 104-byte frame arrives and every field is read: run `ScreenExtend status --json`
per session and confirm every field in the §4.4 layout has a plausible value, and that `serve -v`
names the four tiers in words and lists the set degradation bits by name. Confirm the floor
behaviour: a frame one byte short of 96 reads as v2-tail-absent with everything before it intact,
and a v3 frame's last 8 bytes are dropped by a v2 host rather than half-parsed.

**Verify** the swallowed-error counter moves. 49 formerly-bare `catch (_) {}` now call
`noteSuppressed()`; the total rides offset 76 and is the sum of the page's, the worker's and the
audio module's. A device whose `suppressed` climbs steadily is telling you something is failing
repeatedly and silently — **chase every one of those**. That is a finding per cause.

**Verify** `clockValid`: `hostToPaint` is only populated once the clock offset arrives, and the
client keeps the **minimum-RTT** sample (aged out after 30 s so a route change is not remembered
forever). `hostToPaint.p50` of 0 **with** `clockValid: true` is the bug to look for.

#### C-16 — Four small client behaviours
`static/audio.js`, `static/input.js`, `static/index.html`.

- **The `/audio-outputs` poll** drops from 4 s to 30 s where `devicechange` exists, and
  `postOutputs` skips a body identical to the last one. **Measure the request count over a minute**
  per device (host access log or a devtools network panel) and confirm it is 2 rather than 15 on a
  browser with `devicechange`. On the postMessage audio ring this poll shares the thread that
  forwards every 5 ms packet, so it is not free.
- **Dropped-file screening** against `pc.sctp.maxMessageSize`: a file above the limit is refused
  **before it is read**, with a named console warning. `maxMessageSize` can be `Infinity` and
  `pc.sctp` is absent below Chrome 76 and on Safari, where the limit is not enforced at all —
  confirm both branches. Drop a 50 MB file and a 50 KB file on Chrome and on Safari.
- **The refresh-rate refinement**: a *second* 60-sample measurement taken once the page is live and
  applied only when it disagrees with the join-time value by ≥ 5 Hz. Verify on a 120 Hz tablet whose
  first half-second was in a background tab — the refinement is the safety net for exactly that, and
  on a stationary tablet nothing else ever reconfigures.
- **`detectHasEarpiece`** by screen short edge (≤ 500 CSS px) — see C-10.

#### C-17 — The audio capability handshake
`static/audio.js`, `streamer/webrtc_session.rs`.

`wantsFallbackTrack` and `verified` are wired on the host and **inert until a client advertises
them**. Verify the host log reads `audio caps: verified=… wantsFallbackTrack=…` per join, and that
the answer actually contains a recvonly Opus track when the client asked for one. **WHEP has no
renegotiation verb**, so a track absent from the first answer can never be added later — this is the
only moment a spare can exist, and it is what makes the mid-session decoder failure in C-07 a
recoverable event rather than silence. Confirm the spare track is *remembered and not attached*
while the WebCodecs path is live (`noteFallbackStream`), because attaching it would report `netEQ`
in the stats frame and play a second silent element beside the worklet.

### 8.2 Workstream B — audio under loss

#### C-18 — Audio packet-level redundancy
`streamer/audio/protocol.rs`, `streamer/webrtc_session.rs`, `static/audio-worker.js`.

`FLAG_REDUNDANT` (1 << 2) makes the body `u16 LE primary length | primary Opus packet | previous
frame's Opus packet` — RFC 2198-style redundancy without the RTP framing, and the only
loss-robustness tool this path has, because the host's encoder is CELT-only at 5 ms where libopus's
in-band FEC is structurally inert. The host sets it only against a client whose join advertised
`redundancy: true`, and only when the video BWE reports ≥ 0.5 % loss — plus unconditionally from
packet one on a `CrossNetwork` session (ledger row 213).

**Verify with induced loss at 2 % and then 5 %, audio playing:**
- Above the threshold, `redundantUsed` climbs and `concealed` stays far lower than without
  redundancy. A single lost packet should be **inaudible**; only bursts of two or more audible.
- `redundantDiscarded` climbing with `redundantUsed` at zero is payload spent for nothing and points
  at the host's threshold, not the client.
- **Listen for doubled audio.** A copy played when its own frame also arrived emits the same 5 ms
  twice — that is the failure the sequence de-duplication exists to prevent.
- `FLAG_DISCONTINUITY` (the host saying the capture itself broke) must suppress **both** the copy
  and the concealment, because there is nothing to bridge.

**Verify the relayed start:** a `CrossNetwork` join carries redundant packets from the first one —
payload roughly doubles, visible as bytes on the audio DataChannel — and a `SameNetwork` join on the
same build does **not** until loss appears.

**Optimise:** sweep the 0.5 % threshold. Redundancy doubles the audio payload, which competes with
video for the same budget; the right threshold is the one where `redundantUsed` per minute justifies
the bytes. Measure `concealed` per minute against payload bytes at 0.25 %, 0.5 %, 1 % and 2 %
thresholds and pick on the numbers.

`node scripts/test-client-audio.mjs` is the offline half: a plain packet, a redundant one with no
loss (the copy must be **discarded**), a redundant one after a single loss (the copy decoded first,
at the earlier timestamp), a duplicate, a late packet, a discontinuity, and a truncated redundant
body.

### 8.3 Workstream C — negotiation and codec

#### C-19 — H.264 profile negotiation against real engines
`streamer/negotiate.rs`, `streamer/webrtc_session.rs`.

The host picks High > Main > Baseline from the client's own `profile-level-id`, and derives the
answer's level from the negotiated geometry under `level-asymmetry-allowed=1`. **Nine unit tests
cover it; no browser has ever exercised it.**

**Verify per device:** the join log prints
`H.264 negotiated: profile=High level=0x2a (offered=[Baseline, Main, High], level-asymmetry=true, hevc=false)`.
Confirm each client **decodes what it negotiated** — Safari 13.1, an old iPad, an old Android,
Chrome, Firefox, and the constrained-baseline-only device, which must land on Baseline and still
work. `ffprobe` a captured stream and confirm the SPS's `level_idc` matches the advertised
`profile-level-id`.

**Measure what High is worth:** A/B the same 60 s desktop workload at a fixed rate-control target,
Baseline versus High, and compare VMAF or SSIM; or hold quality and compare achieved bitrate. The
expected saving is 5–15 % from CABAC alone, more on flat regions and sharp text from the 8×8
transform. **Nobody has measured it.**

**Collect the HEVC number.** `negotiate::read_offer` already records whether the offer carried an
H.265 payload type and the join logs it. Record it for **every device in the matrix** and put the
share in `docs/RESULTS.md`. That number decides whether an HEVC encoder is worth building later, and
it is free to collect while you are joining from eighteen devices anyway.

### 8.4 Workstream D — the link

#### C-20 — DSCP / WMM marking
`streamer/sockets.rs`, `streamer/platform.rs`, `streamer/webrtc_session.rs`.

**The tier ships switched off and this measurement decides whether it ships on.** In its first form,
bound to `0.0.0.0`, it was a measured **regression**: the WHEP self-test connected on **3 of 5**
runs, because the muxed gather emits a host candidate per local address (24 on the dev box) while
one socket can only send from one route, so pairs pass their inbound connectivity check and fail
outbound. It now binds the adapter the streamer serves and filters ICE to that address, which
removes the cause — but the self-test cannot exercise that path because its config leaves `bind_ip`
unspecified. With the tier off, 5 of 5.

```sh
SCREENEXTEND_MEDIA_SOCKET=1 ScreenExtend serve -v
```

**Before believing any latency number from it, check the join still connects 10 times out of 10.**
Then confirm the LAN tier costs nothing it should not: the muxed socket disables server-reflexive
gathering, so a same-subnet join must still connect and `ICE candidates in local answer (host)` must
report **one** host candidate, on the adapter's own address. More than one means the IP filter is not
working and the flakiness is back.

Expected log lines:
`media socket: muxing ICE over <adapter ip>:NNNNN with SO_SNDBUF, SO_RCVBUF, SIO_UDP_CONNRESET=off`
then, once the pair is nominated, `media socket: marked as interactive video for <client ip:port>`.
`DSCP marking unavailable` means qWAVE is off or absent — **record which**; the reference Windows box
reports exactly that.

**The measurement:** two stations on one WMM-honouring AP, one saturating the link with `iperf3`.
Record p99 of the client's display lag (`hostToPaint.p99`) and `fraction_lost` from the BWE log
line, with marking on and with `mark_video_flow` commented out. With a monitor-mode capture (a
second Wi-Fi adapter in monitor mode, or a Mac's `airport -z` + Wireshark), confirm the QoS control
field's TID carries **AC_VI**.

**"This AP ignores DSCP entirely" is a real and publishable result.** Many consumer APs do. Test
both APs and record both answers.

Separately: with `SO_SNDBUF` at 1 MB, count `track write_rtp failed` lines during a keyframe storm,
before and after.

**macOS host note:** the marking is `SO_NET_SERVICE_TYPE` (0x1116) plus `IP_TOS` AF41 (0x88), which
macOS accepts on an unconnected socket with no entitlement. Windows deliberately does nothing with
`setsockopt(IP_TOS)` — it is silently ignored unless a group policy allows it — and uses qWAVE's
`QOSAddSocketToFlow(QOSTrafficTypeAudioVideo)` instead.

#### C-21 — Send pacing tiers 2 and 3
`streamer/pacer.rs`.

Three tiers: `Off` (same-network — verified free on a LAN), `Light` (5× pacing factor, 24-packet
burst), `Full` (libwebrtc's 2.5× factor with Stadia's measured ~8-packet burst). Tiers 2 and 3 have
**never run on a real contended path**.

**Verify** on a relayed session and on a busy AP: packet-cluster size in a capture (Wireshark on the
client, or a monitor capture), and p99 of the client's `hostToPaint`.
`pace_wait` in `status --json` holds **only** the time the pacer slept, separately from
`socket_write`, so "the pacer is free on this path" is distinguishable from "the socket is slow". On
a `SameNetwork` session that histogram must be all zeroes — confirm it is.

**Watch `pacer_deadline_misses`** — packets released early because holding them longer would have
outlasted the frame interval being smoothed. A pacer that grows unbounded is worse than no pacer, so
this must stay small.

**Optimise:** sweep the pacing factor and the burst size on the relayed path. The tier boundaries
(`locality_for` → `PacingMode`) are yours to move on the numbers.

#### C-22 — RTP MTU on a relayed path, and the playout-delay hint
`streamer/server.rs` (`locality_for`), `streamer/webrtc_session.rs`.

The MTU is `1200 - 12 = 1188` bytes of payload same-subnet and `1024 - 12 = 1012` once the path goes
through a relay, because the datagram then carries TURN framing on top and can exceed a tunnelled
path MTU (1400/1420 are common) or the 1280-byte IPv6 minimum — and a fragmented UDP datagram loses
everything when one fragment drops. Moonlight caps remote streams similarly.

**Verify:** the join log's locality line, then stutter vs no stutter across the relay. Force the
wrong MTU (set both constants to 1188) and confirm you can reproduce the failure the smaller value
prevents; if you cannot, that is worth knowing too — the constant may be costing throughput for
nothing, and the honest answer is a measurement, not a guess.

**The playout-delay hint tiers:** `0, 0` same-subnet, `0, 200 ms` relayed, absent on Firefox and
WebKit (Firefox strips the answer attribute — Mozilla bug 1585009). Remember §5.5: on the WebCodecs
path this is **inert**, because the encoded transform takes the frame before the frame buffer. It is
real on the `<video>` fallback. So measure it there, on Chromium and on Firefox, and record which
engines honour it at all.

#### C-23 — Congestion-signal tiers 2 and 3
`streamer/congestion.rs`, `streamer/bitrate.rs`, `streamer/webrtc_session.rs`.

Tier 1 (transport-wide congestion control) is verified. Tier 2 is REMB, for a client that declines
`transport-wide-cc`; tier 3 is RTCP receiver-report loss plus the RTT-excursion detector. **No client
on the dev boxes declines the extension**, so tiers 2 and 3 have never been selected.

**Verify:** the once-per-session line
`BWE: congestion signal is <name> (twcc reports=…, remb reports=…)`. Find a browser that declines
TWCC — try Safari variants and older Firefox, and if none of your clients does it naturally, strip
the extension from the offer with a small SDP munge in the page (temporarily, behind a query flag
you remove before committing) to force the tier. **Forcing a tier to prove it works is legitimate;
shipping the forcing switch is not.**

The RTT arm uses a rolling baseline with a > 40 ms excursion for two consecutive polls, not an
absolute threshold — an absolute 80 ms threshold pinned every WAN session at the 1 Mbps floor.
Verify a steady 150 ms WAN RTT **holds** the target rather than cutting it.

#### C-24 — The loss-based bitrate driver's step response
`streamer/bitrate.rs`, `streamer/congestion.rs`, `streamer/keyframe.rs`.

`estimate_from_loss` is continuous from 1 % upward and meets GCC's `1 − 0.5·loss` exactly at 10 %;
the driver polls `getStats` every 120 ms; `DEFAULT_MIN_BITRATE_BPS` is 1 Mbps; downward moves have a
250 ms floor.

**Measure the step response** at injected 1 %, 3 % and 5 % loss: `fraction_lost`, the count of
`request_idr()` calls, `idrs_sent`, `bitrate_cuts`, `delay_overuse_events`, and p99 of the client's
`hostToPaint`. Plot the target bitrate against time across a step from 0 % to 5 % and back.

**The questions to answer with numbers:** is 120 ms too slow? Is loss the right signal, or does the
delay-gradient arm (`delay_overuse_events`, which fires *before* loss) get there first often enough
to drive on? Should **resolution** adapt too, not just bitrate — and can that be done without a
keyframe storm or a visible reset? Answer each by measuring, and record the answer either way.

**Watch for the keyframe storm.** `keyframe_requests_suppressed` against `keyframe_requests` is the
pair; a large suppressed ratio is healthy.

#### C-25 — Repair from a long-term reference instead of an IDR
`streamer/repair.rs`, `streamer/keyframe.rs`, `windows_utils/streamer/nvidia/` (read-only).

**NVENC host only.** Confirm the tier is live:
`NVENC: 2 long-term reference slot(s); a lost frame can be repaired without an IDR`. If that line is
absent the encoder has no slots and every recovery is a keyframe — the pre-existing behaviour, not a
failure. DXGI duplication, the synthetic probe path and macOS all report zero slots.

The byte cost is measured: **23,698 B repair against a 28,423 B IDR** on a synthetic worst case
where every pixel changes every frame. **The rate is not.** Induce 2 % and then 5 % loss and count
per minute, from `status --json` and the log:
- `idrs_sent` — the number this change exists to reduce;
- `keyframe_requests` and `keyframe_requests_suppressed`;
- `picture recovery: … (repairing from a long-term reference, no IDR)` lines — the repairs that
  replaced a keyframe.

Compare the client's p95 inter-frame gap and `framesDropped` across the two runs. **The target is
that a single lost packet costs one P frame's worth of artefact rather than an IDR burst.**

Also watch `nvEncInvalidateRefFrames` refusing
(*"the frame has probably already left the DPB, so this loss needs a keyframe"*). A high refusal rate
means feedback is arriving later than the DPB holds the picture, and the **marking cadence should be
slowed** rather than the tier abandoned. Tune it on that number.

#### C-26 — Rolling intra refresh
`streamer/config.rs`, the encoder backends (read-only).

`cfg.intra_refresh = true` is on in the shipped path and verified as **no regression**. The
**recovery benefit** is unverified. Measure time-to-recover after a loss burst with intra refresh on
and off, at each loss rate, and the frame-size distribution (p50/p95/p99 of encoded frame bytes) —
the design claim is that it collapses the p99/p50 ratio from ~10× to ~2×. If it does not, say so.

### 8.5 Workstream E — network topology

#### C-27 — Cross-network: the cloud relay and TURN
`streamer/cloud.rs`, `streamer/server.rs`, `streamer/webrtc_session.rs`, `streamer/session.rs`.

**Nothing in this project has ever exercised the relay against a real `session.screenextend.app`.**
The control channel is a WebSocket (`tokio-tungstenite`) to
`wss://session.screenextend.app/host/v1/connect`, protocol version 1, 45 s heartbeat timeout,
exponential backoff to 30 s, with `CloudState` surfaced to the UI via `CloudStatusChange`.
Cross-network joins additionally require a configured TURN server.

**Verify:** `CloudStatusChange` reaching the desktop UI; a completed relayed join from the second
network; `ScreenExtend status --json` showing the session as `CrossNetwork`. Verify the typed
`RegisterRejected` error on a "session taken" condition (start two hosts with the same session id).
Verify the reconnect: kill the network for 60 s and confirm the backoff behaves and the session
recovers rather than wedging.

**Verify the relayed join restructure:** the WHEP handshake and the display+pipeline work run under
one `tokio::join!`, measured on a LAN at 12–20 ms of handshake against 77–221 ms before. **The
relayed case is unmeasured** — measure it, and expect the handshake term to dominate.

**Measure the whole relayed budget** and put it in `docs/RESULTS.md`: time-to-first-frame,
`hostToPaint` p50/p90/p99, `fraction_lost`, and the glass-to-glass camera number. This is the
condition where ScreenExtend is furthest from Luna and closest to Hopp, and it is the one nobody has
ever seen.

#### C-28 — STUN configuration tiers
`streamer/server.rs`, `streamer/session.rs`, `src-tauri/src/cli.rs` (read-only).

Three tiers: **none** for a same-subnet peer (in the answer's `RTCConfiguration` *and* in the
`/ice-config` the client reads), a configured `stunUrls` setting, and the built-in Google default
(`stun:stun.l.google.com:19302`). An empty configured value means "no STUN at all" and must be
honoured as such rather than falling back to the default.

**Verify** with `ScreenExtend stun show` and the ICE candidate list in the client's offer, on a LAN
join and on a cross-network join. Confirm a same-subnet join gathers **no** srflx candidate — that
is a round trip removed from every LAN join and it is worth confirming rather than assuming.

#### C-29 — IPv6-only network
`streamer/server.rs`, `streamer/config.rs`, the two `networking.rs` (read-only).

`Config.bind_ip` is `IpAddr`, and the URL host must be bracketed per RFC 3986. Link-local addresses
must be **reported rather than silently served**.

**Verify:** a successful join over a global-unicast v6 address; the QR and join URL carry
`https://[2001:db8::1]:8443/`; a link-local `fe80::` address is named as such and not offered as a
join target. If you cannot build a v6-only segment, a dual-stack network with v4 disabled on the
client is close enough — say which you did.

#### C-30 — Wired versus wireless
`streamer/platform.rs`, the two `networking.rs` (read-only).

`link_medium_for_local_ip` already reports `wired` / `wireless` / `other` for the local end of the
nominated ICE pair, half a second after connection:
`session link: local candidate 192.168.1.20 is on a wired adapter`
(Windows: `MSFT_NetAdapter.NdisPhysicalMedium`, 14 = 802.3, 9/1 = 802.11; macOS: `getifaddrs` then
`SIOCGIFMEDIA`). **USB tethering and RNDIS both report Ethernet**, which is the point — a phone on a
cable is an ordinary IP interface.

**The decision this makes:** whether ScreenExtend should document the USB-tether route as a
low-latency tier, or close it as "measured, not worth documenting". Both outcomes are results.

Setup (Android only; iOS has no USB tethering, so the honest documentation is Android-only):
Settings → Hotspot & tethering → USB tethering, cable in the host, then
`ScreenExtend serve -v --no-cloud` and open the join URL on the tether subnet. **Confirm the log says
`wired`.** If it says `wireless`, the phone joined over Wi-Fi instead and the measurement is void.

Measure glass-to-glass by camera (§8.6) for:

| Case | Host link | Client link |
|---|---|---|
| A | wired Ethernet | USB-tethered Android |
| B | wired Ethernet | 5 GHz Wi-Fi, quiet AP |
| C | wired Ethernet | 5 GHz Wi-Fi, competing 4K stream on the same AP |
| D | wired Ethernet | Wi-Fi 6, quiet AP |

Report p50 and p99 for each. **Luna Display's own 50-sample bench is 11.3 ms mean USB against
22.4 ms Wi-Fi** — that is the comparison. Also verify the detection does not misclassify a Wi-Fi
adapter behind a virtual switch (Hyper-V, VirtualBox host-only), which is a realistic false positive.

#### C-33 — A host with three network adapters
`streamer/mod.rs` (`transport_runtime`).

The one-process-wide tokio runtime claim — that worker threads drop from `available_parallelism() × N`
(48 on the reference box with 3 adapters) to a clamped 4 — is **arithmetic, not a measurement**,
because the dev box has one adapter. If your host can present three adapters (Ethernet + Wi-Fi + a
USB Ethernet dongle, or a hosted network alongside), measure thread count and startup wall time with
one, two and three adapters. Note that the previous startup measurement showed **no improvement and
medians 69 ms apart in the wrong direction**, on a box shared with 4–11 concurrent compilers, with
overlapping spreads (before 325–416 ms, after 375–521 ms) — i.e. noise. A clean-box re-measurement
settles it, and you have a clean box.

### 8.6 Workstream F — input, multi-client, and the camera rig

#### C-31 — Two pen-capable clients at once
`streamer/input/windows.rs` (read-only unless the measurement says otherwise).

Synthetic pen events now carry the client's own `pointerId` instead of the literal `1`. Whether
Windows disambiguates two synthetic pen contacts across two device handles is **undocumented**, so
the fix is a correctness argument, not an observation.

1. Join two pen-capable clients, both with remote control.
2. Draw a continuous stroke on each **at the same time**, in a host application that shows ink
   (Paint, Fresh Paint, Photoshop).

**Expect two independent strokes.** If the host application still shows one stroke jumping between
the two clients' positions, the id is not what disambiguates them and the per-session pen device is —
**record that**, because it decides whether the injector can ever be hoisted to one process-wide
instance. Also confirm a touch drag still reports a real contact size on the host (touch does not
batch; pen does — a batch sample is 24 bytes with no room for contact width and height).

#### C-32 — Four simultaneous clients
`streamer/session.rs`, `streamer/input/`, `windows_utils/streamer/{tuning,nvidia}/` (read-only).

1. **Single-client baseline first**: host CPU, the `capture_delivery` / `encode` /
   `capture_to_socket` percentiles from `status --json`, and the process's thread and handle counts
   (`Get-Process ScreenExtend | Select Threads,HandleCount`, or Process Explorer).
2. Join four clients with remote control, drag continuously on all four for 60 s, take the same
   readings.
3. Disconnect all four and take thread and handle counts again.

**Expect** thread and handle counts to return to the single-client baseline: `SessionHandle::close`
joins the injector thread and `Drop for Injector` destroys the synthetic pen and touch devices, so a
residue is a leak and a finding. `live_transport_sessions` in `status --json` must equal the number
of live sessions; a number that only climbs across joins is a leak.

**The number that decides the injector hoist** is step 2's `encode` percentile against step 1's:
four `THREAD_PRIORITY_TIME_CRITICAL` injector threads sit above the encode thread in the scheduler,
and if that shows up as encode-stage tail growth, hoisting to one process-wide injector is worth its
arbitration policy. If it does not, per-session injectors are correct as they stand.

**Also measure:** `frames_dropped_broadcast` (zero in all twelve single-viewer host runs — four
viewers is where it can move), the idle-repeater priority drop, and the **NVENC concurrent-session
cap**, measured at **8** on the RTX 3080 Ti with the 9th refused and the other eight unaffected.
Open sessions until refusal and confirm the refusal is *identified*, not reported as a generic
failure. And watch whether two clients dragging at once produce a usable cursor at all — they share
one physical cursor and there is no arbitration today.

#### C-34 — Glass-to-glass by camera, against the competitor table

**This is the only measurement in the entire project that is not self-reported, and it is the reason
this guide exists.** Without a side-by-side, every number ScreenExtend publishes is unfalsifiable.

**The procedure** (from `docs/BASELINE.md`, which you extend):

1. Run the host with the frame counter:
   `ScreenExtend probe --live --duration 60 --synthetic-pattern --workload motion --counter`.
   The overlay is a large frame number plus an elapsed-millisecond clock drawn **into the encoded
   frames**, so whatever the client displays carries the value the host encoded.
2. Put the client device beside the host display, both showing the same region.
3. Film both screens in one shot at 240 fps.
4. Step through the recording. For each sample, subtract the counter shown on the client from the
   counter shown on the host and multiply by the frame period. **At least 30 samples.**
5. Report p50 / p90 / p99 alongside the camera model, its frame rate, and both refresh rates.
   **A 240 fps camera quantises to 4.17 ms; state that with the result.**

**Fix the caveat first.** `--counter` paints into the *synthetic* generator, which bypasses screen
capture, so the camera number excludes capture delivery. Two ways to close it, and you should do
both: (a) add the measured `capture_delivery` p50 for the backend under test and say so; (b) better,
extend the probe so `--counter` can paint into the **borderless top-most window** that
`probe --live --workload <w>` already uses to cover the captured display — that path does go through
real capture. `streamer/synthetic.rs` is where the overlay lives, and `probe --live --workload` is
already wired to the window path. Doing (b) makes every camera number in this guide a true
glass-to-glass figure. Record which you used per cell.

**The matrix to film.** Each cell is p50 / p90 / p99 over ≥ 30 samples:

| | ScreenExtend | spacedesk | Deskreen | Duet Display | Parsec | Moonlight+Sunshine | Steam Link | AirPlay→iPad |
|---|---|---|---|---|---|---|---|---|
| Wired host / 5 GHz client | | | | | | | | |
| Wired host / Wi-Fi 6 client | | | | | | | | |
| Wired host / USB-tethered Android | | | | | n/a | n/a | n/a | n/a |
| Hosted network | | | | | | | | |
| Cross-network via TURN | | n/a | n/a | | | | | n/a |

**Identical host, identical client device, identical AP, identical room, same session.** That is the
whole value of the exercise. AirPlay is only comparable if the host is a Mac; if it is, run it,
because "AirPlay beats this app on the same hardware" is the observation that started this
engagement and it has never been quantified. For competitors with no counter overlay, use a
millisecond clock on the host desktop (any stopwatch app with a centisecond digit) and film both
screens — the method is the same and the resolution is the same 4.17 ms.

**Then also film ScreenExtend's own tiers against each other**, because these are the numbers that
tell you where to spend the next week:
- `videoPath` 1 (WebCodecs → canvas) vs `videoPath` 3 (`<video>` element) on the same device;
- `renderTier` 1 (canvas) vs 2 (`?present=track`), windowed and fullscreen;
- host wired vs Wi-Fi 5 vs Wi-Fi 6;
- with and without the DSCP marking (C-20), under contention.

Record everything in `docs/BASELINE.md` under a new "Camera-measured glass-to-glass" section and
summarise in `docs/RESULTS.md`.

### 8.7 Suggested agent partition

Run at most eight Opus 5 agents concurrently. The device sweeps are serialised on you; everything
else parallelises.

| Agent | Territory |
|---|---|
| **G5-A** | The client page's video half: C-01, C-02, C-03, C-04, C-15. Owns `index.html`'s transport ladder and `transform-worker.js`. |
| **G5-B** | The client page's audio half: C-07, C-08, C-09, C-10, C-17, C-18. Owns `audio.js`, `audio-worker.js`, `audio-worklet.js`. |
| **G5-C** | Join, session and input: C-05, C-06, C-11, C-12, C-13, C-14, C-16, C-31. Owns `input.js` and the join path, including the six trickle/token/net-config fixes listed in §5.4 that no browser has ever exercised. |
| **G5-D** | Transport shaping: C-19, C-20, C-21, C-22, C-23, C-24, C-25, C-26. Owns `sockets.rs`, `pacer.rs`, `congestion.rs`, `bitrate.rs`, `negotiate.rs`, `repair.rs`. |
| **G5-E** | Topology: C-27, C-28, C-29, C-30, C-33. Owns `cloud.rs` and the network setup. |
| **G5-F** | The camera rig and the competitor bench: C-34, plus the `--counter` capture fix in `synthetic.rs`, plus C-32. |
| **G5-R1..R3** | Cold reviewers (§11.2). Spawned per client diff, never given the author's framing. |

Hand each agent this guide, its own item list, the relevant `findings/` files, and the specific
ledger rows it must flip. Hold them to the same standard this guide sets for you: a report of "done"
is not accepted while any of that agent's findings files remain.

---

## 9. Phase 4 — the research agenda

Run these concurrently with Phase 3, as dedicated Opus 5 research agents. Each produces a written
report into `docs/research/` with citations and an explicit "what we should do about it" section, or
extends an existing one. **Prefer primary sources.**

1. **Does the `<video>` fallback really cost ~90 ms, and of what?** The figure is inherited from
   libwebrtc analysis and the Hopp teardown (jitter buffer 189 → 46 ms after tuning). Break it down
   on your hardware with C-01's measurement and reconcile it with the literature. Chromium's
   `RtpVideoStreamReceiver2::OnAssembledFrame`, the frame buffer, and `jitterBufferTarget`'s actual
   effect are all open source — read them, do not infer.
2. **Per-engine WebCodecs behaviour, 2026.** `optimizeForLatency` semantics per engine;
   `hardwareAcceleration` hint handling (Chromium's `prefer-hardware` installs a GPU-only decoder
   factory with **no software fallback**, so it reports unsupported on machines that would decode
   the stream fine — the page deliberately probes `no-preference` first and this must stay true);
   `ondequeue` availability and reliability; whether any engine has shipped a real
   `renderQuantumSize` / `renderSizeHint` (Chrome 153 desktop only at last look, which would shave
   the AudioWorklet's fixed 2.67 ms); `AudioContext.playbackStats` (Chrome 146) as a source of
   glitch counters the worklet currently has to infer.
3. **The iOS Safari WebCodecs story, specifically.** Which iOS versions have `VideoDecoder`, which
   have `AudioDecoder`, what `RTCRtpScriptTransform` does and does not do there, what the
   home-screen-app wake-lock behaviour actually is, and whether `webkitEnterFullscreen` on the
   `<video>` element can be combined with the canvas path. This is the single largest client
   population and the one with the least documentation.
4. **`getPredictedEvents()`, re-opened.** The wire format has the predicted-sample count byte and the
   host already knows to drop predicted samples for pen and touch and keep them for the cursor. The
   trade — latency saved against speculative positions the next real sample corrects — is a
   per-engine number **you can now obtain**. Research the current engine behaviour, then measure it:
   cursor-travel error against apparent responsiveness, on Chrome desktop, Chrome Android and
   Safari. Implement if it wins; record the number if it does not.
5. **Wi-Fi behaviour under a browser client.** Client-side power save (a phone's Wi-Fi power-save can
   add tens of milliseconds — can a browser be persuaded out of it, and does the wake lock affect
   the radio at all?); WMM access-category mapping on consumer APs and how much of the field
   actually honours DSCP; 802.11ax OFDMA and whether it helps or hurts a single low-rate,
   latency-sensitive flow; airtime fairness. Primary sources: the Wi-Fi Alliance specs, the Linux
   `mac80211` source, and AP vendor documentation.
6. **Does any browser expose a lower-latency presentation path than OffscreenCanvas 2D?** WebGPU,
   WebGL2 `texImage2D` from a `VideoFrame`, `requestVideoFrameCallback` pacing, and the compositor
   overlay question that C-04's A/B tests. Chromium's `html_canvas_element.cc` /
   `offscreen_canvas.cc` are the authority on `desynchronized`; the overlay behaviour is in the viz
   compositor. Find out whether the track generator can win an overlay at all, and on which
   platforms.
7. **The competitor teardowns, extended with your measurements.** `docs/research/competitors-*.md`
   already covers spacedesk, Duet, Luna, Parsec, Moonlight/Sunshine, Steam Link, Rainway, Chrome
   Remote Desktop, Sidecar, Miracast, Deskreen, Splashtop, Jump Desktop, Guacamole, RDP/RemoteFX,
   and OBS/NDI. Your job is not to redo them but to **attach real numbers from your own bench** and
   to answer the one open question in that file that only hardware can settle: where do the
   milliseconds actually live in *our* pipeline, now that both halves are measured?
8. **Rainway's HN thread (item 18242300)** is the one primary source in the competitor report that
   could not be fetched (HTTP 429). It is the closest published account of a browser-client
   game-streaming protocol. One retry.

---

## 10. Phase 5 — the hyper-optimisation sweep

After the verifications, go back over the whole area with the standard from §3: **every knob,
measured.** These are not "nice to have" — they are the reason token cost is not a constraint.

**Client page:**

1. **Every remaining main-thread action per frame.** The design intent is that nothing per-frame
   touches the page's main thread on the fast path: the DataChannel buffer is transferred to the
   audio worker untouched, the video transform runs in its own worker, the canvas is transferred.
   Profile a real device with the browser's performance panel and prove it, or find what is left.
2. **The audio path's per-packet cost at 200 packets/second.** On the `shared` transport a decoded
   packet reaches the speakers without touching a message queue. On `port` it crosses one. On
   `postMessage` it crosses the main thread twice and allocates two `Float32Array`s. Measure the
   main-thread time per second on a mid-range Android on each transport and use it to decide whether
   anything more can be done for the browsers stuck on tier 3.
3. **The 512-sample `StatWindow` sort.** `pct()` allocates and sorts on every stats request. It runs
   once a second, so it is almost certainly free — measure it and record the number rather than
   assuming.
4. **`sendStats()` at 1 Hz on the reliable channel.** 104 bytes plus a PING. Confirm it never lands
   in the same task as a frame paint on a slow device.
5. **The `/reconfig` poll.** One HTTP request per second per device, at `PROBE_OK_MS = 1000`
   normally and `PROBE_FAST_MS = 300` when the link is suspect. Measure what it costs on a phone and
   on the host at four clients, and re-evaluate the rejected "move it onto the reliable
   DataChannel" idea now that `server.rs` is inside your fence — the liveness argument against it
   (§5.5) still stands, so the only acceptable version keeps the HTTP path as a fallback.
6. **The join path's serial sections.** `Promise.all([capsPromise, readyPromise, resolveRefreshRate()])`
   already overlaps the three; `getIceConfig()` is a serial fetch before the peer connection exists.
   Time each segment on a slow phone and remove anything that does not have to be in front of the
   first pixel.
7. **Asset delivery.** Each static asset's `ETag` is a hash of its own bytes, so a device picks up an
   edit after a host restart with no hard reload. Confirm 304s are actually happening on a rejoin,
   and measure the cold-load cost of the six JS files on a 3G-class link — the join page is the
   first thing every user sees.
8. **The decoder configuration ladder.** `CODEC_CANDIDATES` is eight `avc1.` strings tried in order,
   in up to two rounds (sized then unsized), each an awaited `isConfigSupported`. On a slow engine
   that is a measurable delay in front of the first frame, and it happens on every session start.
   Measure it per engine and short-circuit where the answer is knowable.

**Transport (within the fence):**

9. **Every constant that a measurement in Phase 3 touched.** `ICE_HOST_GRACE_MS` (200),
   `ICE_RELAY_GRACE_MS` (400), `TRANSFORM_START_TIMEOUT_MS` (2500), `FRAME_WATCHDOG_MS` (5000),
   `WHEP_TIMEOUT_MS` (10000), `RECONFIG_DEADLINE_MS` (15000), `KEY_REQUEST_MIN_INTERVAL_MS` (250),
   `KEY_WAIT_ESCALATE_MS` (1500), `KEY_WAIT_GIVE_UP_MS` (8000), `MAX_IN_FLIGHT` (2),
   `IN_FLIGHT_STALE_MS` (500), `CODEC_HINT_DEADLINE_MS` (600), `AUDIO_PREPARE_DEADLINE_MS` (2000),
   `WORKER_READY_DEADLINE_MS` (3000), `REFRESH_DEADLINE_MS` (500), `MIN_TARGET_MS` (10),
   `MAX_TARGET_MS` (400), `SAFETY_STEP_MS` (4), `SAFETY_MAX_MS` (60), the pacing factors and burst
   sizes, the two MTUs, the 0.5 % redundancy threshold, the 120 ms BWE poll, the 250 ms downward
   floor, the keyframe gate's 200–1000 ms window. **Each of these was chosen from research or from a
   dev-box measurement. You have real devices and real links. Re-derive every one of them from your
   own numbers, keep the ones that survive, and record every change with its before and after.**
10. **The BWE poll at 120 ms.** Measure whether a shorter interval improves the step response enough
    to justify the `getStats` cost, and whether the delay-gradient arm makes the loss arm's latency
    irrelevant.
11. **Socket buffers.** `SO_SNDBUF`/`SO_RCVBUF` at 1 MB in `sockets.rs`. Sweep on a real lossy link
    and count `track write_rtp failed` during keyframe storms.
12. **Allocation on the send path.** The packetizer's allocation ratio is asserted at > 4× better
    than the previous implementation in a release bench; confirm it holds under four simultaneous
    clients.

**And the standing instruction:** anything you find in this sweep that shaves even **0.1 ms on one
browser** gets implemented and measured, not written up. If it turns out not to help, that is a
ledger row with a number, not a skipped item.

---

## 11. Verification and gates

### 11.1 The commands

Every command in §4.6, before every commit. Plus, for this guide specifically:

```sh
# the offline client harnesses — your regression net for every wire format. One command:
pnpm test:client
# or individually:
node scripts/test-client-stats.mjs      # stats v3 layout, PONG capability gate, clock arithmetic,
                                        # the input tails and the reorder sequence offsets
node scripts/test-client-input.mjs      # input.js golden vectors, cross-checked by the Rust parser
node scripts/test-client-audio.mjs      # audio-worker: plain, redundant-no-loss (discarded),
                                        # redundant-after-loss, duplicate, late, discontinuity,
                                        # truncated redundant body
node scripts/test-client-worklet.mjs    # the jitter buffer and the planar ring ABI
node scripts/test-client-transform.mjs  # the WebCodecs video path and A/V sync
node scripts/test-client-ice.mjs        # the candidate/grace table, relayed wait gating,
                                        # deadline → degradation bit 15, listener cleanup
node scripts/test-client-reconfig.mjs   # /reconfig epochs and kick

# the parse floor, after every client edit
npx --yes acorn@8 --ecma2017 --silent <each JS file, and the two inline scripts from index.html>

# the client-page hazard scan (it also reads CSS and HTML)
pnpm build && node scripts/check-legacy-webkit.mjs
```

**Extend the harnesses as you go.** Every new wire field gets an assertion in the same commit. They
are the only automated coverage the client page has. The finding that used to say there was no test
infrastructure at all is closed — `pnpm test:client` is the runner it asked for — so what is in
scope now is **coverage**, not a runner: any client behaviour you verify by hand on a device and
could have asserted offline is a missing harness case, and adding it is how the next person on the
next device does not have to repeat your afternoon.

### 11.2 The review discipline

The project's rule was that the client page must not be browser-automated, and that **multiple
independent Opus 5 cold reviews** take the place of execution. On your machine, **real devices
replace automation for anything you can actually run**. The review discipline still applies to
everything you cannot:

- Every client diff gets **at least three independent cold reviews** before it is committed, by
  agents that read the diff and the full post-change files **without being told what the change was
  supposed to do**. Different angles, no shared framing:
  1. correctness and control flow — exception paths, promise ordering, races between
     WHEP/ICE/transform/configure/first-frame, decoder queue and `VideoFrame` ownership, the audio
     ring producer/consumer arithmetic, the stats byte layout, the PING/PONG clock arithmetic, and
     **every path that can end in a black screen or silent audio with no indication**;
  2. feature detection and fallbacks — does every modern API have a working floor beneath it, and
     does the fallback actually *trigger* rather than throwing?
  3. old-browser compatibility — against §2.4's construct list and the ES2017 parse floor, using the
     acorn method, plus the CSS constraints.
- Pay special attention to the **silent-failure surface**. The client is full of paths that degrade
  quietly: WebCodecs falling back to `<video>`, `SharedArrayBuffer` unavailable because
  cross-origin isolation failed, `setSinkId` unsupported, wake lock refused, pointer lock denied. A
  bug here does not crash — it just makes the app slow, and the user experiences it as "still laggy"
  without knowing why. **Every one of them must log or report which path it took.**
- Where a review and a device disagree, the device wins and the review gets a finding.

Write each review to `docs/reviews/g5-review-<n>-<angle>.md` so the next person can read what was
already reasoned through.

### 11.3 The final production build

Per §4.6. State the result explicitly at the top of the report: which machine, that it succeeded,
and that the only error was updater signing.

---

## 12. Deliverables and the final report

### 12.1 What must be true when you finish

- `findings/` is **empty**, or every remaining file is listed in the report with why it will not be
  done.
- Every "Client …" row in `docs/CAPABILITY-TIERS.md` reads `VERIFIED (<device>, <version>)` or
  states explicitly what was missing.
- Ledger rows 28–37, 44, 46–48, 52–61, 65, 74–75, 171–177, 179–187, 192, 204–215, 271 and 285–288 in
  `docs/OPTIMIZATIONS.md` are flipped to `VERIFIED` with numbers, reverted with reasons, or
  annotated with what was missing.
- `docs/BASELINE.md` has a **camera-measured glass-to-glass section** with the competitor
  side-by-side, and its "Client arrival → paint, and glass-to-glass" row in "What could not be
  measured" is deleted or rewritten.
- `docs/manual-tests/client-page.md` is updated so its per-device table carries **observed** values
  beside the expected ones, and its fallback catalogue's "how to force the floor" column is
  corrected wherever forcing it turned out to work differently.
- `docs/RESULTS.md` exists with the sections below.
- The tree is clean, the branch is `guide/clients-network-and-glass-to-glass`, and **nothing was
  pushed**.

### 12.2 The report

1. **Machine facts** — exact OS build, host model, CPU/GPU, driver versions, every client device and
   its exact browser version, both AP models and bands and channels, the TURN server, the camera
   model and its verified frame rate. Everything a reader needs to reproduce.
2. **Per-item status**, one line each, against C-01 … C-34:
   `VERIFIED (numbers) | reverted (reason) | still UNVERIFIED (what was missing)`. **No item may be
   silently absent.**
3. **Before/after numbers** for every change, as p50 / p90 / p99 / max, with the trial count and
   whether the machine and the AP were quiet.
4. **The ledger diff** — the exact rows added or edited in `docs/OPTIMIZATIONS.md` and
   `docs/CAPABILITY-TIERS.md`, and the `docs/BASELINE.md` cells filled.
5. **Findings** — created, resolved-and-deleted, and retagged, by filename.
6. **The competitor comparison** — the C-34 matrix, with ScreenExtend's position stated plainly
   against Luna Display (22.4 ms Wi-Fi, the target) and Hopp (98 ms, the browser-WebRTC ceiling), and
   against every competitor you actually ran.
7. **Decisions Made Without Consultation** — every judgment call the user might have made
   differently, with your reasoning, so each can be reviewed and reversed.
8. **UI-visible recommendations (not applied)** — anything that would have helped but required a
   visible change, with its measured upside.
9. **What could not be done and why** — honestly, including negative results, which are results.
10. **The client test plan, updated.** `docs/manual-tests/client-page.md` is the artefact the user
    reads with a phone in hand. Leave it better than you found it: every fallback with a *verified*
    way to force it, every expected value confirmed or corrected, and every new tier documented.
11. **Commits** — branch name and one line per commit. Nothing pushed.

---

## 13. A closing note on what "done" means here

The other four guides in this folder close a platform. This one closes the **half of the product the
project has never seen.** Eighteen device rows, nine network conditions, thirty-four items, and one
camera — and at the end of it the first honest sentence anyone will have been able to write about
ScreenExtend's latency: not "the host path is 3.93 ms at p90", which is true and insufficient, but
"a phone on a Wi-Fi 6 AP sees the host's pixel *N* milliseconds after it changed, against Luna's
22.4 and spacedesk's *M* on the same table."

Everything else in this guide exists to make that sentence true and defensible.

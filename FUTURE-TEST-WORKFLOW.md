# FUTURE-TEST-WORKFLOW.md — test what the overhaul-new job could not

You are the agent for a later ScreenExtend test job. This file is your whole brief, and nothing else has been said to you. Read it to the end before you act.

---

## 0. PRECONDITIONS — read and satisfy before anything else

**The input ban is in force until the owner lifts it in writing.**

The owner said: "do NOT manipulate my cursor. displays is fine. if you want to simulate a cursor, paint a toiny moving window. do not hijack the cursor".

Until the owner explicitly lifts that ban **for this job, in this conversation**, you must not do any of the following:

- inject input by any route: no `SendInput`, `SetCursorPos`, `mouse_event`, no keyboard injection;
- call browser-protocol `Input.dispatchMouseEvent`, `Input.dispatchKeyEvent` or `Input.dispatchTouchEvent`;
- create synthetic DOM pointer, keyboard or wheel events on the client page (the page forwards them to the host, which moves the real cursor);
- run `scripts/measure-input.ps1`, `scripts/measure-stream.ps1` or `scripts/test-startup-gate.mjs`;
- start the desktop GUI (it takes focus on launch: `focus_main_window` calls `SetForegroundWindow` with `AttachThreadInput`).

**If the owner has not lifted the ban, stop and ask the owner before you run anything in sections 6.1 (input and cursor) or 6.2 (desktop UI).** You may still do the items that need no input: section 6.3 onward, except where an item says it needs a human or the GUI.

Two more things need the owner before work starts:

1. **A machine nobody is using**, if input is to be injected. The development box is the owner's own laptop: Windows 11 26200, RTX 3080 Ti Laptop with hybrid graphics, internal 1920×1080 panel at 360 Hz. Injected input there moves the owner's real cursor.
2. **A freed display-driver host.** See item D-1. Every display the app creates leaks about 47 MB, or more, in `WUDFHost.exe` until a driver restart or reboot. If commit headroom is low (`$c=(Get-Counter '\Memory\Committed Bytes','\Memory\Commit Limit').CounterSamples; ($c[1].CookedValue-$c[0].CookedValue)/1GB` under 3 GB), ask the owner to restart the driver or reboot before you measure.

Write down the owner's exact words lifting each rule, and keep them in your ledger.

---

## 1. What this job is

ScreenExtend is a Tauri 2 desktop app. A Rust host creates a virtual display per client, captures it, GPU-encodes it, and streams it over WebRTC (WHEP, H.264) to a dependency-free browser page.

- **Repository:** `C:\Users\sarve\Downloads\funnyscnew\ScreenExtend-workspace\main`.
- **Rules:** `CLAUDE.md` at the repository root describes the architecture and rules, and it binds you.

An earlier job, "overhaul-new", took the 442 commits between the old app `bef5f89` and `d4d8be9` (the head of `latency-overhaul`). It measured them, kept what helped, and consolidated the result on the branch `latency-overhaul-new`.

That job ran under four constraints:

- **No input.** Nothing could move a pointer or press a key.
- **No desktop UI.** The GUI was never started.
- **One machine.** Windows and NVIDIA only: no macOS, no AMD or Intel encoder, no phones, no real Wi-Fi.
- **A memory emergency.** The display driver leaked, so many confirmations were cut and recorded as untested.

Your job is the list in section 6: everything that job did not or could not test. Section 5 orders it.

- **The keep rule** applies to any change you make. A change is kept only if:
  - it fixes a defect, **or**
  - its median beats the current best's median by more than the larger of the two run-to-run ranges, across 3–5 interleaved runs each (A, B, A, B…), on p50, with no clearly worse p99 or painted fps, and nothing else made worse.

  If the ranges overlap, go to five runs each. If they still overlap, it is not a win. Revert everything you do not keep, and document it with its numbers.
- **Where to write it up:** one ledger per area under `docs/future-test/`, with the same sections the overhaul-new ledgers use. Those sections are the experiment table, defects found, fixed and still open, unverified, and questions for the owner.

---

## 2. The state of the code

- **Branch:** `latency-overhaul-new`, plus branch `post-reboot` on top of its `90f4840` (see `docs/overhaul-new/POST-REBOOT.md`): a test-handler clock fix and new requests (`0f15e67`, `2bbce85`, `8f817f8`), the GPU clock hold (`6c0bc71`) and the post-reboot documents. Measure the head of `post-reboot` if it has been merged, else `latency-overhaul-new`.
- **Head:** `cb0ab8d` (the consolidation merge), plus the commit that adds this file and `docs/overhaul-new/REPORT.md` on top of it. When this file was written the branch head was `5e20e52`, the merge of G03, before consolidation.
- **Relative to the old app `bef5f89` (v0.6.3):** the 442 overhaul commits from `latency-overhaul`, plus the commits below.
- **Relative to `latency-overhaul` (head `d4d8be9`):** 81 non-merge commits before consolidation. List them with `git log --oneline latency-overhaul..latency-overhaul-new --no-merges`.

What the 81 commits add, by area. The keep status of each is in `docs/overhaul-new/REPORT.md` section 3.

- **Test handler and isolation switch:**
  - `655e828`: `SCREENEXTEND_DATA_DIR` isolates a host.
  - `dcc4311` onward: `scripts/test-handler/`.
- **Capture (G01):**
  - `48d3807`: WGC retries a stale monitor handle before falling back to DXGI.
  - `f7c2a55`: `CURSORINFO.flags` tested as a bitfield.
  - `ea821a9`: WGC `MinUpdateInterval` at ⅔ of the stream's period.
- **Encode (G02):** `cf63279`, constant QP really applied on NVENC.
- **Transport (G03):**
  - `30d5719`: `PliBackoff`.
  - `396916b`: 10 s cap while the page paints.
  - `07ec295`: audio delayed-SACK allowance.
  - `1f85421`: close the superseded peer connection.
- **Session and display (G04):**
  - `f789b76`: a failed renegotiation keeps one display.
  - `97e46d1`: about 0.55 s off a fresh join.
  - `58fd8b1`: the refresh rate recorded at a join is a detection.
  - `8e709bd`: relayed renegotiation.
  - `26630dc`: the GPU toggle renegotiates in place.
  - `1eca751`: `driver_ipc` logs.
  - `7584269`: `attemptId` host half.
  - `183aea8`: D-08.
- **Client video (G05):**
  - `9aae1d3`: no demotion once painting.
  - `a00083d`: keyframes handed back to the receiver.
- **Audio (G06):**
  - `c020fc0` then `ef9dfe6`: outputs detected before the offer; the microphone card only where nothing routable lists, at most once per device; the `se:choose` entry answered at the next join.
  - `61da27a`, `24f75e8`, `736f0f9`, `201a981`, `83facec`: audio defect fixes.
  - `dbb2863`: process loopback first.
- **Client page (G07):**
  - `cd611ce`: warm-up at page load.
  - `2dda493`: same-tier retry.
  - `aec25fb`: `pagehide` releases the join.
  - `e8cbe87`: `attemptId`.
- **Telemetry (G08):** `e96fe2c`, `3120abf`, `114cc01`.
- **Host core (G09):** `304cf95`, `8e4db79`, `3032603`, `18311e0`, `8423abf`, `52c3eb3`.
- **Consolidation:** `fb6d08b` (a superseded session's teardown no longer removes the live session's telemetry), `9433732` (the four inline `session.stop()` calls in `server.rs` run on the blocking pool), `d5ea62a` + `29e75d9` (client audio path 4 named `worklet+port`), `d1b494c` and `7bf24ae` (`CLAUDE.md`, `CAPABILITY-TIERS.md` and 33 `OPTIMIZATIONS.md` entries brought in line with the code), `844c35f` (a stale comment), `5c01387` (`docs/overhaul-new/CONSOLIDATION.md`).

What stayed untouched:

- **Input files:** `git diff latency-overhaul latency-overhaul-new -- src-tauri/src/streamer/input src-tauri/src/streamer/static/input.js` is empty.
- **The desktop UI:** `src/` is byte-identical to `latency-overhaul`.
- **macOS, AMD and Intel code:** unchanged, except two macOS mirrors.
  - `58fd8b1`: `session::clear_join_seeded` in `macos_utils/mod.rs::set_device_override`, marked UNVERIFIED (not compiled).
  - `655e828`: the config store name in `macos_utils/networking.rs`.

**Where the reports and ledgers are:**

| file | what it is |
|---|---|
| `TEST-WORKFLOW.md` (repo root) | the previous job's brief |
| `docs/overhaul-new/REPORT.md` | the previous job's final report: verdict, kept and rejected changes, bug census, questions for the owner |
| `docs/overhaul-new/CONSOLIDATION.md` | the consolidation agent's merge log and the final three-way measurement |
| `docs/overhaul-new/BASELINE.md` | old app vs previous head, bug census, harness faults H-1…H-7 |
| `docs/overhaul-new/G01.md` … `G09.md` | one ledger per commit group |
| `docs/overhaul-new/G03-experiment-switches.patch` | G03's `SE_G03_*` experiment switches, a diff against `06e6fbb`, to be ported by hand |
| `docs/overhaul-new/INPUT-REPORT.md` | the input path, read and not run: hypotheses H1–H10, proposed changes P1–P11, the manual plan, the instrumented plan |
| `docs/commit-groups/COMMIT-GROUPS.md`, `commit-groups.json` | the 442 commits in 14 groups (leads, not facts) |
| `scripts/test-handler/README.md` | the measurement harness |
| `docs/OPTIMIZATIONS.md`, `docs/FINAL-REPORT.md`, `docs/CAPABILITY-TIERS.md`, `docs/TELEMETRY.md`, `docs/INPUT-PROTOCOL.md` | the older engineering record |
| `guides/*.md` | standalone directives for hardware the dev box lacks (macOS, AMD, legacy Windows, clients and glass-to-glass) |

**Binaries that existed at the end of the previous job.** None of these may be overwritten. A fresh build needs its own `CARGO_TARGET_DIR`.

- `C:\Users\sarve\.cargo\shared_target\ScreenExtend-old\release\ScreenExtend.exe`: the old app, `bef5f89`. It has no isolation switch.
- `C:\Users\sarve\.cargo\shared_target\ScreenExtend\release\ScreenExtend.exe`: `d4d8be9`. It has no isolation switch.
- `C:\Users\sarve\.cargo\shared_target\ScreenExtend-handler\release\ScreenExtend.exe`: `06e6fbb`, which is `d4d8be9` plus the isolation switch.
- Real-profile (fat LTO) builds made after the reboot: `90f4840` at `C:\Users\sarve\.cargo\shared_target\ScreenExtend-final\final\ScreenExtend.exe` (SHA-256 `89B2BEA7F38B64786E971620EA55DB69F616DB18A1FADEFC1B8502A38547ADEC`), and the `post-reboot` code with the GPU clock hold at `C:\Users\sarve\.cargo\shared_target\ScreenExtend-final\final-hold\ScreenExtend.exe` (SHA-256 starts `814A7FB52945D0A7`). The thin consolidation build `ScreenExtend-consol\final` is superseded.
- `C:\Users\sarve\.cargo\shared_target\ScreenExtend-x\xbin\ScreenExtend.exe`: the post-reboot experiment build (thin LTO; `SE_X_*` and `SE_G03_*` switches, readable from a switch file between renegotiations with the handler's `arms` request; the patch is not committed).
- **Absolute stamp numbers before 2026-10-06** carry an unknown per-run clock bias of up to ±16 ms whenever the machine ran at Windows' default 15.6 ms timer resolution (`0f15e67`, POST-REBOOT.md §0). Re-measure, do not reuse.
- `C:\Users\sarve\.cargo\shared_target\ScreenExtend-g08\bin-exp1`: G08's experiment binary, with the `SE_G08_*` switches.

**Building.**

- Run `cargo build --release --offline` from `src-tauri/`; online dependency resolution hangs on this machine.
- Put `libopus.dll` and `libx264-164.dll` from `src-tauri/resources/` beside the exe.
- `tauri_build` needs `dist/`: run `pnpm build` once.
- A cold fat-LTO release build takes 12–25 minutes. For A/B iteration you may build both arms with `CARGO_PROFILE_RELEASE_LTO=thin` and `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`. Never compare a thin build against a fat one.
- Prefer environment switches (`SE_*`), so one binary serves both arms.

---

## 3. Rules

**These still hold, whatever the owner lifts:**

1. **Never push.** Nothing on any remote may change. Commit only to your own branch, created from `latency-overhaul-new`. Never commit to `latency-overhaul`, `latency-overhaul-new` or `main`.
2. **The owner's machine and config.**
   - Kill only processes you started, by PID, never by image name (the owner's own Chrome runs).
   - Never leave `%APPDATA%\app.screenextend.desktop\config.json` changed. Record its SHA-256 before any exclusive-mode run and verify it after. Never restore older content over a newer one the owner's own app wrote.
   - Before you finish, confirm that no host, test browser, stage window or virtual display of yours is left.
   - If the owner's installed app (`%LOCALAPPDATA%\ScreenExtend\ScreenExtend.exe`) is running, do not start exclusive-mode runs or any test that attaches a display. Never touch that process.
3. **Probe capabilities, never version numbers** (`CLAUDE.md`).
4. **No change to how the desktop UI or the client page looks** unless the owner asks for it. Behaviour is in scope.
5. **Repo conventions:**
   - `tprintln!`, not `println!`;
   - `lock_recover()`, and no guard across `.await`;
   - secrets by environment variable, not argv;
   - the client page stays dependency-free and old-browser-safe (`node scripts/check-legacy-webkit.mjs`);
   - a new Tauri command goes in all three `*_utils` modules;
   - never hand-edit `src/lib/bindings.ts`.
6. **Report faithfully.** A test that could not run says so. A change not measured is not called a win. An unexplained result is written down as unexplained.
7. **Gates before you hand back** (no host running):
   - `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`, from `src-tauri/`;
   - `pnpm test:client`;
   - `node scripts/check-legacy-webkit.mjs` if the client page changed;
   - `pnpm lint` and `pnpm build` if `src/` changed;
   - `node scripts/check-bindings.mjs` if a command or event changed.
8. **Commit trailers** on every commit: `Co-Authored-By:` naming the model, and the session line the conversation gives you.

**These the owner must lift explicitly before the job starts, each one separately.** Record the words.

| rule | what lifting it allows | which items need it |
|---|---|---|
| **No input injection** (section 0) | `SendInput`, `SetCursorPos`, CDP `Input.*`, synthetic DOM events, `measure-input.ps1`, editing `src-tauri/src/streamer/input/` and `static/input.js` | 6.1 (I-1 to I-12), U-1, U-3, U-5 |
| **No desktop GUI** | starting the Tauri window, which takes focus, and driving its webview | 6.2 (U-1 to U-10) |
| **Never write a display the host did not create** (`CLAUDE.md`: "A display write names a monitor, never a `\\.\DISPLAYn`", `virtual_display::pick_owned_display`) | the host restoring the internal panel's own refresh rate after an attach | D-2, second option only |
| **No public relay session** | `serve` without `--no-cloud`, which registers a session with `session.screenextend.app` on the owner's behalf | N-5 |
| **Admin actions** | `pnputil /restart-device`, reading `HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Configuration`, a loss tool such as `clumsy`, `netsh` | D-1, D-2, N-4, N-7 |

If a rule is not lifted, skip every item that needs it and say so in your report.

---

## 4. The test handler

Every measurement in the previous job went through it.

```
node scripts/test-handler/handler.mjs --exe <path-to-ScreenExtend.exe> [--request <file.json | inline JSON>]
node scripts/test-handler/handler.mjs --status
node scripts/test-handler/handler.mjs --calibrate-only
```

`scripts/test-handler/README.md` is the full manual. What you need first follows.

### 4.1 What one run does

1. It starts that build's CLI host: `serve --no-cloud --no-qr -v` on the slot's ports.
2. It opens a fresh headless Chrome 154 with `--headless=new --ignore-certificate-errors --mute-audio --autoplay-policy=no-user-gesture-required --use-fake-device-for-media-stream`.
3. It joins, through DOM method calls only.
4. It runs the standard scenarios plus whatever the request asks for.
5. It tears everything down and prints one JSON document on stdout.

Exit codes: 0 means it ran, whatever the numbers; 1 means a harness failure (`errors[]`); 2 means bad arguments; 3 means the request was refused. Artefacts go under `%TEMP%\screenextend-test-handler\runs\<time>-slot<n>-<pid>\`: `serve.log` (timestamped), `handler.log`, `result.json`, and `netlog-*.json` with `netlog: true`.

### 4.2 The primary metric

A borderless, click-through, never-activated, topmost window (`stage.ps1`) draws the host's system clock as a stamp of 38 black and white cells at the top-left of the test's virtual display. A paint hook injected into the page reads the pixels the client actually painted, at its `drawImage` call (or `requestVideoFrameCallback` on `<video>`).

**latency = paint time − stamp − clock offset.** That covers host draw → DWM compose → capture → encode → send → receive → decode → paint call, for any build.

Each window reports:

- p50, p90, p99, max and min latency;
- painted fps, counted as distinct stamps per second;
- duplicate and skipped stamps;
- the longest gap;
- decode errors;
- machine load (CPU, and GPU through `nvidia-smi`);
- `R.memory`: commit headroom, free memory, and the largest `WUDFHost.exe` commit, handles and threads at both ends of the live-display part.

The calibration floor is p50 0.5–0.8 ms and p99 1.0–1.4 ms.

### 4.3 The noise figure

Two figures exist, and they disagree:

- **Loaded machine** (`scripts/test-handler/README.md`, acceptance 3): five consecutive standard runs at 82–90 % CPU. Small window p50 33.34 ms (range 1.48), large area 37.07 (range 2.31), hi-res 50.10 (range 7.90). p99 ranges reached tens of ms.
- **Quieter machine** (`docs/overhaul-new/BASELINE.md`): four runs spread over seven hours. Small window p50 range 7.6 ms (24.5–32.1), large area 4.5, hi-res at 60 Hz 7.9.
- **Final build:** none measured separately. In the final three-way (three rounds, `stamp_band: "center"`) the new build's p50 ranges were 2.52 ms (small window, n=4), 2.17 ms (large area, n=4) and 1.17 ms (hi-res 120 Hz, n=6). About 10 % of 1080p frames still failed the stamp checksum with `center`.

Measure your own noise figure first: five consecutive default runs with `{"stamp_band":"center"}`, and two with `{"stamp_band":"full"}` beside them. The previous job never did.

### 4.4 Caveats you must know

- **`hostToPaint` vs the stamp metric.** The page's own `hostToPaint` starts after the host's ring copy. The offset between the two depended on the scenario: 7.1–10.8 ms on the small window and 11.3–14.3 ms on the large area (`docs/overhaul-new/G08.md` §3). Compare either metric only within one scenario, never across scenarios through a fixed offset.
- **`stamp_band`.** The default `'full'` averages each cell over its height. About 10 % of 1080p frames fail the checksum because the compositor catches the stage's GDI blit half done (`BASELINE.md` H-1). `'center'` reads a 2-row band and is immune. Use the same band on both arms of an A/B, never `center` on one arm only.
- **The load gate.** Before every timed window the handler waits up to 180 s for whole-machine CPU under 75 %. A window whose own mean CPU ended above 75 % is marked `loaded: true`: discard and repeat it. Runs at 85–100 % CPU read 10–25 ms slower. A window taken with free physical memory under about 1.5 GB is suspect too.
- **Slots and exclusive mode.**
  - Four machine-wide slots (`%TEMP%\screenextend-test-handler\slot-{1..4}.json`). Slot *n* uses HTTP `18000+10n`, HTTPS `18001+10n`, Chrome debugging `19200+10n`, data directory `data-slot<n>`.
  - A measurement lock (`measure.lock`) covers the whole live-display part of each test, so only one test at a time ever has a display.
  - A build without the isolation switch (`bef5f89`, `d4d8be9`) runs in **exclusive mode**: it holds all four slots and uses the real data directory, with `config.json` backed up and restored byte for byte (verified by SHA-256 in `ownerConfig`). Its credentials go on argv (`bef5f89` has no other way), and it is stopped by PID.
- **`SCREENEXTEND_DATA_DIR` and shared-driver mode.** The switch moves the lock file, control socket, `config.json` and TLS cache into the slot's directory. On Windows it also puts the host in shared-driver mode:
  - the driver's monitor list is machine-wide, and a gap in the id run detaches every display;
  - isolated hosts therefore allocate dense ids under a lock in `%TEMP%\screenextend-shared-driver\`;
  - a released id in the middle of the run stays attached and idle until the ones above it go.

  That is how two displays were once attached during a "single-display" measurement (`G01.md` E1). Check `machine [..]` in `serve.log`.
- **Joins are a finite resource until the driver is fixed.** Every display creation or real mode change costs about 47 MB at 1080p and 94–124 MB for larger DPR ladders in `WUDFHost.exe`, never returned (`G04.md` §3). Read `R.memory` in every result. Prefer one join with `runs: N` over N joins.
- **Never kill a handler mid-run.** It leaves a virtual display, a slot file, `measure.lock` and an `owners.json` entry. If you must, clean up all four.
- **The handler refuses input today.** Any request mentioning `input`, `mouse`, `keyboard`, `pointer`, `touch` or `cursor`, or a `custom.js` that creates or dispatches events, is refused with exit 3. `lib/cdp.mjs` refuses every `Input.*` method. Lifting that is section 4.6.
- **Known handler faults still open.**
  - With two displays, the stage can pick the orphan display (`G07.md` D6).
  - The `plain_http` tier of `fallback_paths` / `audio_paths` cannot complete a join: `/whep` POST `net::ERR_ABORTED` after the HTTPS-upsell modal (`G06.md`).
  - `mover_stop` is a 48 px window, not a pointer. It cannot exercise `CursorProbe`.

### 4.5 Request keys

The full list is in the README. Those that exist today:

- `scenarios`: `static`, `small_window`, `large_area`, `hires_small_window`, `hires_large_area`
- `duration`, `static_duration`, `warmup`, `runs`
- `client`, `hires_client`
- `host_env`, `host_args`, `chrome_flags`, `headed`
- `calibrate`, `force_exclusive`
- `audio`, `audio_paths`, `click_source`, `mic_prompt`
- `settings_change`, `settings_only`
- `two_clients`, `leave_rejoin`, `slow_answer`, `auto_rejoin`
- `logs`, `pipe_close`, `fallback_paths`, `loss` (reports "not run")
- `netlog`, `stamp_band`, `mover_stop`, `cpu_burn`
- `url_params`, `join_probe`, `custom`
- `overlap_displays`, `load_gate`

### 4.6 Requests the future job must add (design sketches)

Each request below goes behind an explicit `--allow-input` command-line flag that refuses to run unless `%TEMP%\screenextend-test-handler\input-allowed.txt` exists. The owner creates that file, and it holds the owner's own sentence lifting the ban. `lib/cdp.mjs` gains an allow list of `Input.*` methods that is honoured only under that flag.

Before any of these, apply **P8** (`INPUT-REPORT.md` §6): diagnostics only. Add `delta_path=relative|tracked`, `flag=unadjusted` and `btn=down/up` to the `SE_INPUT_TRACE=1` line, add counters `tracked_deltas`, `relative_deltas` and `spurious_button_up` to telemetry, and log one line per session when the delta path flips. It costs nothing when the trace is off, and it turns H1, H2 and H4 from inference into a log line.

**`input_path`: the client → host leg and its correctness.**

- **Client.** An injected hook writes protocol frames straight onto the page's own `fast` and `reliable` DataChannels, using the real `RemoteInput` encoders. It scripts:
  - N × `MOUSE_DELTA(+1,0)`, `(−1,0)`, `(+3,0)` and `(−3,0)`, each with flags 0 and with flags `UNADJUSTED`, under pointer scope All and pointer scope Display;
  - a 1 kHz burst of 1000;
  - click-while-moving sequences: delta (buttons 0), Down, delta (buttons 0), delta (buttons 1), Up;
  - the same with absolute `POINTER_MOVE` and batches.
- **Host.** Runs with `SE_INPUT_TRACE=1` (with P8), and samples `GetCursorPos` (a read) after each step. Turn Windows pointer acceleration off for the test with `SystemParametersInfo(SPI_SETMOUSE)` and restore it after.
- **Report:**
  - displacement error per direction and path, in px;
  - motion lost per burst;
  - spurious button transitions;
  - forward latency p50 and p99, client send stamp against the host trace wall clock;
  - `input_queue_drops`, `input_stale_drops`, `input_reorder_drops`, `input_parse_errors`.
- **Pass:**
  - 0 px error over 1000 single-pixel steps in each direction, on both paths;
  - 1000/1000 of a 1 kHz burst injected;
  - zero spurious button transitions;
  - forward p50 ≤ 2 ms and p99 ≤ 5 ms on the same machine; ≤ RTT + 5 ms on a LAN.
- **Automatable:** yes.

**`pointer_lock`: the page's own lock state machine.**

- Headless Chrome cannot hold a real pointer lock. Run headed Chrome on a dedicated desktop, and grant the lock with one real `Input.dispatchMouseEvent` click.
- Then trigger, in turn:
  - a reconfigure (epoch bump through `devices set`);
  - the `<video>` fallback (`url_params` switch, or `fallback_paths` style);
  - an output-picker re-offer (command a sink the engine refuses);
  - Esc.
- After each one, read:
  - `document.pointerLockElement`;
  - the next `MOUSE_DELTA`'s flags byte (hook `RTCDataChannel.send`);
  - whether `POINTERLOCK_STATE` was sent on the new channel.
- **Pass:**
  - the flags byte equals the granted state on the first delta of every connection;
  - `POINTERLOCK_STATE` is restated within one RTT of every `reliable.onopen`;
  - the first click after a rebuild does not move the cursor.
- **Automatable:** yes, headed.

**`cursor_capture`: motion-to-photon for the real cursor.**

- **Host.** Moves the cursor with `SetCursorPos` along scripted paths on the test's virtual display: sweep, isolated moves, stop-and-hold. Run at 60, 125 and 1000 Hz, stamping each move's host time.
- **Client.** The existing paint hook crops the painted frame and finds the cursor sprite by template match. Install a high-contrast custom cursor for the test and restore the user's scheme afterwards. Record position over time.
- **Report:**
  - cursor motion-to-photon p50 and p99;
  - cursor painted fps during a sweep;
  - frames between the stop and the final position being painted;
  - any frame with two cursors (headed, unlocked) or none while the cursor is on the display;
  - `frames_skipped_cadence`, `keyframe_requests` and `client_keyframe_requests` from `status --json` before and after.
- **Runs:** with the default and with `SE_CURSOR_OVERLAY=1`; over a static background and over the `large_area` noise; at a 60 Hz and a 120 Hz client.
- **Pass:**
  - painted fps during a sweep ≥ min(client refresh, 60);
  - motion-to-photon p50 within one frame period of the moving-window stand-in's p50 in the same run;
  - the final position painted within one client frame period of the stop;
  - zero double or missing cursors.
- **Automatable:** yes.

**`input_load`.**

- Repeat `input_path` while `large_area` runs, at `SE_TRANSPORT_WORKERS` 4 and 16.
- **Pass:** no change in forward p99 beyond the noise figure.

**`gui`: drive the desktop app.**

- **Start.** The built exe with no arguments and `SCREENEXTEND_DATA_DIR` set to a slot directory, so it never touches the owner's `config.json`. Enable the WebView2 debugging port with `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<slot port + 5>`, and attach CDP to the Tauri webview.
- **Actions.** Through `Runtime.evaluate` calls to `window.__TAURI__` / `@tauri-apps/api` `invoke` (the same commands the UI calls), or through DOM `.click()` on the UI's own controls.
- **Reads:**
  - the dashboard row count (`[data-device-row]` or the device list's DOM);
  - the log terminal's rows;
  - `ScreenExtend status --json` with the same `SCREENEXTEND_DATA_DIR`.
- **Focus.** The window steals focus on launch. Run it only on a machine nobody is using, or start it on a second desktop and never let it take keyboard focus from the owner.
- **Automatable:** yes, once the GUI rule is lifted.

---

## 5. Ordering

Most likely to matter to a user first, cheapest to check first.

**Cheapest:**

- Items needing only the owner's eyes and five minutes: D-2 (panel 360 Hz) and the manual input check (I-2…I-7 by hand, `INPUT-REPORT.md` §7 T1–T6).
- Items that are one handler run on an existing or rebuilt build: M-1…M-8.

**Most likely to matter:**

1. cursor and input feel (I-1 to I-7);
2. "two devices" through the GUI and over the relay (U-3, N-5);
3. sound over plain HTTP and on phones (A-1, A-2, C-1);
4. constant QP on real Wi-Fi (N-1);
5. the driver leak (D-1);
6. the log panel (U-1).

**Suggested sequence:**

| step | items | needs |
|---|---|---|
| 0 | D-1 (driver restart), then measure your own noise figure (section 4.3) | owner (admin) |
| 1 | D-2 (panel set once), D-3 two-display cost | owner's eyes; handler |
| 2 | I-12 (P8 diagnostics), then the owner's ten-minute manual check (`INPUT-REPORT.md` §7 T1–T6) | owner by hand; input ban lifted for the trace build only |
| 3 | I-2, I-3, I-4, I-5 with `input_path`; apply P1, P2, P4, P3/P5 one at a time | input ban lifted, idle machine |
| 4 | I-1 and I-6/I-7 with `cursor_capture` and `pointer_lock` | input ban lifted |
| 5 | M-1 … M-8 (handler re-measures still owed) | handler only |
| 6 | U-1 … U-10 (GUI) | GUI rule lifted |
| 7 | A-1 … A-6, C-1 … C-5 | a second device, real browsers |
| 8 | N-1 … N-7 | a real AP, a loss tool, relay permission |
| 9 | P-1 … P-5 (other platforms) | the hardware named in each item |

---

## 6. The items

Each item gives six fields:

1. **What** the defect or risk is, and who reported or suspected it.
2. **Why untested**: why the previous job could not test it.
3. **Suspects**: the commits and files most likely involved.
4. **Known**: what was learned without testing.
5. **How to test**: equipment, permissions, tool, measure, pass, and whether a handler or a human does it.
6. **Then**: what to do with the result.

Ledger paths are relative to the repository root.

### 6.1 Input and cursor

**These need the input ban lifted.**

#### I-1 Cursor glitches on the capture side: stutter, lag, stopping short, doubled or missing pointer

- **What.** The composited pointer stutters, lags, jumps, vanishes or is drawn twice; this is the owner's complaint, "cursor glitches". Two capture-side mechanisms were found by reading (`docs/overhaul-new/G01.md` §6, `INPUT-REPORT.md` H9):
  - **(a) Stopping short.** `CursorProbe::changed()` compares `GetCursorInfo` with the previous probe, not with the last delivered frame. The frame carrying the pointer's final position can read "unchanged" and be dropped by the ¾-period cadence gate. On a still desktop the pointer then sits one step short until the 200 ms keepalive, and that keepalive re-encodes the held, stale slot.
  - **(b) Too many frames.** Cursor-moved frames are exempt from the cadence gate (`6faa359`), so a moving pointer drives the stream at the compositor's rate: 113 fps measured on a 60 Hz client with the old 4 ms WGC floor. A weak client decoder then drops to the freshest frame and asks for keyframes while the mouse moves.
- **Why untested.** No pointer could be moved. The stand-in (`mover_stop`, a 48 px window) is a dirty region and never sets `cursor_moved`.
- **Suspects:**
  - `b0183dd`, which added the cadence gate and `CursorProbe`;
  - `d848234` and `0fa6f54`, the host-drawn overlay, now behind `SE_CURSOR_OVERLAY=1`;
  - `a871ec7`, which gave the pointer back to WGC;
  - `cabf63a`, the ⅔ window;
  - `6faa359`, the exemption;
  - `ea821a9`, the WGC ⅔-period floor, which now bounds exempt frames at 1.5× the stream rate (about 90 fps at 60 Hz);
  - `f7c2a55`, the `CURSOR_SHOWING` bit test;
  - `408099e` and `b0183dd` for the GPU scheduling class (REALTIME, then ABOVE_NORMAL; the owner reported a flickering pointer at REALTIME).
  - File: `src-tauri/src/windows_utils/streamer/pipeline.rs` (`on_frame_arrived`, `CursorProbe`, `cursor_overlay.rs`).
- **Known:**
  - The stand-in was never stale at rest: 0 of 11 stops on either build, final position shown 11–35 ms after the stop (`BASELINE.md`).
  - Painted fps for a pointer sweep was measured in September: 59 with the overlay, 85 on the old app, 113 with the exemption (`docs/OPTIMIZATIONS.md` UX-INPUT).
  - `ea821a9` was measured only for a moving window with two displays (`G01.md` E5).
  - **Doubled cursor:** in unlocked (windowed) mode the browser's own cursor floats over the stream, and the composited one trails it by the full latency. This is pre-existing (`INPUT-REPORT.md` §4.5).
  - **Missing cursor:** scope All lets a locked pointer leave the display (I-6), and Windows hides the pointer after touch input or while typing.
- **How to test:**
  - Add `cursor_capture` (section 4.6). Equipment: an idle Windows + NVIDIA machine.
  - Run on `latency-overhaul-new` and on `ea821a9^`, interleaved, 3 runs each, at 60 Hz and 120 Hz clients, over a static background and over `large_area`, with and without `SE_CURSOR_OVERLAY=1`.
  - Read `frames_skipped_cadence`, `keyframe_requests` and `client_keyframe_requests` from `status --json`.
  - Pass criteria are in section 4.6.
  - Add one human check: the owner sweeps the pointer quickly over a still virtual display, stops dead, and compares where the client draws it with the host. Repeat with GPU scheduling class 2 (`SE_GPU_SCHED_CLASS=2`) to judge flicker by eye.
  - Automatable except the flicker judgement.
- **Then:**
  - If the pointer stops short, try a "settle frame": let the first unchanged frame after a run of cursor-moved frames through the gate once (`G01.md` §7 item 3).
  - If painted fps is too high for a weak client, cap the exemption at exactly one client period, never below it (`6faa359` measured sub-period windows as worse; `INPUT-REPORT.md` P11).
  - If flicker follows class 3, go to class 2 (the old app never set one).
  - Keep only by the keep rule, 3–5 interleaved runs.

#### I-2 Pointer gain changes: about 1 s into every connection, and for the rest of the session after any rebuild (H1)

- **What.** Locked motion takes the "tracked absolute" path, which applies no Windows pointer curve (a 2–6× gain cut, by `420d76f`'s own estimate), far more often than intended (`INPUT-REPORT.md` §4.3). It happens:
  1. for the first ≈1 s + RTT of every connection, because the flags byte is gated on capability bits from the first PONG, and the first PING goes out only after `setInterval(…,1000)`;
  2. for the rest of the session after any rebuild while the lock is held, because `teardown()` clears `lockIsUnadjusted` and pointer lock survives on `#stage`, so no `pointerlockchange` re-sets it;
  3. on browsers without `unadjustedMovement` (Safari before 18.4, Linux Chrome, Firefox before 152);
  4. under scope Display;
  5. after a plain-lock retry.

  Cases 1 and 2 are defects. This is the owner's "slower" and "cursor glitches".
- **Why untested.** Input ban.
- **Suspects:**
  - `664d26e` (the tracked path, `mouse_relative` starts false);
  - `d03efe5` (flags gated on capabilities);
  - `8e5d3c8` (PONG capability bits);
  - `803e88f` (`teardown()` clears the claim);
  - `420d76f` (`lets_os_curve`).
  - Files: `src-tauri/src/streamer/static/input.js` (`teardown`, `reliable.onopen`, `wantEvFlags`, `evFlags`), `src-tauri/src/streamer/input/windows.rs` (`lets_os_curve`, `mouse_delta`).
- **Known:**
  - Pure reading. The old app accelerated twice (client OS + Windows), so even a correct single curve feels slower than the old app (`INPUT-REPORT.md` §2, the timeline table).
  - `9aae1d3` removes one rebuild source, the false `<video>` demotion.
- **How to test:**
  - **Human, no injection:** `INPUT-REPORT.md` §7 T1. The owner joins with a second laptop and a physical mouse, fullscreen ticked, host started with `SE_INPUT_TRACE=1`.
    - Sweep a fixed 10 cm within the first second, again after 5 s, and again after a video-quality edit.
    - Change the host's Windows pointer speed from 10 to 18: if the remote cursor's speed changes, the session is on the relative path.
  - **Automated:** `pointer_lock` (flags byte on the first delta of every connection), plus `input_path` with P8 logging `delta_path`.
  - Pass:
    - the same path (`relative` for Chrome with `unadjustedMovement` and scope All) from the first delta of every connection and after every rebuild;
    - the same hand distance → the same screen distance.
- **Then:**
  - Apply **P1** (`INPUT-REPORT.md` §6): `teardown()` clears `lockIsUnadjusted` only when `!document.pointerLockElement`, and `reliable.onopen` sends `FOCUS_STATE`, `POINTERLOCK_STATE` and an immediate PING.
  - Optionally P1b: the host keeps `unadjusted` sticky per device.
  - Update `test-client-input.mjs` and the golden vectors.
  - Keep if `pointer_lock` passes and `input_path` shows no regression.

#### I-3 One-pixel drift on the tracked path (H2, H2b)

- **What.** `advance_rel` reads `GetCursorPos` as the base for every delta and normalises with `round(u/W*65535)`. If Windows maps an absolute coordinate back to a pixel by truncation, then across 82–87 % of the screen (every `u` past the midpoint):
  - +1 px moves right or down land where they started;
  - −1 px moves overshoot by one;
  - any delta k moves k−1 one way and k+1 the other.

  Simulated (`INPUT-REPORT.md` §4.2): 100 × (+1) moved 0 px, and 100 × (−1) moved −188.

  **H2b:** if `GetCursorPos` lags `SendInput`, back-to-back deltas read the same stale base and motion is lost.

  This is the owner's "cursor glitches", by reading.
- **Why untested.** Input ban. It is also UNVERIFIED whether this Windows build truncates or rounds.
- **Suspects:** `301230d` (base = `GetCursorPos` per delta), on top of `664d26e`. File: `src-tauri/src/streamer/input/windows.rs` lines around `advance_rel` and `mouse_delta` (381–453 at `80e00a4`), and `abs()`, which has the same one-pixel shortfall but does not feed it back.
- **Known:** the arithmetic is exact and covers 1920, 2560, 1080, 1440 and two 4480-wide virtual desktops. Under round-to-nearest there is no error at all.
- **How to test:**
  - **Human:** `INPUT-REPORT.md` §7 T2. Tick "Restrict cursor to this display" (forces the tracked path), and in Paint at 800 % creep slowly right, then left, then draw small circles. Asymmetry means truncation.
  - **Automated:** `input_path` with N × (±1) and (±3) under scope Display, reading `GetCursorPos` after each step. That also settles truncation vs rounding.
  - Pass: 0 px error over 1000 steps each way.
- **Then:**
  - Apply **P2** (`INPUT-REPORT.md` §6):
    - `norm(px) = ceil(u·65536/W)`, exact under both models (verified for W ∈ {1080, 1440, 1920, 2560, 4480, 7680});
    - a tracked target that resynchronises with `GetCursorPos` only when someone else moved the cursor.
  - Add a pure test of `norm` against both models.
  - Keep if `input_path` reads 0 px.

#### I-4 One click becomes two: spurious button transitions (H4)

- **What.** Positional frames (moves, batches, deltas) carry a button word, and `button_transitions(old, new)` turns a stale word into an up and then a down. There are three routes:
  - **(a)** unlocked mouse batches (`pendingStrokes`) are not flushed or cleared at `pointerdown`, so a stale batch with buttons = 0 arrives after the reliable `Down`;
  - **(b)** a pending locked delta with buttons = 0 is flushed after `Down`; `39f00cb`'s rewrite of a late delta's word to the last delta's word synthesises the up itself;
  - **(c)** crossbeam `select!` picks randomly between the reliable and fast queues.

  The symptoms are double-clicks, drags that drop, and text selection that jumps. This is the owner's "input glitches", by reading.
- **Why untested.** Input ban.
- **Suspects:**
  - `d5cfe79` (unlocked mouse batching);
  - `7ce9d34` (batch injected as one `SendInput`);
  - `39f00cb` (late-delta word rewrite);
  - pre-existing button authority.
  - Files: `input.js` `onPointerDown` / `endPointer` / flush order; `streamer/input/mod.rs` (lines around 540–545, `last_delta_buttons`); `windows.rs` `button_transitions` callers.
- **Known:** reading only. The old app had (b) and (c) but not (a).
- **How to test:**
  - **Human:** `INPUT-REPORT.md` §7 T4. Click 20 times while moving, locked and then unlocked with a 1000 Hz mouse, in a local page that logs `mousedown`, `mouseup` and `dblclick`.
  - **Automated:** `input_path` click-while-moving sequences.
  - Pass: zero spurious transitions.
- **Then:**
  - Apply **P4**. Only Down, Up, Cancel and `release_all` change `self.buttons`; delete the late-delta rewrite. The client flushes `rel` and `pendingStrokes` with the pre-transition word before sending Down or Up.
  - The relay's old page is unaffected: it still sends Down and Up.

#### I-5 Lost motion: 8-sample truncation (H3), age and overflow drops of deltas (H10)

- **What.**
  - **H3.** Under lock, `onPointerMove` sums only the newest 8 coalesced samples, so a 1000 Hz mouse at 60 Hz loses about 50 % of its motion, if `getCoalescedEvents` is exposed on the page. `clampI16(v|0)` also discards fractional deltas.
  - **H10.** `MouseDelta` is "supersedable": a delta queued more than 50 ms is dropped, and `FAST_CAP` 64 drops the oldest. Motion is lost for good whenever the injector stalls (clipboard or file-drop work on the injector thread, a low-level mouse hook).
- **Why untested.** Input ban. Also unverified: whether Chromium exposes `getCoalescedEvents()` on non-secure `http://` pages, and the LAN QR code is `http://`.
- **Suspects:** `d5cfe79` (cap at 8, coalesced sum), `2becec0`, `35986dd` (age gate, `FAST_CAP` 512 → 64), `da2c2ec`. Files: `input.js` lines around 668–725; `streamer/input/mod.rs` lines around 382–393, 444–455 and 521.
- **Known:** reading only.
- **How to test:**
  - **Human:** `INPUT-REPORT.md` §7 T3. Sweep 10 cm over `http://` vs `https://…:8443` at 125 Hz and 1000 Hz polling.
  - **Automated:** `input_path` 1 kHz burst; `input_load`.
  - Pass:
    - the same distance on both schemes and both polling rates;
    - 1000/1000 of a burst;
    - `input_stale_drops` 0 for deltas.
- **Then:**
  - **P3:** use the dispatched event's `movementX/Y`, and carry the fractional remainder.
  - **P5:** never drop a delta; fold old deltas into the next one, and on overflow drop the oldest absolute move or merge deltas.
  - **P7:** move clipboard and drop work off the injector thread.
  - One change at a time, keep rule.

#### I-6 The pointer leaves the virtual display or escapes at the edge (H5)

- **What.**
  - With pointer scope All (the default since `d42f0df`) and the relative path, a locked pointer goes onto the host's own monitors and vanishes from the stream; clicks land on the host desktop.
  - On the absolute path, `pixel()` maps `nx = 1.0` to one column past the display, so an edge tap can land on the neighbouring monitor (pre-existing).

  This may be the owner's "cursor being defocussed".
- **Why untested.** Input ban.
- **Suspects:** `f957022` (scope, default Display), `d42f0df` (default All), `pixel()` in `windows.rs`.
- **Known:** reading only. Scope All is the owner's own ruling (`INPUT-REPORT.md` §10 Q6).
- **How to test:**
  - **Human:** `INPUT-REPORT.md` §7 T6. Locked and scope All, push past each edge. Then unlocked, click at the extreme right and bottom edge of the picture.
  - **Automated:** `input_path` with absolute moves at `nx = 1.0` and `ny = 1.0`, reading `GetCursorPos`.
  - Pass: the absolute edge lands on the display's last column and row.
- **Then:**
  - Apply **P6** (map into `width − 1`).
  - Ask the owner whether scope All stays the default.

#### I-7 Focus and lock lost, the first click after a rebuild lands at the lock point, input dead across a rebuild (H6, H7)

- **What:**
  - **(a)** After any rebuild, the new `Injector` starts with `mouse_relative: false` and the client never restates `POINTERLOCK_STATE`. A Down that arrives before the first delta is treated as unlocked, and the cursor teleports to the frozen lock point.
  - **(b)** Any modal over a live stage drops pointer lock, keyboard lock and fullscreen (`44f3f6d`'s `setSuspended`). `83facec` (G06) stopped the output-picker re-offer that raised one unasked.
  - **(c)** Alt-Tab, blur and Esc drop the lock; re-locking takes a click, which is itself forwarded.
  - **H7.** Everything sent between `teardown()` and the new channels opening is dropped, and a drag in progress is broken by `release_all`.

  This is the owner's "cursor or stage being defocused".
- **Why untested.** Input ban; headless Chrome cannot hold a pointer lock.
- **Suspects:** `664d26e`, `803e88f`, `44f3f6d`, `a38ad21` (modal and fullscreen timers); `index.html` `beginModal` / `endModal`, `teardownSession`, `fallbackToVideo`, `queueOutputPicker`; `input.js` `setup` / `onopen`.
- **Known:**
  - Every rebuild path (`reconfigure`, `fallbackToVideo`, `retryWithLegacyTransform`, `trickleFailed`) runs `teardown()`.
  - `9aae1d3` stops the false demotion (2 of 56 runs on the previous head).
  - `2dda493` stops a lost answer demoting to `<video>`.
  - On the CLI path, every stream-class edit is exactly one renegotiation (`G04.md` §4).
- **How to test:**
  - **Human:** `INPUT-REPORT.md` §7 T5 (change the refresh rate, do not move, click once), T7 (note every self-dropped lock and what preceded it; `?debug` in the client URL), and T8.
  - **Automated:** `pointer_lock`. Pass criteria are in section 4.6.
- **Then:**
  - Apply **P1** (restate lock and focus on `reliable.onopen`).
  - Ask the owner about re-locking on Esc.
  - Keep rule.

#### I-8 Keyboard, wheel, touch and pen, clipboard

- **What:**
  - Keys sit behind head-of-line blocking on `reliable` under loss, so key-ups arrive late while auto-repeat runs.
  - Keys sent during a rebuild gap are lost.
  - Stuck modifiers after Alt-Tab.
  - Wheel notches are lost on the unreliable `fast` channel.
  - Reordered touch and pen samples are injected out of order (zig-zag ink).
  - Ctrl/Cmd+V pastes the **host's** clipboard: `preventDefault()` on keydown cancels the page's `paste` event (pre-existing).
- **Why untested.** Input ban.
- **Suspects:** `72c4b11`, `da2c2ec`, `4280b13`, `39f00cb`, `ea0aa36`; `input.js` `sendKey`; `windows.rs` `key()`, `release_all`.
- **Known:** reading only (`INPUT-REPORT.md` §3.2–3.5).
- **How to test:**
  - **Human:** `INPUT-REPORT.md` §7 T9 (type fast for 30 s, hold a key 3 s, modifiers, Alt-Tab, Ctrl+V), T10 (wheel and trackpad), T14 (lossy 2.4 GHz Wi-Fi with a busy scene).
  - **Pass:**
    - no missing or repeated characters;
    - no stuck modifiers;
    - Ctrl+V behaviour as the owner decides (`INPUT-REPORT.md` §10 Q10).
  - **Automated:** an `input_path` extension that writes key frames on `reliable` and reads them back through a host-side keyboard hook in a test window. This needs the ban lifted.
- **Then:**
  - Fix clipboard ordering through P7 if paste breaks.
  - Decide the client-clipboard question with the owner.

#### I-9 Input lag: transport starvation, motion-to-photon and click-to-photon, 1 kHz mouse (H8)

- **What.** Lag spikes on keys and clicks. Possible causes:
  - transport runtime width (`932e0bc` narrowed it to 4 workers and lost 221/1000 frames of a 1 kHz burst; `3791155` restored 16);
  - webrtc-sctp's 200 ms delayed SACK for lone packets;
  - Wi-Fi airtime shared with IDR bursts.
- **Why untested.** Input ban; no real AP.
- **Suspects:** `932e0bc`, `3791155`; `streamer/mod.rs` (`SE_TRANSPORT_WORKERS`); `route()` stamps `host_recv_ns` *after* the scheduler wait, so a starved runtime is invisible in every host stage.
- **Known:** the keyframe storm that drove IDR bursts is cut about 6× (`30d5719`, `396916b`, `a00083d`).
- **How to test:**
  - `input_load` at `SE_TRANSPORT_WORKERS` 4, 8 and 16.
  - Click-to-photon with a 240 fps camera filming the client screen and the mouse button (`guides/clients-network-and-glass-to-glass.md`). This needs a human and a camera.
  - Pass: forward p99 at 16 within the noise of an idle run; click-to-photon reported with its distribution.
- **Then:** if 16 still starves, consider a dedicated input-receive thread. Measure before keeping.

#### I-10 The injector addresses its target by `\\.\DISPLAYn` name (P9)

- **What.** `refresh_geometry` re-resolves the target every 500 ms by the cached GDI slot name. If the name is reused or fails to resolve, absolute input maps across the whole virtual desktop, so client taps can land on the host's real monitors. After a mode change the rect is stale for up to 500 ms. `CLAUDE.md` forbids this addressing pattern for display writes. Raised in `INPUT-REPORT.md` §5.10 and `G04.md` §4.
- **Why untested.** Input code is frozen; injection is banned.
- **Suspects:** `streamer/input/windows.rs` `refresh_geometry`; `InputParams` in `streamer/webrtc_session.rs`.
- **Known:** `LiveDisplay.device_path` (the stable identity) exists on the session side, but `InputParams` has no field for it.
- **How to test:**
  - Add `device_path` to `InputParams`, resolve it through `virtual_display::resolve_owned_display`, and refuse to inject when it does not resolve.
  - Test with `input_path` absolute moves during a `settings_change` mode change, and with two clients joining and leaving so that slot names are reused.
  - Pass: no injected coordinate outside the session's own display.
- **Then:** keep as a defect fix if the test passes.

#### I-11 Input-path log flood

- **What.** `send_inputs` and `inject_pointer` log every failed `SendInput` with no rate limit (UAC secure desktop, lock screen, UIPI). At pointer rates that formats a line per event on the input path and evicts the 2000-line log ring within seconds. That is also a "logs do not come through" cause (`G09.md`).
- **Why untested.** Input code is off limits.
- **Suspects:** `d3ce813`; `streamer/input/windows.rs`.
- **Known:** reading only.
- **How to test:**
  - With input allowed, move the client pointer while the host shows a UAC prompt or the lock screen.
  - Read `ScreenExtend logs` and `[logbus] dropped` counts.
  - Pass: at most one line per second of failures, with a count.
- **Then:** rate-limit with a counter.

#### I-12 Make the delta path visible first (P8)

- **What.** `SE_INPUT_TRACE` cannot tell the tracked path from the relative one, nor show button transitions. Without that, I-2 to I-4 stay inference (`INPUT-REPORT.md` §5.13, P8).
- **Why untested.** Input code is frozen.
- **Suspects:** `a6331c9` (the trace); `streamer/input/mod.rs`, `windows.rs`.
- **Known:** it costs nothing when the trace is off.
- **How to test:**
  - Implement P8.
  - Run the owner's manual T1–T6 with the trace on.
  - Pass: every line names its path and flag.
- **Then:** use it for I-2 to I-4.

### 6.2 Desktop UI

**These need the GUI rule lifted, and some need input.**

#### U-1 The log panel misses lines: listen after backlog (race 2)

- **What.** On mount the Settings log panel calls `get_log_backlog` (which arms streaming) and then registers `events.logLine.listen` one IPC round trip later. Any batch emitted in that gap reaches no listener. This is the owner's "logs do not come through" (`G09.md`, race 2).
- **Why untested.** The GUI was never started.
- **Suspects:** `src/components/log-terminal.tsx`; `src-tauri/src/logbus.rs`; `c3780ca`, `d150c1d`. Race 1 (host side) was fixed in `304cf95`.
- **Known:**
  - Line rates are 0.7–12.5 lines/s on average, with peaks of 30–62/s at a join (`G09.md` E1).
  - The ring holds 2000 lines.
  - With race 1's old shape, a race test failed 20/20.
- **How to test:**
  - `gui` request: open Settings → Logs during a join and compare the panel's rows with `ScreenExtend logs` (same `SCREENEXTEND_DATA_DIR`).
  - Pass: every line in the backlog after the panel opened is in the panel.
  - Also a human check.
- **Then:**
  - Listen first and buffer, then read the backlog and drop the overlap.
  - That needs a per-line sequence number in `get_log_backlog` / `LogLine`, a bindings change: run `pnpm tauri dev`, diff `src/lib/bindings.ts`, then `node scripts/check-bindings.mjs`.

#### U-2 The GUI never restores a device's saved audio output

- **What.** `src/pages/bootstrap.tsx` replays `setDeviceOverride(…, systemAudio)` for each saved device but never calls `setDeviceAudioOutput`; `serve` does (`apply_saved_devices`). After a GUI restart, a device plays on its default output until the sheet is saved again (`G06.md`, `G09.md` item 3).
- **Why untested.** GUI out of scope.
- **Suspects:** `src/pages/bootstrap.tsx`.
- **Known:** the fix is one call beside the existing one: `commands.setDeviceAudioOutput(device.token, device.audioOutputDeviceId ?? "")`.
- **How to test:**
  - Human or `gui`: pick a non-default output for a device in the sheet, save, restart the app, and join that device.
  - Read the host log or `status --json` for the sink in use.
  - Pass: the chosen output.
- **Then:** add the call and re-run.

#### U-3 The device sheet emits its own `deviceModify`: two rows after a save

- **What.** `device-details.tsx` emits `deviceModify` with the row's `joinId`. `App.tsx` *appends* a row when no row has that join id. If the host's renegotiation came back as a new join, or the old join was removed first, the dashboard shows two devices, or resurrects a removed one. This is part of the owner's "two devices with two virtual screens" (`G04.md` §1).
- **Why untested.** GUI out of scope.
- **Suspects:** `src/components/pages/device-details.tsx`, `src/App.tsx`. Host fixes for the other routes: `f789b76`, `8e709bd`, `26630dc`, `7584269`.
- **Known:**
  - Through the CLI, every class is one join and one display before and after.
  - The two-display route needs a failed renegotiation; it was reproduced with an injected failure.
- **How to test:**
  - `G04.md` §1's UI-job reproduction: open the app, join one tab over the LAN URL, open the device sheet from the Devices page, change the refresh rate and save. Watch the dashboard row count and `ScreenExtend status --json` `telemetry.joins`.
  - Repeat over the cloud URL (relay permission needed).
  - Repeat with a host built with a temporary `SE_G04_FAIL_RENEGOTIATION=1` switch. That switch is not committed; re-create it so it fails the first continuing join once.
  - Pass: one row, one display, the same `joinId` before and after. `latency-overhaul` is expected to show two in the injected case.
  - `gui` request plus `status --json`, or a human.
- **Then:** stop the sheet emitting `deviceModify` (the host already sends `DeviceModify` for an in-place renegotiation), and re-run.

#### U-4 Field fallbacks differ between the UI and the host

- **What.** For a saved device row missing a field, `apply-config.ts` / the TS schema use `videoQuality` 23 and `remoteControl` false. The host (`parse_saved_devices`, `join_defaults`, `Device::defaults`) uses 15 and true. A hand-edited row therefore runs differently under the GUI and `serve`, and a config-editor apply after `RELOAD` costs one renegotiation (`G04.md` §1 item 3, `G09.md` item 4).
- **Why untested.** GUI out of scope; it is also a question for the owner.
- **Suspects:** `src/lib/apply-config.ts`, `src/lib/config-schema.ts`, `src-tauri/src/cli.rs`.
- **Known:** only hand-edited rows lack these fields.
- **How to test:**
  - Unit level: `pnpm test:config` plus a new case.
  - Live: write a row without the fields into a slot's `config.json`, start the GUI under that `SCREENEXTEND_DATA_DIR`, join, and read the encoder line in the log.
- **Then:** align to the owner's answer (`REPORT.md` question 22).

#### U-5 Server port changes restart every streamer on the UI thread; the GPU toggle button never pressed

- **What.** `set_server_ports` runs `restart_streamers` on the main thread. That means up to about 2.5 s of frozen window, and every session dropped (unavoidable for a re-bind, but it should not freeze the UI). `set_disable_gpu_encode` now renegotiates in place (`26630dc`), but the GUI button itself was never pressed (`G09.md` item 6, `G04.md` fix 6).
- **Why untested.** GUI out of scope.
- **Suspects:** `src-tauri/src/windows_utils/mod.rs` (`set_server_ports`, `set_disable_gpu_encode`), `26630dc`.
- **Known:** through `RELOAD`, the GPU toggle renegotiated in place: same join id, x264 then NVENC, no display created, one run.
- **How to test:**
  - `gui`: toggle "disable GPU encode" with one client joined. Pass: the same `joinId` and display, no `DeviceRemove`, the window responsive.
  - Change the HTTP port with one client joined. Measure window responsiveness (a `requestAnimationFrame` probe in the webview) and the session outcome.
- **Then:** move `restart_streamers` to a background thread.

#### U-6 GUI start-up ordering

- **What.** The GUI starts its servers (`watchForNetworkChanges`) before `setSessionCredentials`, so an OTP join is refused for that moment. The disconnect grace is pushed by a separate effect that can land after the servers are up (`G09.md` item 2).
- **Why untested.** GUI out of scope.
- **Suspects:** `src/pages/bootstrap.tsx`, `src/App.tsx`.
- **Known:** reading only; low impact.
- **How to test:**
  - `gui`: start the app and immediately attempt an OTP join from the handler's browser.
  - Pass: no refusal once the dashboard shows credentials.
- **Then:** reorder.

#### U-7 The microphone card at the join, `se:choose` end to end, and the device sheet's stale audio text

- **What.**
  - Behaviour since `ef9dfe6` (2026-10-05, owner's request; supersedes `c020fc0`'s join half): at Join, before the offer, the page detects the outputs without asking and reports any routable list. Only where nothing routable lists (desktop Chrome/Edge with no microphone grant: one entry, empty id and label — measured), with a secure context, `getUserMedia`, a permission that is not `denied`, and no earlier answer on this device, it raises the microphone card; Continue → the browser's prompt → the real list. Any answer is remembered (`localStorage se.outputs.micCardAnswer`), so at most once per device.
  - "Choose a speaker on this device…" (`se:choose`) picked during a session is answered at the device's **next join** (the card once more, or Firefox's chooser from the Join click) — nothing is raised over a live stage.
  - The sheet's "still playing on … needs a tap" text no longer matches the client, which reports a refused sink instead of re-offering it (`83facec`) and has no tap-to-open chooser any more. Raised in `G06.md`.
- **Why untested.** A real headed browser with a human answering the browser's own prompt; the GUI for the sheet; Firefox and Safari are not installed here.
- **Suspects:** `ef9dfe6`, `c020fc0`, `83facec`; `static/audio.js` (`outputPlan`, `prepareOutputsForJoin`, `noteOutputChoiceRequest`), `static/index.html` (the submit handler); `src/components/pages/device-details.tsx`, `src/i18n/locales/en.json`.
- **Known:**
  - `mic_prompt` on the new build (`{"mic_prompt":{"audio":true,"rejoin":true}}`, 4 displays): speaker-granted and microphone-granted → no card, 3 labelled outputs; nothing granted → the card (Skipped), `se:choose` offered; rejoin → no card. One offer each, audio on the default output in all.
  - The `test-client-audio`, `test-client-reconfig` (case 14) and `test-client-join` cases pass.
- **How to test:**
  - Human, headed desktop Chrome with a profile that never granted the microphone: Join → the card before any picture; Continue → Chrome's prompt; Allow → the sheet lists real names; join again → no card. Repeat with Skip: no card at the next join, the sheet shows the choose entry.
  - Pick the choose entry in the sheet and save; leave and join again: the card appears once more.
  - Firefox: pick the entry, rejoin: the browser's chooser opens from the Join click; no microphone prompt.
  - Without the GUI: `config set devices.<i>.audioOutputDeviceId se:choose` with a headed browser.
- **Then:** fix the sheet text, which is a UI-text change and needs the owner's agreement; and the owner decides whether the card at a Chrome/Edge first join is wanted (`G06.md` addendum, question 1).

#### U-8 PowerShell does not wait for the GUI-subsystem CLI

- **What.** From an interactive PowerShell (pwsh 7 and 5.1), `ScreenExtend.exe <verb>` returns after 15–46 ms. Output lands after the next prompt, and `$LASTEXITCODE` is not set, so `doctor`'s exit-code verdict is lost to scripts (`G09.md` E3).
- **Why untested.** It is a packaging change.
- **Suspects:** `src-tauri/src/main.rs` (`windows_subsystem = "windows"`), `attach_console()` in `cli.rs`.
- **Known:** piped through `| Out-String` or `| Out-Host`, PowerShell waits.
- **How to test:**
  - Build a console-subsystem companion (for example `ScreenExtend-cli.exe`, a thin `[[bin]]` that calls the same CLI) and include it in the bundle (`tauri.conf.json` `externalBin` or `resources`).
  - Run `doctor` in an interactive pwsh.
  - Pass: output before the prompt, and `$LASTEXITCODE` set.
- **Then:** keep, and document it in `README.md`.

#### U-9 Every page of the desktop UI

- **What.** The owner reported "the desktop UI is odd". The pages: dashboard device list, device sheet, Settings and its config editor, log panel, onboarding walkthrough, notifications, and the bootstrap gate.
- **Why untested.** Out of scope for the previous job.
- **Suspects:** G10's 29 commits (`docs/commit-groups/COMMIT-GROUPS.md` G10), notably `e5c95e1` (context split), `57a907a` (UI tweaks), `8b5aef6` (device sheet), `d4e19b7` (one row per join), `e8f66a7` (config normalisation), `e324e8e` / `074098c` (adaptive checkboxes).
- **Known:** nothing tested.
- **How to test:**
  - Human walkthrough with the owner, page by page, against the owner's description of "odd".
  - Automated: `gui` request screenshots per route (`/`, `/dashboard`, `/devices`, `/settings`), plus console errors (`log_frontend_error` reaches the log bus).
  - Also on Safari 13.1 (the 10.15 WKWebView floor): `node scripts/check-legacy-webkit.mjs` after `pnpm build`.
- **Then:** only behaviour fixes, unless the owner asks for appearance changes.

#### U-10 One save bumps the epoch twice

- **What.** A sheet save can call `set_device_override` and then `set_device_adaptive_modes` or `set_device_auto_frame_rate`, which bumps the epoch twice and costs two in-place renegotiations (1–2 s picture gap each) (`G04.md` §1 item 2, `G07.md` §4).
- **Why untested.** GUI.
- **Suspects:** `src/components/pages/device-details.tsx`; `streamer/session.rs` (`bump_reconfig_epoch_for_device`).
- **Known:** the page applies the second bump correctly (harness case 13 in `scripts/test-client-reconfig.mjs`).
- **How to test:**
  - `gui`: change a stream setting and an adaptive checkbox in one save.
  - Count new peer connections on the page and `whepRequests`.
  - Pass: one renegotiation.
- **Then:** coalesce on the host (one epoch per save) or debounce on the client.

### 6.3 Display and driver

#### D-1 The virtual display driver's host leaks with every display

- **What.** `WUDFHost.exe`, the IddCx host for `VirtualDisplayDriver.dll`, leaks about **+46 handles, +6.5 threads and +67 MB of commit per display create** (≈47 MB at 1080p, 94–124 MB for larger DPR ladders), and **+27 handles, +6 threads and +47.6 MB per in-place mode change**. A removal returns almost nothing. It reached 20–34 GB during the previous job. The previous job's own memory emergency is the evidence (`G04.md` §3, `BASELINE.md` D-11).
- **Why untested.** The fix is in the driver, and freeing the memory needs admin.
- **Suspects:**
  - The bundled driver: a rebranded fork of `MolotovCherry/virtual-display-rs` at upstream commit `eae5120a676831ebb96a78125f7e84532bcaf152`. The fork repository is not published; see `src-tauri/resources/PROVENANCE.md`.
  - App side: the number of creates and mode commits (`windows_utils/virtual_display.rs`, `streamer/server.rs`).
- **Known:**
  - Equal on the old app and the previous head.
  - Skipping the redundant same-mode set (`97e46d1`) did not reduce the per-create figure.
  - Shared-driver list notifies, host starts, renegotiations without a mode change, scale-live and name/audio edits leak 0.
  - The suspected cause is that the swapchain and monitor objects of a departed or re-moded IddCx monitor are not released.
- **How to test:**
  - **Free the leak (owner, elevated, no host running):** `pnputil /enum-devices /class Display` (or `Get-PnpDevice -FriendlyName '*Virtual Display*'`), then `pnputil /restart-device "<instance id>"`, or reboot.
  - **Measure per cycle:** run `{"scenarios":["small_window"],"runs":1}` and read `R.memory`, which records the largest `WUDFHost.exe`'s commit, handles and threads at both ends of the live-display part plus `joinsInWindow`.
  - For mode changes add `"settings_change":true,"settings_only":["refresh rate"]`.
  - Sample `Get-Process -Id <WUDFHost pid> | Select PrivateMemorySize64,HandleCount,Threads` every 2 s alongside, as G04 did.
  - Pass for a driver fix: net 0 MB (±5 MB) and 0 handles per create → remove cycle over 20 cycles.
  - **A driver fix:** rebuild the fork (it needs the private branding patch or a re-creation from `PROVENANCE.md`'s listed `.inf` values), release IddCx swapchain and monitor objects on departure and mode change, re-sign the `.cat` with the key behind `ScreenExtend.cer` (owner only), and install with `ScreenExtend drivers install`.
- **Then:**
  - With a fixed driver, re-run every M-item.
  - Without one, keep reducing creates. Candidate, not done: adopt a display still in its disconnect grace for a fresh join from the same device token.

#### D-2 The owner's 360 Hz panel drops to 60 Hz while exactly one device is connected

- **What.** Read-only `QueryDisplayConfig` sampling (`G04.md` §4) shows the internal panel (target 8388688) at 360.006 Hz with no virtual display, **60.003 Hz with exactly one**, and 360 Hz again with two or more. With one VD, DWM runs at 60 Hz; with two, DWM runs at 360 Hz and composes each 60 Hz VD about 3× too often, flooding capture (D-3). Raised in `G04.md`.
- **Why untested.** Writing a display the host did not create is forbidden, and reading the persisted topology needs admin.
- **Suspects:** Windows' persisted per-topology configuration (the CCD database). The IDD monitor's identity is the same on every attach (UID256). `b0183dd` attributed a "357 → 60 fps" collapse to a 360 Hz client; this is likely what it saw.
- **Known:** reading only; the cause is unproven.
- **How to test:**
  - **"Set it once" (owner, by hand):** with exactly one ScreenExtend client joined, set the internal panel to 360 Hz in Settings → Display → Advanced display. Disconnect the client, reconnect, and read the panel rate with the same read-only sampler.
  - Pass: it stays at 360 Hz with one device on every reconnect.
  - **Persisted topology (admin):** look in `HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Configuration` for an entry pairing the panel with a `CHY0000` monitor at 60 Hz.
  - **Host restores it (needs the owner to relax the never-write rule to "restore what it was"):** record the panel's mode before the first attach (`QueryDisplayConfig`), and after the attach settles, if the panel's rate changed, re-apply exactly the recorded mode to that one target (`SetDisplayConfig` with the saved path). Test with the handler (one join) and the sampler.
  - Pass: the panel stays at 360 Hz, and the virtual display's stream is unchanged in latency.
- **Then:** if "set once" persists, document it as an owner step; otherwise implement the restore only with the owner's written permission.

#### D-3 Two or more virtual displays degrade every stream

- **What.** With 2+ displays attached:
  - the handler author saw streams freeze or run at about 100–220 ms p50;
  - G01 measured WGC delivering 175–190 frames/s to a 60 Hz session;
  - `two_clients` once left client B without a picture;
  - on `d4d8be9` B's gap was 635 ms and A's p50 221 ms.

  Sources: `BASELINE.md` D-03, `scripts/test-handler/README.md`, `G01.md` E1/E5.
- **Why untested.** The join ration.
- **Suspects:** D-2 (panel rate), `ff8bfff` (flat 4 ms WGC floor, now `ea821a9`), `b0183dd` (cadence gate).
- **Known:**
  - `ea821a9` cut deliveries from about 190/s to about 70/s in one run per arm, at the same latency.
  - On a quiet machine, G04's two-client runs showed no collapse (`bUntouched` true, A 24.2 ms / 88 fps).
- **How to test:** run `{"label":"g04-two","scenarios":["small_window"],"duration":15,"two_clients":true,"runs":3}` with a `QueryDisplayConfig` sampler running alongside, then again after D-2's "set once". Also sample `nvidia-smi` and per-process CPU for `WUDFHost.exe` and `dwm.exe` (`G01.md` §7 item 11). Pass: B's p50 during A's edits within noise of B before; A after B joined within noise of single-display.
- **Then:** if the panel rate is the cause, D-2 fixes it. Otherwise investigate DWM compose cost.

#### D-4 What makes an in-place renegotiation fail on the owner's machine

- **What.** `f789b76` makes any failed renegotiation converge on one display, but what triggered the failure on the owner's machine is unknown (`G04.md` §1, §8).
- **Why untested.** The CLI edits never failed on their own here.
- **Suspects:** WGC `ItemConvertFailed` after a mode set (fixed for the fresh-join path by `48d3807`), and a DXGI fallback that cannot duplicate on hybrid graphics.
- **Known:** G01 D1 occurred in 1 of 48 logs.
- **How to test:**
  - On the owner's real usage (GUI, real client), run with `serve -v` or read `ScreenExtend logs` after a reported duplicate.
  - Search for `WGC could not open`, `falling back to DXGI`, `abandon`.
  - Handler: repeat `{"scenarios":[],"calibrate":false,"settings_change":true}` ten times and grep `serve.log` for a failed continuing join.
- **Then:** fix the trigger it names.

#### D-5 WGC stale-monitor retry: the natural trigger (G01 D1)

- **What.** `48d3807` was verified only by forcing the failure.
- **Why untested.** Rare (about 1 in 50 joins) and timing-dependent.
- **Suspects:** `48d3807`; `windows_utils/streamer/pipeline.rs` `start_on_monitor`.
- **Known:** forced ×3, it recovered on WGC with first picture at 5.6 s.
- **How to test:** repeat `{"scenarios":["small_window","hires_small_window"],"duration":10}` until a `serve.log` contains `WGC could not open … re-resolving the display`. That join must then show `offers: 1` and no `DXGI` line.
- **Then:** if it falls through anyway, raise the retry count or the delay.

### 6.4 Real link and network

**These need a second device on a real access point.**

#### N-1 Constant QP 15 on Wi-Fi

- **What.** The owner's devices are set to `videoQuality` 15. Since `cf63279` that really is constant QP 15:
  - IDRs of 83 KB at 1080p and 144 KB at 1440p;
  - tiny P frames on a still desktop;
  - **no bitrate adaptation**.

  Before, it silently ran CBR at 12–22 Mbps. On loopback it made no latency difference (`G02.md` E5b/E6), but on Wi-Fi each IDR is a burst of 50–100 MTU packets. Raised in `G02.md`, `CONSOLIDATION-TODO.md`.
- **Why untested.** No real link.
- **Suspects:** `cf63279`, `83cc915`, `e9c521b` (the QP-floor path under `adaptive_quality`); `windows_utils/streamer/nvidia/encoder.rs`; `streamer/server.rs` (`cfg.qp = Some(o.video_quality)`).
- **Known:**
  - The IDR cost table is in `G02.md`.
  - The keyframe storm is now about 72 IDRs per session instead of about 430.
  - The old app ran every session at about 1 Mbps by accident (the RTT ×1000 bug fixed by `f5d0e27`).
- **How to test:**
  - Equipment: host wired or on Wi-Fi, a second laptop on 5 GHz and then 2.4 GHz Wi-Fi.
  - Measure with the camera method (`guides/clients-network-and-glass-to-glass.md`), or the handler with its browser on the second device. A remote-browser mode is not built: the handler's CDP client would need to connect to that device's Chrome debugging port.
  - Compare CQP 15 vs CBR (`host_env` with an `SE_X_FORCE_CBR`-style switch, re-created from `G02.md`'s description) vs `adaptive_quality` on, 3 interleaved runs each, `small_window` and `large_area`, with `two_clients` optional.
  - Pass: p99 and longest gap not worse than CBR.
- **Then:** if CQP 15 is worse on Wi-Fi, propose VBR with a max QP (`G02.md` Q1) to the owner.

#### N-2 The tuned media socket tier

- **What.** `8feb63c`'s socket tier (DSCP/`IP_TOS`, `SO_SNDBUF`) has been off since `fca80af` because a wildcard bind made joins flaky. It only matters on a real AP (`G03.md` §6).
- **Why untested.** Loopback only.
- **Suspects:** `8feb63c`, `fca80af`; `streamer/sockets.rs`, `webrtc_session.rs` (`SCREENEXTEND_MEDIA_SOCKET`).
- **Known:** nothing measured.
- **How to test:**
  - `host_env` `{"SCREENEXTEND_MEDIA_SOCKET":"1"}` vs `{}` with a LAN client on a busy AP, 5 interleaved runs, counting join failures.
  - Check DSCP marking with a capture on the AP side (Wireshark on a mirror port or a monitor-mode adapter).
  - Pass: no join failures, and p99 better under background load.
- **Then:** keep by the rule; fix the bind if joins fail.

#### N-3 Transport ablations: pacer, BWE, profile, packetizer, runtime width

- **What.** Not run on loopback (`G03.md` §7):
  - the pacer's Light tier on a LAN session (every Unknown-locality join gets it);
  - the BWE and delay-BWE off;
  - Baseline and Main profiles at 120 Hz with matched arms;
  - the stock packetizer under matched load;
  - transport runtime widths.
- **Why untested.** The join ration and memory reaping.
- **Suspects:** `42d364a`, `9de8eb4`, `8feb63c`, `932e0bc`, `3791155`; `streamer/pacer.rs`, `congestion.rs`, `negotiate.rs`, `packetizer.rs`.
- **Known:**
  - Pacer cost on LAN is zero by construction (`pace_wait` 0).
  - With no TWCC, REMB starves the stream (p50 510–1216 ms).
- **How to test:**
  - Rebuild with `docs/overhaul-new/G03-experiment-switches.patch`, which adds the `SE_G03_*` switches. It is a diff against `06e6fbb`; port it to the current head by hand.
  - Requests, verbatim from `G03.md` §7, each against `{}`, 3 interleaved runs:
    - `{"scenarios":["small_window","hires_small_window"],"runs":3,"host_env":{"SE_G03_PACE_LIGHT":"1"}}`
    - `host_env` `SE_G03_NO_BWE` / `SE_G03_NO_DELAY_BWE` at hi-res 120 Hz
    - `{"scenarios":["hires_small_window","hires_large_area"],"runs":3,"host_env":{"SE_G03_BASELINE":"1"}}` (and `SE_G03_MAIN`)
    - the same request with `SE_G03_STOCK_PAYLOADER`
    - `host_env` `SE_TRANSPORT_WORKERS` 4/8/16
  - On loopback first, then on Wi-Fi.
- **Then:** keep rule.

#### N-4 Behaviour under loss

- **What.** Untested under real loss:
  - `34ae41d` (continuous cuts from 1 % loss);
  - the delay estimator;
  - LTR repair, inert anyway since NVENC runs `ltr=false`;
  - the keyframe gate and `PliBackoff`, where NACK and TWCC loss open the window;
  - the audio redundancy flag.

  Raised in `G03.md` §7.
- **Why untested.** The handler's `loss` request is not implemented: inducing loss needs admin or the owner's network.
- **Suspects:** `34ae41d`, `30d5719`, `396916b`, `240f9f6`, `9fd5615`; `streamer/congestion.rs`, `keyframe.rs`, `repair.rs`.
- **Known:** nothing under loss.
- **How to test:**
  - With admin, implement `loss` with a user-mode tool such as `clumsy` (WinDivert) on the slot's UDP ports: 1 %, 3 % and 10 % loss, and 50 ms of jitter.
  - Measure stamp p50/p99, longest gap, IDRs per minute, `keyframe_requests_suppressed`, and audio `dc_drops` and clicks.
  - Pass: recovery from a burst within one RTT plus one frame; no keyframe storm.
- **Then:** tune `PliBackoff`'s cap if a real loss waits too long.

#### N-5 The cloud relay and TURN

- **What.**
  - `8e709bd` (a relayed page without join ids renegotiates on its live join) has never run. If the owner joins over the cloud URL, this is the most likely cure for "two devices, two screens" (`G04.md` §1).
  - `PliBackoff` against the relay's older page.
  - The cloud client's own 16-worker MMCSS runtime (`G09.md` item 11).
  - `7fa2ebc`'s relayed `/reconfig` `.next()` (addressed by `8e709bd`).
  - TURN.
- **Why untested.** Running `serve` without `--no-cloud` registers a public session with `session.screenextend.app` on the owner's behalf.
- **Suspects:** `8e709bd`, `7fa2ebc`, `3ff2da7`, `89f1ac6`; `streamer/cloud.rs`, `streamer/session.rs`.
- **Known:**
  - Unit test `a_relayed_page_without_join_ids_lands_on_its_live_join`.
  - The relay serves its own older client page and tunnels only `POST /whep`, `GET /reconfig` and `POST /leave`.
- **How to test (with the owner's permission):**
  - Start `serve` without `--no-cloud`, join from a second device over the cloud URL, and change each stream-class setting with `devices set`.
  - Count displays (`status --json`) and dashboard rows.
  - Pass: one join and one display after every edit.
  - Also measure idle CPU of the cloud runtime threads.
  - For TURN: `turn set <urls> --username U --credential C` against a test TURN server, with the client behind a symmetric NAT.
- **Then:** if duplicates appear, trace `continuing_join` / the relayed continuation in `server.rs`.

#### N-6 The hosted network

- **What.** `netsh` hosted network and the Mobile Hotspot ladder (`5b2c437`) were never exercised.
- **Why untested.** It needs admin and a radio.
- **Suspects:** `windows_utils/hosted_network.rs`; `5b2c437`.
- **Known:** nothing.
- **How to test:** `ScreenExtend network start <ssid> <password>` (admin), join a phone to it, and run a session. Pass: the session streams; `network stop` restores the adapter.
- **Then:** fix per `guides/windows-legacy.md`.

#### N-7 Join timing on a real network

- **What.** Join timing was only measured on loopback:
  - first-join answer about 2.1 s host-side after `97e46d1`;
  - first offer about 0.1 s after the click with `cd611ce`.
- **Why untested.** Loopback only.
- **Suspects:** `97e46d1`, `cd611ce`, `4527723` (trickle ICE).
- **Known:**
  - Page timeline: before `cd611ce`, the first `enumerateDevices()` took 1.36–1.67 s and the first `new RTCPeerConnection()` 0.41–0.56 s inside the click (`G07.md` §1).
  - Headless used fake devices, so the real-device `enumerateDevices()` cost is unknown.
- **How to test:**
  - Real Chrome (not headless) on a second device.
  - Join with the `join_probe` timeline re-created from `G07.md` (a temporary page timeline behind a `?g07=` switch).
  - Pass: first paint under 3 s on a fresh browser.
- **Then:** if the warm-up hurts a phone, move it to the form's first focus (`G07.md` §8).

### 6.5 Audio

#### A-1 Plain-HTTP audio end to end

- **What.** The LAN QR code is `http://` (`src/lib/utils.ts`). With no secure context there is no `AudioDecoder` and no `AudioWorklet`, so audio takes the standard Opus track. `201a981` fixed that track's clock: before, 627,600 concealed samples, 5/15 clicks and p50 453 ms; after, 240, 15/15 and 131 ms. That was proven with `AudioDecoder` removed on HTTPS, **not on HTTP itself** (`G06.md`).
- **Why untested.** The handler's `plain_http` tier cannot complete a join: the `/whep` POST fails with `net::ERR_ABORTED` after the HTTPS-upsell modal.
- **Suspects:** `201a981`, `736f0f9`; `static/index.html` (the upsell modal path), `static/audio.js`.
- **Known:** the tier ladder in `G06.md`.
- **How to test:**
  - First fix the handler so the plain-HTTP join completes. Dismiss the upsell modal with its own button through `.click()`, and check why the POST aborts.
  - Then run `{"scenarios":[],"audio_paths":["plain_http"],"calibrate":false}`.
  - Also a human: scan the LAN QR, decline HTTPS, enable audio, play speech on the host.
  - Pass: 15/15 clicks, p50 under 200 ms, continuous speech.
- **Then:** if the POST abort is a page defect rather than a harness defect, fix it in `index.html` (G07's area).

#### A-2 `AudioContext` suspended in a real browser

- **What.** The context is built and resumed inside the Join click (`beginFromGesture`) and resumed on `visibilitychange`. A real browser may still suspend it, for example after a phone is locked and unlocked; the picture returns without sound (`G06.md` route 8).
- **Why untested.** Headless runs use `--autoplay-policy=no-user-gesture-required`.
- **Suspects:** `static/audio.js`, `static/index.html`.
- **Known:** `degraded` bit 7 in the client STATS frame reports it.
- **How to test:**
  - Human, with real Chrome and Safari on a phone: join, enable audio, lock and unlock the phone, and play sound on the host.
  - Pass: sound returns within 1 s.
  - Check `status --json` for `degraded` bit 7.
- **Then:** add a resume on the next user gesture if needed.

#### A-3 Audio DataChannel drop bursts and the SACK allowance

- **What.** Host logs show backpressure drops (`dc_drops`) in bursts of 41–43 packets, about 205–215 ms of audio, on an idle link. `07ec295` allows one 220 ms SACK interval of in-flight data before dropping. Raised in `G06.md` and `G03.md`; UNVERIFIED.
- **Why untested.** The join ration.
- **Suspects:** `07ec295` (`AUDIO_SACK_ALLOWANCE_MS` in `streamer/webrtc_session.rs`); webrtc-sctp 0.17.1's `buffered_amount` accounting.
- **Known:**
  - `G03.md` attributes the drops to `buffered_amount` counting bytes until the peer's SACK.
  - `G06.md` says the link to delayed SACK is not established.
- **How to test:**
  - A/B `{"scenarios":["small_window"],"duration":15,"runs":1,"audio":true}` on `1f85421` vs `07ec295^`.
  - Count `dc_drops` in the host audio diagnostics, and clicks.
  - Add an SCTP trace (webrtc-sctp `log` output) to tie the bursts to SACK timing.
  - Pass: no bursts on an idle link.
- **Then:** keep, or lower the allowance per the owner's answer (`REPORT.md` question 16).

#### A-4 Forced redundancy cost

- **What.** The cost of `FLAG_REDUNDANT` (current plus previous Opus frame in each packet) was never measured (`G06.md` E8).
- **Why untested.** Budget.
- **Suspects:** `streamer/audio/protocol.rs`, `webrtc_session.rs`.
- **Known:** nothing.
- **How to test:**
  - Add a temporary switch forcing redundancy.
  - Run `{"scenarios":[],"audio":true,"calibrate":false,"click_source":"wasapi"}` ×3 each arm, interleaved.
- **Then:** keep the default unless it changes delay by more than the noise.

#### A-5 Process loopback on other machines, and the endpoint fallback

- **What.** `dbb2863` tries process loopback first (−29 ms end to end here). It is untested on:
  - Windows 10 2004–22H2;
  - a default output that is Bluetooth or has APOs;
  - a machine where process loopback is refused, so the endpoint fallback has to work.

  Raised in `G06.md`.
- **Why untested.** One machine.
- **Suspects:** `dbb2863`; `windows_utils/audio/loopback.rs`, `process_loopback.rs`.
- **Known:** probe 5 + 5 and handler 2 + 2 here. `SE_ENDPOINT_LOOPBACK=1` forces the old order.
- **How to test:**
  - On each machine: `{"scenarios":[],"audio":true,"calibrate":false,"click_source":"wasapi"}` ×2, and the same with `"host_env":{"SE_ENDPOINT_LOOPBACK":"1"}` ×2, interleaved.
  - Pass: 15/15 clicks on both tiers; process loopback not slower.
- **Then:** if process loopback is refused or slower somewhere, probe and fall back (probe, never version).

#### A-6 The G06 changes on the real release profile

- **What.** Every G06 change ran on thin-LTO builds.
- **Why untested.** No fat build by agents.
- **Suspects:** `c020fc0`, `61da27a`, `24f75e8`, `736f0f9`, `201a981`, `83facec`, `dbb2863`.
- **Known:** section 3 of `docs/overhaul-new/REPORT.md`.
- **How to test:**
  - On the fat-LTO build: `{"scenarios":[],"audio_paths":true,"mic_prompt":true,"calibrate":false}`.
  - Then `{"scenarios":[],"audio":true,"calibrate":false,"click_source":"wasapi"}` ×2, plus the same with `"host_env":{"SE_ENDPOINT_LOOPBACK":"1"}` ×2, interleaved.

  Consolidation ran none of these.
- **Then:** if a fix does not hold, re-open it.

### 6.6 Client devices and the client page

#### C-1 Phones, tablets, Safari, Firefox, older browsers

- **What.** Every client-browser tier was exercised only in headless Chrome 154 on the host itself. This includes:
  - the video paths (`VIDEO_PATH` webcodecsCanvas / webcodecsVideo / videoElement) and the audio paths (`AUDIO_PATH` workletSab / workletPort / workletPost / opusTrack);
  - trickle ICE against real engines;
  - the 0.4–0.8 s warm-up task at page load on phones (`cd611ce`);
  - clients without transport-cc, which REMB starves (`G03.md`);
  - Safari without `unadjustedMovement` (input, I-2).
- **Why untested.** No second device.
- **Suspects:** `static/*`, `cd611ce`, `9aae1d3`, `a00083d`, `2dda493`.
- **Known:** the client page has been through seven cold review passes (`docs/reviews/`).
- **How to test:** `guides/clients-network-and-glass-to-glass.md` (fourteen client-browser tiers). Read each device's tier from `status --json` (`video_path`, `audio_path`, `transport_tier`, `key_tier`, `degraded` bits).
- **Then:** fix per tier; keep rule.

#### C-2 `<video>` vs WebCodecs, the hardware decoder, and after-paint effects

- **What:**
  - On one window each, `<video>` read 23.4 and 34.3 ms against WebCodecs' 25–26 ms at 1080p (`G05.md` §2).
  - Headless Chrome decoded on the Intel iGPU of this hybrid machine (NVDEC 0 %).
  - `desynchronized` and `VideoTrackGenerator` (`?present=track`) act after the paint call, which the stamp metric does not include.
- **Why untested.** The join ration; glass-to-glass needs a camera.
- **Suspects:** `fd2c1d0` (removed `desynchronized`), `static/transform-worker.js`.
- **Known:** the old page's worker and transport measured the same as the new ones (k02, k03).
- **How to test:**
  - Run `{"scenarios":[],"calibrate":false,"fallback_paths":true,"netlog":true}` three times on a quiet machine.
  - Then a 240 fps camera filming the host stamp and the client screen together (`guides/clients-network-and-glass-to-glass.md`), comparing the default and `?present=track`.
  - Pass: a number, with run counts.
- **Then:** if `<video>` is faster somewhere, it is a tier-order question for the owner.

#### C-3 The false `<video>` demotion, live

- **What.** `9aae1d3` is harness-proven. Live it occurred in 2 of 56 runs on the previous head, so confirming it needs tens of joins (`G05.md`).
- **Why untested.** The join ration.
- **Suspects:** `9aae1d3`; `af69481`, `2f5d314`; `static/index.html` `armFrameWatchdog`.
- **Known:** 0 demotions in 23 joins on fixed builds.
- **How to test:**
  - On builds with and without `9aae1d3`, interleaved: `{"scenarios":["small_window"],"runs":1,"duration":10,"calibrate":false,"slow_answer":{"n":20}}`.
  - Count `offers > 1` with videoPath 3, and the page console line "no WebCodecs frame within 5s".
  - Pass: 0 on the fixed build.
- **Then:** keep.

#### C-4 The legacy `createEncodedStreams` half of the keyframe fix

- **What.** On the legacy transport `a00083d` transfers the writable too; that half is unit-tested only (`G05.md`).
- **Why untested.** Budget.
- **Suspects:** `a00083d`; `static/index.html` `createEncodedStreams` branch.
- **Known:** on the script-transform tier, PLIs went 28–35 → 3–4 per 10 s.
- **How to test:**
  - `{"scenarios":["small_window"],"runs":2,"duration":10,"calibrate":false,"chrome_flags":[],"fallback_paths":true}`.
  - Read the "no RTCRtpScriptTransform" tier's PLI count.
  - Pass: 3–4 per 10 s, not 30.
- **Then:** keep or fix.

#### C-5 `pagehide` release, the attempt id end to end, the faster reconfigure poll

- **What:**
  - `aec25fb` (`pagehide` releases the join in any state) was never run through the handler.
  - The lost-answer continuation (`e8cbe87` + `7584269`) ran once, simulated.
  - The 250 ms reconfigure poll (`poll250`) was never run (`G07.md` §8).
- **Why untested.** Budget.
- **Suspects:** `aec25fb`, `e8cbe87`, `7584269`; `static/index.html`.
- **Known:** `7584269` was measured once: displays 2 → 1, renegotiated in 0.9 s.
- **How to test:**
  - **K3:** `{"scenarios":[],"calibrate":false,"leave_rejoin":true}`. Pass: a `leave beacon` in `serve.log` for every leave, display gone within about 2 s.
  - **K4:** `{"scenarios":["small_window"],"duration":5,"calibrate":false,"url_params":"g07=dropfirst"}` on a build that re-creates the `?g07=dropfirst` switch (not committed). Pass: one join and one display after the retry.
  - **poll250:** `{"scenarios":[],"calibrate":false,"settings_change":true,"url_params":"g07=poll250"}` interleaved with `g07=none`, 3 each, on a build with that switch.
- **Then:** keep rule for poll250 (picture recovery time).

#### C-6 A failed join gives everything back (`restorePreJoinPage`, G07-RESTORE)

- **What.** After a refused or unanswered `/whep`, a failed renegotiation, or `pagehide`, the page must be back on its join form with the error, Connect enabled, keys reaching the form (Ctrl+C on the error text works), no wake lock, no fullscreen/pointer/keyboard lock, no `beforeunload` prompt, no poll, and exactly two offers on a failing host (one same-tier retry, no `<video>` demotion). Harness-verified only (`test-client-join.mjs`).
- **Why untested.** Virtual display creation was failing on the machine (`SetDisplayConfig` error 31, "did not attach within timeout", driver host ~36 GB), so no join could complete.
- **Suspects:** `static/index.html` `restorePreJoinPage`, `restorePageChrome`, `syncInputGate`, `setupFailureAction`; `static/audio.js` `endSession`.
- **How to test:**
  - **The owner's case (a 500), on a host whose display creation fails** — the machine as it was, before the driver restart: `{"scenarios":[],"calibrate":false,"custom":{"js":"new Promise(r=>setTimeout(()=>r({err:document.getElementById('errorMessage').textContent, form:getComputedStyle(document.getElementById('join')).display, connect:!document.getElementById('connect').disabled, live:isLive, pc:pc===null, armed:streamInputArmed, wake:SEVideo.getStats().wakeLock, fs:!!document.fullscreenElement, offers:performance.getEntriesByType('resource').filter(e=>/\\/whep$/.test(e.name)).length}),25000))","host_commands":[["status","--json"]],"timeout":40}}` (if the handler aborts on a failed join, open the join URL in the handler's Chrome and read the same expression through `Runtime.evaluate`). Pass: `err` starts `WHEP POST failed: 500`, form not `none`, `connect` true, `live` false, `pc` true, `armed` false, `wake` false, `fs` false, `offers` 2; `serve.log` shows two `join failed` lines, not three.
  - **On a healthy host**, the join and pagehide: `{"scenarios":[],"calibrate":false,"leave_rejoin":true,"join_probe":"({live:isLive, armed:streamInputArmed, wake:SEVideo.getStats().wakeLock})"}`. Pass: `armed` and `wake` true while live; a `leave beacon` per leave.
  - **A failed renegotiation:** while a session is live, stop the host's display path from completing a mode change (or `ScreenExtend stop` mid-`settings_change`), and read the expression above. Pass: as for the 500, with the join kept (no `leave beacon`).
  - **Copy, by a person** (the one check the no-input rule leaves to the owner): after a failed join, select the error text and press Ctrl+C; type a digit into the OTP boxes. Both must work. On a phone, check the screen dims on its normal timeout after a failed join.
- **Then:** keep; if a check fails, the inventory comment above `restorePreJoinPage` names what armed it.

### 6.7 Measurements still owed

The post-reboot job (branch `post-reboot`, `docs/overhaul-new/POST-REBOOT.md`) ran most of what this
section used to list. They are done and not repeated here: the fat-LTO build and the five-round
three-way; the noise figure with `stamp_band: "center"`; the `ea821a9` A/B with one and two
displays (the latter refuted it on painted fps; see that file); x264, async NVENC and the 1440p
encode-stage question; G08's telemetry fixes; D-05 after the fix (twice); `07ec295`; G04b through
the real page; G05's 40-join watchdog check, the legacy-transport keyframe hand-back and `<video>`
vs WebCodecs; every G06 tier including plain HTTP; `mic_prompt`; the join-failure restore in four
cases; `settings_change` in every class; `two_clients`, `leave_rejoin`, `auto_rejoin` and `logs`.
What is left:

#### M-1 G01's other open runs

- **What:** P-core confinement (`d071133`) at 1080p; GPU scheduling class 2 and P-core off at
  hi-res; the scheduled cadence grid (E2); the CPU-bridge scaler paths (`01f91c8`, `42df0f7`).
- **Why untested.** Never reached; lower value than the GPU-clock finding.
- **How to test:** on a binary built from branch `g01-exp`, interleaved, 3–5 each:
  `{"scenarios":["small_window","large_area"],"runs":2,"host_env":{"SE_G01_NO_PCORE":"1"}}`,
  `{"scenarios":["hires_small_window"],"runs":3,"host_env":{"SE_GPU_SCHED_CLASS":"2"}}`, and both
  with `"host_env":{}`. Use the handler's `arms` request where the switch can be read per
  renegotiation, so one join serves every arm.
- **Then:** keep rule.

#### M-2 The adaptive-quality QP floor's value

- **What.** `--adaptive-quality on` renegotiates in place and reopens NVENC as CBR (measured), but
  no line logs the floor QP, so whether the floor is applied is unknown.
- **How to test:** add one `tprintln!` of the min QP in the NVENC configure line (or read it from
  `status --json` if exposed), then the `settings_change` request with `settings_only: ["adaptive quality"]`.

#### M-3 G04's join re-check (round 3 of E2/E3)

- **What.** `023014b^` vs `023014b`, three interleaved runs each of
  `{"scenarios":["small_window","hires_small_window"],"duration":15,"slow_answer":{"n":5}}`;
  compare `join request → WHEP answer generated` p50 and `joins[].answerMs`.
- **Why untested.** Two builds and 42 joins for join time, which the owner said does not matter.

#### M-4 The previous job's baseline specials that never ran

- `slow_answer` on `d4d8be9` and in headed Chrome; `slow_answer` under induced load on the old app:
  `{"scenarios":[],"calibrate":false,"slow_answer":{"n":10},"netlog":true}` (headed: add
  `"headed":true,"chrome_flags":["--start-minimized"]`) and
  `{"scenarios":[],"calibrate":false,"slow_answer":{"n":5},"netlog":true,"cpu_burn":{"threads":16,"seconds":150}}`.
- G08's static idle cadence with instruments off (`bin-exp1`, 3 interleaved pairs of
  `{"scenarios":["static"],"static_duration":30}` with and without
  `"host_env":{"SE_G08_TELEM_OFF":"1","SE_G08_CLIENT_TELEM_OFF":"1"}`); one of six ran (0 changed,
  0 green), the rest were dropped for memory.

#### M-5 D-02, the lost `/whep` answer: root cause

- **What.** The host logs `join accepted: WHEP answer generated`, but the browser never receives
  the response before its 10 s deadline. Post-reboot: **0 of ~110 quiet joins** retried
  (`offers` 1 everywhere, including 40 back-to-back `slow_answer` joins). It needs load.
- **How to test:** `{"scenarios":[],"calibrate":false,"slow_answer":{"n":5},"netlog":true,"cpu_burn":{"threads":16,"seconds":150}}`,
  repeated; lay each flagged join's `serve.log` against Chrome's net log `HTTP2_SESSION_RECV_HEADERS`;
  add a worker-stall probe (`tokio::runtime::Handle::metrics`) behind a switch.

#### M-6 Unexplained numbers

- **Host socket → client transform: 7–12 ms on loopback.** The per-stage budget (POST-REBOOT.md
  Part 4) leaves this as the largest term after the GPU clocks and the composition wait. It spans
  webrtc-rs's send path after `write_rtp` returns, the loopback and Chrome's receive pipeline up to
  the encoded transform. Instrument: a host stamp at the real `send_to` (behind a switch) and a
  per-frame receive time on the page; or a packet capture (`pktmon`, admin).
- **~10 % stamp checksum failures at 1080p** on every build, with `center` and the precise clock;
  none at hi-res. Vary the stage's cell size (48 px at 1080p vs 64 px at hi-res) first.
- `hires_large_area` at 120 Hz p50 outliers of 67–520 ms (`G03.md` §5): `runs: 5`, with
  `status --json` per window.
- Why the old app collapses at 120 Hz; the stage's 200 ms bounds check (H-2).

#### M-7 The GPU clock hold beyond this machine

- **What.** `6c0bc71` holds an NVIDIA GPU out of idle while a session streams (AC power only):
  −5.5 ms at 1080p60, −6.1 ms at 1440p120 here, +15–18 W. Unmeasured on any other GPU, on a
  desktop (whose idle clocks may differ), with the per-application "prefer maximum performance"
  driver setting instead (which would make it redundant), and on battery (forced with
  `SE_GPU_CLOCK_HOLD=1`).
- **How to test:** the same A/B as POST-REBOOT.md §4.2 (`SE_GPU_CLOCK_HOLD=0` vs default, five
  interleaved pairs of `{"scenarios":["small_window","hires_small_window"],"runs":2,"duration":15,"stamp_band":"center","force_exclusive":true}`),
  reading `load.powerW`, `load.pstates` and `load.videoClockMHz` in each window.
- **Also owed by analogy:** the AMF, Quick Sync and classic NVENC paths do not take the hold;
  whether their GPUs idle the same way is unmeasured.

#### M-8 The CLI pins a 120 Hz device at 60 Hz

- **What.** `ScreenExtend devices set <key> …` on a device with no saved row writes
  `refreshRate: 60` (`cli.rs`, the `devices set` default row), and a save makes every field the
  user's, so a 120 Hz client drops to 60 fps after any CLI edit. Found by the handler; the desktop
  UI writes the row from its `DeviceJoin` payload and is not affected, by reading.
- **How to test after a fix** (seed a new row's refresh rate from the live join's reported rate):
  join a 120 Hz hi-res client, `devices set <token> --video-quality 20`, read `serve.log`'s
  `ring encoder initialized … @120`.

### 6.8 Everything this machine cannot run

#### P-1 macOS

- **What.** Every macOS capture, audio and display tier, and the macOS mirrors:
  - `58fd8b1`'s `session::clear_join_seeded` in `macos_utils/mod.rs::set_device_override`, UNVERIFIED (not compiled);
  - `301230d`'s macOS half (`CGEventGetLocation`), uncompiled;
  - `57d1620`'s log strings that lost their `\` continuations;
  - the 67 G12 commits.
- **Why untested.** The machine is Windows.
- **How to test:** `guides/modern-mac.md` (macOS 14.2+), `guides/intel-mac.md` (12.3–12.7.6, dual GPU), and the 10.15.8 development Mac per `docs/dev-workflow.md`. At minimum, `cargo check` / `clippy` / `test` on macOS and `scripts/check-macos-dyld.sh`.
- **Then:** per guide.

#### P-2 AMD and Intel encoders

- **What.** The AMF backend is entirely UNVERIFIED (`CLAUDE.md`). G02 says Intel picks `MFX_RATECONTROL_CQP` without a capability mask, so it does not share the constant-QP bug.
- **Why untested.** No AMD or Intel encode machine.
- **How to test:** `guides/windows-amd.md`; Intel through the handler with `host_args` `["--encoder","intel"]` on an Intel-GPU machine.
- **Then:** per guide.

#### P-3 Windows below 24H2, DXGI duplication, single-GPU desktops

- **What:**
  - `ff8bfff` / `3bcd1e8` (WGC on Windows 10, pre-24H2, pre-1809);
  - every DXGI commit (`f979694`, `0e0b384`, `04ac630`, `c85d905`, `5ebaac6`, `ae14b0c`, `969025e`); DXGI duplication of an IDD display returns `ACCESS_LOST` on every acquire on this hybrid machine;
  - process loopback on Windows 10 (A-5).
- **Why untested.** OS and hardware.
- **How to test:** `guides/windows-legacy.md`; `docs/manual-tests/windows-capture.md` §5b; the probe harness's `--capture-backend dxgi`.
- **Then:** per guide.

#### P-4 Installer and auto-update

- **What.** NSIS / WiX driver install and cleanup (`windows/hooks.nsh`, `windows/fragment.wxs`), `drivers install`, and the updater's custom comparator.
- **Why untested.** Never exercised.
- **How to test:** on a clean VM: install, join, update from an older release, uninstall. Check that the driver and cert are removed.
- **Then:** fix per finding.

#### P-5 Clients, network and glass-to-glass

- **What.** DSCP/WMM on a real AP, TURN, repair under induced loss, and camera glass-to-glass.
- **Why untested.** Hardware.
- **How to test:** `guides/clients-network-and-glass-to-glass.md`.
- **Then:** per guide.

---

## 7. When you finish

- Write `docs/future-test/REPORT.md` in the shape of `docs/overhaul-new/REPORT.md`.
- Update `docs/OPTIMIZATIONS.md` with each change and its status: `VERIFIED`, `COMPILE-CHECKED` or `UNVERIFIED`.
- Run the gates in section 3, item 7.
- Leave the machine with no host, no test browser, no stage window and no virtual display of yours.
- Leave the owner's `config.json` hash unchanged by you, and the owner's cursor scheme and pointer settings restored.
- Do not push and do not merge.

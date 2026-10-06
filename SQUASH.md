# SQUASH.md — bug-squash directive for the latency-overhaul run

Read this before touching anything. It applies to every agent working in this tree.

## Situation

`bef5f89` (2026-09-06) is the last human-written commit. The app at that commit was slower but
reliable: it had been hardened by hand over months and carried no known bugs.

`f5d0e27..HEAD` (branch `latency-overhaul`, from 2026-09-08 02:50) is a single autonomous run:
one Fable 5.1 agent, ~25 hours, ~8 billion tokens, 562 commits, 407 files, +146k/-18k lines.
It made the app measurably faster. It also introduced bugs. The speedups are wanted; the bugs are not.

| Ref | Meaning |
|---|---|
| `bef5f89` = tag `pre-overhaul` | **baseline** — the human-written app, the reference for correct behaviour |
| `f5d0e27` | first overhaul commit (`bef5f89` is its parent) |
| `HEAD` of `latency-overhaul` | current tree, fast, buggy |

## Goal

Patch the bugs one at a time while keeping the latency gains. The baseline is the oracle for
*what the app should do*; the current tree is the code you edit. A wholesale revert to `bef5f89`
is not the goal and is not acceptable.

## Method — every bug, in this order

1. **Diagnose in the current tree first.** Reproduce or trace the reported symptom. Find the
   code that produces it. Write down the failing path before reading any history.
2. **Compare against the baseline.** For each file on that path:
   ```sh
   git diff bef5f89 HEAD -- <path>            # what the overhaul changed
   git log --no-merges bef5f89..HEAD -- <path> # which overhaul commits touched it, and why they say they did
   git show bef5f89:<path>                     # the human version, whole
   ```
   Read the baseline implementation of the same behaviour, not just the diff hunks.
3. **Identify what went wrong.** State it in one or two sentences: what invariant the baseline
   upheld, which overhaul commit broke it, and whether that commit's speedup depended on breaking
   it. Look the change up in `docs/OPTIMIZATIONS.md`; its status tag (`VERIFIED` /
   `COMPILE-CHECKED` / `UNVERIFIED`) tells you how much the speedup was ever worth.
   A commit message is a claim, not evidence.
4. **Fix, keeping the speedup.** Restore the baseline's invariant inside the new design. Reach
   for the baseline's code shape only where the new design cannot hold the invariant. Revert a
   change outright only when its speedup is `UNVERIFIED` or unmeasurable, and say so.
5. **Verify.** Run what applies: `cargo check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test` (from `src-tauri/`), `pnpm build`, `pnpm lint`, `pnpm test:client`,
   `node scripts/check-bindings.mjs`. Reproduce the original symptom again and confirm it is gone.
   State plainly what was and was not run.
6. **Record.** Append one entry to the Log below (format there). Update the row in
   `docs/OPTIMIZATIONS.md` if you changed the status of an optimisation.

## Rules

- One bug per change. Do not fix things you were not asked about; note them in the Log as
  `open` instead.
- Do not widen scope into refactors, doc rewrites or style passes. `CLAUDE.md` and the ledgers
  in `docs/` are the overhaul's own account of itself; where they disagree with the code, the
  code wins, and where the code disagrees with the baseline's *behaviour*, the baseline wins.
- Never remove a test or turn a `skip` into a pass to get green. `src-tauri/src/test_support.rs`
  explains the convention.
- Keep the existing conventions the overhaul introduced where they are sound (`lock_recover`,
  `tprintln!`, no guard across `.await`, join-id/token addressing, no cursor manipulation on the
  dev box). A bug fix is not a licence to reintroduce the baseline's globals or IP-keyed maps.
- **Never push. Never touch `origin` or any remote.** No `git push`, no `git fetch`, no `git pull`, no
  remote-tracking branch, no PR. All work stays in this local tree; the user moves commits
  between machines with git bundles. Commit locally, one commit per bug, subject `fix(<area>): <symptom>`,
  body = the "what went wrong" sentence plus before/after.
- Be token-efficient toward the user: lead with the cause, then the fix, then verification.
  No narration of the search.

## Hot zones (most-churned files in the overhaul)

`windows_utils/streamer/pipeline.rs` (40 commits), `streamer/server.rs` (39), `cli.rs` (39),
`streamer/webrtc_session.rs` (35), `streamer/static/index.html` (30), `lib.rs` (24),
`streamer/telemetry.rs` (19), `streamer/input/windows.rs` (18), `macos_utils/streamer/pipeline.rs`
(16), `streamer/static/audio.js` (15), `streamer/static/input.js` (14), `streamer/session.rs` (14),
`streamer/config.rs` (14), `windows_utils/virtual_display.rs` (13). A bug in one of these has a
long `git log -- <path>` to read; read it.

Files that did not exist at `bef5f89` (`git show bef5f89:<path>` fails) have no baseline to
compare against. For those, compare the *behaviour* they replaced: find what the baseline did
for the same job and check the new file preserves it.

## Log

One entry per bug, newest last. Keep each under ten lines.

```
### <short symptom>                                   status: fixed | open | wontfix
- Where:      <file:line in current tree>
- Baseline:   <what bef5f89 did; file:line at bef5f89>
- Broke in:   <overhaul commit sha> — <its stated reason>
- Cause:      <one sentence>
- Fix:        <one sentence; speedup kept / partially reverted / reverted (why)>
- Verified:   <what ran; what did not>
- Agent:      <model, date>
```

### Flat grey areas flicker green on the client            status: fixed (awaiting user's visual check)
- Where:      src-tauri/src/streamer/server.rs (join path sets `cfg.intra_refresh`); src-tauri/src/windows_utils/streamer/nvidia/encoder.rs (`maxNumRefFrames`, LTR)
- Baseline:   `intra_refresh` false unless the probe's `--intra-refresh`; NVENC 1 ref, no LTR (pre-overhaul:nvidia/encoder.rs:593-602, config.rs:412-416 warns waves are visible bands)
- Broke in:   d996635 — turned intra refresh on for every live join to A/B the relay's keyframe rate; f66a5a7/240f9f6 — 4 refs + 2 LTR slots for P-frame repair
- Cause:      continuous intra stripe quantises chroma differently in flat regions → moving green band over grey; LTR set additionally over-fills the SPS-declared DPB
- Fix:        intra refresh off (opt-in SE_INTRA_REFRESH=1); NVENC back to 1 ref, LTR opt-in SE_NVENC_LTR=1. Speedup kept: neither knob was a latency gain. Undoes abd1d2c/e9b839d, which disabled LTR only.
- Verified:   cargo check/clippy/test on the RTX 3080 Ti box; visual check pending (user)
- Agent:      Claude Fable 5.1, 2026-09-14

### White text has a yellow fringe, picture soft            status: fixed (awaiting user's visual check)
- Where:      src-tauri/src/streamer/server.rs (`start_session`, device override → cfg)
- Baseline:   `cfg.qp = Some(o.video_quality)` → constant QP 23 on every overridden session (pre-overhaul:server.rs:827)
- Broke in:   e9c521b — slider became a QP floor under CBR so congestion control could act; ledger row 17, COMPILE-CHECKED, never measured on a client
- Cause:      CBR at the computed ~12 Mbps quantises above 23 on busy frames; 4:2:0 chroma smears glyph edges
- Fix:        constant QP restored; `BitrateApplied` reporting kept. Speedup: none lost (CBR was not a latency change); congestion control inert on overridden sessions as at baseline
- Verified:   compile via tauri dev; visual check pending (user)
- Agent:      Claude Fable 5.1, 2026-09-14

### Device sheet crashed on "Select.Item must have a value prop"       status: fixed
- Where:      src/components/pages/device-details.tsx (audio output picker); streamer/static/audio.js listOutputs
- Baseline:   same map, but the client only listed outputs after the mic prompt; the overhaul's page reports them before it, with deviceId ""
- Broke in:   the overhaul's client audio-output reporting (labels-before-permission)
- Cause:      enumerateDevices returns "" ids before the microphone permission; Radix refuses value=""
- Fix:        client drops id-less outputs; sheet filters empty/duplicate ids (relayed clients run the old page)
- Verified:   test:client, legacy-webkit, tsc, lint; user to confirm with a live device
- Agent:      Claude Fable 5.1, 2026-09-14

### Cursor teleports back after a native trackpad move            status: fixed
- Where:      src-tauri/src/streamer/input/windows.rs advance_rel; input/macos.rs mouse_delta
- Baseline:   pure relative MOUSEEVENTF_MOVE, no remembered position (pre-overhaul:windows.rs:310)
- Broke in:   the overhaul's tracked-absolute path (rel_pos / self.pos) that avoids a second pointer curve
- Cause:      the delta advanced from the injector's remembered point, not from where the cursor really was
- Fix:        every delta starts from GetCursorPos / CGEventGetLocation; the cache is gone. Curve rule kept (still no double acceleration). macOS half unverified here
- Verified:   clippy, 46 input tests; user to confirm on the client
- Agent:      Claude Fable 5.1, 2026-09-14

### Device sheet said "This device's browser can't switch speakers"; output discovery rebuilt to the owner's spec      status: fixed (headless Chrome verified; other browsers open)
- Where:      src-tauri/src/streamer/session.rs (AudioOutputsReport default, supported_rank), server.rs (/audio-outputs), cloud.rs (relay dispatch), single_instance.rs (status --json), static/audio.js (discovery), static/index.html (Connect handler)
- Baseline:   bef5f89 had no per-device output list at all; the sheet's "unsupported" text came with the overhaul's list
- Broke in:   the overhaul's output reporting: `AudioOutputsReport` derived Default (supported=false), so offline / not-yet-posted / relayed joins (the relay does not tunnel /audio-outputs) read "can't switch"; a body without the field parsed as false; on plain HTTP (the LAN QR scheme) the page required enumerateDevices, which Chrome hides there
- Cause:      "not reported" and "cannot" shared one value
- Fix:        db377a1 + 6cce67c (unreported is supported:true with no outputs; page supported = any sink API, iPhone category or insecure context; false only on a secure page with no sink API at all), then the owner's spec (findings/overhaul-new/OWNER-SPEC-audio-discovery.md) in 6c0e363: every discovery API in order, bounded, with a definite result; card only when nothing routable; getUserMedia once + one more enumeration; else default output alone, labelled:false; `discovery` map on /audio-outputs, in status --json and docs/TELEMETRY.md. Branch audio-discovery off latency-overhaul-new (not latency-overhaul, per the orchestrator); handler change cf17b7b
- Verified:   pnpm test:client (10/10), check-legacy-webkit, cargo fmt/clippy -D warnings/test (490 pass); handler mic_prompt + custom on Chrome 154 headless: host held supported:true in 4/4 cases (mic granted, speaker-only, nothing granted → card → default only, plain HTTP → default only). Mac sync (SSH-GIT.md) skipped: the Mac is shut down
- Open:       Firefox (chooser on every Connect), Safari 18.4+ macOS/iPadOS/iOS, Safari < 18.4 (the one supported:false case), iOS 16.4+ audio session and earpiece hold, Android Chrome, Samsung Internet, real headed Chrome/Edge prompt, the relay forwarding /audio-outputs. Questions in docs/overhaul-new/G06.md addendum (2)
- Agent:      Claude Opus 5.5, 2026-10-05
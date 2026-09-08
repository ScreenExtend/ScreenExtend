# O — Git archaeology

Territory: the git history, not the working tree. 412 commits across 4 refs (`main`,
`origin/highdpi`, `origin/sys-audio-winmac`, `origin/HEAD`), 33 tags, plus ~55 dangling commits
and 67 dangling blobs in the object database.

Everything below is derived from the repository. Where it contradicts `CLAUDE.md`, the
contradiction is stated explicitly.

---

## 1. Evolution by era

Commits per month, all refs:

| Period | Commits | |
| --- | ---: | --- |
| 2024-01 … 2024-03 | 86 | React/Next UI prototype |
| 2024-07 … 2024-12 | 90 | Tauri shell, driver IPC, Tauri v2 |
| 2025-02 … 2025-12 | 12 | dormant |
| 2026-06 | 87 | the streaming engine |
| 2026-07 | 31 | second encoder tier, input, updates |
| 2026-08 | 75 | CLI, security, DPR, system audio, the perf push |
| 2026-09 | 31 | AirPlay tier, config editor, 10.15 webview, BYE |

### Era 1 — Web UI prototype (2024-01 → 2024-03, 86 commits)

`5cc74e2` "Initial commit" through `8aabd1c`. A React app with no host: device-detail forms, QR
panes, auth screens, a sidebar. `ae7cde3` (2024-02-03) "feat: migrate app to tauri desktop shell"
is the pivot; `95110ca` and `fa41ef2` add the first native commands (WiFi Direct hosted network,
`list_ips` via `pnet`, swapped to `local-ip-address` the next day in `af50a73`). Nothing streams.

### Era 2 — Desktop shell and driver plumbing (2024-07 → 2024-12, 90 commits)

`03ad67d` (2024-07-22) "feat: add virtual display driver IPC and localStorage-backed user data"
and `81eaf6f` "feat(drivers): add self-service driver install flow" bring in the virtual display
driver. `843d7c6` (2024-12-21) migrates to Tauri v2; `0052cab` moves user storage off
`localstoragedb` onto `tauri-plugin-store` — the ancestor of today's `config.json`; `0fe3511`
adopts `tauri-specta` typed events, which is the origin of `src/lib/bindings.ts`. A run of
dependabot merges (`c032dd3` … `96d61cc`) dates the first appearance of the bot.

### Era 3 — Dormancy (2025-02 → 2025-12, 12 commits)

Twelve commits in ten months. `74b52ba` (2025-07-08) adds the file-lock single-instance detection
that `single_instance.rs` still uses.

### Era 4 — The streaming engine (2026-06, 87 commits)

The single most consequential month. `a6bbb2e` (2026-06-09) "feat(streamer): add WebRTC-based
low-latency screen streaming" lands the entire `streamer/` module, the NVENC backend and the
client page in one commit, together with `amd.md` and `intel.md` as design notes for the encoders
not yet written. Then, in order: `70a09f9` Intel Quick Sync + DXGI fallback capture (and the
deletion of `intel.md`); `30d3833` the log bus and in-app terminal; `b424835` the cloud relay;
`1e7cbd9` (2026-06-28) the whole macOS backend — capture, VideoToolbox, `CGVirtualDisplay`;
`df946fe` the Quick Sync same-adapter bridge; `ad598b2` vendors the `windows-capture` crate
in-tree.

### Era 5 — Second tier and product surface (2026-07 → 2026-08-21, ~60 commits)

`0b138bc` remote input injection, `43c9a7b` the x264 software fallback, `ebdd219` + `37c0f82` +
`8af812a` the updater, `fcf95cb` i18n, `4c0d063` the first `CLAUDE.md`, `2f25b6a` single-instance
enforcement, `8cb680e` macOS input + permission gating, `005adb4` the universal macOS binary,
`f8c99d8` the onboarding walkthrough, `9810540` (2026-08-21) the full CLI, `0ac7cc7` + `46bee7f`
device banning and auto-join.

### Era 6 — The security batch (2026-08-22, 8 commits in 4m13s)

One scripted burst: `d83e6d9` eslint flat config, `fbb62c1` vendored-blob checksums, `c3825d5` CI
+ dependabot, `f49d4fc` CSP and shell allowlist, `75190a9` fmt/clippy, `a03e655` UI hardening,
`00e1bef` TLS cert perms, `2be8d8d` token-based device trust, `43827ea` docs — and `e1d5d28`,
which deletes the CI added 4m13s earlier. See §4.

### Era 7 — DPR, system audio, and the perf push (2026-08-24 → 2026-08-31)

`f85701d` renders virtual displays at the client's native pixel ratio (merged from
`origin/highdpi` at `f83173f`). Then the audio work, which is the most tangled part of this
history: it was developed on `origin/sys-audio-winmac` as four commits (`dcf3dc3` "to be
reviewed", `420004a` "more agent work for audio, need to be reviewed", `ea12e0b`, `374a6c1`,
`f9e2312`, `bba3711` "hardening work by agent, reviewed but needs final pass") and **re-landed on
`main` as a different, cleaner series** — `f343fa1` Windows WASAPI, `c7d7cff` macOS, `407df89` the
shared host clock, `bb1de86` the shared core and client player. The re-split is documented in a
dangling blob titled "Commit plan" (`81cfd8b5`), which names the chunks A–M and the `git add`
recipe for each. The design documents were dropped in that re-split; see §2.

Immediately after, four dedicated perf commits on 2026-08-28: `5ba1183`, `450778a`, `a56eedc`,
`15b41e4`, plus `3a4e3d2` for input. See §3.

### Era 8 — Compatibility floor and endgame (2026-09)

`8b3a31e` the AirPlay display tier for 10.13–10.14.3; `8ba0e3b` display-capacity refusal;
`1e77f12` + `f02fc51` the config editor and UI error recovery; `8c53d6a` the audio worker + shared
ring; `5122b0b` raises the macOS floor to 10.15 (two days *after* shipping a 10.13 tier, which is
why that tier is now below the floor); `da0494e` + `de461ab` the Safari 13.1 webview campaign;
`ab37665` the BYE frame and link-stall tolerance; `bef5f89` removes dependabot entirely.

### Commits mentioning latency, perf, lag, sync, jitter, or a measurement

Complete, newest first. Bold = analysed in full in §3.

| Date | Commit | Subject | On `main`? |
| --- | --- | --- | --- |
| 2026-09-05 | **`de461ab`** | fix(macos): harden the JSON editor and its build target for the 10.15 webview | yes |
| 2026-09-05 | **`ab37665`** | feat: tell clients why the stream ended and ride out brief link stalls | yes |
| 2026-09-04 | **`8c53d6a`** | perf: move client Opus decode into a worker with a shared-memory ring | yes |
| 2026-09-04 | `c5bf2afe` | perf: warm the whole settings editor during bootstrap, not just its chunk | **dangling** |
| 2026-09-04 | `6bb00f8` | fix: snap the client-reported refresh rate to the nearest 10 Hz | yes |
| 2026-09-04 | **`da0494e`** | fix(macos): stop the JSON editor crashing on the 10.15 webview | yes |
| 2026-09-03 | **`f02fc51`** | fix: recover from failing backend calls across the desktop UI | yes |
| 2026-09-03 | **`1e77f12`** | feat: add a schema-backed config.json editor to Settings | yes |
| 2026-08-28 | **`15b41e4`** | perf(macos): cut capture and encode latency in the video pipeline | yes |
| 2026-08-28 | **`a56eedc`** | perf(windows): skip re-encoding unchanged frames in DXGI capture | yes |
| 2026-08-28 | **`450778a`** | perf(windows): encode BGRA directly in the x264 software fallback | yes |
| 2026-08-28 | **`5ba1183`** | perf(windows): decouple NVENC capture from encode via a texture ring | yes |
| 2026-08-28 | `3a4e3d2` | fix(input): correct modifier injection and reduce input latency | yes |
| 2026-08-27 | `bb1de86` | feat(audio): add the shared core, client player, and app wiring | yes |
| 2026-08-27 | `407df89` | feat(av): sync audio to video with a shared host clock | yes |
| 2026-08-26 | **`374a6c1`** | feat: small performance changes regarding threading | **no** (sys-audio-winmac) |
| 2026-08-25 | **`ea12e0b`** | perf: cut end-to-end streaming latency and tighten A/V sync | **no** (sys-audio-winmac) |
| 2026-07-24 | `43c9a7b` | feat: add software x264 H.264 encoder fallback | yes |
| 2026-06-30 | `df946fe` | feat(streamer): add Quick Sync same-adapter input bridge | yes |
| 2026-06-28 | `1e7cbd9` | feat(macos): implement macOS capture, encode, and virtual display | yes |
| 2026-06-12 | `70a09f9` | feat(streamer): add Intel Quick Sync encoder and DXGI fallback capture | yes |
| 2026-06-09 | `a6bbb2e` | feat(streamer): add WebRTC-based low-latency screen streaming | yes |

`ea12e0b` and `374a6c1` are not reachable from `main`, but every hunk of both was verified present
in the working tree — they were re-landed inside `bb1de86` / `407df89`. `c5bf2afe` was also
re-landed, but with its comments stripped; see §6.

---

## 2. Recovered documents

Written verbatim to `docs/recovered/`, each with a one-line provenance header.

| File | Bytes (with header) | Recovered from | Notes |
| --- | ---: | --- | --- |
| `docs/recovered/PRD.md` | 32 264 | `dcf3dc3:PRD.md` (added 2026-08-23, deleted in `420004a`) | The Windows system-audio PRD, §1–§11. Sole revision. |
| `docs/recovered/AUDIO_NOTES.md` | 15 657 | `dcf3dc3:AUDIO_NOTES.md` (same) | The measured spike results. Sole revision. |
| `docs/recovered/airplay-fallback-design.md` | 59 707 | **dangling** commit `67687cb` "temp commit", 2026-09-02 | The only copy in existence; blob `371b5879`. |
| `docs/recovered/intel.md` | 15 963 | `17fcbc2:src-tauri/.../intel.md`, deleted in `70a09f9` | Quick Sync design note; sibling of the surviving `amd.md`. |
| `docs/recovered/DEVLOG.md` | 17 204 | `91fe73f8:DEVLOG.md`, deleted in `d737efa` 2026-07-16 | Project narrative at v0.2.3; identical across all 3 revisions. |

**Correction to `CLAUDE.md:637`.** It states that `AUDIO_NOTES.md`, `AUDIO_NOTES_MACOS.md`,
`AUDIO_NOTES_MACOS_LEGACY.md` and `PRD-macos-legacy-audio.md` "survive only in history, on the
`sys-audio-winmac` branch (commit `420004a`)". Three of those four never existed.
`git ls-tree -r --name-only 420004a` lists exactly six markdown files: `CLAUDE.md`, `README.md`,
`macos/ScreenExtendAudio/README.md`, the two `PROVENANCE.md`, and `amd.md`. Scanning every tree of
every reachable commit *and* every dangling commit finds no file named `AUDIO_NOTES_MACOS*.md`,
`PRD-macos-audio.md` or `PRD-macos-legacy-audio.md` at any path. Only three docs were ever
committed and later deleted: `AUDIO_NOTES.md`, `PRD.md`, `DEVLOG.md`.

The macOS names appear only *inside* `//!` headers on dangling blobs — e.g. `eec4f989`
("Default-output-device routing state machine (PRD-macos-legacy-audio.md §8.3)"), `caec4420`
("Core Audio **Process Tap** backend — PRD-macos §5.2"), `d1ab97e6`, `9df6e0fb`. Those blobs are
earlier drafts of files that did land; the headers were rewritten before landing in some cases and
kept in others, which is why the tree still cites documents that never existed.

**How they were found.** `git log --all --diff-filter=A --name-only -- '*.md'` gives the reachable
set. For the rest: `git fsck --lost-found`, then `git cat-file -p` over all 67 dangling blobs
matching on the first line, and `git ls-tree -r` over all ~55 dangling commits filtering for
`.md`. That is what surfaced `67687cb`.

### 2.1 Why the AirPlay doc was lost — the root cause, and it is still active

`.gitignore:41-44` reads:

```
# Temporary agent files
docs/
.agents/
skills-lock.json
```

**All of `docs/` is ignored.** `git check-ignore -v docs/recovered/PRD.md` →
`.gitignore:44:docs/`. The commit timestamps for 2026-09-02 show exactly how that lost the
document:

| Time (committer) | Commit | Event |
| --- | --- | --- |
| 11:49:48 | `67687cb` "temp commit" | `docs/airplay-fallback-design.md` is committed. `docs/` is **not** ignored yet — `git show 67687cb:.gitignore` ends at `generate-sources.sh` / `sources/`. |
| 17:47:32 | `d5e3ea0` "chore: ignore agent scratch directories" | adds `docs/` under "# Temporary agent files". |
| 17:49:46 | `8b3a31e` | the AirPlay implementation lands on `main`, **two minutes later**. The design doc is now invisible to `git add` and is silently left behind. |
| 18:19:25 | `d6811f8` | `CLAUDE.md:373` is updated to cite `docs/airplay-fallback-design.md` — a path git will never track. |

`67687cb` was subsequently orphaned, leaving the only copy in a dangling object.

This is not merely historical. **Every file this discovery phase writes under `docs/` is untracked
right now** — `git status --porcelain` is empty despite five recovered documents and three
completed discovery reports existing on disk. Until `.gitignore:44` is narrowed, every
`git add docs/...` in this engagement silently no-ops and a `git clean -xdf` deletes the lot.
Filed as `blocker-build-gitignore-excludes-the-entire-docs-directory.md`.

---

## 3. Perf commits, in full

### `5ba1183` — perf(windows): decouple NVENC capture from encode via a texture ring (2026-08-28)

651 insertions. Stages each captured frame into one of **K = 3** shared, keyed-mutex D3D11
textures (`const RING: usize = 3` in `pipeline.rs`) so capture(N+1) overlaps encode(N). Adds
`Encoder::new_shared_ring`, `encode_input_slot`, `encode_repeat_slot`, a `SlotEncodeError`
distinguishing `Poisoned` (keyed mutex wedged) from `Encode`, an `AsyncNvencRing` driving a named
`nvenc-ring-encode` thread over two `crossbeam_channel::bounded(k)` channels (`free_slots`,
`ready`), and a `poisoned_slots` set that permanently skips a slot whose mutex wedged at
`KEY_ENCODER`. Also raises `BROADCAST_CAPACITY` 2 → 4. Log line on success:
`"pipeline: live capture ready -- NVENC ZERO-COPY RING (K={}) decoupled path"`.
**No before/after number is cited anywhere in the commit.**

### `450778a` — perf(windows): encode BGRA directly in the x264 software fallback (2026-08-28)

Probes whether the loaded libx264 accepts `X264_CSP_BGRA` by opening a BGRA encoder first and
falling back to I420 if it returns null, so x264 does its own SIMD colour convert and the rayon CSC
pool is skipped entirely. Logs the choice as `csp=BGRA` / `csp=I420`. Also moves the I420 convert
off rayon's global pool onto a dedicated pool of `available_parallelism()/2` clamped to **1–4**
workers named `x264-csc-{i}`, each raised to `THREAD_PRIORITY_ABOVE_NORMAL`. Keeps a
`repeat_bgra` copy of the last frame for `encode_repeat`. Rationale, quoted:
"the x264 encode thread is TIME_CRITICAL + MMCSS and x264 runs its own slice threads; letting the
convert fan out across every core oversubscribes those hot threads and adds latency under CPU
contention." **No number.**

### `a56eedc` — perf(windows): skip re-encoding unchanged frames in DXGI capture (2026-08-28)

Adds `Duplicator::present_region_is_empty`, which reads `GetFrameMoveRects` and
`GetFrameDirtyRects` into a reused `meta_buf` and returns true only when
`AccumulatedFrames <= 1`, `TotalMetadataBufferSize != 0`, zero move rects and zero dirty rects. On
an empty region the `CopyResource` is skipped. Guarded by `const MAX_CONSECUTIVE_SKIPS: u32 = 5`
so at most five frames in a row can be skipped. **No number, and the constant is uncommented.**
This commit also deleted three rationale comments from `450778a`, one commit earlier — see §6.

### `15b41e4` — perf(macos): cut capture and encode latency in the video pipeline (2026-08-28)

Five distinct changes:
- `activity.rs`: `NSActivityOptions::UserInteractive` → `UserInitiated | LatencyCritical`, and the
  activity is moved from the encode loop to the session lifetime (`SessionCapture._activity`), so
  App Nap and timer coalescing are suppressed for the whole session.
- `cgds.rs`: `kCGDisplayStreamMinimumFrameTime` biased to `(1.0/fps) * 0.95` so early frames are
  not coalesced away; the frame handler now reads `CGDisplayStreamUpdate::rects(DirtyRects)` and
  returns immediately when `rect_count == 0`.
- `encoder.rs`: drops the LTR feature entirely (`kVTCompressionPropertyKey_EnableLTR`),
  changes `MaxFrameDelayCount` from `0` to `1` with the rejection logged non-fatally
  ("encoder may queue more frames"), and tightens the `DataRateLimits` window from **1.0 s to
  0.5 s** in both `configure_rate_control` and the live bitrate setter (cap stays
  `bitrate * 1.5 / 8` bytes).
- `pipeline.rs`: replaces the hot-path "submit the same frame again" completion barrier with
  `encoder.complete_pending()`; `BROADCAST_CAPACITY` 2 → 4.
- `qos.rs`: the realtime time-constraint policy was `period = constraint = period_ns`,
  `computation = period_ns/8`. Now `computation = period_ns/8` and
  `constraint = min(period_ns/2, 4 ms)` raised to at least `computation + 1 ms` and capped at
  `period_ns` — a tighter scheduling window.

**No before/after number.**

### `374a6c1` — feat: small performance changes regarding threading (2026-08-26, branch-only)

The `qos.rs` constraint change and `complete_pending` above, landed two days earlier on the branch.
Plus two things `15b41e4` does not contain:
- `windows_utils/streamer/pipeline.rs`: WGC
  `MinimumUpdateIntervalSettings::Custom(frame_duration)` → `Custom(frame_duration * 2 / 3)`,
  i.e. ask WGC for updates at 1.5x the target frame rate. **Still in the tree at line 319.**
- `index.html`: reverses `ea12e0b`'s `jitterBufferTarget = 0` / `playoutDelayHint = 0` back to
  `30` / `0.03` — **but only on the plain `<video>` fallback path.** The WebCodecs path still
  carries the zeros. That divergence is live today (`index.html:832`, `:874` vs `:922-923`) and is
  filed as `speculative-client-jitter-buffer-target-diverges-between-paths.md`.

### `ea12e0b` — perf: cut end-to-end streaming latency and tighten A/V sync (2026-08-25, branch-only)

The densest commit of the set for tunable constants. Every one verified present in the tree today:

| Knob | Before | After | File |
| --- | --- | --- | --- |
| `BWE_POLL_INTERVAL` | 250 ms | **120 ms** | `webrtc_session.rs:40` |
| `AUDIO_BACKPRESSURE_BYTES` | 256 KiB | **16 KiB** | `webrtc_session.rs:43` |
| DataChannel `buffered_amount()` poll | every packet | **every 4th packet** (`since_bp_check % 4`) | `webrtc_session.rs` |
| input `TICK_MS` | 8 ms | **4 ms** | `input.js:275` |
| `BACKLOG_DROP_THRESHOLD` | 4 | **3** | `transform-worker.js` |
| audio target-depth churn rate-limit | 1000 ms | **300 ms** | `audio.js` |
| `jitterBufferTarget` | 10 | **0** (later 30 on fallback only) | `index.html` |
| `playoutDelayHint` | unset | **0** | `index.html` |
| `pcConfig` | `{iceServers}` | **`+ bundlePolicy: 'max-bundle'`** | `index.html` |
| video-delay EMA alpha | 0.05 both ways | **0.30 falling / 0.05 rising** (asymmetric, so the estimate tracks improvements fast and degradations slowly) | `transform-worker.js` |
| `VideoDecoder` config | `optimizeForLatency: true` | **removed** from both `isConfigSupported` and `configure` | `transform-worker.js` |
| WASAPI stereo-f32 convert | generic path | **`if fmt.is_fast_path()` straight `from_le_bytes` loop** | `windows_utils/audio/format.rs:134` |

Also: `useScriptTransform = hasScriptTransform && !hasLegacyEncodedStreams` (prefer the legacy
`createEncodedStreams` when both are present), the transform worker is loaded from a cached
`Blob` URL with a direct-URL fallback, and an empty-payload H.264 packetization now triggers
`pipeline.request_idr()` instead of being dropped silently.

**No before/after number.**

### `8c53d6a` — perf: move client Opus decode into a worker with a shared-memory ring (2026-09-04)

Adds `audio-worker.js` (header parse → dedup → WebCodecs `AudioDecoder` → deinterleave → ring
write) and makes the worklet ring **planar and power-of-two** so both sides are `set()` memcpys.
Layout constants, a hard ABI contract between `audio-worker.js` and `audio-worklet.js`:
`HDR_BYTES = 13` (seq u32 LE, capture ns u64 LE, flags u8), `SR = 48000`, `CTRL_INTS = 16`
(`CTRL_BYTES = 64`) with slots `W = 0`, `R = 1`, `OVERRUNS = 2` as absolute uint32 frame counters,
`DEFAULT_CAP_FRAMES = 65536` (power of two, so `& mask` wraps), `SYNC_POST_INTERVAL_MS = 250`.
Dedup rule: drop `seq === lastSeq`, and drop when `((seq - lastSeq) >>> 0) >= 0x80000000` (older,
i.e. late). When `crossOriginIsolated`, the ring is a `SharedArrayBuffer` the worker writes and
the worklet reads with `Atomics`; otherwise the identical ring is private and samples arrive by
`postMessage`. The `sab` capability was dropped from the join handshake — the host now only
checks `webcodecsOpus && worklet`, because the transport is the client's own business.
**No latency number.**

### `ab37665` — feat: tell clients why the stream ended and ride out brief link stalls (2026-09-05)

Two features in one commit. The BYE control frame: opcode `0x63`, two bytes `[BYE, reason]`,
`reason ∈ {KICKED = 0x01, HOST_EXIT = 0x02}`, pushed over the reliable data channel;
`broadcast_bye` fans it to every session and waits on a `std::sync::mpsc` ack with
`BYE_FLUSH_TIMEOUT = 250 ms` before displays are torn down, called from `exit_app` and from the
CLI `serve` shutdown. Note `parse(&build_bye(..))` is asserted to return `None` — BYE is
host→client only and must not be parseable as an input event.

The link-stall tolerance replaces the flat `setInterval(..., 1000)` `/reconfig` poll with an
adaptive probe. Timings, all new constants:

| Constant | Value | Meaning |
| --- | ---: | --- |
| `PROBE_OK_MS` | 1000 | poll interval while healthy |
| `PROBE_FAST_MS` | 300 | poll interval once `degraded` or after a refusal |
| `DEGRADE_AFTER_MS` | 1200 | show "Reconnecting…" / stalled state |
| `DROP_AFTER_MS` | 5000 | give up: "Disconnected, reconnect to continue." |
| `REFUSED_MS` | 400 | a fetch that throws faster than this counts as connection-refused (host gone) rather than a network stall |

`markAlive()` is called from every successful `/reconfig`, so the probe doubles as the liveness
signal. `navigator.onLine === false` switches the message to "Offline — waiting for your network".

### Frontend perf series

- **`1e77f12`** (2026-09-03) — the schema-backed `config.json` editor. Lazy-loaded Monaco plus a
  hand-written 361-line JSON schema (`src/lib/config-schema.ts`) describing every key, range and
  default, surfaced as hover docs and Ctrl+Space completions. 1746 insertions.
- **`f02fc51`** (2026-09-03) — 1663 insertions across 17 files. Wraps every Tauri command, config
  write and event subscription in error handling, rolling state back and raising a destructive
  toast instead of failing silently. Adds a reactive config store
  (`getConfigSnapshot` / `subscribeToConfig` / `useConfig`) so name, theme and per-device overrides
  follow `config.json` without refetching, and detects concurrent external edits to `config.json`
  with a keep-mine / load-current dialog. Not a latency change; a correctness change on the same
  files.
- **`da0494e`** (2026-09-04) — 60 lines. First attempt at the 10.15 webview: adds
  `src/lib/legacy-webview-polyfills.ts` and imports it from `main.tsx`.
- **`de461ab`** (2026-09-05) — the real fix, 717 insertions. Adds WeakRef, AggregateError,
  `replaceChildren`, `Array#at`, `findLast`, RegExp `d`/`v` flags and a `matchMedia` patch; splits
  the Monaco workers into dedicated entry files that import the polyfills first (they never run
  the entry chunk); pins the build target to **safari 13.1**; changes `darkMode` to
  `["variant", ".dark &"]` because Tailwind 3.4's default compiles to `:is(.dark *)` which
  Safari 13 drops the whole rule over; adds `scripts/check-legacy-webkit.mjs` (85 lines) to
  re-derive the shim list from `dist/` after a build and fail on anything unpatched; and caches
  JSON scanning/masking, debounces validation, and preloads the editor "to cut input lag".

The measured numbers behind that last clause are **not in the tree**. They exist only in dangling
commit `c5bf2afe` (2026-09-04, "perf: warm the whole settings editor during bootstrap, not just
its chunk"), whose code landed but whose comments did not: monaco chunk import+parse **~2.2 s
warm / 4.3 s first**, `editor.create` **~100 ms**, first render to settle **~620 ms**, a second
`create` after warm-up **8 ms vs 99 ms cold**, and the reason the throwaway editor must reuse the
exact `editorOptions` (monaco keys its font-measurement cache on the font config). The 300 ms
preload delay is `setTimeout`, deliberately not `requestIdleCallback`, because Safari 13 lacks it.

---

## 4. Why CI was disabled — and the file, verbatim

`c3825d5` "ci: add CI pipeline and dependabot, gate releases on version tags" was committed at
**2026-08-22 17:49:35 -0700**. `e1d5d28` "fix: temporarily disable ci till further notice" deleted
all 92 lines at **17:53:48 -0700** — **4 minutes 13 seconds later**. The six intervening commits
are 13–30 s apart, so the entire span is one scripted batch. `ci.yml` was never edited in between
(`git diff c3825d5:.github/workflows/ci.yml e1d5d28^:.github/workflows/ci.yml` is empty), and
`git log --all -- .github/workflows/ci.yml` lists exactly those two commits.

The file has never existed at a release tag: it is absent from `app-v0.4.0`, `app-v0.5.0` and
`app-v0.6.3`. No commit message states a reason.

**Conclusion: CI never ran.** `CLAUDE.md`'s "temporarily disabled" reads as though it once worked;
it did not. Treat restoring it as new work whose first run will be red, not as reverting a
regression. The most likely trigger is that `cargo clippy --all-targets -- -D warnings` or
`cargo test` was already failing on the `windows-latest` / `macos-latest` matrix — compare
`findings/blocker-build-cargo-test-red-on-windows.md`.

The deleted file, verbatim (`git show e1d5d28^:.github/workflows/ci.yml`):

```yaml
name: CI

on:
  pull_request:
  push:
    branches: [main]

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true

jobs:
  frontend:
    name: Frontend (build + lint)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: pnpm
      - run: pnpm install --frozen-lockfile
      - run: pnpm build
      - run: pnpm lint

  frontend-audit:
    name: Frontend (audit runtime deps)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: pnpm
      - run: pnpm install --frozen-lockfile
      - run: pnpm audit --audit-level=high --prod

  rust:
    name: Rust (${{ matrix.os }})
    strategy:
      fail-fast: false
      matrix:
        os: [windows-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: pnpm
      - run: pnpm install --frozen-lockfile
      - run: pnpm build
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: src-tauri
      - name: Format
        working-directory: src-tauri
        run: cargo fmt --check
      - name: Clippy
        working-directory: src-tauri
        run: cargo clippy --all-targets -- -D warnings
      - name: Test
        working-directory: src-tauri
        run: cargo test

  cargo-audit:
    name: Rust (cargo audit)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: taiki-e/install-action@v2
        with:
          tool: cargo-audit
      - working-directory: src-tauri
        run: cargo audit

  blob-checksums:
    name: Verify vendored binary checksums
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - working-directory: src-tauri/binaries
        run: sha256sum -c SHA256SUMS
      - working-directory: src-tauri/resources
        run: sha256sum -c SHA256SUMS
```

Note that some of these steps would need updating before they pass today: `0f745d8` (2026-09-05)
removed the `pnpm approve-builds` step from setup docs in favour of `allowBuilds` in
`pnpm-workspace.yaml`, and `be6e695` pinned pnpm to 10.26.1 — `pnpm/action-setup@v4` with no
version will read `packageManager` from `package.json`, so that part is fine. `bef5f89` removed
`.github/dependabot.yml`, which `c3825d5` had added alongside this file; per project memory,
dependabot must not be reintroduced.

---

## 5. Reverted and abandoned work

**No commit anywhere reverts another.** `git log --all -i --grep='revert|rollback|back out|undo'`
returns nothing, and there is no `Revert "..."` subject in 412 commits. The one behavioural
reversal in the history is implicit: `374a6c1` silently undid `ea12e0b`'s
`jitterBufferTarget = 0` on the fallback path (§3).

**`origin/highdpi` — fully merged, safe to delete.** Its tip `f85701d` "feat: render virtual
displays at the client's native pixel ratio" *is* the merge base with `main`, and
`git log origin/highdpi ^main` is empty. It was merged at `f83173f` (PR #35). It contains nothing
that never landed. `main` is 55 commits ahead.

**`origin/sys-audio-winmac` — superseded, but not identical.** Tip `bba3711` "feat: hardening work
by agent, reviewed but needs final pass". Forked at `e1d5d28`; six commits not on `main`
(`dcf3dc3`, `420004a`, `ea12e0b`, `f9e2312`, `374a6c1`, `bba3711`). Its code was re-landed on
`main` as the cleaner `f343fa1` / `c7d7cff` / `407df89` / `bb1de86` series. Every perf hunk from
`ea12e0b` and `374a6c1` was individually checked against the working tree and is present. What did
**not** come across is `PRD.md` and `AUDIO_NOTES.md` (§2). Do not delete this branch until those
are committed from `docs/recovered/`.

**Dangling work that never landed anywhere.** ~55 dangling commits, mostly abandoned dependabot
branches, plus:
- `67687cb` "temp commit" (2026-09-02) — sole copy of `docs/airplay-fallback-design.md`.
- `c5bf2afe` (2026-09-04) — the monaco warm-up commit whose code landed and whose measurements did
  not.
- `81cfd8b5` (blob) — "Commit plan", the A–M split recipe for the system-audio feature; the only
  map from the branch's four commits to `main`'s cleaner series.
- `35d0205c` — a literal `git stash` entry, "WIP on main: f02fc51".
- ~25 blobs of macOS legacy-audio source whose `//!` headers cite `PRD-macos-legacy-audio.md` and
  `PRD-macos-audio.md`, documents that were never committed. Those headers are the closest thing to
  a spec for `macos_utils/audio/legacy/` that exists anywhere.

All of this is deleted by `git gc --prune=now` or by git's automatic gc, silently and
unrecoverably. Filed as `major-build-recovered-docs-live-only-in-dangling-objects.md`.

---

## 6. Hygiene sweep — AI tooling, PRD, M1/M2, §

**AI tooling.** No source comment anywhere references a model, an assistant, or a generator. No
commit carries a `Co-Authored-By` trailer naming one. Four commit messages disclose agent
authorship, three of them on `origin/sys-audio-winmac` only and therefore not in `main`'s history:
`dcf3dc3` "feat: add system audio support (to be reviewed)", `420004a` "feat: more agent work for
audio, need to be reviewed", `bba3711` "feat: hardening work by agent, reviewed but needs final
pass". The fourth, `d5e3ea0` "chore: ignore agent scratch directories", is on `main` and its
`.gitignore` entry is legitimate. `amd.md:196` contains a related warning worth keeping: read the
vendored AMF headers first, "APIs get hallucinated otherwise".

**PRD / § / M1 / M2.** 35 `§` occurrences across 6 files, and the `M1/M2` markers in
`macos_utils/streamer/`. The full `file:line` inventory, classified into restore / repoint /
delete, is in `findings/minor-docs-stale-reference-sweep-list.md`. The two that matter most:
- `macos_utils/streamer/mod.rs:38` — `#[error("{0} not yet implemented (M1/M2): see PRD {1}")]`.
  This is a **user-visible error string** pointing at a milestone scheme and a PRD that ship
  nowhere. The variant is also never constructed (`grep -rn 'NotImplemented' src-tauri/src`
  matches only the declaration), so it can simply be deleted.
- `macos_utils/streamer/sck.rs` — 22 `PRD §5.x` / `§1.5` citations. No macOS video PRD ever
  existed. The numbers do map onto real subsections of the module's own doc comment, so renumber
  them as local references.

**A pattern, not a one-off.** Comment-stripping has removed measurements from this codebase at
least twice, in both cases from the commit *after* the one that took the measurement:
- `a56eedc` deleted three rationale comments that `450778a` had added one commit earlier, about
  the BGRA-CSP probe, the bounded convert pool, and why its workers run `ABOVE_NORMAL` while the
  pool is nominally below-normal — while changing none of that code
  (`minor-x264-perf-rationale-comments-deleted-by-the-next-commit.md`).
- The monaco numbers in §3 (`major-ui-monaco-preload-measurements-stripped-from-the-tree.md`).

Any hygiene sweep over the streaming code should treat a comment containing a number as
load-bearing until proven otherwise.

---

## 7. For the orchestrator

0. **`.gitignore:44` ignores all of `docs/`, and it is eating this engagement's output right
   now.** `git status --porcelain` is empty even though every discovery report and all five
   recovered documents are on disk. Narrow that entry before anything else is committed; it is
   also the root cause of the missing AirPlay design doc (§2.1). One line, two minutes of work,
   and everything else in this report depends on it.
1. **The history has almost no measurements in it.** Ten commits are explicitly about latency;
   *none* cites a before/after number, in the message or in a comment. The only hard numbers this
   project ever recorded are in `AUDIO_NOTES.md` (deleted), the monaco block in `c5bf2afe`
   (dangling), and `amd.md`'s test methodology. This overhaul is starting from a baseline that
   does not exist, and §16 — the "photographic method" `amd.md` treats as canonical — is defined
   in no document in the repository.
2. **Two branch-only perf commits were verified re-landed.** `ea12e0b` and `374a6c1` are not
   reachable from `main`, but every hunk of both is in the working tree. No action needed beyond
   knowing that `git log main` will not show where those constants came from.
3. **`374a6c1` changed the jitter buffer on only one of the two client render paths** and said
   nothing about it in its message. That divergence is live and worth measuring first, since it is
   30 ms on the fallback path.
4. **The window on the dangling objects is open now.** Committing `docs/recovered/` and tagging
   `67687cb` and `c5bf2afe` costs two commands and closes it permanently.

Findings written: 10.

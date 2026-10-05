# REPORT — the overhaul-new job

`TEST-WORKFLOW.md` §10. This report compares three builds:

- **old**: the old app, `bef5f89` (v0.6.3).
- **previous head**: the head of `latency-overhaul`, `d4d8be9`. `BASELINE.md` measured it as `06e6fbb`, which is `d4d8be9` plus the test scripts and the inert isolation switch.
- **new**: `latency-overhaul-new` after the nine group merges and consolidation.

All of it ran on one machine: Windows 11 26200, RTX 3080 Ti Laptop (hybrid graphics), headless Chrome 154 on the same machine. Every number comes from `scripts/test-handler/handler.mjs`. Each number names the ledger it came from, in `docs/overhaul-new/`. No claim here is stronger than its ledger's.

Two conditions bound everything:

- **The memory emergency.** The virtual display driver's host process (`WUDFHost.exe`) leaks about 47–124 MB of commit for every display created or mode changed, and gives almost none of it back. It reached about 34 GB during the job. From 06:05 on 10/5 each agent was rationed to about 15 display creations or mode changes. Many confirmation rounds were cut as a result, and each one is recorded below as untested, with the exact handler request that would settle it. Only a driver device restart or a reboot frees the memory. Both need the owner (admin).
- **A shared machine.** Up to nine other agents were building and measuring at the same time. A load gate (`53d6701`) discards windows taken above 75 % CPU.

---

## 1. Verdict

On three interleaved rounds (n=4–6 windows per build and scenario), `latency-overhaul-new` is **level with the old app and with the previous head on per-frame latency in every video scenario** (1080p small window 25.9 ms against 25.6 and 27.0; large area 28.4 against 28.9 and 29.2; hi-res 2560×1440 at a real 120 Hz 34.3 against 34.9 and 35.0), with p99 and painted fps not worse anywhere. It is **faster where the job found real defects**: submit → first picture 2.9–3.2 s against 7.2–8.0 s (previous head) and 7.4–10.6 s (old app), submit → answer 2.7–3.0 s against 4.6–4.9 s and 7.3–7.7 s, and audio click → rendered 62.8 ms against 92.3 ms on the previous head (one run each; the old app played none of the clicks). No hyper-fast version was found hiding in the 442 commits: on a same-machine headless join the old app, the previous head and the new build spend the same time per frame. The new build's numbers are from a thin-LTO build taken under memory pressure; the fat-LTO re-measure is owed.

What the baseline already established (`BASELINE.md`, "Verdict" and the hr1 table):

- **At 1080p the previous head was level with the old app.** Small-window p50 was 27.2 vs 26.5 ms and large-area p50 29.4 vs 29.6 ms. Every range overlapped in 4 of 4 interleaved pairs.
- **High resolution depends on the refresh rate.** The high-resolution client is 2560×1440 at DPR 2. At 60 Hz the previous head read slower in 3 of 4 pairs: p50 39.4 vs 31.9 ms. That is not established, because the quietest pair read 32.7 vs 31.8. At the 120 Hz such a client really asks for, the old app collapses: p50 rose 48 → 127 → 137 ms over three windows, with gaps up to 392 ms. The previous head held at 35.6–37.8 ms. That is one clean pair.
- **Joins were about twice as fast on the previous head.** Submit → answer took 3.1 vs 6.2 s for a later join and 4.6 vs 8.8 s for a host's first join.
- **Both builds had a keyframe storm** of about 3 IDRs a second (D-01).
- **The old app's audio did not play at all.**

What this job changed on top:

- The keyframe storm is cut from about 430 IDRs per session to about 72 (G03, G05).
- A host's fresh join is about 0.55 s shorter (G04).
- A browser's first offer leaves about 1.8 s sooner (G07).
- System audio arrives about 29 ms sooner (G06).
- Defect fixes target the owner's two-devices report, the microphone prompt, sound over HTTP, the `<video>` demotion, the quality slider and the logs.

None of these was a measured latency win on the stamp metric by itself on loopback. The latency effects either were inside the noise or were not measurable there.

---

## 2. The three-way table

Three interleaved rounds, all three builds in exclusive mode, `stamp_band: "center"`, two 20 s windows per scenario per round; no window marked loaded, every join one offer; free physical memory 2.6–4.7 GB throughout. Source: `CONSOLIDATION.md`. **The new build is thin LTO (16 codegen units); the previous head's binary is fat LTO.** All builds ran CBR with no constant QP (12.44 Mbps at 1920×1080@60, 30 Mbps at 2560×1440@120, a real 120 Hz with a fresh device token and no saved override). The one configuration difference: the old app always answers H.264 Baseline; previous head and new negotiate High.

| scenario | old `bef5f89` p50 ms (range), n | previous head `d4d8be9` p50 ms (range), n | new p50 ms (range), n | verdict |
|---|---|---|---|---|
| small window 1080p | 25.63 (1.39), 6 | 27.02 (1.82), 6 | 25.90 (2.52), 4 | level with both |
| large area 1080p | 28.92 (1.26), 6 | 29.20 (1.51), 6 | 28.41 (2.17), 4 | level with both |
| hi-res 2560×1440@120 | 34.86 (5.45), 6 | 34.98 (0.85), 6 | 34.30 (1.17), 6 | level with both |

| other measure | old | previous head | new |
|---|---|---|---|
| hi-res p99 | 53.7 ms | 50.1 ms | 48.0 ms |
| hi-res painted fps | 86.0 (range 74–88) | 81.2 | 82.5 |
| duplicate frames per 20 s window, 1080p | not reported | 50–65 | 1–6 |
| duplicate frames per 20 s window, hi-res | 627–857 | 330–427 | 294–364 |
| join: submit → answer | 7.3–7.7 s | 4.6–4.9 s | 2.7–3.0 s |
| join: submit → first 1080p picture | 7.4–10.6 s | 7.2–8.0 s | 2.9–3.2 s |
| audio: click → rendered p50 (one run per build) | 0 of 15 clicks matched | 92.3 ms | 62.8 ms |

Not in this measurement (not run; memory): calibration per build, the static scenario, p90/max per scenario, PLI/IDR counts per window, rounds four and five, and the fat-LTO build of the new head. One of the three new-build rounds lost its 1080p join's four windows (see §7), which is why n=4 there.

### 2.1 What the baseline measured (old vs previous head)

From `BASELINE.md`, "Old vs current". The previous head was measured as `06e6fbb`.

- **Runs:** four interleaved pairs (std2–std5) taken from 04:43 to 12:00 UTC. The 1080p client was 1920×1080 CSS, DPR 1, 60 Hz. The hi-res client was 1280×720 CSS at DPR 2, which claims 120 Hz but **ran at 60 Hz on both builds** (harness fault H-3).
- **Format:** median [min–max] (n). Latency is in ms, measured from host draw to client paint call. Calibration floor was p50 0.5–0.7 ms.
- **Excluded:** std1 ran at 92–99 % CPU. One previous-head large-area window was marked loaded.

| scenario | metric | old `bef5f89` | previous head `06e6fbb` | difference |
|---|---|---|---|---|
| calibration | p50 / p99 | 0.65 [0.6–0.7] / 1.15 [1.1–1.2] (4) | 0.65 [0.5–0.7] / 1.15 [1.0–1.2] (4) | none |
| static desktop 30 s | frames with any change | 0.5 [0–55] (4) | 0 [0–0] (4) | one old run had 55 one-sample changes |
| | frames with new green samples | 0 [0–2] (4) | 0 [0–0] (4) | not reproduced on the previous head |
| | idle painted fps | 4.95 [4.9–12.6] | 6.05 [5.9–7.7] | both about 5 fps keepalive |
| small window | p50 | 26.46 [24.80–28.10] (4) | 27.16 [24.47–32.06] (4) | overlap |
| | p90 | 33.19 [31.79–33.50] | 32.83 [32.36–36.00] | overlap |
| | p99 | 35.86 [34.84–42.05] | 36.79 [35.23–43.71] | overlap |
| | painted fps | 54.3 [53.0–54.7] | 54.65 [53.0–57.8] | overlap |
| | longest gap ms | 63 [62–81] | 71 [63–92] | overlap |
| large area | p50 | 29.64 [27.93–31.20] (4) | 29.40 [27.96–32.47] (3) | overlap |
| | p99 | 41.30 [38.05–70.40] | 43.69 [37.75–45.69] | overlap |
| | painted fps | 54.35 [53.5–54.7] | 53.4 [52.4–53.9] | overlap |
| hi-res small window (60 Hz) | p50 | 31.89 [30.29–32.04] (4) | 39.38 [32.71–40.62] (4) | previous head slower in 3 of 4; not established |
| | p99 | 46.99 [46.57–124.08] | 58.30 [48.11–59.22] | same pattern |
| | painted fps | 59.85 [59.2–59.9] | 58.65 [57.8–59.7] | |
| | longest gap ms | 32 [32–111] | 59 [32–63] | |
| PLIs per 20 s window | small window | 56–61 | 64–83 | keyframe storm on both |
| join, first of a host | submit → answer ms | 8787 [8483–12351] | 4615 [4293–6916] | about 2× faster |
| join, later | submit → answer ms | 6175 [5894–6348] | 3117 [3013–3334] | about 2× faster |
| `slow_answer`, 10 joins, quiet | answer s | 5.56 [3.53–7.51] | 2.48 [2.42–4.66] | |
| driver leak per join | WUDFHost commit | +93.9 MB per join (one join) | +47.5 to +94.9 MB per join | the same: the leak is in the driver |

**The one clean pair at a real 120 Hz (hr1).** Both builds ran at 2560×1440@120 with CBR 30 Mbps. The old app used Baseline and the previous head used High + CABAC. Values are per window.

| window | old p50 / p99 / fps / longest gap | previous head p50 / p99 / fps / longest gap |
|---|---|---|
| 1 | 47.8 / 146.3 / 75.5 / 236 ms | 35.6 / 59.7 / 75.3 / 40 ms |
| 2 | 127.3 / 163.5 / 62.1 / 217 ms | 36.0 / 55.5 / 78.5 / 48 ms |
| 3 | 136.9 / 177.3 / 54.2 / 392 ms | 37.8 / 63.3 / 74.8 / 38 ms |

### 2.2 The noise figure

Two noise figures exist, and they disagree:

- **Handler acceptance runs** (`scripts/test-handler/README.md`, acceptance 3): five consecutive standard runs on a loaded machine at 82–90 % CPU. Small-window p50 range was 1.48 ms, large-area 2.31 ms and hi-res 7.90 ms. p99 ranges ran into tens of ms.
- **The baseline's quiet runs** (`BASELINE.md`, "Noise figure"): four previous-head runs spread over seven hours. Small-window p50 range was 7.6 ms (24.5–32.1), large-area 4.5 ms and hi-res at 60 Hz 7.9 ms.

The noise figure with `stamp_band: "center"` was never measured.

No separate noise figure was measured on the final build. The run-to-run ranges in the three-way table (1.2–2.5 ms on p50 for the new build, three rounds, `stamp_band: "center"`) are the best available figure. About 10 % of 1080p frames still failed the stamp checksum with `center` on all three builds, so the handler README's claim that `center` removes them is unconfirmed (`CONSOLIDATION.md`).

---

## 3. Every change kept

Each change below gives its commit, what it does, the measured effect, and its status **as its ledger gives it**. "Thin" means a thin-LTO, 16-codegen-unit experiment build. The real fat-LTO profile was the consolidation agent's job alone.

The per-merge five-run re-measure that `TEST-WORKFLOW.md` §8 asks for was **not done**: joins were rationed because of the driver leak, so the groups were merged as they finished and measured once, together, at the end (the three-way table in §2). No group's individual effect was re-confirmed after merging except where noted in this section. Three cross-group defects were fixed during consolidation (`CONSOLIDATION.md`): `fb6d08b` — a superseded session's teardown no longer removes the live session's telemetry (`unregister_if` with `Arc::ptr_eq`; unit test; verified end to end on one run: after an encoder-only edit `status --json` still listed the join's telemetry); `9433732` — the four inline `session.stop()` calls and one teardown branch in `server.rs` now run on the blocking pool (compile-checked only; not shown to cure the lost `/whep` answer); `d5ea62a` — client audio path 4 is named `worklet+port`, with a test that every `AUDIO_PATH` value has a name. Also verified on one run: G03's `1f85421` (the superseded transport closed in the same millisecond the new one connected; same join and display; picture back in 855 ms). `slow_answer` n=5 on the final build: 0 flagged, answers 1.98–2.21 s.

### G01 — Windows capture (`G01.md`)

| commit | what it does | measured effect | status (ledger) |
|---|---|---|---|
| `48d3807` | Retries a WGC start that fails on a stale monitor handle. It waits 200 ms, finds the display again by name, and tries up to 5 times before falling back to DXGI, which never delivers on this hybrid machine. | Unforced, the failure cost a second offer and a first picture at 12.9 s (1 of 48 logs). With the failure forced three times, it recovered on WGC: picture at 5.6 s, small-window p50 27.0 ms. | VERIFIED (forced): verified by simulation. The natural failure was never reproduced on demand. |
| `f7c2a55` | Tests `CURSORINFO.flags` as a bitfield instead of with `==`. | Same behaviour for every documented value. | COMPILE-CHECKED: not run. Nobody may move a pointer. |
| `ea821a9` | Sets WGC `MinUpdateInterval` back to ⅔ of the stream's period, never under 4 ms. That is the old app's value; it replaces a flat 4 ms. | One display: level (E3). Two displays attached: WGC deliveries to a 60 Hz session fell from about 190/s to about 70/s at the same latency (E5, one run per arm). As a side effect, cursor-exempt frames are now bounded at 1.5× the stream rate (about 90 fps for a 60 Hz client). | VERIFIED (one run per arm). The confirmation run was contaminated. `CONSOLIDATION-TODO.md` asks for an A/B before the change is called kept, including at hi-res 120 Hz, where one run on `d7a448a` read p50 73.5 ms with 810 duplicate frames, unattributed. Consolidation: no A/B against the parent was run. The 73.5 ms run did not reproduce — hi-res at 120 Hz on the merged build read p50 34.30 ms (range 1.17, n=6), level with the old app and the previous head. The two-display confirmation is still owed. |

### G02 — Windows encode (`G02.md`)

| commit | what it does | measured effect | status (ledger) |
|---|---|---|---|
| `cf63279` | Makes NVENC accept constant QP. `supports_rc` tested `rc_modes & 0`, so constant QP was never selected, and every quality-slider session silently ran CBR. | Live sessions now log `rc=NV_ENC_PARAMS_RC_CONSTQP (qp=15)`. Hardware test `nvenc_constant_qp_request_is_applied`: a QP 18 IDR is more than 2× the size of a QP 42 IDR. On loopback, CBR vs CQP 15 made **no latency difference** (E5b: small p50 26.99 vs 26.95, n = 3 vs 6; E6 hi-res: 34.2 vs 33.8). | Kept as a **defect fix, not a speed-up**. Verified by log line and a hardware test. |

### G03 — Transport (`G03.md`)

| commit | what it does | measured effect | status (ledger) |
|---|---|---|---|
| `30d5719` | `PliBackoff`: when PLI/FIR requests keep coming after they are answered, the window between answered requests doubles, up to a 2 s cap. Four quiet seconds reset it. A NACK, transport-wide loss or the client's own keyframe counter opens it at once. | 1080p, 3 interleaved pairs: IDRs per session 426–435 → 72–73. Small-window bitrate about 5.1 → 2.7 Mbps. Small p50 pair medians 27.4/27.8/27.7 → 25.3/25.7/24.3. | VERIFIED: defect fix; the latency gain is inside the noise. |
| `396916b` | Raises the cap to 10 s while the client's STATS say it is painting through WebCodecs. | 120 Hz hi-res, 1 pair × 3 windows: IDRs 54 → 17. Hi-res small p50 35.5/37.3/40.7 → 34.3/35.4/37.0. fps 72–76 → 79–84. | VERIFIED as a defect reduction, one pair. |
| `07ec295` | Lets the audio DataChannel carry one delayed-SACK interval (220 ms) of in-flight data on top of the 40 ms before backpressure drops audio. | Reasoned from source reading and G06's burst sizes. | UNVERIFIED. |
| `1f85421` | D-05: closes the transport a renegotiation replaced once the new one connects. A registry entry is removed only by its owner. | Before the fix, the old transport was still alive 30 s after each edit (one run). The after-fix run was reaped for memory. Unit test. | UNVERIFIED end to end. |

### G04 — Session and display (`G04.md`)

| commit | what it does | measured effect | status (ledger) |
|---|---|---|---|
| `f789b76` | Fix 1: a failed renegotiation keeps its session and display for the page's retry. A failed fresh join removes its display. A superseded attempt stands down at two more checkpoints. | Injected failure, same binary, 1 + 1 runs. Head behaviour: displays 1 → 2, a new join id, an extra dashboard row, and an orphaned display. Fixed: displays 1 → 1, the same ids. | Kept (defect). Verified by simulation. |
| `97e46d1` | Fix 4: skips the forced mode set when a fresh display already reports the requested mode, writes the DPI concurrently with capture start, and no longer awaits the `monitor_rect` log line. | 2 + 2 interleaved runs, 7 fresh joins each: host request → answer p50 2686/2617 → 2094/2097 ms. Small p50 28.99/27.05 → 26.98/26.16. | Kept (verified by A/B). Round 3 was cancelled under the ration. |
| `58fd8b1` | Fix 3: the refresh rate recorded at a join is a detection, not a setting, until the user saves one. | Hi-res with a reused token: pinned 2560×1440@60, 57.5/57.4 fps, p50 38.0/38.4. Fixed @120, 72.5/86.1 fps, p50 42.1/35.1 (one fixed window p99 124 ms). | Kept (defect). fps up; p50 inconclusive. macOS mirror UNVERIFIED (not compiled). |
| `8e709bd` | Fix 5: a relayed page without join ids renegotiates on its live join, and a relay client resolves to its live join for `/reconfig`. | None. | Unit-tested only (`a_relayed_page_without_join_ids_lands_on_its_live_join`). Never run. |
| `26630dc` | Fix 6: the GPU-encode toggle renegotiates live joins in place instead of restarting the servers. `RELOAD` applies `disableGpuEncode`. | One run through `RELOAD`: same join id, x264 then NVENC, no display created. | Host side measured once. The GUI button was never pressed. |
| `1eca751` | Two `log::error!` calls in `driver_ipc/sync/client.rs` now reach the log bus. `Drop` no longer reports a panic on every normal drop. | None. | By reading. |
| `7584269` | Host half of the per-page `attemptId`. A joinless retry whose attempt names a join that never connected continues that join. | Simulated lost answer (switch held the first answer 12 s), 1 run: displays 2 → 1. The retry "continues join…", renegotiated in place in 0.9 s. Small p50 24.55 ms. | Verified by simulation (1 run) plus 3 unit tests. |
| `183aea8` | D-08: an orientation edit counts only when auto-orientation is off. | None live. | Unit-tested only. |

### G05 — Client video path (`G05.md`)

| commit | what it does | measured effect | status (ledger) |
|---|---|---|---|
| `9aae1d3` | D1: the frame watchdog never demotes a session to `<video>` once WebCodecs has painted. | Previous head: 2 of 56 runs demoted after painting. Fixed: 0 demotions in 23 joins. The regression test fails 4 of 7 assertions on the old page. | Harness-proven; live-unverified. A confirmation needs tens of joins. |
| `a00083d` | D3, the client half of the storm: keyframes go back to the receiver, so libwebrtc's own decoder stops asking for them. | PLIs 28–35 → 3–4 per 10 s. Medians base vs fixed: small 26.95 vs 24.8, large 30.7 vs 29.5, hi-res 32.9 vs 32.05, all inside the base range. Encoder load at hi-res 40–43 % → 36 %. | Kept as a defect fix (measured); not a latency win by the keep rule. |

### G06 — Audio (`G06.md`)

| commit | what it does | measured effect | status (ledger) |
|---|---|---|---|
| `c020fc0` | No microphone card or output chooser on the join. Where nothing routable enumerates, the device sheet gets one entry, "Choose a speaker on this device…" (`se:choose`), and only picking it asks the client. | `mic_prompt` with nothing granted: the card on every join, with the join waiting on it → no card, no `getUserMedia`, no permission query. | Fixed, before/after on the same request. |
| `61da27a` | A refused or stale saved output no longer reroutes the graph every poll. | `stale_output`: 22 element restarts, 4 underruns → 0, 0. | Fixed, before/after. |
| `24f75e8` | A page opened on the host machine itself starts muted, which ends the echo loop. | Chrome unmuted: 1/15 clicks detectable, 0.40 RMS still ringing → page muted, no loop. | Fixed, 1 + 1. |
| `736f0f9` | The spare Opus track is fed once the client steps down to it. | `worker_fails`: 0/15 clicks → 15/15 together with `201a981`. | Fixed, before/after. |
| `201a981` | The standard track's RTP clock steps in whole frames. This is the HTTP / no-`AudioDecoder` tier. | `no_audio_decoder`: 627,600 concealed samples, 5/15 clicks, p50 453 ms → 240 concealed, 15/15, p50 131 ms. | Fixed, before/after. Proven through the same track path with `AudioDecoder` removed, **not on plain HTTP itself**. |
| `83facec` | A refused host-chosen output is reported, not offered as a chooser over the live stage. | Harness: 10 polls → 0 picker offers, 1 report. | Harness only. |
| `dbb2863` | Process loopback is tried first; endpoint loopback when it cannot activate. `SE_ENDPOINT_LOOPBACK=1` forces the old order. | Probe 5 + 5: median 80.6 → 59.4 ms. Handler 2 + 2 interleaved: click → rendered 95.1/93.9 → 65.9/64.9 ms; → output 149.1/149.3 → 120.2/120.4. 15/15 every run. | Kept (verified by A/B). The second handler pair ran at about 1 GB free physical memory. |

All of G06 ran on thin builds. A fat-profile re-run is owed.

### G07 — Client page (`G07.md`)

| commit | what it does | measured effect | status (ledger) |
|---|---|---|---|
| `cd611ce` | `warmJoinPath()`: at page load, runs one `enumerateDevices()` and one throwaway `RTCPeerConnection`. Never awaited, never prompts. | First-join POST median 1920 → 116 ms (3 v 3, ranges 715 and 85). First paint 4728–5198 → 2904–4057 ms. Small p50 unchanged. | VERIFIED. |
| `2dda493` | A lost answer, a 5xx or a dropped connection is offered again once on the **same** tier instead of being demoted to `<video>`. | Simulated lost answer: before, the retry ran on `<video>`, path 3. After, the retry ran on WebCodecs, path 1. | VERIFIED by simulation. |
| `aec25fb` | `pagehide` releases the held join in any state. | None. | COMPILE-CHECKED + harness. |
| `e8cbe87` | Every offer carries a per-page-load `attemptId`. The host half is `7584269`. | None alone. | COMPILE-CHECKED + harness. |

### G08 — Telemetry (`G08.md`)

| commit | what it does | measured effect | status (ledger) |
|---|---|---|---|
| `e96fe2c` | The ring keepalive's encode stamps go around the encode, not after it. | — | Defect by reading. |
| `3120abf` | WGC's `SystemRelativeTime` is no longer used as a capture time. It is a target present time, 3–14 ms in the future on 2,879 of 2,880 frames. | Capture-relative stages had been dropping 89–99.97 % of their samples. | Measured with a diagnostic switch. Not re-run through the handler on the final tree. |
| `114cc01` | The worker's stage windows are sorted as typed arrays. | 320–350 → 77 µs per 1 Hz poll; end to end inside noise. | Direct timing. |

Instrumentation all off vs on showed no measurable cost (3 v 3 joins, `G08.md` §2).

### G09 — Host core (`G09.md`)

| commit | what it does | evidence | status (ledger) |
|---|---|---|---|
| `304cf95` | The log bus decides whether to emit under the lock the panel arms its stream under. | Race test: 20/20 failures on the old shape, 0/20 fixed. | Unit test. |
| `8e4db79` | `ScreenExtend logs` no longer arms the desktop panel's stream forever. | Code. | By reading. |
| `3032603` | A control reply is decoded once, not per 4 KiB read, so no U+FFFD appears. | Unit test. | Unit-tested only. |
| `18311e0` | Avatar errors reach the log bus. | — | By reading. |
| `8423abf` | A `-v` echo to a closed pipe no longer panics the logging thread. | `pipe_close`: head lost `logs` and `stop`; g09 kept both. | Verified by handler reproduction, 1 + 1. |
| `52c3eb3` | `serve` starts its control server before its adapter servers. | Head: `/health` at 399 ms but control server at 698 ms. g09: telemetry at once. | Verified by handler reproduction, 1 + 1. |

### Handler and tooling commits (all `scripts/test-handler/`)

These are tooling, not product changes:

- isolation and harness: `655e828` (the `SCREENEXTEND_DATA_DIR` isolation switch), `dcc4311`, `53d6701`, `b5c4225`, `7870de8`, `80e00a4`
- stamp reading and staging: `8377723`, `ef8b923`, `2af1791`, `60b3d38`, `4ff3e74`, `8a8bcf3`, `df1f0c7`, `e8220e5`, `e8cd615`
- request keys and probes: `8009d5b`, `17cd207`, `35b98e9`, `55c7d99`, `c809c26`, `da81ab3`, `cdc34a8`, `d7f52a6`, `aa6a430`

The fixes among them that changed what earlier numbers meant are listed in `BASELINE.md` "Harness faults" (H-1 to H-7).

---

## 4. Every change tried and not kept

| group / # | change | runs | numbers | why not kept |
|---|---|---|---|---|
| G01 E1 | cadence gate off when the display runs at the stream's rate | 3 runs, one valid A and one B | B, with two displays attached: small p50 777 ms, ring dropped 187 frames | 30× worse with two displays. On one display the gate drops about 0.5 frames/s: nothing to win. |
| G01 E2 | scheduled cadence grid | 0 | — | Not run: nothing to win on one display, and the ration went elsewhere. |
| G01 E3 | WGC 12 ms vs 4 ms, one display | 1 + 1 (2 windows each) | small 25.2/23.3 vs 25.3/25.8; large 28.0/26.8 vs 29.3/29.1 | No difference on one display. The ⅔-period form was kept for the two-display case (E5). |
| G01 E4 | GPU scheduling class 2, and P-core confinement off, at hi-res | 1 join × 3 windows per arm | medians 35.3 (A) / 34.6 (class 2) / 36.0 (P-core off); ranges 2.2–3.0 | ±0.7 ms inside 2–3 ms ranges. All three arms ran at 60 Hz with CBR. |
| G02 E2 | async NVENC vs `SE_NVENC_NO_ASYNC=1` | bench 6 + 4; handler 1 loaded pair | clean NVENC p50 1080p 2.3/3.3 vs 3.5/3.2 ms | Inconclusive (bimodal GPU clock). Left as shipped for its bounded wait. |
| G02 E3 | `lowDelayKeyFrameScale` 1 (old app) vs 2 | bench ×2; hi-res 1 join × 4 windows each | IDR 42.3 → 24.5 KB (1080p), 72.6 → 43.3 KB (1440p); hi-res p50 32.7 vs 32.9 | No latency effect on loopback. The byte saving is real on a real link. |
| G02 E4 | Baseline/CAVLC (old app) vs High/CABAC | bench ×2; hi-res 4 windows | Baseline P frames +91 % bytes; hi-res p50 34.2–39.8 vs 32.5–34.0 | Not faster; 55–90 % more bytes. |
| G02 E5/E5b/E6 | CBR vs constant QP 23 / 15 | E5b 1 v 2 joins; E6 3 rounds | small 26.99 vs 26.95; large 30.86 vs 29.95 (p99 +7); hi-res 34.2 vs 33.8 | Not a latency lever. No default changed; the E1 fix stays as a defect fix. |
| G03 3 | stock `H264Payloader` vs own packetizer | 1 | 35.0–41.6 vs 32.6–34.1 ms, but at 61–66 % vs 31–41 % CPU | Inconclusive (load). |
| G03 4 | no transport-cc stamping | 1 | p50 46 → 510–1216 ms; REMB about 2.2–3.7 Mbps | Destructive. |
| G03 5 | Baseline profile at hi-res 120 Hz | 1 | 38–55 ms | Inconclusive: no matched 120 Hz default arm. |
| G03 7 | `get_stats` every 120 ms | instrumented | 85–203 µs p50 per call | No change needed. |
| G04 E4 | reap an answered join that never sends a byte | 0 | — | Reverted unmeasured: the lost answer could not be triggered on demand. |
| G04 `2dba8b9` | correlate a new display by friendly name | 136 attaches | named directly 0 of 136 | Inert here (every EDID name is `VirtuDisplay+`). Left in, not counted as a win. |
| G05 f01 | backlog policy off | 1 | small 24.0/27.1, hi-res 29.5/33.1 | No effect: the policy dropped 0 frames in every window. |
| G05 e02/e06 | every frame handed back to the receiver | 2 | PLIs 0; libwebrtc decoded every frame twice; e02 5 of 6 windows loaded | Discarded in favour of keyframes only. |
| G05 k02 | the old page's worker under the new page | 1 | hi-res 35.5/34.2/32.7 vs base 34.1/34.1/33.9 | No difference. The worker is not the hi-res gap. |
| G05 k03 | legacy `createEncodedStreams` transport | 1 | 34.0/32.8/32.3 | No difference. |
| G05 D2 | G05's own `setupRetries` | — | — | Dropped on rebase; G07's `2dda493` is kept, the better rule (retries 5xx only). |
| G06 E3 | raw vs clamped `qpcpos` capture stamp (`dfd5dc9` vs `f36f2ec`) | 2 + 2 | render p50 91.6/101.5 vs 92.0/93.3 | No effect: the jitter target sits at its 10 ms floor. |
| G06 E8 | forced `FLAG_REDUNDANT` cost | 0 | — | Not run (budget). |
| G07 2 | prewarm caps, `/ice-config`, worker source | 1 cold + 3 warm | saves about 70 ms of caps only | Not the cost. |
| G07 9 | reconfigure poll 1000 → 250 ms | 0 | — | Not run (budget). |
| G08 §2 | all instrumentation off | 3 v 3 joins | OFF 1.4–2.5 ms *slower* on p50 | No cost to remove. The OFF slowdown is unexplained (§7). |
| G09 E5 | startup old vs head | 3 + 3 | `/health` medians 454 vs 897 ms, ranges overlapping | Inconclusive. |
| G09 E7 | `bench_push_line` | 0 | — | Not run; the measured line rate makes it irrelevant. |

---

## 5. The bug census

### 5.1 The owner's six complaints

**1. "Slower."**

- **Found.** `BASELINE.md` O-1: at 1080p the previous head was level with the old app. At hi-res and 60 Hz it was slower in 3 of 4 pairs, which is not established. At a real 120 Hz the old app collapses and the previous head holds. Joins were about 2× faster. Both builds carried a keyframe storm of about 3 IDRs/s, and the previous head could demote itself to `<video>` (D-04).
- **Fixed.**
  - The storm, three ways: `30d5719` and `396916b` on the host, `a00083d` on the client.
  - The demotion: `9aae1d3`.
  - A fresh join, about 0.55 s shorter: `97e46d1`.
  - The first offer, about 1.8 s sooner: `cd611ce`.
  - System audio, about 29 ms sooner: `dbb2863`.
- **Evidence.** See section 3.
- **Not explained.** No ledger accounts for the hi-res 60 Hz gap. See section 7 and the contradictions in section 7.1.
- **Remains.**
  - The real-link cost of constant QP 15 (IDRs of 83 KB at 1080p and 144 KB at 1440p, `G02.md`).
  - The 120 Hz hi-res state on the merged head (p50 73.5 ms in one run, `G01.md` E5-confirm).

  The final three-way (§2) shows new **level** with old and with the previous head on per-frame latency in all three video scenarios, and faster on join time and audio delay. "Slower" is therefore not reproduced on a same-machine headless join on any of the three builds.

**2. Cursor glitches.**

- **Not testable.** No agent may move a pointer.
- **Found.**
  - The stand-in (`mover_stop`, a 48 px window) was never stale at rest on either build: 0 of 11 stops (`BASELINE.md`).
  - By reading, two capture-side mechanisms (`G01.md` §6): the frame showing a stopped pointer's final position can be dropped by the cadence gate, and cursor-moved frames ran at the compositor's rate (113 fps).
  - The input-side mechanisms are ranked H1–H10 in `INPUT-REPORT.md` §4. The top two:
    - locked motion takes the tracked-absolute path far more often than intended, losing the Windows pointer curve (H1);
    - `GetCursorPos` plus `round()` feed Windows' pixel truncation back into the next move, so 1 px moves right or down are lost and left or up overshoot, on 82–87 % of the screen (H2). This holds only if Windows truncates, which is unverified.
- **Fixed.**
  - `ea821a9` bounds cursor-exempt frames at 1.5× the stream rate, about 90 fps for a 60 Hz client. Unverified for a pointer.
  - `9aae1d3` removes one source of stream rebuilds. Each rebuild resets the client's raw-movement flag (`G05.md`).
  - `f7c2a55` is a robustness edit.
- **Remains.** Everything in `INPUT-REPORT.md` P1–P11. Input code is byte-identical to `latency-overhaul`: `git diff latency-overhaul latency-overhaul-new -- src-tauri/src/streamer/input src-tauri/src/streamer/static/input.js` is empty.

**3. Sound does not work.**

- **Found** (`BASELINE.md` O-3, `G06.md`):
  - The old app's page could not play at all: 0/15 clicks, because its worker scripts lacked COEP. That was fixed before this job, in `8db3887`.
  - The previous head played on the fast path (15/15).
  - G06 ranked the routes to silence:
    1. plain-HTTP joins, which the LAN QR code uses, took the standard Opus track, which concealed over half the audio and ran 350–600 ms late;
    2. audio is off by default per device (by design);
    3. the spare track was never fed after a fast-path failure;
    4. a same-machine echo loop;
    5. a stale output id restarted playback every second;
    6. the GUI never restores a saved output;
    7. `AudioContext` suspension in a real browser (untested).
- **Fixed.** `201a981`, `736f0f9`, `24f75e8`, `61da27a`, `83facec`, `dbb2863`, with before/after numbers in section 3.
- **Remains.**
  - Plain HTTP itself was never joined: the handler's `plain_http` tier fails after the HTTPS upsell modal.
  - The GUI output restore is a UI defect, open.
  - `AudioContext` suspension.
  - `dc_drops` bursts of 41–43 packets (about 200 ms holes). The `07ec295` fix is unverified.

**4. Two devices with two virtual screens after a settings change.**

- **Found** (`G04.md` §1). The CLI path did not produce it when the renegotiation succeeded: 0 of 10 edits in `BASELINE.md`, and every class in G04 B2. It **does** appear the moment one in-place renegotiation **fails**. The failed attempt's session was removed and its display orphaned, the old teardown kept the display as "superseded", and the page's retry became a fresh join with a second display and a second row.
- **Reproduced.** Reproduced with an injected failure: displays 1 → 2, a new join id, an extra row.
- **Other routes, all fixed.**
  - a superseded attempt that completes anyway and leaks a capture (`f789b76`);
  - a failed fresh join orphaning its display (`f789b76`);
  - a relayed client's old page renegotiating without join ids (`8e709bd`, unit-tested only);
  - the GPU-encode toggle dropping every session (`26630dc`);
  - a lost first answer whose retry opens a second display (`7584269` with `e8cbe87`; `2dda493` keeps that retry on WebCodecs);
  - `pagehide` not releasing the join (`aec25fb`, not run);
  - the previous peer connection lingering 15–30 s (`1f85421`, not run end to end).
- **Remains.**
  - The device sheet emits its own `deviceModify`, which can add or resurrect a dashboard row (a UI defect, open, `G04.md` §1).
  - One save can bump the epoch twice, which costs a second renegotiation (`G07.md` §4, open).
  - What makes a renegotiation fail on the owner's machine is not established.

**5. A microphone prompt where speaker outputs need none.**

- **Found.** Confirmed with `mic_prompt` on the previous head: with nothing granted, the card appeared on every join and the join waited on it.
  - Chromium lists every output with an empty `deviceId` until the microphone is granted, and `needsMicPermission()` read that as "ask".
  - `44f3f6d` put the card in front of the join.
  - The old page had the same rule (`BASELINE.md` O-5, `G06.md`).
- **Fixed.** `c020fc0`: no card and no `getUserMedia` on the join. The new "Choose a speaker on this device…" entry is the only door.
- **Evidence.** `mic_prompt` before/after.
- **Remains.** `se:choose` end to end with a real browser and a human answering the card.

**6. Logs do not come through.**

- **Found.** On the previous head the CLI route worked (`BASELINE.md` O-6). The old app's `serve` had no control server. G09 traced every route (`G09.md`). The most likely candidates for the owner's experience:
  - PowerShell does not wait for the GUI-subsystem exe, so output lands after the prompt and `$LASTEXITCODE` is lost;
  - two races in the desktop log panel;
  - the 2000-line ring being flooded by keyframe and congestion chatter, or by the unthrottled input-path `SendInput` failure log (`d3ce813`).
- **Fixed.** `304cf95` (panel race 1), `8e4db79`, `3032603`, `18311e0`, `8423abf`, `52c3eb3`. The storm fixes also cut the chatter.
- **Remains.**
  - Panel race 2: `listen` registered after the backlog read (a UI defect).
  - PowerShell not waiting: a packaging change. The workaround is `ScreenExtend logs | Out-Host`.
  - The input-path log flood (input code, off limits).

### 5.2 Every other defect

**Fixed, with the test or run that shows it:**

| defect | fix | evidence |
|---|---|---|
| D-01 keyframe storm (both builds) | `30d5719`, `396916b`, `a00083d` | IDRs per session 426–435 → 72–73; PLIs 28–35 → 3–4 per 10 s |
| D-02b retry after a lost first answer opens a second display | `e8cbe87` + `7584269`; `2dda493` keeps the retry on WebCodecs | simulated lost answer: 2 displays → 1 |
| D-04 demotion to `<video>` while painting | `9aae1d3` | `scripts/test-client-watchdog.mjs`; 0 of 23 live joins |
| D-05 superseded peer connection alive 15–30 s | `1f85421` | unit test; not run end to end |
| D-06 quality slider never reached NVENC | `cf63279` | log line + hardware test |
| D-07 first refresh rate sticks for life | `58fd8b1` | hi-res fixed run at @120 |
| D-08 manual orientation ignored but renegotiated | `183aea8` | unit test |
| G01 D1 WGC stale monitor → dead DXGI | `48d3807` | forced ×3 |
| superseded attempt overwrote the newer stopper (capture/encoder leak) | `f789b76` | reading + injected run |
| failed fresh join orphaned its display | `f789b76` | reading |
| GPU toggle dropped every session | `26630dc` | one `RELOAD` run |
| relayed page's edits each opened a display | `8e709bd` | unit test only |
| `pagehide` did not release a join that was not live | `aec25fb` | harness only |
| a lost answer demoted the session to `<video>` | `2dda493` | simulation |
| first join paid two first-use costs inside the click | `cd611ce` | 3 v 3 |
| HTTP-tier audio concealed over half the samples | `201a981` | `no_audio_decoder` tier |
| spare audio track never fed | `736f0f9` | `worker_fails` tier |
| same-machine echo loop | `24f75e8` | E9 |
| stale output restarted playback every second | `61da27a` | `stale_output` tier |
| refused output re-offered over the stage | `83facec` | harness |
| microphone card on the join | `c020fc0` | `mic_prompt` |
| WGC capture stamp in the future; keepalive encode stamped after the encode | `3120abf`, `e96fe2c` | diagnostic switch; reading |
| log bus emit race; `LOGS` arming the panel; U+FFFD; closed-pipe panic; control server late; avatar errors lost | `304cf95`, `8e4db79`, `3032603`, `8423abf`, `52c3eb3`, `18311e0` | tests and reproductions in `G09.md` |
| `driver_ipc` errors lost; `Drop` misreported a panic | `1eca751` | reading |
| D-12 old app's RTT unit bug (×1000) | `f5d0e27` (before this job) | — |

Fixed during consolidation (`CONSOLIDATION.md`): `fb6d08b` telemetry removed by the wrong session (unit test; one end-to-end run); `9433732` blocking `session.stop()` moved off the async workers (compile-checked only); `d5ea62a` the missing telemetry name for client audio path 4 (test).

**Still open:**

| defect | source | note |
|---|---|---|
| D-02 lost `/whep` answer, root cause | `BASELINE.md`, `G03.md`, `G07.md` | Mitigated (retry continues the join), cause not found; see section 7.1 |
| D-03 / 2+ displays degrade streams | `BASELINE.md`, `G01.md`, `G04.md` | Mechanism: the owner's panel drops to 60 Hz with exactly one VD and is at 360 Hz with two or more, so DWM composes each VD about 3× too often. `ea821a9` only mitigates. End-to-end cost unmeasured |
| D-11 driver host leak, ~47 MB per create or mode change, never returned | `G04.md` §3 | in the driver; needs a driver rebuild and re-sign |
| internal panel at 60 Hz whenever exactly one device is connected | `G04.md` §4 | owner decision (section 9) |
| D-13 audio trails the picture | `G06.md` | about 29 ms recovered; the rest is the Windows mix before loopback and the browser's output latency |
| superseded teardown unregisters the live session's telemetry | `G03.md` | fixed, `fb6d08b` (unit test; verified on one run) |
| `session.stop()` inline on four async paths in `server.rs` | `G03.md` | a D-02 candidate. fixed, `9433732` (compile-checked; not shown to cure D-02) |
| a hi-res second join inherits the first join's saved override (QP 15) | `G03.md` | handler worked around it (`df1f0c7`); product behaviour by design |
| two epoch bumps per save → two renegotiations | `G04.md`, `G07.md` | coalesce on host or debounce on client; neither done |
| `5e2dff0` codec-hint wait always times out | `G05.md` | harmless, inert |
| `telemetry.rs::audio_path_name` has no name for code 4 | `G06.md` | prints "unknown" |
| GUI: panel race 2; output pin not restored; sheet emits its own `deviceModify`; TS vs Rust field fallbacks (`videoQuality` 23 vs 15, `remoteControl` false vs true); servers start before credentials; `set_server_ports` restarts streamers on the UI thread | `G04.md`, `G06.md`, `G09.md` | desktop UI, out of scope |
| PowerShell does not wait for the GUI-subsystem CLI | `G09.md` E3 | packaging |
| input-path `SendInput` failure log flood | `G09.md` | input code |
| cloud client builds its own 16-worker MMCSS runtime | `G09.md` | not run (it would register a public session) |
| `cloud_status.lock().unwrap()` in register/unregister | `G09.md` | — |
| log writer thread spawn failure drops every line silently; no flush on exit (`c3780ca`) | `G09.md` | — |
| macOS log strings lost their `\` continuations (`57d1620`) | `G09.md` | macOS, not compiled here |
| `refresh_geometry` resolves the input target by `\\.\DISPLAYn` name | `INPUT-REPORT.md` §5.10, `G04.md` §4 | input code (P9) |
| handler: stage picks the wrong display with two (D6); `plain_http` tier cannot join after the upsell modal | `G07.md`, `G06.md` | harness |
| D-07 for DPR and orientation | `BASELINE.md` D-07 | `58fd8b1` addresses the refresh rate only; whether DPR and orientation still stick is not stated in any ledger |

**Not testable here:** everything in section 6, plus real Wi-Fi with loss, the relay and TURN, the hosted network, the installer and auto-update, phones and other browsers, and the GUI.

---

## 6. Left untouched and unverified

- **Input (G11, 16 commits, plus 27 other commits touching input files).** Byte-identical to `latency-overhaul`. `INPUT-REPORT.md` covers the 43 commits from reading alone: ten hypotheses, eleven proposed changes (P1–P11), a manual plan and an instrumented plan. Nothing in it was run.
- **macOS (G12, 67 commits).** Unchanged except two mirrors: `58fd8b1` added `session::clear_join_seeded` to `macos_utils::set_device_override`, marked UNVERIFIED (not compiled), and `655e828` routed `networking.rs`'s store name through `data_dir::config_store()`. Nothing macOS was compiled or run.
- **AMD and Intel (G13, 15 commits).** Untouched. G02 notes that Intel picks `MFX_RATECONTROL_CQP` without a capability mask, so it does not share the constant-QP bug. The AMF backend remains UNVERIFIED as `CLAUDE.md` says.
- **Desktop UI (G10, 29 commits).** `src/` is byte-identical to `latency-overhaul`. The GUI was never started: it steals focus on launch (`G09.md`). Every GUI finding is from reading.
- **Not compiled or not run on this machine for other reasons:** DXGI duplication (it never delivers on this hybrid machine), the x264 path through the handler, Windows below 24H2, the hosted network, the relay.

---

## 7. Unexplained

1. **The hi-res 60 Hz gap** in `BASELINE.md` (p50 39.4 vs 31.9 ms, 3 of 4 pairs). No group reproduced it in its own area. G02 measured 32.5–37.2 ms on its branch and called it load-related. G05 showed the client contributes about 1 ms. G01 ruled out capture.
2. **The hi-res 120 Hz state on `d7a448a`.** One run had p50 73.5 ms, about 1 ring drop/s and 810 duplicate frames (`G01.md` E5-confirm). Against it: G03 measured 34–37 ms at 120 Hz on `96a6d2f`, and G04 measured 35.1–42.1 ms at 120 Hz.
3. **`hires_large_area` at 120 Hz:** p50 outliers of 67–520 ms in both arms (`G03.md` §5).
4. **G08's all-off arm read 1.4–2.5 ms slower** than all-on. That arm ran at higher CPU.
5. **D-02, the lost answer.** The host logs the answer, but the browser never gets the response.
6. **Why the stage's 200 ms bounds check missed the mode change** (`BASELINE.md` H-2).
7. **What makes an in-place renegotiation fail on the owner's machine** (`G04.md`).
8. **Why the panel goes to 60 Hz with exactly one virtual display.** The suspected cause is Windows' persisted per-topology configuration. Unproven (`G04.md` §4).
9. **The `dc_drops` bursts** of 41–43 packets, about 205–215 ms of audio. A delayed SACK is suspected, not established (`G06.md`; `G03.md` attributes them to `buffered_amount` counting until the peer's SACK).
10. **The live 1440p encode stage** reads 8.39 ms p50 against 5.0–5.7 ms of clean NVENC time (`G02.md` E8). The open lead is the keyed-mutex wait for the capture-side copy.
11. **`<video>` vs WebCodecs at 1080p:** 23.4 and 34.3 ms on two `<video>` joins, against 25–26 on WebCodecs, one window each (`G05.md` §2).
12. **Green samples on a static desktop.** 2 of 5 handler acceptance runs (loaded) showed them; 0 since on the previous head. The old app showed 2 green frames in 1 of 4 baseline runs.
13. **Why the old app collapses at 120 Hz.** Observed, not diagnosed (`BASELINE.md`).
14. **`serve`'s unpumped main thread** and display-topology broadcasts (`G09.md` item 7). A hypothesis, never measured.

### 7.1 Where the ledgers disagree

- **The cause of the hi-res gap.**
  - `BASELINE.md` ("The hi-res comparison"): both builds ran CBR at 60 Hz, so neither refresh nor rate control is the confound. It points to encoder configuration (High + CABAC, async) and noise.
  - `G01.md` §0 and §4: "60 Hz instead of 120, and CBR instead of constant QP". This compares against an old app said to run constant QP, which `BASELINE.md` says it did not: the old app keys overrides by IP, and the owner's config has no row for this address.
  - `G02.md` E7: not reproduced, load-related.
  - `G03.md` "Cross-findings": the hi-res second join runs constant QP 15 on the head, "first candidate". That is true only after `cf63279`. On `06e6fbb` the request fell back to CBR (D-06). G02 E6 found no rate-control effect at hi-res.
  - `G05.md` §3: upstream of the client.
- **The "120 Hz capped at 60" finding.** `AGENT-BRIEF.md` attributed it to `DEFAULT_MAX_AUTO_REFRESH` (`b0183dd`). `G04.md` §4 says that constant had already been removed by `170138e`, and the cap was mostly the handler (H-3) plus the seeded override (D-07).
- **G05's baseline.** The table is labelled "2560×1440 @120", but its fps (58.7–58.9) are those of a 60 Hz display.
- **How often the answer is lost.**
  - `scripts/test-handler/README.md` and `AGENT-BRIEF.md`: "about one join in five".
  - `BASELINE.md`: 0 of 20 on a quiet machine, "it needs load".
  - `G04.md`: reproduced once on a quiet machine (`09-20-23-310Z-slot4`).
  - `G07.md`: 10 of 66 first joins and 7 of 63 later joins in mined run directories.
- **The lost answer's mechanism.** All are readings, none proven, and they are compatible.
  - `BASELINE.md`: the h2 connection task stalls behind a blocked worker (tokio LIFO slot).
  - `G03.md`: `session.stop()` inline on four async paths blocks a worker.
  - `G07.md`: the host's connection task is not polled.
- **The source of the keyframe storm.** `INPUT-REPORT.md` §9 suggested the client's out-of-band 0x64 requests. `G03.md` measured `client_keyframe_requests` = 0: the whole storm is RTCP from libwebrtc's starving decoder.
- **The ~10 % stamp checksum failures.** `G02.md` calls them "unexplained (harness cell size or capture tearing)". `BASELINE.md` H-1 and `G05.md` §5 established that the stage's GDI blit tears, and fixed it with `stamp_band: "center"`.
- **The noise figure.** `AGENT-BRIEF.md` and `README.md` give a small-window p50 range of 1.48 ms on a loaded machine. `BASELINE.md` gives 7.6 ms over four quiet runs spread across seven hours.
- **The offset between `hostToPaint` and the stamp.** `README.md` says about 10–12 ms and "the same for every build". `G08.md` §3 measured 7.1–10.8 ms on the small window and 11.3–14.3 ms on the large area: it depends on the scenario.
- **Open in G09, since fixed by G04.** `G09.md` lists as open three items that G04 later fixed: the GPU toggle dropping sessions (`26630dc`), the `driver_ipc` `log::error!` calls (`1eca751`), and the relay `/reconfig` `.next()` (`8e709bd`).
- **The SACK mechanism.** `G03.md` says delayed SACK drives the `dc_drops`. `G06.md` says it is not established, and that the relevant SACKs are the browser's, not webrtc-sctp's.
- **The early-frame cadence gate.** `AGENT-BRIEF.md` says it drops "~500 frames a session". `G01.md` measured about 0.5 a second with one display (44–51 in a ~110 s session) and about 125 a second with two.
- **The G03 handler kill.** The orchestrator's list of incidents describes it as leaving a display. `G03.md` §10 says the run "had already removed its display (`removed [0]`, machine `[]`)".

---

## 8. Questions for the owner

Each question gives what was assumed and what changes if the answer differs. Duplicates across ledgers are merged; the source ledgers are named. The first six change behaviour you will notice.

1. **The quality slider now really applies: constant QP 15** (`G02.md` Q1, `QUESTIONS.md`).
   - **Background:** your devices are set to `videoQuality` 15. Until `cf63279` that silently ran CBR at 12–22 Mbps. It now gives near-lossless text, IDRs of 83 KB (1080p) and 144 KB (1440p), tiny P frames on a still desktop, and **no bitrate adaptation on a bad link**.
   - **Assumed:** you want the slider to do what it says.
   - **If not:** the next step is VBR with a QP cap. Constant QP on real Wi-Fi was never measured.
2. **Process loopback is tried first** (`G06.md` Q4).
   - **Background:** it captures every process except ScreenExtend, on any output device. Endpoint loopback captured only the default output's mix.
   - **Assumed:** equivalent for you.
   - **If not:** if you route some apps to a second output on purpose, they are now captured. `SE_ENDPOINT_LOOPBACK=1` restores the old order.
3. **A page opened on the host machine itself starts muted** (`G06.md` Q2).
   - **Assumed:** yes, because the host's own speakers already play the sound and an unmuted page loops.
   - **If not:** if you test with headphones on the browser only, this should become a choice.
4. **The new "Choose a speaker on this device…" entry** in the device sheet's output list (`G06.md` Q3).
   - **Background:** it replaces an endless "Waiting for this device to report its speakers…" on browsers that list nothing without a permission. Picking it asks the client for the microphone once.
   - **Assumed:** acceptable (one list item, no visual change).
   - **If not:** the entry can be removed, and the old wait comes back.
5. **A device that reports 120 Hz gets 120 Hz** (`G04.md` Q3, `BASELINE.md` Q-6).
   - **Background:** `58fd8b1` restores the old app's behaviour. `b0183dd` had added a cap after a 360 Hz client collapsed the real panel's compositor.
   - **Assumed:** honour the report up to `MAX_REFRESH_RATE`.
   - **If not:** a default cap returns, or the first join's rate is kept (D-07 becomes intended and only needs to be visible).
6. **A libwebrtc keyframe request may wait up to 10 s while the page is painting** (`G03.md` Q2).
   - **Background:** `396916b` does this. The page's own requests are answered at once, and a NACK or reported loss opens the window.
   - **Assumed:** acceptable.
   - **If not:** a client page that answers decode errors only through `sendKeyFrameRequest`, without its STATS counter moving, could wait up to 10 s. The cap would have to come down.
7. **Pointer frame rate: about 90 fps (old app) or 113 fps (overhaul) for a moving pointer?** (`G01.md` Q1)
   - **Assumed:** the old bound is safe; `ea821a9` brings it back.
   - **If not:** the cursor exemption can get its own floor.
8. **Did the pointer flicker you reported happen with GPU scheduling at REALTIME only, or also at ABOVE_NORMAL?** (`G01.md` Q2)
   - **Assumed:** REALTIME only; the class stays at 3.
   - **If not:** it goes back to 2, as in the old app.
9. **Your laptop panel drops from 360 Hz to 60 Hz whenever exactly one device is connected** (`G04.md` Q4, `QUESTIONS.md`).
   - **The choice:** will you set 360 Hz once by hand with one device connected, or may the host restore your panel's mode after it attaches a display? The second option needs the "never write a display we did not create" rule relaxed for "restore what it was".
   - **Assumed:** neither yet; reported only.
10. **May you restart the virtual display device (or reboot), and do you want the driver fixed?** (`G04.md` Q2, `QUESTIONS.md`)
    - **Background:** a driver fix means rebuilding and re-signing `VirtualDisplayDriver.dll`.
    - **Assumed:** a restart is needed to finish measuring.
    - **If no fix:** the leak continues at about 47 MB per display created or mode changed.
11. **Do you join over the cloud/relay URL, the LAN URL, or both?** (`G04.md` Q1)
    - **Assumed:** both, so both routes were fixed.
    - **If the relay:** `8e709bd` (unit-tested only, never run) is the likely cure for "two devices, two screens", and it should be tested first.
12. **When a settings edit's renegotiation fails, should the page silently get the old display back within the 10 s grace, or should the host tell you?** (`G04.md` Q5)
    - **Assumed:** silently.
13. **Should system audio be on by default for a new device?** (`G06.md` Q1)
    - **Assumed:** off, unchanged.
    - **If on:** flip `config-schema.ts`, `bootstrap.tsx`, `apply-config.ts` and `cli.rs` together.
14. **Should the LAN QR code point at HTTPS instead of `http://`?** (`G06.md` Q5)
    - **Background:** every HTTP join takes the slow audio and video tiers, and the pointer path differs (`INPUT-REPORT.md` H3).
    - **Assumed:** unchanged.
15. **Picture quality or Wi-Fi latency?** (`G03.md` Q1)
    - **Background:** the old app ran every session at about 1 Mbps by accident (the RTT unit bug), which is part of why it felt fast on Wi-Fi.
    - **Assumed:** full quality, with congestion control doing its job.
    - **If latency matters more:** a lower default target.
16. **Up to about 260 ms of queued audio on a congested link, or about 200 ms holes on an idle one?** (`G03.md` Q3)
    - **Assumed:** the former (`07ec295`).
    - **If not:** lower `AUDIO_SACK_ALLOWANCE_MS` in `webrtc_session.rs`.
17. **Should the host ignore RTCP PLI/FIR outright from a page that reports the out-of-band keyframe tier?** (`G05.md`, `G03.md` §9)
    - **Assumed:** no host change; one PLI per ~3 s remains.
    - **If yes:** the worker should prefer the 0x64 request.
18. **Remove `5e2dff0`'s codec-hint wait, or arm it from the answer?** (`G05.md`)
    - **Assumed:** leave it (inert).
19. **Should the microphone card ever block the join again?** (`G07.md`)
    - **Assumed:** no.
20. **Is a 0.4–0.8 s one-off main-thread task at page load acceptable on phones, for a faster first join?** (`G07.md`)
    - **Assumed:** yes.
    - **If not:** run the warm-up on the form's first focus.
21. **How do you read logs: the Settings panel, `ScreenExtend logs`, or `serve -v` in PowerShell?** (`G09.md` Q1, `BASELINE.md` Q-3)
    - **Assumed:** any of the three.
    - **If PowerShell:** the console-subsystem companion exe is the fix.
22. **Which default for a device row missing a field: `videoQuality` 23 (UI) or 15 (host), and `remoteControl` false (UI) or true (host)?** (`G09.md` Q2, `G04.md` §1)
    - **Assumed:** leave both.
23. **Should `serve` auto-approve and remember a joined device as the GUI does?** (`G09.md` Q3)
    - **Assumed:** no.
24. **Should public relay sessions stay on by default?** (`G09.md` Q4)
    - **Background:** the GUI registers one at every start and spawns a second 16-worker runtime for it.
    - **Assumed:** yes.
25. **Is a 2000-line log backlog enough?** (`G09.md` Q5)
    - **Assumed:** yes, with the chatter cut.
26. **Should WGC report any `capture_delivery` number, and should host histograms be windowed rather than cumulative?** (`G08.md`)
    - **Assumed:** no, and out of scope.
27. **Which client and which numbers felt "slower"?** (`BASELINE.md` Q-1, `G02.md` Q2)
    - **Assumed:** a 1080p desktop Chrome on the LAN.
    - **If hi-res or high refresh:** that path comes first.
28. **"Sound does not work": which device, browser, page (HTTP or HTTPS) and audio setting?** (`BASELINE.md` Q-2)
    - **Assumed:** HTTPS, headless Chrome.
    - **If HTTP:** `201a981` is the likely cure.
29. **"Two new devices": made from the desktop UI's device sheet, and which setting?** (`BASELINE.md` Q-4)
    - **Assumed:** from the UI, a stream-class setting.
30. **Cursor questions** (`BASELINE.md` Q-5, `INPUT-REPORT.md` §10 Q1–Q11):
    - does "laggy" mean the cursor trails in time, or needs more hand movement than the old app (which curved twice)?
    - which client browser and OS?
    - `http://` QR or HTTPS?
    - fullscreen (pointer lock) or windowed?
    - does "defocussed" mean the lock or fullscreen dropping, the cursor leaving the display, or host focus moving?
    - should pointer scope stay "All"?
    - may the page hide its own cursor in windowed mode?
    - do you run PowerToys, AutoHotkey or a mouse utility with a global hook?
    - was something holding the cursor at the primary's centre on 09-11 (`af59c4a`)?
    - should Ctrl/Cmd+V carry the client's clipboard?
    - does pointer speed change after a mid-session settings edit?

    Each assumption is in `INPUT-REPORT.md` §10.
31. **May isolated tests run alongside your installed ScreenExtend?** (`QUESTIONS.md`)
    - **Background:** asked twice on 10/4, not answered.
    - **Assumed:** no; agents stood down from display tests while the app was open.

---

## 9. Things the owner should do, in order

1. **Free the driver host's memory.**
   - From an elevated prompt, with no ScreenExtend host running, find the instance id: `pnputil /enum-devices /class Display` (or `Get-PnpDevice -FriendlyName '*Virtual Display*'`).
   - Then run `pnputil /restart-device "<instance id>"` (`G04.md` §3).
   - Or reboot.
   - Until then every join costs commit the machine cannot give back.
2. **Try setting the internal panel to 360 Hz once with exactly one device connected** (Settings → Display → Advanced display, with one ScreenExtend client joined).
   - Disconnect, reconnect one device, and check whether it stays at 360 Hz.
   - If it does, Windows remembered it for that topology (`G04.md` §4, untested).
   - Optionally, as admin, look in `HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Configuration` for an entry pairing the panel with a `CHY0000` monitor at 60 Hz.
3. **The ten-minute manual input check** (`INPUT-REPORT.md` §7, T1–T6).
   - **Setup:** host started with `SE_INPUT_TRACE=1`. A second laptop with a physical mouse (ideally 1000 Hz) in Chrome, fullscreen ticked. Paint at 800 % and Notepad on the virtual display.
   - **T1:** sweep speed at connect and after a quality edit. Change Windows pointer speed to tell the relative path from the tracked path.
   - **T2:** with "Restrict cursor to this display" ticked, creep slowly right vs left.
   - **T3:** `http://` vs `https://` sweep at 125 and 1000 Hz.
   - **T4:** click while moving, 20 times, locked then unlocked.
   - **T5:** first click after a refresh-rate edit, without moving.
   - **T6:** push past every edge, locked and unlocked.
4. **Answer the questions in section 8,** first 1–6 and 9–11.
5. **Run the desktop-UI reproductions the ledgers name.** Each needs the GUI, which no agent started.
   - **Two devices after a save** (`G04.md` §1): join one tab, change the refresh rate in the device sheet, and watch the row count and `ScreenExtend status --json` `telemetry.joins`. Repeat over the cloud URL.
   - **GUI output not restored** (`G06.md`): pick a non-default output, save, restart the app, join.
   - **Log panel gap** (`G09.md`): open Settings → Logs during a join and compare with `ScreenExtend logs`.
6. **Over the cloud URL, change a stream setting and count displays.** This is the only way to exercise `8e709bd`.
7. **On a phone, join, lock and unlock the phone,** and check sound returns. If it does not, `status --json` `degraded` bit 7 is set (`G06.md`).
8. **Decide on the driver fix.** It means a rebuild of the `virtual-display-rs` fork at `eae5120` (the fork is unpublished, `src-tauri/resources/PROVENANCE.md`) and re-signing with the key behind `ScreenExtend.cer`.
9. **When you are ready to have input tested,** lift the input ban explicitly. The next job (`FUTURE-TEST-WORKFLOW.md`) stops and asks otherwise.

---

## 10. Rule breaches and incidents

- **Runs started below the memory floor.**
  - The baseline agent's old-app `audio`/`settings_change` run (spA1-old) started at **2.3 GB commit headroom**, under the 3 GB floor. The agent killed the run's waiting memory check, which let it proceed; it completed and cleaned up, with no display left and the config identical (`BASELINE.md`).
  - G09 started one debug test build at 3.8 GB headroom, under the 6 GB rule that arrived mid-session (`G09.md`).
  - G06's second E7 handler pair ran at about 1 GB free *physical* memory, under the 1.5 GB "suspect" line; the result was kept because the first pair, at about 3 GB, agrees (`G06.md`).
  - G07's e3-1 window ended at 1.8 GB free and was marked suspect (`G07.md`).
- **Join-ration overrun.** G02 spent about 23 units against a ration of about 15, by its own mistake: it removed its runner's STOP file before the previous series had reached its check, so a third round ran (`G02.md`).
- **Build-lock overlap.**
  - A build-lock directory was removed while its owner was still building, and two builds overlapped. The 06:20 rule was added after it (`AGENT-BRIEF.md`).
  - At 07:06 G03 ran another agent's `build.ps1` from the shared scratchpad by mistake. It held the build lock for 3 s and built nothing new (`G03.md` §10).
- **A handler stopped before teardown.** The memory reaper stopped G03's half-started run `r-old` at 08:24. `G03.md` §10 records that the run had already removed its display (`removed [0]`, machine `[]`). G03 removed the stale `slot-1.json`, `measure.lock`, the profile directory and its own stale build lock. No host, Chrome, stage window or display of G03's remained.
- **Background shells reaped or killed for memory.**
  - G03's D-05 after-fix run was reaped (`G03.md` §4).
  - G03 killed two of its own loop shells between runs to stop unrationed joins.
  - The baseline agent killed a queued handler (msmoke2) by PID before it held a slot.
  - The orchestrator's 02:00 resume timer was reaped for low memory, so the job resumed at 02:12 on the owner's "continue" (`QUESTIONS.md`).
- **Contaminated measurements despite the lock.** In shared-driver mode a released monitor in the middle of the id run stays attached. G01's E1 B run and `cad-A-1` therefore ran with two displays attached machine-wide (`G01.md` §3). Both were discarded for latency. G01's E5-confirm and several other runs were contaminated by lost answers (`offers: 2`).
- **std1 was taken loaded** (92–99 % CPU) and discarded (`BASELINE.md`). Not a breach.
- **The owner's `config.json`** was rewritten twice on 10/4 by the owner's own app while agents were paused. Agents preserved whatever was current (`QUESTIONS.md`). Every ledger that ran in exclusive mode reports the hash unchanged by its runs (`176B9DAD7C9C4985…`).

Consolidation: no rule breach reported. One unexplained event: in one new-build round the 1080p join's four timed windows decoded only implausible stamps (older than 5 s or in the future) while the host encoded 59 fps and the page's own counters showed path 1, 70 painted fps and hostToPaint 15.7 ms; those windows were discarded (see §7). `SE_REQUIRE_HW=1 cargo test` has one failure by configuration (`nvenc_ltr_repair_is_cheaper_than_an_idr` skips because LTR is off by default; it passes with `SE_NVENC_LTR=1`). The fat-LTO build and `pnpm tauri build` were not done (commit headroom 9.63 GB, under the 10 GB floor set for a fat link). From the orchestrator's own record, not in a ledger: at the first owner-requested pause (22:16 on 10/4) stopping G02's runner killed a handler before teardown and left one virtual display attached, which G02 removed through the driver; and the orchestrator stopped three agents' release builds by PID to honour the second pause. Machine state at hand-back: no host, no test browser, no stage window and no virtual display left; all handler slots free; owner's `config.json` SHA-256 still `176B9DAD7C9C4985…`; the driver host at 36.7 GB commit and 3,131 threads; commit headroom 7.9 GB.

One contradiction in the brief itself, recorded as an incident: `TEST-WORKFLOW.md` §3 rule 1 says never to run `scripts/test-startup-gate.mjs`, while §8 lists `pnpm test:gate` (which runs it) in the final gate. The consolidation agent read the script in full first — it is a pure decision-table test that imports `startup-gate.ts` and reads `en.json`, starts no process, opens no window and injects nothing — and then ran it; it passed. No input was injected at any point in the job, and `git diff latency-overhaul latency-overhaul-new -- src-tauri/src/streamer/input src-tauri/src/streamer/static/input.js` is empty.

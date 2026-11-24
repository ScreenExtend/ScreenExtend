# macOS host tuning audit (BENCH-M2)

What the Windows host does to keep capture → encode → send on time, what the macOS host does for
the same purpose, and what is missing. Written from the code as of BENCH-M2 (2026-11), not from
memory: Windows in `windows_utils/streamer/tuning.rs`, `nvidia/clock_hold.rs`, `capture.rs` and
the call sites in `windows_utils/streamer/pipeline.rs`; macOS in
`macos_utils/streamer/{qos,tuning,power,activity}.rs`, their call sites in
`macos_utils/streamer/pipeline.rs`, `streamer/platform.rs` and `streamer/mod.rs`, and the capture
queues in `cgds.rs` / `sck.rs`.

Every existing macOS item now has an `SE_TUNE_OFF` name and every new candidate an `SE_TUNE_ON`
name (`macos_utils/streamer/tune.rs`). With neither variable set nothing changes. Each call site
records what it actually got (`tune::report()`: applied / return code / skipped), and the bench
puts that record in every result (`"tuning"`). **Nothing new is on by default.**

## Windows → macOS

| # | Windows action (where) | macOS equivalent | macOS today | `SE_TUNE_*` name | gap / note |
|---|---|---|---|---|---|
| 1 | Process priority class `HIGH_PRIORITY_CLASS` (`raise_process_priority`, at `Streamer::prepare`) | No priority class; a negative nice needs root. Darwin expresses process importance through QoS, NSProcessInfo activities and the task role | NSActivity `UserInitiated \| AutomaticTerminationDisabled \| SuddenTerminationDisabled`, process-wide, once (`tuning.rs`) | `process_activity` (off) | **`task_role_foreground` — on by default while a session is live** (`SE_TUNE_OFF` to disable): `task_policy_set(TASK_CATEGORY_POLICY, TASK_FOREGROUND_APPLICATION)` at session start and about once a second from the encode loop's slow path (macOS resets the role on activation changes), rc and assertion count in the tuning report, nothing restored at session end. M1, pipe stage, 1080p60 under 8 busy threads: offer→packetized p50/p90/p95/p99 8.37/11.55/12.09/16.06 → 6.18/6.36/6.43/6.92 ms (`TUNE-ROLE`). No effect on an idle machine or on the Intel Mac. |
| 2 | Power throttling (EcoQoS) opt-out, process (`exempt_from_power_throttling`) and per thread (`exempt_thread_from_power_throttling`) | App Nap / timer coalescing / QoS clamping for background apps; opted out with an NSActivity, and latency/throughput QoS tiers on the task | NSActivity `UserInitiated` process-wide (row 1) and `UserInitiated \| LatencyCritical` per session (`activity.rs`, held by `SessionCapture`) | `process_activity`, `latency_activity` (off) | **Candidate `task_qos_tier0`**: `task_policy_set(TASK_BASE_QOS_POLICY, {LATENCY_QOS_TIER_0, THROUGHPUT_QOS_TIER_0})` — the explicit form of what `LatencyCritical` asks for implicitly (timer-coalescing latency tier and throughput tier). May be refused for an unprivileged task; the rc is recorded. |
| 3 | Timer resolution 1 ms (`timeBeginPeriod(1)`) + honour it in the background (`PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`) | No global timer resolution. Wakeup precision is governed by timer coalescing (latency QoS tier, row 2) and, for a deadline thread, by the time-constraint policy | The encode thread is a time-constraint thread (row 5); nothing else | — | Covered by rows 2 and 5. Nothing further to set. |
| 4 | MMCSS registration (`Pro Audio` → `Games` → `Playback`) at `AVRT_PRIORITY_CRITICAL` on capture/encode threads (`tune_current_thread`) | `THREAD_TIME_CONSTRAINT_POLICY` (real-time band) — the only Darwin mechanism that removes a thread from timeshare decay | Encode thread only | `encode_time_constraint` (off) | Drain thread and capture queue are not real-time: see rows 6–7. |
| 5 | `THREAD_PRIORITY_TIME_CRITICAL` + priority boost disabled on capture/encode (`raise_current_thread_priority`) | Same as row 4 on macOS | Encode thread: period = frame, computation = 0.75 × period, constraint = period, preemptible (`qos::pin_current_thread_time_constraint`, read back and logged) | `encode_time_constraint` (off) | Parameters checked: the reservation is far larger than the encode thread's work (a ~40 µs low-latency `EncodeFrame` call plus loop bookkeeping), which is harmless while it sleeps but tells the scheduler to expect 12.5 ms of CPU per 16.7 ms. **Candidate `encode_tc_short`**: computation = min(2 ms, period/4), constraint = period/2. |
| 6 | Timer/repeater thread at `HIGHEST` + MMCSS (`tune_timer_thread`) | The idle repeater is not a thread on macOS: it is the encode loop's own keepalive branch | Inherits the encode thread's policy | — | No gap. |
| 7 | (drain is inside the encode thread on Windows; the ring encode thread is time-critical) | Drain / Annex B thread | `QOS_CLASS_USER_INTERACTIVE` + `THREAD_PRECEDENCE_POLICY` importance 63 + affinity tag (Intel) (`spawn_drain`) | `drain_qos`, `drain_precedence`, `encode_affinity` (off) | **Candidate `drain_time_constraint`**: period = frame, computation = period/4, constraint = period, replacing QoS + precedence (one scheme per thread, as `qos.rs` argues for the encode thread). |
| 8 | Hybrid CPU: confine capture/encode to the highest efficiency class (`SetThreadSelectedCpuSetMasks`) | No CPU-set API. On Apple silicon the scheduler places USER_INTERACTIVE and real-time threads on P-cores; `THREAD_AFFINITY_POLICY` is unimplemented there (`KERN_NOT_SUPPORTED`) | Affinity tag on Intel only; QoS does the P-core placement on Apple silicon | `encode_affinity` (off) | No further mechanism exists. The affinity call is recorded as rc 46 on arm64 without being made. |
| 9 | Transport (tokio) workers: MMCSS `Playback` + `ABOVE_NORMAL` (`tune_transport_thread`) | QoS class on each worker (`platform::tune_transport_thread` via `on_thread_start`) | `QOS_CLASS_USER_INITIATED` | `transport_qos` (off) | **Candidate `transport_user_interactive`**: USER_INTERACTIVE, the class the drain and capture queue already use. The bench's `pipe` stage applies the same function to its packetize thread. |
| 10 | GPU scheduling priority class `ABOVE_NORMAL` (`D3DKMTSetProcessSchedulingPriorityClass`) and D3D11 GPU thread priority +5 (`SetGPUThreadPriority`) | No public GPU scheduling priority on macOS. Metal command-queue priorities are private; VideoToolbox runs on the media engine, not on our command queue | Nothing | — | No public API. `MaximizePowerEfficiency=false` and `PrioritizeEncodingSpeedOverQuality=true` are the encoder-side knobs and are already set (recipes can vary them). |
| 11 | GPU clock hold: copies on a private device to keep NVENC out of P8 (`nvidia/clock_hold.rs`, AC only) | No equivalent need found yet: the M1 low-latency encoder's submit→callback is ~7.4 ms p50 at 1080p60 with no load. Whether the media engine clocks down under a light load is measurable with the bench (`pipe` + `power_report`) but has not been measured | Nothing | — | Open question, not a missing setting. Needs `powermetrics` (root) to see engine clocks. |
| 12 | Keep-awake `SetThreadExecutionState(ES_SYSTEM_REQUIRED \| ES_DISPLAY_REQUIRED)` per encode thread | IOKit power assertions | `PreventUserIdleSystemSleep` + `PreventUserIdleDisplaySleep` per encode loop (`power.rs`, released on drop) | `keep_awake` (off) | Already at parity. A `PreventSystemSleep` assertion would only matter with the lid closed on battery, where the host has no display to extend; not implemented. |
| 13 | DPI awareness `PER_MONITOR_AWARE_V2` (`set_dpi_awareness`) | Not applicable: WindowServer delivers backing pixels; HiDPI is a per-display flag the host sets (`virtual_display.rs`) | n/a | — | No gap. |
| 14 | Capture thread: WGC frame-arrived callback on a time-critical thread | Capture callback on a serial dispatch queue | Serial queue targeting the `UserInteractive` global queue (`cgds.rs`, `sck.rs`) | not switchable in M2 | The pipe stage has no real capture, so a switch here could not be measured yet; `sck.rs` is being edited by another session. A real-time capture queue is a candidate for the live-capture milestone (M3). |
| 15 | (no Windows equivalent) | Low-power mode (macOS 12+, `NSProcessInfo.isLowPowerModeEnabled`, probed with `respondsToSelector:`), thermal state and the power source (`IOPSCopyPowerSourcesInfo` + `IOPSGetProvidingPowerSourceType`, 10.2+, `dlsym`) | Not reported by the host (the bench's env stage reads them) | **Candidate `power_report`** | Logs `power_source=ac\|battery\|ups\|unknown low_power_mode=… thermal=…` at session start and puts it in the session telemetry (`host_power`), so a later default can be "AC only". |
| 16 | (no Windows equivalent; cf. the NVIDIA clock hold, row 11) | The M1 encodes the same clip in ~6.3 ms when the streaming process's threads sit on performance cores and ~8.1 ms on an idle machine (`ave.avc`: 6.5 vs 12.2 ms); an external low-priority spinner does not help | Nothing | **`perf_hold` — default policy** (`perf_hold.rs`, `SE_TUNE_OFF=perf_hold` disables the default) | One `QOS_CLASS_USER_INTERACTIVE` thread busy-spins (`spin_loop` hint) from each captured frame's submit until 200 ms after the newest one (`SE_PERF_HOLD_LINGER_MS` with `SE_TEST_HOOKS=1`), parked otherwise; repeats and hold pushes never start or extend it. On by default only with `hw.nperflevels >= 2`, AC power and Low Power Mode off, re-decided about once a second on its own thread; `SE_PERF_HOLD=0` off / `=1` linger forced / `=spin` the always-spin variant; `tune_on: perf_hold_spin` / `perf_hold_inflight` force a variant in a plan. Tuning record `perf_hold`: variant, why, decision inputs, decision changes, spin fraction, wake-ups (`PERF-HOLD`). |
| 16 | (no Windows equivalent) | `NSProcessInfoThermalStateDidChangeNotification` (10.10.3, name `dlsym`'d, NSNotificationCenter through `msg_send!`) | Not observed | **Candidate `thermal_notify`** | Logs every thermal-state change with the power line. |

## Switch names

`SE_TUNE_OFF` (existing, on by default): `process_activity`, `latency_activity`, `keep_awake`,
`encode_time_constraint`, `encode_affinity`, `drain_qos`, `drain_precedence`, `transport_qos`,
`hold_push`, `task_role_foreground`, `perf_hold` (the default policy; `SE_PERF_HOLD` wins over it).

`SE_TUNE_ON` (candidates, off by default): `drain_time_constraint`, `transport_user_interactive`,
`encode_tc_short`, `task_qos_tier0`, `power_report`, `thermal_notify`, `perf_hold_spin` (force spin), `perf_hold_inflight` (force linger), and the test hook `hold_push_start_immediate` (needs `SE_TEST_HOOKS=1`). `task_role_foreground` moved to the existing (default-on) list with `hold_push`.

A bench plan group carries them as `"tune_off": [...]` / `"tune_on": [...]`; the runner clears
both variables from every child and sets them only from the group. They take effect in the `pipe`
stage (which runs the production encode loop, drain and process tuning). In the `lab` stage only
the process-wide items would apply, and the lab never calls them, so there they are inert.

## Not implemented, and why

- **NSActivity `LatencyCritical | UserInitiated`**: already used, per session (`activity.rs`).
- **Further IOKit assertions**: the two idle-sleep assertions cover what a streaming host needs;
  `PreventSystemSleep` and `DeclareUserActivity` change nothing about latency.
- **GPU / media-engine priority**: no public API on macOS.
- **Capture-queue real-time**: deferred to the milestone that has real capture to measure it.

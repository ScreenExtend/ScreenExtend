# Optimizations ledger

A running record of every performance change attempted during the latency & compatibility
overhaul: the hypothesis, the measurement before, the measurement after, whether it was kept or
reverted, and why. Failures are recorded on purpose — a reverted change with a number attached
is information the next person needs.

Verification status vocabulary (used here and in `CAPABILITY-TIERS.md`):

- `VERIFIED` — executed and measured on one of the two development machines.
- `COMPILE-CHECKED` — builds and passes static/layout tests, never executed.
- `UNVERIFIED` — written from research and review; no local runtime signal at all.

Development machines: Windows 11 build 26200 with an RTX 3080 Ti Laptop GPU (hybrid graphics,
NVENC driver 580.97); macOS 10.15.8 Catalina on Intel with Command Line Tools only.

Measurement conventions: nanosecond timestamps off the shared host clock
(`streamer::audio::host_now_ns`), reported as p50 / p90 / p99 / max, never as a bare mean.

| # | Area | Change | Hypothesis | Before | After | Status | Kept? | Notes |
|---|------|--------|------------|--------|-------|--------|-------|-------|
| 1 | build | `[profile.release]`: `lto="fat"`, `codegen-units=1`, `opt-level=3`, `debug="line-tables-only"`, `strip=false` | Shipping builds ran on cargo's stock release profile (LTO off, 16 CGUs); the hot path crosses crate boundaries every frame (encoder FFI -> `streamer` packetizer -> `webrtc`/`bytes` -> SRTP), so cross-crate inlining should pay. | cold `cargo build --release` **244 s**, `ScreenExtend.exe` **33,698,304 B** | cold build **698 s** (11.6 min), binary **27,878,400 B** (-17.3%) | VERIFIED (build); UNVERIFIED (latency) | yes | `fat` measured under the 15-min budget so `thin` was not needed. Binary shrank despite adding line tables. Runtime effect not yet measured -- no baseline run existed to compare against when this landed. |
| 2 | permissions (macOS) | Screen Recording is read with `CGWindowListCopyWindowInfo` + `kCGWindowName` instead of a capture probe; `CGPreflightScreenCaptureAccess` demoted to a positive-only signal; `CGDisplayStream` removed from the authorization decision. | The gate polls `check_permissions` at 5 Hz on the main thread, and the first call ran a real capture probe inline. The window-list read answers the same question without opening a capture session. | first `check_permissions` 700 ms (CGDisplayStream) to 5 s (SCK `getShareableContentWithCompletionHandler` timeout); every later call a hard `false` | window-list probe **37 ms cold, 0.6 ms warm** (median of 5 consecutive calls, 10.15.8 Intel, `cargo test --lib permissions::tests::window_name`); SCK moved to a background slot | VERIFIED (10.15.8, CGDS/window-list paths); UNVERIFIED (12.3+ SCK confirmation, 15/26 behaviour) | yes | The old ordering made the gate unclosable, not merely slow: the preflight is stale by design and it returned before the probe built to work around it. Grant-to-gate-close is now bounded by the 200 ms poll wherever any other titled window is on screen. |
| 3 | permissions (macOS) | `accessibility_api_enabled()` off the primary read: `AXIsProcessTrusted()` alone decides, the system-wide `AXUIElementCopyAttributeValue` becomes a background-refreshed secondary whose timeout reads as *unknown*. | The helper could only ever return `false` on `kAXErrorAPIDisabled`, a pre-10.9 condition — so it contributed no signal while paying an AX IPC round trip per call. | one system-wide AX round trip per `check_permissions`, 5x/second, ceiling 0.5 s each | **20-23 ms** per call measured on 10.15.8 (`doctor --json`, `macos.probes.axSystemWideElementUs`), now at most once per 2 s on a background thread | VERIFIED (10.15.8) | yes | 20 ms x 5/s was ~10% of the main thread for zero information. It also silently set the process-wide AX messaging timeout; that side effect survives, see `findings/minor-permissions-ax-messaging-timeout-still-set-process-wide.md`. |

## Notes

### `CGPreflightScreenCaptureAccess` is not present on 10.15.8

Measured, not assumed: `dlsym(RTLD_DEFAULT, "CGPreflightScreenCaptureAccess")` returns
null on macOS 10.15.8, so `doctor` reports the probe as `unknown` there.
`docs/research/macos-permissions-tcc.md` §2.1 states both preflight/request functions are
"macOS 10.15+, resolvable by `dlsym` on every host ScreenExtend supports" — that is wrong on
this project's own floor. It does not change the design (the preflight was already
positive-only and the code already handles `None`), but it does mean the original
short-circuit bug could only bite on 11+, which matches the reports coming from new Macs.

### macOS 15 Sequoia: apply for `com.apple.developer.persistent-content-capture`

**This is a request for the maintainer, not a code change. Do not add the key to
`src-tauri/Entitlements.plist` — Apple has to grant it first, and an unapproved
entitlement breaks signing.**

Sequoia re-asks for screen-recording consent roughly every 30 days (weekly plus
every reboot in the early betas, softened to monthly in beta 6 and relaxed further in
15.1 for regularly used apps). There is no supported way for an app to suppress the
prompt. It applies to ScreenCaptureKit clients too; the only construct that avoids it is
`SCContentSharingPicker`, which makes the user pick a capture target every time — useless
for ScreenExtend, which captures a virtual display it created itself for a device that is
already connected.

The documented escape is the `com.apple.developer.persistent-content-capture`
entitlement. Apple scopes it to **VNC applications** and grants it through a request form
on developer.apple.com with essentially no public criteria. ScreenExtend qualifies on the
merits — it is a persistent remote display of the user's own machine for the user's own
devices — so it is worth applying. Until it is granted, a host left running as a permanent
second monitor will be interrupted about once a month, and the interruption is expected
rather than a bug.

What ships in the meantime: `ensure_screen_recording_access()` now consults the live
authorization read (never frame delivery, which an unauthorized process still gets) and
logs a `[tcc]` line naming the monthly re-consent as the likely cause. It runs at every
pipeline start — every device join and every stream refresh — so a lapsed consent is
noticed there. There is still no re-probe *while* a session streams; see
`findings/major-permissions-no-periodic-reprobe-while-a-session-streams.md`.

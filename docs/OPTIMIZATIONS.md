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
| 4 | encode (Windows/AMD) | New `Backend::Amf` / `Backend::AmfCpu`: a hand-written AMF FFI (`amd_sys`) plus a latency-optimal AVC encoder, inserted into the `Auto` chain between Intel and x264. `USAGE = ULTRA_LOW_LATENCY` (with the mandatory `LOW_LATENCY` fallback), `QUALITY_PRESET = SPEED`, CBR with peak == target, `VBV = bitrate/fps`, no B-frames, no pre-encode/pre-analysis/VBAQ, `IDRPeriod = i32::MAX` with on-demand IDR, one slice, `QueryTimeout = 1 ms` set pre-`Init`, one frame in flight. | Every AMD host encodes 1080p60 H.264 on the CPU today, because `build_backend` has no AMD branch and `amd.rs` was a `bail!`-only stub. Hardware AVC on any VCN generation should land in the NVENC band (single-digit ms at 1080p) and free the CPU that currently competes with the `THREAD_PRIORITY_TIME_CRITICAL` capture threads. Within AMF, `USAGE` alone is worth ~3 frames: AMD's own doc says transcoding mode "needs to accept at least 3 input frames before any output is produced" while low-latency modes emit after the first. | not measured — no AMD hardware on either dev machine. The behaviour being replaced is `pipeline: live capture ready -- SOFTWARE x264 CPU path` on every Radeon and Ryzen host. | not measured | **UNVERIFIED** (FFI COMPILE-CHECKED by 13 assertions in `test/amd_layout.rs`, generated from the real headers by `cl.exe`; absence path tested; no frame has ever been encoded) | yes | The whole change is `amd/` + `amd_sys/` + 54 lines of `pipeline.rs`. Correctness that could be established without hardware was: every vtable slot index and total slot count, every struct layout, every enum value, every property-name string, the graceful-absence path, and the ULL-fallback decision. What cannot: whether it encodes. `docs/manual-tests/amd-amf.md` is the script that closes it in one sitting; the single most valuable line to get back is the `AMF AVC caps:` dump, which answers on hardware what the research could only guess (is `QueryTimeout` honoured, is BGRA a native input, does slice output exist). |
| 5 | ui / ipc | `getConfig()` reads the store with one `entries()` call instead of `length()` + one `get()` per key; the result is memoized, `updateConfig` issues its writes concurrently, and `publish` is gated on the serialized value (`src/components/config-provider.tsx`) | Each `db.get` is a full webview -> Rust -> webview hop and the whole store is already resident on the Rust side, so a key-by-key read is 15 serialized round-trips for data that could come back in one. `getConfig()` runs at least 5 times before the dashboard paints. | **75** store IPC round-trips on the entry -> dashboard path (5 x `getConfig` x (1 `length` + 14 `get`); derived from the code and from `plugin-store/dist-js/index.js`, one `invoke` per method) | **2** measured (`entries` x1, `set` x1) | VERIFIED | yes | Measured by proxying the `Store` instance and counting method calls, reported at `Dashboard` mount. The one `set` is the zoom persist. `reloadConfig()` exists for the single path that still needs the file: resyncing after a failed write. |
| 6 | ui / render | `GlobalProviderContext` split into nine contexts plus a never-changing setter context; the state moved into a `GlobalState` component that renders `children` untouched; `useToast`'s effect dependency fixed and 12 of its 13 call sites switched to the module-level `toast` (`src/App.tsx`, `src/components/global-provider.tsx`, `src/components/ui/use-toast.ts`) | One context object built as an inline JSX literal changes identity on every `App` render, and every `[value, setter]` tuple inside it was freshly allocated, so nothing below could bail out. Splitting alone is not enough: the whole tree was `App`'s own JSX, so `App` re-rendering re-rendered it regardless of context. | Structural: every backend event re-rendered `App` -> `HighlightProxy` -> the routed page -> `Layout` -> `Sidebar` -> `ProfileMenu` -> every QR card -> 2 components per device row. Every toast re-rendered all 13 `useToast()` consumers and re-subscribed each of them. | one `deviceModify` = **7** commits (`GlobalState` 1, `Dashboard` 2, `Layout` 2, `ProfileMenu` 2); a `deviceModify` whose payload is unchanged = **0** commits and **0** extra IPC; one toast = **0** commits outside `Toaster` | VERIFIED (after); UNVERIFIED (before) | yes | Measured with a dev-only render counter driven by real `events.deviceJoin`/`deviceModify` emissions, dashboard route mounted, one device. The before column is the structural consequence of the inline context literal, not a measurement -- the harness did not exist before the change. `Dashboard`/`Layout`/`ProfileMenu` commit twice because the `[devices]` effect sets `capacity`, which is a second render; `Layout` and `ProfileMenu` are `Dashboard`'s children, not context consumers. |
| 7 | ui / startup | The updater `check()` runs concurrently with `proceed()` and is awaited only where an update would take the screen over; saved and known devices are replayed with one `Promise.all` instead of an awaited loop; the config is read once and threaded through `runSetup` (`src/pages/bootstrap.tsx`) | `await check()` is an HTTP round-trip that gated the permission probe, the compatibility report, `commands.setup()` and therefore virtual-display creation. On an offline or captive-portal network that is the full HTTP timeout spent on a spinner, for a call that in the common case returns nothing. | offline/blackholed endpoint: the whole startup path waits for the updater's timeout | the check overlaps the startup path; a 2 s deadline bounds the pathological case | UNVERIFIED | yes | Not measured: the box has no way to blackhole the GitHub endpoint without disturbing the other workstreams. The change is a reordering, and an available update still shows the same modal and installs the same way. |
| 8 | ui / bundle | `/dashboard`, `/devices` and `/settings` are `React.lazy` in the memory router, with the dashboard chunk warmed alongside the Rust setup and the other two after it paints; `worker.format: "iife"` pinned (`src/App.tsx`, `src/lib/route-chunks.ts`, `vite.config.ts`) | The four routes were statically imported, so embla, formik and input-otp were parsed before the first frame. Parse+evaluate of one large module is the dominant startup cost in a webview, most of all on the Safari 13.1 / 2015-Intel floor. | eager `main-*.js` **914,493 B** (gzip 293.36 kB); no other app chunks | eager `main-*.js` **565,285 B** (gzip 181.17 kB), **-38.2%**; new lazy chunks: layout 68,337 B, devices 76,538 B, settings 52,772 B, dashboard 31,060 B, tooltip 92,226 B, two shared 17,796 / 15,095 B | VERIFIED (bytes); UNVERIFIED (parse time) | yes | Byte counts from `pnpm build`. React Router 7 wraps navigations in `startTransition`, so a not-yet-loaded route holds the current screen instead of flashing its fallback -- that plus the prefetch is what keeps the split invisible. Monaco stays out of the entry chunk (2,957,045 B, lazy, unchanged). `worker.format` was previously whatever Vite defaulted to; a module worker cannot be constructed on 13.1 and would silently disable the settings editor's language services. |
| 9 | ui / startup | The monaco warm-up moved from `setTimeout(300)` at bootstrap mount to two animation frames plus an idle callback after the dashboard paints (`src/lib/preload-editor.ts`) | The preload itself is worth keeping -- the recovered measurements are ~2.2 s warm / 4.3 s cold to import and parse the chunk, ~100 ms for `editor.create`, and 8 ms vs 99 ms for a second create -- but at T+300 ms from bootstrap mount all of that overlapped `commands.setup()`, the config reads and any early WebRTC negotiation. | ~2-4 s of main-thread scripting starting 300 ms into startup | the same work, started after the dashboard's first paint; `requestIdleCallback`'s `timeout` keeps the 300 ms deadline | UNVERIFIED | yes | `requestIdleCallback` was rejected the first time for two reasons and both are now answered: the floor gets it from `legacy-webview-polyfills.ts`, and the `timeout` option means it fires within 300 ms whether or not the thread goes idle, so it still cannot lose the race against a user opening Settings. The four rationale comment blocks that existed only in dangling commit `c5bf2afe` are restored verbatim. |
| 10 | ui / render | The log terminal buffers incoming lines in a ref and flushes on a `requestAnimationFrame`, and rows are keyed by a monotonic sequence number (`src/components/log-terminal.tsx`) | Per event: one array spread, a second slice once the 2000-line buffer is full, a commit over the whole list, and -- because rows were keyed by array index while the window slides -- a re-paint of all 2000 divs rather than one append. Then a forced layout read per line. | 1 commit + 1 forced layout per `LogLine`; up to 2000 rows re-painted per event | 1 commit + 1 forced layout per frame regardless of burst size; an append mounts one node | UNVERIFIED | yes | Not measured: reproducing a burst needs a `tprintln!` loop on the Rust side, which is W13's territory. Rendering is unchanged -- same lines, same order, same scroll-when-pinned behaviour, still 2000 lines in the buffer and all of them rendered. |
| 11 | ui / ipc | The hosted-network credentials persist 400 ms after typing settles and skip the write when the pair is unchanged; the zoom factor persists 500 ms after the last step (`src/pages/settings.tsx`, `src/App.tsx`) | Both were wired straight to a state effect, so a 12-character password was 12 store writes and 12 global config notifications, each re-rendering the profile menu, any open device sheet and the config editor's subscriber, and re-running an O(devices x saved-devices) diff. | 12 IPC writes + 12 global notifications per 12 characters typed; 5 writes for 5 zoom steps | 1 and 1 | UNVERIFIED | yes | Derived from the code path, not instrumented -- the same store-call counter used for row 5 would measure it, but it needs a human at the keyboard. Nothing reads these values back from the store while the page is open (the Save button works off React state), so the delay is not observable. |
| 12 | ui / css | `button, input { transition: all 200ms !important }` replaced with an explicit property list -- colour, background, border, outline-color, box-shadow, opacity, fill, stroke (`src/index.css`) | `all` animates the layout-affecting properties too, so every hover, focus or disabled flip on a button or input forced layout and paint on each frame of the 200 ms window, on the main thread, on a host that is simultaneously capturing and encoding a display. | every animatable property, including width, height, padding, margin, top/left and font-size | eight compositor- and paint-only properties, same duration, same easing | UNVERIFIED | yes | Not measured -- a devtools performance recording of a hover needs a human. Audited by hand instead: nothing in the kit transitions geometry on a `button` or `input` (`button.tsx` size variants are static per instance, the switch thumb and the toast root are not buttons), so the removed properties had nothing to animate. The `!important` was deliberately kept; see the recommendations below. |
| 13 | compat | Floor repairs behind `@supports not (accent-color: red)`: a `:focus` ring, the `[hidden]` and `input[type=button]` preflight resets, and a `left` fallback for `.start-1` (`src/index.css`) | `:focus-visible` is Safari 15.4 and the kit's entire keyboard-focus affordance is built on it, while `src/index.css` had already suppressed the UA outline -- so tabbing through the app on macOS 10.15 produced no focus indicator at all. Preflight's `:where()` rules die whole on 13.1 for the same parse reason. | no focus ring anywhere on the 10.15 floor; `[hidden]` elements still displayed | a visible ring at every tab stop, resets restored | COMPILE-CHECKED | yes | Verified in the built stylesheet, not on a 10.15 machine -- this box is Windows. `accent-color` shipped in the same Safari release as `:focus-visible`, so the guard is an exact capability proxy: no browser that renders the app today enters the block. CSS grew 52,317 -> 52,919 B. |
| 14 | build | `scripts/check-legacy-webkit.mjs` now walks the tree recursively, reads CSS and HTML, verifies each `cover: "shim"` against a live function in `legacy-webview-polyfills.ts`, uses per-check guards anchored to the statement, reports guard-suppressed hits, and scans the client page under a stricter contract | The script was being treated as the "will this load on the floor?" gate but only read `.js` files in one directory, could not see a single CSS hazard, took a `cover: "shim"` annotation as proof, and dropped real hits whenever an unrelated `&&` appeared within 100 characters of them in minified output. | 0 findings; 5 CSS hazards and 6 client-page hazards invisible to it | 5 CSS rows with explicit cover reasons, 6 client-page rows as known debt, exit 0 | VERIFIED | yes | Both failure modes tested: deleting `shimReplaceChildren`'s call site makes it exit 1, and adding a throwaway `:is()` rule to the built CSS makes it exit 1. The client-page `inset:` hits became `findings/minor-client-inset-shorthand-above-the-mobile-floor.md`. |
| 15 | observability | `log_frontend_error` Tauri command plus a `console.error` / `window.onerror` / `unhandledrejection` forwarder (`src-tauri/src/logbus.rs`, `src/lib/error-reporting.ts`) | Roughly seventy `console.error` calls in the desktop UI, and in a release build every one goes nowhere: no devtools are attached and nothing captures the webview console. | frontend failures invisible in a shipped build | they appear in the Settings log terminal, `get_log_backlog` and `ScreenExtend logs`, prefixed `[ui]` | VERIFIED | yes | Confirmed live during the instrumented `pnpm tauri dev` run: an uncaught `TypeError` and a React warning both arrived on the bus. Wrapping the console rather than editing each call site also catches uncaught exceptions and rejected promises, which never reach `console.error`. The command lives in `logbus.rs` next to `get_log_backlog` rather than being triplicated across the three `*_utils` modules -- it has no OS-specific behaviour, and that is the precedent `get_log_backlog` already set. |
| 16 | bwe | `RemoteInboundRTPStats::round_trip_time` read as milliseconds, not seconds; absolute 80 ms congestion threshold replaced by `RttCongestionMonitor` (rolling baseline, >40 ms excursion for two consecutive polls); 250 ms floor on downward moves. | webrtc-rs 0.17 already returns this field in milliseconds (`interceptor-0.17.1/src/stats/interceptor.rs::calculate_rtt_ms`), not the W3C seconds unit. Multiplying by 1000 read a 5 ms LAN RTT as 5000 ms. | a synthetic 5 ms RTT series walks the target **40 Mbps -> 1 Mbps in ~2 s** and pins there | the same series **holds 40 Mbps for 200 polls** (`lan_rtt_never_cuts_the_target`); a steady 150 ms WAN RTT also holds; a sustained excursion above baseline still clamps | VERIFIED (unit tests) | yes | Every session on every link sat at the 1 Mbps floor within seconds, which made every latency and quality measurement before this meaningless. An absolute threshold pinned every WAN session even with the unit fixed, hence the rolling baseline. |
| 17 | encode | Per-device quality slider maps to `EncoderConfig::min_qp` (a QP *floor* under CBR/ABR) instead of `Config::qp` (constant-QP rate control). `set_bitrate` returns `BitrateApplied::{Applied, IgnoredConstantQp}` instead of a silent `Ok(())`. | The desktop UI writes a device override for essentially every device, so the slider put every real session into CONSTQP, where `set_bitrate` did nothing while the log claimed an adaptation. | congestion control inert on every session that had a device override, i.e. effectively all of them; log printed "adapting bitrate: X -> Y bps" for a change that never happened | CBR/ABR retained on all four backends (NVENC `enableMinQP`, Intel `mfxExtCodingOption2::MinQP*`, x264 `rc.i_qp_min`, VideoToolbox `MinAllowedFrameQP`); an ignored set_bitrate is now reported once per encode thread | COMPILE-CHECKED (NVENC/Intel/x264 exercised by unit tests; end-to-end effect not yet measured) | yes | x264 also gained a real constant-QP implementation; it previously ignored `qp` at init but still refused `set_bitrate` because of it. |
| 18 | telemetry | New `streamer/telemetry.rs`: `FrameTiming` per frame, lock-free log-scale histograms per stage, 12 degradation counters; OS capture timestamps (WGC `SystemRelativeTime`, DXGI `LastPresentTime`, CGDisplayStream `displayTime`) converted onto the shared host clock; client stats over `OP::STATS`. | Nothing could be optimized because nothing could be measured. The only signal was an *average* printed every 60 frames on the WGC classic path, and the NVENC ring path — the default on any NVIDIA host — returned before reaching even that. | no per-stage numbers at all on the default path; three of four capture backends discarded the OS frame timestamp; no way to learn which client fast paths were live | p50/p90/p99/max for eight stages, per session, in `serve -v` and `ScreenExtend status --json`; see docs/BASELINE.md: capture->socket p50 1.57 ms / p90 8.39 ms on NVENC+WGC, 11.53/16.78 on the Intel CPU bridge | VERIFIED (Windows WGC + NVENC ring); COMPILE-CHECKED (macOS) | yes | Measurement apparatus, not an optimization: it adds four relaxed atomics per frame per stage and no allocation. Its own cost is inside the noise of the stages it measures — `packetize` p50 is 0.11 ms and `encode_queue` p50 is 0.02 ms, both of which include their own instrumentation. |
| 19 | audio-win | WASAPI packets stamped with `GetBuffer`'s `qpcpos` (the endpoint's own read time, converted to the host clock) instead of `host_now_ns()` at encode time. | The drain loop read `qpcpos` into a variable and discarded it, so `capture_ns` folded the drain interval, the Opus encode and any scheduling delay into what the client is told is the capture instant — and that is the number A/V sync is built on. | every packet in a drained burst claimed the same capture time, late by however long the capture thread took to reach it | the accumulator is anchored to `qpcpos` and frames within a burst are spaced by exactly one 5 ms frame period; `TIMESTAMP_ERROR` or a zero `qpcpos` falls back to the old behaviour | COMPILE-CHECKED | yes | Verifying the numbers needs a client with system audio enabled and a camera or a scope; not done in this run. The macOS side has the same defect and is still open (`findings/major-audio-mac-capture-ns-is-drain-time.md`). |
| 20 | net-win | `watch_for_network_changes` re-enumerates on `NotifyIpInterfaceChange` / `NotifyUnicastIpAddressChange` (debounced 250 ms) with a 30 s safety poll, instead of `loop { sleep(1s); 3 WMI queries }`; the Wi-Fi Direct adapter's label reads `AppState.hosted_network_running` instead of calling `is_hosted_network`, which spawned `netsh`. | The watcher ran for the life of the process next to capture and encode. Each tick was three `root\StandardCimv2` CIM round trips through `WmiPrvSE.exe` plus, on any laptop carrying the Wi-Fi Direct virtual adapter (the default on Windows 10/11 Wi-Fi machines), a `netsh.exe` process creation - a periodic, avoidable wakeup that should show as frame-pacing wobble on a constrained host. | 60 s of idle `ScreenExtend serve` on Win11 26200: **46 `netsh.exe` spawns observed** (60 by construction - one per tick; a 100 ms sampler misses the shortest-lived), ScreenExtend's own CPU **0.188 s (0.31% of one core)**, which excludes the work charged to `netsh.exe` and `WmiPrvSE.exe`. Unit costs measured separately: `netsh wlan show drivers` 185-230 ms wall (5 runs), the three CIM queries p50 114 ms / p90 1027 ms, a full tick p50 391 ms. | Same 60 s window, same binary: **0 `netsh.exe` spawns**, ScreenExtend CPU **0.000 s (0.00% of one core)**. The adapter list is still correct (`serve` printed `Sreenilayam-5g  http://172.16.11.76:8080/`). | VERIFIED | yes | Measured A/B on one binary: the before figure comes from a scratch patch that restored the 1 Hz sleep and the per-tick `netsh` spawn in the same build, then reverted. Detection is now *faster* than the old poll, not slower - the debounce window bounds it rather than a whole second. The same commit stops a transient WMI failure from presenting as "this machine has no network adapters", which `sync_streamers` answered by shutting down every streamer. |
| 21 | net-win | Hosted network became a probed ladder - Mobile Hotspot (`NetworkOperatorTetheringManager`, 6 GHz > 5 GHz > auto), then Wi-Fi Direct legacy AP, then `netsh` legacy, then unsupported - and "started" now requires the AP's virtual adapter to hold a routable IPv4. | `start_wifi_direct_` opened with `GetInternetConnectionProfile()?`, so the feature whose purpose is joining a client where there is no router could not start without internet. The tethering manager it built was read once for a property and dropped; the AP that actually ran was Wi-Fi Direct legacy mode, which exposes no band or channel control at all and typically lands on 2.4 GHz. | offline: hosted network cannot start. Online: 2.4 GHz Wi-Fi Direct, no band control, and `hosted_network_running` set from our own API call rather than from the radio. | Mobile Hotspot with a chosen band; on this adapter `IsBandSupported` reports Auto/2.4/5 GHz true and 6 GHz false, so it takes 5 GHz. Start is verified against the adapter's IPv4 and retried with backoff. | VERIFIED (the capability probe and band support, on this box); UNVERIFIED (throughput and RTT on 5 GHz vs 2.4 GHz - needs a second device and `iperf3`) | yes | The measurement that unblocked this: `GetTetheringCapabilityFromConnectionProfile` returns **`Enabled` (0)** from the unpackaged `ScreenExtend` binary on Windows 11 26200, and `CreateFromConnectionProfile` succeeds on *every* connection profile including Ethernet and a saved-but-disconnected Wi-Fi profile. Microsoft documents `DisabledBySystemCapability` (7) as the signal that the caller lacks the `wiFiControl` manifest capability; we never get it, so an unpackaged desktop app is not blocked. `MaxClientCount` is 8. Reproduce with `cargo test --lib hosted_network -- --ignored --nocapture`. |
| 22 | compat-win | `CompatibilityReport` gains `capture_backend`, `encoder_backend` and `hosted_network_backend`; `audio_backend` and `display_backend` stopped being string literals and are probed. `os_supported` is a capture-backend probe rather than `CurrentBuild >= 19041`, and the `UniversalApiContract 8` conjunct is gone from the WGC probe. | The panel is the user's only self-diagnosis surface and could not tell them the two things that most often go wrong: the display driver is not installed, and you are on the x264 CPU fallback. It reported `display_backend: "IddDriver"` on a machine that had never installed the driver. | two hardcoded strings; no encoder tier anywhere in the report; `ProductName` reads "Windows 10" on Windows 11. | this box reports capture `WindowsGraphicsCapture`, encoder `nvenc`, display `IddDriver`, audio `wasapi`, hosted network `MobileHotspot`, on "Windows 11 25H2 (build 26200)". A missing or unresponsive driver is a *blocking* entry; a WARP-only machine gets an entry saying so. | VERIFIED (positive paths on this box); UNVERIFIED (the DriverNotInstalled and software-adapter branches - this machine has both the driver and two real GPUs) | yes | No UI was added: the fields are printed by `ScreenExtend doctor` (and `--json`), and the driver entry hangs off the "Install drivers" action the compatibility panel already has. |
| 23 | airplay | `MirrorSink::run` accepts in a loop instead of serving one connection and dropping its listener; the per-connection latches (`connected`, `geometry_changed`) reset per connection while the cumulative counters carry on (`macos_utils/airplay/mirror.rs`) | The receiver deliberately outlives a session, and the mirror `dataPort` it hands back in every type-110 SETUP is cached for the receiver's whole life — so the second AirPlay session of a process's life pointed macOS at a listener that no longer existed. | live harness on 10.15.8, `cargo test --lib macos_utils::airplay::tests::live_end_to_end -- --ignored --nocapture`: first session attaches in **2.1 s**; **second session FAILED** — "macOS did not attach a display within the timeout", no mirror connection at all, full 40 s `SESSION_BUDGET` burnt. Whole harness **51.15 s**. | same command, same box: first session **2.3 s**, **second session 1.3 s**, both attach as extended desktops at 1920x1080, both tear down cleanly (0.6 s / 0.4 s). Whole harness **12.67 s**. | VERIFIED (10.15.8) | yes | The blocker was predicted from the code by discovery and is now confirmed on hardware in both directions. The failure was silent in the worst way: `diagnose()` reported "the sender never opened the mirroring connection", which reads as a sender problem. Note the second-session time is *lower* than the first because the receiver is warm and the picker already lists us. |
| 24 | airplay | SystemUIServer's pid comes from `proc_listpids`/`proc_name` (libproc, libSystem since 10.5) and is cached, revalidated by name; poll interval widens 25 ms -> 100 ms after the first second (`macos_utils/airplay/ax.rs`) | `find_displays_extra_once` opened with `Command::new("/usr/bin/pgrep")` — a fork+exec+wait — and is called in a 25 ms retry loop, up to 200 times per attempt and 3 attempts per connect. Up to 600 process spawns for one failed join, on exactly the old hardware this tier exists for. | measured on 10.15.8, 200 iterations each: **16.35 ms per `pgrep` fork**. At the 600-call worst case that is **9.8 s** of pure fork overhead inside an 18 s budget — over half of it our own. | **521 µs** per `proc_listpids` scan (31x), **5.2 µs** per cached lookup (3100x). With the poll backoff the call count also drops from ~600 to ~240, so the worst case goes 9.8 s -> **1.2 ms**. | VERIFIED (10.15.8) | yes | Measured with a throwaway `#[ignore]`d benchmark in `ax.rs`, removed before commit. The cache revalidates with one `proc_name` call, so a restarted SystemUIServer is self-healing rather than a stale pid. Correctness confirmed against `pgrep -x SystemUIServer`: both return 412. |
| 25 | airplay | Discovery settle cut from a flat 6000 ms to 500 ms; the other displays' restore and the AirPlay display's own mode change merged into one `CGBeginDisplayConfiguration` commit; `server.rs`'s follow-up mode set no longer routes through `CGDisplaySetDisplayMode` (`macos_utils/airplay/{mod,topology}.rs`, `macos_utils/virtual_display.rs`) | A cold join paid a flat 6 s sleep for a wait that already exists one step later and does it better — `ax::connect_to` holds the picker open and re-reads it for its whole 6 s device-row timeout, because the picker fills device rows in as discovery answers. And the post-attach path committed three consecutive display configurations, each of which blanks every attached screen for about a second, with settle polls hung off two of them. | 3 WindowServer reconfigurations after attach + a 4th from `server.rs`; 6 s flat on a cold start | 2 reconfigurations (un-mirror, then one combined restore+mode commit); the 4th is skipped when the display is already at the target size and otherwise goes through `kCGConfigurePermanently`; 500 ms flat | VERIFIED (10.15.8 — the live harness join times in row 23 are with all of this in) | yes | The 4th call mattered beyond its cost: `CGDisplaySetDisplayMode` scopes the change to the calling process, which `topology.rs` measured as macOS holding the AirPlay display open 48 s past the end of its session. The tier had gone out of its way to avoid that call and `server.rs` was making it anyway, on the display the tier had just created. |
| 26 | airplay | `ax::children()` returns an owning `Children` (the CFArray plus its borrowed element pointers) instead of `std::mem::forget`ing the array; anything that must outlive it is `CFRetain`ed (`macos_utils/airplay/ax.rs`) | `CFArrayGetValueAtIndex` returns a borrowed reference, so the array had to outlive the pointers — arranged by leaking it. Every call leaked one CFArray plus a Mach port send right into SystemUIServer per child, from inside poll loops that run for seconds. | ~1000-3000 leaked CFArrays per failed `connect_to`, unbounded across a host left running with clients joining and leaving | none: the array drops with the borrow, retained elements drop with their owner | COMPILE-CHECKED (leak counts not instrumented) | yes | Not measured with `leaks`/`lsmp` — the ownership change is provable by inspection and the live harness exercises every one of the five call sites, but the *rate* of the old leak was never quantified. It was the correct shape of fix regardless: `OwnedElement` already existed as the RAII wrapper. |
| 27 | airplay | `kDNSServiceFlagsNoAutoRename` (0x8, was defined as 0x0 = no flags at all) with a real `DNSServiceRegisterReply` and a bounded one-shot pump (`macos_utils/airplay/dnssd.rs`) | With auto-rename on and a NULL callback, a name collision silently became "ScreenExtend (2)", which `ax::connect_to` matches by exact title and therefore never finds — an 18 s failure on a device that is right there. | 0x0: rename is silent and unreportable | 0x8 + callback: a conflict is a `kDNSServiceErr_NameConflict` with a message that says what happened | VERIFIED (10.15.8) | yes | **The interesting measurement is the one that says the obvious fix does not work.** 0x8 with a NULL callback returns `kDNSServiceErr_BadParam` (-65540) on any interface, every time; the identical call with a callback returns 0. The flag's meaning is "report the conflict", and a NULL callback leaves nowhere to report it. Caught by the live harness — the receiver would not publish at all, which is worse than the bug being fixed. The pump is one `poll` + one `DNSServiceProcessResult` at publish time, so there is still no background event loop. |
| 28 | client / transport | `RTCRtpScriptTransform` preferred over `createEncodedStreams`; a 2.5 s watchdog steps down to the legacy transform rather than to `<video>`; new `KEYFRAME_REQUEST` (`0x64`) on the reliable input channel when neither transform can ask (`static/index.html`, `static/transform-worker.js`, `static/input.js`) | `useScriptTransform` was `hasScriptTransform && !hasLegacyEncodedStreams`, and Chrome 141+ ships both — so every Chrome and Edge client took the legacy branch. The encoded transform runs *instead of* libwebrtc's reference finder, so nothing downstream ever emits a PLI: a receiver without `sendKeyFrameRequest()` cannot ask for an IDR at all. | Chrome/Edge: any lost or corrupt keyframe froze the picture, then `handleRenderCount` demoted the session to `<video>` after ~4 s — permanently, for that session, taking libwebrtc's ~90 ms receive path back with it | tier 1 asks in ~1 RTT; where only the legacy transform exists the request goes out of band; demotion happens only when the request had nowhere to go | **UNVERIFIED** | yes | Neither dev machine can run the client page (Windows has no second device attached to the test host; the Mac is the 10.15 floor and its Safari is 13.1). The out-of-band tier is *inert* until the host parses `0x64` — `findings/major-host-keyframe-request-opcode-unhandled.md`. Verification script: `docs/manual-tests/client-page.md` §2. |
| 29 | client / decode | Backpressure moved from `decodeQueueSize` to outstanding decoder *outputs*, target 1, with `ondequeue` clearing a stale backlog; `isConfigSupported` given the same config `configure()` receives, plus the real coded size; acceleration probed `no-preference` then separately `prefer-hardware` for telemetry only; bounded decoder resets (`static/transform-worker.js`) | `decodeQueueSize` counts acceptance, not completion — it drops as soon as the control message is handed toward the codec and can read 0 with frames in flight — so the `> max(3, fps*0.05)` rule fired on noise, and twice in a row it latched `waitingForKey` and discarded everything until a keyframe. Probing `{codec}` and configuring `{codec, optimizeForLatency}` meant a UA could accept the probe and throw on the configure where nothing was watching. | a decoder holding two frames could read `decodeQueueSize === 0`; up to 33 ms of queued work at 60 fps before the threshold tripped; a decode error rebuilt the decoder on every frame | in-flight outputs gate submission; `framesDropped` counts real drops; at most three rebuilds before the page is told and can fall back | **UNVERIFIED** | yes | `optimizeForLatency: true` is kept: it is a no-op on every Chromium hardware backend and dropped by WebKit, but Gecko plumbs it fully (`AV_CODEC_FLAG_LOW_DELAY`, `CODECAPI_AVLowLatencyMode`, VideoToolbox reorder depth 0). `prefer-hardware` is deliberately *not* used to configure: Chromium's version installs a GPU-only decoder factory with no software fallback, so it reports unsupported on machines that would decode the stream fine. |
| 30 | client / render | Three-argument `drawImage(frame, 0, 0)`; canvas re-sized whenever the frame size changes instead of latching on the first frame; `desynchronized` removed; opt-in `VideoTrackGenerator` tier behind `?present=track` (`static/transform-worker.js`) | The five-argument destination form applies a scale even at 1:1. `desynchronized` is implemented in Chromium only for `HTMLCanvasElement` (`LowLatencyEnabled` lives in `html_canvas_element.cc`; `offscreen_canvas.cc` overrides none of it), so on the OffscreenCanvas it did nothing but mislead a reader. | an implicit scale per frame; a mid-stream resolution change was rescaled, never re-fitted | 1:1 blit; correct fit at any resolution | **UNVERIFIED** | yes | The canvas path is left as the default deliberately. Worker OffscreenCanvas already goes `DidDraw` → `SetNeedsBeginFrame` → the display compositor at the next vsync without touching the main thread, i.e. ≤1 vsync; the research found **no** public evidence that a track generator wins a hardware overlay, and the constructor exists on Chrome (legacy name) and Safari 18 (new name) and nowhere else. Making it the default on that evidence would be a guess, so it is an A/B instead. |
| 31 | client / join | Microphone prompt removed from the join path (output enumeration runs after the first frame); ICE gathering ends a grace period after the last useful candidate instead of at the deadline; measured refresh rate folded into the join request rather than applied by a post-live renegotiation (`static/index.html`, `static/audio.js`) | Three things stood between the user and the first pixel, in series: the mic prompt (12 s cap) gated ICE, which gated the WHEP post, which gated the host creating the virtual display; then `refineRefreshRate()` tore the whole session down about a second after it went live. None of the three is about video. | mic prompt 0-12 000 ms, ICE 0-3000 ms (6000 with TURN), then a full teardown and rebuild on every 90/120/144 Hz client — which also retakes the host's `DISPLAY_CORRELATION_LOCK` | mic prompt off the path entirely; ICE ends ~200 ms after the last host/srflx candidate (~400 ms after a relay candidate when TURN is configured); the refresh rate is right in the first offer, with a 400 ms measurement deadline | **UNVERIFIED** | yes | The ICE change is client-only and correct against both old and new hosts, because WHEP is still a single non-trickle POST — it just stops waiting for STUN results a LAN connection will never nominate. Real trickle needs the host to accept a PATCH: `findings/major-whep-no-trickle-ice.md`. |
| 32 | client / audio | Fast path chosen from `AudioDecoder.isConfigSupported({codec:'opus'})` awaited before the offer, not from constructor existence (`static/audio.js`, `static/index.html`) | The host reads `audioCapabilities` once. Told the client is fast it sends Opus on a DataChannel and puts **no audio track in the SDP at all**, so a decoder that then refuses `opus` is permanent silence with no way back. Safari 26 and Firefox 130 have only just shipped `AudioDecoder`, so the constructor sniff went from imprecise to dangerous. | a Safari 26 client whose Opus decode fails gets silence and no diagnostic | the same client is negotiated onto the browser's own Opus track | **UNVERIFIED** | yes | A decoder that passes the probe and fails anyway is still possible; the worker now reports `decoder-failed` after three rebuilds, and the page attaches the fallback stream if one exists. It only exists if the host put a track in the SDP, which today it does not when the client claimed the fast path — `findings/major-host-audio-track-absent-when-client-claims-fast-path.md`. |
| 33 | client / audio | Underrun no longer re-primes the jitter buffer on the first starved block (four consecutive), and both the worker and the worklet conceal instead of outputting silence; sequence gaps detected in the worker (`static/audio-worklet.js`, `static/audio-worker.js`) | `priming = true` after any underrun means digital silence until the ring refills to the target. The worker had dedup and late-drop but no gap detection at all, so a single lost packet arrived as an empty ring. | one lost 5 ms packet cost 10-40 ms of silence — described in the audio research as the largest audible-quality lever in the client | the gap is filled with an attenuated repeat (≤6 packets in the worker, a fading last quantum in the worklet); a real stall still ends in silence, without the click | **UNVERIFIED** | yes | Concealment by repetition is what a codec's own PLC does without side information; WebCodecs `AudioDecoder` exposes no PLC entry point. `FLAG_DISCONTINUITY` is now honoured as an immediate resynchronisation point and suppresses concealment, since there is nothing there to bridge. |
| 34 | client / audio | `outputLatency` estimated from `getOutputTimestamp()` where the browser does not report it; the commanded depth gains an adaptive margin that grows on a real underrun and decays on a clean link (`static/audio.js`) | `outputLatencyMs()` treated a missing `outputLatency` as zero, so the A/V target was too large by exactly the output latency and audio played early. | Safari < 18.4, Chrome < 102 and the 10.15-floor WebView: early by 3-10 ms on a wired output, 100-200 ms on Bluetooth | measured where available, otherwise a low-passed estimate re-taken on every sink change, with the source (`measured`/`estimated`/`baseLatency`/`unavailable`) in the telemetry | **UNVERIFIED** | yes | The estimate is `ctx.currentTime − (ts.contextTime + elapsed)` from one `getOutputTimestamp()` sample, clamped to [0, 500] ms. It is not exact — the two clocks are not read atomically — but it is the right order of magnitude, which zero was not. |
| 35 | client / input | Coordinates normalised against the `object-fit: contain` picture rect; `getCoalescedEvents()` extended from pen to touch and mouse, capped at 8 samples per flush; Pointer Lock asks for `unadjustedMovement` (`static/input.js`, `static/index.html`) | Normalising against the stage put every touch off by half the letterbox bar. Coalesced samples were read only for pen, so touch and high-rate mice were decimated to one sample per task by the `MessageChannel` flush. Pointer Lock without `unadjustedMovement` applies the client OS pointer curve, which then compounds with the host's. | a tap in the bar was delivered as a tap on the desktop edge; a drag was a polyline of whatever landed on a task boundary; the cursor accelerated twice | coordinates land where the user pointed; up to 8 intermediate samples per flush; raw deltas where the platform has them | **UNVERIFIED** | yes | The cap is deliberate: the host replays a batch one injection per sample (`findings/major-input-batch-replayed-one-syscall-per-sample.md`, W10), so an uncapped 1000 Hz burst would cost it more than the fidelity is worth. Eight per flush is ~one sample per 2 ms at 60 Hz. Single-sample moves still go as a full pointer message, which carries contact width and height the batch format has no room for. |
| 36 | client / session | Screen Wake Lock as a tier above `nosleep.js`; `nosleep.js` constructed lazily and muted; `enterFullscreen()` always returns a Promise; `inset: 0` replaced by long-hands (`static/index.html`, `static/styles.css`) | Only the unprefixed `requestFullscreen` returns a Promise. Safari below 16.4 returns `undefined`, so every `.catch()` on it threw a `TypeError` — and because `startLock()` is called un-awaited from `onStreamLive()`, that rejection silently skipped pointer lock, keyboard lock and the wake lock for the whole session. `const noSleep = new NoSleep()` sat at the top level of the one shared inline script, so a failed `/nosleep.js` fetch took the join form and the submit handler with it. | Safari < 16.4: no pointer lock, no keyboard lock, no wake lock, no diagnostic. Mobile WebKit < 14.1: six full-bleed layers collapsed, because `inset` is ignored whole. | each degradation is a bit in the stats frame; the wake lock is the real API where it exists; the layers have long-hands | **UNVERIFIED** | yes | `nosleep.js`'s video is muted, which also relaxes the autoplay rules that could refuse it. The `inset` change removes both `KNOWN_DEBT` entries from `scripts/check-legacy-webkit.mjs`; the client-page pass now reports zero. |
| 37 | client / observability | Stats frame v2 (96 bytes, first 72 byte-identical to v1): four tier bytes, a 22-bit degradation field, the swallowed-error count and three audio numbers; 49 bare `catch (_) {}` across `index.html` and `audio.js` now count what they drop (`static/input.js`, `static/index.html`, `static/audio.js`, `static/transform-worker.js`) | The client audit found 63 `catch (_) {}` and 22 distinct silent degradations, none of them visible to the host. A device on the `<video>` fallback with an estimated output latency and no wake lock looked, from the host, exactly like one on every fast path. | 0 of 22 degradations reportable | 22 bits plus four tier bytes, once a second on the reliable channel; `SEVideo.getStats()` returns the same values for a person holding the phone | **UNVERIFIED** (the frame is asserted byte-for-byte by `node scripts/test-client-stats.mjs`, which is COMPILE-CHECKED) | yes | The host does not yet parse past byte 72 — `parse_client_stats` reads the v1 prefix and stops — so the tail currently reaches the host and is discarded: `findings/major-telemetry-client-stats-v2-tail-is-discarded.md`. Until that lands the fields are read on the device. Appending was still right: the wire format had to change once, and doing it before the host side means no second migration. |
| 38 | macOS / hosted network | `startHostAPModeWithSSID:securityType:channel:password:error:` and `stopHostAPMode` guarded by `respondsToSelector:`; a probed tier (HostAP SPI -> IBSS -> Unsupported); a most-secure-first `securityType` ladder replacing WEP-or-open; the open fallback moved behind every secured attempt on every channel; start verified against `interfaceMode()` and a routable IPv4 on the interface; a liveness watcher; AWDL social channels 6/44/149 dropped from the channel preferences. | A bare `msg_send!` to a selector `CWInterface` may not implement raises an Obj-C exception through a non-unwinding Rust frame, which aborts the process — so on any Mac without the SPI, "start hosted network" made the app vanish. Separately, "started" meant the SPI returned YES, which says nothing about whether a client can associate or get a DHCP lease. | selector sent unguarded; WEP or a silent open AP; success = the SPI's return value; channel 44 preferred | all seven selectors probed on 10.15.8 and **all present**; secured ladder walked once per process and remembered; success = AP mode + a routable IPv4, logged separately when only the first holds | VERIFIED (probe, on 10.15.8); UNVERIFIED (association, DHCP, throughput) | yes | Association could not be tested: the dev box's only Wi-Fi interface (`en3`) carries the SSH session this work runs over, and starting an AP drops it. Script: `docs/manual-tests/macos-hosted-network.md`. The security-type sweep ships as `macos_utils::hosted_network::tests::sweep_security_types` (`--ignored`). |
| 39 | macOS / virtual display | `create_display_with_modes` implemented (the trait default discarded the ladder); HiDPI derived from the ladder so the display is born at the client's DPR; the mode list carries the primary mode, its transpose and the ladder's other resolutions plus transposes; `reconfigure_display` skips `applySettings:` when the geometry is already registered at the same HiDPI, and no longer holds the registry mutex across WindowServer IPC; `set_display_scale` works from the remembered backing geometry rather than `CGDisplayPixelsWide` (points); `setTerminationHandler:` installed behind `respondsToSelector:`; `activate_mode` polls at 5 ms for 250 ms then 25 ms. | Every Retina join created the display at 1x with one mode and then paid a second `applySettings:` plus a full `activate_mode` — a mode-find loop of up to 1.5 s and a settle loop of up to 1.5 s — inside `DISPLAY_CORRELATION_LOCK`, where every other joining client waits. | 2 x `applySettings:` + 2 x `activate_mode` per Retina join, the second searching for a mode that did not exist yet (up to 3 s); orientation flips and in-ladder geometry changes were full mode-list rewrites | 1 x `applySettings:` + 1 x `activate_mode` that finds its mode on the first `CGDisplayCopyAllDisplayModes`; the post-join scale call is an early return; orientation flips are a plain `CGDisplaySetDisplayMode` | COMPILE-CHECKED | yes | **Not measured.** `time_a_retina_join` is checked in as an `--ignored` A/B of both sequences, but it must run in the console session: over SSH the process has no WindowServer connection and `initWithDescriptor:` / `applySettings:` **block indefinitely** rather than failing (measured — the test had to be killed twice). |
| 40 | macOS / compatibility | `NSProcessInfo.operatingSystemVersion` (cached) replaces three `sw_vers` forks per report; `capture_backend()` is an `AnyClass::get` on `SCStream`/`SCShareableContent`; `hosted_network_backend()` exposes the HostAP tier; `UnsupportedApi` entries added for the AirPlay single-display cap and for a Mac that can host no network. | The report forked three processes on the main thread from a command the bootstrap screen calls twice, and gated the capture tier on a version string in a code path whose correct probe was one line away. The capture tier — the one with the largest latency and quality consequence — was never reported at all. | 3 x `posix_spawn` per `check_system_requirements`; no capture tier; the AirPlay one-display cap surfaced only when a second device failed to connect | 0 subprocesses; capture and hosted-network tiers probed by class lookup; both caps reported as `UnsupportedApi` entries | VERIFIED (0 forks, 10.15.8) | yes | Exposed as `pub fn` accessors rather than new `CompatibilityReport` fields: W9 is adding `capture_backend` / `encoder_backend` / `hosted_network_backend` to that struct on the Windows branch. Wiring recorded in `findings/minor-compat-mac-capture-and-hostap-tiers-not-in-the-report-struct.md`. |
| 41 | macOS / network watcher | One `AF_INET` socket for the whole enumeration (was two per interface); functional type cached per interface name; `is_hosted_network` asked once per poll rather than once per Wi-Fi interface; the last readable SSID remembered; the watcher compares `(interface_index, ip_addresses)` instead of the whole struct; `cli_adapters` and `get_network_adapters` collapsed into one walk. | At 1 Hz forever, the watcher opened and closed a dozen sockets, ran a dozen ioctls and made two to four `airportd` XPC round trips per second at idle — and a nil SSID read (which needs Location authorization on 10.15+) counted as an adapter change and re-ran `sync_streamers`. | ~12 socket+ioctl pairs/s and 2-4 CoreWLAN XPC calls/s at idle; a name-only flip re-ran the reconciler | 1 socket + N ioctls per poll, 1 CoreWLAN call, and no resync for a name-only change | COMPILE-CHECKED | yes | The `SCDynamicStore` subscription that would remove the poll entirely is left as a finding; this change removes the per-interface cost, not the 1 Hz cadence. |
| 42 | macOS / session | `set_server_ports` sends each client a `Bye` and shuts down gracefully instead of dropping connections, and finishes the restart on its own thread; `remove_all_displays` no longer spawns a thread it immediately joins; `TauriDeviceReporter` skips its per-join and per-leave resync on any backend that does not cap concurrent displays. | Three small main-thread and per-event costs on the join, leave and settings paths, none of which did anything the caller needed. | 300 ms main-thread sleep per port change plus an abrupt transport close; one OS thread per join and per leave doing nothing on the `CGVirtualDisplay` tier | port change returns immediately and the client is told why; no thread per join/leave outside the AirPlay tier | COMPILE-CHECKED | yes | The `AppHandle` parameter Tauri injects is invisible to specta, so `bindings.ts` is unchanged. |

| 43 | input (macOS) | `CGEventCreateScrollWheelEvent` replaced by the non-variadic `CGEventCreateScrollWheelEvent2` (10.13+, below the 10.15 floor) (`streamer/input/macos.rs`) | The declaration put `wheel1` past the `...`, but Apple's header has it as a named parameter and only `wheel2`/`wheel3` are variadic. On Apple arm64 named arguments go in registers and variadic ones on the stack, so the callee read `wheel1` from `w3` while the caller wrote it to the stack. | arm64: the vertical scroll delta and the pinch-zoom line count were whatever junk sat in `w3`. x86_64 passes both through one register sequence, so the Intel dev box could not see it — and the shipped `universal-apple-darwin` build carried it in the arm64 slice | fixed arity, no variadic call left. `streamer::input::backend::tests::scroll_wheel_arguments_land_on_their_own_axes` builds an event with `wheel1 = 3`, `wheel2 = -1` and reads both back out of `kCGScrollWheelEventDeltaAxis1/2`; a second test binds the symbol to a spelled-out fixed-arity pointer type so the ellipsis cannot come back | VERIFIED (10.15.8, both tests pass); UNVERIFIED (arm64 — no Apple Silicon available) | yes | The behavioural test passes under the old declaration too *on Intel*, which is the point: it is there to fail on arm64. `CGEventCreateScrollWheelEvent2` was preferred over simply moving `wheel1` ahead of the ellipsis because it removes the class of bug rather than restating the prototype. Every other hand-declared `extern` in `macos.rs` was checked against the 10.15 SDK headers — `CGEventSourceStateID` is `int32_t`, `CFTimeInterval` is `double`, `CGScrollEventUnit`/`CGEventType`/`CGMouseButton`/`CGEventField`/`CGEventTapLocation` are `uint32_t`, `CGEventFlags` is `uint64_t`, `UniCharCount` is `unsigned long` — all eleven match. `windows.rs` declares nothing by hand; every symbol comes from the `windows` crate. |
| 44 | input | `mouse_relative` starts `false` and means only "the client reported pointer lock"; `protocol::positions_cursor(locked, phase)` becomes the single owner of whether a frame's coordinates count; Windows gains the `POINTERLOCK_STATE` arm and tracks its own position for relative motion (`streamer/input/{protocol,windows,macos}.rs`) | `POINTER_DOWN` carries coordinates the parser decoded and the injector discarded. Both backends began in relative mode and left it only on a move-ish phase, so a `Down` with no preceding move pressed the button at the old cursor position. | every touch client's first tap of a session landed at the host's pre-existing cursor — the normal case, since browsers fire `pointerdown` with no `pointermove`, and macOS has no touch injection at all so *every* tap goes down this path. Windows also ignored `POINTERLOCK_STATE` entirely, so leaving pointer lock never restored absolute positioning until an absolute move arrived | a lone `Down` positions then clicks; pointer-lock state has one owner and one source (the lifecycle message, with an absolute move as the fallback signal for clients that never send it). `down_and_up_position_the_cursor_unless_the_pointer_is_locked` asserts all eight phases in both states | VERIFIED (unit); UNVERIFIED (no touch client available on either dev machine) | yes | The finding proposed clearing `mouse_relative` on `Down`/`Up`. That is wrong under pointer lock: the browser freezes `clientX`/`clientY` at the lock point, so a locked `Down`'s coordinates are stale by construction and applying them would teleport the cursor. Gating on the lock state instead gets both cases right. Windows relative motion now injects absolutely from a tracked position rather than as `MOUSEEVENTF_MOVE`, which also stops the Windows pointer curve compounding with the client's; clamped to the virtual desktop, matching macOS exactly, because whether a locked client should be confined to its own display is a product question (`findings/minor-input-relative-mode-escapes-the-target-display.md`, rewritten to say only that). |
| 45 | input | `pointer_batch` injects the cursor run as one `SendInput` array on Windows and collapses it to the last sample carrying the whole run's delta on macOS; pen and touch stay per-sample and drop predicted samples (`streamer/input/{windows,macos}.rs`) | The client coalesces up to 40 pointer samples into one `POINTER_MOVE_BATCH`; the host parsed the batch and then unrolled it into one OS call each, undoing exactly the work the batching was for. | 40-sample mouse batch, 500 repetitions, Windows dev box: **7.09 / 5.93 / 4.57 ms per batch** across three runs, 40 `SendInput` calls each | same harness, same box: **5.67 / 3.72 / 2.89 ms per batch**, one `SendInput` — **-20% to -37%**, and 40 kernel transitions become 1 | VERIFIED (Windows, `cargo test --lib -- --ignored --nocapture pointer_batch_costs`) | yes | The finding proposed batching pen and touch the same way. That is not possible: `InjectSyntheticPointerInput` takes a frame of *simultaneous contacts* capped at the device's `maxCount` (1 pen, 10 touch), not a sequence of samples over time, so per-sample calls are what the API asks for. It also claimed `refresh_geometry` runs once per sample — it runs once per `dispatch`, i.e. once per batch already. The residual cost after the fix is win32k processing 40 events, not the transitions; that is inherent. macOS has no batch form of `CGEventPost` at all, so the cursor collapse (up to 40 Mach round trips to WindowServer saved) is the whole fix there. Precondition for W11's `getCoalescedEvents` extension, which is now landed on the client side. |
| 46 | input | `FAST_CAP` 512 -> 64 with oldest-first shedding; positional events older than 50 ms dropped at the consumer; `input_recv` / `input_queue` / `input_inject` histograms and `input_queue_drops` / `input_stale_drops` counters in `SessionTelemetry`; optional client timestamp on the pointer, batch, wheel, key and mouse-delta frames (`streamer/input/{mod,protocol}.rs`, `streamer/telemetry.rs`, `docs/INPUT-PROTOCOL.md`) | 512 entries of last-value-wins pointer data is seconds of history that then gets *replayed in order*, which for a cursor is strictly worse than dropping it. And the whole path had no instrument: `Stats::fast_received`/`fast_dropped` were written and never read, `fast_queue_depth()` and `build_stats` had no callers. | queue could hold ~2.5 s of stale moves at the client flush cadence and replay all of it; a saturated input path was invisible — no log line, no counter anyone read, nothing in `status --json` | queue bounded at 64 and by age; three stages and two counters in `status --json` and the `serve -v` summary; `input_recv` gives the excess one-way delay from the host's own minimum-delay estimate of the client clock offset, so no client report is needed | VERIFIED (unit: the offset estimator, the window roll and the supersedable-event rule); UNVERIFIED (no two-machine run to produce real percentiles) | yes | The offset estimator is deliberately *relative*: one direction cannot recover an absolute offset, so `input_recv` reads 0 on the best sample and reports lateness against it. Only positional events can be dropped for age — keys, buttons, clipboard and lifecycle are state transitions the host cannot recover from. `input::tune_process` was **deleted rather than wired up**: `platform::apply_process_tuning()`, which every `Streamer` entry point already calls through `prepare()`, does `timeBeginPeriod(1)` and `SetPriorityClass(HIGH_PRIORITY_CLASS)` plus the power-throttling exemptions and logs each result — the input copy was a strictly smaller duplicate with its return values discarded. The injector thread was verified to be a dedicated `THREAD_PRIORITY_TIME_CRITICAL` OS thread sharing no lock with the video path; its only inputs are the two crossbeam channels. |
| 47 | input (Windows) | `Drop for Injector` calling `DestroySyntheticPointerDevice` for the pen and touch handles (`streamer/input/windows.rs`) | `CreateSyntheticPointerDevice` ran twice per `Injector`, `Injector::new` runs once per injector thread and `input::spawn` runs once per WHEP negotiation, and the destructor appeared nowhere in the tree. | two kernel-backed HID injection handles leaked per client session, reconnect and page reload, for the life of the host process | released with the injector, mirroring the `CFRelease` the macOS backend already had | COMPILE-CHECKED (handle count not instrumented) | yes | Not measured with Process Explorer — the leak is provable by inspection and the fix is four lines. The observable symptom was Windows' undocumented per-process cap on synthetic pointer devices being reached, after which `CreateSyntheticPointerDevice` fails and pen and touch silently degrade to mouse emulation, which reads as flakiness rather than as a leak. Hoisting the devices to a process-wide `OnceLock` is the larger question and stays in `findings/speculative-input-per-session-injector-thread-and-devices.md`. |
| 48 | input / transport | `KEYFRAME_REQUEST` (`0x64`) parsed and routed to `pipeline.request_idr()`, rate-limited to one per 200 ms and counted as `client_keyframe_requests`; the `Pong` reply moved off the DataChannel receive path onto a task that owns the channel (`streamer/input/protocol.rs`, `streamer/webrtc_session.rs`) | The client's out-of-band keyframe request fell through `parse`'s `_ => return None`, so the tier W11 built was inert. Separately the `Pong` reply awaited an SCTP send inside `on_message`, which webrtc-rs awaits before dispatching the next message on that channel. | on Chrome and Edge 86-140 — where the legacy encoded transform runs instead of libwebrtc's reference finder, so nothing downstream ever emits a PLI — a single lost keyframe froze the picture until the client's watchdog abandoned the WebCodecs path. And every `PING` head-of-line blocked the reliable channel behind an SCTP write, inflating the very RTT it measures | the request takes the same path a PLI takes, within one round trip; the receive handler parses and enqueues only, and no longer clones three `Arc`s and boxes a future per message | COMPILE-CHECKED (parse and routing unit-tested; no client on either dev machine to originate one) | yes | Rate-limited host side because the client's 200 ms limiter is advisory: a looping or hostile client must not be able to pin the encoder at one IDR per frame. Counted separately from `keyframe_requests` (RTCP PLI/FIR) so the two sources stay distinguishable. Verification script: `docs/manual-tests/client-page.md` §2, row 3. |
| 49 | telemetry | `parse_client_stats` reads the v2 tail (tier bytes, the 22-bit degradation field, the swallowed-error count and three audio numbers); tiers and degradation named in the `serve -v` summary, `status --json` and `ScreenExtend status` (`streamer/input/protocol.rs`, `streamer/telemetry.rs`, `cli.rs`) | The client sends 96 bytes and the host read the first 72 and stopped. The floor rule meant nothing broke, but the half of the telemetry that answers "which path is this device actually on" arrived and was discarded. | a device on the `<video>` fallback, estimating its output latency, with no wake lock and no way to request a keyframe reported the same eighteen numbers as a device on every fast path | four tier bytes and 22 degradation bits decoded to names; `CLIENT_STATS_VERSION` 2, with the "version 0 rejected, anything else accepted" rule kept so a v3 client stays readable | VERIFIED (unit: a full v2 frame, a v1 frame, and a v2 frame one byte short reading as tail-absent rather than half-parsed) | yes | The tail is read only when all of it is present and defaults to 0 otherwise — which is also what "not measured" means for every field, so a v1 client and a v2 client that measured nothing are correctly indistinguishable, as they should be. |
| 50 | input / transport | A `u16` host-capability bitfield appended to the `PONG` payload at offset 17, assembled once as `HOST_CAPABILITIES` (`streamer/input/protocol.rs`, `docs/INPUT-PROTOCOL.md`) | Everything optional the host understands was invisible to the client, which had to assume it worked. A client that believes it can ask for an IDR keeps its `<video>` demotion watchdog disarmed, and is worse off than one that knows it cannot. | no host-side feature detection at all: `KEYFRAME_REQUEST`, the v2 stats tail, the input timestamp and the mouse-delta flags byte were each a guess on the client's part | five bits, four of them set, on a frame every client already receives once a second; bytes 0..17 of the `PONG` stay byte-identical, and a client seeing fewer than 19 bytes reads no bits | VERIFIED (unit: `the_pong_tail_announces_what_this_host_understands` asserts the frozen prefix and every bit) | yes | Bit 2 (a separate unordered channel for pointer moves) is deliberately clear and is expected to stay clear: the existing `fast` channel is already `ordered: false, maxRetransmits: 0`, so a second one has nothing to add. It is reserved rather than reused so the bit cannot come to mean something else. The bits are a floor, not a contract to stop sending — everything they gate is additive and safely ignored by a host that does not set them. |
| 51 | webrtc | The first IDR is requested on `RTCPeerConnectionState::Connected` instead of at answer creation (`webrtc_session.rs`) | `TrackLocalStaticRTP::write_rtp` returns `Ok(0)` when the track has no binding, so every frame written before the connection came up was discarded with no error. With an infinite GOP nothing else would ever produce a keyframe, so the client had to rescue itself with a PLI — one browser PLI interval plus a round trip. | The host log carried no sign of it at all; the discard was silent by construction. | The self-test now prints `RTP discarded: the video track has no binding yet` once per join — the discard, made visible — followed by `keyframe requested: connection is up and the track is bound`. 6/6 runs. | VERIFIED | yes | The PLI rescue itself is not measurable with the webrtc-rs self-test client, which does not decode and so never emits a PLI. The mechanism is confirmed; the milliseconds saved need a browser. |
| 52 | webrtc | One `KeyframeGate` per session: at most one IDR request per RTT-derived window (200-1000 ms), shared by RTCP PLI/FIR, the client's `0x64` opcode, a payloader failure and a lagging viewer (`streamer/keyframe.rs`) | Four independent paths each answered with the largest frame the encoder emits, on a link that had just demonstrated it could not carry the previous burst. A decoder that has lost its reference chain emits a PLI for every frame in its backlog. | not measurable without induced loss; the loop is read from the code | Unit-tested: 20 requests inside one window collapse to zero extra encoder work; the window tracks RTT and clamps at both ends. `keyframe_requests_suppressed` now reports the difference between what the client asked for and what the encoder was told. | VERIFIED (unit); UNVERIFIED (the storm itself — needs `clumsy` and a browser) | yes | A lagging viewer additionally cuts the rate, so the keyframe that does go out is smaller than the one that made it lag. |
| 53 | webrtc | Send pacer: a leaky bucket at libwebrtc's 2.5x pacing factor with Stadia's measured ~8-packet burst, capped at 75% of one frame interval, three modes chosen by locality (`streamer/pacer.rs`) | webrtc-rs has no pacer, so a whole access unit went to the socket in one tight loop — ~35 datagrams for a 40 KB IDR, landing in a single 802.11 TXOP. | `socket_write` p50 2.10 ms (median of five 20 s self-test runs, loopback) | `socket_write` p50 2.10 ms. On `SessionLocality::SameNetwork` the pacer is `Off` and the writer takes the pre-pacer path exactly, which is what this number confirms: **no regression on the path that does not need it**. | VERIFIED (no-regression on LAN); UNVERIFIED (the benefit — needs a busy AP) | yes | The benefit is unmeasurable on loopback by construction: there is no contention to smooth. `pace_wait` is a stage histogram so the cost is visible wherever it is paid, and `pacer_deadline_misses` counts every packet the frame-interval cap released early. |
| 54 | webrtc | Real bandwidth estimation: the writer stamps the transport-wide sequence number itself; the RTCP loop turns `TransportLayerCc` into exact per-report loss and per-packet delay gradients through libwebrtc's trendline estimator; REMB is consumed as a fallback (`streamer/congestion.rs`) | The host had no delay signal at all. It advertised `transport-cc` and `goog-remb`, installed the TWCC *receiver* (useless on a send-only peer), and drove the rate off receiver-report `fraction_lost`, which the browser refreshes about once a second. On Wi-Fi, link-layer ARQ hides loss until the queue is already deep. | no delay signal; loss sampled at ~1 Hz | `BWE: congestion signal is twcc-delay-gradient (twcc reports=N)` on every self-test join — the whole loop, end to end, on real RTCP. Unit tests pin that a filling queue is detected with **zero loss**, and that a clean path yields a flat gradient. | VERIFIED (the loop and the filter); UNVERIFIED (the step response — needs `clumsy`) | yes | `interceptor::twcc::sender` would stamp the extension but keeps its sequence counter private, and without the packet-to-sequence mapping the feedback cannot become a gradient — hence stamping it ourselves. Stamped only when the client offered the extension. |
| 55 | bitrate | `estimate_from_loss` made continuous from 1% upward, meeting GCC's `1 - 0.5*loss` exactly at 10%; the driver runs the estimator on its own value rather than on the emitted target | The old function returned its input unchanged for any loss from 2% to 10%. 5% packet loss is a visibly broken picture and the controller held the rate flat through it. | 1/3/5/8% loss: a literal no-op, all four | every sample in the old band cuts, monotonically; 5% loss compounds past the controller's 4% cut deadband within two 120 ms polls | VERIFIED (unit) | yes | The dead band is GCC's, where it is defensible because a delay-based estimator carries the session meanwhile. Here there was nothing else — which is what row 54 supplies. |
| 56 | webrtc | `SessionLocality` decided before the handshake from the peer IP, driving the RTP MTU (1188 / 1012), the ICE gather wait, the playout-delay hint and the pacing mode (`server.rs::locality_for`) | It was detected 500 ms *after* connection and dropped into a log line; `grep SessionLocality` found only the definition, the detector and that one call. Meanwhile every constant that should differ between a 1 ms hop and a relay was hardcoded. | locality logged, never used | classification unit-tested for same-subnet / router-hop / public / unparseable; the old detector still runs and now warns when the pre-classification was wrong | VERIFIED (classification); UNVERIFIED (the relayed constants — no TURN path on either dev machine) | yes | The relayed MTU follows Moonlight's documented 1024 cap and Sunshine's default. |
| 57 | session | No STUN server for a same-subnet peer, in the answer's `RTCConfiguration` and in the `/ice-config` the client gathers from | A reflexive candidate is exactly what a same-subnet pair has no use for, and offering one made both ends block on a round trip to the public internet. An offline or hosted-network host paid both timeouts in full for a candidate nobody would use. | host WHEP handshake **77-221 ms** (5 runs) | **12-20 ms** (12 runs across two builds) | VERIFIED | yes | TURN is still offered whenever configured, so a misclassified peer still connects. The client-side saving — its own gather deadline — is larger and needs a browser to measure. |
| 58 | vdisplay / session | The virtual display's refresh rate is no longer the stream's frame rate: the display is asked for 120 Hz, `cfg.max_fps` stays the encoder cap (`server.rs`, `DISPLAY_COMPOSE_HZ`) | An indirect display has no hardware vblank, so the mode's refresh rate *is* DWM's compose cadence and therefore the ceiling on how fresh a captured frame can be: 8.3 ms of average staleness at 60 Hz, 4.2 ms at 120 Hz. 120 Hz was already registered on every created display and simply never selected. | `capture_delivery` p99 median **13.63 ms**; `capture_to_socket` p99 median **13.63 ms** (5 runs) | mode accepted (`set to 1280x720@120`); `capture_delivery` p99 median **7.60 ms**, `capture_to_socket` p99 median **11.53 ms** (6 runs) | VERIFIED (the mode change); WEAK (the latency delta) | yes | The delta is right in direction and close to the predicted magnitude, but the self-test drives a **static** desktop where ~95 of ~175 frames are WGC repeats, so `capture_delivery` largely measures the repeat cadence and the variance is wide — a second 6-run set gave a p99 median of 10.99 ms. Re-measure with a moving workload before quoting a number. |
| 59 | session | The four unwrapped blocking OS calls on the join path moved to `spawn_blocking`; `wait_for_display_settle` seeded with the geometry we asked for | `start_on_monitor` — a DWM check, display enumeration, a D3D11 device, the scaler, an encoder session and the capture start — ran inline on the axum task, as did `monitor_device_names` up to 50 times in the attach poll. Each adapter's runtime workers also own every other session's RTP writer and bitrate poll. | the join stages were not instrumented at all | the join logs `display=`/`pipeline=`/`handshake=`; on this box 700-1300 / 540-1020 / 12-20 ms | VERIFIED (the wrapping and the instrumentation); UNVERIFIED (the cross-session stall it prevents — needs two clients) | yes | The settle seed removes a guaranteed 75 ms sleep and two `spawn_blocking` round trips from every join that touches a display. |
| 60 | webrtc | The peer connection is closed and the injector thread joined on teardown; the state-change handler and the audio data channel's `on_open` hold `Weak`, not `Arc` | Nothing closed it — `grep close()` over the transport found only the self-test — and it could not have been dropped anyway, because its own state-change handler, stored inside the peer connection, held an `Arc` back to it. Every *join* (not every device) leaked a peer connection, an ICE agent, its UDP sockets and one `injector` OS thread. | unbounded growth; no counter existed to see it | `live_transport_sessions` in the telemetry snapshot, and a `session for <ip> closed; live transport sessions: N` line per teardown | VERIFIED (the close path runs and the counter exists); UNVERIFIED (a long soak) | yes | The client rebuilds the whole session on any settings change, so a user nudging a slider a dozen times leaked a dozen of each. |
| 61 | encode / session | `cfg.intra_refresh = true` in the shipped path | Rolling intra refresh has been plumbed through NVENC, Quick Sync and VideoToolbox since those backends were written and **no shipping path ever set it**, so every recoverable loss cost a full IDR. With a refresh wave the decoder rebuilds over a few frames, and a client that missed the session's opening keyframe recovers on its own instead of sitting black behind an infinite GOP. | off, always | on; no measurable cost on this path — `encode` p50 unchanged (0.00-1.31 ms) and `socket_write` p50 unchanged (2.10 ms) over 6 runs | VERIFIED (no regression); UNVERIFIED (the recovery benefit — needs induced loss) | yes | x264 ignores the flag, so the software fallback is unaffected. |
| 62 | webrtc | `enable_sender_rtx(true)` evaluated and **rejected** | The RTX codecs are advertised in the answer but sender RTX is never enabled, which reads like an oversight. | — | From `interceptor-0.17.1/src/nack/responder/mod.rs`: `bind_local_stream` registers by `info.ssrc` and only when `stream_support_nack(info)`, and the RTX stream is registered with `rtcp_feedback: vec![]`, so the responder never binds it. `resend_packets` looks the stream up by `nack.media_ssrc` and writes to that stream's own `next_rtp_writer` — the **primary** SSRC. Enabling sender RTX in webrtc-rs 0.17.1 therefore creates an RTX SSRC that nothing ever writes to, while adding an `a=ssrc-group:FID` telling Chrome to expect retransmissions on a stream that will always be silent. | VERIFIED (by source reading) | no — not enabled | Repairs already reach the client on the primary SSRC and are accepted as late arrivals. The RTX codec registration is left in place: without the FID group Chrome does not enable RTX reception, so it is inert rather than wrong, and the self-test asserts it. |
| 63 | webrtc | Per-packet cost of the transport work reduced: transport-cc stamped only when the client offered the extension, pacer clock read skipped entirely in `PacingMode::Off` | The first cut stamped the extension and wrote the send history unconditionally — a marshal, a `BytesMut` and a mutex acquisition per packet, for a peer that had not negotiated it. | `socket_write` p50 **3.00 ms** (median of 6 runs; regressed from the 2.10 ms baseline) | `socket_write` p50 **2.10 ms**, back to baseline | VERIFIED | yes | These are debug builds, so the absolute cost of added instructions is inflated; the direction is what the change is for. |
| 64 | webrtc | An access unit carrying an SPS whose payload list carries no parameter sets logs once, loudly | `H264Payloader` drops the SPS+PPS STAP-A silently when it exceeds the MTU **and clears the stashed copies anyway**, so they are never retried. With an infinite GOP that is a permanent black screen with traffic flowing, and every host-side signal reporting success. | silent; unobservable from the host | a `teprintln!` naming the MTU, unit-tested against the FU-A-only payload list the failure actually produces | VERIFIED (unit) | yes | Detection only — the packetizer cannot be fixed from the caller. It matters more since the relayed MTU dropped to 1012, which shrank the SPS+PPS+5 budget with it. |
| 65 | cloud | `CloudClient::spawn`'s `.ok()` replaced with a reported error; the `signal_response` send checked; the no-op `if let Some(()) = …{}` reduced | A failed thread spawn left `handle` as `None`, so `stop()` silently did nothing and the desktop UI sat on the startup "connecting" state forever with no line explaining it. A dropped `signal_response` is a join that hangs for the client and looks like nothing at all from the host. | three errors discarded | `CloudState::Error` with the reason; a named log line when the relay writer is gone | COMPILE-CHECKED | yes | The `SESSION_TAKEN` substring match on a formatted error string is untouched — see `findings/minor-cloud-session-taken-is-a-substring-match.md`. |

| 66 | input (macOS) | The Ctrl<->Command remap becomes conditional on the client's own platform: `ClientPlatform::from_reported` parses the `os` string the join request already carries, `ClientProfile` threads it to the injector, and `modifier_keycode` selects a pass-through or a swapped modifier row (`streamer/input/{mod,macos,windows,generic,linux}.rs`, `streamer/{webrtc_session,server}.rs`) | `code_to_keycode` rewrote the modifier row unconditionally — Ctrl to Command, Meta to Control — with no reference to who was typing. That is right for a PC keyboard, where Ctrl is the command modifier, and exactly backwards for an Apple one. | a Mac or iPad client pressing Command+C sent `MetaLeft`, the host injected `VK_CONTROL`, and macOS received Control+C, which is not copy; their Ctrl+A (start-of-line in Cocoa) arrived as Command+A, select-all | Command+C copies from every client and Ctrl+A moves the caret from an Apple one, with Windows and Android clients unchanged | VERIFIED (unit: both tables asserted key by key, plus that every non-modifier key is identical in both); UNVERIFIED (no Mac or iPad client available to type on) | yes | The information was already on the wire and validated — `os` is in the join request (`server.rs`, `MAX_OS_LEN`) and `detectOs()` in the client page produces it — it had simply never been threaded past the session layer. `ClientPlatform` keeps all five variants rather than collapsing to a boolean, because guessing from a boolean is how the original bug happened; an unreported platform logs once and assumes a PC layout, which is the pre-existing behaviour and the commoner client. `recompute_mods` derives its `CGEventFlags` from the *remapped* keycodes, so it is self-consistent with whichever table is chosen and needed no change. A per-device override for a non-standard layout is the natural extension and is not implemented. |
| 67 | audio-win | `capture_ns` anchored to `min(qpcpos, now - read_duration)` instead of `qpcpos` alone | `qpcpos` is documented as when "the audio endpoint device read the device position", but on a **loopback** stream the endpoint is a *render* device and that position is its play cursor — so the stamp is a playout time, in the future. The client sizes its jitter buffer from `enqueue - capture_ns`, so a future capture time is surplus audio delay. | `capture -> subscriber` recorded **0 samples** (every span negative); `capture_ns` skew min -20.36 / mean -14.65 / max +5.61 ms, **1596/1601 packets stamped ahead of the host clock**, spread 26 ms, offset moving with host load | `capture -> subscriber` p50 9.44 / p90 9.44 / p99 9.44-10.49 / max 10.9 ms; skew min -3.04 / mean +7.69 / max +10.9 ms, <=1/1601 ahead | VERIFIED (Win 11 26200, `probe --audio`, 3 runs, idle) | yes | One engine period plus the intra-burst offset, which is what it should physically be. `ts_clamped` counts how often the endpoint stamp was in the future: on this box, every drain. |
| 68 | audio-win | Silent render companion negotiates the engine period via `IAudioClient3::GetSharedModeEnginePeriod` + `InitializeSharedAudioStream(min)` | Microsoft documents that any stream on an endpoint can lower the whole endpoint's period, and the loopback capture stream cannot ask for itself (`AUDCLNT_E_INVALID_STREAM_FLAG`). The companion can, and capture inherits it. Expected -5 to -7.3 ms where a driver exposes a small buffer. | wake interval p50/p90/p99 9.44 ms, max 10.3-10.6 ms; engine periods `default = fundamental = min = max = 480 frames (10.000 ms)` | identical — the probe correctly declines, because there is no smaller period to request on this driver | VERIFIED **no-op on this hardware**; the request path is COMPILE-CHECKED | yes | Not a failure: this OEM driver exposes exactly one period. The in-box High Definition Audio Device driver documents 128-480 frames and is how the other branch gets exercised; that needs a driver swap plus a reboot, so it is a manual test. Landing it also fixed "nothing on any machine could say what period it was running at". |
| 69 | audio-win | Process-loopback tier added (`ActivateAudioInterfaceAsync` + `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, excluding our own pid), probed by attempting activation | A second tier for the cases endpoint loopback structurally cannot serve: an exclusive-mode endpoint, device churn, an endpoint that will not activate. Ordering to be decided by measurement, not assumption. | endpoint tier: wake 9.44/9.44/9.44/23.11 ms, capture->packet 5.77/9.44/9.44/24.07 ms | process tier: wake 9.44/9.44/10.49/11.32 ms, capture->packet 6.29/9.44/10.49/11.41 ms | VERIFIED (both tiers, 8 s each, idle) | yes, as **tier 2** | Neither is meaningfully faster — same shared engine. The endpoint tier keeps p99 by one bucket, so the long-shipping path stays first. Settled two open questions from the research: process loopback needs **no** silent companion (999 packets in 5 s with none anywhere), and its event does fire. |
| 70 | audio (all) | `audio-bridge` copy thread deleted; capture backends write straight into the broadcast through `AudioSink` | `broadcast::Sender::send` is synchronous and non-blocking, so the bridge was a pure type adapter — running at default priority between a Pro Audio thread and the network. | capture -> subscriber p50 9.44 ms (idle) | capture -> subscriber p50 9.44 ms (idle) | VERIFIED (no p50 change); UNVERIFIED under load | yes | Honest result: on an idle host the hop cost nothing at p50. It was a tail risk under load. Removing it also removed the unbounded capture->hub channel and the `bridge.join()` that could wedge the hub mutex forever. |
| 71 | audio (all) | Broadcast fan-out capacity 128 -> 16 packets | 128 packets is 640 ms of audio held for a subscriber that stopped reading; a real-time consumer that far behind should skip to live rather than catch up. | 640 ms of buffered staleness before `Lagged` | 80 ms; `lagged=0` over 10 s with subscriber inter-arrival p99 9.44 ms | VERIFIED | yes | Sized against the measured burst (two 5 ms packets per 10 ms wake) rather than guessed. |
| 72 | audio (all) | Percentiles from `telemetry::Histogram` instead of cloning and sorting a 4096-element `Vec` every 2 s on the capture thread; the 2 s log emit moved to its own thread | A 32 KiB allocation, a 4096-element sort and a logbus mutex acquisition on an MMCSS "Pro Audio" thread is a periodic priority inversion against the exact guarantee MMCSS was registered to get. | encode p50 0.26 / p99 0.33 ms, plus a 2 s periodic allocate+sort+lock on the RT thread | encode p50 0.05-0.09 / p99 0.20-0.33 ms, no allocation and no lock on the RT thread | VERIFIED (same percentiles, allocation removed) | yes | Applies to both platforms; the macOS worker had the identical helper. The stall itself was never measured directly — that needs an ETW trace — so the claim is "the allocation and the lock are gone", not "N microseconds saved". |
| 73 | audio (all) | Opus `OPUS_SET_VBR_CONSTRAINT` 0 -> 1; `OPUS_SET_DTX` / `OPUS_SET_INBAND_FEC` removed; encoder state read back and logged | libopus defaults `VBR_CONSTRAINT` to 1 and the code opted out with no recorded reason. Unconstrained VBR spikes packet size on transients; at 200 packets/s that is arrival jitter, which the client answers by growing its buffer. DTX and in-band FEC are LPC-layer (SILK) features and structurally inert under `RESTRICTED_LOWDELAY` at 5 ms. | settings applied and never confirmed; `vbr_constraint=0` | `application=2051 bitrate=128000 complexity=5 signal=3002 vbr=1 vbr_constraint=1 lsb_depth=24 prediction_disabled=0 lookahead=120 samples (2.50 ms)` | VERIFIED in force (read back); the packet-size distribution is UNVERIFIED — an idle host encodes 3-byte silence frames, so the peakiness claim needs real audio | yes | `OPUS_SET_PREDICTION_DISABLED` is wired as a config field defaulting **off**: right for a zero-retransmit transport, but it costs bitrate at fixed quality and deserves a listening test rather than a blind landing. |
| 74 | audio (all) | DataChannel backpressure checked every packet against 40 ms of audio, instead of one packet in four against a flat 16 KiB | 16 KiB at the measured ~93-byte message is about a second of queued audio before dropping starts, and sampling detected both entry *and exit* up to 20 ms late. | ~1 s of queued audio; 20 ms detection lag in both directions | 40 ms, evaluated per packet, threshold derived from the message size actually being sent | COMPILE-CHECKED — needs an induced-congestion run to observe | yes | Drops now also set `FLAG_DISCONTINUITY` on the next packet sent, so the client resyncs instead of splicing. |
| 75 | audio (all) | Opus-track fallback samples spaced by real elapsed `capture_ns` instead of a hardcoded 5 ms | `TrackLocalStaticSample` derives RTP timestamps by accumulating `duration`, so every producer-side gap left audio permanently 5 ms per lost packet behind video, which rides the true host clock. | offset grows 5 ms per lost packet and never recovers | audio and video RTP timelines both follow `host_now_ns` | COMPILE-CHECKED — needs a browser forced onto the NetEQ path | yes | This is the path old mobile browsers land on, i.e. the compatibility target of this engagement. |
| 76 | logbus (all) | `tprintln!` moved off the calling thread: a bounded-channel `try_send` that moves the already-formatted string, a single writer thread owning the 2000-line ring, batches of up to 64 lines or 50 ms per `LogLine`, and no event emitted at all until something has read the backlog | Every one of the 440 call sites took a process-global mutex, allocated the line a second time for the ring, and — whenever `attach()` had run, i.e. always in the GUI — emitted a Tauri event. `EmitArgs::new` serializes the payload to JSON *before* the listener check (`tauri-2.11.5/src/manager/mod.rs:534-552`), so with the Settings log panel closed every line paid a JSON serialization and two more mutexes for a message nobody received. The ring mutex was shared by the capture thread, the encode thread, the MMCSS audio thread and every tokio worker. | `logbus::tests::bench_push_line`, release, panel closed: **132 ns/line** at 1 thread, **351 ns/line** at 4 threads. Excludes the emit, which the "before" arm cannot reach from a test — so both numbers understate the real cost. | **181 ns/line** at 1 thread (0.73x), **126 ns/line** at 4 threads (**2.80x**, 0 dropped) | VERIFIED (microbenchmark); the removed JSON serialization is reasoned from the vendored crate source, not timed | yes | The single-thread regression is real and deliberate: waking a sleeping consumer costs more than an uncontended mutex push. It does not matter — one thread logging alone is the sparse case. The contended shape is the one on the latency path. Overflow drops the line and says so in-band on the next batch rather than blocking a real-time thread. Verified by grep that no `tprintln!` sits on a per-frame success path: the densest file, `windows_utils/streamer/pipeline.rs` (50 sites), has them on error paths and one 60-frame-throttled probe line. **Not fully closed:** the `STREAMING` flag arms on the first `get_log_backlog` and never disarms, because Tauri 2 exposes no way to ask whether a JS listener is still attached — so once the Settings log panel has been opened, the emits continue at up to 20 batches/s for the rest of the process instead of stopping when it closes. Bounded and much smaller than one emit per line, but real: `findings/minor-cli-logbus-streaming-flag-latches-on-and-never-turns-off.md`. |
| 77 | streamer (all) | One process-wide tokio runtime (`streamer::transport_runtime()`, `OnceLock`, `worker_threads` clamped to 4) instead of one full runtime per network adapter | `Streamer::run_with_handle` built a complete multi-thread runtime every call and `sync_streamers` calls it once per adapter IP. `worker_threads` was never set, so each runtime took `available_parallelism()`: three adapters on a 16-thread CPU meant 48 priority-boosted workers, nearly all idle, competing with the encoder. A Wi-Fi roam that changes the IP tore down and rebuilt a whole runtime. | **50 threads** (release `serve -v --no-cloud --no-qr`, one adapter, `Get-Process ScreenExtend`, 2 trials both 50); startup to `server listening` median **391 ms** (5 trials: 325/388/391/403/416) | **38 threads** (same conditions, 2 trials both 38); startup to `server listening` median **460 ms** (9 warm trials: 375/416/455/457/460/475/504/504/521) | VERIFIED (thread count, one adapter, both directions). The 3-adapter case is UNVERIFIED: this box has one adapter, so the 48 -> 4 figure is arithmetic (`available_parallelism() * N`), not measured. **Startup: no improvement, and the medians are 69 ms apart in the wrong direction.** Both runs shared the box with 4-11 concurrent `rustc` processes and the spreads overlap (before 325-416, after 375-521), so this is noise, not a regression — but it is recorded rather than dropped, because building 4 worker threads instead of 16 cannot plausibly make startup slower and a clean-box re-measurement would settle it. | yes | The adapter still gets its own OS thread and `block_on`s its own `server::run` there, so one accept loop still cannot starve another; only the worker pool is shared. `on_thread_start(tune_transport_thread)` is unchanged. Threads are named `se-adapter-<ip>` / `se-transport` so a dump is legible. The change is in `streamer/mod.rs` and therefore applies to macOS with no edit; only the Windows call site got the naming and spawn-failure hardening — see `findings/major-session-tokio-runtime-per-adapter.md`. |
| 78 | build (Windows) | `-Clink-arg=/MANIFESTDEPENDENCY:…Common-Controls…version='6.0.0.0'…` for every Windows target, in a new `src-tauri/.cargo/config.toml` | `tauri_build` writes the common-controls v6 dependency into the *app binary's* manifest, but nothing gives one to a `cargo test` harness. The moment dead-code elimination keeps the dialog path alive, the test binary imports `comctl32!TaskDialogIndirect`, the loader binds comctl32 to the v5 copy in System32, and the process dies at load with `STATUS_ENTRYPOINT_NOT_FOUND` (0xc0000139) before a single test runs. | reproduced on this box: an unrelated edit to `logbus.rs` flipped `cargo test --lib` from 170 passing to an unloadable binary — no output, no diagnostic, no stack | `cargo test` 171 passed / 0 failed; `cargo build --release` links clean | VERIFIED | yes | This is the "latent Windows linker trap" the resume brief asked about. It is not caused by any change in this branch; it is a pre-existing hazard that surfaces whenever DCE happens to retain the dialog path, which is why it appeared to be triggered by edits nowhere near it. Pinning it in `.cargo/config.toml` removes the coin flip. |
| 79 | build (macOS) | `MACOSX_DEPLOYMENT_TARGET = "10.15"` in `src-tauri/.cargo/config.toml`, repeated in the `tauri-action` env in `build-release.yml` | rustc's default for `x86_64-apple-darwin` is 10.12 and nothing in the repo set it, so the shipped binary's load command contradicted `minimumSystemVersion: "10.15"` — it claimed to run on three releases it cannot — and clang's availability/weak-import machinery was calibrated three releases too low. | measured over SSH on the dev Mac 2026-09-08, debug binary at `80e28c7a`: `LC_VERSION_MIN_MACOSX version 10.12, sdk 11.1`, x86_64 only | pinned; the artifact check is `docs/manual-tests/macos-build.md` and the `dyld-floor` CI job, which reads `minimumSystemVersion` out of `tauri.macos.conf.json` and asserts the load command against it | UNVERIFIED on an artifact — the Mac is at its build-capacity limit and no release build with the pin exists yet. `findings/minor-build-verify-lc-build-version-minos-after-merge.md` is the post-merge step. | yes | The SDK is deliberately **not** pinned: `SDKROOT` is an absolute path that is wrong on every other machine, and on CI no 10.15 SDK exists at all. Gating the artifact is portable where pinning the SDK is not. |
| 80 | build (all) | `.gitattributes` marks the vendored blobs `binary` and both `SHA256SUMS` `text eol=lf`; the two darwin `nefconc` hashes re-pinned from the blob content | The two darwin stubs contain no NUL byte, so git's auto text detection rewrote them to CRLF on a Windows checkout — 19 bytes instead of the 17 in the blob, and a different SHA-256. `binaries/SHA256SUMS` pinned that CRLF hash. Both checksum files were themselves CRLF, and `sha256sum -c` reads a trailing CR as part of the filename. | `resources`: 7 x `FAILED open or read` on Windows. `binaries`: passed on Windows, would have failed on the ubuntu CI job (`306c6ca7…` vs pinned `be43f00b…`) | `sha256sum -c` passes in both directories on this checkout; the `blob-checksums` CI job is the standing guard | VERIFIED | yes | This is plausibly part of *why* CI was disabled four minutes after it was added — the `blob-checksums` job would have failed on day one. |
| 81 | cli / security | `single_instance`: constant-time control-token comparison, `screenextend.ctrl` created 0o600 and re-chmodded if wider, per-connection read timeout 2 s -> 500 ms, bad tokens logged | The token was compared with `!=` on `&str` while the join OTP two modules over already used `subtle::ConstantTimeEq`. The ctrl file took whatever the umask gave it, and whoever can read it can `QUIT` the host. Connections are served one at a time, so anything that connected and said nothing stalled a real `stop` for 2 s. | `!=`, umask-dependent mode, 2 s stall, silent drop | constant-time, 0o600, 500 ms, one log line per bad token | COMPILE-CHECKED (the loopback path is not exercised by a test) | yes | The two remaining items in that finding change `acquire_lock`'s signature and every caller, and every caller is in `cli.rs`/`lib.rs` — retagged W13b, with the suggested `LockOutcome` enum written out. |
| 82 | build / CI | `.github/workflows/ci.yml` (six jobs), a dyld-floor gate on the release job, `scripts/perf-gate.mjs` + `ci/perf-baseline.json` | CI was added and deleted 4m13s apart in 2026-08 and has never run, so nothing gated `cargo fmt`/`clippy`/`test`/`pnpm lint`, nothing verified the blob checksums the two PROVENANCE files claim are "verified in CI", and nothing checked the shipped macOS binary against the 10.15 dyld floor — a load failure that is invisible on every machine that could debug it. | no gate of any kind since 2026-08-22 | frontend (eslint, build, Safari 13.1 re-derivation, client-stats harness), rust matrix (windows + macos-13/14/15), macos-smoke (doctor's probed tiers + the `#[ignore]`d VideoToolbox encode tests), dyld-floor, blob-checksums, perf-gate | UNVERIFIED — the workflow cannot run until the user pushes; validated by parsing the YAML and checking every job/step key, not by an actual run | yes | The macOS matrix is the point: the dev Mac is 10.15, so `macos-13/14/15` are the only machines on the project where ScreenCaptureKit, the Core Audio process tap and VideoToolbox execute at all. `macos-smoke` asserts the audio probe picks `screencapturekit` on 13 and `process_tap` on 14/15 — a silent demotion is exactly the regression nobody here can see. || 95 | capture-win (wgc) | `MinUpdateInterval`, `DirtyRegionMode` and `IsBorderRequired` are each probed with `ApiInformation::IsPropertyPresent` and the setter skipped when absent, instead of `GraphicsCaptureApi::new` failing on the first missing one. The feature set is logged and carried in telemetry as `capture_features`. | The three are **build 26100** (Win 11 24H2) / **20348** properties. Requesting any absent one made the whole WGC session fail, so **every Windows 10 host and every Windows 11 before 24H2** fell silently through to DXGI Desktop Duplication — losing the NVENC zero-copy ring and serializing capture against encode. Sunshine's shape is per-property probing. | every pre-24H2 host: DXGI, no ring | those hosts stay on WGC; the dev box (26200) reports `+cursor +border +secondary-windows +min-update-interval +dirty-region +free-threaded-pool (build 26100+)` and is unchanged | VERIFIED on 26200 (no regression); the pre-26100 half is COMPILE-CHECKED | yes | The change here with the largest population behind it and the least ability to measure it locally. Script: `docs/manual-tests/windows-capture.md` §1-2 |
| 96 | capture-win (wgc) | Frame-pool depth is per-path — 2 for the handler that encodes inline, 3 for the ring that only stages — and `TryGetNextFrame` drains to the **newest** frame, counting what it discards into `frames_dropped_stale`. | `Recreate` passed buffer count 1 while `Create` passed 3, so a content-size change silently undid the capture/encode decoupling for the rest of the session. The pool is a queue handed out oldest-first, so depth is latency for any consumer that falls behind: at 3 buffers and 60 Hz an unlucky frame was 33 ms old before the encoder saw it. OBS and Sunshine both use 2 for an inline consumer. | oldest-first, up to 33 ms of pool age | newest-first; `frames_dropped_stale` counts the difference | VERIFIED (counter moves under load on 26200) | yes | The counter is the point: the drain is only correct if what it drops is visible. |
| 97 | capture-win (wgc) | A keyframe request now always produces an IDR within one frame period, including on a completely static desktop: the ring's keepalive reads `idr_request` and calls `encode_repeat_slot` with it, and the encode thread ticks at the frame period rather than at the 200 ms keepalive interval. | NVENC is configured with an infinite GOP and IDR period, so there is no periodic keyframe at all. `idr_request` was read only in `AsyncNvencRing::stage`, which runs only when WGC delivers a *new* frame; the 200 ms keepalive read `Encoder::idr_requested`, whose only writer had no callers. A client that joined, or lost its reference chain, while nothing moved on the extended display waited for the user to move something. | **2 IDRs in a 30 s static selftest**, both at session start | an IDR within one frame period of any request | COMPILE-CHECKED as to the *request* path — no probe asks for a mid-session keyframe on a static desktop, so nothing here exercises it end to end. The selftest confirms no regression: still exactly the 2 session-start IDRs, and `idrs_sent` is now written by both the stage and the keepalive. | yes | Corroborated by baseline note 5 in `docs/BASELINE.md`, which recorded the 2-IDR symptom before the cause was known. Verifying it needs a browser client that drops a frame mid-session, or a probe flag that injects a keyframe request at t+15 s — worth adding. |
| 98 | capture-win (wgc) | The WGC D3D device is created on the encoder's adapter: `IDXGIFactory6::EnumAdapterByGpuPreference` (probed by `QueryInterface`, Win 10 1803+) plus `VendorId`, with `GpuPreference::Vendor`/`HighPerformance`/`MinimumPower` chosen from the configured encoder. The registry write that only applied to the *next* launch was dropped. Which adapter captures, which encodes, and whether they match is logged once per session. | The encoder input is a `SHARED_NTHANDLE \| SHARED_KEYEDMUTEX` texture opened on the capture device, and that mechanism is same-adapter only; when the two adapters differ every frame takes a GPU→CPU→GPU round trip. WGC can capture any monitor from a device on any adapter, so the choice is free. x264 gets `MinimumPower` for the opposite reason: reading back from the iGPU that composes the desktop beats dragging the frame across PCIe. | inferable only from a "zero-copy path unavailable" line that named no adapter | `adapters: capture=… encode=… same_adapter=true\|false` once per session | VERIFIED (`same_adapter=true` on the dev box for NVENC and for x264) | yes | Also shouts when the capture adapter is a software rasteriser, which is what a WARP-composed indirect display looks like. |
| 99 | tuning-win | Capture and encode threads are confined to the highest-`EfficiencyClass` CPU sets (`GetSystemCpuSetInformation` + `SetThreadSelectedCpuSetMasks`), probed by "does this machine report more than one distinct efficiency class". | Thread priority orders threads *within* a core's queue and says nothing about which core the scheduler picks, so an MMCSS-critical capture thread could still be parked on an efficiency core. The cost shows up as tail latency, not as a lower average. `SetThreadSelectedCpuSetMasks` is a soft restriction, unlike `SetThreadAffinityMask`, so the scheduler may still migrate rather than deadlock. | no core-class preference | `[cpu] hybrid CPU detected: confining capture/encode threads to the 12 logical processors of efficiency class 1` | VERIFIED (the dev box is a hybrid Intel part and takes the branch) | yes | Probed by capability, never by CPU model; a homogeneous CPU takes no action at all. |
| 100 | capture-win (fork) | ~2,800 lines removed from the vendored `windows_capture` fork — the unused DXGI duplication API, `window.rs`, `graphics_capture_picker.rs` and the Media Foundation `VideoEncoder` — with the `Media_*`/`Storage*` `windows` features and the `clap` dependency they forced. The six genuine upstream bug fixes the fork carries are recorded in `windows_capture/FORK.md`. | The fork's provenance was undocumented, so every one of those fixes was one careless dependency refresh away from being lost, and the dead half made the diff against upstream unreadable. | 2,800 lines + 3 crate features + 1 dependency | gone; `FORK.md` names each surviving divergence | VERIFIED (builds, tests pass) | yes | Compile time and binary size, not frame latency. |
| 101 | capture-win (dxgi) | The cursor is composited incrementally: only the rectangle it previously occupied is restored and only the rectangle it now occupies is drawn, instead of recopying the whole desktop texture whenever the pointer moved. | Moving the mouse over a static desktop cost a full-resolution `CopyResource` per pointer update — 8.29 MB at 1080p — for a change of at most 32×32 pixels. | full-frame copy per pointer update | two small blits | VERIFIED (dev box; cursor correctness by eye per `docs/manual-tests/windows-capture.md` §6) | yes | |
| 102 | capture-win (wgc) | `DirtyRegionSettings::ReportOnly` (probed) plus a skip when `Frame::dirty_regions()` comes back empty, capped at 5 consecutive skips. | The DXGI path had skipped unchanged frames since `a56eedc`; WGC — the *fast* path — re-encoded every delivered frame, burning encoder time, GPU copy bandwidth and bitrate on frames byte-identical to the previous one. For a second monitor showing a document or a terminal that is most of the frames. The repeater already emits a keepalive every 200 ms, so a skipped frame is not a stalled client. | every delivered frame encoded | unchanged frames skipped; the 5-skip valve covers a driver that under-reports | VERIFIED on 26200 (the property exists there); no effect pre-24H2, where the property is absent | yes | The valve matters: a driver that reported no dirty regions when there were some would otherwise serve a stale picture forever. |
| 103 | capture-win (dxgi) | `AcquireNextFrame`/`ReleaseFrame` paired by an RAII guard on every exit path, and `DXGI_ERROR_INVALID_CALL` recovered by re-duplicating (capped at 20) instead of killing the capture thread. | The two calls are a strict pair: miss one release and every subsequent acquire returns `DXGI_ERROR_INVALID_CALL` forever. `poll` had a `?` between them, so any early return leaked the frame and wedged the thread. Against a freshly created virtual display it reproduced every time — five `ACCESS_LOST` re-duplications while the IDD settled, then a permanent invalid call — and the client saw a negotiated session that stayed black. | `probe --whep-selftest --capture-backend dxgi` died at the first invalid acquire, 0 RTP packets | the capture survives the settling period | VERIFIED (the invalid-call death no longer reproduces) | yes | Half of the "AcquireNextFrame invalid call on a fresh virtual display" blocker; the other half is row 105. |
| 104 | capture-win (dxgi) | `IDXGIOutput5::DuplicateOutput1` with an explicit format list is tried before `IDXGIOutput1::DuplicateOutput`, probed by `QueryInterface`; and **every** adapter that enumerates the display is tried, not just the first. The compatibility report's `dxgi` probe creates a duplication and drops it instead of asking whether a DXGI factory exists. | On a hybrid-graphics laptop the same display can be listed by several adapters and only the one that composes it can duplicate it; `DuplicateOutput1` is also the only entry point that can duplicate a display in HDR mode. And `CreateDXGIFactory1().is_ok()` is true on every Windows since 7, so the report was advertising a fallback that could not start. | `DuplicateOutput failed after 5 attempts: 0x887A0004` — one bare HRESULT — and a compatibility report claiming DXGI was available | every adapter tried and named; the report prints `dxgi: duplication fallback is NOT available on this machine (…)`, and a working host logs which entry point is live | VERIFIED — and the answer on this box is **negative**: `\\.\DISPLAY1` is enumerated only by the RTX 3080 Ti and both entry points return `DXGI_ERROR_UNSUPPORTED`. This machine is WGC-only, which it now states rather than implies. | yes | See the note below on what is left of the DXGI fallback. |
| 105 | capture-win (dxgi) | Duplication that is re-established without ever producing a frame fails the capture after 40 attempts, and the "re-established" line is rate-limited to the first and every tenth. | `redup_failures` counted only *failed* re-duplications and a success reset it, so an output returning `DXGI_ERROR_ACCESS_LOST` on every acquire looped forever: acquire, lose access, re-duplicate, repeat. Against a fresh IDD virtual display it did exactly that for a whole 12 s probe run, delivered no frames, and emitted a log line — hence a UI IPC event — every time round. | unbounded silent spin, ~3 log lines/s, 0 frames | bounded and named: the capture stops with `duplication was re-established 41 times without ever delivering a frame` | VERIFIED (reproduced the spin, then the bound) | yes | "Degrade loudly" is the requirement; a negotiated session showing black is the failure this removes. |
| 106 | capture-win (dxgi) | The DXGI capture thread stages the composed frame into one of three slot textures on the duplication device and hands the index to a `dxgi-encode` thread, instead of encoding inline. Overrun drops are counted in `frames_dropped_ring`; the per-60-frame latency line now prints the frame budget beside the measured average. | Capture of frame N+1 could not begin until the encode of frame N returned, so the achievable rate was `1 / (capture + composite + encode)` rather than `1 / capture` — and on every CPU-bridge backend the encode is the large term. This is the path every Windows 10 and pre-24H2 Windows 11 host falls back to, so it is the common case rather than the rare one. | capture and encode serialized on one thread | decoupled; the copy is GPU-to-GPU on a device that is already multithread-protected | COMPILE-CHECKED — this box cannot duplicate its own primary display (row 104) and its IDD displays do not settle into a duplication, so the throughput claim has no local number | yes | One caveat, stated because it bounds the win: the encode thread shares the duplication device with the capture thread, and D3D11 multithread protection holds the device lock for the whole of a blocking `Map`. So on a CPU-bridge backend the *readback* (1192 us BGRA / 740 us NV12 at 720p, row 108) still stalls the capture thread, while the encode itself — 2-8 ms for x264 — no longer does. The majority of the serialization is gone, not all of it. Measured verification needs a machine where DXGI duplication works: `docs/manual-tests/windows-capture.md` §5b. |
| 107 | capture-win (dxgi) | No video processor on the adapter composing the display means the encoder is configured at native resolution and the client does the scale, with a log line, instead of the session failing. | `Scaler::new` casts the duplication device to `ID3D11VideoDevice` and its context to `ID3D11VideoContext`; both fail on a device created without `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` — which `create_device_on` silently retries without — or on an adapter with no video engine, and an indirect-display adapter is not obliged to have one. The failure was `?`-propagated, so on a host that had already fallen back from WGC, a client with video-scale under 100% could not start a session at all. | error "building GPU downscaler for --scale", no session | native-resolution stream plus one explanatory line | VERIFIED that this box's duplication devices *do* expose a video processor, so the fallback branch is COMPILE-CHECKED | yes | A worse picture is not a dead session. |
| 108 | scaler-win | `Scaler::with_format` can make its destination and staging surfaces `DXGI_FORMAT_NV12`, so the one `VideoProcessorBlt` that already scales also does the colour conversion, and `read_back` returns `height + height/2` rows at one pitch. NV12 is probed with `CheckVideoProcessorFormat`; an odd dimension or a refusal falls back to BGRA with a log line. `Backend::cpu_bridge_format()` is where a backend opts in. | BGRA is 4 bytes per pixel and NV12 is 1.5, so the `CopyResource` into staging, the blocking `Map` and the memcpy into the retained buffer all shrink by 62.5% — and every CPU-bridge encoder converts BGRA to 4:2:0 on the CPU anyway (x264 on a rayon pool, oneVPL through a second VPP pass, AMF through an internal converter), so that conversion moves onto silicon that was already running. | 1920x1080 → 1280x720, 60 iterations, median us/frame: **blt 5, readback 1192, total 1196**; 3,686,400 bytes | **blt 4, readback 740, total 743**; 1,382,400 bytes — **38% off the readback** | VERIFIED (`cargo test --lib scaler -- --nocapture`; two tests also assert the picture survives the conversion) | yes | **The encoder half has since landed on W3b's branch** (`worktree-agent-aac1ee30de0497a07`), built on this: x264 NV12 1655 -> 450 us/frame, the Intel CPU bridge 3766 -> 2820 us, and an identity-blt NV12 readback 2394 -> 1170 us. The handoff finding it was written against has been consumed and deleted. Until that branch merges, `cpu_bridge_format` on `latency-overhaul` still answers BGRA for every backend, so the scaler-side saving below is what this row alone buys. |
| 109 | tuning-win | The idle repeater thread drops from `THREAD_PRIORITY_TIME_CRITICAL` to `THREAD_PRIORITY_HIGHEST`, keeping MMCSS and the P-core confinement. | The process is `HIGH_PRIORITY_CLASS` and every capture and encode thread is time-critical, so N clients meant roughly 2N time-critical threads competing with the tokio transport workers — which sit at `ABOVE_NORMAL` and are what actually push RTP. The repeater emits a keepalive every 200 ms; being a millisecond late costs nothing. | 2-3 time-critical threads per session | one fewer per DXGI and per classic session | VERIFIED (builds and runs; the contention claim itself needs a 4-client host to measure) | yes | The x264 case is the worst one, because a time-critical CPU encoder can saturate cores. |
| 110 | audio (macOS) | Legacy-tier playthrough setpoint: fixed 4096 frames -> three of the output device's own IO periods, floored at 128 and ceilinged at 4096, raised 64 frames at a time only by a *measured* underrun; `DRIFT_GAIN` 2.0e-9 -> 5.0e-8 | The setpoint **is** the steady-state monitoring latency -- the drift loop's fixed point is that many frames of backlog. On this tier the virtual device is the system default output, so every sound the Mac makes passes through it before reaching the speakers. | 4096 frames = **85.3 ms** at 48 kHz, plus the device's own IOProc latency | 3 x 512 = 1536 frames = **32 ms** on a typical cycle; 3 x 128 = **8 ms** on a short one | COMPILE-CHECKED (setpoint arithmetic asserted by two new tests; click-to-sound needs a second machine to record with) | yes | The old gain needed ~30 s to move an 85 ms backlog, so the setpoint was the shock absorber for a loop too slow to reach it. 25x the gain against errors an order of magnitude smaller reaches the 0.3% trim limit (about five cents of pitch) in ~2 s. `IoCtx::underruns` was incremented and never loaded; the control thread now reports it and the bump every 10 s. |
| 111 | audio (macOS) | `capture_ns` end to end on the legacy tier: the driver stamps `mach_absolute_time()` in `OnProcessMixedOutput`, publishes it through the layout-2 shm anchor, and the encoder worker extrapolates by sample counter | W8's Windows note asked for the signed skew against `host_now_ns()` to be measured before trusting any hardware stamp -- on Windows loopback the endpoint timestamp turned out to be a *play cursor* about 15 ms in the future and had to be clamped. | drain-time stamp: the moment the worker woke up, plus a synthetic forward ladder that could regress between passes | capture -> Opus packet **p50 -0.57 / p90 3.59 / p99 4.54 ms** (median of three 10 s runs, 3277 packets); **0 of 3277** `capture_ns` deltas anything but exactly 5.000 ms; **0** regressions, **0** sequence gaps; encode p50 0.09-0.12 ms | **VERIFIED** on macOS 10.15.8 | yes | p50 is negative on purpose: the driver stamps the mix when it exists, before it would have been heard, so a packet reaches the encoder ahead of its own audio's playout -- under one IO cycle and in the right direction, unlike the Windows case. **No clamp applied**, and that is the decision: clamping would flatten the within-block ladder that produces the exact 5 ms spacing above. A fourth run across a 3 s silent gap gave exactly 1 non-5 ms delta (the re-seat), 0 regressions, 0 sequence gaps. |
| 112 | audio (macOS) | Tap aggregate `kAudioDevicePropertyBufferFrameSize` set to `clamp(128, range)`; the legacy tier does the same on the virtual device | The IO cycle is added to every sample before the ring sees it, and neither we nor the AudioCap reference the aggregate is modelled on ever set it -- the CoreAudio default for a built-in output is commonly 512 frames. | 512 frames = **10.67 ms**, assumed; never measured on a *tap* aggregate by anyone | legacy tier: requested 128, **granted 128 = 2.67 ms**, with `overruns` 0 and `dropped_backpressure` 0 | **VERIFIED** (legacy tier, 10.15.8); COMPILE-CHECKED (tap aggregate, needs 14.2+) | yes | `period_frames` was only ever written by the Windows path, so the 2 s summary reported the macOS IO cycle as `period=0fr`; both tiers now store the granted value, which is how the 128 above is known. SCK has no equivalent knob -- it owns no device. |
| 113 | audio (macOS) | `Producer::push`/`Consumer::pop` split-at-wrap `copy_nonoverlapping`; `convert_interleaved` fast path for 48 kHz stereo f32 interleaved (one memcpy plus one clamp pass) | Five to six scalar passes over every sample where two vectorisable ones would do, on the real-time capture thread where cost is IOProc deadline-miss risk rather than throughput. The ring stores `UnsafeCell<f32>`, which the compiler cannot see as contiguous through the per-element `get()`. | per-sample masked store and load; per-sample four-byte reassembly, a two-into-two "downmix", two clamps | two `copy_nonoverlapping`s per ring crossing; one memcpy plus one clamp pass on the format fast path | COMPILE-CHECKED (no per-callback timing harness on this tier; end-to-end encode p50 0.09-0.12 ms) | yes | `AudioFormatDesc::is_fast_path` existed to name exactly this case and had **no callers at all**, so the fast path it named did not exist. The clamp is kept deliberately -- the system mix is not bounded to +-1 and a bare memcpy would drop it. Two tests assert the fast path agrees with the general one on identical input, clamps, and stops at whichever buffer runs out first. |
| 114 | audio (macOS) | shm reader thread: `THREAD_TIME_CONSTRAINT_POLICY(5 ms)` + `park_timeout(500 us)` -> `user_interactive` QoS + `sleep(250 us)` | A Mach time-constraint policy declares a periodic deadline the scheduler holds the thread to; this one then slept for an unrelated interval, waiting for a wakeup that can never come -- the producer is `coreaudiod` in another process writing to shared memory, so nothing unparks it and `park_timeout` only wrapped a plain sleep in a mutex and a condvar. | 0-500 us of added delay per drain, on a thread the scheduler may demote out of the real-time band for missing its declared deadline | 0-250 us, a quarter of the driver's own 2.67 ms IO cycle, on the QoS class the work actually wants | **VERIFIED** as part of the end-to-end p99 of 4.54 ms in row 97 | yes | The finding offered `mach_wait_until` as the lower-latency, higher-risk alternative. Rejected: the policy was never honoured in the first place, and the poll is already a fraction of the block period it drains. |
| 115 | audio (macOS) | Crash-recovery LaunchAgent: installed per start and never removed, `StartInterval` 30, each run forking `pgrep -x ScreenExtend` -> installed once, removed on every path that ends its job, `StartInterval` 60, lock-file check | A permanent background job the user never consented to, surviving app deletion, relaunching a stale absolute path, and doing a process spawn plus HAL IPC 2880 times a day for a case that only matters after a crash. | 2880 launches/day, **forever**, each forking `pgrep` and parsing PIDs | 1440/day and **only while the legacy tier is streaming**; the agent removes itself the first run that finds no host holding the lock | **VERIFIED** (install and removal observed on 10.15.8; `launchctl list \| grep screenextend` is empty after a clean stop) | yes | `single_instance` already keeps an exclusive `fs4` lock for exactly this question -- one `open` plus one `flock` replaces the spawn -- and on macOS its directory is the parent of the routing state file, which the watchdog can resolve without a Tauri app. |
| 116 | audio (macOS) | Volume-key `CGEventTap` callback: synchronous `AudioObject{Get,Set}PropertyData` -> classify and `try_send` on a 16-slot bounded channel to a worker thread | An *active* session-level tap with `kCGEventTapOptionDefault` blocks **every** keyboard and mouse event in the login session for its duration, including the remote-control input path this product exists to provide. Two `coreaudiod` round trips inside it overrun the tap deadline when the daemon is busy, and macOS then silently disables the tap -- which is why the `kCGEventTapDisabledByTimeout` re-enable branch already existed. | two synchronous HAL round trips per volume key, in the session input path | classify plus one `try_send`; no IPC, no allocation | COMPILE-CHECKED (the tap is opt-in via `set_legacy_volume_key_proxy` and needs Accessibility; the structural claim is by construction) | yes | `try_send`, never `send`: a full queue means the worker is behind on key repeats, and dropping one of those is invisible where stalling the session's input path is not. Also fixed the mutex held across `VolumeKeyTap::start`'s blocking handshake, and the `unwrap()` that let one panic poison every later call. |
| 117 | audio (macOS) | SCK's two async-to-sync bridges 5 s -> 1.5 s; `start_capture`'s `ready_rx.recv()` given a 15 s ceiling where it had none | Both SCK waits are taken serially under the `AudioHub` mutex, which holds every other session's audio subscribe and unsubscribe, and both are hit in the denied-Screen-Recording case a user is most likely to meet. | up to **10 s** of frozen audio subscribe for every other device, more once the candidate loop moved on to the next backend; `start_capture` could wait forever | 3 s worst case for the SCK pair, 15 s hard ceiling overall | COMPILE-CHECKED (13.0+ path) | yes | SCK offers no non-blocking variant, so bounding the waits is the whole fix. A display list or a stream start that has not arrived in 1.5 s is not going to. |
| 118 | audio (macOS) | Driver shm ring layout 1 -> 2: capture-time seqlock anchor, reader-published `readPos`, `overruns` actually written, scalar write loop -> two `memcpy`s, umask cleared across `shm_open`, `os_log` throughout | The ring carried no time, so a reader could only date samples from its own drain. `overruns` was declared in the ABI and never incremented, so the one signal that says this tier is dropping samples read a constant 0. And `shm_open` applied `coreaudiod`'s umask, so under 077 the host got a bare `EACCES` and silently took the slower HAL-input path with nothing anywhere saying so. | no anchor, no `readPos`, `overruns` always 0, 1024 scalar stores per callback, mode at the mercy of a umask this code does not control, and zero logging in the entire driver | anchor published per block; `readPos` tracking `writePos` at a backlog of **0** with `overruns` **0** over 2,083,840 samples; `O_RDWR` **granted**; geometry and errno visible in `log stream` | **VERIFIED** on macOS 10.15.8 | yes | ABI bumped on both sides in one change (`shm_ring.hpp` <-> `shm_reader.rs`); a layout-1 segment is unlinked and recreated at load. `legacy/test/ring.rs` now **scrapes the C++ header** and fails if the two drift, instead of comparing Rust constants to literals typed in the same repo. |
| 119 | audio (macOS) | `dropped_backpressure`: the worker's unconditional `store(consumer.overruns())` per drain pass removed; the capture callbacks and the shm reader own the counter | Three producers `fetch_add` into it -- ring overruns from each callback, and the samples the legacy reader loses to a lapped shared-memory ring, which is a different loss with a different cause and the interesting number for that tier. The worker then overwrote all of it within milliseconds. | legacy-tier loss reported as a flat **0** in `AudioDiagnostics::summary()` | both loss sources accumulate; measured 0 on a healthy run, which now means something | **VERIFIED** on macOS 10.15.8 | yes | Picked the producers as the single owner rather than adding a second field, because both numbers are genuinely "samples lost to backpressure" and the shared `AudioDiagnostics` struct is W8's to grow. |
| 138 | macos capture | CGDisplayStream: `IOSurfaceIncrementUseCount`/`DecrementUseCount` around every surface held past the callback (RAII `HeldSurface`), `kCGDisplayStreamQueueDepth` 2 -> 5, `kCGDisplayStreamMinimumFrameTime` deleted | `CGDisplayStream.h` requires *both* a `CFRetain` and a use-count increment of a client that keeps a delivered surface; we did the retain only, so WindowServer was free to composite the next frame into the buffer VideoToolbox was reading. Marking surfaces in use makes the queue depth real for the first time, so the depth had to move in the same change: 2 was below CoreGraphics' own documented default of 3 while the pipeline pinned two surfaces. `MinimumFrameTime` defaults to zero, so setting it to 0.95/fps added a hold-off nothing asked for. | queueDepth 2 + the use count missing: a 58 fps capture collapsed to one frame in six seconds the moment an encoder joined the holding pattern | 61 fps sustained (365/367/366 frames in 6 s), **0** frames dropped by WindowServer over three trials | VERIFIED (10.15.8) | yes | The three had to land together: the increment alone converts a corruption bug into a starvation bug, and the hold-off is invisible while the queue is the binding constraint. |
| 139 | macos capture | the first frame of a session is exempt from the empty-dirty-rect skip; both wrap paths take ownership of the `CVPixelBuffer` before checking the `CVReturn` | A freshly created virtual display is completely still until a window is dragged onto it, so every frame reports zero dirty rects, nothing is ever published, and the 200 ms keepalive has no previous frame to repeat. CoreVideo can also return non-zero *and* hand back a +1 buffer. | client saw black until something moved | first frame always published | VERIFIED (10.15.8) | yes | The other half of "connected but black" is row 94. |
| 140 | macos capture | CGDisplayStream de-linked: five functions and five CFString constants resolved by one cached `dlsym` | The API is *obsoleted*, not merely deprecated, in the macOS 15 SDK, and it is the fallback tier. A link-time reference to a symbol Apple has announced the end of makes the entire binary unloadable the day it goes — the rule the codebase already applies to ScreenCaptureKit, in the other direction. | `nm -u \| grep -i displaystream` listed 10 symbols | listed none; capture and the TCC probe both run through the resolved pointers | VERIFIED (10.15.8) | yes | A machine without the API now fails its capture start with a line, instead of failing to launch. |
| 141 | macos encode | `VTCopyVideoEncoderList` logged once per process; `VTCopySupportedPropertyDictionaryForEncoder` queried per session; every `macos_at_least` gate on this path deleted | The encoder was configured from OS version numbers. The property set actually depends on the encoder VideoToolbox picks, which depends on the frame size and the machine. The version gate on `EnableLowLatencyRateControl` named 11.0; the key is 11.3, and the spec ladder already answers the question by trying. | no inventory, no property report, one `sw_vers` fork per query | one `[vt] encoder:` line per H.264 encoder (id, hardware, GPU, instance-limited) and one `[vt] properties:` line per session | VERIFIED (10.15.8) | yes | `InstanceLimit` is the documented mechanism behind "it only works when I quit everything else", and nothing had ever printed it. |
| 142 | macos encode | `UsingHardwareAcceleratedVideoEncoder` read back and stored; a software session logs at error level; `CompatibilityReport.encoder_backend` reports the probed tier | The `hardware` flag recorded on success was the flag that had been *requested*. A session created on the `Require` rung can still be software, and then nothing was logged at all. | `encoder_backend` was the constant `"videotoolbox"` | `videotoolbox-hardware` / `videotoolbox-software`, from the encoder list at report time and from the session read-back at stream time | VERIFIED (10.15.8) | yes | `UsingGPURegistryID` is NULL for a built-in-GPU hardware encoder *and* for software, so it can never stand in for the hardware flag. |
| 143 | macos encode | `PreferredEncoderGPURegistryID` from `CGDirectDisplayCopyCurrentMetalDevice` of the captured display | On a multi-GPU Mac, WindowServer vends the display's IOSurface from the GPU driving that display while VideoToolbox picks an encoder by its own criteria; when they differ, every frame migrates across the PCIe bridge. `Preferred`, never `Required`, which would defeat the fallback ladder. | not set | set when the display reports a Metal device | COMPILE-CHECKED — the 10.15.8 dev Mac is single-GPU, so it correctly reports `gpu_registry_id=none` | yes | Needs a 2016-2019 15-inch MacBook Pro or an eGPU to verify. Script: `docs/manual-tests/macos-capture-encode.md` §9. |
| 144 | macos encode | `DataRateLimits` removed; `MaxFrameDelayCount` unset; `MaxKeyFrameIntervalDuration` 10 s -> 600 s | QA1958 defines `DataRateLimits` as a hard cap over a sliding window and `AverageBitRate` as a target; Chromium documents the cap as undocumented-incompatible with `EnableLowLatencyRateControl` and as making the encoder undershoot regardless. The loss-based controller already owns the bitrate. `MaxFrameDelayCount` is a contract on the *calling* thread, so `1` blocks the deadline-scheduled encode loop inside the encoder. A 10 s periodic IDR is a bitrate spike the removed cap then had to absorb. | two rate controllers with different time constants, plus a periodic IDR | one target, no cap, keyframes only on request | capture-to-encoded p50 **13.70 ms**, p99 15.60 ms, over three 6 s trials at 1440x900@60 immediately before this row landed (medians of 13.587/13.910/13.695 and 40.894/15.597/15.499). VERIFIED as the *before*; the *after* run is queued behind four other agents’ release builds on the shared Mac and is not in yet | yes | OBS reports `RealTime = true` *plus* bitrate limiting caused heavy frame drops on M1-era Macs; the pairing is gone, `RealTime` stays. |
| 145 | macos encode | per-frame stamps travel through VideoToolbox as `sourceFrameRefCon` | A `std::sync::Mutex<VecDeque>` sat between the real-time encode thread and the drain thread, and assumed submit order equals emit order. VideoToolbox carries an opaque pointer per frame for exactly this. | a lock per submit and per emit | none; the stamps arrive with their own frame | VERIFIED (10.15.8) | yes | Also deletes the ordering assumption, which mattered the moment frames could be dropped. |
| 146 | macos encode | encoder output channel bounded at 4, drop-oldest, counted, and the count forces an IDR | The channel was unbounded, so a drain thread that fell behind turned into latency rather than into a visible drop. A dropped access unit breaks the reference chain, which is why the count has to become a keyframe. | unbounded | 4, with `encoder_output_drops` in `status --json` | VERIFIED (10.15.8: 0 drops over three 6 s runs) | yes | Dropping the oldest keeps the freshest picture; either end of the queue needs the IDR. |
| 147 | macos encode | frames in flight counted and capped at 2, with the cap standing aside to 16 after 100 ms of no output; the blocking `CompleteFrames` is gone from the encode loop | VideoToolbox accepts frames until memory runs out. A comparable product measured ~8 resident when overfed — 130 ms at 60 fps, invisible to every timer we have. But a hard cap against an encoder that structurally holds frames is a deadlock, which is what the old blocking flush on the keepalive path was really working around; that flush was synchronous and unbounded on a deadline thread, five times a second on a still screen. | unbounded, plus a blocking flush per keepalive | 2, or 16 against a frame-holding encoder, and no flush | same before-number as row 144 (p50 13.70 ms / p99 15.60 ms); the after-run is queued. The 10.15 encoder is a frame-holder, so it is tier 2 of the cap that applies here — which is the case that would have deadlocked under a hard cap, and the reason the bypass exists | yes | The max depth reached is logged next to VideoToolbox's own `NumberOfPendingFrames`; a disagreement between the two says which problem you have. |
| 148 | macos encode | a submit failure no longer ends the stream: three consecutive failures rebuild the encoder, old session released first | One `VTCompressionSessionEncodeFrame` error broke out of the encode loop forever. The session stayed up, the transport stayed connected, and the picture simply stopped, with one line in the log. | first error = permanent freeze | up to three rebuilds, then a deliberate end so clients are told | COMPILE-CHECKED — the failure is not reproducible on demand | yes | The old session is released *before* the replacement is asked for; the hardware encoder is instance-limited, so the other order turns a transient error into a permanent software fallback. |
| 149 | macos encode | a first-frame deadline: 750 ms with no capture yields a synthesized black IDR, counted as `capture_no_first_frame` | A capture backend that delivers nothing produced a session that negotiated, connected, reported healthy and showed nothing, forever — this is why every macOS cell in `docs/BASELINE.md` was empty. | `probe --whep-selftest` sent 0 frames; `probe --live` wrote 2 in 5 s | the client gets a decodable picture within a second, and the silence is visible in `status --json` | VERIFIED (10.15.8) | yes | Pairs with row 84: one fixes the still-display capture, the other covers a backend that fails outright. |
| 150 | macos sched | one scheduling scheme per thread; every `thread_policy_set` / `pthread_set_qos_class_self_np` return checked and logged once; `THREAD_TIME_CONSTRAINT` computation `period/8` -> `0.75 x period`, read back with `thread_policy_get`; `os_workgroup` deleted; `setpriority(-10)` deleted | The encode thread was given a QoS class *and* a deadline policy *and* a precedence policy *and* an affinity tag, with every return value discarded — a QoS class is how the kernel manages priority and a deadline reservation is how that management is removed, so the last writer won and nothing said which. 2.08 ms of a 16.67 ms frame is under a third of what the work costs, and a reservation smaller than the work is worse than none. The workgroup was created and joined but never given an interval. `setpriority(-10)` needs root and printed EACCES on every normal launch. | budget 2.08 ms, no readback, one inert workgroup, one error line per launch | budget 12.5 ms, effective policy printed, no workgroup, no error line | VERIFIED (10.15.8) | yes | Research is explicit that a `DEFAULT`-type workgroup is a no-op and interval workgroups are audio-only. |
| 151 | macos capture | the 60 fps floor in `live_encoder_config` is gone | `refresh.clamp(60.min(max_fps), max_fps)` could only ever raise the rate, so a 30 Hz display or a client that asked for 30 was captured and encoded at 60: twice the frames, none of them new, competing with the ones that were. The `refresh_hz == 0 -> 60` line one statement earlier already covers a display that reports nothing sensible. | 30 Hz display encoded at 60 | encoded at 30 | VERIFIED (unit-tested; the dev display is 60 Hz so the behaviour change is asserted, not measured) | yes | |
| 152 | macos capture | ScreenCaptureKit: `queueDepth` 3 -> 8, `minimumFrameInterval` 0.9/fps, `SCFrameStatusStarted` accepted, presentation timestamp read, colour matched to the CGDisplayStream path, `stop()` blanks the delegate ivar and drains the delivery queue before freeing its context | Apple raised its own documented default for the queue from 3 to 8 and describes depth as memory against stalling, not latency; the comment claiming "3 is lowest latency" had it backwards. OpenDisplay measured ~51 fps out of a request for exactly 1/60 and OBS ships a 10 % shortening for the same beat-frequency reason. `Started` is the first frame after `startCapture`, carrying the whole display, and it was discarded. The PTS is host-clock nanoseconds, so it needs its own conversion — the tick path would have scaled it twice. | depth 3, exact interval, first frame discarded, `os_time_ns` always 0, SCK colour defaults, ctx freed with a handler possibly in flight | as described | **COMPILE-CHECKED** — the dev Mac is 10.15.8, where ScreenCaptureKit does not exist | yes | Script: `docs/manual-tests/macos-capture-encode.md`. |
| 153 | macos capture | capture tier chosen by `AnyClass::get`, and ScreenCaptureKit not committed to until it has delivered a frame (1500 ms); the CGDisplayStream TCC probe is not reached on a host that has SCK | The selector was a `sw_vers` fork whose parse failure returned *true*. Chromium reports full-display SCK capture is broken on 12.3 — it starts and delivers nothing, which from outside is indistinguishable from slow. Probing with the obsoleted API on a modern Mac is also what raises Sequoia's recurring privacy alert. | version string comparison, no frame-level check | class probe plus a first-frame gate, both logged | VERIFIED (10.15.8 selects CGDisplayStream by class absence); the frame gate is UNVERIFIED | yes | A mid-session fallback from SCK to CGDisplayStream was considered and **rejected**: swapping capture backends under a live session risks a worse failure than the stall it papers over, and CGDisplayStream would not fix a WindowServer stall anyway. The watchdog that matters is row 94, which covers both. |
| 154 | macos | `pipeline::start` returns a `SessionCapture` instead of leaking; the drain thread reuses one Annex B buffer; the wake token is consumed when a frame is taken; the reachable CoreFoundation panics on the encode path return `None` | `start` called `mem::forget` on the capture backend and the latency activity and dropped the stop flag and the join handle, so one call permanently held a capture stream, a VideoToolbox session, a real-time thread, a process-wide App Nap suppression and two power assertions. The wake channel is a `bounded(1)` counter and a frame taken after a timeout left its token behind, so the next wait returned instantly with nothing to take. | leaks per call; a `Vec` grown from empty per frame; a spurious wake per timed-out take; `expect` on every `CFDictionaryCreate`, including the per-frame force-keyframe dictionary | none of the above | VERIFIED (10.15.8; `start` has no production caller, so the leak was latent) | yes | |
| 155 | macos encode | `ConstantBitRate` and `BaseLayerFrameRate` probed and reported, deliberately not set | CBR spends bits on a static screen to hold the rate, which is the opposite of what a screen sharer wants and is why the removed `DataRateLimits` was not replaced with one. `BaseLayerFrameRate` is hierarchical-P temporal layering: it adds reference depth to buy scalability nothing here consumes. | both unmentioned | both named in the `[vt] properties:` line as `available` or `absent` | VERIFIED (10.15.8 reports both `absent`) | n/a — recorded rejection | The report exists so a future reader can see the decision was made rather than missed. |
| 178 | macos build | `NSSupportsAutomaticGraphicsSwitching` = true in `src-tauri/Info.plist` | On a dual-GPU Mac, an app that touches Metal or OpenGL without this key forces the discrete GPU on for its entire lifetime, streaming or not. QA1734 attaches a condition — the app must work correctly with multiple GPUs — which is satisfied by construction here: `gpu.rs` is an empty placeholder and nothing in this process does direct GPU work; the surfaces come from WindowServer and go to VideoToolbox. | absent | present | **COMPILE-CHECKED**, and inert until a separate fix lands: `findings/minor-build-mac-info-plist-is-never-referenced-by-either-tauri-config.md` says `src-tauri/Info.plist` is not referenced by `tauri.conf.json` or `tauri.macos.conf.json`, so the key does not reach the bundled app yet. W13b is wiring the config on its own branch. | yes | Manual check once both are in: `defaults read ScreenExtend.app/Contents/Info.plist NSSupportsAutomaticGraphicsSwitching`. Pairs with row 143 — pinning the encoder to the display GPU is only half the decision if the app is also forcing the wrong GPU on. |


| 156 | ui (desktop) | Deleted `ui/form.tsx` and `ui/resizable.tsx` (no importers), `generateSlug`, `useInterval`, two never-assigned `Window` globals and a commented-out component; dropped nine now-unreferenced runtime dependencies | Rollup already tree-shook the two components out of the bundle, so the claim to test was whether removing them was pure hygiene or an actual size win. It is a CSS win: tailwind was still scanning their class names. | `main-*.js` **565.27 kB** (gzip 181.44), `main-*.css` **52.92 kB** (gzip 9.33) | JS **565.26 kB**, CSS **50.14 kB** (gzip 9.03) -- CSS -2.78 kB (-5.3%), JS unchanged | VERIFIED | yes | Deps removed: `react-hook-form`, `react-resizable-panels`, `@radix-ui/react-icons`, `zod`, `@hookform/resolvers`, `react-avatar`, `react-svg`, `@radix-ui/react-tooltip`, `@tauri-apps/plugin-dialog`. `motion` looks unused and is not -- it is `nextstepjs`'s peer and the walkthrough breaks without it. |
| 157 | ui (desktop) | Four upstream shadcn defects: `TOAST_REMOVE_DELAY` 1,000,000 ms -> 5,000; a guard on `InputOTPSlot`'s `context.slots[index]`; `SheetContent`'s unused `trapFocus` prop deleted; `Button`'s `hover:opacity-75` merged before `className` instead of after | All four shipped verbatim from `npx shadcn add`. The toast one is the only ongoing cost: every dismissed toast held its object and a `setTimeout` handle for 16 min 40 s in a process that stays open for the whole streaming session. | one live `toastTimeouts` entry per toast for 16 min | freed 5 s after dismissal, which is after Radix has already unmounted the toast | VERIFIED (build + lint); the timer window is arithmetic | yes | `hover:opacity-75` stayed out of `buttonVariants`: alert-dialog.tsx and devices.tsx call that directly and must keep rendering without the fade, so moving it there would have been a visible change. `<Input>`'s `forwardRef` render function also took one argument, so every ref handed to it stayed null. |
| 158 | ui (desktop) | `index.html` paints the two literal `--background` values on `html`, and `ThemeProvider` writes the user's theme choice to `localStorage` as a synchronous paint hint that the inline script and the provider's initial state both read | Nothing set a background before the render-blocking 50 KB stylesheet arrived, so both themes opened on the WebView's white canvas; and the inline script resolved the theme from `prefers-color-scheme` alone, so a user on a dark OS who had chosen light saw dark chrome until `config.json` round-tripped over IPC. | white frame every launch; wrong-theme frame on every launch where the choice differs from the OS | first painted frame matches the settled frame in both cases | VERIFIED (build); the frame-by-frame capture is `docs/manual-tests` work | yes | Deliberately no `<meta name="color-scheme">`: it also restyles the native form controls, which would be a visible change. Only `html` is painted -- `body` takes `bg-background` from the stylesheet and covers the viewport, so the literal never competes with the token. |
| 159 | build | `vite.config.ts`: an `existsSync` guard with a real message in `monacoDeps()`, `handleHotUpdate` invalidating the `classicWorkers` bundle cache, header comments on `classicWorkers` and on the `esbuild.target` 13.0-vs-13.1 discrepancy; `index.html` added to tailwind's content globs; `scripts/**/*.mjs` brought under ESLint | Renaming `src/lib/monaco-setup.ts` or `json-editor.tsx` killed every vite command with a bare ENOENT thrown from inside the config, before any project code was reached; and the memoised worker bundle was never invalidated, so editing a worker entry served the stale build for the rest of the dev session. | ENOENT with no explanation; stale worker until restart; the compatibility gate itself unlinted | named error; cache dropped on change; `eslint .` covers `scripts/` | VERIFIED | yes | The tailwind glob is a trap-remover, not a size change: the emitted stylesheet is byte-identical with and without `./index.html`. `build.sourcemap: "hidden"` was considered and rejected -- see the note below. |
| 160 | ui (desktop) | `DeviceDetails` split into a light trigger and a body that mounts with the sheet (held `SHEET_EXIT_MS` = 500 ms across Radix's close animation); table rows keyed on the device's trust token instead of the array index | Radix unmounts `SheetContent` while closed, but the component around it -- a formik instance with its validation and touched machinery, a config-store subscription, two effects and a 500-line render function -- ran once per row on every parent render. Since the context split (row 6) a toast no longer re-renders the app, but `deviceModify` still did. | 5 connected devices = 5 live formik instances, all re-rendering on any parent render | 0 while every sheet is closed | VERIFIED (build + lint); the DevTools commit count is `docs/manual-tests` work | yes | The index key was also a correctness bug: `deviceJoin` rebuilds the list as filter-then-push, so a rejoining device moves to the end and shifts every index after it; React then reuses a row instance across two devices and can carry an open sheet's form state onto the wrong one. |
| 161 | ui (desktop, macOS) | Permission-gate poll: fixed 200 ms -> 200 ms for the first 2 s, then 1 s to 10 s, then 3 s | The gate is open for as long as the user is in System Settings, which is minutes. 200 ms was five IPC round-trips a second into TCC for all of it. The `focus` listener on the next line is what actually makes the dialog feel instant. | **~300** `check_permissions` calls in the first minute | **~35** worst case in the first minute (10 + 8 + 17) | VERIFIED by construction; not executed here -- Windows and Linux return an empty permission list, so this path never runs on this box | yes | User-visible behaviour is unchanged: flipping the permission in System Settings and returning to the app still advances immediately, via `focus`. |
| 162 | ui (desktop) | `QrDisplay` memoized and its `<QRCode>` element keyed on `[url, blurred]`; `App.tsx` hands back the previous `qrValues` array when the newly built one is equal | An N-adapter host builds 2N QR codes -- the wide row and the carousel each render the full set -- plus two for the cloud URL, and none was memoized, so all of them re-ran Reed-Solomon and matrix generation whenever `qrValues` was reallocated. `networkChange` reallocates it on any adapter change: a VPN coming up, Wi-Fi roaming. | 2N+2 re-encodes per `networkChange`, on an adapter set that is usually identical | 0 when the adapter set is unchanged; only the changed card otherwise | VERIFIED (build + lint) | yes | `size={500}` was left alone deliberately: the prop is redundant against the `viewBox` but the encode cost does not depend on it, so changing it is risk without gain. |
| 163 | ui (desktop) | Walkthrough highlight: unconditional `requestAnimationFrame` loop -> full rate only during a 700 ms burst after a scroll, a resize or any CSS transition in the document, with a 250 ms idle net and the target element cached between measurements | A `querySelector` plus a `getBoundingClientRect` -- a synchronous layout flush -- at the display's refresh rate for as long as a step was on screen, on a host that is simultaneously capturing, encoding and streaming a display. | 60-144 forced layouts/s for the whole tour | ~4/s when nothing is moving; unchanged while the 300 ms sidebar collapse or a resize is in flight | VERIFIED (build + lint) | yes | `transitionrun`/`transitionend` is what covers the sidebar collapse -- the case the rAF loop existed for and the one `onResized` alone missed. |
| 164 | ui (desktop) | `get_avatar`/`set_avatar` carry base64 `String` instead of `Vec<u8>` (`number[]` over the bridge); `loadAvatar` assigns the result straight to `<img src>` | A 256x256 PNG crossed as a JSON array of decimal integers -- 3-4 bytes of JSON per image byte -- was parsed into an array of tens of thousands of boxed numbers, walked into a `Uint8Array`, wrapped in a `Blob` and run through a `FileReader` to produce the base64 data URL the `<img>` wanted in the first place. This runs on every launch, concurrently with `setup()`. | ~50 KB PNG -> **~180 KB** JSON payload plus three representations | **~67 KB** base64 payload, one representation, no conversion | VERIFIED (build, clippy, 186 tests, bindings regenerated from a debug build with no CLI args -- the diff is the two signatures and nothing else) | yes | `write_avatar`/`read_avatar` stay byte-level so the `account avatar set|show` CLI path does not round-trip through base64 for nothing. `base64` was already in the tree via the webrtc stack. |
| 165 | ui (desktop) | Settings: `sortedKnownDevices` memoized over a `Set` of connected IPs; the Past Devices table extracted into a `React.memo` child | The comparator called `connectedDevices.some(...)` twice per comparison, so the sort was O(n log n * m) and ran on every render -- which on this page means every keystroke in any of eleven controlled inputs. | re-sorted per keystroke; N rows re-rendered per keystroke | sorted only when the device lists change; 0 rows re-rendered per keystroke | VERIFIED (build + lint) | yes | The finding's larger proposal -- extracting all seven `<Card>`s -- was not done: state crosses card boundaries in this 1616-line component (the session OTP feeds both the session card and the wifi-QR dialog), and both concrete sub-issues it named are fixed. |
| 166 | ui (desktop) | Seven `catch {}` blocks that discarded the reason now report it; type-aware ESLint turned on with `@typescript-eslint/no-floating-promises` as a warning | `console.error` reaches the Rust log bus since the error-reporting wrapper landed, so a swallowed reason is now cheap to surface. The updater one mattered most: a network failure, a bad signature or a malformed `latest.json` was indistinguishable from "no update available". | 7 silent failure paths; every type-aware lint rule off | all 7 on the log bus; `no-floating-promises` finds 18 real sites | VERIFIED | yes | `pnpm lint` 3.2 s -> 6.7 s. The rule is a warning, not an error: most hits are formik's `setFieldValue`, which cannot reject. |
| 167 | ui (desktop) | `<link rel=preload as=font>` for the webfont, injected from the emitted bundle by a `transformIndexHtml` plugin; the dashboard carousel's scrollbar rule moved out of a per-render inline `<style>` into `index.css`; `transition-all duration-500` on the inner sidebar narrowed to `transition-[border-right-width]` | The font is declared inside the 50 KB render-blocking stylesheet, so its fetch only started once all of that CSS had been parsed, and `font-display: swap` paints the fallback first. The sidebar's `transition-all` was inert during the collapse but made the style system keep a full transition snapshot on every recalc, on the same subtree as a 300 ms per-frame layout. | font discovered after 50 KB of CSS; a `<style>` element injected on every dashboard render | font fetch starts with the head; the rule is in the stylesheet | VERIFIED (the preload link is in `dist/index.html`) | yes | The font's `serif` fallback -- the visible half of that finding -- is still open under "UI-visible recommendations". |
| 168 | ui (desktop) | Past Devices rows keyed on `${token}\|${ip}` instead of `ip`; the device-editor body mounts during render rather than from an effect | Device trust is keyed on the per-device token, so one IP legitimately owns several rows -- one per device that has used the address -- and keying on the IP alone collapsed them onto a single React identity, which is how a row can show another device's ban state. | React's duplicate-key warning on every Settings render, invisible in a release build | unique per row | VERIFIED -- found by running the app, not by reading it: `logbus::set_verbose(true)` forced on in a debug build made the frontend's `console.error` forwarding print to the terminal, and this was the only frontend line in the whole boot | yes | The same bug in the *connected* devices table was fixed in row 87. Worth noting as a method: the error-forwarding wrapper turned a class of React warning that nobody can see in a release build into something a terminal run reports. |
| 169 | audio (macOS) | SCK audio teardown: blank the delegate ivar, stop the stream, post a synchronous barrier onto the serial delivery queue, then free the context box | `stop()` freed the box after releasing the stream, delegate and queue, on a comment that asserted no callback could still read it. Releasing a `DispatchQueue` handle neither drains nor cancels blocks already enqueued on it, and `stopCaptureWithCompletionHandler:` is not documented as a barrier against an in-flight `stream:didOutputSampleBuffer:ofType:` -- so the delegate surviving one extra retain handed a real-time callback a dangling pointer. | a use-after-free racing the `UserInteractive` delivery queue on every teardown, presenting as a rare crash on disconnecting the last audio-enabled device -- or as nothing | the ivar is null before the stop, and the serial queue is drained to a barrier before the free; three ordered steps with no window between them | COMPILE-CHECKED (13.0+ path, no machine on this project runs it) | yes | The Process Tap sibling needs none of this: `AudioDeviceDestroyIOProcID` is documented not to return while the IOProc runs. `macos_utils/streamer/sck.rs` (W5) still has the identical shape and the recipe transfers verbatim. |
| 170 | audio (macOS) | SCK dummy video stream: added `showsCursor: false` and `queueDepth: 3` to the existing 2x2 at 1 fps | SCK has no audio-only mode before the 14.2 Process Tap, so this backend must run a real screen stream to get audio at all. The Screen Recording requirement and the menu-bar indicator are Apple's constraint; what the stream costs while it is up is not. | 2x2 at 1 fps with default cursor compositing and default queue depth | same geometry with cursor compositing off and the shallowest queue SCK accepts; no screen output is registered, so every frame produced is dropped at the delegate | COMPILE-CHECKED (13.0+ path) | yes | `powermetrics` on a 13.x machine is the measurement; `setExcludesCurrentProcessAudio: true` was already correct, so the host's own audio is not fed back. |
| 171 | client / input | The optional input tails are sent, gated on the host's `PONG` capability bits: a `u64` client stamp on `POINTER_*`, `POINTER_MOVE_BATCH`, `WHEEL`, `KEY` and `MOUSE_DELTA`, and the `MOUSE_DELTA` event-flags byte carrying `UNADJUSTED` (`static/input.js`) | The host can measure its own queue and injection but not the wire: `input_recv` is derived from a client stamp no client sent, so the histogram stays empty however busy the session is. Separately the client knew whether `unadjustedMovement` had been granted and had nowhere to put it, so the host could not tell whether the deltas had already been through an OS pointer curve. | `input_recv` count 0 on every session; the host guesses about the pointer curve | frames carry the stamp and the flag once the host advertises bits 3 and 4, and are byte-identical to v1 until then | COMPILE-CHECKED (`node scripts/test-client-stats.mjs` asserts both directions byte for byte, and the merged host advertises all four bits and reads every tail at the offsets asserted here; nothing has been measured end to end, because neither dev box can run the client page) | yes | Gated rather than sent unconditionally even though the host's length checks are floors: a stamp a host does not read is bytes buying no measurement, and byte 8 of `MOUSE_DELTA` is a byte an older host reads as the pad it used to be. `hostCaps` is 0 until the first `PONG`, so a session's first second is plain v1 -- which is also what an older host gets for its whole life. |
| 172 | client / audio | `audioCapabilities` gains `wantsFallbackTrack` and `verified`; `ontrack` remembers the audio stream instead of attaching it (`static/audio.js`, `static/index.html`) | WHEP has no renegotiation verb, so a track absent from the first answer can never be added later. A client claiming the WebCodecs path is answered with no audio section at all, which makes every way that path can fail *after* the answer a silence with no route back -- and the client is the only side that knows whether a spare track would be useful. | a mid-session decoder failure is silence until the user rejoins | the request is on the wire, and the client attaches a spare track only once the fast path is gone | UNVERIFIED (both halves are now on the branch: `AudioCapabilities` reads both fields, logs `verified` and honours `wantsFallbackTrack`; no browser here to join with) | yes | Attaching on `ontrack` is what would have made a spare track a regression rather than a safety net: a fast-path client handed one would report `netEQ` in the stats frame and play a silent element beside its worklet. The retirement and worklet-failed paths recompute the request, since off the fast path the host already sends a track it feeds. |
| 173 | client / audio | Samples go worker to worklet over a transferred `MessagePort` when there is no `SharedArrayBuffer`, instead of worker to page to worklet (`static/audio.js`, `audio-worker.js`, `audio-worklet.js`) | 200 packets a second at the host's 5 ms framing means 400 postMessage hops and 400 typed-array allocations, half of them on the main thread that also runs the reconfig fetch, the `getStats` poll and every pointer event. A long task there is an underrun, and an underrun re-primes the jitter buffer -- up to `MAX_TARGET_MS` of silence from one stall. | 2 hops and 2 allocations per packet, one hop on the main thread | 1 hop, no main-thread involvement, same 2 allocations | UNVERIFIED (no browser on either dev box; the path also needs a secure context that is *not* cross-origin isolated, which is why it is almost never exercised) | yes | Offered, not assumed: the page sends the far end to the worklet, the worklet acknowledges on its own port, and only then does the worker get the near end. An engine that will not transfer a port into an `AudioWorkletGlobalScope` never acknowledges and the page relay carries on unchanged. `getSyncInfo().transport` gained a third value (`port`) so which of the three is live is readable. |
| 174 | client / net | The `/audio-outputs` poll drops from 4 s to 30 s where `devicechange` exists, and `postOutputs` skips a body identical to the last one sent (`static/audio.js`) | Where the event exists the poll is a redundant second mechanism, and the POST fired unconditionally for a list that changes a handful of times in a session. | 15 enumerations + 15 POSTs per minute per device | 2 per minute where `devicechange` works, and a POST only when the list differs | UNVERIFIED | yes | The identity setter clears the cache, so a new connection -- including a reconfigure that rebuilt a session with the same token -- always posts once. Where `devicechange` is absent the 4 s poll is unchanged, because there it *is* the mechanism. |
| 175 | client / input | Dropped files are screened against `pc.sctp.maxMessageSize` before they are read, and a refusal is named; `rawSend` reports a throw once per session (`static/input.js`) | A drop is one SCTP message with no chunking opcode and no host-side reassembly, so anything above the negotiated maximum threw inside an empty catch: no transfer, no error, no console line, nothing the host could log. The whole-file read also allocated the file before any size was checked. | a large drop silently does nothing | the file is refused before it is read, with the size and the limit named | UNVERIFIED | yes | Enforced only where the browser reports a limit: `maxMessageSize` can be `Infinity` and `pc.sctp` is absent on Safari and below Chrome 76, so refusing on a guess there would break drops that work today. Chunking is the real fix and needs a host opcode -- rejected here, see the note below. |
| 176 | client | The refresh-rate refinement is a *second* 60-sample measurement taken once the page is live, and is applied once by a reconfigure when it disagrees with the join-time number by more than the clamp step (`static/index.html`) | The join can only afford 24 samples behind a 500 ms deadline. A page whose first half-second was throttled — backgrounded, or behind a layout storm — ships a rate that is far too low, and a refinement that only ever wrote `joinData` was never applied at all on a stationary tablet, where nothing else reconfigures. Re-reading the promise the join already consumed could only ever hand back the number already in `joinData`, so the refinement was structurally inert. | a throttled first second means the host display runs at the wrong rate for the whole session | a better measurement, taken while the page is foregrounded and painting, applied about a second in and never again | UNVERIFIED | yes | One-shot, because `onStreamLive` runs again after every reconfigure — without the latch the correction would reconfigure in a loop. |
| 177 | client / audio | `detectHasEarpiece` reuses `detectIOS()` and separates phone from tablet by the screen's short edge instead of testing the User-Agent for `iPhone` (`static/audio.js`) | It was the one capability gate in the file that was a raw User-Agent sniff, and an iPhone with "Request Desktop Website" enabled sends a Macintosh UA -- losing the Speaker/Earpiece pair, which is the only output control iOS has below 18.4 where `HTMLMediaElement.setSinkId` arrives. | a desktop-UA iPhone shows an empty output list and reports `supported: false` | it keeps the category pair | UNVERIFIED | yes | No capability says "this device has an earpiece" -- `navigator.audioSession` exists on iPad too -- so some device-shape inference is unavoidable. What was avoidable was using the weakest one available. |
| 207 | webrtc / packetize | `streamer/packetizer.rs` replaces `rtp::codecs::h264::H264Payloader`: SPS and PPS are sent as separate packets (FU-A fragmented if either needs it) when the STAP-A aggregate exceeds the MTU; a fragmented NAL gets one allocation carved with `split_to` instead of one `BytesMut` per fragment; a NAL that fits is a zero-copy `Bytes::slice` of the frame; the payload vector is reused across frames. | The stock payloader drops an oversized STAP-A with no error and clears the stashed copies anyway, so they are never retried -- with an infinite GOP that is a permanent black screen with RTP flowing and every host-side signal reporting success. Separately it allocated once per FU-A fragment, ~260 for a 300 KB IDR, per viewer, per frame. | stock payloader: **9355 ns/frame, 36398 packets, 42568 allocations** (best of 7 interleaved rounds) | local packetizer: **9610 ns/frame, 36398 packets, 4034 allocations** -- **0.97x** wall time, **10.6x fewer allocations** | VERIFIED (`cargo test --lib packetizer::bench -- --ignored`, release, 2000 frames at a 1188 B MTU, one 300 KB IDR per 59 16 KB P-frames, 7 interleaved rounds reported as the minimum per implementation; both packetizers asserted to produce the same packet count and the allocation ratio asserted at >4x). | yes | **There is no speed win, and that is the honest number** -- at 0.97x the two are within measurement noise of each other, because the memcpy of the frame body dominates and both pay it. The first attempt was **1.65x slower** (17818 vs 10812 ns/frame): the scanner was a start-code predicate called at every offset, reading up to four bounds-checked bytes per byte of frame, and on a 16 KB P-frame the scan is the entire cost. Rewriting it as one pass counting consecutive zeros -- the shape the stock payloader always had -- recovered it. What the rewrite buys is the correctness fix plus allocator traffic: a 300 KB IDR stops putting ~260 allocations into one tight loop on a tokio worker that is also driving socket writes, multiplied by every connected viewer. Resolves `major-webrtc-sps-pps-silently-dropped-when-stap-a-exceeds-mtu` and `minor-webrtc-fua-fragmentation-allocates-and-copies-every-frame-per-viewer`. |
| 179 | webrtc / codec | `streamer/negotiate.rs`: the offer is parsed for `profile-level-id` and `packetization-mode`, High > Main > Baseline is chosen from what the client advertised, and the result is written into `cfg.h264_profile` **before** `start_on_monitor` so the encoder is built for it. The answer's level is derived from the negotiated geometry (Annex A `MaxFS`/`MaxMBPS`) and applied by rewriting the answer SDP. | `Config::default()` set `H264Profile::Baseline` and nothing outside the developer probe harness ever wrote the field, so every session paid CAVLC instead of CABAC and no 8x8 transform -- typically 5-15 % of the bitrate at equal quality, and screen content with flat regions and sharp text is exactly where the 8x8 transform helps most. On a constrained link bytes are queueing delay. | every session Baseline; `profile-level-id` always ending `1f` (level 3.1 = 1280x720@30) regardless of what was being sent | a Chrome-shaped offer negotiates **High**; a 1080p60 session advertises **level 4.2**, 1440p60 **5.1**, 4K60 **5.2** | COMPILE-CHECKED and unit-tested (9 tests in `negotiate.rs`); the bitrate saving is **not** measured here -- it needs an A/B at fixed quality with a real client | yes | The level had to be applied by rewriting the answer: webrtc-rs answers with the *remote* codec parameters (`update_from_remote_description` pushes the offer's `RTCRtpCodecParameters` into `negotiated_video_codecs`, and `get_codecs_by_kind` hands those straight back), so a level registered in the media engine never reaches the wire. Only done when the offer carries `level-asymmetry-allowed=1`, which RFC 6184 8.2.2 makes the peer's explicit permission. Baseline is registered alongside the choice as a floor. Encoder-side capability probing (`NV_ENC_CAPS_SUPPORT_CABAC`, `SUPPORT_ADAPTIVE_TRANSFORM`) is W3b's half and is the remaining risk on old hardware. **The level rewrite reaches the client's decoder, not just its SDP**: `index.html::negotiatedProfileLevelId` reads the answer's `profile-level-id` and the worker configures `VideoDecoder` with `avc1.<that>` (`transform-worker.js:315`), so a 1080p session used to configure its decoder for level 3.1 — 1280x720@30 — while being fed 1080p. It now matches, and an unsupported string still falls through the whole `CODEC_CANDIDATES` list (`transform-worker.js:584`), so the failure mode is unchanged. Resolves `major-webrtc-h264-profile-hardcoded-baseline-never-negotiated`. |
| 180 | webrtc / sockets | `streamer/sockets.rs` plus a per-session muxed ICE socket on the LAN tier: `SO_SNDBUF`/`SO_RCVBUF` at 1 MB, Windows `SIO_UDP_CONNRESET` disabled, macOS `SO_NET_SERVICE_TYPE = NET_SERVICE_TYPE_VI` plus `IP_TOS` AF41, and Windows qWAVE `QOSAddSocketToFlow(QOSTrafficTypeAudioVideo)` once the candidate pair is nominated. | A grep for `CONNRESET`, `SO_SNDBUF`, `IP_TOS` or `socket2` over `webrtc-ice-0.17.1` and `webrtc-util-0.17.1` returns nothing: ICE sockets are created with OS defaults and never touched. On Wi-Fi that queues video in the AP's best-effort class alongside every background download, and on Windows one ICMP port-unreachable can fail the next `recv_from`. | OS defaults on every socket; no DSCP marking anywhere | 1 MB buffers and `SIO_UDP_CONNRESET=off` on every LAN session, and a qWAVE flow per nominated pair | COMPILE-CHECKED and unit-tested (the options are asserted to apply on a bound socket); the **effect** is UNVERIFIED -- separating AC_VI from AC_BE needs two stations on one WMM-honouring AP and a monitor-mode capture, which is not available here | yes | Gated to `SessionLocality::SameNetwork` because `UDPNetwork::Muxed` skips server-reflexive gathering outright (`agent_gather.rs`, `UDPNetwork::Muxed(_) => continue`) -- and a same-subnet peer is already offered no STUN server, so nothing is lost. Relay gathering builds its own sockets and is unaffected. **Windows deliberately does not `setsockopt(IP_TOS)`**: it is ignored unless `DisableUserTOSSetting` is cleared in the registry, which this app has no business doing to a user's machine. `QOSSetFlow(QOSSetOutgoingDSCPValue)` needs Administrators; the traffic-type route does not, which is why it is the one used. Resolves `major-webrtc-ice-sockets-get-no-socket-options`. |
| 181 | session / join | The WHEP handshake and the display+pipeline work run under one `tokio::join!`. `handle_whep_offer` splits into `prepare_whep` (peer connection, tracks, audio m-lines, `set_remote_description`, `create_answer`, ICE gathering) and `activate_whep` (everything that needs a running pipeline). | Nothing in the handshake needs the pipeline until the RTP writer is spawned: the tracks, the SDP and the gather depend only on the offer, the negotiated codec and the ICE server list. Registering the pipeline-dependent handlers after the answer exists is safe because the answer is not returned until `activate_whep` finishes, and the peer cannot connect before it has one. | serial: `display=706-1299ms pipeline=713-1018ms handshake=13-20ms` (six 20 s self-test runs, Win 11 26200 / RTX 3080 Ti, loopback, 1280x720) | the handshake term leaves time-to-first-frame entirely; on LAN that is 13-20 ms of ~1600 ms (~1 %) | COMPILE-CHECKED (300 lib tests pass, and the in-process WHEP self-test exercises the whole restructured join end to end); the relayed case is UNVERIFIED -- neither dev box has a TURN path | yes | The LAN win is ~1 % and does not on its own justify the restructure; the case that does is the relayed one, where the host gather runs to its 1500 ms cap against a 1500-2100 ms display term, i.e. most of the gather. Both halves unwind independently: a handshake failure stops the capture and removes the display, a display failure calls `PendingWhep::abandon`, which closes the peer connection and the mux read worker instead of leaking them. Resolves `major-session-join-is-a-fully-serial-multi-second-chain` (item 2) and `minor-webrtc-handshake-does-not-overlap-display-creation`. |
| 182 | session / robustness | `streamer/lock.rs`: a `LockExt::lock_recover()` that recovers a poisoned `std::sync::Mutex` instead of unwrapping, at all 56 shared-state sites in `session.rs`, `server.rs`, `webrtc_session.rs` and `cloud.rs`, logging the first recovery once. | Every one of those locks guards the same few `Arc<Mutex<..>>` values, shared across every adapter's server and the cloud relay thread, so one panic that unwinds while any guard is live poisons the host for **all** devices. axum catches panics per task, so the symptom is not a crash but every request returning 500 and the device list going inert, with nothing in the log to explain it. | one panic in a guarded scope = every later `/whep`, `/reconfig`, `/leave` and UI command panics, permanently, until restart | the guard is recovered, one `teprintln!` names it, and the host keeps serving | VERIFIED (unit test: a thread panics holding a lock, `lock().is_err()` confirms the poisoning, `lock_recover()` returns the data and the lock stays usable) | yes | Recovering is correct here: the guarded data is plain maps, counters and epochs, and a torn update is far less harmful than a dead host. Resolves `minor-session-mutex-poisoning-cascades-across-every-session`. |
| 183 | bwe | The bitrate driver holds its loss and RTT arms when the RTCP receiver report has not moved since the last 120 ms poll, instead of re-running the controller on the same numbers. | `fraction_lost` and `round_trip_time` come from the remote's receiver reports, which browsers emit about once a second; the loop runs at 8 Hz. Seven polls in eight re-read the same two values, and feeding an unchanged reading back in lets one loss event be acted on up to eight times -- the mechanism behind the ratcheting the cut rate-limit was added to stop. | up to 8 controller actions per independent loss observation | one; transport-wide feedback is unaffected because it is consumed on read, so a `Some` there is always new | COMPILE-CHECKED | yes | A run of 16 unchanged polls now logs once, so a real client's RR cadence is observable rather than assumed. The poll stays at 120 ms because `bytes_sent` genuinely moves at that rate and `measured_send_bps` is derived from it. Resolves `speculative-webrtc-get_stats-polled-8-times-a-second-per-session`. |
| 184 | session / http | The client page is substituted for `__SAME_DEVICE_FLAG__` once per variant into a `OnceLock` at first use, instead of `String::replace` over the largest asset in the tree on every `GET /`. | Every other static route already serves a `&'static str` with no copy. The page is fetched once per join so this was never large, but it is on the join path and a client in a reload loop paid it repeatedly. | one full scan and one allocation of the whole page per request | one per variant, for the life of the process | COMPILE-CHECKED | yes | Recorded as clarity rather than sold as a performance fix, which is what the finding itself asked for. Resolves `speculative-session-index-html-reallocated-on-every-page-load`. |
| 185 | cloud | The relay's "session taken" condition is a typed `RegisterRejected` error read back with `downcast_ref`, not `e.to_string().contains(SESSION_TAKEN)`. | The control flow depended on string formatting that nothing enforced: any future `format!` that echoed a relay message into an error would rotate a working join code out from under the user, and the code on screen would silently stop matching the one the client has. | a substring match on a formatted message | a typed condition, with a unit test that an unrelated error whose text contains the same substring does **not** rotate | VERIFIED (3 unit tests) | yes | The backoff-reset asymmetry around it is deliberate and is preserved. Resolves `minor-cloud-session-taken-is-a-substring-match`. |
| 186 | session / display | `VirtualDisplayController::created_geometry` (default `None`) lets a backend report the size it actually landed on, and the server skips the forced `set_display_mode` and its settle poll when that already matches the request. | `create_display` returned a display id and nothing else, so the server always followed a create with a forced mode set -- an extra reconfiguration on a path that had already done several, and one guaranteed to fail on a backend that negotiates its geometry rather than being commanded. The macOS AirPlay tier computes the closest mode from the ladder macOS published and then discards the answer. | one `set_display_mode` plus one `wait_for_display_settle` on every new-display join, on every backend | skipped on a backend that reports a matching geometry | COMPILE-CHECKED and unit-tested (4 tests, including that a portrait request is compared in monitor coordinates and that a commanded backend keeps the old behaviour) | yes | `None` is deliberately still the answer on Windows: the IDD registers a mode list and the OS picks from it, so the backend genuinely does not know which. The tier that benefits is macOS AirPlay, which no machine here can run. Resolves the server-side half of `minor-session-create-display-cannot-report-the-geometry-it-landed-on`. |
| 187 | session / ice | `stunUrls` becomes a real setting: a `SharedStunConfig` in `AppState` read from the config store at `setup` (so the desktop app, `serve` and the CLI all see one value), a `ScreenExtend stun show|set|clear` verb, and a `stunUrls` entry in the frontend config type defaulting to today's URL. | `DEFAULT_STUN_URL` was a compile-time constant and the only writer outside `Config::default` was `--stun` in the developer probe harness, which is not wired into the shipping binary. A same-subnet peer is no longer offered STUN at all, so the common case costs nothing -- but a cross-network or unclassified join still contacted a Google endpoint with no setting and no way to point it at a private server. | `stun:stun.l.google.com:19302`, always, unchangeable | configurable; empty means no STUN at all, which is a legitimate setting for a LAN-only host | COMPILE-CHECKED | yes | Read through the store rather than a new Tauri command on purpose: it needs no `bindings.ts` regeneration and keeps one source of truth with the CLI, which already reads and writes the same file. Resolves `minor-session-stun-list-has-no-configuration-surface`. |
| 189 | net (macOS) | `watch_for_network_changes` blocks on the kernel routing socket (`PF_ROUTE`, which carries `RTM_NEWADDR`/`RTM_DELADDR`/`RTM_IFINFO`) with a 250 ms debounce and a 30 s backstop, instead of re-running the whole enumeration once a second; a second call no longer starts a second identical watcher thread (`macos_utils/networking.rs`) | Every second the watcher ran `getifaddrs`, a `SIOCGIFMEDIA` ioctl per interface, `if_nametoindex`, an `is_hosted_network` CoreWLAN call and a `wifi_ssid` CoreWLAN call — two XPC round trips to `airportd` per poll — and compared the result to the previous one. Nothing about that work is needed when nothing has changed, and the 1 s granularity is also 1 s of dead time before `sync_streamers` can rebind after a Wi-Fi switch. | **30 full enumerations per 30 s at idle** (`macos_utils::networking::tests::measure_adapter_watch_cost`, 10.15.8, one reported adapter) | **0 per 30 s at idle**; a real change wakes the thread immediately instead of up to 1 s later | VERIFIED (10.15.8) | yes | `PF_ROUTE` predates macOS and needs no framework, no run loop and no entitlement, which is why it was chosen over `SCDynamicStoreSetNotificationKeys` — the latter needs a CFRunLoop thread and a SystemConfiguration link for the same events. The socket's payload is never parsed for meaning: the enumeration remains the source of truth and the socket only decides when to run it. |
| 190 | virtual display (macOS) | `activate_mode`'s settle wait is a condvar fed by `CGDisplayRegisterReconfigurationCallback`, registered from a thread that runs a CFRunLoop, instead of a 5 ms/25 ms sleep-poll (`macos_utils/virtual_display.rs`) | The callback fires exactly when WindowServer finishes reconfiguring a display, so the wait can report the true settle time instead of rounding it up to the next poll. The callback is delivered on the *registering thread's* run loop, and the join path is a `spawn_blocking` worker with none — hence the dedicated thread. | poll: 5 ms for the first 250 ms, then 25 ms, inside `DISPLAY_CORRELATION_LOCK` | event-driven, with a 100 ms re-read as the ceiling on a missed event | VERIFIED (registration); UNVERIFIED (the saving) | yes | A generation counter is read *before* the geometry read, so a reconfiguration that lands between the two cannot be lost. Correctness never depends on the callback: the wait re-reads on a bounded timeout either way, so a machine where it never fires is no slower than the poll it replaced. `the_reconfiguration_watcher_reports_whether_it_registered` asserts the probe answers and caches. |
| 191 | virtual display (macOS) | The descriptor's `setSerialNum:` is FNV-1a over the display's name (`"ScreenExtend - <device>"`) instead of a process-local counter starting at 1; the registry's termination-handler key is a separate process-unique token (`macos_utils/virtual_display.rs`) | macOS keys per-display state — arrangement position, colour profile, resolution preference — on vendor/product/serial. Vendor and product are the same constants for every display, so the serial was the whole identity, and it restarted at 1 on every launch: the first device to join after a restart inherited whatever the previous run's first device had left behind, and a returning device never got its own back. | serial 1, 2, 3… per process; the same serial for different devices across restarts | the same device name always maps to the same serial, and two different names never collide | COMPILE-CHECKED | yes | The termination handler needs to name exactly one registry entry and two clients can carry the same device name, so it keeps a counter of its own. The declared physical size and the placeholder vendor/product ID were *not* changed — see the rejection below. |
| 192 | net | `Config.bind_ip` widened from `Ipv4Addr` to `IpAddr`; macOS `sync_streamers` serves global-unicast and unique-local IPv6 alongside IPv4 (v4 first); `url_host` brackets IPv6 literals per RFC 3986; link-local is reported rather than silently dropped (`streamer/config.rs`, `streamer/server.rs`, `macos_utils/mod.rs`, `windows_utils/mod.rs`, `cli.rs`) | On an IPv6-only network every address the enumeration found failed `parse::<Ipv4Addr>()` and was dropped with `.ok()`, so `desired` was empty, no axum server was ever started, and the dashboard showed addresses nothing was listening on. | IPv6-only network: zero listeners, zero diagnostics | the v6 addresses a browser can reach are bound and served; the ones it cannot (`fe80::/10`, which needs a zone index no URL carries) are named in the log with the reason | COMPILE-CHECKED (macOS); COMPILE-CHECKED-by-inspection (Windows, one line) | yes | The dev box's only v6 address is `fe80::844:907:64ca:8224`, i.e. exactly the case that still cannot be served, so an end-to-end v6 join was not reachable here. `collect_local_ips` already parsed `IpAddr`, so the WHEP locality detection needed no change. The desktop UI's QR builder is still v4-only — see the UI-visible recommendations. |
| 193 | hosted network (macOS) | The liveness watcher promotes the status when a session appears whose client IP shares the /24 the AP handed out, and after 60 s with nothing associated logs the three usual causes in order (`macos_utils/hosted_network.rs`, plus `streamer/session.rs::client_ips`) | "Hosted network started" has only ever been an assertion about our own API call. An AP that is up, holds an address, and is refused by every phone in the room looked identical in the log to one that works. | AP up with nothing associated: indistinguishable from a working AP | promotion on the first client from our own subnet; a named diagnostic at 60 s otherwise | COMPILE-CHECKED | yes | `HostedNetworkStatus.associated_peer` is the field that separates advertised from verified. The `NotStarted`/`Advertised`/`Verified` tri-state in `AppState` itself is deliberately not done — see the UI-visible recommendations. |
| 194 | airplay (macOS) | Type-`0x05` stream reports are parsed (`binary_plist_len` locates the plist inside the payload) and their numeric fields logged; the header capture stamp is read as a sender-epoch NTP fixed point and reported as spread rather than as an absolute latency (`macos_utils/airplay/mirror.rs`) | Both are plaintext, both already arrive, and both were counted and dropped. The stream report is macOS's own view of the link; the stamp is the only handle anything in this tree has on WindowServer's compose-to-consumer path. | 5 of 128 header bytes read; the whole 25 KiB report discarded | `encoderCurrentFPS`, `rttAvg`, `lossAvg`, `queuedFramesAvg`, `txCapacityAvg`, `txUsageAvg`, `sentFramesAvg` logged; compose-to-consumer spread p50 **26.96 ms** / p90 **43.90 ms** / p99 **67.43 ms** over 66 frames | VERIFIED (10.15.8) | yes | The report did not parse before because it is a ~181-byte plist followed by a 25,000-byte trailer; a binary plist's 32-byte trailer is self-describing, so the one end position whose trailer is internally consistent is the real one. See the close-out section for what the stamp turned out to be and why only the spread is reported. |
| 195 | net (macOS) | The watcher reports the associated Wi-Fi channel and whether `awdl0` is up, once, at start; a channel in {6, 44, 149} is called out by name (`macos_utils/networking.rs`) | AWDL's three social channels are where every nearby Apple device parks its radio for AirDrop and Handoff discovery, and a single-radio Mac retunes to them periodically — a published jitter source that the *user* can act on and that will otherwise be blamed on the stream. | the channel and `awdl0`'s state were never recorded, so no field report could distinguish this cause | one line at startup; on this box, "Wi-Fi channel 36, awdl0 up" | VERIFIED (10.15.8: channel 36, `awdl0` up, so the non-social branch fires) | yes | Reported, not acted on: the host AP's own channel choice already avoids all three (row from W6's earlier work), and whether *sitting* on a social channel is better or worse for jitter is the open A/B in `docs/manual-tests/macos-hosted-network.md` §4. |
| 204 | whep / ice | Trickle ICE, host half: `POST /whep` returns `Location: /whep/{id}`, `PATCH /whep/{id}` takes an `application/trickle-ice-sdpfrag` body and feeds each candidate to the live peer connection, `DELETE /whep/{id}` tears the resource down, and `/net-config` advertises `trickleIce: true`. | `/whep` was a single POST carrying the complete offer, so the client could not start negotiating and add candidates later: the offer it posted was the only one the host would ever see. The client therefore could not post until its own gathering finished -- with TURN configured that is a round trip to the relay before the offer even leaves, and on a network where no candidate arrives it is the full deadline. | no trickle verb; the client's gather is on the critical path of every join | the host accepts candidates after the answer; the session is addressable by a minted id held as a `Weak`, so the registry can never keep a peer connection alive | COMPILE-CHECKED and unit-tested (5 tests: mid and m-line index, a second `m=` section, `a=end-of-candidates`, a bare `candidate:` prefix, and a fragment for a session that is over). The end-to-end effect is UNVERIFIED until the client adopts it. | yes | Backward compatible in both directions on purpose: a client that ignores `Location` behaves exactly as today, and the cloud relay never sets it because it has no URL space of its own -- so a relayed client correctly keeps the gather-then-post shape. The client half is `findings/major-client-can-now-trickle-ice-and-stop-waiting-to-gather.md`, tagged W11, with the wire contract written out. Resolves the host half of `major-whep-no-trickle-ice`. |
| 205 | session / transport | `platform::link_medium_for_local_ip` resolves the local end of the nominated ICE pair to `wired` / `wireless` / `other` and logs it once per session. Windows reads `MSFT_NetAdapter.NdisPhysicalMedium` (14 = 802.3, 9/1 = 802.11); macOS uses `getifaddrs` for the interface name and then `SIOCGIFMEDIA`. | `detect_session_locality` classified a session only as SameNetwork / CrossNetwork / Unknown, so the host could not tell a USB-tethered phone -- which is an ordinary Ethernet interface, and which every competitor in this category treats as its flagship low-latency tier -- from Wi-Fi. Luna Display's own 50-sample bench reports 11.3 ms mean over USB against 22.4 ms over Wi-Fi. | the transport the pair actually rides was invisible | named in the log, off a `spawn_blocking`, once per session | COMPILE-CHECKED. The wired-vs-wireless **latency comparison** is UNVERIFIED and needs a 240 fps camera and an Android phone -- the script is `docs/manual-tests/transport.md` §1, and "not measurably better than clean 5 GHz" is an outcome worth recording | yes | No policy is driven off it yet, deliberately: a tethered client is on the same subnet, so it already classifies `SameNetwork` and already gets `PacingMode::Off`. What it buys today is that the wired route is documented in `README.md` and observable in the log. Resolves `minor-session-usb-tether-is-an-undocumented-low-latency-tier`. |
| 206 | session / display | `DISPLAY_CORRELATION_LOCK` is released as soon as the backend names its own display, and the wait for it is logged when it exceeds 1 ms. | The lock exists because a new virtual display is correlated with its OS device name by diffing `monitor_device_names()` before and after, and two concurrent creations would confuse the diff. But on the fast path -- where `display_device_name(id)` answers directly -- the `before` snapshot is never read, and the lock was still held for the whole create-and-attach window. Two devices joining together serialised: the second waited out the first's entire window, up to `DISPLAY_ATTACH_TIMEOUT` = 5 s, before its own began. | the lock spans create + name + up to a 5 s attach poll, on every backend | released immediately on the direct-name path; the wait behind another join is now a log line rather than an inference | COMPILE-CHECKED | yes | W9 landed the Windows half in parallel (row 196): `WindowsVirtualDisplay::display_device_name` now correlates through `QueryDisplayConfig`, returning `None` on an ambiguous match — the EDID monitor-name descriptor is 13 bytes, so every `ScreenExtend - <device>` truncates to a shared prefix — and dropping to the diff when it cannot be sure. The two changes compose: the direct-name path is now reachable on Windows, and this is what makes the lock go away when it succeeds. Resolves the server-side half of `minor-session-display-correlation-lock-serializes-all-joins-for-up-to-5s`. |
| 215 | input | The 50 ms age gate is source-aware: a stale `POINTER_MOVE_BATCH` or `POINTER_MOVE` from a mouse is still dropped, one from a pen or touch never is. | Only where a cursor *is* is observable, so a 50 ms-old position is worse than nothing. A pen or touch sample is not redundant with its neighbours -- it is the shape of the line -- so dropping one leaves a permanent straight segment where the user drew a curve, and no later event repairs it. | every supersedable event dropped for age regardless of `source` | mouse unchanged; pen/touch batches and moves are never age-dropped (`ink_is_never_dropped_for_age_but_a_cursor_is`) | VERIFIED (unit); UNVERIFIED (no pen client on this hardware) | yes | The gate has never fired in practice on either dev box. If a pen backlog ever does become real, the right answer is to inject the batch and skip its *intermediate* samples, not discard the message -- the endpoints are what the stroke needs. |
| 208 | input | `protocol::parse` returns `Result<Parsed, ParseError>` naming opcode, needed length and reason; the transport logs one line per distinct rejection per channel and counts every one in `input_parse_errors`. `Phase::from_opcode` returns `Option` instead of falling through to `Move`. | Every failure mode -- unknown opcode, truncation, bad UTF-8, a length prefix past the end -- collapsed to one `None` with no log, no counter and no reply. A client whose cached `input.js` had drifted from the host produced a session where input silently did nothing, and nothing anywhere explained why. | 0 diagnostics of any kind | one log line per distinct error per channel, `input_parse_errors` in `status --json`, five typed variants asserted by `every_rejection_names_its_reason` | VERIFIED (unit) | yes | The dedup is a `HashSet<ParseError>` in the handler state, so a drifted client flooding the same malformed frame produces one line, not thousands. |
| 209 | input | Pointer, batch, wheel and mouse-delta frames may carry a `u16` sequence in the optional tail past the client stamp; the injector drops a *supersedable* fast event whose sequence is behind the highest injected (wrapping-aware) and counts it in `input_reorder_drops`. | The fast channel is unordered with `maxRetransmits: 0` and the host merges two queues with a `crossbeam::select!` that picks randomly, so two moves can reach the injector swapped and the older one walks the cursor backwards for a frame. The age gate bounds how *stale* a replayed move is and says nothing about order. | no sequence anywhere in the input protocol | 2 bytes on the frames that need it, one wrapping comparison; an absent tail means the session is simply not reorder-gated | VERIFIED (unit: `the_fast_channel_sequence_number_is_optional_on_every_frame_that_carries_one`, `sequence_comparison_survives_the_u16_wrap`); UNVERIFIED (no induced-reorder link) | yes | Host-side only and backward compatible by construction. The client half is W11's -- see the return note for the exact layout. Buttons and keys are never gated, only positional data. |
| 210 | input (Windows) | Synthetic pen events carry the client's own `pointerId` instead of the literal `1`. | Each session builds its own `Injector` and therefore its own `CreateSyntheticPointerDevice(PT_PEN, 1, ...)`, and both injected `pointerId: 1`. Whether Windows disambiguates two synthetic pen contacts sharing an id across two device handles is undocumented. | every pen contact on every session injected as pointer 1 | the client's id, guarded so 0 becomes 1 (Windows treats 0 as no pointer); the touch path has always done this | COMPILE-CHECKED; UNVERIFIED (needs two pen-capable clients) | yes | Same shape as the touch path, which keys its `contacts` map on the client's id and passes it straight through. |
| 211 | telemetry | The summary line reports delivered frames per second between consecutive summaries, not only a running total. | A capture path silently delivering 45 fps against a 60 fps target looked identical in the log to one keeping up. | `frames=<running total>` | `frames=<total> fps=<rate>` | VERIFIED (probe run) | yes | Computed from the frame counter and the host clock at each summary, so it costs one atomic load per 5 s. |
| 212 | capture (Windows) | DXGI `AccumulatedFrames` past the first is counted into `frames_dropped_capture`; `ProtectedContentMaskedOut` is reported once per capture. | `AccumulatedFrames` is documented as the number of frames the OS accumulated since the application processed the last desktop image, so everything past the first is a present the client never saw -- free drop telemetry, read only as a `> 1` early-out inside the dirty-region check. | both fields read and discarded | `frames_dropped_capture` in `status --json` and the summary line; a DRM black rectangle now has an explanation in the log | VERIFIED (probe run, counter reachable) | yes | The two W2 findings asking for a per-stage breakdown were already resolved by the merged W1/W3a telemetry (`FrameTiming`, `Stages`, p50/p90/p99/max histograms, all three Windows paths including the NVENC ring, plus packetize and socket_write in the transport). These were the two gaps that remained. |
| 213 | audio | `AudioParams` carries the session's `SessionLocality`, and a `CrossNetwork` session starts with `FLAG_REDUNDANT` on rather than waiting for the video BWE to observe loss. | The redundancy policy was driven only off `fraction_lost`, which is the right signal but a lagging one: it can only move after loss has already happened. A relayed session is *known* to be relayed before the first packet is sent -- `AppState::locality_for` decides it from the peer IP before the handshake. | a relayed client's opening packets were unprotected until the first observed loss | defensive from packet one on `CrossNetwork`; `SameNetwork` and `Unknown` unchanged | VERIFIED (unit: `a_relayed_session_starts_defensive_and_a_lan_one_does_not`); UNVERIFIED (no relayed link on this hardware) | yes | The client's declared capability still outranks everything: one that cannot parse the format is never sent it, whatever the link is doing. |
| 214 | audio | `OpusApi::version()`'s null-pointer fallback is no longer a string the library itself can return. | The log had read `opus: libopus unknown` for months and the code's own fallback was `"unknown"`, so the two were indistinguishable and the finding assumed a null pointer. | ambiguous | measured: the Windows DLL genuinely answers `libopus unknown` (upstream's own fallback for a build with no `package_version`, which a shallow checkout does not generate) and the macOS dylib answers `libopus 1.5.x-screenextend-src` | VERIFIED (both bundled artifacts probed directly) | yes | `PROVENANCE.md` claimed the dylib reported `libopus unknown` as well. It does not. Two platforms reporting different strings for one source tree is exactly why the host must print what the library says rather than a fallback the library can also produce. |
| 196 | vdisplay (Windows) | `WindowsVirtualDisplay::display_device_name` correlates a new display to its GDI device name with `QueryDisplayConfig` instead of leaving the trait default of `None`. | The default sent every join down `wait_for_new_monitor`: a 100 ms poll of `EnumDisplayDevices` diffed against a pre-create snapshot, serialised across concurrent joins by `DISPLAY_CORRELATION_LOCK` and bounded only by the 5 s attach timeout -- all of it between the device scanning the QR and a picture appearing. | 100 ms-granularity poll on every join, always | a direct lookup when the monitor's EDID friendly name is unambiguous; `None`, and therefore the old diff, when it is not | COMPILE-CHECKED + unit (`an_ambiguous_friendly_name_resolves_to_nothing`); UNVERIFIED (the `report_display_name_correlation` harness was not run against a full ladder) | yes | The match is by prefix because the EDID monitor-name descriptor is 13 bytes, so `ScreenExtend - iPad` comes back truncated. Ambiguity returns `None` deliberately: a wrong device name points the capture at somebody else's screen, while `None` costs only the poll we already had. |
| 197 | net (Windows) | The legacy hosted-network capability is `WlanQueryInterface(wlan_intf_opcode_hosted_network_capable)` and its status is `WlanHostedNetworkQueryStatus`, not `netsh` output. | Both were decided by string-matching English phrases in localized `netsh` output, so on a German, French, Japanese or Chinese Windows the probe returned no unconditionally and a successfully started hosted network read back as stopped. | locale-dependent | structured `BOOL` and `WLAN_HOSTED_NETWORK_STATE`; measured on Windows 11 26200: `hosted_network_capable => Some(false)`, `active => Some(false)`, matching what `netsh` prints in English on the same box | VERIFIED (`report_wlan_hosted_network_capability`) | yes | The finding's earlier measurement found `WlanHostedNetworkQueryProperty`/`QueryStatus` ambiguous for *capability*; the interface opcode is the one that is not, and it had not been tried. The `netsh` parse survives only for a machine where the WLAN service declines to answer, and says so in the log. |
| 198 | vdisplay (Windows) | `DriverClient` resynchronises with `request_state()` when the driver event stream reports a gap, instead of only logging it. A `.v2` pipe name is preferred at connect, so a rebuilt driver can be recognised without ever sending an old one a message it cannot decode. | The cached monitor list is fed by that stream and `refresh_state` reads the same cache, so a dropped `Changed` left `monitors()` disagreeing with the driver with no path to recovery. The wire format is `#[serde(untagged)]` with no version field, but changing it means changing a format shared with a signed third-party binary. | log line, cache stays wrong | `EventOrLag` on the stream and the cache repaired from the driver; `PROTOCOL_VERSION` negotiated by which pipe exists | VERIFIED (unit: `a_lagging_consumer_is_told_how_much_it_missed`, `an_unversioned_driver_still_connects_and_reports_v1`) | yes | Neither half is reachable today -- the driver is the only writer and 64 is more than any burst we generate -- but both are silent-corruption shapes in a component whose symptom is that the virtual display quietly did not appear. |
| 199 | vdisplay (Windows) | Driver round trips are serialised by their own lock; the state mutex is taken only for the moments that touch its fields. | `Inner::commit` held `WindowsVirtualDisplay::inner` across `RUNTIME.block_on(notify())` -- bounded at 5 s connect plus 2 s write, but bounded is not short -- so a slow driver stalled every *reader* of the monitor list behind it, including the display-name lookup now on the join path. | one blocking mutex across the whole round trip | a `commit` lock for the round trip and `inner` for the field access; id allocation stays atomic because `commit` is held across it | COMPILE-CHECKED; UNVERIFIED (needs a deliberately wedged driver) | yes | `DriverClient::duplicate()` gives an independent client over the same connection, which is what lets the state guard go before the wait. |
| 200 | probe | `probe::finish` removes the virtual display its `platform::setup` created, on normal exit, on Ctrl+C and from a panic hook. | A failed probe left a display attached and the *next* process saw two monitors and failed with a `GraphicsCaptureItem` conversion error; a minute later the same command worked. One bad run silently changing the next run's numbers is the worst failure a measurement harness can have. | a leftover display poisoned the following run | verified: a probe run followed immediately by `probe --capture` reports one display | VERIFIED (normal and error exit); COMPILE-CHECKED (Ctrl+C, panic hook) | yes | Cleanup is idempotent -- it takes the controller out of a static -- so the handlers can race the normal exit. |
| 201 | probe | `probe --live --workload <w>` paints the generated frames into a borderless top-most window covering the captured display, so a scripted workload goes through the real capture path. | `--workload` implied `--synthetic-pattern`, which fed the buffer straight to the encoder: no duplicator, no frame pool, no GPU texture, no scaler. The four workloads -- whose whole point is that different content stresses capture and encode differently -- measured only the encode half, and `capture_delivery`/`scale` were empty by construction. | `capture_delivery` and `scale` empty on every workload run | `--live --workload motion --duration 12` at 1920x1080: 817 frames, `capture_delivery` p50/p90/p99 **7.34 / 18.87 / 31.46 ms**, `scale` **0.15 / 0.21 / 0.33 ms**, WGC plus the NVENC zero-copy ring | VERIFIED (Windows dev box) | yes | The `--counter` overlay rides the same window, so the 240 fps camera method in `docs/BASELINE.md` can now be run off the real display instead of the synthetic buffer. |
| 202 | capture (macOS) | `CGDisplayStreamUpdateGetDropCount` is read on every delivery into an `AtomicU64`, reported at most once a second from a thread of its own, and printed by `probe_main_display`. | `CGDisplayStream.h` documents this counter as useful for determining whether the client is keeping up with all WindowServer updates -- exactly the question three open findings about `queueDepth`, `minimumFrameTime` and WindowServer contention were each guessing at. The update reference was already in hand on every frame and used only to count dirty rectangles. | not read | `drops_captured()` next to `frames_captured()`; one log line a second, only when the number moved | COMPILE-CHECKED (Mac `cargo check`); UNVERIFIED (needs a 30 s animated-screen run on the 10.15.8 box) | yes | Read *before* the dirty-rect early-out: a delivery with no dirty rectangles can still be reporting frames dropped on the way to it. The delivery block is a real-time `UserInteractive` dispatch callback, so it does one relaxed `fetch_add` and nothing else. |
| 203 | input / compat | `CompatibilityReport` gains `input_backend`, filled on all three platforms from `streamer::input::probe_backend()`; on macOS the tier is re-probed on the injector's existing 500 ms geometry cadence and a missing grant joins `unsupported_apis`. The injector backends' `log::` diagnostics move to the log bus. | Without Accessibility, `CGEventPost` returns `void` and the OS drops every event with no error the process can observe: a client whose remote control is enabled, whose pointer events are being sent and acknowledged, and whose host simply never moves. Input was the only subsystem with no tier at all, and its one warning went to `log::warn!`, which does not reach the Settings terminal. | silent no-op, and a relaunch demanded | a reported tier, an actionable entry in the list the UI already renders, and injection that starts working within half a second of the permission being granted | COMPILE-CHECKED (Mac `cargo check`); UNVERIFIED (needs a Mac with Accessibility revoked) | yes | No new UI: the degraded case rides `unsupported_apis`, which the compatibility surface already renders, and `doctor` prints the tier. `bindings.ts` regenerated. |
| 220 | net (macOS) | `watch_for_network_changes` blocks on the kernel routing socket (`PF_ROUTE`, which carries `RTM_NEWADDR`/`RTM_DELADDR`/`RTM_IFINFO`) with a 250 ms debounce and a 30 s backstop, instead of re-running the whole enumeration once a second; a second call no longer starts a second identical watcher thread (`macos_utils/networking.rs`) | Every second the watcher ran `getifaddrs`, a `SIOCGIFMEDIA` ioctl per interface, `if_nametoindex`, an `is_hosted_network` CoreWLAN call and a `wifi_ssid` CoreWLAN call — two XPC round trips to `airportd` per poll — and compared the result to the previous one. Nothing about that work is needed when nothing has changed, and the 1 s granularity is also 1 s of dead time before `sync_streamers` can rebind after a Wi-Fi switch. | **30 full enumerations per 30 s at idle** (`macos_utils::networking::tests::measure_adapter_watch_cost`, 10.15.8, one reported adapter) | **0 per 30 s at idle**; a real change wakes the thread immediately instead of up to 1 s later | VERIFIED (10.15.8) | yes | `PF_ROUTE` predates macOS and needs no framework, no run loop and no entitlement, which is why it was chosen over `SCDynamicStoreSetNotificationKeys` — the latter needs a CFRunLoop thread and a SystemConfiguration link for the same events. The socket's payload is never parsed for meaning: the enumeration remains the source of truth and the socket only decides when to run it. |
| 221 | virtual display (macOS) | `activate_mode`'s settle wait is a condvar fed by `CGDisplayRegisterReconfigurationCallback`, registered from a thread that runs a CFRunLoop, instead of a 5 ms/25 ms sleep-poll (`macos_utils/virtual_display.rs`) | The callback fires exactly when WindowServer finishes reconfiguring a display, so the wait can report the true settle time instead of rounding it up to the next poll. The callback is delivered on the *registering thread's* run loop, and the join path is a `spawn_blocking` worker with none — hence the dedicated thread. | poll: 5 ms for the first 250 ms, then 25 ms, inside `DISPLAY_CORRELATION_LOCK` | event-driven, with a 100 ms re-read as the ceiling on a missed event | VERIFIED (registration); UNVERIFIED (the saving) | yes | A generation counter is read *before* the geometry read, so a reconfiguration that lands between the two cannot be lost. Correctness never depends on the callback: the wait re-reads on a bounded timeout either way, so a machine where it never fires is no slower than the poll it replaced. `the_reconfiguration_watcher_reports_whether_it_registered` asserts the probe answers and caches. |
| 222 | virtual display (macOS) | The descriptor's `setSerialNum:` is FNV-1a over the display's name (`"ScreenExtend - <device>"`) instead of a process-local counter starting at 1; the registry's termination-handler key is a separate process-unique token (`macos_utils/virtual_display.rs`) | macOS keys per-display state — arrangement position, colour profile, resolution preference — on vendor/product/serial. Vendor and product are the same constants for every display, so the serial was the whole identity, and it restarted at 1 on every launch: the first device to join after a restart inherited whatever the previous run's first device had left behind, and a returning device never got its own back. | serial 1, 2, 3… per process; the same serial for different devices across restarts | the same device name always maps to the same serial, and two different names never collide | COMPILE-CHECKED | yes | The termination handler needs to name exactly one registry entry and two clients can carry the same device name, so it keeps a counter of its own. The declared physical size and the placeholder vendor/product ID were *not* changed — see the rejection below. |
| 223 | net | `Config.bind_ip` widened from `Ipv4Addr` to `IpAddr`; macOS `sync_streamers` serves global-unicast and unique-local IPv6 alongside IPv4 (v4 first); `url_host` brackets IPv6 literals per RFC 3986; link-local is reported rather than silently dropped (`streamer/config.rs`, `streamer/server.rs`, `macos_utils/mod.rs`, `windows_utils/mod.rs`, `cli.rs`) | On an IPv6-only network every address the enumeration found failed `parse::<Ipv4Addr>()` and was dropped with `.ok()`, so `desired` was empty, no axum server was ever started, and the dashboard showed addresses nothing was listening on. | IPv6-only network: zero listeners, zero diagnostics | the v6 addresses a browser can reach are bound and served; the ones it cannot (`fe80::/10`, which needs a zone index no URL carries) are named in the log with the reason | COMPILE-CHECKED (macOS); COMPILE-CHECKED-by-inspection (Windows, one line) | yes | The dev box's only v6 address is `fe80::844:907:64ca:8224`, i.e. exactly the case that still cannot be served, so an end-to-end v6 join was not reachable here. `collect_local_ips` already parsed `IpAddr`, so the WHEP locality detection needed no change. The desktop UI's QR builder is still v4-only — see the UI-visible recommendations. |
| 224 | hosted network (macOS) | The liveness watcher promotes the status when a session appears whose client IP shares the /24 the AP handed out, and after 60 s with nothing associated logs the three usual causes in order (`macos_utils/hosted_network.rs`, plus `streamer/session.rs::client_ips`) | "Hosted network started" has only ever been an assertion about our own API call. An AP that is up, holds an address, and is refused by every phone in the room looked identical in the log to one that works. | AP up with nothing associated: indistinguishable from a working AP | promotion on the first client from our own subnet; a named diagnostic at 60 s otherwise | COMPILE-CHECKED | yes | `HostedNetworkStatus.associated_peer` is the field that separates advertised from verified. The `NotStarted`/`Advertised`/`Verified` tri-state in `AppState` itself is deliberately not done — see the UI-visible recommendations. |
| 225 | airplay (macOS) | Type-`0x05` stream reports are parsed (`binary_plist_len` locates the plist inside the payload) and their numeric fields logged; the header capture stamp is read as a sender-epoch NTP fixed point and reported as spread rather than as an absolute latency (`macos_utils/airplay/mirror.rs`) | Both are plaintext, both already arrive, and both were counted and dropped. The stream report is macOS's own view of the link; the stamp is the only handle anything in this tree has on WindowServer's compose-to-consumer path. | 5 of 128 header bytes read; the whole 25 KiB report discarded | `encoderCurrentFPS`, `rttAvg`, `lossAvg`, `queuedFramesAvg`, `txCapacityAvg`, `txUsageAvg`, `sentFramesAvg` logged; compose-to-consumer spread p50 **26.96 ms** / p90 **43.90 ms** / p99 **67.43 ms** over 66 frames | VERIFIED (10.15.8) | yes | The report did not parse before because it is a ~181-byte plist followed by a 25,000-byte trailer; a binary plist's 32-byte trailer is self-describing, so the one end position whose trailer is internally consistent is the real one. See the close-out section for what the stamp turned out to be and why only the spread is reported. |
| 226 | net (macOS) | The watcher reports the associated Wi-Fi channel and whether `awdl0` is up, once, at start; a channel in {6, 44, 149} is called out by name (`macos_utils/networking.rs`) | AWDL's three social channels are where every nearby Apple device parks its radio for AirDrop and Handoff discovery, and a single-radio Mac retunes to them periodically — a published jitter source that the *user* can act on and that will otherwise be blamed on the stream. | the channel and `awdl0`'s state were never recorded, so no field report could distinguish this cause | one line at startup; on this box, "Wi-Fi channel 36, awdl0 up" | VERIFIED (10.15.8: channel 36, `awdl0` up, so the non-social branch fires) | yes | Reported, not acted on: the host AP's own channel choice already avoids all three (row from W6's earlier work), and whether *sitting* on a social channel is better or worse for jitter is the open A/B in `docs/manual-tests/macos-hosted-network.md` §4. |
| 227 | hosted network (macOS) | `wait_for_ipv4` takes the address the interface held *before* the start and refuses it; `measure_hostap_start` is a bounded harness that runs the shipping ladder and stops at the first success, with `SCREENEXTEND_HOSTAP_HOLD_SECS` to hold the AP up for a scan (`macos_utils/hosted_network.rs`) | The verification added in row 224 reads `getifaddrs` on the AP interface. If macOS has not yet torn down the station address, that read answers with the address of the network the Mac just left, and the AP is reported as working on the wrong network. | **VERIFIED (10.15.8, twice)**: at t+2.2 s with `interfaceMode()` already reading HostAP, `interface_ipv4("en3")` returned `172.16.11.81` — the pre-start LAN address | at t+5 s through t+45 s it returns `None`, which is the true answer: the AP holds no address of its own, so nothing is serving DHCP | VERIFIED (10.15.8) | yes | The same run established that `securityType 128` is accepted and reaches AP mode in **2.21 s** on channel 36, while 1024, 512 and 256 are each refused after ~2.1 s — the first new HostAP security constant this project has found, and one the 2012 `[-127, 127]` sweep could not have seen. It also established that the resulting network is unjoinable: see the HostAP section below. |
| 228 | build (macOS) | `src-tauri/.cargo/config.toml` sets `MACOSX_DEPLOYMENT_TARGET = "10.15"` (W13a) | rustc defaults x86_64-apple-darwin to 10.12, so the shipped binary`s load command contradicted `minimumSystemVersion: "10.15"` in `tauri.macos.conf.json` and clang`s availability machinery was calibrated three releases too low. | `LC_VERSION_MIN_MACOSX version 10.12, sdk 11.1` (release binary at `80e28c7a`, and again at Phase A `46ba59e7`) | `LC_BUILD_VERSION platform 1, minos 10.15, sdk 11.1` -- the load command changes form as well as value. `check-macos-dyld.sh` still passes with **0 offenders**; `undefined 551 / ok 551`, and **weak imports fell 1 -> 0**, so raising the floor bound one previously-weak import strongly rather than adding any. | VERIFIED (10.15.8 Intel, `cargo build --release` from `src-tauri/`, 66m20s) | yes | Measured on the Phase A-3 converged tree. Verified beforehand that `MACOSX_DEPLOYMENT_TARGET` was unset in the environment, since cargo`s `[env]` does not override an already-set variable without `force = true`. arm64 cannot be checked here (the 10.15 SDK cannot target it); rustc is expected to clamp it up to 11.0 on its own. |
| 229 | bwe / encode | `SendHistory` keeps a per-frame tally beside its per-packet ring, `interpret` votes every reported symbol against the access unit its packet belonged to, and a frame every one of whose packets arrived is published as `SharedFeedback::acked_frame_capture_ns()`. | The macOS VideoToolbox and NVENC long-term-reference paths both need the same input and neither has it: which frames has the client received *in full*? The finding assumed the signal would have to be synthesised over the control DataChannel because "RTCP does not acknowledge frames". Transport-wide feedback does — it names every sequence number that did and did not arrive, and since the writer stamps those sequence numbers itself it already knows which packet belonged to which frame. | no frame-level acknowledgement of any kind; loss recovery is a forced IDR, the largest frame the encoder emits, sent on a link that has just demonstrated it could not carry the last burst | the newest intact frame, by its host-clock capture instant, at the browser's TWCC cadence — no client change, no new opcode, no round trip beyond the one TWCC already pays | VERIFIED (3 unit tests: a frame missing one packet is never acknowledged, only the newest intact frame is reported and decided tallies do not pile up, and the published value never walks backwards on a reordered report) | yes | The identity is the capture instant because the encoder already stamped it on the frame, so nothing new has to be carried to match an acknowledgement back to a reference. **Nothing consumes it yet, deliberately**: setting `EnableLTR` and feeding `kVTEncodeFrameOptionKey_AcknowledgedLTRTokens` is W5's, NVENC reference invalidation is W3's. A frame with even one lost packet is never acknowledged — the encoder may keep referencing that picture and a decoder missing a slice of it cannot. Resolves the transport half of `major-vt-ltr-frames-would-replace-idr-on-every-pli`. |
| 230 | config | `Config` carries `gpu_encode_toggle: Option<Arc<AtomicBool>>` and `software_encode_now()` beside the `bool` snapshot; the Windows pipeline reads it at encoder-selection time. | `disable_gpu_encode` was read out of the shared atomic once, at streamer-construction time, and stored as a plain `bool` — the one field in that struct literal that was not a cloned `Arc`. The toggle did take effect, but only by draining every streamer and rebuilding the whole server, which every connected device sees. | flipping "GPU video encoding" restarts the server | the handle is live where a host exists; the probe harness still falls back to the snapshot `--software-encode` sets | VERIFIED (2 unit tests: the harness falls back to its snapshot, and a live handle beats a stale snapshot in both directions) | yes | Additive on purpose, so the macOS pipeline compiles unchanged and W5 can switch it over without a cross-OS flag day. Resolves the `streamer/config.rs` half of `minor-session-gpu-toggle-snapshotted`; reading it at encoder-selection time on macOS is the remaining half. |
| 231 | session / reconfig | A scale-only device override applies to the live display through `server::apply_scale_live` instead of bumping the reconfig epoch. | The client answers an epoch bump by tearing the whole session down — peer connection, encoder, capture session, injector thread — and paying the full join cost, measured at display 706-1299 ms plus pipeline 713-1018 ms. For orientation, refresh rate, video scale, video quality and DPR that is unavoidable today because each is an encoder-session parameter fixed at init. The display scale is not one of them: it changes the extended desktop's DPI, not the captured geometry. | dragging the scale slider costs a full rejoin, ~1.6 s of black | the host applies it and the client is never told | COMPILE-CHECKED | yes | `apply_scale_live` also writes the landed scale back into the session's `LiveDisplay`, so a later reconfig compares against what is on screen rather than a stale record. Resolves item 4 of `major-session-join-is-a-fully-serial-multi-second-chain`. |
| 232 | encode / intel | `set_bitrate` re-attaches the three low-latency ext buffers to `MFXVideoENCODE_Reset`; rate control moves CBR to `MFX_RATECONTROL_VCM`, probed with `MFXVideoENCODE_Query`; `BufferSizeInKB` becomes a one-frame VBV with the output allocation floored separately | oneVPL applies whatever ext buffers are present at Reset and reverts the rest to defaults, so the first bitrate change from the loss-based driver restored HRD conformance, PicTiming SEI, the default `MaxDecFrameBuffering` and B-pyramid, and dropped `RepeatPPS`, the per-frame size caps and intra refresh. `LowDelayBRC`/`MaxFrameSizeI`/`MaxFrameSizeP` are defined for VBR/QVBR/VCM only, so under CBR all three were no-ops from the start. | the low-latency posture lasted until the first congestion event; `BufferSizeInKB` was one uncompressed BGRA frame, 8160 KB at 1920x1088, about 6.5 s of CPB at 10 Mbps | ext buffers survive every Reset; VCM accepted on Intel UHD Graphics; VBV is one frame | VERIFIED (Windows, Intel UHD Graphics) | yes | Falls back to CBR and clears the three dead fields when `Query` refuses VCM, rather than leaving them set and claiming a cap that is not applied. |
| 233 | encode / intel | oneVPL session creation serialised behind a process-wide lock, with four retries and a 25 ms linear backoff | `MFXLoad` + `MFXSetConfigFilterProperty` + `MFXCreateSession` is a dispatcher-wide enumeration; two at once against the same iGPU and one gets `MFX_ERR_NOT_FOUND`. An encoder is built per streaming session, so two clients joining together is exactly that race, and the loser fell to x264 for the life of the session having logged that the machine has no Intel hardware. | 5 of 6 parallel runs skipped at least one Intel encoder test, reported as no Intel hardware oneVPL implementation | 0 skips in 6 consecutive runs; `intel_quicksync_survives_concurrent_session_creation` fails 4/4 without the lock | VERIFIED (Windows, Intel UHD Graphics) | yes | Resolves `major-intel-quicksync-partly-unavailable-on-this-box`, which read as a per-configuration refusal. Statuses are named now (`status_name`), so `mfxStatus=-9` is no longer the whole message. |
| 234 | encode / intel | `mfxExtCodingOption2::MaxSliceSize` set to 1150 bytes, one RTP payload, probed with `ENCODE_Query` and falling back to the height heuristic when refused | `NumSlice = (height/256).clamp(4,8)` cut the picture by height, while the thing that loses packets is the network. `MaxSliceSize` overrides every slice-count control where it is honoured, and the finding doubted it would be on `LowPower = ON` parts. | slices sized to a fraction of the picture height | honoured on Intel UHD Graphics with `LowPower = ON`: 30 frames at 1280x720 produced 151 slices, p50 1191 B, max 11876 B so the cap is best-effort on the IDR | VERIFIED (Windows, Intel UHD Graphics) | yes | Loss resilience rather than latency: one lost packet damages one slice instead of a resolution-dependent band. |
| 235 | encode / intel | `MFX_WRN_DEVICE_BUSY` retries change from `sleep(1ms)` to spin, then yield, then 200 us sleeps, under a 50 ms wall-clock budget; a wait long enough to cost a frame is logged | `std::thread::sleep(1ms)` on Windows is bounded below by the timer resolution, up to about 15.6 ms without a `timeBeginPeriod(1)` that nothing here sets. Thirty retries was up to ~450 ms of blocked encode thread for a condition that usually clears in microseconds, and the loop was entirely silent. | up to ~450 ms, unobservable | bounded at 50 ms, three frames at 60 fps, and reported | COMPILE-CHECKED (the busy path did not trigger on this hardware) | yes | A retry count meant something different depending on how long each sleep actually took; a wall-clock budget does not. |
| 236 | encode / intel | The per-frame `Flush()` after the copy into the VPP input surface: removed, measured, and put back | Dropping it looked right because both the copy and the oneVPL VPP submit go to the same immediate context, so the flush adds no ordering and only splits the command list. That misses that this pipeline is synchronous per frame (`encode_surface` runs `SyncOperation` before returning), so submitting the copy early lets the GPU start it while the CPU sets up the VPP call. | flush on: p50 6971 us, p95 7834, p99 11900 | flush off: p50 7393 us, p95 7843, p99 11850 (five release trials, 1280x720, Intel UHD Graphics, medians of the per-run percentiles) | VERIFIED (Windows, Intel UHD Graphics) | reverted | The overlap is worth more than the extra submit costs. `SE_INTEL_NO_PER_FRAME_FLUSH=1` re-measures it on a part where submits are dearer. |
| 237 | encode / intel | `libvpl.dll` deliberately not bundled; the dispatcher search order reversed to OS path first, then the exe directory, then `resources/` | The load was `Library::new("libvpl.dll")` with no search at all, under a comment claiming the Intel driver ships it. Intel's guidance is that Xe (Gen12) and later drivers install the dispatcher and pre-Gen12 applications must ship their own. | silent fall-through to x264 on a host without one, through a `teprintln!` nobody reads | an honest error naming everything that was tried, and the resolved path in the session log | VERIFIED (dev box: `C:\Windows\System32\libvpl.dll`, oneVPL Dispatcher 2.7.0, Intel, WHQL-signed) | yes, as a recorded rejection | Rejected for three reasons, written up in `resources/PROVENANCE.md`: Gen12+ drivers install it; pre-Gen12 hosts sit behind a Media SDK 1.x runtime this encoder's version gate refuses anyway, so shipping one buys a cleaner error rather than a working encoder; and `intel/libvpl` publishes source-only releases, so the only binary available to vendor is the WHQL-signed driver component, which is not a provenance chain a signed installer can record. There is therefore no `tauri.conf.json` hunk. |
| 238 | encode / x264 | `encode_nv12_padded`: the CPU-bridge readback arrives as NV12 from W3a's scaler and goes into libx264 as `X264_CSP_NV12` | libx264 deinterleaves NV12 to its internal I420 in SIMD inside `x264_frame_copy_picture`, so the scalar colour matrix that ran on a rayon pool is not moved, it is gone. The picture csp may differ from `param.i_csp`: `frame_internal_csp` maps NV12 to I420, the same internal format the parameters opened with, so the SPS is unaffected. That is the difference from the BGRA fast path, which mapped to I444. | p50 1655 us per frame | p50 450 us per frame (640x360, 30 frames, debug libraries), coding the same picture to within 1% of the same bytes | VERIFIED (Windows) | yes | `x264_nv12_input_matches_the_bgra_path` asserts `profile_idc` and `chroma_format_idc` off the real SPS. Asserting that bytes came out is how the 4:4:4 blocker survived for months. |
| 239 | encode / x264 | Colour signalling and the CPU converter both move from BT.601 to BT.709 | The VUI said SMPTE 170M and the CPU converter used the BT.601 matrix, which was self-consistent while that converter was the only door in. It is not any more: the scaler's NV12 comes out of the D3D11 video processor in BT.709 limited, and there is one VUI for both paths. | BT.601 signalled, BT.601 converted | BT.709 signalled, BT.709 converted, matching what the NVENC path has always written | COMPILE-CHECKED | yes | No visible change, because both halves move together and the client still renders what the host encoded. Signalling full range over limited-range samples is the washed-out picture this avoids. |
| 240 | encode / intel | `encode_nv12_padded`: the Intel CPU bridge takes NV12 and skips its VPP pass entirely | On BGRA the bridge re-uploaded 4 bytes a pixel and ran a `VideoProcessorBlt` to reach NV12, a conversion the scaler has already done on the same kind of fixed-function block inside a blt it was running anyway. NV12 goes straight into the encode surface. | p50 3766 us per frame | p50 2820 us per frame, 25% off (640x360, 30 frames, debug libraries), same stream to within 2% of the same bytes | VERIFIED (Windows, Intel UHD Graphics) | yes | `intel_nv12_input_matches_the_bgra_path`. The repeat path holds the encode surface the NV12 door filled directly, so it had to work off that rather than a stale VPP output. |
| 241 | encode / nvenc | `mark_ltr` / `use_ltr` / `invalidate_reference`, cap-gated on `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION` | W1's `congestion::interpret` can now name the packets the client did not receive, and `nvEncInvalidateRefFrames` takes the same `inputTimeStamp` the pipeline stamps on each picture, so a lost frame resolves straight to an invalidation and the repair frame predicts from a long-term reference the client is known to hold. | every PLI cost a full IDR | 2 LTR slots, `invalidate=Applied`, repair frame 23698 B against a 28423 B IDR (0.83x) | VERIFIED (Windows, RTX 3080 Ti Laptop) | yes | 0.83x is the pessimistic figure: `fill_synthetic_bgra` redraws the whole picture every frame, so predicting frame 40 from a reference marked at frame 1 leaves nearly an intra frame's residual. Real screen content is mostly static. The policy half, when to mark and which slots the client holds, stays with the transport, the only side that sees per-frame receipts. |
| 242 | encode / nvenc | The whole driver function table is validated once at construction; `init_params.encodeConfig` no longer points into the encoder's own field; the output allocation moves out of the bitstream lock | Twelve hot-path call sites unwrapped nullable C function pointers with `.expect(...)` on the detached `nvenc-ring-encode` thread, whose join result is discarded, so a partially populated table meant a panic nobody sees and a stream that silently stops. The self-pointer made `Encoder` self-referential while `unsafe impl Send` moves it across threads. `mem::take` on the scratch buffer put the next frame's allocation inside `nvEncLockBitstream`. | a panic on a detached thread; a dangling pointer nothing happened to read; one allocation per frame inside the lock | a named error `build_backend` can fail over on; a null pointer every user overwrites before use; one read into resident memory inside the lock | VERIFIED (Windows, RTX 3080 Ti Laptop) | yes | The `.expect(...)`s stay, but they now document an invariant that was actually established. |
| 243 | encode / nvenc | The concurrent-session cap is identified rather than reported as NVENC being unavailable | Consumer drivers cap concurrent NVENC sessions and ScreenExtend opens one per streaming session, so the Nth client is ordinary. `build_backend` already degrades to Quick Sync or x264; the problem was that the log read as no NVENC on this machine. | `nvEncOpenEncodeSessionEx failed: NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY` | the same, plus what it means and what happens next | VERIFIED (Windows, RTX 3080 Ti Laptop, driver 580.97): 8 sessions opened, the 9th refused | yes | The status is `NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY`, which its name does not suggest and which no code here recognised. Probed by attempting, never counted. `nvenc_session_limit_degrades_instead_of_panicking` also pins that the eight already open keep encoding through it. |
| 244 | encode / nvenc | `nvenc_layout.rs` regenerated from the real header by `tools/nvenc_layout_gen.c` instead of from bindgen | The 6167-line layout test asserted the Rust structs against constants emitted by the same bindgen run that produced them, so it caught a hand edit to the Rust and nothing else. A wrong `NV_ENC_*_VER` word is not a compile error: the driver returns `NV_ENC_ERR_INVALID_VERSION`, `build_backend` catches it, and the NVIDIA path degrades on a machine with a working NVENC. | 6167 lines, self-referential | 380 lines, every number read out of `resources/nvenc-headers/nvEncodeAPI.h` (nv-codec-headers `n12.1.14.1`, MIT) by MSVC; all 9 tests pass | VERIFIED (Windows) | yes | `major-nvenc-sdk-version-vs-struct-layout-mismatch` claimed 12.1 bindings carrying SDK-13 layouts. Wrong: every member it cited is present at 12.1, and 13.0 raises every struct revision by one while `version.rs` carries the lower number in each case. The assertions cover the GUIDs too, which bindgen mis-generates (rust-bindgen#1888) and `guid.rs` transcribes by hand. |
| 245 | encode / amd | `encode_nv12_padded` on the AMF CPU bridge, colour range as a function of the input format, and `OutMatrixCoeff` / `OutColorProfile` set | AMD state that packed-RGB input silently invokes an internal GPU colour converter that competes with the foreground application, and OBS refuses BGRA on its texture path for the same reason. The BGRA path is full range and the scaler's NV12 is BT.709 limited, so a constant `FullRangeColor = true` would signal full range over limited samples, which is a washed-out picture. | BGRA in, internal converter active, full range signalled unconditionally | NV12 in through `retune_input_format`, converter gone, range follows the format | UNVERIFIED, no machine on this project has an AMD GPU | yes | `tools/amf_layout_gen.c` now pins the encoder's hand-transcribed colour constants against `ColorSpace.h`: `COLOR_PROFILE_FULL_709` is 7, not 4, because the header leaves the G10/G2084 values commented out in between. Verification recipe in `docs/manual-tests/amd-amf.md` sections 4 and 11. |
| 246 | encode | Sub-frame and slice-mode output evaluated and rejected, on NVENC (`sliceMode` / `enableSubFrameWrite`), AMF (`OUTPUT_MODE = SLICE`) and x264 (`nalu_process`) | It is the only mechanism any of the three offers that gets bytes onto the wire before a frame finishes encoding. | - | The gain is bounded by the encode itself: NVENC at 1920x1080, 200 steady frames on a fixed picture, p50 3642 us for upload plus map plus encode and 1980 us for map plus encode alone, so at most about 2 ms of transmit overlap. | VERIFIED (the ceiling); rejected on that number | no | The cost is not in the encoder. `webrtc_session.rs` receives one complete access unit per `frame`, packetizes it in one `payload()` call, derives one RTP timestamp from `frame.capture`, marks the last packet, and runs `Pacer::begin_frame` and `FrameTiming::mark_packetize_done` per frame; `access_unit_has_sps` assumes a whole AU and the client recovers capture time per frame. Partial access units change all of that and add a loss mode where the client waits for a frame that never completes, for about 2 ms, against slices that also cost compression efficiency. Revisit only if the transport gains a partial-AU contract for another reason. |
| 247 | encode / linux | `streamer::platform::EncoderBackend` and `BackendConfig` deleted, along with the `VaapiBackend` stub | The trait modelled none of the surface the pipeline calls: no texture input, no repeat, no prescale hint, no device accessor, and `BackendConfig` could not carry the profile or the intra-refresh flag. Its last implementor was a `bail!`-only Linux stub nothing constructed, kept alive by `#![allow(dead_code)]`. | a dead abstraction the next platform would have implemented | nothing; Linux gets the `Backend`-enum shape the other three have, when it is written for real | COMPILE-CHECKED | yes | This is the trap the AMD backend had to step over: satisfying an interface the pipeline cannot call. |
| 248 | encode / nvenc | Per-frame `nvEncMapInputResource`/`nvEncUnmapInputResource` measured and left alone | The map/unmap pair is two driver round trips per frame on a path where every other serialisation was being questioned; the finding asked for a measurement before deciding whether to hold the mapping across frames. | - | 1920x1080, 200 steady frames on a fixed picture: p50 3642 us for upload plus map plus encode, 1980 us for map plus encode alone, against a 16666 us budget | VERIFIED (Windows, RTX 3080 Ti Laptop) | no change | Holding a registered resource mapped across frames means guaranteeing the producer never writes it, which the ring's keyed mutex does give — but the whole pair is a small fraction of a frame that already fits the budget five times over, and the NVENC samples map per frame for a reason. Not worth the invariant. |
| 249 | encode / nvenc | `set_bitrate` keeps sizing the VBV from the *configured* frame rate rather than the delivered one | `config.fps` is fixed at construction while the delivered rate varies: WGC throttles, the ring drops frames when all slots are busy, and the idle keepalive emits at 200 ms. So the VBV is sized for a rate the encoder is often not running at. | - | - | reasoned, not measured | no change | Sizing from the configured rate is the conservative direction: if the real rate is lower, `bitrate/fps` under-sizes the VBV relative to the true per-frame budget, so the encoder quantises harder than it needs to. That costs quality, not latency — and it only bites when delivery is well below the configured rate, which is the congestion case where a smaller VBV is what you want anyway. The other half of the finding, `lowDelayKeyFrameScale` being inherited from the preset and never read back, is fixed: it is set to 2 explicitly and the measured IDR/P ratio is 2.0x. |
| 250 | cli / config | `disconnectGraceSecs` moves from `localStorage` to `config.json` (`Config` type, `defaultConfig`, the editor schema, `applyConfig`), with a one-time migration in `App.tsx` (W13b) | The GUI wrote the setting to `localStorage`; `ScreenExtend serve` read it from `config.json`. Neither knew about the other. | `serve` applied the 10 s default however the user had configured it; `config set disconnectGraceSecs 30` changed `serve` while the Settings field kept showing the old value | one key, one store, read by both; existing `localStorage` values are migrated and the key removed | VERIFIED (build + type-check + lint; the 45 s end-to-end join test needs a client) | yes | The three-touch rule in CLAUDE.md, applied: TS type + default, the Rust command, the `serve`/`config` path. Nothing changes on screen. |
| 251 | cli / session | `serve` replays device trust from `knownDevices[]`, the per-device audio-output pin and `legacyVolumeKeyProxy`, not just the display overrides (W13b) | `apply_saved_devices` was written when `devices[]` was the only persisted per-device state and was never revisited when trust tokens and audio pinning landed. | a headless host started from the same `config.json` as the GUI un-banned **every** banned device and made every approved device retype the OTP; a device pinned to a host output reverted to the default | bans, approvals and output pins are in `AppState` before the first adapter binds | VERIFIED (unit tests over the `devices[]`/`knownDevices[]` shapes; the ban-then-serve run needs a client) | yes | Trust is keyed on the per-device token, not the IP. `parse_saved_devices`/`parse_saved_trust` are split out so the shapes are testable without an `AppHandle`. |
| 252 | cli / ipc | `RELOAD`, `PING` and `LOGS` verbs on the single-instance control socket (W13b) | The CLI is a different process from the host, so `devices set` could only write the store and print "restart it to apply", `logs` could only read its own empty ring, and `host_running` had to *take* the lock to test it. The socket already existed. | `devices set --refresh-rate 120` needed a reconnect; `ScreenExtend logs` printed nothing, ever; a `status` racing a starting GUI could win the lock and make the real instance believe it was the second one | `devices set|reset`, `turn set|clear` and `config set` print "Applied to the running host."; `logs` prints the host's backlog; `host_running` asks over the socket and falls back to the lock | VERIFIED (build, clippy, tests) | yes | `RELOAD` re-reads `config.json` from disk and runs the same `apply_saved_config` `serve` uses, so the two paths cannot drift. |
| 253 | cli / security | `serve` reads the OTP from `SCREENEXTEND_OTP`, `--otp-file` or stdin (`--otp -`), warns when it came from argv, and `session new` prints the env-var form (W13b) | argv is readable by every process on the machine and is written to shell history; the OTP is the only gate on a join once the session id is known, which is why `OtpLimiter` exists at all. | `session new` actively told the user to paste the OTP on a command line | the credential can travel without ever entering argv; the old flags still work and say why they are worse | VERIFIED (build; the `ps`/Task Manager confirmation is in the CLI manual script) | yes | Same treatment for `--session-id` via `SCREENEXTEND_SESSION_ID`. |
| 254 | cli | `fast_path` claims `-h`/`--help`/`-V`/`--version` only as the **first** token; per-subcommand help now comes from `tauri-plugin-cli`'s own match (W13b) | It scanned the whole argument vector, before the plugin parsed anything. | `config set key -h` printed the global help and exited 0 instead of storing `-h`; every value-taking flag had the same hole, and a script could not tell it from success | the literal is stored; `--help`, `-h`, `help`, `serve --help` and the version forms all still work | VERIFIED | yes | The plugin reports help as a match with no subcommand and a `help` arg; `route` prints that text rather than falling through to `LaunchGui`. |
| 255 | cli / robustness | `acquire_lock` returns `LockOutcome { Acquired, HeldByOther, Unavailable }`; the legacy installer subcommands and the GUI's `app_local_data_dir().unwrap()` report instead of panicking (W13b) | Three outcomes collapsed into `Option::None` and every caller read them all as "already running". The legacy branches were ~20 `.unwrap()`s on shell output and resource paths, reachable from the NSIS/WiX uninstall hooks. | an unwritable lock file offered the user "Show running instance" for an instance that does not exist; a missing `nefconc` crashed the process mid-uninstall; a machine whose local-app-data cannot be resolved panicked before drawing a window | each outcome says what it is; the legacy branches exit non-zero with a logged reason, which is what an uninstall hook needs | VERIFIED (build, clippy, tests on both OSes) | yes | Non-zero `certutil -delstore` is still tolerated: a certificate that is already gone is the normal uninstall case, and the previous code ignored the status entirely. |
| 256 | cli / diagnostics | `doctor` and `status` gained the driver section `status`'s own description has always promised (W13b) | `doctor` grew out of a thin printer over `check_system_requirements` + `check_permissions`, so it never knew about drivers. | neither reported any driver state, while `status --help` and `tauri.conf.json` both claimed it | Windows: the IDD driver's pipe state plus whether the signing certificate is still in the root store; macOS: the legacy audio driver's health and whether its installer package is in the bundle | VERIFIED (Windows: `IddDriver` + `trusted`); COMPILE-CHECKED (macOS) | yes | The certificate half matters on its own: a driver installed with the certificate since deleted stops loading after the next reboot. |
| 257 | logbus / ipc | `set_log_streaming(false)` from the Settings log panel's unmount (W13b) | Tauri 2 exposes no way to ask how many JS listeners an event has, so the writer thread had to be told; `get_log_backlog` armed the stream and nothing ever disarmed it. | opening Settings -> Logs once kept up to 20 batched `LogLine` IPC emits per second for the rest of the process's life | the emits stop when the panel closes; `tprintln!` is unaffected | COMPILE-CHECKED (the counter run needs the app open) | yes | Residue of W13a row 76, which removed the per-line cost but not the steady state. The frontend change is a cleanup callback and renders nothing. |
| 258 | permissions (macOS) | `accessibility_api_probe` restores the AX messaging timeout to the system default (`AXUIElementSetMessagingTimeout(el, 0.0)`) before returning (W13b) | Apple documents that setting the timeout on the **system-wide** element applies it to every accessibility message the application sends, not just that element's. There is no getter, but 0 is documented as "use the default". | one permission read left remote input (`streamer/input/macos.rs`) and the AirPlay menu-extra walk (`airplay/ax.rs`) on a 0.5 s ceiling for the life of the process, having never asked for one | the ceiling lasts for the duration of the probe | COMPILE-CHECKED (10.15.8) | yes | The cost and no-signal halves of this were already fixed; only the global side effect remained. |
| 259 | permissions (macOS) | `check_permissions` and `request_permission` are `async` and run their FFI on `spawn_blocking`; the window-list probe is warmed at startup beside `input::prime()` (W13b) | A plain `fn` Tauri command runs on the main thread. | `request_permission("screen_recording")` blocked the main thread until the OS prompt was dismissed — seconds of a frozen window; the first `check_permissions` paid the 37 ms cold window-list cost inline, during launch | neither touches the main thread; the cold cost is paid on a background thread before the gate asks | COMPILE-CHECKED (both OSes) | yes | `probe_permissions()` is the sync entry `doctor` uses, added to all three `*_utils` so the surface stays identical. |
| 260 | permissions (macOS) | A session watchdog re-reads `screen_recording_live()` every 30 s while any session holds a live display, and ends the sessions on a definite `Some(true) -> Some(false)` transition (W13b) | macOS 15 re-asks for consent roughly every 30 days with no supported way to suppress it, and a dismissed dialog does **not** stop capture: `CGDisplayStream` keeps delivering `FrameComplete` frames of the wallpaper. | the host reported a healthy stream while the client showed a blank desktop, with nothing in the log until something restarted the pipeline; detection only happened at pipeline start | the revocation is named in the log within 30 s and the device is dropped rather than left watching wallpaper | UNVERIFIED (needs macOS 15+; neither dev box can produce the transition) | yes | `None` is never treated as a refusal — that is the same false-positive shape the window-list probe replaced. Script: `docs/manual-tests/macos-permissions.md` §8. Raising the permission modal on the host is a UI change and is recorded below instead. |
| 261 | airplay (macOS) | `airplay::probe()` is wired into `doctor`, and `Receiver::take_geometry_changed()` into the reconfig epoch via the session watchdog (W13b) | Both signals were implemented, tested and connected to nothing. | when the AirPlay backend fails it fails by macOS quietly not creating a display, and nothing could ask whether the picker listed us at all; a macOS-initiated geometry change was only noticed by a `CGDisplayPixelsWide` poll | `doctor` prints the picker's device list (and says so when ScreenExtend is absent from it); a geometry change reaches the client on its next `/reconfig` | COMPILE-CHECKED | yes | `geometry_changed()` became a `swap`, so each edge is delivered once. The tier is below the supported floor, so both are cheap safety nets rather than hot paths. |
| 262 | session (macOS) | The macOS adapter thread is named `se-adapter-<ip>` and its spawn failure is reported (W13b) | The Windows call site got this with the shared-runtime change; the macOS one still discarded its `JoinHandle`, and with it the spawn error. | a thread that failed to start left `streamers.insert(ip, ...)` claiming an adapter was served with nothing listening on it | the failure is logged and the adapter is skipped | COMPILE-CHECKED (10.15.8) | yes | The shared `streamer::transport_runtime()` already reached this call site with no edit, since it lives in the OS-independent `streamer/mod.rs`. |
| 263 | build | `tauri-plugin-http` removed; `wmi` moved under `cfg(windows)`; the duplicate macOS `crossbeam-channel` dropped; `crate-type` reduced to `["rlib"]` (W13b) | The http plugin had no capability grant in `default.json` and no `@tauri-apps/plugin-http` on the JS side, so every one of its commands would have been refused by the ACL; nothing in Rust used it either. `staticlib`/`cdylib` are the mobile-target defaults from the Tauri template and there is no mobile config. | 500 unique crates in the resolved graph; three link passes of the whole crate per build; `wmi` (and chrono, and futures) resolved for macOS too | **492** crates, 134 lines out of `Cargo.lock`, one link pass | VERIFIED (`cargo tree` before/after; `cargo check` on Windows and macOS) | yes | Most of the http plugin's 306-crate subtree is shared with the rest of the graph, which is why the net is 8 rather than hundreds. |
| 264 | build (macOS) | `bundle.macOS.infoPlist` and `minimumSystemVersion` declared in both configs; the audio driver package carried by `bundle.macOS.files`; release CI asserts all of it (W13b) | `src-tauri/Info.plist` reached the bundle only by a Tauri filename convention nobody had asserted, and the package was in no config at all. | a macOS build made without the overlay got no `LSMinimumSystemVersion` and no `libopus.dylib`; `install_audio_driver` on a released app took its `Err` branch, so the 10.15-12.x audio tier was unreachable for anyone who did not build from source | the plist is declared as well as conventional, and the release job fails if `NSAudioCaptureUsageDescription`, the ATS keys, `LSMinimumSystemVersion`, `NSSupportsAutomaticGraphicsSwitching` or the package are missing from the built app | COMPILE-CHECKED (`cargo check` on the Mac; the bundle assertions run on the next release) | yes | `bundle.resources` was tried first and rejected — see the note below. |
| 265 | build (macOS) | `tauri.macos.conf.json` gets the Windows CSP back plus `worker-src 'self' blob:; child-src 'self' blob:` (W13b) | The overlay set `csp: null`, and it is auto-merged on every macOS build, so every shipped macOS app ran its control UI with no policy while Windows ran with one. | no CSP at all on macOS | the same policy as Windows, with the two worker directives monaco's language services need — `child-src` because Safari 13.1 has no `worker-src` and falls back to it | COMPILE-CHECKED | yes | Manual check in `docs/manual-tests/macos-build.md` §5: exercise the settings editor on the 10.15 box and confirm no `Refused to ...` line in the Web Inspector. |
| 266 | build | `scripts/check-bindings.mjs` and `scripts/check-versions.mjs`, both in CI; the bindings export path resolves from `CARGO_MANIFEST_DIR` (W13b) | `bindings.ts` is written by whichever debug binary last ran with no arguments, is prefixed `// @ts-nocheck`, and was written to a path relative to the *current directory*. | a stale debug binary could silently revert the whole TS API surface, with `tsc` unable to see it and the only symptom a runtime "command not found"; running the binary from anywhere but `src-tauri/` wrote to the wrong file or panicked | the path is always right, and CI fails on any command or event that is in one of the two files and not the other | VERIFIED (the checker was negative-tested by removing a command and an event from `bindings.ts`) | yes | `// @ts-nocheck` stays: without it the only error is `TAURI_CHANNEL is declared but never read` in the generated file, which cannot be fixed without hand-editing a generated artifact. The static cross-check is the better guard anyway. |
| 267 | build (Windows) | `resources/PROVENANCE.md`'s virtual-display-driver section rewritten from the artifact itself (W13b) | It described the driver as an upstream release. It is not: the DLL embeds this dev box's Cargo registry paths and reports `Initialized Virtual Display Driver v1.0.0 @ eae5120`, and the INF is rebranded throughout. | nobody but this machine could reproduce the binary, and the only way to learn what it was, was to run `strings` on it | the fork's upstream base commit, the build machine, the branding delta, the signing situation, and — the fact nothing else in the tree recorded — `UmdfExtensions = IddCx0102`, i.e. the IddCx 1.2 capability ceiling | VERIFIED (every claim read out of the shipped `.dll`/`.inf`) | yes | Publishing the fork is still outstanding; two queued driver improvements are blocked on it in practice. |
| 268 | macos compat | `macos_at_least` reads `NSProcessInfo.operatingSystemVersion` once and caches it, instead of forking `sw_vers -productVersion` per call | The fork ran several times a second while the permission gate was open, parsed stdout, and **returned `true` when the parse failed** — the one thing a version gate must not do. Every tier on this path is now chosen by probing the capability itself (`AnyClass::get`, `dlsym`, an attempted encoder create), so the version is wanted only for the human-readable line in `CompatibilityReport`. | one `posix_spawn` + stdout parse per call, fail-open | one Objective-C message at first use, cached for the process | VERIFIED (10.15.8: gates green, 294 tests pass; `dtruss` for `posix_spawn` not re-run) | yes | Closes the retagged `major-compat-mac-screencapturekit-available-is-a-version-check-not-a-class-lookup` and `major-compat-mac-capabilities-gated-by-a-sw-vers-subprocess`; the class-lookup half landed with the capture-tier work in row 153. |
| 269 | webrtc / rtx | `enable_sender_rtx(true)` **re-evaluated and rejected again** (row 62), with the reason moved into the code beside `udp_only_setting_engine`. | Brief item 6 asked for it, and it looks like an oversight every time somebody reads the answer's `video/rtx` registration. | — | Re-verified against `interceptor-0.17.1`: `bind_local_stream` binds only when `stream_support_nack(info)` — `rtcp_feedback` containing `nack` with an empty parameter — and the RTX codec is registered with `rtcp_feedback: vec![]`, so the responder never binds it; `resend_packets` looks the stream up by `nack.media_ssrc` and writes to the **primary** SSRC's writer. | VERIFIED (source reading, twice) | no — not enabled | Enabling it adds an `a=ssrc-group:FID` telling Chrome to expect retransmissions on a stream that is always silent, which is worse than the repairs it already accepts as late arrivals on the primary SSRC. It was briefly enabled during this close-out and reverted; the comment now lives at the setting engine so the next reader does not rediscover it from a ledger row. |
| 270 | build | `.gitattributes`: `src-tauri/resources/amf-headers/** text eol=lf` (W13b) | The 26 vendored AMF headers have their SHA-256 pinned in `SHA256SUMS`, taken from the LF content, and nothing told git they were not ordinary text. | `sha256sum -c` in `src-tauri/resources` reported **7 OK, 26 FAILED** on Windows, for files nobody had touched, while the `blob-checksums` CI job passed on its ubuntu runner | **33 OK, 0 FAILED** on Windows | VERIFIED | yes | Found while checking that the driver checksums still matched after the PROVENANCE rewrite. Same rule W13a wrote for the binary blobs, extended to the one directory it did not cover; a checksum that only holds on one OS is not a checksum. |
| 271 | whep / leak | The whole fallible region of `prepare_whep` is one `async` block whose `Err` closes the peer connection, closes the media socket and decrements `LIVE_SESSIONS` before returning. | Everything between `new_peer_connection` and the answer can fail — `set_remote_description` on a malformed offer most realistically — and every one of those `?`s returned with the connection, its ICE agent, that agent's UDP sockets and the mux read worker still alive. A client retrying a bad SDP leaked one of each per attempt. | one peer connection, one ICE agent and its sockets leaked per failed negotiation, with `LIVE_SESSIONS` still counting them | nothing outlives a failed negotiation; the trickle resource id is minted after the block, so a failure leaves no registry entry either | COMPILE-CHECKED (`LIVE_SESSIONS` is the counter that would show it; no induced-failure harness exists) | yes | Same shape as the leak `SessionHandle::close` was added to fix, on the path that never reaches a `SessionHandle`. |
| 272 | encode / pipeline | `needs_scaler` gains a second reason: a CPU-bridge backend that wants NV12 gets an identity blt when the client is already at the display's size | The pipeline built a scaler only when the sizes differed, on the reasoning that an equal-size blt is an identity pass and a wasted surface. True of the blt, and it misses that the *readback* is the cost — so a native-resolution client on a CPU-bridge encoder, the common software-encode case, read back 4 bytes a pixel and converted colour on the CPU. | 1920x1080 identity, bgra8: blt 11 us, readback 2394 us, total 2405 us, 8,294,400 bytes | nv12: blt 6 us, readback 1170 us, total 1176 us, 3,110,400 bytes — 51% off the stage (60 iterations, median per frame, `identity_blt_to_nv12_against_a_bgra_readback`) | VERIFIED (Windows) | yes | And that is before the encoder-side saving of not running a colour matrix on the CPU at all. It also means every x264 CPU-bridge path now has a scaler, so the scalar BGRA convert is gone rather than merely avoidable. |
| 273 | encode / intel | The same-adapter path's per-stage split measured, and oneVPL surface import rejected on the number | `major-intel-extra-full-frame-copy-per-frame` called the full-frame `CopySubresourceRegion` into the VPP input surface a real cost, and noted that nothing timed it separately from the VPP pass. | unmeasured | 1920x1080 to 1280x720 on Intel UHD Graphics, per-frame medians: get_surface 2 us, copy_into_surface 136 us, vpp_submit 64 us, against a ~7000 us frame | VERIFIED (Windows, Intel UHD Graphics) | no change | About 2% of the frame, and most of the 136 us is the `Flush` that measured as a p50 win and would go with the copy. Importing the captured texture as an `mfxFrameSurface1` (`mfxSurfaceD3D11Tex2D`, oneVPL 2.9+) would be new FFI on a version-gated API to remove 2% and a measured gain. The timing lives in a fixed-capacity sample ring that allocates once; sorting happens only in the accessor, which the encode thread never calls. |
| 274 | encode / intel | Cross-adapter Quick Sync documented as a conclusion rather than a gap | `OpenSharedResource1` on an NT handle requires both devices on the same adapter, so the shared-texture trick that lets NVENC take a cross-adapter capture texture has no equivalent here and the frame must come back through system memory. | a `cross-adapter bridge is not implemented` message that read like an omission | the rationale block at the guard, and three things that make it matter less: W3a's adapter policy puts capture on the encoder's adapter where it can (this box now reports `intel-same-adapter`), the bridge is 62.5% cheaper through NV12, and `Auto` tries NVENC first | VERIFIED (Windows) | yes | The `InputBridge` in the same file proves the pattern — for the *same-adapter* no-VIDEO_SUPPORT case, which is a different problem. |
| 275 | encode / x264 | ABR-with-VBV measured against a per-frame budget; `nal-hrd=cbr` rejected | `minor-x264-rate-control-is-abr-without-nal-hrd-cbr` asked whether ABR plus a VBV holds a per-frame budget or only a long-run average, which is what decides whether to set `i_nal_hrd = X264_NAL_HRD_CBR`. | unmeasured | 240 frames at 1280x720@60 and 8 Mbps against a 16666 B budget: mean 14154 B, p50 13159, p95 18054, p99 18350, max 19039 — 1.14x budget — and an IDR of 26075 B (`x264_vbv_bounds_the_access_unit_size`) | VERIFIED (Windows) | no | The VBV binds: the largest P access unit is 14% over budget against a VBV sized at two, and the mean lands at 85% of target. `nal-hrd=cbr` would add HRD parameters to the VUI and start inserting **filler data** to hold a genuinely constant rate — bandwidth spent on nothing, which is why NVENC disables filler explicitly and Quick Sync sets `NalHrdConformance = OFF`. |
| 276 | encode / x264 | The zerolatency configuration is read back from the encoder instead of asserted | `ultrafast` is a preset and `zerolatency` is a tune, both of which move fields; `param_apply_profile` moves more; `x264_encoder_open` validates and can move them again. Every claim this codebase made about the software encoder's configuration was a claim about the request. | the init log printed the request | `x264_encoder_parameters` bound; the init log prints the effective values and `x264_zerolatency_settings_were_accepted` asserts them. On the shipped DLL (build 164) at 1280x720@60: threads 8, sliced_threads 1, bframes 0, rc_lookahead 0, sync_lookahead 0, keyint 60 (finite, because with intra refresh on keyint *is* the sweep period), intra_refresh 1, scenecut 0, repeat_headers 1, annexb 1, vbv_maxrate == bitrate, vbv_bufsize 266 kbit | VERIFIED (Windows) | yes | Each asserted field is a distinct way for the encoder to be quietly buffering a frame. |
| 277 | encode | The per-frame changed-region hint evaluated and **not** consumed | Both Windows capture backends compute a dirty/move-rect union and discard it after the skip decision. The named consumers were x264's CPU colour convert (confine it to changed rows), Intel's `mfxExtDirtyRect`, and AMF's ROI map. | - | The largest of those is gone by another route: every CPU-bridge path now has a scaler, so x264 takes NV12 from the video processor and runs no colour matrix at all. | VERIFIED that the primary consumer no longer exists | no | What remains needs the capture side to expose the rect (a `Duplicator` accessor and a `ReadyMsg` field) plus a coordinate scale into encoder space, for `mfxExtDirtyRect` on one vendor and an unverifiable ROI map on another — against a saving nobody has measured, on a path whose largest cost was just deleted. Revisit if a measurement ever shows the encoders themselves spending time on unchanged macroblocks. |
| 278 | encode / amd | The AMF backend stays `UNVERIFIED`, with the manual script extended to close the remaining questions | No machine on this project has an AMD GPU, so `major-amd-backend-has-never-encoded-a-frame` cannot be resolved by measurement here — only by making sure the person who does have one is asked the right questions. | 9 sections, none covering NV12 input or the intra-refresh cost | 11 sections: section 4 now expects `color_range` to follow the *input format* and asks for an NVENC side-by-side on the colour VUI; section 10 measures what ULL's free rolling intra refresh costs (encode the same clip with `--intra-refresh on` and `off`, compare size and SSIM/PSNR); section 11 verifies the NV12 retune, the post-retune latency and the chroma | UNVERIFIED by nature | n/a | The ABI half is as verified as it can be without hardware: `test/amd_layout.rs` is regenerated from the vendored MIT headers by `tools/amf_layout_gen.c`, and now pins the encoder's hand-transcribed colour constants too. Behaviour is the only thing left that needs a Radeon. |
| 279 | encode / nvenc | NVENC's transient encoder-busy now says "device busy", the phrase `pipeline::is_transient_encode_error` matches on | The classifier decides whether to shed the frame and carry on or tear the session down by substring. AMF says it in those words for its transient results and Quick Sync does too; NVENC's own name for the condition is `NV_ENC_ERR_ENCODER_BUSY`, which does not contain the phrase. | every NVENC busy classified as fatal, killing a session that would have recovered on the next frame | classified as transient, like the other two | COMPILE-CHECKED (the busy path did not trigger on this hardware) | yes | The contract is a string match, which is fragile — but it is the contract three backends already share, and unifying it properly is a pipeline-side change. |
| 280 | macos encode | The software-encode setting reaches VideoToolbox: `EncoderConfig.software_encode` plus a three-state `HwPolicy` on the session specification, whose `Forbid` rung sets `EnableHardwareAcceleratedVideoEncoder = false` | The flag was consumed on macOS by **nothing at all** — `streamer/platform.rs` reads it under `cfg(windows)` — so both the Settings toggle and `serve --software-encode` were inert on this platform. The ladder could not express the request either: it was a `bool require_hw`, and VideoToolbox spells "require hardware", "prefer hardware" and "refuse hardware" with two different keys and two different values. | software encode requested, hardware encoder used, nothing logged | one ladder rung, no hardware fallback, `[vt] software encode was requested …` on the session line | VERIFIED (10.15.8: gates green, 311 tests pass; the flag reaching `EncoderConfig` is unit-tested both ways. Which encoder VideoToolbox then picks is `COMPILE-CHECKED` — the 10.15 box has no low-latency encoder to distinguish it from) | yes | Closes the macOS half of `minor-session-gpu-toggle-snapshotted`. The live-handle half is one word away: `cfg.software_encode_now()` in place of the snapshot, once W1's `Config.gpu_encode_toggle` merges. Also splits the loud SOFTWARE-ENCODER warning from the calm by-request line, so the warning keeps meaning "something went wrong". |
| 281 | macos capture | The CGDisplayStream drop count reaches telemetry as `frames_dropped_capture`, not `frames_dropped_ring` | Two workstreams fixed `major-cgds-drop-count-never-read` in parallel. `frames_dropped_ring` is ours — a frame we dropped between capture and encode. `frames_dropped_capture` is documented as presents the OS coalesced away because we were still busy, which is exactly what `CGDisplayStreamUpdateGetDropCount` counts, and is the same counter DXGI's `AccumulatedFrames` feeds on Windows. Filing it under the wrong one makes the two platforms uncomparable. | macOS WindowServer drops counted as a ring drop | counted as a capture drop, comparable with the DXGI number | VERIFIED (10.15.8: gates green) | yes | Merge resolution, not a new change; the drop count itself landed in row 143. The once-a-second reporter thread from the other branch is kept and is now stopped before the summary line prints, so it cannot log a partial total on the way out. |
| 282 | macos capture | The CGDisplayStream API stays behind `dlsym` after the merge, rather than taking the incoming direct `CGDisplayStreamUpdate::drop_count` / `::rects` calls | The incoming implementation is correct and shorter, and it is a link-time reference to an API Apple obsoleted in the macOS 15 SDK. An eagerly bound symbol dyld cannot resolve kills the process before `main()`, so this is a load bug waiting on an SDK bump, not a missing feature. | 5 CGDisplayStream symbols + 5 CFString constants resolvable at link time | none: `nm -u \| grep -i displaystream` is empty | VERIFIED (10.15.8: `nm -u` guard is in `target/w5-measure.sh` and runs on every measurement pass) | yes | The same resolution keeps the first-frame bypass of the empty-dirty-rect skip, without which a client joining a still display never gets a picture — the incoming version does not have it. |
| 283 | macos encode | Long-term reference frames: still probed and reported, still not enabled | `EnableLTR` without an acknowledgement loop is not a partial win, it is a loss: the encoder enters a reference-management mode nobody drives, and `ForceLTRRefresh` with nothing acknowledged emits an IDR — today's behaviour, at a cost. W1 built the acknowledgement signal (`SharedFeedback::acked_frame_capture_ns()`), but it is on `w1-closeout`, which has not merged into `latency-overhaul`; grepping this branch for it returns nothing. | `EnableLTR` reported as `available` / `absent` per session | unchanged | DEFERRED — unblocked by exactly one event, `w1-closeout` merging | n/a | Not a rejection. After that merge the change is the four encoder-side steps written out in `findings/major-vt-ltr-frames-would-replace-idr-on-every-pli.md`, plus one `Arc<AtomicU64>` set beside the existing `pipeline.set_target_bitrate(target)` in `webrtc_session.rs`. Untestable here regardless: 10.15.8 reports `EnableLTR=absent`. |
| 284 | macos vdisplay | The AirPlay tier stores the geometry it negotiated instead of discarding it | It is the one display backend that negotiates rather than being commanded — `settle_geometry` picks off the ladder CoreDisplay publishes and the session then threw the answer away — so every join off that ladder (every DPR>1 client) pays a `CGDisplaySetDisplayMode` that cannot succeed, plus the settle poll behind it, plus a `could not force …` line. | settled geometry computed, dropped | stored on the session, readable as `airplay::negotiated_geometry(id)` | COMPILE-CHECKED (10.15.8: gates green on `mac-closeout2`. The saving is only realised once the trait override lands, and the live AirPlay harness needs a second Mac) | yes | Landed on `mac-closeout2` (`0ae9bbcc5`), not here. W5 implemented the same hunk independently and reverted it (`ef21bea4b`) so the two do not collide: the other version carries a unit test for the `None` case, which is the one the server's fallback depends on. The trait override is three lines and waits on W1's `VirtualDisplayController::created_geometry`. |

## Notes

### Decision: mac-closeout`s AirPlay NTP instrumentation supersedes the core-closeout mirror`s

Two branches instrumented the AirPlay mirror stream`s plaintext timestamps independently, and
both were written before either could see the other. The Mac mirror worktree `_wt/core-closeout`
carried `ntp_now` / `ntp_delta_ns` / `plausible` (660 lines of `airplay/mirror.rs`);
`mac-closeout` carried `ntp_fixed_to_ns` / `plausible_sender_seconds` (871 lines) and is the one
that reached `latency-overhaul`.

The orchestrator`s decision is that mac-closeout`s version stands. It is the larger and the
more careful of the two: `plausible_sender_seconds` validates the sender clock as an uptime
against a 400-day ceiling, which is what actually decides the stamp`s byte order from a single
sample, where the mirror`s `plausible` only bounded a latency window.

The mirror branch was diffed against the survivor before being dropped, and one piece of
coverage was genuinely missing rather than merely differently spelled: every surviving
assertion about the fixed-point conversion used a whole second (`1 << 32`), whose low 32 bits
are zero, so nothing exercised the fractional half. That assertion is ported as
`a_sub_second_ntp_value_converts_through_its_fraction`. Nothing else in the mirror was unique.

Dropped with the branch: commits `5d44688df` and `497d13e70` on the Mac.

### What the core close-out could not verify here

Seven of the rows above are COMPILE-CHECKED or unit-tested only, each because the acceptance test
needs hardware, an OS locale or a link neither dev box has: two simultaneous pen clients, a Mac
with Accessibility revoked, a localized Windows with a hosted-network-capable driver, a relayed
audio session, a 30 s animated-screen run on the 10.15.8 box, a deliberately wedged display
driver, and an induced-reorder link. Each is written up with its exact command and expected
reading in `docs/manual-tests/core-closeout.md` rather than left to be re-derived.

### Needs a driver rebuild, and what it is worth (core close-out)

The bundled indirect display driver is a locally built fork of MolotovCherry/virtual-display-rs
at `eae5120a6768...`, shipped signed. Nothing in this repository can rebuild it: there is no
signing identity here, and an unsigned IDD does not load. Three findings all end at that wall, so
what each is worth was measured or bounded from the host side instead, and the driver-side work
is written down here rather than left in a findings file nobody will read again.

**The INF declares `UmdfExtensions = IddCx0102`, so the OS chooses the composing adapter, not
us.** Microsoft's IddCx 1.4 notes say it plainly: "IddCx versions before IddCx 1.4 used the
power-on self-test (POST) adapter to render the desktop image passed to the IDD if it was not
PnpStopped. If the POST adapter was PnpStopped, the system-supplied Windows Advanced
Rasterization Platform (WARP) was used instead." `IddCxAdapterSetRenderAdapter`, the only way for
a driver to ask for a different one, is IddCx 1.4 / Windows 10 1903. The project's stated Windows
floor is 2004 (19041), which carries IddCx 1.5, so targeting 1.2 buys compatibility with
1709-1809 that is neither claimed nor needed.

*What shipped instead:* the diagnosis half, which needs no driver change. `report_adapter_pairing`
already named the capture and encode adapters and whether they matched; it now also says, on a
mismatch, that an IddCx 1.2 driver has no way to avoid it, and it already shouts when the capture
adapter is a software rasteriser (the WARP case, previously silent). On the dev box the two
adapters are the same NVIDIA GPU (`same_adapter=true`, LUID 0x17D9A), so the cross-adapter cost
is **0 here** and this is a hybrid-laptop finding that this hardware cannot exhibit.

*Expected upside of the rebuild:* on a hybrid host where the POST adapter is the iGPU, one
full-frame cross-adapter copy per frame disappears — at 1920x1080 BGRA that is ~8.3 MB over PCIe
per frame, roughly 1-3 ms at 60 fps plus its synchronisation. On a host whose POST adapter is
PnpStopped it is larger and qualitatively different: the desktop for our virtual display is
rasterised by WARP, on the CPU that is also running the encoder.

*Recommendation:* set `UmdfExtensions = IddCx0104` or higher, guard the call with
`IDD_IS_FUNCTION_AVAILABLE(IddCxAdapterSetRenderAdapter)` so one binary still loads on IddCx 1.3+
hosts, add a `SetRenderAdapter(Luid)` command to `driver_ipc::DriverCommand`, and have the host
pass the LUID its encoder already enumerated. The IPC side is ready: the host now negotiates a
protocol version by pipe name, so a rebuilt driver can be recognised without an old one ever
being sent a message it cannot decode.

**A mode-list change is a hot-unplug, and `IddCxMonitorUpdateModes` would not be.** The fork's
`ipc.rs::notify()` says so in its own comment — "updated monitors causes a detach, update, and
reattach. (Required for windows to see the changes)" — and implements it as
`IddCxMonitorDeparture` then `IddCxMonitorCreate` + `IddCxMonitorArrival`. The parenthetical is
wrong: `IddCxMonitorUpdateModes` updates a monitor's reported mode list in place, its minimum
supported client is Windows 10 / Server 2016, i.e. IddCx 1.0 — *below* the 1.2 the INF already
declares — and upstream simply never used it. This is why `dpr_mode_ladder` pre-registers the
whole ladder at create time and why `CLAUDE.md` records that a live display cannot be given new
modes.

*Measured, median of three trials, Windows 11 26200, bundled driver
(`report_mode_change_cost`):*

| | median | trials |
|---|---:|---|
| in-ladder mode change, resolution unchanged | 111 ms | 89 / 111 / 112 |
| in-ladder mode change, new resolution | 308 ms | 296 / 308 / 381 |
| departure alone | 179 ms | 178 / 179 / 187 |
| remove + recreate + reattach | 491 ms | 459 / 491 / 501 |
| then set the new mode | 281 ms | 276 / 281 / 333 |

So reaching a geometry inside the registered ladder costs ~308 ms and one outside it ~772 ms —
2.5x — and the wall clock is the smaller half of the difference: the recreate is a real
hot-unplug, so Windows reflows every window on the desktop and the capture session is rebuilt.
Neither of those shows up in a stopwatch and both are what the user sees.

*Recommendation:* add an `UpdateModes { id, modes }` command implemented with
`IddCxMonitorUpdateModes` against the cached `IDDCX_MONITOR`, leaving the departure/arrival path
for adds, removes and enable/disable. Then the pre-registered ladder can shrink to the modes a
join actually wants, which also shortens the list the OS intersects on every
`EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES`. Note the ordering constraint: the update must land in
both the target-mode query and `EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION`, which regenerates from the
synthesised EDID.

**`IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE` is unset, and OS virtual modes are confirmed active.**
The fork builds `IDDCX_ADAPTER_CAPS` with `Flags` left at `Default::default()`, i.e.
`IDDCX_ADAPTER_FLAGS_NONE`. Microsoft: "The indirect display model automatically supports OS
virtual modes, allowing the OS to seamlessly perform mode changes using Desktop Window Manager
(DWM) scaling on a per frame basis without a display mode change... Setting this flag results in
a mode change each time the desktop resolution is changed." Measured: a display registered for
1920x1080 and 1080x1920 only comes up at **1280x720@60**, a resolution that is not in its
registered list at all, and a refresh-rate change at that resolution is refused with
`DISP_CHANGE_BADMODE` while the same change at 1920x1080 succeeds.

*Rejected for now, with the reason:* the flag needs IddCx 1.4, so it cannot be set without the
rebuild above; and the shipped path is not affected, because `streamer/server.rs` drives the
display to a registered mode with `set_display_mode` on every join and every reconfig. The flag
would remove the *class* of problem rather than an instance of it. It belongs in the same rebuild
as the two items above, together with two more worth evaluating there:
`IDDCX_ADAPTER_FLAGS_PREFER_PRECISE_PRESENT_REGIONS` (IddCx 1.8 — more accurate dirty-region
tracking at "a small CPU usage overhead", a direct win for dirty-rect-aware capture) and
`IddCxSetRealtimeGPUPriority` (IddCx 1.9).

### One injector per session, and why it stays that way (core close-out)

`speculative-input-per-session-injector-thread-and-devices.md` proposed hoisting the injector to
one process-wide thread. The concrete defects it named are all fixed and elsewhere: the discarded
`JoinHandle` (W1 close-out — `SessionHandle::close` now joins the thread), the synthetic-device
leak (`Drop for Injector`, row 47), and the hardcoded pen id (row 191). What is left is the hoist
itself, and it is **not** taken, for two reasons worth writing down rather than rediscovering.

*The measurement it needs cannot be taken here.* Its own acceptance test is four simultaneous
clients with remote control enabled, all dragging at once, compared against a single-client
baseline on host CPU, glass-to-glass latency and handle count. No machine on this project can
produce four real clients. The steady-state cost is meanwhile argued rather than measured, and
the argument holds: every injector thread sits blocked in `crossbeam::select!` whenever its client
is idle, so N idle sessions cost N blocked threads and nothing else. The case the finding actually
worries about — N priority-15 threads each making a syscall per pointer sample, competing with the
capture and encode threads that decide glass-to-glass — is also smaller than when it was written,
because a `POINTER_MOVE_BATCH` is now one `SendInput` array call rather than one per sample
(row 45).

*The hoist does not fix the thing that makes it attractive.* There is one physical cursor and one
keyboard focus, so two clients dragging at once already fight, today, silently. A process-wide
injector gives that conflict somewhere to live but still needs an arbitration policy — last writer
wins, first grabber holds, partition by display — and picking one is a product decision with a UI
consequence, not a latency change. Deciding it while the UI is frozen would be deciding it badly.

*What would change the answer:* a measurement showing those threads actually displacing the encode
thread. The four-client procedure is in `docs/manual-tests/core-closeout.md`; run it before
building anything.

### UI-visible recommendations (core close-out)

**Whether a pointer-locked client may drive the host's real monitors is a product decision, and
it is not made here.** The technical half is closed: both backends track their own position and
inject relative motion absolutely, so Windows and macOS now behave identically and the pointer
curve is applied exactly once. What remains is the clamp target. Today both clamp to
`self.desktop`, the union of every attached display, so accumulated `MOUSE_DELTA` walks the
cursor onto screens the client cannot see and can then click on. Clamping to `self.target` is a
two-line change in one shared place.

It was not made because the arguments are genuinely balanced and neither is a latency question.
*Confine it:* the product model is "hand someone a URL and a 6-digit OTP", and a user who granted
remote control of the second screen they can see has not obviously granted operation of the
desktop they cannot. *Do not confine it:* dragging a window from the virtual display back to the
real one requires crossing the boundary, and that is a thing people do with a second monitor.
The shape once decided is to clamp to `self.target` when one is known, with a per-device
override — `set_device_override` exists and `Device.remoteControl` is already a per-device
boolean, so the plumbing is there and only a control in the device sheet is missing. That control
is a UI change, which this engagement freezes, which is the other reason it stops here.

Note the blast radius is bounded by `control_enabled`: this only ever applies to a device the
user explicitly gave remote control to.

### Considered and rejected on the transport (W1 close-out)

Each of these was a `speculative` finding. None was skipped: each was taken as far as this
hardware and this dependency tree allow, and what stopped it is recorded with the fact that
stopped it.

**Forward error correction is not built, because webrtc-rs ships none of it.**
`interceptor-0.17.1/src/` contains `chain, error, lib, mock, nack, noop, registry, report, stats,
stream_info, stream_reader, twcc` and nothing else. `rtp-0.17.1/src/codecs/` contains
`av1, g7xx, h264, h265, opus, vp8, vp9` and nothing else. The single occurrence of `ulpfec` in the
whole of `webrtc-0.17.1` is a **codec registration** in `register_default_codecs`
(`api/media_engine/mod.rs:367`) with no packetizer behind it -- registering it would advertise a
capability we cannot produce a single packet of. So FEC here means writing RFC 5109 or
FlexFEC-03 from scratch in our own send path, with its own repair SSRC and sequence space, and
then measuring it on a link neither development machine has: the effect only exists on a relayed
path or a loaded AP, and the decision criterion is not "does FEC recover packets" (it will) but
"does glass-to-glass p95 improve given that the overhead comes out of the video budget".
The finding's own sequencing said to land the cheaper alternatives first and re-ask. **All of them
have now landed** -- rolling intra refresh (`cfg.intra_refresh = true` on every session), the
keyframe gate, the send pacer, TWCC + trendline BWE, a smaller RTP MTU on relayed paths, and
audio `FLAG_REDUNDANT`. The bar for revisiting is therefore: a relayed or lossy-AP session,
instrumented, that still shows PLI-driven keyframe storms after all six.

**HEVC is not negotiated, because nothing on the host can encode it.** The transport half is
genuinely small -- `rtp-0.17.1/src/codecs/h265/mod.rs` has a complete RFC 7798 `HevcPayloader`
(`impl Payloader` at `:198`) and `webrtc-0.17.1` defines `MIME_TYPE_HEVC` -- and Chrome 136+ and
Safari really do offer an H.265 payload type now. What does not exist is an HEVC encoder: NVENC's
HEVC GUID is never instantiated and no VideoToolbox HEVC compression session is created, and
`H264Profile` has no HEVC sibling. Registering `video/H265` in `build_api` before that exists
would negotiate a codec and then send H.264 bytes on it. Building transport plumbing for a codec
no encoder can produce is the AMD-AMF shape this codebase already regrets, so what landed instead
is the **detection**: `negotiate::read_offer` records whether the offer carried an H.265 payload
type and the join logs it, so the share of clients that could take it becomes a measured number
rather than a guess. The encoder is the gating item and belongs with W3/W5.

**WebTransport is not built.** The finding's own conclusion, and nothing found since changes it:
Chrome has not shipped `sendOrder`/`createSendGroup()` (the mechanism the one-stream-per-frame
design rests on), Chrome has no session `getStats()` so the whole `bitrate.rs` loop would have to
be rebuilt as in-band probing, RFC 9221 puts a second congestion controller under our own, there
is no transport-level fragmentation so a 200 KB keyframe means reimplementing RTP without
NACK/RTX, and there is no WHEP-over-WebTransport spec. The only published comparison
(arXiv:2505.22132) reports percentages with no absolute milliseconds and no LAN condition. On a
1 ms LAN, SRTP over UDP is already at the floor. Revisit only when Chrome ships stream
prioritisation **and** a LAN glass-to-glass comparison beats the WebRTC path *after* the pacer,
the REMB/TWCC work and reference invalidation -- measuring against the old unpaced, feedback-less
baseline would flatter it unfairly.

**The per-session injector thread stays per-session.** The part of that finding that was a real
defect is fixed: `input::spawn`'s `JoinHandle` was discarded, so the thread was detached and its
lifetime was tied to the peer connection's -- which nothing ever closed. `SessionHandle` now owns
both the `InjectorTx` and the `JoinHandle`, and `close()` drops the sender and **joins the
thread**, so the proof that it went away is the join rather than the hope that it did. The
remaining proposal -- hoist to one process-wide injector with session-tagged events -- is an input
backend design question (one physical cursor, one keyboard focus, and an arbitration policy for
two clients dragging at once), not a transport one, and it is retagged to W10. Its measurement
needs four simultaneous real clients, which no dev box here can produce.

### Considered and rejected on the macOS capture/encode path (W5)

**One VideoToolbox session shared between clients at the same resolution.** The hardware encoder
is instance-limited and ScreenExtend creates a session per connected device, so two phones asking
for the same geometry hold two scarce sessions to produce identical bitstreams. Sharing them is a
real saving and it is *not* a small change: the pipeline, the bitrate controller, the IDR request
and the telemetry are all per-session by construction, and a shared encoder would need a
per-client keyframe policy — a joining device needs an IDR the others do not. What landed instead
is the detection half: the instance-limit flag is read out of `VTCopyVideoEncoderList` and logged,
and a session that falls to software says so loudly. That is what turns "it only works when I quit
everything else" from folklore into a log line. The sharing belongs with whoever owns the session
lifecycle, not with the encoder.

**Falling back from ScreenCaptureKit to CGDisplayStream mid-session.** Rejected. The tier is
decided at start and now requires a delivered frame before it is committed, which covers the
documented 12.3 failure. Swapping capture backends under a live session risks a worse failure than
the stall it would paper over, and a WindowServer stall — the usual cause of a mid-session dry
spell — is not something the other API would fix. The watchdog that does help is the first-frame
deadline: after 750 ms with nothing captured the encode loop synthesizes a black IDR and counts
`capture_no_first_frame`, so the condition is visible instead of mute.

**A Metal convert/scale stage in `gpu.rs`.** Rejected, and the placeholder now says why.
WindowServer delivers the frame already scaled to the requested size and already in the pixel
format the encoder wants (`420f`), and the delivered IOSurface is wrapped rather than copied all
the way into VideoToolbox — so there is no CPU conversion to move onto the GPU. Whether
WindowServer's own downscale costs anything is a separate question, and it now has an instrument:
`cgds::tests::capture_rate` captures at native size and `cgds::tests::capture_rate_downscaled` at
half, and both print the same `displayTime → callback` distribution.

**`kVTCompressionPropertyKey_ConstantBitRate` as a replacement for the removed `DataRateLimits`.**
Rejected. CBR spends bits to hold the rate on a screen that is not changing, which is the opposite
of what a screen sharer wants; `AverageBitRate` plus the loss-based controller is the right shape.
The property is probed and named in the per-session property line so the decision is visible
rather than absent.

**`CGVirtualDisplaySettings.refreshDeadline`.** Absent on the 10.15 floor, so there is nothing to
measure here. It is probed with `respondsToSelector:` and reported, deliberately not set. It is
the only latency-named knob on the WindowServer side of that boundary, which is where the largest
single term in the host path lives, so the experiment is written up for a modern Mac in
`docs/manual-tests/macos-capture-encode.md` §10.

### W11 decisions made without consultation

Two pieces of user-visible copy on the client page changed inside a branch whose brief is latency
and compatibility. Both are kept, and both are recorded here with their exact before and after,
because copy is appearance and nobody asked for either.

**1. The HTTPS-upsell modal's body paragraph.** The trigger was a typo — "sucre" — but the
sentence around it was also wrong about *why* the switch matters: the browser withholds WebCodecs
and `SharedArrayBuffer` from an insecure context, so this is the difference between the fast paths
and the slow ones, not a vague "quicker and privately".

Before:

> This page is on an insecure (HTTP) connection. Switching to the sucre version ensures data is
> exchanged quicker and privately. Your browser may warn about the self-signed certificate once,
> which is expected; choose to More Info/Advanced --> Proceed to site.

After:

> This page is on an insecure (HTTP) connection. Your browser only unlocks the low-latency video
> and audio paths on a secure one, so switching makes the screen faster as well as private. You
> will see a one-time warning about the self-signed certificate; that is expected — choose
> Advanced (or More Info), then continue to the site.

The raw `-->` in the original renders literally in HTML and reads as a broken tag; that is why the
arrow is gone rather than reworded.

**2. The limited-browser-support modal.** The `httpsFixable` flag on each row became conditional
(`httpsCouldHelp`) instead of a hard-coded `true`. That is a correctness fix, not a copy change:
it drove a "Switch to HTTPS" affordance on iPhones whose Safari has never shipped an
`AudioDecoder` for HTTPS to restore, and on pages that were already secure. The rows themselves
were briefly split from one to three and have been **restored to the original single row with its
original wording** — see the finding
`minor-client-limited-browser-modal-lists-three-audio-rows.md`, now resolved.

Nothing else in `<body>` moved on this branch: no element added, removed, reordered or
re-classed, all four modals keep their ids, roles and inline `z-index`, and the only stylesheet
changes are `inset: 0` expanded to its four long-hands (identical rendering, Safari below 14.1)
and one `-webkit-backdrop-filter` prefix.

### W11 rejections (recorded, not applied)

Each of these was considered against a finding and deliberately not done. They are here so the
next person does not re-derive them.

- **`getPredictedEvents()` on the pointer batch.** The wire format has a predicted-sample count
  byte, and the host already knows to drop predicted samples for pen and touch and keep them
  for the cursor. The client writes 0 there and sends measured samples only. Prediction trades
  correctness for latency, and the size of both halves of that trade is a per-engine,
  per-device number that cannot be obtained on either dev box -- a wrong guess puts speculative
  cursor positions on the host that the next real sample corrects, which is worse than the
  latency it saves. The byte is in place, so enabling it later is a client-only change.
- **Chunking a dropped file across SCTP messages.** The right fix for a file above
  `maxMessageSize`, and it needs a continuation opcode, a transfer id and reassembly on the
  host -- a protocol change outside this workstream. What landed is the half that can be done
  client-side: refuse before reading, and say so. A host workstream that wants the rest should
  start from the free opcode values in `docs/INPUT-PROTOCOL.md`.
- **Moving the `/reconfig` poll onto the reliable DataChannel.** Would remove one HTTP request
  per second per device and one of the three liveness mechanisms, and the channel already
  carries `BYE` for the kick case. It is a host change -- `server.rs` would have to push the
  epoch rather than answer a poll -- and the client would still need the HTTP path as the
  fallback for a session with no data channel, so the saving is real but the client cannot make
  it alone. What landed instead is the device-enumeration half, row 86.
- **Consolidating the three client liveness signals onto one.** The reconfig poll, the `PONG`
  and the render/bytes advance all feed one `markAlive()`. That is three mechanisms, but they
  fail independently — a stalled render does not stop the poll, a dead DataChannel does not stop
  the HTTP — and the cost of the extra two is a comparison each. Collapsing them would make the
  session's liveness only as good as its weakest transport for no measurable saving. The
  redundant *work* was the enumeration and the POST, and that is row 86.
- **Gating the AudioWorklet's stats `postMessage` behind `DEBUG`.** Stale: those 256 ms messages
  now drive `adaptSafetyMargin`, so the worklet's underrun count is what grows the jitter margin
  on a link that needs it. There is no longer an unconsumed message to remove.
- **A "choose speaker" affordance for `MediaDevices.selectAudioOutput()`.** Firefox 116+ is the
  only engine with a permission-free output picker, `SEAudio.pickOutput()` implements it, and
  nothing can call it because it needs transient activation and the page has no control to hang
  it on. Adding one is a visible change to the client page, which this engagement does not
  make. Recorded under the UI-visible recommendations below.
- **A/B-ing `jitterBufferTarget` on the `<video>` fallback.** All three jitter-buffer knobs are
  inert on the WebCodecs path -- an encoded transform takes the frame from
  `RtpVideoStreamReceiver2::OnAssembledFrame` *instead of* entering the reference finder and
  the frame buffer -- so the only place any of them can move a number is the fallback. All
  three client sites now say so in comments; `webrtc_session.rs`'s
  `PlayoutDelayExtension::new(0, 0)` is the fourth and belongs to the transport workstream. The
  measurement itself needs a browser and an injected-jitter link, neither of which exists here.
  The same branch is why every receive-side timing field in `getStats()` reads zero on the fast
  path, and why a measurement plan that says "compare `jitterBufferDelay` before and after" is
  only executable on the fallback.

### W11 UI-visible recommendations (not applied)

- **An output picker on Firefox.** See the rejection above. One small control on the stage or in
  the join form, wired to `SEAudio.pickOutput()` from a click and gated on `hasSelectOut`, is
  all it needs; stats byte 75 would then be able to read 4, which today it never can.


### UI-visible recommendation (W7): the screen-recording indicator on macOS 13.0-14.1

Not applied — it changes user-facing copy, which is out of scope for this engagement.

On macOS 13.0 through 14.1 there is no audio-only system-audio API: the Process Tap arrives in
14.2, and below 13.0 the driver tier covers it. In that band ScreenCaptureKit is the only native
backend, and SCK has no audio-only mode, so ScreenExtend must run a real (2x2, 1 fps) screen
stream to receive audio at all. macOS therefore requires Screen Recording and lights the menu-bar
screen-recording indicator for the whole session — including when a device has audio on and video
off, where it appears for no reason the user can see.

`CompatibilityReport.audio_backend` already carries `screencapturekit`, so the UI can say this
exactly where it matters: in the per-device audio toggle, when that value is
`screencapturekit`. Something to the effect of *"macOS will show the screen-recording indicator
while system audio is on — this version of macOS has no audio-only capture."* Nothing needs to be
plumbed; the value is already on the frontend.

The third option the finding raised — reusing the video path's existing `SCStream` when the same
device already has video capture running — is **rejected**. The two streams have different owners
(`macos_utils/streamer/sck.rs` is W5's, per-display, started and stopped per pipeline;
`audio/sck_audio.rs` is host-wide and lives as long as the `AudioHub` has a subscriber), different
lifetimes and different content filters. Fusing them would couple the audio hub's lifetime to a
particular device's video pipeline, which is exactly the coupling `AudioHub` exists to avoid — and
it would buy nothing for the audio-on/video-off case that generates the complaint, since there is
no video stream to share there.

### `probe --audio` on macOS: the cross-platform number, and the two things it caught (W7)

W8 built `probe --audio` on Windows. Its first run on macOS (10.15.8, legacy driver tier, release
binary, `say` driving the mix) gives the directly comparable figure:

    macOS  legacy driver tier   capture -> subscriber  p50 2.10 / p90 4.19 / p99 4.19 / max 5.23 ms
    Win 11 endpoint loopback    capture -> subscriber  p50 9.44 / p90 9.44 / p99 9.44 / max 10.9 ms

    packets=849 (84.9/s), payload avg 78.5 B, silent 31, seq gaps 0, discontinuity 1
    subscriber inter-arrival p50/p90/p99/max = 0.10 / 10.49 / 10.49 / 11.24 ms

The macOS tier is roughly five times better at p50 and twice at p99, which is what a 2.67 ms IO
cycle buys against Windows' 10 ms engine period. The rate is 52.2 packets/s rather than 200
than 200 because the virtual device runs no IO cycle while nothing is playing — the gaps are host silence
and cost nothing on the wire (see row 111).

**The libopus CTL readback matches Windows exactly**, which is what
`major-build-w8-shared-audio-changes-never-gated-on-macos` asked for. macOS loads a *different*
libopus build (`libopus.dylib`, not `libopus.dll`), so "the CTLs are accepted" was not transferable
evidence — `ctl_set` `bail!`s on a rejected value and would have failed capture at startup:

    opus: libopus 1.5.x-screenextend-src — application=2051 bitrate=128000 complexity=5
    signal=3002 vbr=1 vbr_constraint=1 lsb_depth=24 prediction_disabled=0 lookahead=120 samples
    (2.50 ms), frame 5 ms

Identical field for field, `lookahead=120` included — so the number the client uses to align
playout is the same on both platforms.

**Dyld floor check on the release binary: PASS.** `scripts/check-macos-dyld.sh` against the 10.15
SDK reports 551 undefined symbols, 551 ok, **0 offenders**, 0 weak imports, 0 allowlisted — so
nothing this workstream added to the audio FFI (the Process Tap `dlsym` pair, `CATapDescription`,
the SCK class lookups, `libc::kill`, the `dispatch2` barrier) is a link-time reference newer than
the floor. Its source scan lists them as runtime-resolved, which is the intended shape.

**It caught two things no unit test could.**

1. A regression this workstream introduced. Changing the watchdog's "is a host running?" check from
   a `pgrep` fork to the single-instance lock was right for the app and wrong for `probe`, which
   takes no such lock — and the agent is bootstrapped with `RunAtLoad`, so launchd ran it the
   instant the tier started and it restored the previous default output mid-stream. Fixed by
   recording the owning pid in `RoutingState` and having `recover()` return early while that
   process is alive (`kill(pid, 0)`, a signal-existence check, not a fork). Deliberately succeeds
   only for a process we may signal: a pid now owned by another user is pid reuse, our host really
   is gone, and recovery should run.
2. A real device refusal that used to be silent:
   `output device 43 refused volume/mute mirroring (OSStatus volume=2003332927 mute=0)` —
   `kAudioHardwareUnknownPropertyError` on volume, mute accepted. That is the `let _ =` this
   workstream replaced doing its job on the first machine it met.

The buffer-frame-size range for the virtual device is now known too: `[15, 4096]`, 128 requested
and granted.

### macOS audio after the W8 merge: `ts_clamped=0`, and the number to quote (W7)

`w7-macos-audio` merged `latency-overhaul` at `cf9d6fc9`, which brought W8's shared audio core
(broadcast depth 80 ms, `FLAG_REDUNDANT`, `probe --audio`, the `AudioDiagnostics` histograms) onto
the macOS side. All three gates pass on the merged tree — `cargo fmt --check`,
`cargo clippy --all-targets -D warnings`, `cargo test` (286 passed, 16 ignored) — on macOS 10.15.8,
which closes `major-build-w8-shared-audio-changes-never-gated-on-macos`. The one conflict was the
legacy live tests: `AudioCapture` no longer carries an `rx`, so they subscribe to the broadcast
*before* `start_capture` (a receiver made afterwards misses the head of the run), drain with
`try_recv`, and assert `Lagged` never fires. It did not.

Re-measured after the merge through the shipped path, driver loaded:

    capture->packet = 2.10 / 4.19 / 4.72 / 5.22 ms  (p50/p90/p99/max)  <- W8's histogram
    encode          = 0.10 / 0.12 / 0.16 / 0.62 ms
    period=128fr  discontinuity=1  lagged=0  ts_clamped=0
    capture_ns deltas: 1 of 1227 not exactly 5.000 ms, 0 regressions, 0 sequence gaps

**Quote the histogram, not the subscriber-side number.** `capture_to_packet` is the shared
instrument both platforms report, recorded inside the worker as each packet is created. Row 111's
p50 of -0.57 ms is a *signed* subscriber-side measurement taken after the broadcast hop, and it
exists for one purpose: to establish the **direction** of the skew, which a saturating histogram
cannot show. Both agree the p99 is about 5 ms.

**`ts_clamped=0` is the answer W8's note asked for.** On Windows loopback the endpoint timestamp is
a play cursor roughly 15 ms in the future and the shipped anchor is
`min(qpc_host_ns, now - read_duration)`, with `capture_ts_clamped` counting how often that bites —
every drain on the endpoint tier. On macOS the driver stamps the mix with `mach_absolute_time()` in
`OnProcessMixedOutput`, a genuinely past instant, so the same counter reads **zero** and no clamp
is warranted. The Process Tap tier is still an open question: its aggregate carries its own latency
and safety offset, and its skew must be measured on a 14.2+ machine before that tier is trusted.

`discontinuity=1` where it read 0 before: the macOS worker now increments `discontinuity_count` on
a timeline re-seat, so the shared summary and the worker's local `resyncs` counter finally agree.

### macOS audio: what was rejected, and why (W7)

Four W7 findings were resolved by a recorded decision rather than a change. Each was tried or
reasoned to a number first.

**No clamp on the macOS hardware capture timestamp.** W8's Windows fix ships
`min(qpc_host_ns, now - read_duration)` because a loopback stream's endpoint timestamp is a *play
cursor*, roughly 15 ms in the future, and left alone it made the client under-buffer. Its note
asked macOS to measure the signed skew before trusting the equivalent. Measured: p50 **-0.57 ms**,
min -6.23, over 3277 packets — under one 2.67 ms IO cycle, and it is the *right* direction, because
the driver stamps the mix in `OnProcessMixedOutput` when the mix exists rather than when it is
heard. Applying the Windows clamp here would flatten the within-block ladder that produces the
exact 5.000 ms packet spacing (0 of 3277 deltas were anything else), which is the property the
whole `capture_ns` fix exists to provide. The Process Tap tier is a different question — its
aggregate has its own latency and safety offset — and the tap's own skew must be measured on a
14.2+ machine before that tier is trusted. Recorded in `docs/manual-tests/macos-audio.md`.

**`mach_wait_until` for the shm reader.** The finding offered it as the deadline-honouring
alternative to a poll on a thread that had asked for a Mach time-constraint policy. Rejected: the
policy was the thing that was wrong, not the sleep. A time-constraint thread that blocks for an
interval unrelated to its declared period is not honouring the contract and the scheduler's usual
answer is to demote it, so the policy bought nothing but the risk of being treated as a
misbehaving RT thread. Dropping it for `user_interactive` and halving the poll to 250 us — a
quarter of the driver's own IO cycle — is the whole of the available win, and it is measured
inside row 111's p99 of 4.54 ms. `mach_wait_until` would add a hard-real-time contract to a thread
whose producer is a *different process* that cannot signal it.

**No sample-rate converter.** `speculative-audio-mac-no-sample-rate-conversion-anywhere` proposed
either a cheap rate check or a polyphase resampler in `format.rs`. The cheap half is done on both
paths that can be surprised (SCK, which only *requests* 48 kHz, and the legacy HAL-input fallback,
which now refuses a non-48 kHz device rather than encoding at the wrong speed). The resampler is
rejected for now: Opus encodes 8/12/16/24/48 kHz only and the config fixes 48 000, all three macOS
tiers either negotiate or declare 48 kHz, and the legacy driver *is* 48 kHz by construction — so a
resampler would be dead code on every path this project can currently run, carrying a quality
question (polyphase taps, group delay) nobody can evaluate without hardware that reproduces the
problem. The `teprintln!` that fires if SCK ever delivers another rate is the trigger to revisit;
until someone sees that line, the finding has no evidence behind it.

**`shm_open` mode is fixed with a umask window, not `fchmod`.** The finding proposed
`fchmod(fd, 0666)` after `shm_open`. macOS returns `EINVAL` for `fchmod` on a POSIX shared-memory
object and there is no filesystem path to `chmod`, so clearing the umask across the create is the
only lever available. `shmcheck` reports the resolved mode and the driver logs it, so a host that
still cannot map the segment says so instead of silently taking the slow transport.

**What is left of `minor-audio-mac-swallowed-errors-and-panicking-unwraps`.** Every item that
could leave the system in a wrong state with no trace is fixed: `RoutingState::save`'s two
discarded `io::Error`s (a state file that cannot be written is exactly when crash recovery
silently cannot run), the shm reader's `.spawn(...).ok()` (which reported `Transport::Shm` with no
thread behind it, after printing the success line, and produced no audio forever), the
post-aggregate tap format `unwrap_or`s, `install_watchdog_agent`'s discarded `launchctl` result,
the volume proxy's and the volume tap's `let _ =` on HAL writes, SCK's discarded stop timeout, and
`join.join()` swallowing a worker panic. Three classes were deliberately left:

- `NonNull::new(&mut local as *mut _).unwrap()` in the property getters. The pointer is always a
  stack address, so the `unwrap` is unreachable by construction. Churning ~15 call sites to remove
  noise that cannot fire is not worth the diff.
- `AnyClass::get(c"NSString"/`NSUUID`/`NSObject`).expect(...)`. These are Foundation classes on a
  10.15 floor; if one is missing the process is already unrecoverable, and the tier probes that
  *can* legitimately fail (`CATapDescription`, the SCK classes) already return
  `AudioCaptureError::Unsupported` rather than panicking.
- `let _ = self.monitor.push(samples)` in `CaptureTargets::publish`. The monitor ring drops by
  design whenever playthrough is stopped, so a counter here would need to be conditional on the
  playthrough being live; `IoCtx::underruns`, now reported, is the signal that actually matters.

**The unbounded `AudioPacket` channel is resolved by W8's merge, not here.**
`minor-audio-mac-unbounded-packet-channel-and-blocking-teardown` had two halves. The blocking
halves are fixed (SCK's two 5 s bridges cut to 1.5 s, `start_capture`'s unbounded
`ready_rx.recv()` given a 15 s ceiling). The channel itself disappears with W8's shared-core
change, which removes `AudioCapture::rx` entirely and has backends publish into an `AudioSink`
broadcast — so adding back-pressure to a channel that is about to be deleted would be work thrown
away. The `major-build-w8-shared-audio-changes-never-gated-on-macos` finding stays open until that
merge has been gated on the Mac.

**Dead-code items 1, 2, 3 and 5 deleted; item 4 surfaced; item 6 wired.** From
`minor-audio-mac-dead-code-inventory.md`: `ProcessTapCapture::try_create` and
`SckAudioCapture::try_create` (probe constructors nothing called, each of which would have built
and *started* a whole capture session to drop it), `nonsilent_probe` (an `Arc<AtomicU64>` allocated
per start and connected to nothing), `Playthrough::gain()` and `ControlHandle::transport` are gone.
`IoCtx::underruns` is now reported by the control thread every 10 s. `is_fast_path()` went from
zero callers to being the fast path in row 113.

### W3a after-matrix: the Windows capture path measured against `docs/BASELINE.md`

Same recipe as the baseline — `ScreenExtend probe --whep-selftest --duration 30 --encoder <e>
--capture-backend <c>`, release build with `--features probe`, real capture of a virtual display
created for the run at 1280x720@60, WHEP over loopback — on the merged `latency-overhaul` code.

**Read the ranges, not a single number.** This was measured four separate times, three trials
each, because the first result did not reproduce. The laptop was shared with the other
workstreams throughout and the concurrent `rustc`/`cargo` count swung between **2 and 17** —
sometimes inside a single pass. The percentiles of the two CPU-bound cells move by about a factor
of two across passes, and that variance is the other agents' compilers, not this work. Each cell
below gives the **range of the four per-pass medians**, so the spread is visible rather than
hidden by a favourable pick. Raw logs: `scratchpad/runs`, `runs2`, `runs3`, `runs4`.

`capture → socket`, milliseconds, p50 / p90 / p99 / max. "real frames" is `frames` minus the
keepalive repeats — the frames that actually carried new picture.

| Encoder | Path selected | real frames / 30 s | capture → socket |
|---|---|---|---|
| NVENC ring, **before** | `nvenc-zero-copy-ring` | 67 | 1.57 / 8.39 / 16.78 / 18.00 |
| NVENC ring, **after** | `nvenc-zero-copy-ring` | **206** (206, 206, 206) | 1.84-2.36 / 3.67-7.34 / 6.82-37.75 / 7.08-63.19 |
| NVENC ring, after, quietest pass (load 2-4) | | 206 | **1.84 / 3.93 / 6.82 / 7.08** |
| x264, **before** | `software-x264` | 81 | 2.36 / 5.24 / 23.07 / 24.82 |
| x264, **after** | `software-x264` | **159-450** | 4.19-8.39 / 7.34-12.58 / 9.44-27.26 / 11.05-109.67 |
| Quick Sync, **before** | `intel-cpu-bridge` | 79 | 11.53 / 16.78 / 25.17 / 26.05 |
| Quick Sync, **after** | **`intel-same-adapter`** | **126-357** | 8.39-11.53 / 13.63-15.73 / 16.78-20.97 / 21.17-33.94 |
| any, DXGI, **before** | — | — | **unmeasurable** (two undiagnosed failures) |
| any, DXGI, **after** | `dxgi + zero-copy` | 0 | **still zero, now diagnosed and bounded** |

What survives the variance, and what does not:

1. **Quick Sync moved onto the same-adapter path.** Categorical, not a percentile: every Intel run
   in all four passes reports `intel-same-adapter`, and every baseline run reported
   `intel-cpu-bridge`. That is row 98 — the capture device is created on the encoder's adapter —
   and it was not a predicted consequence of it. Its p90 (13.63-15.73 vs 16.78) and p99
   (16.78-20.97 vs 25.17) are below the baseline in **all four** passes, so that improvement is
   real even though the exact figure is not pinned. This retired most of the
   "cross-adapter Quick Sync is unimplemented, so hybrid hosts fall to a CPU bridge" finding; what
   is left of it is `EncoderVendor::Auto` on a machine whose capture adapter is not Intel, since
   the `Auto` ladder asks for `HighPerformance` and would still reach the bridge if NVENC were
   unavailable.
2. **Every cell delivers far more real frames, and this is the most reproducible result here.**
   The NVENC ring returned *exactly* 206 real frames in all three of its valid passes, against 67
   before — 3.1x, with no spread at all. x264 went 81 → 159-450, Quick Sync 79 → 126-357. The
   frame-pool changes (row 96, drain-to-newest with a matched `Recreate` buffer count) and the
   unchanged-frame skip (row 102) are what freed that up: the encoder had been spending its budget
   on frames the compositor had already superseded.
3. **The NVENC tail improved and the quieter the machine the clearer it is.** At load 2-4:
   p90 8.39 → 3.93, p99 16.78 → 6.82, max 18.00 → 7.08. The `encode` maximum went 15.09 → 3.90 ms
   in the same pass. The one heavily-loaded pass (10-17 builds) shows the opposite tail, which is
   what a contended CPU does to a p99 and is why it is shown in the range rather than dropped.
4. **The x264 percentiles are not resolvable on this machine and no claim is made about them.**
   Its p99 range (9.44-27.26) straddles the baseline's 23.07 and its max range reaches 109.67.
   Some of that is genuine: x264's cost scales with how much of the picture changed, and it is now
   encoding two to five times as many real frames, so a higher per-frame cost is expected rather
   than a regression. Disentangling the two needs a quiet host, which never appeared.
5. **The p50s rise on every cell, and that is mostly the same fact as (2).** The ring's `encode`
   p50 of 0.00 in the baseline was never a real number either: `docs/BASELINE.md` note 6 records
   that it measured the submit call, not the encode, because the ring decouples the two.
6. **DXGI is still zero, and that is now a diagnosis rather than a mystery.** Duplication *is*
   created (`via DuplicateOutput1`) and the session negotiates; the output then returns
   `ACCESS_LOST` on every acquire until the new cap fires with `duplication was re-established 41
   times without ever delivering a frame`. Before, that was an unbounded silent spin at three log
   lines a second. See the DXGI note below.

**A measurement trap worth recording.** Back-to-back probe runs with no gap silently corrupt each
other. Four of the twelve runs in the third pass produced no RTP at all, and every one of them
opened with `[display] already extended (2 monitor(s), 2 source(s))` — the previous probe's
virtual display was still attached, so the session attached to a stale display and streamed
nothing. Those four were discarded rather than averaged in. A **15 s sleep between runs** removed
it completely (fourth pass: 6/6 valid). This is
`findings/minor-probe-leaves-its-virtual-display-behind-and-breaks-the-next-run.md` observed from
the other end, and anyone re-running this matrix needs the gap — `docs/BASELINE.md`'s
"Reproducing this table" says the same thing and it is easy to skip.

### What is left of the DXGI fallback on this hardware, and what that costs (W3a)

DXGI Desktop Duplication is the fallback every Windows 10 and pre-24H2 Windows 11 host used to
land on by default, and after row 95 it is a genuine fallback again rather than the default. Rows
103-107 are all repairs to it. What could not be repaired here is that **it does not run on this
machine at all**:

- `\\.\DISPLAY1` (the 1920x1080@360 primary) is enumerated by exactly one adapter, the RTX 3080 Ti
  Laptop GPU, and `DuplicateOutput` and `DuplicateOutput1` both return `DXGI_ERROR_UNSUPPORTED`
  (`0x887A0004`) on it. Trying every adapter that lists the display (row 104) does not help,
  because there is only one. This is the documented hybrid-graphics behaviour: on an Optimus-class
  machine duplication is refused for a process running against the discrete GPU.
- Against a freshly created IDD virtual display, duplication *is* created — and then returns
  `ACCESS_LOST` on every acquire, indefinitely. That is now bounded and named (row 105) rather
  than an endless silent spin, but it still produces no frames.

So every DXGI row above that claims a *latency* improvement — row 106 in particular, the
capture/encode decoupling — is `COMPILE-CHECKED` and cannot be promoted from this machine. The
functional rows (103, 104, 105, 107) are `VERIFIED` because their evidence is a failure mode that
stopped reproducing, which this machine can show. `docs/manual-tests/windows-capture.md` §5b is
the script for a host where duplication works.

The consolation is that the population that needed DXGI most has largely moved: with row 95, a
Windows 10 host stays on Windows.Graphics.Capture, which is the better path anyway.

### Rejected: `Direct3D11CaptureFramePool::CreateFreeThreaded` (W3a)

`WgcFeatures` probes `CreateFreeThreaded` and reports `+free-threaded-pool` on the dev box, and
the pool is still created with `Create`, which binds `FrameArrived` to the calling thread's
`DispatcherQueue`. Every captured frame therefore costs a DWM signal, a dispatcher-queue enqueue,
a thread wake, a `GetMessageW` return and a `DispatchMessageW` before the handler runs. Sunshine
uses `CreateFreeThreaded`; OBS uses `Create`.

Not taken, on the measurement rather than on the difficulty. The stage this would shrink is
`capture_delivery` — WGC's own `SystemRelativeTime` to the first instruction of
`on_frame_arrived` — and `docs/BASELINE.md` measures it at **p50 0.98 / p90 4.19 / p99 6.82 ms**
static and **p50 6.29 / p90 9.44 / p99 20.97 ms** on a live 1080p desktop. A thread wake on a hot
thread is tens of microseconds. Even taking the whole pump hop as pure overhead it is low
single-digit percent of the smallest of those numbers, and the stage is plainly dominated by the
compositor's own cadence.

Against that: with `CreateFreeThreaded` the handler runs on an arbitrary thread-pool thread, so
the MMCSS "Pro Audio" registration, the `TIME_CRITICAL` priority and the P-core confinement
applied in `LiveCapture::new` all land on the wrong thread and would have to be re-applied per
callback behind a `thread_local`; `on_frame_arrived` loses its arrival *ordering* (the handler
mutex still serializes, so this is not a data race, but frames could be encoded out of order); and
the `WM_QUIT` shutdown handshake needs replacing with an event the free-threaded handler observes.
Three real risks to buy a few percent of a stage that is not the bottleneck.

Revisit if `capture_delivery` p50 ever falls to the same order as a thread wake — which would mean
the compositor hop had been solved some other way, and then this would be what is left.

This disposes of both `speculative-wgc-free-threaded-pool-probed-but-not-used` and
`minor-wgc-frame-delivery-goes-through-a-win32-message-pump`, which describe the same change.

### Rejected: double-buffering the CPU-bridge staging texture (W3a)

`Scaler::read_back` and `TextureReader::read_back` both do `CopyResource` into a single staging
texture and then `Map(..., D3D11_MAP_READ, 0, ...)` with no `DO_NOT_WAIT`, which blocks until the
GPU has retired that copy. The proposal was to double-buffer and map frame N-1 while copying frame
N, or to poll with `DO_NOT_WAIT` and skip the frame rather than block.

Measured, so the arithmetic can be done: at 1920x1080 → 1280x720 the whole readback — copy, map
stall and memcpy together — is **1192 us** median in BGRA and **740 us** in NV12 (row 108), while
the `VideoProcessorBlt` submit beside it is 4-5 us. Pipelining the map costs a full frame period
of latency, 16.7 ms at 60 Hz and 8.3 ms at 120 Hz, to remove at most 1.2 ms of stall. Polling with
`DO_NOT_WAIT` costs a captured frame instead, which is worse on a path whose whole job is to
deliver frames.

Two things did land against this finding and they are the ones worth having: the stall moved off
the capture thread onto a dedicated encode thread on the DXGI path (row 106), so it no longer
delays the *next* capture; and NV12 output took 38% off it (row 108). What is left is where it
belongs.

### Rejected: removing the per-frame `Flush` in the NVENC ring stage (W3a)

`AsyncNvencRing::stage` runs `AcquireSync(KEY_WRITER)`, a copy or scale into the shared texture,
`Flush()`, `ReleaseSync(KEY_ENCODER)` on the WGC callback thread, once per frame. The question was
whether the explicit `Flush` is redundant given that `ReleaseSync` is itself a GPU-visible
synchronisation point.

Kept, without a measurement, and deliberately: the destination is a **cross-device** shared
resource (`SHARED_NTHANDLE | SHARED_KEYEDMUTEX`, opened on the other device with
`OpenSharedResource1`). A keyed mutex orders access between devices but does not force the
producer's command list to be submitted; the documented requirement for cross-device shared
surfaces is to flush before releasing, and NVIDIA's own samples do. The failure mode of getting
this wrong is a torn or stale frame that appears only under load on some drivers — the class of
bug that a 30-second probe run will not show and a user will. A microsecond-scale saving is not
worth buying that with an untestable hypothesis.

The related cost was addressed instead: `stage` was already the whole of what the callback does on
the ring path, and rows 96 and 102 removed the frames that reach it unnecessarily.

### Rejected: high-resolution waitable timers and thread memory priority (W3a)

Two scheduling levers from the W3a brief that turn out to have nothing to act on.

**`CreateWaitableTimerExW(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION)`** (Win 10 1803+) gives roughly
0.5 ms wait granularity against the 1 ms that `timeBeginPeriod(1)` already buys. It would only
matter on a thread whose deadline is timer-driven, and the frame path has none: the WGC path is
event-driven end to end (DWM signals the frame pool, the callback runs, the ring hands the slot to
an encode thread over a channel), and the DXGI path's wait is `AcquireNextFrame`'s own kernel wait,
not a sleep. The three timed waits that do exist are the 200 ms keepalive, the ring encode
thread's tick and the repeater's tick, and none of them is precision-sensitive — the keepalive is
allowed to be a millisecond late by construction. Adding a waitable timer would add a handle per
thread and change nothing measurable.

**`SetThreadInformation(ThreadMemoryPriority)`** raises a thread out of a low memory priority.
The default for a normal process is already `MEMORY_PRIORITY_NORMAL`; the call is a no-op unless
something lowered it first, which nothing here does. The neighbouring `ThreadPowerThrottling`
information class *is* worth setting and is now set (row 109's sibling commit) — but for a
different reason: Windows 11 throttles threads of a minimised or occluded window, which is the
ScreenExtend window's normal state while streaming.

### Frame-pool depth: 3 for the ring, 2 for the inline handler, and the number behind that (W3a)

Row 96 made the depth per-path. The brief asked for 2 vs 3 to be measured on the ring path
specifically. What the after-matrix gives instead of an A/B is the counter that makes the A/B
unnecessary:

`frames_dropped_stale` — frames the pool discarded while draining to the newest — reads **5, 0 and
3** across the three 30 s NVENC-ring trials, against 321-421 delivered frames. At depth 3 the pool
is essentially never holding a backlog, because the ring's callback does a `CopyResource`, a
`Flush` and a channel send and then returns: single-digit microseconds, not milliseconds. There is
no queue for depth 2 to shorten.

What depth 2 would change is the failure case rather than the normal one. With two buffers the
compositor has exactly one spare while the callback holds the other, so a callback that is late
once — a scheduler preemption, a GPU hiccup during the staging copy — has nowhere to put the next
frame and the compositor stalls. With three there is a frame of slack, and the drain makes sure
that slack costs freshness rather than latency: the extra buffer is discarded, not queued, and the
discard is counted. That is the trade OBS and Sunshine make in the other direction because their
consumer *encodes* in the callback and therefore genuinely occupies a buffer for milliseconds —
which is exactly the inline path here, and exactly why it is set to 2.

So: 2 where the consumer is slow, 3 where it is fast and the extra buffer buys jitter tolerance
the drain then discards for free. Re-measure if `frames_dropped_stale` ever climbs into the tens
per second on the ring path, which would mean the callback had stopped being cheap.

### Kept at 4: the encoder-to-viewer broadcast capacity (W3a)

`BROADCAST_CAPACITY = 4` on both producers. A tokio broadcast never back-pressures — a receiver
more than four frames behind has the oldest value evicted and gets `RecvError::Lagged(n)`. The
open question was whether 4 is right, with one argument for raising it (a slow write batch should
not count as lag, and with `maxNumRefFrames = 1` any skip breaks the reference chain) and one for
lowering it.

Measured: **`frames_dropped_broadcast` is zero in all twelve runs of the after-matrix above** — the
counter is not printed at all, and the telemetry summary only prints non-zero counters. That spans
three encoders, frame counts from 289 to 886 per 30 s, and a machine under 10-17 concurrent
`rustc` processes, which is a much harsher scheduling environment than a real host. The RTP writer
never fell four frames behind.

Kept at 4, and not only because nothing hit it. Raising it is the wrong direction for this
workload: a video receiver four frames behind should skip to the newest frame, not work through a
backlog of stale ones — the same reasoning that made the WGC frame pool drain to newest (row 96).
A deeper buffer converts a scheduling hiccup from "one dropped frame plus a keyframe request" into
"four frames of accumulated latency the viewer then has to live with". The consumer side already
handles the drop correctly: `Lagged` goes through the rate-limited `KeyframeRequester` rather than
requesting an IDR unconditionally, and it cuts the bitrate at the same time so the keyframe that
does go out is smaller.

Revisit if `frames_dropped_broadcast` is ever seen non-zero on a real host with several clients —
that is the case this measurement does not cover, since the self-test has exactly one loopback
viewer.

### The classic WGC path still encodes in the frame callback, and why that is different from DXGI

Row 106 decoupled capture from encode on the DXGI path. The classic (non-ring) WGC path — which is
what an Intel, AMD or software encoder gets — still runs the whole encode inside
`on_frame_arrived`, i.e. on the WGC delivery thread, holding the handler mutex.

Left alone deliberately, because the two situations are not the same shape. DXGI's
`AcquireNextFrame`/`ReleaseFrame` is a strict pair with no buffering: while the encode ran, the
duplication was simply not being read, so the compositor's next present had nowhere to go and the
achievable rate really was `1 / (capture + encode)`. WGC has a frame pool in front of the callback,
and with rows 96's per-path depth and drain-to-newest, a slow callback costs *stale frames*, which
are counted in `frames_dropped_stale` — not compositor stalls. The consumer falls behind and then
catches up on the newest frame, which for a screen-share is the correct thing to lose.

Extending the ring to the non-NVENC backends would still be worth doing, and it is a bigger change
than it looks: the ring is built around NVENC's shared keyed-mutex input textures, so a generic
version needs the slot-texture ring that `DxgiRing` now is, plus a decision about which device owns
the slots for each backend. `DxgiRing` is the shape to lift. Not attempted here because the
measurable half of it — the DXGI path — is the one that could not be measured on this machine
anyway (see the DXGI note above), so there would have been no number either way.

### The AirPlay tier now has a working end-to-end harness, and what it settled

`cargo test --lib macos_utils::airplay::tests::live_end_to_end -- --ignored --nocapture` runs
against the real machine — it publishes Bonjour, drives the Displays menu through Accessibility,
lets macOS create a display, un-mirrors it, captures the mode ladder, tears it down, and then does
the whole thing a second time. It had never been run. It runs green on macOS 10.15.8 (Intel)
**over ssh, against a logged-in console session**, which is worth knowing because the ssh process
cannot see displays itself: `CGGetActiveDisplayList` returns nothing to it, so
`macos_utils::airplay::topology`'s display tests had to learn to skip rather than fail.

Four things it settled that no amount of reading could:

1. **The mirror-sink blocker is real and is fixed** (rows 23 and the harness numbers there).
2. **The mode ladder macOS publishes for an AirPlay display**, exactly:
   `800x600, 1024x768, 1280x720, 1280x1024, 1440x900, 1680x1050, 1920x1080`, all 60 Hz, plus a
   degenerate `1x1`. That confirms `sender_prefs`'s hardcoded 1920x1080/60 ceiling, which had been
   asserted from one measurement, and it is the ladder any DPR>1 client falls off.
3. **macOS is not flooding us.** The open question was whether the advertised `maxFPS = 1` is
   honoured, or whether a live session means the host encodes its whole desktop into a socket we
   discard, competing for the GPU with our own VideoToolbox encode. On an idle display it plainly
   is not: **1.0 packets/s, 25 KiB/s**, and those are type-`0x05` stream reports, not video. The
   busy-display case is still unmeasured.
4. **No video packets arrive at all when nothing is drawn on the display** — zero type-`0x00` and
   zero type-`0x01` in a 12 s session. So the two experiments that need video traffic (the
   compositor-to-consumer latency histogram, and the ten-minute "is the payload really ciphertext?"
   check) need a console session with a window on the second display. The instrumentation for both
   is in the tree behind `SE_AIRPLAY_TIMING=1`; see
   `findings/speculative-airplay-sender-timestamps-are-received-and-discarded.md`.

One bonus find: the type-`0x05` stream report is a **plaintext `bplist00` binary plist** carrying
the sender's own statistics — `VrttAvg` is visible in the first 32 bytes — arriving once a second
and currently counted and dropped. The `plist` crate is already a dependency of this module.
### Audio silence costs 3 bytes, not a payload

Measured with `probe --audio` on an idle host: a CELT silence frame encodes to **3 bytes**, so
the 13-byte header dominates and the whole idle audio stream is ~16 B x 200/s = **3.2 kB/s**
before SCTP/DTLS/UDP/IP framing. `speculative-audio-win-dtx-off-so-silence-is-fully-encoded-and-sent`
worried about the byte cost; the byte cost is negligible, and the *packet rate* is the only
real consideration. Header-only silence keepalives would cut 200 packets/s to 10, but they need
client-side gap fill (W11) and would interact with the worklet's silence-boundary drift
correction, so this is recorded rather than acted on. Note also that the process-loopback tier
never sets `AUDCLNT_BUFFERFLAGS_SILENT` at all — it delivers real zeros — so on that tier the
silence shortcut does not fire and the client never sees `FLAG_SILENT`.

### `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` reads backwards from its meaning

It does not select the default sample-rate converter. Microsoft documents it as selecting "a
sample rate converter with **better** quality than the default conversion but with a **higher
performance cost**". That is the right choice for audio humans will hear, but it is a filter
with group delay and it is only ever reached on a non-48 kHz endpoint. Now commented at the
constant rather than left to read as its opposite.

### windows-rs `PROPVARIANT` must not be dropped over a borrowed blob

`ActivateAudioInterfaceAsync` takes its parameters as a `VT_BLOB` PROPVARIANT whose
`pBlobData` points at caller-owned memory, exactly as the API requires. Letting that
`PROPVARIANT` go out of scope **killed the process outright** — after a fully successful
activation, which made it read as though the activation had failed. `std::mem::forget` on the
variant is the fix and leaks nothing (it is a stack value whose only pointer is into the same
frame). Recorded because the symptom points nowhere near the cause.

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


### W12 measurement notes

The render and IPC numbers in rows 5 and 6 come from a temporary harness added for this pass and
removed before commit: a proxy around the `Store` instance counting method calls, a per-component
render counter, and a driver that emitted real `deviceJoin` / `deviceModify` / `deviceRemove`
events and fired a toast, reporting through the new `log_frontend_error` command with
`logbus::set_verbose(true)` forced on in the debug build so the lines reached the terminal.

Two things could not be intercepted: `window.__TAURI_INTERNALS__` and its `invoke` property are
both read-only in Tauri 2, so a global invoke counter is not possible from the webview. Counting
at the `Store` boundary measures the thing that actually mattered instead.

`entry -> dashboard mount` measured **588 ms** cold and **401 ms** on a warm reload, but that is a
`pnpm tauri dev` figure -- unbundled ES modules served by Vite over HTTP against a debug Rust
build -- and says nothing useful about a production launch. It is recorded so a later production
measurement has something to be compared against methodologically, not numerically.

### W12 close-out: per-chunk bundle sizes

`pnpm build` output before and after the close-out pass (branch `w12-closeout`, base
`latency-overhaul` at `0e50445`). Raw size, gzip in brackets. Only the chunks that moved are
listed; `monaco-setup` (2,957.05 kB / 765.79 kB), the two monaco workers, the fonts and the icon
chunks are unchanged.

| chunk | before | after | delta |
|---|---|---|---|
| `main-*.js` (eager entry) | 565.27 kB (181.44) | 566.74 kB (181.96) | **+1.47 kB** (+0.52) |
| `main-*.css` | 52.92 kB (9.33) | 50.43 kB (9.07) | **-2.49 kB** (-0.26) |
| `devices-*.js` | 76.54 kB (24.37) | 76.90 kB (24.50) | +0.36 kB |
| `settings-*.js` | 52.77 kB (15.36) | 53.17 kB (15.51) | +0.40 kB |
| `dashboard-*.js` | 31.06 kB (12.02) | 30.89 kB (11.99) | -0.17 kB |
| `config-json-editor-*.js` | 32.78 kB (11.63) | 32.94 kB (11.70) | +0.16 kB |
| `index.html` | 2.75 kB (1.07) | 4.22 kB (1.70) | +1.47 kB |
| **total moved** | **814.09 kB** | **815.29 kB** | **+1.20 kB** (+0.15%) |

The pass was not a size pass and it did not become one: the shipped bytes are flat to within a
fifth of a percent. What moved is where work happens rather than how much of it is downloaded --
the eager chunk grew by the theme paint hint, the `qrValues` equality check, three `React.memo`
wrappers and the walkthrough's event plumbing, and the stylesheet shrank because two components
tailwind was still scanning are gone. `index.html` grew by the background paint, the theme hint
script and the font preload link, all of which are on the critical path *before* any of those
assets are fetched.

The one large size win available was measured and not taken: stubbing out `NextStepProvider` and
`NextStepReact` drops the eager chunk to **420.56 kB** (gzip 135.15), i.e. `nextstepjs` + `motion`
are **146.01 kB / 46.75 kB gzipped, 25.8%** of it. See recommendation 2 below for why it is a
visible change rather than a free one.

### W12 UI-visible recommendations (not applied)

The engagement rule is that the desktop UI stays element-for-element and pixel-for-pixel identical.
These would pay off and were deliberately not made. Items 1-4 are from the first W12 pass; 5-9 came
out of the close-out, which resolved every remaining `Workstream: W12` finding. The findings
themselves are deleted -- this list is what survives them.

1. **Drop the `!important` from the global `button, input` transition.** Component-level
   `transition-colors` utilities on nine kit components are currently unreachable, and
   `src/index.css`'s `.monaco-editor input { transition: none !important }` counter-patch exists
   only to undo the global rule. Removing `!important` would let those utilities take over at
   Tailwind's **150 ms cubic-bezier**, against the current **200 ms ease-in-out** -- a visible
   change to every hover fade in the app. Upside: one fewer counter-patch and per-component control
   of transitions; no measured latency difference, since row 12 already removed the expensive half.
   Filed as `findings/minor-ui-global-button-transition-still-overrides-the-kit.md`.

2. **Lazy-load `nextstepjs` + `motion`.** They are the largest non-React item left in the eager
   chunk, and the upside is now **measured**: replacing `NextStepProvider` and `NextStepReact`
   with pass-through stubs and building takes `main-*.js` from **566.74 kB to 420.56 kB**
   (**-146.01 kB, -25.8%**; gzip 181.96 -> 135.15 kB, **-46.75 kB, -25.7%**). That is the largest
   single win left in the desktop UI.

   It was not taken because `NextStepReact` is not merely an ancestor by convention -- it renders
   two real DOM elements around the app (`div[data-name=nextstep-wrapper]{position:relative;
   width:100%}` wrapping `div[data-name=nextstep-site]{display:block;width:100%}` wrapping
   `children`). The `position: relative` wrapper is the containing block for every
   `position: absolute` descendant that has no closer positioned ancestor, `#innerSidebar` among
   them. So neither of the finding's two routes is free: hoisting `NextStepReact` out from around
   the router deletes those elements, and `React.lazy` with a matching fallback unmounts and
   remounts the whole app when the chunk resolves, losing every piece of React state with it.
   The honest version is a swap that is signed off as a visible change, or a fork of the library
   that portals its overlay instead of wrapping. The tour runs at most once per install.

3. **Fix `tooltip.tsx`'s Tailwind-v4 class syntax.** Five arrow-positioning classes
   (`data-[side=left]:top-1/2!` and siblings, plus `z-60`) generate no CSS under Tailwind 3, so the
   tooltip arrow is not vertically centred on side-anchored tooltips today. Correcting the syntax
   *moves the arrow*, which is exactly the kind of change the rule forbids without sign-off. The
   existing finding `minor-ui-tooltip-tailwind-v4-syntax-dead-classes.md` is left open.

4. **The webfont's `serif` fallback.** `src/index.css` sets
   `font-family: "Geist Variable", serif`, so the pre-swap frame is Times -- at Geist's metrics,
   so the swap is a full-page reflow, not just a colour change, and it lands on the `text-4xl`
   sidebar headline where it is most obvious. `pre`, `code`, `kbd` and `samp` are in the same
   rule and want a monospace fallback rather than a serif one. Fixing the stack changes what the
   user sees during the first frames, so it was not applied. The invisible half of that finding
   *was*: the font now has a `<link rel=preload>` (row 94), which shortens the window without
   changing what is in it. Suggested replacement, for whoever signs it off:

   ```css
   html, body, input, textarea, button, select, optgroup {
     font-family: "Geist Variable", -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
   }
   pre, code, kbd, samp { font-family: ui-monospace, Menlo, Consolas, monospace; }
   ```

5. **`index.html`'s global chrome-suppression scripts.** The `click` listener calls
   `preventDefault()` on *any* click anywhere in the document with Ctrl, Cmd, Shift or Alt held.
   Alt+click is Monaco's multi-cursor gesture, Ctrl/Cmd+click is add-cursor / go-to-definition,
   and Shift+click is the standard range-extend for text and list selection; the Settings JSON
   editor is a full Monaco instance. `contextmenu` is suppressed globally, which also removes
   cut/copy/paste and spellcheck from every text input, with no in-app replacement menu. And the
   reload guard checks `event.ctrlKey && event.key === "r"` but never `metaKey`, so Cmd+R is not
   blocked on the one platform whose WKWebView is most likely to honour it. Narrowing the click
   guard to `event.target.closest("a[href]")`, exempting `.monaco-editor, input, textarea,
   [contenteditable]` from the context-menu guard and adding `metaKey` to the reload guard all
   change behaviour the user can observe, so none was applied. `Element.closest` is Safari 9+,
   so the narrowed form is safe on the floor. Was
   `minor-build-index-html-blocks-modifier-clicks-globally.md`.

6. **An open hosted network is announced by a toast, not a consent prompt.** When macOS refuses
   every secured `securityType`, `start_hosted_network` falls back to an unencrypted AP and the
   only signal is a destructive toast. The toast is transient; the network is not -- it stays up
   for the session, and the dashboard shows it exactly as it shows a secured one, while the join
   URL, session id and OTP are readable by anyone in range who associates. The fix is a
   confirmation before the fallback starts anything plus a persistent badge while it is up; the
   backend half is one branch, since `start_via_host_ap` already knows it is about to drop to
   `SECURITY_OPEN`. Both are new UI. Also W6's territory. Was
   `minor-ui-an-open-hosted-network-is-announced-by-a-toast-that-disappears.md`.

7. **`bootstrap.tsx` offers "Install drivers" on macOS, where the command is a `true`-returning
   stub.** `runSetup(true)` reacts to a failed `setup()` by calling `installDrivers()`, sleeping
   five seconds and retrying -- on macOS that is a guaranteed no-op followed by an identical
   failure -- and the error dialog renders a button whose copy tells the user to press it.
   Gating both on `type() === "macos"` removes a control and changes the timing of an error
   screen. Upside: five seconds off every macOS setup failure and a dialog that says something
   true. `setup` returning a reason rather than a bare `bool` is the other half, and that is a
   command-surface change across all three `*_utils` modules. Also W6's territory. Was
   `minor-ui-bootstrap-offers-install-drivers-on-macos-where-it-does-nothing.md`.

8. **The sidebar collapse animates `width`.** `#sidebar` transitions 275px to 0 over 300 ms, and
   `width` is a layout property: every frame reflows the sidebar *and* the sibling flex child,
   which is the whole page content -- the QR carousel, the device table, the settings form. It
   cannot be composited, and it runs on the main thread of a process that is simultaneously
   capturing, scaling and encoding video. Animating `transform: translateX(-275px)` with a
   matching `margin-left` on the content pane, or a `grid-template-columns` transition, keeps the
   sidebar's own subtree off the layout path -- but the sidebar currently *clips* from the right
   under `overflow-hidden` while a transform would *slide* it, which is a different animation.
   The cheap half was applied (row 94): the second, nested `transition-all duration-500` on
   `#innerSidebar` now names the one property that actually flips. Was
   `minor-ui-sidebar-animates-layout-properties.md`.

9. **Two error paths report the wrong thing.** `buildQrValues` returns `[]` when
   `getNetworkAdapters()` throws, and the dashboard renders an empty list as "Join or Create a
   Network (none were found)" -- the wrong message for an IPC failure. `ensurePermissions`
   returning `false` means "nothing was missing", so a permission *check* that failed is treated
   as all-granted and startup proceeds. Both now put the reason on the log bus (row 93), but
   fixing what the user is told is a copy change in the first case and a new blocking state in
   the second. Was part of `minor-ui-swallowed-errors-and-silent-fallbacks.md`.

### W12 close-out: the boot verification, and what it caught

`pnpm tauri dev` was run once at the end of the pass on the merged branch, with
`logbus::set_verbose(true)` forced on in the debug build (a temporary patch, reverted before the
commit) so the frontend's `console.error` forwarding printed to the terminal instead of only
reaching the log ring. The app reached the dashboard with `setup()` succeeding, the streamer bound
to the LAN adapter, HTTP :8080 / HTTPS :8443 listening and the cloud session registered.

Exactly one frontend line appeared in the whole boot, repeated: React's
`Encountered two children with the same key, 172.16.11.81` from the Past Devices table. That is
row 95. It is worth recording as a method rather than just a fix -- the release build has no
devtools, so every React warning of that class was previously unobservable, and routing
`console.error` to the log bus (which landed earlier in W12) is what made a terminal run report it.
Anyone touching `src/` should do the same run before declaring a pass finished.

Two backend lines in the same boot are pre-existing and belong to other workstreams, noted here
only so the next reader does not re-discover them: `SetProcessDpiAwarenessContext failed (likely
already set): Access is denied` and `MMCSS registration failed for a transport thread; transport
threads will run unprioritised`.

### W12 close-out: what was tried and rejected, and what is deliberately unchanged

**`build.sourcemap: "hidden"` -- rejected.** A production stack trace from the boot-failure panel
in `index.html` is minified and close to useless, and that panel exists precisely for the macOS
10.15 floor where debugging is hardest. But `hidden` emits maps for every chunk, and the monaco
chunk alone is 2.96 MB minified; the maps would ship inside the bundle resources for a payoff that
only materialises when someone can also retrieve them. The right shape is emitting maps in release
CI and keeping them as a build artifact rather than in the app, which is `build-release.yml`'s
business, not the vite config's.

**`eslint-plugin-react` / `jsx-a11y` -- not added.** Both would need new devDependencies, and the
engagement's frontend rule is no new dependencies unless unavoidable. The type-aware half of that
finding, which needs no new package, *was* done (row 93). The a11y gap it names is real and
unaddressed: `src/layout/layout.tsx`'s sidebar toggle is a `<div onClick>` with no `role`, no
`tabIndex` and no `onKeyDown`, so it is not keyboard-reachable. Fixing that is a behaviour change
on a control the user can see, so it belongs with the recommendations above.

**Base UI: a decision is owed, not a change.** `src/components/ui/tooltip.tsx` is the only file in
the kit that uses `@base-ui/react`; every other component is still Radix. Base UI 1.6.0's shared
internals reach for `inert` (Safari 15.5), `Element.checkVisibility` (17.4), `CSS.registerProperty`
(16.4), `Object.hasOwn` (15.4) and `Array.prototype.findLast` (15.4). The current bundle is clean --
tree-shaking keeps all of it out -- so this is a future hazard, not a live break. `inert` is the one
that matters: it is in `floating-ui-react/utils/markOthers`, which every focus-managing Base UI
component pulls in (Dialog, AlertDialog, Menu, Select, Popover, Combobox -- exactly the components a
continued migration would move next), and it degrades *silently*: focus simply is not trapped and
nothing errors. `scripts/check-legacy-webkit.mjs` now has a check for it, anchored on the write
(`setAttribute("inert"`, `.inert =`, `toggleAttribute("inert"`) so that react-remove-scroll's prop
of the same name is not a false positive; the other four were already covered. The unused
`@radix-ui/react-tooltip` is gone (row 83). What remains is a maintainer decision -- either Base UI
is the direction, and that means raising the desktop-UI floor or shipping an `inert` polyfill
deliberately, or the tooltip goes back to Radix. Discovering it one component at a time is the
option to avoid. Was `speculative-ui-base-ui-migration-drags-post-floor-apis.md`.

**The tooltip's Tailwind-v4 classes stay broken on purpose.** Recommendation 3 above covers it: the
five arrow-positioning classes generate no CSS under Tailwind 3, so correcting the syntax *moves the
arrow*. That is the change the appearance rule forbids without sign-off, and the brief named this
case explicitly.

### The bundled indirect display driver: what a rebuild would buy

**A request for the maintainer, not a code change.** The shipped `VirtualDisplayDriver.dll` is a
locally built fork of `MolotovCherry/virtual-display-rs` and its INF pins
`UmdfExtensions = IddCx0102` - IddCx 1.2, which shipped in Windows 10 1709. This project's stated
floor is Windows 10 2004 (19041), which carries IddCx 1.5, so four things are being left on the
table for compatibility we do not claim:

1. **`IddCxAdapterSetRenderAdapter` (IddCx 1.4).** Below 1.4 the OS composes the indirect
   display's desktop on the POST adapter - normally the Intel iGPU on a hybrid laptop - and if
   that adapter is PnpStopped it falls back to WARP, in software, on the CPU. On the dev box that
   means DWM composes on Intel and NVENC encodes from NVIDIA, taking the cross-adapter copy the
   pipeline already names. At 1.4+ the driver names its own render adapter and never lands on
   WARP.
2. **`IddCxMonitorUpdateModes` (available since IddCx 1.0, unused upstream).** The fork's
   `ipc.rs::notify()` implements every mode change as `IddCxMonitorDeparture` +
   `IddCxMonitorCreate` + `IddCxMonitorArrival` - a full hot-unplug and re-plug, with a comment
   claiming Windows requires it. It does not. This is the whole reason the DPR ladder has to be
   pre-registered at create time.
3. **`IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE` (IddCx 1.4).** Left at `NONE`, so OS virtual modes
   are active and a resolution change on our monitor may be DWM scaling rather than a real
   modeset. Measured here: a display created at 1920x1080 came up at **1280x720**, a mode we
   never registered, and a refresh-rate change was refused at that size and accepted at 1920x1080.
4. **`IddCxMonitorSetupHardwareCursor`**, and on Windows 11 21H2+
   `IDDCX_ADAPTER_FLAGS_PREFER_PRECISE_PRESENT_REGIONS` (better dirty-region tracking, which the
   dirty-rect-aware capture path would use directly) and `IddCxSetRealtimeGPUPriority` (22H2).

Rebuilding the fork needs the WDK and a signing story, and
`findings/major-build-bundled-idd-driver-provenance-is-wrong.md` has to land first, since nobody
but the original dev box can currently reproduce the binary.

### What the virtual display does with refresh rates, measured

`cargo test --lib report_selected_refresh_rate -- --ignored --nocapture` on Windows 11 26200:

- A newly created monitor comes up at **1280x720@60**. Windows picks its own default, *not* the
  highest rate we advertised, so registering 120 Hz alongside 60 costs nothing. That settles
  hypothesis 2 of the now-deleted `speculative-vdisplay-win-refresh-rate-ladder-fixed-at-60-120`
  finding: false.
- At 1280x720 - not one of our registered modes - a refresh-rate change is refused with
  `DISP_CHANGE_BADMODE`.
- At 1920x1080, registered with `[60, 120]`, the change is accepted and `EnumDisplaySettingsW`
  reads back **1920x1080@120**. `DM_DISPLAYFREQUENCY` must travel with
  `DM_PELSWIDTH`/`DM_PELSHEIGHT` or it is refused even at a registered mode.

So the compose-cadence work (`major-vdisplay-win-decouple-idd-refresh-from-stream-fps`) is
directly implementable: `virtual_display::set_refresh_rate(device_name, 120)` after the existing
`set_display_mode`, with `current_display_mode` as the read-back. A rate outside
`[requested, 60, 120]` is *not* serviceable on a live display and needs the display recreating.

### What the HostAP SPI actually does on the 10.15.8 box

`cargo test --lib macos_utils::hosted_network::tests::report_hostap_surface -- --ignored --nocapture`
on the Intel MacBook, macOS 10.15.8, interface `en3`:

```
interfaceMode    : CWInterfaceMode(1)          (Station)
powerOn          : true
responds : startHostAPModeWithSSID:securityType:channel:password:error:   true
responds : stopHostAPMode                                                 true
responds : startHostAPMode:                                               true
responds : enableHostAPMode                                               true
responds : disableHostAPMode                                              true
responds : startIBSSModeWithSSID:security:channel:password:error:         true
responds : stopIBSSMode                                                   true
backend          : HostAP
5 GHz channels   : 36 40 44 48 52 56 60 64 100 104 108 112 116 120 124 128 132 136 140 149 153 157 161 165
2.4 GHz channels : 1 … 11
would pick       : 36
```

So on the supported floor the private SPI is not merely present but accompanied by four more
HostAP selectors nothing in the tree has ever called — `startHostAPMode:` (no SSID, no security
type, which plausibly reads both from the Internet Sharing preferences and would get WPA2 for
free) and the `enableHostAPMode` / `disableHostAPMode` pair. Both are worth trying on a modern Mac
before concluding that WEP is the ceiling.

**Update (W6/W14 close-out): the AP was started here after all**, by running the test binary
detached with a watchdog that power-cycles the radio, so the box comes back on its own about ninety
seconds later. `measure_hostap_start` on 10.15.8 Intel, `en3`, reproduced twice with identical
numbers:

```
backend HostApSpi, channel 36 (5 GHz), station address before the start 172.16.11.81
securityType 1024: accepted=false in 2.13s
securityType  512: accepted=false in 2.08s
securityType  256: accepted=false in 2.07s
securityType  128: accepted=true  in 2.21s, ap_mode=true at 2.21s
  t+5s … t+45s   mode_is_ap=true  ipv4=None
```

Three results, and the second is the one that matters.

**`securityType 128` is accepted**, and brings the radio to HostAP mode in 2.21 s. It is the first
new HostAP security constant this project has found, and it sits outside the `[-127, 127]` window
the 2012 sweep covered — which is precisely why the shipped ladder starts at 1024 and works
downward rather than repeating that sweep. The three values above it are each refused, taking about
2.1 s to say so, so the ladder costs ~6.5 s before it lands.

**Nothing serves DHCP.** With the AP up and `interfaceMode()` reading HostAP for 45 s straight,
`en3` held no address of its own at any point in the window. So on this floor
`startHostAPModeWithSSID:securityType:channel:password:error:` brings up a radio and not a network:
the SSID is visible, and a client that associates gets no lease and cannot reach the host. That is
a concrete shape for "slow and randomly fails", and it makes `startHostAPMode:` — the no-argument
variant, which plausibly reads the Internet Sharing preferences and would therefore bring `bootpd`
with it — the most valuable remaining experiment in this territory.
`docs/manual-tests/macos-hosted-network.md` §9.

**The station address survives the switch**, which is a bug the measurement caught in the
verification added alongside it: at t+2.2 s, with the interface already in AP mode, `getifaddrs`
still reported `172.16.11.81`, the address of the network the Mac had just left. Read naively that
is "verified, host address 172.16.11.81" — the AP reported as working, on the LAN address.
`wait_for_ipv4` now takes the pre-start address and refuses it.

**Still not established here:** what security `128` advertises. That needs a scan from a second
device during the 45 s window; `netsh wlan show networks mode=bssid` from the Windows box did not
surface the SSID, which is inconclusive rather than negative — Windows serves cached scan results
and will not force a fresh scan while associated. Since the AP serves no DHCP, a client could not
join whatever it advertises, so the DHCP question is the blocking one and this is the one behind
it. Association, throughput and the channel A/B all sit behind the same gate.

### `refreshDeadline` was probed and deliberately not set

`CGVirtualDisplaySettings` gained `refreshDeadline`, `rotation` and `isReference` somewhere
between 10.14.6 and the present. `report_settings_surface()` now asks `respondsToSelector:` for
all three and logs the answer once, so a field report says which levers existed on that machine.

None is set. `refreshDeadline` is the only latency-named knob anywhere on the WindowServer side of
this boundary and is the most interesting thing in the private surface, but its units and
semantics are unpublished, the class family is known to have crashed WindowServer on Sequoia
betas, and the property does not exist on the 10.15 floor — so there is no machine here on which
setting it could be measured. Shipping a value chosen by guesswork into WindowServer is not a
trade worth making; the probe and the finding are what carry it forward.

`rotation` is more immediately useful and equally unverifiable here: where it exists it should
replace the transposed-width/height modes this commit registers for orientation flips. Both stay recorded here;
the finding file is closed, since neither can be taken further without a Mac at 13.0 or newer.
### Rejected: feeding measured loss into `OPUS_SET_PACKET_LOSS_PERC`

`major-audio-core-no-fec-on-an-unreliable-datachannel` proposed driving the encoder's
packet-loss-percentage from the link loss the BWE already computes. Not done, and not because
the number is unavailable — it is, and the redundancy switch now uses it.

The encoder is **host-wide**. There is one `AudioHub`, one capture, one `OpusEncoder`, fanned
out to every audio-enabled device; loss is **per session**. Feeding one session's loss into a
shared encoder degrades the stream for every other device on the host, and with two devices on
different links there is no correct value to write. Loss robustness therefore belongs on the
per-session transport, which is where `FLAG_REDUNDANT` sits.

The lever that *is* encoder-wide and does apply is `OPUS_SET_PREDICTION_DISABLED`: it makes
frames independent regardless of who is listening. It is wired as a config field defaulting off
because it costs bitrate at fixed quality and the trade needs a listening test — see
`docs/manual-tests/windows-audio-tiers.md`.

### What the AirPlay mirror stream was actually carrying (W14 close-out)

The tier's four remaining findings were all "speculative" and all about the same connection: the
one macOS opens to push H.264 at the fake receiver, which `mirror.rs` frames and discards. Running
`macos_utils::airplay::tests::live_end_to_end` with `SE_AIRPLAY_TIMING=1 SE_AIRPLAY_MAX_FPS=30` on
the 10.15.8 box answered all four, and the earlier attempts had failed only because nothing was
ever drawn on the AirPlay display, so macOS never started its encoder.

**Type-`0x00` payloads are ciphertext.** This was the explicit falsification test the decode
finding asked for: dump the first bytes of a video payload and look for a NAL start code or an
AVCC length prefix. Three separate first-32-byte dumps, three different sessions:

```
0b ac 16 1b 96 f5 7b 83 4a ca df e3 bc 5e 5b 73 ...
b5 9f 42 cc 13 84 66 a6 f8 71 66 84 4c 71 e2 c8 ...
bd 7c 59 5d ed 9e 3a db 2c 08 a6 d2 ff f5 81 3a ...
```

No start code, no length prefix that matches the framed payload length, uniform high entropy. So
forwarding or decoding the mirror stream needs real FairPlay key recovery — the `ekey`/`eiv` and
`streamConnectionID` derivation this module deliberately does not implement — exactly as the
finding assessed. That half is settled by measurement rather than by argument.

**Type-`0x01` payloads are not**, and they are more useful than expected:

```
01 64 00 29 ff e1 00 1d 27 64 00 29 ac 13 16 50 1e 00 89 f9 70 06 e0 20 20 dd 81 80 02 ee
```

That is an `AVCDecoderConfigurationRecord` in the clear: configuration version 1, profile `0x64`
(**High**), level `0x29` (**4.1**), 4-byte NAL length prefixes, one 29-byte SPS. It is the only
first-party answer anywhere in this project to "what does Apple's own AirPlay encoder emit", and it
cost nothing to read.

**The header stamp is not a wall clock.** Read little-endian its seconds field was 41457 and then
41459 two seconds later, while the machine had been up fourteen hours — a monotonic clock whose
epoch is somewhere inside this boot. Read big-endian the same eight bytes are eleven and a half
years, which is what makes the byte order decidable without a specification. The previous probe
compared it against `SystemTime::now()` in NTP form and correctly refused to pick an endianness,
because neither reading was plausible; it just had no way to say why.

The consequence: the *absolute* compose-to-consumer figure needs AirPlay's own NTP time sync (the
sender queries a UDP timing port to learn the offset), which this receiver does not implement and
should not start implementing for a measurement. What does not need the epoch is the spread, since
both clocks are the same machine's monotonic time and the offset between them is constant. Over 66
frames at 30 fps with a window being dragged on the AirPlay display:

```
p50 26.96 ms   p90 43.90 ms   p99 67.43 ms   max 67.43 ms   (excess over the fastest frame)
```

A second session, 18 frames: p50 52.48, p90 64.34, p99 67.90. At 30 fps a frame period is 33 ms, so
macOS's own AirPlay path carries one to two frame periods of variability before anything of ours is
involved. The log line says "excess over the fastest frame in the batch" in full, because a bare
number here would read as an absolute latency and would be a fabrication.

**`maxFPS` is honoured.** The open question from the discovery report was whether a live AirPlay-tier
session means the host is encoding its whole desktop at full rate into a socket we discard. It is
not: at the shipping `maxFPS = 1` the drain reports 1.0 packets/s and 25 KiB/s, and at
`SE_AIRPLAY_MAX_FPS=30` it reports 14.2 packets/s and 271 KiB/s with `encoderCurrentFPS=30` in the
sender's own report. The sender tracks what we ask for.

**The stream reports are a second free instrument.** Type-`0x05` is a ~181-byte binary property list
followed by a 25,000-byte trailer, which is why handing the whole payload to a parser failed. Its
contents, on this box:

```
encoderCurrentFPS=30 lossAvg=0.000 queuedFramesAvg=0 rttAvg=0
sentFramesAvg=0 txCapacityAvg=8000000.000 txUsageAvg=0.000
```

macOS's own view of its own link, once a second, in the clear. `txCapacityAvg = 8 Mbit/s` is the
sender's idea of what the path will carry.

### Rejected: relaying macOS's own AirPlay mirror stream instead of capturing and encoding (W14)

The idea was to skip our CGDisplayStream capture *and* our VideoToolbox encode by forwarding the
H.264 macOS already hands us. Three independent reasons, now with the evidence attached rather than
inferred:

1. **It requires circumvention we will not ship.** The payloads are AES-CTR ciphertext (measured
   above). `fairplay.rs` carries four canned handshake replies and no key material; recovering the
   key means vendoring playfair's derivation tables, which are the part of that project that
   actually circumvents rather than replays.
2. **It would not be faster.** The relay swaps `CGDisplayStream -> VideoToolbox -> RTP` for
   `WindowServer capture -> WindowServer's VideoToolbox -> AES-CTR -> TCP -> our decrypt ->
   repacketize -> RTP`, and the measurement above says Apple's own path already carries a p99 of
   ~67 ms of variability before the extra hop. AirPlay's own `POST /stream` advertises
   `latencyMs: 90` as the sender's budget, so Apple is not targeting anything tighter than we are.
3. **It gives away every control the overhaul depends on.** No loss-based bitrate driver, no
   on-demand IDR for a rejoining client, no QP or GOP or profile — the bitstream says High 4.1
   because that is what macOS chose, not because anything asked.

Plus the structural cap: this tier is below the 10.15 floor and `max_concurrent_displays()` is
`Some(1)` because macOS owns the display. Not built, and the file recording the decision is folded
into this section rather than left open.

### Rejected for now: an AWDL transport, with the number that stopped it (W14)

`dnssd.rs` is the only Bonjour binding in the tree and already takes an interface index and a flags
word, so the question "could ScreenExtend advertise itself somewhere a nearby Apple device sees it
with Wi-Fi off" costs three registrations to ask.
`macos_utils::airplay::dnssd::tests::probe_awdl_registration` on 10.15.8:

```
primary LAN interface, no AWDL:   accepted (index 7,          flags 0x8)
any interface, IncludeAWDL:       accepted (index 0,          flags 0x108)
InterfaceIndexP2P, IncludeAWDL:   rejected (index 4294967294, flags 0x108)
                                  DNSServiceErrorType -65540
```

So `kDNSServiceFlagsIncludeAWDL` is accepted on the supported floor, and
`kDNSServiceInterfaceIndexP2P` — the registration that would prove a peer-to-peer-only
advertisement — is refused outright with `kDNSServiceErr_Unknown`. That is as far as this hardware
can take it: the decisive observation is `dns-sd -B _screenextend._tcp` on an iPhone with Wi-Fi
off, and there is no second Apple device here that can be driven non-interactively.

The direction was already doubtful on its own terms — a browser cannot open an AWDL socket, so the
only shape that could work is binding the existing HTTPS/WebRTC server to the `awdl0` address for
an Apple client's Safari, and AWDL time-shares the radio with the infrastructure link, which adds
the jitter the availability-window research describes. The constants, the flags parameter and the
one-shot reply pump are in the tree, so picking this up later is a caller away rather than a
rewrite. Nothing passes them.

### The Wi-Fi network name on macOS: not asking for Location, deliberately (W6)

`CWInterface.ssid` is documented to return nil on 10.15+ without Location Services authorization,
and the finding offered two options: ask for it, or stop pretending and label the adapter honestly.

**Decision: do not ask.** A Location prompt is one of the heaviest consents macOS shows, and asking
for it to decorate a dashboard row that already carries the adapter's addresses is not a trade
worth making. The row reads "Wi-Fi" on a machine that has never granted it, on purpose, and
`wifi_ssid` now says so once in the log so it does not read as a bug next to the Windows dashboard.

Worth recording, because it contradicts the finding's premise: on this 10.15.8 box the SSID *is*
readable. `macos_utils::networking::tests::report_wifi_naming_and_radio` returns
`adapter index 7 named "Sreenilayam-5g"`. So the nil case is not universal on 10.15 and the cache
that was added to stop the name flapping is doing nothing here; the honest statement is that the
name is available on some 10.15 machines and not others, and the code handles both without asking
for anything.

### Rejected: changing the virtual display's declared physical size and vendor ID (W6)

`create_cg_virtual_display` declares `sizeInMillimeters` from a fixed 96 DPI — a Windows
convention, where macOS's reference is 72 pt/in — and a placeholder vendor/product pair
(`0x3456`/`0x1234`) shared by every display. The serial half of that finding is fixed (row 191).
The other two are not, and deliberately:

- **The physical size.** Whether it is inert or load-bearing is exactly what cannot be established
  here: `time_a_retina_join` and every other experiment that creates a display and inspects how
  macOS scales it needs a console session, and the finding itself says "only after measurement".
  Changing an EDID field that may feed WindowServer's HiDPI decision, blind, on the one thing this
  module exists to do, is a regression risk with no measurement to justify it.
- **The vendor ID.** A distinct one is not something a project can simply choose: PNP vendor IDs
  are assigned by VESA/UEFI, and inventing one collides with whoever holds it. The serial is what
  varies per device and it now does.

### The one-hop link, and the channel A/B: both blocked behind the same gate (W6)

Two findings wanted the same session on the same hardware: make the host-AP link a first-class,
*measured* low-latency mode, and settle whether sitting on an AWDL social channel is better or
worse for jitter than avoiding one. Both are closed here without their numbers, and the reason is a
measurement rather than an absence of one.

The comparison each needs is "client associated to the host AP" versus "client and host on the
router", with p50/p95/p99 glass-to-glass on both. On this floor there is no such thing as the first
configuration: the AP `startHostAPModeWithSSID:` produces holds no address and serves no DHCP (45 s
of `ipv4=None`, above), so nothing can associate and get a lease, so neither comparison has a left
column. This is not the dev box being single-radio — that was worked around by running the
measurement detached — it is the AP itself being unjoinable. Every downstream measurement in
`docs/manual-tests/macos-hosted-network.md` §3, §4 and §5 sits behind §9's question: does
`startHostAPMode:`, with Internet Sharing configured, bring `bootpd` with it.

What did land from those two findings, and does not need the gate: the backend, channel, band and
security type are logged on every start, `SCREENEXTEND_HOSTAP_CHANNEL` A/Bs a channel without a
rebuild, `PREFERRED_5GHZ_CHANNELS` excludes 6/44/149 with a unit test pinning it, and the watcher
now reports the *associated* channel and whether `awdl0` is up so a field report carries the
condition. The choice to avoid the social channels remains what it was: the conservative default,
not a measured one, and the comment in the source says so rather than implying a result.

### Two things in the display settle that were deliberately left alone (W6)

The settle finding carried three items. The event-driven wait is row 221; the other two are not
done, and not by omission.

**`create_display_with_modes` still logs and swallows an `activate_mode` failure.** Returning it
would turn "the display came up at the wrong geometry" into "the join failed", and the client that
would then see nothing is one that today gets a working stream at a size it did not ask for. The
right shape is the finding's own second suggestion — attach it to the session as a warning so the
server can retry or tell the client — and that is a change to the session state and the client
protocol, not to this function. The log line already says exactly what happened.

**The settle wait still runs inside `DISPLAY_CORRELATION_LOCK`.** The lock exists to make the
"which new monitor appeared?" correlation window unambiguous, and once the display id is known the
geometry wait does not need it — but the lock is `streamer/server.rs`'s, its extent is W1's to
decide, and shortening it from the platform side would be guessing at an invariant defined
somewhere else. Now that the wait is event-driven rather than a poll, what it holds the lock for is
also much shorter than it was.

`time_a_retina_join` was attempted here anyway, because CoreGraphics turned out to be answering
over ssh in this session — `CGGetActiveDisplayList` returns the real display list, both macOS
display tests run their assertions rather than skipping, and the AirPlay tier creates and tears
down real displays. It still hangs: the test was killed by its watchdog after 150 s with no output
past the test name, exactly as the finding recorded. So the split is sharper than "ssh sees no
displays" — **read-only CoreGraphics works over ssh; `CGVirtualDisplay`'s `initWithDescriptor:` and
`applySettings:` block forever without a WindowServer connection.** That is why row 221's saving is
UNVERIFIED, and it will stay so until someone runs the A/B from a Terminal window inside the
console session.

### Dyld safety of the W6/W14 close-out

Four link-time symbols are new in this work, and all four are floor-present. Checked against the
authoritative test `scripts/check-macos-dyld.sh` itself uses — presence in the 10.15 SDK's `.tbd`
stubs, which are generated from the dylibs that shipped with that SDK:

```
_CGDisplayRegisterReconfigurationCallback -> MacOSX10.15.sdk/…/CoreGraphics.tbd
_CGDisplayRemoveReconfigurationCallback   -> MacOSX10.15.sdk/…/CoreGraphics.tbd
_CFRunLoopRunInMode                       -> MacOSX10.15.sdk/…/CoreFoundation.tbd
_kCFRunLoopDefaultMode                    -> MacOSX10.15.sdk/…/CoreFoundation.tbd
```

Everything else the close-out added is `libc` (`PF_ROUTE` sockets, `poll`, `recv`) and already
linked.

The whole-binary run then confirmed it. `scripts/check-macos-dyld.sh` on a release build of
`mac-closeout2` (all of the above plus the AirPlay geometry change), against the 10.15 SDK:

```
floor 10.15 · undefined 555 · ok 555 · weak imports 0 · allowlisted 0 · offenders 0
PASS: every strongly-bound undefined symbol exists at 10.15.
```

### W6 / W14 UI-visible recommendations (not applied)

- **The hosted network's `NotStarted` / `Advertised` / `Verified` tri-state in `AppState`.** Both
  platforms now collect everything the type needs — `hosted_network_status()` on each side carries
  the backend, SSID, channel, band, security type, host address and (on macOS) the associated peer
  — and the macOS watcher already promotes on the first client from the AP's own subnet and
  diagnoses at 60 s. What is not done is replacing `AppState.hosted_network_running: Mutex<bool>`
  itself and widening `is_hosted_network` to return the state. That is a command-surface change
  across all three `*_utils` modules, a `bindings.ts` regeneration, and a new thing for the
  dashboard to display — three visible states where there is one toggle today. It belongs to
  whoever is allowed to change the UI.
- **IPv6 join URLs in the desktop dashboard.** `buildQrValues` (`src/lib/utils.ts`) filters
  adapters with `/^\d{1,3}(\.\d{1,3}){3}$/` and returns nothing for an adapter with only IPv6
  addresses, so on an IPv6-only network the host now serves but the dashboard shows no QR code.
  The fix is one predicate and a bracketed URL, and it adds a card to a page this engagement may
  not change the appearance of. The Rust side already prints the bracketed URL through the log bus
  and the CLI.
- **Surfacing the hosted network's channel, band and security type.** They are logged on every
  start and reachable through `hosted_network_status()`; putting them on the network card would
  make the "why will my phone not join this" question answerable without the log terminal.

### What the restored CI deliberately does not do (W13a)

The 2026-08 `ci.yml` that was deleted four minutes after it was added had five jobs. Three are
back in a recognisable form (`frontend`, the `rust` matrix, `blob-checksums`). Two are not, and
the reasons are here rather than in a commit message because they will be asked again.

**`cargo-audit` and `pnpm audit --audit-level=high --prod` are not restored.** Dependency
management on this repo is manual by an explicit decision — Dependabot was removed wholesale on
2026-09-06 (config, 24 PRs, 24 branches, 51 runs, alerts) and is not to be re-added. A blocking
audit job is the same mechanism through a different door: an advisory published upstream against
a transitive dependency turns every unrelated pull request red, on a schedule nobody chose. If
the appetite for it returns, the right shape is a `schedule:` job that opens nothing and blocks
nothing, not a gate on the merge path.

**A Linux `cargo check --target x86_64-unknown-linux-gnu` job is not restored.** `linux_utils/`
is a scaffold whose functions mostly `bail!`; nothing ships from it and nothing is regressed by
breaking it. Gating merges on a target no user runs buys a red build for a platform that is out
of scope. It belongs with whoever picks Linux up, along with the finding that the target does
not currently build at all.

**The macOS SDK is not pinned, only the deployment target.** `SDKROOT` is an absolute path,
which makes it wrong on every machine but the one that wrote it, and no hosted runner carries a
10.15 SDK to point it at. The portable equivalent is to audit the artifact: `scripts/
check-macos-dyld.sh` on a release binary, with the strong `.tbd`-stub verdict on the dev box and
the header-availability half — which is what actually catches a newly linked framework API — on
CI against the runner's own SDK. See `docs/manual-tests/macos-build.md` for what each half is
worth.

**The perf gate's tolerance is 20%, not the 5% originally specified, and it takes the best of
three runs.** The same release binary measured `p95_encode` anywhere from **4405 to 9067 us** on
the dev box across runs, entirely according to how many other `rustc` processes were on the
machine — a 2.06x spread with nothing changed in the code. A 5% gate on a hosted runner, which
is a shared VM and no quieter, would be red on almost every pull request and would be turned off
within a week, which is exactly the history this whole job is trying not to repeat.
`scripts/perf-gate.mjs` therefore accepts several `--input` files and uses the **lowest** reading
per metric — the one least polluted by contention; a mean or a median would fold the runner's
mood into the number — and the tolerance is set from the residual spread of those minima. Both
choices are documented in `ci/perf-baseline.json` so the next person tightening it knows what
they are trading.

**The perf gate is Windows-only.** Not a judgement about macOS: the only timing any test prints
is `p95_encode=<n>us` from the x264 unit test, and libx264 is compiled into the Windows backend
alone. The four macOS encoder tests assert bitstream structure and time nothing, and
`probe --encode` `bail!`s off Windows. Giving `encode_synthetic` the same per-frame timing is
what extends the gate — `findings/minor-build-perf-gate-cannot-cover-macos-no-test-prints-a-timing.md`.

**A hardware job for the Windows encoders is not possible on hosted runners.** `macos-smoke`
covers the modern macOS tiers because a hosted Mac has a real VideoToolbox. There is no hosted
Windows runner with an NVENC-capable GPU, so NVENC, Quick Sync and AMF stay on the dev boxes and
in `docs/manual-tests/`.

### W13b UI-visible recommendations (not applied)

Two changes that would help and that the engagement's no-UI-change rule forbids.

**A "Continue anyway" action on the permission gate.** `blocks()` in `src/pages/bootstrap.tsx`
already counts only `p.required && p.state === "denied"`, so an *unknown* read no longer holds
anything up and Accessibility never did. What is left is the deliberate block: if the Screen
Recording probe reads `denied` and the user is convinced it is wrong — the commonest real case
being a TCC grant bound to a different code identity, where System Settings shows a checkmark
over a grant that does not apply — the modal's only exits are Quit, Re-check and Restart. There
is no way to say "start anyway and let capture fail on its own", which is what every shipping
macOS capture product surveyed in `docs/research/macos-permissions-tcc.md` §3 does: they open,
mark the feature unavailable, and prompt at the point of use.

Capture failure is already the ground truth and already reports itself —
`ensure_screen_recording_access()` logs a `[tcc]` line at every pipeline start and the
per-device error path surfaces it — so the escape hatch costs nothing in diagnosability. It
needs a fourth, secondary-styled button in the permission `AlertDialogFooter` that clears
`permReport` and calls `proceed()`. Worth gating on the state where it is actually useful
(Screen Recording denied *and* at least one Restart already attempted) so first-run users are
not invited to skip a permission they simply have not granted yet.

**Raising the permission modal when consent lapses mid-session.** The watchdog added in this
pass (row: "A session watchdog re-reads `screen_recording_live()`…") names the revocation in
the log and drops the connected device, which is the half that can be done behind the pixels.
The other half is telling the user on the host at the moment it happens, rather than leaving
them to find the log. That needs a new event (`ScreenRecordingRevoked`, say) in
`collect_events!`, a subscription in `App.tsx`, and the permission gate re-opening over
whatever route is mounted — a real UI flow, not a message string.

### W13b: what was tried and rejected, and what is deliberately unchanged

**`dead_code = "allow"` stays crate-wide.** Flipping `[lints.rust] dead_code` to `warn` lights
**62 sites across 28 files**, measured on this branch. The concentrations are
`streamer/{congestion,pacer,telemetry,audio}` (18), `windows_utils/driver_ipc` (15, a vendored
fork) and the encoder backends (7) — modules other workstreams are in the middle of. With
`clippy --all-targets -- -D warnings` as the commit gate, turning it on is a crate-wide build
break for code this change does not own, and scoping the allow to `windows_capture` alone
(the finding's fallback) covers only 15 of the 62. The measurement is now recorded next to the
line in `Cargo.toml` so whoever picks it up after the tree settles starts from a number.

**`bundle.resources` was tried for the macOS audio package and rejected.**
The obvious fix — adding `resources/ScreenExtendAudio.pkg` to `bundle.resources` in
`tauri.macos.conf.json` — breaks a plain `cargo check` on macOS:

```
resource path `resources/ScreenExtendAudio.pkg` doesn't exist
error: failed to run custom build command for `screenextend v0.6.3`
```

`tauri-build` validates `bundle.resources` in the **build script**, so every cargo invocation
on a tree without the (gitignored) package fails — every other Mac workstream, the `rust` job
in `ci.yml`, and the dyld-floor job. Two things this established that were not written down
anywhere: `tauri.<platform>.conf.json` is **auto-merged by platform**, so the
`--config src-tauri/tauri.macos.conf.json` in release CI is redundant rather than load-bearing
(the failure above happened with no `--config` at all); and the resource list is enforced far
earlier than bundling. The package is therefore carried by `bundle.macOS.files`, which the
bundler copies at packaging time, plus an explicit CI assertion that it is inside the built
`.app`. A missing package still fails the release, which is the behaviour the finding wanted —
it just no longer fails everything else.

**`// @ts-nocheck` stays on `bindings.ts`.** Removing it leaves exactly one error:
`src/lib/bindings.ts(255,13): error TS6133: 'TAURI_CHANNEL' is declared but its value is never
read`. That is a tauri-specta generator artifact against `noUnusedLocals`, and the only ways to
clear it are hand-editing a generated file or relaxing the compiler flag for the whole project.
`scripts/check-bindings.mjs` is the better guard and does not depend on the file being
type-checked: it cross-checks the `collect_commands!`/`collect_events!` lists in `lib.rs`
against what the generated file exports, so a stale debug binary that silently reverts the API
surface fails CI. It was negative-tested by deleting a command and an event from `bindings.ts`.

**The legacy installer subcommand names stay.** `installdrivers`, `removedrivers` and
`hostednetwork` are now error-reporting rather than panicking, but they are not deleted and
`windows/hooks.nsh` / `windows/fragment.wxs` still call them by those names. Changing the hooks
to `drivers remove` is a **compatibility break for installed copies**: an old uninstaller
invokes `removedrivers` on whatever binary is present, so the name has to survive at least one
release cycle after the hooks change. That is a release-sequencing decision, not a code change,
and it is deliberately left for whoever cuts the release that makes it.

**`devices set` applies live through `RELOAD`, but the GUI's in-memory copy is not refreshed.**
A CLI edit now reaches a running host's `AppState`; if that host is the GUI, its frontend still
holds the `devices[]` array it read at startup and will overwrite the file on its next edit.
Closing that needs the host to push a config-changed event to its own webview, which is a
frontend change. The headless `serve` case — the one the CLI exists for — has no such copy and
is fully correct.

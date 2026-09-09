# `guides/` — plan

**This file is a planning record, not a guide.** It is the specification the five guides in this
folder were written from, plus the final coverage audit (§7). Nobody runs it on a machine; it is
kept so the next person can see what was inventoried, what was placed where, and what was
deliberately left out.

Each guide in this folder is a **standalone autonomous directive** handed to a fresh Claude Code
instance running on hardware or an OS that ScreenExtend's two development machines could not
exercise. The reader of a guide has **no access to the chat, the orchestrator, or any other
guide** — everything that instance needs must be inside its own file.

The two development machines, for reference throughout:

| Machine | Spec | What it can and cannot prove |
|---|---|---|
| Windows host | Windows 11 build 26200 (25H2), RTX 3080 Ti Laptop + Intel UHD (hybrid graphics), NVIDIA driver 580.97, IDD virtual-display driver installed | WGC ≥ 26100 only (DXGI duplication returns `DXGI_ERROR_UNSUPPORTED` on this box); NVENC, Quick Sync same-adapter, x264; endpoint + process loopback audio. **No AMD GPU. No pre-26100 build. No working DXGI path. No browser client.** |
| macOS host | macOS 10.15.8 Catalina, Intel, Command Line Tools only, no Xcode, no Homebrew, git 2.24, reached over SSH, SIP disabled | CGDisplayStream + VideoToolbox (non-low-latency), the legacy audio driver tier, AirPlay receiver harness, HostAP read-only surface. **No ScreenCaptureKit (12.3+), no Process Tap (14.2+), no low-latency rate control (11.3+), no LTR, no arm64, no signing identity, no second Mac, no console session.** |

---

## 1. Provenance of this inventory

- Ledger state read: the working tree at **`3a137a7`** plus the uncommitted drift then present in
  `docs/OPTIMIZATIONS.md`, `docs/CAPABILITY-TIERS.md` and `docs/manual-tests/transport.md` — i.e.
  the content that the orchestrator subsequently committed as **`2c1c2d3`**, on top of which
  `w1-closeout` merged as **`b01a4ce`**.
- **Everything the inventory listed as unmerged has since merged.** As of **`b333707`** the branch
  `latency-overhaul` carries all of it, and §7's coverage audit is taken against that commit:
  - **W1 close-out** (`b01a4ce`) — rows 179–187, 204–209, 271, 285–288 and
    `streamer/{packetizer,negotiate,sockets,repair,keyframe,congestion,pacer}.rs`.
  - **W5** (macOS capture/encode) — rows 138–155, 280–284, 292–296. `screencapturekit_available()`
    now probes by `AnyClass::get`, `sck.rs` carries `QUEUE_DEPTH`/`minimumFrameInterval` and reads
    the SCK presentation timestamp, `encoder.rs` carries the `HwPolicy` ladder and
    `VTCopySupportedPropertyDictionaryForEncoder`, and `capture_rate` /
    `capture_rate_downscaled` / `the_sps_promises_no_reordering` all exist.
  - **W11-followup** — client trickle ICE and the fast-channel `u16` sequence (rows 285, 286);
    `CLIENT_DEGRADED_NAMES` is now 23 entries ending in `no_trickle_ice`.
  - **W15** (tests) — `crate::test_support::skip` / `skip_because` with `SE_REQUIRE_HW=1`, and
    `pnpm test:client` (`scripts/test-client.mjs`, seven harnesses).
  - **W13b** (hygiene) and **W4b** (AMD, `9efbd7f` → merged at `b333707`) — rows 256–265, 298–304,
    `amd/sps.rs`, `AsyncEncodeRing`, `serve --encoder`.
- Sources parsed: `docs/OPTIMIZATIONS.md` (252 ledger rows: **70 `COMPILE-CHECKED`**, **54
  `UNVERIFIED`-only**, plus every "considered and rejected — cannot measure here" note section),
  `docs/CAPABILITY-TIERS.md` (105 tier rows), all 13 `docs/manual-tests/*.md`,
  `docs/RISK-REGISTER.md` §7 (R-227…R-248), `docs/BASELINE.md` (empty/queued matrix cells and
  the "What could not be measured" table), and the "Verification plan → cannot be verified on
  the dev boxes" section of every `docs/research/*.md`.

### Ledger defects found while inventorying (fix these before the guides ship)

1. ~~**Rows 220–226 are byte-identical duplicates of rows 189–195**~~ — **fixed.** The duplicate
   block has been deleted; only 189–195 survive. The inventory below still cites both numbers
   ("190/221", "193/224") so that a citation found in an older document resolves; edit 189–195.
2. `docs/RESULTS.md` is referenced by `findings/_STATUS.md` and **still does not exist**.
   `clients-network-and-glass-to-glass.md` §4.4 tells its reader to create it with that guide's own
   sections rather than attempting a whole-engagement results document.
3. ~~`docs/BASELINE.md` macOS cells read `queued`~~ — **fixed.** W5 filled the macOS capture and
   capture→encoded rows; the numbers the guides quote (capture → encoded p50 13.61 / p99 15.06 /
   max 20.64 ms at 1440×900@60) come from there.

---

## 2. The guide set

Five guides. Every inventory item lands in exactly one. The floors are pinned deliberately high
so that a single run closes several tiers; the ceilings exist only where a newer OS removes or
changes the API under test.

| # | Slug | Min OS | Max OS | Hardware | Items |
|---|---|---|---|---|---|
| G1 | `modern-mac.md` | macOS **14.2** | none (26 / Tahoe explicitly wanted) | Apple Silicon Mac preferred; + an iPhone/iPad; Part B needs an Apple Developer Program identity | 28 |
| G2 | `intel-mac.md` | macOS **12.3** | macOS **12.7.6** | **Intel** Mac, dual-GPU (integrated + discrete) strongly preferred — a 2018–2019 15"/16" MacBook Pro is the target machine | 10 |
| G3 | `windows-amd.md` | Windows 10 **2004** (19041) | none | AMD GPU with a VCN/VCE **AVC** encoder (any Radeon/Ryzen APU since Raven Ridge/Vega **except** Navi 24 = RX 6400/6500); Adrenalin driver installed | 13 |
| G4 | `windows-legacy.md` | Windows 10 **2004** (19041) | build **22631** (strictly below 26100) | Hybrid-graphics laptop (Intel iGPU + discrete) where DXGI duplication actually works; a hosted-network-capable Wi-Fi adapter; the in-box *High Definition Audio Device* driver; a non-English display language; a phone to join with | 21 |
| G5 | `clients-network-and-glass-to-glass.md` | host: Windows 11 **26100+** with an NVIDIA GPU **or** macOS **14.2+** | none | A device matrix (iOS old + new, Android old + new, desktop Firefox/Safari/Chrome, two pen devices, a Bluetooth sink), a Wi-Fi 5 AP + a Wi-Fi 6 AP, a second network + a real TURN server, `clumsy`, `iperf3`, a 240 fps camera | 34 |

**Total inventory: 106 items.** Two items could not be placed (§5).

### Why each bound

- **G1 min 14.2** — the Core Audio Process Tap floor. Pinning there also gets ScreenCaptureKit
  video (12.3+), ScreenCaptureKit audio (13.0+), VideoToolbox `EnableLowLatencyRateControl`
  (11.3+, *not* 11.0 as the old gate claimed), VideoToolbox LTR (12.0+), the Ventura+ System
  Settings deep link, the Sequoia (15+) permission re-consent model, and — if the machine is
  Apple Silicon — the arm64 input ABI and native universal builds. **No max**: macOS 15/26 is
  where CGDisplayStream is *obsoleted* in the SDK and where BetterDisplay reported WindowServer
  crashes with the same private `CGVirtualDisplay` API, so a Tahoe run is worth more, not less.
- **G2 12.3–12.7.6** — the only band where ScreenCaptureKit video exists but ScreenCaptureKit
  *audio* does not, so the shipped audio path is the ScreenExtend Audio driver on a supported
  modern OS. 12.3 is also SCK's first release, which is where Chromium documented the
  "stream starts and delivers nothing" failure the 1500 ms commit gate exists for. Ceiling
  12.7.6 because at 13.0 SCK audio takes over and G1 owns everything above.
- **G3** — no ceiling; the AMF question is the GPU, not the OS. Navi 24 is excluded because AMD
  removed the AVC encoder from it, so step 2 failing there is correct behaviour, not a bug.
- **G4 max 22631** — build 26100 (Windows 11 24H2) is where `MinUpdateInterval` and
  `DirtyRegionMode` appear on `GraphicsCaptureSession`; the dev box is 26200 and already covers
  everything at or above 26100. `IsBorderRequired` is 20348 and `IsCursorCaptureEnabled` is
  19041, so 19045 and 22631 sit on *different* property tiers and both are worth having.
  This is also the last band where `netsh wlan hostednetwork` still works on shipping Wi-Fi
  drivers (deprecated after 1607, removed in practice by modern drivers), which is the only
  place hosted-network tier 3 can be exercised at all.
- **G5** — the constraint is the client and the network, not the host. The host must be a
  configuration this project has already verified so that a bad number is attributable to the
  client or the link.

---

## 3. Inventory

Columns: **id** · **what** · **files** · **OS/hardware floor (and ceiling where one exists)** ·
**how to verify** (the log line, probe command or test name that produces the answer) ·
**source** (ledger row `R<n>`, capability tier `T<line>` by its subsystem name, risk-register
id, manual-test section).

### G1 — modern Mac (macOS ≥ 14.2, Apple Silicon preferred)

| id | what | files | needs | how to verify | source |
|---|---|---|---|---|---|
| M1-01 | ScreenCaptureKit video tier: `queueDepth` 3→8, `minimumFrameInterval` 0.9/fps, `SCFrameStatusStarted` accepted, presentation timestamp, colour matched to CGDS, `stop()` blanks the delegate ivar | `macos_utils/streamer/sck.rs`, `macos_utils/streamer/mod.rs` | 12.3+ | log `capture: backend = screencapturekit`; `cargo test --release --lib macos_utils::streamer::pipeline::tests::end_to_end_latency -- --ignored --nocapture` | R152, R153, T*Capture backend (macOS)*, manual `macos-capture-encode.md` §1 |
| M1-02 | VideoToolbox session-spec ladder rung 1 (low-latency RC + `RequireHardwareAcceleratedVideoEncoder`) and rung 3; which encoder VT then picks under `HwPolicy::Forbid` | `macos_utils/streamer/encoder.rs` | 11.3+ | log `low_latency=true` and `hardware=true` in the per-session encoder line | R280, T*VideoToolbox session spec*, manual §2/§3 |
| M1-03 | The modern encoders' `VTCopySupportedPropertyDictionaryForEncoder` key sets, at the session's real frame size | `macos_utils/streamer/encoder.rs` | 11+ | the per-session property line (set / rejected(status) / no-symbol / available-but-not-requested) | R141, R155, T*VideoToolbox property support* |
| M1-04 | Long-term reference frames: `EnableLTR` present, and whether enabling it is worth the client-ack half | `macos_utils/streamer/encoder.rs` | 12.0+ | `EnableLTR` in the supported-property list; then an A/B of IDR bytes vs repair bytes | R283, T*Long-term reference frames (macOS)*, manual §4 |
| M1-05 | Encoder back-pressure tier 1 against a **one-in/one-out** encoder (the 10.15 box only has the frame-holding kind, so only tier 2 was exercised); the after-numbers for rows 144/147 on modern hardware | `macos_utils/streamer/encoder.rs`, `pipeline.rs` | 11.3+ | `end_to_end_latency` p50/p90/p99; delivered fps must not fall | R144, R147, T*Encoder back-pressure (macOS)*, manual §7 |
| M1-06 | Three consecutive submit failures rebuild the encoder (not reproducible on demand here) | `macos_utils/streamer/encoder.rs` | any | force contention (start a QuickTime screen recording), then read the rebuild line | R148, manual §2 "scarce-resource case" |
| M1-07 | Core Audio Process Tap tier: aggregate `kAudioDevicePropertyBufferFrameSize` clamp, the tap-UID-must-match-`CATapDescription` silence failure mode, RAII teardown order | `macos_utils/audio/process_tap.rs`, `audio/mod.rs`, `audio/ring.rs` | **14.2+** | `audio: capture backend = process_tap`; `ScreenExtend probe --audio --duration 15` | R112, R113 (tap half), T*System audio capture (macOS)* rung 1, manual `macos-audio.md` tier 1 |
| M1-08 | ScreenCaptureKit audio tier: the 2×2/1 fps dummy stream, `showsCursor:false`, `queueDepth:3`, the 1.5 s async bridges, the teardown barrier | `macos_utils/audio/sck_audio.rs` | 13.0+ | `audio: capture backend = screencapturekit`; `probe --audio` | R117, R169, R170, T*System audio capture (macOS)* rung 2, manual `macos-audio.md` tier 2 |
| M1-09 | The tap's own `capture_ns` skew against the host clock — the number that decides whether the Windows play-cursor clamp is needed on this tier (macOS legacy measured p50 **−0.57 ms**, and the clamp was *rejected* there on that number) | `macos_utils/audio/process_tap.rs`, `streamer/audio/mod.rs` | 14.2+ | `probe --audio` prints the signed skew and the packet-spacing histogram | ledger note "macOS audio: what was rejected, and why (W7)" |
| M1-10 | Audio tier **ordering**: 13.x+ with Screen Recording denied must fall through SCK to an installed driver rather than having no candidate | `macos_utils/audio/mod.rs` (`capable_backends`) | 13.0+ | deny Screen Recording, install the driver, read `audio: <tier> backend failed to start (…); trying next` | T*macOS audio tier ordering* |
| M1-11 | Screen Recording authorization: the SCK probe tier, and 15/26 behaviour of the window-list read | `macos_utils/permissions.rs`, `macos_utils/streamer/sck.rs` | 13+ / 15+ | `ScreenExtend doctor --json` → `macos.probes`; time to gate close | R2, T*Screen Recording authorization (macOS)*, manual `macos-permissions.md` §1–§4 |
| M1-12 | System Settings deep link tier 1 (`com.apple.settings.PrivacySecurity.extension?Privacy_*`) lands on the right pane | `macos_utils/permissions.rs` | 13+ | click the button, observe the pane | T*System Settings deep link (macOS)* |
| M1-13 | App relaunch for a new grant via `open -n -a` (needs a real `.app` bundle; the dev box runs the bare binary) | `macos_utils/permissions.rs`, `lib.rs` | any, bundled | grant mid-run, confirm the new process is its own TCC responsible process | T*App relaunch for a new grant (macOS)* |
| M1-14 | macOS 15+ monthly re-consent, mid-session: the 30 s `screen_recording_live()` watchdog ends sessions on a definite `Some(true)→Some(false)` | `macos_utils/permissions.rs`, `streamer/session.rs` | **15+** | revoke while streaming; the sessions must end, not blank | R260, manual `macos-permissions.md` §7 + §8 |
| M1-15 | `AXUIElementSetMessagingTimeout(el, 0.0)` restore; `check_permissions`/`request_permission` async + `spawn_blocking` + the warmed window-list probe | `macos_utils/permissions.rs` | any | `doctor --json` probe timings; `check_permissions` wall time < 5 ms | R258, R259 |
| M1-16 | `input_backend` tiering, and the `NeedsAccessibility` rung with Accessibility revoked **mid-session** (grant must take effect within 500 ms, no relaunch) | `streamer/input/macos.rs`, `streamer/input/mod.rs`, `macos_utils/permissions.rs` | any | `CompatibilityReport.input_backend`; `doctor --json` | R203, T*Remote input injection*, manual `core-closeout.md` §2, `macos-permissions.md` §8 |
| M1-17 | **arm64 input ABI**: `CGEventCreateScrollWheelEvent2` (the non-variadic replacement) on Apple Silicon — the variadic call was an ABI blocker on arm64 | `streamer/input/macos.rs` | **Apple Silicon** | scroll from a client; both `#[ignore]`d input tests | R43, R-233 |
| M1-18 | The Ctrl↔Command remap driven by the client's reported platform, typed on a real Mac/iPad client | `streamer/input/macos.rs`, `streamer/input/scancode.rs` | any + a Mac/iPad client | type modifier combinations from each client OS | R66 |
| M1-19 | Virtual display: `create_display_with_modes` + HiDPI derived from the ladder (born at the client's DPR), multi-mode registration, the `CGDisplayRegisterReconfigurationCallback` settle saving, FNV-1a `setSerialNum:` | `macos_utils/virtual_display.rs` | 10.14.4+, **console session** | `cargo test --lib macos_utils::virtual_display::tests::time_a_retina_join -- --ignored --nocapture`; the settle duration line | R39, R190/R221, R191/R222, T*Virtual-display DPR / mode list (macOS)* |
| M1-20 | `CGVirtualDisplaySettings` extras — `refreshDeadline`, `rotation`, `isReference` — probed, and `refreshDeadline`'s effect measured. The only latency-named knob on the WindowServer side of that boundary | `macos_utils/virtual_display.rs` | 15+ ideally (absent on 10.15) | `respondsToSelector:` line; then an A/B of `capture delivery` p50/p90/p99 | T*`CGVirtualDisplaySettings` extras*, manual `macos-capture-encode.md` §10 |
| M1-21 | Does `CGVirtualDisplay` still work on 15/26 (BetterDisplay reported WindowServer crashes on Sequoia betas with the same private API), and does `CGDisplayStream` — obsoleted in the 15 SDK — still resolve through `dlsym`? | `macos_utils/virtual_display.rs`, `macos_utils/streamer/cgds.rs` | **15 / 26** | create a display and join; `nm -u target/release/ScreenExtend \| grep -i displaystream` must stay empty | T*CGDisplayStream symbols*, research `macos-capture-encode.md` |
| M1-22 | `set_server_ports` sends each client a `Bye` and restarts gracefully; the macOS adapter thread is named and its spawn failure reported | `macos_utils/mod.rs`, `streamer/server.rs` | any + a client | change the port with a client attached; no dropped connection | R42, R262 |
| M1-23 | The `PF_ROUTE` adapter watcher on a modern macOS (30 enumerations/30 s → 0) | `macos_utils/networking.rs` | any | `cargo test --lib macos_utils::networking::tests::measure_adapter_watch_cost -- --ignored --nocapture` | R41 (and the R189/R220 duplicate pair) |
| M1-24 | The **macOS hosted Wi-Fi network**, end to end: the HostAP SPI still present per machine; the security-type ladder (10.15.8 accepts **128** in 2.21 s and refuses 1024/512/256 after ~2.1 s each — what 128 *advertises* is unknown); association; **DHCP** (on 10.15.8 the interface never gets an address of its own, so the network is `Advertised`, not `Verified`); channel choice and AWDL; throughput; teardown; the two HostAP surfaces nothing has ever called | `macos_utils/hosted_network.rs`, `macos_utils/hostap.m`, `streamer/server.rs` | any modern macOS + an iPhone and an Android | `cargo test --lib macos_utils::hosted_network::tests::{report_hostap_surface,measure_hostap_start} -- --ignored --nocapture`; the `hosted network:` log lines | R38, R193/R224, R227, T*Hosted Wi-Fi network (macOS)*, T*security type*, T*liveness*, T*DHCP*, manual `macos-hosted-network.md` §1–§9 |
| M1-25 | `DISPLAY_CORRELATION_LOCK` released as soon as the backend names its own display; the >1 ms wait is logged | `streamer/server.rs` | any + two clients joining at once | the `display correlation lock held for … ms` line | R206 |
| M1-26 | AirPlay: `created_geometry` reported instead of discarded; `airplay::probe()` in `doctor`; whether macOS honours the receiver's advertised `maxFPS = 1`; whether a **modern** sender completes the fake-receiver handshake at all (`/pair-verify` is a hard 403) | `macos_utils/airplay/`, `macos_utils/virtual_display.rs`, `cli.rs` | any Mac + an iPhone or a second Mac as the AirPlay **sender** | `cargo test --lib macos_utils::airplay::tests::live_end_to_end -- --ignored --nocapture`; `stats.packets` per second | R284, R261, R-242, R-243 |
| M1-27 | **Part B (needs an Apple Developer Program identity).** Release artifact: `MACOSX_DEPLOYMENT_TARGET`/`LC_BUILD_VERSION` = 10.15 on a *release* build; `bundle.macOS.infoPlist`, `minimumSystemVersion` and the audio `.pkg` in `bundle.macOS.files`; the restored CSP; the universal (x86_64 + arm64) binary and a **universal** `libopus.dylib` (the shipped one is x86_64-only); dyld-floor gate tiers 2 and 3; the app's Developer-ID **designated requirement stable across releases** and a TCC grant surviving an update; translocation; the audio driver signed + notarized so `coreaudiod` loads it with **SIP on** | `src-tauri/.cargo/config.toml`, `tauri.macos.conf.json`, `src-tauri/Info.plist`, `src-tauri/Entitlements.plist`, `src-tauri/resources/`, `src-tauri/macos/ScreenExtendAudio/scripts/{build,package,sign_and_notarize}.sh`, `scripts/check-macos-dyld.sh` | any Mac + Developer ID Application **and** Installer certs; a clean VM for the grant-survival test | `otool -l`, `otool -L`, `codesign -d -r-`, `scripts/check-macos-dyld.sh`, `spctl -a -vv`, `probe::device_present()` | R79/R228, R264, R265, T*macOS 10.15 floor enforcement*, T*macOS audio driver load*, R-234, R-235, R-236, manual `macos-build.md`, `macos-app-signing.md`, `macos-audio-driver-signing.md` |
| M1-28 | **Part B, push-gated.** `.github/workflows/ci.yml` (six jobs) and `build-release.yml` have **never run** — validated by parsing YAML only. Includes the perf gate (`scripts/perf-gate.mjs`, `ci/perf-baseline.json`) and the blob-checksum job | `.github/workflows/`, `scripts/perf-gate.mjs` | a GitHub push the repo owner authorizes | a green run | R82 |

### G2 — Intel Mac, macOS 12.3–12.7.6, dual-GPU

| id | what | files | needs | how to verify | source |
|---|---|---|---|---|---|
| M2-01 | ScreenCaptureKit's **first-release** reliability: a stream that starts and delivers nothing (the failure Chromium documents for full-display capture on 12.3) and the 1500 ms first-frame commit gate that must catch it and fall back to CGDisplayStream without failing the session | `macos_utils/streamer/sck.rs`, `macos_utils/streamer/mod.rs::start_capture` | **12.3–12.7.6** | `capture: backend = …` plus the fallback line; `end_to_end_latency` must still produce frames | T*Capture backend (macOS)* frame gate, manual `macos-capture-encode.md` §1 ("12.3–12.x: either tier is acceptable; whichever starts must say so") |
| M2-02 | `PreferredEncoderGPURegistryID` taken from `CGDirectDisplayCopyCurrentMetalDevice` of the captured display — reads `gpu_registry_id=none` on every single-GPU machine, including Apple Silicon, so only a dual-GPU Mac can prove it | `macos_utils/streamer/encoder.rs`, `macos_utils/streamer/gpu.rs` | **dual-GPU Intel Mac** | the per-session encoder line's `gpu_registry_id=` | R143 |
| M2-03 | `NSSupportsAutomaticGraphicsSwitching` — whether the app actually stays on the integrated GPU rather than waking the discrete one | `src-tauri/Info.plist` | dual-GPU Intel Mac | `pmset -g` / gfxCardStatus while streaming; battery drain | R178 |
| M2-04 | Cross-GPU IOSurface migration and the discrete-GPU switch happening *under* a live capture | `macos_utils/streamer/{cgds,sck,gpu,encoder}.rs` | dual-GPU Intel Mac | force a switch mid-session; frames must not stall or tear | research `macos-capture-encode.md` R15 |
| M2-05 | VideoToolbox `InstanceLimit` / encoder contention on Intel: the ladder demotion must be **logged loudly**, never silent, when the hardware encoder is busy elsewhere | `macos_utils/streamer/encoder.rs` | Intel Mac | start a second hardware H.264 encode (QuickTime screen recording), read `hardware=false` at error level | research R14, manual `macos-capture-encode.md` §2 |
| M2-06 | The Intel hardware-encode **threshold table**: Intel Macs refuse hardware H.264 below roughly 640 wide / 480 tall. Sweep 320×240 / 400×300 / 568×320 / 640×360 / 640×480 / 1280×720 and record the rung that succeeded plus the read-back properties | `macos_utils/streamer/encoder.rs` | Intel Mac | `Encoder::new` per size, printing the rung and `UsingHardwareAcceleratedVideoEncoder` | `findings/major-vt-intel-macs-refuse-hardware-h264-below-640-wide-and-480-tall.md`, research R11 |
| M2-07 | The **ScreenExtend Audio driver** tier as the *selected* tier on a supported modern OS (12.x has no SCK audio and no Process Tap), including the shm layout-2 ring transport and the HAL-input fallback that must refuse a non-48 kHz device | `macos_utils/audio/legacy/`, `macos_utils/audio/mod.rs`, `src-tauri/macos/ScreenExtendAudio/src/shm_ring.hpp` | **10.15–12.x**, driver installed | `audio: capture backend = virtual_device`; `probe --audio --duration 15`; compare against the 10.15.8 numbers: capture→packet p50 **−0.57** / p90 3.59 / p99 4.54 ms, IO cycle 128 frames, **0 of 3277** `capture_ns` deltas anything but exactly 5.000 ms, shm `overruns` 0 | T*System audio capture (macOS)* rung 3, T*macOS audio tier ordering*, T*Legacy-tier capture transport*, manual `macos-audio.md` tier 3 |
| M2-08 | Volume keys while the ScreenExtend virtual device is the default output — the device's own Volume/Mute controls mirrored by `volume_proxy`, and the `CGEventTap` backstop | `macos_utils/audio/legacy/{volume_proxy,volume_keys}.rs`, `src-tauri/macos/ScreenExtendAudio/src/` | 10.15–12.x, driver installed + Accessibility | `volume_keys=device_volume_control` vs `event_tap_fallback`; press F10/F11/F12 | R116, T*macOS volume keys while the virtual device is default* |
| M2-09 | The legacy `.pkg` install flow end to end behind **one** admin prompt, on a machine that is not the dev box, including `coreaudiod` restart and default-output save/switch/restore + crash recovery | `macos_utils/audio/legacy/{installer,routing}.rs`, `src-tauri/macos/ScreenExtendAudio/scripts/package.sh`, `postinstall` | 10.15–12.x | install, reboot, kill the app mid-session, confirm the default output is restored | R115, manual `macos-audio.md` "Verifying the driver directly" |
| M2-10 | Power and idle assertions on a **laptop with a battery and a console session** (the dev Mac is headless over SSH): `power.rs` / `activity.rs`, and the thread QoS read-backs on Intel | `macos_utils/streamer/{power,activity,qos,tuning}.rs` | Intel MacBook | `pmset -g assertions` during a session; `thread_policy_get` read-back lines | R150 sibling, manual `macos-capture-encode.md` §6/§9 |

### G3 — Windows + AMD Radeon / Ryzen iGPU

| id | what | files | needs | how to verify | source |
|---|---|---|---|---|---|
| A-01 | The **whole AMF backend** encodes a frame for the first time: `AMFInit`, `CreateComponent(AMFVideoEncoderVCE_AVC)`, `GetCaps().GetAccelerationType() ∈ {HARDWARE, GPU}`, `Init()`, and an actual bitstream out | `windows_utils/streamer/amd/{mod,encoder}.rs`, `windows_utils/streamer/amd_sys/`, `windows_utils/streamer/pipeline.rs` | AMD VCN/VCE AVC part | `ScreenExtend probe --encode out_amd.h264 --encoder amd`; then `ffprobe -show_frames out_amd.h264` | R4, T*H.264 encode (Windows)* rungs 3–4, R-228, R-011, manual `amd-amf.md` §2/§4 |
| A-02 | Nothing is **linked** against AMF: `dumpbin /imports ScreenExtend.exe \| findstr /i amf` must be empty (the Windows analogue of the macOS `nm -u` rule) — otherwise the binary refuses to start on every non-AMD machine | build output | any Windows | `dumpbin` | manual `amd-amf.md` §1, research `amd-amf.md` §6 item 4 |
| A-03 | The **usage tier**: `USAGE = ULTRA_LOW_LATENCY` vs the `LOW_LATENCY` fallback — AMD's cheap probe (read `RateControlPreanalysisEnable` back) then Sunshine's blunt one (re-apply the whole property table). ULL breaks `Init()` outright on pre-RDNA parts from Adrenalin 23.7.1, and **neither branch has ever executed** | `windows_utils/streamer/amd/encoder.rs` | AMD, ideally one pre-RDNA and one RDNA2+ part | the `Usage=` / `RateControlMethod=` read-back line | T*AMF usage tier*, manual §3 |
| A-04 | **Encoder input format**: NV12 (via `scaler.rs::with_format`) vs BGRA + AMF's internal converter. AMD state BGRA input silently invokes an *uncontrolled* internal GPU colour converter that competes with the foreground app. `GetInputCaps → GetFormatAt(i, &fmt, &native)` says which formats are native; `CAP_COLOR_CONVERSION` says whether the internal convert is even accelerated. `retune_input_format` must re-`Init` once if the pipeline hands it another format | `windows_utils/streamer/amd/encoder.rs`, `windows_utils/streamer/scaler.rs` | AMD | the caps dump + an A/B of `encode` p50/p90/p99 | T*AMF encoder input format*, T*CPU-bridge readback format* (UNVERIFIED for AMF), R41, manual §11 |
| A-05 | The AMF **CPU bridge**: `encode_nv12_padded`, colour range as a function of input format, `OutMatrixCoeff` / `OutColorProfile` set | `windows_utils/streamer/amd/encoder.rs`, `pipeline.rs` (`Backend::AmfCpu`) | AMD | force the bridge, `ffprobe` the colour tags, compare `encode` percentiles against the same-adapter path | R245 |
| A-06 | Which probe-first properties actually exist: `LowLatencyInternal`, `OutputMode`, `InputQueueSize` writability, `EnableEncoderSmartAccessVideo`, `IntraRefreshMBsNumberPerSlot` | `windows_utils/streamer/amd/encoder.rs` | AMD | the property read-back log | research `amd-amf.md` §6 item 7 |
| A-07 | Does `IDRPeriod = 0` mean "never" or "every frame"? | `windows_utils/streamer/amd/encoder.rs` | AMD | `ffprobe -show_frames`, count `pict_type=I` | research §6 item 8 |
| A-08 | Does `QUERY_TIMEOUT` blocking beat sleep-polling, and by how much? | `windows_utils/streamer/amd/encoder.rs` | AMD | A/B of the encode-thread wait, p50/p90/p99 | research §6 item 9 |
| A-09 | Encode latency and the **tail**: target is the NVENC band (single-digit ms at 1080p, low teens at 4K). Watch the *max* over a 5-minute run — a creeping tail is a VBV/buffering regression | `windows_utils/streamer/amd/`, `streamer/telemetry.rs` | AMD | `ScreenExtend probe --whep-selftest --duration 30 --encoder amd` and `probe --live --duration 300 --encoder amd`; compare against `docs/BASELINE.md`'s NVENC row (capture→socket **1.57 / 8.39 / 16.78 / 18.00 ms**, post-W3a p90 3.93 / p99 6.82) | manual §5, `docs/BASELINE.md` |
| A-10 | What ULL's **free intra refresh** costs: ULL leaves `IntraRefreshMBsNumberPerSlot` at the usage's 255, and writing 0 is the only way to turn it off | `windows_utils/streamer/amd/encoder.rs` | AMD | A/B bytes-per-frame and encode time with 255 vs 0 | manual §10 |
| A-11 | Adaptive bitrate, on-demand IDR, leaks and restarts; the **concurrent-session cap** on AMD (`NumOfHwInstances` / `MaxNumOfStreams`) — the NVENC analogue measured **8** sessions on the RTX 3080 Ti with the 9th refused and the other eight unaffected | `windows_utils/streamer/amd/`, `pipeline.rs` | AMD + several clients | open sessions until refusal; confirm the refusal is identified, not reported as "no AMF" | manual §7/§8, T*NVENC concurrent sessions* as the model |
| A-12 | Cross-adapter AMD: an Intel-iGPU + Radeon-dGPU laptop — which adapter `D3D11CreateDevice` lands on, and whether the "zero-copy" share is actually a cross-adapter driver transfer | `windows_utils/streamer/pipeline.rs`, `capture.rs` | Intel iGPU + Radeon dGPU laptop | the `adapters: capture=… encode=… same_adapter=` line | manual §9, R-231 (AMD half) |
| A-13 | Re-check the two AMD **rejections** on real hardware: `OUTPUT_MODE = SLICE` sub-frame output (rejected on NVENC's measured ~2 ms ceiling) and whether a transient AMF encoder-busy status needs the `pipeline::is_transient_encode_error` phrase | `windows_utils/streamer/amd/encoder.rs`, `pipeline.rs` | AMD | measure slice-mode output latency directly; grep the log for the transient phrase under load | R246, R279 (NVENC analogue) |

### G4 — Windows 10 22H2 / Windows 11 23H2 on hybrid graphics

| id | what | files | needs | how to verify | source |
|---|---|---|---|---|---|
| L-01 | **WGC below build 26100**: each `GraphicsCaptureSession` property probed on its own with `ApiInformation::IsPropertyPresent` and skipped when absent, so a Win10/pre-24H2 host stays on WGC instead of hard-failing to DXGI. `MinUpdateInterval`/`DirtyRegionMode` are 26100; `IsBorderRequired` is 20348; `IsCursorCaptureEnabled` is 19041 — 19045 and 22631 sit on different tiers | `windows_utils/windows_capture/graphics_capture_api.rs`, `windows_utils/streamer/capture.rs` | **19041 ≤ build ≤ 22631** | `capture: backend = WindowsGraphicsCapture` + the feature-summary line matching the build | R102, R96, T*Screen capture (Windows)*, R-227, manual `windows-capture.md` §1–§3, §8 |
| L-02 | The **DXGI Desktop Duplication path, end to end** — on the dev box `\\.\DISPLAY1` is enumerated only by the dGPU and both entry points return `DXGI_ERROR_UNSUPPORTED`, so *every* DXGI row is compile-checked. `IDXGIOutput5::DuplicateOutput1` first, every adapter that enumerates the display tried, the RAII acquire/release guard, `DXGI_ERROR_INVALID_CALL` re-duplication capped at 20, the never-produced-a-frame bail at 40, incremental cursor compositing, the 3-slot staging ring + `dxgi-encode` thread, `AccumulatedFrames` counted, `ProtectedContentMaskedOut` reported | `windows_utils/streamer/dxgi/` | hybrid laptop where duplication **works** | `ScreenExtend probe --dxgi`, `probe --whep-selftest --capture-backend dxgi --duration 30`, `probe --live --capture-backend dxgi` | R101, R103, R104, R105, R106, R107, R212, T*DXGI duplication entry point*, T*DXGI downscale*, manual `windows-capture.md` §5, §5b, §6 |
| L-03 | DXGI downscale tier 2: an adapter composing the display with **no video processor** must encode at native and let the client scale, with a log line, instead of failing the session | `windows_utils/streamer/dxgi/`, `scaler.rs` | a machine whose display adapter lacks a video engine | the "no video processor; encoding at native" line | T*DXGI downscale (Windows)* |
| L-04 | **Hybrid-adapter selection**: `IDXGIFactory6::EnumAdapterByGpuPreference` + `VendorId`, and whether the NVENC "zero-copy" share is a real same-adapter share or a cross-adapter driver transfer | `windows_utils/streamer/capture.rs`, `pipeline.rs` | Optimus/hybrid laptop | the `adapters: capture=… encode=… same_adapter=true/false` line | R98, R-231 |
| L-05 | CPU-set confinement on a machine that reports **one** efficiency class (the negative branch of the probe) | `windows_utils/streamer/tuning.rs` | a homogeneous-CPU machine | the probe's log line and thread affinity | R99 |
| L-06 | Virtual-display failure tiers: **`DriverNotInstalled`** and **`DriverNotResponding`**, plus the software-adapter branch of `CompatibilityReport` — this box has the driver and two real GPUs, so neither branch is reachable | `windows_utils/virtual_display.rs`, `windows_utils/driver_ipc/`, `windows_utils/compatibility.rs` | a box **without** the IDD driver, then with it, then with it wedged | `cargo test --lib report_display_backend -- --ignored`; `ScreenExtend doctor --json` | R22, T*Virtual display (Windows)*, R199, manual `core-closeout.md` §6 |
| L-07 | Driver IPC resync: `request_state()` on a reported gap; the `.v2` pipe preferred at connect; round trips serialised by their own lock | `windows_utils/driver_ipc/client.rs` | a wedged driver | the resync line and `receive_error` behaviour | R198, R199, T*Driver IPC protocol* |
| L-08 | `QueryDisplayConfig` display-name correlation against a **full ladder** and a multi-display run (EDID monitor names truncate to 13 bytes, so `ScreenExtend - iPad` collides) | `windows_utils/virtual_display.rs` | the IDD driver + several displays | `cargo test --lib report_display_name_correlation -- --ignored --nocapture` | R196, T*Virtual-display name correlation* |
| L-09 | Hosted network: the full probed ladder — Mobile Hotspot (6 GHz > 5 GHz > auto) → Wi-Fi Direct legacy AP → **`netsh` legacy** → unsupported — with a client actually **associating on each band** and the throughput/RTT difference measured. `netsh wlan hostednetwork` is deprecated after 1607 and gone in practice on modern drivers, so this band is the last place tier 3 can run | `windows_utils/hosted_network.rs`, `windows_utils/networking.rs` | Win10 22H2 + a hosted-network-capable Wi-Fi driver + a phone | `iperf3` per band; `ScreenExtend network status`; `cargo test --lib report_wlan_hosted_network_capability` | R21, T*Hosted network (Windows)*, T*Legacy hosted-network capability* |
| L-10 | The **localized** Windows hosted-network probe: `WlanQueryInterface(hosted_network_capable)` and `WlanHostedNetworkQueryStatus` are not localized; the `netsh` string match was. On a non-English install the old probe answered "no" unconditionally and a started network read back as stopped | `windows_utils/networking.rs` | a **non-English** Windows display language | run the probe under the non-English language and compare with `netsh` output | R197, T*Legacy hosted-network capability*, manual `core-closeout.md` §3 |
| L-11 | The **low-latency engine period**: `IAudioClient3::GetSharedModeEnginePeriod` + `InitializeSharedAudioStream(min)` on the silent render companion, which drags the whole endpoint down. The dev box reports `default = fundamental = min = max = 480 frames`, so the request branch has never executed. Microsoft documents the in-box *High Definition Audio Device* driver as 128–480 frames | `windows_utils/audio/silence.rs`, `format.rs` | a driver exposing a small buffer | `ScreenExtend probe --audio --duration 15`; expect the packet-interval mean to move from ~9.4 ms toward ~2.7 ms | R68, T*Engine period (Windows audio)*, manual `windows-audio-tiers.md` §1 |
| L-12 | The **process-loopback** tier correctly **refusing** on a Windows 10 19045 host (the activation type is not supported there even though `ActivateAudioInterfaceAsync` exports) | `windows_utils/audio/loopback.rs` | Win10 19045 | `ScreenExtend probe --audio --process-loopback` must fall back with a named error, not fail the feature | T*System audio capture (Windows)*, manual `windows-audio-tiers.md` §2 |
| L-13 | Audio format conversion tiers 2 and 3 — the engine converting (`AUTOCONVERTPCM \| SRC_DEFAULT_QUALITY`) and the userspace downmix/int-convert — which were unreachable in production until now | `windows_utils/audio/format.rs` | a **non-48 kHz / non-stereo / non-f32** endpoint (a 44.1 kHz USB DAC, a 24-bit interface, or a 5.1 HDMI sink) | `probe --audio` prints the negotiated format and the tier | T*Audio format conversion (Windows)* |
| L-14 | The "no default output device" branch of the Windows system-audio probe | `windows_utils/audio/device.rs`, `windows_utils/compatibility.rs` | disable every render endpoint | `doctor --json` must report it as an entry, not fail silently at stream time | T*System audio (Windows)* |
| L-15 | `capture_ns` anchored to `min(qpcpos, now − read_duration)` — the play-cursor clamp — verified with a client that actually has system audio on | `windows_utils/audio/loopback.rs`, `streamer/audio/mod.rs` | a client with audio enabled | `probe --audio` skew line + `SEAudio.getSyncInfo()` on the client | R19, R67 |
| L-16 | `OPUS_SET_PREDICTION_DISABLED` and constrained-VBR's effect on the **packet-size distribution** — an idle host encodes 3-byte silence frames, so the peakiness claim needs real audio playing | `streamer/audio/encoder.rs`, `opus_sys.rs` | real audio content | `probe --audio --duration 30` with music playing; the packet-size histogram | R73, manual `windows-audio-tiers.md` §3, §4 |
| L-17 | Intel Quick Sync on a **different generation and a clean install**: is `libvpl.dll` present on a real Intel host with only the GPU driver? (it is deliberately not bundled). Plus the `MFX_WRN_DEVICE_BUSY` retry path, which never triggered here, and the `MaxSliceSize` cap under `LowPower = ON` on another Gen | `windows_utils/streamer/intel/`, `intel_sys.rs` | an Intel iGPU, ideally a fresh Windows install | the dispatcher line naming the path it loaded; `probe --encode --encoder intel` | R235, R237, T*oneVPL dispatcher*, T*Quick Sync slice layout*, R-230 |
| L-18 | The DPR ladder on a pre-26100 build: mode registration at create time, the DPI re-applied *after* the resolution change | `windows_utils/virtual_display.rs`, `streamer/server.rs`, `streamer/platform.rs` | the IDD driver on this band | join at DPR 1.25/1.5/2.0 from a client and read the applied mode + DPI | `docs/BASELINE.md` mode-change table (in-ladder 111/308 ms, out-of-ladder ~772 ms) |
| L-19 | Cursor correctness on **both** backends at several DPRs and both orientations (the DXGI path composites the cursor itself, incrementally) | `windows_utils/streamer/dxgi/`, `capture.rs` | this band | by eye per `windows-capture.md` §6, both backends | R101, manual §6 |
| L-20 | Latency on this band, if the machine is worth measuring on: the full `selftest` and `live` matrices, to be compared against `docs/BASELINE.md`'s 26200 rows | all of the above | this band | `ScreenExtend probe --whep-selftest --duration 30 --encoder <e> --capture-backend <c>` per cell | manual `windows-capture.md` §7, `docs/BASELINE.md` |
| L-21 | Confirm nothing regressed at 26100 itself, if a 24H2 box is also available | as above | build 26100 | the same matrix | manual `windows-capture.md` §8 |

### G5 — real clients, network conditions and the glass-to-glass rig

| id | what | files | needs | how to verify | source |
|---|---|---|---|---|---|
| C-01 | **Client video transport tier**: `RTCRtpScriptTransform` → `createEncodedStreams` → `<video>`, with the 2.5 s watchdog stepping down rather than aborting the join | `streamer/static/index.html`, `transform-worker.js` | Chrome/Edge 141+, Chrome/Edge 86–140, Safari 15.4+, Firefox 117+ | stats byte 72; `SEVideo.getStats()` | R28, T*Client video transport*, manual `client-page.md` §9 |
| C-02 | **Client keyframe request**: `sendKeyFrameRequest()` → `KEYFRAME_REQUEST` (`0x64`) on the reliable channel → none; the host must parse it, rate-limit to one per 200 ms, and count `client_keyframe_requests` | `streamer/static/{index.html,transform-worker.js,input.js}`, `streamer/input/protocol.rs`, `streamer/keyframe.rs` | Chrome/Edge/Samsung Internet **below 141** is the highest-risk device | host log `client keyframe request`; `status --json` counter | R28, R48, R52, T*Client keyframe request*, manual `client-page.md` §2 |
| C-03 | **Client decode**: backpressure on outstanding decoder *outputs* (target 1) rather than `decodeQueueSize`; `isConfigSupported` given the real coded size | `streamer/static/transform-worker.js` | the matrix | `SEVideo.getStats()` decoder hold p50/p90/p99 | R29 |
| C-04 | **Client render**: three-argument `drawImage`, canvas re-sized on every frame-size change, `desynchronized` removed; the opt-in `VideoTrackGenerator` tier behind `?present=track` — an A/B that no public measurement settles | `streamer/static/transform-worker.js` | Safari 18+ / Chrome 94+ | stats byte 73; paint-to-paint interval A/B | R30, T*Client presentation*, manual `client-page.md` §4 |
| C-05 | **Client join chain**: no microphone prompt on the join path, ICE gathering ending a grace period after the last useful candidate, the measured refresh rate folded into the join, the WHEP POST deadline | `streamer/static/index.html` | a slow phone; a relayed join | join wall time; `node scripts/test-client-ice.mjs` is the offline half | R31, manual `client-page.md` §3 |
| C-06 | **Client trickle ICE** end to end: `POST /whep` → `Location: /whep/{id}` → `PATCH` with `application/trickle-ice-sdpfrag` → `a=end-of-candidates`, and the `trickleOff` latch + single rejoin when a host advertises trickle and returns no resource. Measured offline at **201 ms** saved on a LAN and **713 ms** relayed | `streamer/static/index.html`, `streamer/server.rs` | a real browser + a relayed path | host log `trickle: added N remote candidate(s)`; degradation bits 22 and 15 | R204, R285, T*Client ICE signalling*, manual `transport.md` §3 |
| C-07 | **Client audio decode tier**: `AudioDecoder.isConfigSupported({codec:'opus'})` awaited before the offer (constructor existence is not the probe — Safari 26 and Firefox 130 newly pass it) → the browser's own NetEQ | `streamer/static/{audio.js,audio-worker.js,index.html}` | Safari 26, Firefox 130, Firefox ≤ 129 | stats byte 3; `SEAudio.getSyncInfo()` | R32, T*Client audio decode*, R75 |
| C-08 | **Client audio ring tier**: `SharedArrayBuffer`+`Atomics` → a transferred `MessagePort` → page relay. The COOP/COEP + secure-context question, including whether a self-signed cert still yields `crossOriginIsolated` | `streamer/static/{audio.js,audio-worker.js,audio-worklet.js}`, `streamer/server.rs` | the matrix, incl. a self-signed HTTPS join | `SEAudio.getSyncInfo().transport` = `shared`/`port`/`postMessage` | R173, T*Client audio ring* |
| C-09 | **Client audio output latency** tier and the adaptive safety margin: `outputLatency` → `getOutputTimestamp()` estimate → `baseLatency` → unknown. Treating a missing value as zero made the commanded depth too large by exactly the output latency — a few ms wired, **100–200 ms on Bluetooth** | `streamer/static/audio.js`, `audio-worklet.js` | a **Bluetooth sink** on each of Chrome/Android, Safari/iOS and Chrome/Windows | stats byte 92, degradation bit 8; A/V offset by camera | R34, T*Client audio output latency* |
| C-10 | **Client audio sink selection** tiers 1–5, and the concealment/underrun behaviour (four consecutive starved blocks before re-priming; sequence gaps detected in the worker) | `streamer/static/audio.js`, `audio-worker.js`, `audio-worklet.js` | the matrix; Firefox 116+ for tier 1 | stats byte 75; underrun counters | R33, T*Client audio sink selection* |
| C-11 | **Client screen wake**: Wake Lock → `nosleep.js` → none, including the standalone home-screen-app case where the request fulfils and the screen sleeps anyway | `streamer/static/{index.html,nosleep.js}` | iPhone (home-screen app) + Android | flags bit 2, degradation bits 3–4 | R36, T*Client screen wake* |
| C-12 | **Client pointer input**: Pointer Lock with `{unadjustedMovement:true}` → plain → prefixed → hidden cursor; coordinates normalised against the `object-fit: contain` picture rect; `getCoalescedEvents()` capped at 8 (Firefox Android has the method and always returns `[]`) | `streamer/static/input.js` | Chrome 88+, Safari 18.4+, Firefox Android, iPhone (never locks) | flags bit 3, degradation bits 2 and 14 | R35, T*Client pointer input*, T*Client coalesced input* |
| C-13 | **Client fullscreen** tiers 1–4, including `video.webkitEnterFullscreen` as the only thing that works on iPhone, and that a tier-2 `undefined` return no longer takes pointer lock, keyboard lock and the wake lock down with it | `streamer/static/{index.html,styles.css}` | iPhone, iPad, desktop | flags bit 5, degradation bit 0 | R36, T*Client fullscreen* |
| C-14 | **Client input tails and the fast-channel sequence**: the `u64` client stamp, the `MOUSE_DELTA` `UNADJUSTED` flag byte, and the `u16` reorder sequence — all gated on the host's `PONG` capability bits 3/4/5. `input_recv` is structurally empty and `input_reorder_drops` structurally 0 until a client sends them | `streamer/static/input.js`, `streamer/input/protocol.rs` | the matrix + an induced-reorder link (`clumsy` or a TURN relay) | `status --json` histograms; `node scripts/test-client-stats.mjs` is the offline half | R171, R209, R286, T*Client input tails*, T*Client fast-channel sequence*, R-244 |
| C-15 | **Client stats frame v2/v3** end to end, and the 49 formerly-bare `catch (_) {}` now counting swallowed errors | `streamer/static/{index.html,audio.js,input.js}`, `streamer/telemetry.rs` | any client | `ScreenExtend status --json` per session; `SEVideo.getStats()` | R37, R49 |
| C-16 | Client `/audio-outputs` poll 4 s → 30 s where `devicechange` exists; dropped-file screening against `pc.sctp.maxMessageSize`; the second 60-sample refresh-rate measurement applied by reconfigure; `detectHasEarpiece` by screen short edge | `streamer/static/{audio.js,input.js,index.html}` | the matrix | request count over a minute; the refusal message | R174, R175, R176, R177 |
| C-17 | Client audio capability handshake: `wantsFallbackTrack` and `verified` — wired on the host and **inert** until a client advertises them | `streamer/static/audio.js`, `streamer/webrtc_session.rs` | a real client | host log `audio caps: verified=… wantsFallbackTrack=…` | R172, T*Audio loss robustness* |
| C-18 | **Audio packet-level redundancy** end to end: `FLAG_REDUNDANT` enabled when the client advertises it *and* the BWE reports ≥ 0.5 % loss; and a `CrossNetwork` session starting defensive without waiting for the video BWE | `streamer/audio/protocol.rs`, `webrtc_session.rs`, `static/audio-worker.js` | an induced-loss link + a relayed session | the two redundancy counters in `status --json`; `node scripts/test-client-audio.mjs` is the offline half | R74, R213, T*Audio loss robustness*, manual `core-closeout.md` §4 |
| C-19 | **H.264 profile negotiation** against the real client matrix: High > Main > Baseline from the client's own `profile-level-id`, and the answer's level derived from the negotiated geometry under `level-asymmetry-allowed=1`. Includes an old Android / smart-TV constrained-baseline decoder | `streamer/negotiate.rs`, `webrtc_session.rs` | the matrix incl. one constrained-baseline-only decoder | the join log's chosen profile; an A/B of bitrate at fixed quality | R179, T*H.264 profile*, T*H.264 level*, manual `transport.md` §4 |
| C-20 | **DSCP / WMM marking**: the muxed tuned socket, Windows qWAVE `QOSAddSocketToFlow(QOSTrafficTypeAudioVideo)`, macOS `SO_NET_SERVICE_TYPE`/`IP_TOS` AF41. Separating AC_VI from AC_BE needs **two stations on one WMM-honouring AP and a monitor-mode capture** — and "this AP ignores DSCP entirely" is a real, publishable result | `streamer/sockets.rs`, `streamer/platform.rs` | two stations, a WMM AP, monitor-mode capture | the marked bytes in the capture; p99 under a competing download | R180, T*Media socket*, T*Socket DSCP / send buffer*, T*DSCP marking (Windows)*, manual `transport.md` §2 |
| C-21 | **Send pacing tiers 2 and 3** (`Light` 5×/24-packet, `Full` libwebrtc 2.5×/8-packet) on a relayed or contended path; tier 1 (`Off`) is already verified free on a LAN | `streamer/pacer.rs` | a relayed path and a busy AP | packet-cluster size in a capture; p99 `videoDelayEmaMs` | R53, T*Send pacing* |
| C-22 | **RTP MTU 1012 on a relayed path** and the playout-delay hint tiers (`0,0` same-subnet vs `0,200 ms` relayed vs absent on Firefox/WebKit) | `streamer/server.rs` (`locality_for`), `webrtc_session.rs` | a TURN relay + Chromium and Firefox clients | the locality line; stutter vs no stutter across the relay | R56, T*RTP MTU*, T*Playout-delay hint* |
| C-23 | **Congestion-signal tiers 2 and 3**: REMB (a client that declines `transport-wide-cc`) and RTCP-receiver-report loss + the RTT-excursion detector. Tier 1 is verified; no client here declines the extension | `streamer/congestion.rs`, `bitrate.rs`, `webrtc_session.rs` | a browser that declines TWCC (Safari/Firefox variants) | the once-per-session line `BWE: congestion signal is <name>` | R54, R16, T*Congestion signal* |
| C-24 | The **loss-based bitrate driver's step response** and the keyframe-storm behaviour under injected loss (1 %, 3 %, 5 %) | `streamer/bitrate.rs`, `congestion.rs`, `keyframe.rs` | `clumsy` + a browser | `fraction_lost`, the count of `request_idr()`, p99 `videoDelayEmaMs` | R52, R54, R55, R183, R288 |
| C-25 | **Repair from a long-term reference instead of an IDR — the rate.** A repair frame measures **23,698 B against a 28,423 B IDR** on the pessimistic synthetic fixture; what is unknown is how many IDRs a lossy minute stops producing | `streamer/repair.rs`, `windows_utils/streamer/nvidia/`, `streamer/keyframe.rs` | NVENC host + an induced-loss link | IDRs per minute with and without repair, at each loss rate | R287, T*Picture recovery*, manual `transport.md` §5 |
| C-26 | **Rolling intra refresh** (`cfg.intra_refresh = true`) — no regression is verified, the recovery *benefit* is not | `streamer/config.rs`, encoder backends | induced loss | time-to-recover after a loss burst | R61 |
| C-27 | **Cross-network: the cloud relay + TURN**, which nothing in the corpus has ever exercised against a real `session.screenextend.app`; plus the relayed join restructure and the `RegisterRejected` typed error | `streamer/cloud.rs`, `server.rs`, `webrtc_session.rs`, `session.rs` | a second network + a real TURN server | `CloudStatusChange` in the UI; a completed relayed join; `ScreenExtend status --json` | R65, R181, R185, R56, R-247 |
| C-28 | **STUN configuration tiers** — none (same-subnet), configured `stunUrls`, built-in Google default; and an empty configured value meaning "no STUN at all" | `streamer/server.rs`, `session.rs`, `cli.rs` | a LAN and a cross-network join | `ScreenExtend stun show`; the ICE candidate list | R187, T*STUN servers* |
| C-29 | **IPv6-only network**: the bind-family tiers, `url_host` bracketing per RFC 3986, and link-local being reported rather than silently served | `macos_utils/networking.rs`, `windows_utils/networking.rs`, `streamer/{config,server}.rs` | an IPv6-only network | a successful join over a global-unicast v6 address | R192/R223, T*Streamer bind family* |
| C-30 | **Wired vs wireless**: `link_medium_for_local_ip` already reports `wired`/`wireless`/`other`; whether USB tethering is worth documenting as a low-latency tier is open. Luna Display's own bench: **11.3 ms mean USB vs 22.4 ms Wi-Fi** over 50 samples | `streamer/platform.rs`, `windows_utils/networking.rs`, `macos_utils/networking.rs` | a 240 fps camera + an Android phone on a cable | the `session link:` line + the camera method | R205, T*Local link medium*, manual `transport.md` §1 |
| C-31 | **Two pen-capable clients at once**: synthetic pen events now carry the client's own `pointerId`; whether Windows disambiguates two synthetic pen contacts across two device handles is undocumented | `streamer/input/windows.rs` | two pen devices (iPad+Pencil, Surface, Android+S Pen) | draw simultaneously in an ink app | R210, R215, manual `core-closeout.md` §1 |
| C-32 | **Four simultaneous clients**: the per-session injector decision, the cross-session stall the sequence wrapping prevents, `frames_dropped_broadcast` (zero in all twelve single-viewer runs), the idle-repeater priority drop, the **NVENC 8-session cap** under real clients, and the transient encoder-busy path | `streamer/session.rs`, `input/`, `windows_utils/streamer/{tuning,nvidia}/`, `pipeline.rs` | four client devices | `status --json` per session; the counters above | R59, R109, R279, T*NVENC concurrent sessions*, manual `core-closeout.md` §7 |
| C-33 | A host with **three network adapters**: the one-process-wide tokio runtime claim (48 → 4 worker threads) is arithmetic here, not measured | `streamer/mod.rs` (`transport_runtime`) | three adapters | thread count; startup wall time | R77 |
| C-34 | **Glass-to-glass, by camera, against the competitor table** — the only measurement that is not self-reported. Film host and client in one shot at 240 fps with `probe --live --counter`, ≥ 30 samples, report p50/p90/p99 with the camera model and both refresh rates (240 fps quantises to 4.17 ms). Then run **spacedesk** and **Deskreen** on the identical host/client/AP by the same method — without a side-by-side every number here is unfalsifiable | `streamer/synthetic.rs`, `docs/BASELINE.md` | 240 fps camera + the client matrix | `ScreenExtend probe --live --duration 60 --synthetic-pattern --workload motion --counter`; targets: **Luna 22.4 ms Wi-Fi / 11.3 ms USB**, Hopp (browser WebRTC) **98 ms**, Parsec 4–8 ms LAN, Steam Link 13–16 ms, Moonlight 20–35 ms, Miracast 80–100 ms. Host half today: **8.39 ms p90** (pre-W3a) → **3.93 ms p90** | `docs/BASELINE.md` "Competitor baselines" + "Glass-to-glass procedure", manual `client-page.md`, `windows-capture.md` §7 |

---

## 4. Context every guide must embed

Each guide is read by an instance with **zero** prior context. Every guide must contain all of
the following, written out — not referenced.

### 4.1 What ScreenExtend is, and the one-paragraph architecture

Tauri 2 desktop app; the **host** creates a real virtual display per client, GPU-encodes it and
streams H.264 over WebRTC (WHEP signalling); the **client** is a dependency-free browser page
served by the host's own axum server. Two frontends: `src/` (React desktop control UI) and
`src-tauri/src/streamer/static/` (the client page, plain HTML/JS/CSS, `include_str!`'d, **no
bundler, no dependencies**). Rust core in `src-tauri/src/` with three `cfg`-gated sibling
platform modules (`windows_utils/`, `macos_utils/`, `linux_utils/`) exposing the same surface.

### 4.2 Engagement norms (verbatim in every guide)

- **Research first.** Load web tools before concluding anything. Prefer primary sources: vendor
  SDK docs and headers, WWDC transcripts, RFCs, and the source of Moonlight, Sunshine, OBS,
  FFmpeg, Chromium and webrtc-rs. **If one shipping product does the thing, it is possible** —
  "impossible" is only allowed after searching and finding nobody doing it.
- **Measure before and after every change**, as **p50 / p90 / p99 / max**, never a bare mean.
  Jitter is felt more than the average. Nanosecond stamps come off the shared host clock
  (`streamer::audio::host_now_ns`).
- **Hyper-optimisation standard:** every optimisation that shaves even **0.1 ms** on even one OS
  build or one browser is in scope and gets *implemented and measured*, even at high
  implementation cost. "Good enough", "diminishing returns", "minor" and "speculative" are not
  reasons to stop. A speculative idea is resolved by trying it and measuring, or by a **recorded
  rejection with a reason and a number** in `docs/OPTIMIZATIONS.md` — never by being skipped.
- **Parallelise.** Spawn Opus 5 subagents for independent work; run measurement passes
  concurrently where the hardware allows and serially where it does not (one release build at a
  time; check for competing compilers before measuring).
- **Never trust a flag without a read-back.** Every property that is set must be read back and
  logged; every tier that is selected must announce itself. `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`
  reads backwards from its meaning; `x264`'s `encoder_open` returning non-null for `X264_CSP_BGRA`
  means High 4:4:4 Predictive, not "BGRA supported". Assume nothing.
- **Measurement hygiene.** ≥ 3 trials, report the median with p50/p90/p99. Check for competing
  builds first. Leave ≥ 15 s between probe runs — a probe that exits without tearing down leaves
  its virtual display attached and silently corrupts the next run's capture (this bit 4 of 12
  runs during the overhaul).

### 4.3 Repo map for that guide's territory — and the rule that everything else is off limits

Each guide lists the exact files it may touch, and states plainly: **everything outside that list
has already been audited, optimised and verified on the development machines; do not touch it.**
A change outside the territory is a merge conflict against work the reader cannot see.

### 4.4 Ledger and findings conventions

- `docs/OPTIMIZATIONS.md` — one table row per change attempted:
  `| # | Area | Change | Hypothesis | Before | After | Status | Kept? | Notes |`. Failures are
  recorded on purpose. Status vocabulary: **`VERIFIED`** (executed and measured on the machine),
  **`COMPILE-CHECKED`** (builds and passes static/layout tests, never executed), **`UNVERIFIED`**
  (written from research; no runtime signal). A guide's job is to move its rows to `VERIFIED`
  *with numbers*, or to revert them *with reasons*.
- `docs/CAPABILITY-TIERS.md` — one row per subsystem × tier:
  `| Subsystem | Tier (newest first) | Probe | Era / platform | Expected delta | Status |`.
- `docs/BASELINE.md` — pre-change numbers; `docs/TELEMETRY.md` — how to read them;
  `docs/manual-tests/*` — the scripts.
- `findings/` at the repo root (gitignored via `.git/info/exclude`) is the work queue and the
  crash-recovery point. **Every problem, opportunity, suspicion and idea becomes one file**,
  named `<severity>-<component>-<slug>.md` with severity in
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

  **Lifecycle: a file is deleted the moment its finding is resolved** — implemented and verified,
  or deliberately rejected with the reasoning recorded in `docs/OPTIMIZATIONS.md` *first*. New
  discoveries become new files. Nothing may live only in the agent's context.

### 4.5 Gates and commit rules

- Windows, from `src-tauri/`: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`. From the root when `src/` or `src/lib/bindings.ts` changed: `pnpm lint`,
  `pnpm build`, `node scripts/check-legacy-webkit.mjs`, `node scripts/check-bindings.mjs`,
  `node scripts/check-versions.mjs`.
- macOS, additionally after any FFI change:
  `scripts/check-macos-dyld.sh target/release/ScreenExtend <path-to-MacOSX10.15.sdk>` (or
  `--floor 10.15` against the newest SDK when the 10.15 one is absent).
- Commits are small and single-purpose; a performance commit **states its measured before/after**.
  Commit at least every 30 minutes. Every commit message ends with:

  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: <the URL of your own session>
  ```

- **NEVER `git push`. Never touch `origin`.** Local branches and commits only.

### 4.6 The invariants that apply (from `CLAUDE.md`)

Every guide carries the ones relevant to it; the macOS guides carry all of them:

- **No link-time references to macOS APIs newer than 10.15.** `dlsym` / `AnyClass::get` /
  `msg_send!` / `set_optional_*` only. dyld binds every non-weak undefined symbol before
  `main()`, so one such reference means the app does not launch on Catalina while launching fine
  on the reader's own Mac. `extern static` constants count. Audit with `scripts/check-macos-dyld.sh`
  and `nm -u`. **This is the single easiest way for a modern-Mac agent to destroy the product.**
- **Probe capabilities, never version numbers.** Every tier is selected by attempting the real
  capability and falling back at runtime without crashing; the selected tier is reported through
  `CompatibilityReport` and the log.
- **Real-time threads take no locks and do not allocate.** The audio capture thread (MMCSS "Pro
  Audio" / `AudioDeviceIOProc` / the SCK sample handler) and the video capture callbacks talk to
  the async world over `crossbeam-channel` only.
- **`std::sync::Mutex` is the one blocking mutex type**, and a `MutexGuard` is **never held
  across an `.await`**. Watch the `if let` / `match` scrutinee temporary-lifetime footgun. The two
  intentional async locks are `DISPLAY_CORRELATION_LOCK` and `driver_ipc`'s `receive_error`.
  `parking_lot` stays confined to the vendored `windows_utils/windows_capture/` fork.
- **`tprintln!` / `teprintln!`, never `println!`/`eprintln!`** in the Rust core.
- **The client page has no build step and no dependencies** and must keep parsing on old mobile
  WebKit (that constraint is why flex `gap` was replaced with margins).
- **The desktop UI's appearance does not change.** Performance behind the pixels is in scope;
  anything user-visible goes to `docs/OPTIMIZATIONS.md` under "UI-visible recommendations" and
  the finding is then deleted. The desktop UI must also still run on **Safari 13.1**.
- A new Tauri command goes into **all three** `*_utils` modules and `collect_commands!`; never
  hand-edit `src/lib/bindings.ts` (regenerate from a debug build with **no CLI arguments**, run
  from `src-tauri/`, then **diff it**). A new shell invocation goes into
  `src-tauri/capabilities/default.json`. A new user setting needs three touches: the TS config
  type + default, the Rust command, and the `serve`/`config` CLI path.
- DPR is only correct when the resolution multiplier and the OS scale step are the same number;
  pre-register the whole ladder at create time and re-apply the DPI **after** a resolution change.
- Simultaneous displays are **uncapped** everywhere except the macOS AirPlay tier; never evict a
  connected device to make room for a new one.
- No AI-tooling references in comments.

### 4.7 The measurement harness (every guide repeats the parts it needs)

```sh
# whole host path, real WHEP join over loopback, per-stage telemetry on exit
ScreenExtend probe --whep-selftest --duration 30 --encoder <auto|nvidia|intel|amd|software> \
                   --capture-backend <wgc|dxgi|auto>

# real content through the real capture path, to a file
ScreenExtend probe --live out.h264 --duration 30 --workload <mixed|static|scroll|motion|drag>

# glass-to-glass overlay (frame number + elapsed-ms clock painted into the encoded frames)
ScreenExtend probe --live --duration 60 --synthetic-pattern --workload motion --counter

# single-frame captures, encoder smoke, adaptive-bitrate path
ScreenExtend probe --capture cap.png | --dxgi dxgi.bmp | --encode out.h264 | --bitrate

# system audio: packet rate, subscriber inter-arrival, capture→subscriber latency, capture_ns skew
ScreenExtend probe --audio [--process-loopback] --duration 15

# from a running host
ScreenExtend serve -v            # one summary line per session every 5 s
ScreenExtend status --json       # the same counters, per session, from another shell
ScreenExtend doctor --json       # capability report + per-probe timings
ScreenExtend logs --lines 400    # the 2000-line ring backlog

# in the client page's console
SEVideo.getStats()  ·  SEAudio.getSyncInfo()

# offline client harnesses (no browser needed, and no substitute for one)
node scripts/test-client-stats.mjs · test-client-audio.mjs · test-client-ice.mjs
```

`probe` needs `--features probe` for `--whep-selftest`. Every guide states which of these its
items use and what a *correct* reading looks like.

### 4.8 What was already tried and rejected in that area, with the numbers

Each guide must carry the rejections for its own territory so nothing is re-tried blindly.

**Transport (goes in G5):** FEC not built — webrtc-rs ships no packetizer for it (`ulpfec` is a
bare codec registration with nothing behind it), and the six cheaper alternatives all landed
first (rolling intra refresh, the keyframe gate, the pacer, TWCC + trendline BWE, the relayed MTU,
audio `FLAG_REDUNDANT`); the bar for revisiting is an instrumented relayed session that *still*
shows PLI storms. HEVC not negotiated — the RTP payloader exists but no host backend can encode
it. WebTransport not built — Chrome has not shipped `sendOrder`/`createSendGroup()`, there is no
session `getStats()`, no transport-level fragmentation, and no WHEP-over-WebTransport spec.
Sender RTX **rejected twice** (rows 62 and 269) — webrtc-rs binds the NACK responder by
`info.ssrc` for streams whose `rtcp_feedback` declares nack, which the RTX registration does not.
The per-session injector thread stays per-session (the real defect — a detached thread — is fixed).

**macOS capture/encode (goes in G1 and G2):** one shared VideoToolbox session between clients at
the same resolution — rejected as a session-lifecycle change, not an encoder one; the detection
half landed (`InstanceLimit` logged, a software fall-back logged loudly). Mid-session SCK →
CGDisplayStream fallback — rejected; the tier is committed only after a delivered frame, and the
first-frame deadline (750 ms → synthesized black IDR, counted as `capture_no_first_frame`) is the
watchdog that helps. A Metal convert/scale stage in `gpu.rs` — rejected; WindowServer delivers the
frame already scaled and already in `420f`. `kVTCompressionPropertyKey_ConstantBitRate` — rejected;
CBR spends bits on a still screen.

**macOS audio (goes in G1 and G2):** no play-cursor clamp on macOS — measured skew p50 **−0.57 ms**
(min −6.23) over 3277 packets, which is under one 2.67 ms IO cycle and in the *right* direction;
applying the Windows clamp would flatten the ladder that produces exactly-5.000 ms spacing (0 of
3277 deltas were anything else). **The Process Tap's skew is a separate, unmeasured question and
must be measured before that tier is trusted.** `mach_wait_until` for the shm reader — rejected;
the time-constraint policy was the defect, `user_interactive` + a 250 µs poll is the whole win.
No sample-rate converter — Opus fixes 48 kHz and all three tiers negotiate or declare it; the
`teprintln!` that fires if SCK ever delivers another rate is the trigger to revisit.

**Windows capture/encode (goes in G3 and G4):** `Direct3D11CaptureFramePool::CreateFreeThreaded` —
rejected on the measurement, not the difficulty: it would shrink `capture_delivery`, measured at
p50 0.98 / p90 4.19 / p99 6.82 ms static and p50 6.29 / p90 9.44 / p99 20.97 ms live, and a thread
wake is tens of microseconds; against that it moves the MMCSS/`TIME_CRITICAL`/P-core registration
onto the wrong thread and loses arrival ordering. Double-buffering the CPU-bridge staging texture —
rejected: the whole readback is **1192 µs** BGRA / **740 µs** NV12, and pipelining the map costs a
full frame period (16.7 ms at 60 Hz) to remove at most 1.2 ms of stall. Removing the per-frame
`Flush` in the NVENC ring stage — **kept deliberately**: the destination is a cross-device shared
resource and the failure mode is a torn frame under load on some drivers. High-resolution waitable
timers and `ThreadMemoryPriority` — no-ops here. Frame-pool depth 3 (ring) / 2 (inline) —
`frames_dropped_stale` reads 5, 0 and 3 against 321–421 delivered frames, so there is no queue to
shorten. `BROADCAST_CAPACITY = 4` — `frames_dropped_broadcast` was zero in all twelve after-matrix
runs. Sub-frame / slice-mode output rejected on NVENC, AMF and x264 against a measured ~2 ms
ceiling (**re-open this on AMD hardware**). `libvpl.dll` deliberately not bundled. oneVPL surface
import rejected on the per-stage split. `nal-hrd=cbr` rejected. The per-frame changed-region hint
evaluated and not consumed. Per-frame NVENC map/unmap measured and left alone.

**Client page (goes in G5):** `getPredictedEvents()` on the pointer batch — the byte is in place
and deliberately written as 0; prediction's trade is a per-engine number no dev box can obtain.
Chunking a dropped file across SCTP messages — needs a host-side protocol change. Moving the
`/reconfig` poll onto the reliable DataChannel — a host change the client cannot make alone.
Consolidating the three liveness signals — they fail independently, so collapsing them makes
liveness only as good as the weakest transport. A/B-ing `jitterBufferTarget` — all three
jitter-buffer knobs are **inert on the WebCodecs path** (an encoded transform takes the frame
instead of it entering the frame buffer), so the measurement is only executable on the `<video>`
fallback, and that is also why every receive-side timing field in `getStats()` reads zero on the
fast path.

### 4.9 The required final report

Every guide ends with the same report contract. The report must contain:

1. **Machine facts** — exact OS build, model, CPU/GPU, driver versions, browser versions,
   AP model and band, and anything else a reader would need to reproduce.
2. **Per-item status**, one line each, against the guide's own numbered inventory:
   `VERIFIED (numbers) | reverted (reason) | still UNVERIFIED (what was missing)`. No item may be
   silently absent.
3. **Before/after numbers** for every change, as p50 / p90 / p99 / max, with the trial count and
   whether the machine was quiet.
4. **The ledger diff** — the exact rows added or edited in `docs/OPTIMIZATIONS.md` and
   `docs/CAPABILITY-TIERS.md`, and the `docs/BASELINE.md` cells filled.
5. **Findings** — created, resolved-and-deleted, and retagged, by filename.
6. **Comparison against the competitor/baseline numbers** the guide named.
7. **What could not be done and why** — recorded honestly rather than left blank, including
   negative results, which are results.
8. **Commits** — branch name and one line per commit. Nothing pushed.

---

## 5. Items that could not be placed

1. **R-241 — the AirPlay display tier being *selected*.** It is chosen only below macOS 10.14.4,
   which is below the app's own `minimumSystemVersion` of 10.15, so the shipped binary refuses to
   launch on every OS where this backend would be chosen. No guide can close it. This needs a
   **scope decision** — delete the tier, or lower the floor — not hardware. (The parts that *can*
   be tested from a modern sender are in M1-26.)
2. **R82 / M1-28 — the CI workflows' first run.** `ci.yml` and `build-release.yml` have never
   executed; they were validated by parsing YAML. Running them requires a `git push` to the
   owner's GitHub, which the engagement's standing rule forbids. It is written into G1 Part B but
   is **contingent on the repo owner explicitly authorising a push**; otherwise it stays open.

**Declared out of scope:** Linux (`findings/_out-of-scope/` — `major-build-linux-does-not-compile`,
`major-build-streamer-core-cannot-compile-on-linux`, `minor-linux-stubs-return-success`).
**`linux_utils/` does not compile**; it implements only part of the command set
`collect_commands!` names, and several of its stubs report success rather than "unsupported". No
guide covers it, and every guide says so where it names the three platform modules.

## 6. Items closeable on the existing development machines (deliberately **not** in any guide)

Listed so a guide writer does not adopt them by mistake, and so the Phase D owner can sweep them:
row 1's latency half (the LTO profile), rows 6/8's before-numbers and parse time, row 76's JSON
claim, row 77's clean-box startup re-measurement, rows 157–168's DevTools/frame-capture halves,
row 202 (the 30 s animated-screen CGDisplayStream drop-count run on the 10.15.8 box), row 239
(`ffprobe` the x264 colour tags), row 246's NVENC arm, the two macOS display tests that need a
console session rather than SSH, and R-245/R-246 (the Safari 13.1 desktop-UI floor and cold-start
wall time, both of which the 10.15.8 box **is** the right machine for).

§7 adds eight more that the final audit found in the same shape: rows 7, 9, 10, 11, 12 (the
remaining desktop-UI render/startup/IPC rows), row 13 (the Safari 13.1 `@supports` floor repairs),
row 17 (the quality slider's end-to-end effect — the dev box has NVENC, Quick Sync and x264), row 81
(`single_instance`'s loopback control path), row 184 (the `OnceLock` client-page substitution), row
200 (`probe::finish` on Ctrl+C and from the panic hook), row 257 (`set_log_streaming(false)` on the
log panel's unmount), and row 279's NVENC arm.

---

## 7. Coverage audit (final)

Taken against **`b333707`** on `latency-overhaul`, after every workstream merged. The rule the audit
enforces: **every `COMPILE-CHECKED` or `UNVERIFIED` row in `docs/OPTIMIZATIONS.md`, every
non-`VERIFIED` row in `docs/CAPABILITY-TIERS.md`, every `docs/manual-tests/*` step that needs other
hardware, and every `docs/RISK-REGISTER.md` §7 entry appears in exactly one guide, at a named task,
with a verification step** — or is recorded below as deliberately outside the guide set.

Where two guides can physically reach the same row, the table names the **owner** (whose "Done
when" flips it) and marks the other **corroborating** (a second data point, recorded, but it does
not own the status). Those are the only shared cells and there are five of them.

Guide keys: **G1** `modern-mac.md` · **G2** `intel-mac.md` · **G3** `windows-amd.md` ·
**G4** `windows-legacy.md` · **G5** `clients-network-and-glass-to-glass.md` ·
**DEV** closeable on the project's own two development machines (§6) · **OOS** declared out of scope.

### 7.1 `docs/OPTIMIZATIONS.md` — every non-`VERIFIED` row

| Row | Subject | Guide | Task |
|---|---|---|---|
| 1 | LTO profile, latency half | DEV | §6 sweep |
| 2 | Screen Recording read (12.3+ SCK confirmation, 15/26) | G1 | M1-11 |
| 4 | the whole AMF backend | G3 | A-01 |
| 6, 7, 8, 9, 10, 11, 12 | desktop-UI render / startup / IPC / CSS rows | DEV | §6 sweep (the Windows box runs the UI) |
| 13 | Safari 13.1 floor repairs behind `@supports` | DEV | §6 sweep (R-245) |
| 17 | quality slider → `min_qp`, end-to-end effect | DEV | §6 sweep |
| 18 | telemetry, macOS arm | G1 | §7.2 — route `probe --live` through the production pipeline |
| 19 | WASAPI `qpcpos` stamping | G4 | 3.16 (L-15) |
| 21 | hosted-network ladder, throughput/RTT per band | G4 | 3.9 (L-09) |
| 22 | `CompatibilityReport`, `DriverNotInstalled` + software-adapter branches | G4 | 3.7 (L-06) |
| 26 | `ax::children()` owning `Children`, leak counts | G1 | M1-26 |
| 28 | client video transport tier | G5 | C-01 |
| 29 | client decoder backpressure | G5 | C-03 |
| 30 | client render / `?present=track` | G5 | C-04 |
| 31 | client join chain | G5 | C-05 |
| 32 | client audio decode probe | G5 | C-07 |
| 33 | client underrun / concealment | G5 | C-10 |
| 34 | client `outputLatency` + safety margin | G5 | C-09 |
| 35 | client pointer normalisation + coalesced events | G5 | C-12 |
| 36 | client wake lock + fullscreen | G5 | C-11, C-13 |
| 37 | client stats frame v2/v3 | G5 | C-15 |
| 38 | macOS HostAP probe → association, DHCP, throughput | G1 | M1-24 |
| 39 | `create_display_with_modes` + HiDPI from the ladder | G1 | M1-19 |
| 41 (=189/220) | `PF_ROUTE` adapter watcher | G1 | M1-23 |
| 42 | `set_server_ports` graceful restart | G1 | M1-22 |
| 43 | `CGEventCreateScrollWheelEvent2` on arm64 | G1 | M1-17 |
| 44 | `mouse_relative` / a touch client | G5 | C-12 |
| 46 | `FAST_CAP` 512→64, real percentiles | G5 | C-14 |
| 47 | `Drop for Injector` handle count | G5 | C-32 |
| 48 | `KEYFRAME_REQUEST` (`0x64`) end to end | G5 | C-02 |
| 52 | `KeyframeGate` under a storm | G5 | C-24 |
| 53 | send pacer, the benefit | G5 | C-21 |
| 54 | BWE step response | G5 | C-23, C-24 |
| 56 | `SessionLocality` relayed constants | G5 | C-22 |
| 59 | `spawn_blocking` join path, the cross-session stall | G5 | C-32 |
| 60 | peer-connection teardown, long soak | G5 | C-32 |
| 61 | rolling intra refresh, the recovery benefit | G5 | C-26 |
| 65 | `CloudClient::spawn` reported error | G5 | C-27 |
| 66 | Ctrl↔Command remap, typed | G1 | M1-18 |
| 67 | play-cursor clamp with a real audio client | G4 | 3.16 (L-15) |
| 68 | low-latency engine period, the request branch | G4 | 3.12 (L-11) |
| 70 | `audio-bridge` deleted, under load | G4 | 3.16 (L-15) |
| 73 | Opus constrained VBR packet-size distribution | G4 | 3.17 (L-16) |
| 74 | DataChannel backpressure under congestion | G5 | C-18 |
| 75 | Opus-track fallback spacing (NetEQ path) | G5 | C-07 |
| 77 | one process-wide tokio runtime, 3 adapters | G5 | C-33 |
| 79 | `MACOSX_DEPLOYMENT_TARGET` on an artifact | G1 | M1-27 (a) |
| 81 | `single_instance` constant-time compare, loopback path | DEV | §6 sweep |
| 82 | the CI workflows' first run | G1 | M1-28 (**owner-gated**; otherwise stays open) |
| 97 | keyframe → IDR within a frame period on a static desktop | G4 | 3.21 |
| 106 | DXGI 3-slot staging ring + `dxgi-encode` thread | G4 | 3.11 (L-02h) |
| 107 | DXGI downscale tier 2 (no video processor) | G4 | 3.4 (L-03) |
| 110 | legacy playthrough setpoint | G2 | M2-07, §10 |
| 112 | tap aggregate `BufferFrameSize` clamp | G1 | M1-07 |
| 113 | ring split-at-wrap + `convert_interleaved` fast path | G1 | M1-07, M1-07c |
| 116 | volume-key `CGEventTap` classify-and-`try_send` | G2 | M2-08 |
| 117, 169, 170 | SCK audio bridges, teardown barrier, dummy stream | G1 | M1-08 |
| 141, 155 | modern encoders' supported-property key sets | G1 | M1-03 |
| 143 | `PreferredEncoderGPURegistryID` | G2 | M2-02 |
| 144, 147 | encoder back-pressure tier 1, the after-numbers | G1 | M1-05 |
| 148 | three submit failures rebuild the encoder | G1 owner / G2 corroborating | M1-06 / M2-05 step 4 |
| 152 | SCK stream configuration | G1 owner / G2 corroborating | M1-01 / M2-01 |
| 153 | the 1500 ms first-frame commit gate | G2 owner / G1 corroborating | M2-01 / M1-01 |
| 171 | client optional input tails | G5 | C-14 |
| 172 | `wantsFallbackTrack` / `verified` | G5 | C-17 |
| 173 | `MessagePort` audio ring | G5 | C-08 |
| 174, 175, 176, 177 | four small client behaviours | G5 | C-16 |
| 178 | `NSSupportsAutomaticGraphicsSwitching` | G2 | M2-03 |
| 179 | H.264 profile negotiation, the bitrate saving | G5 | C-19 |
| 181 | relayed join under one `tokio::join!` | G5 | C-27 |
| 183 | BWE holds a stale receiver report | G5 | C-24 |
| 184 | client page substituted once into a `OnceLock` | DEV | §6 sweep |
| 186 | `created_geometry` | G1 | M1-26 |
| 187 | `stunUrls` as a real setting | G5 | C-28 |
| 190/221, 191/222 | `activate_mode` condvar settle; `setSerialNum:` | G1 | M1-19 |
| 192/223 | `bind_ip` widened to `IpAddr` | G5 | C-29 |
| 193/224 | HostAP liveness watcher | G1 | M1-24 |
| 196 | `QueryDisplayConfig` name correlation, full ladder | G4 | 3.8 (L-08) |
| 198, 199 | driver IPC resync; serialised round trips | G4 | 3.8, 3.7 (L-07, L-06) |
| 200 | `probe::finish` on Ctrl+C / panic | DEV | §6 sweep |
| 202 | `CGDisplayStreamUpdateGetDropCount` 30 s run | DEV | §6 sweep (10.15.8 box) |
| 203 | `CompatibilityReport.input_backend` | G1 | M1-16 |
| 204 | trickle ICE, host half | G5 | C-06 |
| 205 | `link_medium_for_local_ip` latency comparison | G5 | C-30 |
| 206 | `DISPLAY_CORRELATION_LOCK` release point | G1 | M1-25 |
| 209 | fast-channel `u16` sequence (host half) | G5 | C-14 |
| 210 | synthetic pen `pointerId` | G5 | C-31 |
| 213 | `CrossNetwork` starts defensive | G5 | C-18 |
| 215 | source-aware 50 ms age gate (pen) | G5 | C-31 |
| 231 | `apply_scale_live` scale-only override | G4 | 3.18 (L-18) |
| 235 | `MFX_WRN_DEVICE_BUSY` retry path | G4 | 3.20 (L-17) |
| 237 | `libvpl.dll` on a clean Intel install | G4 | 3.20 (L-17) |
| 239 | x264 BT.709 colour tags | DEV | §6 sweep |
| 245 | AMF CPU bridge, colour range | G3 | A-05 |
| 246 | slice-mode output, the AMD arm | G3 | A-13 (a) |
| 247 | `EncoderBackend`/`BackendConfig` deleted (Linux) | OOS | Linux is out of scope |
| 256 | `doctor`/`status` driver section, macOS arm | G1 | §6.2 discovery |
| 257 | `set_log_streaming(false)` on unmount | DEV | §6 sweep |
| 258, 259 | AX messaging-timeout restore; async permission probes | G1 | M1-15 |
| 260 | macOS 15+ mid-session re-consent watchdog | G1 | M1-14 |
| 261 | `airplay::probe()` in `doctor` | G1 | M1-26 |
| 262 | `se-adapter-<ip>` thread name | G1 | M1-22 |
| 264, 265 | `bundle.macOS.infoPlist`; the restored CSP | G1 owner / G2 corroborating | M1-27 (e)(f) / M2-03 step 1 |
| 271 | `prepare_whep`'s single fallible region | G5 | C-05 |
| 278 | AMF stays `UNVERIFIED` | G3 | A-01 |
| 279 | transient encoder-busy — the AMD analogue | G3 | A-13 (b) |
| 279 | — the NVENC arm | DEV | §6 sweep |
| 280 | `HwPolicy`; which encoder VT picks under `Forbid` | G1 | M1-02 |
| 283 | `EnableLTR` probed and reported | G1 | M1-04, **M1-29** |
| 284, 295 | AirPlay `created_geometry` | G1 | M1-26 |
| 285 | client trickle ICE, time to first frame | G5 | C-06 |
| 286 | host `u16` reorder sequence on the fast channel | G5 | C-14 |
| 287 | LTR repair — the *rate* | G5 | C-25 |
| 288 | keyframe storm under injected loss | G5 | C-24 |
| 293 | VideoToolbox instance-limit contention | G2 | M2-05 |
| 296 | `software_encode_now()` live handle, end-to-end swap | G1 | M1-02 |
| 298 | `probe_encoder` by `Vendor::from_pci_id`, AMD arm | G3 | D-01 |
| 299 | `set_gated_*` (`HasProperty` + write + read-back) | G3 | D-02, A-06 |
| 300 | `classify_query_output` | G3 | D-07 |
| 301 | ladder-routed synthetic loop; `serve --encoder` | G3 | D-03, D-06 |
| 302 | `AsyncEncodeRing` over `RingEncoder`/`RingTextures` | G3 | §10.2 |
| 303 | AMF LTR wired to `RepairState` | G3 | D-08, §10.4 |
| 304 | `amd/sps.rs` SPS VUI rewrite | G3 | §10.3 |
| **new** | **VideoToolbox LTR — the token-shaped adapter** | **G1** | **M1-29** |

### 7.2 `docs/CAPABILITY-TIERS.md` — every non-`VERIFIED` row

| Subsystem row | Guide | Task |
|---|---|---|
| System audio capture (Windows) — the 19045 refusal | G4 | 3.13 (L-12) |
| Engine period (Windows audio) — the request branch | G4 | 3.12 (L-11) |
| Audio format conversion (Windows) — tiers 2 and 3 | G4 | 3.14 (L-13) |
| System audio (Windows) — the "none" branch | G4 | 3.15 (L-14) |
| Audio loss robustness | G5 | C-18 |
| Screen Recording authorization (macOS) — SCK tier, 15/26 | G1 | M1-11 |
| System Settings deep link (macOS) | G1 | M1-12 |
| App relaunch for a new grant (macOS) | G1 | M1-13 |
| H.264 encode (Windows) — the AMD rungs | G3 | A-01 |
| CPU-bridge readback format — the AMF half | G3 | A-04 |
| AMF usage tier | G3 | A-03 |
| AMF capture/encode overlap | G3 | §10.2 |
| AMF loss recovery | G3 | §10.4 |
| AMF encoder input format | G3 | A-04 |
| Hosted network (Windows) — association and throughput | G4 | 3.9 (L-09) |
| Legacy hosted-network capability — positive + localized | G4 | 3.9, 3.10 |
| Virtual display (Windows) — the two failure tiers | G4 | 3.7 (L-06) |
| Screen capture (Windows) — pre-26100 | G4 | 3.1 (L-01) |
| DXGI duplication entry point | G4 | 3.3 (L-02) |
| DXGI downscale (Windows) — tier 2 | G4 | 3.4 (L-03) |
| Virtual-display name correlation (Windows) | G4 | 3.8 (L-08) |
| Driver IPC protocol (Windows) | G4 | 3.8 (L-07) |
| oneVPL dispatcher (Windows) | G4 | 3.20 (L-17) |
| Quick Sync slice layout (Windows) | G4 | 3.20 (L-17) |
| NVENC concurrent sessions — under real clients | G5 | C-32 |
| Client video transport | G5 | C-01 |
| Client keyframe request | G5 | C-02 |
| Client presentation | G5 | C-04 |
| Client audio decode | G5 | C-07 |
| Client audio ring | G5 | C-08 |
| Client audio output latency | G5 | C-09 |
| Client audio sink selection | G5 | C-10 |
| Client screen wake | G5 | C-11 |
| Client pointer input · Client coalesced input | G5 | C-12 |
| Client fullscreen | G5 | C-13 |
| Client input tails · Client fast-channel sequence | G5 | C-14 |
| Client ICE signalling | G5 | C-06 |
| Congestion signal — tiers 2 and 3 | G5 | C-23 |
| Send pacing — tiers 2 and 3 | G5 | C-21 |
| RTP MTU · Playout-delay hint | G5 | C-22 |
| Socket DSCP / send buffer · Media socket · DSCP marking (Windows) | G5 | C-20 |
| STUN servers | G5 | C-28 |
| Streamer bind family | G5 | C-29 |
| Local link medium | G5 | C-30 |
| Picture recovery | G5 | C-25 |
| H.264 profile · H.264 level | G5 | C-19 |
| Hosted Wi-Fi network (macOS) + security type + liveness + DHCP | G1 | M1-24 |
| Adapter-change source (macOS) | G1 | M1-23 |
| Display mode settle (macOS) — the saving | G1 | M1-19 |
| Virtual-display DPR (macOS) · mode list (macOS) | G1 | M1-19 |
| `CGVirtualDisplaySettings` extras (macOS) | G1 | M1-20 |
| CGDisplayStream symbols (macOS) — on 15/26 | G1 | M1-21 |
| Capture backend (macOS) — the SCK frame gate | G2 owner / G1 | M2-01 / M1-01 |
| VideoToolbox session spec (macOS) | G1 | M1-02 |
| VideoToolbox property support (macOS) — modern key sets | G1 | M1-03 |
| Long-term reference frames (macOS) | G1 | M1-04, **M1-29** |
| Encoder back-pressure (macOS) — tier 1 | G1 | M1-05 |
| System audio capture (macOS) — Process Tap · SCK audio | G1 | M1-07, M1-08 |
| System audio capture (macOS) — the driver tier on a modern OS | G2 | M2-07 |
| macOS audio tier ordering | G1 | M1-10 |
| Legacy-tier capture transport — the HAL-input fallback | G2 | M2-07 step 6 |
| macOS volume keys while the virtual device is default | G2 | M2-08 |
| macOS audio driver load — tier 1 (signed, SIP on) | G1 owner / G2 | M1-27 (l) / M2-09 step 10 |
| macOS 10.15 floor enforcement — tiers 2 and 3 | G1 | M1-27 (c)(d), M1-28 |
| Remote input injection | G1 | M1-16 |
| Sender RTX (**REJECTED**) · Video codec (**NOT A TIER**) | — | recorded rejections; no guide |

### 7.3 `docs/manual-tests/*` — every step that needs other hardware

| Script | Guide | Where |
|---|---|---|
| `amd-amf.md` §1–§12 | G3 | A-01 … A-13, D-05, §10.2–§10.4 |
| `client-page.md`, all sections | G5 | C-01 … C-16 |
| `core-closeout.md` §1 (two pens) | G5 | C-31 |
| `core-closeout.md` §2 (input backend) | G1 | M1-16 |
| `core-closeout.md` §3 (localized hosted network) | G4 | 3.10 (L-10) |
| `core-closeout.md` §4 (audio redundancy) | G5 | C-18 |
| `core-closeout.md` §6 (virtual-display tiers) | G4 | 3.7 (L-06) |
| `core-closeout.md` §7 (four clients) | G5 | C-32 |
| `latency-harnesses.md` | DEV + all | each guide's Phase 2 |
| `macos-app-signing.md`, `macos-audio-driver-signing.md`, `macos-build.md` | G1 | M1-27 |
| `macos-audio.md` tier 1, tier 2 | G1 | M1-07, M1-08 |
| `macos-audio.md` tier 3 + "Verifying the driver directly" | G2 | M2-07, M2-09 |
| `macos-capture-encode.md` §1–§4, §7, §7b, §10 | G1 | M1-01 … M1-05, M1-20 |
| `macos-capture-encode.md` §2 (Intel threshold), §6, §9 | G2 | M2-02, M2-05, M2-06, M2-10 |
| `macos-hosted-network.md` §1–§9 | G1 | M1-24 |
| `macos-permissions.md` §1–§9 | G1 | M1-11 … M1-15, M1-27 (j)(k) |
| `transport.md` §1 (wired vs wireless) | G5 | C-30 |
| `transport.md` §2 (DSCP) | G5 | C-20 |
| `transport.md` §3 (trickle) | G5 | C-06 |
| `transport.md` §4 (H.264 profile) | G5 | C-19 |
| `transport.md` §5 (repair rate) | G5 | C-25 |
| `windows-audio-tiers.md` §1–§4 | G4 | 3.12 … 3.17 |
| `windows-capture.md` §1–§8 | G4 | 3.1 … 3.6 |

### 7.4 `docs/RISK-REGISTER.md` §7 — "Unverifiable on the dev box"

| Id | Guide | Task |
|---|---|---|
| R-227 WGC pre-24H2 hard-fail | G4 | 3.1 |
| R-228 AMD AMF tier | G3 | A-01 |
| R-229 x264 4:4:4 output | DEV | §6 sweep (the register itself says "this box") |
| R-230 `libvpl.dll` on a real Intel host | G4 | 3.20 |
| R-231 hybrid-adapter selection | G4 Windows half / G3 AMD half | 3.5 / A-12 |
| R-232 every macOS 12.3+ path | G1 owner / G2 12.x band | M1-01/02/07/08 / M2-01 |
| R-233 Apple Silicon (scroll ABI, HostAP abort) | G1 | M1-17 |
| R-234 universal artifacts, x86_64-only `libopus.dylib` | G1 | M1-27 (g) |
| R-235 Developer ID, TCC identity, translocation | G1 | M1-27 (h)(i)(j)(k) |
| R-236 `nm -u` on a real build | G1 | M1-27 (c) |
| R-237 CGDS queue-depth starvation | DEV | §6 sweep (10.15.8 box) |
| R-238 hardware-vs-software VideoToolbox | DEV | §6 sweep |
| R-239 `cgds::probe_screen_recording()` on 10.15 | DEV | §6 sweep |
| R-240 the legacy audio driver — behaviour | DEV | §6 sweep |
| R-240 — the signed `.pkg`, SIP on | G1 | M1-27 (l) |
| R-240 — the tier on a supported modern OS | G2 | M2-07, M2-09 |
| R-241 the AirPlay tier being *selected* | **none** | §5.1 — a scope decision, not hardware |
| R-242 does macOS honour `maxFPS = 1` | G1 | M1-26 |
| R-243 does a modern sender complete the handshake | G1 | M1-26 |
| R-244 every client-browser matrix claim | G5 | C-01 … C-16 |
| R-245 the Safari 13.1 desktop-UI floor | DEV | §6 sweep |
| R-246 cold-start wall time on 10.15 | DEV | §6 sweep |
| R-247 cross-network / TURN / cloud relay | G5 | C-27 |
| R-248 `cargo test` red / CI never run — the test half | DEV | §6 sweep |
| R-248 — the workflows' first run | G1 | M1-28 (owner-gated) |

Also placed, from outside §7: **R-249/R-426** (`unpark` in the real-time audio callback) → G1
M1-07c; **R-293** (two IOSurfaces pinned against a queue depth of 2) → G1 M1-01; **R-315/R-317**
(`amd.md` hygiene) → G3 D-04; **R-225** (version-gated tier tests) → G1 M1-10; **R-260** (the
macOS 15 re-consent watchdog) → G1 M1-14; **R-227** also underpins G4 3.21's WGC contract question.

### 7.5 Result

| Bucket | Count |
|---|---|
| Ledger rows placed in a guide | 96 |
| Ledger rows recorded as dev-box or out-of-scope | 17 |
| Capability-tier rows placed in a guide | 66 |
| Capability-tier rows that are recorded rejections rather than tiers | 2 |
| Manual-test sections placed | 23 |
| Risk-register §7 entries placed in a guide | 16 |
| Risk-register §7 entries recorded as dev-box | 8 |
| **Unplaced after the audit** | **1** — R-241, which needs a scope decision, not a machine |

Five rows are physically reachable by two guides, and each names one owner: **148** (G1), **152**
(G1), **153** (G2), **264/265** (G1), and the *macOS audio driver load* tier (G1). Each
non-owning guide carries a one-line clause saying it records a second data point rather than
flipping the status.

One item was **created** by this audit: **M1-29** in `modern-mac.md`, the VideoToolbox
long-term-reference adapter, absorbed from
`findings/major-vt-ltr-frames-would-replace-idr-on-every-pli.md`. The finding file was then deleted
per the findings lifecycle, the design carried into the guide, and a new `docs/OPTIMIZATIONS.md` row
added reading `UNVERIFIED — see guides/modern-mac.md`.

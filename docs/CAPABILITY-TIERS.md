# Capability tiers

Every OS/browser API surface is selected at runtime by **probing the capability**, never by
reading a version number. The rule for every tiered subsystem:

```
probe → select the newest tier that actually works on this machine → fall back on failure, at runtime, without crashing
```

Requirements per subsystem: a `probe_*()` that attempts the real capability (class lookup,
`dlsym`, `GetProcAddress`, trial API call, feature-level query); results cached in a `OnceLock`
where static for the process and recomputed where they can change (install state, permissions,
hotplug); graceful runtime fallback if the selected tier fails *after* selection; the chosen
tier reported through `CompatibilityReport` and the log; on macOS, construction via
`dlsym` / `AnyClass::get` / `msg_send!` only (no link-time references newer than 10.15).

Verification status per tier: `VERIFIED` (executed and measured here), `COMPILE-CHECKED`
(builds and passes static/layout tests, never run), `UNVERIFIED` (written from research; no
local signal). The two development machines are Windows 11 26200 + RTX 3080 Ti Laptop (hybrid
graphics) and macOS 10.15.8 Intel, so most modern-macOS tiers and every AMD tier are at best
`COMPILE-CHECKED` here.

This table is filled in as tiers are added or audited; it is not reconstructed at the end.

| Subsystem | Tier (newest first) | Probe | Era / platform | Expected delta | Status |
|-----------|---------------------|-------|----------------|----------------|--------|
| System audio capture (Windows) | 1. **Endpoint loopback** — legacy `IAudioClient::Initialize(SHARED, LOOPBACK\|EVENTCALLBACK)` on the `eRender`/`eConsole` endpoint, with a mandatory silent render companion · 2. **Process loopback** — `ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK)` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` and `EXCLUDE_TARGET_PROCESS_TREE` on our own pid | tier 1 is unconditional (Vista+); tier 2 is probed by **attempting the activation and the `Initialize`**, never by a build number — `ActivateAudioInterfaceAsync` exports on builds that do not support this activation type | Win 10 1607+ / Win 10 20348+ respectively / Windows | measured equal: wake p99 9.44 ms (tier 1) vs 10.49 ms (tier 2); capture->packet p50 5.77 vs 6.29 ms | VERIFIED (both tiers exercised with `probe --audio [--process-loopback]` on Win 11 26200); the "refuses on Win 10 19045" half is UNVERIFIED — needs a second machine |
| Engine period (Windows audio) | 1. **Low-latency** — `IAudioClient3::InitializeSharedAudioStream` on the *render companion* at `GetSharedModeEnginePeriod`'s minimum, which drags the whole endpoint down and the loopback capture stream with it · 2. **Default** — legacy `Initialize`, whatever the engine grants | `QueryInterface(IAudioClient3)` then `min < default`; falls back on `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED`, `AUDCLNT_E_INVALID_DEVICE_PERIOD` or any other refusal, and never fails capture | Win 10 1607+ / Windows | -5 to -7.3 ms on hardware that exposes a small buffer; **0 ms** where the driver exposes one period | VERIFIED as a correctly-detected **no-op** on the dev box (`default = fundamental = min = max = 480 frames`); the request branch is COMPILE-CHECKED and needs the in-box High Definition Audio Device driver (documented 128-480 frames) to exercise |
| Audio format conversion (Windows) | 1. **Endpoint is already 48 kHz f32 stereo** — mix format taken verbatim, no conversion · 2. **Engine converts** — `AUTOCONVERTPCM \| SRC_DEFAULT_QUALITY` · 3. **Userspace converts** — capture the endpoint's own format and downmix/int-convert in `format.rs` | the format comparison itself for tier 1; the `Initialize` return for tier 2, which now falls through to tier 3 instead of failing the feature | all / Windows | tier 3 costs CPU, not latency; it is what keeps 44.1 kHz DACs, 24-bit interfaces and 5.1/7.1 outputs working when the engine refuses to convert | VERIFIED tier 1 (the dev box endpoint is 48 kHz f32 stereo); tiers 2 and 3 are COMPILE-CHECKED and unit-tested — tier 3 was unreachable in production until now |
| Audio loss robustness | 1. **Packet-level redundancy** — previous frame's Opus payload appended behind `FLAG_REDUNDANT`, enabled when the client advertises it **and** the BWE reports >=0.5 % loss · 2. **Nothing** — the client's own concealment | the join request's `audioCapabilities.redundancy`, plus the live loss estimate | all | makes any single packet loss invisible for ~2x payload (~118 B against 50-70 B of framing already paid) | COMPILE-CHECKED on the host; **inert end to end** until a client advertises the capability, which is W11's half. In-band FEC is *not* a tier: it is LPC-layer-only and cannot exist under `RESTRICTED_LOWDELAY` at 5 ms |
| Screen Recording authorization (macOS) | 1. `CONFIRMED` latch · 2. `CGPreflightScreenCaptureAccess` **positive only** · 3. `CGWindowListCopyWindowInfo` + `kCGWindowName` · 4. `SCShareableContent` (background) · 5. cached preflight negative | `dlsym` for the preflight; the window-list read needs no probe (10.5+); `AnyClass::get("SCShareableContent")` inside `sck::probe_screen_recording` | 10.15+ / macOS | window-list read replaces a 700 ms-5 s capture probe; 0.6 ms warm | VERIFIED (10.15.8: preflight absent, window-list probe live); UNVERIFIED (SCK tier, 15/26) |
| Accessibility authorization (macOS) | 1. `AXIsProcessTrusted()` · 2. system-wide `AXUIElementCopyAttributeValue` (background secondary; a timeout means *unknown*, never denied) | direct call; no version gate | 10.15+ / macOS | primary read is microseconds; the 20-23 ms secondary leaves the hot path | VERIFIED (10.15.8) |
| System Settings deep link (macOS) | 1. `com.apple.settings.PrivacySecurity.extension?Privacy_*` (Ventura+) · 2. `com.apple.preference.security?Privacy_*` (pre-13) | `open` exit status — the modern id is tried first and the legacy one runs only if it fails | 10.15+ / macOS | correct pane anchor on 13+ instead of the top of Privacy & Security | UNVERIFIED (needs 13/14/15/26; the dev box only has the legacy pane) |
| App relaunch for a new grant (macOS) | 1. `open -n -a <bundle>` via LaunchServices · 2. `AppHandle::restart()` | bundle layout probe (`…/Contents/MacOS/`); falls back when not running from an `.app` | 10.15+ / macOS | the new process is its own TCC responsible process instead of an heir of the dying one | UNVERIFIED (needs a bundled build; the dev box runs the bare binary) |

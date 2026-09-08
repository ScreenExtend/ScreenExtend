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
| Screen Recording authorization (macOS) | 1. `CONFIRMED` latch · 2. `CGPreflightScreenCaptureAccess` **positive only** · 3. `CGWindowListCopyWindowInfo` + `kCGWindowName` · 4. `SCShareableContent` (background) · 5. cached preflight negative | `dlsym` for the preflight; the window-list read needs no probe (10.5+); `AnyClass::get("SCShareableContent")` inside `sck::probe_screen_recording` | 10.15+ / macOS | window-list read replaces a 700 ms-5 s capture probe; 0.6 ms warm | VERIFIED (10.15.8: preflight absent, window-list probe live); UNVERIFIED (SCK tier, 15/26) |
| Accessibility authorization (macOS) | 1. `AXIsProcessTrusted()` · 2. system-wide `AXUIElementCopyAttributeValue` (background secondary; a timeout means *unknown*, never denied) | direct call; no version gate | 10.15+ / macOS | primary read is microseconds; the 20-23 ms secondary leaves the hot path | VERIFIED (10.15.8) |
| System Settings deep link (macOS) | 1. `com.apple.settings.PrivacySecurity.extension?Privacy_*` (Ventura+) · 2. `com.apple.preference.security?Privacy_*` (pre-13) | `open` exit status — the modern id is tried first and the legacy one runs only if it fails | 10.15+ / macOS | correct pane anchor on 13+ instead of the top of Privacy & Security | UNVERIFIED (needs 13/14/15/26; the dev box only has the legacy pane) |
| App relaunch for a new grant (macOS) | 1. `open -n -a <bundle>` via LaunchServices · 2. `AppHandle::restart()` | bundle layout probe (`…/Contents/MacOS/`); falls back when not running from an `.app` | 10.15+ / macOS | the new process is its own TCC responsible process instead of an heir of the dying one | UNVERIFIED (needs a bundled build; the dev box runs the bare binary) |

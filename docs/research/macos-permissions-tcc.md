# macOS permissions & TCC — why "granted in System Settings" still reports missing

## 1. Question

ScreenExtend's first-run showstopper on new Macs: the user grants **Screen Recording** (and
**Accessibility**) in System Settings, the toggle is visibly ON, and the app still reports the
permission missing and refuses to start. Establish the real macOS behaviour behind this — the
caching semantics of `CGPreflightScreenCaptureAccess` / `AXIsProcessTrusted`, which probes read
live TCC state, how TCC binds a grant to a code identity, what macOS 15/26 changed, and what
mature screen-capture apps actually ship — then diagnose the local code and specify the fix.

---

## 2. Findings

### 2.1 `CGPreflightScreenCaptureAccess()` never reflects a mid-process grant. This is by design.

Both functions are macOS 10.15+, resolvable by `dlsym` on every host ScreenExtend supports.

- `CGRequestScreenCaptureAccess()` **returns immediately**; it does not block waiting for the
  user. It may cause a *separate* process to show a dialog pointing at System Settings. Its
  return value is the *current* (i.e. pre-decision) answer.
- `CGPreflightScreenCaptureAccess()` returns the current authorization — but the answer a
  process gets is bound to that process and is not refreshed when the user grants.

Quinn "The Eskimo!" (Apple DTS) on the forums, explaining the "*<App> may not be able to record
the contents of your screen until it is quit*" alert:

> "A single user-visible privilege can cover multiple APIs. Some of their implementations react
> to change better than others, and in some cases it's determined by how the app uses the API.
> Thus, the system can't tell whether your app will or won't see the new privilege immediately."

and, critically:

> the permission change may take effect immediately for the actual screen capture functionality
> (like `CGWindowListCreateImage()`), **but `CGPreflightScreenCaptureAccess()` will not reflect
> this change until the application is restarted.**

Sources:
[Apple DevForums 732726 — "Understanding CGRequestScreenCaptureAccess"](https://developer.apple.com/forums/thread/732726),
[Apple DevForums 695689 — "macOS application screen recording"](https://developer.apple.com/forums/thread/695689).

So the *capture APIs* are the live read; the *preflight* is the stale one. This is the exact
inversion of the assumption in `permissions.rs`.

There is no version on which this is safe to rely on as a negative. It reproduces on 10.15
through 26. Electron shipped Chromium's implementation
(`ui/base/cocoa/permissions_utils.mm`: `bool IsScreenCaptureAllowed() { return
CGPreflightScreenCaptureAccess(); }`) and has the identical user-facing bug filed against it —
[electron/electron#36722, "getMediaAccessStatus('screen') doesn't retrieve new status once
changed in macOS Preferences"](https://github.com/electron/electron/issues/36722), closed as
not planned. Chromium/Electron are *not* a model to copy here.

### 2.2 Probes that read live TCC state without a relaunch

| Probe | Floor | Prompts? | Cost | Live after grant? | Notes |
|---|---|---|---|---|---|
| `CGWindowListCopyWindowInfo` + `kCGWindowName` visibility | 10.15 | no | ~µs, no session | **yes** | Without the grant, other processes' windows carry **no `kCGWindowName`** (and `kCGWindowSharingState` reads 0). Widely used; the canonical implementation is [soffes/canRecordScreen.swift](https://gist.github.com/soffes/da6ea98be4f56bc7b8e75079a5224b37). Must exclude own PID and the Dock. |
| `SCShareableContent.getShareableContentWithCompletionHandler` | 12.3 | no | async; can be slow | **yes** | `error != nil` with `SCStreamErrorUserDeclined` (**-3801**) = denied. Authoritative. But it is documented to hang or take 5+ s ([nonstrict-hq/SCShareableContent-hangs-sample](https://github.com/nonstrict-hq/SCShareableContent-hangs-sample), [Apple DevForums 804083](https://developer.apple.com/forums/thread/804083)) — never call it synchronously. |
| `CGDisplayStreamCreate` returning non-NULL / delivering a frame | 10.15 | no | opens a real capture session | **NO — false positive** | An unauthorized process still gets a stream and still gets `FrameComplete` frames; the frames simply contain wallpaper + menu bar + its own windows. Confirmed on 26.x in [trycua/cua#870](https://github.com/trycua/cua/issues/870): windows present in `CGWindowListCopyWindowInfo`, TCC row `auth_value=2`, and capture returning desktop-only. Same for `CGDisplayCreateImage` / `CGWindowListCreateImage`. |
| `CGPreflightScreenCaptureAccess` | 10.15 | no | ~µs | **no** (see 2.1) | Usable only as a *positive* signal. |

`CGDisplayCreateImage` / `CGWindowListCreateImage` / `CGDisplayStream` are additionally
deprecated (14.0/15.0) and are what triggers Sequoia's "may be attempting to bypass security
settings" messaging; Apple's stated replacement is ScreenCaptureKit
([nashaofu/xcap#160](https://github.com/nashaofu/xcap/issues/160)).

### 2.3 Accessibility: `AXIsProcessTrusted()` has the same per-process cache

`AXIsProcessTrusted()` / `AXIsProcessTrustedWithOptions()` read a per-process cache populated on
first call; macOS binds the answer to the process and granting afterwards does **not** upgrade a
running process in place. Apple exposes no `AXResetTrustedCache()` and no change notification.
The documented remedy is the same: quit and relaunch. Threads:
[99868](https://developer.apple.com/forums/thread/99868) ("returns false when I restart my
application"), [649501](https://developer.apple.com/forums/thread/649501) ("does not work after
change setting manually"),
[openowl.dev, *Fix macOS Permissions for Computer Use Agents*](https://openowl.dev/blog/macos-accessibility-screen-recording-permissions).

The cheapest call that consults **live** Accessibility state is
`CGEventTapCreate(kCGSessionEventTap, …, kCGEventTapOptionListenOnly, …)`: if the tap is created,
the process holds the privilege right now; if it returns NULL, it genuinely does not. This is what
agent tooling uses when `AXIsProcessTrusted()` is suspected stale.

**Spurious `AXUIElement…` failures are real and are not a permission signal.**
[Apple DevForums 703188](https://developer.apple.com/forums/thread/703188): Accessibility APIs
sporadically return `nil`/empty while `AXIsProcessTrusted()` returns `true`. Quinn's two theories
are (a) a broken/changed code signature and (b) the Accessibility subsystem itself faulting — in
neither case is a single failed `AXUIElementCopyAttributeValue` evidence of a missing grant. His
recommended handling is to detect the anomalous state (`AXIsProcessTrusted() && no data`) and ask
for a sysdiagnose, **not** to report the permission as missing.

Note also a documented side effect: `AXUIElementSetMessagingTimeout` called on the **system-wide**
element sets the default timeout for *all* accessibility messaging in the process, not just that
one call.

### 2.4 TCC identity binding — the "toggle is ON but the running binary is a different subject" class

- A TCC row is keyed by `(service, client/bundle-id)` and carries a **`csreq`** blob — the code
  requirement captured *at grant time*
  ([TN3127 *Inside Code Signing: Requirements*](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements)).
  `auth_value=2` (allowed) is what draws the checkmark in System Settings; the `csreq` match is
  what actually authorizes the call. They can disagree, and when they do, the UI lies.
- **Ad-hoc signing** (`codesign --sign -`, Xcode's "Signed to Run Locally", and what `cargo`/
  `tauri dev` produce) yields a designated requirement pinned to the exact **cdhash**, which
  changes on every rebuild. tccd then logs `Failed to match existing code requirement for
  subject … and service kTCCServiceScreenCapture` while the toggle stays checked. Quinn's
  guidance is explicit: use a **stable** signing identity (Apple Development for dev, Developer
  ID for distribution) and **never mix ad-hoc and Developer ID on one bundle id**
  ([Apple DevForums 730043](https://developer.apple.com/forums/thread/730043),
  [695689](https://developer.apple.com/forums/thread/695689)).
- **macOS 15 raised the floor further:** `CGPreflightScreenCaptureAccess()` / ScreenCaptureKit are
  reported to require a binary signed with a Developer ID certificate carrying a **Team ID**;
  ad-hoc identities get `false` regardless of what the user granted
  ([CapSoftware/Cap#1722](https://github.com/CapSoftware/Cap/issues/1722) — four independent
  workarounds tried, all failed; the only fix was a debug-build bypass).
- **macOS 26 (Tahoe)** additionally appears to require a real **`.app` bundle** for an item to be
  listed in the Screen Recording pane at all ([trycua/cua#870](https://github.com/trycua/cua/issues/870)).
- **Responsible process.** A binary launched from Terminal inherits Terminal's TCC attribution —
  Terminal's grant applies, and the app never appears in the list on its own. This is why
  `pnpm tauri dev` and a Finder double-click behave differently, and why a support report of
  "granted but missing" can be an artefact of how the app was started.
- **App Translocation (Gatekeeper Path Randomisation).** An app with `com.apple.quarantine`,
  opened via Launch Services, and never moved *by Finder*, is copied to a read-only
  `/private/var/folders/…/AppTranslocation/<uuid>/d/ScreenExtend.app` and run from there. The
  three conditions are exact; moving via Finder disables it permanently, moving via `mv` does
  not ([lapcatsoftware](https://lapcatsoftware.com/articles/app-translocation.html),
  [Eclectic Light](https://eclecticlight.co/2023/05/09/what-causes-app-translocation/)).
  Detect with `SecTranslocateIsTranslocatedURL` (`<Security/SecTranslocate.h>`, 10.12+, so it is
  link-safe below the 10.15 floor), or by testing whether the bundle path contains
  `/AppTranslocation/`. Consequences: self-update breaks, and the identity the user sees in the
  Screen Recording list is not the path that is running.
- Diagnosis from the shell: `codesign -dv --verbose=4 /Applications/ScreenExtend.app` (Identifier,
  TeamIdentifier, CDHash, flags), `codesign -d -r- <path>` (the designated requirement itself),
  `spctl -a -vvv <path>`, `xattr -p com.apple.quarantine <path>`, `csrutil status`, and
  `tccutil reset ScreenCapture app.screenextend.desktop`. Reading
  `~/Library/Application Support/com.apple.TCC/TCC.db` requires Full Disk Access and is a
  support-only path, not something to ship in the app.

### 2.5 macOS 15 Sequoia and 26 changes

- Screen-recording **re-consent** was introduced in Sequoia: weekly plus on every reboot in the
  early betas, softened to **monthly** in beta 6 with the reboot trigger removed
  ([9to5Mac](https://9to5mac.com/2024/08/14/macos-sequoia-screen-recording-prompt-monthly/)),
  and softened again in 15.1 for regularly-used apps. There is no way for an app to dismiss it
  permanently. The approval dates live in the TCC-protected
  `~/Library/Group Containers/group.com.apple.replayd/ScreenCaptureApprovals.plist`
  ([Michael Tsai](https://mjtsai.com/blog/2024/08/08/sequoia-screen-recording-prompts-and-the-persistent-content-capture-entitlement/),
  [Eternal Storms](https://blog.eternalstorms.at/2024/09/23/psa-no-more-macos-15-sequoia-monthly-screen-recording-permission-reminders/)).
- The prompt applies to ScreenCaptureKit users too; only `SCContentSharingPicker` (which makes the
  user pick a target every time) avoids it — useless for ScreenExtend, which captures a virtual
  display it created itself.
- **`com.apple.developer.persistent-content-capture`** suppresses the recurring prompt. Apple's
  documentation says it targets **VNC applications**, requested through a form on
  developer.apple.com with essentially no public guidance. ScreenExtend is a VNC-class product
  (a persistent remote display for the machine's own user), so it is a plausible applicant.
- Apple documents **no `NSScreenCaptureUsageDescription`** key — Screen Recording has no
  developer-supplied usage string; the alert text is fixed. (Community advice to add one is
  cargo-culting; it is harmless but does nothing.) `NSAudioCaptureUsageDescription` *is* real and
  is what the Core Audio process tap consents against — ScreenExtend already ships it
  (`src-tauri/Info.plist`).
- Settings deep links: the Ventura+ pane id is `com.apple.settings.PrivacySecurity.extension`
  (`…?Privacy_ScreenCapture`, `…?Privacy_Accessibility`, `…?Privacy_Microphone`). The legacy
  `com.apple.preference.security?Privacy_ScreenCapture` still redirects on many 13/14 builds but
  is not guaranteed and has been reported broken on 26 for some anchors
  ([rmcdongit/macOS Settings URL gist](https://gist.github.com/rmcdongit/f66ff91e0dad78d4d6346a75ded4b751)).

---

## 3. How others do it

- **Chromium / Electron** — `IsScreenCaptureAllowed() { return CGPreflightScreenCaptureAccess(); }`
  in `ui/base/cocoa/permissions_utils.mm`. It has exactly ScreenExtend's bug
  (electron#36722, electron#38190), closed as not planned. Chromium mitigates only by *warming*
  the permission early (`ShouldWarmScreenCapture()`), not by probing live.
- **OBS Studio** — `CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess`, and its
  own [macOS Permissions Guide](https://obsproject.com/kb/macos-permissions-guide) tells users
  the app must be **fully quit and reopened** after granting; the forums are full of
  "remove OBS from the list, re-add, restart" threads. OBS's answer to the stale read is
  documentation plus relaunch, not a smarter probe.
- **Screen-capture utilities and agent tooling** (CleanShot/Shottr-class apps, computer-use
  agents) converge on the `CGWindowListCopyWindowInfo` + `kCGWindowName` heuristic as the live
  read, because it is the only cheap prompt-free probe that reflects a grant immediately. The
  soffes gist is the shared ancestor of most implementations.
- **Everyone** treats "quit and relaunch" as a first-class UI state rather than an error, and
  offers a one-click restart. The correct macOS relaunch is via LaunchServices —
  `open -n -a /Applications/ScreenExtend.app` (or `NSWorkspace.openApplication`) followed by
  `exit(0)` — so the new instance gets its own responsible-process attribution instead of
  inheriting the dying parent's.
- **LetsMove** (used by dozens of shipping Mac apps) is the standard answer to translocation:
  detect that the bundle is not in `/Applications`, offer to move it, and relaunch from the new
  path.
- No shipping product was found that makes a *missing* permission read fatal to startup. They all
  degrade: the app opens, the affected feature is marked unavailable, and the prompt is raised at
  the moment the feature is used.

---

## 4. What ScreenExtend does today

`src-tauri/src/macos_utils/permissions.rs` (217 lines) exposes three Tauri commands.

**Screen Recording** — `screen_recording_granted()` (`permissions.rs:123-158`):

1. `permissions.rs:124` — `if CONFIRMED.load(Relaxed) { return true }` (one-way latch).
2. `permissions.rs:127-129` — `if preflight_screen_capture() == Some(false) { return false }`.
3. `permissions.rs:130-151` — the `PROBE` state machine (`started` / `running` / `last`,
   `PROBE_REFRESH = 2 s` at `:38`) that spawns `probe_and_latch()` on a background thread.
4. `permissions.rs:152-154` — `if !first { return false }`.
5. `permissions.rs:155-157` — the very first call runs `probe_and_latch()` **synchronously**.

`preflight_screen_capture()` (`:98-100`) `dlsym`s `CGPreflightScreenCaptureAccess`, which
resolves on every supported host, so step 2 is always reached with a concrete answer.

`probe_and_latch()` (`:115-121`) calls `crate::macos_utils::streamer::probe_screen_recording()`
(`macos_utils/streamer/mod.rs:107-114`), which uses `sck::probe_screen_recording()`
(`sck.rs:754-768`, `recv_timeout(5 s)` at `:767`) when `screencapturekit_available()`
(`mod.rs:80` — a **version check via a `sw_vers` fork**, `mod.rs:61`), and otherwise
`cgds::probe_screen_recording()` (`cgds.rs:221-268`), whose entire verdict is
`rx.recv_timeout(700 ms).is_ok()` on a 64×64 `CGDisplayStream` (`cgds.rs:265`).

**Accessibility** — `accessibility_trusted()` (`:40-42`) = `AXIsProcessTrusted() &&
accessibility_api_enabled()`. The helper (`:44-60`) sets a 0.5 s messaging timeout on the
**system-wide** element (`:50`) and does a synchronous `AXUIElementCopyAttributeValue` for
`AXFocusedApplication`, returning `err != AX_ERROR_API_DISABLED` (`-25211`, `:36`, `:58`).

**Commands** — `check_permissions()` (`:170-190`) is a non-`async` `#[tauri::command]` and so runs
on the main thread; both entries are `required: true`. `request_permission` (`:192-200`),
`open_permission_settings` (`:202-217`) emits only the **pre-Ventura** URLs
(`com.apple.preference.security?Privacy_ScreenCapture` / `?Privacy_Accessibility`, `:206-211`).

**UI** — `src/pages/bootstrap.tsx`: `ensurePermissions()` (`:205-223`) filters
`p.required && !p.granted` (`:212`); `proceed()` (`:244-272`) returns early at `:246` if anything
is missing, so `runSetup` — and therefore `commands.setup()` — never runs. The modal (`:547-636`)
polls `checkPermissions()` every `PERMISSION_POLL_MS = 200` (`:29`, `:311`) plus on `focus`
(`:312`), and offers Quit / Re-check / Relaunch (`:600-633`). `relaunch()` is
`@tauri-apps/plugin-process`, whose Rust side spawns the binary directly
(`Command::new(path).spawn()` then `exit(0)`) rather than going through LaunchServices.

**Diagnostics** — `cli.rs::run_doctor` (`:1386-1434`) prints only
`compatibility::check_system_requirements()` (`macos_utils/compatibility.rs:34-85`: OS name/version,
`os_supported`, unsupported APIs, audio/display backend tiers) and the two permission booleans.
Nothing about signing identity, bundle path, translocation, SIP, or the responsible process.

**Signing** — `src-tauri/tauri.macos.conf.json` sets `minimumSystemVersion: 10.15` and an
`Entitlements.plist` with the `com.apple.CG.virtual-display` mach-lookup exception,
`app-sandbox=false` and `device.audio-input`. There is no persistent-content-capture entitlement,
and `Info.plist` is not referenced by either Tauri config (see
`findings/minor-build-mac-info-plist-is-never-referenced-by-either-tauri-config.md`).

### 4.1 The precise interleaving — a permanent lock, not a race

Take a Mac where Screen Recording has never been granted. `CONFIRMED=false`,
`PROBE{started:false, running:false, last:None}`.

| t | Actor | Code | Result |
|---|---|---|---|
| 0 | `Bootstrap.proceed()` → `ensurePermissions()` (`bootstrap.tsx:208`) | `check_permissions` → `screen_recording_granted()` | `CONFIRMED` false → `preflight == Some(false)` → **return false at `:128`**. `PROBE.started` is still `false`; `probe_and_latch()` was never called. |
| +ε | `ensurePermissions` (`bootstrap.tsx:215`) | `request_permission("screen_recording")` → `request_screen_recording()` (`:160`) | `screen_recording_granted()` false again → `CGRequestScreenCaptureAccess()` returns immediately (it does **not** wait for the user) and the app is added to the Screen Recording list. |
| +ε | `bootstrap.tsx:218` | `setPermReport(...)` | Gate opens. `proceed()` returned before `runSetup`. |
| 0.2 s, 0.4 s, … | poll (`bootstrap.tsx:311`) | `screen_recording_granted()` | Every call hits `preflight == Some(false)` at `:127` and **returns at `:128`, before the `PROBE` block at `:130`.** |
| ~30 s | user | ticks ScreenExtend in System Settings; clicks **Later** on "…until it is quit" | TCC row now `auth_value=2`. `SCShareableContent` would now succeed. |
| 30.2 s → ∞ | poll | `screen_recording_granted()` | Still `:128`. `st.started` is *still* `false`. `probe_and_latch()` is **never called even once**. `CONFIRMED` can never be set. |

The gate never closes. `proceed()` is never re-entered, `runSetup` never runs, no virtual display
is created, no server binds. The only escapes are the Relaunch button (`bootstrap.tsx:620`) or
quitting by hand — and the modal copy never says a relaunch is mandatory.

Two things make this a *lock* rather than a slow convergence:

- `CGPreflightScreenCaptureAccess` is guaranteed present ≥ 10.15, so `preflight_screen_capture()`
  is never `None` and the `Some(false)` branch is always live.
- Per §2.1, the preflight provably never flips to `true` in-process. So the predicate at `:127` is
  *monotonically* false for the life of a process that started denied. The entire `CONFIRMED` /
  `PROBE` machinery below it — which exists precisely to defeat the stale preflight — is
  **unreachable dead code** on the failing path.

Even with line `:127-129` deleted, the design still misbehaves, for three independent reasons:

1. `if !first { return false }` (`:152-154`) makes every call but the first return a hard `false`
   instead of the last known value, so the gate can only close via `CONFIRMED`, i.e. no earlier
   than `PROBE_REFRESH (2 s) + probe latency + up to 200 ms of poll`.
2. `st.running` is cleared only by the spawned closure itself (`:141-143`). A panic anywhere in
   `probe_screen_recording()` (objc2 `msg_send!`, `RcBlock`, `AnyClass::get`) leaves
   `running == true` forever, `due && !st.running` is never satisfied again, and the answer is
   `false` for the rest of the process. The panic is swallowed and unlogged. The three
   `PROBE.lock().unwrap()` sites (`:131`, `:141`, `:156`) then compound it via poisoning, and
   `bootstrap.tsx:308` silently discards the resulting command error forever.
3. Below 12.3 the probe is `cgds::probe_screen_recording()`, which is a *liveness* check being used
   as an *authorization* check. §2.2 confirms an unauthorized process still receives
   `FrameComplete` frames (wallpaper-only). That latches `CONFIRMED = true` while recording is
   denied — the mirror-image failure, and unrecoverable because the latch is one-way. This
   promotes `findings/speculative-permissions-cgds-probe-can-return-true-while-recording-is-denied.md`
   from speculative to confirmed-by-external-evidence.

And on 12.3+, if the SCK probe times out (`sck.rs:767`; documented to hang), `mod.rs:107-114`
falls through to that same unreliable CGDS probe.

Finally, `check_permissions` is synchronous on the main thread (`:170`) and the first call runs the
probe inline (`:155`), so the very first `checkPermissions()` can freeze the UI for up to 5 s on a
12.3+ Mac; and `accessibility_api_enabled()` adds an AX round trip with a 0.5 s ceiling to each of
the 5 calls/second.

---

## 5. Recommendations

Ranked by "unblocks a broken first run" per unit of work. None of these change the appearance of
the UI; (R3) adds one line of copy and re-labels an existing button.

### R1 — Never let a negative preflight terminate the read. *(blocker; ~10 lines)*

`findings/blocker-permissions-preflight-shortcircuit-locks-the-gate.md`

```rust
// positive-only: a true preflight is trustworthy, a false one is not
if preflight_screen_capture() == Some(true) {
    CONFIRMED.store(true, Ordering::Relaxed);
    return true;
}
// fall through to the live probe — do NOT return on Some(false)
```

Platform: all (10.15+). Probe: `dlsym` as today. Win: converts an infinite hang into a
sub-second convergence. Measure: wall time from ticking the toggle to the gate closing.

### R2 — Make the window-name read the primary live probe. *(blocker; ~40 lines)*

`findings/major-permissions-window-name-probe-is-the-live-tcc-read.md`

`CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID)`; scan for a window
whose `kCGWindowOwnerPID` is neither our PID nor the Dock and that has a `kCGWindowName`. Present
since 10.15, no prompt, no capture session, microseconds, and it reflects a grant **immediately**.
Ladder: window-name probe → (12.3+) SCK `SCShareableContent` for confirmation, off-thread → never
CGDisplayStream. Return `Option<bool>`: "no candidate window at all" is `None` (unknown), not
`false`. Win: removes the 700 ms–5 s probe from the hot path *and* removes the CGDS false
positive. Measure: time `check_permissions` (target < 5 ms), and revoke/grant while running.

### R3 — Ship "relaunch required" as a first-class state with a LaunchServices relaunch. *(major)*

`findings/major-permissions-relaunch-required-ui-and-one-click-restart.md`

Some grants (notably Accessibility, §2.3) genuinely cannot be picked up in-process. After
`request_permission` has fired once and the probe still reads denied for > ~3 s, promote Relaunch
to the primary action and say plainly that macOS requires it. Relaunch through LaunchServices —
`open -n -a <bundle path>` (or `NSWorkspace.openApplication`) then `exit(0)` — not Tauri's raw
`Command::spawn`, so the new process gets its own responsible-process attribution.

### R4 — Degrade, never die, on an unknown or wrong permission read. *(major)*

`findings/major-permissions-degrade-do-not-die-on-a-wrong-permission-read.md`

Make the bootstrap gate advisory: `proceed()` must reach `runSetup` when a required permission
reads `Unknown`, and the *capture* failure (which is the ground truth) must surface as a
device-level error rather than a startup refusal. Pairs with the existing
`findings/blocker-permissions-accessibility-required-blocks-all-startup.md` (Accessibility should
not be `required` at all).

### R5 — Emit the Ventura+ Settings URL. *(minor; ~6 lines)*

`findings/major-permissions-settings-url-wrong-pane-on-ventura-plus.md`

Try `x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_ScreenCapture`
first and fall back to the legacy `com.apple.preference.security?…` if `open` fails.

### R6 — Replace the AX side-probe with a live, side-effect-free one. *(minor)*

`findings/minor-permissions-ax-messaging-timeout-is-set-process-wide.md`

`accessibility_api_enabled()` only ever fails on `kAXErrorAPIDisabled` (-25211), a pre-10.9
condition modern macOS does not produce — so it contributes no signal while costing up to 0.5 s of
main thread per call and, per §2.3, silently setting the process-wide AX messaging timeout. Delete
it; if a live read is wanted, use a listen-only `CGEventTapCreate` and immediately release it.

### R7 — `doctor` must dump the TCC-relevant identity, not just two booleans. *(major)*

`findings/major-cli-doctor-omits-tcc-signing-and-translocation-state.md`

Add: bundle path + whether it contains `/AppTranslocation/` (or `SecTranslocateIsTranslocatedURL`);
`codesign -dv --verbose=4` Identifier / TeamIdentifier / CDHash / flags and `codesign -d -r-`
(the designated requirement); `xattr -p com.apple.quarantine`; `csrutil status`; the responsible
process / parent (`getppid` + its executable); the SCK/CGDS backend actually selected; the raw
values of `preflight`, the window-name probe and the SCK probe **separately**; and the exact
`tccutil reset ScreenCapture app.screenextend.desktop` line for support to paste.

### R8 — Guarantee a stable Developer ID identity, and detect translocation at launch. *(major)*

`findings/major-build-mac-release-must-be-developer-id-signed-with-stable-designated-requirement.md`

macOS 15 rejects screen capture for ad-hoc identities outright, and any csreq drift shows the user
a ticked toggle over a grant that does not apply. Verify the release artefact's designated
requirement is stable across versions, never mix ad-hoc with Developer ID on
`app.screenextend.desktop`, and add a LetsMove-style "move to /Applications" offer when the bundle
is translocated or running from `~/Downloads`.

### R9 — Plan for Sequoia's monthly re-consent. *(speculative)*

`findings/speculative-permissions-sequoia-monthly-reconsent-will-interrupt-long-sessions.md`

The 30-day dialog will interrupt an active stream. Apply for
`com.apple.developer.persistent-content-capture` (Apple scopes it to VNC-class apps; ScreenExtend
qualifies on the merits), and until then make the host survive a mid-session revoke by detecting
capture failure and re-prompting rather than streaming a frozen or wallpaper-only frame.

---

## 6. Verification plan

Before/after, on a Mac with Screen Recording never granted:

1. **Grant-to-gate-close latency.** Instrument `screen_recording_granted()` with a `tprintln!` of
   `(CONFIRMED, preflight, window_name_probe, sck_probe, started, running, last.elapsed())`. Tick
   the toggle, click *Later*, measure to `runSetup`. Today: never. Target: < 1 s.
2. **`check_permissions` cost.** Time the command from the renderer. Today: 700 ms–5 s on the first
   call, plus up to 0.5 s AX per call. Target: < 5 ms, always.
3. **False-positive test (the CGDS probe).** On the 10.15 box, with Screen Recording revoked, run
   `cgds::probe_screen_recording()` from an `#[ignore]`d test and record the result; then capture a
   frame with a titled third-party window on screen and confirm whether it is excluded. This is the
   one local measurement that settles the speculative finding on this repo's own floor.
4. **Identity drift.** `codesign -d -r- ScreenExtend.app` on two consecutive releases; the
   designated requirements must be identical. Then `tccutil reset ScreenCapture
   app.screenextend.desktop`, grant, upgrade, and confirm the grant survives.
5. **Translocation.** Run the DMG copy straight from `~/Downloads` and confirm `doctor` reports the
   `/AppTranslocation/` path and the app offers to move itself.

**Cannot be verified on the dev boxes.** The macOS box is 10.15.8 Intel, Command Line Tools only:
ScreenCaptureKit (12.3+) and the SCK probe cannot run there at all; the Sequoia Developer-ID
enforcement, the monthly re-consent, the Tahoe `.app`-bundle requirement, and the Ventura+ Settings
URL all need 13/15/26 hardware. Windows can verify nothing here — `check_permissions` returns an
empty list. Items 3 and 5 *are* testable on 10.15; items 1, 2 and 4 are testable there in part
(CGDS path only).

---

## 7. Open questions

1. On 14.x/15.x, does `CGPreflightScreenCaptureAccess` ever pick up a grant live? Quinn's answer
   and every reproduction found say no, but no source states it as a version-scoped guarantee, so
   R1 is written to be correct either way.
2. Does the window-name probe stay reliable on 26 (Tahoe)? trycua/cua#870 shows `kCGWindowName`
   present while capture returned desktop-only on 26.1 — if names are visible without the grant on
   that release, the probe degrades to a positive-only signal there and the SCK path must carry it.
   Needs a 26 machine.
3. Is `com.apple.developer.persistent-content-capture` grantable to a product like ScreenExtend,
   and what does Apple's form require? No public criteria exist.
4. Does the responsible-process attribution change when Tauri's `relaunch()` spawns the binary
   directly from a dying parent? If it does, R3's `open -n` is not merely tidier but required.
5. Does `macos_at_least(12, 3)` (a `sw_vers` fork, `mod.rs:61`) ever misreport under
   `SYSTEM_VERSION_COMPAT`? It is a version gate where the doctrine demands a probe
   (`AnyClass::get("SCShareableContent")` is already used inside `sck.rs:755` and would suffice) —
   tracked separately in `findings/major-compat-mac-capabilities-gated-by-a-sw-vers-subprocess.md`.

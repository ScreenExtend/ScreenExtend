# Manual test: macOS permissions and the first-run gate

Everything below needs hardware the project does not have. The development Mac is
10.15.8 Catalina with SIP disabled, so it can exercise the window-list probe, the
Accessibility probes and `doctor`, and nothing else — no ScreenCaptureKit, no
Sequoia re-consent, no Developer ID signing, no System Settings (it still ships
System Preferences). Run this script on **13, 14, 15 and 26** before calling the
first-run gate fixed.

Record, for every step: the macOS build, what the UI showed, what
`ScreenExtend doctor --json` reported, and the wall time from the user action to
the UI reacting. `doctor`'s `macos.probes` object is the ground truth — the whole
point of printing the probes separately is that a disagreement between them names
the failure.

## What "correct" means

| Situation | Required behaviour |
|---|---|
| Screen Recording denied | Gate opens, Screen Recording row shows a red X |
| Screen Recording granted while running | Gate closes within one poll (200 ms) **or** the modal switches to the restart wording |
| Screen Recording unreadable | App starts anyway; the failure appears when a device connects |
| Accessibility denied | App starts and streams; the row stays visible, marked optional |
| Any state | `check_permissions` returns in well under 100 ms |

The gate must never sit on "Checking for changes…" forever. That is the bug this
whole script exists to catch.

---

## 1. Fresh install, permission never granted

1. On a Mac that has never run ScreenExtend: `tccutil reset ScreenCapture app.screenextend.desktop`
   and `tccutil reset Accessibility app.screenextend.desktop`, then move
   `ScreenExtend.app` to `/Applications` **in Finder** and launch it from there.
2. Expect: the permission modal, Screen Recording with a red X, Accessibility
   marked optional, and the macOS "ScreenExtend would like to record this screen"
   prompt.
3. Run `ScreenExtend doctor --json` from a second terminal. Note especially
   `macos.probes.cgPreflightScreenCaptureAccess` versus
   `macos.probes.windowListNameProbe` — they are allowed to disagree, and which way
   they disagree is the diagnosis.
4. **Fail if** the app reaches the dashboard with Screen Recording denied, or the
   modal never appears.

## 2. Deny, and stay denied

1. Dismiss the OS prompt without granting. Leave the modal open for 60 s.
2. Expect: the row stays red, the modal stays responsive, no beachball, and — after
   about 3 s — the amber box switches from "After granting a permission" to
   "Restart needed".
3. Click **Open Settings** on the row. On 13+ System Settings must open *scrolled to
   and highlighting* Screen Recording, not the top of Privacy & Security. This is
   the Ventura pane-id fallback; if it lands at the top of the pane, record which
   macOS build did it.
4. **Fail if** the UI freezes at any point, or Open Settings misses the anchor.

## 3. Grant while the app is running — the showstopper

This is the case the whole rewrite exists for.

1. With the modal open, tick ScreenExtend in System Settings → Privacy & Security →
   Screen Recording. macOS will offer "Quit & Reopen" / "Later". **Click Later.**
2. Return to ScreenExtend. Start a stopwatch at the moment the toggle went on.
3. Expect **one** of:
   - the gate closes and setup runs (target: under 1 s, and it will only happen if
     at least one other app has a titled window on screen — the window-list probe
     needs something to read); or
   - the modal says a restart is needed and the blue **Restart** button is the
     obvious next action.
4. Repeat the whole step with **every other app hidden and no other window on
   screen**. The window-list probe returns unknown there, so the gate is expected
   to fall back to the restart wording rather than closing. Confirm it does not
   report a false "granted".
5. **Fail if** the modal sits on "Checking for changes…" indefinitely with no
   restart advice. That is the original bug.

## 4. Quit and relaunch

1. From the state above, click **Restart**.
2. Expect: the app disappears and comes back; Screen Recording reads granted;
   setup runs; the dashboard appears.
3. In a terminal, `ps -o pid,ppid,command -p $(pgrep -x ScreenExtend)`. The parent
   must be `launchd` (pid 1), **not** a dead pid or another ScreenExtend. That is
   the whole reason the relaunch goes through `open -n -a` instead of spawning the
   binary: TCC attributes a request to the responsible process, and an heir of a
   dying parent inherits the parent's attribution.
4. Confirm in Console.app that no `tccd` line reads
   `Failed to match existing code requirement for subject`.
5. **Fail if** the relaunched process is parented to anything but launchd, or the
   permission still reads denied after the restart.

## 5. Translocated launch straight from Downloads

App Translocation needs all three conditions: the bundle carries
`com.apple.quarantine`, it is opened through Launch Services, and it has never been
moved *by Finder*.

1. Download the release DMG in Safari. Copy `ScreenExtend.app` out of it into
   `~/Downloads` **with `cp` in a terminal**, not by dragging in Finder (dragging
   clears the flag permanently).
2. `xattr -p com.apple.quarantine ~/Downloads/ScreenExtend.app` must print a value.
3. Double-click it in Finder.
4. `ScreenExtend doctor --json` must report `macos.translocated: true` and an
   `macos.executable` path under `/private/var/folders/…/AppTranslocation/…`.
5. Expect: the permission the user grants applies to a path that will not exist next
   launch, so grants do not stick. Record whether the app says anything useful about
   this. It currently does not offer to move itself to /Applications. The real fix is a
   Developer ID signature with a stable designated requirement, which is also what stops
   translocation happening at all.
6. **Fail if** `doctor` reports `translocated: false` for a launch that is visibly
   running from `/AppTranslocation/`.

## 6. Re-signed binary — the csreq mismatch

A TCC row carries the code requirement captured at grant time. Ad-hoc signatures
pin it to the exact cdhash, so every rebuild breaks the match while System Settings
keeps showing a checkmark.

1. Grant Screen Recording to a Developer ID-signed `ScreenExtend.app`. Confirm
   capture works.
2. `codesign -d -r- /Applications/ScreenExtend.app` — save the designated
   requirement string.
3. Re-sign ad-hoc: `codesign --force --deep --sign - /Applications/ScreenExtend.app`.
4. Relaunch. Expect: System Settings still shows the toggle ON, but capture fails.
5. `ScreenExtend doctor` must show `code object is not signed at all` / no
   TeamIdentifier in the designated requirement block, which is what identifies this
   cause. On macOS 15 specifically, an ad-hoc identity is rejected for screen
   capture outright regardless of the toggle — record whether the probes reflect
   that.
6. Repeat with two consecutive **release** builds: their designated requirements
   must be byte-identical, or every update silently drops the user's grant.
7. **Fail if** `doctor` cannot distinguish this from a genuine denial.

## 7. Sequoia (15+) monthly re-consent, mid-session

1. On macOS 15 or later, start a stream and leave a device connected.
2. Force the re-consent rather than waiting 30 days: edit the approval date
   backwards in
   `~/Library/Group Containers/group.com.apple.replayd/ScreenCaptureApprovals.plist`
   (needs Full Disk Access), or wait it out.
3. Expect the OS dialog over a live session. Dismiss it *without* re-approving.
4. Trigger a new pipeline start — reconnect the device, or change a display setting
   so the stream refreshes.
5. Expect in the log terminal (Settings → the log pane): a `[tcc]` line saying
   Screen Recording is not authorized and that macOS 15+ re-asks about every 30
   days.
6. **Fail if** the client keeps receiving frames showing only wallpaper and the menu
   bar with no error anywhere. Frame delivery is not proof of authorization —
   an unauthorized process still gets `FrameComplete` frames.
7. Known gap: there is no re-probe *while* a session streams, only at pipeline start, so
   a grant revoked mid-session is never noticed by the host.

## 8. Accessibility is optional

1. Leave Accessibility denied and Screen Recording granted.
2. Expect: the app reaches the dashboard and streams video. The Accessibility row is
   still listed, marked optional.
3. Turn Remote Control on for a connected device and confirm input injection reports
   its own failure rather than the app having refused to start an hour earlier.
4. **Fail if** a denied Accessibility grant blocks startup.

## 9. Cost

1. With the modal open, watch the UI for 60 s (the gate polls at 200 ms).
2. `ScreenExtend doctor --json | jq '.macos.probes'` — `windowListNameProbeUs`
   should be roughly 500-1500 µs warm. On the 10.15.8 box it measured 37 ms cold and
   0.6 ms warm (median of five consecutive calls).
3. `axSystemWideElementUs` is the expensive one — 20-23 ms on 10.15.8 — which is why
   it is a background secondary probe and not part of the primary read.
4. **Fail if** the modal stutters, or `check_permissions` is visibly slow.

---

## Notes from the 10.15.8 box

- `CGPreflightScreenCaptureAccess` **does not exist** on 10.15.8: the `dlsym` returns
  null and `doctor` reports the probe as `null` / `unknown`. The research report
  assumed it resolves on every supported host. It does not, which is another reason
  the preflight can only ever be a positive signal.
- The window-list probe returns `Some(true)` there for a binary launched over SSH,
  because the responsible process (the shell) holds the grant. Attribution, not the
  binary, is what TCC keys on — which is exactly why step 4 checks the parent pid.
- `doctor` exits 0 when everything is fine, 1 when the OS is unsupported, and 3 when
  the OS is fine but a required permission reads denied.

---

## 8. The mid-session re-probe (W13b) — needs macOS 15+

`macos_utils::start_session_watchdog` re-reads `permissions::screen_recording_live()` every
30 s **while at least one session holds a live display**, and acts only on a definite
`Some(true) -> Some(false)` transition. Nothing on the 10.15.8 dev box can produce that
transition, so this is the script for a Sequoia machine.

Why it exists: macOS 15 re-asks for screen-recording consent roughly every 30 days. If the
user dismisses that dialog, the capture APIs keep delivering frames — they just contain
wallpaper, the menu bar and our own windows — so before this the host reported a healthy
stream while the client showed a blank desktop, and nothing appeared in the log until
something happened to restart the pipeline.

1. Start the host and join a device. Confirm the stream is live.
2. Revoke Screen Recording for ScreenExtend in System Settings (or move the approval date
   backwards in
   `~/Library/Group Containers/group.com.apple.replayd/ScreenCaptureApprovals.plist`, which
   needs Full Disk Access, and dismiss the dialog without re-approving).
3. Within ~30 s the host log must show
   `[tcc] Screen Recording authorization was revoked while streaming.` and the connected
   device must be dropped rather than left watching wallpaper.
4. Re-grant, relaunch, reconnect: the stream comes back.

Two things this deliberately does **not** do. It never treats a `None` read as a refusal —
`screen_recording_live()` returns `None` when it could not learn the answer (no other
titled window on screen), and acting on that would reproduce the false-positive defect the
window-list probe was introduced to fix. And it does not raise the permission modal on the
host: that needs a new event and a fourth button in the gate, which is a UI change and is
recorded as a recommendation in `docs/OPTIMIZATIONS.md` instead.

The same watchdog polls `airplay::take_geometry_changed()` when the display backend is the
AirPlay tier, and bumps the reconfig epoch for every session when macOS has moved the
display underneath us. To exercise that (pre-10.14.4 tier only): resize the AirPlay display
from Displays preferences during a live session and confirm the client re-negotiates on its
next `/reconfig` poll without a restart, with `[airplay] macOS changed the display
geometry` in the log.

---

## One prompt, then silence: the prompt policy (needs a human, ~10 min)

What it checks: the rule "poll with silent checks only after the first request; no more than
one prompt unless the user explicitly presses Retry". The policy itself
(`permissions::prompt_policy`) and the call-site pins are unit-tested; what no test can do is
watch the screen while a person grants the permission. Run on macOS 15 or 26 (the releases
where `SCShareableContent` raises the alert on every unauthorized call — the defect this
fixes re-raised it every 2 s while the gate polled).

Setup, on a freshly built, whole-signed app (`bash scripts/check-macos-signature.sh` passes):

```sh
tccutil reset ScreenCapture app.screenextend.desktop
tccutil reset Accessibility app.screenextend.desktop
rm -f ~/Library/Application\ Support/app.screenextend.desktop/permission-requests.json
```

1. Launch the app from `/Applications` in Finder. Expect **exactly one** macOS
   screen-recording prompt. Count every system dialog that appears from here to step 4.
2. Do not answer it for 30 s, then press "Open System Settings" in the prompt (or dismiss it
   and use the gate's **Open Settings** button). Leave System Settings in front for at least
   60 s. Expect: no further prompt covers System Settings. `permission-requests.json` now
   holds a `screen_recording` row with `count: 1`.
3. Turn ScreenExtend on. Expect: the gate either closes on its own or switches to the
   "Restart needed" wording. Press **Restart**. Expect: the relaunched app shows no prompt
   at all (the automatic request after a relaunch is a silent read) and passes the gate.
4. Total system dialogs counted: **1**. Anything more is a failure; note which step.

Retry, and the stale-row repair:

5. Turn ScreenExtend **off** in System Settings, quit, relaunch. Expect: the gate, no prompt,
   and the row's text "macOS has already asked once… press Retry".
6. Put **at least two other apps' windows** on screen (say Finder and TextEdit: a definite
   denial needs ≥ 2 untitled foreign windows from ≥ 2 apps), then press **Retry** on the Screen
   Recording row. Expect one log line `[permissions] screen_recording reads as definitely
   denied (on-screen-windows-untitled) on an explicit Retry; … running tccutil reset
   ScreenCapture app.screenextend.desktop before asking again`, then exactly one prompt, and the
   row's text changes to "ScreenExtend cleared its Screen Recording entry and asked macOS
   again — the entry was either switched off or no longer matched this copy…".
6b. Repeat step 6 with **no** other window on screen (Show Desktop, or a fresh empty Space).
   Expect the log line `Retry without a TCC reset — the silent read is Unknown
   (no-foreign-window-to-read)` and **no** reset: an unconfirmable state is never reset.
7. **Open Settings** must only open the pane: pressing it repeatedly raises no prompt.

## Granted while running, then Show Desktop (the stale-cache case, needs a human, ~5 min)

What it checks: `CGPreflightScreenCaptureAccess` stays `false` for the life of a process that
started unauthorized, so a host granted *while running* must not treat that cached negative as
a denial when there is nothing else on screen to read. Unit-tested as
`granted_while_running_then_nothing_on_screen_is_unknown_and_harmless`; this is the screen half.

1. Reset as in the setup above, launch from `/Applications`, answer the one prompt with
   "Open System Settings", turn ScreenExtend on. **Do not restart.** The gate should close (the
   System Settings window's title is readable now) — if it shows "Restart needed" instead,
   that is acceptable, press nothing and carry on from the dashboard if it lets you.
2. Join from a client device and confirm the picture is your desktop, not wallpaper.
3. Press Show Desktop (F11 / fn-F11 or the hot corner) so no other app's window is on screen,
   and minimise everything. Leave it 60 s (two watchdog re-probes at 30 s).
   Expect: **the session stays up**, no "Screen Recording Is Off" dialog, no
   `[tcc] Screen Recording is definitely not authorized while streaming` log line.
4. With the desktop still shown, disconnect and rejoin from the client. Expect: the join
   succeeds (the log may say `state could not be confirmed (…); starting capture anyway`, or
   nothing if the grant was already confirmed). It must **not** say the permission is denied.
5. If system audio is on for that device on macOS 13.0–14.1 (the ScreenCaptureKit audio tier),
   expect audio to start; the log must not say `ScreenCaptureKit audio is not started`.
6. Press Retry in the gate or the lapse dialog if either is up. Expect **no** `tccutil reset`
   line: the read is `Unknown` or `Granted`, never a definite denial, after a confirmed grant.

## `serve` only, first run (needs a human, ~5 min)

What it checks: a headless host makes the one first request itself. Before this, nothing in
`serve` ever requested Screen Recording, so the app never appeared in the Screen Recording list.

1. Reset as in the setup above. Do **not** open the GUI at any point.
2. Start the host the way it will run in production — through LaunchServices, so the request is
   attributed to the app and not to Terminal:
   `open -n -a /Applications/ScreenExtend.app --args serve --no-cloud`.
   Expect exactly one macOS screen-recording prompt naming ScreenExtend, and a
   `screen_recording` row in `permission-requests.json`.
3. Grant it, `ScreenExtend stop`, start it again the same way. Expect no prompt at all.
4. Started instead from a shell (`/Applications/ScreenExtend.app/Contents/MacOS/ScreenExtend
   serve`), the request is attributed to Terminal: expect a prompt naming Terminal at most once
   per run (unless Terminal is already allowed), the log line `requested from a process that does
   not own the app's TCC identity … remembered for this process only`, and
   `permission-requests.json` **unchanged**. The same holds for `pnpm tauri dev` / `cargo run`:
   a dev session must never write the marker and suppress the `.app`'s one automatic prompt.

Record the macOS build, the number of dialogs at each step, and the contents of
`permission-requests.json` after steps 2 and 6.

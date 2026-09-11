# Manual tests — desktop control UI (UX-C, ledger rows 309–321)

The signed-off UI-visible changes from `.claude/briefs/ui17.md`. Everything here builds, lints,
passes `check-legacy-webkit.mjs` and boots to the dashboard with zero `[ui]` lines in the log
ring — what is left needs someone looking at the window, a second device, or an OS neither dev
box runs. Rows 309–321 in `docs/OPTIMIZATIONS.md`.

Run the app with `pnpm tauri dev`. For the log-bus half, `ScreenExtend logs --lines 400` from a
second shell reads the running instance's backlog; a `[ui]` line is a frontend `console.error`.

---

## 1. The walkthrough still runs, and the app looks the same before it does (row 310)

The library that draws the tour is no longer in the entry chunk, and the two wrapper elements it
used to render around the app are now rendered statically.

1. With `walkthroughCompleted: false` in `config.json`, launch. The tour should auto-start on the
   dashboard exactly as before — same card, same arrow, same highlight ring, same 0.4 s card
   transition.
2. Step through all six steps and press **Done**. Then Profile menu → **Replay Tour**: it must
   start again from step one.
3. With the tour *closed*, inspect the DOM (`pnpm tauri dev` gives devtools). Under `#root` there
   must be `div[data-name="nextstep-wrapper"][data-nextstep="dev"]` with
   `position: relative; width: 100%`, wrapping `div[data-name="nextstep-site"]` with
   `display: block; width: 100%`.
4. Collapse the sidebar and confirm `#innerSidebar` is still positioned against that wrapper
   (below the `lg` breakpoint, where it is `position: absolute`) — resize the window narrow
   enough to cross 1024 px if the window will go that small.

**Expect:** identical appearance and behaviour. A tour that opens with the highlight ring in the
wrong place means the static wrapper's box does not match what the library produced.

## 2. The sidebar collapse (row 314)

Click the panel toggle in the header a few times.

**Expect:** 300 ms, same easing. The panel now **slides** left and fades out where it used to be
clipped from the right, and the page content takes the freed width at the start of the animation
rather than growing through it. That is the signed-off change. Watch for the panel being clipped
by the row (it must not be — `overflow-hidden` moved to the row and the panel is `z-10` above the
content pane) and for any horizontal scrollbar appearing mid-animation.

## 3. Tooltip arrows (row 311)

Hover any `TipLabel` in the device sheet that anchors left or right (narrow the window until the
tooltip flips to a side).

**Expect:** the arrow sits at the **vertical centre** of the tooltip body. Before this it sat at
the top edge.

## 4. Monaco gestures and the context menu (row 313)

Settings → the `config.json` editor.

1. Alt+click in two places: two cursors.
2. Ctrl/Cmd+click in two places: two cursors.
3. Shift+click: extends the selection.
4. Right-click in the editor, in the network-name field and in any other `<input>`: the system
   menu appears, with cut/copy/paste.
5. Right-click on the dashboard background: no menu (still suppressed).
6. Ctrl+R, Cmd+R and F5: nothing happens.
7. Ctrl/Cmd+click a link in the app: no new window.

## 5. Hosted network: the tri-state, the details and the open-network banner (rows 316, 317)

Settings → Create Hosted Network → start one.

**Expect:** a badge appears under the toggle. It reads **Advertised** while the AP has no IPv4 on
its adapter and **Verified** once it has, with the SSID beside it and a list of Created with /
Channel / Band / Security / This computer below. The panel refreshes every 10 s, so an AP the OS
takes down on its own turns the badge off within that window.

On macOS only: if this Mac's HostAP SPI refuses every secured `securityType` (the log says
`this Mac accepted none of the secured security types`), a red banner must appear and **stay** for
as long as the network is up, saying the network is unencrypted and that the join link, session id
and OTP are readable by anyone in range. Windows never reaches this: all three of its tiers require
the passphrase.

Windows shows no channel — neither WinRT tier exposes one, and asking `netsh` would put a process
spawn on a path the UI polls.

## 6. IPv6-only join URL (row 315)

Needs a network with IPv6 and no IPv4 on the adapter (an IPv6-only SSID, or disabling IPv4 on the
adapter).

**Expect:** the dashboard shows a QR card for that adapter with `http://[<addr>]:8080/?id=<id>`,
and a client on the same network joins from it. Before this the dashboard said "Join or Create a
Network (none were found)" while the host was serving. A `fe80::` address must **not** produce a
card.

## 7. Screen Recording revoked mid-session (row 318) — macOS 15+

Per `docs/manual-tests/macos-permissions.md` §8: with a device connected and streaming, revoke
Screen Recording in System Settings.

**Expect:** within 30 s the device drops (already the case), *and* the permission dialog opens over
whatever page is showing, headed "Screen Recording Was Turned Off", with Dismiss / Open Settings /
Restart. Re-tick the permission: the dialog closes on its own within ~3 s if the grant reaches this
process, and otherwise stays up telling you to restart — which is the honest answer, since macOS
gives a new grant only to a freshly launched app.

Nothing emits this event off macOS; on Windows the only thing to check is that nothing regresses.

## 8. The screen-recording indicator note (row 319) — macOS 13.0–14.1

On a Mac in that band, open a device sheet and turn **System Audio** on.

**Expect:** a line under the toggle saying the menu-bar recording indicator is expected because
this macOS has no audio-only capture. It must **not** appear on 14.2+ (Process Tap), on 10.15–12.x
(the driver tier) or on Windows.

## 9. Restrict cursor to this display (rows 320, 344)

The control is in the device sheet under Remote Control and only shows while remote control is
on. It is a **checkbox**, and the default is **unchecked**: mouse and keyboard reach every
desktop, which is the shipped behaviour and the owner's ruling.

1. Open the sheet on a device that has never had the setting saved. The box must be clear and
   the helper text under it must say the cursor reaches every screen.
2. Tick it, Save, reopen: still ticked, and `config.json`'s `devices[].pointerScope` reads
   `"display"`. Untick, Save, reopen: clear, and the key reads `"all"`.
3. **The migration.** Delete `pointerScope` from a device entry in `config.json` by hand and
   relaunch. The box must come back *clear* — an upgraded install must not find its devices
   silently fenced in. (`ScreenExtend devices list` shows `all` in the POINTER column for the
   same entry.)

The behaviour half — that ticking it actually stops the cursor at the display's edge — is
`docs/manual-tests/startup-gate.md` §4, which needs a second physical monitor.

## 9b. Automatic frame rate (row 345)

Directly under the Refresh Rate slider, a **checkbox**, default **unchecked**.

1. Unchecked is the shipped state: the helper text must say the stream runs at the rate above
   and never lowers it. Save with it clear and `config.json`'s `devices[].autoFrameRate` reads
   `false`; a device saved before the setting existed carries no key at all and must still read
   as unchecked.
2. Tick it, Save, reopen: still ticked, key reads `true`. `ScreenExtend devices list` shows it
   in the AUTOFPS column, and `ScreenExtend devices set <ip> --auto-framerate off` clears it
   (with a host running, the "(The running host reloaded its configuration.)" line confirms the
   `RELOAD` replay reached `AppState`).

**Not covered here:** what the pipeline does with the flag. Delivering at a fixed cadence and
repeating an unchanged frame is the stream-regression workstream's half; this test only proves
the setting is stored, replayed and reaches `DeviceOverride`.

## 10. The client's actual audio sink (row 321)

Needs a client whose browser has its own output picker (Firefox) or one that refuses `setSinkId`
without a gesture.

**Expect:** the device sheet's audio-output row shows "Playing on: X" under the picker, and turns
amber with "still playing on X — its browser needs a tap" when the client is on a different sink
from the one the host applied. With a client that does not report the field, no line appears at
all — that is "the client did not say", not "the default".

## 11. The config.json editor renders as an editor (UX-D)

Settings → Advanced → Configuration File, in a **built** app, not `pnpm dev`: the defect this
guards against only exists in the packaged page, because it is Tauri's CSP rewriting that
triggers it.

**Expect:** JSON syntax colouring (keys, strings, numbers and punctuation in different
colours), line numbers in a dimmer colour than the text with a visible gutter, a working
scrollbar inside the editor, and the status line reading "Valid" with `0 error(s)`.

**The failure it replaces:** every character the same near-white, line numbers flush against
the text in the body colour, and one error, `Property stunUrls is not allowed`. If the
colours are gone again, open the devtools console — the editor preload logs "monaco's theme
stylesheet was rejected by the page's Content-Security-Policy" — or check the served policy:
`style-src` must read `'self' 'unsafe-inline'` with **no** `nonce-`. A nonce there means
something re-introduced an inline `<style>` into `index.html`; `pnpm test:config` fails on
that before it ships.

**Also check while you are there:** each `devices[].token`, `knownDevices[].token`,
`hostedNetworkCredentials.password` and `turnConfig.credential` renders as a row of dots with
its key name still legible beside it, and hovering or clicking into one reveals it. Editing
any value and pressing Save Config must write and re-read without an error toast.

## 12. One row per join (rows 338, 339)

A device with two tabs open holds two sessions and two displays, and the Devices page is where
that has to be legible.

| Check | How | Expected |
|---|---|---|
| Two rows, one device | Join from two tabs of one browser | Two rows on **Devices** with the same device name. The header reads "2 Screens Connected". |
| Told apart | Look at the two row labels | The name carries a suffix: the tab's screen size where the two differ, else `(1)`/`(2)`. A device with only one session shows no suffix at all. |
| Refresh is per tab | Click the refresh button on one row | Only that tab renegotiates; the other keeps painting. |
| Settings are per device | Open the sheet on either row and change the video quality | Both tabs reconfigure, and reopening the sheet on the *other* row shows the same value. There is one saved row in `config.json`, keyed by `token`. |
| Removing the device | "Remove device" on either row | Both sessions end and both rows go. |
| Trust is by token | Settings -> known devices, with two tabs open | One row for the device, marked Connected. Banning it ends both sessions. |
| A recycled address | Ban a device, disconnect it, then join from a different device that picks up its address | The new device joins normally and is not marked banned. Its settings are its own. |

## 13. The host never ends on a settings change, and always says why it did (UX-EXIT)

A host that disappears leaves no OS record when it exits cleanly, so the only usable evidence is
the one the app writes itself. These checks are what make a repeat of the silent-exit report
answerable in one command.

| Check | How | Expected |
|---|---|---|
| A settings change never ends the host | With a device connected, open its sheet, move **Refresh Rate** and **Pixel Ratio** together, Save | The device rebuilds its session once and comes back. The host window is still there; `ScreenExtend status --json` answers. |
| The change is the one that landed | `ScreenExtend logs \| findstr "virtual display created"` | The new display carries the refresh rate you chose and `css x dpr` for its size — not `1920x1080@60`. |
| The device keeps its name | `ScreenExtend devices list --json` before and after the save | The `joins[].token` is unchanged, and the log says `re-presented a device token this host has settings for; keeping it` rather than `issued a new device token`. On a host that has never had settings for this device, minting once is correct. |
| A deliberate quit says so | Quit from the profile menu, then `ScreenExtend logs` | `The last host (pid N) ended deliberately: exit_app: the desktop UI asked the host to quit` |
| `stop` says so | Start the host, `ScreenExtend stop`, then `ScreenExtend logs` | `…ended deliberately: control server: another process sent QUIT…` |
| A kill says so too — by saying nothing | Start the host, end it from Task Manager, then `ScreenExtend logs` | `The last host left no exit record, so it did not end through any route ScreenExtend owns: it was terminated from outside, or it faulted.` |
| The record is one launch deep | Start the host again after any of the above, `ScreenExtend logs` | The startup log carries `[exit] the previous host (pid N) ended deliberately: …` once, and `screenextend.exit` in `%LOCALAPPDATA%\app.screenextend.desktop\` is gone. |

**Note on the third row.** The settings the sheet writes are keyed by the device token, and the
save bumps the reconfig epoch, which makes the client rejoin. If that rejoin is minted a *new*
token, the settings it was just given belong to nobody and the display comes back at the
defaults — which reads as "the change did nothing and the screen glitched". That is the bug
UX-EXIT fixed; this row is what catches it coming back.

## 14. An edit costs the stream only what its class costs (UX-RECONFIG)

The owner's report was that editing a device's *name* disconnected the client. Every row below is
one class of field from the table in `CLAUDE.md`'s `session.rs` section; read the verdict out of
`ScreenExtend logs` and the shape of it out of `ScreenExtend devices list --json`, taken before
and after the save.

Keep a second terminal on `ScreenExtend logs --lines 40` throughout, and note the join id and the
`joins[].display.id` before each save — those two are what must not move.

| Class | How | Expected |
|---|---|---|
| (a) cosmetic | With a device connected, open its sheet, change **only** the name, Save | Nothing in the log but the toast. `joins[]` is unchanged — same join id, same display id, same geometry — and the dashboard row keeps its place rather than disappearing and coming back. The picture does not flicker. |
| (b) input-only | Toggle **Remote control** off, Save, then move the mouse on the client | `device … settings saved; nothing the live stream reads has changed`. Input stops *immediately*, with no rebuild — this used to need a rejoin to take effect. Toggle it back on and input resumes on the next move. |
| (b) pointer scope | Change **Pointer scope** to "This display only", Save, then drag with pointer lock held | `[input] pointer scope for device … is now display (1 live session(s))` — the count is the point; a `0` there means it did not reach the running injector. The cursor stops at the streamed display's edge without the session rebuilding. |
| (c) audio | Change the **Output device**, Save | `/reconfig` carries the new `audioSink` within about a second and the sound moves. **The epoch must not change** — `curl "http://<host>:8080/reconfig?join=<id>"` before and after. A bump here is the defect this row exists for: the sheet pushes this command on every save, so bumping made *every* edit a disconnect. |
| (e) display scale | Move **Display scale**, Save | `display scale for device … saved` and the extended desktop's DPI changes under a stream that never stops. `joins[].display.scale` is the new value, `.id` is unchanged, and the epoch is unchanged. |
| (d) stream | Move **Video quality**, Save | `device … changed a stream parameter; 1 live session(s) will renegotiate in place`, then `join … is renegotiating in place; keeping its session and display` and `virtual display id=N untouched (encoder-only edit)`. The join id and display id are unchanged and no new display is created. |
| (e) display mode | Move **Refresh rate** (or orientation, or pixel ratio), Save | The same two lines, then `virtual display id=N settings changed in place via Windows APIs (WxH@R, scale=S%)`. **One** mode change, not a create and a remove: `ScreenExtend logs \| findstr "virtual display created"` must show no new line, and the windows you had on that display must still be on it. |
| Two tabs stay two | With the device in two tabs, save any class-(d) edit | `2 live session(s) will renegotiate in place`, two join ids, two display ids, and neither tab inherits the other's display. |
| A no-op save | Open the sheet and Save without touching anything | `nothing the live stream reads has changed`, and no command reaches the host at all for the fields the sheet knows are unmoved. |

**What a failure looks like.** A `DeviceRemove` followed by a `DeviceJoin` on the dashboard, two
OS notifications ("device left", "device joined"), a new join id in `devices list --json`, or a
second `virtual display created` line. Any one of those means the edit took the leave-and-rejoin
path instead of renegotiating in place.

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

## 9. Pointer range (row 320)

The control is in the device sheet under Remote Control and only shows while remote control is on.

**Expect today:** choosing "This display only" persists — reopen the sheet, and `config.json`'s
`devices[].pointerScope`, to confirm. It does **not** yet change where the cursor can go: the
`set_device_override` parameter is the other workstream's half, and both call sites carry an
`INTEGRATOR:` comment naming the one-line change. Re-run this test as a behaviour test once that
lands: with `display`, a pointer-locked client moving the mouse hard left/right must not be able
to walk the host cursor onto a monitor it cannot see.

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

# ScreenExtend manual test plan

This plan exists because the pre-engagement build (commit `271aa01`) was slower but reliable, and the
current build must be at least as reliable before speed counts for anything. Every procedure below
is a feature, or a combination of features that genuinely interact. Combinations of unrelated
features (a DPR change while starting a hosted network, a theme change during an audio test) are
deliberately absent: they share no code path and would only add runs.

Work through the sections in order. Each procedure names the devices it runs on, the steps, what must
be true, how to verify it from the host, and a result box. A failed box is a bug report: capture the
evidence the way §1.3 says and move on. Do not retry a failed step until the evidence is saved.

---

## 0. Devices

Three physical devices cover everything. Keep to these letters in every result you record.

| Letter | Device | Role | Notes |
|---|---|---|---|
| **A** | This Windows 11 laptop (build 26200, RTX 3080 Ti, hybrid graphics). | **Host** in every test. Also a *same-device client* when a test says "A (tab)": open `https://172.16.11.76:8443/` in Edge or Chrome on A itself. | The host address is whatever the dashboard shows; 172.16.11.76 is the current one. A same-device join raises the "You're on the device being controlled" card; dismiss it. |
| **B** | The Intel MacBook, macOS 10.15.8. | **Client.** Two browsers, two tiers: **Safari** (no WebCodecs: the `<video>` element tier) and **Chrome** (WebCodecs canvas tier over HTTPS). | Run a client test on B in *both* browsers unless it says otherwise. Hosting from B is out of scope until the macOS build is caught up. |
| **C** | One phone or tablet on the same Wi-Fi (iPhone/iPad with Safari, or Android with Chrome). | **Client.** The only touch device, the only device with a DPR above 2, the only one that rotates, and the only one that can leave the LAN for cellular. | If you own both an iOS and an Android device, use the second one only where a test says "C (other OS)". Otherwise skip those lines. |

Two-device tests use **B + C** (two operating systems, two browser engines). If C is unavailable,
substitute **A (tab) + B**; it exercises the same host code, only with one fewer client engine.

---

## 1. Before you start

### 1.1 Launch and confirm the host

1. Start ScreenExtend on A. It must reach the dashboard with no Setup Error.
2. In a PowerShell on A, run `ScreenExtend status --json` (the exe is on PATH once installed, or use
   the full path to `ScreenExtend.exe`). Confirm `hostRunning: true`, the `serverPorts`, the
   `sessionId`, and `drivers.virtualDisplay: "IddDriver"`.
3. Note the join URL the dashboard shows. HTTPS (`:8443`) is the fast path (WebCodecs, shared-memory
   audio); HTTP (`:8080`) is the compatibility path. Accept the self-signed certificate warning once
   per client browser.

### 1.2 How to read what the host thinks

`ScreenExtend status --json` is the source of truth for every "did the stream change" question.
Under `telemetry.joins[]` each live join lists:

- `joinId` — the session. **A settings edit must never change it.** A new value means the client
  left and rejoined, which is the class of bug this plan hunts.
- `display.device` (`\\.\DISPLAYn`) and `display.id` — the virtual display. Must also stay put across
  every edit.
- `display.width/height/refresh/scale/portrait` — **what the display actually reached**, which may
  differ from what you asked for (see §4). It must never lie and must never oscillate.
- `token`, `ip`, `name`, `screenSize` — identity and labels.

`ScreenExtend logs --lines 300` prints the host log; Settings → Logs shows it live.

### 1.3 When something fails: capture before you retry

1. `ScreenExtend logs --lines 2000 > findings\<test-id>.log` (create `findings\` anywhere you like).
2. `ScreenExtend status --json > findings\<test-id>.status.json`.
3. Screenshot of the host dashboard and a photo/screenshot of the client.
4. Client console when the client page is the thing that broke: on B, Safari → Develop → Show
   JavaScript Console (enable the Develop menu in Safari Preferences → Advanced first); on C (iOS),
   Safari on B → Develop → *iPhone name*; on C (Android), `chrome://inspect` in Chrome on A.
5. If the host window disappeared: Event Viewer → Windows Logs → Application, filter Event ID 1000,
   and the folder `%LOCALAPPDATA%\CrashDumps`. Note the time.
6. One line in the results table: test id, device letters, what you saw, the file names.

### 1.4 Vocabulary used in the "Expect" columns

- **Nothing happens** — no `joinId` change, no display change, no picture interruption, no
  notification, no log line about renegotiation. Only a UI toast ("Device Settings Updated").
- **Rebuilt in place** — the picture blanks for under about two seconds and comes back at the new
  geometry. `joinId` and `display.device` are **the same before and after**, the dashboard row
  never leaves the list, no "Device Connected" / "Device Disconnected" OS notification, and windows
  you had placed on that display are still on it afterwards.
- **Live change** — the picture never blanks; the change is visible within a second.
- **The other stream is untouched** — every field of the *other* join in `status --json` is
  byte-identical before and after, its picture never blanked, its audio never gapped, its cursor
  kept moving during the whole edit, and its row never moved on the dashboard.
- **Host untouched** — A's own display in Settings → System → Display still shows the same scale
  and resolution, the wallpaper is present, the taskbar is normal. Check after every display
  change in §4 and §5.

---

## 2. Startup and lifecycle — host only

| ID | Devices | Steps | Expect | Result |
|---|---|---|---|---|
| S1 | A | Cold start with the app closed. | Dashboard within a few seconds; no Setup Error; `status --json` answers. | [ ] |
| S2 | A | Start it a second time while it is running (double-click the exe again). | The dialog "ScreenExtend Is Already Running" with the option to quit the other copy or show it. Choosing show focuses the first window; the second process exits. | [ ] |
| S3 | A | With a client connected (B), run `ScreenExtend stop` from PowerShell. | The host exits cleanly, the client shows its disconnected state, and Settings → System → Display on A no longer lists the virtual display. Relaunch: dashboard again, B can rejoin and gets a fresh display. | [ ] |
| S4 | A | Profile menu → Exit App with a client connected. | Same as S3. | [ ] |
| S5 | A | Profile menu → Replay Tour, then finish or skip it. Reset Preferences. Toggle theme. Settings → Zoom to another value and back. Account Settings → change the name. Set a profile picture. | Cosmetic only. With B connected throughout, B's stream is untouched (§1.4). | [ ] |
| S6 | A | Settings → Advanced → Launch at Startup on, sign out and back in, then off. | The app opens at sign-in with the toggle on and does not with it off. | [ ] |
| S7 | A | Run `ScreenExtend doctor` and `ScreenExtend doctor --json`. | Exit code 0; the six backend names are filled (capture, encoder, display, audio, hosted network, input); the encoder says nvidia. | [ ] |
| S8 | A | Kill `explorer.exe` from Task Manager and restart it while B is streaming. | The host keeps running; B's stream continues; the dashboard row stays. | [ ] |

---

## 3. Joining

| ID | Devices | Steps | Expect | Result |
|---|---|---|---|---|
| J1 | A host; B (Safari), B (Chrome), C | Open the HTTPS URL, accept the certificate once, enter the session id and OTP from the dashboard, a device name, leave "fullscreen" checked, Connect. | Picture within ~3 s. A "Device Connected" notification on A. One row on the dashboard with the device name. `status --json` lists one join with a display. On A, Settings → System → Display shows one new monitor sized like the client. | [ ] |
| J2 | A host; B, C | Same over HTTP (`:8080`). | The page offers "Switch to HTTPS: faster and more secure". Choose "Stay on HTTP": the join still works (the `<video>` element tier). Choose "Switch to HTTPS" on a second run: it lands on the HTTPS page with the fields kept. | [ ] |
| J3 | A host; C | Scan the dashboard's QR code with the phone camera instead of typing the URL. | Same as J1. | [ ] |
| J4 | A host; A (tab) | Join from a browser on A itself. | The "You're on the device being controlled" card appears; dismissing it joins normally. Moving the mouse on A with the client tab focused must not feed back into itself in a loop. | [ ] |
| J5 | A host; B | Wrong OTP five times in a row, then the right one. | The fifth wrong attempt starts a 60 s lockout; the correct OTP inside the lockout is refused with a message; after 60 s it joins. The host log shows the limiter. | [ ] |
| J6 | A host; B | Join, leave (close the tab), join again the same day. | The second join needs no OTP approval step if the dashboard's "remember" behaviour is on: the device token was minted on the first join and the host log says "device token approved for auto-join". | [ ] |
| J7 | A host; B | Join, then reload the page (F5). | Reloading is a leave (page hide releases the join): the dashboard row goes, the display is removed after ~1.5 s, and the reload joins fresh with a new `joinId` and a new display. This is expected; note only if it hangs, doubles up, or leaves a display behind. | [ ] |
| J8 | A host; B, C at the same time | Join from both. | Two rows, two joins, two virtual displays on A with different `display.device` names. Both pictures live at once. | [ ] |
| J9 | A host; B | Two tabs of the same browser, both joined. | Two rows, two joins, two displays — but **one device token**, so the two rows share settings (see D-series). | [ ] |
| J10 | A host; B | Join with a 64-character device name, then a name with emoji. | Accepted and shown; nothing breaks. Anything longer is cut, not rejected with an error page. | [ ] |

---

## 4. Device settings — one device, every field

Open Devices → the device row → Edit Device. Save after each change. Run the whole table once on
**B (Chrome)**, once on **B (Safari)**, and once on **C**; the three tiers repaint differently.

The expectation column is the design rule (CLAUDE.md, "What a settings edit costs the stream"):

| ID | Field | Change to try | Expect | Result B-Chrome / B-Safari / C |
|---|---|---|---|---|
| D1 | Device Name | Any new name. | Nothing happens on the stream; the row is renamed; the name is what the OS notification uses next time. | [ ] [ ] [ ] |
| D2 | Remote Control | Off, then on. | Live: with it off, mouse and keyboard from the client do nothing on A within a second; on again, they work. No blank. | [ ] [ ] [ ] |
| D3 | Restrict cursor to this display | On, then off. | Live: with it on, the cursor cannot leave the virtual display when driven from the client; off, it can cross to A's own screen. No blank. | [ ] [ ] [ ] |
| D4 | System Audio | On, then off. | Live: audio starts within ~2 s of saving (play something on A); stops when off. Picture never blanks. | [ ] [ ] [ ] |
| D5 | Audio Output | Choose a different speaker on the client (after the "Allow microphone access" card, which reveals the names). | Live: the client switches output; the card says "Playing on: …". Picture untouched. | [ ] [ ] [ ] |
| D6 | Video Quality | 15 → 30 → 5. | Rebuilt in place each time. Visibly softer at 30, sharper at 5. | [ ] [ ] [ ] |
| D7 | Video Scale | 100 → 50 → 100. | Rebuilt in place; the picture is upscaled at 50 and crisp again at 100. `display.width/height` do not change (this is an encode setting, not a display one). | [ ] [ ] [ ] |
| D8 | Automatic frame rate | On, then off. | Rebuilt in place. | [ ] [ ] [ ] |
| D9 | Orientation | Landscape → Portrait → Landscape. | Rebuilt in place; `display.portrait` flips; width/height swap; **host untouched**. | [ ] [ ] [ ] |
| D10 | Refresh rate | 60 → 120 → 30 → 60. | Rebuilt in place. `display.refresh` shows **what the display reached**: the virtual display registers a fixed set of rates when it is created (the log line "registering display … x [60, 120] Hz" says which), so 30 lands on one of those. The value shown must be the landed one, must not flip back and forth, and the stream must not rebuild more than once per save. | [ ] [ ] [ ] |
| D11 | Pixel Ratio (DPR) | 1 → 1.5 → 2 → 1 (the slider stops at the client's own maximum). | Rebuilt in place; `display.width/height` scale by the ratio and `display.scale` reads 100×ratio (150 at 1.5). **Host untouched: A's own display scale must not move and the wallpaper must stay.** Text on the client is the same physical size at every step. | [ ] [ ] [ ] |
| D12 | Scale | 100 → 125 → 150 → 100. | Live change: the desktop on the virtual display re-lays out at the new DPI with no blank. `display.scale` shows the landed value: the display accepts only a band of steps for its resolution (100–175 % at 1920×1080), so 200 at a small resolution lands lower and must be shown as such. **Host untouched.** | [ ] [ ] [ ] |
| D13 | Refresh stream (Devices page button) | Click it. | Rebuilt in place with a keyframe; the toast says "Refreshing Stream". With the device offline the toast says "Nothing to Refresh". | [ ] [ ] [ ] |
| D14 | Remove Device (Edit Device sheet) | Click it and confirm. | The saved settings for the device are dropped and the row returns to defaults. If any stream field differed from default, one rebuild in place; the device does not get disconnected. | [ ] [ ] [ ] |
| D15 | Several fields in one save | Change Refresh rate, Video Quality and Orientation, then save once. | Exactly **one** rebuild in place, not three. | [ ] [ ] [ ] |
| D16 | Two saves in quick succession | Save DPR 1.5, and within a second save DPR 2. | The stream ends at DPR 2 with the display at the 2× geometry; at most two rebuilds; never a stuck black picture, never two displays. | [ ] [ ] [ ] |
| D17 | Two tabs, one device (J9) | With both tabs open, change Refresh rate once. | **Both** tabs rebuild in place (they share the token). Both keep their own `joinId` and display. | [ ] (B only) |

After the D-series on each device, check **host untouched** one more time, and check that
`ScreenExtend status --json` still lists exactly one join for that device.

---

## 5. Device settings — two devices, one edited

This is the section that finds the regressions you reported. **A hosts; B and C are both joined**
(fallback A (tab) + B). Before each row write down both `joinId`s and `display.device`s. Edit only the
device named in the row; the other one is the control.

| ID | Edit on the first device | Expect on the first | Expect on the second (the control) | Result |
|---|---|---|---|---|
| M1 | Pixel Ratio 1 → 2 → 1 | Rebuilt in place, geometry as D11. | **Untouched** (§1.4): same join, same display, cursor kept moving throughout, no blank, no notification. | [ ] |
| M2 | Refresh rate 60 → 120 | Rebuilt in place. | Untouched; its own `display.refresh` did not change. | [ ] |
| M3 | Orientation → Portrait | Rebuilt in place. | Untouched. | [ ] |
| M4 | Scale 100 → 150 | Live change. | Untouched; its `display.scale` did not change. | [ ] |
| M5 | Video Quality 15 → 40 | Rebuilt in place, softer. | Untouched, still sharp. | [ ] |
| M6 | Remote Control off | Live; the first device can no longer drive A. | The second still drives A. | [ ] |
| M7 | Restrict cursor on | Live. | The second's cursor is not restricted. | [ ] |
| M8 | System Audio on for the first only | The first hears A. | The second hears nothing; its picture untouched. | [ ] |
| M9 | System Audio on for both, then off for the first | The first goes silent. | The second **keeps playing without a gap** (one shared capture is reference-counted). | [ ] |
| M10 | Both audio on; the first closes its tab | The first leaves. | The second keeps playing. | [ ] |
| M11 | Refresh stream on the first | Rebuilt in place. | Untouched. | [ ] |
| M12 | Ban the first (Settings → Past Devices → Banned) | Disconnected immediately; cannot rejoin; toast "Device Banned". | Untouched. Unban: the first can join again. | [ ] |
| M13 | Close the first's tab | Row leaves, display removed after ~1.5 s. | Untouched; its display keeps its `\\.\DISPLAYn` name. | [ ] |
| M14 | Swap roles: repeat M1, M2 and M4 editing the *second* device | as above | as above | [ ] |
| M15 | Rotate C physically while B is the control | C's picture rotates (a mode switch, rebuilt in place, same join). | B untouched. | [ ] |
| M16 | Edit both in one minute: DPR on the first, then Refresh rate on the second | Each rebuilds once. | Each is untouched while the other is being edited. Two displays remain, both correct. | [ ] |

The M-series must also pass with the same result when the two clients are the **two tiers on B**
(Safari and Chrome, two tokens because they are two browsers): run M1, M2 and M9 that way.

---

## 6. Display changes and the host's own screen

Every display write must name the virtual monitor, never A's own. These runs watch A.

| ID | Devices | Steps | Expect | Result |
|---|---|---|---|---|
| H1 | A host; C | Before joining, note A's display scale in Settings → System → Display. Join C. Change DPR 1 → 2 → 1.5 → 1. Change Scale 100 → 175 → 100. Change Orientation twice. | After every step A's own scale and resolution are unchanged, the wallpaper is present, no window on A's screen moved. | [ ] |
| H2 | A host; B, C | Join both. Close C's tab (its display is removed). Immediately change DPR on B. | B rebuilds in place on **its** display. Nothing is written to the slot C just freed: no third display appears, A's screen untouched. | [ ] |
| H3 | A host; B | Join, then set an unreachable value: Scale 25 at a small client, or Refresh rate 500. | The display lands where it can; `status --json` shows the landed value; the log records a refused or clamped write once; **no rebuild loop** (watch the log for a minute), no blank picture that never returns. | [ ] |
| H4 | A host; B | With B joined, open Windows Settings → System → Display on A and drag the virtual display to the other side of A's screen; then change A's own scale from the OS and back. | The stream keeps running; the virtual display keeps its own scale; the host's edit does not bounce back. | [ ] |
| H5 | A host; B | Plug in or unplug a real external monitor on A while B streams. | The stream keeps running; the virtual display survives the topology change; A's scale unchanged. | [ ] |

---

## 7. Input from the client

Run on **B (Chrome)**, **B (Safari)** and **C**. Open a text editor and a browser on the virtual
display first.

| ID | Steps | Expect | Result B-Chrome / B-Safari / C |
|---|---|---|---|
| I1 | Move the mouse / finger slowly across the stream, then fast. | The cursor on the virtual display follows without a gap of more than a frame; no stalls, no jumps back. | [ ] [ ] [ ] |
| I2 | Click, double-click, right-click, drag a window, scroll a page, horizontal scroll. | All arrive; drag does not drop mid-way; scroll direction is right. | [ ] [ ] [ ] |
| I3 | Type a paragraph including Shift, Ctrl+A, Ctrl+C/V, arrows, Backspace, Tab, Enter. | Every key arrives once. No repeats, no dropped modifiers. | [ ] [ ] [ ] |
| I4 | (B only) With fullscreen on, click the stage: pointer lock. Move in circles; press Esc. | Pointer speed feels like A's own mouse (one acceleration curve, not two); Esc releases the lock; clicking re-acquires it. | [ ] [ ] — |
| I5 | (B only) In fullscreen, press Alt+Tab, Win key, F11, Ctrl+W. | With keyboard lock (Chrome) they reach the virtual display and do not act on B. In Safari they act on B; note which. | [ ] [ ] — |
| I6 | Clipboard: copy text on B, paste on the virtual display; copy on the virtual display, paste on B. | Both directions work where the browser allows it; a browser that does not is noted, not a bug. | [ ] [ ] [ ] |
| I7 | (C only) Two-finger scroll, pinch, long-press. | Scroll and right-click equivalents arrive; nothing crashes the page. | — — [ ] |
| I8 | Restrict cursor to this display on: try to push the cursor onto A's screen from the client. | It stops at the edge. Off: it crosses. | [ ] [ ] [ ] |
| I9 | Remote Control off: repeat I1–I3. | Nothing arrives; the client still shows the picture. | [ ] [ ] [ ] |
| I10 | Input while a dialog is up: open the audio-output card, then move/click. | Input is paused while the card is open and resumes when it closes. | [ ] [ ] [ ] |
| I11 | Ten minutes of continuous typing and mousing. | No drift, no stuck key, no growing lag. | [ ] [ ] [ ] |

---

## 8. Video: cadence, quality and the static picture

Run on **B (Chrome)** and **C** over HTTPS, and once on **B (Safari)**. Then repeat V1, V2 and V4 with
Settings → Video Encoding → GPU encoding disabled (the x264 path) and again over HTTP.

| ID | Steps | Expect | Result |
|---|---|---|---|
| V1 | **Static desktop.** Leave the virtual display showing a still window with no cursor on it for five minutes. Watch the client. | **Not one visible change.** No green or coloured flashes, no blocks, no blur that comes and goes, no brightness pulse. The picture is identical for the whole five minutes. (This is the green-flicker report; it is the first thing to run after every build.) | [ ] |
| V2 | Play a 60 fps video full-screen on the virtual display for five minutes. | Smooth, no tearing, no periodic stutter, no colour smear that clears on its own, audio (if on) in sync. | [ ] |
| V3 | Scroll a long web page continuously for a minute. | Text stays readable while moving; no ghosting after it stops. | [ ] |
| V4 | Move only the cursor over a static desktop for a minute. | The cursor moves smoothly; nothing else in the picture changes. | [ ] |
| V5 | Refresh rate 30, 60, 120 (D10) with a moving window. | Each rebuild plays at the landed rate; 120 is visibly smoother than 60 on a client that can show it. | [ ] |
| V6 | Video Quality 5 vs 40 on a text-heavy page. | 5 is crisp, 40 is soft; neither shows colour errors. | [ ] |
| V7 | Video Scale 50. | Upscaled but stable; no flicker at the scaled size. | [ ] |
| V8 | Automatic frame rate on, then leave the desktop still, then move. | Frame rate drops when still and rises when moving without a visible stall at the transition. | [ ] |
| V9 | Walk C to the edge of Wi-Fi range for 30 s, then back. | The picture degrades and recovers by itself within a few seconds of the link returning. No permanent blocks, no green, no stuck frame requiring a reload. | [ ] |
| V10 | Host under load: start a large compile or a game on A while B streams. | The stream slows but does not stall or corrupt; input still arrives. | [ ] |
| V11 | Lock A (Win+L) for 30 s and unlock. | The client shows the lock screen or a frozen last frame while locked and resumes by itself on unlock, same join. | [ ] |

---

## 9. Audio

Run on **B (Chrome)**, **B (Safari)** and **C**, HTTPS first, then HTTP once on B.

| ID | Steps | Expect | Result |
|---|---|---|---|
| A1 | System Audio on; play music on A. | Audio on the client within ~2 s; no gaps over five minutes. | [ ] |
| A2 | Play a video with dialogue. | Lip sync is right by eye. | [ ] |
| A3 | Change A's default output device (Windows sound settings) while streaming. | Audio continues after a short pause; no permanent silence. | [ ] |
| A4 | Change the client's Audio Output (D5). | Switches within a second. | [ ] |
| A5 | Toggle System Audio off and on five times. | Each on resumes; no leak of a second stream (no echo/double playback). | [ ] |
| A6 | Audio on, then change DPR (D11). | Audio continues across the rebuild, at most a short gap. | [ ] |
| A7 | Two devices, M9 and M10. | as in §5 | [ ] |
| A8 | Mute the client (the "Tap to start playback" / unmute overlay when the browser blocks autoplay). | Unmute starts playback; the overlay does not come back on its own. | [ ] |

---

## 10. Leaving, losing the link, coming back

Settings → Device Disconnect Timeout is 10 s by default; it is the "grace" below.

| ID | Devices | Steps | Expect | Result |
|---|---|---|---|---|
| L1 | A host; B | Close the tab. | Dashboard row gone within a second; "Device Disconnected" notification; the virtual display removed after ~1.5 s; windows that were on it move to A's screen. | [ ] |
| L2 | A host; B | Turn B's Wi-Fi off for 5 s, back on. | Inside the grace: the row **stays**, the display **stays**, the same `joinId` reconnects, the picture returns. No notification. | [ ] |
| L3 | A host; B | Wi-Fi off for 30 s (longer than the grace). | The row goes and the display is removed at the grace deadline; when B returns it joins fresh (new join, new display). | [ ] |
| L4 | A host; B | Set Device Disconnect Timeout to 60 s and repeat L3 with 30 s off. | Now it survives (same join). Set it back. | [ ] |
| L5 | A host; C | Phone screen off for 5 s, on again; then for 2 minutes. | 5 s: same join. 2 min: depends on the phone's background policy; it must rejoin cleanly either way, never leave a dead display behind. | [ ] |
| L6 | A host; C | Switch C from Wi-Fi to cellular and back while joined (LAN URL). | The LAN link cannot cross; the session is kept for the grace and then dropped; back on Wi-Fi within the grace it resumes. | [ ] |
| L7 | A host; B | Navigate the tab to another site, then Back. | Navigating away is a leave (L1); Back is a fresh join. | [ ] |
| L8 | A host; B | Sleep A (close the lid) for a minute with B streaming, then wake. | The host is alive after wake; B reconnects (same join if within grace, else fresh); no orphan display; A's screen untouched. | [ ] |
| L9 | A host; B | Ban B from Past Devices while streaming; then unban. | Disconnected at once; cannot join while banned (an error page, no OTP prompt loop); joins after unban. | [ ] |

---

## 11. Network: hosted network, roaming, ports

| ID | Devices | Steps | Expect | Result |
|---|---|---|---|---|
| N1 | A host | Settings → Create Hosted Network with a name and password. | State goes Not started → Advertised → Verified once something is on it; the dashboard gains a QR for the Wi-Fi itself and a join QR for that network. A banner if you leave the password empty. | [ ] |
| N2 | A host; C | Join C to the hosted Wi-Fi (scan the Wi-Fi QR), then join the session over it. | "{ip} has joined over this network." Picture as J1. | [ ] |
| N3 | A host; C on hosted; B on the router | Both joined over different networks. | Two rows, two displays; both streams live. | [ ] |
| N4 | A host; C | Stop the hosted network while C is on it. | C drops (expected); the dashboard removes it after the grace; A's router network and B's stream are unaffected. | [ ] |
| N5 | A host; B | Roam A between two access points of the same network (or toggle A's Wi-Fi off/on for 3 s). | A "network change" is logged; B reconnects within the grace with the same join; the dashboard's addresses update. | [ ] |
| N6 | A host; B | Settings → Server Ports → 9080/9443, save. | Toast "Server Ports Updated"; the dashboard URLs and QR show the new ports; B joins on 9443; the old 8443 refuses. Set back to 8080/8443. | [ ] |
| N7 | A host | Change the hosted network's name/password while it is up ("Change network settings?"). | The network restarts with the new credentials; the Wi-Fi QR updates. | [ ] |

---

## 12. Public (relayed) sessions

| ID | Devices | Steps | Expect | Result |
|---|---|---|---|---|
| P1 | A host | Settings → Public Internet Sessions on. | Toast "Public Sessions Enabled"; the dashboard's cloud QR/URL (`https://session.screenextend.app/?id=…`) appears; `status --json` says `cloudState: "registered"`. | [ ] |
| P2 | A host; C on cellular | Wi-Fi off on C; open the cloud URL; OTP. | Joins over the relay; picture within ~5 s; input works. Latency is higher than LAN; note it. | [ ] |
| P3 | A host; C relayed | Change Video Quality, then DPR, on the relayed device. | **Known limitation:** the relay serves an older client page, so a stream-class edit makes a relayed client *rejoin* (new join, brief blank) instead of rebuilding in place. It must still come back on its own. Record the time it takes. | [ ] |
| P4 | A host; C relayed + B on LAN | Edit DPR on B. | C untouched; B rebuilt in place. | [ ] |
| P5 | A host | Public sessions off while C is relayed. | C drops; `cloudState` leaves registered; the LAN stream (B) untouched. | [ ] |
| P6 | A host; C | Run `ScreenExtend session new` in PowerShell. | Prints a new id and OTP; the dashboard shows them within a few seconds; the cloud URL changes; **existing** streams keep running; a *new* join needs the new code. | [ ] |
| P7 | A host; C on cellular | Settings → TURN Server: set a TURN URL with credentials (if you have one), save, repeat P2. | Joins; `status --json` says `turnConfigured: true`. Clear it: joins still work over STUN. | [ ] |

---

## 13. Settings, the configuration file, and the CLI

| ID | Devices | Steps | Expect | Result |
|---|---|---|---|---|
| C1 | A | Settings → Configuration File: change `disconnectGraceSecs` to 20, Save Config. | "Configuration saved"; Device Disconnect Timeout shows 20; L2 now survives 15 s off. | [ ] |
| C2 | A | In the editor, delete a comma and save. | "Invalid JSON" with a line and column; nothing applied; the app keeps running. | [ ] |
| C3 | A | In the editor, replace the `devices` array with an object (`"devices": {}`), save. | "Configuration rejected" naming the problem; nothing applied. Restart the app: it still starts. | [ ] |
| C4 | A; B | Start editing, then have B join (the app writes the file), then save. | The conflict dialog "Configuration changed since you started editing" with both versions; either choice leaves a valid file. | [ ] |
| C5 | A | PowerShell: `ScreenExtend config set devices.0.refreshRate 30` | **Refused**, exit code 2, no change to the file (a numeric segment under an array key is not a new object). `ScreenExtend config get devices` still prints an array. | [ ] |
| C6 | A | `ScreenExtend config get serverPorts.http`, `ScreenExtend config set disconnectGraceSecs 15`, then `config get disconnectGraceSecs`. | Reads back 15; the running host applies it (`RELOAD`), visible in Settings without a restart. | [ ] |
| C7 | A; B | `ScreenExtend devices list`, then `ScreenExtend devices set <B's ip> --refresh-rate 120`. | Lists B with its token; the set is a class-(d) edit: B rebuilt in place, the dashboard row stays. `devices reset <ip>` returns it to defaults. | [ ] |
| C8 | A | `ScreenExtend config list` | Prints the config with `sessionPassword` redacted. | [ ] |
| C9 | A | `ScreenExtend status`, `status --json`, `qr`, `qr --json`, `logs --lines 50` | All answer from the running host without starting a second one (no new window flashes). | [ ] |
| C10 | A | Settings → Video Encoding → disable GPU encoding while B streams. | Toast; B rebuilt in place on x264; the log names x264. Re-enable: back on NVENC. | [ ] |
| C11 | A | Profile menu → Uninstall Drivers, confirm; then relaunch. | Drivers removed with an elevation prompt; the next launch offers to install them and, after it does, a join works. (Only if you are willing to reinstall.) | [ ] |
| C12 | A | `ScreenExtend serve -v` from PowerShell with the GUI closed; join from B; Ctrl+C. | Headless host prints the URLs; B joins; Ctrl+C removes the virtual display before exiting. | [ ] |

---

## 14. Soaks and long runs

These are the "did not keep crashing" tests. Run them last, and run them after every build.

| ID | Devices | Steps | Expect | Result |
|---|---|---|---|---|
| K1 | A host; B, C | Both joined, audio on for both, still desktop, **30 minutes**. | No colour changes on either picture, no reconnect, no notification, both rows still there, `status --json` joins unchanged, no growth in A's memory for the ScreenExtend process beyond a few MB. | [ ] |
| K2 | A host; B, C | Both joined, 60 fps video on one display, typing on the other, **30 minutes**. | Same as K1 plus no growing input lag. | [ ] |
| K3 | A host; B | Every 2 minutes for 20 minutes, change one stream field (D6–D11 in rotation). | Ten rebuilds in place; `joinId` and display unchanged throughout; no leftover displays in Settings → System → Display; host untouched. | [ ] |
| K4 | A host; B, C | Join/leave each device ten times in a row (close and reopen the tab). | Ten clean joins; the display count on A returns to zero between runs; no "Device Connected" notification without a join. | [ ] |
| K5 | A host | Leave the app running overnight with no client. | Alive in the morning; a join works without a restart. | [ ] |
| K6 | A host; B | Repeat K1 with GPU encoding disabled (x264) for 15 minutes. | Same. | [ ] |

---

## 15. Results

Copy this table into `findings\RESULTS.md` and fill one line per failed or noteworthy row.

| Test | Devices | Build (commit) | What happened | Files |
|---|---|---|---|---|
| | | | | |

A run is complete when every box in §2–§14 is marked. The rows most likely to catch the current
reports are V1 (colour flicker on a still picture), M1–M4 (a second stream disturbed by an edit),
H1 (the host's own display changed), and K1–K3 (anything that stops working after minutes rather
than seconds).

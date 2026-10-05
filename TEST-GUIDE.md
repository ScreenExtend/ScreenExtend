# TEST-GUIDE.md — end-to-end audio + video test in headless Chrome

One command starts the host from the CLI, opens a brand-new headless Chrome, joins the session,
measures per-frame end-to-end time and the audio path, shuts everything down and prints a
pass/fail result.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\e2e-headless\e2e.ps1
```

It takes about 50 seconds. Exit code 0 and a last line of `E2E PASS` mean every check below held;
exit 2 means a check failed (the `failures` list says which); exit 1 means the run never got as
far as measuring.

| Switch | Effect |
|---|---|
| `-Headed` | a visible Chrome window instead of headless (same measurements) |
| `-NoAudio` | video only: no tone is played and `config.json` is never written |
| `-Exe <path>` | test a different host binary (default: the shared release build) |

## Before you run it

- **Build first.** The test runs the release binary, and the client page is compiled into it, so
  an edit under `src-tauri/src/streamer/static/` or to the host is not tested until
  `pnpm tauri build` has run.
- **Quit ScreenExtend.** The test starts its own host and refuses to run if one is already up
  (`ScreenExtend stop`). Ports 8080 and 8443 must be free.
- **Close any ScreenExtend client tab in your own browser.** An approved device rejoins by itself
  the moment a host appears, which adds a second display and a second stream to the measurement.
- **Expect a faint tone.** For about 26 seconds a quiet 440 Hz tone plays on the default output.
  System-audio capture has nothing to carry otherwise.
- Needs Node 22 or newer and Google Chrome in its default location (`$env:CHROME` overrides it).

Nothing in the test injects input or moves the cursor. Page actions are DOM calls made over
Chrome's debugging protocol, and on-screen motion is a 48 px window sliding across the virtual
display.

## What it does, in order

1. **Host.** Mints a session id and a 6-digit code, passes them by environment variable (never on
   the command line), and runs `ScreenExtend serve --no-cloud --no-qr -v`. It waits for the HTTPS
   address in the log.
2. **Browser.** Deletes the test profile and starts Chrome with `--headless=new`, a throwaway
   profile, `--ignore-certificate-errors` (the host's certificate is self-signed),
   `--use-fake-device-for-media-stream` and `--mute-audio`. Muting matters: host and client are
   one machine, so unmuted playback would be captured again as a feedback loop. The audio graph
   still runs when muted.
3. **Permissions.** Grants microphone, wake lock and speaker selection through
   `Browser.grantPermissions`. That call denies every permission it does not name, so all three
   are named.
4. **Join.** Opens `https://<lan-ip>:8443/?id=…&name=HeadlessProbe&otp=…&fs=0`, dismisses the
   same-device card, submits the form, and waits for the first painted frame. The host creates a
   virtual display for the join.
5. **Audio on.** Runs `ScreenExtend devices set <token> --audio on` for the probe device (audio is
   off by default for a new device). Samples three seconds of silence, then starts the tone.
6. **Measure, 20 s.** The small window slides across the virtual display so the encoder has real
   frames. Client video timings are read from `SEVideo.getStats()`, client audio state from
   `SEAudio.getSyncInfo()`, and the signal that actually reaches the speakers from an analyser
   tapped onto the audio output.
7. **Shut down.** Accepts the page's "leave site?" dialog, closes Chrome, runs
   `ScreenExtend stop`, restores `config.json` byte-for-byte if step 5 changed it, and reports
   what is left running.

## Reading the result

The script prints one `RESULT` block. Reference values are from the development machine
(Windows 11, RTX 3080 Ti laptop, Chrome 154, host and client on the same box, 800×600 display),
four consecutive runs.

### Video

| Field | Meaning | Reference | Fails the run when |
|---|---|---|---|
| `path` | 1 = WebCodecs + canvas (fast path), 3 = `<video>` fallback | 1 | not 1 |
| `hostToPaintMs` | host capture to client paint, per frame — the end-to-end frame time | p50 12.2–12.8, p99 19.8–20.5 | — |
| `decodeMs` | decoder input to output | p50 0.7–0.8, p99 1.3–2.1 | missing |
| `arrivalToPaintMs` | frame arrival at the client to paint | p50 0.7–0.8, p99 1.3–2.1 | — |
| `paintedFps` | frames painted per second during the 20 s | 58–61 | below 30 |
| `framesDropped`, `decodeErrors` | | 0, 0 | any decode error |
| `hardwareDecoder` | whether Chrome picked a hardware decoder | true | — |
| `firstPaintMs` | form submit to first painted frame | 3100–3250 | — |

`hostToPaintMs` has no hard threshold because it depends on the machine; compare it against the
reference, or against your own previous run on the same machine.

### Audio

| Field | Meaning | Reference | Fails the run when |
|---|---|---|---|
| `path` | 1 = Opus over DataChannel → worker → shared-memory ring (fast path), 3 = plain Opus track | 1 | not 1 |
| `tonePeakHz` | strongest frequency at the client's output during the tone | 439 (the analyser's bins are 5.9 Hz wide) | more than 12 Hz from 440 |
| `toneRms` | level at the client during the tone | 0.152 | 0.01 or lower |
| `silenceRmsBefore`, `silenceRmsAfter` | level before and after the tone | 0, 0 | above 0.005 after |
| `packetsPerSec` | audio packets arriving (5 ms Opus frames) | 200 | — |
| `underruns`, `concealed` | jitter-buffer starvation, concealed frames | 0, 0 | either is non-zero |
| `bufferDepthMs`, `bufferTargetMs` | jitter-buffer depth range and its commanded target | 3–11, 10 | — |
| `captureToEnqueueMs` | host audio capture to the client's ring | 6.4–6.7 | — |
| `outputLatencyMs` | the browser's own output latency | 52–57 | — |
| `avResidualMs` | how far audio trails the picture after the buffer has done what it can | −58 to −67 | — |

`avResidualMs` is negative here because the picture takes about 8 ms to appear while headless
Chrome's audio output alone takes more than 50 ms. The page cannot play audio earlier than the
output device allows, so that gap is the browser's, not the stream's.

## When it fails

- **`video not on the WebCodecs path`** — look in `run.out` for `falling back to <video>`. The
  reason follows it. `no WebCodecs frame within 5s` means the decoder got no frames: check that
  nothing waits between `addTransceiver('video')` and the transform assignment in `index.html`
  (`docs/OPTIMIZATIONS.md` UX-WEBCODECS-START). `The host did not answer in time` means the join
  request took over 10 s; seen once in development and not yet explained — rerun, and if it
  repeats read `serve.log` around `join request`.
- **`audio not on the worklet+shared-memory path`** — look for `[audio] decoder worker error`.
  The worker scripts must be served with `Cross-Origin-Embedder-Policy: require-corp`
  (`docs/OPTIMIZATIONS.md` UX-COEP-WORKER); check with
  `curl -sk -D - -o NUL https://<ip>:8443/audio-worker.js`.
- **`440 Hz tone not heard at the client`** — confirm `AUDIO-ON` in `run.out` says
  `Saved override`, and that the host has a working default output device (system-audio capture
  follows it).
- **`2 devices are connected from <ip>`** — another client joined from this machine, usually a
  ScreenExtend tab left open in your own browser.
- **Painted fps far below 60 with no errors** — the moving window landed on a different display.
  `LATENCY.mover` in `run.out` names the one it used; only the test's own virtual display should
  be attached besides the built-in screen.

Logs are in `%TEMP%\screenextend-e2e\`: `serve.log` (host, verbose), `run.out` (every step, the
raw stats, each audio sample, page console output) and `config.json.before`.

## Running the pieces by hand

```powershell
$env:SCREENEXTEND_SESSION_ID = 'HEADLESS2345'   # 12 chars from 2-9 and A-Z without I and O
$env:SCREENEXTEND_OTP = '482915'
ScreenExtend serve --no-cloud --no-qr -v        # prints https://<lan-ip>:8443

# in a second shell
$env:AUDIO = '1'; $env:CHROME_FLAGS = '--mute-audio'
node scripts\e2e-headless\run.mjs <lan-ip> HEADLESS2345 482915

ScreenExtend stop
```

Run this way, nothing restores `config.json`: with `AUDIO=1` the probe device's override row
stays in it. `run.mjs` also takes `HEADED=1` (visible window) and `DEBUG=1` (worker and RTP
counters while waiting for the first frame); the remaining switches at the top of the file are
diagnostics from the WebCodecs investigation.

## Files

| File | Role |
|---|---|
| `scripts/e2e-headless/e2e.ps1` | the one command: host, browser run, cleanup, config restore |
| `scripts/e2e-headless/run.mjs` | drives Chrome over its debugging protocol and measures |
| `scripts/e2e-headless/mover.ps1` | the 48 px moving window (never takes focus) |
| `scripts/e2e-headless/tone.ps1` | the quiet 440 Hz tone |

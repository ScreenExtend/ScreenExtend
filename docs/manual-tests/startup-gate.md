# Manual test: the startup gate and the pointer-lock clamp

Everything here is either destructive (it removes a signed driver), needs Administrator, or needs
a second monitor. `pnpm test:gate` and the Rust `capability_notes` tests cover the decision table;
these are the parts that only a real machine can answer.

## 1. The driver remediation path (Windows, Administrator)

The rule: a host that can create one virtual display and send video starts, whatever else is
missing. A missing driver is not a verdict — the app installs it and re-probes.

1. `ScreenExtend doctor --json` — note `backends.display` is `IddDriver` and `unsupportedApis` is
   empty. This is the healthy baseline.
2. From an elevated shell, `ScreenExtend drivers remove`.
3. `ScreenExtend doctor --json` must now show `backends.display: "DriverNotInstalled"` and one
   `unsupportedApis` entry with `key: "display-driver"`, `severity: "installable"`, and
   `bareMinimumMissing: false` — an installable row is not a verdict.
4. Launch the desktop app. Expected: the loader reads **Installing the display driver**, one UAC
   prompt appears, and after it the app goes straight to the dashboard. **No compatibility modal
   at any point.**
5. Repeat step 4 having *declined* the UAC prompt. Expected: the destructive "Couldn't Install
   Drivers" toast, then the compatibility modal titled **ScreenExtend Can't Start on This
   Computer**, listing the driver row, with **no Continue anyway** — the app tried and it did not
   work, which is the one case the escape hatch is withheld.
6. `ScreenExtend drivers install` from an elevated shell to restore the box.

The `display-driver-unresponsive` branch (remove-then-install, then one bounded self-relaunch)
needs a driver that is installed but does not answer `\\.\pipe\virtualdisplaydriver`. Killing the
driver's service while the app is closed is the only way found to induce it; it has not been
executed. The relaunch is memoised in `localStorage` under `se.driverRelaunchAt` with a
ten-minute window, so if you do induce it, confirm from the log that at most one relaunch happens.

## 2. "Continue anyway" on a degraded host

Any optional fault will do; the cheapest is to disable every audio output device in Sound
settings, which makes `probe_audio_backend()` report `none`.

Expected: the modal titled **Limited System Support**, the audio row listed, and **both** a
Continue button and the "Don't show this message again" checkbox. Continue reaches the dashboard;
a device can join and stream with no audio. Ticking the box and continuing means the next launch
is silent about it.

## 3. At capacity (macOS AirPlay tier only)

`at-capacity.html` is reachable only where `max_concurrent_displays()` is `Some(1)`, which today
is the macOS AirPlay display tier alone. On Windows, with any number of devices connected, the
client page must never be replaced by it and `ScreenExtend status --json` must never report
`full: true`. Connect three devices and confirm both.

## 4. Pointer scope (needs a second real monitor)

Two devices' worth of setup: one joined client with remote control on, and the host's own
physical second monitor to the right of the first.

1. Default (`pointerScope: "display"`, or the field absent). On the client, click the video to
   take pointer lock, then push the mouse hard to the right for several seconds. The host cursor
   must stop at the right edge of the *virtual display this client is streaming* and stay there.
2. `ScreenExtend devices set <ip> --pointer-scope all`, then have the client reconnect — the scope
   is read when the session is built, exactly like remote control, so it does not apply live.
   Repeat the push. The cursor must now cross onto the host's real monitor, and the client can
   click on a screen it cannot see. That is the previous behaviour of every build before this.
3. `ScreenExtend devices list` shows the value in the POINTER column; `devices reset <ip>` clears
   it back to `display`.

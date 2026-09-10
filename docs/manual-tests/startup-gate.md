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
executed. The relaunch is memoised twice — in `localStorage` under `se.driverRelaunchAt` with a
ten-minute window, and in `config.json`'s `driverRepair` for the whole boot — so if you do induce
it, confirm from the log that at most one relaunch happens.

## 1a. The driver is busy, not broken (Windows)

The driver serves many clients at once and answers in under a millisecond, so the busy state is
not reachable by simply running two copies. What it catches is a probe that lands while the driver
is not answering *and* another ScreenExtend process is alive — after which nothing may be
installed and nothing may ask for elevation.

1. Start a host: `ScreenExtend serve --no-cloud --no-qr`, and note the pid it prints.
2. From a second shell, `ScreenExtend doctor --json`. On a healthy machine this reports
   `drivers.virtualDisplay: "IddDriver"`, `virtualDisplayHolderPid: null` and **exit 0** — the
   driver really is available to both processes, and that is the correct answer.
3. Confirm the machinery that would name the holder:
   `cargo test --lib windows_utils::virtual_display::test::report_driver_holder -- --ignored
   --nocapture`. With the host from step 1 up it must print `control socket answers: Some(<that
   pid>)`, `driver_pipe_holder(): Some(<that pid>)` and the line `doctor` would show:
   `installed, in use by ScreenExtend (pid N)`. Stop the host and it must print `None` for all
   three — a stale answer here would tell a user to quit a process that is not there.
4. If you *can* induce the state (a driver mid-attach with a host up), the desktop app must show
   **ScreenExtend Is Already Running**, naming the pid, with **Quit the Other Copy** and **Try
   Again** — and **no UAC prompt at any point**. Quit the Other Copy sends `QUIT` over the control
   server and retries; it only works on a host that answers the control socket, so against a
   `probe --live` it must toast "Couldn't Quit the Other Copy" rather than doing nothing.

## 1b. The repair budget (Windows, Administrator)

The rule: one driver install, one reinstall and one self-relaunch **per boot**, and after that the
app says restart rather than asking for elevation again.

1. `ScreenExtend config get driverRepair` — after a fresh boot with no repairs it reads
   `{"boot":"","install":0,"reinstall":0,"relaunch":0}` or is absent.
2. Run step 5 of section 1 (decline the UAC prompt so the install fails). Read `driverRepair`
   again: `install` must be 1 and `boot` must be non-empty.
3. Press **Try Again** on the modal. It must re-probe and re-show the modal **without a second
   UAC prompt**, and `driverRepair.install` must still be 1.
4. Quit and relaunch the app. Still no UAC prompt, still 1 — the budget survives a launch, which
   is the whole point.
5. Restart Windows, launch the app: the budget is clear (`boot` no longer matches), so exactly one
   fresh UAC prompt appears. Clearing `driverRepair` by hand in the Settings config editor has the
   same effect without a restart.

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

1. Default (`pointerScope: "all"`, or **the field absent**, which is every device saved before
   the setting existed). On the client, click the video to take pointer lock, then push the
   mouse hard to the right for several seconds. The cursor must cross onto the host's real
   monitor: reaching every desktop is the intended behaviour, not a leak.
2. `ScreenExtend devices set <ip> --pointer-scope display` (or tick "Restrict cursor to this
   display" in the device sheet), then have the client reconnect — the scope is read when the
   session is built, exactly like remote control, so it does not apply live. Repeat the push.
   The host cursor must now stop at the right edge of the *virtual display this client is
   streaming* and stay there.
3. `ScreenExtend devices list` shows the value in the POINTER column; `devices reset <ip>` clears
   it back to `all`.

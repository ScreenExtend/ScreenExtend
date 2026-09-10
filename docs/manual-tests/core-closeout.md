# Manual tests — core close-out (W2 / W8 / W9 / W10)

Changes landed whose acceptance test needs hardware, an OS locale, an unheld single-instance lock,
or a link that neither dev box has. Each is written down here with the exact command and the exact expected reading, so
whoever has the machine can settle it in minutes rather than re-deriving the setup.

Everything here is `COMPILE-CHECKED` or unit-tested; none of it is `VERIFIED`. Ledger rows:
`docs/OPTIMIZATIONS.md` 188-203.

---

## 1. Two pens on one host (Windows, needs two pen-capable clients)

Synthetic pen events used to inject `pointerId: 1` for every session; they now carry the client's
own id. Whether Windows disambiguates two synthetic pen contacts sharing an id across two device
handles is undocumented, so the fix is a correctness argument, not an observation.

1. Join two clients with pen input (an iPad with a Pencil, a Surface, an Android with an S Pen).
2. Give both remote control.
3. Draw a continuous stroke on each **at the same time**, in a host application that shows ink
   (Paint, Fresh Paint, Photoshop).

**Expect:** two independent strokes. **Before the change** the two contacts shared `pointerId: 1`;
if the host application still shows one stroke jumping between the two clients' positions, the id
is not what disambiguates them and the per-session pen device is — record that, because it decides
whether the injector can ever be hoisted to one process-wide instance
(`speculative-input-per-session-injector-thread-and-devices.md`).

## 2. macOS Accessibility revoked, then granted mid-session

`CompatibilityReport.input_backend` now reports `NeedsAccessibility` when TCC has not granted it,
and the tier is re-probed on the injector's 500 ms geometry cadence.

1. System Settings > Privacy & Security > Accessibility, remove or untick ScreenExtend.
2. Start the host. `ScreenExtend doctor` must print `Input: NeedsAccessibility`, and
   `check_system_requirements` must carry an `Accessibility (Remote Control)` entry in
   `unsupported_apis` with severity `optional`.
3. Join a client, enable remote control, move the pointer. The Settings log terminal must carry
   `input: Accessibility (TCC) is not granted, so every injected event is dropped by the OS`.
   (Before this change the warning went to `log::warn!` and reached nothing.)
4. **Without relaunching**, tick ScreenExtend in Accessibility.

**Expect:** within about half a second the log says `input: Accessibility granted — remote control
is live` and the pointer starts moving. A relaunch must **not** be needed; that is the whole point
of moving the probe out of the constructor.

5. Untick it again mid-session and confirm `input: Accessibility was revoked`.

## 3. Localized Windows hosted-network probe

The legacy tier's capability is now `WlanQueryInterface(wlan_intf_opcode_hosted_network_capable)`
rather than a match on English `netsh` output.

1. On a machine whose WLAN driver *does* support the hosted network (`netsh wlan show drivers`
   says `Hosted network supported : Yes` in English), record
   `cargo test --lib report_wlan_hosted_network_capability -- --ignored --nocapture`.
2. Change the Windows display language to German (or French, Japanese, Chinese) and reboot.
3. Run the same test.

**Expect:** identical answers. On the Windows 11 26200 dev box both readings are `Some(false)`,
which agrees with `netsh` in English — but that box's driver does not support the hosted network,
so it exercises the negative case only. The positive case is unmeasured.

4. Also confirm `ScreenExtend network start <ssid> <pass>` and `network status` agree on a
   localized install; the started/stopped readback is now `WLAN_HOSTED_NETWORK_STATE`, not a
   `": Started"` match.

## 4. Relayed audio starts defensive

A `CrossNetwork` session now carries `FLAG_REDUNDANT` from its first packet instead of waiting for
the video BWE to observe loss.

1. Configure a TURN server (`ScreenExtend turn set …`) and join from a device on a different
   network so `locality_for` classifies it `CrossNetwork` — the join log prints
   `join stages (CrossNetwork)`.
2. Enable system audio for that device.
3. Compare the opening five seconds against a build with the locality start removed: the client's
   `audio_underruns` in `OP::STATS`, and `dropped_backpressure` in the host's audio diagnostics.

**Expect:** redundant packets from the first one — payload roughly doubles, visible as bytes sent
on the audio DataChannel — and fewer opening underruns. Confirm a `SameNetwork` join on the same
build does **not** carry them until loss appears.

## 5. macOS WindowServer drop count

`CGDisplayStreamUpdateGetDropCount` is read on every delivery and reported once a second.

1. On the 10.15.8 Intel box, put something continuously animating on the captured display (a
   video, or `probe --live --workload motion` once that path exists on macOS).
2. `cargo test --lib cgds::tests::probe_main_display -- --ignored --nocapture`, 30 s.

**Expect:** `dropped_by_windowserver=` next to `delivered=`. Near zero is the capture keeping up.
Re-run after each of: adding `IOSurfaceIncrementUseCount`, raising `queueDepth`, removing the
`minimumFrameTime` hold-off. The expected signature is documented with the use-count finding —
near zero, then a spike, then near zero — and this counter is the only way to see it.

## 6. Wedged driver, and the shortened display lock

Driver round trips no longer hold the state mutex, and `send_command` is bounded at 2 s.

1. Start the host with the IDD installed and a client joined.
2. Suspend the driver's user-mode host process (Process Explorer > Suspend) so the pipe stops
   being drained.
3. Join a second client.

**Expect:** the second join fails with a logged `SendError::PipeBroken(TimedOut)` inside a few
seconds rather than hanging, and `[virtual-display] driver commit took …` appears. Crucially,
`ScreenExtend status --json` and the device list must still answer while the driver is wedged —
that is what taking the round trip out of the state mutex buys.

## 7. Four simultaneous clients (the injector-hoist decision)

`speculative-input-per-session-injector-thread-and-devices.md` was closed without hoisting the
injector to one process-wide thread, and this is the measurement that would reopen it. See
`docs/OPTIMIZATIONS.md`, "One injector per session, and why it stays that way".

1. Join four clients, all with remote control enabled.
2. Record a single-client baseline first: host CPU, the `capture_delivery` / `encode` /
   `capture_to_socket` percentiles from `ScreenExtend status --json`, and the process's thread and
   handle counts (Process Explorer, or `Get-Process ScreenExtend | Select Threads,HandleCount`).
3. Drag continuously on all four at once for 60 s and take the same readings.
4. Disconnect all four and take the thread and handle counts again.

**Expect:** thread and handle counts return to the single-client baseline after step 4 —
`SessionHandle::close` joins the injector thread and `Drop for Injector` destroys the synthetic
pen and touch devices, so a residue is a leak and a finding. The number that decides the hoist is
step 3's `encode` percentile against step 2's: four `THREAD_PRIORITY_TIME_CRITICAL` threads sit
above the encode thread in the scheduler, and if that shows up as encode-stage tail growth, the
hoist is worth its arbitration policy. If it does not, per-session injectors are correct as they
stand.

Also worth watching in step 3: whether two clients dragging at once produce a usable cursor at
all. They share one physical cursor and there is no arbitration today.

## 8. The COM apartment map on this machine (UX-E, Windows)

`windows_utils::com` reports one `[com]` line per (call site, apartment) pair, so the map is
readable from any host without a debugger. Two minutes, no client needed.

1. Open the app, let the dashboard render, then `ScreenExtend logs --lines 500`.
2. Grep for `[com]` and for `RPC_E_CHANGED_MODE` / `0x80010106`.
3. Stop the app and run `ScreenExtend serve -v` for ten seconds; grep the same two things.
4. `ScreenExtend probe --audio 3` and `ScreenExtend probe --capture`, same greps.

**Expect:** `[com] process MTA pinned for the lifetime of the process` exactly once per process;
one `process COM security` line (either "initialized (impersonate)" or "was already set by the
host process" — both are answers); `network-watcher`, `compatibility-probe` and `audio` each
reporting *no apartment of its own → initialized MTA*; and, in the **GUI** only, `network`
reporting *already in the STA apartment; leaving it alone*, because that call arrives on Tauri's
main thread. **No occurrence of `RPC_E_CHANGED_MODE` or `0x80010106` anywhere**, and
`doctor --json`'s six backends identical to what the machine reported before.

A host that reports `network: … MTA` in the GUI is not a failure — it means that build of Tauri
did not give the main thread an STA — but it is worth reporting, because the whole map is written
from what wry does today. A host that reports `RPC_E_CHANGED_MODE` at all is a regression: the
helper asks before it initializes, so the only way to produce one is a call site that stopped
going through it.

## 9. The CLI's session, against a live `serve` host (UX-SESSION, either OS)

`qr`, `status` and `session new` used to mint a session id per invocation, so every URL and QR
they printed was for a session nobody was serving. They now read the running host's id over the
control server's `SESSION` verb. The unit half is covered — `a_session_query_reads_back_what_a_
session_set_wrote` drives the whole exchange over a real loopback socket — but the half that
needs a real host is `#[ignore]`d, because it takes the single-instance lock, initializes the
display driver and binds the streaming ports.

**With no other ScreenExtend running**, from `src-tauri/`:

```sh
cargo test qr_and_status_report_the_session_a_live_serve_host_is_serving -- --ignored --nocapture
```

It spawns `ScreenExtend serve --no-cloud --no-qr` with a known `SCREENEXTEND_SESSION_ID` /
`SCREENEXTEND_OTP`, waits for the host, and asserts that `status --json` and `qr --json` both
report *that* id, that `sessionIdSource` is `host`, that every join URL carries it, and that
`session new --json` replaces the pair on the running host so the next `qr` reads the replacement
back. It stops the host on the way out, including when an assertion fails. It needs the built
binary beside the test executable (`cargo build` first, or `SE_TEST_BIN=<path>`), and it **skips
with a reason** when another instance holds the lock — which is what it does on a machine where
the owner's host is already up.

The relay is the one part that harness does not cover (`--no-cloud`, so the numbers are
deterministic and no session is published). To settle it by hand, with public sessions enabled:

```sh
SCREENEXTEND_SESSION_ID=TESTSESSION1 SCREENEXTEND_OTP=424242 ScreenExtend serve --no-qr
# in another shell:
ScreenExtend qr -t cloud --no-render --json     # sessionId TESTSESSION1, cloudState "registered"
ScreenExtend session new --json                 # applied true, a new id, cloudState "registered"
```

**Expect:** the second command's `sessionId` is `TESTSESSION1` and its `cloudState` is
`registered`, which is `serve` having registered the relay under the id it was given rather than
one of its own; and `session new` re-registers under the id it just minted, so the printed
"Anywhere (Internet)" URL is joinable the moment it appears. A `cloudState` of `connecting` after
`session new` means the 8 s bound expired — the command says so rather than presenting the URL as
ready, and that is a network reading, not a failure.

Beside a host from a build that predates the verb the CLI degrades and says so, which is worth
reading once: `qr` prints `hostRunning: true` with `hostReportedSession: false` and a note that
the id shown is not the one being served, and `session new` fails with exit 1 and changes
nothing. (Verified on the Windows dev box against a running `serve` from the previous build.)

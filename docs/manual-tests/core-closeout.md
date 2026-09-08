# Manual tests — core close-out (W2 / W8 / W9 / W10)

Seven changes landed whose acceptance test needs hardware, an OS locale, or a link that neither dev
box has. Each is written down here with the exact command and the exact expected reading, so
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

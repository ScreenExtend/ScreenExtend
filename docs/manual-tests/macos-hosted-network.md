# Manual test: the macOS hosted Wi-Fi network

The host AP is the reachable analogue of AWDL — one 802.11 hop between the Mac and
the client instead of two through a router — and
[`docs/research/airplay-why-fast.md`](../research/airplay-why-fast.md) argues it is
where the p99 latency win lives. None of that can be established on the development
machines.

The 10.15.8 dev box has exactly one Wi-Fi interface, `en3`, and it carries the SSH
session all of this work runs over. Starting an AP on it drops the connection, so
**no `startHostAPMode…` call has ever been issued on this project's hardware.** What
*was* established there is the read-only surface (see below); everything about
association, security, DHCP and throughput is open.

Run this on a modern Mac — ideally two, one Intel and one Apple Silicon — with an
iPhone or iPad and an Android phone. Record the macOS build, the Mac's model, and
the exact log lines ScreenExtend prints, which now name the backend, the channel,
the band, the security type and the host's own address.

## What is already known

`cargo test --lib macos_utils::hosted_network::tests::report_hostap_surface -- --ignored --nocapture`
is read-only and safe anywhere. On macOS 10.15.8 Intel it reports:

```
interfaceMode : CWInterfaceMode(1)          (Station)
responds : startHostAPModeWithSSID:securityType:channel:password:error:   true
responds : stopHostAPMode                                                 true
responds : startHostAPMode:                                               true
responds : enableHostAPMode                                               true
responds : disableHostAPMode                                              true
responds : startIBSSModeWithSSID:security:channel:password:error:         true
responds : stopIBSSMode                                                   true
backend       : HostAP
5 GHz channels: 36 40 44 48 52 56 60 64 100 … 140 149 153 157 161 165
would pick    : 36
```

**Run this first on every machine in the matrix.** If `startHostAPModeWithSSID:…`
reports `false` anywhere — the outcome the Apple Silicon question hangs on — that is
the single most valuable result in this document, and the rest of the script does not
apply to that machine.

## What "correct" means

| Situation | Required behaviour |
|---|---|
| The SPI is absent | `probe_hosted_network_backend()` returns `IBSS` or `Unsupported`; the app **does not crash**; the log names the tier |
| A secured start succeeds | the phone associates without a security warning; the log line says `verified … host address 192.168.x.x` |
| The AP comes up with no DHCP | the log says `advertised … but the interface never got an IPv4 address`; the app never claims it is working |
| Every secured type is refused | the open AP is started **last**, and the destructive "Network Created Without Password" toast appears |
| `SCREENEXTEND_HOSTAP_ALLOW_OPEN=0` | the open fallback is refused and `start_hosted_network` returns false |
| The AP is taken away by the OS | within 5 s the liveness watcher logs it and the dashboard's hosted-network state clears |

---

## 1. Does the SPI still exist, per machine

Run `report_hostap_surface` on each of: an Intel Mac on 12/13/14, an Apple Silicon
Mac on 14/15/26, and anything else available. Record the seven booleans verbatim.

Class dumps show the selector present through CoreWLAN 1375.2 (Monterey era) and
nothing after that; Apple Silicon Macs are widely reported not to support HostAP on
the built-in interface. This step is what turns both into facts.

## 2. The security-type sweep

This is the one that unblocks the whole feature.

```
cargo test --lib macos_utils::hosted_network::tests::sweep_security_types -- --ignored --nocapture
```

It walks `securityType` over 0-64 plus 96/128/160/192/256/384/512/768/1024/2048,
recording for each value whether the SPI returned YES and whether `interfaceMode()`
then read HostAP, and stops the AP between attempts.

Do it on a Mac you can reach physically — the sweep repeatedly takes the Wi-Fi
interface down.

For every value the sweep accepts, scan from a second device and record what the
network advertises: open, WEP, WPA2-PSK or WPA3-SAE. Then try to associate an iPhone
and an Android phone to each.

The two constants in the tree, 2 (open) and 16 (WEP), come from a 2012 gist whose own
comment says it swept `[-127, 127]`. **Nothing at or above 128 has ever been tried on
any macOS**, which is why the shipped ladder starts at 1024 and works down. If a WPA2
value exists, replace `SECURED_CANDIDATES` in `hosted_network.rs` with it and delete
the guesses.

Separately, try the two surfaces nothing in the tree calls:

- `startHostAPMode:` — no SSID and no security type, so it plausibly reads both from
  the Internet Sharing preferences, and macOS's own Internet Sharing produces a WPA2
  AP through the same CoreWLAN machinery. If that is what it does, it is a better tier
  than anything the sweep can find.
- `enableHostAPMode` / `disableHostAPMode` — a separate two-step surface.

## 3. Association and DHCP

1. Start the hosted network from the app with an 8+ character password.
2. Read the log. It says one of:
   - `'<ssid>' verified on HostAP — channel 36 (5 GHz), securityType …, host address 192.168.2.1`
   - `'<ssid>' is advertised on HostAP … but the interface never got an IPv4 address`
3. On the phone: does the network appear? Does it associate? Does it show a security
   warning? Does it get an address in the same subnet as the host?
4. Open the join URL and complete a session.
5. `ifconfig en0` (or whichever interface) on the Mac, and `ps ax | grep bootpd` to
   confirm the DHCP server is the one macOS started.

Deliberately break it: block the client, or stop `bootpd`, and confirm the app reports
`advertised`, never `verified`.

## 4. Channel choice, AWDL, and whether 44 is actually bad

`PREFERRED_5GHZ_CHANNELS` is now `[36, 40, 48, 153, 157, 161]` and 2.4 GHz falls back
to 11 rather than 6 — 6, 44 and 149 are AWDL's social channels, where every nearby
Apple device parks its radio for AirDrop and Handoff discovery.

That change is **not settled**, and
[`findings/minor-net-hostap-avoid-awdl-social-channels.md`](../../findings/minor-net-hostap-avoid-awdl-social-channels.md)
explains why the two effects point in opposite directions: sharing a social channel
means contending with every AWDL peer in the room, but the *stutter* people measure
comes from the radio leaving the link channel, so sitting on a social channel might
make the hop free.

`SCREENEXTEND_HOSTAP_CHANNEL=<n>` forces a channel without a rebuild. A/B it:

1. With the client associated to the host AP, run a 60 s ping at 10 ms intervals from
   the client and plot the RTT histogram.
2. At t=20 s and t=40 s, open and dismiss the AirDrop sheet on the host, and have a
   second Apple device nearby do the same, to force AWDL activity.
3. Repeat on channel 44, channel 36 and channel 157.
4. Metric: p99 RTT and the number of samples above 50 ms. Community measurements
   describe the hop as 1-2 s every 10-12 s costing 50-200 ms stalls; look for that
   shape.

Then set the constant from the result and write the reasoning into the comment above
it, so it is not "fixed" later by someone who has only read the Stute paper.

## 5. Throughput against a router

The claim this feature rests on is that removing the AP is worth a few milliseconds at
the median and tens to hundreds at p99. Measure it:

1. `iperf3` between host and client, (a) both associated to a normal router, (b) the
   client associated to the ScreenExtend host AP.
2. Repeat (a) with the router loaded — a large file transfer from a third device.
3. Then run the glass-to-glass harness (`docs/TELEMETRY.md`) for five minutes in each
   configuration and compare p50/p95/p99 plus `getStats` jitter and `framesDropped`.

If p99 does not improve,
[`findings/major-net-direct-link-lowlatency-mode.md`](../../findings/major-net-direct-link-lowlatency-mode.md)
is wrong and should be closed.

## 6. The IBSS tier

On a machine where the HostAP SPI is absent but IBSS is not, confirm the fallback
starts, that the log states the ~20 Mbit/s ceiling, and how a modern phone reacts to a
WEP-104 ad-hoc network. Apple removed "Create Network…" from the Wi-Fi menu on Apple
Silicon and in the redesigned Ventura menu, so this tier may already be dead in
practice — which is itself the result worth recording.

## 7. Teardown

After every case: stop the hosted network from the app and confirm
`stop_hosted_network` returns true, `interfaceMode()` is back to Station, and the Mac
rejoins its normal Wi-Fi network without the user touching the menu bar.

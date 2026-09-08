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

## What the 10.15.8 box has now answered

The AP was started here after all, by running the measurement binary detached with a watchdog that
restores the radio — the dev box's only network is the Wi-Fi interface the AP takes, so every
remote session drops for about a minute. `docs/manual-tests/macos-hosted-network.md` §8 has the
script. `macos_utils::hosted_network::tests::measure_hostap_start`, macOS 10.15.8 Intel, `en3`,
run twice with identical results:

```
backend: HostApSpi
channel 36 (5 GHz)
interface Some("en3"), station address before the start Some(172.16.11.81)
securityType 1024 (unconfirmed): accepted=false in 2.13s
securityType  512 (unconfirmed): accepted=false in 2.08s
securityType  256 (unconfirmed): accepted=false in 2.07s
securityType  128 (unconfirmed): accepted=true  in 2.21s, ap_mode=true at 2.21s
holding 'ScreenExtend-Measure' up for 45s
  t+ 5s mode_is_ap=true ipv4=None
  …
  t+45s mode_is_ap=true ipv4=None
```

Three things, and the second is the important one.

1. **`securityType 128` is accepted and brings the radio to AP mode in 2.21 s.** It is the first
   new HostAP security constant this project has found, and it is outside the `[-127, 127]` window
   the 2012 sweep covered — which is exactly why the shipped ladder starts at 1024 and works down.
   1024, 512 and 256 are each refused, taking ~2.1 s to say so.

2. **Nothing serves DHCP.** With the AP up and `interfaceMode()` reading HostAP for 45 s straight,
   `en3` held no IPv4 address of its own at any point. So on this floor
   `startHostAPModeWithSSID:securityType:channel:password:error:` brings up a *radio* and not a
   *network*: a client can see the SSID, and if it associates it gets no lease and cannot reach
   the host. This is the concrete shape of "slow and randomly fails", and it is why
   `startHostAPMode:` (the no-argument variant, which plausibly reads the Internet Sharing
   preferences and would therefore get `bootpd` and NAT with it) is the next thing to try — see §9.

3. **The station address survives the switch.** At t+2.2 s, with the interface already in HostAP
   mode, `getifaddrs` still reported `172.16.11.81` — the address of the network the Mac had just
   left. A verification that accepts it reports the AP as working on the old LAN address, so
   `wait_for_ipv4` now takes the pre-start address and refuses it.

**Not established here:** what security `128` advertises. That needs a scan from a second device
while the AP is up. A `netsh wlan show networks mode=bssid` from the Windows box during the 45 s
window did not surface the SSID, which is inconclusive rather than negative — Windows serves
cached scan results and will not force a fresh scan while associated. Since the AP serves no DHCP,
a client could not join regardless of what it advertises, so (2) is the blocking question and this
is the one after it.

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

## 8. Running the AP on a box whose only network is the Wi-Fi interface

The dev box has one Wi-Fi interface and no Ethernet, so starting the AP drops every SSH session.
The measurement still runs, detached, with two safeguards: the test binary is invoked **directly**
rather than through cargo (so no build can start while the box is unreachable, and no shared cargo
lock is held), and a watchdog restores the radio whether or not the measurement finishes.

```sh
# Build first, while the box is still reachable.
cargo test --lib --no-run          # note the "Executable unittests" path it prints

cat > /tmp/hostap.sh <<'SH'
#!/bin/sh
LOG=/tmp/hostap.log; IFACE=en3; BIN=<the path cargo printed>
: > $LOG; rm -f /tmp/hostap.done
restore() {
  /usr/sbin/networksetup -setairportpower $IFACE off; sleep 4
  /usr/sbin/networksetup -setairportpower $IFACE on;  sleep 25
  ifconfig $IFACE | grep 'inet ' >> $LOG
}
( sleep 260; [ -f /tmp/hostap.done ] || { pkill -f "$BIN"; restore; } ) &
SCREENEXTEND_HOSTAP_HOLD_SECS=45 $BIN --ignored --nocapture --test-threads 1   measure_hostap_start >> $LOG 2>&1
touch /tmp/hostap.done
restore
SH
chmod +x /tmp/hostap.sh
nohup /bin/sh /tmp/hostap.sh > /dev/null 2>&1 < /dev/null &
```

Reconnect after about ninety seconds and read `/tmp/hostap.log`. `networksetup -setairportpower`
needs no admin rights, and macOS rejoins its known network from the keychain on its own; the power
cycle is belt and braces. Use `measure_hostap_start` (the shipped ladder, stops at the first
success) rather than `sweep_security_types` (75 values, up to ten minutes off the network) unless
the sweep is the point.

## 9. The two HostAP surfaces nothing has ever called

`report_hostap_surface` reports `startHostAPMode:`, `enableHostAPMode` and `disableHostAPMode` all
present on 10.15.8, and nothing in the tree calls any of them. Given §"What the 10.15.8 box has now
answered" (2) — the SSID-taking variant brings up a radio with no DHCP behind it — these are the
most valuable remaining experiment in this document:

1. Configure Internet Sharing in System Preferences (share from Ethernet to Wi-Fi), then call
   `startHostAPMode:` with no SSID and no security type. Record whether the interface gets an
   address, whether `bootpd` starts, and what the resulting network advertises.
2. Try `enableHostAPMode` followed by `startHostAPModeWithSSID:…`, in case the pair is a
   prepare/commit sequence and the SSID variant alone is only half of it.
3. In both cases hold the AP up and associate a phone. An address on the client is the answer.

If (1) works, the shipping path should probably be "configure sharing, then `startHostAPMode:`",
with the SSID variant kept only for machines where that fails.

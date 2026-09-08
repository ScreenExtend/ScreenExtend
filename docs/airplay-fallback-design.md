# AirPlay virtual-display fallback (macOS 10.13 – 10.14.3)

How ScreenExtend creates a display on Macs too old for `CGVirtualDisplay`: by
standing up a minimal fake AirPlay receiver on the same machine and letting
macOS itself create the extended desktop.

Implementation: `src-tauri/src/macos_utils/airplay/`.
Backend selection: `src-tauri/src/macos_utils/virtual_display.rs::probe_display_backend`.

---

## 1. Scope, and a correction to the premise

The task was framed as "CGVirtualDisplay doesn't exist below 10.15". That floor
is one release too conservative.

**`CGVirtualDisplay` ships in macOS 10.14.4**, six months before Sidecar.
Verified two independent ways:

* Versioned private-header dumps ([w0lfschild/macOS_headers](https://github.com/w0lfschild/macOS_headers)):
  CoreGraphics `1247.4.1` (10.14.1) exports **no** ObjC classes at all;
  `1251.8.4.2` (build 18E174f, **10.14.4**) exports all four
  `CGVirtualDisplay*` classes, as do 10.14.5 and 10.14.6 (`1265.6.1`).
* Apple's own SDK stubs ([phracker/MacOSX-SDKs](https://github.com/phracker/MacOSX-SDKs)),
  `CoreGraphics.tbd` → `objc-classes:` — 0 blocks in the 10.12/10.13/10.14 SDKs,
  1 block in the 10.15 SDK naming exactly those four classes.

So the tiering is a **runtime class lookup, never a version comparison**
(`virtual_display.rs::cg_virtual_display_available`) — `probe_display_backend`
asks whether the classes exist and falls back only when they do not. That alone recovers
10.14.4 – 10.14.6 on the good path with no new risk, and narrows what the
AirPlay fallback has to cover to **10.13.0 – 10.14.3**.

There is also a lower door worth knowing about: `CGVirtualDisplay` is a thin
ObjC wrapper that resolves `CDVirtualDisplayCreate` / `…ApplySettings` /
`…Destroy` out of CoreDisplay at runtime (`strings CoreGraphics` contains
`CDVirtualDisplay supported` / `CDVirtualDisplay NOT available at runtime`).
`probe_display_backend` `dlsym`s for `CDVirtualDisplayCreate` and logs when it
finds it without the ObjC classes — if that ever fires on 10.13, a direct C path
would be far better than everything below.

### Tiers

| macOS | Backend | Notes |
|---|---|---|
| 10.15+ | `CGVirtualDisplay` | unchanged |
| 10.14.4 – 10.14.6 | `CGVirtualDisplay` | **newly reachable**, was version-gated away |
| 10.13.0 – 10.14.3 | `AirPlayFallback` | this document |
| < 10.13 | unsupported | Tauri itself does not run there |

`bundle.macOS.minimumSystemVersion` moves 10.15 → 10.13 accordingly.

---

## 2. Viability — verified end to end on a live Mac

**This is not a paper design. The whole flow was run against macOS 10.15.8 and it
works**, with the AirPlay backend forced (10.15 would normally pick
`CGVirtualDisplay`). One run, start to finish, in 15 seconds:

```
accessibility trusted: true
[airplay] advertising "ScreenExtend Live Test" on port 50188, interface index 7,
          deviceid d2:40:f4:a9:02:b3 (features 0x527FFEE6,0x0)
[airplay] receiver up: control 50188, mirror 50189, timing 49486, advertising 1920x1080@60
[airplay] selected "ScreenExtend Live Test" in the Displays menu
[airplay] control connection from 172.16.11.81:50193
[airplay] mirror stream connected from 172.16.11.81:50194
[airplay] display 4128832 moved to the requested 1920x1080
[airplay] display 4128832 attached as an extended desktop (1920x1080)
created display 4128832 at (1920, 1080)
mirroring: false
modes offered: [(800,600,60), (1024,768,60), (1280,720,60), (1280,1024,60),
                (1440,900,60), (1680,1050,60), (1920,1080,60)]
after 5s: (1920, 1080)
[airplay] stopped the AirPlay session from the Displays menu
[airplay] receiver down after 26 requests (55 mirror packets)
[airplay] withdrew Bonjour advertisement ×2
removed cleanly: true
```

Every load-bearing claim in this document is therefore observed rather than
inferred: the Mac **did** list a receiver running on itself, **did** connect to
it, **did** accept the static FairPlay tables and the SETUP responses, **did**
create a real extended-desktop `CGDirectDisplayID`, and **did** remove it again.
The receiver served 26 RTSP requests and drained 55 mirror packets without ever
decoding one.

Reproduce it with:

```sh
cargo test --lib macos_utils::airplay::tests::live_end_to_end -- --ignored --nocapture
```

### 2.1 The same-host self-filter — real, and defeated

This was the gate the design rested on, and it is a real mechanism.

`APTransportDeviceIsSelf` (`APTransport` +0x1b1a1) is:

```c
uint64_t deviceID = CFDictionaryGetInt64(dev->eventInfo, kAPBrowserEventInfoKey_DeviceID, NULL);
uint64_t myMAC    = GetPrimaryMACAddress(&buf, NULL);
bool     localAdv = CFDictionaryGetInt64(dev->eventInfo, kAPBrowserEventInfoKey_IsLocallyAdvertised, NULL) != 0;
return (deviceID == myMAC) || localAdv;
```

and `_APBrowserGetIsLocallyAdvertised` (+0x41185) is "did **any** Bonjour record
for this device arrive with `interfaceIndex == 0`". The override that would
permit a local endpoint regardless (`mgr+0x70`, written only at
`_manager_handlePreferencesChanged` +0x1fbd3) is gated on
`IsAppleTV() || APSIsAPMSpeaker()` — permanently false on a Mac.

Both halves are avoided deliberately:

* **`deviceid`** is a synthetic, stable, locally-administered MAC derived from
  the machine's `IOPlatformUUID` — never the host's real NIC address
  (`dnssd.rs::Identity::derive`).
* **Registration uses an explicit interface index**, never
  `kDNSServiceInterfaceIndexAny` (`dnssd.rs::primary_interface_index`).
  Measured: `dns-sd -R` (any) still produces `Bonjour PTR Add … on 0` inside
  `AirPlayXPCHelper`, while `DNSServiceRegister(…, if_nametoindex("en3"), …)`
  produces only `on 7`.

**This is the single most important line of code in the module and the easiest
thing to regress.** A test asserts the index is never 0.

Supporting cross-host field evidence, for the OS we cannot test on:

| Date | Sender | Receiver | Result |
|---|---|---|---|
| 2019-08-04 | **10.14 Mojave** | RPiPlay | ***"Extending the display works"*** ([RPiPlay#6](https://github.com/FD-/RPiPlay/issues/6)) |
| 2021-09-28 | 10.15.7 | RPiPlay / UxPlay 1.35 | verified by maintainer ([RPiPlay#271](https://github.com/FD-/RPiPlay/issues/271)) |
| 2025-09-23 | macOS 26 | UxPlay | still works ([UxPlay#458](https://github.com/FDH2/UxPlay/issues/458)) |

Discovery of a non-paired third-party receiver has never regressed on any macOS
release, and extend mode is separately confirmed on Mojave.

The strongest precedent for the approach: **Duet Display shipped exactly this
mechanism on macOS 10.13.4**, after Apple broke the `IOFramebuffer` kext path
that Duet, Air Display, iDisplay and DisplayLink all used. Duet support at the
time: *"AirPlay only supports the Apple TVs' resolutions… causing resolution
conflicts."* That constraint is real, and §6 is how we get around it.

### 2.2 The `APBonjourCache` "pairing ID" log is a red herring

An earlier investigation hit
`### Ignoring device found without pairing ID: …` and read it as the receiver
being rejected. It is not. Both that string and its sibling
`Ignoring device found non-cacheable` live in
`-[APBonjourCacheManager _deviceFound:altPairedInfo:recheck:event:]`
(APTransport +0x1e966); every branch falls into the same void epilogue, and the
only thing an "ignore" skips is `_addDeviceToCache:` at +0x1ee19. That class
persists devices so they can be shown while offline. It is a *sibling* consumer
of the browser event, disjoint from `manager_handleBrowserAddOrUpdateEvent`, and
cannot suppress a device. Confirmed live: a genuine third-party receiver on the
LAN (`'YYBox', 0x5A7FFEE6`) logs `Ignoring device found non-cacheable` on every
announcement while remaining perfectly listable.

### 2.3 Anything better available on 10.13?

No.

* **`IOFramebuffer` kext** — what Duet/Air Display/iDisplay/DisplayLink used.
  Apple closed it in **10.13.4**; Avatron's own FAQ still says *"Apple broke its
  virtual video drivers in macOS High Sierra 10.13.4 through 10.13.6"*. Dead on
  the test box by Apple's design, and needs a kext signing entitlement Apple no
  longer grants.
* **DriverKit / `IOUserFramebuffer`** — does not exist until 10.15/11.
* **Screen sharing** — circular; needs a display to already exist.

---

## 3. The wire protocol

Two AirPlay mirroring protocols are easy to confuse. The one in
`sources/docs/nto-AirPlay.html` §6 — hard-coded port 7100, `GET /stream.xml`,
`POST /stream` — is the 2012 iOS-5-era protocol and is **not** what a 10.13 Mac
speaks. macOS 10.13/10.14 uses the RAOP RTSP protocol on the `_raop._tcp` port,
with mirroring carried as SETUP stream type 110. That is what UxPlay and RPiPlay
implement and what this module implements.

Dispatch reference: `sources/repos/UxPlay/lib/raop.c:406-472`; the AirPlay-1-era
subset that actually matters is `sources/repos/RPiPlay/lib/raop.c:160-209` —
**11 verbs, zero HTTP/1.1 handlers**, and RPiPlay mirrors macOS fine. That is the
tightest available bound on the minimum.

### 3.1 Sequence

| # | Request | Our answer | Required? |
|---|---|---|---|
| 1 | `GET /info` + bplist qualifier `txtAirPlay` | bplist `{txtAirPlay: <TXT blob>}` | yes |
| 2 | `GET /info` bare | full device description incl. `displays[0]` — **this is where geometry is requested** | yes |
| 3 | `POST /pair-setup` | our 32-byte public key, unvalidated | only if features bit 27 is on |
| 4 | `POST /pair-verify` ×2 | **not implemented** — see §3.3 | only if bit 27 is on |
| 5 | `POST /fp-setup` (16 B) | `REPLY_MESSAGES[req[14]]`, 142 B | yes |
| 6 | `POST /fp-setup` (164 B) | `FP_HEADER[12] ‖ req[144..164]`, 32 B | yes |
| 7 | `OPTIONS` | 200 + `Public:` | stub |
| 8 | `SETUP` #1 (`ekey`/`eiv`/`timingPort`) | `{timingPort, eventPort: 0}` | yes |
| 9 | `SETUP` #2 (`streams:[{type:110,…}]`) | `{streams:[{type:110, dataPort}]}` | **yes — this creates the display** |
| 10 | *(sender opens TCP to `dataPort`)* | accept, frame, discard | yes |
| 11 | `SETUP` #3 (`streams:[{type:96,…}]`, `usingScreen`) | `{streams:[{type:96, dataPort, controlPort}]}` | yes — see §3.4 |
| 12 | `RECORD` | 200 + `Audio-Latency`, `Audio-Jack-Status` | stub |
| 13 | `GET_PARAMETER` | `text/parameters`, `volume: 0.0` | answer properly (cheap) |
| 14 | `SET_PARAMETER` | 200, parse nothing | stub |
| 15 | `POST /feedback`, every ~2 s | bare 200 | **yes, forever** |
| 16 | `TEARDOWN` ×N | 200 + `Connection: close` | yes |

Every response echoes `CSeq` and carries `Server: AirTunes/220.68`
(`raop.c:487-499`; RPiPlay hard-codes it at `raop.c:156`).

Omitted entirely: the whole HTTP/1.1 half (`/reverse`, `/server-info`,
`/playback-info`, `/scrub`, `/rate`, `/play`) — AirPlay video/HLS, dispatched
only under `protocol == "HTTP/1.1"` (`raop.c:445-472`), and RPiPlay implements
none of it while mirroring correctly. We answer 404 rather than 501, because
UxPlay's own `default:` only 501s unknown *RTSP* verbs and an unknown verb from a
real sender means we misparsed.

`eventPort` is genuinely dead in mirror mode — UxPlay hardcodes 0 with the
comment *"the event port is not used in mirror mode or audio mode"*
(`raop_handlers.h:921-928`).

### 3.2 `/fp-setup` needs no crypto

`fairplay.rs`. This is the only part of the handshake that looks like FairPlay
and isn't:

* Message 1 (16 bytes) → one of **four** static 142-byte replies selected by
  `req[14]` (`sources/repos/UxPlay/lib/fairplay_playfair.c:48-62`).
* Message 2 (164 bytes) → 12 constant bytes followed by an echo of
  `req[144..164]` (same file, `:64-80`).

No key material, no session state, no RNG. The real FairPlay work begins at
`fairplay_decrypt`, which recovers the AES key **for the video stream** — and we
never decode video, so `lib/playfair/` (the 483 KB GPLv3 `omg_hax` tables) is
unreachable from here. Independently corroborated by four implementations
carrying byte-identical tables: RPiPlay `lib/fairplay_playfair.c:8-13`,
`airplay2-receiver-python/ap2/playfair.py:113-124`,
`shairplay-rust/src/crypto/fairplay.rs:16-57`.

Two things a "just replay a canned response" reading gets wrong, and which the
implementation gets right:

* the 142-byte reply is **four** blobs, not one — replaying a captured one is
  wrong for three of the four modes;
* the 32-byte reply is **request-dependent** by construction, so a canned one
  fails deterministically.

`respond()` also rejects `req[4] != 0x03` explicitly. A captured **FairPlay
version 2** message-1 exists in the wild
(`airplay2-receiver-python/ap2/playfair.py:126`) with no published reply table
anywhere. UxPlay's failure mode for it is silent — it returns 200 with a
zero-length body — so we return 400 and log the actual version/type/seq/mode
bytes instead. On a 10.13-era sender that is the single most likely way this
fails, and it will now say so.

### 3.3 Pairing

Features bit 27 ("supports legacy pairing") is advertised **off**
(`receiver.rs::LEGACY_PAIRING`), which is what UxPlay ships by default
(`uxplay.cpp:164`, `:2142-2143`) and what the field reports on Catalina and
Tahoe were running. With the bit clear a sender skips `/pair-setup` and
`/pair-verify` entirely, which saves a round trip and the ed25519 / x25519 /
AES-CTR implementation we would otherwise need.

This is a real, flagged risk rather than a settled question. UxPlay's own README
hedges ("**most** clients will then not attempt…") and documents reverting to bit
27 **on** as the fix for protocol failures; `raop_handlers.h:834-836` records a
client that ignores the bit entirely. RPiPlay — the implementation with the
explicit "extending works" report on Mojave — ships bit 27 **on** and implements
full pairing.

So: `LEGACY_PAIRING` is a one-line switch, `/pair-setup` is already answered, and
`/pair-verify` returns 403 with a log line naming this document. If 10.13.6 turns
out to demand pairing, the work is real pair-verify (ed25519 sign, x25519 ECDH,
`SHA512("Pair-Verify-AES-Key" ‖ shared)`, AES-128-CTR over the signature —
`sources/repos/UxPlay/lib/pairing.c:29-30, :441`), and `ring` is already in the
tree for the first three of those four.

### 3.4 Audio: we answer, but decline

A real mirroring session sets up a type-96 audio stream alongside type 110,
carrying `usingScreen: true` (`raop_handlers.h:946-1044`, esp. `:1014-1021`), and
tears both down at the end. No reference receiver refuses one, and UxPlay treats
an unhandled stream type as a disconnect condition
(`raop_handlers.h:1044-1048`) — so refusing it risks killing the session and
therefore the display.

We therefore answer the type-96 SETUP with real, **bound** UDP ports (naming an
unbound port earns ICMP unreachables) and never read them, while advertising an
**empty `audioFormats` / `audioLatencies`** in `/info` so the sender has no
reason to route system audio at us in the first place. This matters beyond
politeness: if macOS treats the receiver as the audio destination it silences the
host, which is exactly what ScreenExtend's own audio capture needs. On the target
range this is belt-and-braces — `probe_audio_backend()` returns `Unsupported`
below 10.15 regardless (`audio/mod.rs:83-113`, `audio/legacy/probe.rs:17-20`).

---

## 4. What we build on

**Hand-rolled, with two crates that were already in `Cargo.lock`.**

Every candidate receiver is either the wrong direction, the wrong protocol era,
or licence-incompatible with distribution as-is:

| Crate | Verdict |
|---|---|
| `shairplay-rust` | LGPL-3; and its mirroring path is AirPlay-2 stream-110 behind HomeKit pairing + ChaCha20 RTSP — the era our target predates. No AP1 `/info`, no `/stream`. |
| `airplay2-rs-lmcgartland` | GPL-2.0-only, and a **sender**. |
| `airplay2-rs-jburnhams` | a sender; eight `-rc` crypto crates; pulls `fdk-aac` (C). |
| `airguitar` | RAOP audio only — no `fp-setup`, no `_airplay._tcp`. |
| `fake-airplay` | 150 lines, discovery only; pulls `zeroconf` → `avahi-sys` on unix. |
| `rtsp-types` | RFC 7826 only — verified to `Err` on the `HTTP/1.1` requests that arrive on the same socket. |

There is no crate that shortens the critical path. What we wrote instead:

| Piece | Module | Why hand-rolled |
|---|---|---|
| Bonjour advertise | `dnssd.rs` | 7 `extern "C"` decls against libSystem's `dns_sd.h` (10.5-era symbols, no `#[link]`, no `build.rs`). A NULL callback means mDNSResponder owns the advertisement — no event loop. Critically, it also lets us choose the **interface index**, which the crates do not expose and which §2.1 makes mandatory. |
| RTSP transport | `rtsp.rs` | `hyper` cannot parse `RTSP/1.0` (`httparse-1.10.1/src/lib.rs:758-788` byte-compares the version token) nor emit it (`hyper-1.6.0/src/proto/h1/role.rs:406-413` can only write `HTTP/1.x`). So axum is out. We swap the 4-byte token **on the request line only** and echo the original back. |
| Binary plists | `info.rs` via `plist` | `bplist00` by hand is ~600–800 LOC and the one component where a subtle bug is a silent no-connect. |

Dependencies added:

```toml
plist    = "1"   # resolves to 1.7.4, MIT, already present via tauri-codegen
httparse = "1"   # resolves to 1.10.1, MIT/Apache-2.0, already present via hyper
```

Both were already resolved in `Cargo.lock`. Confirmed empirically: adding them
changes `Cargo.lock` by **exactly two lines and zero packages**. No C build
dependency, nothing that breaks on the Command-Line-Tools-only box.

Rejected on purpose: pure-Rust mDNS responders (`mdns-sd`, `libmdns`). They do
work alongside `mDNSResponder` — that part of the folklore is wrong, and it was
tested — but they add five crates for no benefit and, decisively, give us no
control over the advertised interface index.

---

## 5. Display-ID detection

`topology.rs`. **Polling, not `CGDisplayRegisterReconfigurationCallback`.**

The callback is the obvious choice and it is wrong here:

* `ScreenExtend serve` never starts a run loop. `cli::run_serve` parks the main
  thread on `rx.recv()` (`cli.rs:486-495`) and `.run()` is never reached, so a
  registered callback would return `kCGErrorSuccess` and never fire.
* The obvious workaround is dead. Measured on 10.15.8: registering from a worker
  thread returns `CGError 0` but installs **no run-loop source** — that thread's
  `CFRunLoopRunInMode` returns `kCFRunLoopRunFinished` (no sources at all). Unlike
  `CGEventTapCreate`, there is no `CFMachPortRef` to add to a loop of our own.
  Matches `CGDisplayConfiguration.h:186-189`.
* Even in the GUI path it is the wrong shape: the post-change callback fires once
  per added, removed *and* currently-online display, so you debounce and diff
  against a snapshot anyway — i.e. you write the polling diff regardless.

The algorithm:

1. `topology::baseline()` snapshots `CGGetActiveDisplayList` **before** the
   picker press.
2. `wait_for_new_display` polls at 100 ms for a display not in the baseline,
   skipping built-ins and zero-geometry entries, preferring an exact
   width×height match and settling for the first candidate near the deadline.
3. `stop_mirroring` un-mirrors it (§7).

This slots into the existing correlation for free. `server.rs:861-890` already
holds `DISPLAY_CORRELATION_LOCK`, snapshots `pipeline::monitor_device_names()`
(which on macOS *is* `CGGetActiveDisplayList`, `display.rs:15-24`) before
`create_display_with_modes`, and polls `wait_for_new_monitor` after. Because
`create_display` blocks until the display exists, `wait_for_new_monitor` finds it
immediately. On macOS `device_name` **is** the `CGDirectDisplayID` in decimal
(`display.rs:37-44`), so an AirPlay display flows through every downstream
consumer unchanged.

One caveat worth knowing: `SESSION_BUDGET` is 25 s against `server.rs`'s
`DISPLAY_ATTACH_TIMEOUT` of 5 s. That is deliberate — an AirPlay session is a
network handshake plus a WindowServer reconfiguration, not a synchronous API
call — but it means a joining client waits noticeably longer on this backend.

---

## 6. Geometry and refresh rate

This is the part the earlier analysis got wrong in both directions, so it is
worth stating the mechanism precisely.

The receiver's advertised size is carried faithfully all the way to a timing
mode, and then **squeezed through a one-bit bottleneck** at the WindowServer
boundary — unless one preference is set.

```
our /info: displays[0].widthPixels/heightPixels = W,H
  -> APEndpointDisplayDescription (AirPlaySender: widthPixels, heightPixels, edid,
       uuid, rotation, overscanned, refreshRate, maxFPS, widthPhysical,
       heightPhysical, features, primaryInputDevice)
  -> kFigVirtualDisplaySinkDeviceInfoKey_DisplaySizeInPixels
  -> FigDisplayModes_BuildColorAndTimingModes(W, H, overscan, 60, 100, 0)
  -> MediaToolbox scr_bringAirDisplayOnline (+0x4d5d7e):
         kCGSVirtualDisplay1080pMode = (H >= 1080.0)          <-- the bottleneck
         + if com.apple.coremedia/wirelessdisplaymac_set_display_size != 0:
             com.apple.windowserver.virtualDisplayWidth  = W
             com.apple.windowserver.virtualDisplayHeight = H
  -> CoreDisplay __CGXAirDisplayOnline (+0x88e93)
```

**Without the preference**, CoreDisplay `memcpy`s a static 26-entry table
(`__ZL12gVFBModeList`) and `1080pMode` merely picks the default index — 7
(1920×1080) when true, 3 (1280×720) when false. The user-selectable ladder is
800×600 / 1024×768 / 1280×720 / 1280×1024 / 1440×900 / 1680×1050 / 1920×1080,
all 1×, all 60 Hz. That is Duet's 2018 complaint, exactly.

**With the preference**, `__CGXAirDisplayOnline` reaches for its custom-geometry
branch (+0x89348–0x89500), which would write one mode of arbitrary width ×
height. It never gets there — see below.

### What actually happens — measured, not predicted

The static trace above is right about the mechanism and **wrong about the
outcome**. With `wirelessdisplaymac_set_display_size` confirmed set
(`defaults read com.apple.coremedia wirelessdisplaymac_set_display_size` → `1`),
a request for 1440×900 still produced a **1280×720** display, and 1920×1080 also
produced 1280×720. The custom-geometry branch never fires.

The reason: CoreDisplay's `__CGXAirDisplayOnline` takes that branch only when
`com.apple.windowserver.virtualDisplayResolution` is present *as well as*
Width/Height (read at +0x8900f, gating the custom-mode writer at +0x89348), and
**no binary in the shared cache ever writes that key** — MediaToolbox emits
Width and Height but never Resolution. The custom-mode writer is dead code on
10.15.8.

So the design **does not write that preference**. Mutating a global user setting
for no measurable benefit is not a trade worth making. `sender_prefs` keeps only
the ceiling, the refresh-rate truth, and a read-only probe.

**What does work is the mode ladder.** Once the display exists macOS publishes a
real list of modes, and they are selectable. Measured on the live display:

```
modes offered: 800x600, 1024x768, 1280x720, 1280x1024, 1440x900, 1680x1050, 1920x1080
               — all 60 Hz, all 1×
```

exactly the 1× block of CoreDisplay's static `gVFBModeList`. Two consequences
shape the implementation:

* **We always advertise 1920×1080 in `/info`, whatever the client asked for.**
  The advertised size is not a request — macOS keeps one bit of it, and that bit
  decides which ladder you get. Advertising a client's 900-tall viewport would
  set the bit false and cost us the top of the ladder. Advertising 1080-tall
  keeps 1920×1080 reachable.
* **Then we select the client's geometry out of the ladder**
  (`topology::best_effort_mode`), falling back to the entry closest in aspect
  ratio and then area. A 1920×1080 request lands exactly:
  `[airplay] display 4128832 moved to the requested 1920x1080`.

**A trap found the hard way.** `CGDisplaySetDisplayMode` scopes the change to the
calling application, and macOS then keeps the display alive until that
application exits. Measured: with it, the display survived **48 seconds** past
the end of its AirPlay session and only vanished when the process died; with no
mode change at all it disappeared **immediately**. Committing through
`CGBeginDisplayConfiguration` + `CGConfigureDisplayWithDisplayMode` +
`kCGConfigurePermanently` does *not* avoid it. What does is putting the mode back
before teardown — `remove_display` records the geometry macOS granted and
restores it first (§9).

### Confidence

| Property | Verdict | Confidence |
|---|---|---|
| Any geometry on the published ladder (up to 1920×1080) | **Works** | **High — observed** |
| A geometry off the ladder | Snapped to the nearest, and logged | High — observed |
| Matching an arbitrary client resolution | **No** | High — observed |
| Refresh rate | **Always 60 Hz.** `refresh_rate` is dead on this backend | High — observed |
| Backing scale / HiDPI | **Always 1×.** No Retina desktop | High — observed |
| Portrait | **Not reachable** — the ladder is landscape-only | High — observed |
| Exact arbitrary geometry via `wirelessdisplaymac_set_display_size` | **Inert** — the consumer needs a key nothing produces | High — observed + disassembled |

`sender_prefs::clamp` coerces refresh to 60 and enforces the ceiling, returning a
note the caller logs, so the gap between request and grant lands in the log
rather than being silently swallowed.

Why aspect matters more than it looks: the client page uses `object-fit: contain`
(`static/styles.css:19-26`) but `input.js` normalizes pointer coordinates against
the **full viewport box** (`input.js:36-39, 99-100`), which `macos.rs:685-686`
maps linearly onto the **full display rect**. That is only correct when display
aspect equals viewport aspect — which `start_session` guarantees on the
`CGVirtualDisplay` path by creating the display at the client's exact size. On
this backend the ladder will often not match a phone's aspect, so pointer input
will be skewed and the letterbox bands will be dead zones. **That is the most
important known defect of this backend** and it is listed in §13.

## 7. Automation: no user click

**Every step is app-driven. There is no manual step in the design.**

### The extend step is a public API

Selecting "Use As Separate Display" is one CoreGraphics call. Traced at
instruction level on 10.15.8:

* `-[DisplaysExtra buildMenu]` (`Displays.menu/Contents/MacOS/Displays` +0x4c66–0x4cb6)
  builds the item with title ivar `_sUseAsDisplay` and action `stopMirroring:`.
* `-[DisplaysExtra stopMirroring:]` (+0x5ad5) `dispatch_async`es a block (+0x5c55)
  that calls `-[MPDisplayMgr stopMirroringForDisplay:]`.
* That method (`MonitorPanel` +0xc7c2) is, in full:
  `CGSBeginDisplayConfiguration` → `mirrorSetForDisplay:` →
  `CGSConfigureDisplayMirrorOfDisplay(cfg, displayID, 0)` per display →
  `CGSCompleteDisplayConfigurationWithOption(cfg, 2 /* kCGConfigurePermanently */)`.
* The public and private entry points are provably the same function on 10.15:
  in SkyLight, `_SLConfigureDisplayMirrorOfDisplay` @0x73718 is a bare
  `jmp _SLSConfigureDisplayMirrorOfDisplay`, and likewise for Begin/Complete.

So `topology::stop_mirroring` calls the **public**
`CGBeginDisplayConfiguration` / `CGConfigureDisplayMirrorOfDisplay(cfg, id,
kCGNullDirectDisplay)` / `CGCompleteDisplayConfiguration(cfg,
kCGConfigurePermanently)` and skips the menu entirely. `ax::use_as_separate_display()`
exists only as a fallback for the case where CoreGraphics reports success and the
display stays in the mirror set.

Caveat, stated plainly: the API identity is verified; that a **live AirPlay
session survives the unmirror and re-negotiates as an extended desktop** is an
inference. The Mojave field report ("extending the display works") is the
supporting evidence.

### Prerequisite: the Displays menu extra must exist

There is nothing to drive if the menu extra is not in the menu bar, and macOS
only keeps it resident *"when available"* — i.e. while it has seen an AirPlay
destination and the user's **"Show mirroring options in the menu bar when
available"** box is ticked. On a Mac where that box was never ticked, the extra
is absent and the AX route has no target.

Leaving that to the user would make this backend depend on a manual step, so
`ax::find_displays_extra` handles it, cheapest lever first:

1. Extra already present → go.
2. Set `showInMenuBarIfPresent` in `com.apple.airplay` (a per-user preference —
   no privileges, no restart) — this is the same checkbox, written directly.
3. `open` the `.menu` bundle, which asks LaunchServices/SystemUIServer to load
   it live. Note **never** `killall SystemUIServer`: it is unnecessary for the
   add path and visibly flashes the user's entire menu bar. (Removal is the
   operation that would need it — so we only ever add.)
4. Poll up to 5 s, then fail with a message naming the checkbox.

Measured caveat: `open` on `Displays.menu` did *not* append it to
`com.apple.systemuiserver`'s `menuExtras` on 10.15.8 (it does for other bundles,
e.g. `TimeMachine.menu`). What reliably brings it back is the preference plus a
discoverable AirPlay destination — which our own receiver supplies, since it is
already advertising by the time the picker is driven.

### The connect step is Accessibility

There is **no** receiver-side lever for extend-vs-mirror. Checked four places and
found nothing: the `displays[0]` key set has no mode field (its `features: 14` is
bits 1|2|3 = Photo|VideoFairPlay|VideoVolumeControl, and the 53-bit table has no
extended-desktop bit); `statusFlags = 68` is bits 2 and 6 in the status-flag
namespace; the TXT `flags` field is that same namespace; and the client sends no
display-mode `SET_PARAMETER` (`raop_handlers.h:1118-1170` handles only volume,
progress, artwork and DMAP). Mirror-vs-extend is a sender-side, WindowServer-local
decision.

The public sender API is closed: `+[AVOutputContext sharedSystemScreenContext]`
returns nil without `com.apple.avfoundation.allow-system-wide-context`, and
self-signing that entitlement gets the process **SIGKILLed by AMFI even with SIP
disabled**.

So `ax.rs` drives the menu extra through the Accessibility API — the permission
ScreenExtend **already requires and already prompts for**, for remote input
(`permissions.rs`).

**How rows are identified, and why it is not a screen-scrape.** On 10.15 AppKit
publishes each menu item's action selector as its `AXIdentifier`, so the device
row is `connectToAirPlayDevice:` in every locale. That is a **10.15-only
affordance and must not be relied on**: `-[NSMenuItem userInterfaceItemIdentifier]`
on 10.13.6 (AppKit +0x1bc9fe) and 10.14.6 (+0x2e5f2) returns only the `_uiid`
ivar, and `DisplaysExtra` never calls `setIdentifier:` — so on exactly the OS
versions this backend exists for, **the rows have no identifier at all.**

The primary key is therefore the **title**, made language-safe two different ways:

* The **device row** is matched on the name *we* chose when publishing the
  Bonjour service. Nothing to translate.
* **Command rows** are matched against strings read out of the menu extra's own
  bundle with CFBundle — `AIRPLAY_TURN_OFF`, `USE_AS_DISPLAY`, `AX_AIRPLAY_*` —
  so they come back in whatever language the menu is drawn in
  (`ax.rs::menu_string`). A test asserts they still resolve.
* `AXIdentifier` is kept as a *confirmation* where the platform supplies one,
  and as a positional fallback for the Stop row on 10.15+.

**Surviving a live menu.** The picker is a real `NSMenu` that SystemUIServer
rebuilds as devices come and go, and the person at the keyboard can dismiss it
at any moment by clicking elsewhere. Both make element references go stale
(`-25202 kAXErrorInvalidUIElement`) and are indistinguishable from here, so every
menu action goes through one helper that reopens the menu, re-finds the row and
presses it in immediate succession, up to three times. A menu that comes back
*populated but without our row* is a real miss and fails straight away rather
than retrying. Rows must also be **enabled** to match: a busy picker contains
disabled headers such as `AirPlay: <name>` whose titles overlap a device row's,
and on 10.13/10.14 there is no `AXIdentifier` to tell them apart. Several AirPlay
devices being listed is the ordinary case — the row is matched on the exact name
we published, so it is unaffected by how many neighbours appear.

Verified live on 10.15.8: the menu is flat (zero `setSubmenu:` in the binary),
items have an explicit `setTarget:` (+0x2e03) so `AXPress` cannot mis-dispatch,
populate latency is 1–32 ms, and the frontmost application is **not** stolen.
**Waiting for the row.** The picker paints as soon as it opens, but it opens on
`AirPlay: Looking for TV…` and fills the device rows in as discovery answers. So
"the menu has items" does **not** mean "the menu has ours" — looking exactly once
and then closing is how a selection silently fails to happen. Every menu action
therefore holds the menu open and re-reads it until its row appears: up to 6 s
for a device row, which depends on discovery, but only 1 s for `Stop AirPlay` and
`Use As Separate Display`, which are drawn from state the menu already has and
would otherwise make teardown slow. After a successful press we pause briefly
before returning, because `DisplaysExtra` handles these on a `dispatch_async` and
the caller goes straight on to reconfiguring displays.

**Dismissing it.** Pressing the menu-bar extra is a *toggle*, which makes it the
wrong tool for closing: activating a menu item is what dismisses an `NSMenu`, so
a press afterwards re-opens it — that is how the picker ends up sitting on screen
after a device connects. So the successful path deliberately does nothing, and
`close_menu` (used only when the menu was opened without activating anything)
never touches the extra: it sends `AXCancel` to the menu below it — which does
work, unlike on the extra, where it returns `-25206 kAXErrorActionUnsupported` —
and falls back to a synthetic Escape, which can close what is open but cannot
open anything. A test asserts `close_menu` never regains a press on the extra.

Tier order in `create_display`:

1. Advertise 1920×1080 in `/info`, whatever was asked for (§6), and wait for
   Bonjour to settle so the picker can see us.
2. `ax::connect_to(name)` — ensure the menu extra exists, open it, press the row
   whose title is our receiver's name. Fails with a message naming what the
   picker *did* list, which is the diagnostic that matters.
3. `topology::wait_for_airplay_display` — poll the **online** display list for a
   display tagged `aapl`/`airp`.
4. `topology::stop_mirroring` (public CG) → on failure,
   `ax::use_as_separate_display()` (menu) → on failure, error out and disconnect.
   This step is not optional: macOS attaches in **mirror** mode by default on
   all three OS versions, and a mirror slave is not in the active display list,
   so until it runs the rest of ScreenExtend cannot see the display at all.
5. `topology::best_effort_mode` — move onto the requested geometry.

If Accessibility is not granted, `availability()` returns the actionable string
before anything else happens.

**The one honest caveat:** this is verified end-to-end on 10.15.8 for everything
except the final "does our row appear in the menu" hop (§2.1), and the AX
traversal itself is inferred for 10.13.6, where `Displays.menu` exists but was
not available to inspect. The code matches on `AXIdentifier` and
`AXDescription`, not on layout, which is the most version-portable thing
available.

---

## 8. Keep-alive, and exactly how much of the stream we touch

Once the display exists, three things keep it alive.

**`POST /feedback`** — bare 200, every ~2 s. Confirmed sufficient: RPiPlay's
handler is a single log line (`raop_handlers.h:561-566`), and the captured
exchange shows the sender's `POST /feedback RTSP/1.0` answered with a response
carrying no `Content-Length` at all.

**The mirror data socket** — accept and drain. `grep -n 'send\|write(\|sendto'`
over all 965 lines of `raop_rtp_mirror.c` returns **nothing**: the channel is
strictly one-way, so there is no ACK to omit. But a receiver that stops reading
back-pressures the sender's encoder, and `raop_rtp_mirror.c:311-317` and
`:379-385` both special-case `ECONNRESET` into a connection-lost path — that code
exists because it happens.

**This is the one place we go past "handshake only", and it is 15 lines.** We
read the 128-byte header, take the little-endian `u32` at offset 0, read that
many bytes, and discard them — framed, because a blind drain could never
resynchronise. We read exactly one further byte, `header[4]`, the packet type,
because `0x01` (unencrypted SPS/PPS) is re-sent when the sender's geometry
changes and is therefore a free signal that macOS moved the display underneath
us. **No AES, no NAL parsing, no VideoToolbox, no frame buffer, ever.**

**NTP** — nothing needed. The receiver is the NTP *client*: `raop_ntp_thread`
`sendto`s a request to the sender every 3 s (`raop_ntp.c:466-493, :580-586`), and
`nto-AirPlay.html` §6.3 agrees ("The AirPlay server runs an NTP client"). Nothing
polls our timing port, so we bind it, name it honestly in the SETUP response so
the sender's replies have somewhere to land, and never read it.

Also answered on the control connection so the session is not dropped for
rudeness: `OPTIONS`, `GET_PARAMETER` (`volume: 0.0`), `SET_PARAMETER`, with a
`451 Parameter Not Understood` when `Content-Type` is missing — which is a real
requirement, not politeness (`raop_handlers.h:1072-1075`).

We also ask for as little traffic as the protocol allows: `displays[0].maxFPS` is
advertised as **1**. UxPlay does the same when it will render nothing
(`uxplay.cpp:3122-3129`), and it is pure saved host CPU and LAN bandwidth.

---

## 9. Teardown

**Receiver-initiated `TEARDOWN` is not a thing.** In this protocol `TEARDOWN`
travels sender → receiver: it is routed as an inbound method (`raop.c:437-438`)
and handled by parsing a `streams` array to decide what to stop. No code path in
UxPlay or RPiPlay ever *sends* one. Originating one up a connection where we are
the server is not something a sender is written to parse.

**Closing the control socket is the mechanism**, and it is the only
receiver-initiated termination either reference implementation has:
`httpd_remove_connection` does `shutdown(fd, SHUT_WR)` then close
(`httpd.c:225-234`), and `conn_destroy` explicitly tears down the RTP and mirror
sessions *"in case TEARDOWN was not called"* — i.e. connection loss **is** the
normal termination path.

`remove_display(id)` does four things, **in this order** — the order was
established by watching each wrong one fail:

1. **Restore the mode macOS originally granted.** Any mode change we made
   associates the display with this process and macOS then keeps it alive until
   we exit (§6). Putting it back releases that association.
2. **Press "Stop AirPlay" in the menu**, while our device is still advertised.
   This must happen *before* step 3: once the Bonjour records are withdrawn there
   is no row left to press, and the first attempt at this — disconnecting after
   dropping the receiver — failed with
   `macOS is not offering "…" as an AirPlay display`.
   The row is found structurally, not by title: while connected the menu grows a
   leading `connectToAirPlayDevice:` row (Stop AirPlay, i.e. connect-to-nil)
   above the `startMirroring:` / `stopMirroring:` block, with the real devices
   listed after. `ax::disconnect` keys on that ordering, so it survives
   localization.
3. **Drop the receiver** — withdraws both Bonjour registrations (RAII in
   `dnssd::Service::drop`), cancels the accept loops, closes the control socket.
4. **Wait for the id to leave `CGGetActiveDisplayList`.** With steps 1–3 in this
   order it is already gone by the first poll.

`remove_all_displays` additionally calls `sender_prefs::restore()`.

Measured, all three orderings:

| Ordering | Result |
|---|---|
| drop receiver, then disconnect | disconnect fails (device no longer listed); display lingers |
| disconnect, then drop, mode left changed | display survives 48 s+, until the process exits |
| **restore mode → disconnect → drop** | **gone at the first poll** |

The registry (`mod.rs::registry`) mirrors `virtual_display.rs`'s
`DisplayRegistry` deliberately: same `OnceLock<Mutex<…>>` shape, same
`remove_display` / `remove_all_displays` semantics, keyed by
`CGDirectDisplayID`. The difference is that the CG path relies on ARC dropping
the last `Retained<CGVirtualDisplay>`, and here there is no such object — the
session *is* the display's lifetime.

`reconfigure_display` (`virtual_display.rs`) now branches on
`airplay::owns_display(id)`. An AirPlay display has no `applySettings`
equivalent and its advertised geometry is fixed for the life of the session, so
all we can do is `activate_mode` against the modes macOS published — with an
error message that says so rather than the generic "not a known virtual display".

---

## 10. Per-device settings

Every per-device setting ScreenExtend exposes, and what it does on this backend.
Three facts drive the whole table:

1. On macOS `device_name` **is** the display id (`display.rs:37-44`), so id flow
   is unchanged.
2. Every display-object mutation funnels through
   `virtual_display::reconfigure_display`, which is now AirPlay-aware but can
   only select from macOS-published modes.
3. The receiver advertises **one** geometry, fixed at session start. Changing it
   means teardown + re-advertise + reconnect.

| Setting | Where it acts | Verdict | Why |
|---|---|---|---|
| **Client screen size → display geometry** | display object, at create time | **Partial** | The client's exact size is only available if it happens to be on the published ladder (800×600 … 1920×1080, 16:9 and 16:10 only). Otherwise the nearest entry is used and the client is letterboxed — see the input-skew note in §6 |
| **`video_scale`** 10–100% | encoder only (`config.rs:38`) | **Works** | Never touches the display |
| **`video_quality`** QP 1–51 | encoder only (`config.rs:54`) | **Works** | Pure VideoToolbox parameter |
| **`control_enabled`** (remote control) | `CGDisplayBounds` of whatever id (`input/macos.rs:294-315, 724-750`) | **Works** | Traced end-to-end: nothing CGVirtualDisplay-specific. Needs only that the id be in `CGGetActiveDisplayList` and have a sane global-space rect — both true. Accessibility TCC unchanged |
| **Audio output device** (per client) | client-side `setSinkId` (`audio.js:311-348`) | **Works** | Chooses a speaker on the *client*; never touches the host |
| **Approve for auto-join** | token set (`server.rs:607-616`) | **Works** | No display involvement |
| **Ban / unban** | gate + teardown | **Works** | Gate unchanged; teardown is implemented (§9) |
| **`refresh_rate`** 15–500 Hz | display object + encoder | **Partial** | Encoder side works and degrades gracefully (`live_encoder_config` clamps by the display's *real* refresh). Display side is **fixed at 60 Hz** — every published mode is 60 Hz. Requests are coerced and logged |
| **`orientation_portrait`** | display object | **Broken** | Every mode macOS publishes for an AirPlay display is landscape; there is no portrait entry and no way to add one. Observed, not inferred |
| **`scale`** (display scale 25–200%) | `setHiDPI` + halved mode | **Broken** | Two independent failures: no `applySettings`, and no AirPlay wire equivalent for backing scale. CoreDisplay forces scale 1 on this path |
| **`dpr`** (pixel ratio 1.0–2.0×) | pixel count + HiDPI | **Partial, and actively harmful above 1.0** | The pixel-count half works; the sharpness half is the `scale` path above. `dpr=2` yields a 2× larger **1×** desktop — tiny unreadable UI. Should be clamped to 1.0 on this backend |
| **`audio_enabled`** (system audio) | `AudioHub` | **Unavailable on this OS range** | Not a fallback limitation: `probe_audio_backend()` returns `Unsupported` below 10.15 anyway (Process Tap needs 14.2, SCK 13.0, the bundled device 10.15). We additionally decline the AirPlay audio stream so macOS does not hijack the host's output |
| **Device name** | descriptor name | **Partial (cosmetic)** | The macOS-visible name is the Bonjour instance name, fixed at publish. Nothing keys off it |
| **Disconnect grace** 0–600 s | session lifetime | **Partial** | Logic unchanged, but holding the session open keeps the Mac encoding H.264 at a receiver that discards it. Free on CGVirtualDisplay, not free here — consider a lower default |
| **Multiple devices** | — | **Broken by macOS** | One AirPlay display at a time. `create_display` refuses a second with an explicit message |
| **DPR mode ladder** (`extra_modes`) | `create_display_with_modes` | **Dropped** | AirPlay advertises one mode; the trait's default impl already discards `extra_modes` |
| **In-place reconfigure** (`reconfig_epoch`) | `server.rs:831-857` | **Needs care** | That branch only `teprintln!`s a `set_display_mode` failure and then logs "settings changed in place" — on this backend it would *lie*. The error string now says the display is AirPlay-created; routing to destroy-and-recreate is the follow-up |
| Legacy volume-key proxy, audio-driver install | 10.15–12.x only | **N/A** | The subsystem does not exist below 10.15 |
| `ip`, `os`, `screenSize`, `token`, `maxDpr` readback | telemetry | **Works** | From the client's join request |

The capture path is indifferent to how the display was born:
`CgDisplayStreamBackend` takes a bare `DisplayId` (`cgds.rs:47-62`) and its
properties dict contains nothing display-kind-specific. On 10.13.6 the Screen
Recording TCC prompt does not exist at all, which removes that concern entirely.

---

## 11. Licensing

**ScreenExtend is AGPL-3.0** (`LICENSE`, `README.md:142`), which changes the
analysis completely from what a proprietary app would face.

AGPL-3 §13 ¶2 (`LICENSE:552-559`) grants permission *"to link or combine any
covered work with a work licensed under version 3 of the GNU General Public
License into a single combined work, and to convey the resulting work."* So
GPL-3 code from UxPlay and RPiPlay may legitimately be ported in — they are a
**licit source**, not merely read-only documentation.

What is actually reused, and under what:

* **`fairplay.rs`'s 568 bytes of tables** come from UxPlay's
  `lib/fairplay_playfair.c`, which is **LGPL-2.1-or-later** (Copyright © 2018
  Juho Vähä-Herttua) — reachable via the "or later" grant → LGPL-3 → GPL-3 →
  AGPL-3 §13. The module header carries the attribution and marks the changes
  (transcribed to Rust, dispatch keyed on type/seq rather than body length,
  table index bounds-checked), as both licences require.
* Everything else — Bonjour, RTSP, plists, the session machine — is written from
  the protocol facts, with UxPlay/RPiPlay cited as behavioural references.
  Interfaces and protocol facts are not copyrightable; the code is ours.
* **`lib/playfair/` is not used and must not be.** It is full GPLv3 and it is the
  actual FairPlay key-derivation implementation. We never reach it, because we
  never decrypt.

Things a maintainer must know, stated plainly:

* This is **reverse-engineered protocol work**. There is no Apple specification;
  everything here derives from open-source receivers and from disassembly of
  system frameworks on a machine the author owns.
* The `/fp-setup` tables are captured FairPlay handshake constants. We use them
  to *complete a handshake*, and we decrypt nothing — no content protection is
  circumvented, because no protected content is ever processed. That distinction
  is real and worth preserving; it is a reason not to drift into decoding.
* We impersonate an Apple TV model string in a Bonjour record for interoperation.
  No Apple trademark is used in the product UI.

---

## 12. What is still open on 10.13.6

Everything in §2–§9 was verified on 10.15.8. What the 10.13.6 test adds:

1. **Does the picker list us there too?** The self-filter mechanism is the same
   code lineage, but 10.13's `APTransport` was not available to inspect. Watch
   for `[airplay] selected …` versus
   `macOS is not offering … as an AirPlay display. The picker lists: …` — the
   error path deliberately reports what the picker *did* contain.
2. **Does the `Displays.menu` extra expose the same selectors?** `ax.rs` matches
   on `AXIdentifier` (`connectToAirPlayDevice:`, `stopMirroring:`,
   `startMirroring:`, `openDisplaysPref:`) and on `AXDescription`, never on a
   title, which is the most version-portable thing available — but the
   identifiers themselves are a 10.15 observation. If the menu is not found the
   error names that specifically.
3. **What is on the mode ladder?** Print it from the log line; on 10.15 it is
   800×600 … 1920×1080. If 10.13's `gVFBModeList` is shorter, the closest-match
   fallback in `best_effort_mode` covers it, but the achievable sizes shrink.
4. **Does the sender demand pairing?** If the log shows
   `the sender requested /pair-verify`, flip `receiver.rs::LEGACY_PAIRING` and
   plan real pair-verify (§3.3).
5. **Which FairPlay version?** If the log shows
   `FairPlay version N is not supported`, the tables need extending — the
   diagnostic prints version/type/seq/mode.
6. **Does the binary load at all?** Two VideoToolbox keys
   (`kVTCompressionPropertyKey_MaximizePowerEfficiency`, 10.14, and
   `…_PrioritizeEncodingSpeedOverQuality`, 11.0) used to be *eagerly bound*
   references, which dyld resolves before `main()` — on 10.13 that is a hard
   crash of the whole app, not a missing feature. They are now looked up with
   `dlsym` (`macos_utils/streamer/encoder.rs`), and `nm -u` on the release binary
   confirms both symbols are gone. The remaining VideoToolbox imports are all
   10.8-era. Run the raw binary from Terminal (`./ScreenExtend`) rather than
   double-clicking, so any `dyld: Symbol not found:` lands on stderr.

The live harness is the fastest way to answer 1–5 in one go:

```sh
cargo test --lib macos_utils::airplay::tests::live_end_to_end -- --ignored --nocapture
```

Worth capturing alongside it:
```sh
log stream --predicate 'process == "AirPlayXPCHelper" OR eventMessage CONTAINS[c] "airplay"'
```

## 11b. Correlating the display we made — and never touching the user's screen

`server.rs` historically identified the display it had just asked for by
**diffing the monitor list** around `create_display`: snapshot the names before,
snapshot after, take the one that appeared. That is the only option on Windows,
where the driver's display id is not the monitor device name.

It is wrong on this backend, and the failure is severe. Taking a display out of
a mirror set reconfigures the topology, and macOS may **renumber
`CGDirectDisplayID`s while it does** — so the diff can report the *built-in*
display as the newcomer. Everything downstream keys off that name, so the host
would set its **own screen** to the client's resolution and then stream its own
desktop to the client: the two displays effectively swap.

Three changes close it:

* **`VirtualDisplayController::display_device_name(id)`** — a backend that
  already knows which display it made says so, instead of the server guessing.
  `None` (the default, and Windows) keeps the diff.
* Both macOS backends implement it, because on macOS the monitor "device name"
  *is* the display id. The AirPlay one goes further and **re-resolves by
  identity** — `CGDisplayVendorNumber`/`ModelNumber` == `aapl`/`airp` — rather
  than trusting the id it returned a moment earlier, and it moves its registry
  entry if macOS renumbered underneath it.
* `reconfigure_display` now **refuses to touch the main display** unless it is
  one we created. Everything reaching that function is meant to be a
  ScreenExtend display; if a correlation bug ever hands it the user's screen,
  failing loudly is much better than resizing it.

## 11c. Putting the Mac's own screen back

Attaching an AirPlay display starts in **mirror** mode, and to mirror, macOS
changes the built-in display's mode to match. Un-mirroring does **not** undo
that: the Mac's own screen is left at the client's resolution, showing as
"Scaled" in Displays preferences, until someone picks "Default for display" by
hand.

So `create_display` takes a `topology::ModeSnapshot` before it connects and calls
`restore_except(new_display)` once the un-mirror lands — and again on teardown,
since removing the display can disturb the others the same way. Each display's
mode is remembered as its **`io_display_mode_id`**, not its size: at one logical
size there can be both a native and a scaled mode, and "Default for display" is a
specific one of them, so the id is what has to survive. (It is also plain `i32`
data, which `CGDisplayMode` is not — the object is neither `Send` nor `Sync` and
could not live in the session registry.)

## 11d. Keeping the join short

A join used to spend most of its time standing up the receiver: publishing over
Bonjour and then waiting for the sender to rescan. None of that is per-client —
the advertised geometry is a **constant** (§6: macOS keeps only one bit of it,
and the client's real size comes from the mode ladder afterwards) — so the
receiver is started **once, when the backend is chosen**, and simply left
running:

* `AirPlayVirtualDisplay::new_shared()` kicks off `prewarm()` on a background
  thread, so discovery settles while the user is still looking at the QR code.
* `create_display` calls `ensure_receiver()`, which is a no-op when warm. Only a
  cold start pays `DISCOVERY_SETTLE`.
* `remove_display` ends the AirPlay *session* but deliberately leaves the device
  advertised, so a re-join is fast too. Only `remove_all_displays` (app
  shutdown) stops it.

The receiver therefore advertises one stable name, `ScreenExtend`, rather than
one per client — which is also the name macOS shows for the display. The
per-client name is still what the desktop UI uses.

Each attach logs how long it took, and says when it was a cold start:

```
[airplay] display 4128832 attached as an extended desktop (1440x900) in 3.2s
```

## 12b. One display at a time, handled end to end

macOS drives a single AirPlay display, so on this backend the host has exactly
one to give. Rather than letting a second device get all the way through the OTP
and the WHEP handshake before failing, the limit is expressed once and enforced
everywhere:

* `VirtualDisplayController::max_concurrent_displays()` — `None` by default,
  `Some(1)` for the AirPlay backend. Nothing else in the codebase needs to know
  which backend is running.
* `AppState::display_slot_unavailable(client_ip)` in `server.rs` — true when the
  limit is reached **and** this client does not already hold a display. A client
  that does is never blocked, so its `/reconfig` polling, its live WebRTC stream
  and its eventual `/leave` all keep working untouched.
* **The other adapters stop listening entirely.** `sync_streamers` treats a full
  backend as "the desired adapter set is just the one the holder joined through"
  — every other per-adapter server is gracefully shut down, and they come back
  when the slot frees. `DeviceSessionState.host_ip` records which adapter a
  client reached us on so the right one survives; `TauriDeviceReporter` re-runs
  the reconcile on every join and leave. The holder's own server stays up
  because that session still needs signalling (`/reconfig` polling,
  renegotiation after a settings edit, `/leave`) — its WebRTC media is a
  separate peer connection and is unaffected either way.
* `GET /` serves `static/at-capacity.html` with **503** instead of the join page,
  so a new phone gets an explanation rather than a dead end. That page polls `/`
  every 4 s and reloads itself the moment the status turns 200, so whoever is
  holding it does not have to know when to retry.
* `POST /whep` is gated by the same predicate — a page loaded before the display
  was taken cannot sneak an offer through.
* `get_display_capacity` reports `{max, in_use, full, backend}` to the desktop
  UI, which re-reads it whenever the device list changes. While `full`, the
  dashboard replaces both QR codes with an explanation naming the limit and
  pointing at **Devices** to free it, and says macOS 10.15+ is required for more
  than one.

Nothing is torn down when the limit is hit and nothing needs restarting when it
clears: `take_live_display` on leave drops the count, the next `GET /` answers
200, and the dashboard's next capacity read re-shows the codes.

## 13. Known limits

* **Aspect mismatch skews remote input.** The ladder is landscape-only and
  coarse, so a phone client will usually get a display whose aspect differs from
  its viewport. `input.js` normalizes against the viewport box while
  `macos.rs` maps onto the full display rect, so taps land in the wrong place
  and the letterbox bands become dead zones. This is latent on the
  `CGVirtualDisplay` path (which always matches aspect exactly) and becomes real
  here. **Fixing `input.js` to normalize against the video content box is a
  prerequisite for shipping this backend to phone clients.**
* **No portrait.** Every published mode is landscape.
* **60 Hz, scale 1.** Both fixed upstream of anything we control; no HiDPI.
* **Max 1920×1080**, and only the sizes on the ladder.
* **One display at a time.** macOS drives a single AirPlay display; a second
  ScreenExtend client cannot get its own. Enforced at the join page, at `/whep`
  and in the dashboard — see §12b.
* **Accessibility permission is mandatory**, not optional as on 10.15+.
* **Requires a logged-in, unlocked GUI session.** There is no menu bar to drive
  otherwise: with the screen locked, SystemUIServer reports *zero* accessibility
  extras and every AX route fails. `ScreenExtend serve` on a headless box cannot
  use this backend.
* **Back-to-back sessions are not perfectly deterministic on 10.15.8.** The first
  session is reliable across every geometry tested; an immediately following one
  sometimes reports that macOS accepted the session but attached no display.
  Detection now scans the *online* list and identifies the display by its
  `aapl`/`airp` vendor/model pair rather than diffing the active list, which
  fixed the common case (macOS reuses the same display id, and a mirrored
  display is not in the active list at all) — but it has not been made
  bulletproof.
* **macOS is not always prompt about dropping the display** once its session
  ends. Restoring the granted mode and calling
  `CGRestorePermanentDisplayConfiguration` before teardown makes removal
  immediate in most runs; when it does not, the display is inert and does not
  block the next session.
* **Session setup takes ~10 s**, against a synchronous API call on the
  `CGVirtualDisplay` path. The joining client waits that long.
* **The Mac keeps encoding H.264** for the whole session and we discard all of
  it. That is the cost of borrowing macOS's display machinery. `maxFPS` is
  advertised as 1 to keep it as cheap as possible.
* **`server.rs`'s in-place reconfigure branch needs routing** to
  destroy-and-recreate on this backend, or it will log success while changing
  nothing.
* **The mode-restore-before-teardown step is load-bearing.** Remove it and every
  session leaves a ghost display behind until the app quits.

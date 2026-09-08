# Virtual displays and the host-created Wi-Fi network

## 1. Question

Two questions, both about the machinery that sits *underneath* capture and transport.

**Part 1 — virtual displays.** On Windows: which indirect display driver (IDD) is actually
bundled, how it presents modes, and whether the IddCx model puts anything between DWM's
composition and our capture. Does the IDD's adapter appear in DXGI enumeration and can it
mislead encoder-adapter selection? Does the IDD's reported refresh rate set DWM's compose
cadence — and therefore our capture cadence? What do `IDD_CX_CLIENT_VERSION`, the IddCx 1.10 HDR
work and `IddCxMonitorSetupHardwareCursor` buy us? Can DWM be told not to vsync-wait on a virtual
monitor? On macOS: what `CGVirtualDisplay` really exposes, whether its refresh-rate field changes
WindowServer's compose cadence, who else uses it, and whether a virtual display costs latency
that a real one does not.

**Part 2 — the hosted network**, i.e. the host creating its own Wi-Fi so a client joins with no
router in the path. This is the reachable analogue of AWDL that
[`airplay-why-fast.md`](airplay-why-fast.md) §2.1 established we can never have directly. On
Windows: what survives of `netsh wlan hostednetwork`, whether `NetworkOperatorTetheringManager`
(Mobile Hotspot) is callable from an unpackaged desktop app, what Wi-Fi Direct legacy mode can and
cannot do, 5 GHz, and channel choice. On macOS: IBSS vs. the private HostAP SPI vs. Internet
Sharing, and what actually works per release. For both: how to *verify* the network came up and
how to report real status.

---

## 2. Findings

### 2.1 The bundled Windows driver is a locally built, rebranded `virtual-display-rs` fork

`src-tauri/resources/PROVENANCE.md:14-15` says the driver package "is published on the upstream
GitHub Releases". **It is not the upstream artifact.** `strings` on the shipped
`VirtualDisplayDriver.dll` yields:

```
Initialized Virtual Display Driver v1.0.0 @ eae5120
C:\Users\sarve\.cargo\registry\src\index.crates.io-6f17d22bba15001f\tokio-1.38.0\src\...
virtual-display-driver\src\callbacks.rs
ScreenExtend
\\.\pipe\virtualdisplaydriver
```

The `C:\Users\sarve\.cargo\...` paths are this dev box's Cargo registry: the DLL was compiled
locally. `eae5120` resolves to
[`eae5120a676831ebb96a78125f7e84532bcaf152`](https://github.com/MolotovCherry/virtual-display-rs/commit/eae5120a676831ebb96a78125f7e84532bcaf152)
(2024-07-08) in MolotovCherry/virtual-display-rs — so it is a fork of upstream at that commit,
rebranded (INF strings all say `ScreenExtend`, `DriverVer = 07/18/2024,1.0.0.0`) and signed with
the bundled `ScreenExtend.cer`. The IPC wire format in `driver_ipc/core.rs` is upstream's
`driver-ipc` crate verbatim (`Monitor { id, name, enabled, modes }`, `DriverCommand::{Notify,
Remove, RemoveAll}`, pipe `virtualdisplaydriver`), which is why the client code matches so exactly.

**The INF pins the class extension version:**

```
[VirtualDisplayDriver_Install]
UmdfLibraryVersion=2.15.0
UmdfExtensions = IddCx0102
UmdfHostProcessSharing = ProcessSharingDisabled
HKR, "WUDF", "DeviceGroupId", %REG_SZ%, "VirtualDisplayDriverGroup"
```

`IddCx0102` is **IddCx 1.2**, which shipped in Windows 10 1709 (build 16299)
([IddCx versions](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/iddcx-versions)).
Everything from IddCx 1.4 onward is therefore unavailable to this driver. That is the single most
consequential fact in this report; §2.3 and §2.4 are its downstream effects.

### 2.2 How the IddCx frame path actually works, and where our capture taps it

Microsoft's model
([Indirect Display Driver Model Overview](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview),
[IddCx Objects](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/iddcx-objects)):

- The IDD creates an `IDDCX_ADAPTER` and one `IDDCX_MONITOR` per virtual monitor. **The OS**
  creates the `IDDCX_SWAPCHAIN` and hands it to the driver via `EvtIddCxMonitorAssignSwapChain`.
- "The swapchain has multiple buffers to allow the OS to compose the next desktop image in one
  buffer while the IDD is accessing another buffer." The driver calls
  `IddCxSwapChainReleaseAndAcquireBuffer` to get the composed desktop as a DirectX surface, then
  `IddCxSwapChainFinishedProcessingFrame`.

The fork's consumer (`rust/virtual-display-driver/src/swap_chain_processor.rs`) is a **pure
sink**: it acquires a buffer and immediately calls `IddCxSwapChainFinishedProcessingFrame` without
reading a pixel. On `E_PENDING` it does `WaitForSingleObject(hNextSurfaceAvailable, 16)` and
retries. The thread is MMCSS-boosted (`AvSetMmThreadCharacteristicsW(L"Distribution")`).

**Where does our frame come from, and is there an extra copy?** No. DXGI Desktop Duplication and
WGC do not read the IddCx swapchain; they read DWM's composed output for that *output*, the same
composition DWM also presents into the swapchain. The IDD is a parallel consumer, not an upstream
stage. Because the fork finishes every frame immediately, it never back-pressures DWM, so it adds
no queueing delay to our capture either. **The IDD costs us ~zero latency; the interesting
question is not the swapchain, it is which GPU composes and at what cadence.**

**Does the IDD adapter appear in DXGI enumeration?** The IddCx 1.4 documentation answers this
directly, in the render-adapter table
([IddCx 1.4 updates for console and remote IDDs](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/iddcx1.4-updates-for-console-and-remote-idds)),
whose columns are "*Enum for DWM* = The GPU that the DX runtime enumerates the ID monitor on for
Desktop Window Manager" and "*Enum for App* = The GPU that the DX runtime enumerates the ID
monitor on for the application". So **the indirect monitor is enumerated as an `IDXGIOutput` of a
real GPU adapter**, not of a synthetic IDD adapter. Two consequences:

1. There is no phantom software adapter for `select_vendor` to trip over. Good.
2. **"Enum for DWM" and "Enum for App" can be different GPUs** on a hybrid machine. The IDD
   monitor can be enumerated on Intel for DWM and on NVIDIA for the app, or vice versa, depending
   on the IDD's preference and the user's Graphics Settings preference. That is precisely the
   cross-adapter case ScreenExtend's NVENC path already labels
   `"NVENC ZERO-COPY cross-adapter GPU path"` (`windows_utils/streamer/pipeline.rs:1360`).

**And here is the IddCx 1.2 cost.** From the same page: *"IddCx versions before IddCx 1.4 used the
power-on self-test (POST) adapter to render the desktop image passed to the IDD if it was not
PnpStopped. If the POST adapter was PnpStopped, the system-supplied Windows Advanced
Rasterization Platform (WARP) was used instead."* `IddCxAdapterSetRenderAdapter` — the only way to
choose — is IddCx 1.4. So on our IddCx 1.2 driver:

- On a hybrid laptop (the RTX 3080 Ti dev box), the POST adapter is normally the **iGPU**. DWM
  composes the virtual display on Intel; NVENC then encodes from a different adapter, paying a
  cross-adapter copy on every single frame — one that a real dGPU-attached monitor would not pay.
- On a machine whose POST adapter is PnpStopped, the desktop for our virtual display is composed
  by **WARP, in software, on the CPU.** That is a silent catastrophe with no log line anywhere.

### 2.3 The IDD mode model, and why "a live display cannot be given new modes" is not true

The fork reports modes twice: `EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION` builds `IDDCX_MONITOR_MODE`s
from the EDID it synthesises, and `EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES` builds matching
`IDDCX_TARGET_MODE`s. The OS reports the *intersection*. Both come from the same
`Vec<Mode>` the IPC delivered, so the intersection is the whole list.

The target mode's `vSyncFreq` is `{ Numerator: refresh_rate, Denominator: 1 }` — an exact integer
Hz. The monitor mode's is computed from a fabricated pixel clock
(`clock_rate = refresh*(h+4)*(h+4) + 1000`), landing within ~1 mHz of the same integer. There is
no fractional-rate support and no cap in the driver; upstream users run 120/144/240 Hz on it.

**Mode changes tear the monitor down.** `rust/virtual-display-driver/src/ipc.rs::notify()` is
explicit:

```rust
/// Note that updated monitors causes a detach, update, and reattach. (Required for windows to see the changes)
...
if modes_changed || !monitor.enabled {
    if let Some(mut obj) = mon.object.take() { IddCxMonitorDeparture(obj) }
}
```

i.e. a mode-list change is an unplug (`IddCxMonitorDeparture`) followed by
`IddCxMonitorCreate` + `IddCxMonitorArrival`. That is the entire reason ScreenExtend pre-registers
the whole DPR ladder at create time (`CLAUDE.md`: "pre-register every ladder mode at create time —
a live display cannot be given new ones").

**The parenthetical in that upstream comment is wrong.**
[`IddCxMonitorUpdateModes`](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxmonitorupdatemodes)
— *"A driver can call IddCxMonitorUpdateModes to update the mode list previously reported for a
monitor"* — has a minimum supported client of **Windows 10 / Windows Server 2016**, i.e. it is
available at IddCx 1.0, well below our 1.2. Upstream simply never used it. A one-function patch to
our fork (add an `UpdateModes` IPC command that calls `IddCxMonitorUpdateModes` instead of
departure/arrival) removes the pre-registration constraint entirely.

### 2.4 Refresh rate, DWM's compose cadence, and the vsync question

An indirect display has no hardware vblank. The cadence at which DWM composes for that monitor is
synthesised from the mode's `vSyncFreq`, and the OS signals `hNextSurfaceAvailable` when a frame
is ready. So **yes: the reported refresh rate is the compose cadence, and therefore the capture
cadence.** A virtual monitor at 60 Hz quantises every desktop change to a 16.67 ms grid before
Desktop Duplication can ever see it — a uniform 0…16.67 ms delay, mean ~8.3 ms. At 120 Hz that
becomes mean ~4.2 ms; at 240 Hz, ~2.1 ms.

**Can DWM be told not to vsync-wait on a virtual monitor?** No — there is no such IddCx knob, and
no documented DWM setting scoped to one output. The only lever is the reported rate. That lever is
free to us in a way it is not for a real monitor: nothing is physically scanning out, so raising
the rate costs only DWM compose work, not display bandwidth.

Critically, **the compose cadence and our encode rate are independent knobs that ScreenExtend
currently welds together.** `streamer/server.rs:799-843` sets `cfg.max_fps = detected_refresh` from
the client's reported refresh, then passes the same number as the display's refresh rate
(`:935-941`). A phone reporting 60 gets a 60 Hz virtual display *and* a 60 fps stream. Nothing
requires that: the display could be created at 120 Hz while the encoder still targets 60 fps, and
every frame we do send would be up to 8 ms fresher.

DWM only composes when something changes, so a static desktop produces no frames at any rate —
the cost is bounded by how much is actually moving.

### 2.5 `IddCxMonitorSetupHardwareCursor` and the other version-gated features

The fork **does** set up a hardware cursor
(`context.rs::setup_hw_cursor`, `IDDCX_CURSOR_CAPS { AlphaCursorSupport: TRUE, MaxX: 512, MaxY:
512, ColorXorCursorSupport: NONE }`), and the bundled DLL contains the corresponding strings
(`IddCxMonitorSetupHardwareCursor() failed:`, `... warn:`). When a hardware cursor is set up the
OS stops compositing the cursor into the desktop image for that monitor and instead signals
`hNewCursorDataAvailable`; the fork creates that event and **never reads it**. So the cursor is
neither in the swapchain nor delivered anywhere. Our DXGI path composites its own cursor from
`DXGI_OUTDUPL_FRAME_INFO.PointerPosition` (that is what `dxgi/`'s `CursorSprite`, `cursor_pos`,
`cursor_visible` fields are for) and is unaffected; the WGC path is the one that needs checking on
a virtual monitor, and nobody has.

Version-gated features we forfeit at IddCx 1.2, from the
[`IDDCX_ADAPTER_FLAGS`](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/ne-iddcx-iddcx_adapter_flags)
and version tables:

| Feature | IddCx | Windows | Relevance |
|---|---|---|---|
| `IddCxAdapterSetRenderAdapter` | 1.4 | 10 1903 | choose the GPU DWM composes our virtual display on (§2.2) |
| `IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE` | 1.4 | 10 1903 | forces a real mode change instead of DWM per-frame scaling |
| `EVT_IDD_CX_MONITOR_GET_PHYSICAL_SIZE` | 1.4 | 10 1903 | physical size without an EDID |
| `IDDCX_ADAPTER_FLAGS_PREFER_PRECISE_PRESENT_REGIONS` | 1.8 | 11 21H2 | tighter dirty rects, "small CPU usage overhead" |
| `IddCxSetRealtimeGPUPriority` | 1.9 | 11 22H2 | RT GPU priority for the IDD's device |
| HDR10 / SDR-WCG / FP16 swapchains | 1.10 | 11 23H2 | out of scope today (we encode 8-bit 709) |

The fork leaves `IDDCX_ADAPTER_CAPS.Flags` at its `Default::default()` (i.e.
`IDDCX_ADAPTER_FLAGS_NONE`), so `USE_SMALLEST_MODE` is unset even in principle. Per the docs that
means **OS virtual modes are active**: "mode changes can be performed seamlessly by OS using DWM
scaling on a per frame basis without any display mode change… the desktop image provided to the
driver is larger than the desktop size". For a sink driver that is harmless. For *us* it means a
resolution change on our virtual monitor may be DWM scaling rather than a real modeset — worth
knowing when reasoning about why the DPR ladder behaves as it does.

The INF's `DeviceGroupId = VirtualDisplayDriverGroup` is contrary to Microsoft's advice for
low-contention IDDs (*"it is highly recommended that remote IDDs set `UmdfHostProcessSharing` to
`ProcessSharingDisabled` and remove any `DeviceGroupId` directives"* — the first half is done, the
second is not). We are a console IDD, not a remote one, so this is advisory, not required.

### 2.6 macOS: what `CGVirtualDisplay` exposes and what we use of it

`macos_utils/virtual_display.rs:50-136` declares the private classes. The descriptor surface it
binds: `setName:`, `setMaxPixelsWide:`/`setMaxPixelsHigh:`, `setSizeInMillimeters:`, `setVendorID:`,
`setProductID:`, `setSerialNum:`, `setRedPrimary:`/`setGreenPrimary:`/`setBluePrimary:`/
`setWhitePoint:`, `setQueue:`. Settings: `setModes:`, `setHiDPI:`. Mode:
`initWithWidth:height:refreshRate:` (refresh is a `double`, so fractional rates are expressible —
unlike Windows). Display: `initWithDescriptor:`, `applySettings:`, `displayID`.

Two things stand out.

**`setTerminationHandler:` is never bound or set,** even though `setQueue:` *is*
(`:172`, a global high-priority dispatch queue — which exists precisely to run that handler). The
descriptor's queue is therefore configured for a callback we never register: if WindowServer tears
the display down under us, we learn nothing, and the `registry()` map keeps a stale
`Retained<CGVirtualDisplay>` for a display that no longer exists.

**`applySettings:` re-registers the whole mode list on a live display.** `reconfigure_display`
(`:381-433`) calls `make_settings(...)` with one mode and applies it to an existing display, then
`activate_mode` polls `CGDisplayCopyAllDisplayModes` for up to 1.5 s waiting for it to appear.
This is the opposite of the Windows constraint: on macOS the ladder is *not* needed, but only one
mode is ever registered at a time, so every geometry change is a WindowServer round trip with a
polling settle (already filed as
`findings/major-vdisplay-mac-activate-mode-can-block-three-seconds-on-the-join-path.md` and
`findings/major-vdisplay-mac-discards-the-dpr-mode-ladder-and-always-creates-at-1x.md`).

**Does the refresh-rate field change WindowServer's compose cadence?** There is no Apple
documentation — the API is private — and the honest answer is *unverified*. What is known: the
value is a `double` accepted at mode construction and surfaces through
`CGDisplayModeGetRefreshRate`, and macOS's per-display compositing is driven by each display's
`CVDisplayLink`/vsync source, which for a virtual display is synthesised. The structural argument
from Windows (§2.4) should hold — a synthesised vblank at 120 Hz halves the quantisation — but on
macOS `create_display` hardcodes `60.0` when the client reports 0 and otherwise passes the client's
rate, so we have never *tried* decoupling it. This is the cheapest untested experiment in this
report and it needs a Mac newer than the 10.15 box only if we want to test >60.

**Who else uses this.** `CGVirtualDisplay*` is the same private CoreGraphics API behind
[`deskpad`](https://github.com/Stengo/DeskPad), [BetterDisplay](https://github.com/waydabber/BetterDisplay)
and FluffyDisplay; it predates Sidecar (the classes exist from 10.14.4, which is why
`probe_display_backend()` correctly probes by `AnyClass::get` rather than by version —
`virtual_display.rs:334-361`). Sidecar itself and Luna Display do *not* go through it: Sidecar is
an in-WindowServer path, and Luna ships a hardware dongle that presents as a real DisplayPort sink,
which is why Luna's own marketing claims lower latency than software-only competitors. Duet
historically used a virtual display + USB transport. The code already notes the alternative C
entry point `CDVirtualDisplayCreate` in CoreDisplay (`:342-345`) and logs when it is present but
`CGVirtualDisplay` is not — a good instinct, currently only a log line.

**Latency of a virtual vs. a real display in capture.** No published measurement exists for
CGDisplayStream or SCK on a `CGVirtualDisplay`. Structurally the composition work is identical;
what differs is the vsync source. The measured macOS capture costs we already know about
(`kCGDisplayStreamMinimumFrameTime` hold-off, ~8 ms mean at 60 fps, and the queue-depth starvation
— see `airplay-why-fast.md` R3 and `findings/blocker-cgds-queue-depth-starvation.md`) dominate
anything the virtual display itself could plausibly add.

### 2.7 Windows hosted network: three mechanisms, and which one we are on

**(a) `netsh wlan hostednetwork` (SoftAP, the legacy path).** Deprecated by Microsoft; most
drivers since Windows 10 1607 report `Hosted network supported : No` because the WLAN Autoconfig
virtual-adapter model that backed it was superseded by Wi-Fi Direct. It is what
`supports_legacy_hosted_network_` probes for by string-matching `netsh wlan show drivers`
(`hosted_network.rs:124-138`). On modern hardware this probe overwhelmingly returns false.

**(b) Wi-Fi Direct legacy AP (`WiFiDirectAdvertisementPublisher` + `LegacySettings`).** This is
what ScreenExtend actually runs. It is a genuine soft AP that non-Wi-Fi-Direct clients (phones,
tablets) can associate to, with an SSID and a WPA2 passphrase, and Windows serves DHCP on the
"Microsoft Wi-Fi Direct Virtual Adapter" — which is exactly the adapter `networking.rs:107-113`
special-cases. Two documented limits matter:

- *"The connection does not serve as a general-purpose Wi-Fi access point"*
  ([WiFiDirectLegacySettings](https://learn.microsoft.com/en-us/uwp/api/windows.devices.wifidirect.wifidirectlegacysettings)).
  It is a link for reaching *this* device, which is exactly our use case.
- **No band or channel control is exposed at all.** Wi-Fi Direct GO negotiation in legacy mode
  typically lands on 2.4 GHz for client compatibility. For a 1080p60 H.264 stream that is the
  wrong band: crowded, and HT20-limited.
- `WiFiDirectLegacySettings` lists **no required app capability**, which is very likely why this
  path works at all from our unpackaged Tauri binary.

**(c) Mobile Hotspot (`NetworkOperatorTetheringManager`).** The modern replacement, and the only
one with band control:
[`TetheringWiFiBand`](https://learn.microsoft.com/en-us/uwp/api/windows.networking.networkoperators.tetheringwifiband)
= `Auto(0) | TwoPointFourGigahertz(1) | FiveGigahertz(2) | SixGigahertz(3)`, introduced in
Windows 10 2004 (19041) alongside `IsBandSupported`/`IsBandSupportedAsync` on
[`NetworkOperatorTetheringAccessPointConfiguration`](https://learn.microsoft.com/en-us/uwp/api/windows.networking.networkoperators.networkoperatortetheringaccesspointconfiguration)
(which also carries `Ssid`, `Passphrase` — 8–63 ASCII per 802.11 — and `AuthenticationKind`).
`StartTetheringAsync(NetworkOperatorTetheringSessionAccessPointConfiguration)` starts a session
with a per-session config, so nothing persistent has to be written.

The catch is the capability. From
[`CreateFromConnectionProfile`](https://learn.microsoft.com/en-us/uwp/api/windows.networking.networkoperators.networkoperatortetheringmanager.createfromconnectionprofile):

> *"In order for the call to succeed, however, the app must have the appropriate Wi-Fi control
> device capability declared in its manifest… `<DeviceCapability Name="wiFiControl"/>` … If the
> **wiFiControl** device capability is not set in the manifest, then calls to
> CreateFromConnectionProfile fail by throwing an exception."*
>
> *"Your code can determine whether the capability is set at runtime by calling
> `GetTetheringCapabilityFromConnectionProfile`. If **DisabledBySystemCapability** is returned,
> then the manifest does not set the required device capability."*

`TetheringCapability` is `Enabled(0) | DisabledByGroupPolicy(1) | DisabledByHardwareLimitation(2) |
DisabledByOperator(3) | DisabledBySku(4) | DisabledByRequiredAppNotInstalled(5) |
DisabledDueToUnknownCause(6) | DisabledBySystemCapability(7)` — a ready-made, precise runtime
probe that ScreenExtend never calls.

And the two mechanisms are mutually exclusive: *"The Mobile Hotspot and Wi-Fi Direct technologies
cannot run at the same time. Mobile Hotspot takes precedence over all Wi-Fi Direct scenarios."*

**What ScreenExtend's code does with all this is a near miss.** `start_wifi_direct_`
(`hosted_network.rs:21-74`) opens with:

```rust
let connection_profile = NetworkInformation::GetInternetConnectionProfile()?;
let tethering_manager = NetworkOperatorTetheringManager::CreateFromConnectionProfile(&connection_profile)?;
let initial_state = tethering_manager.TetheringOperationalState()?;
if initial_state != windows::Networking::NetworkOperators::TetheringOperationalState(2) { ...bail... }
```

It builds a `NetworkOperatorTetheringManager` **purely to read one property, then discards it** and
starts Wi-Fi Direct instead. Three problems fall out:

1. `GetInternetConnectionProfile()` returns the *internet* profile. On a host with **no internet
   connection** — the flagship reason to create a hosted network in the first place, and the
   whole point of the offline demo — it fails, `?` propagates, and the hosted network never
   starts. The feature is disabled exactly when it is most needed.
2. If `wiFiControl` is genuinely enforced for unpackaged desktop apps, `CreateFromConnectionProfile`
   throws and Wi-Fi Direct is dead on every machine, for a reason that has nothing to do with
   Wi-Fi Direct. This must be tested; it is a five-minute experiment on the Windows box.
3. `TetheringOperationalState(2)` is a magic literal. `2` is `Off`, so the intent — "refuse if the
   Mobile Hotspot is already running, because it would win" — is correct, but it reads as a bug
   and would be one after any enum reordering.

Meanwhile the *capability* probe in `compatibility.rs:79-84` only asks whether the WinRT
**types are present** (`type_present("…NetworkOperatorTetheringManager")`), which is true on every
Windows 10 build and says nothing about whether the call will succeed. That is the tier-doctrine
defect: presence, not capability.

Finally, `supports_legacy_hosted_network_` spawns `netsh wlan show drivers` on **every**
`start`/`stop`/`is_hosted_network` call, and `is_hosted_network` is reached from
`get_network_adapters`, which the 1 Hz watcher calls forever
(`networking.rs:110`, `:228-236`) — the process-per-second finding already recorded in
[`docs/discovery/G-windows-utils.md`](../discovery/G-windows-utils.md). The result of that probe
is a static property of the driver and should be computed once.

### 2.8 macOS hosted network: HostAP (private), IBSS (deprecated and crippled), Internet Sharing

ScreenExtend does **not** use `startIBSSModeWithSSID:` — despite the brief's framing and the dead
`hostap.m` in the tree. It calls the private
`startHostAPModeWithSSID:securityType:channel:password:error:`
(`macos_utils/hosted_network.rs:76-85`) via `msg_send!`.

**IBSS, for the record, is not the alternative.** In the current CoreWLAN headers
`startIBSSModeWithSSID:security:channel:password:error:` is marked `#[deprecated]` outright, and
its security enum `CWIBSSModeSecurity` has exactly three members: `None = 0`, `WEP40 = 1`,
`WEP104 = 2` (verified against the generated `objc2-core-wlan` bindings on docs.rs). So IBSS is
**open or WEP, nothing else**, and Apple's IBSS is 802.11g-rate 2.4 GHz — a ~54 Mbit/s PHY, which
after 802.11g overhead and a shared ad-hoc medium realistically yields ~20 Mbit/s of TCP goodput
and degrades hard with distance. That is a hard ceiling below comfortable 1080p60. Separately,
Apple removed "Create Network…" from the Wi-Fi menu on Apple Silicon and in the redesigned Ventura
menu; IBSS is not a path forward on any current Mac.

**The HostAP SPI, by contrast, is alive.** Class dumps of CoreWLAN show it present through at
least version 1375.2 (Monterey era):

```objc
- (void)stopHostAPMode;
- (BOOL)startHostAPMode:(out id *)arg1;
- (BOOL)startHostAPModeWithSSID:(id)arg1 securityType:(unsigned long long)arg2 channel:(id)arg3 password:(id)arg4 error:(out id *)arg5;
- (void)disableHostAPMode;
- (void)enableHostAPMode;
```

alongside the public `CWInterfaceMode.HostAP` that `is_hosted_network` already reads
(`hosted_network.rs:213-215`), which is a genuinely good liveness check — better than anything the
Windows side has. Note `startHostAPMode:` (no SSID) also exists: it presumably starts the AP from
the Internet Sharing preferences rather than from arguments, which is a fallback worth probing.

The security types in our code, `SECURITY_OPEN = 2` and `SECURITY_WEP = 16`, are magic numbers
inherited from a 2012 gist (`hostap.m:82-93`: *"Note: values [-127..127] have been tried, and all
but these return errors"*) and are **not** `CWIBSSModeSecurity` values — different enum, different
API. WEP is the strongest thing we currently offer, and modern iOS and Android will either refuse
it or warn loudly; this is already filed as
`findings/major-hostap-offers-only-wep-or-a-silently-open-network.md`. The value space between 2
and 16 has never been swept for a WPA2 constant on a modern macOS, and that sweep is cheap.

**`hostap.m` is dead code.** `src-tauri/build.rs` is three lines (`tauri_build::build()`), there is
no `cc` invocation and no reference to the file anywhere — and the file contains hardcoded
credentials (`@"Test"` / `@"barbarbarr"`) that override its own arguments. Already filed as
`findings/minor-mac-hostap-m-is-dead-code-with-hardcoded-credentials.md`.

**Internet Sharing** (`/usr/libexec/InternetSharing`, `com.apple.nat` preferences,
`launchctl … com.apple.InternetSharing`) is the supported route to the same radio state, and is
what puts the interface into `kCWInterfaceModeHostAP` when a user enables it in System Settings.
Driving it programmatically means writing
`/Library/Preferences/SystemConfiguration/com.apple.nat.plist` and kicking the daemon, both of
which require root, and the SSID password lives in the System keychain rather than in that plist.
Critically for us: **Internet Sharing is defined as sharing *from* an upstream interface** — its
preference schema names a primary/upstream interface and a set of sharing devices — so it is not
the mechanism for "no upstream at all, just make an AP". The private HostAP SPI, which takes SSID
and channel directly and needs no upstream, remains the only in-process way to get a host AP with
no router and no internet. That asymmetry is the single most important difference between the two
platforms here: on macOS we can make an AP with no upstream; on Windows our current code refuses to
even try without one (§2.7).

### 2.9 Verifying that a hosted network actually came up — neither platform does

Right now "started" means:

- Windows Wi-Fi Direct: `WiFiDirectAdvertisementPublisherStatus::Started` arrived on the callback
  (`hosted_network.rs:50-52`), after which `to_return = true` unconditionally (`:196`).
- Windows legacy: `netsh wlan show hostednetwork` stdout contains `Status` … `: Started` — English
  only, already filed (`findings/minor-compat-win-hosted-network-probe-is-english-only.md`).
- macOS: the private selector returned `YES` (`:87`).

None of these says a client can associate, that DHCP is being served, or that the host has an IP
on that adapter. All three are observable, cheaply, from data ScreenExtend already collects:

- **Host has an IP on the AP adapter.** `get_network_adapters` already enumerates
  `MSFT_NetIPAddress` per `InterfaceIndex` and already identifies the Wi-Fi Direct adapter by
  `DriverDescription == "Microsoft Wi-Fi Direct Virtual Adapter"` (`networking.rs:107-128`).
  A hotspot that is up has a link-local-ish `192.168.137.x`; one that is up but broken has no IPv4
  at all. That is a one-line assertion on data already in hand. On macOS the equivalent is a
  `192.168.2.x` on the Wi-Fi interface with `interfaceMode() == HostAP`.
- **A client associated.** On the Mobile Hotspot path, `NetworkOperatorTetheringManager` exposes
  `ClientCount`, `MaxClientCount` and `GetTetheringClients()` directly — real, first-class status
  we could surface instead of a bool. Wi-Fi Direct has no equivalent; the proxy is an ARP-table or
  neighbour-cache entry on that interface, or simply "a WHEP join arrived from a `192.168.137.x`
  peer", which the streamer already knows because sessions are keyed by client IP.
- **DHCP served.** Implied by the client's source address being in the AP subnet rather than
  169.254/16. Again free at join time.

The right shape is a tri-state — `NotStarted | Advertised | Verified(peer)` — rather than a bool,
promoted to `Verified` by the first successful join from the AP subnet, with `Advertised` timing
out into a diagnostic after ~60 s. Retry belongs in the same state machine: on macOS the code
already retries once on channel 11 after the 5 GHz attempt (`hosted_network.rs:161-190`); Windows
does not retry at all, and its single `success_rx.recv()` blocks forever if the publisher never
reaches `Started` or `Aborted`, because the local `success_tx` clone is kept alive
(`:174-175`; filed as `findings/minor-hosted-network-unbounded-recv-and-unwrap-panic.md`).

**Channels.** [`airplay-why-fast.md`](airplay-why-fast.md) §R4 already establishes the AWDL
social-channel argument (6, 44, 149) and that `PREFERRED_5GHZ_CHANNELS = [36, 40, 44, 48]`
contains one of them. The Windows finding here is complementary and new: Wi-Fi Direct legacy mode
gives us **no channel or band control at all**, so the only way to get Windows onto 5 GHz — and
therefore off channels 6 and 44 by construction — is the Mobile Hotspot path with
`TetheringWiFiBand::FiveGigahertz`. `SixGigahertz` is better still where the adapter and the client
both support Wi-Fi 6E: AWDL does not operate there, so the social-channel problem disappears
entirely rather than being dodged.

---

## 3. How others do it

| Project | Virtual display | Hosted network |
|---|---|---|
| **Sunshine** (LizardByte) | Does not ship a driver; documents pairing with `VirtualDrivers/Virtual-Display-Driver` (the `itsmikethetech` fork of the same upstream we forked). Captures with DD/WGC exactly as we do. Known community issue: a 120 Hz VDD delivering ~30 fps to Moonlight, i.e. the compose-cadence/encode-rate coupling of §2.4 is a real, observed failure mode in this ecosystem. | n/a — assumes a LAN. |
| **`VirtualDrivers/Virtual-Display-Driver`** | The most-developed fork of our upstream: XML/registry-driven mode config, advertised refresh rates 60/75/90/120/144/165/240/480/500 Hz, and a "1.5" line adding 8-bit SDR + hardware cursor. Same IddCx model, same `Notify`-rebuilds-the-monitor limitation. | n/a |
| **`MolotovCherry/virtual-display-rs`** (our base) | `IddCx0102`, `MAX_MONITORS = 16`, EDID serial number carries the monitor id, hardware cursor set up but never read, swapchain drained and discarded, mode change = departure+arrival. | n/a |
| **Parsec VDD** | Ships its own signed IDD. Not open source. Presents a fixed ladder up to 4K/240 and is widely reported to work when other VDDs stutter, consistent with a newer IddCx target. | n/a |
| **spacedesk** | IDD on the host, browser/native client — architecturally the closest product to ScreenExtend. Closed source. | n/a |
| **Duet Display** | Virtual display + USB transport historically; USB sidesteps the whole radio question. | n/a — USB is the answer. |
| **deskpad / BetterDisplay / FluffyDisplay** | All three drive `CGVirtualDisplayDescriptor`/`Settings`/`Mode` exactly as `macos_utils/virtual_display.rs` does. BetterDisplay is the most complete reference for what the private surface accepts, including HiDPI behaviour. | n/a |
| **Sidecar** | Not a `CGVirtualDisplay` consumer — in-WindowServer, over AWDL. Unreachable to third parties (`airplay-why-fast.md` §2.1). | AWDL, which is itself the one-hop network. |
| **Luna Display** | Hardware dongle presenting as a real DP/HDMI sink, so no virtual display at all. This is why its latency claims beat software-only rivals. | n/a |

The pattern worth taking: **nobody in the Windows IDD ecosystem uses `IddCxMonitorUpdateModes`,
and nobody decouples the virtual display's refresh rate from the stream's frame rate.** Both are
available to us today and both are cheap.

---

## 4. What ScreenExtend does today

**Windows virtual display** (`windows_utils/virtual_display.rs`)

- `WindowsVirtualDisplay::new_shared` (`:33-43`) connects `DriverClient`, calls `remove_all()`,
  discards the notify result (`:36`, `let _ =`), and discards the *reason* a connect failed
  (`:34`, `DriverClient::new().ok()?` — "driver not installed" and "driver installed but wedged"
  become the same `None`).
- `create_display_with_modes` (`:57-120`) builds the dimension list from the requested geometry
  plus `extra_modes` **plus each transpose**, and attaches refresh rates
  `[requested, 60, 120]` (`COMMON_REFRESH_RATES` at `:7`) to *every* dimension. So a display
  requested at 60 Hz already advertises 120 Hz — we simply never select it.
- `Inner::commit` (`:16-24`) calls `set_monitors` then `notify` under a `std::sync::Mutex` held
  across `RUNTIME.block_on` in the sync shim (`driver_ipc/sync.rs:10-16`, a one-worker runtime).
- Removal (`:122-129`) and `remove_all` (`:131-138`) log failures via `teprintln!`.

**Windows capture-side adapter handling** is already correct: `dxgi/mod.rs::find_output`
(`:73-108`) walks `EnumAdapters1` × `EnumOutputs` to find which adapter owns the virtual monitor's
`DeviceName`, logs it (`"dxgi: output {} found on adapter '{}'"`), and `create_device_on` builds
the D3D11 device on *that* adapter. `select_vendor` (`streamer/mod.rs:23-34`) classifies by
description substring. So we already discover the composing adapter — we have simply never used
that knowledge to notice it is the wrong one.

**Join path** (`streamer/server.rs`) sets `cfg.max_fps = detected_refresh` from the client
(`:799-806`), lets a device override replace it (`:828`), then uses the *same* number as the
virtual display's refresh rate (`:839-843`, `:935-941`), inside `DISPLAY_CORRELATION_LOCK`.

**macOS virtual display** (`macos_utils/virtual_display.rs`) — `create_cg_virtual_display`
(`:162-228`) always `setHiDPI(0)` and always registers exactly one mode; `setQueue:` is set but no
termination handler; serial number auto-increments, product/vendor IDs are fixed `0x1234`/`0x3456`,
physical size is derived from a hardcoded 96 DPI (`:24`, `:194-197`). Backend selection probes by
class lookup (`:334-361`) — correct per the tier doctrine.

**Windows hosted network** (`windows_utils/hosted_network.rs`) — as dissected in §2.7. Note also
that the legacy branch shells out to an **elevated self-invocation** (`:151-159`,
`ScreenExtend hostednetwork <ssid> <password>`), putting the passphrase on a command line.

**macOS hosted network** (`macos_utils/hosted_network.rs`) — private HostAP SPI, 5 GHz channels
`[36, 40, 44, 48]` preferred then channel 11, secured attempt then a **silent** open-network
fallback (`try_start_on_channel:108-116`, mitigated only by the `HostedNetworkNoPassword` event),
liveness via `interfaceMode() == HostAP`.

**Network enumeration** (`windows_utils/networking.rs`) — three WMI queries at 1 Hz forever
(`:214-237`), each iteration reaching `is_hosted_network` → `netsh wlan show drivers`.

---

## 5. Recommendations

Ranked by expected latency/compatibility delta ÷ implementation cost.

---

### R1 — Decouple the virtual display's refresh rate from the stream's frame rate

**The single cheapest latency win in this report.** The IDD's reported refresh rate is DWM's
compose cadence for that monitor (§2.4). Today it is welded to `cfg.max_fps`, so a 60 Hz client
gets a 60 Hz compose grid and every desktop change waits a mean 8.3 ms before Desktop Duplication
can see it.

- **What to change:** in `streamer/server.rs`, pass a *display* refresh rate that is
  `max(client_refresh, 120)` (clamped to `MAX_REFRESH_RATE`) to `create_display_with_modes`
  (`:935-941`), while leaving `cfg.max_fps` — the encoder's target — exactly as it is. On Windows
  this needs no driver change at all: `COMMON_REFRESH_RATES` already registers 120 Hz on every
  mode (`windows_utils/virtual_display.rs:7`), so 120 is already in the mode list; we just never
  ask for it.
- **Platform/version:** Windows 10+, any IddCx. macOS: the same idea via
  `CGVirtualDisplayMode initWithWidth:height:refreshRate:` — but unverified there (§2.6), so gate
  it behind a probe that reads back `CGDisplayModeGetRefreshRate` and falls back to 60 if the OS
  did not honour it.
- **Probe:** after creation, read the mode back (`EnumDisplaySettings` / `CGDisplayCopyDisplayMode`)
  and log the achieved rate. If it is not what we asked for, log and continue — never fail a join.
- **Expected win:** ~4 ms mean, ~8 ms worst case at 120 Hz vs 60 Hz; ~6 ms mean / ~14 ms worst at
  240 Hz. Independent of, and additive to, every encoder-side improvement.
- **Cost:** DWM composes the virtual display more often. Bounded by actual screen change; a static
  desktop composes nothing. Watch host GPU utilisation with several clients.
- **Measure:** the existing `capture→encoded latency ms (mean/p50/p99/max)` harness in
  `pipeline.rs`, at display 60 vs 120 vs 240 with `max_fps` pinned to 60 throughout, plus host GPU
  busy % from `PresentMon` or Task Manager.

Findings file: `findings/major-vdisplay-decouple-idd-refresh-from-stream-fps.md`

---

### R2 — Rebuild the fork against IddCx 1.4+ and call `IddCxAdapterSetRenderAdapter`

At `IddCx0102` we cannot choose which GPU DWM composes our virtual display on, so it is the POST
adapter — the iGPU on most laptops — or **WARP, in software**, if the POST adapter is PnpStopped
(§2.2). On a hybrid machine that means an unavoidable cross-adapter copy per frame into NVENC; in
the WARP case it means the desktop for our virtual display is rasterised on the CPU and nothing
logs it.

- **What to change:** in the fork, set `UmdfExtensions = IddCx0104` (or higher) in the INF, follow
  [Building IddCx 1.4 drivers](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/building-iddcx1.4-drivers)
  so one binary still loads on 1.3+ hosts, and call `IddCxAdapterSetRenderAdapter` with the LUID
  the host process nominates. Extend the `driver_ipc` protocol with a `SetRenderAdapter(Luid)`
  command; the host already knows the encoder's adapter (`nvidia/encoder.rs:855-859`,
  `intel/encoder.rs:933-937` both enumerate it).
- **Platform/version:** `IddCxAdapterSetRenderAdapter` needs IddCx 1.4 = Windows 10 1903. Our
  stated floor is "Windows 10 2020-era" (2004 = 19041 = IddCx 1.5), so this costs us nothing.
- **Probe:** the driver uses `IDD_IS_FUNCTION_AVAILABLE(IddCxAdapterSetRenderAdapter)`; the host
  reads back the composing adapter it already discovers in `dxgi/mod.rs::find_output` and logs a
  warning when it differs from the encode adapter.
- **Expected win:** removes one full-frame cross-adapter copy per frame on hybrid hosts — at
  1920×1080 BGRA that is ~8.3 MB over PCIe per frame, roughly 1–3 ms at 60 fps plus its
  synchronisation. Eliminates the silent-WARP class outright.
- **Measure:** log the composing adapter and the encode adapter on every join; A/B the
  `pipeline.rs` capture→encode harness on the hybrid dev box with the render adapter forced to
  Intel vs NVIDIA.

Findings files:
`findings/major-vdisplay-win-idd-declares-iddcx-1-2-and-cannot-choose-the-render-adapter.md`,
`findings/major-vdisplay-win-warp-composition-is-silent.md`

---

### R3 — Replace the Windows hotspot's internet-profile dependency, and probe tethering properly

`GetInternetConnectionProfile()` at `hosted_network.rs:26` makes the hosted network **fail when
the host has no internet**, which is the primary scenario the feature exists for. And the
`NetworkOperatorTetheringManager` it builds is used for one property read and then thrown away.

- **What to change, in order:**
  1. Call `GetTetheringCapabilityFromConnectionProfile` (or `GetTetheringCapability`) *first* and
     surface the `TetheringCapability` value verbatim in the log bus and in
     `CompatibilityReport`. `DisabledBySystemCapability` tells us definitively whether the
     `wiFiControl` manifest capability blocks us as an unpackaged app; the other seven values are
     all actionable diagnostics we currently render as `false`.
  2. If tethering is `Enabled`, prefer it: `ConfigureAccessPointAsync` / the per-session
     `StartTetheringAsync(NetworkOperatorTetheringSessionAccessPointConfiguration)` overload with
     our SSID, an 8–63-char ASCII passphrase, and `Band = FiveGigahertz` (falling back to `Auto`
     when `IsBandSupported` says no, and trying `SixGigahertz` first where supported).
  3. Otherwise fall back to Wi-Fi Direct legacy mode as today — but reach it **without**
     `GetInternetConnectionProfile`. Wi-Fi Direct needs no connection profile; only the discarded
     tethering probe did. Guard the mutual exclusion by reading
     `TetheringOperationalState` through a *named* enum, not the literal `2`.
- **Platform/version:** `Band` and `IsBandSupported` need Windows 10 2004 (19041) — our floor.
- **Expected win:** compatibility, not milliseconds — the hotspot working at all offline. Plus the
  5 GHz move, which for a 1080p60 stream is the difference between a congested HT20 2.4 GHz channel
  and clean VHT80 spectrum, and which by construction keeps us off AWDL social channels 6 and 44
  (`airplay-why-fast.md` R4). 6 GHz removes the AWDL question entirely.
- **Measure:** `iperf3` between host and client over the AP, 2.4 vs 5 vs 6 GHz; the same
  RTT-histogram protocol as R4 in `airplay-why-fast.md`.

Findings files:
`findings/blocker-hostednetwork-win-requires-an-internet-connection-profile.md`,
`findings/major-hostednetwork-win-never-uses-mobile-hotspot-so-no-5ghz-or-band-control.md`,
`findings/major-compat-win-tethering-probe-checks-type-presence-not-capability.md`

---

### R4 — Report a *verified* hosted network, not an advertised one

All three "started" checks are assertions about our own API call, not about the radio (§2.9).

- **What to change:** make hosted-network state a tri-state
  `NotStarted | Advertised | Verified { peer_ip, since }` in `AppState`, promoted to `Verified` by
  the first WHEP join whose client IP is inside the AP subnet — a fact the streamer already has,
  since sessions are keyed by client IP (`streamer/session.rs`). Assert the host's own IP on the AP
  adapter from data `get_network_adapters` already collects
  (`networking.rs:107-128`); on the Mobile Hotspot path also surface `ClientCount` and
  `GetTetheringClients()`, which are first-class WinRT status. Time `Advertised` out into a
  diagnostic after ~60 s rather than leaving a green light on a dead AP.
- **Retry:** on Windows, bound the `success_rx.recv()` with a timeout and drop the local sender
  clone; retry once with `Band = Auto` after a `FiveGigahertz` failure. macOS already retries on
  channel 11 — extend it to try a non-social 5 GHz channel (`36/40/48`, then `149`-avoiding UNII-3)
  before dropping to 2.4 GHz.
- **Expected win:** compatibility and support burden. Today a hotspot that comes up without DHCP
  reports success and the user sees an unexplained failure to join.
- **Measure:** kill DHCP / block association deliberately and confirm the UI and `ScreenExtend
  status --json` say `Advertised`, never `Verified`.

Findings file: `findings/major-hostednetwork-no-verification-that-clients-can-associate.md`

---

### R5 — Add `IddCxMonitorUpdateModes` to the fork and stop unplugging live displays

`notify()` with a changed mode list does `IddCxMonitorDeparture` → `MonitorCreate` →
`MonitorArrival` (§2.3). Every DPR or orientation change that is not already in the pre-registered
ladder is a monitor hot-unplug: Windows reflows every window on the desktop, the client sees a
black frame, and our capture session must be rebuilt.

- **What to change:** add an `UpdateModes { id, modes }` command to `driver_ipc` and implement it
  with `IddCxMonitorUpdateModes` (Windows 10 / Server 2016 minimum — available at our IddCx 1.2).
  Keep `Notify`'s departure/arrival path for enable/disable and for adds/removes.
- **Expected win:** not a per-frame latency number — a step-change removal of a multi-hundred-ms
  reconfiguration stall on every out-of-ladder geometry change, and the retirement of the whole
  "pre-register the ladder at create time" doctrine (which also shrinks the mode list the OS has to
  intersect on every query).
- **Measure:** time a scale change end to end (client sends `/reconfig` → first frame at the new
  geometry) before and after; count `WM_DISPLAYCHANGE` events on the host.

Findings file: `findings/major-vdisplay-win-mode-change-unplugs-the-monitor.md`

---

### R6 — Set a `CGVirtualDisplay` termination handler on macOS

The descriptor's `queue` is configured (`virtual_display.rs:172`) for a `terminationHandler` we
never install, so a WindowServer-initiated teardown is invisible: `registry()` keeps a stale
`Retained<CGVirtualDisplay>`, `remove_display` later reports success, and the session hangs on a
display that is gone.

- **What to change:** bind `setTerminationHandler:` (it takes a `dispatch_block_t`) in the
  `extern_methods!` block and install a handler that removes the id from `registry()` and kicks the
  session's kick epoch. Guard with `respondsToSelector:` — this is a private API and the selector
  may not exist on every release.
- **Expected win:** correctness, and it closes the gap left open by
  `findings/speculative-vdisplay-mac-teardown-relies-on-dropping-the-last-retained-reference.md`.
- **Measure:** force a teardown (log out and back in, or hot-plug a real display while a virtual
  one is active) and confirm the handler fires and the session is cleaned up.

Findings file: `findings/major-vdisplay-mac-no-termination-handler.md`

---

### R7 — Fix the driver provenance record

`PROVENANCE.md` claims the bundled driver comes from upstream GitHub Releases; it is a locally
built, rebranded fork of `virtual-display-rs` at commit `eae5120a6768…` (§2.1), self-signed with
`ScreenExtend.cer`. Nobody but this dev box can reproduce the binary today, and R2/R5 both require
rebuilding it.

- **What to change:** record the fork URL and commit, the WDK/EWDK version, the exact build
  command, and the signing procedure; pin the upstream base commit; note the IddCx level so a
  future reader does not have to `strings` the DLL. Publish the fork so the build is reproducible.
- **Expected win:** none in milliseconds; R2 and R5 are blocked on it in practice.

Findings file: `findings/major-build-bundled-idd-driver-provenance-is-wrong.md`

---

### R8 — Cache the `netsh wlan show drivers` result

`supports_legacy_hosted_network_` spawns a process on every hosted-network query, and
`get_network_adapters` calls `is_hosted_network` at 1 Hz forever. The answer is a static property
of the installed WLAN driver.

- **What to change:** compute once into a `OnceLock<bool>`, invalidated only on a `NetworkChange`
  that adds or removes a Wi-Fi adapter. While in there, replace the English `"Hosted network
  supported"` string match with the localised-safe `WlanQueryInterface` /
  `wlan_intf_opcode_supported_...` route, or accept the string match and record the limitation
  (already filed as `findings/minor-compat-win-hosted-network-probe-is-english-only.md`).
- **Expected win:** removes 86 400 process spawns per day from a long-running host. Not a
  glass-to-glass number, but it is CPU and scheduler noise on the same box that is encoding.

Findings file: `findings/minor-hostednetwork-win-netsh-probe-spawned-per-query.md`

---

### R9 — Verify what happens to the cursor on a virtual monitor under WGC

The IDD sets up a hardware cursor, so the OS does not composite the cursor into the desktop image
for that monitor (§2.5). Our DXGI path composites its own from `PointerPosition` and is fine. The
WGC path has never been checked on a virtual monitor.

- **What to check:** capture our virtual display with WGC and confirm the cursor appears. If it
  does not, either force the DXGI path for IDD monitors or composite the cursor ourselves in the
  WGC path too.
- **Cost:** ten minutes on the Windows box. This is a correctness bug or it is nothing.

Findings file: `findings/minor-vdisplay-win-hardware-cursor-vs-wgc-capture-unverified.md`

---

### R10 — Sweep the macOS HostAP security-type space for a WPA2 constant

`SECURITY_OPEN = 2` / `SECURITY_WEP = 16` are magic numbers from a 2012 gist whose own comment says
the space was swept on macOS 10.8. Fourteen years of CoreWLAN later, a WPA2 or WPA3 constant may
well exist, and WEP is the reason a modern iPad refuses or warns.

- **What to do:** on a modern Mac, sweep `securityType` over a plausible range, calling the SPI
  with a valid channel and passphrase and recording which values return `YES` and what
  `interfaceMode`/security the resulting AP advertises to a scanning client. Also try
  `enableHostAPMode` / `startHostAPMode:` (the no-SSID variant), which may read security from the
  Internet Sharing preferences and get WPA2 for free.
- **Expected win:** unblocks the whole one-hop mode on modern clients — `airplay-why-fast.md` R1
  is explicitly blocked on
  `findings/major-hostap-offers-only-wep-or-a-silently-open-network.md`.
- **Cannot be done on the 10.15 dev box** meaningfully; needs a current Mac.

Findings file: `findings/major-hostap-sweep-security-types-for-wpa2.md`

---

## 6. Verification plan

**Measurable on the Windows box (11 26200, RTX 3080 Ti laptop, hybrid graphics):**

- R1: `pipeline.rs` capture→encode harness at display refresh 60 / 120 / 240 with `max_fps` pinned
  at 60. Expect the mean to drop by ~4 ms from 60→120. This is the headline number.
- R2 (diagnosis half): log the composing adapter from `dxgi/mod.rs::find_output` against the encode
  adapter on every join. On a hybrid laptop we expect to see Intel compose and NVIDIA encode
  *today*. The fix half needs a driver rebuild.
- R3: whether `CreateFromConnectionProfile` throws from our unpackaged binary, and what
  `GetTetheringCapabilityFromConnectionProfile` returns. **Do this first** — it decides whether the
  Mobile Hotspot path exists for us at all, and it is a ten-line test.
- R3 offline case: disconnect all uplinks and confirm `start_hosted_network` fails today, then
  confirm it succeeds after removing the `GetInternetConnectionProfile` dependency.
- R4: `iperf3` and RTT histograms over Wi-Fi Direct 2.4 GHz vs Mobile Hotspot 5 GHz vs 6 GHz.
- R9: WGC capture of a virtual monitor, cursor present or not.

**Not measurable on either dev box:**

- Everything about **macOS HostAP on a modern release** (R10, and the `respondsToSelector:` blocker
  already filed): the 10.15.8 Intel MacBook cannot tell us what Sonoma/Sequoia/Tahoe do, and it
  cannot tell us anything about Apple Silicon radios.
- **macOS `CGVirtualDisplay` refresh rate above 60** (§2.6, R1's macOS half): needs a
  ProMotion-capable Mac to be meaningful.
- **The WARP composition case** (R2): requires a machine whose POST adapter is PnpStopped —
  a headless server or a muxed desktop, neither of which we have.
- Any **IddCx 1.8/1.9/1.10** feature: needs a Windows 11 host *and* a rebuilt driver, and the
  1.10 HDR work is out of scope while we encode 8-bit 709.
- **6 GHz tethering**: needs a Wi-Fi 6E adapter on the host and a 6E client.

---

## 7. Open questions

1. **Does `CreateFromConnectionProfile` actually throw from an unpackaged Win32 process?** The
   documentation says the `wiFiControl` DeviceCapability is required and that its absence throws,
   but that text is written for UWP. Several unpackaged .NET hotspot tools are reported to work.
   If it throws, ScreenExtend's Wi-Fi Direct path is dead on *every* machine today for a reason
   unrelated to Wi-Fi Direct — which would make this the highest-severity item in the report.
   Nobody has run the test.
2. **Does the macOS `CGVirtualDisplayMode` refresh rate change WindowServer's compose cadence?**
   Structurally it should. There is no documentation, and no measurement exists in this project or
   in `deskpad`/`BetterDisplay`.
3. **Is there a WPA2/WPA3 `securityType` for the private HostAP SPI on modern macOS?** (R10.)
4. **Does the private HostAP SPI still function on macOS 14/15/26 and on Apple Silicon?** The
   selector is present in class dumps through CoreWLAN 1375.2 (Monterey era); beyond that we have
   no evidence either way, and the `respondsToSelector:` guard that would tell us at runtime is
   the subject of an already-filed blocker.
5. **With `USE_SMALLEST_MODE` unset, is a resolution change on our virtual monitor a real modeset
   or DWM per-frame scaling?** This has a direct bearing on the DPR-ladder doctrine and can be
   answered by watching whether `EVT_IDD_CX_ADAPTER_COMMIT_MODES` fires on a scale change.
6. **What does Parsec's VDD do differently?** It is closed source, but its INF is readable on any
   machine that has it installed, and its `UmdfExtensions` line would tell us in one grep whether
   the IddCx level is the reason it is reported to stutter less than the open-source VDDs.
7. **Can Internet Sharing be driven with no upstream interface on macOS?** The preference schema
   assumes one. If a null/loopback primary interface is accepted, it becomes a supported
   alternative to the private SPI — but nobody has tried it.

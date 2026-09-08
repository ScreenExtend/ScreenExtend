# K — `macos_utils/` (top level), macOS bundle config, and the ScreenExtendAudio AudioServerPlugIn

Territory: every file directly in `src-tauri/src/macos_utils/` (not `streamer/`, `audio/`, `airplay/`),
`src-tauri/Entitlements.plist`, `src-tauri/Info.plist`, `src-tauri/tauri.macos.conf.json`,
`src-tauri/build.rs`, all of `src-tauri/macos/ScreenExtendAudio/`, and `src/pages/bootstrap.tsx`
(read only to trace the permission/setup call sequence).

Read in full: `mod.rs` (669), `virtual_display.rs` (516), `networking.rs` (303), `permissions.rs` (217),
`hosted_network.rs` (217), `hostap.m` (142), `device_reporter.rs` (98), `compatibility.rs` (85),
`objc_exception.rs` (81) — 2328 lines; the driver project (17 files, ~900 lines); the three config files;
`bootstrap.tsx` (639).

Two files exist in the directory that the brief did not name: **`objc_exception.rs`** (an
`objc_setExceptionPreprocessor` logging hook) and the `streamer/`, `audio/`, `airplay/` subdirectories
(other territories). There is no `linux`-style scaffold file and no `mod.rs` submodule I was not given.

---

## 1. Files and purpose

**`mod.rs` (669)** — the macOS platform surface. Declares `AppState` (identical field-for-field to the
Windows one), the `StreamerHandle` wrapper around `axum_server::Handle`, `TauriCloudStatusSink` (the
`CloudStatusSink` impl that mirrors relay state into a `Mutex<(String,String)>` and emits
`CloudStatusChange` / `SessionIdChange`), `focus_main_window` (raw `NSApplication` +
`makeKeyAndOrderFront:`), the `setup` command, twenty-odd Tauri commands (device overrides, bans,
approvals, disconnect grace, TURN, ports, GPU-encode toggle, display capacity, cloud register/unregister,
the driver stubs, the three legacy-audio driver commands, the volume-key proxy), `remove_all_displays`,
and `sync_streamers` — the function that reconciles the set of bound axum servers against the current
adapter list.

**`virtual_display.rs` (516)** — the `CGVirtualDisplay` backend. `extern_class!` declarations for the
four private CoreGraphics classes plus `extern_methods!` for every selector used; `create_cg_virtual_display`
(descriptor + settings + `applySettings:`); `activate_mode` (the mode-find and settle polling loops);
a process-global `OnceLock<Mutex<DisplayRegistry>>` keeping `Retained<CGVirtualDisplay>` alive by display
id; the `DisplayBackend` tier enum and `probe_display_backend()`; `new_shared_controller()`;
`reconfigure_display()`; and the `VirtualDisplayController` impl.

**`permissions.rs` (217)** — the Accessibility and Screen Recording surface. Link-time `ApplicationServices`
declarations for the AX functions, `dlsym`'d `CGPreflightScreenCaptureAccess` /
`CGRequestScreenCaptureAccess`, and the `CONFIRMED` / `PROBE` latch that is supposed to work around
TCC's per-process caching. Exposes `check_permissions`, `request_permission`, `open_permission_settings`.

**`networking.rs` (303)** — adapter enumeration and the change watcher. Hand-rolled `ifmediareq` / `ifreq`
structs and `_IOWR` ioctl numbers for `SIOCGIFMEDIA` and `SIOCGIFFUNCTIONALTYPE`; a `getifaddrs` walk;
CoreWLAN SSID lookup; `get_network_adapters` (Tauri command), `cli_adapters` (a near-verbatim duplicate
for the CLI), and `watch_for_network_changes` (a 1 Hz polling thread).

**`hosted_network.rs` (217)** — the Wi-Fi hotspot. CoreWLAN `CWWiFiClient` power on/off, channel selection
(5 GHz 36/40/44/48 preferred, then 2.4 GHz ch 11, then whatever is first), and `startHostAPModeWithSSID:
securityType:channel:password:error:` — a private SPI sent by bare `msg_send!` with WEP or open security.
Exposes `is_wifi_on`, `turn_on_wifi`, `start_hosted_network`, `stop_hosted_network`, `is_hosted_network`.

**`hostap.m` (142)** — **not compiled by anything.** A verbatim copy of a public gist
(`gist.github.com/wolever/4418079`, credited on line 1) that the Rust was derived from. Its `start`
branch discards the parsed `-s`/`-p` arguments and invokes with hardcoded `@"Test"` / `@"barbarbarr"`.
`build.rs` is two lines of `tauri_build::build()`; there is no `cc` step.

**`compatibility.rs` (85)** — `check_system_requirements`. Forks `sw_vers` for the OS name and version,
asks `macos_at_least(10,15)`, `screencapturekit_available()` (itself another `sw_vers` fork),
`probe_display_backend()` and `probe_audio_backend()`, and assembles `CompatibilityReport` with a list of
`UnsupportedApi` entries.

**`device_reporter.rs` (98)** — `TauriDeviceReporter`, the `DeviceReporter` impl that turns
join/remove/audio-outputs/join-paused callbacks from the streamer core into `tauri-specta` events, and
kicks a `sync_streamers` on join and leave (on a freshly spawned thread each time).

**`objc_exception.rs` (81)** — installs an `objc_setExceptionPreprocessor` hook (via `dlsym`) that
`eprintln!`s the exception name, reason and up to 40 `callStackSymbols` frames, then chains to the
previous preprocessor. Called once from `lib.rs:332`, right after `cli::fast_path()`. It logs but does not
stop propagation.

**`src-tauri/Entitlements.plist`** — three keys: the `com.apple.CG.virtual-display` mach-lookup temporary
exception, `app-sandbox: false`, `device.audio-input: true`.

**`src-tauri/Info.plist`** — `NSAudioCaptureUsageDescription` and the ATS `NSAllowsLocalNetworking` /
`NSAllowsArbitraryLoadsInWebContent` exceptions. **Referenced by neither Tauri config** (see §12).

**`src-tauri/tauri.macos.conf.json`** — the overlay: version 0.6.3, `bundle.resources` =
`["resources/libopus.dylib"]` only, `bundle.macOS.entitlements` + `minimumSystemVersion: "10.15"`,
`app.windows: []` and `csp: null`.

**`src-tauri/build.rs`** — `fn main() { tauri_build::build() }`.

**`src-tauri/macos/ScreenExtendAudio/`** — a standalone C++ `AudioServerPlugIn` built on libASPL:
`branding.hpp` (all branded strings + the shm name + the factory UUID), `shm_ring.{hpp,cpp}` (the 64-byte
header + SPSC float ring, the ABI contract with `shm_reader.rs`), `device.{hpp,cpp}` (the
`CaptureHandler` implementing `OnProcessMixedOutput` and `OnReadClientInput`, plus `BuildDevice`),
`volume_control.{hpp,cpp}` (sets the volume scalar to 1.0 and unmutes at construction), `main.cpp`
(the `ScreenExtendAudioEntryPoint` CFPlugIn factory), `Info.plist` (CFPlugInFactories/Types wiring),
`scripts/{build,package,sign_and_notarize}.sh` + `postinstall`, and `tools/{hal_test.c,shmcheck.c}`
(two standalone diagnostic programs, not part of the build).

**`src/pages/bootstrap.tsx` (639)** — the startup state machine: update check -> `ensurePermissions` ->
`checkSystemRequirements` -> `runSetup`. Owns the permission modal and its 200 ms poll.

---

## 2. Function catalog

### `permissions.rs`
- `accessibility_trusted() -> bool` (40) — `AXIsProcessTrusted() && accessibility_api_enabled()`.
- `accessibility_api_enabled() -> bool` (44) — creates the system-wide AX element, sets a 0.5 s messaging
  timeout, reads `AXFocusedApplication`, releases both, returns `err != kAXErrorAPIDisabled (-25211)`.
  Returns `true` when the element is null.
- `prompt_accessibility() -> bool` (62) — builds a one-entry `CFDictionary`
  `{AXTrustedCheckOptionPrompt: true}` and calls `AXIsProcessTrustedWithOptions`. Falls back to
  `accessibility_trusted()` if the dictionary allocation returns `None`.
- `cg_bool_fn(name) -> Option<fn() -> bool>` (83) — `dlsym(RTLD_DEFAULT, name)` + `transmute`.
- `preflight_screen_capture() -> Option<bool>` (98) — `CGPreflightScreenCaptureAccess` via `cg_bool_fn`.
- `probe_and_latch() -> bool` (115) — calls `streamer::probe_screen_recording()`; on success sets
  `CONFIRMED` (one-way).
- `screen_recording_granted() -> bool` (123) — the latch state machine. **See §11 and the blocker finding.**
- `request_screen_recording() -> bool` (160) — `screen_recording_granted()`, else `CGRequestScreenCaptureAccess`.
- `check_permissions() -> Vec<PermissionStatus>` (172) — two entries, both `required: true`.
- `request_permission(key) -> bool` (194) — dispatches to the two prompts; unknown key -> `false`.
- `open_permission_settings(key)` (204) — spawns `open x-apple.systempreferences:…`.

### `virtual_display.rs`
- `make_settings(w,h,refresh,hidpi) -> Retained<CGVirtualDisplaySettings>` (138) — one mode; halves w/h
  when `hidpi == 1`.
- `create_cg_virtual_display(name,w,h,refresh)` (162) — descriptor with a high-priority global dispatch
  queue, sRGB-ish primaries, 96-DPI-derived physical size, incrementing serial, fixed vendor/product;
  `initWithDescriptor:` (nil -> a descriptive `Err` naming the missing entitlement) then a **single-mode**
  `applySettings:` with `setHiDPI(0)`.
- `activate_mode(id,w,h) -> Result<(),String>` (230) — early-returns if already the right size; else a
  find loop (1.5 s, 50 ms sleeps) over `CGDisplayCopyAllDisplayModes`, `CGDisplaySetDisplayMode`, then a
  settle loop (1.5 s, 50 ms sleeps).
- `registry() -> &'static Mutex<DisplayRegistry>` (308) — the `OnceLock` map.
- `DisplayBackend::as_str` (325).
- `cg_virtual_display_available() -> bool` (334) — `AnyClass::get` on all four class names.
- `cd_virtual_display_available() -> bool` (342) — `dlsym("CDVirtualDisplayCreate")`, informational only.
- `probe_display_backend() -> DisplayBackend` (347) — `OnceLock`-cached; `CgVirtualDisplay` or
  `AirPlayFallback`. **Never returns `Unsupported`.**
- `new_shared_controller() -> Option<SharedVirtualDisplay>` (363) — dispatches on the tier.
- `reconfigure_display(name,w,h,refresh,hidpi)` (381) — parses the display id from a string; refuses the
  main display unless AirPlay owns it; for AirPlay-owned displays only `activate_mode`; otherwise
  `applySettings` under the registry lock then `activate_mode`.
- `MacosVirtualDisplay::new_shared()` (447) — touches `CGMainDisplayID`/`PixelsWide`/`PixelsHigh` (a
  CoreGraphics warm-up), clears the registry, returns `Arc::new(Self)`.
- `impl VirtualDisplayController` (459) — `create_display` (460), `display_device_name` (488),
  `remove_display` (493), `remove_all_displays` (499). **`create_display_with_modes` and
  `max_concurrent_displays` are not overridden** — the trait defaults at `streamer/session.rs:636,655`
  discard `extra_modes` and return `None`.
- `impl Debug for MacosVirtualDisplay` (437) — takes the registry lock.

### `mod.rs`
- `TauriCloudStatusSink::new_shared` (69), `report` (75), `session_rotated` (87).
- `focus_main_window(window)` (96) — `NSApplication activateIgnoringOtherApps:` + `makeKeyAndOrderFront:`.
- `setup(app_handle) -> bool` (120) — idempotent via `try_state`; `recover_on_launch()`;
  `spawn_blocking(new_shared_controller).await`; builds and manages `AppState`. **Reads no permission.**
- `set_device_override(...)` (167) — clamps everything, decides whether a reconfig epoch bump is needed.
- `refresh_device_stream(ip) -> bool` (223), `set_device_audio_output` (235),
  `get_device_audio_outputs` (246), `remove_device_override` (255), `set_device_banned` (264),
  `set_device_approved` (286), `set_disconnect_grace` (301), `get_disconnect_grace` (314),
  `get_display_capacity` (322), `set_turn_config` (344), `get_turn_config` (370),
  `get_server_ports` (387), `set_server_ports` (394), `get_disable_gpu_encode` (442),
  `set_disable_gpu_encode` (450).
- `install_drivers` / `remove_drivers` (458, 464) — **unconditional `true` stubs.**
- `install_audio_driver(app) -> String` (470) — resolves `resources/ScreenExtendAudio.pkg` (never bundled),
  calls `installer::install_pkg`.
- `uninstall_audio_driver` (487), `audio_driver_status` (493), `set_legacy_volume_key_proxy` (504).
- `remove_all_displays(client)` (508) — spawns a thread and immediately joins it.
- `set_session_credentials` (515), `register_cloud_session` (523), `unregister_cloud_session` (554),
  `get_cloud_status` (566).
- `display_slot_holder(state) -> Option<String>` (574) — returns `None` immediately unless
  `max_concurrent_displays()` is `Some`.
- `sync_streamers(state)` (582) — the adapter/streamer reconciler.

### `networking.rs`
- `iowr(num,size) -> u64` (25) — const `_IOWR` encoder; `name_into` (55).
- `media_status(name) -> Option<i32>` (61), `functional_type(name) -> Option<i32>` (75) — socket + ioctl +
  close each.
- `interface_index(name)` (89), `wifi_ssid(name)` (97), `hosted_network_name(app)` (105).
- `get_network_adapters(app,state) -> Vec<NetworkInfo>` (120) — full enumeration.
- `cli_adapters() -> Vec<NetworkInfo>` (199) — a copy of the above without the hosted-network naming.
- `watch_for_network_changes(app)` (277) — spawns the 1 Hz poll thread.
- `apply(app,adapters)` (295) — stores, `sync_streamers`, emits `NetworkChange`.

### `hosted_network.rs`
- `wifi_interface() -> Option<Retained<CWInterface>>` (14), `is_wifi_on` (20), `turn_on_wifi` (29).
- `pick_channel(interface)` (38) — 5 GHz 36/40/44/48, then ch 11, then first-in-set.
- `try_start_host_ap(interface,channel,ssid,password) -> bool` (59) — the raw `msg_send!` SPI call.
- `try_start_on_channel(...)` (99) — secured attempt, then **open** attempt, emitting
  `HostedNetworkNoPassword` on the downgrade.
- `stop_host_ap_mode()` (119) — `msg_send![iface, stopHostAPMode]`.
- `start_hosted_network(app,state,name,password) -> bool` (129), `stop_hosted_network` (201),
  `is_hosted_network` (212).

### `compatibility.rs`
- `sw_vers(field) -> String` (7), `os_version_string()` (17) — two forks.
- `check_system_requirements() -> CompatibilityReport` (36).

### `device_reporter.rs`
- `TauriDeviceReporter::new_shared` (105), `resync_streamers` (109) — thread per call.
- `report_join` (121), `report_remove` (139), `report_join_attempts_paused` (156),
  `report_audio_outputs` (165).

### `objc_exception.rs`
- `describe(value) -> String` (195), `report(exception)` (206) — the preprocessor, `install_logger()` (250).

### C++ driver
- `ShmRing::~ShmRing` (`shm_ring.cpp:13`), `Open()` (22), `Write(frames,sampleCount)` (70).
- `CaptureHandler::CaptureHandler` (`device.cpp:16`), `OnProcessMixedOutput` (23),
  `OnReadClientInput` (54), `BuildDevice(context,handler)` (73).
- `ConfigureVolumeAndMute(device)` (`volume_control.cpp:14`).
- `CreateDriver()` (`main.cpp:18`), `ScreenExtendAudioEntryPoint(allocator,typeUUID)` (36).

---

## 3. Threads

| Where | Kind | Priority/QoS | Lifetime | Owns |
|---|---|---|---|---|
| `permissions.rs:139` | `std::thread::spawn`, one per due refresh | default | one probe (up to 5 s SCK / 700 ms CGDS) | the `running` flag it must clear; a panic before line 141 wedges it forever |
| `mod.rs:126` | `tauri::async_runtime::spawn_blocking` | tokio blocking pool | one `new_shared_controller()` call | nothing after it returns |
| `mod.rs:510` | `std::thread::spawn` + immediate `.join()` | default | one `remove_all_displays()` | pointless hop; still blocks the caller |
| `mod.rs:660` | `std::thread::spawn`, **one per adapter IP** | default | until the axum handle shuts down | a full multi-thread tokio runtime each (`Streamer::run_with_handle`) |
| `networking.rs:278` | `std::thread::spawn`, one | default | **forever** (no stop signal, no join handle) | the 1 Hz adapter poll; calls `sync_streamers` on change |
| `device_reporter.rs:112` | `std::thread::spawn`, **one per join and per leave** | default | one `sync_streamers()` | nothing |
| `virtual_display.rs:172` | `dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH)` handed to the display descriptor | high | display lifetime | CoreGraphics' own callbacks |
| driver `OnProcessMixedOutput` / `OnReadClientInput` | coreaudiod's real-time IO thread | RT | IO cycle | no locks, no allocation — correct |

Nothing in this territory creates a tokio task. The permission probe, the adapter watcher and the
device-reporter resync are all raw OS threads with no lifecycle management; the adapter watcher in
particular can never be stopped, and `watch_for_network_changes` is a Tauri command, so a second call
starts a **second** identical poll thread.

---

## 4. Locks

| Lock | Guards | Taken by | Hot path? | Across `.await` / callback? |
|---|---|---|---|---|
| `permissions.rs:109` `PROBE: Mutex<ProbeState>` | `started` / `running` / `last` | `screen_recording_granted` (131, 156), the probe thread (141) | yes — 5 Hz while the gate is open | no `.await`; **poisoning on a panic bricks every later call** (`.unwrap()` at all three sites) |
| `permissions.rs:108` `CONFIRMED: AtomicBool` | the one-way grant latch | same | yes | n/a; `Relaxed` ordering, fine |
| `virtual_display.rs:309` `REGISTRY: Mutex<DisplayRegistry>` | the id -> `Retained<CGVirtualDisplay>` map | `create_display` (479), `remove_display` (494), `remove_all_displays` (500), `reconfigure_display` (414), `new_shared` (454), `Debug::fmt` (439) | **yes — join and leave** | no `.await`, but **held across `make_settings` (ObjC alloc) and `applySettings:` (WindowServer IPC)** at 419-421 |
| `AppState.streamers` | the bound axum handles | `sync_streamers` (618), `set_server_ports` (428) | join/leave and adapter change | held across `graceful_shutdown` (non-blocking) and across `std::thread::spawn` in the insert loop |
| `AppState.network_adapters` | the adapter list | `sync_streamers` (584), `apply` (298) | 1 Hz | short |
| `AppState.device_overrides` | per-IP overrides | `set_device_override` (201), `remove_device_override` (256), `report_join` (**124, `if let` scrutinee**) | join | **`device_reporter.rs:124` is the temporary-lifetime pattern**: `if let Some(o) = self.overrides.lock().unwrap().get(&ip) { … }` keeps the guard to line 132. Synchronous today, so safe — but it is exactly the shape the project bans |
| `AppState.banned_devices` / `approved_devices` | trust sets | `set_device_banned` (271,279), `set_device_approved` (291,294) | no | short |
| `AppState.cloud` | `Option<CloudClient>` | `register_cloud_session` (545), `unregister` (555) | no | held across `prev.stop()` |
| `AppState.cloud_status` | `(state, detail)` | `TauriCloudStatusSink::report` (77), `get_cloud_status` (567), `register` (542) | on every relay state change | statement-scoped; dropped before `emit` |
| `AppState.hosted_network_running` / `stop_hosted_network` | hotspot state | `hosted_network.rs:136,141,147,193,195,202,206,216` | no | short |
| `AppState.disconnect_grace` / `disable_gpu_encode` | `AtomicU64` / `AtomicBool` | commands | no | fine |
| `virtual_display.rs:198` `NEXT_SERIAL: AtomicU32` | display serial numbers | `create_cg_virtual_display` | join | fine; resets per process |
| `objc_exception.rs:193` `PREVIOUS: AtomicPtr` | the chained preprocessor | `install_logger` / `report` | on every ObjC exception | fine |
| C++ `ShmHeader::writePos` / `generation` / `overruns` | the SPSC ring | driver RT thread (producer), Rust reader (consumer) | **yes — audio hot path** | lock-free; Release on the write, no lock at all. Correct. |

No `tokio::sync` lock is used anywhere in this territory. There is no place a `MutexGuard` currently
lives across an `.await` — `setup` is the only `async fn` here and takes no lock before its `.await`.

---

## 5. Buffers and copies

This territory owns no video frame buffer. The transfers that exist:

- **`ShmRing::Write`** (`shm_ring.cpp:70-82`) — a **scalar per-sample copy** from the HAL's mix buffer
  into the mmap'd ring: `for (i…) ring_[(w+i) & mask_] = frames[i];`. One CPU copy per IO cycle, no
  allocation, RT-safe. A two-`memcpy` split around the wrap would be strictly better.
- **The loopback mirror** (`device.cpp:44-51`) — a **second** CPU copy of the same frames into
  `loopback_` (a 65536-frame x 2-channel `std::vector<float>`, 512 KiB, allocated once in the
  constructor). Every IO cycle pays both copies whether or not the HAL fallback is in use.
- **`OnReadClientInput`** (`device.cpp:54-71`) — a third copy out of `loopback_` into the client's buffer,
  only when the HAL-input fallback path is active.
- **The shm mapping itself** — `kShmTotalBytes = 64 + 131072*4 = 524_352` bytes, mapped `MAP_SHARED` once;
  the Rust reader maps the same pages, so the ring is genuinely zero-copy across the process boundary.
- **`networking.rs`** — one `String` allocation per interface per poll (`to_string_lossy().into_owned()`,
  lines 133-135 and 211-213), plus a `Vec<String>` of IPs and three `BTreeMap`s rebuilt from scratch every
  second. Small, but it is allocation churn at 1 Hz forever.
- **`compatibility.rs`** — three `Command::output()` allocations per report.
- **`virtual_display.rs`** — ObjC object allocation per `make_settings` / `create_cg_virtual_display` call
  (a settings object, a mode object, a one-element `NSArray`); `Retained` clones are refcount bumps.

Ownership transfers: `Retained<CGVirtualDisplay>` moves from `create_cg_virtual_display` into the registry
map (479) and is dropped out of it on remove (494/500) — that drop is the **only** teardown mechanism
(see the speculative finding).

---

## 6. Queues

- **`ShmRing`** — a single-producer/single-consumer float ring. Capacity `kRingCapacity = 131072`
  **samples** (`shm_ring.hpp:17`), i.e. 65536 stereo frames ~= **1.365 s @ 48 kHz**. Power-of-two,
  `static_assert`ed. **Unbounded in the write direction**: the producer never blocks and never checks the
  reader — it wraps and silently overwrites. The `overruns` counter reserved at offset 48 is **never
  incremented**, so back-pressure and loss are entirely invisible.
- **`loopback_`** (`device.hpp:46-49`) — 65536 frames x 2 channels, indexed by absolute sample time masked
  to the capacity. Not a queue in the usual sense: a timestamp-addressed circular scratch, so a reader
  requesting a timestamp older than 1.37 s silently gets whatever wrapped into that slot.
- **`permissions.rs:130-144`** — the `ProbeState` is a one-slot "is a probe in flight" gate, not a queue.
  Depth 1, no back-pressure; when `running` is stuck true it is permanently closed.
- **`cgds::probe_screen_recording` / `sck::probe_screen_recording`** — `std::sync::mpsc` channels used as
  one-shot signals with `recv_timeout` (700 ms / 5 s). Unbounded, but at most one value is read.
- **`AppState.streamers`** — a map, not a queue; `sync_streamers` is the only reconciler and it is called
  from four places (`networking::apply`, `device_reporter::resync_streamers` x2, `set_server_ports`) with
  **no debounce or coalescing**.

No `crossbeam-channel`, no `tokio::sync::broadcast`, no bounded channel exists in this territory.

---

## 7. Syscall / FFI boundaries

**`permissions.rs`**
- `#[link(name = "ApplicationServices", kind = "framework")]` (18): `AXIsProcessTrusted`,
  `AXIsProcessTrustedWithOptions`, `AXUIElementCreateSystemWide`, `AXUIElementCopyAttributeValue`,
  `AXUIElementSetMessagingTimeout`. All ≤ 10.9. Link-time — safe on the 10.15 floor.
- `#[link(name = "CoreFoundation")]` (31): `CFRelease`.
- `libc::dlsym(RTLD_DEFAULT, …)` for `CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess`
  (10.15+) — **correctly runtime-resolved**, `transmute`d to `unsafe extern "C" fn() -> bool`.
- `Command::new("open")` for the System Settings deep links. The Tauri shell allowlist in
  `capabilities/default.json` covers `open x-apple.systempreferences:…`; this call goes through
  `std::process::Command`, not the shell plugin, so the allowlist does not gate it.

**`virtual_display.rs`**
- `extern "C" { fn dispatch_get_global_queue(isize, usize) -> *mut AnyObject; }` (20) — 10.6, link-time,
  safe. Note the real signature returns `dispatch_queue_t`; typing it as `*mut AnyObject` is fine for
  passing straight to `setQueue:`.
- `extern_class!` x4 for `CGVirtualDisplay{,Descriptor,Settings,Mode}` — objc2 resolves these at **runtime**
  via the Obj-C runtime, so a Mac without them still loads the binary. **Instantiating a missing class
  panics**, and the guard is `probe_display_backend()` -> `cg_virtual_display_available()`
  (`AnyClass::get` on all four names, line 334-340), consulted by `new_shared_controller` (364) and
  `get_display_capacity` (`mod.rs:329`). `reconfigure_display` does not re-check but is only reachable via
  a registry entry that only exists after a successful create.
- `extern_methods!` selectors: `setName:`, `setMaxPixelsWide:`, `setMaxPixelsHigh:`,
  `setSizeInMillimeters:`, `setVendorID:`, `setProductID:`, `setSerialNum:`, `set{Red,Green,Blue}Primary:`,
  `setWhitePoint:`, `setQueue:`, `initWithWidth:height:refreshRate:`, `setModes:`, `setHiDPI:`,
  `initWithDescriptor:`, `applySettings:`, `displayID`. All private SPI; none is `respondsToSelector:`-checked,
  but the class probe covers the common case.
- `objc2_core_graphics`: `CGMainDisplayID`, `CGDisplayPixelsWide/High`, `CGDisplayCopyAllDisplayModes`,
  `CGDisplaySetDisplayMode`, `CGDisplayMode::width/height`. All ≤ 10.6.
- `libc::dlsym` for `CDVirtualDisplayCreate` (342) — informational probe only.

**`networking.rs`**
- `libc`: `getifaddrs`/`freeifaddrs`, `socket`, `ioctl`, `close`, `if_nametoindex`. The two ioctl numbers
  are **hand-computed** with a local `_IOWR` (`const fn iowr`, line 25) over hand-declared
  `#[repr(C, packed(4))] ifmediareq` and `#[repr(C)] ifreq` structs (31-46). `SIOCGIFMEDIA` = `iowr(56,
  size_of::<ifmediareq>())` and `SIOCGIFFUNCTIONALTYPE` = `iowr(173, size_of::<ifreq>())` — **the ioctl
  number embeds `sizeof`, so if the Rust struct layout ever diverges from the kernel's the ioctl silently
  returns `ENOTTY` and the interface is dropped from the list.** `ifreq`'s `data: [u8; 16]` is a
  hand-rolled union stand-in.
- `objc2_core_wlan`: `CWWiFiClient::sharedWiFiClient`, `interfaceWithName:`, `ssid` — 10.10/10.7, and an
  XPC round trip to `airportd` each.

**`hosted_network.rs`**
- `objc2_core_wlan`: `interface()`, `powerOn`, `setPower:error:`, `supportedWLANChannels`,
  `channelBand`, `channelNumber`, `interfaceMode` — all 10.7-10.10, link-time, safe.
- **Bare `msg_send!` to two private SPIs** with no `respondsToSelector:` guard (76-85, 122). The
  `securityType` argument is typed `u64`; the real parameter is `CWSecurityMode`/`NSUInteger` — correct on
  64-bit. `password` is passed as a possibly-null `*const NSString`. `error:` is `&mut *mut NSError`
  (autoreleased, correctly not released).

**`objc_exception.rs`** — `dlsym("objc_setExceptionPreprocessor")`, `msg_send!` for
`respondsToSelector:`, `name`, `reason`, `callStackSymbols`, `count`, `objectAtIndex:`, `UTF8String`.
All guarded by `respondsToSelector:` for the exception's own selectors. Safe.

**`mod.rs`** — `msg_send![class!(NSApplication), sharedApplication]`,
`activateIgnoringOtherApps:` (deprecated 14.0 but present), `makeKeyAndOrderFront:`.

**C++ driver** — `shm_open`, `ftruncate`, `mmap`, `munmap`, `close` (POSIX); `CFEqual`,
`kAudioServerPlugInTypeUUID` (CoreAudio, ancient); libASPL's own `AudioServerPlugIn` vtable. Linked
frameworks: `CoreFoundation`, `CoreAudio` only (`build.sh:59`). No versioned struct is passed across;
libASPL owns the `AudioServerPlugInDriverInterface` versioning.

**Dyld safety verdict for this territory: clean.** Every symbol newer than 10.15 is `dlsym`'d
(`CGPreflight/RequestScreenCaptureAccess`, `objc_setExceptionPreprocessor`, `CDVirtualDisplayCreate`) or
runtime-resolved (`extern_class!`, `msg_send!`). There is **no** `extern static` and no link-time
reference to a post-10.15 API anywhere in the nine files. The `nm -u` check in `CLAUDE.md` should stay
clean after any change here.

---

## 8. Panics

- `permissions.rs:131, 141, 156` — `PROBE.lock().unwrap()`. **Poisoning is reachable**: a panic inside the
  spawned closure while the guard is held at 141-143 poisons it, and every later `check_permissions` call
  panics inside the command. `bootstrap.tsx:308` swallows the resulting error forever.
- `permissions.rs:41, 46-57` — the AX calls themselves cannot panic, but `CFString::from_str` allocates.
- `virtual_display.rs:343` — `CString::new("CDVirtualDisplayCreate").unwrap()` (infallible in practice).
- `virtual_display.rs:414, 439, 454, 479, 494, 500` — `registry().lock().unwrap()`; poisoning is reachable
  if any ObjC call inside the 414-425 critical section panics or raises.
- `virtual_display.rs:144, 152, 168, 171, 203, 211` — `CGVirtualDisplay*::alloc()` **panics if the class is
  absent**. Gated by `probe_display_backend()` for every reachable path, but the gate is a `OnceLock` cache
  that is never re-evaluated.
- `virtual_display.rs:246-252` — raw pointer arithmetic over `CFArray` contents; bounded by `count()`.
- `networking.rs:85` — `req.data[..4].try_into().unwrap()` on a `[u8; 16]`; infallible.
- `networking.rs:279, 285, 297` — **`app.state::<AppState>()` panics if `AppState` is not managed.**
  `watch_for_network_changes` is called from `bootstrap.tsx:131` after `setup`, so the ordering holds
  today; nothing enforces it, and the panic is inside a detached thread with no handler.
- `mod.rs` — 25 `.lock().unwrap()` sites; all poison-reachable in principle.
- `mod.rs:517-518` — `session_auth.{session_id,otp}.lock().unwrap()`.
- `hosted_network.rs:136, 141, 147, 193, 195, 202, 206, 216` — `.lock().unwrap()`.
- `hosted_network.rs:76-85, 122` — **not a Rust panic but worse**: an unrecognized selector raises an
  Obj-C exception through a non-`-unwind` frame (UB / abort). See the blocker finding.
- `device.cpp:61-70` — no null check on `bytes`; `static_cast<float*>` on a possibly-unaligned pointer.
- `shm_ring.cpp:78` — `ring_[(w+i) & mask_]` is mask-bounded, so no OOB, but `ring_`/`mask_` are only valid
  when `header_ != nullptr`, which line 72 does check.

---

## 9. Silent fallbacks and swallowed errors

1. **`permissions.rs:127-129`** — a negative `CGPreflightScreenCaptureAccess` terminates the whole check
   before the probe machinery that exists to work around it. No log. **This is the blocker.**
2. **`permissions.rs:152-154`** — every call after the first returns a hard `false` regardless of the true
   state. No log.
3. **`permissions.rs:139-144`** — the probe thread's panic is swallowed by the runtime and leaves
   `running = true` forever.
4. **`permissions.rs:215`** — `open` spawn failure is logged (good) but the command returns `()`, so the UI
   cannot tell.
5. **`virtual_display.rs:481-483`** — `activate_mode` failure on the join path is logged and **discarded**;
   `create_display` returns `Ok(id)` with the wrong geometry.
6. **`virtual_display.rs:352-358`** — `CDVirtualDisplayCreate` being present is logged as "please report
   this" and then ignored; the tier still falls to AirPlay.
7. **`mod.rs:128-132`** — `Ok(None)` and `Err(JoinError)` from `spawn_blocking(new_shared_controller)` are
   collapsed into `_ => return false`, **discarding the panic payload**.
8. **`mod.rs:458-465`** — `install_drivers`/`remove_drivers` return `true` unconditionally on macOS, so the
   frontend's recovery path (`bootstrap.tsx:157-160`) waits 5 s and retries an identical failure.
9. **`mod.rs:470-482`** — `install_audio_driver` cannot find its `.pkg` in any shipped build; it returns the
   generic string `"failed"`.
10. **`hosted_network.rs:111-116`** — a WEP failure silently brings up the **same SSID with no encryption**
    and reports success; only a `HostedNetworkNoPassword` event marks the downgrade.
11. **`hosted_network.rs:114, 147`** — `let _ = crate::HostedNetworkNoPassword.emit(app);` — the emit result
    is discarded, so if the event fails the user is never told about the downgrade at all.
12. **`hosted_network.rs:192-197`** — success is never verified against `interfaceMode()`.
13. **`hosted_network.rs:201-208`** — `stop_hosted_network` always returns `true`.
14. **`networking.rs:127, 205`** — `getifaddrs` failure returns an empty adapter list with no log, which
    `sync_streamers` reads as "stop every streamer".
15. **`networking.rs:61-87`** — `media_status`/`functional_type` returning `None` (a failed socket or ioctl)
    silently drops the interface via `?` in the `filter_map`.
16. **`networking.rs:100-102`** — a nil SSID silently becomes the string `"Wi-Fi"`.
17. **`macos_utils/streamer/mod.rs:76`** — `macos_at_least` **fails open** (`None => true`), so a host where
    `sw_vers` cannot run is told SCK is available.
18. **`mod.rs:510`** — `let _ = std::thread::spawn(…).join();` discards a panic in `remove_all_displays`.
19. **`main.cpp:23`** — `shm->Open();` return value discarded, comment says "best-effort", **no logging
    anywhere in the driver** (no `os_log`, no `syslog`).
20. **`shm_ring.cpp:25-33, 42-44`** — three failure returns, all silent.
21. **`shm_ring.cpp:50`** — a stale segment whose `magic` matches but whose `version` differs is reused
    with the old layout.
22. **`device.cpp:38-42`** — the non-stereo branch is identical to the stereo one; a non-stereo mix is
    written as if it were stereo.
23. **`volume_control.cpp:16-22`** — both `if (auto …)` blocks silently do nothing when the control is absent,
    which is precisely the failure that breaks the macOS volume keys this file exists to preserve.
24. **`scripts/postinstall`** — every command is `|| true` / `2>/dev/null` and the script ends `exit 0`.
25. **`scripts/package.sh:42-45`** — an unsigned `.pkg` (which cannot load) is a successful exit.

---

## 10. TODO / FIXME / HACK / XXX

`grep` over the whole territory finds **no** `TODO`, `FIXME`, `HACK` or `XXX` marker. The nearest things
are prose comments that mark unfinished work:

- `device.cpp:37` — `// fast path: full-scale, pre-gain, RT-safe` immediately above a branch whose two arms
  are identical (the downmix that was never written).
- `main.cpp:23` — `shm->Open(); // best-effort; a failed open is not fatal`.
- `shm_ring.cpp:26` — `return false; // sandbox denied or wrong perms — host falls back to HAL`.
- `shm_ring.cpp:29` — `// ftruncate on an already-sized segment is harmless`.
- `shm_ring.hpp:32` — `std::atomic<uint64_t> overruns;    // 48 : informational; producer never blocks` —
  describes a field nothing writes.
- `virtual_display.rs:354-357` — the `CDVirtualDisplayCreate` "please report this" log, an explicit
  unfinished-investigation marker.
- `hostap.m:83-85` — `// known security types: 2: no securiry / 16: wep / Note: values [-127..127] have
  been tried, and all but these return errors.` (typo verbatim).
- `hostap.m:72, 106, 135` — `printf("Done?")`, `printf("Found Channel: …")`, `printf("Success?\n")` —
  debugging output from the original gist, still checked in.
- `mod.rs:334` (commented-out) — nothing; but `lib.rs:337` has `//            get_devices,` in the command
  list.

---

## 11. Latency path

None of these files touch a video frame. They sit on **startup**, **join/leave**, **settings-change** and
**audio-capture** paths.

### Startup (the user-visible one, and the one that is broken)

| Stage | File:line | Cost | Serialization |
|---|---|---|---|
| `runUpdate()` -> `check()` | `bootstrap.tsx:174` | network round trip | blocks `proceed()` entirely |
| `checkPermissions()` #1 | `permissions.rs:172` | AX round trip (≤ 500 ms) + **the first synchronous `probe_and_latch()`: SCK ≤ 5 s, CGDS ≤ 700 ms** | **synchronous Tauri command -> main thread**; the UI is frozen |
| `requestPermission("screen_recording")` | `permissions.rs:160` | `CGRequestScreenCaptureAccess` blocks until the OS prompt is dismissed | main thread |
| the 200 ms poll | `bootstrap.tsx:295-311` | AX round trip + `sw_vers` fork per tick | main thread, 5 Hz |
| `checkSystemRequirements()` | `compatibility.rs:36` | **3 x `sw_vers` process spawn** (~15-60 ms) + `probe_audio_backend()` | main thread |
| `setup()` | `mod.rs:120` | `recover_on_launch()` inline + `spawn_blocking(new_shared_controller)` | the only `async` command here |
| `watchForNetworkChanges()` | `networking.rs:277` | first enumeration inline, then 1 Hz forever | detached thread |

**The interleaving in which the gate never opens** (the reported showstopper), written out:

1. `proceed()` -> `ensurePermissions()` -> `checkPermissions()`. `screen_recording_granted()`:
   `CONFIRMED` false -> `preflight_screen_capture()` returns `Some(false)` -> **`return false` at line 128**.
   `PROBE.started` is still `false`; `probe_and_latch()` never ran.
2. `requestPermission("screen_recording")` -> `screen_recording_granted()` false again ->
   `CGRequestScreenCaptureAccess()` prompts and adds the app to the TCC list (unchecked).
3. `setPermReport(await checkPermissions())` -> false again -> the modal opens; the 200 ms poll starts.
4. The user grants in System Settings. macOS shows "…will not be able to record… until it is quit"; the
   user clicks Later.
5. Accessibility flips to granted on the next poll (`AXIsProcessTrusted` re-reads TCC live).
6. Screen Recording does **not**: `CGPreflightScreenCaptureAccess` is cached per process and keeps
   returning false, so line 127-129 short-circuits on every one of the five polls per second, forever.
   `CONFIRMED` can never be latched because the code that would latch it is unreachable.
7. `perms.every(p => !p.required || p.granted)` is never true -> `proceed()` is never re-entered ->
   **`runSetup` never runs**. Only Relaunch (or quitting) escapes.

Even with the preflight removed, the latch's own design delays the truth: the first call returns the real
answer, every later call returns `false` (line 152-154), and the earliest `CONFIRMED` can be set is
`PROBE_REFRESH` (2 s) + probe latency (0.7-5 s) + up to 200 ms of poll — **2.9 s to 7.2 s** after the grant.

**Does `setup` refuse to start if a permission read is false?** No. `macos_utils::setup`
(`mod.rs:120-162`) reads no permission at all; it only needs `new_shared_controller()`, which succeeds on
any macOS ≥ 10.14.4. The refusal is entirely the frontend gate. The backend would have started and
streamed (Screen Recording only affects what the captured frames *contain*).

### Join path

| Stage | File:line | Cost | Serialization |
|---|---|---|---|
| `create_display` -> `create_cg_virtual_display` | `virtual_display.rs:473` | descriptor + `applySettings:` WindowServer IPC | inside `spawn_blocking`, under `DISPLAY_CORRELATION_LOCK` (`server.rs:94`) |
| registry insert | `virtual_display.rs:479` | µs | contends with reconfigure's long critical section |
| `activate_mode` | `virtual_display.rs:481` | **find loop ≤ 1.5 s + `CGDisplaySetDisplayMode` + settle loop ≤ 1.5 s = up to 3 s**, 50 ms sleep granularity | **inside `DISPLAY_CORRELATION_LOCK`** — every other joining client waits |
| DPR correction | `reconfigure_display` | a second `applySettings:` + a second `activate_mode` (another ≤ 3 s) | the display is always created at `hidpi = 0`, so a Retina client always pays this |
| `report_join` | `device_reporter.rs:121` | event emit + **a new OS thread** for a `sync_streamers` that is a no-op on the CGVirtualDisplay tier | — |

`activate_mode` is the single largest latency term this territory contributes, and it is paid **twice**
per Retina join because the mode ladder is discarded (§14).

### Settings-change path

`set_device_override` -> `bump_reconfig_epoch` is cheap (a lock and a counter). `reconfigure_display` is
not: it holds the registry mutex across ObjC allocation and `applySettings:` (WindowServer IPC), then
runs `activate_mode` for up to another 3 s. `set_server_ports` adds a **300 ms `std::thread::sleep` on the
main thread** (`mod.rs:434`) and hard-`shutdown()`s every streamer without a `Bye`.

### Audio path (10.15-12.x tier only)

`OnProcessMixedOutput` (RT thread) -> two CPU copies (shm ring + loopback mirror) -> `writePos` Release
store. The consumer is `shm_reader.rs` (territory elsewhere). The driver adds **zero queueing latency of
its own** — `Latency = 0`, `SafetyOffset = 0` (`device.cpp:83-84`) — and the ring holds 1.365 s of slack.
The RT callback takes no lock and allocates nothing; that convention is honoured. The HAL-input fallback
adds a full HAL round trip, and there is no signal saying which path is live.

---

## 12. Stale references

- **`CLAUDE.md:376`**: "`hosted_network.rs` + `hostap.m` — CoreWLAN **ad-hoc** network (prefers 5 GHz
  channels)". The code uses `startHostAPModeWithSSID:` — **HostAP/infrastructure mode, not IBSS/ad-hoc**.
  Nothing in the tree calls `startIBSSModeWithSSID:`; `hostap.m` only *prints* IBSS as a possible
  `interfaceMode`. And `hostap.m` is not compiled by anything.
- **`CLAUDE.md` command list** omits `get_display_capacity`, which is registered in `lib.rs` and consumed
  at `dashboard.tsx:121`. It also still lists version **0.5.4** while `tauri.macos.conf.json:5` and
  `package.json` say **0.6.3**.
- **`CLAUDE.md`** DPR doctrine ("pre-register one display mode per ratio … plus each transpose … a live
  display cannot be given new ones") does not describe macOS: `MacosVirtualDisplay` never overrides
  `create_display_with_modes`, so `extra_modes` is discarded (`streamer/session.rs:637`, `let _ =
  extra_modes;`) and every geometry change is a live `applySettings:`.
- **`src-tauri/macos/ScreenExtendAudio/README.md`** cites `PRD-macos-legacy-audio.md` and
  `AUDIO_NOTES_MACOS_LEGACY.md` "at the repo root" (lines 5-6, 61-62) — **neither exists on any branch**;
  `CLAUDE.md` itself says they survive only in history at `420004a`. Eight `PRD §n` citations
  (§1.1, §2, §3, §5.2a, §5.3, §6.2, §7.5, §7.7, §13.1) therefore resolve to nothing, and **§7.5/§7.7 are
  repeated in `scripts/package.sh:44`**.
- **README.md line 31** points at `./generate-sources-macos-legacy-audio.sh` at the repo root — **does not
  exist**, yet `scripts/build.sh:17` still probes for its output directory. With `third_party/` gitignored
  and empty, a fresh clone has no way to obtain libASPL at all.
- **README.md line 41-42** points at `project.yml` for `xcodegen generate` — **does not exist**.
- **README.md lines 56-62** ("Status on the dev box"): "has **not** been signed, notarized, installed, or
  loaded into `coreaudiod` … no runtime behaviour has been verified". The user reports it **does** load and
  work on the dev box with SIP disabled. Stale and actively misleading.
- **`shm_ring.hpp:31`** and the README claim `generation` is "bumped each driver (re)start / **format
  change**"; it is bumped once in `Open()` and never on a format change.
- **`shm_ring.hpp:32`** documents `overruns` as informational; nothing writes it.
- **`device.cpp:37`** — the `// fast path` comment describes a branch structure whose two arms are identical.
- **`hostap.m:1`** — `// FROM https://gist.github.com/wolever/4418079`; the file is a checked-in third-party
  gist with debugging `printf`s and hardcoded credentials.
- **`src-tauri/Info.plist`** is referenced by neither `tauri.conf.json` nor `tauri.macos.conf.json`.
  `tauri-bundler` merges a file of that exact name by convention, but `CLAUDE.md:575` states it as settled
  fact and nothing in CI asserts it.
- No AI-tooling references were found anywhere in this territory.

---

## 13. Dead code candidates

| Item | Evidence |
|---|---|
| **`src-tauri/src/macos_utils/hostap.m`** (142 lines) | `build.rs` is `tauri_build::build()` only; no `cc`/`clang` step exists; the only textual reference in the repo is `CLAUDE.md:376`. Also self-broken (ignores its parsed args, hardcodes `@"Test"`/`@"barbarbarr"`). |
| **`DisplayBackend::Unsupported`** (`virtual_display.rs:321`) and the `Unsupported => None` arm (377) | `probe_display_backend()` (347-361) returns only `CgVirtualDisplay` or `AirPlayFallback`. The variant is unconstructible outside the enum's own `as_str`. |
| **`cd_virtual_display_available()`** (`virtual_display.rs:342`) | Called only from `probe_display_backend`'s dead branch (353), where its result is logged and then discarded. |
| **`device.cpp:40-42`, the `else` arm of `OnProcessMixedOutput`** | Byte-identical to the `if` arm. Either dead (if `ch` is always 2) or wrong (if it is not). |
| **`ShmHeader::overruns`, `reserved0/1/2`** (`shm_ring.hpp:28-33`) | Zeroed once, never read, never written. |
| **`install_drivers` / `remove_drivers`** (`mod.rs:456-466`) | Return `true` unconditionally; the only caller is the frontend's Windows-shaped recovery path. |
| **`resync_streamers()` on the CGVirtualDisplay tier** (`device_reporter.rs:109`) | `sync_streamers` -> `display_slot_holder` returns `None` immediately when `max_concurrent_displays()` is `None`, which is always true on this tier; the desired set is unchanged, so the whole thread is a no-op with three lock acquisitions. |
| **`remove_all_displays`'s thread hop** (`mod.rs:508-511`) | `std::thread::spawn(...).join()` blocks the caller exactly as a direct call would. |
| **`cli_adapters()`** (`networking.rs:199-273`) | Not dead, but a 75-line copy of `get_network_adapters` that has already diverged (no hosted-network naming). |
| **`tools/hal_test.c`, `tools/shmcheck.c`** | Deliberately standalone diagnostics with `clang` one-liners in their headers; not dead, but not built by anything either. Worth a note in the README so they are not mistaken for build inputs. |
| **`lib.rs:337`** `//            get_devices,` | Commented-out command registration. |

---

## 14. Capability probing

**Correct (probe, not version):**
- `cg_virtual_display_available()` (`virtual_display.rs:334`) — `AnyClass::get` on all four class names,
  cached in a `OnceLock`. This is the doctrine done right, and correctly notes that the classes predate
  10.15 (10.14.4+), so no version number appears.
- `cd_virtual_display_available()` (342) — `dlsym`.
- `preflight_screen_capture()` / `request_screen_recording()` (`permissions.rs:98, 164`) — `dlsym`.
- `objc_exception::install_logger()` — `dlsym`, and every selector it sends is `respondsToSelector:`-guarded.

**Defects (version number where a probe belongs):**
1. **`screencapturekit_available()`** (`macos_utils/streamer/mod.rs:79-82`) is `macos_at_least(12, 3)`, i.e.
   a **`sw_vers` subprocess fork and a string comparison**. `CLAUDE.md`'s own rule is "Probe by class
   lookup, never by version number", and `sck::probe_screen_recording` does `AnyClass::get(c"SCShareableContent")`
   one line later — the correct probe is right there. This gates the *capture* tier, the *audio-probe*
   choice inside `probe_screen_recording`, and the compatibility report. It also **fails open**
   (`None => true`, line 76) and is **not cached**, so it forks a process on every call — including five
   times a second while the permission gate is open.
2. **`compatibility.rs:37`** `macos_at_least(10, 15)` is the only `severity: "blocking"` check in the whole
   report — a version string, not a capability.
3. **`os_version_string()`** (`compatibility.rs:17-32`) forks `sw_vers` twice more per report, uncached.

**Missing tiers:**
- **There is no capture tier in `CompatibilityReport`.** `audio_backend` and `display_backend` are reported;
  the SCK-vs-CGDisplayStream choice — the one with the biggest latency and quality consequence — is not,
  and is made silently inside `start_capture` with a `teprintln!` on fallback.
- **There is no HostAP capability tier.** `start_hosted_network` calls a private SPI that does not exist on
  Apple Silicon and on several macOS releases, with no `respondsToSelector:` probe and no report to the UI.
  The correct shape is a `probe_hostap_backend()` alongside the display and audio probes.
- **`DisplayBackend::AirPlayFallback` is reported as a string but carries no `UnsupportedApi` entry**, so the
  UI never tells the user that this tier is capped at one display (`airplay/mod.rs:390`,
  `max_concurrent_displays() -> Some(1)`) until they try to connect a second device.
- **There is no "cannot create a display" tier.** `DisplayBackend::Unsupported` exists but is unreachable, so
  a Mac where both backends fail falls through to `AirPlayFallback` and fails later, at join time.
- **`accessibility_api_enabled()`** (`permissions.rs:44`) probes for `kAXErrorAPIDisabled` (-25211), which is
  a pre-10.9 "Enable access for assistive devices" concept. On any supported macOS this always returns
  `true`, so `accessibility_trusted()` is effectively just `AXIsProcessTrusted()` at the cost of a
  0.5 s-timeout AX round trip five times a second.

---

## 15. Questions and cross-cutting concerns

1. **The permission gate is the top item.** The fix is a two-line reordering in
   `screen_recording_granted()` plus copy that tells macOS users a relaunch is required. But it interacts
   with territory I could not verify: **is `cgds::probe_screen_recording()` a valid TCC probe on 10.15?**
   If CGDisplayStream delivers `FrameComplete` for an unauthorized process (wallpaper-only frames), then
   removing the preflight short-circuit trades a permanently-closed gate for a permanently-latched
   *false positive* — the gate opens and the client gets a blank desktop. Someone with the 10.15 dev box
   must measure this before the fix lands. Filed as a speculative finding.
2. **Should Accessibility block startup at all?** It gates only per-device opt-in remote input. Making it
   optional is a one-line change with a large compatibility payoff, but it needs a product decision and a
   place to raise the prompt later (the Remote Control toggle).
3. **`activate_mode` inside `DISPLAY_CORRELATION_LOCK`.** Up to 3 s of 50 ms-granularity polling is held
   under the lock that serializes all display creation. Territory owning `streamer/server.rs` should
   confirm whether the settle wait needs to be inside the lock at all; if not, this is the cheapest
   join-latency win on macOS. The event-driven replacement is
   `CGDisplayRegisterReconfigurationCallback` (10.3+, dyld-safe).
4. **The mode ladder never reaches macOS.** `create_display_with_modes`'s default discards `extra_modes`,
   and `create_cg_virtual_display` hardcodes `setHiDPI(0)`. Whoever owns the DPR work should decide whether
   macOS adopts the ladder (implement the override) or the doctrine in `CLAUDE.md` is amended to say macOS
   reconfigures live. Either way the current behaviour — always born at 1x, then a full mode switch — is
   the worst of both.
5. **IPv6 is enumerated and never served** (`mod.rs:588`, `parse::<Ipv4Addr>().ok()`). The same filter is in
   `windows_utils/mod.rs`, so this is a cross-territory decision about `Config.bind_ip: Ipv4Addr`. On an
   IPv6-only network the app binds nothing and says nothing.
6. **`sync_streamers` has four callers and no debounce** (adapter change at 1 Hz, join, leave, port change),
   and it spawns a full tokio runtime per adapter IP. Whoever owns `streamer/mod.rs` should say whether one
   runtime shared across adapters is feasible; three runtimes on a Wi-Fi + Ethernet + hotspot Mac is a lot
   of threads for a latency-sensitive app.
7. **`sw_vers` forking.** `screencapturekit_available()` is called from the capture path and from the
   permission poll; replacing it with `AnyClass::get(c"SCStream")` in a `OnceLock` is a small change that
   touches the SCK territory, not mine. Flagging it here because the permission poll is what makes it hot.
8. **The whole 10.15-12.x audio tier is unreachable in a shipped build** — `install_audio_driver` resolves
   `resources/ScreenExtendAudio.pkg`, which `tauri.macos.conf.json` does not bundle and which no script or
   CI job produces at that path. Combined with the missing libASPL fetch script, a fresh clone cannot even
   build the driver. This needs a build/CI owner, not a code fix.
9. **Driver observability.** The AudioServerPlugIn has no logging of any kind. A HAL plug-in that cannot be
   read from `log stream` is not debuggable in the field, and the two most interesting failures
   (`shm_open` denied, a non-stereo mix) are both silent.
10. **Is `Info.plist` actually merged into the bundle?** Nothing in the repo asserts it, and the failure
    mode (missing `NSAudioCaptureUsageDescription`) is a hard process kill the first time a user enables
    System Audio. A one-line `PlistBuddy` assertion in release CI would settle it permanently.
11. **`hostap.m` should be deleted** and `CLAUDE.md:376` corrected from "ad-hoc" to "HostAP". Minor, but the
    doc error will keep sending readers to the wrong Apple API.
12. **Positive note worth carrying forward:** dyld safety in this territory is clean. Every post-10.15
    symbol is `dlsym`'d or runtime-resolved, there are no `extern static`s, and the private CoreGraphics
    classes are correctly probed before instantiation. The one runtime-resolution gap is
    `hosted_network.rs`'s two `msg_send!`s, which need `respondsToSelector:` for the same reason
    `extern_class!` needs `AnyClass::get`.

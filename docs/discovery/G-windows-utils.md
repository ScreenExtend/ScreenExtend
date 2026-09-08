# G — `windows_utils/` (excluding `streamer/` and `windows_capture/`)

Territory: `src-tauri/src/windows_utils/` — `mod.rs`, `audio/`, `driver_ipc/`, `virtual_display.rs`,
`hosted_network.rs`, `networking.rs`, `compatibility.rs`, `permissions.rs`, `device_reporter.rs`.
Every file read in full. `audio/test/` and the `streamer/` + `windows_capture/` subdirectories belong
to other agents and are only cited where they anchor a claim.

---

## 1. Files and purpose

**`mod.rs` (814 lines)** — the Windows backend's public surface. Declares the nine submodules, defines
`AppState` (the same 22-field struct macOS has), the `StreamerHandle` wrapper around an
`axum_server::Handle`, and `TauriCloudStatusSink` (bridges `CloudState` changes to the
`CloudStatusChange` / `SessionIdChange` Tauri events). Holds two free functions used at startup —
`set_display_topology_extend` (QueryDisplayConfig → `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)`) and
`focus_main_window` (the `AttachThreadInput` foreground-stealing dance) — the `setup` command, 26
further Tauri commands, and `sync_streamers`, which reconciles one axum streamer thread per adapter
IP against `state.network_adapters`.

**`audio/mod.rs` (16 lines)** — re-exports the five audio submodules and exposes exactly one entry
point, `start_capture() -> Result<AudioCapture>`, delegating to `loopback::start()`. This is the
whole platform contract `streamer/platform.rs::start_audio_capture` dispatches into.

**`audio/loopback.rs` (467 lines)** — the WASAPI loopback capture engine. `start()` spawns a named
`audio-loopback` OS thread and blocks on a `std::sync::mpsc` handshake for the negotiated
`AudioFormat` + Opus lookahead. `run()` is the thread body: COM MTA, MMCSS "Pro Audio", enumerator,
`IMMNotificationClient` registration, Opus encoder construction, `acquire()`, then an event-driven
loop that drains the capture buffer into a reusable `Vec<f32>` accumulator, cuts it into 5 ms Opus
frames, and pushes `AudioPacket`s down an unbounded crossbeam channel. `acquire()` starts the silent
render companion *first*, then negotiates the mix format (fast path verbatim, else engine
AUTOCONVERTPCM to 48 kHz stereo f32). `reacquire()` rebuilds a session on device change or
`AUDCLNT_E_DEVICE_INVALIDATED`.

**`audio/silence.rs` (155 lines)** — the mandatory silent render companion. A second OS thread opens
an `IAudioRenderClient` on the same default endpoint, pre-rolls the whole buffer with
`AUDCLNT_BUFFERFLAGS_SILENT`, and thereafter refills whatever `GetCurrentPadding` says is free on
every event. It is the clock source that makes the loopback event fire on an idle host. Owned by
`Session`, stopped by `Drop`.

**`audio/format.rs` (200 lines)** — `WAVEFORMATEX`/`WAVEFORMATEXTENSIBLE` parsing into a `MixFormat`
(`SampleKind` ∈ F32/I16/I24In3/I32), plus `convert_to_stereo_f32` with a fast path and a BS.775
downmix for 1/2/3/4/6/8-channel layouts. The downmix half is unreachable in production (see §13).

**`audio/device.rs` (98 lines)** — `IMMDeviceEnumerator` creation, default render endpoint lookup, and
`DeviceNotifier`, an `IMMNotificationClient` COM object whose only live callback,
`OnDefaultDeviceChanged`, forwards a `DeviceEvent::DefaultRenderChanged` over a crossbeam sender and
does nothing else. `NotifierRegistration` is the RAII un-registration.

**`audio/guards.rs` (91 lines)** — three RAII wrappers: `ComGuard` (`CoInitializeEx(MTA)` /
`CoUninitialize`), `EventHandle` (auto-reset `CreateEventW`, `WaitForSingleObject`, `CloseHandle`),
`MmcssGuard` (`AvSetMmThreadCharacteristicsW("Pro Audio")` / `AvRevertMmThreadCharacteristics`), plus
the hand-built UTF-16 `"Pro Audio"` literal.

**`driver_ipc/mod.rs` (13 lines)** — module wiring and `DEFAULT_PIPE_NAME = "virtualdisplaydriver"`.

**`driver_ipc/core.rs` (81 lines)** — the wire types: `Monitor { id, name, enabled, modes }`,
`Mode { width, height, refresh_rates }`, and four command enums (`DriverCommand` Notify/Remove/
RemoveAll, `RequestCommand::State`, `ReplyCommand::State`, `EventCommand::Changed`) plus two
`#[serde(untagged)]` union enums for decoding. No protocol version field.

**`driver_ipc/client.rs` (596 lines, ~200 of them tests)** — the async named-pipe client. One shared
`_Shared { NamedPipeClient, Notify, RwLock<Option<Arc<io::Error>>> }` behind an `Arc`, a spawned
receiver task, and a `broadcast::channel` of depth 10 fanning parsed messages to all clones. Messages
are JSON terminated by an `EOF` byte `0x04`. `request_state()` has a 5 s timeout; `notify`/`remove`/
`remove_all` are fire-and-forget writes with no timeout. `persist()` writes the monitor list to
`HKCU\SOFTWARE\VirtualDisplayDriver\data`.

**`driver_ipc/driver_client.rs` (663 lines)** — a stateful façade over `Client`. Caches the monitor
list locally, mirrors driver events into a `watch` channel, and offers the whole edit vocabulary
(`add`, `remove`, `set_enabled`, `add_mode`, `remove_mode`, `find_*`, `new_id`, `duplicate`) with
duplicate validation. Nothing reaches the driver until `notify()`.

**`driver_ipc/sync.rs` + `sync/client.rs` (391) + `sync/driver_client.rs` (384)** — blocking wrappers
over the async pair, backed by a `LazyLock<Runtime>` with **one worker thread**. Every method is
`RUNTIME.block_on(...)`. `EventsSubscription` adds a `catch_unwind`-guarded callback subscription with
`cancel`/`cancel_blocking`/`cancel_async`. Only `sync::DriverClient` is used by first-party code.

**`driver_ipc/mock.rs` (157 lines, `#[cfg(test)]`)** — an in-process named-pipe server for the client
tests. Contains two `&NamedPipeServer → *mut` casts to get `&mut` (`mock.rs:41-45`, `:108-112`).

**`virtual_display.rs` (139 lines)** — `WindowsVirtualDisplay`, the `VirtualDisplayController`
implementation. Holds `Mutex<Inner { DriverClient, owned: Vec<Monitor> }>`. `create_display_with_modes`
picks a free id, builds the dimension list (each requested mode plus its transpose, deduped, min 2px),
attaches the refresh-rate list (requested + 60 + 120), pushes the monitor and commits. `remove_display`
and `remove_all_displays` are the inverse. `max_concurrent_displays` is **not** overridden, so it
takes the trait default `None` (`streamer/session.rs:655-657`) — displays are uncapped, as doctrine
requires.

**`hosted_network.rs` (250 lines)** — the Wi-Fi hotspot. Two mutually exclusive backends chosen by
`supports_legacy_hosted_network_` (parses `netsh wlan show drivers` stdout): the legacy
`netsh wlan start hostednetwork` path (via an elevated self-invocation of the `hostednetwork`
subcommand), or the WinRT `WiFiDirectAdvertisementPublisher` + `NetworkOperatorTetheringManager` path.
Also `is_wifi_on` / `turn_on_wifi` over `Windows.Devices.Radios`.

**`networking.rs` (248 lines)** — WMI adapter enumeration. `get_network_adapters` (command) and
`cli_adapters` (near-verbatim copy for the CLI) run three `raw_query` calls against
`root\StandardCimv2` and join adapters → IPs → connection profiles by `InterfaceIndex`.
`watch_for_network_changes` spawns a 1 Hz polling thread; `apply` writes `AppState.network_adapters`,
calls `sync_streamers` and emits `NetworkChange`.

**`compatibility.rs` (174 lines)** — `check_system_requirements`. Reads the OS name/build from
`HKLM\...\Windows NT\CurrentVersion`, runs six probes on a scratch thread (WGC, DXGI, WinRT Radios,
Wi-Fi Direct, Tethering, WMI) and assembles a `CompatibilityReport`. `audio_backend` and
`display_backend` are string literals, not probes.

**`permissions.rs` (17 lines)** — three no-op commands (`check_permissions` → empty vec,
`request_permission` → `true`, `open_permission_settings` → `()`), present only to keep the command
surface identical with macOS.

**`device_reporter.rs` (86 lines)** — `TauriDeviceReporter` implements the `DeviceReporter` trait by
emitting `DeviceJoin` / `DeviceRemove` / `JoinAttemptsPaused` / `DeviceAudioOutputs` tauri-specta
events, overlaying any stored `DeviceOverride` onto the join payload.

---

## 2. Function catalog

### `mod.rs`
- `TauriCloudStatusSink::new_shared(app, status) -> SharedCloudStatusSink` — `:71`. Arc-wraps the sink.
- `CloudStatusSink::report(&self, state, detail)` — `:77`. Writes `(state, detail)` into the shared
  mutex, then emits `CloudStatusChange`; emit failure logged only.
- `CloudStatusSink::session_rotated(&self, session_id)` — `:89`. Emits `SessionIdChange`.
- `set_display_topology_extend() -> bool` — `:99`. Queries all display paths, detects clone topology
  (`active_targets ≥ 2 && active_sources < active_targets`) or an available-but-inactive target, and
  if either holds calls `SetDisplayConfig(SDC_TOPOLOGY_EXTEND | SDC_APPLY)`. Returns whether it
  applied. Nested helpers `source_key` / `target_key` build `(adapterId.LowPart, HighPart, id)` tuples.
- `focus_main_window(&WebviewWindow)` — `:166`. `ShowWindow(SW_RESTORE|SW_SHOW)`, then
  `AttachThreadInput` to the foreground thread around `BringWindowToTop` + `SetForegroundWindow`.
- `setup(app_handle) -> bool` — `:208` **[command, async]**. Idempotent (`try_state` guard);
  fire-and-forget `spawn_blocking(set_display_topology_extend)`; `spawn_blocking(new_shared).await`;
  builds and `manage`s `AppState`. Returns `false` with no diagnostics on any failure.
- `set_device_override(state, ip, scale, orientation, refresh_rate, video_scale, video_quality,
  control_enabled, dpr, audio_enabled)` — `:255` **[command]**. Clamps everything, computes whether a
  restart-worthy field changed, inserts, and bumps the reconfig epoch. Note `control_enabled` and
  `audio_enabled` deliberately do **not** count as restart-worthy.
- `refresh_device_stream(state, ip) -> bool` — `:346` **[command]**. No-op unless a live display
  exists; otherwise bumps the reconfig epoch.
- `set_device_audio_output(state, ip, device_id)` — `:358` **[command]**. Empty string clears.
- `get_device_audio_outputs(state, ip) -> AudioOutputsReport` — `:369` **[command]**.
- `remove_device_override(state, ip)` — `:378` **[command]**. Removes the override, sends
  `ByeReason::Kicked`, bumps the kick epoch, signals leave.
- `set_device_banned(state, token, ip, banned)` — `:387` **[command]**. Key is the token, falling back
  to the IP when the token is blank; on ban also kicks the live session.
- `set_device_approved(state, token, approved)` — `:409` **[command]**. Blank token → no-op.
- `set_disconnect_grace(state, seconds)` / `get_disconnect_grace(state) -> u32` — `:424` / `:437`
  **[commands]**. Clamped atomic.
- `get_display_capacity(state) -> DisplayCapacity` — `:445` **[command]**. `max` from
  `max_concurrent_displays()` (always `None` here), `in_use` from `live_display_count`, `backend`
  hardcoded `"IddDriver"`. **Not listed in `CLAUDE.md`'s command inventory** (it is registered at
  `lib.rs:374`).
- `set_turn_config(state, urls, username, credential)` / `get_turn_config(state) -> TurnConfig` —
  `:465` / `:491` **[commands]**. Comma-split, trimmed, empties dropped.
- `get_server_ports` / `set_server_ports(state, http_port, https_port) -> ServerPorts` — `:508` /
  `:515` **[commands]**. `0` means default; equal ports rejected; a change drains and hard-`shutdown`s
  every streamer, sleeps 300 ms, then `sync_streamers`.
- `get_disable_gpu_encode` / `set_disable_gpu_encode(state, disabled)` — `:563` / `:571`
  **[commands]**. Same drain/sleep/resync as the port change.
- `install_drivers(app) -> bool` / `remove_drivers(app) -> bool` — `:599` / `:625` **[commands]**.
  Byte-identical apart from the subcommand string; elevate a self-invocation via `elevated-command`;
  five `unwrap()`s each; always return `true`.
- `install_audio_driver` / `uninstall_audio_driver` / `audio_driver_status` — `:651` / `:657` / `:663`
  **[commands]**. All return `"unsupported"` (macOS-parity stubs).
- `set_legacy_volume_key_proxy(_enabled)` — `:669` **[command]**. Empty body (macOS-parity stub).
- `remove_all_displays(&SharedVirtualDisplay)` — `:671`. Spawns a thread and immediately joins it.
- `set_session_credentials(state, session_id, otp)` — `:678` **[command]**. Also resets the OTP
  limiter.
- `register_cloud_session(app, state, session_id)` — `:686` **[command]**. Builds a `Config` from
  `AppState` handles, spawns a `CloudClient`, stops any previous one.
- `unregister_cloud_session(state)` — `:720` **[command]**.
- `get_cloud_status(state) -> CloudStatusChange` — `:732` **[command]**.
- `sync_streamers(&AppState)` — `:740`. Collects desired `(ip, Ipv4Addr)` pairs from
  `network_adapters`, refreshes `local_ips`, `graceful_shutdown`s streamers whose IP disappeared,
  and spawns one OS thread per new IP running `Streamer::new(config).run_with_handle(handle)`.

### `audio/mod.rs`
- `start_capture() -> Result<AudioCapture>` — `:14`. Delegates to `loopback::start`.

### `audio/loopback.rs`
- `start() -> Result<AudioCapture>` — `:38`. Creates the unbounded packet channel, the diagnostics
  Arc and the stop flag; spawns the `audio-loopback` thread; blocks on `ready_rx.recv()` for the
  negotiated format; builds the `AudioStopFn` closure that sets the flag and joins the thread.
- `Session::drop` — `:91`. `client.Stop()`; the companion stops via its own `Drop`.
- `run(pkt_tx, diagnostics, stop, ready_tx)` — `:100`. The whole thread body (see §1). Fatal setup
  errors are reported through `ready_tx`; runtime errors are logged.
- `drain(&Session, &mut accum, &mut pending_flags, &AudioDiagnostics) -> Result<(), DrainError>` —
  `:274` (unsafe). Loops `GetNextPacketSize` → `GetBuffer` → append/convert → `ReleaseBuffer` until
  the buffer is empty. Silent packets append zeros and set `FLAG_SILENT`; discontinuity sets
  `FLAG_DISCONTINUITY`. `AUDCLNT_E_DEVICE_INVALIDATED` is mapped to `DrainError::DeviceInvalidated`
  from all three call sites.
- `acquire(&IMMDeviceEnumerator) -> Result<Session>` — `:344` (unsafe). Starts the companion,
  activates the client, negotiates the format, creates the event, gets `IAudioCaptureClient`, reads
  `GetBufferSize`, starts capture.
- `reacquire(&IMMDeviceEnumerator, &mut Session) -> Result<()>` — `:433` (unsafe). Builds the new
  session before dropping the old one.
- `target_wave_format() -> WAVEFORMATEX` — `:443`. 48 kHz / 2 ch / 32-bit IEEE float.
- `update_encode_percentiles(&[u64], &AudioDiagnostics)` — `:458`. `to_vec()` + `sort_unstable()` +
  nearest-rank p50/p99. Runs on the RT thread.

### `audio/silence.rs`
- `SilenceCompanion::start() -> Result<Self>` — `:21`. Spawns the `audio-silence` thread and blocks on
  a ready handshake.
- `SilenceCompanion::stop(self)` / `Drop` — `:47` / `:56`. Both set the flag and join.
- `run(stop, ready_tx)` — `:64`. Setup, report, `pump`, `client.Stop()`.
- `setup(&Arc<AtomicBool>) -> Result<(IAudioClient, IAudioRenderClient, EventHandle, u32, ComGuard)>` —
  `:80` (unsafe). COM MTA, enumerator, default endpoint, `Initialize` with the device mix format and
  default period, event handle, render service, `GetBufferSize`, whole-buffer silent pre-roll, `Start`.
- `pump(...)` — `:123` (unsafe). Event-driven refill of `buffer_frames - GetCurrentPadding()` frames
  of silence. Three unlogged `break`s.

### `audio/format.rs`
- `MixFormat::is_fast_path()` / `bytes_per_frame()` — `:33` / `:37`.
- `parse_mix_format(*const WAVEFORMATEX) -> Result<MixFormat>` — `:42` (unsafe). `read_unaligned`,
  handles EXTENSIBLE / IEEE_FLOAT / PCM tags, rejects 0 channels.
- `classify(GUID, bits)` / `classify_pcm(bits)` — `:76` / `:89`.
- `read_sample(kind, bytes, off) -> f32` — `:99`. Includes the 24-in-3 sign-extend
  `(raw << 8) >> 8`.
- `bytes_per_sample(kind) -> usize` — `:125`.
- `convert_to_stereo_f32(src, frames, fmt, out)` — `:133`. Fast path pushes clamped f32 per sample;
  slow path walks `block_align` strides through `downmix_frame`.
- `downmix_frame(src, base, bps, fmt) -> (f32, f32)` — `:160`. BS.775 coefficients (`M3DB` =
  `1/sqrt(2)`), LFE dropped for 5.1/7.1, unknown layouts averaged.

### `audio/device.rs`
- `create_enumerator() -> Result<IMMDeviceEnumerator>` — `:15`.
- `default_render_endpoint(&IMMDeviceEnumerator) -> Result<IMMDevice>` — `:20`. `eRender`/`eConsole`.
- `DeviceNotifier::OnDefaultDeviceChanged` — `:33`. Filters to `eRender`+`eConsole`, non-blocking
  `let _ = self.tx.send(...)`. The other four callbacks return `Ok(())` immediately.
- `NotifierRegistration::register(...) -> Result<Self>` / `Drop` — `:76` / `:90`.

### `audio/guards.rs`
- `ComGuard::init_mta()` / `Drop` — `:13` / `:20`.
- `EventHandle::new_auto_reset()` / `raw()` / `wait(timeout_ms) -> bool` / `Drop` — `:29`/`:35`/`:39`/`:45`.
- `MmcssGuard::register_pro_audio()` / `Drop` — `:58` / `:69`. **The return is checked** at the call
  site (`loopback.rs:113-121`): failure is logged and capture continues without MMCSS.

### `driver_ipc/client.rs`
- `Client::connect()` / `connect_to(name)` — `:44` / `:53`. Opens `\\.\pipe\{name}` in `PipeMode::Byte`,
  spawns the receiver task, returns the client + a broadcast receiver.
- `Client::notify(&[Monitor])` / `remove(&[Id])` / `remove_all()` — `:87` / `:95` / `:103`.
- `Client::request_state() -> Result<Vec<Monitor>, RequestError>` — `:114`. Resubscribes, sends, then
  waits up to 5 s for a `ReplyCommand::State`, skipping `Lagged`.
- `Client::receive_events() -> impl Stream<...>` — `:158`. Filters the broadcast to events, dropping
  `Lagged`.
- `Client::persist(&[Monitor])` — `:175`. Writes JSON to `HKCU\SOFTWARE\VirtualDisplayDriver`.
- `Client::clone` / `Drop` — `:204` / `:213`. Drop signals `abort_receiver` only when
  `Arc::strong_count == 2`.
- `send_command(&NamedPipeClient, &impl Serialize)` — `:221`. JSON + `EOF`, `writable().await` then
  `try_write` in a retry loop with no timeout and an `unreachable!()` arm.
- `receive_command(&NamedPipeClient, &Sender, &Notify)` — `:267`. 4 KiB read buffer into a growing
  `recv_buf`, splits on `EOF`, decodes, broadcasts, drains.
- `error` module — `:320`. Seven error types; two `unreachable!()` conversions for the
  `Encode` variant.

### `driver_ipc/driver_client.rs`
- `DriverClient::new()` / `new_with(name)` — `:27` / `:34`. Connect, `request_state()` (5 s timeout),
  seed a `watch` channel, spawn the event-mirroring task.
- `find_id(query)` — `:82`. Name takes precedence over id.
- `refresh_state()` — `:103`. Clones from the `watch` borrow.
- `receive_events()` / `monitors()` / `find_monitor*` (5 variants) — `:118`/`:126`/`:172`-`:252`.
- `set_monitors` / `replace_monitor` / `add` / `remove` / `remove_query` / `remove_all` /
  `set_enabled` / `set_enabled_query` / `add_mode` / `add_mode_query` / `remove_mode` /
  `remove_mode_query` — local state edits with duplicate validation; none touch the driver.
- `notify()` — `:164`. The only method that writes to the pipe.
- `new_id(preferred)` — `:269`. **Inverted logic for the `preferred_id` case**: it returns `None` when
  the id is *free* and `Some(id)` when it is *taken* (`:276-282`). Unused by first-party code
  (`virtual_display.rs` computes its own id at `:67-69`), so it is latent.
- `duplicate()` — `:550`. Shares the connection, forks the cached state.
- `mons_have_duplicates` / `mon_has_duplicates` / `mode_has_duplicates` — `:559`/`:573`/`:593`. O(n²)
  scans over small lists.

### `driver_ipc/sync/client.rs`, `sync/driver_client.rs`
Pure `RUNTIME.block_on` mirrors of every async method, plus `EventsSubscription`
(`start_subscriber` `:110`, `cancel` `:166`, `cancel_blocking` `:193`, `cancel_async` `:207`,
`Drop` `:223`) which wraps the callback in `panic::catch_unwind` and reports the payload through a
oneshot. `sync/client.rs:64` contains `.expect("Stream should never finish")`.

### `virtual_display.rs`
- `Inner::commit(Vec<Monitor>) -> Result<(), String>` — `:16`. `set_monitors` → store → `notify()`.
- `WindowsVirtualDisplay::new_shared() -> Option<SharedVirtualDisplay>` — `:33`. `DriverClient::new()`,
  clear any pre-existing monitors, wrap in `Arc<Mutex<_>>`. Errors discarded.
- `create_display(name, w, h, rr)` — `:47`. Delegates with no extra modes.
- `create_display_with_modes(name, w, h, rr, extra_modes)` — `:57`. See §1.
- `remove_display(id)` / `remove_all_displays()` — `:122` / `:131`.

### `hosted_network.rs`
- `start_wifi_direct_(name, password, success_tx) -> Result<WiFiDirectAdvertisementPublisher>` — `:21`.
  Requires the tethering state to be `2` (Off) before starting; registers a `StatusChanged` handler
  that reports only `Started`/`Aborted`; sets autonomous group owner + legacy SSID/passphrase.
- `wifi_radios() -> Vec<Radio>` — `:76`. `RequestAccessAsync().join()` then `GetRadiosAsync().join()`,
  filtered to `RadioKind::WiFi`. All errors → empty vec.
- `is_wifi_on() -> bool` / `turn_on_wifi() -> bool` — `:96` / `:104` **[commands]**.
- `supports_legacy_hosted_network_(app) -> bool` — `:124`. Spawns `netsh wlan show drivers` under
  `block_on` and string-matches English output. Never cached.
- `start_hosted_network(app, state, name, password) -> bool` — `:142` **[command]**.
- `stop_hosted_network(app, state) -> bool` — `:204` **[command]**. Legacy branch contains
  `.unwrap()`.
- `is_hosted_network(app, state) -> bool` — `:231` **[command]**. Legacy branch spawns two processes;
  modern branch reads a bool from `AppState`.

### `networking.rs`
- `hosted_network_name(&AppHandle) -> String` — `:48`. Reads `config.json` →
  `hostedNetworkCredentials.name`, default `"ScreenExtend"`.
- `get_network_adapters(app, state) -> Vec<NetworkInfo>` — `:63` **[command]**. See §1; every failure
  returns an empty vec.
- `cli_adapters() -> Vec<NetworkInfo>` — `:139`. ~70 lines duplicated from the above, minus the
  hosted-network naming.
- `watch_for_network_changes(app)` — `:214` **[command]**. Spawns the 1 Hz poller.
- `apply(&AppHandle, Vec<NetworkInfo>)` — `:240`. Store, `sync_streamers`, emit `NetworkChange`.

### `compatibility.rs`
- `Probes::optimistic()` — `:23`. All-true fallback used when COM init or the probe thread fails.
- `read_os_info() -> (String, u32)` — `:35`. Registry read of ProductName/DisplayVersion/CurrentBuild.
- `run_probes() -> Probes` — `:64`. Spawns a thread, inits COM, tests a `root\StandardCimv2`
  connection, runs four capability probes, joins.
- `wgc_supported()` — `:93`. `UniversalApiContract` ≥ 8 **and** `GraphicsCaptureSession::IsSupported`.
- `dxgi_supported()` — `:102`. `CreateDXGIFactory1::<IDXGIFactory1>().is_ok()`.
- `type_present(name)` — `:106`. `ApiInformation::IsTypePresent`.
- `check_system_requirements() -> CompatibilityReport` — `:112` **[command]**.

### `device_reporter.rs`
- `TauriDeviceReporter::new_shared(app, overrides) -> Arc<Self>` — `:20`.
- `report_join(DeviceInfo)` — `:26`. Builds `Device::defaults`, overlays the stored override
  (clamping `dpr` to `max_dpr`), emits `DeviceJoin`.
- `report_remove(ip)` — `:43`. Emits a `DeviceRemove` carrying a *fully blank* `Device` except the IP.
- `report_join_attempts_paused(secs)` — `:59`. Emits `JoinAttemptsPaused`.
- `report_audio_outputs(ip, report)` — `:68`. Emits `DeviceAudioOutputs`.

### `permissions.rs`
Three no-op commands at `:5`, `:11`, `:17`.

---

## 3. Threads

| # | Spawn site | Kind | Priority / QoS | Lifetime | Owns |
|---|---|---|---|---|---|
| 1 | `loopback.rs:47-50` `"audio-loopback"` | `std::thread::Builder` | **MMCSS "Pro Audio"** (`loopback.rs:113`), COM **MTA** | Started on the first audio subscriber; joined by the `AudioStopFn` closure (`loopback.rs:66-71`) | `IAudioClient`, `IAudioCaptureClient`, the `EventHandle`, the Opus encoder, the `accum` Vec, the `SilenceCompanion` |
| 2 | `silence.rs:26-29` `"audio-silence"` | `std::thread::Builder` | Normal, COM MTA. **No MMCSS** | Owned by `Session`; joined by `SilenceCompanion::Drop` | render `IAudioClient` + `IAudioRenderClient` |
| 3 | `networking.rs:215` (unnamed) | `std::thread::spawn` | Normal, COM (`COMLibrary::new`) | **Never exits.** No stop flag, no handle kept, no duplicate guard | the WMI connection per iteration |
| 4 | `compatibility.rs:65` (unnamed) | `std::thread::spawn` + immediate `.join()` | Normal, COM | One `check_system_requirements` call | the probe COM apartment |
| 5 | `mod.rs:805-809` — one per adapter IP | `std::thread::spawn` | Normal | Until `handle.shutdown()` / `graceful_shutdown` from `sync_streamers`, `set_server_ports` or `set_disable_gpu_encode` | a whole `Streamer` (which itself builds a multi-thread tokio runtime with `platform::tune_transport_thread`) |
| 6 | `mod.rs:673` | `std::thread::spawn` then `.join()` | Normal | One `remove_all_displays` call | nothing |
| 7 | `driver_ipc/sync.rs:10-16` | `LazyLock<Runtime>`, **1 worker thread** | tokio default | Process lifetime | every driver-IPC async task |
| 8 | `driver_ipc/client.rs:73` | `task::spawn` on the ambient runtime (#7 for sync callers) | tokio | Until abort/EOF | the pipe receiver loop, `recv_buf` |
| 9 | `driver_ipc/driver_client.rs:43` | `task::spawn` | tokio | Until the event stream ends | the `watch` sender mirroring driver state |
| 10 | `sync/client.rs:123` (`EventsSubscription`) | `RUNTIME.spawn` | tokio | Until cancelled or the callback panics | the user callback |
| 11 | `mod.rs:212` | `spawn_blocking`, handle **dropped** | Tauri blocking pool | Detached; result never observed | the `SetDisplayConfig` call |
| 12 | `mod.rs:214` | `spawn_blocking`, awaited | Tauri blocking pool | One `setup` | `WindowsVirtualDisplay::new_shared` |
| 13 | COM-owned: `DeviceNotifier` callbacks (`device.rs:33`) | not ours — the MMDevice notification thread | — | Registration lifetime | only a crossbeam `Sender` |

Threads 1 and 2 are the real-time pair. Thread 13 is the one callback context that must never block;
it correctly does nothing but a non-blocking `send`. Thread 3 is the one that should not exist in its
current form (§11, findings).

---

## 4. Locks

**`std::sync::Mutex`**
- `AppState.stop_hosted_network` (`mod.rs:42`) — guards an `Option<Box<dyn Fn()>>`. Taken in
  `hosted_network.rs:187` (write) and `:218` (read, **inside an `if let` scrutinee** —
  `if let Some(ref stop_func) = *state.stop_hosted_network.lock().unwrap() { stop_func(); }`). The
  guard lives for the whole block and the callback it invokes takes a *second* mutex
  (`wlan_hosted_network_helper`, `hosted_network.rs:189`) and calls into WinRT. No `.await` inside, so
  it is sound today — but this is exactly the temporary-lifetime shape the project rule warns about,
  and it is one `async` refactor away from being a bug.
- `AppState.hosted_network_running` (`mod.rs:43`) — cold path. `hosted_network.rs:178-179` and
  `:183-184` lock it twice in consecutive statements to write then read the same literal.
- `AppState.network_adapters` (`mod.rs:44`) — written in `networking.rs:243` as a bare assignment
  statement (guard dropped at the `;`), then re-locked immediately by `sync_streamers`
  (`mod.rs:742`). Correct only because it is an assignment and not an `if let`/`match` scrutinee.
- `AppState.streamers` (`mod.rs:46`) — `mod.rs:549`, `:587`, `:760`. Held across
  `handle.shutdown()` / `graceful_shutdown(Some(1s))` and across the whole `sync_streamers` spawn
  loop, so a slow shutdown blocks the network watcher's next `apply`. Cold path.
- `AppState.device_overrides` — `mod.rs:324` (short), `mod.rs:379`, and **inside the join hot path**
  at `device_reporter.rs:29` (`if let Some(o) = self.overrides.lock().unwrap().get(&ip)` — again the
  scrutinee-temporary shape; the block does field copies only, no await, so sound).
- `AppState.banned_devices` / `approved_devices` — `mod.rs:394`, `:402`, `:414`, `:417`. Cold.
- `AppState.user_turn` — `mod.rs:477`, `:492`. Cold.
- `AppState.cloud` — `mod.rs:711`, `:721`. Held across `prev.stop()`.
- `AppState.cloud_status` (`Arc<Mutex<(String,String)>>`) — `mod.rs:78` (in `report`, called from the
  cloud client's task), `:708`, `:726`, `:733`.
- `WindowsVirtualDisplay.inner` (`virtual_display.rs:29`) — **held across `RUNTIME.block_on`** for the
  whole named-pipe round trip in `create_display_with_modes` (`:65`→`:118`), `remove_display` (`:123`)
  and `remove_all_displays` (`:132`). Not an `.await`, so the compiler is happy, but it is a blocking
  wait under a blocking lock with no write timeout. On the join path.
- `hosted_network.rs:186` `Arc<Mutex<WiFiDirectAdvertisementPublisher>>` — locked inside the stop
  closure, which is itself called under the `stop_hosted_network` guard (nested locks, fixed order,
  no cycle).

**`tokio::sync::RwLock`**
- `_Shared.receive_error: RwLock<Option<Arc<io::Error>>>` (`driver_ipc/client.rs:35`) — the second of
  the repo's two intentional async locks. Written once by the receiver task on terminal error
  (`:77`), read in `request_state`'s `RecvError::Closed` arm (`:130`). Genuinely held across
  `.await`, correctly async.

**`tokio::sync::watch` / `broadcast` / `Notify`** — `driver_client.rs:39` (`watch`),
`client.rs:68` (`broadcast`, depth 10), `client.rs:60` (`Notify` for receiver abort).

**Atomics**
- `AppState.disable_gpu_encode: AtomicBool` — `Relaxed` everywhere (`mod.rs:243`, `:566`, `:574`,
  `:703`, `:797`). Read once per streamer start, so `Relaxed` is fine.
- `AppState.disconnect_grace` — `Relaxed` atomic (`mod.rs:431`, `:440`).
- `loopback.rs` `stop: Arc<AtomicBool>` — `Relaxed`, checked once per loop iteration (`:175`).
- `silence.rs` `stop: Arc<AtomicBool>` — `Relaxed`, checked twice per iteration (`:130`, `:132`).
- All `AudioDiagnostics` counters — `Relaxed` (`loopback.rs:150-158`, `:181`, `:203`, `:316`, `:329`,
  `:465-466`).

**Hot-path verdict:** the only locks on a per-frame/per-packet path are `device_overrides` on join
(once per device) and `virtual_display.inner` on create/remove (once per device). The audio RT thread
takes **no** `Mutex` in the drain/encode path — except transitively through `tprintln!`'s logbus mutex
every 2 s (§9, findings).

---

## 5. Buffers and copies

Audio, per ~10 ms WASAPI wake (assuming the default engine period; unverified per machine):

1. **WASAPI engine buffer → `pdata`** (`loopback.rs:297-303`) — zero-copy; `GetBuffer` hands back a
   pointer into the engine's shared buffer. `std::slice::from_raw_parts(pdata, bytes)`
   (`loopback.rs:323`) borrows it.
2. **`pdata` → `accum: Vec<f32>`** (`loopback.rs:324` → `format.rs:133-141`) — **CPU copy with
   per-sample work**. The "fast path" is not a memcpy: it is
   `for c in src.chunks_exact(4) { out.push(f32::from_le_bytes(...).clamp(-1.0,1.0)) }`, i.e. ~960
   iterations of load/clamp/push per wake. `out.reserve(n)` amortizes the allocation; `accum` is
   reused across iterations (`loopback.rs:168`, capacity `FRAME_INTERLEAVED * 16` = 7680 f32 = 30 KiB).
3. **Silence** (`loopback.rs:319`) — `accum.resize(len + n, 0.0)`, a memset, no source read.
4. **`accum` slice → Opus** (`loopback.rs:225-227`) — the encoder borrows `&accum[start..start+480]`;
   whether libopus copies internally is its business.
5. **Opus output → `Bytes`** (`loopback.rs:247`) — `Bytes::copy_from_slice(encoded)`: **one heap
   allocation + memcpy per 5 ms packet**, i.e. **200 allocations/second** of a few dozen bytes each.
   This is the documented, accepted per-packet copy.
6. **`accum` compaction** (`loopback.rs:255-257`) — `accum.drain(0..full*480)`, a `memmove` of the
   residue (0–479 f32) to the front, every iteration.
7. **Packet → channel** (`loopback.rs:249`) — moves the `Bytes` (refcount, no data copy) into the
   unbounded crossbeam queue.
8. **Diagnostics** (`loopback.rs:462`) — `samples.to_vec()`: a **32 KiB allocation every 2 s on the RT
   thread**, plus a `format!` string from `summary()`.

Silence companion, per wake: `render.GetBuffer(avail)` returns a pointer that is immediately released
with `AUDCLNT_BUFFERFLAGS_SILENT` (`silence.rs:143-151`) — the buffer is **never written**, the flag
tells WASAPI to treat it as zeros. Genuinely zero-copy.

Driver IPC, per display create/remove: `Vec<Monitor>` cloned (`virtual_display.rs:66`, `:124`),
cloned again into `set_monitors` (`driver_client.rs:137` `monitors.to_owned()`), cloned a third time
into `DriverCommand::Notify` (`client.rs:88` `monitors.to_owned()`), then serialized to a `Vec<u8>`
(`client.rs:227`). Four copies of a list that is at most a handful of monitors — irrelevant in
absolute terms, on a once-per-join path.

Driver IPC receive: a fixed 4 KiB `buf` read into a growing `recv_buf`
(`client.rs:272-273`, `:284`), then `recv_buf.drain(..offset)` per message batch (`:316`) — another
`memmove`.

Network enumeration, per second: three `Vec`s of deserialized WMI objects, two `HashMap`s, one
`String` clone per IP (`networking.rs:125`), plus a full `Vec<NetworkInfo>` clone on change
(`networking.rs:233`).

No video frame buffers live in this territory — capture/encode is `windows_utils/streamer/`.

---

## 6. Queues

| Queue | Site | Type | Depth | Blocking | Back-pressure |
|---|---|---|---|---|---|
| Audio packets, capture → `AudioHub` | `loopback.rs:39` | `crossbeam_channel::unbounded` | **unbounded** | producer never blocks | **none**; a send error is read as teardown (`:249-252`) and the thread returns |
| Device events, COM notifier → capture thread | `loopback.rs:131` | `crossbeam_channel::unbounded` | **unbounded** | non-blocking `send` in the COM callback (`device.rs:40`); `try_recv` once per loop (`:179`) | none; a burst is processed one-per-iteration, each triggering a full reacquire |
| Format handshake | `loopback.rs:43` | `std::sync::mpsc` | 1 use | `start()` blocks on `recv()` — **no timeout** | n/a |
| Companion ready handshake | `silence.rs:24` | `std::sync::mpsc` | 1 use | `start()` blocks on `recv()` — **no timeout** | n/a |
| Hotspot start result | `hosted_network.rs:174` | `std::sync::mpsc` | 1 use | `recv()` — **no timeout, and the local sender clone keeps it open forever** (`:175`) | n/a |
| Driver IPC commands, receiver → clients | `driver_ipc/client.rs:68` | `tokio::sync::broadcast` | **10** | non-blocking | drops oldest and yields `Lagged(n)`, which is **discarded** at `client.rs:129` and `:166` |
| Driver state mirror | `driver_client.rs:39` | `tokio::sync::watch` | 1 (latest wins) | non-blocking | conflation is intended |
| Subscription abort | `sync/client.rs:120` | `tokio::sync::mpsc` | 1 | `try_send` | full ⇒ "already cancelled" |
| Panic payload | `sync/client.rs:121` | `tokio::sync::oneshot` | 1 | — | — |
| Mock server commands | `mock.rs:34` | `broadcast` | 64 | test-only | — |

Every queue in this territory is either unbounded or conflating. The one bounded queue (broadcast,
depth 10) silently drops on overflow.

---

## 7. Syscall / FFI boundaries

**Win32 / COM — display**
- `GetDisplayConfigBufferSizes`, `QueryDisplayConfig`, `SetDisplayConfig` (`mod.rs:65-67`, called at
  `:90`, `:99`, `:151`). `SetDisplayConfig` returns a raw Win32 code checked against `0`
  (`mod.rs:158`), not an HRESULT.
- `DISPLAYCONFIG_PATH_ACTIVE` is redeclared locally as `0x0000_0001` (`mod.rs:70`) rather than taken
  from the `windows` crate — a hand-maintained constant.
- Window management: `IsIconic`, `ShowWindow`, `GetForegroundWindow`, `GetWindowThreadProcessId`,
  `GetCurrentThreadId`, `AttachThreadInput`, `BringWindowToTop`, `SetForegroundWindow`
  (`mod.rs:167-172`). `HWND(h.0 as _)` (`mod.rs:175`) casts a Tauri raw handle.

**Win32 / COM — audio**
- `CoInitializeEx(COINIT_MULTITHREADED)` / `CoUninitialize` (`guards.rs:14`, `:22`).
- `AvSetMmThreadCharacteristicsW` / `AvRevertMmThreadCharacteristics` (`guards.rs:62`, `:73`), with a
  hand-built UTF-16 NUL-terminated `"Pro Audio"` array (`guards.rs:80-91`).
- `CreateEventW` / `WaitForSingleObject` / `CloseHandle` (`guards.rs:30`, `:40`, `:49`).
- `CoCreateInstance(MMDeviceEnumerator)` (`device.rs:16`), `GetDefaultAudioEndpoint(eRender,eConsole)`
  (`device.rs:23`), `RegisterEndpointNotificationCallback` /
  `UnregisterEndpointNotificationCallback` (`device.rs:81`, `:94`).
- `#[windows::core::implement(IMMNotificationClient)]` (`device.rs:27`) — a **COM object implemented in
  Rust**, whose vtable is called from a thread the process does not own.
- `IMMDevice::Activate(CLSCTX_ALL)`, `IAudioClient::GetMixFormat` / `Initialize` / `SetEventHandle` /
  `GetService` / `GetBufferSize` / `Start` / `Stop` / `GetCurrentPadding`,
  `IAudioCaptureClient::GetNextPacketSize` / `GetBuffer` / `ReleaseBuffer`,
  `IAudioRenderClient::GetBuffer` / `ReleaseBuffer` — `loopback.rs` and `silence.rs` throughout.
- `CoTaskMemFree` (`loopback.rs:364`, `:370`; `silence.rs:99`) — manual lifetime management of the
  `GetMixFormat` allocation, with one leak path (§9).
- **Two stream flags are hand-declared** because the `windows` crate does not expose them:
  `STREAMFLAGS_AUTOCONVERTPCM = 0x8000_0000` and `STREAMFLAGS_SRC_DEFAULT_QUALITY = 0x0800_0000`
  (`loopback.rs:26-27`), and `AUDCLNT_E_DEVICE_INVALIDATED = 0x8889_0004u32 as i32`
  (`loopback.rs:30`).
- `std::ptr::read_unaligned` on `*const WAVEFORMATEX` and `*const WAVEFORMATEXTENSIBLE`
  (`format.rs:43`, `:51`) — the correct handling for a variable-length, potentially unaligned COM
  struct. The `WAVEFORMATEXTENSIBLE` read is only guarded by `tag == WAVE_FORMAT_EXTENSIBLE`, not by
  `cbSize >= 22`; a malformed driver format could over-read by up to 22 bytes.

**WinRT**
- `Windows.Foundation.Metadata.ApiInformation::IsApiContractPresentByMajor` / `IsTypePresent`
  (`compatibility.rs:94`, `:107`) — the one place capability probing is done properly.
- `Windows.Graphics.Capture.GraphicsCaptureSession::IsSupported` (`compatibility.rs:99`).
- `Windows.Devices.Radios.Radio::RequestAccessAsync` / `GetRadiosAsync`, `SetStateAsync`
  (`hosted_network.rs:77`, `:84`, `:116`) — `.join()` blocks the calling thread on the WinRT async op.
- `Windows.Devices.WiFiDirect.WiFiDirectAdvertisementPublisher` +
  `Windows.Networking.NetworkOperators.NetworkOperatorTetheringManager`
  (`hosted_network.rs:26-71`). `TetheringOperationalState(2)` is a **magic numeric variant**
  (`hosted_network.rs:30`) rather than the named enum.
- `Windows.Security.Credentials.PasswordCredential` (`hosted_network.rs:41`).

**DXGI** — `CreateDXGIFactory1::<IDXGIFactory1>()` (`compatibility.rs:103`), used only as a
liveness probe. The IDD adapter itself is never inspected here; how the driver's adapter surfaces in
DXGI is decided in `windows_utils/streamer/` (out of territory) — the display is created by name/id
over the pipe and the capture side finds it through the normal monitor enumeration.

**Registry (winreg)** — `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` read
(`compatibility.rs:36-43`); `HKCU\SOFTWARE\VirtualDisplayDriver` create/write
(`driver_ipc/client.rs:176-198`).

**WMI (`wmi` crate over COM)** — `COMLibrary::new()` / `assume_initialized()`,
`WMIConnection::with_namespace_path("root\\StandardCimv2")`, three `raw_query` calls
(`networking.rs:64-86`, `:140-167`; `compatibility.rs:66-74`).

**Named pipe** — `tokio::net::windows::named_pipe::ClientOptions` on
`\\.\pipe\virtualdisplaydriver` in `PipeMode::Byte` (`client.rs:54-58`). Note the comment at
`client.rs:229` says "the pipe is in message mode", contradicting the `Byte` mode requested.

**Process spawn** — `netsh` via `tauri_plugin_shell` (`hosted_network.rs:127`, `:162`, `:210`,
`:236`); the app's own exe via `elevated_command::Command` (`mod.rs:612-619`, `:638-645`;
`hosted_network.rs:155-159`). All must be enumerated in `capabilities/default.json`.

macOS/dyld exposure: none — this territory is entirely `#[cfg(windows)]`.

---

## 8. Panics

Reachable at runtime, ordered by how plausible the trigger is:

1. `mod.rs:619` / `:645` — `let _ = fincmd.output().unwrap();` **Cancelling the UAC prompt panics
   the command.** Also `mod.rs:603-606`, `:609`, `:629-632`, `:635`: four `unwrap()`s on resource-path
   resolution and `OsString → String` conversion (non-UTF-8 profile path).
2. `hosted_network.rs:214` — `.status().await.unwrap()` on `netsh wlan stop hostednetwork`. Panics if
   the process cannot be spawned (missing, or blocked by the shell allowlist).
3. `hosted_network.rs:152` — `.into_os_string().into_string().unwrap()` on `current_exe()`.
4. `sync/client.rs:64` — `.expect("Stream should never finish")` in `Client::receive_event`. The
   stream *does* finish when all clients drop, so this is only unreachable because no first-party code
   calls this method.
5. `driver_ipc/client.rs:379`, `:388` — `unreachable!("{:?}", e)` for `SendCommandError::Encode`.
   Serializing `Vec<Monitor>` cannot realistically fail, but it is an `unreachable!` on a
   `serde_json::Error`, not a compile-time impossibility.
6. `driver_ipc/client.rs:259` — `_ => unreachable!()` in `send_command`'s match. The `Ok(n)` arms are
   guarded by `written + n < len` and `written + n >= len`, which is exhaustive over `usize`, so this
   is genuinely dead — but it is one guard edit away from being live.
7. `driver_ipc/driver_client.rs:428` — `Err(_) => unreachable!()` in `add_mode`'s error mapping;
   `:480` `unreachable!("Mon must exist")`; `:538` `.expect("Monitor must exist")`. All rely on
   `find_id` and the subsequent lookup agreeing, which they do only because the state is not mutated
   in between.
8. `driver_ipc/driver_client.rs:286` — `.expect("failed to get a new ID")` on `(0..).find(...)`.
   Infallible in practice.
9. Every `.lock().unwrap()` in `mod.rs` / `hosted_network.rs` / `virtual_display.rs` — a panic
   anywhere under one of these guards poisons the mutex and turns every subsequent access into a
   panic. There is no poison recovery anywhere in the territory.
10. `mock.rs` (test-only) — `.unwrap()` / `.expect()` throughout, plus two
    `&NamedPipeServer as *const _ as *mut` casts (`:41-45`, `:108-112`) that create aliasing `&mut`
    references. Test-only, but UB by the letter of the rules.

Index panics: none — `format.rs:99-121`'s `bytes[off + n]` accesses are bounded by the
`base + bps*ch > src.len()` check at `format.rs:150`, and `update_encode_percentiles`'s index is
`.min(len-1)`-clamped (`loopback.rs:464`) after an `is_empty` guard.

---

## 9. Silent fallbacks and swallowed errors

**Silent capability/init failures**
- `virtual_display.rs:34` — `DriverClient::new().ok()?` discards the distinction between "driver not
  installed" and "driver timed out after 5 s", and logs nothing. `mod.rs:216-219` then folds that,
  plus any `spawn_blocking` panic, into `setup → false`. **The most consequential swallow in the
  territory.**
- `virtual_display.rs:36` — `let _ = client.notify();`
- `mod.rs:212` — `spawn_blocking(set_display_topology_extend)` result dropped; a panic inside would be
  invisible.
- `compatibility.rs:68`, `:90` — COM init failure or a probe-thread panic yields
  `Probes::optimistic()`, i.e. **"assume everything works"**, so the compatibility report claims full
  support precisely when it could not check.
- `compatibility.rs:98`, `:99`, `:107` — `.unwrap_or(false)` collapses "API says no" and "the query
  itself failed".
- `loopback.rs:132` — `NotifierRegistration::register(...).ok()` discards the error. If registration
  fails, default-device changes are never noticed and the only recovery left is
  `AUDCLNT_E_DEVICE_INVALIDATED`.
- `loopback.rs:411` — `client.GetBufferSize().unwrap_or(0)` reports 0 frames as a valid diagnostic.

**Silent degradation with no log**
- `silence.rs:137`, `:149`, `:152` — three bare `break`s end the companion thread. Nothing logs,
  nothing restarts it, and the capture loop's only symptom is a permanently un-signalled event
  (`loopback.rs:197`, whose `bool` return is discarded).
- `loopback.rs:313` — `let _ts_error = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR...;` computed and
  dropped. Prefixed with `_` so clippy stays quiet.
- `loopback.rs:295-303` — `devpos` and `qpcpos` are requested from `GetBuffer` and never read.
- `loopback.rs:229-232` — an Opus encode failure logs and `continue`s, **silently dropping the frame
  without setting `FLAG_DISCONTINUITY` on the next one**, so the client is never told there is a hole.
- `driver_ipc/client.rs:305-307` — undecodable messages `continue` with no log or counter.
- `driver_ipc/client.rs:129`, `:166` — `Lagged(n)` discarded in both the request and event paths, so
  a dropped `EventCommand::Changed` leaves `DriverClient`'s cached state stale with no signal.
- `driver_ipc/client.rs:283` — `Ok(0) => Err(io::Error::last_os_error())`: a clean EOF is reported
  with whatever `GetLastError` happens to hold, potentially "The operation completed successfully".
- `device.rs:40` — `let _ = self.tx.send(...)` (correct here: the COM callback must not block).
- `device.rs:93-95` — unregistration failure ignored in `Drop`.

**Silent empty results**
- `networking.rs:67`, `:73`, `:79`, `:85` and the four mirrors in `cli_adapters` — every WMI failure
  returns `Vec::new()` with no log. `apply` then treats that as "no adapters" and `sync_streamers`
  **shuts down every streamer**, dropping every connected device.
- `hosted_network.rs:79`, `:86` — `wifi_radios()` returns an empty vec on any error, making "no Wi-Fi
  hardware" and "the radio API failed" indistinguishable.
- `hosted_network.rs:132-137`, `:167-172`, `:241-246` — English-only string matching. On localized
  Windows every one of these silently answers "no"/"not started".

**Always-true returns**
- `mod.rs:620`, `:646` — `install_drivers` / `remove_drivers` return `true` unconditionally.
- `permissions.rs:11` — `request_permission` returns `true` for any key.

---

## 10. TODO / FIXME / HACK / XXX

Only two, both in `driver_ipc`:

- `driver_ipc/client.rs:166` —
  `Err(errors::BroadcastStreamRecvError::Lagged(_n)) => None, // TODO: Indicate lagged? (Maybe changing Item to Result<EventCommand, ...> is better?)`
- `driver_ipc/driver_client.rs:185` — `/// TODO: may leave client in an invalid state` (doc comment on
  `find_monitor_mut`; the `mons_have_duplicates(&self.state).ok()?` at `:199` detects the invalid
  state *after* the callback has already mutated it, and then returns `None` without rolling back).

No `FIXME`, `HACK` or `XXX` anywhere in the territory.

---

## 11. Latency path

**On the glass-to-glass video path:** nothing in this territory sits in the per-frame loop. Capture
and encode live in `windows_utils/streamer/` and `windows_capture/`. This territory contributes to
video latency only through **setup-time** and **background-load** effects:

| Stage | Where | Cost | Notes |
|---|---|---|---|
| Topology extend | `mod.rs:99-164` | one `SetDisplayConfig`, hundreds of ms, once | detached; races display init |
| Driver connect | `driver_client.rs:37` via `virtual_display.rs:34` | up to **5 s** (the `request_state` timeout) | once per process, under `spawn_blocking` |
| Display create | `virtual_display.rs:57-119` | one pipe round trip under a blocking mutex, **no write timeout** | **once per device join**, inside `server.rs`'s `DISPLAY_ATTACH_TIMEOUT` (5 s) + `LEAVE_SETTLE` (1.5 s) window |
| Streamer start | `mod.rs:805-809` | thread spawn + full tokio runtime + TLS cert load | once per adapter IP |
| **WMI poll** | `networking.rs:228-236` | **3 CIM queries + allocations, every 1 s, forever** | background jitter |
| **`netsh` spawn** | `hosted_network.rs:127` via `networking.rs:110` | **≥1 process creation per second, forever** on any machine with the Wi-Fi Direct virtual adapter | background jitter |
| Compat probe | `compatibility.rs:64-91` | COM init + 4 WinRT/DXGI probes, thread spawn + join | per UI call |

**On the audio path** (the territory's real latency contribution), per hop:

| Stage | Where | Estimated cost |
|---|---|---|
| WASAPI engine buffering | `Initialize(..., 0, 0, ...)` at `loopback.rs:363` / `:373-381` | one engine period, default ~10 ms ⇒ **+5 ms mean, +10 ms worst**. Never measured: `GetDevicePeriod` is not called, and the value logged as "period" is actually `GetBufferSize` (`loopback.rs:411`) |
| Event wait | `loopback.rs:197` | 0 when signalled; the 100 ms timeout is the starvation fallback |
| Convert to stereo f32 | `format.rs:134-141` | ~960 load/clamp/push iterations ≈ low single-digit µs |
| Accumulator residue | `loopback.rs:222`, `:255-257` | 0–4.99 ms, depending on whether the period is a multiple of 5 ms |
| Opus encode | `loopback.rs:227` | measured into `encode_p50_ns`/`encode_p99_ns`; CELT `RESTRICTED_LOWDELAY` at 5 ms is typically well under 1 ms |
| Opus algorithmic lookahead | reported via `lookahead_samples` (`loopback.rs:165`) and passed to the client | ~2.5 ms |
| `Bytes` copy + unbounded channel | `loopback.rs:247`, `:249` | ~1 µs; **queue depth is unbounded, so a stalled consumer converts directly into latency** |
| RT-thread diagnostics stall | `loopback.rs:259-263` | a 32 KiB alloc + 4096-element sort + logbus mutex + Tauri event emit, **every 2 s on the Pro Audio thread** |

Host-side audio capture→packet is therefore roughly **12–20 ms**, dominated by the un-negotiated
engine period.

**Serialization points**
- `start()` (`loopback.rs:52`) blocks the caller on `ready_rx.recv()` with **no timeout** until the
  audio thread has completed COM init, MMCSS registration, enumerator creation, Opus load, the
  companion's own full handshake (`silence.rs:31`, also untimed) and `acquire()`.
- `SilenceCompanion::start()` (`silence.rs:31`) is nested inside the above, so audio startup is two
  chained untimed handshakes.
- `WindowsVirtualDisplay.inner` serializes every display create/remove across all devices, and holds
  the lock across a `block_on` pipe round trip.
- `driver_ipc`'s runtime has **one worker thread** (`sync.rs:12`), so every driver operation in the
  process is serialized through it.
- `AppState.streamers` is held for the whole of `sync_streamers`, including `graceful_shutdown(1s)`,
  blocking the 1 Hz watcher's next tick.
- `setup`'s topology task is *not* serialized against display creation, which is the bug (§findings).

---

## 12. Stale references

- **`loopback.rs:362`** — `// Fast path (§4.4): 48 kHz float32 stereo — take the mix format verbatim.`
  and **`loopback.rs:369`** — `// (§4.4 option 1: AUTOCONVERTPCM). No userspace resampler needed.`
  `§4.4` refers to `AUDIO_NOTES.md`, which `CLAUDE.md` itself records as **not present in the tree on
  any branch** (it survives only on `sys-audio-winmac`, commit `420004a`). A reader cannot resolve
  these.
- **`loopback.rs:434`** — `// Replace with a placeholder-free swap: build the new one first, then drop
  the old.` reads as an instruction to a future editor, but describes what the next two lines already
  do. It should be a statement of intent ("build the new session before dropping the old so the
  endpoint is never left without a companion").
- **`driver_ipc/client.rs:229`** — `// This is required because the pipe is in message mode.` The pipe
  is opened with `PipeMode::Byte` (`client.rs:57`). The comment describes the opposite of the code;
  the single-write requirement it justifies is no longer justified by that reason.
- **`silence.rs:137`** — `// device invalidated; capture thread will re-acquire us`. Not true in
  general: the capture thread re-acquires on a *capture-side* invalidation or a default-device change,
  neither of which is implied by a render-side failure. See the companion-death finding.
- **`loopback.rs:96`** — `// companion stopped by its own Drop` is correct, but only because
  `SilenceCompanion` is a field of `Session`; worth keeping.
- **`CLAUDE.md` vs code** (discrepancies found, code wins):
  - The registered-command list omits **`get_display_capacity`**, which exists at `mod.rs:445` and is
    registered at `lib.rs:374`.
  - "48 kHz float32 stereo is the zero-copy fast path, everything else is `AUTOCONVERTPCM`'d /
    downmixed (ITU BS.775) / int→float" — the three are not alternatives; AUTOCONVERTPCM makes the
    downmix and int→float paths unreachable (§13), and the fast path is not zero-copy
    (`format.rs:137-139`).
  - "Current version: **0.5.4**" — `src-tauri/tauri.conf.json:5` says `0.6.3`.
  - `windows_utils/` is described as having `driver_ipc/` and `windows_capture/` as its extras; the
    repo map does not mention `permissions.rs` or `device_reporter.rs` at this level, though both
    exist and are part of the required per-OS surface.
- No references to AI tooling anywhere in the territory.

---

## 13. Dead code candidates

1. **The multichannel downmix and integer sample paths** — `format.rs:143-156` (the non-fast branch of
   `convert_to_stereo_f32`), `downmix_frame` (`:160-199`), the `I16`/`I24In3`/`I32` arms of
   `read_sample` (`:104-120`) and `bytes_per_sample` (`:125-131`).
   *Evidence:* `Session.mix` is assigned `effective_mix` (`loopback.rs:426`), and `acquire` produces
   only two possible values for it — the verbatim mix (taken only when it is already 48 kHz/2 ch/F32,
   `loopback.rs:358-360`) or a hardcoded `MixFormat { 48000, 2, F32, 8, 0 }` (`loopback.rs:395-401`).
   Both satisfy `is_fast_path()` (`format.rs:33-35`), so `convert_to_stereo_f32` always takes the
   fast branch. The only callers of the dead code are `windows_utils/audio/test/format.rs`.
2. **`DriverClient::new_id`** (`driver_client.rs:269-289`) — no first-party caller;
   `virtual_display.rs:67-69` computes its own free id inline. Its `preferred_id` branch is also
   logically inverted (returns `None` when the id is free), so reviving it would be a bug.
3. **`sync::Client`** (`driver_ipc/sync/client.rs`) as a whole — `driver_ipc/sync.rs:6` re-exports only
   `DriverClient`; `sync::Client` is reachable only through `super::client::EventsSubscription`. The
   `receive_event` / `add_event_receiver` / `request_state` / `persist` methods have no first-party
   caller, and neither does `EventsSubscription` itself (`virtual_display.rs` never subscribes to
   driver events).
4. **`DriverClient`'s query/edit vocabulary** — `find_id`, `find_monitor*` (5 variants),
   `replace_monitor`, `add`, `remove`, `remove_query`, `set_enabled`, `set_enabled_query`, `add_mode`,
   `add_mode_query`, `remove_mode`, `remove_mode_query`, `persist`, `duplicate`, `refresh_state`,
   `receive_events`. `virtual_display.rs` uses exactly three: `set_monitors`, `notify`, `remove_all`.
   This is a vendored general-purpose library surface; it should be marked as such rather than pruned
   piecemeal, but the maintenance cost is real (`driver_client.rs` is 663 lines to support three
   calls).
5. **`MixFormat::channel_mask`** (`format.rs:29`) — parsed from `WAVEFORMATEXTENSIBLE`
   (`format.rs:52`) and never read. `downmix_frame` assumes canonical channel order by count rather
   than consulting the mask, which is also a correctness gap for non-canonical layouts.
6. **`MixFormat::is_fast_path`** is called only from `convert_to_stereo_f32`; `acquire` re-implements
   the same predicate inline (`loopback.rs:358-360`) instead of calling it.
7. **`_stop` parameter** of `silence::setup` (`silence.rs:81`) — unused, `_`-prefixed.
8. **`permissions.rs`** — all three commands are no-ops. Intentional (macOS parity), not a candidate
   for removal, but worth a comment saying so.
9. `EventHandle::wait`'s return value (`guards.rs:39`) is never used by any caller
   (`loopback.rs:197`, `silence.rs:131`) — the function is effectively `wait_ignoring_result`.

---

## 14. Capability probing

**Where behavior is selected by version number instead of capability**
- `compatibility.rs:11,114` — `MIN_BUILD = 19041`, `os_supported = build_num >= MIN_BUILD`. A pure
  version gate, sitting three lines away from `wgc_supported()` / `dxgi_supported()`, which are real
  probes whose results are *not* used to decide support.
- `compatibility.rs:94-98` — `IsApiContractPresentByMajor("Windows.Foundation.UniversalApiContract", 8)`
  ANDed with the real `GraphicsCaptureSession::IsSupported()`. The contract check can only subtract
  from the true answer.

**Where a tier is claimed without being probed**
- `compatibility.rs:171-172` — `audio_backend: "wasapi"` and `display_backend: "IddDriver"` are
  literals. macOS probes and reports the tier it selected; Windows asserts one. The report will say
  `IddDriver` on a machine where the driver is not installed.
- `mod.rs:452` — the same `"IddDriver"` literal in `get_display_capacity`.

**Where a tier is missing entirely**
- **No encoder tier is reported.** The pipeline picks NVENC → Intel (same-adapter, then the
  `IntelCpu` bridge) → x264 at runtime, and `CompatibilityReport` has no field for it. A user on the
  software fallback has no way to learn that from the UI.
- **No driver-presence tier.** There is no `NotInstalled` state anywhere, even though it is the
  single most likely first-run condition and `install_drivers` exists to fix it.
- **No capture-backend tier is reported.** `wgc` and `dxgi` are probed and only turned into
  `UnsupportedApi` warnings (`compatibility.rs:119-136`); which one the pipeline will actually use is
  never stated.
- **The audio backend has exactly one tier and no probe.** `audio/mod.rs:14` unconditionally calls
  `loopback::start()`. There is no equivalent of macOS's `probe_audio_backend()`, so a machine where
  WASAPI loopback cannot be opened produces an error at first use rather than a reported tier.

**Where probing is done correctly** — `compatibility.rs:79-85` (`type_present` for WinRT Radios,
Wi-Fi Direct and Tethering) and `:102-104` (`dxgi_supported` by actually creating a factory) are
model examples; the problem is that their results are advisory rather than load-bearing.

**Where a capability is decided by parsing localized text** — `supports_legacy_hosted_network_`
(`hosted_network.rs:132-137`) and the three status readbacks. See the findings file.

---

## 15. Questions and cross-cutting concerns for the orchestrator

1. **A/V sync is asymmetric between platforms.** macOS stamps audio packets with a burst anchor plus a
   per-frame offset (`macos_utils/audio/mod.rs:315`); Windows stamps every packet with
   `host_now_ns()` at encode time (`loopback.rs:245`) and discards the QPC position it already
   fetches. Whoever owns `streamer/audio/` should decide whether the anchor+offset helper belongs in
   the shared module so both backends cannot drift apart again.
2. **Does the reconfig path recreate the virtual display, or reuse it?** The Windows IDD driver only
   honours modes registered at create time (`virtual_display.rs:87-101` registers the requested rate
   plus 60 and 120, and the DPR ladder's resolutions plus their transposes). If a live refresh-rate or
   DPR change reuses the display, rates outside {requested, 60, 120} silently do nothing. This
   spans this territory and `streamer/server.rs` (agent covering `dpr_mode_ladder` /
   `DISPLAY_CORRELATION_LOCK`), and I could not settle it from `windows_utils/` alone.
3. **Where does the DPI re-apply happen, and is it ordered after the resolution change?** The only
   `DISPLAYCONFIG_SET_DPI` code is in `windows_utils/streamer/capture.rs:289,383-393`, outside this
   territory. The project memory note says the DPI must be re-applied *after* the resolution change
   because Windows resets it; someone should confirm the ordering holds for the mid-session reconfig
   path, not just the create path.
4. **Is the `AudioHub` consumer capable of stalling?** The capture→hub channel is unbounded
   (`loopback.rs:39`), so the answer determines whether the bounded-queue finding is theoretical or
   real. That is a `streamer/audio/` question.
5. **Is `OPUS_SET_DTX_REQUEST, 0` deliberate?** (`streamer/audio/encoder.rs:95`.) It costs 200
   packets/s per device during silence. I suspect it is deliberate — `audio-worklet.js` corrects drift
   only at silence boundaries and may need a continuous timeline — but the reason is not written down
   anywhere, and it belongs in a comment.
6. **`CompatibilityReport` needs a new field and that is a three-module change.** Adding
   `encoder_backend` (and fixing `display_backend` to be probed) requires touching `windows_utils`,
   `macos_utils` and `linux_utils` plus regenerating `bindings.ts` and updating the settings UI.
   Worth batching with whatever else changes that struct.
7. **`setup`'s `-> bool` signature is the bottleneck for every "why did it fail" improvement.** Three
   findings (driver missing, topology race, install-drivers result) all end at "the command cannot
   express a reason". Changing it to a `Result`/report struct is a small, cross-cutting change that
   unblocks all of them.
8. **Background host load is a shared theme.** The 1 Hz WMI poll and the per-second `netsh` spawn are
   in this territory, but the question "what else wakes up on a timer while we are streaming?"
   spans every territory. If other agents found periodic timers, they should be collected into one
   list for the latency map — a host that wakes 4 times a second for housekeeping cannot hold a tight
   frame cadence.
9. **Mutex poisoning has no strategy.** Every `.lock().unwrap()` in `AppState` turns one panic into a
   permanently broken app. Given how many `unwrap()`s are reachable (§8), a decision is needed:
   either remove the reachable panics, or adopt a poison-recovery helper.
10. **`driver_ipc` is a vendored library with ~1200 lines supporting three call sites.** It is not
    marked as vendored the way `windows_capture/` is (which carries an
    `#[allow(warnings, clippy::all)]` and a documented "leave it as-is" rule). If it is vendored,
    saying so would stop future readers from treating its `unreachable!`s and untagged enums as
    first-party choices; if it is not, it is a candidate for a large simplification.

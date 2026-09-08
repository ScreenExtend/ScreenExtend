# A — Streamer core (`src-tauri/src/streamer/`)

Territory: `mod.rs`, `server.rs`, `session.rs`, `webrtc_session.rs`, `bitrate.rs`, `config.rs`,
`cloud.rs`, `tls.rs`, `platform.rs`, `pipeline.rs`, `cli.rs`, plus `test/` (reached from
`mod.rs`). 4,856 lines read in full. Dependency source was read where behaviour depended on it
(`webrtc-0.17.1`, `interceptor-0.17.1`, `rtp-0.17.1`, all from the local cargo registry).

Everything below cites `file:line`. Where I could not verify something on this Windows-only
tree, it says so.

---

## 1. Files and purpose

**`mod.rs` (93 lines)** — declares the twelve child modules and defines `Streamer`, a thin
wrapper over `Config`. `run` / `run_with_handle` / `serve` each call `prepare()` (DPI awareness
+ process tuning) and then `server::run`; the first two build their own multi-thread tokio
runtime with `on_thread_start(platform::tune_transport_thread)`. The `probe_*` and
`whep_selftest` methods are the entry points of a developer harness that nothing reaches
(§13).

**`server.rs` (1,531 lines)** — the axum HTTP/HTTPS server, one instance per network adapter.
Owns `AppState` (config, ICE servers, OTP limiter, local IPs), the route table, the client-page
and static-asset handlers, and — most importantly — `process_whep` / `start_session`, which is
the whole join sequence: OTP and trust checks, virtual-display creation and correlation,
display mode/scale application, capture-pipeline start, WHEP handshake, and the spawned
session-teardown task. Also holds the clamp constants (`MIN/MAX_REFRESH_RATE`,
`MIN/MAX_DISPLAY_SCALE`, `MAX_EFFECTIVE_SCALE`, `DISPLAY_ATTACH_TIMEOUT`, `LEAVE_SETTLE`,
the input size caps) and the process-wide `DISPLAY_CORRELATION_LOCK`.

**`session.rs` (785 lines)** — the shared-state types with no behaviour of their own: the
per-device `DeviceSessionState` map (`SharedSessions`), device overrides, the leave signal, the
control channel used to send BYE frames, live-display bookkeeping, reconfig/kick epochs, audio
output lists, `SessionAuth` (constant-time OTP compare), `OtpLimiter` (per-key lockout plus a
global cross-key brute-force guard), the `VirtualDisplayController` and `DeviceReporter` traits,
and the various `new_shared_*` constructors. Almost every function is "lock a `Mutex`, read or
write one field, drop".

**`webrtc_session.rs` (951 lines)** — everything WebRTC. Media engine and codec registration,
the interceptor registry, the UDP-only setting engine, ICE locality detection and SDP candidate
summarising, `handle_whep_offer` (the negotiation), the manual H.264 RTP writer task, the RTCP
reader that turns PLI/FIR into IDR requests, the audio DataChannel and Opus-track forwarders,
and `spawn_bitrate_driver` (the loss+RTT bitrate controller). This file is the transport.

**`bitrate.rs` (122 lines)** — `BitrateController` (EWMA smoothing, asymmetric change/cut
thresholds, a minimum interval that applies only to increases) and the pure function
`estimate_from_loss`. No I/O, no locks.

**`config.rs` (447 lines)** — the `Config` struct threading every shared handle from `AppState`
into the streamer, the `H264Profile` / `EncoderVendor` / `ScalePercent` newtypes, the defaults
(ports 8080/8443, monitor 1, `max_fps` 500, Google STUN), and `from_args` + `print_help` for the
dead probe harness.

**`cloud.rs` (514 lines)** — the relay control channel. A dedicated OS thread with its own tokio
runtime holds a WebSocket to `wss://session.screenextend.app/host/v1/connect`, registers a
session, and tunnels `POST /whep`, `GET /reconfig` and `POST /leave` back into the same
`process_*` functions the HTTP server uses. Protocol version 1, 45 s heartbeat, exponential
backoff to 30 s, `CloudState` reported to the UI.

**`tls.rs` (170 lines)** — self-signed certificate generation via `rcgen`, cached in the app
data dir, with a migration step that deletes stray certs from the working directory. Also
accepts an operator-supplied cert/key pair.

**`platform.rs` (177 lines)** — the compile-time dispatcher to the per-OS backend for DPI
awareness, process/thread tuning, the DPR ladder (`max_display_dpr`, `display_scale_steps`,
`display_dpr_ladder`, `snap_display_dpr`), the probe entry points, and `start_audio_capture`.
Also declares the `EncoderBackend` trait and `BackendConfig` shared by the encoders. Carries
`#![allow(dead_code, unused_imports)]` at line 1, which is what keeps the dead probe region
quiet.

**`pipeline.rs` (30 lines)** — pure re-export shim to `windows_utils::streamer::pipeline` or
`macos_utils::streamer::pipeline`. **No Linux arm and no fallback arm** (§14).

**`cli.rs` (36 lines)** — the probe harness dispatcher. Unreachable; nothing calls
`streamer::cli::run` (§13).

**`test/`** — `mod.rs` (4 lines), `whep_selftest.rs` (363 lines, an in-process WHEP client that
asserts RTP flows — broken and unreachable, §13), `bitrate.rs` (170 lines, `#[cfg(test)]` unit
tests for the controller, which do run under `cargo test`).

---

## 2. Function catalog

### `mod.rs`
| Fn | Purpose / notable behaviour |
|---|---|
| `Streamer::new` | Trivial constructor. |
| `Streamer::config` | Borrow the config. |
| `Streamer::prepare` (private) | `platform::set_dpi_awareness()` + `apply_process_tuning()`. Called by every entry point. |
| `Streamer::run` | Builds a multi-thread runtime, `block_on(server::run(cfg, None))`. |
| `Streamer::run_with_handle` | Same, with an `axum_server::Handle` for graceful shutdown. **This is the only path the shipped app uses** (`windows_utils/mod.rs:806`, `macos_utils/mod.rs:661`). |
| `Streamer::serve` | Async variant; no runtime built. No caller. |
| `Streamer::whep_selftest` | Builds a runtime, runs `test::whep_selftest::run`. Dead. |
| `Streamer::probe_capture` / `probe_dxgi` / `probe_encode` / `probe_live` / `probe_bitrate` | Dead probe entry points. |

### `pipeline.rs`
No functions — 30 lines of `pub use` behind `#[cfg(target_os = ...)]`.

### `cli.rs`
| Fn | Purpose |
|---|---|
| `run` | Parses `-h`/`--help`, builds `Streamer::new(Config::from_args())`, dispatches to whichever probe field is set, else `streamer.run()`. **No caller.** |

### `platform.rs`
| Fn | Purpose / notable behaviour |
|---|---|
| `set_dpi_awareness` | Windows only; no-op elsewhere. |
| `max_display_dpr` | 2.0 on macOS, 4.0 elsewhere. Hard-coded by `cfg`, not probed. |
| `display_scale_steps` | Windows: the real DPI table. macOS: `&[100, 200]`. Other: empty. |
| `display_dpr_ladder` | Maps steps to ratios in `[1.0, cap]`; falls back to 0.5-step synthesis if the table is empty. |
| `snap_display_dpr` | Snaps *down* to the largest available step ≤ requested. Returns 1.0 for non-finite input. |
| `apply_process_tuning` | Per-OS process priority tuning. |
| `tune_transport_thread` | Per-OS thread boost. Passed to every tokio runtime's `on_thread_start`. |
| `probe_capture` / `probe_dxgi` | Windows only; `bail!` elsewhere. Dead. |
| `probe_encode` | Windows only; picks the vendor backend, honouring `disable_gpu_encode`. Dead. |
| `start_audio_capture` | Dispatches to the per-OS audio backend; has a real Linux arm. |
| `EncoderBackend::{new, encode, set_bitrate}` | The trait the encoders implement. |

### `bitrate.rs`
| Fn | Purpose / notable behaviour |
|---|---|
| `BitrateController::new` | `with_params(min, max, alpha=0.4, change=0.10, interval=500ms).with_cut_threshold(0.04)`. |
| `with_params` | Asserts `min <= max` and `0 < alpha <= 1`. |
| `with_cut_threshold` | Asserts `0 < cut <= 1`. |
| `current_target` | Last emitted value. |
| `update` | EWMA-smooth, clamp to `[min,max]`, emit if the relative delta clears the direction-specific threshold. **Cuts skip the `min_interval` check entirely** (`:87-93`). |
| `emit` (private) | Records value + timestamp. |
| `estimate_from_loss` | Three bands: loss > 10% → multiply by `1 - 0.5*loss`; loss < 2% → probe `+8%` ceilinged at `1.5 × measured_send_bps`; otherwise hold. |

### `tls.rs`
| Fn | Purpose / notable behaviour |
|---|---|
| `dev_cert_dir` (private) | `%LOCALAPPDATA%` / `~/Library/Application Support` / `$XDG_DATA_HOME`, + `ScreenExtend`. |
| `install_crypto_provider` (private) | `let _ = ring::default_provider().install_default();` — result discarded (correct; it fails only if already installed). |
| `load_or_generate` | Operator cert+key, or `bail!` if only one is given, or the cached dev cert. |
| `rustls_config` | Builds a `RustlsConfig` from PEM; **clones both PEM buffers** every call. |
| `generate_or_load_dev_cert` (private) | Reuses the cached pair if both read successfully; otherwise regenerates. |
| `cache_dev_cert` (private) | Creates the dir, writes cert and key. |
| `write_private_file` (private) | Writes, then `chmod 0600` on Unix. |
| `migrate_stray_cwd_cert` (private) | Deletes stray cert files from the CWD. |
| `subject_alt_names` (private) | `localhost`, `127.0.0.1`, plus the LAN IP. |

### `config.rs`
| Fn | Purpose |
|---|---|
| `H264Profile::parse` | `baseline\|base\|cb`, `main`, `high`. |
| `EncoderVendor::parse` | `auto`, `nvidia\|nvenc`, `intel\|quicksync\|qsv\|onevpl\|vpl`, `software\|sw\|cpu\|x264`. |
| `ScalePercent::{new, percent, apply, is_native, parse, default}` | Clamped 10–100; `apply` is rounded integer math on `u64`. |
| `Config::default` | Ports 8080/8443, monitor 1, `max_fps` 500, `h264_profile` Baseline, `stun_urls` = Google STUN, everything shared `None`. |
| `Config::from_args` | ~200 lines of hand-rolled argv parsing. Unmatched args fall through `_ => i += 1`. **Dead.** |
| `print_help` | Help text. Says `USAGE: untitled17 [OPTIONS]` (`:397`). **Dead.** |

### `session.rs`
| Fn | Purpose / notable behaviour |
|---|---|
| `ByeReason::code` | Maps to the input protocol's BYE codes. |
| `new_shared_local_ips` / `new_shared_banned_devices` / `new_shared_approved_devices` / `new_shared_disconnect_grace` / `new_shared_server_ports` / `new_shared_turn_config` / `new_shared_otp_limiter` | Constructors for the `Arc<Mutex<…>>` / atomic handles. |
| `mint_device_token` | 16 random bytes as lowercase hex. `rand::random`. |
| `is_device_banned` | True if the set contains the **token or the IP**. |
| `is_device_approved` | Token only; empty token is never approved. |
| `ServerPortState::{get,set}` | Two `AtomicU16`, `Relaxed`. |
| `LiveDisplay::display_params` | The 5-tuple compared to decide whether a mode change is needed. |
| `LiveDisplay::swap_axes` | `portrait != (height > width)`. |
| `arm_leave` / `signal_leave` | Creates and fires the per-device `LeaveSignal` (`AtomicBool` + `Notify`). |
| `set_control_sender` / `clear_control_sender` | Stores the BYE channel; clear is guarded by `same_channel` so a stale session cannot clear a newer one. |
| `send_bye` / `send_bye_acked` (private) / `broadcast_bye` | Queues BYE frames; `broadcast_bye` waits for acks up to a deadline using a `std::sync::mpsc` channel. |
| `get_live_display` / `set_live_display` / `take_live_display` | Live-display bookkeeping. |
| `bump_reconfig_epoch` / `reconfig_epoch` / `bump_kick_epoch` / `kick_epoch` | The epoch counters the client polls. |
| `set_active_capture` / `take_active_capture` / `take_active_capture_if` | Stores the `CaptureStopper` boxed closure; the `_if` variant only takes it when the sequence matches, which is how a superseded session avoids killing the new one. |
| `next_session_seq` / `is_current_session` | Monotonic per-device session counter. |
| `set_audio_outputs` / `audio_outputs_report` / `set_selected_audio_output` / `selected_audio_output` | Per-device audio sink list. |
| `SessionAuth::validate` | `subtle::ConstantTimeEq` on both session id and OTP; rejects if either stored value is empty. |
| `OtpLimiter::{new, global_paused, note_global_failure, reset, locked_for, record_failure, record_success}` | 5 attempts per key / 60 s lockout; 20 failures across keys in 60 s → 60 s global pause. |
| `VirtualDisplayController` trait | `create_display`, `remove_display`, `remove_all_displays` are required; `create_display_with_modes`, `display_device_name` (returns `None`) and `max_concurrent_displays` (returns `None`) have defaults. |
| `set_host_ip` / `display_holder_host_ip` / `live_displays_except` / `live_display_count` | Adapter and capacity bookkeeping. |

### `server.rs`
| Fn | Purpose / notable behaviour |
|---|---|
| `default_dpr` | serde default, 1.0. |
| `sanitize_device_name` | Strips control chars, bidi overrides, ZWSP/BOM; truncates to 64 chars. Well tested (`:1465-1479`). |
| `deserialize_device_name` | serde hook onto the above. |
| `AppState::new` | Builds the ICE server list and net JSON **once**, falling back to fresh limiter / local-IP handles if the config has none. |
| `AppState::display_slot_unavailable` | Only ever true for the macOS AirPlay tier (`max_concurrent_displays() == Some(1)`); the current holder is never blocked. |
| `AppState::display_capacity` | `(max, in_use)`. |
| `AppState::is_same_device` | Loopback or in `local_ips`. |
| `AppState::fallback_ice_servers` | Clones the cached list. |
| `AppState::ice_with_turn` | Appends the user TURN and the ephemeral TURN servers. |
| `AppState::ice_json_live` | Builds the `/ice-config` JSON fresh each call (ephemeral TURN creds are time-limited). |
| `user_turn_ice_server` / `ephemeral_turn_ice_server` | TURN assembly; the ephemeral path mints HMAC creds via `turn::auth::generate_long_term_credentials`. |
| `normalize_peer_ip` | Unwraps IPv4-mapped IPv6. |
| `collect_local_ips` | Loopback v4+v6 plus every adapter IP, sorted and deduped. |
| `ProcessedResponse::err` | Error response helper. |
| `run` | Loads/generates TLS, logs URLs, binds HTTP and HTTPS with `NoDelayAcceptor`, `try_join!`s both. |
| `router` | 16 routes. |
| `health` | `"ok"`. |
| `isolation_headers` | COOP `same-origin` + COEP `require-corp`. |
| `index` | At-capacity check, then `include_str!(...).replace("__SAME_DEVICE_FLAG__", …)` **per request**. |
| `at_capacity_page` | Static 503 page. |
| `transform_worker` / `input_js` / `audio_js` / `audio_worklet_js` / `audio_worker_js` / `nosleep_js` / `logo` / `styles` | Static `include_str!` assets, zero-copy, **no cache headers**. |
| `ice_config` / `net_config` | JSON config for the client. |
| `reconfig` / `process_reconfig` | Returns `{epoch, kick, audioSink}`. Three sessions-mutex acquisitions per call; polled at 1 Hz per device. |
| `leave` / `process_leave` | Fires the leave signal. |
| `truncate_chars` | Char-count truncation helper. |
| `audio_outputs` | Ingests the client's sink list (≤32 entries, ≤256 chars each). Malformed body → silent 204. |
| `build_net_json` | `{"httpsPort": …}`. |
| `whep` | Capacity check, ICE assembly, `process_whep`, then maps the result to a response with the `x-device-token` header. |
| `process_whep` | JSON parse, size caps, ban check, pre-approval check, global pause / per-key lockout / OTP validation, token mint, then `start_session`. **Shared with the cloud relay** (`cloud.rs:484`). |
| `start_session` | The join sequence — see §11. |
| `remove_display_async` | `spawn_blocking(remove_display)`, result discarded. |
| `dpr_mode_ladder` | One even-rounded mode per DPR step; sorted and deduped. Does **not** include the transposed modes despite `CLAUDE.md` saying it does (§12). |
| `read_back_dimensions` | `spawn_blocking(monitor_dimensions)`, optionally transposed. |
| `restore_displays` | Serially re-applies mode+settle+scale to every other live display after a topology reset. |
| `apply_display_scale` | `spawn_blocking(set_display_scale)`. |
| `wait_for_display_settle` | Polls `monitor_dimensions` every 75 ms until two consecutive readings match; 2 s deadline. **Always sleeps at least once** because `last` starts `None`. |
| `wait_for_new_monitor` | Polls `monitor_device_names` every 100 ms for up to 5 s, nudging the topology after 600 ms. **Calls the blocking enumeration on the async thread.** |
| `build_ice_servers` | STUN (default Google) + static TURN. Logs a warning when TURN creds are incomplete. |
| `log_urls` | Startup banner. |

### `webrtc_session.rs`
| Fn | Purpose / notable behaviour |
|---|---|
| `AudioParams::audio_enabled_now` | Reads the per-device `audio_enabled` override under a lock. Called in the forwarder loop. |
| `h264_fmtp` / `h264_codecs` | fmtp string and the (pt, rtx_pt, profile-level-id) table. High profile registers two payload types. |
| `build_api` | Registers H.264 + RTX + Opus, the playout-delay header extension, and `register_default_interceptors`. |
| `udp_only_setting_engine` | UDP4/UDP6 only; srflx/prflx acceptance min-wait 50 ms, relay 0 ms. |
| `detect_session_locality` | Reads the nominated candidate pair from `get_stats`; classifies SameNetwork / CrossNetwork / Unknown. **Result is only logged** (§11, finding). |
| `is_private_ip` / `same_subnet_24` | Helpers for the AP-client-isolation warning. |
| `summarize_sdp_candidates` | Counts candidate types and lists routable ones for the log. |
| `log_ice_diagnostics` | Dumps every candidate pair on Failed/Disconnected. |
| `handle_whep_offer` | The negotiation. Builds the API and PC, wires the input data channels, adds the video track, spawns the RTCP reader, the bitrate driver, the audio forwarder and the RTP writer, installs the state-change handler, then `set_remote_description` → `request_idr` → `create_answer` → `set_local_description` → gather wait (≤1500 ms) → return the answer. |
| `setup_audio` | Fast path: unordered / no-retransmit `audio` DataChannel. Fallback: a standard Opus `TrackLocalStaticSample`. |
| `pc_alive` | Not Closed/Failed/Disconnected. |
| `subscribe_blocking` / `unsubscribe_blocking` | `spawn_blocking` around the audio hub refcount. Join errors are swallowed with no log (`:735`). |
| `forward_datachannel` | Drains the audio broadcast, writes the 13-byte header + Opus, with a buffered-amount backpressure check every 4th packet (16 KiB threshold). |
| `forward_track` | The NetEQ fallback; `write_sample` with a fixed 5 ms duration. |
| `spawn_bitrate_driver` | 120 ms `get_stats` poll → `bytes_sent` delta, `fraction_lost`, `round_trip_time` → `estimate_from_loss` → RTT clamp → `BitrateController::update` → `pipeline.set_target_bitrate`. |

### `cloud.rs`
| Fn | Purpose / notable behaviour |
|---|---|
| `HostCapabilities::default` | `webcodecs: true`, all three H.264 profiles. Static, not probed. |
| `IceServerWire::into_rtc` | Wire → `RTCIceServer`. |
| `CloudState::as_str` | State name for the UI. |
| `generate_session_id` | 12 chars from a 32-char ambiguity-free alphabet. |
| `CloudConfig::new` | Fixed WS URL, `CARGO_PKG_VERSION`. |
| `CloudClient::spawn` | Spawns a dedicated OS thread with its own multi-thread tokio runtime; `.ok()` discards a spawn failure. |
| `CloudClient::stop` / `Drop` | `notify_one` then `join`. |
| `install_crypto_provider` | Same discarded-result pattern as `tls.rs`. |
| `run_loop` | Reconnect loop with 1→30 s backoff plus ≤1 s jitter; rotates the session id on `session_taken` (substring match on the formatted error). |
| `connect_and_serve` | Connects, splits the socket, spawns a writer task fed by an unbounded mpsc, sends `Register`, runs `read_loop`, aborts the writer. |
| `read_loop` | 45 s per-frame timeout; parses text frames, answers WS pings, ignores malformed frames. |
| `handle_relay_message` | Dispatches; `SignalRequest` is `tokio::spawn`ed so slow joins do not stall the socket. Returns `Result<Option<()>>` where the `Option` carries nothing. |
| `dispatch` | Routes `POST /whep`, `GET /reconfig`, `POST /leave` into the same `process_*` functions the HTTP server uses; everything else 404. |

---

## 3. Threads

| Where | What | Lifetime | Owns |
|---|---|---|---|
| `windows_utils/mod.rs:805` / `macos_utils/mod.rs:660` | One `std::thread` **per network adapter IP**, each running `Streamer::run_with_handle` | Until the adapter IP disappears (1 s graceful shutdown, `windows_utils/mod.rs:769`) | A whole runtime |
| `streamer/mod.rs:39-42`, `:48-51`, `:62-65` | `tokio::runtime::Builder::new_multi_thread()` — **no `worker_threads` cap**, so one worker per logical CPU, each boosted by `on_thread_start(platform::tune_transport_thread)` | Runtime lifetime | All async tasks for that adapter |
| `cloud.rs:223-236` | Named OS thread `cloud-relay-client` with **another** full multi-thread runtime | Until `CloudClient::stop`/drop | The relay socket |
| `cloud.rs:339-345` | Writer task draining an unbounded mpsc into the WebSocket sink | Aborted when `read_loop` returns | The write half |
| `cloud.rs:422-439` | One task per tunnelled `SignalRequest` | Per request | A join |
| `webrtc_session.rs:394` | `input::spawn` → OS thread named `injector`, **one per WHEP offer** (`input/mod.rs:96-117`) | Exits when both crossbeam senders drop — which never happens, see the PC-leak finding | Input injection |
| `webrtc_session.rs:414-422` | BYE-sender task, one per reliable data channel | Until the control channel closes | Sending BYE frames |
| `webrtc_session.rs:468-484` | RTCP reader task | Until `rtp_sender.read` errors | PLI/FIR → IDR |
| `webrtc_session.rs:500-561` | **The RTP writer task, one per session** — the hot path | Until a track write fails or the broadcast closes | Payloading and socket writes |
| `webrtc_session.rs:585-589` | One-shot locality detector, 500 ms after Connected | ~500 ms | Nothing |
| `webrtc_session.rs:597-600` | ICE diagnostics dump on Failed/Disconnected | Brief | Nothing |
| `webrtc_session.rs:613-628` | Disconnected 8 s grace task | ≤8 s | The `closed_tx` take |
| `webrtc_session.rs:694` / `:713` | Audio forwarder (DataChannel or track), one per session | Until the PC dies | The audio subscription |
| `webrtc_session.rs:872-950` | Bitrate driver, one per session, 120 ms tick | Until the PC is Closed/Failed/Disconnected | The bitrate target |
| `server.rs:1000` | `restore_displays`, spawned after a topology reset | Serial over the other displays | Display settings |
| `server.rs:1117-1186` | The session-teardown task, one per session | Until leave/close + settle + grace | Display removal |
| `server.rs` (many) | `spawn_blocking` tasks for display work: `:795`, `:896`, `:934`, `:951`, `:975`, `:1142`, `:1193`, `:1209`, `:1231`, `:1244`, `:1260`, `:1288` | Per call | Blocking Win32/CG calls |

Windows/macOS worker-thread count on a 16-thread box with three adapters: ~48 boosted streamer
workers + 16 relay workers + Tauri's own, for a steady-state workload of a few tasks. See
`findings/minor-session-one-whole-tokio-runtime-per-network-adapter.md`.

---

## 4. Locks

All first-party locks are `std::sync::Mutex` except one, per `CLAUDE.md`'s rule.

| Lock | Guards | Taken by | Hot path? |
|---|---|---|---|
| `SharedSessions` = `Arc<Mutex<HashMap<String, DeviceSessionState>>>` | The whole per-device session map | ~25 functions in `session.rs`; `start_session` takes it **10+ separate times** (`:789`, `:793`, `:852`, `:867`, `:1009`, `:1035`, `:1068`, `:1093`, `:1096`, `:1101`, `:1140`, `:1163`, `:1180`); `process_reconfig` takes it 3× per call | Warm — 3 acquisitions/second/device from `/reconfig` polling, plus a burst per join. Shared across every adapter's runtime and the cloud thread. |
| `SharedDeviceOverrides` | Per-IP overrides | `server.rs:811`, `:867`; `webrtc_session.rs:62` in the audio forwarder loop | Warm — the audio forwarder checks it once per drained packet batch |
| `SharedLocalIps` | Adapter IP list | `server.rs:160` (`is_same_device`), `windows_utils/mod.rs:756` | Cold |
| `SharedBannedDevices` / `SharedApprovedDevices` | Trust sets | `session.rs:104`, `:115` | Join only |
| `OtpLimiter.attempts` / `.global` | Rate-limit state | `session.rs:531`, `:547`, `:575-576`, `:580`, `:597`, `:620-621` | Join only |
| `SessionAuth.session_id` / `.otp` | The credentials | `session.rs:475-476`; `cloud.rs:293` on rotation | Join only |
| `SharedTurnConfig` | User TURN | `server.rs:208` | Join only |
| `closed_tx: Arc<Mutex<Option<Sender>>>` | The one-shot close signal | `webrtc_session.rs:606`, `:618` | Rare |
| `control_rx: Arc<Mutex<Option<Receiver>>>` | The BYE receiver, taken once | `webrtc_session.rs:410` | Once |
| `DISPLAY_CORRELATION_LOCK: tokio::sync::Mutex<()>` | New-display creation and name correlation | `server.rs:928` | **Held across `spawn_blocking` awaits and a ≤5 s poll loop.** Process-wide static, so it serialises across adapters. |
| Atomics | `SharedDisconnectGrace` (`AtomicU64`), `ServerPortState` (2× `AtomicU16`), `LeaveSignal.left` (`AtomicBool`), `locality_logged` (`AtomicBool`), `Pipeline.idr_request` / `target_bitrate` | — | `idr_request` and `target_bitrate` are on the encode path, `Relaxed` |

**Guards across `.await`:** none. `DISPLAY_CORRELATION_LOCK` is the only lock deliberately held
across awaits and it is the async type, which is correct.

**The `if let` scrutinee temporary-lifetime pattern** (the footgun `CLAUDE.md` calls out) appears
three times. All three are safe *today* because no `.await` sits inside the block, but they are
the fragile shape:

- `server.rs:866-870` — `if let Some(overrides) = … { if let Some(o) = overrides.lock().unwrap().get_mut(client_ip) { o.orientation_portrait = client_portrait; } }`. The guard lives to the end of the inner block, inside an `async fn` on an axum handler. Adding any await inside would fail to compile (`MutexGuard: !Send`), so the compiler backstops this one.
- `webrtc_session.rs:606` — `if let Some(tx) = closed_tx.lock().unwrap().take() { let _ = tx.send(()); }` inside a synchronous callback closure. **Not** compiler-checked, because the closure body is sync.
- `webrtc_session.rs:618` — the same inside a `tokio::select!` arm in a spawned task. Compiler-checked (the task must be `Send`), and it passes because the guard drops at the end of the block.

Everything else that could have been a scrutinee guard is written safely — e.g. `session.rs:289-293`
and `:249-254` clone out of the map, drop the guard, then act.

---

## 5. Buffers and copies

Video, per frame, per viewer:

1. **Encoder → `Bytes`** — one CPU copy out of the encoder's mapped bitstream buffer into a
   `Bytes` (done in the OS pipeline; `EncodedFrame { data: Bytes, capture: Instant }`,
   `windows_utils/streamer/pipeline.rs:34-37`).
2. **`Bytes` → broadcast** — refcount clone only. **Zero-copy**, and the fan-out to N viewers is
   free.
3. **Payloading** (`webrtc_session.rs:513`, `rtp-0.17.1/src/codecs/h264/mod.rs:53-140`) —
   - NAL ≤ MTU: `nalu.clone()`, a `Bytes` slice. **Zero-copy.**
   - NAL > MTU: `BytesMut::with_capacity(2 + fragment)` **allocation + memcpy per fragment**
     (~1186 B each). A 16 KB frame → ~14 allocations + a 16 KB copy; a 300 KB IDR → ~250
     allocations + a 300 KB copy.
   - SPS/PPS: a `Vec::with_capacity` + two `extend_from_slice` copies to build the STAP-A.
   - `payloads` is `vec![]` with no capacity hint, so it reallocates as it grows.
   **This happens once per viewer**, because the payloader is constructed inside the per-session
   writer task (`:504`).
4. **Per-binding packet clone** — `let mut pkt = p.clone()` plus a header-extension `Vec`
   allocation, in `write_rtp_with_extensions_attributes`
   (`webrtc-0.17.1/.../track_local_static_rtp.rs:98`, `:131-150`).
5. **Marshal + SRTP** — one more buffer and encrypt on the way to the socket, inside webrtc-rs.

Audio, per packet, per session:
- `AudioPacket.data` is a `Bytes` from the encoder; the broadcast fan-out is refcount-only.
- DataChannel path: `scratch.resize(HEADER_LEN + len)` (amortised, the `Vec` is reused across
  iterations at `webrtc_session.rs:752`) + `protocol::write_message` copy + **`Bytes::copy_from_slice(&scratch)`
  — a second full copy per packet** (`:792`). The reused scratch buffer is defeated by that
  final copy.
- Track path: `write_sample` takes `pkt.data` by value — no extra copy (`:838`).

HTTP, per request:
- `index` (`server.rs:394`) — `include_str!(...).replace(...)` scans and **allocates the whole
  client page on every GET /**.
- All other static assets return `&'static str`. **Zero-copy.**
- `tls::rustls_config` clones both PEM buffers (`tls.rs:60`), once per server start.
- `Config` is cloned per adapter (`server.rs:288`) and again per join (`server.rs:805`).

---

## 6. Queues

| Queue | Type | Depth | Blocking / back-pressure |
|---|---|---|---|
| Encoder → RTP writers | `tokio::sync::broadcast<EncodedFrame>` (`windows_utils/streamer/pipeline.rs:141`, `:253`) | **4** (`BROADCAST_CAPACITY`) | `send` **never blocks**. A receiver more than 4 frames behind loses the oldest and gets `RecvError::Lagged`. **No back-pressure on the encoder at all.** The writer responds to `Lagged` by requesting an IDR (`webrtc_session.rs:550-553`) — see the feedback-loop finding. |
| Audio hub → session forwarders | `tokio::sync::broadcast<AudioPacket>` | Set in `streamer/audio/` (outside this territory) | `Lagged` is silently `continue`d (`webrtc_session.rs:796`, `:849`) with no counter |
| Encoder wake | `crossbeam_channel::bounded::<()>(1)` (`windows_utils/streamer/pipeline.rs:303`) | 1 | `try_send`, result discarded — coalescing by design |
| Input events | `crossbeam` `bounded(FAST_CAP)` + `unbounded` (`input/mod.rs:92-93`) | Fast path bounded, reliable path unbounded | Outside this territory |
| Session close signal | `tokio::sync::oneshot` (`server.rs:1047`) | 1 | — |
| BYE control | `tokio::sync::mpsc::unbounded` (`server.rs:1048`) | Unbounded | Never blocks |
| BYE ack | `std::sync::mpsc` + `recv_timeout` (`session.rs:302-321`) | Unbounded | Bounded by a deadline |
| Relay outbound | `tokio::sync::mpsc::unbounded` (`cloud.rs:327`) | **Unbounded** | Never blocks; the writer task breaks on the first send error and subsequent sends are silently dropped (`:435`) |
| Grace cancel | `tokio::sync::watch<bool>` (`webrtc_session.rs:568`) | 1 (latest) | — |
| DataChannel audio backpressure | Not a queue — a check on `dc.buffered_amount()` every 4th packet against `AUDIO_BACKPRESSURE_BYTES` = 16 KiB (`webrtc_session.rs:43`, `:774-783`) | — | Over threshold → the packet is dropped and a diagnostics counter incremented |

**No pacer anywhere.** After payloading, each FU-A fragment is written with a separate awaited
`write_rtp_with_extensions` (`webrtc_session.rs:527-545`) straight into the SRTP/DTLS/UDP path.
A 300 KB IDR is ~250 back-to-back sends with no spacing.

---

## 7. Syscall / FFI boundaries

This module is deliberately OS-independent; almost every crossing is via `pipeline::` or
`platform::` into a sibling module.

| Crossing | Where | Notes |
|---|---|---|
| `pipeline::monitor_device_names` | `server.rs:930`, `:1280` | Win32 `EnumDisplayDevices` walk. **Called on the async thread at both sites** (`:1288` correctly wraps it) |
| `pipeline::monitor_dimensions` | `server.rs:1209`, `:1260` | `spawn_blocking` ✓ |
| `pipeline::monitor_rect` | `server.rs:1043` | **On the async thread**, for a log line |
| `pipeline::set_display_mode` | `server.rs:897`, `:976`, `:1231` | `spawn_blocking` ✓ |
| `pipeline::set_display_scale` | `server.rs:1245` | `spawn_blocking` ✓ |
| `pipeline::set_display_topology_extend` | `server.rs:1288` | `spawn_blocking` ✓ |
| `pipeline::start_on_monitor` | `server.rs:1038` | **On the async thread.** D3D11 device + encoder session + capture start, with a full DXGI fallback path behind it |
| `VirtualDisplayController::*` | `server.rs:934`, `:951`, `:1193` | `spawn_blocking` ✓. On Windows this crosses into `driver_ipc`; on macOS into the private `CGVirtualDisplay*` classes |
| `platform::set_dpi_awareness` / `apply_process_tuning` / `tune_transport_thread` | `mod.rs:33-34`, `:41` | Per-OS |
| `platform::start_audio_capture` | `platform.rs:142-159` | WASAPI / CoreAudio / stub |
| `turn::auth::generate_long_term_credentials` | `server.rs:225` | Pure Rust HMAC |
| `rcgen` + `rustls`/`ring` | `tls.rs:93-104`, `:29` | Pure Rust; `ring` has C/asm underneath |
| `libwebrtc-rs` UDP sockets, SRTP, DTLS | Throughout `webrtc_session.rs` | Pure Rust |
| `tokio-tungstenite` + rustls | `cloud.rs:322` | TLS to the relay |
| `subtle::ConstantTimeEq` | `session.rs:474-482` | Constant-time compare |
| `rand::random` | `session.rs:93`, `cloud.rs:191`, `:304` | Token, session id, backoff jitter |

**macOS dyld exposure:** nothing in this territory links a macOS symbol directly. All macOS API
use is behind `macos_utils`, so the "no link-time references above 10.15" rule is not at risk
from these files.

---

## 8. Panics

Runtime-reachable `unwrap` / `expect` / index / assert:

| Site | Condition |
|---|---|
| ~40 × `.lock().unwrap()` across `session.rs`, `server.rs`, `webrtc_session.rs`, `cloud.rs` | `PoisonError` — one panic while any of these guards is held poisons that lock **for the whole host**, across every adapter runtime and the relay. Cascading and permanent. See `findings/minor-session-mutex-poisoning-cascades-across-every-session.md` |
| `bitrate.rs:31` `assert!(min_bps <= max_bps)` | Guarded at the one call site by `DEFAULT_MIN_BITRATE_BPS.min(max_bps)` (`webrtc_session.rs:868`), so unreachable today. Would fire if a future caller passed an inverted pair |
| `bitrate.rs:32` `assert!(alpha > 0.0 && alpha <= 1.0)` | Constant today |
| `bitrate.rs:47-50` `assert!(cut_threshold …)` | Constant today |
| `webrtc_session.rs:454` `h264_codecs(profile)[0].2` | Index. `h264_codecs` never returns an empty vec, so safe, but it is an unchecked index on a function that could grow a new arm |
| `config.rs:199` `std::process::exit(0)` | Not a panic, but `from_args` calls `print_help()` then exits the process on `--help`. Dead today; would be a surprising side effect if the harness were revived inside the GUI process |
| `input/mod.rs:117` `.expect("spawn injector thread")` | Reached from `webrtc_session.rs:394` on every join. A thread-spawn failure under resource pressure panics the join handler — and, per the poisoning row above, could poison a lock on the way out |
| `test/whep_selftest.rs:297` `ServerName::try_from("localhost").unwrap()` | Constant; dead code |
| `server.rs:944-945` `.context("create-display task")?` then `.map_err(...)?` | Not a panic — a `JoinError` from a panicking `spawn_blocking` is converted to an error. Good |
| `server.rs:906-909`, `:987-990`, `:1250-1252` | `JoinError` handled explicitly with a "task panicked" log. Good |

No `unreachable!()`, no `todo!()`, no slice indexing on untrusted input. Request-size caps
(`server.rs:23-27`, `:636-642`) bound the parse. `sanitize_device_name` is well tested.

---

## 9. Silent fallbacks and swallowed errors

**Silent, and consequential:**

| Site | What is lost |
|---|---|
| `webrtc-0.17.1/.../track_local_static_rtp.rs:100-123` (via `webrtc_session.rs:539`) | `write_rtp` with **no bindings returns `Ok(0)`**. Every frame encoded before the track is bound — including the IDR requested at `:643` — is discarded with no error and no log. Combined with the infinite GOP this is the biggest silent latency cost in the file |
| `rtp-0.17.1/src/codecs/h264/mod.rs:80-88` | If the SPS+PPS STAP-A exceeds the MTU it is **not pushed**, and the stashed parameter sets are cleared anyway. Permanent black screen with no host-side symptom |
| `rtp-0.17.1/src/codecs/h264/mod.rs:61-62` | AUD and FILLER NALUs dropped silently (correct, but invisible) |
| `webrtc_session.rs:796`, `:849` | Audio broadcast `Lagged(_) => continue` — packets dropped with no counter and no log |
| `webrtc_session.rs:800`, `:853` | 250 ms recv timeout `Err(_) => continue` — silent |
| `webrtc_session.rs:735` | `spawn_blocking` `JoinError` on audio subscribe → `None`, no log |
| `cloud.rs:246` | `.ok()` on the relay thread spawn. A failed spawn leaves the UI on "connecting" forever with nothing logged |
| `cloud.rs:435` | `let _ = tx.send(text)` for the tunnelled WHEP **answer**. If the writer task has exited, the client's join hangs with no host log |
| `server.rs:534` | Malformed `/audio-outputs` body → `204 No Content`, indistinguishable from success |
| `server.rs:1193` | `let _ = spawn_blocking(remove_display).await` — a failed display removal leaves an orphaned virtual display, silently |
| `tls.rs:77-88` | If either cached PEM fails to read, both are silently regenerated (and a client that trusted the old cert now sees a warning) |
| `cloud.rs:284` | `SESSION_TAKEN` detected by substring match on a formatted error string; a relay message merely *containing* `session_taken` would rotate the user's join code |

**Logged, so acceptable:** `server.rs:902-909` / `:983-990` (mode-set failure → `mode_applied = false`,
read back the real dimensions, retry next reconfigure), `server.rs:1021-1026`,
`webrtc_session.rs:490-492` (audio setup failure → continue without audio),
`webrtc_session.rs:516-519` (payload failure → request IDR), `tls.rs:106-108`,
`server.rs:1322-1325` (incomplete TURN creds), `cloud.rs:376-379` (malformed relay frame).

**Benign:** `tls.rs:29` / `cloud.rs:265` `let _ = …install_default()` — fails only when already
installed. `webrtc_session.rs:59`, `:64` `let _ = wake.try_send(())` — coalescing by design.
`webrtc_session.rs:582`, `:612` watch sends. `server.rs:1068` `let _ = take_live_display`.

**Fast-path degradations that are logged (good practice, noted for the map):**
`windows_utils/streamer/pipeline.rs:273-287` — WGC failure falls back to DXGI with a
`teprintln!`. `webrtc_session.rs:697-714` — no WebCodecs/worklet on the client falls back from
the DataChannel to a NetEQ Opus track, logged at `:712`.

---

## 10. TODO / FIXME / HACK / XXX

```
$ grep -n "TODO\|FIXME\|HACK\|XXX" mod.rs server.rs session.rs webrtc_session.rs \
      bitrate.rs config.rs cloud.rs tls.rs platform.rs pipeline.rs cli.rs test/*.rs
(no matches)
```

None in the territory.

---

## 11. Latency path

### The join sequence, traced

Every stage below runs to completion before the next begins. Nothing overlaps.

| # | Stage | Where | Cost |
|---|---|---|---|
| 1 | Client fetches `/ice-config`, creates the offer, **waits for its own ICE gathering** | `static/index.html:687`, `:948-950` — `waitForIceGathering(pc, hasTurn ? 6000 : 3000)` | ≤3 s LAN, ≤6 s with TURN. Google STUN is always in the list, so this is never zero |
| 2 | `POST /whep` → capacity check → ICE assembly | `server.rs:589-618` | µs |
| 3 | JSON parse + size caps | `server.rs:626-642` | µs |
| 4 | Ban check → pre-approval check → global pause → per-key lockout → `SessionAuth::validate` → mint token | `server.rs:655-747` | Several mutex round trips |
| 5 | `next_session_seq`; **stop and await the previous capture** | `server.rs:785-797` — `spawn_blocking(stop).await` | Full encoder+capture teardown on any rejoin |
| 6 | Compute backing size, snap DPR, resolve overrides, decide orientation | `server.rs:799-887` | µs |
| 7a | *Existing display, params changed:* `set_display_mode` → `wait_for_display_settle` → `apply_display_scale` | `server.rs:892-916` | ≥75 ms + 3 `spawn_blocking` round trips |
| 7b | *New display:* **acquire `DISPLAY_CORRELATION_LOCK`** → `monitor_device_names` (blocking, on the async thread) → `create_display_with_modes` → `display_device_name`, else `wait_for_new_monitor` | `server.rs:927-971` | **≤5 s** (`DISPLAY_ATTACH_TIMEOUT`), topology nudge at 600 ms, 100 ms poll. Process-wide lock, so concurrent joins serialise |
| 8 | `set_display_mode` → `wait_for_display_settle` → `apply_display_scale` (+ `restore_displays` if the topology reset) | `server.rs:973-1003` | ≥75 ms |
| 9 | Record the landed display; read back dimensions if the mode did not apply | `server.rs:1008-1036` | one `spawn_blocking` on the failure path |
| 10 | **`pipeline::start_on_monitor`** — D3D11 device, scaler, encoder session, capture start (with a full DXGI fallback behind it) | `server.rs:1038-1041` | **Blocking, on the async thread.** Tens to hundreds of ms |
| 11 | `monitor_rect` for a log line | `server.rs:1043` | Blocking, on the async thread |
| 12 | `handle_whep_offer`: `build_api`, `new_peer_connection`, input data-channel wiring, `add_track`, spawn RTCP reader / bitrate driver / audio forwarder / **RTP writer** | `webrtc_session.rs:367-562` | ms |
| 13 | `set_remote_description(offer)` → **`request_idr()`** → `create_answer` | `webrtc_session.rs:639-645` | The IDR here is thrown away (§9) |
| 14 | `set_local_description(answer)` → **wait for ICE gathering**, `GATHER_MAX_WAIT` = 1500 ms | `webrtc_session.rs:647-657` | ≤1.5 s. Vanilla (non-trickle) ICE — the client cannot start connectivity checks until the answer lands |
| 15 | Answer returned; reporter notified; leave signal armed; teardown task spawned | `server.rs:1076-1186` | µs |
| 16 | Client applies the answer → ICE connectivity → DTLS → SRTP | client side | RTT-dependent |
| 17 | Track binds. First frames written are P-frames referencing a keyframe the client never saw → client emits PLI → host produces an IDR | `webrtc_session.rs:472-483` | **One PLI round trip plus the browser's PLI-generation latency** |

Worst case before DTLS even starts: 3 s + 5 s + ~0.2 s + pipeline + 1.5 s. Typical LAN case is
still dominated by 3 s (client STUN) + 1.5 s (host STUN) unless STUN resolves promptly.

**And it all runs again on every settings change** — `applyReconfig` →
`reconfigure()` → `teardownSession(); start(true)` (`static/index.html:356-380`). So
`CLAUDE.md`'s "settings changes apply live via epoch bumps rather than tearing down the
display" is true of the *display* and false of the WebRTC session, the encoder, the capture
and the injector thread.

### The steady-state frame path

`encoder thread → tx.send(EncodedFrame)` → **broadcast, depth 4, no back-pressure** →
per-viewer writer task: `host_ns_to_rtp90k(host_instant_to_ns(frame.capture))` →
`H264Payloader::payload(1188, &frame.data)` → for each fragment, `Packet { marker: i == last,
sequence_number: sequencer.next(), timestamp: rtp_ts }` → `write_rtp_with_extensions(&pkt,
&[PlayoutDelay(0,0)])` → binding sets SSRC/PT/extension ids → interceptor chain (NACK responder
buffers 8192 packets, SR/RR reports) → SRTP → UDP `send_to`. **Awaited per fragment, no pacer.**

The RTP timestamp rides the shared host clock (`streamer/audio/mod.rs:19-33`), the same epoch
audio `capture_ns` uses — this is the A/V sync mechanism and it is correct.

### Serialisation points

1. Client gather (3 s) blocks the POST.
2. `DISPLAY_CORRELATION_LOCK` serialises new-display creation **process-wide**, up to 5 s each.
3. `start_on_monitor` blocks a tokio worker shared with every other session's RTP writer on that
   adapter.
4. `wait_for_display_settle` always sleeps ≥75 ms even when nothing changed (`last` starts `None`).
5. Host gather (1.5 s) blocks the answer.
6. `spawn_blocking(stop).await` on rejoin serialises teardown before setup.
7. The RTP writer awaits each fragment's socket write in turn.

### Estimated stage costs

I could not measure on this box (Windows tree, no host running, and the brief forbids touching
the Mac). The numbers above are code-derived bounds, not measurements. The two ICE gather waits
and the display attach timeout are the three that are *certainly* on the critical path and
*certainly* multi-hundred-millisecond in the worst case.

---

## 12. Stale references

| Location | Text | Reality |
|---|---|---|
| `server.rs:364` | `// Cross-origin isolation (§6.4): …` | No document in the tree has a §6.4. The comment's content is accurate; only the citation is orphaned |
| `config.rs:397` | `USAGE: untitled17 [OPTIONS]` | The crate is `screenextend`, the binary `ScreenExtend`. `untitled17` is a dead crate name |
| `test/whep_selftest.rs:33`, `:244` | `=== M3 WHEP self-test ===` / `PASSED` | "M3" is a milestone that appears nowhere in the tree |
| `CLAUDE.md` vs `server.rs:1196-1205` | "`dpr_mode_ladder` pre-registers one display mode per ratio … **(plus each transpose, so an orientation flip is already registered)**" | `dpr_mode_ladder` maps the ladder, sorts and dedupes. **It does not add transposed modes.** An orientation flip on a live display therefore needs a mode that may not be registered |
| `CLAUDE.md` vs `static/index.html:356-380` | "Settings changes apply live via epoch bumps … rather than tearing down the display" | True for the display; the client tears down and rebuilds the entire WebRTC session, encoder and capture |
| `CLAUDE.md` vs `pipeline.rs` | "`linux_utils/` (compiles, mostly `bail!`s)" and "`pipeline.rs` — thin re-export layer pointing at the active OS pipeline" | `pipeline.rs` has no Linux arm, so the unconditionally-compiled `streamer` core cannot build for Linux (§14) |
| `CLAUDE.md` vs `Cargo.toml` | "Current version: **0.5.4**" | `git log` shows `build(release): bump version to 0.6.3` as the second-most-recent commit |
| `CLAUDE.md` vs code | "`--turn-*`, `--encoder`, `--qp` … tuning flags" described as a usable harness | The harness is unreachable; the flags do nothing (§13) |

No references to AI tooling anywhere in the territory.

---

## 13. Dead code candidates

**The entire developer probe harness.** Evidence:

```
$ grep -rn "streamer::cli\|Config::from_args" src-tauri/src --include=*.rs
src-tauri/src/streamer/cli.rs:14:    let streamer = Streamer::new(Config::from_args());
```

That is the only hit outside the definitions. `main.rs` is three lines into
`screenextend_lib::run()`; `lib.rs` routes arguments through `src-tauri/src/cli.rs`'s
`fast_path`/`dispatch` — a *different* module. The only `Streamer` users are
`windows_utils/mod.rs:806` and `macos_utils/mod.rs:661`, both building `Config`
programmatically.

Dead as a result: `streamer/cli.rs::run`; `Config::from_args` (`config.rs:186-390`);
`print_help` (`config.rs:393-447`); the six probe fields on `Config` (`config.rs:96-102`);
`Streamer::{whep_selftest, probe_capture, probe_dxgi, probe_encode, probe_live, probe_bitrate}`
(`mod.rs:60-92`); `platform::{probe_capture, probe_dxgi, probe_encode}` (`platform.rs:90-140`);
`test/whep_selftest.rs` in its entirety; and by extension the per-OS `pipeline::probe_live` /
`probe_bitrate` (`windows_utils/streamer/pipeline.rs:656`, `:677`;
`macos_utils/streamer/pipeline.rs:297`, `:356`), which have no other caller.

`platform.rs:1`'s `#![allow(dead_code, unused_imports)]` is what has kept the compiler silent.

**Also unused:**
- `Streamer::serve` (`mod.rs:55-58`) — no caller.
- `cloud::CLOUD_DOMAIN` (`cloud.rs:16`) — carries `#[allow(dead_code)]`.
- `cloud::register_error_code::{INVALID_SESSION, VERSION_UNSUPPORTED, RATE_LIMITED, UNAUTHORIZED}`
  (`cloud.rs:151-157`) — only `SESSION_TAKEN` is read, and only via a substring match. The module
  carries `#[allow(dead_code)]`.
- `handle_relay_message`'s `Option` return (`cloud.rs:397`, `:454`) and the
  `if let Some(()) = … {}` at `:381` — the option never carries information.
- `Config.synthetic_pattern` (`config.rs:101`) — parsed by the dead `from_args`; the field is
  read only by the per-OS pipelines' probe paths.
- `SessionLocality` and `detect_session_locality` are *not* dead but are effectively so: the
  result is formatted into a log line and discarded (`webrtc_session.rs:588`).

---

## 14. Capability probing

**Correct — probed, not version-gated:**
- `platform::display_scale_steps` / `snap_display_dpr` read the OS's real DPI table on Windows
  rather than assuming a ladder (`platform.rs:23-38`, `:59-74`).
- `VirtualDisplayController::max_concurrent_displays` returns `None` by default
  (`session.rs:655-657`), so only a backend that genuinely has a cap declares one, and
  `display_slot_unavailable` short-circuits to `false` otherwise (`server.rs:127-141`). Correct
  per the "simultaneous displays are uncapped" rule.
- `VirtualDisplayController::display_device_name` returns `None` by default
  (`session.rs:650-653`) and the caller falls back to the diff-based correlation
  (`server.rs:957-960`) — a capability probe, not a version check.
- Client audio capability is reported by the client and read as a fact
  (`server.rs:29-35`, `:818`: `fast: webcodecs_opus && worklet`), with a real fallback tier
  (`webrtc_session.rs:676-714`).
- Capture backend selection (WGC → DXGI, `windows_utils/streamer/pipeline.rs:261-288`) is
  try-and-fall-back, not version-gated.

**Version-gated where it should be probed:**
- `platform::max_display_dpr` (`platform.rs:12-21`) returns 2.0 on macOS and 4.0 elsewhere from
  a `cfg`, with no probe. That is a *platform* constant rather than an OS-version check, so it
  is defensible, but it hard-codes an assumption about what the OS can do rather than asking.

**Missing tiers:**
- **No Linux arm in `pipeline.rs`.** The file has exactly two `cfg` blocks — windows
  (`:1-16`) and macos (`:18-30`) — and no `not(any(...))` fallback, while `lib.rs:18` declares
  `mod streamer;` unconditionally and `server.rs` uses `pipeline::*` with no guards
  (`:897`, `:930`, `:976`, `:1038`, `:1043`, `:1209`, `:1231`, `:1245`, `:1260`, `:1280`,
  `:1288`) and `webrtc_session.rs:37` imports `Pipeline`. `platform.rs` shows the correct
  pattern with explicit `#[cfg(not(any(windows, macos)))]` arms (`:33-37`, `:95-99`, `:106-111`,
  `:135-139`, `:155-158`). **Unverified** — I could not cross-compile from this tree — but the
  `cfg` attributes are unambiguous. See
  `findings/major-build-streamer-core-cannot-compile-on-linux.md`.
- **No client-capability tier for the playout-delay extension.** It is stamped unconditionally
  (`webrtc_session.rs:505-507`) and silently dropped for peers that did not negotiate it
  (i.e. Safari and Firefox), with no log recording which happened.
- **`HostCapabilities::default()`** (`cloud.rs:30-37`) advertises `webcodecs: true` and all three
  H.264 profiles to the relay as a hard-coded constant, with no probe of what this host's
  encoder can actually produce.

---

## 15. Questions and cross-cutting concerns

1. **The RTT unit bug is the single highest-value finding I have and it crosses into the encoder
   territory.** `webrtc_session.rs:897-899` treats `RemoteInboundRTPStats::round_trip_time` as
   seconds; webrtc-rs stores milliseconds
   (`interceptor-0.17.1/src/stats/interceptor.rs:812`). The clamp therefore fires on every poll
   of every session and ratchets the target to the 1 Mbps floor. Whoever owns the encoder
   territory should confirm the `adapting bitrate: X -> 1000000 bps` line
   (`windows_utils/streamer/pipeline.rs:75`) actually appears in a live host log — that is the
   one-line confirmation. If it does, this explains a large fraction of any "quality is bad"
   report and should be fixed before anything else is measured, because it contaminates every
   other measurement.

2. **Infinite GOP is load-bearing across three territories and nothing guarantees a keyframe
   reaches the client.** The encoders set `NVENC_INFINITE_GOPLENGTH` / `X264_KEYINT_MAX_INFINITE`;
   the transport discards frames written before the track binds (silently, `Ok(0)`); the
   payloader drops parameter sets that do not fit one MTU (silently). Any one of those alone is
   survivable; together they mean the client's own PLI is the *only* thing that makes a session
   start. Somebody should own the invariant "a keyframe is delivered within N ms of Connected"
   end to end.

3. **`SessionLocality` exists, works, and is thrown away.** Almost every transport decision in
   this module — STUN, both gather waits, playout delay, the RTT threshold, the loss thresholds,
   the broadcast depth — is a constant that should differ between a same-subnet hop and a relayed
   path. The information needed to classify is available at `process_whep` time
   (`AppState::local_ips`, and the cloud relay knows it is the relay). This is the largest
   single architectural lever I found for the engagement's LAN goal.

4. **Who owns the "one runtime per adapter" decision?** Each adapter IP gets a full
   `new_multi_thread` runtime with a worker per CPU, all priority-boosted, all sharing the same
   `Mutex`es and the same process-wide `DISPLAY_CORRELATION_LOCK`. It may be entirely benign; it
   may be contending with the capture and encode threads. Whoever measures thread scheduling for
   the encoder territory should include these in the picture.

5. **Is `VirtualDisplayController::display_device_name` implemented by the Windows IDD backend?**
   If not, every new-display join takes the slow diff-and-poll correlation path under the
   process-wide lock (up to 5 s) instead of the fast direct-name path. I did not read
   `windows_utils/driver_ipc/`; someone in that territory should check. It is a cheap, large win
   if the answer is "no".

6. **Is `Lagged` → `request_idr` (`webrtc_session.rs:550-553`) actually helping?** On a
   4-deep broadcast with an infinite-GOP encoder, the response to "you are behind" is "here is
   the biggest frame I can produce". I believe this is a positive-feedback loop on a congested
   link but could not reproduce it. The client territory may have corroborating evidence — the
   client also has a backlog-driven keyframe request
   (`findings/minor-client-backlog-threshold-triggers-keyframe-thrash.md` from another agent
   looks like the same loop seen from the other end). **These two findings should be reconciled.**

7. **The self-test is the repo's only end-to-end capture→encode→RTP assertion, and it is both
   unreachable and wire-incompatible** (it POSTs raw SDP; `/whep` has required a JSON envelope
   since the join form landed). Restoring it would give the latency work a regression gate it
   does not currently have. Worth deciding early whether that is in scope.

8. **Peer connections are never closed and hold `Arc`s to themselves**, so every join leaks a
   PC, an ICE agent with open UDP sockets, and an `injector` OS thread. Because a settings change
   is a full rejoin, this accumulates faster than "once per device". The input territory owns the
   injector thread; the leak is jointly theirs and mine.

9. **Unmeasured.** Everything in §11 is derived from reading code, not from a profile. I could
   not run the host from this tree. The three numbers I would want first: actual
   `start_on_monitor` duration, actual `wait_for_new_monitor` duration on a healthy IDD, and
   whether the client's 3 s gather wait is real or resolves in tens of milliseconds when STUN is
   reachable.

# I — macOS system audio (`src-tauri/src/macos_utils/audio/`)

Territory: every file under `src-tauri/src/macos_utils/audio/` (excluding the `test/` subdirectories,
which belong to another agent), plus `src-tauri/macos/ScreenExtendAudio/src/shm_ring.hpp` as the ABI
contract for `legacy/shm_reader.rs`. `device.cpp`, `device.hpp`, `main.cpp`, `shm_ring.cpp`,
`branding.hpp` and `volume_control.cpp` were read as supporting evidence for the contract.

Note on paths: `CLAUDE.md` places the driver project at `macos/ScreenExtendAudio/`; it is actually at
`src-tauri/macos/ScreenExtendAudio/`. `CLAUDE.md` also states the current version is 0.5.4; all four
version files say 0.6.3.

---

## 1. Files and purpose

**`mod.rs` (379 lines)** — the tier selector and the encoder worker. Declares `AudioBackend`
(`ProcessTap | ScreenCaptureKitAudio | VirtualDevice | NeedsDriverInstall | Unsupported`), the
`AudioSource` trait every backend implements, the `AudioFrameSink` handed to a backend at start, and
`probe_audio_backend()`. `start_capture()` builds the SPSC ring, the control channel and the packet
channel, spawns the `macos-audio` thread, and blocks until the worker reports the Opus lookahead.
`worker()` is the drain loop: pop 5 ms of interleaved stereo f32 out of the ring, Opus-encode it,
stamp it, send it. Also owns the 2-second diagnostics log and the "only silence for 5 s" warning.

**`ring.rs` (147 lines)** — a hand-rolled SPSC f32 ring: `Box<[UnsafeCell<f32>]>` with absolute
`AtomicUsize` head/tail, a power-of-two mask, and an overrun counter. `Producer` is held by the
real-time capture callback, `Consumer` by the encoder worker. Push is drop-new on overflow and never
blocks. It also carries the `OnceLock<Thread>` used to unpark the consumer (see §4).

**`format.rs` (208 lines)** — `AudioStreamBasicDescription` parsing into `AudioFormatDesc`
(`sample_rate`, `channels`, `SampleKind::{F32,I16,I32}`, `non_interleaved`), plus the two conversion
routines every backend funnels through: `convert_interleaved` and `convert_planar`, both producing
interleaved stereo f32 with an ITU-R BS.775 downmix for 1/3/4/6/8-channel sources. Planar handling is
the macOS-specific half — Windows never produces it.

**`opus_encoder.rs` (3 lines)** — a pure re-export of `streamer::audio::encoder`'s `OpusEncoder`,
`OpusEncoderConfig` and `FRAME_SAMPLES`, plus `FRAME_INTERLEAVED = FRAME_SAMPLES * 2` (= 480 samples
× 2 = 960 f32 per 5 ms frame). No macOS-specific code at all.

**`process_tap.rs` (772 lines)** — the preferred backend on 14.2+. Builds a `CATapDescription`
entirely through the Obj-C runtime, creates the tap with a `dlsym`'d
`AudioHardwareCreateProcessTap`, builds a private aggregate device whose tap-list entry carries the
tap's UUID, registers an `AudioDeviceIOProc` on it, and starts it. The IOProc converts to
interleaved stereo f32 in preallocated scratch and pushes into the ring. Also owns a
default-output-device property listener that posts `ControlMsg::Reacquire`, and the RAII
`TapResources` teardown. All the CoreFoundation container plumbing for the aggregate dictionary is
hand-declared at the bottom of the file.

**`sck_audio.rs` (557 lines)** — the 13.0+ fallback. ScreenCaptureKit has no audio-only mode, so it
rides a 2×2 pixel, 1 fps dummy video stream with `capturesAudio` on. Everything is runtime interop:
`AnyClass::get` for the SC classes, a `ClassBuilder`-constructed `SCStreamOutput` delegate with a
raw `*mut OutputCtx` ivar, `RcBlock` completion handlers bridged to `std::sync::mpsc` for the
async→sync boundary. The sample handler pulls an `AudioBufferList` out of the `CMSampleBuffer` and
runs the same convert-and-push as the tap.

**`legacy/mod.rs` (284 lines)** — the 10.15–12.x virtual-device tier's orchestrator.
`LegacyVirtualDeviceSource::start` finds the driver's device by UID, forces a 128-frame buffer,
creates a second "monitor" ring for local playthrough, activates the `Router` (saves and replaces
the default output), installs the crash-recovery LaunchAgent, starts the shm reader, starts
playthrough on the previously-default device, applies the volume proxy, registers routing/volume/
format listeners and spawns the `se-audio-legacy-ctl` thread that reconciles all of it.

**`legacy/branding.rs` (11 lines)** — six string constants shared with the C++ driver: device UID,
device name, bundle id, install path, helper id, shm name.

**`legacy/probe.rs` (37 lines)** — `driver_bundle_installed()` (path stat), `device_present()` /
`driver_healthy()` (HAL lookup by UID), `eligible_os()` (a version window), and the
`LegacyState::{Ready, NeedsInstall, InstalledButUnhealthy}` tri-state.

**`legacy/hal.rs` (268 lines)** — thin typed wrappers over `AudioObject{Get,Set}PropertyData`:
default output device get/set, device UID, device enumeration, `device_by_uid`, output channel count
via `kAudioDevicePropertyStreamConfiguration`, nominal sample rate, volume scalar, mute, and the two
hand-fourcc'd buffer-frame-size selectors (`fsiz`, `fsz#`).

**`legacy/shm_reader.rs` (379 lines)** — the transport. `ShmMap::open` maps the driver's POSIX shared
segment read-only and validates the header; `shm_reader_loop` polls `writePos`, detects generation
bumps and ring laps, and publishes each drained chunk to both the encoder ring and the monitor ring.
`HalInputProc` is the fallback when the segment is unavailable: a plain input IOProc on the virtual
device's input stream. `clamp_read` is the lap-detection primitive, factored out for testing.

**`legacy/playthrough.rs` (285 lines)** — plays the captured mix back to the real output device so
the host still hears itself. An output IOProc drains the monitor ring, linearly resamples
48 kHz → the device rate with a slow drift-trim control loop, applies a smoothed gain, and writes to
the output buffers. `MonitorGain` is the atomic f32 the volume proxy writes.

**`legacy/routing.rs` (449 lines)** — default-output-device custody. `Router` saves the current
default, sets ours, watches for the user switching away, and restores on stop; `RoutingState` is the
JSON file that makes that survive a crash, with `recover()`/`recover_on_launch()`/`watchdog_recover()`
as the recovery paths and `install_watchdog_agent()` as a LaunchAgent that retries every 30 s. Also
holds the `RoutingListeners` / `FormatListener` HAL property listeners and
`preferred_playthrough_device`.

**`legacy/volume_proxy.rs` (115 lines)** — mirrors the virtual device's Volume/Mute controls onto the
`MonitorGain` and onto the real output device, driven by `VolumeListeners` on
`kAudioDevicePropertyVolumeScalar` / `kAudioDevicePropertyMute`.

**`legacy/volume_keys.rs` (272 lines)** — the backstop for when the OS does not route the F10/F11/F12
keys to the virtual device's controls. A `CGEventTap` on `NX_SYSDEFINED` decodes the aux-button
subtype, nudges the virtual device's volume by 1/16 or toggles mute, and swallows the event. Managed
by a process-global `Mutex<Manager>` reconciled from `set_enabled`/`bind_device`/`unbind`.

**`legacy/installer.rs` (118 lines)** — runs the `.pkg` under `elevated_command`, polls for the
device to appear, and the mirror-image uninstall (`rm -rf` the bundle, kickstart `coreaudiod`,
`shm_unlink`).

**`src-tauri/macos/ScreenExtendAudio/src/shm_ring.hpp` (64 lines)** — the 64-byte header plus
131072-float ring, with `static_assert`s pinning `sizeof` and two offsets. This is the ABI the Rust
reader mirrors by hand.

---

## 2. Function catalog

### `mod.rs`
- `AudioBackend::as_str` — stable string for the compatibility report and logs.
- `AudioSource` trait — `start`, `stop`, `backend_name`, `reacquire` (default `Ok(())`),
  `nonsilent_samples` (default 0).
- `probe_audio_backend()` — caches the *native* choice in a `OnceLock<Option<AudioBackend>>`,
  recomputes the legacy branch on every call, returns `NeedsDriverInstall`/`Unsupported` as the
  terminal states.
- `capable_native_backends()` — `[ProcessTap?, ScreenCaptureKitAudio?]` in preference order, each
  gated on a version check **and** a runtime capability probe.
- `capable_backends()` — native list, plus `VirtualDevice` if empty and the driver is healthy.
  **Bypasses the `OnceLock`**; this is what `start_capture` uses.
- `make_backend(kind)` — constructor dispatch; `unreachable!()` for the two terminal states.
- `start_capture()` — the public entry point (called from `streamer/platform.rs:149`). Builds
  ring + control channel + packet channel, spawns `macos-audio`, blocks on `ready_rx.recv()`,
  returns `AudioCapture { rx, stop, format, diagnostics, lookahead_samples }`.
- `worker(...)` — sets QoS, constructs the Opus encoder, tries each candidate backend in order,
  reports readiness, then loops: drain control messages → drain ring in 960-sample frames → encode →
  stamp → send → publish overruns → periodic diag → park 125 µs when idle.
- `update_encode_percentiles(samples, diagnostics)` — clones and sorts the 4096-entry window every
  2 s to compute p50/p99.
- `os_version()` — forks `sw_vers -productVersion`; error path only.

### `ring.rs`
- `ring(capacity)` — allocates, rounds capacity up to a power of two, returns
  `(Producer, Consumer, Arc<OnceLock<Thread>>)`.
- `Producer::set_consumer_thread(t)` / `Producer::push(&[f32]) -> dropped` / `Producer::overruns()`.
- `Consumer::available()` / `Consumer::pop(&mut [f32]) -> n` / `Consumer::overruns()`.

### `format.rs`
- `SampleKind::bytes()` — 4/2/4.
- `AudioFormatDesc::is_fast_path()` — 48 kHz stereo f32 interleaved. **Never called outside tests.**
- `parse_asbd(&ASBD) -> Result<AudioFormatDesc>` — float/signed-int discrimination, bit-depth
  validation, non-interleaved flag. Does not check `mFormatID` or the big-endian flag.
- `read_sample(kind, bytes, off) -> f32` — byte-wise reassembly; i16 divides by 32768, i32 by 2^31.
- `downmix_channels(&[f32]) -> (f32, f32)` — BS.775 for 1/2/3/4/6/8; LFE dropped; unknown layouts
  averaged.
- `convert_interleaved(src, frames, desc, out) -> written`.
- `convert_planar(planes, frames, desc, out) -> written`.

### `process_tap.rs`
- `tap_fns()` — `dlsym(RTLD_DEFAULT)` for `AudioHardwareCreateProcessTap` and
  `AudioHardwareDestroyProcessTap`.
- `runtime_available()` — `tap_fns().is_some() && AnyClass::get(c"CATapDescription").is_some()`.
- `tap_ioproc(...)` — the real-time callback. Reads the `AudioBufferList`, picks planar or
  interleaved, converts into `IoCtx::scratch`, counts non-silent samples, pushes into the ring, adds
  drops to diagnostics. No allocation. One lock (via `unpark`, §4).
- `TapResources::drop` — stop → `AudioDeviceDestroyIOProcID` → free ctx →
  `AudioHardwareDestroyAggregateDevice` → `destroy_tap`. Matches the documented order.
- `default_device_listener(...)` — `try_send(ControlMsg::Reacquire)`, nothing else.
- `default_device_listener_address()` — global/main address for
  `kAudioHardwarePropertyDefaultOutputDevice`.
- `ProcessTapCapture::new` / `try_create` (**dead**) / `build_and_start` / `register_device_listener`
  / `remove_device_listener`.
- `build_and_start` in order: resolve fns → get `CATapDescription`/`NSArray`/`NSUUID` classes →
  `initStereoGlobalTapButExcludeProcesses:[]` → set UUID / muteBehavior 0 / private / name → create
  tap → read tap ASBD → read default **system** output device and its UID → force that device to
  48 kHz → create aggregate → re-read ASBD → reject non-48 kHz → allocate `IoCtx` →
  `AudioDeviceCreateIOProcID` → `AudioDeviceStart`.
- `AudioSource for ProcessTapCapture` — `start` (build + register listener), `reacquire` (drop
  resources, rebuild), `stop` (remove listener, drop resources), `nonsilent_samples` (reads through
  the raw `ctx`).
- `ns_string`, `nsstring_to_string`, `uuid_string`, `cfstr`, `cfstr_cstr` — Obj-C/CF string helpers.
- `get_property<T>`, `read_default_system_output_device`, `read_device_uid`, `read_tap_format`,
  `set_nominal_sample_rate`.
- `create_aggregate_device(output_uid, tap_uuid)` — builds the 8-key dictionary
  (name, UID, main sub-device, private=true, stacked=false, tap-auto-start=true, sub-device list,
  tap list), releases every intermediate, calls `AudioHardwareCreateAggregateDevice`. **The tap-list
  entry's `kAudioSubTapUIDKey` is the `CATapDescription`'s own UUID string** — the invariant that
  separates a working tap from one that returns `noErr` and silence.

### `sck_audio.rs`
- `SckAudioCapture::runtime_available()` — four `AnyClass::get` lookups.
- `SckAudioCapture::try_create()` — **dead**.
- `SckAudioCapture::build_and_start` — resolve classes → `resolve_any_display` → content filter →
  config (`capturesAudio`, 48 kHz, 2 ch, exclude own audio, 2×2, 1 fps) → `OutputCtx` → delegate →
  `SCStream` → `UserInteractive` serial queue → `addStreamOutput:type:1` →
  `startCaptureWithCompletionHandler` bridged to a 5 s `recv_timeout`.
- `AudioSource for SckAudioCapture` — `start`, `stop` (blocking stop + release + free ctx),
  `backend_name`, `nonsilent_samples` (through a raw `*const AtomicU64`).
- `delegate_class()` — `OnceLock<usize>`; reuses an existing class if registered, else
  `ClassBuilder` with a `*mut c_void` ivar, the `stream:didOutputSampleBuffer:ofType:` method and the
  `SCStreamOutput` protocol.
- `did_output_audio(...)` — the objc method; filters on output type, loads the ivar, forwards.
- `handle_audio_sample(sample, ctx)` — the real work: format description → ASBD → `parse_asbd` →
  `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` into a 4 KiB scratch → planar or
  interleaved convert → silence scan → ring push.
- `runtime_class`, `retain_new`, `err_setup`, `set_ctx_ivar`, `get_ctx_ivar`, `resolve_any_display`
  (5 s blocking), `first_display`, `nserror_description`.

### `legacy/mod.rs`
- `LegacyVirtualDeviceSource::{new, default}`; `AudioSource` impl — `start` (13 ordered steps, see
  §1), `stop` (drop the control handle, unbind volume keys), `backend_name`, `reacquire` (no-op),
  `nonsilent_samples`.
- `ControlHandle::drop` — send stop, join the control thread.
- `ControlRuntime::run` — `crossbeam_channel::select!` over stop/routing/volume; on exit restores the
  default output device.
- `ControlRuntime::on_routing` — `DefaultOutput` → ignore or release playthrough; `DeviceList` →
  `repoint_playthrough`; `PlaythroughFormat` → re-sync.
- `ControlRuntime::repoint_playthrough` / `set_playthrough(device)` — RAII swap of the playthrough
  IOProc and its format listener, then re-apply the volume proxy.

### `legacy/probe.rs`
- `driver_bundle_installed`, `device_present`, `driver_healthy`, `eligible_os`, `legacy_state`.

### `legacy/hal.rs`
- `addr`, `global`, `get_pod<T>`, `set_pod<T>`, `default_output_device`, `set_default_output_device`,
  `device_uid`, `all_devices`, `device_by_uid`, `has_output_channels`, `stream_channel_count`,
  `nominal_sample_rate`, `output_volume_scalar`, `set_output_volume_scalar`, `output_mute`,
  `set_output_mute`, `buffer_frame_size_range`, `buffer_frame_size`, `set_buffer_frame_size`.

### `legacy/shm_reader.rs`
- `clamp_read(read, write, capacity) -> (new_read, avail, lapped)`.
- `Transport::as_str`.
- `CaptureTargets::publish(&[f32])` — silence scan, encoder push (counted), monitor push (uncounted).
- `ShmMap::{open, u32_at, atomic_u64_at, ring_sample, header_valid}`; `Drop` munmaps.
- `Reader::transport`; `Drop` sets stop and joins.
- `start(device_id, targets) -> Reader` — try shm, else HAL input.
- `shm_reader_loop(map, targets, stop)` — the poll loop.
- `input_ioproc(...)` — the HAL-input fallback callback (no ASBD parsing).
- `HalInputProc::start`; `Drop` stops/destroys/frees.

### `legacy/playthrough.rs`
- `MonitorGain::{new, set, target}` — an `AtomicU32` holding f32 bits.
- `ramp(current, target)` — one-pole smoothing at 1/48 per sample.
- `drift_trim(trim, avail_frames, setpoint)` — the integral controller, clamped to ±0.3 %.
- `playthrough_ioproc(...)` — measure backlog → update trim → pull `ceil(dst*ratio)+2` source frames
  → linear-interpolate → apply ramped gain → write interleaved or planar → save history.
- `Playthrough::{start, gain}` (`gain` is **dead**); `Drop` stops/destroys/frees.

### `legacy/routing.rs`
- `DefaultDevicePort` trait (`current_default_uid`, `set_default_uid`, `fallback_output_uid`) and its
  `HalDefaultDevicePort` impl — the seam that makes `Router` testable.
- `RoutingState::{load, save, clear}`.
- `Router::{new, activate, restore, on_default_changed, is_active, saved_uid}`.
- `recover(port, our_uid, path) -> bool`, `recover_on_launch()`, `watchdog_recover()`,
  `another_instance_running()` (forks `pgrep`).
- `default_state_path()`, `watchdog_plist_path()`, `install_watchdog_agent()`.
- `routing_listener(...)`, `system_addr`, `RoutingListeners::register` + `Drop`,
  `FormatListener::register` + `Drop`.
- `preferred_playthrough_device(our_uid, saved_uid)`, `now_secs()`.

### `legacy/volume_proxy.rs`
- `compute_gain(scalar, muted)`, `apply(our_device, real_device, gain)`, `volume_listener(...)`,
  `output_addr`, `VolumeListeners::register` + `Drop`.

### `legacy/volume_keys.rs`
- `nudge_volume`, `toggle_mute`, `tap_callback(...)`, `VolumeKeyTap::start` + `Drop`,
  `tap_thread(...)`, `env_forced`, `desired`, `reconcile`, `set_enabled`, `bind_device`, `unbind`,
  `is_active`.

### `legacy/installer.rs`
- `InstallOutcome::as_str`, `run_privileged(shell_cmd)`, `bundle_icon_bytes()`, `install_pkg(path)`,
  `uninstall()`, `health()`.

### `shm_ring.hpp` / `shm_ring.cpp` (C++ producer side)
- `ShmRing::Open()` — `shm_open(O_CREAT|O_RDWR, 0666)`, `ftruncate`, `mmap`, initialise the header
  once (magic published last, behind a release fence), then `generation.fetch_add(1, release)`.
- `ShmRing::Write(frames, count)` — relaxed load of own `writePos`, element-wise store into the ring,
  release store of `writePos + count`. **Never checks the reader; never blocks; never increments
  `overruns`.**

---

## 3. Threads

| Thread | Spawned at | Priority / QoS | Lifetime | Owns |
|---|---|---|---|---|
| `macos-audio` (encoder worker) | `mod.rs:167-180` | `pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED)` (`mod.rs:226` → `qos.rs:25-29`) | from `start_capture` until the `AudioStopFn` sets the flag and joins | the `OpusEncoder`, the ring `Consumer`, the `Box<dyn AudioSource>`, the sequence counter, the encode-latency window |
| CoreAudio HAL IOProc thread (Process Tap) | created by `AudioDeviceCreateIOProcID` (`process_tap.rs:346-353`), started at `process_tap.rs:361` | real-time, owned by the HAL | tap lifetime | `IoCtx` (scratch, non-silent counter, `Arc<Producer>`) |
| SCK delivery queue (`com.screenextend.sck.audio`) | `sck_audio.rs:156-161`, serial, targeting the global `DispatchQoS::UserInteractive` queue | user-interactive | stream lifetime | `OutputCtx` (two scratch buffers, counter, `Arc<Producer>`) |
| `se-audio-shm` | `legacy/shm_reader.rs:188-191` | `THREAD_TIME_CONSTRAINT_POLICY`, period 5 ms (`shm_reader.rs:213` → `qos.rs:31-54`) | until `Reader::drop` | the `ShmMap`, a 4096-f32 scratch, both `Producer`s |
| `se-audio-legacy-ctl` | `legacy/mod.rs:150-153` | default | until `ControlHandle::drop` | the `Router`, the `Reader`, the `Playthrough`, all HAL listeners |
| Playthrough IOProc thread | `AudioDeviceCreateIOProcID` (`playthrough.rs:231-238`), started `playthrough.rs:244` | real-time, owned by the HAL | playthrough lifetime | its own `IoCtx` (monitor `Consumer`, resampler state, 16384-f32 scratch) |
| HAL-input IOProc thread | `shm_reader.rs:322-329`, started `shm_reader.rs:335` | real-time | fallback lifetime | `HalInputCtx` |
| `se-audio-volkeys` | `volume_keys.rs:149-152` | default | until `CFRunLoopStop` + join | the `CGEventTap` port, its run-loop source, `TapCtx` |
| CoreAudio property-listener threads | `AudioObjectAddPropertyListener` in `process_tap.rs:290-297`, `routing.rs:331-338`, `routing.rs:393-400`, `volume_proxy.rs:81-88` | HAL-owned, not ours | listener lifetime | nothing; each callback does one `try_send` and returns |
| `audio-bridge` (outside the territory but on the path) | `streamer/audio/mod.rs:154-160` | default | until the capture channel closes | forwards `AudioPacket` into the tokio broadcast |

`start_capture` also blocks the **calling** thread on `ready_rx.recv()` (`mod.rs:182`) with no
timeout, and the caller is `AudioHub::subscribe` holding `AudioHub::inner`.

---

## 4. Locks

**The audio data path is deliberately lock-free — with one exception.**

- `ring.rs` `Shared { head: AtomicUsize, tail: AtomicUsize, overruns: AtomicU64 }` — SPSC.
  `push` does relaxed-load head / acquire-load tail / release-store head; `pop` does relaxed-load
  tail / acquire-load head / release-store tail. Correct, and the only shared state on the hot path.
- **`Producer::push` calls `Thread::unpark()` (`ring.rs:68-72`) from inside the real-time capture
  callback.** Rust's macOS parker takes a `pthread_mutex` in `unpark` whenever the target is parked,
  and the consumer parks whenever the ring is short of a frame (`mod.rs:351-353`) — i.e. every idle
  gap. This is a blocking, non-priority-inheriting lock acquisition on a real-time thread. Filed as
  `major-audio-mac-unpark-inside-realtime-callback.md`.
- `OnceLock<Thread>` (`ring.rs:19`, `mod.rs:68`) — `get_or_init`, written twice at
  `mod.rs:259-260` (redundantly, same value, same `Arc`), read on every push.
- `OnceLock<Option<AudioBackend>>` in `probe_audio_backend` (`mod.rs:84`) — the native-tier cache.
- `OnceLock<usize>` in `sck_audio.rs:286` — the delegate class pointer.
- `static STATE: Mutex<Manager>` in `volume_keys.rs:225-228` — the only `std::sync::Mutex` in the
  territory. Taken by `set_enabled`, `bind_device`, `unbind`, `is_active`. **`bind_device`
  (`volume_keys.rs:257-262`) and `set_enabled` (`volume_keys.rs:252-255`) hold it across
  `VolumeKeyTap::start`, which blocks on `ready_rx.recv()` with no timeout** — a thread handshake
  under a global mutex. `.unwrap()` on the guard means one panic in the tap thread poisons it
  permanently.
- Atomics used as one-way channels: `IoCtx::nonsilent_samples`, `MonitorGain::target_bits` (f32 bits,
  relaxed, written by the control thread and read per sample by the playthrough IOProc),
  `IoCtx::underruns`, all of `AudioDiagnostics`.
- Cross-process atomics: `writePos` and `generation` in the shared-memory header, read through
  `&AtomicU64` over a `PROT_READ` mapping (`shm_reader.rs:131-134`).

**`.await` exposure: none.** Nothing in this territory is `async`. No `MutexGuard` can cross an
`.await` because there are no awaits. The `if let` scrutinee temporary-lifetime pattern appears once,
at `ring.rs:69` (`if let Some(t) = self.consumer_thread.get() { t.unpark(); }`), but the scrutinee is
a `OnceLock::get`, not a guard, so there is no held lock — the hazard there is the `unpark` itself.
The only place a lock and a blocking wait coincide is `volume_keys::STATE` above.

Where the territory *does* touch async code is one hop out: `AudioHub::subscribe` /
`unsubscribe` (`streamer/audio/mod.rs:147, 194`) hold a `std::sync::Mutex` across
`start_capture()` and across the `AudioStopFn`, and the latter can block for up to 5 s on the SCK
backend (`sck_audio.rs:238`). That guard is not held across an `.await`, but it does serialise every
session's audio subscribe behind a multi-second teardown.

---

## 5. Buffers and copies

Per 5 ms Opus frame (960 interleaved f32 = 3840 bytes), on the Process Tap / SCK path:

1. **HAL/SCK → callback**: `AudioBufferList` pointing at kernel/CoreMedia memory. Zero-copy, read
   only. For SCK the block buffer is retained for the duration of `handle_audio_sample`
   (`sck_audio.rs:376-379`).
2. **Convert into scratch**: `format::convert_interleaved` / `convert_planar` — a **CPU
   sample-by-sample pass** (`read_sample` reassembles each sample from individual bounds-checked
   bytes, `downmix_channels` runs even for stereo, two `clamp`s, two stores). Destination is a
   preallocated `Box<[f32]>`: 16384 f32 = 64 KiB for the tap (`process_tap.rs:334`), the same for SCK
   plus a 4 KiB `AudioBufferList` scratch (`sck_audio.rs:132-133`). **No allocation per frame.**
   `AudioFormatDesc::is_fast_path()` exists but is never consulted, so even a 48 kHz stereo f32
   interleaved source — the common case — takes the slow path.
3. **Silence scan**: a *second* full pass over the scratch (`process_tap.rs:143-148`,
   `sck_audio.rs:444-449`) computing `abs() > 1/32768`.
4. **Scratch → ring**: `Producer::push` — a **third** pass, element-wise through `UnsafeCell`, not a
   memcpy (`ring.rs:60-66`). Ring is 8192 f32 = 32 KiB (`mod.rs:25`, `48_000*2*80/1000` rounded up),
   i.e. 85.3 ms of headroom. Drop-new on overflow.
5. **Ring → worker frame**: `Consumer::pop` into a `[f32; 960]` stack array (`mod.rs:262`) — a
   **fourth** pass, also element-wise.
6. **Fifth pass**: the silence check in the worker (`mod.rs:290`, `frame.iter().all(|&x| x == 0.0)`)
   — note this uses exact zero while the callbacks use a −90 dBFS threshold, so the two disagree.
7. **Opus encode**: `encoder.encode_float(&frame)` returns a borrowed slice of the encoder's internal
   output buffer.
8. **`Bytes::copy_from_slice(encoded)`** (`mod.rs:317`) — **one heap allocation per 5 ms frame**,
   typically ~80 bytes at 128 kbit/s. This is the only per-frame allocation in the pipeline.
9. **`pkt_tx.send(pkt)`** — moves the `AudioPacket` (24 bytes + `Bytes`) into an **unbounded**
   crossbeam channel.
10. Out of territory: the `audio-bridge` thread moves it again into a `tokio::sync::broadcast`
    (capacity 128), which clones the `Bytes` handle (refcount bump, not a data copy) per subscriber.

On the legacy path, insert before step 4:
- Driver `OnProcessMixedOutput` → `ShmRing::Write`: element-wise store into the mapped ring
  (`shm_ring.cpp:77-79`), plus a **second** element-wise write into the driver's own 65536-frame
  loopback ring for the HAL-input fallback (`device.cpp:46-51`) — that second copy happens on every
  callback whether or not the fallback is in use.
- `shm_reader_loop` → 4096-f32 scratch: element-wise `ptr::read_unaligned` per sample
  (`shm_reader.rs:249-251`).
- `CaptureTargets::publish` pushes the **same** slice into two rings (`shm_reader.rs:82, 88`) — the
  encoder ring (8192 f32) and the monitor ring (65536 f32 = 683 ms, `legacy/mod.rs:27`).
- The playthrough IOProc pops from the monitor ring into a 16384-f32 scratch and then resamples
  frame-by-frame into the device's output buffers, reconstructing the destination slice **inside**
  the per-frame loop (`playthrough.rs:154-156`).

Nothing here is GPU-resident, so there is no GPU↔CPU readback in this territory.

---

## 6. Queues

| Queue | Where | Depth | Bounded | On full | Back-pressure |
|---|---|---|---|---|---|
| capture SPSC ring | `mod.rs:152`, `ring.rs` | 8192 f32 = 4096 frames = **85.3 ms** | yes | drop **newest**, `overruns += dropped` | none — producer never blocks |
| monitor SPSC ring (legacy) | `legacy/mod.rs:27, 81` | 65536 f32 = 32768 frames = **683 ms** | yes | drop newest, silently (`let _ =` at `shm_reader.rs:88`) | none |
| shared-memory ring (legacy, cross-process) | `shm_ring.hpp:17` | 131072 f32 = 65536 frames = **1.365 s** | yes | producer **overwrites**; reader detects the lap via `clamp_read` and jumps forward | none by design |
| driver loopback ring (legacy) | `device.hpp:68` | 65536 frames = 1.37 s | yes | overwrite | none |
| `ctrl_tx/rx` (`ControlMsg::Reacquire`) | `mod.rs:153` | 8 | yes | `try_send` drops (`process_tap.rs:199`, `shm_reader.rs:229`) | none — correct, these are edge notifications |
| `pkt_tx/rx` (`AudioPacket`) | `mod.rs:161` | **unbounded** | **no** | grows | none |
| `routing_tx/rx` | `legacy/mod.rs:125` | 16 | yes | `try_send` drops | none |
| `volume_tx/rx` | `legacy/mod.rs:126` | 16 | yes | `try_send` drops | none |
| `stop_tx/rx` | `legacy/mod.rs:131` | 1 | yes | — | — |
| `ready_tx/rx` (setup handshake) | `mod.rs:162` | mpsc, unbounded, one message | — | — | blocks `start_capture` with **no timeout** |
| SCK completion handlers | `sck_audio.rs:186, 232` | 1 each | — | — | 5 s `recv_timeout` |
| `resolve_any_display` | `sck_audio.rs:500` | 1 | — | — | 5 s `recv_timeout` |

The only unbounded queue on the data path is `pkt_tx`. Filed as
`minor-audio-mac-unbounded-packet-channel-and-blocking-teardown.md`.

---

## 7. Syscall / FFI boundaries

**Runtime-resolved (dyld-safe on the 10.15 floor):**
- `libc::dlsym(RTLD_DEFAULT, "AudioHardwareCreateProcessTap" / "AudioHardwareDestroyProcessTap")`
  (`process_tap.rs:38-52`) — 14.2+, transmuted to `extern "C-unwind" fn`. Correct.
- `AnyClass::get(c"CATapDescription")` (`process_tap.rs:59`, `271`) — 14.2+.
- `AnyClass::get` for `SCStream`, `SCShareableContent`, `SCContentFilter`, `SCStreamConfiguration`
  (`sck_audio.rs:67-72`) — 13.0+. **There is no `objc2-screen-capture-kit` dependency**
  (verified against `src-tauri/Cargo.toml`), so no SCK symbol is bound at link time.
- Every SCK message is `msg_send!` through the runtime; the delegate class is built at runtime with
  `ClassBuilder` (`sck_audio.rs:292-312`) and adopts `SCStreamOutput` only if
  `AnyProtocol::get` finds it.
- `AnyClass::get` for `NSString`, `NSArray`, `NSUUID`, `NSObject` — all floor-present, but still
  resolved at runtime.

**Link-time (all floor-present; verified by API vintage, not by `nm`):**
- `objc2-core-audio` (feature `AudioHardware`): `AudioObjectGetPropertyData`,
  `AudioObjectGetPropertyDataSize`, `AudioObjectSetPropertyData`, `AudioObjectAddPropertyListener`,
  `AudioObjectRemovePropertyListener` (10.4+), `AudioDeviceCreateIOProcID`,
  `AudioDeviceDestroyIOProcID`, `AudioDeviceStart`, `AudioDeviceStop`,
  `AudioHardwareCreateAggregateDevice`, `AudioHardwareDestroyAggregateDevice` (10.5+).
- The 14.2-era **keys** — `kAudioAggregateDeviceTapListKey`, `kAudioSubTapUIDKey`,
  `kAudioSubTapDriftCompensationKey`, `kAudioAggregateDeviceTapAutoStartKey` — are consumed as
  `&CStr` compile-time constants (`cfstr_cstr(kAudioSubTapUIDKey)`, `process_tap.rs:359-360`), and
  `kAudioTapPropertyFormat` is consumed as a `u32` selector (`process_tap.rs:579`). They are
  `#define`d string/fourcc literals in the CoreAudio headers, **not exported symbols**, so they add
  no undefined dyld symbol. This is the one place where the "extern static counts" rule could have
  bitten and does not.
- Hand-declared CoreFoundation externs (`process_tap.rs:610-635`): `kCFTypeDictionaryKeyCallBacks`,
  `kCFTypeDictionaryValueCallBacks`, `kCFTypeArrayCallBacks`, `kCFBooleanTrue`, `kCFBooleanFalse`
  (**these are genuine `extern static`s**, all 10.0-era), plus `CFStringCreateWithCString`,
  `CFArrayCreate`, `CFDictionaryCreate`, `CFRelease`.
- `objc2-core-media`: `CMSampleBufferGetFormatDescription` (10.7+),
  `CMAudioFormatDescriptionGetStreamBasicDescription` (10.7+),
  `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` (10.10+), and the extern static
  `kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment` (10.7+).
- `dispatch2`: `dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE)` and
  `dispatch_queue_create_with_target` (10.10+ / 10.12+).
- `volume_keys.rs:19-47`: `#[link(name = "CoreGraphics")]` `CGEventTapCreate`, `CGEventTapEnable`
  (10.4+); `#[link(name = "CoreFoundation")]` `CFMachPortCreateRunLoopSource`, `CFRunLoopGetCurrent`,
  `CFRunLoopAddSource`, `CFRunLoopRun`, `CFRunLoopStop`, `CFRunLoopSourceInvalidate`,
  `CFMachPortInvalidate`, `CFRelease`, and the extern static `kCFRunLoopCommonModes` (10.5+).
- `mach2` via `qos.rs`: `thread_policy_set`, `mach_thread_self`, plus the hand-declared
  `pthread_set_qos_class_self_np` (10.10+).
- POSIX: `shm_open`, `mmap`, `munmap`, `close`, `shm_unlink`, `getuid` (`shm_reader.rs`,
  `installer.rs`, `routing.rs`).

**Subprocesses:** `sw_vers` (via `macos_at_least`, on every call — §11), `pgrep`
(`routing.rs:210`), `launchctl bootstrap` (`routing.rs:270`), and under elevation
`/bin/sh -c` wrapping `/usr/sbin/installer`, `/bin/rm`, `/bin/launchctl kickstart`,
`/usr/bin/killall` (`installer.rs:31, 67, 92`). None of these go through the Tauri shell plugin, so
`capabilities/default.json` does not constrain them.

**Versioning structs:** `AudioObjectPropertyAddress` and `AudioStreamBasicDescription` are fixed
layouts, constructed field-by-field. `AudioValueRange` is hand-declared `#[repr(C)]`
(`hal.rs:250-255`) and matches CoreAudio's two-`Float64` definition. The two buffer-size selectors
are hand-fourcc'd from byte literals (`hal.rs:247-248`, `"fsiz"` / `"fsz#"`) because
`objc2-core-audio` does not export them. `JoinToken([u8; 64])` in `qos.rs:103` is a guessed size for
`os_workgroup_join_token_s` — outside this territory but on the same trust boundary.

**Calling conventions:** every callback is `extern "C-unwind"` (correct for callbacks that could
observe a Rust panic), and `client_data` is always a `Box::into_raw` pointer null-checked on entry.

**Definitive dyld verification (`nm -u target/release/ScreenExtend`) cannot be run from this Windows
tree.** The audit above is by API vintage and by inspection of how each constant is consumed.

---

## 8. Panics

Filed in full as `minor-audio-mac-swallowed-errors-and-panicking-unwraps.md`. Summary of the ones a
runtime condition can actually reach:

- `volume_keys.rs:254, 258, 271` — `STATE.lock().unwrap()`; poisoned by any panic in the tap thread,
  and the mutex is held across a blocking handshake.
- `sck_audio.rs:292-295` — `.expect("NSObject present")` and
  `.expect("ClassBuilder for SCK audio delegate")`; the latter returns `None` if a class of that name
  already exists, guarded non-atomically by an `AnyClass::get` two lines above.
- `sck_audio.rs:527, 537` — `Retained::from_raw(...).unwrap()` / `Retained::retain(d).unwrap()` on
  objects that came back non-null but could be autoreleased out from under a slow handler.
- `process_tap.rs:223, 227, 766` — `.expect("NSString present")`, `CString::new(s).unwrap()`,
  `.expect("NSUUID present")`.
- `mod.rs:128-131` — `unreachable!("capable_backends never yields a non-startable backend")`.
  Unreachable via `capable_backends()` today, but `make_backend` accepts any `AudioBackend`.
- `mod.rs:366` — `sorted[((len as f64 * q) as usize).min(len - 1)]`; the `is_empty` guard at
  `mod.rs:361` makes `len - 1` safe.
- Indexing that is bounded by construction but worth naming: `format.rs:85-95` indexes `bytes[off..]`
  after a `base + stride > src.len()` check in the interleaved case and an `off + bps > plane.len()`
  check in the planar case — both correct; `process_tap.rs:100` and `sck_audio.rs:406` index
  `bufs[0]` after checking `mNumberBuffers != 0`.
- `playthrough.rs:155-176` — the output slices are built from `mDataByteSize`, and `f` is bounded by
  `dst_frames` derived from the same field, so the indexing is consistent.
- `process_tap.rs:653-661` — `CFRelease(*s)` over every key/value; if any `CFStringCreateWithCString`
  returned null (out of memory, or a key with an interior NUL — neither possible with the current
  constants) this would be a null `CFRelease`, which crashes rather than panics.

---

## 9. Silent fallbacks and swallowed errors

The full list is in `minor-audio-mac-swallowed-errors-and-panicking-unwraps.md`. The ones that
change behaviour rather than just hiding a message:

- **`process_tap.rs:320`** — `let _ = set_nominal_sample_rate(output_dev, 48_000.0)`. Rewrites the
  user's default output device's sample rate, ignores the status, never restores it.
- **`process_tap.rs:324-325`** — if the post-aggregate `read_tap_format` or `parse_asbd` fails, the
  code `unwrap_or`s back to the pre-aggregate values and proceeds as if nothing changed.
- **`process_tap.rs:326-331`** — a non-48 kHz tap is a hard error whose message says "deferring to
  SCK backend"; the actual deferral happens implicitly in `worker`'s candidate loop
  (`mod.rs:238-251`), which logs it as a generic backend failure.
- **`legacy/shm_reader.rs:188-191`** — `.spawn(...).ok()`: a failed thread spawn produces a `Reader`
  that reports `Transport::Shm` and never reads anything, after the success line was already logged.
- **`legacy/shm_reader.rs:200-202`** — "shared memory unavailable (sandbox?), falling back to HAL
  input" is the only diagnostic; the actual `errno` from `shm_open` is discarded
  (`shm_reader.rs:101-104`).
- **`legacy/shm_reader.rs:330-343`** — if the HAL-input IOProc also fails, `HalInputProc::start`
  returns `None`, `Reader` is constructed anyway, and the tier reports success while producing
  silence forever.
- **`legacy/routing.rs:39-46`** — `RoutingState::save` discards both the `create_dir_all` and the
  `write` error, so a failure means crash recovery silently cannot restore the user's audio device.
- **`legacy/routing.rs:270-272`** — `launchctl bootstrap` result discarded.
- **`legacy/volume_proxy.rs:28-29`** and **`legacy/volume_keys.rs:73, 78`** — every volume/mute write
  is `let _ =`.
- **`sck_audio.rs:238`** — a stop that times out after 5 s is indistinguishable from one that
  succeeded, and the delegate context is freed immediately afterwards.
- **`mod.rs:329-331`** — `dropped_backpressure.store(consumer.overruns())` **overwrites** the
  `fetch_add`s done by the three capture paths, erasing the legacy tier's shm-lap drops. Filed as
  `minor-audio-mac-dropped-backpressure-counter-clobbered.md`.
- **Diagnostics never populated on macOS**: `AudioDiagnostics::period_frames` and
  `discontinuity_count` are written only by the Windows backend
  (`windows_utils/audio/loopback.rs:157, 329`), so the macOS 2-second summary always prints
  `period=0fr, discontinuity=0`.

Silent *fast-path* degradations: `AudioFormatDesc::is_fast_path()` is never consulted
(`format.rs:38`), so the 48 kHz stereo f32 interleaved case — the one the whole pipeline is designed
around — goes through the same byte-wise reassembly and BS.775 downmix as an 8-channel i16 source.
The Windows sibling does branch on it (`windows_utils/audio/format.rs:134`).

---

## 10. TODO / FIXME / HACK / XXX

A grep for `TODO|FIXME|HACK|XXX` across the whole territory (Rust and C++, excluding `test/`) returns
**zero** hits. The only stale marker of any kind is a documentation citation:

- `src-tauri/src/macos_utils/audio/mod.rs:340-345`:
  > `"audio: {} has produced only silence for 5s — if the host IS playing audio, this is the
  > CATapDescription silent-samples failure mode (see AUDIO_NOTES_MACOS.md §4.1)"`

See §12.

---

## 11. Latency path

This territory is on the **audio** glass-to-glass path only; nothing here touches video capture or
input injection. The audio path from a sound being mixed on the host to an Opus packet leaving
`start_capture`'s channel:

### Process Tap (14.2+), the preferred tier

| Stage | Cost | Notes |
|---|---|---|
| HAL mixes and delivers an IOProc block | one device period, typically 512 frames = **10.7 ms** | not controllable from here; the code does not query or set the aggregate device's buffer size (contrast the legacy tier, which sets 128 frames) |
| `tap_ioproc`: convert + silence scan + ring push | ~3 scalar passes over 1024–2048 samples, sub-100 µs expected | **unverified** — needs a Mac; the `unpark` inside `push` is the one unbounded-blocking risk |
| ring residency | 0 to one frame | the worker drains greedily |
| worker wake | `park_timeout(125 µs)` when idle, or an `unpark` | `mod.rs:352` |
| Opus encode, 5 ms frame, complexity 5, `RESTRICTED_LOWDELAY` | measured into `encode_p50_ns`/`encode_p99_ns`; typically 100–300 µs | `mod.rs:292-305` |
| Opus algorithmic lookahead | reported as `lookahead_samples`, forwarded to the client | `mod.rs:236` |
| `Bytes` alloc + unbounded channel send | ~ns | |
| bridge thread hop + broadcast | one extra thread wake | outside the territory |

**≈ one capture block (10–11 ms) plus ~5 ms of framing, plus the encoder lookahead.** The dominant
term is the HAL block size, which this code never tries to reduce.

### ScreenCaptureKit (13.0–14.1)

Same shape, but the delivery chunk is whatever SCK chooses (larger than a HAL period; **unverified**,
needs measurement) and the 1 fps 2×2 dummy video stream runs alongside. Two 5 s blocking waits sit on
the *start* path (`resolve_any_display`, `startCapture`), and one on the *stop* path.

### Legacy virtual device (10.15–12.x)

| Stage | Cost |
|---|---|
| driver `OnProcessMixedOutput` → shm write | one virtual-device period; the host requests **128 frames = 2.67 ms** (`legacy/mod.rs:70-79`), the smallest in the system |
| `se-audio-shm` poll | 0–**500 µs** (`park_timeout`, `shm_reader.rs:236`) — nothing can unpark it, the producer is another process |
| convert + two ring pushes | one scalar pass each |
| encoder ring → worker → Opus | as above |

**≈ 3–4 ms plus 5 ms of framing** — the lowest-latency of the three tiers, and the only one that
controls its own buffer size. Separately, the **local monitor** path (what the host's own speakers
hear) targets `DRIFT_SETPOINT_FRAMES = 4096` of backlog = **85.3 ms**
(`major-audio-mac-playthrough-85ms-monitor-latency.md`).

### Serialisation points

1. `start_capture` blocks the caller (`AudioHub::subscribe`, which holds the hub mutex) on
   `ready_rx.recv()` with **no timeout** until the worker has constructed an Opus encoder and
   successfully started a backend. With SCK that is up to 10 s of blocking waits.
2. The `AudioStopFn` joins the worker, which finishes its drain pass and then calls `backend.stop()`
   — up to 5 s for SCK. Held under the same hub mutex.
3. The worker is single-threaded across *all* sessions: it drains, encodes and sends serially. The
   `AudioHub` fan-out means one worker feeds N devices, so an encode stall delays every device.
4. `probe_audio_backend()` / `capable_backends()` fork `sw_vers` 3–5 times and (below 13.0)
   enumerate the whole HAL device list. `capable_backends()` runs on the **capture start path**
   (`mod.rs:135`), so those forks are directly in front of the first audio packet.
5. `install_watchdog_agent()` writes a plist and forks `launchctl bootstrap` on **every** legacy
   start (`legacy/mod.rs:95`).
6. `Router::activate` writes `legacy_audio_routing.json` synchronously on the start path
   (`legacy/mod.rs:91-93` → `routing.rs:92-97`).

### A/V sync

`capture_ns` is stamped at **drain** time, not capture time, and is synthesised forward in 5 ms steps
from the worker's wake-up (`mod.rs:284-315`). The hardware timestamps both callbacks receive —
`in_input_time` (`process_tap.rs:75`) and the `CMSampleBuffer` PTS — are discarded. The video path
uses the true capture instant (`webrtc_session.rs:512`). This is a systematic offset of roughly one
capture block plus scheduler jitter, and the sequence can regress under jitter. Filed as
`major-audio-mac-capture-ns-is-drain-time.md` — the single highest-value latency finding in this
territory, because it is a correctness bug in the one number the client uses to line audio up with
video.

---

## 12. Stale references

- **`mod.rs:343` — `"(see AUDIO_NOTES_MACOS.md §4.1)"`.** That file does not exist on any branch;
  `CLAUDE.md` says so explicitly. It is printed to the user-facing Settings log terminal. Filed as
  `minor-audio-mac-stale-doc-reference-audio-notes-macos.md`.
- **`CLAUDE.md` vs the tree**: the driver project is at `src-tauri/macos/ScreenExtendAudio/`, not
  `macos/ScreenExtendAudio/`; the stated current version 0.5.4 is now 0.6.3 in all four files.
- **`legacy/probe.rs` doc comment on `default_state_path` (`routing.rs:188`)** correctly names
  `~/Library/Application Support/app.screenextend.desktop/legacy_audio_routing.json` — accurate.
- **`ring.rs:16-24`** — "producer half, held by the RT capture callback" / "consumer half, held by
  the encoder thread". Accurate, and worth keeping: it is the clearest statement of the RT contract
  that `unpark` violates.
- **`sck_audio.rs:257`** — `// SAFETY: the stream + delegate are released, so no callback can still
  read ctx`. This comment asserts an invariant the code does not establish; see
  `major-audio-mac-sck-stop-frees-ctx-with-handler-possibly-in-flight.md`.
- **`device.cpp:37`** — `// fast path: full-scale, pre-gain, RT-safe` sits above an `if/else` whose
  two arms are identical; the comment describes a split that was never implemented.
- **`mod.rs:25`** — `// 7680 → rounds to 8192` is arithmetically correct but does not say that 8192
  f32 is 85 ms, which is the number that matters.
- No references to AI tooling anywhere in the territory.

---

## 13. Dead code candidates

Filed as `minor-audio-mac-dead-code-inventory.md`, with the `grep` evidence for each:

1. `ProcessTapCapture::try_create` (`process_tap.rs:248-265`) — no callers. Would build and start an
   entire tap + aggregate device just to drop it.
2. `SckAudioCapture::try_create` (`sck_audio.rs:74-89`) — no callers. Would block up to 10 s.
3. `ProcessTapCapture::nonsilent_probe` (`process_tap.rs:232, 243, 343`) — assigned a fresh
   `Arc<AtomicU64>` that is never connected to the `IoCtx`; write-only.
4. `Playthrough::gain()` (`playthrough.rs:265-267`) — no callers.
5. `IoCtx::underruns` (`playthrough.rs:63, 118`) — incremented, never read. It is precisely the
   number needed to safely lower the 85 ms playthrough setpoint.
6. `ControlHandle::transport` (`legacy/mod.rs:32, 171`) — stored, never read.
7. `AudioFormatDesc::is_fast_path` (`format.rs:38-43`) — tests only.
8. `AudioDiagnostics::period_frames` and `discontinuity_count` — never written on macOS.
9. `shm_ring.hpp` `overruns` (offset 48) — never written by the C++ producer, never read by the Rust
   reader. `sampleFormat` (offset 16) — written, never validated.
10. `device.cpp:38-42` — the `else` arm of the channel-count branch is byte-identical to the `if`.

---

## 14. Capability probing

**Where the tier doctrine is followed:**
- `process_tap::runtime_available()` (`process_tap.rs:58-60`) — `dlsym` for the two 14.2 functions
  plus `AnyClass::get(c"CATapDescription")`. Textbook.
- `SckAudioCapture::runtime_available()` (`sck_audio.rs:67-72`) — four class lookups.
- `legacy::probe::driver_healthy()` — asks the HAL whether the device exists, rather than trusting
  the bundle path or a version.
- No `objc2-screen-capture-kit` dependency; the SCK delegate is built at runtime; the 14.2 CoreAudio
  constants are compile-time literals rather than extern statics. The dyld rule holds.

**Where it is violated** (filed as
`major-audio-mac-tiering-gated-by-version-not-capability.md`):
- `mod.rs:103` — `macos_at_least(14, 2) && process_tap::runtime_available()`. The version half adds a
  `sw_vers` fork and a fail-open branch (`streamer/mod.rs:75`: `None => true`) and nothing else.
- `mod.rs:106-108` — `macos_at_least(13, 0) && screencapturekit_available() && ...runtime_available()`.
- `streamer/mod.rs:80-82` — `screencapturekit_available()` is **only** `macos_at_least(12, 3)`.
  Despite the name it performs no capability probe at all, and it is shared with the video path.
- `legacy/probe.rs:17-20` — `eligible_os()` is `macos_at_least(10, 15) && !macos_at_least(13, 0)`, a
  pure version window with no capability component.

**Where a tier is missing:**
- **No fallback below the native tiers on 13.0+.** If `capable_native_backends()` yields SCK and SCK
  then fails to start (Screen Recording denied, no displays, `startCapture` times out),
  `worker`'s candidate loop exhausts and reports "all capable audio backends failed to start"
  (`mod.rs:252-255`). Because `eligible_os()` excludes 13.0+, the legacy virtual device is never
  offered even when its driver is installed and healthy. A capability-based `eligible_os` would fix
  this for free.
- **No fallback below the legacy tier.** On 10.15–12.x without the driver, the answer is
  `NeedsDriverInstall`, and the install cannot succeed because the pkg is not bundled — see
  `major-audio-mac-legacy-driver-pkg-is-never-bundled.md`. So the effective audio support matrix today is:
  14.2+ works, 13.0–14.1 works (with a screen-recording indicator), **10.15–12.x has nothing**.
- **No probe for the aggregate device's buffer size.** The legacy tier negotiates 128 frames
  (`legacy/mod.rs:70-79`); the Process Tap tier accepts whatever the HAL gives it. That is 8 ms of
  avoidable latency on the *preferred* tier, and the code to negotiate it already exists in
  `hal.rs:257-267`.

---

## 15. Questions and cross-cutting concerns

1. **`capture_ns` semantics are wrong on both platforms.** Windows
   (`windows_utils/audio/loopback.rs:245`) uses `host_now_ns()` at encode time exactly as macOS does.
   Whoever owns the client-side A/V sync territory should confirm what `audio.js` /
   `audio-worklet.js` do with the value (`audio-worker.js:137` feeds it into
   `EncodedAudioChunk.timestamp`) and whether a non-monotonic sequence is tolerated. The fix is
   cheap and identical on both platforms; it should be one change, not two.
2. **`macos_at_least()` forking `sw_vers` on every call is not an audio problem.** It lives in
   `macos_utils/streamer/mod.rs:61-77` and is used by the display tier, the compatibility report and
   the video SCK probe as well. Whoever owns `macos_utils/streamer/` should take the fix; I filed it
   under audio because that is where I measured the blast radius (3–5 forks per probe, on the capture
   start path).
3. **`screencapturekit_available()` is a version check wearing a capability check's name**, and it is
   shared with the video path. Fixing it changes video behaviour too — coordinate.
4. **`AudioHub::subscribe`/`unsubscribe` hold a `std::sync::Mutex` across `start_capture()` and the
   `AudioStopFn`**, which on macOS can block for 5–10 s. That is `streamer/audio/mod.rs`, not this
   territory, but the blocking durations come from here.
5. **Is the `elevated_command` crate's argv handling what `run_privileged` assumes?**
   `installer.rs:29-33` builds `/bin/sh -c '<single-quoted command>'`. That is only correct if the
   crate re-joins program and args into one string for `osascript ... do shell script`. If it passes
   argv through, every privileged command fails. Whoever owns `cli.rs`/packaging and has the
   `Cargo.lock` version to hand should confirm.
6. **Nobody can verify the dyld audit from Windows.** `nm -u target/release/ScreenExtend` on a macOS
   build is the only definitive check that no 12.3+/13.0+/14.2+ symbol is bound at link time. My
   audit (§7) is by API vintage and by inspecting how each constant is consumed; it found no
   violation, but it is not proof.
7. **The legacy tier is 1500 lines of Rust plus a C++ driver that has never been signed, installed
   or loaded, wired to an installer that cannot find its package.** Before anyone invests in it,
   someone should decide whether 10.15–12.x system audio is in scope at all. If it is, the pkg needs
   to be built in CI. If it is not, deleting the tier removes the LaunchAgent, the CGEventTap, the
   default-device hijack, the shared-memory ABI and about a third of this territory's risk surface.
8. **Two silence definitions disagree.** The capture callbacks use `abs() > 1/32768`
   (`process_tap.rs:30`, `sck_audio.rs:28`, `shm_reader.rs:68`); the worker uses exact `== 0.0`
   (`mod.rs:290`) to set `FLAG_SILENT`. A −96 dBFS dither floor would therefore be reported as
   non-silent by the backend and silent by the flag, or vice versa. Whoever owns the client's
   silence handling should say which definition the wire protocol intends.

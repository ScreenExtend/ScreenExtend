# N — Tests

Territory: every test module in the Rust crate, plus `src-tauri/examples/audio_spike.rs`.
Read in full: all 19 files under a `test/` directory, all 33 inline `#[cfg(test)]` blocks found by
`grep -rn "cfg(test)" src-tauri/src`, and the spike.

Test run on this Windows box, 2026-09-08 (shared target dir
`C:\Users\sarve\.cargo\shared_target\ScreenExtend`, first `cargo test --no-run` build ≈ 4 min):

```
$ cd src-tauri && cargo test
   Running unittests src\lib.rs
running 138 tests
test result: ok. 137 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.16s
   Running unittests src\main.rs
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
   Doc-tests screenextend_lib
running 14 tests
test result: FAILED. 0 passed; 9 failed; 5 ignored; 0 measured; 0 filtered out; finished in 0.17s
error: doctest failed, to rerun pass `--doc`
$ echo $?
101
```

Incremental wall time for the whole `cargo test` invocation: **4 s**. `cargo test --lib` alone is
green; bare `cargo test` is red and has been since 2026-06-30.

---

## 1. Files and purpose

### `src-tauri/src/streamer/test/`

**`mod.rs`** (4 lines) — declares `pub mod whep_selftest;` **unconditionally** (line 1) and
`#[cfg(test)] mod bitrate;` (lines 3-4). The asymmetry is load-bearing: the self-test compiles into
release builds, the bitrate tests do not.

**`whep_selftest.rs`** (363 lines) — an in-process WHEP client that stands up the real axum server
and negotiates against it. Has **no `#[test]` attribute**; it is `pub async fn run(config: Config)`.
See §3 below for what it exercises and §13 for why it is unreachable.

**`bitrate.rs`** (170 lines, 14 tests) — the most complete test module in the crate. Pure logic
over `BitrateController` and `estimate_from_loss`: clamping to min/max, hysteresis suppression,
rate limiting, EWMA damping of a spike, convergence on steady input, the asymmetric cut threshold,
loss back-off and upward probing, and an end-to-end degrade-then-recover loop over 40 simulated
ticks. No hardware, no I/O, deterministic (`Instant::now()` is taken once and offsets are added).

### `src-tauri/src/windows_utils/streamer/test/`

**`mod.rs`** (12 lines) — declares the five submodules and holds one test,
`dxgi_duplication_probe`, `#[ignore = "requires an interactive desktop; run with --ignored"]`. It
calls `super::dxgi::probe_to_bmp(0, "…/target/dxgi_probe.bmp")` and asserts only `Ok`. It is the
sole `ignored` in the Windows lib run.

**`nvenc_layout.rs`** (6167 lines, 55 tests) — verbatim bindgen output. Characterised in §7.

**`intel_layout.rs`** (102 lines, 2 tests) — hand-written oneVPL ABI assertions. 13 `size_of`
checks and 14 `offset_of!` checks. No `align_of` anywhere. The comments carry the real intent:
`offset_of!(mfxInfoMFX, InitialDelayInKB) == 122, "mfxInfoMFX.InitialDelayInKB (==QPI)"` documents
a C union alias a size check would miss.

**`intel_encoder.rs`** (211 lines, 3 tests) — real Quick Sync sessions. Encodes 10 frames at
1920x1080@357 fps to prove the frame-rate clamp; 30+10 frames at 1280x720 asserting the first
access unit is an IDR with in-band SPS/PPS and that `set_bitrate` works in place; and a fused
VPP downscale 1920x1080→1280x720 from a native `ID3D11Texture2D`. Honours `INTEL_PROBE_OUT` to
dump the bitstream. All three skip on missing hardware via `teprintln!` + `return`.

**`x264_encoder.rs`** (201 lines, 2 tests) — the software fallback. Encodes 120 frames of 1080p,
walks the Annex-B NAL units, asserts frame-in/packet-out (zerolatency must emit every frame), that
frame 0 and a mid-stream forced keyframe each carry SPS+PPS+IDR, that no NAL payload exceeds a
VBV-implied ceiling (`2 * bitrate/fps / 8 * 4`), and that the achieved bitrate lands in 0.6-1.4x
of target. Measures p95 per-frame encode time against the frame budget. Second test proves
`set_bitrate(LOW)` shrinks the output.

**`pipeline.rs`** (64 lines, 7 tests) — pure arithmetic on `scaled_dims` and `ScalePercent`:
100 % passthrough with odd-dimension rounding, 90 %/75 %/50 % scaling, even-rounding, clamping of
out-of-range percentages, string parsing, a never-zero floor at minimum scale, and
`live_encoder_config`'s fps clamping against `max_fps`.

### `src-tauri/src/windows_utils/audio/test/`

**`mod.rs`** (2 lines) — declares `format` and `opus_layout`.

**`format.rs`** (137 lines, 8 tests) — `convert_to_stereo_f32`. Stereo-f32 passthrough plus
`is_fast_path()`, int16 scaling by 32768 not 32767, mono duplication, ITU BS.775 5.1 downmix with
the exact -3 dB coefficient, clamping instead of wrapping, out-of-range float clamping, quad
downmix, and a partial-trailing-frame test that asserts no out-of-bounds read.

**`opus_layout.rs`** (52 lines, 4 tests) — libopus constants (`OPUS_APPLICATION_*`,
`OPUS_SIGNAL_*`, 16 CTL request numbers, 8 error codes) and the frame math
(`OPUS_SAMPLE_RATE == 48000`, `FRAME_MS == 5`, `FRAME_SAMPLES == 240`). All hardcoded literals
against Rust constants; nothing links or loads libopus.

### `src-tauri/src/macos_utils/audio/test/`

**`mod.rs`** (53 lines, 2 tests) — tier probing. Both open `if macos_at_least(13, 0) { return; }`.
See §14.

**`format.rs`** (172 lines, 9 tests) — the macOS format layer. Parses `AudioStreamBasicDescription`
including the **planar** float layout Windows never produces, checks `is_fast_path()` is false for
planar and true for packed interleaved, interleaved passthrough, mono→stereo, i16 scaling, BS.775
5.1 downmix, clamping, planar→interleaved for the SCK layout, and rejection of an unknown format.

### `src-tauri/src/macos_utils/audio/legacy/test/`

**`mod.rs`** (67 lines) — declares `live`, `ring`, `routing` and holds `mod gain` (5 tests):
`compute_gain` mute/clamp behaviour, `ramp` monotonicity and no-overshoot over 2000 steps, a
click-free single step (`0 < ramp(0.0, 1.0) < 0.1`), `drift_trim` sign under over-full/drained
rings, and `drift_trim` boundedness over 1,000,000 iterations.

**`ring.rs`** (69 lines, 6 tests) — `clamp_read` semantics: no lap when the reader keeps up,
exactly-full is not a lap, lap skips to one ring behind head, a 1000-step wraparound consistency
loop against an 8-slot ring, generation-change resync, and `layout_matches_cpp_abi_contract`.

**`routing.rs`** (167 lines, 8 tests) — the default-output takeover, using a `FakePort`
implementing `DefaultDevicePort`. Activate saves the previous device and persists a flag; restore
puts it back and clears the flag; activate never saves *ourselves* (the silent-device trap); a
user switching output is respected, not fought; a change back to us is ignored; and three crash-
recovery cases. Uses per-process temp files with an `AtomicU64` counter so the tests are
parallel-safe.

**`live.rs`** (60 lines, 1 test) — `legacy_live_capture_roundtrip`,
`#[ignore = "requires the ScreenExtend Audio driver loaded; takes over the default output device"]`.
Starts the real virtual-device source, shells out to `say`, sleeps 8 s, pops the ring, computes
RMS, asserts `nonsilent > 0`. Prints with plain `eprintln!`, so it is visible under `--nocapture`.

### `src-tauri/src/streamer/audio/tests.rs`

(168 lines, 7 tests) — a hand-built `AudioCapture` over a `crossbeam-channel` and a spawned
producer thread (2 tests), plus 5 `seqgate_*` tests over a `SeqGate` struct **defined in the test
file**. See §13.

### `src-tauri/examples/audio_spike.rs`

(361 lines) — a Windows-only WASAPI measurement spike. See §5 below and
`findings/minor-docs-audio-spike-references-a-prd-that-does-not-exist.md`.

### Inline `#[cfg(test)]` blocks (33 of them)

`streamer/audio/protocol.rs` (5), `streamer/input/protocol.rs` (8), `streamer/server.rs` (10),
`streamer/session.rs` (5); `windows_utils/driver_ipc/{client,sync/client}.rs` (3+4) plus
`driver_ipc/mock.rs` (a `#[cfg(test)]` `MockServer` over a real Win32 named pipe);
`macos_utils/audio/ring.rs` (3), `macos_utils/audio/legacy/routing.rs` (a `#[cfg(test)]`
`set_default_for_test` helper), `macos_utils/audio/legacy/shm_reader.rs` (a `#[cfg(test)]`
`layout` re-export module); `macos_utils/streamer/{cgds,encoder,frame,pipeline,sck}.rs`,
`macos_utils/virtual_display.rs`; and ten `macos_utils/airplay/*.rs` files carrying 76 tests
between them.

---

## 2. Test census

| scope | count | notes |
|---|---|---|
| compiled + run on Windows | **138** | 137 pass, 1 ignored, 1.16 s |
| — cross-platform (`streamer/`) | 49 | protocol 5+8, server 10, session 5, bitrate 14, audio/tests 7 |
| — Windows-only | 89 | nvenc_layout 55, driver_ipc 7, audio 12, pipeline 7, intel 5, x264 2, dxgi 1 |
| macOS-only, never run here | **123** | airplay 76, audio 35, streamer 11, virtual_display 1 |
| **total distinct `#[test]` in the crate** | **261** | |
| `#[ignore]`d | 14 | 1 Windows, 13 macOS |
| doctests | 14 | 5 ignored, **9 failing to compile** |
| not a `#[test]` at all | 1 | `whep_selftest::run` |

Of the 123 macOS-only tests, 76 (62 %) belong to the AirPlay fallback tier, which `CLAUDE.md`
describes as "below the supported floor now and kept only as a safety net."

---

## 3. `whep_selftest.rs` — what it does and whether it can run

**What it exercises, end to end:**
1. Spawns `crate::streamer::server::run(config, None)` as a tokio task on ports 18080/18443
   (`:22-44`), i.e. the real HTTP/HTTPS server, the real display-creation path, the real pipeline.
2. Polls `GET /health` over TLS until 200, up to 100 attempts at 100 ms (`:248-260`).
3. Builds a `webrtc` API with an H.264 codec at PT 102
   (`level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f`), an RTX codec at
   PT 103 (`apt=102`), the five `RTCPFeedback` entries (goog-remb, ccm fir, nack, nack pli,
   transport-cc), default interceptors, and a `SettingEngine` restricted to Udp4/Udp6 (`:49-119`).
4. Adds a recvonly video transceiver, registers `on_track` and a state-change handler (`:123-169`).
5. Creates the offer, waits for full ICE gathering, POSTs the SDP to `/whep` over TLS with a
   certificate verifier that accepts anything (`mod danger`, `:306-362`).
6. **Asserts 8 SDP attributes on the answer** (`:185-216`): `H264`, `packetization-mode=1`,
   `profile-level-id=42e01f`, `a=rtcp-fb`+`nack`, `nack pli`, `ccm fir`, `transport-cc`, `rtx`.
   On failure it dumps the whole answer SDP and bails.
7. Applies the answer and requires **`on_track` to fire** and **≥30 RTP packets within 10 s**
   (`MIN_RTP_PACKETS = 30`, `TIMEOUT_SECS = 10`, `:19-20`, `:222-242`).

**Can it run on both OSes?** The code is OS-independent — it only touches `streamer::server`,
`webrtc`, `tokio`, `tokio_rustls`. So yes in principle, and its dependency on a real virtual
display is the only platform constraint.

**Can it run in CI?** Not as written. It binds two ports and needs `server::run` to create a
virtual display, which needs a driver on Windows and TCC/private CG API on macOS. But **step 6 —
the SDP attribute checks — needs neither**: the answer is produced by
`webrtc_session.rs`'s negotiation before any media flows. Splitting the negotiation half out is
the single highest-value test change available and would run on both runners.

**It would fail today even if it could run.** `:262-268` POSTs the raw SDP with
`Content-Type: application/sdp`, but `server.rs:620-627` `process_whep` does
`serde_json::from_slice::<JoinRequest>(body)` and returns `400 invalid join request` on anything
else. `JoinRequest` carries `session_id`, `otp`, `os`, `sdp`. The join protocol grew OTP auth and
the self-test never followed — direct evidence that nothing has executed this file in a long
time. (Independently found by another territory:
`findings/minor-cli-whep-selftest-posts-sdp-but-server-requires-json.md`.)

**Can anyone run it today?** No. It has no `#[test]`, and its only caller chain
(`Streamer::whep_selftest` ← `streamer::cli::run`) is dead:
`grep -rn "streamer::cli::run\|cli::run()" src-tauri/src` returns nothing. `CLAUDE.md` confirms the
probe harness "is **not** wired into `main.rs`". Filed as
`findings/major-webrtc-whep-selftest-is-unreachable.md`.

---

## 4. Gating: how each hardware/OS-dependent test skips

Full inventory in `findings/minor-build-inventory-of-silently-passing-tests.md`. Summary:

**Honest (`#[ignore]`, harness prints `ignored`) — 14 tests:**
`dxgi_duplication_probe`; `cgds.rs::{probe_virtual_display, probe_main_display}`;
`pipeline.rs::{end_to_end_latency, production_pipeline_smoke}`;
`macos encoder.rs::{encode_synthetic, recreate_churn_releases_hw, creates_small_via_software,
hw_capability_probe}`; `dnssd.rs::publishes_and_withdraws`;
`sender_prefs.rs::enable_then_restore_round_trips`; `ax.rs::can_read_the_live_displays_menu`;
`airplay/mod.rs::live_end_to_end`; `legacy/test/live.rs::legacy_live_capture_roundtrip`.

Eight of the 14 carry a **bare `#[ignore]` with no reason string** — `cgds.rs:280,361`,
`encoder.rs:718,811,841,856`, `pipeline.rs:563,675` — so the harness prints
`ignored` with no hint of what is needed to run them. The AirPlay and legacy-audio ones all carry
a reason. Adding reasons to the eight is a one-line-each fix and is the difference between a
documented manual harness and a forgotten one.

**Silent — 6 tests skip via `teprintln!` + `return`, and `teprintln!` is swallowed**
(`logbus::VERBOSE` is `false` and no test sets it — `logbus.rs:12,89-95`):
`x264_encoder.rs:59-62` and `:164-167`; `intel_encoder.rs:46-49`, `:84-87`, `:143-146`,
`:165-168`.

**Silent — 3 tests skip via a bare `return` with no message:**
`server.rs:1499-1501` (`display_scale_steps().is_empty()`);
`macos_utils/audio/test/mod.rs:9-11` and `:31-33` (`macos_at_least(13, 0)`).

**Silent — 1 test's assertions live inside an `if let` that is normally `None`:**
`topology.rs:549-556` `no_airplay_display_is_found_when_none_is_attached` — the test name promises
a check the body never performs when no AirPlay display is attached.

**Measured proof that this is not theoretical.** On this box the x264 pair skips while printing
`ok`:

| run | wall of `cargo test --lib x264_encoder` |
|---|---|
| default | **0.44 s**, 2 passed |
| with `debug/resources` on `PATH` | **4.25 s**, 2 passed |

Cause: `x264_sys.rs:412-422` looks in `current_exe().parent()/resources/`, which for the test
binary is `…/debug/deps/resources/` — a directory that does not exist; the DLL is at
`…/debug/resources/libx264-164.dll`, and neither it nor `libx264.dll`/`x264.dll` is on `PATH`.
Filed as `findings/blocker-x264-tests-pass-without-encoding-anything.md`.

Contrast: the Intel trio *does* run here — verified by setting `INTEL_PROBE_OUT` and observing a
726,835-byte H.264 file. Same silent-skip shape, different luck.

---

## 5. The FFI layout-test method (source of truth)

Full write-up in `findings/speculative-build-ffi-layout-test-method.md`.

**NVENC — bindgen output, frozen, no provenance.** Every one of the 55 tests is:

```rust
const UNINIT: MaybeUninit<T> = MaybeUninit::uninit();
let ptr = UNINIT.as_ptr();
assert_eq!(size_of::<T>(),  <literal>);
assert_eq!(align_of::<T>(), <literal>);
assert_eq!(addr_of!((*ptr).field) as usize - ptr as usize, <literal>);
```

The literals were computed **offline by bindgen** from `nvEncodeAPI.h` at generation time. There is
no `bindgen` dependency in `Cargo.toml` and no bindgen call in `build.rs`;
`nvenc_sys/nvEncodeAPI.rs` (177,671 bytes) is checked in, sourced from
`https://github.com/ViliamVadocz/nvidia-video-codec-sdk` per `nvenc_sys/mod.rs`. The file has **no
`cfg` gating at all**, so the offsets silently assume a 64-bit target (function pointers at 8-byte
stride throughout `_NV_ENCODE_API_FUNCTION_LIST`, size 2552, 44 offsets).

What it proves: our Rust structs still match the numbers bindgen produced. What it does **not**
prove: anything about the header the loaded driver was built against. No SDK version is recorded
anywhere in the file.

**Intel oneVPL — hand-transcribed, selective, hand-derived.** `intel_sys.rs` is written by hand
with section comments naming the C headers (`mfxdefs.h` etc.), 17 `pub struct`s, 18 `repr(C)`.
`intel_layout.rs` covers 13 of 17 with `size_of`, spot-checks 14 offsets, and asserts **no
alignment at all**.

**Third style:** `legacy/test/ring.rs::layout_matches_cpp_abi_contract` asserts Rust constants
against literals with the comment "MUST match `shm_ring.hpp`'s static_asserts" — but reads only the
Rust side (`shm_reader.rs:370-378` is a `#[cfg(test)]` re-export of our own constants). Filed as
`findings/minor-audio-mac-shm-abi-asserted-on-one-side-only.md`.

**Implications for a new `amd_layout.rs` and for VideoToolbox/CoreAudio/CoreWLAN:** generate rather
than transcribe; record header version + bindgen command as a provenance comment; add an
`#[ignore]`d regeneration/diff test so the numbers can be re-derived; always assert `align_of`;
gate pointer-bearing asserts on `target_pointer_width`. And for macOS specifically: layout tests
are the wrong tool for the main hazard there. Per `CLAUDE.md` the killer is link-time symbol
exposure, and the only defence named — `nm -u target/release/ScreenExtend` — exists nowhere in the
test suite. It should.

---

## 6. `audio_spike.rs` — what it measured and what it embeds

Windows-only (`#[cfg(not(windows))] fn main()` just prints "audio_spike is Windows-only"). It runs
a COM MTA and answers three numbered questions, printing a VERDICT for each:

- **Q3** (`:58-78`) — `IAudioClient3::GetSharedModeEnginePeriod`: default / fundamental / minimum /
  maximum engine period in frames and ms; `minimum` is labelled "capture-latency floor".
- **Q1** (`:85-112`) — `IAudioClient3::InitializeSharedAudioStream(LOOPBACK|EVENTCALLBACK, min)`.
  On failure prints *"loopback NOT supported on the IAudioClient3 low-latency path; use legacy
  Initialize."* This conclusion is now shipped behaviour (`windows_utils/audio/loopback.rs`).
- **Q2a** (`:138-139`) — legacy `IAudioClient::Initialize(SHARED, LOOPBACK|EVENTCALLBACK)`, idle
  host, **no** silent render companion.
- **Q2b** (`:147-159`) — the identical probe **with** a silent render companion
  (`start_silence_companion`, `:275-315`).

`run_capture_probe` (`:162-262`) is the instrument: 20 × `WaitForSingleObject(200 ms)`, counting
event signals vs timeouts, packets, total frames (converted to ms of audio),
`AUDCLNT_BUFFERFLAGS_SILENT`, `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`, and wake-to-wake interval
avg/max. Verdicts (`:255-261`): *"event NEVER fired -> need a companion/clock source"* /
*"fired unreliably"* / *"fired reliably."*

`describe_format` (`:317-360`) classifies 48 kHz as "the 48k FAST PATH (no resample)" vs "NOT 48k
(needs rate conversion)" — the distinction `windows_utils/audio/format.rs::is_fast_path` now
encodes.

**Numbers embedded in comments: none.** Every value is computed and printed at runtime. The only
surviving result anywhere is `CLAUDE.md`'s parenthetical about `silence.rs`: *"(measured: 0/20
event signals without it, 20/20 with it)"* — a Q2a/Q2b result with no date, machine, or device.
The banner at `:44` cites `PRD §3`, a document `CLAUDE.md` confirms is not in the tree on any
branch. Filed.

---

## 7. Coverage gaps against `CLAUDE.md`'s component list

Zero tests (no `cfg(test)` block, no sibling `test/` entry):

| `CLAUDE.md` component | files with no test |
|---|---|
| Windows capture (WGC) | `windows_utils/streamer/capture.rs` |
| Windows capture (DXGI + cursor) | `windows_utils/streamer/dxgi/{mod,cursor}.rs` — only the `#[ignore]`d `probe_to_bmp` |
| **NVENC encoder** | `nvidia/encoder.rs` (39 KB) — 6167 lines of *layout* tests, zero functional |
| AMD encoder | `windows_utils/streamer/amd.rs` (stub), no `amd_layout.rs` |
| GPU scaling / tuning | `scaler.rs`, `tuning.rs` |
| Windows virtual display | `windows_utils/virtual_display.rs` |
| WASAPI loopback + silent companion + device notify | `audio/{loopback,silence,device,guards}.rs` |
| **WebRTC/WHEP negotiation, `host_ns_to_rtp90k`, BWE driver** | `streamer/webrtc_session.rs` |
| Cloud relay | `streamer/cloud.rs` |
| TLS cert generation | `streamer/tls.rs` |
| Input injection backends | `streamer/input/{windows,macos,linux,generic,scancode,mod}.rs` — only `protocol.rs` is tested |
| Shipped CLI | `src/cli.rs` |
| Single instance | `src/single_instance.rs` |
| Log bus | `src/logbus.rs` |
| Device reporter | `{windows,macos}_utils/device_reporter.rs` |
| Compatibility report | `{windows,macos}_utils/compatibility.rs` (reached only by a macOS test that returns early on 13+) |
| Hosted network | `{windows,macos}_utils/hosted_network.rs` |
| macOS SCK / CGDS capture | `macos_utils/streamer/mod.rs` (the fallback decision itself), `display.rs`, `gpu.rs`, `mach.rs`, `qos.rs`, `tuning.rs`, `power.rs`, `activity.rs` |
| **macOS Process Tap (14.2+) and SCK audio (13.0+)** | `audio/process_tap.rs`, `audio/sck_audio.rs` |
| macOS legacy installer / HAL / volume keys | `legacy/{installer,hal,probe,volume_keys}.rs` |
| Opus encoder wrapper | `streamer/audio/encoder.rs` (only constants asserted in `opus_layout.rs`; libopus is never loaded by a test) |
| **The entire client web page** | `streamer/static/*.js` — no test runner in `package.json` at all |
| The React desktop UI | `src/` — same |
| Linux scaffold | all of `linux_utils/` |

Partial coverage worth naming: `session.rs`'s 5 tests are all `OtpLimiter`/`SessionAuth` — the
reconfig/kick **epoch bumps**, disconnect grace, `DeviceSessionState`, and `mint_device_token` have
none. `server.rs`'s 10 tests cover display-slot capacity, name sanitisation and the DPR ladder —
not the join path, `/whep`, `/reconfig`, or the clamp constants.

Filed: `major-build-capture-subsystems-have-no-automated-test.md`,
`major-client-web-page-has-no-test-infrastructure-at-all.md`,
`major-nvenc-layout-tests-without-functional-tests.md`,
`minor-webrtc-session-cloud-tls-have-no-tests.md`,
`minor-audio-mac-native-tiers-untested-while-the-dead-tier-is-well-tested.md`.

---

## 8. Threads spawned by tests

- `streamer/audio/tests.rs:16` — one `std::thread` producer per `mock_capture(n)`, joined by the
  `stop` closure. Two tests, short-lived.
- `whep_selftest.rs:40` — `tokio::spawn` running the whole axum server; `:148` — a per-track
  `tokio::spawn` reading RTP into a 1600-byte buffer. Both killed by `server_handle.abort()`.
- `driver_ipc/mock.rs:38` — `task::spawn` per `MockServer` serving a real Win32 named pipe. Seven
  tests, each with a unique pipe name (`virtualdisplaydriver-test-<testname>`), so parallel-safe.
- `macos_utils/streamer/pipeline.rs` `end_to_end_latency` — a `std::thread` draining the encoder
  output while the main thread submits; calls `qos::pin_current_thread_user_interactive()`.
- `airplay/mirror.rs` — three `#[tokio::test]`s each `tokio::spawn` a `MirrorSink::run` accept
  loop against a real loopback `TcpStream`.

No test sets a real-time priority except the macOS latency harness, and that one is `#[ignore]`d.

## 9. Locks in test code

All `std::sync::Mutex`, none held across an `.await`:
- `driver_ipc/sync/client.rs` tests — `Arc<Mutex<i32>>` / `Arc<Mutex<Vec<_>>>` written from the
  callback thread, read after a 100 ms `sleep`. The sleep is the synchronisation, which is a
  timing assumption, not a guarantee.
- `macos_utils/streamer/pipeline.rs` `end_to_end_latency` — `Arc<Mutex<VecDeque<(u64, bool)>>>`
  shared between the submit loop and the drain thread; locked and immediately dropped on both
  sides.
- `airplay/receiver.rs` tests read `i.state.lock().unwrap().phase` — the `if let` scrutinee
  temporary-lifetime hazard `CLAUDE.md` warns about does not appear in any test I read; every such
  guard is on a `let`-bound line or an `assert_eq!` argument with no `.await` in scope.
- `logbus.rs:43` — `bus().backlog.lock()` is taken on **every** `tprintln!`/`teprintln!`, including
  from test code, and the backlog grows to 2000 entries per test process.

## 10. Panics a runtime condition can hit

Test code, so panics are the point — but a few are `expect`s on environment rather than on the
thing under test, which turns a missing prerequisite into a confusing failure rather than a skip:
- `intel_encoder.rs:92` — `std::fs::File::create(p).expect("create INTEL_PROBE_OUT")` panics if
  the env var names an unwritable path.
- `macos_utils/streamer/encoder.rs` `encode_synthetic` — `std::fs::write("/tmp/screenextend_test.h264", …).expect("write bitstream")`, a hardcoded absolute path.
- `mirror.rs::timing_socket_binds_a_real_port` — `TimingSocket::bind().expect("bind")` panics if
  the port range is occupied.
- `dnssd.rs::a_real_interface_index_is_found` — asserts `primary_interface_index().is_some()`;
  fails on a machine with no usable interface (a CI container).
- `topology.rs::baseline_of_the_live_system_is_not_empty` — asserts a Mac has an active display;
  fails headless.
- `x264_encoder.rs:85` — `.unwrap_or_else(|e| panic!("encode frame {i} failed: {e:?}"))`, correct.

## 11. Silent fallbacks and swallowed errors in test code

- The six `teprintln!` skips (§4) — the single biggest one.
- `x264_encoder.rs`, `intel_encoder.rs` success lines use `tprintln!`, so **the measured p95 encode
  time and achieved bitrate are computed and then discarded**. Filed as
  `major-build-tprintln-hides-every-test-measurement.md`.
- `routing.rs` — every test ends with `let _ = std::fs::remove_file(&path);`; a failure to clean up
  leaves temp files behind silently. Harmless.
- `cgds.rs::probe_main_display` — on `backend.start()` failure it prints a long TCC explanation via
  `eprintln!` and `return`s, passing. It is `#[ignore]`d, so this is opt-in, and the message is
  real stderr, so it is visible.
- `whep_selftest.rs:42` — `teprintln!("[selftest] server exited with error: {e:?}")` inside the
  spawned server task; if the server dies, the client just times out on `/health` and reports
  "server did not become healthy in time" rather than the real cause.
- `whep_selftest.rs:234` — `pc.close().await.ok()`.

## 12. TODO / FIXME / HACK / XXX

None in any test file in the territory. `grep -rn "TODO\|FIXME\|HACK\|XXX"` across all 19 `test/`
files, the inline blocks I read, and `audio_spike.rs` returns nothing.

## 13. Latency path

Tests are not on the runtime latency path. What matters here is which tests *measure* it:

| harness | what it measures | status |
|---|---|---|
| `macos_utils/streamer/pipeline.rs::end_to_end_latency` | capture→encoded mean/p50/p99/max in ms over 6 s, plus keyframe and byte counts | `#[ignore]`, macOS only, prints with real `println!` |
| `macos_utils/streamer/pipeline.rs::production_pipeline_smoke` | submit→broadcast mean/p50/max | `#[ignore]`, macOS only |
| `macos_utils/streamer/cgds.rs::probe_main_display` | frame-arrival age mean/p99/max via `mach_age_ms` | `#[ignore]`, macOS only |
| `macos_utils/streamer/encoder.rs::hw_capability_probe` | which resolutions VideoToolbox will take in HW, and the concurrent-session cap | `#[ignore]`, macOS only |
| `x264_encoder.rs` | p95 per-frame encode µs vs the frame budget | runs, but **the number is swallowed**, and on this box it is not even computed |
| `intel_encoder.rs` | total bytes only | runs here; no timing |
| `whep_selftest.rs` | RTP packets in 10 s (a throughput floor, not latency) | **unreachable** |

**There is no Windows equivalent of `end_to_end_latency`.** The primary shipping platform has no
capture-to-encoded measurement at all. Every serialization point — capture callback → texture ring
→ encode → packetize → RTP — is unmeasured on Windows.

## 14. Stale references

- `audio_spike.rs:44` — `"==== ScreenExtend audio spike (PRD §3) ===="`. `CLAUDE.md` states the PRD
  is not in the tree on any branch. Filed.
- `whep_selftest.rs:33` and `:244` — `"=== M3 WHEP self-test ==="` / `"=== M3 WHEP self-test
  PASSED ==="`. "M3" is a milestone label from a plan that does not exist in the tree.
- `legacy/test/ring.rs:60` — "MUST match src-tauri/macos/ScreenExtendAudio/src/shm_ring.hpp's
  static_asserts". The path is correct and the file exists; the comment describes an enforcement
  that is not implemented. Filed.
- `sck.rs`'s test doc — "This is the only piece testable on 10.15 (SCK classes are absent)" —
  accurate, and worth keeping.
- `CLAUDE.md` itself says "Current version: **0.5.4**"; `package.json` says `0.6.3`.
- `CLAUDE.md`'s test-suite description ("Tests are inline `#[cfg(test)]` modules, several of them
  collected in per-module `test/` subdirectories … Many are hardware- or OS-gated and no-op off
  their platform") is accurate but understates the problem: "no-op" reads as benign, and the
  no-ops are invisible.

No references to AI tooling anywhere in the territory.

## 15. Dead code candidates

- **`streamer/test/whep_selftest.rs`** (363 lines) — no `#[test]`, and its only caller chain
  (`Streamer::whep_selftest` ← `streamer::cli::run`) has no callers. Compiled into release because
  `streamer/mod.rs:11` is `pub mod test;` not `#[cfg(test)] mod test;`, dragging a
  certificate-ignoring `ServerCertVerifier` into the shipped binary. Two findings filed
  (`major-webrtc-whep-selftest-is-unreachable.md`,
  `minor-build-whep-selftest-ships-a-no-verify-tls-verifier-in-release.md`). **Do not delete it —
  make it runnable.** It is the most valuable test in the repo.
- **`SeqGate`** in `streamer/audio/tests.rs:74-115` — a test-only reimplementation of
  `audio-worker.js:124-130`'s dedup, and the two have already diverged by one value at
  `0x7FFF_FFFF`. Filed.
- `streamer/cli.rs` and `Config::from_args`/`print_help` — the whole probe harness. Outside my
  territory to judge, but noted because it is what strands the self-test.

## 16. Capability probing

- `macos_utils/audio/test/mod.rs:9` and `:31` gate on **`macos_at_least(13, 0)`** — a version
  number. `CLAUDE.md`'s tier doctrine is "Probe by class lookup, never by version number." The two
  tests written to defend the doctrine are themselves version-gated, and on any Mac from 2022
  onward they return before asserting anything.
- The tiering function `probe_audio_backend()` itself is untested. Its ranking (ProcessTap → SCK →
  VirtualDevice → NeedsDriverInstall → Unsupported) is pure logic once the probe results are known,
  and could be tested on **any** OS if the probing were separated from the ranking. Filed.
- `probe_display_backend()` (`CgVirtualDisplay` vs `AirPlayFallback`) has no test.
- `select_vendor()` (`windows_utils/streamer/mod.rs:23-34`) is pure string matching over an adapter
  description and is trivially testable. It is untested.
- Missing tier entirely: there is no test anywhere that asserts the **encoder** fallback ladder
  (`Auto` → NVENC → Intel → x264, `Intel` → same-adapter → CPU bridge → x264). The ladder is the
  compatibility story for Windows and nothing exercises it.
- `nm -u` — the macOS dyld-safety check `CLAUDE.md` names as load-bearing exists only as prose.

## 17. Questions and cross-cutting concerns

1. **Why was CI disabled, and can it come back this week?** Nothing in this engagement can be
   measured against a baseline until something runs the suite. `cargo test` must be made green
   first (one doc-comment fix). Filed as `minor-build-no-ci-means-the-test-suite-is-unowned.md`.
2. **The `tprintln!` rule needs a test-module carve-out.** It is currently costing us every number
   the test suite computes, including the only per-frame encode timing in the repo. Cheap to fix,
   and it should be fixed before anyone starts measuring.
3. **Whoever owns `webrtc_session.rs` should also own splitting the WHEP self-test.** The SDP-
   attribute half needs no display and would give both OSes a real negotiation test in CI. This is
   the highest-leverage single change I found.
4. **The client-side territory owner should know there is no JS test runner at all**, and that two
   wire formats (audio DataChannel header, input protocol) are tested on the Rust side and
   unverified on the JS side. Golden-vector fixtures generated from the existing Rust tests would
   close that with no new test infrastructure on the host.
5. **Whoever adds AMD or new macOS FFI should read
   `speculative-build-ffi-layout-test-method.md` first** — the two existing layout-test styles
   differ, neither records provenance, and neither is the right tool for the macOS dyld hazard.
6. **Hardware reality of this box, for whoever plans the measurement runs:** Intel Quick Sync
   works (726 KB of H.264 produced under test); NVIDIA is present per `PATH`; libx264 is present
   in `resources/` but unreachable from the test binary; there is no macOS access from here.

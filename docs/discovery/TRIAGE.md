# TRIAGE — findings sorted into execution workstreams

Every findings file now carries a `Workstream: W<n>` line immediately after its `Component:` line,
and an `Also affects: W<n>` line where the work genuinely spans two owners (101 files carry one).
Component tokens — the second dash-separated field of the filename — have been normalised onto the
controlled set; 59 files were renamed accordingly.

**Counts.** 421 open findings in the pool at the end of the pass. 33 files were folded into 25 merge
groups (the survivor carries the folded detail under a `Merged from: <name>` line; the other file was
deleted). 3 findings were moved to `findings/_out-of-scope/`. 3 findings were resolved and deleted by
other agents *during* this pass — the RTT-unit blocker, the NVENC CONSTQP blocker and the x264 QP
minor — all three verified fixed in the tree: `webrtc_session.rs:899-904` now reads
`round_trip_time` as milliseconds with a comment citing the interceptor source,
`nvidia/encoder.rs:464-466` and `x264/encoder.rs:202-204` return
`BitrateApplied::IgnoredConstantQp` instead of a lying `Ok(())`, and `server.rs:827` sets
`cfg.quality_qp` (a cap) rather than `cfg.qp` (a mode). The pool was still growing when this
document was written; research agents added roughly 50 findings during the triage itself, and every
one of them is included below.

Severity ordering within each workstream is blocker → major → minor → speculative. The `(file:line)`
on each row is the first source reference in that finding — a locator, not the evidence.

---


## W1 — Transport / WebRTC core — `streamer/webrtc_session.rs`, `bitrate.rs`, `server.rs`, `session.rs`, `cloud.rs`, `tls.rs`

- [major] `major-session-blocking-os-calls-on-tokio-workers.md` — The join path runs the heaviest blocking OS calls directly on tokio worker threads (server.rs:1038)
- [major] `major-session-google-stun-on-every-lan-join.md` — Google's public STUN server is contacted on every join, including LAN-only and offline hosts (config.rs:134)
- [major] `major-session-join-is-a-fully-serial-multi-second-chain.md` — Time-to-first-frame is a fully serial chain of independent multi-second waits (static/index.html:948)
- [major] `major-session-tokio-runtime-per-adapter.md` — One full multi-threaded tokio runtime is built per network adapter, each with `num_cpus` workers — N_adapters x N_cpus threads competing for the machine (windows_utils/mod.rs:804)
- [major] `major-webrtc-bitrate-control-is-slower-and-slacker-than-the-field.md` — The bitrate controller ignores 2–10% loss entirely and reacts an order of magnitude slower than every comparable product (src-tauri/src/streamer/bitrate.rs:103)
- [major] `major-webrtc-first-idr-discarded-before-track-bound.md` — The IDR requested at handshake time is silently thrown away, so first frame waits for the client's PLI (webrtc_session.rs:495)
- [major] `major-webrtc-h264-profile-hardcoded-baseline-never-negotiated.md` — H.264 profile is hardcoded to Baseline for every session and never negotiated against the client's SDP (src-tauri/src/streamer/config.rs:163)
- [major] `major-webrtc-ice-sockets-get-no-socket-options.md` — The ICE sockets get no DSCP, no SO_SNDBUF and no SIO_UDP_CONNRESET, and the only hook to fix that costs srflx candidates (webrtc_session.rs:185)
- [major] `major-webrtc-lagged-viewer-requests-idr-and-makes-it-worse.md` — A lagging viewer triggers an IDR, which is bigger, which makes it lag more (windows_utils/streamer/pipeline.rs:43)
- [major] `major-webrtc-no-send-pacer.md` — Video RTP is written in an unpaced burst per frame; webrtc-rs has no pacer (src-tauri/src/streamer/webrtc_session.rs:527)
- [major] `major-webrtc-peerconnection-never-closed-arc-cycle.md` — Every WebRTC session leaks its peer connection, ICE agent, UDP sockets and one OS thread (src-tauri/src/streamer/test/whep_selftest.rs:234)
- [major] `major-webrtc-remb-feedback-is-received-and-discarded.md` — The browser is almost certainly sending us REMB bandwidth estimates and the RTCP loop throws them away (webrtc_session.rs:86)
- [major] `major-webrtc-rtp-mtu-fixed-at-1188-on-relayed-paths.md` — RTP payload MTU is hardcoded at 1188 bytes even on relayed paths, where Moonlight and Sunshine both drop to ~1024 (src-tauri/src/streamer/webrtc_session.rs:502)
- [major] `major-webrtc-session-locality-is-computed-then-thrown-away.md` — SessionLocality is detected once and only logged; nothing in the transport is LAN-aware (webrtc_session.rs:196)
- [major] `major-webrtc-sps-pps-silently-dropped-when-stap-a-exceeds-mtu.md` — If SPS+PPS do not fit one MTU they are silently dropped, and an infinite GOP means the client never recovers (webrtc_session.rs:504)
- [major] `major-webrtc-transport-cc-advertised-but-never-stamped.md` — transport-cc is advertised in the SDP but the sender interceptor is missing, so there is no real BWE (webrtc_session.rs:102)
- [major] `major-webrtc-twcc-feedback-would-identify-the-exact-lost-frame-for-reference-invalidation.md` — TWCC feedback would name the exact lost packets, which is the missing input for Sunshine-style reference-frame invalidation (webrtc_session.rs:466)
- [minor] `minor-cloud-swallowed-errors-and-a-no-op-if-let.md` — The cloud relay client swallows a failed thread spawn and contains a no-op `if let Some(()) = ... {}` (cloud.rs:220)
- [minor] `minor-session-display-correlation-lock-serializes-all-joins-for-up-to-5s.md` — One process-wide async mutex serialises every new-display creation, worst case 5+ seconds each (server.rs:93)
- [minor] `minor-session-mutex-poisoning-cascades-across-every-session.md` — Every shared-state lock is `.lock().unwrap()`, so one panic anywhere poisons the host for all devices (windows_utils/mod.rs:786)
- [minor] `minor-session-usb-tether-is-an-undocumented-low-latency-tier.md` — The wired path halves latency in every competitor's own benchmark, and we neither detect nor mention it (src-tauri/src/streamer/webrtc_session.rs:203)
- [minor] `minor-webrtc-bitrate-cuts-bypass-the-rate-limit-and-can-ratchet-every-120ms.md` — Bitrate cuts deliberately bypass the rate limit, so any sustained signal ratchets the target to the floor (bitrate.rs:87)
- [minor] `minor-webrtc-fua-fragmentation-allocates-and-copies-every-frame-per-viewer.md` — H.264 FU-A fragmentation allocates and memcpys the whole frame, once per viewer, per frame (webrtc_session.rs:495)
- [minor] `minor-webrtc-playout-delay-zero-is-chromium-only-and-unconditional.md` — The playout-delay hint is Chromium-only and is set to zero even on relayed cross-network paths (webrtc_session.rs:164)
- [speculative] `speculative-input-per-session-injector-thread-and-devices.md` — Every client session spawns its own TIME_CRITICAL injector thread and its own synthetic HID devices (webrtc_session.rs:394)
- [speculative] `speculative-session-index-html-reallocated-on-every-page-load.md` — The client page is re-scanned and reallocated on every GET /, and the at-capacity check runs first (server.rs:382)
- [speculative] `speculative-webrtc-get_stats-polled-8-times-a-second-per-session.md` — get_stats() is polled ~8 times a second per session to extract two numbers (webrtc_session.rs:862)
- [speculative] `speculative-webrtc-hevc-tier-is-reachable-now-chrome-136-shipped-h265.md` — An HEVC tier is reachable today — Chrome 136 shipped H.265 in WebRTC by default, and the RTP payloader is already in our dependency tree (webrtc-0.17.1/src/api/media_engine/mod.rs:33)
- [speculative] `speculative-webrtc-no-fec-on-relayed-paths.md` — No forward error correction anywhere; relayed sessions pay a full RTT for every repair (src-tauri/src/streamer/webrtc_session.rs:475)
- [speculative] `speculative-webrtc-rtx-registered-but-disabled.md` — RTX codecs are advertised in the SDP answer but sender RTX is never enabled (src-tauri/src/streamer/webrtc_session.rs:126)
- [speculative] `speculative-webrtc-webtransport-tier-is-buildable-but-unjustified.md` — A WebTransport/HTTP-3 tier is now buildable and fits our cert story, but there is no evidence it would be faster on a LAN (webrtc_session.rs:862)

## W2 — Telemetry, harness and baseline

- [major] `major-build-tprintln-hides-every-test-measurement.md` — Every measurement and skip reason a test prints is swallowed by the log bus (logbus.rs:72)
- [major] `major-cgds-drop-count-never-read.md` — `CGDisplayStreamUpdateGetDropCount` — the API's built-in "are you keeping up?" counter — is never read (cgds.rs:110)
- [major] `major-client-jitter-buffer-levers-are-inert-on-the-webcodecs-path.md` — All three jitter-buffer latency levers are no-ops on the WebCodecs path — the transform runs before the frame buffer (webrtc_session.rs:505)
- [major] `major-client-no-glass-to-glass-telemetry.md` — The client measures no usable latency and reports nothing to the host — the overhaul has no client-side metric (transform-worker.js:57)
- [major] `major-pipeline-win-one-aggregate-timer-and-no-stage-breakdown.md` — The Windows pipeline reports one aggregate avg/max and no per-stage timing, so no capture or encode change can be evaluated (pipeline.rs:631)
- [major] `major-webrtc-no-per-stage-encode-latency-metric.md` — The host measures no per-stage latency, so every optimisation in the overhaul is unfalsifiable (src-tauri/src/streamer/webrtc_session.rs:512)
- [major] `major-webrtc-whep-selftest-is-unreachable.md` — The only end-to-end WebRTC test in the repo cannot be run by any shipped or dev entry point (whep_selftest.rs:248)
- [minor] `minor-cli-probe-harness-is-unreachable-dead-code.md` — The developer probe harness is unreachable from the shipped binary and its help text names a dead crate (src-tauri/src/streamer/cli.rs:14)
- [minor] `minor-docs-glass-to-glass-method-cited-but-undefined.md` — The project's canonical glass-to-glass measurement method is cited but defined nowhere (src-tauri/src/windows_utils/streamer/amd.md:164)

## W3 — Windows capture + encode — `windows_utils/streamer/**` (except `amd*`), `windows_utils/windows_capture/**`

- [blocker] `blocker-intel-reset-drops-all-ext-buffers.md` — Every Intel Quick Sync bitrate change resets the encoder without its ext buffers, throwing away every low-latency setting (intel/encoder.rs:430)
- [blocker] `blocker-nvenc-ring-ignores-idr-on-static-desktop.md` — NVENC ring path never honours an IDR request while the desktop is static, and the GOP is infinite (nvidia/encoder.rs:565)
- [blocker] `blocker-wgc-min-update-interval-forces-dxgi.md` — Windows 10 / pre-24H2 hosts can never use the WGC capture path — they silently fall back to DXGI duplication (pipeline.rs:314)
- [blocker] `blocker-x264-bgra-input-emits-444-stream.md` — The x264 software fallback's "BGRA direct" fast path emits a High 4:4:4 Predictive stream no browser can decode (x264/encoder.rs:113)
- [major] `major-dxgi-cursor-costs-a-full-frame-copy.md` — Showing the cursor on the DXGI path costs an extra full-frame GPU copy on every frame (dxgi/mod.rs:530)
- [major] `major-dxgi-path-serializes-capture-and-encode.md` — The DXGI capture path has no encode ring: capture of frame N+1 cannot start until encode of frame N returns (pipeline.rs:565)
- [major] `major-dxgi-scaler-requires-video-device-on-the-duplication-adapter.md` — On the DXGI path the GPU downscaler is built on the virtual-display adapter, which may not expose ID3D11VideoDevice (dxgi/mod.rs:231)
- [major] `major-intel-buffersizeinkb-is-a-multi-second-vbv.md` — Intel `BufferSizeInKB` is sized from the raw frame area, giving a multi-second CPB instead of a ~1-frame VBV (intel/encoder.rs:394)
- [major] `major-intel-cross-adapter-unimplemented-forces-cpu-bridge.md` — Cross-adapter Quick Sync is unimplemented, so hybrid hosts fall to a CPU bridge with a full GPU->CPU->GPU round trip per frame (intel/encoder.rs:200)
- [major] `major-intel-extra-full-frame-copy-per-frame.md` — The Intel "same-adapter" path is not zero-copy: it does a full-frame CopySubresourceRegion plus a Flush into the VPP surface on every frame (intel/encoder.rs:530)
- [major] `major-intel-init-warnings-swallowed.md` — Intel `check()` treats every oneVPL warning as success, so partial acceleration and silently-changed params are never reported (intel_sys.rs:687)
- [major] `major-intel-libvpl-dll-not-bundled.md` — Quick Sync is loaded from a bare `libvpl.dll` the app does not ship, so Intel hosts may silently never get hardware encode (intel/intel_sys.rs:648)
- [major] `major-intel-lowdelaybrc-and-maxframesize-are-noops-under-cbr.md` — Intel's low-delay rate-control settings are written under CBR, where oneVPL documents them as inapplicable (intel/encoder.rs:417)
- [major] `major-nvenc-idr-only-loss-recovery.md` — Every PLI costs a full IDR: NVENC has one reference frame and no reference-frame invalidation (src-tauri/src/windows_utils/streamer/nvidia/encoder.rs:601)
- [major] `major-nvenc-no-capability-probing.md` — NVENC is configured entirely blind: `nvEncGetEncodeCaps` is never called anywhere in the codebase (nvenc_sys/nvEncodeAPI.rs:1050)
- [major] `major-nvenc-poisoned-slot-recycled-into-free-pool.md` — A poisoned NVENC ring slot is handed back to the free pool, so the WGC capture thread can block a full second per frame (nvidia/encoder.rs:29)
- [major] `major-nvenc-ring-drops-frames-silently.md` — The NVENC ring drops captured frames with no log and no counter when all slots are busy (pipeline.rs:1149)
- [major] `major-nvenc-sdk-version-vs-struct-layout-mismatch.md` — The vendored NVENC bindings advertise API 12.1 but carry SDK 13-era struct and function-list layouts (nvenc_sys/nvEncodeAPI.rs:1088)
- [major] `major-nvenc-single-slice-no-subframe-readback.md` — NVENC emits one slice per frame with sub-frame readback off, so a whole encode duration sits unhidden on the latency path (nvidia/encoder.rs:599)
- [major] `major-nvenc-sync-single-bitstream-defeats-ring.md` — NVENC runs synchronously with one bitstream buffer, so the K=3 texture ring buys no encode pipelining (nvidia/encoder.rs:653)
- [major] `major-pipeline-win-adapter-selection-has-no-runtime-policy.md` — The Windows backend picks a GPU three different ways and coordinates none of them (windows_capture/d3d11.rs:203)
- [major] `major-pipeline-win-capture-timestamp-is-callback-arrival-not-frame-time.md` — The frame timestamp used for A/V sync is "when our callback ran", not the WGC/DXGI frame time (webrtc_session.rs:511)
- [major] `major-pipeline-win-cpu-bridge-readback-stalls-encode-thread.md` — Every CPU-bridge backend stalls the encode thread on a blocking `Map` against a single staging texture (streamer/scaler.rs:243)
- [major] `major-pipeline-win-encode-threads-not-pinned-to-p-cores.md` — Real-time capture and encode threads can be scheduled onto E-cores on hybrid CPUs (tuning.rs:71)
- [major] `major-pipeline-win-no-d3dkmt-realtime-gpu-scheduling-priority.md` — GPU scheduling priority stops at `SetGPUThreadPriority`, so capture is starved under full-screen GPU load (tuning.rs:254)
- [major] `major-vt-intel-macs-refuse-hardware-h264-below-640-wide-and-480-tall.md` — Intel Macs refuse hardware H.264 below ~640×480, and ScreenExtend sizes the encoder from client-chosen dimensions — a small phone or a low video-scale silently lands on the software encoder (config.rs:24)
- [major] `major-wgc-frame-buffer-allocates-a-staging-texture-per-frame.md` — `Frame::buffer()` creates and destroys a full-resolution D3D11 staging texture on every captured frame (frame.rs:186)
- [major] `major-wgc-framepool-never-drains-to-the-newest-frame.md` — The WGC frame-arrived handler takes the OLDEST pooled frame and never drains, so a K=3 pool can add up to 2 frames of pure latency (graphics_capture_api.rs:196)
- [major] `major-wgc-framepool-recreate-drops-to-one-buffer.md` — WGC frame pool falls back to 1 buffer on every resize, silently undoing the K=3 texture-ring decoupling (graphics_capture_api.rs:196)
- [major] `major-wgc-path-has-no-unchanged-frame-skip.md` — The WGC path re-encodes unchanged frames: dirty-region reporting is requested as Default and never read (dxgi/mod.rs:349)
- [major] `major-x264-abi-accepts-mismatched-builds.md` — The x264 loader accepts libx264 builds 150-175 while the vendored `x264_param_t` is pinned to build 164 (x264/x264_sys.rs:188)
- [major] `major-x264-intra-refresh-is-a-noop.md` — x264 intra-refresh can never advance: the refresh period is tied to an "infinite" keyint (x264/x264_sys.rs:34)
- [major] `major-x264-repeat-buffer-copied-every-frame.md` — The x264 BGRA path memcpys a full frame per encode to keep a repeat copy, and clones it again on every repeat (x264/encoder.rs:167)
- [major] `major-x264-repeat-frame-uses-wrong-stride.md` — x264 repeat frames re-encode a padded buffer with an unpadded stride, shearing the picture (x264/encoder.rs:166)
- [major] `major-x264-slice-level-output-via-nalu-process-unused.md` — x264's `nalu_process` slice callback is bound but never used, so a whole software encode sits on the latency path (x264/x264_sys.rs:295)
- [minor] `minor-build-vendored-fork-carries-2000-lines-of-dead-code.md` — Roughly 2,400 of the 5,900 vendored `windows-capture` lines are dead code that still costs `windows` crate features and build time (dxgi/mod.rs:352)
- [minor] `minor-build-vendored-fork-has-no-provenance-record.md` — The vendored `windows-capture` fork records neither its upstream version nor its patch list (mod.rs:23)
- [minor] `minor-capture-win-set-display-scale-reports-success-after-failing.md` — set_display_scale returns Ok after 2 seconds even when the DPI was never applied (capture.rs:411)
- [minor] `minor-docs-intel-md-design-note-deleted-while-amd-md-survives.md` — The Intel encoder's design note was deleted the moment the encoder was written; amd.md survives (-)
- [minor] `minor-dxgi-dirty-rects-fetched-then-discarded.md` — DXGI dirty and move rects are fetched every frame and thrown away — only the "is it empty" bit is used (dxgi/mod.rs:366)
- [minor] `minor-dxgi-vendored-wrapper-dropped-hdr-formats-behind-an-unreachable.md` — The vendored DXGI duplication wrapper deleted the 10-bit/sRGB formats, leaving `unreachable!()` on the HDR path (dxgi_duplication_api.rs:96)
- [minor] `minor-intel-device-busy-sleeps-1ms-on-hot-path.md` — Intel's DEVICE_BUSY retry loops sleep 1 ms on the encode thread, which Windows may round up to a full timer tick (intel/encoder.rs:628)
- [minor] `minor-intel-fps-probe-omits-ext-buffers-and-silently-caps-at-60.md` — The Intel frame-rate probe queries a different configuration than it will use, and silently caps at 60 when the probe fails (intel/encoder.rs:1017)
- [minor] `minor-intel-init-log-and-claude-md-claim-settings-the-code-does-not-verify.md` — The Intel init log asserts a configuration it never reads back, and `NumSlice` collapses to 4 at every common resolution (intel/encoder.rs:276)
- [minor] `minor-intel-no-onevpl-api-version-gate.md` — The oneVPL session is created with no API-version filter, so a 1.x runtime passes init and fails at the first frame (intel/encoder.rs:889)
- [minor] `minor-intel-numslice-is-fixed-instead-of-maxslicesize.md` — Quick Sync slices are a hard-coded height guess instead of oneVPL's `MaxSliceSize` (intel/encoder.rs:413)
- [minor] `minor-intel-onevpl-loader-leaked-per-session.md` — The oneVPL dispatcher loader is leaked on every successful Intel encoder creation (intel/encoder.rs:865)
- [minor] `minor-nvenc-encoder-is-self-referential-and-declared-send.md` — `Encoder` stores a pointer into itself, is declared `Send`, and is then moved across threads (nvidia/encoder.rs:78)
- [minor] `minor-nvenc-function-pointer-expect-panics-on-every-frame-path.md` — Every NVENC call re-unwraps its driver function pointer with `.expect(...)`, so a partially populated function list panics the encode thread (nvenc_sys/nvEncodeAPI.rs:1040)
- [minor] `minor-nvenc-keyed-mutex-one-second-timeout-stalls-the-capture-thread.md` — A wedged encoder blocks the capture thread for a full second on every frame (KEY_TIMEOUT_MS = 1000) (nvidia/encoder.rs:29)
- [minor] `minor-nvenc-map-unmap-input-resource-every-frame.md` — NVENC maps and unmaps the input resource on every single frame (nvidia/encoder.rs:158)
- [minor] `minor-nvenc-per-frame-bitstream-memcpy-and-heap-alloc.md` — Every NVENC frame costs one heap allocation plus a memcpy out of write-combined driver memory (nvidia/encoder.rs:789)
- [minor] `minor-nvenc-ring-stage-flushes-the-immediate-context-per-frame.md` — The NVENC ring stages every frame with a full immediate-context Flush on the WGC capture callback thread (pipeline.rs:1164)
- [minor] `minor-nvenc-sps-vui-missing-bitstream-restriction.md` — NVENC never sets `bitstreamRestrictionFlag`, so the SPS has no reorder hint of its own (nvidia/encoder.rs:632)
- [minor] `minor-nvenc-vbv-is-one-frame-and-reconfig-uses-stale-fps.md` — NVENC's VBV is exactly one frame with zero burst headroom, and the bitrate reconfigure recomputes it from a possibly stale fps (nvidia/encoder.rs:578)
- [minor] `minor-pipeline-win-dead-code-in-windows-streamer-core.md` — Dead code in the Windows streamer core: request_idr, Pipeline::frame_duration, AmfBackend (nvidia/encoder.rs:493)
- [minor] `minor-pipeline-win-encode-core-mutex-expect-panics-the-capture-thread.md` — A panic in the repeater thread poisons the encode mutex and then panics the WGC capture callback (pipeline.rs:1653)
- [minor] `minor-pipeline-win-pending-bitrate-target-is-lost-when-set-fails.md` — A failed set_bitrate discards the requested target forever instead of retrying (pipeline.rs:62)
- [minor] `minor-pipeline-win-time-critical-thread-count-scales-with-clients.md` — Each session adds two to three THREAD_PRIORITY_TIME_CRITICAL threads in a HIGH_PRIORITY_CLASS process (tuning.rs:151)
- [minor] `minor-pipeline-win-tune-transport-thread-leaks-mmcss.md` — tune_transport_thread leaks an MMCSS registration per tokio worker and never checks whether it worked (streamer/mod.rs:41)
- [minor] `minor-wgc-frame-delivery-goes-through-a-win32-message-pump.md` — WGC frames are delivered through a Win32 message pump hop instead of `CreateFreeThreaded` (graphics_capture_api.rs:196)
- [minor] `minor-wgc-hardware-cursor-not-composed-unverified.md` — The IDD enables a hardware cursor, so the cursor is not in the composed image; the WGC path is unverified (windows_utils/streamer/dxgi/mod.rs:59)
- [minor] `minor-wgc-min-update-interval-is-a-ratio-not-a-flat-floor.md` — WGC's `MinUpdateInterval` is set as a fraction of the target frame period, which under-delivers at every frame rate (pipeline.rs:319)
- [minor] `minor-x264-colour-convert-is-scalar-on-at-most-four-threads.md` — The x264 I420 colour conversion is scalar C on at most four threads, sized independently of the encoder's thread budget (x264/encoder.rs:411)
- [minor] `minor-x264-perf-rationale-comments-deleted-by-the-next-commit.md` — A perf commit's own rationale comments were deleted by the perf commit that followed it (src-tauri/src/windows_utils/streamer/x264/encoder.rs:370)
- [minor] `minor-x264-rate-control-is-abr-without-nal-hrd-cbr.md` — x264 runs ABR with a VBV instead of CBR with HRD, so the VBV is advisory rather than binding (x264/encoder.rs:79)
- [speculative] `speculative-build-module-level-clippy-cargo-lints-in-vendored-fork.md` — The vendored fork re-enables crate-wide clippy lint groups (`clippy::cargo`, `multiple_crate_versions`, `missing_docs`) from inside a module (mod.rs:157)
- [speculative] `speculative-intel-bridge-uses-capture-context-from-encode-thread.md` — The Intel input bridge drives the capture device's immediate context from the encode thread, with multithread protection only best-effort (intel/encoder.rs:56)
- [speculative] `speculative-intel-copy-box-can-exceed-source-when-upscaling.md` — The Intel VPP copy box is sized from `max(native, requested)`, so an upscale request would copy past the source texture (intel/encoder.rs:208)
- [speculative] `speculative-intel-vpp-syncpoint-never-synchronised.md` — The VPP sync point is created and then dropped without ever being synchronised (intel/encoder.rs:617)
- [speculative] `speculative-nvenc-intra-refresh-is-unreachable-from-any-shipping-path.md` — Intra refresh is implemented in the NVENC encoder but unreachable from any shipping code path (src-tauri/src/streamer/config.rs:167)
- [speculative] `speculative-nvenc-splitencodemode-never-requested-or-reported.md` — NVENC split-frame encoding is never requested and the engine count is never reported (nvidia/nvenc_sys/nvEncodeAPI.rs:4247)
- [speculative] `speculative-wgc-handler-mutex-is-held-for-the-whole-encode.md` — The WGC handler mutex is held for the entire frame callback (i.e. the whole encode) and is publicly handed out by `CaptureControl::callback()` (graphics_capture_api.rs:318)

## W4 — AMD AMF backend (new)

- [major] `major-amd-amf-latency-property-set.md` — The AMF AVC latency property set: USAGE first, one-frame VBV, no B, no pre-analysis, no filler (EncoderLatency.cpp:378)
- [major] `major-amd-amf-layout-test-harness.md` — Verify the AMF FFI on a box with no AMD GPU: generate amd_layout.rs from the real headers with cl.exe (-)
- [major] `major-amd-amf-one-frame-in-flight.md` — The AMF submit/query loop must be one-frame-in-flight with a 1 ms blocking QueryOutput, not a sleep-poll (texture-amf.cpp:736)
- [major] `major-amd-amfinit-version-pin.md` — AMFInit must be called with a LOW pinned version, not the vendored header's AMF_FULL_VERSION (texture-amf.cpp:2750)
- [major] `major-amd-hosts-get-no-hardware-encoder.md` — AMD-GPU Windows hosts silently encode in software x264 — the AMF backend is a stub nothing selects (pipeline.rs:1443)
- [major] `major-amd-scaler-needs-nv12-output.md` — scaler.rs can only emit BGRA; AMF needs NV12, and BGRA input triggers an uncontrolled internal GPU convert (scaler.rs:76)
- [major] `major-amd-ull-usage-fallback.md` — AMF USAGE=ULTRA_LOW_LATENCY breaks Init() on pre-RDNA drivers; a LOW_LATENCY fallback is mandatory (src/video.cpp:1068)
- [minor] `minor-amd-amf-does-not-write-vui-colour-signalling.md` — AMF never writes OUTPUT_COLOR_PROFILE into the bitstream, so AMD and NVIDIA hosts emit differently signalled H.264 (nvidia/encoder.rs:632)
- [minor] `minor-amd-detect-vendor-by-dxgi-vendorid.md` — GPU vendor is detected by DXGI description string, not VendorId; WARP and rebranded adapters slip through (mod.rs:23)
- [minor] `minor-amd-encoderbackend-trait-is-dead.md` — The EncoderBackend trait has one bail-only implementor and no callers; implementing AMF against it would produce dead code (platform.rs:169)
- [minor] `minor-amd-intra-refresh-and-ltr-followups.md` — ULTRA_LOW_LATENCY already enables intra-refresh; MAX_NUM_REFRAMES = 1 would silently disable it, and LTR excludes it (pipeline.rs:120)
- [minor] `minor-amd-silence-amf-console-trace.md` — AMF writes to the console by default, bypassing logbus; disable its writer and bind GetResultText (texture-amf.cpp:2750)
- [minor] `minor-amd-slices-per-frame-default-one.md` — amd.md's SLICES_PER_FRAME = 4..8 buys no latency without OUTPUT_MODE = SLICE, and costs quality (nvidia/encoder.rs:599)
- [minor] `minor-docs-amd-md-references-missing-documents-and-ai-tooling.md` — `amd.md` cites documents that are not in the tree and instructs the reader about AI hallucination (amd.md:164)

## W5 — macOS capture + encode — `macos_utils/streamer/**`

- [blocker] `blocker-cgds-iosurface-use-count-never-incremented.md` — CGDisplayStream frames are retained but their IOSurface use count is never incremented — WindowServer may overwrite a frame we are still encoding (cgds.rs:134)
- [blocker] `blocker-cgds-queue-depth-starvation.md` — CGDisplayStream queueDepth=2 while the pipeline holds 2 IOSurfaces — capture self-throttles (src-tauri/src/macos_utils/streamer/cgds.rs:71)
- [major] `major-audio-mac-sck-stop-frees-ctx-with-handler-possibly-in-flight.md` — SCK audio `stop()` frees the delegate context while a sample handler may still be queued (sck_audio.rs:230)
- [major] `major-cgds-capture-timestamp-is-callback-arrival-not-display-time.md` — The RTP timestamp comes from callback-arrival time; the real display/presentation time is thrown away (cgds.rs:172)
- [major] `major-cgds-dirty-rect-skip-can-blank-a-new-client.md` — Empty-dirty-rect frames are dropped, and the keepalive path needs a prior frame that may never arrive (cgds.rs:115)
- [major] `major-cgds-obsoleted-in-the-macos-15-sdk-and-triggers-sequoia-privacy-alerts.md` — CGDisplayStream is *obsoleted* (not merely deprecated) in the macOS 15 SDK, and using it triggers Sequoia's recurring privacy alert — yet it is a link-time dependency and the sole fallback on 12.3+ (cgds.rs:16)
- [major] `major-sck-first-frame-after-start-is-discarded.md` — The SCK backend discards `SCFrameStatusStarted` — the first frame after the stream starts is thrown away (sck.rs:87)
- [major] `major-sck-minimum-frame-interval-exactly-one-over-fps-drops-frames.md` — SCK `minimumFrameInterval = 1/fps` beats against the compositor and loses ~15 % of frames; Apple's documented "capture at native refresh" value is `kCMTimeZero` (sck.rs:556)
- [major] `major-sck-queue-depth-3-contradicts-apples-documented-default.md` — SCK `queueDepth = 3` is set with a comment claiming it is "lowest latency"; Apple's header says the default is 8 and that a deeper queue prevents stalling the display stream (sck.rs:573)
- [major] `major-sck-tier-enabled-at-12.3-but-known-broken-before-13.0.md` — The SCK capture tier is switched on at macOS 12.3, where Chromium reports full-screen capture is broken; libwebrtc gates the same API at 14.0 (mod.rs:79)
- [major] `major-vt-complete-frames-blocks-the-realtime-thread.md` — On pre-11.0 macOS the keepalive path calls VTCompressionSessionCompleteFrames on the real-time thread (pipeline.rs:209)
- [major] `major-vt-datarate-limits-imposes-a-hard-cap-on-a-realtime-screen-encoder.md` — VideoToolbox is given a hard `DataRateLimits` cap on top of `AverageBitRate`; comparable products ship none (encoder.rs:253)
- [major] `major-vt-encode-loop-exits-silently-on-first-submit-error.md` — A single encode-submit failure kills the encode loop forever; the session keeps running with no video (pipeline.rs:188)
- [major] `major-vt-encoder-fps-floored-at-60.md` — Capture and encode are forced to at least 60 fps even for a 30 Hz display or client (macos_utils/streamer/config.rs:30)
- [major] `major-vt-encoder-implementation-is-never-read-back-and-decides-a-4-to-16-frame-hold.md` — VideoToolbox picks the encoder implementation for us and we never read back which one; the choice is worth 1 vs 4–16 frames of buffering (encoder.rs:73)
- [major] `major-vt-encoder-not-pinned-to-the-gpu-driving-the-virtual-display.md` — The encoder is never pinned to the GPU that drives the virtual display (encoder.rs:85)
- [major] `major-vt-hardware-encoder-is-a-scarce-machine-wide-resource-with-no-detection.md` — The hardware H.264 encoder is a machine-wide scarce resource and ScreenExtend never notices losing it (encoder.rs:73)
- [major] `major-vt-ltr-frames-would-replace-idr-on-every-pli.md` — Every packet-loss recovery costs a full IDR; VideoToolbox has had long-term reference frames since macOS 12.0 and they are unused (pipeline.rs:41)
- [major] `major-vt-no-cap-on-frames-in-flight-inside-the-encoder.md` — Nothing limits how many frames are in flight inside VideoToolbox; a comparable product measured ~8 frames queued when overfed (encoder.rs:266)
- [major] `major-vt-pending-ts-mutex-priority-inversion.md` — A std::sync::Mutex sits between the real-time encode thread and the drain thread (pipeline.rs:140)
- [major] `major-vt-qos-thread-policy-returns-ignored.md` — Encode thread sets a QoS class and THEN thread_policy_set; every return code is discarded (pipeline.rs:133)
- [major] `major-vt-silent-software-encoder.md` — VideoToolbox can silently pick the SOFTWARE H.264 encoder and nothing notices (encoder.rs:73)
- [major] `major-vt-supported-property-dictionary-never-queried.md` — VideoToolbox can be asked what it supports; ScreenExtend guesses from the OS version instead (encoder.rs:74)
- [major] `major-vt-unbounded-encoder-output-channel.md` — VideoToolbox output callback pushes into an UNBOUNDED channel: encoded frames queue without limit (encoder.rs:71)
- [major] `major-vt-workgroup-created-but-never-driven.md` — os_workgroup is created and joined but never given an interval, so it is inert (qos.rs:128)
- [minor] `minor-audio-mac-sck-dummy-video-lights-the-screen-recording-indicator.md` — SCK audio capture runs a real 2x2 screen stream, so audio-only streaming shows the screen-recording indicator (sck_audio.rs:114)
- [minor] `minor-cgds-minimum-frame-time-coalescing.md` — CGDisplayStream `minimumFrameTime` holds every frame for up to one frame interval (cgds.rs:70)
- [minor] `minor-cgds-pixelbuffer-leaked-on-nonzero-cvreturn.md` — CVPixelBufferCreateWithIOSurface leaks its output when it returns non-zero with a non-null pointer (cgds.rs:133)
- [minor] `minor-sck-color-config-diverges-from-cgds.md` — SCK sets no color matrix / color space while CGDisplayStream forces 709, and 420f is full-range but never signalled (cgds.rs:81)
- [minor] `minor-vt-gpu-struct-is-an-empty-stub-claude-md-says-otherwise.md` — `macos_utils::streamer::Gpu` is an empty stub; CLAUDE.md describes it as a Metal texture cache (CLAUDE.md:354)
- [minor] `minor-vt-low-latency-version-gate-is-redundant-with-the-spec-ladder.md` — The 11.0 version gate on EnableLowLatencyRateControl is redundant: the spec ladder already falls through (encoder.rs:73)
- [minor] `minor-vt-panics-reachable-at-runtime.md` — Reachable unwrap/expect on the macOS streamer path (cgds.rs:98)
- [minor] `minor-vt-per-frame-allocations-in-hot-path.md` — Three per-frame heap allocations on the macOS capture/encode path (cgds.rs:136)
- [minor] `minor-vt-periodic-idr-every-10-seconds.md` — MaxKeyFrameIntervalDuration=10s forces a periodic IDR that the loss-driven design does not need (encoder.rs:203)
- [minor] `minor-vt-pipeline-start-leaks-backend-and-thread.md` — pipeline::start deliberately leaks the capture backend, the activity token, the stop flag and the encode thread (pipeline.rs:260)
- [minor] `minor-vt-set-optional-results-discarded.md` — Every optional VideoToolbox property silently no-ops when its symbol is missing (encoder.rs:566)
- [minor] `minor-vt-setpriority-fails-for-unprivileged-users.md` — setpriority(PRIO_PROCESS, 0, -10) requires root and logs an error on every normal launch (tuning.rs:28)
- [speculative] `speculative-cgds-windowserver-does-the-downscale-with-no-source-rect.md` — All scaling is delegated to WindowServer/SCK with no SourceRect, DestinationRect or scaling-quality control (pipeline.rs:253)
- [speculative] `speculative-vt-encode-loop-4ms-idle-and-wake-token-race.md` — The encode loop's 4 ms idle timeout plus a coalescing wake token can add up to a frame of delay (pipeline.rs:169)

## W6 — macOS permissions, hosted network, virtual display, setup/compat

- [blocker] `blocker-hostednet-mac-private-spi-called-without-respondstoselector.md` — `startHostAPModeWithSSID:` is a private CoreWLAN SPI invoked with a bare `msg_send!` — an unrecognized selector raises an Obj-C exception through Rust frames (hosted_network.rs:59)
- [blocker] `blocker-permissions-accessibility-required-blocks-all-startup.md` — Accessibility is `required: true`, so a Mac user who only wants a second monitor cannot start the app at all (permissions.rs:172)
- [blocker] `blocker-permissions-preflight-shortcircuit-locks-the-gate.md` — macOS permission gate can never close: the stale `CGPreflightScreenCaptureAccess` short-circuit runs before the probe built to work around it (permissions.rs:123)
- [major] `major-compat-mac-capabilities-gated-by-a-sw-vers-subprocess.md` — macOS capability selection forks `sw_vers` and compares version numbers instead of probing, with no caching and a fail-open default (macos_utils/streamer/mod.rs:61)
- [major] `major-hostednet-mac-offers-only-wep-or-a-silently-open-network.md` — The macOS hosted network is WEP at best and silently falls back to a fully open AP (hosted_network.rs:10)
- [major] `major-hostednet-mac-sweep-security-types-for-wpa2.md` — The macOS HostAP security types are 2012 magic numbers; sweep them for a WPA2 constant (macos_utils/hosted_network.rs:10)
- [major] `major-net-direct-link-lowlatency-mode.md` — Make the one-hop host-AP link a first-class, measured low-latency mode (hosted_network.rs:60)
- [major] `major-net-ipv6-addresses-are-enumerated-but-never-served.md` — IPv6 addresses are collected and shown but silently dropped when binding streamers — an IPv6-only network cannot host at all (networking.rs:140)
- [major] `major-net-mac-adapter-poll-forks-syscalls-and-corewlan-every-second.md` — The macOS network watcher re-enumerates every adapter once a second with fresh sockets, ioctls and CoreWLAN calls instead of subscribing to change notifications (networking.rs:277)
- [major] `major-permissions-check-blocks-the-main-thread-for-seconds.md` — `check_permissions` is a synchronous Tauri command that can block the main thread for 5 s, and it is polled five times a second (permissions.rs:170)
- [major] `major-permissions-degrade-do-not-die-on-a-wrong-permission-read.md` — A permission read that is merely *unknown* is treated as denied and refuses startup; the permission model has no third state (permissions.rs:174)
- [major] `major-permissions-probe-latch-returns-false-and-can-stick-forever.md` — The screen-recording probe latch returns `false` on every call but the first, and a panicking probe thread wedges it permanently (permissions.rs:123)
- [major] `major-permissions-relaunch-required-ui-and-one-click-restart.md` — macOS genuinely requires a relaunch for some grants, but the UI never says so and the Relaunch button does not go through LaunchServices (permissions.rs:40)
- [major] `major-permissions-settings-url-wrong-pane-on-ventura-plus.md` — `open_permission_settings` only emits the pre-Ventura System Preferences URL (permissions.rs:202)
- [major] `major-permissions-window-name-probe-is-the-live-tcc-read.md` — The only cheap probe that reflects a Screen Recording grant without a relaunch is the `CGWindowListCopyWindowInfo` window-name read, and ScreenExtend does not use it (permissions.rs:98)
- [major] `major-vdisplay-mac-activate-mode-can-block-three-seconds-on-the-join-path.md` — `activate_mode` polls with 50 ms sleeps for up to 3 s on the join path, and its failure is logged and swallowed (virtual_display.rs:230)
- [major] `major-vdisplay-mac-discards-the-dpr-mode-ladder-and-always-creates-at-1x.md` — macOS ignores the DPR mode ladder entirely and always creates the virtual display at 1x, forcing a mode switch right after join (virtual_display.rs:459)
- [major] `major-vdisplay-mac-no-termination-handler.md` — The macOS virtual display sets a dispatch queue for a termination handler it never installs (macos_utils/virtual_display.rs:172)
- [major] `major-vdisplay-mac-registry-mutex-held-across-windowserver-ipc.md` — The virtual-display registry mutex is held across ObjC allocation and `applySettings:` WindowServer IPC, serializing reconfigure against create (virtual_display.rs:302)
- [minor] `minor-compat-mac-install-drivers-stub-makes-the-setup-error-dialog-useless.md` — On macOS `install_drivers` is a `true`-returning stub, so the bootstrap "Install drivers" recovery path is a guaranteed-useless 5 s wait (macos_utils/mod.rs:456)
- [minor] `minor-compat-mac-report-omits-the-capture-backend-and-cannot-fail-a-blocking-check.md` — `CompatibilityReport` names the display and audio tiers but not the capture tier, and its only blocking check is a version string (compatibility.rs:34)
- [minor] `minor-hostednet-mac-hostap-m-is-dead-code-with-hardcoded-credentials.md` — `hostap.m` is uncompiled dead code that ignores its own arguments and hardcodes an SSID and password (CLAUDE.md:376)
- [minor] `minor-net-hostap-avoid-awdl-social-channels.md` — The macOS host AP prefers channel 44, which is an AWDL social channel (hosted_network.rs:36)
- [minor] `minor-net-mac-cli-adapters-duplicates-get-network-adapters.md` — `cli_adapters()` is a 75-line copy-paste of `get_network_adapters()` that has already diverged (networking.rs:118)
- [minor] `minor-net-mac-ssid-read-needs-location-permission-and-flaps-the-streamers.md` — The Wi-Fi SSID read requires Location Services on macOS 10.15+; without it the network name silently becomes "Wi-Fi" and a flap tears down every streamer (networking.rs:97)
- [minor] `minor-permissions-ax-messaging-timeout-is-set-process-wide.md` — `accessibility_api_enabled()` contributes no signal, costs main-thread time, and silently sets the process-wide AX messaging timeout (permissions.rs:40)
- [minor] `minor-session-mac-device-reporter-spawns-a-thread-per-join-and-leave.md` — `TauriDeviceReporter` spawns a fresh OS thread for every join and leave just to call `sync_streamers` (device_reporter.rs:109)
- [minor] `minor-session-mac-set-server-ports-sleeps-300ms.md` — `set_server_ports` hard-shuts every streamer and then sleeps 300 ms inside a synchronous Tauri command (macos_utils/mod.rs:392)
- [speculative] `speculative-permissions-sequoia-monthly-reconsent-will-interrupt-long-sessions.md` — macOS 15's monthly screen-recording re-consent will interrupt a live ScreenExtend session, and nothing handles a mid-session revoke (macos_utils/streamer/mod.rs:84)
- [speculative] `speculative-vdisplay-mac-descriptor-uses-a-96dpi-physical-size-and-a-fixed-vendor-product-id.md` — The virtual display descriptor declares a 96-DPI physical size and a fixed vendor/product ID for every client (virtual_display.rs:162)
- [speculative] `speculative-vdisplay-mac-refresh-deadline-never-set.md` — CGVirtualDisplaySettings has a refreshDeadline property and nobody sets it (virtual_display.rs:96)
- [speculative] `speculative-vdisplay-mac-teardown-relies-on-dropping-the-last-retained-reference.md` — Virtual-display teardown on macOS is only a `Retained` drop — nothing verifies the display actually went away (virtual_display.rs:493)

## W7 — macOS audio — `macos_utils/audio/**`, `macos/ScreenExtendAudio/**`

- [major] `major-audio-mac-capture-ns-is-drain-time.md` — macOS audio `capture_ns` is stamped at drain time, not capture time, and can regress (macos_utils/audio/mod.rs:284)
- [major] `major-audio-mac-driver-onprocessmixedoutput-has-two-identical-branches.md` — The AudioServerPlugIn's channel-count branch in `OnProcessMixedOutput` is a no-op: both arms write the same raw interleaved buffer (device.cpp:23)
- [major] `major-audio-mac-driver-shm-mode-depends-on-coreaudiod-umask.md` — The shared-memory ring's permissions depend on `coreaudiod`'s umask, and a failed open degrades to the HAL fallback with zero logging (shm_ring.cpp:22)
- [major] `major-audio-mac-legacy-driver-pkg-is-never-bundled.md` — `install_audio_driver` resolves `resources/ScreenExtendAudio.pkg`, which no build ever produces or bundles — the whole 10.15-12.x audio tier is unreachable in a shipped app (macos_utils/mod.rs:468)
- [major] `major-audio-mac-playthrough-85ms-monitor-latency.md` — Legacy-tier local playthrough targets an 85 ms buffer, so the host's own speakers lag by 85 ms (legacy/playthrough.rs:44)
- [major] `major-audio-mac-process-tap-mutates-user-device-sample-rate.md` — Process Tap start silently rewrites the user's default output device to 48 kHz and never restores it (process_tap.rs:320)
- [major] `major-audio-mac-sw-vers-fork-on-every-probe.md` — The audio capture start path re-probes past its own cache, forking `sw_vers` 3-5 times before the first packet (audio/mod.rs:83)
- [major] `major-audio-mac-tap-aggregate-buffer-frame-size-never-set.md` — The Process Tap aggregate device's IO buffer size is never set, so macOS audio capture runs at whatever the HAL defaults to (process_tap.rs:700)
- [major] `major-audio-mac-tiering-gated-by-version-not-capability.md` — Audio backend tiering is gated by OS version number, contradicting the project's probe-by-capability doctrine (audio/mod.rs:100)
- [major] `major-audio-mac-unpark-inside-realtime-callback.md` — The lock-free audio ring calls `Thread::unpark()` from inside the CoreAudio real-time callback (macos_utils/audio/ring.rs:68)
- [major] `major-audio-mac-volume-key-tap-blocks-on-coreaudio-ipc.md` — The volume-key CGEventTap callback makes blocking CoreAudio IPC calls, risking a system-wide input stall (legacy/volume_keys.rs:81)
- [major] `major-audio-mac-watchdog-launchagent-installed-per-start-never-removed.md` — Every legacy-audio start installs a permanent LaunchAgent that forks `pgrep` every 30 seconds forever (legacy/mod.rs:95)
- [minor] `minor-audio-mac-dead-code-inventory.md` — Dead code in the macOS audio module: two unreachable probe constructors and four write-only fields (process_tap.rs:248)
- [minor] `minor-audio-mac-driver-onreadclientinput-leaves-the-buffer-tail-uninitialized.md` — `OnReadClientInput` truncates on partial frames and leaves the tail of the client buffer uninitialized (device.cpp:54)
- [minor] `minor-audio-mac-driver-overruns-counter-is-never-written.md` — The shm ring's `overruns` counter is declared as part of the ABI but never incremented, so reader lag is undetectable (shm_ring.hpp:20)
- [minor] `minor-audio-mac-dropped-backpressure-counter-clobbered.md` — `dropped_backpressure` is incremented by the capture callbacks and then overwritten by the worker (process_tap.rs:153)
- [minor] `minor-audio-mac-hal-input-fallback-assumes-f32-stereo-without-checking.md` — The legacy HAL-input fallback reinterprets whatever the device delivers as f32 stereo without parsing the ASBD (legacy/shm_reader.rs:271)
- [minor] `minor-audio-mac-legacy-probe-enumerates-every-hal-device-uncached.md` — `probe_audio_backend()` enumerates every CoreAudio device and reads a UID from each, on every call (audio/mod.rs:83)
- [minor] `minor-audio-mac-per-sample-copy-loops-instead-of-memcpy.md` — Every audio buffer crossing is a per-sample scalar loop, not a memcpy (ring.rs:60)
- [minor] `minor-audio-mac-playthrough-leaves-extra-output-channels-untouched.md` — Playthrough leaves output channels 3+ untouched on non-interleaved devices, replaying whatever was in the buffer (legacy/playthrough.rs:153)
- [minor] `minor-audio-mac-shm-abi-asserted-on-one-side-only.md` — The Rust/C++ shared-memory ABI contract is asserted only on the Rust side (macos_utils/audio/legacy/test/ring.rs:58)
- [minor] `minor-audio-mac-shm-abi-sampleformat-and-overruns-unused.md` — The shm ABI's `sampleFormat` field is written by the driver and never validated by the Rust reader (shm_ring.hpp:20)
- [minor] `minor-audio-mac-swallowed-errors-and-panicking-unwraps.md` — Swallowed errors and runtime-reachable panics in the macOS audio module (process_tap.rs:320)
- [minor] `minor-audio-mac-unbounded-packet-channel-and-blocking-teardown.md` — The SCK backend can hold the AudioHub mutex for 10 seconds on start and 5 on stop (streamer/platform.rs:149)
- [minor] `minor-docs-audio-driver-readme-cites-four-files-that-do-not-exist.md` — The AudioServerPlugIn README cites four missing documents, two missing build inputs, and a status line the dev box contradicts (-)
- [speculative] `speculative-audio-mac-install-does-not-restart-coreaudiod.md` — Driver install never restarts `coreaudiod`, so a first install probably always reports `needs_reboot` (legacy/installer.rs:66)
- [speculative] `speculative-audio-mac-no-sample-rate-conversion-anywhere.md` — Nothing in the macOS audio path resamples; a non-48 kHz source is rejected rather than converted (format.rs:74)
- [speculative] `speculative-audio-mac-shm-reader-time-constraint-thread-parks.md` — The shm reader asks for a Mach time-constraint (real-time) policy and then parks on a condvar (legacy/shm_reader.rs:213)

## W8 — Windows audio + shared audio core — `windows_utils/audio/**`, `streamer/audio/**`

- [major] `major-audio-core-bridge-thread-is-a-removable-hop.md` — The AudioHub bridge thread adds a whole scheduling hop to every 5 ms audio packet, for nothing (streamer/audio/mod.rs:146)
- [major] `major-audio-core-broadcast-lag-swallowed-without-discontinuity.md` — Broadcast lag silently swallows up to 640 ms of audio without setting the discontinuity flag (streamer/audio/mod.rs:93)
- [major] `major-audio-core-fallback-track-discards-capture-timestamp.md` — The Opus-track fallback path throws away capture_ns and flags, so A/V sync drifts permanently after any gap (streamer/audio/mod.rs:38)
- [major] `major-audio-core-hub-mutex-held-across-device-open-and-thread-join.md` — AudioHub holds its mutex across device open and across a thread join that can hang forever (streamer/audio/mod.rs:146)
- [major] `major-audio-core-no-fec-on-an-unreliable-datachannel.md` — Opus runs with FEC off and packet-loss-perc unset on a zero-retransmit DataChannel (webrtc_session.rs:676)
- [major] `major-audio-win-capture-timestamp-taken-at-encode-time.md` — Windows audio packets are timestamped at encode time, not capture time — the QPC position is read and thrown away (loopback.rs:245)
- [major] `major-audio-win-engine-period-lowerable-via-render-companion.md` — The silent render companion can lower the whole endpoint's engine period, cutting WASAPI capture latency from 10 ms to ~2.67 ms (silence.rs:91)
- [major] `major-audio-win-silence-companion-death-is-undetected.md` — If the mandatory silent render companion dies, audio stops with no log and nothing restarts it (loopback.rs:345)
- [minor] `minor-audio-core-backpressure-drop-is-sampled-and-silent.md` — Audio backpressure is checked one packet in four and drops packets without flagging discontinuity (webrtc_session.rs:773)
- [minor] `minor-audio-core-double-copy-and-per-packet-allocation-in-the-forwarder.md` — Every audio packet is copied twice and heap-allocated once on the way to the DataChannel (webrtc_session.rs:752)
- [minor] `minor-audio-core-opus-settings-are-never-read-back.md` — Opus encoder settings are applied but never verified, and the 5 ms frame choice is undocumented (encoder.rs:89)
- [minor] `minor-audio-core-opus-unconstrained-vbr-inflates-jitter.md` — Opus runs unconstrained VBR on a real-time link, so packet size — and therefore arrival jitter — is unbounded (encoder.rs:97)
- [minor] `minor-audio-win-cotaskmem-leak-and-unstopped-client-on-acquire-failure.md` — `acquire()` leaks the mix-format allocation and leaves a started client behind on partial failure (loopback.rs:353)
- [minor] `minor-audio-win-downmix-path-is-unreachable-at-runtime.md` — The multichannel downmix / integer conversion path is unreachable at runtime; CLAUDE.md says otherwise (loopback.rs:358)
- [minor] `minor-audio-win-process-loopback-tier-not-implemented.md` — Windows has a second, endpoint-independent loopback tier (process loopback) that ScreenExtend does not implement or probe for (loopback.rs:307)
- [minor] `minor-audio-win-realtime-thread-allocates-sorts-and-logs.md` — The MMCSS "Pro Audio" capture thread allocates, sorts 4096 u64 and takes the log-bus mutex every 2 s (loopback.rs:113)
- [minor] `minor-audio-win-unbounded-packet-channel-hides-backpressure.md` — The audio capture-to-hub channel is unbounded, so a stalled consumer grows memory instead of reporting back-pressure (loopback.rs:39)
- [minor] `minor-docs-audio-spike-references-a-prd-that-does-not-exist.md` — `audio_spike.rs` prints a banner citing a PRD that is not in the tree, and its findings survive only as a CLAUDE.md sentence (src-tauri/examples/audio_spike.rs:44)
- [speculative] `speculative-audio-win-dtx-off-so-silence-is-fully-encoded-and-sent.md` — Opus DTX is explicitly disabled, so digital silence is encoded and transmitted at 200 packets/second (streamer/audio/encoder.rs:95)

## W9 — Windows utils — `windows_utils/{mod,networking,hosted_network,virtual_display,compatibility}.rs`, `driver_ipc/**`

- [blocker] `blocker-hostednet-win-requires-an-internet-connection-profile.md` — The Windows hosted network cannot start without an internet connection, which is the case it exists for (windows_utils/hosted_network.rs:21)
- [major] `major-compat-win-backends-hardcoded-not-probed.md` — `check_system_requirements` reports hardcoded audio/display backends and never surfaces the encoder tier (compatibility.rs:171)
- [major] `major-compat-win-tethering-probe-checks-type-presence-not-capability.md` — The Windows hotspot compatibility probe tests WinRT type presence, not whether the call will work (windows_utils/compatibility.rs:79)
- [major] `major-hostednet-win-never-uses-mobile-hotspot-no-band-control.md` — The Windows hotspot is Wi-Fi Direct legacy mode, which exposes no band or channel control (windows_utils/hosted_network.rs:38)
- [major] `major-net-hostednetwork-no-verification-that-clients-can-associate.md` — "Hosted network started" is an assertion about our own API call, never about the radio (hosted_network.rs:50)
- [major] `major-net-netsh-process-spawned-every-second.md` — `netsh.exe` is spawned once (sometimes twice) per second for the whole life of the app (networking.rs:107)
- [major] `major-net-wmi-polled-once-per-second-forever.md` — The network watcher runs three WMI queries every second, forever, with no stop and no duplicate guard (networking.rs:214)
- [major] `major-vdisplay-win-decouple-idd-refresh-from-stream-fps.md` — The virtual display's refresh rate is welded to the stream frame rate, so DWM composes at 60 Hz (streamer/server.rs:799)
- [major] `major-vdisplay-win-idd-declares-iddcx-1-2-and-cannot-choose-the-render-adapter.md` — The bundled IDD declares IddCx 1.2, so DWM composes our virtual display on the POST adapter (windows_utils/streamer/pipeline.rs:1360)
- [major] `major-vdisplay-win-mode-change-unplugs-the-monitor.md` — Changing a virtual display's modes hot-unplugs the monitor; IddCxMonitorUpdateModes exists and is unused (windows_utils/virtual_display.rs:57)
- [major] `major-vdisplay-win-setup-fails-silently-when-driver-missing.md` — Virtual display setup fails silently when the driver is absent — `setup` returns false with zero diagnostics (virtual_display.rs:33)
- [major] `major-vdisplay-win-warp-composition-is-silent.md` — If the POST adapter is PnpStopped, Windows composes our virtual display in software and says nothing (windows_utils/streamer/dxgi/mod.rs:73)
- [minor] `minor-compat-win-hosted-network-probe-is-english-only.md` — The hosted-network capability probe parses English-only `netsh` output, so it always says "no" on localized Windows (hosted_network.rs:124)
- [minor] `minor-hostednet-win-unbounded-recv-and-unwrap-panic.md` — `start_hosted_network` can block forever on an unbounded `recv()`, and `stop_hosted_network` panics if `netsh` cannot be spawned (hosted_network.rs:182)
- [minor] `minor-net-get-network-adapters-assumes-com-initialized.md` — `get_network_adapters` calls `COMLibrary::assume_initialized()` and returns an empty list on any failure, silently killing all streamers (networking.rs:64)
- [minor] `minor-session-gpu-toggle-snapshotted.md` — `disableGpuEncode` is copied by value into each streamer's `Config` at creation, so toggling it never affects a running streamer (windows_utils/mod.rs:278)
- [minor] `minor-session-win-settings-change-sleeps-300ms.md` — Changing server ports or the GPU-encode toggle sleeps 300 ms on the command thread and hard-drops every connected device (mod.rs:548)
- [minor] `minor-vdisplay-win-block-on-held-under-std-mutex.md` — Virtual-display create/remove holds a `std::sync::Mutex` across a `RUNTIME.block_on` named-pipe round trip (virtual_display.rs:29)
- [minor] `minor-vdisplay-win-driver-ipc-untagged-decode-failures.md` — Driver-IPC decode failures are silently dropped, and a clean EOF is reported as `last_os_error()` (client.rs:305)
- [minor] `minor-vdisplay-win-install-drivers-command-unwraps-everywhere.md` — `install_drivers` / `remove_drivers` unwrap five fallible calls each and always return `true` (mod.rs:599)
- [minor] `minor-vdisplay-win-setup-races-topology-extend.md` — `setup` fires `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` in a detached task that races virtual-display init, and never checks the result (mod.rs:212)
- [minor] `minor-vdisplay-win-use-smallest-mode-flag-is-unset.md` — The IDD leaves IDDCX_ADAPTER_FLAGS at NONE, so resolution changes may be DWM scaling, not modesets (-)
- [speculative] `speculative-vdisplay-win-refresh-rate-ladder-fixed-at-60-120.md` — Every virtual display is registered with only the requested rate plus 60 and 120 Hz (virtual_display.rs:7)

## W10 — Input — `streamer/input/**`

- [blocker] `blocker-input-cgscroll-variadic-abi-arm64.md` — CGEventCreateScrollWheelEvent is declared variadic one argument too early — scroll deltas are garbage on Apple Silicon (src-tauri/src/streamer/input/macos.rs:33)
- [major] `major-input-batch-replayed-one-syscall-per-sample.md` — Coalesced pointer batches are replayed one OS call per sample — up to 40 SendInput/CGEventPost per message (src-tauri/src/streamer/static/input.js:51)
- [major] `major-input-fast-queue-512-deep-silent-drop.md` — The hot input queue is 512 events deep — it buffers ~2.5 s of stale pointer moves, then drops silently (input/mod.rs:52)
- [major] `major-input-macos-accessibility-has-no-capability-tier.md` — Missing Accessibility permission on macOS degrades to a silent no-op instead of a reported tier (macos.rs:265)
- [major] `major-input-macos-modifier-remap-is-unconditional.md` — macOS injection hard-remaps Ctrl to Command for every client, including Mac clients (macos.rs:172)
- [major] `major-input-pointer-down-uses-stale-cursor-position.md` — POINTER_DOWN carries coordinates the injector throws away — the first tap of a session clicks in the wrong place (windows.rs:159)
- [major] `major-input-synthetic-pointer-device-leak.md` — Windows synthetic pen/touch devices are created per session and never destroyed (windows.rs:139)
- [major] `major-input-two-channel-ordering-race.md` — Pointer moves and button presses travel on two DataChannels with no ordering between them (streamer/static/input.js:493)
- [minor] `minor-input-datachannel-handler-allocates-and-sends-pong-inline.md` — The input DataChannel handler allocates a boxed future per message and awaits an SCTP send inside it (webrtc_session.rs:425)
- [minor] `minor-input-malformed-frames-are-dropped-without-a-trace.md` — A malformed or unknown input frame is discarded with no log, no counter and no client feedback (protocol.rs:235)
- [minor] `minor-input-parse-drop-preallocates-from-an-unvalidated-count.md` — parse_drop preallocates from an unvalidated item count — a 12-byte message allocates ~4.7 MB (protocol.rs:406)
- [minor] `minor-input-pointerlock-ignored-on-windows.md` — Windows ignores POINTERLOCK_STATE entirely; macOS honours it — the two backends disagree (protocol.rs:27)
- [minor] `minor-input-relative-mode-escapes-the-target-display.md` — Relative mouse deltas are unclamped on Windows and clamped to the whole desktop on macOS — a client can drive the host's real monitors (windows.rs:709)
- [minor] `minor-input-stats-and-build-stats-are-dead-code.md` — The input STATS opcode, the Stats counters and fast_queue_depth() are all dead — the input path has zero telemetry (mod.rs:18)
- [minor] `minor-input-tune-process-is-never-called.md` — input::tune_process() is dead — timeBeginPeriod(1) and HIGH_PRIORITY_CLASS are never applied (input/mod.rs:43)
- [minor] `minor-ui-input-silently-drops-forwarded-ref.md` — `<Input>` is wrapped in `forwardRef` but never forwards the ref (src/components/ui/input.tsx:8)

## W11 — Client page — `streamer/static/**`

- [major] `major-client-audiocontext-resume-outside-user-gesture.md` — `AudioContext.resume()` is called after an `await`, so on iOS/Safari the context can stay suspended for the whole session with no unmute UI (index.html:1385)
- [major] `major-client-audiodecoder-opus-support-never-verified.md` — The client picks the WebCodecs path from constructor existence alone; Safari 26 now ships `AudioDecoder`, and a codec rejection is permanent silence with no fallback (audio.js:13)
- [major] `major-client-coalesced-events-only-used-for-pen.md` — `getCoalescedEvents()` is used only for pen, so touch and high-rate mice are decimated to one sample per task (input.js:299)
- [major] `major-client-css-inset-breaks-older-webkit.md` — The client page's layout depends on CSS `inset`, which older mobile WebKit ignores — the whole page collapses (styles.css:14)
- [major] `major-client-decodequeuesize-is-the-wrong-backpressure-signal.md` — The WebCodecs client throttles on `decodeQueueSize`, which measures acceptance, not completion (transform-worker.js:229)
- [major] `major-client-fullscreen-catch-on-undefined.md` — `enterFullscreen(...).catch()` throws wherever unprefixed `requestFullscreen` is missing, silently killing pointer + keyboard lock (index.html:498)
- [major] `major-client-isconfigsupported-probes-a-different-config.md` — The client probes one `VideoDecoder` config and then configures a different one (transform-worker.js:141)
- [major] `major-client-join-blocks-on-ice-gathering.md` — Join blocks on full ICE gathering (no trickle) — up to 3 s on LAN, 6 s with TURN (index.html:948)
- [major] `major-client-mic-prompt-gates-the-stream.md` — The microphone permission prompt is on the critical path of joining — up to 12 s before WebRTC even starts (index.html:1354)
- [major] `major-client-no-audio-fallback-when-prepare-fails.md` — If `SEAudio.prepare()` fails, the client gets no audio at all — the Opus-track fallback was never negotiated (index.html:170)
- [major] `major-client-no-keyframe-request-on-chrome.md` — WebCodecs client cannot request a keyframe on Chrome/Edge — any decode hiccup freezes the picture for ~4 s (index.html:166)
- [major] `major-client-one-lost-packet-costs-a-full-jitter-buffer.md` — One lost 5 ms audio packet costs a full jitter buffer of silence, because any underrun re-primes the worklet (streamer/webrtc_session.rs:676)
- [major] `major-client-outputlatency-absent-breaks-avsync-on-safari-and-bluetooth.md` — A/V sync treats a missing `outputLatency` as zero, so audio plays early on Safari < 18.4 and by 100–200 ms on any Bluetooth sink (audio.js:426)
- [major] `major-client-pointer-coords-ignore-letterboxing.md` — Absolute pointer coordinates are computed against the stage, not the letterboxed picture — touch is offset in non-fullscreen mode (input.js:100)
- [major] `major-client-pointerlock-missing-unadjustedmovement.md` — Pointer lock is requested without `unadjustedMovement`, so mouse acceleration is applied twice (index.html:531)
- [major] `major-client-refresh-rate-renegotiates-right-after-live.md` — Every high-refresh client tears down and rebuilds the whole session ~1 s after it goes live (index.html:1363)
- [major] `major-client-video-fallback-jitter-buffer.md` — The `<video>` fallback path reintroduces the WebRTC jitter buffer (~90 ms) and nothing reports it (index.html:166)
- [minor] `minor-client-audio-flags-byte-ignored.md` — The client never reads the audio packet flags byte — `FLAG_SILENT` and `FLAG_DISCONTINUITY` are dead on the wire (src-tauri/src/streamer/audio/protocol.rs:3)
- [minor] `minor-client-audio-ring-abi-duplicated-in-three-files.md` — The planar audio ring ABI is hand-duplicated across three files with no shared definition or assertion (audio.js:478)
- [minor] `minor-client-avsync-mixes-min-tracker-with-mean-estimator.md` — A/V sync mixes a min-tracking EMA for video with a mean EMA for audio, biasing the commanded buffer depth (transform-worker.js:68)
- [minor] `minor-client-canvas-size-latched-on-first-frame.md` — The canvas is sized once from the first decoded frame and never resized — a mid-stream resolution change is rescaled, not re-fitted (transform-worker.js:107)
- [minor] `minor-client-dead-code-and-unused-constants.md` — Dead code in the client page: unused constants, an unread RTT, an unwritten settings key, an unsent opcode (index.html:488)
- [minor] `minor-client-file-drop-exceeds-sctp-message-limit.md` — Dropped files are sent as one SCTP message and silently vanish above the max message size (input.js:215)
- [minor] `minor-client-four-overlapping-polling-loops.md` — Four independent polling loops run on the client main thread for the whole session (index.html:315)
- [minor] `minor-client-https-modal-typo-and-arrow.md` — User-facing typo and raw ASCII arrow in the HTTPS upsell modal (index.html:105)
- [minor] `minor-client-ime-sink-never-focused-on-touch-laptops.md` — IME/composition input is disabled on any device reporting touch points, including touchscreen laptops (input.js:501)
- [minor] `minor-client-no-scheduling-invariant.md` — Make "the client never schedules, it resynchronises" an explicit, asserted invariant (transform-worker.js:230)
- [minor] `minor-client-nosleep-plays-unmuted-video.md` — On browsers without the Wake Lock API, NoSleep plays an unmuted looping video alongside the stream (audio.js:223)
- [minor] `minor-client-nosleep-unguarded-constructor.md` — `new NoSleep()` runs unguarded at inline-script top level — one failed script load bricks the whole page (index.html:490)
- [minor] `minor-client-postmessage-audio-ring-crosses-main-thread.md` — Without SharedArrayBuffer, every 5 ms audio packet makes two hops through the client main thread (audio.js:23)
- [minor] `minor-client-safari-before-26-has-no-audiodecoder.md` — Every Safari before 26 silently gets the slow audio path, and the compatibility modal blames the browser (index.html:168)
- [minor] `minor-client-selectaudiooutput-never-used.md` — `selectAudioOutput()` is probed and discarded — the one permission-free output picker goes unused (audio.js:104)
- [minor] `minor-client-static-assets-have-no-cache-headers.md` — The client's static assets are served with no cache headers — every reload refetches ~120 KB (src-tauri/src/streamer/server.rs:408)
- [speculative] `speculative-client-desynchronized-is-inert-on-offscreencanvas.md` — `desynchronized: true` does nothing on the client's OffscreenCanvas (transform-worker.js:39)
- [speculative] `speculative-client-jitter-buffer-target-diverges-between-paths.md` — The two client render paths disagree on jitter-buffer target, and the history shows why by accident (index.html:832)

## W12 — Desktop UI — `src/**`, `scripts/check-legacy-webkit.mjs`, vite/tailwind config

- [major] `major-build-legacy-webkit-checker-blind-spots.md` — The legacy-WebKit checker never looks at CSS, HTML, subdirectories, or the shims it claims cover things (scripts/check-legacy-webkit.mjs:56)
- [major] `major-build-single-eager-startup-chunk.md` — The desktop UI ships as one 914 KB eager chunk with no manual chunking (vite.config.ts:87)
- [major] `major-build-vite-worker-format-unpinned.md` — Production Monaco workers rely on an unpinned Vite default; a module worker would silently break the settings editor on Safari 13.1 (vite.config.ts:19)
- [major] `major-ui-config-store-n-plus-one-reads.md` — `getConfig()` costs 15 sequential Tauri IPC round-trips and runs ~5 times before the dashboard paints (src/components/config-provider.tsx:122)
- [major] `major-ui-focus-visible-dropped-on-safari-13.md` — Every keyboard focus ring in the UI kit is invisible on the macOS 10.15 floor (`:focus-visible` is Safari 15.4) (src/components/ui/button.tsx:8)
- [major] `major-ui-global-context-single-object.md` — One context object holds all 11 pieces of global state, so any change re-renders every consumer (src/App.tsx:377)
- [major] `major-ui-global-transition-all-important.md` — A global `transition: all 200ms !important` on every button and input defeats every transition utility (src/index.css:95)
- [major] `major-ui-logline-unbatched-unvirtualized.md` — The log terminal copies and re-renders up to 2000 lines per `LogLine` event (src/components/log-terminal.tsx:35)
- [major] `major-ui-monaco-preload-during-startup.md` — Monaco is preloaded — and a throwaway editor instantiated — 300 ms into startup, on the main thread (src/pages/bootstrap.tsx:286)
- [major] `major-ui-settings-persists-every-keystroke.md` — Settings writes the hosted-network credentials to the config store on every keystroke (src/pages/settings.tsx:537)
- [major] `major-ui-updater-check-blocks-startup.md` — The startup path awaits a network updater check before it even tries to create the display (src/pages/bootstrap.tsx:274)
- [major] `major-ui-usetoast-rerenders-every-consumer.md` — Every `useToast()` consumer re-renders on every toast, and re-subscribes on every toast (src/components/ui/use-toast.ts:170)
- [minor] `minor-build-index-html-blocks-modifier-clicks-globally.md` — The global chrome-suppression script in `index.html` preventDefaults every modifier-click, breaking Monaco multi-cursor and shift-selection (index.html:18)
- [minor] `minor-build-startup-theme-and-background-flash.md` — Startup shows a flash of the wrong theme: the inline bootstrap script ignores the persisted choice, and nothing paints a background before CSS arrives (index.html:8)
- [minor] `minor-build-tailwind-content-globs-and-eslint-scope-gaps.md` — Three of four Tailwind content globs point at directories that do not exist, and lint covers only `src/` (tailwind.config.js:4)
- [minor] `minor-build-vite-config-fragile-and-underspecified.md` — `vite.config.ts` throws on a renamed file, leaves chunk-size and sourcemaps unset, and hides a full nested build in a dev middleware (vite.config.ts:8)
- [minor] `minor-ui-aspect-ratio-and-logical-inset-above-the-floor.md` — `aspect-ratio` and `inset-inline-start` also die on Safari 13.1 (plus the audited-clean list) (src/components/ui/avatar.tsx:27)
- [minor] `minor-ui-avatar-loaded-as-number-array-over-ipc.md` — The avatar crosses the IPC boundary as a JSON array of numbers, then a FileReader, on every launch (src/lib/bindings.ts:50)
- [minor] `minor-ui-bootstrap-serial-device-replay.md` — Startup replays every saved device and known device to the backend one serialized IPC at a time (src/pages/bootstrap.tsx:105)
- [minor] `minor-ui-dead-components-and-unused-dependencies.md` — Two UI-kit components and six runtime dependencies are unreachable from `src/` (form.tsx:11)
- [minor] `minor-ui-dead-helpers-and-globals-in-utils.md` — Dead exported helpers, unassigned window globals and a commented-out component in `src/` (src/lib/utils.ts:108)
- [minor] `minor-ui-device-events-trigger-extra-ipc.md` — Every device-array identity change fires two extra IPC bursts (`get_display_capacity` + a full config read) (src/pages/dashboard.tsx:117)
- [minor] `minor-ui-device-row-mounts-full-editor.md` — Every device row mounts a whole formik form, a Sheet and three dialogs even while closed (src/pages/devices.tsx:159)
- [minor] `minor-ui-font-fallback-is-serif-and-unpreloaded.md` — The webfont has a `serif` fallback and no preload, so first paint is Times and then reflows (src/index.css:55)
- [minor] `minor-ui-getconfig-publishes-unconditionally.md` — Every `getConfig()` read publishes a brand-new snapshot, forcing a global re-render even when nothing changed (src/components/config-provider.tsx:110)
- [minor] `minor-ui-i18n-allocates-per-render.md` — `useTranslation()` allocates a new object and a new locale array on every render, at 15 call sites (src/i18n/index.ts:81)
- [minor] `minor-ui-json-editor-worker-keepalive-interval.md` — Each mounted JSON editor runs a 60-second worker keep-alive interval, and up to three can be mounted (src/components/json-editor.tsx:47)
- [minor] `minor-ui-kit-upstream-defects-carried-verbatim.md` — Four upstream shadcn defects were carried into the kit verbatim and are still live (src/components/ui/form.tsx:42)
- [minor] `minor-ui-legacy-polyfills-runtime-cost.md` — The legacy WebKit polyfills patch `window.matchMedia` on every host, including modern WebView2 (main.tsx:1)
- [minor] `minor-ui-permission-poll-200ms.md` — The bootstrap permission gate polls `check_permissions` every 200 ms with no backoff (src/pages/bootstrap.tsx:29)
- [minor] `minor-ui-qrcode-rendered-at-500px.md` — Each dashboard QR code renders as a 500-unit SVG and re-renders whenever `qrValues` is reallocated (src/pages/dashboard.tsx:252)
- [minor] `minor-ui-runtime-panics-nonnull-assertions.md` — Two non-null assertions on DOM lookups can throw at runtime and take down the app (src/main.tsx:10)
- [minor] `minor-ui-safari13-has-selector-in-table.md` — `:has()` in the table styles is silently dropped on the Safari 13.1 floor (src/components/ui/table.tsx:76)
- [minor] `minor-ui-sidebar-animates-layout-properties.md` — The sidebar collapse animates `width` (a layout property) for 300–500 ms with no `will-change` (src/layout/layout.tsx:18)
- [minor] `minor-ui-swallowed-errors-and-silent-fallbacks.md` — Fifteen places in the React app discard an error or silently degrade (src/pages/bootstrap.tsx:215)
- [minor] `minor-ui-tailwind-preflight-where-rules-die-on-safari-13.md` — Tailwind preflight uses `:where()` (Safari 14), so four reset rules are dropped whole on the 10.15 floor (src/components/ui/input.tsx:25)
- [minor] `minor-ui-tooltip-tailwind-v4-syntax-dead-classes.md` — `tooltip.tsx` uses Tailwind v4 class syntax under Tailwind v3, so five arrow-positioning classes generate no CSS (package.json:79)
- [minor] `minor-ui-walkthrough-raf-loop-and-nextstep-always-mounted.md` — The walkthrough provider wraps the whole router unconditionally, and its highlight runs a `requestAnimationFrame` loop (src/App.tsx:391)
- [minor] `minor-ui-window-commands-global-side-effect.md` — `src/lib/utils.ts` assigns the entire Tauri command surface to `window.commands` at module scope (src/lib/utils.ts:23)
- [minor] `minor-ui-zoom-persists-and-reflows-per-step.md` — Every zoom step issues an IPC, a store write, a whole-window reflow and a walkthrough teardown (src/App.tsx:110)
- [speculative] `speculative-ui-base-ui-migration-drags-post-floor-apis.md` — The half-finished Radix → Base UI migration will drag `inert`, `checkVisibility` and `CSS.registerProperty` past the Safari 13.1 floor (src/components/ui/tooltip.tsx:3)
- [speculative] `speculative-ui-requestidlecallback-unavailable-on-floor.md` — Deferring startup work correctly needs `requestIdleCallback`, which does not exist on the Safari 13.1 floor (src/lib/preload-editor.ts:40)
- [speculative] `speculative-ui-settings-monolith-rerenders.md` — The 1600-line Settings page re-renders its whole tree on every keystroke in any of its 11 inputs (src/pages/settings.tsx:86)

## W13 — App shell / CLI / build / docs / hygiene

- [blocker] `blocker-build-gitignore-excludes-the-entire-docs-directory.md` — `.gitignore` excludes all of `docs/` as "Temporary agent files" — no document there can ever be committed (CLAUDE.md:373)
- [major] `major-build-bundled-idd-driver-provenance-is-wrong.md` — PROVENANCE.md misdescribes the bundled display driver: it is a locally built, rebranded fork (src-tauri/resources/PROVENANCE.md:10)
- [major] `major-build-ci-deleted-docs-still-claim-it.md` — CI was deleted in `e1d5d28`; nothing runs fmt/clippy/test/lint/audit/checksums, yet README and both PROVENANCE files still claim it does (README.md:127)
- [major] `major-build-darwin-stub-checksum-mismatch.md` — `binaries/SHA256SUMS` pins the CRLF (Windows-checkout) hash of the macOS `nefconc` stubs — the checksum check fails on any LF checkout (binaries/PROVENANCE.md:29)
- [major] `major-build-mac-app-forces-the-discrete-gpu-for-its-whole-lifetime.md` — Info.plist lacks NSSupportsAutomaticGraphicsSwitching, so ScreenExtend pins the discrete GPU on for its whole run (-)
- [major] `major-build-mac-release-must-be-developer-id-signed-with-stable-designated-requirement.md` — A drifting or ad-hoc macOS code identity makes the Screen Recording toggle show ON while the running binary is a different TCC subject (-)
- [major] `major-build-recovered-docs-live-only-in-dangling-objects.md` — Everything recovered in this discovery lives in dangling objects that the next `git gc` deletes (src-tauri/src/macos_utils/airplay/receiver.rs:301)
- [major] `major-cli-disconnect-grace-split-brain.md` — Disconnect grace is persisted in `localStorage` by the GUI but read from `config.json` by the CLI — `serve` silently ignores the user's setting (src-tauri/src/cli.rs:480)
- [major] `major-cli-doctor-omits-backend-tiers.md` — `ScreenExtend doctor` never reports the chosen audio/display backend tier, even though `CompatibilityReport` already carries both (src-tauri/src/lib.rs:103)
- [major] `major-cli-doctor-omits-tcc-signing-and-translocation-state.md` — `ScreenExtend doctor` prints two permission booleans and none of the state that explains why they are wrong (cli.rs:1386)
- [major] `major-cli-logbus-emits-ipc-event-per-line.md` — Every `tprintln!` serializes a Tauri IPC event and takes three global mutexes, whether or not the log view is mounted — 429 call sites, some on per-frame paths (src-tauri/src/logbus.rs:40)
- [major] `major-cli-serve-drops-device-trust.md` — `ScreenExtend serve` replays only display overrides — device bans, approvals and audio-output choices are silently dropped (src-tauri/src/cli.rs:524)
- [major] `major-docs-audio-prd-and-spike-notes-deleted.md` — The audio PRD and its measured spike results were deleted; 35 comments still cite their sections (src-tauri/src/windows_utils/audio/loopback.rs:362)
- [minor] `minor-build-http-plugin-registered-but-unreachable.md` — `tauri-plugin-http` is compiled in and initialized but has no capability grant and no JS counterpart — it is dead weight (Cargo.toml:40)
- [minor] `minor-build-mac-clippy-warnings-fail-gate.md` — macOS tree: two rustc warnings in macos_utils/audio/mod.rs break the `clippy -D warnings` gate (macos_utils/audio/mod.rs:271)
- [minor] `minor-build-mac-info-plist-is-never-referenced-by-either-tauri-config.md` — `src-tauri/Info.plist` is not referenced by either Tauri config — verify it actually reaches the bundle (tauri.macos.conf.json:36)
- [minor] `minor-build-macos-config-disables-csp.md` — The macOS build overlay sets `app.security.csp: null`, disabling the webview Content-Security-Policy that the Windows build enforces (src-tauri/tauri.conf.json:15)
- [minor] `minor-build-resources-sha256sums-crlf.md` — `src-tauri/resources/SHA256SUMS` checks out with CRLF, so `sha256sum -c` fails on a Windows/Git-Bash checkout (resources/PROVENANCE.md:3)
- [minor] `minor-build-unused-and-misplaced-dependencies.md` — Unused and misplaced Cargo dependencies: `clap` has zero call sites, `wmi` is declared for every platform but only used on Windows, `crossbeam-channel` is declared twice (Cargo.toml:141)
- [minor] `minor-cli-devices-set-does-not-apply-live.md` — `ScreenExtend devices set` writes config but never pushes it to a running host, unlike the GUI which applies the same change live (src-tauri/src/cli.rs:842)
- [minor] `minor-cli-fastpath-hijacks-h-and-v-anywhere.md` — `cli::fast_path` treats `-h`/`--help`/`-V`/`--version` appearing anywhere in argv as the whole command, so they cannot be used as argument values (src-tauri/src/cli.rs:142)
- [minor] `minor-cli-legacy-subcommand-branches-panic.md` — The legacy `installdrivers` / `removedrivers` / `hostednetwork` branches in `lib.rs::setup` are a wall of `.unwrap()`s on shell output and resource paths (lib.rs:446)
- [minor] `minor-cli-logs-command-is-always-empty.md` — `ScreenExtend logs` can never print anything — the backlog lives in the host process, and `logs` runs in a new one (src-tauri/src/cli.rs:1438)
- [minor] `minor-cli-serve-leaks-otp-via-argv-and-omits-https.md` — `serve --otp` puts the join credential in argv (shell history, `ps`, Task Manager) and the serve banner never offers the HTTPS endpoint (src-tauri/src/cli.rs:486)
- [minor] `minor-cli-single-instance-lock-and-token-issues.md` — `single_instance`: a failed lock-file open is indistinguishable from "already running", the control token is compared non-constant-time, and `host_running()` steals the lock to test it (single_instance.rs:19)
- [minor] `minor-docs-claude-md-drifted-from-code.md` — `CLAUDE.md` has drifted from the tree: wrong version, an incomplete command list, a Linux claim that is false, and undocumented directories (package.json:4)
- [minor] `minor-docs-readme-stale-claims.md` — `README.md` carries several claims the tree contradicts (CI, macOS audio driver, nefconc on macOS, build output path) (README.md:127)
- [minor] `minor-docs-stale-reference-sweep-list.md` — Sweep list: every reference in the tree to a document, milestone or tool that does not exist (src-tauri/src/windows_utils/audio/loopback.rs:362)
- [speculative] `speculative-build-bindings-export-guard-can-silently-revert-api.md` — The `bindings.ts` export guard writes the generated file from whichever debug binary happens to run with no arguments — a stale binary silently reverts the whole TS API surface (src-tauri/src/lib.rs:398)

## W14 — AirPlay tier — `macos_utils/airplay/**`

- [blocker] `blocker-airplay-mirror-sink-serves-one-connection.md` — The AirPlay mirror data port serves exactly one connection, so a second session has nowhere to land (mirror.rs:78)
- [major] `major-airplay-ax-children-leaks-a-cfarray-per-call.md` — `ax::children()` leaks a CFArray on every call, and it is called in a 25 ms poll loop (ax.rs:296)
- [major] `major-airplay-bonjour-auto-rename-flag-is-zero.md` — `KDNS_SERVICE_FLAGS_NO_AUTO_RENAME` is defined as 0x0, so mDNSResponder silently renames the receiver (dnssd.rs:52)
- [major] `major-airplay-join-costs-five-serial-windowserver-reconfigs.md` — An AirPlay-tier join performs 4-5 serial WindowServer reconfigurations under the global display lock (mod.rs:238)
- [major] `major-airplay-pgrep-fork-storm-in-menu-extra-retry.md` — Waiting for the Displays menu extra forks `/usr/bin/pgrep` and rebuilds the AX tree every 25 ms for up to 5 s (ax.rs:452)
- [major] `major-airplay-server-set-display-mode-fights-the-ladder.md` — After the AirPlay backend picks a ladder mode, `server.rs` immediately asks for a size that is not on the ladder, via the exact API `topology.rs` was written to avoid (sender_prefs.rs:32)
- [minor] `minor-airplay-availability-check-is-incomplete-and-ignored.md` — `availability()` checks one of the two requirements its doc names, and `new_shared()` installs the controller regardless (mod.rs:202)
- [minor] `minor-airplay-bonjour-interface-is-the-first-match-not-the-active-one.md` — The Bonjour interface is whichever `getifaddrs` returns first, with no route or metric preference and no vmnet/bridge exclusion (dnssd.rs:70)
- [minor] `minor-airplay-close-menu-injects-a-global-escape-keystroke.md` — `close_menu` falls back to posting a synthetic Escape into the HID event tap, which reaches whatever the user is typing into (ax.rs:608)
- [minor] `minor-airplay-dead-code-inventory.md` — Ten public items in the AirPlay module have no caller anywhere in the tree (mod.rs:492)
- [minor] `minor-airplay-fairplay-sequence-dispatch-is-dead.md` — `fairplay::respond` claims to dispatch on the sequence byte but the or-pattern makes that arm unreachable (fairplay.rs:16)
- [minor] `minor-airplay-log-strings-contain-collapsed-whitespace-runs.md` — 13 log and error strings contain 6-18 space runs where a line continuation was intended (mod.rs:181)
- [minor] `minor-airplay-menu-bar-preference-is-never-restored.md` — The AirPlay tier permanently turns on a user preference it never restores, against the principle the sibling module states (ax.rs:137)
- [minor] `minor-airplay-pairing-and-model-are-compile-time-constants-not-probes.md` — Legacy pairing, model and `srcvers` are compile-time constants, so there is no tier below "the sender demanded pairing" (receiver.rs:40)
- [minor] `minor-airplay-phase-latches-to-ended-so-diagnose-misreports.md` — The receiver's `Phase` latches at `Ended` after the first session, so the second session's failure diagnosis is wrong (receiver.rs:362)
- [minor] `minor-airplay-rtsp-head-scan-is-quadratic-and-copies-per-request.md` — The hand-rolled RTSP reader rescans the whole buffer for CRLFCRLF on every read, and copies the head and body per request (rtsp.rs:144)
- [minor] `minor-airplay-teardown-drives-the-menu-bar-during-app-shutdown.md` — Shutdown drives the user's menu bar and can block for tens of seconds on a thread with no runtime (mod.rs:447)
- [speculative] `speculative-airplay-decoding-the-mirror-stream-needs-real-fairplay.md` — Using the AirPlay mirror stream as a frame source would require real FairPlay key recovery, which the static tables deliberately do not provide (mirror.rs:16)
- [speculative] `speculative-airplay-dnssd-ffi-is-reusable-for-an-awdl-transport.md` — The `dns_sd.h` FFI is 90% of what an AWDL/peer-to-peer transport experiment needs, but it is missing the two flags that actually enable AWDL (dnssd.rs:22)
- [speculative] `speculative-airplay-relay-rejected.md` — Relaying macOS's own AirPlay mirror stream instead of capturing and encoding: investigated, rejected (mirror.rs:1)
- [speculative] `speculative-airplay-sender-timestamps-are-received-and-discarded.md` — macOS already sends us a per-frame capture timestamp over the mirror stream and we throw it away — free one-way-delay ground truth (mirror.rs:1)

## W15 — Tests

- [major] `major-build-capture-subsystems-have-no-automated-test.md` — The entire capture half of the pipeline has no automated test on either OS (windows_utils/streamer/test/mod.rs:7)
- [major] `major-client-web-page-has-no-test-infrastructure-at-all.md` — The client web page and the desktop UI have zero tests and no test runner (protocol.rs:57)
- [major] `major-nvenc-layout-tests-without-functional-tests.md` — NVENC has 6167 lines of struct-layout assertions and not one test that encodes a frame (nvenc_layout.rs:5684)
- [minor] `minor-audio-mac-native-tiers-untested-while-the-dead-tier-is-well-tested.md` — macOS audio coverage is inverted: the legacy tier has 20 tests, the two native tiers have none (-)
- [minor] `minor-build-inventory-of-silently-passing-tests.md` — Inventory: every test that can return green having verified nothing (windows_utils/streamer/test/x264_encoder.rs:59)
- [minor] `minor-tests-audio-seqgate-tests-a-reimplementation.md` — The five `seqgate_*` tests exercise a copy of the client's dedup logic, and the copy has drifted (streamer/audio/tests.rs:74)
- [minor] `minor-webrtc-session-cloud-tls-have-no-tests.md` — The WebRTC negotiation, cloud relay, and TLS modules have no tests at all (-)
- [speculative] `speculative-build-ffi-layout-test-method.md` — How the two existing FFI layout tests establish truth, and what that implies for amd_layout / VideoToolbox / CoreAudio / CoreWLAN (-)
- [speculative] `speculative-build-source-scraping-tests-in-ax-rs.md` — Two tests assert on the text of their own source file with `include_str!` (-)

---

## Cross-workstream dependencies

**W2 gates almost everything.** There is currently no host-side per-stage latency number, no
delivered-frame-rate number, and the one client-side latency figure is off by ~1.75e12 because
`performance.timeOrigin` is a Unix epoch while `HOST_EPOCH` is a process-relative monotonic
`Instant`. W3 and W5 must both adopt W2's stage-timing API — the same six stamps
(`t_frame`, `t_callback`, `t_staged`, `t_submit`, `t_bitstream`, `t_broadcast`) with p50/p95/p99 per
adjacent difference — or their before/after numbers will not be comparable with each other or with
the client's. Concretely: `major-pipeline-win-one-aggregate-timer-and-no-stage-breakdown` is the
Windows half, `major-webrtc-no-per-stage-encode-latency-metric` is the transport half,
`major-cgds-drop-count-never-read` and `major-vt-encoder-implementation-is-never-read-back-...` are
the macOS half, and `major-client-no-glass-to-glass-telemetry` supplies the client half plus the
clock-offset exchange that makes the host and client numbers addable. **None of the encoder or
capture recommendations in W3, W4 or W5 should be attempted before their half of that instrument
exists.** The single highest-information-per-minute item in the whole set is the macOS `EncoderID` +
submit→emit dwell log: it distinguishes a one-frame encoder from a sixteen-frame one and re-orders
every other macOS recommendation.

**W2 also owns the reporting *channel*, and W11 owns its client end.** The `STATS` opcode `0x62` is
reserved in `streamer/input/protocol.rs:31` and never sent; the client-side round-trip time is
computed every second in `input.js:253-273` and read by nobody. W11 must not invent a second
telemetry path — the opcode and its payload shape are defined by W2, W11 implements the sender. And
`major-client-jitter-buffer-levers-are-inert-on-the-webcodecs-path` constrains what can be measured
at all: every receive-side timing counter in `getStats()` is populated from libwebrtc's frame buffer,
which the WebCodecs path bypasses entirely (`RtpVideoStreamReceiver2::OnAssembledFrame` hands the
frame to the transform *instead of* the reference finder), so `jitterBufferDelay`,
`totalProcessingDelay` and `totalAssemblyTime` read zero on the path that matters and are usable only
on the `<video>` fallback.

**W4 depends on W3 twice over.** The AMF backend cannot be fed until `scaler.rs` can emit NV12
(`major-amd-scaler-needs-nv12-output`), which is a W3 change to a W3 file — and the same NV12 output
is what would let W3 delete the scalar BGRA→I420 convert in x264 and shrink the CPU-bridge readback
by 38 %. `major-pipeline-win-adapter-selection-has-no-runtime-policy` must land before W4 so the AMF
device and the capture device are chosen by one policy rather than three, and
`major-pipeline-win-cpu-bridge-readback-stalls-encode-thread` is the fallback W4 exists to stop
being reached. The FFI layout-test method (`speculative-build-ffi-layout-test-method`, W15, plus
`major-amd-amf-layout-test-harness` in W4) should be settled *before* `amd_sys` bindings are
generated: AMF is bound through COM-like vtables where a wrong slot index is not a compile error and
not a runtime error message, but a call through the wrong function pointer.

**W3 and W5 share three cross-platform contracts that must not diverge.** (1) The `EncoderBackend` /
`BackendConfig` trait in `streamer/platform.rs` — the minimum-encode-size clamp
(`major-vt-intel-macs-refuse-hardware-h264-below-640-wide-and-480-tall`) belongs there, not in the
macOS file. (2) The fps floor: `refresh.clamp(60.min(max_fps), max_fps)` is byte-identical in
`macos_utils/streamer/config.rs:30-36` and `windows_utils/streamer/pipeline.rs:94-99`, and
`windows_utils/streamer/test/pipeline.rs:62` asserts the current wrong behaviour, so the test must
move with the fix. (3) Slices per frame / max slice bytes: NVENC is single-slice, Intel uses a fixed
height heuristic, x264 sets nothing, VideoToolbox has no rolling intra refresh, and `amd.md`
prescribes a slice count that buys nothing without `OUTPUT_MODE = SLICE`. If all four are touched
this becomes one concept belonging in `streamer/config.rs` next to `H264Profile`, as Sunshine models
it.

**W1 owns the H.264 profile *and* level negotiation, and every encoder consumes the result.**
`major-webrtc-h264-profile-hardcoded-baseline-never-negotiated` (which now also carries the level
defect — every `profile-level-id` ends in `1f`, i.e. level 3.1 / 720p30, on sessions that are
routinely 1440p) must be parsed once in `webrtc_session.rs` and passed down; W3, W4 and W5 must not
each sniff the SDP. Similarly, `major-webrtc-session-locality-is-computed-then-thrown-away` is the
single producer of a signal that at least seven other findings consume — RTP MTU on relayed paths,
playout-delay bounds, STUN suppression, both gather timeouts, pacing on/off, FEC on/off and the
USB-tether tier. Build it once, in W1, before any of them.

**W1 and W5 must coordinate on the broadcast capacity and the `Lagged`→IDR response.** The consumer
is one file (`webrtc_session.rs:550-553`) but the capacity constant exists twice
(`windows_utils/streamer/pipeline.rs:43` and `macos_utils/streamer/pipeline.rs:27`), and raising one
without the other produces two different failure profiles on two platforms.

**W6 and W9 are the same feature on two OSes and will diverge if worked separately.** The hosted
network, the network-adapter watcher, the compatibility report and the `set_server_ports` 300 ms
sleep all exist as near-verbatim per-OS copies.
`major-net-hostednetwork-no-verification-that-clients-can-associate` proposes one
`HostedNetworkState` tri-state for both; agree that type before either side starts.
`major-net-direct-link-lowlatency-mode` (W6) and
`major-hostednet-win-never-uses-mobile-hotspot-no-band-control` (W9) are the two halves of one
recommendation and must be measured with one protocol.

**W9 blocks itself on a driver rebuild that W13 has to make reproducible first.** Four W9 findings
(`major-vdisplay-win-idd-declares-iddcx-1-2-...`, `major-vdisplay-win-mode-change-unplugs-the-monitor`,
`major-vdisplay-win-warp-composition-is-silent`, `minor-vdisplay-win-use-smallest-mode-flag-is-unset`)
all require rebuilding the bundled IDD, and `major-build-bundled-idd-driver-provenance-is-wrong`
(W13) establishes that the shipped DLL is a locally built fork whose provenance file is wrong and
which nobody but the original dev box can currently reproduce. Land the provenance work first.

**W7 and W8 share `streamer/audio/` and must not both edit it.** The `AudioHub`, the bridge thread,
the Opus encoder wrapper, `AudioDiagnostics` and the 13-byte protocol header are W8's territory but
are consumed by W7's backends. The `capture_ns` defect exists on both platforms with *contradictory*
write-ups — `major-audio-mac-capture-ns-is-drain-time` says Windows has the identical pattern, while
`major-audio-win-capture-timestamp-taken-at-encode-time` says macOS already does it correctly. Both
files are deliberately kept and cross-linked; **reconcile them before either is fixed**, because the
sample-counter anchor they both propose is one shared helper in `streamer/audio/`.

**W11's audio findings depend on W8's wire format.** `minor-client-audio-flags-byte-ignored`,
`major-client-one-lost-packet-costs-a-full-jitter-buffer` and
`major-audio-core-broadcast-lag-swallowed-without-discontinuity` are one defect seen from three
places: the host never sets `FLAG_DISCONTINUITY` on a producer-side skip, and the client never reads
byte 12 at all. Fixing either half alone changes nothing.

**W12 must not change the desktop UI's appearance**, which is a hard rule of the engagement. Three
W12 findings are appearance-adjacent and need explicit sign-off before they land:
`minor-ui-font-fallback-is-serif-and-unpreloaded` (changes the fallback stack and adds a monospace
stack for `pre`/`code`), `major-ui-global-transition-all-important` (removes a 200 ms `transition:
all !important` from every button and input) and `major-ui-focus-visible-dropped-on-safari-13` (adds
a visible focus ring on the 10.15 floor, where today there is none at all). All three are defect
repairs rather than redesigns, but all three are visible.

**W13's `.gitignore` fix gates the whole engagement's paper trail.** Until
`blocker-build-gitignore-excludes-the-entire-docs-directory` is committed, every `git add docs/…`
silently no-ops, and `major-build-recovered-docs-live-only-in-dangling-objects` warns that the
recovered material — including the only copy of the AirPlay design document and of the Monaco preload
measurements — is one `git gc` from gone. Do this before anything else in W13.

**W15 cannot certify anything until W13 restores a gate.** `major-build-ci-deleted-docs-still-claim-it`
establishes that CI was added and removed 4m13s apart and has never run on a single pull request, so
every W15 test addition is currently unenforced. The dependency runs the other way too:
`blocker-build-cargo-test-red-on-windows` (W2) must be fixed before `cargo test` can be the gate at
all.

**W14 is self-contained and lowest priority.** The AirPlay tier sits below the 10.15 supported floor
and is never selected while `CGVirtualDisplay` exists (10.14.4+). Its only cross-workstream ties are
the repo-wide string-continuation hygiene sweep (W13) and
`speculative-build-source-scraping-tests-in-ax-rs` (W15).

---

## Top 25 by expected latency impact

Ranked by expected effect on glass-to-glass or time-to-first-frame, weighted by how many hosts take
the path. Pure instrumentation is excluded from the ranking and discussed underneath, because its
value is that it makes the other 25 measurable, not that it removes milliseconds.

1. **`blocker-wgc-min-update-interval-forces-dxgi`** (W3) — every Windows 10 host and every Windows 11
   before 24H2 fails the WGC start on an *optional* setting and silently falls to DXGI duplication,
   which never builds the NVENC ring (capture and encode serialise on one thread) and adds one to two
   full-frame GPU copies per frame. The largest population-weighted regression in the set, and the fix
   is downgrade-instead-of-fail in a file we own.
2. **`blocker-nvenc-ring-ignores-idr-on-static-desktop`** (W3) — the default path on a modern NVIDIA
   host cannot emit a keyframe while the desktop is still, and the GOP is infinite. A client that
   joins, or that loses a packet, against a static screen stays black until something moves.
   Unbounded, not incremental.
3. **`major-webrtc-no-send-pacer`** (W1) — 30–60 back-to-back RTP writes per frame into a single
   802.11 TXOP. Stadia was measured pacing the same workload into ~8-packet groups 2 ms apart. Both a
   latency term in its own right and the cause of the loss that drives items 4 and 25.
4. **`major-webrtc-lagged-viewer-requests-idr-and-makes-it-worse`** (W1) — a 4-frame broadcast overrun
   triggers an IDR, which is the largest frame the encoder emits, which makes the next overrun more
   likely. A self-sustaining keyframe storm on marginal Wi-Fi, and the one mechanism that turns a
   transient into a sustained failure.
5. **`major-session-join-is-a-fully-serial-multi-second-chain`** (W1) — 3 s of client ICE gathering,
   then up to 5 s of display attach, then 1.5 s of host gathering, none of it overlapped. On an
   offline hosted-network setup — a mode this product ships — both STUN waits are paid in full for a
   reflexive address a same-subnet peer will never use.
6. **`major-vt-no-cap-on-frames-in-flight-inside-the-encoder`** (W5) — nothing limits submissions into
   VideoToolbox; a comparable product measured ~8 frames queued when overfed, i.e. ~133 ms at 60 fps
   sitting inside the encoder, invisible to every counter we have.
7. **`major-vt-encoder-implementation-is-never-read-back-and-decides-a-4-to-16-frame-hold`** (W5) — the
   same configuration selects implementations that hold 1 frame or 4–16 frames depending on which one
   VideoToolbox hands back, and we never ask which. 67–267 ms of difference, unmeasured, on the
   platform the latency complaint came from.
8. **`blocker-cgds-iosurface-use-count-never-incremented`** + **`blocker-cgds-queue-depth-starvation`**
   (W5) — the only capture path on the 10.15 floor either corrupts frames (the use count is never
   raised, so WindowServer may composite into a surface VideoToolbox is still reading) or starves at
   `queueDepth = 2` with two surfaces held. They invert each other's diagnosis and must be fixed
   together, in that order.
9. **`major-webrtc-first-idr-discarded-before-track-bound`** (W1) — the IDR is requested seconds before
   ICE and DTLS complete, written into a track with no bindings, and silently returns `Ok(0)`. Every
   join then waits for the browser's own PLI: commonly 100–500 ms of black, and on a decoder that
   never emits one, indefinitely.
10. **`major-sck-minimum-frame-interval-exactly-one-over-fps-drops-frames`** (W5) — a measured 15 %
    frame loss from beating against the compositor, plus a mean half-frame hold-off, on every 12.3+
    Mac. Two independent products treat `1/fps` as a bug and Apple's own header points at
    `kCMTimeZero`.
11. **`major-nvenc-sync-single-bitstream-defeats-ring`** (W3) — `enableEncodeAsync = 0` with one output
    buffer means the K=3 texture ring decouples capture from encode but never overlaps encode N+1 with
    encode N. A whole encode duration stays on the path.
12. **`major-dxgi-path-serializes-capture-and-encode`** (W3) — on the path most Windows hosts are
    actually on (item 1), achievable frame rate is `1/(capture+composite+encode)` rather than
    `1/capture`.
13. **`major-audio-win-engine-period-lowerable-via-render-companion`** (W8) — the silent render
    companion we already own and already start first can drag the whole endpoint's engine period from
    10 ms to 2.67 ms, which the loopback capture stream then inherits. The largest controllable term
    in the Windows audio budget, and it needs no change to the capture stream at all.
14. **`major-audio-mac-tap-aggregate-buffer-frame-size-never-set`** (W7) — the Process Tap aggregate
    inherits whatever IO cycle the HAL defaults to, commonly 512 frames / 10.67 ms. Setting 128–256
    frames removes 5–8 ms, the largest single term in the macOS audio budget.
15. **`major-client-one-lost-packet-costs-a-full-jitter-buffer`** (W11) — a single lost 5 ms packet
    re-primes the worklet, producing 10–40 ms of digital silence and permanently growing the target by
    3 ms. A 2–8× amplification of every loss event, on the deliberately-unreliable transport the
    product actually runs on.
16. **`major-vdisplay-win-decouple-idd-refresh-from-stream-fps`** (W9) — an indirect display has no
    hardware vblank, so DWM's compose cadence for it *is* the refresh rate we ask for, and we weld
    that to the client's panel rate. Running the display at 120 Hz while encoding at 60 removes a mean
    4 ms of compose quantisation, and the 120 Hz mode is *already registered* on every display we
    create.
17. **`minor-cgds-minimum-frame-time-coalescing`** (W5) — `kCGDisplayStreamMinimumFrameTime` is a
    hold-off, not a target: mean ~7.9 ms added to every frame at 60 fps, on the only capture path a
    10.15 Mac can take. Must be measured after item 8, not before.
18. **`major-webrtc-bitrate-control-is-slower-and-slacker-than-the-field`** (W1) — a literal no-op
    between 2 % and 10 % loss, a 120 ms poll of a ~1 Hz signal, and a single absolute-RTT threshold
    that is congestion on a LAN and baseline on a WAN. The stream never finds its operating point;
    every comparable product is loss-predictive.
19. **`major-vt-complete-frames-blocks-the-realtime-thread`** (W5) — a synchronous
    `VTCompressionSessionCompleteFrames` on the deadline-scheduled encode thread, five times a second
    on a static screen, on exactly the pre-11.0 Intel Macs where the complaint originates.
20. **`major-session-blocking-os-calls-on-tokio-workers`** (W1) — a join parks a priority-boosted
    transport worker for 0.2 s (Windows typical) to 10.7 s (macOS worst case) inside FFI, stalling the
    RTP writers of every other client on that adapter.
21. **`major-pipeline-win-cpu-bridge-readback-stalls-encode-thread`** (W3) — a blocking
    `Map(D3D11_MAP_READ)` against a single staging texture plus an 8–33 MB memcpy, serially, inside the
    frame deadline, on every software-encode and bridge host.
22. **`major-pipeline-win-adapter-selection-has-no-runtime-policy`** (W3) — three independent adapter
    choices per session with nothing reconciling them. When capture and encoder land on different GPUs
    the NT-handle share fails, the error is swallowed into a `teprintln!`, and every frame thereafter
    takes a full GPU→CPU→GPU round trip.
23. **`major-wgc-framepool-never-drains-to-the-newest-frame`** (W3) — `TryGetNextFrame` returns the
    *oldest* pooled frame and is called once per callback, so the 3-buffer pool can hand the encoder a
    frame up to 2 frames (~33 ms) stale.
24. **`major-client-refresh-rate-renegotiates-right-after-live`** (W11) — every 90/120/144 Hz client
    tears down the peer connection, the transform worker and the audio graph about a second after first
    paint and rebuilds all of it, because a measurement that finished while the user was typing the OTP
    is consulted only after the stream is live.
25. **`major-nvenc-idr-only-loss-recovery`** (W3) — `maxNumRefFrames = 1`, infinite GOP, intra refresh
    off and unreachable, no reference-frame invalidation. Every recovery is a full IDR into a one-frame
    VBV. Sunshine and Moonlight both treat this as the thing to avoid, and Moonlight advertises
    reference-frame invalidation as a headline capability.

**Reasoning.** The ranking weights three things. *Population* — items 1, 2, 12 and 22 are default
paths, not fallbacks, so their cost is paid by most hosts rather than by an unlucky few. *Boundedness*
— items 2, 8 and 9 can produce a black or corrupted screen rather than a slower one, and an unbounded
failure outranks any number of milliseconds. *Causality* — items 3 and 4 generate the loss that items
18 and 25 then react to, so fixing them changes the value of everything downstream, and item 22
determines whether item 21 is on the path at all. Audio items 13, 14 and 15 rank high despite being
audio because they are the largest single terms in their respective budgets and because A/V skew is
what makes video latency *perceptible* rather than merely present. Items 6, 7, 17 and 19 are
macOS-specific but concentrated: between them they plausibly account for the entire "macOS is very
high latency, AirPlay on the same Mac is faster" report, and items 6 and 7 in particular would explain
a figure that no amount of QoS, thread-policy or capture tuning moves.

**Excluded but prerequisite.** `major-pipeline-win-one-aggregate-timer-and-no-stage-breakdown`,
`major-webrtc-no-per-stage-encode-latency-metric`, `major-client-no-glass-to-glass-telemetry`,
`major-cgds-drop-count-never-read` and `major-nvenc-ring-drops-frames-silently` remove no latency and
must be done first anyway: without them none of the 25 above has a before/after number, and three of
them (8, 17, 22) are currently hypotheses that the instruments would confirm or kill outright.
`major-cgds-drop-count-never-read` is the cheapest measurement in the engagement — CoreGraphics is
already computing the number and handing it to us on every callback.

**Excluded because already resolved.** The RTT-unit blocker — `round_trip_time` read as seconds and
multiplied by 1000, so a 5 ms LAN RTT presented as 5000 ms and the congestion clamp fired on every
poll of every session, pinning the target at the 1 Mbps floor within seconds — would have been rank 1.
It was fixed and its findings file deleted during this pass, as was the NVENC CONSTQP blocker that
disabled bitrate adaptation entirely for any device with a saved override.

---

## Rejected / out of scope

Moved to `findings/_out-of-scope/`, with a one-line reason appended to each file:

- **`major-build-linux-does-not-compile.md`** — Linux fix. `linux_utils` is missing roughly twenty of
  the commands `collect_commands!` registers and `linux_utils/networking.rs` is a single blank line,
  so the target does not build.
- **`major-build-streamer-core-cannot-compile-on-linux.md`** — Linux fix. `streamer/pipeline.rs` has
  Windows and macOS arms only, and `server.rs` uses its symbols unconditionally, so the
  OS-independent core does not build for Linux at all.
- **`minor-linux-stubs-return-success.md`** — Linux fix. `start_hosted_network`, `stop_hosted_network`,
  `is_hosted_network`, `request_permission` and `compatibility.os_supported` all return `true`
  unconditionally, so the UI is told a hosted network is always running.

All three contradict the hard rule that Linux is out of scope. They are retained rather than deleted
because each carries a **documentation** correction W13 should absorb into the stale-reference sweep:
`CLAUDE.md`'s repo map claims "Linux scaffold (compiles, mostly `bail!`s)" and its "add it to all
three modules to keep the surface consistent" rule, both of which are currently false.

**Nothing was rejected for Windows-on-ARM** — no finding in the set targets it. Note that
`blocker-input-cgscroll-variadic-abi-arm64` is *macOS* arm64 (Apple Silicon), which is squarely in
scope and ships today in the universal binary: `CGEventCreateScrollWheelEvent` is declared variadic
one argument too early, so under AAPCS64 the callee reads `wheel1` from `w3` while the caller wrote it
to the stack, and scroll deltas are garbage on every Apple Silicon host. It is invisible on the Intel
dev box because x86-64 SysV passes named and variadic integer arguments through the same registers.

**Nothing was rejected as a UI appearance change**, but the three W12 findings flagged in the
dependencies section need sign-off before landing, because their repairs are visible even though none
is a redesign. `minor-client-https-modal-typo-and-arrow` is deliberately *not* among them: it changes
the client web page, not the desktop UI, and the rule constrains only the latter — and the string in
question ("Switching to the **sucre** version") is shown to every plain-HTTP joiner.

---

## Notes on the merge pass

25 merge groups, 33 files folded. The ones worth knowing about before picking work up:

- The three "blocking OS calls on a tokio worker" findings (shared, Windows, macOS) are now one file,
  `major-session-blocking-os-calls-on-tokio-workers`, carrying both platforms' magnitudes.
- The three CI findings are now `major-build-ci-deleted-docs-still-claim-it`, which also carries the
  archaeology showing CI was added and removed 4m13s apart and has never run, plus the inventory of
  unowned `#[ignore]`d latency harnesses.
- `major-pipeline-win-adapter-selection-has-no-runtime-policy` absorbed three symptom findings — the
  WGC device landing on the wrong adapter, NVENC's adapter chosen by description substring, and the
  next-launch-only `UserGpuPreferences` registry write — and is now the single owner of GPU selection
  on Windows.
- `major-pipeline-win-one-aggregate-timer-and-no-stage-breakdown` absorbed the NVENC ring telemetry
  gap; the ring emitting nothing at all is the highest-priority half of it and should be W3's first
  change.
- `major-permissions-window-name-probe-is-the-live-tcc-read` absorbed the CGDS false-positive
  speculation, which external evidence has since confirmed — the probe can latch `CONFIRMED = true`
  while Screen Recording is denied, and the latch is one-way.
- `major-webrtc-h264-profile-hardcoded-baseline-never-negotiated` now covers the level as well as the
  profile; both are hardcoded in the same three-line match.
- `speculative-webrtc-hevc-tier-is-reachable-now-chrome-136-shipped-h265` supersedes and absorbed the
  older HEVC finding, whose central factual claim (no browser offers an H.265 payload type) has since
  stopped being true.
- Two `capture_ns` findings (Windows and macOS) were deliberately **kept separate and cross-linked**
  despite describing the same defect, because their write-ups contradict each other about which
  platform is currently correct. Reconcile before fixing either.

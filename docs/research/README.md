# Research reports — method

Reports in this directory answer one research question each for the ScreenExtend latency &
compatibility overhaul. Prefer **primary sources**: vendor SDK docs and headers, WWDC session
transcripts, RFCs, source code of open projects (Moonlight, Sunshine, OBS, FFmpeg, Chromium,
webrtc-rs), conference talks, patents, reverse-engineering write-ups. Cite URLs inline.

## Brief for a research agent

Context: ScreenExtend is a Tauri 2 desktop app (Rust core, `src-tauri/`) that creates a virtual
display per client, captures it (Windows: WGC or DXGI Desktop Duplication; macOS:
ScreenCaptureKit or CGDisplayStream), encodes H.264 (NVENC / Quick Sync / x264 on Windows,
VideoToolbox on macOS), packetizes RTP by hand and streams over WebRTC (WHEP signaling, the
`webrtc-rs` crate) to a dependency-free browser page (`src-tauri/src/streamer/static/`) that
decodes with WebCodecs via insertable streams and plays system audio (Opus over a DataChannel)
through an AudioWorklet. Read `CLAUDE.md` at the repo root for the map, and read the specific
local files your brief names so recommendations are grounded in what the code does today.

Hard constraints you must respect in recommendations:
- macOS floor is 10.15; **no link-time references to newer APIs** (`dlsym` / `AnyClass::get` /
  `msg_send!` only). Windows floor is Windows 10 2020-era. Client page has no build step and no
  dependencies and must keep working on old mobile browsers with feature-detected fast paths.
- Capability is selected by **probing**, never by OS version number.
- The frontend UI may not change in appearance.

Load web tools first: `ToolSearch` with query `select:WebSearch,WebFetch`. Search hard; if you
conclude something is impossible, it must be because you searched and found nobody doing it.
If one shipping product does the thing, it is possible.

Your report goes to `docs/research/<topic>.md` with these sections:

1. **Question** — what was asked.
2. **Findings** — what the sources say, with citations. Numbers wherever they exist.
3. **How others do it** — per product/project, the concrete technique.
4. **What ScreenExtend does today** — with `file:line` references from the local tree.
5. **Recommendations** — ranked by expected latency delta vs. implementation cost, each with:
   the exact API/flag/setting, the platform/version it needs, how to probe for it at runtime,
   the expected win, and how to measure it.
6. **Verification plan** — what to measure before/after, and what cannot be verified on the dev
   boxes (Windows 11 26200 + RTX 3080 Ti laptop; macOS 10.15.8 Intel with no Xcode).
7. **Open questions**.

For every actionable recommendation also write one file in `findings/` (schema in
`docs/discovery/README.md`) so it enters the execution queue.

Return to the orchestrator at most 400 words: the top recommendations with expected deltas,
the number of findings files written, and anything that changes the overall plan.

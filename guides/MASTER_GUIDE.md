# ScreenExtend verification guides — index

## How to use a guide

1. Find the row below whose **min OS**, **max OS** and **hardware** your machine actually satisfies.
2. On that machine, clone or check out this repository on the `latency-overhaul` branch and open a
   **fresh Claude Code session** in the repository root.
3. Paste the whole guide file in as your **first message**. It is self-contained: it carries the
   norms, the repo map, the current state of its area with numbers, the tasks, and the report format.
4. It works on a local branch (`guide/<slug>`), commits as it goes, and **never pushes** — the
   repository owner reviews and pushes afterwards.
5. It ends by writing a report. Read that first; the ledger edits are the durable half.

If your machine satisfies more than one row, run them in **separate sessions**. Each guide owns a
disjoint set of ledger rows and merging two runs makes both unattributable.

## The guides

Each file is a complete, standalone directive for one fresh Claude Code instance running on hardware
or an OS that ScreenExtend's two development machines — a Windows 11 build 26200 / RTX 3080 Ti
hybrid-graphics laptop and a macOS 10.15.8 Intel MacBook reached over SSH — could not exercise. A
guide's reader has no access to the original conversation, to the other guides, or to either dev
box. Everything outside a guide's stated territory has already been audited, optimised and verified
elsewhere and must not be touched.

Durations are for a single autonomous run with subagents, on a machine that is otherwise quiet. They
are dominated by measurement and by reboots, not by writing code.

| Guide | What it is | Min OS | Max OS | Hardware | Items | Rough duration |
|---|---|---|---|---|---|---|
| [`modern-mac.md`](modern-mac.md) | Everything macOS above the 10.15 dev floor in one run: the ScreenCaptureKit video and audio tiers, the Core Audio Process Tap, the VideoToolbox low-latency ladder and its long-term-reference adapter, the Sequoia permission re-consent model, the private `CGVirtualDisplay` surface on modern macOS, the macOS hosted Wi-Fi network end to end, the arm64 input ABI — and, in Part B with an Apple Developer identity, Developer ID signing, notarization, TCC identity, translocation, the universal binary and the dyld floor on a real release artifact. | macOS **14.2** | none — macOS 15 / 26 explicitly wanted | An **Apple Silicon** Mac preferred (Intel acceptable, deltas noted); a **console session, not SSH**; an iPhone or iPad. **Part B** additionally needs Developer ID **Application** *and* **Installer** certificates, a `notarytool` credential, and a clean VM. | **30** (M1-00 … M1-29; the last four are Part B) | 4–7 days |
| [`intel-mac.md`](intel-mac.md) | The band no other guide reaches: ScreenCaptureKit's first release and the first-frame commit gate that must catch its "starts and delivers nothing" failure; the ScreenExtend Audio driver as the *selected* audio tier on a supported modern OS, with its `.pkg` install flow, `coreaudiod` restart, default-output save/switch/restore, crash recovery and volume-key proxy; and everything that needs **two GPUs** — `PreferredEncoderGPURegistryID`, automatic graphics switching, cross-GPU IOSurface migration under a live capture, VideoToolbox instance limits and the Intel hardware-encode threshold table. Plus the first console-session, on-battery verification of the thread-QoS and power-assertion work. | macOS **12.3** | macOS **12.7.6** | An **Intel** Mac, **dual-GPU** strongly preferred — a 2018–2019 15"/16" MacBook Pro is the target; a laptop with a battery, run **at the console, not over SSH**; Xcode Command Line Tools; and either **SIP disabled** or an Apple Developer ID, without which three of the ten items are unreachable. | **10** (M2-01 … M2-10) | 2–4 days |
| [`windows-amd.md`](windows-amd.md) | The entire AMD AMF H.264 backend, which has never encoded a single frame on this project — audit it, verify it, then hyper-optimize it to NVENC parity. The ABI against a live runtime, the ULTRA_LOW_LATENCY / LOW_LATENCY usage tiers, NV12 versus BGRA input and AMD's uncontrolled internal colour converter, the CPU bridge, intra-refresh cost, LTR-driven repair, the concurrent-session cap, encode-latency tails, the decoupled capture/encode ring, the SPS VUI rewrite, and the cross-adapter Intel-iGPU + Radeon-dGPU case — plus eight defects found by reading the shipped code, all of which have landed `COMPILE-CHECKED` and need a Radeon to confirm. | Windows 10 **2004** (19041) | none | A Windows box with an AMD GPU carrying a **VCN/VCE AVC** encoder — any Radeon or Ryzen APU since Raven Ridge/Vega **except Navi 24** (RX 6400 / RX 6500), where AMD removed the AVC encoder — plus the Adrenalin driver, `ffprobe`/`ffplay`/`ffmpeg`, MSVC Build Tools 2022 x64 (optional but strongly preferred — it is where `dumpbin` and the ABI generator come from), and a phone to join with. A pre-RDNA part *and* an RDNA2+ part is better than either; an Intel-iGPU + Radeon-dGPU laptop additionally unlocks the cross-adapter item. | **21** (A-01 … A-13, D-01 … D-08) | 3–6 days |
| [`windows-legacy.md`](windows-legacy.md) | Everything below Windows 11 24H2: the Windows.Graphics.Capture property tiering that used to force *every* Windows 10 and pre-24H2 host onto DXGI; the DXGI Desktop Duplication path end to end — unreachable on the dev box, which is WGC-only — including its capture/encode staging ring, the one row whose throughput claim has no number anywhere; hybrid-adapter selection; the virtual-display failure tiers and the DPR mode ladder on an older WDDM; the `netsh` hosted-network tier and its non-English probe; the low-latency WASAPI engine period; the process-loopback tier refusing on Windows 10; the audio format-conversion tiers; and a measure-only pass over Quick Sync on a clean Intel install. | Windows 10 **2004** (19041) | build **22631** — strictly below 26100 | A **hybrid-graphics** laptop (Intel iGPU + discrete) on which **DXGI duplication actually works** — verify that first, it is the gate; a hosted-network-capable Wi-Fi adapter; an audio endpoint the in-box *High Definition Audio Device* driver can drive, plus a non-48 kHz or multichannel endpoint; a non-English Windows display language; Administrator; a phone to join with. Windows 10 22H2 (19045) and Windows 11 23H2 (22631) sit on different property tiers — both is materially better than either. | **21** (L-01 … L-21, as tasks 3.1 … 3.21) | 3–6 days |
| [`clients-network-and-glass-to-glass.md`](clients-network-and-glass-to-glass.md) | Everything the host cannot prove alone: all fourteen client-browser capability tiers and the whole of `streamer/static/`; trickle ICE and H.264 profile negotiation against real engines; DSCP/WMM on a real AP; send pacing and the relayed RTP MTU; cross-network TURN and the cloud relay; repair-from-LTR under induced loss; two pens and four simultaneous clients; and the **camera glass-to-glass measurement** against spacedesk, Deskreen and the published competitor table — the only number this project has that is not self-reported. | Host: Windows 11 **26100+** with an NVIDIA GPU, **or** macOS **14.2+** | none | A client matrix (iOS ≤ 17 **and** ≥ 18.4, an iPhone as well as an iPad, Android Chrome ≥ 141 **and** 86–140, desktop Chrome ≤ 140 and current, desktop Firefox ≥ 130 **and** ≤ 129, Safari 26, Samsung Internet, one constrained-baseline-only decoder, two pen-capable devices, a Bluetooth sink, four devices that can join at once); a Wi-Fi 5 AP and a Wi-Fi 6 AP; a second network and a real TURN server; an IPv6-capable network; wired Ethernet and an Android phone that can USB-tether; `clumsy` (Windows host) or `dnctl`+`pfctl` (macOS host); `iperf3`; a **240 fps camera**; and the competitor apps for the side-by-side. | **34** (C-01 … C-34) | 5–10 days, gated on hands rather than compute |

## Not covered by any guide

- **The pre-10.14.4 AirPlay display tier.** It is only ever *selected* below macOS 10.14.4, which is
  below the app's own `minimumSystemVersion` of 10.15, so the shipped binary will not launch on any
  OS where it would be chosen. This needs a scope decision — delete the tier or lower the floor —
  not hardware. The parts a modern AirPlay *sender* can settle are inside `modern-mac.md` (M1-26).
- **The first run of the GitHub Actions workflows.** `ci.yml` and `build-release.yml` have never
  executed. Running them requires pushing to the repository, which every guide forbids; it is
  written into `modern-mac.md` Part B (M1-28) contingent on the repo owner explicitly authorising
  it, and otherwise stays open.
- **Linux.** `src-tauri/src/linux_utils/` **does not compile**. It implements only part of the
  command set `collect_commands!` names, and several of its stubs report success rather than
  "unsupported". It is declared out of scope for this engagement; see the final report's
  "Recommendations requiring the user".

## The planning record

[`PLAN.md`](PLAN.md) in this folder is a **planning record, not a guide** — nobody runs it on a
machine. It carries the 106-item inventory the five guides were written from, the per-guide item
lists, the context checklist every guide embeds, the items deliberately left to the project's own
development machines, and — in §7 — the **final coverage audit**: every non-`VERIFIED` ledger row,
capability tier, manual-test section and risk-register entry, mapped to the one guide and the one
task that closes it.

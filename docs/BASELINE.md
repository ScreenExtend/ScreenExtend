# Baseline measurements

Numbers captured **before** any optimization, so every later claim has something to reference.
All latencies are per-stage timestamps off the shared host clock (`streamer::audio::host_now_ns`)
unless marked as camera-measured glass-to-glass. Distributions are reported as p50 / p90 / p99 /
max — jitter is felt more than the mean.

## Machines

| Role | Machine | Notes |
|------|---------|-------|
| Windows host | Windows 11 build 26200, NVIDIA RTX 3080 Ti Laptop GPU (driver 580.97), Intel iGPU present (hybrid) | NVENC primary; QSV and x264 also exercised |
| macOS host (dev box) | macOS 10.15.8 Catalina, Intel, Command Line Tools only | CGDisplayStream + VideoToolbox baseline only; SCK / Process Tap / VT low-latency unavailable |
| macOS host (modern) | not available during this run | cells marked *pending user verification* |

## Matrix

Cells are filled by the probe harness (`ScreenExtend --probe-*`) and by the client stats opcode.
`—` means not measurable on the machines available during this run.

| Host | Encoder | Capture | Client | Network | Capture→encode-out p50/p99 (ms) | Encode-out→socket p50/p99 (ms) | Client arrival→paint p50/p99 (ms) | Glass-to-glass (camera) | Status |
|------|---------|---------|--------|---------|------|------|------|------|--------|

## Competitor baselines

Camera-measured on the same hardware and network where possible. Where a product could not be
run during this run, the best independently published number is cited with its source and the
cell is marked *published, not measured here*.

| Product | Host | Client | Network | Glass-to-glass (ms) | Source |
|---------|------|--------|---------|---------------------|--------|

## Glass-to-glass procedure (repeatable)

1. Run the host with a full-screen frame counter (`--probe-live --counter` renders a large
   monotonically increasing frame number plus a millisecond clock in the virtual display).
2. Put the client device beside the host display showing the same virtual display region.
3. Film both screens in one shot at 240 fps (a modern phone in slow-motion mode is enough).
4. Step through the recording; for a sample of frames, subtract the counter shown on the client
   from the counter shown on the host. Multiply by the frame period. Take at least 30 samples
   and report p50 / p90 / p99.
5. Record camera model, frame rate, and the host and client refresh rates alongside the numbers.

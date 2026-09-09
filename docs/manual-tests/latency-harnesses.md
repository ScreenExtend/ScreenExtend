# The capture-to-encoded latency harnesses, and how to run them

**Why this exists:** the `#[ignore]`d latency harnesses are good, and until this note nothing said
they existed. `cargo test` never runs them, so unless somebody knows the exact invocation the
numbers they produce are never produced at all — which is how the primary shipping platform ended
up with no capture-to-encoded measurement even after the macOS one had been written.

Every number here is the *host* half of glass-to-glass: from the OS presenting a frame to the
encoded access unit leaving the encoder. The client half is `docs/TELEMETRY.md`'s
`hostToPaint`; the full loop is the camera method in `docs/BASELINE.md`.

---

## Windows: `windows_capture_to_encoded_latency`

```powershell
cd src-tauri
cargo test --release --lib windows_capture_to_encoded -- --ignored --nocapture
```

- **Release, always.** A debug encoder measures the debug profile and nothing else.
- Covers the primary display with its own `WorkloadWindow` for about six seconds, so the display
  is guaranteed to be changing. Do not use the machine while it runs.
- Duplicates through the real `Duplicator`, encodes through the real `build_backend` ladder, and
  reads back through the real `TextureReader` — so the backend named in the output is the one
  that machine would actually stream with.

Output:

```
duplicating \\.\DISPLAY1 1920x1080 via <duplication api>
encoder backend: <name> (cpu_bridge=<bool>)
windows capture->encoded OK: backend=<name> 1920x1080@60, <n> frames, <bytes> bytes,
  coalesced=<n>, capture_to_encoded=<p50>/<p90>/<p99>/<max> (p50/p90/p99/max ms),
  capture_encode_p95=<n>us (budget 16666us)
```

`scripts/perf-gate.mjs` parses `capture_to_encoded=` and `capture_encode_p95=`; pipe the run into
it with `--key <machine class>` to compare against `ci/perf-baseline.json`, or `--seed` to write
the numbers there.

**Known failure, and it is the machine rather than the harness.** On the Windows dev box every
Desktop Duplication attempt returns `DXGI_ERROR_UNSUPPORTED` for `\\.\DISPLAY1` on the only
adapter that enumerates the output:

```
SKIP windows capture latency (Desktop Duplication unavailable): no adapter can duplicate
\\.\DISPLAY1 (NVIDIA GeForce RTX 3080 Ti Laptop GPU (vendor 0x10DE, LUID 0x17D9A):
Error { code: HRESULT(0x887A0004) ... })
```

The pre-existing `dxgi_duplication_probe` fails identically on that box, which is what identifies
it as environmental. Run this from a real interactive console session on a machine where
duplication works, and seed `ci/perf-baseline.json` from there. Until then the three
`windows_capture_*` metrics have no committed baseline, which the gate reports and passes.

---

## macOS: `end_to_end_latency` and `production_pipeline_smoke`

```sh
cd src-tauri
cargo test --release --lib macos_utils::streamer::pipeline -- --ignored --nocapture
```

- Needs Screen Recording granted to the terminal, and a **console session** — over SSH the
  capture starts and no frames arrive.
- `end_to_end_latency` prints capture-to-encoded mean/p50/p99/max over six seconds.
  `production_pipeline_smoke` prints submit-to-broadcast.
- Unlike the Windows harness it does **not** paint its own workload, so the screen has to be
  changing or it asserts `no frames captured (idle screen? add activity)`. Play a video, or drag
  a window continuously.
- Wrap long runs in `caffeinate -dims`, and run `caffeinate -u -t 5` first: the screen lock
  stalls compositing and poisons the numbers.

VideoToolbox encode on its own, with no display and no Screen Recording grant — this is what CI's
`macos-smoke` job runs:

```sh
cargo test --release --lib macos_utils::streamer::encoder -- --ignored --nocapture
```

---

## The other measuring tests

| what | command | notes |
| --- | --- | --- |
| x264 per-frame encode p95 | `cargo test --release --lib x264_encoder -- --nocapture` | always runs, no `--ignored`; the only metric with a committed baseline |
| CGDisplayStream frame-arrival age | `cargo test --release --lib macos_utils::streamer::cgds -- --ignored --nocapture` | macOS, console session |
| VideoToolbox capability sweep | `cargo test --lib macos_utils::streamer::encoder::tests::hw_capability_probe -- --ignored --nocapture` | which resolutions take the hardware path, and the concurrent-session cap |
| DXGI duplication smoke | `cargo test --lib dxgi_duplication_probe -- --ignored --nocapture` | writes a BMP; the fastest way to tell whether duplication works on a machine at all |
| GPU scaler blt + readback | `cargo test --release --lib scaler -- --nocapture` | always runs; prints median µs for both readback formats |

**A note on reading any of them.** `scripts/perf-gate.mjs` takes several `--input` files and uses
the **lowest** reading per metric, because the lowest is the one least polluted by contention.
Measured on the idle Windows dev box, five consecutive runs of one release binary gave
4157 / 4243 / 4244 / 4261 / 4490 µs — an 8.0 % spread with nothing changed in the code. Under
load the same binary has ranged 4405–9067 µs. Take three runs minimum, and check
`tasklist | findstr /i "rustc cargo"` first.

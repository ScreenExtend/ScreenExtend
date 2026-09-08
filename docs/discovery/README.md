# Discovery reports — method and findings schema

This directory holds the raw per-territory reports produced during the DISCOVERY phase of the
latency & compatibility overhaul. Each report was produced by reading every file in its
territory in full. The synthesized deliverables live one level up: `INVENTORY.md`,
`DATAFLOW.md`, `LATENCY-MAP.md`, `RISK-REGISTER.md`.

## Brief for a discovery agent

You are reading part of ScreenExtend: a Tauri 2 desktop app that turns any browser device into
a wireless second monitor. The host creates a virtual display per client, GPU-encodes it, and
streams it over WebRTC (WHEP, H.264). The engagement goal is **minimum glass-to-glass latency
and maximum compatibility on every host and client**. Read `CLAUDE.md` at the repo root first;
it is the map. Where `CLAUDE.md` and the code disagree, the code wins — note the discrepancy.

Rules:
- Read from the Windows working tree `C:\Users\sarve\Downloads\funnyscnew\ScreenExtend-workspace\main`.
  Use `cat`/`sed -n` over Bash. Read **every file in your territory in full** — not grep, not skim.
- **Do not modify any source file.** Discovery is read-only. The only files you write are your
  report and your `findings/` files.
- Cite `file:line` for everything. Never assume a path is fast, a lock is uncontended, a copy is
  elided, or a flag did anything — say what the code actually does and mark what is unverified.
- Do not touch the Mac (no SSH, no `Z:\`).

Your report goes to `docs/discovery/<id>-<name>.md` with these sections, in order:

1. **Files and purpose** — each file, one paragraph.
2. **Function catalog** — every public and private function: one line each, purpose + notable
   behavior. Group by file.
3. **Threads** — every thread/task spawned: where, priority/QoS, lifetime, what it owns.
4. **Locks** — every mutex/rwlock/atomic used: what it guards, who takes it, hot-path or not,
   whether any guard can live across an `.await` or across a callback (flag the `if let`
   scrutinee temporary-lifetime pattern explicitly).
5. **Buffers and copies** — every frame/audio/packet buffer and each ownership transfer,
   classified: zero-copy / GPU→GPU / GPU→CPU readback / CPU memcpy / allocation per frame.
6. **Queues** — every channel/ring: depth, bounded or not, blocking behavior, back-pressure.
7. **Syscall / FFI boundaries** — every crossing into an OS API, a driver, or a C library;
   note versioning structs, calling conventions, and dyld/link-time exposure on macOS.
8. **Panics** — every `unwrap`/`expect`/index/`unreachable` that a runtime condition can hit.
9. **Silent fallbacks and swallowed errors** — every place a fast path degrades to a slow one
   without logging, or an error is discarded (`let _ =`, `.ok()`, empty `Err(_) => {}`).
10. **TODO / FIXME / HACK / XXX** — verbatim, with location.
11. **Latency path** — which of these files sit on the glass-to-glass (or input, or audio) path,
    an estimated cost per stage, and every serialization point where one stage waits on another.
12. **Stale references** — comments citing documents that do not exist in the tree (`PRD`,
    `AUDIO_NOTES*`, `docs/airplay-fallback-design.md`, `§`, `M1/M2`), references to AI tooling,
    and comments that no longer describe the code.
13. **Dead code candidates** — with the evidence that nothing selects them.
14. **Capability probing** — where behavior is selected by OS version number rather than by
    probing the capability (a defect per the tier doctrine), and where a tier is missing.
15. **Questions and cross-cutting concerns** for the orchestrator.

## Findings files (`findings/` at the repo root; gitignored via `.git/info/exclude`)

**Every problem, opportunity, suspicion, and idea becomes one file.** One finding per file.
Err heavily toward writing the file. Name it `<severity>-<component>-<short-slug>.md` where
severity is `blocker` | `major` | `minor` | `speculative` and component is a short token such as
`nvenc`, `wgc`, `dxgi`, `sck`, `cgds`, `vt`, `webrtc`, `client`, `audio-win`, `audio-mac`,
`input`, `session`, `cloud`, `ui`, `cli`, `build`, `docs`.

Each file must stand alone for a reader with zero context:

```
# <Title — one line>
Status: open
Severity: blocker | major | minor | speculative
Component: <subsystem>, <files>, <platform>
Evidence: what you observed, with numbers and file:line references
Hypothesis: what you think is going on, and what you are unsure about
Proposed fix: concrete enough that someone else could implement it
Verification: how to prove it worked; what to measure before and after
Context: anything a fresh agent would otherwise have to re-derive
```

The file is deleted when the finding is resolved. Do not batch several findings into one file.

## What to return to the orchestrator

At most 400 words: the 5–10 most important things you found, the number of findings files you
wrote, and any cross-cutting concern that spans territories. Everything else belongs in the
report file, not in the return message.

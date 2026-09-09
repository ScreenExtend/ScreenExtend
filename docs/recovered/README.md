# Recovered documents

Four design notes that source comments cite were written, committed and then deleted — one of
them only ever existed inside an unreachable commit that the next `git gc` would have collected.
They are restored here verbatim so that every `.md` path named in a comment resolves in the tree.

Each file opens with an HTML comment recording where it came from: the commit that added it, the
commit that removed it, and its original path. **Do not edit the bodies.** They are a historical
record, and several of them describe decisions the code no longer makes; the header is what tells
a reader how much to trust the contents.

| File | Origin | Why it is still cited |
|------|--------|-----------------------|
| `PRD.md` | added `dcf3dc3` (2026-08-23), deleted `420004a`; root `PRD.md` | The Windows system-audio spec. §4.4 (format negotiation), §6.4 (cross-origin isolation) and §7.10 (the macOS/Linux stubs) are cited from `windows_utils/audio/` and `streamer/`. |
| `AUDIO_NOTES.md` | added `dcf3dc3`, deleted `420004a`; root `AUDIO_NOTES.md` | The measured spike results behind the audio pipeline's shape: §3.1 the engine period, §3.2 the silent-companion evidence (0/20 event signals without it, 20/20 with), §3.5 the 5 ms Opus frame. Cited from `audio/engine.rs`, `audio/silence.rs` and `cli.rs`. |
| `DEVLOG.md` | added `ff622b3`/`0bf50e6` (2026-07-01), deleted `d737efa` (2026-07-16); root `DEVLOG.md` | Not cited from code. Kept because it is the only narrative record of why the Rust core is shaped the way it is. |
| `intel.md` | added `17fcbc2`, deleted `70a09f9`/`937902a`; `src-tauri/src/windows_utils/streamer/intel.md` | The *pre-implementation* design note. A file of the same name lives at that path again today, rewritten after the backend shipped — this is the original, kept for the reasoning it records and nothing else. |
| `airplay-fallback-design.md` | a stub | Promoted back to its original path, `docs/airplay-fallback-design.md`, which is where the code cites it. |

## The objects behind these files

Three of the originals were only ever in **unreachable** objects, which `git gc --prune=now` — or
git's own automatic gc — would have deleted silently, with no remote copy to recover from. Their
content is now in the tree, and the objects themselves are anchored by local annotated tags so
the provenance survives too:

| Tag | Object | What it holds |
|-----|--------|---------------|
| `archive/airplay-design` | commit `67687cb` | the only revision of `docs/airplay-fallback-design.md` that ever existed |
| `archive/monaco-preload` | commit `c5bf2afe` | the monaco preload rationale comments and measurements, restored into `src/lib/preload-editor.ts` and `docs/OPTIMIZATIONS.md` row 9 |
| `archive/audio-commit-plan` | blob `81cfd8b5` | the plan that split the system-audio work into the A-M series that landed on `main` — the only record of which shipped commit is which chunk, and it is in no tree |

The tags are local. They are not release tags and should not be pushed with `--tags`; `git push
origin archive/…` by name is the way to share one if it is ever wanted.

Documents that comments used to cite and that **never existed in any commit, reachable or not** —
`PRD-macos-audio.md`, `PRD-macos-legacy-audio.md`, `AUDIO_NOTES_MACOS.md`,
`AUDIO_NOTES_MACOS_LEGACY.md` — cannot be recovered. Every citation of them has been replaced by
the substance it was standing in for; do not re-add a pointer to one.

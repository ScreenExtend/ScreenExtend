# Development workflow for the two-machine setup

ScreenExtend is developed on two machines that hold independent clones of this repository:
a Windows box (the primary tree) and an Intel Mac running macOS 10.15.8 reachable over SSH.
This note records the working protocol used during the latency & compatibility overhaul so
that the same conventions can be followed afterwards.

## Trees and branches

| Machine | Tree | Notes |
|---------|------|-------|
| Windows | `C:\Users\sarve\Downloads\funnyscnew\ScreenExtend-workspace\main` | primary; all cross-platform and Windows-only commits originate here |
| Mac | `/Users/sarvesh/Desktop/ScreenExtend` | also mounted on Windows as `Z:\` (Git Bash `/z/`); `Z:\` is the Mac repo **root** |

The overhaul branch is `latency-overhaul` on both trees. Parallel workstreams use git
worktrees: on Windows under `.claude/worktrees/<name>` (each with its own
`CARGO_TARGET_DIR=C:/Users/sarve/.cargo/shared_target/ScreenExtend-<name>` so builds do not
contend), on the Mac under `/Users/sarvesh/Desktop/ScreenExtend/_wt/<name>` (visible from Windows
as `Z:\_wt\<name>\`) sharing the single Mac target dir
`/Users/sarvesh/Desktop/ScreenExtend/src-tauri/target` because the Mac has 16 GB free, 4 cores and
8 GB of RAM — the cargo lock serializes builds, and no more than three Mac workstreams should build
at once. `_wt/`, `_sync/` and `findings/` are excluded locally through `.git/info/exclude`, never
through `.gitignore`.

## Editing and building on the Mac

- **Edit Mac files by writing to `Z:\...`** (a plain file write; verify afterwards over SSH with
  `git status --short && git diff --stat` before building — SMB writes can land partially).
- **Everything else over SSH**, always with the explicit OpenSSH binary:

  ```
  "C:/Windows/System32/OpenSSH/ssh.exe" -o ConnectTimeout=15 -o BatchMode=yes sarvesh@172.16.11.81 \
    "cd /Users/sarvesh/Desktop/ScreenExtend && export PATH=\"$HOME/.cargo/bin:$HOME/.nvm/versions/node/v22.22.3/bin:$PATH\" && <cmd>"
  ```

  Node and pnpm are not on the non-interactive PATH; cargo is `~/.cargo/bin/cargo` (1.96).
- Large reads and repo-wide greps go over SSH, not through `Z:\`.
- In a shared checkout, `git add` only your own files by explicit path. Never `git add -A`,
  `git checkout -- .` or `git stash` in a tree another workstream is using.

## Moving commits between the trees

- **Windows → Mac**: `git bundle create /z/_sync/win-to-mac.bundle <branch>` on Windows, then over
  SSH `git bundle verify _sync/win-to-mac.bundle && git fetch _sync/win-to-mac.bundle <branch> && git merge FETCH_HEAD`.
- **Mac → Windows**: `GIT_SSH_COMMAND="C:/Windows/System32/OpenSSH/ssh.exe -o BatchMode=yes" git fetch ssh://sarvesh@172.16.11.81/Users/sarvesh/Desktop/ScreenExtend <branch>` then `git merge FETCH_HEAD`.
  PowerShell shows git's progress output in red; that is not a failure.
- The remote `origin` is read-only during the overhaul: nothing is pushed.

## Gates before every commit

Windows, from `src-tauri/`: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test`; from the root when `src/` or `src/lib/bindings.ts` changed: `pnpm lint`,
`pnpm build`, `node scripts/check-legacy-webkit.mjs`.

Mac, over SSH from `src-tauri/`: the same three cargo gates, plus
`scripts/check-macos-dyld.sh target/release/ScreenExtend /Library/Developer/CommandLineTools/SDKs/MacOSX10.15.sdk`
after any macOS change that touches FFI (it fails on any link-time symbol newer than 10.15).

Commits are small and single-purpose; a performance commit states its measured before/after.
Commit messages end with:

```
Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_011SucscwafoFNVycnPcxzyt
```

## Invariants (the full list is in `CLAUDE.md`)

`tprintln!`/`teprintln!` rather than `println!`; `std::sync::Mutex` only, never held across an
`.await`; no allocation or locking on real-time capture threads; probe capabilities, never
version numbers; no link-time references to macOS APIs newer than 10.15; the client page has no
build step and no dependencies and must parse on old mobile WebKit; a new Tauri command goes into
all three `*_utils` modules and `collect_commands!`; never hand-edit `src/lib/bindings.ts`
(regenerate with `pnpm tauri dev` run from `src-tauri/`, no CLI arguments, debug build; diff it);
a new shell invocation goes into `src-tauri/capabilities/default.json`; the desktop UI's
appearance does not change.

## The findings queue

`findings/` at the Windows repo root is the work queue and the crash-recovery point: one finding
per file (`<severity>-<component>-<slug>.md`, schema in `docs/discovery/README.md`, each tagged
`Workstream: W<n>` by `docs/discovery/TRIAGE.md`). A file is deleted the moment its finding is
resolved — implemented and verified, or deliberately rejected with the reasoning recorded in
`docs/OPTIMIZATIONS.md` first. New discoveries that are not fixed on the spot become new files.

## Ledgers

- `docs/OPTIMIZATIONS.md` — every change attempted: hypothesis, before, after, kept or reverted,
  status `VERIFIED` / `COMPILE-CHECKED` / `UNVERIFIED`. Failures are recorded on purpose.
- `docs/CAPABILITY-TIERS.md` — every subsystem × tier with its probe and status.
- `docs/BASELINE.md` — the pre-optimization numbers; `docs/TELEMETRY.md` — how to read them.
- `docs/manual-tests/` — scripts for what the two development machines cannot execute.

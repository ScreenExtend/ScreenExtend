# Vendored binaries — provenance

These are committed third-party binaries. Checksums are pinned in `SHA256SUMS`
and verified by the `blob-checksums` job in `.github/workflows/ci.yml`.
**When updating any blob, update both its SHA-256 here and in
`SHA256SUMS`.** Values below were recorded during the hardening audit; the
upstream version/tag marked "confirm" should be verified against the upstream
release the next time these are refreshed.

## nefcon console tool (`nefconc-*`)

- **Upstream:** [nefarius/nefcon](https://github.com/nefarius/nefcon) — a CLI for
  creating/removing device nodes and installing drivers (`devcon` replacement).
- **Used for:** installing/removing the virtual display driver device node
  (see `src-tauri/src/lib.rs` `installdrivers`/`removedrivers`), via Tauri
  `externalBin` sidecar `nefconc`.
- **Version/tag:** confirm against the upstream release the `.exe`s were taken
  from (nefcon releases are published on the GitHub Releases page).
- **License:** MIT (confirm against the upstream `LICENSE`).

| File | SHA-256 |
| --- | --- |
| `nefconc-x86_64-pc-windows-msvc.exe` | `9dba1f1a9e2b21843c4a0ca6c6ffa9e747250dd6e42c8c14325ca069ad43ea8f` |
| `nefconc-aarch64-pc-windows-msvc.exe` | `74f5a8628e9591b10994080bea60c1af6063a4fb4aec50e358af4f02c15da6ca` |
| `nefconc-i686-pc-windows-msvc.exe` | `7dcbba1dbf59c035dc70a02f6270f765767f3d288bceb5f9a5b43f37039ca25e` |

### macOS placeholder stubs

`nefconc-x86_64-apple-darwin` and `nefconc-aarch64-apple-darwin` are **not**
nefcon — they are 17-byte no-op shell stubs (`#!/bin/sh\nexit 0`). Tauri's
`externalBin` requires a file for every target triple and nefcon is Windows-only,
so they exist to satisfy that. `tauri.macos.conf.json` sets `"externalBin": []`,
so the macOS bundle ships neither them nor nefcon.

`.gitattributes` marks them `binary`. They contain no NUL byte, so without that
rule git's text detection rewrites them to CRLF on a Windows checkout and the
checksums below stop matching — which is exactly what the old CI checksum job
tripped over.

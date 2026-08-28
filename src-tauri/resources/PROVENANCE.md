# Vendored resources — provenance

Third-party binaries bundled as Tauri `resources`. Checksums are pinned in
`SHA256SUMS` (verified in CI). **When updating any blob, update both its SHA-256
here and in `SHA256SUMS`.** Versions/tags marked "confirm" should be verified
against the upstream release the next time these are refreshed.

## Virtual Display Driver (`VirtualDisplayDriver.{dll,cat,inf}`)

- **Upstream:** [MolotovCherry/virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs)
  — a Windows indirect-display driver (IDD) creating virtual monitors. Credited
  in the project README.
- **Used for:** creating a real virtual display per connected client.
- **Version/tag:** confirm against the upstream release; the driver package
  (`.inf`/`.cat`/`.dll`) is published on the upstream GitHub Releases.
- **License:** confirm against the upstream `LICENSE` (virtual-display-rs).
- **Signing:** the `.cat` catalog is signed; `ScreenExtend.cer` (below) is the
  certificate that must be trusted for Windows to load the driver.

| File | SHA-256 |
| --- | --- |
| `VirtualDisplayDriver.dll` | `8e19212066a042fa8cc99d648f5bcfe48326db164f43b5b695e12df04f52afa5` |
| `VirtualDisplayDriver.cat` | `4957f049286edc52ab7901ec594e362eb35f66ab268fce3bdfbd64a68b83d86d` |
| `VirtualDisplayDriver.inf` | `7163a11c9b7c521cb758253c9bbad9f85bd875f3add5260e4093ad94f2a8ad88` |

## Driver code-signing certificate (`ScreenExtend.cer`)

- **What:** the public certificate installed into the Windows `root` and
  `TrustedPublisher` stores (via `certutil`) so the signed virtual display driver
  loads. Public cert only — no private key.
- **SHA-256:** `db896ceccd9cba9e5f5cbebdb1d61d27e8e984f076233f97bcc75840faf0492c`

## libopus (`libopus.dll`)

- **Upstream:** [xiph/opus](https://github.com/xiph/opus) — the reference Opus audio
  codec. BSD-3-Clause.
- **Used for:** low-delay (`OPUS_APPLICATION_RESTRICTED_LOWDELAY`, CELT-only) encode of
  captured system audio before WebRTC transport. Loaded at runtime via `libloading`, the
  same way `libx264-164.dll` is — see `windows_utils/audio/opus_sys.rs`.
- **Version:** built from source at commit
  `03647f524a40b05a1898522e92033810b58103c7` (2026-08-14). `opus_get_version_string()`
  reports `libopus unknown` because the DLL was produced from a shallow git checkout that
  lacks the generated `package_version` file; the commit hash above is the authoritative
  pin. Built x64 Release, `-DBUILD_SHARED_LIBS=ON` (DRED/OSCE off — encoder-only path).
- **License:** BSD-3-Clause (Xiph.Org / Skype et al.). Compatible with this project's
  AGPL-3.0 (permissive → copyleft is fine).
- **SHA-256:** `73924f6a4124a52c9a3e1b5cedb68339f4d21627826b2b5fce5c453dde2857b5`

## libopus — macOS (`libopus.dylib`)

- **Upstream:** [xiph/opus](https://github.com/xiph/opus). BSD-3-Clause. Same codec as the
  Windows `libopus.dll`; feeds the shared encoder wrapper (`streamer/audio/opus_sys.rs`).
- **Used for:** low-delay Opus encode of captured macOS system audio (Process Tap / SCK) before
  WebRTC transport. Loaded at runtime via `libloading`, same pattern as the Windows DLL.
- **Version:** built from source at the **same** commit as the Windows DLL,
  `03647f524a40b05a1898522e92033810b58103c7`. `opus_get_version_string()` likewise reports
  `libopus unknown` (shallow checkout lacks the generated `package_version`); the commit hash is
  the authoritative pin.
- **Build:** `clang -O2 -fPIC -shared -DOPUS_BUILD -DVAR_ARRAYS -DHAVE_LRINTF -DFLOAT_APPROX`
  over the `celt_sources.mk` + float `silk_sources.mk` + `opus_sources.mk` C sources (x86/arm/mips
  intrinsics and the fixed-point SILK path excluded — generic-C float build). `otool -L` shows it
  links only `/usr/lib/libSystem.B.dylib`.
- **⚠️ Architecture:** this artifact is **x86_64 only** — it was built on the macOS 10.15 (Intel)
  dev box, whose SDK cannot target arm64 macOS. The **shipped** binary should be a **universal**
  (x86_64 + arm64) `libopus.dylib` built on a newer Mac / CI runner from the same opus commit; the
  runtime loader finds `libopus.dylib` regardless of slice, so swapping in a fat binary needs no
  code change. Re-pin the SHA-256 below when that universal artifact is produced.
- **License:** BSD-3-Clause. Compatible with AGPL-3.0.
- **SHA-256 (current x86_64 build):**
  `8285c2a9faf6360c6eb0803639a0d6011fcc600827c4ae5356bfdfe7a694ff0e`

## libx264 (`libx264-164.dll`)

- **Upstream:** [videolan/x264](https://code.videolan.org/videolan/x264) — the
  x264 H.264 encoder. `164` is the libx264 ABI/build number.
- **Used for:** the CPU (software) H.264 encode fallback when no supported GPU
  encoder is available.
- **Version:** libx264 build 164 (confirm the exact commit/snapshot against the
  build it was produced from).
- **License:** GPL-2.0-or-later (x264). Note the licensing implications of
  bundling x264.
- **SHA-256:** `87bf8a8331691b32cf6c8e9b282c9c2a825ed0b83eba0f24e539f667c042684e`

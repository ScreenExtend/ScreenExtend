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

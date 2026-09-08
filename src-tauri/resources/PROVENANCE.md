# Vendored resources — provenance

Third-party binaries bundled as Tauri `resources`. Checksums are pinned in
`SHA256SUMS` and verified by the `blob-checksums` job in
`.github/workflows/ci.yml`. **When updating any blob, update both its SHA-256
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

## Intel oneVPL dispatcher (`libvpl.dll`) — **deliberately not bundled**

- **Upstream:** [intel/libvpl](https://github.com/intel/libvpl), MIT.
- **Used for:** `windows_utils/streamer/intel/intel_sys.rs` loads it by name to reach the Quick
  Sync encoder. Without a dispatcher there is no Quick Sync at all and the host falls to x264.
- **Why nothing is shipped here.** Three reasons, in order of weight:
  1. Intel's own guidance is that Xe (Gen12) and later driver packages install the dispatcher.
     Measured on the Windows dev box (Intel UHD Graphics, DCH driver):
     `C:\Windows\System32\libvpl.dll`, oneVPL Dispatcher **2.7.0** (ProductVersion 2022.2),
     CompanyName Intel, Authenticode-valid under *Microsoft Windows Hardware Compatibility
     Publisher*, SHA-256
     `803a53d4a8062838a7fa85bc7385a036121162fb969d2f501803d1b01e863815`.
  2. Pre-Gen12 hosts, the ones Intel says must ship their own dispatcher, run **Media SDK**
     behind it — API 1.x. This encoder requires oneVPL **2.x** for internal surface allocation
     (`MFXMemory_GetSurfaceForVPP`/`Encode`, `mfxFrameSurfaceInterface::GetNativeHandle`) and
     `create_session` rejects a 1.x runtime at session creation. Shipping a dispatcher for those
     hosts would buy a cleaner error message, not a working encoder.
  3. There is no official prebuilt Windows binary to vendor: `intel/libvpl` releases are source
     only (checked through v2.17.0 — no assets). The only `libvpl.dll` on a real machine is the
     driver's, and copying a WHQL-signed driver component into our own signed installer is not a
     provenance chain this file could honestly record.
- **Consequences for the loader.** Because the driver's copy is the one matched to the runtime
  the same package installed, `open_dispatcher` searches the **OS path first**, then the exe
  directory, then `resources/`. An app-side copy must never shadow the driver's.
- **If this is ever revisited:** build from an `intel/libvpl` release tag with MSVC, record the
  tag, the commit, the compiler and the SHA-256 here and in `SHA256SUMS`, add
  `"resources/libvpl.dll"` to `bundle.resources` in `tauri.conf.json`, and re-verify on a Gen12
  host that the bundled copy does not displace the driver's.

## NVENC header (`nvenc-headers/nvEncodeAPI.h`) — source only, **not bundled**

- **Upstream:** [FFmpeg/nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers),
  `include/ffnvcodec/nvEncodeAPI.h` at tag **`n12.1.14.1`** (commit
  `e1ff958747e9e08247cea31b6e629d61ece824c0`), which is NVENC API **12.1**
  (`NVENCAPI_MAJOR_VERSION 12`, `NVENCAPI_MINOR_VERSION 1`).
- **License:** MIT, in the header's own copyright block — "permission is hereby granted, free of
  charge... to deal in the Software without restriction, including... distribute". This is the
  header FFmpeg itself builds NVENC support against; NVIDIA's own SDK copy carries a more
  restrictive licence and is deliberately not the source used here.
- **Used for:** `tools/nvenc_layout_gen.c` compiles against it and emits the assertions in
  `windows_utils/streamer/test/nvenc_layout.rs`. Nothing links against it and nothing ships it —
  `nvEncodeAPI64.dll` comes from the NVIDIA driver and is loaded by name at runtime.
- **Why this version.** The bindings in `nvenc_sys/` were audited against it rather than assumed.
  Every struct revision in the hand-written `version.rs` matches 12.1 exactly (CONFIG 8,
  INITIALIZE_PARAMS 6, RECONFIGURE_PARAMS 1, PRESET_CONFIG 4, PIC_PARAMS 6, LOCK_BITSTREAM 1,
  REGISTER_RESOURCE 4, EVENT_PARAMS 1); 13.0 raises every one of those by one. The members that
  looked like SDK-13 additions — `nvEncRestoreEncoderState`, `nvEncLookaheadPicture`,
  `numStateBuffers`, `outputStatsLevel`, `splitEncodeMode`, `NV_ENC_CAPS_EXPOSED_COUNT = 55` —
  are all present at 12.1, so there was no version skew to fix.
- **SHA-256:** `dbdaa8bcbc2b9325bd24bc68b36170a8432411518040e8d4c8421608cf217425`

## AMD AMF headers (`amf-headers/`) — source only, **not bundled**

- **Upstream:** [GPUOpen-LibrariesAndSDKs/AMF](https://github.com/GPUOpen-LibrariesAndSDKs/AMF),
  `amf/public/include/{core,components}` at commit
  **`c35f613aea2e5057a688c979e75b1cf24253297e`** (`master`, 2026-07-06), which is AMF
  **1.5.2.0** (`AMF_FULL_VERSION` = 281496451678208). Every file below was verified
  byte-identical to that commit's blob.
- **Used for:** measuring the AMF C ABI. `src-tauri/tools/amf_layout_gen.c` compiles these
  headers in C mode and prints the struct sizes, field offsets, vtable slot indices, enum values
  and property-name strings that `windows_utils/streamer/test/amd_layout.rs` asserts against the
  hand-written bindings in `windows_utils/streamer/amd_sys/`.
- **Not shipped, not linked, not compiled into the app.** They are a test fixture: absent from
  `bundle.resources` in `tauri.conf.json`, unread by `build.rs`, and the encoder loads
  `amfrt64.dll` at runtime with `libloading`. The runtime itself ships with the AMD
  Radeon/Adrenalin driver and is never redistributed by us.
- **License:** MIT, plus a non-license "Notice Regarding Standards" preamble disclaiming any
  patent grant for MPEG-2/MPEG-4/AVC/HEVC/AAC/VC-1/MP3. Both blocks are kept intact at the top of
  every vendored file. MIT is compatible with AGPL-3.0.
- **Refreshing:** re-fetch the whole `{core,components}` set at one commit, re-run
  `src-tauri/tools/amf_layout_build.bat`, paste the regenerated assertions into
  `test/amd_layout.rs`, and update the commit above plus every SHA-256 in `SHA256SUMS`. A diff in
  the generated numbers is a required review item — a changed vtable slot index is silent memory
  corruption, not a compile error.

| File | SHA-256 |
| --- | --- |
| `components/ColorSpace.h` | `4ef6392bb4b9c6b05d00b5cd1486b99866e40e61449206a3a515063e4ffd289b` |
| `components/Component.h` | `f3f18680d55a65fdf5e0351d3fde1dcd9bdd2d6caec268c8c915c4227ffa1edc` |
| `components/ComponentCaps.h` | `2479eb2a6174a1b63552c509abac1b701eddcab5a5eac5a7f4d9f40274d588b4` |
| `components/PreAnalysis.h` | `801a197f578104c4481d21f68c3dd6ea8fbe2f9774424277499c5de70d213662` |
| `components/VideoConverter.h` | `a7d565fbe50d82db7be7922a0d4408e989a632bc46d38cba6d9f107ad8d6dcb5` |
| `components/VideoEncoderAV1.h` | `2c4e18f6d0be4701d9a9d9ad4298ed5d7ed7c1fd7a909995c4e745874d8203a0` |
| `components/VideoEncoderHEVC.h` | `138ad005e4b7131582fc1f82dad8851c1362f0d156bf62f10a64e462c8a037c0` |
| `components/VideoEncoderVCE.h` | `8dcddd6df00debd096db29f00fa155e3e179ebc6ea4c6c1edc5027eb857cffd5` |
| `core/AudioBuffer.h` | `74b4bbc22c66520c233f258dba0642c3f4de66ae02d16201c3e224f9238b5d5d` |
| `core/Buffer.h` | `3e52733b46e168f1b0118381df1ce17eed17353ca89b39c77394f59e254aef67` |
| `core/Compute.h` | `dc35befae2cecf5d2b866632a8b16c2f2ec3a543a6b474f941a33fcd7a8d1e0a` |
| `core/ComputeFactory.h` | `50acf69fb670bc6cf093384d1b698bf9e44cda8dd49e02ef24eacfb86ad8ccae` |
| `core/Context.h` | `f9b4acb784038aad234ddec490e5f6ad6dacc4b14794eb84f3523d56dc3d6dd2` |
| `core/Data.h` | `eaac4d80dd3215638c70f8a4ca01791d26d9df77586f665a772bb51907a8035b` |
| `core/Debug.h` | `56666589bf6ad20c6286a048da751c2d7722f70c73d962ee3c205a6d709b6aff` |
| `core/Factory.h` | `644814e97dd359f08d66a73748b60262149b106d28423c31b573cecb4be57979` |
| `core/Interface.h` | `304f6d6077e9b0733859d44a5e43a343ae374c8fe72c7d79741c0aca89c21162` |
| `core/Plane.h` | `5ad5165e9de2afcfc2da828e8704c838b1f82e8ead7422fbcfc32c758672e1f3` |
| `core/Platform.h` | `c060dc2466d0d886d1a09e5fea891aa3bc2efd08dc8d5ef59faa4d0a7f42abd4` |
| `core/PropertyStorage.h` | `0059b787e78cd7c4e57212ccaab56ad7e17e9474a31337da0eac5b2d056a5dd8` |
| `core/PropertyStorageEx.h` | `871136e46586baa1fc77121935b503aaa16523d006c3e435ce50698605675fad` |
| `core/Result.h` | `a469ff33875484b5762199da757819056ff4a7535bfdcec1f4cf2ffc403b98f2` |
| `core/Surface.h` | `2f7f899a9f6291e0b02a447af6dd0e2c367a02271e7295c26cfd3b15de7d82a9` |
| `core/Trace.h` | `ae8c5fee671f02b90d3ccab312cfa6f74424351f070a7dd4ed7aba437fd55c30` |
| `core/Variant.h` | `5d15e778f3b846fa6eb95737e7222e0726b09615eb6270aac12bd2caa7137234` |
| `core/Version.h` | `739f6559891d627fc373bfd63c4bcc3c8a432766703928d94cc1b92bc7bcfdf3` |

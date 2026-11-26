# `src-tauri/tools`

Build-time C programs for ScreenExtend: they measure the AMD AMF and NVIDIA NVENC ABIs from the vendored headers and print Rust test assertions.

They are helpers only. Nothing here is compiled by `cargo`, read by `build.rs`, or shipped.

## Requirements

- Windows with the MSVC Build Tools (`cl.exe`). Both `.bat` files enter the x64 environment through `C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat`.
- Not MinGW `gcc`. `amfrt64.dll` and `nvEncodeAPI64.dll` are MSVC-built, and MinGW models neither the MSVC C++ ABI nor MSVC's bit-field packing.

## Quick start

```bat
cd src-tauri\tools
amf_layout_build.bat
nvenc_layout_build.bat
```

Each script prints the files it wrote:

```text
wrote amf_layout_generated.txt and amf_layout_cpp_check.txt
wrote nvenc_layout_generated.txt
```

Paste the sections of each generated file over the matching test bodies, in order:

| Generated file | Test file in `src-tauri/src/windows_utils/streamer/test/` |
|---|---|
| `amf_layout_generated.txt` | `amd_layout.rs`, the `abi_*_matches_amf_headers` bodies |
| `nvenc_layout_generated.txt` | `nvenc_layout.rs`, the `abi_*` bodies |

The generated files, `*.exe` and `*.obj` are gitignored.

**Do not run the generators from `build.rs`.** A build machine may have no `cl.exe` (no macOS host does), and the checked-in Rust is the product. Regenerate only when the vendored headers change, and review any change in the numbers before you commit it.

## `amf_layout_gen.c`: the AMD AMF ABI generator

`windows_utils/streamer/amd_sys` binds AMF through COM-like C vtables, where the slot *order* is the ABI. Calling slot 48 where you meant 49 compiles and runs without an error message. It calls through the wrong function pointer.

`AMFContext::CreateSurfaceFromDX11Native` is on the hot path, and it is slot **49 of 55**, with the OpenCL, OpenGL, XV and Gralloc blocks before it. So the numbers are measured, not counted.

`amf_layout_gen.c` includes the vendored headers from `../resources/amf-headers` in C mode (`cl /TC`). That measures the `#else /* !__cplusplus */` branch, which is the one `amd_sys` binds, and prints ready-to-paste Rust assertions. `offsetof(SomeVtbl, member) / sizeof(void *)` is the vtable slot index, because a C-API vtable struct is exactly an array of function pointers.

`amf_layout_cpp_check.cpp` compiles the same headers with `/TP` and dumps the C++ layouts. Any struct whose two builds disagree must be modelled from the C++ side, because `amfrt64.dll` is built from the C++ headers.

Today exactly one struct diverges: `AMFPropertyInfo`. It is 112 bytes with `name` at offset 0 in C, but 120 bytes with `name` at 8 in C++, because it has a virtual destructor. `amd_sys` avoids it: it never calls `GetPropertyInfo` and probes optional properties with `HasProperty` instead.

To run the AMF generator by hand instead of through `amf_layout_build.bat`:

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cl /nologo /TC /W3 /I ..\resources\amf-headers amf_layout_gen.c /Fe:amf_layout_gen.exe
amf_layout_gen.exe > amf_layout_generated.txt
```

## `nvenc_layout_gen.c`: the NVENC ABI generator

`windows_utils/streamer/nvidia/nvenc_sys` is bindgen output plus a hand-written `version.rs`. `test/nvenc_layout.rs` used to assert the Rust structs against constants from the same bindgen run, so it caught only a hand edit to the Rust. It could not catch bindings generated from one SDK header while `version.rs` copied the struct revisions of another.

A wrong `NV_ENC_*_VER` word is not a compile error. `nvEncInitializeEncoder` returns `NV_ENC_ERR_INVALID_VERSION`, `build_backend` catches it, and the NVIDIA path silently falls back to Quick Sync or x264 on a machine with a working NVENC.

`nvenc_layout_gen.c` compiles against the vendored `../resources/nvenc-headers/nvEncodeAPI.h` (FFmpeg/nv-codec-headers `n12.1.14.1`, MIT). It prints ready-to-paste Rust assertions for:

- the API and struct version words;
- sizes and alignments;
- every field offset the encoder writes or reads;
- every `NV_ENCODE_API_FUNCTION_LIST` offset (the driver fills that table, so a wrong offset is a call through the wrong function pointer);
- the enum values and capability ids;
- the GUIDs, which bindgen mis-generates (rust-bindgen#1888), so `nvenc_sys/guid.rs` writes them out by hand.

MSVC and MinGW do not pack the `unsigned int x : 1` runs in `NV_ENC_CONFIG_H264` and `NV_ENC_PIC_PARAMS_H264` the same way, which is why this needs MSVC.

To run the NVENC generator by hand instead of through `nvenc_layout_build.bat`:

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cl /nologo /TC /W3 /I ..\resources\nvenc-headers nvenc_layout_gen.c /Fe:nvenc_layout_gen.exe
nvenc_layout_gen.exe > nvenc_layout_generated.txt
```

The generator prints C's bare enum-constant names. The glob `use` lines at the top of `nvenc_layout.rs` make the paste work as is.

Neither generator can check whether the *driver* agrees. Only running the encoder does that, in `test/nvenc_encoder.rs`.

## License

These files are part of ScreenExtend and are covered by the repository's [AGPL-3.0 license](../../LICENSE). The AMF and NVENC headers they compile against are MIT-licensed; their upstream pins and SHA-256 checksums are in [`../resources/PROVENANCE.md`](../resources/PROVENANCE.md).

# `src-tauri/tools`

Build-time-only helpers. Nothing here is compiled by `cargo`, read by `build.rs`, or shipped.

## `amf_layout_gen.c` — the AMD AMF ABI generator

`windows_utils/streamer/amd_sys` binds AMF through COM-like C vtables, where the slot *order* is
the ABI. Calling slot 48 where you meant 49 is not a type error and not a runtime error message;
it is a call through the wrong function pointer. `AMFContext::CreateSurfaceFromDX11Native` — the
hot path — is slot **49 of 55**, with the OpenCL/OpenGL/XV/Gralloc blocks in between, so the
numbers are measured rather than counted.

`amf_layout_gen.c` includes the vendored headers from `../resources/amf-headers` **in C mode**
(`cl /TC`), so the `#else /* !__cplusplus */` branch — the one we bind — is what gets measured,
and prints ready-to-paste Rust assertions. `offsetof(SomeVtbl, member) / sizeof(void *)` *is* the
vtable slot index, because a C-API vtable struct is exactly an array of function pointers.

`amf_layout_cpp_check.cpp` compiles the same headers with `/TP` and dumps the C++ layouts. Any
struct whose two builds disagree must be modelled from the **C++** side, because `amfrt64.dll` is
built from the C++ headers. Today exactly one struct diverges — `AMFPropertyInfo`, 112 bytes with
`name` at offset 0 in C but 120 bytes with `name` at 8 in C++, because it has a virtual
destructor. `amd_sys` sidesteps it by never calling `GetPropertyInfo` and probing optional
properties with `HasProperty` instead.

### Running it

Needs the MSVC Build Tools on a Windows box. MinGW `gcc` is the wrong tool: it does not model the
MSVC C++ ABI, and `amfrt64.dll` is MSVC-built.

```bat
cd src-tauri\tools
amf_layout_build.bat
```

That enters the x64 environment via `vcvars64.bat`, builds both programs and writes
`amf_layout_generated.txt` and `amf_layout_cpp_check.txt`. Or by hand:

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cl /nologo /TC /W3 /I ..\resources\amf-headers amf_layout_gen.c /Fe:amf_layout_gen.exe
amf_layout_gen.exe > amf_layout_generated.txt
```

Then paste the sections of `amf_layout_generated.txt` over the corresponding
`abi_*_matches_amf_headers` bodies in
`src-tauri/src/windows_utils/streamer/test/amd_layout.rs`, in order.

**Do not run this from `build.rs`.** The usual dev box for this repo is a macOS 10.15 machine with
no `cl.exe`; the generated Rust is the checked-in product. Regenerate only when the vendored
headers move, and treat a diff in the numbers as a required review item.

Generated artifacts (`*.exe`, `*.obj`, `amf_layout_generated.txt`, `amf_layout_cpp_check.txt`) are
gitignored.

## `nvenc_layout_gen.c` — the NVENC ABI generator

`windows_utils/streamer/nvidia/nvenc_sys` is bindgen output plus a **hand-written** `version.rs`,
and `test/nvenc_layout.rs` used to assert the Rust structs against constants from the same
bindgen run. Self-referential: it caught a hand edit to the Rust and nothing else, and in
particular it could not catch bindings generated against one SDK header while `version.rs`
transcribed the struct revisions of another. A wrong `NV_ENC_*_VER` word is not a compile error —
`nvEncInitializeEncoder` returns `NV_ENC_ERR_INVALID_VERSION`, `build_backend` catches it, and
the NVIDIA path silently degrades to Quick Sync or x264 on a machine with a working NVENC.

`nvenc_layout_gen.c` compiles against the vendored `../resources/nvenc-headers/nvEncodeAPI.h`
(FFmpeg/nv-codec-headers `n12.1.14.1`, MIT) and prints ready-to-paste Rust assertions: the API
and struct version words, sizes and alignments, every field offset the encoder writes or reads,
every `NV_ENCODE_API_FUNCTION_LIST` offset (the driver fills that table, so a wrong offset is a
call through the wrong function pointer), the enum values and capability ids, and the GUIDs —
which bindgen mis-generates (rust-bindgen#1888) and `nvenc_sys/guid.rs` therefore transcribes by
hand.

MSVC, not MinGW: `nvEncodeAPI64.dll` is MSVC-built, and the two compilers do not pack the
`unsigned int x : 1` runs in `NV_ENC_CONFIG_H264` and `NV_ENC_PIC_PARAMS_H264` the same way.

### Running it

```bat
cd src-tauri	ools
nvenc_layout_build.bat
```

That enters the x64 environment via `vcvars64.bat`, builds the program and writes
`nvenc_layout_generated.txt`. Or by hand:

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio2\BuildTools\VC\Auxiliary\Buildcvars64.bat"
cl /nologo /TC /W3 /I ..esources
venc-headers nvenc_layout_gen.c /Fe:nvenc_layout_gen.exe
nvenc_layout_gen.exe > nvenc_layout_generated.txt
```

Then paste the sections over the corresponding `abi_*` bodies in
`src-tauri/src/windows_utils/streamer/test/nvenc_layout.rs`, in order. The generator emits C's
bare enum-constant names; the glob `use` lines at the top of that file are what make the paste
literal. Same rules as the AMF generator: not from `build.rs`, and a diff in the numbers after a
header bump is a required review item.

What neither generator can check is whether the *driver* agrees. Only running the encoder does
that — `test/nvenc_encoder.rs`.

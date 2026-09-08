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

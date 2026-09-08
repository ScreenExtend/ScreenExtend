# AMD AMF H.264 encoder backend — everything needed to implement it with no SDK at build time

## 1. Question

Everything required to write a **production, latency-optimal AMD AMF H.264 encoder backend in
Rust with no SDK at build time**: dynamically load `amfrt64.dll` with `libloading`, resolve only
`AMFInit` and `AMFQueryVersion`, and reach every other entry point through the COM-like C vtables.
Specifically: the exact vtable *order* (the ABI — a wrong slot index is memory corruption), the
struct layouts, the enum values, the complete `AMF_VIDEO_ENCODER_*` property-name catalogue as
wide-string literals, the latency-optimal property set and its ordering rules, the submit/query
model, adapter detection and capability probing, the HEVC/AV1 parallels, and a way to verify the
FFI on a box with no AMD GPU.

The local spec is `src-tauri/src/windows_utils/streamer/amd.md`; this report is its verification
pass. **It contains several factual errors, corrected in §2.9.**

---

## 2. Findings

### 2.1 The SDK: version, license, what to vendor

- Repo: <https://github.com/GPUOpen-LibrariesAndSDKs/AMF>. Header tree used throughout this report
  is `amf/public/include/{core,components}` at `master`, fetched
  2026-09-08 from `raw.githubusercontent.com`.
- **Version on master: `1.5.2.0`.** From `core/Version.h`:
  ```c
  #define AMF_VERSION_MAJOR       1
  #define AMF_VERSION_MINOR       5
  #define AMF_VERSION_RELEASE     2
  #define AMF_VERSION_BUILD_NUM   0
  #define AMF_FULL_VERSION AMF_MAKE_FULL_VERSION(AMF_VERSION_MAJOR, AMF_VERSION_MINOR, AMF_VERSION_RELEASE, AMF_VERSION_BUILD_NUM)
  ```
  `AMF_FULL_VERSION` for 1.5.2.0 is **`281496451678208`** = `0x0001_0005_0002_0000` (measured, §7).
- **Version packing** (`core/Version.h`, verbatim):
  ```c
  #define AMF_MAKE_FULL_VERSION(VERSION_MAJOR, VERSION_MINOR, VERSION_RELEASE, VERSION_BUILD_NUM) \
      ( ((amf_uint64)(VERSION_MAJOR) << 48ull) | ((amf_uint64)(VERSION_MINOR) << 32ull) \
      | ((amf_uint64)(VERSION_RELEASE) << 16ull)  | (amf_uint64)(VERSION_BUILD_NUM))

  #define AMF_GET_MAJOR_VERSION(x)      ((x >> 48ull) & 0xFFFF)
  #define AMF_GET_MINOR_VERSION(x)      ((x >> 32ull) & 0xFFFF)
  #define AMF_GET_SUBMINOR_VERSION(x)   ((x >> 16ull) & 0xFFFF)
  #define AMF_GET_BUILD_VERSION(x)      ((x >>  0ull) & 0xFFFF)
  ```
- **AMF version → minimum Radeon driver.** The repo README carries the full table (*"AMF SDK is
  backward compatible with all previous driver versions"*). The rows that matter to us:

  | AMF | Min. Adrenalin/Crimson | Feature landing |
  |---|---|---|
  | 1.5.2 | 26.5.1 | (current master) |
  | 1.5.0 | 25.10.1 | (what OBS builds against) |
  | 1.4.36 | 25.1.1 | |
  | 1.4.35 | 24.9.1 | multi-HW-instance encode, split-frame |
  | 1.4.33 | 24.1.1 | native DX12 encode |
  | **1.4.30** | **23.5.2** | **frame-in → slice/tile output (AVC/HEVC/AV1)** |
  | 1.4.29 | 23.1.2 | SmartAccess Video |
  | 1.4.28 | 22.12.1 | AV1 encode |
  | 1.4.23 | 21.12.1 | Auto-LTR, extra usage presets, statistics feedback |
  | 1.4.21 | 21.10.1 | PSNR/SSIM, QVBR, **LTR mode** |
  | 1.4.9 | 18.8.1 | |
  | 1.4.4 | Crimson 17.7.2 | |
  | **1.4** | **Crimson 17.1.1** | the practical floor for AMF at all |

  The exact first driver to ship `amfrt64.dll` could not be confirmed (the AMF wiki's driver pages do
  not render); AMF 1.1/1.2 predate the public repo. **Crimson 17.1.1 is the working floor.**
- **License: MIT**, plus a non-license "Notice Regarding Standards" preamble in every header
  disclaiming any patent grant for MPEG-2/MPEG-4/AVC/HEVC/AAC/VC-1/MP3 ("you will pay any royalties
  due for such third party technologies"). The MIT grant itself is unrestricted:
  *"Permission is hereby granted, free of charge, to any person obtaining a copy of this software …
  to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies"*. Vendoring the
  headers into `src-tauri/` is fine; keep the full notice block at the top of each vendored file and
  record provenance the way `resources/PROVENANCE.md` does. We do **not** redistribute `amfrt64.dll`
  — it ships with the Radeon driver.

**Headers to vendor** (28 files; this is the transitive closure — verified by compiling
`#include "core/Factory.h"` with nothing else on the include path):

```
core/       Factory.h Platform.h Version.h Result.h Interface.h Variant.h
            PropertyStorage.h PropertyStorageEx.h Data.h Plane.h Surface.h
            Buffer.h AudioBuffer.h Context.h Compute.h ComputeFactory.h
            Debug.h Trace.h
components/ Component.h ComponentCaps.h VideoEncoderVCE.h VideoEncoderHEVC.h
            VideoEncoderAV1.h VideoConverter.h ColorSpace.h PreAnalysis.h
```

`Factory.h` pulls in `Context.h`, `Debug.h`, `Trace.h`, `Compute.h` and `../components/Component.h`;
`Context.h` pulls `Buffer.h`, `AudioBuffer.h`, `Surface.h`, `Compute.h`, `ComputeFactory.h`;
`VideoEncoderVCE.h` pulls `ColorSpace.h` and `PreAnalysis.h`. Nothing outside this set is referenced.

### 2.2 Entry points and calling convention

`core/Factory.h`, verbatim:

```c
#define AMF_INIT_FUNCTION_NAME             "AMFInit"
#define AMF_QUERY_VERSION_FUNCTION_NAME    "AMFQueryVersion"

typedef AMF_RESULT (AMF_CDECL_CALL *AMFInit_Fn)(amf_uint64 version, AMFFactory **ppFactory);
typedef AMF_RESULT (AMF_CDECL_CALL *AMFQueryVersion_Fn)(amf_uint64 *pVersion);

#if defined(_WIN32)
    #if defined(_M_AMD64)
        #define AMF_DLL_NAME    L"amfrt64.dll"
        #define AMF_DLL_NAMEA   "amfrt64.dll"
        #define AMFLITE_DLL_NAME    L"amfrtlt64.dll"
        #define AMFLITE_DLL_NAMEA   "amfrtlt64.dll"
```

**Calling conventions** (`core/Platform.h`):

```c
#if defined(_WIN32)
    #define AMF_STD_CALL            __stdcall
    #define AMF_CDECL_CALL          __cdecl
    #define AMF_FAST_CALL           __fastcall
#else // Linux and Mac
    #define AMF_STD_CALL
    #define AMF_CDECL_CALL
#endif
```

The **two exported functions are `__cdecl`**; **every vtable method is `__stdcall`**. On x64 Windows
both collapse to the single Microsoft x64 calling convention, so `extern "system"` (== `extern "C"`
on x64) is correct for all of them. Document it anyway: the legacy `obs-amd-encoder` plugin carried a
32-bit-only driver blacklist with the comment `// Older drivers crash due to using the wrong calling
standard.` — on x86 the distinction is real, and we only ship x64.

`amfrtlt64.dll` ("AMF Lite") is the alternate runtime name; not needed — the full runtime is what the
Radeon driver installs.

### 2.3 The C ABI — vtable order, verbatim

These are the `#else /* !__cplusplus */` branches of the headers, i.e. exactly what we bind.
**Slot indices below were measured**, not read off: `amf_layout_gen.c` (§7) compiles the real headers
in C mode and prints `offsetof(Vtbl, member) / sizeof(void*)`.

#### `AMFInterfaceVtbl` — `core/Interface.h` (3 slots)

```c
typedef struct AMFInterfaceVtbl
{
    amf_long        (AMF_STD_CALL *Acquire)(AMFInterface* pThis);                                    // 0
    amf_long        (AMF_STD_CALL *Release)(AMFInterface* pThis);                                    // 1
    enum AMF_RESULT (AMF_STD_CALL *QueryInterface)(AMFInterface* pThis, const struct AMFGuid *interfaceID, void** ppInterface); // 2
} AMFInterfaceVtbl;
```

Every AMF interface except `AMFFactory` and `AMFTrace` starts with these three slots.

#### `AMFPropertyStorageVtbl` — `core/PropertyStorage.h` (13 slots)

```c
    // AMFInterface interface
    amf_long        (*Acquire)(...);                                                     // 0
    amf_long        (*Release)(...);                                                     // 1
    enum AMF_RESULT (*QueryInterface)(..., const struct AMFGuid *interfaceID, void** ppInterface); // 2
    // AMFPropertyStorage interface
    AMF_RESULT      (*SetProperty)(..., const wchar_t* name, AMFVariantStruct value);    // 3
    AMF_RESULT      (*GetProperty)(..., const wchar_t* name, AMFVariantStruct* pValue);  // 4
    amf_bool        (*HasProperty)(..., const wchar_t* name);                            // 5
    amf_size        (*GetPropertyCount)(...);                                            // 6
    AMF_RESULT      (*GetPropertyAt)(..., amf_size index, wchar_t* name, amf_size nameSize, AMFVariantStruct* pValue); // 7
    AMF_RESULT      (*Clear)(...);                                                       // 8
    AMF_RESULT      (*AddTo)(..., AMFPropertyStorage* pDest, amf_bool overwrite, amf_bool deep); // 9
    AMF_RESULT      (*CopyTo)(..., AMFPropertyStorage* pDest, amf_bool deep);            // 10
    void            (*AddObserver)(..., AMFPropertyStorageObserver* pObserver);          // 11
    void            (*RemoveObserver)(..., AMFPropertyStorageObserver* pObserver);       // 12
```

Note **`SetProperty` takes `AMFVariantStruct` BY VALUE** (24 bytes). On x64 Windows a 24-byte
aggregate is passed by hidden reference (caller allocates, passes a pointer), which Rust's
`extern "system" fn(..., value: AMFVariantStruct)` with `#[repr(C)]` reproduces correctly.
`GetProperty` takes a pointer.

#### `AMFPropertyStorageExVtbl` — `core/PropertyStorageEx.h` (17 slots)

Slots 0–12 identical to `AMFPropertyStorageVtbl`, then:

```c
    amf_size   (*GetPropertiesInfoCount)(AMFPropertyStorageEx* pThis);                                  // 13
    AMF_RESULT (*GetPropertyInfoAt)(..., amf_size index, const AMFPropertyInfo** ppInfo);               // 14
    AMF_RESULT (*GetPropertyInfo)(..., const wchar_t* name, const AMFPropertyInfo** ppInfo);            // 15
    AMF_RESULT (*ValidateProperty)(..., const wchar_t* name, AMFVariantStruct value, AMFVariantStruct* pOutValidated); // 16
```

The C++ side declares `GetPropertyInfo` twice (by index and by name); the C branch renames the index
overload **`GetPropertyInfoAt`**. Same slot, different name.

#### `AMFDataVtbl` — `core/Data.h` (23 slots)

Slots 0–12 = `AMFPropertyStorage`, then:

```c
    AMF_MEMORY_TYPE (*GetMemoryType)(AMFData* pThis);                                    // 13
    AMF_RESULT      (*Duplicate)(..., AMF_MEMORY_TYPE type, AMFData** ppData);           // 14
    AMF_RESULT      (*Convert)(..., AMF_MEMORY_TYPE type);                               // 15
    AMF_RESULT      (*Interop)(..., AMF_MEMORY_TYPE type);                               // 16
    AMF_DATA_TYPE   (*GetDataType)(...);                                                 // 17
    amf_bool        (*IsReusable)(...);                                                  // 18
    void            (*SetPts)(..., amf_pts pts);                                         // 19
    amf_pts         (*GetPts)(...);                                                      // 20
    void            (*SetDuration)(..., amf_pts duration);                               // 21
    amf_pts         (*GetDuration)(...);                                                 // 22
```

`Convert` copies through host memory if it must; `Interop` refuses rather than copying. On the hot
path we call neither.

#### `AMFSurfaceVtbl` — `core/Surface.h` (33 slots)

Slots 0–22 = `AMFData`, then:

```c
    AMF_SURFACE_FORMAT (*GetFormat)(AMFSurface* pThis);                                  // 23
    amf_size           (*GetPlanesCount)(...);                                           // 24
    AMFPlane*          (*GetPlaneAt)(..., amf_size index);                               // 25
    AMFPlane*          (*GetPlane)(..., AMF_PLANE_TYPE type);                            // 26
    AMF_FRAME_TYPE     (*GetFrameType)(...);                                             // 27
    void               (*SetFrameType)(..., AMF_FRAME_TYPE type);                        // 28
    AMF_RESULT         (*SetCrop)(..., amf_int32 x, amf_int32 y, amf_int32 width, amf_int32 height); // 29
    AMF_RESULT         (*CopySurfaceRegion)(..., AMFSurface* pDest, amf_int32 dstX, amf_int32 dstY, amf_int32 srcX, amf_int32 srcY, amf_int32 width, amf_int32 height); // 30
    // Observer management
    void               (*AddObserver_Surface)(..., AMFSurfaceObserver* pObserver);       // 31
    void               (*RemoveObserver_Surface)(..., AMFSurfaceObserver* pObserver);     // 32
```

**Trap:** slots 11/12 are `AddObserver`/`RemoveObserver` taking an `AMFPropertyStorageObserver*`;
slots 31/32 are `AddObserver_Surface`/`RemoveObserver_Surface` taking an `AMFSurfaceObserver*`. The
C++ header names both pairs `AddObserver` (hence its `-Woverloaded-virtual` pragmas). Calling 11
where you meant 31 passes a mistyped vtable to the runtime.

`AMFSurface1Vtbl` = the above 33 plus `Map` (33) and `Unmap` (34); IID
`0xd236db3e,0x6945,0x4158,{ad,29,4a,8a,38,6b,fc,61}`. We do not need it (no CPU access on the fast path).

#### `AMFBufferVtbl` — `core/Buffer.h` (28 slots)

Slots 0–22 = `AMFData`, then:

```c
    AMF_RESULT (*SetSize)(AMFBuffer* pThis, amf_size newSize);                           // 23
    amf_size   (*GetSize)(AMFBuffer* pThis);                                             // 24
    void*      (*GetNative)(AMFBuffer* pThis);                                           // 25
    void       (*AddObserver_Buffer)(..., AMFBufferObserver* pObserver);                 // 26
    void       (*RemoveObserver_Buffer)(..., AMFBufferObserver* pObserver);              // 27
```

`GetNative` + `GetSize` on slots 25/24 is the **entire hot-path output read**: the encoder hands back
an `AMFBuffer` whose native pointer is host memory containing the Annex-B access unit.

`AMFBuffer1Vtbl` adds `GetOffset` (28), `SetOffset` (29), `Map` (30), `Unmap` (31).

#### `AMFPlaneVtbl` — `core/Plane.h` (13 slots)

```c
    // AMFInterface: Acquire 0, Release 1, QueryInterface 2
    AMF_PLANE_TYPE (*GetType)(AMFPlane* pThis);                                          // 3
    void*          (*GetNative)(...);                                                    // 4
    amf_int32      (*GetPixelSizeInBytes)(...);                                          // 5
    amf_int32      (*GetOffsetX)(...);                                                   // 6
    amf_int32      (*GetOffsetY)(...);                                                   // 7
    amf_int32      (*GetWidth)(...);                                                     // 8
    amf_int32      (*GetHeight)(...);                                                    // 9
    amf_int32      (*GetHPitch)(...);                                                    // 10
    amf_int32      (*GetVPitch)(...);                                                    // 11
    amf_bool       (*IsTiled)(...);                                                      // 12
```

Only needed for the CPU-upload fallback. **`GetPlaneAt`/`GetPlane` return a borrowed pointer** —
`core/Surface.h` says *"do not store planes outside. should be used together with Surface."*

#### `AMFComponentVtbl` — `components/Component.h` (28 slots)

Slots 0–16 = `AMFPropertyStorageEx`, then:

```c
    AMF_RESULT  (*Init)(AMFComponent* pThis, AMF_SURFACE_FORMAT format, amf_int32 width, amf_int32 height); // 17
    AMF_RESULT  (*ReInit)(..., amf_int32 width, amf_int32 height);                       // 18
    AMF_RESULT  (*Terminate)(...);                                                       // 19
    AMF_RESULT  (*Drain)(...);                                                           // 20
    AMF_RESULT  (*Flush)(...);                                                           // 21
    AMF_RESULT  (*SubmitInput)(..., AMFData* pData);                                     // 22
    AMF_RESULT  (*QueryOutput)(..., AMFData** ppData);                                   // 23
    AMFContext* (*GetContext)(...);                                                      // 24
    AMF_RESULT  (*SetOutputDataAllocatorCB)(..., AMFDataAllocatorCB* callback);          // 25
    AMF_RESULT  (*GetCaps)(..., AMFCaps** ppCaps);                                       // 26
    AMF_RESULT  (*Optimize)(..., AMFComponentOptimizationCallback* pCallback);           // 27
```

`AMFComponentExVtbl` extends to 32 slots (`GetInputCount` 28, `GetOutputCount` 29, `GetInput` 30,
`GetOutput` 31). Unused for a single-stream encoder.

#### `AMFContextVtbl` — `core/Context.h` (55 slots)

Slots 0–12 = `AMFPropertyStorage`. The full tail, in order:

| Slot | Member |
|---|---|
| 13 | `Terminate` |
| 14 | `InitDX9` |
| 15 | `GetDX9Device` |
| 16 | `LockDX9` |
| 17 | `UnlockDX9` |
| **18** | **`InitDX11(void* pDX11Device, AMF_DX_VERSION dxVersionRequired)`** |
| 19 | `GetDX11Device(AMF_DX_VERSION)` |
| 20 | `LockDX11` |
| 21 | `UnlockDX11` |
| 22 | `InitOpenCL` |
| 23 | `GetOpenCLContext` |
| 24 | `GetOpenCLCommandQueue` |
| 25 | `GetOpenCLDeviceID` |
| 26 | `GetOpenCLComputeFactory` |
| 27 | `InitOpenCLEx` |
| 28 | `LockOpenCL` |
| 29 | `UnlockOpenCL` |
| 30 | `InitOpenGL` |
| 31 | `GetOpenGLContext` |
| 32 | `GetOpenGLDrawable` |
| 33 | `LockOpenGL` |
| 34 | `UnlockOpenGL` |
| 35 | `InitXV` |
| 36 | `GetXVDevice` |
| 37 | `LockXV` |
| 38 | `UnlockXV` |
| 39 | `InitGralloc` |
| 40 | `GetGrallocDevice` |
| 41 | `LockGralloc` |
| 42 | `UnlockGralloc` |
| 43 | `AllocBuffer` |
| 44 | `AllocSurface` |
| 45 | `AllocAudioBuffer` |
| 46 | `CreateBufferFromHostNative` |
| 47 | `CreateSurfaceFromHostNative` |
| 48 | `CreateSurfaceFromDX9Native` |
| **49** | **`CreateSurfaceFromDX11Native(void* pDX11Surface, AMFSurface** ppSurface, AMFSurfaceObserver* pObserver)`** |
| 50 | `CreateSurfaceFromOpenGLNative` |
| 51 | `CreateSurfaceFromGrallocNative` |
| 52 | `CreateSurfaceFromOpenCLNative` |
| 53 | `CreateBufferFromOpenCLNative` |
| 54 | `GetCompute` |

**Do not derive these by counting the header by hand** — the OpenCL/OpenGL/XV/Gralloc blocks make it
easy to be off by one, and slot 49 (`CreateSurfaceFromDX11Native`) is the hot path. Generate them.

`AMFContext1Vtbl` (65 slots) has slots 0–54 **identical** to `AMFContext`, then:

| Slot | Member |
|---|---|
| 55 | `CreateBufferFromDX11Native` |
| 56 | `AllocBufferEx` |
| 57 | `AllocSurfaceEx(type, format, w, h, AMF_SURFACE_USAGE, AMF_MEMORY_CPU_ACCESS, AMFSurface**)` |
| 58 | `InitVulkan` |
| 59 | `GetVulkanDevice` |
| 60 | `LockVulkan` |
| 61 | `UnlockVulkan` |
| 62 | `CreateSurfaceFromVulkanNative` |
| 63 | `CreateBufferFromVulkanNative` |
| 64 | `GetVulkanDeviceExtensions` |

`AMFContext2Vtbl` = 65 + `InitDX12` (65), `GetDX12Device` (66), `LockDX12` (67), `UnlockDX12` (68),
`CreateSurfaceFromDX12Native` (69), `CreateBufferFromDX12Native` (70).

#### `AMFCapsVtbl` — `components/ComponentCaps.h` (16 slots)

Slots 0–12 = `AMFPropertyStorage` (so caps are read as **properties**, see §2.8), then:

```c
    AMF_ACCELERATION_TYPE (*GetAccelerationType)(AMFCaps* pThis);                        // 13
    AMF_RESULT            (*GetInputCaps)(..., AMFIOCaps** input);                       // 14
    AMF_RESULT            (*GetOutputCaps)(..., AMFIOCaps** output);                     // 15
```

#### `AMFIOCapsVtbl` — `components/ComponentCaps.h` (11 slots)

```c
    // AMFInterface: Acquire 0, Release 1, QueryInterface 2
    void       (*GetWidthRange)(AMFIOCaps* pThis, amf_int32* minWidth, amf_int32* maxWidth);   // 3
    void       (*GetHeightRange)(..., amf_int32* minHeight, amf_int32* maxHeight);             // 4
    amf_int32  (*GetVertAlign)(...);                                                           // 5
    amf_int32  (*GetNumOfFormats)(...);                                                        // 6
    AMF_RESULT (*GetFormatAt)(..., amf_int32 index, AMF_SURFACE_FORMAT* format, amf_bool* native); // 7
    amf_int32  (*GetNumOfMemoryTypes)(...);                                                    // 8
    AMF_RESULT (*GetMemoryTypeAt)(..., amf_int32 index, AMF_MEMORY_TYPE* memType, amf_bool* native); // 9
    amf_bool   (*IsInterlacedSupported)(...);                                                  // 10
```

`GetFormatAt`'s `native` out-flag is how you learn whether the encoder takes BGRA natively or will
insert a conversion (§2.7).

#### `AMFFactoryVtbl` — `core/Factory.h` (7 slots)

```c
typedef struct AMFFactoryVtbl
{
    AMF_RESULT     (AMF_STD_CALL *CreateContext)(AMFFactory* pThis, AMFContext** ppContext);                        // 0
    AMF_RESULT     (AMF_STD_CALL *CreateComponent)(AMFFactory* pThis, AMFContext* pContext, const wchar_t* id, AMFComponent** ppComponent); // 1
    AMF_RESULT     (AMF_STD_CALL *SetCacheFolder)(AMFFactory* pThis, const wchar_t* path);                          // 2
    const wchar_t* (AMF_STD_CALL *GetCacheFolder)(AMFFactory* pThis);                                               // 3
    AMF_RESULT     (AMF_STD_CALL *GetDebug)(AMFFactory* pThis, AMFDebug** ppDebug);                                 // 4
    AMF_RESULT     (AMF_STD_CALL *GetTrace)(AMFFactory* pThis, AMFTrace** ppTrace);                                 // 5
    AMF_RESULT     (AMF_STD_CALL *GetPrograms)(AMFFactory* pThis, AMFPrograms** ppPrograms);                        // 6
} AMFFactoryVtbl;
```

**`AMFFactory` is NOT an `AMFInterface`** — no `Acquire`/`Release`/`QueryInterface`, `CreateContext`
is slot **0**. It is a singleton owned by the DLL; never Release it.

#### `AMFTraceVtbl` — `core/Trace.h` (25 slots)

**Also not an `AMFInterface`** — starts directly at `TraceW`:

```c
    void               (*TraceW)(AMFTrace* pThis, const wchar_t* src_path, amf_int32 line, amf_int32 level, const wchar_t* scope, amf_int32 countArgs, const wchar_t* format, ...); // 0  (VARIADIC)
    void               (*Trace)(..., const wchar_t* message, va_list* pArglist);          // 1  (va_list)
    amf_int32          (*SetGlobalLevel)(AMFTrace* pThis, amf_int32 level);               // 2
    amf_int32          (*GetGlobalLevel)(...);                                            // 3
    amf_bool           (*EnableWriter)(..., const wchar_t* writerID, amf_bool enable);    // 4
    amf_bool           (*WriterEnabled)(..., const wchar_t* writerID);                    // 5
    AMF_RESULT         (*TraceEnableAsync)(..., amf_bool enable);                         // 6
    AMF_RESULT         (*TraceFlush)(...);                                                // 7
    AMF_RESULT         (*SetPath)(..., const wchar_t* path);                              // 8
    AMF_RESULT         (*GetPath)(..., wchar_t* path, amf_size* pSize);                   // 9
    amf_int32          (*SetWriterLevel)(..., const wchar_t* writerID, amf_int32 level);   // 10
    amf_int32          (*GetWriterLevel)(..., const wchar_t* writerID);                    // 11
    amf_int32          (*SetWriterLevelForScope)(..., const wchar_t* writerID, const wchar_t* scope, amf_int32 level); // 12
    amf_int32          (*GetWriterLevelForScope)(..., const wchar_t* writerID, const wchar_t* scope); // 13
    amf_int32          (*GetIndentation)(...);                                            // 14
    void               (*Indent)(..., amf_int32 addIndent);                               // 15
    void               (*RegisterWriter)(..., const wchar_t* writerID, AMFTraceWriter* pWriter, amf_bool enable); // 16
    void               (*UnregisterWriter)(..., const wchar_t* writerID);                 // 17
    const wchar_t*     (*GetResultText)(AMFTrace* pThis, AMF_RESULT res);                 // 18
    const wchar_t*     (*SurfaceGetFormatName)(..., const AMF_SURFACE_FORMAT eSurfaceFormat); // 19
    AMF_SURFACE_FORMAT (*SurfaceGetFormatByName)(..., const wchar_t* name);               // 20
    const wchar_t*     (*GetMemoryTypeName)(..., const AMF_MEMORY_TYPE memoryType);       // 21
    AMF_MEMORY_TYPE    (*GetMemoryTypeByName)(..., const wchar_t* name);                  // 22
    const wchar_t*     (*GetSampleFormatName)(..., const AMF_AUDIO_FORMAT eFormat);       // 23
    AMF_AUDIO_FORMAT   (*GetSampleFormatByName)(..., const wchar_t* name);                // 24
```

We bind exactly two of these: **`GetResultText` (slot 18)** to turn an `AMF_RESULT` into a readable
string for `teprintln!`, and `SetGlobalLevel` (slot 2) / `EnableWriter` (slot 4) to *silence* AMF's
default console writer, which otherwise prints straight to stdout and bypasses `logbus`. Do not bind
the variadic slots 0/1.

Trace constants:

```c
#define AMF_TRACE_ERROR 0 / WARNING 1 / INFO 2 (sdk default) / DEBUG 3 / TRACE 4 / TEST 5 / NOLOG 100
#define AMF_TRACE_WRITER_CONSOLE      L"Console"
#define AMF_TRACE_WRITER_DEBUG_OUTPUT L"DebugOutput"
#define AMF_TRACE_WRITER_FILE         L"File"
```

#### IID values (`AMF_DECLARE_IID`, C branch)

Only needed for `QueryInterface`. The C-branch macro emits `AMFGuid IID_<Name>(void)`:

| Interface | GUID |
|---|---|
| `AMFInterface` | `0x9d872f34, 0x90dc, 0x4b93, b6 b2 6c a3 7c 85 25 db` |
| `AMFPropertyStorage` | `0xc7cec05b, 0xcfb9, 0x48af, ac e3 f6 8d f8 39 5f e3` |
| `AMFPropertyStorageEx` | `0x16b8958d, 0xe943, 0x4a33, a3 5a 88 5a d8 28 f2 67` |
| `AMFData` | `0xa1159bf6, 0x9104, 0x4107, 8e aa c5 3d 5d ba c5 11` |
| `AMFSurface` | `0x3075dbe3, 0x8718, 0x4cfa, 86 fb 21 14 c0 a5 a4 51` |
| `AMFSurface1` | `0xd236db3e, 0x6945, 0x4158, ad 29 4a 8a 38 6b fc 61` |
| `AMFBuffer` | `0xb04b7248, 0xb6f0, 0x4321, b6 91 ba a4 74 0f 9f cb` |
| `AMFBuffer1` | `0x8051a17d, 0x462d, 0x4f09, b8 42 3c 72 f7 a8 5b 24` |
| `AMFPlane` | `0xbede1aa6, 0xd8fa, 0x4625, 94 65 6c 82 c4 37 71 2e` |
| `AMFContext` | `0xa76a13f0, 0xd80e, 0x4fcc, b5 08 65 d0 b5 2e d9 ee` |
| **`AMFContext1`** | **`0xd9e9f868, 0x6220, 0x44c6, a2 2f 7c d6 da c6 86 46`** |
| `AMFContext2` (C++) | `0x726241d3, 0xbd46, 0x4e90, 99 68 93 e0 7e a2 98 4d` |
| `AMFComponent` | `0x8b51e5e4, 0x455d, 0x4034, a7 46 de 1b ed c3 c4 06` |
| `AMFComponentEx` | `0xfda792af, 0x8712, 0x44df, 8e a0 df fa ad 2c 80 93` |
| `AMFDataAllocatorCB` | `0x4bf46198, 0x8b7b, 0x49d0, aa 72 48 d4 07 ce 24 c5` |
| `AMFInput` | `0x1181eee7, 0x95f2, 0x434a, 9b 96 ea 55 0a a7 84 89` |
| `AMFOutput` | `0x86a8a037, 0x912c, 0x4698, b0 46 07 5a 1f ac 6b 97` |

> **Header bug — do not copy it.** In the **C branch** of `core/Context.h`, `AMFContext2`'s
> `AMF_DECLARE_IID` is a copy-paste of `AMFContext1`'s (`0xd9e9f868…`), while the C++ class at
> `core/Context.h:168` declares the correct `0x726241d3…`. Use the C++ value if you ever need
> `AMFContext2` (we don't — DX12 is not on our path).

> `AMFCaps`, `AMFIOCaps`, `AMFFactory` and `AMFTrace` have **no IID in either branch**. You cannot
> `QueryInterface` to them; `AMFCaps` comes only from `AMFComponent::GetCaps`, `AMFIOCaps` only from
> `AMFCaps::Get{Input,Output}Caps`, and the other two from `AMFInit`/`GetTrace`.

### 2.4 Types, structs and their measured layout

`core/Platform.h` scalar map (all `stdint.h`-backed):

| AMF | C | Rust |
|---|---|---|
| `amf_int8/16/32/64` | `int8_t…int64_t` | `i8/i16/i32/i64` |
| `amf_uint8/16/32/64` | `uint8_t…uint64_t` | `u8/u16/u32/u64` |
| `amf_size` | `size_t` | `usize` |
| `amf_handle` | `void*` | `*mut c_void` |
| `amf_double` / `amf_float` | `double` / `float` | `f64` / `f32` |
| **`amf_bool`** | **`amf_uint8` in C**, `bool` in C++ | **`u8`** (1 byte either way on MSVC) |
| `amf_long` | `long` | **`i32`** (LLP64: `long` is 32-bit on Windows, 64-bit on Linux) |
| `amf_int` / `amf_uint` | `int` / `unsigned int` | `i32` / `u32` |
| **`amf_pts`** | `amf_int64`, **units of 100 ns** | `i64` |
| `amf_flags` | `amf_uint32` | `u32` |

`#define AMF_SECOND 10000000L` — 1 second in `amf_pts` ticks. `AMF_MILLISECOND` = 10 000,
`AMF_MICROSECOND` = 10.

> `amf_long` returned by `Acquire`/`Release` is **`i32` on Windows**. Typing it `i64` reads 4 bytes of
> adjacent register garbage.

**Measured layout** (`cl.exe 19.44.35227` x64, C mode — full output in §7):

```
AMFGuid                 size 16, align 4   (data1 @0, data2 @4, data3 @6, data41..data48 @8..15)
AMFVariantStruct        size 24, align 8   (type @0 [4 bytes + 4 pad], union @8)
AMFSize / AMFRate / AMFRatio / AMFPoint     size 8    (two i32/u32)
AMFRect                 size 16            (left, top, right, bottom : amf_int32)
AMFColor                size 4             (packed r,g,b,a — inside #pragma pack(1))
AMFEnumDescriptionEntry size 16            (amf_int value; const wchar_t* name)
pointer                 size 8
```

```c
typedef struct AMFSize  { amf_int32 width; amf_int32 height; } AMFSize;
typedef struct AMFRate  { amf_uint32 num; amf_uint32 den; } AMFRate;   // NOTE: unsigned
typedef struct AMFRatio { amf_uint32 num; amf_uint32 den; } AMFRatio;  // NOTE: unsigned
```

`AMFSize` is **signed**, `AMFRate`/`AMFRatio` are **unsigned**. They are otherwise identical and the
variant discriminates them (`AMF_VARIANT_SIZE` = 5, `AMF_VARIANT_RATE` = 7, `AMF_VARIANT_RATIO` = 8);
passing a Rate where a Size is wanted is accepted by the type system and rejected by AMF.

`AMFVariantStruct` (`core/Variant.h`) as Rust:

```rust
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AMFVariantStruct {
    pub var_type: AMF_VARIANT_TYPE,   // i32 discriminant, offset 0
    _pad: u32,                        // offset 4 — REQUIRED: the union is 8-aligned
    pub value: AMFVariantValue,       // offset 8, size 16
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union AMFVariantValue {
    pub bool_value: u8,
    pub int64_value: i64,
    pub double_value: f64,
    pub float_value: f32,
    pub string_value: *mut c_char,
    pub wstring_value: *mut u16,
    pub p_interface: *mut AMFInterface,
    pub rect_value: AMFRect,          // 16 bytes — the widest member
    pub size_value: AMFSize,
    pub point_value: AMFPoint,
    pub rate_value: AMFRate,
    pub ratio_value: AMFRatio,
    pub color_value: AMFColor,
    pub float_size_value: AMFFloatSize,
    pub float_point2d_value: AMFFloatPoint2D,
    pub float_point3d_value: AMFFloatPoint3D,
    pub float_vector4d_value: AMFFloatVector4D, // 16 bytes
}
```

`AMFVariantInit`/`AMFVariantClear`/`AMFVariantAssign*` are `static AMF_INLINE` — **header-only, not
exported from the DLL.** We must reimplement them. For our use that is trivial: every property we set
is `Bool`, `Int64`, `Size` or `Rate`, none of which allocate, so "init" is `zeroed()` and "clear" is
a no-op. Only `AMF_VARIANT_STRING`/`WSTRING`/`INTERFACE` need cleanup — string variants are freed
with `CoTaskMemFree` on Windows (`amf_variant_free` in `Platform.h` is `CoTaskMemFree`), and
interface variants need a `Release`. We read no string properties on the hot path; if we ever read
one (`GetPropertyAt` for diagnostics) it must be `CoTaskMemFree`d.

> **`AMFPropertyInfo` is a C/C++ ABI trap — measured.**
> In the **C** branch it is a plain struct: `size 112`, `name @0`, `desc @8`, `type @16`,
> `contentType @20`, `defaultValue @24`, `minValue @48`, `maxValue @72`, `accessType @96`,
> `pEnumDescription @104`.
> In **C++** it gains `virtual ~AMFPropertyInfo()`, so it grows a vptr: `size 120`, `name @8`,
> `type @24`, `accessType @104`.
> **`amfrt64.dll` is compiled from the C++ headers, so the runtime layout is the 120-byte one.**
> A binding that mirrors the C branch and calls `GetPropertyInfo` (slot 15) reads every field one
> pointer early. Either model `AMFPropertyInfo` with a leading `_vptr: *const c_void` and
> `size == 120`, or — better — **do not call `GetPropertyInfo` at all**; use `HasProperty` (slot 5)
> to probe optional properties instead. Every other struct we touch (`AMFVariantStruct`, `AMFGuid`,
> `AMFSize`, `AMFRate`, `AMFRect`) is identical in both branches.

`AMF_PROPERTY_ACCESS_TYPE` (only meaningful if you do parse `AMFPropertyInfo`):

```c
AMF_PROPERTY_ACCESS_PRIVATE = 0, READ = 0x1, WRITE = 0x2, READ_WRITE = 0x3,
WRITE_RUNTIME = 0x4, FULL = 0xFF, NON_PERSISTANT = 0x4000, INVALID = 0x8000
```

`AllowedChangeInRuntime()` is `(accessType & AMF_PROPERTY_ACCESS_WRITE_RUNTIME) != 0` — that bit is
the machine-readable answer to "is this property dynamic?" (§2.6).

### 2.5 Enum values (all measured, §7)

**`AMF_RESULT`** (`core/Result.h`) — a plain sequential C enum with no explicit values, so the numbers
below are positional and must be regenerated if AMD ever inserts a member:

| Value | Name | | Value | Name |
|---|---|---|---|---|
| 0 | `AMF_OK` | | 20 | `AMF_GLX_FAILED` |
| 1 | `AMF_FAIL` | | 21 | `AMF_XV_FAILED` |
| 2 | `AMF_UNEXPECTED` | | 22 | `AMF_ALSA_FAILED` |
| 3 | `AMF_ACCESS_DENIED` | | **23** | **`AMF_EOF`** |
| 4 | `AMF_INVALID_ARG` | | **24** | **`AMF_REPEAT`** |
| 5 | `AMF_OUT_OF_RANGE` | | **25** | **`AMF_INPUT_FULL`** |
| 6 | `AMF_OUT_OF_MEMORY` | | 26 | `AMF_RESOLUTION_CHANGED` |
| 7 | `AMF_INVALID_POINTER` | | 27 | `AMF_RESOLUTION_UPDATED` |
| 8 | `AMF_NO_INTERFACE` | | 28 | `AMF_INVALID_DATA_TYPE` |
| 9 | `AMF_NOT_IMPLEMENTED` | | 29 | `AMF_INVALID_RESOLUTION` |
| 10 | `AMF_NOT_SUPPORTED` | | 30 | `AMF_CODEC_NOT_SUPPORTED` |
| 11 | `AMF_NOT_FOUND` | | 31 | `AMF_SURFACE_FORMAT_NOT_SUPPORTED` |
| 12 | `AMF_ALREADY_INITIALIZED` | | 32 | `AMF_SURFACE_MUST_BE_SHARED` |
| 13 | `AMF_NOT_INITIALIZED` | | 33..35 | decoder errors |
| 14 | `AMF_INVALID_FORMAT` | | **36** | **`AMF_ENCODER_NOT_PRESENT`** |
| 15 | `AMF_WRONG_STATE` | | 37..43 | DEM / TAN |
| 16 | `AMF_FILE_NOT_OPEN` | | **44** | **`AMF_NEED_MORE_INPUT`** |
| 17 | `AMF_NO_DEVICE` | | 45 | `AMF_VULKAN_FAILED` |
| 18 | `AMF_DIRECTX_FAILED` | | | |
| 19 | `AMF_OPENCL_FAILED` | | | |

Header comments, verbatim: `AMF_INPUT_FULL` — *"returned by AMFComponent::SubmitInput if input queue
is full"*; `AMF_RESOLUTION_CHANGED` — *"resolution changed client needs to Drain/Terminate/Init"*;
`AMF_SURFACE_MUST_BE_SHARED` — *"surface should be shared (DX11: (MiscFlags &
D3D11_RESOURCE_MISC_SHARED) == 0, DX9: No shared handle found)"*; `AMF_NEED_MORE_INPUT` — *"returned
by AMFComponent::SubmitInput did not produce a buffer because more input submissions are required."*

**`AMF_SURFACE_FORMAT`** (`core/Surface.h`): `UNKNOWN 0, NV12 1, YV12 2, BGRA 3, ARGB 4, RGBA 5,
GRAY8 6, YUV420P 7, U8V8 8, YUY2 9, P010 10, RGBA_F16 11, UYVY 12, R10G10B10A2 13, Y210 14, AYUV 15,
Y410 16, Y416 17, GRAY32 18, P012 19, P016 20, Y216 21, R16G16 22, R24G8 23, R32 24, R16 25`.

**`AMF_MEMORY_TYPE`** (`core/Data.h`): `UNKNOWN 0, HOST 1, DX9 2, **DX11 3**, OPENCL 4, OPENGL 5,
XV 6, GRALLOC 7, COMPUTE_FOR_DX9 8 (deprecated), COMPUTE_FOR_DX11 9 (deprecated), VULKAN 10, DX12 11`.

**`AMF_DX_VERSION`**: `AMF_DX9 90, AMF_DX9_EX 91, AMF_DX11_0 110, **AMF_DX11_1 111**, AMF_DX12 120`.

**`AMF_DATA_TYPE`**: `AMF_DATA_BUFFER 0, AMF_DATA_SURFACE 1, AMF_DATA_AUDIO_BUFFER 2, AMF_DATA_USER 1000`.

**`AMF_VARIANT_TYPE`**: `EMPTY 0, BOOL 1, INT64 2, DOUBLE 3, RECT 4, SIZE 5, POINT 6, RATE 7,
RATIO 8, COLOR 9, STRING 10 (char*), WSTRING 11 (wchar_t*), INTERFACE 12 (AMFInterface*), FLOAT 13,
FLOAT_SIZE 14, FLOAT_POINT2D 15, FLOAT_POINT3D 16, FLOAT_VECTOR4D 17`.

**`AMF_FRAME_TYPE`**: `AMF_FRAME_UNKNOWN = -1`, `AMF_FRAME_PROGRESSIVE = 0`; the rest are flag
combinations (`STEREO 0x10000000`, `INTERLEAVED 0x01000000`, `FIELD 0x02000000`, `EVEN 0x04000000`,
`ODD 0x08000000`). We always set `AMF_FRAME_PROGRESSIVE`.

**`AMF_PLANE_TYPE`**: `UNKNOWN 0, PACKED 1 (BGRA/YUY2/…), Y 2, UV 3, U 4, V 5`.

**`AMF_ACCELERATION_TYPE`** (`components/ComponentCaps.h`):
`AMF_ACCEL_NOT_SUPPORTED = -1, AMF_ACCEL_HARDWARE = 0, AMF_ACCEL_GPU = 1, AMF_ACCEL_SOFTWARE = 2`.
**`AMF_ACCEL_HARDWARE` is 0, not a truthy "supported" flag** — the "not supported" case is the
negative one. `HARDWARE` = fixed-function VCN/VCE block; `GPU` = shader-based; `SOFTWARE` = CPU.

**`AMF_MEMORY_CPU_ACCESS_BITS`**: `DEFAULT 0x80000000, NONE 0, READ 0x1, WRITE 0x2, LOCAL 0x4,
PINNED 0x8`.
**`AMF_SURFACE_USAGE_BITS`**: `DEFAULT 0x80000000, NONE 0, SHADER_RESOURCE 0x1, RENDER_TARGET 0x2,
UNORDERED_ACCESS 0x4, TRANSFER_SRC 0x8, TRANSFER_DST 0x10, LINEAR 0x20, NOSYNC 0x40,
DECODER_DST 0x80, DECODER_DPB 0x100, NO_TRANSITION 0x200`.

#### AVC encoder enums (`components/VideoEncoderVCE.h`)

```c
#define AMFVideoEncoderVCE_AVC L"AMFVideoEncoderVCE_AVC"
#define AMFVideoEncoderVCE_SVC L"AMFVideoEncoderVCE_SVC"

enum AMF_VIDEO_ENCODER_USAGE_ENUM {
    AMF_VIDEO_ENCODER_USAGE_TRANSCONDING = 0,   // kept for backwards compatability [sic]
    AMF_VIDEO_ENCODER_USAGE_TRANSCODING  = 0,   // fixed typo
    AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY,          // = 1
    AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY,                // = 2
    AMF_VIDEO_ENCODER_USAGE_WEBCAM,                     // = 3
    AMF_VIDEO_ENCODER_USAGE_HIGH_QUALITY,               // = 4
    AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY_HIGH_QUALITY    // = 5
};

enum AMF_VIDEO_ENCODER_PROFILE_ENUM {
    AMF_VIDEO_ENCODER_PROFILE_UNKNOWN = 0,
    AMF_VIDEO_ENCODER_PROFILE_BASELINE = 66,
    AMF_VIDEO_ENCODER_PROFILE_MAIN = 77,
    AMF_VIDEO_ENCODER_PROFILE_HIGH = 100,
    AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE = 256,
    AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_HIGH = 257
};

enum AMF_VIDEO_ENCODER_H264_LEVEL_ENUM {
    AMF_H264_LEVEL__1 = 10, __1_1 = 11, __1_2 = 12, __1_3 = 13,
    __2 = 20, __2_1 = 21, __2_2 = 22, __3 = 30, __3_1 = 31, __3_2 = 32,
    __4 = 40, __4_1 = 41, __4_2 = 42, __5 = 50, __5_1 = 51, __5_2 = 52,
    __6 = 60, __6_1 = 61, __6_2 = 62
};

enum AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_ENUM {
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_UNKNOWN                 = -1,
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP             =  0,
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR,                    //  1
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR,   //  2
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR,//  3
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_QUALITY_VBR,            //  4
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_HIGH_QUALITY_VBR,       //  5
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_HIGH_QUALITY_CBR        //  6
};

enum AMF_VIDEO_ENCODER_QUALITY_PRESET_ENUM {
    AMF_VIDEO_ENCODER_QUALITY_PRESET_BALANCED = 0,
    AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED,          // 1
    AMF_VIDEO_ENCODER_QUALITY_PRESET_QUALITY,        // 2
    AMF_VIDEO_ENCODER_QUALITY_PRESET_HIGH_QUALITY    // 3
};

enum AMF_VIDEO_ENCODER_PICTURE_TYPE_ENUM {
    AMF_VIDEO_ENCODER_PICTURE_TYPE_NONE = 0, SKIP = 1, IDR = 2, I = 3, P = 4, B = 5
};

enum AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_ENUM {  // no explicit values
    AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR = 0, I = 1, P = 2, B = 3
};

enum AMF_VIDEO_ENCODER_PREENCODE_MODE_ENUM {
    AMF_VIDEO_ENCODER_PREENCODE_DISABLED = 0,
    AMF_VIDEO_ENCODER_PREENCODE_ENABLED  = 1
};

enum AMF_VIDEO_ENCODER_CODING_ENUM {
    AMF_VIDEO_ENCODER_UNDEFINED = 0, // BASELINE = CALV; MAIN, HIGH = CABAC
    AMF_VIDEO_ENCODER_CABAC,         // 1
    AMF_VIDEO_ENCODER_CALV           // 2   [sic — CAVLC, spelled CALV]
};

enum AMF_VIDEO_ENCODER_PICTURE_STRUCTURE_ENUM { NONE = 0, FRAME = 1, TOP_FIELD = 2, BOTTOM_FIELD = 3 };
enum AMF_VIDEO_ENCODER_PICTURE_TRANSFER_MODE_ENUM { OFF = 0, ON = 1 };
enum AMF_VIDEO_ENCODER_LTR_MODE_ENUM { RESET_UNUSED = 0, KEEP_UNUSED = 1 };
enum AMF_VIDEO_ENCODER_OUTPUT_MODE_ENUM { FRAME = 0, SLICE = 1 };
enum AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_ENUM { FRAME = 0, SLICE = 1, SLICE_LAST = 2 };
enum AMF_VIDEO_ENCODER_SCANTYPE_ENUM { PROGRESSIVE = 0, INTERLACED = 1 };
```

> **Cross-codec landmines.** The "same" enum has **different numeric values per codec**:
> - `RATE_CONTROL_METHOD`: AVC is `CQP 0, CBR 1, PEAK_VBR 2, LATENCY_VBR 3, QVBR 4`;
>   **HEVC and AV1 are `CQP 0, LATENCY_VBR 1, PEAK_VBR 2, CBR 3, QVBR 4`.** AVC's `CBR` value 1
>   means `LATENCY_CONSTRAINED_VBR` on HEVC.
> - `QUALITY_PRESET`: AVC `BALANCED 0, SPEED 1, QUALITY 2, HIGH_QUALITY 3`;
>   HEVC `QUALITY 0, BALANCED 5, SPEED 10, HIGH_QUALITY 15`;
>   AV1 `HIGH_QUALITY 0, QUALITY 30, BALANCED 70, SPEED 100`.
> - `USAGE`: AVC/HEVC `TRANSCODING 0, ULTRA_LOW_LATENCY 1, LOW_LATENCY 2, WEBCAM 3, HIGH_QUALITY 4,
>   LOW_LATENCY_HIGH_QUALITY 5`; **AV1 `TRANSCODING 0, LOW_LATENCY 1, ULTRA_LOW_LATENCY 2, …`**
>   (1 and 2 are swapped).
> - `PICTURE_TYPE`: AVC has a `B = 5`; HEVC stops at `P = 4`; AV1 uses a different enum entirely
>   (`FORCE_FRAME_TYPE`: `NONE 0, KEY 1, INTRA_ONLY 2, SWITCH 3, SHOW_EXISTING 4`).
>
> Give each codec its own Rust enum type. Do not share one.

### 2.6 The AVC property catalogue — verbatim wide-string names

`components/VideoEncoderVCE.h` groups properties by lifecycle. **These groupings are the header's own
static/dynamic/per-submission contract**, which is the authoritative answer to "what can I change
live?". Reproduced verbatim (name, wide string, type, default):

**Static — settable before `Init()`**

| Constant | Wide string | Type / default |
|---|---|---|
| `AMF_VIDEO_ENCODER_INSTANCE_INDEX` | `L"EncoderInstance"` | `amf_int64`; selected HW instance idx |
| `AMF_VIDEO_ENCODER_FRAMESIZE` | `L"FrameSize"` | `AMFSize`; default 0,0 |
| `AMF_VIDEO_ENCODER_EXTRADATA` | `L"ExtraData"` | `AMFInterface* -> AMFBuffer*`; SPS/PPS in Annex-B — **read-only** |
| `AMF_VIDEO_ENCODER_USAGE` | `L"Usage"` | `AMF_VIDEO_ENCODER_USAGE_ENUM`; **"fully configures parameter set."** |
| `AMF_VIDEO_ENCODER_PROFILE` | `L"Profile"` | default `PROFILE_MAIN` |
| `AMF_VIDEO_ENCODER_PROFILE_LEVEL` | `L"ProfileLevel"` | default `AMF_H264_LEVEL__4_2` |
| `AMF_VIDEO_ENCODER_MAX_LTR_FRAMES` | `L"MaxOfLTRFrames"` | `amf_int64`; default 0 |
| `AMF_VIDEO_ENCODER_LTR_MODE` | `L"LTRMode"` | default `LTR_MODE_RESET_UNUSED` |
| `AMF_VIDEO_ENCODER_SCANTYPE` | `L"ScanType"` | default `PROGRESSIVE` |
| `AMF_VIDEO_ENCODER_MAX_NUM_REFRAMES` | `L"MaxNumRefFrames"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_MAX_CONSECUTIVE_BPICTURES` | `L"MaxConsecutiveBPictures"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_ADAPTIVE_MINIGOP` | `L"AdaptiveMiniGOP"` | bool; default false |
| `AMF_VIDEO_ENCODER_ASPECT_RATIO` | `L"AspectRatio"` | `AMFRatio`; default 1,1 |
| `AMF_VIDEO_ENCODER_INPUT_FULL_RANGE_COLOR` | `L"InputFullRangeColor"` | bool; default false |
| `AMF_VIDEO_ENCODER_OUTPUT_FULL_RANGE_COLOR` | `L"FullRangeColor"` | bool; default false |
| `AMF_VIDEO_ENCODER_FULL_RANGE_COLOR` | `L"FullRangeColor"` | **deprecated alias — same string** |
| **`AMF_VIDEO_ENCODER_LOWLATENCY_MODE`** | **`L"LowLatencyInternal"`** | bool; default false; *"enables low latency mode and POC mode 2 in the encoder"* |
| `AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE` | `L"EnablePreAnalysis"` | bool; default false; *"Currently only works in …PEAK_CONSTRAINED_VBR mode"* |
| `AMF_VIDEO_ENCODER_PREENCODE_ENABLE` | `L"RateControlPreanalysisEnable"` | `PREENCODE_MODE_ENUM`; default DISABLED |
| `AMF_VIDEO_ENCODER_RATE_CONTROL_PREANALYSIS_ENABLE` | `L"RateControlPreanalysisEnable"` | **deprecated alias — same string** (`#pragma deprecated`) |
| `AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD` | `L"RateControlMethod"` | default **depends on USAGE** |
| `AMF_VIDEO_ENCODER_QVBR_QUALITY_LEVEL` | `L"QvbrQualityLevel"` | `amf_int64`; default 23; range 1–51 |
| `AMF_VIDEO_ENCODER_MAX_NUM_TEMPORAL_LAYERS` | `L"MaxNumOfTemporalLayers"` | `amf_int64`; default 1 |
| `AMF_VIDEO_ENCODER_QUALITY_PRESET` | `L"QualityPreset"` | default **depends on USAGE** |
| `AMF_VIDEO_ENCODER_COLOR_BIT_DEPTH` | `L"ColorBitDepth"` | default `AMF_COLOR_BIT_DEPTH_8` |
| `AMF_VIDEO_ENCODER_INPUT_COLOR_PROFILE` | `L"InColorProfile"` | `AMF_VIDEO_CONVERTER_COLOR_PROFILE_ENUM` |
| `AMF_VIDEO_ENCODER_INPUT_TRANSFER_CHARACTERISTIC` | `L"InColorTransferChar"` | |
| `AMF_VIDEO_ENCODER_INPUT_COLOR_PRIMARIES` | `L"InColorPrimaries"` | |
| `AMF_VIDEO_ENCODER_INPUT_MATRIX_COEFF` | `L"InMatrixCoeff"` | |
| `AMF_VIDEO_ENCODER_INPUT_HDR_METADATA` | `L"InHDRMetadata"` | `AMFBuffer` |
| `AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE` | `L"OutColorProfile"` | |
| `AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC` | `L"OutColorTransferChar"` | |
| `AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES` | `L"OutColorPrimaries"` | |
| `AMF_VIDEO_ENCODER_OUTPUT_MATRIX_COEFF` | `L"OutMatrixCoeff"` | |
| `AMF_VIDEO_ENCODER_OUTPUT_HDR_METADATA` | `L"OutHDRMetadata"` | `AMFBuffer` |
| **`AMF_VIDEO_ENCODER_OUTPUT_MODE`** | **`L"OutputMode"`** | `OUTPUT_MODE_ENUM`; default `FRAME` |

**Dynamic — "can be set at any time"**

| Constant | Wide string | Type / default |
|---|---|---|
| `AMF_VIDEO_ENCODER_FRAMERATE` | `L"FrameRate"` | `AMFRate`; default depends on usage |
| `AMF_VIDEO_ENCODER_B_PIC_DELTA_QP` | `L"BPicturesDeltaQP"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_REF_B_PIC_DELTA_QP` | `L"ReferenceBPicturesDeltaQP"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_ENFORCE_HRD` | `L"EnforceHRD"` | bool; default depends on USAGE |
| `AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE` | `L"FillerDataEnable"` | bool; default **false** |
| `AMF_VIDEO_ENCODER_ENABLE_VBAQ` | `L"EnableVBAQ"` | bool; default depends on USAGE |
| `AMF_VIDEO_ENCODER_HIGH_MOTION_QUALITY_BOOST_ENABLE` | `L"HighMotionQualityBoostEnable"` | bool |
| `AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE` | `L"VBVBufferSize"` | `amf_int64`; **in bits** |
| `AMF_VIDEO_ENCODER_INITIAL_VBV_BUFFER_FULLNESS` | `L"InitialVBVBufferFullness"` | `amf_int64`; default **64**; *"0=0% 64=100%"* |
| `AMF_VIDEO_ENCODER_MAX_AU_SIZE` | `L"MaxAUSize"` | `amf_int64`; default 0; **in bits** |
| `AMF_VIDEO_ENCODER_MIN_QP` / `MAX_QP` | `L"MinQP"` / `L"MaxQP"` | `amf_int64`; 0–51 |
| `AMF_VIDEO_ENCODER_QP_I` / `_QP_P` / `_QP_B` | `L"QPI"` / `L"QPP"` / `L"QPB"` | `amf_int64`; default 22; 0–51 |
| `AMF_VIDEO_ENCODER_TARGET_BITRATE` | `L"TargetBitrate"` | `amf_int64`; **in bits/s** |
| `AMF_VIDEO_ENCODER_PEAK_BITRATE` | `L"PeakBitrate"` | `amf_int64`; **in bits/s** |
| `AMF_VIDEO_ENCODER_RATE_CONTROL_SKIP_FRAME_ENABLE` | `L"RateControlSkipFrameEnable"` | bool |
| `AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING` | `L"HeaderInsertionSpacing"` | `amf_int64`; **range 0–1000** |
| `AMF_VIDEO_ENCODER_B_PIC_PATTERN` | `L"BPicturesPattern"` | `amf_int64`; default 0 |
| `AMF_VIDEO_ENCODER_DE_BLOCKING_FILTER` | `L"DeBlockingFilter"` | bool |
| `AMF_VIDEO_ENCODER_B_REFERENCE_ENABLE` | `L"BReferenceEnable"` | bool; default true |
| `AMF_VIDEO_ENCODER_IDR_PERIOD` | `L"IDRPeriod"` | `amf_int64`; in frames |
| `AMF_VIDEO_ENCODER_INTRA_PERIOD` | `L"IntraPeriod"` | `amf_int64`; default 0 |
| **`AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT`** | **`L"IntraRefreshMBsNumberPerSlot"`** | `amf_int64`; **in macroblocks** |
| `AMF_VIDEO_ENCODER_SLICES_PER_FRAME` | `L"SlicesPerFrame"` | `amf_int64`; default 1 |
| `AMF_VIDEO_ENCODER_CABAC_ENABLE` | `L"CABACEnable"` | `CODING_ENUM`; default `UNDEFINED` |
| `AMF_VIDEO_ENCODER_MOTION_HALF_PIXEL` | `L"HalfPixel"` | bool; default true |
| `AMF_VIDEO_ENCODER_MOTION_QUARTERPIXEL` | `L"QuarterPixel"` | bool; default true |
| `AMF_VIDEO_ENCODER_NUM_TEMPORAL_ENHANCMENT_LAYERS` | `L"NumOfTemporalEnhancmentLayers"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_PICTURE_TRANSFER_MODE` | `L"PicTransferMode"` | default OFF |
| **`AMF_VIDEO_ENCODER_QUERY_TIMEOUT`** | **`L"QueryTimeout"`** | `amf_int64`; **default 0 (no wait); timeout for QueryOutput call in ms** |
| `AMF_VIDEO_ENCODER_MEMORY_TYPE` | `L"EncoderMemoryType"` | `AMF_MEMORY_TYPE`; default `UNKNOWN` (auto) |
| `AMF_VIDEO_ENCODER_ENABLE_SMART_ACCESS_VIDEO` | `L"EnableEncoderSmartAccessVideo"` | `amf_bool`; default false |
| `AMF_VIDEO_ENCODER_INPUT_QUEUE_SIZE` | `L"InputQueueSize"` | `amf_int64`; **default 16** |

> There is **no `AMF_VIDEO_ENCODER_GOP_SIZE`** for AVC (only HEVC and AV1 have one). AVC uses
> `IDR_PERIOD` + `INTRA_PERIOD`. There is **no `AMF_VIDEO_ENCODER_INTRA_REFRESH_MODE`** for AVC
> either — only AV1 has `Av1IntraRefreshMode`.

**Per-submission — set on the input `AMFSurface`, not the component**

| Constant | Wide string | Type / default |
|---|---|---|
| `AMF_VIDEO_ENCODER_END_OF_SEQUENCE` | `L"EndOfSequence"` | bool; default false |
| `AMF_VIDEO_ENCODER_END_OF_STREAM` | `L"EndOfStream"` | bool; default false |
| **`AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE`** | **`L"ForcePictureType"`** | `PICTURE_TYPE_ENUM`; default NONE |
| `AMF_VIDEO_ENCODER_INSERT_AUD` | `L"InsertAUD"` | bool; default false |
| **`AMF_VIDEO_ENCODER_INSERT_SPS`** | **`L"InsertSPS"`** | bool; default false |
| **`AMF_VIDEO_ENCODER_INSERT_PPS`** | **`L"InsertPPS"`** | bool; default false |
| `AMF_VIDEO_ENCODER_PICTURE_STRUCTURE` | `L"PictureStructure"` | default FRAME |
| `AMF_VIDEO_ENCODER_MARK_CURRENT_WITH_LTR_INDEX` | `L"MarkCurrentWithLTRIndex"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_FORCE_LTR_REFERENCE_BITFIELD` | `L"ForceLTRReferenceBitfield"` | `amf_int64`; default 0 |
| `AMF_VIDEO_ENCODER_ROI_DATA` | `L"ROIData"` | 2D `AMFSurface`, `AMF_SURFACE_GRAY32`, 0–10 per 16×16 MB |
| `AMF_VIDEO_ENCODER_REFERENCE_PICTURE` | `L"ReferencePicture"` | `AMFInterface(AMFSurface)` |
| `AMF_VIDEO_ENCODER_PSNR_FEEDBACK` | `L"PSNRFeedback"` | `amf_bool`; default false |
| `AMF_VIDEO_ENCODER_SSIM_FEEDBACK` | `L"SSIMFeedback"` | `amf_bool`; default false |
| `AMF_VIDEO_ENCODER_STATISTICS_FEEDBACK` | `L"StatisticsFeedback"` | `amf_bool`; default false |
| `AMF_VIDEO_ENCODER_BLOCK_QP_FEEDBACK` | `L"BlockQpFeedback"` | `amf_bool`; default false |

**Set by the encoder on the output buffer**

| Constant | Wide string | Type |
|---|---|---|
| **`AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE`** | **`L"OutputDataType"`** | `OUTPUT_DATA_TYPE_ENUM` |
| `AMF_VIDEO_ENCODER_OUTPUT_MARKED_LTR_INDEX` | `L"MarkedLTRIndex"` | `amf_int64`; default −1 |
| `AMF_VIDEO_ENCODER_OUTPUT_REFERENCED_LTR_INDEX_BITFIELD` | `L"ReferencedLTRIndexBitfield"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_OUTPUT_TEMPORAL_LAYER` | `L"OutputTemporalLayer"` | `amf_int64` |
| **`AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE`** | **`L"OutputBufferType"`** | `OUTPUT_BUFFER_TYPE_ENUM` (frame / slice / slice_last) |
| `AMF_VIDEO_ENCODER_PRESENTATION_TIME_STAMP` | `L"PresentationTimeStamp"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_RECONSTRUCTED_PICTURE` | `L"ReconstructedPicture"` | `AMFInterface(AMFSurface)` |
| `AMF_VIDEO_ENCODER_STATISTIC_*` | `L"StatisticsFeedback…"` | ~20 `amf_int64` counters (frame/avg/max/min QP, pixel counts, bitcounts, MV sums, RD costs, SATD, variance) |

**Capability properties — read from `AMFCaps` with `GetProperty`**

| Constant | Wide string | Type |
|---|---|---|
| `AMF_VIDEO_ENCODER_CAP_MAX_BITRATE` | `L"MaxBitrate"` | `amf_int64`, bits |
| `AMF_VIDEO_ENCODER_CAP_NUM_OF_STREAMS` | `L"NumOfStreams"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_CAP_MAX_PROFILE` | `L"MaxProfile"` | `PROFILE_ENUM` |
| `AMF_VIDEO_ENCODER_CAP_MAX_LEVEL` | `L"MaxLevel"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_CAP_BFRAMES` | `L"BFrames"` | bool |
| `AMF_VIDEO_ENCODER_CAP_MIN_REFERENCE_FRAMES` | `L"MinReferenceFrames"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_CAP_MAX_REFERENCE_FRAMES` | `L"MaxReferenceFrames"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_CAP_MAX_TEMPORAL_LAYERS` | `L"MaxTemporalLayers"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_CAP_FIXED_SLICE_MODE` | `L"FixedSliceMode"` | bool |
| `AMF_VIDEO_ENCODER_CAP_NUM_OF_HW_INSTANCES` | `L"NumOfHwInstances"` | `amf_int64` |
| `AMF_VIDEO_ENCODER_CAP_COLOR_CONVERSION` | `L"ColorConversion"` | `AMF_ACCELERATION_TYPE`; default `AMF_ACCEL_GPU` |
| `AMF_VIDEO_ENCODER_CAP_PRE_ANALYSIS` | `L"PreAnalysis"` | `amf_bool` |
| `AMF_VIDEO_ENCODER_CAP_ROI` | `L"ROIMap"` | `amf_bool` |
| `AMF_VIDEO_ENCODER_CAP_MAX_THROUGHPUT` | `L"MaxThroughput"` | `amf_int64`, **in 16×16 MB** |
| `AMF_VIDEO_ENCODER_CAP_REQUESTED_THROUGHPUT` | `L"RequestedThroughput"` | `amf_int64`, in MB |
| `AMF_VIDEO_ENCODER_CAP_QUERY_TIMEOUT_SUPPORT` | `L"QueryTimeoutSupport"` | `amf_bool` (deprecated alias `CAPS_QUERY_TIMEOUT_SUPPORT`, same string) |
| `AMF_VIDEO_ENCODER_CAP_SUPPORT_SLICE_OUTPUT` | `L"SupportSliceOutput"` | `amf_bool` |
| `AMF_VIDEO_ENCODER_CAP_SUPPORT_SMART_ACCESS_VIDEO` | `L"EncoderSupportSmartAccessVideo"` | `amf_bool` |

> **Aliasing traps in the AVC table.** Four constant pairs share one wide string:
> `PREENCODE_ENABLE` == `RATE_CONTROL_PREANALYSIS_ENABLE` == `L"RateControlPreanalysisEnable"`;
> `LOWLATENCY_MODE` == `L"LowLatencyInternal"` (so "LOWLATENCY_MODE" and the thing people call
> "LOW_LATENCY_INTERNAL" are **one property**, not two);
> `OUTPUT_FULL_RANGE_COLOR` == `FULL_RANGE_COLOR` == `L"FullRangeColor"`;
> `CAP_QUERY_TIMEOUT_SUPPORT` == `CAPS_QUERY_TIMEOUT_SUPPORT` == `L"QueryTimeoutSupport"`.
> Setting "both" of a pair is a no-op, not a double configuration.

### 2.7 Submission model

**Bring-up order** (each step's slot index in brackets):

1. `libloading::Library::new("amfrt64.dll")`.
2. `AMFQueryVersion(&mut runtime_version)` — `__cdecl`, `AMF_RESULT`.
3. `AMFInit(AMF_FULL_VERSION, &mut factory)` — pass the version we *built* against. AMF's
   compatibility contract is that the runtime accepts a client built against an older SDK; the
   runtime rejects a *newer* client. So: pin `AMF_FULL_VERSION` low (see §5.R2), compare
   `runtime_version` for logging/feature gating, and let `AMFInit`'s own `AMF_RESULT` be the gate.
4. `factory->CreateContext(&ctx)` `[0]`.
5. `ctx->QueryInterface(IID_AMFContext1, &ctx1)` `[2]` — optional; only needed for `AllocSurfaceEx`.
6. **`ctx->InitDX11(our_d3d11_device, AMF_DX11_1)` `[18]`** — pass the *capture* device, so
   `CreateSurfaceFromDX11Native` is a pointer wrap. Passing `NULL` makes AMF create its own device
   and every submit becomes a hidden cross-device copy.
7. `factory->CreateComponent(ctx, L"AMFVideoEncoderVCE_AVC", &enc)` `[1]`.
8. `enc->GetCaps(&caps)` `[26]` — **works before `Init()`** (this is exactly what OBS's
   `check_preset_compatibility` does at `texture-amf.cpp:510-548`).
9. Set properties (§2.8) via `enc->SetProperty` `[3]`, **`USAGE` first**.
10. `enc->Init(AMF_SURFACE_NV12, width, height)` `[17]`.
11. `enc->GetProperty(L"ExtraData", &var)` `[4]` → `var.pInterface` → `QueryInterface(IID_AMFBuffer)`
    → `GetNative` `[25]` / `GetSize` `[24]` = the Annex-B SPS+PPS. Cache it for the WHEP SDP path.

**Per frame:**

```
ctx->CreateSurfaceFromDX11Native(tex.as_raw(), &surf, observer_or_null)   [49]
surf->SetFrameType(AMF_FRAME_PROGRESSIVE)                                 [28]
surf->SetPts(host_ns / 100)                                               [19]
surf->SetDuration(frame_ns / 100)                                         [21]
if force_idr {
    surf->SetProperty(L"ForcePictureType", Int64(PICTURE_TYPE_IDR))       [3]
    surf->SetProperty(L"InsertSPS", Bool(true))                           [3]
    surf->SetProperty(L"InsertPPS", Bool(true))                           [3]
}
enc->SubmitInput(surf as *mut AMFData)                                    [22]
loop { enc->QueryOutput(&mut data) }                                      [23]
data->QueryInterface(IID_AMFBuffer, &buf)                                 [2]
buf->GetNative() / buf->GetSize()                                        [25]/[24]
data->GetProperty(L"OutputDataType", &var)                                 [4]  -> IDR?
surf->Release(); data->Release(); buf->Release()                           [1]
```

**Result semantics on the loop:**

| Call | Result | Meaning | Action |
|---|---|---|---|
| `SubmitInput` | `AMF_OK` | accepted | proceed |
| `SubmitInput` | `AMF_NEED_MORE_INPUT` (44) | accepted, no output yet | treat as success |
| `SubmitInput` | `AMF_INPUT_FULL` (25) | input queue full — **the surface was NOT consumed** | back off and re-submit the *same* surface |
| `SubmitInput` | `AMF_EOF` (23) | after `Drain()` | stop submitting |
| `QueryOutput` | `AMF_OK` + non-null | a packet | consume |
| `QueryOutput` | `AMF_OK` + **null** | possible; means "nothing yet" | treat as `AMF_REPEAT` |
| `QueryOutput` | `AMF_REPEAT` (24) | nothing ready | stop draining this pass |
| `QueryOutput` | `AMF_EOF` (23) | drained after `Drain()` | done |

`QueryOutput` can return `AMF_OK` with a null `ppData`. Both OBS (`if (new_packet)`
`texture-amf.cpp:812`) and FFmpeg check the pointer, not just the result. Do the same.

**`QUERY_TIMEOUT`** (`L"QueryTimeout"`, `amf_int64`, ms) turns `QueryOutput` from a non-blocking poll
into a blocking wait. `0` = the documented default, *"no wait"*, and `QueryOutput` then returns
`AMF_REPEAT` immediately. AMD's own doc: *"Setting this to a nonzero value will reduce polling load …
it will be blocked until the frame is ready or until the timeout is reached."*

> **It is effectively STATIC, despite the header filing it under "Dynamic properties".** AMD in
> [AMF#516](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/516): set it **before `Init()`**.
> Setting it afterwards returns `AMF_OK` and `GetProperty` echoes the value back, but it **silently
> does nothing.** FFmpeg sets it pre-`Init` and reads it back to detect support
> (amfenc_h264.c:549-552); do the same. `AMF_VIDEO_ENCODER_CAP_QUERY_TIMEOUT_SUPPORT`
> (`L"QueryTimeoutSupport"`) is the caps-side gate.

For a one-frame-in-flight design this is the single most important knob, and the shipping answer is
FFmpeg's: **`QUERY_TIMEOUT = 1` (one millisecond), set pre-`Init`, then read back.** If it is
honoured, never sleep — `QueryOutput` itself does the waiting, and the comment at amfenc.c:662-666 is
explicit: *"Only sleep if the driver doesn't support waiting in QueryOutput()."* If it is not
honoured, fall back to `av_usleep(1000)`-equivalent polling. AMD notes that a `Sleep`-based poll
additionally needs `amf_increase_timer_precision()` to get anywhere near 1 ms granularity on Windows.
A larger timeout (say one frame period) is *not* better: it is a ceiling, not a target, and 1 ms is
already below any encode time we will see.

**Threading.** `AMFComponent` is documented nowhere as thread-safe, but AMD states in
[AMF#502](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/502) that *"the main AMF model is to
use two threads"* — submit on one, query on the other — and adds that `AMF_NEED_MORE_INPUT` is **not**
a reliable "don't query" signal. FFmpeg locks `SubmitInput` behind the device-context mutex and
leaves `QueryOutput` unlocked (amfenc.c:547-561); OBS keeps both on one thread and is fine.
**Recommendation: keep one thread** (§5.R4) — with strict one-in/one-out there is nothing for a
second thread to overlap. The context's `LockDX11`/`UnlockDX11` `[20]/[21]` exist for touching the
shared D3D11 device from another thread while AMF may be using it. Regardless of threading model,
call **`ID3D10Multithread::SetMultithreadProtected(TRUE)`** on the device we hand to `InitDX11`: AMF's
own `DeviceDX11.cpp:189-194` does it unconditionally, and AMD's fix for flicker plus *growing*
latency on a Desktop-Duplication device was *"do not use `D3D11_CREATE_DEVICE_SINGLETHREADED`"* plus
an explicit `Flush()` + `D3D11_QUERY_EVENT` fence after `CopyResource` before `ReleaseFrame`
([AMF#92](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/92)). Conversely,
`D3D11_CREATE_DEVICE_VIDEO_SUPPORT` is **not** required by AMF — AMD's sample, OBS and Sunshine's AMF
path all pass `createDeviceFlags = 0`. (We need it anyway for `ID3D11VideoDevice` in `scaler.rs`.)

**Texture lifetime.** `CreateSurfaceFromDX11Native` **wraps, it does not copy**. The `ID3D11Texture2D`
must stay alive and unmodified until AMF is done reading it — which is *not* when `SubmitInput`
returns, but when the surface's data is released. The `AMFSurfaceObserver*` third parameter is
precisely the "AMF is done with your texture" callback (`OnSurfaceDataRelease`). Two correct designs:

- **Ring + observer** (OBS): keep a pool of encoder-owned textures, `CopyResource` the capture frame
  into a free one, wrap it, and recycle the texture in `OnSurfaceDataRelease`. Costs one GPU→GPU copy
  per frame, but the capture path is never blocked.
- **One-in-flight, no observer** (recommended for us): submit exactly one surface, `QueryOutput` its
  packet, `Release` the surface, then reuse the same texture. Zero copies, zero pool, and the
  "is AMF done" question is answered by the packet arriving. This is the discipline the NVENC backend
  already uses (`encode_input` at `nvidia/encoder.rs:425-440` acquires the keyed mutex, encodes, and
  returns the bitstream synchronously) and it is what `amd.md` §2 specifies.

**Static desktop / frame repeat.** AMF has no "encode the previous frame again" call. The existing
pipeline already solves this: `spawn_repeater` (`pipeline.rs:1456+`) re-encodes the last input
texture on a timer, and `Backend::encode_repeat` dispatches it. For AMF, "repeat" is literally
"submit the same surface again" — cheap and correct, because the texture is still valid.

**Teardown order.** `Terminate` **down the ownership chain, then release up**:

```
component->Drain()        [20]   // optional; only if you want the tail packets
loop { component->QueryOutput(&d) [23] } until AMF_EOF
component->Terminate()    [19]
component->Release()      [1]
context->Terminate()      [13]
context->Release()        [1]
// factory is DLL-owned: never Release
drop(library)                    // FreeLibrary last
```

Releasing the context before terminating the component, or dropping `libloading::Library` while any
AMF object is alive, unloads the code the vtables point into. In Rust that means the `Library` must
be the **last** field of the encoder struct (Rust drops fields in declaration order) or held in a
field the `Drop` impl clears last — the same rule `nvidia/encoder.rs:83` already follows with
`_lib: libloading::Library` declared last.

**Transient vs fatal.** Retryable: `AMF_INPUT_FULL`, `AMF_REPEAT`, `AMF_NEED_MORE_INPUT`.
Recoverable by teardown+rebuild: `AMF_RESOLUTION_CHANGED` (26 — the header says *"client needs to
Drain/Terminate/Init"*), `AMF_NO_DEVICE` (17), `AMF_DIRECTX_FAILED` (18) — these are the
device-lost/TDR class and should surface as "rebuild the pipeline", the same way the NVENC path
handles a poisoned keyed mutex (`SlotEncodeError::Poisoned`, `nvidia/encoder.rs:31-34`). Fatal:
everything else, in particular `AMF_ENCODER_NOT_PRESENT` (36),
`AMF_SURFACE_FORMAT_NOT_SUPPORTED` (31), `AMF_INVALID_RESOLUTION` (29) — fall through to the next
vendor.

### 2.8 Detection and capability probing

**Adapter.** Enumerate with `IDXGIFactory1::EnumAdapters1` and match **`desc.VendorId == 0x1002`**,
not the description string. `0x1002` is ATI/AMD's PCI vendor ID and covers both Radeon dGPUs and
Ryzen iGPUs. Reject `DXGI_ADAPTER_FLAG_SOFTWARE`. OBS does exactly this and throws
`"Seems somehow AMF is trying to initialize on a non-AMD adapter"` otherwise
(`texture-amf.cpp:360-363`).

> The repo's current `select_vendor` (`windows_utils/streamer/mod.rs:23-33`) matches on the
> description string (`"AMD" | "RADEON" | "ADVANCED MICRO DEVICES"`). That misses at least the
> localized and OEM-rebranded descriptions, and it is strictly worse than the ID. See finding
> `minor-amd-detect-vendor-by-dxgi-vendorid.md`.

**APU vs dGPU.** There is no DXGI flag for it. The workable proxy is
`DXGI_ADAPTER_DESC1::DedicatedVideoMemory` — an APU reports a small (often 0–512 MB) carve-out and a
large `SharedSystemMemory`, a dGPU reports its real VRAM. We do not actually need the distinction:
what matters is *which adapter owns the captured texture*, which we already know from
`device_vendor(&device)` (`mod.rs:35-59`).

**Hybrid laptops.** Two cases, and the AMD one is the *easy* one:

- **AMD iGPU (or dGPU) owns the display and the capture texture** → capture, VideoProcessor and VCN
  are all on one adapter. `InitDX11(capture_device)` and `CreateSurfaceFromDX11Native` on the
  VideoProcessor's output = **zero copies, no keyed mutex, no shared handle.** This removes the
  cross-adapter dance the NVENC path needs on this repo's dev laptop.
- **Non-AMD adapter owns the capture, AMD dGPU is the encoder** (Intel iGPU + Radeon dGPU) → AMF
  cannot wrap a texture created on a different `ID3D11Device`; `CreateSurfaceFromDX11Native` will
  fail or, worse, the encoder reads from the wrong device. You need one GPU→GPU copy through a
  `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX` texture — exactly
  `pipeline.rs::build_zero_copy` (`pipeline.rs:1009-1032`), reused verbatim with `Encoder::new_shared`
  swapped for the AMF equivalent.

**Binding AMF to a specific adapter** is done by *giving it your device* (`InitDX11(device, AMF_DX11_1)`),
full stop. There is no adapter-index property on `AMFContext` — the only related property is
`AMF_CONTEXT_DEVICE_TYPE` (`L"AMF_Context_DeviceType"`, `core/Context.h:785`, GPU vs CPU), which is
not an adapter selector. `AMF_VIDEO_ENCODER_INSTANCE_INDEX` (`L"EncoderInstance"`) picks between
*multiple VCN engines on one GPU*, not between GPUs; the count comes from
`AMF_VIDEO_ENCODER_CAP_NUM_OF_HW_INSTANCES`.

**Capability probe, before `Init()`:**

```
factory->CreateComponent(ctx, AMFVideoEncoderVCE_AVC, &enc)   // AMF_OK? -> AVC encode exists at all
enc->GetCaps(&caps)                                     [26]
caps->GetAccelerationType()                             [13]  // must be AMF_ACCEL_HARDWARE (0) or GPU (1)
caps->GetProperty(L"MaxBitrate", &v)                    [4]
caps->GetProperty(L"NumOfHwInstances", &v)              [4]
caps->GetProperty(L"MaxThroughput", &v)                 [4]   // in 16x16 MBs
caps->GetProperty(L"RequestedThroughput", &v)           [4]   // already committed by other apps
caps->GetProperty(L"QueryTimeoutSupport", &v)           [4]
caps->GetProperty(L"SupportSliceOutput", &v)            [4]
caps->GetInputCaps(&io)                                 [14]
io->GetWidthRange(&min, &max) / GetHeightRange(...)     [3]/[4]
io->GetNumOfFormats() / GetFormatAt(i, &fmt, &native)   [6]/[7]  // is BGRA in the list? is it native?
```

`MaxThroughput − RequestedThroughput` vs the throughput this stream needs is the
"can this preset actually keep up" check; OBS uses it to silently downgrade the quality preset
(`texture-amf.cpp:510-548`, comment: *"The throughput depends on the current preset and the other
static encoder properties. If the throughput is lower than the max throughput, switch to a lower
preset."*).

**Feature availability is version-and-driver dependent and must be probed, not assumed.** Two probe
mechanisms exist and both are cheap:
- `component->HasProperty(name)` `[5]` — does this build of the runtime know the property at all?
- `component->SetProperty(name, v)` `[3]` returning `AMF_NOT_FOUND` (11) / `AMF_NOT_SUPPORTED` (10) —
  does *this GPU* support it?

Treat both as "skip this knob and log once", never as a hard failure. `LOWLATENCY_MODE`,
`OUTPUT_MODE=SLICE`, `ENABLE_SMART_ACCESS_VIDEO` and `INTRA_REFRESH_NUM_MBS_PER_SLOT` are the four
that realistically vary.

**Driver floor.** `amfrt64.dll` has shipped with the Radeon driver since the Crimson/17.x era, and
the AMF SDK's own README/releases track Adrenalin. The concrete data point available in the wild:
the legacy `obs-amd-encoder` 2.6.0 release notes set the effective floor at **driver 19.7.1**
(recommended 19.9.2), and the plugin enforced it in code — `old/amf.cpp:154-158`:
`if (m_AMFVersion_Runtime < AMF_FULL_VERSION) { PLOG_ERROR("The detected AMF runtime is too old, please update your drivers."); throw std::runtime_error("AMF Runtime is outdated."); }`.
Current OBS **dropped that check entirely** and gates purely on
`CreateComponent(...) == AMF_OK`, which is a better test because it asks the actual question.
The AMF README's own table (§2.1) gives the real mapping — AMF 1.4 ≈ Crimson 17.1.1, and the
features we might reach for land at 1.4.21 (LTR), 1.4.28 (AV1), 1.4.29 (SmartAccess), 1.4.30 (slice
output). **Copy current OBS, not the legacy plugin**: no version-number floor; probe by creating the
component, and let per-property probes handle the rest. Log `AMFQueryVersion`'s value for support
tickets — but **never parse the DLL's `ProductVersion` resource string**: a 2025 driver shipped it as
`1.5.0.0;1.2;CL` ([AMF#579](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/579)). Note also
that `LoadLibrary` failing with error 126 is common on minimal driver installs and on machines
missing the VC runtime ([AMF#11](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/11)) — that
is a normal "no AMF here" outcome, not an error worth surfacing to the user.

**The one thing a driver version *does* predict** is the ULL regression: `USAGE =
ULTRA_LOW_LATENCY` fails `Init()` on pre-RDNA parts from driver **23.7.1** onward
([AMF#410](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/410)). Even there, the right
response is a runtime probe rather than a version check — see R3a.

### 2.9 Corrections to `src-tauri/src/windows_utils/streamer/amd.md`

The spec is directionally right and its latency reasoning is sound, but it names APIs that do not
exist. Every item below is verified against the real headers:

| `amd.md` says | Reality |
|---|---|
| `AMF_VIDEO_ENCODER_GOP_SIZE` set "very large" | **No such AVC property.** AVC has `IDR_PERIOD` (`L"IDRPeriod"`) and `INTRA_PERIOD` (`L"IntraPeriod"`). `GOP_SIZE` exists only for HEVC (`L"HevcGOPSize"`) and AV1 (`L"Av1GOPSize"`). |
| `AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING` = "emit SPS/PPS on every IDR" | It is an `amf_int64` **spacing in frames, range 0–1000**, not a mode. The per-IDR guarantee comes from the per-surface `INSERT_SPS`/`INSERT_PPS` booleans. (The *mode* enum `HEADER_INSERTION_MODE` with `IDR_ALIGNED` exists on **HEVC and AV1 only**.) |
| `AMF_VIDEO_ENCODER_INTRA_REFRESH_MODE` "support by GPU generation" | **No such AVC property.** AVC has only `INTRA_REFRESH_NUM_MBS_PER_SLOT`; `INTRA_REFRESH_MODE` is AV1-only (`L"Av1IntraRefreshMode"`, with `DISABLED/GOP_ALIGNED/CONTINUOUS`). |
| `AMF_VIDEO_ENCODER_RATE_CONTROL_PREANALYSIS_ENABLE` and `PREENCODE_ENABLE` as two knobs | **One property.** Both constants expand to `L"RateControlPreanalysisEnable"`; the `_RATE_CONTROL_` spelling carries `#pragma deprecated`. |
| `LOWLATENCY_MODE` and `LOW_LATENCY_INTERNAL` as two things | **One property**, `L"LowLatencyInternal"`. |
| `INITIAL_VBV_BUFFER_FULLNESS = 0` ("full / no pre-fill") | **Backwards.** Header: *"default = 64; Initial VBV Buffer Fullness 0=0% 64=100%"*. `0` = **empty**, which is the worst case for a rate controller that then has to fill it. For minimum startup delay you want **64 (=100%)**, i.e. the default; leave it alone. |
| `MAX_NUM_REFRAMES = 1` for latency | **Actively harmful.** It is a *reference-list* size, not a delay source — with `B_PIC_PATTERN = 0` there is no reorder delay regardless. But the docs state intra-refresh, `B_PIC_PATTERN`, `B_PIC_DELTA_QP` and `B_REFERENCE_ENABLE` are available **only when `MaxOfReferenceFrames > 1` and `NumOfLTR == 0`** — so setting 1 silently disables the rolling intra-refresh that `USAGE = ULTRA_LOW_LATENCY` gives us for free at 255 MBs/slot, which is the very feature §3 of `amd.md` asks for. Leave it at the USAGE default. |
| `INITIAL_VBV_BUFFER_FULLNESS`, `USAGE`, `PREENCODE` etc. treated as independent knobs | `USAGE` **re-defaults most of them** (see the table in §3.5) — AMD: *"Setting USAGE you overwrite most of previous settings including rate control method"* ([AMF#517](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/517)). `amd.md` says "set it first", which is right, but does not say that ULL already delivers `LOWLATENCY_MODE = true`, `ENFORCE_HRD = true`, `QUALITY_PRESET = SPEED`, `INTRA_REFRESH = 255` and `RATE_CONTROL_METHOD = LATENCY_CONSTRAINED_VBR`. Several of the table's rows are no-ops; two (`ENFORCE_HRD`, `RATE_CONTROL_METHOD`) are deliberate overrides of ULL and should be labelled as such. |
| `QUERY_TIMEOUT = 0` (non-blocking) + "spin a tight bounded poll" | Backwards. AMD ([AMF#516](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/516)) and FFmpeg both set a **1 ms blocking** timeout **before `Init()`** and then never sleep. Setting it *after* `Init` returns `AMF_OK` and does nothing. |
| No mention of the ULL failure mode | `USAGE = ULTRA_LOW_LATENCY` **breaks `Init()` outright on pre-RDNA parts from driver 23.7.1** ([AMF#410](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/410)). A fallback to `LOW_LATENCY` is mandatory, not optional — see R3a. |
| "AMF AVC emits Annex-B start codes" | Correct — `AMF_VIDEO_ENCODER_EXTRADATA` is documented as *"SPS/PPS buffer in Annex B format"*, and the output buffer is Annex-B too. |
| "prefer the VideoProcessor over AMF's own `AMFVideoConverter`" | Correct, and stronger than stated: **`scaler.rs` currently produces `DXGI_FORMAT_B8G8R8A8_UNORM` only** (`scaler.rs:76`), so this is real work, not a config change. |
| §5 milestone M-AMD2 expects `slice count 4–8` | `SLICES_PER_FRAME > 1` on AVC costs compression efficiency and, without `OUTPUT_MODE=SLICE`, buys **nothing** for latency — you still get one buffer per frame. See §5.R6. |
| References `docs/…` design notes | Also flagged by the existing finding `minor-docs-amd-md-references-missing-documents-and-ai-tooling`. |

### 2.10 HEVC and AV1 — the parallel surface

Everything is name-mangled per codec, and **the macro names do not match the wire strings**:

```c
#define AMFVideoEncoderVCE_AVC   L"AMFVideoEncoderVCE_AVC"    // matches
#define AMFVideoEncoder_HEVC     L"AMFVideoEncoderHW_HEVC"    // NOTE: "HW", not the macro's name
#define AMFVideoEncoder_AV1      L"AMFVideoEncoderHW_AV1"     // NOTE: "HW"
#define AMFVideoEncoderVCE_SVC   L"AMFVideoEncoderVCE_SVC"
```

The property strings are prefixed `Hevc`/`Av1` (with a handful of inconsistencies), so a shared
`amf_sys` cannot share constants across codecs:

| Concept | AVC | HEVC | AV1 |
|---|---|---|---|
| Usage | `L"Usage"` | `L"HevcUsage"` | `L"Av1Usage"` |
| Frame size | `L"FrameSize"` | `L"HevcFrameSize"` | `L"Av1FrameSize"` |
| Frame rate | `L"FrameRate"` | `L"HevcFrameRate"` | `L"Av1FrameRate"` |
| Profile | `L"Profile"` | `L"HevcProfile"` | `L"Av1Profile"` |
| Level | `L"ProfileLevel"` | `L"HevcProfileLevel"` | `L"Av1Level"` |
| Quality preset | `L"QualityPreset"` | `L"HevcQualityPreset"` | `L"Av1QualityPreset"` |
| Rate control | `L"RateControlMethod"` | `L"HevcRateControlMethod"` | `L"Av1RateControlMethod"` |
| Target bitrate | `L"TargetBitrate"` | `L"HevcTargetBitrate"` | `L"Av1TargetBitrate"` |
| Peak bitrate | `L"PeakBitrate"` | `L"HevcPeakBitrate"` | `L"Av1PeakBitrate"` |
| VBV size | `L"VBVBufferSize"` | `L"HevcVBVBufferSize"` | `L"Av1VBVBufferSize"` |
| Initial VBV | `L"InitialVBVBufferFullness"` | `L"HevcInitialVBVBufferFullness"` | `L"Av1InitialVBVBufferFullness"` |
| Enforce HRD | `L"EnforceHRD"` | `L"HevcEnforceHRD"` | `L"Av1EnforceHRD"` |
| Filler data | `L"FillerDataEnable"` | `L"HevcFillerDataEnable"` | `L"Av1FillerData"` |
| Max AU size | `L"MaxAUSize"` | `L"HevcMaxAUSize"` | `L"Av1MaxCompressedFrameSize"` |
| **Low latency** | `L"LowLatencyInternal"` | **`L"LowLatencyInternal"`** (same string!) | `L"Av1EncodingLatencyMode"` (an enum, not a bool) |
| Pre-analysis | `L"EnablePreAnalysis"` | `L"HevcEnablePreAnalysis"` | `L"Av1EnablePreAnalysis"` |
| Pre-encode | `L"RateControlPreanalysisEnable"` | `L"HevcRateControlPreAnalysisEnable"` | `L"Av1RateControlPreEncode"` |
| VBAQ / AQ | `L"EnableVBAQ"` | `L"HevcEnableVBAQ"` | `L"Av1AQMode"` (enum) |
| B-frames | `L"BPicturesPattern"` | *(none — no B on HEVC)* | *(none in the AVC sense)* |
| GOP / IDR | `L"IDRPeriod"` + `L"IntraPeriod"` | `L"HevcGOPSize"` + `L"HevcGOPSPerIDR"` | `L"Av1GOPSize"` + `L"Av1IntraPeriod"` |
| Header insertion | `L"HeaderInsertionSpacing"` (int, 0–1000) | `L"HevcHeaderInsertionMode"` (**enum**: NONE/GOP_ALIGNED/IDR_ALIGNED/SUPPRESSED) | `L"Av1HeaderInsertionMode"` (enum) |
| Slices / tiles | `L"SlicesPerFrame"` | `L"HevcSlicesPerFrame"` | `L"Av1NumTilesPerFrame"` |
| Intra refresh | `L"IntraRefreshMBsNumberPerSlot"` (16×16 MB) | `L"HevcIntraRefreshCTBsNumberPerSlot"` (64×64 CTB) | `L"Av1IntraRefreshMode"` (**enum**) + `L"Av1IntraRefreshNumOfStripes"` |
| Deblocking | `L"DeBlockingFilter"` (bool, on) | `L"HevcDeBlockingFilter"` (**named `_DISABLE`** in the constant) | `L"Av1CdefMode"` (different filter entirely) |
| Entropy coding | `L"CABACEnable"` | *(n/a — HEVC is always CABAC)* | *(n/a)* |
| Query timeout | `L"QueryTimeout"` | `L"HevcQueryTimeout"` | `L"Av1QueryTimeout"` |
| Output mode | `L"OutputMode"` (FRAME/**SLICE**) | `L"HevcOutputMode"` (FRAME/SLICE) | `L"AV1OutputMode"` (FRAME/**TILE**) — note the `AV1` caps |
| Output buffer type | `L"OutputBufferType"` | `L"HevcOutputBufferType"` | `L"AV1OutputBufferType"` |
| Force pic type | `L"ForcePictureType"` | `L"HevcForcePictureType"` | `L"Av1ForceFrameType"` (**different enum**) |
| Insert headers | `L"InsertSPS"` + `L"InsertPPS"` | `L"HevcInsertHeader"` (**one** bool: SPS+PPS+VPS) | `L"Av1ForceInsertSequenceHeader"` |
| Insert AUD | `L"InsertAUD"` | `L"HevcInsertAUD"` | *(n/a)* |
| Output data type | `L"OutputDataType"` | `L"HevcOutputDataType"` | `L"Av1OutputFrameType"` |
| Max LTR | `L"MaxOfLTRFrames"` | `L"HevcMaxOfLTRFrames"` | `L"Av1MaxNumLTRFrames"` |
| Mark LTR | `L"MarkCurrentWithLTRIndex"` | `L"HevcMarkCurrentWithLTRIndex"` | `L"Av1MarkCurrentWithLTRIndex"` |
| Force LTR ref | `L"ForceLTRReferenceBitfield"` | `L"HevcForceLTRReferenceBitfield"` | `L"Av1ForceLTRReferenceBitfield"` |
| Max refs | `L"MaxNumRefFrames"` | `L"HevcMaxNumRefFrames"` | `L"Av1MaxNumRefFrames"` |
| Full range | `L"FullRangeColor"` (bool) | `L"HevcNominalRange"` (**enum** STUDIO/FULL) | `L"Av1OutputColorProfile"` |
| SmartAccess | `L"EnableEncoderSmartAccessVideo"` | `L"HevcEnableEncoderSmartAccessVideo"` | `L"Av1EnableEncoderSmartAccessVideo"` |
| Input queue | `L"InputQueueSize"` | `L"HevcInputQueueSize"` | `L"Av1InputQueueSize"` |
| Statistics | `L"StatisticsFeedback"` | `L"HevcStatisticsFeedback"` | `L"Av1StatisticsFeedback"` |
| PSNR/SSIM out | `L"PSNRY"`, `L"SSIMY"`, … | **`L"PSNRY"`, `L"SSIMY"`** (unprefixed!) | `L"Av1PSNRY"`, `L"Av1SSIMY"` |

AV1-only: `L"Av1ScreenContentTools"` (default true), `L"Av1PaletteMode"`, `L"Av1ForceIntegerMv"`,
`L"Av1CdfUpdate"`, `L"Av1AlignmentMode"`, `L"Av1ErrorResilientMode"`, `L"Av1TileGroupObu"`,
`L"Av1OrderHint"`, `L"Av1SwitchFrameInsertionMode"`, `L"Av1MinQIndex_Intra"`/`_Inter` (range
**1–255**, not 0–51), and the `L"Av1EncodingLatencyMode"` enum (`NONE 0`,
`POWER_SAVING_REAL_TIME 1` = finish within 1/fps, `REAL_TIME 2` = within 1/(2·fps),
`LOWEST_LATENCY 3` = as fast as possible, highest power). For a screen-sharing use case
`Av1ScreenContentTools` + `Av1PaletteMode` are genuinely valuable — AV1's palette mode is built for
exactly the flat-colour UI content we stream.

**Hardware availability** (per [Video Core Next](https://en.wikipedia.org/wiki/Video_Core_Next)):
HEVC encode on **every** VCN generation (Raven Ridge/Vega onward, and VCE 3.x before that); AVC
encode on VCN 1.0–3.0 and 4.0+, dropped **only** on VCN 3.0.33 (Navi 24 / RX 6400–6500);
**AV1 encode requires VCN 4.0 = RDNA3** (RX 7000, Radeon 700M/800M) and later. H.264 **B-frames are
absent on VCN 1.0–2.x, RDNA1 included**, re-introduced in VCN 3.0 (RDNA2) — so never assume B-frame
support; query `AMF_VIDEO_ENCODER_CAP_BFRAMES` (this is exactly what FFmpeg does at
amfenc_h264.c:478-537 and OBS at texture-amf.cpp:1579-1580).

Practical rule for extending the backend later: gate each codec purely on
`CreateComponent(<id>) == AMF_OK` **plus** `GetAccelerationType() == AMF_ACCEL_HARDWARE`, never on a
version or a GPU name.

---

## 3. How others do it

### 3.1 OBS Studio — `plugins/obs-ffmpeg/texture-amf.cpp` (current, texture path)

Full read of the 2822-line file. The macro `set_avc_property(enc, NAME, v)` expands to
`enc->amf_encoder->SetProperty(AMF_VIDEO_ENCODER_##NAME, v)` (L281-289).

**Order is the whole story.** `amf_create_encoder` does `CreateContext` → `InitDX11` →
`CreateComponent(AVC)` → `GetCaps` (reading `CAP_BFRAMES`, `CAP_MAX_THROUGHPUT`,
`CAP_REQUESTED_THROUGHPUT`, `CAP_ROI`) **before touching a single property** (L1671-1691). Then, at
L1693-1704, verbatim:

```cpp
set_avc_property(enc, FRAMESIZE, AMFConstructSize(enc->cx, enc->cy));
set_avc_property(enc, USAGE, AMF_VIDEO_ENCODER_USAGE_TRANSCODING);
set_avc_property(enc, QUALITY_PRESET, get_avc_preset(enc, preset));
set_avc_property(enc, PROFILE, get_avc_profile(settings));
set_avc_property(enc, LOWLATENCY_MODE, false);
set_avc_property(enc, CABAC_ENABLE, AMF_VIDEO_ENCODER_UNDEFINED);
set_avc_property(enc, PREENCODE_ENABLE, true);
set_avc_property(enc, OUTPUT_COLOR_PROFILE, enc->amf_color_profile);
set_avc_property(enc, OUTPUT_TRANSFER_CHARACTERISTIC, enc->amf_characteristic);
set_avc_property(enc, OUTPUT_COLOR_PRIMARIES, enc->amf_primaries);
set_avc_property(enc, FULL_RANGE_COLOR, enc->full_range);
set_avc_property(enc, FRAMERATE, enc->amf_frame_rate);
```

**`USAGE` is set second, before everything that matters**; only `FRAMESIZE` precedes it, and that is
harmless because `Init(fmt, cx, cy)` re-supplies the size. HEVC (L2088-2089) and AV1 (L2519-2520) are
identical, AV1 inserting `ALIGNMENT_MODE` right after `USAGE`. **No comment anywhere states the
re-defaulting rule** — OBS encodes it purely as ordering. That ordering is the de-facto contract.

Then `amf_avc_init` (L1558-1660), still pre-`Init()`:

```cpp
if (pa_enabled) set_avc_property(enc, PRE_ANALYSIS_ENABLE, pa_enabled);       // L1575
set_avc_property(enc, MAX_CONSECUTIVE_BPICTURES, bf);                         // L1579 (bf default 2)
set_avc_property(enc, B_PIC_PATTERN, bf);                                     // L1580
if (bf > 0 && pa_enabled) set_avc_property(enc, ADAPTIVE_MINIGOP, true);      // L1587
set_avc_property(enc, RATE_CONTROL_METHOD, rc);                               // L1599
if (rc != CONSTANT_QP && rc != HIGH_QUALITY_VBR && rc != HIGH_QUALITY_CBR)
        set_avc_property(enc, ENABLE_VBAQ, true);                             // L1603
amf_avc_update_data(enc, rc, bitrate * 1000, qp);                             // L1606
set_avc_property(enc, ENFORCE_HRD, true);                                     // L1608
set_avc_property(enc, HIGH_MOTION_QUALITY_BOOST_ENABLE, false);               // L1609
set_avc_property(enc, IDR_PERIOD, gop_size);                                  // L1614
if (repeat_headers) set_avc_property(enc, HEADER_INSERTION_SPACING, gop_size);// L1618
set_avc_property(enc, DE_BLOCKING_FILTER, true);                              // L1621
amf_set_codec_level(enc);          // -> PROFILE_LEVEL                        // L1624
check_preset_compatibility(enc, preset);   // re-sets QUALITY_PRESET + GetCaps // L1626
```

and the rate-control block (L1388-1420):

```cpp
if (rc != CONSTANT_QP && rc != QUALITY_VBR) {
    set_avc_property(enc, TARGET_BITRATE, bitrate);
    set_avc_property(enc, PEAK_BITRATE, bitrate);
    set_avc_property(enc, VBV_BUFFER_SIZE, bitrate);            // == 1 SECOND of bits
    if (rc == CBR) { set_avc_property(enc, FILLER_DATA_ENABLE, true); pipeline_flush = false; }
    else if (rc == PEAK_CONSTRAINED_VBR || rc == HIGH_QUALITY_VBR) {
        set_avc_property(enc, PEAK_BITRATE,    bitrate * 1.5);
        set_avc_property(enc, VBV_BUFFER_SIZE, bitrate * 1.5); }
    else if (rc == LATENCY_CONSTRAINED_VBR) {
        set_avc_property(enc, VBV_BUFFER_SIZE, (bitrate / framerate) * 1.1); }   // ~1 FRAME
}
```

**OBS's AMF path is explicitly tuned for quality, not latency**: `USAGE = TRANSCODING`,
`LOWLATENCY_MODE = false`, `PREENCODE_ENABLE = true`, B-frames default 2, `ENFORCE_HRD = true`,
`FILLER_DATA_ENABLE = true` in CBR, and a **one-second** VBV. The one place it does the low-latency
thing is the `LATENCY_CONSTRAINED_VBR` branch, which sizes VBV at `(bitrate/fps)*1.1` — **one frame,
+10%**. That is the number worth stealing. Intra-refresh, LTR and `SMART_ACCESS_VIDEO` are never set;
`MAX_AU_SIZE` and `INTRA_REFRESH_NUM_MBS_PER_SLOT` are reachable only through the free-form
`ffmpeg_opts` escape hatch.

**Context and textures.** OBS creates its own `ID3D11Device` (`amf_init_d3d11`, L328-388:
`CreateDXGIFactory1` → `EnumAdapters(ovi.adapter)` → assert `VendorId == 0x1002` →
`D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, …)`) and calls
`amf_context->InitDX11(device, AMF_DX11_1)`. The host/CPU fallback passes `nullptr` and lets AMF
create the device (L265). `AMFContext1` is used only for the ROI map's `AllocSurfaceEx` (L695-698).
No `InitDX12`, no `InitVulkan`.

The texture ring is **demand-grown, not fixed** (`amf_encode_tex`, L823-897):

```cpp
km->AcquireSync(lock_key, INFINITE);
context->CopyResource(output_tex, input_tex);
context->Flush();
km->ReleaseSync(*next_key);
res = enc->amf_context->CreateSurfaceFromDX11Native(output_tex, &amf_surf, enc);
```

`get_output_tex` pops from `available_textures` or clones a new one from the input desc
(`BindFlags = RENDER_TARGET|SHADER_RESOURCE, MiscFlags = 0`, L391-405); the encoder implements
`AMFSurfaceObserver::OnSurfaceDataRelease` (L214-228) to recycle. Format is **NV12 or P010, never
BGRA** on the texture path — `check_texture_encode_capability` (L1179) refuses otherwise. RGBA
(`AMF_SURFACE_RGBA`) appears only on the CPU-upload fallback (L900-946), which proves AMF *accepts*
packed RGB, but OBS does its BGRA→NV12 in its own graphics pipeline. **`AMFVideoConverter` is not
used anywhere in `texture-amf.cpp`.** (The legacy plugin did use it:
`AMF_VIDEO_CONVERTER_OUTPUT_FORMAT = AMF_SURFACE_NV12`, `old/amf-encoder.cpp:183-216`.)

**The submit/query loop** (L736-820), single-threaded, verbatim:

```cpp
bool waiting = true;
while (waiting) {
    if (enc->roi_supported && obs_encoder_has_roi(enc->encoder)) add_roi(enc, amf_surf);
    res = enc->amf_encoder->SubmitInput(amf_surf);
    if (res == AMF_OK || res == AMF_NEED_MORE_INPUT) {
        waiting = false;
    } else if (res == AMF_INPUT_FULL) {
        os_sleep_ms(1);
        uint64_t duration = os_gettime_ns() - ts_start;
        constexpr uint64_t timeout = 5 * SEC_TO_NSEC;
        if (duration >= timeout) throw amf_error("SubmitInput timed out", res);
    } else { throw amf_error("SubmitInput failed", res); }

    AMFDataPtr new_packet;
    do {
        res = enc->amf_encoder->QueryOutput(&new_packet);
        if (new_packet) queued_packets.push_back(new_packet);
        if (res != AMF_REPEAT && res != AMF_OK) throw amf_error("QueryOutput failed", res);
    } while (!!new_packet);
}
```

`AMF_INPUT_FULL` → sleep 1 ms, retry, 5-second deadline. **`QUERY_TIMEOUT` is never set anywhere in
OBS** — the drain is a busy poll. `AMF_EOF` is never handled; there is no `Drain()` and no
`Terminate()` — `amf_destroy` just `delete enc` and lets the smart-pointer refcounts unwind
(L1173-1177). Output→input association is a **custom wide-string property**:
`amf_surf->SetProperty(L"PTS", pts)` on submit (L869), read back with
`data->GetProperty(L"PTS", &packet->pts)` (L575). DTS comes from `data->GetPts()` minus
`dts_offset = min(B_PIC_PATTERN, MAX_CONSECUTIVE_BPICTURES) + 1` (L1729).

**Detection.** A three-stage dance: `LoadLibraryExW(AMF_DLL_NAME, nullptr, LOAD_LIBRARY_AS_DATAFILE)`
as a crash-proof presence probe (L2673-2678, comment: *"Check if the DLL is present before running
the more expensive obs-amf-test.exe, but load it as data so it can't crash us"*); then
**`obs-amf-test.exe` run out-of-process**, passing the DXGI LUID order as argv and parsing its stdout
(L2683-2740, with `// Seems the AMF test subprocess crashed. Better there than here I guess.`); the
child self-kills after 2500 ms. Inside the child the test is
`VendorId == 0x1002` → `D3D11CreateDevice` → `CreateContext` → `InitDX11(device)` →
`CreateComponent(name) == AMF_OK` per codec (`obs-amf-test.cpp:54-101`) — **no caps property query
and no driver-version check.** Finally the parent does `LoadLibraryW` → `AMFInit(AMF_FULL_VERSION,…)`
→ `GetTrace` → `AMFQueryVersion(&amf_version)`, and **`amf_version` is stored and never compared to
anything** (L2750-2779). obs-deps pins the SDK to **AMF 1.5.0** (`obs-deps/deps.ffmpeg/80-amf.ps1`).

The one AMF-quirk comment worth quoting in full (L1389-1392, repeated at L1872 and L2298):

> *"Return a flag indicating if a pipeline flush is needed. Changing the bitrate (or any other
> dynamic property) is updated with the next SubmitInput() call. For CBR mode, flushing the pipeline
> is not needed and could cause unaligned IDRs in an encoder group."*

and L1453: `// A pipeline flush was not requested, however force an IDR frame to align with the
bitrate change.` — i.e. **dynamic property writes take effect on the next `SubmitInput`, with no
`ReInit`**, which is exactly what a live bitrate controller needs.

### 3.2 FFmpeg — `libavcodec/amfenc*.c` + `libavutil/hwcontext_amf.c`

**Loading lives in `hwcontext_amf.c`, not `amfenc.c`.** `amf_load_library()` (hwcontext_amf.c:648-671)
`dlopen`s `AMF_DLL_NAMEA` (`amfrt64.dll` / `libamfrt64.so.1`; on Windows via `compat/w32dlfcn.h`
= `LoadLibrary`), `dlsym`s `"AMFInit"` and `"AMFQueryVersion"`, calls **`AMFQueryVersion` first** and
stores the result, then `init_fun(AMF_FULL_VERSION, &factory)` — passing the **header's** version, not
the runtime's. It does **not gate loading on version**; it gates *features*: P010 is refused below
`AMF_MAKE_FULL_VERSION(1, 4, 32, 0)` (amfenc.c:122-124).

`amf_device_init()` (hwcontext_amf.c:589-646) tries `InitDX11(NULL, AMF_DX11_1)` → on failure
`InitDX9(NULL)` → on failure `QueryInterface(IID_AMFContext1())` + `InitVulkan(NULL)`.
**`AMFContext1` is used only for Vulkan; `AMFContext2` only for DX12.** Deriving from an existing
device is `InitDX11(hwctx->device, AMF_DX11_1)`, **tolerating `AMF_ALREADY_INITIALIZED`**
(:786-801) — worth copying, since a context may legitimately be initialised twice.

Per-encoder property names are made unique with a global counter (amfenc.c:90-100): `L"PtsProp%lld"`
and `L"av_frame_ref%lld"`. Two encoders in one process would otherwise collide on a custom property
name — directly relevant to us, since ScreenExtend runs **one encoder per connected client**.

**`QUERY_TIMEOUT` is set to 1 ms, before `Init`, and read back to detect driver support**
(amfenc_h264.c:549-552, verbatim):

```c
549	    // Wait inside QueryOutput() if supported by the driver
550	    AMF_ASSIGN_PROPERTY_INT64(res, ctx->encoder, AMF_VIDEO_ENCODER_QUERY_TIMEOUT, 1);
551	    res = ctx->encoder->pVtbl->GetProperty(ctx->encoder, AMF_VIDEO_ENCODER_QUERY_TIMEOUT, &var);
552	    ctx->query_timeout_supported = res == AMF_OK && var.int64Value;
```

**The submit tail** (amfenc.c:529-545) — note that on `AMF_INPUT_FULL` the surface is **stashed, not
released**, because it was not consumed:

```c
530	    res = ctx->encoder->pVtbl->SubmitInput(ctx->encoder, (AMFData*)surface);
531	    if (res == AMF_INPUT_FULL) { // handle full queue
532	        //store surface for later submission
533	        *surface_resubmit = surface;
534	    } else {
535	        surface->pVtbl->Release(surface);
536	        AMF_RETURN_IF_FALSE(ctx, res == AMF_OK, AVERROR_UNKNOWN, "SubmitInput() failed with error %d\n", res);
537	            ctx->submitted_frame++;
538	        ret = av_fifo_write(ctx->timestamp_list, &frame->pts, 1);
541	        if(ctx->submitted_frame <= ctx->encoded_frame + output_delay)
542	            return AVERROR(EAGAIN); // too soon to poll or wait
543	    }
```

`output_delay = FFMAX(ctx->max_b_frames, 0) + ((avctx->flags & AV_CODEC_FLAG_LOW_DELAY) ? 0 : 1)`
(amfenc.c:401, :590). **`AV_CODEC_FLAG_LOW_DELAY` drives it to 0** — that is what makes strict
one-in/one-out possible, and Sunshine sets exactly that flag.

**The query** (amfenc.c:562-577) — "no output yet" is detected by **`data == NULL`, not by a result
code**:

```c
566	    AMF_RESULT ret = ctx->encoder->pVtbl->QueryOutput(ctx->encoder, &data);
567	    *buffer = NULL;
568	    if (data) {
569	        AMFGuid guid = IID_AMFBuffer();
570	        data->pVtbl->QueryInterface(data, &guid, (void**)buffer); // query for buffer interface
571	        data->pVtbl->Release(data);
572	        if (amf_release_attached_frame_ref(ctx, *buffer) == AMF_OK)
573	            ctx->hwsurfaces_in_queue--;
574	        ctx->encoded_frame++;
575	    }
576	    return ret;
```

**The receive loop's block-and-wait** (amfenc.c:640-668) is the single most important passage in this
report:

```c
640	    do {
641	        block_and_wait = 0;
642	        // poll data
643	        res_query = amf_query_output(avctx, &buffer);
644	        if (buffer) {
645	            ret = amf_copy_buffer(avctx, avpkt, buffer);
646	            buffer->pVtbl->Release(buffer);
648	            AMF_RETURN_IF_FALSE(ctx, ret >= 0, ret, "amf_copy_buffer() failed with error %d\n", ret);
650	            if (ctx->delayed_drain) { // try to resubmit drain
651	                res = ctx->encoder->pVtbl->Drain(ctx->encoder);
652	                if (res != AMF_INPUT_FULL) {
653	                    ctx->delayed_drain = 0;
654	                    ctx->eof = 1; // drain started
655	                    AMF_RETURN_IF_FALSE(ctx, res == AMF_OK, AVERROR_UNKNOWN, "Repeated Drain() failed with error %d\n", res);
656	                } else {
657	                    av_log(avctx, AV_LOG_WARNING, "Data acquired but delayed drain submission got AMF_INPUT_FULL- should not happen\n");
658	                }
659	            }
660	        } else if (ctx->delayed_drain || (ctx->eof && res_query != AMF_EOF) || (ctx->hwsurfaces_in_queue >= ctx->hwsurfaces_in_queue_max) || surface) {
661	            block_and_wait = 1;
662	            // Only sleep if the driver doesn't support waiting in QueryOutput()
663	            // or if we already have output data so we will skip calling it.
664	            if (!ctx->query_timeout_supported || avpkt->data || avpkt->buf) {
665	                av_usleep(1000);
666	            }
667	        }
668	    } while (block_and_wait);
```

and the stuck-surface resubmit (amfenc.c:675-708), which **harvests output to make room** rather than
just spinning:

```c
675	        if(surface) {
677	            do {
678	                res = ctx->encoder->pVtbl->SubmitInput(ctx->encoder, (AMFData*)surface);
679	                if (res != AMF_INPUT_FULL)
680	                    break;
682	                if (!ctx->query_timeout_supported)
683	                    av_usleep(1000);
685	                // Need to free up space in the encoder queue.
686	                // The number of retrieved outputs is limited currently to 21
687	                amf_query_output(avctx, &buffer);
688	                if (buffer != NULL) {
689	                    ret = av_fifo_write(ctx->output_list, &buffer, 1);
693	            } while(res == AMF_INPUT_FULL);
695	            surface->pVtbl->Release(surface);
```

**Four facts to carry into the Rust port:**

1. **`AMF_REPEAT` and `AMF_NEED_MORE_INPUT` are never named anywhere in `amfenc.c`.** Only
   `AMF_INPUT_FULL`, `AMF_EOF` and `AMF_OK` are tested. `AMF_REPEAT` falls through as "no buffer" and,
   thanks to the `res_query != AMF_EOF` term at :660, does not spin forever. Treating
   `data == NULL` as the "nothing yet" signal is more robust than matching result codes.
2. **The only sleep is `av_usleep(1000)` — 1 ms — and it is skipped entirely when
   `query_timeout_supported` is true**, because `QueryOutput` itself blocks for `QUERY_TIMEOUT` ms.
   This is the recipe: set `QUERY_TIMEOUT = 1` **before `Init`**, read it back, and if honoured never
   sleep.
3. There is **no `amf_submit_delay` or `timer_amf`** in current master — those were pre-2024. The
   modern machinery is `delayed_surface` (the stashed `surface`), `delayed_drain`, and the
   `submitted_frame` / `encoded_frame` counters against `output_delay`.
4. `SubmitInput` is wrapped by the device-context mutex (`amf_submit_frame_locked`, amfenc.c:547-561);
   **`QueryOutput` is not locked.**

**Property order** (amfenc_h264.c:225-552). `AMF_VIDEO_ENCODER_USAGE` is set **first**, before
`FRAMESIZE` and `FRAMERATE` — stricter than OBS, which puts `FRAMESIZE` ahead of it. Then, in source
order: `FRAMESIZE`(:228) → `FRAMERATE`(:230) → `PROFILE`(:256) → `PROFILE_LEVEL`(:265) →
`MAX_NUM_REFRAMES`(:270, from `avctx->refs`) → `ASPECT_RATIO`(:274) → `OUTPUT_COLOR_PROFILE`(:278) →
`OUTPUT_FULL_RANGE_COLOR`(:281) → `COLOR_BIT_DEPTH`(:292) → `OUTPUT_TRANSFER_CHARACTERISTIC`(:294) →
`OUTPUT_COLOR_PRIMARIES`(:296) → `PREENCODE_ENABLE`(:313/:322, forced off for CQP) →
`QVBR_QUALITY_LEVEL`(:328) → `HIGH_MOTION_QUALITY_BOOST_ENABLE`(:333) → `QUALITY_PRESET`(:337) →
`RATE_CONTROL_METHOD`(:341) → `VBV_BUFFER_SIZE`(:345) → `INITIAL_VBV_BUFFER_FULLNESS`(:350) →
`MAX_AU_SIZE`(:355) → `INSERT_AUD`(:359) → `MIN_QP`/`MAX_QP`(:364-374) → `QP_I/P/B`(:378-382) →
`TARGET_BITRATE`(:385) → `PEAK_BITRATE`(:390/:395) → **`LOWLATENCY_MODE`(:401, from the `latency`
option)** → `ENABLE_SMART_ACCESS_VIDEO`(:405 — **and if enabled it *forces* `LOWLATENCY_MODE = true`**
at :414) → `PRE_ANALYSIS_ENABLE`(:421) → the `AMF_PA_*` block(:429-473) → B-frames, **gated on
`GetCaps()` / `AMF_VIDEO_ENCODER_CAP_BFRAMES`**(:478-537) → `ENABLE_VBAQ`(:540/:545) →
`QUERY_TIMEOUT`(:550). **`Init(format, w, h)` at :555.**

**After `Init`** (:559-599) — proof these really are dynamic: `ENFORCE_HRD`(:560),
`FILLER_DATA_ENABLE`(:564), `RATE_CONTROL_SKIP_FRAME_ENABLE`(:568), `DE_BLOCKING_FILTER`(:571),
`IDR_PERIOD`(:575, **from `avctx->gop_size`**), `HEADER_INSERTION_SPACING`(:580),
`INTRA_REFRESH_NUM_MBS_PER_SLOT`(:584), `SLICES_PER_FRAME`(:586), `CABAC_ENABLE`(:590),
`MOTION_HALF_PIXEL`(:594), `MOTION_QUARTERPIXEL`(:598). Then `EXTRADATA` is read back into
`avctx->extradata` (:605-627).

Rate-control auto-detection (:299-310): QP set → CQP; `bit_rate > 0 && rc_max_rate == bit_rate` →
**CBR**; else `PEAK_CONSTRAINED_VBR`.

**Per-frame** (amfenc.c:472-496), all on the **input surface**:

```c
472	    surface->pVtbl->SetPts(surface, frame->pts);
474	    AMF_ASSIGN_PROPERTY_INT64(res, surface, ctx->pts_property_name, frame->pts);
478	        AMF_ASSIGN_PROPERTY_BOOL(res, surface, AMF_VIDEO_ENCODER_STATISTICS_FEEDBACK, 1);
479	        AMF_ASSIGN_PROPERTY_INT64(res, surface, AMF_VIDEO_ENCODER_INSERT_AUD, !!ctx->aud);
480	        switch (frame->pict_type) {
481	        case AV_PICTURE_TYPE_I:
482	            if (ctx->forced_idr) {
483	                AMF_ASSIGN_PROPERTY_INT64(res, surface, AMF_VIDEO_ENCODER_INSERT_SPS, 1);
484	                AMF_ASSIGN_PROPERTY_INT64(res, surface, AMF_VIDEO_ENCODER_INSERT_PPS, 1);
485	                AMF_ASSIGN_PROPERTY_INT64(res, surface, AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE, AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR);
486	            } else {
487	                AMF_ASSIGN_PROPERTY_INT64(res, surface, AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE, AMF_VIDEO_ENCODER_PICTURE_TYPE_I);
488	            }
```

and the key-frame readback on the **output buffer** (amfenc.c:208-215):

```c
210	        buffer->pVtbl->GetProperty(buffer, AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE, &var);
211	        pkt->flags |= AV_PKT_FLAG_KEY * (var.int64Value == AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR);
```

**Only `_IDR` sets the keyframe flag — a plain `_I` does not.** For a WHEP/WebRTC stream where a
joiner must be able to start decoding, that distinction is the difference between a working join and
a green screen.

**Surface handling.** `CreateSurfaceFromDX11Native(context, texture, &surface, NULL)` — **a NULL
observer**; FFmpeg does not use `AMFSurfaceObserver` at all. Instead (amfenc.c:368-378, :445-450,
:572-573) it `av_frame_clone()`s the frame, stores the clone's raw pointer as an `int64` property
named `av_frame_ref<N>` **on the surface**, and increments `hwsurfaces_in_queue`. **AMF propagates
that property onto the output buffer**, so when the packet emerges `amf_release_attached_frame_ref()`
frees the clone. That is both the lifetime mechanism and the backpressure signal —
`hwsurfaces_in_queue >= hwsurfaces_in_queue_max` (default `async_depth` = 16) is one of the
block-and-wait conditions. It is a neat trick and cheaper than an observer vtable, but it depends on
AMF copying input-surface properties to the output buffer.

It also calls **`SetCrop(surface, 0, 0, frame->width, frame->height)`** because *"input HW surfaces
can be vertically aligned by 16"* — a real hazard for us, since our virtual displays are arbitrary
client-driven sizes. And the texture-array slice index is passed through **D3D11 private data**, not
an AMF call (amfenc.c:408-414):

```c
408	static const GUID AMFTextureArrayIndexGUID = { 0x28115527, 0xe7c3, 0x4b66, { 0x99, 0xd3, 0x4f, 0x2a, 0xe6, 0xb4, 0x7f, 0xaf } };
413	    texture->lpVtbl->SetPrivateData(texture, &AMFTextureArrayIndexGUID, sizeof(index), &index);
414	    res = context->pVtbl->CreateSurfaceFromDX11Native(context, texture, &surface, NULL);
```

Format map (`hwcontext_amf.c`): `NV12→NV12`, `BGR0→BGRA`, `RGB0→RGBA`, `BGRA→BGRA`, `ARGB→ARGB`,
`RGBA→RGBA`, `GRAY8→GRAY8`, `YUV420P→YUV420P`, `YUYV422→YUY2`, `P010→P010`,
`X2BGR10→R10G10B10A2`, `RGBAF16→RGBA_F16` — **packed RGB is a first-class encoder input.**

**Teardown** (`ff_amf_encode_close`, amfenc.c:140-167 + hwcontext_amf.c:524-559):
`encoder->Terminate()` → `encoder->Release()` → unref device ctx → free FIFOs → release leftover
`AMFBuffer`s → free the wide-string property names; then `context->Terminate()` →
`context->Release()` → `trace->UnregisterWriter(...)` → `dlclose(library)`. **Component before
context, context before dlclose** — the same order AMD's own `SimpleEncoder.cpp:284` uses
(surface = NULL → `encoder->Terminate()` → encoder = NULL → `context->Terminate()` → context = NULL).

**The AVOption table** (amfenc_h264.c:32-194), abridged to the latency-relevant rows:

| Option | Field | Type / default | AMF property |
|---|---|---|---|
| `usage` | `usage` | int, **−1 (unset)**, max `LOW_LATENCY_HIGH_QUALITY` | `USAGE` — consts `transcoding`, `ultralowlatency`, `lowlatency`, `webcam`, `high_quality`, `lowlatency_high_quality` |
| `profile` | `profile` | int, −1 | `PROFILE` — `main`, `high`, `constrained_baseline`, `constrained_high` |
| `level` | `level` | int, 0 (`auto`), 0–62 | `PROFILE_LEVEL` |
| **`latency`** | `latency` | **bool, −1** | **`LOWLATENCY_MODE`** |
| `quality` / `preset` | `quality` | int, −1 | `QUALITY_PRESET` — `balanced`, `speed`, `quality`, `high_quality` |
| `rc` | `rate_control_mode` | int, `..._UNKNOWN` | `RATE_CONTROL_METHOD` — `cqp`, `cbr`, `vbr_peak`, `vbr_latency`, `qvbr`, `hqvbr`, `hqcbr` |
| `qvbr_quality_level` | | int, −1, 0–51 | `QVBR_QUALITY_LEVEL` |
| `enforce_hrd` | | bool, −1 | `ENFORCE_HRD` |
| `filler_data` | | bool, −1 | `FILLER_DATA_ENABLE` |
| `vbaq` | `enable_vbaq` | bool, −1 | `ENABLE_VBAQ` |
| `frame_skipping` | `skip_frame` | bool, −1 | `RATE_CONTROL_SKIP_FRAME_ENABLE` |
| `qp_i` / `qp_p` / `qp_b` | | int, −1, 0–51 | `QP_I` / `QP_P` / `QP_B` |
| `preencode` | | bool, −1 | `PREENCODE_ENABLE` |
| `max_au_size` | | int, −1 | `MAX_AU_SIZE` |
| `header_spacing` | | int, −1, 0–1000 | `HEADER_INSERTION_SPACING` |
| **`async_depth`** | `hwsurfaces_in_queue_max` | **int, default 16**, 1..`MAX_LOOKAHEAD_DEPTH+1`; *"Set maximum encoding parallelism. **Higher values increase output latency.**"* | (FFmpeg-side queue cap, not an AMF property) |
| `bf` | `max_b_frames` | int, −1, −1..3 | `B_PIC_PATTERN` |
| `max_b_frames` | `max_consecutive_b_frames` | int, −1, −1..3 | `MAX_CONSECUTIVE_BPICTURES` |
| `bf_delta_qp` / `bf_ref` / `bf_ref_delta_qp` | | 4 / true / 4 | `B_PIC_DELTA_QP` / `B_REFERENCE_ENABLE` / `REF_B_PIC_DELTA_QP` |
| **`intra_refresh_mb`** | | **int, −1 (off)**, −1..INT_MAX | `INTRA_REFRESH_NUM_MBS_PER_SLOT` |
| `coder` | `coding_mode` | int, `UNDEFINED` | `CABAC_ENABLE` — `auto`, `cavlc`, `cabac` |
| `high_motion_quality_boost_enable` | | bool, −1 | `HIGH_MOTION_QUALITY_BOOST_ENABLE` |
| `me_half_pel` / `me_quarter_pel` | | bool, −1 | `MOTION_HALF_PIXEL` / `MOTION_QUARTERPIXEL` |
| `forced_idr` | | bool, **0** | per-frame `FORCE_PICTURE_TYPE = IDR` + `INSERT_SPS`/`INSERT_PPS` |
| `aud` | | bool, −1 | per-frame `INSERT_AUD` |
| `smart_access_video` | | bool, −1 | `ENABLE_SMART_ACCESS_VIDEO` (forces `LOWLATENCY_MODE = true`) |
| `preanalysis` | | bool, −1 | `PRE_ANALYSIS_ENABLE` |
| `pa_*` (13 options) | | −1 | the `AMF_PA_*` block, incl. `pa_lookahead_buffer_depth`, `pa_ltr_enable`, `pa_paq_mode`, `pa_taq_mode`, `pa_caq_strength`, `pa_adaptive_mini_gop` |

**`log_to_dbg` no longer exists in FFmpeg master** — Sunshine still passes it and it is silently
ignored. Note that almost every option defaults to **−1 = "don't set it, let USAGE decide"**, which is
the right default for a wrapper and the wrong one for us: we want every latency-relevant knob set
explicitly.

### 3.3 Sunshine (Moonlight host)

**No native AMF path** — Sunshine encodes only through FFmpeg. Its only direct AMF use is a version
gate (`display_vram.cpp:2053-2094`): `LoadLibraryW(AMF_DLL_NAME)` →
`GetProcAddress("AMFQueryVersion")` → `fnAMFQueryVersion(&version)` → `FreeLibrary`. It rejects
**AV1 below `AMF_MAKE_FULL_VERSION(1,4,30,0)`** (*"AMF 1.4.30 adds ultra low latency mode for AV1"*)
and **HDR below 1.4.23** because *"Older versions of the AMD AMF runtime can crash when fed P010
surfaces."* Vendor matching is `VendorId == 0x1002` requiring an `_amf`-suffixed encoder name
(:2045-2047).

The `h264_amf` dictionary, verbatim (`video.cpp:1044-1073`):

```cpp
1047	        {"filler_data"s, false},
1048	        {"forced_idr"s, 1},
1049	        {"latency"s, 1},
1050	        {"async_depth"s, 1},
1051	        {"frame_skipping"s, 0},
1052	        {"log_to_dbg"s, []() { return config::sunshine.min_log_level < 2 ? 1 : 0; }},
1055	        {"preencode"s, &config::video.amd.amd_preanalysis},
1056	        {"quality"s, &config::video.amd.amd_quality_h264},
1057	        {"rc"s, &config::video.amd.amd_rc_h264},
1058	        {"usage"s, &config::video.amd.amd_usage_h264},
1059	        {"vbaq"s, &config::video.amd.amd_vbaq},
1060	        {"coder"s, &config::video.amd.amd_coder},
1061	        {"enforce_hrd"s, &config::video.amd.amd_enforce_hrd},
1062	        {"max_au_size"s, &config::video.amd.amd_max_au_size},
1063	      },
1068	      {
1069	        // Fallback options
1070	        {"usage"s, 2 /* AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY */},  // Workaround for https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/410
1071	      },
1072	      "h264_amf"s,
```

The four hard-coded values are **`latency = 1`** (→ `LOWLATENCY_MODE = true`), **`async_depth = 1`**
(→ the FFmpeg-side surface queue is **one deep**, i.e. strictly one frame in flight),
**`filler_data = false`**, and **`forced_idr = 1`**. Everything else — `usage`, `quality`, `rc`,
`vbaq`, `preencode`, `enforce_hrd`, `coder`, `max_au_size` — is a `std::optional<int>` that defaults
to **unset** (`config.h:89-98`), so a stock Sunshine ships **no** opinion on USAGE or rate control for
AMD and lets FFmpeg/AMF defaults stand. `header_insertion_mode` and `gops_per_idr` appear only in the
**HEVC** dict, not H.264.

The generic `AVCodecContext` settings around it are the real latency configuration (`video.cpp`):

- `max_b_frames = 0` — *"B-frames delay decoder output, so never use them"* (:2024-2025)
- `gop_size = INT_MAX`, `keyint_min = INT_MAX` — *"Use an infinite GOP length since I-frames are
  generated on demand"* (:2027-2038). Since FFmpeg maps `gop_size` → `IDR_PERIOD`, **this is the
  shipping answer to "what do I put in `IDRPeriod` for no periodic IDRs": `INT_MAX`, not 0.**
- `flags = AV_CODEC_FLAG_CLOSED_GOP | AV_CODEC_FLAG_LOW_DELAY` (:2049-2051) — `LOW_DELAY` is what
  drives FFmpeg's `output_delay` to 0
- `rc_max_rate = bit_rate = rc_min_rate = bitrate` (:2187-2197) — peak == target
- **`rc_buffer_size = bitrate / framerate`** (:2211) — **a one-frame VBV**, independently confirming
  the number in R3
- No `qmin`/`qmax` for AMD, so `MIN_QP`/`MAX_QP` are never set

The **fallback** is worth copying wholesale: after `avcodec_open2` fails, Sunshine retries with
`usage = AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY` (2). That is a workaround for
[AMF#410](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/410) — **`USAGE =
ULTRA_LOW_LATENCY` broke `Init()` outright on pre-RDNA hardware from driver 23.7.1 onward.** AMD's
own suggested probe is: set `USAGE`, then `GetProperty(PREENCODE_ENABLE)`, and if that errors, fall
back. See R3a.

No intra-refresh and no LTR is configured for AMD anywhere in Sunshine — it relies purely on
on-demand IDR (`forced_idr`) plus an infinite GOP. That is the same design ScreenExtend already has.

### 3.4 Prior art in Rust

Only two crates bind AMD AMF; everything else on crates.io matching "amf" is Action Message Format
(Flash) or additive-manufacturing formats. **No Rust game-streaming or remote-desktop host has a
native AMF encoder** — RustDesk goes through FFmpeg/MediaCodec, `nestri`/`selkies`/`wolf` are
GStreamer-based (`vaapi`/`vah264enc` on AMD), and there is no `sunshine-rs`. A from-scratch binding
is genuinely the state of the art here.

- **`amffi`** — <https://github.com/AlsoSylv/amffi> (MIT, v0.1.0, edition 2024, MSRV 1.88, active
  Mar 2026). The closest prior art and the only one covering **Windows + D3D11 + hand-written
  vtables**. `#[repr(C)]` vtable structs under a `stdcall!` macro that switches convention for x86 vs
  x86_64; `libloading` with the correct per-target names (`amfrt64.dll`/`amfrtlt64.dll`,
  `amfrt32.dll`, `libamfrt64.so.1`, `libamfrt.framework/libamfrt`) and `c"AMFInit"` /
  `c"AMFQueryVersion"`. Module layout mirrors the SDK. **Right:** `query_version()` before
  `init_factory(version)` passing the *runtime* version (unlike FFmpeg); a real
  `cast::<AMFContext1>()`; `AMFError::Repeat` and `AMFError::Eof` as distinct variants.
  **Wrong for us:** its `examples/simple_encode.rs` uses `Usage::Transcoding`, sets no
  `QUERY_TIMEOUT`, and its output thread is a **hot busy-loop** (`Err(AMFError::Repeat) => {}` with
  no sleep and no blocking query, :154-179); it calls `init_dx11(None, DX11_0)` rather than
  `AMF_DX11_1`; and it sets `AMF_VIDEO_ENCODER_AV1_USAGE` from the **HEVC** usage enum (:133-135) —
  a live copy-paste bug of exactly the cross-codec kind flagged in §2.5. No
  `CreateSurfaceFromDX11Native` example. **Read it for the vtable declarations; do not copy the
  encode loop.**
- **`shiguredo_amf`** — <https://github.com/shiguredo/amf-rs> (Apache-2.0, v2026.3.0, active
  Jul 2026). `build.rs` git-clones the SDK at tag `v1.5.2` and runs **bindgen**; it explicitly does
  not link the runtime — `src/dl.rs` hand-rolls `dlopen`/`LoadLibraryW`. Encode + decode for
  H.264/H.265/AV1 plus `reconfigure` for live property changes. **Right:** runtime `dlopen`;
  `terminate()` on component then context (`encode.rs:962-963`); an explicit
  `AMF_INPUT_FULL || AMF_REPEAT` retry on submit; tolerating `AMF_INPUT_FULL` from `Drain` (:928-930)
  exactly as FFmpeg's `delayed_drain` does. **Wrong for us:** hard-coded `USAGE = 0`
  (`TRANSCODING`) for all three codecs (:657, :715, :773), no `QUERY_TIMEOUT`, and a flat
  `thread::sleep(1ms)` on `INPUT_FULL` (:866). It is **Linux/Vulkan-only by design** — no D3D11 path
  — so it is not directly usable, but its `issues/closed/` log is worth reading
  (`0001-bug-thread-safety-sigsegv.md`, `0004-enhance-raii-wrapper-for-amf-objects.md`,
  `0005-change-zero-copy-encode-decode.md`).

**Both existing Rust bindings default to `USAGE = TRANSCODING` and neither sets `QUERY_TIMEOUT`.**
Per AMD's own documentation, transcoding mode *"needs to accept at least 3 input frames before any
output is produced"* — so both would add ~3 frames of latency out of the box. Whatever we build will
be, as far as this search can establish, the first low-latency AMF binding in Rust.

### 3.5 AMD's own documentation and issue tracker

The SDK ships six markdown docs on master (`amf/doc/AMF_Video_Encode_API.md`, `_HEVC_`, `_AV1_`,
`AMF_API_Reference.md`, `AMF_Video_PreAnalysis_API.md`, plus Display_Capture / FRC / HQ_Scaler /
VQ_Enhancer / Video_Converter / Video_Decode / PreProcessing).

**The per-USAGE default table** (Encode doc, Annex A) is the most useful thing in them — it is what
`USAGE` actually *does*:

| | Transcode | **ULTRA_LOW_LATENCY** | LOW_LATENCY | Webcam | HIGH_QUALITY | LL_HIGH_QUALITY |
|---|---|---|---|---|---|---|
| RateControl | PeakVBR | **LatencyConstrainedVBR** | PeakVBR | PeakVBR | QVBR/PeakVBR | CBR |
| VBV size | 20 Mb | **735 kb** | 4 Mb | 2 Mb | 40 Mb | 10 Mb |
| `ENFORCE_HRD` | false | **true** | false | false | false | false |
| `LOWLATENCY_MODE` | false | **true** | false | false | false | true |
| `INTRA_REFRESH` MBs/slot | 0 | **255** | **255** | 0 | 0 | 0 |
| `PRE_ANALYSIS` | false | false | false | false | **true** | false |
| `QUALITY_PRESET` | BALANCED | SPEED | SPEED | SPEED | QUALITY | QUALITY |
| IDR period | 240 | 240 | 240 | 30 | 240 | 120 |
| `QUERY_TIMEOUT` | 0 | 0 | 0 | 0 | 50 | 50 |

`B_PIC_PATTERN` defaults to 0 in every usage. Three things fall out of this table:

1. **ULL already sets `LOWLATENCY_MODE = true` for you** — our explicit set is belt-and-braces.
2. **ULL already turns intra-refresh on at 255 MBs/slot.** At 1080p (8160 MBs) that is a ~32-frame
   refresh wave, about 0.53 s at 60 fps — a perfectly sensible default we get for free, and something
   we would *break* by setting `MAX_NUM_REFRAMES = 1` (see R7).
3. **ULL's default rate control is `LATENCY_CONSTRAINED_VBR`, not CBR**, with a **735 kb** VBV.
   Overriding to CBR is a deliberate choice, not a correction — and on GCN parts it may not even be
   possible ([AMF#301](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/301) reports
   `RATE_CONTROL_METHOD` cannot be overridden after ULL on RX 580 / Vega).

The doc's own latency statement (§2.1), verbatim: *"In the Transcoding mode the encoder needs to
accept at least 3 input frames before any output is produced. In low latency modes output becomes
available as soon as the first submitted frame is encoded."* AMD repeats this in
[AMF#312](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/312). **That is a ~3-frame
difference — 50 ms at 60 fps — and it is decided entirely by one property.**

The **USAGE-first rule is not in the `.md` docs.** It is stated in AMD's own sample,
`EncoderLatency.cpp:378/420/460`: `// AMF_VIDEO_ENCODER_USAGE needs to be set before the rest,
default transcoding`, and confirmed by AMD in
[AMF#517](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/517): *"Setting USAGE you overwrite
most of previous settings including rate control method."*

**Issue-tracker findings that change the design:**

- **`AMF_INPUT_FULL` means the output is not being drained.**
  [AMF#274](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/274): poll `QueryOutput` until it
  produces data, *then* resubmit. AMD in
  [AMF#502](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/502): *"the main AMF model is to
  use two threads"*, and `AMF_NEED_MORE_INPUT` is **not** a reliable "don't query" signal.
  [AMF#472](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/472): `AMF_REPEAT` just means "no
  output yet" and is unrelated to `Flush()`; end a stream with `Drain()` then query until `AMF_EOF`,
  and reserve `Flush()` for reinit.
- **`QUERY_TIMEOUT` is effectively static**, despite the header filing it under "Dynamic".
  [AMF#516](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/516): AMD says set it **before
  `Init()`**; setting it afterwards returns `AMF_OK` and `GetProperty` echoes it back, but it
  **silently does nothing**. FFmpeg's placement (pre-`Init`, then read back) is therefore correct and
  ours must match. AMD also recommends a 1 ms timeout over `Sleep`, which needs
  `amf_increase_timer_precision()`.
- **`D3D11_CREATE_DEVICE_VIDEO_SUPPORT` is a myth** for AMF — AMD's own sample, OBS and Sunshine's
  AMF path all use `createDeviceFlags = 0`. (Sunshine does pass it, but its comment says
  *"QuickSync requires our device to be multithread-protected"* — an Intel requirement applied
  unconditionally.) **`ID3D10Multithread::SetMultithreadProtected(TRUE)` is not a myth**: AMF's own
  `DeviceDX11.cpp:189-194` calls it unconditionally, and AMD's fix for flicker + growing latency on a
  Desktop-Duplication device was *"do not use `D3D11_CREATE_DEVICE_SINGLETHREADED`"* plus an explicit
  `Flush()` + `D3D11_QUERY_EVENT` fence after `CopyResource` before `ReleaseFrame`
  ([AMF#92](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/92)).
- **BGRA input silently invokes an internal GPU color converter that competes with the foreground
  application** ([AMF#92](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/92)). This is the
  strongest argument for R5 — it is not just an extra pass, it is an *uncontrolled* extra pass.
- **`OUTPUT_COLOR_PROFILE` never reaches the bitstream on any codec.** Only
  `AMF_VIDEO_ENCODER_FULL_RANGE_COLOR` (H.264) and `HEVC_NOMINAL_RANGE` actually work
  ([AMF#398](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/398), still reproducing on
  24.8.1; also [#277](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/277),
  [#540](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/540)). Corrects R3 row 29.
- **`CreateSurfaceFromDX11Native` does not migrate or re-flag a texture**
  ([AMF#605](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/605)): a texture lacking
  `D3D11_BIND_SHADER_RESOURCE` fails with `AMF_DIRECTX_FAILED` (18) **at the first `QueryOutput`**,
  not at wrap time, and `Duplicate()` does not help. Confirms both the cross-adapter analysis in §2.8
  and that our NV12 texture must carry `D3D11_BIND_SHADER_RESOURCE`.
- **Refcounts, not an explicit release rule.** Holding an output `AMFData` keeps input surfaces alive
  and starves the pool; never `Detach()`
  ([#205](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/205),
  [#270](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/270)). The surface observer fires at
  refcount 0.
- **Dynamic bitrate works but "you cannot change it too often"**
  ([AMF#517](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/517)). Most "bitrate is ignored"
  reports ([#536](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/536),
  [#39](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/39),
  [#18](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/18)) are really the USAGE-overwrite
  trap. Our loss-based controller already smooths and rate-limits (`bitrate.rs`); keep it that way.
- **Do not parse the DLL's `ProductVersion` string** — a 2025 driver truncated it to
  `1.5.0.0;1.2;CL` ([AMF#579](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/579)). Use
  `AMFQueryVersion`. Error 126 on `LoadLibrary` happens on minimal driver installs / missing VC
  runtime ([#11](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/11)).

**LTR detail from the docs** (not in the headers): `MAX_LTR_FRAMES` range is **0–2**, further bounded
by the H.264 level's DPB limit. `0` = the encoder may use LTRs internally; `>0` = full user control,
and *"With user control of LTR, B-pictures and Intra-refresh features are not supported."*
`FORCE_LTR_REFERENCE_BITFIELD` both restricts prediction to the named LTRs **and disallows LTRs not
in the bitfield from current and future reference**. Separately, intra-refresh, `B_PIC_PATTERN`,
`B_PIC_DELTA_QP` and `B_REFERENCE_ENABLE` are available **only when `MaxOfReferenceFrames > 1` and
`NumOfLTR == 0`.**

**`OUTPUT_MODE = SLICE` is real and dated**: frame-in → slice-out arrived in **AMF 1.4.30 /
Adrenalin 23.5.2**, gated on `AMF_VIDEO_ENCODER_CAP_SUPPORT_SLICE_OUTPUT`. Each `QueryOutput` yields
one slice and the buffer carries `OUTPUT_BUFFER_TYPE ∈ {FRAME, SLICE, SLICE_LAST}`; AMD's
`EncoderLatency.cpp:594` uses `SLICE_LAST || FRAME` as the end-of-frame marker. (The AVC `.md` doc
wrongly names the AVC enumerant `_TILE` — copy-pasted from AV1. **Trust the header**, which says
`_SLICE`.)

**`ENABLE_SMART_ACCESS_VIDEO`** (Encode doc §2.2.9) is *"parallelization of encode and decode streams
across multiple VCN hardware instances"* on supported APU+dGPU pairings — **throughput, not
single-stream latency**. Confirms leaving it off. Note
[AMF#562](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/562): random `AMF_INPUT_FULL` with
AV1 + pre-analysis on RDNA4.

---

## 4. What ScreenExtend does today

- **AMD hosts get no hardware encoder at all.** `Backend` has four variants —
  `Nvenc`, `Intel`, `IntelCpu`, `X264` (`windows_utils/streamer/pipeline.rs:770-775`) — and
  `build_backend`'s `EncoderVendor::Auto` chain is NVENC → Intel → x264
  (`pipeline.rs:1443-1455`). There is no AMD branch. On a Radeon box `try_nvenc` fails (no adapter
  whose description contains "NVIDIA", `nvidia/encoder.rs:871-880`), `try_intel` fails
  (`device_vendor(device) != Vendor::Intel`), and the session silently lands on `Backend::X264`.
  Already filed as `findings/major-amd-hosts-get-no-hardware-encoder.md`.
- **`amd.rs` is a 19-line stub.** `AmfBackend::{new,encode,set_bitrate}` all
  `bail!("AMD AMF encoder backend not implemented")` (`windows_utils/streamer/amd.rs:5-19`), and
  nothing constructs it.
- **The `EncoderBackend` trait is effectively dead.** `crate::streamer::platform::EncoderBackend`
  (`streamer/platform.rs:169-177`) has exactly one implementor — `AmfBackend` — and no caller. The
  real backend surface is the `Backend` enum's inherent methods (`name`, `device`, `set_bitrate`,
  `wants_prescale`, `is_cpu_bridge`, `encode_gpu`, `encode_cpu`, `encode_repeat`,
  `pipeline.rs:777-886`), whose signatures the trait does not model (no texture, no repeat, no
  prescale hint). Implementing `EncoderBackend` for AMF would produce a backend the pipeline cannot
  call.
- **`EncoderVendor` has no `Amd` variant.** `streamer/config.rs:29-47`:
  `Auto | Nvidia | Intel | Software`, and `parse` accepts no AMD spelling — so
  `--encoder amd` is unrepresentable, and `platform::probe_encode`'s match
  (`streamer/platform.rs:114-140`) has nowhere to dispatch an AMD probe.
- **`select_vendor` matches description strings, not vendor IDs.**
  `windows_utils/streamer/mod.rs:23-33` uppercases the DXGI description and looks for
  `"NVIDIA"` / `"INTEL"` / `"AMD" | "RADEON" | "ADVANCED MICRO DEVICES"`. `Vendor::Amd` exists but
  its only consumer is `device_vendor` (`mod.rs:35-59`), whose only caller is
  `IntelEncoder::new_on_device`'s Intel guard.
- **The scaler cannot produce NV12.** `Scaler::new` hard-codes the destination texture to
  `DXGI_FORMAT_B8G8R8A8_UNORM` (`scaler.rs:76`). The D3D11 `VideoProcessor` machinery is already
  there (`ID3D11VideoDevice`/`ID3D11VideoContext`/`ID3D11VideoProcessor`, `scaler.rs:19-33`) and
  can emit NV12 with a format change plus an NV12 output view — the work is real but small.
- **The NVENC backend is the shape to copy.** `nvidia/encoder.rs` already establishes:
  the `libloading` + function-table load with a driver-too-old check
  (`encoder.rs:101-118`), a shared keyed-mutex input texture (`setup_shared`, `encoder.rs:265-280`),
  a ring variant (`new_shared_ring`, `encoder.rs:97-195`), `encode_input`/`encode_repeat`/
  `encode_input_slot`/`encode_repeat_slot` (`encoder.rs:293-458`), live bitrate reconfiguration
  without teardown (`set_bitrate`, `encoder.rs:460-491`), `request_idr` as a latched flag
  (`encoder.rs:493-495`), `sequence_params()` for out-of-band SPS/PPS (`encoder.rs:497-515`), and
  the `_lib: libloading::Library` declared **last** so it drops last (`encoder.rs:83`).
  The ULL configuration it applies — `NV_ENC_PRESET_P1_GUID` + `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`,
  `gopLength = NVENC_INFINITE_GOPLENGTH`, `frameIntervalP = 1` (no B), `enableLookahead(0)`,
  `enableAQ(0)`, `multiPass = DISABLED`, `zeroReorderDelay(1)`,
  `vbvBufferSize = bitrate/fps`, `vbvInitialDelay = vbvBufferSize`, `maxNumRefFrames = 1`,
  `repeatSPSPPS(1)`, `enableFillerDataInsertion(0)`, `sliceMode = 0` (**one slice**),
  `enableIntraRefresh` with `intraRefreshPeriod = fps*4` / `intraRefreshCnt = fps/2`
  (`encoder.rs:540-608`) — is the property-for-property template for the AMF table in §5.
  Note `sliceMode = 0`: **the shipping NVENC path uses one slice per frame**, not the 4–8 that
  `amd.md` §3 asks for.
- **The pipeline already owns everything else.** Cross-adapter shared texture
  (`build_zero_copy`, `pipeline.rs:1009-1032`), latest-wins encode with a repeater for the static
  desktop (`spawn_repeater`, `pipeline.rs:1456+`), the `EncodeCore` bitrate/IDR plumbing
  (`pipeline.rs:882-900`), and `EncoderConfig` (`pipeline.rs:111-121`:
  `width, height, fps, bitrate_bps, max_bitrate_bps, profile, qp, intra_refresh`). An AMF backend
  needs to satisfy that interface and nothing more.

---

## 5. Recommendations

Ranked by expected latency delta per unit of implementation cost. Every one has a `findings/` file.

### R1 — Ship the AMF backend at all *(delta: enormous; cost: large)*

`findings/major-amd-hosts-get-no-hardware-encoder.md` (pre-existing).

Today an AMD host encodes 1080p60 H.264 **on the CPU with libx264**, competing with capture threads
running at `THREAD_PRIORITY_TIME_CRITICAL` (`tuning.rs:247-251`). Hardware AVC on any VCN generation
encodes 1080p in single-digit milliseconds; x264 at a latency-safe preset is several times that
*plus* the frame-level threading it must disable to stay low-latency. This is the whole point of the
work; everything below is refinement.

Shape: `windows_utils/streamer/amd/{mod.rs, encoder.rs, amf_sys/{mod.rs, ...}}`, mirroring
`nvidia/`. Add `Backend::Amf { encoder, path }` to `pipeline.rs:770`, an `EncoderVendor::Amd` to
`config.rs:29`, a `try_amf()` to `build_backend`, and put it in the `Auto` chain **after Intel and
before x264** (an Intel iGPU + Radeon dGPU laptop should still prefer the same-adapter Intel path;
an all-AMD box never reaches the Intel branch because `device_vendor != Intel`).

Probe: `--probe-encode --encoder amd` writing `out_amd.h264`, wired through
`streamer/platform.rs:114-140`.

### R2 — Pin `AMFInit`'s version low, gate features by probe *(delta: compatibility; cost: trivial)*

`findings/major-amd-amfinit-version-pin.md`

Call `AMFInit(AMF_MAKE_FULL_VERSION(1,4,30,0), &factory)`, not `AMF_FULL_VERSION` of the headers we
vendor. AMF's contract is that the runtime serves clients built against **older** SDKs; asking for
1.5.2 on a 2019-era driver risks an outright `AMFInit` failure on a machine whose VCN can encode
perfectly well. OBS ships against 1.5.0 and does not compare versions at all. Log
`AMFQueryVersion`'s value with `tprintln!` for support, and derive *nothing* from it — use
`HasProperty` and `SetProperty`'s return code for every optional knob.

Measure: on the dev box (no AMD GPU) nothing; on an AMD box, `AMFInit` returns `AMF_OK` and the log
line reports the runtime version.

### R3 — The latency-optimal AVC property table *(delta: 1–3 frames vs. OBS defaults; cost: small)*

`findings/major-amd-amf-latency-property-set.md`

Set in exactly this order, all before `Init()` unless marked. `Bool` = `AMF_VARIANT_BOOL`,
`Int64` = `AMF_VARIANT_INT64`, `Size`/`Rate` as noted.

| # | Property (wide string) | Value | Why |
|---|---|---|---|
| 1 | `L"Usage"` | `Int64(AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY)` = 1 | **First. Re-defaults every other RC/GOP property**, so anything set before it is lost. ULL presets no B-frames, no lookahead, tight RC. |
| 2 | `L"QualityPreset"` | `Int64(AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED)` = 1 | Fastest VCN algorithm path. **Note the value differs per codec** (§2.5). |
| 3 | `L"FrameSize"` | `Size(w, h)` | Also passed to `Init`; set both. |
| 4 | `L"FrameRate"` | `Rate(fps, 1)` | Wrong fps mis-sizes the VBV and the RC per-frame budget. |
| 5 | `L"Profile"` | `Int64(66 / 77 / 100)` from `EncoderConfig::profile` | Match the NVENC mapping (`nvidia/encoder.rs:560-564`). |
| 6 | `L"ProfileLevel"` | `Int64` computed from `w*h*fps`, else leave default | AMF defaults to the max level; some client decoders refuse a level above their tier. OBS has a whole table for this (`texture-amf.cpp:74-78`). Our clients are browsers, which are lenient — leave default unless a device complains. |
| 7 | `L"RateControlMethod"` | `Int64(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR)` = **1** | Predictable per-frame size = predictable pacing. `LATENCY_CONSTRAINED_VBR` (3) is the alternative — see below. |
| 8 | `L"TargetBitrate"` | `Int64(bitrate_bps)` | dynamic |
| 9 | `L"PeakBitrate"` | `Int64(bitrate_bps)` (== target) | dynamic; peak == target stops one frame ballooning and head-of-line-blocking the next |
| 10 | `L"VBVBufferSize"` | `Int64(bitrate_bps / fps)` | **The single biggest CBR latency lever.** One frame of bits. Mirrors `rcParams.vbvBufferSize` in the NVENC path (`nvidia/encoder.rs:582`). OBS's `LATENCY_CONSTRAINED_VBR` branch uses `(bitrate/fps)*1.1`; if CBR proves too tight, use ×1.1. |
| 11 | `L"InitialVBVBufferFullness"` | **leave default (64 = 100%)** | `amd.md` says set 0; that is 0% = empty and *worse*. |
| 12 | `L"EnforceHRD"` | `Bool(false)` | HRD conformance forces the RC to obey a buffer model we do not need; the receiver is WebRTC, not a broadcast decoder. |
| 13 | `L"FillerDataEnable"` | `Bool(false)` | Filler pads to hit CBR exactly — pure wasted transmit time on our link. (OBS sets `true`; that is for a fixed-bitrate RTMP muxer, not us.) |
| 14 | `L"BPicturesPattern"` | `Int64(0)` | **No B-frames.** A B-frame needs a future frame → ≥1 frame of reorder delay. Non-negotiable. |
| 15 | `L"MaxConsecutiveBPictures"` | `Int64(0)` | Belt and braces; set only if `CAP_BFRAMES` is true (on VCN parts without B support, setting it may return `AMF_NOT_SUPPORTED`). |
| 16 | `L"LowLatencyInternal"` | `Bool(true)`, **probe first** | *"enables low latency mode and POC mode 2 in the encoder."* Not on every driver — `HasProperty` then `SetProperty`, and log-and-continue on `AMF_NOT_SUPPORTED`. |
| 17 | `L"RateControlPreanalysisEnable"` | `Int64(AMF_VIDEO_ENCODER_PREENCODE_DISABLED)` = 0 | Pre-encode is a first analysis pass = one frame of lookahead. Off. (OBS sets it **on**.) |
| 18 | `L"EnablePreAnalysis"` | `Bool(false)` | The full PA module is a lookahead pipeline (`AMF_PA_LOOKAHEAD_BUFFER_DEPTH`). Off. |
| 19 | `L"EnableVBAQ"` | `Bool(false)` | Variance-based adaptive quantization is a quality feature with a per-frame analysis cost. Off. |
| 20 | `L"HighMotionQualityBoostEnable"` | `Bool(false)` | Same. |
| 21 | `L"IDRPeriod"` | **`Int64(i32::MAX)`** | **Keyframes on demand only** — join, PLI, FIR. Periodic IDRs are bitrate spikes = latency spikes. Mirrors `NVENC_INFINITE_GOPLENGTH` (`nvidia/encoder.rs:565,593`). Use a huge number, **not 0**: Sunshine sets `gop_size = INT_MAX` with the comment *"Use an infinite GOP length since I-frames are generated on demand"* (`video.cpp:2027-2038`) and FFmpeg maps `gop_size` straight to `IDR_PERIOD` (amfenc_h264.c:575). Zero's meaning is undocumented for AVC (HEVC's `NUM_GOPS_PER_IDR` documents 0 = never, AVC's `IDR_PERIOD` does not). |
| 22 | `L"IntraPeriod"` | `Int64(0)` | Default; no periodic I. |
| 23 | `L"HeaderInsertionSpacing"` | `Int64(0)` | Do **not** spam SPS/PPS every N frames — bytes on the wire we do not need. In-band parameter sets come from the per-surface `InsertSPS`/`InsertPPS` on forced IDRs instead. |
| 24 | `L"DeBlockingFilter"` | `Bool(true)` (default) | Free on the encoder, improves what the receiver shows. |
| 25 | `L"SlicesPerFrame"` | `Int64(1)` | See R6 — do **not** set 4–8 by default. |
| 26 | `L"CABACEnable"` | `Int64(AMF_VIDEO_ENCODER_UNDEFINED)` = 0 for Main/High; `Int64(AMF_VIDEO_ENCODER_CALV)` = 2 for Baseline | Matches `nvidia/encoder.rs:610-630`, where Baseline forces CAVLC for a cheaper client-side decode. |
| 27 | `L"HalfPixel"` / `L"QuarterPixel"` | leave default (`true`) | Sub-pel ME is on the fixed-function block; disabling it costs quality and saves no wall-clock. |
| 28 | `L"InputFullRangeColor"` and `L"FullRangeColor"` | `Bool(true)` | We feed full-range RGB and the NVENC path already signals `videoFullRangeFlag = 1` (`nvidia/encoder.rs:635`). Keep the two backends consistent or AMD hosts will look washed out relative to NVIDIA ones. **`FullRangeColor` is the only colour property AMF actually writes into the AVC bitstream** — see next row. |
| 29 | `L"OutColorProfile"` / `L"OutColorTransferChar"` / `L"OutColorPrimaries"` | set them, but **do not rely on them** | [AMF#398](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/398) (still reproducing on driver 24.8.1): `OUTPUT_COLOR_PROFILE` **never reaches the bitstream on any codec**; only `FULL_RANGE_COLOR` (H.264) and `HEVC_NOMINAL_RANGE` work. The NVENC path writes a real VUI (`nvidia/encoder.rs:632-640`); AMD will not. If the client renders AMD streams differently from NVIDIA ones, this is why — the fix is client-side (assume BT.709 full-range) or a hand-patched SPS, not a property. |
| 30 | `L"QueryTimeout"` | `Int64(1)` — **and read it back** | **Must be set before `Init()`** ([AMF#516](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/516)); setting it after returns `AMF_OK` and does nothing. FFmpeg's exact recipe (amfenc_h264.c:549-552): set 1 ms, `GetProperty` it back, and if the value survives, let `QueryOutput` block instead of ever sleeping. Also check `L"QueryTimeoutSupport"` in caps. |
| 31 | `L"InputQueueSize"` | `Int64(1)` if writable, else leave 16 | The default 16-deep input queue is a latency reservoir we never want to fill. With one-in-flight discipline it stays empty anyway, but a hard 1 makes `AMF_INPUT_FULL` a real signal instead of a 16-frame cushion. Sunshine achieves the same thing FFmpeg-side with `async_depth = 1`. Probe: it may be static-only or read-only. |
| 32 | `L"EnableEncoderSmartAccessVideo"` | `Bool(false)` | SmartAccess Video distributes encode across iGPU+dGPU for **throughput**; splitting a single low-latency stream across engines adds coordination, not speed. Leave off. (`CAP_SUPPORT_SMART_ACCESS_VIDEO` reports availability.) |
| 33 | `L"StatisticsFeedback"` / `L"PSNRFeedback"` / `L"SSIMFeedback"` / `L"BlockQpFeedback"` | leave `false` | Each makes the encoder compute and attach extra data per frame. |
| 34 | `L"PicTransferMode"` | leave `OFF` | Exchanging reconstructed pictures with the app is a debugging feature. |
| 35 | `L"MaxNumRefFrames"` | **do not set — leave the USAGE default** | `amd.md` says 1. Do not: the docs state intra-refresh (and `B_PIC_PATTERN`, `B_PIC_DELTA_QP`, `B_REFERENCE_ENABLE`) are available **only when `MaxOfReferenceFrames > 1` and `NumOfLTR == 0`**, and ULL turns intra-refresh on for you at 255 MBs/slot. Forcing 1 ref silently disables the free loss resilience and buys no latency — with `B_PIC_PATTERN = 0` there is no reorder delay regardless of DPB size. |
| 36 | `L"IntraRefreshMBsNumberPerSlot"` | **leave the ULL default (255)** | See §3.5: `USAGE = ULTRA_LOW_LATENCY` already enables a rolling intra-refresh at 255 MBs/slot ≈ a 32-frame wave at 1080p ≈ 0.53 s at 60 fps. That is close to what `amd.md` asks for, for free. Set it explicitly only when `Config::intra_refresh` is off, in which case write `Int64(0)`. |

Per-frame, on the `AMFSurface`:

| Property | When | Why |
|---|---|---|
| `L"ForcePictureType"` = `Int64(AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR)` = 2 | join / PLI / FIR | The **only** keyframe trigger. Wire to `EncodeCore::take_force_idr` (`pipeline.rs:899-900`). |
| `L"InsertSPS"` = `Bool(true)`, `L"InsertPPS"` = `Bool(true)` | on each forced IDR | Guarantees the IDR is self-contained for a mid-stream joiner. |
| `L"InsertAUD"` = `Bool(false)` | always | Access-unit delimiters are bytes the browser does not need. |

Dynamic, on the running component (no `ReInit`, no `Flush`):

| Property | Trigger |
|---|---|
| `L"TargetBitrate"` + `L"PeakBitrate"` + `L"VBVBufferSize"` | `Backend::set_bitrate` from the loss-based controller (`bitrate.rs`). OBS's comment confirms these land on the next `SubmitInput` with no flush. |
| `L"FrameRate"` | only if we ever change the target fps |

Never change resolution at runtime — that is `ReInit`, a full IDR + surface reallocation. And do not
push bitrate updates every frame: AMD's own guidance is *"you cannot change it too often"*
([AMF#517](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/517)). `BitrateController` already
smooths and threshold-gates (`streamer/bitrate.rs`); keep it as the only writer.

Measure: `ffprobe -show_frames out_amd.h264` must show `has_b_frames=0` and only I/P `pict_type`;
the per-60-frame encode-time log must land in the NVENC band; a 5-minute run's **max** (not average)
encode time must not creep.

### R3a — Verify `USAGE` took, and fall back to `LOW_LATENCY` when it did not *(delta: prevents a hard failure on pre-RDNA; cost: trivial)*

`findings/major-amd-ull-usage-fallback.md`

**`USAGE = ULTRA_LOW_LATENCY` outright breaks `Init()` on pre-RDNA hardware from driver 23.7.1
onward** — [AMF#410](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/410). Sunshine hardcodes
the workaround (`video.cpp:1068-1071`: retry the whole encoder open with
`usage = 2 /* AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY */`), which is also the root cause of
[Sunshine#1559](https://github.com/LizardByte/Sunshine/issues/1559). AMD's own suggested probe is
cheaper than a full retry:

```
enc->SetProperty(L"Usage", Int64(ULTRA_LOW_LATENCY))
if enc->GetProperty(L"RateControlPreanalysisEnable", &var) != AMF_OK {
    teprintln!("AMF: ULTRA_LOW_LATENCY usage rejected by this driver/GPU; falling back to LOW_LATENCY");
    enc->SetProperty(L"Usage", Int64(LOW_LATENCY))
}
```

Separately, on GCN parts (RX 580, Vega) `RATE_CONTROL_METHOD` reportedly **cannot be overridden at
all after ULL** ([AMF#301](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/301)) — you get
ULL's default `LATENCY_CONSTRAINED_VBR` whatever you write. So after setting `RATE_CONTROL_METHOD`,
read it back and log what actually stuck. LCVBR with a one-frame VBV is a perfectly acceptable
outcome; silently believing you are in CBR when you are not is what makes bitrate control look
broken.

`LOW_LATENCY` (2) differs from ULL only in rate control (PeakVBR), VBV (4 Mb) and `LOWLATENCY_MODE`
(false) per the table in §3.5 — all of which we override explicitly anyway — so the fallback costs
almost nothing.

Measure: on an AMD box, log the read-back values of `Usage` and `RateControlMethod` after setting
them. On the dev box, unit-test that a `GetProperty` failure takes the fallback branch.

### R4 — One frame in flight, one thread, blocking `QueryOutput` *(delta: 1–2 frames; cost: small)*

`findings/major-amd-amf-one-frame-in-flight.md`

```
submit(surface) -> loop { QueryOutput } -> release -> repeat
```

Never submit frame N+1 before frame N's packet is out. If you submit ahead, wall-clock latency grows
by (queue depth × frame time) even though throughput looks perfect — this is the classic way an
encoder integration "looks fine" in fps and feels awful in a stopwatch test. Sunshine reaches the
same conclusion from the other end: `async_depth = 1` plus `AV_CODEC_FLAG_LOW_DELAY`, which drives
FFmpeg's `output_delay` to 0 (amfenc.c:401).

Three details that the shipping implementations agree on:

- **Detect "no output yet" by `data == NULL`, not by a result code.** `amfenc.c` never names
  `AMF_REPEAT` or `AMF_NEED_MORE_INPUT` at all; it checks the out-pointer. AMD confirms
  `AMF_NEED_MORE_INPUT` is not a reliable "don't query" signal
  ([AMF#502](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/502)).
- **On `AMF_INPUT_FULL` the surface was NOT consumed.** Stash it and re-submit the *same* surface
  (amfenc.c:531-534). And `AMF_INPUT_FULL` means *the output is not being drained*
  ([AMF#274](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/274)) — so the correct response
  is to call `QueryOutput` and consume, then retry, exactly as FFmpeg's resubmit loop at
  amfenc.c:677-693 does. With strict one-in-flight it should never fire; log loudly if it does.
- **Do not sleep when `QUERY_TIMEOUT` is honoured.** FFmpeg's condition is verbatim
  `if (!ctx->query_timeout_supported || avpkt->data || avpkt->buf) av_usleep(1000);`
  (amfenc.c:664-666) under the comment *"Only sleep if the driver doesn't support waiting in
  QueryOutput()."* A `Sleep(1)` on Windows is really 1–15 ms depending on timer resolution (the
  process already raises it via `tuning.rs`), so this is not a rounding error.

Shutdown: `Drain()` once, then `QueryOutput` until `AMF_EOF`; tolerate `AMF_INPUT_FULL` from `Drain`
itself and retry it after consuming output (FFmpeg's `delayed_drain`, amfenc.c:617-627 and :650-659).
Reserve `Flush()` for re-init, never for shutdown
([AMF#472](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/472)).

Measure: instrument submit→first-byte-out per frame; the p99 must be ≤ p50 + one frame period.

### R5 — Teach `scaler.rs` to output NV12; keep one GPU pass *(delta: removes a CPU convert; cost: medium)*

`findings/major-amd-scaler-needs-nv12-output.md`

VCN wants NV12; WGC/DXGI give BGRA. The `ID3D11VideoProcessor` in `scaler.rs` can do scale **and**
BT.709 color-convert in one pass — it is already doing the scale. Changes: destination texture
`DXGI_FORMAT_NV12` instead of `DXGI_FORMAT_B8G8R8A8_UNORM` (`scaler.rs:76`), an NV12
`CreateVideoProcessorOutputView`, `VideoProcessorSetStreamColorSpace` /
`VideoProcessorSetOutputColorSpace` for full-range BT.709, and `VideoProcessorSetOutputTargetRect`.
NV12 requires even dimensions — `scaled_dims` already masks with `& !1` (`pipeline.rs:123-130`).

Do **not** use AMF's `AMFVideoConverter` component as a second GPU pass, and do not CPU-convert.
Note the fallback: `AMFCaps::GetInputCaps` → `GetFormatAt` tells you whether `AMF_SURFACE_BGRA` is in
the encoder's input list and whether it is `native`. If BGRA is accepted, submitting BGRA directly
and letting AMF convert internally (`CAP_COLOR_CONVERSION` says whether that conversion is
`AMF_ACCEL_GPU`) is a legitimate M-AMD2 shortcut that gets a picture on screen before the scaler
work lands. It is not the shipping path — an internal convert is a pass we do not control.

Measure: dump the NV12 and check with `ffplay -f rawvideo -pix_fmt nv12 -s WxH`; confirm no
`Map`/`memcpy` on the hot path with a debug counter; compare encode-time/frame against the BGRA
shortcut.

### R6 — One slice per frame until slice-mode output is proven *(delta: avoids a regression; cost: none)*

`findings/minor-amd-slices-per-frame-default-one.md`

`amd.md` §3 calls for `SLICES_PER_FRAME = 4..8` "so slices can be packetized as they complete." On
AVC that is only true if you **also** set `AMF_VIDEO_ENCODER_OUTPUT_MODE = SLICE` (`L"OutputMode"`,
`OUTPUT_MODE_ENUM`, default `FRAME`) — otherwise AMF still hands you one `AMFBuffer` per frame and
multiple slices buy nothing but a compression-efficiency loss (each slice restarts entropy coding and
breaks intra prediction across its boundary). The shipping NVENC path sets `sliceMode = 0`
(`nvidia/encoder.rs:599-600`), i.e. one slice, so copying `amd.md` here would make AMD *worse* than
NVIDIA on quality for no latency gain.

Default to `SlicesPerFrame = 1`, `OutputMode = FRAME`. Slice-mode output is a genuine
sub-frame-latency win and is worth a follow-up: it is real, it landed in **AMF 1.4.30 / Adrenalin
23.5.2**, each `QueryOutput` yields one slice, and AMD's own `EncoderLatency.cpp:594` uses
`OUTPUT_BUFFER_TYPE ∈ {SLICE_LAST, FRAME}` as the end-of-frame marker. But it is gated on
`AMF_VIDEO_ENCODER_CAP_SUPPORT_SLICE_OUTPUT` (`L"SupportSliceOutput"`), needs
`AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE` (`L"OutputBufferType"`: `FRAME` 0 / `SLICE` 1 /
`SLICE_LAST` 2) read on every buffer, and needs the RTP packetizer to emit partial access units — a
change to `webrtc_session.rs`'s hand-rolled H.264 packetization, not just the encoder. (Watch the
enumerant name: the AVC **`.md` doc wrongly calls it `_TILE`**, copy-pasted from AV1; the header says
`_SLICE`. AV1 really is `_TILE`.) File it as future work.

### R7 — Intra-refresh and LTR are follow-ups, not launch features *(delta: loss resilience; cost: medium)*

`findings/minor-amd-intra-refresh-and-ltr-followups.md`

**Intra-refresh: we get it for free and should not fight it.** `USAGE = ULTRA_LOW_LATENCY` already
sets `INTRA_REFRESH_NUM_MBS_PER_SLOT = 255` (§3.5) — a ~32-frame refresh wave at 1080p, ~0.53 s at
60 fps. So the right v1 behaviour is **leave it alone**, not tune it: do not set
`MAX_NUM_REFRAMES = 1` (which would disable it — the docs require `MaxOfReferenceFrames > 1` and
`NumOfLTR == 0`), and write `Int64(0)` only when `Config::intra_refresh` is explicitly off. If we
later want a specific wave length, `mbs_per_slot = ceil(ceil(w/16) * ceil(h/16) / (fps * seconds))`.
Note that **no shipping project sets this explicitly on AMD** — OBS reaches it only through
`ffmpeg_opts`, FFmpeg's `intra_refresh_mb` defaults to −1 (unset), and Sunshine never touches it —
so ULL's default is, in practice, the only configuration anyone has validated.

**LTR is mutually exclusive with intra-refresh, which changes the calculus.** The docs are explicit:
`MAX_LTR_FRAMES > 0` means *"With user control of LTR, B-pictures and Intra-refresh features are not
supported."* Range is **0–2**, further bounded by the H.264 level's DPB limit, and it must be set
before `Init()`. So this is a fork, not an addition: either the ULL default (rolling intra-refresh,
passive healing, no feedback needed) **or** user-controlled LTR (mark frames with
`MARK_CURRENT_WITH_LTR_INDEX` = `L"MarkCurrentWithLTRIndex"`; on a NACK/PLI force the next frame to
reference a known-received LTR via `FORCE_LTR_REFERENCE_BITFIELD` = `L"ForceLTRReferenceBitfield"`,
which *also disallows LTRs outside the bitfield from current and future reference*; track state from
`OUTPUT_MARKED_LTR_INDEX` and `OUTPUT_REFERENCED_LTR_INDEX_BITFIELD` on the output buffer).

LTR is strictly better *if* we act on RTCP feedback, because it repairs precisely and immediately
rather than over half a second. But it needs plumbing that does not exist yet — the WebRTC session
would have to tell the encoder which frames the client acknowledged, which is a change to
`webrtc_session.rs`, not just the encoder.

Ship neither knob in v1: take ULL's defaults, measure, and revisit. Note that PreAnalysis has a
separate "Auto LTR" (`AMF_PA_LTR_ENABLE`, AMF 1.4.23+) which is scene-based and **not** user
controlled — it is not the loss-recovery mechanism we want, and it drags in the whole PA lookahead.

### R8 — Detect AMD by DXGI `VendorId`, not description string *(delta: correctness; cost: trivial)*

`findings/minor-amd-detect-vendor-by-dxgi-vendorid.md`

Add `Vendor::from_pci_id(u32)` alongside `select_vendor` and prefer it:
`0x10DE` NVIDIA, `0x8086` Intel, `0x1002` AMD/ATI (`0x1022` is AMD's *CPU* vendor ID and never
appears on a display adapter). Keep the string match as a fallback for the one place we only have a
description. Also reject `DXGI_ADAPTER_FLAG_SOFTWARE`, which `create_nvidia_d3d11_device` already
does (`nvidia/encoder.rs:861-862`) and which a new AMD path must do too — WARP's description contains
none of these strings but its VendorId is Microsoft's `0x1414`.

### R9 — Route AMF's own logging into `logbus` *(delta: 0 latency, big diagnostics; cost: trivial)*

`findings/minor-amd-silence-amf-console-trace.md`

`AMFTrace`'s default configuration writes to the console. On Windows this app is GUI-subsystem in
release, so those writes are either lost or interleaved with `logbus`'s `attach_console` output. At
startup: `factory->GetTrace(&trace)` `[5]`, then
`trace->EnableWriter(AMF_TRACE_WRITER_CONSOLE, false)` `[4]` and
`trace->SetGlobalLevel(AMF_TRACE_ERROR)` `[2]`. Keep `GetResultText` `[18]` bound so every
`AMF_RESULT` we report carries AMD's own message, the way `nvidia/encoder.rs:517-538` uses
`nvEncGetLastErrorString`.

### R10 — Delete or fix the dead `EncoderBackend` trait *(delta: 0; cost: trivial)*

`findings/minor-amd-encoderbackend-trait-is-dead.md`

`streamer/platform.rs:169-177` defines a trait with one bail-only implementor and no callers, whose
`BackendConfig` (`width, height, fps, bitrate_bps`) is a strict subset of `EncoderConfig`
(`pipeline.rs:111-121`) and whose `encode(&mut self, force_idr)` models neither texture input nor
repeat nor prescale. Writing `impl EncoderBackend for AmfEncoder` would produce something the
pipeline still cannot call. Either delete the trait and `amd.rs`'s stub outright and add
`Backend::Amf` to the enum (recommended — it is what Nvenc/Intel/X264 do), or widen the trait to the
real surface and migrate all four backends. Do not leave a third pattern.

---

## 6. Verification plan

**Verifiable on the Windows dev box (11 26200, RTX 3080 Ti laptop — no AMD GPU):**

1. **The FFI, completely.** `amd_layout.rs` (R11 / §7) makes every struct size, field offset, vtable
   slot index and enum value a `cargo test` assertion checked against numbers generated from the real
   AMD headers by a C compiler. This is the same discipline as `test/nvenc_layout.rs` (183 KB of
   bindgen-derived assertions) and `test/intel_layout.rs`, and it catches the entire class of "wrong
   slot index = memory corruption" bug without hardware.
2. **Graceful absence.** With no `amfrt64.dll` on the box, `AmfEncoder::new` must return a clean
   `Err`, `build_backend`'s `Auto` chain must fall through to Intel/x264, and the log must say why.
   Add a unit test asserting the error path (the existing `test/pipeline.rs` is the model).
3. **Property-name table.** A test that every `L"…"` constant we hard-code matches the vendored
   header — grep the header for `#define <CONST>\s+L"(.*)"` and compare. Cheap, and it catches a
   typo'd wide string, which otherwise fails silently (`SetProperty` on an unknown name returns
   `AMF_NOT_FOUND` and is easy to ignore).
4. **`nm`-equivalent check.** `dumpbin /imports target\release\ScreenExtend.exe | findstr /i amf`
   must produce **nothing** — if `amfrt64` appears in the import table, something linked instead of
   `libloading`ing, and the binary will refuse to start on every non-AMD machine. This is the Windows
   analogue of the macOS `nm -u` rule in `CLAUDE.md`.
5. **Header-drift check.** Re-run the layout generator whenever the vendored headers are updated; a
   diff in the generated file is a required review item.

**Requires an AMD host (a Radeon desktop or a Ryzen APU laptop) — cannot be done here:**

6. `AMFInit` succeeds; `AMFQueryVersion` value; `CreateComponent(AMFVideoEncoderVCE_AVC)` succeeds;
   `GetAccelerationType() == AMF_ACCEL_HARDWARE`; the caps dump (`MaxBitrate`, `NumOfHwInstances`,
   `MaxThroughput`, `QueryTimeoutSupport`, `SupportSliceOutput`, the input format list and which
   formats are `native`).
7. Which of the probe-first properties actually exist: `LowLatencyInternal`, `OutputMode`,
   `InputQueueSize` writability, `EnableEncoderSmartAccessVideo`, `IntraRefreshMBsNumberPerSlot`.
8. Whether `IDRPeriod = 0` means "never" or "every frame".
9. Whether `QUERY_TIMEOUT` blocking actually beats sleep-polling, and by how much.
10. **Encode-path latency**: reuse the pipeline's per-60-frame avg/max encode-time log. Target: the
    NVENC band (single-digit ms at 1080p, low teens at 4K). Watch the **max** over a 5-minute run —
    a creeping tail is a VBV/buffering regression.
11. **Glass-to-glass**: the project's photographic method (millisecond stopwatch on the captured
    monitor, photographed next to the receiving browser), AMD vs NVENC on comparable hardware.
    Confirm hardware decode in `chrome://media-internals`.
12. **Stream conformance**: `ffprobe -show_frames out_amd.h264` → `has_b_frames=0`, `pict_type` only
    I/P, SPS/PPS present at each IDR, exactly one IDR per injected PLI.
13. **Cross-adapter**: on an Intel-iGPU + Radeon-dGPU laptop, force the shared-texture path and
    confirm 150 clean frames with a single GPU→GPU copy.
14. **Leak/repeat**: create and destroy the encoder 100× in a loop; process handle count and VRAM
    must be flat. (AMF's `Terminate`-then-`Release` ordering is the thing being tested.)

**Explicitly not verifiable anywhere in this project right now:** anything about VCN generations we
do not own (RDNA2 vs RDNA3 vs RDNA4 intra-refresh behaviour, AV1 encode), and SmartAccess Video,
which needs a specific iGPU+dGPU AMD pairing.

---

## 7. Struct-layout testing without an AMD GPU

**Toolchain on this box (checked):** `cl.exe` **19.44.35227 for x64** at
`C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\cl.exe`,
entered via `VC\Auxiliary\Build\vcvars64.bat`. `gcc 15.2.0` (MinGW-w64/UCRT) is also on `PATH` but is
the wrong choice — it does not model the MSVC C++ ABI, and `amfrt64.dll` is MSVC-built. No `clang`.

**The generator works and has been run.** `amf_layout_gen.c` includes the vendored headers **in C
mode** (`cl /TC`), so the `#else /* !__cplusplus */` C-API branch — the one we bind — is what gets
measured, and prints ready-to-paste Rust assertions:

```c
#define SZ(T)      printf("    assert_eq!(size_of::<%s>(), %zu, \"%s\");\n", #T, sizeof(T), #T)
#define OFF(T, f)  printf("    assert_eq!(offset_of!(%s, %s), %zu, \"%s.%s\");\n", #T, #f, offsetof(T, f), #T, #f)
#define SLOT(V, f) printf("    assert_eq!(SLOT_%s_%s, %zu, \"%s::%s vtable slot\");\n", #V, #f, \
                          offsetof(V, f) / sizeof(void *), #V, #f)
#define ENUMV(E)   printf("    assert_eq!(%s as i64, %lld, \"%s\");\n", #E, (long long)(E), #E)
```

`offsetof(AMFContextVtbl, InitDX11) / sizeof(void*)` **is** the vtable slot index — the C-API vtable
struct is exactly an array of function pointers, so byte offset ÷ 8 is the index we pass to
`(*(*ctx).vtbl).init_dx11`. Every number in §2.3 came out of this program.

Build and run:

```sh
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cl /nologo /TC /W3 /I <amf-include-root> amf_layout_gen.c /Fe:amf_layout_gen.exe
.\amf_layout_gen.exe > amd_layout_generated.rs
```

Excerpt of the real output (full run archived in the scratchpad):

```
assert_eq!(size_of::<AMFVariantStruct>(), 24, "AMFVariantStruct");
assert_eq!(align_of::<AMFVariantStruct>(), 8, "AMFVariantStruct");
assert_eq!(offset_of!(AMFVariantStruct, int64Value), 8, "AMFVariantStruct.<union>");
assert_eq!(SLOT_AMFContextVtbl_InitDX11, 18, "AMFContextVtbl::InitDX11 vtable slot");
assert_eq!(SLOT_AMFContextVtbl_CreateSurfaceFromDX11Native, 49, ...);
assert_eq!(size_of::<AMFContextVtbl>() / size_of::<*const c_void>(), 55, "AMFContextVtbl slots");
assert_eq!(SLOT_AMFComponentVtbl_SubmitInput, 22, ...);
assert_eq!(SLOT_AMFComponentVtbl_QueryOutput, 23, ...);
assert_eq!(SLOT_AMFBufferVtbl_GetNative, 25, ...);
assert_eq!(AMF_INPUT_FULL as i64, 25, "AMF_INPUT_FULL");
assert_eq!(AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY as i64, 1, ...);
// AMF_FULL_VERSION built against: 281496451678208 (1.5.2.0)
```

The **total slot count per vtable is the most valuable assertion of the set** — it catches a member
we forgot entirely, which offset checks on the members we *did* declare cannot:

| Vtable | Slots |
|---|---|
| `AMFFactoryVtbl` | 7 |
| `AMFInterfaceVtbl` | 3 |
| `AMFPropertyStorageVtbl` | 13 |
| `AMFPropertyStorageExVtbl` | 17 |
| `AMFDataVtbl` | 23 |
| `AMFSurfaceVtbl` | 33 |
| `AMFBufferVtbl` | 28 |
| `AMFPlaneVtbl` | 13 |
| `AMFComponentVtbl` | 28 |
| `AMFContextVtbl` | 55 |
| `AMFContext1Vtbl` | 65 |
| `AMFCapsVtbl` | 16 |
| `AMFIOCapsVtbl` | 11 |
| `AMFTraceVtbl` | 25 |

**Run the generator in C++ mode too, and diff.** A second tiny program compiled with `/TP` proved the
`AMFPropertyInfo` divergence in §2.4 (112 bytes in C, **120 in C++ because of the virtual
destructor**) — and `amfrt64.dll` is the C++ side. Any struct whose two builds disagree is a struct
we must model from the **C++** layout, not the C branch we otherwise bind. Measured C++ results:

```
C++ sizeof(amf::AMFPropertyInfo)      = 120     (C: 112)   <-- DIVERGES
C++ offsetof(AMFPropertyInfo, name)   = 8       (C: 0)     <-- DIVERGES
C++ sizeof(AMFVariantStruct)          = 24      (C: 24)    ok
C++ sizeof(amf::AMFVariant)           = 24                 ok (no virtuals)
C++ sizeof(AMFGuid)                   = 16      (C: 16)    ok
C++ sizeof(AMFSize)/(AMFRate)         = 8 / 8              ok
```

**Where this lives.** `findings/major-amd-amf-layout-test-harness.md`. Mirror the existing
convention: the generator + vendored headers under `src-tauri/src/windows_utils/streamer/amd/`
(alongside `amf_sys/`), the generated assertions as
`src-tauri/src/windows_utils/streamer/test/amd_layout.rs`, registered in
`windows_utils/streamer/test/mod.rs` next to `nvenc_layout` and `intel_layout`. The generator itself
is a build-time-only artifact — check it in, do not run it from `build.rs` (the dev box for this repo
is often a Mac without `cl.exe`; the generated `.rs` is the checked-in product).

---

## 8. Open questions

Resolved during the research and recorded here so they are not re-asked: `IDRPeriod` should be
`INT_MAX`, not 0 (Sunshine + FFmpeg, §3.3); `QUERY_TIMEOUT` is static and wants 1 ms
([AMF#516](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/516), §2.7); AMD's own model is
two threads but one works ([AMF#502](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/502));
the AMF→driver mapping is published (§2.1). What remains:

1. **Is `AMF_VIDEO_ENCODER_INPUT_QUEUE_SIZE` writable, and static or dynamic?** It appears in the
   header's "Dynamic properties" block but a queue depth is the kind of thing allocated at `Init`,
   and AMD's docs list it as dynamic while `QUERY_TIMEOUT` — also listed dynamic — turned out to be
   static. If `1` is honoured, `AMF_INPUT_FULL` becomes a meaningful backpressure signal instead of a
   16-frame cushion. Sunshine sidesteps the question by capping FFmpeg's own queue
   (`async_depth = 1`); we may have to do the same and simply never submit ahead.
2. **Does `LOWLATENCY_MODE` ("POC mode 2") change the bitstream in a way a browser's WebCodecs H.264
   decoder cares about?** POC type 2 constrains picture-order-count signalling for no-reordering
   streams. It should be strictly *easier* to decode, but it is a bitstream-level change and worth
   checking on Safari/WebKit as well as Chrome — our client page must work on old mobile browsers.
3. **How do we get correct colour signalling out of AMF at all?** `OUTPUT_COLOR_PROFILE`,
   `OUTPUT_TRANSFER_CHARACTERISTIC` and `OUTPUT_COLOR_PRIMARIES` are reported never to reach the
   bitstream ([AMF#398](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/issues/398), still open on
   driver 24.8.1); only `FULL_RANGE_COLOR` works. The NVENC path writes a full VUI
   (`nvidia/encoder.rs:632-640`), so AMD and NVIDIA hosts will produce *differently signalled*
   streams from identical input. Does our client actually depend on the VUI, or does it assume
   BT.709? If it depends on it, we may need to patch the SPS ourselves — which we are already
   positioned to do, since we packetize H.264 by hand.
   Filed as `findings/minor-amd-amf-does-not-write-vui-colour-signalling.md`.
4. **Does AMF really copy input-surface properties onto the output buffer?** FFmpeg's entire
   frame-lifetime mechanism depends on it (`av_frame_ref<N>` set on the surface, read off the
   buffer, amfenc.c:368-378 / :572-573), and OBS depends on the same behaviour for its `L"PTS"`
   property. It is nowhere documented. If it holds, it is a cheaper alternative to an
   `AMFSurfaceObserver` for us too; if it is version-dependent, an observer is the safe choice.
5. **Slice-mode output** (`OUTPUT_MODE = SLICE` + `OUTPUT_BUFFER_TYPE`, AMF 1.4.30+) is the only
   genuinely sub-frame latency mechanism AMF offers, and it needs changes in `webrtc_session.rs`'s
   hand-rolled H.264 packetizer, not just the encoder. Worth its own research pass once the base
   backend lands — and worth checking whether NVENC (`sliceMode` + `NV_ENC_CONFIG_H264::sliceMode` +
   the async output-slice callback) and VideoToolbox can do the same thing, because a slice-level
   pipeline that exists on only one vendor is not worth the packetizer complexity.
6. **How much does the ULL default intra-refresh (255 MBs/slot) actually cost in bitrate?** It is on
   by default and we recommend leaving it on, but a permanent rolling refresh raises the per-frame
   bit cost against a fixed CBR budget. If it turns out to cost 10–15%, gating it on
   `Config::intra_refresh` (currently defaulted off elsewhere, `pipeline.rs:120`) is the better
   default.
7. **Does the `AMFSize`-vs-`AMFRate` signedness distinction bite anywhere?** Both are 8 bytes of two
   32-bit ints; the variant type tag is the only discriminator, and a Rust binding that reuses one
   struct for both will compile and silently pass the wrong `AMF_VARIANT_TYPE`. The layout test
   catches sizes, not semantics — this one needs a careful review of every `SetProperty` call site.

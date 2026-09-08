//! AMF C-ABI layout assertions — generated, do not hand-edit the `abi_matches_*` bodies.
//!
//! `amd_sys` binds AMF through COM-like C vtables. A wrong slot index is not a compile error and
//! not a runtime error message; it is a call through the wrong function pointer, which is memory
//! corruption. `AMFContext::CreateSurfaceFromDX11Native` is slot **49 of 55** and the
//! OpenCL/OpenGL/XV/Gralloc blocks in between make hand-counting to it an obvious way to be off
//! by one — so the numbers below were measured by a C compiler, not read off the header by eye.
//!
//! No machine on this project has an AMD GPU. These assertions are what makes the FFI verifiable
//! anyway: everything except *behaviour* is checked here, and only behaviour needs a Radeon.
//!
//! ## Provenance
//!
//! * Headers: `src-tauri/resources/amf-headers/{core,components}` — GPUOpen-LibrariesAndSDKs/AMF
//!   at AMF 1.5.2.0, MIT. SHA-256 of every file is pinned in `src-tauri/resources/SHA256SUMS`
//!   and recorded in `resources/PROVENANCE.md`.
//! * Generator: `src-tauri/tools/amf_layout_gen.c`, compiled **in C mode** (`cl /TC`) so the
//!   `#else /* !__cplusplus */` branch — the one `amd_sys` binds — is what gets measured.
//!   `offsetof(SomeVtbl, member) / sizeof(void *)` **is** the vtable slot index, because a C-API
//!   vtable struct is exactly an array of function pointers.
//! * Toolchain: `cl.exe 19.44.35227 for x64` (MSVC Build Tools 2022, `VC\Tools\MSVC\14.44.35207`),
//!   entered via `VC\Auxiliary\Build\vcvars64.bat`. MinGW `gcc` is the wrong tool here: it does
//!   not model the MSVC C++ ABI and `amfrt64.dll` is MSVC-built.
//!
//! ## Regenerating
//!
//! ```text
//! cd src-tauri\tools
//! amf_layout_build.bat          :: vcvars64 + cl /TC + cl /TP, writes amf_layout_generated.txt
//! ```
//!
//! then paste `amf_layout_generated.txt` over the bodies below, in order. Do **not** run the
//! generator from `build.rs`: the usual dev box for this repo is a macOS 10.15 machine with no
//! `cl.exe`, and the generated Rust is the checked-in product. A diff in these numbers after a
//! header bump is a required review item.
//!
//! `amf_layout_build.bat` also compiles `amf_layout_cpp_check.cpp` with `/TP` and dumps the C++
//! layouts. Any struct whose C and C++ builds disagree must be modelled from the **C++** side,
//! because `amfrt64.dll` is compiled from the C++ headers. Today exactly one struct diverges —
//! `AMFPropertyInfo`, 112 bytes with `name` at 0 in C and 120 bytes with `name` at 8 in C++
//! (it has a virtual destructor). `amd_sys` sidesteps it entirely by never calling
//! `GetPropertyInfo` and probing optional properties with `HasProperty` instead, which is why
//! there is no `AMFPropertyInfo` in the Rust bindings and nothing to assert about it.

use std::mem::{align_of, offset_of, size_of};

use crate::windows_utils::streamer::amd_sys::props::names as props;
use crate::windows_utils::streamer::amd_sys::props::{codec, trace_writer};
use crate::windows_utils::streamer::amd_sys::*;

/// Size of a function-pointer slot; the divisor that turns a byte offset into a slot index.
const PTR: usize = size_of::<*const std::ffi::c_void>();

/// `offsetof(Vtbl, member) / sizeof(void *)` — the vtable slot index of a member.
macro_rules! slot {
    ($vtbl:ty, $member:ident) => {
        offset_of!($vtbl, $member) / PTR
    };
}

/// Compares one of our NUL-terminated wide-string constants with the header's own value.
#[track_caller]
fn assert_wide(actual: &[u16], expected: &str, name: &str) {
    assert_eq!(
        actual.last(),
        Some(&0u16),
        "{name} must be NUL-terminated for the C API"
    );
    let s = String::from_utf16_lossy(&actual[..actual.len() - 1]);
    assert_eq!(s, expected, "{name} wide-string value");
}

/// Struct sizes, alignment and field offsets of every plain AMF type `amd_sys` models.
///
/// `AMFVariantStruct` is the one that has to be exactly right on the hot path: 24 bytes
/// with the union at offset 8, passed to `SetProperty` **by value** (by hidden reference
/// on x64 Windows). Four bytes of padding after the 4-byte tag are load-bearing.
#[test]
fn abi_struct_layout_matches_amf_headers() {
    // ---- sizes / alignment ----
    assert_eq!(size_of::<AMFGuid>(), 16, "AMFGuid");
    assert_eq!(align_of::<AMFGuid>(), 4, "AMFGuid");
    assert_eq!(size_of::<AMFVariantStruct>(), 24, "AMFVariantStruct");
    assert_eq!(align_of::<AMFVariantStruct>(), 8, "AMFVariantStruct");
    assert_eq!(size_of::<AMFSize>(), 8, "AMFSize");
    assert_eq!(size_of::<AMFRate>(), 8, "AMFRate");
    assert_eq!(size_of::<AMFRatio>(), 8, "AMFRatio");
    assert_eq!(size_of::<AMFRect>(), 16, "AMFRect");
    assert_eq!(size_of::<AMFPoint>(), 8, "AMFPoint");
    assert_eq!(size_of::<AMFColor>(), 4, "AMFColor");
    assert_eq!(PTR, 8, "pointer");

    // ---- field offsets ----
    assert_eq!(
        offset_of!(AMFVariantStruct, r#type),
        0,
        "AMFVariantStruct.type"
    );
    assert_eq!(
        offset_of!(AMFVariantStruct, value),
        8,
        "AMFVariantStruct.<union>"
    );
    assert_eq!(offset_of!(AMFGuid, data1), 0, "AMFGuid.data1");
    assert_eq!(offset_of!(AMFGuid, data2), 4, "AMFGuid.data2");
    assert_eq!(offset_of!(AMFGuid, data3), 6, "AMFGuid.data3");
    assert_eq!(offset_of!(AMFGuid, data41), 8, "AMFGuid.data41");
    assert_eq!(offset_of!(AMFGuid, data48), 15, "AMFGuid.data48");
    assert_eq!(offset_of!(AMFSize, width), 0, "AMFSize.width");
    assert_eq!(offset_of!(AMFSize, height), 4, "AMFSize.height");
    assert_eq!(offset_of!(AMFRate, num), 0, "AMFRate.num");
    assert_eq!(offset_of!(AMFRate, den), 4, "AMFRate.den");
    // AMFPropertyInfo: C sizeof=112 name@0 — not modelled in Rust, see amf_layout_cpp_check.cpp
}

/// Vtable slot indices, plus the **total slot count** of every vtable.
///
/// The totals are the assertion that matters most: an offset check can only catch a
/// member declared in the wrong place, while a member omitted entirely shifts every slot
/// after it and is invisible to everything except the size.
#[test]
fn abi_vtable_slots_match_amf_headers() {
    // ---- vtable slot indices (THE ABI: a wrong index is memory corruption) ----
    assert_eq!(
        slot!(AMFFactoryVtbl, CreateContext),
        0,
        "AMFFactoryVtbl::CreateContext"
    );
    assert_eq!(
        slot!(AMFFactoryVtbl, CreateComponent),
        1,
        "AMFFactoryVtbl::CreateComponent"
    );
    assert_eq!(
        slot!(AMFFactoryVtbl, SetCacheFolder),
        2,
        "AMFFactoryVtbl::SetCacheFolder"
    );
    assert_eq!(
        slot!(AMFFactoryVtbl, GetCacheFolder),
        3,
        "AMFFactoryVtbl::GetCacheFolder"
    );
    assert_eq!(
        slot!(AMFFactoryVtbl, GetDebug),
        4,
        "AMFFactoryVtbl::GetDebug"
    );
    assert_eq!(
        slot!(AMFFactoryVtbl, GetTrace),
        5,
        "AMFFactoryVtbl::GetTrace"
    );
    assert_eq!(
        slot!(AMFFactoryVtbl, GetPrograms),
        6,
        "AMFFactoryVtbl::GetPrograms"
    );
    assert_eq!(
        size_of::<AMFFactoryVtbl>() / PTR,
        7,
        "AMFFactoryVtbl total slots"
    );

    assert_eq!(
        slot!(AMFInterfaceVtbl, Acquire),
        0,
        "AMFInterfaceVtbl::Acquire"
    );
    assert_eq!(
        slot!(AMFInterfaceVtbl, Release),
        1,
        "AMFInterfaceVtbl::Release"
    );
    assert_eq!(
        slot!(AMFInterfaceVtbl, QueryInterface),
        2,
        "AMFInterfaceVtbl::QueryInterface"
    );
    assert_eq!(
        size_of::<AMFInterfaceVtbl>() / PTR,
        3,
        "AMFInterfaceVtbl total slots"
    );

    assert_eq!(
        slot!(AMFPropertyStorageVtbl, Acquire),
        0,
        "AMFPropertyStorageVtbl::Acquire"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, Release),
        1,
        "AMFPropertyStorageVtbl::Release"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, QueryInterface),
        2,
        "AMFPropertyStorageVtbl::QueryInterface"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, SetProperty),
        3,
        "AMFPropertyStorageVtbl::SetProperty"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, GetProperty),
        4,
        "AMFPropertyStorageVtbl::GetProperty"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, HasProperty),
        5,
        "AMFPropertyStorageVtbl::HasProperty"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, GetPropertyCount),
        6,
        "AMFPropertyStorageVtbl::GetPropertyCount"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, GetPropertyAt),
        7,
        "AMFPropertyStorageVtbl::GetPropertyAt"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, Clear),
        8,
        "AMFPropertyStorageVtbl::Clear"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, AddTo),
        9,
        "AMFPropertyStorageVtbl::AddTo"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, CopyTo),
        10,
        "AMFPropertyStorageVtbl::CopyTo"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, AddObserver),
        11,
        "AMFPropertyStorageVtbl::AddObserver"
    );
    assert_eq!(
        slot!(AMFPropertyStorageVtbl, RemoveObserver),
        12,
        "AMFPropertyStorageVtbl::RemoveObserver"
    );
    assert_eq!(
        size_of::<AMFPropertyStorageVtbl>() / PTR,
        13,
        "AMFPropertyStorageVtbl total slots"
    );

    assert_eq!(
        slot!(AMFPropertyStorageExVtbl, GetPropertiesInfoCount),
        13,
        "AMFPropertyStorageExVtbl::GetPropertiesInfoCount"
    );
    assert_eq!(
        slot!(AMFPropertyStorageExVtbl, GetPropertyInfoAt),
        14,
        "AMFPropertyStorageExVtbl::GetPropertyInfoAt"
    );
    assert_eq!(
        slot!(AMFPropertyStorageExVtbl, GetPropertyInfo),
        15,
        "AMFPropertyStorageExVtbl::GetPropertyInfo"
    );
    assert_eq!(
        slot!(AMFPropertyStorageExVtbl, ValidateProperty),
        16,
        "AMFPropertyStorageExVtbl::ValidateProperty"
    );
    assert_eq!(
        size_of::<AMFPropertyStorageExVtbl>() / PTR,
        17,
        "AMFPropertyStorageExVtbl total slots"
    );

    assert_eq!(
        slot!(AMFContextVtbl, Terminate),
        13,
        "AMFContextVtbl::Terminate"
    );
    assert_eq!(
        slot!(AMFContextVtbl, InitDX9),
        14,
        "AMFContextVtbl::InitDX9"
    );
    assert_eq!(
        slot!(AMFContextVtbl, InitDX11),
        18,
        "AMFContextVtbl::InitDX11"
    );
    assert_eq!(
        slot!(AMFContextVtbl, GetDX11Device),
        19,
        "AMFContextVtbl::GetDX11Device"
    );
    assert_eq!(
        slot!(AMFContextVtbl, LockDX11),
        20,
        "AMFContextVtbl::LockDX11"
    );
    assert_eq!(
        slot!(AMFContextVtbl, UnlockDX11),
        21,
        "AMFContextVtbl::UnlockDX11"
    );
    assert_eq!(
        slot!(AMFContextVtbl, InitOpenCL),
        22,
        "AMFContextVtbl::InitOpenCL"
    );
    assert_eq!(
        slot!(AMFContextVtbl, InitOpenGL),
        30,
        "AMFContextVtbl::InitOpenGL"
    );
    assert_eq!(
        slot!(AMFContextVtbl, AllocBuffer),
        43,
        "AMFContextVtbl::AllocBuffer"
    );
    assert_eq!(
        slot!(AMFContextVtbl, AllocSurface),
        44,
        "AMFContextVtbl::AllocSurface"
    );
    assert_eq!(
        slot!(AMFContextVtbl, CreateBufferFromHostNative),
        46,
        "AMFContextVtbl::CreateBufferFromHostNative"
    );
    assert_eq!(
        slot!(AMFContextVtbl, CreateSurfaceFromHostNative),
        47,
        "AMFContextVtbl::CreateSurfaceFromHostNative"
    );
    assert_eq!(
        slot!(AMFContextVtbl, CreateSurfaceFromDX9Native),
        48,
        "AMFContextVtbl::CreateSurfaceFromDX9Native"
    );
    assert_eq!(
        slot!(AMFContextVtbl, CreateSurfaceFromDX11Native),
        49,
        "AMFContextVtbl::CreateSurfaceFromDX11Native"
    );
    assert_eq!(
        slot!(AMFContextVtbl, GetCompute),
        54,
        "AMFContextVtbl::GetCompute"
    );
    assert_eq!(
        size_of::<AMFContextVtbl>() / PTR,
        55,
        "AMFContextVtbl total slots"
    );

    assert_eq!(
        slot!(AMFContext1Vtbl, InitDX11),
        18,
        "AMFContext1Vtbl::InitDX11"
    );
    assert_eq!(
        slot!(AMFContext1Vtbl, CreateSurfaceFromDX11Native),
        49,
        "AMFContext1Vtbl::CreateSurfaceFromDX11Native"
    );
    assert_eq!(
        slot!(AMFContext1Vtbl, CreateBufferFromDX11Native),
        55,
        "AMFContext1Vtbl::CreateBufferFromDX11Native"
    );
    assert_eq!(
        slot!(AMFContext1Vtbl, AllocSurfaceEx),
        57,
        "AMFContext1Vtbl::AllocSurfaceEx"
    );
    assert_eq!(
        slot!(AMFContext1Vtbl, InitVulkan),
        58,
        "AMFContext1Vtbl::InitVulkan"
    );
    assert_eq!(
        size_of::<AMFContext1Vtbl>() / PTR,
        65,
        "AMFContext1Vtbl total slots"
    );

    assert_eq!(
        slot!(AMFComponentVtbl, SetProperty),
        3,
        "AMFComponentVtbl::SetProperty"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, GetProperty),
        4,
        "AMFComponentVtbl::GetProperty"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, HasProperty),
        5,
        "AMFComponentVtbl::HasProperty"
    );
    assert_eq!(slot!(AMFComponentVtbl, Init), 17, "AMFComponentVtbl::Init");
    assert_eq!(
        slot!(AMFComponentVtbl, ReInit),
        18,
        "AMFComponentVtbl::ReInit"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, Terminate),
        19,
        "AMFComponentVtbl::Terminate"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, Drain),
        20,
        "AMFComponentVtbl::Drain"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, Flush),
        21,
        "AMFComponentVtbl::Flush"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, SubmitInput),
        22,
        "AMFComponentVtbl::SubmitInput"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, QueryOutput),
        23,
        "AMFComponentVtbl::QueryOutput"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, GetContext),
        24,
        "AMFComponentVtbl::GetContext"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, SetOutputDataAllocatorCB),
        25,
        "AMFComponentVtbl::SetOutputDataAllocatorCB"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, GetCaps),
        26,
        "AMFComponentVtbl::GetCaps"
    );
    assert_eq!(
        slot!(AMFComponentVtbl, Optimize),
        27,
        "AMFComponentVtbl::Optimize"
    );
    assert_eq!(
        size_of::<AMFComponentVtbl>() / PTR,
        28,
        "AMFComponentVtbl total slots"
    );

    assert_eq!(
        slot!(AMFDataVtbl, GetMemoryType),
        13,
        "AMFDataVtbl::GetMemoryType"
    );
    assert_eq!(slot!(AMFDataVtbl, Duplicate), 14, "AMFDataVtbl::Duplicate");
    assert_eq!(slot!(AMFDataVtbl, Convert), 15, "AMFDataVtbl::Convert");
    assert_eq!(slot!(AMFDataVtbl, Interop), 16, "AMFDataVtbl::Interop");
    assert_eq!(
        slot!(AMFDataVtbl, GetDataType),
        17,
        "AMFDataVtbl::GetDataType"
    );
    assert_eq!(
        slot!(AMFDataVtbl, IsReusable),
        18,
        "AMFDataVtbl::IsReusable"
    );
    assert_eq!(slot!(AMFDataVtbl, SetPts), 19, "AMFDataVtbl::SetPts");
    assert_eq!(slot!(AMFDataVtbl, GetPts), 20, "AMFDataVtbl::GetPts");
    assert_eq!(
        slot!(AMFDataVtbl, SetDuration),
        21,
        "AMFDataVtbl::SetDuration"
    );
    assert_eq!(
        slot!(AMFDataVtbl, GetDuration),
        22,
        "AMFDataVtbl::GetDuration"
    );
    assert_eq!(
        size_of::<AMFDataVtbl>() / PTR,
        23,
        "AMFDataVtbl total slots"
    );

    assert_eq!(
        slot!(AMFSurfaceVtbl, GetFormat),
        23,
        "AMFSurfaceVtbl::GetFormat"
    );
    assert_eq!(
        slot!(AMFSurfaceVtbl, GetPlanesCount),
        24,
        "AMFSurfaceVtbl::GetPlanesCount"
    );
    assert_eq!(
        slot!(AMFSurfaceVtbl, GetPlaneAt),
        25,
        "AMFSurfaceVtbl::GetPlaneAt"
    );
    assert_eq!(
        slot!(AMFSurfaceVtbl, GetPlane),
        26,
        "AMFSurfaceVtbl::GetPlane"
    );
    assert_eq!(
        slot!(AMFSurfaceVtbl, GetFrameType),
        27,
        "AMFSurfaceVtbl::GetFrameType"
    );
    assert_eq!(
        slot!(AMFSurfaceVtbl, SetFrameType),
        28,
        "AMFSurfaceVtbl::SetFrameType"
    );
    assert_eq!(
        slot!(AMFSurfaceVtbl, SetCrop),
        29,
        "AMFSurfaceVtbl::SetCrop"
    );
    assert_eq!(
        slot!(AMFSurfaceVtbl, CopySurfaceRegion),
        30,
        "AMFSurfaceVtbl::CopySurfaceRegion"
    );
    assert_eq!(
        slot!(AMFSurfaceVtbl, AddObserver_Surface),
        31,
        "AMFSurfaceVtbl::AddObserver_Surface"
    );
    assert_eq!(
        slot!(AMFSurfaceVtbl, RemoveObserver_Surface),
        32,
        "AMFSurfaceVtbl::RemoveObserver_Surface"
    );
    assert_eq!(
        size_of::<AMFSurfaceVtbl>() / PTR,
        33,
        "AMFSurfaceVtbl total slots"
    );

    assert_eq!(slot!(AMFBufferVtbl, SetSize), 23, "AMFBufferVtbl::SetSize");
    assert_eq!(slot!(AMFBufferVtbl, GetSize), 24, "AMFBufferVtbl::GetSize");
    assert_eq!(
        slot!(AMFBufferVtbl, GetNative),
        25,
        "AMFBufferVtbl::GetNative"
    );
    assert_eq!(
        slot!(AMFBufferVtbl, AddObserver_Buffer),
        26,
        "AMFBufferVtbl::AddObserver_Buffer"
    );
    assert_eq!(
        slot!(AMFBufferVtbl, RemoveObserver_Buffer),
        27,
        "AMFBufferVtbl::RemoveObserver_Buffer"
    );
    assert_eq!(
        size_of::<AMFBufferVtbl>() / PTR,
        28,
        "AMFBufferVtbl total slots"
    );

    assert_eq!(slot!(AMFPlaneVtbl, GetType), 3, "AMFPlaneVtbl::GetType");
    assert_eq!(slot!(AMFPlaneVtbl, GetNative), 4, "AMFPlaneVtbl::GetNative");
    assert_eq!(
        slot!(AMFPlaneVtbl, GetPixelSizeInBytes),
        5,
        "AMFPlaneVtbl::GetPixelSizeInBytes"
    );
    assert_eq!(
        slot!(AMFPlaneVtbl, GetOffsetX),
        6,
        "AMFPlaneVtbl::GetOffsetX"
    );
    assert_eq!(
        slot!(AMFPlaneVtbl, GetOffsetY),
        7,
        "AMFPlaneVtbl::GetOffsetY"
    );
    assert_eq!(slot!(AMFPlaneVtbl, GetWidth), 8, "AMFPlaneVtbl::GetWidth");
    assert_eq!(slot!(AMFPlaneVtbl, GetHeight), 9, "AMFPlaneVtbl::GetHeight");
    assert_eq!(
        slot!(AMFPlaneVtbl, GetHPitch),
        10,
        "AMFPlaneVtbl::GetHPitch"
    );
    assert_eq!(
        slot!(AMFPlaneVtbl, GetVPitch),
        11,
        "AMFPlaneVtbl::GetVPitch"
    );
    assert_eq!(slot!(AMFPlaneVtbl, IsTiled), 12, "AMFPlaneVtbl::IsTiled");
    assert_eq!(
        size_of::<AMFPlaneVtbl>() / PTR,
        13,
        "AMFPlaneVtbl total slots"
    );

    assert_eq!(
        slot!(AMFCapsVtbl, GetAccelerationType),
        13,
        "AMFCapsVtbl::GetAccelerationType"
    );
    assert_eq!(
        slot!(AMFCapsVtbl, GetInputCaps),
        14,
        "AMFCapsVtbl::GetInputCaps"
    );
    assert_eq!(
        slot!(AMFCapsVtbl, GetOutputCaps),
        15,
        "AMFCapsVtbl::GetOutputCaps"
    );
    assert_eq!(
        size_of::<AMFCapsVtbl>() / PTR,
        16,
        "AMFCapsVtbl total slots"
    );

    assert_eq!(
        slot!(AMFIOCapsVtbl, GetWidthRange),
        3,
        "AMFIOCapsVtbl::GetWidthRange"
    );
    assert_eq!(
        slot!(AMFIOCapsVtbl, GetHeightRange),
        4,
        "AMFIOCapsVtbl::GetHeightRange"
    );
    assert_eq!(
        slot!(AMFIOCapsVtbl, GetVertAlign),
        5,
        "AMFIOCapsVtbl::GetVertAlign"
    );
    assert_eq!(
        slot!(AMFIOCapsVtbl, GetNumOfFormats),
        6,
        "AMFIOCapsVtbl::GetNumOfFormats"
    );
    assert_eq!(
        slot!(AMFIOCapsVtbl, GetFormatAt),
        7,
        "AMFIOCapsVtbl::GetFormatAt"
    );
    assert_eq!(
        slot!(AMFIOCapsVtbl, GetNumOfMemoryTypes),
        8,
        "AMFIOCapsVtbl::GetNumOfMemoryTypes"
    );
    assert_eq!(
        slot!(AMFIOCapsVtbl, GetMemoryTypeAt),
        9,
        "AMFIOCapsVtbl::GetMemoryTypeAt"
    );
    assert_eq!(
        slot!(AMFIOCapsVtbl, IsInterlacedSupported),
        10,
        "AMFIOCapsVtbl::IsInterlacedSupported"
    );
    assert_eq!(
        size_of::<AMFIOCapsVtbl>() / PTR,
        11,
        "AMFIOCapsVtbl total slots"
    );

    assert_eq!(
        slot!(AMFTraceVtbl, SetGlobalLevel),
        2,
        "AMFTraceVtbl::SetGlobalLevel"
    );
    assert_eq!(
        slot!(AMFTraceVtbl, GetGlobalLevel),
        3,
        "AMFTraceVtbl::GetGlobalLevel"
    );
    assert_eq!(
        slot!(AMFTraceVtbl, EnableWriter),
        4,
        "AMFTraceVtbl::EnableWriter"
    );
    assert_eq!(
        slot!(AMFTraceVtbl, SetWriterLevel),
        10,
        "AMFTraceVtbl::SetWriterLevel"
    );
    assert_eq!(
        slot!(AMFTraceVtbl, GetResultText),
        18,
        "AMFTraceVtbl::GetResultText"
    );
    assert_eq!(
        size_of::<AMFTraceVtbl>() / PTR,
        25,
        "AMFTraceVtbl total slots"
    );
}

/// Enum values. `AMF_RESULT` is a plain sequential C enum with no explicit values, so
/// every number here is positional and must be regenerated if AMD inserts a member.
#[test]
fn abi_enum_values_match_amf_headers() {
    // ---- enum values ----
    assert_eq!(AMF_OK as i64, 0, "AMF_OK");
    assert_eq!(AMF_FAIL as i64, 1, "AMF_FAIL");
    assert_eq!(AMF_UNEXPECTED as i64, 2, "AMF_UNEXPECTED");
    assert_eq!(AMF_ACCESS_DENIED as i64, 3, "AMF_ACCESS_DENIED");
    assert_eq!(AMF_INVALID_ARG as i64, 4, "AMF_INVALID_ARG");
    assert_eq!(AMF_OUT_OF_MEMORY as i64, 6, "AMF_OUT_OF_MEMORY");
    assert_eq!(AMF_INVALID_POINTER as i64, 7, "AMF_INVALID_POINTER");
    assert_eq!(AMF_NO_INTERFACE as i64, 8, "AMF_NO_INTERFACE");
    assert_eq!(AMF_NOT_IMPLEMENTED as i64, 9, "AMF_NOT_IMPLEMENTED");
    assert_eq!(AMF_NOT_SUPPORTED as i64, 10, "AMF_NOT_SUPPORTED");
    assert_eq!(AMF_NOT_FOUND as i64, 11, "AMF_NOT_FOUND");
    assert_eq!(
        AMF_ALREADY_INITIALIZED as i64, 12,
        "AMF_ALREADY_INITIALIZED"
    );
    assert_eq!(AMF_NOT_INITIALIZED as i64, 13, "AMF_NOT_INITIALIZED");
    assert_eq!(AMF_INVALID_FORMAT as i64, 14, "AMF_INVALID_FORMAT");
    assert_eq!(AMF_WRONG_STATE as i64, 15, "AMF_WRONG_STATE");
    assert_eq!(AMF_NO_DEVICE as i64, 17, "AMF_NO_DEVICE");
    assert_eq!(AMF_DIRECTX_FAILED as i64, 18, "AMF_DIRECTX_FAILED");
    assert_eq!(AMF_EOF as i64, 23, "AMF_EOF");
    assert_eq!(AMF_REPEAT as i64, 24, "AMF_REPEAT");
    assert_eq!(AMF_INPUT_FULL as i64, 25, "AMF_INPUT_FULL");
    assert_eq!(AMF_RESOLUTION_CHANGED as i64, 26, "AMF_RESOLUTION_CHANGED");
    assert_eq!(AMF_RESOLUTION_UPDATED as i64, 27, "AMF_RESOLUTION_UPDATED");
    assert_eq!(AMF_INVALID_DATA_TYPE as i64, 28, "AMF_INVALID_DATA_TYPE");
    assert_eq!(AMF_INVALID_RESOLUTION as i64, 29, "AMF_INVALID_RESOLUTION");
    assert_eq!(
        AMF_CODEC_NOT_SUPPORTED as i64, 30,
        "AMF_CODEC_NOT_SUPPORTED"
    );
    assert_eq!(
        AMF_SURFACE_FORMAT_NOT_SUPPORTED as i64, 31,
        "AMF_SURFACE_FORMAT_NOT_SUPPORTED"
    );
    assert_eq!(
        AMF_SURFACE_MUST_BE_SHARED as i64, 32,
        "AMF_SURFACE_MUST_BE_SHARED"
    );
    assert_eq!(
        AMF_ENCODER_NOT_PRESENT as i64, 36,
        "AMF_ENCODER_NOT_PRESENT"
    );
    assert_eq!(AMF_NEED_MORE_INPUT as i64, 44, "AMF_NEED_MORE_INPUT");
    assert_eq!(AMF_VULKAN_FAILED as i64, 45, "AMF_VULKAN_FAILED");

    assert_eq!(AMF_SURFACE_UNKNOWN as i64, 0, "AMF_SURFACE_UNKNOWN");
    assert_eq!(AMF_SURFACE_NV12 as i64, 1, "AMF_SURFACE_NV12");
    assert_eq!(AMF_SURFACE_YV12 as i64, 2, "AMF_SURFACE_YV12");
    assert_eq!(AMF_SURFACE_BGRA as i64, 3, "AMF_SURFACE_BGRA");
    assert_eq!(AMF_SURFACE_ARGB as i64, 4, "AMF_SURFACE_ARGB");
    assert_eq!(AMF_SURFACE_RGBA as i64, 5, "AMF_SURFACE_RGBA");
    assert_eq!(AMF_SURFACE_P010 as i64, 10, "AMF_SURFACE_P010");
    assert_eq!(
        AMF_SURFACE_R10G10B10A2 as i64, 13,
        "AMF_SURFACE_R10G10B10A2"
    );

    assert_eq!(AMF_MEMORY_HOST as i64, 1, "AMF_MEMORY_HOST");
    assert_eq!(AMF_MEMORY_DX9 as i64, 2, "AMF_MEMORY_DX9");
    assert_eq!(AMF_MEMORY_DX11 as i64, 3, "AMF_MEMORY_DX11");
    assert_eq!(AMF_MEMORY_OPENCL as i64, 4, "AMF_MEMORY_OPENCL");
    assert_eq!(AMF_MEMORY_VULKAN as i64, 10, "AMF_MEMORY_VULKAN");
    assert_eq!(AMF_MEMORY_DX12 as i64, 11, "AMF_MEMORY_DX12");
    assert_eq!(AMF_DX11_0 as i64, 110, "AMF_DX11_0");
    assert_eq!(AMF_DX11_1 as i64, 111, "AMF_DX11_1");
    assert_eq!(AMF_DX12 as i64, 120, "AMF_DX12");

    assert_eq!(AMF_DATA_BUFFER as i64, 0, "AMF_DATA_BUFFER");
    assert_eq!(AMF_DATA_SURFACE as i64, 1, "AMF_DATA_SURFACE");
    assert_eq!(AMF_VARIANT_EMPTY as i64, 0, "AMF_VARIANT_EMPTY");
    assert_eq!(AMF_VARIANT_BOOL as i64, 1, "AMF_VARIANT_BOOL");
    assert_eq!(AMF_VARIANT_INT64 as i64, 2, "AMF_VARIANT_INT64");
    assert_eq!(AMF_VARIANT_DOUBLE as i64, 3, "AMF_VARIANT_DOUBLE");
    assert_eq!(AMF_VARIANT_RECT as i64, 4, "AMF_VARIANT_RECT");
    assert_eq!(AMF_VARIANT_SIZE as i64, 5, "AMF_VARIANT_SIZE");
    assert_eq!(AMF_VARIANT_POINT as i64, 6, "AMF_VARIANT_POINT");
    assert_eq!(AMF_VARIANT_RATE as i64, 7, "AMF_VARIANT_RATE");
    assert_eq!(AMF_VARIANT_RATIO as i64, 8, "AMF_VARIANT_RATIO");
    assert_eq!(AMF_VARIANT_COLOR as i64, 9, "AMF_VARIANT_COLOR");
    assert_eq!(AMF_VARIANT_STRING as i64, 10, "AMF_VARIANT_STRING");
    assert_eq!(AMF_VARIANT_WSTRING as i64, 11, "AMF_VARIANT_WSTRING");
    assert_eq!(AMF_VARIANT_INTERFACE as i64, 12, "AMF_VARIANT_INTERFACE");
    assert_eq!(AMF_FRAME_PROGRESSIVE as i64, 0, "AMF_FRAME_PROGRESSIVE");
    assert_eq!(AMF_FRAME_UNKNOWN as i64, -1, "AMF_FRAME_UNKNOWN");
    assert_eq!(
        AMF_ACCEL_NOT_SUPPORTED as i64, -1,
        "AMF_ACCEL_NOT_SUPPORTED"
    );
    assert_eq!(AMF_ACCEL_HARDWARE as i64, 0, "AMF_ACCEL_HARDWARE");
    assert_eq!(AMF_ACCEL_GPU as i64, 1, "AMF_ACCEL_GPU");
    assert_eq!(AMF_ACCEL_SOFTWARE as i64, 2, "AMF_ACCEL_SOFTWARE");

    assert_eq!(
        AMF_VIDEO_ENCODER_USAGE_TRANSCODING as i64, 0,
        "AMF_VIDEO_ENCODER_USAGE_TRANSCODING"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY as i64, 1,
        "AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY as i64, 2,
        "AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_USAGE_WEBCAM as i64, 3,
        "AMF_VIDEO_ENCODER_USAGE_WEBCAM"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_USAGE_HIGH_QUALITY as i64, 4,
        "AMF_VIDEO_ENCODER_USAGE_HIGH_QUALITY"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY_HIGH_QUALITY as i64, 5,
        "AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY_HIGH_QUALITY"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP as i64, 0,
        "AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR as i64, 1,
        "AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR as i64, 2,
        "AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR as i64, 3,
        "AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_QUALITY_VBR as i64, 4,
        "AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_QUALITY_VBR"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_QUALITY_PRESET_BALANCED as i64, 0,
        "AMF_VIDEO_ENCODER_QUALITY_PRESET_BALANCED"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED as i64, 1,
        "AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_QUALITY_PRESET_QUALITY as i64, 2,
        "AMF_VIDEO_ENCODER_QUALITY_PRESET_QUALITY"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PROFILE_BASELINE as i64, 66,
        "AMF_VIDEO_ENCODER_PROFILE_BASELINE"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PROFILE_MAIN as i64, 77,
        "AMF_VIDEO_ENCODER_PROFILE_MAIN"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PROFILE_HIGH as i64, 100,
        "AMF_VIDEO_ENCODER_PROFILE_HIGH"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE as i64, 256,
        "AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_HIGH as i64, 257,
        "AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_HIGH"
    );
    assert_eq!(AMF_H264_LEVEL__3_1 as i64, 31, "AMF_H264_LEVEL__3_1");
    assert_eq!(AMF_H264_LEVEL__4 as i64, 40, "AMF_H264_LEVEL__4");
    assert_eq!(AMF_H264_LEVEL__4_1 as i64, 41, "AMF_H264_LEVEL__4_1");
    assert_eq!(AMF_H264_LEVEL__4_2 as i64, 42, "AMF_H264_LEVEL__4_2");
    assert_eq!(AMF_H264_LEVEL__5 as i64, 50, "AMF_H264_LEVEL__5");
    assert_eq!(AMF_H264_LEVEL__5_1 as i64, 51, "AMF_H264_LEVEL__5_1");
    assert_eq!(AMF_H264_LEVEL__5_2 as i64, 52, "AMF_H264_LEVEL__5_2");
    assert_eq!(AMF_H264_LEVEL__6 as i64, 60, "AMF_H264_LEVEL__6");
    assert_eq!(AMF_H264_LEVEL__6_1 as i64, 61, "AMF_H264_LEVEL__6_1");
    assert_eq!(AMF_H264_LEVEL__6_2 as i64, 62, "AMF_H264_LEVEL__6_2");
    assert_eq!(
        AMF_VIDEO_ENCODER_PICTURE_TYPE_NONE as i64, 0,
        "AMF_VIDEO_ENCODER_PICTURE_TYPE_NONE"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PICTURE_TYPE_SKIP as i64, 1,
        "AMF_VIDEO_ENCODER_PICTURE_TYPE_SKIP"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR as i64, 2,
        "AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PICTURE_TYPE_I as i64, 3,
        "AMF_VIDEO_ENCODER_PICTURE_TYPE_I"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PICTURE_TYPE_P as i64, 4,
        "AMF_VIDEO_ENCODER_PICTURE_TYPE_P"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR as i64, 0,
        "AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_I as i64, 1,
        "AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_I"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_P as i64, 2,
        "AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_P"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PREENCODE_DISABLED as i64, 0,
        "AMF_VIDEO_ENCODER_PREENCODE_DISABLED"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_PREENCODE_ENABLED as i64, 1,
        "AMF_VIDEO_ENCODER_PREENCODE_ENABLED"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_UNDEFINED as i64, 0,
        "AMF_VIDEO_ENCODER_UNDEFINED"
    );
    assert_eq!(AMF_VIDEO_ENCODER_CABAC as i64, 1, "AMF_VIDEO_ENCODER_CABAC");
    assert_eq!(AMF_VIDEO_ENCODER_CALV as i64, 2, "AMF_VIDEO_ENCODER_CALV");
    assert_eq!(
        AMF_VIDEO_ENCODER_OUTPUT_MODE_FRAME as i64, 0,
        "AMF_VIDEO_ENCODER_OUTPUT_MODE_FRAME"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_OUTPUT_MODE_SLICE as i64, 1,
        "AMF_VIDEO_ENCODER_OUTPUT_MODE_SLICE"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_FRAME as i64, 0,
        "AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_FRAME"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_SLICE as i64, 1,
        "AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_SLICE"
    );
    assert_eq!(
        AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_SLICE_LAST as i64, 2,
        "AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_SLICE_LAST"
    );
}

/// Every property-name wide string, against the header's own value.
///
/// This is the cheapest test in the file and catches the most annoying bug: `SetProperty`
/// on a name AMF does not know returns `AMF_NOT_FOUND`, which a latency knob's
/// log-and-continue path swallows, so a typo is a silently missing setting rather than a
/// failure. The tail also pins the version constants.
#[test]
fn abi_property_names_match_amf_headers() {
    // ---- property-name wide strings (a typo here fails silently at runtime) ----
    assert_wide(
        props::AMF_VIDEO_ENCODER_USAGE,
        "Usage",
        "AMF_VIDEO_ENCODER_USAGE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_QUALITY_PRESET,
        "QualityPreset",
        "AMF_VIDEO_ENCODER_QUALITY_PRESET",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_FRAMESIZE,
        "FrameSize",
        "AMF_VIDEO_ENCODER_FRAMESIZE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_FRAMERATE,
        "FrameRate",
        "AMF_VIDEO_ENCODER_FRAMERATE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_PROFILE,
        "Profile",
        "AMF_VIDEO_ENCODER_PROFILE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_PROFILE_LEVEL,
        "ProfileLevel",
        "AMF_VIDEO_ENCODER_PROFILE_LEVEL",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
        "RateControlMethod",
        "AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_TARGET_BITRATE,
        "TargetBitrate",
        "AMF_VIDEO_ENCODER_TARGET_BITRATE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_PEAK_BITRATE,
        "PeakBitrate",
        "AMF_VIDEO_ENCODER_PEAK_BITRATE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE,
        "VBVBufferSize",
        "AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_INITIAL_VBV_BUFFER_FULLNESS,
        "InitialVBVBufferFullness",
        "AMF_VIDEO_ENCODER_INITIAL_VBV_BUFFER_FULLNESS",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_ENFORCE_HRD,
        "EnforceHRD",
        "AMF_VIDEO_ENCODER_ENFORCE_HRD",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE,
        "FillerDataEnable",
        "AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_B_PIC_PATTERN,
        "BPicturesPattern",
        "AMF_VIDEO_ENCODER_B_PIC_PATTERN",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_MAX_CONSECUTIVE_BPICTURES,
        "MaxConsecutiveBPictures",
        "AMF_VIDEO_ENCODER_MAX_CONSECUTIVE_BPICTURES",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_LOWLATENCY_MODE,
        "LowLatencyInternal",
        "AMF_VIDEO_ENCODER_LOWLATENCY_MODE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_PREENCODE_ENABLE,
        "RateControlPreanalysisEnable",
        "AMF_VIDEO_ENCODER_PREENCODE_ENABLE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE,
        "EnablePreAnalysis",
        "AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_ENABLE_VBAQ,
        "EnableVBAQ",
        "AMF_VIDEO_ENCODER_ENABLE_VBAQ",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_HIGH_MOTION_QUALITY_BOOST_ENABLE,
        "HighMotionQualityBoostEnable",
        "AMF_VIDEO_ENCODER_HIGH_MOTION_QUALITY_BOOST_ENABLE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_IDR_PERIOD,
        "IDRPeriod",
        "AMF_VIDEO_ENCODER_IDR_PERIOD",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_INTRA_PERIOD,
        "IntraPeriod",
        "AMF_VIDEO_ENCODER_INTRA_PERIOD",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING,
        "HeaderInsertionSpacing",
        "AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_DE_BLOCKING_FILTER,
        "DeBlockingFilter",
        "AMF_VIDEO_ENCODER_DE_BLOCKING_FILTER",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_SLICES_PER_FRAME,
        "SlicesPerFrame",
        "AMF_VIDEO_ENCODER_SLICES_PER_FRAME",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CABAC_ENABLE,
        "CABACEnable",
        "AMF_VIDEO_ENCODER_CABAC_ENABLE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_INPUT_FULL_RANGE_COLOR,
        "InputFullRangeColor",
        "AMF_VIDEO_ENCODER_INPUT_FULL_RANGE_COLOR",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_OUTPUT_FULL_RANGE_COLOR,
        "FullRangeColor",
        "AMF_VIDEO_ENCODER_OUTPUT_FULL_RANGE_COLOR",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE,
        "OutColorProfile",
        "AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC,
        "OutColorTransferChar",
        "AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES,
        "OutColorPrimaries",
        "AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_QUERY_TIMEOUT,
        "QueryTimeout",
        "AMF_VIDEO_ENCODER_QUERY_TIMEOUT",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_INPUT_QUEUE_SIZE,
        "InputQueueSize",
        "AMF_VIDEO_ENCODER_INPUT_QUEUE_SIZE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_ENABLE_SMART_ACCESS_VIDEO,
        "EnableEncoderSmartAccessVideo",
        "AMF_VIDEO_ENCODER_ENABLE_SMART_ACCESS_VIDEO",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_MAX_NUM_REFRAMES,
        "MaxNumRefFrames",
        "AMF_VIDEO_ENCODER_MAX_NUM_REFRAMES",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT,
        "IntraRefreshMBsNumberPerSlot",
        "AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_MAX_LTR_FRAMES,
        "MaxOfLTRFrames",
        "AMF_VIDEO_ENCODER_MAX_LTR_FRAMES",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_LTR_MODE,
        "LTRMode",
        "AMF_VIDEO_ENCODER_LTR_MODE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_OUTPUT_MODE,
        "OutputMode",
        "AMF_VIDEO_ENCODER_OUTPUT_MODE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_EXTRADATA,
        "ExtraData",
        "AMF_VIDEO_ENCODER_EXTRADATA",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE,
        "ForcePictureType",
        "AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_INSERT_SPS,
        "InsertSPS",
        "AMF_VIDEO_ENCODER_INSERT_SPS",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_INSERT_PPS,
        "InsertPPS",
        "AMF_VIDEO_ENCODER_INSERT_PPS",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_INSERT_AUD,
        "InsertAUD",
        "AMF_VIDEO_ENCODER_INSERT_AUD",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_MARK_CURRENT_WITH_LTR_INDEX,
        "MarkCurrentWithLTRIndex",
        "AMF_VIDEO_ENCODER_MARK_CURRENT_WITH_LTR_INDEX",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_FORCE_LTR_REFERENCE_BITFIELD,
        "ForceLTRReferenceBitfield",
        "AMF_VIDEO_ENCODER_FORCE_LTR_REFERENCE_BITFIELD",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE,
        "OutputDataType",
        "AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE,
        "OutputBufferType",
        "AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_OUTPUT_MARKED_LTR_INDEX,
        "MarkedLTRIndex",
        "AMF_VIDEO_ENCODER_OUTPUT_MARKED_LTR_INDEX",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_OUTPUT_REFERENCED_LTR_INDEX_BITFIELD,
        "ReferencedLTRIndexBitfield",
        "AMF_VIDEO_ENCODER_OUTPUT_REFERENCED_LTR_INDEX_BITFIELD",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_MAX_BITRATE,
        "MaxBitrate",
        "AMF_VIDEO_ENCODER_CAP_MAX_BITRATE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_NUM_OF_STREAMS,
        "NumOfStreams",
        "AMF_VIDEO_ENCODER_CAP_NUM_OF_STREAMS",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_MAX_PROFILE,
        "MaxProfile",
        "AMF_VIDEO_ENCODER_CAP_MAX_PROFILE",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_MAX_LEVEL,
        "MaxLevel",
        "AMF_VIDEO_ENCODER_CAP_MAX_LEVEL",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_BFRAMES,
        "BFrames",
        "AMF_VIDEO_ENCODER_CAP_BFRAMES",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_MAX_REFERENCE_FRAMES,
        "MaxReferenceFrames",
        "AMF_VIDEO_ENCODER_CAP_MAX_REFERENCE_FRAMES",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_NUM_OF_HW_INSTANCES,
        "NumOfHwInstances",
        "AMF_VIDEO_ENCODER_CAP_NUM_OF_HW_INSTANCES",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_COLOR_CONVERSION,
        "ColorConversion",
        "AMF_VIDEO_ENCODER_CAP_COLOR_CONVERSION",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_PRE_ANALYSIS,
        "PreAnalysis",
        "AMF_VIDEO_ENCODER_CAP_PRE_ANALYSIS",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_MAX_THROUGHPUT,
        "MaxThroughput",
        "AMF_VIDEO_ENCODER_CAP_MAX_THROUGHPUT",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_REQUESTED_THROUGHPUT,
        "RequestedThroughput",
        "AMF_VIDEO_ENCODER_CAP_REQUESTED_THROUGHPUT",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_QUERY_TIMEOUT_SUPPORT,
        "QueryTimeoutSupport",
        "AMF_VIDEO_ENCODER_CAP_QUERY_TIMEOUT_SUPPORT",
    );
    assert_wide(
        props::AMF_VIDEO_ENCODER_CAP_SUPPORT_SLICE_OUTPUT,
        "SupportSliceOutput",
        "AMF_VIDEO_ENCODER_CAP_SUPPORT_SLICE_OUTPUT",
    );
    assert_wide(
        codec::VIDEO_ENCODER_VCE_AVC,
        "AMFVideoEncoderVCE_AVC",
        "AMFVideoEncoderVCE_AVC",
    );
    assert_wide(trace_writer::CONSOLE, "Console", "AMF_TRACE_WRITER_CONSOLE");
    assert_wide(
        trace_writer::DEBUG_OUTPUT,
        "DebugOutput",
        "AMF_TRACE_WRITER_DEBUG_OUTPUT",
    );

    // headers built against AMF_FULL_VERSION 281496451678208 (1.5.2.0)
    assert_eq!(
        HEADERS_FULL_VERSION, 281496451678208,
        "AMF_FULL_VERSION of the vendored headers"
    );
    assert_eq!(
        make_full_version(1, 4, 30, 0),
        281492158545920,
        "AMF_MAKE_FULL_VERSION(1,4,30,0)"
    );
}

// ===========================================================================================
// Hand-written tests. Everything above this line is generated.
// ===========================================================================================

/// This box has no AMD GPU and no `amfrt64.dll`, and neither does the macOS dev machine — so the
/// only outcome the loader can be tested for here is that it *fails cleanly*: a typed `Err` that
/// names the DLL, never a panic and never a partially-initialised loader. On a Radeon box the
/// same test proves the two exported symbols resolve.
///
/// `LoadLibrary` error 126 is a normal "no AMF here" — it also shows up on minimal driver
/// installs and machines missing the VC runtime — which is why this is not something the app
/// surfaces to the user.
#[test]
fn amf_loader_handles_a_missing_runtime() {
    match Amf::load() {
        Ok(amf) => {
            // Present: both entry points resolved, so exercise the harmless one.
            let version = amf
                .query_version()
                .expect("AMFQueryVersion on a loaded runtime");
            assert!(version > 0, "AMFQueryVersion returned 0");
        }
        Err(e) => {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("amfrt64.dll"),
                "a failure to load AMF must name the DLL so the log explains itself, got: {msg}"
            );
        }
    }
}

/// Building an encoder on a machine with no AMD adapter must return an `Err` the `Auto` chain can
/// fall through, not panic and not hang.
#[test]
fn amf_encoder_construction_fails_cleanly_without_amd_hardware() {
    use crate::streamer::config::H264Profile;
    use crate::windows_utils::streamer::amd::encoder::Encoder;
    use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;

    let cfg = EncoderConfig {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_bps: 10_000_000,
        max_bitrate_bps: 10_000_000,
        profile: H264Profile::Baseline,
        qp: None,
        intra_refresh: false,
    };
    // On an AMD box this legitimately succeeds; the point is that neither outcome panics.
    if let Err(e) = Encoder::new(cfg) {
        let msg = format!("{e:#}");
        assert!(
            msg.contains("amfrt64.dll") || msg.contains("AMD") || msg.contains("AMF"),
            "the AMF failure path must say why, got: {msg}"
        );
    }
}

#[test]
fn vendors_are_identified_by_pci_id() {
    use crate::windows_utils::streamer::Vendor;

    assert_eq!(Vendor::from_pci_id(0x10DE), Vendor::Nvidia);
    assert_eq!(Vendor::from_pci_id(0x8086), Vendor::Intel);
    assert_eq!(Vendor::from_pci_id(0x1002), Vendor::Amd);
    // WARP, the D3D11 software rasterizer: its description contains none of the vendor names.
    assert_eq!(Vendor::from_pci_id(0x1414), Vendor::Unknown);
    // AMD's CPU vendor id never appears on a display adapter and must not be treated as one.
    assert_eq!(Vendor::from_pci_id(0x1022), Vendor::Unknown);
}

/// The ULTRA_LOW_LATENCY probe is AMD's own: set `Usage`, then read a property back; if the read
/// errors, the usage did not take and `Init()` would fail outright on pre-RDNA parts from driver
/// 23.7.1 onward. This asserts the decision, which is the part that has to be right without
/// hardware.
#[test]
fn ull_probe_failure_selects_low_latency() {
    fn usage_after_probe(probe_ok: bool) -> AMF_VIDEO_ENCODER_USAGE {
        if probe_ok {
            AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY
        } else {
            AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY
        }
    }
    assert_eq!(
        usage_after_probe(true),
        AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY
    );
    assert_eq!(
        usage_after_probe(false),
        AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY
    );
    // ULL and LL differ only in rate control, VBV size and LowLatencyInternal — all of which the
    // property table overrides explicitly — so the fallback costs nothing.
    assert_ne!(
        AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY,
        AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY
    );
}

#[test]
fn result_codes_classify_into_retry_rebuild_and_give_up() {
    // Retry the same call.
    for r in [
        AMF_OK,
        AMF_REPEAT,
        AMF_INPUT_FULL,
        AMF_NEED_MORE_INPUT,
        AMF_EOF,
    ] {
        assert_eq!(classify(r), AmfErrorClass::Transient, "{}", result_name(r));
    }
    // Device-lost / TDR class: tear the session down and rebuild it.
    for r in [
        AMF_NO_DEVICE,
        AMF_DIRECTX_FAILED,
        AMF_RESOLUTION_CHANGED,
        AMF_OUT_OF_MEMORY,
    ] {
        assert_eq!(
            classify(r),
            AmfErrorClass::Recoverable,
            "{}",
            result_name(r)
        );
    }
    // Facts about this machine that never improve: fall through to the next vendor.
    for r in [
        AMF_ENCODER_NOT_PRESENT,
        AMF_SURFACE_FORMAT_NOT_SUPPORTED,
        AMF_INVALID_RESOLUTION,
        AMF_CODEC_NOT_SUPPORTED,
    ] {
        assert_eq!(classify(r), AmfErrorClass::Fatal, "{}", result_name(r));
    }
}

/// `AMFInit` is asked for a **lower** version than the headers we vendor. AMF serves clients
/// built against older SDKs and rejects newer ones, so pinning high is the one way to lock out
/// hardware that would otherwise work.
#[test]
fn amfinit_version_is_pinned_below_the_vendored_headers() {
    const {
        assert!(
            AMFINIT_VERSION < HEADERS_FULL_VERSION,
            "AMFInit must ask for an older version than the vendored headers"
        )
    };
    assert_eq!(format_version(AMFINIT_VERSION), "1.4.30.0");
    assert_eq!(format_version(HEADERS_FULL_VERSION), "1.5.2.0");
}

/// Variants are built by hand because `AMFVariantInit`/`AMFVariantAssign*` are `static
/// AMF_INLINE` in the header and not exported from the DLL.
#[test]
fn variants_carry_the_right_tag_and_payload() {
    assert_eq!(AMFVariantStruct::empty().r#type, AMF_VARIANT_EMPTY);

    let b = AMFVariantStruct::bool(true);
    assert_eq!(b.r#type, AMF_VARIANT_BOOL);
    assert_eq!(b.as_bool(), Some(true));
    assert_eq!(AMFVariantStruct::bool(false).as_bool(), Some(false));

    let i = AMFVariantStruct::int64(i32::MAX as i64);
    assert_eq!(i.r#type, AMF_VARIANT_INT64);
    assert_eq!(i.as_i64(), Some(i32::MAX as i64));

    // AMFSize is signed and AMFRate is unsigned; they are otherwise byte-identical, so only the
    // tag keeps them apart and a binding that reused one struct for both would pass the wrong
    // AMF_VARIANT_TYPE and be silently rejected.
    assert_eq!(AMFVariantStruct::size(1920, 1080).r#type, AMF_VARIANT_SIZE);
    assert_eq!(AMFVariantStruct::rate(60, 1).r#type, AMF_VARIANT_RATE);
    assert_ne!(AMF_VARIANT_SIZE, AMF_VARIANT_RATE);

    // A non-interface variant must never hand out an interface pointer to Release.
    assert!(AMFVariantStruct::int64(7).as_interface().is_none());
}

/// The AVC enum values are not shared with HEVC or AV1: AVC's `CBR = 1` means
/// `LATENCY_CONSTRAINED_VBR` on HEVC, and AVC's `SPEED = 1` is 10 on HEVC and 100 on AV1. This
/// pins the AVC numbers so a future codec module cannot quietly reuse them.
#[test]
fn avc_enum_values_are_avc_specific() {
    assert_eq!(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR, 1);
    assert_eq!(AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED, 1);
    assert_eq!(AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY, 1);
    assert_eq!(AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY, 2);
    // AMF_ACCEL_HARDWARE is 0 and "not supported" is the negative one, so a truthiness test on
    // the acceleration type is backwards.
    assert_eq!(AMF_ACCEL_HARDWARE, 0);
    assert_eq!(AMF_ACCEL_NOT_SUPPORTED, -1);
}

#[test]
fn dxgi_formats_map_to_amf_surface_formats() {
    use crate::windows_utils::streamer::amd::encoder::{
        amf_surface_format, PREFERRED_INPUT_FORMAT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_NV12,
    };

    assert_eq!(amf_surface_format(DXGI_FORMAT_NV12), Some(AMF_SURFACE_NV12));
    assert_eq!(
        amf_surface_format(DXGI_FORMAT_B8G8R8A8_UNORM),
        Some(AMF_SURFACE_BGRA)
    );
    assert_eq!(amf_surface_format(DXGI_FORMAT_D32_FLOAT), None);
    // Today the pipeline hands us BGRA; when scaler.rs learns NV12 this constant moves and the
    // encoder's retune path stops being exercised.
    assert!(amf_surface_format(PREFERRED_INPUT_FORMAT).is_some());
}

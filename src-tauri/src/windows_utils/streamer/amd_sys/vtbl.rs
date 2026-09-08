//! The AMF C ABI: one `#[repr(C)]` struct per vtable, in exact slot order.
//!
//! Field names are the C member names verbatim (hence `#![allow(non_snake_case)]` on the
//! module) so `src-tauri/tools/amf_layout_gen.c`'s output pastes straight into
//! `windows_utils/streamer/test/amd_layout.rs`. Slot order **is** the ABI: calling slot 48
//! where you meant 49 is not a type error, it is a call through the wrong function pointer.
//!
//! Slots this backend never calls are typed `*const c_void`. They still have to be declared —
//! the total slot count is the only assertion that catches a member omitted entirely, and an
//! omission silently shifts every slot after it.
//!
//! Two traps encoded here:
//! * `AMFFactory` and `AMFTrace` are **not** `AMFInterface`s. `AMFFactory::CreateContext` is
//!   slot 0, `AMFTrace::TraceW` is slot 0; neither has Acquire/Release/QueryInterface.
//! * `AMFSurface` has two observer pairs: slots 11/12 take an `AMFPropertyStorageObserver*`,
//!   slots 31/32 an `AMFSurfaceObserver*`. The C++ header overloads both onto the name
//!   `AddObserver`; the C branch disambiguates, and so do these names.

use std::ffi::c_void;

use super::types::*;

// ===========================================================================================
// Objects. Every AMF object is a single pointer to its vtable.
// ===========================================================================================

macro_rules! amf_object {
    ($(#[$m:meta])* $name:ident, $vtbl:ident) => {
        $(#[$m])*
        #[repr(C)]
        pub struct $name {
            pub pVtbl: *const $vtbl,
        }
    };
}

amf_object!(AMFInterface, AMFInterfaceVtbl);
amf_object!(AMFPropertyStorage, AMFPropertyStorageVtbl);
amf_object!(AMFPropertyStorageEx, AMFPropertyStorageExVtbl);
amf_object!(AMFData, AMFDataVtbl);
amf_object!(AMFSurface, AMFSurfaceVtbl);
amf_object!(AMFBuffer, AMFBufferVtbl);
amf_object!(AMFPlane, AMFPlaneVtbl);
amf_object!(AMFContext, AMFContextVtbl);
amf_object!(AMFContext1, AMFContext1Vtbl);
amf_object!(AMFComponent, AMFComponentVtbl);
amf_object!(AMFCaps, AMFCapsVtbl);
amf_object!(AMFIOCaps, AMFIOCapsVtbl);
amf_object!(AMFFactory, AMFFactoryVtbl);
amf_object!(AMFTrace, AMFTraceVtbl);

// ===========================================================================================
// AMFInterface — core/Interface.h (3 slots)
// ===========================================================================================

#[repr(C)]
pub struct AMFInterfaceVtbl {
    pub Acquire: unsafe extern "system" fn(*mut AMFInterface) -> amf_long,
    pub Release: unsafe extern "system" fn(*mut AMFInterface) -> amf_long,
    pub QueryInterface: unsafe extern "system" fn(
        *mut AMFInterface,
        *const AMFGuid,
        *mut *mut c_void,
    ) -> AMF_RESULT,
}

// ===========================================================================================
// AMFPropertyStorage — core/PropertyStorage.h (13 slots)
//
// Slots 0..=12 of AMFData, AMFSurface, AMFBuffer, AMFContext, AMFComponent and AMFCaps are
// layout-identical to this vtable, which is why `props.rs`'s helpers can drive all of them
// through one `*mut AMFPropertyStorage` cast.
// ===========================================================================================

#[repr(C)]
pub struct AMFPropertyStorageVtbl {
    pub Acquire: unsafe extern "system" fn(*mut AMFPropertyStorage) -> amf_long,
    pub Release: unsafe extern "system" fn(*mut AMFPropertyStorage) -> amf_long,
    pub QueryInterface: unsafe extern "system" fn(
        *mut AMFPropertyStorage,
        *const AMFGuid,
        *mut *mut c_void,
    ) -> AMF_RESULT,
    pub SetProperty: unsafe extern "system" fn(
        *mut AMFPropertyStorage,
        *const u16,
        AMFVariantStruct,
    ) -> AMF_RESULT,
    pub GetProperty: unsafe extern "system" fn(
        *mut AMFPropertyStorage,
        *const u16,
        *mut AMFVariantStruct,
    ) -> AMF_RESULT,
    pub HasProperty: unsafe extern "system" fn(*mut AMFPropertyStorage, *const u16) -> amf_bool,
    pub GetPropertyCount: unsafe extern "system" fn(*mut AMFPropertyStorage) -> amf_size,
    pub GetPropertyAt: unsafe extern "system" fn(
        *mut AMFPropertyStorage,
        amf_size,
        *mut u16,
        amf_size,
        *mut AMFVariantStruct,
    ) -> AMF_RESULT,
    pub Clear: unsafe extern "system" fn(*mut AMFPropertyStorage) -> AMF_RESULT,
    pub AddTo: *const c_void,
    pub CopyTo: *const c_void,
    pub AddObserver: *const c_void,
    pub RemoveObserver: *const c_void,
}

// ===========================================================================================
// AMFPropertyStorageEx — core/PropertyStorageEx.h (17 slots)
//
// Declared for the layout test only. `GetPropertyInfo` (slot 15) is deliberately never called:
// `AMFPropertyInfo` is 112 bytes in the C branch and 120 in C++ (it has a virtual destructor),
// and amfrt64.dll is built from the C++ headers, so a C-shaped binding reads every field one
// pointer early. Optional properties are probed with `HasProperty` instead.
// ===========================================================================================

#[repr(C)]
pub struct AMFPropertyStorageExVtbl {
    pub Acquire: *const c_void,
    pub Release: *const c_void,
    pub QueryInterface: *const c_void,
    pub SetProperty: *const c_void,
    pub GetProperty: *const c_void,
    pub HasProperty: *const c_void,
    pub GetPropertyCount: *const c_void,
    pub GetPropertyAt: *const c_void,
    pub Clear: *const c_void,
    pub AddTo: *const c_void,
    pub CopyTo: *const c_void,
    pub AddObserver: *const c_void,
    pub RemoveObserver: *const c_void,
    pub GetPropertiesInfoCount: *const c_void,
    pub GetPropertyInfoAt: *const c_void,
    pub GetPropertyInfo: *const c_void,
    pub ValidateProperty: *const c_void,
}

// ===========================================================================================
// AMFData — core/Data.h (23 slots)
// ===========================================================================================

#[repr(C)]
pub struct AMFDataVtbl {
    pub Acquire: unsafe extern "system" fn(*mut AMFData) -> amf_long,
    pub Release: unsafe extern "system" fn(*mut AMFData) -> amf_long,
    pub QueryInterface:
        unsafe extern "system" fn(*mut AMFData, *const AMFGuid, *mut *mut c_void) -> AMF_RESULT,
    pub SetProperty: *const c_void,
    pub GetProperty:
        unsafe extern "system" fn(*mut AMFData, *const u16, *mut AMFVariantStruct) -> AMF_RESULT,
    pub HasProperty: *const c_void,
    pub GetPropertyCount: *const c_void,
    pub GetPropertyAt: *const c_void,
    pub Clear: *const c_void,
    pub AddTo: *const c_void,
    pub CopyTo: *const c_void,
    pub AddObserver: *const c_void,
    pub RemoveObserver: *const c_void,
    pub GetMemoryType: unsafe extern "system" fn(*mut AMFData) -> AMF_MEMORY_TYPE,
    pub Duplicate: *const c_void,
    /// Copies through host memory when it must. Never on the hot path.
    pub Convert: *const c_void,
    /// Refuses rather than copying. Never on the hot path.
    pub Interop: *const c_void,
    pub GetDataType: unsafe extern "system" fn(*mut AMFData) -> AMF_DATA_TYPE,
    pub IsReusable: *const c_void,
    pub SetPts: unsafe extern "system" fn(*mut AMFData, amf_pts),
    pub GetPts: unsafe extern "system" fn(*mut AMFData) -> amf_pts,
    pub SetDuration: unsafe extern "system" fn(*mut AMFData, amf_pts),
    pub GetDuration: unsafe extern "system" fn(*mut AMFData) -> amf_pts,
}

// ===========================================================================================
// AMFSurface — core/Surface.h (33 slots)
// ===========================================================================================

#[repr(C)]
pub struct AMFSurfaceVtbl {
    pub Acquire: unsafe extern "system" fn(*mut AMFSurface) -> amf_long,
    pub Release: unsafe extern "system" fn(*mut AMFSurface) -> amf_long,
    pub QueryInterface: *const c_void,
    pub SetProperty: *const c_void,
    pub GetProperty: *const c_void,
    pub HasProperty: *const c_void,
    pub GetPropertyCount: *const c_void,
    pub GetPropertyAt: *const c_void,
    pub Clear: *const c_void,
    pub AddTo: *const c_void,
    pub CopyTo: *const c_void,
    pub AddObserver: *const c_void,
    pub RemoveObserver: *const c_void,
    pub GetMemoryType: *const c_void,
    pub Duplicate: *const c_void,
    pub Convert: *const c_void,
    pub Interop: *const c_void,
    pub GetDataType: *const c_void,
    pub IsReusable: *const c_void,
    pub SetPts: unsafe extern "system" fn(*mut AMFSurface, amf_pts),
    pub GetPts: *const c_void,
    pub SetDuration: unsafe extern "system" fn(*mut AMFSurface, amf_pts),
    pub GetDuration: *const c_void,
    pub GetFormat: unsafe extern "system" fn(*mut AMFSurface) -> AMF_SURFACE_FORMAT,
    pub GetPlanesCount: *const c_void,
    pub GetPlaneAt: *const c_void,
    pub GetPlane: *const c_void,
    pub GetFrameType: *const c_void,
    pub SetFrameType: unsafe extern "system" fn(*mut AMFSurface, AMF_FRAME_TYPE),
    /// Input hardware surfaces can be aligned up by 16; a crop keeps the coded picture at the
    /// size the client asked for.
    pub SetCrop: unsafe extern "system" fn(
        *mut AMFSurface,
        amf_int32,
        amf_int32,
        amf_int32,
        amf_int32,
    ) -> AMF_RESULT,
    pub CopySurfaceRegion: *const c_void,
    pub AddObserver_Surface: *const c_void,
    pub RemoveObserver_Surface: *const c_void,
}

// ===========================================================================================
// AMFBuffer — core/Buffer.h (28 slots)
//
// GetNative (25) + GetSize (24) are the whole hot-path output read: the encoder hands back a
// buffer whose native pointer is host memory holding the Annex-B access unit.
// ===========================================================================================

#[repr(C)]
pub struct AMFBufferVtbl {
    pub Acquire: unsafe extern "system" fn(*mut AMFBuffer) -> amf_long,
    pub Release: unsafe extern "system" fn(*mut AMFBuffer) -> amf_long,
    pub QueryInterface: *const c_void,
    pub SetProperty: *const c_void,
    pub GetProperty:
        unsafe extern "system" fn(*mut AMFBuffer, *const u16, *mut AMFVariantStruct) -> AMF_RESULT,
    pub HasProperty: *const c_void,
    pub GetPropertyCount: *const c_void,
    pub GetPropertyAt: *const c_void,
    pub Clear: *const c_void,
    pub AddTo: *const c_void,
    pub CopyTo: *const c_void,
    pub AddObserver: *const c_void,
    pub RemoveObserver: *const c_void,
    pub GetMemoryType: *const c_void,
    pub Duplicate: *const c_void,
    pub Convert: *const c_void,
    pub Interop: *const c_void,
    pub GetDataType: *const c_void,
    pub IsReusable: *const c_void,
    pub SetPts: *const c_void,
    pub GetPts: unsafe extern "system" fn(*mut AMFBuffer) -> amf_pts,
    pub SetDuration: *const c_void,
    pub GetDuration: *const c_void,
    pub SetSize: *const c_void,
    pub GetSize: unsafe extern "system" fn(*mut AMFBuffer) -> amf_size,
    pub GetNative: unsafe extern "system" fn(*mut AMFBuffer) -> *mut c_void,
    pub AddObserver_Buffer: *const c_void,
    pub RemoveObserver_Buffer: *const c_void,
}

// ===========================================================================================
// AMFPlane — core/Plane.h (13 slots)
//
// Declared for the layout test and a future CPU-upload path. `GetPlaneAt`/`GetPlane` return a
// borrowed pointer: the header says "do not store planes outside".
// ===========================================================================================

#[repr(C)]
pub struct AMFPlaneVtbl {
    pub Acquire: *const c_void,
    pub Release: *const c_void,
    pub QueryInterface: *const c_void,
    pub GetType: *const c_void,
    pub GetNative: unsafe extern "system" fn(*mut AMFPlane) -> *mut c_void,
    pub GetPixelSizeInBytes: *const c_void,
    pub GetOffsetX: *const c_void,
    pub GetOffsetY: *const c_void,
    pub GetWidth: unsafe extern "system" fn(*mut AMFPlane) -> amf_int32,
    pub GetHeight: unsafe extern "system" fn(*mut AMFPlane) -> amf_int32,
    pub GetHPitch: unsafe extern "system" fn(*mut AMFPlane) -> amf_int32,
    pub GetVPitch: unsafe extern "system" fn(*mut AMFPlane) -> amf_int32,
    pub IsTiled: *const c_void,
}

// ===========================================================================================
// AMFContext — core/Context.h (55 slots)
//
// The OpenCL / OpenGL / XV / Gralloc blocks between slots 22 and 42 are why hand-counting to
// slot 49 goes wrong. They are all declared, unused.
// ===========================================================================================

#[repr(C)]
pub struct AMFContextVtbl {
    pub Acquire: unsafe extern "system" fn(*mut AMFContext) -> amf_long,
    pub Release: unsafe extern "system" fn(*mut AMFContext) -> amf_long,
    pub QueryInterface:
        unsafe extern "system" fn(*mut AMFContext, *const AMFGuid, *mut *mut c_void) -> AMF_RESULT,
    pub SetProperty: *const c_void,
    pub GetProperty: *const c_void,
    pub HasProperty: *const c_void,
    pub GetPropertyCount: *const c_void,
    pub GetPropertyAt: *const c_void,
    pub Clear: *const c_void,
    pub AddTo: *const c_void,
    pub CopyTo: *const c_void,
    pub AddObserver: *const c_void,
    pub RemoveObserver: *const c_void,
    pub Terminate: unsafe extern "system" fn(*mut AMFContext) -> AMF_RESULT,
    pub InitDX9: *const c_void,
    pub GetDX9Device: *const c_void,
    pub LockDX9: *const c_void,
    pub UnlockDX9: *const c_void,
    /// Passing our own device is what makes `CreateSurfaceFromDX11Native` a pointer wrap.
    /// Passing NULL makes AMF create its own device and every submit a hidden cross-device copy.
    pub InitDX11:
        unsafe extern "system" fn(*mut AMFContext, *mut c_void, AMF_DX_VERSION) -> AMF_RESULT,
    pub GetDX11Device: *const c_void,
    pub LockDX11: *const c_void,
    pub UnlockDX11: *const c_void,
    pub InitOpenCL: *const c_void,
    pub GetOpenCLContext: *const c_void,
    pub GetOpenCLCommandQueue: *const c_void,
    pub GetOpenCLDeviceID: *const c_void,
    pub GetOpenCLComputeFactory: *const c_void,
    pub InitOpenCLEx: *const c_void,
    pub LockOpenCL: *const c_void,
    pub UnlockOpenCL: *const c_void,
    pub InitOpenGL: *const c_void,
    pub GetOpenGLContext: *const c_void,
    pub GetOpenGLDrawable: *const c_void,
    pub LockOpenGL: *const c_void,
    pub UnlockOpenGL: *const c_void,
    pub InitXV: *const c_void,
    pub GetXVDevice: *const c_void,
    pub LockXV: *const c_void,
    pub UnlockXV: *const c_void,
    pub InitGralloc: *const c_void,
    pub GetGrallocDevice: *const c_void,
    pub LockGralloc: *const c_void,
    pub UnlockGralloc: *const c_void,
    pub AllocBuffer: *const c_void,
    pub AllocSurface: *const c_void,
    pub AllocAudioBuffer: *const c_void,
    pub CreateBufferFromHostNative: *const c_void,
    pub CreateSurfaceFromHostNative: *const c_void,
    pub CreateSurfaceFromDX9Native: *const c_void,
    /// Slot 49, the hot path. **Wraps, it does not copy** — the `ID3D11Texture2D` must stay
    /// alive and unmodified until the surface's data is released. The third parameter is an
    /// `AMFSurfaceObserver*`; we pass NULL and rely on one-frame-in-flight discipline instead.
    pub CreateSurfaceFromDX11Native: unsafe extern "system" fn(
        *mut AMFContext,
        *mut c_void,
        *mut *mut AMFSurface,
        *mut c_void,
    ) -> AMF_RESULT,
    pub CreateSurfaceFromOpenGLNative: *const c_void,
    pub CreateSurfaceFromGrallocNative: *const c_void,
    pub CreateSurfaceFromOpenCLNative: *const c_void,
    pub CreateBufferFromOpenCLNative: *const c_void,
    pub GetCompute: *const c_void,
}

// ===========================================================================================
// AMFContext1 — core/Context.h (65 slots). Slots 0..=54 are identical to AMFContext.
// Declared for the layout test; the encoder has no need for AllocSurfaceEx or Vulkan.
// ===========================================================================================

#[repr(C)]
pub struct AMFContext1Vtbl {
    pub Acquire: *const c_void,
    pub Release: *const c_void,
    pub QueryInterface: *const c_void,
    pub SetProperty: *const c_void,
    pub GetProperty: *const c_void,
    pub HasProperty: *const c_void,
    pub GetPropertyCount: *const c_void,
    pub GetPropertyAt: *const c_void,
    pub Clear: *const c_void,
    pub AddTo: *const c_void,
    pub CopyTo: *const c_void,
    pub AddObserver: *const c_void,
    pub RemoveObserver: *const c_void,
    pub Terminate: *const c_void,
    pub InitDX9: *const c_void,
    pub GetDX9Device: *const c_void,
    pub LockDX9: *const c_void,
    pub UnlockDX9: *const c_void,
    pub InitDX11: *const c_void,
    pub GetDX11Device: *const c_void,
    pub LockDX11: *const c_void,
    pub UnlockDX11: *const c_void,
    pub InitOpenCL: *const c_void,
    pub GetOpenCLContext: *const c_void,
    pub GetOpenCLCommandQueue: *const c_void,
    pub GetOpenCLDeviceID: *const c_void,
    pub GetOpenCLComputeFactory: *const c_void,
    pub InitOpenCLEx: *const c_void,
    pub LockOpenCL: *const c_void,
    pub UnlockOpenCL: *const c_void,
    pub InitOpenGL: *const c_void,
    pub GetOpenGLContext: *const c_void,
    pub GetOpenGLDrawable: *const c_void,
    pub LockOpenGL: *const c_void,
    pub UnlockOpenGL: *const c_void,
    pub InitXV: *const c_void,
    pub GetXVDevice: *const c_void,
    pub LockXV: *const c_void,
    pub UnlockXV: *const c_void,
    pub InitGralloc: *const c_void,
    pub GetGrallocDevice: *const c_void,
    pub LockGralloc: *const c_void,
    pub UnlockGralloc: *const c_void,
    pub AllocBuffer: *const c_void,
    pub AllocSurface: *const c_void,
    pub AllocAudioBuffer: *const c_void,
    pub CreateBufferFromHostNative: *const c_void,
    pub CreateSurfaceFromHostNative: *const c_void,
    pub CreateSurfaceFromDX9Native: *const c_void,
    pub CreateSurfaceFromDX11Native: *const c_void,
    pub CreateSurfaceFromOpenGLNative: *const c_void,
    pub CreateSurfaceFromGrallocNative: *const c_void,
    pub CreateSurfaceFromOpenCLNative: *const c_void,
    pub CreateBufferFromOpenCLNative: *const c_void,
    pub GetCompute: *const c_void,
    pub CreateBufferFromDX11Native: *const c_void,
    pub AllocBufferEx: *const c_void,
    pub AllocSurfaceEx: *const c_void,
    pub InitVulkan: *const c_void,
    pub GetVulkanDevice: *const c_void,
    pub LockVulkan: *const c_void,
    pub UnlockVulkan: *const c_void,
    pub CreateSurfaceFromVulkanNative: *const c_void,
    pub CreateBufferFromVulkanNative: *const c_void,
    pub GetVulkanDeviceExtensions: *const c_void,
}

// ===========================================================================================
// AMFComponent — components/Component.h (28 slots). Slots 0..=16 are AMFPropertyStorageEx.
// ===========================================================================================

#[repr(C)]
pub struct AMFComponentVtbl {
    pub Acquire: unsafe extern "system" fn(*mut AMFComponent) -> amf_long,
    pub Release: unsafe extern "system" fn(*mut AMFComponent) -> amf_long,
    pub QueryInterface: *const c_void,
    pub SetProperty:
        unsafe extern "system" fn(*mut AMFComponent, *const u16, AMFVariantStruct) -> AMF_RESULT,
    pub GetProperty: unsafe extern "system" fn(
        *mut AMFComponent,
        *const u16,
        *mut AMFVariantStruct,
    ) -> AMF_RESULT,
    pub HasProperty: unsafe extern "system" fn(*mut AMFComponent, *const u16) -> amf_bool,
    pub GetPropertyCount: *const c_void,
    pub GetPropertyAt: *const c_void,
    pub Clear: *const c_void,
    pub AddTo: *const c_void,
    pub CopyTo: *const c_void,
    pub AddObserver: *const c_void,
    pub RemoveObserver: *const c_void,
    pub GetPropertiesInfoCount: *const c_void,
    pub GetPropertyInfoAt: *const c_void,
    pub GetPropertyInfo: *const c_void,
    pub ValidateProperty: *const c_void,
    pub Init: unsafe extern "system" fn(
        *mut AMFComponent,
        AMF_SURFACE_FORMAT,
        amf_int32,
        amf_int32,
    ) -> AMF_RESULT,
    pub ReInit: *const c_void,
    pub Terminate: unsafe extern "system" fn(*mut AMFComponent) -> AMF_RESULT,
    pub Drain: unsafe extern "system" fn(*mut AMFComponent) -> AMF_RESULT,
    /// Reserved for re-init. Ending a stream is `Drain` then `QueryOutput` until `AMF_EOF`.
    pub Flush: unsafe extern "system" fn(*mut AMFComponent) -> AMF_RESULT,
    pub SubmitInput: unsafe extern "system" fn(*mut AMFComponent, *mut AMFData) -> AMF_RESULT,
    pub QueryOutput: unsafe extern "system" fn(*mut AMFComponent, *mut *mut AMFData) -> AMF_RESULT,
    pub GetContext: *const c_void,
    pub SetOutputDataAllocatorCB: *const c_void,
    /// Works before `Init()`, which is what makes a pre-flight capability probe possible.
    pub GetCaps: unsafe extern "system" fn(*mut AMFComponent, *mut *mut AMFCaps) -> AMF_RESULT,
    pub Optimize: *const c_void,
}

// ===========================================================================================
// AMFCaps / AMFIOCaps — components/ComponentCaps.h (16 / 11 slots)
// ===========================================================================================

#[repr(C)]
pub struct AMFCapsVtbl {
    pub Acquire: *const c_void,
    pub Release: unsafe extern "system" fn(*mut AMFCaps) -> amf_long,
    pub QueryInterface: *const c_void,
    pub SetProperty: *const c_void,
    pub GetProperty:
        unsafe extern "system" fn(*mut AMFCaps, *const u16, *mut AMFVariantStruct) -> AMF_RESULT,
    pub HasProperty: *const c_void,
    pub GetPropertyCount: *const c_void,
    pub GetPropertyAt: *const c_void,
    pub Clear: *const c_void,
    pub AddTo: *const c_void,
    pub CopyTo: *const c_void,
    pub AddObserver: *const c_void,
    pub RemoveObserver: *const c_void,
    pub GetAccelerationType: unsafe extern "system" fn(*mut AMFCaps) -> AMF_ACCELERATION_TYPE,
    pub GetInputCaps: unsafe extern "system" fn(*mut AMFCaps, *mut *mut AMFIOCaps) -> AMF_RESULT,
    pub GetOutputCaps: *const c_void,
}

#[repr(C)]
pub struct AMFIOCapsVtbl {
    pub Acquire: *const c_void,
    pub Release: unsafe extern "system" fn(*mut AMFIOCaps) -> amf_long,
    pub QueryInterface: *const c_void,
    pub GetWidthRange: unsafe extern "system" fn(*mut AMFIOCaps, *mut amf_int32, *mut amf_int32),
    pub GetHeightRange: unsafe extern "system" fn(*mut AMFIOCaps, *mut amf_int32, *mut amf_int32),
    pub GetVertAlign: unsafe extern "system" fn(*mut AMFIOCaps) -> amf_int32,
    pub GetNumOfFormats: unsafe extern "system" fn(*mut AMFIOCaps) -> amf_int32,
    /// The `native` out-flag is how you learn whether the encoder takes a format directly or
    /// will insert its own colour converter.
    pub GetFormatAt: unsafe extern "system" fn(
        *mut AMFIOCaps,
        amf_int32,
        *mut AMF_SURFACE_FORMAT,
        *mut amf_bool,
    ) -> AMF_RESULT,
    pub GetNumOfMemoryTypes: *const c_void,
    pub GetMemoryTypeAt: *const c_void,
    pub IsInterlacedSupported: *const c_void,
}

// ===========================================================================================
// AMFFactory — core/Factory.h (7 slots). NOT an AMFInterface: CreateContext is slot 0 and the
// factory is a DLL-owned singleton that must never be Released.
// ===========================================================================================

#[repr(C)]
pub struct AMFFactoryVtbl {
    pub CreateContext:
        unsafe extern "system" fn(*mut AMFFactory, *mut *mut AMFContext) -> AMF_RESULT,
    pub CreateComponent: unsafe extern "system" fn(
        *mut AMFFactory,
        *mut AMFContext,
        *const u16,
        *mut *mut AMFComponent,
    ) -> AMF_RESULT,
    pub SetCacheFolder: *const c_void,
    pub GetCacheFolder: *const c_void,
    pub GetDebug: *const c_void,
    pub GetTrace: unsafe extern "system" fn(*mut AMFFactory, *mut *mut AMFTrace) -> AMF_RESULT,
    pub GetPrograms: *const c_void,
}

// ===========================================================================================
// AMFTrace — core/Trace.h (25 slots). Also NOT an AMFInterface; it starts at TraceW.
//
// Slots 0 and 1 are variadic / `va_list*` and are deliberately left untyped: there is no sound
// Rust signature for either and nothing here needs them.
// ===========================================================================================

#[repr(C)]
pub struct AMFTraceVtbl {
    pub TraceW: *const c_void,
    pub Trace: *const c_void,
    pub SetGlobalLevel: unsafe extern "system" fn(*mut AMFTrace, amf_int32) -> amf_int32,
    pub GetGlobalLevel: unsafe extern "system" fn(*mut AMFTrace) -> amf_int32,
    pub EnableWriter: unsafe extern "system" fn(*mut AMFTrace, *const u16, amf_bool) -> amf_bool,
    pub WriterEnabled: *const c_void,
    pub TraceEnableAsync: *const c_void,
    pub TraceFlush: *const c_void,
    pub SetPath: *const c_void,
    pub GetPath: *const c_void,
    pub SetWriterLevel: *const c_void,
    pub GetWriterLevel: *const c_void,
    pub SetWriterLevelForScope: *const c_void,
    pub GetWriterLevelForScope: *const c_void,
    pub GetIndentation: *const c_void,
    pub Indent: *const c_void,
    pub RegisterWriter: *const c_void,
    pub UnregisterWriter: *const c_void,
    pub GetResultText: unsafe extern "system" fn(*mut AMFTrace, AMF_RESULT) -> *const u16,
    pub SurfaceGetFormatName: *const c_void,
    pub SurfaceGetFormatByName: *const c_void,
    pub GetMemoryTypeName: *const c_void,
    pub GetMemoryTypeByName: *const c_void,
    pub GetSampleFormatName: *const c_void,
    pub GetSampleFormatByName: *const c_void,
}

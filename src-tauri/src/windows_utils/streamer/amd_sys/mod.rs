//! Hand-written FFI for AMD's Advanced Media Framework (AMF) C API.
//!
//! Same philosophy as `nvidia/nvenc_sys`: **no SDK at build time, no import library.** The AMF
//! runtime ships with the Radeon driver as `amfrt64.dll`; it is loaded with `libloading` at
//! runtime and only two exported functions are resolved — `AMFInit` and `AMFQueryVersion`.
//! Everything else is reached through COM-like vtables, whose slot order is in `vtbl.rs`.
//!
//! `dumpbin /imports target\release\ScreenExtend.exe | findstr /i amf` must print nothing. If
//! `amfrt64` ever appears in the import table something linked instead of loading, and the
//! binary stops starting on every machine without a Radeon driver.
//!
//! Provenance of every constant: `src-tauri/resources/amf-headers` (AMF 1.5.2.0, MIT), measured
//! by `src-tauri/tools/amf_layout_gen.c` and asserted in
//! `windows_utils/streamer/test/amd_layout.rs`.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

pub mod props;
pub mod types;
pub mod vtbl;

pub use props::{codec, names as prop, trace_writer};
pub use types::*;
pub use vtbl::*;

use std::ffi::c_void;

use anyhow::{Context as _, Result};

/// `AMFInit` and `AMFQueryVersion` are `__cdecl`; every vtable method is `__stdcall`. On x64
/// Windows both collapse into the single Microsoft x64 calling convention, so this is only a
/// documentation difference — but it is a real one on x86, which this project does not ship.
type AMFInitFn = unsafe extern "C" fn(amf_uint64, *mut *mut AMFFactory) -> AMF_RESULT;
type AMFQueryVersionFn = unsafe extern "C" fn(*mut amf_uint64) -> AMF_RESULT;

/// The loaded AMF runtime. Hold this alive for as long as any AMF object exists: dropping the
/// `Library` unloads the code every vtable points into.
pub struct Amf {
    AMFInit: AMFInitFn,
    AMFQueryVersion: AMFQueryVersionFn,
    _lib: libloading::Library,
}

// SAFETY: the two entry points are plain function pointers into a DLL that stays mapped for the
// lifetime of this struct; AMF's own objects carry the thread-affinity rules, not the loader.
unsafe impl Send for Amf {}

impl Amf {
    /// Loads `amfrt64.dll`. A missing DLL is the normal outcome on any machine without a Radeon
    /// driver (and `LoadLibrary` error 126 also shows up on minimal driver installs and boxes
    /// missing the VC runtime) — it is a clean `Err`, not something to surface to the user.
    pub fn load() -> Result<Self> {
        // SAFETY: `Library::new` runs the DLL's entry point. `amfrt64.dll` is a normal
        // user-mode library installed by the Radeon driver; loading it has no side effects
        // beyond its own initialisation.
        let lib = unsafe { libloading::Library::new("amfrt64.dll") }.context(
            "loading amfrt64.dll (the AMF runtime; ships with the AMD Radeon/Adrenalin driver)",
        )?;

        // SAFETY: both symbols are resolved while `lib` is borrowed; the fn pointers are `Copy`
        // and stay valid because `lib` is moved into the struct below and dropped last.
        let (init, query) = unsafe {
            let init: libloading::Symbol<AMFInitFn> = lib
                .get(b"AMFInit\0")
                .context("resolving AMFInit from amfrt64.dll")?;
            let query: libloading::Symbol<AMFQueryVersionFn> = lib
                .get(b"AMFQueryVersion\0")
                .context("resolving AMFQueryVersion from amfrt64.dll")?;
            (*init, *query)
        };

        Ok(Self {
            AMFInit: init,
            AMFQueryVersion: query,
            _lib: lib,
        })
    }

    /// The runtime's own version. Logged for support and used for **nothing else** — every
    /// optional feature is probed. Never parse the DLL's `ProductVersion` resource instead: a
    /// 2025 driver shipped it as `1.5.0.0;1.2;CL`.
    pub fn query_version(&self) -> Result<amf_uint64> {
        let mut v: amf_uint64 = 0;
        // SAFETY: exported `AMFQueryVersion`; the out-parameter is a live local.
        let r = unsafe { (self.AMFQueryVersion)(&mut v) };
        check(r, "AMFQueryVersion")?;
        Ok(v)
    }

    /// Creates the factory. `version` is what we were *built* against, not what the runtime is:
    /// AMF serves clients built against older SDKs and rejects newer ones, so
    /// [`AMFINIT_VERSION`] is pinned low.
    ///
    /// The returned factory is a DLL-owned singleton — never `Release` it.
    pub fn init_factory(&self, version: amf_uint64) -> Result<*mut AMFFactory> {
        let mut factory: *mut AMFFactory = std::ptr::null_mut();
        // SAFETY: exported `AMFInit`; the out-parameter is a live local, and the returned
        // pointer is checked for null before use.
        let r = unsafe { (self.AMFInit)(version, &mut factory) };
        check(r, "AMFInit")?;
        if factory.is_null() {
            anyhow::bail!("AMFInit returned AMF_OK with a null factory");
        }
        Ok(factory)
    }
}

pub fn format_version(v: amf_uint64) -> String {
    format!(
        "{}.{}.{}.{}",
        version_major(v),
        version_minor(v),
        version_release(v),
        version_build(v)
    )
}

/// How the encode loop should react to an `AMF_RESULT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmfErrorClass {
    /// Retry the same call; nothing is wrong.
    Transient,
    /// Device-lost / TDR class. The session must be torn down and rebuilt.
    Recoverable,
    /// Give up on AMF and fall through to the next encoder vendor.
    Fatal,
}

/// Classifies an `AMF_RESULT` for the submit/query loop.
///
/// `AMF_RESOLUTION_CHANGED`'s own header comment is "client needs to Drain/Terminate/Init", and
/// `AMF_NO_DEVICE` / `AMF_DIRECTX_FAILED` are the device-lost class — all three mean rebuild,
/// not abandon. `AMF_ENCODER_NOT_PRESENT`, `AMF_SURFACE_FORMAT_NOT_SUPPORTED` and
/// `AMF_INVALID_RESOLUTION` are configuration facts about this machine and never improve.
pub fn classify(r: AMF_RESULT) -> AmfErrorClass {
    match r {
        AMF_OK | AMF_REPEAT | AMF_INPUT_FULL | AMF_NEED_MORE_INPUT | AMF_EOF => {
            AmfErrorClass::Transient
        }
        AMF_RESOLUTION_CHANGED | AMF_RESOLUTION_UPDATED | AMF_NO_DEVICE | AMF_DIRECTX_FAILED
        | AMF_OUT_OF_MEMORY | AMF_WRONG_STATE => AmfErrorClass::Recoverable,
        _ => AmfErrorClass::Fatal,
    }
}

/// Turns a non-`AMF_OK` result into an `Err` carrying both the numeric code and its name.
pub fn check(r: AMF_RESULT, what: &str) -> Result<()> {
    if r == AMF_OK {
        return Ok(());
    }
    anyhow::bail!("{what} failed: {} ({r})", result_name(r))
}

/// Every AMF interface except `AMFFactory` and `AMFTrace` begins with the 13 `AMFPropertyStorage`
/// slots, so one set of property helpers drives components, surfaces, buffers and caps alike.
///
/// # Safety
/// `p` must be a live pointer to an AMF object whose vtable starts with the `AMFPropertyStorage`
/// prefix — that is, anything but `AMFFactory`, `AMFTrace` and `AMFPlane`.
pub unsafe fn as_property_storage<T>(p: *mut T) -> *mut AMFPropertyStorage {
    p.cast()
}

/// `AMFPropertyStorage::SetProperty`, vtable slot 3.
///
/// # Safety
/// `storage` must be a live property-storage-shaped AMF object; `name` must be NUL-terminated.
pub unsafe fn set_property(
    storage: *mut AMFPropertyStorage,
    name: &[u16],
    value: AMFVariantStruct,
) -> AMF_RESULT {
    debug_assert_eq!(name.last(), Some(&0), "property names must be NUL-terminated");
    // SAFETY: vtable slot 3 (AMFPropertyStorage::SetProperty). The variant is passed by value,
    // which on x64 Windows means by hidden reference to this stack copy — valid for the call.
    unsafe { ((*(*storage).pVtbl).SetProperty)(storage, name.as_ptr(), value) }
}

/// `AMFPropertyStorage::GetProperty`, vtable slot 4.
///
/// # Safety
/// As [`set_property`].
pub unsafe fn get_property(
    storage: *mut AMFPropertyStorage,
    name: &[u16],
) -> Result<AMFVariantStruct, AMF_RESULT> {
    let mut var = AMFVariantStruct::empty();
    // SAFETY: vtable slot 4 (AMFPropertyStorage::GetProperty); `var` is a live local.
    let r = unsafe { ((*(*storage).pVtbl).GetProperty)(storage, name.as_ptr(), &mut var) };
    if r == AMF_OK {
        Ok(var)
    } else {
        Err(r)
    }
}

/// `AMFPropertyStorage::HasProperty`, vtable slot 5 — the cheap "does this build of the runtime
/// know the property at all" probe, and the reason `GetPropertyInfo` is never called.
///
/// # Safety
/// As [`set_property`].
pub unsafe fn has_property(storage: *mut AMFPropertyStorage, name: &[u16]) -> bool {
    // SAFETY: vtable slot 5 (AMFPropertyStorage::HasProperty).
    unsafe { ((*(*storage).pVtbl).HasProperty)(storage, name.as_ptr()) != 0 }
}

/// `AMFInterface::Release`, vtable slot 1, on any interface-shaped object.
///
/// # Safety
/// `p` must be a live pointer to an AMF object whose vtable starts with the `AMFInterface`
/// prefix, and the caller must own the reference being dropped.
pub unsafe fn release<T>(p: *mut T) {
    if p.is_null() {
        return;
    }
    let iface: *mut AMFInterface = p.cast();
    // SAFETY: vtable slot 1 (AMFInterface::Release); null-checked above.
    unsafe {
        ((*(*iface).pVtbl).Release)(iface);
    }
}

/// `AMFInterface::QueryInterface`, vtable slot 2.
///
/// # Safety
/// `p` must be a live interface-shaped AMF object. On success the caller owns one reference to
/// the returned pointer and must `release` it.
pub unsafe fn query_interface<T, U>(p: *mut T, iid: &AMFGuid) -> Result<*mut U, AMF_RESULT> {
    let iface: *mut AMFInterface = p.cast();
    let mut out: *mut c_void = std::ptr::null_mut();
    // SAFETY: vtable slot 2 (AMFInterface::QueryInterface); `iid` and `out` are live locals.
    let r = unsafe { ((*(*iface).pVtbl).QueryInterface)(iface, iid, &mut out) };
    if r == AMF_OK && !out.is_null() {
        Ok(out.cast())
    } else if r == AMF_OK {
        Err(AMF_NO_INTERFACE)
    } else {
        Err(r)
    }
}

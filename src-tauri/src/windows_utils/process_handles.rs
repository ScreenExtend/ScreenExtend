//! What this process holds, by kernel object type, and what it has resident on a GPU: the two
//! things a session that does not tear down completely leaves behind. Read without elevation.

use std::collections::BTreeMap;
use std::ffi::c_void;

use windows::core::{s, w, Interface};
use windows::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory1, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
    DXGI_QUERY_VIDEO_MEMORY_INFO,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::GetCurrentProcess;

const PROCESS_HANDLE_INFORMATION: u32 = 51;
const OBJECT_NAME_INFORMATION: u32 = 1;
const OBJECT_TYPE_INFORMATION: u32 = 2;
const OBJECT_TYPES_INFORMATION: u32 = 3;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023_u32 as i32;

/// Types whose object name can be asked for without the query blocking. A file or pipe name query
/// can wait on the I/O the handle has pending, so anything not listed keeps an empty name.
const NAMEABLE: &[&str] = &[
    "Section",
    "Mutant",
    "Event",
    "Semaphore",
    "Timer",
    "Key",
    "Directory",
    "SymbolicLink",
];

/// `PROCESS_HANDLE_TABLE_ENTRY_INFO`.
#[repr(C)]
struct HandleEntry {
    handle: HANDLE,
    handle_count: usize,
    pointer_count: usize,
    access: u32,
    type_index: u32,
    attributes: u32,
    reserved: u32,
}

#[repr(C)]
pub(crate) struct UnicodeString {
    pub len: u16,
    pub max: u16,
    pub buf: *const u16,
}

impl UnicodeString {
    /// # Safety
    /// `buf` must point at `len` bytes of UTF-16 that outlive the call, or be null.
    pub(crate) unsafe fn to_string_lossy(&self) -> String {
        if self.buf.is_null() || self.len == 0 {
            return String::new();
        }
        // SAFETY: the caller's contract.
        String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(self.buf, self.len as usize / 2)
        })
    }
}

/// The fixed part of `OBJECT_TYPE_INFORMATION`; the type's name follows each record.
#[repr(C)]
struct TypeRecord {
    name: UnicodeString,
    counters: [u32; 12],
    invalid_attributes: u32,
    generic_mapping: [u32; 4],
    valid_access_mask: u32,
    security_required: u8,
    maintain_handle_count: u8,
    type_index: u8,
    reserved_byte: u8,
    pool_type: u32,
    default_paged_pool_charge: u32,
    default_non_paged_pool_charge: u32,
}

type NtQueryInformationProcess =
    unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;
type NtQueryObject = unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;

/// One handle this process holds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Held {
    pub kind: String,
    pub name: String,
    pub value: usize,
}

fn ntdll<T>(name: windows::core::PCSTR) -> Option<T> {
    // SAFETY: ntdll is mapped into every process; the caller names the exact signature.
    unsafe {
        let m = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let f = GetProcAddress(m, name)?;
        Some(std::mem::transmute_copy(&f))
    }
}

/// Calls `query` with a growing buffer until it fits. The buffer is `u64`s so every record in
/// it is aligned for pointer-sized fields.
fn grow(mut query: impl FnMut(*mut c_void, u32, &mut u32) -> i32) -> Option<Vec<u64>> {
    let mut buf = vec![0u64; 1 << 13];
    for _ in 0..12 {
        let mut need = 0u32;
        let st = query(buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut need);
        if st == STATUS_INFO_LENGTH_MISMATCH
            || st == STATUS_BUFFER_OVERFLOW
            || st == STATUS_BUFFER_TOO_SMALL
        {
            buf = vec![0u64; (buf.len() * 2).max(need as usize / 8 + 512)];
            continue;
        }
        return (st >= 0).then_some(buf);
    }
    None
}

/// Every object type the kernel knows, by the index a handle entry names it with. Read once from
/// the type table, so no handle has to be alive for its type to be known.
fn type_names(qo: NtQueryObject) -> Option<BTreeMap<u32, String>> {
    // SAFETY: a null handle with this class lists every type; the buffer is the stated size.
    let buf = grow(|p, len, need| unsafe {
        qo(HANDLE::default(), OBJECT_TYPES_INFORMATION, p, len, need)
    })?;
    let base = buf.as_ptr() as *const u8;
    let end = base as usize + buf.len() * 8;
    let align = std::mem::size_of::<usize>();
    // SAFETY: the buffer starts with the type count, then pointer-aligned records, each followed
    // by its name rounded up to pointer size; every read is checked against the buffer's end.
    unsafe {
        let count = *(base as *const u32) as usize;
        let mut p = base as usize + align;
        let mut out = BTreeMap::new();
        for _ in 0..count {
            if p + std::mem::size_of::<TypeRecord>() > end {
                return None;
            }
            let rec = &*(p as *const TypeRecord);
            out.insert(u32::from(rec.type_index), rec.name.to_string_lossy());
            let name_bytes = (rec.name.max as usize).next_multiple_of(align);
            p += std::mem::size_of::<TypeRecord>() + name_bytes;
        }
        Some(out)
    }
}

fn query_unicode(qo: NtQueryObject, h: HANDLE, class: u32) -> Option<String> {
    // SAFETY: a handle this process owns and a buffer of the stated size.
    let buf = grow(|p, len, need| unsafe { qo(h, class, p, len, need) })?;
    // SAFETY: both classes start with a UNICODE_STRING whose buffer lies inside `buf`.
    Some(unsafe { (*(buf.as_ptr() as *const UnicodeString)).to_string_lossy() })
}

/// The object name behind `value`, read through a duplicate so the value cannot be closed and
/// handed to an object of another type (a file, whose name query can block) while it is asked.
fn object_name(qo: NtQueryObject, value: HANDLE, kind: &str) -> String {
    if !NAMEABLE.contains(&kind) {
        return String::new();
    }
    let me = unsafe { GetCurrentProcess() };
    let mut dup = HANDLE::default();
    // SAFETY: duplicating within this process; a stale value just fails.
    if unsafe { DuplicateHandle(me, value, me, &mut dup, 0, false, DUPLICATE_SAME_ACCESS) }.is_err()
    {
        return String::new();
    }
    let same_kind = query_unicode(qo, dup, OBJECT_TYPE_INFORMATION).as_deref() == Some(kind);
    let name = if same_kind {
        query_unicode(qo, dup, OBJECT_NAME_INFORMATION).unwrap_or_default()
    } else {
        String::new()
    };
    // SAFETY: the duplicate is ours alone.
    let _ = unsafe { CloseHandle(dup) };
    name
}

/// Every handle this process holds, with its type and, for the types where asking cannot block,
/// its object name.
pub(crate) fn snapshot() -> Option<Vec<Held>> {
    let qip: NtQueryInformationProcess = ntdll(s!("NtQueryInformationProcess"))?;
    let qo: NtQueryObject = ntdll(s!("NtQueryObject"))?;
    let types = type_names(qo)?;
    let me = unsafe { GetCurrentProcess() };
    // SAFETY: this process's own handle table, into a buffer of the stated size.
    let buf = grow(|p, len, need| unsafe { qip(me, PROCESS_HANDLE_INFORMATION, p, len, need) })?;
    let count = buf[0] as usize;
    if 16 + count * std::mem::size_of::<HandleEntry>() > buf.len() * 8 {
        return None;
    }
    // SAFETY: the header is two pointer-sized fields, followed by `count` entries, checked above.
    let entries =
        unsafe { std::slice::from_raw_parts(buf.as_ptr().add(2) as *const HandleEntry, count) };
    let mut out = Vec::with_capacity(count);
    for e in entries {
        let kind = types
            .get(&e.type_index)
            .cloned()
            .unwrap_or_else(|| format!("type{}", e.type_index));
        let name = object_name(qo, e.handle, &kind);
        out.push(Held {
            kind,
            name,
            value: e.handle.0 as usize,
        });
    }
    Some(out)
}

/// Handle counts by type.
pub(crate) fn by_type(held: &[Held]) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    for h in held {
        *m.entry(h.kind.clone()).or_insert(0) += 1;
    }
    m
}

/// Splits the handles `after` holds and `before` did not into the extra handles to one of the
/// named objects `excused` lists (by `(kind, name suffix)`, counted by `kind name`), and
/// everything else, returned whole for comparison by type.
pub(crate) fn split_excused(
    before: &[Held],
    after: &[Held],
    excused: &[(&str, &str)],
) -> (BTreeMap<String, usize>, Vec<Held>) {
    let old: std::collections::HashSet<(&str, usize)> =
        before.iter().map(|h| (h.kind.as_str(), h.value)).collect();
    let mut matched = BTreeMap::new();
    let mut rest = Vec::new();
    for h in after {
        let new = !old.contains(&(h.kind.as_str(), h.value));
        let is_excused = excused
            .iter()
            .any(|(k, n)| h.kind == *k && h.name.ends_with(n));
        if new && is_excused {
            *matched.entry(format!("{} {}", h.kind, h.name)).or_insert(0) += 1;
        } else {
            rest.push(h.clone());
        }
    }
    (matched, rest)
}

/// Per type, how many more handles `after` holds than `before`. Only the types that moved.
pub(crate) fn growth(
    before: &BTreeMap<String, i64>,
    after: &BTreeMap<String, i64>,
) -> BTreeMap<String, i64> {
    let mut d = BTreeMap::new();
    for k in before.keys().chain(after.keys()) {
        let v = after.get(k).copied().unwrap_or(0) - before.get(k).copied().unwrap_or(0);
        if v != 0 {
            d.insert(k.clone(), v);
        }
    }
    d
}

/// Bytes of local (dedicated) memory this process has committed on each NVIDIA adapter, summed,
/// or `None` when there is no NVIDIA adapter or any query fails.
pub(crate) fn nvidia_dedicated_bytes() -> Option<u64> {
    // SAFETY: plain DXGI enumeration and a query on an adapter it returned.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0;
        let mut total = None;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let desc = adapter.GetDesc1().ok()?;
            if desc.VendorId != 0x10de {
                continue;
            }
            let a3: IDXGIAdapter3 = adapter.cast().ok()?;
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
                .ok()?;
            total = Some(total.unwrap_or(0) + info.CurrentUsage);
        }
        total
    }
}

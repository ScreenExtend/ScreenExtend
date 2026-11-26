//! Encoder recipes: a whole VideoToolbox configuration expressed as data, used by the production
//! encoder as an optional override (`SE_VT_RECIPE=<file.json>`) and by the `bench` harness.
//!
//! Keys are short names resolved at runtime with `dlsym` under their `kVT...` prefix (`props` are
//! applied in order); a name starting with `kVT`/`kCV`/`kCM` is looked up verbatim, and
//! `lit:<text>` is the key string itself. A symbol that does not resolve is the outcome
//! `missing_symbol`, never an error: on an old macOS that is the capability probe. Nothing here
//! may link a symbol newer than the 10.15 floor.

use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr;
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2_core_foundation::{
    kCFAllocatorDefault, kCFTypeArrayCallBacks, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks, CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained,
    CFString, CFType,
};
use objc2_core_media::CMSampleBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// One property value. JSON spelling: `null`, `true`, `60`, `600.0`, `"literal string"`,
/// `{"sym": "kVTProfileLevel_H264_High_AutoLevel"}` (or a list of symbols, the first that
/// resolves wins), and `[a, b]` for a CFArray such as `DataRateLimits`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Val {
    /// Set the property to NULL (VideoToolbox's "back to default" for most keys).
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Sym {
        sym: SymRef,
    },
    List(Vec<Val>),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SymRef {
    One(String),
    /// Alternatives in order; the first symbol that resolves is used and recorded.
    Any(Vec<String>),
}

impl SymRef {
    fn names(&self) -> Vec<&str> {
        match self {
            SymRef::One(s) => vec![s.as_str()],
            SymRef::Any(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    #[default]
    H264,
    Hevc,
}

impl Codec {
    /// The `CMVideoCodecType` FourCC.
    pub fn fourcc(self) -> u32 {
        match self {
            Codec::H264 => 0x6176_6331, // 'avc1'
            Codec::Hevc => 0x6876_6331, // 'hvc1'
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Recipe {
    pub name: String,
    #[serde(default)]
    pub codec: Codec,
    #[serde(default)]
    pub spec: Vec<(String, Val)>,
    #[serde(default)]
    pub props: Vec<(String, Val)>,
    #[serde(default)]
    pub frame_opts: Vec<(String, Val)>,
    /// Call `VTCompressionSessionCompleteFrames` after every submit (a synchronous encoder).
    #[serde(default)]
    pub flush_each_frame: bool,
}

impl Recipe {
    pub fn from_json(text: &str) -> Result<Recipe, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    /// Whether the recipe itself pins the rate control, in which case the production bitrate
    /// controller must leave `AverageBitRate` alone.
    pub fn pins_rate(&self) -> bool {
        const RATE_KEYS: [&str; 7] = [
            "AverageBitRate",
            "ConstantBitRate",
            "VariableBitRate",
            "DataRateLimits",
            "MaxAllowedFrameQP",
            "MinAllowedFrameQP",
            "Quality",
        ];
        self.props
            .iter()
            .any(|(k, _)| RATE_KEYS.contains(&k.as_str()))
    }

    pub fn spec_flag(&self, name: &str) -> bool {
        self.spec
            .iter()
            .any(|(k, v)| k == name && *v == Val::Bool(true))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Prop,
    Spec,
    FrameOpt,
}

impl KeyKind {
    fn prefix(self) -> &'static str {
        match self {
            KeyKind::Prop => "kVTCompressionPropertyKey_",
            KeyKind::Spec => "kVTVideoEncoderSpecification_",
            KeyKind::FrameOpt => "kVTEncodeFrameOptionKey_",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// `VTSessionSetProperty` returned 0.
    Set,
    /// `VTSessionSetProperty` returned non-zero (see `status`).
    Rejected,
    /// The key's symbol does not exist in this process.
    MissingSymbol,
    /// A `{"sym": ...}` value did not resolve, so nothing was set.
    MissingValueSymbol,
    /// Put into a specification / frame-options dictionary (no per-key status exists there).
    Included,
    /// Read-only query.
    Read,
}

/// What one key did. The read-back is what shows a set that was accepted but not applied.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PropResult {
    pub key: String,
    pub value: Val,
    /// The symbol (or `literal`) the key resolved through; `None` when it did not resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// For a `sym` value: the alternative that resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_symbol: Option<String>,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    /// Listed in the session's supported-property dictionary. `None` when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advertised: Option<bool>,
    /// Value read back before the set (only where asked for).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Json>,
    /// Value read back after the set; `null` when the copy succeeded with no value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readback: Option<Json>,
    /// `VTSessionCopyProperty` status of the read-back when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readback_status: Option<i32>,
}

impl PropResult {
    fn new(key: &str, value: &Val, outcome: Outcome) -> PropResult {
        PropResult {
            key: key.to_string(),
            value: value.clone(),
            symbol: None,
            value_symbol: None,
            outcome,
            status: None,
            advertised: None,
            before: None,
            readback: None,
            readback_status: None,
        }
    }

    /// A short token for one-line logs: `ok`, `rejected(-12900)`, `no-symbol`...
    pub fn short(&self) -> String {
        match self.outcome {
            Outcome::Set => "ok".into(),
            Outcome::Rejected => format!("rejected({})", self.status.unwrap_or(0)),
            Outcome::MissingSymbol => "no-symbol".into(),
            Outcome::MissingValueSymbol => "no-value-symbol".into(),
            Outcome::Included => "included".into(),
            Outcome::Read => "read".into(),
        }
    }
}

pub fn dlsym_ptr(symbol: &str) -> Option<*mut c_void> {
    let cname = std::ffi::CString::new(symbol).ok()?;
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr()) };
    (!p.is_null()).then_some(p)
}

/// A `CFStringRef` constant exported under `symbol`, if this process has it.
pub fn cfstring_symbol(symbol: &str) -> Option<&'static CFString> {
    let p = dlsym_ptr(symbol)?;
    unsafe { (*(p as *const *const CFString)).as_ref() }
}

/// Resolve a short key name to its CFString. `Err` carries the symbol that was tried.
///
/// A key named in `SE_VT_DENY` (see [`Deny`]) resolves as if its symbol were missing.
pub fn resolve_key(kind: KeyKind, name: &str) -> Result<(CFRetained<CFString>, String), String> {
    if denied(kind, name) {
        return Err(format!(
            "{}{} (denied by SE_VT_DENY)",
            kind.prefix(),
            short_name(kind, name)
        ));
    }
    if let Some(lit) = name.strip_prefix("lit:") {
        return Ok((CFString::from_str(lit), "literal".into()));
    }
    let symbol = if name.starts_with("kVT") || name.starts_with("kCV") || name.starts_with("kCM") {
        name.to_string()
    } else {
        format!("{}{name}", kind.prefix())
    };
    match cfstring_symbol(&symbol) {
        Some(s) => Ok((unsafe { CFRetained::retain(NonNull::from(s)) }, symbol)),
        None => Err(symbol),
    }
}

/// What `SE_VT_DENY` hides from this process, so every fallback rung can be driven on a Mac that
/// has the capability the rung exists for.
///
/// `SE_VT_DENY="prop:AverageBitRate,frame:BaseFrameQP,spec:EnableLowLatencyRateControl,
/// read:SupportsBaseFrameQP,create:lowlatency"`:
///
/// | item | effect |
/// |---|---|
/// | `prop:<key>` | the property's symbol resolves as missing (`missing_symbol`); a linked-constant set of it returns `-12900` (`kVTPropertyNotSupportedErr`); it is left out of every supported-property set |
/// | `frame:<key>` | the per-frame option's symbol resolves as missing, so it is never put in the frame dictionary |
/// | `spec:<key>` | the specification key resolves as missing; `spec:EnableLowLatencyRateControl` removes the production ladder's low-latency rung entirely |
/// | `read:<key>` | `VTSessionCopyProperty` of it returns `-12900` |
/// | `create:lowlatency` | creating a low-latency session (the ladder's first rung, or a recipe whose spec sets `EnableLowLatencyRateControl`) fails with `-12902` without calling VideoToolbox |
///
/// Read once per process and honoured only with `SE_TEST_HOOKS=1` beside it; a lone `SE_VT_DENY`
/// is ignored with a log line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Deny {
    pub props: HashSet<String>,
    pub frame: HashSet<String>,
    pub spec: HashSet<String>,
    pub reads: HashSet<String>,
    pub create_low_latency: bool,
}

/// `kVTPropertyNotSupportedErr`.
pub const DENIED_STATUS: i32 = -12900;
/// `kVTCouldNotFindVideoEncoderErr`.
pub const DENIED_CREATE_STATUS: i32 = -12902;

/// A key's bare name (no `lit:`, no `kVT...Key_` prefix): the string VideoToolbox itself uses.
fn short_name(kind: KeyKind, name: &str) -> String {
    let n = name.strip_prefix("lit:").unwrap_or(name);
    n.strip_prefix(kind.prefix()).unwrap_or(n).to_string()
}

impl Deny {
    pub fn parse(text: &str) -> Result<Deny, String> {
        let mut d = Deny::default();
        for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (kind, name) = item
                .split_once(':')
                .ok_or_else(|| format!("'{item}': expected <kind>:<name>"))?;
            let name = name.trim();
            if name.is_empty() {
                return Err(format!("'{item}': empty name"));
            }
            match kind.trim() {
                "prop" => {
                    d.props.insert(short_name(KeyKind::Prop, name));
                }
                "frame" => {
                    d.frame.insert(short_name(KeyKind::FrameOpt, name));
                }
                "spec" => {
                    d.spec.insert(short_name(KeyKind::Spec, name));
                }
                "read" => {
                    d.reads.insert(short_name(KeyKind::Prop, name));
                }
                "create" if name == "lowlatency" => d.create_low_latency = true,
                "create" => {
                    return Err(format!(
                        "'{item}': the only create item is create:lowlatency"
                    ))
                }
                other => {
                    return Err(format!(
                        "'{item}': unknown kind '{other}' (prop | frame | spec | read | create)"
                    ))
                }
            }
        }
        Ok(d)
    }

    pub fn is_empty(&self) -> bool {
        *self == Deny::default()
    }

    /// The items, sorted, in the same spelling `parse` takes.
    pub fn items(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for (kind, set) in [
            ("prop", &self.props),
            ("frame", &self.frame),
            ("spec", &self.spec),
            ("read", &self.reads),
        ] {
            let mut names: Vec<&String> = set.iter().collect();
            names.sort();
            v.extend(names.into_iter().map(|n| format!("{kind}:{n}")));
        }
        if self.create_low_latency {
            v.push("create:lowlatency".into());
        }
        v
    }

    fn set(&self, kind: KeyKind) -> &HashSet<String> {
        match kind {
            KeyKind::Prop => &self.props,
            KeyKind::Spec => &self.spec,
            KeyKind::FrameOpt => &self.frame,
        }
    }
}

/// `SE_TEST_HOOKS=1`: the one switch every developer-only encoder hook needs beside its own
/// variable (`SE_VT_DENY`, `SE_VT_RECIPE`, `SE_VT_QP_BPS`, `SE_VT_SIM_HOLD`). Read once.
pub fn test_hooks() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::envcfg::var("SE_TEST_HOOKS").as_deref() == Ok("1"))
}

/// A developer-only variable, honoured only with `SE_TEST_HOOKS=1`. A lone one is ignored with
/// one log line (callers read each variable once, so the line is printed at most once).
pub fn hook_var(name: &str) -> Option<String> {
    let v = crate::envcfg::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())?;
    if !test_hooks() {
        teprintln!(
            "[vt] {name} is set but IGNORED: it is a developer hook and is honoured only together \
             with SE_TEST_HOOKS=1"
        );
        return None;
    }
    Some(v)
}

/// The process's deny list, read once. Empty unless both `SE_VT_DENY` and `SE_TEST_HOOKS=1`.
pub fn deny() -> &'static Deny {
    static DENY: OnceLock<Deny> = OnceLock::new();
    DENY.get_or_init(|| {
        let Some(text) = crate::envcfg::var("SE_VT_DENY")
            .ok()
            .filter(|t| !t.trim().is_empty())
        else {
            return Deny::default();
        };
        if !test_hooks() {
            teprintln!(
                "[vt] SE_VT_DENY is set but IGNORED: it is a test hook and is honoured only \
                 together with SE_TEST_HOOKS=1"
            );
            return Deny::default();
        }
        match Deny::parse(&text) {
            Ok(d) => {
                teprintln!(
                    "[vt] !!!!! SE_VT_DENY TEST HOOK ACTIVE !!!!! VideoToolbox capabilities are \
                     being hidden on purpose: {}. Nothing this process measures describes this \
                     Mac's real behaviour.",
                    d.items().join(",")
                );
                d
            }
            Err(e) => {
                teprintln!("[vt] SE_VT_DENY={text} ignored: {e}");
                Deny::default()
            }
        }
    })
}

/// Whether `SE_VT_DENY` hides this key.
pub fn denied(kind: KeyKind, name: &str) -> bool {
    let d = deny();
    !d.is_empty() && d.set(kind).contains(&short_name(kind, name))
}

/// Whether a property set through a linked constant must be refused: compares the key's own
/// string, which is what VideoToolbox uses for every documented key.
pub fn prop_key_denied(key: &CFString) -> bool {
    let d = deny();
    !d.is_empty() && d.props.contains(&key.to_string())
}

/// Whether `SE_VT_DENY read:<key>` refuses a copy of this property.
pub fn copy_raw_denied(key: &CFString) -> bool {
    read_denied(key)
}

fn read_denied(key: &CFString) -> bool {
    let d = deny();
    !d.is_empty() && d.reads.contains(&key.to_string())
}

fn upcast<T: objc2_core_foundation::Type>(v: CFRetained<T>) -> CFRetained<CFType> {
    unsafe { CFRetained::cast_unchecked::<CFType>(v) }
}

/// Build the CF value. `Ok(None)` is NULL; `Err` names a `sym` that did not resolve.
/// The second element is the symbol a `sym` resolved to.
pub fn build_value(v: &Val) -> Result<(Option<CFRetained<CFType>>, Option<String>), String> {
    Ok(match v {
        Val::Null => (None, None),
        Val::Bool(b) => (
            Some(upcast(unsafe {
                CFRetained::retain(NonNull::from(CFBoolean::new(*b)))
            })),
            None,
        ),
        Val::Int(i) => {
            // i32 when it fits: that is how the production encoder passes numbers
            let n = match i32::try_from(*i) {
                Ok(small) => CFNumber::new_i32(small),
                Err(_) => CFNumber::new_i64(*i),
            };
            (Some(upcast(n)), None)
        }
        Val::Float(f) => (Some(upcast(CFNumber::new_f64(*f))), None),
        Val::Str(s) => (Some(upcast(CFString::from_str(s))), None),
        Val::Sym { sym } => {
            let names = sym.names();
            let found = names
                .iter()
                .find_map(|n| cfstring_symbol(n).map(|s| (s, n.to_string())));
            match found {
                Some((s, n)) => (
                    Some(upcast(unsafe { CFRetained::retain(NonNull::from(s)) })),
                    Some(n),
                ),
                None => return Err(names.join("|")),
            }
        }
        Val::List(items) => {
            let mut built = Vec::with_capacity(items.len());
            for it in items {
                if let (Some(x), _) = build_value(it)? {
                    built.push(x);
                }
            }
            let mut raw: Vec<*const c_void> = built
                .iter()
                .map(|x| CFRetained::as_ptr(x).as_ptr().cast_const().cast())
                .collect();
            let arr = unsafe {
                CFArray::new(
                    None,
                    raw.as_mut_ptr(),
                    raw.len() as isize,
                    &kCFTypeArrayCallBacks,
                )
            };
            (arr.map(upcast), None)
        }
    })
}

unsafe extern "C" {
    fn VTSessionCopyProperty(
        session: &VTSession,
        property_key: &CFString,
        allocator: *const c_void,
        property_value_out: *mut *const CFType,
    ) -> i32;
    fn VTSessionCopySupportedPropertyDictionary(
        session: &VTSession,
        out: *mut *const CFDictionary,
    ) -> i32;
    fn CFCopyDescription(cf: *const c_void) -> *const CFString;
}

/// Copy a property. `Ok(None)` = the copy succeeded and the value is NULL.
pub fn copy_raw(session: &VTSession, key: &CFString) -> Result<Option<CFRetained<CFType>>, i32> {
    if read_denied(key) {
        return Err(DENIED_STATUS);
    }
    let mut value: *const CFType = ptr::null();
    let st = unsafe { VTSessionCopyProperty(session, key, ptr::null(), &mut value) };
    if st != 0 {
        return Err(st);
    }
    Ok(NonNull::new(value as *mut CFType).map(|p| unsafe { CFRetained::from_raw(p) }))
}

/// Any CF value as JSON. Unknown types become their `CFCopyDescription`.
pub fn cf_to_json(v: &CFType) -> Json {
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Json::Bool(b.as_bool());
    }
    if let Some(n) = v.downcast_ref::<CFNumber>() {
        if n.is_float_type() {
            return n
                .as_f64()
                .and_then(serde_json::Number::from_f64)
                .map(Json::Number)
                .unwrap_or(Json::Null);
        }
        return n.as_i64().map(Json::from).unwrap_or(Json::Null);
    }
    if let Some(s) = v.downcast_ref::<CFString>() {
        return Json::String(s.to_string());
    }
    if let Some(a) = v.downcast_ref::<CFArray>() {
        let mut out = Vec::new();
        for i in 0..a.count() {
            let item = unsafe { a.value_at_index(i) } as *const CFType;
            match unsafe { item.as_ref() } {
                Some(t) => out.push(cf_to_json(t)),
                None => out.push(Json::Null),
            }
        }
        return Json::Array(out);
    }
    if let Some(d) = v.downcast_ref::<CFDictionary>() {
        let mut map = serde_json::Map::new();
        for (k, val) in dictionary_entries(d) {
            map.insert(k, cf_to_json(val));
        }
        return Json::Object(map);
    }
    let desc = unsafe { CFCopyDescription((v as *const CFType).cast()) };
    match NonNull::new(desc as *mut CFString) {
        Some(p) => Json::String(unsafe { CFRetained::from_raw(p) }.to_string()),
        None => Json::Null,
    }
}

/// A dictionary's entries, keys stringified (non-string keys through their description).
pub fn dictionary_entries(dict: &CFDictionary) -> Vec<(String, &CFType)> {
    let count = dict.count();
    if count <= 0 {
        return Vec::new();
    }
    let mut keys: Vec<*const c_void> = vec![ptr::null(); count as usize];
    let mut vals: Vec<*const c_void> = vec![ptr::null(); count as usize];
    unsafe { dict.keys_and_values(keys.as_mut_ptr(), vals.as_mut_ptr()) };
    let mut out = Vec::with_capacity(count as usize);
    for (k, v) in keys.into_iter().zip(vals) {
        let (Some(k), Some(v)) = (unsafe { (k as *const CFType).as_ref() }, unsafe {
            (v as *const CFType).as_ref()
        }) else {
            continue;
        };
        let name = match cf_to_json(k) {
            Json::String(s) => s,
            other => other.to_string(),
        };
        out.push((name, v));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The session's own supported-property dictionary (`VTSessionCopySupportedPropertyDictionary`,
/// 10.8): what *this* encoder instance says it takes.
pub fn supported_dictionary(session: &VTSession) -> Result<CFRetained<CFDictionary>, i32> {
    let mut out: *const CFDictionary = ptr::null();
    let st = unsafe { VTSessionCopySupportedPropertyDictionary(session, &mut out) };
    if st != 0 {
        return Err(st);
    }
    NonNull::new(out as *mut CFDictionary)
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or(0)
}

pub fn supported_keys(session: &VTSession) -> Option<HashSet<String>> {
    let dict = supported_dictionary(session).ok()?;
    let denied = &deny().props;
    Some(
        dictionary_entries(&dict)
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| !denied.contains(k))
            .collect(),
    )
}

/// Set one property and report everything about it.
pub fn set_prop(
    session: &VTSession,
    name: &str,
    val: &Val,
    advertised: Option<&HashSet<String>>,
    read_before: bool,
) -> PropResult {
    let mut r = PropResult::new(name, val, Outcome::MissingSymbol);
    let key = match resolve_key(KeyKind::Prop, name) {
        Ok((k, sym)) => {
            r.symbol = Some(sym);
            k
        }
        Err(_) => return r,
    };
    r.advertised = advertised.map(|set| set.contains(&key.to_string()));
    if read_before {
        r.before = Some(match copy_raw(session, &key) {
            Ok(Some(v)) => cf_to_json(&v),
            Ok(None) => Json::Null,
            Err(st) => serde_json::json!({ "copy_status": st }),
        });
    }
    let (value, value_symbol) = match build_value(val) {
        Ok(v) => v,
        Err(_) => {
            r.outcome = Outcome::MissingValueSymbol;
            return r;
        }
    };
    r.value_symbol = value_symbol;
    let st = unsafe { VTSessionSetProperty(session, &key, value.as_deref()) };
    r.status = Some(st);
    r.outcome = if st == 0 {
        Outcome::Set
    } else {
        Outcome::Rejected
    };
    match copy_raw(session, &key) {
        Ok(Some(v)) => r.readback = Some(cf_to_json(&v)),
        Ok(None) => r.readback = Some(Json::Null),
        Err(st) => r.readback_status = Some(st),
    }
    r
}

/// Read one property (read-only keys such as `NumberOfPendingFrames`).
pub fn read_prop(session: &VTSession, name: &str) -> PropResult {
    let mut r = PropResult::new(name, &Val::Null, Outcome::MissingSymbol);
    let Ok((key, sym)) = resolve_key(KeyKind::Prop, name) else {
        return r;
    };
    r.symbol = Some(sym);
    r.outcome = Outcome::Read;
    match copy_raw(session, &key) {
        Ok(Some(v)) => r.readback = Some(cf_to_json(&v)),
        Ok(None) => r.readback = Some(Json::Null),
        Err(st) => r.readback_status = Some(st),
    }
    r
}

/// Build a specification or frame-options dictionary. Keys or values that do not resolve are left
/// out and reported; `extra` pairs (already resolved) are appended.
pub fn build_dict(
    kind: KeyKind,
    pairs: &[(String, Val)],
    extra: &[(&CFString, &CFType)],
) -> (Option<CFRetained<CFDictionary>>, Vec<PropResult>) {
    let mut report = Vec::with_capacity(pairs.len());
    let mut keys: Vec<CFRetained<CFString>> = Vec::new();
    let mut vals: Vec<CFRetained<CFType>> = Vec::new();
    for (name, val) in pairs {
        let mut r = PropResult::new(name, val, Outcome::MissingSymbol);
        if let Ok((k, sym)) = resolve_key(kind, name) {
            r.symbol = Some(sym);
            match build_value(val) {
                Ok((Some(v), vs)) => {
                    r.value_symbol = vs;
                    r.outcome = Outcome::Included;
                    keys.push(k);
                    vals.push(v);
                }
                // NULL in a dictionary means "leave the key out"
                Ok((None, _)) => r.outcome = Outcome::Included,
                Err(_) => r.outcome = Outcome::MissingValueSymbol,
            }
        }
        report.push(r);
    }
    let mut kp: Vec<*const c_void> = keys
        .iter()
        .map(|k| CFRetained::as_ptr(k).as_ptr().cast_const().cast())
        .collect();
    let mut vp: Vec<*const c_void> = vals
        .iter()
        .map(|v| CFRetained::as_ptr(v).as_ptr().cast_const().cast())
        .collect();
    for (k, v) in extra {
        kp.push((*k as *const CFString).cast());
        vp.push((*v as *const CFType).cast());
    }
    if kp.is_empty() {
        return (None, report);
    }
    let dict = unsafe {
        CFDictionary::new(
            kCFAllocatorDefault,
            kp.as_mut_ptr(),
            vp.as_mut_ptr(),
            kp.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    };
    (dict, report)
}

pub type OutputCallback = unsafe extern "C-unwind" fn(
    *mut c_void,
    *mut c_void,
    i32,
    VTEncodeInfoFlags,
    *mut CMSampleBuffer,
);

/// `VTCompressionSessionCreate` for any codec.
pub fn create_session(
    width: u32,
    height: u32,
    codec: u32,
    spec: Option<&CFDictionary>,
    callback: OutputCallback,
    refcon: *mut c_void,
) -> Result<CFRetained<VTCompressionSession>, i32> {
    let mut session_ptr: *mut VTCompressionSession = ptr::null_mut();
    let status = unsafe {
        VTCompressionSession::create(
            kCFAllocatorDefault,
            width as i32,
            height as i32,
            codec,
            spec,
            None,
            None,
            Some(callback),
            refcon,
            NonNull::from(&mut session_ptr),
        )
    };
    match NonNull::new(session_ptr) {
        Some(p) if status == 0 => Ok(unsafe { CFRetained::from_raw(p) }),
        Some(p) => {
            // a session handed back with a failure status is still ours to release
            drop(unsafe { CFRetained::from_raw(p) });
            Err(status)
        }
        None => Err(if status == 0 { -1 } else { status }),
    }
}

pub fn as_vt_session(s: &VTCompressionSession) -> &VTSession {
    unsafe { &*(s as *const VTCompressionSession as *const VTSession) }
}

unsafe extern "C-unwind" fn discard_output(
    _refcon: *mut c_void,
    _frame: *mut c_void,
    _status: i32,
    _info: VTEncodeInfoFlags,
    _sample: *mut CMSampleBuffer,
) {
}

/// A bare session for probing: created, configured, read, invalidated. Never encodes.
pub struct ProbeSession {
    session: CFRetained<VTCompressionSession>,
}

impl ProbeSession {
    pub fn create(
        width: u32,
        height: u32,
        codec: Codec,
        spec: &[(String, Val)],
    ) -> (Result<ProbeSession, i32>, Vec<PropResult>) {
        let (dict, report) = build_dict(KeyKind::Spec, spec, &[]);
        let r = create_session(
            width,
            height,
            codec.fourcc(),
            dict.as_deref(),
            discard_output,
            ptr::null_mut(),
        )
        .map(|session| ProbeSession { session });
        (r, report)
    }

    pub fn vt(&self) -> &VTSession {
        as_vt_session(&self.session)
    }
}

impl Drop for ProbeSession {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_through_json() {
        let text = r#"{"name":"x","codec":"hevc","spec":[["EnableLowLatencyRateControl",true]],
            "props":[["ExpectedFrameRate",60],["MaxKeyFrameIntervalDuration",600.0],
                     ["ColorPrimaries","ITU_R_709_2"],
                     ["ProfileLevel",{"sym":["kA","kB"]}],["DataRateLimits",[1500000,1.0]],
                     ["AverageBitRate",null]]}"#;
        let r = Recipe::from_json(text).expect("parses");
        assert_eq!(r.codec, Codec::Hevc);
        assert_eq!(r.props[0].1, Val::Int(60));
        assert_eq!(r.props[1].1, Val::Float(600.0));
        assert_eq!(r.props[2].1, Val::Str("ITU_R_709_2".into()));
        assert!(matches!(
            r.props[3].1,
            Val::Sym {
                sym: SymRef::Any(_)
            }
        ));
        assert_eq!(
            r.props[4].1,
            Val::List(vec![Val::Int(1_500_000), Val::Float(1.0)])
        );
        assert_eq!(r.props[5].1, Val::Null);
        let again: Recipe = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(again, r);
        assert!(r.pins_rate());
        assert!(r.spec_flag("EnableLowLatencyRateControl"));
    }

    #[test]
    fn the_deny_list_parses_every_kind_and_normalises_names() {
        let d = Deny::parse(
            " prop:AverageBitRate, frame:kVTEncodeFrameOptionKey_BaseFrameQP,\
             spec:EnableLowLatencyRateControl,read:lit:SupportsBaseFrameQP,create:lowlatency ,",
        )
        .expect("parses");
        assert!(d.props.contains("AverageBitRate"));
        assert!(
            d.frame.contains("BaseFrameQP"),
            "the symbol prefix is stripped"
        );
        assert!(d.spec.contains("EnableLowLatencyRateControl"));
        assert!(d.reads.contains("SupportsBaseFrameQP"), "lit: is stripped");
        assert!(d.create_low_latency);
        assert_eq!(
            d.items(),
            [
                "prop:AverageBitRate",
                "frame:BaseFrameQP",
                "spec:EnableLowLatencyRateControl",
                "read:SupportsBaseFrameQP",
                "create:lowlatency"
            ]
        );
        assert!(Deny::parse("").unwrap().is_empty());
        assert!(Deny::parse("AverageBitRate").is_err(), "a kind is required");
        assert!(Deny::parse("prop:").is_err());
        assert!(Deny::parse("create:hardware").is_err());
        assert!(Deny::parse("propx:A").is_err());
    }

    #[test]
    fn an_unknown_symbol_is_an_outcome_not_an_error() {
        assert!(resolve_key(KeyKind::Prop, "DefinitelyNotAKey").is_err());
        assert!(resolve_key(KeyKind::Prop, "lit:Anything").is_ok());
        let (dict, report) = build_dict(
            KeyKind::FrameOpt,
            &[("DefinitelyNotAKey".into(), Val::Bool(true))],
            &[],
        );
        assert!(dict.is_none());
        assert_eq!(report[0].outcome, Outcome::MissingSymbol);
    }
}

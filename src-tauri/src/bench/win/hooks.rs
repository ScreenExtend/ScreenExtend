//! Recipe overrides for the production encoders, applied from inside their init paths.
//!
//! Each encoder calls one `#[cfg(feature = "bench")]` hook here right before its session opens.
//! Nothing happens unless the lab has armed an override set for that vendor. Field names are the
//! vendor SDK's own structure member names; an unknown name fails the recipe.

// union members (the codec config, `mfxInfoMFX`) need `unsafe` to read and plain members do
// not; the field macros use one form for both
#![allow(unused_unsafe)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Value as Json};

use super::recipe::Val;
use crate::streamer::lock::LockExt;
use crate::windows_utils::streamer::intel::intel_sys::{
    mfxExtBuffer, mfxExtCodingOption, mfxExtCodingOption2, mfxExtCodingOption3, mfxSession,
    mfxStatus, mfxVideoParam, MFX_EXTBUFF_CODING_OPTION, MFX_EXTBUFF_CODING_OPTION2,
    MFX_EXTBUFF_CODING_OPTION3,
};
use crate::windows_utils::streamer::nvidia::nvenc_sys::{
    GUID, NVENCSTATUS, NV_ENCODE_API_FUNCTION_LIST, NV_ENC_CONFIG, NV_ENC_CONFIG_VER,
    NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE, NV_ENC_H264_ENTROPY_CODING_MODE, NV_ENC_INITIALIZE_PARAMS,
    NV_ENC_MULTI_PASS, NV_ENC_NUM_REF_FRAMES, NV_ENC_PARAMS_RC_MODE, NV_ENC_PRESET_CONFIG,
    NV_ENC_PRESET_CONFIG_VER, NV_ENC_TUNING_INFO,
};
use crate::windows_utils::streamer::x264::x264_sys::x264_param_t;

/// Which encoder an armed override set is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Nvenc,
    Qsv,
    X264,
}

impl Vendor {
    pub fn parse(s: &str) -> Option<Vendor> {
        match s {
            "nvenc" => Some(Vendor::Nvenc),
            "qsv" => Some(Vendor::Qsv),
            "x264" => Some(Vendor::X264),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Not applied yet (the hook did not fire, e.g. the session failed before reaching it).
    Pending,
    Applied,
    Unknown,
    Rejected,
    Changed,
}

impl Outcome {
    /// Whether the hook (re)applies the entry. A vendor whose first session fails can open a
    /// fallback session through the same init path, and that one gets the overrides too.
    fn applies(self) -> bool {
        matches!(self, Outcome::Pending | Outcome::Applied | Outcome::Changed)
    }
}

/// What became of one `set` entry.
#[derive(Clone, Debug, Serialize)]
pub struct FieldReport {
    pub field: String,
    pub value: Val,
    pub outcome: Outcome,
    /// The value the API reports after the session opened (Quick Sync `GetVideoParam`, x264
    /// `x264_encoder_parameters`), or the value in the structure handed to
    /// `nvEncInitializeEncoder` (NVENC has no configuration read-back).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readback: Option<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

type GetVideoParamFn = unsafe extern "C" fn(mfxSession, *mut mfxVideoParam) -> mfxStatus;
type X264ParseFn = unsafe extern "C" fn(*mut x264_param_t, *const c_char, *const c_char) -> c_int;

struct Armed {
    vendor: Vendor,
    report: Vec<FieldReport>,
    /// The parsed integer per entry (NVENC / Quick Sync), in `report` order.
    ints: Vec<i64>,
    fired: u32,
    notes: Vec<String>,
    qsv_session: Option<(usize, GetVideoParamFn)>,
    x264_parse: Option<X264ParseFn>,
    /// Keeps libx264 loaded for `x264_parse`.
    _x264_lib: Option<libloading::Library>,
}

// SAFETY: the raw session pointer is only an integer here; it is dereferenced by the runtime,
// on the lab's own thread, while the encoder that owns it is alive.
unsafe impl Send for Armed {}

static ARMED: Mutex<Option<Armed>> = Mutex::new(None);

/// The outcome of a validated recipe: what will be applied, or why it cannot be.
pub struct Validated {
    pub report: Vec<FieldReport>,
    ints: Vec<i64>,
    x264: Option<(libloading::Library, X264ParseFn)>,
}

impl Validated {
    /// True when every entry is applicable (no `unknown`, no `rejected`).
    pub fn ok(&self) -> bool {
        self.report
            .iter()
            .all(|r| r.outcome == Outcome::Pending || r.outcome == Outcome::Applied)
    }
}

/// Fields that are run axes or are handled outside the hook (the hook only reads them).
fn nvenc_special(name: &str) -> Option<&'static str> {
    match name {
        "enableEncodeAsync" => Some(
            "applied through production's SE_NVENC_NO_ASYNC switch before the session opens \
             (production decides async mode before init and sizes its output ring from it); the \
             hook only reads the value it reached",
        ),
        _ => None,
    }
}

/// Check every name and value before any encoder is built. `x264_lib` is the library the
/// production x264 loader would open, needed to ask `x264_param_parse` about a name.
pub fn validate(
    vendor: Vendor,
    set: &[(String, Val)],
    x264_lib: Option<&std::path::Path>,
) -> Validated {
    let mut report = Vec::with_capacity(set.len());
    let mut ints = Vec::with_capacity(set.len());
    let mut x264 = None;
    if vendor == Vendor::X264 && !set.is_empty() {
        x264 = x264_lib.and_then(|p| load_x264_parse(p).ok());
    }
    for (name, value) in set {
        let mut r = FieldReport {
            field: name.clone(),
            value: value.clone(),
            outcome: Outcome::Pending,
            readback: None,
            note: None,
        };
        let mut int = 0i64;
        match vendor {
            Vendor::Nvenc => match nvenc_field(name) {
                None => {
                    r.outcome = Outcome::Unknown;
                    r.note = Some(match refused_nvenc(name) {
                        Some(why) => why.to_string(),
                        None => "no such NVENC field (names follow NV_ENC_INITIALIZE_PARAMS / \
                                 NV_ENC_CONFIG / NV_ENC_RC_PARAMS (rc.) / NV_ENC_CONFIG_H264 \
                                 (h264.))"
                            .into(),
                    });
                }
                Some(f) => match f.parse(value) {
                    Ok(v) => int = v,
                    Err(e) => {
                        r.outcome = Outcome::Rejected;
                        r.note = Some(e);
                    }
                },
            },
            Vendor::Qsv => match qsv_field(name) {
                None => {
                    r.outcome = Outcome::Unknown;
                    r.note = Some(
                        "no such Quick Sync field (names: AsyncDepth, mfx.<mfxInfoMFX member>, \
                         co./co2./co3.<mfxExtCodingOption/2/3 member>)"
                            .into(),
                    );
                }
                Some(f) => match f.parse(value) {
                    Ok(v) => int = v,
                    Err(e) => {
                        r.outcome = Outcome::Rejected;
                        r.note = Some(e);
                    }
                },
            },
            Vendor::X264 => match &x264 {
                None => {
                    r.outcome = Outcome::Rejected;
                    r.note = Some("libx264 could not be loaded to check the option".into());
                }
                Some((_, parse)) => match x264_check(*parse, name, value) {
                    Ok(()) => {}
                    Err((o, e)) => {
                        r.outcome = o;
                        r.note = Some(e);
                    }
                },
            },
        }
        report.push(r);
        ints.push(int);
    }
    Validated { report, ints, x264 }
}

/// Arm the override set for the next encoder of `vendor` built in this process.
pub fn arm(vendor: Vendor, v: Validated) {
    let (lib, parse) = match v.x264 {
        Some((l, p)) => (Some(l), Some(p)),
        None => (None, None),
    };
    *ARMED.lock_recover() = Some(Armed {
        vendor,
        report: v.report,
        ints: v.ints,
        fired: 0,
        notes: Vec::new(),
        qsv_session: None,
        x264_parse: parse,
        _x264_lib: lib,
    });
}

/// What the hook did, after the encoder was built (or failed to build).
pub struct Disarmed {
    pub fired: u32,
    pub report: Vec<FieldReport>,
    pub notes: Vec<String>,
    qsv_session: Option<(usize, GetVideoParamFn)>,
}

impl Disarmed {
    /// Every applied field becomes `rejected`: the session refused the set as a whole and there
    /// is no way to tell which entry it objected to.
    pub fn mark_rejected(&mut self, error: &str) {
        for r in &mut self.report {
            if matches!(r.outcome, Outcome::Applied | Outcome::Pending) {
                r.outcome = Outcome::Rejected;
                r.note = Some(format!("the session did not open: {error}"));
            }
        }
    }

    /// Quick Sync: read the session's own parameters back (`MFXVideoENCODE_GetVideoParam` with
    /// all three coding-option buffers attached) and compare each applied field.
    ///
    /// # Safety
    /// The encoder whose session the hook saw must still be alive.
    pub unsafe fn qsv_readback(&mut self) {
        let Some((session, get)) = self.qsv_session else {
            return;
        };
        let mut q = Q::zeroed();
        let mut ptrs: [*mut mfxExtBuffer; 3] = [
            &mut q.co as *mut _ as *mut mfxExtBuffer,
            &mut q.co2 as *mut _ as *mut mfxExtBuffer,
            &mut q.co3 as *mut _ as *mut mfxExtBuffer,
        ];
        q.par.ExtParam = ptrs.as_mut_ptr();
        q.par.NumExtParam = 3;
        let st = unsafe { get(session as mfxSession, &mut q.par) };
        if st < 0 {
            self.notes.push(format!(
                "GetVideoParam for the read-back failed (mfxStatus={st})"
            ));
            return;
        }
        for r in &mut self.report {
            if r.outcome != Outcome::Applied {
                continue;
            }
            if let Some(f) = qsv_field(&r.field) {
                let want = f.parse(&r.value).unwrap_or(i64::MIN);
                let got = (f.get)(&q);
                r.readback = Some(json!(got));
                if got != want {
                    r.outcome = Outcome::Changed;
                    r.note = Some("the runtime reports a different value after Init".into());
                }
            }
        }
    }

    /// x264: compare against `x264_encoder_parameters` (`X264Encoder::effective_params`).
    pub fn x264_readback(&mut self, eff: &x264_param_t, parse: Option<&std::path::Path>) {
        let parse_fn = parse.and_then(|p| load_x264_parse(p).ok());
        for r in &mut self.report {
            if r.outcome != Outcome::Applied {
                continue;
            }
            let Some(get) = x264_readable(&r.field) else {
                r.note = Some("no read-back for this option".into());
                continue;
            };
            let got = get(eff);
            r.readback = Some(json!(got));
            // what the option means in the struct: apply it to a default param and read it
            if let Some((_lib, f)) = &parse_fn {
                if let Some(want) = x264_expected(*f, &r.field, &r.value, get) {
                    if (want - got).abs() > 1e-6 {
                        r.outcome = Outcome::Changed;
                        r.note = Some(format!(
                            "libx264 settled on {got} (the option alone sets {want}); the \
                             preset, the profile or validation moved it"
                        ));
                    }
                }
            }
        }
    }

    pub fn to_json(&self) -> Json {
        json!({
            "hook_fired": self.fired,
            "fields": self.report,
            "notes": self.notes,
        })
    }
}

pub fn disarm() -> Option<Disarmed> {
    ARMED.lock_recover().take().map(|a| Disarmed {
        fired: a.fired,
        report: a.report,
        notes: a.notes,
        qsv_session: a.qsv_session,
    })
}

/// The symbolic names a field accepts, beside plain integers.
type Syms = &'static [(&'static str, i64)];

fn value_int(v: &Val, syms: Syms) -> Result<i64, String> {
    let names = || syms.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(" | ");
    match v {
        Val::Bool(b) => Ok(i64::from(*b)),
        Val::Int(i) => Ok(*i),
        Val::Float(f) if f.fract() == 0.0 && f.is_finite() => Ok(*f as i64),
        Val::Str(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                return Ok(i);
            }
            if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                if let Ok(i) = i64::from_str_radix(h, 16) {
                    return Ok(i);
                }
            }
            let up = t.to_ascii_uppercase();
            for (n, val) in syms {
                // accept the bare name, or any SDK prefix in front of it (`MFX_CODINGOPTION_ON`,
                // `NV_ENC_PARAMS_RC_CBR`, ...)
                if up == *n || up.ends_with(&format!("_{n}")) {
                    return Ok(*val);
                }
            }
            if syms.is_empty() {
                Err(format!("'{s}' is not an integer"))
            } else {
                Err(format!("'{s}' is not an integer or one of: {}", names()))
            }
        }
        other => Err(format!("{other:?} is not an integer value")),
    }
}

/// One addressable field of a structure set `C`.
struct Field<C> {
    lo: i64,
    hi: i64,
    syms: Syms,
    get: fn(&C) -> i64,
    set: fn(&mut C, i64),
}

impl<C> Field<C> {
    fn parse(&self, v: &Val) -> Result<i64, String> {
        let i = value_int(v, self.syms)?;
        if i < self.lo || i > self.hi {
            return Err(format!("{i} is outside {}..={}", self.lo, self.hi));
        }
        Ok(i)
    }

    /// Set, then read back from the structure: a value the field cannot hold (a bitfield
    /// narrower than the number) is refused rather than truncated.
    fn apply(&self, c: &mut C, v: i64) -> Result<(), String> {
        let before = (self.get)(c);
        (self.set)(c, v);
        let now = (self.get)(c);
        if now != v {
            (self.set)(c, before);
            return Err(format!("the field holds {now} after setting {v}; refused"));
        }
        Ok(())
    }
}

const U8: (i64, i64) = (0, u8::MAX as i64);
const I8: (i64, i64) = (i8::MIN as i64, i8::MAX as i64);
const U16: (i64, i64) = (0, u16::MAX as i64);
const I16: (i64, i64) = (i16::MIN as i64, i16::MAX as i64);
const U32: (i64, i64) = (0, u32::MAX as i64);
const BIT: (i64, i64) = (0, 1);

/// A plain integer member, by path from the context.
macro_rules! plain {
    ($range:expr, $syms:expr, $($p:tt)+) => {
        Field {
            lo: $range.0,
            hi: $range.1,
            syms: $syms,
            get: |c| unsafe { c.$($p)+ as i64 },
            set: |c, v| unsafe { c.$($p)+ = v as _ },
        }
    };
}

/// A bindgen bitfield, by its getter and setter.
macro_rules! bits {
    ($range:expr, [$($p:tt)+], $get:ident, $set:ident) => {
        Field {
            lo: $range.0,
            hi: $range.1,
            syms: &[],
            get: |c| unsafe { c.$($p)+.$get() as i64 },
            set: |c, v| unsafe { c.$($p)+.$set(v as u32) },
        }
    };
}

/// A Rust-enum member: read as its discriminant, written through `from`.
macro_rules! enumf {
    ($syms:expr, $from:expr, $($p:tt)+) => {
        Field {
            lo: 0,
            hi: 255,
            syms: $syms,
            get: |c| unsafe { c.$($p)+ as i64 },
            set: |c, v| {
                if let Some(e) = $from(v) {
                    unsafe { c.$($p)+ = e }
                }
            },
        }
    };
}

/// An array member, set as a whole to one value; read as its first element.
macro_rules! array {
    ($range:expr, $($p:tt)+) => {
        Field {
            lo: $range.0,
            hi: $range.1,
            syms: &[],
            get: |c| c.$($p)+[0] as i64,
            set: |c, v| {
                for x in c.$($p)+.iter_mut() {
                    *x = v as _;
                }
            },
        }
    };
}

/// The NVENC structures a recipe can reach: the session's init params and the config they point
/// at, copied in, edited, copied back.
#[derive(Clone, Copy)]
struct Nv {
    init: NV_ENC_INITIALIZE_PARAMS,
    cfg: NV_ENC_CONFIG,
}

const ON_OFF: Syms = &[("OFF", 0), ("ON", 1), ("FALSE", 0), ("TRUE", 1)];
const NV_RC: Syms = &[("CONSTQP", 0), ("VBR", 1), ("CBR", 2)];
const NV_MULTIPASS: Syms = &[
    ("DISABLED", 0),
    ("QUARTER", 1),
    ("TWO_PASS_QUARTER_RESOLUTION", 1),
    ("FULL", 2),
    ("TWO_PASS_FULL_RESOLUTION", 2),
];
const NV_ENTROPY: Syms = &[("AUTOSELECT", 0), ("CABAC", 1), ("CAVLC", 2)];
const NV_TRANSFORM: Syms = &[("AUTOSELECT", 0), ("DISABLE", 1), ("ENABLE", 2)];
const NV_NUMREF: Syms = &[("AUTOSELECT", 0)];
const NV_TUNING: Syms = &[
    ("UNDEFINED", 0),
    ("HIGH_QUALITY", 1),
    ("HQ", 1),
    ("LOW_LATENCY", 2),
    ("LL", 2),
    ("ULTRA_LOW_LATENCY", 3),
    ("ULL", 3),
    ("LOSSLESS", 4),
];
const NV_PRESET: Syms = &[
    ("P1", 1),
    ("P2", 2),
    ("P3", 3),
    ("P4", 4),
    ("P5", 5),
    ("P6", 6),
    ("P7", 7),
];
const NV_PROFILE: Syms = &[
    ("AUTOSELECT", 0),
    ("BASELINE", 1),
    ("MAIN", 2),
    ("HIGH", 3),
    ("CONSTRAINED_HIGH", 4),
    ("PROGRESSIVE_HIGH", 5),
];
const NV_SPLIT: Syms = &[
    ("AUTO", 0),
    ("AUTO_FORCED", 1),
    ("TWO_FORCED", 2),
    ("THREE_FORCED", 3),
    ("DISABLE", 15),
];
const NV_GOP: Syms = &[("INFINITE", u32::MAX as i64)];

fn nv_rc(v: i64) -> Option<NV_ENC_PARAMS_RC_MODE> {
    use NV_ENC_PARAMS_RC_MODE::*;
    Some(match v {
        0 => NV_ENC_PARAMS_RC_CONSTQP,
        1 => NV_ENC_PARAMS_RC_VBR,
        2 => NV_ENC_PARAMS_RC_CBR,
        _ => return None,
    })
}
fn nv_mp(v: i64) -> Option<NV_ENC_MULTI_PASS> {
    use NV_ENC_MULTI_PASS::*;
    Some(match v {
        0 => NV_ENC_MULTI_PASS_DISABLED,
        1 => NV_ENC_TWO_PASS_QUARTER_RESOLUTION,
        2 => NV_ENC_TWO_PASS_FULL_RESOLUTION,
        _ => return None,
    })
}
fn nv_entropy(v: i64) -> Option<NV_ENC_H264_ENTROPY_CODING_MODE> {
    use NV_ENC_H264_ENTROPY_CODING_MODE::*;
    Some(match v {
        0 => NV_ENC_H264_ENTROPY_CODING_MODE_AUTOSELECT,
        1 => NV_ENC_H264_ENTROPY_CODING_MODE_CABAC,
        2 => NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC,
        _ => return None,
    })
}
fn nv_transform(v: i64) -> Option<NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE> {
    use NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE::*;
    Some(match v {
        0 => NV_ENC_H264_ADAPTIVE_TRANSFORM_AUTOSELECT,
        1 => NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE,
        2 => NV_ENC_H264_ADAPTIVE_TRANSFORM_ENABLE,
        _ => return None,
    })
}
fn nv_numref(v: i64) -> Option<NV_ENC_NUM_REF_FRAMES> {
    use NV_ENC_NUM_REF_FRAMES::*;
    Some(match v {
        0 => NV_ENC_NUM_REF_FRAMES_AUTOSELECT,
        1 => NV_ENC_NUM_REF_FRAMES_1,
        2 => NV_ENC_NUM_REF_FRAMES_2,
        3 => NV_ENC_NUM_REF_FRAMES_3,
        4 => NV_ENC_NUM_REF_FRAMES_4,
        5 => NV_ENC_NUM_REF_FRAMES_5,
        6 => NV_ENC_NUM_REF_FRAMES_6,
        7 => NV_ENC_NUM_REF_FRAMES_7,
        _ => return None,
    })
}
fn nv_tuning(v: i64) -> Option<NV_ENC_TUNING_INFO> {
    use NV_ENC_TUNING_INFO::*;
    Some(match v {
        0 => NV_ENC_TUNING_INFO_UNDEFINED,
        1 => NV_ENC_TUNING_INFO_HIGH_QUALITY,
        2 => NV_ENC_TUNING_INFO_LOW_LATENCY,
        3 => NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
        4 => NV_ENC_TUNING_INFO_LOSSLESS,
        _ => return None,
    })
}

fn nv_preset_guid(i: i64) -> Option<GUID> {
    use crate::windows_utils::streamer::nvidia::nvenc_sys::*;
    Some(match i {
        1 => NV_ENC_PRESET_P1_GUID,
        2 => NV_ENC_PRESET_P2_GUID,
        3 => NV_ENC_PRESET_P3_GUID,
        4 => NV_ENC_PRESET_P4_GUID,
        5 => NV_ENC_PRESET_P5_GUID,
        6 => NV_ENC_PRESET_P6_GUID,
        7 => NV_ENC_PRESET_P7_GUID,
        _ => return None,
    })
}

fn nv_preset_index(g: &GUID) -> i64 {
    (1..=7)
        .find(|i| nv_preset_guid(*i).as_ref() == Some(g))
        .unwrap_or(0)
}

fn nv_profile_guid(i: i64) -> Option<GUID> {
    use crate::windows_utils::streamer::nvidia::nvenc_sys::*;
    Some(match i {
        0 => NV_ENC_CODEC_PROFILE_AUTOSELECT_GUID,
        1 => NV_ENC_H264_PROFILE_BASELINE_GUID,
        2 => NV_ENC_H264_PROFILE_MAIN_GUID,
        3 => NV_ENC_H264_PROFILE_HIGH_GUID,
        4 => NV_ENC_H264_PROFILE_CONSTRAINED_HIGH_GUID,
        5 => NV_ENC_H264_PROFILE_PROGRESSIVE_HIGH_GUID,
        _ => return None,
    })
}

fn nv_profile_index(g: &GUID) -> i64 {
    (0..=5)
        .find(|i| nv_profile_guid(*i).as_ref() == Some(g))
        .unwrap_or(-1)
}

/// Names a recipe may not use, with the reason (reported as `unknown` with that note).
fn refused_nvenc(name: &str) -> Option<&'static str> {
    match name {
        "enableOutputInVidmem" => Some(
            "refused by the bench: production's bitstream lock reads a system-memory output and \
             would read garbage from a video-memory one",
        ),
        "enableMEOnlyMode" | "enableReconFrameOutput" | "enableOutputStats" => Some(
            "refused by the bench: production does not consume this output, so the session \
             would not be the production one",
        ),
        "encodeWidth" | "encodeHeight" | "maxEncodeWidth" | "maxEncodeHeight" | "darWidth"
        | "darHeight" | "bufferFormat" => Some(
            "refused by the bench: geometry and input format are run axes (the workload size and \
             the recipe's `input`), not fields",
        ),
        _ => None,
    }
}

fn nvenc_field(name: &str) -> Option<Field<Nv>> {
    Some(match name {
        "preset" | "presetGUID" => Field {
            lo: 1,
            hi: 7,
            syms: NV_PRESET,
            get: |c| nv_preset_index(&c.init.presetGUID),
            set: |c, v| {
                if let Some(g) = nv_preset_guid(v) {
                    c.init.presetGUID = g;
                }
            },
        },
        "tuning" | "tuningInfo" => enumf!(NV_TUNING, nv_tuning, init.tuningInfo),
        "enableEncodeAsync" => plain!(BIT, ON_OFF, init.enableEncodeAsync),
        "enablePTD" => plain!(BIT, ON_OFF, init.enablePTD),
        "frameRateNum" => plain!(U32, &[], init.frameRateNum),
        "frameRateDen" => plain!(U32, &[], init.frameRateDen),
        "enableWeightedPrediction" => {
            bits!(
                BIT,
                [init],
                enableWeightedPrediction,
                set_enableWeightedPrediction
            )
        }
        "enableSubFrameWrite" => bits!(BIT, [init], enableSubFrameWrite, set_enableSubFrameWrite),
        "reportSliceOffsets" => bits!(BIT, [init], reportSliceOffsets, set_reportSliceOffsets),
        "splitEncodeMode" => Field {
            lo: 0,
            hi: 15,
            syms: NV_SPLIT,
            get: |c| c.init.splitEncodeMode() as i64,
            set: |c, v| c.init.set_splitEncodeMode(v as u32),
        },
        "numStateBuffers" => plain!(U32, &[], init.numStateBuffers),
        "profile" | "profileGUID" => Field {
            lo: 0,
            hi: 5,
            syms: NV_PROFILE,
            get: |c| nv_profile_index(&c.cfg.profileGUID),
            set: |c, v| {
                if let Some(g) = nv_profile_guid(v) {
                    c.cfg.profileGUID = g;
                }
            },
        },
        "gopLength" => plain!(U32, NV_GOP, cfg.gopLength),
        "frameIntervalP" => plain!((0, i32::MAX as i64), &[], cfg.frameIntervalP),
        "rc.rateControlMode" => enumf!(NV_RC, nv_rc, cfg.rcParams.rateControlMode),
        "rc.constQP.qpInterP" => plain!((0, 51), &[], cfg.rcParams.constQP.qpInterP),
        "rc.constQP.qpInterB" => plain!((0, 51), &[], cfg.rcParams.constQP.qpInterB),
        "rc.constQP.qpIntra" => plain!((0, 51), &[], cfg.rcParams.constQP.qpIntra),
        "rc.averageBitRate" => plain!(U32, &[], cfg.rcParams.averageBitRate),
        "rc.maxBitRate" => plain!(U32, &[], cfg.rcParams.maxBitRate),
        "rc.vbvBufferSize" => plain!(U32, &[], cfg.rcParams.vbvBufferSize),
        "rc.vbvInitialDelay" => plain!(U32, &[], cfg.rcParams.vbvInitialDelay),
        "rc.enableMinQP" => bits!(BIT, [cfg.rcParams], enableMinQP, set_enableMinQP),
        "rc.enableMaxQP" => bits!(BIT, [cfg.rcParams], enableMaxQP, set_enableMaxQP),
        "rc.enableInitialRCQP" => {
            bits!(
                BIT,
                [cfg.rcParams],
                enableInitialRCQP,
                set_enableInitialRCQP
            )
        }
        "rc.enableAQ" => bits!(BIT, [cfg.rcParams], enableAQ, set_enableAQ),
        "rc.enableLookahead" => bits!(BIT, [cfg.rcParams], enableLookahead, set_enableLookahead),
        "rc.disableIadapt" => bits!(BIT, [cfg.rcParams], disableIadapt, set_disableIadapt),
        "rc.disableBadapt" => bits!(BIT, [cfg.rcParams], disableBadapt, set_disableBadapt),
        "rc.enableTemporalAQ" => {
            bits!(BIT, [cfg.rcParams], enableTemporalAQ, set_enableTemporalAQ)
        }
        "rc.zeroReorderDelay" => {
            bits!(BIT, [cfg.rcParams], zeroReorderDelay, set_zeroReorderDelay)
        }
        "rc.enableNonRefP" => bits!(BIT, [cfg.rcParams], enableNonRefP, set_enableNonRefP),
        "rc.strictGOPTarget" => bits!(BIT, [cfg.rcParams], strictGOPTarget, set_strictGOPTarget),
        "rc.aqStrength" => bits!((0, 15), [cfg.rcParams], aqStrength, set_aqStrength),
        "rc.enableExtLookahead" => {
            bits!(
                BIT,
                [cfg.rcParams],
                enableExtLookahead,
                set_enableExtLookahead
            )
        }
        "rc.minQP.qpInterP" => plain!((0, 51), &[], cfg.rcParams.minQP.qpInterP),
        "rc.minQP.qpInterB" => plain!((0, 51), &[], cfg.rcParams.minQP.qpInterB),
        "rc.minQP.qpIntra" => plain!((0, 51), &[], cfg.rcParams.minQP.qpIntra),
        "rc.maxQP.qpInterP" => plain!((0, 51), &[], cfg.rcParams.maxQP.qpInterP),
        "rc.maxQP.qpInterB" => plain!((0, 51), &[], cfg.rcParams.maxQP.qpInterB),
        "rc.maxQP.qpIntra" => plain!((0, 51), &[], cfg.rcParams.maxQP.qpIntra),
        "rc.initialRCQP.qpInterP" => plain!((0, 51), &[], cfg.rcParams.initialRCQP.qpInterP),
        "rc.initialRCQP.qpInterB" => plain!((0, 51), &[], cfg.rcParams.initialRCQP.qpInterB),
        "rc.initialRCQP.qpIntra" => plain!((0, 51), &[], cfg.rcParams.initialRCQP.qpIntra),
        "rc.targetQuality" => plain!(U8, &[], cfg.rcParams.targetQuality),
        "rc.targetQualityLSB" => plain!(U8, &[], cfg.rcParams.targetQualityLSB),
        "rc.lookaheadDepth" => plain!(U16, &[], cfg.rcParams.lookaheadDepth),
        "rc.lowDelayKeyFrameScale" => plain!(U8, &[], cfg.rcParams.lowDelayKeyFrameScale),
        "rc.yDcQPIndexOffset" => plain!(I8, &[], cfg.rcParams.yDcQPIndexOffset),
        "rc.uDcQPIndexOffset" => plain!(I8, &[], cfg.rcParams.uDcQPIndexOffset),
        "rc.vDcQPIndexOffset" => plain!(I8, &[], cfg.rcParams.vDcQPIndexOffset),
        "rc.multiPass" => enumf!(NV_MULTIPASS, nv_mp, cfg.rcParams.multiPass),
        "rc.cbQPIndexOffset" => plain!(I8, &[], cfg.rcParams.cbQPIndexOffset),
        "rc.crQPIndexOffset" => plain!(I8, &[], cfg.rcParams.crQPIndexOffset),
        "h264.enableTemporalSVC" => {
            bits!(
                BIT,
                [cfg.encodeCodecConfig.h264Config],
                enableTemporalSVC,
                set_enableTemporalSVC
            )
        }
        "h264.enableStereoMVC" => {
            bits!(
                BIT,
                [cfg.encodeCodecConfig.h264Config],
                enableStereoMVC,
                set_enableStereoMVC
            )
        }
        "h264.hierarchicalPFrames" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            hierarchicalPFrames,
            set_hierarchicalPFrames
        ),
        "h264.hierarchicalBFrames" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            hierarchicalBFrames,
            set_hierarchicalBFrames
        ),
        "h264.outputBufferingPeriodSEI" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            outputBufferingPeriodSEI,
            set_outputBufferingPeriodSEI
        ),
        "h264.outputPictureTimingSEI" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            outputPictureTimingSEI,
            set_outputPictureTimingSEI
        ),
        "h264.outputAUD" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            outputAUD,
            set_outputAUD
        ),
        "h264.disableSPSPPS" => {
            bits!(
                BIT,
                [cfg.encodeCodecConfig.h264Config],
                disableSPSPPS,
                set_disableSPSPPS
            )
        }
        "h264.outputFramePackingSEI" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            outputFramePackingSEI,
            set_outputFramePackingSEI
        ),
        "h264.outputRecoveryPointSEI" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            outputRecoveryPointSEI,
            set_outputRecoveryPointSEI
        ),
        "h264.enableIntraRefresh" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            enableIntraRefresh,
            set_enableIntraRefresh
        ),
        "h264.enableConstrainedEncoding" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            enableConstrainedEncoding,
            set_enableConstrainedEncoding
        ),
        "h264.repeatSPSPPS" => {
            bits!(
                BIT,
                [cfg.encodeCodecConfig.h264Config],
                repeatSPSPPS,
                set_repeatSPSPPS
            )
        }
        "h264.enableVFR" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            enableVFR,
            set_enableVFR
        ),
        "h264.enableLTR" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            enableLTR,
            set_enableLTR
        ),
        "h264.qpPrimeYZeroTransformBypassFlag" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            qpPrimeYZeroTransformBypassFlag,
            set_qpPrimeYZeroTransformBypassFlag
        ),
        "h264.useConstrainedIntraPred" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            useConstrainedIntraPred,
            set_useConstrainedIntraPred
        ),
        "h264.enableFillerDataInsertion" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            enableFillerDataInsertion,
            set_enableFillerDataInsertion
        ),
        "h264.disableSVCPrefixNalu" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            disableSVCPrefixNalu,
            set_disableSVCPrefixNalu
        ),
        "h264.enableScalabilityInfoSEI" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            enableScalabilityInfoSEI,
            set_enableScalabilityInfoSEI
        ),
        "h264.singleSliceIntraRefresh" => bits!(
            BIT,
            [cfg.encodeCodecConfig.h264Config],
            singleSliceIntraRefresh,
            set_singleSliceIntraRefresh
        ),
        "h264.enableTimeCode" => {
            bits!(
                BIT,
                [cfg.encodeCodecConfig.h264Config],
                enableTimeCode,
                set_enableTimeCode
            )
        }
        "h264.level" => plain!(
            U32,
            &[("AUTOSELECT", 0)],
            cfg.encodeCodecConfig.h264Config.level
        ),
        "h264.idrPeriod" => plain!(U32, NV_GOP, cfg.encodeCodecConfig.h264Config.idrPeriod),
        "h264.disableDeblockingFilterIDC" => plain!(
            (0, 2),
            &[],
            cfg.encodeCodecConfig.h264Config.disableDeblockingFilterIDC
        ),
        "h264.numTemporalLayers" => {
            plain!(U32, &[], cfg.encodeCodecConfig.h264Config.numTemporalLayers)
        }
        "h264.maxTemporalLayers" => {
            plain!(U32, &[], cfg.encodeCodecConfig.h264Config.maxTemporalLayers)
        }
        "h264.adaptiveTransformMode" => enumf!(
            NV_TRANSFORM,
            nv_transform,
            cfg.encodeCodecConfig.h264Config.adaptiveTransformMode
        ),
        "h264.entropyCodingMode" => enumf!(
            NV_ENTROPY,
            nv_entropy,
            cfg.encodeCodecConfig.h264Config.entropyCodingMode
        ),
        "h264.intraRefreshPeriod" => {
            plain!(
                U32,
                &[],
                cfg.encodeCodecConfig.h264Config.intraRefreshPeriod
            )
        }
        "h264.intraRefreshCnt" => {
            plain!(U32, &[], cfg.encodeCodecConfig.h264Config.intraRefreshCnt)
        }
        "h264.maxNumRefFrames" => plain!(
            (0, 16),
            &[],
            cfg.encodeCodecConfig.h264Config.maxNumRefFrames
        ),
        "h264.sliceMode" => plain!((0, 3), &[], cfg.encodeCodecConfig.h264Config.sliceMode),
        "h264.sliceModeData" => plain!(U32, &[], cfg.encodeCodecConfig.h264Config.sliceModeData),
        "h264.ltrNumFrames" => plain!(U32, &[], cfg.encodeCodecConfig.h264Config.ltrNumFrames),
        "h264.ltrTrustMode" => plain!(BIT, &[], cfg.encodeCodecConfig.h264Config.ltrTrustMode),
        "h264.numRefL0" => enumf!(
            NV_NUMREF,
            nv_numref,
            cfg.encodeCodecConfig.h264Config.numRefL0
        ),
        "h264.numRefL1" => enumf!(
            NV_NUMREF,
            nv_numref,
            cfg.encodeCodecConfig.h264Config.numRefL1
        ),
        "h264.vui.videoSignalTypePresentFlag" => plain!(
            BIT,
            &[],
            cfg.encodeCodecConfig
                .h264Config
                .h264VUIParameters
                .videoSignalTypePresentFlag
        ),
        "h264.vui.videoFullRangeFlag" => plain!(
            BIT,
            &[],
            cfg.encodeCodecConfig
                .h264Config
                .h264VUIParameters
                .videoFullRangeFlag
        ),
        "h264.vui.colourDescriptionPresentFlag" => plain!(
            BIT,
            &[],
            cfg.encodeCodecConfig
                .h264Config
                .h264VUIParameters
                .colourDescriptionPresentFlag
        ),
        "h264.vui.bitstreamRestrictionFlag" => plain!(
            BIT,
            &[],
            cfg.encodeCodecConfig
                .h264Config
                .h264VUIParameters
                .bitstreamRestrictionFlag
        ),
        "h264.vui.timingInfoPresentFlag" => plain!(
            BIT,
            &[],
            cfg.encodeCodecConfig
                .h264Config
                .h264VUIParameters
                .timingInfoPresentFlag
        ),
        _ => return None,
    })
}

#[cfg(test)]
const NVENC_FIELDS: &[&str] = &[
    "preset",
    "tuning",
    "enableEncodeAsync",
    "enablePTD",
    "frameRateNum",
    "frameRateDen",
    "enableWeightedPrediction",
    "enableSubFrameWrite",
    "reportSliceOffsets",
    "splitEncodeMode",
    "numStateBuffers",
    "profile",
    "gopLength",
    "frameIntervalP",
    "rc.rateControlMode",
    "rc.constQP.qpInterP",
    "rc.constQP.qpInterB",
    "rc.constQP.qpIntra",
    "rc.averageBitRate",
    "rc.maxBitRate",
    "rc.vbvBufferSize",
    "rc.vbvInitialDelay",
    "rc.enableMinQP",
    "rc.enableMaxQP",
    "rc.enableInitialRCQP",
    "rc.enableAQ",
    "rc.enableLookahead",
    "rc.disableIadapt",
    "rc.disableBadapt",
    "rc.enableTemporalAQ",
    "rc.zeroReorderDelay",
    "rc.enableNonRefP",
    "rc.strictGOPTarget",
    "rc.aqStrength",
    "rc.enableExtLookahead",
    "rc.minQP.qpInterP",
    "rc.minQP.qpInterB",
    "rc.minQP.qpIntra",
    "rc.maxQP.qpInterP",
    "rc.maxQP.qpInterB",
    "rc.maxQP.qpIntra",
    "rc.initialRCQP.qpInterP",
    "rc.initialRCQP.qpInterB",
    "rc.initialRCQP.qpIntra",
    "rc.targetQuality",
    "rc.targetQualityLSB",
    "rc.lookaheadDepth",
    "rc.lowDelayKeyFrameScale",
    "rc.yDcQPIndexOffset",
    "rc.uDcQPIndexOffset",
    "rc.vDcQPIndexOffset",
    "rc.multiPass",
    "rc.cbQPIndexOffset",
    "rc.crQPIndexOffset",
    "h264.enableTemporalSVC",
    "h264.enableStereoMVC",
    "h264.hierarchicalPFrames",
    "h264.hierarchicalBFrames",
    "h264.outputBufferingPeriodSEI",
    "h264.outputPictureTimingSEI",
    "h264.outputAUD",
    "h264.disableSPSPPS",
    "h264.outputFramePackingSEI",
    "h264.outputRecoveryPointSEI",
    "h264.enableIntraRefresh",
    "h264.enableConstrainedEncoding",
    "h264.repeatSPSPPS",
    "h264.enableVFR",
    "h264.enableLTR",
    "h264.qpPrimeYZeroTransformBypassFlag",
    "h264.useConstrainedIntraPred",
    "h264.enableFillerDataInsertion",
    "h264.disableSVCPrefixNalu",
    "h264.enableScalabilityInfoSEI",
    "h264.singleSliceIntraRefresh",
    "h264.enableTimeCode",
    "h264.level",
    "h264.idrPeriod",
    "h264.disableDeblockingFilterIDC",
    "h264.numTemporalLayers",
    "h264.maxTemporalLayers",
    "h264.adaptiveTransformMode",
    "h264.entropyCodingMode",
    "h264.intraRefreshPeriod",
    "h264.intraRefreshCnt",
    "h264.maxNumRefFrames",
    "h264.sliceMode",
    "h264.sliceModeData",
    "h264.ltrNumFrames",
    "h264.ltrTrustMode",
    "h264.numRefL0",
    "h264.numRefL1",
    "h264.vui.videoSignalTypePresentFlag",
    "h264.vui.videoFullRangeFlag",
    "h264.vui.colourDescriptionPresentFlag",
    "h264.vui.bitstreamRestrictionFlag",
    "h264.vui.timingInfoPresentFlag",
];

/// NVENC hook, called right before `nvEncInitializeEncoder`. `preset` and `tuning` go first and
/// rebase the config onto the new preset, keeping every bit production changed from P1 / ULL.
pub(crate) fn nvenc(
    fns: &NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    init: &mut NV_ENC_INITIALIZE_PARAMS,
) {
    let mut g = ARMED.lock_recover();
    let Some(a) = g.as_mut().filter(|a| a.vendor == Vendor::Nvenc) else {
        return;
    };
    a.fired += 1;
    if init.encodeConfig.is_null() {
        a.notes
            .push("init.encodeConfig was null; nothing applied".into());
        return;
    }
    // SAFETY: production points `encodeConfig` at its live local config for this call.
    let cfg_ptr = init.encodeConfig;
    let mut nv = Nv {
        init: *init,
        cfg: unsafe { *cfg_ptr },
    };
    let old_preset = nv.init.presetGUID;
    let old_tuning = nv.init.tuningInfo;
    for (i, r) in a.report.iter_mut().enumerate() {
        if !r.outcome.applies()
            || !matches!(
                r.field.as_str(),
                "preset" | "presetGUID" | "tuning" | "tuningInfo"
            )
        {
            continue;
        }
        if let Some(f) = nvenc_field(&r.field) {
            match f.apply(&mut nv, a.ints[i]) {
                Ok(()) => {
                    r.outcome = Outcome::Applied;
                    r.readback = Some(json!((f.get)(&nv)));
                }
                Err(e) => {
                    r.outcome = Outcome::Rejected;
                    r.note = Some(e);
                }
            }
        }
    }
    if nv.init.presetGUID != old_preset || nv.init.tuningInfo != old_tuning {
        match rebase_preset(
            fns,
            encoder,
            nv.init.encodeGUID,
            (old_preset, old_tuning),
            (nv.init.presetGUID, nv.init.tuningInfo),
            &nv.cfg,
        ) {
            Ok((merged, kept)) => {
                nv.cfg = merged;
                a.notes.push(format!(
                    "preset/tuning re-derived: the config now starts from the new preset, with \
                     the {kept} bits production changed relative to P1/ULL kept"
                ));
            }
            Err(e) => a.notes.push(format!(
                "preset/tuning set on the init params only; re-deriving the config failed: {e}"
            )),
        }
    }
    for (i, r) in a.report.iter_mut().enumerate() {
        if !r.outcome.applies() {
            continue;
        }
        let Some(f) = nvenc_field(&r.field) else {
            continue;
        };
        if let Some(note) = nvenc_special(&r.field) {
            let got = (f.get)(&nv);
            r.readback = Some(json!(got));
            r.outcome = if got == a.ints[i] {
                Outcome::Applied
            } else {
                Outcome::Changed
            };
            r.note = Some(note.into());
            continue;
        }
        match f.apply(&mut nv, a.ints[i]) {
            Ok(()) => {
                r.outcome = Outcome::Applied;
                r.readback = Some(json!((f.get)(&nv)));
            }
            Err(e) => {
                r.outcome = Outcome::Rejected;
                r.note = Some(e);
            }
        }
    }
    nv.init.encodeConfig = cfg_ptr;
    *init = nv.init;
    // SAFETY: as above.
    unsafe { *cfg_ptr = nv.cfg };
}

/// An all-zero C struct. NVENC structs hold enums with no zero variant that the driver reads as
/// "unset", which `mem::zeroed` refuses in a debug build.
///
/// # Safety
/// `T` must be a plain C struct the code fills before any enum field is read as a Rust value.
unsafe fn pod_zeroed<T>() -> T {
    unsafe { std::mem::MaybeUninit::zeroed().assume_init() }
}

fn preset_config(
    fns: &NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    codec: GUID,
    preset: GUID,
    tuning: NV_ENC_TUNING_INFO,
) -> Result<NV_ENC_CONFIG, String> {
    let f = fns
        .nvEncGetEncodePresetConfigEx
        .ok_or("nvEncGetEncodePresetConfigEx missing")?;
    // SAFETY: plain C structs, zero is a valid initial value; version words set as production does.
    let mut pc: NV_ENC_PRESET_CONFIG = unsafe { pod_zeroed() };
    pc.version = NV_ENC_PRESET_CONFIG_VER;
    pc.presetCfg.version = NV_ENC_CONFIG_VER;
    let st = unsafe { f(encoder, codec, preset, tuning, &mut pc) };
    if st != NVENCSTATUS::NV_ENC_SUCCESS {
        return Err(format!("nvEncGetEncodePresetConfigEx returned {st:?}"));
    }
    Ok(pc.presetCfg)
}

/// Bitwise rebase of `prod` from the `old` preset onto the `new` one.
fn rebase_preset(
    fns: &NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    codec: GUID,
    old: (GUID, NV_ENC_TUNING_INFO),
    new: (GUID, NV_ENC_TUNING_INFO),
    prod: &NV_ENC_CONFIG,
) -> Result<(NV_ENC_CONFIG, u32), String> {
    let o = preset_config(fns, encoder, codec, old.0, old.1)?;
    let n = preset_config(fns, encoder, codec, new.0, new.1)?;
    Ok(rebase_bytes(prod, &o, &n))
}

/// `(n & !mask) | (prod & mask)` with `mask = prod ^ o`, over the raw bytes of a POD struct, and
/// the number of bits production had changed.
fn rebase_bytes<T: Copy>(prod: &T, o: &T, n: &T) -> (T, u32) {
    let size = std::mem::size_of::<T>();
    let mut out = *n;
    let mut kept = 0u32;
    // SAFETY: `T` is a plain-old-data FFI struct (Copy, no references); viewing it as bytes and
    // writing bytes back produces another valid value of it.
    unsafe {
        let p = prod as *const T as *const u8;
        let ob = o as *const T as *const u8;
        let d = &mut out as *mut T as *mut u8;
        for i in 0..size {
            let mask = *p.add(i) ^ *ob.add(i);
            kept += mask.count_ones();
            *d.add(i) = (*d.add(i) & !mask) | (*p.add(i) & mask);
        }
    }
    (out, kept)
}

#[derive(Clone, Copy)]
struct Q {
    par: mfxVideoParam,
    co: mfxExtCodingOption,
    co2: mfxExtCodingOption2,
    co3: mfxExtCodingOption3,
}

impl Q {
    fn zeroed() -> Q {
        // SAFETY: oneVPL parameter structs are plain C structs; zero is their documented "unset".
        let mut q: Q = unsafe { std::mem::zeroed() };
        q.co.Header.BufferId = MFX_EXTBUFF_CODING_OPTION;
        q.co.Header.BufferSz = std::mem::size_of::<mfxExtCodingOption>() as u32;
        q.co2.Header.BufferId = MFX_EXTBUFF_CODING_OPTION2;
        q.co2.Header.BufferSz = std::mem::size_of::<mfxExtCodingOption2>() as u32;
        q.co3.Header.BufferId = MFX_EXTBUFF_CODING_OPTION3;
        q.co3.Header.BufferSz = std::mem::size_of::<mfxExtCodingOption3>() as u32;
        q
    }
}

const MFX_TRI: Syms = &[
    ("UNKNOWN", 0x00),
    ("ON", 0x10),
    ("OFF", 0x20),
    ("ADAPTIVE", 0x30),
];
const MFX_RC: Syms = &[
    ("CBR", 1),
    ("VBR", 2),
    ("CQP", 3),
    ("AVBR", 4),
    ("LA", 8),
    ("ICQ", 9),
    ("VCM", 10),
    ("LA_ICQ", 11),
    ("LA_HRD", 13),
    ("QVBR", 14),
];
const MFX_TU: Syms = &[("BEST_QUALITY", 1), ("BALANCED", 4), ("BEST_SPEED", 7)];
const MFX_SCENARIO: Syms = &[
    ("UNKNOWN", 0),
    ("DISPLAY_REMOTING", 1),
    ("VIDEO_CONFERENCE", 2),
    ("ARCHIVE", 3),
    ("LIVE_STREAMING", 4),
    ("CAMERA_CAPTURE", 5),
    ("VIDEO_SURVEILLANCE", 6),
    ("GAME_STREAMING", 7),
    ("REMOTE_GAMING", 8),
];
const MFX_CONTENT: Syms = &[
    ("UNKNOWN", 0),
    ("FULL_SCREEN_VIDEO", 1),
    ("NON_VIDEO_SCREEN", 2),
];
const MFX_REFRESH: Syms = &[("NO", 0), ("VERTICAL", 1), ("HORIZONTAL", 2), ("SLICE", 3)];
const MFX_BREF: Syms = &[("UNKNOWN", 0), ("OFF", 1), ("PYRAMID", 2)];
const MFX_PREF: Syms = &[("DEFAULT", 0), ("SIMPLE", 1), ("PYRAMID", 2)];
const MFX_GOPOPT: Syms = &[("CLOSED", 1), ("STRICT", 2)];
const MFX_PROFILE: Syms = &[
    ("BASELINE", 66),
    ("MAIN", 77),
    ("HIGH", 100),
    ("CONSTRAINED_BASELINE", 66 + 0x200),
    ("CONSTRAINED_HIGH", 100 + 0x800),
    ("PROGRESSIVE_HIGH", 100 + 0x1000),
];

fn qsv_field(name: &str) -> Option<Field<Q>> {
    Some(match name {
        "AsyncDepth" => plain!(U16, &[], par.AsyncDepth),
        "mfx.LowPower" => plain!(U16, MFX_TRI, par.info.mfx.LowPower),
        "mfx.BRCParamMultiplier" => plain!(U16, &[], par.info.mfx.BRCParamMultiplier),
        "mfx.CodecProfile" => plain!(U16, MFX_PROFILE, par.info.mfx.CodecProfile),
        "mfx.CodecLevel" => plain!(U16, &[], par.info.mfx.CodecLevel),
        "mfx.NumThread" => plain!(U16, &[], par.info.mfx.NumThread),
        "mfx.TargetUsage" => plain!((0, 7), MFX_TU, par.info.mfx.TargetUsage),
        "mfx.GopPicSize" => plain!(U16, &[], par.info.mfx.GopPicSize),
        "mfx.GopRefDist" => plain!(U16, &[], par.info.mfx.GopRefDist),
        "mfx.GopOptFlag" => plain!(U16, MFX_GOPOPT, par.info.mfx.GopOptFlag),
        "mfx.IdrInterval" => plain!(U16, &[], par.info.mfx.IdrInterval),
        "mfx.RateControlMethod" => plain!(U16, MFX_RC, par.info.mfx.RateControlMethod),
        "mfx.InitialDelayInKB" | "mfx.QPI" | "mfx.Accuracy" => {
            plain!(U16, &[], par.info.mfx.InitialDelayInKB)
        }
        "mfx.BufferSizeInKB" => plain!(U16, &[], par.info.mfx.BufferSizeInKB),
        "mfx.TargetKbps" | "mfx.QPP" | "mfx.ICQQuality" => {
            plain!(U16, &[], par.info.mfx.TargetKbps)
        }
        "mfx.MaxKbps" | "mfx.QPB" | "mfx.Convergence" => plain!(U16, &[], par.info.mfx.MaxKbps),
        "mfx.NumSlice" => plain!(U16, &[], par.info.mfx.NumSlice),
        "mfx.NumRefFrame" => plain!(U16, &[], par.info.mfx.NumRefFrame),
        // mfxExtCodingOption
        "co.RateDistortionOpt" => plain!(U16, MFX_TRI, co.RateDistortionOpt),
        "co.MECostType" => plain!(U16, &[], co.MECostType),
        "co.MESearchType" => plain!(U16, &[], co.MESearchType),
        "co.CAVLC" => plain!(U16, MFX_TRI, co.CAVLC),
        "co.RecoveryPointSEI" => plain!(U16, MFX_TRI, co.RecoveryPointSEI),
        "co.NalHrdConformance" => plain!(U16, MFX_TRI, co.NalHrdConformance),
        "co.SingleSeiNalUnit" => plain!(U16, MFX_TRI, co.SingleSeiNalUnit),
        "co.VuiVclHrdParameters" => plain!(U16, MFX_TRI, co.VuiVclHrdParameters),
        "co.RefPicListReordering" => plain!(U16, MFX_TRI, co.RefPicListReordering),
        "co.ResetRefList" => plain!(U16, MFX_TRI, co.ResetRefList),
        "co.RefPicMarkRep" => plain!(U16, MFX_TRI, co.RefPicMarkRep),
        "co.IntraPredBlockSize" => plain!(U16, &[], co.IntraPredBlockSize),
        "co.InterPredBlockSize" => plain!(U16, &[], co.InterPredBlockSize),
        "co.MVPrecision" => plain!(U16, &[], co.MVPrecision),
        "co.MaxDecFrameBuffering" => plain!(U16, &[], co.MaxDecFrameBuffering),
        "co.AUDelimiter" => plain!(U16, MFX_TRI, co.AUDelimiter),
        "co.PicTimingSEI" => plain!(U16, MFX_TRI, co.PicTimingSEI),
        "co.VuiNalHrdParameters" => plain!(U16, MFX_TRI, co.VuiNalHrdParameters),
        // mfxExtCodingOption2
        "co2.IntRefType" => plain!(U16, MFX_REFRESH, co2.IntRefType),
        "co2.IntRefCycleSize" => plain!(U16, &[], co2.IntRefCycleSize),
        "co2.IntRefQPDelta" => plain!(I16, &[], co2.IntRefQPDelta),
        "co2.MaxFrameSize" => plain!(U32, &[], co2.MaxFrameSize),
        "co2.MaxSliceSize" => plain!(U32, &[], co2.MaxSliceSize),
        "co2.MBBRC" => plain!(U16, MFX_TRI, co2.MBBRC),
        "co2.ExtBRC" => plain!(U16, MFX_TRI, co2.ExtBRC),
        "co2.LookAheadDepth" => plain!(U16, &[], co2.LookAheadDepth),
        "co2.Trellis" => plain!(U16, &[], co2.Trellis),
        "co2.RepeatPPS" => plain!(U16, MFX_TRI, co2.RepeatPPS),
        "co2.BRefType" => plain!(U16, MFX_BREF, co2.BRefType),
        "co2.AdaptiveI" => plain!(U16, MFX_TRI, co2.AdaptiveI),
        "co2.AdaptiveB" => plain!(U16, MFX_TRI, co2.AdaptiveB),
        "co2.LookAheadDS" => plain!(U16, &[], co2.LookAheadDS),
        "co2.NumMbPerSlice" => plain!(U16, &[], co2.NumMbPerSlice),
        "co2.SkipFrame" => plain!(U16, &[], co2.SkipFrame),
        "co2.MinQPI" => plain!(U8, &[], co2.MinQPI),
        "co2.MaxQPI" => plain!(U8, &[], co2.MaxQPI),
        "co2.MinQPP" => plain!(U8, &[], co2.MinQPP),
        "co2.MaxQPP" => plain!(U8, &[], co2.MaxQPP),
        "co2.MinQPB" => plain!(U8, &[], co2.MinQPB),
        "co2.MaxQPB" => plain!(U8, &[], co2.MaxQPB),
        "co2.FixedFrameRate" => plain!(U16, MFX_TRI, co2.FixedFrameRate),
        "co2.DisableDeblockingIdc" => plain!(U16, &[], co2.DisableDeblockingIdc),
        "co2.DisableVUI" => plain!(U16, MFX_TRI, co2.DisableVUI),
        "co2.BufferingPeriodSEI" => plain!(U16, &[], co2.BufferingPeriodSEI),
        "co2.EnableMAD" => plain!(U16, MFX_TRI, co2.EnableMAD),
        "co2.UseRawRef" => plain!(U16, MFX_TRI, co2.UseRawRef),
        // mfxExtCodingOption3
        "co3.NumSliceI" => plain!(U16, &[], co3.NumSliceI),
        "co3.NumSliceP" => plain!(U16, &[], co3.NumSliceP),
        "co3.NumSliceB" => plain!(U16, &[], co3.NumSliceB),
        "co3.WinBRCMaxAvgKbps" => plain!(U16, &[], co3.WinBRCMaxAvgKbps),
        "co3.WinBRCSize" => plain!(U16, &[], co3.WinBRCSize),
        "co3.QVBRQuality" => plain!(U16, &[], co3.QVBRQuality),
        "co3.EnableMBQP" => plain!(U16, MFX_TRI, co3.EnableMBQP),
        "co3.IntRefCycleDist" => plain!(U16, &[], co3.IntRefCycleDist),
        "co3.DirectBiasAdjustment" => plain!(U16, MFX_TRI, co3.DirectBiasAdjustment),
        "co3.GlobalMotionBiasAdjustment" => plain!(U16, MFX_TRI, co3.GlobalMotionBiasAdjustment),
        "co3.MVCostScalingFactor" => plain!(U16, &[], co3.MVCostScalingFactor),
        "co3.MBDisableSkipMap" => plain!(U16, MFX_TRI, co3.MBDisableSkipMap),
        "co3.WeightedPred" => plain!(U16, &[], co3.WeightedPred),
        "co3.WeightedBiPred" => plain!(U16, &[], co3.WeightedBiPred),
        "co3.AspectRatioInfoPresent" => plain!(U16, MFX_TRI, co3.AspectRatioInfoPresent),
        "co3.OverscanInfoPresent" => plain!(U16, MFX_TRI, co3.OverscanInfoPresent),
        "co3.OverscanAppropriate" => plain!(U16, MFX_TRI, co3.OverscanAppropriate),
        "co3.TimingInfoPresent" => plain!(U16, MFX_TRI, co3.TimingInfoPresent),
        "co3.BitstreamRestriction" => plain!(U16, MFX_TRI, co3.BitstreamRestriction),
        "co3.LowDelayHrd" => plain!(U16, MFX_TRI, co3.LowDelayHrd),
        "co3.MotionVectorsOverPicBoundaries" => {
            plain!(U16, MFX_TRI, co3.MotionVectorsOverPicBoundaries)
        }
        "co3.ScenarioInfo" => plain!(U16, MFX_SCENARIO, co3.ScenarioInfo),
        "co3.ContentInfo" => plain!(U16, MFX_CONTENT, co3.ContentInfo),
        "co3.PRefType" => plain!(U16, MFX_PREF, co3.PRefType),
        "co3.FadeDetection" => plain!(U16, MFX_TRI, co3.FadeDetection),
        "co3.GPB" => plain!(U16, MFX_TRI, co3.GPB),
        "co3.MaxFrameSizeI" => plain!(U32, &[], co3.MaxFrameSizeI),
        "co3.MaxFrameSizeP" => plain!(U32, &[], co3.MaxFrameSizeP),
        "co3.EnableQPOffset" => plain!(U16, MFX_TRI, co3.EnableQPOffset),
        "co3.QPOffset" => array!(I16, co3.QPOffset),
        "co3.NumRefActiveP" => array!(U16, co3.NumRefActiveP),
        "co3.NumRefActiveBL0" => array!(U16, co3.NumRefActiveBL0),
        "co3.NumRefActiveBL1" => array!(U16, co3.NumRefActiveBL1),
        "co3.TransformSkip" => plain!(U16, MFX_TRI, co3.TransformSkip),
        "co3.BRCPanicMode" => plain!(U16, MFX_TRI, co3.BRCPanicMode),
        "co3.LowDelayBRC" => plain!(U16, MFX_TRI, co3.LowDelayBRC),
        "co3.EnableMBForceIntra" => plain!(U16, MFX_TRI, co3.EnableMBForceIntra),
        "co3.AdaptiveMaxFrameSize" => plain!(U16, MFX_TRI, co3.AdaptiveMaxFrameSize),
        "co3.RepartitionCheckEnable" => plain!(U16, MFX_TRI, co3.RepartitionCheckEnable),
        "co3.EncodedUnitsInfo" => plain!(U16, MFX_TRI, co3.EncodedUnitsInfo),
        "co3.EnableNalUnitType" => plain!(U16, MFX_TRI, co3.EnableNalUnitType),
        "co3.AdaptiveLTR" | "co3.ExtBrcAdaptiveLTR" => plain!(U16, MFX_TRI, co3.AdaptiveLTR),
        "co3.AdaptiveCQM" => plain!(U16, MFX_TRI, co3.AdaptiveCQM),
        "co3.AdaptiveRef" => plain!(U16, MFX_TRI, co3.AdaptiveRef),
        _ => return None,
    })
}

/// Quick Sync hook, called right before `MFXVideoENCODE_Init` (after production's `Query`).
/// `get_video_param` is kept for the read-back.
pub(crate) fn qsv(
    session: mfxSession,
    get_video_param: GetVideoParamFn,
    par: &mut mfxVideoParam,
    co: &mut mfxExtCodingOption,
    co2: &mut mfxExtCodingOption2,
    co3: &mut mfxExtCodingOption3,
) {
    let mut g = ARMED.lock_recover();
    let Some(a) = g.as_mut().filter(|a| a.vendor == Vendor::Qsv) else {
        return;
    };
    a.fired += 1;
    a.qsv_session = Some((session as usize, get_video_param));
    let mut q = Q {
        par: *par,
        co: *co,
        co2: *co2,
        co3: *co3,
    };
    for (i, r) in a.report.iter_mut().enumerate() {
        if !r.outcome.applies() {
            continue;
        }
        let Some(f) = qsv_field(&r.field) else {
            continue;
        };
        match f.apply(&mut q, a.ints[i]) {
            Ok(()) => {
                r.outcome = Outcome::Applied;
                r.readback = Some(json!((f.get)(&q)));
            }
            Err(e) => {
                r.outcome = Outcome::Rejected;
                r.note = Some(e);
            }
        }
    }
    // the init params keep production's own ext-buffer pointers
    let (ext, n) = (par.ExtParam, par.NumExtParam);
    *par = q.par;
    par.ExtParam = ext;
    par.NumExtParam = n;
    let (h, h2, h3) = (co.Header, co2.Header, co3.Header);
    *co = q.co;
    *co2 = q.co2;
    *co3 = q.co3;
    co.Header = h;
    co2.Header = h2;
    co3.Header = h3;
}

fn load_x264_parse(path: &std::path::Path) -> Result<(libloading::Library, X264ParseFn), String> {
    // SAFETY: the same libx264 the production loader opened; loading it again returns the same
    // module, and `x264_param_parse` has had this signature since the API began.
    unsafe {
        let lib = libloading::Library::new(path).map_err(|e| e.to_string())?;
        let f: X264ParseFn = *lib
            .get::<X264ParseFn>(b"x264_param_parse\0")
            .map_err(|e| e.to_string())?;
        Ok((lib, f))
    }
}

fn x264_value(v: &Val) -> Result<String, String> {
    Ok(match v {
        Val::Bool(b) => (if *b { "1" } else { "0" }).into(),
        Val::Int(i) => i.to_string(),
        Val::Float(f) => f.to_string(),
        Val::Str(s) => s.clone(),
        Val::Null => return Err("null is not an x264 option value".into()),
        other => return Err(format!("{other:?} is not an x264 option value")),
    })
}

/// `X264_PARAM_BAD_NAME` / `X264_PARAM_BAD_VALUE`.
const X264_BAD_NAME: c_int = -1;
const X264_BAD_VALUE: c_int = -2;

fn x264_parse_into(
    parse: X264ParseFn,
    p: &mut x264_param_t,
    name: &str,
    value: &Val,
) -> Result<(), (Outcome, String)> {
    let v = x264_value(value).map_err(|e| (Outcome::Rejected, e))?;
    let n = CString::new(name).map_err(|_| (Outcome::Unknown, "bad name".into()))?;
    let vv = CString::new(v.clone()).map_err(|_| (Outcome::Rejected, "bad value".into()))?;
    // SAFETY: `p` is a live param struct; the strings outlive the call.
    let rv = unsafe { parse(p, n.as_ptr(), vv.as_ptr()) };
    match rv {
        0 => Ok(()),
        X264_BAD_NAME => Err((
            Outcome::Unknown,
            format!("x264_param_parse: no option '{name}'"),
        )),
        X264_BAD_VALUE => Err((
            Outcome::Rejected,
            format!("x264_param_parse: bad value '{v}' for '{name}'"),
        )),
        other => Err((
            Outcome::Rejected,
            format!("x264_param_parse returned {other}"),
        )),
    }
}

/// Ask libx264 whether the option exists and takes this value, on a scratch struct.
fn x264_check(parse: X264ParseFn, name: &str, value: &Val) -> Result<(), (Outcome, String)> {
    // a zeroed struct is enough: x264_param_parse writes fields, it does not validate the struct
    let mut p: x264_param_t = unsafe { std::mem::zeroed() };
    x264_parse_into(parse, &mut p, name, value)
}

/// x264 hook, called right before `x264_encoder_open` with the fully built params.
pub(crate) fn x264(p: &mut x264_param_t) {
    let mut g = ARMED.lock_recover();
    let Some(a) = g.as_mut().filter(|a| a.vendor == Vendor::X264) else {
        return;
    };
    a.fired += 1;
    let Some(parse) = a.x264_parse else {
        a.notes
            .push("x264_param_parse was not resolved; nothing applied".into());
        return;
    };
    for r in a.report.iter_mut() {
        if !r.outcome.applies() {
            continue;
        }
        match x264_parse_into(parse, p, &r.field, &r.value) {
            Ok(()) => r.outcome = Outcome::Applied,
            Err((o, e)) => {
                r.outcome = o;
                r.note = Some(e);
            }
        }
    }
}

/// Struct members the common options land in, for the read-back.
fn x264_readable(name: &str) -> Option<fn(&x264_param_t) -> f64> {
    Some(match name {
        "threads" => |p| p.i_threads as f64,
        "sliced-threads" => |p| p.b_sliced_threads as f64,
        "sync-lookahead" => |p| p.i_sync_lookahead as f64,
        "ref" => |p| p.i_frame_reference as f64,
        "keyint" => |p| p.i_keyint_max as f64,
        "min-keyint" => |p| p.i_keyint_min as f64,
        "scenecut" => |p| p.i_scenecut_threshold as f64,
        "intra-refresh" => |p| p.b_intra_refresh as f64,
        "bframes" => |p| p.i_bframe as f64,
        "b-adapt" => |p| p.i_bframe_adaptive as f64,
        "b-pyramid" => |p| p.i_bframe_pyramid as f64,
        "cabac" => |p| p.b_cabac as f64,
        "deblock" => |p| p.b_deblocking_filter as f64,
        "constrained-intra" => |p| p.b_constrained_intra as f64,
        "8x8dct" => |p| p.analyse.b_transform_8x8 as f64,
        "weightp" => |p| p.analyse.i_weighted_pred as f64,
        "me" => |p| p.analyse.i_me_method as f64,
        "merange" => |p| p.analyse.i_me_range as f64,
        "subme" => |p| p.analyse.i_subpel_refine as f64,
        "trellis" => |p| p.analyse.i_trellis as f64,
        "fast-pskip" => |p| p.analyse.b_fast_pskip as f64,
        "mixed-refs" => |p| p.analyse.b_mixed_references as f64,
        "psy" => |p| p.analyse.b_psy as f64,
        "qp" => |p| p.rc.i_qp_constant as f64,
        "qpmin" => |p| p.rc.i_qp_min as f64,
        "qpmax" => |p| p.rc.i_qp_max as f64,
        "qpstep" => |p| p.rc.i_qp_step as f64,
        "bitrate" => |p| p.rc.i_bitrate as f64,
        "crf" => |p| p.rc.f_rf_constant as f64,
        "vbv-maxrate" => |p| p.rc.i_vbv_max_bitrate as f64,
        "vbv-bufsize" => |p| p.rc.i_vbv_buffer_size as f64,
        "vbv-init" => |p| p.rc.f_vbv_buffer_init as f64,
        "aq-mode" => |p| p.rc.i_aq_mode as f64,
        "aq-strength" => |p| p.rc.f_aq_strength as f64,
        "mbtree" => |p| p.rc.b_mb_tree as f64,
        "rc-lookahead" => |p| p.rc.i_lookahead as f64,
        "ipratio" => |p| p.rc.f_ip_factor as f64,
        "filler" => |p| p.rc.b_filler as f64,
        "slices" => |p| p.i_slice_count as f64,
        "slices-max" => |p| p.i_slice_count_max as f64,
        "slice-max-size" => |p| p.i_slice_max_size as f64,
        "slice-max-mbs" => |p| p.i_slice_max_mbs as f64,
        "slice-min-mbs" => |p| p.i_slice_min_mbs as f64,
        "aud" => |p| p.b_aud as f64,
        "repeat-headers" => |p| p.b_repeat_headers as f64,
        "nal-hrd" => |p| p.i_nal_hrd as f64,
        _ => return None,
    })
}

/// What `name = value` alone puts in the struct member `get` reads.
fn x264_expected(
    parse: X264ParseFn,
    name: &str,
    value: &Val,
    get: fn(&x264_param_t) -> f64,
) -> Option<f64> {
    let mut p: x264_param_t = unsafe { std::mem::zeroed() };
    x264_parse_into(parse, &mut p, name, value).ok()?;
    Some(get(&p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(name: &str, v: Val) -> (String, Val) {
        (name.to_string(), v)
    }

    #[test]
    fn every_listed_nvenc_name_resolves_and_unknown_ones_do_not() {
        for n in NVENC_FIELDS {
            assert!(nvenc_field(n).is_some(), "{n}");
        }
        assert!(nvenc_field("rc.lowDelayKeyFramScale").is_none());
        assert!(nvenc_field("enableOutputInVidmem").is_none());
        let v = validate(
            Vendor::Nvenc,
            &[
                sv("rc.nope", Val::Int(1)),
                sv("enableOutputInVidmem", Val::Int(1)),
            ],
            None,
        );
        assert!(!v.ok());
        assert!(v.report.iter().all(|r| r.outcome == Outcome::Unknown));
        assert!(v.report[1]
            .note
            .as_deref()
            .unwrap()
            .contains("refused by the bench"));
    }

    #[test]
    fn values_parse_symbols_ranges_and_bitfield_widths() {
        let f = nvenc_field("rc.multiPass").unwrap();
        assert_eq!(f.parse(&Val::Str("QUARTER".into())), Ok(1));
        assert_eq!(
            f.parse(&Val::Str("NV_ENC_TWO_PASS_FULL_RESOLUTION".into())),
            Ok(2)
        );
        assert!(f.parse(&Val::Str("HALF".into())).is_err());
        let p = nvenc_field("preset").unwrap();
        assert_eq!(p.parse(&Val::Str("p4".into())), Ok(4));
        assert!(p.parse(&Val::Int(9)).is_err());
        let q = qsv_field("co3.ScenarioInfo").unwrap();
        assert_eq!(
            q.parse(&Val::Str("MFX_SCENARIO_DISPLAY_REMOTING".into())),
            Ok(1)
        );
        let lp = qsv_field("mfx.LowPower").unwrap();
        assert_eq!(lp.parse(&Val::Str("OFF".into())), Ok(0x20));
        assert_eq!(lp.parse(&Val::Bool(true)), Ok(1));
        let u8f = qsv_field("co2.MinQPP").unwrap();
        assert!(u8f.parse(&Val::Int(300)).is_err());
        assert!(u8f.parse(&Val::Float(1.5)).is_err());
    }

    #[test]
    fn nvenc_fields_apply_to_real_structures() {
        let mut nv = Nv {
            init: unsafe { pod_zeroed() },
            cfg: unsafe { pod_zeroed() },
        };
        let aq = nvenc_field("rc.aqStrength").unwrap();
        assert!(aq.apply(&mut nv, 15).is_ok());
        assert_eq!(nv.cfg.rcParams.aqStrength(), 15);
        // out-of-range never reaches `apply` through `parse`, but a direct write is still caught
        assert!(aq.apply(&mut nv, 16).is_err());
        assert_eq!(
            nv.cfg.rcParams.aqStrength(),
            15,
            "restored after a refused write"
        );
        let mp = nvenc_field("rc.multiPass").unwrap();
        mp.apply(&mut nv, 1).unwrap();
        assert_eq!(
            nv.cfg.rcParams.multiPass,
            NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION
        );
        let refs = nvenc_field("h264.maxNumRefFrames").unwrap();
        refs.apply(&mut nv, 4).unwrap();
        assert_eq!(
            unsafe { nv.cfg.encodeCodecConfig.h264Config.maxNumRefFrames },
            4
        );
        let ss = nvenc_field("h264.singleSliceIntraRefresh").unwrap();
        ss.apply(&mut nv, 1).unwrap();
        assert_eq!(
            unsafe {
                nv.cfg
                    .encodeCodecConfig
                    .h264Config
                    .singleSliceIntraRefresh()
            },
            1
        );
        let preset = nvenc_field("preset").unwrap();
        preset.apply(&mut nv, 3).unwrap();
        assert_eq!(
            nv.init.presetGUID,
            crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_PRESET_P3_GUID
        );
        let gop = nvenc_field("gopLength").unwrap();
        assert_eq!(gop.parse(&Val::Str("INFINITE".into())), Ok(u32::MAX as i64));
    }

    #[test]
    fn qsv_fields_apply_and_arrays_set_every_element() {
        let mut q = Q::zeroed();
        let tu = qsv_field("mfx.TargetUsage").unwrap();
        tu.apply(&mut q, 4).unwrap();
        assert_eq!(unsafe { q.par.info.mfx.TargetUsage }, 4);
        let arr = qsv_field("co3.NumRefActiveP").unwrap();
        arr.apply(&mut q, 2).unwrap();
        assert!(q.co3.NumRefActiveP.iter().all(|&x| x == 2));
        let qp = qsv_field("mfx.QPP").unwrap();
        qp.apply(&mut q, 30).unwrap();
        assert_eq!(
            unsafe { q.par.info.mfx.TargetKbps },
            30,
            "QPP aliases TargetKbps"
        );
        assert!(qsv_field("co3.Nope").is_none());
    }

    #[test]
    fn the_qsv_hook_applies_in_order_and_keeps_production_pointers() {
        let v = validate(
            Vendor::Qsv,
            &[
                sv("mfx.TargetUsage", Val::Int(4)),
                sv("co3.ScenarioInfo", Val::Str("DISPLAY_REMOTING".into())),
                sv("mfx.TargetUsage", Val::Int(5)),
            ],
            None,
        );
        assert!(v.ok());
        let mut q = Q::zeroed();
        let mut ptrs: [*mut mfxExtBuffer; 1] = [std::ptr::null_mut()];
        q.par.ExtParam = ptrs.as_mut_ptr();
        q.par.NumExtParam = 1;
        unsafe extern "C" fn gvp(_: mfxSession, _: *mut mfxVideoParam) -> mfxStatus {
            0
        }
        // not armed: untouched
        let _ = disarm();
        qsv(
            std::ptr::null_mut(),
            gvp,
            &mut q.par,
            &mut q.co,
            &mut q.co2,
            &mut q.co3,
        );
        assert_eq!(unsafe { q.par.info.mfx.TargetUsage }, 0);
        arm(Vendor::Qsv, v);
        let before_hdr = q.co3.Header.BufferId;
        let mut par = q.par;
        qsv(
            std::ptr::null_mut(),
            gvp,
            &mut par,
            &mut q.co,
            &mut q.co2,
            &mut q.co3,
        );
        let d = disarm().unwrap();
        assert_eq!(d.fired, 1);
        assert_eq!(unsafe { par.info.mfx.TargetUsage }, 5, "later entries win");
        assert_eq!(q.co3.ScenarioInfo, 1);
        assert_eq!(q.co3.Header.BufferId, before_hdr);
        assert_eq!(par.ExtParam, ptrs.as_mut_ptr());
        assert!(d.report.iter().all(|r| r.outcome == Outcome::Applied));
        let mut d = d;
        d.mark_rejected("MFX_ERR_INVALID_VIDEO_PARAM");
        assert!(d.report.iter().all(|r| r.outcome == Outcome::Rejected));
    }

    #[test]
    fn rebase_keeps_production_bits_and_takes_the_rest_from_the_new_preset() {
        let old: [u8; 4] = [0b0000_1111, 0, 7, 9];
        let prod: [u8; 4] = [0b0000_1010, 0, 7, 1]; // changed bits in bytes 0 and 3
        let new: [u8; 4] = [0b1111_0000, 5, 8, 9];
        let (out, kept) = rebase_bytes(&prod, &old, &new);
        assert_eq!(
            out[0], 0b1111_0000,
            "a bit production set to the old value is dropped"
        );
        assert_eq!(out[1], 5, "untouched by production: new preset");
        assert_eq!(out[2], 8, "equal to the old preset: new preset");
        assert_eq!(out[3], 1, "production's changed bit kept");
        assert_eq!(kept, 2 + 1);
    }

    #[test]
    fn x264_values_stringify() {
        assert_eq!(x264_value(&Val::Bool(true)).unwrap(), "1");
        assert_eq!(x264_value(&Val::Int(4)).unwrap(), "4");
        assert_eq!(x264_value(&Val::Str("hex".into())).unwrap(), "hex");
        assert!(x264_value(&Val::Null).is_err());
        assert!(x264_readable("rc-lookahead").is_some());
        assert!(x264_readable("nope").is_none());
    }
}

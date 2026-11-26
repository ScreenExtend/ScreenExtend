//! A fake AMF runtime behind the C vtables `amd_sys` binds, so the encoder's FFI and every one of
//! its reactions to a runtime answer can run on a machine with no AMD GPU.
//!
//! One `World` holds the component's property table (AMF's documented usage defaults and range
//! checks), checks the call order the runtime requires (`Usage` first, static properties before
//! `Init`, `Terminate` before a second `Init`, every object released exactly once) and runs the
//! script that decides each answer. A scripted answer is one the headers, AMD's documentation or
//! a public issue says a runtime gives; whether a given driver gives it needs real hardware.
//! The input textures are real, on a WARP device: no GPU, display or capture is touched.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::ptr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};

use crate::streamer::config::H264Profile;
use crate::windows_utils::streamer::amd::encoder::{
    AmfSwitches, Encoder, IdentityCounts, IdentitySource, NeedMoreInputMeaning, QpApplied,
    RateTier, MAX_LTR_FRAMES_SUPPORTED,
};
use crate::windows_utils::streamer::amd_sys::*;
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;
use crate::windows_utils::streamer::test::h264;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Var {
    Bool(bool),
    Int(i64),
    Size(i32, i32),
    Rate(u32, u32),
    Iface(usize),
    Other(i32),
}

impl Var {
    pub(crate) fn int(self) -> Option<i64> {
        match self {
            Var::Int(v) => Some(v),
            Var::Bool(b) => Some(i64::from(b)),
            _ => None,
        }
    }

    fn of(v: &AMFVariantStruct) -> Self {
        // SAFETY: the union member read is the one the tag names.
        unsafe {
            match v.r#type {
                AMF_VARIANT_BOOL => Var::Bool(v.value.boolValue != 0),
                AMF_VARIANT_INT64 => Var::Int(v.value.int64Value),
                AMF_VARIANT_SIZE => Var::Size(v.value.sizeValue.width, v.value.sizeValue.height),
                AMF_VARIANT_RATE => Var::Rate(v.value.rateValue.num, v.value.rateValue.den),
                AMF_VARIANT_INTERFACE => Var::Iface(v.value.pInterface as usize),
                t => Var::Other(t),
            }
        }
    }

    fn to_variant(self) -> AMFVariantStruct {
        match self {
            Var::Bool(b) => AMFVariantStruct::bool(b),
            Var::Int(v) => AMFVariantStruct::int64(v),
            Var::Size(w, h) => AMFVariantStruct::size(w, h),
            Var::Rate(n, d) => AMFVariantStruct::rate(n, d),
            Var::Iface(p) => AMFVariantStruct {
                r#type: AMF_VARIANT_INTERFACE,
                _pad: 0,
                value: AMFVariantValue {
                    pInterface: p as *mut c_void,
                },
            },
            Var::Other(_) => AMFVariantStruct::empty(),
        }
    }
}

/// What one submitted picture does.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    /// Returned by successive `SubmitInput` calls before the picture is accepted. The surface is
    /// not consumed by any of them.
    pub submit_refusals: Vec<AMF_RESULT>,
    /// What the accepting `SubmitInput` returns: `AMF_OK`, or `AMF_NEED_MORE_INPUT`.
    pub accepted_with: AMF_RESULT,
    /// `QueryOutput` calls that find nothing before the output is ready.
    pub not_ready: u32,
    pub out: Out,
    /// The runtime ignores `ForcePictureType = IDR` on this picture.
    pub ignore_forced_idr: bool,
    /// What an empty poll returns with its null pointer: `AMF_REPEAT`, or `AMF_OK`, which a
    /// runtime also does.
    pub not_ready_with: AMF_RESULT,
    /// Before this picture's own output, the runtime hands back a second copy of the previous
    /// picture's output: a stray the identity check must drop. Invented, to exercise that path.
    pub stray_before: bool,
    /// The picture comes back as this many output buffers (`OutputBufferType` SLICE, then
    /// SLICE_LAST), as it would if a runtime ignored `OutputMode = FRAME`.
    pub buffers: u32,
    /// The stray carries no identity at all: no property, PTS 0.
    pub stray_unlabelled: bool,
    /// This picture's output comes back without the surface's properties, PTS intact.
    pub drop_property: bool,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            submit_refusals: Vec::new(),
            accepted_with: AMF_OK,
            not_ready: 0,
            out: Out::Au,
            ignore_forced_idr: false,
            not_ready_with: AMF_REPEAT,
            stray_before: false,
            buffers: 1,
            stray_unlabelled: false,
            drop_property: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Out {
    /// An access unit.
    Au,
    /// A real output object carrying no bytes.
    Empty,
    /// Nothing, ever: the stall.
    Never,
    /// `QueryOutput` returns this code with a null pointer, as a lost device does.
    Fail(AMF_RESULT),
}

#[derive(Debug, Clone)]
pub(crate) struct Script {
    /// Names this runtime does not know: `HasProperty` is false and a write is `AMF_NOT_FOUND`.
    pub unknown: HashSet<&'static str>,
    /// Names it knows but refuses to take, with the code it refuses with.
    pub refused: HashMap<&'static str, AMF_RESULT>,
    /// Registered integer ranges; a write outside is `AMF_OUT_OF_RANGE`, as #407 shows.
    pub ranges: HashMap<&'static str, (i64, i64)>,
    pub frame_rate_max: u32,
    /// `GetProperty` errors right after `Usage = ULTRA_LOW_LATENCY`, AMD's probe for #410.
    pub ull_probe_fails: bool,
    /// `Init()` fails under ULTRA_LOW_LATENCY, the pre-RDNA failure itself.
    pub ull_init_fails: bool,
    /// `RateControlMethod = CONSTANT_QP` is accepted and reads back as latency-constrained VBR.
    pub cqp_refused: bool,
    /// `QueryOutput` waits for the timeout when it was set before `Init()`.
    pub query_timeout_honoured: bool,
    /// The SPS AMF writes when asked for parameter sets on the first IDR it emits by itself.
    pub headers_on_first_idr: bool,
    pub caps_max_refs: i64,
    pub caps_max_level: i64,
    pub caps_b_frames: bool,
    pub caps_query_timeout: bool,
    pub accel: AMF_ACCELERATION_TYPE,
    pub create_component: AMF_RESULT,
    pub sps_max_refs: u32,
    pub extradata: bool,
    pub frames: VecDeque<Frame>,
    /// Bytes of filler in each slice, so access units have a realistic size.
    pub slice_bytes: usize,
    /// Properties set on an input surface come back on its output buffer. FFmpeg's `amfenc.c`
    /// relies on it for its PTS and AMD's EncoderLatency sample for its start time; off models a
    /// runtime that does not.
    pub propagate_surface_properties: bool,
    /// Pictures that ask for statistics get `StatisticsFeedbackFrameQP`/`AvgQP` back (RX 5000
    /// and newer, per AMF's encode documentation); off models an older part.
    pub statistics_supported: bool,
    /// The runtime keeps the QPs it was initialised with and ignores live `QPI`/`QPP` writes
    /// until a `ReInit`. Nothing says a runtime does this; it is the world in which OBS's
    /// flush-and-re-init would be needed, and the one `set_qp`'s confirmation must catch.
    pub qp_live_ignored: bool,
    /// The input PTS comes back on the output buffer (`GetPts`). Off, outputs carry PTS 0.
    pub propagate_pts: bool,
    /// Every IDR's output comes back without the surface's properties, PTS intact: a runtime
    /// that rebuilds the buffer when it inserts parameter sets. Invented, to exercise that path.
    pub drop_property_on_idr: bool,
    /// What `Flush` returns.
    pub flush_result: AMF_RESULT,
    /// Refuse a write that would put `TargetBitrate` above `PeakBitrate`. No source says AMF
    /// checks the pair; this models a runtime that does.
    pub target_le_peak: bool,
}

impl Default for Script {
    fn default() -> Self {
        let mut ranges = HashMap::new();
        for (name, lo, hi) in [
            ("IDRPeriod", 0, 1000),
            ("IntraPeriod", 0, 1000),
            ("HeaderInsertionSpacing", 0, 1000),
            ("QPI", 0, 51),
            ("QPP", 0, 51),
            ("MinQP", 0, 51),
            ("MaxQP", 0, 51),
            ("MaxOfLTRFrames", 0, 2),
            ("InputQueueSize", 1, 32),
            // AMF documents this range for PeakBitrate only; TargetBitrate is assumed the same
            ("TargetBitrate", 10_000, 100_000_000),
            ("PeakBitrate", 10_000, 100_000_000),
            ("VBVBufferSize", 1_000, 100_000_000),
        ] {
            ranges.insert(name, (lo, hi));
        }
        Self {
            unknown: HashSet::new(),
            refused: HashMap::new(),
            ranges,
            frame_rate_max: 120,
            ull_probe_fails: false,
            ull_init_fails: false,
            cqp_refused: false,
            query_timeout_honoured: true,
            headers_on_first_idr: false,
            caps_max_refs: 4,
            caps_max_level: 52,
            caps_b_frames: false,
            caps_query_timeout: true,
            accel: AMF_ACCEL_HARDWARE,
            create_component: AMF_OK,
            sps_max_refs: 1,
            extradata: true,
            frames: VecDeque::new(),
            slice_bytes: 64,
            propagate_surface_properties: true,
            statistics_supported: true,
            qp_live_ignored: false,
            propagate_pts: true,
            drop_property_on_idr: false,
            flush_result: AMF_OK,
            target_le_peak: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Context,
    Component,
    Caps,
    IoCaps,
    Surface,
    Output,
    ExtraData,
}

/// What every fake object looks like to the encoder: a vtable pointer first.
#[repr(C)]
struct Obj {
    vtbl: *const c_void,
    world: *const RefCell<World>,
    id: usize,
}

#[repr(C)]
struct FactoryObj {
    vtbl: *const AMFFactoryVtbl,
    world: *const RefCell<World>,
}

struct ObjState {
    kind: Kind,
    refs: i64,
    over_released: u32,
    props: HashMap<String, Var>,
    bytes: Vec<u8>,
    /// `SetPts` on a surface; `GetPts` on an output buffer.
    pts: i64,
}

/// One component write, as the runtime saw it.
#[derive(Debug, Clone)]
pub(crate) struct Write {
    pub name: String,
    pub value: Var,
    pub result: AMF_RESULT,
    pub after_init: bool,
}

/// One accepted picture.
#[derive(Debug, Clone)]
pub(crate) struct Submitted {
    pub seq: u64,
    pub idr: bool,
    pub forced: bool,
    pub insert_sps: bool,
    pub insert_pps: bool,
    pub mark: Option<i64>,
    pub force_ltr: Option<i64>,
    pub qp: i64,
    pub target_bitrate: Option<i64>,
    pub statistics: bool,
}

/// The newest output's bytes, carried properties, IDR flag and PTS.
type LastOutput = (Vec<u8>, HashMap<String, Var>, bool, i64);

struct Pending {
    /// `None` for a stray copy the runtime holds no surface for.
    surface: Option<usize>,
    /// The input surface's properties, carried to the output buffer.
    props: HashMap<String, Var>,
    statistics: bool,
    pts: i64,
    /// `OutputBufferType` this output carries.
    buffer_type: i64,
    frame: Frame,
    au: Vec<u8>,
    idr: bool,
    mark: Option<i64>,
    force_ltr: Option<i64>,
    qp: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompState {
    Configured,
    Initialized,
}

struct Vtables {
    context: Box<AMFContextVtbl>,
    component: Box<AMFComponentVtbl>,
    caps: Box<AMFCapsVtbl>,
    iocaps: Box<AMFIOCapsVtbl>,
    surface: Box<AMFSurfaceVtbl>,
    buffer: Box<AMFBufferVtbl>,
    factory: Box<AMFFactoryVtbl>,
}

pub(crate) struct World {
    pub script: Script,
    objs: Vec<ObjState>,
    // boxed so the pointers handed to the encoder stay put when the Vec grows
    #[allow(clippy::vec_box)]
    boxes: Vec<Box<Obj>>,
    vt: Vtables,
    self_ptr: *const RefCell<World>,

    pub props: HashMap<String, Var>,
    pub writes: Vec<Write>,
    pub violations: Vec<String>,
    pub events: Vec<String>,
    pub submitted: Vec<Submitted>,
    pub query_calls: u64,
    pub inits: u32,
    pub failed_inits: u32,
    pub terminates: u32,
    pub drains: u32,
    pub init_format: Option<(AMF_SURFACE_FORMAT, i32, i32)>,
    pub crops: Vec<(i32, i32, i32, i32)>,
    state: CompState,
    usage: Option<i64>,
    query_timeout_before_init: bool,
    context_dx11: bool,
    pending: VecDeque<Pending>,
    current: Option<Frame>,
    seq: u64,
    since_init: u64,
    pub reinits: u32,
    pub flushes: u32,
    /// The QPs in force at the last `Init`/`ReInit`, for `qp_live_ignored`.
    init_qp: (i64, i64),
    /// The newest output handed back, for `stray_before`.
    last_output: Option<LastOutput>,
    ltr_slots: [bool; 4],
    draining: bool,
}

/// Properties the runtime only takes before `Init()`.
const STATIC_PROPS: &[&str] = &[
    "Usage",
    "FrameSize",
    "Profile",
    "ProfileLevel",
    "MaxOfLTRFrames",
    "LTRMode",
    "MaxNumRefFrames",
    "MaxConsecutiveBPictures",
    "InputFullRangeColor",
    "FullRangeColor",
    "LowLatencyInternal",
    "EnablePreAnalysis",
    "RateControlPreanalysisEnable",
    "RateControlMethod",
    "QualityPreset",
    "OutColorProfile",
    "OutColorTransferChar",
    "OutColorPrimaries",
    "OutMatrixCoeff",
    "OutputMode",
    "QueryTimeout",
];

const USAGE_ULL: i64 = 1;
const USAGE_LL: i64 = 2;

fn usage_defaults(usage: i64) -> Vec<(&'static str, Var)> {
    let ull = usage == USAGE_ULL;
    vec![
        ("QualityPreset", Var::Int(1)),
        ("RateControlMethod", Var::Int(if ull { 3 } else { 2 })),
        ("IDRPeriod", Var::Int(240)),
        ("IntraRefreshMBsNumberPerSlot", Var::Int(255)),
        ("EnforceHRD", Var::Bool(ull)),
        (
            "VBVBufferSize",
            Var::Int(if ull { 735_000 } else { 4_000_000 }),
        ),
        ("LowLatencyInternal", Var::Bool(ull)),
        ("RateControlPreanalysisEnable", Var::Int(0)),
        ("QueryTimeout", Var::Int(0)),
        ("BPicturesPattern", Var::Int(0)),
        ("TargetBitrate", Var::Int(20_000_000)),
        ("PeakBitrate", Var::Int(30_000_000)),
        ("FrameRate", Var::Rate(30, 1)),
        ("ProfileLevel", Var::Int(42)),
        ("QPI", Var::Int(22)),
        ("QPP", Var::Int(22)),
        ("MinQP", Var::Int(0)),
        ("MaxQP", Var::Int(51)),
        // VideoEncoderVCE.h: FullRangeColor "default = false"
        ("FullRangeColor", Var::Bool(false)),
    ]
}

fn wide(p: *const u16) -> String {
    let mut n = 0usize;
    // SAFETY: every name the encoder passes is a NUL-terminated static.
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

/// # Safety
/// `this` must be one of this module's objects.
unsafe fn obj<'a, T>(this: *mut T) -> &'a Obj {
    // SAFETY: the caller's contract; the box outlives every call.
    unsafe { &*(this as *const Obj) }
}

/// # Safety
/// `this` must be one of this module's objects, and no other borrow of the world may be live.
unsafe fn world<'a, T>(this: *mut T) -> std::cell::RefMut<'a, World> {
    // SAFETY: the caller's contract; the world outlives every object.
    unsafe { (*obj(this).world).borrow_mut() }
}

impl World {
    fn new_obj(&mut self, kind: Kind, vtbl: *const c_void) -> *mut Obj {
        let id = self.objs.len();
        self.objs.push(ObjState {
            kind,
            refs: 1,
            over_released: 0,
            props: HashMap::new(),
            bytes: Vec::new(),
            pts: 0,
        });
        let mut b = Box::new(Obj {
            vtbl,
            world: self.self_ptr,
            id,
        });
        let p: *mut Obj = &mut *b;
        self.boxes.push(b);
        p
    }

    fn release(&mut self, id: usize) -> i64 {
        let kind = self.objs[id].kind;
        let o = &mut self.objs[id];
        if o.refs <= 0 {
            o.over_released += 1;
            self.violations
                .push(format!("{kind:?} {id} released more often than acquired"));
            return 0;
        }
        o.refs -= 1;
        let refs = o.refs;
        if refs == 0 {
            self.events.push(format!("{kind:?}.released"));
            if kind == Kind::Component && self.state == CompState::Initialized {
                self.violations
                    .push("the component was released while still initialised".into());
            }
        }
        refs
    }

    fn knows(&self, name: &str) -> bool {
        !self.script.unknown.contains(name)
    }

    fn set_component(&mut self, name: String, v: Var) -> AMF_RESULT {
        let after_init = self.state == CompState::Initialized;
        let r = self.set_component_inner(&name, v, after_init);
        self.writes.push(Write {
            name,
            value: v,
            result: r,
            after_init,
        });
        r
    }

    fn set_component_inner(&mut self, name: &str, v: Var, after_init: bool) -> AMF_RESULT {
        if !self.knows(name) {
            return AMF_NOT_FOUND;
        }
        if let Some(&r) = self.script.refused.get(name) {
            return r;
        }
        if name != "Usage" && self.usage.is_none() {
            self.violations.push(format!(
                "{name} written before Usage, which would re-default it"
            ));
        }
        if after_init && STATIC_PROPS.contains(&name) {
            self.violations
                .push(format!("static property {name} written after Init"));
        }
        if let (Some(&(lo, hi)), Some(x)) = (self.script.ranges.get(name), v.int()) {
            if x < lo || x > hi {
                return AMF_OUT_OF_RANGE;
            }
        }
        if self.script.target_le_peak {
            let other = |n: &str| self.props.get(n).and_then(|v| v.int());
            match (name, v.int()) {
                ("TargetBitrate", Some(t)) if other("PeakBitrate").is_some_and(|p| t > p) => {
                    return AMF_INVALID_ARG
                }
                ("PeakBitrate", Some(p)) if other("TargetBitrate").is_some_and(|t| p < t) => {
                    return AMF_INVALID_ARG
                }
                _ => {}
            }
        }
        if name == "FrameRate" {
            match v {
                Var::Rate(n, d) if d > 0 && n >= d && n <= self.script.frame_rate_max * d => {}
                Var::Rate(..) => return AMF_OUT_OF_RANGE,
                _ => return AMF_INVALID_ARG,
            }
        }
        if name == "Usage" {
            let u = v.int().unwrap_or(-1);
            self.usage = Some(u);
            for (k, d) in usage_defaults(u) {
                self.props.insert(k.to_string(), d);
            }
        }
        let stored = if name == "RateControlMethod" && v.int() == Some(0) && self.script.cqp_refused
        {
            Var::Int(3)
        } else {
            v
        };
        self.props.insert(name.to_string(), stored);
        AMF_OK
    }

    fn get_component(&mut self, name: &str) -> Result<Var, AMF_RESULT> {
        if !self.knows(name) {
            return Err(AMF_NOT_FOUND);
        }
        if name == "RateControlPreanalysisEnable"
            && self.script.ull_probe_fails
            && self.usage == Some(USAGE_ULL)
        {
            return Err(AMF_NOT_SUPPORTED);
        }
        if name == "ExtraData" {
            if self.state != CompState::Initialized {
                return Err(AMF_NOT_INITIALIZED);
            }
            if !self.script.extradata {
                return Err(AMF_NOT_FOUND);
            }
            let vt = &*self.vt.buffer as *const AMFBufferVtbl as *const c_void;
            let p = self.new_obj(Kind::ExtraData, vt);
            let id = self.boxes.last().map(|b| b.id).unwrap_or(0);
            let mut bytes = self.sps_nal();
            bytes.extend_from_slice(&PPS);
            self.objs[id].bytes = bytes;
            return Ok(Var::Iface(p as usize));
        }
        self.props.get(name).copied().ok_or(AMF_NOT_FOUND)
    }

    fn int_prop(&self, name: &str) -> Option<i64> {
        self.props.get(name).and_then(|v| v.int())
    }

    fn sps_nal(&self) -> Vec<u8> {
        let (w, h) = match self.props.get("FrameSize") {
            Some(Var::Size(w, h)) => (*w as u32, *h as u32),
            _ => (1920, 1080),
        };
        let level = self.int_prop("ProfileLevel").unwrap_or(42) as u32;
        let mut out = vec![0, 0, 0, 1];
        out.extend_from_slice(&build_sps(w, h, level, self.script.sps_max_refs));
        out
    }

    fn submit(&mut self, surface: usize) -> AMF_RESULT {
        if self.state != CompState::Initialized {
            self.violations.push("SubmitInput before Init".into());
            return AMF_NOT_INITIALIZED;
        }
        if self.draining {
            self.violations.push("SubmitInput after Drain".into());
            return AMF_EOF;
        }
        if self.objs[surface].kind != Kind::Surface {
            self.violations
                .push("SubmitInput with something that is not a surface".into());
            return AMF_INVALID_ARG;
        }
        if self.current.is_none() {
            let frame = self.script.frames.pop_front().unwrap_or_default();
            if frame.stray_before {
                if let Some((au, props, idr, pts)) = self.last_output.clone() {
                    let (props, pts) = if frame.stray_unlabelled {
                        (HashMap::new(), 0)
                    } else {
                        (props, pts)
                    };
                    self.pending.push_front(Pending {
                        surface: None,
                        props,
                        statistics: false,
                        pts,
                        buffer_type: 0,
                        frame: Frame::default(),
                        au,
                        idr,
                        mark: None,
                        force_ltr: None,
                        qp: 0,
                    });
                }
            }
            self.current = Some(frame);
        }
        let frame = self.current.as_mut().expect("loaded above");
        if !frame.submit_refusals.is_empty() {
            return frame.submit_refusals.remove(0);
        }
        let frame = self.current.take().expect("loaded above");
        let sp = self.objs[surface].props.clone();
        let int = |n: &str| sp.get(n).and_then(|v| v.int());
        let forced = int("ForcePictureType") == Some(AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR as i64);
        let insert_sps = int("InsertSPS") == Some(1);
        let insert_pps = int("InsertPPS") == Some(1);
        let mark = int("MarkCurrentWithLTRIndex");
        let force_ltr = int("ForceLTRReferenceBitfield");
        let statistics = int("StatisticsFeedback") == Some(1);
        if int("InsertAUD") == Some(1) {
            self.violations.push("an AUD was asked for".into());
        }
        let first = self.since_init == 0;
        let idr = first || (forced && !frame.ignore_forced_idr);

        let ltr_allowed = self.int_prop("MaxOfLTRFrames").unwrap_or(0);
        if let Some(bits) = force_ltr.filter(|&b| b != 0) {
            for slot in 0..4 {
                if bits & (1 << slot) != 0 && !self.ltr_slots[slot] {
                    self.violations.push(format!(
                        "picture {} forced a reference to empty LTR slot {slot}",
                        self.seq
                    ));
                }
            }
        }
        if idr {
            self.ltr_slots = [false; 4];
        }
        if let Some(m) = mark.filter(|&m| m >= 0) {
            if m >= ltr_allowed {
                self.violations
                    .push(format!("mark {m} with MaxOfLTRFrames={ltr_allowed}"));
            } else {
                self.ltr_slots[m as usize] = true;
            }
        }

        let cqp = self.int_prop("RateControlMethod") == Some(0);
        let qp = if cqp && self.script.qp_live_ignored {
            if idr {
                self.init_qp.0
            } else {
                self.init_qp.1
            }
        } else if cqp {
            self.int_prop(if idr { "QPI" } else { "QPP" }).unwrap_or(22)
        } else {
            self.int_prop("MinQP").unwrap_or(0).max(20)
        };
        let headers = idr && (insert_sps || (first && self.script.headers_on_first_idr));
        let mut au = Vec::new();
        if headers {
            au.extend_from_slice(&self.sps_nal());
        }
        if idr && (insert_pps || (first && self.script.headers_on_first_idr)) {
            au.extend_from_slice(&PPS);
        }
        au.extend_from_slice(&[0, 0, 0, 1, if idr { 0x65 } else { 0x41 }, 0x88]);
        // the slice carries the frame identity, the encode it came from (a second encode of the
        // same surface differs, as a real one would) and the QP, so a test can tell them apart
        let label = int("ScreenExtendFrameId").unwrap_or(self.seq as i64);
        au.extend_from_slice(format!("f{}s{}q{}", label, self.seq, qp).as_bytes());
        au.extend(std::iter::repeat_n(0xAAu8, self.script.slice_bytes));

        self.submitted.push(Submitted {
            seq: self.seq,
            idr,
            forced,
            insert_sps,
            insert_pps,
            mark,
            force_ltr,
            qp,
            target_bitrate: self.int_prop("TargetBitrate"),
            statistics,
        });
        self.objs[surface].refs += 1;
        let accepted_with = frame.accepted_with;
        let props = if self.script.propagate_surface_properties
            && !frame.drop_property
            && !(idr && self.script.drop_property_on_idr)
        {
            sp.clone()
        } else {
            HashMap::new()
        };
        let pts = if self.script.propagate_pts {
            self.objs[surface].pts
        } else {
            0
        };
        // several buffers: the first carries everything up to the slice, each further one a
        // slice of its own; only the last releases the surface
        let parts = frame.buffers.max(1);
        for part in 1..parts {
            let mut extra = vec![0, 0, 0, 1, 0x41, 0x88];
            extra.extend_from_slice(format!("f{label}s{}q{qp}", self.seq).as_bytes());
            let first = part == 1;
            self.pending.push_back(Pending {
                surface: None,
                props: props.clone(),
                statistics,
                pts,
                buffer_type: 1,
                frame: frame.clone(),
                au: if first { au.clone() } else { extra.clone() },
                idr,
                mark,
                force_ltr,
                qp,
            });
            if first {
                au = extra;
            }
        }
        self.pending.push_back(Pending {
            surface: Some(surface),
            props,
            statistics,
            pts,
            buffer_type: if parts > 1 { 2 } else { 0 },
            frame,
            au,
            idr,
            mark,
            force_ltr,
            qp,
        });
        self.seq += 1;
        self.since_init += 1;
        accepted_with
    }

    fn honoured_wait(&self) {
        if self.script.query_timeout_honoured
            && self.query_timeout_before_init
            && self.int_prop("QueryTimeout").unwrap_or(0) > 0
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn query(&mut self) -> (AMF_RESULT, *mut Obj) {
        self.query_calls += 1;
        if self.state != CompState::Initialized {
            return (AMF_NOT_INITIALIZED, ptr::null_mut());
        }
        let Some(front) = self.pending.front_mut() else {
            if self.draining {
                return (AMF_EOF, ptr::null_mut());
            }
            self.honoured_wait();
            return (AMF_REPEAT, ptr::null_mut());
        };
        match front.frame.out {
            Out::Never => {
                self.honoured_wait();
                return (AMF_REPEAT, ptr::null_mut());
            }
            _ if front.frame.not_ready > 0 => {
                front.frame.not_ready -= 1;
                let code = front.frame.not_ready_with;
                self.honoured_wait();
                return (code, ptr::null_mut());
            }
            _ => {}
        }
        let p = self.pending.pop_front().expect("front exists");
        if let Some(s) = p.surface {
            self.release(s);
        }
        match p.frame.out {
            Out::Fail(code) => (code, ptr::null_mut()),
            Out::Never => unreachable!(),
            Out::Empty | Out::Au => {
                let vt = &*self.vt.buffer as *const AMFBufferVtbl as *const c_void;
                let o = self.new_obj(Kind::Output, vt);
                let id = self.objs.len() - 1;
                if p.frame.out == Out::Au {
                    self.last_output = Some((p.au.clone(), p.props.clone(), p.idr, p.pts));
                }
                let statistics = p.statistics && self.script.statistics_supported;
                let st = &mut self.objs[id];
                st.props = p.props;
                st.pts = p.pts;
                st.props
                    .insert("OutputBufferType".into(), Var::Int(p.buffer_type));
                if p.frame.out == Out::Au {
                    st.bytes = p.au;
                }
                st.props
                    .insert("OutputDataType".into(), Var::Int(if p.idr { 0 } else { 2 }));
                st.props
                    .insert("MarkedLTRIndex".into(), Var::Int(p.mark.unwrap_or(-1)));
                st.props.insert(
                    "ReferencedLTRIndexBitfield".into(),
                    Var::Int(p.force_ltr.unwrap_or(0)),
                );
                if statistics {
                    st.props
                        .insert("StatisticsFeedbackFrameQP".into(), Var::Int(p.qp));
                    st.props
                        .insert("StatisticsFeedbackAvgQP".into(), Var::Int(p.qp));
                }
                (AMF_OK, o)
            }
        }
    }

    fn drop_pending(&mut self) {
        while let Some(p) = self.pending.pop_front() {
            if let Some(s) = p.surface {
                self.release(s);
            }
        }
        self.current = None;
    }
}

const PPS: [u8; 8] = [0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80];

/// A Main-profile SPS with no VUI, `pic_order_cnt_type = 2` (what `LowLatencyInternal` selects),
/// the shape AMF is reported to write.
pub(crate) fn build_sps(width: u32, height: u32, level: u32, max_refs: u32) -> Vec<u8> {
    let mut w = Bits::default();
    w.put(77, 8); // profile_idc: Main
    w.put(0, 8); // constraint flags
    w.put(level, 8);
    w.ue(0); // seq_parameter_set_id
    w.ue(0); // log2_max_frame_num_minus4
    w.ue(2); // pic_order_cnt_type
    w.ue(max_refs);
    w.put(0, 1); // gaps_in_frame_num_value_allowed_flag
    let (mbw, mbh) = (width.div_ceil(16), height.div_ceil(16));
    w.ue(mbw - 1);
    w.ue(mbh - 1);
    w.put(1, 1); // frame_mbs_only_flag
    w.put(1, 1); // direct_8x8_inference_flag
    let (cr, cb) = ((mbw * 16 - width) / 2, (mbh * 16 - height) / 2);
    if cr != 0 || cb != 0 {
        w.put(1, 1);
        w.ue(0);
        w.ue(cr);
        w.ue(0);
        w.ue(cb);
    } else {
        w.put(0, 1);
    }
    w.put(0, 1); // vui_parameters_present_flag
    w.put(1, 1); // rbsp_stop_one_bit
    let mut rbsp = w.finish();
    let mut out = vec![0x67];
    let mut zeros = 0;
    for b in rbsp.drain(..) {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    cur: u8,
    n: u32,
}

impl Bits {
    fn put(&mut self, v: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.cur = (self.cur << 1) | ((v >> i) & 1) as u8;
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.cur);
                self.cur = 0;
                self.n = 0;
            }
        }
    }

    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.put(0, len - 1);
        for i in (0..len).rev() {
            self.put(((x >> i) & 1) as u32, 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.cur <<= 8 - self.n;
            self.out.push(self.cur);
        }
        self.out
    }
}

unsafe extern "system" fn acquire<T>(this: *mut T) -> amf_long {
    // SAFETY: `this` is one of our objects.
    let (mut w, id) = unsafe { (world(this), obj(this).id) };
    w.objs[id].refs += 1;
    w.objs[id].refs as amf_long
}

unsafe extern "system" fn release_fn<T>(this: *mut T) -> amf_long {
    // SAFETY: `this` is one of our objects.
    let (mut w, id) = unsafe { (world(this), obj(this).id) };
    w.release(id) as amf_long
}

unsafe extern "system" fn query_interface_fn<T>(
    this: *mut T,
    iid: *const AMFGuid,
    out: *mut *mut c_void,
) -> AMF_RESULT {
    // SAFETY: `this` is one of our objects; `iid` and `out` are the caller's live locals.
    unsafe {
        let mut w = world(this);
        let id = obj(this).id;
        let is_buffer = matches!(w.objs[id].kind, Kind::Output | Kind::ExtraData);
        if is_buffer && *iid == IID_AMF_BUFFER {
            w.objs[id].refs += 1;
            *out = this.cast();
            AMF_OK
        } else {
            *out = ptr::null_mut();
            AMF_NO_INTERFACE
        }
    }
}

unsafe extern "system" fn set_prop<T>(
    this: *mut T,
    name: *const u16,
    value: AMFVariantStruct,
) -> AMF_RESULT {
    // SAFETY: `this` is one of our objects and `name` a NUL-terminated static.
    unsafe {
        let mut w = world(this);
        let id = obj(this).id;
        let name = wide(name);
        let v = Var::of(&value);
        if w.objs[id].kind == Kind::Component {
            w.set_component(name, v)
        } else {
            w.objs[id].props.insert(name, v);
            AMF_OK
        }
    }
}

unsafe extern "system" fn get_prop<T>(
    this: *mut T,
    name: *const u16,
    out: *mut AMFVariantStruct,
) -> AMF_RESULT {
    // SAFETY: `this` is one of our objects; `out` is the caller's live local.
    unsafe {
        let mut w = world(this);
        let id = obj(this).id;
        let name = wide(name);
        let got = match w.objs[id].kind {
            Kind::Component => w.get_component(&name),
            Kind::Caps => caps_prop(&w.script, &name),
            _ => w.objs[id].props.get(&name).copied().ok_or(AMF_NOT_FOUND),
        };
        match got {
            Ok(v) => {
                *out = v.to_variant();
                AMF_OK
            }
            Err(r) => r,
        }
    }
}

unsafe extern "system" fn has_prop<T>(this: *mut T, name: *const u16) -> amf_bool {
    // SAFETY: `this` is one of our objects and `name` a NUL-terminated static.
    unsafe {
        let w = world(this);
        let id = obj(this).id;
        let name = wide(name);
        let has = match w.objs[id].kind {
            Kind::Component => w.knows(&name),
            _ => w.objs[id].props.contains_key(&name),
        };
        amf_bool::from(has)
    }
}

fn caps_prop(script: &Script, name: &str) -> Result<Var, AMF_RESULT> {
    Ok(match name {
        "MaxBitrate" => Var::Int(100_000_000),
        "NumOfStreams" => Var::Int(16),
        "MaxProfile" => Var::Int(100),
        "MaxLevel" => Var::Int(script.caps_max_level),
        "BFrames" => Var::Bool(script.caps_b_frames),
        "MaxReferenceFrames" => Var::Int(script.caps_max_refs),
        "NumOfHwInstances" => Var::Int(1),
        "ColorConversion" => Var::Int(AMF_ACCEL_GPU as i64),
        "PreAnalysis" => Var::Bool(true),
        "MaxThroughput" => Var::Int(2_000_000),
        "RequestedThroughput" => Var::Int(0),
        "QueryTimeoutSupport" => Var::Bool(script.caps_query_timeout),
        "SupportSliceOutput" => Var::Bool(true),
        _ => return Err(AMF_NOT_FOUND),
    })
}

unsafe extern "system" fn comp_init(
    this: *mut AMFComponent,
    format: AMF_SURFACE_FORMAT,
    width: amf_int32,
    height: amf_int32,
) -> AMF_RESULT {
    // SAFETY: `this` is our component.
    let mut w = unsafe { world(this) };
    w.events.push("Component.Init".into());
    if w.state == CompState::Initialized {
        w.violations.push("Init on an initialised component".into());
        return AMF_ALREADY_INITIALIZED;
    }
    if w.usage.is_none() {
        w.violations.push("Init with no Usage".into());
    }
    if w.script.ull_init_fails && w.usage == Some(USAGE_ULL) {
        w.failed_inits += 1;
        return AMF_FAIL;
    }
    w.state = CompState::Initialized;
    w.inits += 1;
    w.since_init = 0;
    w.ltr_slots = [false; 4];
    w.draining = false;
    w.query_timeout_before_init = w.int_prop("QueryTimeout").unwrap_or(0) > 0;
    w.init_qp = (
        w.int_prop("QPI").unwrap_or(22),
        w.int_prop("QPP").unwrap_or(22),
    );
    w.init_format = Some((format, width, height));
    AMF_OK
}

unsafe extern "system" fn comp_terminate(this: *mut AMFComponent) -> AMF_RESULT {
    // SAFETY: `this` is our component.
    let mut w = unsafe { world(this) };
    w.events.push("Component.Terminate".into());
    if w.state != CompState::Initialized {
        return AMF_NOT_INITIALIZED;
    }
    w.drop_pending();
    w.state = CompState::Configured;
    w.terminates += 1;
    w.draining = false;
    AMF_OK
}

unsafe extern "system" fn comp_drain(this: *mut AMFComponent) -> AMF_RESULT {
    // SAFETY: `this` is our component.
    let mut w = unsafe { world(this) };
    w.events.push("Component.Drain".into());
    if w.state != CompState::Initialized {
        return AMF_NOT_INITIALIZED;
    }
    w.drains += 1;
    w.draining = true;
    AMF_OK
}

unsafe extern "system" fn comp_flush(this: *mut AMFComponent) -> AMF_RESULT {
    // SAFETY: `this` is our component.
    let mut w = unsafe { world(this) };
    w.events.push("Component.Flush".into());
    w.flushes += 1;
    if w.script.flush_result != AMF_OK {
        return w.script.flush_result;
    }
    w.drop_pending();
    AMF_OK
}

/// OBS's QP change: `Flush` then `ReInit(width, height)` on an initialised component, which
/// starts a new sequence, so the next picture is an IDR (texture-amf.cpp `amf_avc_update_data`).
unsafe extern "system" fn comp_reinit(
    this: *mut AMFComponent,
    _width: amf_int32,
    _height: amf_int32,
) -> AMF_RESULT {
    // SAFETY: `this` is our component.
    let mut w = unsafe { world(this) };
    w.events.push("Component.ReInit".into());
    if w.state != CompState::Initialized {
        return AMF_NOT_INITIALIZED;
    }
    w.drop_pending();
    w.reinits += 1;
    w.since_init = 0;
    w.ltr_slots = [false; 4];
    w.init_qp = (
        w.int_prop("QPI").unwrap_or(22),
        w.int_prop("QPP").unwrap_or(22),
    );
    AMF_OK
}

unsafe extern "system" fn comp_submit(this: *mut AMFComponent, data: *mut AMFData) -> AMF_RESULT {
    // SAFETY: `this` is our component and `data` one of our surfaces.
    unsafe {
        let id = obj(data).id;
        world(this).submit(id)
    }
}

unsafe extern "system" fn comp_query(
    this: *mut AMFComponent,
    out: *mut *mut AMFData,
) -> AMF_RESULT {
    // SAFETY: `this` is our component; `out` is the caller's live local.
    unsafe {
        let (r, o) = world(this).query();
        *out = o.cast();
        r
    }
}

unsafe extern "system" fn comp_get_caps(
    this: *mut AMFComponent,
    out: *mut *mut AMFCaps,
) -> AMF_RESULT {
    // SAFETY: `this` is our component; `out` is the caller's live local.
    unsafe {
        let mut w = world(this);
        let vt = &*w.vt.caps as *const AMFCapsVtbl as *const c_void;
        *out = w.new_obj(Kind::Caps, vt).cast();
        AMF_OK
    }
}

unsafe extern "system" fn caps_accel(this: *mut AMFCaps) -> AMF_ACCELERATION_TYPE {
    // SAFETY: `this` is our caps object.
    unsafe { world(this).script.accel }
}

unsafe extern "system" fn caps_input(this: *mut AMFCaps, out: *mut *mut AMFIOCaps) -> AMF_RESULT {
    // SAFETY: `this` is our caps object; `out` is the caller's live local.
    unsafe {
        let mut w = world(this);
        let vt = &*w.vt.iocaps as *const AMFIOCapsVtbl as *const c_void;
        *out = w.new_obj(Kind::IoCaps, vt).cast();
        AMF_OK
    }
}

unsafe extern "system" fn io_width(_this: *mut AMFIOCaps, lo: *mut amf_int32, hi: *mut amf_int32) {
    // SAFETY: the caller's live locals.
    unsafe {
        *lo = 64;
        *hi = 4096;
    }
}

unsafe extern "system" fn io_height(_this: *mut AMFIOCaps, lo: *mut amf_int32, hi: *mut amf_int32) {
    // SAFETY: the caller's live locals.
    unsafe {
        *lo = 64;
        *hi = 2304;
    }
}

unsafe extern "system" fn io_align(_this: *mut AMFIOCaps) -> amf_int32 {
    16
}

unsafe extern "system" fn io_count(_this: *mut AMFIOCaps) -> amf_int32 {
    2
}

unsafe extern "system" fn io_format(
    _this: *mut AMFIOCaps,
    i: amf_int32,
    format: *mut AMF_SURFACE_FORMAT,
    native: *mut amf_bool,
) -> AMF_RESULT {
    // SAFETY: the caller's live locals.
    unsafe {
        match i {
            0 => {
                *format = AMF_SURFACE_NV12;
                *native = 1;
            }
            1 => {
                *format = AMF_SURFACE_BGRA;
                *native = 0;
            }
            _ => return AMF_OUT_OF_RANGE,
        }
    }
    AMF_OK
}

unsafe extern "system" fn ctx_terminate(this: *mut AMFContext) -> AMF_RESULT {
    // SAFETY: `this` is our context.
    let mut w = unsafe { world(this) };
    w.events.push("Context.Terminate".into());
    let live_component = w
        .objs
        .iter()
        .any(|o| o.kind == Kind::Component && o.refs > 0);
    if live_component {
        w.violations
            .push("the context was terminated while its component was alive".into());
    }
    AMF_OK
}

unsafe extern "system" fn ctx_init_dx11(
    this: *mut AMFContext,
    device: *mut c_void,
    _version: AMF_DX_VERSION,
) -> AMF_RESULT {
    // SAFETY: `this` is our context.
    let mut w = unsafe { world(this) };
    if device.is_null() {
        w.violations.push("InitDX11 with a null device".into());
        return AMF_INVALID_ARG;
    }
    w.context_dx11 = true;
    AMF_OK
}

unsafe extern "system" fn ctx_wrap(
    this: *mut AMFContext,
    texture: *mut c_void,
    out: *mut *mut AMFSurface,
    _observer: *mut c_void,
) -> AMF_RESULT {
    // SAFETY: `this` is our context; `out` is the caller's live local.
    unsafe {
        let mut w = world(this);
        if !w.context_dx11 {
            w.violations
                .push("a DX11 surface wrapped on a context with no DX11 device".into());
        }
        if texture.is_null() {
            return AMF_INVALID_ARG;
        }
        let vt = &*w.vt.surface as *const AMFSurfaceVtbl as *const c_void;
        *out = w.new_obj(Kind::Surface, vt).cast();
        AMF_OK
    }
}

unsafe extern "system" fn surf_set_pts(this: *mut AMFSurface, pts: amf_pts) {
    // SAFETY: `this` is our surface.
    unsafe {
        let id = obj(this).id;
        world(this).objs[id].pts = pts;
    }
}

unsafe extern "system" fn surf_duration(_this: *mut AMFSurface, _duration: amf_pts) {}

unsafe extern "system" fn surf_format(_this: *mut AMFSurface) -> AMF_SURFACE_FORMAT {
    AMF_SURFACE_BGRA
}

unsafe extern "system" fn surf_frame_type(_this: *mut AMFSurface, _t: AMF_FRAME_TYPE) {}

unsafe extern "system" fn surf_crop(
    this: *mut AMFSurface,
    x: amf_int32,
    y: amf_int32,
    w: amf_int32,
    h: amf_int32,
) -> AMF_RESULT {
    // SAFETY: `this` is our surface.
    unsafe { world(this).crops.push((x, y, w, h)) };
    AMF_OK
}

unsafe extern "system" fn buf_pts(this: *mut AMFBuffer) -> amf_pts {
    // SAFETY: `this` is our buffer.
    unsafe {
        let w = world(this);
        w.objs[obj(this).id].pts
    }
}

unsafe extern "system" fn buf_size(this: *mut AMFBuffer) -> amf_size {
    // SAFETY: `this` is our buffer.
    unsafe {
        let w = world(this);
        w.objs[obj(this).id].bytes.len()
    }
}

unsafe extern "system" fn buf_native(this: *mut AMFBuffer) -> *mut c_void {
    // SAFETY: `this` is our buffer; its bytes live as long as the world.
    unsafe {
        let mut w = world(this);
        let id = obj(this).id;
        w.objs[id].bytes.as_mut_ptr().cast()
    }
}

unsafe extern "system" fn fac_context(
    this: *mut AMFFactory,
    out: *mut *mut AMFContext,
) -> AMF_RESULT {
    // SAFETY: `this` is our factory; `out` is the caller's live local.
    unsafe {
        let f = &*(this as *const FactoryObj);
        let mut w = (*f.world).borrow_mut();
        let vt = &*w.vt.context as *const AMFContextVtbl as *const c_void;
        *out = w.new_obj(Kind::Context, vt).cast();
        AMF_OK
    }
}

unsafe extern "system" fn fac_component(
    this: *mut AMFFactory,
    context: *mut AMFContext,
    id: *const u16,
    out: *mut *mut AMFComponent,
) -> AMF_RESULT {
    // SAFETY: `this` is our factory; `out` is the caller's live local.
    unsafe {
        let f = &*(this as *const FactoryObj);
        let mut w = (*f.world).borrow_mut();
        if context.is_null() {
            w.violations.push("CreateComponent with no context".into());
        }
        if wide(id) != "AMFVideoEncoderVCE_AVC" {
            w.violations.push(format!("CreateComponent({})", wide(id)));
            return AMF_CODEC_NOT_SUPPORTED;
        }
        if w.script.create_component != AMF_OK {
            return w.script.create_component;
        }
        let vt = &*w.vt.component as *const AMFComponentVtbl as *const c_void;
        *out = w.new_obj(Kind::Component, vt).cast();
        AMF_OK
    }
}

unsafe extern "system" fn fac_trace(_this: *mut AMFFactory, out: *mut *mut AMFTrace) -> AMF_RESULT {
    // SAFETY: the caller's live local.
    unsafe { *out = ptr::null_mut() };
    AMF_NOT_SUPPORTED
}

fn vp<T>(f: T) -> *const c_void
where
    T: Copy,
{
    // SAFETY: only ever called with a function pointer, which is pointer-sized.
    unsafe { std::mem::transmute_copy::<T, *const c_void>(&f) }
}

fn vtables() -> Vtables {
    let n = ptr::null::<c_void>();
    let st_set = vp(set_prop::<AMFPropertyStorage> as unsafe extern "system" fn(_, _, _) -> _);
    let st_get = vp(get_prop::<AMFPropertyStorage> as unsafe extern "system" fn(_, _, _) -> _);
    let st_has = vp(has_prop::<AMFPropertyStorage> as unsafe extern "system" fn(_, _) -> _);
    let qi = vp(query_interface_fn::<AMFInterface> as unsafe extern "system" fn(_, _, _) -> _);
    Vtables {
        context: Box::new(AMFContextVtbl {
            Acquire: acquire::<AMFContext>,
            Release: release_fn::<AMFContext>,
            QueryInterface: query_interface_fn::<AMFContext>,
            SetProperty: n,
            GetProperty: n,
            HasProperty: n,
            GetPropertyCount: n,
            GetPropertyAt: n,
            Clear: n,
            AddTo: n,
            CopyTo: n,
            AddObserver: n,
            RemoveObserver: n,
            Terminate: ctx_terminate,
            InitDX9: n,
            GetDX9Device: n,
            LockDX9: n,
            UnlockDX9: n,
            InitDX11: ctx_init_dx11,
            GetDX11Device: n,
            LockDX11: n,
            UnlockDX11: n,
            InitOpenCL: n,
            GetOpenCLContext: n,
            GetOpenCLCommandQueue: n,
            GetOpenCLDeviceID: n,
            GetOpenCLComputeFactory: n,
            InitOpenCLEx: n,
            LockOpenCL: n,
            UnlockOpenCL: n,
            InitOpenGL: n,
            GetOpenGLContext: n,
            GetOpenGLDrawable: n,
            LockOpenGL: n,
            UnlockOpenGL: n,
            InitXV: n,
            GetXVDevice: n,
            LockXV: n,
            UnlockXV: n,
            InitGralloc: n,
            GetGrallocDevice: n,
            LockGralloc: n,
            UnlockGralloc: n,
            AllocBuffer: n,
            AllocSurface: n,
            AllocAudioBuffer: n,
            CreateBufferFromHostNative: n,
            CreateSurfaceFromHostNative: n,
            CreateSurfaceFromDX9Native: n,
            CreateSurfaceFromDX11Native: ctx_wrap,
            CreateSurfaceFromOpenGLNative: n,
            CreateSurfaceFromGrallocNative: n,
            CreateSurfaceFromOpenCLNative: n,
            CreateBufferFromOpenCLNative: n,
            GetCompute: n,
        }),
        component: Box::new(AMFComponentVtbl {
            Acquire: acquire::<AMFComponent>,
            Release: release_fn::<AMFComponent>,
            QueryInterface: n,
            SetProperty: set_prop::<AMFComponent>,
            GetProperty: get_prop::<AMFComponent>,
            HasProperty: has_prop::<AMFComponent>,
            GetPropertyCount: n,
            GetPropertyAt: n,
            Clear: n,
            AddTo: n,
            CopyTo: n,
            AddObserver: n,
            RemoveObserver: n,
            GetPropertiesInfoCount: n,
            GetPropertyInfoAt: n,
            GetPropertyInfo: n,
            ValidateProperty: n,
            Init: comp_init,
            ReInit: comp_reinit,
            Terminate: comp_terminate,
            Drain: comp_drain,
            Flush: comp_flush,
            SubmitInput: comp_submit,
            QueryOutput: comp_query,
            GetContext: n,
            SetOutputDataAllocatorCB: n,
            GetCaps: comp_get_caps,
            Optimize: n,
        }),
        caps: Box::new(AMFCapsVtbl {
            Acquire: n,
            Release: release_fn::<AMFCaps>,
            QueryInterface: n,
            SetProperty: n,
            GetProperty: get_prop::<AMFCaps>,
            HasProperty: n,
            GetPropertyCount: n,
            GetPropertyAt: n,
            Clear: n,
            AddTo: n,
            CopyTo: n,
            AddObserver: n,
            RemoveObserver: n,
            GetAccelerationType: caps_accel,
            GetInputCaps: caps_input,
            GetOutputCaps: n,
        }),
        iocaps: Box::new(AMFIOCapsVtbl {
            Acquire: n,
            Release: release_fn::<AMFIOCaps>,
            QueryInterface: n,
            GetWidthRange: io_width,
            GetHeightRange: io_height,
            GetVertAlign: io_align,
            GetNumOfFormats: io_count,
            GetFormatAt: io_format,
            GetNumOfMemoryTypes: n,
            GetMemoryTypeAt: n,
            IsInterlacedSupported: n,
        }),
        surface: Box::new(AMFSurfaceVtbl {
            Acquire: acquire::<AMFSurface>,
            Release: release_fn::<AMFSurface>,
            QueryInterface: qi,
            SetProperty: st_set,
            GetProperty: st_get,
            HasProperty: st_has,
            GetPropertyCount: n,
            GetPropertyAt: n,
            Clear: n,
            AddTo: n,
            CopyTo: n,
            AddObserver: n,
            RemoveObserver: n,
            GetMemoryType: n,
            Duplicate: n,
            Convert: n,
            Interop: n,
            GetDataType: n,
            IsReusable: n,
            SetPts: surf_set_pts,
            GetPts: n,
            SetDuration: surf_duration,
            GetDuration: n,
            GetFormat: surf_format,
            GetPlanesCount: n,
            GetPlaneAt: n,
            GetPlane: n,
            GetFrameType: n,
            SetFrameType: surf_frame_type,
            SetCrop: surf_crop,
            CopySurfaceRegion: n,
            AddObserver_Surface: n,
            RemoveObserver_Surface: n,
        }),
        buffer: Box::new(AMFBufferVtbl {
            Acquire: acquire::<AMFBuffer>,
            Release: release_fn::<AMFBuffer>,
            QueryInterface: qi,
            SetProperty: st_set,
            GetProperty: get_prop::<AMFBuffer>,
            HasProperty: st_has,
            GetPropertyCount: n,
            GetPropertyAt: n,
            Clear: n,
            AddTo: n,
            CopyTo: n,
            AddObserver: n,
            RemoveObserver: n,
            GetMemoryType: n,
            Duplicate: n,
            Convert: n,
            Interop: n,
            GetDataType: n,
            IsReusable: n,
            SetPts: n,
            GetPts: buf_pts,
            SetDuration: n,
            GetDuration: n,
            SetSize: n,
            GetSize: buf_size,
            GetNative: buf_native,
            AddObserver_Buffer: n,
            RemoveObserver_Buffer: n,
        }),
        factory: Box::new(AMFFactoryVtbl {
            CreateContext: fac_context,
            CreateComponent: fac_component,
            SetCacheFolder: n,
            GetCacheFolder: n,
            GetDebug: n,
            GetTrace: fac_trace,
            GetPrograms: n,
        }),
    }
}

pub(crate) struct Fake {
    world: Rc<RefCell<World>>,
    factory: Box<FactoryObj>,
}

impl Fake {
    pub(crate) fn new(script: Script) -> Self {
        let world = Rc::new(RefCell::new(World {
            script,
            objs: Vec::new(),
            boxes: Vec::new(),
            vt: vtables(),
            self_ptr: ptr::null(),
            props: HashMap::new(),
            writes: Vec::new(),
            violations: Vec::new(),
            events: Vec::new(),
            submitted: Vec::new(),
            query_calls: 0,
            inits: 0,
            failed_inits: 0,
            terminates: 0,
            drains: 0,
            init_format: None,
            crops: Vec::new(),
            state: CompState::Configured,
            usage: None,
            query_timeout_before_init: false,
            context_dx11: false,
            pending: VecDeque::new(),
            current: None,
            seq: 0,
            since_init: 0,
            reinits: 0,
            flushes: 0,
            init_qp: (22, 22),
            last_output: None,
            ltr_slots: [false; 4],
            draining: false,
        }));
        let raw = Rc::as_ptr(&world);
        world.borrow_mut().self_ptr = raw;
        let vt = {
            let w = world.borrow();
            &*w.vt.factory as *const AMFFactoryVtbl
        };
        let factory = Box::new(FactoryObj {
            vtbl: vt,
            world: raw,
        });
        Self { world, factory }
    }

    pub(crate) fn factory(&mut self) -> *mut AMFFactory {
        (&mut *self.factory as *mut FactoryObj).cast()
    }

    pub(crate) fn world(&self) -> std::cell::Ref<'_, World> {
        self.world.borrow()
    }

    pub(crate) fn world_mut(&self) -> std::cell::RefMut<'_, World> {
        self.world.borrow_mut()
    }

    pub(crate) fn push_frames(&self, frames: impl IntoIterator<Item = Frame>) {
        self.world.borrow_mut().script.frames.extend(frames);
    }

    /// Every object the runtime handed out has been released exactly as often as it was
    /// acquired, and no call came in an order the runtime forbids.
    pub(crate) fn assert_clean(&self) {
        let w = self.world.borrow();
        let leaked: Vec<String> = w
            .objs
            .iter()
            .enumerate()
            .filter(|(_, o)| o.refs != 0)
            .map(|(i, o)| format!("{:?} {i} refs={}", o.kind, o.refs))
            .collect();
        assert!(leaked.is_empty(), "objects still referenced: {leaked:?}");
        let over: Vec<String> = w
            .objs
            .iter()
            .enumerate()
            .filter(|(_, o)| o.over_released != 0)
            .map(|(i, o)| format!("{:?} {i} x{}", o.kind, o.over_released))
            .collect();
        assert!(over.is_empty(), "objects released too often: {over:?}");
        assert!(
            w.violations.is_empty(),
            "call-order violations: {:?}",
            w.violations
        );
    }

    pub(crate) fn created(&self, kind: Kind) -> usize {
        self.world
            .borrow()
            .objs
            .iter()
            .filter(|o| o.kind == kind)
            .count()
    }

    /// The last value written to a component property, and whether the runtime took it.
    pub(crate) fn last_write(&self, name: &str) -> Option<Write> {
        self.world
            .borrow()
            .writes
            .iter()
            .rev()
            .find(|w| w.name == name)
            .cloned()
    }

    pub(crate) fn prop(&self, name: &str) -> Option<Var> {
        self.world.borrow().props.get(name).copied()
    }
}

/// A WARP D3D11 device: real textures, no GPU, no display.
pub(crate) fn warp_device(test: &str) -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: device creation with live out-parameters.
    let created = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    };
    match (created, device, context) {
        (Ok(()), Some(d), Some(c)) => Some((d, c)),
        _ => {
            crate::test_support::skip_because(test, "no WARP device could be created");
            None
        }
    }
}

/// Whether WARP can make the encoder's NV12 input texture.
fn warp_has_nv12(device: &ID3D11Device) -> bool {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 64,
        Height: 64,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t = None;
    // SAFETY: a live device and live locals.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)) }.is_ok() && t.is_some()
}

pub(crate) fn config(qp: Option<u8>) -> EncoderConfig {
    EncoderConfig {
        width: 320,
        height: 180,
        fps: 60,
        bitrate_bps: 6_000_000,
        max_bitrate_bps: 6_000_000,
        profile: H264Profile::High,
        qp,
        min_qp: None,
        intra_refresh: false,
    }
}

/// Field order matters: the encoder must drop before the fake runtime it calls into, including
/// when a test panics.
struct Session {
    encoder: Option<Encoder>,
    fake: Fake,
}

impl Session {
    fn enc(&mut self) -> &mut Encoder {
        self.encoder.as_mut().expect("encoder alive")
    }

    fn close(mut self) -> Fake {
        drop(self.encoder.take());
        self.fake
    }
}

fn open_with(
    test: &str,
    script: Script,
    switches: AmfSwitches,
    cfg: EncoderConfig,
    ltr: i64,
    format: DXGI_FORMAT,
) -> Option<(Fake, anyhow::Result<Encoder>)> {
    let (device, context) = warp_device(test)?;
    if format == DXGI_FORMAT_NV12 && !warp_has_nv12(&device) {
        crate::test_support::skip_because(test, "WARP cannot create an NV12 render target");
        return None;
    }
    let mut fake = Fake::new(script);
    let built = Encoder::build_on_factory(
        fake.factory(),
        switches,
        cfg,
        device,
        context,
        ltr,
        (2, format),
    );
    Some((fake, built))
}

fn open(test: &str, script: Script, cfg: EncoderConfig) -> Option<Session> {
    open_custom(test, script, AmfSwitches::shipped(), cfg, 0)
}

fn open_custom(
    test: &str,
    script: Script,
    switches: AmfSwitches,
    cfg: EncoderConfig,
    ltr: i64,
) -> Option<Session> {
    let (fake, built) = open_with(test, script, switches, cfg, ltr, DXGI_FORMAT_B8G8R8A8_UNORM)?;
    let encoder = built.expect("the encoder builds on this fake runtime");
    Some(Session {
        fake,
        encoder: Some(encoder),
    })
}

/// The picture's position in the session counting from 0 (the encoder's identity, which starts at
/// 1, minus one), and the QP the fake wrote into the access unit's slice.
pub(crate) fn frame_of(au: &[u8]) -> Option<(u64, i64)> {
    let (_, payload) = h264::nal_slices(au)
        .into_iter()
        .find(|&(t, _)| t == 1 || t == 5)?;
    let text: Vec<u8> = payload.iter().copied().skip(1).take(40).collect();
    let s = String::from_utf8_lossy(&text);
    let s = s.strip_prefix('f')?;
    let (label, rest) = s.split_once('s')?;
    let (_, rest) = rest.split_once('q')?;
    let qp: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let label: u64 = label.parse().ok()?;
    Some((label.checked_sub(1)?, qp.parse().ok()?))
}

fn is_idr(au: &[u8]) -> bool {
    h264::nal_units(au).iter().any(|&(t, _)| t == 5)
}

/// The shipped table on a runtime that knows every property: ULTRA_LOW_LATENCY first, CBR with a
/// one-frame VBV held by `EnforceHRD`, no periodic IDR, no lookahead of any kind, the input queue
/// left at its default, the query timeout set before `Init()`, and nothing written in an order
/// the runtime forbids. What it proves is what the encoder writes and in which order; the fake's
/// usage table re-defaults fewer properties than a real one.
#[test]
fn a_full_runtime_gets_the_low_latency_table_in_the_order_it_requires() {
    let Some(s) = open(
        "a_full_runtime_gets_the_low_latency_table_in_the_order_it_requires",
        Script::default(),
        config(None),
    ) else {
        return;
    };
    let f = &s.fake;
    {
        let w = f.world();
        assert_eq!(w.writes.first().map(|w| w.name.as_str()), Some("Usage"));
        assert_eq!(w.inits, 1);
        assert_eq!(w.init_format, Some((AMF_SURFACE_BGRA, 320, 180)));
    }
    let int = |n: &str| f.prop(n).and_then(|v| v.int());
    assert_eq!(int("Usage"), Some(USAGE_ULL));
    assert_eq!(int("RateControlMethod"), Some(1), "CBR");
    assert_eq!(int("TargetBitrate"), Some(6_000_000));
    assert_eq!(int("PeakBitrate"), Some(6_000_000));
    assert_eq!(
        int("VBVBufferSize"),
        Some(100_000),
        "one frame of bits at 60 fps"
    );
    assert_eq!(int("IDRPeriod"), Some(0), "the documented 'IDR off'");
    assert_eq!(int("BPicturesPattern"), Some(0));
    assert_eq!(int("RateControlPreanalysisEnable"), Some(0));
    assert_eq!(int("EnablePreAnalysis"), Some(0));
    assert_eq!(int("EnableVBAQ"), Some(0));
    assert_eq!(int("LowLatencyInternal"), Some(1));
    assert_eq!(int("EnforceHRD"), Some(1), "the bitrate tier holds its VBV");
    assert_eq!(int("FillerDataEnable"), Some(0));
    assert_eq!(int("RateControlSkipFrameEnable"), Some(0));
    assert!(
        f.last_write("InputQueueSize").is_none(),
        "left at the runtime's default"
    );
    assert_eq!(int("IntraRefreshMBsNumberPerSlot"), Some(0));
    assert_eq!(int("SlicesPerFrame"), Some(1));
    assert_eq!(int("HeaderInsertionSpacing"), Some(0));
    assert_eq!(int("QueryTimeout"), Some(1));
    assert_eq!(int("Profile"), Some(100));
    assert_eq!(
        int("ProfileLevel"),
        Some(31),
        "320x180@60 is level 3.1 in the SDP"
    );
    assert_eq!(f.prop("FrameRate"), Some(Var::Rate(60, 1)));
    assert_eq!(f.prop("FrameSize"), Some(Var::Size(320, 180)));
    assert!(
        f.last_write("MaxOfLTRFrames").is_none(),
        "no long-term references without a repair loop"
    );
    let qt = f.last_write("QueryTimeout").expect("written");
    assert!(!qt.after_init, "QueryTimeout is only honoured before Init");
    assert_eq!(s.encoder.as_ref().unwrap().rate_tier(), RateTier::LinkCbr);
    let fake = s.close();
    fake.assert_clean();
}

/// AMD's cheap probe for issue #410 failing selects LOW_LATENCY before `Init()`; an `Init()` that
/// fails under ULTRA_LOW_LATENCY is terminated and retried once under LOW_LATENCY, with the whole
/// table re-applied after the new `Usage`.
#[test]
fn ultra_low_latency_refusals_fall_back_to_low_latency() {
    let probe = Script {
        ull_probe_fails: true,
        ..Script::default()
    };
    if let Some(s) = open(
        "ultra_low_latency_refusals_fall_back_to_low_latency",
        probe,
        config(None),
    ) {
        assert_eq!(s.fake.prop("Usage").and_then(|v| v.int()), Some(USAGE_LL));
        assert_eq!(s.fake.world().inits, 1);
        assert_eq!(s.fake.world().failed_inits, 0);
        s.close().assert_clean();
    }

    let init = Script {
        ull_init_fails: true,
        ..Script::default()
    };
    let Some(s) = open(
        "ultra_low_latency_refusals_fall_back_to_low_latency",
        init,
        config(None),
    ) else {
        return;
    };
    {
        let w = s.fake.world();
        assert_eq!(w.failed_inits, 1);
        assert_eq!(w.inits, 1);
        let usages: Vec<i64> = w
            .writes
            .iter()
            .filter(|x| x.name == "Usage")
            .filter_map(|x| x.value.int())
            .collect();
        assert_eq!(usages.last(), Some(&USAGE_LL));
        let retry = w.writes.iter().rposition(|x| x.name == "Usage").unwrap();
        assert!(
            w.writes[retry..]
                .iter()
                .any(|x| x.name == "RateControlMethod"),
            "the table is re-applied after the LOW_LATENCY Usage resets it"
        );
    }
    assert_eq!(
        s.fake.prop("RateControlMethod").and_then(|v| v.int()),
        Some(1)
    );
    s.close().assert_clean();
}

/// Optional properties a runtime does not know never cost the AMD path, and the missing
/// `QueryTimeout` turns the collect loop to polling.
#[test]
fn a_runtime_missing_every_optional_property_still_encodes() {
    let mut script = Script::default();
    for n in [
        "LTRMode",
        "InputQueueSize",
        "QueryTimeout",
        "IntraPeriod",
        "LowLatencyInternal",
        "EnablePreAnalysis",
        "RateControlPreanalysisEnable",
        "HighMotionQualityBoostEnable",
        "EnableEncoderSmartAccessVideo",
        "InputFullRangeColor",
        "OutMatrixCoeff",
        "OutputMode",
        "ProfileLevel",
        "RateControlSkipFrameEnable",
        "CABACEnable",
        "EnforceHRD",
        "QualityPreset",
    ] {
        script.unknown.insert(n);
    }
    let Some(mut s) = open(
        "a_runtime_missing_every_optional_property_still_encodes",
        script,
        config(None),
    ) else {
        return;
    };
    assert!(!s.enc().query_timeout_blocking());
    for i in 0..5 {
        let au = s.enc().encode_slot(i % 2, false).expect("encodes");
        assert_eq!(frame_of(&au).map(|f| f.0), Some(i as u64));
    }
    s.close().assert_clean();
}

/// A refusal of a write that is part of the correctness core costs the tier, cleanly: the error
/// names the property and every object the runtime handed out is released.
#[test]
fn a_refused_core_property_fails_the_build_and_leaks_nothing() {
    for core in ["FrameSize", "Profile", "RateControlMethod", "TargetBitrate"] {
        let mut script = Script::default();
        script.refused.insert(core, AMF_NOT_SUPPORTED);
        let Some((fake, built)) = open_with(
            "a_refused_core_property_fails_the_build_and_leaks_nothing",
            script,
            AmfSwitches::shipped(),
            config(None),
            0,
            DXGI_FORMAT_B8G8R8A8_UNORM,
        ) else {
            return;
        };
        let err = built
            .err()
            .expect("refusing a core property fails the build");
        assert!(format!("{err:#}").contains(core), "{core}: {err:#}");
        fake.assert_clean();
    }

    let script = Script {
        create_component: AMF_ENCODER_NOT_PRESENT,
        ..Script::default()
    };
    let Some((fake, built)) = open_with(
        "a_refused_core_property_fails_the_build_and_leaks_nothing",
        script,
        AmfSwitches::shipped(),
        config(None),
        0,
        DXGI_FORMAT_B8G8R8A8_UNORM,
    ) else {
        return;
    };
    assert!(built.is_err(), "no encoder on this adapter");
    fake.assert_clean();

    let script = Script {
        accel: AMF_ACCEL_NOT_SUPPORTED,
        ..Script::default()
    };
    let Some((fake, built)) = open_with(
        "a_refused_core_property_fails_the_build_and_leaks_nothing",
        script,
        AmfSwitches::shipped(),
        config(None),
        0,
        DXGI_FORMAT_B8G8R8A8_UNORM,
    ) else {
        return;
    };
    assert!(built.is_err(), "a software encoder is refused");
    fake.assert_clean();
}

/// A runtime that enforces the documented ranges: `FrameRate` above 120 is held at 120 with the
/// target scaled so the bits per second still match, and an `IDRPeriod` it will not take in
/// either spelling leaves the usage's period in place without costing the encoder. Bookkeeping
/// only: whether a real runtime refuses these at `SetProperty`, clamps them or fails `Init` is
/// not known, and the fake refuses at `SetProperty`.
#[test]
fn documented_ranges_are_worked_around_rather_than_fatal() {
    let mut cfg = config(None);
    cfg.fps = 240;
    let Some(s) = open(
        "documented_ranges_are_worked_around_rather_than_fatal",
        Script::default(),
        cfg,
    ) else {
        return;
    };
    assert_eq!(s.fake.prop("FrameRate"), Some(Var::Rate(120, 1)));
    assert_eq!(
        s.fake.prop("TargetBitrate").and_then(|v| v.int()),
        Some(3_000_000),
        "6 Mbps at 240 fps under a 120 fps budget"
    );
    assert_eq!(
        s.fake.prop("VBVBufferSize").and_then(|v| v.int()),
        Some(25_000),
        "the VBV is still one real frame"
    );
    s.close().assert_clean();

    let mut script = Script::default();
    script.ranges.insert("IDRPeriod", (1, 1000));
    let Some(mut s) = open(
        "documented_ranges_are_worked_around_rather_than_fatal",
        script,
        config(None),
    ) else {
        return;
    };
    assert_eq!(
        s.fake.prop("IDRPeriod").and_then(|v| v.int()),
        Some(240),
        "neither spelling taken: the usage's period stays"
    );
    s.enc().encode_slot(0, false).expect("still encodes");
    s.close().assert_clean();
}

/// What `SubmitInput`'s `AMF_NEED_MORE_INPUT` means in a scripted run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NeedMoreInput {
    /// OBS's reading: the surface was taken and its output follows.
    Taken,
    /// AMD's samples' reading: the surface was not taken and must be submitted again.
    NotTaken,
}

/// 120 pictures with random input-full refusals, empty polls (some answered `AMF_OK` with a null
/// pointer) and `AMF_NEED_MORE_INPUT` in the given meaning.
fn random_frames(meaning: NeedMoreInput) -> VecDeque<Frame> {
    let mut seed = 0x9e37_79b9_u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    (0..120)
        .map(|_| {
            let r = next();
            let mut refusals = if r % 7 == 0 {
                vec![AMF_INPUT_FULL; (r % 3) as usize + 1]
            } else {
                Vec::new()
            };
            let nmi = r % 5 == 0;
            if nmi && meaning == NeedMoreInput::NotTaken {
                refusals.push(AMF_NEED_MORE_INPUT);
            }
            Frame {
                submit_refusals: refusals,
                accepted_with: if nmi && meaning == NeedMoreInput::Taken {
                    AMF_NEED_MORE_INPUT
                } else {
                    AMF_OK
                },
                not_ready: r % 4,
                not_ready_with: if r % 3 == 0 { AMF_OK } else { AMF_REPEAT },
                ..Frame::default()
            }
        })
        .collect()
}

fn run_loop(test: &str, script: Script) -> Option<IdentityCounts> {
    let mut s = open(test, script, config(None))?;
    for i in 0..120u64 {
        let au = s
            .enc()
            .encode_slot((i % 2) as usize, false)
            .expect("encodes");
        assert_eq!(frame_of(&au).map(|f| f.0), Some(i), "picture {i}");
        assert_eq!(s.enc().next_frame_index(), i + 1);
    }
    let counts = s.enc().identity_counts();
    s.close().assert_clean();
    Some(counts)
}

/// Every scripted answer on the submit and collect path, with `AMF_NEED_MORE_INPUT` meaning the
/// surface was taken (OBS): each call returns exactly its own picture, in order, none lost, none
/// twice, and no picture is submitted a second time.
#[test]
fn the_loop_loses_and_repeats_nothing_when_need_more_input_means_taken() {
    let script = Script {
        frames: random_frames(NeedMoreInput::Taken),
        ..Script::default()
    };
    let Some(c) = run_loop(
        "the_loop_loses_and_repeats_nothing_when_need_more_input_means_taken",
        script,
    ) else {
        return;
    };
    assert_eq!(
        c.need_more_input_consumed, 1,
        "the first occurrence teaches the meaning"
    );
    assert_eq!(c.need_more_input_resubmitted, 0);
    assert_eq!(c.duplicates_dropped + c.stale_dropped, 0);
}

/// The same run with `AMF_NEED_MORE_INPUT` meaning the surface was not taken (AMD's samples): the
/// first time the surface is submitted again after the probing window, every time after that at
/// once, never after a two-second stall, and every picture still comes out once and in order.
#[test]
fn the_loop_loses_and_repeats_nothing_when_need_more_input_means_not_taken() {
    let script = Script {
        frames: random_frames(NeedMoreInput::NotTaken),
        ..Script::default()
    };
    let started = Instant::now();
    let Some(c) = run_loop(
        "the_loop_loses_and_repeats_nothing_when_need_more_input_means_not_taken",
        script,
    ) else {
        return;
    };
    assert!(c.need_more_input_resubmitted > 0);
    assert_eq!(c.need_more_input_consumed, 0);
    assert_eq!(c.duplicates_dropped + c.stale_dropped, 0);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "no stall per occurrence: {:?}",
        started.elapsed()
    );
}

/// A runtime that carries neither the PTS nor the surface property to the output: pictures are
/// matched by position, `AMF_NEED_MORE_INPUT` is always taken as accepted (a resubmitted copy
/// could not be told from the next picture), and the loop still loses and repeats nothing.
#[test]
fn the_loop_falls_back_to_position_when_outputs_carry_no_identity() {
    let script = Script {
        frames: random_frames(NeedMoreInput::Taken),
        propagate_surface_properties: false,
        propagate_pts: false,
        ..Script::default()
    };
    let Some(c) = run_loop(
        "the_loop_falls_back_to_position_when_outputs_carry_no_identity",
        script,
    ) else {
        return;
    };
    assert!(c.unlabelled >= 120);
    assert_eq!(
        c.need_more_input_resubmitted, 0,
        "never resubmitted without an identity"
    );
}

/// An output harvested while clearing `AMF_INPUT_FULL` belongs to an earlier picture and is never
/// joined to this one: the access unit returned is this picture alone, and the next is an IDR.
/// A stray that turns up while collecting (the same bytes as the last picture, from a picture
/// never submitted twice) is dropped the same way. The stray itself is invented to reach the
/// path; under one-in-flight a real runtime should never produce it.
#[test]
fn a_harvested_or_stray_output_is_never_joined_to_another_picture() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                stray_before: true,
                submit_refusals: vec![AMF_INPUT_FULL],
                ..Frame::default()
            },
            Frame::default(),
            Frame {
                stray_before: true,
                ..Frame::default()
            },
            Frame::default(),
        ]
        .into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_harvested_or_stray_output_is_never_joined_to_another_picture",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    let au = s.enc().encode_slot(1, false).expect("frame 1");
    assert_eq!(frame_of(&au).map(|f| f.0), Some(1));
    let slices = h264::nal_units(&au)
        .iter()
        .filter(|&&(t, _)| t == 1 || t == 5)
        .count();
    assert_eq!(slices, 1, "one picture per access unit");
    let au = s.enc().encode_slot(0, false).expect("frame 2");
    assert!(is_idr(&au), "the harvested picture is answered with an IDR");
    let au = s.enc().encode_slot(1, false).expect("frame 3");
    assert_eq!(frame_of(&au).map(|f| f.0), Some(3));
    let au = s.enc().encode_slot(0, false).expect("frame 4");
    assert!(
        !is_idr(&au),
        "the stray's IDR falls inside the keyframe gate's window and is held back"
    );
    let c = s.enc().identity_counts();
    assert_eq!(c.duplicates_dropped + c.stale_dropped, 2);
    assert_eq!(c.own_idrs_suppressed, 1);
    s.close().assert_clean();
}

/// A resync IDR the keyframe limiter holds back is deferred, not dropped: two strays 50 ms apart
/// give two IDRs, the second once the limiter's window has passed.
#[test]
fn a_held_back_resync_idr_is_deferred_not_dropped() {
    let stray = || Frame {
        stray_before: true,
        ..Frame::default()
    };
    let mut frames = vec![Frame::default(), stray(), Frame::default(), stray()];
    frames.extend((0..40).map(|_| Frame::default()));
    let script = Script {
        frames: frames.into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_held_back_resync_idr_is_deferred_not_dropped",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("picture 0");
    s.enc()
        .encode_slot(1, false)
        .expect("picture 1: the first stray");
    let first = Instant::now();
    assert!(is_idr(&s.enc().encode_slot(0, false).expect("picture 2")));
    std::thread::sleep(Duration::from_millis(50));
    s.enc()
        .encode_slot(1, false)
        .expect("picture 3: the second stray");
    assert_eq!(
        s.enc().identity_counts().own_idrs_suppressed,
        1,
        "held back"
    );
    let mut second = None;
    for i in 0..40 {
        let au = s.enc().encode_slot(i % 2, false).expect("later picture");
        if is_idr(&au) {
            second = Some(first.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let second = second.expect("the deferred IDR was sent");
    assert!(
        // the window (200 ms) runs from the first admission, a little before `first`
        second >= Duration::from_millis(180),
        "only after the window: {second:?}"
    );
    s.close().assert_clean();
}

/// A duplicate of a resubmitted picture while the meaning is "not taken" proves "taken".
#[test]
fn a_duplicate_while_not_taken_flips_the_meaning_to_taken() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                submit_refusals: vec![AMF_NEED_MORE_INPUT],
                ..Frame::default()
            },
            Frame::default(),
            // the runtime takes the surface after all: the immediate resubmission is a copy
            Frame {
                accepted_with: AMF_NEED_MORE_INPUT,
                not_ready: 5,
                ..Frame::default()
            },
        ]
        .into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_duplicate_while_not_taken_flips_the_meaning_to_taken",
        script,
        config(None),
    ) else {
        return;
    };
    let (ids, _) = pictures(&mut s, 3);
    assert_eq!(ids, vec![Some(0), Some(1), Some(2)]);
    assert_eq!(
        s.enc().need_more_input_meaning_for_test(),
        NeedMoreInputMeaning::NotTaken
    );
    let (ids, _) = pictures(&mut s, 3);
    assert_eq!(ids, vec![Some(3), Some(4), Some(5)], "each once");
    assert_eq!(
        s.enc().need_more_input_meaning_for_test(),
        NeedMoreInputMeaning::Taken
    );
    assert_eq!(s.enc().identity_counts().duplicates_dropped, 1);
    s.close().assert_clean();
}

/// A picture taken on the word of the code that never comes out, while the meaning is "taken",
/// proves "not taken".
#[test]
fn a_stall_after_an_accepted_occurrence_flips_the_meaning_to_not_taken() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                accepted_with: AMF_NEED_MORE_INPUT,
                ..Frame::default()
            },
            Frame::default(),
            Frame {
                accepted_with: AMF_NEED_MORE_INPUT,
                out: Out::Never,
                ..Frame::default()
            },
        ]
        .into(),
        query_timeout_honoured: false,
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_stall_after_an_accepted_occurrence_flips_the_meaning_to_not_taken",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().set_submit_deadline(Duration::from_millis(60));
    pictures(&mut s, 3);
    assert_eq!(
        s.enc().need_more_input_meaning_for_test(),
        NeedMoreInputMeaning::Taken
    );
    assert!(s.enc().encode_slot(1, false).is_err(), "the stall");
    assert_eq!(
        s.enc().need_more_input_meaning_for_test(),
        NeedMoreInputMeaning::NotTaken
    );
    s.close().assert_clean();
}

/// A re-init removes a resubmitted copy before it can come out; the picture after it must not be
/// read as proof of "not taken".
#[test]
fn a_reinit_does_not_vote_on_the_meaning() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                accepted_with: AMF_NEED_MORE_INPUT,
                out: Out::Never,
                ..Frame::default()
            },
        ]
        .into(),
        query_timeout_honoured: false,
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_reinit_does_not_vote_on_the_meaning",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().set_submit_deadline(Duration::from_millis(150));
    s.enc().encode_slot(0, false).expect("picture 0");
    assert!(
        s.enc().encode_slot(1, false).is_err(),
        "probe, resubmit, stall"
    );
    assert_eq!(s.enc().identity_counts().need_more_input_resubmitted, 1);
    s.enc()
        .encode_slot(0, false)
        .expect("picture after the re-init");
    s.enc().encode_slot(1, false).expect("and another");
    assert_eq!(
        s.enc().need_more_input_meaning_for_test(),
        NeedMoreInputMeaning::Unknown
    );
    s.close().assert_clean();
}

/// Once the identity source is known, an output whose identity cannot be read is another
/// picture's: dropped as stale and counted, never returned as this one.
#[test]
fn an_output_with_an_unreadable_identity_is_stale_once_the_source_is_known() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                stray_before: true,
                stray_unlabelled: true,
                ..Frame::default()
            },
        ]
        .into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "an_output_with_an_unreadable_identity_is_stale_once_the_source_is_known",
        script,
        config(None),
    ) else {
        return;
    };
    let (ids, _) = pictures(&mut s, 3);
    assert_eq!(ids, vec![Some(0), Some(1), Some(2)]);
    assert_eq!(s.enc().identity_counts().stale_dropped, 1);
    s.close().assert_clean();
}

fn slow_taken_once() -> Frame {
    Frame {
        accepted_with: AMF_NEED_MORE_INPUT,
        not_ready: 80,
        ..Frame::default()
    }
}

fn pictures(s: &mut Session, n: usize) -> (Vec<Option<u64>>, Vec<bool>) {
    let mut ids = Vec::new();
    let mut idr = Vec::new();
    for i in 0..n {
        let au = s.enc().encode_slot(i % 2, false).expect("encodes");
        ids.push(frame_of(&au).map(|f| f.0));
        idr.push(is_idr(&au));
    }
    (ids, idr)
}

/// "Taken", learnt the expensive way, once: the first `AMF_NEED_MORE_INPUT` with an encode
/// slower than the window is submitted again, the runtime encodes it twice, the second output
/// proves the meaning and is dropped with one IDR. The second occurrence is simply accepted: no
/// resubmission, no duplicate, no keyframe.
#[test]
fn need_more_input_meaning_taken_is_learnt_once() {
    let script = Script {
        frames: vec![
            Frame::default(),
            slow_taken_once(),
            Frame::default(),
            Frame::default(),
            slow_taken_once(),
            Frame::default(),
        ]
        .into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "need_more_input_meaning_taken_is_learnt_once",
        script,
        config(None),
    ) else {
        return;
    };
    let (ids, idr) = pictures(&mut s, 7);
    assert_eq!(
        ids,
        (0..7).map(Some).collect::<Vec<_>>(),
        "each once, in order"
    );
    assert_eq!(
        idr,
        vec![true, false, false, true, false, false, false],
        "one IDR, after the one duplicate; none for the second occurrence"
    );
    assert_eq!(
        s.enc().need_more_input_meaning_for_test(),
        NeedMoreInputMeaning::Taken
    );
    let c = s.enc().identity_counts();
    assert_eq!(c.need_more_input_resubmitted, 1);
    assert_eq!(c.duplicates_dropped, 1);
    s.close().assert_clean();
}

/// "Not taken", learnt once: the first occurrence waits the probing window and submits again;
/// the next picture's output, with no second one before it, proves the meaning; the second
/// occurrence is submitted again at once, with no wait.
#[test]
fn need_more_input_meaning_not_taken_is_learnt_once() {
    let refused = || Frame {
        submit_refusals: vec![AMF_NEED_MORE_INPUT],
        ..Frame::default()
    };
    let script = Script {
        frames: vec![
            Frame::default(),
            refused(),
            Frame::default(),
            refused(),
            Frame::default(),
        ]
        .into(),
        query_timeout_honoured: false,
        ..Script::default()
    };
    let Some(mut s) = open(
        "need_more_input_meaning_not_taken_is_learnt_once",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("picture 0");
    let t = Instant::now();
    s.enc().encode_slot(1, false).expect("picture 1");
    let probing = t.elapsed();
    s.enc().encode_slot(0, false).expect("picture 2");
    assert_eq!(
        s.enc().need_more_input_meaning_for_test(),
        NeedMoreInputMeaning::NotTaken
    );
    let t = Instant::now();
    let au = s.enc().encode_slot(1, false).expect("picture 3");
    let known = t.elapsed();
    assert_eq!(frame_of(&au).map(|f| f.0), Some(3));
    assert!(
        probing >= Duration::from_millis(30),
        "the first waits the window: {probing:?}"
    );
    assert!(
        known < Duration::from_millis(20),
        "the second does not: {known:?}"
    );
    let c = s.enc().identity_counts();
    assert_eq!(c.need_more_input_resubmitted, 2);
    assert_eq!(c.duplicates_dropped + c.stale_dropped, 0);
    s.close().assert_clean();
}

/// The private property set on the input comes back on the output and is the identity (how OBS
/// and FFmpeg match outputs); where a runtime drops it the PTS takes over; where both are
/// dropped the session says so and works by position.
#[test]
fn the_property_is_the_identity_and_the_pts_the_fallback() {
    let test = "the_property_is_the_identity_and_the_pts_the_fallback";
    for (pts, prop, want) in [
        (true, true, IdentitySource::Property),
        (true, false, IdentitySource::Pts),
        (false, true, IdentitySource::Property),
        (false, false, IdentitySource::Unavailable),
    ] {
        let script = Script {
            propagate_pts: pts,
            propagate_surface_properties: prop,
            ..Script::default()
        };
        let Some(mut s) = open(test, script, config(None)) else {
            return;
        };
        s.enc().encode_slot(0, false).expect("picture 0");
        s.enc().encode_slot(1, false).expect("picture 1");
        assert_eq!(s.enc().identity_source_for_test(), want);
        s.close().assert_clean();
    }
}

/// One output without the property but with its PTS is still its picture's: matched by the
/// PTS and counted, not dropped as stale and waited out.
#[test]
fn an_output_missing_the_property_is_matched_by_its_pts() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                drop_property: true,
                ..Frame::default()
            },
        ]
        .into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "an_output_missing_the_property_is_matched_by_its_pts",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().set_submit_deadline(Duration::from_millis(500));
    let started = Instant::now();
    let (ids, _) = pictures(&mut s, 3);
    assert_eq!(ids, vec![Some(0), Some(1), Some(2)]);
    assert!(started.elapsed() < Duration::from_millis(400), "no stall");
    let counts = s.enc().identity_counts();
    assert_eq!(counts.property_missing, 1);
    assert_eq!(counts.stale_dropped, 0);
    assert_eq!(s.enc().identity_source_for_test(), IdentitySource::Property);
    s.close().assert_clean();
}

/// A runtime that leaves the property off every IDR (the join's first picture included) costs
/// nothing: every forced keyframe is delivered at once, as its own picture.
#[test]
fn a_runtime_dropping_the_property_on_every_idr_does_not_stall() {
    let script = Script {
        drop_property_on_idr: true,
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_runtime_dropping_the_property_on_every_idr_does_not_stall",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().set_submit_deadline(Duration::from_millis(500));
    let started = Instant::now();
    let mut idrs = 0;
    for i in 0..12usize {
        let au = s
            .enc()
            .encode_slot(i % 2, i % 3 == 0)
            .expect("every picture, keyframe or not");
        assert_eq!(frame_of(&au).map(|f| f.0), Some(i as u64), "picture {i}");
        if is_idr(&au) {
            idrs += 1;
        }
    }
    assert!(idrs >= 4, "the forced keyframes went out: {idrs}");
    assert!(started.elapsed() < Duration::from_millis(400), "no stall");
    let counts = s.enc().identity_counts();
    assert_eq!(counts.stale_dropped, 0);
    assert_eq!(counts.duplicates_dropped, 0);
    s.close().assert_clean();
}

/// A picture that comes back in several buffers (as if `OutputMode = FRAME` were ignored) is
/// joined from the buffers carrying its own identity and returned once; nothing is dropped and
/// no keyframe is asked for.
#[test]
fn a_picture_in_several_buffers_is_joined_by_its_identity() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                buffers: 3,
                ..Frame::default()
            },
            Frame::default(),
        ]
        .into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_picture_in_several_buffers_is_joined_by_its_identity",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("picture 0");
    let au = s.enc().encode_slot(1, false).expect("picture 1");
    let slices = h264::nal_units(&au)
        .iter()
        .filter(|&&(t, _)| t == 1 || t == 5)
        .count();
    assert_eq!(slices, 3, "the three buffers of one picture, joined");
    let au = s.enc().encode_slot(0, false).expect("picture 2");
    assert_eq!(frame_of(&au).map(|f| f.0), Some(2));
    assert!(!is_idr(&au));
    let c = s.enc().identity_counts();
    assert_eq!(c.multi_buffer_pictures, 1);
    assert_eq!(c.duplicates_dropped + c.stale_dropped, 0);
    s.close().assert_clean();
}

/// The re-init QP path checks `Flush`'s result and brings the component back the long way when
/// it fails; an unchanged QP costs nothing at all.
#[test]
fn the_reinit_qp_path_checks_flush_and_skips_an_unchanged_qp() {
    let test = "the_reinit_qp_path_checks_flush_and_skips_an_unchanged_qp";
    let Some(mut s) = open(test, Script::default(), config(Some(20))) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("picture 0");
    assert!(s.enc().set_qp_reinit(20).expect("answers"));
    assert_eq!(
        s.fake.world().flushes,
        0,
        "an unchanged QP is not a re-init"
    );
    s.close().assert_clean();

    let script = Script {
        flush_result: AMF_FAIL,
        ..Script::default()
    };
    let Some(mut s) = open(test, script, config(Some(20))) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("picture 0");
    assert!(s.enc().set_qp_reinit(30).expect("answers"));
    {
        let w = s.fake.world();
        assert_eq!(w.reinits, 0, "no ReInit after a failed Flush");
        assert_eq!(w.inits, 2, "Terminate and a full Init instead");
    }
    let au = s.enc().encode_slot(1, false).expect("picture 1");
    assert!(h264::is_idr_with_headers(&au));
    assert_eq!(frame_of(&au).map(|f| f.1), Some(30));
    s.close().assert_clean();
}

/// On a runtime that refuses target above peak at any step, a raise writes peak first and a
/// lowering target first, and a refused change is put back the same way. The clamped rate is
/// what the encoder records.
#[test]
fn bitrate_writes_never_put_target_above_peak() {
    let script = Script {
        target_le_peak: true,
        ..Script::default()
    };
    let Some(mut s) = open(
        "bitrate_writes_never_put_target_above_peak",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().set_bitrate(20_000_000).expect("a raise");
    s.enc().set_bitrate(2_000_000).expect("a lowering");
    s.enc().set_bitrate(400_000_000).expect("clamped");
    assert_eq!(
        s.enc().bitrate_for_test(),
        100_000_000,
        "the clamped rate is recorded"
    );
    s.fake
        .world_mut()
        .script
        .refused
        .insert("VBVBufferSize", AMF_INVALID_ARG);
    assert!(s.enc().set_bitrate(3_000_000).is_err());
    s.fake.world_mut().script.refused.clear();
    assert_eq!(
        s.fake.prop("TargetBitrate").and_then(|v| v.int()),
        Some(100_000_000)
    );
    assert_eq!(
        s.fake.prop("PeakBitrate").and_then(|v| v.int()),
        Some(100_000_000)
    );
    assert_eq!(s.enc().bitrate_for_test(), 100_000_000);
    s.close().assert_clean();
}

/// A real output object with no bytes is a dropped frame, returned at once; the next picture is
/// normal. Bookkeeping only: no source says a runtime returns an empty output; this checks the
/// encoder's handling if one ever does.
#[test]
fn an_empty_output_is_one_dropped_frame_not_a_stall() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                out: Out::Empty,
                ..Frame::default()
            },
            Frame::default(),
        ]
        .into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "an_empty_output_is_one_dropped_frame_not_a_stall",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    let started = Instant::now();
    let empty = s.enc().encode_slot(1, false).expect("frame 1");
    assert!(empty.is_empty());
    assert!(started.elapsed() < Duration::from_millis(500));
    let au = s.enc().encode_slot(0, false).expect("frame 2");
    assert_eq!(frame_of(&au).map(|f| f.0), Some(2));
    s.close().assert_clean();
}

/// An output that never comes is a stall: within the deadline the component is terminated and
/// re-initialised, the stuck picture can never come out as a later one, and the next picture is
/// an IDR that carries its parameter sets.
#[test]
fn a_stalled_picture_reinitialises_the_component_and_resyncs_with_an_idr() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                out: Out::Never,
                ..Frame::default()
            },
        ]
        .into(),
        query_timeout_honoured: false,
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_stalled_picture_reinitialises_the_component_and_resyncs_with_an_idr",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().set_submit_deadline(Duration::from_millis(60));
    s.enc().encode_slot(0, false).expect("frame 0");
    let err = s.enc().encode_slot(1, false).expect_err("frame 1 stalls");
    assert!(format!("{err:#}").contains("re-initialised"), "{err:#}");
    {
        let w = s.fake.world();
        assert_eq!(w.inits, 2);
        assert_eq!(w.terminates, 1);
    }
    let au = s
        .enc()
        .encode_slot(0, false)
        .expect("frame after the stall");
    assert!(is_idr(&au));
    assert!(
        h264::is_idr_with_headers(&au),
        "the resync IDR carries SPS and PPS"
    );
    assert_eq!(
        frame_of(&au).map(|f| f.0),
        Some(2),
        "the stuck picture never appears"
    );
    let au = s.enc().encode_slot(1, false).expect("and the one after");
    assert_eq!(frame_of(&au).map(|f| f.0), Some(3));
    assert!(!is_idr(&au));
    s.close().assert_clean();
}

/// A lost device answers `QueryOutput` with an error: the encode fails at once with the
/// recoverable class the pipeline rebuilds on, and teardown afterwards is prompt and complete.
#[test]
fn a_lost_device_fails_fast_and_tears_down_promptly() {
    let script = Script {
        frames: vec![Frame {
            out: Out::Fail(AMF_DIRECTX_FAILED),
            ..Frame::default()
        }]
        .into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_lost_device_fails_fast_and_tears_down_promptly",
        script,
        config(None),
    ) else {
        return;
    };
    let started = Instant::now();
    let err = s.enc().encode_slot(0, false).expect_err("device lost");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(format!("{err:#}").contains("recoverable"), "{err:#}");
    let started = Instant::now();
    let fake = s.close();
    assert!(started.elapsed() < Duration::from_millis(500));
    fake.assert_clean();
}

/// A runtime that waits inside `QueryOutput` is trusted and polled with no sleeps of our own; one
/// that echoes the timeout but returns at once is found out within sixteen empty polls and
/// backed off from, so a slow picture costs sleeps rather than a core.
#[test]
fn the_query_timeout_is_measured_not_trusted() {
    let frames = || -> VecDeque<Frame> {
        (0..4)
            .map(|_| Frame {
                not_ready: 20,
                ..Frame::default()
            })
            .collect()
    };
    let honest = Script {
        frames: frames(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "the_query_timeout_is_measured_not_trusted",
        honest,
        config(None),
    ) else {
        return;
    };
    for i in 0..4 {
        s.enc().encode_slot(i % 2, false).expect("encodes");
    }
    assert!(
        s.enc().query_timeout_blocking(),
        "a waiting runtime stays trusted"
    );
    s.close().assert_clean();

    let liar = Script {
        frames: vec![Frame {
            not_ready: 400,
            ..Frame::default()
        }]
        .into(),
        query_timeout_honoured: false,
        ..Script::default()
    };
    let Some(mut s) = open(
        "the_query_timeout_is_measured_not_trusted",
        liar,
        config(None),
    ) else {
        return;
    };
    let started = Instant::now();
    s.enc().encode_slot(0, false).expect("encodes");
    let took = started.elapsed();
    assert!(
        !s.enc().query_timeout_blocking(),
        "an echo that does not wait is distrusted"
    );
    assert!(
        took >= Duration::from_millis(20),
        "400 empty polls past the spin and yield budget must sleep, took {took:?}"
    );
    assert!(
        s.fake.world().query_calls <= 410,
        "one QueryOutput per poll"
    );
    s.close().assert_clean();
}

/// A forced IDR asks for `ForcePictureType = IDR` plus SPS and PPS on that surface, and comes out
/// self-contained with the VUI the backend writes; the very first picture after `Init()` carries
/// them even when nobody asked. The fake writes parameter sets only when asked, which is likely
/// stricter than a real runtime; the test proves the encoder asks, not that it had to.
#[test]
fn keyframes_carry_their_parameter_sets_and_the_rewritten_vui() {
    let Some(mut s) = open(
        "keyframes_carry_their_parameter_sets_and_the_rewritten_vui",
        Script::default(),
        config(None),
    ) else {
        return;
    };
    let first = s.enc().encode_slot(0, false).expect("frame 0");
    assert!(
        h264::is_idr_with_headers(&first),
        "the first picture is decodable on its own"
    );
    let p = s.enc().encode_slot(1, false).expect("frame 1");
    assert!(!is_idr(&p));
    assert_eq!(
        h264::nal_units(&p).len(),
        1,
        "a P frame is one slice and nothing else"
    );
    s.enc().request_idr();
    let idr = s.enc().encode_slot(0, false).expect("frame 2");
    assert!(h264::is_idr_with_headers(&idr));
    let sub = s.fake.world().submitted[2].clone();
    assert!(sub.forced && sub.insert_sps && sub.insert_pps);
    let sps = h264::parse_sps(&idr).expect("the SPS parses");
    assert!(sps.vui_present && sps.bitstream_restriction);
    assert_eq!(sps.max_num_reorder_frames, 0);
    assert!(
        h264::nal_units(&idr).iter().all(|&(t, _)| t != 9 && t != 6),
        "no AUD and no SEI"
    );
    s.close().assert_clean();
}

/// What the packetizer receives: one SPS and one PPS ahead of the IDR slice, which it sends as a
/// STAP-A, and P frames that are a single slice.
#[test]
fn access_units_packetize_into_what_a_browser_decoder_needs() {
    use crate::streamer::packetizer::{payloads_carry_parameter_sets, H264Packetizer};
    use bytes::Bytes;

    let script = Script {
        slice_bytes: 4000,
        ..Script::default()
    };
    let Some(mut s) = open(
        "access_units_packetize_into_what_a_browser_decoder_needs",
        script,
        config(None),
    ) else {
        return;
    };
    let mut pk = H264Packetizer::new();
    let mut out = Vec::new();
    let idr = Bytes::from(s.enc().encode_slot(0, false).expect("idr"));
    let nals = h264::nal_units(&idr);
    assert_eq!(nals.iter().filter(|&&(t, _)| t == 7).count(), 1);
    assert_eq!(nals.iter().filter(|&&(t, _)| t == 8).count(), 1);
    pk.packetize(1188, &idr, &mut out);
    assert!(payloads_carry_parameter_sets(&out));
    assert_eq!(
        out[0][0] & 0x1f,
        24,
        "SPS and PPS travel together as a STAP-A"
    );
    assert!(out.len() >= 4, "a 4 KB slice is fragmented");
    let p = Bytes::from(s.enc().encode_slot(1, false).expect("p"));
    pk.packetize(1188, &p, &mut out);
    assert!(!payloads_carry_parameter_sets(&out));
    assert_eq!(out[0][0] & 0x1f, 28, "a large P slice starts an FU-A");
    s.close().assert_clean();
}

/// Live bitrate changes are dynamic writes that take effect on the next picture, with no
/// `Terminate`, no new `Init` and no IDR; a refused one leaves the encoder's idea of its rate
/// unchanged.
#[test]
fn a_bitrate_change_is_live_and_costs_no_keyframe() {
    let Some(mut s) = open(
        "a_bitrate_change_is_live_and_costs_no_keyframe",
        Script::default(),
        config(None),
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    s.enc().set_bitrate(3_000_000).expect("applied");
    let au = s.enc().encode_slot(1, false).expect("frame 1");
    assert!(!is_idr(&au));
    {
        let w = s.fake.world();
        assert_eq!(w.inits, 1);
        assert_eq!(w.terminates, 0);
        assert_eq!(w.submitted[1].target_bitrate, Some(3_000_000));
    }
    assert_eq!(
        s.fake.prop("VBVBufferSize").and_then(|v| v.int()),
        Some(50_000)
    );
    assert_eq!(s.enc().constant_qp(), None);
    assert_eq!(
        s.enc().set_constant_qp(30).expect("answers"),
        QpApplied::NotConstantQp
    );

    s.fake
        .world_mut()
        .script
        .refused
        .insert("TargetBitrate", AMF_INVALID_ARG);
    assert!(s.enc().set_bitrate(1_000_000).is_err());
    s.fake.world_mut().script.refused.clear();
    s.enc().set_bitrate(2_000_000).expect("applied again");
    s.close().assert_clean();
}

/// The constant-QP tier's control input, with the shipped live path: the encoder writes the QPs
/// between two pictures and asks for no keyframe, and the bitrate is not ours to move.
/// Bookkeeping only: the fake applies a live QP write on the next picture because that is the
/// claim; whether a Radeon does is a question for real hardware.
#[test]
fn a_qp_change_under_constant_qp_is_live_and_costs_no_keyframe() {
    let Some(mut s) = open(
        "a_qp_change_under_constant_qp_is_live_and_costs_no_keyframe",
        Script::default(),
        config(Some(15)),
    ) else {
        return;
    };
    assert_eq!(s.enc().rate_tier(), RateTier::ConstantQp);
    assert_eq!(s.enc().constant_qp(), Some(15));
    let au = s.enc().encode_slot(0, false).expect("frame 0");
    assert_eq!(frame_of(&au).map(|f| f.1), Some(15));
    assert_eq!(
        s.enc().set_constant_qp(28).expect("answers"),
        QpApplied::Applied
    );
    let au = s.enc().encode_slot(1, false).expect("frame 1");
    assert!(!is_idr(&au));
    assert_eq!(frame_of(&au).map(|f| f.1), Some(28));
    assert_eq!(s.enc().constant_qp(), Some(28));
    assert_eq!(s.fake.world().inits, 1);
    assert!(matches!(
        s.enc().set_bitrate(1_000_000),
        Ok(crate::streamer::platform::BitrateApplied::IgnoredConstantQp)
    ));
    s.close().assert_clean();
}

/// The governor's `set_qp` on the live path: `QPI`, `QPP` and `QPB` written between two submits,
/// no IDR and no re-init asked for, the next picture asking for statistics so the change can be
/// confirmed; an unchanged QP writes nothing; a CBR session, a refused constant QP and an
/// out-of-range QP answer false and change nothing; a refused write is an error that leaves the
/// QPs as they were. Bookkeeping only as far as the QP in force goes (see the test above).
#[test]
fn set_qp_is_live_free_when_unchanged_and_honest_when_it_cannot() {
    let test = "set_qp_is_live_free_when_unchanged_and_honest_when_it_cannot";
    let switches = AmfSwitches {
        statistics_feedback: true,
        ..AmfSwitches::shipped()
    };
    let Some(mut s) = open_custom(test, Script::default(), switches, config(Some(20)), 0) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    assert_eq!(s.enc().last_frame_qp(), Some(20));
    assert!(s.enc().set_qp(32).expect("answers"));
    {
        let w = s.fake.world();
        let tail: Vec<&Write> = w.writes.iter().rev().take(3).collect();
        assert!(tail.iter().all(|x| x.after_init && x.result == AMF_OK));
        for name in ["QPI", "QPP", "QPB"] {
            assert!(tail.iter().any(|x| x.name == name), "{name} written");
        }
    }
    let au = s.enc().encode_slot(1, false).expect("frame 1");
    assert!(!is_idr(&au));
    assert_eq!(frame_of(&au).map(|f| f.1), Some(32));
    assert_eq!(
        s.enc().last_frame_qp(),
        Some(32),
        "read back from the statistics"
    );
    assert_eq!(s.fake.world().inits, 1);
    assert_eq!(s.fake.world().terminates, 0);

    let writes = s.fake.world().writes.len();
    assert!(s.enc().set_qp(32).expect("answers"));
    assert_eq!(
        s.fake.world().writes.len(),
        writes,
        "an unchanged QP writes nothing"
    );
    assert!(!s.enc().set_qp(52).expect("answers"));
    assert_eq!(s.fake.world().writes.len(), writes);

    s.fake
        .world_mut()
        .script
        .refused
        .insert("QPP", AMF_INVALID_ARG);
    assert!(s.enc().set_qp(40).is_err());
    s.fake.world_mut().script.refused.clear();
    assert_eq!(s.fake.prop("QPI").and_then(|v| v.int()), Some(32));
    assert_eq!(s.fake.prop("QPP").and_then(|v| v.int()), Some(32));
    assert_eq!(s.enc().constant_qp(), Some(32));
    s.close().assert_clean();

    let Some(mut s) = open(test, Script::default(), config(None)) else {
        return;
    };
    let writes = s.fake.world().writes.len();
    assert!(!s.enc().set_qp(30).expect("answers"), "a CBR session");
    assert_eq!(s.fake.world().writes.len(), writes);
    s.close().assert_clean();

    let refused = Script {
        cqp_refused: true,
        ..Script::default()
    };
    let Some(mut s) = open(test, refused, config(Some(20))) else {
        return;
    };
    let writes = s.fake.world().writes.len();
    assert!(
        !s.enc().set_qp(30).expect("answers"),
        "constant QP did not stick"
    );
    assert_eq!(s.fake.world().writes.len(), writes);
    s.close().assert_clean();
}

/// A driver that keeps its own rate control under a constant-QP request gets the session's
/// bitrate and a QP floor, and then both control inputs act. Bookkeeping only: the refusal is an
/// invented scenario, no source documents it on GCN.
#[test]
fn a_refused_constant_qp_runs_at_a_bitrate_with_a_qp_floor() {
    let script = Script {
        cqp_refused: true,
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_refused_constant_qp_runs_at_a_bitrate_with_a_qp_floor",
        script,
        config(Some(18)),
    ) else {
        return;
    };
    assert_eq!(s.enc().rate_tier(), RateTier::ConstantQpRefused);
    assert_eq!(
        s.fake.prop("TargetBitrate").and_then(|v| v.int()),
        Some(6_000_000)
    );
    assert_eq!(s.fake.prop("MinQP").and_then(|v| v.int()), Some(18));
    assert!(matches!(
        s.enc().set_bitrate(4_000_000),
        Ok(crate::streamer::platform::BitrateApplied::Applied)
    ));
    assert_eq!(s.enc().set_constant_qp(24).unwrap(), QpApplied::Applied);
    assert_eq!(s.fake.prop("MinQP").and_then(|v| v.int()), Some(24));
    s.close().assert_clean();
}

/// Long-term references, when a repair loop asks for them and the SPS has room: `KEEP_UNUSED`,
/// rolling refresh off, marks on the surfaces, a repair that only names slots really filled,
/// and a stall that empties them all. The fake treats a reference to an empty slot as a fault,
/// where AMF documents an intra-only frame: stricter, so it checks that the encoder never asks.
#[test]
fn long_term_references_only_name_slots_the_encoder_holds() {
    let script = Script {
        sps_max_refs: 3,
        query_timeout_honoured: false,
        ..Script::default()
    };
    let Some(mut s) = open_custom(
        "long_term_references_only_name_slots_the_encoder_holds",
        script,
        AmfSwitches::shipped(),
        config(None),
        MAX_LTR_FRAMES_SUPPORTED,
    ) else {
        return;
    };
    assert_eq!(s.enc().ltr_slots(), 2);
    assert_eq!(s.fake.prop("LTRMode").and_then(|v| v.int()), Some(1));
    assert_eq!(s.fake.prop("MaxOfLTRFrames").and_then(|v| v.int()), Some(2));
    assert_eq!(
        s.fake
            .prop("IntraRefreshMBsNumberPerSlot")
            .and_then(|v| v.int()),
        Some(0)
    );

    assert!(!s.enc().use_ltr(0b01), "nothing marked yet");
    assert_eq!(s.enc().mark_ltr(), Some(0));
    s.enc().encode_slot(0, false).expect("frame 0 marks slot 0");
    assert!(!s.enc().use_ltr(0b10), "slot 1 was never filled");
    assert!(s.enc().use_ltr(0b11));
    s.enc()
        .encode_slot(1, false)
        .expect("frame 1 references slot 0 only");
    {
        let w = s.fake.world();
        assert_eq!(w.submitted[0].mark, Some(0));
        assert_eq!(w.submitted[1].force_ltr, Some(0b01));
    }

    s.fake.push_frames([Frame {
        out: Out::Never,
        ..Frame::default()
    }]);
    s.enc().set_submit_deadline(Duration::from_millis(60));
    assert!(s.enc().use_ltr(0b01));
    s.enc().encode_slot(0, false).expect_err("stall");
    assert!(!s.enc().use_ltr(0b01), "the fresh component holds no slot");
    let au = s.enc().encode_slot(1, false).expect("after the stall");
    assert!(is_idr(&au));
    assert_eq!(s.fake.world().submitted.last().unwrap().force_ltr, None);
    s.close().assert_clean();
}

/// An SPS that declares one reference cannot carry long-term ones next to it; the encoder turns
/// them off rather than let a decoder evict what a repair would reference.
#[test]
fn an_sps_without_room_for_long_term_references_turns_them_off() {
    let script = Script {
        sps_max_refs: 1,
        ..Script::default()
    };
    let Some(mut s) = open_custom(
        "an_sps_without_room_for_long_term_references_turns_them_off",
        script,
        AmfSwitches::shipped(),
        config(None),
        MAX_LTR_FRAMES_SUPPORTED,
    ) else {
        return;
    };
    assert_eq!(s.enc().ltr_slots(), 0);
    assert_eq!(s.enc().mark_ltr(), None);
    s.enc().encode_slot(0, false).expect("encodes");
    s.close().assert_clean();

    let script = Script {
        caps_max_refs: 1,
        ..Script::default()
    };
    let Some(s) = open_custom(
        "an_sps_without_room_for_long_term_references_turns_them_off",
        script,
        AmfSwitches::shipped(),
        config(None),
        MAX_LTR_FRAMES_SUPPORTED,
    ) else {
        return;
    };
    assert!(
        s.fake.last_write("MaxOfLTRFrames").is_none(),
        "an adapter with one reference"
    );
    s.close().assert_clean();
}

/// A forced IDR the runtime ignores does not break the stream; the encoder carries on.
#[test]
fn a_forced_idr_the_runtime_ignores_is_survived() {
    let script = Script {
        frames: vec![
            Frame::default(),
            Frame {
                ignore_forced_idr: true,
                ..Frame::default()
            },
        ]
        .into(),
        ..Script::default()
    };
    let Some(mut s) = open(
        "a_forced_idr_the_runtime_ignores_is_survived",
        script,
        config(None),
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    let au = s.enc().encode_slot(1, true).expect("frame 1");
    assert!(!is_idr(&au));
    s.enc().encode_slot(0, false).expect("frame 2");
    s.close().assert_clean();
}

/// The CPU bridge is built in NV12 and never pays a second `Init()`; an encoder built in BGRA and
/// handed NV12 re-initialises once and starts again on an IDR.
#[test]
fn the_cpu_bridge_starts_in_nv12_and_a_format_change_reinitialises_once() {
    let test = "the_cpu_bridge_starts_in_nv12_and_a_format_change_reinitialises_once";
    let cfg = config(None);
    let nv12 = vec![0x80u8; (cfg.width * cfg.height * 3 / 2) as usize];
    if let Some((fake, built)) = open_with(
        test,
        Script::default(),
        AmfSwitches::shipped(),
        cfg,
        0,
        DXGI_FORMAT_NV12,
    ) {
        let mut enc = built.expect("builds in NV12");
        enc.encode_nv12_padded(&nv12, cfg.width, false)
            .expect("encodes");
        assert_eq!(fake.world().inits, 1);
        assert_eq!(
            fake.world().init_format.map(|f| f.0),
            Some(AMF_SURFACE_NV12)
        );
        assert_eq!(fake.prop("FullRangeColor").and_then(|v| v.int()), Some(0));
        drop(enc);
        fake.assert_clean();
    } else {
        return;
    }

    let Some(mut s) = open(test, Script::default(), cfg) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    let au = s
        .enc()
        .encode_nv12_padded(&nv12, cfg.width, false)
        .expect("nv12 frame");
    assert!(h264::is_idr_with_headers(&au));
    assert_eq!(s.fake.world().inits, 2);
    assert_eq!(s.fake.world().terminates, 1);
    s.close().assert_clean();
}

/// Teardown with a picture still inside the encoder drains it, terminates the component before
/// releasing it, and terminates the context only after.
#[test]
fn teardown_drains_terminates_and_releases_in_order() {
    let Some(mut s) = open(
        "teardown_drains_terminates_and_releases_in_order",
        Script::default(),
        config(None),
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    let fake = s.close();
    {
        let w = fake.world();
        let pos = |e: &str| w.events.iter().rposition(|x| x == e);
        let drain = pos("Component.Drain").expect("drained");
        let term = pos("Component.Terminate").expect("terminated");
        let rel = pos("Component.released").expect("released");
        let ctx = pos("Context.Terminate").expect("context terminated");
        assert!(drain < term && term < rel && rel < ctx, "{:?}", w.events);
    }
    assert_eq!(fake.created(Kind::Context), 1);
    assert_eq!(fake.created(Kind::Component), 1);
    fake.assert_clean();
}

/// Under the statistics candidate each surface asks for feedback and the encoder keeps the QP
/// the output reports; shipped, nothing is asked.
#[test]
fn the_statistics_candidate_reports_the_frame_qp() {
    let switches = AmfSwitches {
        statistics_feedback: true,
        ..AmfSwitches::shipped()
    };
    let Some(mut s) = open_custom(
        "the_statistics_candidate_reports_the_frame_qp",
        Script::default(),
        switches,
        config(Some(21)),
        0,
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    assert!(s.fake.world().submitted[0].statistics);
    assert_eq!(s.enc().last_frame_qp(), Some(21));
    s.close().assert_clean();

    let Some(mut s) = open(
        "the_statistics_candidate_reports_the_frame_qp",
        Script::default(),
        config(Some(21)),
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    assert!(!s.fake.world().submitted[0].statistics);
    assert_eq!(s.enc().last_frame_qp(), None);
    s.close().assert_clean();
}

/// What each switch's off branch writes: no skip-frame write, `EnforceHRD = false`, and
/// `InputQueueSize = 1` under its candidate.
#[test]
fn switches_restore_the_old_writes() {
    let switches = AmfSwitches {
        rc_skip_off: false,
        enforce_hrd: false,
        max_au_size: true,
        input_queue_one: true,
        ..AmfSwitches::shipped()
    };
    let Some(s) = open_custom(
        "switches_restore_the_old_writes",
        Script::default(),
        switches,
        config(None),
        0,
    ) else {
        return;
    };
    assert!(s.fake.last_write("RateControlSkipFrameEnable").is_none());
    assert_eq!(s.fake.prop("EnforceHRD"), Some(Var::Bool(false)));
    assert_eq!(s.fake.prop("InputQueueSize").and_then(|v| v.int()), Some(1));
    assert_eq!(
        s.fake.prop("MaxAUSize").and_then(|v| v.int()),
        Some(300_000)
    );
    s.close().assert_clean();
}

/// A key frame clears every long-term slot (AMF's encode documentation), so after a forced IDR a
/// repair may not name a slot marked before it.
#[test]
fn long_term_slots_are_forgotten_on_every_idr() {
    let script = Script {
        sps_max_refs: 3,
        ..Script::default()
    };
    let Some(mut s) = open_custom(
        "long_term_slots_are_forgotten_on_every_idr",
        script,
        AmfSwitches::shipped(),
        config(None),
        MAX_LTR_FRAMES_SUPPORTED,
    ) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    assert_eq!(s.enc().mark_ltr(), Some(0));
    s.enc().encode_slot(1, false).expect("frame 1 marks slot 0");
    assert!(s.enc().use_ltr(0b01), "slot 0 is held");
    s.enc()
        .encode_slot(0, false)
        .expect("frame 2 repairs from it");
    s.enc().request_idr();
    s.enc().encode_slot(1, false).expect("frame 3 is an IDR");
    assert!(!s.enc().use_ltr(0b01), "the IDR emptied slot 0");
    s.enc().encode_slot(0, false).expect("frame 4");
    assert_eq!(s.fake.world().submitted.last().unwrap().force_ltr, None);
    s.close().assert_clean();
}

/// With `amf_qp_live` off, `set_qp` takes OBS's path: the QPs are written, the component is
/// flushed and re-initialised, and the next picture is an IDR carrying its parameter sets at the
/// new QP. `set_qp_reinit` does the same directly. The fake's `ReInit` starts a new sequence,
/// as OBS's use of it implies.
#[test]
fn the_reinit_path_changes_the_qp_at_the_cost_of_one_idr() {
    let test = "the_reinit_path_changes_the_qp_at_the_cost_of_one_idr";
    let switches = AmfSwitches {
        qp_live: false,
        ..AmfSwitches::shipped()
    };
    let script = Script {
        qp_live_ignored: true,
        ..Script::default()
    };
    let Some(mut s) = open_custom(test, script, switches, config(Some(20)), 0) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    s.enc().encode_slot(1, false).expect("frame 1");
    assert!(s.enc().set_qp(34).expect("answers"));
    let au = s.enc().encode_slot(0, false).expect("frame 2");
    assert!(h264::is_idr_with_headers(&au));
    assert_eq!(frame_of(&au).map(|f| f.1), Some(34));
    let au = s.enc().encode_slot(1, false).expect("frame 3");
    assert!(!is_idr(&au));
    assert_eq!(frame_of(&au).map(|f| f.1), Some(34));
    {
        let w = s.fake.world();
        assert_eq!(w.reinits, 1);
        assert_eq!(w.flushes, 1);
        assert_eq!(w.inits, 1, "ReInit, not a second Init");
    }
    assert!(s.enc().set_qp_reinit(28).expect("answers"));
    let au = s.enc().encode_slot(0, false).expect("frame 4");
    assert!(is_idr(&au));
    assert_eq!(frame_of(&au).map(|f| f.1), Some(28));
    s.close().assert_clean();
}

/// On a runtime that ignores a live QP write, the first picture after the change reports its old
/// QP in its statistics and the change is counted as unconfirmed; on one that applies it, it is
/// not. Without statistics the change is counted as unchecked. Bookkeeping only for what a real
/// runtime does; what it proves is that the encoder notices either way.
#[test]
fn a_live_qp_change_that_did_not_take_is_counted() {
    let test = "a_live_qp_change_that_did_not_take_is_counted";
    let script = Script {
        qp_live_ignored: true,
        ..Script::default()
    };
    let Some(mut s) = open(test, script, config(Some(20))) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    assert!(s.enc().set_qp(30).expect("answers"));
    s.enc().encode_slot(1, false).expect("frame 1");
    assert!(
        s.fake.world().submitted[1].statistics,
        "the picture after a change asks"
    );
    assert_eq!(s.enc().qp_change_unconfirmed(), 1);
    assert_eq!(s.enc().last_frame_qp(), Some(20));
    s.enc().encode_slot(0, false).expect("frame 2");
    assert!(
        !s.fake.world().submitted[2].statistics,
        "only that picture asks"
    );
    s.close().assert_clean();

    let Some(mut s) = open(test, Script::default(), config(Some(20))) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    assert!(s.enc().set_qp(30).expect("answers"));
    s.enc().encode_slot(1, false).expect("frame 1");
    assert_eq!(s.enc().qp_change_unconfirmed(), 0);
    assert_eq!(s.enc().qp_change_unchecked(), 0);
    s.close().assert_clean();

    let old_part = Script {
        statistics_supported: false,
        ..Script::default()
    };
    let Some(mut s) = open(test, old_part, config(Some(20))) else {
        return;
    };
    s.enc().encode_slot(0, false).expect("frame 0");
    assert!(s.enc().set_qp(30).expect("answers"));
    s.enc().encode_slot(1, false).expect("frame 1");
    assert_eq!(s.enc().qp_change_unchecked(), 1);
    s.close().assert_clean();
}

/// The VUI announces the range AMF says it encodes: a runtime that refuses `FullRangeColor` keeps
/// its limited-range default, and the rewritten SPS says limited, not the full range BGRA input
/// would suggest.
#[test]
fn the_vui_range_follows_what_amf_reports_not_the_input_format() {
    let mut script = Script::default();
    script.refused.insert("FullRangeColor", AMF_NOT_SUPPORTED);
    let Some(mut s) = open(
        "the_vui_range_follows_what_amf_reports_not_the_input_format",
        script,
        config(None),
    ) else {
        return;
    };
    let au = s.enc().encode_slot(0, false).expect("frame 0");
    let sps = h264::parse_sps(&au).expect("SPS");
    assert!(sps.vui_present);
    assert!(
        crate::windows_utils::streamer::amd::sps::patch_annexb(&au, false).is_none(),
        "the SPS already says limited range"
    );
    s.close().assert_clean();
}

/// A bitrate is held to the documented range and the adapter's cap, and a refused write puts
/// back what was last written, so target and peak are never left apart.
#[test]
fn bitrates_are_clamped_and_never_left_split() {
    let Some(mut s) = open(
        "bitrates_are_clamped_and_never_left_split",
        Script::default(),
        config(None),
    ) else {
        return;
    };
    s.enc().set_bitrate(400_000_000).expect("applied");
    assert_eq!(
        s.fake.prop("TargetBitrate").and_then(|v| v.int()),
        Some(100_000_000)
    );
    s.enc().set_bitrate(8_000_000).expect("applied");
    s.fake
        .world_mut()
        .script
        .refused
        .insert("PeakBitrate", AMF_INVALID_ARG);
    assert!(s.enc().set_bitrate(3_000_000).is_err());
    s.fake.world_mut().script.refused.clear();
    assert_eq!(
        s.fake.prop("TargetBitrate").and_then(|v| v.int()),
        Some(8_000_000),
        "the target written before the refusal is put back"
    );
    assert_eq!(
        s.fake.prop("PeakBitrate").and_then(|v| v.int()),
        Some(8_000_000)
    );
    s.close().assert_clean();
}

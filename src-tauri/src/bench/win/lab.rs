//! `lab` on Windows: one production encoder session per run, fed at the recorded deadlines.
//!
//! The session is built the way `pipeline.rs` builds it for the vendor (capture device from
//! `capture_gpu_preference`, then the NVENC ring, the Quick Sync inline backend, x264 behind the
//! scaler, or the AMF ring), and each frame arrives as a BGRA8 texture on the capture device.
//! The encode call is synchronous on the paced thread: submit is the call, bitstream is its return.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

use super::super::metrics::{peak_rate_bps, summary};
use super::super::plan::{ProductionCfg, RunSpec};
use super::super::series::{self, Row, Sampler};
use super::avc::{self, PicType};
use super::feed::{self, FeedSession, FeedStats, Input, Pacer, Schedule};
use super::hooks::{self, Disarmed, Vendor as HookVendor};
use super::sys;
use crate::streamer::config::{EncoderVendor, H264Profile};
use crate::windows_utils::streamer::amd::encoder::Encoder as AmfEncoder;
use crate::windows_utils::streamer::intel::encoder::Encoder as IntelEncoder;
use crate::windows_utils::streamer::nvidia::clock_hold;
use crate::windows_utils::streamer::nvidia::encoder::{
    Encoder as NvEncoder, EncoderConfig, SlotEncodeError, KEY_ENCODER, KEY_WRITER,
};
use crate::windows_utils::streamer::pipeline::{self, Backend};
use crate::windows_utils::streamer::scaler::{ReadbackFormat, Scaler, TextureReader};
use crate::windows_utils::streamer::x264::encoder::X264Encoder;
use crate::windows_utils::streamer::{capture_gpu_preference, tuning, AMD_PCI_VENDOR_ID};
use crate::windows_utils::windows_capture::d3d11::{
    create_d3d_device, device_adapter_info, enumerate_adapters, GpuPreference,
};

/// A run with more than this share of frames released > 2 ms late by the harness is invalid.
const LATE_NS: u64 = 2_000_000;
const LATE_SHARE: f64 = 0.01;
/// Consecutive hard encode errors after which a session stops submitting.
const MAX_CONSECUTIVE_ERRORS: usize = 30;

pub const HELP: &str = "\
LAB (Windows): one production encoder session per run, fed at the recorded deadlines
  recipe fields:  vendor  nvenc | qsv | x264 | amf | none (none = the feeder alone)
                  production  {qp, min_qp, bitrate_bps, profile}  (qp 15 = what a default join ships)
                  set  [[field, value], ...]  applied after the production defaults, before the
                       session opens. NVENC: preset, tuning, enableEncodeAsync, rc.*, h264.*
                       (NV_ENC_* member names); Quick Sync: AsyncDepth, mfx.*, co.*, co2.*, co3.*
                       (mfxInfoMFX / mfxExtCodingOption* member names, MFX_* symbols accepted);
                       x264: x264_param_parse option names. An unknown name fails the run.
                  input  bgra (production) | nv12 (the production scaler, ahead of the deadline)
                  gpu_clock_hold  true | false  (SE_GPU_CLOCK_HOLD for the child)
                  env  [[\"SE_...\", value], ...]  set in the child before anything starts
  workload:       synthetic (static | scroll | motion | drag | mixed | noise, any size) or a
                  recording; transform = fps:N and/or tile2x2, crop:WxH, down2x, chained in order
  builtin plans:  win-quick (minutes), win-encoders (the settings sweep; see --list)
";

/// The production encoder configuration for a size and rate (`pipeline::live_encoder_config`'s
/// arithmetic with the plan's quality knobs).
pub fn production_config(p: &ProductionCfg, w: u32, h: u32, fps: u32) -> EncoderConfig {
    let computed = ((w as u64 * h as u64 * fps as u64) / 10).clamp(6_000_000, 30_000_000) as u32;
    let bitrate = p.bitrate_bps.unwrap_or(computed);
    EncoderConfig {
        width: w,
        height: h,
        fps,
        bitrate_bps: bitrate,
        max_bitrate_bps: bitrate,
        profile: match p.profile.as_deref() {
            Some("main") => H264Profile::Main,
            Some("baseline") => H264Profile::Baseline,
            _ => H264Profile::High,
        },
        qp: p.qp,
        min_qp: p.min_qp,
        intra_refresh: false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LabVendor {
    Nvenc,
    Qsv,
    X264,
    Amf,
    None,
}

impl LabVendor {
    pub(super) fn parse(s: &str) -> Option<LabVendor> {
        Some(match s {
            "nvenc" => LabVendor::Nvenc,
            "qsv" => LabVendor::Qsv,
            "x264" => LabVendor::X264,
            "amf" => LabVendor::Amf,
            "none" => LabVendor::None,
            _ => return None,
        })
    }
    pub(super) fn name(self) -> &'static str {
        match self {
            LabVendor::Nvenc => "nvenc",
            LabVendor::Qsv => "qsv",
            LabVendor::X264 => "x264",
            LabVendor::Amf => "amf",
            LabVendor::None => "none",
        }
    }
    fn encoder_vendor(self) -> EncoderVendor {
        match self {
            LabVendor::Nvenc => EncoderVendor::Nvidia,
            LabVendor::Qsv => EncoderVendor::Intel,
            LabVendor::Amf => EncoderVendor::Amd,
            LabVendor::X264 => EncoderVendor::Software,
            LabVendor::None => EncoderVendor::Auto,
        }
    }
    fn hook(self) -> Option<HookVendor> {
        match self {
            LabVendor::Nvenc => Some(HookVendor::Nvenc),
            LabVendor::Qsv => Some(HookVendor::Qsv),
            LabVendor::X264 => Some(HookVendor::X264),
            _ => None,
        }
    }
}

/// Whether this vendor's production encoder type takes the input, and through what.
fn input_path(v: LabVendor, input: Input) -> Result<&'static str, String> {
    Ok(match (v, input) {
        (LabVendor::Nvenc, Input::Bgra) => "ring stage copy -> encode_input_slot (ARGB input)",
        (LabVendor::Nvenc, Input::Nv12) => {
            return Err("the production NVENC encoder registers its inputs as \
                        NV_ENC_BUFFER_FORMAT_ARGB and has no NV12 entry point"
                .into())
        }
        (LabVendor::Qsv, Input::Bgra) => {
            "Backend::encode_gpu (VPP from the BGRA texture), or the CPU bridge"
        }
        (LabVendor::Qsv, Input::Nv12) => {
            "encode_nv12_padded (the CPU-bridge entry of the same encoder type)"
        }
        (LabVendor::X264, Input::Bgra) => "production scaler blt -> NV12 readback -> encode_cpu",
        (LabVendor::X264, Input::Nv12) => {
            "encode_cpu(Nv12) with the scaler run ahead of the deadline"
        }
        (LabVendor::Amf, Input::Bgra) => {
            "AMF ring: copy into the encoder's own texture -> encode_slot"
        }
        (LabVendor::Amf, Input::Nv12) => {
            return Err(
                "the AMF ring takes textures; NV12 bytes reach only the AMF CPU bridge, \
                        which the production ring path does not use"
                    .into(),
            )
        }
        (LabVendor::None, _) => "none (feeder only)",
    })
}

#[allow(clippy::large_enum_variant)]
pub(super) enum Enc {
    NvRing {
        enc: Box<NvEncoder>,
        pairs: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>,
        next: usize,
        acquire_ms: u32,
        _hold: Option<clock_hold::Guard>,
    },
    AmfRing {
        enc: Box<AmfEncoder>,
        textures: Vec<ID3D11Texture2D>,
        next: usize,
    },
    Inline {
        backend: Box<Backend>,
        scaler: Option<Scaler>,
        reader: Option<TextureReader>,
    },
    None,
}

pub(super) struct Session {
    pub(super) enc: Enc,
    pub(super) dev: ID3D11Device,
    pub(super) ctx: ID3D11DeviceContext,
    pub(super) backend: &'static str,
    pub(super) identity: Json,
    pub(super) create_ms: f64,
    pub(super) hooks: Option<Disarmed>,
    pub(super) clock_hold: Json,
}

// SAFETY: every COM object in a session is used by the one thread that runs it; the encoders are
// `Send` themselves and the textures belong to this session's own devices.
unsafe impl Send for Session {}

impl Session {
    /// Encode one frame; returns the access unit and the time spent before the encoder's own
    /// submit (the NVENC ring's stage copy), ns.
    fn encode(
        &mut self,
        tex: Option<&ID3D11Texture2D>,
        nv12: Option<&feed::Nv12>,
        force: bool,
    ) -> anyhow::Result<(Vec<u8>, u64)> {
        use anyhow::{anyhow, Context as _};
        let ctx = &self.ctx;
        match &mut self.enc {
            Enc::None => Ok((Vec::new(), 0)),
            Enc::NvRing {
                enc,
                pairs,
                next,
                acquire_ms,
                ..
            } => {
                let tex = tex.ok_or_else(|| anyhow!("NVENC takes a texture"))?;
                let slot = *next;
                *next = (*next + 1) % pairs.len();
                let t = Instant::now();
                let (dst, m) = &pairs[slot];
                // mirrors `AsyncEncodeRing::stage`, without a scaler (the frame is the encode size)
                unsafe {
                    m.AcquireSync(KEY_WRITER, *acquire_ms)
                        .context("ring slot AcquireSync(writer)")?;
                    ctx.CopyResource(dst, tex);
                    ctx.Flush();
                    m.ReleaseSync(KEY_ENCODER)
                        .context("ring slot ReleaseSync(encoder)")?;
                }
                let stage = t.elapsed().as_nanos() as u64;
                let au = enc.encode_input_slot(slot, force).map_err(|e| match e {
                    SlotEncodeError::Poisoned(e) => e.context("slot poisoned"),
                    SlotEncodeError::Encode(e) => e,
                })?;
                Ok((au, stage))
            }
            Enc::AmfRing {
                enc,
                textures,
                next,
            } => {
                let tex = tex.ok_or_else(|| anyhow!("AMF takes a texture"))?;
                let slot = *next;
                *next = (*next + 1) % textures.len();
                let t = Instant::now();
                unsafe {
                    ctx.CopyResource(&textures[slot], tex);
                    ctx.Flush();
                }
                let stage = t.elapsed().as_nanos() as u64;
                Ok((enc.encode_slot(slot, force)?, stage))
            }
            Enc::Inline {
                backend,
                scaler,
                reader,
            } => {
                if let Some(f) = nv12 {
                    let au = match backend.as_mut() {
                        Backend::Intel { encoder } => {
                            encoder.encode_nv12_padded(&f.data, f.pitch, force)?
                        }
                        b => b.encode_cpu(&f.data, f.pitch, ReadbackFormat::Nv12, force)?,
                    };
                    return Ok((au, 0));
                }
                let tex = tex.ok_or_else(|| anyhow!("no texture"))?;
                // mirrors `EncodeCore::encode_texture`
                let au = if backend.is_cpu_bridge() {
                    let (format, (data, pitch)) = if let Some(s) = scaler {
                        let format = s.format();
                        s.scale(tex)?;
                        (format, s.read_back()?)
                    } else {
                        let r = reader
                            .as_mut()
                            .ok_or_else(|| anyhow!("cpu bridge without a reader"))?;
                        (ReadbackFormat::Bgra8, r.read_back(tex)?)
                    };
                    backend.encode_cpu(data, pitch, format, force)?
                } else {
                    backend.encode_gpu(tex, scaler.as_mut(), force)?
                };
                Ok((au, 0))
            }
        }
    }
}

fn adapter_label(dev: &ID3D11Device) -> Option<String> {
    device_adapter_info(dev).map(|a| a.label())
}

/// Build the production session for `v` on a fresh capture device.
fn open(v: LabVendor, config: EncoderConfig, input: Input) -> Result<Session, String> {
    let pref = capture_gpu_preference(v.encoder_vendor());
    let (dev, ctx, cap) = create_d3d_device(pref).map_err(|e| format!("capture device: {e}"))?;
    let capture = json!({
        "preference": format!("{pref:?}"),
        "adapter": cap.label(),
        "vendor_id": format!("0x{:04X}", cap.vendor_id),
        "luid": format!("0x{:X}", cap.luid),
    });
    let t = Instant::now();
    let (enc, backend, mut identity): (Enc, &'static str, Json) = match v {
        LabVendor::None => (Enc::None, "none", json!({})),
        LabVendor::Nvenc => {
            let (enc, pairs) = pipeline::build_zero_copy_ring(config, &dev, 3)
                .map_err(|e| format!("NVENC zero-copy ring: {e:#}"))?;
            tuning::raise_d3d11_gpu_priority(enc.device());
            let c = enc.caps();
            let id = json!({
                "encoder_device": adapter_label(enc.device()),
                "ltr_slots": enc.ltr_slots(),
                "pipeline_depth": enc.pipeline_depth(),
                "async": enc.pipeline_depth() > 1,
                "constant_qp": enc.constant_qp(),
                "caps": {
                    "rc_modes": format!("0x{:x}", c.rc_modes), "async_encode": c.async_encode,
                    "intra_refresh": c.intra_refresh, "single_slice_intra_refresh": c.single_slice_intra_refresh,
                    "custom_vbv_buf_size": c.custom_vbv_buf_size, "multiple_ref_frames": c.multiple_ref_frames,
                    "max_ltr_frames": c.max_ltr_frames, "num_encoder_engines": c.num_encoder_engines,
                    "level_max": c.level_max, "cabac": c.cabac, "dynamic_slice_mode": c.dynamic_slice_mode,
                },
            });
            // `AsyncEncodeRing::new` takes the clock hold for an NVENC ring
            let hold = clock_hold::acquire();
            let period_ms = 1000 / config.fps.max(1);
            (
                Enc::NvRing {
                    enc: Box::new(enc),
                    pairs,
                    next: 0,
                    acquire_ms: (2 * period_ms).clamp(5, 50),
                    _hold: hold,
                },
                "nvenc-zero-copy-ring",
                id,
            )
        }
        LabVendor::Amf => {
            let has_amd = enumerate_adapters(None)
                .map(|a| {
                    a.iter()
                        .any(|(_, i)| i.vendor_id == AMD_PCI_VENDOR_ID && !i.software)
                })
                .unwrap_or(false);
            if !has_amd {
                return Err("no AMD adapter".into());
            }
            let (enc, textures) = pipeline::build_amf_ring(config, &dev, &ctx, 3)
                .map_err(|e| format!("AMF ring: {e:#}"))?;
            tuning::raise_d3d11_gpu_priority(enc.device());
            let id = json!({ "encoder_device": adapter_label(enc.device()), "ltr_slots": enc.ltr_slots() });
            (
                Enc::AmfRing {
                    enc: Box::new(enc),
                    textures,
                    next: 0,
                },
                "amd-amf-ring",
                id,
            )
        }
        LabVendor::Qsv => {
            let (backend, first_error) = match IntelEncoder::new_on_device(
                config,
                config.width,
                config.height,
                &dev,
                &ctx,
            ) {
                Ok(e) => (Backend::Intel { encoder: e }, None),
                Err(e) => {
                    let first = format!("{e:#}");
                    let enc = IntelEncoder::new(config).map_err(|e2| {
                        format!("Quick Sync unavailable: same-adapter: {first}; CPU bridge: {e2:#}")
                    })?;
                    (Backend::IntelCpu { encoder: enc }, Some(first))
                }
            };
            let (enc_dev, slice) = match &backend {
                Backend::Intel { encoder } | Backend::IntelCpu { encoder } => (
                    adapter_label(encoder.device()),
                    format!("{:?}", encoder.slice_mode()),
                ),
                _ => (None, String::new()),
            };
            let id = json!({ "encoder_device": enc_dev, "slice_mode": slice, "same_adapter_error": first_error });
            inline(backend, &dev, &ctx, config, input, id)?
        }
        LabVendor::X264 => {
            let enc = X264Encoder::new(config).map_err(|e| format!("x264: {e:#}"))?;
            let eff = enc.effective_params();
            let id = json!({
                "x264_build": enc.build(),
                "effective": {
                    "threads": eff.i_threads, "sliced_threads": eff.b_sliced_threads,
                    "bframes": eff.i_bframe, "rc_lookahead": eff.rc.i_lookahead,
                    "sync_lookahead": eff.i_sync_lookahead, "ref": eff.i_frame_reference,
                    "cabac": eff.b_cabac, "rc_method": eff.rc.i_rc_method,
                },
            });
            inline(
                Backend::X264 { encoder: enc },
                &dev,
                &ctx,
                config,
                input,
                id,
            )?
        }
    };
    let create_ms = t.elapsed().as_secs_f64() * 1e3;
    identity["capture_device"] = capture;
    let (want, why) = clock_hold::wanted();
    let held = matches!(&enc, Enc::NvRing { _hold: Some(_), .. });
    Ok(Session {
        enc,
        dev,
        ctx,
        backend,
        identity,
        create_ms,
        hooks: None,
        clock_hold: json!({
            "policy": want, "why": why, "taken": held,
            "note": if v == LabVendor::Nvenc { "production takes the hold for an NVENC ring session" }
                    else { "production holds clocks only for NVENC ring sessions; nothing taken" },
        }),
    })
}

/// The inline (`build_backend`) session around a constructed backend, with the scaler or reader
/// `LiveCapture::new` would give it.
fn inline(
    backend: Backend,
    dev: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    config: EncoderConfig,
    input: Input,
    id: Json,
) -> Result<(Enc, &'static str, Json), String> {
    let name = backend.name();
    // mirrors `needs_scaler` / `scaler_output_format`, at native == encode size
    let want_fmt = if backend.is_cpu_bridge() {
        backend.cpu_bridge_format()
    } else {
        ReadbackFormat::Bgra8
    };
    let needs = backend.is_cpu_bridge() && backend.cpu_bridge_format() != ReadbackFormat::Bgra8;
    let (scaler, reader) = if input == Input::Nv12 {
        (None, None)
    } else if needs {
        (
            Some(
                Scaler::with_format(
                    dev,
                    ctx,
                    config.width,
                    config.height,
                    config.width,
                    config.height,
                    want_fmt,
                )
                .map_err(|e| format!("production scaler: {e:#}"))?,
            ),
            None,
        )
    } else if backend.is_cpu_bridge() {
        (
            None,
            Some(
                TextureReader::new(dev, ctx, config.width, config.height)
                    .map_err(|e| format!("texture reader: {e:#}"))?,
            ),
        )
    } else {
        (None, None)
    };
    let enc_dev = match &backend {
        Backend::Intel { encoder } | Backend::IntelCpu { encoder } => {
            Some(encoder.device().clone())
        }
        _ => None,
    };
    if let Some(d) = &enc_dev {
        tuning::raise_d3d11_gpu_priority(d);
    }
    Ok((
        Enc::Inline {
            backend: Box::new(backend),
            scaler,
            reader,
        },
        name,
        id,
    ))
}

/// Open with the recipe's overrides armed. A failure is retried without them to tell a rejected
/// recipe from a missing capability.
pub(super) fn open_with(
    v: LabVendor,
    config: EncoderConfig,
    input: Input,
    set: &[(String, super::recipe::Val)],
) -> Result<Session, (bool, String, Option<Disarmed>)> {
    let hv = v.hook();
    if let (Some(h), false) = (hv, set.is_empty()) {
        let lib = if h == HookVendor::X264 {
            crate::windows_utils::streamer::x264::x264_sys::X264Api::load()
                .ok()
                .map(|a| a.library_path().to_path_buf())
        } else {
            None
        };
        hooks::arm(h, hooks::validate(h, set, lib.as_deref()));
    }
    let r = open(v, config, input);
    let mut d = hooks::disarm();
    match r {
        Ok(mut s) => {
            if let Some(d) = d.as_mut() {
                if v == LabVendor::Qsv {
                    // SAFETY: the session's encoder (whose oneVPL session the hook saw) is alive.
                    unsafe { d.qsv_readback() };
                }
                if let (Enc::Inline { backend, .. }, LabVendor::X264) = (&s.enc, v) {
                    if let Backend::X264 { encoder } = backend.as_ref() {
                        let lib = crate::windows_utils::streamer::x264::x264_sys::X264Api::load()
                            .ok()
                            .map(|a| a.library_path().to_path_buf());
                        d.x264_readback(&encoder.effective_params(), lib.as_deref());
                    }
                }
            }
            s.hooks = d;
            Ok(s)
        }
        Err(e) => {
            if set.is_empty() {
                return Err((false, e, d));
            }
            match open(v, config, input) {
                Ok(_) => {
                    if let Some(d) = d.as_mut() {
                        d.mark_rejected(&e);
                    }
                    Err((true, e, d))
                }
                Err(_) => Err((false, e, d)),
            }
        }
    }
}

/// One session's per-frame record, preallocated.
struct Track {
    submit_ns: Vec<u64>,
    done_ns: Vec<u64>,
    stage_ns: Vec<u64>,
    bytes: Vec<u64>,
    pic: Vec<PicType>,
    slices: Vec<u16>,
    qp: Vec<i8>,
    /// 0 ok, 1 device busy (dropped, as production does), 2 error, 3 not submitted.
    status: Vec<u8>,
    late_ns: Vec<u64>,
    late_by_encoder: usize,
    underruns: usize,
    reader_gap: bool,
    errors: BTreeMap<String, usize>,
    first_error: Option<(usize, String)>,
    feed: FeedStats,
    avc: avc::State,
    pacer: &'static str,
    stopped_early: Option<String>,
}

impl Track {
    fn new(n: usize) -> Track {
        Track {
            submit_ns: vec![0; n],
            done_ns: vec![0; n],
            stage_ns: vec![0; n],
            bytes: vec![0; n],
            pic: vec![PicType::None; n],
            slices: vec![0; n],
            qp: vec![-1; n],
            status: vec![3; n],
            late_ns: Vec::with_capacity(n),
            late_by_encoder: 0,
            underruns: 0,
            reader_gap: false,
            errors: BTreeMap::new(),
            first_error: None,
            feed: FeedStats::with_capacity(n),
            avc: avc::State::new(),
            pacer: "",
            stopped_early: None,
        }
    }
}

fn run_session(
    mut s: Session,
    fs: FeedSession,
    sched: Arc<Schedule>,
    t_start: Instant,
    idr_at: Arc<Vec<bool>>,
) -> (Session, Track) {
    let _tuning = tuning::tune_current_thread();
    let pacer = Pacer::new();
    let total = sched.total();
    let mut tr = Track::new(total);
    tr.pacer = pacer.kind();
    let ns = |t: Instant| t.saturating_duration_since(t_start).as_nanos() as u64;
    let mut prev_end: Option<Instant> = None;
    let mut consecutive = 0usize;
    for g in 0..total {
        let r = match fs.try_recv() {
            Some(r) => r,
            None => {
                tr.underruns += 1;
                match fs.recv() {
                    Some(r) => r,
                    None => {
                        tr.reader_gap = true;
                        break;
                    }
                }
            }
        };
        if r.g != g {
            tr.stopped_early = Some(format!("feeder order: expected {g}, got {}", r.g));
            break;
        }
        let deadline = t_start + Duration::from_nanos(sched.at[g]);
        let tex = if r.nv12.is_none() {
            match fs.acquire(&r) {
                Ok(t) => Some(t.clone()),
                Err(e) => {
                    tr.stopped_early = Some(format!("frame {g}: {e}"));
                    break;
                }
            }
        } else {
            None
        };
        let miss = tr.feed.note(g, deadline, &r.times);
        let late = pacer.wait_until(deadline);
        // a frame the feeder had not delivered is late by the harness, whatever the encoder did
        let feeder_late = matches!(miss, Some(c) if c != "consumer");
        if late > LATE_NS && !feeder_late && prev_end.is_some_and(|e| e > deadline) {
            tr.late_by_encoder += 1;
        }
        tr.late_ns.push(late);
        let force = g == 0 || idr_at[g];
        let t0 = Instant::now();
        let res = s.encode(tex.as_ref(), r.nv12.as_deref(), force);
        let t1 = Instant::now();
        prev_end = Some(t1);
        if r.nv12.is_none() {
            fs.release(r);
        } else {
            fs.done(r);
        }
        match res {
            Ok((au, stage)) => {
                consecutive = 0;
                tr.submit_ns[g] = ns(t0) + stage;
                tr.stage_ns[g] = stage;
                tr.done_ns[g] = ns(t1);
                tr.status[g] = 0;
                tr.bytes[g] = au.len() as u64;
                if !au.is_empty() {
                    let a = tr.avc.parse(&au);
                    tr.pic[g] = a.pic;
                    tr.slices[g] = a.slices;
                    tr.qp[g] = a.qp.unwrap_or(-1);
                }
            }
            Err(e) => {
                let text = format!("{e:#}");
                let busy = text.contains("device busy");
                tr.submit_ns[g] = ns(t0);
                tr.done_ns[g] = ns(t1);
                tr.status[g] = if busy { 1 } else { 2 };
                let key = text.chars().take(160).collect::<String>();
                *tr.errors.entry(key).or_default() += 1;
                tr.first_error.get_or_insert((g, text));
                if !busy {
                    consecutive += 1;
                    if consecutive >= MAX_CONSECUTIVE_ERRORS {
                        tr.stopped_early = Some(format!(
                            "{MAX_CONSECUTIVE_ERRORS} consecutive encode errors"
                        ));
                        break;
                    }
                }
            }
        }
    }
    drop(fs);
    (s, tr)
}

pub(super) fn fault(kind: &str) {
    eprintln!("[bench] test fault '{kind}' requested");
    match kind {
        // access violation: the runner must see NTSTATUS 0xC0000005 and call it a crash
        "crash" => unsafe {
            std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 0);
        },
        "abort" => std::process::abort(),
        "panic" => panic!("bench test fault"),
        "hang" => loop {
            std::thread::sleep(Duration::from_secs(1));
        },
        _ => {}
    }
}

pub(super) fn base(spec: &RunSpec, result: &str, reason: String) -> Json {
    json!({
        "id": spec.id,
        "result": result,
        "valid": false,
        "invalid_reasons": [reason.clone()],
        "stage": "lab",
        "platform": "windows",
        "error": reason,
    })
}

pub(super) fn skipped(spec: &RunSpec, reason: String) -> Json {
    let mut j = base(spec, "skipped", reason.clone());
    j["skip_reason"] = json!(reason);
    j
}

pub fn run(spec: &RunSpec) -> Json {
    if let Some(f) = spec.recipe.test_fault.as_deref() {
        fault(f);
    }
    let wall = Instant::now();
    let r = &spec.recipe.recipe;
    if let Err(e) = super::pipe::env_of(r) {
        return base(spec, "recipe_error", e);
    }
    let Some(vendor) = r.vendor.as_deref() else {
        return skipped(
            spec,
            "the recipe names no Windows vendor (a VideoToolbox recipe; vendor: nvenc | qsv | x264 | amf | none)".into(),
        );
    };
    let Some(v) = LabVendor::parse(vendor) else {
        return base(spec, "recipe_error", format!("unknown vendor '{vendor}'"));
    };
    let input = match feed::Input::parse(r.input.as_deref()) {
        Ok(i) => i,
        Err(e) => return base(spec, "recipe_error", e),
    };
    let path = match input_path(v, input) {
        Ok(p) => p,
        Err(why) => {
            let mut j = skipped(spec, format!("input {input:?} unsupported: {why}"));
            j["input"] = json!({ "format": input, "supported": false, "why": why });
            return j;
        }
    };
    let set = &r.set;
    if !set.is_empty() {
        let Some(h) = v.hook() else {
            return base(
                spec,
                "recipe_error",
                format!("vendor '{vendor}' takes no `set` overrides on this bench"),
            );
        };
        let lib = (h == HookVendor::X264)
            .then(|| {
                crate::windows_utils::streamer::x264::x264_sys::X264Api::load()
                    .ok()
                    .map(|a| a.library_path().to_path_buf())
            })
            .flatten();
        let check = hooks::validate(h, set, lib.as_deref());
        if !check.ok() {
            let bad: Vec<String> = check
                .report
                .iter()
                .filter(|f| !matches!(f.outcome, hooks::Outcome::Pending | hooks::Outcome::Applied))
                .map(|f| {
                    format!(
                        "{} ({:?}): {}",
                        f.field,
                        f.outcome,
                        f.note.clone().unwrap_or_default()
                    )
                })
                .collect();
            let mut j = base(
                spec,
                "recipe_error",
                format!("recipe fields refused: {}", bad.join("; ")),
            );
            j["recipe_report"] = json!({ "fields": check.report });
            return j;
        }
    }
    // axes the child applies to itself before any thread exists
    let mut env_set: Vec<(String, String)> = Vec::new();
    if let Some(hold) = r.gpu_clock_hold {
        let v = if hold { "1" } else { "0" };
        std::env::set_var("SE_GPU_CLOCK_HOLD", v);
        env_set.push(("SE_GPU_CLOCK_HOLD".into(), v.into()));
    }
    if v == LabVendor::Nvenc {
        if let Some((_, val)) = set.iter().rev().find(|(k, _)| k == "enableEncodeAsync") {
            let off = matches!(
                val,
                super::recipe::Val::Int(0) | super::recipe::Val::Bool(false)
            ) || matches!(val, super::recipe::Val::Str(s) if s.eq_ignore_ascii_case("off") || s == "0");
            if off {
                std::env::set_var("SE_NVENC_NO_ASYNC", "1");
                env_set.push(("SE_NVENC_NO_ASYNC".into(), "1".into()));
            }
        }
    }
    tuning::apply_process_tuning();

    let sampler = Sampler::start();
    let (rss0, _) = sys::rss();
    let (source, sched, ops) =
        match feed::plan(&spec.workload, spec.clip_path.as_deref(), spec.loops) {
            Ok(x) => x,
            Err(e) => return base(spec, "setup_error", e),
        };
    let (ow, oh) = (sched.width as u32, sched.height as u32);
    let fps = encode_fps(&spec.workload, &source, &sched);
    let pcfg = spec.recipe.production.clone().unwrap_or_default();
    let config = production_config(&pcfg, ow, oh, fps);
    let n_sess = spec.sessions.max(1) as usize;

    let mut sessions: Vec<Session> = Vec::with_capacity(n_sess);
    for k in 0..n_sess {
        match open_with(v, config, input, set) {
            Ok(s) => sessions.push(s),
            Err((rejected, e, d)) => {
                let mut j = if rejected {
                    base(
                        spec,
                        "rejected",
                        format!("session {k}: the encoder refused the recipe: {e}"),
                    )
                } else {
                    skipped(spec, format!("{vendor}: {e}"))
                };
                j["session_index"] = json!(k);
                j["session_create_status"] = json!(-1);
                j["recipe_report"] = d.map(|d| d.to_json()).unwrap_or(Json::Null);
                j["encoder_size"] = json!([ow, oh]);
                j["vendor"] = json!(vendor);
                return j;
            }
        }
    }

    let sched = Arc::new(sched);
    let devices: Vec<ID3D11Device> = sessions.iter().map(|s| s.dev.clone()).collect();
    let (feeder, feeds) = match feed::start(source, Arc::clone(&sched), ops, &devices, input) {
        Ok(x) => x,
        Err(e) => return base(spec, "setup_error", format!("feeder: {e}")),
    };
    let prefilled = feeder.wait_prefill(
        &feeds,
        feed::SLOTS.min(sched.total()),
        Duration::from_secs(60),
    );

    // only `force_idr` has a production meaning here
    let mut idr_at = sched.force_idr.clone();
    let mut event_log = Vec::new();
    for (g, evs) in event_schedule(&spec.events, &sched.at) {
        for ei in evs {
            let ev = &spec.events[ei];
            let t_s = sched.at[g] as f64 / 1e9;
            if ev.action == "force_idr" {
                idr_at[g] = true;
                event_log.push(json!({ "frame": g, "t_s": t_s, "action": "force_idr" }));
            } else {
                event_log.push(json!({ "frame": g, "t_s": t_s, "action": ev.action,
                    "error": "not supported by the Windows lab (force_idr only)" }));
            }
        }
    }
    let idr_at = Arc::new(idr_at);

    let t_start = Instant::now() + Duration::from_millis(20);
    let mut handles = Vec::with_capacity(n_sess);
    for (s, f) in sessions.into_iter().zip(feeds) {
        let sched = Arc::clone(&sched);
        let idr = Arc::clone(&idr_at);
        struct SendPair(Session, FeedSession);
        unsafe impl Send for SendPair {}
        let pair = SendPair(s, f);
        handles.push(
            std::thread::Builder::new()
                .name("bench-lab-session".into())
                .spawn(move || {
                    let pair = pair;
                    let SendPair(s, f) = pair;
                    let (s, t) = run_session(s, f, sched, t_start, idr);
                    struct Back(Session, Track);
                    unsafe impl Send for Back {}
                    Back(s, t)
                })
                .expect("spawn session"),
        );
    }
    let mut done: Vec<(Session, Track)> = Vec::with_capacity(n_sess);
    for h in handles {
        match h.join() {
            Ok(b) => done.push((b.0, b.1)),
            Err(_) => return base(spec, "setup_error", "a session thread panicked".into()),
        }
    }
    let submit_end = Instant::now();
    let report = feeder.finish();

    let frame_bytes = (ow * oh * 4) as usize;
    let outs: Vec<SessOut> = done
        .iter()
        .map(|(s, t)| session_metrics(&s.meta(), t, &sched, v))
        .collect();
    let tr0 = &done[0].1;
    let late_count = tr0.late_ns.iter().filter(|&&l| l > LATE_NS).count();
    let late_harness = late_count - tr0.late_by_encoder.min(late_count);
    let late_ms: Vec<f64> = tr0.late_ns.iter().map(|&l| l as f64 / 1e6).collect();
    let submitted = tr0.late_ns.len();

    let mut reasons: Vec<String> = Vec::new();
    if late_harness as f64 > LATE_SHARE * submitted.max(1) as f64 {
        reasons.push(format!(
            "{late_harness} of {submitted} frames released > 2 ms late by the harness"
        ));
    }
    for (k, (_, t)) in done.iter().enumerate() {
        let hm: Vec<&(usize, &str)> = t
            .feed
            .misses
            .iter()
            .filter(|(_, c)| *c != "consumer")
            .collect();
        if !hm.is_empty() {
            let mut causes: BTreeMap<&str, usize> = BTreeMap::new();
            for (_, c) in &hm {
                *causes.entry(c).or_default() += 1;
            }
            let txt = causes
                .iter()
                .map(|(c, n)| format!("{c} {n}"))
                .collect::<Vec<_>>()
                .join(", ");
            reasons.push(format!(
                "{}{} frames were not on the GPU by their deadline (the harness could not feed this size and rate: {txt})",
                if n_sess > 1 { format!("session {k}: ") } else { String::new() },
                hm.len()
            ));
        }
        if let Some(e) = &t.stopped_early {
            reasons.push(format!("session {k} stopped early: {e}"));
        }
        if t.reader_gap {
            reasons.push("the feeder ended early".into());
        }
    }
    for (k, o) in outs.iter().enumerate() {
        for r in &o.4 {
            reasons.push(if n_sess > 1 {
                format!("session {k}: {r}")
            } else {
                r.clone()
            });
        }
    }
    if let Some(e) = &report.producer_error {
        reasons.push(format!("reader: {e}"));
    }
    if let Some(e) = &report.uploader_error {
        reasons.push(format!("uploader: {e}"));
    }
    if !prefilled {
        reasons.push("the feeder could not prefill its slots within 60 s".into());
    }

    let (rss_end, rss_peak) = sys::rss();
    let mut out = json!({
        "id": spec.id,
        "result": "completed",
        "valid": reasons.is_empty(),
        "invalid_reasons": reasons,
        "stage": "lab",
        "platform": "windows",
        "vendor": vendor,
        "encoder_size": [ow, oh],
        "fps": fps,
        "codec": "h264",
        "frames_per_loop": sched.per_loop,
        "loops": sched.loops,
        "timeline_s": sched.timeline_ns as f64 / 1e9,
        "sessions_n": n_sess,
        "input": { "format": input, "supported": true, "path": path },
        "production": pcfg,
        "encoder_config": {
            "width": config.width, "height": config.height, "fps": config.fps,
            "bitrate_bps": config.bitrate_bps, "qp": config.qp, "min_qp": config.min_qp,
            "profile": format!("{:?}", config.profile),
        },
        "env_set": env_set.iter().map(|(k, v)| json!({ k.as_str(): v })).collect::<Vec<_>>(),
    });
    if let (Some(o), Json::Object(m)) = (out.as_object_mut(), outs[0].0.clone()) {
        o.extend(m);
    }
    let more = json!({
        "pacing": {
            "late_over_2ms": late_count,
            "late_over_2ms_encoder_blocked": tr0.late_by_encoder,
            "late_over_2ms_harness": late_harness,
            "late_ms": summary(&late_ms),
            "reader_underruns": tr0.underruns,
            "read_ahead_depth": report.read_ahead,
            "submit_span_s": submit_end.saturating_duration_since(t_start).as_secs_f64(),
            "timer": tr0.pacer,
        },
        "feeder": tr0.feed.to_json(&sched, frame_bytes, &report, tr0.pacer),
        "events": event_log,
        "rss_mb": { "start": rss0 as f64 / 1e6, "end": rss_end as f64 / 1e6, "peak": rss_peak as f64 / 1e6 },
        "thermal_state": { "start": Json::Null, "end": Json::Null },
        "tuning": crate::windows_utils::streamer::tune::report(),
        "gpu_clock_hold": done[0].0.clock_hold,
        "ltr": if spec.ltr.is_some() { json!({ "error": "the LTR harness is macOS-only; Windows NVENC declares 0 LTR slots unless SE_NVENC_LTR=1" }) } else { Json::Null },
        "psnr": if spec.psnr { json!({ "error": "PSNR needs a decoder; not implemented on Windows" }) } else { Json::Null },
    });
    if let (Some(o), Json::Object(m)) = (out.as_object_mut(), more) {
        o.extend(m);
    }
    if n_sess > 1 {
        let mut lat: Vec<f64> = Vec::new();
        let mut rows: Vec<Row> = Vec::new();
        let mut ev: Vec<(u64, u64)> = Vec::new();
        for o in &outs {
            lat.extend_from_slice(&o.2);
            rows.extend_from_slice(&o.1);
            ev.extend_from_slice(&o.3);
        }
        ev.sort_unstable();
        let sum = |k: &str| -> u64 { outs.iter().filter_map(|o| o.0[k].as_u64()).sum() };
        out["aggregate"] = json!({
            "sessions": n_sess,
            "submitted": sum("submitted"),
            "emitted": sum("emitted"),
            "dropped": sum("dropped"),
            "errored": sum("errored"),
            "missing_callbacks": 0,
            "output_drops": 0,
            "latency_ms": summary(&lat),
            "avg_mbps": outs.iter().filter_map(|o| o.0["avg_mbps"].as_f64()).sum::<f64>(),
            "peak_1s_mbps": peak_rate_bps(&ev, 1_000_000_000) / 1e6,
            "buckets_10s": series::buckets(10, sched.timeline_ns, &rows),
            "buckets_60s": series::buckets(60, sched.timeline_ns, &rows),
        });
        out["per_session"] = Json::Array(
            outs.iter()
                .enumerate()
                .map(|(k, o)| {
                    let mut j = o.0.clone();
                    j["index"] = json!(k);
                    j
                })
                .collect(),
        );
    }
    if let Some(p) = spec.per_frame_path.as_deref() {
        let _ = write_per_frame(Path::new(p), &sched, &done[0].1);
    }
    drop(done);
    out["series"] = sampler.finish();
    out["wall_s"] = json!(wall.elapsed().as_secs_f64());
    out
}

/// The rate a session is configured for: `fps:N`, else the synthetic rate, else the take's own
/// nominal cap (or its measured rate), times `speed`, in production's 15..550 range.
pub(super) fn encode_fps(
    w: &super::super::plan::Workload,
    source: &feed::Source,
    sched: &Schedule,
) -> u32 {
    let speed = if w.speed > 0.0 { w.speed } else { 1.0 };
    let base = match (source, transform_fps(w)) {
        (_, Some(f)) => f,
        (feed::Source::Synthetic { .. }, None) => w.fps as f64,
        (feed::Source::Clip { info, .. }, None) if info.layout.nominal_fps > 0 => {
            info.layout.nominal_fps as f64
        }
        (feed::Source::Clip { .. }, None) => sched.fps / speed,
    };
    ((base * speed).round() as u32).clamp(15, 550)
}

pub(super) fn transform_fps(w: &super::super::plan::Workload) -> Option<f64> {
    feed::Ops::parse(w.transform.as_deref())
        .ok()
        .and_then(|o| o.fps)
}

/// Frame index -> the events that fire on it (the macOS lab's rule).
pub(super) fn event_schedule(
    events: &[super::super::plan::Event],
    at: &[u64],
) -> BTreeMap<usize, Vec<usize>> {
    let total = at.len();
    let mut fire: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (ei, ev) in events.iter().enumerate() {
        let first = match (ev.at_frame, ev.at_s) {
            (Some(f), _) => f as usize,
            (None, Some(s)) => at.partition_point(|&t| (t as f64) < s * 1e9),
            _ => 0,
        };
        if first >= total {
            continue;
        }
        fire.entry(first).or_default().push(ei);
        if let Some(every) = ev.every_s.filter(|e| *e > 0.0) {
            let mut k = 1u64;
            loop {
                let t = at[first] as f64 + every * 1e9 * k as f64;
                let g = at.partition_point(|&x| (x as f64) < t);
                if g >= total {
                    break;
                }
                fire.entry(g).or_default().push(ei);
                k += 1;
            }
        }
    }
    fire
}

/// A session's result JSON, bucket rows, latencies, output events and invalid reasons.
type SessOut = (Json, Vec<Row>, Vec<f64>, Vec<(u64, u64)>, Vec<String>);

/// What a session's metrics need to know about it beside its frames.
struct Meta<'a> {
    create_ms: f64,
    backend: &'a str,
    identity: &'a Json,
    hooks: Option<&'a Disarmed>,
}

impl Session {
    fn meta(&self) -> Meta<'_> {
        Meta {
            create_ms: self.create_ms,
            backend: self.backend,
            identity: &self.identity,
            hooks: self.hooks.as_ref(),
        }
    }
}

/// One session's metrics, under the macOS lab's keys where the meaning is the same.
fn session_metrics(s: &Meta, t: &Track, sched: &Schedule, v: LabVendor) -> SessOut {
    let total = sched.total();
    let mut lat_ms = Vec::new();
    let mut lat_by_loop: Vec<Vec<f64>> = vec![Vec::new(); sched.loops];
    let mut lat_by_10: BTreeMap<u64, Vec<f64>> = BTreeMap::new();
    let (mut bytes, mut key_bytes) = (Vec::<f64>::new(), Vec::<f64>::new());
    let mut out_events: Vec<(u64, u64)> = Vec::new();
    let mut rows: Vec<Row> = Vec::with_capacity(total);
    let mut stage_ms = Vec::new();
    let mut call_us = Vec::new();
    let mut qps = Vec::new();
    let mut slices = Vec::new();
    let (mut submitted, mut submitted_ok, mut dropped, mut errored, mut busy) = (0, 0, 0, 0, 0);
    let mut types: BTreeMap<&str, usize> = BTreeMap::new();
    let mut total_bytes = 0u64;
    let (mut run, mut longest) = (0usize, 0usize);
    for g in 0..total {
        let st = t.status[g];
        if st == 3 {
            continue;
        }
        submitted += 1;
        let mut row = Row {
            t_ns: sched.at[g],
            ..Default::default()
        };
        call_us.push(t.done_ns[g].saturating_sub(t.submit_ns[g] - t.stage_ns[g]) as f64 / 1e3);
        match st {
            0 => {
                submitted_ok += 1;
                let b = t.bytes[g];
                if b == 0 && v == LabVendor::None {
                    run = 0;
                } else if b == 0 {
                    dropped += 1;
                    row.dropped = true;
                    run += 1;
                } else {
                    run = 0;
                    let l = t.done_ns[g].saturating_sub(t.submit_ns[g]) as f64 / 1e6;
                    lat_ms.push(l);
                    lat_by_loop[(g / sched.per_loop.max(1)).min(sched.loops - 1)].push(l);
                    lat_by_10
                        .entry(sched.at[g] / 10_000_000_000)
                        .or_default()
                        .push(l);
                    row.lat_ms = Some(l);
                    row.bytes = b;
                    bytes.push(b as f64);
                    if matches!(t.pic[g], PicType::Idr | PicType::I) {
                        key_bytes.push(b as f64);
                    }
                    total_bytes += b;
                    out_events.push((t.done_ns[g], b));
                    *types.entry(t.pic[g].name()).or_default() += 1;
                    slices.push(t.slices[g] as f64);
                    if t.qp[g] >= 0 {
                        qps.push(t.qp[g] as f64);
                    }
                    if t.stage_ns[g] > 0 {
                        stage_ms.push(t.stage_ns[g] as f64 / 1e6);
                    }
                }
            }
            1 => {
                busy += 1;
                dropped += 1;
                row.dropped = true;
                run += 1;
            }
            _ => {
                errored += 1;
                row.errored = true;
                run += 1;
            }
        }
        longest = longest.max(run);
        rows.push(row);
    }
    out_events.sort_unstable();
    let gaps: Vec<f64> = out_events
        .windows(2)
        .map(|w| (w[1].0 - w[0].0) as f64 / 1e6)
        .collect();
    let secs = sched.timeline_ns as f64 / 1e9;
    let mut reasons = Vec::new();
    if submitted < total {
        reasons.push(format!("only {submitted} of {total} frames submitted"));
    }
    if v != LabVendor::None && submitted_ok > 0 && bytes.is_empty() {
        reasons.push("the encoder emitted no pictures".into());
    }
    let params = avc::params_json(&t.avc);
    // bitstream read-backs for the fields a parameter set shows
    let mut recipe = s.hooks.map(|d| d.to_json());
    if let (Some(r), Some(sps)) = (recipe.as_mut(), t.avc.sps.as_ref()) {
        if let Some(fields) = r["fields"].as_array_mut() {
            for f in fields {
                let name = f["field"].as_str().unwrap_or("").to_string();
                let seen = match name.as_str() {
                    "h264.maxNumRefFrames" | "mfx.NumRefFrame" => {
                        Some(json!(sps.max_num_ref_frames))
                    }
                    "h264.level" | "mfx.CodecLevel" => Some(json!(sps.level_idc)),
                    "h264.entropyCodingMode" | "co.CAVLC" => t
                        .avc
                        .pps
                        .as_ref()
                        .map(|p| json!(if p.cabac { "cabac" } else { "cavlc" })),
                    "h264.vui.bitstreamRestrictionFlag" | "co3.BitstreamRestriction" => {
                        Some(json!(sps.vui.as_ref().map(|v| v.bitstream_restriction)))
                    }
                    "h264.sliceMode" | "h264.sliceModeData" | "mfx.NumSlice"
                    | "co2.MaxSliceSize" => Some(json!(summary(&slices))),
                    _ => None,
                };
                if let Some(seen) = seen {
                    f["bitstream"] = seen;
                }
            }
        }
    }
    let errors: serde_json::Map<String, Json> = t
        .errors
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    let j = json!({
        "session_create_status": 0,
        "session_create_ms": s.create_ms,
        "identity": {
            "encoder_id": v.name(),
            "hardware": !matches!(v, LabVendor::X264 | LabVendor::None),
            "backend": s.backend,
            "details": s.identity,
        },
        "recipe_report": recipe,
        "submitted": submitted,
        "submitted_ok": submitted_ok,
        "callbacks": submitted,
        "emitted": bytes.len(),
        "no_pictures": bytes.is_empty(),
        "dropped": dropped,
        "dropped_pct": if submitted > 0 { (dropped as f64 * 1e4 / submitted as f64).round() / 100.0 } else { 0.0 },
        "device_busy": busy,
        "errored": errored,
        "errored_by_status": errors,
        "first_callback_error": t.first_error.as_ref().map(|(g, e)| json!({ "frame": g, "error": e })),
        "missing_callbacks": 0,
        "unknown_tags": 0,
        "longest_drop_run": longest,
        "encode_call_failures": errored,
        "encode_call_us": summary(&call_us),
        "latency_ms": summary(&lat_ms),
        "stage_ms": summary(&stage_ms),
        "latency_ms_by_loop": lat_by_loop.iter().map(|v| summary(v)).collect::<Vec<_>>(),
        "latency_ms_by_10s": lat_by_10.iter().map(|(b, v)| json!({ "from_s": b * 10, "stats": summary(v) })).collect::<Vec<_>>(),
        "buckets_10s": series::buckets(10, sched.timeline_ns, &rows),
        "buckets_60s": series::buckets(60, sched.timeline_ns, &rows),
        "output_gap_ms": summary(&gaps),
        "frame_bytes": summary(&bytes),
        "keyframes": key_bytes.len(),
        "keyframe_bytes": summary(&key_bytes),
        "frame_types": types,
        "slices_per_picture": summary(&slices),
        "slice_qp": summary(&qps),
        "avg_mbps": total_bytes as f64 * 8.0 / secs.max(1e-9) / 1e6,
        "peak_1s_mbps": peak_rate_bps(&out_events, 1_000_000_000) / 1e6,
        "peak_100ms_mbps": peak_rate_bps(&out_events, 100_000_000) / 1e6,
        "sps": params,
        "output_drops": 0,
        "max_in_flight": 1,
    });
    (j, rows, lat_ms, out_events, reasons)
}

fn write_per_frame(path: &Path, sched: &Schedule, t: &Track) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(
        f,
        "frame,loop,src,due_ms,submit_ms,late_us,stage_us,done_ms,latency_ms,status,bytes,type,slices,qp,read_ms,transform_ms,upload_ms,upload_lead_ms"
    )?;
    for g in 0..sched.total() {
        let sub = t.status[g] != 3;
        let opt = |b: bool, s: String| if b { s } else { String::new() };
        writeln!(
            f,
            "{g},{},{},{:.3},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            g / sched.per_loop.max(1),
            sched.src[g],
            sched.at[g] as f64 / 1e6,
            opt(sub, format!("{:.3}", t.submit_ns[g] as f64 / 1e6)),
            t.late_ns
                .get(g)
                .map(|l| (l / 1000).to_string())
                .unwrap_or_default(),
            opt(sub, (t.stage_ns[g] / 1000).to_string()),
            opt(sub, format!("{:.3}", t.done_ns[g] as f64 / 1e6)),
            opt(
                sub && t.status[g] == 0 && t.bytes[g] > 0,
                format!(
                    "{:.3}",
                    t.done_ns[g].saturating_sub(t.submit_ns[g]) as f64 / 1e6
                )
            ),
            t.status[g],
            t.bytes[g],
            t.pic[g].name(),
            t.slices[g],
            if t.qp[g] >= 0 {
                t.qp[g].to_string()
            } else {
                String::new()
            },
            t.feed
                .read_ms
                .get(g)
                .map(|v| format!("{v:.3}"))
                .unwrap_or_default(),
            t.feed
                .xform_ms
                .get(g)
                .map(|v| format!("{v:.3}"))
                .unwrap_or_default(),
            t.feed
                .upload_ms
                .get(g)
                .map(|v| format!("{v:.3}"))
                .unwrap_or_default(),
            t.feed
                .lead_ms
                .get(g)
                .map(|v| format!("{v:.3}"))
                .unwrap_or_default(),
        )?;
    }
    f.flush()
}

/// What a preference resolves to, for the tests and `env`.
#[allow(dead_code)]
fn preference_for(v: &str) -> Option<GpuPreference> {
    LabVendor::parse(v).map(|v| capture_gpu_preference(v.encoder_vendor()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_production_config_is_live_encoder_config_arithmetic() {
        let c = production_config(
            &ProductionCfg {
                qp: Some(15),
                ..Default::default()
            },
            1920,
            1080,
            60,
        );
        assert_eq!(c.bitrate_bps, 12_441_600);
        assert_eq!(c.qp, Some(15));
        assert_eq!(c.profile, H264Profile::High);
        let small = production_config(&ProductionCfg::default(), 640, 360, 30);
        assert_eq!(small.bitrate_bps, 6_000_000, "the floor");
        let big = production_config(&ProductionCfg::default(), 3840, 2160, 120);
        assert_eq!(big.bitrate_bps, 30_000_000, "the ceiling");
        assert!(!c.intra_refresh, "live joins never set intra refresh");
    }

    #[test]
    fn vendors_map_to_production_preferences_and_inputs() {
        assert_eq!(preference_for("nvenc"), Some(GpuPreference::Vendor(0x10DE)));
        assert_eq!(preference_for("qsv"), Some(GpuPreference::Vendor(0x8086)));
        assert_eq!(preference_for("x264"), Some(GpuPreference::MinimumPower));
        assert_eq!(preference_for("bogus"), None);
        assert!(input_path(LabVendor::Nvenc, Input::Nv12).is_err());
        assert!(input_path(LabVendor::Qsv, Input::Nv12).is_ok());
        assert!(input_path(LabVendor::X264, Input::Nv12).is_ok());
    }

    fn sched(n: usize, loops: usize) -> Schedule {
        let at: Vec<u64> = (0..n * loops).map(|g| g as u64 * 10_000_000).collect();
        Schedule {
            at,
            src: (0..n * loops).map(|g| g % n).collect(),
            force_idr: vec![false; n * loops],
            per_loop: n,
            loops,
            period_ns: n as u64 * 10_000_000,
            timeline_ns: (n * loops) as u64 * 10_000_000,
            width: 64,
            height: 32,
            fps: 100.0,
        }
    }

    #[test]
    fn session_metrics_assemble_under_the_macos_keys() {
        let sc = sched(4, 2);
        let mut t = Track::new(8);
        for g in 0..8 {
            t.status[g] = 0;
            t.submit_ns[g] = sc.at[g] + 100_000;
            t.done_ns[g] = sc.at[g] + 2_100_000;
            t.bytes[g] = 1000;
            t.pic[g] = if g == 0 { PicType::Idr } else { PicType::P };
            t.slices[g] = 1;
            t.qp[g] = 20;
            t.late_ns.push(0);
        }
        t.bytes[3] = 0; // a rate drop
        t.status[5] = 1; // device busy
        t.status[6] = 2; // error
        t.status[7] = 3; // never submitted
        let id = json!({});
        let s = Meta {
            create_ms: 1.0,
            backend: "none",
            identity: &id,
            hooks: None,
        };
        let (j, rows, lat, _, reasons) = session_metrics(&s, &t, &sc, LabVendor::Nvenc);
        assert_eq!(j["submitted"], 7);
        assert_eq!(j["submitted_ok"], 5);
        assert_eq!(j["emitted"], 4);
        assert_eq!(j["dropped"], 2, "the rate drop and the busy frame");
        assert_eq!(j["device_busy"], 1);
        assert_eq!(j["errored"], 1);
        assert_eq!(j["keyframes"], 1);
        assert_eq!(j["frame_types"]["IDR"], 1);
        assert_eq!(j["frame_types"]["P"], 3);
        assert_eq!(j["latency_ms"]["p50"], 2.0);
        assert_eq!(j["slice_qp"]["p50"], 20.0);
        assert_eq!(rows.len(), 7);
        assert_eq!(lat.len(), 4);
        assert_eq!(reasons, vec!["only 7 of 8 frames submitted".to_string()]);
        assert_eq!(j["longest_drop_run"], 2);
    }

    #[test]
    fn the_windows_plans_expand_and_every_recipe_field_resolves() {
        use super::super::super::plan::{Plan, WIN_ENCODERS, WIN_QUICK};
        let q = Plan::parse(WIN_QUICK).unwrap();
        let qr = q.expand().unwrap();
        assert_eq!(qr.len(), 8, "4 vendors x 2 workloads");
        let e = Plan::parse(WIN_ENCODERS).unwrap();
        let er = e.expand().unwrap();
        assert_eq!(er.len(), 27 * 48);
        for r in e.recipes.iter().chain(q.recipes.iter()) {
            let v = r
                .recipe
                .vendor
                .as_deref()
                .unwrap_or_else(|| panic!("{} names no vendor", r.recipe.name));
            let lv = LabVendor::parse(v).unwrap();
            assert!(Input::parse(r.recipe.input.as_deref()).is_ok());
            if let Some(h) = lv.hook() {
                if h != HookVendor::X264 {
                    let check = hooks::validate(h, &r.recipe.set, None);
                    assert!(check.ok(), "{}: {:?}", r.recipe.name, check.report);
                }
            }
        }
        // 4K and 1440p from a 1080p take: the transforms chain
        let w = e.workloads.iter().find(|w| w.id == "rec-1440p60").unwrap();
        let ops = feed::Ops::parse(w.transform.as_deref()).unwrap();
        assert_eq!(ops.out_dims(1920, 1080).unwrap(), (2560, 1440));
        assert_eq!(ops.fps, Some(60.0));
        assert!(e
            .workloads
            .iter()
            .filter(|w| w.kind == "synthetic")
            .all(feed::renders_live));
    }

    #[test]
    fn events_fire_on_the_frames_the_macos_rule_picks() {
        let at: Vec<u64> = (0..100).map(|g| g * 10_000_000).collect();
        let ev: Vec<super::super::super::plan::Event> = serde_json::from_value(json!([
            { "at_s": 0.25, "every_s": 0.3, "action": "force_idr" },
            { "at_frame": 7, "action": "set", "key": "x" },
        ]))
        .unwrap();
        let f = event_schedule(&ev, &at);
        assert_eq!(f[&25], vec![0]);
        assert_eq!(f[&55], vec![0]);
        assert_eq!(f[&85], vec![0]);
        assert_eq!(f[&7], vec![1]);
    }
}

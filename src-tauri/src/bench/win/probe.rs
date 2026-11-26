//! Probe stage on Windows. A row names a vendor in `hw` (`nvenc` | `qsv` | `amf` | `x264`); the
//! child opens that vendor's production encoder on its own device, reads back what it reports,
//! encodes three synthetic frames and parses the SPS. A vendor without hardware reports
//! `session_create_status: -1` and the error.

use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use super::super::metrics;
use super::super::plan::{Catalogue, ProbeRow};
use super::recipe::Codec;
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeSpec {
    pub row: ProbeRow,
    pub catalogue: Catalogue,
    pub result_path: String,
}

pub const VENDORS: [&str; 4] = ["nvenc", "qsv", "amf", "x264"];

/// The default matrix: every vendor at 1080p, plus NVENC and Quick Sync at 4K.
pub fn default_rows() -> Vec<ProbeRow> {
    let row = |hw: &str, width, height| ProbeRow {
        codec: Codec::H264,
        low_latency: true,
        hw: hw.into(),
        width,
        height,
    };
    let mut rows: Vec<ProbeRow> = VENDORS.iter().map(|v| row(v, 1920, 1080)).collect();
    rows.push(row("nvenc", 3840, 2160));
    rows.push(row("qsv", 3840, 2160));
    rows
}

/// Split an Annex-B access unit into NAL units (start codes removed).
fn nal_units(au: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(au.len(), |&n| n - 3);
        // a four-byte start code leaves its leading zero on the previous unit
        while e > s && au[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&au[s..e]);
        }
    }
    out
}

enum Session {
    Nvenc(Box<crate::windows_utils::streamer::nvidia::encoder::Encoder>),
    Qsv(Box<crate::windows_utils::streamer::intel::encoder::Encoder>),
    Amf(Box<crate::windows_utils::streamer::amd::encoder::Encoder>),
    X264(Box<crate::windows_utils::streamer::x264::encoder::X264Encoder>),
}

impl Session {
    fn open(vendor: &str, config: EncoderConfig) -> anyhow::Result<Session> {
        use crate::windows_utils::streamer::{amd, intel, nvidia, x264};
        Ok(match vendor {
            "nvenc" => Session::Nvenc(Box::new(nvidia::encoder::Encoder::new(config)?)),
            "qsv" => Session::Qsv(Box::new(intel::encoder::Encoder::new(config)?)),
            "amf" => Session::Amf(Box::new(amd::encoder::Encoder::new(config)?)),
            "x264" => Session::X264(Box::new(x264::encoder::X264Encoder::new(config)?)),
            other => anyhow::bail!("unknown vendor '{other}' ({})", VENDORS.join(" | ")),
        })
    }

    fn encode(&mut self, bgra: &[u8], width: u32, idr: bool) -> anyhow::Result<Vec<u8>> {
        match self {
            Session::Nvenc(e) => e.encode_bgra(bgra, idr),
            Session::Qsv(e) => e.encode_bgra(bgra, idr),
            Session::Amf(e) => e.encode_bgra_padded(bgra, width * 4, idr),
            Session::X264(e) => e.encode_bgra(bgra, idr),
        }
    }

    /// What the encoder reports about itself, and how many fields that is.
    fn identity(&mut self) -> (Json, usize) {
        match self {
            Session::Nvenc(e) => {
                let c = e.caps();
                let caps = json!({
                    "width_min": c.width_min, "width_max": c.width_max,
                    "height_min": c.height_min, "height_max": c.height_max,
                    "rc_modes": format!("0x{:x}", c.rc_modes),
                    "cabac": c.cabac, "adaptive_transform": c.adaptive_transform,
                    "constrained_encoding": c.constrained_encoding,
                    "intra_refresh": c.intra_refresh,
                    "single_slice_intra_refresh": c.single_slice_intra_refresh,
                    "custom_vbv_buf_size": c.custom_vbv_buf_size,
                    "dyn_bitrate_change": c.dyn_bitrate_change,
                    "dyn_rcmode_change": c.dyn_rcmode_change,
                    "dynamic_slice_mode": c.dynamic_slice_mode,
                    "subframe_readback": c.subframe_readback,
                    "async_encode": c.async_encode,
                    "ref_pic_invalidation": c.ref_pic_invalidation,
                    "multiple_ref_frames": c.multiple_ref_frames,
                    "max_ltr_frames": c.max_ltr_frames,
                    "num_encoder_engines": c.num_encoder_engines,
                    "level_max": c.level_max,
                });
                let extra = json!({
                    "caps": caps,
                    "ltr_slots": e.ltr_slots(),
                    "pipeline_depth": e.pipeline_depth(),
                    "constant_qp": e.constant_qp(),
                    "adapter": crate::windows_utils::windows_capture::d3d11::device_adapter_info(e.device())
                        .map(|a| a.label()),
                });
                (extra, 20)
            }
            Session::Qsv(e) => (
                json!({
                    "slice_mode": format!("{:?}", e.slice_mode()),
                    "adapter": crate::windows_utils::windows_capture::d3d11::device_adapter_info(e.device())
                        .map(|a| a.label()),
                }),
                1,
            ),
            Session::Amf(e) => (
                json!({
                    "caps": format!("{:?}", e.caps()),
                    "ltr_slots": e.ltr_slots(),
                    "adapter": crate::windows_utils::windows_capture::d3d11::device_adapter_info(e.device())
                        .map(|a| a.label()),
                }),
                1,
            ),
            Session::X264(e) => (json!({ "x264_build": e.build() }), 1),
        }
    }

    fn stage_medians(&self) -> Option<Json> {
        match self {
            Session::Qsv(e) => {
                let (a, b, c) = e.stage_medians_us();
                Some(json!({ "stage_medians_us": [a, b, c] }))
            }
            _ => None,
        }
    }
}

pub fn run(spec: &ProbeSpec) -> Json {
    let row = &spec.row;
    let id = row.id();
    let vendor = row.hw.as_str();
    let mut out = json!({
        "id": id,
        "row": row,
        "vendor": vendor,
        "platform": "windows",
        "trials": [],
        "catalogue_note": "the plan catalogue holds VideoToolbox keys; not applicable on Windows",
    });
    if row.codec != Codec::H264 {
        out["session_create_status"] = json!(-1);
        out["error"] = json!("the Windows encoders are H.264 only");
        out["supported_key_count"] = json!(0);
        return out;
    }
    let config = EncoderConfig {
        width: row.width,
        height: row.height,
        fps: 60,
        bitrate_bps: 20_000_000,
        max_bitrate_bps: 40_000_000,
        profile: crate::streamer::config::H264Profile::High,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    };
    let t = Instant::now();
    let opened = Session::open(vendor, config);
    let create_ms = t.elapsed().as_secs_f64() * 1e3;
    out["create_ms"] = json!((create_ms * 100.0).round() / 100.0);
    let mut session = match opened {
        Ok(s) => s,
        Err(e) => {
            out["session_create_status"] = json!(-1);
            out["error"] = json!(format!("{e:#}"));
            out["identity"] = json!({ "encoder_id": vendor, "hardware": Json::Null });
            out["supported_key_count"] = json!(0);
            return out;
        }
    };
    out["session_create_status"] = json!(0);
    let (identity, keys) = session.identity();
    out["identity"] = json!({
        "encoder_id": vendor,
        "hardware": vendor != "x264",
        "details": identity,
    });
    out["supported_key_count"] = json!(keys);

    let (w, h) = (row.width, row.height);
    let mut bgra = vec![0u8; w as usize * h as usize * 4];
    let mut frames = Vec::new();
    let mut params: Vec<Vec<u8>> = Vec::new();
    for i in 0..3u32 {
        crate::streamer::synthetic::fill(
            &mut bgra,
            w,
            h,
            i,
            crate::streamer::synthetic::Workload::Motion,
        );
        let t = Instant::now();
        let r = session.encode(&bgra, w, i == 0);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        match r {
            Ok(au) => {
                let nals = nal_units(&au);
                let types: Vec<u8> = nals.iter().map(|n| n[0] & 0x1f).collect();
                if params.is_empty() {
                    params = nals
                        .iter()
                        .filter(|n| matches!(n[0] & 0x1f, 7 | 8))
                        .map(|n| n.to_vec())
                        .collect();
                }
                frames.push(json!({
                    "ms": (ms * 100.0).round() / 100.0,
                    "bytes": au.len(),
                    "idr": types.contains(&5),
                    "nal_types": types,
                }));
            }
            Err(e) => frames.push(json!({ "ms": ms, "error": format!("{e:#}") })),
        }
    }
    out["encode"] = json!(frames);
    out["sps"] = metrics::h264_sps_info(&params);
    if let Some(m) = session.stage_medians() {
        out["stage"] = m;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_splits_on_three_and_four_byte_start_codes() {
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9,
        ];
        let n = nal_units(&au);
        assert_eq!(n, [&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
    }

    #[test]
    fn the_default_rows_name_every_vendor_and_have_unique_ids() {
        let rows = default_rows();
        let ids: std::collections::HashSet<String> = rows.iter().map(ProbeRow::id).collect();
        assert_eq!(ids.len(), rows.len());
        for v in VENDORS {
            assert!(rows.iter().any(|r| r.hw == v), "{v}");
        }
        assert_eq!(rows[0].id(), "h264.ll.nvenc.1920x1080");
    }
}

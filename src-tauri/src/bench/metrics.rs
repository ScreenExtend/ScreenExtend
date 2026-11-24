//! Statistics and bitstream header parsing.

use serde_json::{json, Value as Json};

/// Nearest-rank percentile over a sorted slice (the same rule as `test_support::percentiles`).
fn rank(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() as f64) * p).floor() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn r3(v: f64) -> Json {
    if v.is_finite() {
        json!((v * 1000.0).round() / 1000.0)
    } else {
        Json::Null
    }
}

/// `{n, mean, p50, p90, p95, p99, p999, max}`; `{n: 0}` when empty.
pub fn summary(values: &[f64]) -> Json {
    if values.is_empty() {
        return json!({ "n": 0 });
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    json!({
        "n": v.len(),
        "mean": r3(mean),
        "p50": r3(rank(&v, 0.50)),
        "p90": r3(rank(&v, 0.90)),
        "p95": r3(rank(&v, 0.95)),
        "p99": r3(rank(&v, 0.99)),
        "p999": r3(rank(&v, 0.999)),
        "max": r3(*v.last().unwrap()),
    })
}

/// Low-tail summary for quality numbers: `{n, mean, p50, p5, p1, min}`.
pub fn low_summary(values: &[f64]) -> Json {
    if values.is_empty() {
        return json!({ "n": 0 });
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    json!({
        "n": v.len(),
        "mean": r3(mean),
        "p50": r3(rank(&v, 0.50)),
        "p5": r3(rank(&v, 0.05)),
        "p1": r3(rank(&v, 0.01)),
        "min": r3(v[0]),
    })
}

/// Largest number of bytes delivered within any `window_ns` span, as bits per second.
/// `events` = (time ns, bytes), sorted by time.
pub fn peak_rate_bps(events: &[(u64, u64)], window_ns: u64) -> f64 {
    let (mut lo, mut sum, mut best) = (0usize, 0u64, 0u64);
    for hi in 0..events.len() {
        sum += events[hi].1;
        while events[hi].0 - events[lo].0 >= window_ns {
            sum -= events[lo].1;
            lo += 1;
        }
        best = best.max(sum);
    }
    best as f64 * 8.0 * 1e9 / window_ns as f64
}

/// Remove emulation-prevention bytes.
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// H.264: from the SPS (first parameter set, NAL header included).
pub fn h264_sps_info(param_sets: &[Vec<u8>]) -> Json {
    let Some(sps) = param_sets
        .iter()
        .find(|p| !p.is_empty() && p[0] & 0x1f == 7)
    else {
        return json!({ "error": "no SPS" });
    };
    if sps.len() < 4 {
        return json!({ "error": "short SPS", "sps_hex": hex(sps) });
    }
    json!({
        "profile_idc": sps[1],
        "constraint_flags": format!("0x{:02x}", sps[2]),
        "level_idc": sps[3],
        "max_num_ref_frames": h264_max_num_ref_frames(sps),
        "sps_hex": hex(sps),
    })
}

/// HEVC: general profile/tier/level from the SPS (`profile_tier_level` right after the first
/// 16 bits of the SPS payload).
pub fn hevc_sps_info(param_sets: &[Vec<u8>]) -> Json {
    let Some(sps) = param_sets
        .iter()
        .find(|p| p.len() > 2 && (p[0] >> 1) & 0x3f == 33)
    else {
        return json!({ "error": "no SPS" });
    };
    let r = rbsp(&sps[2..]);
    // sps_video_parameter_set_id(4) max_sub_layers_minus1(3) temporal_id_nesting(1) = 1 byte,
    // then general_profile_space(2) tier(1) profile_idc(5), 4 bytes compat, 6 bytes constraint,
    // general_level_idc(8).
    if r.len() < 13 {
        return json!({ "error": "short SPS", "sps_hex": hex(sps) });
    }
    json!({
        "general_profile_space": r[1] >> 6,
        "general_tier_flag": (r[1] >> 5) & 1,
        "general_profile_idc": r[1] & 0x1f,
        "general_level_idc": r[12],
        "sps_hex": hex(sps),
    })
}

/// Exp-Golomb reader over an RBSP.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.b.get(self.pos / 8)?;
        let v = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(v as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
}

/// H.264 `max_num_ref_frames` from an SPS NAL (header included). `None` where the SPS uses a
/// scaling matrix or a field-order mode this reader does not walk.
pub fn h264_max_num_ref_frames(sps: &[u8]) -> Option<u32> {
    if sps.len() < 4 || sps[0] & 0x1f != 7 {
        return None;
    }
    let r = rbsp(&sps[1..]);
    let mut b = Bits { b: &r, pos: 0 };
    let profile = b.bits(8)?;
    b.bits(16)?; // constraint flags + level
    b.ue()?; // seq_parameter_set_id
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        if b.ue()? == 3 {
            b.bit()?;
        }
        b.ue()?;
        b.ue()?;
        b.bit()?;
        if b.bit()? == 1 {
            return None; // seq_scaling_matrix_present_flag
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.bit()?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_rate_over_a_window() {
        let ev = [
            (0, 100),
            (50_000_000, 100),
            (150_000_000, 1000),
            (160_000_000, 1000),
        ];
        // 100 ms window: the last two land 10 ms apart.
        assert_eq!(peak_rate_bps(&ev, 100_000_000), 2000.0 * 8.0 * 10.0);
    }

    #[test]
    fn summary_matches_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = summary(&v);
        assert_eq!(s["p50"], json!(51.0));
        assert_eq!(s["p99"], json!(100.0));
    }

    #[test]
    fn max_num_ref_frames_from_real_sps() {
        // SPS bytes VideoToolbox emitted in BENCH-M2 runs on an M1 (macOS 26.7.1), hand-decoded.
        let hex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        // Low-latency encoder, BaseFrameQP 15 recipe: level 4.2, max_num_ref_frames 4.
        let ll = hex("27640c2aac1314501e0089f966e0202020f0804258");
        assert_eq!(h264_max_num_ref_frames(&ll), Some(4));
        // Low-latency encoder under the 300 Mbps constant-QP ceiling: level 5.1, 15.
        let qp = hex("27640c33ac13141001e0089f966e0202020f08042108");
        assert_eq!(h264_max_num_ref_frames(&qp), Some(15));
        // The standard hardware encoder (`ave.avc`): 1.
        let std = hex("2764002aac5680780227e59b80808081");
        assert_eq!(h264_max_num_ref_frames(&std), Some(1));
        assert_eq!(h264_max_num_ref_frames(&[0x68, 1, 2, 3]), None);
    }

    #[test]
    fn h264_sps_bytes() {
        let s = h264_sps_info(&[vec![0x67, 100, 0x0c, 51, 0xaa]]);
        assert_eq!(s["level_idc"], json!(51));
        assert_eq!(s["profile_idc"], json!(100));
    }
}

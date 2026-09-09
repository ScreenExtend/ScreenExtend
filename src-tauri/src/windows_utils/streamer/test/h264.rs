//! Annex-B walking and just enough SPS parsing to assert what an encoder actually emitted.
//!
//! Asserting "SPS + PPS + IDR are present" is satisfied by a stream no browser can decode —
//! that is how `blocker-x264-bgra-input-emits-444-stream` passed its own test for months. The
//! decoded `profile_idc` / `chroma_format_idc` are what make the assertion mean something.

/// `(nal_type, payload_len)` for every NAL in an Annex-B access unit.
pub(crate) fn nal_units(au: &[u8]) -> Vec<(u8, usize)> {
    nal_slices(au)
        .into_iter()
        .map(|(t, p)| (t, p.len()))
        .collect()
}

/// `(nal_type, payload)` for every NAL, payload excluding the one-byte NAL header.
pub(crate) fn nal_slices(au: &[u8]) -> Vec<(u8, &[u8])> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if i + 3 < au.len() {
                starts.push(i + 3);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (idx, &hdr) in starts.iter().enumerate() {
        let mut end = starts.get(idx + 1).map(|&s| s - 3).unwrap_or(au.len());
        while end > hdr && au[end - 1] == 0 {
            end -= 1;
        }
        out.push((au[hdr] & 0x1f, &au[hdr + 1..end.max(hdr + 1)]));
    }
    out
}

pub(crate) fn starts_with_annexb(au: &[u8]) -> bool {
    au.starts_with(&[0, 0, 1]) || au.starts_with(&[0, 0, 0, 1])
}

pub(crate) fn contains(nals: &[(u8, usize)], nal_type: u8) -> bool {
    nals.iter().any(|&(t, _)| t == nal_type)
}

/// SPS(7) + PPS(8) + IDR(5), the in-band-headers keyframe shape every client needs.
pub(crate) fn is_idr_with_headers(au: &[u8]) -> bool {
    let nals = nal_units(au);
    contains(&nals, 7) && contains(&nals, 8) && contains(&nals, 5)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sps {
    pub profile_idc: u8,
    pub level_idc: u8,
    /// 1 for 4:2:0 — either decoded from the bitstream or the standard's default for a profile
    /// that does not carry the field.
    pub chroma_format_idc: u32,
    pub max_num_ref_frames: u32,
    pub vui_present: bool,
    pub bitstream_restriction: bool,
    /// Only meaningful when `bitstream_restriction` is set.
    pub max_num_reorder_frames: u32,
}

/// The profiles whose SPS carries `chroma_format_idc` (H.264 7.3.2.1.1).
const HIGH_PROFILES: &[u8] = &[100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Parse the first SPS in an access unit. Returns `None` if there is no SPS or it is truncated.
pub(crate) fn parse_sps(au: &[u8]) -> Option<Sps> {
    let (_, payload) = nal_slices(au).into_iter().find(|&(t, _)| t == 7)?;
    let rbsp = unescape(payload);
    let mut r = BitReader::new(&rbsp);

    let profile_idc = r.bits(8)? as u8;
    let _constraints = r.bits(8)?;
    let level_idc = r.bits(8)? as u8;
    r.ue()?; // seq_parameter_set_id

    let mut chroma_format_idc = 1;
    if HIGH_PROFILES.contains(&profile_idc) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.bits(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.bits(1)? == 1 {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..lists {
                if r.bits(1)? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    r.ue()?; // log2_max_frame_num_minus4
    let poc_type = r.ue()?;
    if poc_type == 0 {
        r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if poc_type == 1 {
        r.bits(1)?; // delta_pic_order_always_zero_flag
        r.se()?; // offset_for_non_ref_pic
        r.se()?; // offset_for_top_to_bottom_field
        let n = r.ue()?;
        for _ in 0..n {
            r.se()?;
        }
    }
    let max_num_ref_frames = r.ue()?;
    r.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    let frame_mbs_only = r.bits(1)?;
    if frame_mbs_only == 0 {
        r.bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.bits(1)?; // direct_8x8_inference_flag
    if r.bits(1)? == 1 {
        // frame_cropping_flag
        r.ue()?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
    }

    let vui_present = r.bits(1)? == 1;
    let mut bitstream_restriction = false;
    let mut max_num_reorder_frames = 0;
    if vui_present {
        if let Some((br, reorder)) = parse_vui(&mut r) {
            bitstream_restriction = br;
            max_num_reorder_frames = reorder;
        }
    }

    Some(Sps {
        profile_idc,
        level_idc,
        chroma_format_idc,
        max_num_ref_frames,
        vui_present,
        bitstream_restriction,
        max_num_reorder_frames,
    })
}

/// Returns `(bitstream_restriction_flag, max_num_reorder_frames)`.
fn parse_vui(r: &mut BitReader) -> Option<(bool, u32)> {
    if r.bits(1)? == 1 {
        // aspect_ratio_info_present_flag
        let idc = r.bits(8)?;
        if idc == 255 {
            r.bits(16)?;
            r.bits(16)?;
        }
    }
    if r.bits(1)? == 1 {
        r.bits(1)?; // overscan_appropriate_flag
    }
    if r.bits(1)? == 1 {
        // video_signal_type_present_flag
        r.bits(3)?; // video_format
        r.bits(1)?; // video_full_range_flag
        if r.bits(1)? == 1 {
            r.bits(24)?; // colour_primaries / transfer / matrix
        }
    }
    if r.bits(1)? == 1 {
        // chroma_loc_info_present_flag
        r.ue()?;
        r.ue()?;
    }
    if r.bits(1)? == 1 {
        // timing_info_present_flag
        r.bits(32)?;
        r.bits(32)?;
        r.bits(1)?;
    }
    let nal_hrd = r.bits(1)? == 1;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.bits(1)? == 1;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.bits(1)?; // low_delay_hrd_flag
    }
    r.bits(1)?; // pic_struct_present_flag
    if r.bits(1)? == 0 {
        return Some((false, 0));
    }
    // bitstream_restriction_flag
    r.bits(1)?; // motion_vectors_over_pic_boundaries_flag
    r.ue()?; // max_bytes_per_pic_denom
    r.ue()?; // max_bits_per_mb_denom
    r.ue()?; // log2_max_mv_length_horizontal
    r.ue()?; // log2_max_mv_length_vertical
    let reorder = r.ue()?;
    Some((true, reorder))
}

fn skip_hrd(r: &mut BitReader) -> Option<()> {
    let cpb_cnt = r.ue()? + 1;
    r.bits(4)?; // bit_rate_scale
    r.bits(4)?; // cpb_size_scale
    for _ in 0..cpb_cnt {
        r.ue()?;
        r.ue()?;
        r.bits(1)?;
    }
    r.bits(5)?;
    r.bits(5)?;
    r.bits(5)?;
    r.bits(5)?;
    Some(())
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

/// Strip H.264 emulation-prevention bytes (`00 00 03` -> `00 00`).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos >> 3)?;
            v = (v << 1) | ((byte >> (7 - (self.pos & 7))) as u32 & 1);
            self.pos += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut lead = 0;
        while self.bits(1)? == 0 {
            lead += 1;
            if lead > 31 {
                return None;
            }
        }
        if lead == 0 {
            return Some(0);
        }
        Some((1u32 << lead) - 1 + self.bits(lead)?)
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

#[test]
fn sps_parser_decodes_a_known_baseline_sps() {
    // 1280x720 constrained-baseline SPS+PPS emitted by libx264, captured verbatim.
    let au: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x1f, 0xd9, 0x00, 0x50, 0x05, 0xbb, 0x01, 0x6a,
        0x02, 0x02, 0x02, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x1e, 0x07, 0x8c, 0x18,
        0xcb, 0x00, 0x00, 0x00, 0x01, 0x68, 0xcb, 0x83, 0xcb, 0x20,
    ];
    let sps = parse_sps(au).expect("SPS parses");
    assert_eq!(sps.profile_idc, 66, "constrained baseline");
    assert_eq!(sps.chroma_format_idc, 1, "4:2:0 default for baseline");
    assert!(sps.vui_present);
}

#[test]
fn unescape_removes_emulation_prevention_bytes() {
    assert_eq!(unescape(&[0, 0, 3, 1, 2]), vec![0, 0, 1, 2]);
    assert_eq!(unescape(&[0, 0, 3, 0, 0, 3, 1]), vec![0, 0, 0, 0, 1]);
    assert_eq!(unescape(&[1, 2, 3]), vec![1, 2, 3]);
}

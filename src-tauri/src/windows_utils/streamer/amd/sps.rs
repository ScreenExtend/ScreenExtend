//! Rewrites the SPS VUI on the way out of the AMF encoder.
//!
//! AMF is reported to write none of its `OutColor*` properties into an AVC SPS (only
//! `FullRangeColor`) and no `bitstream_restriction_flag`; unverified, the `AMF SPS/PPS ready` log
//! line on a Radeon settles it. Without `max_num_reorder_frames = 0` a decoder infers the reorder
//! depth from the level and holds `min(16, MaxDpbMbs / PicSizeInMbs)` frames before showing the
//! first: 33-267 ms at 60 fps.
//!
//! The rewrite is gated on a read-back (`needs_patch`), so an SPS that is already correct is left
//! alone. The VUI written is canonical, not merged: no HRD and no timing info (RTP carries the
//! clock), so an HRD block from `EnforceHRD = true` is dropped. Browsers skip the buffering-period
//! and picture-timing SEI that then refer to it by size.
//!
//! The independent check is `windows_utils/streamer/test/h264.rs::parse_sps`: a rewriter verified
//! by its own parser proves nothing.

/// H.264 profiles whose SPS carries `chroma_format_idc` (7.3.2.1.1).
const HIGH_PROFILES: &[u8] = &[100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// `video_format` = 5, "Unspecified video format" (Table E-2). What NVENC writes.
const VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
/// BT.709 in Tables E-3, E-4 and E-5, `colour_primaries`, `transfer_characteristics` and
/// `matrix_coefficients` all take the value 1.
const BT709: u32 = 1;
/// `log2_max_mv_length_*`: 15 is the value H.264 defines as the default when
/// `bitstream_restriction_flag` is 0, so writing it changes nothing but the presence of the flag.
const LOG2_MAX_MV_LENGTH: u32 = 15;

/// What the encoder's own SPS says about its VUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VuiObserved {
    colour_description: bool,
    full_range: bool,
    bitstream_restriction: bool,
    max_num_reorder_frames: u32,
}

/// The parts of an SPS this module needs: where the VUI flag sits, and the DPB size.
#[derive(Debug, Clone, Copy)]
struct SpsPrefix {
    profile_idc: u8,
    level_idc: u8,
    /// Bit index of `vui_parameters_present_flag` in the unescaped RBSP.
    vui_flag_bit: usize,
    max_num_ref_frames: u32,
    /// `None` when the SPS has no VUI at all.
    vui: Option<VuiObserved>,
}

/// What the encoder's own parameter sets say, for the one-off decision in `read_extradata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpsInspection {
    /// No SPS NAL in the buffer at all (an empty or SPS-less `ExtraData`).
    NoSps,
    /// An SPS was there but could not be walked to its VUI.
    Unparseable,
    /// Every SPS already carries the canonical colour description and bitstream restriction.
    AlreadyCorrect,
    /// At least one SPS needed the canonical VUI; this is the rewritten buffer.
    Patched(Vec<u8>),
}

/// The fields of the first SPS that the encoder logs: what AMF really chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsSummary {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub max_num_ref_frames: u32,
}

/// One SPS NAL, judged.
enum SpsVerdict {
    Unparseable,
    AlreadyCorrect,
    Patched(Vec<u8>),
}

/// What one pass over the parameter-set prefix of an access unit found.
struct Scan {
    out: Option<Vec<u8>>,
    sps_seen: u32,
    sps_unparseable: u32,
}

/// Rewrite every SPS in an Annex-B buffer. `None` means nothing needed changing, so the caller
/// keeps the bytes it already has.
///
/// Scanning stops at the first slice NAL (types 1 and 5): parameter sets always precede the
/// slices of their access unit, so a P frame costs one NAL header read and no allocation.
pub fn patch_annexb(au: &[u8], full_range: bool) -> Option<Vec<u8>> {
    scan(au, full_range).out
}

/// [`patch_annexb`], plus the reason when nothing was rewritten.
pub fn inspect_annexb(au: &[u8], full_range: bool) -> SpsInspection {
    let s = scan(au, full_range);
    match s.out {
        Some(patched) => SpsInspection::Patched(patched),
        None if s.sps_seen == 0 => SpsInspection::NoSps,
        None if s.sps_unparseable > 0 => SpsInspection::Unparseable,
        None => SpsInspection::AlreadyCorrect,
    }
}

/// Profile, level and reference count of the first SPS in an Annex-B buffer.
pub fn summarize_annexb(au: &[u8]) -> Option<SpsSummary> {
    for (start, end) in parameter_set_nals(au) {
        if au[start] & 0x1f != 7 {
            continue;
        }
        let prefix = parse_prefix(&unescape(&au[start + 1..end]))?;
        return Some(SpsSummary {
            profile_idc: prefix.profile_idc,
            level_idc: prefix.level_idc,
            max_num_ref_frames: prefix.max_num_ref_frames,
        });
    }
    None
}

fn scan(au: &[u8], full_range: bool) -> Scan {
    let mut s = Scan {
        out: None,
        sps_seen: 0,
        sps_unparseable: 0,
    };
    let mut copied_to = 0usize;

    for (start, end) in parameter_set_nals(au) {
        if au[start] & 0x1f != 7 {
            continue;
        }
        s.sps_seen += 1;
        let patched = match judge_sps_nal_body(&au[start + 1..end], full_range) {
            SpsVerdict::Patched(p) => p,
            SpsVerdict::Unparseable => {
                s.sps_unparseable += 1;
                continue;
            }
            SpsVerdict::AlreadyCorrect => continue,
        };
        let buf = s
            .out
            .get_or_insert_with(|| Vec::with_capacity(au.len() + 32));
        buf.extend_from_slice(&au[copied_to..start + 1]);
        buf.extend_from_slice(&patched);
        copied_to = end;
    }

    if let Some(buf) = s.out.as_mut() {
        buf.extend_from_slice(&au[copied_to..]);
    }
    s
}

/// One SPS NAL body (everything after the one-byte NAL header), escaped as it appears on the
/// wire.
fn judge_sps_nal_body(escaped_body: &[u8], full_range: bool) -> SpsVerdict {
    let rbsp = unescape(escaped_body);
    let Some(prefix) = parse_prefix(&rbsp) else {
        return SpsVerdict::Unparseable;
    };
    if !needs_patch(&prefix, full_range) {
        return SpsVerdict::AlreadyCorrect;
    }

    let mut w = BitWriter::new();
    w.copy_bits(&rbsp, prefix.vui_flag_bit);
    w.bit(1); // vui_parameters_present_flag
    write_vui(&mut w, full_range, prefix.max_num_ref_frames);
    w.rbsp_trailing_bits();
    SpsVerdict::Patched(escape(&w.finish()))
}

/// False when AMF's SPS already carries the VUI this module would write.
fn needs_patch(prefix: &SpsPrefix, full_range: bool) -> bool {
    match prefix.vui {
        None => true,
        Some(v) => {
            !v.colour_description
                || v.full_range != full_range
                || !v.bitstream_restriction
                || v.max_num_reorder_frames != 0
        }
    }
}

/// The canonical VUI: colour signalling, and a bitstream restriction that says nothing is
/// reordered.
fn write_vui(w: &mut BitWriter, full_range: bool, max_num_ref_frames: u32) {
    w.bit(0); // aspect_ratio_info_present_flag
    w.bit(0); // overscan_info_present_flag

    w.bit(1); // video_signal_type_present_flag
    w.bits(VIDEO_FORMAT_UNSPECIFIED, 3);
    w.bit(u32::from(full_range));
    w.bit(1); // colour_description_present_flag
    w.bits(BT709, 8); // colour_primaries
    w.bits(BT709, 8); // transfer_characteristics
    w.bits(BT709, 8); // matrix_coefficients

    w.bit(0); // chroma_loc_info_present_flag
    w.bit(0); // timing_info_present_flag, RTP carries the clock
    w.bit(0); // nal_hrd_parameters_present_flag: see the module comment on HRD
    w.bit(0); // vcl_hrd_parameters_present_flag
    w.bit(0); // pic_struct_present_flag

    w.bit(1); // bitstream_restriction_flag, the whole reason this exists
    w.bit(1); // motion_vectors_over_pic_boundaries_flag
    w.ue(0); // max_bytes_per_pic_denom: 0 = no limit
    w.ue(0); // max_bits_per_mb_denom: 0 = no limit
    w.ue(LOG2_MAX_MV_LENGTH);
    w.ue(LOG2_MAX_MV_LENGTH);
    w.ue(0); // max_num_reorder_frames
             // max_dec_frame_buffering: at least max_num_ref_frames, which is the DPB in use
    w.ue(max_num_ref_frames.max(1));
}

/// Walk the SPS to `vui_parameters_present_flag`, recording its bit position, the reference
/// count, and whatever VUI is already there.
fn parse_prefix(rbsp: &[u8]) -> Option<SpsPrefix> {
    let mut r = BitReader::new(rbsp);

    let profile_idc = r.bits(8)? as u8;
    r.bits(8)?; // constraint flags + reserved
    let level_idc = r.bits(8)? as u8;
    r.ue()?; // seq_parameter_set_id

    if HIGH_PROFILES.contains(&profile_idc) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.bits(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.bits(1)? == 1 {
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
    if r.bits(1)? == 0 {
        // frame_mbs_only_flag
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

    let vui_flag_bit = r.pos;
    let vui = if r.bits(1)? == 1 {
        parse_vui(&mut r)
    } else {
        None
    };

    Some(SpsPrefix {
        profile_idc,
        level_idc,
        vui_flag_bit,
        max_num_ref_frames,
        vui,
    })
}

/// Read an existing VUI far enough to know whether it is already the canonical one. A VUI that
/// cannot be walked is treated as absent, which means it gets replaced.
fn parse_vui(r: &mut BitReader) -> Option<VuiObserved> {
    if r.bits(1)? == 1 {
        // aspect_ratio_info_present_flag
        if r.bits(8)? == 255 {
            r.bits(16)?;
            r.bits(16)?;
        }
    }
    if r.bits(1)? == 1 {
        r.bits(1)?; // overscan_appropriate_flag
    }
    let mut colour_description = false;
    let mut full_range = false;
    if r.bits(1)? == 1 {
        // video_signal_type_present_flag
        r.bits(3)?; // video_format
        full_range = r.bits(1)? == 1;
        if r.bits(1)? == 1 {
            colour_description = true;
            r.bits(24)?; // primaries / transfer / matrix
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
        return Some(VuiObserved {
            colour_description,
            full_range,
            bitstream_restriction: false,
            max_num_reorder_frames: 0,
        });
    }
    r.bits(1)?; // motion_vectors_over_pic_boundaries_flag
    r.ue()?; // max_bytes_per_pic_denom
    r.ue()?; // max_bits_per_mb_denom
    r.ue()?; // log2_max_mv_length_horizontal
    r.ue()?; // log2_max_mv_length_vertical
    let max_num_reorder_frames = r.ue()?;
    Some(VuiObserved {
        colour_description,
        full_range,
        bitstream_restriction: true,
        max_num_reorder_frames,
    })
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

/// The index just past the next `00 00 01` at or after `from`, or `None` when there is none. A
/// start code that ends the buffer opens no NAL and also ends the walk.
fn next_payload(au: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            return (i + 3 < au.len()).then_some(i + 3);
        }
        i += 1;
    }
    None
}

/// The NALs in front of an access unit's first slice, as `(header index, end)` spans, found
/// lazily: the walk stops at the first VCL NAL (types 1 and 5) without looking for its end, so
/// the slice data, nearly every byte of a frame, is never scanned.
fn parameter_set_nals(au: &[u8]) -> ParameterSetNals<'_> {
    ParameterSetNals {
        au,
        next: next_payload(au, 0),
    }
}

struct ParameterSetNals<'a> {
    au: &'a [u8],
    next: Option<usize>,
}

impl Iterator for ParameterSetNals<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        let start = self.next?;
        let nal_type = self.au[start] & 0x1f;
        if nal_type == 1 || nal_type == 5 {
            self.next = None;
            return None;
        }
        let following = next_payload(self.au, start);
        // the next start code may be three or four bytes: trailing zeros belong to it
        let mut end = following.map(|s| s - 3).unwrap_or(self.au.len());
        while end > start && self.au[end - 1] == 0 {
            end -= 1;
        }
        self.next = following;
        Some((start, end.max(start + 1)))
    }
}

/// One NAL in an Annex-B buffer: where its header byte is, and where the next start code begins.
#[cfg(test)]
struct NalSpan {
    payload_start: usize,
    end: usize,
}

/// Whole-buffer splitter: the reference the lazy `parameter_set_nals` walk is tested against.
#[cfg(test)]
fn split_annexb(au: &[u8]) -> Vec<NalSpan> {
    let mut starts = Vec::new();
    let mut i = 0usize;
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
    let mut out = Vec::with_capacity(starts.len());
    for (idx, &start) in starts.iter().enumerate() {
        // the next start code may be three or four bytes: trailing zeros belong to it
        let mut end = starts.get(idx + 1).map(|&s| s - 3).unwrap_or(au.len());
        while end > start && au[end - 1] == 0 {
            end -= 1;
        }
        out.push(NalSpan {
            payload_start: start,
            end: end.max(start + 1),
        });
    }
    out
}

/// Strip emulation-prevention bytes (`00 00 03` -> `00 00`).
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

/// Insert emulation-prevention bytes so no `00 00 00`, `00 00 01`, `00 00 02` or `00 00 03`
/// sequence can be mistaken for a start code.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
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

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    partial: u8,
    used: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self::default()
    }

    fn bit(&mut self, b: u32) {
        self.partial = (self.partial << 1) | (b as u8 & 1);
        self.used += 1;
        if self.used == 8 {
            self.out.push(self.partial);
            self.partial = 0;
            self.used = 0;
        }
    }

    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }

    /// Exp-Golomb, unsigned.
    fn ue(&mut self, v: u32) {
        let value = v as u64 + 1;
        let lead = 63 - value.leading_zeros();
        for _ in 0..lead {
            self.bit(0);
        }
        for i in (0..=lead).rev() {
            self.bit(((value >> i) & 1) as u32);
        }
    }

    /// Copy the first `count` bits of `src` verbatim. The prefix of the SPS is not ours to
    /// re-encode: `pic_width_in_mbs_minus1` and friends must come out exactly as they went in.
    fn copy_bits(&mut self, src: &[u8], count: usize) {
        for i in 0..count {
            let byte = src[i >> 3];
            self.bit(((byte >> (7 - (i & 7))) & 1) as u32);
        }
    }

    /// `rbsp_trailing_bits()`: a one, then zeros to the byte boundary.
    fn rbsp_trailing_bits(&mut self) {
        self.bit(1);
        while self.used != 0 {
            self.bit(0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.used != 0 {
            self.partial <<= 8 - self.used;
            self.out.push(self.partial);
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows_utils::streamer::test::h264;

    /// 1280x720 constrained-baseline SPS+PPS from libx264, captured verbatim. Its VUI claims
    /// `max_num_reorder_frames = 2` and limited range, both wrong for this pipeline (limited
    /// range is the washed-out picture).
    const BASELINE_AU: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x1f, 0xd9, 0x00, 0x50, 0x05, 0xbb, 0x01, 0x6a,
        0x02, 0x02, 0x02, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x1e, 0x07, 0x8c, 0x18,
        0xcb, 0x00, 0x00, 0x00, 0x01, 0x68, 0xcb, 0x83, 0xcb, 0x20,
    ];

    /// The same access unit with the VUI removed, which is the shape AMF is reported to produce:
    /// `vui_parameters_present_flag = 0` and nothing after it.
    fn baseline_without_vui() -> Vec<u8> {
        let (_, body) = h264::nal_slices(BASELINE_AU)
            .into_iter()
            .find(|&(t, _)| t == 7)
            .expect("the fixture has an SPS");
        let rbsp = unescape(body);
        let prefix = parse_prefix(&rbsp).expect("the fixture SPS parses");
        let mut w = BitWriter::new();
        w.copy_bits(&rbsp, prefix.vui_flag_bit);
        w.bit(0); // vui_parameters_present_flag
        w.rbsp_trailing_bits();
        let stripped = escape(&w.finish());

        let sps_start = BASELINE_AU
            .windows(5)
            .position(|w| w[..4] == [0, 0, 0, 1] && w[4] & 0x1f == 7)
            .expect("start code before the SPS");
        let pps_start = BASELINE_AU
            .windows(5)
            .position(|w| w[..4] == [0, 0, 0, 1] && w[4] & 0x1f == 8)
            .expect("start code before the PPS");
        let mut out = Vec::new();
        out.extend_from_slice(&BASELINE_AU[..sps_start + 5]);
        out.extend_from_slice(&stripped);
        out.extend_from_slice(&BASELINE_AU[pps_start..]);
        out
    }

    #[test]
    fn a_stream_claiming_a_reorder_delay_is_told_we_never_reorder() {
        let before = h264::parse_sps(BASELINE_AU).expect("fixture SPS parses");
        assert_eq!(
            before.max_num_reorder_frames, 2,
            "the fixture must start with a reorder claim, or this test proves nothing"
        );

        let patched = patch_annexb(BASELINE_AU, true).expect("a patch was needed");
        let after = h264::parse_sps(&patched).expect("patched SPS parses");

        assert!(after.vui_present);
        assert!(after.bitstream_restriction);
        assert_eq!(
            after.max_num_reorder_frames, 0,
            "a decoder told 2 holds two frames before it shows the first"
        );
        // the prefix is copied bit for bit, so everything the encoder decided is unchanged
        assert_eq!(after.profile_idc, before.profile_idc);
        assert_eq!(after.level_idc, before.level_idc);
        assert_eq!(after.chroma_format_idc, before.chroma_format_idc);
        assert_eq!(after.max_num_ref_frames, before.max_num_ref_frames);
    }

    /// The AMF case: no VUI at all. Everything has to be written, not amended.
    #[test]
    fn an_sps_with_no_vui_gets_a_whole_one() {
        let au = baseline_without_vui();
        let before = h264::parse_sps(&au).expect("the stripped SPS still parses");
        assert!(!before.vui_present, "the fixture must have no VUI");

        let patched = patch_annexb(&au, true).expect("a patch was needed");
        let after = h264::parse_sps(&patched).expect("patched SPS parses");
        assert!(after.vui_present);
        assert!(after.bitstream_restriction);
        assert_eq!(after.max_num_reorder_frames, 0);
        assert_eq!(after.max_num_ref_frames, before.max_num_ref_frames);
        assert_eq!(after.profile_idc, before.profile_idc);
        assert_eq!(after.level_idc, before.level_idc);
    }

    #[test]
    fn the_pps_and_the_slice_data_are_carried_through_untouched() {
        let patched = patch_annexb(BASELINE_AU, true).expect("a patch was needed");
        let before: Vec<(u8, usize)> = h264::nal_units(BASELINE_AU);
        let after: Vec<(u8, usize)> = h264::nal_units(&patched);
        assert_eq!(
            before.iter().map(|&(t, _)| t).collect::<Vec<_>>(),
            after.iter().map(|&(t, _)| t).collect::<Vec<_>>(),
            "the same NALs in the same order"
        );
        let pps_before = h264::nal_slices(BASELINE_AU)
            .into_iter()
            .find(|&(t, _)| t == 8)
            .map(|(_, p)| p.to_vec());
        let pps_after = h264::nal_slices(&patched)
            .into_iter()
            .find(|&(t, _)| t == 8)
            .map(|(_, p)| p.to_vec());
        assert_eq!(pps_before, pps_after, "the PPS is not ours to touch");
    }

    #[test]
    fn an_sps_that_already_says_what_we_want_is_left_alone() {
        // the second call must be a no-op: that keeps the steady-state cost a comparison, and
        // makes this code stand down once a driver writes the VUI itself
        let patched = patch_annexb(BASELINE_AU, true).expect("a patch was needed");
        let parsed = h264::parse_sps(&patched).expect("patched SPS parses");
        assert!(
            parsed.vui_present && parsed.bitstream_restriction,
            "the second call must be skipped because the SPS is right, not because it is unreadable"
        );
        assert!(
            patch_annexb(&patched, true).is_none(),
            "re-patching an already-correct SPS must not rewrite it"
        );
        // but a different colour range is a different answer
        assert!(
            patch_annexb(&patched, false).is_some(),
            "a limited-range stream needs video_full_range_flag = 0"
        );
    }

    /// The VUI written here is long enough to contain a `00 00 00` run, and an SPS handed to a
    /// decoder without emulation prevention is a stream nothing can parse.
    #[test]
    fn emulation_prevention_survives_a_round_trip() {
        let patched = patch_annexb(BASELINE_AU, true).expect("a patch was needed");
        let sps = h264::nal_slices(&patched)
            .into_iter()
            .find(|&(t, _)| t == 7)
            .map(|(_, p)| p.to_vec())
            .expect("the patched AU still has an SPS");
        for w in sps.windows(3) {
            assert!(
                w != [0, 0, 0] && w != [0, 0, 1] && w != [0, 0, 2],
                "an unescaped start-code-alike survived into the SPS: {sps:02x?}"
            );
        }
        // the escaping is reversible: the reader must see what the writer meant
        let prefix = parse_prefix(&unescape(&sps)).expect("the patched SPS parses");
        let vui = prefix.vui.expect("it has a VUI");
        assert!(vui.colour_description && vui.full_range && vui.bitstream_restriction);
        assert_eq!(vui.max_num_reorder_frames, 0);
    }

    /// An SPS carrying emulation-prevention bytes *inside* the part copied verbatim: the prefix
    /// must survive unescape and re-escape unchanged.
    #[test]
    fn a_prefix_containing_escaped_bytes_is_copied_exactly() {
        assert!(
            BASELINE_AU.windows(3).any(|w| w == [0, 0, 3]),
            "the fixture must contain an emulation-prevention byte"
        );
        let patched = patch_annexb(BASELINE_AU, true).expect("a patch was needed");
        let a = h264::parse_sps(BASELINE_AU).unwrap();
        let b = h264::parse_sps(&patched).unwrap();
        assert_eq!(
            (a.profile_idc, a.level_idc, a.max_num_ref_frames),
            (b.profile_idc, b.level_idc, b.max_num_ref_frames)
        );
    }

    #[test]
    fn a_frame_with_no_parameter_sets_is_not_scanned_past_its_first_slice() {
        // a bare P slice: no SPS, so no patch and no allocation
        let p_frame: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x41, 0x9a, 0x24, 0x6c];
        assert!(patch_annexb(p_frame, true).is_none());
    }

    /// The lazy walk must find exactly the NALs the whole-buffer splitter finds, up to the
    /// first slice, on every shape of buffer: three- and four-byte start codes, trailing zeros,
    /// a start code that ends the buffer, empty NALs and no start code at all.
    #[test]
    fn the_lazy_walk_finds_the_same_parameter_sets_as_the_full_split() {
        let alphabet = [
            0x00u8, 0x00, 0x00, 0x01, 0x03, 0x67, 0x68, 0x06, 0x09, 0x41, 0x65,
        ];
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..4000 {
            let len = (next() % 48) as usize;
            let au: Vec<u8> = (0..len)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            let mut expected = Vec::new();
            for nal in split_annexb(&au) {
                let t = au[nal.payload_start] & 0x1f;
                if t == 1 || t == 5 {
                    break;
                }
                expected.push((nal.payload_start, nal.end));
            }
            let lazy: Vec<(usize, usize)> = parameter_set_nals(&au).collect();
            assert_eq!(lazy, expected, "buffer {au:02x?}");
        }
    }

    /// The per-frame cost: a P frame's slice data is never walked. A 40 KB slice full of byte
    /// patterns that look like start codes followed by SPS headers would be visited by a
    /// whole-buffer split; the lazy walk returns at the first NAL header.
    #[test]
    fn a_p_frame_is_rejected_at_its_first_nal_header() {
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x41];
        for _ in 0..10_000 {
            au.extend_from_slice(&[0x00, 0x00, 0x01, 0x67]);
        }
        assert_eq!(parameter_set_nals(&au).count(), 0);
        assert!(patch_annexb(&au, true).is_none());
        assert_eq!(inspect_annexb(&au, true), SpsInspection::NoSps);
    }

    #[test]
    fn inspection_tells_missing_unparseable_and_correct_apart() {
        assert_eq!(inspect_annexb(&[], true), SpsInspection::NoSps);
        // a PPS alone: parameter sets, but no SPS to judge
        assert_eq!(
            inspect_annexb(&[0, 0, 0, 1, 0x68, 0xcb, 0x83, 0xcb, 0x20], true),
            SpsInspection::NoSps
        );
        // an SPS truncated after two bytes cannot be walked to the VUI
        assert_eq!(
            inspect_annexb(&[0, 0, 0, 1, 0x67, 0x42, 0xc0], true),
            SpsInspection::Unparseable
        );
        let patched = match inspect_annexb(BASELINE_AU, true) {
            SpsInspection::Patched(p) => p,
            other => panic!("the fixture needs a patch, got {other:?}"),
        };
        assert_eq!(
            inspect_annexb(&patched, true),
            SpsInspection::AlreadyCorrect
        );
    }

    #[test]
    fn the_summary_reads_the_level_the_encoder_chose() {
        let parsed = h264::parse_sps(BASELINE_AU).expect("fixture SPS parses");
        let summary = summarize_annexb(BASELINE_AU).expect("the fixture has an SPS");
        assert_eq!(summary.profile_idc, parsed.profile_idc);
        assert_eq!(summary.level_idc, parsed.level_idc);
        assert_eq!(summary.max_num_ref_frames, parsed.max_num_ref_frames);
        assert_eq!(summary.level_idc, 0x1f, "the fixture is level 3.1");
    }

    #[test]
    fn exp_golomb_round_trips() {
        for v in [0u32, 1, 2, 3, 4, 7, 8, 15, 16, 255, 65_535] {
            let mut w = BitWriter::new();
            w.ue(v);
            w.rbsp_trailing_bits();
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.ue(), Some(v), "ue({v})");
        }
    }
}

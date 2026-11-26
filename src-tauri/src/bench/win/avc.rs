//! Enough H.264 to describe an encoder's output: NAL units, SPS, PPS, and per access unit the
//! picture type, slice count and first-slice QP. The per-AU parse does not allocate.

use serde_json::{json, Value as Json};

/// `(start, end)` of each NAL unit's payload (start code removed, header byte included).
pub fn nal_ranges(au: &[u8], out: &mut Vec<(usize, usize)>) {
    out.clear();
    let n = au.len();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 3 <= n {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if let Some(s) = start {
                let mut e = i;
                while e > s && au[e - 1] == 0 {
                    e -= 1;
                }
                if e > s {
                    out.push((s, e));
                }
            }
            start = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        let mut e = n;
        while e > s && au[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push((s, e));
        }
    }
}

/// Remove emulation-prevention bytes into `out`, stopping when it is full. Returns the length.
fn unescape_into(nal: &[u8], out: &mut [u8]) -> usize {
    let mut zeros = 0;
    let mut k = 0;
    for &b in nal {
        if k == out.len() {
            break;
        }
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out[k] = b;
        k += 1;
    }
    k
}

/// `(nal type, id)` of an SPS or PPS (`seq_parameter_set_id` after the profile and level bytes,
/// `pic_parameter_set_id` first); `None` for any other unit or an unreadable or out-of-range id.
pub fn parameter_set_key(nal: &[u8]) -> Option<(u8, u32)> {
    let kind = *nal.first()? & 0x1f;
    let (skip, max_id) = match kind {
        7 => (3usize, 31u32),
        8 => (0, 255),
        _ => return None,
    };
    let mut buf = [0u8; 16];
    let n = unescape_into(&nal[1..], &mut buf);
    let mut b = Bits::new(buf.get(skip..n)?);
    let id = b.ue()?;
    (id <= max_id).then_some((kind, id))
}

fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; nal.len()];
    let n = unescape_into(nal, &mut v);
    v.truncate(n);
    v
}

/// Exp-Golomb bit reader.
pub struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(b: &'a [u8]) -> Bits<'a> {
        Bits { b, pos: 0 }
    }
    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.b.get(self.pos / 8)?;
        let v = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(v as u32)
    }
    pub fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.bits(zeros)? as u64) as u32)
    }
    pub fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
    /// `more_rbsp_data()`: anything before the final stop bit.
    fn more_data(&self) -> bool {
        let Some(last) = self.b.iter().rposition(|&x| x != 0) else {
            return false;
        };
        let stop = last * 8 + 7 - self.b[last].trailing_zeros() as usize;
        self.pos < stop
    }
}

fn skip_scaling_list(b: &mut Bits, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            let delta = b.se()?;
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

/// The fields of an SPS this bench reports, plus what a slice-header walk needs.
#[derive(Clone, Debug, Default)]
pub struct Sps {
    pub profile_idc: u32,
    pub constraint_flags: u32,
    pub level_idc: u32,
    pub sps_id: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub max_num_ref_frames: u32,
    pub width: u32,
    pub height: u32,
    pub frame_mbs_only: bool,
    pub cropped: bool,
    pub vui: Option<Vui>,
}

#[derive(Clone, Debug, Default)]
pub struct Vui {
    pub video_signal_type: bool,
    pub full_range: Option<bool>,
    pub colour_primaries: Option<u32>,
    pub transfer: Option<u32>,
    pub matrix: Option<u32>,
    pub timing_info: bool,
    pub nal_hrd: bool,
    pub vcl_hrd: bool,
    pub bitstream_restriction: bool,
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
}

fn skip_hrd(b: &mut Bits) -> Option<()> {
    let cpb_cnt = b.ue()? + 1;
    b.bits(8)?;
    for _ in 0..cpb_cnt {
        b.ue()?;
        b.ue()?;
        b.bit()?;
    }
    b.bits(20)?;
    Some(())
}

fn parse_vui(b: &mut Bits) -> Option<Vui> {
    let mut v = Vui::default();
    if b.bit()? == 1 {
        let idc = b.bits(8)?;
        if idc == 255 {
            b.bits(32)?;
        }
    }
    if b.bit()? == 1 {
        b.bit()?;
    }
    v.video_signal_type = b.bit()? == 1;
    if v.video_signal_type {
        b.bits(3)?;
        v.full_range = Some(b.bit()? == 1);
        if b.bit()? == 1 {
            v.colour_primaries = Some(b.bits(8)?);
            v.transfer = Some(b.bits(8)?);
            v.matrix = Some(b.bits(8)?);
        }
    }
    if b.bit()? == 1 {
        b.ue()?;
        b.ue()?;
    }
    v.timing_info = b.bit()? == 1;
    if v.timing_info {
        b.bits(32)?;
        b.bits(32)?;
        b.bit()?;
    }
    v.nal_hrd = b.bit()? == 1;
    if v.nal_hrd {
        skip_hrd(b)?;
    }
    v.vcl_hrd = b.bit()? == 1;
    if v.vcl_hrd {
        skip_hrd(b)?;
    }
    if v.nal_hrd || v.vcl_hrd {
        b.bit()?;
    }
    b.bit()?; // pic_struct_present_flag
    v.bitstream_restriction = b.bit()? == 1;
    if v.bitstream_restriction {
        b.bit()?;
        b.ue()?;
        b.ue()?;
        b.ue()?;
        b.ue()?;
        v.max_num_reorder_frames = Some(b.ue()?);
        v.max_dec_frame_buffering = Some(b.ue()?);
    }
    Some(v)
}

/// Parse an SPS NAL (header byte included).
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    if nal.len() < 4 || nal[0] & 0x1f != 7 {
        return None;
    }
    let r = unescape(&nal[1..]);
    let mut b = Bits::new(&r);
    let mut s = Sps {
        profile_idc: b.bits(8)?,
        constraint_flags: b.bits(8)?,
        level_idc: b.bits(8)?,
        sps_id: b.ue()?,
        chroma_format_idc: 1,
        ..Default::default()
    };
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&s.profile_idc) {
        s.chroma_format_idc = b.ue()?;
        if s.chroma_format_idc == 3 {
            s.separate_colour_plane = b.bit()? == 1;
        }
        b.ue()?;
        b.ue()?;
        b.bit()?;
        if b.bit()? == 1 {
            let n = if s.chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..n {
                if b.bit()? == 1 {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    s.log2_max_frame_num = b.ue()? + 4;
    s.poc_type = b.ue()?;
    if s.poc_type == 0 {
        s.log2_max_poc_lsb = b.ue()? + 4;
    } else if s.poc_type == 1 {
        s.delta_pic_order_always_zero = b.bit()? == 1;
        b.se()?;
        b.se()?;
        let n = b.ue()?;
        for _ in 0..n {
            b.se()?;
        }
    }
    s.max_num_ref_frames = b.ue()?;
    b.bit()?;
    let w_mbs = b.ue()? + 1;
    let h_units = b.ue()? + 1;
    s.frame_mbs_only = b.bit()? == 1;
    if !s.frame_mbs_only {
        b.bit()?;
    }
    b.bit()?;
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    s.cropped = b.bit()? == 1;
    if s.cropped {
        cl = b.ue()?;
        cr = b.ue()?;
        ct = b.ue()?;
        cb = b.ue()?;
    }
    let mul_h = if s.frame_mbs_only { 1 } else { 2 };
    let (cx, cy) = match s.chroma_format_idc {
        1 => (2, 2 * mul_h),
        2 => (2, mul_h),
        _ => (1, mul_h),
    };
    s.width = w_mbs * 16 - cx * (cl + cr);
    s.height = h_units * 16 * mul_h - cy * (ct + cb);
    if b.bit()? == 1 {
        s.vui = parse_vui(&mut b);
    }
    Some(s)
}

#[derive(Clone, Debug, Default)]
pub struct Pps {
    pub pps_id: u32,
    pub sps_id: u32,
    pub cabac: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default: u32,
    pub weighted_pred: bool,
    pub weighted_bipred: u32,
    pub pic_init_qp: i32,
    pub deblocking_filter_control_present: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8: bool,
}

pub fn parse_pps(nal: &[u8]) -> Option<Pps> {
    if nal.len() < 2 || nal[0] & 0x1f != 8 {
        return None;
    }
    let r = unescape(&nal[1..]);
    let mut b = Bits::new(&r);
    let mut p = Pps {
        pps_id: b.ue()?,
        sps_id: b.ue()?,
        cabac: b.bit()? == 1,
        bottom_field_pic_order_in_frame_present: b.bit()? == 1,
        ..Default::default()
    };
    if b.ue()? != 0 {
        return Some(p); // slice groups: nothing below is read
    }
    p.num_ref_idx_l0_default = b.ue()? + 1;
    b.ue()?;
    p.weighted_pred = b.bit()? == 1;
    p.weighted_bipred = b.bits(2)?;
    p.pic_init_qp = 26 + b.se()?;
    b.se()?;
    b.se()?;
    p.deblocking_filter_control_present = b.bit()? == 1;
    b.bit()?;
    p.redundant_pic_cnt_present = b.bit()? == 1;
    if b.more_data() {
        p.transform_8x8 = b.bit()? == 1;
    }
    Some(p)
}

/// Picture type of an access unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PicType {
    #[default]
    None,
    Idr,
    I,
    P,
    B,
}

impl PicType {
    pub fn name(self) -> &'static str {
        match self {
            PicType::None => "-",
            PicType::Idr => "IDR",
            PicType::I => "I",
            PicType::P => "P",
            PicType::B => "B",
        }
    }
}

/// What one access unit is.
#[derive(Clone, Copy, Debug, Default)]
pub struct Au {
    pub pic: PicType,
    pub slices: u16,
    pub has_sps: bool,
    pub has_pps: bool,
    /// QP of the first slice (`26 + pic_init_qp_minus26 + slice_qp_delta`), when the header could
    /// be walked with the SPS and PPS seen so far.
    pub qp: Option<i8>,
}

/// The parameter sets the slice-header walk needs, kept across AUs.
#[derive(Default)]
pub struct State {
    pub sps: Option<Sps>,
    pub pps: Option<Pps>,
    /// The first SPS and PPS NAL units, as emitted.
    pub first_params: Vec<Vec<u8>>,
    ranges: Vec<(usize, usize)>,
}

impl State {
    pub fn new() -> State {
        State {
            ranges: Vec::with_capacity(256),
            ..Default::default()
        }
    }

    /// Describe one AU. Allocates only the first time it meets an SPS or PPS (to keep it).
    pub fn parse(&mut self, au: &[u8]) -> Au {
        let mut out = Au::default();
        let mut ranges = std::mem::take(&mut self.ranges);
        nal_ranges(au, &mut ranges);
        let mut first_slice: Option<(usize, usize)> = None;
        for &(s, e) in &ranges {
            let nal = &au[s..e];
            match nal[0] & 0x1f {
                7 => {
                    out.has_sps = true;
                    if self.sps.is_none() {
                        self.sps = parse_sps(nal);
                        self.first_params.push(nal.to_vec());
                    }
                }
                8 => {
                    out.has_pps = true;
                    if self.pps.is_none() {
                        self.pps = parse_pps(nal);
                        self.first_params.push(nal.to_vec());
                    }
                }
                1 | 5 => {
                    out.slices = out.slices.saturating_add(1);
                    if first_slice.is_none() {
                        first_slice = Some((s, e));
                    }
                }
                _ => {}
            }
        }
        self.ranges = ranges;
        if let Some((s, e)) = first_slice {
            let nal = &au[s..e];
            let (pic, qp) = self.slice_header(nal);
            out.pic = pic;
            out.qp = qp;
        }
        out
    }

    fn slice_header(&self, nal: &[u8]) -> (PicType, Option<i8>) {
        let idr = nal[0] & 0x1f == 5;
        let nal_ref_idc = (nal[0] >> 5) & 3;
        let mut buf = [0u8; 96];
        let n = unescape_into(&nal[1..], &mut buf);
        let mut b = Bits::new(&buf[..n]);
        let Some(_first_mb) = b.ue() else {
            return (PicType::None, None);
        };
        let Some(st) = b.ue() else {
            return (PicType::None, None);
        };
        let st = st % 5;
        let pic = if idr {
            PicType::Idr
        } else {
            match st {
                0 | 3 => PicType::P,
                1 => PicType::B,
                _ => PicType::I,
            }
        };
        let qp = (|| -> Option<i8> {
            let sps = self.sps.as_ref()?;
            let pps = self.pps.as_ref()?;
            b.ue()?; // pps id
            if sps.separate_colour_plane {
                b.bits(2)?;
            }
            b.bits(sps.log2_max_frame_num)?;
            let mut field = false;
            if !sps.frame_mbs_only {
                field = b.bit()? == 1;
                if field {
                    b.bit()?;
                }
            }
            if idr {
                b.ue()?;
            }
            if sps.poc_type == 0 {
                b.bits(sps.log2_max_poc_lsb)?;
                if pps.bottom_field_pic_order_in_frame_present && !field {
                    b.se()?;
                }
            }
            if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
                b.se()?;
                if pps.bottom_field_pic_order_in_frame_present && !field {
                    b.se()?;
                }
            }
            if pps.redundant_pic_cnt_present {
                b.ue()?;
            }
            if pic == PicType::B {
                b.bit()?;
            }
            let p_or_b = matches!(st, 0 | 1 | 3);
            if p_or_b && b.bit()? == 1 {
                b.ue()?;
                if pic == PicType::B {
                    b.ue()?;
                }
            }
            // ref_pic_list_modification
            if st != 2 && st != 4 {
                for _ in 0..if pic == PicType::B { 2 } else { 1 } {
                    if b.bit()? == 1 {
                        loop {
                            let op = b.ue()?;
                            if op == 3 {
                                break;
                            }
                            b.ue()?;
                        }
                    }
                }
            }
            if (pps.weighted_pred && matches!(st, 0 | 3))
                || (pps.weighted_bipred == 1 && pic == PicType::B)
            {
                return None; // pred_weight_table: not walked
            }
            if nal_ref_idc != 0 {
                if idr {
                    b.bit()?;
                    b.bit()?;
                } else if b.bit()? == 1 {
                    loop {
                        let op = b.ue()?;
                        if op == 0 {
                            break;
                        }
                        if op == 1 || op == 3 {
                            b.ue()?;
                        }
                        if op == 2 {
                            b.ue()?;
                        }
                        if op == 3 || op == 6 {
                            b.ue()?;
                        }
                        if op == 4 {
                            b.ue()?;
                        }
                    }
                }
            }
            if pps.cabac && st != 2 && st != 4 {
                b.ue()?;
            }
            let qp = pps.pic_init_qp + b.se()?;
            (0..=51).contains(&qp).then_some(qp as i8)
        })();
        (pic, qp)
    }
}

/// The SPS / PPS summary a result carries, from the first parameter sets seen.
pub fn params_json(state: &State) -> Json {
    let Some(sps) = &state.sps else {
        return json!({ "error": "no SPS" });
    };
    let vui = sps.vui.as_ref().map(|v| {
        json!({
            "video_signal_type": v.video_signal_type,
            "full_range": v.full_range,
            "colour_primaries": v.colour_primaries,
            "transfer_characteristics": v.transfer,
            "matrix_coefficients": v.matrix,
            "timing_info": v.timing_info,
            "nal_hrd": v.nal_hrd,
            "vcl_hrd": v.vcl_hrd,
            "bitstream_restriction": v.bitstream_restriction,
            "max_num_reorder_frames": v.max_num_reorder_frames,
            "max_dec_frame_buffering": v.max_dec_frame_buffering,
        })
    });
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    json!({
        "profile_idc": sps.profile_idc,
        "profile": match sps.profile_idc {
            66 => if sps.constraint_flags & 0x40 != 0 { "constrained-baseline" } else { "baseline" },
            77 => "main",
            100 => if sps.constraint_flags & 0x0c == 0x0c { "constrained-high" } else { "high" },
            _ => "other",
        },
        "constraint_flags": format!("0x{:02x}", sps.constraint_flags),
        "level_idc": sps.level_idc,
        "max_num_ref_frames": sps.max_num_ref_frames,
        "width": sps.width,
        "height": sps.height,
        "poc_type": sps.poc_type,
        "vui_present": sps.vui.is_some(),
        "vui": vui,
        "pps": state.pps.as_ref().map(|p| json!({
            "entropy": if p.cabac { "cabac" } else { "cavlc" },
            "transform_8x8": p.transform_8x8,
            "pic_init_qp": p.pic_init_qp,
            "num_ref_idx_l0_default": p.num_ref_idx_l0_default,
            "weighted_pred": p.weighted_pred,
        })),
        "params_hex": state.first_params.iter().map(|p| hex(p)).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bits written MSB first, for building headers in tests.
    struct W {
        out: Vec<u8>,
        n: usize,
    }
    impl W {
        fn new() -> W {
            W {
                out: Vec::new(),
                n: 0,
            }
        }
        fn bit(&mut self, b: u32) {
            if self.n.is_multiple_of(8) {
                self.out.push(0);
            }
            if b != 0 {
                let last = self.out.len() - 1;
                self.out[last] |= 0x80 >> (self.n % 8);
            }
            self.n += 1;
        }
        fn bits(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                self.bit((v >> i) & 1);
            }
        }
        fn ue(&mut self, v: u32) {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            for _ in 0..len - 1 {
                self.bit(0);
            }
            self.bits(x, len);
        }
        fn se(&mut self, v: i32) {
            self.ue(if v > 0 {
                (2 * v - 1) as u32
            } else {
                (-2 * v) as u32
            });
        }
        fn trailing(mut self) -> Vec<u8> {
            self.bit(1);
            while !self.n.is_multiple_of(8) {
                self.bit(0);
            }
            self.out
        }
    }

    fn sps_1080p_high() -> Vec<u8> {
        let mut w = W::new();
        w.bits(100, 8); // profile high
        w.bits(0, 8);
        w.bits(42, 8); // level 4.2
        w.ue(0); // sps id
        w.ue(1); // chroma 4:2:0
        w.ue(0);
        w.ue(0);
        w.bit(0);
        w.bit(0); // no scaling matrix
        w.ue(0); // log2_max_frame_num - 4
        w.ue(2); // poc type 2
        w.ue(1); // max_num_ref_frames
        w.bit(0);
        w.ue(119); // 1920/16 - 1
        w.ue(67); // 68 MB rows - 1 (1088)
        w.bit(1); // frame_mbs_only
        w.bit(1); // direct_8x8
        w.bit(1); // cropping
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(4); // bottom: 8 rows
        w.bit(1); // vui present
        w.bit(0); // aspect
        w.bit(0); // overscan
        w.bit(1); // video signal type
        w.bits(5, 3);
        w.bit(1); // full range
        w.bit(1);
        w.bits(1, 8);
        w.bits(1, 8);
        w.bits(1, 8);
        w.bit(0); // chroma loc
        w.bit(0); // timing
        w.bit(0); // nal hrd
        w.bit(0); // vcl hrd
        w.bit(0); // pic struct
        w.bit(1); // bitstream restriction
        w.bit(1);
        w.ue(2);
        w.ue(1);
        w.ue(16);
        w.ue(16);
        w.ue(0); // reorder
        w.ue(1); // dec buffering
        let mut nal = vec![0x67];
        nal.extend(w.trailing());
        nal
    }

    fn pps_cabac() -> Vec<u8> {
        let mut w = W::new();
        w.ue(0);
        w.ue(0);
        w.bit(1); // cabac
        w.bit(0);
        w.ue(0); // slice groups
        w.ue(0);
        w.ue(0);
        w.bit(0);
        w.bits(0, 2);
        w.se(-4); // pic_init_qp = 22
        w.se(0);
        w.se(0);
        w.bit(1);
        w.bit(0);
        w.bit(0);
        w.bit(1); // transform_8x8
        w.bit(0);
        w.se(0);
        let mut nal = vec![0x68];
        nal.extend(w.trailing());
        nal
    }

    fn slice(idr: bool, slice_type: u32, qp_delta: i32) -> Vec<u8> {
        let mut w = W::new();
        w.ue(0); // first mb
        w.ue(slice_type);
        w.ue(0); // pps
        w.bits(0, 4); // frame_num
        if idr {
            w.ue(0);
        }
        if slice_type == 0 || slice_type == 5 {
            w.bit(0); // num_ref_idx override
            w.bit(0); // ref list mod
        }
        if idr {
            w.bit(0);
            w.bit(0);
        } else {
            w.bit(0); // adaptive marking
        }
        if slice_type % 5 != 2 {
            w.ue(0); // cabac_init_idc
        }
        w.se(qp_delta);
        let mut nal = vec![if idr { 0x65 } else { 0x41 }];
        nal.extend(w.trailing());
        nal
    }

    fn annexb(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }

    #[test]
    fn sps_pps_and_slices_are_read() {
        let sps = parse_sps(&sps_1080p_high()).unwrap();
        assert_eq!((sps.profile_idc, sps.level_idc), (100, 42));
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert_eq!(sps.max_num_ref_frames, 1);
        let vui = sps.vui.unwrap();
        assert_eq!(vui.full_range, Some(true));
        assert_eq!(vui.matrix, Some(1));
        assert!(vui.bitstream_restriction);
        assert_eq!(vui.max_num_reorder_frames, Some(0));
        assert_eq!(vui.max_dec_frame_buffering, Some(1));
        let pps = parse_pps(&pps_cabac()).unwrap();
        assert!(pps.cabac && pps.transform_8x8);
        assert_eq!(pps.pic_init_qp, 22);

        let mut st = State::new();
        let idr = annexb(&[
            sps_1080p_high(),
            pps_cabac(),
            slice(true, 7, 3),
            slice(true, 7, 3),
        ]);
        let a = st.parse(&idr);
        assert_eq!(a.pic, PicType::Idr);
        assert_eq!(a.slices, 2);
        assert!(a.has_sps && a.has_pps);
        assert_eq!(a.qp, Some(25));
        let p = annexb(&[slice(false, 5, -2)]);
        let a = st.parse(&p);
        assert_eq!(a.pic, PicType::P);
        assert_eq!(a.slices, 1);
        assert_eq!(a.qp, Some(20));
        let j = params_json(&st);
        assert_eq!(j["vui_present"], true);
        assert_eq!(j["pps"]["entropy"], "cabac");
        assert_eq!(j["profile"], "high");
    }

    #[test]
    fn ranges_split_on_three_and_four_byte_start_codes() {
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9,
        ];
        let mut r = Vec::new();
        nal_ranges(&au, &mut r);
        let nals: Vec<&[u8]> = r.iter().map(|&(s, e)| &au[s..e]).collect();
        assert_eq!(nals, [&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
    }

    #[test]
    fn emulation_prevention_is_removed() {
        let mut out = [0u8; 8];
        let n = unescape_into(&[0, 0, 3, 1, 0, 0, 3, 0], &mut out);
        assert_eq!(&out[..n], &[0, 0, 1, 0, 0, 0]);
    }
}

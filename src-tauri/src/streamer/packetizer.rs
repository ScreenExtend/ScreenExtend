//! RFC 6184 packetization for the per-viewer RTP writer, in place of
//! `rtp::codecs::h264::H264Payloader`.
//!
//! The stock payloader re-emits SPS and PPS as a STAP-A only if the aggregate fits the MTU. If it
//! does not, nothing is pushed, no error is returned and the stashed copies are cleared: with an
//! infinite GOP, a black client with RTP flowing and every host-side signal reporting success.
//! Here sets that do not fit together go as their own packets (RFC 6184 never required
//! aggregation), FU-A fragmented if one does not fit alone.
//!
//! A NAL that must be fragmented gets one allocation, carved into per-fragment `Bytes` with
//! `split_to`; one that fits the MTU is a `Bytes::slice` of the frame, with no copy. The stock
//! payloader allocates per fragment: ~260 times for a 300 KB IDR at a 1188-byte MTU, per viewer.

use bytes::{BufMut, Bytes, BytesMut};

const NAL_TYPE_MASK: u8 = 0x1f;

const NAL_AUD: u8 = 9;
const NAL_FILLER: u8 = 12;
pub(crate) const NAL_SPS: u8 = 7;
pub(crate) const NAL_PPS: u8 = 8;
const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;

/// STAP-A, RFC 6184 §5.7.1.
pub(crate) const NAL_STAP_A: u8 = 24;
/// FU-A, RFC 6184 §5.8.
const NAL_FU_A: u8 = 28;

/// The STAP-A NAL header.
const STAP_A_HEADER: usize = 1;
/// The size prefix in front of each NAL a STAP-A aggregates.
const STAP_A_SIZE_PREFIX: usize = 2;
/// FU indicator plus FU header.
const FU_A_HEADER: usize = 2;

/// The most NAL units one STAP-A may carry. libwebrtc through M126 records the ids of at most ten
/// per packet (`kMaxNalusPerPacket`) and ignores the rest, so a PPS past the tenth was never
/// registered and the IDR naming it was dropped. M128 made it unbounded; older receivers remain.
const MAX_STAP_A_NALUS: usize = 10;

/// Packetizes H.264 access units in Annex-B form into RTP payloads.
///
/// One per viewer, because the parameter-set stash is a per-stream state machine. Between calls
/// it keeps a private copy of each parameter set the stream currently uses, and nothing else.
#[derive(Default)]
pub struct H264Packetizer {
    /// Parameter sets of the access unit in hand, in stream order, waiting for the next NAL that
    /// is not one. All of them: Quick Sync can emit five PPS (ids 0 to 4) and pick one per
    /// picture, and a receiver drops an IDR whose PPS id it has never seen.
    params: Vec<Bytes>,
    /// `(type, id)` of every parameter set the access unit in hand has sent so far.
    au_keys: Vec<(u8, u32)>,
    /// The current copy of every parameter set in use, keyed by NAL type and id. An IDR that
    /// carries no SPS gets these in front of its first slice, so a viewer starting there can
    /// decode it. An IDR that carries an SPS restates the stream and replaces the whole set.
    /// Copies, so a stored set does not keep the access unit it came in alive.
    stored: Vec<(u8, u32, Bytes)>,
    /// Parameter sets were dropped because the caller's MTU could not carry them even fragmented.
    /// Only reachable at an absurd MTU; counted rather than assumed impossible.
    dropped_parameter_sets: u64,
    /// Parameter sets larger than the MTU, sent as FU-A. libwebrtc learns no id from a fragment,
    /// so a keyframe that depends on one is refused; sending it is still better than dropping it,
    /// which would also starve a receiver that does reassemble. Logged once per packetizer.
    fragmented_parameter_sets: u64,
}

impl H264Packetizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dropped_parameter_sets(&self) -> u64 {
        self.dropped_parameter_sets
    }

    pub fn fragmented_parameter_sets(&self) -> u64 {
        self.fragmented_parameter_sets
    }

    /// Packetize one access unit into `out`.
    ///
    /// `frame` must be the whole access unit in Annex-B form. `out` is cleared first and is the
    /// caller's to reuse across frames, which keeps the payload vector off the allocator.
    pub fn packetize(&mut self, mtu: usize, frame: &Bytes, out: &mut Vec<Bytes>) {
        out.clear();
        if mtu <= FU_A_HEADER || frame.is_empty() {
            return;
        }
        // at least this many packets: one growth instead of eight on a large IDR
        out.reserve(frame.len().div_ceil(mtu).max(1));

        self.au_keys.clear();
        let mut carries_sps = false;
        let mut seen_slice = false;
        for (start, end) in AnnexBNals::new(frame) {
            let nal = frame.slice(start..end);
            let Some(&first) = nal.first() else { continue };
            match first & NAL_TYPE_MASK {
                NAL_AUD | NAL_FILLER => continue,
                NAL_SPS | NAL_PPS => {
                    carries_sps |= first & NAL_TYPE_MASK == NAL_SPS;
                    self.params.push(nal);
                    continue;
                }
                // the first slice decides what the access unit needs, whatever precedes it
                // (typically an SEI)
                kind @ NAL_SLICE..=NAL_IDR if !seen_slice => {
                    seen_slice = true;
                    let idr = kind == NAL_IDR;
                    self.flush_parameter_sets(mtu, idr && !carries_sps, out);
                    if idr && carries_sps {
                        let carried = std::mem::take(&mut self.au_keys);
                        self.stored
                            .retain(|(kind, id, _)| carried.contains(&(*kind, *id)));
                        self.au_keys = carried;
                    }
                }
                _ if !self.params.is_empty() => self.flush_parameter_sets(mtu, false, out),
                _ => {}
            }
            self.emit_nal(mtu, nal, out);
        }
        // sets after the last slice, or a parameter-sets-only access unit, which some encoders
        // emit at session start
        self.flush_parameter_sets(mtu, false, out);
    }

    /// Sends the pending parameter sets, SPS before PPS and otherwise in stream order, keeping
    /// only the last of any repeated id. With `inject`, stored sets the access unit has not sent
    /// go with them: Safari's decoder takes the NAL after the first SPS as the PPS, and ffmpeg
    /// drops a PPS whose SPS it has not seen, so the order is not cosmetic.
    fn flush_parameter_sets(&mut self, mtu: usize, inject: bool, out: &mut Vec<Bytes>) {
        let params = std::mem::take(&mut self.params);
        let mut ordered = true;
        let mut seen_pps = false;
        for (i, nal) in params.iter().enumerate() {
            let key = parameter_set_key(nal);
            if let Some(key) = key {
                if !self.au_keys.contains(&key) {
                    self.au_keys.push(key);
                }
            }
            let is_sps = nal[0] & NAL_TYPE_MASK == NAL_SPS;
            if (is_sps && seen_pps) || superseded(&params, i, key) {
                ordered = false;
            }
            seen_pps |= !is_sps;
            self.remember(nal, key);
        }
        let missing = |stored: &[(u8, u32, Bytes)], au: &[(u8, u32)], kind: u8| {
            stored
                .iter()
                .filter(move |(k, id, _)| *k == kind && !au.contains(&(*k, *id)))
                .map(|(_, _, nal)| nal.clone())
                .collect::<Vec<_>>()
        };
        let (missing_sps, missing_pps) = if inject {
            (
                missing(&self.stored, &self.au_keys, NAL_SPS),
                missing(&self.stored, &self.au_keys, NAL_PPS),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        if ordered && missing_sps.is_empty() && missing_pps.is_empty() {
            self.emit_parameter_sets(mtu, &params, out);
        } else {
            let mut all = missing_sps;
            for kind in [NAL_SPS, NAL_PPS] {
                for (i, nal) in params.iter().enumerate() {
                    if nal[0] & NAL_TYPE_MASK == kind
                        && !superseded(&params, i, parameter_set_key(nal))
                    {
                        all.push(nal.clone());
                    }
                }
            }
            all.extend(missing_pps);
            self.emit_parameter_sets(mtu, &all, out);
        }
        // hand the vector back so its capacity is reused next frame
        self.params = params;
        self.params.clear();
    }

    /// Keeps `nal` as the current copy of its parameter set. The copy is taken only when the
    /// content changed, so a stream that repeats its sets every frame allocates nothing. One
    /// whose id cannot be read is sent but not kept.
    fn remember(&mut self, nal: &[u8], key: Option<(u8, u32)>) {
        let Some((kind, id)) = key else {
            return;
        };
        match self
            .stored
            .iter_mut()
            .find(|(k, i, _)| *k == kind && *i == id)
        {
            Some(entry) if entry.2[..] == *nal => {}
            Some(entry) => entry.2 = Bytes::copy_from_slice(nal),
            None => self.stored.push((kind, id, Bytes::copy_from_slice(nal))),
        }
    }

    /// Sends parameter sets in order, aggregating neighbours into STAP-As up to the MTU and
    /// `MAX_STAP_A_NALUS`. One that fits with nothing else goes alone, fragmented if it must be:
    /// RFC 6184 never required aggregation.
    fn emit_parameter_sets(&mut self, mtu: usize, params: &[Bytes], out: &mut Vec<Bytes>) {
        let mut i = 0;
        while i < params.len() {
            let mut end = i;
            let mut len = STAP_A_HEADER;
            while end < params.len()
                && end - i < MAX_STAP_A_NALUS
                && len + STAP_A_SIZE_PREFIX + params[end].len() <= mtu
            {
                len += STAP_A_SIZE_PREFIX + params[end].len();
                end += 1;
            }
            if end - i < 2 {
                if params[i].len() > mtu {
                    if self.fragmented_parameter_sets == 0 {
                        teprintln!(
                            "H.264: a {}-byte parameter set does not fit the {mtu}-byte MTU and goes \
                             out fragmented; a WebRTC receiver cannot register it",
                            params[i].len()
                        );
                    }
                    self.fragmented_parameter_sets += 1;
                }
                self.emit_nal(mtu, params[i].clone(), out);
                i += 1;
                continue;
            }
            let group = &params[i..end];
            let mut buf = BytesMut::with_capacity(len);
            // the STAP-A header takes its NRI from the highest of the aggregated NALs
            let nri = group.iter().map(|n| n[0] & 0x60).max().unwrap_or(0);
            buf.put_u8(nri | NAL_STAP_A);
            for nal in group {
                buf.put_u16(nal.len() as u16);
                buf.put_slice(nal);
            }
            out.push(buf.freeze());
            i = end;
        }
    }

    fn emit_nal(&mut self, mtu: usize, nal: Bytes, out: &mut Vec<Bytes>) {
        if nal.len() <= mtu {
            out.push(nal);
            return;
        }

        let header = nal[0];
        let nal_type = header & NAL_TYPE_MASK;
        let nri = header & 0x60;
        let body = &nal[1..];
        let per_fragment = mtu - FU_A_HEADER;
        if per_fragment == 0 {
            if matches!(nal_type, NAL_SPS | NAL_PPS) {
                self.dropped_parameter_sets += 1;
            }
            return;
        }

        let fragments = body.len().div_ceil(per_fragment);
        // one allocation for the whole NAL: `split_to` hands out views into it, so a fragment
        // costs a refcount bump
        let mut buf = BytesMut::with_capacity(body.len() + fragments * FU_A_HEADER);
        let mut offset = 0usize;
        let mut index = 0usize;
        while offset < body.len() {
            let take = per_fragment.min(body.len() - offset);
            let start = index == 0;
            let end = offset + take == body.len();
            buf.put_u8(nri | NAL_FU_A);
            buf.put_u8((u8::from(start) << 7) | (u8::from(end) << 6) | nal_type);
            buf.put_slice(&body[offset..offset + take]);
            offset += take;
            index += 1;
        }
        for i in 0..fragments {
            let take = per_fragment.min(body.len() - i * per_fragment);
            let chunk = buf.split_to(FU_A_HEADER + take);
            out.push(chunk.freeze());
        }
    }
}

/// `(nal type, id)` of an SPS or PPS: `seq_parameter_set_id` after the three profile and level
/// bytes, or `pic_parameter_set_id` first. `None` for any other NAL, or an id out of range.
fn parameter_set_key(nal: &[u8]) -> Option<(u8, u32)> {
    let kind = *nal.first()? & NAL_TYPE_MASK;
    let (skip_bits, max_id) = match kind {
        NAL_SPS => (24, 31),
        NAL_PPS => (0, 255),
        _ => return None,
    };
    // enough RBSP for the id, with emulation-prevention bytes removed
    let mut rbsp = [0u8; 8];
    let mut len = 0;
    let mut zeros = 0;
    for &b in &nal[1..] {
        if len == rbsp.len() {
            break;
        }
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp[len] = b;
        len += 1;
    }
    let bit = |pos: usize| -> Option<u32> {
        let byte = *rbsp[..len].get(pos / 8)?;
        Some(u32::from((byte >> (7 - pos % 8)) & 1))
    };
    let mut pos = skip_bits;
    let mut leading = 0;
    while bit(pos)? == 0 {
        pos += 1;
        leading += 1;
        if leading > 8 {
            return None;
        }
    }
    pos += 1;
    let mut rest = 0u32;
    for _ in 0..leading {
        rest = (rest << 1) | bit(pos)?;
        pos += 1;
    }
    let id = (1u32 << leading) - 1 + rest;
    (id <= max_id).then_some((kind, id))
}

/// Whether a later parameter set in `params` has the same type and id as the one at `i`, which
/// makes this one stale. A set whose id cannot be read is never superseded.
fn superseded(params: &[Bytes], i: usize, key: Option<(u8, u32)>) -> bool {
    key.is_some() && params[i + 1..].iter().any(|n| parameter_set_key(n) == key)
}

/// Walks Annex-B start codes, yielding `(start, end)` byte ranges of each NAL unit's payload.
struct AnnexBNals<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> AnnexBNals<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// The next start code at or after `from`, as `(offset of its first zero, its length)`.
    ///
    /// One pass, one comparison per byte, counting consecutive zeros. A "does a start code begin
    /// here?" predicate at every offset measured 1.65x slower than the stock payloader on a 16 KB
    /// P-frame: it reads up to four bytes per offset, with a length test each.
    fn next_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
        if from >= buf.len() {
            return None;
        }
        let mut zeros = 0usize;
        for (offset, &b) in buf[from..].iter().enumerate() {
            if b == 0 {
                zeros += 1;
            } else if b == 1 && zeros >= 2 {
                let len = if zeros >= 3 { 4 } else { 3 };
                return Some((from + offset + 1 - len, len));
            } else {
                zeros = 0;
            }
        }
        None
    }
}

impl Iterator for AnnexBNals<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        let buf = self.buf;
        loop {
            let (sc_at, sc_len) = Self::next_start_code(buf, self.pos)?;
            let start = sc_at + sc_len;
            let end = match Self::next_start_code(buf, start) {
                Some((next_at, _)) => next_at,
                None => buf.len(),
            };
            self.pos = end;
            if end > start {
                return Some((start, end));
            }
            // two start codes back to back: skip the empty NAL rather than spin on it
            if end >= buf.len() {
                return None;
            }
        }
    }
}

/// Whether a payload list actually carries the parameter sets, as a STAP-A aggregate, a bare SPS
/// or the first fragment of a fragmented one.
pub fn payloads_carry_parameter_sets(payloads: &[Bytes]) -> bool {
    payloads.iter().any(|p| match p.first().map(|b| b & 0x1f) {
        Some(NAL_STAP_A) | Some(NAL_SPS) => true,
        Some(NAL_FU_A) => p
            .get(1)
            .is_some_and(|h| h & 0x80 != 0 && h & NAL_TYPE_MASK == NAL_SPS),
        _ => false,
    })
}

/// Whether an access unit carries a sequence parameter set.
///
/// Parameter sets lead a keyframe access unit, at most behind an AUD or an SEI, so a short prefix
/// scan answers the question and costs nothing on the P-frames that dominate the stream.
pub fn access_unit_has_sps(data: &[u8]) -> bool {
    let end = data.len().min(256);
    let mut i = 0usize;
    while i + 3 < end {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if data[i + 3] & NAL_TYPE_MASK == NAL_SPS {
                return true;
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(kind: u8, len: usize) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x60 | kind];
        v.resize(v.len() + len, 0xAA);
        v
    }

    fn frame(parts: &[Vec<u8>]) -> Bytes {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p);
        }
        Bytes::from(v)
    }

    /// Reassemble the original NAL payloads from a payload list, the way a depacketizer would.
    fn reassemble(payloads: &[Bytes]) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut pending: Option<Vec<u8>> = None;
        for p in payloads {
            match p[0] & NAL_TYPE_MASK {
                NAL_STAP_A => {
                    let mut i = 1usize;
                    while i + 2 <= p.len() {
                        let n = u16::from_be_bytes([p[i], p[i + 1]]) as usize;
                        i += 2;
                        out.push(p[i..i + n].to_vec());
                        i += n;
                    }
                }
                NAL_FU_A => {
                    let fu = p[1];
                    if fu & 0x80 != 0 {
                        pending = Some(vec![(p[0] & 0xe0) | (fu & NAL_TYPE_MASK)]);
                    }
                    if let Some(acc) = pending.as_mut() {
                        acc.extend_from_slice(&p[2..]);
                        if fu & 0x40 != 0 {
                            out.push(pending.take().unwrap());
                        }
                    }
                }
                _ => out.push(p.to_vec()),
            }
        }
        out
    }

    #[test]
    fn a_small_keyframe_aggregates_its_parameter_sets() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(
            1188,
            &frame(&[nal(NAL_SPS, 20), nal(NAL_PPS, 4), nal(5, 100)]),
            &mut out,
        );
        assert_eq!(out.len(), 2, "STAP-A then the IDR");
        assert_eq!(out[0][0] & NAL_TYPE_MASK, NAL_STAP_A);
        assert!(payloads_carry_parameter_sets(&out));
        let nals = reassemble(&out);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0][0] & NAL_TYPE_MASK, NAL_SPS);
        assert_eq!(nals[1][0] & NAL_TYPE_MASK, NAL_PPS);
        assert_eq!(nals[2][0] & NAL_TYPE_MASK, 5);
    }

    #[test]
    fn parameter_sets_that_do_not_fit_together_are_sent_separately() {
        // the case the stock payloader drops in silence
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        let f = frame(&[nal(NAL_SPS, 40), nal(NAL_PPS, 40), nal(5, 30)]);
        p.packetize(60, &f, &mut out);
        assert!(
            payloads_carry_parameter_sets(&out),
            "SPS must survive an MTU too small for the aggregate"
        );
        let nals = reassemble(&out);
        assert_eq!(nals[0][0] & NAL_TYPE_MASK, NAL_SPS);
        assert_eq!(nals[0].len(), 41);
        assert_eq!(nals[1][0] & NAL_TYPE_MASK, NAL_PPS);
        assert_eq!(nals[1].len(), 41);
        assert_eq!(p.dropped_parameter_sets(), 0);
    }

    #[test]
    fn an_sps_larger_than_the_mtu_is_fragmented_rather_than_dropped() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        let f = frame(&[nal(NAL_SPS, 300), nal(NAL_PPS, 10), nal(5, 10)]);
        p.packetize(64, &f, &mut out);
        assert!(payloads_carry_parameter_sets(&out));
        let nals = reassemble(&out);
        assert_eq!(nals[0][0] & NAL_TYPE_MASK, NAL_SPS);
        assert_eq!(nals[0].len(), 301);
    }

    #[test]
    fn a_large_nal_round_trips_through_fu_a() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        let f = frame(&[nal(1, 5000)]);
        p.packetize(1188, &f, &mut out);
        assert!(out.len() > 4);
        assert!(out.iter().all(|x| x.len() <= 1188));
        let nals = reassemble(&out);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].len(), 5001);
        assert_eq!(nals[0][0] & NAL_TYPE_MASK, 1);
        assert!(nals[0][1..].iter().all(|b| *b == 0xAA));
    }

    #[test]
    fn every_fu_a_fragment_of_one_nal_shares_a_single_allocation() {
        // fragments carved from one `BytesMut` are adjacent in memory
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&[nal(1, 20000)]), &mut out);
        assert!(out.len() >= 4);
        let a = out[0].as_ptr() as usize;
        let b = out[1].as_ptr() as usize;
        assert_eq!(b - a, out[0].len(), "fragments must be contiguous");
    }

    #[test]
    fn a_nal_that_fits_is_a_zero_copy_view_of_the_frame() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        let f = frame(&[nal(1, 100)]);
        p.packetize(1188, &f, &mut out);
        assert_eq!(out.len(), 1);
        let base = f.as_ptr() as usize;
        let got = out[0].as_ptr() as usize;
        assert!(
            got >= base && got < base + f.len(),
            "payload should point into the frame buffer, not a copy"
        );
    }

    #[test]
    fn aud_and_filler_are_dropped() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(
            1188,
            &frame(&[nal(NAL_AUD, 1), nal(1, 10), nal(NAL_FILLER, 50)]),
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][0] & NAL_TYPE_MASK, 1);
    }

    /// A PPS NAL whose payload begins `pic_parameter_set_id = id`, `seq_parameter_set_id = 0`.
    fn pps_with_id(id: u8) -> Vec<u8> {
        // ue(v): `id + 1` in binary, after one zero for each bit past the first
        let code = u32::from(id) + 1;
        let width = 2 * (32 - code.leading_zeros()) - 1;
        // the id and sps id 0 ("1"), left-aligned in 16 bits, then a byte that keeps the NAL
        // from ending in a zero the start-code scan would claim
        let bits = ((code << 1) | 1) << (16 - width - 1);
        vec![0, 0, 0, 1, 0x68, (bits >> 8) as u8, bits as u8, 0x80]
    }

    fn payloads_of(parts: &[Vec<u8>]) -> Vec<Vec<u8>> {
        parts.iter().map(|n| n[4..].to_vec()).collect()
    }

    #[test]
    fn every_pps_of_a_multi_pps_access_unit_reaches_the_receiver() {
        // the Quick Sync keyframe shape: one SPS, five PPS (ids 0..4), then an IDR in MTU-sized
        // slices that each name one of them; a receiver drops an IDR whose PPS it has not seen,
        // so all five must precede the first slice
        let mut parts = vec![nal(NAL_SPS, 22)];
        parts.extend((0..5).map(pps_with_id));
        parts.extend((0..28).map(|_| nal(5, 1150)));
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&parts), &mut out);
        assert_eq!(
            out[0][0] & NAL_TYPE_MASK,
            NAL_STAP_A,
            "all six in one STAP-A"
        );
        assert_eq!(out.len(), 1 + 28);
        assert_eq!(
            reassemble(&out),
            payloads_of(&parts),
            "every NAL, in stream order"
        );
    }

    #[test]
    fn parameter_sets_that_overflow_one_stap_a_spill_into_the_next() {
        let mut parts = vec![nal(NAL_SPS, 20)];
        parts.extend((0..7).map(pps_with_id));
        parts.push(nal(5, 10));
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        // room for the SPS and a few PPS, not all of them
        p.packetize(40, &frame(&parts), &mut out);
        let staps = out
            .iter()
            .filter(|p| p[0] & NAL_TYPE_MASK == NAL_STAP_A)
            .count();
        assert!(staps >= 2, "split across aggregates: {staps}");
        assert!(out.iter().all(|p| p.len() <= 40));
        assert_eq!(reassemble(&out), payloads_of(&parts));
    }

    #[test]
    fn parameter_sets_after_a_slice_are_sent_where_they_stand() {
        let parts = [nal(5, 10), nal(NAL_SPS, 8), pps_with_id(1), nal(1, 10)];
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&parts), &mut out);
        assert_eq!(reassemble(&out), payloads_of(&parts));
    }

    #[test]
    fn a_keyframe_that_omits_a_parameter_set_gets_the_stored_copy() {
        let sps = nal(NAL_SPS, 8);
        let mut first = vec![sps.clone()];
        first.extend((0..3).map(pps_with_id));
        first.push(nal(5, 50));
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&first), &mut out);
        // a delta frame carrying only the PPS it uses, then an IDR carrying only PPS 1
        p.packetize(1188, &frame(&[pps_with_id(2), nal(1, 50)]), &mut out);
        assert_eq!(
            reassemble(&out),
            payloads_of(&[pps_with_id(2), nal(1, 50)]),
            "a delta frame is sent as it is"
        );
        p.packetize(1188, &frame(&[pps_with_id(1), nal(5, 50)]), &mut out);
        assert_eq!(
            reassemble(&out),
            payloads_of(&[
                sps,
                pps_with_id(1),
                pps_with_id(0),
                pps_with_id(2),
                nal(5, 50)
            ]),
            "missing SPS first, the unit's own PPS, then the missing PPS"
        );
    }

    #[test]
    fn a_parameter_set_with_a_known_id_replaces_the_stored_one() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        let old_pps = pps_with_id(0);
        let mut new_pps = pps_with_id(0);
        new_pps.push(0x40);
        p.packetize(
            1188,
            &frame(&[nal(NAL_SPS, 8), old_pps, nal(5, 20)]),
            &mut out,
        );
        p.packetize(1188, &frame(&[new_pps.clone(), nal(1, 20)]), &mut out);
        p.packetize(1188, &frame(&[nal(5, 20)]), &mut out);
        assert_eq!(
            reassemble(&out),
            payloads_of(&[nal(NAL_SPS, 8), new_pps, nal(5, 20)])
        );
    }

    #[test]
    fn a_pps_ahead_of_its_sps_is_sent_after_it() {
        let parts = [pps_with_id(0), nal(NAL_SPS, 8), nal(5, 40)];
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&parts), &mut out);
        assert_eq!(
            reassemble(&out),
            payloads_of(&[nal(NAL_SPS, 8), pps_with_id(0), nal(5, 40)])
        );
    }

    #[test]
    fn a_repeated_parameter_set_id_sends_only_the_last_copy() {
        let mut newer_pps = pps_with_id(0);
        newer_pps.push(0x40);
        let parts = [
            nal(NAL_SPS, 8),
            pps_with_id(0),
            nal(NAL_SPS, 9),
            newer_pps.clone(),
            nal(5, 40),
        ];
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&parts), &mut out);
        assert_eq!(
            reassemble(&out),
            payloads_of(&[nal(NAL_SPS, 9), newer_pps, nal(5, 40)])
        );
    }

    #[test]
    fn an_idr_carrying_an_sps_replaces_every_stored_set() {
        // five PPS, then an encoder rebuilt behind the same writer that uses one
        let mut first = vec![nal(NAL_SPS, 8)];
        first.extend((0..5).map(pps_with_id));
        first.push(nal(5, 40));
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&first), &mut out);
        let restated = [nal(NAL_SPS, 9), pps_with_id(0), nal(5, 40)];
        p.packetize(1188, &frame(&restated), &mut out);
        assert_eq!(
            reassemble(&out),
            payloads_of(&restated),
            "nothing stale added"
        );
        p.packetize(1188, &frame(&[nal(5, 40)]), &mut out);
        assert_eq!(
            reassemble(&out),
            payloads_of(&restated),
            "a bare IDR gets only the restated sets"
        );
    }

    #[test]
    fn a_stap_a_carries_at_most_ten_nal_units() {
        let mut parts = vec![nal(NAL_SPS, 8)];
        parts.extend((0..14).map(pps_with_id));
        parts.push(nal(5, 40));
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&parts), &mut out);
        for payload in out.iter().filter(|p| p[0] & NAL_TYPE_MASK == NAL_STAP_A) {
            assert!(reassemble(std::slice::from_ref(payload)).len() <= MAX_STAP_A_NALUS);
        }
        assert_eq!(reassemble(&out), payloads_of(&parts));
    }

    #[test]
    fn an_sei_ahead_of_a_bare_idr_still_gets_the_stored_sets() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(
            1188,
            &frame(&[nal(NAL_SPS, 8), pps_with_id(0), nal(5, 40)]),
            &mut out,
        );
        p.packetize(1188, &frame(&[nal(6, 12), nal(5, 40)]), &mut out);
        assert_eq!(
            reassemble(&out),
            payloads_of(&[nal(6, 12), nal(NAL_SPS, 8), pps_with_id(0), nal(5, 40)])
        );
    }

    #[test]
    fn a_stored_set_is_a_copy_made_only_when_it_changes() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        let first = frame(&[nal(NAL_SPS, 8), pps_with_id(0), nal(5, 40)]);
        p.packetize(1188, &first, &mut out);
        let inside = |b: &Bytes, f: &Bytes| f.as_ptr_range().contains(&b.as_ptr());
        assert!(p.stored.iter().all(|(_, _, b)| !inside(b, &first)));
        let pps_at = p.stored[1].2.as_ptr();
        p.packetize(1188, &frame(&[pps_with_id(0), nal(1, 40)]), &mut out);
        assert_eq!(
            p.stored[1].2.as_ptr(),
            pps_at,
            "an unchanged set is not copied again"
        );
    }

    #[test]
    fn a_parameter_set_larger_than_the_mtu_is_counted() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(
            40,
            &frame(&[nal(NAL_SPS, 60), pps_with_id(0), nal(5, 10)]),
            &mut out,
        );
        assert_eq!(p.fragmented_parameter_sets(), 1);
        assert_eq!(
            reassemble(&out),
            payloads_of(&[nal(NAL_SPS, 60), pps_with_id(0), nal(5, 10)])
        );
    }

    #[test]
    fn parameter_set_ids_are_read_through_emulation_prevention() {
        assert_eq!(parameter_set_key(&pps_with_id(4)[4..]), Some((NAL_PPS, 4)));
        // SPS: profile 0x64, constraints 0, level 0x2a, then sps id 0
        assert_eq!(
            parameter_set_key(&[0x67, 0x64, 0x00, 0x2a, 0xac]),
            Some((NAL_SPS, 0))
        );
        // profile 0, constraints 0, an emulation-prevention byte, level 1, then sps id 1 (010)
        assert_eq!(
            parameter_set_key(&[0x67, 0x00, 0x00, 0x03, 0x01, 0x40]),
            Some((NAL_SPS, 1))
        );
        assert_eq!(parameter_set_key(&[0x65, 0x88]), None);
        assert_eq!(parameter_set_key(&[0x68]), None);
    }

    #[test]
    fn a_parameter_sets_only_access_unit_is_sent_instead_of_stashed() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&[nal(NAL_SPS, 20), nal(NAL_PPS, 4)]), &mut out);
        assert_eq!(out.len(), 1);
        assert!(payloads_carry_parameter_sets(&out));
    }

    #[test]
    fn three_byte_start_codes_are_understood() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        let mut v = vec![0u8, 0, 1, 0x65];
        v.extend([0xAA; 20]);
        v.extend([0u8, 0, 1, 0x61]);
        v.extend([0xAA; 20]);
        p.packetize(1188, &Bytes::from(v), &mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn an_empty_or_degenerate_frame_produces_nothing_and_does_not_spin() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &Bytes::new(), &mut out);
        assert!(out.is_empty());
        p.packetize(1188, &Bytes::from_static(&[0, 0, 1]), &mut out);
        assert!(out.is_empty());
        p.packetize(1188, &Bytes::from_static(&[0, 0, 1, 0, 0, 1]), &mut out);
        assert!(out.is_empty());
        p.packetize(0, &Bytes::from_static(&[0, 0, 1, 0x65, 1]), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn an_idr_with_no_parameter_sets_is_flagged() {
        let mut p = H264Packetizer::new();
        let mut out = Vec::new();
        p.packetize(1188, &frame(&[nal(5, 100)]), &mut out);
        assert!(!payloads_carry_parameter_sets(&out));
    }

    #[test]
    fn a_keyframe_access_unit_is_recognised_by_its_parameter_sets() {
        assert!(access_unit_has_sps(&frame(&[
            nal(NAL_AUD, 1),
            nal(NAL_SPS, 8),
            nal(NAL_PPS, 8),
            nal(5, 8)
        ])));
        assert!(!access_unit_has_sps(&frame(&[nal(1, 8)])));
        assert!(!access_unit_has_sps(&[]));
        assert!(!access_unit_has_sps(&[0, 0, 1]));
    }
}

/// A reference packetizer that keeps one SPS and one PPS. A stream with one of each (NVENC, x264,
/// VideoToolbox) must come out of the current one byte for byte the same.
#[cfg(test)]
mod legacy_parity {
    use super::*;

    const STAP_A_OVERHEAD: usize = 1 + 2 + 2;

    #[derive(Default)]
    struct Legacy {
        sps: Option<Bytes>,
        pps: Option<Bytes>,
        dropped_parameter_sets: u64,
    }

    impl Legacy {
        pub fn packetize(&mut self, mtu: usize, frame: &Bytes, out: &mut Vec<Bytes>) {
            out.clear();
            if mtu <= FU_A_HEADER || frame.is_empty() {
                return;
            }
            out.reserve(frame.len().div_ceil(mtu).max(1));

            let mut emitted_vcl = false;
            for (start, end) in AnnexBNals::new(frame) {
                let nal = frame.slice(start..end);
                let Some(&first) = nal.first() else { continue };
                match first & NAL_TYPE_MASK {
                    NAL_AUD | NAL_FILLER => continue,
                    NAL_SPS => {
                        self.sps = Some(nal);
                        continue;
                    }
                    NAL_PPS => {
                        self.pps = Some(nal);
                        continue;
                    }
                    _ => {}
                }
                if !emitted_vcl {
                    emitted_vcl = true;
                    self.flush_parameter_sets(mtu, out);
                }
                self.emit_nal(mtu, nal, out);
            }
            if !emitted_vcl {
                self.flush_parameter_sets(mtu, out);
            }
            self.sps = None;
            self.pps = None;
        }

        fn flush_parameter_sets(&mut self, mtu: usize, out: &mut Vec<Bytes>) {
            let (Some(sps), Some(pps)) = (self.sps.clone(), self.pps.clone()) else {
                if let Some(sps) = self.sps.clone() {
                    self.emit_nal(mtu, sps, out);
                }
                if let Some(pps) = self.pps.clone() {
                    self.emit_nal(mtu, pps, out);
                }
                return;
            };

            let aggregate_len = STAP_A_OVERHEAD + sps.len() + pps.len();
            if aggregate_len <= mtu {
                let mut buf = BytesMut::with_capacity(aggregate_len);
                let nri = (sps[0] & 0x60).max(pps[0] & 0x60);
                buf.put_u8(nri | NAL_STAP_A);
                buf.put_u16(sps.len() as u16);
                buf.put_slice(&sps);
                buf.put_u16(pps.len() as u16);
                buf.put_slice(&pps);
                out.push(buf.freeze());
                return;
            }

            self.emit_nal(mtu, sps, out);
            self.emit_nal(mtu, pps, out);
        }

        fn emit_nal(&mut self, mtu: usize, nal: Bytes, out: &mut Vec<Bytes>) {
            if nal.len() <= mtu {
                out.push(nal);
                return;
            }

            let header = nal[0];
            let nal_type = header & NAL_TYPE_MASK;
            let nri = header & 0x60;
            let body = &nal[1..];
            let per_fragment = mtu - FU_A_HEADER;
            if per_fragment == 0 {
                if matches!(nal_type, NAL_SPS | NAL_PPS) {
                    self.dropped_parameter_sets += 1;
                }
                return;
            }

            let fragments = body.len().div_ceil(per_fragment);
            let mut buf = BytesMut::with_capacity(body.len() + fragments * FU_A_HEADER);
            let mut offset = 0usize;
            let mut index = 0usize;
            while offset < body.len() {
                let take = per_fragment.min(body.len() - offset);
                let start = index == 0;
                let end = offset + take == body.len();
                buf.put_u8(nri | NAL_FU_A);
                buf.put_u8((u8::from(start) << 7) | (u8::from(end) << 6) | nal_type);
                buf.put_slice(&body[offset..offset + take]);
                offset += take;
                index += 1;
            }
            for i in 0..fragments {
                let take = per_fragment.min(body.len() - i * per_fragment);
                let chunk = buf.split_to(FU_A_HEADER + take);
                out.push(chunk.freeze());
            }
        }
    }

    fn nal(header: u8, len: usize, fill: u8) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, header];
        v.extend((0..len).map(|i| fill.wrapping_add(i as u8) | 1));
        v
    }

    /// SPS and PPS with real id fields (both id 0), so the current packetizer files them.
    fn sps() -> Vec<u8> {
        vec![
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x2a, 0xac, 0x2b, 0x40, 0x3c, 0x01, 0x13, 0xf2, 0xe0,
        ]
    }

    fn pps() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x68, 0xee, 0x3c, 0xb0]
    }

    fn au(parts: &[Vec<u8>]) -> Bytes {
        Bytes::from(parts.concat())
    }

    /// The access-unit shapes the single-PPS encoders produce: every IDR led by its parameter sets
    /// (with and without an AUD and an SEI), large and small, delta frames with and without a
    /// repeated PPS, several slices, a parameter-sets-only unit and three-byte start codes. An
    /// IDR without its parameter sets is where the two differ on purpose: the stored ones are
    /// added.
    fn corpus() -> Vec<Bytes> {
        let mut three_byte = vec![0u8, 0, 1];
        three_byte.extend(&sps()[4..]);
        three_byte.extend([0u8, 0, 1]);
        three_byte.extend(&pps()[4..]);
        three_byte.extend([0u8, 0, 1, 0x65]);
        three_byte.extend([0x5a; 900]);
        three_byte.extend([0u8, 0, 1, 0x41]);
        three_byte.extend([0x5a; 40]);
        vec![
            au(&[
                nal(0x09, 1, 0x10),
                sps(),
                pps(),
                nal(0x06, 30, 7),
                nal(0x65, 40_000, 3),
            ]),
            au(&[nal(0x41, 3_000, 9)]),
            au(&[nal(0x41, 400, 1), nal(0x41, 1_500, 2), nal(0x41, 80, 5)]),
            au(&[pps(), nal(0x41, 2_000, 4)]),
            au(&[sps(), pps(), nal(0x65, 900, 6)]),
            au(&[sps(), pps(), nal(0x65, 1_200, 8), nal(0x65, 1_200, 11)]),
            au(&[sps(), pps()]),
            au(&[nal(0x01, 10, 12)]),
            Bytes::from(three_byte),
            au(&[sps(), pps(), nal(0x65, 120_000, 13)]),
        ]
    }

    fn assert_same(frames: &[Bytes], mtu: usize) {
        let mut old = Legacy::default();
        let mut new = H264Packetizer::new();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for (i, f) in frames.iter().enumerate() {
            old.packetize(mtu, f, &mut a);
            new.packetize(mtu, f, &mut b);
            assert_eq!(a, b, "access unit {i} at a {mtu}-byte MTU");
        }
    }

    #[test]
    fn single_parameter_set_streams_packetize_byte_for_byte_as_before() {
        let frames = corpus();
        for mtu in [1188, 1200, 600, 64, 24] {
            assert_same(&frames, mtu);
        }
    }

    /// The last access unit of `frames` through both packetizers, after the ones before it.
    fn last_through_both(frames: &[Bytes]) -> (Vec<Bytes>, Vec<Bytes>) {
        let mut old = Legacy::default();
        let mut new = H264Packetizer::new();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for f in frames {
            old.packetize(1188, f, &mut a);
            new.packetize(1188, f, &mut b);
        }
        (a, b)
    }

    fn carries(payloads: &[Bytes], kind: u8) -> bool {
        payloads.iter().any(|p| match p[0] & NAL_TYPE_MASK {
            NAL_STAP_A => {
                let mut i = 1;
                let mut found = false;
                while i + 2 <= p.len() {
                    let n = usize::from(u16::from_be_bytes([p[i], p[i + 1]]));
                    found |= p[i + 2] & NAL_TYPE_MASK == kind;
                    i += 2 + n;
                }
                found
            }
            k => k == kind,
        })
    }

    /// Shapes no shipping encoder produces, where the two may differ, each for a reason.
    #[test]
    fn where_the_two_differ_the_old_one_dropped_sets() {
        let sei = nal(0x06, 12, 1);
        let idr = nal(0x65, 400, 2);

        // an SEI ahead of the sets: the old one flushes at the SEI, before the sets arrive, and
        // never again, so it drops them
        let (old, new) = last_through_both(&[au(&[sei.clone(), sps(), pps(), idr.clone()])]);
        assert!(!carries(&old, NAL_SPS) && carries(&new, NAL_SPS));

        // sets after the last slice: dropped by the old one, sent by the new
        let (old, new) = last_through_both(&[au(&[idr.clone(), sps(), pps()])]);
        assert!(!carries(&old, NAL_PPS) && carries(&new, NAL_PPS));

        // a parameter-sets-only unit, then a bare IDR: the new one restates the sets in front of
        // it, so a viewer starting there can decode it
        let (old, new) = last_through_both(&[au(&[sps(), pps()]), au(std::slice::from_ref(&idr))]);
        assert!(!carries(&old, NAL_SPS) && carries(&new, NAL_SPS));

        // the same with an SEI ahead of the bare IDR
        let (old, new) = last_through_both(&[
            au(&[sps(), pps(), idr.clone()]),
            au(&[sei.clone(), idr.clone()]),
        ]);
        assert!(!carries(&old, NAL_SPS) && carries(&new, NAL_SPS));
    }

    #[test]
    fn a_pps_ahead_of_its_sps_and_repeated_sets_come_out_as_before() {
        let idr = nal(0x65, 400, 2);
        let mut later_sps = sps();
        later_sps.push(0x11);
        let mut later_pps = pps();
        later_pps.push(0x22);
        let frames = [
            au(&[pps(), sps(), idr.clone()]),
            au(&[sps(), pps(), later_sps, later_pps, idr]),
        ];
        assert_same(&frames, 1188);
    }

    /// A Quick Sync keyframe with five PPS through the single-PPS packetizer: only the last one
    /// reaches the wire, so a receiver drops an IDR naming any other.
    #[test]
    fn the_old_packetizer_sent_only_the_last_of_several_pps() {
        let mut parts = vec![sps()];
        for id in 0..5u8 {
            // pic_parameter_set_id `id`, seq_parameter_set_id 0, as ue(v) codes
            let byte = match id {
                0 => 0b1100_0000,
                1 => 0b0101_0000,
                2 => 0b0111_0000,
                3 => 0b0010_0100,
                _ => 0b0010_1100,
            };
            parts.push(vec![0, 0, 0, 1, 0x68, byte, 0x80]);
        }
        parts.push(nal(0x65, 900, 3));
        let f = au(&parts);
        let ids = |payloads: &[Bytes]| -> Vec<u32> {
            let mut out = Vec::new();
            for p in payloads {
                let nals: Vec<&[u8]> = if p[0] & NAL_TYPE_MASK == NAL_STAP_A {
                    let mut v = Vec::new();
                    let mut i = 1;
                    while i + 2 <= p.len() {
                        let n = usize::from(u16::from_be_bytes([p[i], p[i + 1]]));
                        v.push(&p[i + 2..i + 2 + n]);
                        i += 2 + n;
                    }
                    v
                } else {
                    vec![&p[..]]
                };
                out.extend(
                    nals.iter()
                        .filter_map(|n| parameter_set_key(n))
                        .filter(|(kind, _)| *kind == NAL_PPS)
                        .map(|(_, id)| id),
                );
            }
            out
        };
        let (mut a, mut b) = (Vec::new(), Vec::new());
        Legacy::default().packetize(1188, &f, &mut a);
        H264Packetizer::new().packetize(1188, &f, &mut b);
        assert_eq!(ids(&a), vec![4]);
        assert_eq!(ids(&b), vec![0, 1, 2, 3, 4]);
    }

    /// The same comparison over a recorded Annex-B stream, split into access units, when
    /// `SE_PACKETIZER_CORPUS` names one.
    #[test]
    fn a_recorded_single_pps_stream_packetizes_byte_for_byte_as_before() {
        let Ok(path) = crate::envcfg::var("SE_PACKETIZER_CORPUS") else {
            crate::test_support::skip_because(
                "recorded packetizer parity",
                "SE_PACKETIZER_CORPUS is not set",
            );
            return;
        };
        let data = std::fs::read(&path).expect("read SE_PACKETIZER_CORPUS");
        let frames = split_access_units(&data);
        assert!(!frames.is_empty(), "no access units in {path}");
        assert_same(&frames, 1188);
        eprintln!("{} access units of {path} identical", frames.len());
    }

    /// Splits at the first non-VCL NAL after a slice, or at a slice with `first_mb_in_slice` 0
    /// after a slice.
    fn split_access_units(data: &[u8]) -> Vec<Bytes> {
        let bytes = Bytes::copy_from_slice(data);
        let mut out = Vec::new();
        let mut start = None;
        let mut after_vcl = false;
        for (s, e) in AnnexBNals::new(&bytes) {
            let at = s - if s >= 4 && bytes[s - 4] == 0 { 4 } else { 3 };
            let kind = bytes[s] & NAL_TYPE_MASK;
            let vcl = kind == 1 || kind == 5;
            let first_slice = vcl && e > s + 1 && bytes[s + 1] & 0x80 != 0;
            if after_vcl && (!vcl || first_slice) {
                if let Some(b) = start {
                    out.push(bytes.slice(b..at));
                }
                start = None;
            }
            start.get_or_insert(at);
            after_vcl = vcl;
        }
        if let Some(b) = start {
            out.push(bytes.slice(b..));
        }
        out
    }
}

/// A/B against the stock `H264Payloader`.
///
/// `cargo test --lib packetizer::bench -- --ignored --nocapture`
#[cfg(test)]
mod bench {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    /// Counts allocations: one relaxed increment each.
    struct Counting;

    static ALLOCS: AtomicU64 = AtomicU64::new(0);

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, l: Layout) {
            unsafe { System.dealloc(ptr, l) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, l: Layout, n: usize) -> *mut u8 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            unsafe { System.realloc(ptr, l, n) }
        }
    }

    #[global_allocator]
    static ALLOC: Counting = Counting;

    fn synthetic_idr(bytes: usize) -> Bytes {
        let mut v = vec![0u8, 0, 0, 1, 0x67];
        v.resize(v.len() + 40, 0xAA); // SPS
        v.extend_from_slice(&[0, 0, 0, 1, 0x68]);
        v.resize(v.len() + 8, 0xBB); // PPS
        v.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        v.resize(v.len() + bytes, 0xCC); // IDR slice
        Bytes::from(v)
    }

    fn synthetic_p(bytes: usize) -> Bytes {
        let mut v = vec![0u8, 0, 0, 1, 0x41];
        v.resize(v.len() + bytes, 0xDD);
        Bytes::from(v)
    }

    #[test]
    #[ignore = "benchmark against the stock payloader, not an assertion; run with --ignored --nocapture"]
    fn compare_with_the_stock_payloader() {
        use webrtc::rtp::codecs::h264::H264Payloader;
        use webrtc::rtp::packetizer::Payloader;

        const MTU: usize = 1188;
        const ITERS: usize = 2_000;
        // one IDR then 59 P-frames: a 60 fps second with an infinite GOP and a keyframe forced
        // at the top
        let idr = synthetic_idr(300 * 1024);
        let p = synthetic_p(16 * 1024);

        // interleaved rounds and the minimum per implementation: a mean folds in machine load
        // (the same stock payloader measured 18404 and 10812 ns/frame an hour apart), and
        // interleaving stops whichever goes first from paying to warm the frame buffers
        const ROUNDS: usize = 7;
        let mut ours = H264Packetizer::new();
        let mut theirs = H264Payloader::default();
        let mut out: Vec<Bytes> = Vec::new();
        let mut ours_packets = 0usize;
        let mut theirs_packets = 0usize;
        let mut ours_ns = f64::MAX;
        let mut theirs_ns = f64::MAX;
        let mut ours_allocs = 0u64;
        let mut theirs_allocs = 0u64;

        for round in 0..ROUNDS {
            let a0 = ALLOCS.load(Ordering::Relaxed);
            let t0 = Instant::now();
            for i in 0..ITERS {
                let frame = if i % 60 == 0 { &idr } else { &p };
                ours.packetize(MTU, frame, &mut out);
                ours_packets += out.len();
            }
            let ns = t0.elapsed().as_nanos() as f64 / ITERS as f64;
            let allocs = ALLOCS.load(Ordering::Relaxed) - a0;

            let a1 = ALLOCS.load(Ordering::Relaxed);
            let t1 = Instant::now();
            for i in 0..ITERS {
                let frame = if i % 60 == 0 { &idr } else { &p };
                let payloads = theirs.payload(MTU, frame).expect("payload");
                theirs_packets += payloads.len();
            }
            let their_ns = t1.elapsed().as_nanos() as f64 / ITERS as f64;
            let their_allocs = ALLOCS.load(Ordering::Relaxed) - a1;

            println!("  round {round}: local {ns:.0} ns/frame, stock {their_ns:.0} ns/frame");
            if ns < ours_ns {
                ours_ns = ns;
            }
            if their_ns < theirs_ns {
                theirs_ns = their_ns;
            }
            ours_allocs = allocs;
            theirs_allocs = their_allocs;
        }
        ours_packets /= ROUNDS;
        theirs_packets /= ROUNDS;

        println!(
            "packetize {ITERS} frames @ {MTU}B MTU (1 IDR per 60), best of {ROUNDS} interleaved rounds:\n  \
             local  {ours_ns:.0} ns/frame, {ours_packets} packets, {ours_allocs} allocations\n  \
             stock  {theirs_ns:.0} ns/frame, {theirs_packets} packets, {theirs_allocs} allocations\n  \
             speedup {:.2}x, allocations {:.1}x fewer",
            theirs_ns / ours_ns,
            theirs_allocs as f64 / ours_allocs.max(1) as f64
        );
        // the comparison means nothing unless both produce the same number of packets
        assert!(
            (ours_packets as i64 - theirs_packets as i64).abs() <= ITERS as i64 / 60,
            "packet counts diverge: {ours_packets} vs {theirs_packets}"
        );
        assert!(
            ours_allocs * 4 < theirs_allocs,
            "the point of the rewrite is far fewer allocations: {ours_allocs} vs {theirs_allocs}"
        );
    }
}

//! RFC 6184 packetization for the per-viewer RTP writer.
//!
//! This replaces `rtp::codecs::h264::H264Payloader`, which had two problems the writer could not
//! work around from outside.
//!
//! **It could drop the parameter sets and say nothing.** The stock payloader stashes SPS and PPS
//! and re-emits them as a STAP-A aggregate in front of the next VCL NAL — but only if the
//! aggregate fits the MTU. If it does not, the aggregate is not pushed, no error is returned, and
//! the stashed copies are cleared anyway so they are never retried. With an infinite GOP the
//! client then has nothing to initialise its decoder from, ever: a black screen with RTP flowing
//! and every host-side signal reporting success. RFC 6184 never required aggregation, so this
//! emits them as their own packets when they do not fit together, and FU-A fragments either one
//! if it does not fit alone.
//!
//! **It allocated once per fragment.** `emit` built a fresh `BytesMut` per FU-A fragment plus a
//! `copy_from_slice`; a 300 KB IDR at a 1188-byte MTU is ~260 allocations landing in one tight
//! loop on a tokio worker that is also driving socket writes, and the whole thing is redone for
//! every connected viewer. Here a NAL that must be fragmented gets **one** allocation for the
//! whole NAL, carved into per-fragment `Bytes` with `split_to`, so the fragments share it by
//! refcount. A NAL that fits the MTU is a `Bytes::slice` of the frame — no allocation and no copy
//! at all, which the stock payloader also managed and is worth keeping.

use bytes::{BufMut, Bytes, BytesMut};

const NAL_TYPE_MASK: u8 = 0x1f;

const NAL_AUD: u8 = 9;
const NAL_FILLER: u8 = 12;
pub(crate) const NAL_SPS: u8 = 7;
pub(crate) const NAL_PPS: u8 = 8;

/// STAP-A, RFC 6184 §5.7.1.
pub(crate) const NAL_STAP_A: u8 = 24;
/// FU-A, RFC 6184 §5.8.
const NAL_FU_A: u8 = 28;

/// One byte of STAP-A NAL header plus a 2-byte size prefix per aggregated NAL.
const STAP_A_OVERHEAD: usize = 1 + 2 + 2;
/// FU indicator plus FU header.
const FU_A_HEADER: usize = 2;

/// Packetizes H.264 access units in Annex-B form into RTP payloads.
///
/// One per viewer, because the parameter-set stash is a per-stream state machine. It holds no
/// buffers between calls, so a viewer costs nothing while it is idle.
#[derive(Default)]
pub struct H264Packetizer {
    sps: Option<Bytes>,
    pps: Option<Bytes>,
    /// Parameter sets were dropped because the caller's MTU could not carry them even fragmented.
    /// Only reachable at an absurd MTU; counted rather than assumed impossible.
    dropped_parameter_sets: u64,
}

impl H264Packetizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dropped_parameter_sets(&self) -> u64 {
        self.dropped_parameter_sets
    }

    /// Packetize one access unit, appending to `out`.
    ///
    /// `frame` must be the whole access unit in Annex-B form. `out` is cleared first and is the
    /// caller's to reuse across frames, which is what keeps the payload vector off the allocator
    /// on the steady-state path.
    pub fn packetize(&mut self, mtu: usize, frame: &Bytes, out: &mut Vec<Bytes>) {
        out.clear();
        if mtu <= FU_A_HEADER || frame.is_empty() {
            return;
        }
        // The frame is at least this many packets; reserving up front is the difference between
        // one growth and eight on a large IDR.
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
        // A parameter-sets-only access unit — some encoders emit one at session start. The stock
        // payloader returned an empty payload list for it, which the writer could only answer with
        // another IDR request. Sending them is both correct and what stops that spin.
        if !emitted_vcl {
            self.flush_parameter_sets(mtu, out);
        }
        self.sps = None;
        self.pps = None;
    }

    fn flush_parameter_sets(&mut self, mtu: usize, out: &mut Vec<Bytes>) {
        let (Some(sps), Some(pps)) = (self.sps.clone(), self.pps.clone()) else {
            // Only one of the pair, which no conforming encoder emits. Send what there is rather
            // than dropping it.
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
            // The STAP-A NAL header takes F and NRI from the highest of the aggregated NALs.
            let nri = (sps[0] & 0x60).max(pps[0] & 0x60);
            buf.put_u8(nri | NAL_STAP_A);
            buf.put_u16(sps.len() as u16);
            buf.put_slice(&sps);
            buf.put_u16(pps.len() as u16);
            buf.put_slice(&pps);
            out.push(buf.freeze());
            return;
        }

        // They do not fit together. RFC 6184 has never required aggregation, so send them
        // separately — fragmenting either one if it needs it.
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
        // One allocation for the whole NAL. `split_to` hands out views into it, so every fragment
        // after the first costs a refcount bump instead of a `BytesMut::with_capacity`.
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

/// Walks Annex-B start codes, yielding `(start, end)` byte ranges of each NAL unit's payload.
struct AnnexBNals<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> AnnexBNals<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Length of the start code at `i`, or `None`.
    fn start_code_at(buf: &[u8], i: usize) -> Option<usize> {
        if buf.len() >= i + 4
            && buf[i] == 0
            && buf[i + 1] == 0
            && buf[i + 2] == 0
            && buf[i + 3] == 1
        {
            Some(4)
        } else if buf.len() >= i + 3 && buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            Some(3)
        } else {
            None
        }
    }
}

impl Iterator for AnnexBNals<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        let buf = self.buf;
        // Find the next start code.
        while self.pos < buf.len() {
            match Self::start_code_at(buf, self.pos) {
                Some(len) => {
                    let start = self.pos + len;
                    let mut end = start;
                    while end < buf.len() {
                        match Self::start_code_at(buf, end) {
                            Some(_) => break,
                            None => end += 1,
                        }
                    }
                    self.pos = end;
                    if end > start {
                        return Some((start, end));
                    }
                    // An empty NAL between two start codes: skip it rather than emitting nothing
                    // forever.
                    if end >= buf.len() {
                        return None;
                    }
                }
                None => self.pos += 1,
            }
        }
        None
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
        // This is the case the stock payloader dropped in silence.
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
        // The point of the rewrite: one `BytesMut` carved up, not one per fragment. Two
        // fragments carved from the same buffer are adjacent in memory.
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

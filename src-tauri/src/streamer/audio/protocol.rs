//! The audio DataChannel wire format: a 13-byte header (`seq` u32 LE, `capture_ns` u64 LE,
//! `flags` u8), then the Opus payload.
//!
//! With [`FLAG_REDUNDANT`] the body is a u16 LE length, the current frame's Opus packet, then
//! the previous frame's (RFC 2198-style redundancy without the RTP framing). It is the only
//! loss tool here: in-band FEC is SILK-only and this encoder is CELT-only. The host sets the
//! flag only for a client whose join request advertised support.

use bytes::{BufMut, Bytes, BytesMut};

use super::{FLAG_DISCONTINUITY, FLAG_REDUNDANT, FLAG_SILENT};

pub const HEADER_LEN: usize = 13;
/// Length prefix in front of the primary payload when [`FLAG_REDUNDANT`] is set.
pub const REDUNDANCY_PREFIX_LEN: usize = 2;

pub fn write_message(out: &mut [u8], seq: u32, capture_ns: u64, flags: u8, opus: &[u8]) -> usize {
    let total = HEADER_LEN + opus.len();
    debug_assert!(
        out.len() >= total,
        "audio protocol: output buffer too small"
    );
    out[0..4].copy_from_slice(&seq.to_le_bytes());
    out[4..12].copy_from_slice(&capture_ns.to_le_bytes());
    out[12] = flags;
    out[HEADER_LEN..total].copy_from_slice(opus);
    total
}

pub fn build_message(seq: u32, capture_ns: u64, flags: u8, opus: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_LEN + opus.len()];
    write_message(&mut out, seq, capture_ns, flags, opus);
    out
}

/// Build one DataChannel message, ready to send: one allocation and one copy of each
/// payload, on a path that runs 200 times a second per session.
///
/// `redundant` carries the previous frame's Opus payload and sets [`FLAG_REDUNDANT`]. Pass
/// `None` unless the client advertised that it can parse it.
pub fn build_datachannel_message(
    seq: u32,
    capture_ns: u64,
    flags: u8,
    opus: &[u8],
    redundant: Option<&[u8]>,
) -> Bytes {
    // a payload too long for the u16 prefix travels alone (unreachable at 128 kbps / 5 ms)
    let redundant = redundant.filter(|_| opus.len() <= u16::MAX as usize);
    let extra = redundant.map_or(0, |r| REDUNDANCY_PREFIX_LEN + r.len());
    let mut buf = BytesMut::with_capacity(HEADER_LEN + opus.len() + extra);
    buf.put_u32_le(seq);
    buf.put_u64_le(capture_ns);
    buf.put_u8(if redundant.is_some() {
        flags | FLAG_REDUNDANT
    } else {
        flags
    });
    if let Some(prev) = redundant {
        buf.put_u16_le(opus.len() as u16);
        buf.put_slice(opus);
        buf.put_slice(prev);
    } else {
        buf.put_slice(opus);
    }
    buf.freeze()
}

/// Split a body into (current, previous) according to `flags`. The host never parses its
/// own messages; this keeps the wire format in one place for the tests.
pub fn split_payloads<'a>(
    header: &AudioHeader,
    body: &'a [u8],
) -> Option<(&'a [u8], Option<&'a [u8]>)> {
    if !header.is_redundant() {
        return Some((body, None));
    }
    if body.len() < REDUNDANCY_PREFIX_LEN {
        return None;
    }
    let primary_len = u16::from_le_bytes([body[0], body[1]]) as usize;
    let rest = &body[REDUNDANCY_PREFIX_LEN..];
    if rest.len() < primary_len {
        return None;
    }
    Some((&rest[..primary_len], Some(&rest[primary_len..])))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub seq: u32,
    pub capture_ns: u64,
    pub flags: u8,
}

impl AudioHeader {
    pub fn is_silent(&self) -> bool {
        self.flags & FLAG_SILENT != 0
    }
    pub fn is_discontinuity(&self) -> bool {
        self.flags & FLAG_DISCONTINUITY != 0
    }
    pub fn is_redundant(&self) -> bool {
        self.flags & FLAG_REDUNDANT != 0
    }
}

pub fn parse(b: &[u8]) -> Option<(AudioHeader, &[u8])> {
    if b.len() < HEADER_LEN {
        return None;
    }
    let seq = u32::from_le_bytes(b[0..4].try_into().ok()?);
    let capture_ns = u64::from_le_bytes(b[4..12].try_into().ok()?);
    let flags = b[12];
    Some((
        AudioHeader {
            seq,
            capture_ns,
            flags,
        },
        &b[HEADER_LEN..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let opus = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let msg = build_message(0x11223344, 0x0102_0304_0506_0708, FLAG_SILENT, &opus);
        assert_eq!(msg.len(), HEADER_LEN + opus.len());
        let (hdr, payload) = parse(&msg).expect("parse");
        assert_eq!(hdr.seq, 0x11223344);
        assert_eq!(hdr.capture_ns, 0x0102_0304_0506_0708);
        assert!(hdr.is_silent());
        assert!(!hdr.is_discontinuity());
        assert_eq!(payload, &opus);
    }

    #[test]
    fn flags_combine() {
        let msg = build_message(1, 2, FLAG_SILENT | FLAG_DISCONTINUITY, &[9]);
        let (hdr, payload) = parse(&msg).unwrap();
        assert!(hdr.is_silent());
        assert!(hdr.is_discontinuity());
        assert_eq!(payload, &[9]);
    }

    #[test]
    fn empty_opus_ok() {
        let msg = build_message(7, 8, 0, &[]);
        assert_eq!(msg.len(), HEADER_LEN);
        let (hdr, payload) = parse(&msg).unwrap();
        assert_eq!(hdr.seq, 7);
        assert!(payload.is_empty());
    }

    #[test]
    fn truncated_is_none() {
        assert!(parse(&[0u8; HEADER_LEN - 1]).is_none());
        assert!(parse(&[]).is_none());
    }

    #[test]
    fn datachannel_message_without_redundancy_matches_the_plain_form() {
        let opus = [1u8, 2, 3, 4];
        let built = build_datachannel_message(9, 12345, FLAG_SILENT, &opus, None);
        assert_eq!(&built[..], &build_message(9, 12345, FLAG_SILENT, &opus)[..]);
        let (hdr, body) = parse(&built).unwrap();
        assert!(!hdr.is_redundant());
        assert_eq!(split_payloads(&hdr, body), Some((&opus[..], None)));
    }

    #[test]
    fn redundant_message_carries_both_payloads() {
        let cur = [1u8, 2, 3, 4, 5];
        let prev = [9u8, 8, 7];
        let built = build_datachannel_message(1, 2, 0, &cur, Some(&prev));
        assert_eq!(built.len(), HEADER_LEN + REDUNDANCY_PREFIX_LEN + 5 + 3);
        let (hdr, body) = parse(&built).unwrap();
        assert!(hdr.is_redundant());
        assert_eq!(hdr.seq, 1);
        assert_eq!(
            split_payloads(&hdr, body),
            Some((&cur[..], Some(&prev[..])))
        );
    }

    #[test]
    fn a_truncated_redundant_body_is_rejected_not_misparsed() {
        let hdr = AudioHeader {
            seq: 0,
            capture_ns: 0,
            flags: FLAG_REDUNDANT,
        };
        assert_eq!(split_payloads(&hdr, &[]), None);
        assert_eq!(split_payloads(&hdr, &[1]), None);
        // claims a 9-byte primary but carries 2
        assert_eq!(split_payloads(&hdr, &[9, 0, 1, 2]), None);
    }

    #[test]
    fn an_empty_previous_frame_still_sets_the_flag() {
        let built = build_datachannel_message(4, 5, 0, &[7, 7], Some(&[]));
        let (hdr, body) = parse(&built).unwrap();
        assert!(hdr.is_redundant());
        assert_eq!(
            split_payloads(&hdr, body),
            Some((&[7u8, 7][..], Some(&[][..])))
        );
    }

    #[test]
    fn write_message_returns_len() {
        let mut buf = [0u8; 32];
        let n = write_message(&mut buf, 1, 2, 0, &[1, 2, 3]);
        assert_eq!(n, HEADER_LEN + 3);
        let (_, payload) = parse(&buf[..n]).unwrap();
        assert_eq!(payload, &[1, 2, 3]);
    }
}

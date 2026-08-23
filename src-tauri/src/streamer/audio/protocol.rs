//! Binary wire format for the host→client audio DataChannel (PRD §6.1).
//!
//! Each message is a 13-byte little-endian header followed by the raw Opus packet:
//!
//! ```text
//! offset  size  field
//!   0      4    sequence number      (u32 LE)
//!   4      8    capture timestamp    (u64 LE, host monotonic nanoseconds)
//!  12      1    flags                (u8: bit0 SILENT, bit1 DISCONTINUITY)
//!  13      ..   raw Opus packet
//! ```
//!
//! Modeled on `streamer/input/protocol.rs`: fixed offsets, little-endian, no allocations on
//! the hot path (the caller supplies the output buffer).

use super::{FLAG_DISCONTINUITY, FLAG_SILENT};

/// Size of the fixed header that precedes every Opus packet.
pub const HEADER_LEN: usize = 13;

/// Serialize a header + Opus packet into `out`, returning the number of bytes written.
/// `out` must be at least `HEADER_LEN + opus.len()` bytes; extra capacity is ignored.
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

/// Convenience allocator variant for tests / non-hot-path callers.
pub fn build_message(seq: u32, capture_ns: u64, flags: u8, opus: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_LEN + opus.len()];
    write_message(&mut out, seq, capture_ns, flags, opus);
    out
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
}

/// Parse a message, returning the header and the Opus-packet slice. `None` if too short.
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
    fn write_message_returns_len() {
        let mut buf = [0u8; 32];
        let n = write_message(&mut buf, 1, 2, 0, &[1, 2, 3]);
        assert_eq!(n, HEADER_LEN + 3);
        let (_, payload) = parse(&buf[..n]).unwrap();
        assert_eq!(payload, &[1, 2, 3]);
    }
}

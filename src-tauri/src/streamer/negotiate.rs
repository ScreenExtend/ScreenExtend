//! What the client's offer says it can decode, and what to answer with.
//!
//! The host used to send Baseline to everything: `Config::default()` set
//! `H264Profile::Baseline`, nothing outside the developer probe harness ever wrote the field, and
//! the offer's `profile-level-id` was logged and thrown away. Baseline costs CAVLC instead of
//! CABAC and no 8x8 transform — 5-15 % of the bitrate at equal quality, and screen content with
//! flat regions and sharp text is where the 8x8 transform helps most. On a constrained link,
//! bytes are queueing delay, so this is a latency question and not only a quality one.
//!
//! Two things are decided here and nowhere else: which H.264 profile the encoder is configured
//! for, and what level the answer advertises.

use super::config::H264Profile;

/// H.264 level 3.1 — the level every shipped answer carried regardless of what was being sent.
/// It is the floor, never the ceiling.
pub const LEVEL_3_1: u8 = 0x1f;

/// Annex A limits, ordered. `(level_idc, MaxMBPS, MaxFS)`.
const LEVELS: &[(u8, u64, u64)] = &[
    (0x1e, 40_500, 1_620),       // 3.0
    (LEVEL_3_1, 108_000, 3_600), // 3.1
    (0x20, 216_000, 5_120),      // 3.2
    (0x28, 245_760, 8_192),      // 4.0
    (0x29, 245_760, 8_192),      // 4.1
    (0x2a, 522_240, 8_704),      // 4.2
    (0x32, 589_824, 22_080),     // 5.0
    (0x33, 983_040, 36_864),     // 5.1
    (0x34, 2_073_600, 36_864),   // 5.2
    (0x3c, 4_177_920, 139_264),  // 6.0
    (0x3d, 8_355_840, 139_264),  // 6.1
    (0x3e, 16_711_680, 139_264), // 6.2
];

/// What one WHEP offer says about the video it can receive.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfferedVideo {
    /// H.264 profiles the client offered, in the order they appeared.
    pub h264_profiles: Vec<H264Profile>,
    /// The offer carried `level-asymmetry-allowed=1`, which is RFC 6184's permission for the
    /// answerer to name a level of its own rather than echoing the offer's.
    pub level_asymmetry_allowed: bool,
    /// The offer carried an `H265` payload type. Chrome 136+ and Safari do; nothing else does.
    pub hevc: bool,
}

/// The profile the session will encode and answer with, and the level the answer will advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedVideo {
    pub profile: H264Profile,
    pub level_idc: u8,
    /// Whether the answer may carry `level_idc` rather than whatever the offer said.
    pub may_raise_level: bool,
}

impl Default for NegotiatedVideo {
    fn default() -> Self {
        Self {
            profile: H264Profile::Baseline,
            level_idc: LEVEL_3_1,
            may_raise_level: false,
        }
    }
}

/// Profile from the first byte of a `profile-level-id`, per RFC 6184 Table 5.
///
/// Only the profiles the encoders can actually produce are recognised; anything else (extended,
/// the High 10/422/444 family) maps to `None` so it is neither offered nor encoded.
fn profile_from_idc(profile_idc: u8, profile_iop: u8) -> Option<H264Profile> {
    match profile_idc {
        0x42 | 0x4d if profile_iop & 0x40 != 0 => {
            // constraint_set1_flag: Main-compatible-with-Baseline, i.e. constrained baseline.
            Some(H264Profile::Baseline)
        }
        0x42 => Some(H264Profile::Baseline),
        0x4d => Some(H264Profile::Main),
        0x58 => Some(H264Profile::Baseline), // extended, decodable as baseline
        0x64 => Some(H264Profile::High),
        _ => None,
    }
}

fn parse_plid(s: &str) -> Option<(u8, u8, u8)> {
    let b = s.as_bytes();
    if b.len() < 6 {
        return None;
    }
    let hex = |i: usize| -> Option<u8> {
        let hi = (b[i] as char).to_digit(16)? as u8;
        let lo = (b[i + 1] as char).to_digit(16)? as u8;
        Some((hi << 4) | lo)
    };
    Some((hex(0)?, hex(2)?, hex(4)?))
}

/// Read the offer for everything the codec choice depends on.
///
/// Deliberately a string scan rather than a full SDP parse: this runs before
/// `set_remote_description`, on the raw body, and it must not be able to fail the join.
pub fn read_offer(sdp: &str) -> OfferedVideo {
    let mut out = OfferedVideo::default();
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let upper = rest.to_ascii_uppercase();
            if upper.contains("H265") || upper.contains("HEVC") {
                out.hevc = true;
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("a=fmtp:") else {
            continue;
        };
        if rest.contains("level-asymmetry-allowed=1") {
            out.level_asymmetry_allowed = true;
        }
        // Packetization mode 0 (single NAL per packet) cannot carry a frame larger than the MTU,
        // which every real frame is. A payload type that only offers it is not usable.
        if !rest.contains("packetization-mode=1") {
            continue;
        }
        let Some(idx) = rest.find("profile-level-id=") else {
            continue;
        };
        let value = &rest[idx + "profile-level-id=".len()..];
        let Some((idc, iop, _level)) = parse_plid(value) else {
            continue;
        };
        if let Some(p) = profile_from_idc(idc, iop) {
            if !out.h264_profiles.contains(&p) {
                out.h264_profiles.push(p);
            }
        }
    }
    out
}

/// The lowest H.264 level that can legally carry this geometry, floored at 3.1.
pub fn level_for(width: u32, height: u32, fps: u32) -> u8 {
    let mbs = (width.div_ceil(16) as u64) * (height.div_ceil(16) as u64);
    let mbps = mbs * (fps.max(1) as u64);
    for (idc, max_mbps, max_fs) in LEVELS {
        if *max_fs >= mbs && *max_mbps >= mbps {
            return (*idc).max(LEVEL_3_1);
        }
    }
    // Past 6.2 there is nothing to name; say the highest that exists rather than lying downwards.
    LEVELS[LEVELS.len() - 1].0
}

/// Pick the best profile both ends can do.
///
/// High beats Main beats Baseline, and Baseline is the floor for a client that offered nothing
/// recognisable — old Android and smart-TV decoders that only do constrained baseline are exactly
/// why the floor exists.
pub fn choose(offer: &OfferedVideo, width: u32, height: u32, fps: u32) -> NegotiatedVideo {
    let profile = if offer.h264_profiles.contains(&H264Profile::High) {
        H264Profile::High
    } else if offer.h264_profiles.contains(&H264Profile::Main) {
        H264Profile::Main
    } else {
        H264Profile::Baseline
    };
    NegotiatedVideo {
        profile,
        level_idc: level_for(width, height, fps),
        may_raise_level: offer.level_asymmetry_allowed,
    }
}

/// Rewrite the level byte of every H.264 `profile-level-id` in an answer.
///
/// webrtc-rs answers with the *remote* codec parameters — `update_from_remote_description` pushes
/// the offer's `RTCRtpCodecParameters` into `negotiated_video_codecs` and `get_codecs_by_kind`
/// hands those straight back — so the level we register in the media engine never reaches the
/// wire. Chrome offers level 3.1 unconditionally, which is 1280x720@30; a 1440p or 4K session
/// therefore advertised a decoder ceiling far below what both ends were actually doing, and the
/// client's own WebCodecs candidate list starts at `avc1.640034` (High, level 5.2).
///
/// RFC 6184 §8.2.2 makes the level the one part of `profile-level-id` that need not be symmetric,
/// and `level-asymmetry-allowed=1` is the peer's explicit statement that it will accept a
/// different one — so this only runs when the offer said so. The profile bytes are never touched:
/// those *must* be symmetric, and webrtc-rs's own fmtp matcher compares exactly those two bytes.
pub fn apply_level_to_answer(answer: &str, level_idc: u8) -> String {
    const KEY: &str = "profile-level-id=";
    let mut out = String::with_capacity(answer.len());
    let mut rest = answer;
    while let Some(i) = rest.find(KEY) {
        let (head, tail) = rest.split_at(i + KEY.len());
        out.push_str(head);
        if tail.len() >= 6 && parse_plid(tail).is_some() {
            out.push_str(&tail[..4]);
            out.push_str(&format!("{level_idc:02x}"));
            rest = &tail[6..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME_VIDEO: &str = "\
m=video 9 UDP/TLS/RTP/SAVPF 96 97 102 103 104 105\r
a=rtpmap:102 H264/90000\r
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r
a=rtpmap:104 H264/90000\r
a=fmtp:104 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f\r
a=rtpmap:106 H264/90000\r
a=fmtp:106 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f\r
a=rtpmap:108 H264/90000\r
a=fmtp:108 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=640c1f\r
";

    #[test]
    fn a_modern_browser_offer_yields_high() {
        let offer = read_offer(CHROME_VIDEO);
        assert!(offer.h264_profiles.contains(&H264Profile::High));
        assert!(offer.h264_profiles.contains(&H264Profile::Main));
        assert!(offer.level_asymmetry_allowed);
        assert!(!offer.hevc);
        assert_eq!(
            choose(&offer, 1280, 720, 60).profile,
            H264Profile::High,
            "High must win when the client offered it"
        );
    }

    #[test]
    fn a_baseline_only_client_still_gets_baseline() {
        let sdp = "a=rtpmap:102 H264/90000\r\n\
                   a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";
        let offer = read_offer(sdp);
        assert_eq!(offer.h264_profiles, vec![H264Profile::Baseline]);
        assert_eq!(choose(&offer, 1280, 720, 60).profile, H264Profile::Baseline);
    }

    #[test]
    fn an_offer_with_nothing_recognisable_falls_to_the_floor() {
        let offer =
            read_offer("v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 VP8/90000\r\n");
        assert!(offer.h264_profiles.is_empty());
        let n = choose(&offer, 3840, 2160, 60);
        assert_eq!(n.profile, H264Profile::Baseline);
        assert!(!n.may_raise_level);
    }

    #[test]
    fn packetization_mode_zero_is_not_usable() {
        // Mode 0 cannot fragment, so a payload type that only offers it must not raise the
        // profile — the 108 line above is High but mode 0.
        let sdp =
            "a=fmtp:108 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=640c1f\r\n";
        assert!(read_offer(sdp).h264_profiles.is_empty());
    }

    #[test]
    fn the_level_tracks_the_geometry() {
        assert_eq!(level_for(1280, 720, 30), LEVEL_3_1);
        assert_eq!(level_for(1280, 720, 60), 0x20); // 3.2
        assert_eq!(level_for(1920, 1080, 60), 0x2a); // 4.2
        assert_eq!(level_for(2560, 1440, 60), 0x33); // 5.1
        assert_eq!(level_for(3840, 2160, 60), 0x34); // 5.2
        assert_eq!(level_for(3840, 2160, 120), 0x3c); // 6.0
    }

    #[test]
    fn the_level_never_goes_below_the_shipped_floor() {
        assert_eq!(level_for(320, 240, 15), LEVEL_3_1);
    }

    #[test]
    fn the_answer_level_is_rewritten_without_touching_the_profile() {
        let answer = "a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
                      a=fmtp:106 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f\r\n";
        let out = apply_level_to_answer(answer, 0x34);
        assert!(out.contains("profile-level-id=420034"));
        assert!(out.contains("profile-level-id=640c34"));
        assert!(!out.contains("1f\r"));
    }

    #[test]
    fn a_malformed_profile_level_id_is_left_alone() {
        let answer = "a=fmtp:102 profile-level-id=zz\r\n";
        assert_eq!(apply_level_to_answer(answer, 0x34), answer);
    }

    #[test]
    fn an_hevc_payload_type_is_noticed() {
        let sdp = "a=rtpmap:45 H265/90000\r\n";
        assert!(read_offer(sdp).hevc);
    }
}

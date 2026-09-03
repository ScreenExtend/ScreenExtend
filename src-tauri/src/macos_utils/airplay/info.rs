//! The `GET /info` response — where the geometry we want is advertised.
//!
//! The sender fetches this twice. The first time it carries a binary-plist
//! qualifier asking only for the Bonjour TXT blob; the second is bare and must
//! return the full device description, including a one-element `displays` array
//! that is the receiver's entire say in what the resulting display looks like.
//!
//! Note `refreshRate` is a frame *interval* in seconds, not a rate in Hz —
//! 1/60 = 0.01666… That inversion is not a mistake, it is what the protocol
//! wants, and getting it backwards asks for a 0.016 Hz display.

use plist::{Dictionary, Value};

use super::dnssd::{Identity, MODEL, SOURCE_VERSION};

/// Geometry we ask macOS for.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// Cap on the frame rate the sender will push at us. We discard every frame,
    /// so we ask for as few as the protocol allows — this is pure saved CPU and
    /// bandwidth on the host, and it is what UxPlay does when it renders nothing.
    pub max_fps: u32,
}

impl Geometry {
    pub fn new(width: u32, height: u32, refresh_hz: u32) -> Self {
        Self {
            width: width.max(2),
            height: height.max(2),
            refresh_hz: if refresh_hz == 0 { 60 } else { refresh_hz },
            max_fps: DISCARD_FPS,
        }
    }
}

/// The sender never renders anything for us, so one frame a second is plenty to
/// keep the stream alive.
const DISCARD_FPS: u32 = 1;

/// `displays[0].features`. Bits 1|2|3 in the same namespace as the TXT feature
/// word; not a display-mode selector — there is no such thing on the wire.
const DISPLAY_FEATURES: u64 = 14;

/// `statusFlags` bits 2 and 6: audio cable attached, supports AirPlay from cloud.
const STATUS_FLAGS: u64 = 68;

fn data(v: Vec<u8>) -> Value {
    Value::Data(v)
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (bytes[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}

/// Response to the qualifier form of `GET /info`, which asks only for the
/// Bonjour TXT blob back over RTSP.
pub fn txt_only(txt_airplay: &[u8]) -> Vec<u8> {
    let mut root = Dictionary::new();
    root.insert("txtAirPlay".into(), data(txt_airplay.to_vec()));
    encode(Value::Dictionary(root))
}

/// The full device description.
pub fn full(name: &str, id: &Identity, geom: Geometry) -> Vec<u8> {
    let mut root = Dictionary::new();

    root.insert("deviceID".into(), Value::String(id.device_id.clone()));
    root.insert("macAddress".into(), Value::String(id.device_id.clone()));
    root.insert("pk".into(), data(hex_to_bytes(&id.public_key)));
    root.insert("pi".into(), Value::String(id.pairing_id.clone()));
    root.insert("name".into(), Value::String(name.to_string()));
    root.insert("model".into(), Value::String(MODEL.to_string()));
    root.insert(
        "sourceVersion".into(),
        Value::String(SOURCE_VERSION.to_string()),
    );
    root.insert("vv".into(), Value::Integer(2.into()));
    root.insert("statusFlags".into(), Value::Integer(STATUS_FLAGS.into()));
    root.insert("keepAliveLowPower".into(), Value::Integer(1.into()));
    root.insert("keepAliveSendStatsAsBody".into(), Value::Boolean(true));
    root.insert(
        "features".into(),
        Value::Integer(super::dnssd::features_word(false).into()),
    );

    // We decline audio: no formats, no latencies. The mirroring session sets up
    // a type-96 audio stream alongside the video one, and accepting it makes
    // macOS route system audio at us — which would silence the host and defeat
    // ScreenExtend's own audio capture. Advertising no formats is the polite way
    // to say no.
    root.insert("audioFormats".into(), Value::Array(vec![]));
    root.insert("audioLatencies".into(), Value::Array(vec![]));

    let mut display = Dictionary::new();
    display.insert("uuid".into(), Value::String(id.pairing_id.clone()));
    display.insert("widthPhysical".into(), Value::Integer(0.into()));
    display.insert("heightPhysical".into(), Value::Integer(0.into()));
    display.insert("width".into(), Value::Integer(geom.width.into()));
    display.insert("height".into(), Value::Integer(geom.height.into()));
    display.insert("widthPixels".into(), Value::Integer(geom.width.into()));
    display.insert("heightPixels".into(), Value::Integer(geom.height.into()));
    display.insert("rotation".into(), Value::Boolean(false));
    display.insert(
        "refreshRate".into(),
        Value::Real(1.0 / geom.refresh_hz as f64),
    );
    display.insert("maxFPS".into(), Value::Integer(geom.max_fps.into()));
    display.insert("overscanned".into(), Value::Boolean(false));
    display.insert("features".into(), Value::Integer(DISPLAY_FEATURES.into()));

    root.insert(
        "displays".into(),
        Value::Array(vec![Value::Dictionary(display)]),
    );

    encode(Value::Dictionary(root))
}

/// SETUP response for the keys/timing phase.
pub fn setup_timing(timing_port: u16) -> Vec<u8> {
    let mut root = Dictionary::new();
    root.insert("timingPort".into(), Value::Integer(timing_port.into()));
    root.insert("eventPort".into(), Value::Integer(0.into()));
    encode(Value::Dictionary(root))
}

/// SETUP response for the streams phase.
///
/// `type: 110` is the mirroring stream and gets our real data port. `type: 96`
/// is audio; refusing an advertised stream type outright is what the reference
/// receivers treat as a disconnect condition, so we answer it with ports that
/// are bound and drained rather than rejecting it.
pub fn setup_streams(entries: &[StreamReply]) -> Vec<u8> {
    let mut streams = Vec::new();
    for e in entries {
        let mut d = Dictionary::new();
        d.insert("type".into(), Value::Integer(u64::from(e.kind).into()));
        d.insert("dataPort".into(), Value::Integer(e.data_port.into()));
        if let Some(cp) = e.control_port {
            d.insert("controlPort".into(), Value::Integer(cp.into()));
        }
        streams.push(Value::Dictionary(d));
    }
    let mut root = Dictionary::new();
    root.insert("streams".into(), Value::Array(streams));
    encode(Value::Dictionary(root))
}

#[derive(Clone, Copy, Debug)]
pub struct StreamReply {
    pub kind: u16,
    pub data_port: u16,
    pub control_port: Option<u16>,
}

pub const STREAM_MIRROR: u16 = 110;
pub const STREAM_AUDIO: u16 = 96;

fn encode(v: Value) -> Vec<u8> {
    let mut out = Vec::new();
    // The only failure mode is an unrepresentable value, and every value here is
    // a literal we constructed.
    if let Err(e) = v.to_writer_binary(&mut out) {
        teprintln!("[airplay] failed to encode a binary plist: {e}");
    }
    out
}

/// Parses the `streams` array out of a SETUP request body, returning the stream
/// types the sender asked for.
pub fn requested_stream_types(body: &[u8]) -> Vec<u16> {
    let Ok(v) = Value::from_reader(std::io::Cursor::new(body)) else {
        return Vec::new();
    };
    let Some(dict) = v.as_dictionary() else {
        return Vec::new();
    };
    let Some(streams) = dict.get("streams").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    streams
        .iter()
        .filter_map(|s| s.as_dictionary()?.get("type")?.as_unsigned_integer())
        .map(|t| t as u16)
        .collect()
}

/// True when the SETUP body is the first phase (carries the encrypted AES key
/// and the sender's timing port) rather than the streams phase.
pub fn is_timing_setup(body: &[u8]) -> bool {
    let Ok(v) = Value::from_reader(std::io::Cursor::new(body)) else {
        return false;
    };
    v.as_dictionary()
        .is_some_and(|d| d.contains_key("ekey") || d.contains_key("eiv"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident() -> Identity {
        Identity::derive()
    }

    fn parse(bytes: &[u8]) -> Value {
        Value::from_reader(std::io::Cursor::new(bytes)).expect("valid bplist")
    }

    #[test]
    fn info_is_a_binary_plist() {
        let out = full("ScreenExtend", &ident(), Geometry::new(1920, 1080, 60));
        assert_eq!(&out[..8], b"bplist00");
    }

    #[test]
    fn geometry_lands_in_the_displays_array() {
        let out = full("ScreenExtend", &ident(), Geometry::new(1600, 900, 60));
        let v = parse(&out);
        let displays = v
            .as_dictionary()
            .unwrap()
            .get("displays")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(displays.len(), 1, "exactly one advertised display");
        let d = displays[0].as_dictionary().unwrap();
        assert_eq!(
            d.get("widthPixels").unwrap().as_unsigned_integer(),
            Some(1600)
        );
        assert_eq!(
            d.get("heightPixels").unwrap().as_unsigned_integer(),
            Some(900)
        );
        assert_eq!(d.get("width").unwrap().as_unsigned_integer(), Some(1600));
        assert_eq!(d.get("height").unwrap().as_unsigned_integer(), Some(900));
    }

    #[test]
    fn refresh_rate_is_published_as_an_interval_not_a_rate() {
        let out = full("ScreenExtend", &ident(), Geometry::new(1920, 1080, 60));
        let v = parse(&out);
        let d = v.as_dictionary().unwrap()["displays"].as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        let r = d.get("refreshRate").unwrap().as_real().unwrap();
        assert!((r - 1.0 / 60.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn zero_refresh_falls_back_to_60() {
        assert_eq!(Geometry::new(800, 600, 0).refresh_hz, 60);
    }

    #[test]
    fn we_advertise_no_audio_formats() {
        let out = full("ScreenExtend", &ident(), Geometry::new(1920, 1080, 60));
        let v = parse(&out);
        let d = v.as_dictionary().unwrap();
        assert!(d["audioFormats"].as_array().unwrap().is_empty());
        assert!(d["audioLatencies"].as_array().unwrap().is_empty());
    }

    #[test]
    fn setup_replies_round_trip() {
        let out = setup_streams(&[
            StreamReply {
                kind: STREAM_MIRROR,
                data_port: 5001,
                control_port: None,
            },
            StreamReply {
                kind: STREAM_AUDIO,
                data_port: 5002,
                control_port: Some(5003),
            },
        ]);
        let v = parse(&out);
        let streams = v.as_dictionary().unwrap()["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(
            streams[0].as_dictionary().unwrap()["type"].as_unsigned_integer(),
            Some(110)
        );
        assert_eq!(
            streams[0].as_dictionary().unwrap()["dataPort"].as_unsigned_integer(),
            Some(5001)
        );
        assert!(streams[1]
            .as_dictionary()
            .unwrap()
            .contains_key("controlPort"));
    }

    #[test]
    fn stream_types_are_read_back_out_of_a_setup_body() {
        let body = setup_streams(&[StreamReply {
            kind: STREAM_MIRROR,
            data_port: 1,
            control_port: None,
        }]);
        assert_eq!(requested_stream_types(&body), vec![110]);
        assert!(!is_timing_setup(&body));
    }

    #[test]
    fn a_timing_setup_is_recognised() {
        let mut d = Dictionary::new();
        d.insert("ekey".into(), Value::Data(vec![0u8; 72]));
        d.insert("eiv".into(), Value::Data(vec![0u8; 16]));
        d.insert("timingPort".into(), Value::Integer(6002.into()));
        let body = encode(Value::Dictionary(d));
        assert!(is_timing_setup(&body));
        assert!(requested_stream_types(&body).is_empty());
    }

    #[test]
    fn garbage_bodies_do_not_panic() {
        assert!(requested_stream_types(b"not a plist").is_empty());
        assert!(!is_timing_setup(b""));
    }

    #[test]
    fn hex_decoding_is_exact() {
        assert_eq!(hex_to_bytes("00ff10"), vec![0x00, 0xff, 0x10]);
        assert_eq!(hex_to_bytes(&"ab".repeat(32)).len(), 32);
    }
}

//! The fake AirPlay receiver: one control socket, one mirroring socket, and
//! just enough protocol to make macOS decide we are a display.
//!
//! What it answers, and nothing more:
//!
//! | request | answer |
//! |---|---|
//! | `GET /info` (qualifier) | our Bonjour TXT blob |
//! | `GET /info` (bare) | the device description, carrying the geometry we want |
//! | `POST /fp-setup` | the static FairPlay table (see [`super::fairplay`]) |
//! | `POST /pair-setup` | 32 bytes of stable public key |
//! | `SETUP` (keys/timing) | our timing port, event port 0 |
//! | `SETUP` (streams) | a real data port per requested stream |
//! | `RECORD` / `FLUSH` / `SET_PARAMETER` / `POST /feedback` | bare 200 |
//! | `GET_PARAMETER` | `volume: 0.0` |
//! | `TEARDOWN` | 200, then the session ends |
//!
//! There is no decryption anywhere in this file. `ekey`/`eiv` from the first
//! SETUP and `streamConnectionID` from the second are the inputs to AES-CTR
//! video decryption, and we parse neither.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};

use super::dnssd::{Advertisement, Identity};
use super::info::{self, Geometry, StreamReply, STREAM_AUDIO, STREAM_MIRROR};
use super::mirror::{MirrorSink, MirrorStats, TimingSocket};
use super::rtsp::{self, Proto, ReadOutcome, Request, Response};
use super::{fairplay, Cancel};

/// Legacy pairing (features bit 27). Off matches the reference receiver's
/// shipped default: with the bit clear a sender skips `/pair-setup` and
/// `/pair-verify` entirely, which saves both a round trip and the ed25519 /
/// x25519 / AES-CTR implementation we would otherwise need. If a sender turns
/// out to insist on pairing, flipping this to `true` is the first thing to try —
/// and then `/pair-verify` has to be implemented for real.
const LEGACY_PAIRING: bool = false;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Advertised, nobody has spoken to us.
    Advertised,
    /// A sender has fetched `/info`.
    Probed,
    /// Streams are set up; macOS should be creating the display.
    Recording,
    /// The sender said goodbye, or the connection dropped.
    Ended,
}

struct State {
    phase: Phase,
    last_contact: Instant,
    /// Set the first time a sender touches us, for diagnostics.
    peer: Option<String>,
}

pub struct Receiver {
    inner: Arc<Inner>,
    cancel: Cancel,
    /// Withdrawn on drop.
    _advertisement: Advertisement,
}

struct Inner {
    name: String,
    identity: Identity,
    geometry: Geometry,
    txt_blob: Vec<u8>,
    mirror_port: u16,
    timing: TimingSocket,
    audio_data_port: u16,
    audio_control_port: u16,
    // Bound so the ports we name in a type-96 SETUP reply are real. Never read:
    // we decline audio, but naming an unbound port earns ICMP unreachables.
    _audio_data: TimingSocket,
    _audio_control: TimingSocket,
    state: Mutex<State>,
    stats: Arc<MirrorStats>,
    requests: AtomicU64,
    saw_teardown: AtomicBool,
}

impl Receiver {
    /// Binds every socket, publishes both Bonjour services, and starts serving.
    pub async fn start(name: &str, geometry: Geometry) -> Result<Self, String> {
        let identity = Identity::derive();

        let control = TcpListener::bind(("0.0.0.0", 0))
            .await
            .map_err(|e| format!("could not bind the AirPlay control port: {e}"))?;
        let control_port = control
            .local_addr()
            .map_err(|e| format!("could not read the AirPlay control port: {e}"))?
            .port();

        let mirror = MirrorSink::bind().await?;
        let mirror_port = mirror.port();
        let timing = TimingSocket::bind()?;

        // Bound so the ports we name in a type-96 SETUP reply are real; never read.
        let audio_data = TimingSocket::bind()?;
        let audio_control = TimingSocket::bind()?;

        let stats = Arc::new(MirrorStats::default());
        let inner = Arc::new(Inner {
            name: name.to_string(),
            txt_blob: super::dnssd::airplay_txt_blob(&identity, LEGACY_PAIRING),
            identity,
            geometry,
            mirror_port,
            timing,
            audio_data_port: audio_data.port,
            audio_control_port: audio_control.port,
            _audio_data: audio_data,
            _audio_control: audio_control,
            state: Mutex::new(State {
                phase: Phase::Advertised,
                last_contact: Instant::now(),
                peer: None,
            }),
            stats: stats.clone(),
            requests: AtomicU64::new(0),
            saw_teardown: AtomicBool::new(false),
        });

        let advertisement =
            Advertisement::publish(name, control_port, &inner.identity, LEGACY_PAIRING)?;

        let cancel = Cancel::new();

        {
            let inner = inner.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { serve_control(control, inner, cancel).await });
        }
        {
            let cancel = cancel.clone();
            tokio::spawn(async move { mirror.run(stats, cancel).await });
        }

        tprintln!(
            "[airplay] receiver {name:?} up: control {control_port}, mirror {mirror_port}, \
             timing {}, advertising {}x{}@{}",
            inner.timing.port,
            geometry.width,
            geometry.height,
            geometry.refresh_hz,
        );

        Ok(Self {
            inner,
            cancel,
            _advertisement: advertisement,
        })
    }

    pub fn phase(&self) -> Phase {
        self.inner.state.lock().unwrap().phase
    }

    pub fn peer(&self) -> Option<String> {
        self.inner.state.lock().unwrap().peer.clone()
    }

    pub fn requests(&self) -> u64 {
        self.inner.requests.load(Ordering::Relaxed)
    }

    pub fn mirror_connected(&self) -> bool {
        self.inner.stats.connected.load(Ordering::Relaxed)
    }

    pub fn geometry_changed(&self) -> bool {
        self.inner.stats.geometry_changed.load(Ordering::Relaxed)
    }

    /// Seconds since the sender last said anything.
    pub fn idle_for(&self) -> Duration {
        self.inner.state.lock().unwrap().last_contact.elapsed()
    }

    /// Ends the session. Closing the control socket is the only receiver-side
    /// termination the protocol has — `TEARDOWN` is a sender-to-receiver
    /// request, not something we may originate.
    pub fn stop(&self) {
        self.cancel.cancel();
        self.inner.state.lock().unwrap().phase = Phase::Ended;
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop();
        tprintln!(
            "[airplay] receiver {:?} down after {} requests ({} mirror packets)",
            self.inner.name,
            self.inner.requests.load(Ordering::Relaxed),
            self.inner.stats.packets.load(Ordering::Relaxed),
        );
    }
}

async fn serve_control(listener: TcpListener, inner: Arc<Inner>, cancel: Cancel) {
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = cancel.cancelled() => return,
        };
        let (stream, peer) = match accepted {
            Ok(v) => v,
            Err(e) => {
                teprintln!("[airplay] control accept failed: {e}");
                return;
            }
        };
        tprintln!("[airplay] control connection from {peer}");
        {
            let mut st = inner.state.lock().unwrap();
            st.peer = Some(peer.to_string());
            st.last_contact = Instant::now();
        }

        let inner = inner.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                r = serve_connection(stream, inner.clone()) => {
                    if let Err(e) = r {
                        teprintln!("[airplay] control connection ended: {e}");
                    }
                    let mut st = inner.state.lock().unwrap();
                    if st.phase != Phase::Ended {
                        st.phase = Phase::Ended;
                    }
                }
                _ = cancel.cancelled() => {}
            }
        });
    }
}

async fn serve_connection(mut stream: TcpStream, inner: Arc<Inner>) -> Result<(), String> {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    loop {
        let read = tokio::time::timeout(
            rtsp::IDLE_TIMEOUT,
            rtsp::read_request(&mut stream, &mut buf),
        )
        .await
        .map_err(|_| format!("sender went quiet for {}s", rtsp::IDLE_TIMEOUT.as_secs()))??;

        let request = match read {
            ReadOutcome::Request(r) => r,
            ReadOutcome::Eof => return Ok(()),
        };

        inner.requests.fetch_add(1, Ordering::Relaxed);
        inner.state.lock().unwrap().last_contact = Instant::now();

        let cseq = request.header("cseq").map(|s| s.to_string());
        let proto = request.proto;
        let response = handle(&request, &inner);
        let close = response.close;

        rtsp::write_response(&mut stream, proto, cseq.as_deref(), &response).await?;

        if close {
            return Ok(());
        }
    }
}

fn handle(req: &Request, inner: &Inner) -> Response {
    // The HTTP/1.1 half of the socket belongs to AirPlay video/HLS, which we do
    // not offer. Answering 404 rather than 501 keeps an over-eager sender from
    // treating us as broken.
    if req.proto == Proto::Http {
        return Response::status(404, "Not Found");
    }

    match (req.method.as_str(), req.url.as_str()) {
        ("GET", url) if url.starts_with("/info") => handle_info(req, inner),

        ("POST", "/fp-setup") => handle_fp_setup(req),

        ("POST", "/pair-setup") => {
            // The reference receivers return their public key with no
            // validation of the request whatsoever.
            let pk = hex_bytes(&inner.identity.public_key);
            Response::ok().body("application/octet-stream", pk)
        }

        ("POST", "/pair-verify") => {
            teprintln!(
                "[airplay] the sender requested /pair-verify, which this receiver does not \
                 implement (legacy pairing is advertised as off). The session will not \
                 proceed — see docs/airplay-fallback-design.md."
            );
            Response::status(403, "Forbidden")
        }

        ("SETUP", _) => handle_setup(req, inner),

        ("RECORD", _) => {
            inner.state.lock().unwrap().phase = Phase::Recording;
            Response::ok()
                .header("Audio-Latency", "0")
                .header("Audio-Jack-Status", "connected; type=analog")
        }

        ("GET_PARAMETER", _) => {
            if req.content_type().is_none() {
                return Response::status(451, "Parameter Not Understood");
            }
            Response::ok().body("text/parameters", b"volume: 0.0\r\n".to_vec())
        }

        ("SET_PARAMETER", _) => {
            if req.content_type().is_none() {
                return Response::status(451, "Parameter Not Understood");
            }
            Response::ok()
        }

        ("OPTIONS", _) => Response::ok().header(
            "Public",
            "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, \
             SET_PARAMETER, POST, GET",
        ),

        ("TEARDOWN", _) => {
            inner.saw_teardown.store(true, Ordering::Relaxed);
            let types = info::requested_stream_types(&req.body);
            // A bodiless TEARDOWN, or one naming the mirroring stream, ends the
            // session. One naming only audio does not.
            let ends = types.is_empty() || types.contains(&STREAM_MIRROR);
            if ends {
                inner.state.lock().unwrap().phase = Phase::Ended;
                tprintln!("[airplay] sender tore down the session");
                Response::ok().closing()
            } else {
                Response::ok()
            }
        }

        ("POST", "/feedback") => Response::ok(),

        ("POST", "/audioMode") | ("FLUSH", _) | ("POST", "/reverse") => Response::ok(),

        (method, url) => {
            tprintln!("[airplay] unhandled {method} {url} — answering 200");
            Response::ok()
        }
    }
}

fn handle_info(req: &Request, inner: &Inner) -> Response {
    {
        let mut st = inner.state.lock().unwrap();
        if st.phase == Phase::Advertised {
            st.phase = Phase::Probed;
        }
    }

    // The qualifier form asks only for the Bonjour TXT blob back.
    if req.is_binary_plist() && wants_txt_airplay(&req.body) {
        return Response::ok().body(
            "application/x-apple-binary-plist",
            info::txt_only(&inner.txt_blob),
        );
    }

    Response::ok().body(
        "application/x-apple-binary-plist",
        info::full(&inner.name, &inner.identity, inner.geometry),
    )
}

fn wants_txt_airplay(body: &[u8]) -> bool {
    let Ok(v) = plist::Value::from_reader(std::io::Cursor::new(body)) else {
        return false;
    };
    v.as_dictionary()
        .and_then(|d| d.get("qualifier"))
        .and_then(|q| q.as_array())
        .is_some_and(|a| {
            a.iter()
                .filter_map(|s| s.as_string())
                .any(|s| s == "txtAirPlay")
        })
}

fn handle_fp_setup(req: &Request) -> Response {
    match fairplay::respond(&req.body) {
        Ok(body) => Response::ok().body("application/octet-stream", body),
        Err(e) => {
            teprintln!(
                "[airplay] {e}; the sender sent {}",
                fairplay::describe(&req.body)
            );
            Response::status(400, "Bad Request")
        }
    }
}

fn handle_setup(req: &Request, inner: &Inner) -> Response {
    if info::is_timing_setup(&req.body) {
        // Phase one: the sender hands us the encrypted AES key and its own
        // timing port. We parse neither — the key is only ever used to decrypt
        // video, and nothing polls our timing port.
        return Response::ok().body(
            "application/x-apple-binary-plist",
            info::setup_timing(inner.timing.port),
        );
    }

    let types = info::requested_stream_types(&req.body);
    if types.is_empty() {
        // Some senders split the phases differently; answering with the timing
        // ports is the tolerant thing to do.
        return Response::ok().body(
            "application/x-apple-binary-plist",
            info::setup_timing(inner.timing.port),
        );
    }

    let mut replies = Vec::new();
    for kind in types {
        match kind {
            STREAM_MIRROR => replies.push(StreamReply {
                kind: STREAM_MIRROR,
                data_port: inner.mirror_port,
                control_port: None,
            }),
            STREAM_AUDIO => replies.push(StreamReply {
                kind: STREAM_AUDIO,
                data_port: inner.audio_data_port,
                control_port: Some(inner.audio_control_port),
            }),
            other => {
                // Refusing an advertised stream type is what the reference
                // receivers treat as a disconnect condition, so answer with a
                // bound-but-ignored port instead.
                tprintln!(
                    "[airplay] sender asked for unknown stream type {other}; answering anyway"
                );
                replies.push(StreamReply {
                    kind: other,
                    data_port: inner.audio_data_port,
                    control_port: Some(inner.audio_control_port),
                });
            }
        }
    }

    Response::ok().body(
        "application/x-apple-binary-plist",
        info::setup_streams(&replies),
    )
}

fn hex_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let hi = (b[i] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (b[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn inner() -> Inner {
        let identity = Identity::derive();
        Inner {
            name: "ScreenExtend Test".to_string(),
            txt_blob: super::super::dnssd::airplay_txt_blob(&identity, false),
            identity,
            geometry: Geometry::new(1920, 1080, 60),
            mirror_port: 5001,
            timing: TimingSocket::bind().unwrap(),
            audio_data_port: 5002,
            audio_control_port: 5003,
            _audio_data: TimingSocket::bind().unwrap(),
            _audio_control: TimingSocket::bind().unwrap(),
            state: Mutex::new(State {
                phase: Phase::Advertised,
                last_contact: Instant::now(),
                peer: None,
            }),
            stats: Arc::new(MirrorStats::default()),
            requests: AtomicU64::new(0),
            saw_teardown: AtomicBool::new(false),
        }
    }

    fn req(method: &str, url: &str) -> Request {
        Request {
            method: method.to_string(),
            url: url.to_string(),
            proto: Proto::Rtsp,
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    fn with_body(mut r: Request, ct: &str, body: Vec<u8>) -> Request {
        r.headers.insert("content-type".into(), ct.into());
        r.body = body;
        r
    }

    #[test]
    fn info_advertises_the_requested_geometry() {
        let i = inner();
        let r = handle(&req("GET", "/info"), &i);
        assert_eq!(r.status, 200);
        let v = plist::Value::from_reader(std::io::Cursor::new(&r.body[..])).unwrap();
        let d = v.as_dictionary().unwrap()["displays"].as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        assert_eq!(d["widthPixels"].as_unsigned_integer(), Some(1920));
        assert_eq!(i.state.lock().unwrap().phase, Phase::Probed);
    }

    #[test]
    fn the_qualifier_form_returns_only_the_txt_blob() {
        let i = inner();
        let mut q = plist::Dictionary::new();
        q.insert(
            "qualifier".into(),
            plist::Value::Array(vec![plist::Value::String("txtAirPlay".into())]),
        );
        let mut body = Vec::new();
        plist::Value::Dictionary(q)
            .to_writer_binary(&mut body)
            .unwrap();

        let r = handle(
            &with_body(
                req("GET", "/info"),
                "application/x-apple-binary-plist",
                body,
            ),
            &i,
        );
        let v = plist::Value::from_reader(std::io::Cursor::new(&r.body[..])).unwrap();
        let d = v.as_dictionary().unwrap();
        assert!(d.contains_key("txtAirPlay"));
        assert!(
            !d.contains_key("displays"),
            "the short form carries no display list"
        );
    }

    #[test]
    fn fp_setup_message_one_is_answered_from_the_table() {
        let i = inner();
        let msg = vec![
            0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x02, 0x00,
            0x02, 0x00,
        ];
        let r = handle(
            &with_body(req("POST", "/fp-setup"), "application/octet-stream", msg),
            &i,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body.len(), 142);
    }

    #[test]
    fn a_streams_setup_hands_back_the_real_mirror_port() {
        let i = inner();
        let body = info::setup_streams(&[StreamReply {
            kind: STREAM_MIRROR,
            data_port: 0,
            control_port: None,
        }]);
        let r = handle(
            &with_body(
                req("SETUP", "rtsp://x/1"),
                "application/x-apple-binary-plist",
                body,
            ),
            &i,
        );
        let v = plist::Value::from_reader(std::io::Cursor::new(&r.body[..])).unwrap();
        let s = v.as_dictionary().unwrap()["streams"].as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        assert_eq!(s["dataPort"].as_unsigned_integer(), Some(5001));
    }

    #[test]
    fn record_moves_the_session_to_recording() {
        let i = inner();
        assert_eq!(handle(&req("RECORD", "rtsp://x/1"), &i).status, 200);
        assert_eq!(i.state.lock().unwrap().phase, Phase::Recording);
    }

    #[test]
    fn feedback_is_a_bare_200() {
        let i = inner();
        let r = handle(&req("POST", "/feedback"), &i);
        assert_eq!(r.status, 200);
        assert!(r.body.is_empty());
        assert!(!r.close);
    }

    #[test]
    fn parameters_without_a_content_type_are_refused() {
        let i = inner();
        assert_eq!(handle(&req("GET_PARAMETER", "rtsp://x/1"), &i).status, 451);
        assert_eq!(handle(&req("SET_PARAMETER", "rtsp://x/1"), &i).status, 451);
    }

    #[test]
    fn get_parameter_answers_a_volume() {
        let i = inner();
        let r = handle(
            &with_body(
                req("GET_PARAMETER", "rtsp://x/1"),
                "text/parameters",
                b"volume\r\n".to_vec(),
            ),
            &i,
        );
        assert_eq!(r.body, b"volume: 0.0\r\n");
    }

    #[test]
    fn teardown_ends_the_session_and_closes() {
        let i = inner();
        let r = handle(&req("TEARDOWN", "rtsp://x/1"), &i);
        assert!(r.close);
        assert_eq!(i.state.lock().unwrap().phase, Phase::Ended);
        assert!(i.saw_teardown.load(Ordering::Relaxed));
    }

    #[test]
    fn an_audio_only_teardown_does_not_end_the_session() {
        let i = inner();
        i.state.lock().unwrap().phase = Phase::Recording;
        let body = info::setup_streams(&[StreamReply {
            kind: STREAM_AUDIO,
            data_port: 0,
            control_port: None,
        }]);
        let r = handle(
            &with_body(
                req("TEARDOWN", "rtsp://x/1"),
                "application/x-apple-binary-plist",
                body,
            ),
            &i,
        );
        assert!(!r.close);
        assert_eq!(i.state.lock().unwrap().phase, Phase::Recording);
    }

    #[test]
    fn http_requests_are_not_served() {
        let i = inner();
        let mut r = req("GET", "/server-info");
        r.proto = Proto::Http;
        assert_eq!(handle(&r, &i).status, 404);
    }

    #[test]
    fn unknown_verbs_get_200_rather_than_501() {
        let i = inner();
        assert_eq!(handle(&req("ANNOUNCE", "rtsp://x/1"), &i).status, 200);
    }
}

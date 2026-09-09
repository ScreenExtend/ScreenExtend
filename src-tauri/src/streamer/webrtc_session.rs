use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use tokio::sync::broadcast::error::RecvError;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp::sequence::{new_random_sequencer, Sequencer};
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::PayloadType;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

pub use webrtc::ice_transport::ice_server::RTCIceServer;

use super::audio::{self, protocol, SharedAudioHub};
use super::bitrate::{
    estimate_from_loss, BitrateController, RttCongestionMonitor, DEFAULT_MIN_BITRATE_BPS,
    RTT_DELAY_THRESHOLD_MS,
};
use super::config::H264Profile;
use super::congestion::{self, DelayBasedEstimate, NetworkState, SendHistory, SharedFeedback};
use super::input;
use super::keyframe::KeyframeGate;
use super::lock::LockExt;
use super::negotiate;
use super::pacer::{Pacer, PacingMode};
use super::packetizer::{access_unit_has_sps, payloads_carry_parameter_sets, H264Packetizer};
use super::pipeline::Pipeline;
use super::session::{self, SharedDeviceOverrides};
use super::sockets;

const BWE_POLL_INTERVAL: Duration = Duration::from_millis(120);

/// Floor on how often a client's out-of-band keyframe request may force an IDR.
const MIN_IDR_REQUEST_GAP: Duration = Duration::from_millis(200);

/// No `useinbandfec=1`: it used to be advertised and it was never true. libopus documents
/// in-band FEC as an LPC-layer (SILK) feature, and this encoder runs
/// `OPUS_APPLICATION_RESTRICTED_LOWDELAY` at 5 ms frames — both force CELT-only — so no FEC
/// can be produced on either transport path. Telling the client to expect it was a lie that
/// could only make its concealment worse.
const OPUS_FALLBACK_FMTP: &str = "minptime=5;stereo=1;sprop-stereo=1";

/// How much audio may sit unsent in the DataChannel before the forwarder starts dropping.
///
/// Expressed in milliseconds, not bytes. The old threshold was a flat 16 KiB, which at the
/// measured ~93-byte message is about **a second** of audio queued before the drop policy
/// engaged — by which point the listener is a second behind and every packet in the queue is
/// worthless. 40 ms is two Opus frames more than the client's own maximum jitter target.
const AUDIO_BACKPRESSURE_MS: usize = 40;

/// The byte figure that `AUDIO_BACKPRESSURE_MS` works out to for the message size actually
/// being sent, so the policy tracks the bitrate instead of assuming one.
fn backpressure_limit_bytes(msg_len: usize) -> usize {
    let frames = AUDIO_BACKPRESSURE_MS / crate::streamer::audio::encoder::FRAME_MS as usize;
    frames * msg_len.max(1)
}

/// RTP payload MTU on a same-subnet path: 1200 bytes of datagram minus the 12-byte RTP header.
const RTP_PAYLOAD_MTU_LAN: usize = 1200 - 12;

/// RTP payload MTU once the path goes through a relay. The datagram then carries TURN
/// ChannelData or Send-indication encapsulation on top of RTP, SRTP auth and the outer IP/UDP
/// headers, which can push it past a tunnelled path MTU (1400/1420 are common) or the 1280-byte
/// IPv6 minimum. Fragmentation turns graceful loss into catastrophic loss — one lost fragment
/// destroys the whole datagram — and a PMTU black hole loses everything. Moonlight caps remote
/// streams at 1024 for exactly this reason and Sunshine defaults to it.
const RTP_PAYLOAD_MTU_RELAY: usize = 1024 - 12;

/// How long to wait for ICE gathering before returning the answer anyway.
const GATHER_MAX_WAIT_MS: u64 = 1500;
/// The same wait for a peer already known to be on this subnet, which needs only host
/// candidates.
const GATHER_MAX_WAIT_LAN_MS: u64 = 200;

/// Playout-delay hint for a relayed path, in 10 ms units as the extension encodes it. Zero is
/// right for a same-subnet hop and wrong across a relay, where forbidding the receiver any
/// de-jitter buffer turns ordinary jitter into stutter.
const PLAYOUT_DELAY_MAX_RELAY: u16 = 20;

/// Live sessions, for the leak check. A session that ends must give its peer connection, its ICE
/// sockets and its injector thread back; before the explicit close these only ever went up.
static LIVE_SESSIONS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub fn live_sessions() -> i64 {
    LIVE_SESSIONS.load(Ordering::Relaxed)
}

/// The one path every keyframe request in a session takes.
///
/// There are five callers — an RTCP PLI, an RTCP FIR, this client opcode, a payloader failure
/// and a lagging viewer — and on an infinite-GOP encoder each one produces the largest frame the
/// session will ever emit. Funnelling them through a single rate-limited requester is what stops
/// a decoder's backlog from being answered five times over.
#[derive(Clone)]
pub struct KeyframeRequester {
    pipeline: Pipeline,
    gate: Arc<KeyframeGate>,
}

impl KeyframeRequester {
    /// Ask the encoder for an IDR. Returns whether the request was admitted; a suppressed one
    /// means an IDR is already in flight for the same event.
    pub fn request(&self, reason: &str) -> bool {
        if !self.gate.allow(audio::host_now_ns()) {
            return false;
        }
        tprintln!("keyframe requested: {reason}");
        self.pipeline.request_idr();
        true
    }

    /// The same, without a log line. For the paths that fire continuously on a lossy link,
    /// where every logbus line is a JSON IPC event to the desktop UI.
    pub fn request_quietly(&self) -> bool {
        if !self.gate.allow(audio::host_now_ns()) {
            return false;
        }
        self.pipeline.request_idr();
        true
    }
}

pub struct InputParams {
    pub device: Option<String>,
    /// The `os` string the client reported at join, which decides the modifier layout on a
    /// macOS host. Empty when the client did not say.
    pub client_os: String,
    pub control_enabled: bool,
    pub control_rx: Option<session::ControlReceiver>,
}

#[derive(Clone)]
pub struct AudioParams {
    pub fast: bool,
    /// Put a standard Opus track in the answer even on the fast path, so a decoder that
    /// answered "supported" and then failed on real packets has somewhere to go. Requested by
    /// the client, because only the client knows whether it can switch to it — see
    /// `AudioCapabilities::wants_fallback_track`.
    pub fallback_track: bool,
    /// The client said it can parse [`audio::FLAG_REDUNDANT`] messages. Old clients cannot, so
    /// this is off unless the join request opted in.
    pub redundancy: bool,
    /// Link loss as measured by the video BWE, in hundredths of a percent. Wired in by
    /// `start_session`; `None` outside a live session.
    pub link_loss: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Where this client is, decided from its IP before the handshake rather than measured
    /// 500 ms into the session. A relayed session is *known* to be relayed before a single
    /// packet is sent, which is the one thing the loss-driven redundancy policy cannot learn
    /// in time.
    pub locality: SessionLocality,
    pub hub: SharedAudioHub,
    pub overrides: SharedDeviceOverrides,
    pub device_key: String,
}

/// Loss at which duplicating the previous frame starts paying for itself.
///
/// Below this, redundancy is pure overhead: every packet doubles in payload to protect against
/// a loss that is not happening. Above it, a single lost packet is an audible 5 ms hole with
/// no codec-level concealment available (in-band FEC cannot exist in this configuration), so
/// the trade inverts. 0.5 % — the same order as the point at which the video BWE starts to
/// react.
const AUDIO_REDUNDANCY_LOSS_THRESHOLD: u32 = 50; // hundredths of a percent

impl AudioParams {
    /// Should this packet carry the previous frame alongside it?
    ///
    /// The client saying it can parse the format is absolute: one that cannot must never be
    /// sent it. Past that, the question is whether the link is losing packets — and the
    /// measured answer lags the loss it describes, because `link_loss` can only move after
    /// loss has already been observed and reported by the video BWE.
    ///
    /// A `CrossNetwork` session does not have to wait for that. It is relayed through a TURN
    /// server by construction, so it starts defensive and the loss-driven policy takes over
    /// from there; it never turns redundancy *off*, it only removes the opening window in
    /// which a relayed client's first packets are unprotected. `SameNetwork` and `Unknown`
    /// keep the old purely-reactive behaviour: a clean LAN should not pay for this.
    fn redundancy_now(&self) -> bool {
        if !self.redundancy {
            return false;
        }
        if self.locality == SessionLocality::CrossNetwork {
            return true;
        }
        self.link_loss
            .as_ref()
            .map(|l| l.load(Ordering::Relaxed) >= AUDIO_REDUNDANCY_LOSS_THRESHOLD)
            .unwrap_or(false)
    }

    fn audio_enabled_now(&self) -> bool {
        self.overrides
            .lock_recover()
            .get(&self.device_key)
            .map(|o| o.audio_enabled)
            .unwrap_or(false)
    }
}

fn h264_fmtp(profile_level_id: &str) -> String {
    format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}")
}

/// The `profile-level-id` this profile is registered and bound under.
///
/// The level byte is decorative here: webrtc-rs's fmtp matcher compares only the first two bytes
/// (`profile_level_id_matches`, `rtp_transceiver/fmtp/h264/mod.rs`), and the answer echoes the
/// offer's line anyway. The real level goes on with `negotiate::apply_level_to_answer`.
fn profile_plid(profile: H264Profile) -> &'static str {
    match profile {
        H264Profile::Baseline => "42e01f",
        H264Profile::Main => "4d001f",
        H264Profile::High => "640c1f",
    }
}

/// Every H.264 payload type to register, chosen profile first.
///
/// The chosen one leads so the track binds to it; Baseline follows as a floor whenever it is not
/// already the choice, because a client that turns out to offer nothing else must still get a
/// picture rather than a silent codec mismatch.
fn h264_codecs(profile: H264Profile) -> Vec<(PayloadType, PayloadType, &'static str)> {
    let mut out = vec![(102, 103, profile_plid(profile))];
    match profile {
        H264Profile::Baseline => {}
        H264Profile::Main => out.push((108, 109, profile_plid(H264Profile::Baseline))),
        H264Profile::High => {
            out.push((104, 105, "64001f"));
            out.push((108, 109, profile_plid(H264Profile::Baseline)));
        }
    }
    out
}

/// The media socket a session muxes ICE over, when it is on the LAN tier.
///
/// Kept whole so the session can close it: `UDPMuxDefault` spawns a read worker that lives until
/// `close()`, and the raw handle is what qWAVE needs once the candidate pair is nominated.
struct MediaSocket {
    mux: Arc<webrtc::ice::udp_mux::UDPMuxDefault>,
    #[cfg(windows)]
    raw: usize,
    #[cfg(not(windows))]
    raw: i32,
    tuning: sockets::SocketTuning,
    local: std::net::SocketAddr,
}

/// Bind and configure the LAN-tier media socket, or return `None` and let the session use the
/// stock ephemeral sockets.
fn lan_media_socket() -> Option<MediaSocket> {
    let (sock, tuning) = match sockets::bind_tuned("0.0.0.0:0".parse().ok()?) {
        Ok(v) => v,
        Err(e) => {
            tprintln!("media socket: could not bind a tuned UDP socket ({e}); using ICE defaults");
            return None;
        }
    };
    let local = sock.local_addr().ok()?;
    #[cfg(windows)]
    let raw = {
        use std::os::windows::io::AsRawSocket;
        sock.as_raw_socket() as usize
    };
    #[cfg(not(windows))]
    let raw = {
        use std::os::fd::AsRawFd;
        sock.as_raw_fd()
    };
    if let Err(e) = sock.set_nonblocking(true) {
        tprintln!("media socket: set_nonblocking failed ({e}); using ICE defaults");
        return None;
    }
    let tokio_sock = match tokio::net::UdpSocket::from_std(sock) {
        Ok(s) => s,
        Err(e) => {
            tprintln!(
                "media socket: could not adopt the socket into tokio ({e}); using ICE defaults"
            );
            return None;
        }
    };
    let mux = webrtc::ice::udp_mux::UDPMuxDefault::new(webrtc::ice::udp_mux::UDPMuxParams::new(
        tokio_sock,
    ));
    Some(MediaSocket {
        mux,
        raw,
        tuning,
        local,
    })
}

fn build_api(
    profile: H264Profile,
    mux: Option<Arc<webrtc::ice::udp_mux::UDPMuxDefault>>,
) -> Result<webrtc::api::API> {
    let mut media_engine = MediaEngine::default();

    let video_feedback = vec![
        RTCPFeedback {
            typ: "goog-remb".to_owned(),
            parameter: "".to_owned(),
        },
        RTCPFeedback {
            typ: "ccm".to_owned(),
            parameter: "fir".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "pli".to_owned(),
        },
        RTCPFeedback {
            typ: "transport-cc".to_owned(),
            parameter: "".to_owned(),
        },
    ];

    for (pt, rtx_pt, plid) in h264_codecs(profile) {
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: MIME_TYPE_H264.to_owned(),
                        clock_rate: 90000,
                        channels: 0,
                        sdp_fmtp_line: h264_fmtp(plid),
                        rtcp_feedback: video_feedback.clone(),
                    },
                    payload_type: pt,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .context("register H.264 codec")?;

        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: "video/rtx".to_owned(),
                        clock_rate: 90000,
                        channels: 0,
                        sdp_fmtp_line: format!("apt={pt}"),
                        rtcp_feedback: vec![],
                    },
                    payload_type: rtx_pt,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .context("register RTX codec")?;
    }

    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: OPUS_FALLBACK_FMTP.to_owned(),
                    rtcp_feedback: vec![RTCPFeedback {
                        typ: "nack".to_string(),
                        parameter: "".to_string(),
                    }],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )
        .context("register Opus codec")?;

    media_engine
        .register_header_extension(
            webrtc::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionCapability {
                uri: "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay".to_owned(),
            },
            RTPCodecType::Video,
            None,
        )
        .context("register playout-delay hdrext")?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)
        .context("register default interceptors")?;

    Ok(APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(udp_only_setting_engine(mux))
        .build())
}

fn udp_only_setting_engine(
    mux: Option<Arc<webrtc::ice::udp_mux::UDPMuxDefault>>,
) -> webrtc::api::setting_engine::SettingEngine {
    use webrtc::ice::network_type::NetworkType;
    use webrtc::ice::udp_mux::UDPMux;
    use webrtc::ice::udp_network::UDPNetwork;

    let mut se = webrtc::api::setting_engine::SettingEngine::default();
    se.set_network_types(vec![NetworkType::Udp4, NetworkType::Udp6]);
    se.set_srflx_acceptance_min_wait(Some(std::time::Duration::from_millis(50)));
    se.set_prflx_acceptance_min_wait(Some(std::time::Duration::from_millis(50)));
    se.set_relay_acceptance_min_wait(Some(std::time::Duration::from_millis(0)));
    // `enable_sender_rtx(true)` is deliberately NOT set, and this comment is here because it looks
    // like an oversight twice a year. webrtc-rs 0.17.1's NACK responder binds a stream only when
    // `stream_support_nack(info)` — `rtcp_feedback` containing `nack` with an empty parameter —
    // and the RTX codec is registered with `rtcp_feedback: vec![]`, so the responder never binds
    // it; `resend_packets` then looks the stream up by `nack.media_ssrc` and writes to the
    // *primary* SSRC's writer. Enabling it would add an `a=ssrc-group:FID` telling Chrome to
    // expect retransmissions on a stream that would always be silent. Repairs already reach the
    // client on the primary SSRC. See `docs/OPTIMIZATIONS.md` row 62.
    if let Some(mux) = mux {
        // Only ever on the LAN tier: muxed mode skips server-reflexive gathering outright
        // (`agent_gather.rs`, `UDPNetwork::Muxed(_) => continue`), and a same-subnet peer is
        // already offered no STUN server. Relay gathering builds its own sockets and is
        // unaffected, so a misclassified peer with TURN configured still connects.
        se.set_udp_network(UDPNetwork::Muxed(mux as Arc<dyn UDPMux + Send + Sync>));
    }
    se
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLocality {
    SameNetwork,
    CrossNetwork,
    Unknown,
}

/// The locality of the nominated pair, and the remote address it nominated.
///
/// The address is what Windows needs to mark the flow: qWAVE's `QOSAddSocketToFlow` requires a
/// destination on an unconnected socket, and this is the first moment one exists.
async fn detect_session_locality(
    pc: &webrtc::peer_connection::RTCPeerConnection,
) -> (SessionLocality, Option<std::net::SocketAddr>) {
    use std::collections::HashMap;

    use webrtc::stats::StatsReportType;

    let report = pc.get_stats().await;

    let mut cand_type: HashMap<String, String> = HashMap::new();
    let mut cand_ip: HashMap<String, String> = HashMap::new();
    let mut cand_addr: HashMap<String, std::net::SocketAddr> = HashMap::new();
    let mut nominated: Option<(String, String)> = None;
    for stat in report.reports.values() {
        match stat {
            StatsReportType::LocalCandidate(c) | StatsReportType::RemoteCandidate(c) => {
                cand_type.insert(c.id.clone(), format!("{:?}", c.candidate_type));
                cand_ip.insert(c.id.clone(), c.ip.clone());
                if let Ok(ip) = c.ip.parse::<std::net::IpAddr>() {
                    cand_addr.insert(c.id.clone(), std::net::SocketAddr::new(ip, c.port));
                }
            }
            StatsReportType::CandidatePair(p) if p.nominated => {
                nominated = Some((p.local_candidate_id.clone(), p.remote_candidate_id.clone()));
            }
            _ => {}
        }
    }

    let Some((lid, rid)) = nominated else {
        return (SessionLocality::Unknown, None);
    };
    let remote_addr = cand_addr.get(&rid).copied();

    // What the local end of the nominated pair is physically riding. A USB-tethered phone is an
    // ordinary Ethernet interface, so this is the only way the host can tell the wired path from
    // Wi-Fi — and the wired path is what every competitor in the category treats as its
    // low-latency tier. Reported through the log bus, because the desktop UI's appearance is
    // fixed.
    if let Some(lip) = cand_ip.get(&lid).cloned() {
        // `spawn_blocking`, not an inline call: on Windows this is two WMI round trips, and on
        // macOS a `getifaddrs` walk plus an ioctl. Once per session, never on a hot path — but
        // never on a tokio worker either.
        let probe = lip.clone();
        let medium = tokio::task::spawn_blocking(move || {
            crate::streamer::platform::link_medium_for_local_ip(&probe)
        })
        .await
        .ok()
        .flatten();
        match medium {
            Some(m) => tprintln!(
                "session link: local candidate {lip} is on a {} adapter",
                m.as_str()
            ),
            None => tprintln!("session link: could not determine the medium behind {lip}"),
        }
    }
    let lt = cand_type
        .get(&lid)
        .cloned()
        .unwrap_or_default()
        .to_lowercase();
    let rt = cand_type
        .get(&rid)
        .cloned()
        .unwrap_or_default()
        .to_lowercase();
    tprintln!("selected ICE candidate pair (local={lt}, remote={rt})");

    let lip = cand_ip.get(&lid).cloned().unwrap_or_default();
    let rip = cand_ip.get(&rid).cloned().unwrap_or_default();

    if lt.contains("relay") || rt.contains("relay") {
        if is_private_ip(&lip) && is_private_ip(&rip) && same_subnet_24(&lip, &rip) {
            teprintln!(
                "WARNING: same-subnet peers ({lip} <-> {rip}) are routing via relay/router — \
                 check AP client isolation (and AP-side WMM). Disabling client isolation lets \
                 the host<->client pair take a direct Wi-Fi path and removes 10-40ms of latency."
            );
        }
        (SessionLocality::CrossNetwork, remote_addr)
    } else if lt.contains("host") && rt.contains("host") {
        (SessionLocality::SameNetwork, remote_addr)
    } else if lt.is_empty() && rt.is_empty() {
        (SessionLocality::Unknown, remote_addr)
    } else {
        (SessionLocality::CrossNetwork, remote_addr)
    }
}

fn is_private_ip(ip: &str) -> bool {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback() || (v6.segments()[0] & 0xffc0) == 0xfe80,
        Err(_) => false,
    }
}

fn same_subnet_24(a: &str, b: &str) -> bool {
    match (a.parse::<std::net::IpAddr>(), b.parse::<std::net::IpAddr>()) {
        (Ok(std::net::IpAddr::V4(x)), Ok(std::net::IpAddr::V4(y))) => {
            let xo = x.octets();
            let yo = y.octets();
            xo[0] == yo[0] && xo[1] == yo[1] && xo[2] == yo[2]
        }
        _ => false,
    }
}

fn summarize_sdp_candidates(sdp: &str) -> String {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    let mut routable: Vec<String> = Vec::new();
    for line in sdp.lines() {
        let line = line.trim_start();
        let Some(body) = line
            .strip_prefix("a=candidate:")
            .or_else(|| line.strip_prefix("candidate:"))
        else {
            continue;
        };
        let toks: Vec<&str> = body.split_whitespace().collect();
        let typ = toks
            .iter()
            .position(|t| *t == "typ")
            .and_then(|i| toks.get(i + 1))
            .copied()
            .unwrap_or("?");
        *counts.entry(typ).or_insert(0) += 1;
        if matches!(typ, "srflx" | "relay" | "prflx") {
            let transport = toks.get(2).copied().unwrap_or("?");
            let ip = toks.get(4).copied().unwrap_or("?");
            let port = toks.get(5).copied().unwrap_or("?");
            routable.push(format!("{typ}/{transport} {ip}:{port}"));
        }
    }
    if counts.is_empty() {
        return "no ICE candidates in SDP".to_string();
    }
    let counts_str = counts
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    if routable.is_empty() {
        format!("{counts_str} (no public/relay candidates)")
    } else {
        format!("{counts_str} | {}", routable.join(", "))
    }
}

async fn log_ice_diagnostics(pc: &webrtc::peer_connection::RTCPeerConnection) {
    use std::collections::HashMap;

    use webrtc::stats::StatsReportType;

    let report = pc.get_stats().await;

    let mut cand: HashMap<String, String> = HashMap::new();
    for stat in report.reports.values() {
        if let StatsReportType::LocalCandidate(c) | StatsReportType::RemoteCandidate(c) = stat {
            cand.insert(
                c.id.clone(),
                format!("{:?} {}:{}", c.candidate_type, c.ip, c.port),
            );
        }
    }

    let mut pair_count = 0;
    for stat in report.reports.values() {
        if let StatsReportType::CandidatePair(p) = stat {
            pair_count += 1;
            let local = cand.get(&p.local_candidate_id).cloned().unwrap_or_default();
            let remote = cand
                .get(&p.remote_candidate_id)
                .cloned()
                .unwrap_or_default();
            teprintln!(
                "  ICE pair [{:?}] nominated={} reqSent={} respRecv={} local=({local}) remote=({remote})",
                p.state, p.nominated, p.requests_sent, p.responses_received
            );
        }
    }
    teprintln!("ICE diagnostics: {pair_count} candidate pair(s) above");
}

/// What the caller must hold to end a session.
///
/// Nothing used to close the host's peer connection — `grep close() ` over the transport found
/// only the self-test — and the connection could not have been dropped anyway, because its own
/// state-change handler held an `Arc` back to it. So every join leaked a peer connection, its ICE
/// agent, that agent's UDP sockets, and one `injector` OS thread whose only exit condition is both
/// of its channel senders being dropped. Not once per device: once per *join*, and the client
/// rebuilds the whole session on any settings change.
pub struct SessionHandle {
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    input_tx: Option<input::InjectorTx>,
    input_join: Option<std::thread::JoinHandle<()>>,
    /// The LAN-tier media socket, when this session got one. `UDPMuxDefault` runs a read worker
    /// that only stops on `close()`, so a session that forgot this would leak a thread and a
    /// socket per join — the exact shape of the leak the peer connection itself used to have.
    mux: Option<Arc<webrtc::ice::udp_mux::UDPMuxDefault>>,
    /// The qWAVE flow, released with the session.
    _qos: Arc<Mutex<Option<sockets::QosFlow>>>,
    /// The WHEP resource id, so the trickle registry entry goes away with the session rather than
    /// waiting for the `Weak` to be swept on the next join.
    resource_id: String,
}

impl SessionHandle {
    /// Close the peer connection and wait for the injector thread to release its input state.
    pub async fn close(mut self) {
        unregister_trickle(&self.resource_id);
        if let Err(e) = self.pc.close().await {
            teprintln!("closing peer connection: {e}");
        }
        if let Some(mux) = self.mux.take() {
            use webrtc::ice::udp_mux::UDPMux;
            if let Err(e) = mux.close().await {
                teprintln!("closing media socket: {e}");
            }
        }
        // Dropping the last sender is what tells the injector thread to leave its `select!`.
        drop(self.input_tx.take());
        if let Some(join) = self.input_join.take() {
            // The injector only ever blocks on the channel it was just told to abandon, so this
            // is a formality — but a formality worth having, because it is the proof the thread
            // went away rather than the hope that it did.
            let _ = tokio::task::spawn_blocking(move || join.join()).await;
        }
        LIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
/// The tracks, the sender and the answer that come out of a successful negotiation.
type NegotiatedMedia = (
    Arc<TrackLocalStaticRTP>,
    Arc<webrtc::rtp_transceiver::rtp_sender::RTCRtpSender>,
    Option<Arc<TrackLocalStaticSample>>,
    Option<Arc<RTCDataChannel>>,
    String,
);

/// A negotiated WHEP session whose pipeline does not exist yet.
///
/// Everything up to and including the answer depends only on the offer, the codec choice and the
/// ICE server list — not on the encoder, the capture session or the virtual display. Splitting
/// here is what lets `start_session` run the handshake and the display/pipeline work as two
/// concurrent futures instead of one serial chain.
pub struct PendingWhep {
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    /// The WHEP resource id this session is addressable by, for trickled ICE.
    resource_id: String,
    media_socket: Option<MediaSocket>,
    track: Arc<TrackLocalStaticRTP>,
    rtp_sender: Arc<webrtc::rtp_transceiver::rtp_sender::RTCRtpSender>,
    audio_track: Option<Arc<TrackLocalStaticSample>>,
    audio_dc: Option<Arc<RTCDataChannel>>,
    answer_sdp: String,
    mtu: usize,
    pacing_mode: PacingMode,
    playout_delay_max: u16,
    twcc_offered: bool,
    locality: SessionLocality,
}

impl PendingWhep {
    /// Give everything back when the other half of the join failed.
    ///
    /// Without this a display or pipeline failure would leak exactly what the peer connection used
    /// to leak on every join: the connection, its ICE agent, that agent's sockets, and the mux
    /// read worker.
    /// The WHEP resource id, which the `Location` header points at.
    pub fn resource_id(&self) -> &str {
        &self.resource_id
    }

    pub async fn abandon(mut self) {
        unregister_trickle(&self.resource_id);
        if let Err(e) = self.pc.close().await {
            teprintln!("closing peer connection after a failed join: {e}");
        }
        if let Some(ms) = self.media_socket.take() {
            use webrtc::ice::udp_mux::UDPMux;
            let _ = ms.mux.close().await;
        }
        LIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Build the peer connection, add the tracks, and produce the answer.
///
/// Needs no pipeline: the profile comes from the negotiation, the audio decision from the join
/// request's capability flags, and the transport constants from the pre-classified locality.
pub async fn prepare_whep(
    offer_sdp: String,
    ice_servers: Vec<RTCIceServer>,
    locality: SessionLocality,
    video: negotiate::NegotiatedVideo,
    audio: Option<&AudioParams>,
) -> Result<PendingWhep> {
    let profile = video.profile;
    // The LAN tier: our own socket, with the send/receive buffers and the Windows CONNRESET fix
    // webrtc-rs never applies, muxed so one port carries every host candidate. Never on a relayed
    // or unclassified path, because muxed mode gives up server-reflexive gathering.
    let media_socket = if locality == SessionLocality::SameNetwork {
        lan_media_socket()
    } else {
        None
    };
    if let Some(ms) = media_socket.as_ref() {
        tprintln!(
            "media socket: muxing ICE over {} with {}",
            ms.local,
            ms.tuning.describe()
        );
    }
    let api = build_api(profile, media_socket.as_ref().map(|m| Arc::clone(&m.mux)))?;

    let rtc_config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    let pc = Arc::new(
        api.new_peer_connection(rtc_config)
            .await
            .context("new_peer_connection")?,
    );
    LIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);

    // Every transport constant that should differ between a 1 ms same-subnet hop and a relayed
    // path, decided once, up front. The post-hoc detector still runs and complains if this
    // guess was wrong — but the first frames go out with the right sizes either way.
    let mtu = match locality {
        SessionLocality::CrossNetwork => RTP_PAYLOAD_MTU_RELAY,
        _ => RTP_PAYLOAD_MTU_LAN,
    };
    let pacing_mode = match locality {
        SessionLocality::SameNetwork => PacingMode::Off,
        SessionLocality::Unknown => PacingMode::Light,
        SessionLocality::CrossNetwork => PacingMode::Full,
    };
    let playout_delay_max = match locality {
        SessionLocality::CrossNetwork => PLAYOUT_DELAY_MAX_RELAY,
        _ => 0,
    };
    tprintln!(
        "session transport: locality={locality:?} mtu={mtu} pacing={pacing_mode:?} \
         playout_delay_max={}ms",
        playout_delay_max * 10
    );

    // Whether this client can carry a transport-wide sequence number at all. Read from the
    // offer, because an extension the peer did not offer cannot be negotiated into the answer.
    let twcc_offered = offer_sdp.contains("transport-wide-cc-extensions-01");

    // Everything from here to the answer can fail — `set_remote_description` on a malformed
    // offer most realistically — and each failure used to return with the peer connection, its
    // ICE agent, that agent's sockets and the mux read worker still alive, and `LIVE_SESSIONS`
    // still counting them. A client retrying a bad SDP leaked one of each per attempt.
    let prepared: Result<NegotiatedMedia> = async {
        let track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: h264_fmtp(profile_plid(profile)),
                rtcp_feedback: vec![],
            },
            "video".to_owned(),
            "webrtc-streamer".to_owned(),
        ));

        let rtp_sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("add_track")?;

        let (audio_track, audio_dc) = match audio {
            Some(a) => add_audio_tracks(&pc, a.fast, a.fallback_track).await?,
            None => (None, None),
        };

        tprintln!(
            "ICE candidates in remote offer (browser): {}",
            summarize_sdp_candidates(&offer_sdp)
        );
        let offer = RTCSessionDescription::offer(offer_sdp).context("parse offer SDP")?;
        pc.set_remote_description(offer)
            .await
            .context("set_remote_description(offer)")?;

        // No IDR here. The track has no binding until the connection comes up, and a write to an
        // unbound track is silently discarded — see the `Connected` arm above.

        let answer = pc.create_answer(None).await.context("create_answer")?;

        let mut gather_complete = pc.gathering_complete_promise().await;
        pc.set_local_description(answer)
            .await
            .context("set_local_description(answer)")?;
        // A same-subnet peer needs nothing but host candidates, and host gathering finishes in
        // single-digit milliseconds. Waiting the WAN-shaped 1.5 s for a reflexive candidate nobody
        // will use is pure time-to-first-frame, paid on every LAN join.
        let gather_max_wait = match locality {
            SessionLocality::SameNetwork => Duration::from_millis(GATHER_MAX_WAIT_LAN_MS),
            _ => Duration::from_millis(GATHER_MAX_WAIT_MS),
        };
        match tokio::time::timeout(gather_max_wait, gather_complete.recv()).await {
            Ok(_) => {}
            Err(_) => tprintln!(
                "ICE gathering still in progress after {gather_max_wait:?}; returning answer with candidates gathered so far"
            ),
        }

        let local = pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description after gathering"))?;

        tprintln!(
            "ICE candidates in local answer (host): {}",
            summarize_sdp_candidates(&local.sdp)
        );

        // webrtc-rs answers with the offer's own codec parameters, so without this the answer keeps
        // whatever level the browser named — Chrome always says 3.1, i.e. 1280x720@30 — no matter what
        // resolution the session actually negotiated.
        let answer_sdp = if video.may_raise_level {
            tprintln!(
                "answering H.264 {profile:?} at level 0x{:02x} (client allowed level asymmetry)",
                video.level_idc
            );
            negotiate::apply_level_to_answer(&local.sdp, video.level_idc)
        } else {
            local.sdp
        };

        Ok((track, rtp_sender, audio_track, audio_dc, answer_sdp))
    }
    .await;

    let (track, rtp_sender, audio_track, audio_dc, answer_sdp) = match prepared {
        Ok(v) => v,
        Err(e) => {
            if let Err(ce) = pc.close().await {
                teprintln!("closing peer connection after a failed negotiation: {ce}");
            }
            if let Some(ms) = media_socket {
                use webrtc::ice::udp_mux::UDPMux;
                let _ = ms.mux.close().await;
            }
            LIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
            return Err(e);
        }
    };

    // Registered only once the answer exists: the client cannot PATCH a resource whose id it has
    // not been given, and a failed negotiation must not leave an entry behind.
    let resource_id = mint_resource_id();
    register_trickle(&resource_id, &pc);

    Ok(PendingWhep {
        pc,
        resource_id,
        media_socket,
        track,
        rtp_sender,
        audio_track,
        audio_dc,
        answer_sdp,
        mtu,
        pacing_mode,
        playout_delay_max,
        twcc_offered,
        locality,
    })
}

/// Attach the running pipeline to an already-negotiated session and start every task.
///
/// Runs after the answer exists but strictly before it is returned to the client, so a handler
/// registered here is still in place before the peer can possibly connect.
pub async fn activate_whep(
    pending: PendingWhep,
    pipeline: &Pipeline,
    closed_tx: Option<tokio::sync::oneshot::Sender<()>>,
    input: InputParams,
    audio: Option<AudioParams>,
) -> Result<(String, SessionHandle)> {
    let PendingWhep {
        pc,
        resource_id,
        media_socket,
        track,
        rtp_sender,
        audio_track,
        audio_dc,
        answer_sdp,
        mtu,
        pacing_mode,
        playout_delay_max,
        twcc_offered,
        locality,
    } = pending;
    let keyframe_gate = Arc::new(KeyframeGate::new());
    let keyframes = KeyframeRequester {
        pipeline: pipeline.clone(),
        gate: Arc::clone(&keyframe_gate),
    };
    let feedback = Arc::new(SharedFeedback::default());
    let clock_epoch = Instant::now();
    *feedback.history.lock_recover() = Some(SendHistory::new(clock_epoch));
    // The pacer refills from the same target the encoder is given, so the two never disagree
    // about how much this session is allowed to send.
    let pacing_target_bps = Arc::new(std::sync::atomic::AtomicU32::new(pipeline.max_bitrate_bps));

    let InputParams {
        device: input_device,
        client_os,
        control_enabled,
        control_rx,
    } = input;
    let client = input::ClientProfile {
        device_name: input_device,
        platform: input::ClientPlatform::from_reported(&client_os),
    };
    let (input_tx, input_join) = input::spawn(client, Arc::clone(&pipeline.telemetry));
    {
        let input_tx = input_tx.clone();
        let control_rx = Arc::new(Mutex::new(control_rx));
        let session_telemetry = Arc::clone(&pipeline.telemetry);
        let keyframes_dc = keyframes.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let session_telemetry = Arc::clone(&session_telemetry);
            let keyframes_dc = keyframes_dc.clone();
            let label = dc.label().to_string();
            if !matches!(label.as_str(), "fast" | "reliable" | "bulk") {
                return Box::pin(async {});
            }
            let is_reliable = label == "reliable";
            let tx = input_tx.clone();
            let dc = Arc::clone(&dc);
            let control_rx = Arc::clone(&control_rx);
            Box::pin(async move {
                tprintln!("remote-input data channel open: {label}");
                let waiting = is_reliable
                    .then(|| control_rx.lock_recover().take())
                    .flatten();
                if let Some(mut rx) = waiting {
                    let dc_ctl = Arc::clone(&dc);
                    tokio::spawn(async move {
                        while let Some(msg) = rx.recv().await {
                            let frame = input::protocol::build_bye(msg.reason.code());
                            let _ = dc_ctl.send(&Bytes::copy_from_slice(&frame)).await;
                            if let Some(sent) = msg.sent {
                                let _ = sent.send(());
                            }
                        }
                    });
                }
                let dc_for_pong = is_reliable.then(|| Arc::clone(&dc));
                let idr_keyframes = keyframes_dc.clone();
                // The client rate-limits itself to one request per 200 ms, but that limit is
                // advisory: a hostile or looping client must not be able to pin the encoder at
                // one IDR per frame. `Instant` rather than the host clock because this is a
                // local interval, not a stamp anything else compares against.
                let mut last_idr_request: Option<std::time::Instant> = None;
                let telemetry_for_stats = Arc::clone(&session_telemetry);
                // One log line per distinct rejection per channel, so a client whose page has
                // drifted from this host build is diagnosable without a flood. A drifted client
                // repeats the same malformed frame thousands of times a second.
                let mut reported_errors: std::collections::HashSet<input::protocol::ParseError> =
                    std::collections::HashSet::new();
                // The Pong reply used to be written from inside the receive handler, which
                // head-of-line blocked every later message on this channel behind an SCTP send.
                // It goes to a task that owns the channel instead, so the handler only ever
                // parses and enqueues.
                let pong_tx = dc_for_pong.map(|dc_pong| {
                    let (pong_tx, mut pong_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
                    tokio::spawn(async move {
                        while let Some(t_ns) = pong_rx.recv().await {
                            let pong = input::protocol::build_pong(
                                t_ns,
                                crate::streamer::audio::host_now_ns(),
                            );
                            if dc_pong.send(&Bytes::copy_from_slice(&pong)).await.is_err() {
                                break;
                            }
                        }
                    });
                    pong_tx
                });
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let parsed = match input::protocol::parse(&msg.data) {
                        Ok(parsed) => parsed,
                        Err(e) => {
                            telemetry_for_stats
                                .degradation
                                .input_parse_errors
                                .fetch_add(1, Ordering::Relaxed);
                            if reported_errors.insert(e) {
                                teprintln!(
                                    "input: rejected a frame on the {} channel: {e} ({} bytes)",
                                    if is_reliable { "reliable" } else { "fast" },
                                    msg.data.len()
                                );
                            }
                            return Box::pin(async {});
                        }
                    };
                    // Both of these are transport business and are answered before the
                    // remote-control gate: a device with control disabled still needs its
                    // clock synced and still reports what its own pipeline is doing.
                    match parsed.ev {
                        input::protocol::InputEvent::ClientStats(stats) => {
                            telemetry_for_stats.set_client_stats(*stats);
                        }
                        input::protocol::InputEvent::Ping { t_ns } => {
                            if let Some(pong_tx) = &pong_tx {
                                let _ = pong_tx.send(t_ns);
                            }
                        }
                        // The receiver has no other way to ask: a client on the legacy encoded
                        // transform runs instead of libwebrtc's reference finder, so no PLI is
                        // ever emitted for it and a single lost keyframe would otherwise freeze
                        // the picture until its own watchdog gave up on the WebCodecs path.
                        input::protocol::InputEvent::KeyframeRequest { .. } => {
                            let now = std::time::Instant::now();
                            let allowed = last_idr_request
                                .is_none_or(|t| now.duration_since(t) >= MIN_IDR_REQUEST_GAP);
                            // The request is counted whether or not it is acted on: it is a real
                            // observation about the client either way.
                            telemetry_for_stats
                                .degradation
                                .keyframe_requests
                                .fetch_add(1, Ordering::Relaxed);
                            telemetry_for_stats
                                .degradation
                                .client_keyframe_requests
                                .fetch_add(1, Ordering::Relaxed);
                            if allowed {
                                last_idr_request = Some(now);
                                // Through the session's one rate-limited requester, not
                                // `pipeline.request_idr()` directly: this floor guards against a
                                // looping client, the gate guards against answering a decoder's
                                // whole backlog with the largest frame the encoder emits.
                                idr_keyframes.request_quietly();
                            }
                        }
                        ev if control_enabled => {
                            tx.route(ev, parsed.fast, parsed.client_ns, parsed.seq)
                        }
                        _ => {}
                    }
                    Box::pin(async {})
                }));
            })
        }));
    }

    {
        let pipeline = pipeline.clone();
        let keyframes = keyframes.clone();
        let feedback = Arc::clone(&feedback);
        tokio::spawn(async move {
            use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
            use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
            use webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
            use webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc;
            use webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
            let telemetry = Arc::clone(&pipeline.telemetry);
            let mut rtcp_buf = vec![0u8; 1500];
            let mut deltas: Vec<f64> = Vec::with_capacity(256);
            while let Ok((packets, _attrs)) = rtp_sender.read(&mut rtcp_buf).await {
                for p in &packets {
                    let any = p.as_any();
                    if any.downcast_ref::<PictureLossIndication>().is_some()
                        || any.downcast_ref::<FullIntraRequest>().is_some()
                    {
                        // The client's request is always counted — it is a real observation
                        // about the client. Whether we act on it is the gate's decision: a
                        // decoder that has just lost its reference chain emits a PLI for every
                        // frame in its backlog, and answering each one with the largest frame
                        // the encoder can produce is what turns a stumble into a storm.
                        telemetry
                            .degradation
                            .keyframe_requests
                            .fetch_add(1, Ordering::Relaxed);
                        keyframes.request_quietly();
                    } else if let Some(nack) = any.downcast_ref::<TransportLayerNack>() {
                        // Counted, never logged: a lossy link produces these continuously and
                        // every logbus line is a JSON IPC event to the UI.
                        let lost: u64 = nack
                            .nacks
                            .iter()
                            .map(|n| 1 + n.lost_packets.count_ones() as u64)
                            .sum();
                        telemetry
                            .degradation
                            .nacks_received
                            .fetch_add(lost.max(1), Ordering::Relaxed);
                    } else if let Some(fb) = any.downcast_ref::<TransportLayerCc>() {
                        let summary = {
                            let mut guard = feedback.history.lock_recover();
                            match guard.as_mut() {
                                Some(h) => congestion::interpret(fb, h, &mut deltas),
                                None => continue,
                            }
                        };
                        if summary.reported > 0 {
                            feedback.record_report(&summary);
                        }
                        if !deltas.is_empty() {
                            let mut det = feedback.detector.lock_recover();
                            for d in deltas.iter() {
                                det.observe(*d);
                            }
                        }
                    } else if let Some(remb) = any.downcast_ref::<ReceiverEstimatedMaximumBitrate>()
                    {
                        // A client that did not negotiate the transport-wide sequence number
                        // makes Chrome run its own receive-side estimator and report it here.
                        feedback.record_remb(remb.bitrate as u32);
                    }
                }
            }
        });
    }

    // One loss estimate, shared: the video BWE already polls `getStats` every 120 ms and the
    // audio DataChannel rides the same link, so audio does not need its own measurement — and
    // could not get a better one, since SCTP does not report loss the way RTCP does.
    let link_loss = Arc::new(std::sync::atomic::AtomicU32::new(0));

    spawn_bitrate_driver(
        Arc::clone(&pc),
        pipeline.clone(),
        Arc::clone(&feedback),
        Arc::clone(&keyframe_gate),
        Arc::clone(&pacing_target_bps),
        Arc::clone(&link_loss),
    );

    if let Some(mut audio) = audio {
        audio.link_loss = Some(link_loss);
        start_audio_forwarders(
            &pc,
            audio_track,
            audio_dc,
            audio,
            Arc::clone(&pipeline.telemetry),
        );
    }

    {
        let mut rx = pipeline.tx.subscribe();
        let track = Arc::clone(&track);
        let pc_keepalive = Arc::clone(&pc);
        let pipeline = pipeline.clone();
        let keyframes = keyframes.clone();
        let feedback = Arc::clone(&feedback);
        let pacing_target = Arc::clone(&pacing_target_bps);
        tokio::spawn(async move {
            let _pc = pc_keepalive;
            let telemetry = Arc::clone(&pipeline.telemetry);
            let mut packetizer = H264Packetizer::new();
            // A per-session access-unit ordinal, so transport-wide feedback can say which *frame*
            // the client received whole rather than only which packets it did. See
            // `SharedFeedback::acked_frame_capture_ns`.
            let mut frame_ordinal: u32 = 0;
            // Reused across frames: the payload list is the one per-frame allocation the writer
            // still owns, and it only ever has to grow to the largest access unit of the session.
            let mut payloads: Vec<Bytes> = Vec::with_capacity(64);
            let sequencer = new_random_sequencer();
            let mut pacer = Pacer::new(
                pacing_mode,
                pipeline.max_bitrate_bps,
                mtu,
                pipeline.frame_duration,
            );
            let mut warned_unbound = false;
            let mut warned_dropped_params = false;
            // Two extensions when the client offered transport-wide-cc, one when it did not.
            // Stamping an extension the peer never negotiated costs a marshal and a `BytesMut`
            // per packet and reaches nobody, and the send history it feeds would only ever be
            // written and never read.
            let mut extensions = Vec::with_capacity(2);
            extensions.push(webrtc::rtp::extension::HeaderExtension::PlayoutDelay(
                webrtc::rtp::extension::playout_delay_extension::PlayoutDelayExtension::new(
                    0,
                    playout_delay_max,
                ),
            ));
            if twcc_offered {
                extensions.push(webrtc::rtp::extension::HeaderExtension::TransportCc(
                    webrtc::rtp::extension::transport_cc_extension::TransportCcExtension::default(),
                ));
            } else {
                *feedback.history.lock_recover() = None;
            }
            loop {
                match rx.recv().await {
                    Ok(frame) => {
                        let mut timing = frame.timing;
                        let capture_ns = audio::host_instant_to_ns(frame.capture);
                        let rtp_ts = audio::host_ns_to_rtp90k(capture_ns);
                        frame_ordinal = frame_ordinal.wrapping_add(1);
                        packetizer.packetize(mtu, &frame.data, &mut payloads);
                        if payloads.is_empty() {
                            // Nothing packetizable in this access unit at all — an encoder that
                            // emitted only an AUD, or a degenerate buffer. Asking for another IDR
                            // is the only recovery, and the gate is what stops that spinning.
                            keyframes.request_quietly();
                            continue;
                        }
                        // The local packetizer sends parameter sets separately, and fragments
                        // either one, rather than dropping an oversized STAP-A the way the stock
                        // payloader did. That failure mode was a permanent black screen with RTP
                        // flowing and every host-side signal reporting success, so the assertion
                        // stays even though the path that produced it is gone.
                        if access_unit_has_sps(&frame.data)
                            && !payloads_carry_parameter_sets(&payloads)
                            && !warned_dropped_params
                        {
                            warned_dropped_params = true;
                            teprintln!(
                                "H.264 parameter sets did not survive packetization at a {mtu}-byte MTU; \
                                 this client cannot initialise its decoder and will stay black"
                            );
                        }
                        timing.mark_packetize_done();
                        pacer.set_target_bps(pacing_target.load(Ordering::Relaxed));
                        pacer.begin_frame();
                        let frame_bytes = frame.data.len() as u64;
                        let last = payloads.len().saturating_sub(1);
                        let mut stop = false;
                        let mut written = 0usize;
                        let waited_before = pacer.total_wait_ns();
                        let missed_before = pacer.deadline_misses();
                        for (i, payload) in payloads.drain(..).enumerate() {
                            let wire_len = payload.len() + 12;
                            // `PacingMode::Off` is the same code path the writer had before the
                            // pacer existed: no clock read, no bucket, no sleep.
                            if pacer.mode() != PacingMode::Off {
                                let wait = pacer.schedule(wire_len, Instant::now());
                                if !wait.is_zero() {
                                    tokio::time::sleep(wait).await;
                                }
                            }
                            if twcc_offered {
                                // Stamp the transport-wide sequence number ourselves. The stock
                                // `twcc::sender::Sender` interceptor would do the stamping but
                                // keeps its counter private, and without the packet-to-sequence
                                // mapping the feedback that comes back cannot be turned into a
                                // delay gradient.
                                let now = Instant::now();
                                let transport_seq = {
                                    let mut guard = feedback.history.lock_recover();
                                    guard.as_mut().map(|h| {
                                        h.record(wire_len as u32, now, frame_ordinal, capture_ns)
                                    })
                                };
                                if let Some(seq) = transport_seq {
                                    extensions[1] =
                                        webrtc::rtp::extension::HeaderExtension::TransportCc(
                                            webrtc::rtp::extension::transport_cc_extension::TransportCcExtension {
                                                transport_sequence: seq,
                                            },
                                        );
                                }
                            }
                            let pkt = webrtc::rtp::packet::Packet {
                                header: webrtc::rtp::header::Header {
                                    version: 2,
                                    marker: i == last,
                                    sequence_number: sequencer.next_sequence_number(),
                                    timestamp: rtp_ts,
                                    ..Default::default()
                                },
                                payload,
                            };
                            match track.write_rtp_with_extensions(&pkt, &extensions).await {
                                Ok(n) => written += n,
                                Err(e) => {
                                    tprintln!(
                                        "track write_rtp failed ({e}); viewer writer stopping"
                                    );
                                    stop = true;
                                    break;
                                }
                            }
                        }
                        if stop {
                            break;
                        }
                        if written == 0 {
                            // `TrackLocalStaticRTP` returns `Ok(0)` when the track has no
                            // binding yet or the sender is paused: the frame is dropped with no
                            // error anywhere. That is exactly how the handshake-time IDR used to
                            // vanish, so say it out loud once rather than let it be silent again.
                            if !warned_unbound {
                                warned_unbound = true;
                                tprintln!(
                                    "RTP discarded: the video track has no binding yet (waiting for the connection to come up)"
                                );
                            }
                            continue;
                        }
                        warned_unbound = false;
                        timing.mark_socket_write_done();
                        // Only the sleeping, not the writes it is interleaved with: the cost of
                        // pacing has to be separable from the cost of sending, or "the pacer is
                        // free on this path" and "the socket is slow" look the same.
                        telemetry
                            .stages
                            .pace_wait
                            .record(pacer.total_wait_ns().saturating_sub(waited_before));
                        let missed = pacer.deadline_misses().saturating_sub(missed_before);
                        if missed > 0 {
                            telemetry
                                .degradation
                                .pacer_deadline_misses
                                .fetch_add(missed, Ordering::Relaxed);
                        }
                        telemetry
                            .bytes_sent
                            .fetch_add(frame_bytes, Ordering::Relaxed);
                        telemetry.record_frame(&timing);
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        // Our own broadcast dropped frames on this viewer, which means the link
                        // is oversubscribed. A keyframe is the biggest frame the encoder can
                        // produce, so answering with one unconditionally is how a stumble
                        // becomes a storm: gate it, and tell the controller to take the rate
                        // down so the keyframe that does go out is smaller.
                        telemetry
                            .degradation
                            .frames_dropped_broadcast
                            .fetch_add(skipped, Ordering::Relaxed);
                        feedback.note_lag();
                        keyframes.request(&format!("viewer lagged (skipped={skipped})"));
                        continue;
                    }
                    Err(RecvError::Closed) => {
                        tprintln!("pipeline broadcast closed; viewer writer stopping");
                        break;
                    }
                }
            }
        });
    }

    // A `Weak`, not an `Arc`: webrtc-rs stores this closure inside the peer connection, so an
    // owning handle here is a reference cycle the connection can never escape — `close()` does
    // not clear the handler either, so even closing it would not have been enough.
    let pc_state = Arc::downgrade(&pc);
    let qos_flow: Arc<Mutex<Option<sockets::QosFlow>>> = Arc::new(Mutex::new(None));
    let qos_flow_state = Arc::clone(&qos_flow);
    let media_raw_handle = media_socket.as_ref().map(|m| m.raw);
    let locality_logged = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let closed_tx = Arc::new(Mutex::new(closed_tx));
    let input_tx_state = input_tx.clone();
    let keyframes_state = keyframes.clone();
    let (grace_cancel_tx, grace_cancel_rx) = tokio::sync::watch::channel(false);
    let grace_cancel_tx = Arc::new(grace_cancel_tx);
    let grace_cancel_rx_closure = grace_cancel_rx.clone();
    let expected_locality = locality;
    pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        tprintln!("peer connection state changed: {state:?}");
        if matches!(
            state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Closed
        ) {
            input_tx_state.release_all();
        }
        if state == RTCPeerConnectionState::Connected {
            let _ = grace_cancel_tx.send(true);
            // The IDR the handshake used to request went nowhere: `write_rtp` on a track with no
            // binding returns `Ok(0)`, so those frames were discarded without an error, and with
            // an infinite GOP nothing else would ever produce a keyframe. The client rescued
            // itself with a PLI, one browser PLI interval plus a round trip late. This is the
            // first moment the track can actually carry it.
            keyframes_state.request("connection is up and the track is bound");
            if !locality_logged.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let pc = pc_state.clone();
                let qos_slot = Arc::clone(&qos_flow_state);
                let media_raw = media_raw_handle;
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let Some(pc) = pc.upgrade() else { return };
                    let (observed, remote) = detect_session_locality(&pc).await;
                    // Windows marks DSCP through qWAVE and nowhere else, and qWAVE needs the
                    // destination — which does not exist until a pair is nominated. This is that
                    // moment. A failure means qWAVE is off or the platform lacks it; carry on
                    // unmarked and say so once.
                    if let (Some(raw), Some(dest)) = (media_raw, remote) {
                        match sockets::mark_video_flow(raw, dest) {
                            Some(flow) => {
                                tprintln!("media socket: marked as interactive video for {dest}");
                                *qos_slot.lock_recover() = Some(flow);
                            }
                            None => tprintln!(
                                "media socket: DSCP marking unavailable for {dest}; \
                                 traffic stays best-effort"
                            ),
                        }
                    }
                    if observed != expected_locality && observed != SessionLocality::Unknown {
                        teprintln!(
                            "session locality was pre-classified {expected_locality:?} but the \
                             nominated candidate pair says {observed:?}; transport constants for \
                             this session were chosen for the former"
                        );
                    } else {
                        tprintln!("session locality confirmed: {observed:?}");
                    }
                });
            }
        }
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Disconnected
        ) {
            let pc = pc_state.clone();
            tokio::spawn(async move {
                let Some(pc) = pc.upgrade() else { return };
                teprintln!("peer connection {state:?} — dumping ICE candidate pairs:");
                log_ice_diagnostics(&pc).await;
            });
        }
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            if let Some(tx) = closed_tx.lock_recover().take() {
                let _ = tx.send(());
            }
        } else if state == RTCPeerConnectionState::Disconnected {
            let closed_tx = Arc::clone(&closed_tx);
            let mut cancel_rx = grace_cancel_rx_closure.clone();
            let _ = grace_cancel_tx.send(false);
            tokio::spawn(async move {
                tprintln!("peer connection Disconnected — waiting 8s for recovery before closing");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(8)) => {
                        tprintln!("peer connection Disconnected grace period expired — closing session");
                        if let Some(tx) = closed_tx.lock_recover().take() {
                            let _ = tx.send(());
                        }
                    }
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            tprintln!("peer connection recovered before grace period expired — keeping session");
                        }
                    }
                }
            });
        }
        Box::pin(async {})
    }));
    let _ = grace_cancel_rx; // keep receiver alive so the watch channel stays open

    Ok((
        answer_sdp,
        SessionHandle {
            pc,
            input_tx: Some(input_tx),
            input_join: Some(input_join),
            mux: media_socket.map(|m| m.mux),
            _qos: qos_flow,
            resource_id,
        },
    ))
}

/// Prepare and activate in one call. The serial shape, kept for the WHEP self-test and any caller
/// that has a pipeline before it has an offer.
#[allow(clippy::too_many_arguments)]
pub async fn handle_whep_offer(
    offer_sdp: String,
    pipeline: &Pipeline,
    ice_servers: Vec<RTCIceServer>,
    closed_tx: Option<tokio::sync::oneshot::Sender<()>>,
    input: InputParams,
    audio: Option<AudioParams>,
    locality: SessionLocality,
    video: negotiate::NegotiatedVideo,
) -> Result<(String, SessionHandle)> {
    let pending = prepare_whep(offer_sdp, ice_servers, locality, video, audio.as_ref()).await?;
    activate_whep(pending, pipeline, closed_tx, input, audio).await
}

/// Add the audio media the answer must carry, before `create_answer`.
///
/// Both halves have to exist in the first answer: WHEP has no renegotiation verb, so a track or a
/// data channel that is not in it can never be added later. What feeds them is a separate
/// question, answered by [`start_audio_forwarders`] once the pipeline exists.
async fn add_audio_tracks(
    pc: &Arc<webrtc::peer_connection::RTCPeerConnection>,
    fast: bool,
    fallback_track: bool,
) -> Result<(
    Option<Arc<TrackLocalStaticSample>>,
    Option<Arc<RTCDataChannel>>,
)> {
    // On the fast path the Opus track is added but never fed: its whole job is to exist in the
    // answer so the client has an `m=audio` section to fall back onto if its WebCodecs decoder
    // dies mid-session.
    let track = if !fast || fallback_track {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: OPUS_FALLBACK_FMTP.to_owned(),
                rtcp_feedback: vec![],
            },
            "audio".to_owned(),
            "webrtc-streamer".to_owned(),
        ));
        pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("add_track(opus fallback)")?;
        if fast {
            tprintln!("audio: added a standard Opus track as an unfed fallback for the client");
        } else {
            tprintln!("audio: added standard Opus track (NetEQ fallback path)");
        }
        Some(track)
    } else {
        None
    };

    let dc = if fast {
        let init = RTCDataChannelInit {
            ordered: Some(false),
            max_retransmits: Some(0),
            ..Default::default()
        };
        let dc = pc
            .create_data_channel("audio", Some(init))
            .await
            .context("create audio data channel")?;
        tprintln!("audio: created unordered/no-retransmit DataChannel 'audio'");
        Some(dc)
    } else {
        None
    };

    Ok((track, dc))
}

/// Start whatever feeds the audio media that [`add_audio_tracks`] put in the answer.
///
/// Registering `on_open` here rather than at track-add time is safe because the answer has not
/// been returned yet, so the peer cannot have connected and the channel cannot have opened.
fn start_audio_forwarders(
    pc: &Arc<webrtc::peer_connection::RTCPeerConnection>,
    track: Option<Arc<TrackLocalStaticSample>>,
    dc: Option<Arc<RTCDataChannel>>,
    audio: AudioParams,
    telemetry: crate::streamer::telemetry::SharedTelemetry,
) {
    if !audio.fast {
        if let Some(track) = track {
            tokio::spawn(forward_track(
                Arc::clone(pc),
                track,
                audio.clone(),
                Arc::clone(&telemetry),
            ));
        }
    }

    if let Some(dc) = dc {
        let dc_for_open = Arc::clone(&dc);
        // Weak for the same reason as the connection-state handler: the data channel this
        // closure is attached to is owned by the peer connection, so an `Arc` here is a cycle.
        let pc_for_open = Arc::downgrade(pc);
        dc.on_open(Box::new(move || {
            let dc = Arc::clone(&dc_for_open);
            let pc = pc_for_open.clone();
            let audio = audio.clone();
            let telemetry = Arc::clone(&telemetry);
            Box::pin(async move {
                let Some(pc) = pc.upgrade() else { return };
                tokio::spawn(forward_datachannel(pc, dc, audio, telemetry));
            })
        }));
    }
}

fn pc_alive(pc: &webrtc::peer_connection::RTCPeerConnection) -> bool {
    !matches!(
        pc.connection_state(),
        RTCPeerConnectionState::Closed
            | RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected
    )
}

async fn subscribe_blocking(hub: &SharedAudioHub) -> Option<audio::AudioSubscription> {
    let hub = Arc::clone(hub);
    match tokio::task::spawn_blocking(move || hub.subscribe()).await {
        Ok(Ok(sub)) => Some(sub),
        Ok(Err(e)) => {
            teprintln!("audio: subscribe failed ({e:#})");
            None
        }
        Err(_) => None,
    }
}

async fn unsubscribe_blocking(hub: &SharedAudioHub) {
    let hub = Arc::clone(hub);
    let _ = tokio::task::spawn_blocking(move || hub.unsubscribe()).await;
}

async fn forward_datachannel(
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    dc: Arc<RTCDataChannel>,
    audio: AudioParams,
    telemetry: crate::streamer::telemetry::SharedTelemetry,
) {
    let mut sub: Option<audio::AudioSubscription> = None;
    // Sticky across iterations: any producer-side skip — a broadcast lag or a backpressure
    // drop — leaves a hole the client must be told about, and it is told on the next packet
    // that actually goes out. Without this the client sees a `seq` jump with `flags == 0`,
    // which is indistinguishable from ordinary network loss, so it splices the new audio onto
    // the old with no resync and keeps the offset for good.
    let mut pending_discontinuity = false;
    let mut prev_payload: Option<Bytes> = None;
    loop {
        if !pc_alive(&pc) || dc.ready_state() == RTCDataChannelState::Closed {
            break;
        }
        let want = audio.audio_enabled_now();
        match (want, sub.is_some()) {
            (true, false) => {
                sub = subscribe_blocking(&audio.hub).await;
                pending_discontinuity = true;
                prev_payload = None;
            }
            (false, true) => {
                sub = None;
                unsubscribe_blocking(&audio.hub).await;
            }
            _ => {}
        }

        let Some(s) = sub.as_mut() else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };

        match tokio::time::timeout(Duration::from_millis(250), s.rx.recv()).await {
            Ok(Ok(pkt)) => {
                // Checked every packet, not one in four. The old sampling detected a backed-up
                // channel up to 20 ms late and — worse — kept discarding for up to 20 ms after
                // it had drained. `buffered_amount` is a read behind a lock in webrtc-rs, not a
                // syscall.
                let msg_len = protocol::HEADER_LEN + pkt.data.len();
                if dc.buffered_amount().await > backpressure_limit_bytes(msg_len) {
                    s.diagnostics
                        .dropped_backpressure
                        .fetch_add(1, Ordering::Relaxed);
                    pending_discontinuity = true;
                    prev_payload = None;
                    continue;
                }
                let mut flags = pkt.flags;
                if pending_discontinuity {
                    flags |= audio::FLAG_DISCONTINUITY;
                }
                let redundant = if audio.redundancy_now() {
                    prev_payload.as_deref()
                } else {
                    None
                };
                let msg = protocol::build_datachannel_message(
                    pkt.seq,
                    pkt.capture_ns,
                    flags,
                    &pkt.data,
                    redundant,
                );
                if dc.send(&msg).await.is_err() {
                    break;
                }
                pending_discontinuity = false;
                prev_payload = Some(pkt.data);
            }
            // The subscriber fell behind the host-wide capture and the broadcast dropped
            // packets on its behalf: a real audio gap, previously invisible.
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                telemetry
                    .degradation
                    .audio_broadcast_lag_drops
                    .fetch_add(skipped, Ordering::Relaxed);
                if let Some(s) = sub.as_ref() {
                    s.diagnostics
                        .lagged_packets
                        .fetch_add(skipped, Ordering::Relaxed);
                }
                pending_discontinuity = true;
                prev_payload = None;
                continue;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                sub = None;
            }
            Err(_) => continue,
        }
    }
    if sub.take().is_some() {
        unsubscribe_blocking(&audio.hub).await;
    }
    tprintln!("audio: DataChannel forwarder stopped");
}

async fn forward_track(
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    audio: AudioParams,
    telemetry: crate::streamer::telemetry::SharedTelemetry,
) {
    let frame_duration = Duration::from_millis(crate::streamer::audio::encoder::FRAME_MS as u64);
    // A gap longer than this is a device change or a stall, not a scheduling wobble; advancing
    // the track's clock by the true elapsed time is right up to that point, and beyond it the
    // sane thing is to treat the timeline as restarted rather than to jump the RTP clock by
    // minutes.
    const MAX_CATCHUP: Duration = Duration::from_millis(500);
    let mut last_capture_ns: Option<u64> = None;
    let mut sub: Option<audio::AudioSubscription> = None;
    loop {
        if !pc_alive(&pc) {
            break;
        }
        let want = audio.audio_enabled_now();
        match (want, sub.is_some()) {
            (true, false) => sub = subscribe_blocking(&audio.hub).await,
            (false, true) => {
                sub = None;
                unsubscribe_blocking(&audio.hub).await;
            }
            _ => {}
        }

        let Some(s) = sub.as_mut() else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };

        match tokio::time::timeout(Duration::from_millis(250), s.rx.recv()).await {
            Ok(Ok(pkt)) => {
                // `TrackLocalStaticSample` derives each RTP timestamp by *accumulating*
                // `duration`, so a fixed 5 ms advanced the audio timeline by 5 ms per packet
                // no matter what actually happened at capture. Every real gap — a device
                // change, a broadcast lag, a backpressure drop — therefore left audio
                // permanently 5 ms per lost packet behind the video, which rides the true host
                // clock in its RTP timestamps, and nothing ever corrected it. Spacing samples
                // by the real elapsed `capture_ns` puts both media back on one timeline.
                let duration = match last_capture_ns {
                    Some(prev) if pkt.capture_ns > prev => {
                        Duration::from_nanos(pkt.capture_ns - prev).min(MAX_CATCHUP)
                    }
                    _ => frame_duration,
                };
                last_capture_ns = Some(pkt.capture_ns);
                if track
                    .write_sample(&Sample {
                        data: pkt.data,
                        duration,
                        ..Default::default()
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            // The subscriber fell behind the host-wide capture and the broadcast dropped
            // packets on its behalf: a real audio gap, previously invisible.
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                telemetry
                    .degradation
                    .audio_broadcast_lag_drops
                    .fetch_add(skipped, Ordering::Relaxed);
                if let Some(s) = sub.as_ref() {
                    s.diagnostics
                        .lagged_packets
                        .fetch_add(skipped, Ordering::Relaxed);
                }
                continue;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                sub = None;
            }
            Err(_) => continue,
        }
    }
    if sub.take().is_some() {
        unsubscribe_blocking(&audio.hub).await;
    }
    tprintln!("audio: track forwarder stopped");
}

fn spawn_bitrate_driver(
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    pipeline: Pipeline,
    feedback: Arc<SharedFeedback>,
    keyframe_gate: Arc<KeyframeGate>,
    pacing_target_bps: Arc<std::sync::atomic::AtomicU32>,
    link_loss: Arc<std::sync::atomic::AtomicU32>,
) {
    use webrtc::stats::StatsReportType;

    let max_bps = pipeline.max_bitrate_bps;
    let min_bps = DEFAULT_MIN_BITRATE_BPS.min(max_bps);
    let mut controller = BitrateController::new(min_bps, max_bps);
    let mut current_target = max_bps;
    // The estimator runs on its own running value rather than on the emitted target. That is
    // what lets a small per-interval cut compound: fed back the *emitted* target, a 5% loss
    // producing a sub-deadband cut would be suppressed by the controller, recomputed from the
    // unchanged target, and suppressed again — the dead band by another route.
    let mut estimate_bps = max_bps;
    let mut delay_estimate = DelayBasedEstimate::new(max_bps, min_bps, max_bps);
    let mut last_bytes_sent: Option<(u64, Instant)> = None;
    let mut rtt_monitor = RttCongestionMonitor::new();
    let mut signal_logged = false;
    // `fraction_lost` and `round_trip_time` come from the remote's RTCP receiver reports, which
    // browsers emit about once a second, and this loop runs every 120 ms. Seven polls in eight
    // therefore re-read the same two numbers — and feeding an unchanged reading back into the
    // controller is not a no-op: it lets one loss event be acted on up to eight times, which is
    // the mechanism behind the ratcheting the cut rate-limit was added to stop. Remember them and
    // only drive the loss and RTT arms when they actually moved.
    let mut last_receiver_report: Option<(u64, u64)> = None;
    let mut stale_report_run: u32 = 0;
    let mut stale_run_logged = false;

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(BWE_POLL_INTERVAL);
        loop {
            ticker.tick().await;

            use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::*;
            match pc.connection_state() {
                Closed | Failed | Disconnected => break,
                _ => {}
            }

            let report = pc.get_stats().await;

            let mut bytes_sent: u64 = 0;
            let mut have_outbound = false;
            let mut fraction_lost: f64 = 0.0;
            let mut max_rtt_ms: f64 = 0.0;
            for stat in report.reports.values() {
                match stat {
                    StatsReportType::OutboundRTP(s) => {
                        bytes_sent += s.bytes_sent;
                        have_outbound = true;
                    }
                    StatsReportType::RemoteInboundRTP(s) => {
                        fraction_lost = fraction_lost.max(s.fraction_lost);
                        // webrtc-rs does NOT use the W3C unit here. `round_trip_time` is
                        // already milliseconds: interceptor-0.17.1/src/stats/interceptor.rs
                        // `calculate_rtt_ms` returns `rtt_seconds * 1000.0 + …` and it is
                        // copied verbatim into this field. Scaling it again pinned every
                        // session at the bitrate floor.
                        if let Some(rtt_ms) = s.round_trip_time {
                            max_rtt_ms = max_rtt_ms.max(rtt_ms);
                        }
                    }
                    _ => {}
                }
            }

            if !have_outbound {
                continue;
            }

            // Publish for the audio forwarder, in hundredths of a percent so it fits an
            // integer without losing the range that matters (0.5 % to 10 %).
            link_loss.store(
                (fraction_lost.clamp(0.0, 1.0) * 10_000.0) as u32,
                Ordering::Relaxed,
            );

            let now = Instant::now();
            let measured_send_bps = match last_bytes_sent {
                Some((prev_bytes, prev_t)) => {
                    let dt = now.duration_since(prev_t).as_secs_f64();
                    if dt > 0.0 {
                        let delta = bytes_sent.saturating_sub(prev_bytes);
                        ((delta as f64 * 8.0) / dt) as u32
                    } else {
                        0
                    }
                }
                None => {
                    last_bytes_sent = Some((bytes_sent, now));
                    continue;
                }
            };
            last_bytes_sent = Some((bytes_sent, now));

            // Bit-exact comparison of the raw doubles: any real receiver report moves at least one
            // of them, and a repeat is a repeat.
            let report_key = (fraction_lost.to_bits(), max_rtt_ms.to_bits());
            let report_fresh = last_receiver_report != Some(report_key);
            if report_fresh {
                last_receiver_report = Some(report_key);
                stale_report_run = 0;
            } else {
                stale_report_run += 1;
                if stale_report_run >= 16 && !stale_run_logged {
                    stale_run_logged = true;
                    tprintln!(
                        "BWE: receiver reports have not moved for {stale_report_run} polls \
                         ({}ms); loss and RTT arms idle until they do",
                        stale_report_run as u64 * BWE_POLL_INTERVAL.as_millis() as u64
                    );
                }
            }

            if report_fresh {
                keyframe_gate.observe_rtt_ms(max_rtt_ms);
            }
            pipeline
                .telemetry
                .degradation
                .keyframe_requests_suppressed
                .store(keyframe_gate.suppressed(), Ordering::Relaxed);

            // Transport-wide feedback, when the client negotiated the extension, supersedes the
            // receiver report on both counts: it names every lost sequence number at the
            // browser's own feedback cadence rather than once a second, and it carries the
            // arrival times that make a delay gradient possible at all.
            let twcc_loss = feedback.take_loss();
            let delivered_bps = feedback.received_bps();
            // Transport-wide feedback is consumed on read, so a `Some` here is always a new
            // observation. The receiver report is not, so it is only used when it moved.
            let loss_signal: Option<f64> = match twcc_loss {
                Some((l, _)) => Some(l),
                None if report_fresh => Some(fraction_lost),
                None => None,
            };
            let loss = loss_signal.unwrap_or(fraction_lost);

            let delay_state = {
                let det = feedback.detector.lock_recover();
                if det.samples() > 0 {
                    Some(det.state())
                } else {
                    None
                }
            };

            let mut estimate = match loss_signal {
                Some(l) => estimate_from_loss(estimate_bps, measured_send_bps, l),
                // Nothing new to react to. Holding the previous estimate is the point: re-running
                // the controller on a stale reading is how one loss event became eight cuts.
                None => estimate_bps,
            };

            if let Some(state) = delay_state {
                let delay_bps = delay_estimate.update(state, delivered_bps, now);
                if state == NetworkState::Overuse {
                    pipeline
                        .telemetry
                        .degradation
                        .delay_overuse_events
                        .fetch_add(1, Ordering::Relaxed);
                }
                // The delay signal fires before loss does, so it may only ever lower the
                // estimate — never raise it above what the loss controller allows.
                estimate = estimate.min(delay_bps.max(min_bps));
            } else if report_fresh && rtt_monitor.observe(max_rtt_ms) {
                // No transport-wide feedback: fall back to the round-trip excursion detector,
                // which is coarse but is all a client without the extension gives us.
                let rtt_estimate = (estimate_bps as f64 * 0.85) as u32;
                if rtt_estimate < estimate {
                    let baseline = rtt_monitor.baseline_ms().unwrap_or(0.0);
                    tprintln!(
                        "BWE: RTT congestion signal (rtt_ms={max_rtt_ms:.1} is >{RTT_DELAY_THRESHOLD_MS}ms \
                         above baseline {baseline:.1}ms); clamping estimate {estimate} -> {rtt_estimate}"
                    );
                    estimate = rtt_estimate;
                }
            }

            if let Some(remb) = feedback.remb_bps() {
                estimate = estimate.min(remb.max(min_bps));
            }

            // A viewer the local broadcast dropped frames on is a viewer whose link is
            // oversubscribed. Cutting here is what keeps the keyframe that follows small.
            let lags = feedback.take_lag_events();
            if lags > 0 {
                let base = if measured_send_bps > 0 {
                    measured_send_bps
                } else {
                    estimate
                };
                estimate = estimate.min(((base as f64) * 0.85) as u32).max(min_bps);
            }

            estimate_bps = estimate.clamp(min_bps, max_bps);

            if !signal_logged && (feedback.twcc_reports() > 0 || feedback.remb_reports() > 0) {
                signal_logged = true;
                tprintln!(
                    "BWE: congestion signal is {} (twcc reports={}, remb reports={})",
                    feedback.signal_name(),
                    feedback.twcc_reports(),
                    feedback.remb_reports()
                );
            }

            if let Some(target) = controller.update(estimate_bps, now) {
                if target < current_target {
                    pipeline
                        .telemetry
                        .degradation
                        .bitrate_cuts
                        .fetch_add(1, Ordering::Relaxed);
                }
                tprintln!(
                    "BWE: pushing adaptive bitrate target (measured_send_bps={measured_send_bps}, \
                     delivered_bps={delivered_bps}, loss={loss:.3}, rtt_ms={max_rtt_ms:.1}, \
                     delay={delay_state:?}, estimate={estimate_bps}, target={target})"
                );
                current_target = target;
                pipeline.set_target_bitrate(target);
                pacing_target_bps.store(target, Ordering::Relaxed);
            }
        }
        tprintln!("bitrate driver stopped (peer connection closed)");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relayed_session_gets_a_smaller_mtu_than_a_lan_one() {
        assert_eq!(RTP_PAYLOAD_MTU_LAN, 1188);
        assert_eq!(RTP_PAYLOAD_MTU_RELAY, 1012);
    }

    #[test]
    fn the_chosen_profile_leads_the_registered_codec_list() {
        // The track binds to the first codec whose fmtp matches, so a High session must register
        // High first — and must still register Baseline, so a client that turns out to offer only
        // Baseline gets a picture instead of a silent codec mismatch.
        let high = h264_codecs(H264Profile::High);
        assert_eq!(high[0].2, "640c1f");
        assert!(high.iter().any(|c| c.2 == "42e01f"));
        let main = h264_codecs(H264Profile::Main);
        assert_eq!(main[0].2, "4d001f");
        assert!(main.iter().any(|c| c.2 == "42e01f"));
        assert_eq!(h264_codecs(H264Profile::Baseline).len(), 1);
    }

    /// SDP lines are joined with a real CRLF, built here rather than escaped inline, so the
    /// fixtures read as the wire format they are.
    fn frag(lines: &[&str]) -> String {
        let mut out = String::new();
        for l in lines {
            out.push_str(l);
            out.push_str("\r\n");
        }
        out
    }

    fn audio_params(locality: SessionLocality, redundancy: bool) -> AudioParams {
        AudioParams {
            fast: true,
            fallback_track: false,
            redundancy,
            link_loss: Some(Arc::new(std::sync::atomic::AtomicU32::new(0))),
            locality,
            hub: crate::streamer::audio::AudioHub::new(),
            overrides: Arc::new(std::sync::Mutex::new(Default::default())),
            device_key: "test".to_string(),
        }
    }

    /// A relayed session is known to be relayed before the first packet, so it does not have to
    /// wait for the BWE to observe loss it could have predicted.
    #[test]
    fn a_relayed_session_starts_defensive_and_a_lan_one_does_not() {
        assert!(audio_params(SessionLocality::CrossNetwork, true).redundancy_now());
        assert!(!audio_params(SessionLocality::SameNetwork, true).redundancy_now());
        assert!(!audio_params(SessionLocality::Unknown, true).redundancy_now());
    }

    /// The client's declared capability outranks everything: one that cannot parse the format
    /// must never be sent it, however bad the link is.
    #[test]
    fn a_client_that_cannot_parse_redundancy_is_never_sent_it() {
        assert!(!audio_params(SessionLocality::CrossNetwork, false).redundancy_now());
        let p = audio_params(SessionLocality::SameNetwork, false);
        p.link_loss
            .as_ref()
            .unwrap()
            .store(9_000, Ordering::Relaxed);
        assert!(!p.redundancy_now());
    }

    /// Loss still drives it on a LAN, which is the case the pre-classification cannot predict.
    #[test]
    fn observed_loss_still_turns_redundancy_on() {
        let p = audio_params(SessionLocality::SameNetwork, true);
        p.link_loss
            .as_ref()
            .unwrap()
            .store(AUDIO_REDUNDANCY_LOSS_THRESHOLD, Ordering::Relaxed);
        assert!(p.redundancy_now());
    }

    #[test]
    fn a_trickle_fragment_carries_its_mid_and_m_line_index() {
        let f = parse_trickle_fragment(&frag(&[
            "a=ice-ufrag:AbCd",
            "m=video 9 UDP/TLS/RTP/SAVPF 102",
            "a=mid:0",
            "a=candidate:1 1 udp 2130706431 192.168.1.5 54321 typ host",
            "a=candidate:2 1 udp 1694498815 203.0.113.7 40000 typ srflx",
        ]));
        assert_eq!(f.ufrag.as_deref(), Some("AbCd"));
        assert_eq!(f.candidates.len(), 2);
        assert_eq!(f.candidates[0].1.as_deref(), Some("0"));
        assert_eq!(f.candidates[0].2, Some(0));
        assert!(f.candidates[0].0.starts_with("1 1 udp"));
        assert!(!f.end_of_candidates);
    }

    #[test]
    fn a_second_m_section_advances_the_m_line_index() {
        let f = parse_trickle_fragment(&frag(&[
            "m=audio 9 UDP/TLS/RTP/SAVPF 111",
            "a=mid:0",
            "a=candidate:1 1 udp 1 10.0.0.1 1 typ host",
            "m=video 9 UDP/TLS/RTP/SAVPF 102",
            "a=mid:1",
            "a=candidate:2 1 udp 1 10.0.0.1 2 typ host",
        ]));
        assert_eq!(f.candidates[0].2, Some(0));
        assert_eq!(f.candidates[1].2, Some(1));
        assert_eq!(f.candidates[1].1.as_deref(), Some("1"));
    }

    #[test]
    fn end_of_candidates_is_noticed_and_a_bare_candidate_line_is_accepted() {
        let f = parse_trickle_fragment(&frag(&[
            "candidate:1 1 udp 1 10.0.0.1 1 typ host",
            "a=end-of-candidates",
        ]));
        assert_eq!(f.candidates.len(), 1);
        assert!(f.end_of_candidates);
    }

    #[test]
    fn an_empty_or_junk_fragment_yields_nothing_rather_than_failing() {
        assert_eq!(parse_trickle_fragment(""), TrickleFragment::default());
        assert_eq!(
            parse_trickle_fragment(&frag(&["v=0", "o=- 0 0 IN IP4 0.0.0.0"])),
            TrickleFragment::default()
        );
    }

    #[tokio::test]
    async fn trickling_to_a_session_that_does_not_exist_is_reported_not_ignored() {
        assert!(!add_trickled_candidates("deadbeef", "a=end-of-candidates").await);
    }

    #[test]
    fn every_registered_payload_type_is_distinct() {
        for profile in [H264Profile::Baseline, H264Profile::Main, H264Profile::High] {
            let mut pts: Vec<PayloadType> = h264_codecs(profile)
                .into_iter()
                .flat_map(|(pt, rtx, _)| [pt, rtx])
                .collect();
            let before = pts.len();
            pts.sort_unstable();
            pts.dedup();
            assert_eq!(pts.len(), before, "duplicate payload type for {profile:?}");
        }
    }
}

/// Live sessions addressable by their WHEP resource id, for trickle ICE.
///
/// `Weak`, so a session that ends drops out of here without anything having to remember to remove
/// it — and so this map can never be the thing that keeps a peer connection alive.
static TRICKLE_SESSIONS: Mutex<
    Option<
        std::collections::HashMap<
            String,
            std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>,
        >,
    >,
> = Mutex::new(None);

fn mint_resource_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn register_trickle(id: &str, pc: &Arc<webrtc::peer_connection::RTCPeerConnection>) {
    let mut guard = TRICKLE_SESSIONS.lock_recover();
    let map = guard.get_or_insert_with(Default::default);
    map.retain(|_, weak| weak.strong_count() > 0);
    map.insert(id.to_string(), Arc::downgrade(pc));
}

fn unregister_trickle(id: &str) {
    if let Some(map) = TRICKLE_SESSIONS.lock_recover().as_mut() {
        map.remove(id);
    }
}

/// One `a=candidate:` line's worth of trickled ICE, as RFC 9725 sends it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TrickleFragment {
    pub candidates: Vec<(String, Option<String>, Option<u16>)>,
    pub ufrag: Option<String>,
    /// The fragment said the client has finished gathering (`a=end-of-candidates`).
    pub end_of_candidates: bool,
}

/// Parse an `application/trickle-ice-sdpfrag` body.
///
/// A fragment is a partial SDP: zero or more `m=` sections, each with an optional `a=mid:` and
/// some `a=candidate:` lines, plus `a=ice-ufrag:` for the generation the candidates belong to. The
/// mid and the m-line index are what tell the agent which media section a candidate is for; with
/// BUNDLE there is only one, but carrying them through is what makes this work with a client that
/// does not bundle.
pub fn parse_trickle_fragment(body: &str) -> TrickleFragment {
    let mut out = TrickleFragment::default();
    let mut mid: Option<String> = None;
    let mut mline: i32 = -1;
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with("m=") {
            mline += 1;
            mid = None;
        } else if let Some(v) = line.strip_prefix("a=mid:") {
            mid = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("a=ice-ufrag:") {
            out.ufrag = Some(v.trim().to_string());
        } else if line == "a=end-of-candidates" {
            out.end_of_candidates = true;
        } else if let Some(v) = line
            .strip_prefix("a=candidate:")
            .or_else(|| line.strip_prefix("candidate:"))
        {
            let index = if mline >= 0 { Some(mline as u16) } else { None };
            out.candidates
                .push((v.trim().to_string(), mid.clone(), index));
        }
    }
    out
}

/// Feed trickled candidates to a live session. Returns whether the session was found.
pub async fn add_trickled_candidates(id: &str, body: &str) -> bool {
    use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

    let pc = {
        let guard = TRICKLE_SESSIONS.lock_recover();
        guard
            .as_ref()
            .and_then(|m| m.get(id).and_then(|w| w.upgrade()))
    };
    let Some(pc) = pc else {
        return false;
    };

    let frag = parse_trickle_fragment(body);
    let mut added = 0usize;
    for (candidate, sdp_mid, sdp_mline_index) in frag.candidates {
        let init = RTCIceCandidateInit {
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment: frag.ufrag.clone(),
        };
        match pc.add_ice_candidate(init).await {
            Ok(()) => added += 1,
            Err(e) => teprintln!("trickle: rejected a candidate ({e})"),
        }
    }
    if added > 0 || frag.end_of_candidates {
        tprintln!(
            "trickle: added {added} remote candidate(s){}",
            if frag.end_of_candidates {
                ", client finished gathering"
            } else {
                ""
            }
        );
    }
    true
}

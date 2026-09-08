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
use webrtc::rtp::codecs::h264::H264Payloader;
use webrtc::rtp::packetizer::Payloader;
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
use super::pacer::{Pacer, PacingMode};
use super::pipeline::Pipeline;
use super::session::{self, SharedDeviceOverrides};

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

/// Whether an access unit carries a sequence parameter set.
///
/// Parameter sets lead a keyframe access unit, at most behind an AUD or an SEI, so a short prefix
/// scan answers the question and costs nothing on the P-frames that dominate the stream.
fn access_unit_has_sps(data: &[u8]) -> bool {
    const SPS_NAL_TYPE: u8 = 7;
    let end = data.len().min(256);
    let mut i = 0usize;
    while i + 3 < end {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if data[i + 3] & 0x1f == SPS_NAL_TYPE {
                return true;
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    false
}

/// Whether a payload list actually carries the parameter sets, as a STAP-A aggregate or as a
/// bare SPS.
fn payloads_carry_parameter_sets(payloads: &[Bytes]) -> bool {
    const STAP_A_NAL_TYPE: u8 = 24;
    const SPS_NAL_TYPE: u8 = 7;
    payloads.iter().any(|p| {
        matches!(
            p.first().map(|b| b & 0x1f),
            Some(STAP_A_NAL_TYPE) | Some(SPS_NAL_TYPE)
        )
    })
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
    /// Two conditions, both required: the client said it can parse the format, and the link is
    /// actually losing packets. A client that cannot parse it must never be sent it, and a
    /// clean link should not pay for it.
    fn redundancy_now(&self) -> bool {
        if !self.redundancy {
            return false;
        }
        self.link_loss
            .as_ref()
            .map(|l| l.load(Ordering::Relaxed) >= AUDIO_REDUNDANCY_LOSS_THRESHOLD)
            .unwrap_or(false)
    }

    fn audio_enabled_now(&self) -> bool {
        self.overrides
            .lock()
            .unwrap()
            .get(&self.device_key)
            .map(|o| o.audio_enabled)
            .unwrap_or(false)
    }
}

fn h264_fmtp(profile_level_id: &str) -> String {
    format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}")
}

fn h264_codecs(profile: H264Profile) -> Vec<(PayloadType, PayloadType, &'static str)> {
    match profile {
        H264Profile::Baseline => vec![(102, 103, "42e01f")],
        H264Profile::Main => vec![(102, 103, "4d001f")],
        H264Profile::High => vec![(102, 103, "640c1f"), (104, 105, "64001f")],
    }
}

fn build_api(profile: H264Profile) -> Result<webrtc::api::API> {
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
        .with_setting_engine(udp_only_setting_engine())
        .build())
}

fn udp_only_setting_engine() -> webrtc::api::setting_engine::SettingEngine {
    use webrtc::ice::network_type::NetworkType;

    let mut se = webrtc::api::setting_engine::SettingEngine::default();
    se.set_network_types(vec![NetworkType::Udp4, NetworkType::Udp6]);
    se.set_srflx_acceptance_min_wait(Some(std::time::Duration::from_millis(50)));
    se.set_prflx_acceptance_min_wait(Some(std::time::Duration::from_millis(50)));
    se.set_relay_acceptance_min_wait(Some(std::time::Duration::from_millis(0)));
    se
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLocality {
    SameNetwork,
    CrossNetwork,
    Unknown,
}

async fn detect_session_locality(
    pc: &webrtc::peer_connection::RTCPeerConnection,
) -> SessionLocality {
    use std::collections::HashMap;

    use webrtc::stats::StatsReportType;

    let report = pc.get_stats().await;

    let mut cand_type: HashMap<String, String> = HashMap::new();
    let mut cand_ip: HashMap<String, String> = HashMap::new();
    let mut nominated: Option<(String, String)> = None;
    for stat in report.reports.values() {
        match stat {
            StatsReportType::LocalCandidate(c) | StatsReportType::RemoteCandidate(c) => {
                cand_type.insert(c.id.clone(), format!("{:?}", c.candidate_type));
                cand_ip.insert(c.id.clone(), c.ip.clone());
            }
            StatsReportType::CandidatePair(p) if p.nominated => {
                nominated = Some((p.local_candidate_id.clone(), p.remote_candidate_id.clone()));
            }
            _ => {}
        }
    }

    let Some((lid, rid)) = nominated else {
        return SessionLocality::Unknown;
    };
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
        SessionLocality::CrossNetwork
    } else if lt.contains("host") && rt.contains("host") {
        SessionLocality::SameNetwork
    } else if lt.is_empty() && rt.is_empty() {
        SessionLocality::Unknown
    } else {
        SessionLocality::CrossNetwork
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
}

impl SessionHandle {
    /// Close the peer connection and wait for the injector thread to release its input state.
    pub async fn close(mut self) {
        if let Err(e) = self.pc.close().await {
            teprintln!("closing peer connection: {e}");
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

pub async fn handle_whep_offer(
    offer_sdp: String,
    pipeline: &Pipeline,
    ice_servers: Vec<RTCIceServer>,
    closed_tx: Option<tokio::sync::oneshot::Sender<()>>,
    input: InputParams,
    audio: Option<AudioParams>,
    locality: SessionLocality,
) -> Result<(String, SessionHandle)> {
    let profile = pipeline.h264_profile;
    let api = build_api(profile)?;

    let offered: Vec<String> = offer_sdp
        .split("profile-level-id=")
        .skip(1)
        .map(|s| s.chars().take(6).collect())
        .collect();
    tprintln!("WHEP offer H.264 profile-level-ids (configured={profile:?}, offered={offered:?})");

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

    let keyframe_gate = Arc::new(KeyframeGate::new());
    let keyframes = KeyframeRequester {
        pipeline: pipeline.clone(),
        gate: Arc::clone(&keyframe_gate),
    };
    let feedback = Arc::new(SharedFeedback::default());
    let clock_epoch = Instant::now();
    *feedback.history.lock().unwrap() = Some(SendHistory::new(clock_epoch));
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
                    .then(|| control_rx.lock().unwrap().take())
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

    let track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: h264_fmtp(h264_codecs(profile)[0].2),
            rtcp_feedback: vec![],
        },
        "video".to_owned(),
        "webrtc-streamer".to_owned(),
    ));

    let rtp_sender = pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
        .await
        .context("add_track")?;

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
                            let mut guard = feedback.history.lock().unwrap();
                            match guard.as_mut() {
                                Some(h) => congestion::interpret(fb, h, &mut deltas),
                                None => continue,
                            }
                        };
                        if summary.reported > 0 {
                            feedback.record_report(&summary);
                        }
                        if !deltas.is_empty() {
                            let mut det = feedback.detector.lock().unwrap();
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
        if let Err(e) = setup_audio(&pc, audio, Arc::clone(&pipeline.telemetry)).await {
            teprintln!("audio: setup failed ({e:#}); continuing without audio");
        }
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
            let mut payloader = H264Payloader::default();
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
                *feedback.history.lock().unwrap() = None;
            }
            loop {
                match rx.recv().await {
                    Ok(frame) => {
                        let mut timing = frame.timing;
                        let rtp_ts =
                            audio::host_ns_to_rtp90k(audio::host_instant_to_ns(frame.capture));
                        let payloads = match payloader.payload(mtu, &frame.data) {
                            Ok(p) => p,
                            Err(e) => {
                                keyframes.request(&format!("h264 payload failed ({e})"));
                                continue;
                            }
                        };
                        if payloads.is_empty() {
                            // A parameter-sets-only access unit produces this: `H264Payloader`
                            // stashes SPS and PPS rather than emitting them. Asking for another
                            // IDR would only produce another one, so the gate is what stops the
                            // spin.
                            keyframes.request_quietly();
                            continue;
                        }
                        // `H264Payloader` stashes SPS and PPS and re-emits them as a STAP-A
                        // aggregate prepended to the next VCL NAL — but only if the aggregate
                        // fits the MTU. If it does not it is dropped with no error and the
                        // stashed copies are cleared anyway, so they are never retried. With an
                        // infinite GOP that is a permanent black screen with traffic flowing, and
                        // it looks like a healthy session from every host-side signal. It cannot
                        // be fixed from out here, but it can stop being silent.
                        if access_unit_has_sps(&frame.data)
                            && !payloads_carry_parameter_sets(&payloads)
                            && !warned_dropped_params
                        {
                            warned_dropped_params = true;
                            teprintln!(
                                "H.264 parameter sets did not fit the {mtu}-byte MTU and were dropped; \
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
                        for (i, payload) in payloads.into_iter().enumerate() {
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
                                    let mut guard = feedback.history.lock().unwrap();
                                    guard.as_mut().map(|h| h.record(wire_len as u32, now))
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
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let Some(pc) = pc.upgrade() else { return };
                    let observed = detect_session_locality(&pc).await;
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
            if let Some(tx) = closed_tx.lock().unwrap().take() {
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
                        if let Some(tx) = closed_tx.lock().unwrap().take() {
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

    Ok((
        local.sdp,
        SessionHandle {
            pc,
            input_tx: Some(input_tx),
            input_join: Some(input_join),
        },
    ))
}

async fn setup_audio(
    pc: &Arc<webrtc::peer_connection::RTCPeerConnection>,
    audio: AudioParams,
    telemetry: crate::streamer::telemetry::SharedTelemetry,
) -> Result<()> {
    // On the fast path the Opus track is added but never fed: its whole job is to exist in the
    // answer so the client has an `m=audio` section to fall back onto if its WebCodecs decoder
    // dies mid-session. WHEP has no renegotiation verb, so a track that is not in the first
    // answer can never be added later.
    if !audio.fast || audio.fallback_track {
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
        if audio.fast {
            tprintln!("audio: added a standard Opus track as an unfed fallback for the client");
        } else {
            tprintln!("audio: added standard Opus track (NetEQ fallback path)");
            tokio::spawn(forward_track(
                Arc::clone(pc),
                track,
                audio.clone(),
                Arc::clone(&telemetry),
            ));
        }
    }

    if audio.fast {
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
    Ok(())
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

            keyframe_gate.observe_rtt_ms(max_rtt_ms);
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
            let loss = twcc_loss.map(|(l, _)| l).unwrap_or(fraction_lost);

            let delay_state = {
                let det = feedback.detector.lock().unwrap();
                if det.samples() > 0 {
                    Some(det.state())
                } else {
                    None
                }
            };

            let mut estimate = estimate_from_loss(estimate_bps, measured_send_bps, loss);

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
            } else if rtt_monitor.observe(max_rtt_ms) {
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

    fn annex_b(nal_types: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for t in nal_types {
            out.extend_from_slice(&[0, 0, 0, 1, *t]);
            out.extend_from_slice(&[0xAA; 8]);
        }
        out
    }

    #[test]
    fn a_keyframe_access_unit_is_recognised_by_its_parameter_sets() {
        // AUD, SPS, PPS, IDR — the shape NVENC and x264 both emit.
        assert!(access_unit_has_sps(&annex_b(&[9, 7, 8, 5])));
        assert!(access_unit_has_sps(&annex_b(&[7, 8, 5])));
    }

    #[test]
    fn a_p_frame_is_not_mistaken_for_one() {
        assert!(!access_unit_has_sps(&annex_b(&[1])));
        assert!(!access_unit_has_sps(&annex_b(&[9, 6, 1])));
        assert!(!access_unit_has_sps(&[]));
        assert!(!access_unit_has_sps(&[0, 0, 1]));
    }

    #[test]
    fn parameter_sets_are_detected_in_either_form_the_payloader_emits() {
        // STAP-A aggregate (24), and a bare SPS (7) for the split case.
        assert!(payloads_carry_parameter_sets(&[Bytes::from_static(&[
            24, 0, 1
        ])]));
        assert!(payloads_carry_parameter_sets(&[
            Bytes::from_static(&[28, 0]),
            Bytes::from_static(&[7, 0]),
        ]));
    }

    #[test]
    fn an_idr_with_no_parameter_sets_is_flagged() {
        // FU-A fragments only: exactly what the payloader emits when the STAP-A did not fit,
        // and what used to leave a client permanently black with no host-side signal.
        let fua = [
            Bytes::from_static(&[28, 0x85]),
            Bytes::from_static(&[28, 0x05]),
        ];
        assert!(!payloads_carry_parameter_sets(&fua));
    }
}

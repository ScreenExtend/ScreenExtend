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

const OPUS_FALLBACK_FMTP: &str = "minptime=5;useinbandfec=1;stereo=1;sprop-stereo=1";
const AUDIO_BACKPRESSURE_BYTES: usize = 16 * 1024;

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

pub struct InputParams {
    pub device: Option<String>,
    pub control_enabled: bool,
    pub control_rx: Option<session::ControlReceiver>,
}

#[derive(Clone)]
pub struct AudioParams {
    pub fast: bool,
    pub hub: SharedAudioHub,
    pub overrides: SharedDeviceOverrides,
    pub device_key: String,
}

impl AudioParams {
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

    let keyframe_gate = Arc::new(KeyframeGate::new());
    let feedback = Arc::new(SharedFeedback::default());
    let clock_epoch = Instant::now();
    *feedback.history.lock().unwrap() = Some(SendHistory::new(clock_epoch));
    // The pacer refills from the same target the encoder is given, so the two never disagree
    // about how much this session is allowed to send.
    let pacing_target_bps = Arc::new(std::sync::atomic::AtomicU32::new(pipeline.max_bitrate_bps));

    let InputParams {
        device: input_device,
        control_enabled,
        control_rx,
    } = input;
    let (input_tx, input_join) = input::spawn(input_device);
    {
        let input_tx = input_tx.clone();
        let control_rx = Arc::new(Mutex::new(control_rx));
        let session_telemetry = Arc::clone(&pipeline.telemetry);
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let session_telemetry = Arc::clone(&session_telemetry);
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
                let telemetry_for_stats = Arc::clone(&session_telemetry);
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let tx = tx.clone();
                    let dc_pong = dc_for_pong.clone();
                    let telemetry = Arc::clone(&telemetry_for_stats);
                    Box::pin(async move {
                        let Some((ev, hot)) = input::protocol::parse(&msg.data) else {
                            return;
                        };
                        // Both of these are transport business and are answered before the
                        // remote-control gate: a device with control disabled still needs its
                        // clock synced and still reports what its own pipeline is doing.
                        if let input::protocol::InputEvent::ClientStats(stats) = ev {
                            telemetry.client.set(Some(*stats));
                            return;
                        }
                        if let Some(dc_pong) = &dc_pong {
                            if let input::protocol::InputEvent::Ping { t_ns } = ev {
                                let pong = input::protocol::build_pong(
                                    t_ns,
                                    crate::streamer::audio::host_now_ns(),
                                );
                                let _ = dc_pong.send(&Bytes::copy_from_slice(&pong)).await;
                                return;
                            }
                        }
                        if !control_enabled {
                            return;
                        }
                        tx.route(ev, hot);
                    })
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
        let keyframe_gate = Arc::clone(&keyframe_gate);
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
                        if keyframe_gate.allow(audio::host_now_ns()) {
                            tprintln!("RTCP keyframe request, requesting IDR");
                            pipeline.request_idr();
                        }
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

    spawn_bitrate_driver(
        Arc::clone(&pc),
        pipeline.clone(),
        Arc::clone(&feedback),
        Arc::clone(&keyframe_gate),
        Arc::clone(&pacing_target_bps),
    );

    if let Some(audio) = audio {
        if let Err(e) = setup_audio(&pc, audio, Arc::clone(&pipeline.telemetry)).await {
            teprintln!("audio: setup failed ({e:#}); continuing without audio");
        }
    }

    {
        let mut rx = pipeline.tx.subscribe();
        let track = Arc::clone(&track);
        let pc_keepalive = Arc::clone(&pc);
        let pipeline = pipeline.clone();
        let keyframe_gate = Arc::clone(&keyframe_gate);
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
            let mut extensions = vec![
                webrtc::rtp::extension::HeaderExtension::PlayoutDelay(
                    webrtc::rtp::extension::playout_delay_extension::PlayoutDelayExtension::new(
                        0,
                        playout_delay_max,
                    ),
                ),
                webrtc::rtp::extension::HeaderExtension::TransportCc(
                    webrtc::rtp::extension::transport_cc_extension::TransportCcExtension::default(),
                ),
            ];
            loop {
                match rx.recv().await {
                    Ok(frame) => {
                        let mut timing = frame.timing;
                        let rtp_ts =
                            audio::host_ns_to_rtp90k(audio::host_instant_to_ns(frame.capture));
                        let payloads = match payloader.payload(mtu, &frame.data) {
                            Ok(p) => p,
                            Err(e) => {
                                tprintln!("h264 payload failed ({e})");
                                if keyframe_gate.allow(audio::host_now_ns()) {
                                    pipeline.request_idr();
                                }
                                continue;
                            }
                        };
                        if payloads.is_empty() {
                            // A parameter-sets-only access unit produces this: `H264Payloader`
                            // stashes SPS and PPS rather than emitting them. Asking for another
                            // IDR would only produce another one, so the gate is what stops the
                            // spin.
                            if keyframe_gate.allow(audio::host_now_ns()) {
                                pipeline.request_idr();
                            }
                            continue;
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
                            let wait = pacer.schedule(wire_len, Instant::now());
                            if !wait.is_zero() {
                                tokio::time::sleep(wait).await;
                            }
                            let now = Instant::now();
                            // Stamp the transport-wide sequence number ourselves. The stock
                            // `twcc::sender::Sender` interceptor would do the stamping but keeps
                            // its counter private, and without the packet-to-sequence mapping the
                            // feedback that comes back cannot be turned into a delay gradient.
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
                        if keyframe_gate.allow(audio::host_now_ns()) {
                            tprintln!(
                                "viewer lagged (skipped={skipped}); requesting IDR to resync"
                            );
                            pipeline.request_idr();
                        }
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
    let pipeline_state = pipeline.clone();
    let keyframe_gate_state = Arc::clone(&keyframe_gate);
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
            if keyframe_gate_state.allow(audio::host_now_ns()) {
                pipeline_state.request_idr();
            }
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
    } else {
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
        tprintln!("audio: added standard Opus track (NetEQ fallback path)");
        tokio::spawn(forward_track(Arc::clone(pc), track, audio, telemetry));
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
    let mut since_bp_check: u32 = 0;
    let mut over_backpressure = false;
    let mut scratch: Vec<u8> = Vec::with_capacity(protocol::HEADER_LEN + 512);
    loop {
        if !pc_alive(&pc) || dc.ready_state() == RTCDataChannelState::Closed {
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
                if since_bp_check == 0 {
                    over_backpressure = dc.buffered_amount().await > AUDIO_BACKPRESSURE_BYTES;
                }
                since_bp_check = (since_bp_check + 1) % 4;
                if over_backpressure {
                    s.diagnostics
                        .dropped_backpressure
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                scratch.resize(protocol::HEADER_LEN + pkt.data.len(), 0);
                protocol::write_message(
                    &mut scratch,
                    pkt.seq,
                    pkt.capture_ns,
                    pkt.flags,
                    &pkt.data,
                );
                if dc.send(&Bytes::copy_from_slice(&scratch)).await.is_err() {
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
    let frame_duration = Duration::from_millis(5); // 5 ms Opus frames
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
                if track
                    .write_sample(&Sample {
                        data: pkt.data,
                        duration: frame_duration,
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

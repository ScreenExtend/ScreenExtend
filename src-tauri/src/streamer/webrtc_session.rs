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
use super::bitrate::{estimate_from_loss, BitrateController, DEFAULT_MIN_BITRATE_BPS};
use super::config::H264Profile;
use super::input;
use super::pipeline::Pipeline;
use super::session::SharedDeviceOverrides;

const BWE_POLL_INTERVAL: Duration = Duration::from_millis(120);

const OPUS_FALLBACK_FMTP: &str = "minptime=5;useinbandfec=1;stereo=1;sprop-stereo=1";
const AUDIO_BACKPRESSURE_BYTES: usize = 16 * 1024;

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

pub async fn handle_whep_offer(
    offer_sdp: String,
    pipeline: &Pipeline,
    ice_servers: Vec<RTCIceServer>,
    closed_tx: Option<tokio::sync::oneshot::Sender<()>>,
    input_device: Option<String>,
    control_enabled: bool,
    audio: Option<AudioParams>,
) -> Result<String> {
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

    let (input_tx, _input_join) = input::spawn(input_device);
    {
        let input_tx = input_tx.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let label = dc.label().to_string();
            if !matches!(label.as_str(), "fast" | "reliable" | "bulk") {
                return Box::pin(async {});
            }
            let is_reliable = label == "reliable";
            let tx = input_tx.clone();
            let dc = Arc::clone(&dc);
            Box::pin(async move {
                tprintln!("remote-input data channel open: {label}");
                let dc_for_pong = is_reliable.then(|| Arc::clone(&dc));
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let tx = tx.clone();
                    let dc_pong = dc_for_pong.clone();
                    Box::pin(async move {
                        let Some((ev, hot)) = input::protocol::parse(&msg.data) else {
                            return;
                        };
                        if let Some(dc_pong) = &dc_pong {
                            if let input::protocol::InputEvent::Ping { t_ns } = ev {
                                let pong = input::protocol::build_pong(t_ns);
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
        tokio::spawn(async move {
            use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
            use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
            let mut rtcp_buf = vec![0u8; 1500];
            while let Ok((packets, _attrs)) = rtp_sender.read(&mut rtcp_buf).await {
                for p in &packets {
                    let any = p.as_any();
                    if any.downcast_ref::<PictureLossIndication>().is_some() {
                        tprintln!("RTCP PLI received, requesting IDR");
                        pipeline.request_idr();
                    } else if any.downcast_ref::<FullIntraRequest>().is_some() {
                        tprintln!("RTCP FIR received, requesting IDR");
                        pipeline.request_idr();
                    }
                }
            }
        });
    }

    spawn_bitrate_driver(Arc::clone(&pc), pipeline.clone());

    if let Some(audio) = audio {
        if let Err(e) = setup_audio(&pc, audio).await {
            teprintln!("audio: setup failed ({e:#}); continuing without audio");
        }
    }

    {
        let mut rx = pipeline.tx.subscribe();
        let track = Arc::clone(&track);
        let pc_keepalive = Arc::clone(&pc);
        let pipeline = pipeline.clone();
        tokio::spawn(async move {
            let _pc = pc_keepalive;
            const RTP_PAYLOAD_MTU: usize = 1200 - 12;
            let mut payloader = H264Payloader::default();
            let sequencer = new_random_sequencer();
            let playout_delay = [webrtc::rtp::extension::HeaderExtension::PlayoutDelay(
                webrtc::rtp::extension::playout_delay_extension::PlayoutDelayExtension::new(0, 0),
            )];
            loop {
                match rx.recv().await {
                    Ok(frame) => {
                        let rtp_ts =
                            audio::host_ns_to_rtp90k(audio::host_instant_to_ns(frame.capture));
                        let payloads = match payloader.payload(RTP_PAYLOAD_MTU, &frame.data) {
                            Ok(p) => p,
                            Err(e) => {
                                tprintln!("h264 payload failed ({e}); requesting IDR");
                                pipeline.request_idr();
                                continue;
                            }
                        };
                        if payloads.is_empty() {
                            pipeline.request_idr();
                            continue;
                        }
                        let last = payloads.len().saturating_sub(1);
                        let mut stop = false;
                        for (i, payload) in payloads.into_iter().enumerate() {
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
                            if let Err(e) =
                                track.write_rtp_with_extensions(&pkt, &playout_delay).await
                            {
                                tprintln!("track write_rtp failed ({e}); viewer writer stopping");
                                stop = true;
                                break;
                            }
                        }
                        if stop {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tprintln!("viewer lagged (skipped={skipped}); requesting IDR to resync");
                        pipeline.request_idr();
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

    let pc_state = Arc::clone(&pc);
    let locality_logged = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let closed_tx = Arc::new(Mutex::new(closed_tx));
    let input_tx_state = input_tx.clone();
    let (grace_cancel_tx, grace_cancel_rx) = tokio::sync::watch::channel(false);
    let grace_cancel_tx = Arc::new(grace_cancel_tx);
    let grace_cancel_rx_closure = grace_cancel_rx.clone();
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
            if !locality_logged.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let pc = Arc::clone(&pc_state);
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let locality = detect_session_locality(&pc).await;
                    tprintln!("session locality detected: {locality:?}");
                });
            }
        }
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Disconnected
        ) {
            let pc = Arc::clone(&pc_state);
            tokio::spawn(async move {
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

    pipeline.request_idr();

    let answer = pc.create_answer(None).await.context("create_answer")?;

    let mut gather_complete = pc.gathering_complete_promise().await;
    pc.set_local_description(answer)
        .await
        .context("set_local_description(answer)")?;
    const GATHER_MAX_WAIT: Duration = Duration::from_millis(1500);
    match tokio::time::timeout(GATHER_MAX_WAIT, gather_complete.recv()).await {
        Ok(_) => {}
        Err(_) => tprintln!(
            "ICE gathering still in progress after {GATHER_MAX_WAIT:?}; returning answer with candidates gathered so far"
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

    Ok(local.sdp)
}

async fn setup_audio(
    pc: &Arc<webrtc::peer_connection::RTCPeerConnection>,
    audio: AudioParams,
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
        let pc_for_open = Arc::clone(pc);
        dc.on_open(Box::new(move || {
            let dc = Arc::clone(&dc_for_open);
            let pc = Arc::clone(&pc_for_open);
            let audio = audio.clone();
            Box::pin(async move {
                tokio::spawn(forward_datachannel(pc, dc, audio));
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
        tokio::spawn(forward_track(Arc::clone(pc), track, audio));
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
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
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
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
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

fn spawn_bitrate_driver(pc: Arc<webrtc::peer_connection::RTCPeerConnection>, pipeline: Pipeline) {
    use webrtc::stats::StatsReportType;

    const RTT_CONGESTION_THRESHOLD_MS: f64 = 80.0;

    let max_bps = pipeline.max_bitrate_bps;
    let mut controller = BitrateController::new(DEFAULT_MIN_BITRATE_BPS.min(max_bps), max_bps);
    let mut current_target = max_bps;
    let mut last_bytes_sent: Option<(u64, Instant)> = None;

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
                        if let Some(rtt_s) = s.round_trip_time {
                            max_rtt_ms = max_rtt_ms.max(rtt_s * 1000.0);
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

            let mut estimate = estimate_from_loss(current_target, measured_send_bps, fraction_lost);

            if max_rtt_ms > RTT_CONGESTION_THRESHOLD_MS {
                let rtt_estimate = (current_target as f64 * 0.85) as u32;
                if rtt_estimate < estimate {
                    tprintln!(
                        "BWE: RTT congestion signal (rtt_ms={max_rtt_ms:.1} > {RTT_CONGESTION_THRESHOLD_MS}ms); \
                         clamping estimate {estimate} -> {rtt_estimate}"
                    );
                    estimate = rtt_estimate;
                }
            }

            if let Some(target) = controller.update(estimate, now) {
                tprintln!(
                    "BWE: pushing adaptive bitrate target (measured_send_bps={measured_send_bps}, \
                     fraction_lost={fraction_lost}, rtt_ms={max_rtt_ms:.1}, estimate={estimate}, target={target})"
                );
                current_target = target;
                pipeline.set_target_bitrate(target);
            }
        }
        tprintln!("bitrate driver stopped (peer connection closed)");
    });
}

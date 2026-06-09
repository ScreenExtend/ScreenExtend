use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_remote::TrackRemote;

use crate::streamer::config::{Config, H264Profile};

const MIN_RTP_PACKETS: u64 = 30;
const TIMEOUT_SECS: u64 = 10;

pub async fn run(mut config: Config) -> Result<()> {
    config.whep_selftest = false;
    config.h264_profile = H264Profile::Baseline;
    if config.port == 8080 {
        config.port = 18080;
    }
    if config.https_port == 8443 {
        config.https_port = 18443;
    }
    let port = config.https_port;

    println!("=== M3 WHEP self-test ===");
    println!("[selftest] starting in-process server (HTTP :{}, HTTPS :{port})", config.port);

    let server_cfg = config.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = crate::streamer::server::run(server_cfg, None).await {
            eprintln!("[selftest] server exited with error: {e:?}");
        }
    });

    wait_for_health(port).await?;
    println!("[selftest] server healthy");

    let api = {
        use webrtc::api::media_engine::MIME_TYPE_H264;
        use webrtc::rtp_transceiver::RTCPFeedback;
        use webrtc::rtp_transceiver::rtp_codec::{
            RTCRtpCodecCapability, RTCRtpCodecParameters,
        };

        let mut m = MediaEngine::default();
        let fb = vec![
            RTCPFeedback { typ: "goog-remb".to_owned(), parameter: "".to_owned() },
            RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
            RTCPFeedback { typ: "nack".to_owned(), parameter: "".to_owned() },
            RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
            RTCPFeedback { typ: "transport-cc".to_owned(), parameter: "".to_owned() },
        ];
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .to_owned(),
                    rtcp_feedback: fb,
                },
                payload_type: 102,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/rtx".to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "apt=102".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 103,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)?;
        let mut se = webrtc::api::setting_engine::SettingEngine::default();
        se.set_network_types(vec![
            webrtc::ice::network_type::NetworkType::Udp4,
            webrtc::ice::network_type::NetworkType::Udp6,
        ]);
        APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .with_setting_engine(se)
            .build()
    };

    let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);

    pc.add_transceiver_from_kind(
        RTPCodecType::Video,
        Some(webrtc::rtp_transceiver::RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await?;

    let rtp_count = Arc::new(AtomicU64::new(0));
    let track_seen = Arc::new(AtomicU64::new(0));

    {
        let rtp_count = Arc::clone(&rtp_count);
        let track_seen = Arc::clone(&track_seen);
        pc.on_track(Box::new(move |track: Arc<TrackRemote>, _recv, _trans| {
            let codec = track.codec();
            println!(
                "[selftest] on_track fired: mime={} payload_type={}",
                codec.capability.mime_type, track.payload_type()
            );
            track_seen.fetch_add(1, Ordering::Relaxed);
            let rtp_count = Arc::clone(&rtp_count);
            Box::pin(async move {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1600];
                    loop {
                        match track.read(&mut buf).await {
                            Ok((pkt, _)) => {
                                let n = rtp_count.fetch_add(1, Ordering::Relaxed) + 1;
                                if n == 1 || n % 30 == 0 {
                                    println!(
                                        "[selftest] RTP #{n} seq={} ts={} payload={}B",
                                        pkt.header.sequence_number,
                                        pkt.header.timestamp,
                                        pkt.payload.len()
                                    );
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            })
        }));
    }

    pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        println!("[selftest] client PC state: {s:?}");
        Box::pin(async {})
    }));

    let offer = pc.create_offer(None).await?;
    let mut gather = pc.gathering_complete_promise().await;
    pc.set_local_description(offer).await?;
    let _ = gather.recv().await;
    let offer_sdp = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no local description"))?
        .sdp;

    println!("[selftest] POST /whep ({} bytes)", offer_sdp.len());
    let answer_sdp = http_post_sdp(port, &offer_sdp).await?;
    println!("[selftest] got answer SDP ({} bytes)", answer_sdp.len());

    let checks = [
        ("H264", answer_sdp.contains("H264")),
        ("packetization-mode=1", answer_sdp.contains("packetization-mode=1")),
        ("profile-level-id=42e01f", answer_sdp.contains("profile-level-id=42e01f")),
        ("rtcp-fb nack", answer_sdp.contains("a=rtcp-fb") && answer_sdp.contains("nack")),
        ("rtcp-fb nack pli", answer_sdp.contains("nack pli")),
        ("rtcp-fb ccm fir", answer_sdp.contains("ccm fir")),
        ("transport-cc", answer_sdp.contains("transport-cc")),
        ("rtx", answer_sdp.contains("rtx")),
    ];
    println!("[selftest] --- SDP attribute checks ---");
    let mut all_ok = true;
    for (name, ok) in checks {
        println!("[selftest]   {} {}", if ok { "PASS" } else { "FAIL" }, name);
        all_ok &= ok;
    }
    if let Some(fmtp) = answer_sdp.lines().find(|l| l.contains("profile-level-id")) {
        println!("[selftest] negotiated fmtp: {}", fmtp.trim());
    }
    if !all_ok {
        eprintln!("---- answer SDP ----\n{answer_sdp}\n--------------------");
        bail!("SDP attribute checks failed");
    }

    pc.set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await?;
    println!("[selftest] answer applied; waiting up to {TIMEOUT_SECS}s for >= {MIN_RTP_PACKETS} RTP packets");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(TIMEOUT_SECS);
    while tokio::time::Instant::now() < deadline {
        if rtp_count.load(Ordering::Relaxed) >= MIN_RTP_PACKETS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let final_count = rtp_count.load(Ordering::Relaxed);
    let tracks = track_seen.load(Ordering::Relaxed);
    println!("[selftest] on_track fired: {tracks}; RTP packets received: {final_count}");

    pc.close().await.ok();
    server_handle.abort();

    if tracks == 0 {
        bail!("on_track never fired — no media track negotiated");
    }
    if final_count < MIN_RTP_PACKETS {
        bail!("only {final_count} RTP packets in {TIMEOUT_SECS}s (need >= {MIN_RTP_PACKETS})");
    }

    println!("=== M3 WHEP self-test PASSED ({final_count} RTP packets) ===");
    Ok(())
}

async fn wait_for_health(port: u16) -> Result<()> {
    let req = format!(
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    for _ in 0..100 {
        if let Ok(resp) = tls_send_recv(port, req.as_bytes()).await {
            if String::from_utf8_lossy(&resp).contains("200") {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("server did not become healthy in time")
}

async fn http_post_sdp(port: u16, sdp: &str) -> Result<String> {
    let req = format!(
        "POST /whep HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        sdp.len(),
        sdp
    );
    let raw = tls_send_recv(port, req.as_bytes()).await?;
    let text = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response (no header/body split)"))?;
    let status_line = head.lines().next().unwrap_or("");
    if !status_line.contains("200") {
        bail!("POST /whep returned: {status_line}\nbody: {body}");
    }
    Ok(body.to_string())
}

async fn tls_send_recv(port: u16, request: &[u8]) -> Result<Vec<u8>> {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::pki_types::ServerName;

    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let client_config = tokio_rustls::rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(danger::NoVerify))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let domain = ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(domain, tcp).await?;

    tls.write_all(request).await?;
    let mut raw = Vec::new();
    tls.read_to_end(&mut raw).await?;
    Ok(raw)
}

mod danger {
    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature};
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tokio_rustls::rustls::{DigitallySignedStruct, Error, SignatureScheme};

    #[derive(Debug)]
    pub struct NoVerify;

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &ring::default_provider().signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &ring::default_provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

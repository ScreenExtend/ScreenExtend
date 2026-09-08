use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
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

/// Credentials the self-test hands itself. `/whep` requires a session id and OTP like any other
/// join; the caller installs the same pair into the config's `SessionAuth`.
pub const SELFTEST_SESSION_ID: &str = "SELFTESTSESSN";
pub const SELFTEST_OTP: &str = "000000";

/// The attributes an answer must carry for a browser to decode this stream at all.
///
/// Split out from the live run so the SDP half is testable without a display, an encoder, or a
/// network: the live run feeds it a real answer, and `answer_sdp_checks_catch_a_stripped_answer`
/// feeds it a captured one.
pub fn answer_sdp_checks(answer_sdp: &str) -> Vec<(&'static str, bool)> {
    vec![
        ("H264", answer_sdp.contains("H264")),
        (
            "packetization-mode=1",
            answer_sdp.contains("packetization-mode=1"),
        ),
        (
            "profile-level-id=42e01f",
            answer_sdp.contains("profile-level-id=42e01f"),
        ),
        (
            "rtcp-fb nack",
            answer_sdp.contains("a=rtcp-fb") && answer_sdp.contains("nack"),
        ),
        ("rtcp-fb nack pli", answer_sdp.contains("nack pli")),
        ("rtcp-fb ccm fir", answer_sdp.contains("ccm fir")),
        ("transport-cc", answer_sdp.contains("transport-cc")),
        ("rtx", answer_sdp.contains("rtx")),
    ]
}

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

    tprintln!("=== M3 WHEP self-test ===");
    tprintln!(
        "[selftest] starting in-process server (HTTP :{}, HTTPS :{port})",
        config.port
    );

    let server_cfg = config.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = crate::streamer::server::run(server_cfg, None).await {
            teprintln!("[selftest] server exited with error: {e:?}");
        }
    });

    wait_for_health(port).await?;
    tprintln!("[selftest] server healthy");

    let api = {
        use webrtc::api::media_engine::MIME_TYPE_H264;
        use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters};
        use webrtc::rtp_transceiver::RTCPFeedback;

        let mut m = MediaEngine::default();
        let fb = vec![
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
            tprintln!(
                "[selftest] on_track fired: mime={} payload_type={}",
                codec.capability.mime_type,
                track.payload_type()
            );
            track_seen.fetch_add(1, Ordering::Relaxed);
            let rtp_count = Arc::clone(&rtp_count);
            Box::pin(async move {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1600];
                    while let Ok((pkt, _)) = track.read(&mut buf).await {
                        let n = rtp_count.fetch_add(1, Ordering::Relaxed) + 1;
                        if n == 1 || n.is_multiple_of(30) {
                            tprintln!(
                                "[selftest] RTP #{n} seq={} ts={} payload={}B",
                                pkt.header.sequence_number,
                                pkt.header.timestamp,
                                pkt.payload.len()
                            );
                        }
                    }
                });
            })
        }));
    }

    pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        tprintln!("[selftest] client PC state: {s:?}");
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

    tprintln!("[selftest] POST /whep ({} bytes)", offer_sdp.len());
    let answer_sdp = http_post_join(port, &offer_sdp).await?;
    tprintln!("[selftest] got answer SDP ({} bytes)", answer_sdp.len());

    let checks = answer_sdp_checks(&answer_sdp);
    tprintln!("[selftest] --- SDP attribute checks ---");
    let mut all_ok = true;
    for (name, ok) in checks {
        tprintln!("[selftest]   {} {}", if ok { "PASS" } else { "FAIL" }, name);
        all_ok &= ok;
    }
    if let Some(fmtp) = answer_sdp.lines().find(|l| l.contains("profile-level-id")) {
        tprintln!("[selftest] negotiated fmtp: {}", fmtp.trim());
    }
    if !all_ok {
        teprintln!("---- answer SDP ----\n{answer_sdp}\n--------------------");
        bail!("SDP attribute checks failed");
    }

    pc.set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await?;
    tprintln!("[selftest] answer applied; waiting up to {TIMEOUT_SECS}s for >= {MIN_RTP_PACKETS} RTP packets");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(TIMEOUT_SECS);
    while tokio::time::Instant::now() < deadline {
        if rtp_count.load(Ordering::Relaxed) >= MIN_RTP_PACKETS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let final_count = rtp_count.load(Ordering::Relaxed);
    let tracks = track_seen.load(Ordering::Relaxed);
    tprintln!("[selftest] on_track fired: {tracks}; RTP packets received: {final_count}");

    pc.close().await.ok();
    server_handle.abort();

    if tracks == 0 {
        bail!("on_track never fired — no media track negotiated");
    }
    if final_count < MIN_RTP_PACKETS {
        bail!("only {final_count} RTP packets in {TIMEOUT_SECS}s (need >= {MIN_RTP_PACKETS})");
    }

    tprintln!("=== M3 WHEP self-test PASSED ({final_count} RTP packets) ===");
    Ok(())
}

async fn wait_for_health(port: u16) -> Result<()> {
    let req =
        format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
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

/// `/whep` takes a JSON join request, not a bare SDP body. Posting `application/sdp` — which
/// this did — could only ever come back 400, which is why the self-test had never passed.
async fn http_post_join(port: u16, sdp: &str) -> Result<String> {
    let body = serde_json::json!({
        "sessionId": SELFTEST_SESSION_ID,
        "otp": SELFTEST_OTP,
        "deviceName": "whep-selftest",
        "os": "selftest",
        "refreshRate": 60,
        "width": 1280,
        "height": 720,
        "dpr": 1.0,
        "sdp": sdp,
    })
    .to_string();
    let req = format!(
        "POST /whep HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
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
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::TlsConnector;

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

/// A verifier that accepts any certificate.
///
/// The server presents a runtime-generated self-signed certificate, so a real chain check is
/// impossible; this whole module is compiled only under `cfg(test)` or the `probe` feature so
/// nothing that skips certificate validation is ever linked into a shipping binary.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A real answer from this server, captured so the attribute checks can run with no
    /// display, no encoder and no network.
    const SAMPLE_ANSWER: &str = concat!(
        "v=0\n",
        "o=- 7080972874634680579 1710000000 IN IP4 0.0.0.0\n",
        "s=-\n",
        "t=0 0\n",
        "a=group:BUNDLE 0\n",
        "m=video 9 UDP/TLS/RTP/SAVPF 102 103\n",
        "c=IN IP4 0.0.0.0\n",
        "a=rtpmap:102 H264/90000\n",
        "a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\n",
        "a=rtcp-fb:102 goog-remb\n",
        "a=rtcp-fb:102 ccm fir\n",
        "a=rtcp-fb:102 nack\n",
        "a=rtcp-fb:102 nack pli\n",
        "a=rtcp-fb:102 transport-cc\n",
        "a=rtpmap:103 rtx/90000\n",
        "a=fmtp:103 apt=102\n",
        "a=sendonly\n",
    );

    #[test]
    fn a_real_answer_passes_every_attribute_check() {
        for (name, ok) in answer_sdp_checks(SAMPLE_ANSWER) {
            assert!(ok, "{name} failed on a known-good answer");
        }
    }

    #[test]
    fn answer_sdp_checks_catch_a_stripped_answer() {
        // Drop the feedback lines and the rtx codec: the stream would still negotiate and would
        // still be H.264, but loss recovery would be gone and nothing today would notice.
        let stripped: String = SAMPLE_ANSWER
            .lines()
            .filter(|l| !l.starts_with("a=rtcp-fb") && !l.contains("rtx"))
            .collect::<Vec<_>>()
            .join("\n");
        let failed: Vec<&str> = answer_sdp_checks(&stripped)
            .into_iter()
            .filter(|(_, ok)| !ok)
            .map(|(name, _)| name)
            .collect();
        assert!(failed.contains(&"rtcp-fb nack pli"), "got {failed:?}");
        assert!(failed.contains(&"rtcp-fb ccm fir"), "got {failed:?}");
        assert!(failed.contains(&"transport-cc"), "got {failed:?}");
        assert!(failed.contains(&"rtx"), "got {failed:?}");
        // The codec itself is untouched, so those checks must still pass.
        assert!(!failed.contains(&"H264"), "got {failed:?}");
        assert!(!failed.contains(&"packetization-mode=1"), "got {failed:?}");
    }

    #[test]
    fn an_empty_answer_fails_everything() {
        assert!(answer_sdp_checks("").iter().all(|(_, ok)| !ok));
    }
}

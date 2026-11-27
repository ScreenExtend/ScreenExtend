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
use super::keyframe::{
    payload_carries_idr, pli_backoff_cap_healthy_ms, DistanceAsk, KeyframeGate, PacketDistance,
    PliBackoff, PLI_BACKOFF_CAP_MS,
};
use super::lock::LockExt;
use super::negotiate;
use super::pacer::{Pacer, PacingMode};
use super::packetizer::{access_unit_has_sps, payloads_carry_parameter_sets, H264Packetizer};
use super::pipeline::{EncodedFrame, Pipeline};
use super::session::{self, SharedDeviceOverrides};
use super::sockets;

const BWE_POLL_INTERVAL: Duration = Duration::from_millis(120);

/// Floor on how often a client's out-of-band keyframe request may force an IDR.
const MIN_IDR_REQUEST_GAP: Duration = Duration::from_millis(200);

/// No `useinbandfec=1`: libopus documents in-band FEC as an LPC-layer (SILK) feature, and this
/// encoder runs `OPUS_APPLICATION_RESTRICTED_LOWDELAY` at 5 ms frames, both of which force
/// CELT-only, so no FEC can be produced on either transport path.
const OPUS_FALLBACK_FMTP: &str = "minptime=5;stereo=1;sprop-stereo=1";

/// How much audio may sit unsent in the DataChannel before the forwarder starts dropping. In
/// milliseconds, not bytes: a flat 16 KiB is about a second of ~93-byte messages. 40 ms is two
/// Opus frames more than the client's own maximum jitter target.
const AUDIO_BACKPRESSURE_MS: usize = 40;

/// Allowance for what `buffered_amount` counts beyond the queue: bytes sent but not yet SACKed.
///
/// webrtc-sctp 0.17.1 releases a message only in `handle_sack`, and a receiver may hold a SACK
/// for up to 200 ms (RFC 9260 section 6.2). Without this one delayed SACK reads as a full queue
/// and the forwarder drops 41-43 packets (~210 ms of audio) on an idle link.
const AUDIO_SACK_ALLOWANCE_MS: usize = 220;

/// The drop threshold in bytes for the message size actually being sent, so the policy tracks
/// the bitrate instead of assuming one.
fn backpressure_limit_bytes(msg_len: usize) -> usize {
    let frames = (AUDIO_BACKPRESSURE_MS + AUDIO_SACK_ALLOWANCE_MS)
        / crate::streamer::audio::encoder::FRAME_MS as usize;
    frames * msg_len.max(1)
}

/// RTP payload MTU on a same-subnet path: 1200 bytes of datagram minus the 12-byte RTP header.
pub(crate) const RTP_PAYLOAD_MTU_LAN: usize = 1200 - 12;

/// RTP payload MTU on a relayed path. TURN encapsulation on top of SRTP and IP/UDP can push a
/// datagram past a tunnelled path MTU (1400/1420 are common) or the 1280-byte IPv6 minimum, and
/// one lost fragment loses the whole datagram. Moonlight caps remote streams at 1024 too.
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

/// Live sessions, for the leak check: a session that ends must give back its peer connection,
/// its ICE sockets and its injector thread.
static LIVE_SESSIONS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub fn live_sessions() -> i64 {
    LIVE_SESSIONS.load(Ordering::Relaxed)
}

/// The encoder controls a session reaches for.
pub(crate) trait EncoderControl: Send + Sync {
    fn request_idr(&self);
    fn request_repair(&self) -> bool;
    fn set_target_bitrate(&self, bps: u32);
}

impl EncoderControl for Pipeline {
    fn request_idr(&self) {
        Pipeline::request_idr(self)
    }

    fn request_repair(&self) -> bool {
        Pipeline::request_repair(self)
    }

    fn set_target_bitrate(&self, bps: u32) {
        Pipeline::set_target_bitrate(self, bps)
    }
}

/// What a session's tasks and handlers keep of the pipeline: its controls and shared state, never
/// the frame channel's sender. A `Pipeline` clone carries `tx`, and one held by the RTP writer
/// keeps the channel it reads from open, so the writer and its peer connection never go away.
#[derive(Clone)]
pub(crate) struct SessionPipeline {
    control: Arc<dyn EncoderControl>,
    telemetry: crate::streamer::telemetry::SharedTelemetry,
    repair: Arc<crate::streamer::repair::RepairState>,
    max_bitrate_bps: u32,
    frame_duration: Duration,
    #[cfg(test)]
    packet_distance_limit: u32,
}

impl SessionPipeline {
    #[cfg(not(test))]
    fn packet_distance_limit(&self) -> u32 {
        super::keyframe::PACKET_DISTANCE_LIMIT
    }

    #[cfg(test)]
    fn packet_distance_limit(&self) -> u32 {
        self.packet_distance_limit
    }

    /// The same session with the packet distance measured against `limit` instead of 2^15.
    #[cfg(test)]
    pub(crate) fn with_packet_distance_limit(mut self, limit: u32) -> Self {
        self.packet_distance_limit = limit;
        self
    }

    fn of(pipeline: &Pipeline) -> Self {
        let mut control = pipeline.clone();
        control.tx = tokio::sync::broadcast::channel(1).0;
        Self {
            telemetry: Arc::clone(&pipeline.telemetry),
            repair: Arc::clone(&pipeline.repair),
            max_bitrate_bps: pipeline.max_bitrate_bps,
            frame_duration: pipeline.frame_duration,
            control: Arc::new(control),
            #[cfg(test)]
            packet_distance_limit: super::keyframe::PACKET_DISTANCE_LIMIT,
        }
    }

    /// A session with no encoder behind it, for the transport tests.
    #[cfg(test)]
    pub(crate) fn without_encoder(control: Arc<dyn EncoderControl>) -> Self {
        Self::without_encoder_at(control, 8_000_000)
    }

    /// The same, paced to `max_bitrate_bps`.
    #[cfg(test)]
    pub(crate) fn without_encoder_at(
        control: Arc<dyn EncoderControl>,
        max_bitrate_bps: u32,
    ) -> Self {
        Self {
            control,
            telemetry: crate::streamer::telemetry::SessionTelemetry::new_shared(),
            repair: Arc::new(crate::streamer::repair::RepairState::new()),
            max_bitrate_bps,
            frame_duration: Duration::from_millis(16),
            packet_distance_limit: super::keyframe::PACKET_DISTANCE_LIMIT,
        }
    }

    #[cfg(test)]
    pub(crate) fn telemetry(&self) -> crate::streamer::telemetry::SharedTelemetry {
        Arc::clone(&self.telemetry)
    }

    fn request_idr(&self) {
        self.control.request_idr()
    }

    fn request_repair(&self) -> bool {
        self.control.request_repair()
    }

    fn set_target_bitrate(&self, bps: u32) {
        self.control.set_target_bitrate(bps)
    }
}

/// The one rate-limited path every keyframe request in a session takes (RTCP PLI, RTCP FIR, the
/// client opcode, a payloader failure, a lagging viewer). On an infinite-GOP encoder each one
/// produces the largest frame of the session, and one backlog must not be answered five times.
#[derive(Clone)]
pub struct KeyframeRequester {
    pipeline: SessionPipeline,
    gate: Arc<KeyframeGate>,
}

impl KeyframeRequester {
    /// Recover the picture, by the cheapest means the encoder and the client can agree on.
    ///
    /// When the encoder has long-term reference slots *and* transport-wide feedback says the
    /// client holds one, the repair is a P frame predicted from it, not an IDR (23698 B against
    /// 28423 B with every pixel changing). A reference the client never received cannot be
    /// decoded, so both halves are required.
    ///
    /// Returns whether anything was requested; `false` means a recovery is already in flight.
    pub fn request(&self, reason: &str) -> bool {
        if !self.gate.allow(audio::host_now_ns()) {
            return false;
        }
        if self.pipeline.request_repair() {
            tprintln!("picture recovery: {reason} (repairing from a long-term reference, no IDR)");
            return true;
        }
        // tagged only when a keyframe is coming: a repair leaves no request behind
        #[cfg(windows)]
        self.tag(key_origin::LAG);
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
        if !self.pipeline.request_repair() {
            #[cfg(windows)]
            self.tag(key_origin::START);
            self.pipeline.request_idr();
        }
        true
    }

    /// A keyframe, and nothing cleverer, because the **client** asked for one.
    ///
    /// A PLI, a FIR or the client's opcode means *this decoder cannot continue from what it has*.
    /// A long-term-reference repair is the wrong answer: "the client holds this reference" is
    /// inferred from packets arriving, not from anything decoding them, and on every join the
    /// opening IDR is acknowledged before the page creates its decoder. Repairs there gave 35
    /// requests one IDR, and the page demoted itself to a `<video>` element. Losses the
    /// transport detects keep the repair path (`request`): those imply a running decoder.
    pub fn request_keyframe_for_client(&self, reason: &str) -> bool {
        if !self.gate.allow(audio::host_now_ns()) {
            return false;
        }
        #[cfg(windows)]
        self.tag(key_origin::START);
        tprintln!("keyframe requested: {reason}");
        self.pipeline.request_idr();
        true
    }

    /// An IDR because a delta is getting too far from its keyframe for some receivers. The gate
    /// is asked first; a refusal is not counted as a suppressed request (it is retried until it
    /// gets through), and only when the request may force is the window taken anyway.
    pub fn request_keyframe_for_distance(
        &self,
        request: super::keyframe::DistanceRequest,
    ) -> DistanceAsk {
        let asked = self
            .gate
            .ask_for_distance(audio::host_now_ns(), request.may_force);
        match asked {
            DistanceAsk::Admitted => tprintln!(
                "keyframe requested: a delta is near the packet distance older receivers allow"
            ),
            DistanceAsk::Forced if request.first_force => tprintln!(
                "keyframe requested: a delta is near the packet distance older receivers allow \
                 (past the gate)"
            ),
            _ => {}
        }
        if matches!(asked, DistanceAsk::Admitted | DistanceAsk::Forced) {
            #[cfg(windows)]
            self.tag(key_origin::DISTANCE);
            self.pipeline.request_idr();
        }
        asked
    }

    /// The same, without the log line, for the paths a stalled decoder repeats.
    pub fn request_keyframe_for_client_quietly(&self) -> bool {
        if !self.gate.allow(audio::host_now_ns()) {
            return false;
        }
        #[cfg(windows)]
        self.tag(key_origin::PLI);
        self.pipeline.request_idr();
        true
    }

    /// Where the request came from, for the encode thread's keyframe policy.
    #[cfg(windows)]
    fn tag(&self, origin: u32) {
        self.pipeline.telemetry.link.note_key_origin(origin);
    }

    /// A keyframe for `origin`, through the gate, without a log line.
    #[cfg(windows)]
    pub(crate) fn request_keyframe_from(&self, origin: u32) -> bool {
        if !self.gate.allow(audio::host_now_ns()) {
            return false;
        }
        self.tag(origin);
        self.pipeline.request_idr();
        true
    }
}

#[cfg(windows)]
use crate::windows_utils::streamer::governor::key_origin;

/// The `transport_tier` a client reports when it reads frames through `RTCRtpScriptTransform`
/// (`telemetry::transport_tier_name`).
const SCRIPT_TRANSFORM_TIER: u8 = 1;

/// The `video_path` a client reports when the browser's own decoder paints into a `<video>`
/// (`telemetry::video_path_name`): a page demoted off WebCodecs, or one that never had it.
const VIDEO_ELEMENT_PATH: u8 = 3;

/// Whether a viewer painting through the browser's own decoder is kept under the packet distance.
/// Unmeasured: an old Chrome there probably has the same limit, a current browser probably not.
const PACKET_DISTANCE_FOR_OWN_DECODER: bool = true;

/// Whether a viewer is kept under the packet distance, from the last STATS it sent. Only a page
/// that has said it reads frames through the script transform is exempt; one that has sent
/// nothing (not yet, never, or an older page such as the relay's copy) might be on the transport
/// with the limit.
fn packet_distance_applies(stats: Option<&crate::streamer::telemetry::ClientStats>) -> bool {
    match stats {
        None => true,
        // checked before the video path: a page demoted to its own decoder that still reports the
        // script-transform tier is exempt whatever `PACKET_DISTANCE_FOR_OWN_DECODER` says
        Some(s) if s.transport_tier == SCRIPT_TRANSFORM_TIER => false,
        Some(s) if s.video_path == VIDEO_ELEMENT_PATH => PACKET_DISTANCE_FOR_OWN_DECODER,
        Some(_) => true,
    }
}

pub struct InputParams {
    pub device: Option<String>,
    /// The display's stable device-interface path, so the injector follows the monitor rather
    /// than a `\\.\DISPLAYn` slot Windows can hand to another one.
    #[cfg(windows)]
    pub device_path: Option<String>,
    /// The `os` string the client reported at join, which decides the modifier layout on a
    /// macOS host. Empty when the client did not say.
    pub client_os: String,
    /// Whether remote control is on, and how far a pointer-locked client may drive the cursor.
    /// Both are read per event rather than captured here, so the desktop UI can turn control off
    /// under a live session without anything renegotiating.
    pub live: input::SharedLiveInput,
    pub control_rx: Option<session::ControlReceiver>,
}

#[derive(Clone)]
pub struct AudioParams {
    pub fast: bool,
    /// Put a standard Opus track in the answer even on the fast path, so a decoder that answered
    /// "supported" and then failed on real packets has somewhere to go. The client asks for it
    /// (`AudioCapabilities::wants_fallback_track`): only it knows whether it can switch.
    pub fallback_track: bool,
    /// The client said it can parse [`audio::FLAG_REDUNDANT`] messages. Old clients cannot, so
    /// this is off unless the join request opted in.
    pub redundancy: bool,
    /// Link loss as measured by the video BWE, in hundredths of a percent. Wired in by
    /// `start_session`; `None` outside a live session.
    pub link_loss: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Where this client is, decided from its IP before the handshake. A relayed session is
    /// known to be relayed before a single packet is sent, which the loss-driven redundancy
    /// policy cannot learn in time.
    pub locality: SessionLocality,
    pub hub: SharedAudioHub,
    pub overrides: SharedDeviceOverrides,
    /// The device this session belongs to. `overrides` is keyed by token, so two tabs of one
    /// browser share the audio toggle.
    pub device_token: String,
}

/// Loss at which duplicating the previous frame starts paying for itself: below it redundancy
/// doubles every payload for nothing, above it a lost packet is an audible 5 ms hole with no
/// codec-level concealment. 0.5 %, about where the video BWE starts to react.
const AUDIO_REDUNDANCY_LOSS_THRESHOLD: u32 = 50; // hundredths of a percent

impl AudioParams {
    /// Should this packet carry the previous frame alongside it?
    ///
    /// Never for a client that cannot parse the format. Otherwise on measured loss, which lags,
    /// so a `CrossNetwork` session has it on from the first packet. A clean LAN does not pay.
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
            .get(&self.device_token)
            .map(|o| o.audio_enabled)
            .unwrap_or(false)
    }
}

fn h264_fmtp(profile_level_id: &str) -> String {
    format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}")
}

/// The `profile-level-id` this profile is registered and bound under.
///
/// The level byte is decorative: webrtc-rs's fmtp matcher compares only the first two bytes
/// (`profile_level_id_matches`), and the answer echoes the offer's line anyway. The real level
/// goes on with `negotiate::apply_level_to_answer`.
fn profile_plid(profile: H264Profile) -> &'static str {
    match profile {
        H264Profile::Baseline => "42e01f",
        H264Profile::Main => "4d001f",
        H264Profile::High => "640c1f",
    }
}

/// Every H.264 payload type to register, chosen profile first so the track binds to it.
/// Baseline follows as a floor: a client that offers nothing else must still get a picture
/// rather than a silent codec mismatch.
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

#[cfg(windows)]
type RawSocket = usize;
#[cfg(not(windows))]
type RawSocket = i32;

/// The media socket's mux before a transport owns it: closed if dropped while still held.
/// `UDPMuxDefault` spawns a read worker that lives until `close()`.
struct MuxGuard(Option<Arc<webrtc::ice::udp_mux::UDPMuxDefault>>);

impl Drop for MuxGuard {
    fn drop(&mut self) {
        let Some(mux) = self.0.take() else {
            return;
        };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                use webrtc::ice::udp_mux::UDPMux;
                if let Err(e) = mux.close().await {
                    teprintln!("closing media socket: {e}");
                }
            });
        }
    }
}

/// Builds the media socket on `bind` and lets go of it before any transport has taken it, the
/// way an error or a dropped negotiation would. Whether a socket was made.
#[cfg(test)]
pub(crate) fn drop_unowned_media_socket(bind: std::net::IpAddr) -> bool {
    let guard = MuxGuard(lan_media_socket(bind).map(|m| m.mux));
    guard.0.is_some()
}

/// The media socket a session muxes ICE over, when it is on the LAN tier. The raw handle is
/// what qWAVE needs once the candidate pair is nominated.
struct MediaSocket {
    mux: Arc<webrtc::ice::udp_mux::UDPMuxDefault>,
    raw: RawSocket,
    tuning: sockets::SocketTuning,
    local: std::net::SocketAddr,
}

#[cfg(test)]
tokio::task_local! {
    // puts one test's negotiations on the tuned media socket without the process-wide variable,
    // which every test running beside it would see
    pub(crate) static FORCE_MEDIA_SOCKET: bool;
}

/// Whether the tuned media socket is enabled for this process.
///
/// Off by default. Bound to `0.0.0.0`, the muxed gather emits a host candidate for *every* local
/// address (24 on a box with several virtual adapters), and a pair whose local address is not
/// the one the OS routes from can pass inbound checks and fail outbound: 3 of 5 joins
/// connected. Binding one named address fixes that but is unmeasured on a real AP, so it stays
/// behind `SCREENEXTEND_MEDIA_SOCKET`.
fn media_socket_enabled() -> bool {
    #[cfg(test)]
    if FORCE_MEDIA_SOCKET.try_with(|on| *on).unwrap_or(false) {
        return true;
    }
    matches!(
        crate::envcfg::var("SCREENEXTEND_MEDIA_SOCKET").as_deref(),
        Ok("1") | Ok("true")
    ) || media_socket_by_default()
}

/// Windows candidate `media_socket_default`: the LAN tier takes the tuned socket without the
/// variable, which is also the only way its ICE sockets get `SIO_UDP_CONNRESET` turned off.
#[cfg(windows)]
fn media_socket_by_default() -> bool {
    crate::windows_utils::streamer::tune::candidate("media_socket_default")
}

#[cfg(not(windows))]
fn media_socket_by_default() -> bool {
    false
}

/// Below this a pacer wait is slept precisely (`pacer_precise_wait`); the async timer rounds
/// every deadline up to the next whole millisecond, which here is most of the wait.
#[cfg(windows)]
const PRECISE_WAIT_BELOW: Duration = Duration::from_millis(2);

/// A short pacer wait slept on a blocking thread: `std::thread::sleep` uses a high-resolution
/// waitable timer on Windows, and the wait holds no runtime worker. A spin on the writer's task
/// would hold one, and the workers are also what notice arriving datagrams and input.
#[cfg(windows)]
async fn pace_sleep_precise(wait: Duration) {
    if wait >= PRECISE_WAIT_BELOW
        || tokio::task::spawn_blocking(move || std::thread::sleep(wait))
            .await
            .is_err()
    {
        tokio::time::sleep(wait).await;
    }
}

/// Bind the tuned media socket to the adapter this streamer serves (`Config::bind_ip`), or
/// return `None` for the stock ephemeral sockets. One named address, not `0.0.0.0`: one host
/// candidate, and every packet leaves from the address it names.
fn lan_media_socket(bind: std::net::IpAddr) -> Option<MediaSocket> {
    if bind.is_unspecified() {
        // no named address: `0.0.0.0` would mean a host candidate per local address
        return None;
    }
    let (sock, tuning) = match sockets::bind_tuned(std::net::SocketAddr::new(bind, 0)) {
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

/// Windows `rtx_budget`: the RTP writer answers video NACKs itself, under a byte budget, instead
/// of the stock responder resending everything asked for. Read once per session.
#[cfg(windows)]
fn rtx_budget_enabled() -> bool {
    // without the link governor nothing ever engages, and the stock responder is what runs
    crate::windows_utils::streamer::tune::on("rtx_budget")
        && crate::windows_utils::streamer::tune::on("link_governor")
}

#[cfg(not(windows))]
fn rtx_budget_enabled() -> bool {
    false
}

/// What the writer keeps of a sent packet to send it again: the header and payload as written,
/// and the transport-wide sequence it carried, which a resend repeats as the stock responder did.
#[cfg(windows)]
struct RtxItem {
    pkt: webrtc::rtp::packet::Packet,
    twcc: Option<u16>,
}

#[cfg(windows)]
type NackRx = tokio::sync::mpsc::Receiver<
    Vec<webrtc::rtcp::transport_feedbacks::transport_layer_nack::NackPair>,
>;

#[cfg(windows)]
async fn next_nack(
    rx: &mut Option<NackRx>,
) -> Option<Vec<webrtc::rtcp::transport_feedbacks::transport_layer_nack::NackPair>> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Answer one NACK from the writer's own history: each packet the budget admits goes out
/// through the same pacer as media. False when the track refused a write.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn answer_nack(
    pairs: &[webrtc::rtcp::transport_feedbacks::transport_layer_nack::NackPair],
    rtx: &mut crate::windows_utils::streamer::rtx::Retransmitter<RtxItem>,
    extensions: &mut [webrtc::rtp::extension::HeaderExtension],
    track: &TrackLocalStaticRTP,
    pacer: &mut Pacer,
    precise_wait: bool,
    telemetry: &super::telemetry::SessionTelemetry,
    target_bps: u32,
    link: &mut LinkPace,
    keyframes: &KeyframeRequester,
) -> bool {
    use crate::windows_utils::streamer::rtx;
    let rtt = rtx::client_rtt(telemetry.client.get().as_ref(), audio::host_now_ns());
    pacer.begin_frame();
    let pace = telemetry.link.pace_bps();
    let engaged = telemetry.link.engaged();
    // paced to the link, resends share what the link delivers
    let target_bps = match telemetry.link.delivered_bps() {
        d if pace > 0 && d > 0 => target_bps.min(d.min(u32::MAX as u64) as u32),
        _ => target_bps,
    };
    for seq in rtx::nacked(pairs) {
        if pace > 0 && link.key_needed {
            // a keyframe is on its way for a loss that could not be repaired: nothing before it
            // is worth the link's time
            break;
        }
        // the limits bind only while the link side is engaged; until then every packet still in
        // the history goes again, as the stock responder sends it
        let verdict = if engaged {
            rtx.request(seq, Instant::now(), rtt, target_bps)
        } else {
            rtx.request_unlimited(seq)
        };
        telemetry
            .rtx
            .count(verdict.as_ref().map(|v| v.1).map_err(|e| *e));
        let (item, bytes) = match verdict {
            Ok(v) => v,
            // that frame can never be completed, and every later one depends on it: one keyframe,
            // sent once the backlog has left, instead of waiting for the page or a PLI
            Err(rtx::Refusal::Stale | rtx::Refusal::Budget) if pace > 0 && !link.key_needed => {
                link.key_needed = keyframes.request_keyframe_from(key_origin::UNREPAIRABLE);
                continue;
            }
            Err(_) => continue,
        };
        let pkt = item.pkt.clone();
        let twcc = item.twcc;
        if pace > 0 {
            link.pacer.set_rate(pace);
            let wait = link.pacer.schedule(bytes as usize, Instant::now());
            if !wait.is_zero() {
                if precise_wait {
                    pace_sleep_precise(wait).await;
                } else {
                    tokio::time::sleep(wait).await;
                }
            }
        } else if engaged && pacer.mode() != PacingMode::Off {
            let wait = pacer.schedule(bytes as usize, Instant::now());
            if !wait.is_zero() {
                if precise_wait {
                    pace_sleep_precise(wait).await;
                } else {
                    tokio::time::sleep(wait).await;
                }
            }
        }
        if let (Some(seq), Some(slot)) = (twcc, extensions.get_mut(1)) {
            *slot = webrtc::rtp::extension::HeaderExtension::TransportCc(
                webrtc::rtp::extension::transport_cc_extension::TransportCcExtension {
                    transport_sequence: seq,
                },
            );
        }
        if track
            .write_rtp_with_extensions(&pkt, extensions)
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

/// The writer's pacing to the link while the link side paces (`link_pace`), and whether it has
/// already asked for a keyframe for a loss it could not repair.
#[cfg(windows)]
#[derive(Default)]
struct LinkPace {
    pacer: crate::windows_utils::streamer::governor::LinkPacer,
    key_needed: bool,
}

/// The interceptors every session registers: the stock set, or with `budgeted_rtx` the same set
/// without video retransmission, which the writer then answers.
fn interceptors(media_engine: &mut MediaEngine, budgeted_rtx: bool) -> Result<Registry> {
    #[cfg(windows)]
    if budgeted_rtx {
        return crate::windows_utils::streamer::rtx::register_interceptors(
            Registry::new(),
            media_engine,
        );
    }
    #[cfg(not(windows))]
    let _ = budgeted_rtx;
    register_default_interceptors(Registry::new(), media_engine)
        .context("register default interceptors")
}

fn build_api(
    profile: H264Profile,
    mux: Option<Arc<webrtc::ice::udp_mux::UDPMuxDefault>>,
    bind_ip: std::net::IpAddr,
    budgeted_rtx: bool,
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

    let registry = interceptors(&mut media_engine, budgeted_rtx)?;

    Ok(APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(udp_only_setting_engine(mux, bind_ip))
        .build())
}

fn udp_only_setting_engine(
    mux: Option<Arc<webrtc::ice::udp_mux::UDPMuxDefault>>,
    bind_ip: std::net::IpAddr,
) -> webrtc::api::setting_engine::SettingEngine {
    use webrtc::ice::network_type::NetworkType;
    use webrtc::ice::udp_mux::UDPMux;
    use webrtc::ice::udp_network::UDPNetwork;

    let mut se = webrtc::api::setting_engine::SettingEngine::default();
    se.set_network_types(vec![NetworkType::Udp4, NetworkType::Udp6]);
    se.set_srflx_acceptance_min_wait(Some(std::time::Duration::from_millis(50)));
    se.set_prflx_acceptance_min_wait(Some(std::time::Duration::from_millis(50)));
    se.set_relay_acceptance_min_wait(Some(std::time::Duration::from_millis(0)));
    // `enable_sender_rtx(true)` is deliberately NOT set: webrtc-rs 0.17.1's NACK responder never
    // binds the RTX stream (its codec has no `nack` feedback) and resends on the primary SSRC, so
    // an `a=ssrc-group:FID` would promise Chrome retransmissions on a stream that stays silent.

    // The transport tests run on loopback, which ICE leaves out of its gathering. Tests only: on
    // the bench, loopback candidates would measure a different path and could not pair with a
    // browser's LAN ones.
    if cfg!(test) && bind_ip.is_loopback() {
        se.set_include_loopback_candidate(true);
        se.set_ip_filter(Box::new(move |ip: std::net::IpAddr| ip == bind_ip));
    }
    if let Some(mux) = mux {
        // one socket sends from one address, so offer only that one, not a host candidate per
        // local adapter (Hyper-V, VirtualBox, VPN)
        se.set_ip_filter(Box::new(move |ip: std::net::IpAddr| ip == bind_ip));
        // muxed mode skips server-reflexive gathering (`agent_gather.rs`), hence LAN tier only.
        // Relay gathering builds its own sockets, so a misclassified peer with TURN still connects.
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

    // the medium under the nominated pair, for the log: a USB-tethered phone is an ordinary
    // Ethernet interface, so this is the only way to tell the wired path from Wi-Fi
    if let Some(lip) = cand_ip.get(&lid).cloned() {
        // `spawn_blocking`: two WMI round trips on Windows, a `getifaddrs` walk plus an ioctl on
        // macOS, and neither belongs on a tokio worker
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
                "WARNING: same-subnet peers ({lip} <-> {rip}) are routing via relay/router: \
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

/// How long a close waits for ICE gathering to finish first. A STUN request gives up after 5 s.
const GATHER_SETTLE: Duration = Duration::from_secs(10);

fn still_gathering(pc: &webrtc::peer_connection::RTCPeerConnection) -> bool {
    use webrtc::ice_transport::ice_gathering_state::RTCIceGatheringState;
    pc.ice_gathering_state() == RTCIceGatheringState::Gathering
}

/// Close `pc`, once it has stopped gathering candidates.
///
/// webrtc-ice 0.17.1 does not cancel a gathering round when its agent closes. A candidate that
/// turns up afterwards (a slow STUN server) is added to the closed agent and given a receive
/// loop nothing will ever stop, and that loop holds the agent and every socket it had.
async fn close_connection(pc: &webrtc::peer_connection::RTCPeerConnection) {
    let deadline = Instant::now() + GATHER_SETTLE;
    while still_gathering(pc) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if let Err(e) = pc.close().await {
        teprintln!("closing peer connection: {e}");
    }
}

/// A session's peer connection and the media socket under it, closed exactly once however the
/// session ends. Dropping an `RTCPeerConnection` closes nothing, and an aborted `/whep` request
/// drops `start_session` between any two `.await`s, so `Drop` closes too, in a task of its own
/// (a `close` the caller stopped polling halfway leaves a connection half closed).
struct Transport {
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    /// `UDPMuxDefault` runs a read worker that only stops on `close()`.
    mux: Option<Arc<webrtc::ice::udp_mux::UDPMuxDefault>>,
    /// The WHEP resource id, once the trickle registry knows this connection by it.
    resource_id: String,
    runtime: tokio::runtime::Handle,
    closing: bool,
}

impl Transport {
    fn new(
        pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
        mux: Option<Arc<webrtc::ice::udp_mux::UDPMuxDefault>>,
    ) -> Self {
        LIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);
        Self {
            pc,
            mux,
            resource_id: String::new(),
            runtime: tokio::runtime::Handle::current(),
            closing: false,
        }
    }

    fn begin_close(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        if std::mem::replace(&mut self.closing, true) {
            return None;
        }
        unregister_trickle(&self.resource_id, &self.pc);
        let pc = Arc::clone(&self.pc);
        let mux = self.mux.take();
        Some(self.runtime.spawn(async move {
            close_connection(&pc).await;
            if let Some(mux) = mux {
                use webrtc::ice::udp_mux::UDPMux;
                if let Err(e) = mux.close().await {
                    teprintln!("closing media socket: {e}");
                }
            }
            LIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
        }))
    }

    /// Close, and wait for it, unless gathering is what it would be waiting for: the caller has a
    /// display to give back, and nothing left to do here depends on it.
    async fn close(mut self) {
        let gathering = still_gathering(&self.pc);
        if let Some(closing) = self.begin_close() {
            if !gathering {
                let _ = closing.await;
            }
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.begin_close();
    }
}

/// What the caller must hold to end a session. Nothing else closes the peer connection, and an
/// unclosed one leaks its ICE agent and sockets and one `injector` OS thread, once per join.
pub struct SessionHandle {
    transport: Option<Transport>,
    /// Dropped to send the RTP writer home. It is the one task with no other way to learn the
    /// session is over: a write to a track whose sender has stopped is `Ok(0)`, not an error.
    writer_stop: Option<tokio::sync::oneshot::Sender<()>>,
    input_tx: Option<input::InjectorTx>,
    input_join: Option<std::thread::JoinHandle<()>>,
    /// The qWAVE flow, released with the session.
    _qos: Arc<Mutex<Option<sockets::QosFlow>>>,
}

/// How long `close` will wait for the injector thread to notice it has no work left.
const INJECTOR_JOIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Wait for the injector to leave, but not forever. `true` when it did.
fn join_injector(join: std::thread::JoinHandle<()>) -> bool {
    let start = std::time::Instant::now();
    while !join.is_finished() {
        if start.elapsed() >= INJECTOR_JOIN_DEADLINE {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    join.join().is_ok()
}

impl SessionHandle {
    /// Close the peer connection and wait for the injector thread to release its input state.
    pub async fn close(mut self) {
        drop(self.writer_stop.take());
        // the connection's own handlers hold clones of the injector's sender (the input channel's
        // `on_message`, the state-change handler), so close it first: otherwise the join below
        // waits on a thread that cannot leave, inside the task that removes the display
        if let Some(transport) = self.transport.take() {
            transport.close().await;
        }
        self.stop_injector();
        if let Some(join) = self.input_join.take() {
            // bounded: a session that cannot prove the thread left must still give its display
            // back, and the cost is one idle OS thread parked on a channel nothing can feed
            let left = tokio::task::spawn_blocking(move || join_injector(join))
                .await
                .unwrap_or(false);
            if !left {
                teprintln!(
                    "injector thread did not leave within {INJECTOR_JOIN_DEADLINE:?}; \
                     continuing so the display is released anyway"
                );
            }
        }
    }

    /// The connection underneath, to ask afterwards whether anything still holds it.
    #[cfg(test)]
    pub(crate) fn peer_connection(
        &self,
    ) -> std::sync::Weak<webrtc::peer_connection::RTCPeerConnection> {
        self.transport
            .as_ref()
            .map(|t| Arc::downgrade(&t.pc))
            .unwrap_or_default()
    }

    fn stop_injector(&mut self) {
        if let Some(tx) = self.input_tx.take() {
            tx.shutdown();
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.stop_injector();
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

/// A negotiated WHEP session whose pipeline does not exist yet. The answer depends only on the
/// offer, the codec choice and the ICE server list, so `start_session` runs the handshake and
/// the display/pipeline work concurrently.
pub struct PendingWhep {
    transport: Transport,
    media_raw: Option<RawSocket>,
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
    /// The writer answers video NACKs itself (`rtx_budget`): the interceptors were built without
    /// the stock video responder.
    #[cfg(windows)]
    rtx_budget: bool,
    /// The transport this one replaces when it is a renegotiation in place: the client has
    /// already abandoned it, and it is closed the moment this one connects.
    superseded: Option<std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>>,
}

impl PendingWhep {
    /// The WHEP resource id, which the `Location` header points at.
    pub fn resource_id(&self) -> &str {
        &self.transport.resource_id
    }

    /// Give everything back when the other half of the join failed: the connection, its ICE
    /// agent, that agent's sockets, and the mux read worker.
    pub async fn abandon(self) {
        self.transport.close().await;
    }

    #[cfg(test)]
    pub(crate) fn peer_connection(
        &self,
    ) -> std::sync::Weak<webrtc::peer_connection::RTCPeerConnection> {
        Arc::downgrade(&self.transport.pc)
    }
}

/// Build the peer connection, add the tracks, and produce the answer.
///
/// Needs no pipeline: the profile comes from the negotiation, the audio decision from the join
/// request's capability flags, and the transport constants from the pre-classified locality.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_whep(
    offer_sdp: String,
    join_id: &str,
    device_token: &str,
    ice_servers: Vec<RTCIceServer>,
    locality: SessionLocality,
    video: negotiate::NegotiatedVideo,
    audio: Option<&AudioParams>,
    bind_ip: std::net::IpAddr,
) -> Result<PendingWhep> {
    let profile = video.profile;
    // LAN tier only: a socket the host binds itself, with the buffers and the Windows CONNRESET
    // fix webrtc-rs never applies. Muxed mode gives up server-reflexive gathering, so never on a
    // relayed or unclassified path, and never without the opt-in (`media_socket_enabled`).
    let media_socket = if locality == SessionLocality::SameNetwork && media_socket_enabled() {
        lan_media_socket(bind_ip)
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
    let media_raw = media_socket.as_ref().map(|m| m.raw);
    // the mux's read worker runs from here: until the transport owns the mux, the guard closes
    // it on any way out (an error below, or the caller ceasing to poll)
    let mut mux = MuxGuard(media_socket.map(|m| m.mux));
    let rtx_budget = rtx_budget_enabled();
    let api = build_api(profile, mux.0.clone(), bind_ip, rtx_budget)?;

    let rtc_config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    let pc = Arc::new(
        api.new_peer_connection(rtc_config)
            .await
            .context("new_peer_connection")?,
    );
    // from here on every way out closes the connection, the caller ceasing to poll included
    let mut transport = Transport::new(Arc::clone(&pc), mux.0.take());

    // transport constants that differ between a same-subnet hop and a relayed path, decided up
    // front so the first frames go out right. The post-hoc detector logs a wrong guess.
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

    // from the offer: an extension the peer did not offer cannot be negotiated into the answer
    let twcc_offered = offer_sdp.contains("transport-wide-cc-extensions-01");

    // everything from here to the answer can fail (`set_remote_description` on a malformed offer,
    // most realistically), so it runs as one block and any error closes the transport below
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

        // no IDR here: the track has no binding until the connection comes up, and a write to an
        // unbound track is silently discarded

        let answer = pc.create_answer(None).await.context("create_answer")?;

        let mut gather_complete = pc.gathering_complete_promise().await;
        pc.set_local_description(answer)
            .await
            .context("set_local_description(answer)")?;
        // a same-subnet peer needs only host candidates, which gather in single-digit milliseconds:
        // the 1.5 s wait for a reflexive candidate would be pure time-to-first-frame
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
        // the level the browser named (Chrome always says 3.1, i.e. 1280x720@30)
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
            transport.close().await;
            return Err(e);
        }
    };

    // registered only once the answer exists: the client cannot PATCH a resource whose id it has
    // not been given, and a failed negotiation must not leave an entry behind
    transport.resource_id = join_id.to_string();
    let superseded = register_trickle(join_id, join_id, device_token, &pc);

    Ok(PendingWhep {
        transport,
        media_raw,
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
        #[cfg(windows)]
        rtx_budget,
        superseded,
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
    #[cfg(windows)]
    let frames = pipeline.subscribe();
    #[cfg(not(windows))]
    let frames = pipeline.tx.subscribe();
    activate_session(
        pending,
        frames,
        SessionPipeline::of(pipeline),
        closed_tx,
        input,
        audio,
    )
    .await
}

/// [`activate_whep`] on the parts of a pipeline a session uses, so the transport can be driven
/// without an encoder behind it.
pub(crate) async fn activate_session(
    pending: PendingWhep,
    frames: tokio::sync::broadcast::Receiver<EncodedFrame>,
    pipeline: SessionPipeline,
    closed_tx: Option<tokio::sync::oneshot::Sender<()>>,
    input: InputParams,
    audio: Option<AudioParams>,
) -> Result<(String, SessionHandle)> {
    let PendingWhep {
        transport,
        media_raw,
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
        #[cfg(windows)]
        rtx_budget,
        superseded,
    } = pending;
    let pc = Arc::clone(&transport.pc);
    let keyframe_gate = Arc::new(KeyframeGate::for_locality(locality));
    let keyframes = KeyframeRequester {
        pipeline: pipeline.clone(),
        gate: Arc::clone(&keyframe_gate),
    };
    let feedback = Arc::new(SharedFeedback::default());
    let clock_epoch = Instant::now();
    *feedback.history.lock_recover() = Some(SendHistory::new(clock_epoch));
    // the pacer refills from the same target the encoder is given, so the two never disagree
    // about how much this session may send
    let pacing_target_bps = Arc::new(std::sync::atomic::AtomicU32::new(pipeline.max_bitrate_bps));

    let InputParams {
        device: input_device,
        #[cfg(windows)]
            device_path: input_device_path,
        client_os,
        live: live_input,
        control_rx,
    } = input;
    let client = input::ClientProfile {
        device_name: input_device,
        #[cfg(windows)]
        device_path: input_device_path,
        platform: input::ClientPlatform::from_reported(&client_os),
        live: std::sync::Arc::clone(&live_input),
    };
    let (input_tx, input_join) = input::spawn(client, Arc::clone(&pipeline.telemetry));
    {
        let input_tx = input_tx.clone();
        let control_rx = Arc::new(Mutex::new(control_rx));
        let session_telemetry = Arc::clone(&pipeline.telemetry);
        let keyframes_dc = keyframes.clone();
        let live_for_dc = Arc::clone(&live_input);
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let session_telemetry = Arc::clone(&session_telemetry);
            let keyframes_dc = keyframes_dc.clone();
            let live_for_dc = Arc::clone(&live_for_dc);
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
                    let dc_ctl = Arc::downgrade(&dc);
                    tokio::spawn(async move {
                        while let Some(msg) = rx.recv().await {
                            let frame = input::protocol::build_bye(msg.reason.code());
                            if let Some(dc) = dc_ctl.upgrade() {
                                let _ = dc.send(&Bytes::copy_from_slice(&frame)).await;
                            }
                            if let Some(sent) = msg.sent {
                                let _ = sent.send(());
                            }
                        }
                    });
                }
                // `Weak`: the channel owns `on_message`, which owns the sender this task reads, so
                // an `Arc` here is a cycle neither the task nor the channel could ever leave
                let dc_for_pong = is_reliable.then(|| Arc::downgrade(&dc));
                let idr_keyframes = keyframes_dc.clone();
                // the client's own 200 ms limit is advisory: a hostile or looping client must not
                // pin the encoder at one IDR per frame. `Instant`, not the host clock: a local
                // interval nothing else compares against
                let mut last_idr_request: Option<std::time::Instant> = None;
                let telemetry_for_stats = Arc::clone(&session_telemetry);
                // one log line per distinct rejection per channel: a page that has drifted from
                // this host build repeats the same malformed frame thousands of times a second
                let mut reported_errors: std::collections::HashSet<input::protocol::ParseError> =
                    std::collections::HashSet::new();
                // the Pong is sent from its own task: an SCTP send inside the receive handler
                // head-of-line blocks every later message on this channel
                let pong_tx = dc_for_pong.map(|dc_pong| {
                    let (pong_tx, mut pong_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
                    let pong_telemetry = Arc::clone(&session_telemetry);
                    tokio::spawn(async move {
                        while let Some(t_ns) = pong_rx.recv().await {
                            let state = if pong_telemetry.picture_pending_now() {
                                input::protocol::hoststate::PICTURE_PENDING
                            } else {
                                0
                            };
                            let pong = input::protocol::build_pong(
                                t_ns,
                                crate::streamer::audio::host_now_ns(),
                                state,
                            );
                            let Some(dc) = dc_pong.upgrade() else { break };
                            if dc.send(&Bytes::copy_from_slice(&pong)).await.is_err() {
                                break;
                            }
                        }
                    });
                    pong_tx
                });
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    // remote control is read per message, never captured: turning it off must not
                    // cost the session a renegotiation
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
                    // stats and ping are answered before the remote-control gate: a device with
                    // control disabled still reports its pipeline and still needs its clock synced
                    match parsed.ev {
                        input::protocol::InputEvent::ClientStats(stats) => {
                            telemetry_for_stats.set_client_stats(*stats);
                        }
                        input::protocol::InputEvent::Ping { t_ns } => {
                            if let Some(pong_tx) = &pong_tx {
                                let _ = pong_tx.send(t_ns);
                            }
                        }
                        // the legacy encoded transform runs instead of libwebrtc's reference
                        // finder, so no PLI is ever emitted: this is its only way to ask
                        input::protocol::InputEvent::KeyframeRequest { .. } => {
                            let now = std::time::Instant::now();
                            let allowed = last_idr_request
                                .is_none_or(|t| now.duration_since(t) >= MIN_IDR_REQUEST_GAP);
                            // counted whether or not it is acted on
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
                                // via the requester: this floor stops a looping client, the gate
                                // stops a decoder's backlog from being answered with IDRs
                                #[cfg(windows)]
                                idr_keyframes.request_keyframe_from(key_origin::PAGE);
                                #[cfg(not(windows))]
                                idr_keyframes.request_keyframe_for_client_quietly();
                            }
                        }
                        // letting go is not control: a page that loses focus or is hidden releases
                        // what it held even while remote control is off
                        ev @ input::protocol::InputEvent::Lifecycle(
                            input::protocol::Lifecycle::Focus(false)
                            | input::protocol::Lifecycle::Visibility(false),
                        ) if cfg!(windows) => {
                            tx.route(ev, parsed.fast, parsed.client_ns, parsed.seq)
                        }
                        ev if live_for_dc.control_enabled() => {
                            tx.route(ev, parsed.fast, parsed.client_ns, parsed.seq)
                        }
                        _ => {}
                    }
                    Box::pin(async {})
                }));
            })
        }));
    }

    // video NACKs go to the writer, which holds what was sent and the pacer
    #[cfg(windows)]
    let (nack_tx, mut nack_rx) = if rtx_budget {
        let (tx, rx) = tokio::sync::mpsc::channel(crate::windows_utils::streamer::rtx::NACK_QUEUE);
        pipeline.telemetry.rtx.active.store(true, Ordering::Relaxed);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

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
            // an encoded-transform receiver sends a PLI every 200 ms for the life of the session,
            // and the gate alone would answer nearly every one with an IDR
            let mut pli_backoff = PliBackoff::new();
            let mut client_key_requests: u32 = 0;
            let mut client_frames_painted: u32 = 0;
            let mut client_stats_at: u64 = 0;
            let mut client_painting = false;
            let mut rtcp_buf = vec![0u8; 1500];
            let mut deltas: Vec<f64> = Vec::with_capacity(256);
            // the video stream's SSRC, read once it is bound: a NACK for anything else is not ours
            #[cfg(windows)]
            let mut video_ssrc = 0u32;
            loop {
                let packets = match rtp_sender.read(&mut rtcp_buf).await {
                    Ok((packets, _attrs)) => packets,
                    // the resends ride this reader: one packet that does not parse must not end it
                    #[cfg(windows)]
                    Err(e) => {
                        use crate::windows_utils::streamer::rtx::{rtcp_read_error, RtcpReadError};
                        match rtcp_read_error(&e) {
                            RtcpReadError::Skip => {
                                if telemetry.rtx.rtcp_unparsed.fetch_add(1, Ordering::Relaxed) == 0
                                {
                                    teprintln!(
                                        "rtcp: skipped a feedback packet that does not parse \
                                         ({e}); later ones are counted in \
                                         retransmission.rtcp_packets_unparsed"
                                    );
                                }
                                continue;
                            }
                            RtcpReadError::Closed => break,
                            RtcpReadError::Fatal => {
                                telemetry
                                    .rtx
                                    .rtcp_reader_failed
                                    .store(true, Ordering::Relaxed);
                                teprintln!(
                                    "rtcp: the feedback reader ended on an error ({e}): no more \
                                     PLIs, NACKs or transport feedback from this viewer"
                                );
                                break;
                            }
                        }
                    }
                    #[cfg(not(windows))]
                    Err(_) => break,
                };
                for p in &packets {
                    let any = p.as_any();
                    if any.downcast_ref::<PictureLossIndication>().is_some()
                        || any.downcast_ref::<FullIntraRequest>().is_some()
                    {
                        // Always counted. Acting on it is the backoff's decision: a decoder that
                        // has just lost its reference chain emits a PLI per frame in its backlog.
                        telemetry
                            .degradation
                            .keyframe_requests
                            .fetch_add(1, Ordering::Relaxed);
                        // the client counts the requests its own page makes: a moved counter is the
                        // page saying this one is genuine
                        let now_ns = audio::host_now_ns();
                        let mut cap_ms = PLI_BACKOFF_CAP_MS;
                        if let Some(stats) = telemetry.client.get() {
                            if stats.keyframe_requests != client_key_requests {
                                client_key_requests = stats.keyframe_requests;
                                pli_backoff.note_evidence();
                            }
                            // painting through WebCodecs, and said so within the last 3 s: the
                            // PLI is libwebrtc's own idle decoder, not the page
                            let fresh =
                                now_ns.saturating_sub(stats.received_host_ns) < 3_000_000_000;
                            // judged per STATS frame (about 1 Hz), not per PLI (5 Hz)
                            if stats.received_host_ns != client_stats_at {
                                client_stats_at = stats.received_host_ns;
                                client_painting = stats.frames_painted != client_frames_painted;
                                client_frames_painted = stats.frames_painted;
                            }
                            if fresh && client_painting && stats.transport_tier != 0 {
                                cap_ms = pli_backoff_cap_healthy_ms();
                            }
                        }
                        if pli_backoff.allow_with_cap(now_ns, cap_ms) {
                            keyframes.request_keyframe_for_client_quietly();
                        } else {
                            keyframes.gate.note_suppressed();
                        }
                    } else if let Some(nack) = any.downcast_ref::<TransportLayerNack>() {
                        // counted, never logged: a lossy link produces these continuously and every
                        // logbus line is a JSON IPC event to the UI
                        let lost: u64 = nack
                            .nacks
                            .iter()
                            .map(|n| 1 + n.lost_packets.count_ones() as u64)
                            .sum();
                        telemetry
                            .degradation
                            .nacks_received
                            .fetch_add(lost.max(1), Ordering::Relaxed);
                        pli_backoff.note_evidence();
                        #[cfg(windows)]
                        if telemetry.link.engaged() {
                            pli_backoff.withdraw_evidence();
                        }
                        // as the stock responder: only requests for the stream this writer owns.
                        // What still differs while nothing is engaged: a resend goes out between
                        // frames rather than from its own task, through the whole interceptor
                        // chain (so sender reports count it), and a full queue drops a batch
                        #[cfg(windows)]
                        if video_ssrc == 0 && nack_tx.is_some() {
                            video_ssrc = rtp_sender
                                .get_parameters()
                                .await
                                .encodings
                                .first()
                                .map_or(0, |e| e.ssrc);
                        }
                        #[cfg(windows)]
                        if let Some(tx) = nack_tx.as_ref().filter(|_| nack.media_ssrc == video_ssrc)
                        {
                            use tokio::sync::mpsc::error::TrySendError;
                            if let Err(TrySendError::Full(_)) = tx.try_send(nack.nacks.clone()) {
                                telemetry
                                    .rtx
                                    .nack_batches_dropped
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        }
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
                            #[cfg(windows)]
                            telemetry
                                .link
                                .on_feedback(&summary.path, audio::host_now_ns());
                        }
                        if summary.lost > 0 {
                            pli_backoff.note_evidence();
                            #[cfg(windows)]
                            if telemetry.link.engaged() {
                                pli_backoff.withdraw_evidence();
                            }
                        }
                        // per-frame receipts for the encode thread: what the client holds and
                        // what it lost, which a long-term-reference repair needs
                        if let Some(ns) = summary.acked_capture_ns {
                            pipeline.repair.acknowledge(ns);
                        }
                        for ns in &summary.lost_capture_ns {
                            pipeline.repair.note_lost(*ns);
                        }
                        if !deltas.is_empty() {
                            let mut det = feedback.detector.lock_recover();
                            for d in deltas.iter() {
                                det.observe(*d);
                            }
                        }
                    } else if let Some(remb) = any.downcast_ref::<ReceiverEstimatedMaximumBitrate>()
                    {
                        // without the transport-wide sequence number Chrome runs its own
                        // receive-side estimator and reports it here
                        feedback.record_remb(remb.bitrate as u32);
                    }
                }
            }
        });
    }

    // one loss estimate, shared with audio: its DataChannel rides the same link, and SCTP does
    // not report loss the way RTCP does
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

    let (writer_stop, mut writer_stopped) = tokio::sync::oneshot::channel::<()>();
    {
        let mut rx = frames;
        let track = Arc::clone(&track);
        let pipeline = pipeline.clone();
        let keyframes = keyframes.clone();
        let feedback = Arc::clone(&feedback);
        let pacing_target = Arc::clone(&pacing_target_bps);
        tokio::spawn(async move {
            let telemetry = Arc::clone(&pipeline.telemetry);
            let mut packetizer = H264Packetizer::new();
            #[cfg(feature = "bench")]
            let mut au_dump = AuDump::from_env();
            // per-session access-unit ordinal, so transport-wide feedback can say which *frame* the
            // client received whole (`SharedFeedback::acked_frame_capture_ns`)
            let mut frame_ordinal: u32 = 0;
            // reused across frames: it only ever grows to the largest access unit of the session
            let mut payloads: Vec<Bytes> = Vec::with_capacity(64);
            let sequencer = new_random_sequencer();
            let mut distance = PacketDistance::with_limit(pipeline.packet_distance_limit());
            let mut pacer = Pacer::new(
                pacing_mode,
                pipeline.max_bitrate_bps,
                mtu,
                pipeline.frame_duration,
            );
            #[cfg(windows)]
            let precise_wait =
                crate::windows_utils::streamer::tune::candidate("pacer_precise_wait");
            #[cfg(windows)]
            let mut link_pace = LinkPace::default();
            let mut warned_unbound = false;
            let mut last_rtp_ts: Option<u32> = None;
            let mut first_frame_written = false;
            let mut warned_dropped_params = false;
            // transport-cc only when the client offered it: an unnegotiated extension costs a
            // marshal and a `BytesMut` per packet and reaches nobody
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
            #[cfg(windows)]
            let mut rtx = nack_rx.is_some().then(|| {
                // the same extensions the frame path writes; a resend fills in its own sequence
                let mut ext = vec![webrtc::rtp::extension::HeaderExtension::PlayoutDelay(
                    webrtc::rtp::extension::playout_delay_extension::PlayoutDelayExtension::new(
                        0,
                        playout_delay_max,
                    ),
                )];
                if twcc_offered {
                    ext.push(webrtc::rtp::extension::HeaderExtension::TransportCc(
                        webrtc::rtp::extension::transport_cc_extension::TransportCcExtension::default(),
                    ));
                }
                (
                    crate::windows_utils::streamer::rtx::Retransmitter::<RtxItem>::new(),
                    ext,
                )
            });
            loop {
                // a repair goes ahead of the next frame: the decoder is waiting on it
                #[cfg(windows)]
                let received = tokio::select! {
                    biased;
                    _ = &mut writer_stopped => break,
                    Some(pairs) = next_nack(&mut nack_rx) => {
                        if let Some((history, ext)) = rtx.as_mut() {
                            answer_nack(
                                &pairs,
                                history,
                                ext,
                                &track,
                                &mut pacer,
                                precise_wait,
                                &telemetry,
                                pacing_target.load(Ordering::Relaxed),
                                &mut link_pace,
                                &keyframes,
                            )
                            .await;
                        }
                        let pace = telemetry.link.pace_bps();
                        if pace > 0 {
                            link_pace.pacer.set_rate(pace);
                            let left = link_pace.pacer.backlog(0, Instant::now());
                            telemetry
                                .link
                                .set_writer_until(audio::host_now_ns() + left.as_nanos() as u64);
                        }
                        continue;
                    }
                    received = rx.recv() => received,
                };
                #[cfg(not(windows))]
                let received = tokio::select! {
                    biased;
                    _ = &mut writer_stopped => break,
                    received = rx.recv() => received,
                };
                match received {
                    Ok(frame) => {
                        let mut timing = frame.timing;
                        let capture_ns = audio::host_instant_to_ns(frame.capture);
                        frame_ordinal = frame_ordinal.wrapping_add(1);
                        #[cfg(feature = "bench")]
                        au_dump.write(&frame.data);
                        packetizer.packetize(mtu, &frame.data, &mut payloads);

                        if payloads.is_empty() {
                            // nothing packetizable (an AUD-only access unit, a degenerate buffer):
                            // another IDR is the only recovery, and the gate stops that spinning
                            keyframes.request_quietly();
                            continue;
                        }
                        // strictly increasing: a frame captured just before a repeat was submitted
                        // but encoded after it would go backwards, and receivers drop those
                        let (rtp_ts, clamped) =
                            next_video_rtp_ts(last_rtp_ts, audio::host_ns_to_rtp90k(capture_ns));
                        last_rtp_ts = Some(rtp_ts);
                        if clamped {
                            telemetry.rtp_ts_clamped.fetch_add(1, Ordering::Relaxed);
                        }
                        // lost parameter sets are a permanent black screen with RTP flowing and
                        // every host-side signal reporting success, so check and say so
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
                        // paced to the link: the session's own pacer stands aside for this frame
                        // and says when the frame will have left. Not paced, the mode it always
                        // had, which changes nothing
                        #[cfg(windows)]
                        let link_bps = telemetry.link.pace_bps();
                        #[cfg(windows)]
                        if link_bps > 0 {
                            pacer.set_mode(PacingMode::Off);
                            link_pace.pacer.set_rate(link_bps);
                            let left = link_pace
                                .pacer
                                .backlog(frame_bytes as usize, Instant::now());
                            telemetry
                                .link
                                .set_writer_until(audio::host_now_ns() + left.as_nanos() as u64);
                        } else {
                            pacer.set_mode(pacing_mode);
                        }
                        if super::telemetry::frame_trace_enabled() {
                            timing.trace_bytes = frame_bytes as u32;
                            timing.trace_idr = frame
                                .data
                                .windows(4)
                                .any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && (w[3] & 0x1F) == 5);
                        }
                        let last = payloads.len().saturating_sub(1);
                        let frame_packets = payloads.len() as u32;
                        let frame_idr = payloads.iter().any(|p| payload_carries_idr(p));
                        let mut stop = false;
                        let mut written = 0usize;
                        let waited_before = pacer.total_wait_ns();
                        let missed_before = pacer.deadline_misses();
                        // what the waits really took: the async timer makes them longer than the
                        // pacer asked for (`pacer_precise_wait` records this instead)
                        #[cfg(windows)]
                        let mut slept_ns = 0u64;
                        for (i, payload) in payloads.drain(..).enumerate() {
                            let wire_len = payload.len() + 12;
                            #[cfg(windows)]
                            if link_bps > 0 {
                                let wait = link_pace.pacer.schedule(wire_len, Instant::now());
                                if !wait.is_zero() {
                                    let started = Instant::now();
                                    if precise_wait {
                                        pace_sleep_precise(wait).await;
                                    } else {
                                        tokio::time::sleep(wait).await;
                                    }
                                    slept_ns += started.elapsed().as_nanos() as u64;
                                }
                            }
                            // `PacingMode::Off`: no clock read, no bucket, no sleep
                            if pacer.mode() != PacingMode::Off {
                                let wait = pacer.schedule(wire_len, Instant::now());
                                #[cfg(windows)]
                                let wait = if !wait.is_zero() && precise_wait {
                                    let started = Instant::now();
                                    pace_sleep_precise(wait).await;
                                    slept_ns += started.elapsed().as_nanos() as u64;
                                    Duration::ZERO
                                } else {
                                    wait
                                };
                                if !wait.is_zero() {
                                    tokio::time::sleep(wait).await;
                                }
                            }
                            #[cfg(windows)]
                            let mut sent_twcc: Option<u16> = None;
                            if twcc_offered {
                                // stamped here: the stock `twcc::sender::Sender` interceptor keeps
                                // its counter private, and the feedback is useless without the
                                // packet-to-sequence mapping
                                let now = Instant::now();
                                let transport_seq = {
                                    let mut guard = feedback.history.lock_recover();
                                    guard.as_mut().map(|h| {
                                        h.record(wire_len as u32, now, frame_ordinal, capture_ns)
                                    })
                                };
                                if let Some(seq) = transport_seq {
                                    #[cfg(windows)]
                                    {
                                        sent_twcc = Some(seq);
                                    }
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
                            // a header with no extensions and a refcounted payload: no allocation
                            #[cfg(windows)]
                            let kept = rtx.as_ref().map(|_| pkt.clone());
                            match track.write_rtp_with_extensions(&pkt, &extensions).await {
                                Ok(n) => {
                                    written += n;
                                    #[cfg(windows)]
                                    if let (Some((history, _)), Some(pkt)) = (rtx.as_mut(), kept) {
                                        history.sent(
                                            pkt.header.sequence_number,
                                            wire_len,
                                            Instant::now(),
                                            RtxItem {
                                                pkt,
                                                twcc: sent_twcc,
                                            },
                                        );
                                    }
                                }
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
                        #[cfg(windows)]
                        if frame_idr {
                            link_pace.key_needed = false;
                        }
                        let asked = distance.on_frame(
                            frame_packets,
                            frame_idr,
                            audio::host_now_ns(),
                            keyframes.gate.interval_ms(),
                            || packet_distance_applies(telemetry.client.get().as_ref()),
                            |request| keyframes.request_keyframe_for_distance(request),
                        );
                        let counter = match asked {
                            DistanceAsk::Admitted | DistanceAsk::Forced => {
                                Some(&telemetry.degradation.keyframes_for_packet_distance)
                            }
                            DistanceAsk::Refused => {
                                Some(&telemetry.degradation.packet_distance_asks_refused)
                            }
                            DistanceAsk::None => None,
                        };
                        if let Some(c) = counter {
                            c.fetch_add(1, Ordering::Relaxed);
                        }
                        if written == 0 {
                            // `Ok(0)` means no binding yet or a paused sender: the frame is
                            // dropped with no error anywhere, so say it once
                            if !warned_unbound {
                                warned_unbound = true;
                                tprintln!(
                                    "RTP discarded: the video track has no binding yet (waiting for the connection to come up)"
                                );
                            }
                            continue;
                        }
                        // The first frame this viewer actually received. The track binds during
                        // ICE, so the opening IDR normally went out unbound, and with an infinite
                        // GOP nothing else produces a keyframe: ask once. Without it the page logs
                        // `no WebCodecs frame within 5s` and demotes itself to a `<video>` element.
                        if warned_unbound || !first_frame_written {
                            first_frame_written = true;
                            keyframes.request_keyframe_for_client("first RTP reached the wire");
                        }
                        warned_unbound = false;
                        timing.mark_socket_write_done();
                        // only the sleeping, not the writes interleaved with it: the cost of pacing
                        // has to be separable from the cost of sending
                        let pace_wait = pacer.total_wait_ns().saturating_sub(waited_before);
                        #[cfg(windows)]
                        let pace_wait = if precise_wait || link_bps > 0 {
                            slept_ns
                        } else {
                            pace_wait
                        };
                        telemetry.stages.pace_wait.record(pace_wait);
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
                        // the local broadcast dropped frames on this viewer: the link is
                        // oversubscribed, so gate the keyframe and cut the rate to keep it small
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

    // `Weak`, not `Arc`: webrtc-rs stores this closure inside the peer connection, so an owning
    // handle is a reference cycle, and `close()` does not clear the handler
    let pc_state = Arc::downgrade(&pc);
    let qos_flow: Arc<Mutex<Option<sockets::QosFlow>>> = Arc::new(Mutex::new(None));
    let qos_flow_state = Arc::clone(&qos_flow);
    let locality_logged = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let closed_tx = Arc::new(Mutex::new(closed_tx));
    let input_tx_state = input_tx.clone();
    let keyframes_state = keyframes.clone();
    let (grace_cancel_tx, grace_cancel_rx) = tokio::sync::watch::channel(false);
    let grace_cancel_tx = Arc::new(grace_cancel_tx);
    let grace_cancel_rx_closure = grace_cancel_rx.clone();
    let expected_locality = locality;
    let superseded = Mutex::new(superseded);
    // a new candidate pair is a new path: the Windows QP governor forgets the old one's delay
    #[cfg(windows)]
    {
        let path_telemetry = Arc::clone(&pipeline.telemetry);
        pc.sctp()
            .transport()
            .ice_transport()
            .on_selected_candidate_pair_change(Box::new(move |_| {
                path_telemetry.link.path_changed();
                Box::pin(async {})
            }));
    }
    let connected_state = Arc::clone(&pipeline.telemetry);
    pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        if state == RTCPeerConnectionState::Connected {
            connected_state
                .transport_connected
                .store(true, Ordering::Relaxed);
            // A renegotiation in place leaves the transport it replaces running until ICE gives
            // up on it 15-30 s later, so close it. Its `Closed` state runs the superseded
            // session's ordinary teardown, which keeps the display (its sequence is not current).
            if let Some(old) = superseded.lock_recover().take().and_then(|w| w.upgrade()) {
                tokio::spawn(async move {
                    tprintln!("closing the transport this renegotiation replaced");
                    close_connection(&old).await;
                });
            }
        }
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
            // the first moment the track can carry an IDR: before the binding `write_rtp` returns
            // `Ok(0)`, and with an infinite GOP nothing else would produce a keyframe
            #[cfg(windows)]
            if keyframes_state.request_keyframe_from(key_origin::START) {
                tprintln!("keyframe requested: connection is up and the track is bound");
            }
            #[cfg(not(windows))]
            keyframes_state.request("connection is up and the track is bound");
            if !locality_logged.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let pc = pc_state.clone();
                let qos_slot = Arc::clone(&qos_flow_state);
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let Some(pc) = pc.upgrade() else { return };
                    let (observed, remote) = detect_session_locality(&pc).await;
                    // Windows marks DSCP only through qWAVE, which needs a destination, and none
                    // exists until a pair is nominated. On failure (qWAVE off or absent) the
                    // traffic stays unmarked.
                    if let (Some(raw), Some(dest)) = (media_raw, remote) {
                        // qWAVE answers through a service: keep it off the runtime's workers
                        let flow =
                            tokio::task::spawn_blocking(move || sockets::mark_video_flow(raw, dest))
                                .await
                                .ok()
                                .flatten();
                        match flow {
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
                teprintln!("peer connection {state:?}, dumping ICE candidate pairs:");
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
                tprintln!("peer connection Disconnected, waiting 8s for recovery before closing");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(8)) => {
                        tprintln!("peer connection Disconnected grace period expired, closing session");
                        if let Some(tx) = closed_tx.lock_recover().take() {
                            let _ = tx.send(());
                        }
                    }
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            tprintln!("peer connection recovered before grace period expired, keeping session");
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
            transport: Some(transport),
            writer_stop: Some(writer_stop),
            input_tx: Some(input_tx),
            input_join: Some(input_join),
            _qos: qos_flow,
        },
    ))
}

/// Prepare and activate in one call. The serial shape, kept for the WHEP self-test and any caller
/// that has a pipeline before it has an offer.
#[allow(clippy::too_many_arguments)]
pub async fn handle_whep_offer(
    offer_sdp: String,
    join_id: &str,
    device_token: &str,
    pipeline: &Pipeline,
    ice_servers: Vec<RTCIceServer>,
    closed_tx: Option<tokio::sync::oneshot::Sender<()>>,
    input: InputParams,
    audio: Option<AudioParams>,
    locality: SessionLocality,
    video: negotiate::NegotiatedVideo,
    bind_ip: std::net::IpAddr,
) -> Result<(String, SessionHandle)> {
    let pending = prepare_whep(
        offer_sdp,
        join_id,
        device_token,
        ice_servers,
        locality,
        video,
        audio.as_ref(),
        bind_ip,
    )
    .await?;
    activate_whep(pending, pipeline, closed_tx, input, audio).await
}

/// Add the audio media the answer must carry, before `create_answer`: WHEP has no renegotiation
/// verb, so a track or data channel missing from the first answer can never be added later.
/// [`start_audio_forwarders`] feeds them once the pipeline exists.
async fn add_audio_tracks(
    pc: &Arc<webrtc::peer_connection::RTCPeerConnection>,
    fast: bool,
    fallback_track: bool,
) -> Result<(
    Option<Arc<TrackLocalStaticSample>>,
    Option<Arc<RTCDataChannel>>,
)> {
    // on the fast path the Opus track is the spare: it has to exist in the answer so the client
    // has an `m=audio` section to fall back onto if its WebCodecs decoder dies mid-session
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
    // on the fast path the track is the client's spare, fed in standby: nothing until the client
    // reports that it is playing the track (`audioPath` 3 in its stats frame)
    if let Some(track) = track {
        tokio::spawn(forward_track(
            Arc::clone(pc),
            track,
            audio.clone(),
            Arc::clone(&telemetry),
            audio.fast,
        ));
    }

    if let Some(dc) = dc {
        // `Weak`: the peer connection owns the channel this closure hangs on, so an `Arc` of
        // either is a cycle that a session which never connects never breaks
        let dc_for_open = Arc::downgrade(&dc);
        let pc_for_open = Arc::downgrade(pc);
        dc.on_open(Box::new(move || {
            let dc = dc_for_open.clone();
            let pc = pc_for_open.clone();
            let audio = audio.clone();
            let telemetry = Arc::clone(&telemetry);
            Box::pin(async move {
                let (Some(pc), Some(dc)) = (pc.upgrade(), dc.upgrade()) else {
                    return;
                };
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
    // sticky until a packet goes out: a producer-side skip (broadcast lag, backpressure drop)
    // must be flagged, or the client reads the `seq` jump as network loss, splices across it
    // with no resync and keeps the offset for good
    let mut pending_discontinuity = false;
    let mut prev_payload: Option<Bytes> = None;
    // a client that stepped down to the spare track has detached this channel's handler: the
    // track forwarder feeds it from then on, and feeding both would be the audio twice
    let mut stepped_down = false;
    let mut last_path_check = Instant::now();
    loop {
        if !pc_alive(&pc) || dc.ready_state() == RTCDataChannelState::Closed {
            break;
        }
        if audio.fallback_track
            && !stepped_down
            && last_path_check.elapsed() >= Duration::from_secs(1)
        {
            last_path_check = Instant::now();
            stepped_down = client_playing_track(&telemetry);
        }
        let want = !stepped_down && audio.audio_enabled_now();
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
                // checked every packet (sampling reacts up to 20 ms late in both directions):
                // `buffered_amount` is a read behind a lock in webrtc-rs, not a syscall
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
            // the subscriber fell behind the host-wide capture and the broadcast dropped
            // packets on its behalf: a real audio gap
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

/// `elapsed` rounded to the nearest whole number of frames, and never less than one: the RTP
/// timestamp step for a packet that carries exactly one frame.
fn whole_frames(elapsed: Duration, frame: Duration) -> Duration {
    let f = frame.as_nanos().max(1);
    let n = ((elapsed.as_nanos() + f / 2) / f).max(1);
    frame * n.min(u32::MAX as u128) as u32
}

/// `audioPath` 3 in the client's stats frame: it is playing the standard Opus track.
const CLIENT_AUDIO_PATH_OPUS_TRACK: u8 = 3;

fn client_playing_track(telemetry: &crate::streamer::telemetry::SessionTelemetry) -> bool {
    telemetry
        .client
        .get()
        .is_some_and(|c| c.audio_path == CLIENT_AUDIO_PATH_OPUS_TRACK)
}

async fn forward_track(
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    audio: AudioParams,
    telemetry: crate::streamer::telemetry::SharedTelemetry,
    mut standby: bool,
) {
    let frame_duration = Duration::from_millis(crate::streamer::audio::encoder::FRAME_MS as u64);
    // a longer gap is a device change or a stall, not a scheduling wobble: treat the timeline as
    // restarted instead of jumping the RTP clock by minutes
    const MAX_CATCHUP: Duration = Duration::from_millis(500);
    let mut last_capture_ns: Option<u64> = None;
    let mut sub: Option<audio::AudioSubscription> = None;
    loop {
        if !pc_alive(&pc) {
            break;
        }
        // the client latches its step-down: it never returns to the fast path in a session
        if standby && client_playing_track(&telemetry) {
            standby = false;
            tprintln!("audio: the client stepped down to the standard Opus track; feeding it");
        }
        let want = !standby && audio.audio_enabled_now();
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
                // `TrackLocalStaticSample` accumulates `duration` into the RTP timestamp, so a
                // fixed 5 ms leaves audio 5 ms behind the video per lost packet: use the real
                // elapsed `capture_ns`. Rounded to whole frames: the raw spacing wobbles with
                // capture scheduling (4.3 ms, 5.6 ms), and NetEQ conceals every timestamp that
                // disagrees with a packet's 240 samples (627,600 concealed samples in 22 s).
                let duration = match last_capture_ns {
                    Some(prev) if pkt.capture_ns > prev => {
                        whole_frames(Duration::from_nanos(pkt.capture_ns - prev), frame_duration)
                            .min(MAX_CATCHUP)
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
            // the subscriber fell behind the host-wide capture and the broadcast dropped
            // packets on its behalf: a real audio gap
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
    pipeline: SessionPipeline,
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
    // the estimator keeps its own value: fed back the emitted target, a sub-deadband cut is
    // suppressed, recomputed from the same target and suppressed again
    let mut estimate_bps = max_bps;
    let mut delay_estimate = DelayBasedEstimate::new(max_bps, min_bps, max_bps);
    let mut last_bytes_sent: Option<(u64, Instant)> = None;
    let mut rtt_monitor = RttCongestionMonitor::new();
    let mut signal_logged = false;
    // `fraction_lost` and `round_trip_time` come from RTCP receiver reports, about one a second,
    // and this loop polls every 120 ms: re-feeding an unchanged reading acts on one loss event
    // up to eight times, so the loss and RTT arms run only when the pair moved
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
                        // NOT the W3C unit: webrtc-rs's `round_trip_time` is already milliseconds
                        // (interceptor 0.17.1, `calculate_rtt_ms`). Scaled again it pins every
                        // session at the bitrate floor
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

            // for the audio forwarder, in hundredths of a percent
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

            // bit-exact on the raw doubles: any real receiver report moves at least one of them
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

            // transport-wide feedback, when negotiated, supersedes the receiver report: every
            // lost sequence number at the browser's own cadence, plus arrival times
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
                // nothing new: hold the estimate, since re-running the controller on a stale
                // reading turns one loss event into several cuts
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
                // the delay signal fires before loss does, so it may only lower the estimate,
                // never raise it above what the loss controller allows
                estimate = estimate.min(delay_bps.max(min_bps));
            } else if report_fresh && rtt_monitor.observe(max_rtt_ms) {
                // no transport-wide feedback: the round-trip excursion detector is coarse, but
                // it is all a client without the extension gives
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

            // a viewer the local broadcast dropped frames on has an oversubscribed link:
            // cutting here keeps the keyframe that follows small
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

/// The video RTP timestamp actually written: `candidate` (the host clock at 90 kHz), unless it is
/// not strictly ahead of the previous one (wrapping comparison), in which case one tick after the
/// previous one. Returns whether it had to clamp.
pub(crate) fn next_video_rtp_ts(last: Option<u32>, candidate: u32) -> (u32, bool) {
    match last {
        Some(l) if (candidate.wrapping_sub(l) as i32) <= 0 => (l.wrapping_add(1), true),
        _ => (candidate, false),
    }
}

/// Test hook: with `SE_TEST_HOOKS=1` and `SE_DUMP_AU=<path>`, the first access units the writer
/// packetizes, exactly as it receives them, appended to one Annex-B file.
#[cfg(feature = "bench")]
struct AuDump {
    file: Option<std::fs::File>,
    left: u32,
}

#[cfg(feature = "bench")]
impl AuDump {
    const FRAMES: u32 = 600;

    fn from_env() -> Self {
        let hooks = crate::envcfg::var("SE_TEST_HOOKS").as_deref() == Ok("1");
        let file = crate::envcfg::var("SE_DUMP_AU")
            .ok()
            .filter(|_| hooks)
            .and_then(|p| std::fs::File::create(p).ok());
        Self {
            file,
            left: Self::FRAMES,
        }
    }

    fn write(&mut self, au: &[u8]) {
        use std::io::Write as _;
        let Some(f) = self.file.as_mut() else { return };
        let _ = f.write_all(au);
        self.left -= 1;
        if self.left == 0 {
            self.file = None;
        }
    }
}

#[cfg(test)]
mod tests {
    /// A loss answered by a long-term-reference repair leaves no keyframe request behind; one
    /// answered by an IDR is tagged as the lagging viewer's.
    #[cfg(windows)]
    #[test]
    fn a_repair_from_a_long_term_reference_leaves_no_keyframe_origin_behind() {
        use crate::windows_utils::streamer::governor::key_origin;
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::Arc;
        struct Control {
            repair: AtomicBool,
            idrs: AtomicU32,
        }
        impl super::EncoderControl for Control {
            fn request_idr(&self) {
                self.idrs.fetch_add(1, Ordering::Relaxed);
            }
            fn request_repair(&self) -> bool {
                self.repair.load(Ordering::Relaxed)
            }
            fn set_target_bitrate(&self, _bps: u32) {}
        }
        for repair in [true, false] {
            let control = Arc::new(Control {
                repair: AtomicBool::new(repair),
                idrs: AtomicU32::new(0),
            });
            let pipeline = super::SessionPipeline::without_encoder(control.clone());
            let k = super::KeyframeRequester {
                pipeline: pipeline.clone(),
                gate: Arc::new(crate::streamer::keyframe::KeyframeGate::new()),
            };
            assert!(k.request("a lagging viewer"));
            let origins = pipeline.telemetry().link.key_origins_pending();
            if repair {
                assert_eq!((origins, control.idrs.load(Ordering::Relaxed)), (0, 0));
            } else {
                assert_eq!(origins, key_origin::LAG);
                assert_eq!(control.idrs.load(Ordering::Relaxed), 1);
            }
        }
    }

    #[test]
    fn the_tuned_media_socket_stays_opt_in() {
        if crate::envcfg::var_os("SCREENEXTEND_MEDIA_SOCKET").is_some()
            || crate::envcfg::var_os("SE_TUNE_ON").is_some()
        {
            crate::test_support::skip_because(
                "the_tuned_media_socket_stays_opt_in",
                "the environment opts in",
            );
            return;
        }
        assert!(!super::media_socket_enabled());
    }

    #[cfg(windows)]
    #[test]
    fn a_precise_pacer_wait_sleeps_its_time_and_leaves_the_runtime_free() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let spins = Arc::new(AtomicU64::new(0));
            let counter = Arc::clone(&spins);
            let other = tokio::spawn(async move {
                loop {
                    counter.fetch_add(1, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                }
            });
            let wait = Duration::from_micros(800);
            let started = Instant::now();
            super::pace_sleep_precise(wait).await;
            assert!(started.elapsed() >= wait);
            other.abort();
            assert!(
                spins.load(Ordering::Relaxed) > 0,
                "another task ran on the only worker while the pacer waited"
            );
        });
    }

    #[test]
    fn the_video_rtp_timestamp_never_goes_backwards() {
        use super::next_video_rtp_ts as next;
        // the first frame and a normal increase are untouched
        assert_eq!(next(None, 12_345), (12_345, false));
        assert_eq!(next(Some(1_000), 2_500), (2_500, false));
        // equal: one tick on
        assert_eq!(next(Some(1_000), 1_000), (1_001, true));
        // slightly behind (a real frame captured just before a repeat's submit)
        assert_eq!(next(Some(1_000), 990), (1_001, true));
        // across the 32-bit wrap: ahead stays ahead, behind is clamped, the +1 wraps too
        assert_eq!(next(Some(u32::MAX - 10), 5), (5, false));
        assert_eq!(next(Some(5), u32::MAX - 10), (6, true));
        assert_eq!(next(Some(u32::MAX), u32::MAX), (0, true));
        assert_eq!(next(Some(u32::MAX), 0), (0, false));
    }

    use super::*;

    #[test]
    fn a_relayed_session_gets_a_smaller_mtu_than_a_lan_one() {
        assert_eq!(RTP_PAYLOAD_MTU_LAN, 1188);
        assert_eq!(RTP_PAYLOAD_MTU_RELAY, 1012);
    }

    #[test]
    fn the_chosen_profile_leads_the_registered_codec_list() {
        // the track binds to the first codec whose fmtp matches, so High must be registered
        // first, and Baseline still registered for a client that turns out to offer nothing else
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
            device_token: "test".to_string(),
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

    /// Each packet carries one 5 ms frame, so its RTP step must be a whole number of frames:
    /// scheduling wobble rounds away, a real gap still advances the clock.
    #[test]
    fn the_track_clock_steps_in_whole_frames() {
        let f = Duration::from_millis(5);
        assert_eq!(whole_frames(Duration::from_micros(4_300), f), f);
        assert_eq!(whole_frames(Duration::from_micros(5_600), f), f);
        assert_eq!(whole_frames(Duration::from_micros(1), f), f);
        assert_eq!(whole_frames(Duration::from_micros(9_800), f), f * 2);
        assert_eq!(whole_frames(Duration::from_millis(41), f), f * 8);
    }

    /// A spare track on the fast path is fed only once the client says it is playing it.
    #[test]
    fn the_spare_track_waits_for_the_client_to_step_down() {
        let t = crate::streamer::telemetry::SessionTelemetry::default();
        assert!(!client_playing_track(&t));
        t.set_client_stats(crate::streamer::telemetry::ClientStats {
            audio_path: 1,
            ..Default::default()
        });
        assert!(!client_playing_track(&t));
        t.set_client_stats(crate::streamer::telemetry::ClientStats {
            audio_path: CLIENT_AUDIO_PATH_OPUS_TRACK,
            ..Default::default()
        });
        assert!(client_playing_track(&t));
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
        assert_eq!(
            add_trickled_candidates("deadbeef", "10.0.0.1", "a=end-of-candidates").await,
            TrickleLookup::Unknown
        );
    }

    #[test]
    fn a_resource_answers_only_to_the_device_that_opened_it_and_only_while_it_lives() {
        // the registry is the whole authority check, so no peer connection is needed: `DELETE`
        // resolves through `trickle_owner`, and `PATCH` applies the same two tests before it
        // touches ICE
        let id = "0123456789abcdef0123456789abcdef";
        let mine = "aaaaaaaabbbbbbbbccccccccdddddddd";
        let theirs = "11111111222222223333333344444444";
        {
            let mut guard = TRICKLE_SESSIONS.lock_recover();
            let map = guard.get_or_insert_with(Default::default);
            map.insert(
                id.to_string(),
                TrickleEntry {
                    // a dangling `Weak` is the state a finished session leaves behind
                    pc: std::sync::Weak::new(),
                    join_id: id.to_string(),
                    device_token: mine.to_string(),
                    fragments: 0,
                },
            );
        }
        assert_eq!(
            trickle_owner(id, theirs),
            None,
            "another device must not resolve someone else's resource"
        );
        assert_eq!(
            trickle_owner(id, ""),
            None,
            "a caller presenting no token must not resolve anything"
        );
        assert_eq!(
            trickle_owner("ffffffffffffffffffffffffffffffff", mine),
            None,
            "an id that was never minted must not resolve"
        );
        assert_eq!(
            trickle_owner(id, mine),
            None,
            "a resource whose session is gone must not resolve, even to its own device"
        );
        if let Some(m) = TRICKLE_SESSIONS.lock_recover().as_mut() {
            m.remove(id);
        }
        assert!(TRICKLE_SESSIONS
            .lock_recover()
            .as_ref()
            .is_none_or(|m| !m.contains_key(id)));
    }

    #[tokio::test]
    async fn a_renegotiation_hands_back_the_transport_it_replaces_and_teardown_keeps_the_new_one() {
        // both transports register under the one join id: the superseded one is handed back to
        // be closed, and its teardown must not remove the new session's entry
        let api = APIBuilder::new().build();
        let old = Arc::new(api.new_peer_connection(Default::default()).await.unwrap());
        let new = Arc::new(api.new_peer_connection(Default::default()).await.unwrap());
        let id = "fedcba9876543210fedcba9876543210";
        let token = "aaaaaaaabbbbbbbbccccccccdddddddd";

        assert!(
            register_trickle(id, id, token, &old).is_none(),
            "a first join replaces nothing"
        );
        let replaced = register_trickle(id, id, token, &new).expect("the old transport");
        assert!(std::ptr::eq(replaced.as_ptr(), Arc::as_ptr(&old)));
        assert!(
            register_trickle(id, id, token, &new).is_none(),
            "re-registering the same transport is not a replacement"
        );

        unregister_trickle(id, &old);
        assert_eq!(
            trickle_owner(id, token).as_deref(),
            Some(id),
            "the superseded session's teardown must leave the live one addressable"
        );
        unregister_trickle(id, &new);
        assert_eq!(trickle_owner(id, token), None);
        let _ = old.close().await;
        let _ = new.close().await;
    }

    /// The body `trickleFragment()` in `static/index.html` actually writes, byte for byte:
    /// `scripts/test-client-ice.mjs` runs the shipped function, writes this fixture and fails
    /// if the file no longer matches.
    #[test]
    fn the_client_fragment_shape_parses_whole() {
        let body = include_str!("test/trickle-fragment.sdpfrag");
        let f = parse_trickle_fragment(body);
        assert_eq!(f.ufrag.as_deref(), Some("UF01"));
        assert_eq!(f.candidates.len(), 2);
        for c in &f.candidates {
            assert_eq!(c.1.as_deref(), Some("0"), "every candidate rides mid 0");
            assert_eq!(c.2, Some(0), "one m= section means m-line index 0");
        }
        assert!(f.end_of_candidates);
        assert!(
            f.candidates.iter().any(|c| c.0.contains("typ relay")),
            "the fixture must carry the relay candidate, which is the one trickle exists for"
        );
        // SDP is CRLF (RFC 4566 section 5). The parser tolerates bare LF, so a checkout that
        // rewrote the endings would still pass everything above.
        assert_eq!(
            body.matches('\n').count(),
            body.matches("\r\n").count(),
            "the fixture must keep its CRLF endings; see .gitattributes"
        );
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

    // two in-process peer connections negotiate, with no display, encoder or network: checks
    // that the MediaEngine `build_api` registers still produces what `whep_selftest.rs` expects

    async fn negotiate_answer(profile: H264Profile) -> Option<String> {
        use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
        use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

        let api = build_api(
            profile,
            None,
            std::net::Ipv4Addr::UNSPECIFIED.into(),
            rtx_budget_enabled(),
        )
        .expect("build_api");
        // a peer connection binds UDP sockets for its ICE agent, which a locked-down box may
        // refuse: a skip, not a failure of the SDP under test
        let client = match api.new_peer_connection(RTCConfiguration::default()).await {
            Ok(pc) => Arc::new(pc),
            Err(e) => {
                crate::test_support::skip("webrtc SDP negotiation (no peer connection)", e);
                return None;
            }
        };
        let host = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("host peer connection"),
        );

        client
            .add_transceiver_from_kind(
                RTPCodecType::Video,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: vec![],
                }),
            )
            .await
            .expect("recvonly video transceiver");

        // the same raw RTP track the live session adds, so the answer describes the real thing
        let track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "screenextend".to_owned(),
        ));
        host.add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .expect("add video track");

        let offer = client.create_offer(None).await.expect("offer");
        client
            .set_local_description(offer.clone())
            .await
            .expect("set local offer");
        host.set_remote_description(offer)
            .await
            .expect("set remote offer");
        let answer = host.create_answer(None).await.expect("answer");

        client.close().await.ok();
        host.close().await.ok();
        Some(answer.sdp)
    }

    #[tokio::test]
    async fn the_media_engine_still_answers_with_everything_a_browser_needs() {
        let Some(sdp) = negotiate_answer(H264Profile::Baseline).await else {
            return;
        };
        for (name, ok) in crate::streamer::test::whep_selftest::answer_sdp_checks(&sdp) {
            assert!(ok, "the negotiated answer is missing {name}:\n{sdp}");
        }
    }

    #[tokio::test]
    async fn each_h264_profile_negotiates_its_own_level_id() {
        // a wrong level-id gives a client that refuses the stream or decodes it in software
        for (profile, plid) in [
            (H264Profile::Baseline, "42e01f"),
            (H264Profile::Main, "4d001f"),
            (H264Profile::High, "640c1f"),
        ] {
            let Some(sdp) = negotiate_answer(profile).await else {
                return;
            };
            assert!(
                sdp.contains(plid),
                "{profile:?} should negotiate profile-level-id={plid}:\n{sdp}"
            );
        }
    }

    #[test]
    fn the_fmtp_line_says_packetization_mode_1_and_allows_level_asymmetry() {
        // without `packetization-mode=1` a browser will not accept the fragmented NAL units the
        // payloader emits; without level asymmetry a phone that decodes less than it encodes
        // refuses the whole m-line
        let line = h264_fmtp("42e01f");
        assert!(line.contains("packetization-mode=1"), "{line}");
        assert!(line.contains("level-asymmetry-allowed=1"), "{line}");
        assert!(line.contains("profile-level-id=42e01f"), "{line}");
    }

    #[test]
    fn every_codec_gets_its_own_rtx_payload_type() {
        // two codecs sharing an apt, or an rtx pt colliding with another codec's, silently
        // disables loss recovery
        for profile in [H264Profile::Baseline, H264Profile::Main, H264Profile::High] {
            let codecs = h264_codecs(profile);
            assert!(!codecs.is_empty(), "{profile:?} registers no codec");
            let mut seen: Vec<PayloadType> = Vec::new();
            for (pt, rtx, plid) in &codecs {
                assert_ne!(
                    pt, rtx,
                    "{profile:?}: a codec and its RTX share a payload type"
                );
                assert_eq!(
                    plid.len(),
                    6,
                    "{profile:?}: {plid} is not a profile-level-id"
                );
                assert!(!seen.contains(pt), "{profile:?}: payload type {pt} reused");
                assert!(
                    !seen.contains(rtx),
                    "{profile:?}: payload type {rtx} reused"
                );
                seen.push(*pt);
                seen.push(*rtx);
            }
        }
    }

    #[test]
    fn private_and_link_local_addresses_are_recognised_in_both_families() {
        for ip in [
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "169.254.10.20",
            "127.0.0.1",
            "::1",
            "fe80::1",
        ] {
            assert!(is_private_ip(ip), "{ip} should be private/link-local");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "2001:4860:4860::8888",
            "not-an-ip",
            "",
        ] {
            assert!(!is_private_ip(ip), "{ip} should not be private");
        }
    }

    #[test]
    fn the_same_subnet_check_is_ipv4_only_and_compares_three_octets() {
        // it exists to spot AP client isolation (two devices on one /24 that still went through
        // a relay), and a false positive prints advice about a router that is not the problem
        assert!(same_subnet_24("192.168.1.10", "192.168.1.200"));
        assert!(!same_subnet_24("192.168.1.10", "192.168.2.10"));
        assert!(!same_subnet_24("192.168.1.10", "10.0.0.10"));
        assert!(!same_subnet_24("fe80::1", "fe80::2"), "IPv6 has no /24");
        assert!(!same_subnet_24("192.168.1.10", "garbage"));
    }

    #[test]
    fn the_candidate_summary_counts_types_and_names_the_routable_ones() {
        let sdp = "\
v=0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
a=candidate:1 1 udp 2130706431 192.168.1.10 50000 typ host\r\n\
a=candidate:2 1 udp 2130706431 192.168.1.10 50001 typ host\r\n\
a=candidate:3 1 udp 1694498815 203.0.113.7 50002 typ srflx raddr 192.168.1.10 rport 50000\r\n\
a=candidate:4 1 udp 16777215 198.51.100.9 50003 typ relay raddr 203.0.113.7 rport 50002\r\n";
        let s = summarize_sdp_candidates(sdp);
        assert!(s.contains("host=2"), "{s}");
        assert!(s.contains("srflx=1"), "{s}");
        assert!(s.contains("relay=1"), "{s}");
        // the routable ones are named with their transport and address
        assert!(s.contains("relay/udp 198.51.100.9:50003"), "{s}");
        assert!(s.contains("srflx/udp 203.0.113.7:50002"), "{s}");
    }

    #[test]
    fn a_host_only_sdp_says_so_rather_than_looking_healthy() {
        let sdp = "a=candidate:1 1 udp 2130706431 192.168.1.10 50000 typ host\r\n";
        let s = summarize_sdp_candidates(sdp);
        assert!(s.contains("host=1"), "{s}");
        assert!(s.contains("no public/relay candidates"), "{s}");
    }

    #[test]
    fn an_sdp_with_no_candidates_at_all_is_reported_as_such() {
        assert_eq!(
            summarize_sdp_candidates("v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\n"),
            "no ICE candidates in SDP"
        );
        assert_eq!(summarize_sdp_candidates(""), "no ICE candidates in SDP");
    }

    #[test]
    fn a_trickled_candidate_line_is_summarised_the_same_way() {
        // a candidate arriving over the data channel has no `a=` prefix, and the summary is used
        // on both spellings
        let s = summarize_sdp_candidates("candidate:1 1 udp 2130706431 10.0.0.5 4000 typ host\r\n");
        assert!(s.contains("host=1"), "{s}");
    }

    #[test]
    fn a_malformed_candidate_is_counted_rather_than_dropping_the_summary() {
        let s = summarize_sdp_candidates("a=candidate:garbage\r\n");
        assert!(
            s.contains("?=1"),
            "an unparseable candidate should still be counted: {s}"
        );
    }
}

/// The most candidate lines one fragment may carry. The shipped client batches 24; a body at the
/// 64 KiB `MAX_SDP_LEN` could otherwise hold several hundred, each awaited individually.
const MAX_TRICKLE_CANDIDATES: usize = 64;
/// The most fragments one session may ever be sent. Gathering on any real client is over inside a
/// few seconds and a handful of fragments; this is the ceiling that stops the route being an
/// unbounded `add_ice_candidate` driver.
const MAX_TRICKLE_FRAGMENTS: u32 = 128;

struct TrickleEntry {
    /// `Weak`, so a session that ends drops out of here without anyone removing it, and this
    /// map can never be what keeps a peer connection alive.
    pc: std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>,
    /// The session this resource is. Identical to the resource id today; kept as its own field
    /// so a caller reads a name rather than an assumption.
    join_id: String,
    /// The device that opened the join. The id travels in a cleartext `Location` header on the
    /// HTTP listener, so it alone is not the authority, and the second factor is the token, not
    /// the peer's address: an address moves under a live session and is recycled between devices.
    device_token: String,
    /// Fragments accepted so far, against `MAX_TRICKLE_FRAGMENTS`.
    fragments: u32,
}

/// Live sessions addressable by their WHEP resource id, for trickle ICE.
static TRICKLE_SESSIONS: Mutex<Option<std::collections::HashMap<String, TrickleEntry>>> =
    Mutex::new(None);

/// What a `PATCH`/`DELETE` on a WHEP resource resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum TrickleLookup {
    /// The id is live and belongs to this peer.
    Found,
    /// No such id, or it belongs to someone else. Deliberately the same answer for both, so a
    /// probe cannot tell a wrong id from a wrong source.
    Unknown,
    /// The session's fragment budget is spent.
    Exhausted,
}

/// Register `pc` as the live transport of `id`, and hand back the one it replaces, if any.
///
/// The resource id *is* the join id, so a renegotiation in place registers a second peer
/// connection under it. The replaced one is returned so the new session can close it once it
/// is itself connected.
fn register_trickle(
    id: &str,
    join_id: &str,
    device_token: &str,
    pc: &Arc<webrtc::peer_connection::RTCPeerConnection>,
) -> Option<std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>> {
    let mut guard = TRICKLE_SESSIONS.lock_recover();
    let map = guard.get_or_insert_with(Default::default);
    map.retain(|_, e| e.pc.strong_count() > 0);
    map.insert(
        id.to_string(),
        TrickleEntry {
            pc: Arc::downgrade(pc),
            join_id: join_id.to_string(),
            device_token: device_token.to_string(),
            fragments: 0,
        },
    )
    .map(|old| old.pc)
    .filter(|old| old.strong_count() > 0 && !std::ptr::eq(old.as_ptr(), Arc::as_ptr(pc)))
}

/// Forget `id`, but only while it still names `pc`: a superseded session's teardown runs after
/// its replacement registered under the same id.
fn unregister_trickle(id: &str, pc: &Arc<webrtc::peer_connection::RTCPeerConnection>) {
    if let Some(map) = TRICKLE_SESSIONS.lock_recover().as_mut() {
        if map
            .get(id)
            .is_some_and(|e| std::ptr::eq(e.pc.as_ptr(), Arc::as_ptr(pc)))
        {
            map.remove(id);
        }
    }
}

#[cfg(test)]
pub(crate) fn trickle_entries() -> usize {
    TRICKLE_SESSIONS
        .lock_recover()
        .as_ref()
        .map_or(0, |m| m.len())
}

/// The join a live resource names, when it belongs to `device_token`.
///
/// `DELETE` (RFC 9725 section 4.4) resolves through this, so the id carries authority only
/// while the resource is live and the caller presents the device token that opened it.
pub fn trickle_owner(id: &str, device_token: &str) -> Option<String> {
    let guard = TRICKLE_SESSIONS.lock_recover();
    let e = guard.as_ref()?.get(id)?;
    if e.pc.strong_count() == 0 || e.device_token.is_empty() || e.device_token != device_token {
        return None;
    }
    Some(e.join_id.clone())
}

/// One `a=candidate:` line's worth of trickled ICE, as RFC 9725 sends it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TrickleFragment {
    pub candidates: Vec<(String, Option<String>, Option<u16>)>,
    pub ufrag: Option<String>,
    /// The fragment said the client has finished gathering (`a=end-of-candidates`).
    pub end_of_candidates: bool,
}

/// Parse an `application/trickle-ice-sdpfrag` body: a partial SDP of zero or more `m=`
/// sections, each with an optional `a=mid:` and some `a=candidate:` lines, plus `a=ice-ufrag:`.
/// The mid and the m-line index are carried through for a client that does not bundle.
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

/// Feed trickled candidates to a live session. The resource must be live *and* belong to
/// `device_token`, with fragment budget left: the id rides a cleartext header on the HTTP
/// listener, so it alone must not let an observer redirect media.
pub async fn add_trickled_candidates(id: &str, device_token: &str, body: &str) -> TrickleLookup {
    use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

    // the budget is charged under the same lock that resolves the session, so concurrent
    // fragments cannot both read the last one
    let pc = {
        let mut guard = TRICKLE_SESSIONS.lock_recover();
        let Some(entry) = guard.as_mut().and_then(|m| m.get_mut(id)) else {
            return TrickleLookup::Unknown;
        };
        if entry.device_token.is_empty() || entry.device_token != device_token {
            return TrickleLookup::Unknown;
        }
        let Some(pc) = entry.pc.upgrade() else {
            return TrickleLookup::Unknown;
        };
        if entry.fragments >= MAX_TRICKLE_FRAGMENTS {
            return TrickleLookup::Exhausted;
        }
        entry.fragments += 1;
        pc
    };

    let mut frag = parse_trickle_fragment(body);
    if frag.candidates.len() > MAX_TRICKLE_CANDIDATES {
        teprintln!(
            "trickle: fragment carried {} candidates; keeping the first {MAX_TRICKLE_CANDIDATES}",
            frag.candidates.len()
        );
        frag.candidates.truncate(MAX_TRICKLE_CANDIDATES);
    }
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
    if frag.end_of_candidates {
        // an empty candidate is how webrtc-rs spells `add_remote_candidate(None)`: remote
        // gathering is over, so the agent can fail the checklist without waiting out its timers
        if let Err(e) = pc
            .add_ice_candidate(RTCIceCandidateInit {
                candidate: String::new(),
                ..Default::default()
            })
            .await
        {
            teprintln!("trickle: end-of-candidates rejected ({e})");
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
    TrickleLookup::Found
}

//! A session that ends must give everything back: its ICE socket, its tasks, its peer connection.
//!
//! In process and on loopback: the real `prepare_whep` / `activate_session` / `SessionHandle` on
//! one runtime, a webrtc-rs client on a runtime of its own per join, and a thread standing in for
//! the encoder. No display, no capture, no encoder. A client that has to vanish without a word is
//! its runtime being shut down under it, which is as abrupt as a killed process.
//!
//! What is counted is the kernel's answer (descriptors that are UDP sockets bound to a port one of
//! these sessions advertised), tokio's (tasks alive on the host's runtime, which nothing else
//! uses) and `Arc`'s (anything still holding a connection, or the audio hub through a session's
//! closures), not our own bookkeeping.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, oneshot};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

use crate::streamer::audio::AudioHub;
use crate::streamer::input::{self, protocol};
use crate::streamer::lock::LockExt;
use crate::streamer::negotiate;
use crate::streamer::pipeline::EncodedFrame;
use crate::streamer::session::SharedDeviceOverrides;
use crate::streamer::telemetry::FrameTiming;
use crate::streamer::webrtc_session::{
    self, activate_session, prepare_whep, AudioParams, EncoderControl, InputParams, PendingWhep,
    RTCIceServer, SessionHandle, SessionLocality, SessionPipeline,
};

const LOOPBACK: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
const TOKEN: &str = "aaaaaaaabbbbbbbbccccccccdddddddd";

/// The descriptor table is the whole process's, so these tests take turns.
static TURN: Mutex<()> = Mutex::new(());

/// Every UDP port this process has open, read back from the kernel.
fn open_udp_ports() -> Vec<u16> {
    let mut ports = Vec::new();
    let Ok(dir) = std::fs::read_dir("/dev/fd") else {
        return ports;
    };
    for fd in dir
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()))
    {
        let mut kind: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                std::ptr::addr_of_mut!(kind).cast(),
                &mut len,
            )
        };
        if rc != 0 || kind != libc::SOCK_DGRAM {
            continue;
        }
        let mut addr: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
        let rc = unsafe { libc::getsockname(fd, std::ptr::addr_of_mut!(addr).cast(), &mut len) };
        if rc != 0 {
            continue;
        }
        // `sin_port` and `sin6_port` are at the same offset.
        if matches!(
            addr.sin6_family as libc::c_int,
            libc::AF_INET | libc::AF_INET6
        ) {
            ports.push(u16::from_be(addr.sin6_port));
        }
    }
    ports
}

fn candidate_ports(sdp: &str) -> Vec<u16> {
    sdp.lines()
        .filter_map(|l| l.trim().strip_prefix("a=candidate:"))
        .filter_map(|c| c.split_whitespace().nth(5)?.parse().ok())
        .collect()
}

fn rss_mb() -> f64 {
    std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok()?.trim().parse::<f64>().ok())
        .map_or(0.0, |kb| kb / 1024.0)
}

fn wait_for(what: &str, limit: Duration, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < limit, "{what}: not within {limit:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An encoder's controls with no encoder. It keeps the frame sender the way a `Pipeline` clone
/// does, so nothing here may rely on the frame channel closing.
struct Controls {
    idrs: AtomicU64,
    _frames: broadcast::Sender<EncodedFrame>,
}

impl EncoderControl for Controls {
    fn request_idr(&self) {
        self.idrs.fetch_add(1, Ordering::Relaxed);
    }
    fn request_repair(&self) -> bool {
        false
    }
    fn set_target_bitrate(&self, _bps: u32) {}
}

/// One access unit a packetizer accepts, every 8 ms.
struct Feed {
    frames: broadcast::Sender<EncodedFrame>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Feed {
    fn start() -> Self {
        let (frames, _) = broadcast::channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let tx = frames.clone();
        let stopped = Arc::clone(&stop);
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        au.resize(au.len() + 4000, 0x5a);
        let au = Bytes::from(au);
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let _ = tx.send(EncodedFrame {
                    data: au.clone(),
                    capture: Instant::now(),
                    timing: FrameTiming::entering_capture(),
                });
                std::thread::sleep(Duration::from_millis(8));
            }
        });
        Self {
            frames,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Host {
    rt: Runtime,
    hub: crate::streamer::audio::SharedAudioHub,
    overrides: SharedDeviceOverrides,
    ports: BTreeSet<u16>,
    /// A killed client's own sockets stay open in this process, where nothing plays the kernel
    /// reaping a dead one, so they are left out of the total.
    client_ports: BTreeSet<u16>,
    pcs: Vec<Weak<RTCPeerConnection>>,
    tasks_before: usize,
    sockets_before: usize,
    trickle_before: usize,
    live_before: i64,
}

struct Live {
    handle: SessionHandle,
    closed: oneshot::Receiver<()>,
    feed: Feed,
    answer: String,
}

impl Host {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("host runtime");
        Self {
            hub: AudioHub::new(),
            overrides: Default::default(),
            ports: BTreeSet::new(),
            client_ports: BTreeSet::new(),
            pcs: Vec::new(),
            tasks_before: rt.metrics().num_alive_tasks(),
            sockets_before: open_udp_ports().len(),
            trickle_before: webrtc_session::trickle_entries(),
            live_before: webrtc_session::live_sessions(),
            rt,
        }
    }

    fn audio(&self) -> AudioParams {
        AudioParams {
            fast: true,
            fallback_track: true,
            redundancy: true,
            link_loss: None,
            locality: SessionLocality::SameNetwork,
            hub: Arc::clone(&self.hub),
            overrides: Arc::clone(&self.overrides),
            device_token: TOKEN.to_string(),
        }
    }

    async fn negotiate(
        offer: String,
        join_id: &str,
        audio: AudioParams,
        ice_servers: Vec<RTCIceServer>,
    ) -> anyhow::Result<PendingWhep> {
        let video = negotiate::choose(&negotiate::read_offer(&offer), 1280, 720, 60);
        prepare_whep(
            offer,
            join_id,
            TOKEN,
            ice_servers,
            SessionLocality::SameNetwork,
            video,
            Some(&audio),
            LOOPBACK,
        )
        .await
    }

    fn prepare(&self, offer: &str, join_id: &str) -> anyhow::Result<PendingWhep> {
        self.prepare_with(offer, join_id, vec![])
    }

    fn prepare_with(
        &self,
        offer: &str,
        join_id: &str,
        ice_servers: Vec<RTCIceServer>,
    ) -> anyhow::Result<PendingWhep> {
        self.rt.block_on(Self::negotiate(
            offer.to_string(),
            join_id,
            self.audio(),
            ice_servers,
        ))
    }

    async fn activate_on(pending: PendingWhep, audio: AudioParams) -> Live {
        let feed = Feed::start();
        let controls = Arc::new(Controls {
            idrs: AtomicU64::new(0),
            _frames: feed.frames.clone(),
        });
        let (closed_tx, closed) = oneshot::channel();
        let (answer, handle) = activate_session(
            pending,
            feed.frames.subscribe(),
            SessionPipeline::without_encoder(controls),
            Some(closed_tx),
            InputParams {
                device: None,
                client_os: String::new(),
                live: input::LiveInput::shared(false, Default::default()),
                control_rx: None,
            },
            Some(audio),
        )
        .await
        .expect("activate");
        Live {
            handle,
            closed,
            feed,
            answer,
        }
    }

    fn activate(&mut self, pending: PendingWhep) -> Live {
        let live = self.rt.block_on(Self::activate_on(pending, self.audio()));
        self.ports.extend(candidate_ports(&live.answer));
        self.pcs.push(live.handle.peer_connection());
        live
    }

    fn join(&mut self, client: &Client, join_id: &str) -> Live {
        self.client_ports.extend(candidate_ports(&client.offer));
        let pending = self.prepare(&client.offer, join_id).expect("prepare");
        let live = self.activate(pending);
        assert!(
            !candidate_ports(&live.answer).is_empty(),
            "the answer carries no candidate:\n{}",
            live.answer
        );
        client.apply(&live.answer);
        client.wait_for_media();
        live
    }

    /// What the server's teardown task does, in its order: the transport, then the capture.
    fn end(&self, live: Live) {
        self.rt.block_on(live.handle.close());
        drop(live.feed);
    }

    /// The server only tears a session down once its connection has reported itself gone.
    fn end_when_reported_closed(&self, live: Live, limit: Duration) {
        let Live {
            handle,
            closed,
            feed,
            ..
        } = live;
        self.rt.block_on(async {
            tokio::time::timeout(limit, closed)
                .await
                .expect("the host never noticed the client had gone")
                .ok();
        });
        self.rt.block_on(handle.close());
        drop(feed);
    }

    fn leaked_ports(&self) -> Vec<u16> {
        open_udp_ports()
            .into_iter()
            .filter(|p| self.ports.contains(p))
            .collect()
    }

    /// Every UDP socket in the process that is not a client's.
    fn host_udp_sockets(&self) -> usize {
        open_udp_ports()
            .into_iter()
            .filter(|p| !self.client_ports.contains(p))
            .count()
    }

    fn alive(&self) -> usize {
        self.pcs.iter().filter(|w| w.strong_count() > 0).count()
    }

    fn report(&self, label: &str, rss_before: f64) -> String {
        format!(
            "{label}: {} session(s) -> leaked host sockets {}, live connections {} \
             (strong counts {:?}), host tasks {} (before {}), process UDP sockets {} (before {}), \
             trickle entries {} (before {}), live_sessions {} (before {}), audio hub handles {}, \
             rss {:.1} MB (before {rss_before:.1})",
            self.pcs.len(),
            self.leaked_ports().len(),
            self.alive(),
            self.pcs
                .iter()
                .map(Weak::strong_count)
                .filter(|n| *n > 0)
                .collect::<Vec<_>>(),
            self.rt.metrics().num_alive_tasks(),
            self.tasks_before,
            self.host_udp_sockets(),
            self.sockets_before,
            webrtc_session::trickle_entries(),
            self.trickle_before,
            webrtc_session::live_sessions(),
            self.live_before,
            Arc::strong_count(&self.hub),
            rss_mb(),
        )
    }

    fn assert_everything_came_back(&self, label: &str, rss_before: f64) {
        // Teardown is asynchronous in places (a task notices its channel closed on its next poll,
        // the bitrate driver within 120 ms, an audio forwarder within 250 ms), so this is "comes
        // back", not "is already back".
        let deadline = Instant::now() + Duration::from_secs(20);
        let clean = || {
            self.leaked_ports().is_empty()
                && self.alive() == 0
                && self.rt.metrics().num_alive_tasks() <= self.tasks_before
                && self.host_udp_sockets() <= self.sockets_before + 1
                && webrtc_session::live_sessions() == self.live_before
                && webrtc_session::trickle_entries() <= self.trickle_before
                && Arc::strong_count(&self.hub) == 1
        };
        while !clean() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let report = self.report(label, rss_before);
        eprintln!("[session-leak] {report}");
        assert!(clean(), "{report}");
    }
}

struct Client {
    rt: Option<Runtime>,
    pc: Arc<RTCPeerConnection>,
    reliable: Arc<RTCDataChannel>,
    rtp: Arc<AtomicU64>,
    pongs: Arc<AtomicU64>,
    offer: String,
}

impl Client {
    /// A page with an offer ready: receive-only video and audio, and the three input channels.
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("client runtime");
        let rtp = Arc::new(AtomicU64::new(0));
        let pongs = Arc::new(AtomicU64::new(0));
        let (pc, reliable, offer) = rt.block_on(async {
            let mut m = MediaEngine::default();
            m.register_default_codecs().unwrap();
            let registry = register_default_interceptors(Registry::new(), &mut m).unwrap();
            let mut se = SettingEngine::default();
            se.set_include_loopback_candidate(true);
            // Each agent otherwise holds an mDNS socket as well, on the one port they all share.
            se.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
            se.set_ip_filter(Box::new(|ip: std::net::IpAddr| ip == LOOPBACK));
            let api = APIBuilder::new()
                .with_media_engine(m)
                .with_interceptor_registry(registry)
                .with_setting_engine(se)
                .build();
            let pc = Arc::new(api.new_peer_connection(Default::default()).await.unwrap());
            for kind in [RTPCodecType::Video, RTPCodecType::Audio] {
                pc.add_transceiver_from_kind(
                    kind,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        send_encodings: vec![],
                    }),
                )
                .await
                .unwrap();
            }
            let reliable = pc.create_data_channel("reliable", None).await.unwrap();
            for label in ["fast", "bulk"] {
                pc.create_data_channel(label, None).await.unwrap();
            }
            let counted = Arc::clone(&pongs);
            reliable.on_message(Box::new(move |msg| {
                if msg.data.first() == Some(&protocol::op::PONG) {
                    counted.fetch_add(1, Ordering::Relaxed);
                }
                Box::pin(async {})
            }));
            let counted = Arc::clone(&rtp);
            pc.on_track(Box::new(move |track, _, _| {
                let counted = Arc::clone(&counted);
                Box::pin(async move {
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 1600];
                        while track.read(&mut buf).await.is_ok() {
                            counted.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                })
            }));
            let offer = pc.create_offer(None).await.unwrap();
            let mut gathered = pc.gathering_complete_promise().await;
            pc.set_local_description(offer).await.unwrap();
            let _ = gathered.recv().await;
            let sdp = pc.local_description().await.unwrap().sdp;
            (pc, reliable, sdp)
        });
        Self {
            rt: Some(rt),
            pc,
            reliable,
            rtp,
            pongs,
            offer,
        }
    }

    fn rt(&self) -> &Runtime {
        self.rt.as_ref().expect("client runtime")
    }

    fn apply(&self, answer: &str) {
        let answer = RTCSessionDescription::answer(answer.to_string()).expect("answer");
        self.rt()
            .block_on(self.pc.set_remote_description(answer))
            .expect("apply answer");
    }

    /// Media flowing one way and a ping answered the other: every per-session task is up.
    fn wait_for_media(&self) {
        wait_for("RTP", Duration::from_secs(20), || {
            self.rtp.load(Ordering::Relaxed) > 5
        });
        let mut ping = vec![protocol::op::PING];
        ping.extend_from_slice(&1u64.to_le_bytes());
        let ping = Bytes::from(ping);
        wait_for("a pong", Duration::from_secs(10), || {
            let _ = self.rt().block_on(self.reliable.send(&ping));
            self.pongs.load(Ordering::Relaxed) > 0
        });
    }

    /// The page going away properly.
    fn leave(mut self) {
        let _ = self.rt().block_on(self.pc.close());
        self.rt.take().unwrap().shutdown_background();
    }

    /// The process dying: nothing is sent, nothing is answered again.
    fn kill(mut self) {
        self.rt.take().unwrap().shutdown_background();
    }
}

fn turn() -> MutexGuard<'static, ()> {
    // One client's descriptors must be gone before the next test counts its own baseline.
    TURN.lock_recover()
}

fn join_id(n: usize) -> String {
    format!("{n:032x}")
}

fn cycles() -> usize {
    std::env::var("SE_LEAK_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15)
}

#[test]
fn a_session_that_leaves_gives_its_sockets_and_tasks_back() {
    let _turn = turn();
    let mut host = Host::new();
    let rss = rss_mb();
    let n = cycles();
    for i in 0..n {
        let client = Client::new();
        let live = host.join(&client, &join_id(i));
        client.leave();
        host.end(live);
        if (i + 1) % 10 == 0 {
            eprintln!(
                "[session-leak] {}",
                host.report(&format!("after {} cycles", i + 1), rss)
            );
        }
    }
    host.assert_everything_came_back("clean leave", rss);
}

#[test]
fn a_client_that_vanishes_gives_its_sockets_and_tasks_back() {
    let _turn = turn();
    let mut host = Host::new();
    let rss = rss_mb();
    // Side by side, because each takes ICE's own 5 s and the state handler's 8 s to be noticed.
    let lives: Vec<Live> = (0..5)
        .map(|i| {
            let client = Client::new();
            let live = host.join(&client, &join_id(0x1000 + i));
            client.kill();
            live
        })
        .collect();
    for live in lives {
        host.end_when_reported_closed(live, Duration::from_secs(60));
    }
    host.assert_everything_came_back("abrupt kill", rss);
}

#[test]
fn a_renegotiation_gives_back_the_transport_it_replaces() {
    let _turn = turn();
    let mut host = Host::new();
    let rss = rss_mb();
    let id = join_id(0x2000);
    let mut client = Client::new();
    let mut live = host.join(&client, &id);
    for _ in 0..5 {
        // The page throws its connection away and offers again under the join it holds. Dropped
        // in silence here, so that it is the replacement connecting that closes the old one.
        client.kill();
        client = Client::new();
        let next = host.join(&client, &id);
        host.end_when_reported_closed(live, Duration::from_secs(20));
        live = next;
    }
    client.leave();
    host.end(live);
    host.assert_everything_came_back("renegotiation", rss);
}

#[test]
fn a_join_that_never_finishes_gives_its_sockets_and_tasks_back() {
    let _turn = turn();
    let mut host = Host::new();
    let rss = rss_mb();
    let client = Client::new();
    host.client_ports.extend(candidate_ports(&client.offer));

    for i in 0..5 {
        // The display half of the join failed, or a newer offer superseded this one.
        let pending = host
            .prepare(&client.offer, &join_id(0x3000 + i))
            .expect("prepare");
        host.pcs.push(pending.peer_connection());
        host.rt.block_on(pending.abandon());

        // The request was dropped between the answer and the activation.
        let pending = host
            .prepare(&client.offer, &join_id(0x3100 + i))
            .expect("prepare");
        host.pcs.push(pending.peer_connection());
        host.rt.block_on(async move { drop(pending) });

        // Activated, and nobody ever connected.
        let pending = host
            .prepare(&client.offer, &join_id(0x3200 + i))
            .expect("prepare");
        let live = host.activate(pending);
        host.end(live);

        // Activated, and the handle itself was dropped.
        let pending = host
            .prepare(&client.offer, &join_id(0x3300 + i))
            .expect("prepare");
        let live = host.activate(pending);
        host.rt.block_on(async move { drop(live.handle) });

        // An offer webrtc-rs rejects after the connection exists.
        assert!(host
            .prepare("v=0\r\nnot an offer\r\n", &join_id(0x3400 + i))
            .is_err());
    }

    // The client aborted `POST /whep`: the server drops the request's future wherever it was.
    // Each round lets the negotiation get a little further before dropping it.
    for i in 0..120u64 {
        let offer = client.offer.clone();
        let audio = host.audio();
        let id = join_id(0x4000 + i as usize);
        let _ = host.rt.block_on(async {
            tokio::time::timeout(
                Duration::from_micros(i * 100),
                Host::negotiate(offer, &id, audio, vec![]),
            )
            .await
            .map(|r| r.map(drop))
        });
    }
    client.leave();
    host.assert_everything_came_back("unfinished joins", rss);
}

/// A STUN server that takes its time: one binding response, `delay` after each request.
struct SlowStun {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    answered: Arc<AtomicU64>,
}

impl SlowStun {
    fn start(delay: Duration) -> Self {
        let sock = std::net::UdpSocket::bind((LOOPBACK, 0)).expect("stun socket");
        sock.set_read_timeout(Some(Duration::from_millis(50)))
            .expect("stun timeout");
        let port = sock.local_addr().expect("stun address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let answered = Arc::new(AtomicU64::new(0));
        let (stopped, count) = (Arc::clone(&stop), Arc::clone(&answered));
        let thread = std::thread::spawn(move || {
            let mut due: Vec<(Instant, std::net::SocketAddr, [u8; 12])> = Vec::new();
            let mut buf = [0u8; 1500];
            while !stopped.load(Ordering::Relaxed) {
                if let Ok((n, from)) = sock.recv_from(&mut buf) {
                    if n >= 20 && buf[..2] == [0, 1] {
                        let id = buf[8..20].try_into().expect("transaction id");
                        due.push((Instant::now() + delay, from, id));
                    }
                }
                due.retain(|(at, from, id)| {
                    if Instant::now() < *at {
                        return true;
                    }
                    // XOR-MAPPED-ADDRESS 203.0.113.7:4096, RFC 8489.
                    let mut r = vec![0x01, 0x01, 0, 12, 0x21, 0x12, 0xa4, 0x42];
                    r.extend_from_slice(id);
                    r.extend_from_slice(&[0, 0x20, 0, 8, 0, 1, 0x10 ^ 0x21, 0x12]);
                    r.extend_from_slice(&[203 ^ 0x21, 0x12, 113 ^ 0xa4, 7 ^ 0x42]);
                    let _ = sock.send_to(&r, *from);
                    count.fetch_add(1, Ordering::Relaxed);
                    false
                });
            }
        });
        Self {
            port,
            stop,
            thread: Some(thread),
            answered,
        }
    }
}

impl Drop for SlowStun {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn a_close_that_overtakes_gathering_leaves_no_candidate_behind() {
    let _turn = turn();
    let stun = SlowStun::start(Duration::from_millis(600));
    let mut host = Host::new();
    let rss = rss_mb();
    let client = Client::new();
    host.client_ports.extend(candidate_ports(&client.offer));
    for i in 0..5 {
        // The answer goes out after 200 ms without the reflexive candidate, and the join fails
        // while the server is still thinking about it.
        let pending = host
            .prepare_with(
                &client.offer,
                &join_id(0x5000 + i),
                vec![RTCIceServer {
                    urls: vec![format!("stun:127.0.0.1:{}", stun.port)],
                    ..Default::default()
                }],
            )
            .expect("prepare");
        host.pcs.push(pending.peer_connection());
        host.rt.block_on(pending.abandon());
    }
    wait_for("the STUN server's answers", Duration::from_secs(10), || {
        stun.answered.load(Ordering::Relaxed) >= 5
    });
    client.leave();
    host.assert_everything_came_back("closed while gathering", rss);
}

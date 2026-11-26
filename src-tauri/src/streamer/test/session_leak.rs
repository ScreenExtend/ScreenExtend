//! A session that ends must give everything back: its ICE socket, its tasks, its peer connection.
//!
//! In process, on loopback: the real `prepare_whep` / `activate_session` / `SessionHandle` on one
//! runtime, a webrtc-rs client on its own runtime per join (shut down to make a client vanish),
//! and a thread standing in for the encoder. Counted from outside the host's bookkeeping: the
//! kernel's UDP sockets, tokio's alive tasks on the host runtime, and `Arc` strong counts.

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
#[cfg(target_os = "macos")]
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
        // `sin_port` and `sin6_port` are at the same offset
        if matches!(
            addr.sin6_family as libc::c_int,
            libc::AF_INET | libc::AF_INET6
        ) {
            ports.push(u16::from_be(addr.sin6_port));
        }
    }
    ports
}

/// Every UDP port this process has open, from the kernel's table of UDP sockets by owner.
#[cfg(windows)]
fn open_udp_ports() -> Vec<u16> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedUdpTable, MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID, UDP_TABLE_OWNER_PID,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let me = std::process::id();
    let tables = [
        (
            AF_INET.0 as u32,
            std::mem::size_of::<MIB_UDPROW_OWNER_PID>(),
            std::mem::offset_of!(MIB_UDPROW_OWNER_PID, dwLocalPort),
            std::mem::offset_of!(MIB_UDPROW_OWNER_PID, dwOwningPid),
        ),
        (
            AF_INET6.0 as u32,
            std::mem::size_of::<MIB_UDP6ROW_OWNER_PID>(),
            std::mem::offset_of!(MIB_UDP6ROW_OWNER_PID, dwLocalPort),
            std::mem::offset_of!(MIB_UDP6ROW_OWNER_PID, dwOwningPid),
        ),
    ];
    let mut ports = Vec::new();
    for (family, row, port_at, pid_at) in tables {
        let mut size = 0u32;
        let mut buf: Vec<u32> = Vec::new();
        // the table can grow between asking its size and reading it
        for _ in 0..4 {
            let ptr = (!buf.is_empty()).then(|| buf.as_mut_ptr().cast::<core::ffi::c_void>());
            // SAFETY: `ptr` is either absent or `size` bytes of `buf`, which outlives the call.
            let rc = unsafe {
                GetExtendedUdpTable(ptr, &mut size, false, family, UDP_TABLE_OWNER_PID, 0)
            };
            if rc == 0 && !buf.is_empty() {
                let words = |at: usize| buf.get(at / 4).copied().unwrap_or(0);
                // a u32 row count, then the rows; every field read here is a u32
                for i in 0..words(0) as usize {
                    let base = 4 + i * row;
                    if words(base + pid_at) == me {
                        ports.push(u16::from_be(words(base + port_at) as u16));
                    }
                }
                break;
            }
            if rc != 0 && rc != ERROR_INSUFFICIENT_BUFFER {
                break;
            }
            buf = vec![0u32; (size as usize).div_ceil(4) + 64];
            size = (buf.len() * 4) as u32;
        }
    }
    ports
}

/// Handles this process holds. The whole suite shares the process and its other tests open
/// hundreds of handles meanwhile, so the count is compared only with `SE_LEAK_HANDLES=1`, meant
/// for a run of these tests alone (`--test-threads 1`); otherwise it is reported.
#[cfg(windows)]
fn handle_count() -> u32 {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut n = 0u32;
    // SAFETY: the pseudo-handle is always valid and `n` outlives the call.
    let _ = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) };
    n
}

/// How many more handles than at the start still count as given back under `SE_LEAK_HANDLES=1`
/// (the runtime's worker threads, a lazily created event). A leaked session also holds sockets,
/// which the port check catches exactly in any run.
#[cfg(windows)]
const HANDLE_SLACK: u32 = 64;

fn candidate_ports(sdp: &str) -> Vec<u16> {
    sdp.lines()
        .filter_map(|l| l.trim().strip_prefix("a=candidate:"))
        .filter_map(|c| c.split_whitespace().nth(5)?.parse().ok())
        .collect()
}

#[cfg(windows)]
fn rss_mb() -> f64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: `c` is a correctly sized counters struct that outlives the call.
    match unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } {
        Ok(()) => c.WorkingSetSize as f64 / (1024.0 * 1024.0),
        Err(_) => 0.0,
    }
}

#[cfg(target_os = "macos")]
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
    /// Set by a request, taken by the feed: the next access unit it sends is an IDR.
    idr_due: Arc<AtomicBool>,
    _frames: broadcast::Sender<EncodedFrame>,
}

impl EncoderControl for Controls {
    fn request_idr(&self) {
        self.idrs.fetch_add(1, Ordering::Relaxed);
        self.idr_due.store(true, Ordering::Relaxed);
    }
    fn request_repair(&self) -> bool {
        false
    }
    fn set_target_bitrate(&self, _bps: u32) {}
}

/// One access unit a packetizer accepts, every 8 ms.
struct Feed {
    frames: broadcast::Sender<EncodedFrame>,
    idr_due: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Feed {
    /// Every access unit an IDR.
    fn start() -> Self {
        Self::with(4000, None)
    }

    /// Every access unit an IDR of `bytes`.
    fn idrs(bytes: usize) -> Self {
        Self::with(bytes, None)
    }

    /// One IDR, then nothing but P frames of `delta_bytes` each.
    fn deltas(delta_bytes: usize) -> Self {
        Self::with(4000, Some(delta_bytes))
    }

    fn with(idr_bytes: usize, delta_bytes: Option<usize>) -> Self {
        let (frames, _) = broadcast::channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let tx = frames.clone();
        let stopped = Arc::clone(&stop);
        let idr_due = Arc::new(AtomicBool::new(false));
        let due = Arc::clone(&idr_due);
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        au.resize(au.len() + idr_bytes, 0x5a);
        let au = Bytes::from(au);
        let delta = delta_bytes.map(|n| {
            let mut d = vec![0, 0, 0, 1, 0x41];
            d.resize(d.len() + n, 0x5a);
            Bytes::from(d)
        });
        let thread = std::thread::spawn(move || {
            let mut sent = 0u64;
            while !stopped.load(Ordering::Relaxed) {
                let data = match &delta {
                    Some(d) if sent > 0 && !due.swap(false, Ordering::Relaxed) => d.clone(),
                    _ => au.clone(),
                };
                // only once a viewer holds a receiver does the opening IDR count as sent
                if tx
                    .send(EncodedFrame {
                        data,
                        capture: Instant::now(),
                        timing: FrameTiming::entering_capture(),
                    })
                    .is_ok()
                {
                    sent += 1;
                }
                std::thread::sleep(Duration::from_millis(8));
            }
        });
        Self {
            frames,
            idr_due,
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
    #[cfg(windows)]
    handles_before: u32,
}

struct Live {
    controls: Arc<Controls>,
    telemetry: crate::streamer::telemetry::SharedTelemetry,
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
            #[cfg(windows)]
            handles_before: handle_count(),
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
        Self::activate_on_feed(pending, audio, Feed::start(), 8_000_000, None).await
    }

    async fn activate_on_feed(
        pending: PendingWhep,
        audio: AudioParams,
        feed: Feed,
        max_bitrate_bps: u32,
        packet_limit: Option<u32>,
    ) -> Live {
        let controls = Arc::new(Controls {
            idrs: AtomicU64::new(0),
            idr_due: Arc::clone(&feed.idr_due),
            _frames: feed.frames.clone(),
        });
        let mut pipeline = SessionPipeline::without_encoder_at(
            Arc::clone(&controls) as Arc<dyn EncoderControl>,
            max_bitrate_bps,
        );
        if let Some(limit) = packet_limit {
            pipeline = pipeline.with_packet_distance_limit(limit);
        }
        let telemetry = pipeline.telemetry();
        let (closed_tx, closed) = oneshot::channel();
        let (answer, handle) = activate_session(
            pending,
            feed.frames.subscribe(),
            pipeline,
            Some(closed_tx),
            InputParams {
                device: None,
                #[cfg(windows)]
                device_path: None,
                client_os: String::new(),
                live: input::LiveInput::shared(false, Default::default()),
                control_rx: None,
            },
            Some(audio),
        )
        .await
        .expect("activate");
        Live {
            controls,
            telemetry,
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

    /// The whole process's UDP sockets. The suite runs in parallel in one process on Windows and
    /// other tests open sockets meanwhile, so there it is reported, not asserted: the exact
    /// check is `leaked_ports`, on the ports these sessions advertised.
    fn process_sockets_back(&self) -> bool {
        cfg!(windows) || self.host_udp_sockets() <= self.sockets_before + 1
    }

    #[cfg(windows)]
    fn handles_back(&self) -> bool {
        std::env::var("SE_LEAK_HANDLES").as_deref() != Ok("1")
            || handle_count() <= self.handles_before + HANDLE_SLACK
    }

    #[cfg(not(windows))]
    fn handles_back(&self) -> bool {
        true
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
        // teardown is asynchronous in places (the bitrate driver within 120 ms, an audio
        // forwarder within 250 ms), so this waits for "comes back", not "is already back"
        let deadline = Instant::now() + Duration::from_secs(20);
        let clean = || {
            self.leaked_ports().is_empty()
                && self.alive() == 0
                && self.rt.metrics().num_alive_tasks() <= self.tasks_before
                && self.process_sockets_back()
                && webrtc_session::live_sessions() == self.live_before
                && webrtc_session::trickle_entries() <= self.trickle_before
                && Arc::strong_count(&self.hub) == 1
                && self.handles_back()
        };
        while !clean() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let report = self.report(label, rss_before);
        #[cfg(windows)]
        let report = format!(
            "{report}, handles {} (before {})",
            handle_count(),
            self.handles_before
        );
        eprintln!("[session-leak] {report}");
        assert!(clean(), "{report}");
    }
}

struct Client {
    rt: Option<Runtime>,
    pc: Arc<RTCPeerConnection>,
    reliable: Arc<RTCDataChannel>,
    rtp: Arc<AtomicU64>,
    /// The most video sequence numbers this receiver saw follow one IDR before the next.
    longest_gop: Arc<AtomicU64>,
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
        let longest_gop = Arc::new(AtomicU64::new(0));
        let (pc, reliable, offer) = rt.block_on(async {
            let mut m = MediaEngine::default();
            m.register_default_codecs().unwrap();
            let registry = register_default_interceptors(Registry::new(), &mut m).unwrap();
            let mut se = SettingEngine::default();
            se.set_include_loopback_candidate(true);
            // each agent otherwise holds an mDNS socket as well, on the one port they all share
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
            let gop = Arc::clone(&longest_gop);
            pc.on_track(Box::new(move |track, _, _| {
                let counted = Arc::clone(&counted);
                let gop = Arc::clone(&gop);
                Box::pin(async move {
                    tokio::spawn(async move {
                        let video = track.kind() == RTPCodecType::Video;
                        let mut since_idr: Option<u64> = None;
                        let mut last_seq: Option<u16> = None;
                        let mut buf = vec![0u8; 1600];
                        while let Ok((pkt, _)) = track.read(&mut buf).await {
                            counted.fetch_add(1, Ordering::Relaxed);
                            if !video {
                                continue;
                            }
                            let seq = pkt.header.sequence_number;
                            let step = last_seq.map_or(1, |l| u64::from(seq.wrapping_sub(l)));
                            last_seq = Some(seq);
                            if crate::streamer::keyframe::payload_carries_idr(&pkt.payload) {
                                since_idr = Some(0);
                            } else if let Some(d) = since_idr.as_mut() {
                                *d += step;
                                gop.fetch_max(*d, Ordering::Relaxed);
                            }
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
            longest_gop,
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
    // one client's descriptors must be gone before the next test counts its own baseline
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
    // side by side, because each takes ICE's own 5 s and the state handler's 8 s to be noticed
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
        // the page drops its connection and offers again under the join it holds: killed in
        // silence, so that the replacement connecting is what closes the old one
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
        // the display half of the join failed, or a newer offer superseded this one
        let pending = host
            .prepare(&client.offer, &join_id(0x3000 + i))
            .expect("prepare");
        host.pcs.push(pending.peer_connection());
        host.rt.block_on(pending.abandon());

        // the request was dropped between the answer and the activation
        let pending = host
            .prepare(&client.offer, &join_id(0x3100 + i))
            .expect("prepare");
        host.pcs.push(pending.peer_connection());
        host.rt.block_on(async move { drop(pending) });

        // activated, and nobody ever connected
        let pending = host
            .prepare(&client.offer, &join_id(0x3200 + i))
            .expect("prepare");
        let live = host.activate(pending);
        host.end(live);

        // activated, and the handle itself was dropped
        let pending = host
            .prepare(&client.offer, &join_id(0x3300 + i))
            .expect("prepare");
        let live = host.activate(pending);
        host.rt.block_on(async move { drop(live.handle) });

        // an offer webrtc-rs rejects after the connection exists
        assert!(host
            .prepare("v=0\r\nnot an offer\r\n", &join_id(0x3400 + i))
            .is_err());
    }

    // an aborted `POST /whep`: the server drops the request's future wherever it was, and each
    // round lets the negotiation get 100 us further first
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

/// Drops `Host::negotiate` at every point a fine timeout sweep reaches and reports any drop
/// after which host tasks are still alive 1.5 s later. `SE_LEAK_BURN` adds a CPU-burning thread
/// per core, to imitate a loaded machine.
#[test]
#[ignore = "stress sweep, minutes long; run by name to look for a drop point that leaves tasks"]
fn a_dropped_negotiation_at_any_point_gives_its_tasks_back() {
    let _turn = turn();
    let host = Host::new();
    let client = Client::new();
    let burn = std::env::var("SE_LEAK_BURN").is_ok();
    let stop = Arc::new(AtomicBool::new(false));
    let burners: Vec<_> = (0..if burn {
        std::thread::available_parallelism().map_or(4, |n| n.get())
    } else {
        0
    })
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut x = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                }
                x
            })
        })
        .collect();
    let step: u64 = std::env::var("SE_LEAK_STEP_US")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let max: u64 = std::env::var("SE_LEAK_MAX_US")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);
    let mut stuck = Vec::new();
    let mut n = 0usize;
    let mut t = 0u64;
    while t <= max {
        // per drop: what this drop must give back is measured from just before it
        let base = host.rt.metrics().num_alive_tasks();
        let offer = client.offer.clone();
        let audio = host.audio();
        let id = join_id(0x5000 + n);
        n += 1;
        let finished = host.rt.block_on(async {
            tokio::time::timeout(
                Duration::from_micros(t),
                Host::negotiate(offer, &id, audio, vec![]),
            )
            .await
            .map(|r| r.map(drop))
            .is_ok()
        });
        let settle = Instant::now();
        while host.rt.metrics().num_alive_tasks() > base
            && settle.elapsed() < Duration::from_millis(1500)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let left = host.rt.metrics().num_alive_tasks().saturating_sub(base);
        if left > 0 {
            let more = Instant::now();
            while host.rt.metrics().num_alive_tasks() > base
                && more.elapsed() < Duration::from_secs(90)
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            let after = host.rt.metrics().num_alive_tasks().saturating_sub(base);
            eprintln!(
                "[leak-sweep] timeout {t} us (finished: {finished}): {left} task(s) alive 1.5 s \
                 after the drop; {after} still alive {} ms later",
                more.elapsed().as_millis()
            );
            stuck.push((t, left, after));
        }
        t += step;
    }
    stop.store(true, Ordering::Relaxed);
    for b in burners {
        let _ = b.join();
    }
    client.leave();
    eprintln!(
        "[leak-sweep] {n} drops, {} left tasks behind: {stuck:?}",
        stuck.len()
    );
    assert!(
        stuck.iter().all(|&(_, _, after)| after == 0),
        "a dropped negotiation left tasks behind for good: {stuck:?}"
    );
}

/// The same unfinished joins on the tuned media socket (off by default in production): its UDP
/// mux runs a read worker from before the peer connection exists, which a drop or an error
/// before the transport takes it over must not leave running.
#[test]
fn a_join_on_the_media_socket_that_never_finishes_gives_everything_back() {
    let _turn = turn();
    let host = Host::new();
    let rss = rss_mb();
    let client = Client::new();
    for i in 0..60u64 {
        let offer = client.offer.clone();
        let audio = host.audio();
        let id = join_id(0x6000 + i as usize);
        let _ = host
            .rt
            .block_on(webrtc_session::FORCE_MEDIA_SOCKET.scope(true, async {
                tokio::time::timeout(
                    Duration::from_micros(i * 250),
                    Host::negotiate(offer, &id, audio, vec![]),
                )
                .await
                .map(|r| r.map(drop))
            }));
    }
    // and one that fails after the mux exists: an offer webrtc-rs rejects
    let audio = host.audio();
    let id = join_id(0x6fff);
    let bad = host.rt.block_on(webrtc_session::FORCE_MEDIA_SOCKET.scope(
        true,
        Host::negotiate("v=0\r\nnot an offer\r\n".to_string(), &id, audio, vec![]),
    ));
    assert!(bad.is_err());
    client.leave();
    host.assert_everything_came_back("unfinished joins on the media socket", rss);
}

/// The media socket let go of before a transport owns it: its read worker and its socket come
/// back. In the negotiation that window holds no await that yields in practice, so the sweep
/// above does not reach it; an error from `build_api` or `new_peer_connection` would.
#[test]
fn a_media_socket_no_transport_took_is_closed() {
    let _turn = turn();
    let host = Host::new();
    let rss = rss_mb();
    for _ in 0..20 {
        let made = host
            .rt
            .block_on(async { webrtc_session::drop_unowned_media_socket(LOOPBACK) });
        assert!(made, "a tuned socket on loopback");
    }
    host.assert_everything_came_back("media sockets no transport took", rss);
}

/// The real writer and an encoder that sends an IDR only when asked, to a viewer that has sent no
/// STATS and so might read frames through the transport with the packet limit: the receiver never
/// sees `limit` sequence numbers follow one IDR. A stream of IDRs is never asked for one.
///
/// Paced by what the receiver has been sent, not by the clock, which a loaded machine stretches.
/// A viewer that lags is sent IDRs of its own, which can keep every run short of the bound; a run
/// like that verified nothing about the bound and says so.
fn deltas_stay_under(limit: Option<u32>, received_enough: u64, what: &str) {
    let _turn = turn();
    let mut host = Host::new();
    let limit_value = limit.unwrap_or(crate::streamer::keyframe::PACKET_DISTANCE_LIMIT);
    for (feed, wants) in [(Feed::idrs(16_000), false), (Feed::deltas(16_000), true)] {
        let client = Client::new();
        let pending = host
            .prepare(&client.offer, &join_id(0x7000 + usize::from(wants)))
            .expect("prepare");
        let live = host.rt.block_on(Host::activate_on_feed(
            pending,
            host.audio(),
            feed,
            200_000_000,
            limit,
        ));
        host.pcs.push(live.handle.peer_connection());
        client.apply(&live.answer);
        client.wait_for_media();
        let bound_driven = || {
            live.telemetry
                .degradation
                .keyframes_for_packet_distance
                .load(Ordering::Relaxed)
        };
        let start = Instant::now();
        while client.rtp.load(Ordering::Relaxed) < received_enough
            && start.elapsed() < Duration::from_secs(150)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let received = client.rtp.load(Ordering::Relaxed);
        let longest = client.longest_gop.load(Ordering::Relaxed);
        let lagged = live
            .telemetry
            .degradation
            .frames_dropped_broadcast
            .load(Ordering::Relaxed);
        eprintln!(
            "[packet-distance] {what}: idrs fed {} received {received} longest run {longest} bound-driven {} viewer lag {lagged}",
            !wants,
            bound_driven()
        );
        assert!(
            received >= received_enough,
            "only {received} packets arrived"
        );
        if wants {
            assert!(longest > 0, "no run of deltas reached the receiver");
            assert!(
                longest < u64::from(limit_value),
                "{longest} sequence numbers followed one IDR"
            );
            if bound_driven() == 0 {
                crate::test_support::skip_because(
                    what,
                    "the viewer lagged, and the IDRs that brought kept every run under the bound",
                );
            }
        } else {
            assert_eq!(bound_driven(), 0, "an IDR asked for on a stream of IDRs");
        }
        client.leave();
        host.end(live);
    }
}

/// The writer's packet distance against a limit of 2,048, which the same code reaches in a few
/// seconds.
#[test]
fn a_long_run_of_deltas_is_sent_an_idr_before_a_lowered_packet_limit() {
    deltas_stay_under(Some(2_048), 5_000, "packet distance, lowered limit");
}

/// The same against the real 2^15.
#[test]
#[ignore = "about 45 s of streaming per run; the lowered-limit version runs the same code"]
fn a_long_run_of_deltas_is_sent_an_idr_before_the_packet_limit() {
    deltas_stay_under(None, 40_000, "packet distance, 2^15");
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
        // an answer to an ICE agent that has already closed draws a port-unreachable, which on
        // Windows fails this socket's next receive and loses the request queued behind it
        #[cfg(windows)]
        crate::streamer::sockets::tune(&sock);
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
                    // XOR-MAPPED-ADDRESS 203.0.113.7:4096, RFC 8489
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
        // the answer goes out after 200 ms without the reflexive candidate, and the join fails
        // before the STUN server (600 ms) has replied
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
    // Windows: under a loaded parallel suite some joins are abandoned before their request
    // leaves, so one late answer is enough for the test to mean something
    let answers = if cfg!(windows) { 1 } else { 5 };
    wait_for("the STUN server's answers", Duration::from_secs(10), || {
        stun.answered.load(Ordering::Relaxed) >= answers
    });
    client.leave();
    host.assert_everything_came_back("closed while gathering", rss);
}

//! The mirroring data connection — accepted, framed, and thrown away.
//!
//! The sender opens one TCP connection to the `dataPort` we hand back in the
//! type-110 SETUP response and pushes H.264 down it, framed as a 128-byte header
//! plus a payload whose length is a little-endian `u32` at offset 0. The channel
//! is strictly one-way: there is no ACK, no window update, nothing the receiver
//! ever writes back.
//!
//! We still have to *read* it. Not reading leaves the socket's receive buffer
//! full, which back-pressures the sender's encoder and eventually produces a
//! reset rather than a stable session. And we have to read it *framed* — a blind
//! drain would have no way to resynchronise or to notice a desync. That framing
//! loop is the only place in this module that goes past "handshake only". We
//! never decrypt a payload, never look at a NAL unit, never touch VideoToolbox.
//!
//! The one header byte we do read is the packet type, because type `0x01`
//! (unencrypted SPS/PPS) is re-sent whenever the sender changes video geometry —
//! a free signal that macOS moved the display out from under us.
//!
//! The listener outlives a connection. The receiver is deliberately kept warm
//! across sessions ([`super::AirPlayVirtualDisplay::remove_display`]), and the
//! port we advertise in SETUP is cached for its whole life, so a sink that
//! served one connection and closed would leave every session after the first
//! pointing at a dead port.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

use super::Cancel;

/// Header size, fixed by the protocol.
const HEADER_LEN: usize = 128;
/// A payload larger than this means we lost framing. The largest legitimate one
/// is a streaming report with a ~25 KiB trailer, so 32 MiB is enormous slack.
const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

/// Packet type at `header[4]`.
const TYPE_VIDEO: u8 = 0x00;
const TYPE_CODEC_DATA: u8 = 0x01;
const TYPE_HEARTBEAT: u8 = 0x02;
const TYPE_STREAM_REPORT: u8 = 0x05;

/// How often the drain loop reports its throughput.
///
/// This is the one number that says whether macOS honours the `maxFPS` we
/// advertise ([`super::info`]). At the advertised 1 fps a live session should
/// show single-digit packets per interval; a full-rate desktop encode the host
/// is doing purely for us to discard would show hundreds.
const RATE_REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default, Debug)]
pub struct MirrorStats {
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    /// Set when the sender re-sends codec data, which it does on a geometry change.
    pub geometry_changed: AtomicBool,
    /// Set once the peer connects, so the session can tell "never connected"
    /// from "connected then dropped".
    pub connected: AtomicBool,
}

impl MirrorStats {
    /// Clears the per-connection latches, keeping the cumulative counters.
    ///
    /// `connected` and `geometry_changed` answer questions about *this* session
    /// ("did the sender ever open the data connection?"), so a latch left over
    /// from the previous one is worse than no answer at all.
    pub fn begin_connection(&self) {
        self.connected.store(true, Ordering::Relaxed);
        self.geometry_changed.store(false, Ordering::Relaxed);
    }

    /// Clears them completely, for a session that has not started yet.
    pub fn begin_session(&self) {
        self.connected.store(false, Ordering::Relaxed);
        self.geometry_changed.store(false, Ordering::Relaxed);
    }
}

/// Listener for the mirroring stream. Bound before SETUP so we can advertise a
/// real port number.
pub struct MirrorSink {
    listener: TcpListener,
    port: u16,
}

impl MirrorSink {
    pub async fn bind() -> Result<Self, String> {
        let listener = TcpListener::bind(("0.0.0.0", 0))
            .await
            .map_err(|e| format!("could not bind the AirPlay mirror data port: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("could not read the mirror data port: {e}"))?
            .port();
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Serves connections until `stop` is cancelled.
    ///
    /// One at a time: AirPlay runs a single display session, so a second
    /// concurrent mirror connection is not a case that exists. What does exist
    /// is a *sequence* of them, one per session, all pointed at this same
    /// advertised port.
    pub async fn run(self, stats: Arc<MirrorStats>, stop: Cancel) {
        loop {
            let (stream, peer) = tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        teprintln!("[airplay] mirror accept failed: {e}");
                        return;
                    }
                },
                _ = stop.cancelled() => return,
            };
            tprintln!("[airplay] mirror stream connected from {peer}");
            stats.begin_connection();

            configure_keepalive(&stream);

            tokio::select! {
                r = drain(stream, &stats) => {
                    match r {
                        Ok(()) => tprintln!("[airplay] mirror stream closed by the sender"),
                        Err(e) => teprintln!("[airplay] mirror stream ended: {e}"),
                    }
                }
                _ = stop.cancelled() => {
                    tprintln!("[airplay] mirror stream torn down locally");
                    return;
                }
            }
        }
    }
}

async fn drain(mut stream: TcpStream, stats: &MirrorStats) -> Result<(), String> {
    let mut header = [0u8; HEADER_LEN];
    let mut scratch = vec![0u8; 64 * 1024];
    let mut probe = HeaderProbe::new();
    let mut rate = RateReport::new();

    loop {
        match stream.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                probe.finish();
                return Ok(());
            }
            Err(e) => {
                probe.finish();
                return Err(format!("header read failed: {e}"));
            }
        }

        let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if payload_len > MAX_PAYLOAD {
            probe.finish();
            return Err(format!(
                "lost framing on the mirror stream (payload claimed {payload_len} bytes)"
            ));
        }

        let kind = header[4];
        match kind {
            TYPE_CODEC_DATA => {
                // SPS/PPS. Re-sent when the sender's video geometry changes.
                stats.geometry_changed.store(true, Ordering::Relaxed);
            }
            TYPE_VIDEO | TYPE_HEARTBEAT | TYPE_STREAM_REPORT => {}
            other => {
                tprintln!("[airplay] mirror stream: ignoring unknown packet type 0x{other:02x}");
            }
        }
        probe.observe(&header);

        let mut left = payload_len;
        let mut first = true;
        while left > 0 {
            let take = left.min(scratch.len());
            match stream.read_exact(&mut scratch[..take]).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    probe.finish();
                    return Ok(());
                }
                Err(e) => {
                    probe.finish();
                    return Err(format!("payload read failed: {e}"));
                }
            }
            if first {
                probe.observe_payload(kind, &scratch[..take]);
                first = false;
            }
            left -= take;
        }

        stats.packets.fetch_add(1, Ordering::Relaxed);
        stats
            .bytes
            .fetch_add((HEADER_LEN + payload_len) as u64, Ordering::Relaxed);
        rate.observe(HEADER_LEN + payload_len);
    }
}

/// Rolling throughput of the discarded stream.
struct RateReport {
    since: Instant,
    packets: u64,
    bytes: u64,
}

impl RateReport {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            packets: 0,
            bytes: 0,
        }
    }

    fn observe(&mut self, bytes: usize) {
        self.packets += 1;
        self.bytes += bytes as u64;
        let elapsed = self.since.elapsed();
        if elapsed < RATE_REPORT_INTERVAL {
            return;
        }
        let secs = elapsed.as_secs_f64().max(0.001);
        tprintln!(
            "[airplay] mirror stream discarding {:.1} packets/s, {:.0} KiB/s",
            self.packets as f64 / secs,
            self.bytes as f64 / secs / 1024.0,
        );
        self.since = Instant::now();
        self.packets = 0;
        self.bytes = 0;
    }
}

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch.
const NTP_UNIX_EPOCH_OFFSET_SECS: u64 = 2_208_988_800;

/// The current time in NTP short-format fixed point, from the same wall clock
/// the sender timestamps its frames off.
fn ntp_now() -> u64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() + NTP_UNIX_EPOCH_OFFSET_SECS;
    let frac = ((d.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

fn ntp_delta_ns(now: u64, then: u64) -> i64 {
    let diff = now as i128 - then as i128;
    ((diff * 1_000_000_000) >> 32) as i64
}

/// A plausible capture-to-delivery latency: never more than a second in the
/// future, never more than ten seconds in the past.
fn plausible(ns: i64) -> bool {
    (-1_000_000_000..10_000_000_000).contains(&ns)
}

/// Reads the plaintext part of the mirror header.
///
/// Every published AirPlay receiver frames a mirror packet as a 128-byte header
/// whose first five bytes are the payload length and the packet type, with a
/// flag field at offset 6 and the sender's capture time — NTP fixed point — at
/// offset 8. This module reads five of those bytes and discards the rest, which
/// throws away the one thing on this connection that nothing else in the tree
/// can measure: the sender is *this same Mac*, so the difference between that
/// timestamp and the instant the bytes reach our socket is macOS's own
/// compositor-to-consumer latency — the floor under every capture path on this
/// OS, CGDisplayStream and ScreenCaptureKit alike.
///
/// It is off unless `SE_AIRPLAY_TIMING=1`, because the measurement is only
/// interesting with `SE_AIRPLAY_MAX_FPS` raised, and that makes the host encode
/// its whole desktop for us to throw away. Purely a read of bytes that already
/// arrive — no decryption, and none is possible: only the payload is FairPlay
/// ciphertext.
struct HeaderProbe {
    enabled: bool,
    /// Header hexdumps still owed, per packet type.
    header_dumps: [u8; 256],
    /// Payload hexdumps still owed, per packet type. Confirms whether a
    /// type-`0x00` payload is really ciphertext or whether a NAL start code is
    /// visible in the clear.
    payload_dumps: [u8; 256],
    deltas: Vec<i64>,
    big_endian: Option<bool>,
}

/// How many packets of each type get dumped when the probe is on.
const DUMPS_PER_TYPE: u8 = 3;
/// Report and reset once this many samples have accumulated.
const SAMPLE_BATCH: usize = 2048;

impl HeaderProbe {
    fn new() -> Self {
        let enabled = std::env::var("SE_AIRPLAY_TIMING").is_ok_and(|v| v != "0");
        if enabled {
            tprintln!(
                "[airplay] mirror header timing probe on (SE_AIRPLAY_TIMING); raise \
                 SE_AIRPLAY_MAX_FPS to get a useful sample count"
            );
        }
        Self {
            enabled,
            header_dumps: [DUMPS_PER_TYPE; 256],
            payload_dumps: [DUMPS_PER_TYPE; 256],
            deltas: Vec::new(),
            big_endian: None,
        }
    }

    fn observe(&mut self, header: &[u8; HEADER_LEN]) {
        if !self.enabled {
            return;
        }
        let now = ntp_now();
        let kind = header[4] as usize;

        if self.header_dumps[kind] > 0 {
            self.header_dumps[kind] -= 1;
            tprintln!(
                "[airplay] mirror header type 0x{:02x} flags 0x{:04x}: {}",
                header[4],
                u16::from_le_bytes([header[6], header[7]]),
                hex(&header[..32]),
            );
        }

        let raw = &header[8..16];
        let le = u64::from_le_bytes([
            raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
        ]);
        let be = u64::from_be_bytes([
            raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
        ]);

        // The byte order of the timestamp is not something we can look up with
        // confidence, so it is decided empirically once: whichever reading puts
        // the capture instant in the recent past is the right one.
        let big_endian = match self.big_endian {
            Some(v) => v,
            None => {
                let pick = if plausible(ntp_delta_ns(now, be)) {
                    Some(true)
                } else if plausible(ntp_delta_ns(now, le)) {
                    Some(false)
                } else {
                    None
                };
                let Some(pick) = pick else {
                    return;
                };
                tprintln!(
                    "[airplay] mirror capture timestamps read as {}-endian NTP",
                    if pick { "big" } else { "little" }
                );
                self.big_endian = Some(pick);
                pick
            }
        };

        let delta = ntp_delta_ns(now, if big_endian { be } else { le });
        if !plausible(delta) {
            return;
        }
        self.deltas.push(delta);
        if self.deltas.len() >= SAMPLE_BATCH {
            self.report();
        }
    }

    fn observe_payload(&mut self, kind: u8, payload: &[u8]) {
        if !self.enabled || self.payload_dumps[kind as usize] == 0 {
            return;
        }
        self.payload_dumps[kind as usize] -= 1;
        let n = payload.len().min(32);
        tprintln!(
            "[airplay] mirror payload type 0x{kind:02x} first {n}B: {}",
            hex(&payload[..n])
        );
    }

    fn finish(&mut self) {
        if self.enabled && !self.deltas.is_empty() {
            self.report();
        }
    }

    fn report(&mut self) {
        self.deltas.sort_unstable();
        let n = self.deltas.len();
        let at = |q: f64| self.deltas[((n as f64 * q) as usize).min(n - 1)];
        tprintln!(
            "[airplay] macOS compositor-to-consumer latency over {n} frames: \
             p50 {:.2}ms p90 {:.2}ms p99 {:.2}ms max {:.2}ms",
            at(0.50) as f64 / 1e6,
            at(0.90) as f64 / 1e6,
            at(0.99) as f64 / 1e6,
            self.deltas[n - 1] as f64 / 1e6,
        );
        self.deltas.clear();
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for b in bytes {
        s.push_str(&format!("{b:02x} "));
    }
    s.pop();
    s
}

/// Matches the keepalive the reference receivers set, so a vanished sender is
/// noticed rather than leaving us blocked in `read` forever.
fn configure_keepalive(stream: &TcpStream) {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, value: libc::c_int, what: &str| {
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &value as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            // Without it a sender that vanishes without a FIN leaves the drain
            // loop blocked in `read` for as long as the process lives.
            teprintln!(
                "[airplay] could not set {what} on the mirror socket: {}",
                std::io::Error::last_os_error()
            );
        }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1, "SO_KEEPALIVE");
    set(libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, 60, "TCP_KEEPALIVE");
}

/// The sender expects *us* to poll *its* timing port; nothing polls ours. We
/// still bind one and answer nothing, because the SETUP response has to name a
/// real port — an unbound one earns ICMP unreachables.
pub struct TimingSocket {
    pub port: u16,
    _socket: std::net::UdpSocket,
}

impl TimingSocket {
    pub fn bind() -> Result<Self, String> {
        let socket = std::net::UdpSocket::bind(("0.0.0.0", 0))
            .map_err(|e| format!("could not bind the AirPlay timing port: {e}"))?;
        let port = socket
            .local_addr()
            .map_err(|e| format!("could not read the timing port: {e}"))?
            .port();
        socket
            .set_nonblocking(true)
            .map_err(|e| format!("could not set the timing socket non-blocking: {e}"))?;
        Ok(Self {
            port,
            _socket: socket,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn packet(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; HEADER_LEN];
        p[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        p[4] = kind;
        p.extend_from_slice(payload);
        p
    }

    #[tokio::test]
    async fn frames_are_consumed_and_counted() {
        let sink = MirrorSink::bind().await.unwrap();
        let port = sink.port();
        let stats = Arc::new(MirrorStats::default());

        let s2 = stats.clone();
        let stop = Cancel::new();
        let s3 = stop.clone();
        let task = tokio::spawn(async move { sink.run(s2, s3).await });

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        client
            .write_all(&packet(TYPE_CODEC_DATA, &[1, 2, 3]))
            .await
            .unwrap();
        client
            .write_all(&packet(TYPE_VIDEO, &vec![0u8; 5000]))
            .await
            .unwrap();
        client
            .write_all(&packet(TYPE_HEARTBEAT, &[]))
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        drop(client);

        // The sink now outlives the connection, so it has to be stopped rather
        // than waited on.
        tokio::time::timeout(Duration::from_secs(2), async {
            while stats.packets.load(Ordering::Relaxed) < 3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("all three packets must be drained");
        stop.cancel();
        task.await.unwrap();

        assert_eq!(stats.packets.load(Ordering::Relaxed), 3);
        assert_eq!(
            stats.bytes.load(Ordering::Relaxed),
            (HEADER_LEN * 3 + 3 + 5000) as u64
        );
        assert!(stats.geometry_changed.load(Ordering::Relaxed));
        assert!(stats.connected.load(Ordering::Relaxed));
    }

    /// The regression that made every session after the first fail: the sink
    /// used to serve one connection and then drop its listener, while the
    /// advertised `dataPort` stayed the same for the receiver's whole life.
    #[tokio::test]
    async fn a_second_session_lands_on_the_same_port() {
        let sink = MirrorSink::bind().await.unwrap();
        let port = sink.port();
        let stats = Arc::new(MirrorStats::default());
        let s2 = stats.clone();
        let stop = Cancel::new();
        let s3 = stop.clone();
        let task = tokio::spawn(async move { sink.run(s2, s3).await });

        for round in 0..2 {
            let mut client = TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap_or_else(|e| panic!("connection {round} refused: {e}"));
            client
                .write_all(&packet(TYPE_VIDEO, &[7u8; 16]))
                .await
                .unwrap();
            client.shutdown().await.unwrap();
            drop(client);
            tokio::time::timeout(Duration::from_secs(2), async {
                while stats.packets.load(Ordering::Relaxed) < round + 1 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("connection {round} was never drained"));
        }

        stop.cancel();
        task.await.unwrap();
        assert_eq!(stats.packets.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn an_absurd_payload_length_is_treated_as_desync() {
        let sink = MirrorSink::bind().await.unwrap();
        let port = sink.port();
        let stats = Arc::new(MirrorStats::default());
        let s2 = stats.clone();
        let stop = Cancel::new();
        let s3 = stop.clone();
        let task = tokio::spawn(async move { sink.run(s2, s3).await });

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut bad = vec![0u8; HEADER_LEN];
        bad[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        client.write_all(&bad).await.unwrap();
        let _ = client.shutdown().await;
        drop(client);

        tokio::time::timeout(Duration::from_secs(2), async {
            while !stats.connected.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the connection must be accepted");
        stop.cancel();
        task.await.unwrap();
        assert_eq!(stats.packets.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn cancellation_stops_a_pending_accept() {
        let sink = MirrorSink::bind().await.unwrap();
        let stop = Cancel::new();
        let stats = Arc::new(MirrorStats::default());
        let s2 = stop.clone();
        let task = tokio::spawn(async move { sink.run(stats, s2).await });
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("run() must return once cancelled")
            .unwrap();
    }

    #[test]
    fn timing_socket_binds_a_real_port() {
        let t = TimingSocket::bind().expect("bind");
        assert!(t.port > 0);
    }

    #[test]
    fn a_session_starts_with_no_latched_state() {
        let s = MirrorStats::default();
        s.begin_connection();
        s.geometry_changed.store(true, Ordering::Relaxed);
        s.packets.store(9, Ordering::Relaxed);
        s.begin_session();
        assert!(!s.connected.load(Ordering::Relaxed));
        assert!(!s.geometry_changed.load(Ordering::Relaxed));
        // Cumulative counters survive; only the per-session latches reset.
        assert_eq!(s.packets.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn ntp_round_trips_through_the_delta() {
        let now = ntp_now();
        assert!(plausible(ntp_delta_ns(now, now)));
        // 20 ms earlier in NTP fixed point.
        let then = now - ((20u64 << 32) / 1000);
        let d = ntp_delta_ns(now, then);
        assert!((19_000_000..21_000_000).contains(&d), "{d}");
        // A timestamp read with the wrong byte order lands nowhere near now.
        assert!(!plausible(ntp_delta_ns(now, now.swap_bytes())));
    }
}

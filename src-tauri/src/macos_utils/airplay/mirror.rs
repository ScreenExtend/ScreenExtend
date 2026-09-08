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
    let mut reports = StreamReports::new();

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
                if kind == TYPE_STREAM_REPORT && take == payload_len {
                    reports.observe(&scratch[..take]);
                }
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

/// The sender's own statistics, which arrive in the clear and used to be counted and dropped.
///
/// Type-`0x05` packets are not video: their payload is a `bplist00` binary property list carrying
/// the AirPlay sender's view of the link — round-trip time, bandwidth, queue depth — measured by
/// macOS itself, about macOS itself, once a second. No FairPlay, no AES, nothing to decrypt; only
/// the type-`0x00` payloads are ciphertext.
///
/// The first report is logged with every scalar it carries, because the key set is undocumented
/// and a field report is the only way to learn it. After that only the numeric keys are logged,
/// throttled, so a long session does not fill the backlog with one line a second.
struct StreamReports {
    since: Instant,
    seen: u64,
}

/// How often the numeric summary is logged after the first full report.
const STREAM_REPORT_INTERVAL: Duration = Duration::from_secs(15);

impl StreamReports {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            seen: 0,
        }
    }

    fn observe(&mut self, payload: &[u8]) {
        let first = self.seen == 0;
        self.seen += 1;
        if !first && self.since.elapsed() < STREAM_REPORT_INTERVAL {
            return;
        }
        self.since = Instant::now();

        // A stream report is a small property list followed by a ~25 KiB trailer of something
        // else, so the whole payload does not parse. `binary_plist_len` finds where the plist
        // ends.
        let Some(len) = binary_plist_len(payload) else {
            if first {
                tprintln!(
                    "[airplay] stream report ({} bytes) does not start with a binary property \
                     list; leaving it alone",
                    payload.len()
                );
            }
            return;
        };
        let Ok(value) = plist::from_bytes::<plist::Value>(&payload[..len]) else {
            if first {
                tprintln!(
                    "[airplay] stream report: {len} of {} bytes look like a property list but it \
                     did not parse",
                    payload.len()
                );
            }
            return;
        };
        if first {
            tprintln!(
                "[airplay] stream reports carry a {len}-byte property list and a {}-byte trailer",
                payload.len() - len
            );
        }

        let mut fields = Vec::new();
        collect_scalars(&value, "", &mut fields, first);
        if fields.is_empty() {
            return;
        }
        fields.sort();
        tprintln!(
            "[airplay] sender stream report #{}: {}",
            self.seen,
            fields.join(" ")
        );
    }
}

/// Where the binary property list at the start of `buf` ends.
///
/// A stream report is a small `bplist00` followed by a much larger trailer, so handing the whole
/// payload to the parser fails. A binary plist ends with a 32-byte trailer whose last three fields
/// are `numObjects`, `topObject` and `offsetTableOffset`, and the offset table sits immediately
/// before that trailer — which makes the end position self-describing: the only `end` whose
/// trailer is internally consistent is the real one.
fn binary_plist_len(buf: &[u8]) -> Option<usize> {
    const HEADER: &[u8] = b"bplist00";
    const TRAILER: usize = 32;
    if buf.len() < HEADER.len() + TRAILER || !buf.starts_with(HEADER) {
        return None;
    }
    for end in HEADER.len() + TRAILER..=buf.len() {
        let t = &buf[end - TRAILER..end];
        let offset_int_size = t[6] as usize;
        let object_ref_size = t[7] as usize;
        if !(1..=8).contains(&offset_int_size) || !(1..=8).contains(&object_ref_size) {
            continue;
        }
        let num_objects = u64::from_be_bytes(t[8..16].try_into().ok()?) as usize;
        let top_object = u64::from_be_bytes(t[16..24].try_into().ok()?) as usize;
        let table_offset = u64::from_be_bytes(t[24..32].try_into().ok()?) as usize;
        if num_objects == 0 || num_objects > 1 << 20 || top_object >= num_objects {
            continue;
        }
        if table_offset < HEADER.len() {
            continue;
        }
        if table_offset.checked_add(num_objects * offset_int_size) == Some(end - TRAILER) {
            return Some(end);
        }
    }
    None
}

/// Flatten a property list into `key=value` pairs.
///
/// `numbers_only` drops strings and booleans, which is what the throttled summaries want; the
/// first report keeps everything so the schema is recorded once.
fn collect_scalars(value: &plist::Value, prefix: &str, out: &mut Vec<String>, everything: bool) {
    const MAX_FIELDS: usize = 64;
    if out.len() >= MAX_FIELDS {
        return;
    }
    match value {
        plist::Value::Dictionary(dict) => {
            for (key, child) in dict {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_scalars(child, &path, out, everything);
            }
        }
        plist::Value::Array(items) => {
            for (index, child) in items.iter().enumerate().take(4) {
                collect_scalars(child, &format!("{prefix}[{index}]"), out, everything);
            }
        }
        plist::Value::Integer(n) => out.push(format!("{prefix}={n}")),
        plist::Value::Real(n) => out.push(format!("{prefix}={n:.3}")),
        plist::Value::Boolean(b) if everything => out.push(format!("{prefix}={b}")),
        plist::Value::String(s) if everything => {
            out.push(format!("{prefix}={:?}", &s[..s.len().min(32)]))
        }
        _ => {}
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

/// A 64-bit NTP fixed-point value in nanoseconds: seconds in the high 32 bits, a binary fraction
/// in the low 32.
fn ntp_fixed_to_ns(v: u64) -> i128 {
    ((v as i128) * 1_000_000_000) >> 32
}

/// Whether an NTP fixed-point reading is a plausible *sender-epoch* stamp.
///
/// Measured on 10.15.8: the mirror header's timestamp is not a wall clock. Read little-endian its
/// seconds field was 41457 and then 41459 two seconds later — a monotonic clock whose epoch is
/// somewhere inside this boot, not 1900 and not 1970. Read big-endian the same bytes are 11.5
/// years, which is what makes the byte order decidable at all.
fn plausible_sender_seconds(v: u64) -> bool {
    const MAX_UPTIME_SECS: u64 = 400 * 24 * 3600;
    let secs = v >> 32;
    secs > 0 && secs < MAX_UPTIME_SECS
}

/// Reads the plaintext part of the mirror header.
///
/// Every published AirPlay receiver frames a mirror packet as a 128-byte header whose first five
/// bytes are the payload length and the packet type, with a flag field at offset 6 and the
/// sender's capture time — NTP fixed point — at offset 8. This module read five of those bytes and
/// discarded the rest, which threw away the one thing on this connection nothing else in the tree
/// can see: the sender is *this same Mac*, so the distance between that timestamp and the instant
/// the bytes reach our socket is macOS's own compose-to-consumer path.
///
/// **What the stamp turned out to be, measured on 10.15.8:** not a wall clock. Read
/// little-endian, its seconds field read 41457 while the machine had been up fourteen hours — a
/// monotonic clock whose epoch is somewhere inside this boot and is published nowhere. So the
/// *absolute* figure is unobtainable without implementing AirPlay's own NTP time sync, which this
/// receiver deliberately does not.
///
/// What survives is the half that does not need the epoch. Both clocks are the same machine's
/// monotonic time, so the offset between them is a constant: subtracting the smallest difference
/// seen in a session leaves how much later than its own best case each frame arrived. That is the
/// variability of macOS's compose-to-consumer path, which is the half that matters to a stream.
///
/// Off unless `SE_AIRPLAY_TIMING=1`, because the measurement is only interesting with
/// `SE_AIRPLAY_MAX_FPS` raised, and that makes the host encode its whole desktop for us to throw
/// away. Purely a read of bytes that already arrive — no decryption, and none is possible: only
/// the payload is FairPlay ciphertext.
struct HeaderProbe {
    enabled: bool,
    /// Header hexdumps still owed, per packet type.
    header_dumps: [u8; 256],
    /// Payload hexdumps still owed, per packet type. Confirms whether a
    /// type-`0x00` payload is really ciphertext or whether a NAL start code is
    /// visible in the clear.
    payload_dumps: [u8; 256],
    /// `receiver monotonic ns - sender stamp ns`, one per stamped packet. Only the differences
    /// between these are meaningful; the constant epoch offset is folded out at report time.
    deltas: Vec<i64>,
    big_endian: Option<bool>,
    started: Instant,
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
            started: Instant::now(),
        }
    }

    fn observe(&mut self, header: &[u8; HEADER_LEN]) {
        if !self.enabled {
            return;
        }
        let now = self.started.elapsed().as_nanos() as i128;
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

        // Stream reports carry zeros here; only video and codec data are stamped.
        if le == 0 {
            return;
        }

        // The byte order is not something to look up with confidence, so it is decided once, on
        // the evidence: one reading is a sane number of seconds of uptime, the other is a decade.
        let big_endian = match self.big_endian {
            Some(v) => v,
            None => {
                let pick = if plausible_sender_seconds(be) {
                    Some(true)
                } else if plausible_sender_seconds(le) {
                    Some(false)
                } else {
                    None
                };
                let Some(pick) = pick else {
                    return;
                };
                let chosen = if pick { be } else { le };
                tprintln!(
                    "[airplay] mirror capture stamps read as {}-endian NTP fixed point, {} s into \
                     the sender's own epoch",
                    if pick { "big" } else { "little" },
                    chosen >> 32
                );
                self.big_endian = Some(pick);
                pick
            }
        };

        let sender = ntp_fixed_to_ns(if big_endian { be } else { le });
        self.deltas.push((now - sender) as i64);
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
        // The epoch offset between the sender's clock and ours is constant and unknown, so the
        // fastest frame in the batch is the zero. Everything reported is *excess over best case*,
        // not an absolute latency, and saying so in the line matters: an absolute number here
        // would be a fabrication.
        let floor = self.deltas[0];
        let at = |q: f64| self.deltas[((n as f64 * q) as usize).min(n - 1)] - floor;
        tprintln!(
            "[airplay] macOS compose-to-consumer spread over {n} frames (excess over the fastest \
             frame in the batch; the absolute floor needs AirPlay's NTP sync, which we do not \
             implement): p50 {:.2}ms p90 {:.2}ms p99 {:.2}ms max {:.2}ms",
            at(0.50) as f64 / 1e6,
            at(0.90) as f64 / 1e6,
            at(0.99) as f64 / 1e6,
            (self.deltas[n - 1] - floor) as f64 / 1e6,
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

/// Our end of the timing exchange: bound, named in the SETUP reply, never read.
///
/// The SETUP response has to name a real port — an unbound one earns ICMP
/// unreachables and the sender treats that as a broken receiver. The sender does
/// send replies here, and a real receiver would run an NTP-style exchange over
/// it to align clocks. We do not need to: the sender is this same Mac, so its
/// timestamps and ours already share a hardware timebase.
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

    /// The bytes are from a real header on 10.15.8, little-endian, 41457 seconds into the
    /// sender's own epoch while the machine had been up fourteen hours. Read the other way round
    /// the same eight bytes are eleven and a half years, which is what makes the byte order
    /// decidable without a specification.
    #[test]
    fn the_capture_stamp_byte_order_is_decidable_from_one_sample() {
        let raw = [0x15u8, 0xb0, 0x57, 0x32, 0xf1, 0xa1, 0x00, 0x00];
        let le = u64::from_le_bytes(raw);
        let be = u64::from_be_bytes(raw);
        assert_eq!(le >> 32, 41457);
        assert!(plausible_sender_seconds(le));
        assert!(!plausible_sender_seconds(be));
        assert_eq!(ntp_fixed_to_ns(1u64 << 32), 1_000_000_000);
    }

    #[test]
    fn a_property_list_is_found_inside_a_larger_payload() {
        let mut dict = plist::Dictionary::new();
        dict.insert("rttAvg".to_string(), plist::Value::Real(0.0031));
        dict.insert("bytes".to_string(), plist::Value::Integer(4096.into()));
        let mut encoded = Vec::new();
        plist::to_writer_binary(&mut encoded, &plist::Value::Dictionary(dict)).unwrap();
        let exact = encoded.len();

        // The real payload is the plist plus a trailer several hundred times its size.
        let mut payload = encoded.clone();
        payload.extend(std::iter::repeat_n(0xABu8, 25_000));

        assert_eq!(binary_plist_len(&payload), Some(exact));
        assert!(plist::from_bytes::<plist::Value>(&payload).is_err());
        assert!(plist::from_bytes::<plist::Value>(&payload[..exact]).is_ok());
        assert_eq!(binary_plist_len(b"not a plist at all, really"), None);
    }

    #[test]
    fn scalars_are_flattened_with_their_paths() {
        let mut inner = plist::Dictionary::new();
        inner.insert("VrttAvg".to_string(), plist::Value::Real(1.5));
        let mut outer = plist::Dictionary::new();
        outer.insert("stats".to_string(), plist::Value::Dictionary(inner));
        outer.insert("name".to_string(), plist::Value::String("mirror".into()));

        let mut numbers = Vec::new();
        collect_scalars(
            &plist::Value::Dictionary(outer.clone()),
            "",
            &mut numbers,
            false,
        );
        assert_eq!(numbers, vec!["stats.VrttAvg=1.500".to_string()]);

        let mut everything = Vec::new();
        collect_scalars(&plist::Value::Dictionary(outer), "", &mut everything, true);
        assert_eq!(everything.len(), 2);
    }
}

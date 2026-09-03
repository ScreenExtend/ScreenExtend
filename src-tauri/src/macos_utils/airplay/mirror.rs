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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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

    /// Accepts one connection and drains it until the peer goes away or `stop`
    /// is cancelled.
    pub async fn run(self, stats: Arc<MirrorStats>, stop: Cancel) {
        let stream = tokio::select! {
            accepted = self.listener.accept() => match accepted {
                Ok((s, peer)) => {
                    tprintln!("[airplay] mirror stream connected from {peer}");
                    s
                }
                Err(e) => {
                    teprintln!("[airplay] mirror accept failed: {e}");
                    return;
                }
            },
            _ = stop.cancelled() => return,
        };
        stats.connected.store(true, Ordering::Relaxed);

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
            }
        }
    }
}

async fn drain(mut stream: TcpStream, stats: &MirrorStats) -> Result<(), String> {
    let mut header = [0u8; HEADER_LEN];
    let mut scratch = vec![0u8; 64 * 1024];

    loop {
        match stream.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(format!("header read failed: {e}")),
        }

        let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if payload_len > MAX_PAYLOAD {
            return Err(format!(
                "lost framing on the mirror stream (payload claimed {payload_len} bytes)"
            ));
        }

        match header[4] {
            TYPE_CODEC_DATA => {
                // SPS/PPS. Re-sent when the sender's video geometry changes.
                stats.geometry_changed.store(true, Ordering::Relaxed);
            }
            TYPE_VIDEO | TYPE_HEARTBEAT | TYPE_STREAM_REPORT => {}
            other => {
                tprintln!("[airplay] mirror stream: ignoring unknown packet type 0x{other:02x}");
            }
        }

        let mut left = payload_len;
        while left > 0 {
            let take = left.min(scratch.len());
            match stream.read_exact(&mut scratch[..take]).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(format!("payload read failed: {e}")),
            }
            left -= take;
        }

        stats.packets.fetch_add(1, Ordering::Relaxed);
        stats
            .bytes
            .fetch_add((HEADER_LEN + payload_len) as u64, Ordering::Relaxed);
    }
}

/// Matches the keepalive the reference receivers set, so a vanished sender is
/// noticed rather than leaving us blocked in `read` forever.
fn configure_keepalive(stream: &TcpStream) {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    unsafe {
        let on: libc::c_int = 1;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        let idle: libc::c_int = 60;
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPALIVE,
            &idle as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// The sender expects *us* to poll *its* timing port; nothing polls ours. We
/// still bind one and answer nothing, because the SETUP response has to name a
/// real port and the sender sends replies there.
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

/// How long to wait for the sender to open the mirror connection after RECORD.
pub const CONNECT_GRACE: Duration = Duration::from_secs(10);

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
        let task = tokio::spawn(async move { sink.run(s2, Cancel::new()).await });

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

        task.await.unwrap();

        assert_eq!(stats.packets.load(Ordering::Relaxed), 3);
        assert_eq!(
            stats.bytes.load(Ordering::Relaxed),
            (HEADER_LEN * 3 + 3 + 5000) as u64
        );
        assert!(stats.geometry_changed.load(Ordering::Relaxed));
        assert!(stats.connected.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn an_absurd_payload_length_is_treated_as_desync() {
        let sink = MirrorSink::bind().await.unwrap();
        let port = sink.port();
        let stats = Arc::new(MirrorStats::default());
        let s2 = stats.clone();
        let task = tokio::spawn(async move { sink.run(s2, Cancel::new()).await });

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut bad = vec![0u8; HEADER_LEN];
        bad[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        client.write_all(&bad).await.unwrap();
        let _ = client.shutdown().await;

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
}

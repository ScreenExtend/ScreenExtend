//! The UDP socket the media actually rides, and the options webrtc-rs never sets on it.
//!
//! `grep -rn "CONNRESET\|SO_SNDBUF\|set_tos\|IP_TOS\|socket2"` over `webrtc-ice-0.17.1` and
//! `webrtc-util-0.17.1` returns nothing: ICE sockets are created by `listen_udp_in_port_range`
//! with OS defaults and never touched again. On a Wi-Fi second monitor that costs three things —
//! video queued in the AP's best-effort access category alongside every background download, a
//! send buffer that a keyframe burst can overrun inside our own process, and, on Windows, the
//! `SIO_UDP_CONNRESET` footgun where one ICMP port-unreachable makes the *next* `recv_from` fail.
//!
//! The only hook webrtc-rs offers is `SettingEngine::set_udp_network(UDPNetwork::Muxed(..))`,
//! which accepts any `Conn` — and `impl Conn for tokio::net::UdpSocket` exists, so a socket
//! configured here can become the ICE transport. It has a hard cost:
//! `agent_gather.rs:117-120` skips server-reflexive gathering entirely in muxed mode
//! (`UDPNetwork::Muxed(_) => continue`). Relay gathering is unaffected, and a same-subnet peer is
//! already offered no STUN server at all, so this is used **only** on a session classified
//! `SameNetwork` — where srflx candidates were never going to be gathered anyway.
//!
//! Everything here is a probed tier: a failure is logged once and the session carries on with the
//! stock ephemeral sockets.

use std::net::{SocketAddr, UdpSocket};

/// Send and receive buffer size for the media socket.
///
/// Sunshine sets exactly this (`send_buffer_size(1024*1024)`) for the same reason: a 200 KB IDR
/// written as ~170 back-to-back datagrams can exhaust a default send buffer and be dropped inside
/// the sending process, before the NIC ever sees it. The pacer makes that far less likely; the
/// buffer is what covers the burst the pacer is explicitly told not to smooth on a LAN.
pub const UDP_BUFFER_BYTES: usize = 1024 * 1024;

/// DSCP AF41 (0x22 << 2 = 0x88) is the class the Wi-Fi Alliance maps to WMM AC_VI.
///
/// macOS accepts `IP_TOS` on an unconnected socket with no entitlement, and pairs it with
/// `SO_NET_SERVICE_TYPE`, which is the API that actually drives the layer-2 WMM access category
/// (`xnu bsd/sys/socket.h` documents the service types as "different categories of delay, jitter
/// and loss parameters" and cites RFC 4594).
#[cfg(target_os = "macos")]
const IP_TOS_AF41: libc::c_int = 0x88;

/// `NET_SERVICE_TYPE_VI` — interactive video, from `xnu bsd/sys/socket.h`
/// (`BE 0, BK 1, SIG 2, VI 3, VO 4, RV 5, AV 6, OAM 7, RD 8`). `VI` rather than `VO`, because
/// input, audio and video share one socket under BUNDLE and marking a 20 Mbps video stream as
/// Voice is antisocial and may be policed by the AP.
#[cfg(target_os = "macos")]
const NET_SERVICE_TYPE_VI: libc::c_int = 3;

#[cfg(target_os = "macos")]
const SO_NET_SERVICE_TYPE: libc::c_int = 0x1116;

/// What was actually applied to one socket, for the log line and the capability tier.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SocketTuning {
    pub send_buffer: bool,
    pub recv_buffer: bool,
    /// Windows only: `SIO_UDP_CONNRESET` turned off.
    pub connreset_disabled: bool,
    /// macOS only: `IP_TOS` / `SO_NET_SERVICE_TYPE` set.
    pub dscp_marked: bool,
}

impl SocketTuning {
    pub fn describe(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.send_buffer {
            parts.push("SO_SNDBUF");
        }
        if self.recv_buffer {
            parts.push("SO_RCVBUF");
        }
        if self.connreset_disabled {
            parts.push("SIO_UDP_CONNRESET=off");
        }
        if self.dscp_marked {
            parts.push("DSCP/AC_VI");
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Bind a UDP socket for media and apply everything the platform lets us apply without a
/// destination address.
pub fn bind_tuned(bind: SocketAddr) -> std::io::Result<(UdpSocket, SocketTuning)> {
    let sock = UdpSocket::bind(bind)?;
    let tuning = tune(&sock);
    Ok((sock, tuning))
}

pub fn tune(sock: &UdpSocket) -> SocketTuning {
    SocketTuning {
        send_buffer: set_buffer(sock, true),
        recv_buffer: set_buffer(sock, false),
        connreset_disabled: disable_connreset(sock),
        dscp_marked: mark_interactive_video(sock),
    }
}

#[cfg(windows)]
fn raw_socket(sock: &UdpSocket) -> usize {
    use std::os::windows::io::AsRawSocket;
    sock.as_raw_socket() as usize
}

#[cfg(target_os = "macos")]
fn raw_socket(sock: &UdpSocket) -> i32 {
    use std::os::fd::AsRawFd;
    sock.as_raw_fd()
}

#[cfg(windows)]
fn set_buffer(sock: &UdpSocket, send: bool) -> bool {
    use windows::Win32::Networking::WinSock::{
        setsockopt, SOCKET, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF,
    };
    let value = UDP_BUFFER_BYTES as i32;
    let bytes = value.to_ne_bytes();
    let opt = if send { SO_SNDBUF } else { SO_RCVBUF };
    unsafe { setsockopt(SOCKET(raw_socket(sock)), SOL_SOCKET, opt, Some(&bytes)) == 0 }
}

#[cfg(target_os = "macos")]
fn set_buffer(sock: &UdpSocket, send: bool) -> bool {
    let value = UDP_BUFFER_BYTES as libc::c_int;
    let opt = if send {
        libc::SO_SNDBUF
    } else {
        libc::SO_RCVBUF
    };
    unsafe {
        libc::setsockopt(
            raw_socket(sock),
            libc::SOL_SOCKET,
            opt,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn set_buffer(_sock: &UdpSocket, _send: bool) -> bool {
    false
}

/// Turn off `SIO_UDP_CONNRESET`.
///
/// On Windows an *unconnected* UDP socket that receives an ICMP port-unreachable makes the next
/// `recv_from` fail with `WSAECONNRESET`. Whether webrtc-rs's read loop survives that is
/// unverified — which is the reason to remove the question rather than answer it. It is the
/// classic Windows UDP-server footgun and nothing in the stack disables it.
#[cfg(windows)]
fn disable_connreset(sock: &UdpSocket) -> bool {
    use windows::Win32::Networking::WinSock::{WSAIoctl, SIO_UDP_CONNRESET, SOCKET};
    let mut disable: u32 = 0;
    let mut returned: u32 = 0;
    unsafe {
        WSAIoctl(
            SOCKET(raw_socket(sock)),
            SIO_UDP_CONNRESET,
            Some((&mut disable as *mut u32).cast()),
            std::mem::size_of::<u32>() as u32,
            None,
            0,
            &mut returned,
            None,
            None,
        ) == 0
    }
}

#[cfg(not(windows))]
fn disable_connreset(_sock: &UdpSocket) -> bool {
    false
}

/// Mark the socket as interactive video where the OS supports it on an unconnected socket.
///
/// **Windows deliberately does nothing here.** `setsockopt(IP_TOS)` is silently ignored unless
/// `HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\DisableUserTOSSetting` is cleared,
/// and writing that key is not something a screen-sharing app should do to a user's machine. The
/// supported route is qWAVE, which needs the destination address and therefore cannot run until
/// the ICE pair is nominated — see [`mark_video_flow`].
#[cfg(target_os = "macos")]
fn mark_interactive_video(sock: &UdpSocket) -> bool {
    let svc = NET_SERVICE_TYPE_VI;
    let svc_ok = unsafe {
        libc::setsockopt(
            raw_socket(sock),
            libc::SOL_SOCKET,
            SO_NET_SERVICE_TYPE,
            (&svc as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    };
    let tos = IP_TOS_AF41;
    let tos_ok = unsafe {
        libc::setsockopt(
            raw_socket(sock),
            libc::IPPROTO_IP,
            libc::IP_TOS,
            (&tos as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    };
    svc_ok || tos_ok
}

#[cfg(not(target_os = "macos"))]
fn mark_interactive_video(_sock: &UdpSocket) -> bool {
    false
}

/// A qWAVE flow, released when dropped.
///
/// Windows marks DSCP through qWAVE and nowhere else.
/// [`QOSAddSocketToFlow`](https://learn.microsoft.com/en-us/windows/win32/api/qos2/nf-qos2-qosaddsockettoflow)
/// with `QOSTrafficTypeAudioVideo` yields DSCP 0x28 (CS5) plus 802.1p 5, and — unlike
/// `QOSSetFlow(QOSSetOutgoingDSCPValue, …)`, which needs Administrators or Network Configuration
/// Operators — the traffic-type route needs no privilege at all. The 802.1p tag is only added for
/// an *adaptive* flow whose destination is on the local link, which is exactly the same-subnet
/// second-monitor case, so `QOS_NON_ADAPTIVE_FLOW` is deliberately not passed.
///
/// `DestAddr` is required because ICE sockets are unconnected, which is why this cannot be part
/// of [`tune`] and runs once the nominated pair is known.
#[cfg(windows)]
pub struct QosFlow {
    handle: windows::Win32::Foundation::HANDLE,
    flow_id: u32,
}

#[cfg(windows)]
impl Drop for QosFlow {
    fn drop(&mut self) {
        use windows::Win32::NetworkManagement::QoS::QOSCloseHandle;
        unsafe {
            let _ = QOSCloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
unsafe impl Send for QosFlow {}
#[cfg(windows)]
unsafe impl Sync for QosFlow {}

#[cfg(windows)]
impl QosFlow {
    pub fn id(&self) -> u32 {
        self.flow_id
    }
}

#[cfg(windows)]
pub fn mark_video_flow(raw: usize, dest: SocketAddr) -> Option<QosFlow> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::NetworkManagement::QoS::{
        QOSAddSocketToFlow, QOSCreateHandle, QOSTrafficTypeAudioVideo, QOS_VERSION,
    };
    use windows::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKET,
    };

    let version = QOS_VERSION {
        MajorVersion: 1,
        MinorVersion: 0,
    };
    let mut handle = HANDLE::default();
    let created = unsafe { QOSCreateHandle(&version, &mut handle) };
    if !created.as_bool() {
        // qWAVE service disabled, or the platform does not have it. Carry on unmarked.
        return None;
    }

    let mut flow_id: u32 = 0;
    let ok = match dest {
        SocketAddr::V4(v4) => {
            let mut sa = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: v4.port().to_be(),
                ..Default::default()
            };
            sa.sin_addr.S_un.S_addr = u32::from_ne_bytes(v4.ip().octets());
            unsafe {
                QOSAddSocketToFlow(
                    handle,
                    SOCKET(raw),
                    Some((&sa as *const SOCKADDR_IN).cast::<SOCKADDR>()),
                    QOSTrafficTypeAudioVideo,
                    None,
                    &mut flow_id,
                )
            }
        }
        SocketAddr::V6(v6) => {
            let mut sa = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: v6.port().to_be(),
                ..Default::default()
            };
            sa.sin6_addr.u.Byte = v6.ip().octets();
            unsafe {
                QOSAddSocketToFlow(
                    handle,
                    SOCKET(raw),
                    Some((&sa as *const SOCKADDR_IN6).cast::<SOCKADDR>()),
                    QOSTrafficTypeAudioVideo,
                    None,
                    &mut flow_id,
                )
            }
        }
    };
    if !ok.as_bool() {
        unsafe {
            let _ = windows::Win32::NetworkManagement::QoS::QOSCloseHandle(handle);
        }
        return None;
    }
    Some(QosFlow { handle, flow_id })
}

#[cfg(not(windows))]
pub struct QosFlow;

#[cfg(not(windows))]
pub fn mark_video_flow(_raw: i32, _dest: SocketAddr) -> Option<QosFlow> {
    // macOS marks per-socket in `tune`, with no destination needed.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_socket_gets_its_buffers() {
        let (sock, tuning) = bind_tuned("127.0.0.1:0".parse().unwrap()).expect("bind");
        assert!(sock.local_addr().is_ok());
        assert!(
            tuning.send_buffer,
            "SO_SNDBUF must apply on every supported platform"
        );
        assert!(tuning.recv_buffer);
        assert!(!tuning.describe().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_disables_the_connreset_footgun() {
        let (_sock, tuning) = bind_tuned("127.0.0.1:0".parse().unwrap()).expect("bind");
        assert!(tuning.connreset_disabled);
        assert!(
            !tuning.dscp_marked,
            "Windows marks through qWAVE after nomination, never with setsockopt"
        );
    }

    #[test]
    fn the_description_names_what_was_applied() {
        let t = SocketTuning {
            send_buffer: true,
            recv_buffer: false,
            connreset_disabled: false,
            dscp_marked: true,
        };
        assert_eq!(t.describe(), "SO_SNDBUF, DSCP/AC_VI");
        assert_eq!(SocketTuning::default().describe(), "none");
    }
}

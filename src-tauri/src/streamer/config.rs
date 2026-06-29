use std::net::Ipv4Addr;

use super::session::{
    SessionAuth, SharedDeviceOverrides, SharedDeviceReporter, SharedDisconnectGrace,
    SharedSessions, SharedTurnConfig, SharedVirtualDisplay,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum H264Profile {
    #[default]
    Baseline,
    Main,
    High,
}

impl H264Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "baseline" | "base" | "cb" => Some(Self::Baseline),
            "main" => Some(Self::Main),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderVendor {
    #[default]
    Auto,
    Nvidia,
    Intel,
}

impl EncoderVendor {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "nvidia" | "nvenc" => Some(Self::Nvidia),
            "intel" | "quicksync" | "qsv" | "onevpl" | "vpl" => Some(Self::Intel),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScalePercent(u32);

impl ScalePercent {
    pub const MIN: u32 = 10;
    pub const MAX: u32 = 100;

    pub fn new(percent: u32) -> Self {
        ScalePercent(percent.clamp(Self::MIN, Self::MAX))
    }

    pub fn percent(self) -> u32 {
        self.0
    }

    pub fn apply(self, native: u32) -> u32 {
        ((native as u64 * self.0 as u64 + 50) / 100) as u32
    }

    pub fn is_native(self) -> bool {
        self.0 >= Self::MAX
    }

    pub fn parse(s: &str) -> Option<Self> {
        s.trim()
            .trim_end_matches('%')
            .trim()
            .parse::<u32>()
            .ok()
            .map(ScalePercent::new)
    }
}

impl Default for ScalePercent {
    fn default() -> Self {
        ScalePercent(100)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_ip: Ipv4Addr,
    pub lan_ip: Option<String>,
    pub port: u16,
    pub https_port: u16,
    pub monitor: u32,
    pub probe_capture: Option<String>,
    pub probe_dxgi: Option<String>,
    pub probe_encode: Option<String>,
    pub whep_selftest: bool,
    pub probe_live: Option<String>,
    pub synthetic_pattern: bool,
    pub probe_bitrate: bool,
    pub scale: ScalePercent,
    pub stun_urls: Vec<String>,
    pub turn_url: Option<String>,
    pub turn_username: Option<String>,
    pub turn_credential: Option<String>,
    pub turn_urls: Vec<String>,
    pub turn_secret: Option<String>,
    pub turn_ttl_secs: u64,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub fps: Option<u32>,
    pub h264_profile: H264Profile,
    pub max_fps: u32,
    pub max_bitrate_kbps: Option<u32>,
    pub qp: Option<u8>,
    pub intra_refresh: bool,
    pub encoder_vendor: EncoderVendor,
    pub virtual_display: Option<SharedVirtualDisplay>,
    pub session_auth: Option<SessionAuth>,
    pub device_reporter: Option<SharedDeviceReporter>,
    pub device_overrides: Option<SharedDeviceOverrides>,
    pub sessions: Option<SharedSessions>,
    pub disconnect_grace: Option<SharedDisconnectGrace>,
    pub user_turn: Option<SharedTurnConfig>,
}

const DEFAULT_STUN_URL: &str = "stun:stun.l.google.com:19302";

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_ip: Ipv4Addr::UNSPECIFIED,
            lan_ip: None,
            port: 8080,
            https_port: 8443,
            monitor: 1,
            probe_capture: None,
            probe_dxgi: None,
            probe_encode: None,
            whep_selftest: false,
            probe_live: None,
            synthetic_pattern: false,
            probe_bitrate: false,
            scale: ScalePercent::default(),
            stun_urls: vec![DEFAULT_STUN_URL.to_string()],
            turn_url: None,
            turn_username: None,
            turn_credential: None,
            turn_urls: vec![],
            turn_secret: None,
            turn_ttl_secs: 600,
            tls_cert: None,
            tls_key: None,
            fps: None,
            h264_profile: H264Profile::Baseline,
            max_fps: 500,
            max_bitrate_kbps: None,
            qp: None,
            intra_refresh: false,
            encoder_vendor: EncoderVendor::Auto,
            virtual_display: None,
            session_auth: None,
            device_reporter: None,
            device_overrides: None,
            sessions: None,
            disconnect_grace: None,
            user_turn: None,
        }
    }
}

impl Config {
    pub fn from_args() -> Self {
        let mut c = Config::default();
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;

        let val = |args: &[String], i: usize| -> Option<String> {
            args.get(i + 1).filter(|s| !s.starts_with("--")).cloned()
        };

        while i < args.len() {
            match args[i].as_str() {
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--port" => {
                    if let Some(v) = val(&args, i).and_then(|s| s.parse().ok()) {
                        c.port = v;
                    }
                    i += 2;
                }
                "--https-port" => {
                    if let Some(v) = val(&args, i).and_then(|s| s.parse().ok()) {
                        c.https_port = v;
                    }
                    i += 2;
                }
                "--monitor" => {
                    if let Some(v) = val(&args, i).and_then(|s| s.parse().ok()) {
                        c.monitor = v;
                    }
                    i += 2;
                }
                "--scale" => {
                    if let Some(s) = val(&args, i).and_then(|s| ScalePercent::parse(&s)) {
                        c.scale = s;
                    }
                    i += 2;
                }
                "--stun" => {
                    if let Some(s) = val(&args, i) {
                        c.stun_urls = s
                            .split(',')
                            .map(|u| u.trim().to_string())
                            .filter(|u| !u.is_empty())
                            .collect();
                    }
                    i += 2;
                }
                "--turn-url" => {
                    c.turn_url = val(&args, i);
                    i += 2;
                }
                "--turn-username" => {
                    c.turn_username = val(&args, i);
                    i += 2;
                }
                "--turn-credential" => {
                    c.turn_credential = val(&args, i);
                    i += 2;
                }
                "--turn-secret" => {
                    c.turn_secret = val(&args, i).filter(|s| !s.trim().is_empty());
                    i += 2;
                }
                "--turn-urls" => {
                    if let Some(s) = val(&args, i) {
                        c.turn_urls = s
                            .split(',')
                            .map(|u| u.trim().to_string())
                            .filter(|u| !u.is_empty())
                            .collect();
                    }
                    i += 2;
                }
                "--turn-ttl" => {
                    if let Some(v) = val(&args, i).and_then(|s| s.parse::<u64>().ok()) {
                        c.turn_ttl_secs = v.max(60);
                    }
                    i += 2;
                }
                "--tls-cert" => {
                    c.tls_cert = val(&args, i);
                    i += 2;
                }
                "--tls-key" => {
                    c.tls_key = val(&args, i);
                    i += 2;
                }
                "--fps" => {
                    if let Some(v) = val(&args, i).and_then(|s| s.parse::<u32>().ok()) {
                        c.fps = Some(v.clamp(15, 500));
                    }
                    i += 2;
                }
                "--max-fps" => {
                    if let Some(v) = val(&args, i).and_then(|s| s.parse::<u32>().ok()) {
                        c.max_fps = v.clamp(15, 500);
                    }
                    i += 2;
                }
                "--max-bitrate-kbps" => {
                    c.max_bitrate_kbps = val(&args, i).and_then(|s| s.parse::<u32>().ok());
                    i += 2;
                }
                "--h264-profile" => {
                    if let Some(p) = val(&args, i).and_then(|s| H264Profile::parse(&s)) {
                        c.h264_profile = p;
                    }
                    i += 2;
                }
                "--qp" => {
                    c.qp = val(&args, i)
                        .and_then(|s| s.parse::<u8>().ok())
                        .map(|q| q.clamp(1, 51));
                    i += 2;
                }
                "--intra-refresh" => {
                    if let Some(v) = val(&args, i) {
                        c.intra_refresh =
                            matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "1" | "true" | "yes");
                    }
                    i += 2;
                }
                "--encoder" => {
                    if let Some(v) = val(&args, i).and_then(|s| EncoderVendor::parse(&s)) {
                        c.encoder_vendor = v;
                    }
                    i += 2;
                }
                "--bind-ip" => {
                    if let Some(ip) = val(&args, i).and_then(|s| s.parse().ok()) {
                        c.bind_ip = ip;
                    }
                    i += 2;
                }
                "--lan-ip" => {
                    c.lan_ip = val(&args, i);
                    i += 2;
                }
                "--probe-capture" => {
                    let (path, step) = match val(&args, i) {
                        Some(p) => (p, 2),
                        None => ("capture_probe.png".to_string(), 1),
                    };
                    c.probe_capture = Some(path);
                    i += step;
                }
                "--probe-dxgi" => {
                    let (path, step) = match val(&args, i) {
                        Some(p) => (p, 2),
                        None => ("dxgi_probe.bmp".to_string(), 1),
                    };
                    c.probe_dxgi = Some(path);
                    i += step;
                }
                "--probe-encode" => {
                    let (path, step) = match val(&args, i) {
                        Some(p) => (p, 2),
                        None => ("out.h264".to_string(), 1),
                    };
                    c.probe_encode = Some(path);
                    i += step;
                }
                "--probe-live" => {
                    let (path, step) = match val(&args, i) {
                        Some(p) => (p, 2),
                        None => ("live.h264".to_string(), 1),
                    };
                    c.probe_live = Some(path);
                    i += step;
                }
                "--whep-selftest" => {
                    c.whep_selftest = true;
                    i += 1;
                }
                "--synthetic-pattern" => {
                    c.synthetic_pattern = true;
                    i += 1;
                }
                "--probe-bitrate" => {
                    c.probe_bitrate = true;
                    i += 1;
                }
                _ => i += 1,
            }
        }

        c
    }
}

pub fn print_help() {
    tprintln!(
        "ultra-low-latency WebRTC screen streamer\n\
\n\
USAGE: untitled17 [OPTIONS]\n\
\n\
SERVER\n\
  --port <n>              HTTP port (default 8080)\n\
  --https-port <n>        HTTPS port (default 8443)\n\
  --bind-ip <ip>          address to bind/listen on (default 0.0.0.0 = all interfaces)\n\
  --lan-ip <ip>           LAN IP to advertise in the cert SAN + URL hint (default auto-detected)\n\
\n\
CAPTURE / ENCODE\n\
  --monitor <n>           0-based display index (default 1, falls back to 0)\n\
  --scale <10..100>       encode resolution as percent of native (default 100)\n\
  --fps <n>               frame-rate override, 15..500 (default = display refresh)\n\
  --max-fps <n>           cap auto-detected fps, 15..500 (default 500)\n\
  --max-bitrate-kbps <n>  override computed CBR target (default auto)\n\
  --h264-profile <p>      baseline | main | high (default baseline)\n\
  --qp <1..51>            constant-QP rate control instead of CBR — kills flicker on\n\
                          static/dark content (lower = higher quality; try 18-23)\n\
  --intra-refresh <on|off>  rolling intra refresh for passive loss recovery (default off;\n\
                          on = periodic refresh waves can be visible as bands repainting\n\
                          the image; off = steady image, recovers via PLI/FIR -> IDR)\n\
  --encoder <v>           hardware encoder: auto | nvidia | intel (default auto =\n\
                          pick by capture adapter; intel = Quick Sync / oneVPL)\n\
  --synthetic-pattern     synthetic pattern instead of live capture\n\
\n\
ICE / TLS\n\
  --stun <a,b,..>         comma-separated STUN urls (default Google STUN)\n\
  --turn-url <url>        static TURN relay url\n\
  --turn-username <u>     static TURN username\n\
  --turn-credential <c>   static TURN credential\n\
  --turn-secret <s>       shared secret for our self-hosted TURN (TURN-SERVER/);\n\
                          enables per-session ephemeral creds (env: SCREENEXTEND_TURN_SECRET)\n\
  --turn-urls <a,b,..>    TURN urls for the self-hosted relay\n\
                          (env: SCREENEXTEND_TURN_URLS; default turn.screenextend.app)\n\
  --turn-ttl <secs>       lifetime of minted TURN credentials (default 600, min 60)\n\
  --tls-cert <path>       PEM certificate (default: cached self-signed)\n\
  --tls-key <path>        PEM private key\n\
\n\
PROBES / SELF-TEST (no browser)\n\
  --probe-capture [path]  one frame -> PNG (default capture_probe.png)\n\
  --probe-dxgi [path]     one frame via DXGI duplication + cursor -> BMP (default dxgi_probe.bmp)\n\
  --probe-encode [path]   300 synthetic frames -> Annex-B (default out.h264)\n\
  --probe-live [path]     150 live frames -> Annex-B (default live.h264)\n\
  --probe-bitrate         exercise adaptive-bitrate reconfigure\n\
  --whep-selftest         in-process WHEP client, assert RTP flows\n\
\n\
  -h, --help              print this help and exit"
    );
}

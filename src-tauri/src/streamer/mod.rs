pub mod audio;
pub mod bitrate;
pub mod cloud;
pub mod config;
pub mod congestion;
pub mod input;
pub mod keyframe;
pub mod lock;
pub mod negotiate;
pub mod pacer;
pub mod packetizer;
pub mod pipeline;
pub mod platform;
pub mod repair;
pub mod server;
pub mod session;
pub mod sockets;
pub mod synthetic;
pub mod telemetry;
pub mod test;
pub mod tls;
pub mod webrtc_session;

pub use config::Config;

use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use tokio::runtime::Runtime;

/// Ceiling on the shared runtime's worker count.
///
/// The transport's work is I/O-bound: capture, encode and audio each own dedicated OS threads,
/// and the display work goes through `spawn_blocking`. What is left on the async side is
/// accepting connections, moving RTP and polling `getStats`. Tokio's default is
/// `available_parallelism()`, which on a 16-thread host meant 16 priority-boosted workers per
/// network adapter — three adapters made 48 of them, nearly all permanently idle, competing
/// with the encoder for the cores it actually needs.
const MAX_TRANSPORT_WORKERS: usize = 4;

static RUNTIME: OnceLock<std::result::Result<Runtime, String>> = OnceLock::new();

fn build_transport_runtime() -> std::io::Result<Runtime> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, MAX_TRANSPORT_WORKERS);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name("se-transport")
        .enable_all()
        .on_thread_start(platform::tune_transport_thread)
        .build()
}

/// The one tokio runtime the host uses for every network adapter.
///
/// Built on first use and kept for the process lifetime — adapter churn (a Wi-Fi roam that
/// changes the IP) used to tear down and rebuild a whole runtime.
pub fn transport_runtime() -> Result<&'static Runtime> {
    match RUNTIME.get_or_init(|| build_transport_runtime().map_err(|e| e.to_string())) {
        Ok(runtime) => Ok(runtime),
        Err(e) => Err(anyhow!("could not build the shared transport runtime: {e}")),
    }
}

pub struct Streamer {
    config: Config,
}

impl Streamer {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn prepare() {
        platform::set_dpi_awareness();
        platform::apply_process_tuning();
    }

    pub fn run(self) -> Result<()> {
        Self::prepare();
        transport_runtime()?.block_on(server::run(self.config, None))
    }

    /// Run this adapter's server to completion on the shared runtime.
    ///
    /// The caller keeps its own thread — `block_on` drives `server::run` there and the tasks it
    /// spawns go to the shared worker pool — so one adapter's accept loop still cannot starve
    /// another's, but the workers are no longer duplicated per adapter.
    pub fn run_with_handle(self, handle: axum_server::Handle) -> Result<()> {
        Self::prepare();
        transport_runtime()?.block_on(server::run(self.config, Some(handle)))
    }

    pub async fn serve(self) -> Result<()> {
        Self::prepare();
        server::run(self.config, None).await
    }

    /// Feature-gated with the module it drives: see `streamer/test/mod.rs`.
    #[cfg(any(test, feature = "probe"))]
    pub fn whep_selftest(self) -> Result<()> {
        Self::prepare();
        transport_runtime()?.block_on(test::whep_selftest::run(self.config))
    }

    pub fn probe_capture(&self, path: &str) -> Result<()> {
        Self::prepare();
        platform::probe_capture(self.config.monitor, path)
    }

    pub fn probe_dxgi(&self, path: &str) -> Result<()> {
        Self::prepare();
        platform::probe_dxgi(self.config.monitor, path)
    }

    pub fn probe_encode(&self, path: &str) -> Result<()> {
        Self::prepare();
        platform::probe_encode(&self.config, path)
    }

    pub fn probe_live(&self, path: &str) -> Result<()> {
        Self::prepare();
        pipeline::probe_live(&self.config, path)
    }

    pub fn probe_bitrate(&self) -> Result<()> {
        Self::prepare();
        pipeline::probe_bitrate(&self.config)
    }
}

pub mod audio;
pub mod bitrate;
pub mod cloud;
pub mod config;
pub mod congestion;
pub mod input;
pub mod keyframe;
pub mod liveness;
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
/// Do not narrow it: tokio has no dedicated I/O thread (a worker services the reactor only when
/// it parks, or every `event_interval` = 61 task polls), so with few workers an arriving pointer
/// datagram waits behind busy tasks. Measured with a 1 kHz pointer burst: 779 of 1000 frames
/// reached `SendInput` at 4 workers, 1000 of 1000 at 16 (forward leg p99 3 ms).
///
/// `route()` stamps `host_recv_ns` after that wait, so a stall here shows in no host-side stage
/// histogram, only as dropped motion on the client.
const MAX_TRANSPORT_WORKERS: usize = 16;

static RUNTIME: OnceLock<std::result::Result<Runtime, String>> = OnceLock::new();

fn build_transport_runtime() -> std::io::Result<Runtime> {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    // SE_TRANSPORT_WORKERS overrides the width for a measurement
    let workers = crate::envcfg::var("SE_TRANSPORT_WORKERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| cores.clamp(1, MAX_TRANSPORT_WORKERS));
    tprintln!("transport runtime: {workers} worker(s) on {cores} logical processors");
    // candidate: tune a worker on its first park, not at start. Only workers park, so
    // blocking-pool threads never register with MMCSS
    #[cfg(windows)]
    if crate::windows_utils::streamer::tune::candidate("transport_mmcss_lazy") {
        return tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("se-transport")
            .enable_all()
            .on_thread_park(crate::windows_utils::streamer::tuning::tune_transport_thread_once)
            .build();
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name("se-transport")
        .enable_all()
        .on_thread_start(platform::tune_transport_thread)
        .build()
}

/// The one tokio runtime the host uses for every network adapter, built on first use and kept
/// for the process lifetime so adapter churn (a Wi-Fi roam) never rebuilds it.
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
    /// The caller keeps its own thread: `block_on` drives `server::run` there and the tasks it
    /// spawns go to the shared worker pool, so one adapter's accept loop cannot starve another's.
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

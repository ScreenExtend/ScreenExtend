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
/// The problem this number was introduced to solve was 48 threads: tokio's default is
/// `available_parallelism()` and a runtime was built **per network adapter**, so three adapters on
/// a 16-thread host meant 48 priority-boosted, mostly idle workers competing with the encoder. One
/// shared runtime fixes that by itself — 16 total is a third of what two adapters used to cost and
/// fewer than one adapter's worth. Capping it at 4 on top of that was a second, separate change,
/// and it is the one that had no measurement behind it.
///
/// It costs input latency because **tokio has no dedicated I/O thread**: a worker services the
/// reactor only when it parks, or every `event_interval` (61) task polls. A session steadily
/// occupies the RTP writer, the RTCP/TWCC reader, the bitrate driver's `getStats`, the audio
/// forwarder at 200 packets/s and the `/reconfig` poll, so with four workers all four can be
/// runnable at once and the arriving SCTP datagram that carries a pointer move waits behind up to
/// 61 task polls. Measured on this box with a 1 kHz pointer burst: at 4 workers 779 of 1000 frames
/// reached `SendInput`; at 16, 1000 of 1000, with the forward leg's p99 at 3 ms.
///
/// The blind spot is worth naming: `route()` stamps `host_recv_ns` *after* that wait, so a stall
/// here leaves every host-side stage histogram clean and shows up only as dropped motion the client
/// can see. `SE_TRANSPORT_WORKERS` overrides the width for a measurement.
const MAX_TRANSPORT_WORKERS: usize = 16;

static RUNTIME: OnceLock<std::result::Result<Runtime, String>> = OnceLock::new();

fn build_transport_runtime() -> std::io::Result<Runtime> {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    // `SE_TRANSPORT_WORKERS` overrides the width for a measurement. The worker count is the one
    // transport constant whose cost cannot be reasoned about from the code — tokio has no
    // dedicated I/O thread, so how soon an arriving datagram is noticed depends on whether any
    // worker is parked, which depends on how many there are — and re-measuring it used to mean a
    // rebuild between every pair of numbers.
    let workers = std::env::var("SE_TRANSPORT_WORKERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| cores.clamp(1, MAX_TRANSPORT_WORKERS));
    tprintln!("transport runtime: {workers} worker(s) on {cores} logical processors");
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

pub mod bitrate;
pub mod cli;
pub mod config;
pub mod pipeline;
pub mod session;
pub mod platform;
pub mod server;
pub mod test;
pub mod tls;
pub mod webrtc_session;

pub use config::Config;

use anyhow::Result;

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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(server::run(self.config, None))
    }

    pub fn run_with_handle(self, handle: axum_server::Handle) -> Result<()> {
        Self::prepare();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(server::run(self.config, Some(handle)))
    }

    pub async fn serve(self) -> Result<()> {
        Self::prepare();
        server::run(self.config, None).await
    }

    pub fn whep_selftest(self) -> Result<()> {
        Self::prepare();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(test::whep_selftest::run(self.config))
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

use anyhow::Result;

use super::Streamer;
use super::config::{Config, print_help};

pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    let streamer = Streamer::new(Config::from_args());

    if let Some(path) = streamer.config().probe_capture.clone() {
        return streamer.probe_capture(&path);
    }
    if let Some(path) = streamer.config().probe_encode.clone() {
        return streamer.probe_encode(&path);
    }
    if let Some(path) = streamer.config().probe_live.clone() {
        return streamer.probe_live(&path);
    }
    if streamer.config().probe_bitrate {
        return streamer.probe_bitrate();
    }
    if streamer.config().whep_selftest {
        return streamer.whep_selftest();
    }

    streamer.run()
}

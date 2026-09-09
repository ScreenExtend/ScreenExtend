// `skip` and `show_measurements` live in `crate::test_support` so the macOS and cross-platform
// suites use the same convention; these two re-exports keep the `super::skip(...)` call sites
// in this directory reading the way they always did.
pub(crate) use crate::test_support::{show_measurements, skip};

mod amd_layout;
mod capture_latency;
// Visible to the rest of `streamer`: `amd::sps`'s tests read their own output back with a
// parser written separately from the writer, which is the only way that assertion means
// anything.
pub(super) mod h264;
mod intel_encoder;
mod intel_layout;
mod nvenc_encoder;
mod nvenc_layout;
mod pipeline;
mod scaler;
mod vendor;
mod x264_encoder;
mod x264_layout;

#[test]
#[ignore = "requires an interactive desktop; run with --ignored"]
fn dxgi_duplication_probe() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/target/dxgi_probe.bmp");
    super::dxgi::probe_to_bmp(0, path).expect("dxgi duplication probe");
}

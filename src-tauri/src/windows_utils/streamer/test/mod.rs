// re-exported for the `super::skip(...)` call sites in this directory
pub(crate) use crate::test_support::{show_measurements, skip};

mod amd_encoder;
mod amd_fake;
mod amd_layout;
mod capture_latency;
mod frame_gate;
// pub(super): `amd::sps`'s tests read their output back with this parser, which is written
// separately from the writer
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

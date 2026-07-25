mod intel_encoder;
mod intel_layout;
mod nvenc_layout;
mod pipeline;
mod x264_encoder;

#[test]
#[ignore = "requires an interactive desktop; run with --ignored"]
fn dxgi_duplication_probe() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/target/dxgi_probe.bmp");
    super::dxgi::probe_to_bmp(0, path).expect("dxgi duplication probe");
}

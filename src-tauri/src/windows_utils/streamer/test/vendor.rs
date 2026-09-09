//! Adapter identification and the encoder fallback ladder.
//!
//! Both are pure decisions that decide which encoder a machine gets, and both were untested:
//! the ladder is the whole Windows compatibility story and nothing exercised it, while
//! `select_vendor` is string matching over an adapter description that no test had ever fed a
//! real description to. Neither of these needs a GPU, so they run everywhere.

use crate::streamer::config::EncoderVendor;
use crate::windows_utils::streamer::pipeline::{encoder_ladder, BackendKind};
use crate::windows_utils::streamer::{
    select_vendor, Vendor, AMD_PCI_VENDOR_ID, INTEL_PCI_VENDOR_ID, NVIDIA_PCI_VENDOR_ID,
};

#[test]
fn a_pci_id_beats_a_description_because_descriptions_are_rebranded() {
    assert_eq!(Vendor::from_pci_id(NVIDIA_PCI_VENDOR_ID), Vendor::Nvidia);
    assert_eq!(Vendor::from_pci_id(INTEL_PCI_VENDOR_ID), Vendor::Intel);
    assert_eq!(Vendor::from_pci_id(AMD_PCI_VENDOR_ID), Vendor::Amd);
    // Microsoft's id, which WARP reports. It must not resolve to a hardware vendor.
    assert_eq!(Vendor::from_pci_id(0x1414), Vendor::Unknown);
    assert_eq!(Vendor::from_pci_id(0), Vendor::Unknown);
    // AMD's *CPU* vendor id. It never appears on a display adapter and adding it would make
    // every Ryzen box claim a Radeon.
    assert_eq!(Vendor::from_pci_id(0x1022), Vendor::Unknown);
}

#[test]
fn real_adapter_descriptions_resolve_to_their_vendor() {
    // Strings as `IDXGIAdapter::GetDesc` reports them on machines this project has seen or
    // expects: a discrete NVIDIA, an Intel iGPU across two generations of naming, and AMD under
    // all three names it ships under.
    for (description, want) in [
        ("NVIDIA GeForce RTX 3080 Ti Laptop GPU", Vendor::Nvidia),
        ("NVIDIA GeForce GTX 1650", Vendor::Nvidia),
        ("Intel(R) UHD Graphics", Vendor::Intel),
        ("Intel(R) Iris(R) Xe Graphics", Vendor::Intel),
        ("Intel(R) Arc(TM) A770 Graphics", Vendor::Intel),
        ("AMD Radeon RX 7900 XTX", Vendor::Amd),
        ("Radeon (TM) RX 480 Graphics", Vendor::Amd),
        ("Advanced Micro Devices, Inc. [AMD/ATI]", Vendor::Amd),
        ("Microsoft Basic Render Driver", Vendor::Unknown),
        ("", Vendor::Unknown),
    ] {
        assert_eq!(
            select_vendor(description),
            want,
            "{description:?} should resolve to {want:?}"
        );
    }
}

#[test]
fn vendor_matching_is_case_insensitive() {
    assert_eq!(select_vendor("nvidia geforce"), Vendor::Nvidia);
    assert_eq!(select_vendor("radeon"), Vendor::Amd);
    assert_eq!(select_vendor("InTeL(R) Uhd"), Vendor::Intel);
}

#[test]
fn every_ladder_ends_in_software_so_a_session_is_always_possible() {
    for vendor in [
        EncoderVendor::Auto,
        EncoderVendor::Nvidia,
        EncoderVendor::Intel,
        EncoderVendor::Amd,
        EncoderVendor::Software,
    ] {
        let ladder = encoder_ladder(vendor);
        assert!(!ladder.is_empty(), "{vendor:?} has an empty ladder");
        assert_eq!(
            ladder.last(),
            Some(&BackendKind::X264),
            "{vendor:?} must fall back to x264 rather than fail the join"
        );
    }
}

#[test]
fn auto_tries_nvenc_then_quick_sync_then_amf_then_software() {
    assert_eq!(
        encoder_ladder(EncoderVendor::Auto),
        &[
            BackendKind::Nvenc,
            BackendKind::Intel,
            BackendKind::Amf,
            BackendKind::X264
        ]
    );
}

#[test]
fn an_explicit_vendor_tries_that_vendor_and_then_software_only() {
    // Asking for one vendor must not silently hand the session to a different one: the fallback
    // is x264, never the other GPU. A machine set to Intel that quietly encoded on NVENC would
    // make every "is Quick Sync working" answer a lie.
    assert_eq!(
        encoder_ladder(EncoderVendor::Nvidia),
        &[BackendKind::Nvenc, BackendKind::X264]
    );
    assert_eq!(
        encoder_ladder(EncoderVendor::Intel),
        &[BackendKind::Intel, BackendKind::X264]
    );
    assert_eq!(
        encoder_ladder(EncoderVendor::Amd),
        &[BackendKind::Amf, BackendKind::X264]
    );
}

#[test]
fn software_never_touches_a_hardware_encoder() {
    // `disableGpuEncode` in the UI is exactly this ladder. If it probed a GPU encoder first the
    // setting would not do what its name says on a machine whose driver is the problem.
    assert_eq!(
        encoder_ladder(EncoderVendor::Software),
        &[BackendKind::X264]
    );
}

#[test]
fn no_ladder_tries_the_same_backend_twice() {
    for vendor in [
        EncoderVendor::Auto,
        EncoderVendor::Nvidia,
        EncoderVendor::Intel,
        EncoderVendor::Amd,
        EncoderVendor::Software,
    ] {
        let ladder = encoder_ladder(vendor);
        for (i, kind) in ladder.iter().enumerate() {
            assert!(
                !ladder[i + 1..].contains(kind),
                "{vendor:?} tries {kind:?} more than once: {ladder:?}"
            );
        }
    }
}

#[test]
fn the_capture_gpu_preference_matches_the_first_rung_of_each_ladder() {
    use crate::windows_capture::d3d11::GpuPreference;
    use crate::windows_utils::streamer::capture_gpu_preference;

    // The capture device must land on the adapter the encoder will run on, or every frame pays a
    // GPU->CPU->GPU round trip. That makes this a property of the ladder, not an independent
    // choice, and the two drifting apart is silent.
    assert_eq!(
        capture_gpu_preference(EncoderVendor::Nvidia),
        GpuPreference::Vendor(NVIDIA_PCI_VENDOR_ID)
    );
    assert_eq!(
        capture_gpu_preference(EncoderVendor::Intel),
        GpuPreference::Vendor(INTEL_PCI_VENDOR_ID)
    );
    assert_eq!(
        capture_gpu_preference(EncoderVendor::Amd),
        GpuPreference::Vendor(AMD_PCI_VENDOR_ID)
    );
    // Auto leads with NVENC, which lives on the discrete card.
    assert_eq!(
        capture_gpu_preference(EncoderVendor::Auto),
        GpuPreference::HighPerformance
    );
    // x264 reads every frame back to the CPU, so the iGPU that already composes the desktop is
    // the cheap place to read from.
    assert_eq!(
        capture_gpu_preference(EncoderVendor::Software),
        GpuPreference::MinimumPower
    );
}

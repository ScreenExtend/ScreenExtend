pub mod capture;
pub mod clock;
pub mod dxgi;
pub mod pipeline;
pub mod scaler;
pub mod tuning;

pub mod amd;
pub mod amd_sys;
pub mod intel;
pub mod nvidia;
pub mod x264;

#[cfg(test)]
mod test;

/// PCI vendor ids, which is what every real implementation matches on. Description strings are
/// localized, OEM-rebranded and change across driver releases, and WARP's contains none of the
/// vendor names while reporting Microsoft's `0x1414`.
///
/// `0x1022` is AMD's **CPU** vendor id and never appears on a display adapter — do not add it.
pub const NVIDIA_PCI_VENDOR_ID: u32 = 0x10DE;
pub const INTEL_PCI_VENDOR_ID: u32 = 0x8086;
pub const AMD_PCI_VENDOR_ID: u32 = 0x1002;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Unknown,
}

impl Vendor {
    pub fn from_pci_id(id: u32) -> Vendor {
        match id {
            NVIDIA_PCI_VENDOR_ID => Vendor::Nvidia,
            INTEL_PCI_VENDOR_ID => Vendor::Intel,
            AMD_PCI_VENDOR_ID => Vendor::Amd,
            _ => Vendor::Unknown,
        }
    }
}

/// Fallback for the one place we only have a description string.
pub fn select_vendor(adapter_description: &str) -> Vendor {
    let d = adapter_description.to_uppercase();
    if d.contains("NVIDIA") {
        Vendor::Nvidia
    } else if d.contains("INTEL") {
        Vendor::Intel
    } else if d.contains("AMD") || d.contains("RADEON") || d.contains("ADVANCED MICRO DEVICES") {
        Vendor::Amd
    } else {
        Vendor::Unknown
    }
}

/// Which adapter the capture device should be created on, given the encoder we intend to use.
///
/// The point is not raw GPU power — it is that the capture device and the encoder device end up
/// on the *same* adapter. The encoder input is a `SHARED_NTHANDLE | SHARED_KEYEDMUTEX` texture
/// opened on the capture device, and that mechanism is same-adapter only; cross-adapter sharing
/// needs `SHARED_CROSS_ADAPTER`, which cannot be an encoder input. When the two adapters differ,
/// every frame falls back to a GPU→CPU→GPU round trip. WGC can capture any monitor from a device
/// on any adapter, so this choice is free.
#[cfg(target_os = "windows")]
pub fn capture_gpu_preference(
    vendor: crate::streamer::config::EncoderVendor,
) -> crate::windows_capture::d3d11::GpuPreference {
    use crate::streamer::config::EncoderVendor;
    use crate::windows_capture::d3d11::GpuPreference;

    match vendor {
        EncoderVendor::Nvidia => GpuPreference::Vendor(NVIDIA_PCI_VENDOR_ID),
        EncoderVendor::Intel => GpuPreference::Vendor(INTEL_PCI_VENDOR_ID),
        EncoderVendor::Amd => GpuPreference::Vendor(AMD_PCI_VENDOR_ID),
        // The `Auto` ladder is NVENC -> Quick Sync -> AMF, so the discrete GPU is where the first
        // two candidates live.
        EncoderVendor::Auto => GpuPreference::HighPerformance,
        // x264 reads every frame back to the CPU. Reading back from the iGPU that composes the
        // desktop is far cheaper than dragging it across PCIe from a discrete card.
        EncoderVendor::Software => GpuPreference::MinimumPower,
    }
}

/// Says, once per session, which GPU captures and which encodes, and whether they match.
///
/// A mismatch is not an error — it is the difference between a shared-texture handoff and a full
/// readback every frame, and it was previously only inferable from a "zero-copy path unavailable"
/// message that names no adapter.
#[cfg(target_os = "windows")]
pub fn report_adapter_pairing(
    capture: &crate::windows_capture::d3d11::AdapterInfo,
    encoder_device: Option<&windows::Win32::Graphics::Direct3D11::ID3D11Device>,
) {
    use crate::windows_capture::d3d11::device_adapter_info;

    if capture.software {
        teprintln!(
            "adapters: capture is running on a SOFTWARE rasteriser -- {}. Every frame of this \
             display is being composed on the CPU; expect very high latency and CPU use",
            capture.label()
        );
    }
    match encoder_device.and_then(device_adapter_info) {
        Some(enc) => tprintln!(
            "adapters: capture={} encode={} same_adapter={}",
            capture.label(),
            enc.label(),
            enc.luid == capture.luid
        ),
        None => tprintln!(
            "adapters: capture={} encode=cpu (software encoder)",
            capture.label()
        ),
    }
}

#[cfg(target_os = "windows")]
pub fn device_vendor(device: &windows::Win32::Graphics::Direct3D11::ID3D11Device) -> Vendor {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;

    let describe = || -> Option<(String, u32)> {
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
        let desc = unsafe { adapter.GetDesc() }.ok()?;
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        Some((
            String::from_utf16_lossy(&desc.Description[..end]),
            desc.VendorId,
        ))
    };
    match describe() {
        Some((name, vendor_id)) => {
            // Prefer the id; the description is only a fallback for adapters that report no id.
            let vendor = match Vendor::from_pci_id(vendor_id) {
                Vendor::Unknown => select_vendor(&name),
                v => v,
            };
            tprintln!(
                "capture device adapter: name={name}, vendor_id=0x{vendor_id:04X}, vendor={vendor:?}"
            );
            vendor
        }
        None => Vendor::Unknown,
    }
}

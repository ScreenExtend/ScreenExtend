pub mod capture;
pub mod clock;
pub mod cursor_overlay;
pub mod dxgi;
pub mod governor;
pub mod keyed;
pub mod pipeline;
pub mod rtx;
pub mod scaler;
pub mod slow_link;
pub mod tune;
pub mod tuning;
pub mod workarea;
pub mod workload_window;

pub mod amd;
pub mod amd_sys;
pub mod intel;
pub mod nvidia;
pub mod x264;

#[cfg(test)]
mod test;

/// PCI vendor ids. Description strings are localized, OEM-rebranded and change across driver
/// releases, and WARP's names no vendor while reporting Microsoft's `0x1414`.
///
/// `0x1022` is AMD's **CPU** vendor id and never appears on a display adapter: do not add it.
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

/// Fallback for an adapter known only by its description string.
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

/// Which adapter the capture device should be created on for this encoder.
///
/// Capture and encode must share an adapter: the encoder input is a
/// `SHARED_NTHANDLE | SHARED_KEYEDMUTEX` texture opened on the capture device, which is
/// same-adapter only (`SHARED_CROSS_ADAPTER` cannot be an encoder input). A mismatch costs a
/// GPU-CPU-GPU round trip per frame. WGC captures any monitor from a device on any adapter, so
/// the choice is free.
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
        // the `Auto` ladder is NVENC -> Quick Sync -> AMF: its first candidates are on the
        // discrete GPU
        EncoderVendor::Auto => GpuPreference::HighPerformance,
        // x264 reads every frame back to the CPU: cheaper from the iGPU that composes the
        // desktop than across PCIe from a discrete card
        EncoderVendor::Software => GpuPreference::MinimumPower,
    }
}

/// Logs, once per session, which GPU captures and which encodes, and whether they match.
///
/// A mismatch is not an error: it means a full readback every frame instead of a
/// shared-texture handoff.
///
/// `monitor` is the captured monitor's `HMONITOR`. When it is one of this host's virtual displays
/// and the driver reports diagnostics, the line also names the adapter Windows composes it on,
/// which decides whether capture has to cross adapters at all.
#[cfg(target_os = "windows")]
pub fn report_adapter_pairing(
    capture: &crate::windows_capture::d3d11::AdapterInfo,
    encoder_device: Option<&windows::Win32::Graphics::Direct3D11::ID3D11Device>,
    monitor: isize,
) {
    use crate::windows_capture::d3d11::device_adapter_info;

    let lookup = crate::windows_utils::virtual_display::composing_adapter_of(monitor);
    // one of this host's displays: the display backend is told which encoder really started
    let ours = lookup.is_some();
    let composed = match lookup {
        Some(Ok(adapter)) => format!(" composed={adapter}"),
        Some(Err(why)) => format!(" composed=unknown ({why})"),
        None => String::new(),
    };

    if capture.software {
        teprintln!(
            "adapters: capture is running on a SOFTWARE rasteriser -- {}. Every frame of this \
             display is being composed on the CPU; expect very high latency and CPU use",
            capture.label()
        );
    }
    let encoder = encoder_device.and_then(device_adapter_info);
    if ours {
        crate::windows_utils::virtual_display::encoder_started(encoder.as_ref().map(|e| e.luid));
    }
    match encoder {
        Some(enc) => {
            tprintln!(
                "adapters: capture={} encode={} same_adapter={}{composed}",
                capture.label(),
                enc.label(),
                enc.luid == capture.luid
            );
            if enc.luid != capture.luid {
                // Desktop Duplication and WGC are adapter-local, so capture runs where the OS
                // composes the display. Before IddCx 1.4 that is the POST adapter (the iGPU on a
                // hybrid laptop); `IddCxAdapterSetRenderAdapter` needs 1.4 and the bundled driver
                // declares 1.2. Cost: a full-frame PCIe copy per frame.
                tprintln!(
                    "adapters: every frame crosses adapters; if this is a ScreenExtend virtual display, the bundled driver declares IddCx 1.2 and cannot ask the OS to compose on the encoder's GPU"
                );
            }
        }
        None => tprintln!(
            "adapters: capture={} encode=cpu (software encoder){composed}",
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
            // the description is only a fallback for an id that names no known vendor
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

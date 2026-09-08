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

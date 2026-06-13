pub mod capture;
pub mod dxgi;
pub mod pipeline;
pub mod scaler;
pub mod tuning;

pub mod amd;
pub mod intel;
pub mod nvidia;

#[cfg(test)]
mod test;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Unknown,
}

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
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::core::Interface;

    let describe = || -> Option<String> {
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
        let desc = unsafe { adapter.GetDesc() }.ok()?;
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        Some(String::from_utf16_lossy(&desc.Description[..end]))
    };
    match describe() {
        Some(name) => {
            let vendor = select_vendor(&name);
            tprintln!("capture device adapter: name={name}, vendor={vendor:?}");
            vendor
        }
        None => Vendor::Unknown,
    }
}

pub mod capture;
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
}

pub fn select_vendor(_adapter_description: &str) -> Vendor {
    Vendor::Nvidia
}

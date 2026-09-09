use super::protocol::InputEvent;
use super::ClientProfile;

pub const NAME: &str = "generic-noop";

pub fn probe() -> super::InputBackend {
    super::InputBackend::Unsupported
}

pub fn boost_thread() {}

pub struct Injector;

impl Injector {
    pub fn new(_client: ClientProfile) -> Self {
        Injector
    }
    pub fn dispatch(&mut self, _ev: &InputEvent) {}
    pub fn release_all(&mut self) {}
}

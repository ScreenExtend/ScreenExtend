use super::protocol::InputEvent;

pub const NAME: &str = "generic-noop";

pub fn boost_thread() {}
pub fn tune_process() {}

pub struct Injector;

impl Injector {
    pub fn new(_device_name: Option<String>) -> Self {
        Injector
    }
    pub fn dispatch(&mut self, _ev: &InputEvent) {}
    pub fn release_all(&mut self) {}
}

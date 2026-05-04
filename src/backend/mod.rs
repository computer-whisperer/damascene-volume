use crate::model::AudioSnapshot;

pub mod pipewire_native;

pub trait AudioBackend {
    fn refresh(&mut self) -> AudioSnapshot;
}

#[derive(Default)]
#[allow(dead_code)]
pub struct DemoBackend;

impl AudioBackend for DemoBackend {
    fn refresh(&mut self) -> AudioSnapshot {
        AudioSnapshot::demo()
    }
}

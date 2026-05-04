use anyhow::Result;
use pipewire as pw;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Once,
};

use crate::backend::AudioBackend;
use crate::model::{AudioCard, AudioClass, AudioNode, AudioSnapshot, Direction};

/// Native PipeWire backend.
///
/// The first implementation milestone is read-only graph inventory:
/// registry globals, node/device/card classification, and metadata
/// discovery. Mutating operations should only land once this layer can
/// reliably name the affected PipeWire object.
#[derive(Default)]
pub struct PipeWireBackend;

impl PipeWireBackend {
    pub fn new() -> Self {
        Self
    }

    pub fn snapshot(&mut self) -> Result<AudioSnapshot> {
        pipewire_init();

        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry()?;

        let snapshot = Rc::new(RefCell::new(AudioSnapshot {
            server_name: Some("PipeWire".into()),
            ..AudioSnapshot::default()
        }));

        let pending = core.sync(0)?;
        let done = Rc::new(Cell::new(false));
        let done_for_listener = done.clone();
        let loop_for_listener = mainloop.clone();
        let _core_listener = core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done_for_listener.set(true);
                    loop_for_listener.quit();
                }
            })
            .register();

        let snapshot_for_registry = snapshot.clone();
        let _registry_listener = registry
            .add_listener_local()
            .global(move |global| {
                let mut snapshot = snapshot_for_registry.borrow_mut();
                if let Some(node) = audio_node_from_global(global) {
                    snapshot.nodes.push(node);
                } else if let Some(card) = audio_card_from_global(global) {
                    snapshot.cards.push(card);
                }
            })
            .register();

        while !done.get() {
            mainloop.run();
        }

        Ok(snapshot.borrow().clone())
    }
}

impl AudioBackend for PipeWireBackend {
    fn refresh(&mut self) -> AudioSnapshot {
        match self.snapshot() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                let mut snapshot = AudioSnapshot::demo();
                snapshot.error = Some(err.to_string());
                snapshot
            }
        }
    }
}

fn pipewire_init() {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
}

fn audio_node_from_global<P>(global: &pw::registry::GlobalObject<P>) -> Option<AudioNode>
where
    P: AsRef<pw::spa::utils::dict::DictRef>,
{
    if global.type_ != pw::types::ObjectType::Node {
        return None;
    }
    let props = global.props.as_ref()?.as_ref();
    if is_internal_aetna_node(props) {
        return None;
    }
    let media_class = prop(props, "media.class")?;
    let class = match media_class {
        "Audio/Sink" => AudioClass::Device {
            direction: Direction::Output,
        },
        "Audio/Source" => AudioClass::Device {
            direction: Direction::Input,
        },
        "Stream/Output/Audio" => AudioClass::Stream {
            direction: Direction::Output,
        },
        "Stream/Input/Audio" => AudioClass::Stream {
            direction: Direction::Input,
        },
        other => AudioClass::Other(other.to_string()),
    };

    if matches!(class, AudioClass::Other(_)) {
        return None;
    }

    let name = prop(props, "node.name").unwrap_or("unnamed").to_string();
    let description = prop(props, "node.description")
        .or_else(|| prop(props, "node.nick"))
        .or_else(|| prop(props, "application.name"))
        .or_else(|| prop(props, "media.name"))
        .unwrap_or(&name)
        .to_string();

    Some(AudioNode {
        id: global.id,
        class,
        name,
        description,
        application: prop(props, "application.name").map(str::to_string),
        media_name: prop(props, "media.name").map(str::to_string),
        target: prop(props, "target.object")
            .or_else(|| prop(props, "node.target"))
            .map(str::to_string),
        volume: None,
        is_default: false,
    })
}

fn audio_card_from_global<P>(global: &pw::registry::GlobalObject<P>) -> Option<AudioCard>
where
    P: AsRef<pw::spa::utils::dict::DictRef>,
{
    if global.type_ != pw::types::ObjectType::Device {
        return None;
    }
    let props = global.props.as_ref()?.as_ref();
    let media_class = prop(props, "media.class").unwrap_or_default();
    if media_class != "Audio/Device" {
        return None;
    }

    let name = prop(props, "device.name").unwrap_or("unnamed").to_string();
    let description = prop(props, "device.description")
        .or_else(|| prop(props, "device.nick"))
        .unwrap_or(&name)
        .to_string();

    Some(AudioCard {
        id: global.id,
        name,
        description,
        active_profile: None,
        profiles: Vec::new(),
    })
}

fn prop<'a>(props: &'a pw::spa::utils::dict::DictRef, key: &str) -> Option<&'a str> {
    props
        .iter()
        .find_map(|(k, v)| if k == key { Some(v) } else { None })
}

fn is_internal_aetna_node(props: &pw::spa::utils::dict::DictRef) -> bool {
    prop(props, "node.name")
        .map(|name| name.starts_with("aetna-volume.meter."))
        .unwrap_or(false)
        || prop(props, "application.name")
            .map(|name| name == "aetna-volume")
            .unwrap_or(false)
}

use anyhow::Result;
use pipewire as pw;
use pw::spa::pod::{Object, Property, Value, ValueArray};
use pw::spa::{param::ParamType, utils::SpaTypes};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Condvar, Mutex, Once},
    thread,
    time::Duration,
};

use crate::backend::AudioBackend;
use crate::model::{
    AudioCard, AudioClass, AudioNode, AudioProfile, AudioSnapshot, Direction, ProfileAvailability,
    Volume,
};

/// Commands sent from the main thread to the PipeWire backend thread
/// over [`pw::channel`] (loop-integrated, fires the receiver callback
/// the next time the mainloop wakes).
enum BackendCommand {
    SetMute { node_id: u32, muted: bool },
    SetVolume { node_id: u32, scalar: f32 },
    SetDefaultSink { node_name: String },
    SetDefaultSource { node_name: String },
    SetCardProfile { card_id: u32, profile_index: u32 },
    Quit,
}

/// Native PipeWire backend.
///
/// Holds a long-lived registry connection on a dedicated thread and
/// publishes every `global` / `global_remove` event into a shared
/// `AudioSnapshot`. The main thread reads via [`refresh`], which is just
/// a mutex-guarded clone — there is no per-call PipeWire round-trip.
pub struct PipeWireBackend {
    snapshot: Arc<Mutex<AudioSnapshot>>,
    commands: pw::channel::Sender<BackendCommand>,
    _thread: thread::JoinHandle<()>,
}

impl PipeWireBackend {
    pub fn new() -> Self {
        let snapshot = Arc::new(Mutex::new(AudioSnapshot {
            server_name: Some("PipeWire".into()),
            ..AudioSnapshot::default()
        }));
        let ready = Arc::new((Mutex::new(false), Condvar::new()));
        let (commands_tx, commands_rx) = pw::channel::channel::<BackendCommand>();

        let snapshot_for_thread = snapshot.clone();
        let ready_for_thread = ready.clone();
        let thread = thread::Builder::new()
            .name("aetna-volume-pipewire".into())
            .spawn(move || {
                if let Err(err) =
                    run_backend_loop(snapshot_for_thread.clone(), commands_rx, &ready_for_thread)
                {
                    eprintln!("aetna-volume: PipeWire backend stopped: {err}");
                    if let Ok(mut snap) = snapshot_for_thread.lock() {
                        snap.error = Some(err.to_string());
                    }
                    signal_ready(&ready_for_thread);
                }
            })
            .expect("spawn PipeWire backend thread");

        // Block briefly for the initial registry walk so the first
        // frame after construction renders against a populated graph
        // rather than an empty placeholder. If PipeWire is hung or
        // unreachable we time out and let the UI render whatever
        // partial state arrived.
        wait_for_ready(&ready, Duration::from_millis(500));

        Self {
            snapshot,
            commands: commands_tx,
            _thread: thread,
        }
    }
}

impl Default for PipeWireBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PipeWireBackend {
    fn drop(&mut self) {
        // Best-effort: ask the backend thread to stop so the OS doesn't
        // have to reap a still-running mainloop on process exit. If the
        // send fails the thread is already gone.
        let _ = self.commands.send(BackendCommand::Quit);
    }
}

impl AudioBackend for PipeWireBackend {
    fn refresh(&self) -> AudioSnapshot {
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn set_mute(&self, node_id: u32, muted: bool) {
        let _ = self
            .commands
            .send(BackendCommand::SetMute { node_id, muted });
    }

    fn set_volume(&self, node_id: u32, scalar: f32) {
        let _ = self
            .commands
            .send(BackendCommand::SetVolume { node_id, scalar });
    }

    fn set_default_sink(&self, node_name: &str) {
        let _ = self.commands.send(BackendCommand::SetDefaultSink {
            node_name: node_name.to_string(),
        });
    }

    fn set_default_source(&self, node_name: &str) {
        let _ = self.commands.send(BackendCommand::SetDefaultSource {
            node_name: node_name.to_string(),
        });
    }

    fn set_card_profile(&self, card_id: u32, profile_index: u32) {
        let _ = self.commands.send(BackendCommand::SetCardProfile {
            card_id,
            profile_index,
        });
    }
}

fn run_backend_loop(
    snapshot: Arc<Mutex<AudioSnapshot>>,
    commands_rx: pw::channel::Receiver<BackendCommand>,
    ready: &Arc<(Mutex<bool>, Condvar)>,
) -> Result<()> {
    pipewire_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    // Per-node Node proxies (and their props listeners), populated as
    // we see globals and dropped as they go away. The proxy is used by
    // the command receiver to issue `set_param` calls for mute /
    // volume; the listener funnels Props parameter changes back into
    // the snapshot so the UI reflects real PipeWire state.
    let proxies: Rc<RefCell<HashMap<u32, NodeEntry>>> = Rc::new(RefCell::new(HashMap::new()));

    // Per-node channel count, learned from `channelVolumes` arrays in
    // the Props events. Required when writing volume so we can send a
    // correctly-sized `channelVolumes` array — the canonical prop that
    // pavucontrol and other PipeWire-aware tools also use. Without
    // this, writing master `volume` instead would stack
    // multiplicatively against any channel-level setting that already
    // exists, producing surprising audible behavior.
    let channels: Rc<RefCell<HashMap<u32, usize>>> = Rc::new(RefCell::new(HashMap::new()));

    // Holds the bound `default` metadata proxy + its listener. Used to
    // write `default.configured.audio.sink`/`…source` (Set Default
    // action) and to receive change notifications on the active
    // `default.audio.sink`/`…source` keys so the snapshot tracks
    // whatever the system considers default.
    let default_metadata: Rc<RefCell<Option<DefaultMetaEntry>>> = Rc::new(RefCell::new(None));

    // Per-card Device proxies + their param listeners. Used to read
    // active profile + enumerate available profiles, and to call
    // `set_param(ParamType::Profile, ...)` when the user picks one.
    let devices: Rc<RefCell<HashMap<u32, DeviceEntry>>> = Rc::new(RefCell::new(HashMap::new()));

    let snapshot_for_global = snapshot.clone();
    let snapshot_for_remove = snapshot.clone();
    let proxies_for_global = proxies.clone();
    let proxies_for_remove = proxies.clone();
    let channels_for_global = channels.clone();
    let channels_for_remove = channels.clone();
    let default_for_global = default_metadata.clone();
    let default_for_remove = default_metadata.clone();
    let devices_for_global = devices.clone();
    let devices_for_remove = devices.clone();
    let registry_for_bind = registry.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let is_card = if let Ok(mut snap) = snapshot_for_global.lock() {
                if let Some(node) = audio_node_from_global(global) {
                    if !snap.nodes.iter().any(|existing| existing.id == node.id) {
                        snap.nodes.push(node);
                    }
                    false
                } else if let Some(card) = audio_card_from_global(global) {
                    if !snap.cards.iter().any(|existing| existing.id == card.id) {
                        snap.cards.push(card);
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            };

            if is_card {
                let card_id = global.id;
                let device = match registry_for_bind.bind::<pw::device::Device, _>(global) {
                    Ok(device) => device,
                    Err(err) => {
                        eprintln!("aetna-volume: failed to bind device {card_id}: {err}");
                        return;
                    }
                };
                let snapshot_for_param = snapshot_for_global.clone();
                let listener = device
                    .add_listener_local()
                    .param(move |_seq, id, _index, _next, param| {
                        let Some(param) = param else {
                            return;
                        };
                        let bytes = param.as_bytes();
                        if id == ParamType::Profile
                            && let Some(active) = decode_active_profile_index(bytes)
                            && let Ok(mut snap) = snapshot_for_param.lock()
                            && let Some(card) = snap.cards.iter_mut().find(|c| c.id == card_id)
                        {
                            card.active_profile = Some(active);
                        } else if id == ParamType::EnumProfile
                            && let Some(profile) = decode_enum_profile(bytes)
                            && let Ok(mut snap) = snapshot_for_param.lock()
                            && let Some(card) = snap.cards.iter_mut().find(|c| c.id == card_id)
                        {
                            if let Some(slot) =
                                card.profiles.iter_mut().find(|p| p.index == profile.index)
                            {
                                *slot = profile;
                            } else {
                                card.profiles.push(profile);
                            }
                        }
                    })
                    .register();
                device.subscribe_params(&[ParamType::Profile, ParamType::EnumProfile]);
                // Trigger an initial enumeration of available profiles.
                // Subscribe alone doesn't fire until the value changes;
                // enum_params asks PipeWire to emit the current set
                // immediately.
                device.enum_params(0, Some(ParamType::EnumProfile), 0, u32::MAX);
                device.enum_params(0, Some(ParamType::Profile), 0, u32::MAX);
                devices_for_global.borrow_mut().insert(
                    card_id,
                    DeviceEntry {
                        proxy: device,
                        _listener: listener,
                    },
                );
            }

            if global.type_ == pw::types::ObjectType::Metadata
                && default_for_global.borrow().is_none()
            {
                let is_default_meta = global
                    .props
                    .as_ref()
                    .and_then(|props| prop(props, "metadata.name"))
                    .map(|name| name == "default")
                    .unwrap_or(false);
                if is_default_meta {
                    let global_id = global.id;
                    match registry_for_bind.bind::<pw::metadata::Metadata, _>(global) {
                        Ok(meta) => {
                            let snapshot_for_meta = snapshot_for_global.clone();
                            let listener = meta
                                .add_listener_local()
                                .property(move |_subject, key, _type_, value| {
                                    let Some(key) = key else {
                                        // null key = all properties
                                        // cleared. Drop both defaults.
                                        if let Ok(mut snap) = snapshot_for_meta.lock() {
                                            snap.default_sink_name = None;
                                            snap.default_source_name = None;
                                        }
                                        return 0;
                                    };
                                    let target = match key {
                                        "default.audio.sink" => Some(true),
                                        "default.audio.source" => Some(false),
                                        _ => None,
                                    };
                                    let Some(is_sink) = target else {
                                        return 0;
                                    };
                                    let name = value.and_then(default_name_from_json);
                                    if let Ok(mut snap) = snapshot_for_meta.lock() {
                                        if is_sink {
                                            snap.default_sink_name = name;
                                        } else {
                                            snap.default_source_name = name;
                                        }
                                    }
                                    0
                                })
                                .register();
                            *default_for_global.borrow_mut() = Some(DefaultMetaEntry {
                                proxy: meta,
                                _listener: listener,
                                global_id,
                            });
                        }
                        Err(err) => {
                            eprintln!("aetna-volume: failed to bind default metadata: {err}")
                        }
                    }
                }
            }

            if global.type_ == pw::types::ObjectType::Node {
                if let Some(props) = global.props.as_ref()
                    && is_internal_aetna_node(props)
                {
                    return;
                }
                let node_id = global.id;
                let node = match registry_for_bind.bind::<pw::node::Node, _>(global) {
                    Ok(node) => node,
                    Err(err) => {
                        eprintln!("aetna-volume: failed to bind node {node_id}: {err}");
                        return;
                    }
                };
                let snapshot_for_props = snapshot_for_global.clone();
                let channels_for_props = channels_for_global.clone();
                let listener = node
                    .add_listener_local()
                    .param(move |_seq, id, _index, _next, param| {
                        if id != ParamType::Props {
                            return;
                        }
                        let Some(param) = param else {
                            return;
                        };
                        let decoded = decode_props(param.as_bytes());
                        if let Some(count) = decoded.channel_count {
                            channels_for_props.borrow_mut().insert(node_id, count);
                        }
                        if decoded.mute.is_none() && decoded.scalar.is_none() {
                            return;
                        }
                        if let Ok(mut snap) = snapshot_for_props.lock()
                            && let Some(node) = snap.nodes.iter_mut().find(|n| n.id == node_id)
                        {
                            let current = node.volume.clone().unwrap_or(Volume {
                                scalar: 1.0,
                                muted: false,
                            });
                            node.volume = Some(Volume {
                                scalar: decoded.scalar.unwrap_or(current.scalar),
                                muted: decoded.mute.unwrap_or(current.muted),
                            });
                        }
                    })
                    .register();
                node.subscribe_params(&[ParamType::Props]);
                proxies_for_global.borrow_mut().insert(
                    node_id,
                    NodeEntry {
                        proxy: node,
                        _listener: listener,
                    },
                );
            }
        })
        .global_remove(move |id| {
            if let Ok(mut snap) = snapshot_for_remove.lock() {
                snap.nodes.retain(|n| n.id != id);
                snap.cards.retain(|c| c.id != id);
            }
            proxies_for_remove.borrow_mut().remove(&id);
            channels_for_remove.borrow_mut().remove(&id);
            devices_for_remove.borrow_mut().remove(&id);
            // Drop the cached default-metadata binding if its global went
            // away — the proxy is stale at that point and the next
            // `default`-named global to appear will re-bind cleanly.
            let mut default_slot = default_for_remove.borrow_mut();
            if default_slot.as_ref().is_some_and(|e| e.global_id == id) {
                *default_slot = None;
            }
        })
        .register();

    let proxies_for_commands = proxies.clone();
    let channels_for_commands = channels.clone();
    let default_for_commands = default_metadata.clone();
    let devices_for_commands = devices.clone();
    let snapshot_for_commands = snapshot.clone();
    let mainloop_for_quit = mainloop.clone();
    let _commands_attached = commands_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        BackendCommand::Quit => mainloop_for_quit.quit(),
        BackendCommand::SetMute { node_id, muted } => {
            apply_mute(&proxies_for_commands.borrow(), node_id, muted);
        }
        BackendCommand::SetVolume { node_id, scalar } => {
            let count = channels_for_commands.borrow().get(&node_id).copied();
            apply_volume(&proxies_for_commands.borrow(), node_id, scalar, count);
        }
        BackendCommand::SetDefaultSink { node_name } => {
            // Mirror what `wpctl set-default` does: write the configured
            // pref and let WirePlumber cascade to the active key. Also
            // optimistically update our own snapshot — WP's metadata server
            // does not reliably broadcast property events back to clients
            // for default-key writes, so the marker would otherwise stay
            // on the previous value until something else churned the state.
            apply_default(
                &default_for_commands.borrow(),
                "default.configured.audio.sink",
                &node_name,
            );
            if let Ok(mut snap) = snapshot_for_commands.lock() {
                snap.default_sink_name = Some(node_name);
            }
        }
        BackendCommand::SetDefaultSource { node_name } => {
            apply_default(
                &default_for_commands.borrow(),
                "default.configured.audio.source",
                &node_name,
            );
            if let Ok(mut snap) = snapshot_for_commands.lock() {
                snap.default_source_name = Some(node_name);
            }
        }
        BackendCommand::SetCardProfile {
            card_id,
            profile_index,
        } => {
            apply_card_profile(&devices_for_commands.borrow(), card_id, profile_index);
        }
    });

    // Sync the core to know when the initial registry walk is complete,
    // then unblock the main thread waiting in `new()`.
    let pending = core.sync(0)?;
    let ready_for_done = ready.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                signal_ready(&ready_for_done);
            }
        })
        .register();

    mainloop.run();
    Ok(())
}

struct NodeEntry {
    proxy: pw::node::Node,
    _listener: pw::node::NodeListener,
}

struct DefaultMetaEntry {
    proxy: pw::metadata::Metadata,
    _listener: pw::metadata::MetadataListener,
    global_id: u32,
}

struct DeviceEntry {
    proxy: pw::device::Device,
    _listener: pw::device::DeviceListener,
}

fn apply_card_profile(devices: &HashMap<u32, DeviceEntry>, card_id: u32, profile_index: u32) {
    let Some(entry) = devices.get(&card_id) else {
        eprintln!("aetna-volume: cannot set profile on card {card_id} — device not bound");
        return;
    };
    let pod = match build_profile_pod(profile_index) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("aetna-volume: failed to build profile pod for card {card_id}: {err}");
            return;
        }
    };
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&pod) else {
        eprintln!("aetna-volume: built invalid profile pod for card {card_id}");
        return;
    };
    entry.proxy.set_param(ParamType::Profile, 0, pod);
}

fn build_profile_pod(profile_index: u32) -> Result<Vec<u8>> {
    let obj = Object {
        type_: SpaTypes::ObjectParamProfile.as_raw(),
        id: ParamType::Profile.as_raw(),
        properties: vec![
            Property::new(
                pw::spa::sys::SPA_PARAM_PROFILE_index,
                Value::Int(profile_index as i32),
            ),
            // Persist across PipeWire restarts. Matches what
            // pavucontrol writes when you pick a profile.
            Property::new(pw::spa::sys::SPA_PARAM_PROFILE_save, Value::Bool(true)),
        ],
    };
    let mut out = Vec::new();
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(&mut out),
        &Value::Object(obj),
    )?;
    Ok(out)
}

fn decode_active_profile_index(bytes: &[u8]) -> Option<u32> {
    let (_, value) =
        pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(bytes).ok()?;
    let Value::Object(obj) = value else {
        return None;
    };
    for prop in &obj.properties {
        if prop.key == pw::spa::sys::SPA_PARAM_PROFILE_index
            && let Value::Int(i) = prop.value
        {
            return Some(i as u32);
        }
    }
    None
}

fn decode_enum_profile(bytes: &[u8]) -> Option<AudioProfile> {
    let (_, value) =
        pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(bytes).ok()?;
    let Value::Object(obj) = value else {
        return None;
    };
    let mut index: Option<u32> = None;
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut available = ProfileAvailability::Unknown;
    for prop in &obj.properties {
        match prop.key {
            k if k == pw::spa::sys::SPA_PARAM_PROFILE_index => {
                if let Value::Int(i) = prop.value {
                    index = Some(i as u32);
                }
            }
            k if k == pw::spa::sys::SPA_PARAM_PROFILE_name => {
                if let Value::String(s) = &prop.value {
                    name = Some(s.clone());
                }
            }
            k if k == pw::spa::sys::SPA_PARAM_PROFILE_description => {
                if let Value::String(s) = &prop.value {
                    description = Some(s.clone());
                }
            }
            k if k == pw::spa::sys::SPA_PARAM_PROFILE_available => {
                if let Value::Id(id) = prop.value {
                    available = match id.0 {
                        x if x == pw::spa::sys::SPA_PARAM_AVAILABILITY_yes => {
                            ProfileAvailability::Yes
                        }
                        x if x == pw::spa::sys::SPA_PARAM_AVAILABILITY_no => {
                            ProfileAvailability::No
                        }
                        _ => ProfileAvailability::Unknown,
                    };
                }
            }
            _ => {}
        }
    }
    let index = index?;
    let name = name.unwrap_or_else(|| format!("profile-{index}"));
    let description = description.clone().unwrap_or_else(|| name.clone());
    Some(AudioProfile {
        index,
        name,
        description,
        available,
    })
}

fn apply_default(entry: &Option<DefaultMetaEntry>, key: &str, node_name: &str) {
    let Some(entry) = entry else {
        eprintln!("aetna-volume: cannot set {key}={node_name} — default metadata not yet bound");
        return;
    };
    // PipeWire metadata stores defaults as JSON like `{"name": "..."}`.
    // The `subject` is `PW_ID_CORE` (= 0) for the global defaults.
    let value = format!("{{\"name\":\"{}\"}}", json_escape(node_name));
    entry
        .proxy
        .set_property(0, key, Some("Spa:String:JSON"), Some(&value));
}

/// Escape a string for inclusion in a JSON string literal. Only the
/// backslash and double-quote escapes are required for PipeWire node
/// names (which never contain control characters in practice).
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Pull the `name` field out of `{"name":"..."}` style JSON that
/// PipeWire stores in the `default` metadata.
fn default_name_from_json(value: &str) -> Option<String> {
    let key = "\"name\"";
    let key_pos = value.find(key)?;
    let after_key = &value[key_pos + key.len()..];
    let colon_pos = after_key.find(':')?;
    let after_colon = &after_key[colon_pos + 1..];
    let open_quote = after_colon.find('"')?;
    let rest = &after_colon[open_quote + 1..];
    let close_quote = rest.find('"')?;
    Some(rest[..close_quote].to_string())
}

fn apply_mute(proxies: &HashMap<u32, NodeEntry>, node_id: u32, muted: bool) {
    let Some(entry) = proxies.get(&node_id) else {
        return;
    };
    let pod = match build_props_pod(vec![Property::new(
        pw::spa::sys::SPA_PROP_mute,
        Value::Bool(muted),
    )]) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("aetna-volume: failed to build mute pod for {node_id}: {err}");
            return;
        }
    };
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&pod) else {
        eprintln!("aetna-volume: built invalid mute pod for {node_id}");
        return;
    };
    entry.proxy.set_param(ParamType::Props, 0, pod);
}

fn apply_volume(
    proxies: &HashMap<u32, NodeEntry>,
    node_id: u32,
    scalar: f32,
    channels: Option<usize>,
) {
    let Some(entry) = proxies.get(&node_id) else {
        return;
    };
    let scalar = scalar.clamp(0.0, 1.5);
    // Prefer `channelVolumes` when we know the node's channel count
    // — that's the prop pavucontrol and the rest of the PipeWire
    // ecosystem read and write, so writes here stay in lock-step with
    // them. Fall back to master `volume` only when we haven't seen a
    // Props event yet (rare, and corrects itself on the next event).
    let property = match channels {
        Some(n) if n > 0 => Property::new(
            pw::spa::sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(vec![scalar; n])),
        ),
        _ => Property::new(pw::spa::sys::SPA_PROP_volume, Value::Float(scalar)),
    };
    let pod = match build_props_pod(vec![property]) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("aetna-volume: failed to build volume pod for {node_id}: {err}");
            return;
        }
    };
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&pod) else {
        eprintln!("aetna-volume: built invalid volume pod for {node_id}");
        return;
    };
    entry.proxy.set_param(ParamType::Props, 0, pod);
}

struct DecodedProps {
    mute: Option<bool>,
    /// Either the first channel volume (preferred — that's what
    /// pavucontrol displays and what we want lock-step with) or the
    /// master volume as a fallback.
    scalar: Option<f32>,
    /// The length of the `channelVolumes` array, used by
    /// `apply_volume` to write a correctly-sized array back.
    channel_count: Option<usize>,
}

fn decode_props(bytes: &[u8]) -> DecodedProps {
    let mut decoded = DecodedProps {
        mute: None,
        scalar: None,
        channel_count: None,
    };
    let Ok((_, value)) = pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(bytes)
    else {
        return decoded;
    };
    let Value::Object(obj) = value else {
        return decoded;
    };
    let mut master = None;
    let mut channel_first = None;
    for prop in &obj.properties {
        match prop.key {
            k if k == pw::spa::sys::SPA_PROP_mute => {
                if let Value::Bool(b) = prop.value {
                    decoded.mute = Some(b);
                }
            }
            k if k == pw::spa::sys::SPA_PROP_volume => {
                if let Value::Float(f) = prop.value {
                    master = Some(f);
                }
            }
            k if k == pw::spa::sys::SPA_PROP_channelVolumes => {
                if let Value::ValueArray(ValueArray::Float(arr)) = &prop.value {
                    decoded.channel_count = Some(arr.len());
                    channel_first = arr.first().copied();
                }
            }
            _ => {}
        }
    }
    decoded.scalar = channel_first.or(master);
    decoded
}

fn build_props_pod(properties: Vec<Property>) -> Result<Vec<u8>> {
    let obj = Object {
        type_: SpaTypes::ObjectParamProps.as_raw(),
        id: ParamType::Props.as_raw(),
        properties,
    };
    let (cursor, _) = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )?;
    Ok(cursor.into_inner())
}

fn signal_ready(ready: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, cvar) = &**ready;
    if let Ok(mut flag) = lock.lock() {
        *flag = true;
        cvar.notify_all();
    }
}

fn wait_for_ready(ready: &Arc<(Mutex<bool>, Condvar)>, timeout: Duration) {
    let (lock, cvar) = &**ready;
    let Ok(flag) = lock.lock() else {
        return;
    };
    let _ = cvar.wait_timeout_while(flag, timeout, |ready| !*ready);
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
        media_role: prop(props, "media.role").map(str::to_string),
        volume: None,
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

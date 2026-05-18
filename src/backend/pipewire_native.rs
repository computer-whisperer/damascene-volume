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
use crate::util::parse_name_json;

/// Commands sent from the main thread to the PipeWire backend thread
/// over [`pw::channel`] (loop-integrated, fires the receiver callback
/// the next time the mainloop wakes).
enum BackendCommand {
    SetMute {
        node_id: u32,
        muted: bool,
    },
    SetVolume {
        node_id: u32,
        scalar: f32,
    },
    SetDefaultSink {
        node_name: String,
    },
    SetDefaultSource {
        node_name: String,
    },
    SetCardProfile {
        card_id: u32,
        profile_index: u32,
    },
    SetStreamTarget {
        stream_id: u32,
        target_serial: Option<u64>,
    },
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

    fn set_stream_target(&self, stream_id: u32, target_serial: Option<u64>) {
        let _ = self.commands.send(BackendCommand::SetStreamTarget {
            stream_id,
            target_serial,
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

    // Per-stream `target.object` values received from the `default`
    // metadata before the matching stream node global has appeared in
    // the registry. Drained into `AudioNode.target` when the node is
    // registered. Without this, the property event that fires at bind
    // time for streams that already have a routing override would be
    // dropped on the floor when the stream came after the metadata.
    let pending_targets: Rc<RefCell<HashMap<u32, String>>> = Rc::new(RefCell::new(HashMap::new()));

    // Live link graph, keyed by the Link's own global id → (output
    // node, input node). Maintained as a side table so we can rebuild
    // `snapshot.peers` cleanly on each add/remove, instead of trying
    // to refcount distinct port-level links between the same pair of
    // nodes (FL + FR between a stream and a sink show up as two Link
    // globals; collapsing them down to one peer edge is the rebuild's
    // job).
    let links: Rc<RefCell<HashMap<u32, (u32, u32)>>> = Rc::new(RefCell::new(HashMap::new()));

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
    let pending_for_global = pending_targets.clone();
    let pending_for_remove = pending_targets.clone();
    let links_for_global = links.clone();
    let links_for_remove = links.clone();
    let registry_for_bind = registry.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let is_card = if let Ok(mut snap) = snapshot_for_global.lock() {
                if let Some(mut node) = audio_node_from_global(global) {
                    if !snap.nodes.iter().any(|existing| existing.id == node.id) {
                        // Apply any `target.object` event the metadata
                        // emitted before this node global arrived. The
                        // metadata value is authoritative for routing
                        // overrides — it overwrites whatever was on the
                        // node's own props at registration time.
                        if let Some(pending) = pending_for_global.borrow_mut().remove(&node.id) {
                            node.target = Some(pending);
                        }
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
                            let pending_for_meta = pending_for_global.clone();
                            let listener = meta
                                .add_listener_local()
                                .property(move |subject, key, _type_, value| {
                                    let Some(key) = key else {
                                        // null key = every property for
                                        // this subject cleared. For the
                                        // global default subject that
                                        // drops both default-device
                                        // slots; for a stream subject it
                                        // drops the routing override.
                                        if let Ok(mut snap) = snapshot_for_meta.lock() {
                                            if subject == 0 {
                                                snap.default_sink_name = None;
                                                snap.default_source_name = None;
                                            } else if let Some(node) =
                                                snap.nodes.iter_mut().find(|n| n.id == subject)
                                            {
                                                node.target = None;
                                            }
                                        }
                                        pending_for_meta.borrow_mut().remove(&subject);
                                        return 0;
                                    };
                                    if subject == 0 {
                                        let is_sink = match key {
                                            "default.audio.sink" => true,
                                            "default.audio.source" => false,
                                            _ => return 0,
                                        };
                                        let name = value
                                            .and_then(|v| parse_name_json(v).map(str::to_string));
                                        if let Ok(mut snap) = snapshot_for_meta.lock() {
                                            if is_sink {
                                                snap.default_sink_name = name;
                                            } else {
                                                snap.default_source_name = name;
                                            }
                                        }
                                        return 0;
                                    }
                                    // Per-stream routing override. WP
                                    // and modern clients write
                                    // `target.object` as `Spa:Id` (bare
                                    // serial) or `Spa:String:JSON`;
                                    // legacy clients use `target.node`.
                                    // `resolved_target_for_stream`
                                    // already handles both shapes, so
                                    // we pass the raw value through.
                                    if key != "target.object" && key != "target.node" {
                                        return 0;
                                    }
                                    let raw = value.map(str::to_string);
                                    let applied = if let Ok(mut snap) = snapshot_for_meta.lock() {
                                        if let Some(node) =
                                            snap.nodes.iter_mut().find(|n| n.id == subject)
                                        {
                                            node.target = raw.clone();
                                            true
                                        } else {
                                            false
                                        }
                                    } else {
                                        false
                                    };
                                    let mut pending = pending_for_meta.borrow_mut();
                                    if applied {
                                        pending.remove(&subject);
                                    } else {
                                        match raw {
                                            Some(v) => {
                                                pending.insert(subject, v);
                                            }
                                            None => {
                                                pending.remove(&subject);
                                            }
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

            if global.type_ == pw::types::ObjectType::Link
                && let Some(props) = global.props.as_ref().map(|p| p.as_ref())
            {
                // PipeWire publishes the endpoints as decimal strings
                // in the registry props. We only need the node-level
                // adjacency, not per-port detail, so the port keys are
                // ignored.
                let out = prop(props, "link.output.node").and_then(|s| s.parse::<u32>().ok());
                let inp = prop(props, "link.input.node").and_then(|s| s.parse::<u32>().ok());
                if let (Some(out), Some(inp)) = (out, inp) {
                    links_for_global.borrow_mut().insert(global.id, (out, inp));
                    if let Ok(mut snap) = snapshot_for_global.lock() {
                        rebuild_peers(&mut snap.peers, &links_for_global.borrow());
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
            // Globals can be of any type — there's no `type_` here to
            // narrow on, so we just probe every per-type table. A
            // missing id is a no-op on each; cheap, and avoids the
            // alternative of tracking id→type ourselves.
            let link_dropped = links_for_remove.borrow_mut().remove(&id).is_some();
            if let Ok(mut snap) = snapshot_for_remove.lock() {
                snap.nodes.retain(|n| n.id != id);
                snap.cards.retain(|c| c.id != id);
                if link_dropped {
                    rebuild_peers(&mut snap.peers, &links_for_remove.borrow());
                }
            }
            proxies_for_remove.borrow_mut().remove(&id);
            channels_for_remove.borrow_mut().remove(&id);
            devices_for_remove.borrow_mut().remove(&id);
            pending_for_remove.borrow_mut().remove(&id);
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
        BackendCommand::SetStreamTarget {
            stream_id,
            target_serial,
        } => {
            apply_stream_target(&default_for_commands.borrow(), stream_id, target_serial);
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

/// Pin (or clear) a stream's routing override by writing the
/// `target.object` property on the `default` metadata, keyed by the
/// stream's node id. `Some(serial)` writes the target's
/// `object.serial` as `Spa:Id` — the form WirePlumber's
/// stream-router actually honors. (The `Spa:String:JSON` `{"name":
/// "..."}` form is also nominally supported but WP silently ignores
/// it for routing decisions, so we don't use it.) `None` deletes the
/// property so the stream falls back to default policy routing.
fn apply_stream_target(
    entry: &Option<DefaultMetaEntry>,
    stream_id: u32,
    target_serial: Option<u64>,
) {
    let Some(entry) = entry else {
        eprintln!(
            "aetna-volume: cannot set stream target on {stream_id} — default metadata not yet bound"
        );
        return;
    };
    match target_serial {
        Some(serial) => {
            let value = serial.to_string();
            entry
                .proxy
                .set_property(stream_id, "target.object", Some("Spa:Id"), Some(&value));
        }
        None => {
            entry
                .proxy
                .set_property(stream_id, "target.object", None, None);
        }
    }
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

/// Rebuild the snapshot's `peers` map from scratch off the current
/// link table. Cheaper than the alternative (per-edge refcounting):
/// link counts are small (tens, not thousands) and rebuilds happen
/// only on graph-shape changes, not on every audio frame. Edges are
/// bidirectional so a single lookup by stream id finds its peer
/// device, and a lookup by device id finds incoming streams.
fn rebuild_peers(peers: &mut HashMap<u32, Vec<u32>>, links: &HashMap<u32, (u32, u32)>) {
    peers.clear();
    for &(out, inp) in links.values() {
        let out_peers = peers.entry(out).or_default();
        if !out_peers.contains(&inp) {
            out_peers.push(inp);
        }
        let in_peers = peers.entry(inp).or_default();
        if !in_peers.contains(&out) {
            in_peers.push(out);
        }
    }
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

    // Object serials are documented as u64. PipeWire never re-uses
    // them, so they make a stable identity for routing writes — but
    // the global id can be reused. If the prop is missing on some
    // exotic object, fall back to the id (cast to u64): better than a
    // 0 that would alias with no-routing.
    let serial = prop(props, "object.serial")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(u64::from(global.id));

    Some(AudioNode {
        id: global.id,
        serial,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_peers_dedupes_parallel_port_links() {
        // FL + FR between the same stream/sink show up as two Link
        // globals. The collapsed adjacency should list the peer once.
        let mut links: HashMap<u32, (u32, u32)> = HashMap::new();
        links.insert(500, (10, 20)); // stream→sink, FL
        links.insert(501, (10, 20)); // stream→sink, FR
        let mut peers: HashMap<u32, Vec<u32>> = HashMap::new();
        rebuild_peers(&mut peers, &links);
        assert_eq!(peers.get(&10), Some(&vec![20]));
        assert_eq!(peers.get(&20), Some(&vec![10]));
    }

    #[test]
    fn rebuild_peers_handles_multiple_distinct_edges() {
        // A stream linked to two sinks (rare but legal — manual
        // pw-link, or a duplicating filter) should list both peers.
        let mut links: HashMap<u32, (u32, u32)> = HashMap::new();
        links.insert(500, (10, 20));
        links.insert(501, (10, 30));
        let mut peers: HashMap<u32, Vec<u32>> = HashMap::new();
        rebuild_peers(&mut peers, &links);
        let stream_peers = peers.get(&10).expect("stream has peers");
        assert_eq!(stream_peers.len(), 2);
        assert!(stream_peers.contains(&20));
        assert!(stream_peers.contains(&30));
        assert_eq!(peers.get(&20), Some(&vec![10]));
        assert_eq!(peers.get(&30), Some(&vec![10]));
    }

    #[test]
    fn rebuild_peers_clears_stale_entries() {
        // Rebuilding off an empty link table must wipe the prior
        // graph — otherwise removed links would leave ghost edges.
        let mut peers: HashMap<u32, Vec<u32>> = HashMap::new();
        peers.insert(10, vec![20]);
        peers.insert(20, vec![10]);
        rebuild_peers(&mut peers, &HashMap::new());
        assert!(peers.is_empty());
    }
}

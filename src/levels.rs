use pipewire as pw;
use pw::{properties::properties, spa};
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    mem,
    rc::Rc,
    sync::{
        Arc, Mutex, Once,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use crate::model::{AudioClass, AudioNode, Direction};

const FFT_SIZE: usize = 2048;
const FFT_HOP: usize = 1024;
const SPECTRUM_BINS: usize = 72;
const SPECTRUM_HISTORY: usize = 256;
const MIN_SPECTRUM_HZ: f32 = 35.0;
const MAX_SPECTRUM_HZ: f32 = 18_000.0;

#[derive(Debug, Clone, Default)]
pub struct NodeLevels {
    pub peaks: Vec<f32>,
    pub rms: Vec<f32>,
}

impl NodeLevels {
    pub fn channel_count(&self) -> usize {
        self.peaks.len().max(self.rms.len())
    }

    pub fn peak(&self, channel: usize) -> f32 {
        self.peaks.get(channel).copied().unwrap_or(0.0)
    }

    pub fn rms(&self, channel: usize) -> f32 {
        self.rms.get(channel).copied().unwrap_or(0.0)
    }
}

#[derive(Debug, Clone)]
pub struct SpectrumSnapshot {
    pub columns: Vec<Vec<f32>>,
    pub bins: usize,
    pub history: usize,
    pub sample_rate: u32,
    pub min_hz: f32,
    pub max_hz: f32,
}

impl Default for SpectrumSnapshot {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            bins: SPECTRUM_BINS,
            history: SPECTRUM_HISTORY,
            sample_rate: 48_000,
            min_hz: MIN_SPECTRUM_HZ,
            max_hz: MAX_SPECTRUM_HZ,
        }
    }
}

#[derive(Default)]
pub struct LevelService {
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    spectra: Arc<Mutex<HashMap<u32, SpectrumSnapshot>>>,
    spectrum_nodes: Arc<Mutex<HashSet<u32>>>,
    meters: HashMap<u32, MeterHandle>,
}

impl LevelService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconcile meter threads against the set of nodes the user can
    /// currently see. Anything outside `visible` is torn down — meters
    /// for the inactive tabs aren't worth their fds, threads, and
    /// PipeWire-side bookkeeping. DSP-role nodes (other apps' peak
    /// detectors, like pavucontrol's "PulseAudio Volume Control" capture
    /// streams) are skipped unconditionally to keep us out of a
    /// metering-each-other feedback loop.
    pub fn ensure_visible(&mut self, visible: &[&AudioNode], spectrum_node: Option<&AudioNode>) {
        let mut wanted = visible
            .iter()
            .filter(|node| meter_route_for(node).is_some())
            .map(|node| node.id)
            .collect::<HashSet<_>>();
        let spectrum_node_id = spectrum_node
            .filter(|node| meter_route_for(node).is_some())
            .map(|node| node.id);
        if let Some(node_id) = spectrum_node_id {
            wanted.insert(node_id);
        }
        self.meters.retain(|node_id, meter| {
            let keep = wanted.contains(node_id);
            if !keep {
                meter.stop.store(true, Ordering::Relaxed);
            }
            keep
        });
        if let Ok(mut levels) = self.levels.lock() {
            levels.retain(|node_id, _| wanted.contains(node_id));
        }
        if let Ok(mut spectrum_nodes) = self.spectrum_nodes.lock() {
            spectrum_nodes.clear();
            if let Some(node_id) = spectrum_node_id {
                spectrum_nodes.insert(node_id);
            }
        }
        if let Ok(mut spectra) = self.spectra.lock() {
            spectra.retain(|node_id, _| Some(*node_id) == spectrum_node_id);
        }
        for node in visible {
            self.ensure_node(node);
        }
        if let Some(node) = spectrum_node {
            self.ensure_node(node);
        }
    }

    pub fn level_for(&self, node_id: u32) -> Option<NodeLevels> {
        self.levels
            .lock()
            .ok()
            .and_then(|levels| levels.get(&node_id).cloned())
    }

    pub fn spectrum_for(&self, node_id: u32) -> Option<SpectrumSnapshot> {
        self.spectra
            .lock()
            .ok()
            .and_then(|spectra| spectra.get(&node_id).cloned())
    }

    pub fn active_meter_count(&self) -> usize {
        self.meters.len()
    }

    fn ensure_node(&mut self, node: &AudioNode) {
        if self.meters.contains_key(&node.id) {
            return;
        }
        let Some(route) = meter_route_for(node) else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        spawn_meter(
            node.id,
            route,
            self.levels.clone(),
            self.spectra.clone(),
            self.spectrum_nodes.clone(),
            stop.clone(),
        );
        self.meters.insert(node.id, MeterHandle { stop });
    }
}

impl Drop for LevelService {
    fn drop(&mut self) {
        for meter in self.meters.values() {
            meter.stop.store(true, Ordering::Relaxed);
        }
    }
}

struct MeterHandle {
    stop: Arc<AtomicBool>,
}

struct MeterData {
    node_id: u32,
    format: spa::param::audio::AudioInfoRaw,
    mainloop: pw::main_loop::MainLoopRc,
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    spectra: Arc<Mutex<HashMap<u32, SpectrumSnapshot>>>,
    spectrum_nodes: Arc<Mutex<HashSet<u32>>>,
    stop: Arc<AtomicBool>,
    smooth_peaks: Vec<f32>,
    smooth_rms: Vec<f32>,
    spectrum: Option<SpectrumProcessor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MeterRoute {
    /// Capture path that lets WirePlumber's policy auto-link us to a real
    /// source. Used for input devices (pinned to their source via
    /// `target.object`) and input streams (which have no output ports of
    /// their own, so the meter falls through to the default source).
    AutoConnect,
    /// Capture path that bypasses WirePlumber and creates explicit
    /// port-to-port links from the target node's output-direction ports
    /// to our inputs. Used for output streams (their outputs) and output
    /// devices (their monitor ports).
    ///
    /// Output devices *must* use this rather than autoconnect: a
    /// `stream.capture.sink` autoconnect treats `target.object` as a hint,
    /// not a pin, so when the intended sink is suspended WirePlumber falls
    /// back to linking us to the *default* sink's monitor. Several idle
    /// sinks then all meter the default sink's audio. Explicit links can't
    /// drift — we name both ports ourselves.
    LinkFromOutputs,
}

fn meter_route_for(node: &AudioNode) -> Option<MeterRoute> {
    // Other apps' peak detectors (pavucontrol's "PulseAudio Volume
    // Control" capture streams, qpwgraph monitors, our own meters when
    // a sibling instance is running) advertise media.role=DSP. Skip
    // them — metering somebody else's meter just adds fds and risks a
    // runaway loop if both apps are using the same heuristic.
    if node.media_role.as_deref() == Some("DSP") {
        return None;
    }
    match node.class {
        AudioClass::Device {
            direction: Direction::Output,
        } => Some(MeterRoute::LinkFromOutputs),
        AudioClass::Device {
            direction: Direction::Input,
        } => Some(MeterRoute::AutoConnect),
        AudioClass::Stream {
            direction: Direction::Output,
        } => Some(MeterRoute::LinkFromOutputs),
        AudioClass::Stream {
            direction: Direction::Input,
        } => Some(MeterRoute::AutoConnect),
        _ => None,
    }
}

fn spawn_meter(
    node_id: u32,
    route: MeterRoute,
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    spectra: Arc<Mutex<HashMap<u32, SpectrumSnapshot>>>,
    spectrum_nodes: Arc<Mutex<HashSet<u32>>>,
    stop: Arc<AtomicBool>,
) {
    let thread_name = format!("damascene-volume-meter-{node_id}");
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let result = match route {
                MeterRoute::AutoConnect => {
                    run_pipewire_auto_meter(node_id, levels, spectra, spectrum_nodes, stop)
                }
                MeterRoute::LinkFromOutputs => {
                    run_pipewire_linked_meter(node_id, levels, spectra, spectrum_nodes, stop)
                }
            };
            if let Err(err) = result {
                eprintln!("damascene-volume: level meter for node {node_id} stopped: {err}");
            }
        })
        .expect("spawn PipeWire level meter");
}

fn run_pipewire_auto_meter(
    node_id: u32,
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    spectra: Arc<Mutex<HashMap<u32, SpectrumSnapshot>>>,
    spectrum_nodes: Arc<Mutex<HashSet<u32>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    pipewire_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    // Input devices pin to their source via `target.object`; input
    // streams have no capturable port of their own, so WirePlumber lands
    // us on the default source. Either way this path captures a real
    // source, where autoconnect honours the channel layout (mono mics
    // meter as a single channel). Output *devices* deliberately do not
    // come through here — see `MeterRoute::LinkFromOutputs`.
    let props = properties! {
        *pw::keys::APP_NAME => "damascene-volume",
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        // Tag ourselves as a DSP/peak-detect stream so other monitoring
        // tools (and `meter_route_for` above, if a sibling instance is
        // running) know not to attach a meter to us.
        *pw::keys::MEDIA_ROLE => "DSP",
        *pw::keys::NODE_NAME => format!("damascene-volume.meter.{node_id}"),
        "target.object" => node_id.to_string(),
    };

    let stream = pw::stream::StreamBox::new(&core, "damascene-volume-meter", props)?;
    let data = MeterData {
        node_id,
        format: Default::default(),
        mainloop: mainloop.clone(),
        levels,
        spectra,
        spectrum_nodes,
        stop,
        smooth_peaks: Vec::new(),
        smooth_rms: Vec::new(),
        spectrum: None,
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, _, state| {
            if let pw::stream::StreamState::Error(err) = state {
                eprintln!(
                    "damascene-volume: meter stream error for node {}: {err}",
                    data.node_id
                );
                data.mainloop.quit();
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            let _ = data.format.parse(param);
        })
        .process(|stream, data| {
            if data.stop.load(Ordering::Relaxed) {
                data.mainloop.quit();
                return;
            }
            process_meter_buffer(stream, data);
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("failed to build PipeWire format pod")?];

    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();
    Ok(())
}

/// Capture a node by creating explicit pw_link objects from its
/// output-direction ports to our capture stream's input ports. For an
/// output stream those are the stream's outputs; for an output device
/// (sink) they are the sink's `monitor_*` ports.
///
/// This bypasses WirePlumber's autoconnect policy entirely, which both
/// node kinds need:
/// - The session manager will not honour `target.object` pointed at a
///   `Stream/Output/Audio` node — such captures fall back to the default
///   source (the user's mic).
/// - A `stream.capture.sink` autoconnect treats `target.object` as a
///   hint; when the intended sink is suspended it silently relinks to the
///   *default* sink's monitor, so every idle sink ends up metering the
///   default sink's audio.
///
/// We open a capture without `AUTOCONNECT`, watch the registry until both
/// sides have ports, then construct link-factory objects for each
/// output→input pair — links we name ourselves and that therefore cannot
/// drift to another node.
fn run_pipewire_linked_meter(
    source_node_id: u32,
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    spectra: Arc<Mutex<HashMap<u32, SpectrumSnapshot>>>,
    spectrum_nodes: Arc<Mutex<HashSet<u32>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    pipewire_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry()?;

    let props = properties! {
        *pw::keys::APP_NAME => "damascene-volume",
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "DSP", // see auto-meter site for rationale
        *pw::keys::NODE_NAME => format!("damascene-volume.meter.{source_node_id}"),
    };

    let stream = pw::stream::StreamBox::new(&core, "damascene-volume-meter", props)?;
    let data = MeterData {
        node_id: source_node_id,
        format: Default::default(),
        mainloop: mainloop.clone(),
        levels,
        spectra,
        spectrum_nodes,
        stop,
        smooth_peaks: Vec::new(),
        smooth_rms: Vec::new(),
        spectrum: None,
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, _, state| {
            if let pw::stream::StreamState::Error(err) = state {
                eprintln!(
                    "damascene-volume: meter stream error for node {}: {err}",
                    data.node_id
                );
                data.mainloop.quit();
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            let _ = data.format.parse(param);
        })
        .process(|stream, data| {
            if data.stop.load(Ordering::Relaxed) {
                data.mainloop.quit();
                return;
            }
            process_meter_buffer(stream, data);
        })
        .register()?;

    // Lock the capture to stereo F32LE so our input ports exist before
    // any peer negotiation. Without an explicit channel count the stream
    // creates one mono input port and only the source's first channel
    // ever reaches us — every output stream looks left-only.
    // PipeWire inserts the necessary remix on the link side for sources
    // with a different channel layout.
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(48_000);
    audio_info.set_channels(2);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("failed to build PipeWire format pod")?];

    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let our_node_name = format!("damascene-volume.meter.{source_node_id}");
    let linker = Rc::new(RefCell::new(LinkerState::default()));
    let core_for_global = core.clone();
    let linker_for_global = linker.clone();
    let linker_for_remove = linker.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props.as_ref() else {
                return;
            };

            let mut state = linker_for_global.borrow_mut();
            match global.type_ {
                pw::types::ObjectType::Node => {
                    if prop(props, "node.name") == Some(our_node_name.as_str()) {
                        state.our_node_id = Some(global.id);
                    }
                }
                pw::types::ObjectType::Port => {
                    let Some(node_id) = prop(props, "node.id").and_then(|s| s.parse::<u32>().ok())
                    else {
                        return;
                    };
                    let direction = prop(props, "port.direction").unwrap_or("");
                    let is_output = direction == "out";
                    state
                        .ports_by_node
                        .entry(node_id)
                        .or_default()
                        .push((global.id, is_output));
                }
                _ => return,
            }
            try_link(&core_for_global, &mut state, source_node_id);
        })
        .global_remove(move |id| {
            let mut state = linker_for_remove.borrow_mut();
            if state.our_node_id == Some(id) {
                state.our_node_id = None;
            }
            for ports in state.ports_by_node.values_mut() {
                ports.retain(|(port_id, _)| *port_id != id);
            }
            state
                .links
                .retain(|(out_port, in_port), _| *out_port != id && *in_port != id);
        })
        .register();

    mainloop.run();
    Ok(())
}

#[derive(Default)]
struct LinkerState {
    our_node_id: Option<u32>,
    /// All port globals seen so far, keyed by their owning node id.
    /// Each entry is `(port_id, is_output_direction)`.
    ///
    /// Caching every port (rather than filtering source-vs-ours at
    /// arrival time) avoids a race where a Port global for our capture
    /// arrives before the corresponding Node global — at that moment we
    /// don't yet know `our_node_id`, so a directly-filtered version
    /// would silently drop the port and never relink.
    ports_by_node: HashMap<u32, Vec<(u32, bool)>>,
    /// Existing links keyed by `(source_output_port, our_input_port)`.
    /// Re-linking is idempotent against this map so late-arriving ports
    /// (e.g. the right channel showing up after the left) get connected
    /// as soon as both sides are present.
    links: HashMap<(u32, u32), pw::link::Link>,
}

fn try_link(core: &pw::core::CoreRc, state: &mut LinkerState, source_node_id: u32) {
    let Some(our_node_id) = state.our_node_id else {
        return;
    };
    let source_outputs: Vec<u32> = state
        .ports_by_node
        .get(&source_node_id)
        .map(|ports| {
            ports
                .iter()
                .filter(|(_, out)| *out)
                .map(|(id, _)| *id)
                .collect()
        })
        .unwrap_or_default();
    let our_inputs: Vec<u32> = state
        .ports_by_node
        .get(&our_node_id)
        .map(|ports| {
            ports
                .iter()
                .filter(|(_, out)| !*out)
                .map(|(id, _)| *id)
                .collect()
        })
        .unwrap_or_default();
    if source_outputs.is_empty() || our_inputs.is_empty() {
        return;
    }

    for (output_port, input_port) in source_outputs.into_iter().zip(our_inputs) {
        let pair = (output_port, input_port);
        if state.links.contains_key(&pair) {
            continue;
        }
        let link_props = properties! {
            "link.output.node" => source_node_id.to_string(),
            "link.output.port" => output_port.to_string(),
            "link.input.node" => our_node_id.to_string(),
            "link.input.port" => input_port.to_string(),
            "object.linger" => "false",
        };
        match core.create_object::<pw::link::Link>("link-factory", &link_props) {
            Ok(link) => {
                state.links.insert(pair, link);
            }
            Err(err) => eprintln!(
                "damascene-volume: failed to link {source_node_id}:{output_port} -> \
                 {our_node_id}:{input_port}: {err}"
            ),
        }
    }
}

fn process_meter_buffer(stream: &pw::stream::Stream, data: &mut MeterData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }
    let chunk = datas[0].chunk();
    let bytes = chunk.size() as usize;
    let channels = data.format.channels() as usize;
    if channels == 0 || bytes == 0 {
        return;
    }
    let Some(samples) = datas[0].data() else {
        return;
    };
    let usable = bytes.min(samples.len()) / mem::size_of::<f32>() * mem::size_of::<f32>();
    if usable == 0 {
        return;
    }

    publish_level_samples(
        data.node_id,
        &samples[..usable],
        channels,
        &data.levels,
        &mut data.smooth_peaks,
        &mut data.smooth_rms,
    );
    if data
        .spectrum_nodes
        .try_lock()
        .map(|nodes| nodes.contains(&data.node_id))
        .unwrap_or(false)
    {
        publish_spectrum_samples(
            data.node_id,
            &samples[..usable],
            channels,
            data.format.rate(),
            &data.spectra,
            &mut data.spectrum,
        );
    }
}

fn publish_level_samples(
    node_id: u32,
    samples: &[u8],
    channels: usize,
    levels: &Arc<Mutex<HashMap<u32, NodeLevels>>>,
    smooth_peaks: &mut Vec<f32>,
    smooth_rms: &mut Vec<f32>,
) {
    if channels == 0 || samples.len() < 4 {
        return;
    }
    let sample_count = samples.len() / 4;
    if sample_count == 0 {
        return;
    }

    let mut peaks = vec![0.0_f32; channels];
    let mut sums = vec![0.0_f32; channels];
    let mut counts = vec![0_u32; channels];
    for sample_index in 0..sample_count {
        let start = sample_index * 4;
        let Ok(bytes) = samples[start..start + 4].try_into() else {
            continue;
        };
        let value = f32::from_le_bytes(bytes);
        let channel = sample_index % channels;
        let abs = value.abs();
        peaks[channel] = peaks[channel].max(abs);
        sums[channel] += value * value;
        counts[channel] += 1;
    }

    resize_smoothing(smooth_peaks, smooth_rms, channels);
    for channel in 0..channels {
        let rms = if counts[channel] == 0 {
            0.0
        } else {
            (sums[channel] / counts[channel] as f32).sqrt()
        };
        smooth_peaks[channel] = smooth(smooth_peaks[channel], peaks[channel], 0.70);
        smooth_rms[channel] = smooth(smooth_rms[channel], rms, 0.82);
    }

    if let Ok(mut levels) = levels.try_lock() {
        levels.insert(
            node_id,
            NodeLevels {
                peaks: smooth_peaks.clone(),
                rms: smooth_rms.clone(),
            },
        );
    }
}

fn resize_smoothing(smooth_peaks: &mut Vec<f32>, smooth_rms: &mut Vec<f32>, channels: usize) {
    if smooth_peaks.len() != channels {
        smooth_peaks.resize(channels, 0.0);
    }
    if smooth_rms.len() != channels {
        smooth_rms.resize(channels, 0.0);
    }
}

fn smooth(previous: f32, next: f32, release: f32) -> f32 {
    if next >= previous {
        next
    } else {
        previous * release + next * (1.0 - release)
    }
}

struct SpectrumProcessor {
    sample_rate: u32,
    window: Vec<f32>,
    pending: Vec<f32>,
    input: Vec<Complex<f32>>,
    fft: Arc<dyn Fft<f32>>,
    last_bins: Vec<f32>,
}

impl SpectrumProcessor {
    fn new(sample_rate: u32) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|i| {
                let phase = std::f32::consts::TAU * i as f32 / (FFT_SIZE - 1) as f32;
                0.5 - 0.5 * phase.cos()
            })
            .collect();
        Self {
            sample_rate,
            window,
            pending: Vec::with_capacity(FFT_SIZE * 2),
            input: vec![Complex::default(); FFT_SIZE],
            fft,
            last_bins: vec![0.0; SPECTRUM_BINS],
        }
    }

    fn push_samples(&mut self, mono: &[f32]) -> Option<Vec<f32>> {
        self.pending.extend_from_slice(mono);
        let mut latest = None;
        while self.pending.len() >= FFT_SIZE {
            latest = Some(self.compute_column());
            self.pending.drain(..FFT_HOP);
        }
        latest
    }

    fn compute_column(&mut self) -> Vec<f32> {
        for (i, input) in self.input.iter_mut().enumerate() {
            input.re = self.pending[i] * self.window[i];
            input.im = 0.0;
        }
        self.fft.process(&mut self.input);

        let nyquist = self.sample_rate as f32 * 0.5;
        let max_hz = MAX_SPECTRUM_HZ.min(nyquist);
        let min_log = MIN_SPECTRUM_HZ.ln();
        let max_log = max_hz.ln();
        let mut bins = vec![0.0; SPECTRUM_BINS];
        for (bin, value) in bins.iter_mut().enumerate() {
            let low_t = bin as f32 / SPECTRUM_BINS as f32;
            let high_t = (bin + 1) as f32 / SPECTRUM_BINS as f32;
            let low_hz = (min_log + (max_log - min_log) * low_t).exp();
            let high_hz = (min_log + (max_log - min_log) * high_t).exp();
            let low_i = hz_to_fft_index(low_hz, self.sample_rate).max(1);
            let high_i = hz_to_fft_index(high_hz, self.sample_rate)
                .max(low_i + 1)
                .min(FFT_SIZE / 2);
            let mut peak = 0.0_f32;
            for i in low_i..high_i {
                let c = self.input[i];
                peak = peak.max((c.re * c.re + c.im * c.im).sqrt());
            }
            let normalized_mag = peak / (FFT_SIZE as f32 * 0.5);
            let db = 20.0 * normalized_mag.max(0.000_001).log10();
            let raw = ((db + 78.0) / 72.0).clamp(0.0, 1.0);
            let prev = self.last_bins[bin];
            *value = if raw > prev {
                prev * 0.30 + raw * 0.70
            } else {
                prev * 0.82 + raw * 0.18
            };
        }
        self.last_bins.clone_from(&bins);
        bins
    }
}

fn publish_spectrum_samples(
    node_id: u32,
    samples: &[u8],
    channels: usize,
    sample_rate: u32,
    spectra: &Arc<Mutex<HashMap<u32, SpectrumSnapshot>>>,
    processor: &mut Option<SpectrumProcessor>,
) {
    if channels == 0 || samples.len() < 4 {
        return;
    }
    let sample_rate = sample_rate.max(1);
    if processor
        .as_ref()
        .map(|processor| processor.sample_rate != sample_rate)
        .unwrap_or(true)
    {
        *processor = Some(SpectrumProcessor::new(sample_rate));
    }

    let sample_count = samples.len() / 4;
    let frame_count = sample_count / channels;
    if frame_count == 0 {
        return;
    }

    let mut mono = Vec::with_capacity(frame_count);
    for frame in 0..frame_count {
        let mut sum = 0.0_f32;
        for channel in 0..channels {
            let sample_index = frame * channels + channel;
            let start = sample_index * 4;
            let Ok(bytes) = samples[start..start + 4].try_into() else {
                continue;
            };
            sum += f32::from_le_bytes(bytes);
        }
        mono.push(sum / channels as f32);
    }

    let Some(column) = processor
        .as_mut()
        .and_then(|processor| processor.push_samples(&mono))
    else {
        return;
    };

    if let Ok(mut spectra) = spectra.try_lock() {
        let entry = spectra.entry(node_id).or_insert_with(|| SpectrumSnapshot {
            sample_rate,
            ..SpectrumSnapshot::default()
        });
        if entry.sample_rate != sample_rate {
            *entry = SpectrumSnapshot {
                sample_rate,
                ..SpectrumSnapshot::default()
            };
        }
        entry.columns.push(column);
        if entry.columns.len() > entry.history {
            let overflow = entry.columns.len() - entry.history;
            entry.columns.drain(..overflow);
        }
    }
}

fn hz_to_fft_index(hz: f32, sample_rate: u32) -> usize {
    ((hz / sample_rate as f32) * FFT_SIZE as f32).round() as usize
}

fn pipewire_init() {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
}

fn prop<'a>(props: &'a pw::spa::utils::dict::DictRef, key: &str) -> Option<&'a str> {
    props
        .iter()
        .find_map(|(k, v)| if k == key { Some(v) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothing_attacks_immediately_and_releases_slowly() {
        assert_eq!(smooth(0.2, 0.8, 0.7), 0.8);
        let released = smooth(0.8, 0.2, 0.7);
        assert!(released > 0.2);
        assert!(released < 0.8);
    }

    #[test]
    fn outputs_link_explicitly_and_inputs_autoconnect() {
        // Output devices and output streams are tapped via explicit
        // port links so a suspended sink can't make us drift onto the
        // default sink's monitor. Input devices/streams stay on
        // autoconnect, which captures a real source with its native
        // channel layout.
        let output_device = test_node(AudioClass::Device {
            direction: Direction::Output,
        });
        let output_stream = test_node(AudioClass::Stream {
            direction: Direction::Output,
        });
        let input_device = test_node(AudioClass::Device {
            direction: Direction::Input,
        });
        let input_stream = test_node(AudioClass::Stream {
            direction: Direction::Input,
        });
        assert_eq!(
            meter_route_for(&output_device),
            Some(MeterRoute::LinkFromOutputs)
        );
        assert_eq!(
            meter_route_for(&output_stream),
            Some(MeterRoute::LinkFromOutputs)
        );
        assert_eq!(
            meter_route_for(&input_device),
            Some(MeterRoute::AutoConnect)
        );
        assert_eq!(
            meter_route_for(&input_stream),
            Some(MeterRoute::AutoConnect)
        );
    }

    fn test_node(class: AudioClass) -> AudioNode {
        AudioNode {
            id: 1,
            serial: 1,
            class,
            name: "test".into(),
            description: "test".into(),
            application: None,
            media_name: None,
            target: None,
            volume: None,
            media_role: None,
        }
    }

    #[test]
    fn dsp_role_streams_are_not_metered() {
        // pavucontrol's per-node peak-detect captures show up as
        // Stream/Input/Audio with media.role=DSP. Without the guard in
        // meter_route_for they'd each get their own capture stream and
        // double our fd usage whenever pavucontrol is open.
        let mut pavucontrol_meter = test_node(AudioClass::Stream {
            direction: Direction::Input,
        });
        pavucontrol_meter.media_role = Some("DSP".into());
        assert_eq!(meter_route_for(&pavucontrol_meter), None);
    }
}

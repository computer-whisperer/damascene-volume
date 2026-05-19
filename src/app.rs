use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;

use aetna_core::prelude::{Image, ImageFit, image};
use aetna_core::*;

use crate::backend::AudioBackend;
use crate::levels::{LevelService, NodeLevels, SpectrumSnapshot};
use crate::model::{
    AudioCard, AudioClass, AudioNode, AudioSnapshot, Direction, ProfileAvailability, Tab, Volume,
};
use crate::util::parse_name_json;

pub const MAX_VOLUME_PERCENT: u32 = 150;

/// Sentinel `value` used by the per-stream target dropdown's "Default
/// — automatic routing" entry. Must not collide with any real
/// `node.name` (no PipeWire node ever uses this string).
const TARGET_DEFAULT_VALUE: &str = "__aetna_default__";
/// Sentinel `value` used by the spectrum-source dropdown's "Follow
/// default output" entry. Distinct from the target sentinel so a stray
/// value from one dropdown can't accidentally satisfy the other.
const SPECTRUM_DEFAULT_VALUE: &str = "__aetna_spectrum_default__";
const WATERFALL_WIDTH: u32 = 256;
const WATERFALL_HEIGHT: u32 = 96;

/// App branding mark shown in the header. Gradients render via Aetna's
/// per-vertex colour bake so the authored linear/radial gradients land
/// as drawn; SVG filters (feDropShadow on this asset) are silently
/// dropped, which is fine — it's just the soft shadow under the knob.
static APP_ICON: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../icon.svg")).expect("icon.svg parses"));

/// Pin glyph shown next to a stream's target dropdown when the
/// stream is pinned (has an explicit `target.object`) — vs the
/// default-following state, which is the common case and gets no
/// marker. Lucide-style strokes in a 24×24 viewBox, `currentColor`
/// fill so [`El::text_color`] tints it.
static PIN_ICON: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="17" x2="12" y2="22"/><path d="M5 17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V6h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1v4.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24Z"/></svg>"#,
    )
    .expect("pin icon svg parses")
});

/// Which node the spectrogram should listen to. `DefaultOutput` is the
/// historical behaviour (follow the current `default.audio.sink`) and
/// stays the startup default so users who never touch the picker see
/// the same display they had before. `Node(id)` pins a specific
/// PipeWire global id; if that id later disappears from the snapshot
/// the resolver falls back to the default output silently rather than
/// blanking the spectrogram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpectrumSource {
    DefaultOutput,
    Node(u32),
}

pub struct VolumeApp {
    pub backend: Box<dyn AudioBackend>,
    pub active_tab: Tab,
    /// The latest snapshot, mirrored from the backend at the top of
    /// every `build` so the UI tracks PipeWire registry changes within
    /// one frame. Behind a `RefCell` because `App::build` is `&self`.
    pub snapshot: RefCell<AudioSnapshot>,
    /// Per-node optimistic overrides applied on top of the live
    /// snapshot. Cleared once the backend snapshot catches up so
    /// external changes can flow through.
    pub volume_overrides: RefCell<HashMap<u32, u32>>,
    pub mute_overrides: RefCell<HashMap<u32, bool>>,
    /// Per-card optimistic profile override, by card id → profile
    /// index. Some PipeWire devices accept a profile write but don't
    /// emit a `Profile` param event back, so we track the user's pick
    /// locally to keep the highlight responsive.
    pub profile_overrides: RefCell<HashMap<u32, u32>>,
    /// Per-stream optimistic target override, by stream id → either
    /// the picked device's `object.serial`, or `None` meaning "Default
    /// (automatic routing)". Cleared by [`resolve_stream_target`]
    /// once the live snapshot agrees, so an external `target.object`
    /// change (e.g. pavucontrol re-routing the same stream) flows
    /// through instead of staying masked by our own pick.
    pub target_overrides: RefCell<HashMap<u32, Option<u64>>>,
    /// Which card's profile dropdown is currently open. Single shared
    /// slot — only one menu can be open at a time and the click-outside
    /// scrim closes it before another can open.
    pub profile_dropdown_open: RefCell<Option<u32>>,
    /// Which stream's target-device dropdown is currently open. Same
    /// shared-slot rule as `profile_dropdown_open`.
    pub target_dropdown_open: RefCell<Option<u32>>,
    /// Which node the spectrogram is currently listening to. Picked by
    /// the spectrum-card dropdown; `DefaultOutput` means "track the
    /// system default sink" so the display follows when the user
    /// switches default devices.
    pub spectrum_source: RefCell<SpectrumSource>,
    /// Whether the spectrum-source dropdown is open. Single boolean —
    /// there's only one spectrum picker on screen, so we don't need
    /// the `Option<id>` shape the per-row dropdowns use.
    pub spectrum_dropdown_open: RefCell<bool>,
    pub levels: RefCell<LevelService>,
}

impl VolumeApp {
    pub fn new(backend: Box<dyn AudioBackend>) -> Self {
        let snapshot = backend.refresh();
        let mut levels = LevelService::new();
        levels.ensure_visible(
            &snapshot.nodes_for_tab(Tab::Playback),
            default_output_node(&snapshot),
        );
        Self {
            backend,
            active_tab: Tab::Playback,
            snapshot: RefCell::new(snapshot),
            volume_overrides: RefCell::new(HashMap::new()),
            mute_overrides: RefCell::new(HashMap::new()),
            profile_overrides: RefCell::new(HashMap::new()),
            target_overrides: RefCell::new(HashMap::new()),
            profile_dropdown_open: RefCell::new(None),
            target_dropdown_open: RefCell::new(None),
            spectrum_source: RefCell::new(SpectrumSource::DefaultOutput),
            spectrum_dropdown_open: RefCell::new(false),
            levels: RefCell::new(levels),
        }
    }

    /// Resolve the configured [`SpectrumSource`] against the current
    /// snapshot. A `Node(id)` pick that no longer exists in the
    /// snapshot falls through to the default-output choice silently —
    /// hot-unplugging a USB headset shouldn't blank the display, and
    /// the dropdown trigger label will read "Default Output" again,
    /// signalling the fallback.
    fn spectrum_source_node<'a>(&self, snapshot: &'a AudioSnapshot) -> Option<&'a AudioNode> {
        match *self.spectrum_source.borrow() {
            SpectrumSource::DefaultOutput => default_output_node(snapshot),
            SpectrumSource::Node(id) => snapshot
                .nodes
                .iter()
                .find(|n| n.id == id)
                .or_else(|| default_output_node(snapshot)),
        }
    }

    pub fn with_active_tab(mut self, tab: Tab) -> Self {
        self.active_tab = tab;
        self
    }

    /// Pull the latest snapshot from the backend and reconcile meter
    /// threads. Called once per frame from `build`. Only the nodes
    /// shown in the active tab get a meter — the Configuration tab
    /// gets none, and switching tabs tears down the previous tab's
    /// meters. This keeps fd / thread / PipeWire-link usage
    /// proportional to what's on screen rather than to the whole
    /// graph.
    fn sync_state(&self) {
        let snapshot = self.backend.refresh();
        let visible = snapshot.nodes_for_tab(self.active_tab);
        let spectrum_node = self.spectrum_source_node(&snapshot);
        self.levels
            .borrow_mut()
            .ensure_visible(&visible, spectrum_node);
        *self.snapshot.borrow_mut() = snapshot;
    }

    fn percent_for(&self, node: &AudioNode) -> u32 {
        let snapshot_pct = node.volume.as_ref().map(Volume::percent);
        let override_pct = self.volume_overrides.borrow().get(&node.id).copied();
        match (override_pct, snapshot_pct) {
            (Some(o), Some(s)) if o.abs_diff(s) <= 1 => {
                self.volume_overrides.borrow_mut().remove(&node.id);
                s
            }
            (Some(o), _) => o,
            (None, Some(s)) => s,
            (None, None) => 100,
        }
    }

    fn active_profile_for(&self, card: &AudioCard) -> Option<u32> {
        let snapshot = card.active_profile;
        let override_val = self.profile_overrides.borrow().get(&card.id).copied();
        match (override_val, snapshot) {
            (Some(o), Some(s)) if o == s => {
                self.profile_overrides.borrow_mut().remove(&card.id);
                Some(s)
            }
            (Some(o), _) => Some(o),
            (None, s) => s,
        }
    }

    fn muted_for(&self, node: &AudioNode) -> bool {
        let snapshot_mute = node.volume.as_ref().map(|v| v.muted);
        let override_mute = self.mute_overrides.borrow().get(&node.id).copied();
        match (override_mute, snapshot_mute) {
            (Some(o), Some(s)) if o == s => {
                self.mute_overrides.borrow_mut().remove(&node.id);
                s
            }
            (Some(o), _) => o,
            (None, Some(s)) => s,
            (None, None) => false,
        }
    }

    fn scrub_from_event(&self, event: &UiEvent, id: u32) {
        let (Some(target), Some((x, _))) = (&event.target, event.pointer) else {
            return;
        };
        let pct = slider_percent_from_x(target.rect, x);
        self.volume_overrides.borrow_mut().insert(id, pct);
        self.backend.set_volume(id, pct as f32 / 100.0);
    }

    /// Step the volume on a focused-slider key event. Arrow keys move
    /// by 1pct; PageUp/PageDown by 10pct (the volume range maxes out
    /// at 150, so 1/150 ≈ 0.67% per arrow press in normalized space).
    fn adjust_volume_from_key(&self, event: &UiEvent, key: &str, id: u32) {
        let current_pct = {
            let snapshot = self.snapshot.borrow();
            let Some(node) = snapshot.nodes.iter().find(|n| n.id == id) else {
                return;
            };
            self.percent_for(node)
        };
        let mut normalized = current_pct as f32 / 150.0;
        if aetna_core::widgets::slider::apply_event(
            &mut normalized,
            event,
            key,
            1.0 / 150.0,
            10.0 / 150.0,
        ) {
            let new_pct = (normalized * 150.0).round() as u32;
            if new_pct != current_pct {
                self.volume_overrides.borrow_mut().insert(id, new_pct);
                self.backend.set_volume(id, new_pct as f32 / 100.0);
            }
        }
    }

    fn set_default(&self, id: u32) {
        let snapshot = self.snapshot.borrow();
        let Some(node) = snapshot.nodes.iter().find(|n| n.id == id) else {
            return;
        };
        match node.class {
            AudioClass::Device {
                direction: Direction::Output,
            } => self.backend.set_default_sink(&node.name),
            AudioClass::Device {
                direction: Direction::Input,
            } => self.backend.set_default_source(&node.name),
            _ => {}
        }
    }

    fn handle_target_event(&mut self, event: &UiEvent, stream_id: u32) {
        let key = format!("target:{stream_id}");
        let Some(action) = aetna_core::widgets::select::classify_event(event, &key) else {
            return;
        };
        match action {
            SelectAction::Toggle => {
                let mut open = self.target_dropdown_open.borrow_mut();
                *open = if *open == Some(stream_id) {
                    None
                } else {
                    // Closing any other open dropdown isn't necessary —
                    // there's only one slot per kind — but guard against
                    // a half-open state by clobbering this slot.
                    Some(stream_id)
                };
            }
            SelectAction::Dismiss => {
                *self.target_dropdown_open.borrow_mut() = None;
            }
            SelectAction::Pick(value) => {
                let target_serial: Option<u64> = if value == TARGET_DEFAULT_VALUE {
                    None
                } else {
                    // The select_menu was populated with each device's
                    // serial as its option value, so parsing back is
                    // total — anything that doesn't parse means the
                    // dropdown shape drifted from the picker source.
                    match value.parse::<u64>() {
                        Ok(serial) => Some(serial),
                        Err(_) => return,
                    }
                };
                self.backend.set_stream_target(stream_id, target_serial);
                self.target_overrides
                    .borrow_mut()
                    .insert(stream_id, target_serial);
                *self.target_dropdown_open.borrow_mut() = None;
            }
            _ => {}
        }
    }

    fn handle_spectrum_event(&mut self, event: &UiEvent) {
        let Some(action) = aetna_core::widgets::select::classify_event(event, "spectrum") else {
            return;
        };
        match action {
            SelectAction::Toggle => {
                let mut open = self.spectrum_dropdown_open.borrow_mut();
                *open = !*open;
            }
            SelectAction::Dismiss => {
                *self.spectrum_dropdown_open.borrow_mut() = false;
            }
            SelectAction::Pick(value) => {
                let next = if value == SPECTRUM_DEFAULT_VALUE {
                    SpectrumSource::DefaultOutput
                } else {
                    // Options are populated with each node's PipeWire
                    // global id as the value token. Anything that
                    // doesn't parse signals a drift between the option
                    // list and this decoder — drop it rather than
                    // pinning the source to a bogus value.
                    match value.parse::<u32>() {
                        Ok(id) => SpectrumSource::Node(id),
                        Err(_) => return,
                    }
                };
                *self.spectrum_source.borrow_mut() = next;
                *self.spectrum_dropdown_open.borrow_mut() = false;
            }
            _ => {}
        }
    }

    fn handle_profile_event(&mut self, event: &UiEvent, card_id: u32) {
        let key = format!("profile:{card_id}");
        let Some(action) = aetna_core::widgets::select::classify_event(event, &key) else {
            return;
        };
        match action {
            SelectAction::Toggle => {
                let mut open = self.profile_dropdown_open.borrow_mut();
                *open = if *open == Some(card_id) {
                    None
                } else {
                    Some(card_id)
                };
            }
            SelectAction::Dismiss => {
                *self.profile_dropdown_open.borrow_mut() = None;
            }
            SelectAction::Pick(value) => {
                let Ok(profile_index) = value.parse::<u32>() else {
                    return;
                };
                self.profile_overrides
                    .borrow_mut()
                    .insert(card_id, profile_index);
                self.backend.set_card_profile(card_id, profile_index);
                *self.profile_dropdown_open.borrow_mut() = None;
            }
            // SelectAction is `#[non_exhaustive]` so future variants
            // need a default; no-op is right for events we don't act on.
            _ => {}
        }
    }

    fn toggle_mute(&self, id: u32) {
        let current = self
            .mute_overrides
            .borrow()
            .get(&id)
            .copied()
            .unwrap_or_else(|| {
                self.snapshot
                    .borrow()
                    .nodes
                    .iter()
                    .find(|node| node.id == id)
                    .and_then(|node| node.volume.as_ref())
                    .map(|v| v.muted)
                    .unwrap_or(false)
            });
        let new_muted = !current;
        self.mute_overrides.borrow_mut().insert(id, new_muted);
        self.backend.set_mute(id, new_muted);
    }
}

impl App for VolumeApp {
    fn theme(&self) -> Theme {
        Theme::radix_slate_blue_dark()
    }

    fn build(&self, _cx: &BuildCx) -> El {
        self.sync_state();
        let snapshot = self.snapshot.borrow();
        let content = match self.active_tab {
            Tab::Configuration => configuration_panel(&snapshot.cards, self),
            tab => node_panel(snapshot.nodes_for_tab(tab), tab, self),
        };
        let spectrum_node = self.spectrum_source_node(&snapshot);
        let (spectrum_snapshot, meter_count) = {
            let levels = self.levels.borrow();
            (
                spectrum_node.and_then(|node| levels.spectrum_for(node.id)),
                levels.active_meter_count(),
            )
        };
        let spectrum_source = *self.spectrum_source.borrow();

        let main = column([
            header(&snapshot),
            tab_bar(self.active_tab),
            content.width(Size::Fill(1.0)).height(Size::Fill(1.0)),
            spectrum_card(spectrum_node, spectrum_snapshot, spectrum_source),
            status_bar(&snapshot, meter_count),
        ])
        .gap(tokens::SPACE_4)
        .padding(tokens::SPACE_4)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));

        // Profile select dropdown — popovers compose at the root of the
        // El tree (see aetna_core widgets::popover docs), so the menu
        // for whichever card is currently open is a sibling of the main
        // column. Only the configuration tab can open one, and only one
        // can be open at a time.
        let profile_menu = if self.active_tab == Tab::Configuration
            && let Some(card_id) = *self.profile_dropdown_open.borrow()
            && let Some(card) = snapshot.cards.iter().find(|c| c.id == card_id)
        {
            let options: Vec<(String, String)> = card
                .profiles
                .iter()
                .map(|p| {
                    let label = if p.available == ProfileAvailability::No {
                        format!("{} · unavailable", p.description)
                    } else {
                        p.description.clone()
                    };
                    (p.index.to_string(), label)
                })
                .collect();
            Some(select_menu(format!("profile:{card_id}"), options))
        } else {
            None
        };

        // Per-stream target picker. Only the Playback / Recording tabs
        // can open one — the other tabs don't render stream rows.
        let target_menu = if matches!(self.active_tab, Tab::Playback | Tab::Recording)
            && let Some(stream_id) = *self.target_dropdown_open.borrow()
            && let Some(stream) = snapshot.nodes.iter().find(|n| n.id == stream_id)
            && let AudioClass::Stream { direction } = stream.class
        {
            // Annotate the "Default" option with the current default
            // device's name, so picking it tells the user where the
            // stream will land — and pre-warns them that switching
            // the system default later will follow it here too.
            let default_name = match direction {
                Direction::Output => snapshot.default_sink_name.as_deref(),
                Direction::Input => snapshot.default_source_name.as_deref(),
            };
            let default_label = default_name
                .and_then(|name| {
                    snapshot.nodes.iter().find(|n| {
                        n.name == name
                            && matches!(n.class, AudioClass::Device { direction: d } if d == direction)
                    })
                })
                .map(|d| format!("Default — {}", d.description))
                .unwrap_or_else(|| "Default — automatic".to_string());
            let mut options: Vec<(String, String)> =
                vec![(TARGET_DEFAULT_VALUE.to_string(), default_label)];
            options.extend(
                snapshot
                    .nodes
                    .iter()
                    .filter(|n| matches!(n.class, AudioClass::Device { direction: d } if d == direction))
                    .map(|n| (n.serial.to_string(), n.description.clone())),
            );
            Some(select_menu(format!("target:{stream_id}"), options))
        } else {
            None
        };

        // Spectrum source picker. Available on every tab — the
        // spectrogram card itself is shown everywhere — so we don't
        // gate this on `active_tab` the way the per-row pickers do.
        let spectrum_menu = if *self.spectrum_dropdown_open.borrow() {
            Some(select_menu(
                "spectrum",
                spectrum_source_options(&snapshot),
            ))
        } else {
            None
        };

        overlays(main, [profile_menu, target_menu, spectrum_menu]).fill_size()
    }

    fn on_event(&mut self, event: UiEvent) {
        // Tabs row first — `tabs::apply_event` filters on Click/Activate
        // and the `{key}:tab:{value}` route shape, so it can run ahead
        // of the per-key dispatch below without conflicting with the
        // other prefixes (`mute:`, `volume:`, `profile:`, …).
        if aetna_core::widgets::tabs::apply_event(
            &mut self.active_tab,
            &event,
            "tabs",
            Tab::from_token,
        ) {
            return;
        }

        let Some(key) = event.key.as_deref() else {
            return;
        };
        match event.kind {
            UiEventKind::Click | UiEventKind::Activate => {
                if key == "refresh" {
                    self.volume_overrides.borrow_mut().clear();
                    self.mute_overrides.borrow_mut().clear();
                } else if let Some(id) = node_id_from_key(key, "mute:") {
                    self.toggle_mute(id);
                } else if let Some(id) = node_id_from_key(key, "default:") {
                    self.set_default(id);
                } else if let Some(card_id) = card_id_for_profile_select(key) {
                    self.handle_profile_event(&event, card_id);
                } else if let Some(stream_id) = stream_id_for_target_select(key) {
                    self.handle_target_event(&event, stream_id);
                } else if is_spectrum_select_key(key) {
                    self.handle_spectrum_event(&event);
                } else if let Some(id) = node_id_from_key(key, "volume:") {
                    self.scrub_from_event(&event, id);
                }
            }
            UiEventKind::PointerDown | UiEventKind::Drag => {
                if let Some(id) = node_id_from_key(key, "volume:") {
                    self.scrub_from_event(&event, id);
                }
            }
            UiEventKind::KeyDown => {
                if let Some(id) = node_id_from_key(key, "volume:") {
                    self.adjust_volume_from_key(&event, key, id);
                }
            }
            _ => {}
        }
    }
}

fn header(snapshot: &AudioSnapshot) -> El {
    row([
        icon(APP_ICON.clone())
            .icon_size(72.0)
            .width(Size::Fixed(72.0)),
        column([
            h1("Volume Control"),
            text(snapshot.server_name.as_deref().unwrap_or("PipeWire"))
                .muted()
                .label(),
        ])
        .gap(tokens::SPACE_1)
        .width(Size::Fill(1.0)),
        button_with_icon("refresh-cw", "Refresh")
            .secondary()
            .key("refresh"),
    ])
    .gap(tokens::SPACE_3)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

fn tab_bar(active: Tab) -> El {
    tabs_list(
        "tabs",
        &active,
        Tab::ALL.into_iter().map(|tab| (tab, tab.label())),
    )
}

fn node_panel(nodes: Vec<&AudioNode>, tab: Tab, app: &VolumeApp) -> El {
    let snapshot = app.snapshot.borrow();
    let rows = if nodes.is_empty() {
        vec![empty_state(tab)]
    } else {
        nodes
            .into_iter()
            .map(|node| {
                let is_stream = matches!(node.class, AudioClass::Stream { .. });
                let (target_label, target_pinned) = if is_stream {
                    let resolved = resolve_stream_target(&snapshot, node, app);
                    let label = format_target_label(&resolved);
                    (Some(label), !resolved.following)
                } else {
                    (None, false)
                };
                node_row(
                    node,
                    app.percent_for(node),
                    app.muted_for(node),
                    snapshot.is_default(node),
                    app.levels.borrow().level_for(node.id),
                    target_label,
                    target_pinned,
                )
            })
            .collect()
    };

    column([
        panel_title(tab.label(), tab_subtitle(tab)),
        scroll(rows).key("node-list").height(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_3)
}

/// Resolved routing state for a stream's target dropdown trigger.
/// `following` says whether the stream is configured to follow the
/// session default (no `target.object` pin), so the UI can mark it
/// clearly — if the system default changes, these are the streams
/// that will move with it. `device` is what the label actually points
/// at: the live link-graph peer when known (matches pavucontrol's
/// "where is this stream actually playing through?"), falling back
/// to the metadata pin or the system default device when there's no
/// live link yet (paused stream, just registered, etc.).
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamTarget<'a> {
    /// True when the stream has no `target.object` pin (and the
    /// optimistic override agrees). Drives the absence of the pin
    /// icon in the row.
    pub following: bool,
    /// True when the stream *actually* tracks the system default —
    /// i.e. it's following AND its resolved device is the current
    /// `default.audio.sink` / `default.audio.source` for its
    /// direction. False for the loopback exception (following but
    /// session policy has diverted it to a non-default device), in
    /// which case the row should NOT claim it'll move when the
    /// default changes. Drives the "default →" tag on the trigger
    /// label.
    pub tracks_default: bool,
    pub device: Option<&'a AudioNode>,
}

/// Format the trigger label for a stream's target dropdown. The
/// device name is always the live destination (matches pavucontrol);
/// streams that actually track the system default get a "default →"
/// prefix so the row reads as "this will move if the default
/// changes". Streams that are following but session-policy-diverted
/// (the loopback exception) get the device name alone — no prefix,
/// because they *won't* track a default change. Pinned streams also
/// get the device name alone; their pinned status is shown by the
/// row's pin icon, not the label.
pub(crate) fn format_target_label(resolved: &StreamTarget<'_>) -> String {
    match resolved.device {
        Some(d) if resolved.tracks_default => format!("default → {}", d.description),
        Some(d) => d.description.clone(),
        // Cold-start edge: nothing resolved yet. Don't claim a default
        // tag we can't back up.
        None => "Default".to_string(),
    }
}

/// Test-only wrapper that goes through the same path as production.
#[cfg(test)]
fn target_label_for_stream(snapshot: &AudioSnapshot, node: &AudioNode, app: &VolumeApp) -> String {
    format_target_label(&resolve_stream_target(snapshot, node, app))
}

pub(crate) fn resolve_stream_target<'a>(
    snapshot: &'a AudioSnapshot,
    node: &AudioNode,
    app: &VolumeApp,
) -> StreamTarget<'a> {
    let direction = match &node.class {
        AudioClass::Stream { direction } => *direction,
        _ => {
            return StreamTarget {
                following: false,
                tracks_default: false,
                device: None,
            };
        }
    };
    let device_with_serial = |serial: u64| -> Option<&AudioNode> {
        snapshot.nodes.iter().find(|n| {
            n.serial == serial
                && matches!(n.class, AudioClass::Device { direction: d } if d == direction)
        })
    };
    let device_with_name = |name: &str| -> Option<&AudioNode> {
        snapshot.nodes.iter().find(|n| {
            n.name == name
                && matches!(n.class, AudioClass::Device { direction: d } if d == direction)
        })
    };
    let device_with_id = |id: u32| -> Option<&AudioNode> {
        snapshot.nodes.iter().find(|n| {
            n.id == id
                && matches!(n.class, AudioClass::Device { direction: d } if d == direction)
        })
    };
    let default_device = || -> Option<&AudioNode> {
        let name = match direction {
            Direction::Output => snapshot.default_sink_name.as_deref(),
            Direction::Input => snapshot.default_source_name.as_deref(),
        };
        name.and_then(device_with_name)
    };

    // The metadata pin — what `target.object` (or legacy `node.target`)
    // requests for this stream. This is the *intent*, not where audio
    // actually flows; a stream with no pin still routes somewhere via
    // WirePlumber's default policy.
    let metadata_target = node
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|raw| {
            if let Ok(serial) = raw.parse::<u64>() {
                device_with_serial(serial)
            } else if let Some(name) = parse_name_json(raw) {
                device_with_name(name)
            } else {
                None
            }
        });
    // The live peer — the actual device this stream is linked to in
    // the PipeWire graph, regardless of intent. Streams that fan out
    // to multiple devices (rare; manual `pw-link` setups) get the
    // first device-class peer; pavucontrol behaves the same way.
    let live_peer = snapshot
        .peers
        .get(&node.id)
        .and_then(|peer_ids| peer_ids.iter().copied().find_map(device_with_id));

    let override_val = app.target_overrides.borrow().get(&node.id).copied();

    // Reconcile any optimistic override against the metadata pin —
    // that's the only field our writes actually touch. Using the live
    // peer here would mis-drop a "Default" override (`o == None`)
    // immediately, because WirePlumber re-routes to *some* real
    // device after the pin is cleared.
    let metadata_pin_serial = metadata_target.map(|n| n.serial);
    let active_override = match override_val {
        Some(o) if o == metadata_pin_serial => {
            app.target_overrides.borrow_mut().remove(&node.id);
            None
        }
        other => other,
    };

    let (following, device): (bool, Option<&AudioNode>) = match active_override {
        Some(None) => (
            true,
            // Prefer the live peer over the configured default. They
            // match for typical streams (Firefox → default sink). For
            // the loopback exception they diverge: my-source has no
            // pin, the system default IS my-sink, but session policy
            // routes my-source to UMC202HD to avoid a self-loop —
            // showing "my-sink" there would lie about routing (audio
            // can't actually flow into my-sink without looping). Falls
            // back to the configured default only when there's no
            // live link yet (paused stream, mid-route).
            live_peer.or_else(default_device),
        ),
        Some(Some(serial)) => (false, device_with_serial(serial)),
        None => match metadata_target {
            Some(pinned) => (
                false,
                // Pinned rows prefer the live peer too — if the pin
                // has drifted off the actual routing, the live peer
                // is the diagnostic the user cares about. Falls back
                // to the pin when no live link exists yet.
                live_peer.or(Some(pinned)),
            ),
            None => (true, live_peer.or_else(default_device)),
        },
    };

    // The stream truly tracks the default only when it's both
    // unpinned AND the resolved device IS the current default for
    // this direction. The loopback-exception case (following but the
    // live peer differs from the default sink) deliberately reads
    // `tracks_default = false` so the UI doesn't claim my-source
    // will move when the default does — it won't, because session
    // policy will keep diverting it.
    let tracks_default = following
        && device.is_some_and(|d| default_device().is_some_and(|def| def.id == d.id));

    StreamTarget {
        following,
        tracks_default,
        device,
    }
}

fn tab_subtitle(tab: Tab) -> &'static str {
    match tab {
        Tab::Playback => "Apps sending audio to a sink.",
        Tab::Recording => "Apps capturing audio from a source.",
        Tab::Outputs => "Audio sinks — speakers, headphones, virtual outputs.",
        Tab::Inputs => "Audio sources — microphones, line-in, virtual inputs.",
        Tab::Configuration => "Cards, profiles, and ports.",
    }
}

fn default_output_node(snapshot: &AudioSnapshot) -> Option<&AudioNode> {
    snapshot.nodes.iter().find(|node| {
        snapshot.default_sink_name.as_deref() == Some(node.name.as_str())
            && matches!(
                node.class,
                AudioClass::Device {
                    direction: Direction::Output
                }
            )
    })
}

fn spectrum_card(
    node: Option<&AudioNode>,
    spectrum: Option<SpectrumSnapshot>,
    source: SpectrumSource,
) -> El {
    let status = match (&node, &spectrum) {
        (Some(_), Some(spectrum)) if !spectrum.columns.is_empty() => {
            format!("{} Hz", spectrum.sample_rate)
        }
        (Some(_), _) => "Listening".to_string(),
        (None, _) => "No source selected".to_string(),
    };
    let subtitle = node
        .map(|node| node.description.as_str())
        .unwrap_or("Pick a source from the dropdown");

    card([
        row([
            row([
                icon("activity")
                    .icon_size(18.0)
                    .text_color(tokens::PRIMARY)
                    .width(Size::Fixed(24.0)),
                column([
                    text("Spectrogram").label(),
                    text(subtitle).caption().muted().ellipsis(),
                ])
                .gap(2.0)
                .width(Size::Fill(1.0)),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
            select_trigger("spectrum", spectrum_trigger_label(source, node))
                .width(Size::Fixed(240.0)),
            badge(status),
        ])
        .gap(tokens::SPACE_3)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
        row([
            column([
                text("18k").caption().mono().muted(),
                spacer(),
                text("1k").caption().mono().muted(),
                spacer(),
                text("35").caption().mono().muted(),
            ])
            .align(Align::End)
            .height(Size::Fixed(96.0))
            .width(Size::Fixed(32.0)),
            column([
                image(waterfall_image(spectrum.as_ref()))
                    .image_fit(ImageFit::Fill)
                    .radius(6.0)
                    .width(Size::Fill(1.0))
                    .height(Size::Fixed(96.0)),
                row([
                    text("-6s").caption().mono().muted(),
                    spacer(),
                    text("now").caption().mono().muted(),
                ])
                .width(Size::Fill(1.0)),
            ])
            .gap(5.0)
            .width(Size::Fill(1.0)),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_3)
    .padding(tokens::SPACE_3)
    .height(Size::Fixed(184.0))
}

fn waterfall_image(spectrum: Option<&SpectrumSnapshot>) -> Image {
    let mut pixels = vec![0_u8; (WATERFALL_WIDTH * WATERFALL_HEIGHT * 4) as usize];
    let columns = spectrum.map(|s| s.columns.as_slice()).unwrap_or(&[]);
    let bins = spectrum.map(|s| s.bins).unwrap_or(72).max(1);
    let draw_start_x = WATERFALL_WIDTH.saturating_sub(columns.len() as u32);

    for y in 0..WATERFALL_HEIGHT {
        for x in 0..WATERFALL_WIDTH {
            let column_index = if x < draw_start_x {
                None
            } else {
                Some((x - draw_start_x) as usize)
            };
            let bin = ((WATERFALL_HEIGHT - 1 - y) as usize * bins / WATERFALL_HEIGHT as usize)
                .min(bins - 1);
            let value = column_index
                .and_then(|i| columns.get(i))
                .and_then(|column| column.get(bin))
                .copied()
                .unwrap_or(0.0);
            let mut color = waterfall_color(value);
            if x % 32 == 0 || y % 24 == 0 {
                color = blend_rgba(color, [42, 55, 70, 255], 0.34);
            }
            if x + 1 == WATERFALL_WIDTH {
                color = blend_rgba(color, [68, 167, 210, 255], 0.45);
            }
            let offset = ((y * WATERFALL_WIDTH + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&color);
        }
    }

    Image::from_rgba8(WATERFALL_WIDTH, WATERFALL_HEIGHT, pixels)
}

fn waterfall_color(value: f32) -> [u8; 4] {
    let value = value.clamp(0.0, 1.0).powf(0.78);
    let stops = [
        (0.00, [11, 16, 24]),
        (0.18, [18, 31, 45]),
        (0.42, [34, 91, 145]),
        (0.68, [46, 177, 184]),
        (0.88, [210, 178, 86]),
        (1.00, [245, 236, 190]),
    ];
    for pair in stops.windows(2) {
        let (a_pos, a) = pair[0];
        let (b_pos, b) = pair[1];
        if value <= b_pos {
            let t = ((value - a_pos) / (b_pos - a_pos)).clamp(0.0, 1.0);
            return [
                lerp_u8(a[0], b[0], t),
                lerp_u8(a[1], b[1], t),
                lerp_u8(a[2], b[2], t),
                255,
            ];
        }
    }
    [245, 236, 190, 255]
}

fn blend_rgba(base: [u8; 4], overlay: [u8; 4], amount: f32) -> [u8; 4] {
    [
        lerp_u8(base[0], overlay[0], amount),
        lerp_u8(base[1], overlay[1], amount),
        lerp_u8(base[2], overlay[2], amount),
        255,
    ]
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

fn configuration_panel(cards: &[AudioCard], app: &VolumeApp) -> El {
    let rows = if cards.is_empty() {
        vec![text("No PipeWire cards discovered yet.").muted()]
    } else {
        cards
            .iter()
            .map(|card| card_row(card, app.active_profile_for(card)))
            .collect()
    };

    column([
        panel_title(Tab::Configuration.label(), tab_subtitle(Tab::Configuration)),
        scroll(rows).key("cards").height(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_3)
}

fn panel_title(title: &'static str, subtitle: &'static str) -> El {
    column([h2(title), text(subtitle).muted().caption()])
        .gap(tokens::SPACE_1)
        .width(Size::Fill(1.0))
}

fn node_row(
    node: &AudioNode,
    volume: u32,
    muted: bool,
    is_default: bool,
    levels: Option<NodeLevels>,
    target_label: Option<String>,
    target_pinned: bool,
) -> El {
    let title = node
        .application
        .as_deref()
        .or(node.media_name.as_deref())
        .unwrap_or(&node.description);
    let is_device = matches!(node.class, AudioClass::Device { .. });

    // Streams get an interactive target picker in place of the static
    // "#id  target" caption; devices keep the caption with the
    // registry-time target string.
    let secondary: El = match &target_label {
        Some(label) => {
            // Pin marker sits between the id and the dropdown trigger
            // when the stream is pinned. The slot is always present
            // (fixed-width spacer when unpinned) so rows align
            // vertically regardless of which streams are pinned.
            let pin_slot: El = if target_pinned {
                icon(PIN_ICON.clone())
                    .icon_size(14.0)
                    .text_color(tokens::PRIMARY)
                    .width(Size::Fixed(16.0))
            } else {
                spacer().width(Size::Fixed(16.0)).height(Size::Fixed(1.0))
            };
            row([
                text(format!("#{id}", id = node.id))
                    .caption()
                    .mono()
                    .muted()
                    .width(Size::Fixed(48.0)),
                pin_slot,
                select_trigger(format!("target:{}", node.id), label.clone())
                    .width(Size::Fill(1.0)),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0))
        }
        None => text(format!(
            "#{id}  {target}",
            id = node.id,
            target = node.target.as_deref().unwrap_or("No route"),
        ))
        .caption()
        .muted()
        .ellipsis(),
    };

    let mut children: Vec<El> = vec![
        icon(if muted { "x" } else { "activity" })
            .icon_size(20.0)
            .text_color(if muted {
                tokens::DESTRUCTIVE
            } else {
                tokens::PRIMARY
            })
            .width(Size::Fixed(32.0)),
        column([
            // Title ellipsizes when the device/app name is long enough
            // to overrun the column (real PipeWire descriptions like
            // "Family 17h/19h/1ah HD Audio Controller Analog Stereo"
            // would otherwise spill into the meter and badge columns).
            // .ellipsis() only takes effect when the box is constrained,
            // hence Fill width on a column that itself has Fill width.
            text(title).label().ellipsis().width(Size::Fill(1.0)),
            secondary,
        ])
        .gap(tokens::SPACE_1)
        .width(Size::Fill(1.0)),
        activity_meter(levels.as_ref(), muted).width(Size::Fixed(98.0)),
        volume_slider(node.id, volume, muted).width(Size::Fixed(180.0)),
        text(format!("{volume}%"))
            .mono()
            .label()
            .width(Size::Fixed(50.0)),
        button(if muted { "Unmute" } else { "Mute" })
            .secondary()
            .key(format!("mute:{}", node.id))
            .width(Size::Fixed(82.0)),
    ];

    // Default-device action lives in its own column at the right edge
    // of the row (only for devices). Fixed width so the badge variant
    // and the "Set Default" variant occupy the same horizontal slot —
    // otherwise their differing intrinsic widths shift the Mute button
    // and break vertical alignment between rows.
    if is_device {
        children.push(
            if is_default {
                default_indicator()
            } else {
                button("Set Default")
                    .secondary()
                    .key(format!("default:{}", node.id))
            }
            .width(Size::Fixed(100.0)),
        );
    }

    card([row(children)
        .gap(tokens::SPACE_3)
        .align(Align::Center)
        .padding(tokens::SPACE_3)
        .width(Size::Fill(1.0))
        .height(Size::Fixed(88.0))])
}

fn card_row(audio_card: &AudioCard, active_profile: Option<u32>) -> El {
    let active_label = active_profile
        .and_then(|idx| audio_card.profiles.iter().find(|p| p.index == idx))
        .map(|p| p.description.as_str())
        .unwrap_or("No active profile");

    let header = row([
        icon("settings").icon_size(20.0).width(Size::Fixed(32.0)),
        column([
            text(&audio_card.description).label(),
            text(format!(
                "#{id}  {name}",
                id = audio_card.id,
                name = audio_card.name
            ))
            .caption()
            .muted()
            .ellipsis(),
        ])
        .gap(tokens::SPACE_1)
        .width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_3)
    .align(Align::Center);

    let profile_picker: El = if audio_card.profiles.is_empty() {
        text("No profiles enumerated yet.").caption().muted()
    } else {
        select_trigger(format!("profile:{}", audio_card.id), active_label).width(Size::Fill(1.0))
    };

    card([
        header,
        row([
            text("Profile").label().muted().width(Size::Fixed(80.0)),
            profile_picker,
        ])
        .gap(tokens::SPACE_3)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_3)
    .padding(tokens::SPACE_3)
}

fn volume_slider(id: u32, percent: u32, muted: bool) -> El {
    let fill = if muted {
        tokens::MUTED_FOREGROUND
    } else {
        tokens::PRIMARY
    };
    let normalized = (percent as f32 / MAX_VOLUME_PERCENT as f32).clamp(0.0, 1.0);
    slider(normalized, fill).key(format!("volume:{id}"))
}

fn activity_meter(levels: Option<&NodeLevels>, muted: bool) -> El {
    let channels = levels
        .map(|levels| levels.channel_count().clamp(1, 2))
        .unwrap_or(2);
    column(
        (0..channels)
            .map(|channel| {
                let label = match (channels, channel) {
                    (1, _) => "M",
                    (_, 0) => "L",
                    (_, 1) => "R",
                    _ => "",
                };
                meter_channel(
                    label,
                    levels.map(|l| l.peak(channel)).unwrap_or(0.0),
                    levels.map(|l| l.rms(channel)).unwrap_or(0.0),
                    muted,
                )
            })
            .collect::<Vec<_>>(),
    )
    .gap(4.0)
    .width(Size::Fill(1.0))
}

fn meter_channel(label: &'static str, peak: f32, rms: f32, muted: bool) -> El {
    row([
        text(label)
            .caption()
            .mono()
            .muted()
            .width(Size::Fixed(12.0)),
        meter_bar(peak, rms, muted).width(Size::Fill(1.0)),
    ])
    .gap(5.0)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

fn meter_bar(peak: f32, rms: f32, muted: bool) -> El {
    let peak = level_to_meter(peak);
    let rms = level_to_meter(rms);
    let fill = if muted {
        tokens::MUTED_FOREGROUND
    } else {
        tokens::SUCCESS
    };
    stack([
        El::new(Kind::Custom("activity-track"))
            .fill(tokens::MUTED)
            .radius(tokens::RADIUS_PILL),
        El::new(Kind::Custom("activity-rms"))
            .fill(fill.with_alpha(70))
            .radius(tokens::RADIUS_PILL),
        El::new(Kind::Custom("activity-peak"))
            .fill(fill)
            .radius(tokens::RADIUS_PILL),
    ])
    .layout(move |ctx| {
        let rect = ctx.container;
        vec![
            rect,
            Rect::new(rect.x, rect.y, rect.w * rms, rect.h),
            Rect::new(rect.x, rect.y, rect.w * peak, rect.h),
        ]
    })
    .height(Size::Fixed(6.0))
    .width(Size::Fill(1.0))
}

fn level_to_meter(value: f32) -> f32 {
    if value <= 0.000_1 {
        0.0
    } else {
        ((20.0 * value.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    }
}

fn node_id_from_key(key: &str, prefix: &str) -> Option<u32> {
    key.strip_prefix(prefix)?.parse().ok()
}

/// Match the per-card profile-select key shape (`profile:{card_id}`,
/// plus `:dismiss` / `:option:{idx}` suffixes the popover layer adds)
/// and pull the card id out so the routed event can be dispatched
/// against the controlled select with a card-scoped key. Decoding the
/// routed action is left to
/// [`aetna_core::widgets::select::classify_event`].
fn card_id_for_profile_select(key: &str) -> Option<u32> {
    let rest = key.strip_prefix("profile:")?;
    rest.split(':').next()?.parse().ok()
}

/// Same shape as [`card_id_for_profile_select`], but for the
/// per-stream target picker (`target:{stream_id}` + select suffixes).
fn stream_id_for_target_select(key: &str) -> Option<u32> {
    let rest = key.strip_prefix("target:")?;
    rest.split(':').next()?.parse().ok()
}

/// Match the spectrum-source select's routed keys: the trigger
/// (`spectrum`), the dismiss scrim (`spectrum:dismiss`), and any
/// option click (`spectrum:option:{value}`). Used to gate dispatch
/// in `on_event` so this select's events don't fall through to the
/// volume-slider branch and other key handlers.
fn is_spectrum_select_key(key: &str) -> bool {
    key == "spectrum" || key.starts_with("spectrum:")
}

/// Display label for the spectrum-source dropdown trigger. When
/// [`SpectrumSource::DefaultOutput`] resolves to a real device, the
/// trigger reads "Default → <device>" so the user can see which
/// device the "follow default" choice currently points at. Pinned
/// picks show the device description verbatim.
fn spectrum_trigger_label(source: SpectrumSource, resolved: Option<&AudioNode>) -> String {
    match (source, resolved) {
        (SpectrumSource::DefaultOutput, Some(node)) => {
            format!("Default → {}", node.description)
        }
        (SpectrumSource::DefaultOutput, None) => "Default Output".to_string(),
        // `Node` pins fall back to the default-output resolver when
        // the pinned node has vanished. `resolved` therefore can't
        // distinguish the two on its own — but the source value can,
        // and we surface the fallback by name so the user can tell
        // why the picker label changed.
        (SpectrumSource::Node(_), Some(node)) => node.description.clone(),
        (SpectrumSource::Node(_), None) => "Unavailable".to_string(),
    }
}

/// Options shown in the spectrum-source dropdown. The first entry is
/// always the "Default Output" sentinel; the rest are every meterable
/// node in the snapshot (devices + streams) sorted by tab affinity so
/// the user sees outputs grouped above streams above inputs. We use
/// the PipeWire global id as the option's value token so the
/// pick-back path is a simple `parse::<u32>`.
fn spectrum_source_options(snapshot: &AudioSnapshot) -> Vec<(String, String)> {
    let mut options: Vec<(String, String)> =
        vec![(SPECTRUM_DEFAULT_VALUE.to_string(), "Default Output".into())];
    let class_rank = |class: &AudioClass| -> u8 {
        match class {
            AudioClass::Device {
                direction: Direction::Output,
            } => 0,
            AudioClass::Stream {
                direction: Direction::Output,
            } => 1,
            AudioClass::Device {
                direction: Direction::Input,
            } => 2,
            AudioClass::Stream {
                direction: Direction::Input,
            } => 3,
            _ => 4,
        }
    };
    let mut nodes: Vec<&AudioNode> = snapshot
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.class,
                AudioClass::Device { .. } | AudioClass::Stream { .. }
            )
        })
        .collect();
    nodes.sort_by(|a, b| {
        class_rank(&a.class)
            .cmp(&class_rank(&b.class))
            .then_with(|| a.description.cmp(&b.description))
    });
    options.extend(nodes.into_iter().map(|n| {
        let prefix = match n.class {
            AudioClass::Device {
                direction: Direction::Output,
            } => "Output",
            AudioClass::Stream {
                direction: Direction::Output,
            } => "Playback",
            AudioClass::Device {
                direction: Direction::Input,
            } => "Input",
            AudioClass::Stream {
                direction: Direction::Input,
            } => "Recording",
            _ => "Other",
        };
        let label = n
            .application
            .as_deref()
            .or(n.media_name.as_deref())
            .unwrap_or(&n.description);
        (n.id.to_string(), format!("{prefix} · {label}"))
    }));
    options
}

pub fn slider_percent_from_x(rect: Rect, x: f32) -> u32 {
    let normalized = aetna_core::widgets::slider::normalized_from_event(rect, x);
    (normalized * MAX_VOLUME_PERCENT as f32).round() as u32
}

/// Read-only "this is the default" indicator placed in the device-row
/// action slot so the user doesn't mistake it for a clickable button.
/// The badge sits centered inside the same fixed-width slot the
/// "Set Default" button occupies, so the Mute column stays vertically
/// aligned across rows regardless of which variant a row shows.
fn default_indicator() -> El {
    row([badge("Default")])
        .align(Align::Center)
        .justify(Justify::Center)
}

fn empty_state(tab: Tab) -> El {
    card([
        icon("info")
            .icon_size(28.0)
            .text_color(tokens::MUTED_FOREGROUND),
        text(format!("No {} streams or devices yet.", tab.label()))
            .label()
            .center_text(),
        text("This panel will update as PipeWire graph discovery lands.")
            .caption()
            .muted()
            .center_text(),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .justify(Justify::Center)
    .height(Size::Fixed(180.0))
}

fn plural(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

fn status_bar(snapshot: &AudioSnapshot, meter_count: usize) -> El {
    row([
        text(format!(
            "{} · {} · {}",
            plural(snapshot.nodes.len(), "node", "nodes"),
            plural(snapshot.cards.len(), "card", "cards"),
            plural(meter_count, "meter", "meters"),
        ))
        .caption()
        .muted()
        .width(Size::Fill(1.0)),
        text(snapshot.error.as_deref().unwrap_or("Ready"))
            .caption()
            .muted(),
    ])
    .width(Size::Fill(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_percent_tracks_thumb_center() {
        use aetna_core::widgets::slider::THUMB_SIZE;
        let rect = Rect::new(10.0, 20.0, 220.0, 18.0);
        let left = rect.x + THUMB_SIZE * 0.5;
        let usable = rect.w - THUMB_SIZE;
        assert_eq!(slider_percent_from_x(rect, left), 0);
        assert_eq!(slider_percent_from_x(rect, left + usable * 0.5), 75);
        assert_eq!(slider_percent_from_x(rect, left + usable), 150);
        assert_eq!(slider_percent_from_x(rect, rect.x - 30.0), 0);
        assert_eq!(slider_percent_from_x(rect, rect.x + rect.w + 30.0), 150);
    }

    #[test]
    fn open_dropdown_inserts_popover_layer_at_root() {
        // Smoke test for the open-state path: when a card's dropdown
        // is open, `build()` adds a select_menu sibling next to the
        // main column. Closed-state regressions show up as the popover
        // disappearing from the tree.
        use crate::backend::DemoBackend;
        let mut app = VolumeApp::new(Box::new(DemoBackend)).with_active_tab(Tab::Configuration);
        let card_id = app
            .snapshot
            .borrow()
            .cards
            .first()
            .map(|c| c.id)
            .expect("DemoBackend exposes at least one card");
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        // Closed: only the main column at the root.
        let closed = app.build(&cx);
        assert_eq!(closed.children.len(), 1, "closed: just the main layer");

        // Open the dropdown for the first card.
        let toggle = profile_click_event(card_id, "");
        app.handle_profile_event(&toggle, card_id);
        let opened = app.build(&cx);
        assert_eq!(opened.children.len(), 2, "open: main + popover at the root");
        // Popover scrim's dismiss key matches the trigger key suffix.
        let popover = &opened.children[1];
        let scrim = &popover.children[0];
        assert_eq!(
            scrim.key.as_deref(),
            Some(format!("profile:{card_id}:dismiss").as_str())
        );

        // Toggling again closes.
        app.handle_profile_event(&toggle, card_id);
        assert_eq!(app.build(&cx).children.len(), 1);
    }

    #[test]
    fn card_id_for_profile_select_decodes_per_card_key() {
        // The trigger key (`profile:{card}`), the dismiss scrim
        // (`profile:{card}:dismiss`), and the option click
        // (`profile:{card}:option:{idx}`) should all yield the same
        // card id — `classify_event` then handles the action shape.
        assert_eq!(card_id_for_profile_select("profile:7"), Some(7));
        assert_eq!(card_id_for_profile_select("profile:7:dismiss"), Some(7));
        assert_eq!(card_id_for_profile_select("profile:7:option:3"), Some(7));
        // Unrelated keys (other widget routes) don't match.
        assert_eq!(card_id_for_profile_select("mute:7"), None);
        assert_eq!(card_id_for_profile_select("profile:abc"), None);
    }

    #[test]
    fn spectrum_select_key_matcher_covers_trigger_and_routed_suffixes() {
        // Trigger, dismiss scrim, and option keys must all match so
        // `on_event` routes them to the spectrum handler instead of
        // letting them fall through to the volume-slider branch.
        assert!(is_spectrum_select_key("spectrum"));
        assert!(is_spectrum_select_key("spectrum:dismiss"));
        assert!(is_spectrum_select_key("spectrum:option:42"));
        assert!(is_spectrum_select_key(&format!(
            "spectrum:option:{SPECTRUM_DEFAULT_VALUE}"
        )));
        // Unrelated routes don't accidentally short-circuit. A key
        // like `spectrumless:5` would be unfortunate but the
        // `starts_with("spectrum:")` boundary keeps it out.
        assert!(!is_spectrum_select_key("target:42"));
        assert!(!is_spectrum_select_key("spectrumless:5"));
    }

    #[test]
    fn pinned_node_id_falls_back_to_default_when_node_disappears() {
        // Hot-unplug case: a USB headset the user pinned the
        // spectrogram to vanishes from the snapshot. The resolver
        // must not return `None` (which would blank the display) —
        // it should fall back to the current default output so the
        // spectrogram keeps working until the user picks again.
        let app = fixture_app();
        *app.spectrum_source.borrow_mut() = SpectrumSource::Node(99_999);
        let snapshot = app.snapshot.borrow().clone();
        let resolved = app.spectrum_source_node(&snapshot);
        let default = default_output_node(&snapshot);
        assert!(default.is_some(), "DemoBackend exposes a default output");
        assert_eq!(
            resolved.map(|n| n.id),
            default.map(|n| n.id),
            "stale pin falls through to the default-output resolver",
        );
    }

    #[test]
    fn spectrum_event_default_pick_resets_source() {
        // After picking a node and then re-picking "Default Output",
        // the source must be back to DefaultOutput so the display
        // resumes tracking the system default sink. The default
        // sentinel is distinct from any numeric id and decoded back
        // to the enum variant by `handle_spectrum_event`.
        use aetna_core::widgets::select::select_option_key;
        let mut app = fixture_app();
        let pick_node = UiEvent::synthetic_click(select_option_key("spectrum", &42u32));
        app.handle_spectrum_event(&pick_node);
        assert_eq!(*app.spectrum_source.borrow(), SpectrumSource::Node(42));

        let pick_default =
            UiEvent::synthetic_click(select_option_key("spectrum", &SPECTRUM_DEFAULT_VALUE));
        app.handle_spectrum_event(&pick_default);
        assert_eq!(
            *app.spectrum_source.borrow(),
            SpectrumSource::DefaultOutput,
        );
    }

    #[test]
    fn stream_id_for_target_select_decodes_per_stream_key() {
        assert_eq!(stream_id_for_target_select("target:42"), Some(42));
        assert_eq!(stream_id_for_target_select("target:42:dismiss"), Some(42));
        assert_eq!(
            stream_id_for_target_select("target:42:option:alsa_output.foo"),
            Some(42),
        );
        // Don't collide with the profile picker shape, even though
        // both use a single-prefix-then-id key form.
        assert_eq!(stream_id_for_target_select("profile:42"), None);
        assert_eq!(stream_id_for_target_select("target:abc"), None);
    }

    fn profile_click_event(card_id: u32, suffix: &str) -> UiEvent {
        let key = if suffix.is_empty() {
            format!("profile:{card_id}")
        } else {
            format!("profile:{card_id}:{suffix}")
        };
        UiEvent::synthetic_click(key)
    }

    /// Build a minimal snapshot: one output stream, one output device.
    /// `target` and `peers` are filled in by the caller so each test
    /// can exercise a specific combination of metadata vs. live links.
    fn fixture_snapshot(stream_target: Option<&str>, peer_ids: Vec<u32>) -> AudioSnapshot {
        let mut snap = AudioSnapshot::default();
        snap.nodes.push(AudioNode {
            id: 100,
            serial: 7100,
            class: AudioClass::Stream {
                direction: Direction::Output,
            },
            name: "firefox".into(),
            description: "Firefox".into(),
            application: None,
            media_name: None,
            target: stream_target.map(str::to_string),
            media_role: None,
            volume: None,
        });
        snap.nodes.push(AudioNode {
            id: 200,
            serial: 7200,
            class: AudioClass::Device {
                direction: Direction::Output,
            },
            name: "alsa_output.physical".into(),
            description: "Physical Speakers".into(),
            application: None,
            media_name: None,
            target: None,
            media_role: None,
            volume: None,
        });
        if !peer_ids.is_empty() {
            snap.peers.insert(100, peer_ids);
        }
        snap
    }

    fn fixture_app() -> VolumeApp {
        use crate::backend::DemoBackend;
        VolumeApp::new(Box::new(DemoBackend))
    }

    #[test]
    fn live_peer_resolves_when_no_metadata_pin() {
        // Reproduces the my-source case from the bug report: a stream
        // with no `target.object` but a live link to a physical sink.
        // The label shows the live destination, and `following=true`
        // tells the UI to suppress the pin icon (the row's "this is
        // pinned, won't move with default" signal).
        let snap = fixture_snapshot(None, vec![200]);
        let app = fixture_app();
        let stream = &snap.nodes[0];
        let resolved = resolve_stream_target(&snap, stream, &app);
        assert_eq!(resolved.device.map(|n| n.id), Some(200));
        assert!(resolved.following, "no metadata pin → following default");
        assert_eq!(
            target_label_for_stream(&snap, stream, &app),
            "Physical Speakers"
        );
    }

    #[test]
    fn live_peer_takes_priority_over_metadata_pin() {
        // If the metadata pin is stale (points to a device the stream
        // is no longer linked to), the displayed label should follow
        // the live graph — that's the source of truth pavucontrol
        // shows, and it's what users actually care about.
        let mut snap = fixture_snapshot(Some("9999"), vec![200]);
        snap.nodes.push(AudioNode {
            id: 300,
            serial: 9999,
            class: AudioClass::Device {
                direction: Direction::Output,
            },
            name: "alsa_output.other".into(),
            description: "Other Output".into(),
            application: None,
            media_name: None,
            target: None,
            media_role: None,
            volume: None,
        });
        let app = fixture_app();
        let resolved = resolve_stream_target(&snap, &snap.nodes[0], &app);
        assert_eq!(resolved.device.map(|n| n.id), Some(200));
    }

    #[test]
    fn metadata_pin_used_when_no_live_peer() {
        // A registered-but-unlinked stream (paused, or briefly
        // mid-route) should still show its pinned destination instead
        // of falling all the way back to "Default".
        let snap = fixture_snapshot(Some("7200"), vec![]);
        let app = fixture_app();
        let resolved = resolve_stream_target(&snap, &snap.nodes[0], &app);
        assert_eq!(resolved.device.map(|n| n.id), Some(200));
    }

    #[test]
    fn default_override_is_not_dropped_by_live_peer_alone() {
        // Picking "Default" stores `target_overrides[id] = None`. The
        // live peer will keep showing a physical device because
        // WirePlumber's default policy routes there — that must not
        // trick reconciliation into dropping the override. The pick
        // is reconciled against the metadata pin, which we just
        // cleared.
        let snap = fixture_snapshot(None, vec![200]);
        let app = fixture_app();
        app.target_overrides.borrow_mut().insert(100, None);
        let resolved = resolve_stream_target(&snap, &snap.nodes[0], &app);
        assert_eq!(resolved.device.map(|n| n.id), Some(200));
        // Override was satisfied (metadata pin is None, matches our
        // pick) so it should have been dropped — future external
        // routing changes flow through.
        assert!(!app.target_overrides.borrow().contains_key(&100));
    }

    #[test]
    fn explicit_override_overrides_live_peer_until_reconciled() {
        // User picks device 300; the metadata write hasn't propagated
        // yet and the live link still points at device 200. The
        // optimistic override should win so the label updates
        // immediately on click instead of lagging.
        let mut snap = fixture_snapshot(None, vec![200]);
        snap.nodes.push(AudioNode {
            id: 300,
            serial: 9300,
            class: AudioClass::Device {
                direction: Direction::Output,
            },
            name: "alsa_output.headphones".into(),
            description: "Headphones".into(),
            application: None,
            media_name: None,
            target: None,
            media_role: None,
            volume: None,
        });
        let app = fixture_app();
        app.target_overrides.borrow_mut().insert(100, Some(9300));
        let resolved = resolve_stream_target(&snap, &snap.nodes[0], &app);
        assert_eq!(resolved.device.map(|n| n.id), Some(300));
        // Override hasn't been satisfied yet (metadata still empty)
        // so it must persist for subsequent frames.
        assert!(app.target_overrides.borrow().contains_key(&100));
    }

    #[test]
    fn pinned_stream_is_marked_following_false() {
        // Streams with a `target.object` pin are *not* following the
        // session default — moving the default later won't move them.
        // The label itself is just the device name (same as for
        // following streams); the distinction is carried by the
        // `following` flag (drives the pin icon) and the absence of
        // `tracks_default` (no "default →" tag even when the pinned
        // device happens to also be the system default).
        let mut snap = fixture_snapshot(Some("7200"), vec![200]);
        // Set the default to the same device the stream is pinned to
        // — `tracks_default` must still be false because the stream
        // wouldn't *move* if the default changed, it's nailed down.
        snap.default_sink_name = Some("alsa_output.physical".into());
        let app = fixture_app();
        let stream = &snap.nodes[0];
        let resolved = resolve_stream_target(&snap, stream, &app);
        assert!(!resolved.following, "metadata pin → not following default");
        assert!(
            !resolved.tracks_default,
            "pinned streams never carry the 'default →' tag, even when pinned to the default device"
        );
        assert_eq!(
            target_label_for_stream(&snap, stream, &app),
            "Physical Speakers"
        );
    }

    #[test]
    fn following_with_no_live_peer_falls_back_to_system_default() {
        // Stream has no pin and no live link yet (e.g. paused / just
        // registered). The trigger should still tell the user where
        // the stream is *configured* to end up — the current system
        // default device — so they can decide whether to leave it
        // alone or pin it. Without this we'd render bare "Default",
        // losing the prior version's signal.
        let mut snap = fixture_snapshot(None, vec![]);
        snap.default_sink_name = Some("alsa_output.physical".into());
        let app = fixture_app();
        let stream = &snap.nodes[0];
        let resolved = resolve_stream_target(&snap, stream, &app);
        assert!(resolved.following);
        assert!(
            resolved.tracks_default,
            "resolved device IS the system default → label gets the 'default →' tag"
        );
        assert_eq!(resolved.device.map(|n| n.id), Some(200));
        assert_eq!(
            target_label_for_stream(&snap, stream, &app),
            "default → Physical Speakers"
        );
    }

    #[test]
    fn following_with_no_peer_and_no_default_renders_bare_default() {
        // Cold-start edge case: stream is following default, has no
        // live link, and we haven't received the default-sink event
        // yet. Bare "Default" is the only honest thing to show.
        let snap = fixture_snapshot(None, vec![]);
        let app = fixture_app();
        let stream = &snap.nodes[0];
        let resolved = resolve_stream_target(&snap, stream, &app);
        assert!(resolved.following);
        assert!(resolved.device.is_none());
        assert_eq!(target_label_for_stream(&snap, stream, &app), "Default");
    }

    #[test]
    fn following_diverged_stream_does_not_get_default_tag() {
        // The whole reason `tracks_default` exists: my-source is
        // following default (no `target.object`), but session policy
        // has routed it to a non-default device to avoid a self-loop.
        // Changing the system default will NOT move my-source — the
        // policy will keep diverting it — so the row must not carry
        // the "default →" tag that claims it would.
        let mut snap = fixture_snapshot(None, vec![200]);
        snap.default_sink_name = Some("loopback_sink".into());
        snap.nodes.push(AudioNode {
            id: 400,
            serial: 7400,
            class: AudioClass::Device {
                direction: Direction::Output,
            },
            name: "loopback_sink".into(),
            description: "my-sink".into(),
            application: None,
            media_name: None,
            target: None,
            media_role: None,
            volume: None,
        });
        let app = fixture_app();
        let resolved = resolve_stream_target(&snap, &snap.nodes[0], &app);
        assert!(resolved.following, "no pin → following");
        assert!(
            !resolved.tracks_default,
            "peer (Physical Speakers) ≠ default (my-sink) → no 'default →' tag"
        );
        assert_eq!(
            target_label_for_stream(&snap, &snap.nodes[0], &app),
            "Physical Speakers"
        );
    }

    #[test]
    fn following_label_shows_live_peer_when_it_diverges_from_default() {
        // The loopback playback-half case: my-source is following
        // default (no `target.object`), the system default sink is
        // my-sink, but session policy routes it to a physical device
        // to avoid a self-loop. The label must show the *live peer*,
        // not the configured default — showing "my-sink" here would
        // be a lie about routing (audio can't actually loop into
        // my-sink). The unpinned status is communicated by the row's
        // missing pin icon, not by the label string.
        let mut snap = fixture_snapshot(None, vec![200]);
        snap.default_sink_name = Some("loopback_sink".into());
        snap.nodes.push(AudioNode {
            id: 400,
            serial: 7400,
            class: AudioClass::Device {
                direction: Direction::Output,
            },
            name: "loopback_sink".into(),
            description: "my-sink".into(),
            application: None,
            media_name: None,
            target: None,
            media_role: None,
            volume: None,
        });
        let app = fixture_app();
        let stream = &snap.nodes[0];
        let resolved = resolve_stream_target(&snap, stream, &app);
        assert!(resolved.following);
        assert_eq!(
            resolved.device.map(|n| n.id),
            Some(200),
            "live peer (Physical Speakers) wins over the configured default (my-sink) — the latter would be circular"
        );
        assert_eq!(
            target_label_for_stream(&snap, stream, &app),
            "Physical Speakers"
        );
    }

    #[test]
    fn input_streams_follow_default_source_not_sink() {
        // Direction matters: a recording stream that's following
        // default needs the system *source* default, not the sink
        // default. Wiring them up backwards would always render the
        // wrong fallback label for mic streams.
        let mut snap = AudioSnapshot {
            default_sink_name: Some("not_a_real_source".into()),
            default_source_name: Some("alsa_input.mic".into()),
            ..AudioSnapshot::default()
        };
        snap.nodes.push(AudioNode {
            id: 50,
            serial: 5050,
            class: AudioClass::Stream {
                direction: Direction::Input,
            },
            name: "obs-capture".into(),
            description: "OBS Studio".into(),
            application: None,
            media_name: None,
            target: None,
            media_role: None,
            volume: None,
        });
        snap.nodes.push(AudioNode {
            id: 60,
            serial: 6060,
            class: AudioClass::Device {
                direction: Direction::Input,
            },
            name: "alsa_input.mic".into(),
            description: "Studio Mic".into(),
            application: None,
            media_name: None,
            target: None,
            media_role: None,
            volume: None,
        });
        let app = fixture_app();
        let resolved = resolve_stream_target(&snap, &snap.nodes[0], &app);
        assert!(
            resolved.tracks_default,
            "resolved device IS the default source → 'default →' tag applies"
        );
        assert_eq!(resolved.device.map(|n| n.id), Some(60));
        assert_eq!(
            target_label_for_stream(&snap, &snap.nodes[0], &app),
            "default → Studio Mic"
        );
    }
}

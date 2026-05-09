use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;

use aetna_core::*;

use crate::backend::AudioBackend;
use crate::levels::{LevelService, NodeLevels};
use crate::model::{
    AudioCard, AudioClass, AudioNode, AudioSnapshot, Direction, ProfileAvailability, Tab, Volume,
};
use crate::util::parse_name_json;

pub const MAX_VOLUME_PERCENT: u32 = 150;

/// Sentinel `value` used by the per-stream target dropdown's "Default
/// — automatic routing" entry. Must not collide with any real
/// `node.name` (no PipeWire node ever uses this string).
const TARGET_DEFAULT_VALUE: &str = "__aetna_default__";

/// App branding mark shown in the header. Gradients render via Aetna's
/// per-vertex colour bake so the authored linear/radial gradients land
/// as drawn; SVG filters (feDropShadow on this asset) are silently
/// dropped, which is fine — it's just the soft shadow under the knob.
static APP_ICON: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../icon.svg")).expect("icon.svg parses"));

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
    /// (automatic routing)". WirePlumber's metadata server doesn't
    /// echo `target.object` writes back to clients, so we never see a
    /// confirmation event — the override sticks for the session.
    pub target_overrides: RefCell<HashMap<u32, Option<u64>>>,
    /// Which card's profile dropdown is currently open. Single shared
    /// slot — only one menu can be open at a time and the click-outside
    /// scrim closes it before another can open.
    pub profile_dropdown_open: RefCell<Option<u32>>,
    /// Which stream's target-device dropdown is currently open. Same
    /// shared-slot rule as `profile_dropdown_open`.
    pub target_dropdown_open: RefCell<Option<u32>>,
    pub levels: RefCell<LevelService>,
}

impl VolumeApp {
    pub fn new(backend: Box<dyn AudioBackend>) -> Self {
        let snapshot = backend.refresh();
        let mut levels = LevelService::new();
        levels.ensure_visible(&snapshot.nodes_for_tab(Tab::Playback));
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
            levels: RefCell::new(levels),
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
        self.levels.borrow_mut().ensure_visible(&visible);
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

        let main = column([
            header(&snapshot),
            tab_bar(self.active_tab),
            content.width(Size::Fill(1.0)).height(Size::Fill(1.0)),
            status_bar(&snapshot, self.levels.borrow().active_meter_count()),
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
            let mut options: Vec<(String, String)> = vec![(
                TARGET_DEFAULT_VALUE.to_string(),
                "Default — automatic".to_string(),
            )];
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

        overlays(main, [profile_menu, target_menu]).fill_size()
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
                let target_label = matches!(node.class, AudioClass::Stream { .. })
                    .then(|| target_label_for_stream(&snapshot, node, app));
                node_row(
                    node,
                    app.percent_for(node),
                    app.muted_for(node),
                    snapshot.is_default(node),
                    app.levels.borrow().level_for(node.id),
                    target_label,
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

/// Resolve the visible label for a stream's target dropdown trigger.
/// Optimistic override wins; otherwise fall back to the registry-time
/// `target.object` value, which is whatever WirePlumber last wrote —
/// typically a bare `object.serial` string for `Spa:Id` writes, or a
/// `{"name":"..."}` JSON blob for older clients. Returns "Default"
/// when no override is set or the target can't be resolved.
fn target_label_for_stream(snapshot: &AudioSnapshot, node: &AudioNode, app: &VolumeApp) -> String {
    resolved_target_for_stream(snapshot, node, app)
        .map(|n| n.description.clone())
        .unwrap_or_else(|| "Default".to_string())
}

fn resolved_target_for_stream<'a>(
    snapshot: &'a AudioSnapshot,
    node: &AudioNode,
    app: &VolumeApp,
) -> Option<&'a AudioNode> {
    let direction = match &node.class {
        AudioClass::Stream { direction } => *direction,
        _ => return None,
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
    match app.target_overrides.borrow().get(&node.id).copied() {
        Some(None) => None,
        Some(Some(serial)) => device_with_serial(serial),
        None => {
            let raw = node.target.as_deref()?.trim();
            if raw.is_empty() {
                return None;
            }
            if let Ok(serial) = raw.parse::<u64>() {
                device_with_serial(serial)
            } else if let Some(name) = parse_name_json(raw) {
                device_with_name(name)
            } else {
                None
            }
        }
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
        Some(label) => row([
            text(format!("#{id}", id = node.id))
                .caption()
                .mono()
                .muted()
                .width(Size::Fixed(48.0)),
            select_trigger(format!("target:{}", node.id), label.clone()).width(Size::Fill(1.0)),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
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
}

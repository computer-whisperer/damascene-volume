use std::cell::RefCell;
use std::collections::HashMap;

use aetna_core::*;

use crate::backend::AudioBackend;
use crate::levels::{LevelService, NodeLevels};
use crate::model::{
    AudioCard, AudioClass, AudioNode, AudioSnapshot, Direction, ProfileAvailability, Tab, Volume,
};

pub const MAX_VOLUME_PERCENT: u32 = 150;

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
    /// Which card's profile dropdown is currently open. Single shared
    /// slot — only one menu can be open at a time and the click-outside
    /// scrim closes it before another can open.
    pub profile_dropdown_open: RefCell<Option<u32>>,
    pub levels: RefCell<LevelService>,
}

impl VolumeApp {
    pub fn new(backend: Box<dyn AudioBackend>) -> Self {
        let snapshot = backend.refresh();
        let mut levels = LevelService::new();
        levels.ensure_snapshot(&snapshot);
        Self {
            backend,
            active_tab: Tab::Playback,
            snapshot: RefCell::new(snapshot),
            volume_overrides: RefCell::new(HashMap::new()),
            mute_overrides: RefCell::new(HashMap::new()),
            profile_overrides: RefCell::new(HashMap::new()),
            profile_dropdown_open: RefCell::new(None),
            levels: RefCell::new(levels),
        }
    }

    pub fn with_active_tab(mut self, tab: Tab) -> Self {
        self.active_tab = tab;
        self
    }

    /// Pull the latest snapshot from the backend and reconcile meter
    /// threads. Called once per frame from `build`.
    fn sync_state(&self) {
        let snapshot = self.backend.refresh();
        self.levels.borrow_mut().ensure_snapshot(&snapshot);
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
    fn build(&self) -> El {
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
        .gap(tokens::SPACE_LG)
        .padding(tokens::SPACE_LG)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .fill(tokens::BG_APP);

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
        overlays(main, [profile_menu]).fill_size()
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
        column([
            h1("Volume Control"),
            text(snapshot.server_name.as_deref().unwrap_or("PipeWire"))
                .muted()
                .label(),
        ])
        .gap(tokens::SPACE_XS)
        .width(Size::Fill(1.0)),
        button_with_icon("refresh-cw", "Refresh")
            .secondary()
            .key("refresh"),
    ])
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
                node_row(
                    node,
                    app.percent_for(node),
                    app.muted_for(node),
                    snapshot.is_default(node),
                    app.levels.borrow().level_for(node.id),
                )
            })
            .collect()
    };

    column([
        panel_title(tab.label(), tab_subtitle(tab)),
        scroll(rows).key("node-list").height(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_MD)
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
    .gap(tokens::SPACE_MD)
}

fn panel_title(title: &'static str, subtitle: &'static str) -> El {
    column([h2(title), text(subtitle).muted().caption()])
        .gap(tokens::SPACE_XS)
        .width(Size::Fill(1.0))
}

fn node_row(
    node: &AudioNode,
    volume: u32,
    muted: bool,
    is_default: bool,
    levels: Option<NodeLevels>,
) -> El {
    let title = node
        .application
        .as_deref()
        .or(node.media_name.as_deref())
        .unwrap_or(&node.description);
    let target = node.target.as_deref().unwrap_or("No route");
    let is_device = matches!(node.class, AudioClass::Device { .. });

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
            text(format!("#{id}  {target}", id = node.id))
                .caption()
                .muted()
                .ellipsis(),
        ])
        .gap(tokens::SPACE_XS)
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

    row(children)
        .gap(tokens::SPACE_MD)
        .align(Align::Center)
        .padding(tokens::SPACE_MD)
        .width(Size::Fill(1.0))
        .height(Size::Fixed(88.0))
        .fill(tokens::BG_CARD)
        .stroke(tokens::BORDER)
        .radius(tokens::RADIUS_MD)
}

fn card_row(card: &AudioCard, active_profile: Option<u32>) -> El {
    let active_label = active_profile
        .and_then(|idx| card.profiles.iter().find(|p| p.index == idx))
        .map(|p| p.description.as_str())
        .unwrap_or("No active profile");

    let header = row([
        icon("settings").icon_size(20.0).width(Size::Fixed(32.0)),
        column([
            text(&card.description).label(),
            text(format!("#{id}  {name}", id = card.id, name = card.name))
                .caption()
                .muted()
                .ellipsis(),
        ])
        .gap(tokens::SPACE_XS)
        .width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_MD)
    .align(Align::Center);

    let profile_picker: El = if card.profiles.is_empty() {
        text("No profiles enumerated yet.").caption().muted()
    } else {
        select_trigger(format!("profile:{}", card.id), active_label).width(Size::Fill(1.0))
    };

    column([
        header,
        row([
            text("Profile").label().muted().width(Size::Fixed(80.0)),
            profile_picker,
        ])
        .gap(tokens::SPACE_MD)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_MD)
    .padding(tokens::SPACE_MD)
    .width(Size::Fill(1.0))
    .fill(tokens::BG_CARD)
    .stroke(tokens::BORDER)
    .radius(tokens::RADIUS_MD)
}

fn volume_slider(id: u32, percent: u32, muted: bool) -> El {
    let fill = if muted {
        tokens::TEXT_MUTED_FOREGROUND
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
        tokens::TEXT_MUTED_FOREGROUND
    } else {
        tokens::SUCCESS
    };
    stack([
        El::new(Kind::Custom("activity-track"))
            .fill(tokens::BG_MUTED)
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

pub fn slider_percent_from_x(rect: Rect, x: f32) -> u32 {
    let normalized = aetna_core::widgets::slider::normalized_from_event(rect, x);
    (normalized * MAX_VOLUME_PERCENT as f32).round() as u32
}

/// Read-only "this is the default" indicator placed in the device-row
/// action slot so the user doesn't mistake it for a clickable button.
fn default_indicator() -> El {
    text("✓ default")
        .label()
        .center_text()
        .text_color(tokens::PRIMARY)
}

fn empty_state(tab: Tab) -> El {
    column([
        icon("info")
            .icon_size(28.0)
            .text_color(tokens::TEXT_MUTED_FOREGROUND),
        text(format!("No {} streams or devices yet.", tab.label()))
            .label()
            .center_text(),
        text("This panel will update as PipeWire graph discovery lands.")
            .caption()
            .muted()
            .center_text(),
    ])
    .gap(tokens::SPACE_SM)
    .align(Align::Center)
    .justify(Justify::Center)
    .height(Size::Fixed(180.0))
    .width(Size::Fill(1.0))
    .fill(tokens::BG_CARD)
    .stroke(tokens::BORDER)
    .radius(tokens::RADIUS_MD)
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
        // Closed: only the main column at the root.
        let closed = app.build();
        assert_eq!(closed.children.len(), 1, "closed: just the main layer");

        // Open the dropdown for the first card.
        let toggle = profile_click_event(card_id, "");
        app.handle_profile_event(&toggle, card_id);
        let opened = app.build();
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
        assert_eq!(app.build().children.len(), 1);
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

    fn profile_click_event(card_id: u32, suffix: &str) -> UiEvent {
        let key = if suffix.is_empty() {
            format!("profile:{card_id}")
        } else {
            format!("profile:{card_id}:{suffix}")
        };
        UiEvent::synthetic_click(key)
    }
}

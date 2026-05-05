use std::cell::RefCell;
use std::collections::HashMap;

use aetna_core::*;

use crate::backend::AudioBackend;
use crate::levels::{LevelService, NodeLevels};
use crate::model::{
    AudioCard, AudioClass, AudioNode, AudioProfile, AudioSnapshot, Direction, ProfileAvailability,
    Tab, Volume,
};

pub const MAX_VOLUME_PERCENT: u32 = 150;
pub const SLIDER_THUMB_SIZE: f32 = 14.0;
pub const SLIDER_TRACK_HEIGHT: f32 = 10.0;

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

        column([
            header(&snapshot),
            row([
                sidebar(self.active_tab),
                content.width(Size::Fill(1.0)).height(Size::Fill(1.0)),
            ])
            .gap(tokens::SPACE_LG)
            .height(Size::Fill(1.0)),
            status_bar(&snapshot, self.levels.borrow().active_meter_count()),
        ])
        .gap(tokens::SPACE_LG)
        .padding(tokens::SPACE_LG)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .fill(tokens::BG_APP)
    }

    fn on_event(&mut self, event: UiEvent) {
        let Some(key) = event.key.as_deref() else {
            return;
        };
        match event.kind {
            UiEventKind::Click | UiEventKind::Activate => {
                if let Some(tab) = Tab::ALL.into_iter().find(|tab| tab.key() == key) {
                    self.active_tab = tab;
                } else if key == "refresh" {
                    self.volume_overrides.borrow_mut().clear();
                    self.mute_overrides.borrow_mut().clear();
                } else if let Some(id) = node_id_from_key(key, "mute:") {
                    self.toggle_mute(id);
                } else if let Some(id) = node_id_from_key(key, "default:") {
                    self.set_default(id);
                } else if let Some((card_id, profile_index)) = profile_key(key) {
                    self.profile_overrides
                        .borrow_mut()
                        .insert(card_id, profile_index);
                    self.backend.set_card_profile(card_id, profile_index);
                } else if let Some(id) = node_id_from_key(key, "volume:") {
                    self.scrub_from_event(&event, id);
                }
            }
            UiEventKind::PointerDown | UiEventKind::Drag => {
                if let Some(id) = node_id_from_key(key, "volume:") {
                    self.scrub_from_event(&event, id);
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

fn sidebar(active: Tab) -> El {
    column(
        Tab::ALL
            .into_iter()
            .map(|tab| {
                let mut item = button(tab.label())
                    .key(tab.key())
                    .width(Size::Fill(1.0))
                    .justify(Justify::Start);
                if tab == active {
                    item = item.primary();
                } else {
                    item = item.ghost();
                }
                item
            })
            .collect::<Vec<_>>(),
    )
    .gap(tokens::SPACE_XS)
    .padding(tokens::SPACE_SM)
    .width(Size::Fixed(190.0))
    .height(Size::Fill(1.0))
    .fill(tokens::BG_CARD)
    .stroke(tokens::BORDER)
    .radius(tokens::RADIUS_MD)
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
        panel_title(
            tab.label(),
            "Live PipeWire objects will populate this surface.",
        ),
        scroll(rows).key("node-list").height(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_MD)
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
        panel_title("Configuration", "Cards, profiles, and ports."),
        scroll(rows).key("cards").height(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_MD)
}

fn panel_title(title: &'static str, subtitle: &'static str) -> El {
    // Hug height so the panel's scroll sibling claims the remaining
    // vertical space — column default is Fill, which would split the
    // available height 50/50.
    column([h2(title), text(subtitle).muted().caption()])
        .gap(tokens::SPACE_XS)
        .width(Size::Fill(1.0))
        .height(Size::Hug)
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

    let default_action: El = if !is_device {
        text("").width(Size::Fixed(0.0))
    } else if is_default {
        badge("default")
    } else {
        button("Set Default")
            .secondary()
            .key(format!("default:{}", node.id))
    };

    row([
        icon(if muted { "x" } else { "activity" })
            .icon_size(20.0)
            .text_color(if muted {
                tokens::DESTRUCTIVE
            } else {
                tokens::PRIMARY
            })
            .width(Size::Fixed(32.0)),
        column([
            row([text(title).label().width(Size::Fill(1.0)), default_action])
                .gap(tokens::SPACE_SM)
                .align(Align::Center),
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
    ])
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
        text(active_label).caption().muted(),
    ])
    .gap(tokens::SPACE_MD)
    .align(Align::Center);

    let profile_rows: Vec<El> = if card.profiles.is_empty() {
        vec![text("No profiles enumerated yet.").caption().muted()]
    } else {
        card.profiles
            .iter()
            .map(|profile| profile_row(card.id, profile, active_profile))
            .collect()
    };

    column([
        header,
        column(profile_rows).gap(tokens::SPACE_XS).height(Size::Hug),
    ])
    .gap(tokens::SPACE_MD)
    .padding(tokens::SPACE_MD)
    .width(Size::Fill(1.0))
    .height(Size::Hug)
    .fill(tokens::BG_CARD)
    .stroke(tokens::BORDER)
    .radius(tokens::RADIUS_MD)
}

fn profile_row(card_id: u32, profile: &AudioProfile, active: Option<u32>) -> El {
    let is_active = active == Some(profile.index);
    let unavailable = profile.available == ProfileAvailability::No;
    let mut btn = button(profile.description.as_str())
        .key(format!("profile:{card_id}:{idx}", idx = profile.index))
        .width(Size::Fill(1.0))
        .justify(Justify::Start);
    btn = if is_active {
        btn.primary()
    } else if unavailable {
        btn.ghost()
    } else {
        btn.secondary()
    };
    btn
}

fn volume_slider(id: u32, percent: u32, muted: bool) -> El {
    let fill = if muted {
        tokens::TEXT_MUTED_FOREGROUND
    } else {
        tokens::PRIMARY
    };
    let pct = (percent as f32 / MAX_VOLUME_PERCENT as f32).clamp(0.0, 1.0);
    let slider_layout = move |ctx: LayoutCtx| {
        let rect = ctx.container;
        let usable = (rect.w - SLIDER_THUMB_SIZE).max(1.0);
        let track_x = rect.x + SLIDER_THUMB_SIZE * 0.5;
        let track_y = rect.y + (rect.h - SLIDER_TRACK_HEIGHT) * 0.5;
        let thumb_x = rect.x + pct * usable;
        let thumb_y = rect.y + (rect.h - SLIDER_THUMB_SIZE) * 0.5;
        vec![
            Rect::new(track_x, track_y, usable, SLIDER_TRACK_HEIGHT),
            Rect::new(track_x, track_y, pct * usable, SLIDER_TRACK_HEIGHT),
            Rect::new(thumb_x, thumb_y, SLIDER_THUMB_SIZE, SLIDER_THUMB_SIZE),
        ]
    };

    stack([
        El::new(Kind::Custom("meter-track"))
            .height(Size::Fixed(SLIDER_TRACK_HEIGHT))
            .width(Size::Fill(1.0))
            .fill(tokens::BG_MUTED)
            .radius(tokens::RADIUS_PILL),
        El::new(Kind::Custom("meter-fill"))
            .height(Size::Fixed(SLIDER_TRACK_HEIGHT))
            .width(Size::Fill(1.0))
            .fill(fill)
            .radius(tokens::RADIUS_PILL),
        El::new(Kind::Custom("slider-thumb"))
            .width(Size::Fixed(SLIDER_THUMB_SIZE))
            .height(Size::Fixed(SLIDER_THUMB_SIZE))
            .fill(tokens::TEXT_FOREGROUND)
            .stroke(tokens::BORDER)
            .radius(tokens::RADIUS_PILL),
    ])
    .key(format!("volume:{id}"))
    .focusable()
    .layout(slider_layout)
    .height(Size::Fixed(18.0))
    .width(Size::Fill(1.0))
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
    .height(Size::Fixed(8.0))
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

fn profile_key(key: &str) -> Option<(u32, u32)> {
    let rest = key.strip_prefix("profile:")?;
    let (card, index) = rest.split_once(':')?;
    Some((card.parse().ok()?, index.parse().ok()?))
}

pub fn slider_percent_from_x(rect: Rect, x: f32) -> u32 {
    let usable = (rect.w - SLIDER_THUMB_SIZE).max(1.0);
    let local = x - rect.x - SLIDER_THUMB_SIZE * 0.5;
    (local / usable * MAX_VOLUME_PERCENT as f32)
        .round()
        .clamp(0.0, MAX_VOLUME_PERCENT as f32) as u32
}

fn badge(label: impl Into<String>) -> El {
    text(label)
        .caption()
        .padding(Sides::xy(tokens::SPACE_SM, 3.0))
        .fill(tokens::BG_MUTED)
        .stroke(tokens::BORDER)
        .radius(tokens::RADIUS_PILL)
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

fn status_bar(snapshot: &AudioSnapshot, meter_count: usize) -> El {
    row([
        text(format!(
            "{} nodes, {} cards, {} meters",
            snapshot.nodes.len(),
            snapshot.cards.len(),
            meter_count
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
        let rect = Rect::new(10.0, 20.0, 220.0, 18.0);
        let left = rect.x + SLIDER_THUMB_SIZE * 0.5;
        let usable = rect.w - SLIDER_THUMB_SIZE;
        assert_eq!(slider_percent_from_x(rect, left), 0);
        assert_eq!(slider_percent_from_x(rect, left + usable * 0.5), 75);
        assert_eq!(slider_percent_from_x(rect, left + usable), 150);
        assert_eq!(slider_percent_from_x(rect, rect.x - 30.0), 0);
        assert_eq!(slider_percent_from_x(rect, rect.x + rect.w + 30.0), 150);
    }
}

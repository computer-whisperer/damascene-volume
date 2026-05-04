use std::{collections::HashMap, time::Duration};

use aetna_core::*;
use aetna_volume::{
    backend::{AudioBackend, pipewire_native::PipeWireBackend},
    levels::{LevelService, NodeLevels},
    model::{AudioCard, AudioNode, AudioSnapshot, Tab, Volume},
};

const MAX_VOLUME_PERCENT: u32 = 150;
const SLIDER_THUMB_SIZE: f32 = 14.0;
const SLIDER_TRACK_HEIGHT: f32 = 10.0;

struct VolumeApp {
    backend: Box<dyn AudioBackend>,
    snapshot: AudioSnapshot,
    active_tab: Tab,
    volume_overrides: HashMap<u32, u32>,
    levels: LevelService,
}

impl VolumeApp {
    fn new(mut backend: Box<dyn AudioBackend>) -> Self {
        let snapshot = backend.refresh();
        let mut levels = LevelService::new();
        levels.ensure_snapshot(&snapshot);
        Self {
            backend,
            snapshot,
            active_tab: Tab::Playback,
            volume_overrides: HashMap::new(),
            levels,
        }
    }

    fn percent_for(&self, node: &AudioNode) -> u32 {
        self.volume_overrides
            .get(&node.id)
            .copied()
            .or_else(|| node.volume.as_ref().map(Volume::percent))
            .unwrap_or(100)
    }

    fn muted_for(&self, node: &AudioNode) -> bool {
        node.volume.as_ref().map(|v| v.muted).unwrap_or(false)
    }

    fn scrub_from_event(&mut self, event: &UiEvent, id: u32) {
        let (Some(target), Some((x, _))) = (&event.target, event.pointer) else {
            return;
        };
        let pct = slider_percent_from_x(target.rect, x);
        self.volume_overrides.insert(id, pct);
        let muted = self
            .snapshot
            .nodes
            .iter()
            .find(|node| node.id == id)
            .and_then(|node| node.volume.as_ref())
            .map(|v| v.muted)
            .unwrap_or(false);
        if let Some(node) = self.snapshot.node_mut(id) {
            node.volume = Some(Volume::from_percent(pct, muted));
        }
    }

    fn toggle_mute(&mut self, id: u32) {
        if let Some(node) = self.snapshot.node_mut(id) {
            let current = node.volume.clone().unwrap_or(Volume {
                scalar: 1.0,
                muted: false,
            });
            node.volume = Some(Volume {
                muted: !current.muted,
                ..current
            });
        }
    }
}

impl App for VolumeApp {
    fn build(&self) -> El {
        let content = match self.active_tab {
            Tab::Configuration => configuration_panel(&self.snapshot.cards),
            tab => node_panel(self.snapshot.nodes_for_tab(tab), tab, self),
        };

        column([
            header(&self.snapshot),
            row([
                sidebar(self.active_tab),
                content.width(Size::Fill(1.0)).height(Size::Fill(1.0)),
            ])
            .gap(tokens::SPACE_LG)
            .height(Size::Fill(1.0)),
            status_bar(&self.snapshot, self.levels.active_meter_count()),
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
                    self.snapshot = self.backend.refresh();
                    self.volume_overrides.clear();
                    self.levels.ensure_snapshot(&self.snapshot);
                } else if let Some(id) = node_id_from_key(key, "mute:") {
                    self.toggle_mute(id);
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

    fn frame_interval(&self) -> Option<Duration> {
        Some(Duration::from_millis(33))
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
                    app.levels.level_for(node.id),
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

fn configuration_panel(cards: &[AudioCard]) -> El {
    let rows = if cards.is_empty() {
        vec![text("No PipeWire cards discovered yet.").muted()]
    } else {
        cards.iter().map(card_row).collect()
    };

    column([
        panel_title("Configuration", "Cards, profiles, and ports."),
        scroll(rows).key("cards").height(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_MD)
}

fn panel_title(title: &'static str, subtitle: &'static str) -> El {
    row([
        column([h2(title), text(subtitle).muted().caption()])
            .gap(tokens::SPACE_XS)
            .width(Size::Fill(1.0)),
        button("Set Default").secondary(),
    ])
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

fn node_row(node: &AudioNode, volume: u32, muted: bool, levels: Option<NodeLevels>) -> El {
    let title = node
        .application
        .as_deref()
        .or(node.media_name.as_deref())
        .unwrap_or(&node.description);
    let target = node.target.as_deref().unwrap_or("No route");

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
            row([
                text(title).label().width(Size::Fill(1.0)),
                if node.is_default {
                    badge("default")
                } else {
                    text("").width(Size::Fixed(0.0))
                },
            ])
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

fn card_row(card: &AudioCard) -> El {
    column([
        row([
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
            button(
                card.active_profile
                    .as_deref()
                    .unwrap_or("No active profile"),
            )
            .secondary(),
        ])
        .gap(tokens::SPACE_MD)
        .align(Align::Center),
        row(card
            .profiles
            .iter()
            .map(|profile| badge(profile.as_str()))
            .collect::<Vec<_>>())
        .gap(tokens::SPACE_SM),
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

fn slider_percent_from_x(rect: Rect, x: f32) -> u32 {
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let viewport = Rect::new(0.0, 0.0, 980.0, 680.0);
    aetna_demo::run(
        "Aetna Volume",
        viewport,
        VolumeApp::new(Box::new(PipeWireBackend::new())),
    )
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

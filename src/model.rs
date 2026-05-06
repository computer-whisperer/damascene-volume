use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tab {
    Playback,
    Recording,
    Outputs,
    Inputs,
    Configuration,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Playback,
        Tab::Recording,
        Tab::Outputs,
        Tab::Inputs,
        Tab::Configuration,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Playback => "Playback",
            Tab::Recording => "Recording",
            Tab::Outputs => "Output Devices",
            Tab::Inputs => "Input Devices",
            Tab::Configuration => "Configuration",
        }
    }

    /// Stable lowercase token used as the value side of the
    /// `tabs_list` API and reused by the `render_artifacts` bin for
    /// per-tab artifact filenames.
    pub fn token(self) -> &'static str {
        match self {
            Tab::Playback => "playback",
            Tab::Recording => "recording",
            Tab::Outputs => "outputs",
            Tab::Inputs => "inputs",
            Tab::Configuration => "configuration",
        }
    }

    /// Inverse of [`Tab::token`]. Used by `tabs::apply_event` to fold
    /// a routed click back into a typed `Tab` value.
    pub fn from_token(token: &str) -> Option<Tab> {
        Tab::ALL.into_iter().find(|tab| tab.token() == token)
    }
}

impl fmt::Display for Tab {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioClass {
    Stream { direction: Direction },
    Device { direction: Direction },
    Card,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    /// Linear PipeWire volume scalar. `1.0` is nominal 100%.
    pub scalar: f32,
    pub muted: bool,
}

impl Volume {
    pub fn percent(&self) -> u32 {
        (self.scalar * 100.0).round().clamp(0.0, 999.0) as u32
    }

    pub fn from_percent(percent: u32, muted: bool) -> Self {
        Self {
            scalar: (percent as f32 / 100.0).clamp(0.0, 1.5),
            muted,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioNode {
    pub id: u32,
    pub class: AudioClass,
    pub name: String,
    pub description: String,
    pub application: Option<String>,
    pub media_name: Option<String>,
    pub target: Option<String>,
    pub volume: Option<Volume>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileAvailability {
    Unknown,
    No,
    Yes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioProfile {
    /// PipeWire's `index` for the profile — the canonical identifier
    /// passed back to `Device::set_param` to switch profiles.
    pub index: u32,
    pub name: String,
    pub description: String,
    pub available: ProfileAvailability,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioCard {
    pub id: u32,
    pub name: String,
    pub description: String,
    /// Index of the currently active profile (matches one of
    /// `profiles[i].index`), or `None` if the device hasn't reported
    /// a Profile param yet.
    pub active_profile: Option<u32>,
    pub profiles: Vec<AudioProfile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioSnapshot {
    pub nodes: Vec<AudioNode>,
    pub cards: Vec<AudioCard>,
    pub server_name: Option<String>,
    pub error: Option<String>,
    /// `node.name` of the current default audio sink, as published by
    /// the PipeWire `default` metadata. Compared against
    /// `AudioNode.name` to derive per-row default badges.
    pub default_sink_name: Option<String>,
    /// `node.name` of the current default audio source.
    pub default_source_name: Option<String>,
}

impl AudioSnapshot {
    pub fn is_default(&self, node: &AudioNode) -> bool {
        match node.class {
            AudioClass::Device {
                direction: Direction::Output,
            } => self.default_sink_name.as_deref() == Some(node.name.as_str()),
            AudioClass::Device {
                direction: Direction::Input,
            } => self.default_source_name.as_deref() == Some(node.name.as_str()),
            _ => false,
        }
    }
}

impl AudioSnapshot {
    pub fn demo() -> Self {
        // Names sized to real-world PipeWire output: motherboard/GPU/USB
        // device descriptions are long, profile descriptions can be very
        // long. The fixture exists to expose layout breakage that short
        // synthetic strings would hide.
        Self {
            server_name: Some("PipeWire 1.2.7".into()),
            default_sink_name: Some("alsa_output.pci-0000_0b_00.4.analog-stereo".into()),
            default_source_name: Some(
                "alsa_input.usb-Razer_Razer_Seiren_X-00.analog-stereo".into(),
            ),
            nodes: vec![
                AudioNode {
                    id: 42,
                    class: AudioClass::Device {
                        direction: Direction::Output,
                    },
                    name: "alsa_output.pci-0000_0b_00.4.analog-stereo".into(),
                    description: "Family 17h/19h/1ah HD Audio Controller Analog Stereo".into(),
                    application: None,
                    media_name: None,
                    target: Some("Analog Stereo Duplex".into()),
                    volume: Some(Volume {
                        scalar: 0.64,
                        muted: false,
                    }),
                },
                AudioNode {
                    id: 48,
                    class: AudioClass::Device {
                        direction: Direction::Output,
                    },
                    name: "alsa_output.pci-0000_0a_00.1.hdmi-stereo".into(),
                    description: "Navi 21 HDMI Audio [Radeon RX 6800/6800 XT / 6900 XT] Digital Stereo (HDMI 3)".into(),
                    application: None,
                    media_name: None,
                    target: Some("Digital Stereo (HDMI 3)".into()),
                    volume: Some(Volume {
                        scalar: 1.0,
                        muted: false,
                    }),
                },
                AudioNode {
                    id: 56,
                    class: AudioClass::Stream {
                        direction: Direction::Output,
                    },
                    name: "Firefox".into(),
                    description: "Firefox".into(),
                    application: Some("Firefox".into()),
                    media_name: Some("YouTube — Mozart Symphony No. 40 in G minor".into()),
                    target: Some(
                        "Family 17h/19h/1ah HD Audio Controller Analog Stereo".into(),
                    ),
                    volume: Some(Volume {
                        scalar: 0.82,
                        muted: false,
                    }),
                },
                AudioNode {
                    id: 61,
                    class: AudioClass::Stream {
                        direction: Direction::Output,
                    },
                    name: "Discord".into(),
                    description: "Discord".into(),
                    application: Some("WEBRTC VoiceEngine".into()),
                    media_name: Some("Voice call (#general)".into()),
                    target: Some(
                        "Family 17h/19h/1ah HD Audio Controller Analog Stereo".into(),
                    ),
                    volume: Some(Volume {
                        scalar: 0.48,
                        muted: true,
                    }),
                },
                AudioNode {
                    id: 64,
                    class: AudioClass::Stream {
                        direction: Direction::Output,
                    },
                    name: "ALSA plug-in [steam_app_2369390]".into(),
                    description: "ALSA plug-in [steam_app_2369390]".into(),
                    application: Some("ALSA plug-in [steam_app_2369390]".into()),
                    media_name: Some("ALSA Playback".into()),
                    target: Some(
                        "Family 17h/19h/1ah HD Audio Controller Analog Stereo".into(),
                    ),
                    volume: Some(Volume {
                        scalar: 1.0,
                        muted: false,
                    }),
                },
                AudioNode {
                    id: 77,
                    class: AudioClass::Device {
                        direction: Direction::Input,
                    },
                    name: "alsa_input.usb-Razer_Razer_Seiren_X-00.analog-stereo".into(),
                    description: "Razer Seiren X Analog Stereo".into(),
                    application: None,
                    media_name: None,
                    target: Some("Analog Stereo".into()),
                    volume: Some(Volume {
                        scalar: 0.71,
                        muted: false,
                    }),
                },
                AudioNode {
                    id: 81,
                    class: AudioClass::Stream {
                        direction: Direction::Input,
                    },
                    name: "OBS Studio".into(),
                    description: "OBS Studio".into(),
                    application: Some("OBS Studio".into()),
                    media_name: Some("Mic/Aux capture".into()),
                    target: Some("Razer Seiren X Analog Stereo".into()),
                    volume: Some(Volume {
                        scalar: 1.0,
                        muted: false,
                    }),
                },
            ],
            cards: vec![
                AudioCard {
                    id: 12,
                    name: "alsa_card.pci-0000_0b_00.4".into(),
                    description: "Family 17h/19h/1ah HD Audio Controller".into(),
                    active_profile: Some(1),
                    profiles: vec![
                        AudioProfile {
                            index: 1,
                            name: "output:analog-stereo+input:analog-stereo".into(),
                            description: "Analog Stereo Duplex".into(),
                            available: ProfileAvailability::Yes,
                        },
                        AudioProfile {
                            index: 2,
                            name: "output:hdmi-stereo".into(),
                            description: "Digital Stereo (HDMI) Output + Analog Stereo Input"
                                .into(),
                            available: ProfileAvailability::Yes,
                        },
                        AudioProfile {
                            index: 3,
                            name: "output:hdmi-surround51".into(),
                            description: "Digital Surround 5.1 (HDMI) Output + Analog Stereo Input"
                                .into(),
                            available: ProfileAvailability::No,
                        },
                        AudioProfile {
                            index: 0,
                            name: "off".into(),
                            description: "Off".into(),
                            available: ProfileAvailability::Yes,
                        },
                    ],
                },
                AudioCard {
                    id: 18,
                    name: "alsa_card.pci-0000_0a_00.1".into(),
                    description: "Navi 21 HDMI Audio [Radeon RX 6800/6800 XT / 6900 XT]".into(),
                    active_profile: Some(2),
                    profiles: vec![
                        AudioProfile {
                            index: 0,
                            name: "off".into(),
                            description: "Off".into(),
                            available: ProfileAvailability::Yes,
                        },
                        AudioProfile {
                            index: 2,
                            name: "output:hdmi-stereo-extra2".into(),
                            description: "Digital Stereo (HDMI 3) Output".into(),
                            available: ProfileAvailability::Yes,
                        },
                    ],
                },
            ],
            error: None,
        }
    }

    pub fn nodes_for_tab(&self, tab: Tab) -> Vec<&AudioNode> {
        self.nodes
            .iter()
            .filter(|node| match (tab, &node.class) {
                (
                    Tab::Playback,
                    AudioClass::Stream {
                        direction: Direction::Output,
                    },
                ) => true,
                (
                    Tab::Recording,
                    AudioClass::Stream {
                        direction: Direction::Input,
                    },
                ) => true,
                (
                    Tab::Outputs,
                    AudioClass::Device {
                        direction: Direction::Output,
                    },
                ) => true,
                (
                    Tab::Inputs,
                    AudioClass::Device {
                        direction: Direction::Input,
                    },
                ) => true,
                _ => false,
            })
            .collect()
    }

    pub fn node_mut(&mut self, id: u32) -> Option<&mut AudioNode> {
        self.nodes.iter_mut().find(|node| node.id == id)
    }
}

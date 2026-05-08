use crate::model::AudioSnapshot;

pub mod pipewire_native;

/// Read-only access to the latest known PipeWire graph snapshot, plus
/// the small write surface needed to drive a volume control.
///
/// Implementations are expected to maintain the snapshot reactively
/// (a background thread driving a PipeWire registry listener), so
/// `refresh` is a cheap clone of shared state — safe to call once per
/// redraw. Writes are fire-and-forget: the call returns immediately
/// and the actual mutation runs on the backend thread; the snapshot
/// will reflect the change once PipeWire has applied it.
pub trait AudioBackend {
    fn refresh(&self) -> AudioSnapshot;

    /// Mute or unmute a node. No-op if the node id is not currently
    /// known to the backend.
    fn set_mute(&self, node_id: u32, muted: bool) {
        let _ = (node_id, muted);
    }

    /// Set a node's master volume on a linear `0.0..=1.5` scale.
    /// `1.0` is nominal 100%, `0.0` is silent.
    fn set_volume(&self, node_id: u32, scalar: f32) {
        let _ = (node_id, scalar);
    }

    /// Make the named node the default audio sink. The name is the
    /// PipeWire `node.name` property — see [`crate::model::AudioNode`].
    fn set_default_sink(&self, node_name: &str) {
        let _ = node_name;
    }

    /// Make the named node the default audio source.
    fn set_default_source(&self, node_name: &str) {
        let _ = node_name;
    }

    /// Switch a card to one of its enumerated profiles. `card_id` is
    /// the PipeWire global id of the `Audio/Device`; `profile_index`
    /// is the `index` field of an [`crate::model::AudioProfile`] from
    /// the same card.
    fn set_card_profile(&self, card_id: u32, profile_index: u32) {
        let _ = (card_id, profile_index);
    }

    /// Pin a stream node to a specific device (sink for playback,
    /// source for recording), or pass `None` to clear the override and
    /// let WirePlumber's default routing apply. `target_serial` is the
    /// device's [`crate::model::AudioNode::serial`] (PipeWire
    /// `object.serial`) — *not* its global id. WirePlumber's
    /// stream-router matches on serial, and the wrong number silently
    /// no-ops. Implemented over the `default` metadata
    /// (`target.object` as `Spa:Id`, keyed by the stream's id).
    fn set_stream_target(&self, stream_id: u32, target_serial: Option<u64>) {
        let _ = (stream_id, target_serial);
    }
}

#[derive(Default)]
#[allow(dead_code)]
pub struct DemoBackend;

impl AudioBackend for DemoBackend {
    fn refresh(&self) -> AudioSnapshot {
        AudioSnapshot::demo()
    }
}

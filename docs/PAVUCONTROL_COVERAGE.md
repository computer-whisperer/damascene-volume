# Pavucontrol Coverage Target

This is the functional target for `aetna-volume`. The app should eventually
cover these pavucontrol workflows, using native PipeWire object concepts where
possible rather than pretending PipeWire is PulseAudio.

## Playback

- List active output streams.
- Show client/app identity, media name, node target, mute state, and volume.
- Adjust per-stream volume.
- Mute/unmute per stream.
- Move stream to another output device.

## Recording

- List active input streams.
- Show client/app identity, source target, mute state, and volume.
- Adjust per-stream input volume.
- Mute/unmute per stream.
- Move stream to another input device.

## Output Devices

- List sinks/output nodes.
- Show description, nickname, media class, active route/port, mute, and volume.
- Adjust device volume.
- Mute/unmute device.
- Set default output.
- Select port/route where PipeWire exposes it.

## Input Devices

- List sources/input nodes.
- Show description, nickname, media class, active route/port, mute, and volume.
- Adjust device gain.
- Mute/unmute device.
- Set default input.
- Select port/route where PipeWire exposes it.

## Configuration

- List cards/devices.
- Show available profiles.
- Switch profile.
- Show unavailable profiles with reasons when PipeWire exposes that data.

## Polish Targets

- Live updates without manual refresh.
- Fast keyboard navigation.
- Dense but legible rows.
- Per-row meters once stream monitoring is in place.
- Clear error surface when PipeWire denies an operation.
- No accidental destructive routing changes from hover/scroll alone.


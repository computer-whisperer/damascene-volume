# Aetna Volume

A PipeWire volume control panel built with Aetna.

The goal is to replace the day-to-day pavucontrol workflow with a native
PipeWire-first utility:

- playback streams
- recording streams
- output devices
- input devices
- card/profile/port configuration
- mute, volume, default-device, and stream-routing controls

This project intentionally starts as a separate consumer app rather than another
demo inside the Aetna repository. It should pressure-test Aetna against a real,
dense, always-useful desktop tool.

## Early Shape

The first milestone is read-only graph discovery plus a polished static control
surface. Mutating operations come after the object model is stable enough that
we can name PipeWire objects and routes correctly.

```bash
cargo run
```

The project currently depends on the local Aetna checkout at:

```text
../aetna/aetna.main
```


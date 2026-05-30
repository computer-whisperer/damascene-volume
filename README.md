<h1>
  <img src="icon.svg" alt="" width="48" height="48" align="left" />
  Damascene Volume
</h1>

A PipeWire volume control panel built with Damascene.

<p align="center">
  <img src="assets/screenshot.png" alt="Damascene Volume — Playback tab" width="640" />
</p>

The goal is to replace the day-to-day pavucontrol workflow with a native
PipeWire-first utility:

- playback streams
- recording streams
- output devices
- input devices
- card/profile/port configuration
- mute, volume, default-device, and stream-routing controls

This project intentionally starts as a separate consumer app rather than another
demo inside the Damascene repository. It should pressure-test Damascene against a real,
dense, always-useful desktop tool.

## Early Shape

The first milestone is read-only graph discovery plus a polished static control
surface. Mutating operations come after the object model is stable enough that
we can name PipeWire objects and routes correctly.

```bash
cargo run
```

Damascene is consumed from crates.io (currently `damascene-core`/`damascene-winit-wgpu` 0.4.0).

## Arch package

An AUR-oriented `PKGBUILD` is provided for tagged releases. It installs the
`damascene-volume` binary, a hicolor scalable app icon
(`/usr/share/icons/hicolor/scalable/apps/damascene-volume.svg`), and a `.desktop`
entry that lands under `AudioVideo → Audio → Mixer` so the launcher picks it up
without a logout.

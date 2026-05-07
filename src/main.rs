use aetna_core::Rect;
use aetna_volume::{app::VolumeApp, backend::pipewire_native::PipeWireBackend};
use aetna_winit_wgpu::{HostConfig, run_with_config};
use std::time::Duration;

const METER_FRAME_INTERVAL: Duration = Duration::from_millis(33);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    raise_open_file_limit();
    // Default to a 50% slice of a 1080p panel — the typical placement on the
    // user's secondary monitor. Window managers reflow this freely, but it's
    // what we polish against.
    let viewport = Rect::new(0.0, 0.0, 960.0, 1080.0);
    run_with_config(
        "Aetna Volume",
        viewport,
        VolumeApp::new(Box::new(PipeWireBackend::new())),
        HostConfig::default().with_redraw_interval(METER_FRAME_INTERVAL),
    )
}

/// Raise the soft `RLIMIT_NOFILE` to the hard limit so PipeWire's
/// per-proxy SHM pools and eventfds don't trip the systemd default of
/// 1024. With ~80 audio nodes the bound proxies alone push baseline fd
/// usage past that; the next wgpu DMA-BUF import then fails with EMFILE
/// inside `zwp_linux_buffer_params_v1.add` and panics the renderer.
/// This is the same pattern Chrome / Firefox / Electron use.
fn raise_open_file_limit() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limits = getrlimit(Resource::Nofile);
    let _ = setrlimit(
        Resource::Nofile,
        Rlimit {
            current: limits.maximum,
            maximum: limits.maximum,
        },
    );
}

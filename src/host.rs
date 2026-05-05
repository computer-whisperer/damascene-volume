use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use aetna_core::{App, KeyModifiers, PointerButton, Rect, UiKey};
use aetna_wgpu::{MsaaTarget, Runner};

const SAMPLE_COUNT: u32 = 4;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    window::{Window, WindowId},
};

const METER_FRAME_INTERVAL: Duration = Duration::from_millis(33);

pub fn run_volume_app<A: App + 'static>(
    title: &'static str,
    viewport: Rect,
    app: A,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let mut host = VolumeHost {
        title,
        viewport,
        app,
        gfx: None,
        last_pointer: None,
        modifiers: KeyModifiers::default(),
        next_meter_frame: Instant::now(),
    };
    event_loop.run_app(&mut host)?;
    Ok(())
}

struct VolumeHost<A: App> {
    title: &'static str,
    viewport: Rect,
    app: A,
    gfx: Option<Gfx>,
    last_pointer: Option<(f32, f32)>,
    modifiers: KeyModifiers,
    next_meter_frame: Instant,
}

struct Gfx {
    renderer: Runner,
    surface: wgpu::Surface<'static>,
    queue: wgpu::Queue,
    device: wgpu::Device,
    window: Arc<Window>,
    config: wgpu::SurfaceConfiguration,
    /// Multisampled color attachment for the surface frame, kept in
    /// sync with `config.width`/`config.height`. Reallocated on resize.
    msaa: MsaaTarget,
}

fn surface_extent(config: &wgpu::SurfaceConfiguration) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width: config.width,
        height: config.height,
        depth_or_array_layers: 1,
    }
}

impl<A: App> ApplicationHandler for VolumeHost<A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title(self.title)
            .with_inner_size(PhysicalSize::new(
                self.viewport.w as u32,
                self.viewport.h as u32,
            ));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("no compatible adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("aetna_volume::device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("request_device");

        let size = window.inner_size();
        let surface_caps = surface.get_capabilities(&adapter);
        let format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: surface_caps.present_modes[0],
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut renderer = Runner::with_sample_count(&device, &queue, format, SAMPLE_COUNT);
        renderer.set_theme(self.app.theme());
        renderer.set_surface_size(config.width, config.height);
        for shader in self.app.shaders() {
            renderer.register_shader_with(
                &device,
                shader.name,
                shader.wgsl,
                shader.samples_backdrop,
            );
        }

        let msaa = MsaaTarget::new(&device, format, surface_extent(&config), SAMPLE_COUNT);

        self.gfx = Some(Gfx {
            renderer,
            surface,
            queue,
            device,
            window,
            config,
            msaa,
        });
        self.next_meter_frame = Instant::now() + METER_FRAME_INTERVAL;
        self.gfx.as_ref().unwrap().window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.gfx.take();
                event_loop.exit();
            }

            event => {
                let Some(gfx) = self.gfx.as_mut() else {
                    return;
                };
                let scale = gfx.window.scale_factor() as f32;

                match event {
                    WindowEvent::Resized(size) => {
                        gfx.config.width = size.width.max(1);
                        gfx.config.height = size.height.max(1);
                        gfx.surface.configure(&gfx.device, &gfx.config);
                        gfx.renderer
                            .set_surface_size(gfx.config.width, gfx.config.height);
                        let extent = surface_extent(&gfx.config);
                        if !gfx.msaa.matches(extent) {
                            gfx.msaa = MsaaTarget::new(
                                &gfx.device,
                                gfx.config.format,
                                extent,
                                SAMPLE_COUNT,
                            );
                        }
                        gfx.window.request_redraw();
                    }

                    WindowEvent::CursorMoved { position, .. } => {
                        let lx = position.x as f32 / scale;
                        let ly = position.y as f32 / scale;
                        self.last_pointer = Some((lx, ly));
                        if let Some(event) = gfx.renderer.pointer_moved(lx, ly) {
                            self.app.on_event(event);
                        }
                        gfx.window.request_redraw();
                    }

                    WindowEvent::CursorLeft { .. } => {
                        self.last_pointer = None;
                        gfx.renderer.pointer_left();
                        gfx.window.request_redraw();
                    }

                    WindowEvent::MouseInput { state, button, .. } => {
                        let Some(button) = pointer_button(button) else {
                            return;
                        };
                        let Some((lx, ly)) = self.last_pointer else {
                            return;
                        };
                        match state {
                            ElementState::Pressed => {
                                if let Some(event) = gfx.renderer.pointer_down(lx, ly, button) {
                                    self.app.on_event(event);
                                }
                                gfx.window.request_redraw();
                            }
                            ElementState::Released => {
                                for event in gfx.renderer.pointer_up(lx, ly, button) {
                                    self.app.on_event(event);
                                }
                                gfx.window.request_redraw();
                            }
                        }
                    }

                    WindowEvent::MouseWheel { delta, .. } => {
                        let Some((lx, ly)) = self.last_pointer else {
                            return;
                        };
                        let dy = match delta {
                            MouseScrollDelta::LineDelta(_, y) => -y * 50.0,
                            MouseScrollDelta::PixelDelta(p) => -(p.y as f32) / scale,
                        };
                        if gfx.renderer.pointer_wheel(lx, ly, dy) {
                            gfx.window.request_redraw();
                        }
                    }

                    WindowEvent::ModifiersChanged(modifiers) => {
                        self.modifiers = key_modifiers(modifiers.state());
                        gfx.renderer.set_modifiers(self.modifiers);
                    }

                    WindowEvent::KeyboardInput {
                        event:
                            key_event @ winit::event::KeyEvent {
                                state: ElementState::Pressed,
                                ..
                            },
                        is_synthetic: false,
                        ..
                    } => {
                        if let Some(key) = map_key(&key_event.logical_key)
                            && let Some(event) =
                                gfx.renderer.key_down(key, self.modifiers, key_event.repeat)
                        {
                            self.app.on_event(event);
                        }
                        if let Some(text) = &key_event.text
                            && let Some(event) = gfx.renderer.text_input(text.to_string())
                        {
                            self.app.on_event(event);
                        }
                        gfx.window.request_redraw();
                    }

                    WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                        if let Some(event) = gfx.renderer.text_input(text) {
                            self.app.on_event(event);
                        }
                        gfx.window.request_redraw();
                    }

                    WindowEvent::RedrawRequested => {
                        let frame = match gfx.surface.get_current_texture() {
                            wgpu::CurrentSurfaceTexture::Success(frame)
                            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
                            wgpu::CurrentSurfaceTexture::Lost
                            | wgpu::CurrentSurfaceTexture::Outdated => {
                                gfx.surface.configure(&gfx.device, &gfx.config);
                                return;
                            }
                            other => {
                                eprintln!("surface unavailable: {other:?}");
                                return;
                            }
                        };
                        let view = frame
                            .texture
                            .create_view(&wgpu::TextureViewDescriptor::default());

                        let mut tree = self.app.build();
                        gfx.renderer.set_theme(self.app.theme());
                        gfx.renderer.set_hotkeys(self.app.hotkeys());
                        let scale_factor = gfx.window.scale_factor() as f32;
                        let viewport = Rect::new(
                            0.0,
                            0.0,
                            gfx.config.width as f32 / scale_factor,
                            gfx.config.height as f32 / scale_factor,
                        );
                        let prepare = gfx.renderer.prepare(
                            &gfx.device,
                            &gfx.queue,
                            &mut tree,
                            viewport,
                            scale_factor,
                        );

                        let mut encoder =
                            gfx.device
                                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                    label: Some("aetna_volume::encoder"),
                                });
                        gfx.renderer.render(
                            &gfx.device,
                            &mut encoder,
                            &frame.texture,
                            &view,
                            Some(&gfx.msaa.view),
                            wgpu::LoadOp::Clear(bg_color()),
                        );
                        gfx.queue.submit(Some(encoder.finish()));
                        frame.present();

                        if prepare.needs_redraw {
                            gfx.window.request_redraw();
                        }
                    }

                    _ => {}
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gfx) = self.gfx.as_ref() else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };

        let now = Instant::now();
        if now >= self.next_meter_frame {
            gfx.window.request_redraw();
            self.next_meter_frame = now + METER_FRAME_INTERVAL;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_meter_frame));
    }
}

fn map_key(key: &Key) -> Option<UiKey> {
    match key {
        Key::Named(NamedKey::Enter) => Some(UiKey::Enter),
        Key::Named(NamedKey::Escape) => Some(UiKey::Escape),
        Key::Named(NamedKey::Tab) => Some(UiKey::Tab),
        Key::Named(NamedKey::Space) => Some(UiKey::Space),
        Key::Named(NamedKey::ArrowUp) => Some(UiKey::ArrowUp),
        Key::Named(NamedKey::ArrowDown) => Some(UiKey::ArrowDown),
        Key::Named(NamedKey::ArrowLeft) => Some(UiKey::ArrowLeft),
        Key::Named(NamedKey::ArrowRight) => Some(UiKey::ArrowRight),
        Key::Named(NamedKey::Backspace) => Some(UiKey::Backspace),
        Key::Named(NamedKey::Delete) => Some(UiKey::Delete),
        Key::Named(NamedKey::Home) => Some(UiKey::Home),
        Key::Named(NamedKey::End) => Some(UiKey::End),
        Key::Character(s) => Some(UiKey::Character(s.to_string())),
        Key::Named(named) => Some(UiKey::Other(format!("{named:?}"))),
        _ => None,
    }
}

fn pointer_button(button: MouseButton) -> Option<PointerButton> {
    match button {
        MouseButton::Left => Some(PointerButton::Primary),
        MouseButton::Right => Some(PointerButton::Secondary),
        MouseButton::Middle => Some(PointerButton::Middle),
        _ => None,
    }
}

fn key_modifiers(modifiers: winit::keyboard::ModifiersState) -> KeyModifiers {
    KeyModifiers {
        shift: modifiers.shift_key(),
        ctrl: modifiers.control_key(),
        alt: modifiers.alt_key(),
        logo: modifiers.super_key(),
    }
}

fn bg_color() -> wgpu::Color {
    let color = aetna_core::tokens::BG_APP;
    wgpu::Color {
        r: srgb_to_linear(color.r as f64 / 255.0),
        g: srgb_to_linear(color.g as f64 / 255.0),
        b: srgb_to_linear(color.b as f64 / 255.0),
        a: color.a as f64 / 255.0,
    }
}

fn srgb_to_linear(channel: f64) -> f64 {
    if channel <= 0.040_45 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

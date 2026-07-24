//! The ultimate example: an interactive, pan-and-zoom "whole of Unicode" page.
//!
//! The page is laid out **once** in world pixels; every frame only updates the
//! projection matrix. Because the Slug shader derives coverage from screen-space
//! derivatives, the vector text stays razor-sharp at any zoom — scroll in on a
//! Chinese character or the 6-pixel line and it resolves to crisp curves, never a
//! blurry bitmap. (Color emoji use a raster atlas, so those do pixelate — the one
//! thing here that isn't resolution-independent.)
//!
//! Run with:  `cargo run --example unicode_zoom`
//!   scroll = zoom at cursor · drag = pan · R = reset · Esc = quit

mod common;

use std::sync::Arc;

use glam::{Mat4, Vec2, Vec3};
use sanscale::{EmojiRenderer, TextArgs, TextEngine, TextRenderer};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use common::{font_chain, unicode_sections, UNICODE_FALLBACK};

fn main() {
    println!("scroll = zoom · drag = pan · R = reset · Esc = quit");
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut App::default()).unwrap();
}

#[derive(Default)]
struct App {
    gfx: Option<Gfx>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            self.gfx = Some(pollster::block_on(Gfx::new(event_loop)));
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Character(ref s) if s.eq_ignore_ascii_case("r") => {
                        gfx.reset_view();
                        gfx.window.request_redraw();
                    }
                    _ => {}
                }
            }
            WindowEvent::Resized(size) => {
                gfx.resize(size);
                gfx.window.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 60.0,
                };
                gfx.zoom_at_cursor(1.15f32.powf(dy));
                gfx.window.request_redraw();
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                gfx.dragging = state == ElementState::Pressed;
            }
            WindowEvent::CursorMoved { position, .. } => {
                let p = Vec2::new(position.x as f32, position.y as f32);
                if gfx.dragging {
                    gfx.offset += p - gfx.cursor;
                    gfx.window.request_redraw();
                }
                gfx.cursor = p;
            }
            WindowEvent::RedrawRequested => gfx.render(),
            _ => {}
        }
    }
}

struct Gfx {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,

    text_renderer: TextRenderer,
    emoji_renderer: EmojiRenderer,
    text_atlas: sanscale::TextAtlas,
    emoji_atlas: sanscale::EmojiAtlas,
    text_buf: wgpu::Buffer,
    emoji_buf: wgpu::Buffer,
    text_count: u32,
    emoji_count: u32,

    // Camera: screen = world * scale + offset.
    scale: f32,
    offset: Vec2,
    cursor: Vec2,
    dragging: bool,
}

impl Gfx {
    async fn new(event_loop: &ActiveEventLoop) -> Self {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("sanscale · zoomable Unicode")
                        .with_inner_size(PhysicalSize::new(1280, 820)),
                )
                .unwrap(),
        );

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(event_loop.owned_display_handle()),
        ));
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("adapter");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("unicode_zoom"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .expect("device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Build the page once, in world pixels.
        let mut engine =
            TextEngine::from_sources(font_chain(UNICODE_FALLBACK)).expect("fallback chain");
        let text_renderer = TextRenderer::new(&device, &config);
        let emoji_renderer = EmojiRenderer::new(&device, &config);
        let mut text_atlas = engine.new_atlas(&device, &queue, &text_renderer.atlas_layout);
        let mut emoji_atlas = engine.new_emoji_atlas(&device, &queue, &emoji_renderer.atlas_layout);

        lay_out_page(&mut engine);

        engine.sync_atlas(&mut text_atlas, &device, &queue, &text_renderer.atlas_layout);
        engine.sync_emoji_atlas(&mut emoji_atlas, &device, &queue, &emoji_renderer.atlas_layout);
        let text_vertices = engine.flush().to_vec();
        let emoji_vertices = engine.emoji_vertices().to_vec();
        let text_buf = TextRenderer::build_vertices(&device, &text_vertices);
        let emoji_buf = EmojiRenderer::build_vertices(&device, &emoji_vertices);

        let mut gfx = Self {
            window,
            device,
            queue,
            surface,
            config,
            text_renderer,
            emoji_renderer,
            text_atlas,
            emoji_atlas,
            text_buf,
            emoji_buf,
            text_count: text_vertices.len() as u32,
            emoji_count: emoji_vertices.len() as u32,
            scale: 1.0,
            offset: Vec2::ZERO,
            cursor: Vec2::ZERO,
            dragging: false,
        };
        gfx.reset_view();
        gfx
    }

    fn reset_view(&mut self) {
        // Fit the ~1180px-wide page into the window with a small margin.
        self.scale = (self.config.width as f32 / 1240.0).clamp(0.2, 3.0);
        self.offset = Vec2::new(24.0, 20.0);
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    fn zoom_at_cursor(&mut self, factor: f32) {
        let new_scale = (self.scale * factor).clamp(0.05, 40.0);
        // Keep the world point under the cursor fixed.
        let world = (self.cursor - self.offset) / self.scale;
        self.offset = self.cursor - world * new_scale;
        self.scale = new_scale;
    }

    fn render(&mut self) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                f
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());

        let proj = Mat4::orthographic_rh(
            0.0,
            self.config.width as f32,
            self.config.height as f32,
            0.0,
            -1.0,
            1.0,
        );
        let model = Mat4::from_translation(Vec3::new(self.offset.x, self.offset.y, 0.0))
            * Mat4::from_scale(Vec3::new(self.scale, self.scale, 1.0));
        let matrix = proj * model;
        self.text_renderer.write_matrix(&self.queue, matrix);
        self.emoji_renderer.write_matrix(&self.queue, matrix);

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.99,
                            g: 0.99,
                            b: 0.99,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.text_renderer
                .draw_vertices(&mut pass, &self.text_atlas, &self.text_buf, 0..self.text_count);
            self.emoji_renderer
                .draw(&mut pass, &self.emoji_atlas, &self.emoji_buf, 0..self.emoji_count);
        }
        self.queue.submit([encoder.finish()]);
        frame.present();
    }
}

/// Lay the page out in world pixels: a heading, the multilingual sections, and a
/// deliberately tiny line to show off zooming in without blur.
fn lay_out_page(engine: &mut TextEngine) {
    let ink = [0.10, 0.11, 0.13, 1.0];
    let muted = [0.45, 0.47, 0.52, 1.0];

    engine.text(
        40.0,
        60.0,
        "The whole of Unicode — crisp at any zoom",
        &TextArgs { size_px: 40.0, color: ink, ..Default::default() },
    );
    engine.text(
        42.0,
        92.0,
        "scroll to zoom · drag to pan · vector text never pixelates (emoji is raster)",
        &TextArgs { size_px: 15.0, color: muted, ..Default::default() },
    );

    let label = TextArgs { size_px: 15.0, color: muted, ..Default::default() };
    let sample = TextArgs { size_px: 30.0, color: ink, ..Default::default() };
    let mut y = 168.0;
    for (name, text) in unicode_sections() {
        engine.text(40.0, y, name, &label);
        engine.text(40.0, y + 34.0, text, &sample);
        y += 74.0;
    }

    engine.text(
        40.0,
        y + 8.0,
        "▸ this line is six pixels tall — zoom in and it stays perfectly sharp ◂",
        &TextArgs { size_px: 6.0, color: ink, ..Default::default() },
    );
}

//! A minimal live sanscale application.
//!
//! Two large multilingual lines move every frame without reshaping. The FPS line
//! changes through one stable block and paragraph slot; only its generation moves
//! when the displayed EMA value changes.
//!
//! Interactive: `cargo run --example hello`
//! Preview PNG: `cargo run --example hello -- --dump`

mod common;

use std::sync::Arc;
use std::time::Instant;

use common::{Harness, UNICODE_FALLBACK, font_chain};
use sanscale::{
    Align, BlockKey, Color, Draw, ParagraphKey, Paragraphs, ShapedHandle, Style, TextService,
    Vec2,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const MARGIN: f32 = 28.0;
const GREETING_PX: f32 = 82.0;
const SCRIPTS_PX: f32 = 76.0;
const FPS_PX: f32 = 48.0;
const EMA_ALPHA: f32 = 0.08;

const GREETING: &str = "Hello, sanscale! 你好 👋";
const SCRIPTS: &str = "日本語 · 한국어 🚀✨";
const BG: wgpu::Color = wgpu::Color {
    r: 0.008,
    g: 0.012,
    b: 0.024,
    a: 1.0,
};

struct Scene {
    text: TextService,
    style: Style,
    greeting: ShapedHandle,
    scripts: ShapedHandle,
    fps: ShapedHandle,
    fps_text: String,
    fps_generation: u32,
}

impl Scene {
    fn new() -> Self {
        let mut text = TextService::new();
        let chain = font_chain(&mut text, UNICODE_FALLBACK);
        let style = Style {
            chain,
            wrap_em: None,
            align: Align::Left,
            line_spacing: 1.0,
        };

        let greeting_key = ParagraphKey {
            namespace: 0,
            slot: 0,
            generation: 0,
        };
        let greeting = text
            .shape(
                BlockKey(0),
                &style,
                &[greeting_key],
                &Paragraphs(&[GREETING]),
            )
            .expect("no usable system font found");

        let scripts_key = ParagraphKey {
            namespace: 0,
            slot: 1,
            generation: 0,
        };
        let scripts = text
            .shape(
                BlockKey(1),
                &style,
                &[scripts_key],
                &Paragraphs(&[SCRIPTS]),
            )
            .expect("no usable system font found");

        let fps_text = "60.0 FPS  ·  16.67 ms EMA".to_string();
        let fps_key = ParagraphKey {
            namespace: 0,
            slot: 2,
            generation: 0,
        };
        let fps = text
            .shape(
                BlockKey(2),
                &style,
                &[fps_key],
                &Paragraphs(&[fps_text.as_str()]),
            )
            .expect("no usable system font found");

        Self {
            text,
            style,
            greeting,
            scripts,
            fps,
            fps_text,
            fps_generation: 0,
        }
    }

    fn draws(
        &mut self,
        width: f32,
        height: f32,
        elapsed: f32,
        frame_seconds_ema: f32,
    ) -> [Draw; 3] {
        let next_fps = format!(
            "{:5.1} FPS  ·  {:5.2} ms EMA",
            frame_seconds_ema.recip(),
            frame_seconds_ema * 1000.0
        );
        if next_fps != self.fps_text {
            self.fps_text = next_fps;
            self.fps_generation = self.fps_generation.wrapping_add(1);
            let key = ParagraphKey {
                namespace: 0,
                slot: 2,
                generation: self.fps_generation,
            };
            self.fps = self
                .text
                .shape(
                    BlockKey(2),
                    &self.style,
                    &[key],
                    &Paragraphs(&[self.fps_text.as_str()]),
                )
                .expect("FPS text should shape");
        }

        let greeting_em = self.text.measure(self.greeting).size_em();
        let greeting_size = Vec2::new(greeting_em.x * GREETING_PX, greeting_em.y * GREETING_PX);
        let scripts_em = self.text.measure(self.scripts).size_em();
        let scripts_size = Vec2::new(scripts_em.x * SCRIPTS_PX, scripts_em.y * SCRIPTS_PX);
        let fps_em = self.text.measure(self.fps).size_em();
        let fps_size = Vec2::new(fps_em.x * FPS_PX, fps_em.y * FPS_PX);
        let fps_y = (height - fps_size.y - MARGIN).max(MARGIN);
        let moving_height = (fps_y - MARGIN).max(0.0);
        let band_height = moving_height * 0.5;

        let greeting_at = Vec2::new(
            MARGIN
                + (width - greeting_size.x - MARGIN * 2.0).max(0.0)
                    * (0.5 + 0.5 * (elapsed * 0.73).sin()),
            MARGIN
                + (band_height - greeting_size.y).max(0.0)
                    * (0.5 + 0.5 * (elapsed * 1.07).cos()),
        );
        let scripts_at = Vec2::new(
            MARGIN
                + (width - scripts_size.x - MARGIN * 2.0).max(0.0)
                    * (0.5 + 0.5 * (elapsed * 0.91 + 2.1).sin()),
            MARGIN
                + band_height
                + (band_height - scripts_size.y).max(0.0)
                    * (0.5 + 0.5 * (elapsed * 1.21 + 0.7).sin()),
        );
        let fps_at = Vec2::new(((width - fps_size.x) * 0.5).max(MARGIN), fps_y);

        [
            Draw {
                block: self.greeting,
                at: greeting_at,
                size: GREETING_PX,
                color: Color([0.35, 0.72, 1.0, 1.0]),
                clip: None,
            },
            Draw {
                block: self.scripts,
                at: scripts_at,
                size: SCRIPTS_PX,
                color: Color([1.0, 0.48, 0.22, 1.0]),
                clip: None,
            },
            Draw {
                block: self.fps,
                at: fps_at,
                size: FPS_PX,
                color: Color([0.38, 1.0, 0.62, 1.0]),
                clip: None,
            },
        ]
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    if std::env::args().any(|arg| arg == "--dump") {
        let harness = Harness::new(WIDTH, HEIGHT);
        let mut scene = Scene::new();
        let draws = scene.draws(WIDTH as f32, HEIGHT as f32, 3.0, 1.0 / 60.0);
        harness.save_png(&mut scene.text, BG, "hello.png", |text, device, queue, pass| {
            text.draw_batch(device, queue, pass, &draws);
        });
        println!("wrote hello.png");
        return;
    }

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop.run_app(&mut App::default()).unwrap();
}

#[derive(Default)]
struct App {
    gfx: Option<Gfx>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        self.gfx = Some(pollster::block_on(async {
            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("sanscale · hello")
                            .with_inner_size(PhysicalSize::new(WIDTH, HEIGHT)),
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
                    apply_limit_buckets: false,
                })
                .await
                .expect("adapter");
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("hello"),
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
                .find(|format| format.is_srgb())
                .unwrap_or(caps.formats[0]);
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                color_space: wgpu::SurfaceColorSpace::Auto,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: caps.alpha_modes[0],
                view_formats: vec![],
                desired_maximum_frame_latency: 1,
            };
            surface.configure(&device, &config);
            let now = Instant::now();
            let gfx = Gfx {
                window,
                surface,
                config,
                device,
                queue,
                scene: Scene::new(),
                started: now,
                last_frame: now,
                frame_seconds_ema: 1.0 / 60.0,
                frames: 0,
            };
            gfx.window.request_redraw();
            gfx
        }));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. }
                if event.state.is_pressed()
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if size.width > 0 && size.height > 0 {
                    gfx.config.width = size.width;
                    gfx.config.height = size.height;
                    gfx.surface.configure(&gfx.device, &gfx.config);
                    gfx.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                let frame = match gfx.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(frame)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
                    wgpu::CurrentSurfaceTexture::Outdated
                    | wgpu::CurrentSurfaceTexture::Lost => {
                        gfx.surface.configure(&gfx.device, &gfx.config);
                        gfx.window.request_redraw();
                        return;
                    }
                    _ => {
                        gfx.window.request_redraw();
                        return;
                    }
                };

                let now = Instant::now();
                if gfx.frames > 0 {
                    let frame_seconds = now.duration_since(gfx.last_frame).as_secs_f32();
                    gfx.frame_seconds_ema +=
                        EMA_ALPHA * (frame_seconds - gfx.frame_seconds_ema);
                }
                gfx.last_frame = now;
                gfx.frames += 1;

                let draws = gfx.scene.draws(
                    gfx.config.width as f32,
                    gfx.config.height as f32,
                    now.duration_since(gfx.started).as_secs_f32(),
                    gfx.frame_seconds_ema,
                );
                gfx.scene.text.set_target(&gfx.device, gfx.config.format);
                gfx.scene.text.set_transform(
                    &gfx.queue,
                    TextService::pixel_ortho(gfx.config.width, gfx.config.height),
                );

                let view = frame.texture.create_view(&Default::default());
                let mut encoder = gfx.device.create_command_encoder(&Default::default());
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("hello"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(BG),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    gfx.scene
                        .text
                        .draw_batch(&gfx.device, &gfx.queue, &mut pass, &draws);
                }
                gfx.queue.submit([encoder.finish()]);
                gfx.queue.present(frame);
                gfx.window.request_redraw();
            }
            _ => {}
        }
    }
}

struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    scene: Scene,
    started: Instant,
    last_frame: Instant,
    frame_seconds_ema: f32,
    frames: u64,
}

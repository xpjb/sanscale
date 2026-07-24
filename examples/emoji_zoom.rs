//! A zoomable board of every RGI emoji — the sibling of `unicode_zoom`, identical
//! in every respect (pan/zoom, world-space category titles, per-row vertex cache,
//! p99 debug line, headless `--dump`) but the content is the full emoji set from
//! Unicode's `emoji-test.txt`, in canonical picker order and grouped by category.
//!
//! Unlike the codespace map, each cell shapes a whole **sequence** (ZWJ, flags,
//! skin-tone, keycaps), so this also exercises multi-code-point emoji shaping.
//!
//! Interactive:  `cargo run --example emoji_zoom`
//!     scroll = zoom at cursor · drag (any button) = pan · R = reset · Esc = quit
//! Headless PNGs: `cargo run --example emoji_zoom -- --dump`

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use glam::{Mat4, Vec2, Vec3};
use sanscale::{
    EmojiAtlas, EmojiRenderer, EmojiVertex, TextArgs, TextAtlas, TextEngine, TextRenderer,
    TextVertex,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use common::emoji_data::GROUPS;
use common::{font_chain, UNICODE_FALLBACK};

const COLS: usize = 24; // emoji per row
const CELL_W: f32 = 44.0;
const CELL_H: f32 = 44.0;
const GLYPH_PX: f32 = 34.0;
const CELL_PAD: f32 = 5.0;
const GUTTER_W: f32 = 560.0; // world-space column reserved on the left for titles
const TITLE_PX: f32 = 52.0; // world-space title height (scales with zoom)
const WORLD_W: f32 = GUTTER_W + COLS as f32 * CELL_W;
const INK: [f32; 4] = [0.12, 0.13, 0.16, 1.0];
const TITLE: [f32; 4] = [0.16, 0.40, 0.82, 1.0];

fn main() {
    if std::env::args().any(|a| a == "--dump") {
        dump();
        return;
    }
    println!("scroll = zoom · drag (any button) = pan · R = reset · Esc = quit");
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut App::default()).unwrap();
}

fn ortho(w: f32, h: f32) -> Mat4 {
    Mat4::orthographic_rh(0.0, w, h, 0.0, -1.0, 1.0)
}
fn model(offset: Vec2, scale: f32) -> Mat4 {
    Mat4::from_translation(Vec3::new(offset.x, offset.y, 0.0)) * Mat4::from_scale(Vec3::splat(scale))
}
fn args(size_px: f32, color: [f32; 4]) -> TextArgs {
    TextArgs { size_px, color, ..Default::default() }
}

/// One emoji cell: world x, the (possibly multi-code-point) string, and its name.
struct Cell {
    x: f32,
    emoji: &'static str,
    name: &'static str,
}

/// One laid-out row of emoji, optionally introducing a category (title in gutter).
struct RowLayout {
    cells: Vec<Cell>,
    title: Option<&'static str>,
}

/// Flatten the grouped emoji list into rows: each category starts on a fresh row
/// with its name in the gutter, then flows `COLS` emoji per row.
fn build_layout() -> Vec<RowLayout> {
    let mut layout = Vec::new();
    for &(group, items) in GROUPS {
        let rows = items.len().div_ceil(COLS);
        for ri in 0..rows {
            let mut cells = Vec::new();
            for ci in 0..COLS {
                let Some(&(emoji, name)) = items.get(ri * COLS + ci) else { break };
                cells.push(Cell { x: GUTTER_W + ci as f32 * CELL_W + CELL_PAD, emoji, name });
            }
            layout.push(RowLayout { cells, title: (ri == 0).then_some(group) });
        }
    }
    layout
}

/// A vertex buffer written in place each frame; reallocated only when it grows.
struct DynBuf {
    buf: wgpu::Buffer,
    cap: u64,
}

impl DynBuf {
    fn new(device: &wgpu::Device) -> Self {
        Self { buf: Self::alloc(device, 4096), cap: 4096 }
    }
    fn alloc(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dyn vertices"),
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }
    fn upload<T: bytemuck::Pod>(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, data: &[T]) -> u32 {
        let bytes: &[u8] = bytemuck::cast_slice(data);
        if bytes.is_empty() {
            return 0;
        }
        if bytes.len() as u64 > self.cap {
            self.cap = (bytes.len() as u64).next_power_of_two();
            self.buf = Self::alloc(device, self.cap);
        }
        queue.write_buffer(&self.buf, 0, bytes);
        data.len() as u32
    }
}

struct Viewer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    engine: TextEngine,
    text_renderer: TextRenderer,
    emoji_renderer: EmojiRenderer,
    text_atlas: TextAtlas,
    emoji_atlas: EmojiAtlas,
    grid_buf: DynBuf,
    emoji_buf: DynBuf,
    overlay_buf: DynBuf,
    layout: Vec<RowLayout>,
    world_h: f32,
    rows: HashMap<i64, (Vec<TextVertex>, Vec<EmojiVertex>)>,
    scratch_text: Vec<TextVertex>,
    scratch_emoji: Vec<EmojiVertex>,
}

impl Viewer {
    fn new(device: wgpu::Device, queue: wgpu::Queue, config: &wgpu::SurfaceConfiguration) -> Self {
        let engine =
            TextEngine::from_sources(font_chain(UNICODE_FALLBACK)).expect("no usable fonts");
        let text_renderer = TextRenderer::new(&device, config);
        let emoji_renderer = EmojiRenderer::new(&device, config);
        let text_atlas = engine.new_atlas(&device, &queue, &text_renderer.atlas_layout);
        let emoji_atlas = engine.new_emoji_atlas(&device, &queue, &emoji_renderer.atlas_layout);
        let layout = build_layout();
        let world_h = (layout.len() as f32 + 1.0) * CELL_H;
        let (grid_buf, emoji_buf, overlay_buf) =
            (DynBuf::new(&device), DynBuf::new(&device), DynBuf::new(&device));
        Self {
            device,
            queue,
            engine,
            text_renderer,
            emoji_renderer,
            text_atlas,
            emoji_atlas,
            grid_buf,
            emoji_buf,
            overlay_buf,
            layout,
            world_h,
            rows: HashMap::new(),
            scratch_text: Vec::new(),
            scratch_emoji: Vec::new(),
        }
    }

    /// Shape and cache one row's emoji (each cell may be a multi-code-point
    /// sequence). Cheap no-op once cached.
    fn build_row(&mut self, r: i64) {
        if self.rows.contains_key(&r) {
            return;
        }
        let (t, e) = if r >= 0 && (r as usize) < self.layout.len() {
            let ink = args(GLYPH_PX, INK);
            let y = r as f32 * CELL_H + GLYPH_PX + 4.0;
            let cells = &self.layout[r as usize].cells;
            for cell in cells {
                self.engine.text(cell.x, y, cell.emoji, &ink);
            }
            self.engine.sync_atlas(&mut self.text_atlas, &self.device, &self.queue, &self.text_renderer.atlas_layout);
            self.engine.sync_emoji_atlas(&mut self.emoji_atlas, &self.device, &self.queue, &self.emoji_renderer.atlas_layout);
            (self.engine.flush().to_vec(), self.engine.emoji_vertices().to_vec())
        } else {
            (Vec::new(), Vec::new())
        };
        self.rows.insert(r, (t, e));
    }

    /// World-space category titles in the gutter — same offset-invariant dedup as
    /// `unicode_zoom` so panning doesn't reshuffle which show.
    fn emit_titles(&mut self, offset: Vec2, scale: f32, h: f32) {
        let args = args(TITLE_PX, TITLE);
        let min_gap = TITLE_PX * scale * 1.1;
        let mut last_kept = f32::MIN;
        for r in 0..self.layout.len() {
            let Some(name) = self.layout[r].title else { continue };
            let screen_y = r as f32 * CELL_H * scale + offset.y;
            if screen_y < last_kept + min_gap {
                continue;
            }
            last_kept = screen_y;
            if screen_y >= -TITLE_PX * scale && screen_y <= h {
                self.engine.text(10.0, r as f32 * CELL_H + TITLE_PX * 0.82, name, &args);
            }
        }
    }

    /// Emoji under the cursor (`name · U+… U+…`), if any.
    fn hovered(&self, cursor: Vec2, offset: Vec2, scale: f32) -> Option<String> {
        let world = (cursor - offset) / scale;
        let col = ((world.x - GUTTER_W) / CELL_W).floor() as i64;
        let row = (world.y / CELL_H).floor() as i64;
        if col < 0 || row < 0 {
            return None;
        }
        let cell = self.layout.get(row as usize)?.cells.get(col as usize)?;
        let cps: Vec<String> = cell.emoji.chars().map(|c| format!("U+{:04X}", c as u32)).collect();
        Some(format!("{}   ·   {}", cell.name, cps.join(" ")))
    }

    fn render(
        &mut self,
        view: &wgpu::TextureView,
        w: f32,
        h: f32,
        offset: Vec2,
        scale: f32,
        hud: Option<&str>,
    ) {
        let max_row = (self.layout.len() as i64 - 1).max(0);
        let inv = 1.0 / scale;
        let r0 = ((((0.0 - offset.y) * inv) / CELL_H).floor() as i64).clamp(0, max_row);
        let r1 = ((((h - offset.y) * inv) / CELL_H).floor() as i64).clamp(0, max_row);
        self.scratch_text.clear();
        self.scratch_emoji.clear();
        for r in r0..=r1 {
            self.build_row(r);
            let (t, e) = &self.rows[&r];
            self.scratch_text.extend_from_slice(t);
            self.scratch_emoji.extend_from_slice(e);
        }
        self.emit_titles(offset, scale, h);
        self.engine.sync_atlas(&mut self.text_atlas, &self.device, &self.queue, &self.text_renderer.atlas_layout);
        let titles = self.engine.flush().to_vec();
        self.scratch_text.extend_from_slice(&titles);

        let tn = self.grid_buf.upload(&self.device, &self.queue, &self.scratch_text);
        let en = self.emoji_buf.upload(&self.device, &self.queue, &self.scratch_emoji);

        let cam = ortho(w, h) * model(offset, scale);
        self.text_renderer.write_matrix(&self.queue, cam);
        self.emoji_renderer.write_matrix(&self.queue, cam);
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("board"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.99, g: 0.99, b: 0.99, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.text_renderer.draw_vertices(&mut pass, &self.text_atlas, &self.grid_buf.buf, 0..tn);
            self.emoji_renderer.draw(&mut pass, &self.emoji_atlas, &self.emoji_buf.buf, 0..en);
        }
        self.queue.submit([enc.finish()]);

        let Some(hud) = hud else { return };
        self.engine.text(16.0, h - 16.0, hud, &args(24.0, [1.0, 0.0, 0.0, 1.0]));
        self.engine.sync_atlas(&mut self.text_atlas, &self.device, &self.queue, &self.text_renderer.atlas_layout);
        let ov = self.engine.flush().to_vec();
        let on = self.overlay_buf.upload(&self.device, &self.queue, &ov);
        self.text_renderer.write_matrix(&self.queue, ortho(w, h));
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.text_renderer.draw_vertices(&mut pass, &self.text_atlas, &self.overlay_buf.buf, 0..on);
        }
        self.queue.submit([enc.finish()]);
    }
}

fn device_descriptor() -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        label: Some("emoji_zoom"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    }
}

// ---------------------------------------------------------------------------
// Interactive window
// ---------------------------------------------------------------------------

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
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left | MouseButton::Right | MouseButton::Middle,
                ..
            } => {
                gfx.dragging = state == ElementState::Pressed;
            }
            WindowEvent::CursorMoved { position, .. } => {
                let p = Vec2::new(position.x as f32, position.y as f32);
                if gfx.dragging {
                    gfx.offset += p - gfx.cursor;
                    gfx.clamp_camera();
                    gfx.window.request_redraw();
                }
                gfx.cursor = p;
            }
            WindowEvent::RedrawRequested => gfx.draw(),
            _ => {}
        }
    }
}

struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    viewer: Viewer,
    scale: f32,
    offset: Vec2,
    cursor: Vec2,
    dragging: bool,
    samples: Vec<(Instant, f32)>,
}

impl Gfx {
    async fn new(event_loop: &ActiveEventLoop) -> Self {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("sanscale · the whole of emoji")
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
        let (device, queue) = adapter.request_device(&device_descriptor()).await.expect("device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: caps
                .present_modes
                .iter()
                .copied()
                .find(|m| *m == wgpu::PresentMode::Mailbox)
                .unwrap_or(wgpu::PresentMode::Fifo),
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);
        let viewer = Viewer::new(device, queue, &config);

        let mut gfx = Self {
            window,
            surface,
            config,
            viewer,
            scale: 1.0,
            offset: Vec2::ZERO,
            cursor: Vec2::ZERO,
            dragging: false,
            samples: Vec::new(),
        };
        gfx.reset_view();
        gfx.window.request_redraw();
        gfx
    }

    fn reset_view(&mut self) {
        self.scale = 0.9;
        self.offset = Vec2::new(10.0, 16.0);
        self.clamp_camera();
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.viewer.device, &self.config);
        self.clamp_camera();
    }

    fn zoom_at_cursor(&mut self, factor: f32) {
        let w = self.config.width as f32;
        let new_scale = (self.scale * factor).clamp(w / WORLD_W, 60.0);
        let world = (self.cursor - self.offset) / self.scale;
        self.offset = self.cursor - world * new_scale;
        self.scale = new_scale;
        self.clamp_camera();
    }

    fn clamp_camera(&mut self) {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        self.scale = self.scale.clamp(w / WORLD_W, 60.0);
        let s = self.scale;
        let margin = 40.0;
        self.offset.x = clamp_axis(self.offset.x, w - WORLD_W * s - margin, margin);
        self.offset.y = clamp_axis(self.offset.y, h - self.viewer.world_h * s - margin, margin);
    }

    fn draw(&mut self) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.viewer.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());

        let p99 = p99(&self.samples);
        let hud = match self.viewer.hovered(self.cursor, self.offset, self.scale) {
            Some(h) => format!("p99 {p99:.2} ms   ·   {h}"),
            None => format!("p99 {p99:.2} ms"),
        };

        let t0 = Instant::now();
        self.viewer.render(
            &view,
            self.config.width as f32,
            self.config.height as f32,
            self.offset,
            self.scale,
            Some(&hud),
        );
        let cost = t0.elapsed().as_secs_f32() * 1000.0;
        frame.present();

        let now = Instant::now();
        self.samples.push((now, cost));
        self.samples.retain(|(t, _)| now.duration_since(*t).as_secs_f32() < 5.0);
        self.window.request_redraw();
    }
}

fn p99(samples: &[(Instant, f32)]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f32> = samples.iter().map(|&(_, c)| c).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[(((v.len() - 1) as f32) * 0.99).round() as usize]
}

fn clamp_axis(v: f32, min: f32, max: f32) -> f32 {
    if min > max {
        (min + max) * 0.5
    } else {
        v.clamp(min, max)
    }
}

// ---------------------------------------------------------------------------
// Headless dump
// ---------------------------------------------------------------------------

fn dump() {
    let (device, queue) = pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .expect("adapter");
        adapter.request_device(&device_descriptor()).await.expect("device")
    });
    let mut viewer = Viewer::new(device, queue, &dummy_config());
    println!("{} emoji in {} rows", GROUPS.iter().map(|(_, e)| e.len()).sum::<usize>(), viewer.layout.len());

    // Top of the board (Smileys & Emotion) at a readable zoom.
    dump_png(&mut viewer, 1450, 920, Vec2::new(10.0, 16.0), 0.95, "emoji_board.png");

    // Deep zoom on a few cells — big, crisp raster emoji.
    dump_png(&mut viewer, 1000, 620, Vec2::new(10.0 - GUTTER_W * 3.0, 16.0), 3.0, "emoji_board_zoom.png");

    println!("wrote emoji_board.png and emoji_board_zoom.png");
}

fn dummy_config() -> wgpu::SurfaceConfiguration {
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        width: 16,
        height: 16,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: wgpu::CompositeAlphaMode::Opaque,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    }
}

fn dump_png(viewer: &mut Viewer, w: u32, h: u32, offset: Vec2, scale: f32, path: &str) {
    let target = viewer.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("dump"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    viewer.render(&view, w as f32, h as f32, offset, scale, None);

    let unpadded = w * 4;
    let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = viewer.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = viewer.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    viewer.queue.submit([enc.finish()]);
    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.unwrap());
    viewer.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((unpadded * h) as usize);
    for row in 0..h {
        let s = (row * padded) as usize;
        pixels.extend_from_slice(&data[s..s + unpadded as usize]);
    }
    image::save_buffer(path, &pixels, w, h, image::ColorType::Rgba8).unwrap();
}

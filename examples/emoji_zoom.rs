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
    Align, Draw, FontChainHandle, ShapedHandle, Style, Text,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use common::emoji_data::GROUPS;
use common::{copy_to_clipboard, font_chain, Hover, UNICODE_FALLBACK};

const COLS: usize = 40; // emoji per row
const CELL_W: f32 = 44.0;
const CELL_H: f32 = 44.0;
const GLYPH_PX: f32 = 34.0;
const GUTTER_W: f32 = 560.0; // world-space column reserved on the left for titles
const TITLE_PX: f32 = 52.0; // world-space title height (scales with zoom)
const WORLD_W: f32 = GUTTER_W + COLS as f32 * CELL_W;
const INK: [f32; 4] = [0.12, 0.13, 0.16, 1.0];
const TITLE: [f32; 4] = [0.16, 0.40, 0.82, 1.0];

fn main() {
    // Surface sanscale's atlas warnings on stderr.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    if std::env::args().any(|a| a == "--dump") {
        dump();
        return;
    }
    println!("scroll = zoom · drag = pan · left-click = copy emoji · right-click = copy code points · R = reset · Esc = quit");
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
/// The replacement box drawn for a sequence the fonts can't ligate.
const TOFU_BOX: &str = "\u{25A1}";

/// One placed cell: a shaped handle plus where and how big to draw it.
#[derive(Clone, Copy)]
struct PlacedCell {
    glyph: ShapedHandle,
    at: Vec2,
    size: f32,
    color: [f32; 4],
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
                // The cell's origin. Centring within it is done at placement time,
                // off the shaped block's measured box.
                cells.push(Cell { x: GUTTER_W + ci as f32 * CELL_W, emoji, name });
            }
            layout.push(RowLayout { cells, title: (ri == 0).then_some(group) });
        }
    }
    layout
}

struct Viewer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    text: Text,
    chain: FontChainHandle,
    format: wgpu::TextureFormat,
    layout: Vec<RowLayout>,
    world_h: f32,
    /// Per-row cached *placements*; the service caches the shaping and the quads.
    /// Gone with it: the `emoji_epoch` invalidation, which existed only because
    /// these used to be atlas-baked vertices held out here.
    rows: HashMap<i64, Vec<PlacedCell>>,
}

impl Viewer {
    fn new(device: wgpu::Device, queue: wgpu::Queue, config: &wgpu::SurfaceConfiguration) -> Self {
        let mut text = Text::new();
        let chain = font_chain(&mut text, UNICODE_FALLBACK);
        let layout = build_layout();
        let world_h = (layout.len() as f32 + 1.0) * CELL_H;
        Self {
            device,
            queue,
            text,
            chain,
            format: config.format,
            layout,
            world_h,
            rows: HashMap::new(),
        }
    }

    fn style(&self) -> Style {
        Style { chain: self.chain, wrap_em: None, align: Align::Left, line_spacing: 1.0 }
    }

    /// Shape and cache one row's emoji (each cell may be a multi-code-point
    /// sequence). Cheap no-op once cached.
    fn build_row(&mut self, r: i64) {
        if self.rows.contains_key(&r) {
            return;
        }
        let mut cells = Vec::new();
        if r >= 0 && (r as usize) < self.layout.len() {
            let style = self.style();
            let y = r as f32 * CELL_H;
            let row: Vec<(f32, &'static str)> = self.layout[r as usize]
                .cells
                .iter()
                .map(|cell| (cell.x, cell.emoji))
                .collect();
            for (x, emoji) in row {
                // Sequences the fonts can't ligate (a missing ZWJ/flag component)
                // shape to several glyphs that overflow the cell — show one tofu
                // box instead of the overlapping pieces. `is_single_glyph` asks
                // the face the shaper will actually pick, so this agrees with what
                // gets drawn.
                let single = self.text.diagnostics().is_single_glyph(self.chain, emoji);
                let (content, size, color) = if single {
                    (emoji, GLYPH_PX, INK)
                } else {
                    (TOFU_BOX, GLYPH_PX * 0.72, [0.72, 0.73, 0.77, 1.0])
                };
                let Some(glyph) = self.text.shape_transient(content, &style) else {
                    continue;
                };
                // Centre the glyph's `size`x`size` em box in the cell. Not the
                // measured *advance*: that varies per emoji, and centring on it
                // makes the columns ragged — the grid wants every cell on the same
                // x, which is what the old fixed pad was doing.
                //
                // Vertically the service owns the baseline, so ask it where the
                // baseline sits within the block (`baseline_em`) and place `at` so
                // the glyph box lands centred. This is the compensation the port
                // dropped when `at` became the block's top-left instead of a
                // baseline — recovered from metrics rather than a tuned constant,
                // so the smaller tofu box centres by the same rule.
                let inset = (CELL_H - size) * 0.5;
                let baseline_em = self
                    .text
                    .measure(glyph)
                    .line(0)
                    .map(|m| m.baseline_em)
                    .unwrap_or(1.0);
                let at = Vec2::new(
                    x + (CELL_W - size) * 0.5,
                    y + inset + size - baseline_em * size,
                );
                cells.push(PlacedCell { glyph, at, size, color });
            }
        }
        self.rows.insert(r, cells);
    }

    /// World-space category titles in the gutter — same offset-invariant dedup as
    /// `unicode_zoom` so panning doesn't reshuffle which show.
    fn title_cells(&mut self, offset: Vec2, scale: f32, h: f32) -> Vec<PlacedCell> {
        let style = self.style();
        let min_gap = TITLE_PX * scale * 1.1;
        let mut last_kept = f32::MIN;
        let mut cells = Vec::new();
        let titles: Vec<(i64, &'static str)> = self
            .layout
            .iter()
            .enumerate()
            .filter_map(|(i, row)| row.title.map(|t| (i as i64, t)))
            .collect();
        for (r, name) in titles {
            let screen_y = r as f32 * CELL_H * scale + offset.y;
            if screen_y < last_kept + min_gap {
                continue;
            }
            last_kept = screen_y;
            if screen_y >= -TITLE_PX * scale && screen_y <= h {
                if let Some(glyph) = self.text.shape_transient(name, &style) {
                    cells.push(PlacedCell {
                        glyph,
                        at: Vec2::new(10.0, r as f32 * CELL_H),
                        size: TITLE_PX,
                        color: TITLE,
                    });
                }
            }
        }
        cells
    }

    fn hovered(&self, cursor: Vec2, offset: Vec2, scale: f32) -> Option<Hover> {
        let world = (cursor - offset) / scale;
        let col = ((world.x - GUTTER_W) / CELL_W).floor() as i64;
        let row = (world.y / CELL_H).floor() as i64;
        if col < 0 || row < 0 {
            return None;
        }
        let cell = self.layout.get(row as usize)?.cells.get(col as usize)?;
        let code = cell
            .emoji
            .chars()
            .map(|c| format!("U+{:04X}", c as u32))
            .collect::<Vec<_>>()
            .join(" ");
        let family = cell
            .emoji
            .chars()
            .next()
            .and_then(|c| self.text.diagnostics().family_for(self.chain, c))
            .unwrap_or_else(|| "—".into());
        // Stable fields first (padded); the noisy variable-length name goes last.
        Some(Hover {
            label: format!("{:<18}   ·   {:<26}   ·   {}", family, code, cell.name),
            text: cell.emoji.to_string(),
            code,
        })
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
        let inv = 1.0 / scale;
        let r0 = ((((0.0 - offset.y) * inv) / CELL_H).floor() as i64).max(0);
        let r1 = ((((h - offset.y) * inv) / CELL_H).floor() as i64)
            .min(self.layout.len() as i64 - 1);
        let mut cells: Vec<PlacedCell> = Vec::new();
        for r in r0..=r1 {
            self.build_row(r);
            cells.extend_from_slice(&self.rows[&r]);
        }
        cells.extend(self.title_cells(offset, scale, h));

        self.text.set_target(&self.device, self.format);
        // The pan/zoom camera is just the transform — same call screen-space text
        // uses, different matrix.
        let cam = ortho(w, h) * model(offset, scale);
        self.text.set_transform(&self.queue, cam.to_cols_array());

        let batch: Vec<Draw> = cells
            .iter()
            .map(|cell| Draw {
                block: cell.glyph,
                at: sanscale::Vec2::new(cell.at.x, cell.at.y),
                size: cell.size,
                color: sanscale::Color(cell.color),
                clip: None,
            })
            .collect();

        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("board"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
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
            self.text
                .draw_batch(&self.device, &self.queue, &mut pass, &batch);
        }
        self.queue.submit([enc.finish()]);

        let Some(hud) = hud else { return };
        let style = self.style();
        let Some(overlay) = self.text.shape_transient(hud, &style) else {
            return;
        };
        self.text
            .set_transform(&self.queue, Text::pixel_ortho(w as u32, h as u32));
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.text.draw(
                &self.device,
                &self.queue,
                &mut pass,
                overlay,
                sanscale::Vec2::new(16.0, h - 40.0),
                24.0,
                sanscale::Color([1.0, 0.0, 0.0, 1.0]),
                None,
            );
        }
        self.queue.submit([enc.finish()]);
    }
}

/// Request the adapter's full 2D texture size (default limits cap it at 8192,
/// which the emoji atlas can exceed) so more color glyphs fit before overflow.
fn device_descriptor(max_texture_dimension_2d: u32) -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        label: Some("emoji_zoom"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits { max_texture_dimension_2d, ..Default::default() },
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
            // Drag to pan; a click (no drag) copies — left the emoji, right its code points.
            WindowEvent::MouseInput {
                state,
                button: button @ (MouseButton::Left | MouseButton::Right | MouseButton::Middle),
                ..
            } => match state {
                ElementState::Pressed => {
                    gfx.dragging = true;
                    gfx.press_pos = gfx.cursor;
                    gfx.moved = false;
                }
                ElementState::Released => {
                    gfx.dragging = false;
                    if !gfx.moved {
                        gfx.on_click(button);
                    }
                }
            },
            WindowEvent::CursorMoved { position, .. } => {
                let p = Vec2::new(position.x as f32, position.y as f32);
                if gfx.dragging {
                    gfx.offset += p - gfx.cursor;
                    if (p - gfx.press_pos).length() > 4.0 {
                        gfx.moved = true;
                    }
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
    press_pos: Vec2,
    moved: bool,
    copied: Option<(String, Instant)>,
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
                apply_limit_buckets: false,
            })
            .await
            .expect("adapter");
        let (device, queue) = adapter.request_device(&device_descriptor(adapter.limits().max_texture_dimension_2d)).await.expect("device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
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
            press_pos: Vec2::ZERO,
            moved: false,
            copied: None,
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

    fn on_click(&mut self, button: MouseButton) {
        let Some(h) = self.viewer.hovered(self.cursor, self.offset, self.scale) else { return };
        let s = match button {
            MouseButton::Left => h.text,
            MouseButton::Right => h.code,
            _ => return,
        };
        copy_to_clipboard(&s);
        println!("copied: {s}");
        self.copied = Some((s, Instant::now()));
        self.window.request_redraw();
    }

    /// Minimum zoom: far enough out that the whole board (width *or* height,
    /// whichever binds) fits — so you can always see the full set.
    fn min_scale(&self) -> f32 {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        (w / WORLD_W).min(h / self.viewer.world_h)
    }

    fn zoom_at_cursor(&mut self, factor: f32) {
        let new_scale = (self.scale * factor).clamp(self.min_scale(), 60.0);
        let world = (self.cursor - self.offset) / self.scale;
        self.offset = self.cursor - world * new_scale;
        self.scale = new_scale;
        self.clamp_camera();
    }

    fn clamp_camera(&mut self) {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        self.scale = self.scale.clamp(self.min_scale(), 60.0);
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
        let mut hud = match self.viewer.hovered(self.cursor, self.offset, self.scale) {
            Some(h) => format!("p99 {p99:6.2} ms   ·   {}", h.label),
            None => format!("p99 {p99:6.2} ms"),
        };
        if let Some((s, t)) = &self.copied {
            if t.elapsed().as_secs_f32() < 2.0 {
                hud = format!("{hud}   ·   copied {s}");
            }
        }
        // Atlas pressure, so overflow is never invisible: current height and, if the
        // working set ever exceeds the budget, the count of glyphs it had to drop.
        let (_, _, (_, atlas_h)) = self.viewer.text.diagnostics().atlas_sizes();
        hud = format!("{hud}   ·   atlas {atlas_h}px");
        let dropped = self.viewer.text.diagnostics().dropped_glyphs();
        if dropped > 0 {
            hud = format!("{hud}   ·   DROPPED {dropped}");
        }

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
        self.viewer.queue.present(frame);

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
                apply_limit_buckets: false,
            })
            .await
            .expect("adapter");
        adapter.request_device(&device_descriptor(adapter.limits().max_texture_dimension_2d)).await.expect("device")
    });
    let mut viewer = Viewer::new(device, queue, &dummy_config());
    println!("{} emoji in {} rows", GROUPS.iter().map(|(_, e)| e.len()).sum::<usize>(), viewer.layout.len());

    // Top of the board (Smileys & Emotion) at a readable zoom.
    dump_png(&mut viewer, 1450, 920, Vec2::new(10.0, 16.0), 0.95, "emoji_board.png");

    // Deep zoom on a few cells — big, crisp raster emoji.
    dump_png(&mut viewer, 1000, 620, Vec2::new(10.0 - GUTTER_W * 3.0, 16.0), 3.0, "emoji_board_zoom.png");

    // Pan the whole board a viewport at a time (each render() is a frame), filling the
    // bounded atlas and forcing cross-frame eviction, then dump the Flags category — the
    // regression the eviction work fixes (flags used to vanish after enough panning).
    sweep_board(&mut viewer, 920, 0.95);
    let flags_row = viewer.layout.iter().position(|r| r.title == Some("Flags")).unwrap();
    let flags_y = 16.0 - flags_row as f32 * CELL_H * 0.95;
    dump_png(&mut viewer, 1450, 920, Vec2::new(10.0, flags_y), 0.95, "emoji_flags.png");

    let (_, _, (aw, ah)) = viewer.text.diagnostics().atlas_sizes();
    println!(
        "wrote emoji_board.png, emoji_board_zoom.png, emoji_flags.png \
         (emoji atlas {aw}x{ah} after full sweep, dropped {})",
        viewer.text.diagnostics().dropped_glyphs()
    );
}

/// Render every viewport slice of the board once, off-screen, so the bounded atlas
/// fills and starts evicting — the churn a real pan produces. No read-back.
fn sweep_board(viewer: &mut Viewer, vh: u32, scale: f32) {
    let target = viewer.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sweep"),
        size: wgpu::Extent3d { width: 1450, height: vh, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let mut y = 0.0;
    while y > -(viewer.world_h * scale) {
        viewer.render(&view, 1450.0, vh as f32, Vec2::new(10.0, 16.0 + y), scale, None);
        y -= vh as f32;
    }
}

fn dummy_config() -> wgpu::SurfaceConfiguration {
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        color_space: wgpu::SurfaceColorSpace::Auto,
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
    let data = slice.get_mapped_range().unwrap();
    let mut pixels = Vec::with_capacity((unpadded * h) as usize);
    for row in 0..h {
        let s = (row * padded) as usize;
        pixels.extend_from_slice(&data[s..s + unpadded as usize]);
    }
    image::save_buffer(path, &pixels, w, h, image::ColorType::Rgba8).unwrap();
}

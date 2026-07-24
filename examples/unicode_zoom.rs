//! The ultimate example: a zoomable map of the whole Unicode codespace.
//!
//! Fixed Unifont-style grid — 256 columns, `code point = row*256 + col` — across
//! planes 0–2. Nothing is enumerated up front: each frame culls to the visible
//! cells and only shapes/draws those. Covered code points draw their glyph; the
//! rest draw a faint tofu box. A reserved left gutter holds big **world-space**
//! block titles that scale with the map like everything else. Below a minimum
//! on-screen cell size the glyphs are culled (the titles carry navigation). It
//! all stays razor-sharp at any zoom because coverage is analytic — only the
//! color-emoji raster atlas pixelates.
//!
//! Interactive:  `cargo run --example unicode_zoom`
//!     scroll = zoom at cursor · drag (any button) = pan · R = reset · Esc = quit
//! Headless PNGs: `cargo run --example unicode_zoom -- --dump`

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

use common::{copy_to_clipboard, font_chain, Hover, UNICODE_FALLBACK};

const COLS: i64 = 256;
const CELL_W: f32 = 20.0;
const CELL_H: f32 = 22.0;
const GLYPH_PX: f32 = 15.0;
const MAX_ROW: i64 = 0x2_FFFF / COLS; // planes 0–2
const GUTTER_W: f32 = 560.0; // world-space column reserved on the left for titles
const TITLE_PX: f32 = 52.0; // world-space title height (scales with zoom)
const WORLD_W: f32 = GUTTER_W + COLS as f32 * CELL_W;
const WORLD_H: f32 = (MAX_ROW as f32 + 1.0) * CELL_H;
const INK: [f32; 4] = [0.12, 0.13, 0.16, 1.0];
const TOFU: [f32; 4] = [0.80, 0.81, 0.84, 1.0];
const TITLE: [f32; 4] = [0.16, 0.40, 0.82, 1.0];

/// Major Unicode blocks (start, short name), ascending. Names are kept short so
/// they fit the title gutter at the world-space title size.
const BLOCKS: &[(u32, &str)] = &[
    (0x0000, "Basic Latin"), (0x0080, "Latin-1 Suppl."), (0x0100, "Latin Ext-A"),
    (0x0180, "Latin Ext-B"), (0x0250, "IPA Extensions"), (0x0300, "Combining Marks"),
    (0x0370, "Greek"), (0x0400, "Cyrillic"), (0x0530, "Armenian"), (0x0590, "Hebrew"),
    (0x0600, "Arabic"), (0x0700, "Syriac"), (0x0900, "Devanagari"), (0x0980, "Bengali"),
    (0x0B80, "Tamil"), (0x0C00, "Telugu"), (0x0D00, "Malayalam"), (0x0E00, "Thai"),
    (0x0E80, "Lao"), (0x0F00, "Tibetan"), (0x1000, "Myanmar"), (0x10A0, "Georgian"),
    (0x1100, "Hangul Jamo"), (0x1200, "Ethiopic"), (0x13A0, "Cherokee"), (0x1780, "Khmer"),
    (0x1800, "Mongolian"), (0x1E00, "Latin Ext. Add'l"), (0x1F00, "Greek Ext."),
    (0x2000, "Punctuation"), (0x20A0, "Currency"), (0x2100, "Letterlike"), (0x2190, "Arrows"),
    (0x2200, "Math Operators"), (0x2300, "Misc Technical"), (0x2460, "Enclosed Alnum"),
    (0x2500, "Box Drawing"), (0x2600, "Misc Symbols"), (0x2700, "Dingbats"), (0x2800, "Braille"),
    (0x2E80, "CJK Radicals"), (0x3000, "CJK Symbols"), (0x3040, "Hiragana"), (0x30A0, "Katakana"),
    (0x3100, "Bopomofo"), (0x3130, "Hangul Compat."), (0x3400, "CJK Ext-A"),
    (0x4E00, "CJK Ideographs"), (0xA000, "Yi Syllables"), (0xAC00, "Hangul Syllables"),
    (0xF900, "CJK Compat."), (0xFB00, "Present. Forms"), (0xFF00, "Half/Fullwidth"),
    (0x10000, "Linear B (pl.1)"), (0x1D400, "Math Alnum."), (0x1F300, "Pictographs"),
    (0x1F600, "Emoticons"), (0x1F680, "Transport"), (0x20000, "CJK Ext-B (pl.2)"),
];

fn main() {
    if std::env::args().any(|a| a == "--dump") {
        dump();
        return;
    }
    println!("scroll = zoom · drag = pan · left-click = copy char · right-click = copy code point · R = reset · Esc = quit");
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

/// Largest size (≤ `GLYPH_PX`) at which a glyph of this em bbox fits its cell.
fn fit_scale(mnx: f32, mny: f32, mxx: f32, mxy: f32) -> f32 {
    let gw = (mxx - mnx).max(1e-3);
    let gh = (mxy - mny).max(1e-3);
    GLYPH_PX.min((CELL_W - 3.0) / gw).min((CELL_H - 5.0) / gh)
}

/// A vertex buffer that's written in place each frame and only reallocated when
/// it needs to grow — avoids allocating a fresh GPU buffer every frame.
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
    /// Upload `data`, growing the buffer if needed; returns the element count.
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

/// Owns the engine, GPU renderers, and atlases; renders one culled frame on demand.
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
    /// Per-row cached vertices. A cell's world position and glyph are fixed by its
    /// code point, so a row is shaped/emitted once and then just re-gathered.
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
            rows: HashMap::new(),
            scratch_text: Vec::new(),
            scratch_emoji: Vec::new(),
        }
    }

    /// Shape and cache one row's cells (covered → glyph, else → tofu box). Cheap
    /// no-op once the row is cached.
    fn build_row(&mut self, r: i64) {
        if self.rows.contains_key(&r) {
            return;
        }
        let tofu = args(GLYPH_PX, TOFU);
        let mut buf = [0u8; 4];
        let base = r * COLS;
        for c in 0..COLS {
            let cp = (base + c) as u32;
            if (0xD800..=0xDFFF).contains(&cp) {
                continue;
            }
            let Some(ch) = char::from_u32(cp) else { continue };
            if ch.is_control() || ch.is_whitespace() {
                continue;
            }
            let x = GUTTER_W + c as f32 * CELL_W + 3.0;
            let y = r as f32 * CELL_H + 16.0;
            if !self.engine.covers(ch) {
                self.engine.text(x, y, "\u{25A1}", &tofu);
                continue;
            }
            // Some glyphs (PUA icons, Cuneiform, …) extend well past 1em; shrink
            // and center those to their cell so they don't overflow into
            // neighbors. Normal glyphs keep the baseline grid.
            match self.engine.glyph_bbox(ch) {
                Some((mnx, mny, mxx, mxy)) if fit_scale(mnx, mny, mxx, mxy) < GLYPH_PX => {
                    let s = fit_scale(mnx, mny, mxx, mxy);
                    let ccx = GUTTER_W + c as f32 * CELL_W + CELL_W * 0.5;
                    let ccy = r as f32 * CELL_H + CELL_H * 0.5;
                    let px = ccx - (mnx + mxx) * 0.5 * s;
                    let py = ccy + (mny + mxy) * 0.5 * s;
                    self.engine.text(px, py, ch.encode_utf8(&mut buf), &args(s, INK));
                }
                _ => {
                    self.engine.text(x, y, ch.encode_utf8(&mut buf), &args(GLYPH_PX, INK));
                }
            }
        }
        self.engine.sync_atlas(&mut self.text_atlas, &self.device, &self.queue, &self.text_renderer.atlas_layout);
        self.engine.sync_emoji_atlas(&mut self.emoji_atlas, &self.device, &self.queue, &self.emoji_renderer.atlas_layout);
        let t = self.engine.flush().to_vec();
        let e = self.engine.emoji_vertices().to_vec();
        self.rows.insert(r, (t, e));
    }

    /// World-space block titles in the gutter — big, scaling with the map. Kept
    /// from overlapping via a *screen-space* gap so zooming in reveals more of
    /// them. The winner set is decided over *all* blocks (the gap test is
    /// offset-invariant, since baseline differences cancel `offset.y`), and only
    /// drawing is gated on visibility — so panning a title off-screen no longer
    /// reshuffles which of the others show.
    fn emit_titles(&mut self, offset: Vec2, scale: f32, h: f32) {
        let args = args(TITLE_PX, TITLE);
        let min_gap = TITLE_PX * scale * 1.1; // screen px between title baselines
        let mut last_kept = f32::MIN;
        for &(start, name) in BLOCKS {
            let r = (start as i64 / COLS) as f32;
            let screen_y = r * CELL_H * scale + offset.y;
            if screen_y < last_kept + min_gap {
                continue; // loses its slot to a nearby title above it
            }
            last_kept = screen_y; // wins its slot whether or not it's on-screen
            if screen_y >= -TITLE_PX * scale && screen_y <= h {
                self.engine.text(10.0, r * CELL_H + TITLE_PX * 0.82, name, &args);
            }
        }
    }

    /// The code point under the cursor (with its resolved font family), or `None`.
    fn hovered(&self, cursor: Vec2, offset: Vec2, scale: f32) -> Option<Hover> {
        let world = (cursor - offset) / scale;
        let col = ((world.x - GUTTER_W) / CELL_W).floor() as i64;
        let row = (world.y / CELL_H).floor() as i64;
        if !(0..COLS).contains(&col) || !(0..=MAX_ROW).contains(&row) {
            return None;
        }
        let cp = (row * COLS + col) as u32;
        if (0xD800..=0xDFFF).contains(&cp) {
            return None;
        }
        let ch = char::from_u32(cp)?;
        let block = BLOCKS.iter().rev().find(|(s, _)| *s <= cp).map(|(_, n)| *n).unwrap_or("—");
        let family = self.engine.family_for(ch).unwrap_or_else(|| "—".into());
        let shown = if ch.is_control() || ch.is_whitespace() { ' ' } else { ch };
        Some(Hover {
            label: format!("U+{cp:04X}  {shown}  ·  {block}  ·  {family}"),
            text: ch.to_string(),
            code: format!("U+{cp:04X}"),
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
        // Gather cached vertices for the visible rows (building any not yet seen).
        let inv = 1.0 / scale;
        let r0 = ((((0.0 - offset.y) * inv) / CELL_H).floor() as i64).clamp(0, MAX_ROW);
        let r1 = ((((h - offset.y) * inv) / CELL_H).floor() as i64).clamp(0, MAX_ROW);
        self.scratch_text.clear();
        self.scratch_emoji.clear();
        for r in r0..=r1 {
            self.build_row(r);
            let (t, e) = &self.rows[&r];
            self.scratch_text.extend_from_slice(t);
            self.scratch_emoji.extend_from_slice(e);
        }
        // Titles depend on the camera, so emit them fresh each frame and append.
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
                label: Some("map"),
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

        // Screen-space debug line (bottom-left), drawn over the map in a second pass.
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
        label: Some("unicode_zoom"),
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
            // Any button drags to pan; a button *click* (no drag) copies — left
            // copies the character, right copies the code point.
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
    moved: bool, // did the cursor move since the button went down (drag vs click)
    copied: Option<(String, Instant)>, // last copied string, for brief HUD feedback
    samples: Vec<(Instant, f32)>, // (timestamp, render-cost ms) over ~5s
}

impl Gfx {
    async fn new(event_loop: &ActiveEventLoop) -> Self {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("sanscale · the whole of Unicode")
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
            // Lowest-latency present mode available (Mailbox → drag tracks the
            // cursor without buffering extra frames), and only 1 frame in flight.
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
        self.scale = 0.6;
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

    /// Copy the hovered character (left) or its code point (right) to the clipboard.
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

    fn zoom_at_cursor(&mut self, factor: f32) {
        let w = self.config.width as f32;
        let min_scale = w / WORLD_W; // never zoom out past the full map width
        let new_scale = (self.scale * factor).clamp(min_scale, 60.0);
        let world = (self.cursor - self.offset) / self.scale;
        self.offset = self.cursor - world * new_scale;
        self.scale = new_scale;
        self.clamp_camera();
    }

    /// Keep the camera over the map (plus a small margin), never off in the void.
    fn clamp_camera(&mut self) {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        self.scale = self.scale.clamp(w / WORLD_W, 60.0);
        let s = self.scale;
        let margin = 40.0;
        self.offset.x = clamp_axis(self.offset.x, w - WORLD_W * s - margin, margin);
        self.offset.y = clamp_axis(self.offset.y, h - WORLD_H * s - margin, margin);
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

        // Debug line: p99 of the last ~5s of per-frame CPU render cost, the hovered
        // code point (+ its font), and a brief note after a copy.
        let p99 = p99(&self.samples);
        let mut hud = match self.viewer.hovered(self.cursor, self.offset, self.scale) {
            Some(h) => format!("p99 {p99:.2} ms   ·   {}", h.label),
            None => format!("p99 {p99:.2} ms"),
        };
        if let Some((s, t)) = &self.copied {
            if t.elapsed().as_secs_f32() < 2.0 {
                hud = format!("{hud}   ·   copied {s}");
            }
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
        frame.present();

        let now = Instant::now();
        self.samples.push((now, cost));
        self.samples.retain(|(t, _)| now.duration_since(*t).as_secs_f32() < 5.0);
        // Keep sampling continuously so the p99 window stays live.
        self.window.request_redraw();
    }
}

/// p99 of the sample costs, in ms.
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
        (min + max) * 0.5 // content smaller than viewport on this axis: center it
    } else {
        v.clamp(min, max)
    }
}

// ---------------------------------------------------------------------------
// Headless dump: a regional view + a deep zoom (no window)
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

    // Regional: Latin → CJK-radicals, big world-space titles + glyph grid + tofu.
    dump_png(&mut viewer, 1450, 920, Vec2::new(10.0, 16.0), 0.9, "unicode_map.png");

    // Deep zoom on the CJK Unified Ideographs block — big, crisp curves.
    let row = (0x4E00i64 / COLS) as f32;
    let scale = 2.6;
    let offset = Vec2::new(40.0 - GUTTER_W * scale, 40.0 - row * CELL_H * scale);
    dump_png(&mut viewer, 1000, 620, offset, scale, "unicode_map_zoom.png");

    println!("wrote unicode_map.png (regional) and unicode_map_zoom.png (CJK deep zoom)");

    // Perf probe: time the per-frame CPU render cost (emit + buffer upload +
    // encode + submit) for a dense, fully zoomed-out frame — the worst case now
    // that there is no LOD cull. Warm the caches first, then report percentiles.
    let (w, h) = (1600u32, 1000u32);
    let target = viewer.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let scale = w as f32 / WORLD_W; // fit whole width → most cells on screen
    let offset = Vec2::new(0.0, 0.0);
    let cells = {
        let cols = COLS;
        let rows = (h as f32 / (CELL_H * scale)).ceil() as i64;
        cols * rows
    };
    for _ in 0..5 {
        viewer.render(&view, w as f32, h as f32, offset, scale, None);
    }
    let mut times = Vec::new();
    for _ in 0..60 {
        let t = Instant::now();
        viewer.render(&view, w as f32, h as f32, offset, scale, None);
        times.push(t.elapsed().as_secs_f32() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = times[times.len() / 2];
    let p99 = times[((times.len() - 1) as f32 * 0.99) as usize];
    println!(
        "perf probe: ~{cells} cells/frame at fit-width, CPU render median {med:.2} ms · p99 {p99:.2} ms"
    );
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

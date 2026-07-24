//! Shared headless-wgpu boilerplate for the examples. Not part of the library —
//! it spins up a GPU device with no window, discovers system fonts, and draws
//! sanscale vertices (glyphs + color emoji) so the examples can focus on the API.

#![allow(dead_code)] // each example uses a different subset of these helpers.

use glam::Mat4;
use sanscale::{EmojiAtlas, EmojiRenderer, FontSource, TextAtlas, TextRenderer, TextVertex};

// Common system fonts, tried in order so the single-font examples run anywhere.
pub const FONT_CANDIDATES: &[&str] = &[
    "C:/Windows/Fonts/segoeui.ttf",
    "C:/Windows/Fonts/arial.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
    "/Library/Fonts/Arial.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

/// A broad fallback chain (by family name) covering Latin, CJK, Indic, emoji, and
/// symbols. Families absent on this OS are skipped, so the same list works across
/// platforms — the emoji/CJK samples simply need *some* covering face installed.
pub const UNICODE_FALLBACK: &[&str] = &[
    // Latin / UI
    "Segoe UI", "Helvetica Neue", "DejaVu Sans", "Arial", "Noto Sans",
    // CJK
    "Microsoft YaHei", "PingFang SC", "Noto Sans CJK SC", "Hiragino Sans",
    "Malgun Gothic", "Noto Sans CJK KR", "Yu Gothic", "Noto Sans CJK JP",
    // Indic / SE-Asia
    "Nirmala UI", "Noto Sans Devanagari", "Leelawadee UI", "Noto Sans Thai",
    // Color emoji
    "Segoe UI Emoji", "Apple Color Emoji", "Noto Color Emoji",
    // Symbols
    "Segoe UI Symbol", "Noto Sans Symbols 2", "DejaVu Sans",
];

/// Resolve family names (or explicit file paths) into loadable font sources via
/// fontdb. Mirrors how a real app builds its fallback chain.
pub fn font_chain(families: &[&str]) -> Vec<FontSource<'static>> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let mut sources = Vec::new();
    for name in families {
        if name.contains(['/', '\\']) || name.ends_with(".ttf") || name.ends_with(".ttc") {
            if let Ok(bytes) = std::fs::read(name) {
                sources.push(FontSource::Bytes(bytes, 0));
            }
            continue;
        }
        let families = [fontdb::Family::Name(name)];
        let query = fontdb::Query {
            families: &families,
            ..Default::default()
        };
        if let Some(id) = db.query(&query) {
            if let Some(src) =
                db.with_face_data(id, |data, index| FontSource::Bytes(data.to_vec(), index))
            {
                sources.push(src);
            }
        }
    }
    sources
}

/// Labelled multilingual samples, shared by the Unicode examples.
pub fn unicode_sections() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Latin + accents", "The quick brown fox — jüber naïve Æsop, coöperate £€$¥."),
        ("Greek", "Ζεύς· ἀλήθεια καὶ σοφία. Μαθηματικά: αβγδ ΔΣΩ π≈3.14159."),
        ("Cyrillic", "Съешь ещё этих мягких французских булок да выпей чаю."),
        ("Chinese 中文", "床前明月光，疑是地上霜。举头望明月，低头思故乡。"),
        ("Japanese 日本語", "いろはにほへと ちりぬるを — 平仮名・片仮名・漢字。"),
        ("Korean 한국어", "다람쥐 헌 쳇바퀴에 타고파. 훈민정음 한글."),
        ("Arabic العربية", "العربية لغة جميلة ومعقدة."),
        ("Hebrew עברית", "עברית: שלום עולם."),
        ("Devanagari", "नमस्ते दुनिया — देवनागरी लिपि।"),
        ("Thai", "ภาษาไทย สวัสดีชาวโลก"),
        ("Symbols & math", "∀x∈ℝ ∃y: x²≥0 ∑∫√∞ ← ↑ → ↓ ↔ ⇒ ✓ ✗ ★ ☆ ♠♥♦♣"),
        ("Emoji 🎨", "😀 😎 🤖 🎉 🚀 🌍 ❤️ 🔥 ✨ 🐙 🍜 🎧 🏔️ 🌈 👋"),
    ]
}

/// A headless GPU device plus the offscreen surface config the renderer needs.
pub struct Harness {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
}

impl Harness {
    pub fn new(width: u32, height: u32) -> Self {
        let (device, queue) = pollster::block_on(request_device());
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        Self {
            device,
            queue,
            config,
        }
    }

    fn offscreen(&self) -> (wgpu::Texture, wgpu::TextureView) {
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d {
                width: self.config.width,
                height: self.config.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        (target, view)
    }

    fn matrix(&self) -> Mat4 {
        Mat4::orthographic_rh(
            0.0,
            self.config.width as f32,
            self.config.height as f32,
            0.0,
            -1.0,
            1.0,
        )
    }

    /// Draw a flushed text vertex slice to a PNG (no emoji).
    pub fn save_png(
        &self,
        renderer: &TextRenderer,
        atlas: &TextAtlas,
        vertices: &[TextVertex],
        clear: wgpu::Color,
        path: &str,
    ) {
        let (target, view) = self.offscreen();
        let buffer = TextRenderer::build_vertices(&self.device, vertices);
        let mut encoder = self.device.create_command_encoder(&Default::default());
        renderer.render(
            &self.queue,
            &mut encoder,
            &view,
            atlas,
            &buffer,
            vertices.len() as u32,
            self.matrix(),
            (self.config.width, self.config.height),
            Some(clear),
        );
        self.queue.submit([encoder.finish()]);
        self.write_png(&target, path);
    }

    /// Draw text and color-emoji vertices in one pass (emoji over text), to a PNG.
    #[allow(clippy::too_many_arguments)]
    pub fn save_png_with_emoji(
        &self,
        text_renderer: &TextRenderer,
        text_atlas: &TextAtlas,
        text_vertices: &[TextVertex],
        emoji_renderer: &EmojiRenderer,
        emoji_atlas: &EmojiAtlas,
        emoji_vertices: &[sanscale::EmojiVertex],
        clear: wgpu::Color,
        path: &str,
    ) {
        let (target, view) = self.offscreen();
        let matrix = self.matrix();
        text_renderer.write_matrix(&self.queue, matrix);
        emoji_renderer.write_matrix(&self.queue, matrix);
        let text_buf = TextRenderer::build_vertices(&self.device, text_vertices);
        let emoji_buf = EmojiRenderer::build_vertices(&self.device, emoji_vertices);

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("text+emoji"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            text_renderer.draw_vertices(&mut pass, text_atlas, &text_buf, 0..text_vertices.len() as u32);
            emoji_renderer.draw(&mut pass, emoji_atlas, &emoji_buf, 0..emoji_vertices.len() as u32);
        }
        self.queue.submit([encoder.finish()]);
        self.write_png(&target, path);
    }

    fn write_png(&self, texture: &wgpu::Texture, path: &str) {
        let (width, height) = (self.config.width, self.config.height);
        let unpadded = width * 4;
        let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.unwrap());
        self.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((unpadded * height) as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            pixels.extend_from_slice(&data[start..start + unpadded as usize]);
        }
        image::save_buffer(path, &pixels, width, height, image::ColorType::Rgba8)
            .unwrap_or_else(|e| panic!("write {path}: {e}"));
    }
}

async fn request_device() -> (wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .expect("no GPU adapter");
    adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("sanscale example device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        })
        .await
        .expect("request device")
}

//! Shared headless-wgpu boilerplate for the examples. Not part of the library —
//! it spins up a GPU device with no window, discovers system fonts, and draws
//! sanscale vertices (glyphs + color emoji) so the examples can focus on the API.

#![allow(dead_code)] // each example uses a different subset of these helpers.

use sanscale::{FontChainHandle, FontData, Text};

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
///
/// Color emoji fonts are placed **near the front** so that codepoints a text font
/// would also cover (☺ ❤ ✈ ★ …) resolve to the color face rather than a mono text
/// glyph. The primary Latin font still precedes them so ASCII digits/letters stay
/// text (emoji fonts also map 0-9 # * for keycap sequences). Note: this is a
/// coarse fix — there is no Unicode emoji-presentation / VS16 handling yet, so
/// text-default symbols the primary font covers can still come out monochrome.
pub const UNICODE_FALLBACK: &[&str] = &[
    // Primary Latin first (keeps digits/letters as text).
    "Segoe UI",
    // Color emoji, high priority. Twemoji (COLR) is a fallback for sequences the
    // platform emoji font can't form — notably flags, which Segoe UI Emoji omits.
    "Segoe UI Emoji", "Apple Color Emoji", "Noto Color Emoji", "Twemoji Mozilla",
    // More Latin.
    "Helvetica Neue", "Arial", "Noto Sans", "DejaVu Sans",
    // CJK.
    "Microsoft YaHei", "Malgun Gothic", "Yu Gothic", "Noto Sans CJK SC",
    "Noto Sans CJK KR", "Noto Sans CJK JP", "PingFang SC", "Hiragino Sans",
    // Indic / SE-Asia / African.
    "Nirmala UI", "Noto Sans Devanagari", "Leelawadee UI", "Noto Sans Thai", "Ebrima",
    // Symbols / math / historic — broad coverage to reduce tofu.
    "Segoe UI Symbol", "Noto Sans Symbols 2", "Cambria Math", "Segoe UI Historic", "Sylfaen",
];

/// Resolve family names (or explicit file paths) into a registered fallback
/// chain. Mirrors how a real app does it: **one** long-lived `Database`, and
/// `make_shared_face_data` so a face used by two chains is the same `Arc` and
/// the service dedups it into one font and one glyph cache.
pub fn font_chain(text: &mut Text, families: &[&str]) -> FontChainHandle {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let mut handles = Vec::new();
    for name in families {
        let is_path = name.contains('/')
            || name.contains(std::path::MAIN_SEPARATOR)
            || name.ends_with(".ttf")
            || name.ends_with(".ttc");
        let shared: Option<(FontData, u32)> = if is_path {
            sanscale::read_font_file(name).ok().map(|data| (data, 0))
        } else {
            let families = [fontdb::Family::Name(name)];
            let query = fontdb::Query {
                families: &families,
                ..Default::default()
            };
            // SAFETY: fontdb mmaps the file; the process must not have the font
            // replaced underneath it. The same contract every mmap font stack takes.
            db.query(&query)
                .and_then(|id| unsafe { db.make_shared_face_data(id) })
        };
        if let Some((data, index)) = shared {
            if let Ok(handle) = text.map_font(data, index) {
                handles.push(handle);
            }
        }
    }
    text.register_chain(&handles)
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
        ("Emoji 🎨", "😀 😆 😅 🤣 😍 😎 🤖 🎉 🚀 🌍 ❤️ 🔥 ✨ 🐙 🍜 👋"),
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

    /// Render one frame through the service and write it to a PNG.
    ///
    /// The service owns its atlases and pipelines, so the harness only supplies
    /// a pass — exactly the seam a real consumer sits on.
    pub fn save_png(
        &self,
        text: &mut Text,
        clear: wgpu::Color,
        path: &str,
        draw: impl FnOnce(&mut Text, &wgpu::Device, &wgpu::Queue, &mut wgpu::RenderPass<'_>),
    ) {
        let (target, view) = self.offscreen();
        text.set_target(&self.device, self.config.format);
        text.set_transform(
            &self.queue,
            Text::pixel_ortho(self.config.width, self.config.height),
        );

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("text"),
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
            draw(text, &self.device, &self.queue, &mut pass);
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

//! Shared headless-wgpu boilerplate for the examples. Not part of the library —
//! it just spins up a GPU device with no window and draws sanscale vertices to a
//! PNG so the examples can focus on the text API.

use glam::Mat4;
use sanscale::{TextAtlas, TextRenderer};

// Common system fonts, tried in order so the examples run unmodified anywhere.
pub const FONT_CANDIDATES: &[&str] = &[
    "C:/Windows/Fonts/segoeui.ttf",
    "C:/Windows/Fonts/arial.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
    "/Library/Fonts/Arial.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

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

    /// Draw a flushed vertex slice to an offscreen sRGB target and save it as PNG.
    /// Uses a pixel-space orthographic projection with (0,0) at the top-left.
    pub fn save_png(
        &self,
        renderer: &TextRenderer,
        atlas: &TextAtlas,
        vertices: &[sanscale::TextVertex],
        clear: wgpu::Color,
        path: &str,
    ) {
        let (width, height) = (self.config.width, self.config.height);
        let vertex_buffer = TextRenderer::build_vertices(&self.device, vertices);
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d {
                width,
                height,
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
        let matrix = Mat4::orthographic_rh(0.0, width as f32, height as f32, 0.0, -1.0, 1.0);

        let mut encoder = self.device.create_command_encoder(&Default::default());
        renderer.render(
            &self.queue,
            &mut encoder,
            &view,
            atlas,
            &vertex_buffer,
            vertices.len() as u32,
            matrix,
            (width, height),
            Some(clear),
        );
        self.queue.submit([encoder.finish()]);

        let pixels = self.read_back(&target);
        image::save_buffer(path, &pixels, width, height, image::ColorType::Rgba8)
            .unwrap_or_else(|e| panic!("write {path}: {e}"));
    }

    /// Copy an RGBA8 texture back to CPU, undoing wgpu's 256-byte row alignment.
    fn read_back(&self, texture: &wgpu::Texture) -> Vec<u8> {
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
        pixels
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

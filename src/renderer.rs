//! wgpu pipeline + atlas upload + draw.

use bytemuck::Pod;
use glam::Mat4;
use std::ops::Range;
use wgpu::util::DeviceExt;

use crate::cache::GlyphCache;
use crate::emoji::EmojiCache;
use crate::vertex::{EmojiVertex, TextVertex};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    matrix: [[f32; 4]; 4],
}

/// One uploaded glyph atlas (curve + band textures) bound as group 1.
pub struct TextAtlas {
    #[allow(dead_code)]
    curve_tex: wgpu::Texture,
    #[allow(dead_code)]
    band_tex: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    curve_width: u32,
    curve_capacity_height: u32,
    band_width: u32,
    band_capacity_height: u32,
    uploaded_curve_len: usize,
    uploaded_band_len: usize,
    synced_revision: u64,
}

impl TextAtlas {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        cache: &GlyphCache,
    ) -> Self {
        let (curve_width, curve_height) = cache.curve_size();
        let (band_width, band_height) = cache.band_size();
        let curve_capacity_height = grow_texture_height(curve_height);
        let band_capacity_height = grow_texture_height(band_height);
        let (curve_tex, band_tex, bind_group) = create_atlas_resources(
            device,
            layout,
            curve_width,
            curve_capacity_height,
            band_width,
            band_capacity_height,
        );
        let mut atlas = Self {
            curve_tex,
            band_tex,
            bind_group,
            curve_width,
            curve_capacity_height,
            band_width,
            band_capacity_height,
            uploaded_curve_len: 0,
            uploaded_band_len: 0,
            synced_revision: 0,
        };
        atlas.upload_full(queue, cache);
        atlas.synced_revision = cache.revision();
        atlas
    }

    pub(crate) fn sync(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        cache: &GlyphCache,
    ) {
        if self.synced_revision == cache.revision() {
            return;
        }

        let (curve_width, needed_curve_height) = cache.curve_size();
        let (band_width, needed_band_height) = cache.band_size();
        let needs_recreate = curve_width != self.curve_width
            || band_width != self.band_width
            || needed_curve_height > self.curve_capacity_height
            || needed_band_height > self.band_capacity_height;

        if needs_recreate {
            self.curve_width = curve_width;
            self.band_width = band_width;
            self.curve_capacity_height = grow_texture_height(needed_curve_height);
            self.band_capacity_height = grow_texture_height(needed_band_height);
            // The band/curve atlas is still unbounded (no eviction); surface the point
            // where wgpu would start silently dropping glyphs instead of failing quietly.
            warn_if_over_device_limit(device, self.curve_capacity_height, "text curve");
            warn_if_over_device_limit(device, self.band_capacity_height, "text band");
            let (curve_tex, band_tex, bind_group) = create_atlas_resources(
                device,
                layout,
                self.curve_width,
                self.curve_capacity_height,
                self.band_width,
                self.band_capacity_height,
            );
            self.curve_tex = curve_tex;
            self.band_tex = band_tex;
            self.bind_group = bind_group;
            self.uploaded_curve_len = 0;
            self.uploaded_band_len = 0;
            self.upload_full(queue, cache);
        } else {
            write_texture_range(
                queue,
                &self.curve_tex,
                self.curve_width,
                self.uploaded_curve_len,
                cache.curve_data(),
            );
            write_texture_range(
                queue,
                &self.band_tex,
                self.band_width,
                self.uploaded_band_len,
                cache.band_data(),
            );
            self.uploaded_curve_len = cache.curve_data().len();
            self.uploaded_band_len = cache.band_data().len();
        }

        self.synced_revision = cache.revision();
    }

    fn upload_full(&mut self, queue: &wgpu::Queue, cache: &GlyphCache) {
        write_texture_range(
            queue,
            &self.curve_tex,
            self.curve_width,
            0,
            cache.curve_data(),
        );
        write_texture_range(queue, &self.band_tex, self.band_width, 0, cache.band_data());
        self.uploaded_curve_len = cache.curve_data().len();
        self.uploaded_band_len = cache.band_data().len();
    }
}

fn create_atlas_resources(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    curve_width: u32,
    curve_height: u32,
    band_width: u32,
    band_height: u32,
) -> (wgpu::Texture, wgpu::Texture, wgpu::BindGroup) {
    let curve_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("text curve texture"),
        size: wgpu::Extent3d {
            width: curve_width,
            height: curve_height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let band_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("text band texture"),
        size: wgpu::Extent3d {
            width: band_width,
            height: band_height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rg16Uint,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(
                    &curve_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(
                    &band_tex.create_view(&Default::default()),
                ),
            },
        ],
        label: Some("text atlas bind group"),
    });
    (curve_tex, band_tex, bind_group)
}

fn grow_texture_height(required: u32) -> u32 {
    required.max(1).next_power_of_two()
}

fn write_texture_range<T: Pod>(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    start_texel: usize,
    data: &[T],
) {
    let texel_size = std::mem::size_of::<T>() as u32;
    let mut cursor = start_texel;
    while cursor < data.len() {
        let x = (cursor % width as usize) as u32;
        let y = (cursor / width as usize) as u32;
        let run = (width as usize - x as usize).min(data.len() - cursor);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&data[cursor..cursor + run]),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(run as u32 * texel_size),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: run as u32,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        cursor += run;
    }
}

/// Renders prepared vertex buffers against an atlas.
pub struct TextRenderer {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    pub atlas_layout: wgpu::BindGroupLayout,
}

impl TextRenderer {
    pub fn new(device: &wgpu::Device, config: &wgpu::SurfaceConfiguration) -> Self {
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
            label: Some("text uniform layout"),
        });
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
            label: Some("text atlas layout"),
        });

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("text uniform buffer"),
            contents: bytemuck::bytes_of(&Params {
                matrix: Mat4::IDENTITY.to_cols_array_2d(),
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
            label: Some("text uniform bind group"),
        });

        let vert = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("text vertex shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/vertex.wgsl").into()),
        });
        let frag = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("text fragment shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/pixel.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("text pipeline layout"),
            bind_group_layouts: &[Some(&uniform_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });

        let attrs = [
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 8,
                shader_location: 1,
                format: wgpu::VertexFormat::Uint32x2,
            },
            wgpu::VertexAttribute {
                offset: 16,
                shader_location: 2,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 24,
                shader_location: 3,
                format: wgpu::VertexFormat::Float32x4,
            },
            wgpu::VertexAttribute {
                offset: 40,
                shader_location: 4,
                format: wgpu::VertexFormat::Float32x4,
            },
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("text pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &vert,
                entry_point: Some("main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<TextVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attrs,
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &frag,
                entry_point: Some("main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            atlas_layout,
        }
    }

    /// Build a vertex buffer from a vertex slice.
    pub fn build_vertices(device: &wgpu::Device, vertices: &[TextVertex]) -> wgpu::Buffer {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("text vertex buffer"),
            contents: bytemuck::cast_slice(vertices),
            usage: wgpu::BufferUsages::VERTEX,
        })
    }

    pub fn write_matrix(&self, queue: &wgpu::Queue, matrix: Mat4) {
        let params = Params {
            matrix: matrix.to_cols_array_2d(),
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&params));
    }

    pub fn draw_vertices<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        atlas: &'a TextAtlas,
        vertex_buffer: &'a wgpu::Buffer,
        vertices: Range<u32>,
    ) {
        if vertices.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.uniform_bind_group, &[]);
        pass.set_bind_group(1, &atlas.bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.draw(vertices, 0..1);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        atlas: &TextAtlas,
        vertex_buffer: &wgpu::Buffer,
        vertex_count: u32,
        matrix: Mat4,
        _viewport: (u32, u32),
        clear: Option<wgpu::Color>,
    ) {
        self.write_matrix(queue, matrix);

        let load_op = match clear {
            Some(c) => wgpu::LoadOp::Clear(c),
            None => wgpu::LoadOp::Load,
        };
        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("text render pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: load_op,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if vertex_count > 0 {
            self.draw_vertices(&mut rpass, atlas, vertex_buffer, 0..vertex_count);
        }
    }
}

// ---------------------------------------------------------------------------
// Emoji atlas + textured-quad pipeline (color glyphs). Same discipline as
// TextAtlas/TextRenderer: one RGBA texture, grow-on-demand (power-of-two height),
// incremental upload of newly-packed rows when the cache revision advances.
// ---------------------------------------------------------------------------

fn data_height(cache: &EmojiCache, width: u32) -> u32 {
    if width == 0 {
        0
    } else {
        (cache.pixels().len() / (width as usize * 4)) as u32
    }
}

fn create_emoji_resources(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::Sampler, wgpu::BindGroup) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("emoji atlas texture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("emoji atlas sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(
                    &texture.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
        label: Some("emoji atlas bind group"),
    });
    (texture, sampler, bind_group)
}

fn upload_emoji_rows(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    from_row: u32,
    to_row: u32,
    pixels: &[u8],
) {
    if to_row <= from_row {
        return;
    }
    let row_bytes = (width * 4) as usize;
    let start = from_row as usize * row_bytes;
    let end = (to_row as usize * row_bytes).min(pixels.len());
    if end <= start {
        return;
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: 0,
                y: from_row,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        &pixels[start..end],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(to_row - from_row),
        },
        wgpu::Extent3d {
            width,
            height: to_row - from_row,
            depth_or_array_layers: 1,
        },
    );
}

/// GPU emoji atlas (premultiplied RGBA).
pub struct EmojiAtlas {
    texture: wgpu::Texture,
    #[allow(dead_code)]
    sampler: wgpu::Sampler,
    bind_group: wgpu::BindGroup,
    width: u32,
    capacity_height: u32,
    synced_revision: u64,
}

impl EmojiAtlas {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        cache: &EmojiCache,
    ) -> Self {
        let width = cache.size().0;
        let real_height = data_height(cache, width);
        let capacity_height = grow_texture_height(real_height.max(1));
        let (texture, sampler, bind_group) =
            create_emoji_resources(device, layout, width, capacity_height);
        let atlas = Self {
            texture,
            sampler,
            bind_group,
            width,
            capacity_height,
            synced_revision: cache.revision(),
        };
        upload_emoji_rows(queue, &atlas.texture, width, 0, real_height, cache.pixels());
        cache.take_dirty(); // consumed by the full upload above
        atlas
    }

    pub(crate) fn sync(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        cache: &EmojiCache,
    ) {
        if self.synced_revision == cache.revision() {
            return;
        }
        let width = cache.size().0;
        let real_height = data_height(cache, width);
        if width != self.width || real_height > self.capacity_height {
            // Growth phase: reallocate the texture and re-upload everything. Height is
            // capped by the cache, so `capacity_height` cannot exceed the device limit.
            self.width = width;
            self.capacity_height = grow_texture_height(real_height.max(1));
            warn_if_over_device_limit(device, self.capacity_height, "emoji");
            let (texture, sampler, bind_group) =
                create_emoji_resources(device, layout, self.width, self.capacity_height);
            self.texture = texture;
            self.sampler = sampler;
            self.bind_group = bind_group;
            upload_emoji_rows(queue, &self.texture, width, 0, real_height, cache.pixels());
            cache.take_dirty();
        } else if let Some((from_row, to_row)) = cache.take_dirty() {
            // Steady state: re-upload only the rows that changed — this covers cells
            // recycled by eviction, which an append-only upload would miss.
            upload_emoji_rows(queue, &self.texture, width, from_row, to_row, cache.pixels());
        }
        self.synced_revision = cache.revision();
    }
}

/// Warn once if an atlas texture would exceed the device's max 2D texture size, past
/// which wgpu silently drops the overflow. The emoji atlas is capped below this, but
/// the (still-unbounded) text band/curve atlas can reach it in long sessions.
fn warn_if_over_device_limit(device: &wgpu::Device, height: u32, which: &str) {
    if height > device.limits().max_texture_dimension_2d {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            log::warn!(
                "{which} atlas height {height} exceeds max_texture_dimension_2d {}; \
                 glyphs may render missing",
                device.limits().max_texture_dimension_2d
            );
        });
    }
}

/// Textured-quad pipeline for the emoji atlas; drawn over the Slug text pass.
pub struct EmojiRenderer {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    pub atlas_layout: wgpu::BindGroupLayout,
}

impl EmojiRenderer {
    pub fn new(device: &wgpu::Device, config: &wgpu::SurfaceConfiguration) -> Self {
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
            label: Some("emoji uniform layout"),
        });
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
            label: Some("emoji atlas layout"),
        });

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("emoji uniform buffer"),
            contents: bytemuck::bytes_of(&Params {
                matrix: Mat4::IDENTITY.to_cols_array_2d(),
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
            label: Some("emoji uniform bind group"),
        });

        let vert = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("emoji vertex shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/emoji_vertex.wgsl").into()),
        });
        let frag = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("emoji fragment shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/emoji_pixel.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("emoji pipeline layout"),
            bind_group_layouts: &[Some(&uniform_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });

        let attrs = [
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 8,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x2,
            },
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("emoji pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &vert,
                entry_point: Some("main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<EmojiVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attrs,
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &frag,
                entry_point: Some("main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            atlas_layout,
        }
    }

    pub fn build_vertices(device: &wgpu::Device, vertices: &[EmojiVertex]) -> wgpu::Buffer {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("emoji vertex buffer"),
            contents: bytemuck::cast_slice(vertices),
            usage: wgpu::BufferUsages::VERTEX,
        })
    }

    pub fn write_matrix(&self, queue: &wgpu::Queue, matrix: Mat4) {
        let params = Params {
            matrix: matrix.to_cols_array_2d(),
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&params));
    }

    /// Draw emoji quads into an existing render pass (call after the text pass).
    pub fn draw<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        atlas: &'a EmojiAtlas,
        vertex_buffer: &'a wgpu::Buffer,
        vertices: Range<u32>,
    ) {
        if vertices.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.uniform_bind_group, &[]);
        pass.set_bind_group(1, &atlas.bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.draw(vertices, 0..1);
    }
}

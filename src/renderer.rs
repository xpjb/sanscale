//! wgpu pipeline + atlas upload + draw.

use bytemuck::Pod;
use std::ops::Range;
use wgpu::util::DeviceExt;

use crate::cache::GlyphCache;
use crate::vertex::{EmojiVertex, TextVertex};

/// Each changed matrix gets immutable storage. Recorded passes retain the old
/// bind group through wgpu, even when several transforms precede one submit.
pub(crate) struct Uniforms {
    pub layout: wgpu::BindGroupLayout,
    pub binding: wgpu::BindGroup,
    bits: [u32; 16],
}

impl Uniforms {
    pub fn new(device: &wgpu::Device, matrix: [f32; 16]) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("text transform layout"),
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
        });
        let binding = Self::binding(device, &layout, &matrix);
        Self { layout, binding, bits: matrix.map(f32::to_bits) }
    }

    pub fn set(&mut self, device: &wgpu::Device, matrix: [f32; 16]) {
        if self.bits != matrix.map(f32::to_bits) {
            self.binding = Self::binding(device, &self.layout, &matrix);
            self.bits = matrix.map(f32::to_bits);
        }
    }

    fn binding(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, matrix: &[f32; 16]) -> wgpu::BindGroup {
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("immutable text transform"),
            contents: bytemuck::cast_slice(matrix),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        crate::work::count!(uniform_upload_bytes, std::mem::size_of_val(matrix));
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("text transform"), layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
        })
    }
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
    synced_revision: Option<u64>,
}

impl TextAtlas {
    /// A minimal atlas with no glyphs yet. The service creates this when it first
    /// learns a target format — before any text exists — and the first `sync`
    /// sizes it to whatever the glyph cache actually holds.
    pub(crate) fn empty(device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> Self {
        let (curve_tex, band_tex, bind_group) = create_atlas_resources(device, layout, 1, 1, 1, 1);
        Self {
            curve_tex,
            band_tex,
            bind_group,
            curve_width: 1,
            curve_capacity_height: 1,
            band_width: 1,
            band_capacity_height: 1,
            uploaded_curve_len: 0,
            uploaded_band_len: 0,
            synced_revision: None,
        }
    }

    /// Retain texture allocations, but forget the uploaded prefix. A replacement
    /// CPU cache may reuse both revision numbers and texel addresses.
    pub(crate) fn invalidate_contents(&mut self) {
        self.synced_revision = None;
        self.uploaded_curve_len = 0;
        self.uploaded_band_len = 0;
    }

    pub(crate) fn sync(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        cache: &GlyphCache,
    ) {
        if self.synced_revision == Some(cache.revision()) {
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

        self.synced_revision = Some(cache.revision());
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
    crate::work::count!(text_atlas_allocations, 1);
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
        crate::work::count!(text_atlas_upload_bytes, run * texel_size as usize);
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
}

impl TextRenderer {
    pub(crate) fn atlas_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        })
    }

    pub(crate) fn for_format(
        device: &wgpu::Device, format: wgpu::TextureFormat,
        uniform_layout: &wgpu::BindGroupLayout, atlas_layout: &wgpu::BindGroupLayout,
    ) -> Self {
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
            bind_group_layouts: &[Some(uniform_layout), Some(atlas_layout)],
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
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<TextVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attrs,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &frag,
                entry_point: Some("main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
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

        Self { pipeline }
    }

    /// Record a text draw. Nothing here needs to outlive the call: wgpu
    /// ref-counts resources bound into a pass, so a vertex buffer built for this
    /// draw alone can be dropped immediately. (The old signature tied every
    /// argument to the pass lifetime — a holdover from when wgpu borrowed them.)
    pub(crate) fn draw_vertices(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        uniforms: &wgpu::BindGroup,
        atlas: &TextAtlas,
        vertex_buffer: &wgpu::Buffer,
        range: Range<u64>,
        vertices: Range<u32>,
    ) {
        if vertices.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, uniforms, &[]);
        pass.set_bind_group(1, &atlas.bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(range.clone()));
        crate::work::count!(text_draw_calls, 1);
        pass.draw(vertices, 0..1);
    }
}

/// Append-only, 16-cell emoji page. Allocator metadata lives in EmojiCache;
/// batches hold these GPU resources independently of cache residency. No cell
/// is ever overwritten, so no submission/frame knowledge is needed.
pub(crate) struct EmojiPage {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pub side: u32,
}

impl EmojiPage {
    pub fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, side: u32) -> Self {
        crate::work::count!(emoji_atlas_allocations, 1);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("emoji page"),
            size: wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("emoji sampler"),
            mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("emoji page"), layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&texture.create_view(&Default::default())) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        Self { texture, bind_group, side }
    }

    pub fn upload(&self, queue: &wgpu::Queue, x: u32, y: u32, size: u32, rgba: &[u8]) {
        crate::work::count!(emoji_atlas_upload_bytes, rgba.len());
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture, mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All,
            }, rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size * 4), rows_per_image: Some(size) },
            wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        );
    }
}

/// Warn once if an atlas texture would exceed the device's max 2D texture size, past
/// which texture creation cannot succeed. Emoji pages are individually bounded;
/// the still-unbounded text band/curve atlas can reach this in long sessions.
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

/// Textured-quad pipeline for emoji pages, interleaved in caller order.
pub struct EmojiRenderer {
    pipeline: wgpu::RenderPipeline,
}

impl EmojiRenderer {
    pub(crate) fn atlas_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        })
    }

    pub(crate) fn for_format(
        device: &wgpu::Device, format: wgpu::TextureFormat,
        uniform_layout: &wgpu::BindGroupLayout, atlas_layout: &wgpu::BindGroupLayout,
    ) -> Self {
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
            bind_group_layouts: &[Some(uniform_layout), Some(atlas_layout)],
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
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<EmojiVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attrs,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &frag,
                entry_point: Some("main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
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

        Self { pipeline }
    }

    /// Record one ordered run of emoji quads from one immutable page.
    pub(crate) fn draw(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        uniforms: &wgpu::BindGroup,
        atlas: &EmojiPage,
        vertex_buffer: &wgpu::Buffer,
        range: Range<u64>,
        vertices: Range<u32>,
    ) {
        if vertices.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, uniforms, &[]);
        pass.set_bind_group(1, &atlas.bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(range.clone()));
        crate::work::count!(emoji_draw_calls, 1);
        pass.draw(vertices, 0..1);
    }
}

use super::{Times, ns};
use sanscale::{Batch, TextService};
use serde_json::{Value, json};
use std::time::Instant;

pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 768;
struct Timestamps {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
}
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub format: wgpu::TextureFormat,
    view: wgpu::TextureView,
    timestamps: Option<Timestamps>,
    metadata: Value,
}
impl Gpu {
    pub fn new() -> Self {
        pollster::block_on(async {
            let instance =
                wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                    apply_limit_buckets: false,
                })
                .await
                .expect("--gpu requested, but no adapter available");
            let info = adapter.get_info();
            let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
            let features = if timestamps {
                wgpu::Features::TIMESTAMP_QUERY
            } else {
                wgpu::Features::empty()
            };
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("sanscale pathological benchmark"),
                    required_features: features,
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::default(),
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                    trace: wgpu::Trace::Off,
                })
                .await
                .unwrap();
            let format = wgpu::TextureFormat::Rgba8UnormSrgb;
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("benchmark target"),
                size: wgpu::Extent3d {
                    width: WIDTH,
                    height: HEIGHT,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let timestamps = timestamps.then(|| Timestamps {
                queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("pass timestamps"),
                    ty: wgpu::QueryType::Timestamp,
                    count: 2,
                }),
                resolve: device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: 16,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                readback: device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: 16,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
            });
            let metadata = json!({"name": info.name, "backend":format!("{:?}",info.backend),
                "device_type":format!("{:?}",info.device_type),"driver":info.driver,"driver_info":info.driver_info,
                "vendor":info.vendor,"device":info.device,"timestamp_query":timestamps.is_some(),
                "width":WIDTH,"height":HEIGHT,"format":format!("{format:?}"),
                "max_texture_dimension_2d":device.limits().max_texture_dimension_2d});
            if info.device_type == wgpu::DeviceType::Cpu {
                eprintln!(
                    "WARNING: software GPU; do not compare these timings against hardware GPU results"
                );
            }
            Self {
                device,
                queue,
                format,
                view,
                timestamps,
                metadata,
            }
        })
    }
    pub fn metadata(&self) -> Value {
        self.metadata.clone()
    }
    pub fn attach(&self, text: &mut TextService) {
        text.set_target(&self.device, self.format);
        text.set_transform(TextService::pixel_ortho(WIDTH, HEIGHT));
        self.drain();
    }
    pub fn drain(&self) {
        self.queue.submit([]);
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
    }
    /// Serial, completed frames: no growing queue backlog. GPU time covers only
    /// the render pass, not queue writes/uploads. Fence wait is reported separately.
    pub fn render(&self, draw: impl FnOnce(&mut wgpu::RenderPass<'_>)) -> Times {
        let start = Instant::now();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("benchmark pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.01,
                            g: 0.01,
                            b: 0.01,
                            a: 1.,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                multiview_mask: None,
                timestamp_writes: self.timestamps.as_ref().map(|t| {
                    wgpu::RenderPassTimestampWrites {
                        query_set: &t.queries,
                        beginning_of_pass_write_index: Some(0),
                        end_of_pass_write_index: Some(1),
                    }
                }),
            });
            draw(&mut pass);
        }
        if let Some(t) = &self.timestamps {
            encoder.resolve_query_set(&t.queries, 0..2, &t.resolve, 0);
            encoder.copy_buffer_to_buffer(&t.resolve, 0, &t.readback, 0, 16);
        }
        let commands = encoder.finish();
        let encode_ns = ns(start);
        let start = Instant::now();
        self.queue.submit([commands]);
        let submit_ns = ns(start);
        let start = Instant::now();
        if let Some(t) = &self.timestamps {
            t.readback
                .slice(..)
                .map_async(wgpu::MapMode::Read, |r| r.unwrap());
        }
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        let wait_ns = ns(start);
        let gpu_pass_ns = self.timestamps.as_ref().map(|t| {
            let data = t.readback.slice(..).get_mapped_range().unwrap();
            let begin = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let end = u64::from_le_bytes(data[8..16].try_into().unwrap());
            let elapsed = (end.wrapping_sub(begin) as f64
                * self.queue.get_timestamp_period() as f64)
                .round() as u64;
            drop(data);
            t.readback.unmap();
            elapsed
        });
        Times {
            encode_ns: Some(encode_ns),
            submit_ns: Some(submit_ns),
            wait_ns: Some(wait_ns),
            gpu_pass_ns,
            ..Default::default()
        }
    }
    pub fn draw_batch(&self, text: &TextService, batch: &Batch) -> Times {
        self.render(|pass| {
            for (i, segment) in batch.segments().iter().enumerate() {
                // Mirror a real consumer: hardware scissor cuts straddling glyphs.
                if let Some(c) = segment.clip {
                    let x = c.x.floor().max(0.).min(WIDTH as f32) as u32;
                    let y = c.y.floor().max(0.).min(HEIGHT as f32) as u32;
                    let right = (c.x + c.width).ceil().max(x as f32).min(WIDTH as f32) as u32;
                    let bottom = (c.y + c.height).ceil().max(y as f32).min(HEIGHT as f32) as u32;
                    if right == x || bottom == y {
                        continue;
                    }
                    pass.set_scissor_rect(x, y, right - x, bottom - y);
                } else {
                    pass.set_scissor_rect(0, 0, WIDTH, HEIGHT);
                }
                text.draw_segment(pass, batch, i);
            }
        })
    }
}

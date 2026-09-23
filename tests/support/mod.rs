//! Real headless rendering and readback for API contract tests. No window.
use sanscale::{Batch, TextService};

pub const W: u32 = 256;
pub const H: u32 = 320;

pub struct Gpu { pub d: wgpu::Device, pub q: wgpu::Queue }
impl Gpu {
    pub fn new() -> Self {
        let (d, q) = pollster::block_on(async {
            let i = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let a = i.request_adapter(&Default::default()).await.unwrap();
            a.request_device(&Default::default()).await.unwrap()
        });
        Self { d, q }
    }
    pub fn attach(&self, t: &mut TextService, format: wgpu::TextureFormat) {
        t.set_target(&self.d, format);
        t.set_transform(TextService::pixel_ortho(W, H));
    }
    pub fn encode(&self, t: &TextService, batches: &[&Batch], format: wgpu::TextureFormat) -> (wgpu::CommandBuffer, wgpu::Buffer) {
        self.encode_with(format, |pass| {
            for batch in batches { t.draw_prepared(pass, batch); }
        })
    }
    pub fn encode_with(&self, format: wgpu::TextureFormat, draw: impl FnOnce(&mut wgpu::RenderPass<'_>)) -> (wgpu::CommandBuffer, wgpu::Buffer) {
        let extent = wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 };
        let target = self.d.create_texture(&wgpu::TextureDescriptor {
            label: None, size: extent, mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2, format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let out = self.d.create_buffer(&wgpu::BufferDescriptor {
            label: None, size: u64::from(W * H * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false,
        });
        let mut e = self.d.create_command_encoder(&Default::default());
        {
            let mut p = e.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, depth_slice: None, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })], ..Default::default()
            });
            draw(&mut p);
        }
        e.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &out, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(W * 4), rows_per_image: Some(H) } }, extent,
        );
        (e.finish(), out)
    }
    pub fn read(&self, out: &wgpu::Buffer) -> Vec<u8> {
        let (tx, rx) = std::sync::mpsc::channel();
        out.slice(..).map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        self.d.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        rx.recv().unwrap().unwrap();
        out.slice(..).get_mapped_range().unwrap().to_vec()
    }
    pub fn render(&self, t: &TextService, b: &[&Batch], f: wgpu::TextureFormat) -> Vec<u8> {
        let (c, out) = self.encode(t, b, f);
        self.q.submit([c]); self.read(&out)
    }
}

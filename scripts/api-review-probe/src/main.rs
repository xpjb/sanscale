//! Reproduce api-review.md against its pinned baseline, using only public APIs.
//! Assertions confirm the documented failures and their working controls. These
//! are NOT correctness assertions to copy unchanged into the library's test suite.
//! Headless; requires DejaVu Sans/Mono, Noto Color Emoji, and a wgpu adapter.

use sanscale::{Align, Batch, BlockKey, Color, Draw, FontHandle, ParagraphKey, Paragraphs, Style, TextService, Vec2, read_font_file};

const W: u32 = 256;
const H: u32 = 320;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const LATIN: &str = "/usr/share/fonts/TTF/DejaVuSans.ttf";
const MONO: &str = "/usr/share/fonts/TTF/DejaVuSansMono.ttf";
const EMOJI: &str = "/usr/share/fonts/noto/NotoColorEmoji.ttf";

fn font(t: &mut TextService, path: &str) -> FontHandle {
    let alternative = path.replace("/TTF/", "/truetype/dejavu/").replace("/fonts/noto/", "/fonts/truetype/noto/");
    let data = read_font_file(path).or_else(|_| read_font_file(&alternative))
        .expect("install DejaVu Sans/Mono and Noto Color Emoji, or adjust the font paths above");
    t.map_font(data, 0).unwrap()
}
fn style(t: &mut TextService, path: &str) -> Style {
    let f = font(t, path);
    Style { chain: t.register_chain(&[f]), wrap_em: None, align: Align::Left, line_spacing: 1.0 }
}
fn draw(t: &mut TextService, s: &Style, content: &str, size: f32) -> Draw {
    Draw { block: t.shape_transient(content, s).unwrap(), at: Vec2::new(8., 8.), size, color: Color([1.; 4]), ..Default::default() }
}
fn cpu() {
    let mut t = TextService::new();
    let s = style(&mut t, LATIN);
    let h = t.shape_transient("ab\ncd", &s).unwrap();
    let l = t.measure(h);
    let c = l.caret_at(2);
    let byte = l.caret_rect(2);
    let typed = l.caret_rect_on_line(Some(c.line_index), c.byte_index);
    println!("caret: byte=2 caret_at.line={} byte_rect.y={} placed_rect.y={}", c.line_index, byte.y_em, typed.y_em);
    assert!(byte.y_em != typed.y_em);
    let mut narrow = s; narrow.wrap_em = Some(0.7);
    let wrapped = t.shape_transient("ab\ncd", &narrow).unwrap();
    let before = t.measure(wrapped).caret_at(2);
    let after = t.measure(h).clamp_caret(before);
    let correct = t.measure(h).caret_at(2);
    println!("caret reflow: old line={} clamped line={} expected line={} byte={} returned line range={:?}", before.line_index, after.line_index, correct.line_index, after.byte_index, t.measure(h).line_range(after.line_index));
    assert!(after.line_index != correct.line_index);

    let mut t = TextService::new();
    let a = style(&mut t, LATIN);
    t.drop_chain(a.chain);
    let b = style(&mut t, MONO);
    let h = t.shape_transient("WWWiii", &b).unwrap();
    assert!(t.measure(h).width_em() > 0.0);
    t.drop_chain(a.chain);
    println!("chain reuse: dropping the old chain again destroys new chain = {}", t.shape_transient("WWWiii", &b).is_none());
    assert!(t.shape_transient("WWWiii", &b).is_none());

    let mut fresh = TextService::new();
    let mono = style(&mut fresh, MONO);
    let h = fresh.shape_transient("WWWiii", &mono).unwrap();
    let expected = fresh.measure(h).width_em();
    let mut t = TextService::new();
    let a = font(&mut t, LATIN);
    let b = font(&mut t, MONO);
    let start = std::time::Instant::now();
    for _ in 0..65_536 { t.register_chain(&[a]); }
    let s = Style { chain: t.register_chain(&[b]), ..mono };
    let h = t.shape_transient("WWWiii", &s).unwrap();
    let actual = t.measure(h).width_em();
    println!("chain capacity: 65,537th live chain width={} expected_mono={} elapsed={:?}", actual, expected, start.elapsed());
    assert!(actual != expected);
}

struct Gpu { d: wgpu::Device, q: wgpu::Queue }
impl Gpu {
    fn new() -> Self {
        let (d, q) = pollster::block_on(async {
            let i = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let a = i.request_adapter(&Default::default()).await.unwrap();
            println!("adapter: {:?}", a.get_info());
            a.request_device(&Default::default()).await.unwrap()
        });
        Self { d, q }
    }
    fn attach(&self, t: &mut TextService, format: wgpu::TextureFormat) {
        t.set_target(&self.d, format);
        t.set_transform(&self.q, TextService::pixel_ortho(W, H));
    }
    fn encode(&self, t: &TextService, batches: &[&Batch], format: wgpu::TextureFormat) -> (wgpu::CommandBuffer, wgpu::Buffer) {
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
            for b in batches { t.draw_prepared(&mut p, b); }
        }
        e.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &out, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(W * 4), rows_per_image: Some(H) } }, extent,
        );
        (e.finish(), out)
    }
    fn read(&self, out: &wgpu::Buffer) -> Vec<u8> {
        let (tx, rx) = std::sync::mpsc::channel();
        out.slice(..).map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        self.d.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        rx.recv().unwrap().unwrap();
        out.slice(..).get_mapped_range().unwrap().to_vec()
    }
    fn render(&self, t: &TextService, b: &[&Batch], f: wgpu::TextureFormat) -> Vec<u8> {
        let (c, out) = self.encode(t, b, f);
        self.q.submit([c]); self.read(&out)
    }
}
fn ink(pixels: &[u8]) -> usize { pixels.chunks_exact(4).filter(|p| p[..3] != [0, 0, 0]).count() }
fn diff(a: &[u8], b: &[u8]) -> usize { a.chunks_exact(4).zip(b.chunks_exact(4)).filter(|(a, b)| a != b).count() }

fn transforms(g: &Gpu) {
    let mut t = TextService::new(); let s = style(&mut t, LATIN); g.attach(&mut t, FORMAT);
    let item = draw(&mut t, &s, "Ag", 48.);
    let b = t.prepare(&g.d, &g.q, &[item]);
    let a = TextService::pixel_ortho(W, H);
    let mut shifted = a; shifted[12] += 0.75;
    t.set_transform(&g.q, a);
    let reference_a = g.render(&t, &[&b], FORMAT);
    t.set_transform(&g.q, shifted);
    let reference_b = g.render(&t, &[&b], FORMAT);
    assert!(ink(&reference_a) > 0 && ink(&reference_b) > 0 && reference_a != reference_b);
    t.set_transform(&g.q, a);
    let (ca, out_a) = g.encode(&t, &[&b], FORMAT);
    t.set_transform(&g.q, shifted);
    let (cb, out_b) = g.encode(&t, &[&b], FORMAT);
    g.q.submit([ca, cb]);
    let actual_a = g.read(&out_a); let actual_b = g.read(&out_b);
    println!("transforms: first pass differs from own control by {} pixels; equals second transform = {}; second pass correct = {}", diff(&actual_a, &reference_a), actual_a == reference_b, actual_b == reference_b);
    assert!(actual_a == reference_b && actual_b == reference_b);
}
fn formats(g: &Gpu) {
    let mut t = TextService::new(); let s = style(&mut t, LATIN); g.attach(&mut t, FORMAT);
    let item = draw(&mut t, &s, "Ag", 48.);
    let b = t.prepare(&g.d, &g.q, &[item]);
    assert!(ink(&g.render(&t, &[&b], FORMAT)) > 0);
    let format = wgpu::TextureFormat::Bgra8Unorm;
    // Set the transform again to isolate lost atlas contents from lost matrix state.
    g.attach(&mut t, format);
    let live = t.batch_live(&b);
    let actual = g.render(&t, &[&b], format);
    let rebuilt = t.prepare(&g.d, &g.q, &[item]);
    let expected = g.render(&t, &[&rebuilt], format);
    println!("target format: old batch_live={} old ink={} re-prepared ink={}", live, ink(&actual), ink(&expected));
    assert!(live && ink(&actual) == 0 && ink(&expected) > 0);
}
fn ordering(g: &Gpu) {
    let mut t = TextService::new(); let latin = style(&mut t, LATIN); let emoji = style(&mut t, EMOJI); g.attach(&mut t, FORMAT);
    let a = draw(&mut t, &emoji, "😀", 96.);
    let mut b = draw(&mut t, &latin, "MMMM", 96.); b.color = Color([1., 0., 0., 1.]);
    let merged = t.prepare(&g.d, &g.q, &[a, b]);
    let ba = t.prepare(&g.d, &g.q, &[a]); let bb = t.prepare(&g.d, &g.q, &[b]);
    let expected = g.render(&t, &[&ba, &bb], FORMAT);
    let reverse = g.render(&t, &[&bb, &ba], FORMAT);
    let actual = g.render(&t, &[&merged], FORMAT);
    println!("mixed-pipeline z-order: differing pixels={} matches reversed order={}", diff(&actual, &expected), actual == reverse);
    assert!(actual != expected && actual == reverse);
}
fn eviction(g: &Gpu) {
    let mut t = TextService::new(); let s = style(&mut t, LATIN); g.attach(&mut t, FORMAT);
    let old_item = draw(&mut t, &s, "keep", 48.);
    let b = t.prepare(&g.d, &g.q, &[old_item]);
    assert!(ink(&g.render(&t, &[&b], FORMAT)) > 0);
    let keys = [ParagraphKey { namespace: 4, slot: 0, generation: 0 }];
    for n in 0..131_072 { t.shape(BlockKey(n), &s, &keys, &Paragraphs(&["x"])).unwrap(); }
    assert!(!t.batch_live(&b));
    let reprepare = t.prepare(&g.d, &g.q, &[old_item]);
    let actual = g.render(&t, &[&reprepare], FORMAT);
    let fresh_item = draw(&mut t, &s, "keep", 48.);
    let reshaped = t.prepare(&g.d, &g.q, &[fresh_item]);
    let expected = g.render(&t, &[&reshaped], FORMAT);
    println!("eviction recovery: reprepare-only ink={} new batch_live={} reshape+prepare ink={}", ink(&actual), t.batch_live(&reprepare), ink(&expected));
    assert!(ink(&actual) == 0 && t.batch_live(&reprepare) && ink(&expected) > 0);
}
fn pixel_scale(g: &Gpu) {
    let mut t = TextService::new(); let s = style(&mut t, EMOJI); g.attach(&mut t, FORMAT);
    let item = draw(&mut t, &s, "😀", 32.);
    let b = t.prepare(&g.d, &g.q, &[item]);
    assert!(ink(&g.render(&t, &[&b], FORMAT)) > 0);
    let mut zoom = TextService::pixel_ortho(W, H); zoom[0] *= 4.; zoom[5] *= 4.;
    t.set_transform(&g.q, zoom); t.set_pixel_scale(4.);
    let live = t.batch_live(&b);
    let actual = g.render(&t, &[&b], FORMAT);
    let fresh = t.prepare(&g.d, &g.q, &[item]);
    let expected = g.render(&t, &[&fresh], FORMAT);
    println!("pixel scale: after 4x zoom old batch_live={} differing pixels from reprepare={} old ink={} fresh ink={}", live, diff(&actual, &expected), ink(&actual), ink(&expected));
    assert!(live && actual != expected && ink(&actual) > 0 && ink(&expected) > 0);
}
fn emoji_pressure(g: &Gpu) {
    let mut t = TextService::new(); let s = style(&mut t, EMOJI); g.attach(&mut t, FORMAT);
    sanscale::profiling::reset_work_counters();
    let mut count = 0;
    for cp in 0x1f300..0x1f900 {
        let c = char::from_u32(cp).unwrap();
        if !t.diagnostics().covers(s.chain, c) { continue; }
        let item = draw(&mut t, &s, &c.to_string(), 256.);
        let b = t.prepare(&g.d, &g.q, &[item]);
        // Each emoji is a separately submitted frame; previous batches are dropped.
        let actual = g.render(&t, &[&b], FORMAT);
        count += 1;
        if t.diagnostics().dropped_glyphs() > 0 {
            let work = sanscale::profiling::work_counters();
            let mut fresh = TextService::new(); let fresh_style = style(&mut fresh, EMOJI); g.attach(&mut fresh, FORMAT);
            let fresh_item = draw(&mut fresh, &fresh_style, &c.to_string(), 256.);
            let fresh_batch = fresh.prepare(&g.d, &g.q, &[fresh_item]);
            let expected = g.render(&fresh, &[&fresh_batch], FORMAT);
            println!("emoji pressure: separate frames={} next U+{:X} dropped={} evictions={} current ink={} fresh ink={}", count, cp, work.emoji_drops, work.emoji_evictions, ink(&actual), ink(&expected));
            assert!(work.emoji_evictions == 0 && ink(&actual) == 0 && ink(&expected) > 0);
            return;
        }
    }
    panic!("fixture did not fill the atlas");
}
fn main() {
    cpu();
    let g = Gpu::new();
    transforms(&g); formats(&g); ordering(&g); eviction(&g); pixel_scale(&g); emoji_pressure(&g);
}

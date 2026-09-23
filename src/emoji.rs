//! Color-emoji rasterization (COLR v0/v1 and PNG CBDT/sbix), cached on the GPU.
//!
//! Pages are append-only. LRU eviction removes cache entries, not texels: a
//! prepared batch owns references to its pages, and recorded draws retain their
//! bindings through wgpu. This supports multiple unsubmitted passes without a
//! frame hook, slot leases, or copying an entire atlas on eviction.

use std::collections::HashMap;
use std::sync::Arc;

use crate::renderer::EmojiPage;
use rustybuzz::Face as RustyFace;
use tiny_skia::{
    BlendMode, Color, FillRule, GradientStop, LinearGradient, Paint as SkPaint, Path, PathBuilder,
    Pixmap, Point, RadialGradient, Shader, SpreadMode, Transform as SkTransform,
};
use ttf_parser::colr::{ClipBox, CompositeMode, GradientExtend, Paint, Painter};
use ttf_parser::{GlyphId, OutlineBuilder, RasterImageFormat, RgbaColor, Transform};

const SIZE_BUCKETS: [u32; 4] = [32, 64, 128, 256];
const ATLAS_PAD: u32 = 2;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
// A bounded number of known-unrenderable glyphs; these have no resident page.
const MAX_FAILURES: usize = 1 << 16;
type GlyphKey = (u16, u32, u32);

pub(crate) fn bucket_for(px: f32) -> u32 {
    let px = px.ceil().max(1.0) as u32;
    SIZE_BUCKETS.into_iter().find(|&b| px <= b).unwrap_or(256)
}

fn bucket_index(bucket: u32) -> usize {
    SIZE_BUCKETS.iter().position(|&b| b == bucket).expect("raster bucket")
}

pub(crate) struct EmojiSlot {
    pub page: Arc<EmojiPage>,
    pub x: u32,
    pub y: u32,
    pub size: u32,
}

#[derive(Clone, Copy)]
struct Cell {
    page: usize,
    index: u32,
}

struct Page {
    gpu: Arc<EmojiPage>,
    bucket: u32,
    columns: u32,
    keys: Vec<GlyphKey>,
    last_used: u64,
}

impl Page {
    fn slot(&self, index: u32) -> EmojiSlot {
        EmojiSlot {
            page: self.gpu.clone(),
            x: (index % self.columns) * (self.bucket + ATLAS_PAD) + 1,
            y: (index / self.columns) * (self.bucket + ATLAS_PAD) + 1,
            size: self.bucket,
        }
    }
    fn bytes(&self) -> usize { (self.gpu.side as usize).pow(2) * 4 }
}

/// Cache-owned residency is byte-bounded. Batches may keep evicted pages alive,
/// just as they own their vertex buffers; this cannot prevent cache eviction.
/// Each page holds at most 16 glyphs, bounding sparse-retention amplification.
pub(crate) struct EmojiCache {
    slots: HashMap<GlyphKey, Option<Cell>>,
    pages: Vec<Option<Page>>,
    free: Vec<usize>,
    open: [Option<usize>; 4],
    resident_bytes: usize,
    budget: usize,
    clock: u64,
    failures: usize,
    dropped: u64,
}

impl Default for EmojiCache {
    fn default() -> Self {
        Self {
            slots: HashMap::new(), pages: Vec::new(), free: Vec::new(), open: [None; 4],
            resident_bytes: 0, budget: CACHE_BYTES, clock: 0, failures: 0, dropped: 0,
        }
    }
}

impl EmojiCache {
    pub fn clear(&mut self) {
        let budget = self.budget;
        *self = Self::default();
        self.budget = budget;
    }

    /// Largest cache-resident page; no pages means (0, 0).
    pub fn size(&self) -> (u32, u32) {
        let side = self.pages.iter().flatten().map(|p| p.gpu.side).max().unwrap_or(0);
        (side, side)
    }

    pub fn usage(&self) -> (usize, usize) {
        (self.pages.len() - self.free.len(), self.resident_bytes)
    }

    pub fn dropped_glyphs(&self) -> u64 { self.dropped }

    pub fn get_or_insert(
        &mut self, face: &RustyFace, face_id: u16, glyph_id: u32, bucket: u32,
        device: &wgpu::Device, queue: &wgpu::Queue, layout: &wgpu::BindGroupLayout,
    ) -> Option<EmojiSlot> {
        let key = (face_id, glyph_id, bucket);
        self.clock = self.clock.wrapping_add(1);
        if let Some(&cached) = self.slots.get(&key) {
            crate::work::count!(emoji_hits, 1);
            let cell = cached?;
            let page = self.pages[cell.page].as_mut().expect("resident cell");
            page.last_used = self.clock;
            return Some(page.slot(cell.index));
        }
        crate::work::count!(emoji_rasterizations, 1);
        let columns = (device.limits().max_texture_dimension_2d / (bucket + ATLAS_PAD)).min(4);
        let rgba = (columns > 0).then(|| rasterize(face, glyph_id as u16, bucket)).flatten();
        let Some(rgba) = rgba else {
            // Only permanent raster/device incompatibility is negatively cached.
            // Cache pressure never drops a glyph or records a failed placement.
            if self.failures == MAX_FAILURES {
                self.slots.retain(|_, slot| slot.is_some());
                self.failures = 0;
            }
            self.slots.insert(key, None);
            self.failures += 1;
            if self.dropped == 0 {
                log::warn!("color glyph rasterization failed; inspect Diagnostics::dropped_glyphs");
            }
            self.dropped += 1;
            crate::work::count!(emoji_drops, 1);
            return None;
        };
        let bi = bucket_index(bucket);
        let page_index = if let Some(index) = self.open[bi] {
            index
        } else {
            let side = columns * (bucket + ATLAS_PAD);
            let bytes = (side as usize).pow(2) * 4;
            // Normal pages are smaller than the budget. A deliberately tiny
            // internal test budget still permits one page, never drops ink.
            while self.resident_bytes + bytes > self.budget.max(bytes) {
                let victim = self.pages.iter().enumerate()
                    .filter_map(|(i, p)| p.as_ref().map(|p| (i, p.last_used)))
                    .min_by_key(|&(_, age)| age).expect("resident victim").0;
                self.evict(victim);
            }
            let index = self.free.pop().unwrap_or_else(|| {
                self.pages.push(None);
                self.pages.len() - 1
            });
            self.pages[index] = Some(Page {
                gpu: Arc::new(EmojiPage::new(device, layout, side)), bucket, columns,
                keys: Vec::new(), last_used: self.clock,
            });
            self.resident_bytes += bytes;
            self.open[bi] = Some(index);
            index
        };
        let page = self.pages[page_index].as_mut().expect("open page");
        page.last_used = self.clock;
        let cell = Cell { page: page_index, index: page.keys.len() as u32 };
        page.keys.push(key);
        if page.keys.len() == (page.columns * page.columns) as usize {
            self.open[bi] = None;
        }
        let slot = page.slot(cell.index);
        slot.page.upload(queue, slot.x, slot.y, bucket, &rgba);
        self.slots.insert(key, Some(cell));
        Some(slot)
    }

    fn evict(&mut self, index: usize) {
        let page = self.pages[index].take().expect("resident page");
        crate::work::count!(emoji_evictions, page.keys.len());
        self.resident_bytes -= page.bytes();
        for key in page.keys { self.slots.remove(&key); }
        let open = &mut self.open[bucket_index(page.bucket)];
        if *open == Some(index) { *open = None; }
        self.free.push(index);
    }
}

/// Rasterize a color glyph into a `size`×`size` premultiplied-RGBA buffer. Returns
/// `None` if the glyph isn't a color glyph or paints nothing. The emoji's em-box
/// `[0, upem]²` (baseline at the bottom) maps to the bitmap, so inline placement is
/// a `1em` square on the baseline.
fn rasterize(face: &RustyFace, glyph_id: u16, size: u32) -> Option<Vec<u8>> {
    let gid = GlyphId(glyph_id);
    if !face.is_color_glyph(gid) {
        // No COLR outline: fall back to an embedded bitmap strike (CBDT/sbix), which
        // is how Apple Color Emoji and Noto Color Emoji ship their color glyphs.
        return rasterize_bitmap(face, gid, size);
    }
    let upem = face.units_per_em() as f32;
    let n = size as f32;

    // Pass 1: union the layer outlines' bounding boxes so the glyph is framed and
    // centered by its actual extent — a fixed em-square clips and off-centers.
    let mut bb = BBoxPainter {
        face,
        min_x: f32::MAX,
        min_y: f32::MAX,
        max_x: f32::MIN,
        max_y: f32::MIN,
        any: false,
        cur: IDENTITY_TF,
        stack: Vec::new(),
    };
    face.paint_color_glyph(gid, 0, RgbaColor::new(0, 0, 0, 255), &mut bb)?;
    let (min_x, min_y, max_x, max_y) = if bb.any {
        (bb.min_x, bb.min_y, bb.max_x, bb.max_y)
    } else {
        (0.0, 0.0, upem, upem)
    };
    let pad = 0.06 * n;
    let w = (max_x - min_x).max(1.0);
    let h = (max_y - min_y).max(1.0);
    let s = ((n - 2.0 * pad) / w).min((n - 2.0 * pad) / h);
    let cx = (min_x + max_x) * 0.5;
    let cy = (min_y + max_y) * 0.5;
    // font units (y-up) -> pixmap (y-down), scaled to fit and centered.
    let base_tf = SkTransform::from_row(s, 0.0, 0.0, -s, n * 0.5 - s * cx, n * 0.5 + s * cy);

    let mut pm = Pixmap::new(size, size)?;
    let painted = {
        let mut painter = EmojiPainter {
            face,
            pm: &mut pm,
            stack: Vec::new(),
            cur: base_tf,
            clip_path: None,
            clip_tf: base_tf,
            blend: BlendMode::SourceOver,
            blend_stack: Vec::new(),
            painted: false,
        };
        let ok = face
            .paint_color_glyph(gid, 0, RgbaColor::new(0, 0, 0, 255), &mut painter)
            .is_some();
        ok && painter.painted
    };
    if !painted {
        return None;
    }
    Some(pm.data().to_vec())
}

/// Rasterize a bitmap color glyph (CBDT/sbix) into a `size`×`size` premultiplied-RGBA
/// buffer. PNG-backed strikes are decoded, aspect-fitted into the bucket square,
/// and premultiplied. `None` if the glyph has no supported strike or decoding fails.
/// Fixed-resolution strikes go soft
/// under deep zoom — acceptable for the "user sees the right thing" bar.
fn rasterize_bitmap(face: &RustyFace, gid: GlyphId, size: u32) -> Option<Vec<u8>> {
    let img = face.glyph_raster_image(gid, size as u16)?;
    if img.format != RasterImageFormat::PNG {
        return None;
    }
    let (sw, sh, src) = decode_png_rgba(img.data)?;
    if sw == 0 || sh == 0 {
        return None;
    }
    let n = size as usize;
    let mut out = vec![0u8; n * n * 4];

    // Preserve aspect and center in the cell (strikes are usually square already).
    let scale = (n as f32 / sw as f32).min(n as f32 / sh as f32);
    let dw = ((sw as f32 * scale).round() as usize).clamp(1, n);
    let dh = ((sh as f32 * scale).round() as usize).clamp(1, n);
    let ox = (n - dw) / 2;
    let oy = (n - dh) / 2;

    for dy in 0..dh {
        let fy = (dy as f32 + 0.5) / dh as f32 * sh as f32 - 0.5;
        for dx in 0..dw {
            let fx = (dx as f32 + 0.5) / dw as f32 * sw as f32 - 0.5;
            let [r, g, b, a] = bilerp_rgba(&src, sw, sh, fx, fy);
            // Straight alpha (PNG) -> premultiplied, to match the tiny-skia atlas.
            let af = a as f32 / 255.0;
            let px = ((oy + dy) * n + ox + dx) * 4;
            out[px] = (r as f32 * af).round() as u8;
            out[px + 1] = (g as f32 * af).round() as u8;
            out[px + 2] = (b as f32 * af).round() as u8;
            out[px + 3] = a;
        }
    }
    Some(out)
}

/// Decode a PNG into straight-alpha RGBA8 `(width, height, pixels)`. Handles the
/// 8-bit RGBA / RGB / grayscale(+alpha) strikes emoji fonts ship; other formats bail.
fn decode_png_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(data);
    // Noto Color Emoji uses indexed PNG strikes. Expand palettes and tRNS into
    // ordinary RGB/RGBA so the conversion below handles them like any other
    // embedded image.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let (w, h) = (info.width, info.height);
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => px
            .chunks_exact(2)
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((w, h, rgba))
}

/// Bilinear sample of straight-alpha RGBA8 at `(fx, fy)`, clamping to the edges.
fn bilerp_rgba(src: &[u8], w: u32, h: u32, fx: f32, fy: f32) -> [u8; 4] {
    let x0 = fx.floor().clamp(0.0, (w - 1) as f32) as u32;
    let y0 = fy.floor().clamp(0.0, (h - 1) as f32) as u32;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let tx = (fx - x0 as f32).clamp(0.0, 1.0);
    let ty = (fy - y0 as f32).clamp(0.0, 1.0);
    let at = |x: u32, y: u32, c: usize| src[((y * w + x) * 4) as usize + c] as f32;
    let mut out = [0u8; 4];
    for (c, o) in out.iter_mut().enumerate() {
        let top = at(x0, y0, c) * (1.0 - tx) + at(x1, y0, c) * tx;
        let bot = at(x0, y1, c) * (1.0 - tx) + at(x1, y1, c) * tx;
        *o = (top * (1.0 - ty) + bot * ty).round().clamp(0.0, 255.0) as u8;
    }
    out
}

fn glyph_path(face: &RustyFace, gid: GlyphId) -> Option<Path> {
    let mut sink = PathSink {
        pb: PathBuilder::new(),
    };
    face.outline_glyph(gid, &mut sink)?;
    sink.pb.finish()
}

struct PathSink {
    pb: PathBuilder,
}
impl OutlineBuilder for PathSink {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.pb.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.pb.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

fn sk_color(c: RgbaColor) -> Color {
    Color::from_rgba8(c.red, c.green, c.blue, c.alpha)
}
fn sk_tf(t: Transform) -> SkTransform {
    SkTransform::from_row(t.a, t.b, t.c, t.d, t.e, t.f)
}
fn spread(e: GradientExtend) -> SpreadMode {
    match e {
        GradientExtend::Pad => SpreadMode::Pad,
        GradientExtend::Repeat => SpreadMode::Repeat,
        GradientExtend::Reflect => SpreadMode::Reflect,
    }
}
fn blend_mode(m: CompositeMode) -> BlendMode {
    match m {
        CompositeMode::SourceOver => BlendMode::SourceOver,
        CompositeMode::Screen => BlendMode::Screen,
        CompositeMode::Overlay => BlendMode::Overlay,
        CompositeMode::Darken => BlendMode::Darken,
        CompositeMode::Lighten => BlendMode::Lighten,
        CompositeMode::ColorDodge => BlendMode::ColorDodge,
        CompositeMode::ColorBurn => BlendMode::ColorBurn,
        CompositeMode::HardLight => BlendMode::HardLight,
        CompositeMode::SoftLight => BlendMode::SoftLight,
        CompositeMode::Difference => BlendMode::Difference,
        CompositeMode::Exclusion => BlendMode::Exclusion,
        CompositeMode::Multiply => BlendMode::Multiply,
        CompositeMode::Hue => BlendMode::Hue,
        CompositeMode::Saturation => BlendMode::Saturation,
        CompositeMode::Color => BlendMode::Color,
        CompositeMode::Luminosity => BlendMode::Luminosity,
        _ => BlendMode::SourceOver,
    }
}

const IDENTITY_TF: Transform = Transform {
    a: 1.0,
    b: 0.0,
    c: 0.0,
    d: 1.0,
    e: 0.0,
    f: 0.0,
};

fn tf_apply(t: Transform, x: f32, y: f32) -> (f32, f32) {
    (t.a * x + t.c * y + t.e, t.b * x + t.d * y + t.f)
}

/// Compose so that `compose(c, t)` applies `t` then `c` (matches `push_transform`).
fn tf_compose(c: Transform, t: Transform) -> Transform {
    Transform {
        a: c.a * t.a + c.c * t.b,
        b: c.b * t.a + c.d * t.b,
        c: c.a * t.c + c.c * t.d,
        d: c.b * t.c + c.d * t.d,
        e: c.a * t.e + c.c * t.f + c.e,
        f: c.b * t.e + c.d * t.f + c.f,
    }
}

/// First pass: unions the layer outlines' bounding boxes — **under their active
/// transforms** — so the glyph is framed by its true painted extent. Ignoring
/// transforms (as before) under-frames glyphs whose layers are translated/scaled,
/// clipping e.g. the top of `U+1F606`.
struct BBoxPainter<'a> {
    face: &'a RustyFace<'a>,
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    any: bool,
    cur: Transform,
    stack: Vec<Transform>,
}

impl BBoxPainter<'_> {
    fn union_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        for (px, py) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
            let (tx, ty) = tf_apply(self.cur, px, py);
            self.min_x = self.min_x.min(tx);
            self.min_y = self.min_y.min(ty);
            self.max_x = self.max_x.max(tx);
            self.max_y = self.max_y.max(ty);
        }
        self.any = true;
    }
}

impl<'a> Painter<'a> for BBoxPainter<'a> {
    fn outline_glyph(&mut self, glyph_id: GlyphId) {
        if let Some(r) = self.face.glyph_bounding_box(glyph_id) {
            self.union_rect(
                r.x_min as f32,
                r.y_min as f32,
                r.x_max as f32,
                r.y_max as f32,
            );
        }
    }
    fn paint(&mut self, _: Paint<'a>) {}
    fn push_clip(&mut self) {}
    fn push_clip_box(&mut self, _: ClipBox) {}
    fn pop_clip(&mut self) {}
    fn push_layer(&mut self, _: CompositeMode) {}
    fn pop_layer(&mut self) {}
    fn push_transform(&mut self, t: Transform) {
        self.stack.push(self.cur);
        self.cur = tf_compose(self.cur, t);
    }
    fn pop_transform(&mut self) {
        if let Some(t) = self.stack.pop() {
            self.cur = t;
        }
    }
}

/// Walks the `COLR` paint tree into tiny-skia. Each `PaintGlyph` fills its outline
/// with the paint (solid/gradient) under the current transform + blend mode;
/// clips collapse to "fill the outline" (the common emoji shape).
struct EmojiPainter<'a, 'b> {
    face: &'a RustyFace<'a>,
    pm: &'b mut Pixmap,
    stack: Vec<SkTransform>,
    cur: SkTransform,
    clip_path: Option<Path>,
    clip_tf: SkTransform,
    blend: BlendMode,
    blend_stack: Vec<BlendMode>,
    painted: bool,
}

impl<'a, 'b> Painter<'a> for EmojiPainter<'a, 'b> {
    fn outline_glyph(&mut self, glyph_id: GlyphId) {
        self.clip_path = glyph_path(self.face, glyph_id);
        self.clip_tf = self.cur;
    }
    fn paint(&mut self, paint: Paint<'a>) {
        let Some(path) = self.clip_path.clone() else {
            return;
        };
        // tiny-skia composes the fill transform with the shader's transform, so
        // applying both would double the base transform. Bake the outline
        // transform into the path (→ device space) and fill with identity; the
        // gradient shader then carries `cur` and is applied exactly once.
        let Some(dev_path) = path.transform(self.clip_tf) else {
            return;
        };
        // Some COLR layers carry zero-area outlines (degenerate contours / hairlines).
        // They fill nothing, so skip them rather than hand tiny-skia an unfillable path
        // it would warn about once per glyph, forever.
        let b = dev_path.bounds();
        if b.width() == 0.0 || b.height() == 0.0 {
            return;
        }
        let shader = match paint {
            Paint::Solid(c) => Shader::SolidColor(sk_color(c)),
            Paint::LinearGradient(g) => {
                let stops = g
                    .stops(0, &[])
                    .map(|s| GradientStop::new(s.stop_offset, sk_color(s.color)))
                    .collect::<Vec<_>>();
                LinearGradient::new(
                    Point::from_xy(g.x0, g.y0),
                    Point::from_xy(g.x1, g.y1),
                    stops,
                    spread(g.extend),
                    self.cur,
                )
                .unwrap_or(Shader::SolidColor(Color::TRANSPARENT))
            }
            Paint::RadialGradient(g) => {
                let stops = g
                    .stops(0, &[])
                    .map(|s| GradientStop::new(s.stop_offset, sk_color(s.color)))
                    .collect::<Vec<_>>();
                RadialGradient::new(
                    Point::from_xy(g.x0, g.y0),
                    Point::from_xy(g.x1, g.y1),
                    g.r1.max(1.0),
                    stops,
                    spread(g.extend),
                    self.cur,
                )
                .unwrap_or(Shader::SolidColor(Color::TRANSPARENT))
            }
            _ => return,
        };
        let mut sk = SkPaint {
            shader,
            blend_mode: self.blend,
            ..Default::default()
        };
        sk.anti_alias = true;
        self.pm.fill_path(
            &dev_path,
            &sk,
            FillRule::Winding,
            SkTransform::identity(),
            None,
        );
        self.painted = true;
    }
    fn push_clip(&mut self) {}
    fn push_clip_box(&mut self, _: ClipBox) {}
    fn pop_clip(&mut self) {}
    fn push_layer(&mut self, mode: CompositeMode) {
        self.blend_stack.push(self.blend);
        self.blend = blend_mode(mode);
    }
    fn pop_layer(&mut self) {
        if let Some(b) = self.blend_stack.pop() {
            self.blend = b;
        }
    }
    fn push_transform(&mut self, t: Transform) {
        self.stack.push(self.cur);
        self.cur = self.cur.pre_concat(sk_tf(t));
    }
    fn pop_transform(&mut self) {
        if let Some(t) = self.stack.pop() {
            self.cur = t;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_png_strikes_expand_to_rgba() {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 1, 1);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(vec![10, 20, 30]);
            encoder.set_trns(vec![128]);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0]).unwrap();
        }

        let (width, height, rgba) = decode_png_rgba(&encoded).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(rgba, [10, 20, 30, 128]);
    }

}

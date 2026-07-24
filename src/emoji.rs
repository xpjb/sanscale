//! Color-emoji (`COLR` v0/v1) support.
//!
//! Color glyphs can't go through the monochrome Slug path, so they're rasterized
//! once (per size bucket) into an RGBA atlas and drawn as textured quads. The
//! rasterizer walks the ttf-parser paint tree — one path for v0 and v1 — into
//! tiny-skia: solid + linear/radial gradients, per-layer transforms, composite
//! modes.
//!
//! Caching mirrors [`GlyphCache`](crate::cache): the CPU side is **append-only**
//! with a `revision` counter; the GPU side ([`EmojiAtlas`](crate::renderer)) uploads
//! incrementally when the revision moves. Bounded by a small fixed set of size
//! buckets, so zoom re-rasters an emoji at most a few times.

use std::collections::HashMap;

use rustybuzz::Face as RustyFace;
use tiny_skia::{
    BlendMode, Color, FillRule, GradientStop, LinearGradient, Paint as SkPaint, Path, PathBuilder,
    Pixmap, Point, RadialGradient, Shader, SpreadMode, Transform as SkTransform,
};
use ttf_parser::colr::{ClipBox, CompositeMode, GradientExtend, Paint, Painter};
use ttf_parser::{GlyphId, OutlineBuilder, RgbaColor, Transform};

/// Atlas is a fixed width; height grows in power-of-two steps (like `TextAtlas`).
pub const EMOJI_ATLAS_WIDTH: u32 = 2048;

/// Raster resolutions. On-screen pixel size snaps up to the nearest bucket, so an
/// emoji is rasterized at most `SIZE_BUCKETS.len()` times across all zooms.
const SIZE_BUCKETS: [u32; 4] = [32, 64, 128, 256];
const ATLAS_PAD: u32 = 2;

/// Bucket (raster resolution) for an on-screen pixel size.
pub(crate) fn bucket_for(px: f32) -> u32 {
    let px = px.ceil().max(1.0) as u32;
    SIZE_BUCKETS
        .into_iter()
        .find(|&b| px <= b)
        .unwrap_or(SIZE_BUCKETS[SIZE_BUCKETS.len() - 1])
}

/// A rasterized emoji's texel rect in the atlas (square, `size`×`size`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct EmojiSlot {
    pub x: u32,
    pub y: u32,
    pub size: u32,
}

/// CPU-side emoji atlas: premultiplied-RGBA pixels + shelf packer + slot map.
/// Append-only; keyed by `(face_id, glyph_id, bucket)`. A cached `None` records a
/// glyph that isn't a renderable color glyph, so we don't retry it every frame.
pub(crate) struct EmojiCache {
    pixels: Vec<u8>,
    height: u32,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    slots: HashMap<(u16, u32, u32), Option<EmojiSlot>>,
    revision: u64,
}

impl EmojiCache {
    pub fn new() -> Self {
        Self {
            pixels: Vec::new(),
            height: 0,
            shelf_x: 0,
            shelf_y: 0,
            shelf_h: 0,
            slots: HashMap::new(),
            revision: 0,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Atlas `(width, height)` in texels (height ≥ 1 for a valid texture).
    pub fn size(&self) -> (u32, u32) {
        (EMOJI_ATLAS_WIDTH, self.height.max(1))
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Slot for a color glyph at a size bucket, rasterizing + packing on first use.
    /// `None` when the glyph isn't a renderable color glyph.
    pub fn get_or_insert(
        &mut self,
        face: &RustyFace,
        face_id: u16,
        glyph_id: u32,
        bucket: u32,
    ) -> Option<EmojiSlot> {
        let key = (face_id, glyph_id, bucket);
        if let Some(slot) = self.slots.get(&key) {
            return *slot;
        }
        let slot = rasterize(face, glyph_id as u16, bucket).and_then(|rgba| self.pack(bucket, &rgba));
        self.slots.insert(key, slot);
        self.revision = self.revision.wrapping_add(1);
        slot
    }

    /// Shelf-pack a `size`×`size` RGBA image; grows atlas height as needed.
    fn pack(&mut self, size: u32, rgba: &[u8]) -> Option<EmojiSlot> {
        let cell = size + ATLAS_PAD;
        if cell > EMOJI_ATLAS_WIDTH {
            return None;
        }
        if self.shelf_x + cell > EMOJI_ATLAS_WIDTH {
            self.shelf_y += self.shelf_h;
            self.shelf_x = 0;
            self.shelf_h = 0;
        }
        let (x, y) = (self.shelf_x, self.shelf_y);
        self.shelf_x += cell;
        self.shelf_h = self.shelf_h.max(cell);

        let needed = y + cell;
        if needed > self.height {
            self.height = needed;
            self.pixels
                .resize((EMOJI_ATLAS_WIDTH * self.height * 4) as usize, 0);
        }

        let row_bytes = (size * 4) as usize;
        for row in 0..size {
            let src = (row * size * 4) as usize;
            let dst = (((y + row) * EMOJI_ATLAS_WIDTH + x) * 4) as usize;
            self.pixels[dst..dst + row_bytes].copy_from_slice(&rgba[src..src + row_bytes]);
        }
        Some(EmojiSlot { x, y, size })
    }
}

impl Default for EmojiCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Rasterize a color glyph into a `size`×`size` premultiplied-RGBA buffer. Returns
/// `None` if the glyph isn't a color glyph or paints nothing. The emoji's em-box
/// `[0, upem]²` (baseline at the bottom) maps to the bitmap, so inline placement is
/// a `1em` square on the baseline.
fn rasterize(face: &RustyFace, glyph_id: u16, size: u32) -> Option<Vec<u8>> {
    let gid = GlyphId(glyph_id);
    if !face.is_color_glyph(gid) {
        return None;
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

/// First pass: unions the bounding boxes of a color glyph's layer outlines to
/// frame it (transforms ignored — layers are near-identity in practice).
struct BBoxPainter<'a> {
    face: &'a RustyFace<'a>,
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    any: bool,
}

impl<'a> Painter<'a> for BBoxPainter<'a> {
    fn outline_glyph(&mut self, glyph_id: GlyphId) {
        if let Some(r) = self.face.glyph_bounding_box(glyph_id) {
            self.min_x = self.min_x.min(r.x_min as f32);
            self.min_y = self.min_y.min(r.y_min as f32);
            self.max_x = self.max_x.max(r.x_max as f32);
            self.max_y = self.max_y.max(r.y_max as f32);
            self.any = true;
        }
    }
    fn paint(&mut self, _: Paint<'a>) {}
    fn push_clip(&mut self) {}
    fn push_clip_box(&mut self, _: ClipBox) {}
    fn pop_clip(&mut self) {}
    fn push_layer(&mut self, _: CompositeMode) {}
    fn pop_layer(&mut self) {}
    fn push_transform(&mut self, _: Transform) {}
    fn pop_transform(&mut self) {}
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
        self.pm
            .fill_path(&dev_path, &sk, FillRule::Winding, SkTransform::identity(), None);
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

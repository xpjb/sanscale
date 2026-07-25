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

use std::cell::Cell;
use std::collections::HashMap;

use rustybuzz::Face as RustyFace;
use tiny_skia::{
    BlendMode, Color, FillRule, GradientStop, LinearGradient, Paint as SkPaint, Path, PathBuilder,
    Pixmap, Point, RadialGradient, Shader, SpreadMode, Transform as SkTransform,
};
use ttf_parser::colr::{ClipBox, CompositeMode, GradientExtend, Paint, Painter};
use ttf_parser::{GlyphId, OutlineBuilder, RasterImageFormat, RgbaColor, Transform};

/// Atlas is a fixed width; height grows in power-of-two steps up to a cap, after
/// which cells are recycled by eviction (see [`EmojiCache`]) so the texture never
/// exceeds the GPU's `max_texture_dimension_2d`.
pub const EMOJI_ATLAS_WIDTH: u32 = 2048;

/// Raster resolutions. On-screen pixel size snaps up to the nearest bucket, so an
/// emoji is rasterized at most `SIZE_BUCKETS.len()` times across all zooms.
const SIZE_BUCKETS: [u32; 4] = [32, 64, 128, 256];
const ATLAS_PAD: u32 = 2;

/// Default cap on atlas height, in force until a device is known. Kept ≤ the wgpu
/// default `max_texture_dimension_2d` (8192) so the atlas can never silently
/// overflow even on a default-limits device; once `Text` builds its GPU resources
/// it replaces this with the device's real `max_texture_dimension_2d` via
/// [`EmojiCache::set_max_height`].
pub(crate) const DEFAULT_EMOJI_ATLAS_MAX_HEIGHT: u32 = 4096;

/// Bucket (raster resolution) for an on-screen pixel size.
pub(crate) fn bucket_for(px: f32) -> u32 {
    let px = px.ceil().max(1.0) as u32;
    SIZE_BUCKETS
        .into_iter()
        .find(|&b| px <= b)
        .unwrap_or(SIZE_BUCKETS[SIZE_BUCKETS.len() - 1])
}

/// Index of a bucket size in [`SIZE_BUCKETS`] (bucket sizes are the only valid keys).
fn bucket_index(bucket: u32) -> usize {
    SIZE_BUCKETS.iter().position(|&b| b == bucket).unwrap_or(0)
}

/// A rasterized emoji's texel rect in the atlas (square, `size`×`size`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct EmojiSlot {
    pub x: u32,
    pub y: u32,
    pub size: u32,
}

/// Which key currently owns a cell, and when it was last drawn. `size` (== bucket)
/// lets eviction scan only cells of the target bucket.
struct Occupant {
    key: (u16, u32, u32),
    size: u32,
    last_used: u64,
}

/// CPU-side emoji atlas: premultiplied-RGBA pixels + a **per-bucket slab allocator**
/// with an LRU free list, keyed `(face_id, glyph_id, bucket)`.
///
/// Because the four bucket sizes are fixed, each shelf holds uniform cells for one
/// bucket, so an evicted cell is reused in place with no repacking. Height is capped
/// at [`max_height`](Self::set_max_height); once every shelf is spoken for, the
/// least-recently-used cell of the requested bucket (that wasn't drawn this frame) is
/// evicted and recycled. A cached `None` records a glyph that isn't renderable, so we
/// don't retry it every frame.
///
/// **Eviction invalidates baked UVs.** [`epoch`](Self::epoch) bumps whenever a cell is
/// recycled; holders of cached vertices (e.g. the examples' per-row cache) must drop
/// them when it changes. The engine's per-frame re-emit path re-fetches slots each
/// frame, so it is unaffected.
pub(crate) struct EmojiCache {
    pixels: Vec<u8>,
    height: u32,
    max_height: u32,
    /// Next free row for a brand-new shelf (shelves are handed out top-to-bottom).
    next_y: u32,
    /// Unused cell origins per bucket, replenished a shelf at a time.
    free: [Vec<(u32, u32)>; SIZE_BUCKETS.len()],
    slots: HashMap<(u16, u32, u32), Option<EmojiSlot>>,
    /// Reverse map: occupied cell origin → occupant, for LRU eviction.
    cells: HashMap<(u32, u32), Occupant>,
    frame: u64,
    epoch: u64,
    revision: u64,
    /// Row range whose pixels changed since the last GPU sync (min..max, exclusive).
    dirty: Cell<Option<(u32, u32)>>,
    dropped: u64,
    warned: bool,
}

impl EmojiCache {
    pub fn new() -> Self {
        Self {
            pixels: Vec::new(),
            height: 0,
            max_height: DEFAULT_EMOJI_ATLAS_MAX_HEIGHT,
            next_y: 0,
            free: Default::default(),
            slots: HashMap::new(),
            cells: HashMap::new(),
            frame: 0,
            epoch: 0,
            revision: 0,
            dirty: Cell::new(None),
            dropped: 0,
            warned: false,
        }
    }

    /// Cap the atlas height in texels (clamped to at least one 256px shelf). Callers
    /// with a known device limit set this to `min(limit, budget)` for more headroom.
    pub fn set_max_height(&mut self, max_height: u32) {
        self.max_height = max_height.max(SIZE_BUCKETS[SIZE_BUCKETS.len() - 1] + ATLAS_PAD);
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Bumped whenever a cell is recycled under eviction; see the type docs.
    #[allow(dead_code)] // kept for a future geometry-pool invalidation hook
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Total glyphs dropped because a bucket was full of glyphs already needed this
    /// frame (the working set exceeded the atlas budget). Should stay 0 in practice.
    pub fn dropped_glyphs(&self) -> u64 {
        self.dropped
    }

    /// Advance the frame clock. Cells drawn in the current frame are never evicted, so
    /// this must be called once per rendered frame (the engine does so in `flush`).
    #[allow(dead_code)] // per-frame budget hook, unused until the service tracks frames
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Atlas `(width, height)` in texels (height ≥ 1 for a valid texture).
    pub fn size(&self) -> (u32, u32) {
        (EMOJI_ATLAS_WIDTH, self.height.max(1))
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Take (and clear) the dirty row range accumulated since the last call, so the
    /// GPU side re-uploads exactly the texels that changed — including recycled cells,
    /// which an append-only upload would miss.
    pub fn take_dirty(&self) -> Option<(u32, u32)> {
        self.dirty.take()
    }

    /// Slot for a color glyph at a size bucket, rasterizing + packing on first use.
    /// `None` when the glyph isn't renderable, or the atlas is momentarily full of
    /// glyphs already needed this frame.
    pub fn get_or_insert(
        &mut self,
        face: &RustyFace,
        face_id: u16,
        glyph_id: u32,
        bucket: u32,
    ) -> Option<EmojiSlot> {
        let key = (face_id, glyph_id, bucket);
        if let Some(&cached) = self.slots.get(&key) {
            if let Some(slot) = cached {
                if let Some(occ) = self.cells.get_mut(&(slot.x, slot.y)) {
                    occ.last_used = self.frame;
                }
            }
            return cached;
        }
        let slot = match rasterize(face, glyph_id as u16, bucket) {
            Some(rgba) => self.place(key, bucket, &rgba),
            None => None,
        };
        self.slots.insert(key, slot);
        self.revision = self.revision.wrapping_add(1);
        slot
    }

    /// Allocate a cell for `bucket`, write the `size`×`size` image into it, and record
    /// the occupant. `None` only if the bucket is wider than the atlas or fully in use
    /// this frame.
    fn place(&mut self, key: (u16, u32, u32), bucket: u32, rgba: &[u8]) -> Option<EmojiSlot> {
        let (x, y) = self.alloc_cell(bucket)?;
        self.write_cell(x, y, bucket, rgba);
        self.cells.insert(
            (x, y),
            Occupant {
                key,
                size: bucket,
                last_used: self.frame,
            },
        );
        Some(EmojiSlot { x, y, size: bucket })
    }

    /// A free cell for `bucket`: reuse one, else open a new shelf, else evict LRU.
    fn alloc_cell(&mut self, bucket: u32) -> Option<(u32, u32)> {
        let cell = bucket + ATLAS_PAD;
        if cell > EMOJI_ATLAS_WIDTH {
            return None;
        }
        let bi = bucket_index(bucket);
        if let Some(pos) = self.free[bi].pop() {
            return Some(pos);
        }
        // Open a fresh shelf if there's vertical room, filling its free list.
        if self.next_y + cell <= self.max_height {
            let y = self.next_y;
            let cols = EMOJI_ATLAS_WIDTH / cell;
            for i in 0..cols {
                self.free[bi].push((i * cell, y));
            }
            self.next_y += cell;
            if self.next_y > self.height {
                self.height = self.next_y;
                self.pixels
                    .resize((EMOJI_ATLAS_WIDTH * self.height * 4) as usize, 0);
            }
            return self.free[bi].pop();
        }
        // Atlas full: recycle the least-recently-used cell of this bucket that isn't
        // part of the current frame's working set.
        if let Some(pos) = self.evict_lru(bucket) {
            return Some(pos);
        }
        // Everything in this bucket is needed this frame — a genuine over-budget frame.
        self.dropped = self.dropped.wrapping_add(1);
        if !self.warned {
            self.warned = true;
            log::warn!(
                "emoji atlas full at {bucket}px (cap {} rows): working set exceeds budget, \
                 dropping glyphs; raise the atlas max height for more headroom",
                self.max_height
            );
        }
        None
    }

    /// Evict the LRU occupied cell of `bucket` not drawn this frame; returns its origin
    /// (now free for reuse). Bumps [`epoch`](Self::epoch) so baked-UV holders refresh.
    // A linear scan over occupied cells; eviction only runs once the atlas is full, and
    // cell count is bounded by the cap, so this stays cheap. Swap for a per-bucket LRU
    // list if profiling ever shows it hot.
    fn evict_lru(&mut self, bucket: u32) -> Option<(u32, u32)> {
        let frame = self.frame;
        let victim = self
            .cells
            .iter()
            .filter(|(_, occ)| occ.size == bucket && occ.last_used != frame)
            .min_by_key(|(_, occ)| occ.last_used)
            .map(|(&pos, _)| pos)?;
        let occ = self.cells.remove(&victim).unwrap();
        self.slots.remove(&occ.key);
        self.epoch = self.epoch.wrapping_add(1);
        Some(victim)
    }

    /// Copy a `size`×`size` premultiplied-RGBA image to cell origin `(x, y)` and mark
    /// those rows dirty for the next GPU upload.
    fn write_cell(&mut self, x: u32, y: u32, size: u32, rgba: &[u8]) {
        let row_bytes = (size * 4) as usize;
        for row in 0..size {
            let src = (row * size * 4) as usize;
            let dst = (((y + row) * EMOJI_ATLAS_WIDTH + x) * 4) as usize;
            self.pixels[dst..dst + row_bytes].copy_from_slice(&rgba[src..src + row_bytes]);
        }
        let (lo, hi) = self.dirty.get().unwrap_or((y, y + size));
        self.dirty.set(Some((lo.min(y), hi.max(y + size))));
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
/// buffer. The embedded strike is always PNG per the OpenType spec; we decode it,
/// aspect-fit it into the bucket square (centered), and premultiply. `None` if the
/// glyph has no strike or the PNG can't be decoded. Fixed-resolution strikes go soft
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
    let mut reader = png::Decoder::new(data).read_info().ok()?;
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
            self.union_rect(r.x_min as f32, r.y_min as f32, r.x_max as f32, r.y_max as f32);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Place-or-touch a synthetic glyph without a font, mirroring `get_or_insert`'s
    /// bookkeeping (the hit path bumps recency; the miss path allocates a cell).
    fn put(cache: &mut EmojiCache, glyph: u32, bucket: u32) -> Option<EmojiSlot> {
        let key = (0u16, glyph, bucket);
        if let Some(&cached) = cache.slots.get(&key) {
            if let Some(slot) = cached {
                if let Some(occ) = cache.cells.get_mut(&(slot.x, slot.y)) {
                    occ.last_used = cache.frame;
                }
            }
            return cached;
        }
        let rgba = vec![255u8; (bucket * bucket * 4) as usize];
        let slot = cache.place(key, bucket, &rgba);
        cache.slots.insert(key, slot);
        cache.revision = cache.revision.wrapping_add(1);
        slot
    }

    /// One shelf of the 256px bucket (`2048 / 258 = 7` cells) — the smallest atlas
    /// that still packs multiple cells, so eviction is easy to force.
    fn one_shelf() -> EmojiCache {
        let mut c = EmojiCache::new();
        c.set_max_height(256 + ATLAS_PAD); // caps at a single 256px shelf
        c
    }

    /// Filling past capacity across frames recycles cells: height stays bounded, the
    /// epoch bumps once per eviction, and nothing is dropped.
    #[test]
    fn eviction_recycles_cells_and_stays_bounded() {
        let mut c = one_shelf();
        for g in 0..7 {
            assert!(put(&mut c, g, 256).is_some());
        }
        assert_eq!(c.size().1, 256 + ATLAS_PAD, "one shelf allocated");
        assert_eq!(c.epoch(), 0, "no eviction while filling the first shelf");

        c.begin_frame();
        let reused = put(&mut c, 100, 256).expect("8th glyph recycles a cell");
        assert_eq!(c.size().1, 256 + ATLAS_PAD, "atlas did not grow past its cap");
        assert_eq!(c.epoch(), 1, "exactly one cell recycled");
        assert_eq!(c.dropped_glyphs(), 0);
        // The recycled cell's rows are dirty so the GPU re-uploads them.
        let d = c.take_dirty().expect("recycled cell marked dirty");
        assert!(d.0 <= reused.y && reused.y + reused.size <= d.1);
    }

    /// Eviction picks the least-recently-used cell of the bucket, not an arbitrary one.
    #[test]
    fn eviction_is_least_recently_used() {
        let mut c = one_shelf();
        for g in 0..7 {
            c.begin_frame();
            put(&mut c, g, 256); // glyph g's recency strictly increases with g
        }
        c.begin_frame();
        put(&mut c, 3, 256); // touch glyph 3 -> now most-recently-used

        c.begin_frame();
        put(&mut c, 50, 256).expect("insert recycles a cell");
        assert!(!c.slots.contains_key(&(0, 0, 256)), "LRU glyph 0 evicted");
        assert!(c.slots.contains_key(&(0, 3, 256)), "touched glyph 3 survives");
        assert!(c.slots.contains_key(&(0, 50, 256)), "new glyph is present");
    }

    /// When a single frame's working set exceeds a bucket's capacity, the overflow is
    /// dropped (and counted) rather than silently corrupting an in-use cell.
    #[test]
    fn over_budget_frame_drops_and_counts() {
        let mut c = one_shelf();
        for g in 0..7 {
            assert!(put(&mut c, g, 256).is_some());
        }
        // No begin_frame: all seven cells belong to the current frame, so none can be
        // evicted. The eighth glyph is dropped.
        assert!(put(&mut c, 99, 256).is_none());
        assert_eq!(c.dropped_glyphs(), 1);
    }

    /// Distinct buckets allocate independently and never collide.
    #[test]
    fn buckets_are_independent() {
        let mut c = EmojiCache::new();
        let a = put(&mut c, 1, 32).unwrap();
        let b = put(&mut c, 1, 64).unwrap();
        assert_eq!(a.size, 32);
        assert_eq!(b.size, 64);
        assert_ne!((a.x, a.y), (b.x, b.y));
    }
}

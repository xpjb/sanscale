# Sanscale public API inventory

Generated from compiler-resolved rustdoc JSON, not a regex. Version **0.1.0**;
**all features** enabled. `profiling` is available only with `perf-counters`.

**33 public types/traits/aliases · 3 free functions · 55 inherent methods ·
4 trait method declarations · 1 associated constant.**
Explicit and derived trait implementations appear in the final section.

Scope: every reachable declaration defined by Sanscale, including macro-generated
items and re-exports from private modules. Public fields and enum payloads are
expanded; private fields are marked, not exposed. Function bodies are omitted.
Upstream blanket implementations and signatures of inherited standard-library
trait defaults are not duplicated; inherited defaults are named in the impl listing.
Inferred auto traits are listed separately; compiler-internal markers such as
`StructuralPartialEq`/`Freeze` are informative, not stable APIs to invoke.
This is a declaration inventory for review, not compilable replacement source.
`Cow` means `std::borrow::Cow`; `Range` means `std::ops::Range`.

Regenerate from the repository root (nightly rustdoc):

```sh
cargo rustdoc --lib --all-features --locked -- -Z unstable-options --output-format json
python3 scripts/export-public-api.py target/doc/sanscale.json public-api.md
```

Rustdoc JSON format: `61`. Declaration fingerprint (SHA-256):
`ad59c1bac935543bbd03f1b16ff941f6556075adac08af8348f4ee8b8c9fd7e3`. Do not edit generated declarations by hand.

## `sanscale::Align`

[src/text.rs:119](src/text.rs#L119)

Horizontal alignment of each line within the wrap width.

```rust
pub enum Align {
    Left,
    Center,
    Right,
}
```

## `sanscale::Batch`

[src/text.rs:1039](src/text.rs#L1039)

Blocks the consumer chose to group, concatenated into one GPU buffer **the
consumer holds** — the unit of vertex ownership (see the "`Batch` owns its
vertices" lock in `decisions.md`).

```rust
pub struct Batch {
    /* private fields */
}

impl Batch {
    pub fn segments(&self) -> &[Segment];
}
```

## `sanscale::BlockKey`

[src/text.rs:210](src/text.rs#L210)

The consumer's identity for a composed block: the unit of *coordinate space*.

```rust
pub struct BlockKey(pub u64);
```

## `sanscale::Caret`

[src/text.rs:264](src/text.rs#L264)

A placed caret: a byte offset **and** the visual line it is shown on.

```rust
pub struct Caret {
    pub byte_index: usize,
    pub line_index: usize,
}
```

## `sanscale::CaretRect`

[src/text.rs:310](src/text.rs#L310)

```rust
pub struct CaretRect {
    pub x_em: f32,
    pub y_em: f32,
    pub height_em: f32,
}
```

## `sanscale::CaretStop`

[src/text.rs:335](src/text.rs#L335)

One caret position on a line: a byte offset and where it sits, in em.

```rust
pub struct CaretStop {
    pub byte_index: usize,
    pub x_em: f32,
}
```

## `sanscale::Color`

[src/text.rs:109](src/text.rs#L109)

Linear RGBA. A draw parameter only — never baked into shaping or the atlases,
so a recolor reshapes and rasterizes nothing. It *is* baked into cached
geometry (per-vertex color is what lets differently-colored blocks share one
draw call), so a recolor re-emits that block's quads: a CPU walk, not a
reshape.

```rust
pub struct Color(pub [f32; 4]);
```

## `sanscale::Diagnostics`

[src/text.rs:2089](src/text.rs#L2089)

Read-only introspection: font coverage and cache occupancy.

```rust
pub struct Diagnostics<'a> {
    /* private fields */
}

impl Diagnostics<'_> {
    pub fn chain_families(&self, chain: FontChainHandle) -> Vec<String>;
    pub fn uncovered_chars(&self, chain: FontChainHandle, text: &str) -> Vec<char>;
    pub fn covers(&self, chain: FontChainHandle, c: char) -> bool;
    pub fn family_for(&self, chain: FontChainHandle, c: char) -> Option<String>;
    pub fn glyph_bbox(&self, chain: FontChainHandle, c: char) -> Option<(f32, f32, f32, f32)>;
    pub fn is_single_glyph(&self, chain: FontChainHandle, text: &str) -> bool;
    pub fn atlas_sizes(&self) -> ((u32, u32), (u32, u32), (u32, u32));
    pub fn dropped_glyphs(&self) -> u64;
    pub fn emoji_cache_usage(&self) -> (usize, usize);
    pub fn cache_occupancy(&self) -> (usize, usize);
}
```

## `sanscale::Draw`

[src/text.rs:981](src/text.rs#L981)

One block to draw, for `TextService::draw_batch`.

```rust
pub struct Draw {
    pub block: ShapedHandle,
    pub at: Vec2,
    pub size: f32,
    pub color: Color,
    pub clip: Option<Rect>,
    pub paint: Option<PaintHandle>,
}
```

## `sanscale::FontChainHandle`

[src/text.rs:156](src/text.rs#L156)

An ordered fallback chain of fonts, local to its service.
Released handles cannot select or release a later occupant of the slot.

```rust
pub struct FontChainHandle {
    /* private fields */
}
```

Variable font instances use `map_font_with_variations` with OpenType axis tags
and design coordinates (for example `(*b"wght", 700.0)`). Shaping, metrics and
outlines share those coordinates. Font identity includes the normalized axis
values; mapping a bold instance does not change a regular instance. Platform
font discovery remains the caller's job. No fonts are bundled by the library.

## `sanscale::FontData`

[src/text.rs:34](src/text.rs#L34)

Shared font bytes. Deliberately fontdb's `make_shared_face_data` return type,
so bytes a consumer already discovered pass straight through — no copy, no
re-wrap. Also takes `Arc::new(vec)` or `Arc::new(include_bytes!(..))`.

```rust
pub type FontData = std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>;
```

## `sanscale::FontError`

[src/text.rs:126](src/text.rs#L126)

```rust
pub enum FontError {
    Parse,
    PoolFull,
}
```

## `sanscale::FontHandle`

[src/text.rs:151](src/text.rs#L151)

One mapped concrete font, local to its service. Deduped by data identity.
Invalid after `clear`; remap rather than reusing an old font handle.

```rust
pub struct FontHandle(/* private field */);
```

## `sanscale::FontSpan`

[src/spans.rs:14](src/spans.rs#L14)

A paragraph-local UTF-8 byte range choosing an ordered font fallback chain.

```rust
pub struct FontSpan {
    pub range: std::ops::Range<usize>,
    pub chain: FontChainHandle,
}
```

## `sanscale::Layout`

[src/text.rs:370](src/text.rs#L370)

Laid-out geometry for one block, in em space, with block-global byte offsets
across all of its paragraphs.

```rust
pub struct Layout {
    /* private fields */
}

impl Layout {
    pub fn from_lines(lines: Vec<LayoutLineSpec>) -> Self;
    pub fn width_em(&self) -> f32;
    pub fn height_em(&self) -> f32;
    pub fn size_em(&self) -> Vec2;
    pub fn line_count(&self) -> usize;
    pub fn line(&self, index: usize) -> Option<LineMetrics>;
    pub fn line_range(&self, index: usize) -> Option<Range<usize>>;
    pub fn len_bytes(&self) -> usize;
    pub fn hit_test(&self, at_em: Vec2) -> Option<Caret>;
    pub fn caret_byte_on_line(&self, line_index: usize, x_em: f32) -> Option<usize>;
    pub fn caret_rect(&self, caret: Caret) -> CaretRect;
    pub fn next_caret_stop(&self, byte_index: usize) -> Option<usize>;
    pub fn prev_caret_stop(&self, byte_index: usize) -> Option<usize>;
    pub fn clamp_caret(&self, caret: Caret) -> Caret;
    pub fn caret_at(&self, byte_index: usize) -> Caret;
    pub fn caret_after_edit(&self, byte_index: usize) -> Caret;
    pub fn caret_move(
        &self,
        caret: Caret,
        motion: Motion,
        goal: &mut Option<f32>,
        text: &impl WordBoundaries + ?Sized,
    ) -> Caret;
    pub fn select_word_at(
        &self,
        byte_index: usize,
        text: &impl WordBoundaries + ?Sized,
    ) -> Range<usize>;
    pub fn select_paragraph_at(&self, byte_index: usize) -> Range<usize>;
    pub fn selection(&self, range: Range<usize>) -> Vec<SelectionSpan>;
}
```

## `sanscale::LayoutLineSpec`

[src/text.rs:342](src/text.rs#L342)

One line's worth of synthetic layout, for `Layout::from_lines`.

```rust
pub struct LayoutLineSpec {
    pub byte_range: std::ops::Range<usize>,
    pub metrics: LineMetrics,
    pub carets: Vec<CaretStop>,
}
```

## `sanscale::LineMetrics`

[src/text.rs:326](src/text.rs#L326)

```rust
pub struct LineMetrics {
    pub top_em: f32,
    pub baseline_em: f32,
    pub height_em: f32,
    pub width_em: f32,
}
```

## `sanscale::Motion`

[src/text.rs:276](src/text.rs#L276)

One caret motion, resolved by `Layout::caret_move`.

```rust
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp(usize),
    PageDown(usize),
    DocStart,
    DocEnd,
    WordLeft,
    WordRight,
}
```

## `sanscale::PaintError`

[src/spans.rs:50](src/spans.rs#L50)

```rust
pub enum PaintError {
    InvalidRange {
        index: usize,
    },
    PoolFull,
}
```

## `sanscale::PaintHandle`

[src/spans.rs:35](src/spans.rs#L35)

An immutable paint snapshot in one `TextService`'s pool. Release explicitly
with `drop_paint`. Slot reuse cannot revive a stale handle. Like other service
handles, this is local to its originating service.

```rust
pub struct PaintHandle(/* private field */);
```

## `sanscale::PaintSpan`

[src/spans.rs:26](src/spans.rs#L26)

A block-local byte range overriding the draw's foreground color.

```rust
pub struct PaintSpan {
    pub range: std::ops::Range<usize>,
    pub color: Color,
}
```

## `sanscale::ParagraphKey`

[src/text.rs:197](src/text.rs#L197)

The consumer's identity for one paragraph: the unit of *invalidation*.
Its generation covers text **and effective font spans**, never paint.

```rust
pub struct ParagraphKey {
    pub namespace: u64,
    pub slot: u32,
    pub generation: u32,
}
```

## `sanscale::ParagraphSource`

[src/text.rs:862](src/text.rs#L862)

Supplies a paragraph's text, by identity, on a shaping cache miss.

```rust
pub trait ParagraphSource {
    fn paragraph_text(&self, index: usize, key: ParagraphKey) -> Option<Cow<'_, str>>;
    fn paragraph_fonts(&self, _index: usize, _key: ParagraphKey) -> Cow<'_, [FontSpan]>; // default implementation provided
}
```

## `sanscale::Paragraphs`

[src/text.rs:876](src/text.rs#L876)

A source over already-materialized paragraphs, for consumers holding strings.

```rust
pub struct Paragraphs<'a>(pub &'a [&'a str]);
```

## `sanscale::Rect`

[src/text.rs:66](src/text.rs#L66)

Min/size rectangle in the transform's source space.

```rust
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self;
}
```

## `sanscale::Segment`

[src/text.rs:1017](src/text.rs#L1017)

One clip-uniform run inside a `Batch`: the vertices between two scissor
changes. Read-only, minted by `TextService::prepare`, dies with its batch.

```rust
pub struct Segment {
    pub clip: Option<Rect>,
    /* private fields */
}
```

## `sanscale::SelectionSpan`

[src/text.rs:317](src/text.rs#L317)

```rust
pub struct SelectionSpan {
    pub line_index: usize,
    pub x_em: f32,
    pub y_em: f32,
    pub width_em: f32,
    pub height_em: f32,
}
```

## `sanscale::ShapedHandle`

[src/text.rs:173](src/text.rs#L173)

A shaped *block* — 1..N paragraphs flowed into one coordinate space.

```rust
pub struct ShapedHandle {
    /* private fields */
}

impl ShapedHandle {
    pub const INVALID: Self;
}
```

## `sanscale::Style`

[src/text.rs:216](src/text.rs#L216)

Base font and paragraph layout policy. Inline font spans are separate source
inputs covered by the paragraph generation. No pixels or color: moving the
camera re-runs nothing.

```rust
pub struct Style {
    pub chain: FontChainHandle,
    pub wrap_em: Option<f32>,
    pub align: Align,
    pub line_spacing: f32,
}
```

## `sanscale::TextService`

[src/text.rs:1121](src/text.rs#L1121)

One text service: every pool, every cache, and the GPU resources.

```rust
pub struct TextService {
    /* private fields */
}

impl TextService {
    pub fn new() -> Self;
    pub fn map_font(&mut self, data: FontData, face_index: u32) -> Result<FontHandle, FontError>;
    pub fn map_font_with_variations(&mut self, data: FontData, face_index: u32,
        variations: &[([u8; 4], f32)]) -> Result<FontHandle, FontError>;
    pub fn register_chain(&mut self, fonts: &[FontHandle]) -> Result<FontChainHandle, FontError>;
    pub fn drop_chain(&mut self, chain: FontChainHandle);
    pub fn register_paint(&mut self, spans: &[PaintSpan]) -> Result<PaintHandle, PaintError>;
    pub fn drop_paint(&mut self, paint: PaintHandle);
    pub fn clear(&mut self);
    pub fn shape(
        &mut self,
        block: BlockKey,
        style: &Style,
        parts: &[ParagraphKey],
        source: &dyn ParagraphSource,
    ) -> Option<ShapedHandle>;
    pub fn shape_transient(&mut self, text: &str, style: &Style) -> Option<ShapedHandle>;
    pub fn measure(&self, h: ShapedHandle) -> &Layout;
    pub fn diagnostics(&self) -> Diagnostics<'_>;
    pub fn set_target(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat);
    pub fn set_transform(&mut self, transform: [f32; 16]);
    pub fn set_pixel_scale(&mut self, px_per_unit: f32);
    pub fn pixel_ortho(width: u32, height: u32) -> [f32; 16];
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        item: Draw,
    );
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, items: &[Draw]) -> Batch;
    pub fn draw_segment(&self, pass: &mut wgpu::RenderPass<'_>, batch: &Batch, index: usize);
    pub fn draw_prepared(&self, pass: &mut wgpu::RenderPass<'_>, batch: &Batch);
    pub fn batch_live(&self, batch: &Batch) -> bool;
    pub fn draw_batch(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        items: &[Draw],
    );
}
```

## `sanscale::Vec2`

[src/text.rs:41](src/text.rs#L41)

```rust
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const fn new(x: f32, y: f32) -> Self;
}
```

## `sanscale::WordBoundaries`

[src/text.rs:295](src/text.rs#L295)

Word classification over the caller's text, for `Motion::WordLeft` /
`Motion::WordRight`. Words are semantics, not shaping, so the crate asks
rather than guesses — the same seam as `ParagraphSource`. Return `None`
to decline; the motion degrades to a cluster step. `()` always declines.

```rust
pub trait WordBoundaries {
    fn prev_word(&self, byte_index: usize) -> Option<usize>;
    fn next_word(&self, byte_index: usize) -> Option<usize>;
}
```

## `sanscale::profiling::WorkCounters` — feature `perf-counters`

[src/profiling.rs:30](src/profiling.rs#L30)

Cumulative counts since the last reset on this thread.

```rust
pub struct WorkCounters {
    pub block_requests: u64,
    pub block_hits: u64,
    pub paragraph_requests: u64,
    pub paragraph_hits: u64,
    pub source_reads: u64,
    pub source_bytes: u64,
    pub shape_calls: u64,
    pub shape_bytes: u64,
    pub font_spans: u64,
    pub paint_registrations: u64,
    pub paint_span_copies: u64,
    pub paint_releases: u64,
    pub paint_lookups: u64,
    pub paint_searches: u64,
    pub shape_runs: u64,
    pub fallback_shape_calls: u64,
    pub shaped_glyphs: u64,
    pub missing_glyphs: u64,
    pub coverage_queries: u64,
    pub glyph_hits: u64,
    pub glyph_misses: u64,
    pub glyph_inserts: u64,
    pub flow_calls: u64,
    pub flow_glyphs: u64,
    pub flow_tokens: u64,
    pub flow_glyph_tests: u64,
    pub flow_lines: u64,
    pub assemblies: u64,
    pub assembled_lines: u64,
    pub assembled_glyphs: u64,
    pub assembled_carets: u64,
    pub paragraph_evictions: u64,
    pub block_evictions: u64,
    pub prepares: u64,
    pub prepared_items: u64,
    pub geometry_hits: u64,
    pub geometry_builds: u64,
    pub visited_lines: u64,
    pub culled_lines: u64,
    pub visited_glyphs: u64,
    pub text_quads: u64,
    pub emoji_quads: u64,
    pub prepared_segments: u64,
    pub batch_buffers: u64,
    pub vertex_upload_bytes: u64,
    pub text_draw_calls: u64,
    pub emoji_draw_calls: u64,
    pub text_atlas_allocations: u64,
    pub text_atlas_upload_bytes: u64,
    pub emoji_atlas_allocations: u64,
    pub emoji_atlas_upload_bytes: u64,
    pub uniform_upload_bytes: u64,
    pub emoji_hits: u64,
    pub emoji_rasterizations: u64,
    pub emoji_evictions: u64,
    pub emoji_drops: u64,
}

impl profiling::WorkCounters {
    pub fn values(self) -> impl Iterator<Item = (&'static str, u64)>;
}
```

## `sanscale::profiling::reset_work_counters` — feature `perf-counters`

[src/profiling.rs:163](src/profiling.rs#L163)

Reset this thread only. Does not affect any cache or another thread's counters.

```rust
pub fn reset_work_counters();
```

## `sanscale::profiling::work_counters` — feature `perf-counters`

[src/profiling.rs:158](src/profiling.rs#L158)

Snapshot this thread's work. Does not reset or allocate.

```rust
pub fn work_counters() -> profiling::WorkCounters;
```

## `sanscale::read_font_file`

[src/font.rs:136](src/font.rs#L136)

Read a font file into shared bytes. Convenience for examples and tests; a real
consumer discovers fonts itself and hands over an `Arc` it already has.

```rust
pub fn read_font_file(path: impl AsRef<std::path::Path>) -> std::io::Result<FontData>;
```

## Trait implementations (including derives)

```rust
impl Clone for Align {
    fn clone(&self) -> Align;
    // Inherited defaults: clone_from
}

impl Clone for BlockKey {
    fn clone(&self) -> BlockKey;
    // Inherited defaults: clone_from
}

impl Clone for Caret {
    fn clone(&self) -> Caret;
    // Inherited defaults: clone_from
}

impl Clone for CaretRect {
    fn clone(&self) -> CaretRect;
    // Inherited defaults: clone_from
}

impl Clone for CaretStop {
    fn clone(&self) -> CaretStop;
    // Inherited defaults: clone_from
}

impl Clone for Color {
    fn clone(&self) -> Color;
    // Inherited defaults: clone_from
}

impl Clone for Draw {
    fn clone(&self) -> Draw;
    // Inherited defaults: clone_from
}

impl Clone for FontChainHandle {
    fn clone(&self) -> FontChainHandle;
    // Inherited defaults: clone_from
}

impl Clone for FontError {
    fn clone(&self) -> FontError;
    // Inherited defaults: clone_from
}

impl Clone for FontHandle {
    fn clone(&self) -> FontHandle;
    // Inherited defaults: clone_from
}

impl Clone for FontSpan {
    fn clone(&self) -> FontSpan;
    // Inherited defaults: clone_from
}

impl Clone for Layout {
    fn clone(&self) -> Layout;
    // Inherited defaults: clone_from
}

impl Clone for LayoutLineSpec {
    fn clone(&self) -> LayoutLineSpec;
    // Inherited defaults: clone_from
}

impl Clone for LineMetrics {
    fn clone(&self) -> LineMetrics;
    // Inherited defaults: clone_from
}

impl Clone for Motion {
    fn clone(&self) -> Motion;
    // Inherited defaults: clone_from
}

impl Clone for PaintError {
    fn clone(&self) -> PaintError;
    // Inherited defaults: clone_from
}

impl Clone for PaintHandle {
    fn clone(&self) -> PaintHandle;
    // Inherited defaults: clone_from
}

impl Clone for PaintSpan {
    fn clone(&self) -> PaintSpan;
    // Inherited defaults: clone_from
}

impl Clone for ParagraphKey {
    fn clone(&self) -> ParagraphKey;
    // Inherited defaults: clone_from
}

impl Clone for Rect {
    fn clone(&self) -> Rect;
    // Inherited defaults: clone_from
}

impl Clone for Segment {
    fn clone(&self) -> Segment;
    // Inherited defaults: clone_from
}

impl Clone for SelectionSpan {
    fn clone(&self) -> SelectionSpan;
    // Inherited defaults: clone_from
}

impl Clone for ShapedHandle {
    fn clone(&self) -> ShapedHandle;
    // Inherited defaults: clone_from
}

impl Clone for Style {
    fn clone(&self) -> Style;
    // Inherited defaults: clone_from
}

impl Clone for Vec2 {
    fn clone(&self) -> Vec2;
    // Inherited defaults: clone_from
}

impl Clone for profiling::WorkCounters {
    fn clone(&self) -> profiling::WorkCounters;
    // Inherited defaults: clone_from
}

impl Copy for Align {}

impl Copy for BlockKey {}

impl Copy for Caret {}

impl Copy for CaretRect {}

impl Copy for CaretStop {}

impl Copy for Color {}

impl Copy for Draw {}

impl Copy for FontChainHandle {}

impl Copy for FontError {}

impl Copy for FontHandle {}

impl Copy for LineMetrics {}

impl Copy for Motion {}

impl Copy for PaintError {}

impl Copy for PaintHandle {}

impl Copy for ParagraphKey {}

impl Copy for Rect {}

impl Copy for Segment {}

impl Copy for SelectionSpan {}

impl Copy for ShapedHandle {}

impl Copy for Style {}

impl Copy for Vec2 {}

impl Copy for profiling::WorkCounters {}

impl Debug for Align {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for BlockKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for Caret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for CaretRect {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for CaretStop {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for Color {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for Draw {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for FontChainHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for FontError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for FontHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for FontSpan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for Layout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for LayoutLineSpec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for LineMetrics {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for Motion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for PaintError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for PaintHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for PaintSpan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for ParagraphKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for Rect {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for Segment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for SelectionSpan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for ShapedHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for Style {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for Vec2 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Debug for profiling::WorkCounters {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl Default for Draw {
    fn default() -> Self;
}

impl Default for Layout {
    fn default() -> Layout;
}

impl Default for LineMetrics {
    fn default() -> LineMetrics;
}

impl Default for TextService {
    fn default() -> TextService;
}

impl Default for Vec2 {
    fn default() -> Vec2;
}

impl Default for profiling::WorkCounters {
    fn default() -> profiling::WorkCounters;
}

impl Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}

impl Display for PaintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}

impl Eq for Align {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for BlockKey {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for Caret {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for FontChainHandle {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for FontError {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for FontHandle {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for FontSpan {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for Motion {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for PaintError {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for PaintHandle {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for ParagraphKey {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for ShapedHandle {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for Style {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Eq for profiling::WorkCounters {
    // Inherited defaults: assert_receiver_is_total_eq, assert_fields_are_eq
}

impl Error for FontError {
    // Inherited defaults: source, type_id, description, cause, provide
}

impl Error for PaintError {
    // Inherited defaults: source, type_id, description, cause, provide
}

impl From<(f32, f32)> for Vec2 {
    fn from((x, y): (f32, f32)) -> Self;
}

impl From<[f32; 2]> for Vec2 {
    fn from([x, y]: [f32; 2]) -> Self;
}

impl From<[f32; 4]> for Color {
    fn from(v: [f32; 4]) -> Self;
}

impl From<[f32; 4]> for Rect {
    fn from([x, y, width, height]: [f32; 4]) -> Self;
}

impl Hash for Align {
    fn hash<__H: core::hash::Hasher>(&self, state: &mut __H);
    // Inherited defaults: hash_slice
}

impl Hash for BlockKey {
    fn hash<__H: core::hash::Hasher>(&self, state: &mut __H);
    // Inherited defaults: hash_slice
}

impl Hash for FontChainHandle {
    fn hash<__H: core::hash::Hasher>(&self, state: &mut __H);
    // Inherited defaults: hash_slice
}

impl Hash for FontHandle {
    fn hash<__H: core::hash::Hasher>(&self, state: &mut __H);
    // Inherited defaults: hash_slice
}

impl Hash for PaintHandle {
    fn hash<__H: core::hash::Hasher>(&self, state: &mut __H);
    // Inherited defaults: hash_slice
}

impl Hash for ParagraphKey {
    fn hash<__H: core::hash::Hasher>(&self, state: &mut __H);
    // Inherited defaults: hash_slice
}

impl Hash for ShapedHandle {
    fn hash<__H: core::hash::Hasher>(&self, state: &mut __H);
    // Inherited defaults: hash_slice
}

impl Hash for Style {
    fn hash<H: Hasher>(&self, state: &mut H);
    // Inherited defaults: hash_slice
}

impl ParagraphSource for Paragraphs<'_> {
    fn paragraph_text(&self, index: usize, _key: ParagraphKey) -> Option<Cow<'_, str>>;
    // Inherited defaults: paragraph_fonts
}

impl PartialEq for Align {
    fn eq(&self, other: &Align) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for BlockKey {
    fn eq(&self, other: &BlockKey) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for Caret {
    fn eq(&self, other: &Caret) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for Color {
    fn eq(&self, other: &Color) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for Draw {
    fn eq(&self, other: &Draw) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for FontChainHandle {
    fn eq(&self, other: &FontChainHandle) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for FontError {
    fn eq(&self, other: &FontError) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for FontHandle {
    fn eq(&self, other: &FontHandle) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for FontSpan {
    fn eq(&self, other: &FontSpan) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for Motion {
    fn eq(&self, other: &Motion) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for PaintError {
    fn eq(&self, other: &PaintError) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for PaintHandle {
    fn eq(&self, other: &PaintHandle) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for PaintSpan {
    fn eq(&self, other: &PaintSpan) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for ParagraphKey {
    fn eq(&self, other: &ParagraphKey) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for Rect {
    fn eq(&self, other: &Rect) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for ShapedHandle {
    fn eq(&self, other: &ShapedHandle) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for Style {
    fn eq(&self, other: &Self) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for Vec2 {
    fn eq(&self, other: &Vec2) -> bool;
    // Inherited defaults: ne
}

impl PartialEq for profiling::WorkCounters {
    fn eq(&self, other: &profiling::WorkCounters) -> bool;
    // Inherited defaults: ne
}

impl StructuralPartialEq for Align {}

impl StructuralPartialEq for BlockKey {}

impl StructuralPartialEq for Caret {}

impl StructuralPartialEq for Color {}

impl StructuralPartialEq for Draw {}

impl StructuralPartialEq for FontChainHandle {}

impl StructuralPartialEq for FontError {}

impl StructuralPartialEq for FontHandle {}

impl StructuralPartialEq for FontSpan {}

impl StructuralPartialEq for Motion {}

impl StructuralPartialEq for PaintError {}

impl StructuralPartialEq for PaintHandle {}

impl StructuralPartialEq for PaintSpan {}

impl StructuralPartialEq for ParagraphKey {}

impl StructuralPartialEq for Rect {}

impl StructuralPartialEq for ShapedHandle {}

impl StructuralPartialEq for Vec2 {}

impl StructuralPartialEq for profiling::WorkCounters {}

impl WordBoundaries for () {
    fn prev_word(&self, _: usize) -> Option<usize>;
    fn next_word(&self, _: usize) -> Option<usize>;
}
```

## Inferred auto traits

Compiler/target-specific; `!` means a negative implementation.

| Type | Auto traits |
|---|---|
| `sanscale::Align` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Batch` | `!RefUnwindSafe`, `!UnwindSafe`, `Freeze`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin` |
| `sanscale::BlockKey` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Caret` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::CaretRect` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::CaretStop` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Color` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Diagnostics` | `!RefUnwindSafe`, `!UnwindSafe`, `Freeze`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin` |
| `sanscale::Draw` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::FontChainHandle` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::FontError` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::FontHandle` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::FontSpan` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Layout` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::LayoutLineSpec` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::LineMetrics` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Motion` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::PaintError` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::PaintHandle` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::PaintSpan` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::ParagraphKey` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Paragraphs` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Rect` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Segment` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::SelectionSpan` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::ShapedHandle` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::Style` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::TextService` | `!RefUnwindSafe`, `!UnwindSafe`, `Freeze`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin` |
| `sanscale::Vec2` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |
| `sanscale::profiling::WorkCounters` | `Freeze`, `RefUnwindSafe`, `Send`, `Sync`, `Unpin`, `UnsafeUnpin`, `UnwindSafe` |

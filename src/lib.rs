//! Resolution-independent GPU text rendering, via the Slug algorithm
//! (Lengyel, 2017): glyph outlines are stored as quadratic Bézier curves and
//! band tables, and coverage is computed analytically in the fragment shader.
//! Monochrome text has no glyph bitmap and no hinting, so it is exact at any
//! scale — zoom is free, and rotated or perspective text is as sharp as upright
//! text. Color emoji use a separate raster atlas.
//!
//! # The model
//!
//! One [`TextService`] holds every pool. You hold `Copy` handles into it.
//!
//! ```text
//! text ─[itemize]→ runs ─[shape]→ glyphs ─[flow]→ lines ─[rasterize]→ atlas ─[draw]→ quads
//! ```
//!
//! Two levels are visible to you, and they do different jobs:
//!
//! - A **paragraph** ([`ParagraphKey`]) is the unit of *invalidation*. The key
//!   carries your text/font-span version. An edit reuses the other paragraphs'
//!   cached shaping, although composing the block still copies their data.
//! - A **block** ([`BlockKey`] → [`ShapedHandle`]) is the unit of *coordinate
//!   space*: 1..N paragraphs flowed into one byte range and one line list. It is
//!   what you measure, hit-test and draw.
//!
//! For labels without consumer identities, [`TextService::shape_transient`]
//! caches a block by full text and [`Style`]. Different styles coexist; width
//! changes still reuse paragraph shaping. An edit creates a different content
//! key rather than updating a named block. Both entry points use the same engine.
//! For one named paragraph, pass a one-element key slice to [`TextService::shape`].
//!
//! [`TextService::clear`] invalidates old shaped handles and batches even after
//! new layouts are allocated. It retains GPU allocations and the transform, but
//! resets upload state so the next `prepare` uploads the new atlas contents.
//!
//! Shaping is em-space and carries no pixel size and no color, so the cache is
//! zoom-invariant. Size and color enter once, at draw time.
//!
//! Drawing is tiered, and the easy path is literally the hard path plus a drop:
//! [`TextService::draw`] and [`TextService::draw_batch`] are sugar over
//! [`TextService::prepare`], which concatenates blocks into a [`Batch`] — one
//! GPU buffer **you** hold, split into [`Segment`]s where the clip changes.
//! Hold a batch and unchanged content costs zero per-frame upload; ignore the
//! word "batch" entirely and nothing is taken from you.
//!
//! # Inline styles
//!
//! [`ParagraphSource::paragraph_fonts`] supplies paragraph-local [`FontSpan`]s
//! on cache misses. Sorted, nonoverlapping grapheme-safe ranges select real font
//! chains; gaps inherit [`Style::chain`]. The paragraph generation covers text
//! **and effective font spans**. Base-style line metrics remain fixed.
//!
//! Foreground color is separate: [`TextService::register_paint`] owns immutable
//! block-local [`PaintSpan`]s behind a [`PaintHandle`]. Set [`Draw::paint`]; gaps
//! use [`Draw::color`]. Painting never splits shaping: a shaped cluster's start
//! byte chooses its color, and native-color emoji remain untinted. Plain text
//! needs no registration. [`Draw::default`] is allocation-free and has an invalid
//! block, origin (0,0), size 1, opaque black, and no paint or clipping.
//!
//! Re-prepare when draw inputs (including paint) change. [`TextService::drop_paint`]
//! releases a snapshot but not colors already baked into retained batches;
//! [`TextService::batch_live`] still tracks layout/atlas changes, not caller inputs.
//!
//! # What this crate does not own
//!
//! - **Your document storage.** Identity-keyed shaping stores derived results
//!   and asks for characters through [`ParagraphSource`] *only* on a paragraph
//!   cache miss. Your rope, gap buffer or CRDT stays authoritative. The identity-free
//!   `shape_transient` path instead retains copied strings as keys while cached;
//!   it does not impose a document model or edit tracking.
//! - **Your render pass.** You build the pass — target, load op, z-ordering,
//!   scissor — and hand it in; the service records glyph quads into it.
//! - **Font discovery.** You resolve family names to bytes (fontdb does this
//!   well) and hand over an `Arc`; the service owns the fallback *walk*, which
//!   is a shaping concern and can't be outsourced.
//!
//! # Example
//!
//! ```no_run
//! use sanscale::{Align, BlockKey, ParagraphKey, Paragraphs, Style, TextService};
//!
//! let mut text = TextService::new();
//! let font = text.map_font(sanscale::read_font_file("font.ttf")?, 0)?;
//! let chain = text.register_chain(&[font]);
//!
//! let style = Style { chain, wrap_em: Some(20.0), align: Align::Left, line_spacing: 1.2 };
//! let key = ParagraphKey { namespace: 0, slot: 0, generation: 0 };
//! let block = text
//!     .shape(BlockKey(0), &style, &[key], &Paragraphs(&["Hello, world"]))
//!     .expect("shaped");
//!
//! // Geometry is em; multiply by the size you will draw at.
//! let height_px = text.measure(block).height_em() * 32.0;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Nothing above touches a GPU. [`TextService::prepare`] (and the `draw*` sugar
//! over it) is the only path that does.
//!
//! # Performance investigations
//!
//! The optional `perf-counters` feature exposes the `profiling` module's
//! thread-local work counters. Probes, including their arguments, compile out
//! without the feature. These are investigation tools, not production timing:
//! the pathological benchmark runs latency and work/allocation measurement in
//! separate builds. See `performance.md` in the repository for the headless
//! workload matrix and before/after report protocol.
//!
//! # Compatibility
//!
//! - **wgpu 30** — [`TextService::draw`] borrows `wgpu::Device`, `Queue` and
//!   `RenderPass` directly, so your application must use the same wgpu major
//!   version. Bumping it here is a breaking change.
//! - **MSRV: Rust 1.87.**

mod bands;
mod cache;
mod emoji;
mod emoji_presentation;
mod flow;
mod font;
mod layout;
mod outline;
mod renderer;
mod spans;
mod text;
mod vertex;
mod work;

#[cfg(feature = "perf-counters")]
pub mod profiling;

pub use font::{FontMetrics, read_font_file};

pub use text::{
    Align, Batch, BlockKey, Boundaries, Caret, CaretRect, CaretStop, Color, Diagnostics, Draw,
    FontChainHandle, FontData, FontError, FontHandle, Layout, LayoutLineSpec, LineMetrics, Motion,
    ParagraphKey, ParagraphSource, Paragraphs, Rect, Segment, SelectionSpan, ShapedHandle, Style,
    TextService, Vec2,
};

pub use spans::{FontSpan, PaintError, PaintHandle, PaintSpan};

//! Resolution-independent GPU text rendering, via the Slug algorithm
//! (Lengyel, 2017): glyph outlines are stored as quadratic Bézier curves and
//! band tables, and coverage is computed analytically in the fragment shader.
//! There is no glyph bitmap and no hinting, so text is exact at any scale — zoom
//! is free, and rotated or perspective text is as sharp as upright text.
//!
//! # The model
//!
//! One [`Text`] service holds every pool. You hold `Copy` handles into it.
//!
//! ```text
//! text ─[itemize]→ runs ─[shape]→ glyphs ─[flow]→ lines ─[rasterize]→ atlas ─[draw]→ quads
//! ```
//!
//! Two levels are visible to you, and they do different jobs:
//!
//! - A **paragraph** ([`ParagraphKey`]) is the unit of *invalidation*. The key
//!   carries your own version, so an edit reshapes the paragraph you touched and
//!   nothing else.
//! - A **block** ([`BlockKey`] → [`ShapedHandle`]) is the unit of *coordinate
//!   space*: 1..N paragraphs flowed into one byte range and one line list. It is
//!   what you measure, hit-test and draw.
//!
//! Shaping is em-space and carries no pixel size and no color, so the cache is
//! zoom-invariant. Size and color enter once, at [`Text::draw`].
//!
//! # What this crate does not own
//!
//! - **Your text.** The service stores only derived shaping, keyed by identity,
//!   and asks for a paragraph's characters through [`ParagraphSource`] *only*
//!   when that paragraph misses the cache. Your rope, gap buffer or CRDT stays
//!   authoritative.
//! - **Your render pass.** You build the pass — target, load op, z-ordering,
//!   scissor — and hand it in; the service records glyph quads into it.
//! - **Font discovery.** You resolve family names to bytes (fontdb does this
//!   well) and hand over an `Arc`; the service owns the fallback *walk*, which
//!   is a shaping concern and can't be outsourced.
//!
//! # Example
//!
//! ```no_run
//! use sanscale::{Align, BlockKey, ParagraphKey, Paragraphs, Style, Text};
//!
//! let mut text = Text::new();
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
//! Nothing above touches a GPU. [`Text::draw`] is the only method that does.
//!
//! # Compatibility
//!
//! - **wgpu 30** — [`Text::draw`] borrows `wgpu::Device`, `Queue` and
//!   `RenderPass` directly, so your application must use the same wgpu major
//!   version. Bumping it here is a breaking change.
//! - **MSRV: Rust 1.82.**

mod bands;
mod cache;
mod emoji;
mod emoji_presentation;
mod flow;
mod font;
mod layout;
mod outline;
mod renderer;
mod text;
mod vertex;

pub use font::{read_font_file, FontMetrics};

pub use text::{
    Align, BlockKey, CaretHit, CaretRect, CaretStop, Color, Diagnostics, Draw, FontChainHandle,
    FontData,
    FontError, FontHandle, Layout, LayoutLineSpec, LineMetrics, ParagraphKey, ParagraphSource,
    Paragraphs, Rect, SelectionSpan, ShapedHandle, Style, Text, Vec2,
};

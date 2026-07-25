//! GPU font renderer based on the Slug algorithm (Lengyel, 2017).
//!
//! Loads TTF/OTF fonts, shapes text with rustybuzz, builds a GPU glyph cache of
//! quadratic Bézier curves and bands, and renders solid filled text via a wgpu
//! pipeline.
//!
//! # Compatibility
//!
//! - **wgpu: `29`** — this crate is built against wgpu 29 and re-exports types
//!   ([`TextRenderer`], [`TextAtlas`]) that borrow `wgpu::Device`/`Queue`
//!   directly, so your application **must** depend on the same wgpu major
//!   version. Bumping wgpu here is a breaking change.
//! - **MSRV: Rust 1.82.**


mod bands;
mod cache;
mod emoji;
mod emoji_presentation;
pub mod engine;
mod font;
mod layout;
mod outline;
mod renderer;
pub mod text;
mod vertex;

pub use engine::{
    Align, CaretHit, CaretRect, CaretStop, ParagraphIdentity, PushedGlyph, SelectionSpan,
    TextArgs, TextClip, TextEngine, TextLayout, TextLayoutLine, TextParagraph,
    TextParagraphIdentity, TextParagraphProvider,
};
pub use font::{FontMetrics, FontSource};
pub use renderer::{EmojiAtlas, EmojiRenderer, TextAtlas, TextRenderer};
pub use vertex::{EmojiVertex, TextVertex};

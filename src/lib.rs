//! GPU font renderer based on the Slug algorithm (Lengyel, 2017).
//!
//! Loads TTF/OTF fonts, shapes text with rustybuzz, builds a GPU glyph cache of
//! quadratic Bézier curves and bands, and renders solid filled text via a wgpu
//! pipeline.


mod bands;
mod cache;
mod emoji;
pub mod engine;
mod font;
mod layout;
mod outline;
mod renderer;
mod vertex;

pub use engine::{
    Align, CaretHit, CaretRect, CaretStop, ParagraphIdentity, PushedGlyph, SelectionSpan,
    TextArgs, TextClip, TextEngine, TextLayout, TextLayoutLine, TextParagraph,
    TextParagraphIdentity, TextParagraphProvider,
};
pub use font::{FontMetrics, FontSource};
pub use renderer::{EmojiAtlas, EmojiRenderer, TextAtlas, TextRenderer};
pub use vertex::{EmojiVertex, TextVertex};

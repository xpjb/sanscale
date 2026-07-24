//! Challenging Unicode: color emoji, CJK, Arabic/Hebrew, Indic, Greek, Cyrillic,
//! math/symbols — all resolved through a system-font fallback chain and drawn in
//! one pass (vector glyphs + the color-emoji atlas). Writes `unicode.png`.
//!
//! Run with:  `cargo run --example unicode`

mod common;

use common::{font_chain, unicode_sections, Harness, UNICODE_FALLBACK};
use sanscale::{EmojiRenderer, TextArgs, TextEngine, TextRenderer};

fn main() {
    let (width, height) = (1220u32, 1000u32);
    let harness = Harness::new(width, height);

    let sources = font_chain(UNICODE_FALLBACK);
    if sources.is_empty() {
        eprintln!("no fonts found via fontdb");
        return;
    }
    let mut engine = TextEngine::from_sources(sources).expect("build fallback chain");
    println!("fallback chain: {}", engine.fallback_family_names().join(" → "));

    let text_renderer = TextRenderer::new(&harness.device, &harness.config);
    let emoji_renderer = EmojiRenderer::new(&harness.device, &harness.config);
    let mut text_atlas = engine.new_atlas(&harness.device, &harness.queue, &text_renderer.atlas_layout);
    let mut emoji_atlas =
        engine.new_emoji_atlas(&harness.device, &harness.queue, &emoji_renderer.atlas_layout);

    let label = TextArgs {
        size_px: 15.0,
        color: [0.45, 0.47, 0.52, 1.0],
        ..Default::default()
    };
    let sample = TextArgs {
        size_px: 30.0,
        color: [0.10, 0.11, 0.13, 1.0],
        ..Default::default()
    };

    let mut y = 46.0;
    for (name, text) in unicode_sections() {
        engine.text(40.0, y, name, &label);
        engine.text(40.0, y + 34.0, text, &sample);
        y += 76.0;
    }

    // Report any codepoints no installed face could cover (diagnostic).
    let all: String = unicode_sections().iter().map(|(_, t)| *t).collect();
    let missing = engine.uncovered_chars(&all);
    if !missing.is_empty() {
        println!("uncovered on this system (no font): {missing:?}");
    }

    engine.sync_atlas(&mut text_atlas, &harness.device, &harness.queue, &text_renderer.atlas_layout);
    engine.sync_emoji_atlas(&mut emoji_atlas, &harness.device, &harness.queue, &emoji_renderer.atlas_layout);
    let text_vertices = engine.flush().to_vec();
    let emoji_vertices = engine.emoji_vertices().to_vec();

    harness.save_png_with_emoji(
        &text_renderer,
        &text_atlas,
        &text_vertices,
        &emoji_renderer,
        &emoji_atlas,
        &emoji_vertices,
        wgpu::Color::WHITE,
        "unicode.png",
    );
    println!(
        "wrote unicode.png ({} glyphs, {} color emoji)",
        text_vertices.len() / 6,
        emoji_vertices.len() / 6
    );
}

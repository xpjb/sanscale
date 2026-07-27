//! Challenging Unicode: color emoji, CJK, Arabic/Hebrew, Indic, Greek, Cyrillic,
//! math/symbols — all resolved through a system-font fallback chain and drawn in
//! one pass (vector glyphs plus the color-emoji atlas). Writes `unicode.png`.
//!
//! Each labelled sample is its own block, so this also exercises many small
//! blocks sharing one font pool and one glyph cache.
//!
//! Run with:  `cargo run --example unicode`

mod common;

use common::{unicode_sections, Harness, UNICODE_FALLBACK};
use sanscale::{Align, Color, ShapedHandle, Style, TextService, Vec2};

fn main() {
    let (width, height) = (1220u32, 1000u32);
    let harness = Harness::new(width, height);

    let mut text = TextService::new();
    let chain = common::font_chain(&mut text, UNICODE_FALLBACK);
    let families = text.diagnostics().chain_families(chain);
    if families.is_empty() {
        eprintln!("no fonts found via fontdb");
        return;
    }
    println!("fallback chain: {}", families.join(" → "));

    let style = Style {
        chain,
        wrap_em: None,
        align: Align::Left,
        line_spacing: 1.2,
    };

    // Labels and samples differ only in draw size — one style, one cache.
    let sections = unicode_sections();
    let mut blocks: Vec<(ShapedHandle, ShapedHandle)> = Vec::new();
    for (name, sample) in sections.iter() {
        // Literals have no stable identity, so content-key them.
        let Some(label) = text.shape_transient(name, &style) else {
            continue;
        };
        let Some(body) = text.shape_transient(sample, &style) else {
            continue;
        };
        blocks.push((label, body));
    }
    // Diagnostic: any code point no installed face can cover renders as tofu.
    let all: String = sections.iter().map(|(_, t)| *t).collect();
    let missing = text.diagnostics().uncovered_chars(chain, &all);
    if !missing.is_empty() {
        println!("uncovered on this system (no font): {missing:?}");
    }

    harness.save_png(
        &mut text,
        wgpu::Color::WHITE,
        "unicode.png",
        |text, device, queue, pass| {
            let mut y = 40.0;
            for (label, body) in &blocks {
                text.draw(
                    device,
                    queue,
                    pass,
                    *label,
                    Vec2::new(40.0, y),
                    15.0,
                    Color([0.45, 0.47, 0.52, 1.0]),
                    None,
                );
                text.draw(
                    device,
                    queue,
                    pass,
                    *body,
                    Vec2::new(40.0, y + 24.0),
                    30.0,
                    Color([0.10, 0.11, 0.13, 1.0]),
                    None,
                );
                y += 76.0;
            }
        },
    );
    let (paragraphs, live) = text.diagnostics().cache_occupancy();
    println!("wrote unicode.png ({paragraphs} cached paragraphs, {live} live blocks)");
}

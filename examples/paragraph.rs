//! Layout features: wrapping, alignment, measurement, and mixed sizes (all crisp
//! — coverage is computed from curves per-pixel, so there is no atlas resolution
//! to outgrow). Also shows a **block**: several paragraphs flowed as one unit
//! with continuous byte offsets. Writes `paragraph.png`.
//!
//! Run with:  `cargo run --example paragraph`

mod common;

use common::Harness;
use sanscale::{Align, BlockKey, Color, ParagraphKey, Paragraphs, Style, TextService, Vec2};

const BODY: &[&str] = &[
    "sanscale renders each glyph from its Bézier outline with analytic per-pixel \
coverage — resolution-independent, with no glyph atlas to re-rasterize when you zoom.",
    "These two paragraphs are one block: they flow together, share a byte range, \
and are measured and drawn as a single unit — but each caches separately, so \
editing one never reshapes the other.",
];

/// Paragraph identities. A real consumer uses its document's own ids; the point
/// is that *it* owns them, so the service never has to guess what changed.
fn keys(namespace: u64, count: usize) -> Vec<ParagraphKey> {
    (0..count)
        .map(|i| ParagraphKey {
            namespace,
            slot: i as u32,
            generation: 0,
        })
        .collect()
}

fn main() {
    let (width, height) = (760u32, 600u32);
    let harness = Harness::new(width, height);

    let mut text = TextService::new();
    let chain = common::font_chain(
        &mut text,
        &["Segoe UI", "Helvetica Neue", "Arial", "DejaVu Sans"],
    );

    let ink = Color([0.10, 0.11, 0.13, 1.0]);
    let muted = Color([0.40, 0.42, 0.46, 1.0]);
    let margin = 40.0;
    let body_px = 22.0;
    let heading_px = 64.0;

    // Wrap width is em: pixels / font size. It is the consumer's one conversion,
    // and it is why the cache survives a zoom — both terms scale together.
    let wrap_px = width as f32 - margin * 2.0;
    let body_style = Style {
        chain,
        wrap_em: Some(wrap_px / body_px),
        align: Align::Left,
        line_spacing: 1.3,
    };
    let heading_style = Style {
        wrap_em: None,
        ..body_style
    };

    let heading_key = keys(1, 1);
    let Some(heading) = text.shape(
        BlockKey(1),
        &heading_style,
        &heading_key,
        &Paragraphs(&["Resolution"]),
    ) else {
        eprintln!("no usable system font found");
        return;
    };

    let body_keys = keys(2, BODY.len());
    let body = text
        .shape(BlockKey(2), &body_style, &body_keys, &Paragraphs(BODY))
        .expect("shape body");

    let layout = text.measure(body);
    println!(
        "body: {} lines, {:.2} x {:.2} em ({:.0} px tall at {body_px}px), {} bytes",
        layout.line_count(),
        layout.width_em(),
        layout.height_em(),
        layout.height_em() * body_px,
        layout.len_bytes()
    );

    // The same block, centered — a different style, so a separate cache entry,
    // but the paragraphs' shaping is what gets reused underneath.
    let centered_style = Style {
        align: Align::Center,
        ..body_style
    };
    let centered = text
        .shape(BlockKey(3), &centered_style, &body_keys, &Paragraphs(BODY))
        .expect("shape centered");

    harness.save_png(
        &mut text,
        wgpu::Color::WHITE,
        "paragraph.png",
        |text, device, queue, pass| {
            let mut draw = |block, y, size, color| {
                text.draw(
                    device,
                    queue,
                    pass,
                    block,
                    Vec2::new(margin, y),
                    size,
                    color,
                    None,
                );
            };
            draw(heading, 30.0, heading_px, ink);
            draw(body, 150.0, body_px, ink);
            draw(centered, 340.0, body_px, muted);
        },
    );
    println!("wrote paragraph.png");
}

//! Layout features: wrapping to a width, alignment, measurement, and mixed sizes
//! (all crisp — coverage is computed from curves per-pixel, so there is no atlas
//! resolution to outgrow). Writes `paragraph.png`.
//!
//! Run with:  `cargo run --example paragraph`

mod common;

use common::{Harness, FONT_CANDIDATES};
use sanscale::{Align, TextArgs, TextEngine, TextRenderer};

const BODY: &str = "sanscale renders each glyph from its Bézier outline with \
analytic per-pixel coverage — resolution-independent, no glyph atlas to \
re-rasterize when you zoom. This paragraph is wrapped to a fixed pixel width.";

fn main() {
    let (width, height) = (760, 520);
    let harness = Harness::new(width, height);

    let Some(mut engine) = FONT_CANDIDATES
        .iter()
        .find_map(|path| TextEngine::load(path).ok())
    else {
        eprintln!("no system font found; edit FONT_CANDIDATES in examples/common");
        return;
    };

    let renderer = TextRenderer::new(&harness.device, &harness.config);
    let mut atlas = engine.new_atlas(&harness.device, &harness.queue, &renderer.atlas_layout);

    let ink = [0.10, 0.11, 0.13, 1.0];
    let muted = [0.40, 0.42, 0.46, 1.0];
    let margin = 40.0;
    let wrap_w = width as f32 - margin * 2.0;

    // A large heading — same engine, much bigger size, still sharp.
    let heading = TextArgs {
        size_px: 64.0,
        color: ink,
        ..TextArgs::default()
    };
    engine.text(margin, 90.0, "Resolution", &heading);

    // Measure the wrapped body before drawing it (layout is cached and reused).
    let body_args = TextArgs {
        size_px: 22.0,
        color: ink,
        max_width_px: Some(wrap_w),
        align: Align::Left,
        ..TextArgs::default()
    };
    let measured = engine.layout(BODY, &body_args);
    println!(
        "body wraps to {} lines, {:.0}x{:.0}px",
        measured.lines.len(),
        measured.width_px,
        measured.height_px
    );

    // Left-aligned wrapped body.
    engine.text(margin, 170.0, BODY, &body_args);

    // The same text, centered and muted, lower on the canvas.
    let centered = TextArgs {
        color: muted,
        align: Align::Center,
        ..body_args.clone()
    };
    engine.text(margin, 360.0, BODY, &centered);

    engine.sync_atlas(&mut atlas, &harness.device, &harness.queue, &renderer.atlas_layout);
    let vertices = engine.flush().to_vec();

    harness.save_png(
        &renderer,
        &atlas,
        &vertices,
        wgpu::Color::WHITE,
        "paragraph.png",
    );
    println!("wrote paragraph.png ({} vertices)", vertices.len());
}

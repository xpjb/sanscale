//! Minimal end-to-end: load a font, render a line of text, save a PNG.
//!
//! Headless — no window is opened. The GPU/PNG plumbing lives in `common`; this
//! file is just the sanscale path: font → `text` → `flush` → upload → draw.
//!
//! Run with:  `cargo run --example hello_png`  (writes `hello.png`)

mod common;

use common::{Harness, FONT_CANDIDATES};
use sanscale::{TextArgs, TextEngine, TextRenderer};

fn main() {
    let harness = Harness::new(900, 220);

    // Load the first available system font into a TextEngine.
    let Some(mut engine) = FONT_CANDIDATES
        .iter()
        .find_map(|path| TextEngine::load(path).ok())
    else {
        eprintln!("no system font found; edit FONT_CANDIDATES in examples/common");
        return;
    };

    // The renderer owns the GPU pipeline; the atlas caches glyph curves on the GPU.
    let renderer = TextRenderer::new(&harness.device, &harness.config);
    let mut atlas = engine.new_atlas(&harness.device, &harness.queue, &renderer.atlas_layout);

    // Queue one line of text at a pixel baseline, upload any new glyphs, flush.
    let args = TextArgs {
        size_px: 96.0,
        color: [0.10, 0.11, 0.13, 1.0],
        ..TextArgs::default()
    };
    engine.text(40.0, 150.0, "Hello, sanscale!", &args);
    engine.sync_atlas(&mut atlas, &harness.device, &harness.queue, &renderer.atlas_layout);
    let vertices = engine.flush().to_vec();

    harness.save_png(&renderer, &atlas, &vertices, wgpu::Color::WHITE, "hello.png");
    println!("wrote hello.png ({} vertices)", vertices.len());
}

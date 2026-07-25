//! Minimal end-to-end: register a font, shape a line, draw it to a PNG.
//!
//! Headless — no window is opened. The GPU/PNG plumbing lives in `common`; this
//! file is the whole sanscale path: map font → register chain → shape → draw.
//!
//! Run with:  `cargo run --example hello_png`  (writes `hello.png`)

mod common;

use common::Harness;
use sanscale::{Align, BlockKey, Color, ParagraphKey, Paragraphs, Style, Text, Vec2};

const TEXT: &str = "Hello, sanscale!";

fn main() {
    let harness = Harness::new(900, 220);

    // One service holds the font pool, the caches and (lazily) the GPU
    // resources. Nothing below touches a device until `save_png` draws.
    let mut text = Text::new();
    let chain = common::font_chain(
        &mut text,
        &["Segoe UI", "Helvetica Neue", "Arial", "DejaVu Sans"],
    );

    let style = Style {
        chain,
        wrap_em: None,
        align: Align::Left,
        line_spacing: 1.2,
    };

    // Identity is the consumer's to mint. A one-off like this can use any stable
    // key; a document would use its own node/paragraph ids.
    let key = ParagraphKey {
        namespace: 0,
        slot: 0,
        generation: 0,
    };
    let Some(block) = text.shape(BlockKey(0), &style, &[key], &Paragraphs(&[TEXT])) else {
        eprintln!("no usable system font found; edit the family list above");
        return;
    };

    // Geometry is em, so it is the same at every size. Multiply to place it.
    let size_px = 96.0;
    let measured = text.measure(block).size_em();
    println!(
        "shaped {TEXT:?} — {:.2} x {:.2} em ({:.0} x {:.0} px at {size_px}px)",
        measured.x,
        measured.y,
        measured.x * size_px,
        measured.y * size_px
    );

    harness.save_png(
        &mut text,
        wgpu::Color::WHITE,
        "hello.png",
        |text, device, queue, pass| {
            text.draw(
                device,
                queue,
                pass,
                block,
                Vec2::new(40.0, 40.0),
                size_px,
                Color([0.10, 0.11, 0.13, 1.0]),
                None,
            );
        },
    );
    println!("wrote hello.png");
}

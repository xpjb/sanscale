Sanscale is a text library. It exposes enough control to never discourage you from using it. It is intended for text heavy applications like editors, chats, games. The renderer is Eric Lengyel's 'Slug' algorithm.

Sanscale shapes text with rustybuzz and draws monochrome glyphs from their
outlines on wgpu, so they stay sharp when you zoom. Color emoji use a separate
raster atlas. You provide font bytes, text storage, and the render pass.

## Install

```toml
[dependencies]
sanscale = "0.1.0"
wgpu = "30"
```

Requires Rust 1.87 or newer. Sanscale uses your application's wgpu device,
queue, and render pass.

## Quick start

With a wgpu device, queue, render pass, target format, and target dimensions:

```rust
use sanscale::{Align, Color, Style, TextService, Vec2};

let mut text = TextService::new();
let font = text.map_font(sanscale::read_font_file("font.ttf")?, 0)?;
let style = Style {
    chain: text.register_chain(&[font]),
    wrap_em: Some(24.0),
    align: Align::Left,
    line_spacing: 1.0,
};
let block = text.shape_transient("Hello, Sanscale!", &style).unwrap();

text.set_target(&device, surface_format);
text.set_transform(&queue, TextService::pixel_ortho(width, height));
text.draw(&device, &queue, &mut pass, block, Vec2::new(40.0, 80.0), 32.0,
    Color([1.0, 1.0, 1.0, 1.0]), None);
```

`pixel_ortho` uses screen pixels with the origin at the top left. Layout
measurements use em units. You can also supply a world-space transform.

## API

- **Fonts:** map shared font bytes and register ordered fallback chains. Font
  discovery is yours; [`fontdb`](https://crates.io/crates/fontdb) works for
  system fonts.
- **Text and layout:** `shape` accepts keyed paragraphs from your own
  `ParagraphSource`. Increment `ParagraphKey::generation` when text or font
  spans change; unchanged paragraphs can reuse their shaping. `Style` sets
  wrapping, alignment, and line spacing. `measure` gives you bounds,
  hit-testing, caret movement, and selection geometry without a GPU.
- **Inline styles:** supply `FontSpan` ranges through
  `ParagraphSource::paragraph_fonts`. Register `PaintSpan` ranges with
  `register_paint` and pass the handle in `Draw::paint` to color text without
  reshaping. Color emoji keep their own colors.
- **Drawing:** `Draw` sets position, size, color, and CPU culling bounds. Use
  `draw_batch` for many blocks, or keep a batch from `prepare` and record it
  with `draw_prepared`. Re-prepare when your draw inputs change or `batch_live`
  returns false. Set a scissor on your render pass for hard clipping; use
  `Batch::segments` and `draw_segment` when the scissor changes between blocks.

## Examples

`cargo run --release --example hello` opens the smallest live example. `editor`
shows caret and selection geometry; `code-editor` and `markdown-editor` show
styled editing. `unicode` and `emoji` show font coverage and color glyphs. Add
`-- --dump` to render a PNG without opening a window. The Markdown parser
belongs to its example, not the library.

[API docs](https://docs.rs/sanscale/0.1.0/sanscale/) · [Performance notes](performance.md) · [Example previews](https://github.com/xpjb/sanscale/tree/master/gallery)

## License

[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE). Eric Lengyel dedicated the [Slug reference shaders](https://github.com/EricLengyel/Slug) to the public domain.

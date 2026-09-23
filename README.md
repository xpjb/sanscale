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

[Full public declarations](public-api.md) — generated type definitions and signatures,
including the optional profiling surface.

- **Fonts:** map shared font bytes and register ordered fallback chains. Font
  discovery is yours; [`fontdb`](https://crates.io/crates/fontdb) works for
  system fonts.
- **Text and layout:** `shape` accepts keyed paragraphs from your own
  `ParagraphSource`. Increment `ParagraphKey::generation` when text or font
  spans change; changing wrap width reuses unchanged paragraphs' shaped glyphs
  and reflows their lines. `Style` sets wrapping, alignment, and line spacing.
  `measure` gives you bounds,
  hit-testing, caret movement, and selection geometry without a GPU. Namespace,
  slot, and generation are consumer-controlled identity components; keep the
  full identity unique across sources sharing a service. For one paragraph,
  call `shape(block_key, &style, &[paragraph_key], &source)`.
- **Labels:** `shape_transient` caches by full text and style, independently of
  caller-supplied identities. Different wrapping/font styles coexist; width-only
  changes reuse paragraph shaping. This path retains text as cache keys until
  eviction. An edit names a different block; use `shape` for an object whose
  identity should survive edits.
- **Reset:** `clear` invalidates shaped handles and retained batches, including
  after new text is loaded. GPU allocations and the transform stay; the next
  `prepare` uploads the replacement atlas contents. Re-map fonts and chains after
  clearing.
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

Run an example with `cargo run --release --example <name>`. All six open a
window; add `-- --dump` to render a preview PNG without opening one.

| Preview | Example |
|---|---|
| [![Hello](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/hello.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/hello.png) | [`hello`](examples/hello.rs)<br>Moving Unicode text and a live FPS counter |
| [![Unicode](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/unicode.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/unicode.png) | [`unicode`](examples/unicode.rs)<br>Zoomable map of Unicode planes 0–2 |
| [![Emoji](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/emoji.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/emoji.png) | [`emoji`](examples/emoji.rs)<br>RGI emoji sequences grouped like a picker |
| [![Editor](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/editor.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/editor.png) | [`editor`](examples/editor.rs)<br>Plain notepad with caret, selection, and wrapping |
| [![Code editor](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/code-editor.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/code-editor.png) | [`code-editor`](examples/code-editor.rs)<br>C syntax colors and bold/italic fonts |
| [![Markdown editor](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/markdown-editor.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/markdown-editor.png) | [`markdown-editor`](examples/markdown-editor.rs)<br>Split source/preview and streaming tables |

The Markdown parser belongs to its example, not the library.

[API docs](https://docs.rs/sanscale/0.1.0/sanscale/) · [Performance notes](performance.md)

## License

[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE). Eric Lengyel dedicated the [Slug reference shaders](https://github.com/EricLengyel/Slug) to the public domain.

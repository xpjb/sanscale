Sanscale is a text library. It exposes enough control to never discourage you from using it. It is intended for text heavy applications like editors, chats, games. The renderer is Eric Lengyel's 'Slug' algorithm.

Sanscale shapes text with rustybuzz and draws monochrome glyphs from their
outlines on wgpu, so they stay sharp when you zoom. Color emoji use separate
append-only raster pages. You provide font bytes, text storage, and the render pass.

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
use sanscale::{Align, Color, Draw, Style, TextService, Vec2};

let mut text = TextService::new();
let font = text.map_font(sanscale::read_font_file("font.ttf")?, 0)?;
let style = Style {
    chain: text.register_chain(&[font])?,
    wrap_em: Some(24.0),
    align: Align::Left,
    line_spacing: 1.0,
};
let block = text.shape_transient("Hello, Sanscale!", &style).unwrap();

text.set_target(&device, surface_format);
text.set_transform(TextService::pixel_ortho(width, height));
text.draw(&device, &queue, &mut pass, Draw {
    block, at: Vec2::new(40.0, 80.0), size: 32.0,
    color: Color([1.0, 1.0, 1.0, 1.0]), ..Default::default()
});
```

`pixel_ortho` uses screen pixels with the origin at the top left. Layout
measurements use em units. You can also supply a world-space transform. Changed
transforms have immutable GPU storage, so several passes can use different
matrices before one submission. Set a target before preparing/drawing; one service
uses one device, with single-sample color targets and no depth testing.

Line flow currently assumes LTR visual order; bidirectional paragraph ordering is
not implemented. Text storage and IME composition remain application concerns.

## API

[Full public declarations](public-api.md) — generated type definitions and signatures,
including the optional profiling surface.

- **Fonts:** map shared font bytes and register ordered fallback chains. Font
  discovery is yours; [`fontdb`](https://crates.io/crates/fontdb) works for
  system fonts. Registration is fallible; releasing a stale chain is harmless.
  Font/chain/paint/shaped handles and batches are local to their service.
- **Text and layout:** `shape` accepts keyed paragraphs from your own
  `ParagraphSource`. Increment `ParagraphKey::generation` when text or font
  spans change; changing wrap width reuses unchanged paragraphs' shaped glyphs
  and reflows their lines. `Style` sets wrapping, alignment, and line spacing.
  `measure` gives you bounds,
  hit-testing, caret movement, and selection geometry without a GPU. Namespace,
  slot, and generation are consumer-controlled identity components; keep the
  full identity unique across sources sharing a service. A `BlockKey` identifies
  a mutable layout instance: simultaneous different layouts of the same content
  need different block keys. Paragraph namespaces do not namespace block keys.
  For one paragraph,
  call `shape(block_key, &style, &[paragraph_key], &source)`.
- **Labels:** `shape_transient` caches by full text and style, independently of
  caller-supplied identities. Different wrapping/font styles coexist; width-only
  changes reuse paragraph shaping. This path retains text as cache keys until
  eviction. An edit names a different block; use `shape` for an object whose
  identity should survive edits.
- **Reset:** `clear` invalidates shaped handles and retained batches, including
  after new text is loaded. Pipelines, transform and monochrome atlas allocations
  stay. Emoji cache ownership is released; retained batches may own old pages.
  Recreate fonts/chains/paint and re-shape before preparing replacement content.
- **Inline styles:** supply `FontSpan` ranges through
  `ParagraphSource::paragraph_fonts`. Register `PaintSpan` ranges with
  `register_paint` and pass the handle in `Draw::paint` to color text without
  reshaping. Color emoji keep their own colors, including alpha; foreground paint
  is not whole-run opacity.
- **Drawing:** `Draw` sets position, size, color, and CPU culling bounds. Use
  `draw_batch` for many blocks, or keep a batch from `prepare` and record it
  with `draw_prepared`. Input order is preserved across text/emoji pipeline runs.
  Re-prepare when your draw inputs change. When `batch_live` returns false,
  re-issue shaping and refresh the handles first: re-preparing an evicted handle
  cannot recover its text. Changing pixel scale invalidates affected emoji buckets,
  not monochrome-only batches. Set a scissor on your render pass for hard clipping; use
  `Batch::segments` and `draw_segment` when the scissor changes between blocks.
- **Emoji residency:** the service caches up to 64 MiB of small append-only pages.
  Eviction removes cache entries without overwriting queued/retained pixels.
  Batches own their pages, including evicted ones; dropping batches releases that
  ownership. `diagnostics().emoji_cache_usage()` reports cache-owned pages/bytes.
- **Carets:** hit-testing and motion return a `Caret`; pass it directly to
  `caret_rect`. Use `clamp_caret` after reflow, `caret_at` for default placement,
  and `caret_after_edit` for end-affine placement after typing.

## Editor integration

The service supplies movement; your editor supplies keybindings and selection state.
Keep a `Caret` **and** an `Option<f32>` horizontal goal between key events:

```rust,ignore
caret = layout.caret_move(caret, motion, &mut goal_x, &word_boundaries);
let rect = layout.caret_rect(caret);
```

- Up/Down → `Motion::Up` / `Down`; retaining `goal_x` prevents drifting across short lines.
- Ctrl+Left/Right → `WordLeft` / `WordRight`, with a `Boundaries` implementation over
  your text. **Passing `()` falls back to cluster steps, not word navigation.**
- Home/End, PageUp/PageDown (visual-line stride), and DocStart/DocEnd are also motions.
- Shift extends from your selection anchor; movement alone does not own a selection.
- Clicks use `hit_test`; clear the horizontal goal after mouse placement or editing.
- After typing use `caret_after_edit`; after reflow use `clamp_caret`. Keep the
  returned line affinity rather than reducing the caret to just a byte offset.

Removing byte-only geometry helpers makes affinity explicit; it does not make
missing keybindings a compile error. Test navigation, selection extension, word
boundaries, hard/soft wraps and preferred-column recovery in the consuming editor.
See `examples/editor.rs` for a complete input adapter.

### Migrating from the reviewed prerelease (`e5c9b03`)

- `register_chain` now returns `Result<FontChainHandle, FontError>`.
- `set_transform(matrix)` no longer takes a queue; setting it before the target works.
- `draw(device, queue, pass, Draw { ... })` replaces positional draw fields and supports paint.
- `caret_rect(Caret)` replaces byte-only geometry and `caret_rect_on_line`;
  `caret_position` is removed and `line_for_byte` is private. Use the caret operations above.
- The unused public `FontMetrics` export is removed. `read_font_file` accepts paths;
  word-boundary arguments also accept trait objects.
- A stale/incomplete prepare stays non-live. Refresh evicted shape handles before
  preparing; emoji bucket changes now invalidate retained native-color batches.

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

# sanscale

**Resolution-independent GPU text rendering for [wgpu](https://wgpu.rs).**

sanscale draws each glyph directly from its quadratic Bézier outline with
analytic per-pixel coverage — Eric Lengyel's [Slug](https://sluglibrary.com)
algorithm. There is no glyph bitmap and no signed-distance field, so text stays
razor-sharp at any zoom without ever re-rasterizing an atlas.

```toml
[dependencies]
sanscale = { git = "https://github.com/xpjb/sanscale" }
```

<sub>License: MIT OR Apache-2.0 · wgpu 30</sub>

## Features

- **Resolution-independent** — coverage is computed from curves per fragment;
  zoom freely, no atlas resolution to outgrow.
- **Anti-aliased** by construction (analytic coverage, not supersampling).
- **Band-accelerated** — glyph outlines are spatially indexed into bands so the
  fragment shader tests only nearby curves, not the whole outline.
- **Lazy, incremental atlas** — glyphs are cached and uploaded to the GPU on
  first use; nothing to pre-declare.
- **Real shaping** via [rustybuzz](https://crates.io/crates/rustybuzz), script
  itemization, and multi-font **fallback chains**.
- **Color emoji** (COLR v0/v1) through a rasterized side atlas.
- **Layout**: line wrapping, left/center/right alignment, multi-paragraph runs.
- **Editor geometry**: measurement, caret positions, hit-testing, selection
  rectangles, and pixel clip rectangles.

## Quick start

```rust
use sanscale::{Align, BlockKey, Color, ParagraphKey, Paragraphs, Style, Text, Vec2};

// 1. One service holds every pool, every cache, and (lazily) the GPU resources.
//    You hold `Copy` handles into it.
let mut text = Text::new();
let font = text.map_font(sanscale::read_font_file("/path/to/font.ttf")?, 0)?;
let chain = text.register_chain(&[font]);   // ordered fallback chain

// 2. Shape a block. A style carries no pixel size and no color, so the layout
//    cache is zoom-invariant; a paragraph key carries your own version, so an
//    edit reshapes the paragraph you touched and nothing else.
let style = Style { chain, wrap_em: Some(20.0), align: Align::Left, line_spacing: 1.2 };
let key = ParagraphKey { namespace: 0, slot: 0, generation: 0 };
let block = text
    .shape(BlockKey(0), &style, &[key], &Paragraphs(&["Hello, sanscale!"]))
    .expect("shaped");

// 3. Measure, hit-test and lay out with no GPU in sight. Geometry is em —
//    multiply by the size you will draw at.
let height_px = text.measure(block).height_em() * 32.0;

// 4. Once per target format, then once per pass. Screen space is `pixel_ortho`
//    (0,0 = top-left); world or 3D text is an MVP through the same call.
text.set_target(&device, surface_format);
text.set_transform(&queue, Text::pixel_ortho(width, height));

// 5. Draw into a render pass you own. Size and color enter here, not at shape
//    time; `draw_batch` takes a `&[Draw]` for many blocks in one go.
text.draw(&device, &queue, &mut pass, block, Vec2::new(40.0, 80.0), 32.0,
    Color([0.10, 0.11, 0.13, 1.0]), None);
```

## Examples

```
cargo run --example hello_png      # one line of text             -> hello.png
cargo run --example paragraph      # wrapping, alignment, sizes   -> paragraph.png
cargo run --example unicode        # color emoji + CJK + 12 scripts via fallback -> unicode.png
cargo run --example unicode_zoom   # interactive: a zoomable map of the whole codespace
cargo run --example emoji_zoom     # interactive: a zoomable board of every RGI emoji
```

The first three are headless (render to a PNG). `unicode_zoom` opens a window: a
Unifont-style 256-column map of the entire Unicode codespace — code point =
`row*256 + col`, a glyph where some font covers it and a tofu box where none does,
block labels down the side. It never enumerates up front; each frame culls to the
visible cells, skips glyphs below a minimum size (labels only), and stays
razor-sharp at any zoom. Scroll to zoom, drag to pan, `R` to reset, `Esc` to quit;
`-- --dump` writes PNG stills instead of opening a window. (Color emoji, a raster
atlas, is the one thing that pixelates when magnified.)

## Status

Extracted from a shipping infinite-canvas app, where it renders live editable
text across a zooming viewport. The crate-root re-exports (the `Text` service and
its handles) are the stable surface; the pipeline internals may change. Not yet
published to crates.io —
depend on it via git.

## Pipeline

```
font bytes ──► itemize + shape ──► flow into lines ──► outlines ──► bands ──► GlyphCache ──► TextAtlas (GPU)
   font.rs      layout.rs           flow.rs             outline.rs   bands.rs    cache.rs       renderer.rs
  (rustybuzz)                                                                                        │
   shape() caches a block ──► draw() / draw_batch() emit TextVertex ──► shader ◄─────────────────────┘
            text.rs                       vertex.rs                  shaders/*.wgsl
```

`Text` is the surface; everything else is internal. `shape()` caches a block's
em-space layout, `measure()` and the caret/selection queries read it without
touching a device, and `draw()` (or `draw_batch()`) rasterizes any newly needed
glyphs into the atlas and records quads into your pass.

| Module | Role |
|---|---|
| `text` | Public API (re-exported at the crate root): the `Text` service, handles, shaping, caret/selection geometry, draw |
| `font` | Face loading + fallback chain; metrics |
| `layout` | Itemization (script/face runs) and shaping into positioned glyphs |
| `flow` | Line breaking over shaped advances; caret stops. Em-space, so reflow never reshapes |
| `outline` | Glyph outlines as quadratic Bézier contours |
| `bands` | Band division + curve sorting → per-glyph `BandData` (Slug layout) |
| `cache` | Packs `BandData` into the shared curve/band atlas; assigns `GlyphInfo` |
| `renderer` | wgpu pipelines, atlas textures, incremental upload, draw |
| `vertex` | Vertex format + quad generation |
| `emoji` | Color-glyph rasterization + atlas |
| `emoji_presentation` | Generated `Emoji_Presentation` ranges — which code points default to color |
| `shaders/` | WGSL: Slug coverage (`pixel.wgsl`), quad transform (`vertex.wgsl`), emoji |

## The Slug atlas invariant

Two textures, both **4096 texels wide**:

- **Curve atlas** (RGBA16F): control points, two per texel, shared along contours.
- **Band atlas** (RG16U): per glyph, band **headers** `(count, offset)` then each
  band's list of **curve-locs** `(col, row)`.

The pixel shader indexes the band atlas *without row-wrapping* for the header
block and for the per-band curve-loc loop — it only wraps in two places:
`calc_band_loc` (header → its curve-loc list) and `fetch_curve` (sequential
curve texels). This mirrors Lengyel's reference shader exactly and keeps the hot
loops branch-free.

That makes a **layout invariant** the packer must uphold, not the shader:

> A glyph's header block, and each band's curve-loc list, must each stay within a
> single texture row (never straddle a multiple of 4096). Curves may straddle —
> `fetch_curve` wraps.

`cache::alloc_bands` enforces it by padding to the next row before any run that
would cross the boundary (as Slug's font compiler does), recomputing header
offsets to match. Break this and glyphs intermittently lose a band or render
scrambled, depending on where they land in the atlas — a bug that rotates between
letters as the atlas fills. Covered by `cache::tests::band_runs_never_straddle_a_texture_row`.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. The Slug algorithm and reference
shaders were dedicated to the public domain by Eric Lengyel.

## Reference

- Eric Lengyel, *GPU-Centered Font Rendering Directly from Glyph Outlines* (2017).
- Reference shaders (public domain): <https://github.com/EricLengyel/Slug>

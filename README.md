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

<sub>License: MIT OR Apache-2.0 · wgpu 29</sub>

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
use sanscale::{TextArgs, TextEngine, TextRenderer};

// 1. Load a font (from a path, bytes, or an ordered fallback chain).
let mut engine = TextEngine::load("/path/to/font.ttf")?;

// 2. Create the pipeline and a GPU glyph atlas.
let renderer = TextRenderer::new(&device, &surface_config);
let mut atlas = engine.new_atlas(&device, &queue, &renderer.atlas_layout);

// 3. Each frame: queue text, upload any newly-cached glyphs, flush vertices.
let args = TextArgs { size_px: 32.0, ..Default::default() };
engine.text(40.0, 80.0, "Hello, sanscale!", &args);      // pixel baseline
engine.sync_atlas(&mut atlas, &device, &queue, &renderer.atlas_layout);
let vertices = engine.flush();

// 4. Draw with a pixel-space orthographic matrix (0,0 = top-left).
let buffer = TextRenderer::build_vertices(&device, vertices);
renderer.render(&queue, &mut encoder, &view, &atlas, &buffer,
    vertices.len() as u32, ortho, (width, height), Some(wgpu::Color::WHITE));
```

## Examples

```
cargo run --example hello_png      # one line of text             -> hello.png
cargo run --example paragraph      # wrapping, alignment, sizes   -> paragraph.png
cargo run --example unicode        # color emoji + CJK + 12 scripts via fallback -> unicode.png
cargo run --example unicode_zoom   # interactive: pan/zoom the whole of Unicode
```

The first three are headless (render to a PNG). `unicode_zoom` opens a window —
scroll to zoom, drag to pan, `R` to reset, `Esc` to quit — and shows the payoff of
analytic coverage: the vector text stays razor-sharp at any zoom (color emoji,
being a raster atlas, is the one thing that pixelates).

## Status

Extracted from a shipping infinite-canvas app, where it renders live editable
text across a zooming viewport. The public API in `engine` is the stable
surface; the pipeline internals may change. Not yet published to crates.io —
depend on it via git.

## Pipeline

```
font bytes ──► shape (rustybuzz) ──► outlines ──► bands ──► GlyphCache ──► TextAtlas (GPU)
   FontSet        layout.rs           outline.rs   bands.rs    cache.rs        renderer.rs
                                                                                   │
   text() / layout() build PushedGlyphs ──► flush() emits TextVertex ──► shader ◄──┘
                         engine.rs                    vertex.rs        shaders/*.wgsl
```

The engine is the surface; everything else is internal. `TextEngine::text*`
queues glyphs, `flush()` returns vertices, and `sync_atlas()` uploads any newly
cached glyphs before drawing.

| Module | Role |
|---|---|
| `engine` | Public API: layout, wrapping, caret/selection geometry, per-frame glyph buffer |
| `font` | Face loading + fallback chain; metrics |
| `layout` | Itemization (script/face runs) and shaping into positioned glyphs |
| `outline` | Glyph outlines as quadratic Bézier contours |
| `bands` | Band division + curve sorting → per-glyph `BandData` (Slug layout) |
| `cache` | Packs `BandData` into the shared curve/band atlas; assigns `GlyphInfo` |
| `renderer` | wgpu pipelines, atlas textures, incremental upload, draw |
| `vertex` | Vertex format + quad generation |
| `emoji` | Color-glyph rasterization + atlas |
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

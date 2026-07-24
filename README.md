# text

GPU font renderer based on the **Slug** algorithm (Lengyel, 2017): quadratic
Bézier outlines rendered with per-pixel analytic coverage, resolution-independent,
no glyph rasterization. Loads TTF/OTF, shapes with rustybuzz, draws through wgpu.
Color glyphs (emoji) fall back to a rasterized atlas.

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

## Modules

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

## Reference

- Eric Lengyel, *GPU-Centered Font Rendering Directly from Glyph Outlines* (2017).
- Reference shaders (public domain): <https://github.com/EricLengyel/Slug>

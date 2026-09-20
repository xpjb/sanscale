# sanscale

**Resolution-independent GPU text rendering for [wgpu](https://wgpu.rs).**

sanscale draws monochrome glyphs directly from their quadratic Bézier outlines
with Eric Lengyel's [Slug](https://sluglibrary.com) algorithm. Analytic
per-pixel coverage replaces bitmaps and signed-distance fields, so text stays
sharp at any zoom. Color emoji use a separate raster atlas.

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
- **Consumer-owned batches** — `prepare` many blocks into one buffer *you* hold;
  unchanged content re-uploads nothing, and the fire-and-forget `draw` path is
  the same machinery plus a drop.
- **Real shaping** via [rustybuzz](https://crates.io/crates/rustybuzz), script
  itemization, and multi-font **fallback chains**.
- **Color emoji** (COLR v0/v1 and PNG-backed CBDT/sbix) through a rasterized side atlas.
- **Inline styles**: real font-chain spans plus pooled foreground paint; colors
  do not split shaping or invalidate layout.
- **Layout**: line wrapping, left/center/right alignment, multi-paragraph runs.
- **Editor geometry**: measurement, hit-testing, selection rectangles, and a
  typed caret with library-resolved motions (`caret_move`: cluster-true
  steps, visual-line Home/End, goal-column verticals, wrap affinity handled).

## Quick start

```rust
use sanscale::{Align, BlockKey, Color, ParagraphKey, Paragraphs, Style, TextService, Vec2};

// 1. One service holds every pool, every cache, and (lazily) the GPU resources.
//    You hold `Copy` handles into it.
let mut text = TextService::new();
let font = text.map_font(sanscale::read_font_file("/path/to/font.ttf")?, 0)?;
let chain = text.register_chain(&[font]);   // ordered fallback chain

// 2. Shape a block. A style carries no pixel size and no color, so the layout
//    cache is zoom-invariant; a paragraph key carries your own version, so an
//    edit reuses other paragraphs' cached shaping (block assembly still copies them).
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
text.set_transform(&queue, TextService::pixel_ortho(width, height));

// 5. Draw into a render pass you own. Size and color enter here, not at shape
//    time; `draw_batch` takes a `&[Draw]` for many blocks in one go.
text.draw(&device, &queue, &mut pass, block, Vec2::new(40.0, 80.0), 32.0,
    Color([0.10, 0.11, 0.13, 1.0]), None);
```

### Font loading

Sanscale accepts shared font bytes rather than owning font discovery. This keeps
bundled fonts straightforward:

```rust
use std::sync::Arc;

let font = text.map_font(Arc::new(include_bytes!("Inter-Regular.ttf")), 0)?;
```

A normal desktop application will often want family-name lookup and system-font
fallback instead. Use [`fontdb`](https://crates.io/crates/fontdb) for discovery,
then pass the selected face bytes and face index to `TextService::map_font`.
The examples' [`font_chain`](examples/common/mod.rs) helper shows the complete
`fontdb` path, including system-font loading and fallback-chain construction.

On `wasm32-unknown-unknown`, `fontdb::load_system_fonts()` is a no-op because
browsers do not expose system font files. Bundle or fetch fonts and pass their
bytes to `map_font` instead.

## Examples

Run an example with `cargo run --example <name>`. All six open a window; append
`-- --dump` to render a preview frame without opening one.

| preview | example / focus |
|---|---|
| [![Hello](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/hello.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/hello.png) | [`hello`](examples/hello.rs)<br>Large moving Unicode text and a live FPS counter |
| [![Unicode](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/unicode.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/unicode.png) | [`unicode`](examples/unicode.rs)<br>Zoomable, lazily populated map of Unicode planes 0–2 |
| [![Emoji](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/emoji.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/emoji.png) | [`emoji`](examples/emoji.rs)<br>Every RGI emoji sequence, grouped like a picker |
| [![Editor](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/editor.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/editor.png) | [`editor`](examples/editor.rs)<br>Plain rope-backed notepad demonstrating caret, selection, hit-testing, and wrapping |
| [![Code editor](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/code-editor.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/code-editor.png) | [`code-editor`](examples/code-editor.rs)<br>C syntax colors, real bold/italic faces, and shared caret/selection geometry |
| [![Markdown editor](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/markdown-editor.png)](https://raw.githubusercontent.com/xpjb/sanscale/master/gallery/markdown-editor.png) | [`markdown-editor`](examples/markdown-editor.rs)<br>Split source/preview, custom incremental Markdown, streaming tables |

Regenerate every committed preview with `scripts/update-gallery.sh`; broad
Latin, CJK, Indic, symbol, and color-emoji system fonts are required.

`hello` is the smallest live application: two large CJK-and-emoji lines move by
changing only their draw positions, while a stable block displays live FPS. Its
paragraph generation changes only when the displayed value changes.

`unicode` opens a window: a
Unifont-style 256-column map of the entire Unicode codespace — code point =
`row*256 + col`, a glyph where some font covers it and a tofu box where none does,
block labels down the side. It never enumerates up front; each frame culls to the
visible cells, skips glyphs below a minimum size (labels only), and stays
razor-sharp at any zoom. Scroll to zoom, drag to pan, `R` to reset, `Esc` to quit;
`-- --dump` writes PNG stills instead of opening a window. (Color emoji, a raster
atlas, is the one thing that pixelates when magnified.)

`editor` remains the plain rope-backed notepad: no syntax highlighter or themed
font spans. It starts empty; pass a text file to open it.

`code-editor` is a separate **C editor** (`ropey` + native open/save dialogs), starting
with a sample C program. Pass a filename to edit your own source:

```sh
cargo run --release --example code-editor -- source.c
cargo run --release --example code-editor -- --dump    # headless code-editor.png
```

Keywords/types use a real bold face; comments use italic/oblique. `--font <family>`
selects a family; missing variants are reported and use regular rather than fake
weight/slant. One font database shares fallback faces across all three chains.
**F2** switches foreground palettes without lexing or shaping; **F3** toggles
comment italics without reparsing or changing file dirtiness. Ctrl+O/S open/save,
Ctrl+wheel zooms, wheel scrolls. Text batches survive caret blinks and selection
changes; hit-testing, wrapping, caret movement and rendering use one layout.

The example's line-state lexer handles C keywords, common typedef names, numbers,
quoted strings/chars, comments, directives and ordinary continued strings/comments.
It re-lexes from an edit until lexical state rejoins the cached suffix. It is **not**
a full C parser/preprocessor: no macro expansion, conditional-compilation analysis,
trigraphs/digraphs, or tokens/delimiters formed across spliced lines. Undo, IME and
bidi remain outside this example. Physical lines are LF; CRLF/CR input is normalized.

### Markdown editing and streaming preview

`markdown-editor` is a separate split-view example. Source editing stays on the
left; headings, real bold/italic/combined faces, links, code, lists and wrapped,
aligned tables render on the right. Click preview text to locate its source.
There is also a [streamed-message preview](gallery/markdown-editor-stream.png).

```sh
cargo run --release --example markdown-editor                 # or append -- file.md
cargo run --release --example markdown-editor -- --stream       # simulated agent reply
cargo run --release --example markdown-editor -- --dump         # no window
cargo run --release --example markdown-editor -- --dump --stream
```

**F2** changes the palette, **F3** toggles italic faces, **F4** reveals the source
block in the preview, **F5** appends/pauses a demo reply, **F6** toggles tail-follow.
Wheel scrolls each pane independently; Shift+wheel pans wide tables; Ctrl+wheel
zooms. Ctrl+O/S opens/saves Markdown. Typing pauses the demo stream. No networking
or automatic saving is involved.

The **custom parser/model and preview adapter live under the example**, not in the
core crate. They are separated from the editor/window so the same component can
later serve streaming agent messages. Table cells keep stable identities, body
updates do not auto-resize columns, and completed rows retain their layouts.
This is an experimental dialect, **not a full CommonMark/GFM implementation or a
published API**. HTML stays literal, images show alt text, links are passive.
See the [component contract, syntax, work bounds and tests](examples/markdown-editor/markdown/README.md)
for what's implemented and what is still broad work. The UI has no undo/IME or
unsaved-change confirmation. `--bench` runs a small CPU-only table diagnostic;
`--features perf-counters` with `--dump` additionally checks retained GPU frames.

## Inline font and foreground spans

Implement `ParagraphSource::paragraph_fonts(index, key) -> Cow<'_, [FontSpan]>`
when a paragraph has font overrides. The default returns no spans. `FontSpan`
ranges are paragraph-local UTF-8 bytes, sorted, nonoverlapping, nonempty, and
aligned to extended grapheme boundaries; invalid input makes `shape` return `None`.
Gaps use `Style.chain`. Bump `ParagraphKey::generation` for text **or effective font
span** changes, not foreground changes. Line metrics still come from the base style.

Foreground snapshots are independent, owned by the service:

```rust
use sanscale::{Color, Draw, PaintSpan, Vec2};

let paint = text.register_paint(&[
    PaintSpan { range: 0..3, color: Color([0.7, 0.3, 0.6, 1.0]) },
])?;
let draw = Draw {
    block,
    at: Vec2::new(14.0, 14.0),
    size: 17.0,
    color: Color([0.8, 0.8, 0.8, 1.0]), // gaps
    paint: Some(paint),
    ..Default::default()
};
let batch = text.prepare(&device, &queue, &[draw]);
// Record it via draw_prepared/draw_segment in your own pass.
text.drop_paint(paint); // batch already owns its baked colors
```

Paint ranges are **block-local bytes**, sorted, nonoverlapping and nonempty. The
color at a shaped cluster's starting byte colors the whole cluster; a boundary
inside a ligature does not split shaping. Emoji keep their native colors. Empty
snapshots are valid, but ordinary text should use `paint: None` (no registration).
Equal-content registrations are not interned: preserve a handle for unchanged
resolved spans. The pool has 131,072 explicitly owned slots, returns `PaintError`
on invalid ranges/exhaustion, and uses generations so stale handles cannot alias
recycled paint. A stale paint skips future preparation of that item; existing
batches do not borrow the snapshot.

`Draw` stays `Copy`. Its default is an invalid/no-op block, position (0,0), size 1,
opaque black, and no paint/clip. It never allocates a resource. Existing `draw(...)`
convenience signatures stay unchanged. Consumers retaining batches compare draw
inputs themselves; `batch_live()` tracks service-owned layout/atlas changes.

**Current granularity:** a tiny font-span edit reshapes/reflows its whole paragraph,
then reassembles the entire block. A recolor rebuilds affected geometry, not shaping
or flow. The code editor rebuilds/diffs its flat block-local paint ranges on text/palette
changes, so an edit still visits all paint tokens. These deliberate costs and the
path to finer reuse are recorded in [the style note](rfc-inline-styles.md).

The font-backed tests require real system faces (DejaVu Sans and DejaVu Sans Mono
with bold/oblique variants are the Linux fixtures). Run the code example's lexer/editor
tests explicitly too: `cargo nextest run --example code-editor`, or the ordinary
`cargo test --example code-editor` equivalent outside managed environments.

## Performance regression suite

The headless [pathological suite](performance.md) separates uninstrumented latency
from opt-in work/allocation counters. It covers thousands of separate Unicode
blocks versus grouped rows, long paragraphs and paragraph groups, edits and cache
hits, clipping, batch retention, atlas uploads, and emoji raster buckets.

```sh
scripts/run-perf.sh perf-results/before --tier quick --gpu --samples 21 --warmup 3
```

This writes raw JSON and a standalone HTML dashboard. Capture again after a change
and use `scripts/perf-report.py` for a comparison; exact fonts, corpora, compiler,
and GPU details are recorded. See [the protocol and coverage matrix](performance.md)
for fixture requirements, stress tiers, implemented `*.spans.*` controls, and
remaining combinations.
The `perf-counters` feature exposes `sanscale::profiling` only for investigations;
its probes compile out of normal builds, and its timings are not production timings.

## Status

Extracted from a shipping infinite-canvas app, where it renders live editable
text across a zooming viewport. The crate-root re-exports (`TextService` and
its handles) are the stable surface; the pipeline internals may change. Not yet
published to crates.io —
depend on it via git.

## Pipeline

```
font bytes ──► itemize + shape ──► flow into lines ──► outlines ──► bands ──► GlyphCache ──► TextAtlas (GPU)
   font.rs      layout.rs           flow.rs             outline.rs   bands.rs    cache.rs       renderer.rs
  (rustybuzz)                                                                                        │
   shape() caches a block ──► prepare() builds a Batch ──► draw_prepared() ──► shader ◄──────────────┘
            text.rs             (draw/draw_batch = sugar)      vertex.rs    shaders/*.wgsl
```

`TextService` is the surface; everything else is internal. `shape()` caches a block's
em-space layout, `measure()` and the caret/selection queries read it without
touching a device, and `prepare()` rasterizes any newly needed glyphs into the
atlas and concatenates blocks into a `Batch` — one vertex buffer you own, drawn
segment-by-segment into your pass (`draw()`/`draw_batch()` are the transient
sugar: prepare, draw, drop).

| Module | Role |
|---|---|
| `text` | Public API (re-exported at the crate root): the `TextService`, handles, shaping, caret/selection geometry, draw |
| `spans` | Inline font inputs, generational immutable paint pool, range validation and color lookup |
| `font` | Face loading + fallback chain; metrics |
| `layout` | Itemization (script/face runs) and shaping into positioned glyphs |
| `flow` | Line breaking over shaped advances and caret stops; the current combined cache still reshapes on an uncached width |
| `outline` | Glyph outlines as quadratic Bézier contours |
| `bands` | Band division + curve sorting → per-glyph `BandData` (Slug layout) |
| `cache` | Packs `BandData` into the shared curve/band atlas; assigns `GlyphInfo` |
| `renderer` | wgpu pipelines, atlas textures, incremental upload, draw |
| `vertex` | Vertex format + quad generation |
| `emoji` | Color-glyph rasterization + atlas |
| `emoji_presentation` | Generated `Emoji_Presentation` ranges — which code points default to color |
| `work` / `profiling` | Compile-out work probes; `profiling` is public only with the opt-in `perf-counters` feature |
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

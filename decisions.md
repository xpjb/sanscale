# sanscale API redesign — decisions

Working notes from the "dome over the internals" redesign discussion. The internal
machinery (Slug pipeline, glyph/emoji caches, eviction, rasterization) is **not**
changing — this is a public-surface rework. Pressure-tested against the only real
consumer, `compendium` (which aliases the crate as `text`), plus our own examples.

Mental model: **there is one `Text` service that holds ~7 keyed pools with eviction.**
Only the atlas is hard, and it's already built. Everything else is a HashMap + a version.

Pools:
1. **Fonts** (`FontHandle`) — one mapped concrete font each, deduped. key: font-bytes identity. via `map_font`.
2. **Chains** (`FontChainHandle`) — an ordered list of fonts (the fallback order). via `register_chain`.
3. **Text glyph atlas** — Slug band/curve data (rasterized pixels). key: `(face_id, glyph_id)`. *(exists)*
4. **Emoji atlas** — rasterized color glyphs (pixels). key: `(face_id, glyph_id, bucket)`. *(exists, bounded+evicting as of this week)*
5. **Run shaping** *(Level 1)* — one itemized run's glyph sequence + advances, em-space. key: `(face, style, run-text)`. *(proposed; today shaping is only cached per-paragraph)*
6. **Paragraph layout** *(Level 2)* — ordered run-refs + line flow for a paragraph. key: `(ParagraphKey, wrap_em, align)`. This is what a `ShapedHandle` points at.
7. **Geometry** *(optional/deferred)* — per-key GPU vertex buffer. key: `(ParagraphKey, raster generation)`.

Note pools 3/4 (rasterization: glyph → pixels) are distinct from pool 5 (shaping:
text → glyph sequence). Different arrows in the pipeline, different keys.

The pipeline, each arrow's output a cacheable pool:
`text → [itemize] runs → [shape] glyph-runs (5) → [line-flow] lines (6) → [rasterize] atlas (3/4) → [resolve] quads (7)`

---

# API sketch (current)

Reflects the locks below. wgpu is borrowed only at `draw` — the "optional wgpu" seam — so
`shape`/`measure` run with no GPU. The `Text` body shows the pools; `Slab`/`Atlas`/`RunShaping`
etc. are illustrative containers. Supporting value types (`Align`, `FontSource`, `Color`,
`Layout`) elided.

```rust
struct Text {
    // the three pools the user's handles index into (a handle IS its slab index):
    fonts:      Slab<Font>,        // FontHandle       — pool 1; deduped by font bytes
    chains:     Slab<Chain>,       // FontChainHandle  — pool 2; each a Vec<FontHandle>
    paragraphs: Slab<Paragraph>,   // ShapedHandle     — pool 6; Level-2 layout (run-refs + lines, em)

    // internally managed — no user handle (all evict LRU):
    shape_lookup: HashMap<(ParagraphKey, Style), ShapedHandle>, // shape() hit → existing slot
    runs:         HashMap<RunKey, RunShaping>,       // pool 5; Level-1, content-keyed (font + run text)
    text_atlas:   Atlas,                             // pool 3; (font, glyph) → Slug pixels      (CPU)
    emoji_atlas:  Atlas,                             // pool 4; (font, glyph, bucket) → RGBA      (CPU)
    geometry:     HashMap<ParagraphKey, VertexBuf>,  // pool 7 (optional) — vertex buffers  (GPU, lazy)
    gpu:          Option<GpuResources>,              // pipelines, format, atlas textures   (GPU, lazy)
}

// handles — all Copy, indices into the pools
#[derive(Clone, Copy)] struct FontHandle(u32);       // one mapped concrete font
#[derive(Clone, Copy)] struct FontChainHandle(u16);  // ordered fallback list of fonts
#[derive(Clone, Copy)] struct ShapedHandle(u32);     // a shaped paragraph (pool 6)

// cousin of the handles: Copy, but consumer-minted (your node id + version), not a pool index
#[derive(Clone, Copy)] struct ParagraphKey { id: u64, version: u64 }
struct Style { chain: FontChainHandle, wrap_em: Option<f32>, align: Align }  // zero px, no color

impl Text {
    // atlas self-bounds to the device limit at first draw (width is fixed 2048 internally,
    // so there's nothing to configure). format is Copy; no device yet. Add a single
    // `max_height: u32` knob later only if memory tuning ever needs it.
    fn new(format: TextureFormat) -> Self;

    // discovery is the consumer's (fontdb → bytes). map dedups; "map" leaves mmap open.
    fn map_font(&mut self, src: FontSource) -> FontHandle;
    fn register_chain(&mut self, fonts: Vec<FontHandle>) -> FontChainHandle;  // stored as-is

    // shape + cache (key, style); `fetch` runs only on a miss; GPU-free
    fn shape(&mut self, key: ParagraphKey, style: &Style, fetch: impl FnOnce() -> Cow<str>) -> ShapedHandle;

    // em-space queries off the same shaping: box size, hit-test, carets; GPU-free
    fn measure(&self, run: ShapedHandle) -> &Layout;

    // wgpu passed in directly (no bundle); size_px scales em→px and picks the raster bucket;
    // transform is a column-major 4x4 (raw, no Camera type); shape/measure don't take wgpu,
    // so leaving draw uncalled = device-free
    fn draw(&mut self, device: &Device, queue: &Queue, pass: &mut RenderPass,
            run: ShapedHandle, at: Vec2, size_px: f32, color: Color,
            transform: [f32; 16], scissor: Rect);
}
```

---

# Locked

Things we're certain about, and why.

- **The surface is one object plus handles.** A single `Text` service owns every pool and
  the GPU resources. Everything else is `Copy` handles into it — `FontHandle`,
  `FontChainHandle`, `ShapedHandle` — and value types — `ParagraphKey`, `Style`. Methods: `new`,
  `map_font`, `register_chain`, `shape`, `measure`, `draw`. wgpu is borrowed only at `draw`.
  No per-frame object.

- **The service is a glyph-quad source, not a compositor — it never owns the render pass.**
  The consumer builds the pass (target, clear/load, z-sorted interleave with its own
  rects/strokes/tiles, per-pane scissor) and passes it in; the service records glyph-quad
  draws into it. Grounded: compendium runs one pass per pane + a chrome pass over a z-sorted
  `(Prim, Range)` draw plan. This invariant is what makes the consumer's GPU orchestration
  allowable. *(How exactly `draw` receives position/transform — world+camera vs screen-space
  + pixel ortho — is a mechanics question, in litigation.)*

- **Shaping is em-space; `size_px` is a draw-time scalar only.** `Style` carries zero px, so
  the shaping cache is zoom-invariant. `size_px` enters once, at `draw`: it scales em→px and
  selects the atlas raster bucket. (Slug is analytic — no hinting/rounding — so this is exact.)

- **Two levels of shaping, both internal; the consumer's handle is the paragraph.**
  `text → [itemize] runs → [shape] glyph-runs (Level 1) → [line-flow] lines (Level 2)`. Level 1
  (run shaping) is local — a run's glyphs depend only on that run's text+attrs, independent of
  wrap/size/other runs. Level 2 (paragraph flow) is global — it breaks and positions all runs
  across lines ("flows to the rest of the paragraph"). The consumer holds a **paragraph**
  handle pointing at *1..N shaped runs + the flow*; it never sees a run. Reinforces: consumer
  owns paragraph identity, runs live below the handle.

- **Three tiers of cost, cleanly separated** (falls out of the two levels + em-space):
  - **Camera zoom** re-runs nothing — it's the draw scalar.
  - **Reflow** (`wrap_em` change) re-runs Level 2 only — same glyphs, new line breaks; no reshape.
  - **Edit** (`version` bump) re-runs Level 2 and reshapes only the touched run(s).

- **Color is a draw parameter, not a shape/geometry input.** Color doesn't move a glyph, so
  baking it (today's `TextVertex.col`) makes the geometry cache color-specific for nothing.
  Same shaped+rasterized run redraws in any color free (selection highlight, theme swap).

- **Wrap is em, and it's the consumer's one conversion.** `wrap_em = pane_px / font_px`,
  done once by the consumer. It's the honest resolution-independent form, and it's where the
  two "zooms" correctly diverge (camera-zoom keeps `wrap_em`, pane-resize changes it).

- **Discovery is the consumer's; fallback is ours.** "Font chain" is two things:
  *sourcing* (family name → font bytes, via fontdb — platform enumeration, outsourced and
  must be) and *the fallback walk* (chain → per-grapheme font → itemized runs — shaping,
  ours and can't be outsourced). `map_font(FontSource) -> Font` then
  `register_chain(&[Font]) -> FontChain` is the seam: resolved bytes in, chain-as-shaping-
  construct out. Grounded: examples' `font_chain` (fontdb) + `layout.rs::itemize` /
  `face_for_grapheme`.

- **The service itemizes a paragraph into per-font runs; the consumer never breaks by font.**
  `"Hi 日本 😀"` is one `shape()` with one chain; the service splits it. Grounded:
  `itemize`, `glyph.face_id`, `itemize_splits_latin_and_cjk_runs`.

- **Fonts are deduped across chains → shared glyph cache.** Today two engines load
  "Segoe UI Emoji" twice into two atlases (emoji rasterized twice). A global font pool +
  chains-as-index-lists means UI and Mono share the one emoji font, cached once. This is the
  payoff that justifies collapsing compendium's two engines into one service. New machinery
  required: `map_font` dedup key on font-bytes identity.

- **The consumer supplies identity; the service never mints paragraph handles from a blob.**
  `ParagraphKey { id, version }`. One `shape()` = one unit = one handle. The service breaks
  internally (font, line) but never returns a variable number of handles. Reason: the
  consumer already owns document structure; service-minted handles force a sync problem.
  Selection/marquee is *easier* this way — `measure` gives per-unit geometry, the consumer
  stitches across its own vertical stack.

- **Break layering:** consumer breaks by semantics (+ style, for rich text) → gets handles →
  service breaks each by font + line. Three axes, clear owners.

- **The shaping cache key is `(ParagraphKey, Style)`, px-free; `fetch` runs only on a miss.** For a
  document where 99% of visible paragraphs are unchanged, their text is never materialized —
  the closure is the escape hatch that says "give me the chars only if I'm actually
  reshaping." This is the un-boxed form of today's `TextParagraphProvider`.

- **`measure` and `draw` read the same `ShapedHandle` → no hit-test/render drift.** A single
  `Copy` handle feeds both. Baseline/ascent baked inside; `at` = top-left of the run box.
  Kills the class of bug where compendium's manual `pos.y + ascent*size_px` (6 sites)
  drifted between measure and draw.

- **`draw` does not center or measure for you.** Horizontal align-within-wrap is `Style.align`
  (shape-time). Centering a box in an external region is consumer arithmetic off `measure`.
  Vertical baseline is internal. Three "centerings", three homes; `draw` stays dumb.

---

# Cutting

Things being removed from today's API. As definite as the locks.

- **The two engines → one service.** UI and Mono become two registered `FontChainHandle`s sharing
  one atlas. Removes 4 atlases → ≤2 and the duplicate emoji rasterization.
- **The `text_*` / `layout_*` method family (14 methods) → `shape` + `measure` + `draw`.**
  `clip`, paragraph-mode, and face stop being method-name axes.
- **The 4 GPU types** (`TextRenderer`, `EmojiRenderer`, `TextAtlas`, `EmojiAtlas`) → internal.
- **Public `TextVertex` / `EmojiVertex` and their fields** (`bnd`, `col`, `glyph`, `loc_em`)
  → geometry is opaque; the consumer never touches a vertex.
- **`curve_atlas_size` / `band_atlas_size`** (Slug internals) → gone (or a `diagnostics()` accessor).
- **The three nested identity structs** (`ParagraphIdentity`, `TextParagraphIdentity`,
  `TextParagraphCacheKey`) → one flat `ParagraphKey { id, version }`.
- **`TextParagraphProvider`** (a boxed trait threaded through the app) → a `fetch` closure.
- **`emoji_epoch`** → gone by structure: it only existed to invalidate caller-held atlas-baked
  geometry; the service owns the geometry pool and invalidates on eviction internally. compendium
  never used it (zero grep hits), so nothing to migrate.
- **`set_emoji_atlas_max_height`** (runtime setter) → gone; the atlas self-bounds to the
  device limit at first draw (width fixed 2048), so `new` takes no budget at all.
- **Color baked into vertices, and `size_px` inside shaping** → both move to `draw`.
- **`is_single_glyph` / `glyph_bbox` / `family_for`** (example-only escape hatches leaked into
  the API) → a `diagnostics()` accessor, or inlined into the examples.

---

# Maybe

Things we were pretty confident on, phrased as if we'd do them — but might need to cook harder.

- **Content-key the run-shaping pool (Level 1) → incremental across runs, for free.** *We'd
  key run shaping by `(face, style, run-text)`* so that after an edit (which re-itemizes and
  re-flows the paragraph), every *unchanged* run's text hashes the same and hits the Level-1
  cache — only the touched run(s) actually re-shape. Incremental becomes emergent, not a mode.
  *But maybe* the win is marginal for Latin (shaping is cheap there) and it adds a pool with
  its own LRU; worth it mainly for complex scripts where shaping dominates. Note this does
  **not** help the single-giant-run degenerate case — see litigation.

- **Geometry pool (service-owned vertex buffers, per key).** *We'd add it* because it kills
  the every-frame re-upload of unchanged paragraphs (battery), removes the four-buffer
  grow-management boilerplate compendium hand-rolls, and — because `draw` is keyed — it slots
  in later with **no API change**. *But maybe not*: compendium re-uploads everything every
  frame today and it's fine, so this is designed-in-shape, deferred-in-fact. Ship without it,
  add transparently when power actually matters.

- **`ShapedHandle` as a distinct `Copy` token vs just `draw(ParagraphKey, …)`.** *We'd keep it* to
  skip re-hashing the key on measure/draw. *But maybe* collapse to draw-by-`ParagraphKey` (one extra
  hashmap lookup per draw, probably nothing). A dial, not a principle.

- **Inline style-runs for rich text / markdown.** *We'd support it* as consumer-supplied
  styled sub-runs within a paragraph (consumer breaks by style, service itemizes each by
  font). *But maybe* it reshapes the "one Key = one paragraph" story — today's `Style` is
  uniform-per-run. A markdown renderer makes this real, so it's likely in scope but undesigned.

- **`FontError` enum replacing `Result<_, String>`.** *We'd do it* — stringly errors are
  wrong for a published crate. *But* exact variants (Io / Parse / NoCoverage / …) TBD.

---

# Up for litigation

Genuinely open.

- **How wgpu enters — leaning: borrow it at `draw`, not an abstract backend trait.** This
  subsumes two questions that turned out to be one: "one object or two" and "what is the
  pass/`draw` really doing." An abstract-GPU trait (a "MaybeWGPU" à la the `log` facade) isn't
  worth it — wgpu is huge, moving, and already *is* the abstraction over Vulkan/Metal/DX;
  abstracting it is abstracting an abstraction. Lean: **borrow live wgpu handles only at
  `draw`** (the `Wgpu<'_>` bundle in the sketch). Device-free `measure` then falls out for free
  — `shape`/`measure` simply don't take it, so there's no second object and no no-op backend to
  build. Still open: the exact `Wgpu<'_>` shape, and — same plumbing — how `draw` gets
  position/transform (world + `camera` vs screen-space `at` + pixel ortho, which is what
  compendium emits today via `world_to_screen` + `slug_pixel_matrix`; sets how `size_px`/bucket
  derives on-screen px).

- **Font ingestion: owned bytes vs mmap.** Today it's `FontSource::Bytes(Vec<u8>)` — a full read
  into a resident copy per face. Fat CJK/emoji fonts are 20–40 MB; production renderers **mmap**
  and let the OS page in glyphs lazily. The "here are some bytes" seam is clean but forces the
  resident copy and blocks mmap. Consider a source that can borrow/mmap (`&'static [u8]`, or an
  `Arc<dyn AsRef<[u8]>>`, or a memmap handle the parser face borrows). Interacts with the
  `map_font` dedup key (dedup by path when mmapping vs by bytes-hash when owning).

- **Handle granularity for markdown blocks.** Heading / paragraph / list-item / code-block
  each their own `ParagraphKey`? Probably yes (independent caching + editing), but selection across
  heterogeneous blocks and the cross-block geometry stitching is unproven.

- **Incremental *within a single run* (the actually-hard part).** The two-level split +
  content-keyed run pool (see Maybe) already gives incremental *across* runs for free. What it
  does **not** solve: a degenerate paragraph that is one font, one script → **one run** → any
  edit re-shapes the whole run (50k chars). True incrementality there needs **intra-run
  chunking** — shape a run in segments, re-shape only the touched segment — which is hard
  because ligatures / contextual forms / cursive joins can straddle a chunk boundary (you
  can't cut an `ffi` ligature or Arabic joining in half). This is precisely what compendium's
  "degenerately long paragraph" note was about. Constraint today: keep the run's shaped value
  segment-structured, not an opaque blob, so a future chunked path can patch a slice. Building
  it is deferred and genuinely open.

- **Per-`ShapedHandle` draw calls vs coalescing.** Leaning "per-handle is fine" (compendium already
  issues per-item draws with per-item scissors; the single coalesced buffer only ever saved
  *uploads*, which the geometry pool saves better). Unverified against a heavy dense-canvas
  frame — worth measuring the draw-call count before building on it.

- **`map_font` dedup key.** Bytes hash vs `(source path, face index)`. Correctness vs cost,
  and tied to the mmap-vs-owned decision above.

- **Text (band/curve) atlas eviction.** Still observe-only from the emoji work — unbounded,
  warns at the device limit, no eviction. Whether it graduates to real eviction (harder: the
  runs are variable-length with a row-alignment invariant, so the emoji slab trick doesn't
  map) or stays observe-only is deferred.

---

# Backlog

Decided-for-now, with a known fancier version later that does **not** change the API — so not
worth litigating, just parked.

- **Line breaking: greedy now, Knuth-Plass / justification later.** Reflow is cheap precisely
  because it's greedy first-fit over already-shaped glyph advances — no reshape. A future
  optimal-breaking or justified pass is a pure swap of the Level-2 flow step; same inputs
  (glyph runs + `wrap_em`), same outputs (lines), no surface change.

- **Chunked (intra-run) shaping.** The *build* is parked here; its one live constraint —
  keeping a run's shaped value segment-structured, not an opaque blob — stays in litigation so
  we don't paint over it now.

- **IME preedit + bidi.** Neither exists in compendium today (no composition path; LTR-only
  carets), so parked. When added, both extend the *shaping/measure* model, not draw: a transient
  IME preedit folds into the shaped body as a revision (not a separate overlay a read-only
  `Layout` shows); bidi needs `CaretHit`/carets to carry direction/level.

- **Text editor as an example.** Dogfood the caret/selection/hit-test surface — the hardest
  consumer path — as a `sanscale` example, not just an app feature buried in compendium. `>:)`

---

# Implications for consumers

## compendium (the real one)

Removed / collapsed:
- **Two engines → one service, two registered `FontChainHandle`s** (UI, Mono) sharing one atlas.
  4 atlases → 2 (or fewer), 4 hand-grown vertex buffers → service-owned.
- **The ~110-line UI-vs-Mono match** collapses — face is a parameter, not an engine instance.
- **Three nested identity structs** (`TextParagraphCacheKey` / `ParagraphIdentity` /
  `TextParagraphIdentity`) + **`BoxParagraphProvider`** collapse to `ParagraphKey { id, version }` +
  a `fetch` closure. The awkward double-boxed provider shim goes away.
- **Manual buffer grow-management** (`slug_text_buf` recreate ×4) and the fragile
  **`emoji_len()`-snapshot range bookkeeping** — gone (draw-by-handle / service-owned geometry).
- **The `scale_pixels` zoom fast path** becomes unnecessary — em-native shaping means zoom is
  always just the draw scalar.
- **`measure` drops size from its cache key** — `measure_body_content_height` /
  `body_content_h` cache keys on content + wrap only, scales the scalar for any zoom.
- **The 8 `pos.y + ascent*size_px` sites** — removed; baseline is baked into `draw`.
- **`emoji_epoch`** — already unused; simply gone, no migration.

Editor (`text_editor.rs`) — verified the caret/selection/hit-test/motion path survives and
gets simpler (all of it is read-only queries over one body `Layout`):
- **The editor's own layout cache goes.** `EditorLayoutKey.size_px_bits`, the `scale_pixels`
  zoom fast-path, and `desired_x *= scale` exist *only* to fake em-invariance today; with size
  applied at `draw` they're deleted. The owned `layout`/`layout_key` become a borrow of the
  service-cached `&Layout`. `TextLayout::scale_pixels` can go with them.
- **The hit-vs-render dual layout collapses onto one keyed handle** — killing the drift-bug
  class flagged at `editing.rs:757`, because hit-test and render read the same `ShapedHandle`.
- **Body stays one handle.** The active editor is one `Layout` over all its paragraphs, so
  caret motion/selection across paragraph boundaries needs no cross-handle stitching.
- Mechanical changes: caret/selection `_px` → em (renderer multiplies by `size_px` instead of
  dividing by zoom); `desired_x` stored em. Nothing in the current feature set breaks. Pin an
  explicit test on **word-wrap caret affinity** (the one feature coupled to `Layout` internals;
  survives via `CaretHit{byte,line}` + `caret_line_hint`).

Still compendium's job (unchanged):
- fontdb discovery (name → bytes).
- Paragraph identities (`id` + `version`) from its document pools.
- The pass / compositor: z-sort, per-pane scissor, panes, interleave with its own geometry.
- World ↔ screen transforms.
- Its own paint-tile residency cache (nothing to do with text).

## examples

- `emoji_zoom` / `unicode_zoom`: the per-row vertex cache + the `emoji_epoch` invalidation we
  just added get replaced by draw-by-handle (or kept if they deliberately hold manual
  buffers). `font_chain` (discovery) stays.
- `is_single_glyph` / `glyph_bbox` / `family_for` were example-only escape hatches leaked
  into the public API — become a `diagnostics()` accessor or get inlined into the examples.
- `common/mod.rs` `Harness` save-PNG helpers retarget the new `draw`.

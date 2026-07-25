# sanscale API redesign — decisions

Working notes from the "dome over the internals" redesign discussion. The internal
machinery (Slug pipeline, glyph/emoji caches, eviction, rasterization) is **not**
changing — this is a public-surface rework. Pressure-tested against the only real
consumer, `compendium` (which aliases the crate as `text`), plus our own examples.

---

# Nouns

The vocabulary, because it has accreted and most of it is one-per-pool. Read this
first; everything below assumes it.

1. **Font** — one mapped concrete face, deduped by data identity. `FontHandle`.
2. **Chain** — an ordered fallback list of fonts. `FontChainHandle`. *Discovery*
   (family name → bytes) is the consumer's; the *fallback walk* (chain → per-grapheme
   face) is ours.
3. **Run** — a maximal span of text resolving to a single face; what `itemize`
   emits. Purely internal: the consumer never sees one, and never breaks text by
   font.
4. **Line** — what flow emits. One paragraph produces 1..N.
5. **Paragraph** — the unit of **invalidation**. The consumer owns its identity and
   version (`ParagraphKey`); it is flowed independently and cached at
   `(ParagraphKey, Style)`, which is why an edit reshapes one paragraph and not the
   document.
6. **Block** — the unit of **coordinate space**. 1..N paragraphs at one `Style`,
   concatenated — *not* reflowed — into one byte range, one line list, one `Layout`.
   `BlockKey` names it; `ShapedHandle` resolves it. The reason it exists is that its
   paragraphs want to be **measured together**: one hit-test, one caret space, a
   selection that spans paragraph boundaries. Sharing a `Style` (so they would
   reflow together if it changed) is a consequence, not the point.
7. **Glyph** — a rasterized atlas cell. Keyed `(face, glyph_id)` for text,
   `(face, glyph_id, bucket)` for emoji. Distinct from a *run*: rasterization and
   shaping are different arrows in the pipeline.
8. **Geometry** — one block's quads at a given set of draw parameters, cached
   host-side. Position- and scale-independent, so a camera move re-runs none of it.
9. **Batch** — *(proposed, see `rfc-batch-cache.md`)* the unit of **pass state**:
   blocks sharing a scissor and a contiguous z-slot, concatenated into one draw
   call. The only noun here that belongs to **rendering** rather than to text, which
   is why it is a slice of `Draw` and not a key.

Three of these are the load-bearing consumer-facing axes, and each has a different
owner — confusing them is how the design goes wrong:

| noun | unit of | owned by |
|---|---|---|
| paragraph | invalidation | consumer (identity + version) |
| block | coordinate space | consumer composes, service flows |
| batch | pass state | consumer (z-order, scissor) |

---

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
    fonts:  Slab<Font>,   // FontHandle       — pool 1; deduped by font bytes
    chains: Slab<Chain>,  // FontChainHandle  — pool 2; each a Vec<FontHandle>
    blocks: Slab<Block>,  // ShapedHandle     — a composed unit: ordered part-refs into
                          //   pool 6 + cumulative line offsets (the scroll index)

    // internally managed — no user handle (all evict LRU):
    block_lookup: HashMap<BlockKey, ShapedHandle>,               // shape() hit → existing slot
    paragraphs:   HashMap<(ParagraphKey, Style), ParaLayout>,    // pool 6; Level-2, per paragraph
    runs:         HashMap<RunKey, RunShaping>,       // pool 5; Level-1, content-keyed (font + run text)
    text_atlas:   Atlas,                             // pool 3; (font, glyph) → Slug pixels      (CPU)
    emoji_atlas:  Atlas,                             // pool 4; (font, glyph, bucket) → RGBA      (CPU)
    geometry:     HashMap<ParagraphKey, VertexBuf>,  // pool 7 (optional) — vertex buffers  (GPU, lazy)
    gpu:          Option<GpuResources>,              // pipeline per target format, atlas textures
}

// handles — all Copy, indices into the pools
#[derive(Clone, Copy)] struct FontHandle(u32);       // one mapped concrete font
#[derive(Clone, Copy)] struct FontChainHandle(u16);  // ordered fallback list of fonts
#[derive(Clone, Copy)] struct ShapedHandle(u32);     // a shaped *block* (1..N paragraphs)

// cousins of the handles: Copy, consumer-minted, not pool indices
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ParagraphKey { namespace: u64, slot: u32, generation: u32 }  // unit of invalidation
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct BlockKey(u64);                                              // unit of coordinate space

#[derive(Clone, Copy, PartialEq)]  // Eq/Hash via wrap_em.to_bits()
struct Style { chain: FontChainHandle, wrap_em: Option<f32>, align: Align, line_spacing: f32 }

impl Text {
    // takes nothing: pipelines are per-target and lazy (see set_target), and the atlas
    // self-bounds to the device limit at first draw (width is fixed 2048 internally).
    fn new() -> Self;   // lives beside the consumer's renderer, never inside it

    // discovery is the consumer's (fontdb → bytes). map dedups; "map" leaves mmap open.
    fn map_font(&mut self, src: FontSource) -> Result<FontHandle, FontError>;
    fn register_chain(&mut self, fonts: &[FontHandle]) -> FontChainHandle;  // stored as-is
    fn drop_chain(&mut self, chain: FontChainHandle);   // settings font reload
    fn clear(&mut self);

    // one block = 1..N paragraphs flowed together, with document-global byte offsets.
    // `source` is consulted only for parts that miss; `None` = stale identity, block
    // skipped. Re-calling with an unchanged `parts` slice is a memcmp, not a reflow.
    // GPU-free. The Cow borrows from `&self`, so no lifetime is threaded anywhere.
    fn shape(&mut self, block: BlockKey, style: &Style, parts: &[ParagraphKey],
             source: &dyn ParagraphSource) -> Option<ShapedHandle>;
    fn shape_one(&mut self, key: ParagraphKey, style: &Style,
                 source: &dyn ParagraphSource) -> Option<ShapedHandle>;
    // text with no stable consumer identity (tooltips, labels): content-keyed
    fn shape_transient(&mut self, text: &str, style: &Style) -> Option<ShapedHandle>;

    // em-space queries over the whole block: box size, hit-test, carets, selection.
    // Transient — hold the handle, never the `&Layout` (see the lock). GPU-free.
    fn measure(&self, h: ShapedHandle) -> &Layout;

    // once per pass, before the draws that use them
    fn set_target(&mut self, device: &Device, format: TextureFormat);  // pipeline cached per format
    fn set_transform(&mut self, queue: &Queue, m: [f32; 16]);          // quads are emitted in local em
    fn pixel_ortho(width: u32, height: u32) -> [f32; 16];              // helper, not a mode

    // wgpu passed in directly (no bundle). `at`/`size` are in the transform's source space —
    // screen pixels under `pixel_ortho`, world units under an MVP. `size` scales em→space and
    // picks the emoji raster bucket. `clip` (same space) culls on the CPU *and*, when the
    // transform is a pixel ortho, sets the scissor. shape/measure don't take wgpu, so leaving
    // draw uncalled = device-free.
    fn draw(&mut self, device: &Device, queue: &Queue, pass: &mut RenderPass,
            h: ShapedHandle, at: Vec2, size: f32, color: Color, clip: Option<Rect>);
}
```

---

# Locked

Things we're certain about, and why.

- **The surface is one object plus handles.** A single `Text` service owns every pool and
  the GPU resources. Everything else is `Copy` handles into it — `FontHandle`,
  `FontChainHandle`, `ShapedHandle` — and value types — `ParagraphKey`, `BlockKey`, `Style`.
  Methods: `new`, `map_font`, `register_chain`, `drop_chain`, `clear`, `shape`, `shape_one`,
  `measure`, `set_target`, `set_transform`, `draw`. wgpu is borrowed only at `draw` (plus the two
  per-pass setters). No per-frame object.

- **Two consumer-facing levels: the *block* is the coordinate space, the *paragraph* is the
  unit of invalidation.** `shape` takes a `BlockKey` and a slice of `ParagraphKey`s and returns
  one `ShapedHandle` over all of them — one byte range, one line list, one thing you measure,
  hit-test and draw. Keys stay per-paragraph, so an edit reshapes one paragraph and the rest hit
  the Level-2 cache. Grounded: compendium's drawn/measured/edited unit is a whole note body —
  `TextSpecText::Document { paragraphs: Vec<TextParagraphIdentity> }`, stitched today by
  `layout_paragraph_identity_lines` + `offset_layout_line`. A single-key `shape` would force
  either a body-wide version bump (reshapes 10k paragraphs on a keystroke) or cross-handle
  stitching in the consumer. `BlockKey` carries no version: change detection is comparing the
  parts slice, whose keys carry their own. Inline style-runs, when they land, are a third level
  *below* paragraph and stay invisible here.

- **`measure` returns a borrow you use, not one you hold.** `measure(&self, h) -> &Layout` stays
  one method, but the rule is: pass the `ShapedHandle` around, call `measure` at the point of
  use. Holding the `&Layout` in a struct deadlocks against `draw`'s `&mut self` (it rasterizes on
  a miss). Compendium clones a whole `TextLayout` today purely to escape that; with the handle
  there is nothing to hold — `ActiveEditorRender` carries the handle plus the overlay rects it
  already computes as owned `Vec`s, and `TextLayout`/`scale_pixels` leave the public surface.

- **Quads are emitted in local em space; the per-pass transform is a full 4×4.** Screen-space is
  the special case: pass `pixel_ortho(w, h)` and `at`/`size` in pixels — compendium's path,
  identical to what it emits today via `world_to_screen` + `slug_pixel_matrix`. World or 3D text
  is the *same call* with an MVP and `at` in world units — no mode flag, no second path. This
  costs nothing and is worth taking now because Slug's antialiasing uses screen-space
  derivatives in the fragment shader, so coverage is already correct under rotation and
  perspective; baking screen-space into the vertex format would throw away the crate's
  differentiator for no gain. Rendering world text offscreen-then-blitting is the wrong answer —
  it discards the resolution independence that is the entire point.
  Two things that don't come along, both fine: `clip` is screen-aligned by nature, so it's
  `Option<Rect>` (in local space it still culls, it just can't set a scissor under a general
  transform); and emoji need a real on-screen pixel size for their bucket, estimated by pushing
  a unit vector through the transform (Slug glyphs need no bucket at all). Genuine 3D would also
  want depth state on the pipeline — `set_target` grows a `depth_format` — noted, not built.

- **`clip` culls, it doesn't just scissor.** One `Rect` on `draw` does both: drop whole lines and
  individual glyphs on the CPU before emitting (what `intersects_y` / `intersects_glyph` do
  today), then set the scissor for the remainder. A scrolled 10k-paragraph body must not emit
  10k paragraphs' quads and lean on the GPU to throw them away.

- **Scrolling is `at.y`.** Scroll offset is subtracted from the draw position; `clip` does the
  rest. The block stores its paragraphs' cumulative line offsets, so `draw` binary-searches the
  clip rect to the first visible line and walks only what's on screen — per-frame cost is
  O(visible lines), not O(document). Block assembly is cached (unchanged parts = memcmp), and
  `measure` gives content height for the scrollbar, which deletes compendium's
  `measure_body_content_height` and its `body_content_h` cache.

- **`fetch` is fallible.** `FnMut(ParagraphKey) -> Option<Cow<str>>`, and `shape` returns
  `Option<ShapedHandle>`. An identity can go stale between building the draw list and shaping
  (a slot freed or reused by a sync merge landing mid-frame); today's crate already guards it
  twice — the provider returns `Option`, and the layout path bails on a `byte_len` mismatch.
  Without a failure path the alternatives are panic or silently rendering the wrong paragraph.

- **`ParagraphKey` keeps a namespace.** `{namespace, slot, generation}`, not `{id, version}`.
  Same 16 bytes, but folding a document id and a pool slot into one `u64` needs a hash, and a
  hash collision in a cache key renders *the wrong text*, silently and persistently. Grounded:
  compendium's `TextParagraphCacheKey::namespace` exists precisely because the shaping cache is
  shared across panes and slots are only unique within a document.

- **Font ingestion is `Arc<dyn AsRef<[u8]> + Send + Sync>`, deduped by data pointer.** It takes
  anything — `Vec<u8>`, `include_bytes!`, an mmap — and it is *exactly* fontdb's
  `make_shared_face_data` return type, so bytes the consumer already discovered pass straight
  through with no copy and no re-wrap (match the `+ Send + Sync` or they won't). This is also
  what makes `drop_chain`/`clear` real: today `Font::from_bytes_with_index` **`Box::leak`s** the
  bytes to get a `'static` face (`font.rs:44`), so every `reload_fonts` leaks a whole chain
  including the 20–40 MB emoji font. Keeping the `Arc` alongside the face — one contained
  `unsafe` to hold the `'static` slice, with the `Arc` outliving it and dropping together —
  replaces a permanent leak with a sound one. Dedup key: `(data.as_ptr(), data.len(),
  face_index)`. No bytes hash: 40 MB per font at startup is a real cost to defend against a
  consumer that didn't share, and sharing is one fontdb call (see the consumer note below).

- **Atlas invariants (internal, no surface).** `draw` may rasterize mid-pass, so: allocate each
  atlas texture **once** at the device cap and never recreate it (a recreate invalidates a bind
  group the consumer has already recorded into their pass), and **never evict a slot touched
  since the last submit** (otherwise a draw recorded earlier in the same pass samples the new
  glyph). Adds are safe — `queue.write_texture` lands before the command buffer executes. This
  is realistic on the *emoji* atlas specifically, since it's the bounded, evicting one. Same
  shape of rule for the vertex arena: bump-allocate, never reuse a region within a frame.

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
  required: `map_font` dedup on data-pointer identity — **which only fires if the consumer
  shares the bytes.** compendium doesn't today: `resolve_font_chain` builds a fresh
  `fontdb::Database` per call and `data.to_vec()`s each face (`renderer.rs:133`), so UI and Mono
  get two different allocations of the same emoji font. The consumer-side fix is to keep one
  `Database` alive and use `make_shared_face_data(id)` — same `Arc` for both chains. Without
  that change this benefit silently does not happen.

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

- **We never own the text — the consumer's data structure stays authoritative.** The service
  stores only *shaping* (derived, disposable), keyed by identity, and borrows a `&str` via
  `fetch` transiently on a miss. So the backing store is the consumer's choice — rope, gap
  buffer, an immutable/persistent rope, a CRDT — and `version` is *its* version. This is the
  same "don't seize the lifecycle" rule as *not owning the pass*, applied to data: render
  lifecycle → draw into your pass; data lifecycle → keep your rope, we fetch by identity.
  Contrast cosmic-text, whose `Buffer` **owns** the text as `Vec<BufferLine>` of `String`s (not
  a rope) — fine with no data model, but a synced shadow of your rope/CRDT if you have one.
  Compendium has both a rope and collaborative sync, so text-ownership had to stay app-side.

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
- **`Box::leak`ing font bytes** (`font.rs:44`) → an `Arc` held beside the face, so a chain can
  actually be dropped. Today every settings-driven `reload_fonts` leaks the whole chain.
- **`FontSource::{Bytes, Path}`** → one shared-bytes handle; the crate stops doing file I/O.
- **The three nested identity structs** (`ParagraphIdentity`, `TextParagraphIdentity`,
  `TextParagraphCacheKey`) → one flat `ParagraphKey { namespace, slot, generation }`, plus a
  `BlockKey` for the composed unit.
- **Public `TextLayout` as a constructed, cloned, owned value** — and `scale_pixels` with it.
  `Layout` becomes something you only ever borrow from `measure` for the length of a call.
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

- **`ShapedHandle` as a distinct `Copy` token vs just `draw(BlockKey, …)`.** *We'd keep it* to
  skip re-hashing the key on measure/draw. *But maybe* collapse to draw-by-`BlockKey` (one extra
  hashmap lookup per draw, probably nothing). A dial, not a principle.

- **Inline style-runs for rich text / markdown.** *We'd support it* as consumer-supplied
  styled sub-runs within a paragraph (consumer breaks by style, service itemizes each by
  font). *But maybe* it reshapes the "one Key = one paragraph" story — today's `Style` is
  uniform-per-run. A markdown renderer makes this real, so it's likely in scope but undesigned.

- **`FontError` enum replacing `Result<_, String>`.** In the sketch as `map_font -> Result`;
  stringly errors are wrong for a published crate. *But* exact variants (Io / Parse /
  NoCoverage / …) TBD.

---

# Up for litigation

Genuinely open.

- ~~**How wgpu enters.**~~ **Settled.** No abstract backend trait — wgpu is huge, moving, and
  already *is* the abstraction over Vulkan/Metal/DX; abstracting it is abstracting an
  abstraction. Live handles are borrowed at `draw` and at the two per-pass setters, passed
  directly rather than in a `Wgpu<'_>` bundle. Device-free `measure` falls out for free.
  Position/transform resolved to local-em quads + a per-pass 4×4 with a `pixel_ortho` helper
  (see the lock), and target format resolved to `set_target` with a pipeline cached per format —
  so `new()` takes nothing.

- ~~**Handle granularity for markdown blocks.**~~ **Mostly settled by the block/paragraph
  split.** Heading / list-item / code-block each get their own `BlockKey` — they need a
  different `size_px` at draw anyway, so they can't share one. Still genuinely open: selection
  *across* heterogeneous blocks, which is consumer-side stitching over per-block `measure`.
  What can't be expressed at all is inline bold/italic *within* a paragraph, since `Style` is
  per-block; that lands as sub-runs below the paragraph (see Maybe) and doesn't change `shape`.

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

- ~~**Per-`ShapedHandle` draw calls vs coalescing.**~~ **Measured.** Per-handle is fine at
  paragraph granularity (compendium draws hundreds of items) and ruinous at glyph
  granularity: `unicode_zoom` draws 41 472 one-glyph blocks, which costs 267 ms one call
  each and 6.5 ms through `draw_batch`. So both exist — `draw` for the ordinary case,
  `draw_batch` when a consumer has many blocks and one set of pass state. Remaining gap
  versus the pre-redesign baseline on that frame is ~2x (6.5 ms vs 3.3 ms), and it is
  memory locality: 41k separate small vertex vecs instead of a few row-sized contiguous
  ones. Closing it would mean caching geometry at a coarser grain than the block, which no
  realistic consumer needs.

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

# Migration test (compendium, branch `text-api-migration`)

The API skeleton is real code (`src/text.rs`) with real types, signatures and borrow
semantics; the bodies are `todo!()`, so this proves the shape compiles, not that it
renders. compendium is ported against it: **`cargo check --lib --tests` is clean**,
with **zero borrow or lifetime errors** anywhere in the port. Compendium's own two
examples (`unicode_smoke`, `perf_scenarios`) are not ported — sanscale's examples
cover that ground.

Net **−92 lines** across 21 consumer files. Worth being honest about that number: the
deletions are much larger than the net, and the offset is parameter threading from
making the service a sibling (below). That is a real cost, paid for a real property.

Confirmed:
- **`&mut self.text` inside a live render pass is fine.** `draw` borrows `&self.device`,
  `&self.queue` and `&mut self.text` while the pass holds `&self.rect_pipeline` etc. —
  disjoint field borrows, no conflict. The whole "does draw-into-their-pass work" question
  is settled.
- **`measure` → `draw` in sequence works.** `renderer.text.measure(h)` immutably, build the
  render struct, then `&mut` the renderer on the next line. The `ActiveEditorRender<'a>`
  lifetime disappears entirely because it carries the `Copy` handle instead of `&TextLayout`.
- **The block/paragraph split fits.** `TextParagraphCacheKey{namespace, id, version{slot,
  generation}}` maps to `ParagraphKey` with no packing and no hash. `BlockKey` is minted from
  `(node id, TextKind)` in one 8-line function.
- **The collapse is real.** `Renderer`'s 17 text fields → 3. `RenderPane`'s
  `P: TextParagraphProvider` generic and `BoxParagraphProvider` → one `&dyn Fn`. Four
  `layout_text_paragraph_runs*` helpers → `shape_block` + `text_style`. Four atlas syncs,
  four buffer grow-and-write blocks and two `write_matrix` calls → `set_target` +
  `set_transform`. Every `pos.y + ascent * size_px` gone. `EditorLayoutKey`, `scale_pixels`,
  the zoom fast path and `desired_x *= scale` all deleted.

Changed by the test:
- **Text is supplied by a `&dyn ParagraphSource`, not a closure.** One method,
  `paragraph_text(&self, index, key) -> Option<Cow<'_, str>>`. A closure forces its text
  lifetime to be a parameter of `shape` *and* of anything storing it, which then unifies
  with the caller's other borrows (`RenderPane<'a, 't>`, and every borrow in the pane
  required to outlive the document). Tying the `Cow` to `&self` removes the lifetime
  entirely at zero cost — verified by compiling both. Note this is **not** the old provider
  trait: `&self` not `&mut self`, used as `&dyn` not as a generic parameter, so the
  five-signature threading and the `BoxParagraphProvider` double-box still go away.
- **The source takes `(index, key)`, not just the key.** `shape` calls it for *misses only*,
  so an implementor cannot assume one in-order call per part; one holding text positionally
  needs the index. The index-free version is silently wrong, not a compile error.
- **`shape_transient(&str, &Style)` added.** compendium has text with no stable identity.
  Without a content-keyed path every consumer must invent a key for "just draw this string",
  reintroducing the collision risk `namespace` exists to avoid.
- **`fontdb::make_shared_face_data` is `unsafe`** (it mmaps). Fine — same contract as any
  mmap font stack — but the "just take the Arc" answer has an `unsafe` in it.

Resolved — where the service lives:
- **`Text` is a sibling of the consumer's renderer, not a field of it.** The migration
  surfaced this as caret motion and edit actions needing a `&Renderer`, cascading into the
  keyboard input path — exactly what compendium keeps GPU-free on purpose. The fix is not to
  hand the editor a copy of the layout (a "small line table" supporting `caret_on_line` needs
  per-line caret stops, i.e. most of `Layout` — that is just today's clone again). It is that
  **nothing in `Text` needs a renderer**: `shape`/`measure` are device-free and `draw` takes
  `device`/`queue`/`pass` as parameters. So model code borrows `&Text` — a shaping cache, no
  GPU handles — and the rule is respected rather than bent. Bonus: it makes shaping and
  measuring work headless, deleting compendium's existing "the layout below needs the
  renderer, so bail without one" path. In the port this is a `TextSystem { text, ui_chain,
  mono_chain, font_db }` sitting next to `Renderer` in `App`.

Also collapsed:
- **`paragraph_snapshots()` is out of the hot path.** It materialized the whole body's text
  (a fresh `String` per rope-backed paragraph) ~4× per frame in the editor path. The view now
  *is* the `ParagraphSource`, so text is pulled only for paragraphs that actually miss.
- **`local_text_clip`** — dead once `draw` takes the clip; deleted.
- **`TextParagraphIdentity`'s `byte_start`/`byte_len`** — block-global offsets are derived
  from the parts by the service, so the consumer no longer computes or carries them. The
  document test asserting them was deleted along with the fields.
- **`TextSpecText::Owned { keys: Some(..) }` should not exist.** Its only site is command-runner
  output, which materializes a `String` purely to satisfy the old API despite being
  document-backed. It becomes `Document { parts }` — which is also why no `&[&str]` source
  adapter is needed: every remaining `Owned` has no identity and takes `shape_transient`.
- Still on the list, not taken: **`body_content_h` / `BodyLayoutKey` / `measure_visible_bodies`**,
  26 references across 6 files caching a measurement that is now an O(1) read off `measure`.

Two things the port added to the API that were not in the sketch:
- **`Layout::from_lines`** — a synthetic layout built from line geometry, no font, no GPU.
  Caret motion, selection and hit-testing are pure geometry and a consumer must be able to
  unit-test them against a fake layout; the opaque `Layout` otherwise makes that logic
  untestable, which is exactly what happened to compendium's caret tests mid-migration.
- **`diagnostics()`** — `chain_families`, `uncovered_chars`, atlas sizes, dropped glyphs.
  Planned as "inline it into the examples", but a *consumer's* headless smoke test uses the
  coverage queries to pin down tofu, so it wants a supported accessor. Low-cost either way.

---

# Implementation notes (the surface is now real)

`src/text.rs` is the implementation, not a skeleton: `lib.rs` exposes only it, the old
`TextEngine` surface is deleted, and the three headless examples render through it.

Learned while building it:
- **Handles must carry a generation, and slot allocation must not scan.** A bare-index
  `ShapedHandle` is a correctness bug: eviction frees a slot, the next `shape` reuses it, and
  a handle the consumer cached silently resolves to a *different* block — glyphs still draw,
  just the wrong ones. Caught as visible corruption in `unicode_zoom`. The companion mistake
  is allocating by scanning for a free slot, which is O(n) per `shape` and quadratic over a
  frame; a tombstone free-list fixes it. Order-preserving removal only — never `swap_remove`,
  which would invalidate live handles wholesale.
- **Block eviction must be capacity-bounded, not time-bounded.** The consumer holds handles
  for as long as it likes and the service cannot know one is dead, so a TTL measured in
  `shape` calls evicts blocks that are still in use. The generation check makes that *safe*
  (they draw nothing) but the result is missing text. Under a capacity bound, normal
  workloads never evict and a pathological one loses its coldest blocks.
- **Geometry must be cached on the CPU and batched, not one GPU buffer per block.** Quads
  live in the transform's source space, so they survive pan and zoom; keeping them host-side
  lets `draw_batch` concatenate many blocks into one upload and one draw call. A buffer and
  a draw call per block cost **267 ms** on a 41k-block frame versus **6.5 ms** batched — 40x,
  and none of it about text.
- **Per-draw vertex buffers are correct and simple.** wgpu 29's
  `RenderPass::set_vertex_buffer` takes a `BufferSlice<'_>` with *no* lifetime tie —
  resources bound into a pass are ref-counted internally. So each `draw` builds its own
  buffer, binds it and drops it. That removes the shared-region clobber hazard entirely
  rather than managing around it, and it means the old `draw_vertices<'a>(&'a self, pass:
  &mut RenderPass<'a>, …, &'a Buffer)` signature was over-constrained — a holdover from
  when wgpu borrowed its resources. The geometry pool remains the optimization that
  removes the per-draw allocation, exactly as designed.
- **Glyphs must carry the *global* font id, not the chain position.** Otherwise two chains
  sharing a face key into two glyph-cache entries and the dedup buys nothing. `ShapedGlyph`
  carries `font_id`; the chain is a borrow of `(id, &Font)` pairs out of the pool.
- **`Layout::from_lines` and `diagnostics()` are load-bearing, not conveniences.** Without
  the first, an editor's caret logic is untestable; without the second, a consumer can't
  diagnose tofu. Both came out of the migration, not the design.
- **Line flow assumes LTR visual order.** `wrap` takes a token's extent from its first and
  last glyph by cluster, which for an RTL run gives a negative width and pushes the line
  left of its origin — visible as Arabic and Hebrew rendering off the left edge in the
  `unicode` example. This is inherited unchanged from the old engine, not new, and it is
  the concrete symptom of the parked bidi item: bidi needs reordering *before* flow, not a
  patch inside it.

Still stubbed, deliberately: the Level-1 run cache (pool 5) is not built — shaping caches
per paragraph, as it did before. It slots under `ensure_paragraph` without touching the
surface.

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

One thing compendium has to *change*, not just delete:
- **Share the font bytes.** `resolve_font_chain` builds a fresh `fontdb::Database` per call and
  `data.to_vec()`s each face (`renderer.rs:104–140`). Keep one `Database` alive and switch to
  `make_shared_face_data(id) -> (Arc<dyn AsRef<[u8]> + Sync + Send>, u32)`, so UI and Mono hand
  `map_font` the *same* `Arc` for a shared face. Without this the dedup never fires and the
  "4 atlases → 2, emoji rasterized once" win silently doesn't happen.

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

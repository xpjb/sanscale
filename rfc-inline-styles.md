# Inline styles, the C editor, and Markdown

**Status: C-editor milestone implemented.** `FontSpan`, `PaintSpan`, the immutable
paint pool, `Draw.paint` and `Draw::default()` are present. The separate `code-editor`
example uses incremental line-state C lexing, bold keywords/types, italic comments
and independent palette/font restyling. A separate `markdown-editor` now prototypes
custom incremental parsing, projected source maps and streaming tables outside the
core crate. Its [component contract](examples/markdown-editor/markdown/README.md)
records the supported dialect and work bounds. Stable component packaging/full
Markdown conformance and finer shaping/flow reuse remain future work. The plain
`editor` remains a notepad. This note records the
design constraints as well as the implementation's explicit coarse work; it is not
a promise that every proposed optimization exists.

### Implemented contracts and costs

- `ParagraphSource::paragraph_fonts` returns `Cow<'_, [FontSpan]>`, empty by default.
  Invalid/non-grapheme-safe ranges or unavailable chains make `shape` return `None`.
- Actual selected faces coalesce across equal spans and different chains sharing a
  face. Real font boundaries pass pre/post context to rustybuzz. There is no run cache.
- `register_paint` copies a validated snapshot; `drop_paint` releases it. The 131,072
  slots use generations; exhausted generations retire instead of wrapping. No interning
  or eviction of live snapshots. Cached spans/gaps plus binary search avoid scanning all
  paint spans for every glyph, including clipped starts and reversed cluster order.
- `Draw::default` uses `ShapedHandle::INVALID`, (0,0), size 1, opaque black, and no
  paint/clip. Construction uses struct literals, not a builder.
- Paragraph and block records retain only their additional inline-chain dependencies,
  so dropping a span-only chain invalidates its dependents, even if paragraph entries
  were evicted. This also fixed inherited tombstone handling: release/eviction now bumps
  a block's generation immediately and discards its CPU geometry, not only on slot reuse.
- A paragraph-byte origin on each assembled line supplies block-wide paint coordinates;
  glyph copies remain paragraph-local. This adds per-line metadata rather than a new
  per-glyph rebasing walk on the plain path. Paint/dependency handles also add metadata;
  legacy benchmarks are retained to expose the cost, not replaced with styled-only cases.
- **Accepted coarse invalidation:** changing even a tiny font span reshapes and flows the
  entire paragraph. All other cached paragraphs are reused, but block assembly still
  copies them. Recoloring rebuilds the block's geometry and re-preparing uploads the
  requested batch. No additional cache layer was introduced. Run-level provenance,
  separate shape/flow keys and segmented block assembly remain measured follow-ups.
- **Editor-specific broad work:** the lexer propagates only until cached line state
  matches. Font resolution visits line metadata after edits and resolves only dirty
  lines (all lines on an italic-theme change), bumping only changed font inputs. The
  flat paint snapshot is rebuilt/diffed across all tokens after text or palette changes,
  including byte rebasing; unchanged results retain the old handle. Per-paragraph paint
  snapshots/indexed aggregation are a future route if that whole-document scan matters.
  Neither theme toggle re-lexes source, dirties the file, or manufactures a text revision.
- Caller-owned body/chrome batches survive caret blinking. All text is prepared before
  any glyph draw, and viewport scissors apply to glyphs, selections and the caret.

Tests cover ranges/graphemes, actual italic faces, ligature-preserving coalescing,
block-global paint, source/layout work on recolor, dependency-local chain drop,
paint-slot reuse/exhaustion and incremental editor/theme behavior. The original
performance suite remains, with additive `cpu.spans.*` / `gpu.spans.*` cases.


## Direction and scope

- First deliverable: a **C editor example** with foreground syntax colors and
  real bold/italic font faces, as `code-editor`, retaining the plain `editor` example
  and using its layout/interaction pattern.
- Core library work: inline font-chain spans and foreground paint spans. Keep one
  coherent layout for rendering, wrapping, carets, hit-testing, and selection.
- Paint belongs in a **service-owned pool**, addressed by a `Copy` handle. Add
  `paint: Option<PaintHandle>` to `Draw`; keep `Draw` `Copy` and lifetime-free.
- Ordinary text does not register paint. `None` uses the existing `Draw.color`.
  Existing `draw(...)` convenience calls keep their signatures and behavior.
- No monospace-specific fast path or additional line-height policy in this work.
  Preserve the current base-style line metrics.
- The subsequent product target is a **reusable incremental Markdown renderer,
  including GFM-style tables**, not just a disposable preview demo. Tables are
  outside the first C-editor milestone, not outside the intended destination.

The optional `Draw` field and its small source migration are implemented. The
source-trait and pool lifecycle contracts are summarized above. Construction should use
**struct literals with `Default`**, not a positional constructor or builder.

## Constructing `Draw`: struct literals with defaults

This supersedes the earlier constructor/builder suggestions in the discussion.
Prefer transparent public fields and ordinary struct-update syntax:

```rust
Draw {
    block,
    at,
    size,
    color,
    paint: Some(paint),
    ..Default::default()
}
```

Plain draws omit `paint`; optional paint and clipping default to `None`. Full
literals remain valid too. Update the examples and consumer-facing snippets when
the field lands. Some source churn when updating a pinned version is acceptable;
do not build another API layer solely to hide the underlying data structure or
avoid that migration.

Defaults must be documented values, not registration work. In particular, the
recommended default block is an explicitly invalid/no-op sentinel, **not pool
slot zero or a newly allocated resource**; normal callers supply a real `block`.
This consciously trades compile-time required fields for a defined no-op default,
consistent with the existing stale-handle draw behavior. The numerical defaults are now documented above; tests pin that an
entirely default draw cannot accidentally render a live block. No allocation,
new pool entry, borrowed lifetime, constructor, or builder is needed.

## Span and ownership contracts

### Shaping

Font spans are paragraph-local UTF-8 byte ranges selecting a `FontChainHandle`.
They are sorted, non-overlapping, and grapheme-safe; gaps inherit `Style.chain`.
Resolve nested style combinations in the application, including bold + italic.
Do not split the source into independently laid-out token blocks.

The paragraph-source extension defaults to no spans, without new plain-source
bookkeeping; its signature and rejection rules are recorded above.
`ParagraphKey::generation` must cover **text and effective font spans**, because
`shape`/`ensure_paragraph` do not consult the source on a cache hit.

That generation is a *shaping-input revision*, not necessarily a text-edit
counter. A theme changing comments from regular to italic must not manufacture a
text edit, invalidate the parser, or alter undo/save state. Conversely, a color
change must not bump this generation. Diff resolved spans before bumping it.

Inline chains become real dependencies: `drop_chain` must find layouts using a
chain in any span, not only in the base style. Do not solve that by clearing all
layouts or fonts.

### Painting

Implemented: an **immutable, foreground-only span snapshot** per paint-pool
entry. Ranges are block-local bytes; gaps use the draw's default color. Registration
copies/owns the input rather than borrowing it across frames. Use generational
handles and explicit release; exhaustion and stale-handle behavior are specified above. A recycled slot must never silently become a different paint snapshot.

A new snapshot gives a new handle, which participates in `Draw` comparison and the
existing geometry cache key. No paragraph-layout or glyph-atlas invalidation is
needed for a foreground change. Keep the existing native-color emoji behavior;
foreground spans do not introduce emoji tinting.

Paint boundaries **must not split shaping**. Initial policy: the color at a shaped
cluster's starting byte colors that whole cluster, even if a range boundary falls
inside a ligature. This is a declared expressiveness limit, not a reason to reshape
on recoloring.

Be explicit about coordinates: current `assemble()` rebases caret offsets but
copies glyphs with paragraph-local cluster offsets. The implementation carries the
paragraph origin on each line during paint lookup; block-wide paint never uses
those local offsets directly.

### Retained batches

Preserve the current ownership division:

- The consumer re-prepares when draw inputs change, including the paint handle.
- `batch_live()` detects service-owned changes, such as reshaping and atlas reuse;
  it is not a detector for a caller choosing different draw inputs.
- Immutable foreground paint is baked into vertices. A prepared batch must not
  borrow the spans or require their pool entry merely to draw those baked colors.
  Releasing that snapshot does not by itself change its already-prepared pixels;
  other batch dependencies still apply.
- Do not mutate paint behind an unchanged handle under this contract. If mutable
  entries are introduced later, geometry and retained batches both need their
  revisions tracked. That would be a deliberate contract extension.

A paint pool owns registered inputs. It is **not** the speculative additional
run-shaping cache discussed below.

## Performance is an explicit part of the contract

**Do not silently trade broader invalidation for less bookkeeping.** A correct
image does not establish that a cache change is acceptable. For each such tradeoff,
record the trigger, necessary invalidation, actual extra work, reason, measurement
or test evidence, and route to a finer implementation. Identify whether it is
inherited behavior, a newly proposed shortcut, or implemented and measured.

The table describes semantic dependencies, not promises about today's cache grain:

| Change | Work it can require | Work it must not imply merely for convenience |
|---|---|---|
| Foreground-only theme change | Paint snapshot, affected geometry, caller's affected batches | Parsing, shaping, flow, or clearing glyph atlases |
| Capture changes but resolved spans are identical | Nothing in sanscale | New paragraph generations or gratuitous paint replacement |
| Regular to italic/bold | Glyph selection/shaping for affected shaping contexts; possibly flow; new quads | Reparse unchanged source or reshape unrelated paragraphs |
| Wrap width, alignment, or line spacing | Flow/placement under the current shaping model | A semantic requirement to reshape the text |
| Text edit | Parsing/highlighting and shaping/layout whose inputs actually change | Whole-document version bumps just because highlighting ran over the whole document |
| One snapshot/chain changes or is released | Its actual dependents, according to the ownership rules above | A global paint epoch or clearing every cached block |

### Italic changes: distinguish shaping from layout

Switching faces is not generally paint-only. Glyph IDs, substitutions, clusters,
advances, and ink bounds can change. Do not promise that italicization needs zero
shaping, and do not simulate it by skewing the regular glyph to avoid doing the
correct work.

The performance opportunities to preserve are:

1. Reuse unaffected shaping runs when their text, effective face/features, and
   relevant context are unchanged.
2. Reuse a previously shaped result when toggling back to the same inputs, if a
   future cache retains that result.
3. Avoid new flow/caret computation if all relevant cluster boundaries, advances,
   break opportunities, and metrics are unchanged. Equal *total width* alone is
   not enough, and equal metrics do not mean the glyph quads are unchanged.

Shaping context matters: a highlight-token boundary or even a grapheme boundary
is not automatically a safe independent shaping/cache boundary. Preserve context
and coalesce equivalent shaping runs. Font discovery, semantic capture names, and
colors must not become accidental run-cache keys.

The first implementation can use paragraph revisions for correctness without
making whole-paragraph reshaping an immutable API promise. If it reshapes an entire
paragraph for a small font-span change, **call that out as coarse invalidation**.
Paragraph generations alone do not magically provide finer reuse: that needs
retained provenance/run data and correct keys. Do not claim it is already solved,
or introduce another cache without measuring the cost and retained memory.

### Current costs to keep visible (not introduced by this note)

- **Shaping and flow are cached together.** `paragraphs` is keyed by
  `(ParagraphKey, Style)`. `Style` includes width, alignment, and line spacing;
  `ensure_paragraph()` shapes before flowing on a miss. An uncached width can
  therefore cause avoidable shaping. Cycling warmed widths measures cache hits,
  not this cost.
- **One changed paragraph still reassembles the composed block.** `assemble()`
  copies the cached lines/carets/glyphs of all its paragraphs, and `shape()` clears
  the block's geometry. "Only one paragraph reshaped" is not "the edit does work
  proportional only to one paragraph."
- **Geometry has one cached variant per block.** Alternating colors/clips already
  causes replacement; alternating paint handles would have the same limitation.
  There is no multi-variant geometry cache in this proposal.
- **Preparing a batch rebuilds/uploads that requested batch.** Per-block CPU
  geometry reuse does not make an edited retained GPU batch a partial update.
  An unchanged retained batch remains the no-upload path.

Do not automatically fix all of these in the span feature. Establish baselines,
name the costs in implementation notes, and revisit the measured bottleneck.
The API should not require throwing away unchanged work to remain correct.

### Proposed simplifications that also need disclosure

- **Whole-document highlighting after edits** is an application-side starting
  point, not incremental parsing. Run it on source edits, not every frame or
  foreground-theme change. Diff resolved per-paragraph font spans and paint
  inputs so its broad work does not become broad library invalidation.
- **Identity-based paint lookup without interning:** independently registered
  equal snapshots can have different handles and miss the geometry cache. Retain
  the old handle when the application resolves identical spans. Do not claim the
  pool deduplicates content unless it actually does; content interning is not
  required merely to introduce the pool.
- **Whole-paragraph font-style invalidation** may be the starting implementation,
  but its extra shaping cost needs explicit evidence as described above. Do not
  quietly classify a font-only change as a reason to reset all source identities.

## C highlighting: a small lexer is an acceptable adapter

Tree-sitter is an option, not a requirement or a sanscale dependency. A tiny
example-local C lexer is sufficient to demonstrate keyword colors/bold and italic
comments. It is not a claim to implement the C grammar or preprocessor.

Keep tokens/capture categories separate from theme resolution. Handle strings,
character literals, escapes, line/block comments, and multiline state deliberately;
`/*` inside a string is not a comment opener. State the supported subset and test
it, including incomplete input and comment edits affecting subsequent lines.
Preprocessing/line-splicing limitations must be explicit rather than mislabeled
as full C support. Either adapter feeds the same byte-span interface.

## Future work, without making it a prerequisite

- Markdown source styling uses the same spans, with nested emphasis resolved to
  the appropriate face. The preview projects a different string: escapes,
  entities, and line-break handling require range conversion, not subtracting
  delimiter offsets. Precise click-to-source mapping is later work.
- Incremental Markdown rendering retains block/cell identities and independent
  text layouts. Tables add column sizing, cell wrapping, and row heights. A column
  width change can legitimately reflow many cells; a changed table height can move
  later blocks without reshaping their text. Track those dependencies rather than
  promising every edit is local. Arbitrary editing versus append-only streaming,
  and the table-width stability policy, still need choosing.
- Incremental rendering and incremental parsing are distinct. Parser selection or
  implementation is a separate task, not a prerequisite for the span API. The
  reusable Markdown component's packaging is also not decided here.
- Consider separating unwrapped shaping from flow, or adding context-safe run
  reuse, only with evidence from style toggles, long paragraphs, pane resizing,
  or table-column updates. The earlier Level-1 pool idea in `decisions.md` remains
  proposed, not a committed new cache.
- Real tab stops, cross-block selection, images/HTML, richer decorations, and
  additional line-metric policies are separate features. None should enlarge the
  first C-editor milestone. Monospace specialization is explicitly deferred.

## Evidence expected when implementation lands

The [pathological regression suite](performance.md) is now present before the span
implementation. Capture its unchanged legacy workloads before/after each feature;
add the pending span-specific cases without replacing the old workload underneath
the comparison. Production timings and instrumented work counts are separate runs.

- Counters/tests distinguish source reads, actual shaping, flow, geometry builds,
  and batch uploads. Do not infer all of them from a single cache-hit number.
- A foreground-only change does no shaping/flow; a shape-only theme change does
  no parsing. An unchanged resolved span set preserves its identities.
- Font-span changes invalidate correctly even when source text is identical,
  including distant lines affected by a C block-comment edit.
- Cover multiple paragraphs, UTF-8, combining sequences, ligatures, native-color
  emoji, and nested bold/italic; hit-testing and drawing use the same layout.
- Test paint-slot reuse, immutable snapshot lifetimes, `Draw` comparison, retained
  batches, and dropping a chain referenced only by a font span.
- Compare plain-text behavior and steady-state costs against the existing path.
  Ordinary draws must not register paint or acquire per-frame span bookkeeping.
- Performance reports for width changes must include genuine cache misses;
  reports for font changes must state the actual reshaping grain. Include memory
  costs when proposing finer caches. The C example should retain `--dump` support.

Before implementation, settle the remaining source/registration signatures and
error contracts. Before calling it complete, record any deviations or conservative
invalidation explicitly here and in the relevant decision/backlog entries.

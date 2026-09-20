# sanscale — backlog

Known-and-parked. Things we've decided not to do *yet*, with enough of the reason
written down that picking one up doesn't mean rediscovering why it's here.

Distinct from `decisions.md`, which records what the design *is* and why. This is
what's owed.

---

## `chain_view` allocates a `Vec` on every call

`TextService::chain_view` doesn't borrow the chain — it builds a fresh
`Vec<ChainFont<'_>>` each time, pairing every handle in the chain with its
`&Font`. Twelve call sites, including all six `Diagnostics` methods.

The visible cost today is startup, not frames: `unicode`'s `build_row` calls
`covers()` and `glyph_bbox()` per cell, so populating every row does roughly 83k
throwaway allocations. Rows are cached, so it never recurs, and it does not appear
in the frame probe at all. `shape()` is clean on its early-out path — it checks
`chain_fonts()`, which returns a slice.

It would start to matter for a consumer calling diagnostics per frame, which both
examples do (once per frame in `hovered()`, not per cell — so, cheaply).

Fix is a scratch buffer threaded through the call, or a small-vector that stays on
the stack for the common chain length. Neither is interesting; it's tidy-up, and
it's parked because it currently costs nobody a frame.

---

## Does `diagnostics()` belong in the public API at all?

It arrived by accretion rather than design. `is_single_glyph`, `glyph_bbox` and
`family_for` were example-only escape hatches that had leaked into the old public
surface; `decisions.md` listed them under *Cutting*, to be "a `diagnostics()`
accessor, or inlined into the examples". They became the accessor, and then
`uncovered_chars`, `covers`, `atlas_sizes`, `dropped_glyphs` and
`cache_occupancy` joined them.

The case for keeping it: a consumer's headless smoke test genuinely uses the
coverage queries to pin down tofu, and `dropped_glyphs`/`atlas_sizes` are how
atlas overflow stops being invisible. Those are real, supported needs.

The case against: it is the one part of the surface with no unifying idea, and it
has already caused a real bug. Three of its methods reimplemented the fallback
walk instead of calling it, drifted, and silently reported the wrong face for 220
code points — and because `glyph_bbox` feeds cell fit-scaling, that moved geometry
rather than just labels. A surface whose methods *look* like the real thing but
answer a slightly different question is a trap, and this one sprung.

Worth deciding deliberately: which of these are load-bearing for a consumer
(coverage, atlas pressure) versus example scaffolding that should live in the
examples (`glyph_bbox`, `is_single_glyph`), and whether the survivors want to be
one grab-bag or to sit next to the thing they describe. Not urgent — the
correctness bug is fixed — but the shape is unresolved.

---

## Clip in the fragment shader (the batch RFC's deferred "Part 2")

Parts 0–1 of `rfc-batch-cache.md` landed 2026-07-28 and the RFC folded into
`decisions.md` (the "`Batch` owns its vertices" lock); this is the one piece
deliberately not built.

A scissor is pass state, so a `Batch` today draws one call per clip-distinct
`Segment`. Moving the cut into the fragment shader — a per-block clip rect,
preferably a +4 B per-vertex index into a storage buffer rather than a +16 B
baked rect — collapses every batch to one segment and one draw call, and
finishes the half-built clip story end-to-end (today the crate culls whole
glyphs and the consumer's scissor cuts straddlers), permitting antialiased and
rounded clip edges. When it lands nothing else changes: the segment loop just
runs once.

**Trigger:** draw-call count actually measuring, or wanting a clip edge the
scissor cannot express. Until then one draw call per separately-scissored item
is accepted deliberately.

---

## No measurement of reflow itself

`flow_paragraph` — greedy first-fit line breaking — has no benchmark. compendium's
`layout.slug.width_cycle_cache_hit` looks like one and is not: `wrap_em` is part of
the shaping key, so each width gets its own cache entry and after warmup the
scenario measures hits. It was renamed to say so.

This is fine today. Reflow runs only when a node or pane is genuinely resized —
zoom does not trigger it, because `wrap_em` derives from world units and the zoom
cancels — so the live path is dragging a resize handle, and nothing suggests it is
slow.

It stops being fine the moment line breaking is touched. The Knuth-Plass item in
`decisions.md` swaps the Level-2 step wholesale for an optimal-breaking pass that is
categorically more expensive, and there is no number it could regress against.
Whoever picks that up should add a reflow scenario that defeats the cache *first*,
and take a baseline before changing anything.

**Pipeline baseline added:** [the pathological suite](performance.md) now has
`cpu.cache.width_fresh` (genuine misses) separately from `width_cycle_cached`, plus
flow-call/token/glyph-test counters. This measures the current shape+flow pipeline,
not isolated `flow_paragraph` time: the combined cache still reshapes on a width
miss. A pure-flow timing probe remains separate work if the breaking algorithm
itself is changed. Do not relabel a warmed width cycle as that measurement.

---

## Rich text: preserve the route to finer invalidation

The [inline-style design note](rfc-inline-styles.md) records the proposed C-editor
milestone and the subsequent incremental Markdown renderer, including tables.
Neither is implemented by that note.

Separate shaping/flow caches and context-safe run reuse remain candidates, not
prerequisites for spans. The current combined cache can reshape on a width-only
miss, a changed paragraph reassembles its whole block, geometry has one cached
variant per block, and preparing again uploads the requested batch. Those are
specific costs, not grounds for claiming the whole pipeline is incremental just
because unchanged paragraphs hit their cache.

**Trigger:** implement rich spans with explicit work counters/baselines, then
revisit the costs that measure in style-only changes, long paragraphs, pane
resizing, or table-column updates. A font change may need different glyphs without
needing different flow; preserve the ability to reuse unaffected shaping and
unchanged layout. Do not introduce global invalidation to avoid tracking these
dependencies. Any intentionally coarser behavior needs its scope, reason, cost,
and path to finer reuse written down as required by the note.


**C-editor milestone landed:** font spans, the immutable paint pool, defaults, and
the styled C editor now implement the first part of the note. The lexer is
line-state incremental; flat paint reconstruction and block assembly remain broad
work, explicitly documented there. `*.spans.*` cases now measure font density,
tiny font changes, paint density/recoloring, pool churn and retained paint release.
Markdown/table rendering has since reached an example-local prototype (below).
Stable component packaging and context-safe finer shaping/flow reuse remain owed. Tall inline faces retain base-style line
metrics (including existing line-culling assumptions), rather than introducing a
new line-height policy in this milestone.


**Example placement clarified:** the C example is `code-editor`; `editor` remains
the plain notepad. This does not change the next product target: a reusable
incremental Markdown renderer including GFM-style tables, with retained block/cell
identities and explicit layout dependency invalidation.


**Markdown first slice landed:** `markdown-editor` adds split source/preview and an
example-local custom parser/model + separate sanscale adapter. It handles streamed
UTF-8, prefix-equivalent parsing, stable table cell projections, source mapping,
real font spans, bounded delta replay and indexed table heights/visibility. Body
content cannot change column widths; this is deliberate streaming policy, not
intrinsic table sizing. See its [contract](examples/markdown-editor/markdown/README.md).

Still owed before a reusable published component: dialect/conformance fixtures,
recursive containers/reference dependencies and desired syntax extensions;
capacity/error/lifecycle APIs; preview selection/copy and bidirectional mapping;
configurable table column sizing; and extraction into an optional companion
component. Do not pull Markdown, Rope, fontdb or windowing into `TextService`.

Measured coarse costs remain visible: initial/all-new-width table layout, middle
structural metadata shifts, active paragraph projection, code-line metadata walks,
top-level block placement, composed source-pane assembly and visible-batch uploads.
Use the local `--bench` probe and work assertions to identify the next worthwhile
refinement, rather than assuming all work is local because parsing/layout identities
are retained. The editor itself still lacks undo, IME, unsaved-change confirmation,
and preview selection; these are not silently supplied by the Markdown component.

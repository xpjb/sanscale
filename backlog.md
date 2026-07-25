# sanscale — backlog

Known-and-parked. Things we've decided not to do *yet*, with enough of the reason
written down that picking one up doesn't mean rediscovering why it's here.

Distinct from `decisions.md`, which records what the design *is* and why. This is
what's owed.

---

## `chain_view` allocates a `Vec` on every call

`Text::chain_view` doesn't borrow the chain — it builds a fresh
`Vec<ChainFont<'_>>` each time, pairing every handle in the chain with its
`&Font`. Twelve call sites, including all six `Diagnostics` methods.

The visible cost today is startup, not frames: `unicode_zoom`'s `build_row` calls
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

## Geometry lives on the CPU as a `Vec` per block

See the note in `decisions.md` on why it is CPU-side and batched rather than one
GPU buffer per block: per-block buffers and draw calls measured 267 ms on a 41k
block frame against 6.5 ms batched. That reasoning still holds and is not in
question.

What is now open is the *storage*, which is a separate question from the batching.
Each block owns two independently heap-allocated `Vec`s, so `draw_batch` chases a
pointer per block into 41k scattered allocations and re-concatenates all of them
every frame. Measured at 2.04 ms of a 4.2 ms frame at 41 472 blocks — and roughly
nothing for a consumer drawing hundreds of items, which is why it is parked rather
than fixed.

Two steps, in increasing order of ambition:

1. **Arena + ranges.** One `Vec<TextVertex>` and one `Vec<EmojiVertex>`, with
   `Geometry` holding `Range<u32>` into each. Halves the metadata array (so the
   per-block probe touches fewer cache lines — the guaranteed win, since that
   access is dense) and makes the payload one allocation. Reuse is nearly free
   because a rebuild almost never changes the vertex *count*: rebuilds fire on
   color/clip/bucket changes, and a text change goes through `shape()`, which
   discards the geometry anyway. So: same length, overwrite in place; different
   length, size-classed free list that is rarely touched.

2. **Make the cache GPU-resident.** The vertex data is now frame-invariant —
   `at` and `size` came out of `GeomKey` and are applied as an affine transform
   during concatenation — so it no longer *has* to be rebuilt when the camera
   moves. That was the constraint that forced host-side staging in the first
   place. With a block id on each vertex indexing a per-block transform buffer,
   the vertices could be uploaded once per geometry change and never again, and a
   camera move would touch a small transform array (~16 B/block) instead of the
   full vertex stream (~336 B/block). That removes the per-frame gather *and* most
   of the upload, not just the allocation.

Step 2 changes the vertex format and both shaders, so it is a major-version shape,
not a cleanup. Step 1 is self-contained. Neither is worth doing on the strength of
the argument alone — the last three predictions in this area were wrong until
measured, so measure first.

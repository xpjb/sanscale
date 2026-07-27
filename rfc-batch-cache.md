# RFC: who owns the vertices

**Status:** settled. Part 0 and Part 1 to build; Part 2 deferred with a trigger.
**Supersedes:** the batch-*caching* draft, which asked whether to cache
concatenation and how to key it. Wrong question — the defect is ownership, and
once ownership is fixed most of the caching question dissolves.

---

## The one-line version

Vertices have no owner. They are written into a service-wide scratch ring with no
lifetime, no frame boundary and no way to size itself, so it rewinds mid-frame and
overwrites vertices belonging to draws already recorded into an open pass. Give
the vertices an owner and the whole class disappears.

## Three layers, only one of which is broken

Worth stating plainly, because "the cache" has meant all three at different points
in this argument:

| layer | what it holds | state |
|---|---|---|
| shaping — `blocks` / `paragraphs`, keyed by `BlockKey` / `ParagraphKey` | text + style → glyph layout | fine |
| **CPU geometry** — `geometry[slot]`, keyed by `GeomKey { color, clip }` | block layout → quads, baked at origin and unit size | **fine, and load-bearing.** Position- and scale-independent, which is what makes it survive a camera move. Dropping an entry is always safe. |
| **`VertexArena`** — one shared ring | the concatenated, *placed* quads on their way to the GPU | **the defect** |

`VertexArena` is not a cache. Nothing in it is retained or reused across frames:
every draw concatenates from the geometry pool, applies `at`/`size`, writes into
the ring, binds a range, draws, and never looks at it again. It is a staging
buffer that outlived the assumption it was written under — one push per frame,
which is what `draw_batch` gives it and what `draw` does not.

### How it fails

`push` grows only when a **single** push exceeds `capacity / ARENA_SLACK`, so
capacity tracks the largest *item* and never per-frame traffic. When it runs out it
sets `offset = 0` and writes over live regions. A consumer issuing one `draw` per
item — which the API invites, and which is the only option for items with differing
clips — wraps within a single frame.

Against compendium's document catalog: `TextVertex` is 56 bytes, 6 per glyph, so a
12-glyph label is 4,032 B → 4,096 after `ARENA_ALIGN`. That never trips the growth
rule against the 64 KB floor, so **16 draws exhaust the arena** and everything
recorded before the wrap is corrupt. A frame containing one large block sizes the
arena generously and hides it; a frame of many small labels does not. Presents as
glyphs sliced mid-quad, garbage triangles, one enormous smeared quad — and, because
`offset` persists across frames, a *different* victim each frame from identical
content.

This contradicts two entries in `decisions.md`; see the appends there.

## The design

**A `Batch` owns a GPU buffer.** That is the whole idea — a batch is already one
contiguous vertex range with a consumer-chosen grain and a consumer-held lifetime,
which is what a VBO handle is. Nothing is shared, so nothing can be clobbered, and
there is no frame boundary to get right.

The CPU geometry pool **stays exactly as it is**. It is what makes building or
rebuilding a batch cheap, it is shared across batches, and it never had a lifetime
question. The two have different invalidation conditions and do not belong in one
entry (see *Rejected*).

```rust
prepare(&[Draw]) -> Batch     // consumer owns it: grain, residency, when to rebuild
draw_prepared(&Batch)         // draw what you own
draw_batch(&[Draw])           // = prepare + draw + drop
draw(block, at, size, …)      // = draw_batch(&[one])
```

The load-bearing property is that **the easy path is literally the hard path plus a
drop**, not a second implementation. Today `draw` and `draw_batch` are separate
routes into one ring whose sizing assumes the second, and that divergence *is* the
bug. One route, one lifetime rule, and the hazard becomes unrepresentable.

A consumer who doesn't care never learns the word "batch" and keeps today's API; a
consumer who does stops asking permission. Full control costs one public type with
two methods.

**Segments.** A scissor is pass state, so items with differing clips still cannot
share a draw call — unchanged by this RFC, that is Part 2. A `Batch` therefore
records `(range, clip)` runs; the consumer sets its scissor per run and draws that
range. Same draw-call count as today, zero per-frame upload. When Part 2 lands the
runs collapse into one draw and nothing else about the design changes.

The scissor stays consumer-side because under an arbitrary MVP the crate cannot map
a source-space clip to framebuffer pixels without also knowing the viewport. That,
not oversight, is why `draw` never set it.

## Part 0 — consumer moves to world-space `at` + MVP

Consumer-side, no crate change, and a prerequisite for any of this being worth
doing. `draw`'s contract is that `at`/`size` are *in the transform's source space*;
`rebuild_geometry` bakes at origin and unit size, and `place` applies `at`/`size` on
the way out. So vertices land in whatever space the consumer passes.

`unicode_zoom` passes world units with pan/zoom in the MVP, and its vertices survive
the camera. compendium passes `world_to_screen(...)` with zoom pre-multiplied into
`size_px`, so its vertices are window pixels and **every retained batch would die on
every pan**. Retention is meaningless until that changes.

Antialiasing is unaffected: the fragment shader takes its pixel scale from `fwidth`
on the interpolated em coordinate — a screen-space derivative — so it picks up the
MVP for free. `unicode_zoom` is the standing proof.

## Part 1 — `Batch`, and delete the arena

Build the API above. `VertexArena` goes away entirely rather than being taught a
frame boundary: with per-owner buffers there is no shared region left to protect.
wgpu 30 ref-counts what a pass binds, so a transient batch may be dropped
immediately after recording — this is the existing per-draw-buffer lock, restored.

Cost: an allocation per `prepare` where there was a bump. `unicode_zoom` is
unaffected (one batch per frame). compendium's per-item usage would allocate a few
hundred buffers a frame *until* it holds batches, which is the point of the work. If
allocation ever shows up in a profile a recycling pool can go **under** `prepare`
without touching the surface — and unlike the arena it would have the lifetime
information it needs.

## Part 2 — clip in the fragment shader (deferred)

The only thing that collapses many blocks into one draw call, by retiring the
per-item scissor. Not required for correctness and explicitly not scheduled.

- Cost: +16 B on a 56 B vertex (**+29%**) for a baked rect, or +4 B (~7%) for a
  per-block index into a storage buffer — the latter a dependent read, though
  uniform across a quad. The compare itself is nothing beside a fragment shader
  already doing several texture loads per pixel for analytic coverage.
- It would also make clipping end-to-end. Today the crate culls whole glyphs and
  leaves straddlers to the consumer's scissor, which `decisions.md` records as
  half-built; a shader clip closes that and permits antialiased and rounded clip
  edges.
- **Trigger:** draw-call count actually measuring, or wanting a clip edge the
  scissor cannot express. Until then one draw call per separately-scissored item is
  accepted deliberately.

## Rejected

- **A VBO stapled onto each CPU geometry entry.** Measured at 267 ms vs 6.5 ms on a
  41k-block frame, and locked in `decisions.md`. Two reasons beyond the number: the
  entries have **different invalidation conditions** — CPU quads are
  position-independent by design, a VBO has placement baked in — so merging them
  throws away the expensive per-glyph work to redo the cheap multiply-add whenever a
  block moves; and it re-commits the original error one level down, hardcoding block
  grain on the GPU side while fixing it on the CPU side. For compendium the two
  nearly coincide today, but per-block forecloses Part 2 and per-batch does not.
- **A service-owned, content-keyed batch cache** (the previous draft). Requires the
  service to invent an identity and an eviction bound for a lifetime the consumer
  already knows. Hashing 41k `Draw`s to discover nothing changed is not obviously
  cheaper than the concatenation it saves. A consumer-held handle deletes both
  questions.
- **Teaching the arena a frame boundary.** Correct, and about ten lines, but it is a
  new contract a consumer can violate, and Part 1 deletes the arena anyway.

## Open

- **z-contiguity.** A batch spans a z-range and drawing it out of order is a bad
  silent failure. Service-verified, or the consumer's responsibility? Leaning
  consumer's, documented — the consumer chose the grain.
- **Eviction.** Consumer-held means consumer-dropped, so there is no bound to pick,
  but a consumer minting one batch per note in a 10k-note document still wants
  guidance. Probably a README note rather than machinery.

---

Four predictions in this area were wrong until measured — locality twice, epoch
invalidation, and a clip normalisation that was invariant on paper and missed every
frame in floats. The ownership defect above is the exception only because it was
reproduced and its arithmetic checked. Everything under *Open* still deserves a
measurement before it is built.

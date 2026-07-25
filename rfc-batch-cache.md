# RFC: batch caching, and who chooses the grain

**Status:** draft, nothing built.
**Prereqs landed:** `at` and `size` are out of `GeomKey` (geometry is position- and
scale-independent); vertex uploads go through a bump arena.

---

## The measurement that starts this

`unicode_zoom`, fit-width, 41 472 one-glyph blocks:

| | baseline `f0e9d7c` | now |
|---|---|---|
| total frame (CPU) | 2.16 ms | 4.2 ms |
| emit + concatenate | — | **2.04 ms** |
| atlas sync + upload + encode | — | 1.21 ms |

The 2.04 ms is 41 472 iterations of: probe a generation array, build and compare a
`GeomKey`, chase a pointer into that block's own heap `Vec`, memcpy six vertices,
then scale-and-translate them. The baseline did the same work in 216 iterations
over row-sized contiguous runs.

It is ~0 for a consumer drawing hundreds of items. It is not a crisis. It is,
however, a symptom worth naming correctly.

---

## What actually regressed

Not "we added a cache". The old engine was immediate-mode — `flush()` handed you
vertices — and **caching was the consumer's choice**. The two consumers chose
oppositely, and both were right:

- `unicode_zoom` hand-rolled a **per-row** cache (`HashMap<i64, (Vec<TextVertex>,
  Vec<EmojiVertex>)>`) and concatenated rows each frame. Row grain is near-perfect
  for a grid: 216 entries, each one contiguous run.
- compendium cached nothing and re-uploaded everything every frame, which was fine
  at its scale.

The redesign fixed the grain at the **block**, because that is the handle a
consumer names. Block grain is right for compendium's paragraphs and badly wrong
for 41k one-glyph cells, and a consumer can no longer override it.

**So the regression is: we took away the choice of grain.** That framing matters,
because it points at an API question rather than an allocator.

---

## Prior art

| Stack | Geometry cached | Grain | Per-frame work |
|---|---|---|---|
| Skia / Chrome | CPU text-blob cache, invalidated when the matrix moves beyond tolerance | glyph run | rebuild vertices into a bump pool, reset every flush |
| Dear ImGui, most UI toolkits | nothing | — | rebuild the whole vertex buffer every frame |
| Zed / GPUI | nothing | — | per-frame primitive list, one upload |
| cosmic-text + glyphon | CPU shaping/layout per buffer line; **no geometry cache** | per `Buffer` | rebuild the vertex buffer every `prepare()` |
| Mapbox GL | **GPU, resident** | tile — big, static, few, long-lived | draw only; upload on tile load |
| Slug (upstream) | caller's problem | per run, if you want one | you choose |
| sanscale **old** | CPU, **consumer's choice** | row / none | concat + upload |
| sanscale **now** | CPU, service-owned | **fixed at block** | concat + upload |

cosmic-text is the closest peer, and the comparison is instructive in both
directions: it takes **more** than we do (its `Buffer` owns the text as
`Vec<BufferLine>` of `String`s, which `decisions.md` deliberately refused) and
**less** (no geometry cache at all — glyphon rebuilds the vertex buffer on every
`prepare`). Two libraries, two different things pulled behind the API. Neither
took both.

The consensus for *text* is the top group: cache at the glyph and atlas level,
regenerate vertices per frame into transient bump storage. Resident GPU ranges is
the Mapbox row, and it works there because tiles are big and static — not because
it is generally better. Applying it at glyph granularity would be tile
architecture at the wrong scale, plus a real allocator, plus a vertex-format and
shader change.

---

## Was there a halfway point? Yes, and it was written down

`decisions.md` put the geometry pool in **Maybe**, not in the design:

> *We'd add it* because it kills the every-frame re-upload of unchanged paragraphs
> … and — because `draw` is keyed — it slots in later with **no API change**.
> *But maybe not*: compendium re-uploads everything every frame today and it's
> fine, so this is designed-in-shape, deferred-in-fact. **Ship without it, add
> transparently when power actually matters.**

It shipped *with* it. And the sentence that justified deferring it — "it slots in
later with no API change" — is the one that turned out to be false in the way that
mattered. It slotted in with no change of *signature*, which is what that sentence
literally claimed, while changing the invalidation semantics and removing the
consumer's choice of grain. Every regression in this document is downstream of a
pool that the design doc said not to build yet.

The useful generalisation, and the thing to check before doming the next thing:

**Take the pools that have one right answer. Leave the ones that encode a policy.**

- *Has this paragraph changed?* One right answer — its version. Safe to own.
- *Which face covers this character?* One right answer. Safe to own, and owning it
  is what fixed the fallback bug.
- *Is this glyph rasterised at this size?* One right answer. Safe to own.
- *At what grain should quads be cached, and when should they be thrown away?*
  **Policy.** `unicode_zoom` wanted rows, compendium wanted nothing at all, and
  both were right for their workload. Owning it meant picking for them.

The atlas, the shaping cache and the layout cache are facts. Geometry is strategy.
The API took a strategy and offered no way to override it — which is exactly why
the fallout was unanticipated: nothing about the *signature* changed, so there was
nothing to review.

---

## The constraint everything hits

**One batch = one draw call = one scissor rect + one slot in the z-order.**

A grain is therefore not a free knob. A group can only contain blocks that share a
clip *and* are contiguous in draw order. `unicode_zoom`'s rows satisfied this
trivially (no clip, one z). This is also why `draw_batch` takes `&[Draw]` rather
than a block: block is the **layout** unit (paragraphs flowed into one coordinate
space, stacked vertically by `assemble`), a batch is the **draw** unit
(independently placed blocks sharing pass state). Orthogonal.

### And this is where compendium falls over

compendium sets `pass.set_scissor_rect()` around **each** text draw, because a
title and a body have different scissors. There is no group above the individual
item. So:

- it cannot use `draw_batch` at all today — hence one buffer and one draw call per
  text item;
- and a batch cache keyed on "the same `&[Draw]` slice" would give it nothing,
  because every batch would have length one.

Any batching story for compendium therefore depends on **not needing the hardware
scissor per item**.

---

## The unlock: clip in the shader, not the scissor

The service already CPU-culls whole lines and individual glyphs against `clip`
before emitting. The hardware scissor is only doing one remaining job: cutting
glyphs that straddle the clip boundary.

That job can move into the fragment shader — a per-block clip rect, reached the
same way a per-block transform would be (a block id on the vertex indexing a small
per-block array), discarding fragments outside. Then no draw needs its own
scissor, everything sharing a z-slot can batch, and compendium's hundreds of
per-item buffers and draw calls collapse to one.

This is the change that makes batching real for the actual consumer. It is also
the most invasive: vertex format, both shaders, and a per-block array.

---

## Independently: compendium's draw list churns every frame

`renderer.rs:1941` builds each draw as `at = camera.world_to_screen(rect.pos)`,
`size_px = scaled_font_size(spec.size, camera.zoom)`, with `pixel_ortho` as the
transform. So `at`, `size` and `clip` all change on every frame of any camera move.

This needs fixing regardless of anything in this RFC. It is a consumer change, not
a library one — put the camera in the transform and pass world-space `at`, which
is what `set_transform` taking a full 4x4 is *for*, and what both examples already
do. Until then:

| | draw list per frame | geometry cache | batch cache |
|---|---|---|---|
| `unicode_zoom` / `emoji_zoom` | identical (world `at`, camera in transform) | hits | would hit |
| compendium today | changes every frame | hits since `at`/`size` left the key | would miss |
| compendium on world coords + MVP | identical | hits | would hit |

Note the middle row is already better than it was: the geometry cache now survives
a camera move even with screen-space `at`, because `at` and `size` are applied at
concatenation rather than baked. The batch cache would not survive it, because the
`&[Draw]` slice itself differs.

---

## Proposal

| | Do it | Why |
|---|---|---|
| **Arena + `Range<u32>`** replacing the `Vec`-per-block | **yes, first** | Self-contained, no format change. Needs no real allocator: rebuilds fire on colour/clip/bucket changes, which do not alter the vertex *count*, so same-length overwrites in place and the rare length change goes on a size-classed free list. Halves the metadata array too, which is the guaranteed win since that probe is dense. |
| **Cached batch** — `prepare_batch(&[Draw]) -> BatchHandle`, reusing the concatenated buffer while the slice is unchanged | **yes, design** | Restores the grain choice as a *draw* concept rather than a third identity type. Worth ~2 ms on the dense examples immediately. Worth nothing to compendium until the two items below. |
| **Per-block clip in the shader**, retiring the per-item scissor | **yes, but major** | The only thing that makes batching reachable for compendium. Vertex format + shaders + per-block array. |
| **compendium to world-space `at` + MVP** | **yes, consumer-side** | Needed regardless. Unlocks the batch cache; also the honest use of the transform. |
| **GPU-resident per-block ranges** | **no** | Tile architecture at glyph granularity, against consensus for text, and buys little once the batch is cached. |
| **Colour out of the vertex** | **no** | The `decisions.md` lock is stale — written when each geometry entry was its own VBO, where a colour change meant reuploading a buffer. With host-side quads a colour change rebuilds one block's `Vec`, and the per-frame case (selection highlight) is one block, not all. Leaving `col` in the vertex costs 16 bytes and buys simplicity. |

Order: arena → compendium coordinate space → cached batch → shader clip. Each is
useful alone, and each earlier one makes the next easier to evaluate.

---

## Open questions

- Does the cached batch key on slice identity, a consumer-supplied generation, or a
  hash? A hash over 41k `Draw`s is not obviously cheaper than the concatenation it
  saves. A consumer-supplied "nothing changed" flag is cheapest and least safe.
- Per-block clip in the shader means a dependent read per vertex. On a dense frame
  that is 250k reads; needs measuring against the scissor changes it removes.
- Does `BatchHandle` need eviction, and against what bound? It holds a
  concatenated vertex buffer, which is far larger than a `Geometry`.
- Is z-contiguity something the service should verify, or the consumer's
  responsibility to get right? Silently drawing in the wrong order is a bad
  failure mode.

## Not doing

Returning to immediate mode. The geometry cache earns its place for the consumer
it was designed for; the problem is that the grain is fixed, not that it exists.

---

## A note on method

Four predictions in this area have now been wrong until measured: locality (twice),
epoch invalidation, and my own clip normalisation, which was invariant on paper and
missed every frame in floats. Nothing in this RFC should be built on the strength
of its argument. Measure first, and prefer the change that is cheapest to abandon.

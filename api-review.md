# Sanscale API review

Reviewed 2026-09-24 at **`e5c9b03ca07add3418acb4e19bc6168f3c04475a`**.
Scope: all 34 public types/traits/aliases, 61 free/inherent functions, public
fields, variants, and trait contracts in [the reviewed inventory](https://github.com/xpjb/sanscale/blob/712ab2f/public-api.md), checked
against implementation, examples, and Compendium usage. Runtime probes target
composition and lifetime boundaries; this is not a proof of every rendering case.

## Resolution

The eight reproduced contracts and proposed surface cleanup are now implemented.
See the [locked follow-up](decisions.md#api-review-follow-up--owned-gpu-snapshots-and-explicit-carets-locked)
and [current regression tests](tests/api_contracts.rs). The README includes the
[migration and editor integration guide](README.md#editor-integration).
The original report below is historical, not the current release verdict. Its
source links are frozen to the reviewed revision; the standalone probe still
intentionally reproduces the old failures, rather than testing the new API.

## Original verdict

**Keep the architecture; do not publish this revision unchanged.** The main
problem is contracts the implementation does not uphold, not excessive type
count. One service, consumer-owned text and passes, em-space layout, typed carets,
and owned retained batches are the right divisions.

The six issues below should be resolved before 0.1. The two retention contracts
also need explicit recovery guidance. Proposed signature changes and cuts are
recommendations, not newly locked decisions. No library implementation or API was
changed by this review.

## Resolve before publishing

### 1. Per-pass transforms overwrite earlier queued passes

[`set_transform`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L1404-L1414) says to call once per pass, but
[`write_matrix`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/renderer.rs#L388-L392) writes into one persistent uniform
buffer per pipeline. Queue writes execute before the next submission, not at the
position of the setter in recorded commands.

**Reproduced:** encode pass A with matrix A, then pass B with matrix B, and submit
both together. Both use B. A differs from its separately submitted control by
1,840 pixels and exactly matches the B control.

**Recommendation:** retain immutable transform bindings/data for recorded draws,
just as batches already own their vertices. Do not introduce another shared ring
with an unverifiable frame boundary. Requiring a submission between transforms
would be a substantial restriction, not an implementation of the advertised
per-pass contract.

### 2. Changing target format destroys atlas validity without invalidating batches

[`set_target`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L1397-L1402) promises pipelines cached per format.
[`ensure_gpu`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L1794-L1815) actually replaces the entire GPU state,
including both atlases, when the format changes.
[`batch_live`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L1673-L1683) knows nothing about that replacement.

**Reproduced:** prepare visible text for `Rgba8Unorm`, switch to `Bgra8Unorm`, set
the transform again, and draw the retained batch. `batch_live` is **true**, but
the result has **zero** ink pixels. Re-preparing the same draw produces 920.
Resetting the transform in this probe isolates atlas loss from matrix loss.

**Recommendation:** separate format-dependent pipelines from format-independent
atlas ownership. At minimum, replacing GPU state must invalidate affected
batches and force the necessary uploads. Make the pipeline-cache documentation
match the actual policy.

### 3. Batching reverses overlap order across monochrome text and emoji

[`prepare`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L1487-L1492) promises input order is preserved.
[`draw_segment`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L1625-L1650) instead draws all monochrome vertices
before all emoji vertices within an equal-clip segment. Its own documentation
states the opposite policy: text always below emoji.

**Reproduced:** draw an emoji followed by overlapping red monochrome text, both
with the same clip. One combined batch differs from two sequential batches by
1,930 pixels and exactly matches the **reversed** draw order.

**Recommendation:** preserve ordered pipeline runs inside a segment. The consumer
already supplied z-order; this is not a request for a scene graph. The locked
input-order guarantee should win over grouping everything by shader kind.

### 4. Emoji eviction cannot progress through the public service

[`EmojiCache::begin_frame`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/emoji.rs#L154-L159) is never called by
`TextService`. Every cell keeps frame zero, and
[`evict_lru`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/emoji.rs#L272-L284) excludes cells used in the current frame.
Thus nothing becomes an eviction candidate. Allocation failures are also cached
as `None` by [`get_or_insert`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/emoji.rs#L194-L206).

**Reproduced:** display one different 256px emoji per separately submitted,
completed frame, dropping each previous batch. At frame 218, the 8,192-row atlas
has exhausted its 217 cells. The next glyph, U+1F3E0, is blank: one drop, zero
evictions. A fresh service renders the same glyph with 34,788 ink pixels. The
visible working set was one glyph, not 218.

**Recommendation:** resolve atlas reuse ownership before claiming bounded LRU
behavior. Simply calling `begin_frame` in `prepare` is unsafe: multiple prepares
can belong to one submission, and retained batches can still reference slots.
This exposes an unresolved tension with the locked no-frame-object/no-hidden-
leases design. A deliberately append-only-until-reset policy is another choice,
but would need honest limits and usable recovery, not an LRU claim. Temporary
capacity failure must also be distinguished from a permanently unrenderable glyph
so it can be retried.

### 5. Caret re-anchoring can return a caret outside its reported line

[`clamp_caret`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L543-L557) falls back to `line_for_byte`, bypassing
the hard-break correction in [`caret_at`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L564-L578).
The byte-only [`caret_rect`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L488-L490) has the same discrepancy.

**Reproduced with real shaping:** wrap `"ab\ncd"` at 0.7em, place the caret at
byte 2 on visual line 1, then widen to an unwrapped layout. `clamp_caret` keeps
line 1, whose range is **3..5**, for **byte 2**. Correct placement is line 0.
Separately, `caret_rect(2)` returns y=1.1640625em while the rectangle for
`caret_at(2)` is at y=0.

**Recommendation:** use one canonical placement rule for re-anchoring and default
caret geometry. Keep the intentional distinction between normal placement and
end-affine post-edit placement. The typed-caret API should not lead consumers
back through a contradictory byte-only path.

### 6. Chain registration silently aliases another live chain at capacity

[`register_chain`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L1163-L1174) casts an unbounded slot index to
`u16`, without a capacity check or an error return.

**Reproduced:** register 65,536 live Sans chains, then one Mono chain. The new
handle selects the first Sans chain. `"WWWiii"` measures **3.777832em** instead of
the Mono control's **3.6123047em**. No stale handle or invalid input was needed.
This is uncommon, but silent resource aliasing is not an acceptable overflow
policy, and choosing a fallible signature is cheapest before publication.

There is also no generation on a chain handle. Dropping A, registering B into the
freed slot, then dropping the old A again destroys B. That second drop is
**use-after-release by the caller**, not proof that ordinary release is broken;
it demonstrates the weaker safety contract compared with `PaintHandle`.

**Recommendation:** make registration capacity-safe and preferably fallible;
use generation-checked reusable chain slots, or explicitly justify and document
the weaker lifetime rule. State separately that font/chain handles must be
recreated after `clear`, and that all resource handles are service-local.

## Retention contracts needing completion

### 7. Re-prepare alone does not recover an evicted block

The [recovery instruction](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L1662-L1672), also repeated in the README,
says to re-prepare when `batch_live` is false. That works for a reshaped live
handle, but not for an evicted handle.

**Reproduced:** retain a visible label, then shape 131,072 other blocks. Its old
batch becomes stale. Re-preparing its unchanged `Draw` produces an empty batch
that is now considered live. Re-shaping the label first, replacing the draw's
handle, then preparing restores 1,489 ink pixels.

**Recommendation:** distinguish "layout changed" from "handle no longer resolves"
in the recovery guidance. A conservative consumer can re-issue `shape` for its
items, refresh their handles, then prepare. The library need not acquire text
ownership or auto-rebuild inside `draw_prepared` to make this usable.

### 8. Pixel-scale changes bypass the retained emoji refresh path

[`set_pixel_scale`](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/text.rs#L1416-L1428) tells callers to update it alongside
the transform to avoid blurry emoji. Bucket comparison happens in `prepare`,
not `batch_live`. Unchanged world-space draws therefore retain their old bitmap
resolution even when following the documented retained-batch check.

**Reproduced:** retain an emoji prepared at 32px, then apply a 4x transform and
pixel scale. The batch remains live; its image differs from re-preparing the same
draw by 7,790 pixels. This is under-resolution, not invalid UVs or missing ink.

**Recommendation:** have retention validity account for requested emoji buckets,
without invalidating monochrome-only batches on zoom. Alternatively, explicitly
require consumers to re-prepare on pixel-scale changes. Do not imply that the
setter plus the current liveness check performs that refresh.

## Surface decisions

### Changes worth making before 0.1

| Surface | Recommendation | Reason |
|---|---|---|
| `register_chain` | Make allocation fallible/capacity-safe; settle stale-handle semantics. | Finding 6; do not freeze an infallible bounded-resource API accidentally. |
| Caret geometry | Prefer `caret_rect(Caret)` as the canonical query. Retire the overlapping byte/hint entry points where caller migration permits. | `hit_test` and motion already return `Caret`; consumers currently dismantle it into `Some(line), byte`. Finding 5 shows the cost of parallel rules. |
| `caret_position` | Remove this redundant projection if simplifying the caret surface. | It is just the x/y of `caret_rect`; no example or Compendium caller needs it. |
| `FontMetrics` export | Remove the public re-export; keep the implementation type internal. | Its only library producer is the private `Font::metrics`; no public service method exposes it. See [the export](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/lib.rs#L132) and [producer](https://github.com/xpjb/sanscale/blob/e5c9b03ca07add3418acb4e19bc6168f3c04475a/src/font.rs#L85). Do not add an accessor merely to justify the orphaned export. |
| Single-item `draw` | Consider taking one `Draw` instead of duplicating its fields positionally. | The current convenience cannot carry `paint`, unlike `prepare`/`draw_batch`. Same vocabulary, no new type or rendering path. Optional; the existing batch route already supports paint. |

The last proposal deliberately revisits the earlier choice to leave plain
`draw(...)` unchanged during the inline-style migration. It is an ergonomic
recommendation for this review, not a correctness blocker or an unnoticed
compatibility change.

`caret_at`, `caret_after_edit`, and `clamp_caret` are **not** interchangeable
conveniences: they encode different placement events. Keep those distinctions.
A raw `line_for_byte` helper should only remain public if its distinct semantics
are useful and clearly named; most callers want the line of a placed caret.

### What to keep

This covers every exported type family, not just the problem areas:

| Public types/traits/aliases | Assessment |
|---|---|
| `TextService` | Keep one owner. No renderer facade, text buffer, font discovery, or editor session object in core. |
| `Vec2`, `Rect`, `Color`, `Align` | Keep the small value vocabulary and explicit em/source-space/linear-color contracts. |
| `FontData`, `FontHandle`, `FontChainHandle`, `FontError`, `FontMetrics` | Shared byte ownership and explicit fallback chains are right. Fix chain allocation/lifetimes; remove only the orphaned metrics export. |
| `ParagraphKey`, `BlockKey`, `ShapedHandle`, `Style` | Keep distinct paragraph invalidation, mutable block identity, and cached-layout references. Preserve full paragraph identities and bitwise style equality. |
| `ParagraphSource`, `Paragraphs`, `FontSpan` | Keep the borrowed source seam and small slice adapter. Paragraph-local font spans correctly participate in shaping invalidation. |
| `PaintSpan`, `PaintHandle`, `PaintError` | Keep immutable paint snapshots separate from layout. Generation-checked release and baked retained colors are coherent. |
| `Layout`, `LineMetrics`, `LayoutLineSpec`, `CaretStop`, `Caret`, `CaretRect`, `Motion`, `Boundaries`, `SelectionSpan` | Keep GPU-free geometry/navigation, caller-owned goal columns and word semantics. **Keep `from_lines` and its input types**: consumer editor tests genuinely need them. Consolidate caret geometry, not the whole model. |
| `Draw`, `Segment`, `Batch` | Keep named-field literals plus defaults and consumer-owned vertex buffers. Fix ordering and retention contracts rather than adding a second rendering route. |
| `Diagnostics` | Keep coverage and pressure visibility. The other queries have real examples and must use the actual fallback resolver. Moving them out only to expose private font machinery would not simplify the API. Tuple naming can wait. |
| `profiling::WorkCounters` | Keep feature-gated and separate from normal operation; document thread-local scope. No reason to add a general instrumentation framework. |

Two small optional Rust ergonomics improvements: accept `AsRef<Path>` in
`read_font_file`, and allow unsized `Boundaries` implementations in the caret and
word-selection methods. Neither needs a new public abstraction.

### Tighten the consumer contract, without adding features

- Explain service-local handles, invalidation after release/reset, and that a
  `BlockKey` identifies a mutable **layout view**. Simultaneous different layouts
  of the same content need distinct named block keys; paragraph namespaces do not
  namespace block keys.
- State target-before-transform/prepare ordering, single-device GPU ownership,
  and current single-sample/no-depth-testing pipeline restrictions. `clear`
  retains GPU allocations; it is not a device-replacement operation.
- Make current LTR flow and deferred bidi/IME support visible to consumers, not
  only in historical design notes. Likewise, distinguish natural line width
  from wrap-box width and spell out coherent geometry requirements for
  `Layout::from_lines`.
- Keep the existing CPU-culling versus hard-scissor distinction; shader clipping
  is already deliberately deferred. Correct `draw_batch`'s "one pair of draw
  calls" claim: it is presently per segment, and ordered pipeline runs may need
  more. Native emoji ignore foreground paint, including its alpha; it is not a
  general whole-run opacity control.

## Reproduction and checks

[Standalone public-API probe](scripts/api-review-probe/src/main.rs):

```sh
cargo run --release --manifest-path scripts/api-review-probe/Cargo.toml
```

Its manifest pins the reviewed Git revision, **not the changing checkout**.
It requires DejaVu Sans/Mono, Noto Color Emoji, and a headless-capable wgpu adapter.
The font helper tries common Arch/Debian paths; adjust them for other systems.
No windows are opened. `scripts/` is already excluded from the published crate.

The assertions intentionally confirm these baseline failures and their working
controls; this is a forensic reproducer, **not** the future regression suite's
expected behavior. Turn each case into a correct-behavior assertion when fixing
it, including controls for ordinary monochrome retention and multiple batches.

Observed on Vulkan / NVIDIA GTX 1060 6GB:

```text
caret: byte=2 caret_at.line=0 byte_rect.y=1.1640625 placed_rect.y=0
caret reflow: old line=1 clamped line=1 expected line=0 byte=2 returned line range=Some(3..5)
chain reuse: dropping the old chain again destroys new chain = true
chain capacity: 65,537th live chain width=3.777832 expected_mono=3.6123047
transforms: first pass differs from own control by 1840 pixels; equals second transform = true; second pass correct = true
target format: old batch_live=true old ink=0 re-prepared ink=920
mixed-pipeline z-order: differing pixels=1930 matches reversed order=true
eviction recovery: reprepare-only ink=0 new batch_live=true reshape+prepare ink=1489
pixel scale: after 4x zoom old batch_live=true differing pixels from reprepare=7790 old ink=10397 fresh ink=9647
emoji pressure: separate frames=218 next U+1F3E0 dropped=1 evictions=0 current ink=0 fresh ink=34788
```

The pinned standalone probe completed all failure/control assertions. The normal
library suite was re-run with `cargo nextest run --lib --examples --tests
--all-features --run-ignored all --locked`: **90 passed, zero skipped**, including
the GPU lifecycle tests. `cargo doc --no-deps --all-features
--document-private-items --locked` also passed with rustdoc warnings denied.

The passing normal suites do not establish these boundary contracts. The new
probes demonstrate gaps in their coverage, not a reason to discard them.

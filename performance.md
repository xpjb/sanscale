# Pathological performance regressions

The baseline must exist **before** paint/font spans land. The purpose is not one
FPS number: it is to distinguish necessary work, cache hits, and accidentally
broad invalidation. This suite uses the real public API and production cache
limits. It does not change shaping, flow, caching, or rendering algorithms.

Entry points:

- `benches/pathological.rs`: deterministic, headless scenarios and raw JSON.
- `scripts/run-perf.sh`: separate timing and instrumented runs, then a dashboard.
- `scripts/perf-report.py`: baseline visualization or before/after comparison.
- Optional `perf-counters` feature / `sanscale::profiling`: thread-local work
  counters. All probes, including their argument evaluation, compile out normally.

## Running and keeping a baseline

```sh
# Full quick matrix, including a real offscreen GPU render path. No window.
scripts/run-perf.sh perf-results/before --tier quick --gpu --samples 21 --warmup 3

# After changing the implementation, on the same machine and fonts:
scripts/run-perf.sh perf-results/after --tier quick --gpu --samples 21 --warmup 3
python3 scripts/perf-report.py perf-results/before-timing.json perf-results/after-timing.json \
  --before-work perf-results/before-work.json --after-work perf-results/after-work.json \
  --html perf-results/comparison.html

# List / isolate cases. The selector is a substring, not a regex.
cargo bench --bench pathological -- --gpu --list
scripts/run-perf.sh perf-results/long --tier stress --only cpu.long.unbroken.fresh_layout --samples 3 --warmup 1
scripts/run-perf.sh perf-results/grid --tier stress --gpu --only gpu.unicode --samples 3 --warmup 2
scripts/run-perf.sh perf-results/paragraphs --tier standard --only cpu.paragraphs --samples 9 --warmup 2
```

Omit `--gpu` for CPU-only runs. The wrapper writes `PREFIX-timing.json`,
`PREFIX-work.json`, `PREFIX.html`, and a copy of the resolved `Cargo.lock`.
Generated results are ignored, not committed as universal speed requirements.
Archive them with the source revision under investigation. A baseline-only HTML
file is a dashboard, **not evidence of a before/after feature comparison**.

### Fixtures and environment

The default fixture candidates are DejaVu Sans regular/oblique, Noto Sans CJK,
Noto Color Emoji, and Noto Sans Devanagari in common Linux install locations.
Override with `--latin-font`, `--italic-font`, `--cjk-font`, `--emoji-font`, and
`--indic-font`. Collection face index is currently explicitly **0**. Missing
fixtures fail with instructions; there is no silent font substitution.

Every report includes exact font and corpus SHA-256s, source/harness/executable
fingerprints (including shader sources), git status/revision, dependency-lock hash, Rust compiler, CPU,
OS/kernel, and (when enabled) GPU/driver/target details. The Unicode generator
selects genuinely covered, distinct scalars from the recorded CJK font (adding
precomposed Hangul and spacing compatibility ideographs/radicals for large grids). It fails
rather than filling a large grid with repeated tofu. Work runs reject `.notdef`
glyphs, dropped emoji, and accidentally command-free rendered frames.

Compare the same tier, operation/sample counts, warmup, fixtures, compiler, dependencies,
and GPU. Source hashes snapshot the files at run time; the executable hash identifies
the actual binary. Do not edit/rebuild sources during a capture. The report refuses
environment or workload mismatches unless the
applicable explicit environment override is given; changed harness source is
flagged for review. The wrapper's lock-file copy matters because this library
normally ignores Cargo.lock. Do not run another CPU/GPU-heavy job alongside a
capture. For serious investigations also control power policy, clocks, thermal
state, and process affinity externally; the tool does not silently change them.

## What the measurements mean

**Two builds, not one instrumented timing run:**

1. Default release/bench build supplies latency samples.
2. `--features perf-counters` supplies work and Rust heap accounting. Its timings
   are deliberately not substituted into the production-timing dashboard.

Normal builds have no TLS probes, counter storage, or benchmark allocator.
Instrumented work counters aggregate service calls on the **calling thread**;
Rust allocation accounting covers **all process threads**. The latter can include
background Rust backend work, but excludes native-driver allocations, GPU memory,
and allocator overhead/fragmentation. It is requested heap memory, not RSS.
A successful realloc counts as an old logical free and a new allocation request,
even if the underlying allocator grew it in place.

Reports show per-operation p50/p95, raw samples, and expandable work/allocation
details. Cases that repeat a cheap operation within one sample state their unit
count; times and counters are normalized by that count. Microbatched hit/query
cases therefore show per-op averages within a sample, not individual-call tail
latencies. Counter/allocation tables include min/max and raw samples so rare
sweeps or rasterizations do not disappear behind a zero median. Few samples do not produce
a trustworthy tail estimate: with fewer than 20, nearest-rank p95 is usually the
maximum. Default timing flags (>10% and at least 1 us increase) are investigation
prompts. `--fail` makes those flags an opt-in exit status, not a recommended flaky
shared-runner CI gate. Repeat baseline/candidate captures to establish noise.

GPU cases use a fixed offscreen target, real queue submission, and completed serial
frames, not a growing asynchronous command backlog. Where supported, timestamps
measure **render-pass GPU duration**, including clearing the target, not uploads.
CPU preparation, command encoding, submission, and fence wait are also reported.
Wait is not mislabeled GPU time. The total includes completion/query readback;
nested phases must not be added to it. For `draw_individual` and transient helpers,
preparation occurs inside the encode phase. The document-edit case's prepare phase
also includes `shape`; its work counters separate the operations.

Font discovery/I/O, corpus generation, key/text edit setup, pipeline creation, and
serialization are outside timed scopes. Cold glyph/atlas cases deliberately use
fresh services per sample, even after warmup; they do not accidentally become hot
benchmarks. This is a library suite, not an editor keystroke benchmark: Rope,
parser, undo, and UI/event-loop costs are not included.

## Sizes and pathological families

| Tier | Distinct Unicode scalars | Long paragraph bytes, approximately | Paragraphs in large groups | Repeated-label calls |
|---|---:|---:|---:|---:|
| quick | 2,048 | 8,192 | 256 | 1,024 |
| standard | 16,384 | 65,536 | 4,096 | 8,192 |
| stress | 41,472 | 1,000,000 | 32,768 | 41,472 |

**Stress is deliberately expensive.** Use selectors first. In particular, the
current many-word flow algorithm examines the full glyph list for each token;
a million-byte paragraph containing tiny words is qualitatively more expensive
than a million-byte unbroken token. Do not interpret a long run as a deadlock,
shrink its corpus silently, or combine these cases into a single average.

### CPU matrix implemented now

| Family | Variants / operation | What it separates |
|---|---|---|
| Long paragraph | Unbroken token: wrapped, unwrapped, very narrow wrap | Main shaping versus line/caret population and allocation |
| Long paragraph | Many one-character words versus one token of similar bytes | Tokenization-dependent flow complexity; `flow_glyph_tests` counts actual scan volume arithmetically |
| Long paragraph | Whitespace only | No ink is not no shaping/caret work; outline-less glyph cache misses remain visible |
| Unicode complexity | Combining sequences, alternating Latin/CJK faces, Devanagari, repeated ZWJ emoji | Grapheme handling, number of actual face runs, and auxiliary fallback shaping |
| One-byte edit | Beginning, middle, end of the same large paragraph | Full-paragraph reshaping and rebuild cost despite a tiny change |
| Paragraph groups | 1, intermediate, large; unchanged block | Parts-slice comparison without source reads or assembly |
| Paragraph groups | New block with the same cached paragraphs | Cache hits followed by full block assembly/copying |
| Paragraph groups | Edit first/middle/last versus invalidate-all control | Necessary paragraph work versus gratuitous document-wide invalidation |
| Structure | Insert/delete at the front, split/merge in the middle, many empty paragraphs | Stable paragraph IDs, byte rebasing, caret/assembly overhead without assuming every change has the same byte length |
| Width/style | Fresh widths versus an explicitly prewarmed width cycle, for a single paragraph **and whole paragraph groups** | Width misses reflow cached glyphs; cached-width hits do neither. Group sweeps vary widths substantially and give every fresh width a unique value; all three cached variants are prewarmed |
| Width/style | Fresh line spacing; alignment-only miss | Style-only misses reuse shaped glyphs but still rebuild flowed paragraphs and blocks |
| Font chains | Cached regular/italic toggle; fresh-layout toggle; distinct chains sharing actual faces | Paragraph-style reuse versus re-shaping with shared outline atlas entries; not a substitute for future inline-span cases |
| Fallback chain | Same text with a deliberately long fallback chain | Coverage-probe cost independently of atlas misses |
| Transient labels | Cached content-keyed text | Hash/split overhead versus consumer identity lookup |
| Repeated labels | Distinct blocks sharing a paragraph versus the same block repeatedly | Paragraph hits/assembly versus complete block hits |
| Unicode grid shaping | 128, intermediate, maximum scalars; glyph-sized blocks versus 256-scalar rows; cold versus warmed glyphs | Run/paragraph/block count independently of number of distinct outlines |
| Editor queries | Bottom hit-test, near-end caret, whole selection, near-end vertical motion | Query traversal and allocation after layout already exists |
| Capacity (stress only) | 131,072 + 1,024 live blocks or paragraph revisions | Real production pool eviction/sweeps and allocation, without test-only reduced limits |

Font-span-only updates now have separate `cpu.spans.*` cases; the original
full-chain controls remain explicitly labeled, not presented as equivalent. The insert/split/merge fixtures have stable
pool slots independent of paragraph order, so inserting at the top does not
manufacture new identities for every later paragraph.

### GPU matrix implemented now

The Unicode grid uses the **same scalar corpus and placement** with one block per
glyph or one block per 256-scalar row. At the stress size that is 41,472 individual
blocks versus 162 rows. Size, target, glyphs, and font are held constant for this
comparison; grouping does not magically remove glyphs or substitute Latin text.

- `prepare_warm`: cached CPU geometry still copied/uploaded into a new Batch.
- `draw_retained`: record the existing Batch; no shaping, geometry rebuild, or
  vertex/atlas uploads. Includes the batch-liveness check.
- `draw_transient_batch` versus `draw_individual`: one batch route versus thousands
  of prepare/buffer/draw operations. Both submit and complete real GPU work.
- `camera_transform`: update only the matrix and reuse a retained batch. Uniform
  writes are counted separately from vertex and atlas uploads.
- Recolor, move, and scale: show exactly which changes hit em-space geometry and
  which require new vertices. These are current whole-block colors, not spans.
- Clip following an item versus scrolling under a fixed clip: normalized geometry
  keys versus changed visible content. Hardware scissors are actually applied.
- Alternating clips: segment/draw-count cost, not just vertex counts.
- Alternating color variants of the same blocks: the current one-geometry-variant
  cache can thrash even though shaping and atlases remain warm.
- Cold geometry and first atlas upload: shaping and pipeline startup are excluded.
- Top/middle/bottom document views with one paragraph edited: assembly, visible
  geometry, and entire-line traversal when most paragraphs are offscreen.
- Very long horizontal line: small visible fraction while scrolling, versus the
  retained-frame control. One line can still require visiting every glyph.
- Emoji: cold raster, same raster bucket, and crossing between buckets. Warmup can
  populate both crossed buckets: CPU geometry now stores logical emoji requests,
  so changing a bucket resolves pages/emits quads without rebuilding that geometry.

### Counter layers

`profiling::WorkCounters` records block requests/hits, paragraph requests/hits,
source reads/bytes, main shaping calls/runs/glyphs, auxiliary fallback shapings,
coverage queries, outline/band hits/misses/inserts, flow calls/tokens/glyph tests,
assembly copies, paragraph/block evictions, geometry hits/rebuilds and visibility
walks, prepare items/segments, batch-buffer allocation and vertex bytes, actual
pipeline draw commands, atlas allocations/uploads, uniform bytes, and emoji
raster/cache/eviction/drop work. Counters describe what executed, not estimates
inferred from names or elapsed time.

Instrumented cases also assert relevant upper bounds: e.g. a block hit reads no
source, a one-paragraph edit does not reshape every paragraph, recoloring does not
shape or flow, a retained batch does not prepare/upload, and a movement whose
normalized clip is unchanged does not rebuild geometry. These guards permit less
work after optimization; they do not enshrine today's avoidable work as required.

## Span-specific coverage and remaining axes

The C-editor milestone adds (without replacing the legacy matrix):

- `cpu.spans.empty`, `whole`, `dense_equivalent`, `dense_alternating`: no spans,
  one italic span, per-byte equivalent faces (must coalesce), and alternating faces.
- `cpu.spans.font_edit_middle`: a three-byte font change in a long paragraph; source
  bytes are unchanged but the shaping generation changes. This currently reshapes
  and flows the entire paragraph, not just those three bytes.
- `cpu.spans.paint_register_drop.*`: explicit pool churn at sparse/dense sizes.
- `gpu.spans.paint_sparse_recolor` / `paint_dense_recolor`: foreground updates with
  zero shape/flow/outline work, through actual completed GPU frames.
- `gpu.spans.paint_retained` / `paint_released_retained`: no prepare or upload on
  retained draws. The latter also verifies a stale snapshot cannot revive cached
  geometry on a new prepare, while previously prepared pixels remain valid.

Editor tests independently check lexical propagation, file namespaces, font-span
resolution, grapheme safety, and zero lexing/source dirtiness on both theme toggles.
These are deterministic adapter-work checks, not a claim to have measured full
interactive keystroke latency. The following broader matrix remains a checklist;
not every combination below is implemented, and absent cases are never zero-cost
results:

| Axis | Required variants |
|---|---|
| Plain-path regression | No paint handle / empty font spans, identical legacy workloads and emitted glyph counts |
| Paint density | Empty snapshot, one whole-block span, sparse token spans, alternating per-cluster spans; 1 / 100 / 10,000+ ranges |
| Paint lookup locality | First, middle, last visible range; most spans offscreen; long line and many paragraphs. Detect glyph-count × span-count scans |
| Paint changes | One range recolored, whole theme, identical resolved result, a new equal-content handle, unchanged retained Batch |
| Paint ownership | Register/drop/recycle churn, shared handle, alternating snapshots, stale handles; bounded memory and no global invalidation |
| Font-span density | One face, alternating regular/italic faces, adjacent equal spans that should coalesce, distinct chains resolving to the same actual face |
| Font-only changes | Tiny range at beginning/middle/end of huge paragraphs; one paragraph in a huge group; nested bold/italic; repeated toggles back to cached inputs |
| Unicode boundaries | Combining text, ligatures, ZWJ emoji, fallback-heavy content at span boundaries; invalid boundaries belong in correctness tests, not misleading fast timings |
| Range rebasing | Insert text/newlines at document start; split/merge paragraphs. Report span copying/rebasing and pool churn separately from shaping |
| Invalidation scope | Drop a chain used only inside a span; unaffected blocks remain cached; one paint change must not invalidate unrelated batches |
| Styling adapter | C lexer/highlighter after text edit versus theme-only restyle. Cache semantic tokens; time parser and renderer separately |
| Markdown tables | Edit a cell without changing widths; change a column constraint and reflow dependents; row-height-only movement; stable block/cell identities |

Preserve existing cases verbatim where possible. Adding `Draw.paint: None` must
not replace the old workload with a different batching strategy and hide a
regression. If the harness itself must change, retain/report that change.

## Known coverage limits and follow-ups

- No synthetic monospace shaper, extra shaping cache, or tiny-capacity alternate
  engine is introduced to make a benchmark look good.
- Public-service emoji pressure is covered for **correctness**, not steady-state
  latency, in `tests/api_contracts.rs`: a batch exceeds the 64 MiB cache budget,
  every glyph is pixel-compared with separate draws, buckets churn, and retained
  and pre-encoded/dropped batches survive eviction. No private frame hooks are
  needed. Add a dedicated pressure timing case before claiming churn latency.
- GPU timestamps and allocator instrumentation do not expose hardware memory
  residency or driver-internal allocation. Add external GPU/RSS tools when those
  are the hypothesis, and record their overhead separately.
- This is a finite, extensible regression matrix, not proof that every possible
  text/font/device/input combination is fast or correct. Keep visual/Unicode
  correctness tests alongside it; fewer drawn glyphs are not an optimization.

When a feature accepts extra invalidation for simpler bookkeeping, record the
precise affected cases and counters here or in its implementation report, following
[rfc-inline-styles.md](rfc-inline-styles.md). Do not hide it in an aggregate score.

### C-editor milestone check

`after-editor-{timing,work}.json` captures 112 quick cases (21 samples, 3 warmups)
with real offscreen GPU frames. `editor-comparison.html` compares the 101 unchanged
legacy workloads against `before-spans`, with matching environment/input metadata.
No legacy p50 triggered the configured **>10% and >=1 µs** timing flag; every common
work-counter sample was identical. This is one local regression check, not a speedup
claim or a rerun of every large stress case. The 11 new cases have no pre-feature
equivalent and are shown separately in `after-editor.html`.

Concrete new work checks: 8,192 equivalent face spans still produce one shaping
run, while alternating actual faces produce 8,192 runs. Sparse/dense paint updates
perform no shaping/flow and visit 2,048 glyphs; interval searches are 64 / 2,048.
Retained paint draws, including after snapshot release, do zero prepare/uploads.
On this 64-bit build `Draw` grows from 56 to 64 bytes and `TextService` from 1,200
to 1,248; per-line origins and paragraph/block dependency lists also add metadata.
These are real storage costs even where the work counters remain unchanged.


### API lifetime follow-up costs

Emoji cache ownership is limited to 64 MiB of small append-only GPU pages;
`Diagnostics::emoji_cache_usage` excludes pages retained only by batches/commands.
There is no full CPU pixel sheet or full-atlas upload on eviction. A new glyph
uploads only its cell. A retained batch does no page lookup or upload; preparing
native geometry resolves glyph requests again and emits quads, including when a
raster bucket changes. Cache hits need no rasterization or texture upload.

Each changed matrix creates one immutable 64-byte uniform binding shared by both
pipelines, instead of overwriting two shared uniforms. Consecutive equal matrices
are free. Ordered text/emoji/page transitions can increase draw calls; adjacent
plain text still coalesces. These are ownership/order costs, not changes to the
plain text shaping/flow algorithm. The pressure correctness test deliberately
keeps more pages alive than the cache budget, which is not a total-process GPU
memory ceiling. Callers control retained-batch lifetimes.


A local before/after capture against `712ab2f` ran all 112 quick cases with 21
samples and 3 warmups, separate timing/work builds, identical locks/fonts/device,
and only necessary harness API migration. No p50 exceeded the report's combined
>10% and >=1 µs regression threshold. This is one local check, not a general
speedup claim or pressure-latency measurement. Camera-transform completed-frame
p50 was 251.93 → 254.54 µs for 2,048 blocks; immutable bindings add real allocation
work while uniform bytes fall from 128 to 64. Cold three-glyph emoji uploads fall
from 278,528 to 12,288 bytes. Prewarmed cross-bucket prepares do zero geometry
rebuilds (formerly one), with zero rerasterization/uploads in both versions.
All six example dump paths also completed; the 3,944-emoji board sweep reported
66,908,160 cache-owned bytes and zero raster failures.

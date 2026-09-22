# Experimental incremental Markdown engine

This is the reusable part of **`markdown-editor`**, not a new responsibility of
`TextService`. It is example-local while its dialect and API settle:

- `mod.rs`: authoritative LF-based Rope, stable physical-line identities, block/
  cell model, UTF-8 edits, streaming, bounded change history and work counters.
- `parse.rs`: custom line-state block parser; no parser framework dependency.
- `inline.rs`: immutable projected UTF-8, semantic runs, and explicit source maps.
- `../preview.rs`: a separate, window-independent **sanscale adapter**. Takes font
  handles, resolves spans, retains cell layouts/paint, and returns glyph draws
  plus under/over rectangles. Does not discover fonts or own a window/pass.
- `../fonts.rs`, the example executable and its sample files: demo/application
  policy only. No part of this feature is added to `src/` or its runtime dependencies.

This is not yet an importable/stable sanscale API. The intended extraction is an
optional companion component, not making all text users depend on Markdown, Rope,
font discovery, or UI infrastructure. Other consumers need not implement an editor:
`Document` + `Preview` are also usable as the model/view for an agent message.

## Streaming and editing contract

```rust
let mut message = markdown::Document::new("");
message.append("| Task | State |\n| --- | --- |\n")?;
message.append("| Parse | **working")?; // a partial row is valid input
message.append("** |\n")?;
message.edit(source_byte_range, replacement)?; // arbitrary UTF-8-boundary edits

// Or consume byte-oriented transport chunks, including split UTF-8 scalars:
let mut stream = markdown::Stream::default();
stream.push(&mut message, bytes)?;
stream.finish()?;
```

`edit` rejects invalid byte boundaries/ranges without changing the source. A byte
stream buffers incomplete UTF-8; invalid chunks are rejected transactionally, and
`finish` reports a truncated scalar. It does not silently introduce replacement
characters. The engine preserves input; the UI normalizes CRLF/CR to LF on file
load/paste. Transport decoding/newline policy belongs outside the parser.

Every prefix has the same interpretation as a fresh parse of that prefix. There
is no timer-based/final-only alternative grammar. Incomplete emphasis/links stay
literal until completed; unclosed fences are code. A paragraph becomes a table
only when a complete, matching delimiter row arrives. Tests compare every scalar
prefix, several byte chunkings, and randomized middle edits against cold parsing.
Those differential tests establish incremental equivalence, **not CommonMark
conformance**; separate golden tests pin the supported syntax.

Untouched physical lines keep their IDs. A table cell is identified by its row's
line ID and column, not an absolute document byte offset. The touched first line
keeps its ID on a split/merge; newly created lines get fresh IDs. This is not a
content-matching diff which guesses identity among duplicate rows. Inserting at a
line boundary may reidentify that touched line's moved content; untouched suffix
lines/cells still retain identity. The model is read-only to clients; edits go
through `Document` so revisions cannot silently diverge from parsed contents.

## Dialect: useful, explicit, not a standards-compliance claim

Implemented:

- Paragraphs and soft breaks; two trailing spaces or backslash-newline hard breaks.
- ATX headings and single-physical-line setext headings (`=` / at least three `-`).
- Fenced code (backticks/tildes, matching marker and sufficient closing length),
  including unfinished streaming fences. Code is rendered per physical line.
- Flat ordered/unordered/task items with indented text continuation, quote lines
  grouped at the same depth, and thematic rules. Nested list indentation is visual;
  this is not a recursive CommonMark container grammar.
- Nested `*` / `_` emphasis and strong emphasis (including combined faces), code
  spans, strikethrough, inline links, and images **as alt text only**.
- Backslash punctuation escapes; `amp`, `lt`, `gt`, `quot`, `apos`, `nbsp` and
  numeric entities. Unknown named entities stay literal.
- GFM-style pipe tables, optional outer pipes, left/center/right alignment,
  escaped pipes, empty/padded cells, and ignored excess body cells. Up to 64
  columns. Pipes inside matched code spans also stay in a cell (a deliberate
  extension; strict GFM requires escaping those pipes). Table body lines must
  contain a pipe; a non-pipe line ends the table.

Not implemented: full CommonMark/GFM delimiter rules (including the rule of three),
full Unicode punctuation classification (currently a heuristic),
reference links/definitions, bare-URL autolinks, HTML interpretation, nested block
containers, indented code blocks, footnotes, math, image loading, or embedded media.
HTML is literal text, not executable content. Links are styled but passive; the
example's click action locates source, not opening arbitrary URLs. Tabs do not gain
real tab stops—the underlying text engine's existing limitation still applies.

## What is incremental

1. Restart classification one line before an edit (tables/setext need lookahead).
   Carry fence/table/paragraph state until an untouched suffix has the same state.
   Removing a fence or separator can legitimately propagate far down the message.
2. Reconcile affected blocks. Ordinary active paragraphs are re-projected as a
   unit; do **not** claim constant-time appends to a million-byte paragraph.
3. Table-body and code-body fast paths splice only touched rows/lines. A changed
   row is scanned as a whole, but unchanged cell projections and IDs survive.
   Completed table/fence prefixes are not copied/reparsed on each tail append.
4. Keep the last 64 change records. Multiple updates before a frame are replayed;
   a lagging/new view reconciles the current model instead of missing changes.
5. The adapter resolves only dirty rows/blocks. Equal projected text and resolved
   font spans preserve shaping identity; color-only changes update paint without
   parsing, shaping, or flow. Source maps can change without changing glyphs.

A 2,000-row table test pins one projected cell / one layout request for an ordinary
cell edit, and one new row's cells for an append. A changed row height updates a
prefix-sum tree; it does not rewrite every later row's Y coordinate. Visible-row
lookup uses that tree, so drawing a tall table visits visible rows, not every cell.
A table height change repositions later blocks without reshaping their text.

### Stable table width policy

Columns are equal-width, using the larger of the viewport width and **6 em per
column**. Header/schema or viewport/font-size changes can change those widths;
streamed body content cannot. Long content wraps inside its existing cell. Wide
tables overflow horizontally; Shift+wheel pans the preview. This intentionally
chooses streaming stability over a content-autosizing algorithm which makes
completed rows jump as longer values arrive. Intrinsic/user-controlled column
sizing is future policy, not silently inferred from a growing suffix.

### Broad work still present

- Middle line/row insertion/deletion moves suffix metadata. Row-count changes in
  the middle rebuild the row-height index; text layouts remain independently keyed.
- Header/schema/structural changes use broader block reconciliation, including
  scanning/copying table row inputs. Unchanged cell projections still survive.
- Active paragraphs re-project as a whole. A huge single table row is still a huge
  line to rescan. Code-block layout updates visit its line metadata, although only
  changed code lines require new text layouts.
- On model changes the adapter walks top-level block metadata to place blocks;
  scene collection also visits block metadata. Only table **rows/cells** have the
  indexed visibility path. Global palette/font changes visit affected text nodes.
- Initial layout measures all cells to know row heights. New widths reflow
  affected cells but reuse their cached shaped glyphs.
- The source pane is one composed text block. Editing it still reassembles that
  source block. The preview/message component does not require this source pane.
- The demo retains one text batch per pane/chrome. Changed draw inputs re-prepare
  the requested visible batch, not just changed vertex subranges. Scrolling or
  shifting later blocks can require uploads even though text is not reshaped.
  Rectangle overlays are rebuilt separately; “zero text upload” is not “zero GPU
  work”. No new cache or Markdown-specific optimization was added to sanscale.
- Source storage, line strings, raw element input and projected text consume real
  memory. Projections are shared with the adapter via `Arc`, not copied each frame.

## Adapter ownership and mapping

```rust
let mut view = preview::Preview::new(42); // reserve a unique namespace
view.sync(&message, &mut text, faces, theme, width_px, font_px);
let scene = view.scene(&mut text, &message, viewport, scroll);
// Prepare scene.draws; render scene.under, glyphs, then scene.over, with a scissor.
// Compare draw inputs yourself, and call text.batch_live before retaining a Batch.
view.release(&mut text); // release owned paint snapshots before discarding a view
```

The namespace reserves keys `(namespace << 32) | element_id`; use distinct
nonzero, non-maximum namespaces for simultaneous views/other service clients.
Font handles and a view remain local to one `TextService`, and handles must stay
valid. Reusing a view for a different Document automatically reconciles ownership;
shaping generations never reset to an old document's values. This prototype
expects usable fonts and service capacity; fallible capacity-aware view APIs are
still part of stabilization, not a claim of unbounded resource support.

Font spans are resolved on projected **grapheme** boundaries, including after
entities/removed delimiters bring combining characters together. Hard breaks are
split into real sanscale paragraphs; LF is not shaped as a pretend glyph. Inline
paint uses projected block-local bytes. Bold+italic chooses an actual combined
face, and base-style line metrics retain sanscale's existing policy.

Source mapping is explicit: projected segments point back into raw element input,
which points to stable line IDs + byte columns. Entity decoding, escapes, code
trimming, line prefixes and hidden markup are not handled by subtracting a fixed
number of delimiter bytes. The split-view demo uses these maps for click-to-source;
preview selection/copy and bidirectional range mapping are future work.

## Exercise it

```sh
cargo nextest run --example markdown-editor
cargo nextest run --example markdown-editor --features perf-counters
cargo run --release --example markdown-editor -- --dump
cargo run --release --features perf-counters --example markdown-editor -- --dump --stream
mkdir -p perf-results
cargo run --release --example markdown-editor -- --bench > perf-results/markdown-cpu.json
```

With `perf-counters`, both dump modes also render a second completed GPU frame and
assert zero shaping, flow, prepare calls and text-vertex uploads while recording
real glyph draws. CPU tests cover palette changes, combined faces/graphemes,
source mapping, row-height propagation, view/document replacement, lagged history,
randomized layout reconciliation, and prefix/edit equivalence.

`--bench` is a small **CPU-only diagnostic**, with 3 warmups and 11 samples over a
2,000-row / three-column table. It checks append/edit work and real paint changes,
and includes uncached widths and a full-parse control. Font discovery/setup are
untimed. Source-location lookup is untimed for the cell edit; the full-parse control
receives an already materialized source string. The main library's fingerprinted
before/after suite is separate; do not treat this probe as end-to-end latency or
compare instrumented durations against production builds.

The UI is a prototype: no undo/redo, IME, bidi, unsaved-change confirmation,
preview selection, or resizable split divider. F5 appends a demo reply rather than
replacing the user's document; ordinary source edits pause that demo stream. There
is no network connection or automatic save.

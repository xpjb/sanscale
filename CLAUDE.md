# sanscale — working notes for Claude

## Docs are part of the change, not a follow-up

This repo carries more prose than code in places, and it is load-bearing: `decisions.md` is
what stops a settled question from being re-litigated, and the README is the only thing a
consumer reads before depending on the crate. **A change that makes a doc wrong is not
finished.** Update the docs in the same commit as the code.

The doc set, and what each one has to stay true to:

| file | tracks | goes stale when |
|---|---|---|
| `README.md` | the public surface as a consumer meets it | a type is renamed or a module added/removed — Quick start, the pipeline diagram *and* the module table each name real identifiers |
| `src/lib.rs` | the crate-level model + the one doctest | the surface moves, or a dependency's major version bumps (`Compatibility`) |
| `decisions.md` | what the design is and why; its *API sketch* tracks `src/text.rs` | a signature, handle width or pool changes; or a decision is overtaken by what got built |
| `backlog.md` | what's owed, and why it's parked | an item is picked up, or its stated reason stops holding |
| `rfc-batch-cache.md` | who owns vertex buffers: the settled `Batch` design, in three parts | a part gets built — fold it into `decisions.md` and drop it from here; the file goes when all three are done or dropped |

Specifics worth knowing before editing:

- **The wgpu version appears in three places** — `Cargo.toml`, the `# Compatibility` section in
  `src/lib.rs`, and the badge line in `README.md`. A bump that updates fewer than three is a
  bug; this has happened.
- **`decisions.md` is a record, so don't rewrite history.** When the build overtakes a
  decision, **append** to that entry — what changed, and what the entry got wrong or right —
  rather than editing it to look as though it was always correct. The existing "Half-built, and
  the missing half has a cost" note under the `clip` lock is the model to follow. The reason for
  the reversal is usually the valuable part; keep it.
- **`decisions.md` sections have different statuses.** *Locked* / *Cutting* are settled, *Maybe*
  and *Up for litigation* are open, and struck-through headings under litigation mean settled —
  read the status before treating a line as current.
- The README's **Slug atlas invariant** section documents a real correctness constraint enforced
  by `cache::alloc_bands` and pinned by `cache::tests::band_runs_never_straddle_a_texture_row`.
  Don't paraphrase it loosely.

## Checks

```bash
cargo test && cargo doc --no-deps --document-private-items
```

`cargo doc` is not optional after a doc edit: intra-doc links to renamed or private items fail
there and nowhere else. The doctest in `src/lib.rs` is compiled by `cargo test`, so it cannot
drift silently — the README's code block **is not**, and needs reading against the real
signatures by hand.

## Examples

Headless (`hello_png`, `paragraph`, `unicode`) write PNGs; `unicode_zoom` and `emoji_zoom` open a
window. Don't launch the windowed ones — pass `-- --dump` for PNG stills instead.

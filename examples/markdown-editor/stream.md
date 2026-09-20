

## Agent · live progress

Here's the next batch. The table is arriving **a few characters at a time**;
completed rows keep their identities and their column widths.

| Task | Result | Detail |
| :--- | :---: | --- |
| Inspect | **done** | Read the interface and ownership contract. |
| Parse | **done** | Keep markup separate from projected UTF-8. |
| Reuse | **done** | Repaint without re-lexing or reshaping. |
| Stream | *active* | Partial rows stay visible while more text arrives. |
| Unicode | `✓` | café, 世界, and 🌍 survive transport boundaries. |
| Measure | **done** | A growing cell updates its row, not every other cell. |
| Finish | **ready** | The message is still editable after the stream ends. |

> An unfinished fence is code until it closes, not a reason to reset the message.

```text
append delta → reclassify frontier → project changed cells
             → update row heights → draw visible rows
```

That's the first cut: a small, **inspectable** streaming Markdown engine.

# An answer, still arriving

A **stream-friendly** Markdown view with *real italics*,
***both faces together***, and `inline code`.

## A plan you can watch grow

| Step | Status | Why it matters |
| :--- | :---: | --- |
| Parse | **Ready** | Keep completed blocks stable. |
| Tables | *Streaming* | A new row doesn't resize old columns. |
| Render | `retained` | Reuse unchanged text layouts. |

> Edit the source on the left. Click preview text to jump
> back through its source map — even after &amp; entities.

- [x] Separate parser, projection, and sanscale adapter
- [x] Bold, italic, links, and tables
- [ ] A full CommonMark implementation (not claimed)

## Small pieces, useful boundaries

```rust
message.append("| new | row | notes |\n")?;
preview.sync(&message, &mut text, faces, theme,
             available_width, font_size);
```

Try **F2** for colors, **F3** for italic faces,
or **F5** to append a simulated agent response.
[Source mapping](https://example.invalid) is explicit;
~~discard all cached layout~~ is not the default.

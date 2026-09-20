use super::*;
fn snapshot(d: &Document) -> Vec<String> {
    d.blocks
        .iter()
        .map(|b| match &b.content {
            Content::Text { kind, element } => {
                format!("{kind:?}:{:?}:{:?}", element.rich.text, element.rich.runs)
            }
            Content::Code {
                language,
                closed,
                lines,
            } => format!(
                "code:{language}:{closed}:{:?}",
                lines.iter().map(|e| &e.rich.text).collect::<Vec<_>>()
            ),
            Content::Table(t) => format!(
                "table:{:?}:{:?}",
                t.align,
                t.rows
                    .iter()
                    .map(|r| r
                        .cells
                        .iter()
                        .map(|e| (&e.rich.text, &e.rich.runs))
                        .collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            ),
            Content::Rule => "rule".into(),
        })
        .collect()
}
fn assert_cold(d: &Document) {
    let text = d.source.to_string();
    let cold = Document::new(&text);
    assert_eq!(snapshot(d), snapshot(&cold), "source: {text:?}");
    assert_eq!(
        d.blocks.iter().map(|b| b.lines.clone()).collect::<Vec<_>>(),
        cold.blocks
            .iter()
            .map(|b| b.lines.clone())
            .collect::<Vec<_>>(),
        "block ranges: {text:?}"
    );
    for block in &d.blocks {
        for e in block.elements() {
            for byte in e
                .rich
                .text
                .char_indices()
                .map(|(i, _)| i)
                .chain([e.rich.text.len()])
            {
                let at = d.resolve(e.origin_at(byte)).unwrap();
                assert!(text.is_char_boundary(at));
            }
        }
    }
}
const SAMPLE: &str = "# Title\n\nA **bold _italic_** &amp; [link](https://example.com).\nStill one paragraph.\n\n| Name | Value | Note |\n| :--- | ---: | :---: |\n| α | 12 | `a|b` |\n| β | 99 | café 🌍 |\n\n```c\nint main(void) {\n  return 0;\n}\n```\n\n- [x] Done\n- next\n\n> a quote\n> continued\n\n---\n";
#[test]
fn every_utf8_append_prefix_matches_cold_parse() {
    let mut d = Document::default();
    for c in SAMPLE.chars() {
        d.append(&c.to_string()).unwrap();
        assert_cold(&d);
    }
}
#[test]
fn byte_stream_handles_split_unicode() {
    let mut d = Document::default();
    let mut stream = Stream::default();
    for b in SAMPLE.bytes() {
        stream.push(&mut d, &[b]).unwrap();
    }
    stream.finish().unwrap();
    assert_eq!(d.source.to_string(), SAMPLE);
    assert_cold(&d);
}
#[test]
fn arbitrary_edits_and_deletions_match_cold_parse() {
    let mut d = Document::new(SAMPLE);
    let pieces = [
        "|", "\n", "**", "```", "é", " ", "# ", "", "---", ":", "[x]",
    ];
    let mut seed = 0x1923abcd_u64;
    for _ in 0..1200 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let s = d.source.to_string();
        let bounds = s
            .char_indices()
            .map(|(i, _)| i)
            .chain([s.len()])
            .collect::<Vec<_>>();
        let a = (seed as usize) % bounds.len();
        let z = (a + ((seed >> 32) as usize) % 5).min(bounds.len() - 1);
        let p = pieces[((seed >> 16) as usize) % pieces.len()];
        d.edit(bounds[a]..bounds[z], p).unwrap();
        assert_cold(&d);
    }
}
#[test]
fn table_cell_edits_and_row_appends_preserve_prefix_work() {
    let mut s = "| A | B |\n| --- | --- |\n".to_owned();
    for i in 0..2000 {
        s.push_str(&format!("| row{i} | same |\n"));
    }
    let mut d = Document::new(&s);
    let Content::Table(t) = &d.blocks[0].content else {
        panic!()
    };
    let old = t.rows[500].cells[1].id;
    d.append("| fresh | row |\n").unwrap();
    assert!(d.last_change.work.classified_lines <= 3);
    assert!(d.last_change.work.projected_elements <= 2);
    assert!(
        d.last_change.work.metadata_lines < 10,
        "{:?}",
        d.last_change.work
    );
    let Content::Table(t) = &d.blocks[0].content else {
        panic!()
    };
    assert_eq!(t.rows[500].cells[1].id, old);
    let at = d.source.to_string().find("row500").unwrap();
    d.edit(at..at + 6, "edited").unwrap();
    assert_eq!(d.last_change.work.projected_elements, 1);
    assert!(d.last_change.work.classified_lines <= 2);
    assert_cold(&d);
}
#[test]
fn table_schema_and_escaping() {
    let d = Document::new("| A | B |\n| :--- | ---: |\n| a\\|b | `c\\|d` |\n");
    let Content::Table(t) = &d.blocks[0].content else {
        panic!()
    };
    assert_eq!(t.rows[1].cells[0].rich.text, "a|b");
    assert_eq!(t.rows[1].cells[1].rich.text, "c|d");
    assert_eq!(t.align, vec![Alignment::Left, Alignment::Right]);
}
#[test]
fn edit_errors_and_stream_truncation_are_explicit() {
    let mut d = Document::new("é");
    assert_eq!(d.edit(1..1, "x").unwrap_err(), EditError::InvalidUtf8Range);
    let mut stream = Stream::default();
    stream.push(&mut d, &[0xf0, 0x9f]).unwrap();
    assert_eq!(stream.finish(), Err(EditError::IncompleteUtf8Stream));
    assert_eq!(d.source.to_string(), "é");
}
#[test]
fn open_fence_tail_does_not_reproject_or_copy_the_prefix() {
    let mut s = "```text\n".to_owned();
    for _ in 0..2000 {
        s.push_str("a completed code line\n");
    }
    let mut d = Document::new(&s);
    d.append("partial").unwrap();
    assert_eq!(d.last_change.work.projected_elements, 1);
    assert!(
        d.last_change.work.metadata_lines < 10,
        "{:?}",
        d.last_change.work
    );
    assert_cold(&d);
    for c in "\n```\n".chars() {
        d.append(&c.to_string()).unwrap();
        assert_cold(&d);
    }
}
#[test]
fn invalid_transport_chunk_is_transactional() {
    let mut d = Document::new("prefix");
    let mut stream = Stream::default();
    stream.push(&mut d, &[0xc3]).unwrap();
    assert_eq!(stream.push(&mut d, b"!"), Err(EditError::InvalidUtf8Stream));
    assert_eq!(d.source.to_string(), "prefix");
    stream.push(&mut d, &[0xa9]).unwrap();
    stream.finish().unwrap();
    assert_eq!(d.source.to_string(), "prefixé");
}
#[test]
fn one_column_table_promotion_changes_code_pipe_projection() {
    let mut d = Document::new("`a\\|b`\n");
    assert_eq!(d.blocks[0].elements().next().unwrap().rich.text, "a\\|b");
    d.append("| --- |\n").unwrap();
    assert_cold(&d);
    let Content::Table(t) = &d.blocks[0].content else {
        panic!()
    };
    assert_eq!(t.rows[0].cells[0].rich.text, "a|b");
    let start = d.source.to_string().find("| ---").unwrap();
    d.edit(start..d.source.len_bytes(), "").unwrap();
    assert_cold(&d);
}
#[test]
fn block_starters_take_precedence_over_setext_lookahead() {
    let d = Document::new("---\n---\n\n- item\n---\n\nHeading\n===\n");
    assert!(matches!(d.blocks[0].content, Content::Rule));
    assert!(matches!(d.blocks[1].content, Content::Rule));
    assert!(matches!(
        d.blocks[2].content,
        Content::Text {
            kind: TextKind::List { .. },
            ..
        }
    ));
    assert!(matches!(d.blocks[3].content, Content::Rule));
    assert!(matches!(
        d.blocks[4].content,
        Content::Text {
            kind: TextKind::Heading(1),
            ..
        }
    ));
}
#[test]
fn richer_chunk_partitions_match_cold_parse() {
    for input in [
        "`a\\|b`\n| --- |\n| é |\n",
        "````lang\n```not closed\n世界\n````\n",
        "- item\n  more **text**\n\n> quoted `code`\n> continued\n",
        "# title ###\n\nA \\* &amp; &#123;\nwith **strong _nested_ text**.\n",
        "| A | B |\n| ---:: | --- |\nnot a table\n",
    ] {
        for chunk in [1, 2, 7, 17] {
            let mut d = Document::default();
            let mut stream = Stream::default();
            for bytes in input.as_bytes().chunks(chunk) {
                stream.push(&mut d, bytes).unwrap();
                assert_cold(&d);
            }
            stream.finish().unwrap();
        }
    }
}

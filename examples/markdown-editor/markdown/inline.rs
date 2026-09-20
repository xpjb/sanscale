//! Inline projection, independent of fonts/GPU/windowing. Delimiters operate on
//! source bytes; output runs and source maps operate on *projected* UTF-8 bytes.
use std::{collections::HashMap, ops::Range};

pub const STRONG: u8 = 1;
pub const EMPHASIS: u8 = 2;
pub const CODE: u8 = 4;
pub const LINK: u8 = 8;
pub const STRIKE: u8 = 16;
pub const IMAGE: u8 = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub range: Range<usize>,
    pub flags: u8,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub display: Range<usize>,
    pub source: Range<usize>,
    pub exact: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RichText {
    pub text: String,
    pub runs: Vec<Run>,
    pub mapping: Vec<Mapping>,
}
impl RichText {
    pub fn literal(source: &str) -> Self {
        let mut out = Self::default();
        out.push(source, 0, 0..source.len(), true);
        out
    }
    pub fn same_pixels(&self, other: &Self) -> bool {
        self.text == other.text && self.runs == other.runs
    }
    pub fn source_byte(&self, byte: usize) -> usize {
        let i = self.mapping.partition_point(|m| m.display.end <= byte);
        if let Some(m) = self.mapping.get(i) {
            if m.exact {
                m.source.start + byte.saturating_sub(m.display.start)
            } else {
                m.source.start
            }
        } else {
            self.mapping.last().map_or(0, |m| m.source.end)
        }
    }
    fn push(&mut self, text: &str, flags: u8, source: Range<usize>, exact: bool) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        let end = self.text.len();
        if let Some(last) = self.runs.last_mut().filter(|r| r.flags == flags) {
            last.range.end = end;
        } else {
            self.runs.push(Run {
                range: start..end,
                flags,
            });
        }
        if let Some(last) = self
            .mapping
            .last_mut()
            .filter(|m| m.exact && exact && m.source.end == source.start)
        {
            last.display.end = end;
            last.source.end = source.end;
        } else {
            self.mapping.push(Mapping {
                display: start..end,
                source,
                exact,
            });
        }
    }
}
#[derive(Clone, Copy)]
struct Tick {
    start: usize,
    end: usize,
    escaped: bool,
}
/// Matched code spans, found without rescanning every unmatched opener's suffix.
pub(super) fn code_spans(s: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let b = s.as_bytes();
    let mut ticks = Vec::new();
    let mut by_len: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'`' {
            i += s[i..].chars().next().unwrap().len_utf8();
            continue;
        }
        let start = i;
        while b.get(i) == Some(&b'`') {
            i += 1;
        }
        let escaped = b[..start].iter().rev().take_while(|&&c| c == b'\\').count() % 2 == 1;
        by_len.entry(i - start).or_default().push(ticks.len());
        ticks.push(Tick {
            start,
            end: i,
            escaped,
        });
    }
    let mut out = Vec::new();
    let mut n = 0;
    while n < ticks.len() {
        let a = ticks[n];
        let open = a.start + usize::from(a.escaped);
        let count = a.end - open;
        let Some(candidates) = by_len.get(&count).filter(|_| count > 0) else {
            n += 1;
            continue;
        };
        let next = candidates.partition_point(|&j| j <= n);
        if let Some(&j) = candidates.get(next) {
            let z = ticks[j];
            out.push((open..z.end, a.end..z.start));
            n = j + 1;
        } else {
            n += 1;
        }
    }
    out
}
fn punctuation(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace()
}
fn entity(s: &str) -> Option<(String, usize)> {
    let end = s.as_bytes().iter().take(34).position(|&b| b == b';')?;
    let name = &s[1..end];
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        _ => {
            let number = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X"));
            let value = if let Some(n) = number {
                u32::from_str_radix(n, 16).ok()?
            } else {
                name.strip_prefix('#')?.parse().ok()?
            };
            char::from_u32(value)
                .filter(|c| *c != '\0')
                .unwrap_or('\u{fffd}')
        }
    };
    Some((c.to_string(), end + 1))
}

/// Deliberately bounded Markdown subset, not a CommonMark conformance parser.
/// Nested emphasis/code/inline links, images-as-alt-text, strike, escapes, six
/// named + numeric entities, soft/hard breaks. Unfinished constructs stay literal.
pub fn parse(s: &str) -> RichText {
    parse_inner(s, false)
}
pub fn parse_cell(s: &str) -> RichText {
    parse_inner(s, true)
}
fn parse_inner(s: &str, table: bool) -> RichText {
    let b = s.as_bytes();
    let codes = code_spans(s);
    let mut hidden = vec![false; b.len()];
    let mut opaque = vec![false; b.len()];
    let mut events: Vec<(usize, u8, i32)> = Vec::new();
    let mut mark = |range: Range<usize>, flag: u8| {
        events.push((range.start, flag, 1));
        events.push((range.end, flag, -1));
    };
    for (full, inner) in &codes {
        hidden[full.start..inner.start].fill(true);
        hidden[inner.end..full.end].fill(true);
        opaque[full.clone()].fill(true);
        let body = &s[inner.clone()];
        if body.len() >= 2
            && (body.starts_with(' ') || body.starts_with('\n'))
            && (body.ends_with(' ') || body.ends_with('\n'))
            && !body.chars().all(|c| c == ' ' || c == '\n')
        {
            hidden[inner.start] = true;
            hidden[inner.end - 1] = true;
        }
        mark(inner.clone(), CODE);
    }
    // Match brackets/parentheses once, outside code and backslash escapes. No
    // repeated suffix searches for adversarial unmatched '[[' / '(((' input.
    let mut bracket = Vec::new();
    let mut paren = Vec::new();
    let mut pairs = HashMap::new();
    let mut i = 0;
    while i < b.len() {
        if opaque[i] {
            i += 1;
            continue;
        }
        if b[i] == b'\\' {
            i += 1;
            if i < b.len() {
                i += s[i..].chars().next().unwrap().len_utf8();
            }
            continue;
        }
        match b[i] {
            b'[' => bracket.push(i),
            b']' => {
                if let Some(a) = bracket.pop() {
                    pairs.insert(a, i);
                }
            }
            b'(' => paren.push(i),
            b')' => {
                if let Some(a) = paren.pop() {
                    pairs.insert(a, i);
                }
            }
            _ => {}
        }
        i += s[i..].chars().next().unwrap().len_utf8();
    }
    i = 0;
    while i < b.len() {
        if opaque[i] || hidden[i] {
            i += 1;
            continue;
        }
        if b[i] == b'\\' {
            i += 1;
            if i < b.len() {
                i += s[i..].chars().next().unwrap().len_utf8();
            }
            continue;
        }
        if b[i] == b'[' {
            if let Some(&close) = pairs.get(&i) {
                if b.get(close + 1) == Some(&b'(') {
                    if let Some(&end) = pairs.get(&(close + 1)) {
                        // Inline destinations are passive. No HTML, networking,
                        // file access or automatic URL activation lives here.
                        hidden[i] = true;
                        hidden[close..=end].fill(true);
                        opaque[close..=end].fill(true);
                        let image = i > 0
                            && b[i - 1] == b'!'
                            && !hidden[i - 1]
                            && (i < 2 || b[i - 2] != b'\\');
                        if image {
                            hidden[i - 1] = true;
                        }
                        mark(i + 1..close, if image { IMAGE } else { LINK });
                    }
                }
            }
        }
        i += s[i..].chars().next().unwrap().len_utf8();
    }
    // Stack discipline prevents crossing emphasis. Only the top opener can
    // close, so pairing is linear; unmatched markers are never silently lost.
    struct Open {
        start: usize,
        remaining: usize,
        ch: u8,
    }
    let mut stack: Vec<Open> = Vec::new();
    i = 0;
    while i < b.len() {
        if opaque[i] || hidden[i] {
            i += 1;
            continue;
        }
        if b[i] == b'\\' {
            i += 1;
            if i < b.len() {
                i += s[i..].chars().next().unwrap().len_utf8();
            }
            continue;
        }
        let ch = b[i];
        if !matches!(ch, b'*' | b'_' | b'~') {
            i += s[i..].chars().next().unwrap().len_utf8();
            continue;
        }
        let start = i;
        while b.get(i) == Some(&ch) && !opaque[i] && !hidden[i] {
            i += 1;
        }
        let mut remaining = i - start;
        let mut cursor = start;
        if ch == b'~' && remaining < 2 {
            continue;
        }
        let prev = s[..start].chars().next_back();
        let next = s[i..].chars().next();
        let left = next.is_some_and(|c| !c.is_whitespace())
            && (next.is_some_and(|c| !punctuation(c))
                || prev.is_none_or(|c| c.is_whitespace() || punctuation(c)));
        let right = prev.is_some_and(|c| !c.is_whitespace())
            && (prev.is_some_and(|c| !punctuation(c))
                || next.is_none_or(|c| c.is_whitespace() || punctuation(c)));
        let can_open = left && (ch != b'_' || !right || prev.is_some_and(punctuation));
        let can_close = right && (ch != b'_' || !left || next.is_some_and(punctuation));
        if can_close {
            while remaining > 0 && stack.last().is_some_and(|o| o.ch == ch) {
                let o = stack.last_mut().unwrap();
                let count = if o.remaining >= 2 && remaining >= 2 {
                    2
                } else {
                    1
                };
                if ch == b'~' && count != 2 {
                    break;
                }
                let open = o.start + o.remaining - count;
                hidden[open..open + count].fill(true);
                hidden[cursor..cursor + count].fill(true);
                mark(
                    open + count..cursor,
                    if ch == b'~' {
                        STRIKE
                    } else if count == 2 {
                        STRONG
                    } else {
                        EMPHASIS
                    },
                );
                o.remaining -= count;
                remaining -= count;
                cursor += count;
                if o.remaining == 0 {
                    stack.pop();
                }
            }
        }
        if remaining > 0 && can_open {
            stack.push(Open {
                start: cursor,
                remaining,
                ch,
            });
        }
    }
    events.sort_by_key(|e| e.0);
    let mut counts = [0i32; 6];
    let mut event = 0;
    let mut out = RichText::default();
    i = 0;
    while i < b.len() {
        while event < events.len() && events[event].0 <= i {
            let (_, flag, delta) = events[event];
            counts[flag.trailing_zeros() as usize] += delta;
            event += 1;
        }
        if hidden[i] {
            i += 1;
            continue;
        }
        let flags = counts
            .iter()
            .enumerate()
            .fold(0, |f, (j, &n)| if n > 0 { f | (1 << j) } else { f });
        if table && b[i] == b'\\' && b.get(i + 1) == Some(&b'|') {
            out.push("|", flags, i..i + 2, false);
            i += 2;
            continue;
        }
        if flags & CODE == 0 {
            if b[i] == b'\\' && b.get(i + 1).is_some_and(u8::is_ascii_punctuation) {
                out.push(&s[i + 1..i + 2], flags, i..i + 2, false);
                i += 2;
                continue;
            }
            if b[i] == b'&' {
                if let Some((value, n)) = entity(&s[i..]) {
                    out.push(&value, flags, i..i + n, false);
                    i += n;
                    continue;
                }
            }
            // Hard breaks consume their source markers; soft breaks project to
            // spaces. Kept newlines are split into real sanscale paragraphs by
            // the adapter, never shaped as a fictitious newline glyph.
            if b[i] == b'\\' && b.get(i + 1) == Some(&b'\n') {
                out.push("\n", flags, i..i + 2, false);
                i += 2;
                continue;
            }
            if b[i] == b' ' {
                let end = i + s[i..].bytes().take_while(|&c| c == b' ').count();
                if end - i >= 2 && b.get(end) == Some(&b'\n') {
                    out.push("\n", flags, i..end + 1, false);
                    i = end + 1;
                    continue;
                }
                out.push(&s[i..end], flags, i..end, true);
                i = end;
                continue;
            }
        }
        let c = s[i..].chars().next().unwrap();
        let end = i + c.len_utf8();
        if c == '\n' {
            out.push(" ", flags, i..end, false);
        } else {
            out.push(&s[i..end], flags, i..end, true);
        }
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_faces_projection_and_source_mapping() {
        let s = "A **bold _café_** &amp; `x*y` [link](https://x/(y))";
        let p = parse(s);
        assert_eq!(p.text, "A bold café & x*y link");
        let i = p.text.find("café").unwrap();
        assert!(
            p.runs
                .iter()
                .any(|r| r.range.contains(&i) && r.flags == STRONG | EMPHASIS)
        );
        let i = p.text.find('&').unwrap();
        assert_eq!(p.source_byte(i), s.find("&amp;").unwrap());
        let i = p.text.find("x*y").unwrap();
        assert!(
            p.runs
                .iter()
                .any(|r| r.range.contains(&i) && r.flags & CODE != 0)
        );
    }
    #[test]
    fn triple_mixed_and_incomplete_delimiters() {
        assert_eq!(parse("***both***").runs[0].flags, STRONG | EMPHASIS);
        assert_eq!(parse("**bold *italic***").text, "bold italic");
        assert_eq!(parse("unfinished **hello").text, "unfinished **hello");
        assert_eq!(parse("snake_case_name").text, "snake_case_name");
        assert_eq!(parse("~~gone~~").runs[0].flags, STRIKE);
    }
    #[test]
    fn literals_entities_and_breaks() {
        assert_eq!(
            parse("\\*yes\\* &#x1f30d; &unknown;").text,
            "*yes* 🌍 &unknown;"
        );
        assert_eq!(
            parse("one\ntwo  \nthree\\\nfour").text,
            "one two\nthree\nfour"
        );
        assert_eq!(parse("`` `x` &amp; ``").text, "`x` &amp;");
        assert_eq!(parse(r"`a\`b`").text, r"a\b`");
        assert_eq!(parse("![alt](remote.png)").text, "alt");
        assert_eq!(
            parse("<script>alert(1)</script>").text,
            "<script>alert(1)</script>"
        );
    }
    #[test]
    fn adversarial_unmatched_markers_are_bounded_and_utf8_safe() {
        for s in [
            "[".repeat(20000),
            "*a ".repeat(10000),
            "`x ``x ```x ".repeat(2000),
            "é\\世界 &x;".repeat(100),
        ] {
            let p = parse(&s);
            for m in p.mapping {
                assert!(s.is_char_boundary(m.source.start) && s.is_char_boundary(m.source.end));
            }
        }
    }
}

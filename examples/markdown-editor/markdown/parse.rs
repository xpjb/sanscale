use super::{Alignment, Id, TextKind};
use std::ops::Range;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Continuation {
    Paragraph,
    Quote(u8),
    List(usize),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Idle,
    Text {
        owner: Id,
        continuation: Continuation,
    },
    Fence {
        owner: Id,
        marker: u8,
        count: usize,
        indent: usize,
    },
    TableDelimiter {
        owner: Id,
        columns: usize,
    },
    Table {
        owner: Id,
        columns: usize,
    },
    Setext {
        owner: Id,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Blank,
    Text(TextKind),
    TableHead(Vec<Alignment>),
    TableDelimiter,
    TableRow,
    FenceOpen { language: String },
    FenceLine,
    FenceClose,
    Setext,
    Rule,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Class {
    pub owner: Option<Id>,
    pub kind: Kind,
    pub content: Range<usize>,
}
impl Class {
    pub fn blank() -> Self {
        Self {
            owner: None,
            kind: Kind::Blank,
            content: 0..0,
        }
    }
}

/// Escaped pipes and pipes inside *matched* code spans stay in their cell. We
/// accept that useful extension even though strict GFM requires escaping a pipe
/// inside code. An unfinished backtick does not swallow all following columns.
pub(super) fn cells(s: &str) -> Vec<Range<usize>> {
    let codes = super::inline::code_spans(s);
    let mut code = 0;
    let mut bars = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if code < codes.len() && i == codes[code].0.start {
            i = codes[code].0.end;
            code += 1;
            continue;
        }
        if b[i] == b'\\' {
            i += 1;
            if i < b.len() {
                i += s[i..].chars().next().unwrap().len_utf8();
            }
            continue;
        }
        if b[i] == b'|' {
            bars.push(i);
        }
        i += s[i..].chars().next().unwrap().len_utf8();
    }
    let mut parts = Vec::new();
    let mut start = 0;
    for end in bars {
        parts.push(start..end);
        start = end + 1;
    }
    parts.push(start..s.len());
    if parts.len() > 1 && s[parts[0].clone()].trim().is_empty() {
        parts.remove(0);
    }
    if parts.len() > 1 && s[parts.last().unwrap().clone()].trim().is_empty() {
        parts.pop();
    }
    for p in &mut parts {
        let raw = &s[p.clone()];
        let trimmed = raw.trim();
        let left = raw.len() - raw.trim_start().len();
        p.start += left;
        p.end = p.start + trimmed.len();
    }
    parts
}
fn alignment(s: &str) -> Option<Vec<Alignment>> {
    if !s.contains('|') {
        return None;
    }
    let cells = cells(s);
    if cells.is_empty() || cells.len() > 64 {
        return None;
    }
    cells
        .into_iter()
        .map(|r| {
            let x = s[r].trim();
            let body = x.strip_prefix(':').unwrap_or(x);
            let body = body.strip_suffix(':').unwrap_or(body);
            if body.len() < 3 || !body.bytes().all(|b| b == b'-') {
                return None;
            }
            Some(if x.starts_with(':') && x.ends_with(':') {
                Alignment::Center
            } else if x.ends_with(':') {
                Alignment::Right
            } else {
                Alignment::Left
            })
        })
        .collect()
}
fn setext(s: &str) -> Option<u8> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.bytes().all(|b| b == b'=') {
        Some(1)
    } else if s.len() >= 3 && s.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
}
fn rule(s: &str) -> bool {
    let mut chars = s.bytes().filter(|b| !b.is_ascii_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, b'-' | b'*' | b'_') {
        return false;
    }
    let mut n = 1;
    for c in chars {
        if c != first {
            return false;
        }
        n += 1;
    }
    n >= 3
}
fn list(s: &str) -> Option<(usize, String)> {
    let b = s.as_bytes();
    let end = if matches!(b.first(), Some(b'-' | b'+' | b'*')) {
        1
    } else {
        let n = b.iter().take_while(|b| b.is_ascii_digit()).count();
        if n == 0 || n > 9 || !matches!(b.get(n), Some(b'.' | b')')) {
            return None;
        }
        n + 1
    };
    if !b.get(end).is_some_and(u8::is_ascii_whitespace) {
        return None;
    }
    let mut at = end;
    while b.get(at) == Some(&b' ') {
        at += 1;
    }
    let mut marker = if end == 1 {
        "•".to_owned()
    } else {
        s[..end].to_owned()
    };
    if s[at..].starts_with("[ ] ") {
        marker = "☐".into();
        at += 4;
    } else if s[at..].starts_with("[x] ") || s[at..].starts_with("[X] ") {
        marker = "☑".into();
        at += 4;
    }
    Some((at, marker))
}
pub(super) fn classify(id: Id, raw: &str, next: Option<&str>, incoming: &Mode) -> (Class, Mode) {
    let spaces = raw.bytes().take_while(|&b| b == b' ').count();
    let trimmed = raw.trim();
    let class = |owner, kind, content| Class {
        owner: Some(owner),
        kind,
        content,
    };
    if let Mode::Fence {
        owner,
        marker,
        count,
        indent,
    } = *incoming
    {
        let run = raw[spaces..].bytes().take_while(|&c| c == marker).count();
        if spaces <= 3 && run >= count && raw[spaces + run..].trim().is_empty() {
            return (class(owner, Kind::FenceClose, 0..0), Mode::Idle);
        }
        return (
            class(owner, Kind::FenceLine, spaces.min(indent)..raw.len()),
            incoming.clone(),
        );
    }
    if let Mode::TableDelimiter { owner, columns } = *incoming {
        return (
            class(owner, Kind::TableDelimiter, 0..0),
            Mode::Table { owner, columns },
        );
    }
    if let Mode::Setext { owner } = *incoming {
        return (class(owner, Kind::Setext, 0..0), Mode::Idle);
    }
    if trimmed.is_empty() {
        return (Class::blank(), Mode::Idle);
    }
    if let Mode::Table { owner, .. } = *incoming {
        if raw.contains('|') {
            return (class(owner, Kind::TableRow, 0..raw.len()), incoming.clone());
        }
    }
    if spaces <= 3 {
        let body = &raw[spaces..];
        let marker = body.as_bytes()[0];
        if matches!(marker, b'`' | b'~') {
            let count = body.bytes().take_while(|&c| c == marker).count();
            let info = body[count..].trim();
            if count >= 3 && (marker != b'`' || !info.contains('`')) {
                return (
                    class(
                        id,
                        Kind::FenceOpen {
                            language: info.to_owned(),
                        },
                        spaces + count..raw.len(),
                    ),
                    Mode::Fence {
                        owner: id,
                        marker,
                        count,
                        indent: spaces,
                    },
                );
            }
        }
        let hashes = body.bytes().take_while(|&c| c == b'#').count();
        if (1..=6).contains(&hashes)
            && (body.len() == hashes || body.as_bytes()[hashes].is_ascii_whitespace())
        {
            let start = spaces + hashes + body[hashes..].len() - body[hashes..].trim_start().len();
            let mut end = raw.trim_end().len();
            let without = raw[..end].trim_end_matches('#').len();
            if without < end && without > start && raw.as_bytes()[without - 1].is_ascii_whitespace()
            {
                end = raw[..without].trim_end().len();
            }
            return (
                class(
                    id,
                    Kind::Text(TextKind::Heading(hashes as u8)),
                    start..end.max(start),
                ),
                Mode::Idle,
            );
        }
    }
    if raw.contains('|') {
        if let Some(align) = next.and_then(alignment) {
            if cells(raw).len() == align.len() {
                let columns = align.len();
                return (
                    class(id, Kind::TableHead(align), 0..raw.len()),
                    Mode::TableDelimiter { owner: id, columns },
                );
            }
        }
    }
    if rule(raw) {
        return (class(id, Kind::Rule, 0..0), Mode::Idle);
    }
    if raw[spaces..].starts_with('>') {
        let mut at = spaces;
        let mut depth = 0u8;
        while raw.as_bytes().get(at) == Some(&b'>') {
            depth = depth.saturating_add(1);
            at += 1;
            if raw.as_bytes().get(at) == Some(&b' ') {
                at += 1;
            }
        }
        let owner = if let Mode::Text {
            owner,
            continuation: Continuation::Quote(n),
        } = *incoming
        {
            if n == depth { owner } else { id }
        } else {
            id
        };
        return (
            class(owner, Kind::Text(TextKind::Quote(depth)), at..raw.len()),
            Mode::Text {
                owner,
                continuation: Continuation::Quote(depth),
            },
        );
    }
    if let Some((prefix, marker)) = list(&raw[spaces..]) {
        return (
            class(
                id,
                Kind::Text(TextKind::List {
                    depth: (spaces / 2).min(16) as u8,
                    marker,
                }),
                spaces + prefix..raw.len(),
            ),
            Mode::Text {
                owner: id,
                continuation: Continuation::List(spaces + prefix),
            },
        );
    }
    if let Some(level) = next.and_then(setext) {
        return (
            class(id, Kind::Text(TextKind::Heading(level)), spaces..raw.len()),
            Mode::Setext { owner: id },
        );
    }
    let (owner, start, continuation) = match incoming {
        Mode::Text {
            owner,
            continuation: Continuation::Paragraph,
        } => (*owner, spaces.min(3), Continuation::Paragraph),
        Mode::Text {
            owner,
            continuation: Continuation::List(indent),
        } if spaces >= *indent => (*owner, *indent, Continuation::List(*indent)),
        _ => (id, spaces.min(3), Continuation::Paragraph),
    };
    (
        class(owner, Kind::Text(TextKind::Paragraph), start..raw.len()),
        Mode::Text {
            owner,
            continuation,
        },
    )
}

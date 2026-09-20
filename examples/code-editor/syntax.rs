//! Small, line-state C highlighter, not a C parser/preprocessor. Handles ordinary
//! strings/chars, escapes, block/line comments and backslash-newline continuation.
//! Does not expand macros, interpret #if, trigraphs/digraphs, or join tokens or
//! comment delimiters split by line splicing. Incomplete uncontinued quotes end
//! at the physical line, so an unfinished string doesn't poison the whole file.

use sanscale::{Color, FontChainHandle, FontSpan};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Code,
    BlockComment,
    LineComment,
    Quoted {
        quote: u8,
        escaped: bool,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Type,
    Comment,
    String,
    Number,
    Directive,
    Function,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub range: Range<usize>,
    pub kind: Kind,
}

pub fn lex(line: &str, mut state: State) -> (Vec<Token>, State) {
    let b = line.as_bytes();
    let continued = b.last() == Some(&b'\\');
    let n = b.len() - usize::from(continued);
    let mut i = 0;
    let mut tokens = Vec::new();
    let mut leading = true;
    while i < n {
        let start = i;
        match state {
            State::BlockComment => {
                if let Some(end) = line[i..n].find("*/") {
                    i += end + 2;
                    state = State::Code;
                } else {
                    i = n;
                }
                push(
                    &mut tokens,
                    start..if i == n && continued { b.len() } else { i },
                    Kind::Comment,
                );
                // A preprocessing comment is whitespace before a directive.
            }
            State::LineComment => {
                push(&mut tokens, i..b.len(), Kind::Comment);
                i = n;
            }
            State::Quoted { quote, mut escaped } => {
                let mut closed = false;
                while i < n {
                    let c = b[i];
                    i += 1;
                    if escaped {
                        escaped = false;
                        continue;
                    }
                    if c == b'\\' {
                        escaped = true;
                    } else if c == quote {
                        closed = true;
                        break;
                    }
                }
                state = if closed {
                    State::Code
                } else {
                    State::Quoted { quote, escaped }
                };
                push(
                    &mut tokens,
                    start..if i == n && continued && !closed {
                        b.len()
                    } else {
                        i
                    },
                    Kind::String,
                );
                leading = false;
            }
            State::Code => {
                let c = line[i..].chars().next().unwrap();
                if c.is_whitespace() {
                    i += c.len_utf8();
                    continue;
                }
                if b[i..n].starts_with(b"/*") {
                    state = State::BlockComment;
                    i += 2;
                    // Include the opening delimiter in this comment token.
                    if let Some(end) = line[i..n].find("*/") {
                        i += end + 2;
                        state = State::Code;
                    } else {
                        i = n;
                    }
                    push(
                        &mut tokens,
                        start..if i == n && continued { b.len() } else { i },
                        Kind::Comment,
                    );
                } else if b[i..n].starts_with(b"//") {
                    push(&mut tokens, i..b.len(), Kind::Comment);
                    state = State::LineComment;
                    i = n;
                } else if c == '"' || c == '\'' {
                    let quote = b[i];
                    i += 1;
                    let mut escaped = false;
                    let mut closed = false;
                    while i < n {
                        let c = b[i];
                        i += 1;
                        if escaped {
                            escaped = false;
                            continue;
                        }
                        if c == b'\\' {
                            escaped = true;
                        } else if c == quote {
                            closed = true;
                            break;
                        }
                    }
                    if !closed {
                        state = State::Quoted { quote, escaped };
                    }
                    push(
                        &mut tokens,
                        start..if i == n && continued && !closed {
                            b.len()
                        } else {
                            i
                        },
                        Kind::String,
                    );
                } else if c == '#' && leading {
                    i += 1;
                    while i < n && b[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    while i < n && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
                        i += 1;
                    }
                    push(&mut tokens, start..i, Kind::Directive);
                    if matches!(line[start + 1..i].trim(), "include" | "include_next") {
                        let mut j = i;
                        while j < n && b[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if b.get(j) == Some(&b'<') {
                            i = line[j..n].find('>').map(|end| j + end + 1).unwrap_or(n);
                            push(&mut tokens, j..i, Kind::String);
                        }
                    }
                } else if c.is_ascii_digit()
                    || (c == '.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
                {
                    i += 1;
                    while i < n {
                        let c = b[i];
                        if c.is_ascii_alphanumeric()
                            || c == b'.'
                            || c == b'_'
                            || ((c == b'+' || c == b'-')
                                && matches!(b[i - 1], b'e' | b'E' | b'p' | b'P'))
                        {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    push(&mut tokens, start..i, Kind::Number);
                } else if c.is_alphabetic() || c == '_' {
                    i += c.len_utf8();
                    while i < n {
                        let c = line[i..].chars().next().unwrap();
                        if c.is_alphanumeric() || c == '_' {
                            i += c.len_utf8();
                        } else {
                            break;
                        }
                    }
                    let word = &line[start..i];
                    let kind = if KEYWORDS.contains(&word) {
                        Some(Kind::Keyword)
                    } else if TYPES.contains(&word) {
                        Some(Kind::Type)
                    } else if line[i..n].trim_start().starts_with('(') {
                        Some(Kind::Function)
                    } else {
                        None
                    };
                    if let Some(kind) = kind {
                        push(&mut tokens, start..i, kind);
                    }
                } else {
                    i += c.len_utf8();
                }
                if !b[start..n].starts_with(b"/*") && !b[start..n].starts_with(b"//") {
                    leading = false;
                }
            }
        }
    }
    if !continued && matches!(state, State::LineComment | State::Quoted { .. }) {
        state = State::Code;
    }
    (tokens, state)
}
fn push(tokens: &mut Vec<Token>, range: Range<usize>, kind: Kind) {
    if !range.is_empty() {
        tokens.push(Token { range, kind });
    }
}
const KEYWORDS: &[&str] = &[
    "auto",
    "break",
    "case",
    "char",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "register",
    "restrict",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "typedef",
    "union",
    "unsigned",
    "void",
    "volatile",
    "while",
    "_Alignas",
    "_Alignof",
    "_Atomic",
    "_Bool",
    "_Complex",
    "_Generic",
    "_Imaginary",
    "_Noreturn",
    "_Static_assert",
    "_Thread_local",
    "alignas",
    "alignof",
    "bool",
    "constexpr",
    "false",
    "nullptr",
    "static_assert",
    "thread_local",
    "true",
    "typeof",
    "typeof_unqual",
];
// A convenience for the demo, not semantic typedef resolution.
const TYPES: &[&str] = &[
    "size_t",
    "ptrdiff_t",
    "uint8_t",
    "uint16_t",
    "uint32_t",
    "uint64_t",
    "int8_t",
    "int16_t",
    "int32_t",
    "int64_t",
    "FILE",
    "NULL",
];

pub fn color(kind: Kind, alternate: bool) -> Color {
    use Kind::*;
    let rgb = match (kind, alternate) {
        (Keyword, false) => [0.72, 0.33, 0.64],
        (Keyword, true) => [0.97, 0.56, 0.18],
        (Type, false) => [0.31, 0.72, 0.72],
        (Type, true) => [0.52, 0.77, 0.31],
        (Comment, false) => [0.34, 0.46, 0.40],
        (Comment, true) => [0.43, 0.48, 0.58],
        (String, false) => [0.56, 0.72, 0.32],
        (String, true) => [0.36, 0.76, 0.65],
        (Number, false) => [0.87, 0.57, 0.28],
        (Number, true) => [0.68, 0.50, 0.85],
        (Directive, false) => [0.75, 0.43, 0.37],
        (Directive, true) => [0.90, 0.53, 0.49],
        (Function, false) => [0.34, 0.61, 0.88],
        (Function, true) => [0.48, 0.70, 0.92],
    };
    Color([rgb[0], rgb[1], rgb[2], 1.])
}

/// Resolve semantic roles separately from lexing. Select the role at each
/// grapheme's start, so even temporarily malformed Unicode cannot split a
/// combining sequence/ZWJ into separate font runs. Adjacent equal chains coalesce.
pub fn fonts(
    line: &str,
    tokens: &[Token],
    normal: FontChainHandle,
    bold: FontChainHandle,
    italic: FontChainHandle,
    italic_comments: bool,
) -> Vec<FontSpan> {
    let mut spans: Vec<FontSpan> = Vec::new();
    let mut t = 0;
    for (byte, g) in line.grapheme_indices(true) {
        while tokens.get(t).is_some_and(|v| v.range.end <= byte) {
            t += 1;
        }
        let kind = tokens
            .get(t)
            .filter(|v| v.range.start <= byte)
            .map(|v| v.kind);
        let chain = match kind {
            Some(Kind::Keyword | Kind::Type | Kind::Directive) => bold,
            Some(Kind::Comment) if italic_comments => italic,
            _ => normal,
        };
        if chain == normal {
            continue;
        }
        let end = byte + g.len();
        if let Some(last) = spans
            .last_mut()
            .filter(|v| v.chain == chain && v.range.end == byte)
        {
            last.range.end = end;
        } else {
            spans.push(FontSpan {
                range: byte..end,
                chain,
            });
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strings_chars_and_comments_do_not_cross_contaminate() {
        let line = r#"char *s = "/* not a comment */ \\"; char c = '\''; // real"#;
        let (tokens, state) = lex(line, State::Code);
        assert_eq!(state, State::Code);
        let comments: Vec<_> = tokens.iter().filter(|t| t.kind == Kind::Comment).collect();
        assert_eq!(comments.len(), 1);
        assert!(line[comments[0].range.clone()].starts_with("//"));
        assert_eq!(tokens.iter().filter(|t| t.kind == Kind::String).count(), 2);
    }
    #[test]
    fn block_state_propagates_and_closing_comment_resumes_code() {
        let (_, state) = lex("int x; /* note", State::Code);
        assert_eq!(state, State::BlockComment);
        let (tokens, state) = lex("still comment */ return 0;", state);
        assert_eq!(state, State::Code);
        assert_eq!(tokens[0].kind, Kind::Comment);
        assert!(tokens.iter().any(|t| t.kind == Kind::Keyword));
    }
    #[test]
    fn continued_comments_and_strings_and_incomplete_input() {
        let (_, state) = lex("// continued \\", State::Code);
        assert_eq!(state, State::LineComment);
        let (tokens, state) = lex("int is still a comment", state);
        assert_eq!(tokens[0].kind, Kind::Comment);
        assert_eq!(state, State::Code);
        let (_, state) = lex("\"continued \\", State::Code);
        assert!(matches!(state, State::Quoted { .. }));
        let (tokens, state) = lex("text\"; int n;", state);
        assert_eq!(tokens[0].kind, Kind::String);
        assert_eq!(state, State::Code);
        assert_eq!(lex("\"incomplete", State::Code).1, State::Code);
        assert_eq!(lex("/* incomplete", State::Code).1, State::BlockComment);
    }
    #[test]
    fn directives_numbers_and_utf8_ranges() {
        let line = "#define VALUE 0x1.fp+3 // café 世界 👩‍💻";
        let (tokens, _) = lex(line, State::Code);
        assert_eq!(tokens[0].kind, Kind::Directive);
        assert_eq!(&line[tokens[1].range.clone()], "0x1.fp+3");
        for token in tokens {
            assert!(
                line.is_char_boundary(token.range.start) && line.is_char_boundary(token.range.end)
            );
        }
    }
    #[test]
    fn directives_can_follow_comment_whitespace_and_color_header_names() {
        let line = "/* note */ # include <stdio.h>";
        let (tokens, state) = lex(line, State::Code);
        assert_eq!(state, State::Code);
        assert_eq!(
            tokens.iter().map(|t| t.kind).collect::<Vec<_>>(),
            vec![Kind::Comment, Kind::Directive, Kind::String]
        );
        assert_eq!(&line[tokens[2].range.clone()], "<stdio.h>");
    }
}

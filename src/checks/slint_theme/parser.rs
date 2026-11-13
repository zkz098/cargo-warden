//! Lightweight context-aware Slint lexer and property/token extractor.

use super::tokens::normalize_slint_ident;
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
enum LexState {
    Normal,
    LineComment,
    BlockComment,
    StringLit { escaped: bool },
}

fn push_masked_char(out: &mut String, ch: char) {
    if ch == '\n' {
        out.push('\n');
    } else {
        for _ in 0..ch.len_utf8() {
            out.push(' ');
        }
    }
}

/// Mask `//`, `/* ... */` comments and `"..."` strings with spaces while preserving byte offsets and newlines.
pub(crate) fn mask_comments_and_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();
    let mut state = LexState::Normal;

    while let Some((_, ch)) = chars.next() {
        state = step_lex_state(state, ch, &mut chars, &mut out);
    }
    out
}

fn step_normal_lex(
    ch: char,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    out: &mut String,
) -> LexState {
    if ch == '/' && chars.peek().is_some_and(|&(_, next)| next == '/') {
        push_masked_char(out, ch);
        if let Some((_, next)) = chars.next() {
            push_masked_char(out, next);
        }
        return LexState::LineComment;
    }
    if ch == '/' && chars.peek().is_some_and(|&(_, next)| next == '*') {
        push_masked_char(out, ch);
        if let Some((_, next)) = chars.next() {
            push_masked_char(out, next);
        }
        return LexState::BlockComment;
    }
    if ch == '"' {
        push_masked_char(out, ch);
        return LexState::StringLit { escaped: false };
    }
    out.push(ch);
    LexState::Normal
}

fn step_lex_state(
    state: LexState,
    ch: char,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    out: &mut String,
) -> LexState {
    match state {
        LexState::Normal => step_normal_lex(ch, chars, out),
        LexState::LineComment => {
            push_masked_char(out, ch);
            if ch == '\n' {
                LexState::Normal
            } else {
                LexState::LineComment
            }
        }
        LexState::BlockComment => {
            push_masked_char(out, ch);
            if ch == '*' && chars.peek().is_some_and(|&(_, next)| next == '/') {
                if let Some((_, next)) = chars.next() {
                    push_masked_char(out, next);
                }
                LexState::Normal
            } else {
                LexState::BlockComment
            }
        }
        LexState::StringLit { escaped } => {
            push_masked_char(out, ch);
            if escaped {
                LexState::StringLit { escaped: false }
            } else if ch == '\\' {
                LexState::StringLit { escaped: true }
            } else if ch == '"' {
                LexState::Normal
            } else {
                LexState::StringLit { escaped: false }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenKind {
    Ident(String),
    Dimension {
        raw: String,
        value: f64,
        unit: String,
    },
    Number(String),
    StringLit(String),
    ColorHex(String),
    Punct(char),
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PropertyAssignment {
    pub prop_name: String,
    pub rhs: Vec<Token>,
}

#[derive(Debug, Clone)]
pub(crate) struct ThemeRef {
    pub token_name: String,
    pub line: usize,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedSlintFile {
    pub assignments: Vec<PropertyAssignment>,
    pub theme_refs: Vec<ThemeRef>,
}

/// Tokenize a `.slint` source file while tracking 1-based line numbers and skipping comments.
pub(crate) fn tokenize_slint(input: &str) -> Vec<Token> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut line = 1usize;

    while i < bytes.len() {
        let (next_i, next_line, maybe_tok) = step_tokenize(input, bytes, i, line);
        i = next_i;
        line = next_line;
        if let Some(tok) = maybe_tok {
            tokens.push(tok);
        }
    }

    tokens
}

fn step_tokenize(
    input: &str,
    bytes: &[u8],
    i: usize,
    line: usize,
) -> (usize, usize, Option<Token>) {
    let b = bytes[i];
    if b == b'\n' {
        return (i + 1, line + 1, None);
    }
    if b.is_ascii_whitespace() {
        return (i + 1, line, None);
    }
    if i + 1 < bytes.len() && b == b'/' && bytes[i + 1] == b'/' {
        return (skip_line_comment(bytes, i), line, None);
    }
    if i + 1 < bytes.len() && b == b'/' && bytes[i + 1] == b'*' {
        let (next_i, added) = skip_block_comment(bytes, i);
        return (next_i, line + added, None);
    }
    if b == b'"' {
        let (lit, next_i, added) = read_string_lit(input, i);
        return (
            next_i,
            line + added,
            Some(Token {
                kind: TokenKind::StringLit(lit),
                line,
            }),
        );
    }
    if b == b'#' {
        let (hex, next_i) = read_hex_token(input, bytes, i);
        return (
            next_i,
            line,
            Some(Token {
                kind: TokenKind::ColorHex(hex),
                line,
            }),
        );
    }
    if b.is_ascii_digit() || (b == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit()) {
        let (tok, next_i) = read_number_or_dimension(input, i, line);
        return (next_i, line, Some(tok));
    }
    if b.is_ascii_alphabetic() || b == b'_' {
        let (ident, next_i) = read_ident(input, i);
        return (
            next_i,
            line,
            Some(Token {
                kind: TokenKind::Ident(ident),
                line,
            }),
        );
    }
    let ch = input[i..].chars().next().unwrap_or(' ');
    (
        i + ch.len_utf8(),
        line,
        Some(Token {
            kind: TokenKind::Punct(ch),
            line,
        }),
    )
}

fn skip_line_comment(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(bytes: &[u8], start: usize) -> (usize, usize) {
    let mut i = start + 2;
    let mut added = 0;
    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
        if bytes[i] == b'\n' {
            added += 1;
        }
        i += 1;
    }
    ((i + 2).min(bytes.len()), added)
}

fn read_hex_token(input: &str, bytes: &[u8], start: usize) -> (String, usize) {
    let mut i = start + 1;
    while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
        i += 1;
    }
    (input[start..i].to_string(), i)
}

fn read_string_lit(input: &str, start: usize) -> (String, usize, usize) {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    let mut escaped = false;
    let mut added_lines = 0;

    while i < bytes.len() {
        let ch = input[i..].chars().next().unwrap_or('"');
        i += ch.len_utf8();
        if ch == '\n' {
            added_lines += 1;
        }
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            break;
        } else {
            out.push(ch);
        }
    }
    (out, i, added_lines)
}

fn read_number_or_dimension(input: &str, start: usize, line: usize) -> (Token, usize) {
    let bytes = input.as_bytes();
    let mut i = start;
    let mut seen_dot = false;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            i += 1;
        } else if bytes[i] == b'.' && !seen_dot {
            seen_dot = true;
            i += 1;
        } else {
            break;
        }
    }
    let num_str = &input[start..i];
    let unit_start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'%') {
        i += 1;
    }
    let unit = &input[unit_start..i];
    if unit.is_empty() {
        (
            Token {
                kind: TokenKind::Number(num_str.to_string()),
                line,
            },
            i,
        )
    } else {
        let value = num_str.parse::<f64>().unwrap_or(0.0);
        (
            Token {
                kind: TokenKind::Dimension {
                    raw: format!("{num_str}{}", unit.to_ascii_lowercase()),
                    value,
                    unit: unit.to_ascii_lowercase(),
                },
                line,
            },
            i,
        )
    }
}

fn read_ident(input: &str, start: usize) -> (String, usize) {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        let b = bytes[i];
        let continue_hyphen = b == b'-'
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphanumeric() || bytes[i + 1] == b'_')
            && !looks_like_subtraction_rhs(&input[i + 1..]);
        if b.is_ascii_alphanumeric() || b == b'_' || continue_hyphen {
            i += 1;
        } else {
            break;
        }
    }
    (input[start..i].to_string(), i)
}

fn looks_like_subtraction_rhs(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return false;
    }
    let mut j = 0;
    while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
        j += 1;
    }
    j < bytes.len() && matches!(bytes[j], b'p' | b'r' | b'm' | b's' | b'%')
}

/// Extract property bindings (`prop: expr;`) and `Theme.<token>` references from a `.slint` file.
pub(crate) fn parse_slint_file(input: &str) -> ParsedSlintFile {
    let tokens = tokenize_slint(input);
    let mut parsed = ParsedSlintFile::default();

    for idx in 0..tokens.len().saturating_sub(2) {
        if let (TokenKind::Ident(obj), TokenKind::Punct('.'), TokenKind::Ident(prop)) = (
            &tokens[idx].kind,
            &tokens[idx + 1].kind,
            &tokens[idx + 2].kind,
        ) && obj == "Theme"
        {
            parsed.theme_refs.push(ThemeRef {
                token_name: normalize_slint_ident(prop),
                line: tokens[idx].line,
            });
        }
    }

    extract_assignments(&tokens, &mut parsed.assignments);
    parsed
}

fn extract_assignments(tokens: &[Token], out: &mut Vec<PropertyAssignment>) {
    let mut stmt_start = 0usize;
    let mut colon_idx: Option<usize> = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;

    for (idx, tok) in tokens.iter().enumerate() {
        match tok.kind {
            TokenKind::Punct('(') => paren_depth += 1,
            TokenKind::Punct(')') => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::Punct('[') => bracket_depth += 1,
            TokenKind::Punct(']') => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::Punct(':')
                if paren_depth == 0 && bracket_depth == 0 && colon_idx.is_none() =>
            {
                colon_idx = Some(idx);
            }
            TokenKind::Punct('{') => {
                stmt_start = idx + 1;
                colon_idx = None;
                paren_depth = 0;
                bracket_depth = 0;
            }
            TokenKind::Punct(';') | TokenKind::Punct('}') => {
                if let Some(cidx) = colon_idx
                    && cidx == stmt_start + 1
                    && let TokenKind::Ident(prop) = &tokens[stmt_start].kind
                    && cidx + 1 < idx
                {
                    out.push(PropertyAssignment {
                        prop_name: normalize_slint_ident(prop).to_ascii_lowercase(),
                        rhs: tokens[cidx + 1..idx].to_vec(),
                    });
                }
                stmt_start = idx + 1;
                colon_idx = None;
                paren_depth = 0;
                bracket_depth = 0;
            }
            _ => {}
        }
    }
}

/// Parse `export global Theme { ... }` / `global Theme { ... }` from a `theme.slint` file and list all exported property names.
pub(crate) fn extract_exported_theme_props(theme_slint: &str) -> BTreeSet<String> {
    let tokens = tokenize_slint(theme_slint);
    let mut props = BTreeSet::new();
    let Some(mut i) = find_theme_global_body(&tokens) else {
        return props;
    };

    let mut depth = 1usize;
    let mut saw_property_kw = false;
    let mut angle_depth = 0usize;

    while i < tokens.len() && depth > 0 {
        match &tokens[i].kind {
            TokenKind::Punct('{') => {
                depth += 1;
                saw_property_kw = false;
            }
            TokenKind::Punct('}') => {
                depth -= 1;
                saw_property_kw = false;
            }
            TokenKind::Punct('<') if depth == 1 => angle_depth += 1,
            TokenKind::Punct('>') if depth == 1 => angle_depth = angle_depth.saturating_sub(1),
            TokenKind::Punct(';') | TokenKind::Punct(':') if depth == 1 => {
                saw_property_kw = false;
            }
            TokenKind::Ident(id) if depth == 1 && id == "property" => {
                saw_property_kw = true;
            }
            TokenKind::Ident(id) if depth == 1 && saw_property_kw && angle_depth == 0 => {
                props.insert(normalize_slint_ident(id));
                saw_property_kw = false;
            }
            _ => {}
        }
        i += 1;
    }

    props
}

fn find_theme_global_body(tokens: &[Token]) -> Option<usize> {
    for i in 0..tokens.len().saturating_sub(2) {
        if let (TokenKind::Ident(kw), TokenKind::Ident(name), TokenKind::Punct('{')) =
            (&tokens[i].kind, &tokens[i + 1].kind, &tokens[i + 2].kind)
            && kw == "global"
            && name == "Theme"
        {
            return Some(i + 3);
        }
    }
    None
}

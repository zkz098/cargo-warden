//! Source-to-`.po` translation key extraction and placeholder consistency helpers.
//!
//! Extracts:
//! - Slint `@tr("...")`, `@tr("ctx" => "...")`, and `@tr("one" | "many" % count)` calls
//! - Rust `tr(...)`, `tr!(...)`, `t!(...)`, `gettext(...)` calls/macros via `syn` AST
//! - Placeholder signatures (`{}`, `{0}`, `{name}`, `%s`, etc.) for translation validation

use super::parallel;
use crate::walker::{WalkOptions, collect_files};
use anyhow::Result;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ExprCall, ExprMacro, ExprMethodCall, ItemFn, ItemMod, Lit, Stmt};

/// A translation lookup key discovered in source code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTranslationKey {
    pub ctx: Option<String>,
    pub msgid: String,
    pub msgid_plural: Option<String>,
    pub file: PathBuf,
    pub line: usize,
}

/// Normalized placeholder multiset for comparing `msgid` vs `msgstr`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaceholderSignature {
    pub positional: usize,
    pub named: BTreeMap<String, usize>,
    pub printf: BTreeMap<String, usize>,
}

/// Extract brace (`{}`, `{0}`, `{name}`) and printf (`%s`, `%d`, `%1$s`) placeholders.
pub fn placeholder_signature(text: &str) -> PlaceholderSignature {
    let mut sig = PlaceholderSignature::default();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            if bytes.get(i + 1) == Some(&b'{') {
                i += 2;
                continue;
            }
            if let Some((end, inner)) = scan_brace_placeholder(text, i + 1) {
                let key = inner.split([':', '!']).next().unwrap_or("").trim();
                if key.is_empty() {
                    sig.positional += 1;
                } else {
                    *sig.named.entry(key.to_string()).or_insert(0) += 1;
                }
                i = end + 1;
                continue;
            }
        } else if bytes[i] == b'}' && bytes.get(i + 1) == Some(&b'}') {
            i += 2;
            continue;
        } else if bytes[i] == b'%' {
            if bytes.get(i + 1) == Some(&b'%') {
                i += 2;
                continue;
            }
            if let Some((end, spec)) = scan_printf_placeholder(bytes, i + 1) {
                *sig.printf.entry(spec).or_insert(0) += 1;
                i = end;
                continue;
            }
        }
        i += 1;
    }

    sig
}

fn scan_brace_placeholder(text: &str, start: usize) -> Option<(usize, &str)> {
    let rest = text.get(start..)?;
    let close_rel = rest.find('}')?;
    let inner = &rest[..close_rel];
    if inner.contains('{') || inner.contains('\n') {
        return None;
    }
    Some((start + close_rel, inner))
}

fn scan_printf_placeholder(bytes: &[u8], start: usize) -> Option<(usize, String)> {
    let mut i = start;
    if i >= bytes.len() {
        return None;
    }
    // Optional positional index like 1$
    let pos_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'$' && i > pos_start {
        i += 1;
    } else {
        i = start;
    }
    while i < bytes.len() && b"-+ #0".contains(&bytes[i]) {
        i += 1;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    while i < bytes.len() && b"hlLzjt".contains(&bytes[i]) {
        i += 1;
    }
    if i < bytes.len() && b"diuoxXfFeEgGaAcspn".contains(&bytes[i]) {
        let end = i + 1;
        let spec = String::from_utf8_lossy(&bytes[start - 1..end]).into_owned();
        return Some((end, spec));
    }
    None
}

pub fn placeholders_match(source: &str, translated: &str) -> bool {
    placeholder_signature(source) == placeholder_signature(translated)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum I18nSourceKind {
    Rust,
    Slint,
}

#[derive(Debug, Clone)]
pub(crate) struct I18nSourceFile {
    pub(crate) path: PathBuf,
    pub(crate) kind: I18nSourceKind,
}

pub(crate) fn resolve_tr_macro_names(tr_macros: &[String]) -> Vec<String> {
    if tr_macros.is_empty() {
        vec!["tr".to_string(), "t".to_string(), "gettext".to_string()]
    } else {
        tr_macros.to_vec()
    }
}

pub(crate) fn discover_i18n_source_files(
    source_dirs: &[PathBuf],
    out_file: &Path,
    respect_gitignore: bool,
    base_dir: Option<&Path>,
) -> Vec<I18nSourceFile> {
    if source_dirs.is_empty() {
        return Vec::new();
    }
    let exts = vec!["rs".to_string(), "slint".to_string()];
    let files = collect_files(
        source_dirs,
        &WalkOptions {
            extensions: &exts,
            ignore_patterns: &[],
            respect_gitignore,
            base_dir,
            changeset: None,
        },
    );

    let out_canon = fs::canonicalize(out_file).ok();
    let mut out = Vec::new();
    for file in files {
        if is_generated_catalog(&file, out_file, out_canon.as_deref()) {
            continue;
        }
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "slint" {
            out.push(I18nSourceFile {
                path: file,
                kind: I18nSourceKind::Slint,
            });
        } else if ext == "rs" && !is_rust_test_file(&file) {
            out.push(I18nSourceFile {
                path: file,
                kind: I18nSourceKind::Rust,
            });
        }
    }
    out
}

/// Scan `source_dirs` for `.slint` `@tr(...)` and `.rs` translation macro/function calls.
pub fn collect_source_keys(
    source_dirs: &[PathBuf],
    tr_macros: &[String],
    out_file: &Path,
    respect_gitignore: bool,
    base_dir: Option<&Path>,
) -> Result<Vec<SourceTranslationKey>> {
    let files = discover_i18n_source_files(source_dirs, out_file, respect_gitignore, base_dir);
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let macro_owned = resolve_tr_macro_names(tr_macros);
    let macro_names: Vec<&str> = macro_owned.iter().map(String::as_str).collect();

    let per_file = parallel::map_items(&files, true, |item| -> Result<Vec<SourceTranslationKey>> {
        let content = fs::read_to_string(&item.path)?;
        Ok(match item.kind {
            I18nSourceKind::Slint => extract_slint_keys(&item.path, &content),
            I18nSourceKind::Rust => extract_rust_keys(&item.path, &content, &macro_names),
        })
    });

    let mut keys = Vec::new();
    for res in per_file {
        keys.extend(res?);
    }
    Ok(keys)
}

fn is_generated_catalog(file: &Path, out_file: &Path, out_canon: Option<&Path>) -> bool {
    if file == out_file {
        return true;
    }
    if let (Some(target), Ok(curr)) = (out_canon, fs::canonicalize(file)) {
        return target == curr;
    }
    false
}

fn is_rust_test_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == "tests.rs" || name.ends_with("_test.rs") || name.ends_with("_tests.rs") {
        return true;
    }
    path.components()
        .any(|c| matches!(c, std::path::Component::Normal(seg) if seg == "tests"))
}

/// Extract `@tr(...)` entries from Slint markup, ignoring `//` and `/* */` comments.
pub fn extract_slint_keys(file: &Path, content: &str) -> Vec<SourceTranslationKey> {
    let bytes = content.as_bytes();
    let mut i = 0;
    let mut line = 1usize;
    let mut keys = Vec::new();

    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line += 1;
            i += 1;
        } else if bytes.get(i..i + 2) == Some(b"//") {
            i = skip_line_comment(bytes, i + 2);
        } else if bytes.get(i..i + 2) == Some(b"/*") {
            let (next_i, added_lines) = skip_block_comment(bytes, i + 2);
            line += added_lines;
            i = next_i;
        } else if bytes[i] == b'"' {
            let (next_i, added_lines) = skip_quoted_bytes(bytes, i + 1);
            line += added_lines;
            i = next_i;
        } else if content[i..].starts_with("@tr") {
            let after_kw = skip_ws_and_comments_idx(bytes, i + 3);
            if bytes.get(after_kw.0) == Some(&b'(') {
                if let Some((key, next_i, added_lines)) =
                    parse_slint_tr_call(file, content, after_kw.0 + 1, line + after_kw.1)
                {
                    keys.push(key);
                    line += after_kw.1 + added_lines;
                    i = next_i;
                    continue;
                }
            }
            i += 3;
        } else {
            i += 1;
        }
    }

    keys
}

fn skip_line_comment(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(bytes: &[u8], mut i: usize) -> (usize, usize) {
    let mut lines = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            lines += 1;
        } else if bytes.get(i..i + 2) == Some(b"*/") {
            return (i + 2, lines);
        }
        i += 1;
    }
    (i, lines)
}

fn skip_quoted_bytes(bytes: &[u8], mut i: usize) -> (usize, usize) {
    let mut lines = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            lines += 1;
            i += 1;
        } else if bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == b'"' {
            return (i + 1, lines);
        } else {
            i += 1;
        }
    }
    (i, lines)
}

fn skip_ws_and_comments_idx(bytes: &[u8], mut i: usize) -> (usize, usize) {
    let mut lines = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            lines += 1;
            i += 1;
        } else if bytes[i].is_ascii_whitespace() {
            i += 1;
        } else if bytes.get(i..i + 2) == Some(b"//") {
            i = skip_line_comment(bytes, i + 2);
        } else if bytes.get(i..i + 2) == Some(b"/*") {
            let (next_i, added) = skip_block_comment(bytes, i + 2);
            lines += added;
            i = next_i;
        } else {
            break;
        }
    }
    (i, lines)
}

fn parse_slint_tr_call(
    file: &Path,
    content: &str,
    arg_start: usize,
    call_line: usize,
) -> Option<(SourceTranslationKey, usize, usize)> {
    let bytes = content.as_bytes();
    let (pos, mut lines) = skip_ws_and_comments_idx(bytes, arg_start);
    let (first, after_first, l1) = parse_slint_string(content, pos)?;
    lines += l1;

    let (pos2, l2) = skip_ws_and_comments_idx(bytes, after_first);
    lines += l2;

    let (ctx, msgid, after_msg) = if bytes.get(pos2..pos2 + 2) == Some(b"=>") {
        let (pos3, l3) = skip_ws_and_comments_idx(bytes, pos2 + 2);
        lines += l3;
        let (msg, end_msg, l4) = parse_slint_string(content, pos3)?;
        lines += l4;
        (Some(first), msg, end_msg)
    } else {
        (None, first, pos2)
    };

    let (pos_p, lp) = skip_ws_and_comments_idx(bytes, after_msg);
    lines += lp;
    let (msgid_plural, after_plural) = if bytes.get(pos_p) == Some(&b'|') {
        let (pos_p2, lp2) = skip_ws_and_comments_idx(bytes, pos_p + 1);
        lines += lp2;
        if let Some((plural, end_pl, lpl)) = parse_slint_string(content, pos_p2) {
            lines += lpl;
            (Some(plural), end_pl)
        } else {
            (None, pos_p)
        }
    } else {
        (None, pos_p)
    };

    Some((
        SourceTranslationKey {
            ctx,
            msgid,
            msgid_plural,
            file: file.to_path_buf(),
            line: call_line,
        },
        after_plural,
        lines,
    ))
}

fn parse_slint_string(content: &str, start: usize) -> Option<(String, usize, usize)> {
    let bytes = content.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = content[start + 1..].char_indices();
    let mut lines = 0;

    while let Some((rel_idx, ch)) = chars.next() {
        if ch == '"' {
            return Some((out, start + 1 + rel_idx + 1, lines));
        }
        if ch == '\n' {
            lines += 1;
            out.push('\n');
            continue;
        }
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some((_, 'n')) => out.push('\n'),
            Some((_, 't')) => out.push('\t'),
            Some((_, 'r')) => out.push('\r'),
            Some((_, '"')) => out.push('"'),
            Some((_, '\\')) => out.push('\\'),
            Some((_, '{')) => out.push('{'),
            Some((_, '}')) => out.push('}'),
            Some((_, other)) => {
                out.push('\\');
                out.push(other);
            }
            None => return None,
        }
    }
    None
}

/// Extract translation keys from an already-parsed Rust AST, ignoring `#[cfg(test)]`.
pub(crate) fn extract_rust_keys_from_ast(
    file: &Path,
    ast: &syn::File,
    tr_macros: &[&str],
) -> Vec<SourceTranslationKey> {
    let mut visitor = RustTrVisitor {
        file,
        tr_macros,
        keys: Vec::new(),
    };
    visitor.visit_file(ast);
    visitor.keys
}

/// Extract translation keys from a Rust file via `syn`, ignoring `#[cfg(test)]`.
pub fn extract_rust_keys(
    file: &Path,
    content: &str,
    tr_macros: &[&str],
) -> Vec<SourceTranslationKey> {
    let Ok(ast) = syn::parse_file(content) else {
        return Vec::new();
    };
    extract_rust_keys_from_ast(file, &ast, tr_macros)
}

struct RustTrVisitor<'a> {
    file: &'a Path,
    tr_macros: &'a [&'a str],
    keys: Vec<SourceTranslationKey>,
}

impl<'a> RustTrVisitor<'a> {
    fn matches_name(&self, name: &str) -> bool {
        self.tr_macros.contains(&name)
    }

    fn record_lit_str(&mut self, ctx: Option<String>, lit: &syn::LitStr) {
        self.keys.push(SourceTranslationKey {
            ctx,
            msgid: lit.value(),
            msgid_plural: None,
            file: self.file.to_path_buf(),
            line: lit.span().start().line,
        });
    }

    fn record_from_tokens(&mut self, tokens: proc_macro2::TokenStream) {
        let mut iter = tokens.into_iter().peekable();
        if let Some(proc_macro2::TokenTree::Literal(first_lit)) = iter.next()
            && let Ok(lit) =
                syn::parse2::<syn::LitStr>(proc_macro2::TokenTree::Literal(first_lit).into())
        {
            if let (
                Some(proc_macro2::TokenTree::Punct(p1)),
                Some(proc_macro2::TokenTree::Punct(p2)),
            ) = (iter.next(), iter.next())
                && p1.as_char() == '='
                && p2.as_char() == '>'
                && let Some(proc_macro2::TokenTree::Literal(second)) = iter.next()
                && let Ok(msg_lit) =
                    syn::parse2::<syn::LitStr>(proc_macro2::TokenTree::Literal(second).into())
            {
                self.record_lit_str(Some(lit.value()), &msg_lit);
                return;
            }
            self.record_lit_str(None, &lit);
        }
    }
}

impl<'ast> Visit<'ast> for RustTrVisitor<'_> {
    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        if has_cfg_test(&node.attrs) {
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if has_cfg_test(&node.attrs) {
            return;
        }
        visit::visit_item_fn(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let Expr::Path(path_expr) = node.func.as_ref()
            && let Some(last) = path_expr.path.segments.last()
            && self.matches_name(&last.ident.to_string())
            && let Some(Expr::Lit(expr_lit)) = node.args.first()
            && let Lit::Str(lit) = &expr_lit.lit
        {
            self.record_lit_str(None, lit);
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if self.matches_name(&node.method.to_string())
            && let Some(Expr::Lit(expr_lit)) = node.args.first()
            && let Lit::Str(lit) = &expr_lit.lit
        {
            self.record_lit_str(None, lit);
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_macro(&mut self, node: &'ast ExprMacro) {
        if let Some(last) = node.mac.path.segments.last()
            && self.matches_name(&last.ident.to_string())
        {
            self.record_from_tokens(node.mac.tokens.clone());
        }
        visit::visit_expr_macro(self, node);
    }

    fn visit_stmt(&mut self, node: &'ast Stmt) {
        if let Stmt::Macro(stmt_mac) = node
            && let Some(last) = stmt_mac.mac.path.segments.last()
            && self.matches_name(&last.ident.to_string())
        {
            self.record_from_tokens(stmt_mac.mac.tokens.clone());
        }
        visit::visit_stmt(self, node);
    }
}

fn has_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("test") {
            return true;
        }
        if attr.path().is_ident("cfg") {
            let mut is_test = false;
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("test") {
                    is_test = true;
                }
                Ok(())
            });
            return is_test;
        }
        false
    })
}

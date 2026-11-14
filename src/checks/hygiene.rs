//! Rust production code hygiene and safety guard.
//!
//! Scans Rust ASTs for:
//! - `unsafe` blocks, functions, traits, and impls (either forbidden or missing `// SAFETY:` comments)
//! - `.unwrap()` / `.unwrap_err()` and `.expect()` / `.expect_err()` calls
//! - `dbg!()`, `todo!()`, `unimplemented!()`, and `println!()` / `eprintln!()` / `print!()` / `eprint!()` macros

use super::parallel;
use crate::config::{HygieneConfig, HygieneOverride};
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use crate::suppression::{FileSuppressions, collect_unused_suppression_diagnostics};
use crate::walker::{PathPatternMatcher, WalkOptions, collect_files};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, ExprMacro, ExprMethodCall, ExprUnsafe, ImplItemFn, Item, ItemFn, ItemImpl,
    ItemTrait, Macro, StmtMacro,
};

pub(crate) const OWNED_HYGIENE_RULES: &[&str] = &[
    "hygiene", "unsafe", "safety", "unwrap", "expect", "dbg", "todo", "println",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HygieneKind {
    Unsafe,
    Unwrap,
    Expect,
    Dbg,
    Todo,
    Println,
}

#[derive(Debug, Clone)]
pub(crate) struct HygieneFinding {
    pub kind: HygieneKind,
    pub line: usize,
    pub detail: String,
    pub has_safety_comment: bool,
}

#[derive(Debug, Clone)]
pub struct HygieneOptions<'a> {
    pub base_dir: &'a Path,
    pub config: &'a HygieneConfig,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

#[derive(Debug, Clone, Copy)]
struct EffectiveHygieneRules {
    forbid_unsafe: bool,
    require_safety_comment: bool,
    forbid_unwrap: bool,
    forbid_expect: bool,
    forbid_dbg: bool,
    forbid_todo: bool,
    forbid_println: bool,
}

impl EffectiveHygieneRules {
    fn from_config(rel_path: &Path, cfg: &HygieneConfig) -> Self {
        let mut rules = Self {
            forbid_unsafe: cfg.forbid_unsafe,
            require_safety_comment: cfg.require_safety_comment,
            forbid_unwrap: cfg.forbid_unwrap,
            forbid_expect: cfg.forbid_expect,
            forbid_dbg: cfg.forbid_dbg,
            forbid_todo: cfg.forbid_todo,
            forbid_println: cfg.forbid_println,
        };
        apply_overrides(rel_path, &cfg.overrides, &mut rules);
        rules
    }
}

fn apply_overrides(
    rel_path: &Path,
    overrides: &[HygieneOverride],
    rules: &mut EffectiveHygieneRules,
) {
    for ov in overrides {
        let matcher = PathPatternMatcher::new(&ov.paths);
        if !matcher.matches(rel_path) {
            continue;
        }
        if let Some(v) = ov.forbid_unsafe {
            rules.forbid_unsafe = v;
        }
        if let Some(v) = ov.require_safety_comment {
            rules.require_safety_comment = v;
        }
        if let Some(v) = ov.forbid_unwrap {
            rules.forbid_unwrap = v;
        }
        if let Some(v) = ov.forbid_expect {
            rules.forbid_expect = v;
        }
        if let Some(v) = ov.forbid_dbg {
            rules.forbid_dbg = v;
        }
        if let Some(v) = ov.forbid_todo {
            rules.forbid_todo = v;
        }
        if let Some(v) = ov.forbid_println {
            rules.forbid_println = v;
        }
    }
}

fn has_test_attr(attrs: &[Attribute]) -> bool {
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

fn item_attrs(item: &Item) -> Option<&[Attribute]> {
    match item {
        Item::Const(i) => Some(&i.attrs),
        Item::Enum(i) => Some(&i.attrs),
        Item::ExternCrate(i) => Some(&i.attrs),
        Item::Fn(i) => Some(&i.attrs),
        Item::ForeignMod(i) => Some(&i.attrs),
        Item::Impl(i) => Some(&i.attrs),
        Item::Macro(i) => Some(&i.attrs),
        Item::Mod(i) => Some(&i.attrs),
        Item::Static(i) => Some(&i.attrs),
        Item::Struct(i) => Some(&i.attrs),
        Item::Trait(i) => Some(&i.attrs),
        Item::TraitAlias(i) => Some(&i.attrs),
        Item::Type(i) => Some(&i.attrs),
        Item::Union(i) => Some(&i.attrs),
        Item::Use(i) => Some(&i.attrs),
        _ => None,
    }
}

fn line_has_safety_marker(line: &str) -> bool {
    line.to_ascii_uppercase().contains("SAFETY:")
}

fn has_safety_comment(lines: &[&str], line_1based: usize) -> bool {
    if line_1based == 0 || line_1based > lines.len() {
        return false;
    }
    let idx = line_1based - 1;
    if line_has_safety_marker(lines[idx]) {
        return true;
    }

    // Walk upwards through contiguous comments and `#[...]` attributes.
    let mut cur = idx;
    while cur > 0 {
        cur -= 1;
        let trimmed = lines[cur].trim();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            if line_has_safety_marker(trimmed) {
                return true;
            }
            continue;
        }
        if trimmed.starts_with("#[") {
            continue;
        }
        break;
    }

    // Also allow `// SAFETY:` on the first non-empty comment line inside `unsafe { ... }`.
    let mut next = idx + 1;
    while next < lines.len() {
        let trimmed = lines[next].trim();
        if trimmed.is_empty() {
            next += 1;
            continue;
        }
        if (trimmed.starts_with("//") || trimmed.starts_with("/*"))
            && line_has_safety_marker(trimmed)
        {
            return true;
        }
        break;
    }

    false
}

struct HygieneVisitor<'a> {
    lines: Vec<&'a str>,
    ignore_tests: bool,
    findings: Vec<HygieneFinding>,
}

impl<'a> HygieneVisitor<'a> {
    fn new(content: &'a str, ignore_tests: bool) -> Self {
        Self {
            lines: content.lines().collect(),
            ignore_tests,
            findings: Vec::new(),
        }
    }

    fn record_unsafe(&mut self, line: usize, detail: &str) {
        let has_safety_comment = has_safety_comment(&self.lines, line);
        self.findings.push(HygieneFinding {
            kind: HygieneKind::Unsafe,
            line,
            detail: detail.to_string(),
            has_safety_comment,
        });
    }

    fn inspect_macro(&mut self, mac: &Macro) {
        let Some(seg) = mac.path.segments.last() else {
            return;
        };
        let name = seg.ident.to_string();
        let line = seg.ident.span().start().line;
        let kind = match name.as_str() {
            "dbg" => HygieneKind::Dbg,
            "todo" | "unimplemented" => HygieneKind::Todo,
            "println" | "eprintln" | "print" | "eprint" => HygieneKind::Println,
            _ => return,
        };
        self.findings.push(HygieneFinding {
            kind,
            line,
            detail: format!("{name}!()"),
            has_safety_comment: false,
        });
    }
}

impl<'ast> Visit<'ast> for HygieneVisitor<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if self.ignore_tests && item_attrs(item).is_some_and(has_test_attr) {
            return;
        }
        visit::visit_item(self, item);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if let syn::Safety::Unsafe(unsafety) = node.sig.safety {
            self.record_unsafe(
                unsafety.span().start().line,
                &format!("unsafe fn `{}`", node.sig.ident),
            );
        }
        visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        if let syn::Safety::Unsafe(unsafety) = node.sig.safety {
            self.record_unsafe(
                unsafety.span().start().line,
                &format!("unsafe fn `{}`", node.sig.ident),
            );
        }
        visit::visit_impl_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if let Some(unsafety) = node.unsafety {
            self.record_unsafe(unsafety.span().start().line, "unsafe impl");
        }
        visit::visit_item_impl(self, node);
    }

    fn visit_item_trait(&mut self, node: &'ast ItemTrait) {
        if let Some(unsafety) = node.unsafety {
            self.record_unsafe(
                unsafety.span().start().line,
                &format!("unsafe trait `{}`", node.ident),
            );
        }
        visit::visit_item_trait(self, node);
    }

    fn visit_expr_unsafe(&mut self, node: &'ast ExprUnsafe) {
        self.record_unsafe(node.unsafe_token.span().start().line, "unsafe block");
        visit::visit_expr_unsafe(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let method = node.method.to_string();
        let line = node.method.span().start().line;
        if (method == "unwrap" || method == "unwrap_err") && node.args.is_empty() {
            self.findings.push(HygieneFinding {
                kind: HygieneKind::Unwrap,
                line,
                detail: format!(".{method}()"),
                has_safety_comment: false,
            });
        } else if (method == "expect" || method == "expect_err") && node.args.len() == 1 {
            self.findings.push(HygieneFinding {
                kind: HygieneKind::Expect,
                line,
                detail: format!(".{method}(...)"),
                has_safety_comment: false,
            });
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_macro(&mut self, node: &'ast ExprMacro) {
        self.inspect_macro(&node.mac);
        visit::visit_expr_macro(self, node);
    }

    fn visit_stmt_macro(&mut self, node: &'ast StmtMacro) {
        self.inspect_macro(&node.mac);
        visit::visit_stmt_macro(self, node);
    }
}

pub(crate) fn scan_ast_hygiene(
    syntax: &syn::File,
    content: &str,
    ignore_tests: bool,
) -> Vec<HygieneFinding> {
    if ignore_tests && has_test_attr(&syntax.attrs) {
        return Vec::new();
    }
    let mut visitor = HygieneVisitor::new(content, ignore_tests);
    visitor.visit_file(syntax);
    visitor.findings
}

pub(crate) fn build_hygiene_diagnostics(
    path: &Path,
    findings: &[HygieneFinding],
    suppressions: &FileSuppressions,
    base_dir: &Path,
    cfg: &HygieneConfig,
) -> Vec<Diagnostic> {
    let rel = normalize_rel_path(base_dir, path);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let rules = EffectiveHygieneRules::from_config(&rel, cfg);
    let mut diagnostics = Vec::new();

    for f in findings {
        let (rule_id, suppress_key, msg): (&str, &str, String) = match f.kind {
            HygieneKind::Unsafe if rules.forbid_unsafe => (
                "warden/hygiene/forbid-unsafe",
                "unsafe",
                format!("{}:{} - forbidden {}", rel.display(), f.line, f.detail),
            ),
            HygieneKind::Unsafe if rules.require_safety_comment && !f.has_safety_comment => (
                "warden/hygiene/unsafe-without-safety",
                "safety",
                format!(
                    "{}:{} - {} is missing a `// SAFETY:` comment",
                    rel.display(),
                    f.line,
                    f.detail
                ),
            ),
            HygieneKind::Unsafe => continue,
            HygieneKind::Unwrap if rules.forbid_unwrap => (
                "warden/hygiene/unwrap",
                "unwrap",
                format!(
                    "{}:{} - `{}` used in production code",
                    rel.display(),
                    f.line,
                    f.detail
                ),
            ),
            HygieneKind::Expect if rules.forbid_expect => (
                "warden/hygiene/expect",
                "expect",
                format!(
                    "{}:{} - `{}` used in production code",
                    rel.display(),
                    f.line,
                    f.detail
                ),
            ),
            HygieneKind::Dbg if rules.forbid_dbg => (
                "warden/hygiene/dbg",
                "dbg",
                format!(
                    "{}:{} - `{}` debug macro left in code",
                    rel.display(),
                    f.line,
                    f.detail
                ),
            ),
            HygieneKind::Todo if rules.forbid_todo => (
                "warden/hygiene/todo",
                "todo",
                format!(
                    "{}:{} - `{}` placeholder macro left in code",
                    rel.display(),
                    f.line,
                    f.detail
                ),
            ),
            HygieneKind::Println if rules.forbid_println => (
                "warden/hygiene/println",
                "println",
                format!(
                    "{}:{} - `{}` stdout/stderr macro used in code",
                    rel.display(),
                    f.line,
                    f.detail
                ),
            ),
            _ => continue,
        };

        let suppressed = suppressions.is_suppressed("hygiene", Some(f.line))
            || suppressions.is_suppressed(suppress_key, Some(f.line))
            || (f.kind == HygieneKind::Unsafe
                && suppressions.is_suppressed("unsafe", Some(f.line)));

        diagnostics.push(Diagnostic {
            rule_id: rule_id.to_string(),
            check: "hygiene".to_string(),
            severity: Severity::Error,
            message: msg,
            file: Some(rel.clone()),
            line: Some(f.line),
            column: None,
            fingerprint: format!("hygiene:{suppress_key}:{rel_str}:{}:{}", f.line, f.detail),
            metric_value: None,
            suppressed,
            suppression_reason: suppressed.then(|| "inline".to_string()),
        });
    }

    diagnostics.extend(collect_unused_suppression_diagnostics(
        &rel,
        suppressions,
        OWNED_HYGIENE_RULES,
        &diagnostics,
    ));
    diagnostics
}

pub(crate) fn discover_hygiene_files(paths: &[PathBuf], opts: &HygieneOptions<'_>) -> Vec<PathBuf> {
    let rust_exts = vec!["rs".to_string()];
    let files = collect_files(
        paths,
        &WalkOptions {
            extensions: &rust_exts,
            ignore_patterns: &opts.config.ignore,
            respect_gitignore: opts.config.respect_gitignore,
            base_dir: Some(opts.base_dir),
            changeset: opts.changeset,
        },
    );

    if !opts.config.ignore_tests {
        return files;
    }

    files
        .into_iter()
        .filter(|path| {
            let path_str = path.to_string_lossy().replace('\\', "/");
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !(path_str.contains("/tests/")
                || file_name.ends_with("_test.rs")
                || file_name.ends_with("_tests.rs")
                || file_name == "tests.rs")
        })
        .collect()
}

/// Collect hygiene and safety diagnostics across configured paths.
pub fn collect_hygiene(
    paths: &[PathBuf],
    opts: &HygieneOptions<'_>,
) -> Result<(usize, Vec<Diagnostic>)> {
    if opts.emit_text {
        println!("[hygiene] scanning Rust source files for code hygiene & safety...");
    }

    let files = discover_hygiene_files(paths, opts);
    let per_file = parallel::map_items(&files, true, |path| {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))
            .ok()?;
        let syntax = syn::parse_file(&content).ok()?;
        let findings = scan_ast_hygiene(&syntax, &content, opts.config.ignore_tests);
        let suppressions = FileSuppressions::parse(&content);
        Some(build_hygiene_diagnostics(
            path,
            &findings,
            &suppressions,
            opts.base_dir,
            opts.config,
        ))
    });

    let mut files_scanned = 0;
    let mut diagnostics = Vec::new();
    for diags in per_file.into_iter().flatten() {
        files_scanned += 1;
        diagnostics.extend(diags);
    }

    Ok((files_scanned, diagnostics))
}

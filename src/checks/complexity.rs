//! Cognitive Complexity checker for Rust functions and methods.
//!
//! Follows G. Ann Campbell's Cognitive Complexity specification (SonarSource):
//! - Increments for breaks in linear control flow (`if`, `match`, `for`, `while`, `loop`).
//! - Nesting increments when control structures or closures are nested.
//! - Boolean operator transitions (e.g. sequence of `&&` then `||`).
//! - Labeled break/continue statements.

use super::parallel;
use crate::config::ComplexityOverride;
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use crate::suppression::{FileSuppressions, collect_unused_suppression_diagnostics};
use crate::walker::{PathPatternMatcher, WalkOptions, collect_files};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{
    BinOp, Block, Expr, ExprBinary, ExprBreak, ExprClosure, ExprContinue, ExprForLoop, ExprIf,
    ExprLoop, ExprMatch, ExprWhile, ImplItemFn, ItemFn, Pat, Signature,
};

#[derive(Debug, Clone)]
pub struct ComplexityReport {
    pub file: PathBuf,
    pub function_name: String,
    pub line: usize,
    pub score: usize,
    pub line_count: usize,
    pub max_nesting: usize,
}

#[derive(Default)]
struct ComplexityCalculator {
    score: usize,
    nesting: usize,
    max_nesting: usize,
    is_else_if: bool,
    last_bool_op: Option<BoolOpKind>,
}

impl ComplexityCalculator {
    fn enter_nesting(&mut self) {
        self.nesting += 1;
        if self.nesting > self.max_nesting {
            self.max_nesting = self.nesting;
        }
    }

    fn exit_nesting(&mut self) {
        self.nesting -= 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoolOpKind {
    And,
    Or,
}

impl<'ast> Visit<'ast> for ComplexityCalculator {
    fn visit_expr_if(&mut self, node: &'ast ExprIf) {
        if self.is_else_if {
            self.score += 1;
            self.is_else_if = false;
        } else {
            self.score += 1 + self.nesting;
        }

        let old_op = self.last_bool_op.take();
        self.visit_expr(&node.cond);
        self.last_bool_op = old_op;

        self.enter_nesting();
        self.visit_block(&node.then_branch);
        self.exit_nesting();

        if let Some((_, else_expr)) = &node.else_branch {
            match else_expr.as_ref() {
                Expr::If(child_if) => {
                    self.is_else_if = true;
                    self.visit_expr_if(child_if);
                }
                _ => {
                    self.score += 1;
                    self.enter_nesting();
                    self.visit_expr(else_expr);
                    self.exit_nesting();
                }
            }
        }
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        self.score += 1 + self.nesting;
        self.visit_expr(&node.expr);

        self.enter_nesting();
        for arm in &node.arms {
            if let Pat::Guard(guard_pat) = &arm.pat {
                self.score += 1;
                self.visit_expr(&guard_pat.guard);
            }
            self.visit_expr(&arm.body);
        }
        self.exit_nesting();
    }

    fn visit_expr_for_loop(&mut self, node: &'ast ExprForLoop) {
        self.score += 1 + self.nesting;
        self.enter_nesting();
        visit::visit_expr_for_loop(self, node);
        self.exit_nesting();
    }

    fn visit_expr_while(&mut self, node: &'ast ExprWhile) {
        self.score += 1 + self.nesting;
        self.enter_nesting();
        visit::visit_expr_while(self, node);
        self.exit_nesting();
    }

    fn visit_expr_loop(&mut self, node: &'ast ExprLoop) {
        self.score += 1 + self.nesting;
        self.enter_nesting();
        visit::visit_expr_loop(self, node);
        self.exit_nesting();
    }

    fn visit_expr_closure(&mut self, node: &'ast ExprClosure) {
        self.enter_nesting();
        visit::visit_expr_closure(self, node);
        self.exit_nesting();
    }

    fn visit_expr_break(&mut self, node: &'ast ExprBreak) {
        if node.label.is_some() {
            self.score += 1;
        }
    }

    fn visit_expr_continue(&mut self, node: &'ast ExprContinue) {
        if node.label.is_some() {
            self.score += 1;
        }
    }

    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        let current_op = match node.op {
            BinOp::And(_) => Some(BoolOpKind::And),
            BinOp::Or(_) => Some(BoolOpKind::Or),
            _ => None,
        };

        if let Some(op) = current_op
            && self.last_bool_op != Some(op)
        {
            self.score += 1;
            self.last_bool_op = Some(op);
        }

        visit::visit_expr_binary(self, node);
    }
}

struct FileAstScanner {
    file: PathBuf,
    reports: Vec<ComplexityReport>,
}

impl FileAstScanner {
    fn record_fn(&mut self, sig: &Signature, block: &Block) {
        let mut calc = ComplexityCalculator::default();
        calc.visit_block(block);

        let name = sig.ident.to_string();
        let line = sig.ident.span().start().line;
        let fn_start = sig.fn_token.span.start().line;
        let fn_end = block.brace_token.span.close().end().line;
        let line_count = fn_end.saturating_sub(fn_start) + 1;

        tracing::trace!(
            function = %name,
            line,
            score = calc.score,
            line_count,
            max_nesting = calc.max_nesting,
            "function metrics calculated"
        );

        self.reports.push(ComplexityReport {
            file: self.file.clone(),
            function_name: name,
            line,
            score: calc.score,
            line_count,
            max_nesting: calc.max_nesting,
        });
    }
}

impl<'ast> Visit<'ast> for FileAstScanner {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.record_fn(&node.sig, &node.block);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        self.record_fn(&node.sig, &node.block);
    }
}

/// Options for running the cognitive complexity check.
#[derive(Debug, Clone)]
pub struct ComplexityOptions<'a> {
    pub base_dir: &'a Path,
    pub max_complexity: usize,
    pub ignore_tests: bool,
    pub ignore_patterns: &'a [String],
    pub respect_gitignore: bool,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

/// Additional function metrics and path-scoped overrides for complexity checks.
#[derive(Debug, Clone, Default)]
pub struct ComplexityPolicy<'a> {
    pub max_function_lines: Option<usize>,
    pub max_nesting: Option<usize>,
    pub overrides: &'a [ComplexityOverride],
}

#[derive(Debug, Clone, Copy)]
struct EffectiveLimits {
    max_complexity: usize,
    max_function_lines: Option<usize>,
    max_nesting: Option<usize>,
}

fn resolve_limits(
    rel_path: &Path,
    base_max: usize,
    policy: &ComplexityPolicy<'_>,
) -> EffectiveLimits {
    let mut limits = EffectiveLimits {
        max_complexity: base_max,
        max_function_lines: policy.max_function_lines,
        max_nesting: policy.max_nesting,
    };
    for ov in policy.overrides {
        let matcher = PathPatternMatcher::new(&ov.paths);
        if matcher.matches(rel_path) {
            if let Some(m) = ov.max {
                limits.max_complexity = m;
            }
            if let Some(fl) = ov.max_function_lines {
                limits.max_function_lines = Some(fl);
            }
            if let Some(mn) = ov.max_nesting {
                limits.max_nesting = Some(mn);
            }
        }
    }
    limits
}

fn analyze_file_with_content(path: &Path) -> Result<(Vec<ComplexityReport>, FileSuppressions)> {
    tracing::debug!(path = %path.display(), "analyzing rust file for cognitive complexity");
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    let syntax: syn::File = syn::parse_file(&content)
        .with_context(|| format!("Failed to parse Rust syntax in {}", path.display()))?;

    let reports = scan_ast_reports(path, &syntax);
    let suppressions = FileSuppressions::parse(&content);
    Ok((reports, suppressions))
}

pub(crate) fn scan_ast_reports(path: &Path, syntax: &syn::File) -> Vec<ComplexityReport> {
    let mut scanner = FileAstScanner {
        file: path.to_path_buf(),
        reports: Vec::new(),
    };
    scanner.visit_file(syntax);
    scanner.reports
}

pub(crate) fn build_complexity_diagnostics_with_policy(
    path: &Path,
    reports: &[ComplexityReport],
    suppressions: &FileSuppressions,
    opts: &ComplexityOptions<'_>,
    policy: &ComplexityPolicy<'_>,
) -> Vec<Diagnostic> {
    let rel = normalize_rel_path(opts.base_dir, path);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let limits = resolve_limits(&rel, opts.max_complexity, policy);
    let mut diagnostics = Vec::new();

    for report in reports {
        if report.score > limits.max_complexity {
            let suppressed = suppressions.is_suppressed("complexity", Some(report.line));
            diagnostics.push(Diagnostic {
                rule_id: "warden/complexity".to_string(),
                check: "complexity".to_string(),
                severity: Severity::Error,
                message: format!(
                    "{}:{} - `{}()` score: {} (> {})",
                    rel.display(),
                    report.line,
                    report.function_name,
                    report.score,
                    limits.max_complexity
                ),
                file: Some(rel.clone()),
                line: Some(report.line),
                column: None,
                fingerprint: format!("complexity:{}:{}", rel_str, report.function_name),
                metric_value: Some(report.score),
                suppressed,
                suppression_reason: suppressed.then(|| "inline".to_string()),
            });
        }

        if let Some(max_fn_lines) = limits.max_function_lines
            && report.line_count > max_fn_lines
        {
            let suppressed = suppressions.is_suppressed("function-length", Some(report.line))
                || suppressions.is_suppressed("complexity", Some(report.line));
            diagnostics.push(Diagnostic {
                rule_id: "warden/function-length".to_string(),
                check: "complexity".to_string(),
                severity: Severity::Error,
                message: format!(
                    "{}:{} - `{}()` has {} lines (> {})",
                    rel.display(),
                    report.line,
                    report.function_name,
                    report.line_count,
                    max_fn_lines
                ),
                file: Some(rel.clone()),
                line: Some(report.line),
                column: None,
                fingerprint: format!("function-length:{}:{}", rel_str, report.function_name),
                metric_value: Some(report.line_count),
                suppressed,
                suppression_reason: suppressed.then(|| "inline".to_string()),
            });
        }

        if let Some(max_nest) = limits.max_nesting
            && report.max_nesting > max_nest
        {
            let suppressed = suppressions.is_suppressed("nesting-depth", Some(report.line))
                || suppressions.is_suppressed("complexity", Some(report.line));
            diagnostics.push(Diagnostic {
                rule_id: "warden/nesting-depth".to_string(),
                check: "complexity".to_string(),
                severity: Severity::Error,
                message: format!(
                    "{}:{} - `{}()` nesting depth: {} (> {})",
                    rel.display(),
                    report.line,
                    report.function_name,
                    report.max_nesting,
                    max_nest
                ),
                file: Some(rel.clone()),
                line: Some(report.line),
                column: None,
                fingerprint: format!("nesting-depth:{}:{}", rel_str, report.function_name),
                metric_value: Some(report.max_nesting),
                suppressed,
                suppression_reason: suppressed.then(|| "inline".to_string()),
            });
        }
    }

    let unused = collect_unused_suppression_diagnostics(
        &rel,
        suppressions,
        &["complexity", "function-length", "nesting-depth"],
        &diagnostics,
    );
    diagnostics.extend(unused);
    diagnostics
}

pub(crate) fn discover_complexity_files(
    paths: &[PathBuf],
    opts: &ComplexityOptions<'_>,
) -> Vec<PathBuf> {
    let rust_exts = vec!["rs".to_string()];
    let files = collect_files(
        paths,
        &WalkOptions {
            extensions: &rust_exts,
            ignore_patterns: opts.ignore_patterns,
            respect_gitignore: opts.respect_gitignore,
            base_dir: Some(opts.base_dir),
            changeset: opts.changeset,
        },
    );

    if !opts.ignore_tests {
        return files;
    }

    files
        .into_iter()
        .filter(|path| {
            let path_str = path.to_string_lossy().replace('\\', "/");
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let is_test = path_str.contains("/tests/")
                || file_name.ends_with("_test.rs")
                || file_name.ends_with("_tests.rs")
                || file_name == "tests.rs";
            if is_test {
                tracing::trace!(file = %path.display(), "skipping test file");
            }
            !is_test
        })
        .collect()
}

pub fn analyze_file(path: &Path) -> Result<Vec<ComplexityReport>> {
    Ok(analyze_file_with_content(path)?.0)
}

/// Collect complexity, function-length, and nesting-depth reports and structured diagnostics.
pub fn collect_complexity(
    paths: &[PathBuf],
    opts: &ComplexityOptions<'_>,
) -> Result<(Vec<ComplexityReport>, usize, Vec<Diagnostic>)> {
    collect_complexity_with_policy(paths, opts, &ComplexityPolicy::default())
}

/// Collect complexity diagnostics honoring function length, nesting depth, and path overrides.
pub fn collect_complexity_with_policy(
    paths: &[PathBuf],
    opts: &ComplexityOptions<'_>,
    policy: &ComplexityPolicy<'_>,
) -> Result<(Vec<ComplexityReport>, usize, Vec<Diagnostic>)> {
    if opts.emit_text {
        println!(
            "[complexity] analyzing cognitive complexity (max allowed: {})...",
            opts.max_complexity
        );
    }
    tracing::debug!(
        roots = ?paths,
        max_complexity = opts.max_complexity,
        max_function_lines = ?policy.max_function_lines,
        max_nesting = ?policy.max_nesting,
        overrides = policy.overrides.len(),
        ignore_tests = opts.ignore_tests,
        respect_gitignore = opts.respect_gitignore,
        ignore_patterns = ?opts.ignore_patterns,
        "starting cognitive complexity check"
    );

    let files = discover_complexity_files(paths, opts);
    let per_file = parallel::map_items(&files, true, |path| {
        match analyze_file_with_content(path) {
            Ok((file_reports, suppressions)) => {
                tracing::trace!(
                    file = %path.display(),
                    functions = file_reports.len(),
                    "analyzed file"
                );
                let diags = build_complexity_diagnostics_with_policy(
                    path,
                    &file_reports,
                    &suppressions,
                    opts,
                    policy,
                );
                Some((file_reports, diags))
            }
            Err(err) => {
                tracing::warn!(file = %path.display(), error = %err, "failed to parse rust file; skipping");
                None
            }
        }
    });

    let mut all_reports = Vec::new();
    let mut diagnostics = Vec::new();
    let mut files_scanned = 0;
    for item in per_file.into_iter().flatten() {
        files_scanned += 1;
        all_reports.extend(item.0);
        diagnostics.extend(item.1);
    }

    Ok((all_reports, files_scanned, diagnostics))
}

pub fn check_complexity(
    paths: &[PathBuf],
    max_complexity: usize,
    ignore_tests: bool,
    ignore_patterns: &[String],
) -> Result<Vec<ComplexityReport>> {
    check_complexity_with_options(
        paths,
        &ComplexityOptions {
            base_dir: Path::new("."),
            max_complexity,
            ignore_tests,
            ignore_patterns,
            respect_gitignore: true,
            changeset: None,
            emit_text: true,
        },
    )
}

pub fn check_complexity_with_options(
    paths: &[PathBuf],
    opts: &ComplexityOptions<'_>,
) -> Result<Vec<ComplexityReport>> {
    let (all_reports, files_scanned, diagnostics) = collect_complexity(paths, opts)?;
    let active_errors: Vec<&Diagnostic> = diagnostics
        .iter()
        .filter(|d| !d.suppressed && d.severity == Severity::Error)
        .collect();

    if active_errors.is_empty() {
        if opts.emit_text {
            println!(
                "[complexity] OK: scanned {} functions across {} files, all within limit ({})",
                all_reports.len(),
                files_scanned,
                opts.max_complexity
            );
        }
        Ok(all_reports)
    } else {
        if opts.emit_text {
            println!(
                "[complexity] ❌ {} function(s) exceed maximum cognitive complexity (threshold: {}):",
                active_errors.len(),
                opts.max_complexity
            );
            for d in &active_errors {
                println!("  ❌ {}", d.message);
            }
        }
        bail!("Cognitive complexity threshold exceeded");
    }
}

//! Cognitive Complexity checker for Rust functions and methods.
//!
//! Follows G. Ann Campbell's Cognitive Complexity specification (SonarSource):
//! - Increments for breaks in linear control flow (`if`, `match`, `for`, `while`, `loop`).
//! - Nesting increments when control structures or closures are nested.
//! - Boolean operator transitions (e.g. sequence of `&&` then `||`).
//! - Labeled break/continue statements.

use crate::report::{Diagnostic, Severity, normalize_rel_path};
use crate::suppression::FileSuppressions;
use crate::walker::{WalkOptions, collect_files};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{
    BinOp, Expr, ExprBinary, ExprBreak, ExprClosure, ExprContinue, ExprForLoop, ExprIf, ExprLoop,
    ExprMatch, ExprWhile, ImplItemFn, ItemFn, Pat,
};

#[derive(Debug, Clone)]
pub struct ComplexityReport {
    pub file: PathBuf,
    pub function_name: String,
    pub line: usize,
    pub score: usize,
}

#[derive(Default)]
struct ComplexityCalculator {
    score: usize,
    nesting: usize,
    is_else_if: bool,
    last_bool_op: Option<BoolOpKind>,
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

        self.nesting += 1;
        self.visit_block(&node.then_branch);
        self.nesting -= 1;

        if let Some((_, else_expr)) = &node.else_branch {
            match else_expr.as_ref() {
                Expr::If(child_if) => {
                    self.is_else_if = true;
                    self.visit_expr_if(child_if);
                }
                _ => {
                    self.score += 1;
                    self.nesting += 1;
                    self.visit_expr(else_expr);
                    self.nesting -= 1;
                }
            }
        }
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        self.score += 1 + self.nesting;
        self.visit_expr(&node.expr);

        self.nesting += 1;
        for arm in &node.arms {
            if let Pat::Guard(guard_pat) = &arm.pat {
                self.score += 1;
                self.visit_expr(&guard_pat.guard);
            }
            self.visit_expr(&arm.body);
        }
        self.nesting -= 1;
    }

    fn visit_expr_for_loop(&mut self, node: &'ast ExprForLoop) {
        self.score += 1 + self.nesting;
        self.nesting += 1;
        visit::visit_expr_for_loop(self, node);
        self.nesting -= 1;
    }

    fn visit_expr_while(&mut self, node: &'ast ExprWhile) {
        self.score += 1 + self.nesting;
        self.nesting += 1;
        visit::visit_expr_while(self, node);
        self.nesting -= 1;
    }

    fn visit_expr_loop(&mut self, node: &'ast ExprLoop) {
        self.score += 1 + self.nesting;
        self.nesting += 1;
        visit::visit_expr_loop(self, node);
        self.nesting -= 1;
    }

    fn visit_expr_closure(&mut self, node: &'ast ExprClosure) {
        self.nesting += 1;
        visit::visit_expr_closure(self, node);
        self.nesting -= 1;
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

impl<'ast> Visit<'ast> for FileAstScanner {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let mut calc = ComplexityCalculator::default();
        calc.visit_block(&node.block);

        let name = node.sig.ident.to_string();
        let line = node.sig.ident.span().start().line;
        tracing::trace!(function = %name, line, score = calc.score, "cognitive complexity calculated");

        self.reports.push(ComplexityReport {
            file: self.file.clone(),
            function_name: name,
            line,
            score: calc.score,
        });
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        let mut calc = ComplexityCalculator::default();
        calc.visit_block(&node.block);

        let name = node.sig.ident.to_string();
        let line = node.sig.ident.span().start().line;
        tracing::trace!(function = %name, line, score = calc.score, "cognitive complexity calculated");

        self.reports.push(ComplexityReport {
            file: self.file.clone(),
            function_name: name,
            line,
            score: calc.score,
        });
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

fn analyze_file_with_content(path: &Path) -> Result<(Vec<ComplexityReport>, FileSuppressions)> {
    tracing::debug!(path = %path.display(), "analyzing rust file for cognitive complexity");
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    let syntax: syn::File = syn::parse_file(&content)
        .with_context(|| format!("Failed to parse Rust syntax in {}", path.display()))?;

    let mut scanner = FileAstScanner {
        file: path.to_path_buf(),
        reports: Vec::new(),
    };
    scanner.visit_file(&syntax);

    let suppressions = FileSuppressions::parse(&content);
    Ok((scanner.reports, suppressions))
}

pub fn analyze_file(path: &Path) -> Result<Vec<ComplexityReport>> {
    Ok(analyze_file_with_content(path)?.0)
}

/// Collect complexity reports and structured diagnostics.
pub fn collect_complexity(
    paths: &[PathBuf],
    opts: &ComplexityOptions<'_>,
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
        ignore_tests = opts.ignore_tests,
        respect_gitignore = opts.respect_gitignore,
        ignore_patterns = ?opts.ignore_patterns,
        "starting cognitive complexity check"
    );

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

    let mut all_reports = Vec::new();
    let mut diagnostics = Vec::new();
    let mut files_scanned = 0;

    for path in files {
        let path_str = path.to_string_lossy().replace('\\', "/");
        if opts.ignore_tests {
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path_str.contains("/tests/")
                || file_name.ends_with("_test.rs")
                || file_name.ends_with("_tests.rs")
                || file_name == "tests.rs"
            {
                tracing::trace!(file = %path.display(), "skipping test file");
                continue;
            }
        }

        match analyze_file_with_content(&path) {
            Ok((file_reports, suppressions)) => {
                tracing::trace!(
                    file = %path.display(),
                    functions = file_reports.len(),
                    "analyzed file"
                );
                files_scanned += 1;
                let rel = normalize_rel_path(opts.base_dir, &path);
                let rel_str = rel.to_string_lossy().replace('\\', "/");

                for report in &file_reports {
                    if report.score > opts.max_complexity {
                        let suppressed =
                            suppressions.is_suppressed("complexity", Some(report.line));
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
                                opts.max_complexity
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
                }
                all_reports.extend(file_reports);
            }
            Err(err) => {
                tracing::warn!(file = %path.display(), error = %err, "failed to parse rust file; skipping");
            }
        }
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
    let active: Vec<&Diagnostic> = diagnostics.iter().filter(|d| !d.suppressed).collect();

    if active.is_empty() {
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
                active.len(),
                opts.max_complexity
            );
            for d in &active {
                println!("  ❌ {}", d.message);
            }
        }
        bail!("Cognitive complexity threshold exceeded");
    }
}

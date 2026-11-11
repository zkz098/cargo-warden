//! Cognitive Complexity checker for Rust functions and methods.
//!
//! Follows G. Ann Campbell's Cognitive Complexity specification (SonarSource):
//! - Increments for breaks in linear control flow (`if`, `match`, `for`, `while`, `loop`).
//! - Nesting increments when control structures or closures are nested.
//! - Boolean operator transitions (e.g. sequence of `&&` then `||`).
//! - Labeled break/continue statements.

use anyhow::{Context, Result, bail};
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

pub fn analyze_file(path: &Path) -> Result<Vec<ComplexityReport>> {
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

    Ok(scanner.reports)
}

pub fn check_complexity(
    paths: &[PathBuf],
    max_complexity: usize,
    ignore_tests: bool,
    ignore_patterns: &[String],
) -> Result<Vec<ComplexityReport>> {
    println!("[complexity] analyzing cognitive complexity (max allowed: {max_complexity})...");
    tracing::debug!(
        roots = ?paths,
        max_complexity,
        ignore_tests,
        ignore_patterns = ?ignore_patterns,
        "starting cognitive complexity check"
    );
    let mut all_reports = Vec::new();
    let mut files_scanned = 0;

    for root in paths {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }

            let path_str = path.to_string_lossy().replace('\\', "/");

            if ignore_patterns.iter().any(|pat| path_str.contains(pat)) {
                tracing::trace!(file = %path.display(), "skipping ignored path");
                continue;
            }

            if ignore_tests {
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

            match analyze_file(path) {
                Ok(file_reports) => {
                    tracing::trace!(
                        file = %path.display(),
                        functions = file_reports.len(),
                        "analyzed file"
                    );
                    files_scanned += 1;
                    all_reports.extend(file_reports);
                }
                Err(err) => {
                    tracing::warn!(file = %path.display(), error = %err, "failed to parse rust file; skipping");
                }
            }
        }
    }

    let violations: Vec<&ComplexityReport> = all_reports
        .iter()
        .filter(|r| r.score > max_complexity)
        .collect();

    if violations.is_empty() {
        println!(
            "[complexity] OK: scanned {} functions across {} files, all within limit ({})",
            all_reports.len(),
            files_scanned,
            max_complexity
        );
        Ok(all_reports)
    } else {
        println!(
            "[complexity] ❌ {} function(s) exceed maximum cognitive complexity (threshold: {}):",
            violations.len(),
            max_complexity
        );
        for v in &violations {
            println!(
                "  ❌ {}:{} - `{}()` score: {} (> {})",
                v.file.display(),
                v.line,
                v.function_name,
                v.score,
                max_complexity
            );
        }
        bail!("Cognitive complexity threshold exceeded");
    }
}

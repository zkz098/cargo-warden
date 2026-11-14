//! Inline source-comment suppression parser (`// warden:allow(...)` / `# warden:allow(...)`).

use crate::report::{Diagnostic, Severity};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A single parsed inline `warden:allow` or `warden:allow-file` directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressionDirective {
    /// 1-based line number where the directive comment appears.
    pub line: usize,
    /// 1-based line number of the target item (`None` for file-level directives).
    pub target_line: Option<usize>,
    /// Whether this directive applies to the entire file.
    pub is_file_level: bool,
    /// Normalized rule names listed in the directive.
    pub rules: Vec<String>,
}

/// Parsed inline suppressions for a single source file.
#[derive(Debug, Default, Clone)]
pub struct FileSuppressions {
    /// Rules suppressed for the entire file (`// warden:allow-file(...)` or `//! warden:allow(...)`).
    pub file_rules: BTreeSet<String>,
    /// Rules suppressed on a specific 1-based line number (`// warden:allow(...)`).
    pub line_rules: BTreeMap<usize, BTreeSet<String>>,
    /// Ordered list of parsed directives for unused-suppression auditing.
    pub directives: Vec<SuppressionDirective>,
}

impl FileSuppressions {
    /// Parse inline `warden:allow` directives from source text.
    pub fn parse(content: &str) -> Self {
        let lines: Vec<&str> = content.lines().collect();
        let mut out = Self::default();

        for (idx, line) in lines.iter().enumerate() {
            out.parse_line(&lines, idx, line);
        }

        out
    }

    fn parse_line(&mut self, lines: &[&str], idx: usize, line: &str) {
        let line_no = idx + 1;
        let trimmed = line.trim();
        let Some(comment) = extract_comment_slice(trimmed) else {
            return;
        };
        let comment_body = strip_comment_prefix(comment);

        if let Some(rules) = parse_leading_directive(comment_body, "warden:allow-file(") {
            self.record_file_directive(line_no, rules);
            return;
        }

        if trimmed.starts_with("//!")
            && let Some(rules) = parse_leading_directive(comment_body, "warden:allow(")
        {
            self.record_file_directive(line_no, rules);
            return;
        }

        if let Some(rules) = parse_leading_directive(comment_body, "warden:allow(") {
            let is_comment_only = trimmed.starts_with("//") || is_hash_comment(trimmed);
            self.record_line_directive(lines, idx, line_no, is_comment_only, rules);
        }
    }

    fn record_file_directive(&mut self, line_no: usize, rules: Vec<String>) {
        for rule in &rules {
            self.file_rules.insert(rule.clone());
        }
        self.directives.push(SuppressionDirective {
            line: line_no,
            target_line: None,
            is_file_level: true,
            rules,
        });
    }

    fn record_line_directive(
        &mut self,
        lines: &[&str],
        idx: usize,
        line_no: usize,
        is_comment_only: bool,
        rules: Vec<String>,
    ) {
        let entry = self.line_rules.entry(line_no).or_default();
        for rule in &rules {
            entry.insert(rule.clone());
        }

        let target_line = is_comment_only
            .then(|| find_next_item_line(lines, idx + 1))
            .flatten();
        if let Some(tl) = target_line {
            let target_entry = self.line_rules.entry(tl).or_default();
            for rule in &rules {
                target_entry.insert(rule.clone());
            }
        }
        self.directives.push(SuppressionDirective {
            line: line_no,
            target_line: target_line.or(Some(line_no)),
            is_file_level: false,
            rules,
        });
    }

    /// Returns `true` if `rule` is suppressed at the file level or at `line` (1-based).
    pub fn is_suppressed(&self, rule: &str, line: Option<usize>) -> bool {
        let rule = normalize_rule(rule);
        if self.file_rules.contains(&rule)
            || self.file_rules.contains("all")
            || self.file_rules.contains("*")
        {
            return true;
        }
        if let Some(l) = line
            && let Some(rules) = self.line_rules.get(&l)
        {
            return rules.contains(&rule) || rules.contains("all") || rules.contains("*");
        }
        false
    }
}

/// Emit `Severity::Warning` diagnostics for inline `warden:allow` directives
/// belonging to `owned_rules` that did not suppress any diagnostic in `file_diagnostics`.
pub fn collect_unused_suppression_diagnostics(
    rel_path: &Path,
    suppressions: &FileSuppressions,
    owned_rules: &[&str],
    file_diagnostics: &[Diagnostic],
) -> Vec<Diagnostic> {
    if suppressions.directives.is_empty() || owned_rules.is_empty() {
        return Vec::new();
    }

    let rel_str = rel_path.to_string_lossy().replace('\\', "/");
    let mut warnings = Vec::new();

    for directive in &suppressions.directives {
        for rule in &directive.rules {
            if !owned_rules.contains(&rule.as_str()) {
                continue;
            }
            if directive_was_used(directive, rule, rel_path, file_diagnostics) {
                continue;
            }
            let kind = if directive.is_file_level {
                "warden:allow-file"
            } else {
                "warden:allow"
            };
            warnings.push(Diagnostic {
                rule_id: "warden/unused-suppression".to_string(),
                check: "suppression".to_string(),
                severity: Severity::Warning,
                message: format!(
                    "{}:{} - unused `{kind}({rule})` directive",
                    rel_path.display(),
                    directive.line
                ),
                file: Some(rel_path.to_path_buf()),
                line: Some(directive.line),
                column: None,
                fingerprint: format!("unused-suppression:{rel_str}:{}:{rule}", directive.line),
                metric_value: None,
                suppressed: false,
                suppression_reason: None,
            });
        }
    }

    warnings
}

fn directive_was_used(
    directive: &SuppressionDirective,
    rule: &str,
    rel_path: &Path,
    diagnostics: &[Diagnostic],
) -> bool {
    diagnostics.iter().any(|d| {
        if !d.suppressed || d.suppression_reason.as_deref() != Some("inline") {
            return false;
        }
        if d.file.as_deref() != Some(rel_path) {
            return false;
        }
        if !rule_matches_diagnostic(rule, &d.rule_id) {
            return false;
        }
        if directive.is_file_level {
            return true;
        }
        match d.line {
            Some(diag_line) => {
                diag_line == directive.line || directive.target_line == Some(diag_line)
            }
            None => false,
        }
    })
}

fn rule_matches_diagnostic(rule: &str, diag_rule_id: &str) -> bool {
    match rule {
        "all" | "*" => true,
        "complexity" => matches!(
            diag_rule_id,
            "warden/complexity"
                | "warden/function-length"
                | "warden/nesting-depth"
                | "warden/function-params"
        ),
        "function-length" => diag_rule_id == "warden/function-length",
        "nesting-depth" => diag_rule_id == "warden/nesting-depth",
        "function-params" => diag_rule_id == "warden/function-params",
        "file-length" => diag_rule_id == "warden/file-length",
        "slint-theme" => diag_rule_id == "warden/slint-theme",
        "dag" => diag_rule_id.starts_with("warden/dag/"),
        "workspace-dag" => diag_rule_id.starts_with("warden/dag/workspace-"),
        "hygiene" => diag_rule_id.starts_with("warden/hygiene/"),
        "unsafe" => matches!(
            diag_rule_id,
            "warden/hygiene/forbid-unsafe" | "warden/hygiene/unsafe-without-safety"
        ),
        "safety" => diag_rule_id == "warden/hygiene/unsafe-without-safety",
        "unwrap" | "expect" | "dbg" | "todo" | "println" => {
            diag_rule_id == format!("warden/hygiene/{rule}")
        }
        "manifest" => diag_rule_id.starts_with("warden/manifest/"),
        other => diag_rule_id == format!("warden/{other}") || diag_rule_id == other,
    }
}

fn is_hash_comment(trimmed: &str) -> bool {
    trimmed.starts_with('#') && !trimmed.starts_with("#[") && !trimmed.starts_with("#!")
}

fn extract_comment_slice(trimmed: &str) -> Option<&str> {
    if trimmed.starts_with("//") || is_hash_comment(trimmed) {
        return Some(trimmed);
    }

    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        if in_string {
            if escaped {
                escaped = false;
            } else if bytes[i] == b'\\' {
                escaped = true;
            } else if bytes[i] == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            return Some(&trimmed[i..]);
        }
        if bytes[i] == b'#' && bytes.get(i + 1) != Some(&b'[') && bytes.get(i + 1) != Some(&b'!') {
            return Some(&trimmed[i..]);
        }
        i += 1;
    }
    None
}

fn find_next_item_line(lines: &[&str], start_idx: usize) -> Option<usize> {
    let mut i = start_idx;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || is_hash_comment(trimmed)
            || trimmed.starts_with("#[")
        {
            i += 1;
            continue;
        }
        return Some(i + 1);
    }
    None
}

fn strip_comment_prefix(comment: &str) -> &str {
    if let Some(rest) = comment.strip_prefix("//!") {
        rest.trim()
    } else if let Some(rest) = comment.strip_prefix("///") {
        rest.trim()
    } else if let Some(rest) = comment.strip_prefix("//") {
        rest.trim()
    } else if let Some(rest) = comment.strip_prefix('#') {
        rest.trim()
    } else {
        comment.trim()
    }
}

fn parse_leading_directive(comment_body: &str, marker: &str) -> Option<Vec<String>> {
    let rest = comment_body.strip_prefix(marker)?;
    let end = rest.find(')')?;
    let inside = &rest[..end];

    // Support optional reason after `:` or `, reason = ...`
    let rules_part = inside.split(':').next().unwrap_or(inside);
    let mut rules = Vec::new();
    for raw in rules_part.split(',') {
        let token = raw.trim();
        if token.is_empty() || token.starts_with("reason") {
            continue;
        }
        rules.push(normalize_rule(token));
    }
    if rules.is_empty() { None } else { Some(rules) }
}

fn normalize_rule(rule: &str) -> String {
    rule.trim()
        .to_ascii_lowercase()
        .replace('_', "-")
        .strip_prefix("warden/")
        .unwrap_or(&rule.trim().to_ascii_lowercase().replace('_', "-"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_string_literals_and_supports_hash_comments() {
        let src = r#"
let msg = "// warden:allow(complexity)";
dep = "1.0" # warden:allow(workspace-dag)
"#;
        let parsed = FileSuppressions::parse(src);
        assert!(parsed.directives.is_empty() || parsed.directives.len() == 1);
        assert!(!parsed.is_suppressed("complexity", Some(2)));
        assert!(parsed.is_suppressed("workspace-dag", Some(3)));
    }

    #[test]
    fn reports_unused_suppression_only_for_owned_rules() {
        let src = "// warden:allow(complexity, dag)\npub fn ok() {}\n";
        let parsed = FileSuppressions::parse(src);
        let unused = collect_unused_suppression_diagnostics(
            Path::new("src/lib.rs"),
            &parsed,
            &["complexity"],
            &[],
        );
        assert_eq!(unused.len(), 1);
        assert_eq!(unused[0].rule_id, "warden/unused-suppression");
        assert_eq!(unused[0].severity, Severity::Warning);
        assert!(unused[0].message.contains("complexity"));
    }
}

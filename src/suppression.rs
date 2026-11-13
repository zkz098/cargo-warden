//! Inline source-comment suppression parser (`// warden:allow(...)`).

use std::collections::{BTreeMap, BTreeSet};

/// Parsed inline suppressions for a single source file.
#[derive(Debug, Default, Clone)]
pub struct FileSuppressions {
    /// Rules suppressed for the entire file (`// warden:allow-file(...)` or `//! warden:allow(...)`).
    pub file_rules: BTreeSet<String>,
    /// Rules suppressed on a specific 1-based line number (`// warden:allow(...)`).
    pub line_rules: BTreeMap<usize, BTreeSet<String>>,
}

impl FileSuppressions {
    /// Parse inline `warden:allow` directives from source text.
    pub fn parse(content: &str) -> Self {
        let lines: Vec<&str> = content.lines().collect();
        let mut out = Self::default();

        for (idx, line) in lines.iter().enumerate() {
            let line_no = idx + 1;
            let trimmed = line.trim();

            if let Some(rules) = extract_directive_rules(trimmed, "warden:allow-file(") {
                for rule in rules {
                    out.file_rules.insert(normalize_rule(&rule));
                }
                continue;
            }

            if trimmed.starts_with("//!")
                && let Some(rules) = extract_directive_rules(trimmed, "warden:allow(")
            {
                for rule in rules {
                    out.file_rules.insert(normalize_rule(&rule));
                }
                continue;
            }

            if let Some(rules) = extract_directive_rules(trimmed, "warden:allow(") {
                let normalized: Vec<String> =
                    rules.into_iter().map(|r| normalize_rule(&r)).collect();
                // Allow inline trailing comments on the target line itself.
                let entry = out.line_rules.entry(line_no).or_default();
                for rule in &normalized {
                    entry.insert(rule.clone());
                }

                // If comment-only line, also attach to the next item line (skipping blank/attribute/comment lines).
                if trimmed.starts_with("//")
                    && let Some(target_line) = find_next_item_line(&lines, idx + 1)
                {
                    let target_entry = out.line_rules.entry(target_line).or_default();
                    for rule in normalized {
                        target_entry.insert(rule);
                    }
                }
            }
        }

        out
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

fn find_next_item_line(lines: &[&str], start_idx: usize) -> Option<usize> {
    let mut i = start_idx;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("#[") {
            i += 1;
            continue;
        }
        return Some(i + 1);
    }
    None
}

fn extract_directive_rules(line: &str, marker: &str) -> Option<Vec<String>> {
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
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
        rules.push(token.to_string());
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

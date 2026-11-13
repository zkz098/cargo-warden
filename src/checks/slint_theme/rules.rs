//! UI specification rule checkers (spacing, radius, typography, duration, unknown `Theme.<token>` refs).

use super::SlintThemePolicy;
use super::parser::{ParsedSlintFile, PropertyAssignment, TokenKind};
use crate::config::SlintRulesConfig;
use crate::report::{Diagnostic, Severity};
use crate::suppression::FileSuppressions;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

pub(crate) struct NormalizedAllowLists {
    pub unchanged_colors: HashSet<String>,
    pub lengths: HashSet<String>,
    pub durations: HashSet<String>,
    pub font_families: HashSet<String>,
    pub font_weights: HashSet<String>,
}

impl NormalizedAllowLists {
    pub fn from_policy(policy: &SlintThemePolicy<'_>) -> Self {
        let lower_set = |items: &[String]| -> HashSet<String> {
            items
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .collect()
        };
        Self {
            unchanged_colors: lower_set(policy.unchanged_colors),
            lengths: lower_set(&policy.rules.allowed_lengths),
            durations: lower_set(&policy.rules.allowed_durations),
            font_families: policy.rules.allowed_font_families.iter().cloned().collect(),
            font_weights: lower_set(&policy.rules.allowed_font_weights),
        }
    }
}

pub(crate) fn collect_unknown_token_diags(
    parsed: &ParsedSlintFile,
    known: &BTreeSet<String>,
    rel: &Path,
    rel_str: &str,
    suppressions: &FileSuppressions,
    out: &mut Vec<Diagnostic>,
) {
    for tr in &parsed.theme_refs {
        if known.contains(&tr.token_name) {
            continue;
        }
        let suppressed = suppressions.is_suppressed("slint-unknown-token", Some(tr.line));
        out.push(Diagnostic {
            rule_id: "warden/slint/unknown-token".to_string(),
            check: "slint-theme".to_string(),
            severity: Severity::Error,
            message: format!(
                "Unknown design token `Theme.{}` at {}:{}",
                tr.token_name,
                rel.display(),
                tr.line
            ),
            file: Some(rel.to_path_buf()),
            line: Some(tr.line),
            column: None,
            fingerprint: format!(
                "slint-unknown-token:{rel_str}:{}:{}",
                tr.line, tr.token_name
            ),
            metric_value: None,
            suppressed,
            suppression_reason: suppressed.then(|| "inline".to_string()),
        });
    }
}

pub(crate) fn collect_ui_spec_diags(
    parsed: &ParsedSlintFile,
    rules: &SlintRulesConfig,
    allow: &NormalizedAllowLists,
    rel: &Path,
    rel_str: &str,
    suppressions: &FileSuppressions,
    out: &mut Vec<Diagnostic>,
) {
    for assign in &parsed.assignments {
        if rules.spacing && is_spacing_prop(&assign.prop_name) {
            check_length_assignment(
                assign,
                rules.spacing_grid,
                allow,
                true,
                rel,
                rel_str,
                suppressions,
                out,
            );
        } else if rules.radius && is_radius_prop(&assign.prop_name) {
            check_length_assignment(assign, None, allow, false, rel, rel_str, suppressions, out);
        } else if rules.typography {
            check_typography_assignment(assign, allow, rel, rel_str, suppressions, out);
        }
        if rules.duration && assign.prop_name == "duration" {
            check_duration_assignment(assign, allow, rel, rel_str, suppressions, out);
        }
    }
}

fn is_spacing_prop(prop: &str) -> bool {
    matches!(
        prop,
        "spacing"
            | "padding"
            | "padding-left"
            | "padding-right"
            | "padding-top"
            | "padding-bottom"
            | "margin"
            | "margin-left"
            | "margin-right"
            | "margin-top"
            | "margin-bottom"
            | "gap"
    )
}

fn is_radius_prop(prop: &str) -> bool {
    matches!(prop, "border-radius" | "border-width")
}

fn is_length_unit(unit: &str) -> bool {
    matches!(
        unit,
        "px" | "pt" | "rem" | "em" | "phx" | "in" | "mm" | "cm"
    )
}

#[allow(clippy::too_many_arguments)]
fn check_length_assignment(
    assign: &PropertyAssignment,
    grid: Option<u32>,
    allow: &NormalizedAllowLists,
    is_spacing: bool,
    rel: &Path,
    rel_str: &str,
    suppressions: &FileSuppressions,
    out: &mut Vec<Diagnostic>,
) {
    let (rule_id, suppress_key, kind) = if is_spacing {
        ("warden/slint/spacing", "slint-spacing", "spacing")
    } else {
        ("warden/slint/radius", "slint-radius", "radius/border")
    };
    for tok in &assign.rhs {
        let TokenKind::Dimension { raw, value, unit } = &tok.kind else {
            continue;
        };
        if !is_length_unit(unit) || allow.lengths.contains(raw) {
            continue;
        }
        if let Some(step) = grid
            && step > 0
            && unit == "px"
            && (value - value.round()).abs() < 1e-6
            && (value.round() as i64) % (step as i64) == 0
        {
            continue;
        }
        let suppressed = suppressions.is_suppressed(suppress_key, Some(tok.line));
        let grid_hint = grid
            .map(|g| format!(" (expected {g}px grid or Theme token)"))
            .unwrap_or_default();
        out.push(Diagnostic {
            rule_id: rule_id.to_string(),
            check: "slint-theme".to_string(),
            severity: Severity::Error,
            message: format!(
                "Hardcoded {kind} literal `{raw}` on `{}` at {}:{}{grid_hint}",
                assign.prop_name,
                rel.display(),
                tok.line
            ),
            file: Some(rel.to_path_buf()),
            line: Some(tok.line),
            column: None,
            fingerprint: format!(
                "{suppress_key}:{rel_str}:{}:{}:{raw}",
                tok.line, assign.prop_name
            ),
            metric_value: None,
            suppressed,
            suppression_reason: suppressed.then(|| "inline".to_string()),
        });
    }
}

fn check_typography_assignment(
    assign: &PropertyAssignment,
    allow: &NormalizedAllowLists,
    rel: &Path,
    rel_str: &str,
    suppressions: &FileSuppressions,
    out: &mut Vec<Diagnostic>,
) {
    match assign.prop_name.as_str() {
        "font-size" => {
            for tok in &assign.rhs {
                if let TokenKind::Dimension { raw, unit, .. } = &tok.kind
                    && is_length_unit(unit)
                    && !allow.lengths.contains(raw)
                {
                    push_typo_diag(assign, raw, tok.line, rel, rel_str, suppressions, out);
                }
            }
        }
        "font-family" => {
            for tok in &assign.rhs {
                if let TokenKind::StringLit(fam) = &tok.kind
                    && !allow.font_families.contains(fam)
                {
                    push_typo_diag(
                        assign,
                        &format!("{fam:?}"),
                        tok.line,
                        rel,
                        rel_str,
                        suppressions,
                        out,
                    );
                }
            }
        }
        "font-weight" => {
            for tok in &assign.rhs {
                if let TokenKind::Number(num) = &tok.kind
                    && !allow.font_weights.contains(&num.to_ascii_lowercase())
                {
                    push_typo_diag(assign, num, tok.line, rel, rel_str, suppressions, out);
                }
            }
        }
        _ => {}
    }
}

fn push_typo_diag(
    assign: &PropertyAssignment,
    literal: &str,
    line: usize,
    rel: &Path,
    rel_str: &str,
    suppressions: &FileSuppressions,
    out: &mut Vec<Diagnostic>,
) {
    let suppressed = suppressions.is_suppressed("slint-typography", Some(line));
    out.push(Diagnostic {
        rule_id: "warden/slint/typography".to_string(),
        check: "slint-theme".to_string(),
        severity: Severity::Error,
        message: format!(
            "Hardcoded typography `{literal}` on `{}` at {}:{}",
            assign.prop_name,
            rel.display(),
            line
        ),
        file: Some(rel.to_path_buf()),
        line: Some(line),
        column: None,
        fingerprint: format!(
            "slint-typography:{rel_str}:{line}:{}:{literal}",
            assign.prop_name
        ),
        metric_value: None,
        suppressed,
        suppression_reason: suppressed.then(|| "inline".to_string()),
    });
}

fn check_duration_assignment(
    assign: &PropertyAssignment,
    allow: &NormalizedAllowLists,
    rel: &Path,
    rel_str: &str,
    suppressions: &FileSuppressions,
    out: &mut Vec<Diagnostic>,
) {
    for tok in &assign.rhs {
        let TokenKind::Dimension { raw, unit, .. } = &tok.kind else {
            continue;
        };
        if !matches!(unit.as_str(), "ms" | "s") || allow.durations.contains(raw) {
            continue;
        }
        let suppressed = suppressions.is_suppressed("slint-duration", Some(tok.line));
        out.push(Diagnostic {
            rule_id: "warden/slint/duration".to_string(),
            check: "slint-theme".to_string(),
            severity: Severity::Error,
            message: format!(
                "Hardcoded animation duration `{raw}` at {}:{}",
                rel.display(),
                tok.line
            ),
            file: Some(rel.to_path_buf()),
            line: Some(tok.line),
            column: None,
            fingerprint: format!("slint-duration:{rel_str}:{}:{raw}", tok.line),
            metric_value: None,
            suppressed,
            suppression_reason: suppressed.then(|| "inline".to_string()),
        });
    }
}

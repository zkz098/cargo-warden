//! Structured diagnostic reporting in Text, JSON, and SARIF 2.1.0 formats.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// CLI output format for checks and gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Sarif,
}

impl OutputFormat {
    pub fn is_structured(self) -> bool {
        !matches!(self, Self::Text)
    }
}

/// Diagnostic severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

/// A single structured finding emitted by any cargo-warden check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub rule_id: String,
    pub check: String,
    pub severity: Severity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    /// Deterministic key used by baseline matching.
    pub fingerprint: String,
    /// Optional numeric measurement (e.g., complexity score or line count) for regression checks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric_value: Option<usize>,
    #[serde(default)]
    pub suppressed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppression_reason: Option<String>,
}

/// Summary statistics for a check run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReportSummary {
    pub total: usize,
    pub active: usize,
    pub suppressed: usize,
    pub errors: usize,
    pub warnings: usize,
}

/// Top-level JSON report emitted when `--format json` is selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckReport {
    pub version: String,
    pub passed: bool,
    pub summary: ReportSummary,
    pub diagnostics: Vec<Diagnostic>,
}

impl CheckReport {
    pub fn from_diagnostics(diagnostics: Vec<Diagnostic>) -> Self {
        let total = diagnostics.len();
        let suppressed = diagnostics.iter().filter(|d| d.suppressed).count();
        let active = total - suppressed;
        let errors = diagnostics
            .iter()
            .filter(|d| !d.suppressed && d.severity == Severity::Error)
            .count();
        let warnings = diagnostics
            .iter()
            .filter(|d| !d.suppressed && d.severity == Severity::Warning)
            .count();
        let passed = errors == 0;

        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            passed,
            summary: ReportSummary {
                total,
                active,
                suppressed,
                errors,
                warnings,
            },
            diagnostics,
        }
    }

    pub fn to_json_pretty(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn to_sarif_pretty(&self) -> anyhow::Result<String> {
        let mut rules_map: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
        for d in &self.diagnostics {
            rules_map
                .entry(d.rule_id.as_str())
                .or_insert((d.check.as_str(), rule_short_description(&d.rule_id)));
        }

        let rules: Vec<serde_json::Value> = rules_map
            .into_iter()
            .map(|(id, (check, desc))| {
                serde_json::json!({
                    "id": id,
                    "name": check,
                    "shortDescription": { "text": desc },
                })
            })
            .collect();

        let results: Vec<serde_json::Value> = self
            .diagnostics
            .iter()
            .map(|d| {
                let level = match d.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                };
                let mut obj = serde_json::json!({
                    "ruleId": d.rule_id,
                    "level": level,
                    "message": { "text": d.message },
                    "partialFingerprints": {
                        "wardenFingerprint/v1": d.fingerprint,
                    }
                });

                if let Some(file) = &d.file {
                    let uri = file.to_string_lossy().replace('\\', "/");
                    let mut phys = serde_json::json!({
                        "artifactLocation": { "uri": uri }
                    });
                    if let Some(line) = d.line {
                        let mut region = serde_json::json!({ "startLine": line.max(1) });
                        if let Some(col) = d.column {
                            region["startColumn"] = serde_json::json!(col.max(1));
                        }
                        phys["region"] = region;
                    }
                    obj["locations"] = serde_json::json!([{ "physicalLocation": phys }]);
                }

                if d.suppressed {
                    let kind = match d.suppression_reason.as_deref() {
                        Some("inline") => "inSource",
                        _ => "external",
                    };
                    obj["suppressions"] = serde_json::json!([{
                        "kind": kind,
                        "justification": d.suppression_reason.as_deref().unwrap_or("suppressed")
                    }]);
                }

                obj
            })
            .collect();

        let sarif = serde_json::json!({
            "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
            "version": "2.1.0",
            "runs": [{
                "tool": {
                    "driver": {
                        "name": "cargo-warden",
                        "version": env!("CARGO_PKG_VERSION"),
                        "informationUri": "https://github.com/zkz098/cargo-warden",
                        "rules": rules,
                    }
                },
                "results": results,
            }]
        });

        Ok(serde_json::to_string_pretty(&sarif)?)
    }
}

fn rule_short_description(rule_id: &str) -> &'static str {
    match rule_id {
        "warden/dag/cycle" => "Module dependency cycle detected",
        "warden/dag/forbid-import" => "Forbidden module import detected",
        "warden/dag/layer-violation" => "Layered architecture dependency violation",
        "warden/complexity" => "Function cognitive complexity exceeds threshold",
        "warden/file-length" => "File line count exceeds threshold",
        "warden/ci-parity" => "Workspace crate missing from CI test coverage",
        "warden/slint-theme" => "Unmapped hardcoded hex color in Slint UI file",
        "warden/i18n" => "Generated Rust i18n catalog is out of sync with .po files",
        "warden/gate" => "Verification gate step failed",
        _ => "cargo-warden quality check violation",
    }
}

/// Normalize a path relative to `base_dir` using forward slashes for stable cross-platform IDs.
pub fn normalize_rel_path(base_dir: &Path, path: &Path) -> PathBuf {
    let rel = path.strip_prefix(base_dir).unwrap_or(path);
    let cleaned = rel
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    PathBuf::from(cleaned)
}

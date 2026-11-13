//! Baseline snapshot management for gradual adoption and regression prevention.

use crate::report::{Diagnostic, Severity};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Persisted baseline file schema (`warden-baseline.json`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BaselineFile {
    pub version: u32,
    pub entries: BTreeMap<String, BaselineEntry>,
}

/// One baselined violation keyed by its deterministic `fingerprint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineEntry {
    pub rule_id: String,
    pub check: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    pub message: String,
    /// Upper bound recorded when baseline was captured; regressions above this value still fail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_value: Option<usize>,
}

impl BaselineFile {
    pub fn from_diagnostics(diagnostics: &[Diagnostic]) -> Self {
        let mut entries = BTreeMap::new();
        for diag in diagnostics {
            // Do not record diagnostics already suppressed inline or informational warnings.
            if (diag.suppressed && diag.suppression_reason.as_deref() == Some("inline"))
                || diag.severity == Severity::Warning
            {
                continue;
            }
            entries.insert(
                diag.fingerprint.clone(),
                BaselineEntry {
                    rule_id: diag.rule_id.clone(),
                    check: diag.check.clone(),
                    file: diag.file.clone(),
                    message: diag.message.clone(),
                    max_value: diag.metric_value,
                },
            );
        }
        Self {
            version: 1,
            entries,
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read baseline file {}", path.display()))?;
        let parsed: BaselineFile = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse baseline JSON in {}", path.display()))?;
        Ok(parsed)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, format!("{json}\n"))
            .with_context(|| format!("Failed to write baseline file {}", path.display()))?;
        Ok(())
    }

    /// Suppress diagnostics covered by this baseline unless their metric regressed above `max_value`.
    pub fn apply(&self, diagnostics: &mut [Diagnostic]) -> BaselineApplyStats {
        let mut stats = BaselineApplyStats::default();
        for diag in diagnostics.iter_mut() {
            if diag.suppressed {
                continue;
            }
            let Some(entry) = self.entries.get(&diag.fingerprint) else {
                continue;
            };
            if let (Some(current), Some(baseline_max)) = (diag.metric_value, entry.max_value)
                && current > baseline_max
            {
                stats.regressions += 1;
                diag.message =
                    format!("{} (regressed above baseline {baseline_max})", diag.message);
                continue;
            }
            diag.suppressed = true;
            diag.suppression_reason = Some("baseline".to_string());
            stats.suppressed += 1;
        }
        stats
    }

    /// Count baseline entries that are no longer present in a full set of current diagnostics.
    pub fn stale_count(&self, current_diagnostics: &[Diagnostic]) -> usize {
        let active_fingerprints = collect_live_fingerprints(current_diagnostics);
        self.entries
            .keys()
            .filter(|fp| !active_fingerprints.contains(*fp))
            .count()
    }

    /// Remove stale baseline entries that no longer correspond to any current violation.
    pub fn prune(&mut self, current_diagnostics: &[Diagnostic]) -> BaselinePruneStats {
        let total_before = self.entries.len();
        let live_fingerprints = collect_live_fingerprints(current_diagnostics);
        let mut removed_keys = Vec::new();
        self.entries.retain(|fp, _| {
            let keep = live_fingerprints.contains(fp);
            if !keep {
                removed_keys.push(fp.clone());
            }
            keep
        });
        BaselinePruneStats {
            total_before,
            kept: self.entries.len(),
            removed: removed_keys.len(),
            removed_keys,
        }
    }
}

fn collect_live_fingerprints(diagnostics: &[Diagnostic]) -> BTreeSet<String> {
    diagnostics
        .iter()
        .filter(|d| {
            d.severity != Severity::Warning
                && !(d.suppressed && d.suppression_reason.as_deref() == Some("inline"))
        })
        .map(|d| d.fingerprint.clone())
        .collect()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BaselineApplyStats {
    pub suppressed: usize,
    pub regressions: usize,
    pub stale: usize,
}

#[derive(Debug, Clone, Default)]
pub struct BaselinePruneStats {
    pub total_before: usize,
    pub kept: usize,
    pub removed: usize,
    pub removed_keys: Vec<String>,
}

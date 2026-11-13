//! Baseline snapshot management for gradual adoption and regression prevention.

use crate::report::Diagnostic;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
            // Do not record diagnostics already suppressed inline.
            if diag.suppressed && diag.suppression_reason.as_deref() == Some("inline") {
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
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BaselineApplyStats {
    pub suppressed: usize,
    pub regressions: usize,
}

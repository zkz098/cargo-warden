//! Git changeset discovery for `--staged` and `--diff` incremental checks.

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Scope of files to check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ChangesetScope {
    #[default]
    All,
    Staged,
    Diff(String),
}

impl ChangesetScope {
    pub fn is_incremental(&self) -> bool {
        !matches!(self, Self::All)
    }

    pub fn label(&self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::Staged => "staged".to_string(),
            Self::Diff(base) => format!("diff:{base}"),
        }
    }
}

/// Resolve the set of changed relative paths under `base_dir`, or `None` when checking everything.
pub fn resolve_changeset(
    base_dir: &Path,
    scope: &ChangesetScope,
) -> Result<Option<BTreeSet<PathBuf>>> {
    resolve_changeset_filtered(base_dir, scope, false)
}

/// Resolve changed relative paths including deleted files (`D`), for Gate impact analysis.
pub fn resolve_changeset_with_deleted(
    base_dir: &Path,
    scope: &ChangesetScope,
) -> Result<Option<BTreeSet<PathBuf>>> {
    resolve_changeset_filtered(base_dir, scope, true)
}

fn resolve_changeset_filtered(
    base_dir: &Path,
    scope: &ChangesetScope,
    include_deleted: bool,
) -> Result<Option<BTreeSet<PathBuf>>> {
    let filter = if include_deleted { "ACMRD" } else { "ACMR" };
    let mut args = vec![
        "diff",
        "--name-only",
        "--diff-filter",
        filter,
        "--relative",
        "-z",
    ];
    match scope {
        ChangesetScope::All => return Ok(None),
        ChangesetScope::Staged => args.push("--cached"),
        ChangesetScope::Diff(base) => args.push(base.as_str()),
    }

    let output = Command::new("git")
        .args(&args)
        .current_dir(base_dir)
        .output()
        .context("Failed to execute `git diff`")?;

    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut files = BTreeSet::new();
    for raw in output.stdout.split(|&b| b == 0) {
        if raw.is_empty() {
            continue;
        }
        let path_str = String::from_utf8_lossy(raw).replace('\\', "/");
        files.insert(PathBuf::from(path_str));
    }

    tracing::debug!(
        scope = %scope.label(),
        changed_count = files.len(),
        "resolved git changeset"
    );
    Ok(Some(files))
}

//! Workspace and CI parity checker.
//!
//! Enforces that:
//! 1. Every workspace member is tested in CI (e.g. named in `.github/workflows/ci.yml`).
//! 2. No deleted/renamed member lingers in CI test lines.
//! 3. (Optional) Sonar/lcov coverage fan-out includes expected crates.

use crate::config::GateConfig;
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Extract workspace member package names using `cargo metadata`.
pub fn workspace_members(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .context("Failed to run `cargo metadata`")?;

    if !output.status.success() {
        bail!(
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Failed to parse `cargo metadata` JSON")?;

    let mut members: Vec<String> = metadata["packages"]
        .as_array()
        .context("`packages` array missing from cargo metadata")?
        .iter()
        .filter_map(|package| package["name"].as_str().map(str::to_string))
        .collect();

    members.sort();
    Ok(members)
}

fn gate_line_runs_tests(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return false;
    }
    let invokes_gate = trimmed.contains("cargo warden gate")
        || trimmed.contains("cargo-warden gate")
        || (trimmed.contains("cargo run") && trimmed.contains(" gate"));
    if !invokes_gate || trimmed.contains("--skip test") || trimmed.contains("--skip=test") {
        return false;
    }
    if let Some((_, rest)) = trimmed.split_once("--only") {
        let arg = rest
            .trim_start_matches('=')
            .split_whitespace()
            .next()
            .unwrap_or("");
        return arg.split(',').any(|s| s == "test");
    }
    true
}

fn find_missing_members(
    active_members: &[&String],
    ci_content: &str,
    test_patterns: &[String],
    gate_cfg: Option<&GateConfig>,
) -> Vec<String> {
    let patterns: Vec<&str> = if test_patterns.is_empty() {
        vec!["nextest", "llvm-cov"]
    } else {
        test_patterns.iter().map(String::as_str).collect()
    };

    let test_lines: Vec<&str> = ci_content
        .lines()
        .filter(|line| patterns.iter().any(|pat| line.contains(pat)))
        .collect();

    let delegates_to_gate = ci_content.lines().any(gate_line_runs_tests);
    let gate_packages: Option<BTreeSet<&str>> = if delegates_to_gate {
        match gate_cfg {
            Some(cfg) if !cfg.test_groups.is_empty() => Some(
                cfg.test_groups
                    .iter()
                    .flat_map(|g| g.packages.iter().map(String::as_str))
                    .collect(),
            ),
            // Empty test_groups means `cargo warden gate` runs workspace-wide tests.
            _ => return Vec::new(),
        }
    } else {
        None
    };

    active_members
        .iter()
        .filter(|member| {
            let in_ci_lines = test_lines.iter().any(|line| line.contains(member.as_str()));
            let in_gate_groups = gate_packages
                .as_ref()
                .is_some_and(|pkgs| pkgs.contains(member.as_str()));
            !in_ci_lines && !in_gate_groups
        })
        .map(|m| (*m).clone())
        .collect()
}

/// Verify that every workspace member is referenced in CI test lines.
pub fn check_ci_parity(
    root: &Path,
    ci_file: &Path,
    test_patterns: &[String],
    ignore_crates: &[String],
    fanout_expected: &[String],
) -> Result<()> {
    check_ci_parity_with_gate(
        root,
        ci_file,
        test_patterns,
        ignore_crates,
        fanout_expected,
        None,
    )
}

/// Options for running CI parity checks with changeset and structured output support.
#[derive(Debug, Clone)]
pub struct CiParityOptions<'a> {
    pub ci_file: &'a Path,
    pub test_patterns: &'a [String],
    pub ignore_crates: &'a [String],
    pub fanout_expected: &'a [String],
    pub gate_cfg: Option<&'a GateConfig>,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

fn should_run_in_changeset(ci_file: &Path, changeset: &BTreeSet<PathBuf>) -> bool {
    let ci_str = ci_file.to_string_lossy().replace('\\', "/");
    changeset.iter().any(|p| {
        let s = p.to_string_lossy().replace('\\', "/");
        s == ci_str
            || s == "Cargo.toml"
            || s.ends_with("/Cargo.toml")
            || s == "warden.toml"
            || s == ".warden.toml"
    })
}

/// Collect structured CI parity diagnostics.
pub fn collect_ci_parity(root: &Path, opts: &CiParityOptions<'_>) -> Result<Vec<Diagnostic>> {
    if let Some(changed) = opts.changeset
        && !should_run_in_changeset(opts.ci_file, changed)
    {
        tracing::info!(
            "skipping ci-parity check (no Cargo.toml, warden.toml, or workflow changes)"
        );
        return Ok(Vec::new());
    }

    let full_ci_path = if opts.ci_file.is_absolute() {
        opts.ci_file.to_path_buf()
    } else {
        root.join(opts.ci_file)
    };

    tracing::debug!(
        ci_file = %full_ci_path.display(),
        test_patterns = ?opts.test_patterns,
        ignore_crates = ?opts.ignore_crates,
        "checking workspace and CI test parity"
    );

    if !full_ci_path.is_file() {
        bail!("CI workflow file not found at: {}", full_ci_path.display());
    }

    let members = workspace_members(root)?;
    tracing::trace!(members = ?members, "retrieved cargo workspace members");
    let active_members: Vec<&String> = members
        .iter()
        .filter(|m| !opts.ignore_crates.contains(m))
        .collect();

    let ci_content = std::fs::read_to_string(&full_ci_path)
        .with_context(|| format!("Failed to read {}", full_ci_path.display()))?;

    let rel_ci = normalize_rel_path(root, &full_ci_path);
    let mut diagnostics = Vec::new();

    let missing = find_missing_members(
        &active_members,
        &ci_content,
        opts.test_patterns,
        opts.gate_cfg,
    );
    for m in missing {
        diagnostics.push(Diagnostic {
            rule_id: "warden/ci-parity".to_string(),
            check: "ci-parity".to_string(),
            severity: Severity::Error,
            message: format!(
                "Workspace member `{m}` is missing from CI test runs in {}",
                opts.ci_file.display()
            ),
            file: Some(rel_ci.clone()),
            line: None,
            column: None,
            fingerprint: format!("ci-parity:missing:{m}"),
            metric_value: None,
            suppressed: false,
            suppression_reason: None,
        });
    }

    if !opts.fanout_expected.is_empty() {
        let fanout_loop: Vec<&str> = ci_content
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("for crate in ")?;
                Some(rest.trim_end_matches("; do").split_whitespace())
            })
            .flatten()
            .collect();

        for expected in opts.fanout_expected {
            if !fanout_loop.contains(&expected.as_str()) {
                diagnostics.push(Diagnostic {
                    rule_id: "warden/ci-parity".to_string(),
                    check: "ci-parity".to_string(),
                    severity: Severity::Error,
                    message: format!(
                        "Crate `{expected}` is missing from coverage report fan-out loop in {}",
                        opts.ci_file.display()
                    ),
                    file: Some(rel_ci.clone()),
                    line: None,
                    column: None,
                    fingerprint: format!("ci-parity:fanout:{expected}"),
                    metric_value: None,
                    suppressed: false,
                    suppression_reason: None,
                });
            }
        }
    }

    if diagnostics.is_empty() && opts.emit_text {
        println!(
            "[ci-parity] OK: all {} active workspace member(s) verified in {}",
            active_members.len(),
            opts.ci_file.display()
        );
    }

    Ok(diagnostics)
}

/// Verify workspace-to-CI parity, recognizing workflows that delegate testing to `cargo warden gate`.
pub fn check_ci_parity_with_gate(
    root: &Path,
    ci_file: &Path,
    test_patterns: &[String],
    ignore_crates: &[String],
    fanout_expected: &[String],
    gate_cfg: Option<&GateConfig>,
) -> Result<()> {
    let diagnostics = collect_ci_parity(
        root,
        &CiParityOptions {
            ci_file,
            test_patterns,
            ignore_crates,
            fanout_expected,
            gate_cfg,
            changeset: None,
            emit_text: true,
        },
    )?;

    let missing: Vec<String> = diagnostics
        .iter()
        .filter(|d| !d.suppressed && d.fingerprint.starts_with("ci-parity:missing:"))
        .filter_map(|d| {
            d.fingerprint
                .strip_prefix("ci-parity:missing:")
                .map(str::to_string)
        })
        .collect();
    if !missing.is_empty() {
        bail!(
            "Workspace members whose tests CI never runs (in {}):\n  ❌ {}\n\
             Ensure each crate is targeted with `-p <crate>`, `--manifest-path`, or `cargo warden gate`.",
            ci_file.display(),
            missing.join(", ")
        );
    }

    let missing_fanout: Vec<String> = diagnostics
        .iter()
        .filter(|d| !d.suppressed && d.fingerprint.starts_with("ci-parity:fanout:"))
        .filter_map(|d| {
            d.fingerprint
                .strip_prefix("ci-parity:fanout:")
                .map(str::to_string)
        })
        .collect();
    if !missing_fanout.is_empty() {
        bail!(
            "Crates missing from coverage report fan-out loop in {}:\n  ❌ {}",
            ci_file.display(),
            missing_fanout.join(", ")
        );
    }

    Ok(())
}

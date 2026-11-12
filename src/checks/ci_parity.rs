//! Workspace and CI parity checker.
//!
//! Enforces that:
//! 1. Every workspace member is tested in CI (e.g. named in `.github/workflows/ci.yml`).
//! 2. No deleted/renamed member lingers in CI test lines.
//! 3. (Optional) Sonar/lcov coverage fan-out includes expected crates.

use crate::config::GateConfig;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::Path;
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

/// Verify workspace-to-CI parity, recognizing workflows that delegate testing to `cargo warden gate`.
pub fn check_ci_parity_with_gate(
    root: &Path,
    ci_file: &Path,
    test_patterns: &[String],
    ignore_crates: &[String],
    fanout_expected: &[String],
    gate_cfg: Option<&GateConfig>,
) -> Result<()> {
    let full_ci_path = if ci_file.is_absolute() {
        ci_file.to_path_buf()
    } else {
        root.join(ci_file)
    };

    tracing::debug!(
        ci_file = %full_ci_path.display(),
        test_patterns = ?test_patterns,
        ignore_crates = ?ignore_crates,
        "checking workspace and CI test parity"
    );

    if !full_ci_path.is_file() {
        bail!("CI workflow file not found at: {}", full_ci_path.display());
    }

    let members = workspace_members(root)?;
    tracing::trace!(members = ?members, "retrieved cargo workspace members");
    let active_members: Vec<&String> = members
        .iter()
        .filter(|m| !ignore_crates.contains(m))
        .collect();

    let ci_content = std::fs::read_to_string(&full_ci_path)
        .with_context(|| format!("Failed to read {}", full_ci_path.display()))?;

    let missing = find_missing_members(&active_members, &ci_content, test_patterns, gate_cfg);

    if !missing.is_empty() {
        bail!(
            "Workspace members whose tests CI never runs (in {}):\n  ❌ {}\n\
             Ensure each crate is targeted with `-p <crate>`, `--manifest-path`, or `cargo warden gate`.",
            ci_file.display(),
            missing.join(", ")
        );
    }

    // Optional fan-out check for Sonar / lcov
    if !fanout_expected.is_empty() {
        let fanout_loop: Vec<&str> = ci_content
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("for crate in ")?;
                Some(rest.trim_end_matches("; do").split_whitespace())
            })
            .flatten()
            .collect();

        let mut missing_fanout = Vec::new();
        for expected in fanout_expected {
            if !fanout_loop.contains(&expected.as_str()) {
                missing_fanout.push(expected.clone());
            }
        }

        if !missing_fanout.is_empty() {
            bail!(
                "Crates missing from coverage report fan-out loop in {}:\n  ❌ {}",
                ci_file.display(),
                missing_fanout.join(", ")
            );
        }
    }

    println!(
        "[ci-parity] OK: all {} active workspace member(s) verified in {}",
        active_members.len(),
        ci_file.display()
    );
    Ok(())
}

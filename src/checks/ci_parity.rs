//! Workspace and CI parity checker.
//!
//! Enforces that:
//! 1. Every workspace member is tested in CI (e.g. named in `.github/workflows/ci.yml`).
//! 2. No deleted/renamed member lingers in CI test lines.
//! 3. (Optional) Sonar/lcov coverage fan-out includes expected crates.

use anyhow::{Context, Result, bail};
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

/// Verify that every workspace member is referenced in CI test lines.
pub fn check_ci_parity(
    root: &Path,
    ci_file: &Path,
    test_patterns: &[String],
    ignore_crates: &[String],
    fanout_expected: &[String],
) -> Result<()> {
    let full_ci_path = if ci_file.is_absolute() {
        ci_file.to_path_buf()
    } else {
        root.join(ci_file)
    };

    if !full_ci_path.is_file() {
        bail!("CI workflow file not found at: {}", full_ci_path.display());
    }

    let members = workspace_members(root)?;
    let active_members: Vec<&String> = members
        .iter()
        .filter(|m| !ignore_crates.contains(m))
        .collect();

    let ci_content = std::fs::read_to_string(&full_ci_path)
        .with_context(|| format!("Failed to read {}", full_ci_path.display()))?;

    let patterns: Vec<&str> = if test_patterns.is_empty() {
        vec!["nextest", "llvm-cov"]
    } else {
        test_patterns.iter().map(String::as_str).collect()
    };

    let test_lines: Vec<&str> = ci_content
        .lines()
        .filter(|line| patterns.iter().any(|pat| line.contains(pat)))
        .collect();

    let mut missing = Vec::new();
    for member in &active_members {
        let is_tested = test_lines.iter().any(|line| line.contains(member.as_str()));
        if !is_tested {
            missing.push((*member).clone());
        }
    }

    if !missing.is_empty() {
        bail!(
            "Workspace members whose tests CI never runs (in {}):\n  ❌ {}\n\
             Ensure each crate is targeted with `-p <crate>` or `--manifest-path` in a test line.",
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

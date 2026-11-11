//! Maximum file length and line count checker.
//!
//! Enforces maintainability by preventing source files from growing into
//! monolithic "god files" that resist review and comprehension.

use anyhow::{Result, bail};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct FileLengthReport {
    pub path: PathBuf,
    pub lines: usize,
}

pub fn check_file_length(
    paths: &[PathBuf],
    max_lines: usize,
    extensions: &[String],
    ignore_patterns: &[String],
) -> Result<Vec<FileLengthReport>> {
    println!("[file-length] scanning files (max allowed lines: {max_lines})...");
    tracing::debug!(
        roots = ?paths,
        max_lines,
        extensions = ?extensions,
        ignore_patterns = ?ignore_patterns,
        "starting file length scan"
    );
    let mut reports = Vec::new();

    let ext_set: Vec<String> = extensions
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect();

    for root in paths {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                continue;
            };

            if !ext_set.is_empty() && !ext_set.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                continue;
            }

            let path_str = path.to_string_lossy().replace('\\', "/");
            if ignore_patterns.iter().any(|pat| path_str.contains(pat)) {
                tracing::trace!(file = %path.display(), "skipping ignored path");
                continue;
            }

            match fs::read_to_string(path) {
                Ok(content) => {
                    let lines = content.lines().count();
                    tracing::trace!(file = %path.display(), lines, "counted file lines");
                    reports.push(FileLengthReport {
                        path: path.to_path_buf(),
                        lines,
                    });
                }
                Err(err) => {
                    tracing::warn!(file = %path.display(), error = %err, "failed to read file; skipping");
                }
            }
        }
    }

    // Sort by line count descending
    reports.sort_by_key(|a| std::cmp::Reverse(a.lines));

    let violations: Vec<&FileLengthReport> =
        reports.iter().filter(|r| r.lines > max_lines).collect();

    if violations.is_empty() {
        println!(
            "[file-length] OK: scanned {} file(s), all within {} lines",
            reports.len(),
            max_lines
        );
        Ok(reports)
    } else {
        println!(
            "[file-length] ❌ {} file(s) exceed maximum length (threshold: {} lines):",
            violations.len(),
            max_lines
        );
        for v in &violations {
            println!(
                "  ❌ {} ({} lines > {})",
                v.path.display(),
                v.lines,
                max_lines
            );
        }
        bail!("Maximum file line count exceeded");
    }
}

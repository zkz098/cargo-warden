//! Shared `.gitignore`-aware file discovery and glob/pattern filtering.

use crate::report::normalize_rel_path;
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Options controlling directory traversal and file filtering.
#[derive(Debug, Clone)]
pub struct WalkOptions<'a> {
    pub extensions: &'a [String],
    pub ignore_patterns: &'a [String],
    pub respect_gitignore: bool,
    pub base_dir: Option<&'a Path>,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
}

struct IgnoreMatcher {
    raw_patterns: Vec<String>,
    globs: Vec<GlobMatcher>,
}

impl IgnoreMatcher {
    fn new(patterns: &[String]) -> Self {
        let mut raw_patterns = Vec::new();
        let mut globs = Vec::new();
        for pat in patterns {
            let trimmed = pat.trim();
            if trimmed.is_empty() {
                continue;
            }
            if looks_like_glob(trimmed)
                && let Ok(glob) = GlobBuilder::new(trimmed).literal_separator(false).build()
            {
                globs.push(glob.compile_matcher());
            }
            raw_patterns.push(trimmed.replace('\\', "/"));
        }
        Self {
            raw_patterns,
            globs,
        }
    }

    fn is_ignored(&self, full_path: &Path, rel_path: &Path) -> bool {
        let full_str = full_path.to_string_lossy().replace('\\', "/");
        let rel_str = rel_path.to_string_lossy().replace('\\', "/");

        if self
            .globs
            .iter()
            .any(|g| g.is_match(&rel_str) || g.is_match(&full_str))
        {
            return true;
        }

        self.raw_patterns
            .iter()
            .any(|pat| full_str.contains(pat) || rel_str.contains(pat))
    }
}

fn looks_like_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[') || pattern.contains('{')
}

/// Collect files under `roots` honoring `.gitignore`, extensions, ignore patterns, and optional changeset.
pub fn collect_files(roots: &[PathBuf], opts: &WalkOptions<'_>) -> Vec<PathBuf> {
    let ext_set: Vec<String> = opts
        .extensions
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect();
    let matcher = IgnoreMatcher::new(opts.ignore_patterns);

    let mut seen = BTreeSet::new();
    let mut files = Vec::new();

    for root in roots {
        if !root.exists() {
            continue;
        }

        let mut builder = WalkBuilder::new(root);
        builder
            .hidden(false)
            .git_ignore(opts.respect_gitignore)
            .git_exclude(opts.respect_gitignore)
            .git_global(false)
            .ignore(false)
            .parents(opts.respect_gitignore)
            .require_git(false)
            .filter_entry(|entry| entry.file_name() != ".git");

        for entry in builder.build().flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            if !ext_set.is_empty() {
                let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !ext_set.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                    continue;
                }
            }

            let rel_path = opts
                .base_dir
                .map(|b| normalize_rel_path(b, path))
                .unwrap_or_else(|| normalize_rel_path(root, path));

            if matcher.is_ignored(path, &rel_path) {
                tracing::trace!(file = %path.display(), "skipping ignored path");
                continue;
            }

            if let Some(changed) = opts.changeset
                && !changed.contains(&rel_path)
            {
                tracing::trace!(file = %path.display(), "skipping unchanged file");
                continue;
            }

            let canonical_key = path.to_path_buf();
            if seen.insert(canonical_key) {
                files.push(path.to_path_buf());
            }
        }
    }

    files.sort();
    files
}

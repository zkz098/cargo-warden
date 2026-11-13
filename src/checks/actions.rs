//! GitHub Actions `uses:` version update checker.
//!
//! Scans `.github/workflows/*.yml|yaml` (or configured paths) for `uses: owner/repo[@subpath]@ref`
//! references, queries remote Git tags via `git ls-remote --tags`, and reports outdated action
//! references.

use crate::checks::parallel;
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

/// Options controlling GitHub Actions update checking.
#[derive(Debug, Clone)]
pub struct ActionsOptions<'a> {
    /// Workflow files or directories to scan (defaults to `[".github/workflows"]` when empty).
    pub workflows: &'a [PathBuf],
    /// Action repositories or patterns to ignore (e.g. `actions/checkout`, `owner/*`).
    pub ignore: &'a [String],
    /// Emit `Severity::Error` instead of `Severity::Warning` for outdated actions.
    pub strict: bool,
    /// Skip remote `git ls-remote` queries (only scan/parse local workflow files).
    pub offline: bool,
    /// Optional git changeset filter (`--staged` / `--diff`).
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    /// Emit human-readable status lines to stdout when `true`.
    pub emit_text: bool,
    /// Query unique remote repositories in parallel.
    pub parallel: bool,
}

/// A parsed `uses:` reference found in a GitHub Actions workflow file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionReference {
    pub file: PathBuf,
    pub line: usize,
    pub owner: String,
    pub repo: String,
    pub subpath: Option<String>,
    pub git_ref: String,
    pub raw_uses: String,
}

impl ActionReference {
    /// Returns `owner/repo`.
    pub fn repo_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Returns `owner/repo` or `owner/repo/subpath`.
    pub fn full_action_name(&self) -> String {
        match &self.subpath {
            Some(sub) => format!("{}/{}/{}", self.owner, self.repo, sub),
            None => self.repo_slug(),
        }
    }
}

/// A single tag and its resolved commit SHA from `git ls-remote --tags`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTagInfo {
    pub tag: String,
    pub sha: String,
}

/// Precision of a parsed semver-like tag (`v4` vs `v4.1` vs `v4.1.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionPrecision {
    MajorOnly,
    MajorMinor,
    Full,
}

/// Parsed semver-like Git tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemverTag {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub precision: VersionPrecision,
    pub is_prerelease: bool,
    pub raw: String,
}

impl SemverTag {
    /// Compare `(major, minor, patch)` and stable > prerelease for ordering.
    fn cmp_version(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| (!self.is_prerelease).cmp(&(!other.is_prerelease)))
    }
}

/// Parse a Git tag like `v4`, `v4.1`, `v4.1.1`, `4.1.1`, or `v1.2.3-rc.1`.
pub fn parse_semver_tag(tag: &str) -> Option<SemverTag> {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return None;
    }

    let has_v_prefix = trimmed.starts_with('v') || trimmed.starts_with('V');
    let without_v = if has_v_prefix { &trimmed[1..] } else { trimmed };

    if !without_v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }

    let without_build = without_v.split('+').next().unwrap_or(without_v);
    let (core, is_prerelease) = match without_build.split_once('-') {
        Some((c, pre)) if !pre.is_empty() => (c, true),
        Some(_) => return None,
        None => (without_build, false),
    };

    let parts: Vec<&str> = core.split('.').collect();
    match parts.as_slice() {
        [maj] if has_v_prefix && !is_prerelease => Some(SemverTag {
            major: maj.parse().ok()?,
            minor: 0,
            patch: 0,
            precision: VersionPrecision::MajorOnly,
            is_prerelease: false,
            raw: trimmed.to_string(),
        }),
        [maj, min] => Some(SemverTag {
            major: maj.parse().ok()?,
            minor: min.parse().ok()?,
            patch: 0,
            precision: VersionPrecision::MajorMinor,
            is_prerelease,
            raw: trimmed.to_string(),
        }),
        [maj, min, pat] => Some(SemverTag {
            major: maj.parse().ok()?,
            minor: min.parse().ok()?,
            patch: pat.parse().ok()?,
            precision: VersionPrecision::Full,
            is_prerelease,
            raw: trimmed.to_string(),
        }),
        _ => None,
    }
}

fn is_sha40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse the stdout of `git ls-remote --tags <url>` into deduplicated tag -> commit SHA entries.
pub fn parse_ls_remote_tags(stdout: &str) -> Vec<RemoteTagInfo> {
    // Prefer dereferenced `^{}` SHA for annotated tags over the tag object SHA.
    let mut map: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for line in stdout.lines() {
        let Some((sha, ref_name)) = line.split_once('\t') else {
            continue;
        };
        let Some(tag_part) = ref_name.strip_prefix("refs/tags/") else {
            continue;
        };
        let (tag_name, is_deref) = match tag_part.strip_suffix("^{}") {
            Some(t) => (t, true),
            None => (tag_part, false),
        };
        if tag_name.is_empty() {
            continue;
        }
        let sha_trimmed = sha.trim().to_string();
        match map.get(tag_name) {
            Some((_, true)) if !is_deref => {}
            _ => {
                map.insert(tag_name.to_string(), (sha_trimmed, is_deref));
            }
        }
    }
    map.into_iter()
        .map(|(tag, (sha, _))| RemoteTagInfo { tag, sha })
        .collect()
}

/// Fetch tags from a Git repository URL via `git ls-remote --tags`.
pub fn fetch_remote_tags(repo_url: &str) -> Result<Vec<RemoteTagInfo>> {
    let output = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["ls-remote", "--tags", repo_url])
        .output()
        .with_context(|| format!("Failed to run `git ls-remote` for {repo_url}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "`git ls-remote --tags {}` failed: {}",
            repo_url,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_ls_remote_tags(&stdout))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedUses {
    owner: String,
    repo: String,
    subpath: Option<String>,
    git_ref: String,
    raw_uses: String,
}

fn strip_yaml_quotes_and_comment(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(first @ ('"' | '\'')) = trimmed.chars().next()
        && let Some(end) = trimmed[1..].find(first)
    {
        return trimmed[1..1 + end].trim().to_string();
    }
    // Unquoted: strip trailing `# comment` if preceded by whitespace or `#`
    let without_comment = match trimmed.find(" #") {
        Some(idx) => &trimmed[..idx],
        None => trimmed,
    };
    without_comment.trim().to_string()
}

fn parse_uses_value(raw_val: &str) -> Option<ParsedUses> {
    let cleaned = strip_yaml_quotes_and_comment(raw_val);
    if cleaned.is_empty()
        || cleaned.starts_with("./")
        || cleaned.starts_with(".\\")
        || cleaned.starts_with("docker://")
        || cleaned.contains("${{")
    {
        return None;
    }

    let (left, git_ref) = cleaned.split_once('@')?;
    let git_ref = git_ref.trim();
    if left.is_empty() || git_ref.is_empty() {
        return None;
    }

    let segments: Vec<&str> = left.split('/').collect();
    if segments.len() < 2 || segments[0].is_empty() || segments[1].is_empty() {
        return None;
    }

    let subpath = if segments.len() > 2 {
        Some(segments[2..].join("/"))
    } else {
        None
    };

    Some(ParsedUses {
        owner: segments[0].to_string(),
        repo: segments[1].to_string(),
        subpath,
        git_ref: git_ref.to_string(),
        raw_uses: cleaned,
    })
}

fn line_indent(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn starts_block_scalar(trimmed_after_key: &str) -> bool {
    let val = trimmed_after_key.trim();
    let Some(first) = val.chars().next() else {
        return false;
    };
    if first != '|' && first != '>' {
        return false;
    }
    val[1..]
        .chars()
        .all(|c| c == '-' || c == '+' || c.is_ascii_digit() || c.is_whitespace())
        || val[1..].contains('#')
}

/// Scan a single GitHub Actions workflow YAML file for external `uses:` references.
pub fn scan_workflow_file(base_dir: &Path, path: &Path) -> Result<Vec<ActionReference>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read workflow file: {}", path.display()))?;
    let rel = normalize_rel_path(base_dir, path);
    let mut refs = Vec::new();
    let mut block_scalar_indent: Option<usize> = None;

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let indent = line_indent(line);

        if let Some(base_indent) = block_scalar_indent {
            if trimmed.is_empty() || indent > base_indent {
                continue;
            }
            block_scalar_indent = None;
        }

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Track multiline `run: |` / `run: >` blocks so script contents aren't parsed as YAML keys.
        let key_part = trimmed.strip_prefix("- ").map_or(trimmed, str::trim_start);
        if let Some(after_run) = key_part.strip_prefix("run:")
            && starts_block_scalar(after_run)
        {
            block_scalar_indent = Some(indent);
            continue;
        }

        if let Some(after_uses) = key_part.strip_prefix("uses:")
            && let Some(parsed) = parse_uses_value(after_uses)
        {
            refs.push(ActionReference {
                file: rel.clone(),
                line: idx + 1,
                owner: parsed.owner,
                repo: parsed.repo,
                subpath: parsed.subpath,
                git_ref: parsed.git_ref,
                raw_uses: parsed.raw_uses,
            });
        }
    }

    Ok(refs)
}

fn is_yaml_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("yml") || e.eq_ignore_ascii_case("yaml"))
}

/// Discover workflow `.yml`/`.yaml` files from the configured paths (defaults to `.github/workflows`).
pub fn discover_workflow_files(base_dir: &Path, workflows: &[PathBuf]) -> Vec<PathBuf> {
    let targets: Vec<PathBuf> = if workflows.is_empty() {
        vec![base_dir.join(".github/workflows")]
    } else {
        workflows
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    p.clone()
                } else {
                    base_dir.join(p)
                }
            })
            .collect()
    };

    let mut files = BTreeSet::new();
    for target in targets {
        if target.is_file() {
            files.insert(target);
        } else if target.is_dir() {
            for entry in WalkDir::new(&target)
                .sort_by_file_name()
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                let p = entry.path();
                if p.is_file() && is_yaml_file(p) {
                    files.insert(p.to_path_buf());
                }
            }
        }
    }
    files.into_iter().collect()
}

fn should_ignore_action(action: &ActionReference, ignore: &[String]) -> bool {
    let slug = action.repo_slug();
    let full = action.full_action_name();
    ignore.iter().any(|pat| {
        let p = pat.trim();
        if let Some(prefix) = p.strip_suffix("/*") {
            action.owner.eq_ignore_ascii_case(prefix)
        } else {
            slug.eq_ignore_ascii_case(p) || full.eq_ignore_ascii_case(p)
        }
    })
}

fn should_include_file_in_changeset(
    rel_file: &Path,
    changeset: Option<&BTreeSet<PathBuf>>,
) -> bool {
    let Some(changed) = changeset else {
        return true;
    };
    let rel_str = rel_file.to_string_lossy().replace('\\', "/");
    changed.iter().any(|p| {
        let s = p.to_string_lossy().replace('\\', "/");
        s == rel_str || s == "warden.toml" || s == ".warden.toml"
    })
}

fn pick_latest_semver(
    tags: &[(RemoteTagInfo, SemverTag)],
    allow_prerelease: bool,
) -> Option<&(RemoteTagInfo, SemverTag)> {
    tags.iter()
        .filter(|(_, sv)| allow_prerelease || !sv.is_prerelease)
        .max_by(|(_, a), (_, b)| a.cmp_version(b))
}

fn find_recommended_tag(
    tags: &[(RemoteTagInfo, SemverTag)],
    latest: &SemverTag,
    precision: VersionPrecision,
) -> String {
    match precision {
        VersionPrecision::MajorOnly => {
            let floating = format!("v{}", latest.major);
            if tags.iter().any(|(info, _)| info.tag == floating) {
                floating
            } else {
                latest.raw.clone()
            }
        }
        VersionPrecision::MajorMinor => {
            let floating = format!("v{}.{}", latest.major, latest.minor);
            if tags.iter().any(|(info, _)| info.tag == floating) {
                floating
            } else {
                latest.raw.clone()
            }
        }
        VersionPrecision::Full => latest.raw.clone(),
    }
}

fn is_outdated_semver(current: &SemverTag, latest: &SemverTag) -> bool {
    match current.precision {
        VersionPrecision::MajorOnly => latest.major > current.major,
        VersionPrecision::MajorMinor => {
            (latest.major, latest.minor) > (current.major, current.minor)
        }
        VersionPrecision::Full => latest.cmp_version(current) == std::cmp::Ordering::Greater,
    }
}

fn evaluate_action_against_tags(
    action: &ActionReference,
    remote_tags: &[RemoteTagInfo],
    severity: Severity,
) -> Option<Diagnostic> {
    let semver_tags: Vec<(RemoteTagInfo, SemverTag)> = remote_tags
        .iter()
        .filter_map(|info| parse_semver_tag(&info.tag).map(|sv| (info.clone(), sv)))
        .collect();

    if semver_tags.is_empty() {
        return None;
    }

    if let Some(current_sv) = parse_semver_tag(&action.git_ref) {
        let latest = pick_latest_semver(&semver_tags, current_sv.is_prerelease)?;
        if !is_outdated_semver(&current_sv, &latest.1) {
            return None;
        }
        let recommended = find_recommended_tag(&semver_tags, &latest.1, current_sv.precision);
        return Some(make_outdated_diagnostic(
            action,
            &action.git_ref,
            &recommended,
            None,
            severity,
        ));
    }

    if is_sha40(&action.git_ref) {
        let matched = semver_tags
            .iter()
            .filter(|(info, _)| info.sha.eq_ignore_ascii_case(&action.git_ref))
            .max_by(|(_, a), (_, b)| a.cmp_version(b))?;
        let latest = pick_latest_semver(&semver_tags, matched.1.is_prerelease)?;
        if latest.1.cmp_version(&matched.1) != std::cmp::Ordering::Greater
            || latest.0.sha.eq_ignore_ascii_case(&action.git_ref)
        {
            return None;
        }
        let current_label = format!("{} ({})", &action.git_ref[..7], matched.1.raw);
        let latest_hint = Some(latest.0.sha[..7.min(latest.0.sha.len())].to_string());
        return Some(make_outdated_diagnostic(
            action,
            &current_label,
            &latest.1.raw,
            latest_hint.as_deref(),
            severity,
        ));
    }

    None
}

fn make_outdated_diagnostic(
    action: &ActionReference,
    current_label: &str,
    recommended: &str,
    latest_sha_short: Option<&str>,
    severity: Severity,
) -> Diagnostic {
    let rel_str = action.file.to_string_lossy().replace('\\', "/");
    let full_name = action.full_action_name();
    let sha_suffix = latest_sha_short
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    let message = format!(
        "{}:{}: action `{}` uses `{}` which can be updated to `{}`{}",
        rel_str, action.line, full_name, current_label, recommended, sha_suffix
    );
    Diagnostic {
        rule_id: "warden/actions/outdated".to_string(),
        check: "actions".to_string(),
        severity,
        message,
        file: Some(action.file.clone()),
        line: Some(action.line),
        column: None,
        fingerprint: format!(
            "actions:outdated:{}:{}:{}:{}",
            rel_str, action.line, full_name, action.git_ref
        ),
        metric_value: None,
        suppressed: false,
        suppression_reason: None,
    }
}

fn make_fetch_failed_diagnostic(action: &ActionReference, err_msg: &str) -> Diagnostic {
    let rel_str = action.file.to_string_lossy().replace('\\', "/");
    let slug = action.repo_slug();
    Diagnostic {
        rule_id: "warden/actions/fetch-failed".to_string(),
        check: "actions".to_string(),
        severity: Severity::Warning,
        message: format!(
            "{}:{}: failed to check updates for `{}`: {}",
            rel_str, action.line, slug, err_msg
        ),
        file: Some(action.file.clone()),
        line: Some(action.line),
        column: None,
        fingerprint: format!("actions:fetch-failed:{}:{}:{}", rel_str, action.line, slug),
        metric_value: None,
        suppressed: false,
        suppression_reason: None,
    }
}

/// Collect diagnostics for outdated GitHub Actions references using the default `https://github.com` remote.
pub fn collect_actions(base_dir: &Path, opts: &ActionsOptions<'_>) -> Result<Vec<Diagnostic>> {
    collect_actions_with_fetcher(base_dir, opts, |slug| {
        let url = format!("https://github.com/{slug}.git");
        fetch_remote_tags(&url)
    })
}

/// Collect GitHub Actions diagnostics using `WardenConfig` with optional CLI overrides.
#[allow(clippy::too_many_arguments)]
pub fn collect_actions_from_config(
    config: &crate::config::WardenConfig,
    base_dir: &Path,
    workflow: &[PathBuf],
    ignore: &[String],
    strict: bool,
    offline: bool,
    changeset: Option<&BTreeSet<PathBuf>>,
    emit_text: bool,
) -> Result<Vec<Diagnostic>> {
    let cfg = config.actions.clone().unwrap_or_default();
    let workflows = if workflow.is_empty() {
        &cfg.workflows
    } else {
        workflow
    };
    let ignored = if ignore.is_empty() {
        &cfg.ignore
    } else {
        ignore
    };
    let parallel = config.check.as_ref().is_none_or(|c| c.parallel);

    collect_actions(
        base_dir,
        &ActionsOptions {
            workflows,
            ignore: ignored,
            strict: strict || cfg.strict,
            offline,
            changeset,
            emit_text,
            parallel,
        },
    )
}

/// Collect diagnostics using a custom tag-fetcher (primarily for offline/mocked testing).
pub fn collect_actions_with_fetcher<F>(
    base_dir: &Path,
    opts: &ActionsOptions<'_>,
    fetcher: F,
) -> Result<Vec<Diagnostic>>
where
    F: Fn(&str) -> Result<Vec<RemoteTagInfo>> + Sync,
{
    let workflow_files = discover_workflow_files(base_dir, opts.workflows);
    let mut refs = Vec::new();
    for wf in &workflow_files {
        let rel = normalize_rel_path(base_dir, wf);
        if !should_include_file_in_changeset(&rel, opts.changeset) {
            continue;
        }
        let file_refs = scan_workflow_file(base_dir, wf)?;
        for r in file_refs {
            if !should_ignore_action(&r, opts.ignore) {
                refs.push(r);
            }
        }
    }

    if refs.is_empty() || opts.offline {
        if opts.emit_text && opts.offline {
            println!(
                "ℹ️  [actions] scanned {} workflow file(s), found {} action reference(s) (offline mode)",
                workflow_files.len(),
                refs.len()
            );
        }
        return Ok(Vec::new());
    }

    let unique_repos: Vec<String> = refs
        .iter()
        .map(ActionReference::repo_slug)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    if opts.emit_text {
        println!(
            "🔍 [actions] checking {} action reference(s) across {} unique repository/repositories...",
            refs.len(),
            unique_repos.len()
        );
    }

    let fetched = parallel::map_items(&unique_repos, opts.parallel, |slug| {
        (slug.clone(), fetcher(slug))
    });
    let repo_map: BTreeMap<String, Result<Vec<RemoteTagInfo>>> = fetched.into_iter().collect();

    let severity = if opts.strict {
        Severity::Error
    } else {
        Severity::Warning
    };

    let mut diagnostics = Vec::new();
    let mut reported_fetch_err = BTreeSet::new();
    for r in &refs {
        let slug = r.repo_slug();
        match repo_map.get(&slug) {
            Some(Ok(tags)) => {
                if let Some(diag) = evaluate_action_against_tags(r, tags, severity) {
                    diagnostics.push(diag);
                }
            }
            Some(Err(err)) if reported_fetch_err.insert(slug.clone()) => {
                diagnostics.push(make_fetch_failed_diagnostic(r, &err.to_string()));
            }
            Some(Err(_)) | None => {}
        }
    }

    Ok(diagnostics)
}

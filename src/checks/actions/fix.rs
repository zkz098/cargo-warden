//! Automatic workflow upgrading (`--fix`) and SHA pinning (`--pin-sha`) for GitHub Actions.

use super::{
    ActionReference, ActionsOptions, RemoteTagInfo, SemverTag, VersionPrecision, fetch_remote_tags,
    find_recommended_tag, is_outdated_semver, is_sha40, parse_semver_tag, pick_latest_semver,
    scan_filtered_workflow_refs,
};
use crate::checks::parallel;
use crate::report::normalize_rel_path;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Summary of automatic workflow updates performed by `fix_actions`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionsFixStats {
    pub files_updated: usize,
    pub refs_updated: usize,
}

/// Automatically upgrade outdated action references and/or pin them to 40-char commit SHAs.
pub fn fix_actions(
    base_dir: &Path,
    opts: &ActionsOptions<'_>,
    upgrade: bool,
    pin_sha: bool,
) -> Result<ActionsFixStats> {
    fix_actions_with_fetcher(base_dir, opts, upgrade, pin_sha, |slug| {
        let url = format!("https://github.com/{slug}.git");
        fetch_remote_tags(&url)
    })
}

/// Automatically upgrade/pin action references using a custom tag-fetcher (for testing).
pub fn fix_actions_with_fetcher<F>(
    base_dir: &Path,
    opts: &ActionsOptions<'_>,
    upgrade: bool,
    pin_sha: bool,
    fetcher: F,
) -> Result<ActionsFixStats>
where
    F: Fn(&str) -> Result<Vec<RemoteTagInfo>> + Sync,
{
    if (!upgrade && !pin_sha) || opts.offline {
        return Ok(ActionsFixStats::default());
    }

    let (_, all_refs) = scan_filtered_workflow_refs(base_dir, opts)?;
    if all_refs.is_empty() {
        return Ok(ActionsFixStats::default());
    }

    let mut file_refs: BTreeMap<PathBuf, Vec<ActionReference>> = BTreeMap::new();
    for r in &all_refs {
        file_refs
            .entry(base_dir.join(&r.file))
            .or_default()
            .push(r.clone());
    }

    let unique_repos: Vec<String> = all_refs
        .iter()
        .map(ActionReference::repo_slug)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let fetched = parallel::map_items(&unique_repos, opts.parallel, |slug| {
        (slug.clone(), fetcher(slug))
    });
    let repo_map: BTreeMap<String, Result<Vec<RemoteTagInfo>>> = fetched.into_iter().collect();

    let mut stats = ActionsFixStats::default();
    for (wf_path, refs) in file_refs {
        let updated_count = apply_fixes_to_file(
            base_dir,
            &wf_path,
            &refs,
            &repo_map,
            upgrade,
            pin_sha,
            opts.emit_text,
        )?;
        if updated_count > 0 {
            stats.files_updated += 1;
            stats.refs_updated += updated_count;
        }
    }

    Ok(stats)
}

fn apply_fixes_to_file(
    base_dir: &Path,
    wf_path: &Path,
    refs: &[ActionReference],
    repo_map: &BTreeMap<String, Result<Vec<RemoteTagInfo>>>,
    upgrade: bool,
    pin_sha: bool,
    emit_text: bool,
) -> Result<usize> {
    let content = fs::read_to_string(wf_path)
        .with_context(|| format!("Failed to read workflow file: {}", wf_path.display()))?;
    let line_ending = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let mut updated_count = 0usize;

    for r in refs {
        let Some(Ok(tags)) = repo_map.get(&r.repo_slug()) else {
            continue;
        };
        let Some((new_ref, version_comment)) = compute_target_ref(r, tags, upgrade, pin_sha) else {
            continue;
        };
        let idx = r.line.saturating_sub(1);
        if let Some(line) = lines.get_mut(idx)
            && let Some(updated) = rewrite_uses_line(line, r, &new_ref, version_comment.as_deref())
        {
            *line = updated;
            updated_count += 1;
        }
    }

    if updated_count > 0 {
        let mut out = lines.join(line_ending);
        if trailing_newline {
            out.push_str(line_ending);
        }
        fs::write(wf_path, out)
            .with_context(|| format!("Failed to write workflow file: {}", wf_path.display()))?;
        if emit_text {
            println!(
                "🔧 [actions] updated {}",
                normalize_rel_path(base_dir, wf_path).display()
            );
        }
    }

    Ok(updated_count)
}

fn compute_target_ref(
    action: &ActionReference,
    remote_tags: &[RemoteTagInfo],
    upgrade: bool,
    pin_sha: bool,
) -> Option<(String, Option<String>)> {
    let semver_tags: Vec<(RemoteTagInfo, SemverTag)> = remote_tags
        .iter()
        .filter_map(|info| parse_semver_tag(&info.tag).map(|sv| (info.clone(), sv)))
        .collect();

    if is_sha40(&action.git_ref) {
        if !upgrade {
            return None;
        }
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
        return Some((latest.0.sha.clone(), Some(latest.1.raw.clone())));
    }

    if let Some(current_sv) = parse_semver_tag(&action.git_ref) {
        if upgrade {
            let latest = pick_latest_semver(&semver_tags, current_sv.is_prerelease)?;
            let outdated = is_outdated_semver(&current_sv, &latest.1);
            if pin_sha {
                let target = if outdated {
                    latest
                } else {
                    semver_tags
                        .iter()
                        .find(|(info, _)| info.tag == action.git_ref)
                        .unwrap_or(latest)
                };
                let label = most_precise_tag_for_sha(&semver_tags, &target.0.sha)
                    .unwrap_or_else(|| target.1.raw.clone());
                return Some((target.0.sha.clone(), Some(label)));
            } else if outdated {
                let recommended =
                    find_recommended_tag(&semver_tags, &latest.1, current_sv.precision);
                return Some((recommended, None));
            }
            return None;
        }
        if pin_sha {
            let exact = remote_tags.iter().find(|info| info.tag == action.git_ref)?;
            let label = most_precise_tag_for_sha(&semver_tags, &exact.sha)
                .unwrap_or_else(|| action.git_ref.clone());
            return Some((exact.sha.clone(), Some(label)));
        }
        return None;
    }

    if pin_sha {
        let exact = remote_tags.iter().find(|info| info.tag == action.git_ref)?;
        return Some((exact.sha.clone(), Some(exact.tag.clone())));
    }
    None
}

fn most_precise_tag_for_sha(tags: &[(RemoteTagInfo, SemverTag)], sha: &str) -> Option<String> {
    tags.iter()
        .filter(|(info, _)| info.sha.eq_ignore_ascii_case(sha))
        .max_by(|(_, a), (_, b)| {
            a.cmp_version(b).then_with(|| {
                let rank = |p: VersionPrecision| match p {
                    VersionPrecision::Full => 2,
                    VersionPrecision::MajorMinor => 1,
                    VersionPrecision::MajorOnly => 0,
                };
                rank(a.precision).cmp(&rank(b.precision))
            })
        })
        .map(|(_, sv)| sv.raw.clone())
}

fn rewrite_uses_line(
    line: &str,
    action: &ActionReference,
    new_ref: &str,
    version_comment: Option<&str>,
) -> Option<String> {
    let uses_idx = line.find("uses:")?;
    let prefix = &line[..uses_idx + "uses:".len()];
    let after = &line[uses_idx + "uses:".len()..];
    let ws_len = after.len() - after.trim_start().len();
    let ws = &after[..ws_len];
    let rest = &after[ws_len..];

    let full_name = action.full_action_name();
    let new_uses = format!("{full_name}@{new_ref}");

    if let Some(quote @ ('"' | '\'')) = rest.chars().next()
        && let Some(end_q) = rest[1..].find(quote)
    {
        let trailing = rest[1 + end_q + 1..].trim_start();
        let comment_part = build_trailing_comment(trailing, version_comment);
        return Some(format!(
            "{prefix}{ws}{quote}{new_uses}{quote}{comment_part}"
        ));
    }

    let trailing = match rest.find(" #") {
        Some(idx) => rest[idx..].trim_start(),
        None => "",
    };
    let comment_part = build_trailing_comment(trailing, version_comment);
    Some(format!("{prefix}{ws}{new_uses}{comment_part}"))
}

fn build_trailing_comment(existing_trailing: &str, version_comment: Option<&str>) -> String {
    match version_comment {
        Some(ver) => format!(" # {ver}"),
        None if !existing_trailing.is_empty() => format!(" {existing_trailing}"),
        None => String::new(),
    }
}

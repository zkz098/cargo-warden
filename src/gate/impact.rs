//! Workspace impact analysis for incremental `cargo warden gate`.
//!
//! Maps a Git changeset to the minimal set of affected workspace packages via:
//! 1. Global configuration file detection (root `Cargo.toml`, `Cargo.lock`, toolchain/config files)
//! 2. Longest-prefix matching against workspace member directories
//! 3. Reverse dependency closure over `normal`/`build` edges + 1-hop propagation over `dev` edges

use crate::report::normalize_rel_path;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Scope of Cargo packages to verify in `cargo warden gate`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AffectedScope {
    /// Run full workspace gate (no changeset, global files changed, or all members affected).
    #[default]
    All,
    /// No workspace packages affected (e.g. only docs/CI/README changed).
    None { has_rust_changes: bool },
    /// Specific subset of workspace packages affected.
    Packages {
        packages: BTreeSet<String>,
        total_members: usize,
    },
}

/// A workspace member crate with its relative directory under the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMember {
    pub name: String,
    pub rel_dir: String,
}

#[derive(Debug, Default)]
pub struct WorkspaceGraph {
    pub members: Vec<WorkspaceMember>,
    /// `dep -> dependents` for `normal` and `build` dependencies.
    pub rev_normal: BTreeMap<String, BTreeSet<String>>,
    /// `dep -> dependents` for `dev` dependencies (1-hop propagation only).
    pub rev_dev: BTreeMap<String, BTreeSet<String>>,
}

const GLOBAL_TRIGGER_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rustfmt.toml",
    ".rustfmt.toml",
    ".cargo/config.toml",
    ".cargo/config",
    "deny.toml",
    "warden.toml",
    ".warden.toml",
];

fn normalize_slash(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    s.trim_start_matches("./").trim_end_matches('/').to_string()
}

fn is_global_trigger(rel_path: &str) -> bool {
    GLOBAL_TRIGGER_FILES.contains(&rel_path)
}

fn find_owning_member<'a>(members: &'a [WorkspaceMember], rel_path: &str) -> Option<&'a str> {
    let mut best: Option<(&str, usize)> = None;
    for m in members {
        if m.rel_dir.is_empty() || m.rel_dir == "." {
            if best.is_none() {
                best = Some((m.name.as_str(), 0));
            }
            continue;
        }
        if rel_path == m.rel_dir || rel_path.starts_with(&format!("{}/", m.rel_dir)) {
            let len = m.rel_dir.len();
            if best.is_none_or(|(_, best_len)| len > best_len) {
                best = Some((m.name.as_str(), len));
            }
        }
    }
    best.map(|(name, _)| name)
}

/// Compute the affected workspace scope from a pre-built workspace dependency graph.
pub fn compute_affected_from_graph(
    graph: &WorkspaceGraph,
    changed: &BTreeSet<PathBuf>,
) -> AffectedScope {
    if changed.is_empty() {
        return AffectedScope::None {
            has_rust_changes: false,
        };
    }

    let mut has_rust_changes = false;
    let mut direct_hits = BTreeSet::new();

    for path in changed {
        let rel = normalize_slash(path);
        if rel.ends_with(".rs") {
            has_rust_changes = true;
        }
        if is_global_trigger(&rel) {
            return AffectedScope::All;
        }
        if let Some(pkg) = find_owning_member(&graph.members, &rel) {
            direct_hits.insert(pkg.to_string());
        }
    }

    if direct_hits.is_empty() {
        return AffectedScope::None { has_rust_changes };
    }

    if graph.members.len() <= 1 {
        return AffectedScope::All;
    }

    let affected = propagate_reverse_deps(graph, direct_hits);
    if affected.len() >= graph.members.len() {
        return AffectedScope::All;
    }

    AffectedScope::Packages {
        packages: affected,
        total_members: graph.members.len(),
    }
}

fn propagate_reverse_deps(
    graph: &WorkspaceGraph,
    direct_hits: BTreeSet<String>,
) -> BTreeSet<String> {
    // Propagate through `normal` and `build` reverse dependencies via BFS.
    let mut normal_closure = BTreeSet::new();
    let mut queue = VecDeque::new();
    for hit in direct_hits {
        if normal_closure.insert(hit.clone()) {
            queue.push_back(hit);
        }
    }

    while let Some(pkg) = queue.pop_front() {
        if let Some(dependents) = graph.rev_normal.get(&pkg) {
            for dep_pkg in dependents {
                if normal_closure.insert(dep_pkg.clone()) {
                    queue.push_back(dep_pkg.clone());
                }
            }
        }
    }

    // 1-hop propagation over `dev` edges (dev-dependents' tests are affected,
    // but their downstream normal dependents are not).
    let mut affected = normal_closure.clone();
    for pkg in &normal_closure {
        if let Some(dev_dependents) = graph.rev_dev.get(pkg) {
            affected.extend(dev_dependents.iter().cloned());
        }
    }
    affected
}

/// Query `cargo metadata --no-deps` and compute the affected workspace scope for `changed`.
pub fn compute_affected_scope(
    base_dir: &Path,
    changed: &BTreeSet<PathBuf>,
) -> Result<AffectedScope> {
    if changed.is_empty() {
        return Ok(AffectedScope::None {
            has_rust_changes: false,
        });
    }
    for path in changed {
        if is_global_trigger(&normalize_slash(path)) {
            return Ok(AffectedScope::All);
        }
    }

    let graph = load_workspace_graph(base_dir)?;
    Ok(compute_affected_from_graph(&graph, changed))
}

fn load_workspace_graph(base_dir: &Path) -> Result<WorkspaceGraph> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(base_dir)
        .output()
        .context("Failed to execute `cargo metadata` for gate impact analysis")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`cargo metadata` failed: {}", stderr.trim());
    }

    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Failed to parse `cargo metadata` JSON")?;

    let member_ids: BTreeSet<String> = meta["workspace_members"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let pkgs = meta["packages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let (members, ws_names) = extract_members(base_dir, pkgs, &member_ids);
    let (rev_normal, rev_dev) = extract_reverse_edges(pkgs, &member_ids, &ws_names);

    Ok(WorkspaceGraph {
        members,
        rev_normal,
        rev_dev,
    })
}

fn extract_members(
    base_dir: &Path,
    pkgs: &[serde_json::Value],
    member_ids: &BTreeSet<String>,
) -> (Vec<WorkspaceMember>, BTreeSet<String>) {
    let mut members = Vec::new();
    let mut ws_names = BTreeSet::new();
    for pkg in pkgs {
        let Some(id) = pkg["id"].as_str() else {
            continue;
        };
        if !member_ids.contains(id) {
            continue;
        }
        let (Some(name), Some(manifest_str)) =
            (pkg["name"].as_str(), pkg["manifest_path"].as_str())
        else {
            continue;
        };
        let dir = Path::new(manifest_str).parent().unwrap_or(base_dir);
        let rel_dir = normalize_slash(&normalize_rel_path(base_dir, dir));
        ws_names.insert(name.to_string());
        members.push(WorkspaceMember {
            name: name.to_string(),
            rel_dir,
        });
    }
    (members, ws_names)
}

#[allow(clippy::type_complexity)]
fn extract_reverse_edges(
    pkgs: &[serde_json::Value],
    member_ids: &BTreeSet<String>,
    ws_names: &BTreeSet<String>,
) -> (
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, BTreeSet<String>>,
) {
    let mut rev_normal: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut rev_dev: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for pkg in pkgs {
        let Some(id) = pkg["id"].as_str() else {
            continue;
        };
        if !member_ids.contains(id) {
            continue;
        }
        let Some(pkg_name) = pkg["name"].as_str() else {
            continue;
        };
        let Some(deps) = pkg["dependencies"].as_array() else {
            continue;
        };
        for dep in deps {
            let Some(dep_name) = dep["name"].as_str() else {
                continue;
            };
            if !ws_names.contains(dep_name) || dep_name == pkg_name {
                continue;
            }
            let kind = dep["kind"].as_str().unwrap_or("normal");
            let target = if kind == "dev" {
                &mut rev_dev
            } else {
                &mut rev_normal
            };
            target
                .entry(dep_name.to_string())
                .or_default()
                .insert(pkg_name.to_string());
        }
    }

    (rev_normal, rev_dev)
}

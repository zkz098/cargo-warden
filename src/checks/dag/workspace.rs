//! Workspace crate-level dependency boundary checker.
//!
//! Uses `cargo metadata` to inspect workspace member dependencies against:
//! - `forbid_deps`: crates that no workspace member may depend on
//! - `layers`: layered architecture rules across workspace crates
//! - `rules`: explicit per-crate or per-layer allow/deny dependency boundaries
//! - Tarjan SCC cycle detection across workspace edges (including dev/build when enabled)
//! - Optional external dependency version conflict detection

use super::find_cycles;
use crate::config::{WorkspaceDagConfig, WorkspaceDagRuleConfig, WorkspaceLayerConfig};
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use crate::suppression::{FileSuppressions, collect_unused_suppression_diagnostics};
use crate::walker::PathPatternMatcher;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Options controlling a workspace crate DAG check run.
#[derive(Debug, Clone)]
pub struct WorkspaceDagOptions<'a> {
    pub base_dir: &'a Path,
    pub config: &'a WorkspaceDagConfig,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DepKind {
    Normal,
    Dev,
    Build,
}

#[derive(Debug, Clone)]
struct CrateDep {
    name: String,
    kind: DepKind,
    rename: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkspaceCrate {
    name: String,
    manifest_path: PathBuf,
    rel_manifest: PathBuf,
    deps: Vec<CrateDep>,
}

struct CompiledLayer<'a> {
    cfg: &'a WorkspaceLayerConfig,
    matcher: PathPatternMatcher,
    has_exact: bool,
}

/// Run workspace-level crate dependency boundary checks and enforce active errors.
pub fn check_workspace_dag(base_dir: &Path, config: &WorkspaceDagConfig) -> Result<()> {
    let diags = collect_workspace_dag(&WorkspaceDagOptions {
        base_dir,
        config,
        changeset: None,
        emit_text: true,
    })?;
    let active: Vec<&Diagnostic> = diags
        .iter()
        .filter(|d| !d.suppressed && d.severity == Severity::Error)
        .collect();
    if active.is_empty() {
        println!("[workspace-dag] OK: workspace crate dependencies satisfy all boundary rules");
        return Ok(());
    }
    let lines: Vec<String> = active
        .iter()
        .map(|d| format!("  ❌ {}", d.message))
        .collect();
    bail!(
        "Workspace crate dependency boundary violation(s) detected:\n{}",
        lines.join("\n")
    );
}

/// Collect structured diagnostics for workspace crate dependency boundaries.
pub fn collect_workspace_dag(opts: &WorkspaceDagOptions<'_>) -> Result<Vec<Diagnostic>> {
    if let Some(changed) = opts.changeset
        && !changeset_touches_manifests(changed)
    {
        tracing::info!("skipping workspace-dag check (no changed Cargo.toml or Cargo.lock)");
        return Ok(Vec::new());
    }

    let metadata = fetch_cargo_metadata(opts.base_dir, opts.config.check_version_conflicts)?;
    let (crates, external_versions) = parse_workspace_metadata(opts.base_dir, &metadata)?;
    let ws_names: BTreeSet<String> = crates.iter().map(|c| c.name.clone()).collect();

    if opts.emit_text {
        println!(
            "[workspace-dag] checking {} workspace crate(s)...",
            crates.len()
        );
    }
    tracing::debug!(
        crates = crates.len(),
        layers = opts.config.layers.len(),
        rules = opts.config.rules.len(),
        forbid_deps = ?opts.config.forbid_deps,
        "starting workspace crate dependency boundary check"
    );

    let compiled_layers = compile_layers(&opts.config.layers);
    let mut diagnostics = Vec::new();
    let mut ws_edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut seen_edges = BTreeSet::new();

    for krate in &crates {
        let manifest_text = fs::read_to_string(&krate.manifest_path).unwrap_or_default();
        let suppressions = FileSuppressions::parse(&manifest_text);
        let rel_str = krate.rel_manifest.to_string_lossy().replace('\\', "/");
        let check_this_crate = should_check_crate(opts.changeset, &krate.rel_manifest);
        let mut crate_diags = Vec::new();

        let from_layer = find_crate_layer(&krate.name, &compiled_layers);

        for dep in &krate.deps {
            if !dep_kind_enabled(dep.kind, opts.config) {
                continue;
            }
            let dep_line = find_dep_line(&manifest_text, dep);
            let suppressed = suppressions.is_suppressed("workspace-dag", dep_line)
                || suppressions.is_suppressed("dag", dep_line);

            if ws_names.contains(&dep.name) && dep.name != krate.name && !suppressed {
                ws_edges
                    .entry(krate.name.clone())
                    .or_default()
                    .insert(dep.name.clone());
            }

            if !check_this_crate {
                continue;
            }

            let target_is_ws = ws_names.contains(&dep.name);
            let to_layer = target_is_ws
                .then(|| find_crate_layer(&dep.name, &compiled_layers))
                .flatten();

            let ctx = EdgeContext {
                krate,
                dep,
                dep_line,
                suppressed,
                target_is_ws,
                from_layer,
                to_layer,
                rel_str: &rel_str,
            };

            check_forbid_dep(&ctx, opts.config, &mut seen_edges, &mut crate_diags);
            check_layer_dep(&ctx, &compiled_layers, &mut seen_edges, &mut crate_diags);
            check_rule_dep(
                &ctx,
                &opts.config.rules,
                &compiled_layers,
                &mut seen_edges,
                &mut crate_diags,
            );
        }

        if check_this_crate && !manifest_text.is_empty() {
            let unused = collect_unused_suppression_diagnostics(
                &krate.rel_manifest,
                &suppressions,
                &["workspace-dag"],
                &crate_diags,
            );
            diagnostics.extend(unused);
        }
        diagnostics.extend(crate_diags);
    }

    for cycle in find_cycles(&ws_names, &ws_edges) {
        let joined = cycle.join(" <-> ");
        diagnostics.push(Diagnostic {
            rule_id: "warden/dag/workspace-cycle".to_string(),
            check: "dag".to_string(),
            severity: Severity::Error,
            message: format!("Workspace crate dependency cycle detected: {joined}"),
            file: Some(PathBuf::from("Cargo.toml")),
            line: None,
            column: None,
            fingerprint: format!("workspace-dag:cycle:{}", cycle.join(",")),
            metric_value: None,
            suppressed: false,
            suppression_reason: None,
        });
    }

    if opts.config.check_version_conflicts {
        collect_version_conflicts(&external_versions, &mut diagnostics);
    }

    Ok(diagnostics)
}

struct EdgeContext<'a> {
    krate: &'a WorkspaceCrate,
    dep: &'a CrateDep,
    dep_line: Option<usize>,
    suppressed: bool,
    target_is_ws: bool,
    from_layer: Option<&'a CompiledLayer<'a>>,
    to_layer: Option<&'a CompiledLayer<'a>>,
    rel_str: &'a str,
}

fn changeset_touches_manifests(changed: &BTreeSet<PathBuf>) -> bool {
    changed.iter().any(|p| {
        matches!(
            p.file_name().and_then(|n| n.to_str()),
            Some("Cargo.toml" | "Cargo.lock")
        )
    })
}

fn should_check_crate(changeset: Option<&BTreeSet<PathBuf>>, rel_manifest: &Path) -> bool {
    let Some(changed) = changeset else {
        return true;
    };
    if changed.contains(Path::new("Cargo.toml")) || changed.contains(Path::new("Cargo.lock")) {
        return true;
    }
    changed.contains(rel_manifest)
}

fn dep_kind_enabled(kind: DepKind, cfg: &WorkspaceDagConfig) -> bool {
    match kind {
        DepKind::Normal => true,
        DepKind::Dev => cfg.include_dev_dependencies,
        DepKind::Build => cfg.include_build_dependencies,
    }
}

fn fetch_cargo_metadata(base_dir: &Path, full_graph: bool) -> Result<serde_json::Value> {
    let mut cmd = Command::new("cargo");
    cmd.arg("metadata");
    if !full_graph {
        cmd.arg("--no-deps");
    }
    cmd.args(["--format-version", "1"]).current_dir(base_dir);
    let output = cmd.output().context("Failed to run `cargo metadata`")?;
    if !output.status.success() {
        bail!(
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).context("Failed to parse `cargo metadata` JSON")
}

type ExternalVersions = BTreeMap<String, BTreeSet<String>>;

fn parse_workspace_metadata(
    base_dir: &Path,
    metadata: &serde_json::Value,
) -> Result<(Vec<WorkspaceCrate>, ExternalVersions)> {
    let ws_ids: BTreeSet<String> = metadata["workspace_members"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let packages = metadata["packages"]
        .as_array()
        .context("`packages` array missing from cargo metadata")?;

    let mut crates = Vec::new();
    let mut external_versions: ExternalVersions = BTreeMap::new();

    for pkg in packages {
        let id = pkg["id"].as_str().unwrap_or_default();
        let name = pkg["name"].as_str().unwrap_or_default().to_string();
        if name.is_empty() {
            continue;
        }
        let is_member = ws_ids.is_empty() || ws_ids.contains(id);
        if !is_member {
            if let Some(ver) = pkg["version"].as_str() {
                external_versions
                    .entry(name)
                    .or_default()
                    .insert(ver.to_string());
            }
            continue;
        }

        let manifest_path = PathBuf::from(pkg["manifest_path"].as_str().unwrap_or("Cargo.toml"));
        let rel_manifest = normalize_rel_path(base_dir, &manifest_path);
        let deps = pkg["dependencies"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_dep_entry).collect())
            .unwrap_or_default();

        crates.push(WorkspaceCrate {
            name,
            manifest_path,
            rel_manifest,
            deps,
        });
    }

    crates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((crates, external_versions))
}

fn parse_dep_entry(val: &serde_json::Value) -> Option<CrateDep> {
    let name = val["name"].as_str()?.to_string();
    let kind = match val["kind"].as_str() {
        Some("dev") => DepKind::Dev,
        Some("build") => DepKind::Build,
        _ => DepKind::Normal,
    };
    let rename = val["rename"].as_str().map(str::to_string);
    Some(CrateDep { name, kind, rename })
}

fn compile_layers(layers: &[WorkspaceLayerConfig]) -> Vec<CompiledLayer<'_>> {
    layers
        .iter()
        .map(|cfg| {
            let patterns = if cfg.crates.is_empty() {
                vec![cfg.name.clone()]
            } else {
                cfg.crates.clone()
            };
            let has_exact = patterns
                .iter()
                .any(|p| !p.contains('*') && !p.contains('?') && !p.contains('['));
            CompiledLayer {
                cfg,
                matcher: PathPatternMatcher::new(&patterns),
                has_exact,
            }
        })
        .collect()
}

fn find_crate_layer<'a>(
    crate_name: &str,
    layers: &'a [CompiledLayer<'a>],
) -> Option<&'a CompiledLayer<'a>> {
    let mut best: Option<(bool, &'a CompiledLayer<'a>)> = None;
    for layer in layers {
        if layer.matcher.matches_name(crate_name)
            && best.is_none_or(|(best_exact, _)| !best_exact && layer.has_exact)
        {
            best = Some((layer.has_exact, layer));
        }
    }
    best.map(|(_, layer)| layer)
}

fn target_matches_spec(dep_name: &str, to_layer: Option<&CompiledLayer<'_>>, spec: &str) -> bool {
    if to_layer.is_some_and(|l| l.cfg.name == spec) {
        return true;
    }
    PathPatternMatcher::new(&[spec.to_string()]).matches_name(dep_name)
}

fn check_forbid_dep(
    ctx: &EdgeContext<'_>,
    cfg: &WorkspaceDagConfig,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Diagnostic>,
) {
    if cfg.forbid_deps.is_empty() {
        return;
    }
    let forbidden_target = cfg
        .forbid_deps
        .iter()
        .any(|pat| PathPatternMatcher::new(std::slice::from_ref(pat)).matches_name(&ctx.dep.name));
    let source_is_forbidden = cfg.forbid_deps.iter().any(|pat| {
        PathPatternMatcher::new(std::slice::from_ref(pat)).matches_name(&ctx.krate.name)
    });
    if !forbidden_target || source_is_forbidden || ctx.dep.name == ctx.krate.name {
        return;
    }

    let key = format!("forbid:{}->{}", ctx.krate.name, ctx.dep.name);
    if seen.insert(key) {
        out.push(Diagnostic {
            rule_id: "warden/dag/workspace-forbid-dep".to_string(),
            check: "dag".to_string(),
            severity: Severity::Error,
            message: format_dep_message(
                &format!(
                    "crate `{}` depends on forbidden crate `{}`",
                    ctx.krate.name, ctx.dep.name
                ),
                &ctx.krate.rel_manifest,
                ctx.dep_line,
            ),
            file: Some(ctx.krate.rel_manifest.clone()),
            line: ctx.dep_line,
            column: None,
            fingerprint: format!("workspace-dag:forbid:{}->{}", ctx.krate.name, ctx.dep.name),
            metric_value: None,
            suppressed: ctx.suppressed,
            suppression_reason: ctx.suppressed.then(|| "inline".to_string()),
        });
    }
}

fn check_layer_dep(
    ctx: &EdgeContext<'_>,
    _layers: &[CompiledLayer<'_>],
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(from_layer) = ctx.from_layer else {
        return;
    };
    let same_layer = ctx
        .to_layer
        .is_some_and(|tl| tl.cfg.name == from_layer.cfg.name);
    let denied =
        from_layer.cfg.deny.iter().any(|d| {
            d != &from_layer.cfg.name && target_matches_spec(&ctx.dep.name, ctx.to_layer, d)
        });
    let not_allowed = ctx.target_is_ws
        && !same_layer
        && ctx.dep.name != ctx.krate.name
        && from_layer.cfg.allow.as_ref().is_some_and(|allowed| {
            !allowed
                .iter()
                .any(|a| target_matches_spec(&ctx.dep.name, ctx.to_layer, a))
        });

    if !denied && !not_allowed {
        return;
    }

    let to_layer_disp = ctx
        .to_layer
        .map_or("external/unlayered", |l| l.cfg.name.as_str());
    let reason = if denied {
        format!("layer `{}` denies `{to_layer_disp}`", from_layer.cfg.name)
    } else {
        format!(
            "layer `{}` does not allow `{to_layer_disp}`",
            from_layer.cfg.name
        )
    };

    let key = format!("layer:{}->{}", ctx.krate.name, ctx.dep.name);
    if seen.insert(key) {
        out.push(Diagnostic {
            rule_id: "warden/dag/workspace-layer-violation".to_string(),
            check: "dag".to_string(),
            severity: Severity::Error,
            message: format_dep_message(
                &format!(
                    "crate `{}` (layer `{}`) -> `{}` ({reason})",
                    ctx.krate.name, from_layer.cfg.name, ctx.dep.name
                ),
                &ctx.krate.rel_manifest,
                ctx.dep_line,
            ),
            file: Some(ctx.krate.rel_manifest.clone()),
            line: ctx.dep_line,
            column: None,
            fingerprint: format!(
                "workspace-dag:layer:{}:{}->{}",
                ctx.rel_str, ctx.krate.name, ctx.dep.name
            ),
            metric_value: None,
            suppressed: ctx.suppressed,
            suppression_reason: ctx.suppressed.then(|| "inline".to_string()),
        });
    }
}

fn check_rule_dep(
    ctx: &EdgeContext<'_>,
    rules: &[WorkspaceDagRuleConfig],
    _layers: &[CompiledLayer<'_>],
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Diagnostic>,
) {
    for rule in rules {
        let matches_from = ctx.from_layer.is_some_and(|l| l.cfg.name == rule.from)
            || PathPatternMatcher::new(std::slice::from_ref(&rule.from))
                .matches_name(&ctx.krate.name);
        if !matches_from {
            continue;
        }
        let same_scope = ctx.to_layer.is_some_and(|l| l.cfg.name == rule.from)
            || PathPatternMatcher::new(std::slice::from_ref(&rule.from))
                .matches_name(&ctx.dep.name);
        let denied = rule
            .deny
            .iter()
            .any(|d| target_matches_spec(&ctx.dep.name, ctx.to_layer, d));
        let not_allowed = ctx.target_is_ws
            && !same_scope
            && ctx.dep.name != ctx.krate.name
            && rule.allow_only.as_ref().is_some_and(|allowed| {
                !allowed
                    .iter()
                    .any(|a| target_matches_spec(&ctx.dep.name, ctx.to_layer, a))
            });

        if !denied && !not_allowed {
            continue;
        }

        let key = format!("rule:{}:{}->{}", rule.from, ctx.krate.name, ctx.dep.name);
        if seen.insert(key) {
            out.push(Diagnostic {
                rule_id: "warden/dag/workspace-layer-violation".to_string(),
                check: "dag".to_string(),
                severity: Severity::Error,
                message: format_dep_message(
                    &format!(
                        "crate `{}` -> `{}` violates workspace DAG rule for `{}`",
                        ctx.krate.name, ctx.dep.name, rule.from
                    ),
                    &ctx.krate.rel_manifest,
                    ctx.dep_line,
                ),
                file: Some(ctx.krate.rel_manifest.clone()),
                line: ctx.dep_line,
                column: None,
                fingerprint: format!(
                    "workspace-dag:rule:{}:{}->{}",
                    rule.from, ctx.krate.name, ctx.dep.name
                ),
                metric_value: None,
                suppressed: ctx.suppressed,
                suppression_reason: ctx.suppressed.then(|| "inline".to_string()),
            });
        }
    }
}

fn collect_version_conflicts(external_versions: &ExternalVersions, out: &mut Vec<Diagnostic>) {
    for (name, versions) in external_versions {
        if versions.len() <= 1 {
            continue;
        }
        let joined: Vec<&str> = versions.iter().map(String::as_str).collect();
        out.push(Diagnostic {
            rule_id: "warden/dag/workspace-version-conflict".to_string(),
            check: "dag".to_string(),
            severity: Severity::Warning,
            message: format!(
                "external dependency `{name}` resolved with multiple versions: {}",
                joined.join(", ")
            ),
            file: Some(PathBuf::from("Cargo.lock")),
            line: None,
            column: None,
            fingerprint: format!("workspace-dag:version-conflict:{name}"),
            metric_value: Some(versions.len()),
            suppressed: false,
            suppression_reason: None,
        });
    }
}

fn format_dep_message(body: &str, manifest: &Path, line: Option<usize>) -> String {
    match line {
        Some(l) => format!("{body} at {}:{}", manifest.display(), l),
        None => format!("{body} in {}", manifest.display()),
    }
}

fn find_dep_line(manifest_text: &str, dep: &CrateDep) -> Option<usize> {
    let key_name = dep.rename.as_deref().unwrap_or(&dep.name);
    let mut in_dep_section = false;

    for (idx, line) in manifest_text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') {
            if is_subtable_for_dep(trimmed, key_name) {
                return Some(idx + 1);
            }
            in_dep_section = is_dep_section_header(trimmed);
            continue;
        }
        if !in_dep_section {
            continue;
        }
        if line_declares_dep(trimmed, key_name, &dep.name) {
            return Some(idx + 1);
        }
    }
    None
}

fn is_dep_section_header(header: &str) -> bool {
    let inner = header.trim_start_matches('[').trim_end_matches(']').trim();
    inner == "dependencies"
        || inner == "dev-dependencies"
        || inner == "build-dependencies"
        || inner.ends_with(".dependencies")
        || inner.ends_with(".dev-dependencies")
        || inner.ends_with(".build-dependencies")
}

fn is_subtable_for_dep(header: &str, dep_key: &str) -> bool {
    let inner = header.trim_start_matches('[').trim_end_matches(']').trim();
    ["dependencies.", "dev-dependencies.", "build-dependencies."]
        .iter()
        .any(|prefix| {
            inner
                .strip_prefix(prefix)
                .is_some_and(|rest| rest == dep_key || rest.trim_matches('"') == dep_key)
        })
}

fn line_declares_dep(trimmed: &str, key_name: &str, actual_name: &str) -> bool {
    let Some((lhs, rhs)) = trimmed.split_once('=') else {
        return false;
    };
    let lhs_key = lhs.trim().trim_matches('"');
    if lhs_key == key_name || lhs_key.strip_suffix(".workspace") == Some(key_name) {
        return true;
    }
    rhs.contains(&format!("package = \"{actual_name}\""))
        || rhs.contains(&format!("package = '{actual_name}'"))
}

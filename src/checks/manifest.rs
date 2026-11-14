//! Cargo workspace manifest consistency guard (`cargo warden manifest`).
//!
//! Verifies workspace member `Cargo.toml` files against `[workspace.package]` and
//! `[workspace.dependencies]` inheritance rules, dependency source policies (`git` / external `path`),
//! and optional `#[cfg(feature = "...")]` feature declarations.

use crate::config::ManifestConfig;
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use crate::suppression::{FileSuppressions, collect_unused_suppression_diagnostics};
use crate::walker::{WalkOptions, collect_files};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, ExprMacro, Meta, StmtMacro};
use toml::Value;

const OWNED_MANIFEST_RULES: &[&str] = &["manifest"];

#[derive(Debug, Clone)]
pub struct ManifestOptions<'a> {
    pub base_dir: &'a Path,
    pub config: &'a ManifestConfig,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

#[derive(Debug, Clone)]
struct MemberCrate {
    name: String,
    manifest_path: PathBuf,
    rel_manifest: PathBuf,
    crate_dir: PathBuf,
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(comp);
                }
            }
            _ => out.push(comp),
        }
    }
    out
}

fn fetch_workspace_members(base_dir: &Path) -> Result<(PathBuf, Vec<MemberCrate>)> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(base_dir)
        .output()
        .context("Failed to execute `cargo metadata`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`cargo metadata` failed: {}", stderr.trim());
    }

    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Failed to parse `cargo metadata` JSON")?;

    let ws_root = meta["workspace_root"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| base_dir.to_path_buf());

    let member_ids: BTreeSet<String> = meta["workspace_members"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut members = Vec::new();
    if let Some(pkgs) = meta["packages"].as_array() {
        for pkg in pkgs {
            let Some(id) = pkg["id"].as_str() else {
                continue;
            };
            if !member_ids.contains(id) {
                continue;
            }
            let Some(name) = pkg["name"].as_str() else {
                continue;
            };
            let Some(manifest_str) = pkg["manifest_path"].as_str() else {
                continue;
            };
            let manifest_path = PathBuf::from(manifest_str);
            let rel_manifest = normalize_rel_path(base_dir, &manifest_path);
            let crate_dir = manifest_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| base_dir.to_path_buf());
            members.push(MemberCrate {
                name: name.to_string(),
                manifest_path,
                rel_manifest,
                crate_dir,
            });
        }
    }
    members.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((ws_root, members))
}

fn changeset_touches_manifests_or_rust(changed: &BTreeSet<PathBuf>, check_features: bool) -> bool {
    changed.iter().any(|p| {
        let name = p.file_name().and_then(|n| n.to_str());
        if matches!(name, Some("Cargo.toml" | "Cargo.lock")) {
            return true;
        }
        check_features && p.extension().and_then(|e| e.to_str()) == Some("rs")
    })
}

fn should_check_member(
    changeset: Option<&BTreeSet<PathBuf>>,
    rel_manifest: &Path,
    crate_rel_dir: &Path,
    check_features: bool,
) -> bool {
    let Some(changed) = changeset else {
        return true;
    };
    if changed.contains(Path::new("Cargo.toml")) || changed.contains(Path::new("Cargo.lock")) {
        return true;
    }
    if changed.contains(rel_manifest) {
        return true;
    }
    if check_features {
        let prefix = crate_rel_dir.to_string_lossy().replace('\\', "/");
        return changed.iter().any(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("rs")
                && (prefix.is_empty()
                    || prefix == "."
                    || p.to_string_lossy()
                        .replace('\\', "/")
                        .starts_with(&format!("{prefix}/")))
        });
    }
    false
}

fn find_key_line(manifest_text: &str, section_prefix: &str, key: &str) -> Option<usize> {
    let mut in_section = false;
    for (idx, line) in manifest_text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            in_section = header == section_prefix
                || (section_prefix == "dependencies"
                    && (header == "dev-dependencies"
                        || header == "build-dependencies"
                        || header.ends_with(".dependencies")
                        || header.ends_with(".dev-dependencies")
                        || header.ends_with(".build-dependencies")));
            continue;
        }
        if in_section
            && let Some((lhs, _)) = trimmed.split_once('=')
            && lhs.trim().trim_matches('"') == key
        {
            return Some(idx + 1);
        }
    }
    None
}

fn is_workspace_true(val: &Value) -> bool {
    val.as_table()
        .and_then(|t| t.get("workspace"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn collect_dep_tables(root: &Value) -> Vec<(&'static str, &toml::map::Map<String, Value>)> {
    let mut tables = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(tbl) = root.get(section).and_then(Value::as_table) {
            tables.push((section, tbl));
        }
    }
    if let Some(targets) = root.get("target").and_then(Value::as_table) {
        for target_val in targets.values() {
            for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(tbl) = target_val.get(section).and_then(Value::as_table) {
                    tables.push((section, tbl));
                }
            }
        }
    }
    tables
}

fn check_package_inheritance(
    member: &MemberCrate,
    doc: &Value,
    manifest_text: &str,
    suppressions: &FileSuppressions,
    ws_pkg_keys: &BTreeSet<String>,
    inherit_fields: &[String],
    out: &mut Vec<Diagnostic>,
) {
    let Some(pkg) = doc.get("package").and_then(Value::as_table) else {
        return;
    };
    let rel_str = member.rel_manifest.to_string_lossy().replace('\\', "/");

    for field in inherit_fields {
        if !ws_pkg_keys.contains(field) {
            continue;
        }
        let line = find_key_line(manifest_text, "package", field);
        let ok = pkg.get(field).is_some_and(is_workspace_true);
        if ok {
            continue;
        }
        let suppressed = suppressions.is_suppressed("manifest", line);
        let reason = if pkg.contains_key(field) {
            format!("overrides `{field}` instead of `{field}.workspace = true`")
        } else {
            format!("is missing `{field}.workspace = true` (defined in `[workspace.package]`)")
        };
        out.push(Diagnostic {
            rule_id: "warden/manifest/workspace-inherit".to_string(),
            check: "manifest".to_string(),
            severity: Severity::Error,
            message: format!(
                "{}: crate `{}` {reason}",
                member.rel_manifest.display(),
                member.name
            ),
            file: Some(member.rel_manifest.clone()),
            line,
            column: None,
            fingerprint: format!("manifest:inherit:{rel_str}:{field}"),
            metric_value: None,
            suppressed,
            suppression_reason: suppressed.then(|| "inline".to_string()),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn check_dependencies(
    member: &MemberCrate,
    doc: &Value,
    manifest_text: &str,
    suppressions: &FileSuppressions,
    ws_root_norm: &Path,
    ws_dep_keys: &BTreeSet<String>,
    cfg: &ManifestConfig,
    out: &mut Vec<Diagnostic>,
) {
    let rel_str = member.rel_manifest.to_string_lossy().replace('\\', "/");

    for (section, tbl) in collect_dep_tables(doc) {
        for (dep_key, dep_val) in tbl {
            let line = find_key_line(manifest_text, "dependencies", dep_key);
            let suppressed = suppressions.is_suppressed("manifest", line);
            let effective_name = dep_val
                .as_table()
                .and_then(|t| t.get("package"))
                .and_then(Value::as_str)
                .unwrap_or(dep_key.as_str());

            if cfg.enforce_workspace_deps
                && (ws_dep_keys.contains(dep_key) || ws_dep_keys.contains(effective_name))
                && !is_workspace_true(dep_val)
            {
                out.push(Diagnostic {
                    rule_id: "warden/manifest/workspace-dep".to_string(),
                    check: "manifest".to_string(),
                    severity: Severity::Error,
                    message: format!(
                        "{}: `{}` in [{section}] should inherit from `[workspace.dependencies]` (`{dep_key}.workspace = true`)",
                        member.rel_manifest.display(),
                        dep_key
                    ),
                    file: Some(member.rel_manifest.clone()),
                    line,
                    column: None,
                    fingerprint: format!("manifest:ws-dep:{rel_str}:{section}:{dep_key}"),
                    metric_value: None,
                    suppressed,
                    suppression_reason: suppressed.then(|| "inline".to_string()),
                });
            }

            let Some(dep_tbl) = dep_val.as_table() else {
                continue;
            };

            if cfg.forbid_git_deps
                && dep_tbl.contains_key("git")
                && !cfg
                    .allow_git_deps
                    .iter()
                    .any(|a| a == dep_key || a == effective_name)
            {
                out.push(Diagnostic {
                    rule_id: "warden/manifest/git-dep".to_string(),
                    check: "manifest".to_string(),
                    severity: Severity::Error,
                    message: format!(
                        "{}: forbidden `git` dependency `{dep_key}` in [{section}]",
                        member.rel_manifest.display()
                    ),
                    file: Some(member.rel_manifest.clone()),
                    line,
                    column: None,
                    fingerprint: format!("manifest:git-dep:{rel_str}:{section}:{dep_key}"),
                    metric_value: None,
                    suppressed,
                    suppression_reason: suppressed.then(|| "inline".to_string()),
                });
            }

            if cfg.forbid_external_path_deps
                && let Some(path_str) = dep_tbl.get("path").and_then(Value::as_str)
            {
                let resolved = normalize_lexical(&member.crate_dir.join(path_str));
                if !resolved.starts_with(ws_root_norm) {
                    out.push(Diagnostic {
                        rule_id: "warden/manifest/external-path-dep".to_string(),
                        check: "manifest".to_string(),
                        severity: Severity::Error,
                        message: format!(
                            "{}: dependency `{dep_key}` points outside workspace root (`{path_str}`)",
                            member.rel_manifest.display()
                        ),
                        file: Some(member.rel_manifest.clone()),
                        line,
                        column: None,
                        fingerprint: format!("manifest:ext-path:{rel_str}:{section}:{dep_key}"),
                        metric_value: None,
                        suppressed,
                        suppression_reason: suppressed.then(|| "inline".to_string()),
                    });
                }
            }
        }
    }
}

struct FeatureRefVisitor {
    refs: Vec<(String, usize)>,
}

impl FeatureRefVisitor {
    fn collect_from_meta(&mut self, meta: &Meta) {
        match meta {
            Meta::NameValue(nv) if nv.path.is_ident("feature") => {
                if let syn::Expr::Lit(expr_lit) = &nv.value
                    && let syn::Lit::Str(lit) = &expr_lit.lit
                {
                    self.refs.push((lit.value(), lit.span().start().line));
                }
            }
            Meta::List(list) => {
                let _ = list.parse_nested_meta(|nested| {
                    if nested.path.is_ident("feature")
                        && let Ok(val) = nested.value()
                        && let Ok(lit) = val.parse::<syn::LitStr>()
                    {
                        self.refs.push((lit.value(), lit.span().start().line));
                    }
                    Ok(())
                });
            }
            _ => {}
        }
    }
}

impl<'ast> Visit<'ast> for FeatureRefVisitor {
    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr") {
            let _ = attr.parse_nested_meta(|nested| {
                if nested.path.is_ident("feature")
                    && let Ok(val) = nested.value()
                    && let Ok(lit) = val.parse::<syn::LitStr>()
                {
                    self.refs.push((lit.value(), lit.span().start().line));
                } else {
                    // Handle nested `all(feature = "...")` / `any(...)` / `not(...)`
                    let _ = nested.parse_nested_meta(|inner| {
                        if inner.path.is_ident("feature")
                            && let Ok(val) = inner.value()
                            && let Ok(lit) = val.parse::<syn::LitStr>()
                        {
                            self.refs.push((lit.value(), lit.span().start().line));
                        }
                        Ok(())
                    });
                }
                Ok(())
            });
            self.collect_from_meta(&attr.meta);
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_expr_macro(&mut self, node: &'ast ExprMacro) {
        if node.mac.path.is_ident("cfg") {
            let tokens = node.mac.tokens.to_string();
            extract_cfg_macro_features(&tokens, node.mac.span().start().line, &mut self.refs);
        }
        visit::visit_expr_macro(self, node);
    }

    fn visit_stmt_macro(&mut self, node: &'ast StmtMacro) {
        if node.mac.path.is_ident("cfg") {
            let tokens = node.mac.tokens.to_string();
            extract_cfg_macro_features(&tokens, node.mac.span().start().line, &mut self.refs);
        }
        visit::visit_stmt_macro(self, node);
    }
}

fn extract_cfg_macro_features(tokens: &str, line: usize, out: &mut Vec<(String, usize)>) {
    let mut rest = tokens;
    while let Some(idx) = rest.find("feature") {
        let after = rest[idx + "feature".len()..].trim_start();
        if let Some(after_eq) = after.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            if let Some(quoted) = after_eq.strip_prefix('"')
                && let Some(end_q) = quoted.find('"')
            {
                out.push((quoted[..end_q].to_string(), line));
                rest = &quoted[end_q + 1..];
                continue;
            }
        }
        rest = &rest[idx + "feature".len()..];
    }
}

fn check_undeclared_features(
    base_dir: &Path,
    member: &MemberCrate,
    doc: &Value,
    out: &mut Vec<Diagnostic>,
) {
    let declared: BTreeSet<String> = doc
        .get("features")
        .and_then(Value::as_table)
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();

    let rs_exts = vec!["rs".to_string()];
    let ignore = vec!["target".to_string()];
    let files = collect_files(
        std::slice::from_ref(&member.crate_dir),
        &WalkOptions {
            extensions: &rs_exts,
            ignore_patterns: &ignore,
            respect_gitignore: true,
            base_dir: Some(base_dir),
            changeset: None,
        },
    );

    let mut seen: BTreeMap<(PathBuf, String), usize> = BTreeMap::new();
    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        let Ok(syntax) = syn::parse_file(&content) else {
            continue;
        };
        let suppressions = FileSuppressions::parse(&content);
        let mut visitor = FeatureRefVisitor { refs: Vec::new() };
        visitor.visit_file(&syntax);
        let rel = normalize_rel_path(base_dir, &file);
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        for (feat, line) in visitor.refs {
            if declared.contains(&feat) {
                continue;
            }
            if seen.contains_key(&(rel.clone(), feat.clone())) {
                continue;
            }
            seen.insert((rel.clone(), feat.clone()), line);
            let suppressed = suppressions.is_suppressed("manifest", Some(line));
            out.push(Diagnostic {
                rule_id: "warden/manifest/undeclared-feature".to_string(),
                check: "manifest".to_string(),
                severity: Severity::Error,
                message: format!(
                    "{}:{} - feature `{feat}` is not declared in `{}` `[features]`",
                    rel.display(),
                    line,
                    member.rel_manifest.display()
                ),
                file: Some(rel.clone()),
                line: Some(line),
                column: None,
                fingerprint: format!("manifest:feature:{rel_str}:{feat}"),
                metric_value: None,
                suppressed,
                suppression_reason: suppressed.then(|| "inline".to_string()),
            });
        }
    }
}

/// Collect workspace manifest consistency diagnostics.
pub fn collect_manifest(opts: &ManifestOptions<'_>) -> Result<Vec<Diagnostic>> {
    if let Some(changed) = opts.changeset
        && !changeset_touches_manifests_or_rust(changed, opts.config.check_features)
    {
        tracing::info!(
            "skipping manifest check (no changed Cargo.toml, Cargo.lock, or Rust files)"
        );
        return Ok(Vec::new());
    }

    let (ws_root, members) = fetch_workspace_members(opts.base_dir)?;
    let ws_root_norm = normalize_lexical(&ws_root);
    let root_manifest_path = ws_root.join("Cargo.toml");
    let root_text = fs::read_to_string(&root_manifest_path)
        .with_context(|| format!("Failed to read {}", root_manifest_path.display()))?;
    let root_doc: Value = toml::from_str(&root_text)
        .with_context(|| format!("Failed to parse {}", root_manifest_path.display()))?;

    let ws_pkg_keys: BTreeSet<String> = root_doc
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(Value::as_table)
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();

    let ws_dep_keys: BTreeSet<String> = root_doc
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(Value::as_table)
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();

    if opts.emit_text {
        println!(
            "[manifest] checking {} workspace member manifest(s)...",
            members.len()
        );
    }

    let mut diagnostics = Vec::new();
    for member in &members {
        if opts.config.ignore_crates.iter().any(|c| c == &member.name) {
            continue;
        }
        let crate_rel_dir = normalize_rel_path(opts.base_dir, &member.crate_dir);
        if !should_check_member(
            opts.changeset,
            &member.rel_manifest,
            &crate_rel_dir,
            opts.config.check_features,
        ) {
            continue;
        }
        let manifest_text = match fs::read_to_string(&member.manifest_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let doc: Value = match toml::from_str(&manifest_text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let suppressions = FileSuppressions::parse(&manifest_text);
        let mut member_diags = Vec::new();

        check_package_inheritance(
            member,
            &doc,
            &manifest_text,
            &suppressions,
            &ws_pkg_keys,
            &opts.config.inherit_fields,
            &mut member_diags,
        );
        check_dependencies(
            member,
            &doc,
            &manifest_text,
            &suppressions,
            &ws_root_norm,
            &ws_dep_keys,
            opts.config,
            &mut member_diags,
        );
        if opts.config.check_features {
            check_undeclared_features(opts.base_dir, member, &doc, &mut member_diags);
        }

        let unused = collect_unused_suppression_diagnostics(
            &member.rel_manifest,
            &suppressions,
            OWNED_MANIFEST_RULES,
            &member_diags,
        );
        diagnostics.extend(member_diags);
        diagnostics.extend(unused);
    }

    Ok(diagnostics)
}

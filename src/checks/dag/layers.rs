use super::{DagOptions, DetailedEdge, top_level};
use crate::config::LayerConfig;
use crate::report::{Diagnostic, Severity};
use std::collections::BTreeSet;
use std::path::Path;

fn module_matches_pattern(module_path: &[String], pattern: &str) -> bool {
    let segs: Vec<&str> = pattern.split("::").filter(|s| !s.is_empty()).collect();
    if segs.is_empty() || module_path.len() < segs.len() {
        return false;
    }
    module_path[..segs.len()]
        .iter()
        .zip(segs.iter())
        .all(|(a, b)| a == b)
}

fn find_layer<'a>(module_path: &[String], layers: &'a [LayerConfig]) -> Option<&'a LayerConfig> {
    let mut best: Option<(usize, &'a LayerConfig)> = None;
    for layer in layers {
        let patterns: Vec<&str> = if layer.modules.is_empty() {
            vec![layer.name.as_str()]
        } else {
            layer.modules.iter().map(String::as_str).collect()
        };
        for pat in patterns {
            if module_matches_pattern(module_path, pat) {
                let specificity = pat.split("::").count();
                if best.is_none_or(|(best_spec, _)| specificity > best_spec) {
                    best = Some((specificity, layer));
                }
            }
        }
    }
    best.map(|(_, l)| l)
}

fn target_matches_spec(to_mod: &[String], to_layer: Option<&LayerConfig>, spec: &str) -> bool {
    to_layer.is_some_and(|l| l.name == spec) || module_matches_pattern(to_mod, spec)
}

fn format_mod_path(parts: &[String]) -> String {
    parts.join("::")
}

pub(super) fn check_edge_forbid(
    edge: &DetailedEdge,
    opts: &DagOptions<'_>,
    root_key: &str,
    rel_file: &Path,
    rel_file_str: &str,
    seen: &mut BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(from_top) = top_level(&edge.from_mod) else {
        return;
    };
    let Some(to_top) = top_level(&edge.to_mod) else {
        return;
    };
    if to_top == from_top
        || !opts.forbid_imports.iter().any(|f| f == to_top)
        || opts.forbid_imports.iter().any(|f| f == from_top)
    {
        return;
    }

    let key = format!("forbid:{from_top}->{to_top}:{rel_file_str}:{}", edge.line);
    if seen.insert(key) {
        diagnostics.push(Diagnostic {
            rule_id: "warden/dag/forbid-import".to_string(),
            check: "dag".to_string(),
            severity: Severity::Error,
            message: format!(
                "{from_top} -> {to_top} (forbidden import at {}:{})",
                rel_file.display(),
                edge.line
            ),
            file: Some(rel_file.to_path_buf()),
            line: Some(edge.line),
            column: None,
            fingerprint: format!("dag:forbid:{root_key}:{from_top}->{to_top}"),
            metric_value: None,
            suppressed: edge.suppressed,
            suppression_reason: edge.suppressed.then(|| "inline".to_string()),
        });
    }
}

pub(super) fn check_edge_layers(
    edge: &DetailedEdge,
    opts: &DagOptions<'_>,
    root_key: &str,
    rel_file: &Path,
    rel_file_str: &str,
    seen: &mut BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(fl) = find_layer(&edge.from_mod, opts.layers) else {
        return;
    };
    let to_layer = find_layer(&edge.to_mod, opts.layers);
    let same_layer = to_layer.is_some_and(|tl| tl.name == fl.name);
    let explicitly_denied = fl
        .deny
        .iter()
        .any(|d| d != &fl.name && target_matches_spec(&edge.to_mod, to_layer, d));
    let not_in_allowlist = !same_layer
        && fl.allow.as_ref().is_some_and(|allowed| {
            !allowed
                .iter()
                .any(|a| target_matches_spec(&edge.to_mod, to_layer, a))
        });

    if !explicitly_denied && !not_in_allowlist {
        return;
    }

    let from_disp = format_mod_path(&edge.from_mod);
    let to_disp = format_mod_path(&edge.to_mod);
    let to_layer_disp = to_layer.map_or("unlayered", |l| l.name.as_str());
    let reason = if explicitly_denied {
        format!("layer `{}` denies `{to_layer_disp}`", fl.name)
    } else {
        format!("layer `{}` does not allow `{to_layer_disp}`", fl.name)
    };
    let key = format!("layer:{from_disp}->{to_disp}:{rel_file_str}:{}", edge.line);
    if seen.insert(key) {
        diagnostics.push(Diagnostic {
            rule_id: "warden/dag/layer-violation".to_string(),
            check: "dag".to_string(),
            severity: Severity::Error,
            message: format!(
                "{from_disp} (layer `{}`) -> {to_disp} ({reason}) at {}:{}",
                fl.name,
                rel_file.display(),
                edge.line
            ),
            file: Some(rel_file.to_path_buf()),
            line: Some(edge.line),
            column: None,
            fingerprint: format!("dag:layer:{root_key}:{}->{to_disp}", fl.name),
            metric_value: None,
            suppressed: edge.suppressed,
            suppression_reason: edge.suppressed.then(|| "inline".to_string()),
        });
    }
}

pub(super) fn check_edge_rules(
    edge: &DetailedEdge,
    opts: &DagOptions<'_>,
    root_key: &str,
    rel_file: &Path,
    rel_file_str: &str,
    seen: &mut BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let from_layer = find_layer(&edge.from_mod, opts.layers);
    let to_layer = find_layer(&edge.to_mod, opts.layers);

    for rule in opts.rules {
        let matches_from = from_layer.is_some_and(|l| l.name == rule.from)
            || module_matches_pattern(&edge.from_mod, &rule.from);
        if !matches_from {
            continue;
        }
        let same_scope = to_layer.is_some_and(|l| l.name == rule.from)
            || module_matches_pattern(&edge.to_mod, &rule.from);
        let denied = rule
            .deny
            .iter()
            .any(|d| target_matches_spec(&edge.to_mod, to_layer, d));
        let not_allowed = !same_scope
            && rule.allow_only.as_ref().is_some_and(|allowed| {
                !allowed
                    .iter()
                    .any(|a| target_matches_spec(&edge.to_mod, to_layer, a))
            });

        if !denied && !not_allowed {
            continue;
        }

        let from_disp = format_mod_path(&edge.from_mod);
        let to_disp = format_mod_path(&edge.to_mod);
        let key = format!(
            "rule:{}:{from_disp}->{to_disp}:{rel_file_str}:{}",
            rule.from, edge.line
        );
        if seen.insert(key) {
            diagnostics.push(Diagnostic {
                rule_id: "warden/dag/layer-violation".to_string(),
                check: "dag".to_string(),
                severity: Severity::Error,
                message: format!(
                    "{from_disp} -> {to_disp} violates DAG rule for `{}` at {}:{}",
                    rule.from,
                    rel_file.display(),
                    edge.line
                ),
                file: Some(rel_file.to_path_buf()),
                line: Some(edge.line),
                column: None,
                fingerprint: format!("dag:rule:{root_key}:{}->{to_disp}", rule.from),
                metric_value: None,
                suppressed: edge.suppressed,
                suppression_reason: edge.suppressed.then(|| "inline".to_string()),
            });
        }
    }
}

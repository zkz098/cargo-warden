//! Module-boundary and dependency DAG checker.
//!
//! Validates module dependencies inside a crate source root:
//! 1. Builds the module dependency graph at top-level module granularity using `syn` AST.
//! 2. Detects module dependency cycles using Tarjan's SCC algorithm.
//! 3. Enforces forbidden import targets and layered architecture boundary rules.

mod layers;

use super::parallel;
use crate::config::{DagRuleConfig, LayerConfig};
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use crate::suppression::FileSuppressions;
use anyhow::{Result, bail};
use layers::{check_edge_forbid, check_edge_layers, check_edge_rules};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{ItemUse, Path as SynPath, UseTree};

/// One source file together with the module path it defines.
#[derive(Debug)]
pub(crate) struct SourceFile {
    pub(crate) module: Vec<String>,
    pub(crate) path: PathBuf,
}

fn collect_source_files(dir: &Path, module: &[String], out: &mut Vec<SourceFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "tests" || name == "target" || name.starts_with('.') {
                continue;
            }
            let mut child = module.to_vec();
            child.push(name);
            collect_source_files(&path, &child, out);
            continue;
        }
        if path.extension().and_then(OsStr::to_str) != Some("rs") {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if is_test_file(&file_name) {
            continue;
        }
        let stem = file_name.trim_end_matches(".rs");
        let mut child = module.to_vec();
        if stem != "mod" && stem != "lib" && stem != "main" {
            child.push(stem.to_string());
        }
        out.push(SourceFile {
            module: child,
            path,
        });
    }
}

fn is_test_file(file_name: &str) -> bool {
    file_name == "tests.rs" || file_name.ends_with("_tests.rs") || file_name.ends_with("_test.rs")
}

fn ident_name(ident: &syn::Ident) -> String {
    let s = ident.to_string();
    if let Some(stripped) = s.strip_prefix("r#") {
        stripped.to_string()
    } else {
        s
    }
}

fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("test") {
            return true;
        }
        if attr.path().is_ident("cfg") {
            let mut is_test = false;
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("test") {
                    is_test = true;
                }
                Ok(())
            });
            if is_test {
                return true;
            }
        }
        false
    })
}

fn is_test_item(item: &syn::Item) -> bool {
    let attrs = match item {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::ExternCrate(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::ForeignMod(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::TraitAlias(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => return false,
    };
    has_test_attr(attrs)
}

fn expand_syn_use_tree(tree: &UseTree, base: &[String], out: &mut Vec<Vec<String>>) {
    match tree {
        UseTree::Path(p) => {
            let mut next = base.to_vec();
            next.push(ident_name(&p.ident));
            expand_syn_use_tree(&p.tree, &next, out);
        }
        UseTree::Name(n) => {
            let ident_str = ident_name(&n.ident);
            if ident_str == "self" && !base.is_empty() {
                out.push(base.to_vec());
            } else {
                let mut full = base.to_vec();
                full.push(ident_str);
                out.push(full);
            }
        }
        UseTree::Rename(r) => {
            let ident_str = ident_name(&r.ident);
            if ident_str == "self" && !base.is_empty() {
                out.push(base.to_vec());
            } else {
                let mut full = base.to_vec();
                full.push(ident_str);
                out.push(full);
            }
        }
        UseTree::Glob(_) => {
            if !base.is_empty() {
                out.push(base.to_vec());
            }
        }
        UseTree::Group(g) => {
            for item in &g.items {
                expand_syn_use_tree(item, base, out);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ImportRef {
    tokens: Vec<String>,
    line: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedDagFile {
    pub(crate) is_test_crate: bool,
    pub(crate) imports: Vec<ImportRef>,
}

pub(crate) fn extract_dag_imports(syntax: &syn::File) -> ParsedDagFile {
    if has_test_attr(&syntax.attrs) {
        return ParsedDagFile {
            is_test_crate: true,
            imports: Vec::new(),
        };
    }
    let mut visitor = ImportVisitor::default();
    visitor.visit_file(syntax);
    ParsedDagFile {
        is_test_crate: false,
        imports: visitor.paths,
    }
}

#[derive(Default)]
struct ImportVisitor {
    paths: Vec<ImportRef>,
}

impl<'ast> Visit<'ast> for ImportVisitor {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if is_test_item(item) {
            return;
        }
        visit::visit_item(self, item);
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        let line = item.use_token.span.start().line;
        let mut expanded = Vec::new();
        expand_syn_use_tree(&item.tree, &[], &mut expanded);
        for tokens in expanded {
            self.paths.push(ImportRef { tokens, line });
        }
        visit::visit_item_use(self, item);
    }

    fn visit_path(&mut self, path: &'ast SynPath) {
        if let Some(first) = path.segments.first() {
            let ident_str = ident_name(&first.ident);
            if ident_str == "crate" || ident_str == "super" || ident_str == "self" {
                let tokens: Vec<String> =
                    path.segments.iter().map(|s| ident_name(&s.ident)).collect();
                let line = first.ident.span().start().line;
                self.paths.push(ImportRef { tokens, line });
            }
        }
        visit::visit_path(self, path);
    }
}

fn resolve_tokens(tokens: &[String], current: &[String]) -> Option<Vec<String>> {
    let first = tokens.first()?;
    match first.as_str() {
        "crate" => Some(tokens[1..].to_vec()),
        "self" => {
            let mut out = current.to_vec();
            out.extend_from_slice(&tokens[1..]);
            Some(out)
        }
        "super" => {
            let mut ups = 0;
            let mut rest = tokens;
            while rest.first().map(String::as_str) == Some("super") {
                ups += 1;
                rest = &rest[1..];
            }
            let mut base = current.to_vec();
            for _ in 0..ups {
                base.pop();
            }
            base.extend_from_slice(rest);
            Some(base)
        }
        _ => None,
    }
}

pub(super) fn top_level(module: &[String]) -> Option<&str> {
    module.first().map(String::as_str)
}

pub fn module_graph(src_root: &Path) -> (BTreeSet<String>, BTreeMap<String, BTreeSet<String>>) {
    let (nodes, edges, _) = collect_detailed_edges(src_root);
    (nodes, edges)
}

pub fn find_cycles(
    nodes: &BTreeSet<String>,
    edges: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<Vec<String>> {
    struct State<'a> {
        edges: &'a BTreeMap<String, BTreeSet<String>>,
        index: BTreeMap<String, usize>,
        low: BTreeMap<String, usize>,
        on_stack: BTreeSet<String>,
        stack: Vec<String>,
        next: usize,
        found: Vec<Vec<String>>,
    }

    fn visit(state: &mut State<'_>, v: &str) {
        state.index.insert(v.to_string(), state.next);
        state.low.insert(v.to_string(), state.next);
        state.next += 1;
        state.stack.push(v.to_string());
        state.on_stack.insert(v.to_string());

        if let Some(targets) = state.edges.get(v) {
            for w in targets {
                if !state.index.contains_key(w) {
                    visit(state, w);
                    let low_w = state.low[w];
                    let low_v = state.low[v];
                    state.low.insert(v.to_string(), low_v.min(low_w));
                } else if state.on_stack.contains(w) {
                    let idx_w = state.index[w];
                    let low_v = state.low[v];
                    state.low.insert(v.to_string(), low_v.min(idx_w));
                }
            }
        }

        if state.low[v] == state.index[v] {
            let mut component = Vec::new();
            while let Some(w) = state.stack.pop() {
                state.on_stack.remove(&w);
                let done = w == v;
                component.push(w);
                if done {
                    break;
                }
            }
            if component.len() > 1 {
                component.sort();
                state.found.push(component);
            }
        }
    }

    let mut state = State {
        edges,
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        on_stack: BTreeSet::new(),
        stack: Vec::new(),
        next: 0,
        found: Vec::new(),
    };
    for node in nodes {
        if !state.index.contains_key(node) {
            visit(&mut state, node);
        }
    }
    state.found.sort();
    state.found
}

/// Options for running the DAG and layered architecture check.
#[derive(Debug, Clone)]
pub struct DagOptions<'a> {
    pub base_dir: &'a Path,
    pub forbid_imports: &'a [String],
    pub layers: &'a [LayerConfig],
    pub rules: &'a [DagRuleConfig],
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct DetailedEdge {
    pub(super) from_mod: Vec<String>,
    pub(super) to_mod: Vec<String>,
    pub(super) file: PathBuf,
    pub(super) line: usize,
    pub(super) suppressed: bool,
}

pub(crate) struct DagFileScan {
    pub(crate) files: Vec<SourceFile>,
    pub(crate) nodes: BTreeSet<String>,
}

pub(crate) fn scan_dag_files(src_root: &Path) -> DagFileScan {
    let mut files = Vec::new();
    collect_source_files(src_root, &[], &mut files);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let nodes: BTreeSet<String> = files
        .iter()
        .filter_map(|file| top_level(&file.module).map(str::to_string))
        .collect();
    DagFileScan { files, nodes }
}

pub(crate) fn assemble_dag_graph<'a>(
    scan: &DagFileScan,
    parsed: &[Option<(&'a ParsedDagFile, &'a FileSuppressions)>],
) -> (BTreeMap<String, BTreeSet<String>>, Vec<DetailedEdge>) {
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut detailed = Vec::new();

    for (file, maybe_parsed) in scan.files.iter().zip(parsed.iter()) {
        let Some(from_top) = top_level(&file.module) else {
            continue;
        };
        let Some((dag_file, suppressions)) = maybe_parsed else {
            continue;
        };
        if dag_file.is_test_crate {
            continue;
        }

        for import_ref in &dag_file.imports {
            let Some(resolved) = resolve_tokens(&import_ref.tokens, &file.module) else {
                continue;
            };
            let Some(to_top) = top_level(&resolved) else {
                continue;
            };
            if !scan.nodes.contains(to_top) {
                continue;
            }

            let suppressed = suppressions.is_suppressed("dag", Some(import_ref.line));
            if to_top != from_top && !suppressed {
                edges
                    .entry(from_top.to_string())
                    .or_default()
                    .insert(to_top.to_string());
            }

            if resolved != file.module {
                detailed.push(DetailedEdge {
                    from_mod: file.module.clone(),
                    to_mod: resolved,
                    file: file.path.clone(),
                    line: import_ref.line,
                    suppressed,
                });
            }
        }
    }

    (edges, detailed)
}

fn collect_detailed_edges(
    src_root: &Path,
) -> (
    BTreeSet<String>,
    BTreeMap<String, BTreeSet<String>>,
    Vec<DetailedEdge>,
) {
    let scan = scan_dag_files(src_root);
    let parsed: Vec<Option<(ParsedDagFile, FileSuppressions)>> =
        parallel::map_items(&scan.files, true, |file| {
            top_level(&file.module)?;
            let raw = std::fs::read_to_string(&file.path).ok()?;
            let syntax = match syn::parse_file(&raw) {
                Ok(s) => s,
                Err(err) => {
                    eprintln!(
                        "[dag] warning: skipping {} due to parse error: {err}",
                        file.path.display()
                    );
                    return None;
                }
            };
            let dag_file = extract_dag_imports(&syntax);
            let suppressions = FileSuppressions::parse(&raw);
            Some((dag_file, suppressions))
        });

    let refs: Vec<Option<(&ParsedDagFile, &FileSuppressions)>> = parsed
        .iter()
        .map(|opt| opt.as_ref().map(|(d, s)| (d, s)))
        .collect();
    let (edges, detailed) = assemble_dag_graph(&scan, &refs);
    (scan.nodes, edges, detailed)
}

fn root_has_changed_rs(src_root: &Path, base_dir: &Path, changeset: &BTreeSet<PathBuf>) -> bool {
    let rel_root = normalize_rel_path(base_dir, src_root);
    let root_prefix = rel_root.to_string_lossy().replace('\\', "/");
    changeset.iter().any(|p| {
        if p.extension().and_then(OsStr::to_str) != Some("rs") {
            return false;
        }
        let s = p.to_string_lossy().replace('\\', "/");
        root_prefix.is_empty()
            || root_prefix == "."
            || s == root_prefix
            || s.starts_with(&format!("{root_prefix}/"))
    })
}

pub(crate) fn should_run_dag_for_opts(src_root: &Path, opts: &DagOptions<'_>) -> Result<bool> {
    if !src_root.exists() {
        bail!("Source root does not exist: {}", src_root.display());
    }
    if let Some(changed) = opts.changeset
        && !root_has_changed_rs(src_root, opts.base_dir, changed)
    {
        tracing::info!(
            src_root = %src_root.display(),
            "skipping DAG check (no changed Rust files under root)"
        );
        return Ok(false);
    }
    Ok(true)
}

pub(crate) fn build_dag_diagnostics(
    src_root: &Path,
    nodes: &BTreeSet<String>,
    edges: &BTreeMap<String, BTreeSet<String>>,
    detailed_edges: &[DetailedEdge],
    opts: &DagOptions<'_>,
) -> Vec<Diagnostic> {
    if opts.emit_text {
        println!(
            "[dag] scanned {} top-level module(s) under {}",
            nodes.len(),
            src_root.display()
        );
    }

    let rel_root = normalize_rel_path(opts.base_dir, src_root);
    let root_key = rel_root.to_string_lossy().replace('\\', "/");
    let mut diagnostics = Vec::new();

    for c in find_cycles(nodes, edges) {
        let joined = c.join(" <-> ");
        diagnostics.push(Diagnostic {
            rule_id: "warden/dag/cycle".to_string(),
            check: "dag".to_string(),
            severity: Severity::Error,
            message: format!(
                "Module dependency cycle detected in {}: {}",
                rel_root.display(),
                joined
            ),
            file: Some(rel_root.clone()),
            line: None,
            column: None,
            fingerprint: format!("dag:cycle:{root_key}:{}", c.join(",")),
            metric_value: None,
            suppressed: false,
            suppression_reason: None,
        });
    }

    let mut seen = BTreeSet::new();
    for edge in detailed_edges {
        let rel_file = normalize_rel_path(opts.base_dir, &edge.file);
        let rel_file_str = rel_file.to_string_lossy().replace('\\', "/");
        check_edge_forbid(
            edge,
            opts,
            &root_key,
            &rel_file,
            &rel_file_str,
            &mut seen,
            &mut diagnostics,
        );
        check_edge_layers(
            edge,
            opts,
            &root_key,
            &rel_file,
            &rel_file_str,
            &mut seen,
            &mut diagnostics,
        );
        check_edge_rules(
            edge,
            opts,
            &root_key,
            &rel_file,
            &rel_file_str,
            &mut seen,
            &mut diagnostics,
        );
    }

    diagnostics
}

/// Collect structured DAG diagnostics (cycles, forbidden imports, and layer/rule violations).
pub fn collect_dag(src_root: &Path, opts: &DagOptions<'_>) -> Result<Vec<Diagnostic>> {
    if !should_run_dag_for_opts(src_root, opts)? {
        return Ok(Vec::new());
    }

    tracing::debug!(
        src_root = %src_root.display(),
        forbid_imports = ?opts.forbid_imports,
        layers = opts.layers.len(),
        rules = opts.rules.len(),
        "starting module dependency DAG analysis"
    );

    let (nodes, edges, detailed_edges) = collect_detailed_edges(src_root);
    Ok(build_dag_diagnostics(
        src_root,
        &nodes,
        &edges,
        &detailed_edges,
        opts,
    ))
}

pub fn check_dag(src_root: &Path, forbid_imports: &[String]) -> Result<()> {
    check_dag_with_options(
        src_root,
        &DagOptions {
            base_dir: Path::new("."),
            forbid_imports,
            layers: &[],
            rules: &[],
            changeset: None,
            emit_text: true,
        },
    )
}

pub fn check_dag_with_options(src_root: &Path, opts: &DagOptions<'_>) -> Result<()> {
    let diagnostics = collect_dag(src_root, opts)?;
    let active: Vec<&Diagnostic> = diagnostics.iter().filter(|d| !d.suppressed).collect();

    if active.is_empty() {
        if opts.emit_text {
            println!("[dag] OK: dependencies form a clean DAG with 0 cycle(s)");
        }
        return Ok(());
    }

    let cycles: Vec<&Diagnostic> = active
        .iter()
        .copied()
        .filter(|d| d.rule_id == "warden/dag/cycle")
        .collect();
    if !cycles.is_empty() {
        let mut msg = String::from("Module dependency cycle(s) detected:\n");
        for c in &cycles {
            msg.push_str(&format!("  ❌ {}\n", c.message));
        }
        bail!("{msg}");
    }

    let forbidden: Vec<&Diagnostic> = active
        .iter()
        .copied()
        .filter(|d| d.rule_id == "warden/dag/forbid-import")
        .collect();
    if !forbidden.is_empty() {
        let lines: Vec<String> = forbidden
            .iter()
            .map(|d| format!("  ❌ {}", d.message))
            .collect();
        bail!(
            "Forbidden imports detected (modules depending on assembly/forbidden targets):\n{}",
            lines.join("\n")
        );
    }

    let layer_violations: Vec<String> = active
        .iter()
        .map(|d| format!("  ❌ {}", d.message))
        .collect();
    bail!(
        "Layered architecture boundary violation(s) detected:\n{}",
        layer_violations.join("\n")
    );
}

//! Module-boundary and dependency DAG checker.
//!
//! Validates module dependencies inside a crate source root:
//! 1. Builds the module dependency graph at top-level module granularity using `syn` AST.
//! 2. Detects module dependency cycles using Tarjan's SCC algorithm.
//! 3. Enforces forbidden import targets (e.g. assembly/bootstrap modules that must not be depended upon).

use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{ItemUse, Path as SynPath, UseTree};

/// One source file together with the module path it defines.
#[derive(Debug)]
struct SourceFile {
    module: Vec<String>,
    path: PathBuf,
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

#[derive(Default)]
struct ImportVisitor {
    paths: Vec<Vec<String>>,
}

impl<'ast> Visit<'ast> for ImportVisitor {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if is_test_item(item) {
            return;
        }
        visit::visit_item(self, item);
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        expand_syn_use_tree(&item.tree, &[], &mut self.paths);
        visit::visit_item_use(self, item);
    }

    fn visit_path(&mut self, path: &'ast SynPath) {
        if let Some(first) = path.segments.first() {
            let ident_str = ident_name(&first.ident);
            if ident_str == "crate" || ident_str == "super" || ident_str == "self" {
                let tokens: Vec<String> =
                    path.segments.iter().map(|s| ident_name(&s.ident)).collect();
                self.paths.push(tokens);
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

fn top_level(module: &[String]) -> Option<&str> {
    module.first().map(String::as_str)
}

pub fn module_graph(src_root: &Path) -> (BTreeSet<String>, BTreeMap<String, BTreeSet<String>>) {
    let mut files = Vec::new();
    collect_source_files(src_root, &[], &mut files);

    let nodes: BTreeSet<String> = files
        .iter()
        .filter_map(|file| top_level(&file.module).map(str::to_string))
        .collect();
    let module_names = nodes.clone();

    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for file in &files {
        let Some(from) = top_level(&file.module) else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&file.path) else {
            continue;
        };
        let syntax = match syn::parse_file(&raw) {
            Ok(s) => s,
            Err(err) => {
                eprintln!(
                    "[dag] warning: skipping {} due to parse error: {err}",
                    file.path.display()
                );
                continue;
            }
        };
        if has_test_attr(&syntax.attrs) {
            continue;
        }

        let mut visitor = ImportVisitor::default();
        visitor.visit_file(&syntax);

        for path in visitor.paths {
            let Some(resolved) = resolve_tokens(&path, &file.module) else {
                continue;
            };
            let Some(to) = top_level(&resolved) else {
                continue;
            };
            if to == from || !module_names.contains(to) {
                continue;
            }
            edges
                .entry(from.to_string())
                .or_default()
                .insert(to.to_string());
        }
    }
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

pub fn check_dag(src_root: &Path, forbid_imports: &[String]) -> Result<()> {
    if !src_root.exists() {
        bail!("Source root does not exist: {}", src_root.display());
    }
    tracing::debug!(
        src_root = %src_root.display(),
        forbid_imports = ?forbid_imports,
        "starting module dependency DAG analysis"
    );
    let (nodes, edges) = module_graph(src_root);
    tracing::trace!(
        nodes_count = nodes.len(),
        edges_count = edges.len(),
        "module dependency graph constructed"
    );
    println!(
        "[dag] scanned {} top-level module(s) under {}",
        nodes.len(),
        src_root.display()
    );

    let cycles = find_cycles(&nodes, &edges);
    tracing::trace!(cycles_count = cycles.len(), "cycle detection finished");
    if !cycles.is_empty() {
        let mut msg = String::from("Module dependency cycle(s) detected:\n");
        for c in &cycles {
            msg.push_str(&format!("  ❌ {}\n", c.join(" <-> ")));
        }
        bail!("{msg}");
    }

    let forbidden_violations: Vec<String> = edges
        .iter()
        .flat_map(|(from, targets)| targets.iter().map(move |to| (from, to)))
        .filter(|(from, to)| forbid_imports.contains(to) && !forbid_imports.contains(from))
        .map(|(from, to)| format!("  ❌ {from} -> {to}"))
        .collect();

    if !forbidden_violations.is_empty() {
        bail!(
            "Forbidden imports detected (modules depending on assembly/forbidden targets):\n{}",
            forbidden_violations.join("\n")
        );
    }

    println!("[dag] OK: dependencies form a clean DAG with 0 cycle(s)");
    Ok(())
}

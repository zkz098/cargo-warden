//! Module-boundary and dependency DAG checker.
//!
//! Validates module dependencies inside a crate source root:
//! 1. Builds the module dependency graph at top-level module granularity.
//! 2. Detects module dependency cycles using Tarjan's SCC algorithm.
//! 3. Enforces forbidden import targets (e.g. assembly/bootstrap modules that must not be depended upon).

use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

fn skip_line_comment(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(chars: &[char], mut i: usize) -> usize {
    let mut depth = 1;
    i += 2;
    while i < chars.len() && depth > 0 {
        if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
            depth += 1;
            i += 2;
        } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
            depth -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

fn skip_string_literal(chars: &[char], mut i: usize) -> usize {
    if i > 0 && (chars[i - 1] == 'r' || chars[i - 1] == 'b') {
        i += 1;
        while i < chars.len() && chars[i] == '#' {
            i += 1;
        }
    }
    i += 1;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
        } else if chars[i] == '"' {
            return i + 1;
        } else {
            i += 1;
        }
    }
    i
}

fn char_literal_len(chars: &[char], i: usize) -> Option<usize> {
    match chars.get(i + 1) {
        Some('\\') => (chars.get(i + 3) == Some(&'\'')).then_some(4),
        Some(_) => (chars.get(i + 2) == Some(&'\'')).then_some(3),
        None => None,
    }
}

pub fn strip_noncode(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            i = skip_line_comment(&chars, i);
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i = skip_block_comment(&chars, i);
        } else if c == '"' {
            i = skip_string_literal(&chars, i);
        } else if let Some(len) = char_literal_len(&chars, i) {
            i += len;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn use_statements(clean: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = clean.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let is_use = bytes[i..].starts_with(b"use")
            && (i == 0 || !is_ident_byte(bytes[i - 1]))
            && !bytes.get(i + 3).is_some_and(|b| is_ident_byte(*b));
        if !is_use {
            i += 1;
            continue;
        }
        let start = i + 3;
        let mut j = start;
        let mut depth: i32 = 0;
        while j < bytes.len() {
            match bytes[j] {
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => depth -= 1,
                b';' if depth <= 0 => break,
                _ => {}
            }
            j += 1;
        }
        out.push(clean[start..j].trim());
        i = j + 1;
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn split_top(tree: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    for (idx, c) in tree.char_indices() {
        match c {
            '{' | '(' | '[' | '<' => depth += 1,
            '}' | ')' | ']' | '>' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&tree[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&tree[start..]);
    parts
}

fn expand_use_tree(tree: &str, base: &[String], out: &mut Vec<Vec<String>>) {
    for part in split_top(tree) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part.starts_with('{') && part.ends_with('}') {
            expand_use_tree(&part[1..part.len() - 1], base, out);
            continue;
        }
        if let Some(open) = part.find('{') {
            let head: Vec<String> = part[..open]
                .trim()
                .trim_end_matches(':')
                .split("::")
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let mut nested = base.to_vec();
            nested.extend(head);
            let inner = part[open..].trim();
            let inner = inner
                .strip_prefix('{')
                .unwrap_or(inner)
                .trim_end_matches('}');
            expand_use_tree(inner, &nested, out);
            continue;
        }
        let leaf = part.split(" as ").next().unwrap_or(part).trim();
        let mut tokens: Vec<String> = leaf
            .split("::")
            .filter(|s| !s.is_empty() && *s != "*")
            .map(str::to_string)
            .collect();
        if tokens.is_empty() {
            continue;
        }
        if tokens[0] == "self" && tokens.len() == 1 {
            out.push(base.to_vec());
            continue;
        }
        let mut full = base.to_vec();
        full.append(&mut tokens);
        out.push(full);
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

fn inline_paths(clean: &str, out: &mut Vec<Vec<String>>) {
    let bytes = clean.as_bytes();
    for keyword in ["crate", "super", "self"] {
        let needle = keyword.as_bytes();
        let mut i = 0;
        while i + needle.len() < bytes.len() {
            if &bytes[i..i + needle.len()] != needle {
                i += 1;
                continue;
            }
            let starts_word = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after = skip_whitespace(bytes, i + needle.len());
            if starts_word && bytes[after..].starts_with(b"::") {
                let mut tokens = vec![keyword.to_string()];
                parse_path_tail(clean, bytes, i + needle.len(), &mut tokens);
                out.push(tokens);
            }
            i += needle.len();
        }
    }
}

fn skip_whitespace(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

fn parse_path_tail(clean: &str, bytes: &[u8], mut j: usize, tokens: &mut Vec<String>) {
    while bytes[j..].starts_with(b"::") {
        let name_start = skip_whitespace(bytes, j + 2);
        let mut name_end = name_start;
        while bytes.get(name_end).is_some_and(|b| is_ident_byte(*b)) {
            name_end += 1;
        }
        if name_end == name_start {
            break;
        }
        tokens.push(clean[name_start..name_end].to_string());
        j = skip_whitespace(bytes, name_end);
    }
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
        let clean = strip_noncode(&raw);
        let mut tokens = Vec::new();
        for stmt in use_statements(&clean) {
            expand_use_tree(stmt, &[], &mut tokens);
        }
        inline_paths(&clean, &mut tokens);
        for path in tokens {
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
    let (nodes, edges) = module_graph(src_root);
    println!(
        "[dag] scanned {} top-level module(s) under {}",
        nodes.len(),
        src_root.display()
    );

    let cycles = find_cycles(&nodes, &edges);
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

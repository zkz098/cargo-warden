//! Slint theme color checks, token rewriting, and theme generator.
//!
//! Provides:
//! - `check`: Scans `.slint` files for unmapped hex literals (enforces Theme tokens).
//! - `apply`: Rewrites hex literals into `Theme.c<hex>` and injects import statements.
//! - `generate`: Emits `theme.slint` from a declarative color mapping.

use crate::report::{Diagnostic, Severity, normalize_rel_path};
use crate::suppression::FileSuppressions;
use crate::walker::{WalkOptions, collect_files};
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThemeDefinition {
    /// Static dark to light color mapping: key -> light_hex
    #[serde(default)]
    pub static_map: BTreeMap<String, String>,
    /// Accent color ramps
    #[serde(default)]
    pub accent_map: Vec<AccentDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccentDef {
    pub key: String,
    pub dark: AccentHues,
    pub light: AccentHues,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccentHues {
    pub default: String,
    #[serde(flatten)]
    pub variants: BTreeMap<String, String>,
}

/// Recursively find all `.slint` files under `dir`, excluding `theme.slint`.
pub fn find_slint_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(find_slint_files(&path)?);
        } else if path.extension().is_some_and(|ext| ext == "slint")
            && path.file_name().is_some_and(|n| n != "theme.slint")
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[derive(Clone, Copy)]
enum LexState {
    Normal,
    LineComment,
    BlockComment,
    StringLit { escaped: bool },
}

fn push_masked_char(out: &mut String, ch: char) {
    if ch == '\n' {
        out.push('\n');
    } else {
        for _ in 0..ch.len_utf8() {
            out.push(' ');
        }
    }
}

/// Mask `//`, `/* ... */` comments and `"..."` strings with spaces while preserving byte offsets and newlines.
fn mask_comments_and_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();
    let mut state = LexState::Normal;

    while let Some((_, ch)) = chars.next() {
        state = step_lex_state(state, ch, &mut chars, &mut out);
    }
    out
}

fn step_normal_lex(
    ch: char,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    out: &mut String,
) -> LexState {
    if ch == '/' && chars.peek().is_some_and(|&(_, next)| next == '/') {
        push_masked_char(out, ch);
        if let Some((_, next)) = chars.next() {
            push_masked_char(out, next);
        }
        return LexState::LineComment;
    }
    if ch == '/' && chars.peek().is_some_and(|&(_, next)| next == '*') {
        push_masked_char(out, ch);
        if let Some((_, next)) = chars.next() {
            push_masked_char(out, next);
        }
        return LexState::BlockComment;
    }
    if ch == '"' {
        push_masked_char(out, ch);
        return LexState::StringLit { escaped: false };
    }
    out.push(ch);
    LexState::Normal
}

fn step_lex_state(
    state: LexState,
    ch: char,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    out: &mut String,
) -> LexState {
    match state {
        LexState::Normal => step_normal_lex(ch, chars, out),
        LexState::LineComment => {
            push_masked_char(out, ch);
            if ch == '\n' {
                LexState::Normal
            } else {
                LexState::LineComment
            }
        }
        LexState::BlockComment => {
            push_masked_char(out, ch);
            if ch == '*' && chars.peek().is_some_and(|&(_, next)| next == '/') {
                if let Some((_, next)) = chars.next() {
                    push_masked_char(out, next);
                }
                LexState::Normal
            } else {
                LexState::BlockComment
            }
        }
        LexState::StringLit { escaped } => {
            push_masked_char(out, ch);
            if escaped {
                LexState::StringLit { escaped: false }
            } else if ch == '\\' {
                LexState::StringLit { escaped: true }
            } else if ch == '"' {
                LexState::Normal
            } else {
                LexState::StringLit { escaped: false }
            }
        }
    }
}

fn expand_short_hex(hex: &str) -> Option<String> {
    if hex.len() != 3 && hex.len() != 4 {
        return None;
    }
    let mut out = String::with_capacity(hex.len() * 2);
    for ch in hex.chars() {
        out.push(ch);
        out.push(ch);
    }
    Some(out)
}

fn is_whitelisted_hex(hex: &str, unchanged_set: &HashSet<String>) -> bool {
    if unchanged_set.contains(hex) {
        return true;
    }
    expand_short_hex(hex).is_some_and(|expanded| unchanged_set.contains(&expanded))
}

fn find_unmapped_in_masked_line(
    masked_line: &str,
    hex_re: &Regex,
    rgb_re: &Regex,
    unchanged_set: &HashSet<String>,
) -> Vec<(String, String)> {
    let mut hits = Vec::new();
    for cap in hex_re.captures_iter(masked_line) {
        let hex = cap[1].to_ascii_lowercase();
        if matches!(hex.len(), 3 | 4 | 6 | 8) && !is_whitelisted_hex(&hex, unchanged_set) {
            hits.push((hex.clone(), format!("#{hex}")));
        }
    }
    for m in rgb_re.find_iter(masked_line) {
        let expr: String = m.as_str().split_whitespace().collect();
        hits.push((expr.clone(), expr));
    }
    hits
}

/// Scan `ui_dir` for hex (`#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`) or `rgb()`/`rgba()` literals not in `unchanged`.
pub fn scan_unmapped(ui_dir: &Path, unchanged: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
    tracing::debug!(
        ui_dir = %ui_dir.display(),
        unchanged_count = unchanged.len(),
        "scanning slint files for unmapped color literals"
    );
    let unchanged_set: HashSet<String> = unchanged.iter().map(|s| s.to_ascii_lowercase()).collect();
    let hex_re = Regex::new(r"#([0-9a-fA-F]{3,8})\b")?;
    let rgb_re = Regex::new(r"\brgba?\s*\([^)]*\)")?;
    let slint_files = find_slint_files(ui_dir)?;
    let mut unmapped: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for file in &slint_files {
        let file_name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.display().to_string());
        let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
        let masked = mask_comments_and_strings(&text);

        for line in masked.lines() {
            for (key, _) in find_unmapped_in_masked_line(line, &hex_re, &rgb_re, &unchanged_set) {
                let entry = unmapped.entry(key).or_default();
                if !entry.contains(&file_name) {
                    entry.push(file_name.clone());
                }
            }
        }
    }

    Ok(unmapped)
}

/// Options for running the Slint theme check with `.gitignore`, changeset, and structured reporting.
#[derive(Debug, Clone)]
pub struct SlintThemeOptions<'a> {
    pub base_dir: &'a Path,
    pub unchanged: &'a [String],
    pub respect_gitignore: bool,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

/// Collect structured Slint theme diagnostics (line-accurate, comment/string-safe, inline suppression aware).
pub fn collect_theme(ui_dir: &Path, opts: &SlintThemeOptions<'_>) -> Result<Vec<Diagnostic>> {
    if opts.emit_text {
        println!(
            "[slint-theme] scanning {} for unmapped color literals...",
            ui_dir.display()
        );
    }
    tracing::debug!(
        ui_dir = %ui_dir.display(),
        unchanged_count = opts.unchanged.len(),
        respect_gitignore = opts.respect_gitignore,
        "scanning slint files for unmapped color literals"
    );

    let slint_exts = vec!["slint".to_string()];
    let ignore_theme = vec!["theme.slint".to_string()];
    let slint_files = collect_files(
        &[ui_dir.to_path_buf()],
        &WalkOptions {
            extensions: &slint_exts,
            ignore_patterns: &ignore_theme,
            respect_gitignore: opts.respect_gitignore,
            base_dir: Some(opts.base_dir),
            changeset: opts.changeset,
        },
    );

    let unchanged_set: HashSet<String> = opts
        .unchanged
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let hex_re = Regex::new(r"#([0-9a-fA-F]{3,8})\b")?;
    let rgb_re = Regex::new(r"\brgba?\s*\([^)]*\)")?;
    let mut diagnostics = Vec::new();

    for file in &slint_files {
        let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
        let masked = mask_comments_and_strings(&text);
        let rel = normalize_rel_path(opts.base_dir, file);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let suppressions = FileSuppressions::parse(&text);

        for (idx, line) in masked.lines().enumerate() {
            let line_no = idx + 1;
            for (key, display) in
                find_unmapped_in_masked_line(line, &hex_re, &rgb_re, &unchanged_set)
            {
                let suppressed = suppressions.is_suppressed("slint-theme", Some(line_no));
                diagnostics.push(Diagnostic {
                    rule_id: "warden/slint-theme".to_string(),
                    check: "slint-theme".to_string(),
                    severity: Severity::Error,
                    message: format!("Unmapped color {display} at {}:{}", rel.display(), line_no),
                    file: Some(rel.clone()),
                    line: Some(line_no),
                    column: None,
                    fingerprint: format!("slint-theme:{rel_str}:{key}"),
                    metric_value: None,
                    suppressed,
                    suppression_reason: suppressed.then(|| "inline".to_string()),
                });
            }
        }
    }

    Ok(diagnostics)
}

pub fn check_theme_with_options(ui_dir: &Path, opts: &SlintThemeOptions<'_>) -> Result<()> {
    let diagnostics = collect_theme(ui_dir, opts)?;
    let active: Vec<&Diagnostic> = diagnostics.iter().filter(|d| !d.suppressed).collect();
    if active.is_empty() {
        if opts.emit_text {
            println!(
                "[slint-theme] OK: all hex colors in {} are mapped or whitelisted",
                ui_dir.display()
            );
        }
        Ok(())
    } else {
        if opts.emit_text {
            println!(
                "[slint-theme] ❌ unmapped color literals found ({}):",
                active.len()
            );
            for d in &active {
                println!("  {}", d.message);
            }
        }
        bail!("Unmapped color literals remain in .slint components");
    }
}

/// Run the theme color check. Fails if any unmapped colors are found.
pub fn check_theme(ui_dir: &Path, unchanged: &[String]) -> Result<()> {
    println!(
        "[slint-theme] scanning {} for unmapped hex literals...",
        ui_dir.display()
    );
    let unmapped = scan_unmapped(ui_dir, unchanged)?;
    if unmapped.is_empty() {
        println!(
            "[slint-theme] OK: all hex colors in {} are mapped or whitelisted",
            ui_dir.display()
        );
        Ok(())
    } else {
        println!(
            "[slint-theme] ❌ unmapped hex colors found ({}):",
            unmapped.len()
        );
        for (color, files) in &unmapped {
            println!("  #{color} in: {}", files.join(", "));
        }
        bail!("Unmapped hex colors remain in .slint components");
    }
}

/// Load a hex-to-token mapping (`hex -> Theme.c<hex>`) from an optional theme definition TOML file.
pub fn load_token_mapping(base_dir: &Path, theme_def: Option<&Path>) -> BTreeMap<String, String> {
    let mut mapping = BTreeMap::new();
    let Some(def_path) = theme_def else {
        return mapping;
    };
    let Ok(content) = fs::read_to_string(base_dir.join(def_path)) else {
        return mapping;
    };
    let Ok(def) = toml::from_str::<ThemeDefinition>(&content) else {
        return mapping;
    };
    for (k, _) in def.static_map {
        mapping.insert(k.to_ascii_lowercase(), format!("Theme.c{k}"));
    }
    for a in def.accent_map {
        mapping.insert(a.key.to_ascii_lowercase(), format!("Theme.c{}", a.key));
    }
    mapping
}

fn replace_unmasked_hex(
    text: &str,
    masked: &str,
    token_re: &Regex,
    mapping: &BTreeMap<String, String>,
    unchanged_set: &HashSet<String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for caps in token_re.captures_iter(masked) {
        let Some(full) = caps.get(0) else { continue };
        let hex = caps[1].to_ascii_lowercase();
        if !matches!(hex.len(), 3 | 4 | 6 | 8) || is_whitelisted_hex(&hex, unchanged_set) {
            continue;
        }
        out.push_str(&text[last..full.start()]);
        if let Some(token) = mapping.get(&hex) {
            out.push_str(token);
        } else {
            out.push_str(&format!("Theme.c{hex}"));
        }
        last = full.end();
    }
    out.push_str(&text[last..]);
    out
}

/// Apply tokens across `.slint` files in `ui_dir`, assuming `theme.slint` sits directly in `ui_dir`.
pub fn apply_tokens(
    ui_dir: &Path,
    mapping: &BTreeMap<String, String>,
    unchanged: &[String],
) -> Result<()> {
    apply_tokens_with_out_file(ui_dir, &ui_dir.join("theme.slint"), mapping, unchanged)
}

/// Apply tokens across `.slint` files in `ui_dir` with accurate relative imports to `out_file`.
pub fn apply_tokens_with_out_file(
    ui_dir: &Path,
    out_file: &Path,
    mapping: &BTreeMap<String, String>,
    unchanged: &[String],
) -> Result<()> {
    let token_re = Regex::new(r"#([0-9a-fA-F]{3,8})\b")?;
    let slint_files = find_slint_files(ui_dir)?;
    let unchanged_set: HashSet<String> = unchanged.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut updated = 0;

    for file in &slint_files {
        let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
        let masked = mask_comments_and_strings(&text);
        let result = replace_unmasked_hex(&text, &masked, &token_re, mapping, &unchanged_set);

        if result != text {
            let mut final_text = result;
            if !final_text.contains("import { Theme") && !final_text.contains("import {Theme") {
                final_text = insert_import(&final_text, &theme_import_to(file, out_file));
            }
            fs::write(file, &final_text).with_context(|| format!("write {}", file.display()))?;
            println!("[slint-theme] updated {}", file.display());
            updated += 1;
        }
    }

    println!("[slint-theme] applied tokens to {updated} Slint file(s)");
    Ok(())
}

fn normalized_components(path: &Path) -> Vec<std::path::Component<'_>> {
    path.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

fn theme_import_to(file: &Path, out_file: &Path) -> String {
    let from_dir = file.parent().unwrap_or_else(|| Path::new(""));
    let from_parts = normalized_components(from_dir);
    let to_parts = normalized_components(out_file);

    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut rel_parts: Vec<String> = Vec::new();
    for part in &from_parts[common..] {
        if matches!(part, std::path::Component::Normal(_)) {
            rel_parts.push("..".to_string());
        }
    }
    for part in &to_parts[common..] {
        rel_parts.push(part.as_os_str().to_string_lossy().into_owned());
    }

    let rel = if rel_parts.is_empty() {
        "theme.slint".to_string()
    } else {
        rel_parts.join("/")
    };
    format!("import {{ Theme }} from \"{rel}\";")
}

fn insert_import(text: &str, import_stmt: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut last_import_idx = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("import ") {
            last_import_idx = Some(i);
        } else if last_import_idx.is_some() && !trimmed.is_empty() && !trimmed.starts_with("//") {
            break;
        }
    }

    let mut new_lines = Vec::with_capacity(lines.len() + 1);
    match last_import_idx {
        Some(idx) => {
            for (i, line) in lines.into_iter().enumerate() {
                new_lines.push(line.to_string());
                if i == idx {
                    new_lines.push(import_stmt.to_string());
                }
            }
        }
        None => {
            new_lines.push(import_stmt.to_string());
            new_lines.extend(lines.into_iter().map(String::from));
        }
    }

    let mut result = new_lines.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Generate `theme.slint` from definition.
pub fn generate_theme(def: &ThemeDefinition, out_file: &Path) -> Result<()> {
    let mut out = String::new();
    out.push_str(
        r#"// Auto-generated by cargo-warden theme. Do not edit manually.
import { Palette } from "std-widgets.slint";

export enum ColorModePref { System, Light, Dark }

export global Theme {
    in-out property <ColorModePref> mode: ColorModePref.System;
    in-out property <bool> dark: mode == ColorModePref.Dark
        || (mode == ColorModePref.System && Palette.color-scheme == ColorScheme.dark);

"#,
    );

    for (dark, light) in &def.static_map {
        out.push_str(&format!("    /// dark #{dark} / light #{light}\n"));
        out.push_str(&format!(
            "    in property <color> c{dark}: dark ? #{dark} : #{light};\n"
        ));
    }

    for accent in &def.accent_map {
        let key = &accent.key;
        out.push_str(&format!("    /// accent #{key}\n"));
        out.push_str(&format!(
            "    in property <color> c{key}: dark ? #{} : #{};\n",
            accent.dark.default, accent.light.default
        ));
    }

    out.push_str("}\n");

    if let Some(parent) = out_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out_file, out)?;
    println!("[slint-theme] generated {}", out_file.display());
    Ok(())
}

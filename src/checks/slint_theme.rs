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

/// Scan `ui_dir` for hex literals not in `unchanged`.
pub fn scan_unmapped(ui_dir: &Path, unchanged: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
    tracing::debug!(
        ui_dir = %ui_dir.display(),
        unchanged_count = unchanged.len(),
        "scanning slint files for unmapped hex literals"
    );
    let unchanged_set: HashSet<String> = unchanged.iter().map(|s| s.to_ascii_lowercase()).collect();
    let re = Regex::new(r"#([0-9a-fA-F]{3,8})\b")?;
    let slint_files = find_slint_files(ui_dir)?;
    let mut unmapped: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for file in &slint_files {
        let file_name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.display().to_string());
        let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;

        for cap in re.captures_iter(&text) {
            let hex = cap[1].to_ascii_lowercase();
            if (hex.len() == 6 || hex.len() == 8) && !unchanged_set.contains(&hex) {
                let entry = unmapped.entry(hex).or_default();
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

/// Collect structured Slint theme diagnostics (line-accurate + inline suppression aware).
pub fn collect_theme(ui_dir: &Path, opts: &SlintThemeOptions<'_>) -> Result<Vec<Diagnostic>> {
    if opts.emit_text {
        println!(
            "[slint-theme] scanning {} for unmapped hex literals...",
            ui_dir.display()
        );
    }
    tracing::debug!(
        ui_dir = %ui_dir.display(),
        unchanged_count = opts.unchanged.len(),
        respect_gitignore = opts.respect_gitignore,
        "scanning slint files for unmapped hex literals"
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
    let re = Regex::new(r"#([0-9a-fA-F]{3,8})\b")?;
    let mut diagnostics = Vec::new();

    for file in &slint_files {
        let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
        let rel = normalize_rel_path(opts.base_dir, file);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let suppressions = FileSuppressions::parse(&text);

        for (idx, line) in text.lines().enumerate() {
            let line_no = idx + 1;
            for cap in re.captures_iter(line) {
                let hex = cap[1].to_ascii_lowercase();
                if (hex.len() == 6 || hex.len() == 8) && !unchanged_set.contains(&hex) {
                    let suppressed = suppressions.is_suppressed("slint-theme", Some(line_no));
                    diagnostics.push(Diagnostic {
                        rule_id: "warden/slint-theme".to_string(),
                        check: "slint-theme".to_string(),
                        severity: Severity::Error,
                        message: format!("Unmapped color #{hex} at {}:{}", rel.display(), line_no),
                        file: Some(rel.clone()),
                        line: Some(line_no),
                        column: None,
                        fingerprint: format!("slint-theme:{rel_str}:{hex}"),
                        metric_value: None,
                        suppressed,
                        suppression_reason: suppressed.then(|| "inline".to_string()),
                    });
                }
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
                "[slint-theme] ❌ unmapped hex colors found ({}):",
                active.len()
            );
            for d in &active {
                println!("  {}", d.message);
            }
        }
        bail!("Unmapped hex colors remain in .slint components");
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

/// Apply tokens across `.slint` files in `ui_dir`.
pub fn apply_tokens(
    ui_dir: &Path,
    mapping: &BTreeMap<String, String>,
    unchanged: &[String],
) -> Result<()> {
    let token_re = Regex::new(r"#([0-9a-fA-F]{3,8})\b")?;
    let slint_files = find_slint_files(ui_dir)?;
    let unchanged_set: HashSet<String> = unchanged.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut updated = 0;

    for file in &slint_files {
        let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
        let result = token_re
            .replace_all(&text, |caps: &regex::Captures| {
                let hex = caps[1].to_ascii_lowercase();
                if unchanged_set.contains(&hex) {
                    return caps[0].to_string();
                }
                if let Some(token) = mapping.get(&hex) {
                    token.clone()
                } else {
                    // Default convention: Theme.c<hex>
                    format!("Theme.c{hex}")
                }
            })
            .into_owned();

        if result != text {
            let mut final_text = result;
            if !final_text.contains("import { Theme") && !final_text.contains("import {Theme") {
                final_text = insert_import(&final_text, &theme_import(file));
            }
            fs::write(file, &final_text).with_context(|| format!("write {}", file.display()))?;
            println!("[slint-theme] updated {}", file.display());
            updated += 1;
        }
    }

    println!("[slint-theme] applied tokens to {updated} Slint file(s)");
    Ok(())
}

fn theme_import(file: &Path) -> String {
    let is_component = file
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "components");
    let rel = if is_component {
        "../theme.slint"
    } else {
        "theme.slint"
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

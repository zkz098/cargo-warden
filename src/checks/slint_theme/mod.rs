//! Slint theme color checks, design token schema/generator, and UI specification rules.

pub mod parser;
mod rules;
pub mod tokens;

use super::parallel;
use crate::config::SlintRulesConfig;
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use crate::suppression::{FileSuppressions, collect_unused_suppression_diagnostics};
use crate::walker::{PathPatternMatcher, WalkOptions, collect_files};
use anyhow::{Context, Result, bail};
use parser::{extract_exported_theme_props, mask_comments_and_strings, parse_slint_file};
use regex::Regex;
use rules::{NormalizedAllowLists, collect_ui_spec_diags, collect_unknown_token_diags};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub use tokens::{
    AccentDef, AccentHues, ColorTokenDef, FontWeightValue, TextStyleDef, ThemeDefinition,
    TypographyTokens, defined_token_idents, generate_theme, load_theme_definition,
    load_token_mapping, parse_dtcg_tokens, render_theme,
};

const OWNED_SLINT_RULES: &[&str] = &[
    "slint-theme",
    "slint-spacing",
    "slint-radius",
    "slint-typography",
    "slint-duration",
    "slint-unknown-token",
];

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

/// Options for running the legacy Slint color check with `.gitignore`, changeset, and structured reporting.
#[derive(Debug, Clone)]
pub struct SlintThemeOptions<'a> {
    pub base_dir: &'a Path,
    pub unchanged: &'a [String],
    pub respect_gitignore: bool,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

/// Extended policy for running all Slint design token and UI specification checks.
#[derive(Debug, Clone)]
pub struct SlintThemePolicy<'a> {
    pub base_dir: &'a Path,
    pub out_file: &'a Path,
    pub theme_def: Option<&'a Path>,
    pub unchanged_colors: &'a [String],
    pub exclude: &'a [String],
    pub respect_gitignore: bool,
    pub verify_generated: bool,
    pub check_unknown_tokens: bool,
    pub check_unused_tokens: bool,
    pub rules: &'a SlintRulesConfig,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
    pub parallel: bool,
}

/// Collect structured Slint theme diagnostics using default color-only rules.
pub fn collect_theme(ui_dir: &Path, opts: &SlintThemeOptions<'_>) -> Result<Vec<Diagnostic>> {
    collect_theme_with_parallel(ui_dir, opts, true)
}

pub(crate) fn collect_theme_with_parallel(
    ui_dir: &Path,
    opts: &SlintThemeOptions<'_>,
    parallel: bool,
) -> Result<Vec<Diagnostic>> {
    let default_rules = SlintRulesConfig::default();
    let default_out = ui_dir.join("theme.slint");
    let policy = SlintThemePolicy {
        base_dir: opts.base_dir,
        out_file: &default_out,
        theme_def: None,
        unchanged_colors: opts.unchanged,
        exclude: &[],
        respect_gitignore: opts.respect_gitignore,
        verify_generated: false,
        check_unknown_tokens: false,
        check_unused_tokens: false,
        rules: &default_rules,
        changeset: opts.changeset,
        emit_text: opts.emit_text,
        parallel,
    };
    collect_theme_with_policy(ui_dir, &policy)
}

/// Collect structured diagnostics across all enabled Slint design token & UI specification rules.
pub fn collect_theme_with_policy(
    ui_dir: &Path,
    policy: &SlintThemePolicy<'_>,
) -> Result<Vec<Diagnostic>> {
    if policy.emit_text {
        println!(
            "[slint-theme] scanning {} against Slint token/UI rules...",
            ui_dir.display()
        );
    }

    let out_abs = resolve_against_base(policy.base_dir, policy.out_file);
    let out_rel = normalize_rel_path(policy.base_dir, &out_abs);
    let loaded_def = load_optional_theme_def(policy.base_dir, policy.theme_def)?;
    let known_tokens = resolve_known_tokens(policy, &out_abs, loaded_def.as_ref());

    let slint_files = collect_target_slint_files(ui_dir, policy, &out_rel);
    let normalized_allow = NormalizedAllowLists::from_policy(policy);
    let hex_re = Regex::new(r"#([0-9a-fA-F]{3,8})\b")?;
    let rgb_re = Regex::new(r"\brgba?\s*\([^)]*\)")?;

    let per_file = parallel::map_items(&slint_files, policy.parallel, |file| {
        analyze_slint_file(
            file,
            policy,
            &normalized_allow,
            known_tokens.as_ref(),
            &hex_re,
            &rgb_re,
        )
    });

    let mut diagnostics = Vec::new();
    let mut used_tokens = BTreeSet::new();
    for res in per_file {
        let (file_diags, refs) = res?;
        diagnostics.extend(file_diags);
        used_tokens.extend(refs);
    }

    if policy.verify_generated {
        append_generated_sync_diag(
            policy,
            &out_abs,
            &out_rel,
            loaded_def.as_ref(),
            &mut diagnostics,
        )?;
    }

    if policy.check_unused_tokens
        && policy.changeset.is_none()
        && let Some(def) = loaded_def.as_ref()
    {
        append_unused_token_warnings(policy, &out_rel, def, &used_tokens, &mut diagnostics);
    }

    Ok(diagnostics)
}

fn resolve_against_base(base_dir: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        base_dir.join(target)
    }
}

fn load_optional_theme_def(
    base_dir: &Path,
    theme_def: Option<&Path>,
) -> Result<Option<ThemeDefinition>> {
    let Some(rel) = theme_def else {
        return Ok(None);
    };
    let full = resolve_against_base(base_dir, rel);
    if !full.is_file() {
        bail!("Theme definition file not found: {}", full.display());
    }
    Ok(Some(load_theme_definition(&full)?))
}

fn resolve_known_tokens(
    policy: &SlintThemePolicy<'_>,
    out_abs: &Path,
    loaded_def: Option<&ThemeDefinition>,
) -> Option<BTreeSet<String>> {
    if !policy.check_unknown_tokens && !policy.check_unused_tokens {
        return None;
    }
    let mut tokens = BTreeSet::new();
    if let Some(def) = loaded_def {
        tokens.extend(defined_token_idents(def));
    }
    if out_abs.is_file()
        && let Ok(text) = fs::read_to_string(out_abs)
    {
        tokens.extend(extract_exported_theme_props(&text));
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

fn collect_target_slint_files(
    ui_dir: &Path,
    policy: &SlintThemePolicy<'_>,
    out_rel: &Path,
) -> Vec<PathBuf> {
    let slint_exts = vec!["slint".to_string()];
    let raw_files = collect_files(
        &[ui_dir.to_path_buf()],
        &WalkOptions {
            extensions: &slint_exts,
            ignore_patterns: policy.exclude,
            respect_gitignore: policy.respect_gitignore,
            base_dir: Some(policy.base_dir),
            changeset: policy.changeset,
        },
    );
    let matcher = PathPatternMatcher::new(policy.exclude);
    raw_files
        .into_iter()
        .filter(|path| {
            let rel = normalize_rel_path(policy.base_dir, path);
            rel != out_rel && !matcher.matches(&rel)
        })
        .collect()
}

fn needs_slint_parse(policy: &SlintThemePolicy<'_>) -> bool {
    policy.check_unknown_tokens
        || policy.check_unused_tokens
        || policy.rules.spacing
        || policy.rules.radius
        || policy.rules.typography
        || policy.rules.duration
}

fn analyze_slint_file(
    file: &Path,
    policy: &SlintThemePolicy<'_>,
    allow: &NormalizedAllowLists,
    known_tokens: Option<&BTreeSet<String>>,
    hex_re: &Regex,
    rgb_re: &Regex,
) -> Result<(Vec<Diagnostic>, BTreeSet<String>)> {
    let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let rel = normalize_rel_path(policy.base_dir, file);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let suppressions = FileSuppressions::parse(&text);
    let mut diags = Vec::new();
    let mut used_refs = BTreeSet::new();

    if policy.rules.color {
        collect_color_diags(
            &text,
            &rel,
            &rel_str,
            &suppressions,
            &allow.unchanged_colors,
            hex_re,
            rgb_re,
            &mut diags,
        );
    }

    if needs_slint_parse(policy) {
        let parsed = parse_slint_file(&text);
        for tr in &parsed.theme_refs {
            used_refs.insert(tr.token_name.clone());
        }
        if policy.check_unknown_tokens
            && let Some(known) = known_tokens
        {
            collect_unknown_token_diags(&parsed, known, &rel, &rel_str, &suppressions, &mut diags);
        }
        collect_ui_spec_diags(
            &parsed,
            policy.rules,
            allow,
            &rel,
            &rel_str,
            &suppressions,
            &mut diags,
        );
    }

    diags.extend(collect_unused_suppression_diagnostics(
        &rel,
        &suppressions,
        OWNED_SLINT_RULES,
        &diags,
    ));
    Ok((diags, used_refs))
}

#[allow(clippy::too_many_arguments)]
fn collect_color_diags(
    text: &str,
    rel: &Path,
    rel_str: &str,
    suppressions: &FileSuppressions,
    unchanged_set: &HashSet<String>,
    hex_re: &Regex,
    rgb_re: &Regex,
    out: &mut Vec<Diagnostic>,
) {
    let masked = mask_comments_and_strings(text);
    for (idx, line) in masked.lines().enumerate() {
        let line_no = idx + 1;
        for (key, display) in find_unmapped_in_masked_line(line, hex_re, rgb_re, unchanged_set) {
            let suppressed = suppressions.is_suppressed("slint-theme", Some(line_no));
            out.push(Diagnostic {
                rule_id: "warden/slint-theme".to_string(),
                check: "slint-theme".to_string(),
                severity: Severity::Error,
                message: format!("Unmapped color {display} at {}:{}", rel.display(), line_no),
                file: Some(rel.to_path_buf()),
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

fn append_generated_sync_diag(
    policy: &SlintThemePolicy<'_>,
    out_abs: &Path,
    out_rel: &Path,
    loaded_def: Option<&ThemeDefinition>,
    out: &mut Vec<Diagnostic>,
) -> Result<()> {
    let Some(def) = loaded_def else {
        bail!("`verify_generated` requires `slint_theme.theme_def` to be configured");
    };
    let expected = render_theme(def)?.replace("\r\n", "\n");
    let current = fs::read_to_string(out_abs)
        .map(|s| s.replace("\r\n", "\n"))
        .unwrap_or_default();
    if current != expected {
        let def_disp = policy
            .theme_def
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "theme_def".to_string());
        out.push(Diagnostic {
            rule_id: "warden/slint/theme-out-of-date".to_string(),
            check: "slint-theme".to_string(),
            severity: Severity::Error,
            message: format!(
                "Generated Slint theme `{}` is out of sync with `{def_disp}`; run `cargo warden theme generate`",
                out_rel.display()
            ),
            file: Some(out_rel.to_path_buf()),
            line: Some(1),
            column: None,
            fingerprint: format!("slint-theme-sync:{}", out_rel.to_string_lossy().replace('\\', "/")),
            metric_value: None,
            suppressed: false,
            suppression_reason: None,
        });
    }
    Ok(())
}

fn append_unused_token_warnings(
    policy: &SlintThemePolicy<'_>,
    out_rel: &Path,
    def: &ThemeDefinition,
    used_tokens: &BTreeSet<String>,
    out: &mut Vec<Diagnostic>,
) {
    let target_file = policy
        .theme_def
        .map(|p| normalize_rel_path(policy.base_dir, &resolve_against_base(policy.base_dir, p)))
        .unwrap_or_else(|| out_rel.to_path_buf());
    for token in defined_token_idents(def) {
        if token == "mode" || token == "dark" || used_tokens.contains(&token) {
            continue;
        }
        out.push(Diagnostic {
            rule_id: "warden/slint/unused-token".to_string(),
            check: "slint-theme".to_string(),
            severity: Severity::Warning,
            message: format!("Unused Slint design token `Theme.{token}`"),
            file: Some(target_file.clone()),
            line: None,
            column: None,
            fingerprint: format!("slint-unused-token:{token}"),
            metric_value: None,
            suppressed: false,
            suppression_reason: None,
        });
    }
}

pub fn check_theme_with_options(ui_dir: &Path, opts: &SlintThemeOptions<'_>) -> Result<()> {
    let diagnostics = collect_theme(ui_dir, opts)?;
    let active: Vec<&Diagnostic> = diagnostics
        .iter()
        .filter(|d| !d.suppressed && d.severity == Severity::Error)
        .collect();
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

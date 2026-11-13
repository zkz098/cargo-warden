//! Slint design token schema, W3C DTCG JSON/TOML loader, alias resolution, and `theme.slint` generator.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ThemeDefinition {
    /// Legacy static dark to light color mapping: dark_hex -> light_hex (generates `Theme.c<hex>`)
    pub static_map: BTreeMap<String, String>,
    /// Legacy accent color ramps (generates `Theme.c<key>`)
    pub accent_map: Vec<AccentDef>,
    /// Semantic color tokens (generates `Theme.<token>`)
    pub colors: BTreeMap<String, ColorTokenDef>,
    /// Spacing tokens (`<length>`, e.g. `sm = "8px"`)
    pub spacing: BTreeMap<String, String>,
    /// Corner radius tokens (`<length>`, e.g. `md = "8px"`)
    pub radius: BTreeMap<String, String>,
    /// Border width tokens (`<length>`, e.g. `thin = "1px"`)
    pub border_width: BTreeMap<String, String>,
    /// Animation / transition duration tokens (`<duration>`, e.g. `fast = "120ms"`)
    pub duration: BTreeMap<String, String>,
    /// Typography tokens (families, sizes, weights, and composite text styles)
    pub typography: TypographyTokens,
    /// Raw path->value lookup populated by DTCG imports so `{group.token}` aliases resolve cleanly
    #[serde(skip)]
    pub(crate) extra_aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColorTokenDef {
    Single(String),
    Dual { dark: String, light: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TypographyTokens {
    pub font_family: BTreeMap<String, String>,
    pub font_size: BTreeMap<String, String>,
    pub font_weight: BTreeMap<String, FontWeightValue>,
    pub styles: BTreeMap<String, TextStyleDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FontWeightValue {
    Number(f64),
    String(String),
}

impl FontWeightValue {
    fn as_alias_str(&self) -> String {
        match self {
            Self::Number(n) => format_weight_number(*n),
            Self::String(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TextStyleDef {
    pub font_size: String,
    pub font_weight: Option<FontWeightValue>,
    pub font_family: Option<String>,
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

fn format_weight_number(n: f64) -> String {
    if (n.fract()).abs() < 1e-6 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Normalize a Slint identifier for matching (`-` and `_` are interchangeable in Slint).
pub(crate) fn normalize_slint_ident(name: &str) -> String {
    name.trim().replace('_', "-")
}

fn to_slint_prop_name(prefix: &str, key: &str) -> String {
    let body = key.trim().replace(['.', '_'], "-");
    if prefix.is_empty() {
        body
    } else {
        format!("{prefix}-{body}")
    }
}

/// Load a `ThemeDefinition` from TOML or W3C DTCG JSON (`.json` / `.tokens.json` / `.tokens`).
pub fn load_theme_definition(path: &Path) -> Result<ThemeDefinition> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read theme definition {}", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".json") || name.ends_with(".tokens") {
        parse_dtcg_tokens(&content)
            .with_context(|| format!("parse W3C DTCG tokens {}", path.display()))
    } else {
        toml::from_str::<ThemeDefinition>(&content)
            .with_context(|| format!("parse TOML theme definition {}", path.display()))
    }
}

/// Parse a W3C Design Tokens Community Group (DTCG) JSON file into `ThemeDefinition`.
pub fn parse_dtcg_tokens(content: &str) -> Result<ThemeDefinition> {
    let root: JsonValue = serde_json::from_str(content)?;
    let obj = root
        .as_object()
        .context("DTCG root must be a JSON object")?;
    let mut def = ThemeDefinition::default();
    let mut path = Vec::new();
    collect_dtcg_node(obj, None, &mut path, &mut def)?;
    Ok(def)
}

fn collect_dtcg_node(
    obj: &serde_json::Map<String, JsonValue>,
    inherited_type: Option<&str>,
    path: &mut Vec<String>,
    def: &mut ThemeDefinition,
) -> Result<()> {
    let current_type = obj
        .get("$type")
        .and_then(JsonValue::as_str)
        .or(inherited_type);

    if let Some(val) = obj.get("$value") {
        insert_dtcg_token(path, current_type, val, obj.get("$extensions"), def)?;
        return Ok(());
    }

    for (k, child) in obj {
        if k.starts_with('$') {
            continue;
        }
        if let Some(child_obj) = child.as_object() {
            path.push(k.clone());
            collect_dtcg_node(child_obj, current_type, path, def)?;
            path.pop();
        }
    }
    Ok(())
}

fn insert_dtcg_token(
    path: &[String],
    token_type: Option<&str>,
    val: &JsonValue,
    extensions: Option<&JsonValue>,
    def: &mut ThemeDefinition,
) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    let full_key = path.join(".");
    let short_key = dtcg_short_key(path);
    match token_type {
        Some("color") => insert_dtcg_color(&full_key, &short_key, val, extensions, def)?,
        Some("dimension") => {
            let formatted = dtcg_dimension_string(val)?;
            def.extra_aliases.insert(full_key, formatted.clone());
            route_dimension_token(path, &short_key, formatted, def);
        }
        Some("duration") => {
            let formatted = dtcg_duration_string(val)?;
            def.extra_aliases.insert(full_key, formatted.clone());
            def.duration.insert(short_key, formatted);
        }
        Some("fontFamily") => {
            let fam = dtcg_font_family_string(val)?;
            def.extra_aliases.insert(full_key, fam.clone());
            let key = joined_strip_prefix(
                path,
                &[
                    "font.family",
                    "font-family",
                    "fontfamily",
                    "typography.family",
                    "font",
                ],
            )
            .unwrap_or_else(|| short_key.clone());
            def.typography.font_family.insert(key, fam);
        }
        Some("fontWeight") => {
            let w = dtcg_font_weight_value(val)?;
            def.extra_aliases.insert(full_key, w.as_alias_str());
            let key = joined_strip_prefix(
                path,
                &[
                    "font.weight",
                    "font-weight",
                    "fontweight",
                    "typography.weight",
                    "font",
                ],
            )
            .unwrap_or_else(|| short_key.clone());
            def.typography.font_weight.insert(key, w);
        }
        Some("typography") => {
            if let Some(map) = val.as_object() {
                let key = joined_strip_prefix(path, &["typography", "type", "text"])
                    .unwrap_or_else(|| short_key.clone());
                let style = TextStyleDef {
                    font_size: map
                        .get("fontSize")
                        .map(dtcg_dimension_string)
                        .transpose()?
                        .unwrap_or_else(|| "14px".to_string()),
                    font_weight: map
                        .get("fontWeight")
                        .map(dtcg_font_weight_value)
                        .transpose()?,
                    font_family: map
                        .get("fontFamily")
                        .map(dtcg_font_family_string)
                        .transpose()?,
                };
                def.typography.styles.insert(key, style);
            }
        }
        _ => {}
    }
    Ok(())
}

fn dtcg_short_key(path: &[String]) -> String {
    if path.len() > 1 {
        let head = path[0].to_ascii_lowercase();
        if matches!(
            head.as_str(),
            "color" | "colors" | "spacing" | "space" | "radius" | "duration" | "animation"
        ) {
            return path[1..].join(".");
        }
    }
    path.join(".")
}

fn route_dimension_token(path: &[String], short_key: &str, val: String, def: &mut ThemeDefinition) {
    let joined = path.join(".").to_ascii_lowercase();
    if joined.contains("radius") {
        let key = joined_strip_prefix(path, &["radius", "border-radius", "borderradius"])
            .unwrap_or_else(|| short_key.to_string());
        def.radius.insert(key, val);
    } else if joined.contains("border") {
        let key = joined_strip_prefix(path, &["border-width", "borderwidth", "border"])
            .unwrap_or_else(|| short_key.to_string());
        def.border_width.insert(key, val);
    } else if joined.contains("font") {
        let key = joined_strip_prefix(
            path,
            &["font.size", "fontsize", "font-size", "typography.size"],
        )
        .unwrap_or_else(|| short_key.to_string());
        def.typography.font_size.insert(key, val);
    } else {
        def.spacing.insert(short_key.to_string(), val);
    }
}

fn joined_strip_prefix(path: &[String], prefixes: &[&str]) -> Option<String> {
    let full = path.join(".");
    let lower = full.to_ascii_lowercase();
    for pref in prefixes {
        if let Some(rest) = lower.strip_prefix(pref) {
            let trimmed = rest.trim_start_matches(['.', '-', '_']);
            if !trimmed.is_empty() {
                return Some(full[full.len() - trimmed.len()..].to_string());
            }
        }
    }
    None
}

fn insert_dtcg_color(
    full_key: &str,
    short_key: &str,
    val: &JsonValue,
    extensions: Option<&JsonValue>,
    def: &mut ThemeDefinition,
) -> Result<()> {
    let primary = dtcg_color_string(val)?;
    def.extra_aliases
        .insert(full_key.to_string(), primary.clone());

    if let Some(modes) = extensions
        .and_then(|e| e.as_object())
        .and_then(|m| m.get("mode").or_else(|| m.get("modes")))
        .and_then(JsonValue::as_object)
        && let (Some(dark_v), Some(light_v)) = (modes.get("dark"), modes.get("light"))
    {
        def.colors.insert(
            short_key.to_string(),
            ColorTokenDef::Dual {
                dark: dtcg_color_string(dark_v)?,
                light: dtcg_color_string(light_v)?,
            },
        );
        return Ok(());
    }

    def.colors
        .insert(short_key.to_string(), ColorTokenDef::Single(primary));
    Ok(())
}

fn dtcg_color_string(val: &JsonValue) -> Result<String> {
    if let Some(s) = val.as_str() {
        return Ok(s.to_string());
    }
    let Some(obj) = val.as_object() else {
        bail!("invalid DTCG color value: {val}");
    };
    if let Some(hex) = obj.get("hex").and_then(JsonValue::as_str) {
        return Ok(hex.to_string());
    }
    if let Some(comps) = obj.get("components").and_then(JsonValue::as_array)
        && comps.len() >= 3
    {
        let r = component_to_u8(comps[0].as_f64().unwrap_or(0.0));
        let g = component_to_u8(comps[1].as_f64().unwrap_or(0.0));
        let b = component_to_u8(comps[2].as_f64().unwrap_or(0.0));
        if let Some(alpha) = obj.get("alpha").and_then(JsonValue::as_f64)
            && alpha < 0.999
        {
            let a = component_to_u8(alpha);
            return Ok(format!("#{r:02x}{g:02x}{b:02x}{a:02x}"));
        }
        return Ok(format!("#{r:02x}{g:02x}{b:02x}"));
    }
    bail!("unsupported DTCG color object: {val}")
}

fn component_to_u8(c: f64) -> u8 {
    (c.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn dtcg_dimension_string(val: &JsonValue) -> Result<String> {
    if let Some(s) = val.as_str() {
        return Ok(s.to_string());
    }
    if let Some(obj) = val.as_object()
        && let Some(num) = obj.get("value").and_then(JsonValue::as_f64)
    {
        let unit = obj.get("unit").and_then(JsonValue::as_str).unwrap_or("px");
        return Ok(format!("{}{}", format_weight_number(num), unit));
    }
    bail!("invalid DTCG dimension value: {val}")
}

fn dtcg_duration_string(val: &JsonValue) -> Result<String> {
    if let Some(s) = val.as_str() {
        return Ok(s.to_string());
    }
    if let Some(obj) = val.as_object()
        && let Some(num) = obj.get("value").and_then(JsonValue::as_f64)
    {
        let unit = obj.get("unit").and_then(JsonValue::as_str).unwrap_or("ms");
        return Ok(format!("{}{}", format_weight_number(num), unit));
    }
    bail!("invalid DTCG duration value: {val}")
}

fn dtcg_font_family_string(val: &JsonValue) -> Result<String> {
    if let Some(s) = val.as_str() {
        return Ok(s.to_string());
    }
    if let Some(arr) = val.as_array()
        && let Some(first) = arr.first().and_then(JsonValue::as_str)
    {
        return Ok(first.to_string());
    }
    bail!("invalid DTCG fontFamily value: {val}")
}

fn dtcg_font_weight_value(val: &JsonValue) -> Result<FontWeightValue> {
    if let Some(n) = val.as_f64() {
        return Ok(FontWeightValue::Number(n));
    }
    if let Some(s) = val.as_str() {
        return Ok(FontWeightValue::String(s.to_string()));
    }
    bail!("invalid DTCG fontWeight value: {val}")
}

fn build_alias_table(def: &ThemeDefinition) -> BTreeMap<String, String> {
    let mut table = def.extra_aliases.clone();
    for (k, v) in &def.colors {
        if let ColorTokenDef::Single(s) = v {
            table.insert(format!("colors.{k}"), s.clone());
            table.insert(format!("color.{k}"), s.clone());
        }
    }
    for (k, v) in &def.spacing {
        table.insert(format!("spacing.{k}"), v.clone());
        table.insert(format!("space.{k}"), v.clone());
    }
    for (k, v) in &def.radius {
        table.insert(format!("radius.{k}"), v.clone());
    }
    for (k, v) in &def.border_width {
        table.insert(format!("border_width.{k}"), v.clone());
        table.insert(format!("border-width.{k}"), v.clone());
        table.insert(format!("border.{k}"), v.clone());
    }
    for (k, v) in &def.duration {
        table.insert(format!("duration.{k}"), v.clone());
    }
    for (k, v) in &def.typography.font_family {
        table.insert(format!("typography.font_family.{k}"), v.clone());
        table.insert(format!("font.family.{k}"), v.clone());
    }
    for (k, v) in &def.typography.font_size {
        table.insert(format!("typography.font_size.{k}"), v.clone());
        table.insert(format!("font.size.{k}"), v.clone());
    }
    for (k, v) in &def.typography.font_weight {
        let s = v.as_alias_str();
        table.insert(format!("typography.font_weight.{k}"), s.clone());
        table.insert(format!("font.weight.{k}"), s);
    }
    table
}

pub(crate) fn resolve_alias(
    raw: &str,
    table: &BTreeMap<String, String>,
    stack: &mut Vec<String>,
) -> Result<String> {
    let trimmed = raw.trim();
    let Some(target) = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return Ok(trimmed.to_string());
    };
    let key = target.trim();
    if stack.iter().any(|seen| seen == key) {
        bail!(
            "Circular design token alias detected: {} -> {key}",
            stack.join(" -> ")
        );
    }
    let next = table
        .get(key)
        .with_context(|| format!("Unknown design token alias `{{{key}}}`"))?;
    stack.push(key.to_string());
    let resolved = resolve_alias(next, table, stack)?;
    stack.pop();
    Ok(resolved)
}

fn ensure_hash_hex(color: &str) -> String {
    let trimmed = color.trim();
    if trimmed.starts_with('#') || trimmed.starts_with("rgb") {
        trimmed.to_string()
    } else {
        format!("#{trimmed}")
    }
}

/// Return all normalized Slint property identifiers exported by `Theme` for a `ThemeDefinition`.
pub fn defined_token_idents(def: &ThemeDefinition) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    set.insert("mode".to_string());
    set.insert("dark".to_string());

    for k in def.static_map.keys() {
        set.insert(normalize_slint_ident(&format!("c{k}")));
    }
    for a in &def.accent_map {
        set.insert(normalize_slint_ident(&format!("c{}", a.key)));
    }
    for k in def.colors.keys() {
        set.insert(normalize_slint_ident(&to_slint_prop_name("", k)));
    }
    for k in def.spacing.keys() {
        set.insert(normalize_slint_ident(&to_slint_prop_name("space", k)));
    }
    for k in def.radius.keys() {
        set.insert(normalize_slint_ident(&to_slint_prop_name("radius", k)));
    }
    for k in def.border_width.keys() {
        set.insert(normalize_slint_ident(&to_slint_prop_name("border", k)));
    }
    for k in def.duration.keys() {
        set.insert(normalize_slint_ident(&to_slint_prop_name("duration", k)));
    }
    for k in def.typography.font_family.keys() {
        set.insert(normalize_slint_ident(&to_slint_prop_name("font-family", k)));
    }
    for k in def.typography.font_size.keys() {
        set.insert(normalize_slint_ident(&to_slint_prop_name("font-size", k)));
    }
    for k in def.typography.font_weight.keys() {
        set.insert(normalize_slint_ident(&to_slint_prop_name("font-weight", k)));
    }
    for k in def.typography.styles.keys() {
        set.insert(normalize_slint_ident(&to_slint_prop_name("type", k)));
    }
    set
}

/// Render `theme.slint` source text deterministically from `ThemeDefinition`.
pub fn render_theme(def: &ThemeDefinition) -> Result<String> {
    let table = build_alias_table(def);
    let mut out = String::new();
    out.push_str(
        r#"// Auto-generated by cargo-warden theme. Do not edit manually.
import { Palette } from "std-widgets.slint";

export enum ColorModePref { System, Light, Dark }
"#,
    );

    if !def.typography.styles.is_empty() {
        out.push_str(
            "\nexport struct TextStyle {\n    size: length,\n    weight: float,\n    family: string,\n}\n",
        );
    }

    out.push_str(
        r#"
export global Theme {
    in-out property <ColorModePref> mode: ColorModePref.System;
    in-out property <bool> dark: mode == ColorModePref.Dark
        || (mode == ColorModePref.System && Palette.color-scheme == ColorScheme.dark);

"#,
    );

    render_legacy_colors(def, &mut out);
    render_semantic_tokens(def, &table, &mut out)?;
    out.push_str("}\n");
    Ok(out)
}

fn render_legacy_colors(def: &ThemeDefinition, out: &mut String) {
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
}

fn render_semantic_tokens(
    def: &ThemeDefinition,
    table: &BTreeMap<String, String>,
    out: &mut String,
) -> Result<()> {
    let mut stack = Vec::new();
    if !def.colors.is_empty() {
        out.push_str("\n    // Semantic Colors\n");
        for (key, c) in &def.colors {
            let prop = to_slint_prop_name("", key);
            match c {
                ColorTokenDef::Single(raw) => {
                    let val = ensure_hash_hex(&resolve_alias(raw, table, &mut stack)?);
                    out.push_str(&format!("    in property <color> {prop}: {val};\n"));
                }
                ColorTokenDef::Dual { dark, light } => {
                    let d = ensure_hash_hex(&resolve_alias(dark, table, &mut stack)?);
                    let l = ensure_hash_hex(&resolve_alias(light, table, &mut stack)?);
                    out.push_str(&format!(
                        "    in property <color> {prop}: dark ? {d} : {l};\n"
                    ));
                }
            }
        }
    }

    render_simple_category("Spacing", "length", "space", &def.spacing, table, out)?;
    render_simple_category("Radius", "length", "radius", &def.radius, table, out)?;
    render_simple_category(
        "Border Widths",
        "length",
        "border",
        &def.border_width,
        table,
        out,
    )?;
    render_simple_category(
        "Durations",
        "duration",
        "duration",
        &def.duration,
        table,
        out,
    )?;
    render_typography_section(&def.typography, table, out)
}

fn render_simple_category(
    heading: &str,
    slint_ty: &str,
    prefix: &str,
    map: &BTreeMap<String, String>,
    table: &BTreeMap<String, String>,
    out: &mut String,
) -> Result<()> {
    if map.is_empty() {
        return Ok(());
    }
    let mut stack = Vec::new();
    out.push_str(&format!("\n    // {heading}\n"));
    for (k, raw) in map {
        let prop = to_slint_prop_name(prefix, k);
        let val = resolve_alias(raw, table, &mut stack)?;
        out.push_str(&format!("    in property <{slint_ty}> {prop}: {val};\n"));
    }
    Ok(())
}

fn render_typography_section(
    typo: &TypographyTokens,
    table: &BTreeMap<String, String>,
    out: &mut String,
) -> Result<()> {
    if typo.font_family.is_empty()
        && typo.font_size.is_empty()
        && typo.font_weight.is_empty()
        && typo.styles.is_empty()
    {
        return Ok(());
    }
    let mut stack = Vec::new();
    out.push_str("\n    // Typography\n");
    for (k, raw) in &typo.font_family {
        let prop = to_slint_prop_name("font-family", k);
        let val = resolve_alias(raw, table, &mut stack)?;
        out.push_str(&format!("    in property <string> {prop}: {:?};\n", val));
    }
    for (k, raw) in &typo.font_size {
        let prop = to_slint_prop_name("font-size", k);
        let val = resolve_alias(raw, table, &mut stack)?;
        out.push_str(&format!("    in property <length> {prop}: {val};\n"));
    }
    for (k, raw) in &typo.font_weight {
        let prop = to_slint_prop_name("font-weight", k);
        let val = resolve_alias(&raw.as_alias_str(), table, &mut stack)?;
        out.push_str(&format!("    in property <float> {prop}: {val};\n"));
    }
    for (k, style) in &typo.styles {
        let prop = to_slint_prop_name("type", k);
        let size = resolve_alias(&style.font_size, table, &mut stack)?;
        let weight = match &style.font_weight {
            Some(w) => resolve_alias(&w.as_alias_str(), table, &mut stack)?,
            None => "400".to_string(),
        };
        let family = match &style.font_family {
            Some(f) => resolve_alias(f, table, &mut stack)?,
            None => String::new(),
        };
        out.push_str(&format!(
            "    in property <TextStyle> {prop}: {{ size: {size}, weight: {weight}, family: {:?} }};\n",
            family
        ));
    }
    Ok(())
}

/// Generate `theme.slint` from definition.
pub fn generate_theme(def: &ThemeDefinition, out_file: &Path) -> Result<()> {
    let out = render_theme(def)?;
    if let Some(parent) = out_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out_file, out)?;
    println!("[slint-theme] generated {}", out_file.display());
    Ok(())
}

/// Load a hex-to-token mapping from a TOML or DTCG JSON theme definition file.
pub fn load_token_mapping(base_dir: &Path, theme_def: Option<&Path>) -> BTreeMap<String, String> {
    let mut mapping = BTreeMap::new();
    let Some(def_path) = theme_def else {
        return mapping;
    };
    let Ok(def) = load_theme_definition(&base_dir.join(def_path)) else {
        return mapping;
    };
    for k in def.static_map.keys() {
        mapping.insert(k.to_ascii_lowercase(), format!("Theme.c{k}"));
    }
    for a in &def.accent_map {
        mapping.insert(a.key.to_ascii_lowercase(), format!("Theme.c{}", a.key));
    }

    let table = build_alias_table(&def);
    let mut stack = Vec::new();
    for (k, c) in &def.colors {
        let prop = format!("Theme.{}", to_slint_prop_name("", k));
        let mut insert_if_free = |raw: &str| {
            if let Ok(resolved) = resolve_alias(raw, &table, &mut stack) {
                let hex = resolved.trim().trim_start_matches('#').to_ascii_lowercase();
                if matches!(hex.len(), 3 | 4 | 6 | 8)
                    && hex.chars().all(|ch| ch.is_ascii_hexdigit())
                {
                    mapping.entry(hex).or_insert_with(|| prop.clone());
                }
            }
        };
        match c {
            ColorTokenDef::Single(s) => insert_if_free(s),
            ColorTokenDef::Dual { dark, light } => {
                insert_if_free(dark);
                insert_if_free(light);
            }
        }
    }
    mapping
}

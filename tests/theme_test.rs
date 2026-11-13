use cargo_warden::checks::slint_theme::{apply_tokens, check_theme};
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_theme_check_and_apply() {
    let dir = tempdir().unwrap();
    let ui_dir = dir.path().join("ui");
    fs::create_dir_all(&ui_dir).unwrap();

    let comp = ui_dir.join("main.slint");
    fs::write(
        &comp,
        r#"
export component App inherits Window {
    background: #12151a;
    color: #ffffff;
}
"#,
    )
    .unwrap();

    // Whitelist only #ffffff, so #12151a should trigger violation
    let res = check_theme(&ui_dir, &["ffffff".to_string()]);
    assert!(res.is_err());

    // Apply tokens: replace #12151a with Theme.c12151a
    let mut mapping = BTreeMap::new();
    mapping.insert("12151a".to_string(), "Theme.c12151a".to_string());
    apply_tokens(&ui_dir, &mapping, &["ffffff".to_string()]).unwrap();

    let updated = fs::read_to_string(&comp).unwrap();
    assert!(updated.contains("Theme.c12151a"));
    assert!(updated.contains("import { Theme } from \"theme.slint\";"));
    assert!(updated.contains("#ffffff")); // Whitelisted color remains unchanged
}

#[test]
fn test_theme_generate_components_subdir_and_alpha_hex() {
    use cargo_warden::checks::slint_theme::{
        AccentDef, AccentHues, ThemeDefinition, generate_theme,
    };

    let dir = tempdir().unwrap();
    let ui_dir = dir.path().join("ui");
    let comp_dir = ui_dir.join("components");
    fs::create_dir_all(&comp_dir).unwrap();

    let theme_out = ui_dir.join("theme.slint");
    let mut def = ThemeDefinition::default();
    def.static_map
        .insert("12151a".to_string(), "f8fafc".to_string());
    def.accent_map.push(AccentDef {
        key: "primary".to_string(),
        dark: AccentHues {
            default: "38bdf8".to_string(),
            variants: BTreeMap::new(),
        },
        light: AccentHues {
            default: "0284c7".to_string(),
            variants: BTreeMap::new(),
        },
    });
    generate_theme(&def, &theme_out).unwrap();
    let theme_src = fs::read_to_string(&theme_out).unwrap();
    assert!(theme_src.contains("in property <color> c12151a: dark ? #12151a : #f8fafc;"));
    assert!(theme_src.contains("in property <color> cprimary: dark ? #38bdf8 : #0284c7;"));

    // Existing import at top + component inside components/ subdir + 8-digit uppercase hex
    let btn = comp_dir.join("btn.slint");
    fs::write(
        &btn,
        "import { Button } from \"std-widgets.slint\";\n\nexport component Btn {\n    background: #3B076466;\n    color: #FFFFFF;\n}\n",
    )
    .unwrap();

    // theme.slint itself is excluded from scanning
    assert!(check_theme(&ui_dir, &["ffffff".to_string()]).is_err());

    apply_tokens(&ui_dir, &BTreeMap::new(), &["ffffff".to_string()]).unwrap();
    let updated = fs::read_to_string(&btn).unwrap();
    assert!(updated.contains(
        "import { Button } from \"std-widgets.slint\";\nimport { Theme } from \"../theme.slint\";"
    ));
    assert!(updated.contains("Theme.c3b076466"));
    assert!(updated.contains("#FFFFFF"));
}

#[test]
fn test_theme_comments_strings_short_hex_rgb_and_deep_import() {
    use cargo_warden::checks::slint_theme::{
        SlintThemeOptions, apply_tokens_with_out_file, collect_theme,
    };

    let dir = tempdir().unwrap();
    let ui_dir = dir.path().join("ui");
    let deep_dir = ui_dir.join("pages/settings");
    fs::create_dir_all(&deep_dir).unwrap();

    // Comments and string literals should NOT trigger violations or get rewritten
    let clean = ui_dir.join("clean.slint");
    fs::write(
        &clean,
        r#"
// Example color #deadbf in comment
/* Multi-line block comment
   #badc0d rgb(1, 2, 3) */
export component Clean {
    property <string> hint: "Use #123456 or rgba(0, 0, 0, 0.5)";
    background: #fff; // whitelisted via "ffffff"
}
"#,
    )
    .unwrap();

    let diags_clean = collect_theme(
        &ui_dir,
        &SlintThemeOptions {
            base_dir: dir.path(),
            unchanged: &["ffffff".to_string()],
            respect_gitignore: false,
            changeset: None,
            emit_text: false,
        },
    )
    .unwrap();
    assert!(
        diags_clean.is_empty(),
        "unexpected violations in comments/strings: {diags_clean:?}"
    );

    // Real 3/4-digit hex + rgb/rgba in deep nested file should be caught
    let panel = deep_dir.join("panel.slint");
    fs::write(
        &panel,
        "export component Panel {\n    background: #abc;\n    border-color: rgba(10, 20, 30, 0.5);\n}\n",
    )
    .unwrap();

    let diags = collect_theme(
        &ui_dir,
        &SlintThemeOptions {
            base_dir: dir.path(),
            unchanged: &["ffffff".to_string()],
            respect_gitignore: false,
            changeset: None,
            emit_text: false,
        },
    )
    .unwrap();
    assert_eq!(diags.len(), 2, "{diags:?}");
    assert!(diags[0].message.contains("#abc"));
    assert!(diags[1].message.contains("rgba(10,20,30,0.5)"));

    // apply_tokens_with_out_file leaves clean.slint comments/strings untouched and computes ../../theme.slint
    apply_tokens_with_out_file(
        &ui_dir,
        &ui_dir.join("theme.slint"),
        &BTreeMap::new(),
        &["ffffff".to_string()],
    )
    .unwrap();

    let clean_after = fs::read_to_string(&clean).unwrap();
    assert!(clean_after.contains("#deadbf"));
    assert!(clean_after.contains("\"Use #123456 or rgba(0, 0, 0, 0.5)\""));

    let panel_after = fs::read_to_string(&panel).unwrap();
    assert!(panel_after.contains("import { Theme } from \"../../theme.slint\";"));
    assert!(panel_after.contains("Theme.cabc"));
}

#[test]
fn test_dtcg_json_tokens_aliases_generation_and_drift_check() {
    use cargo_warden::checks::slint_theme::{
        SlintThemePolicy, collect_theme_with_policy, generate_theme, load_theme_definition,
    };
    use cargo_warden::config::SlintRulesConfig;

    let dir = tempdir().unwrap();
    let ui_dir = dir.path().join("ui");
    let styles_dir = ui_dir.join("styles");
    fs::create_dir_all(&styles_dir).unwrap();

    let tokens_path = dir.path().join("design.tokens.json");
    fs::write(
        &tokens_path,
        r##"{
  "color": {
    "$type": "color",
    "brand": {
      "primary": {
        "$value": { "colorSpace": "srgb", "components": [0, 0.4, 0.8] },
        "$extensions": { "mode": { "dark": "#38bdf8", "light": "#0066cc" } }
      }
    },
    "surface": {
      "card": { "$value": "{color.brand.primary}" }
    }
  },
  "spacing": {
    "$type": "dimension",
    "sm": { "$value": { "value": 8, "unit": "px" } },
    "md": { "$value": "{spacing.sm}" }
  },
  "radius": {
    "$type": "dimension",
    "md": { "$value": { "value": 6, "unit": "px" } }
  },
  "duration": {
    "$type": "duration",
    "fast": { "$value": { "value": 150, "unit": "ms" } }
  },
  "font": {
    "family": {
      "sans": { "$type": "fontFamily", "$value": ["Inter", "sans-serif"] }
    },
    "size": {
      "body": { "$type": "dimension", "$value": { "value": 14, "unit": "px" } }
    },
    "weight": {
      "bold": { "$type": "fontWeight", "$value": 700 }
    }
  },
  "typography": {
    "heading": {
      "$type": "typography",
      "$value": {
        "fontFamily": "{font.family.sans}",
        "fontSize": "{font.size.body}",
        "fontWeight": "{font.weight.bold}"
      }
    }
  }
}"##,
    )
    .unwrap();

    let def = load_theme_definition(&tokens_path).unwrap();
    let custom_out = styles_dir.join("tokens.slint");
    generate_theme(&def, &custom_out).unwrap();

    let generated = fs::read_to_string(&custom_out).unwrap();
    assert!(generated.contains("export struct TextStyle"), "{generated}");
    assert!(
        generated.contains("in property <color> brand-primary: dark ? #38bdf8 : #0066cc;"),
        "{generated}"
    );
    assert!(
        generated.contains("in property <color> surface-card: #0066cc;"),
        "{generated}"
    );
    assert!(
        generated.contains("in property <length> space-sm: 8px;"),
        "{generated}"
    );
    assert!(
        generated.contains("in property <length> space-md: 8px;"),
        "{generated}"
    );
    assert!(
        generated.contains("in property <length> radius-md: 6px;"),
        "{generated}"
    );
    assert!(
        generated.contains("in property <duration> duration-fast: 150ms;"),
        "{generated}"
    );
    assert!(
        generated.contains("in property <string> font-family-sans: \"Inter\";"),
        "{generated}"
    );
    assert!(
        generated.contains("in property <TextStyle> type-heading:"),
        "{generated}"
    );

    // Component referencing generated tokens; custom out_file `ui/styles/tokens.slint` must be excluded
    fs::write(
        ui_dir.join("card.slint"),
        "import { Theme } from \"styles/tokens.slint\";\nexport component Card {\n    padding: Theme.space-sm;\n    background: Theme.brand_primary;\n}\n",
    )
    .unwrap();

    let rules = SlintRulesConfig {
        color: true,
        spacing: true,
        radius: true,
        typography: true,
        duration: true,
        ..SlintRulesConfig::default()
    };
    let policy = SlintThemePolicy {
        base_dir: dir.path(),
        out_file: std::path::Path::new("ui/styles/tokens.slint"),
        theme_def: Some(std::path::Path::new("design.tokens.json")),
        unchanged_colors: &[],
        exclude: &[],
        respect_gitignore: false,
        verify_generated: true,
        check_unknown_tokens: true,
        check_unused_tokens: false,
        rules: &rules,
        changeset: None,
        emit_text: false,
        parallel: false,
    };

    let clean = collect_theme_with_policy(&ui_dir, &policy).unwrap();
    assert!(clean.is_empty(), "expected clean check, got: {clean:?}");

    // Drift detection triggers when generated file differs from theme_def
    fs::write(&custom_out, "// edited manually\nexport global Theme {}\n").unwrap();
    let drift = collect_theme_with_policy(&ui_dir, &policy).unwrap();
    assert!(
        drift
            .iter()
            .any(|d| d.rule_id == "warden/slint/theme-out-of-date"),
        "expected drift diagnostic: {drift:?}"
    );
}

#[test]
fn test_slint_ui_spec_rules_grid_unknown_unused_and_suppressions() {
    use cargo_warden::checks::slint_theme::{
        SlintThemePolicy, collect_theme_with_policy, generate_theme, load_theme_definition,
    };
    use cargo_warden::config::SlintRulesConfig;
    use cargo_warden::report::Severity;

    let dir = tempdir().unwrap();
    let ui_dir = dir.path().join("ui");
    fs::create_dir_all(&ui_dir).unwrap();

    let def_path = dir.path().join("tokens.toml");
    fs::write(
        &def_path,
        r##"
[colors]
bg_surface = { dark = "#12151a", light = "#ffffff" }
unused_color = "#10b981"

[spacing]
sm = "8px"

[radius]
sm = "4px"

[duration]
fast = "120ms"

[typography.font_family]
sans = "Inter"

[typography.font_size]
body = "14px"

[typography.font_weight]
bold = 700
"##,
    )
    .unwrap();

    let def = load_theme_definition(&def_path).unwrap();
    generate_theme(&def, &ui_dir.join("theme.slint")).unwrap();

    fs::write(
        ui_dir.join("view.slint"),
        r#"
import { Theme } from "theme.slint";
export component View {
    in property <length> custom-card-gap: 13px;
    width: 320px;
    height: 200px;
    // warden:allow(slint-duration) -- unused suppression on clean line
    background: Theme.bg-surface;
    padding: 0px;
    padding-top: 8px;
    spacing: 13px;
    margin: 5px; // warden:allow(slint-spacing)
    border-radius: 6px; // warden:allow(slint-radius)
    font-size: 15px;
    font-family: "Arial";
    font-weight: 500;
    border-color: Theme.missing-token;
    animate opacity {
        duration: 250ms;
    }
}
"#,
    )
    .unwrap();

    let rules = SlintRulesConfig {
        color: true,
        spacing: true,
        radius: true,
        typography: true,
        duration: true,
        spacing_grid: Some(4),
        ..SlintRulesConfig::default()
    };

    let diags = collect_theme_with_policy(
        &ui_dir,
        &SlintThemePolicy {
            base_dir: dir.path(),
            out_file: std::path::Path::new("ui/theme.slint"),
            theme_def: Some(std::path::Path::new("tokens.toml")),
            unchanged_colors: &[],
            exclude: &[],
            respect_gitignore: false,
            verify_generated: true,
            check_unknown_tokens: true,
            check_unused_tokens: true,
            rules: &rules,
            changeset: None,
            emit_text: false,
            parallel: false,
        },
    )
    .unwrap();

    let active_errors: Vec<_> = diags
        .iter()
        .filter(|d| !d.suppressed && d.severity == Severity::Error)
        .map(|d| d.rule_id.as_str())
        .collect();
    assert_eq!(
        active_errors
            .iter()
            .filter(|r| **r == "warden/slint/spacing")
            .count(),
        1,
        "only `spacing: 13px;` should fail spacing (0px allowed, 8px on 4px grid, margin suppressed, width & property decl ignored): {diags:?}"
    );
    assert!(
        active_errors.contains(&"warden/slint/unknown-token"),
        "{diags:?}"
    );
    assert!(
        active_errors.contains(&"warden/slint/duration"),
        "{diags:?}"
    );
    assert_eq!(
        active_errors
            .iter()
            .filter(|r| **r == "warden/slint/typography")
            .count(),
        3,
        "font-size, font-family, and font-weight should fail: {diags:?}"
    );

    assert!(
        diags
            .iter()
            .any(|d| d.suppressed && d.rule_id == "warden/slint/radius"),
        "expected suppressed border-radius diagnostic: {diags:?}"
    );
    assert!(
        diags.iter().any(|d| {
            d.severity == Severity::Warning
                && d.rule_id == "warden/slint/unused-token"
                && d.message.contains("unused-color")
        }),
        "expected unused-token warning for unused_color: {diags:?}"
    );
    assert!(
        diags.iter().any(|d| {
            d.severity == Severity::Warning
                && d.rule_id == "warden/unused-suppression"
                && d.message.contains("slint-duration")
        }),
        "expected unused-suppression warning for unused slint-duration allow: {diags:?}"
    );
}

#[test]
fn test_cli_theme_generate_dtcg_and_check_all_rules() {
    use std::process::Command;

    let bin = env!("CARGO_BIN_EXE_cargo-warden");
    let dir = tempdir().unwrap();
    let ui_dir = dir.path().join("ui");
    fs::create_dir_all(&ui_dir).unwrap();

    fs::write(
        dir.path().join("tokens.tokens.json"),
        r##"{
  "spacing": { "$type": "dimension", "md": { "$value": { "value": 16, "unit": "px" } } },
  "color": { "$type": "color", "accent": { "$value": "#0ea5e9" } }
}"##,
    )
    .unwrap();
    fs::write(
        dir.path().join("warden.toml"),
        r#"
[slint_theme]
ui_dir = "ui"
out_file = "ui/theme.slint"
theme_def = "tokens.tokens.json"
"#,
    )
    .unwrap();
    fs::write(
        ui_dir.join("app.slint"),
        "import { Theme } from \"theme.slint\";\nexport component App {\n    background: Theme.accent;\n    padding: 12px;\n}\n",
    )
    .unwrap();

    let gen_out = Command::new(bin)
        .current_dir(dir.path())
        .args(["theme", "generate"])
        .output()
        .unwrap();
    assert!(
        gen_out.status.success(),
        "theme generate failed: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );
    assert!(ui_dir.join("theme.slint").is_file());

    let chk_out = Command::new(bin)
        .current_dir(dir.path())
        .args([
            "--format",
            "github-actions",
            "theme",
            "check",
            "--all-rules",
            "--unknown-tokens",
            "--unused-tokens",
            "--verify-generated",
        ])
        .output()
        .unwrap();
    assert!(!chk_out.status.success(), "expected spacing failure");
    let stdout = String::from_utf8_lossy(&chk_out.stdout);
    assert!(stdout.contains("::error"), "{stdout}");
    assert!(stdout.contains("warden/slint/spacing"), "{stdout}");
    assert!(stdout.contains("::warning"), "{stdout}");
    assert!(stdout.contains("warden/slint/unused-token"), "{stdout}");
}

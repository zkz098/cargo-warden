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

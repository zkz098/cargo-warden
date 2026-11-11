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

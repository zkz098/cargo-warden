use cargo_warden::checks::i18n_po::{check_i18n, sync_i18n};
use std::fs;
use tempfile::tempdir;

#[test]
fn test_i18n_generate_and_check() {
    let dir = tempdir().unwrap();
    let lang_dir = dir.path().join("lang");
    let en_dir = lang_dir.join("en").join("LC_MESSAGES");
    let zh_dir = lang_dir.join("zh_CN").join("LC_MESSAGES");
    fs::create_dir_all(&en_dir).unwrap();
    fs::create_dir_all(&zh_dir).unwrap();

    let en_po = r#"
msgid "Download"
msgstr "Download"

msgid "Cancel"
msgstr "Cancel"
"#;

    let zh_po = r#"
msgid "Download"
msgstr "下载"

msgid "Cancel"
msgstr "取消"
"#;

    fs::write(en_dir.join("app.po"), en_po).unwrap();
    fs::write(zh_dir.join("app.po"), zh_po).unwrap();

    let out_file = dir.path().join("catalog.rs");
    let locales = vec!["zh_CN".to_string(), "en".to_string()];

    // Generate catalog
    sync_i18n(&lang_dir, "app", &locales, &out_file).unwrap();

    assert!(out_file.is_file());
    let catalog_src = fs::read_to_string(&out_file).unwrap();
    assert!(catalog_src.contains("Download"));
    assert!(catalog_src.contains("下载"));

    // Check should pass
    assert!(check_i18n(&lang_dir, "app", &locales, &out_file).is_ok());

    // Modify a PO file without updating catalog -> check should fail
    fs::write(zh_dir.join("app.po"), zh_po.replace("取消", "放弃")).unwrap();
    assert!(check_i18n(&lang_dir, "app", &locales, &out_file).is_err());
}

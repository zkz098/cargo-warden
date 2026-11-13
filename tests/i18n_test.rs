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

#[test]
fn test_i18n_multiline_escapes_fallback_crlf_and_errors() {
    let dir = tempdir().unwrap();
    let lang_dir = dir.path().join("lang");
    let en_dir = lang_dir.join("en").join("LC_MESSAGES");
    let zh_dir = lang_dir.join("zh_CN").join("LC_MESSAGES");
    fs::create_dir_all(&en_dir).unwrap();
    fs::create_dir_all(&zh_dir).unwrap();

    let en_po = r#"
# Header should be skipped
msgid ""
msgstr ""
"Content-Type: text/plain; charset=UTF-8\n"

msgid "Hello\n"
"World\t\"!\"\\"
msgstr "Hello\nWorld\t\"!\"\\"

msgid "Untranslated"
msgstr ""
"#;

    let zh_po = r#"
msgid "Hello\nWorld\t\"!\"\\"
msgstr "你好\n世界\t\"!\"\\"
"#;

    fs::write(en_dir.join("app.po"), en_po).unwrap();
    fs::write(zh_dir.join("app.po"), zh_po).unwrap();

    let out_file = dir.path().join("nested/catalog.rs");
    let locales = vec!["en".to_string(), "zh_CN".to_string()];

    sync_i18n(&lang_dir, "app", &locales, &out_file).unwrap();
    let generated = fs::read_to_string(&out_file).unwrap();
    assert!(generated.contains("你好\\n世界\\t\\\"!\\\"\\\\"));
    // Empty msgstr or missing entry in zh_CN falls back to msgid
    assert!(generated.contains(
        "Entry { msgid: \"Untranslated\", translations: [\"Untranslated\", \"Untranslated\"] }"
    ));

    // CRLF normalization should still pass check_i18n
    fs::write(&out_file, generated.replace('\n', "\r\n")).unwrap();
    assert!(check_i18n(&lang_dir, "app", &locales, &out_file).is_ok());

    // Empty locales errors
    assert!(check_i18n(&lang_dir, "app", &[], &out_file).is_err());

    // Malformed PO string errors
    fs::write(en_dir.join("bad.po"), "msgid \"unclosed\nmsgstr \"ok\"\n").unwrap();
    assert!(check_i18n(&lang_dir, "bad", &["en".to_string()], &out_file).is_err());
}

#[test]
fn test_po_entries_msgctxt_plural_fuzzy_and_placeholders() {
    use cargo_warden::checks::i18n_po::{I18nOptions, collect_i18n, parse_po_entries};

    let dir = tempdir().unwrap();
    let lang_dir = dir.path().join("lang");
    let zh_dir = lang_dir.join("zh_CN").join("LC_MESSAGES");
    let en_dir = lang_dir.join("en").join("LC_MESSAGES");
    fs::create_dir_all(&zh_dir).unwrap();
    fs::create_dir_all(&en_dir).unwrap();

    let zh_po_path = zh_dir.join("app.po");
    fs::write(
        &zh_po_path,
        r#"
#, fuzzy, rust-format
msgctxt "menu"
msgid "Open"
msgstr "打开"

msgid "Hello {name} ({})"
msgstr "你好"

msgid "1 item"
msgid_plural "{} items"
msgstr[0] "{} 项"

msgid "EmptyKey"
msgstr ""
"#,
    )
    .unwrap();

    fs::write(
        en_dir.join("app.po"),
        r#"
msgctxt "menu"
msgid "Open"
msgstr "Open"

msgid "Hello {name} ({})"
msgstr "Hello {name} ({})"

msgid "1 item"
msgid_plural "{} items"
msgstr[0] "1 item"
msgstr[1] "{} items"

msgid "EmptyKey"
msgstr "EmptyKey"

msgid "OnlyInEn"
msgstr "OnlyInEn"
"#,
    )
    .unwrap();

    let entries = parse_po_entries(&zh_po_path).unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].msgctxt.as_deref(), Some("menu"));
    assert!(entries[0].fuzzy);
    assert_eq!(entries[2].msgid_plural.as_deref(), Some("{} items"));
    assert_eq!(entries[2].msgstr_plural, vec!["{} 项"]);

    let out_file = dir.path().join("catalog.rs");
    sync_i18n(
        &lang_dir,
        "app",
        &["zh_CN".to_string(), "en".to_string()],
        &out_file,
    )
    .unwrap();

    let empty_dirs = Vec::new();
    let empty_macros = Vec::new();
    let locales = vec!["zh_CN".to_string(), "en".to_string()];

    // Non-strict: catches placeholder mismatch ("Hello {name} ({})" vs "你好")
    let non_strict_diags = collect_i18n(&I18nOptions {
        base_dir: dir.path(),
        lang_dir: &lang_dir,
        domain: "app",
        locales: &locales,
        out_file: &out_file,
        source_dirs: &empty_dirs,
        tr_macros: &empty_macros,
        strict: false,
        check_placeholders: true,
        check_unused: false,
        respect_gitignore: false,
        changeset: None,
        emit_text: false,
    })
    .unwrap();
    assert_eq!(non_strict_diags.len(), 1);
    assert_eq!(non_strict_diags[0].rule_id, "warden/i18n-placeholder");

    // Strict: also catches fuzzy, empty msgstr, and cross-locale missing "OnlyInEn"
    let strict_diags = collect_i18n(&I18nOptions {
        base_dir: dir.path(),
        lang_dir: &lang_dir,
        domain: "app",
        locales: &locales,
        out_file: &out_file,
        source_dirs: &empty_dirs,
        tr_macros: &empty_macros,
        strict: true,
        check_placeholders: true,
        check_unused: false,
        respect_gitignore: false,
        changeset: None,
        emit_text: false,
    })
    .unwrap();
    let rules: Vec<&str> = strict_diags.iter().map(|d| d.rule_id.as_str()).collect();
    assert!(rules.contains(&"warden/i18n-fuzzy"), "{rules:?}");
    assert!(rules.contains(&"warden/i18n-empty"), "{rules:?}");
    assert!(rules.contains(&"warden/i18n-placeholder"), "{rules:?}");
    assert!(rules.contains(&"warden/i18n-missing-key"), "{rules:?}");
}

#[test]
fn test_source_to_po_coverage_and_unused_detection() {
    use cargo_warden::checks::i18n_po::{I18nOptions, collect_i18n};

    let dir = tempdir().unwrap();
    let ui_dir = dir.path().join("ui");
    let src_dir = dir.path().join("src");
    let lang_dir = dir.path().join("lang/en/LC_MESSAGES");
    fs::create_dir_all(&ui_dir).unwrap();
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&lang_dir).unwrap();

    fs::write(
        ui_dir.join("app.slint"),
        r#"
// @tr("IgnoredComment")
export component App {
    Text { text: @tr("menu" => "Open"); }
    Text { text: @tr("MissingSlint"); }
    Text { text: @tr("1 file" | "{} files" % 2); }
}
"#,
    )
    .unwrap();

    fs::write(
        src_dir.join("main.rs"),
        r#"
fn tr(s: &str) -> &str { s }
macro_rules! t { ($e:expr) => { $e }; }
fn main() {
    let _ = tr("Save");
    let _ = t!("MissingRust");
}
#[cfg(test)]
mod tests {
    #[test]
    fn ignored() {
        let _ = super::tr("TestOnlyIgnored");
    }
}
"#,
    )
    .unwrap();

    fs::write(
        lang_dir.join("app.po"),
        r#"
msgctxt "menu"
msgid "Open"
msgstr "Open"

msgid "1 file"
msgid_plural "{} files"
msgstr[0] "1 file"
msgstr[1] "{} files"

msgid "Save"
msgstr "Save"

msgid "DeadObsoleteKey"
msgstr "Dead"
"#,
    )
    .unwrap();

    let out_file = src_dir.join("catalog.rs");
    let locales = vec!["en".to_string()];
    sync_i18n(&dir.path().join("lang"), "app", &locales, &out_file).unwrap();

    let source_dirs = vec![
        std::path::PathBuf::from("ui"),
        std::path::PathBuf::from("src"),
    ];
    let tr_macros = vec!["tr".to_string(), "t".to_string()];
    let diags = collect_i18n(&I18nOptions {
        base_dir: dir.path(),
        lang_dir: &dir.path().join("lang"),
        domain: "app",
        locales: &locales,
        out_file: &out_file,
        source_dirs: &source_dirs,
        tr_macros: &tr_macros,
        strict: false,
        check_placeholders: true,
        check_unused: true,
        respect_gitignore: false,
        changeset: None,
        emit_text: false,
    })
    .unwrap();

    let missing_msgs: Vec<&str> = diags
        .iter()
        .filter(|d| d.rule_id == "warden/i18n-missing-key")
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(missing_msgs.len(), 2, "{diags:?}");
    assert!(missing_msgs.iter().any(|m| m.contains("MissingSlint")));
    assert!(missing_msgs.iter().any(|m| m.contains("MissingRust")));
    assert!(!missing_msgs.iter().any(|m| m.contains("IgnoredComment")));
    assert!(!missing_msgs.iter().any(|m| m.contains("TestOnlyIgnored")));

    let unused: Vec<&str> = diags
        .iter()
        .filter(|d| d.rule_id == "warden/i18n-unused-key")
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(unused.len(), 1, "{diags:?}");
    assert!(unused[0].contains("DeadObsoleteKey"));
}

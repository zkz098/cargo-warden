use cargo_warden::config::WardenConfig;
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn test_load_defaults_when_no_config_present() {
    let dir = tempdir().unwrap();
    let cfg = WardenConfig::load(dir.path()).unwrap();
    assert!(cfg.dag.is_none());
    assert!(cfg.ci_parity.is_none());
    assert!(cfg.slint_theme.is_none());
    assert!(cfg.i18n.is_none());
    assert!(cfg.complexity.is_none());
    assert!(cfg.file_length.is_none());
    assert!(cfg.gate.is_none());
}

#[test]
fn test_warden_toml_takes_precedence_over_dotfile_and_cargo_metadata() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("warden.toml"), "[complexity]\nmax = 12\n").unwrap();
    fs::write(dir.path().join(".warden.toml"), "[complexity]\nmax = 99\n").unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\n[workspace.metadata.warden.complexity]\nmax = 55\n",
    )
    .unwrap();

    let cfg = WardenConfig::load(dir.path()).unwrap();
    let comp = cfg.complexity.expect("complexity section should load");
    assert_eq!(comp.max, 12);
    // Partial fields inherit #[serde(default)] values
    assert!(comp.ignore_tests);
    assert_eq!(comp.paths, vec![PathBuf::from(".")]);
}

#[test]
fn test_dot_warden_toml_used_when_warden_toml_absent() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join(".warden.toml"),
        "[file_length]\nmax_lines = 450\n",
    )
    .unwrap();

    let cfg = WardenConfig::load(dir.path()).unwrap();
    let fl = cfg.file_length.expect("file_length section should load");
    assert_eq!(fl.max_lines, 450);
    assert_eq!(fl.extensions, vec!["rs", "slint"]);
}

#[test]
fn test_cargo_workspace_metadata_fallback() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        r#"
[workspace]
members = []

[workspace.metadata.warden.dag]
roots = ["crates/core/src"]
forbid_imports = ["bootstrap"]
"#,
    )
    .unwrap();

    let cfg = WardenConfig::load(dir.path()).unwrap();
    let dag = cfg.dag.expect("dag metadata should load");
    assert_eq!(dag.roots, vec![PathBuf::from("crates/core/src")]);
    assert_eq!(dag.forbid_imports, vec!["bootstrap"]);
}

#[test]
fn test_invalid_toml_returns_contextual_error() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("warden.toml");
    fs::write(&path, "[complexity\nmax = \"not-a-number\"\n").unwrap();

    let err = WardenConfig::load(dir.path()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("Failed to parse TOML"));
    assert!(msg.contains("warden.toml"));
}

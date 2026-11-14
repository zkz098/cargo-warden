use cargo_warden::checks::manifest::{ManifestOptions, collect_manifest};
use cargo_warden::config::ManifestConfig;
use cargo_warden::report::Severity;
use std::fs;

#[test]
fn test_manifest_inheritance_deps_and_features() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let outside = dir.path().join("outside-crate");
    fs::create_dir_all(root.join("crates/core/src")).unwrap();
    fs::create_dir_all(outside.join("src")).unwrap();
    fs::write(
        outside.join("Cargo.toml"),
        "[package]\nname = \"ext\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(outside.join("src/lib.rs"), "").unwrap();

    fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = ["crates/core"]

[workspace.package]
edition = "2024"
license = "MIT"

[workspace.dependencies]
serde = "1.0"
"#,
    )
    .unwrap();

    fs::write(
        root.join("crates/core/Cargo.toml"),
        r#"[package]
name = "ws-core"
version = "0.1.0"
edition = "2021"

[features]
json = []

[dependencies]
serde = "1.0"
evil = { git = "https://example.com/evil.git" }
ext = { path = "../../../outside-crate" }
"#,
    )
    .unwrap();

    fs::write(
        root.join("crates/core/src/lib.rs"),
        r#"
#[cfg(feature = "json")]
pub fn ok() {}

#[cfg(feature = "typo_feat")]
pub fn bad() {}
"#,
    )
    .unwrap();

    let cfg = ManifestConfig {
        inherit_fields: vec!["edition".to_string(), "license".to_string()],
        enforce_workspace_deps: true,
        forbid_git_deps: true,
        forbid_external_path_deps: true,
        allow_git_deps: Vec::new(),
        ignore_crates: Vec::new(),
        check_features: true,
    };

    let diags = collect_manifest(&ManifestOptions {
        base_dir: &root,
        config: &cfg,
        changeset: None,
        emit_text: false,
    })
    .unwrap();

    let active: Vec<_> = diags
        .iter()
        .filter(|d| !d.suppressed && d.severity == Severity::Error)
        .collect();
    let rules: Vec<&str> = active.iter().map(|d| d.rule_id.as_str()).collect();
    assert_eq!(
        rules
            .iter()
            .filter(|r| **r == "warden/manifest/workspace-inherit")
            .count(),
        2,
        "expected 2 inherit errors (edition override + missing license): {active:#?}"
    );
    assert!(
        rules.contains(&"warden/manifest/workspace-dep"),
        "{rules:?}"
    );
    assert!(rules.contains(&"warden/manifest/git-dep"), "{rules:?}");
    assert!(
        rules.contains(&"warden/manifest/external-path-dep"),
        "{rules:?}"
    );
    assert!(
        rules.contains(&"warden/manifest/undeclared-feature"),
        "{rules:?}"
    );
    assert_eq!(active.len(), 6, "{active:#?}");
}

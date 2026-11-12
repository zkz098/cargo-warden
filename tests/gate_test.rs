use cargo_warden::config::{GateConfig, TestGroupConfig};
use cargo_warden::gate::runner::{Step, build_filtered_steps, build_steps, scrub_cargo_metadata};
use std::process::Command;

#[test]
fn test_build_steps_quick_vs_full_profile() {
    let cfg = GateConfig::default();

    let quick = build_steps(&cfg, false);
    let quick_names: Vec<&str> = quick.iter().map(|s| s.name).collect();
    assert_eq!(quick_names, vec!["fmt", "clippy", "test", "doc"]);

    let doc_step = quick.iter().find(|s| s.name == "doc").unwrap();
    assert!(doc_step.env.contains(&("RUSTDOCFLAGS", "-D warnings")));

    let full = build_steps(&cfg, true);
    let full_names: Vec<&str> = full.iter().map(|s| s.name).collect();
    assert_eq!(
        full_names,
        vec!["fmt", "clippy", "test", "doc", "machete", "deny", "audit"]
    );
}

#[test]
fn test_build_steps_with_test_groups_and_missing_toolchain_fallback() {
    let cfg = GateConfig {
        rustfmt_toolchain: Some("9.99.99-nonexistent".to_string()),
        test_groups: vec![TestGroupConfig {
            name: "engine".to_string(),
            packages: vec!["core-a".to_string(), "core-b".to_string()],
            features: vec!["feat1".to_string(), "feat2".to_string()],
        }],
        ..GateConfig::default()
    };

    let steps = build_steps(&cfg, false);
    let fmt_step = steps.iter().find(|s| s.name == "fmt").unwrap();
    // Uninstalled pinned toolchain falls back to default cargo fmt
    assert_eq!(fmt_step.args, vec!["fmt", "--all", "--", "--check"]);

    let test_step = steps.iter().find(|s| s.name == "test").unwrap();
    let rendered = test_step.rendered();
    assert!(rendered.contains("-p core-a"));
    assert!(rendered.contains("-p core-b"));
    assert!(rendered.contains("--features feat1,feat2"));
}

#[test]
fn test_step_rendered_includes_env_and_args() {
    let mut step = Step::cargo("doc", &["doc", "--no-deps"]);
    step.env.push(("RUSTDOCFLAGS", "-D warnings"));
    assert_eq!(
        step.rendered(),
        "RUSTDOCFLAGS=-D warnings cargo doc --no-deps"
    );
}

#[test]
fn test_scrub_cargo_metadata_removes_cargo_env_vars() {
    let mut cmd = Command::new("cargo");
    scrub_cargo_metadata(&mut cmd);

    let removed_keys: Vec<String> = cmd
        .get_envs()
        .filter_map(|(k, v)| {
            if v.is_none() {
                Some(k.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();

    // During `cargo test`, Cargo always sets CARGO_MANIFEST_DIR and CARGO_PKG_NAME
    assert!(removed_keys.iter().any(|k| k == "CARGO_MANIFEST_DIR"));
    assert!(removed_keys.iter().any(|k| k == "CARGO_PKG_NAME"));
}

#[test]
fn test_build_filtered_steps_and_custom_args() {
    let cfg = GateConfig {
        clippy_args: vec![
            "--all-targets".to_string(),
            "--".to_string(),
            "-D".to_string(),
            "warnings".to_string(),
        ],
        doc_args: vec!["--no-deps".to_string(), "--all-features".to_string()],
        skip_steps: vec!["audit".to_string()],
        ..GateConfig::default()
    };

    let filtered = build_filtered_steps(
        &cfg,
        true,
        &["clippy".to_string(), "doc".to_string(), "audit".to_string()],
        &[],
    );
    let names: Vec<&str> = filtered.iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["clippy", "doc"]);

    assert_eq!(
        filtered[0].args,
        vec!["clippy", "--all-targets", "--", "-D", "warnings"]
    );
    assert_eq!(filtered[1].args, vec!["doc", "--no-deps", "--all-features"]);

    let skipped = build_filtered_steps(&cfg, false, &[], &["test".to_string()]);
    let skipped_names: Vec<&str> = skipped.iter().map(|s| s.name).collect();
    assert_eq!(skipped_names, vec!["fmt", "clippy", "doc"]);
}

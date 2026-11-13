use cargo_warden::config::{CustomStepConfig, GateConfig, TestGroupConfig, WardenConfig};
use cargo_warden::gate::runner::{
    GateRunOptions, Step, build_all_steps, build_filtered_all_steps, build_filtered_steps,
    build_steps, run_gate_with_static_options, scrub_cargo_metadata,
};
use std::collections::BTreeMap;
use std::process::Command;
use tempfile::tempdir;

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

#[test]
fn test_build_all_steps_custom_steps_and_fix_mode() {
    let mut env = BTreeMap::new();
    env.insert("CUSTOM_ENV".to_string(), "1".to_string());

    let cfg = GateConfig {
        clippy_args: vec![
            "--all-targets".to_string(),
            "--".to_string(),
            "-D".to_string(),
            "warnings".to_string(),
        ],
        custom_steps: vec![
            CustomStepConfig {
                name: "rust-ver".to_string(),
                program: "rustc".to_string(),
                args: vec!["--version".to_string()],
                env,
                read_only: true,
                fix_args: Some(vec!["-Vv".to_string()]),
                ..CustomStepConfig::default()
            },
            CustomStepConfig {
                name: "shell-check".to_string(),
                command: Some("echo hi && echo ok".to_string()),
                full_only: true,
                read_only: true,
                fix_command: Some("echo fixing".to_string()),
                ..CustomStepConfig::default()
            },
        ],
        ..GateConfig::default()
    };

    // Quick non-fix: built-in steps + rust-ver, excluding full_only shell-check
    let quick = build_all_steps(&cfg, false, false);
    let quick_names: Vec<&str> = quick.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        quick_names,
        vec!["fmt", "clippy", "test", "doc", "rust-ver"]
    );
    assert!(quick[0].read_only); // fmt
    assert!(!quick[1].read_only); // clippy
    assert!(quick[4].read_only); // rust-ver
    assert_eq!(quick[4].args, vec!["--version"]);
    assert_eq!(
        quick[4].env,
        vec![("CUSTOM_ENV".to_string(), "1".to_string())]
    );

    // Full non-fix includes shell-check with shell wrapping
    let full = build_all_steps(&cfg, true, false);
    let shell_step = full.iter().find(|s| s.name == "shell-check").unwrap();
    if cfg!(windows) {
        assert_eq!(shell_step.program, "cmd");
    } else {
        assert_eq!(shell_step.program, "sh");
        assert_eq!(shell_step.args, vec!["-c", "echo hi && echo ok"]);
    }

    // --fix mode places clippy --fix BEFORE fmt --all, disables read_only on fixed custom steps
    let fixed = build_all_steps(&cfg, true, true);
    let fix_names: Vec<&str> = fixed.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(fix_names[0], "clippy");
    assert_eq!(fix_names[1], "fmt");
    assert_eq!(
        fixed[0].args,
        vec![
            "clippy",
            "--all-targets",
            "--fix",
            "--allow-dirty",
            "--allow-staged",
            "--",
            "-D",
            "warnings"
        ]
    );
    assert!(!fixed[0].read_only);
    assert!(!fixed[1].read_only);
    assert!(!fixed[1].args.iter().any(|a| a == "--check"));

    let rust_fix = fixed.iter().find(|s| s.name == "rust-ver").unwrap();
    assert_eq!(rust_fix.args, vec!["-Vv"]);
    assert!(!rust_fix.read_only);

    let shell_fix = fixed.iter().find(|s| s.name == "shell-check").unwrap();
    assert_eq!(shell_fix.program, "echo");
    assert_eq!(shell_fix.args, vec!["fixing"]);
    assert!(!shell_fix.read_only);

    // Filtering works on custom step names too
    let filtered = build_filtered_all_steps(
        &cfg,
        true,
        false,
        &["shell-check".to_string(), "rust-ver".to_string()],
        &["rust-ver".to_string()],
    );
    assert_eq!(
        filtered.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["shell-check"]
    );
}

#[test]
fn test_run_gate_custom_steps_only_skip_fail_and_parallel_batches() {
    let dir = tempdir().unwrap();
    let cfg = WardenConfig {
        gate: Some(GateConfig {
            include_checks: false,
            custom_steps: vec![
                CustomStepConfig {
                    name: "ok-one".to_string(),
                    program: "cargo".to_string(),
                    args: vec!["--version".to_string()],
                    read_only: true,
                    ..CustomStepConfig::default()
                },
                CustomStepConfig {
                    name: "ok-two".to_string(),
                    program: "rustc".to_string(),
                    args: vec!["--version".to_string()],
                    read_only: true,
                    ..CustomStepConfig::default()
                },
                CustomStepConfig {
                    name: "bad-step".to_string(),
                    program: "cargo".to_string(),
                    args: vec!["nonexistent-warden-subcommand-xyz".to_string()],
                    read_only: true,
                    ..CustomStepConfig::default()
                },
            ],
            ..GateConfig::default()
        }),
        ..WardenConfig::default()
    };

    // Pass when selecting the two read-only custom steps in parallel
    let pass_opts = GateRunOptions {
        skip_checks: true,
        parallel: Some(true),
        only: vec!["ok-one".to_string(), "ok-two".to_string()],
        ..GateRunOptions::default()
    };
    run_gate_with_static_options(dir.path(), &cfg, &pass_opts, &Default::default()).unwrap();

    // Fail when bad-step runs
    let fail_opts = GateRunOptions {
        skip_checks: true,
        parallel: Some(false),
        only: vec!["bad-step".to_string()],
        ..GateRunOptions::default()
    };
    let err = run_gate_with_static_options(dir.path(), &cfg, &fail_opts, &Default::default())
        .unwrap_err();
    assert!(err.to_string().contains("gate step(s) failed"));
}

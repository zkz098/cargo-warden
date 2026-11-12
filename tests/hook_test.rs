use cargo_warden::hook::{
    ExistingHookPolicy, HookKind, HookMode, HookState, InstallOutcome, UninstallOutcome,
    inspect_hooks, install_hook, resolve_hooks_dir, uninstall_hook,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn init_git_repo(dir: &Path) {
    let status = Command::new("git")
        .arg("init")
        .arg(dir)
        .output()
        .expect("git must be available for hook tests");
    assert!(
        status.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

#[test]
fn test_install_update_status_and_uninstall_standalone() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());

    // Initial install (pre-commit, check mode)
    let outcome = install_hook(
        dir.path(),
        HookKind::PreCommit,
        HookMode::Check,
        ExistingHookPolicy::Fail,
    )
    .unwrap();
    let hook_path = match outcome {
        InstallOutcome::Created(p) => p,
        other => panic!("expected Created, got {other:?}"),
    };
    assert!(hook_path.is_file());

    let content = fs::read_to_string(&hook_path).unwrap();
    assert!(content.starts_with("#!/usr/bin/env sh"));
    assert!(content.contains("cargo-warden check"));

    // Status should report Managed(Check, executable=true)
    let statuses = inspect_hooks(dir.path()).unwrap();
    let pre_commit = statuses
        .iter()
        .find(|s| s.kind == HookKind::PreCommit)
        .unwrap();
    assert_eq!(
        pre_commit.state,
        HookState::Managed {
            mode: Some(HookMode::Check),
            executable: true
        }
    );
    let pre_push = statuses
        .iter()
        .find(|s| s.kind == HookKind::PrePush)
        .unwrap();
    assert_eq!(pre_push.state, HookState::NotInstalled);

    // Re-install with GateFull updates in place
    let updated = install_hook(
        dir.path(),
        HookKind::PreCommit,
        HookMode::GateFull,
        ExistingHookPolicy::Fail,
    )
    .unwrap();
    assert!(matches!(updated, InstallOutcome::Updated(_)));

    let statuses_after = inspect_hooks(dir.path()).unwrap();
    let pre_commit_after = statuses_after
        .iter()
        .find(|s| s.kind == HookKind::PreCommit)
        .unwrap();
    assert_eq!(
        pre_commit_after.state,
        HookState::Managed {
            mode: Some(HookMode::GateFull),
            executable: true
        }
    );

    // Uninstall removes standalone file completely
    let removed = uninstall_hook(dir.path(), HookKind::PreCommit).unwrap();
    assert!(matches!(removed, UninstallOutcome::RemovedFile(_)));
    assert!(!hook_path.exists());

    // Second uninstall is a clean No-op
    let noop = uninstall_hook(dir.path(), HookKind::PreCommit).unwrap();
    assert!(matches!(noop, UninstallOutcome::NotInstalled(_)));
}

#[test]
fn test_existing_hook_fail_append_and_preserve_on_uninstall() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());

    let hooks_dir = resolve_hooks_dir(dir.path()).unwrap();
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join("pre-commit");
    let custom_script = "#!/usr/bin/env sh\necho \"running custom hook\"\n";
    fs::write(&hook_path, custom_script).unwrap();

    // Unmanaged hook should fail under ExistingHookPolicy::Fail
    let err = install_hook(
        dir.path(),
        HookKind::PreCommit,
        HookMode::Check,
        ExistingHookPolicy::Fail,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--append"));

    // Status sees Unmanaged
    let statuses = inspect_hooks(dir.path()).unwrap();
    let pre_commit = statuses
        .iter()
        .find(|s| s.kind == HookKind::PreCommit)
        .unwrap();
    assert!(matches!(pre_commit.state, HookState::Unmanaged { .. }));

    // Uninstall refuses to touch unmanaged hook
    assert!(uninstall_hook(dir.path(), HookKind::PreCommit).is_err());

    // Append adds managed block while preserving custom script
    let appended = install_hook(
        dir.path(),
        HookKind::PreCommit,
        HookMode::Gate,
        ExistingHookPolicy::Append,
    )
    .unwrap();
    assert!(matches!(appended, InstallOutcome::Appended(_)));

    let combined = fs::read_to_string(&hook_path).unwrap();
    assert!(combined.contains("running custom hook"));
    assert!(combined.contains("cargo-warden gate"));

    // Uninstall strips only managed block and keeps custom script intact
    let removed_block = uninstall_hook(dir.path(), HookKind::PreCommit).unwrap();
    assert!(matches!(removed_block, UninstallOutcome::RemovedBlock(_)));
    let remaining = fs::read_to_string(&hook_path).unwrap();
    assert_eq!(remaining, custom_script);
}

#[test]
fn test_force_overwrite_and_custom_hooks_path() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());

    // Configure custom core.hooksPath
    let cfg = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["config", "core.hooksPath", ".githooks"])
        .output()
        .unwrap();
    assert!(cfg.status.success());

    let hooks_dir = resolve_hooks_dir(dir.path()).unwrap();
    assert!(hooks_dir.ends_with(".githooks"));
    fs::create_dir_all(&hooks_dir).unwrap();

    let hook_path = hooks_dir.join("pre-push");
    fs::write(&hook_path, "#!/bin/sh\necho old\n").unwrap();

    let res = install_hook(
        dir.path(),
        HookKind::PrePush,
        HookMode::Gate,
        ExistingHookPolicy::Force,
    )
    .unwrap();
    assert!(matches!(res, InstallOutcome::Overwritten(_)));

    let content = fs::read_to_string(&hook_path).unwrap();
    assert!(!content.contains("echo old"));
    assert!(content.contains("cargo-warden gate"));
}

#[test]
fn test_non_git_directory_fails() {
    let dir = tempdir().unwrap();
    let res = resolve_hooks_dir(dir.path());
    assert!(res.is_err());
}

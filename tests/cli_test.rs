use std::fs;
use std::process::Command;
use tempfile::tempdir;

fn warden_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cargo-warden")
}

#[test]
fn test_cli_direct_and_cargo_subcommand_invocation() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "pub fn hi() {}\n").unwrap();
    fs::write(
        dir.path().join("warden.toml"),
        "[file_length]\nmax_lines = 50\nextensions = [\"rs\"]\npaths = [\"src\"]\n",
    )
    .unwrap();

    let direct = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        direct.status.success(),
        "direct check failed: {}",
        String::from_utf8_lossy(&direct.stderr)
    );

    // `cargo warden <cmd>` passes `"warden"` as argv[1]
    let subcmd = Command::new(warden_bin())
        .args(["warden", "-C", dir.path().to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        subcmd.status.success(),
        "subcommand check failed: {}",
        String::from_utf8_lossy(&subcmd.stderr)
    );
}

#[test]
fn test_cli_explicit_config_and_failure_exit_code() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    let long_file: String = (0..25).map(|i| format!("// line {i}\n")).collect();
    fs::write(dir.path().join("src/lib.rs"), long_file).unwrap();

    let custom_cfg = dir.path().join("custom.toml");
    fs::write(
        &custom_cfg,
        "[file_length]\nmax_lines = 10\nextensions = [\"rs\"]\npaths = [\"src\"]\n",
    )
    .unwrap();

    let out = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "--config",
            custom_cfg.to_str().unwrap(),
            "-q",
            "check",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("Maximum file line count exceeded"));
}

#[test]
fn test_cli_hook_status_in_temp_repo() {
    let dir = tempdir().unwrap();
    let init = Command::new("git")
        .arg("init")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(init.status.success());

    let install = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "hook", "install"])
        .output()
        .unwrap();
    assert!(install.status.success());

    let status = Command::new(warden_bin())
        .args([
            "warden",
            "-C",
            dir.path().to_str().unwrap(),
            "hook",
            "status",
        ])
        .output()
        .unwrap();
    assert!(status.status.success());
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("pre-commit: managed (check, executable)"));
}

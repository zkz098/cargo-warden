use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn warden_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cargo-warden")
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_git_repo(dir: &Path) {
    git(dir, &["init"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test User"]);
}

#[test]
fn test_structured_output_json_and_sarif() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/bad.rs"),
        r#"
pub fn nested(x: i32) {
    if x > 0 {
        if x > 1 {
            if x > 2 {}
        }
    }
}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("warden.toml"),
        "[complexity]\nmax = 2\npaths = [\"src\"]\n",
    )
    .unwrap();

    // JSON output on failure
    let json_out = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
            "check",
        ])
        .output()
        .unwrap();
    assert!(!json_out.status.success());
    let json_val: serde_json::Value =
        serde_json::from_slice(&json_out.stdout).expect("stdout must be valid JSON");
    assert_eq!(json_val["passed"], false);
    assert_eq!(json_val["summary"]["errors"], 1);
    assert_eq!(json_val["diagnostics"][0]["rule_id"], "warden/complexity");
    assert_eq!(json_val["diagnostics"][0]["file"], "src/bad.rs");

    // SARIF 2.1.0 output on failure
    let sarif_out = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "--format",
            "sarif",
            "check",
        ])
        .output()
        .unwrap();
    assert!(!sarif_out.status.success());
    let sarif_val: serde_json::Value =
        serde_json::from_slice(&sarif_out.stdout).expect("stdout must be valid SARIF JSON");
    assert_eq!(sarif_val["version"], "2.1.0");
    assert_eq!(
        sarif_val["runs"][0]["tool"]["driver"]["name"],
        "cargo-warden"
    );
    assert_eq!(
        sarif_val["runs"][0]["results"][0]["ruleId"],
        "warden/complexity"
    );
    assert_eq!(
        sarif_val["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["artifactLocation"]
            ["uri"],
        "src/bad.rs"
    );
}

#[test]
fn test_baseline_generate_suppress_and_detect_regression() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        r#"
pub fn complex_one(x: i32) {
    if x > 0 {
        if x > 1 {
            if x > 2 {}
        }
    }
}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("warden.toml"),
        "baseline = \"warden-baseline.json\"\n\n[complexity]\nmax = 2\npaths = [\"src\"]\n",
    )
    .unwrap();

    // 1. Fails before baseline
    let before = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(!before.status.success());

    // 2. Generate baseline
    let gen_out = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "baseline", "generate"])
        .output()
        .unwrap();
    assert!(
        gen_out.status.success(),
        "baseline generate failed: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );
    assert!(dir.path().join("warden-baseline.json").is_file());

    // 3. Check passes with baseline active
    let with_baseline = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
            "check",
        ])
        .output()
        .unwrap();
    assert!(with_baseline.status.success());
    let report: serde_json::Value = serde_json::from_slice(&with_baseline.stdout).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["summary"]["suppressed"], 1);
    assert_eq!(report["diagnostics"][0]["suppression_reason"], "baseline");

    // 4. `--no-baseline` rejects the baselined violation
    let no_bl = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "--no-baseline", "check"])
        .output()
        .unwrap();
    assert!(!no_bl.status.success());

    // 5. Regressing complexity higher than baseline max_value still fails!
    fs::write(
        dir.path().join("src/lib.rs"),
        r#"
pub fn complex_one(x: i32) {
    if x > 0 {
        if x > 1 {
            if x > 2 {
                if x > 3 {}
            }
        }
    }
}
"#,
    )
    .unwrap();
    let regressed = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
            "check",
        ])
        .output()
        .unwrap();
    assert!(!regressed.status.success());
    let reg_report: serde_json::Value = serde_json::from_slice(&regressed.stdout).unwrap();
    assert_eq!(reg_report["passed"], false);
    assert!(
        reg_report["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("regressed above baseline")
    );
}

#[test]
fn test_incremental_staged_and_diff_checks() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());
    fs::create_dir_all(dir.path().join("src")).unwrap();

    // Commit an existing violating file (legacy.rs) and a clean file (clean.rs)
    fs::write(
        dir.path().join("src/legacy.rs"),
        "pub fn bad(x: i32) { if x > 0 { if x > 1 { if x > 2 {} } } }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("src/clean.rs"),
        "pub fn ok() -> i32 { 1 }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("warden.toml"),
        "[complexity]\nmax = 2\npaths = [\"src\"]\n",
    )
    .unwrap();

    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "initial"]);

    // Full check fails because of src/legacy.rs
    let full = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(!full.status.success());

    // Stage only a clean edit to src/clean.rs -> `--staged` passes!
    fs::write(
        dir.path().join("src/clean.rs"),
        "pub fn ok() -> i32 { 2 }\n",
    )
    .unwrap();
    git(dir.path(), &["add", "src/clean.rs"]);
    let staged_ok = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "--staged", "check"])
        .output()
        .unwrap();
    assert!(
        staged_ok.status.success(),
        "staged check should pass when only clean.rs is staged: {}",
        String::from_utf8_lossy(&staged_ok.stdout)
    );

    // Stage a new violating file -> `--staged` catches it!
    fs::write(
        dir.path().join("src/new_bad.rs"),
        "pub fn too_deep(x: i32) { if x > 0 { if x > 1 { if x > 2 {} } } }\n",
    )
    .unwrap();
    git(dir.path(), &["add", "src/new_bad.rs"]);
    let staged_fail = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "--staged", "check"])
        .output()
        .unwrap();
    assert!(!staged_fail.status.success());

    // Commit it and test `--diff HEAD~1`
    git(dir.path(), &["commit", "-m", "add new_bad"]);
    let diff_fail = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "--diff",
            "HEAD~1",
            "check",
        ])
        .output()
        .unwrap();
    assert!(!diff_fail.status.success());

    // Hook install --staged check
    let hook_install = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "hook",
            "install",
            "--staged",
        ])
        .output()
        .unwrap();
    assert!(hook_install.status.success());
    let hook_status = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "hook", "status"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&hook_status.stdout).contains("check --staged"));
}

#[test]
fn test_init_autodetects_workspace_ci_slint_and_i18n() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("crates/core/src")).unwrap();
    fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    fs::create_dir_all(dir.path().join("ui")).unwrap();
    fs::create_dir_all(dir.path().join("lang/en/LC_MESSAGES")).unwrap();
    fs::create_dir_all(dir.path().join("lang/zh_CN/LC_MESSAGES")).unwrap();

    fs::write(dir.path().join("crates/core/src/lib.rs"), "pub fn x() {}\n").unwrap();
    fs::write(
        dir.path().join(".github/workflows/ci.yml"),
        "jobs:\n  test:\n    steps:\n      - run: cargo nextest run\n",
    )
    .unwrap();
    fs::write(dir.path().join("ui/app.slint"), "export component App {}\n").unwrap();
    fs::write(
        dir.path().join("lang/en/LC_MESSAGES/app.po"),
        "msgid \"hi\"\nmsgstr \"hi\"\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("lang/zh_CN/LC_MESSAGES/app.po"),
        "msgid \"hi\"\nmsgstr \"你好\"\n",
    )
    .unwrap();

    let init_out = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "init"])
        .output()
        .unwrap();
    assert!(
        init_out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let generated = fs::read_to_string(dir.path().join("warden.toml")).unwrap();
    assert!(generated.contains("crates/core/src"));
    assert!(generated.contains(".github/workflows/ci.yml"));
    assert!(generated.contains("nextest"));
    assert!(generated.contains("ui_dir = \"ui\""));
    assert!(generated.contains("domain = \"app\""));
    assert!(generated.contains("\"en\", \"zh_CN\""));
    assert!(generated.contains("respect_gitignore = true"));

    // Second run without --force refuses to overwrite
    let second = Command::new(warden_bin())
        .args(["-C", dir.path().to_str().unwrap(), "init"])
        .output()
        .unwrap();
    assert!(!second.status.success());

    // `--dotfile --force` writes `.warden.toml`
    let dot = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "init",
            "--dotfile",
            "--force",
        ])
        .output()
        .unwrap();
    assert!(dot.status.success());
    assert!(dir.path().join(".warden.toml").is_file());
}

#[test]
fn test_cli_file_length_effective_and_no_gitignore() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join(".gitignore"), "src/gen.rs\n").unwrap();
    fs::write(dir.path().join("src/gen.rs"), "fn a() {}\n".repeat(30)).unwrap();

    let commented = format!(
        "{}{}",
        "// comment line\n".repeat(25),
        "pub fn main_fn() {}\n"
    );
    fs::write(dir.path().join("src/app.rs"), commented).unwrap();

    // Physical fails on src/app.rs (26 > 5), while src/gen.rs is ignored via .gitignore
    let phys_fail = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "file-length",
            "--max-lines",
            "5",
            "--ext",
            "rs",
            "--paths",
            "src",
        ])
        .output()
        .unwrap();
    assert!(!phys_fail.status.success());

    // Effective (SLOC) passes because src/app.rs only has 1 code line and src/gen.rs is gitignored!
    let eff_pass = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "file-length",
            "--max-lines",
            "5",
            "--ext",
            "rs",
            "--paths",
            "src",
            "--effective",
        ])
        .output()
        .unwrap();
    assert!(
        eff_pass.status.success(),
        "effective check failed: {}",
        String::from_utf8_lossy(&eff_pass.stdout)
    );

    // With `--effective --no-gitignore`, src/gen.rs (30 code lines) is scanned and fails!
    let no_git = Command::new(warden_bin())
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "file-length",
            "--max-lines",
            "5",
            "--ext",
            "rs",
            "--paths",
            "src",
            "--effective",
            "--no-gitignore",
        ])
        .output()
        .unwrap();
    assert!(!no_git.status.success());
}

use cargo_warden::checks::ci_parity::{check_ci_parity, workspace_members};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn create_workspace(root: &Path, crates: &[&str]) {
    let members_list: Vec<String> = crates.iter().map(|c| format!("\"crates/{c}\"")).collect();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\nresolver = \"2\"\nmembers = [{}]\n",
            members_list.join(", ")
        ),
    )
    .unwrap();

    for crate_name in crates {
        let crate_dir = root.join("crates").join(crate_name);
        fs::create_dir_all(crate_dir.join("src")).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{crate_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
            ),
        )
        .unwrap();
        fs::write(crate_dir.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
    }
}

#[test]
fn test_missing_ci_workflow_fails() {
    let dir = tempdir().unwrap();
    create_workspace(dir.path(), &["core"]);

    let err = check_ci_parity(
        dir.path(),
        &PathBuf::from(".github/workflows/ci.yml"),
        &[],
        &[],
        &[],
    )
    .unwrap_err();
    assert!(err.to_string().contains("CI workflow file not found"));
}

#[test]
fn test_workspace_members_and_full_ci_coverage_pass() {
    let dir = tempdir().unwrap();
    create_workspace(dir.path(), &["alpha", "beta", "fuzz"]);

    let members = workspace_members(dir.path()).unwrap();
    assert_eq!(members, vec!["alpha", "beta", "fuzz"]);

    let wf_path = dir.path().join(".github/workflows/ci.yml");
    fs::create_dir_all(wf_path.parent().unwrap()).unwrap();
    fs::write(
        &wf_path,
        r#"
jobs:
  test:
    steps:
      - run: cargo nextest run -p alpha -p beta
      - run: |
          for crate in alpha beta; do
            echo $crate
          done
"#,
    )
    .unwrap();

    check_ci_parity(
        dir.path(),
        Path::new(".github/workflows/ci.yml"),
        &["nextest".to_string()],
        &["fuzz".to_string()],
        &["alpha".to_string(), "beta".to_string()],
    )
    .unwrap();
}

#[test]
fn test_missing_member_and_missing_fanout_fail() {
    let dir = tempdir().unwrap();
    create_workspace(dir.path(), &["alpha", "beta"]);

    let wf_path = dir.path().join("ci.yml");
    fs::write(&wf_path, "run: cargo test -p alpha\n").unwrap();

    let err = check_ci_parity(
        dir.path(),
        Path::new("ci.yml"),
        &["cargo test".to_string()],
        &[],
        &[],
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("beta"));
    assert!(!msg.contains("alpha"));

    // Now test fanout failure when both members are tested but fanout misses beta
    fs::write(
        &wf_path,
        "run: cargo test -p alpha -p beta\nfor crate in alpha; do\n",
    )
    .unwrap();
    let fanout_err = check_ci_parity(
        dir.path(),
        Path::new("ci.yml"),
        &["cargo test".to_string()],
        &[],
        &["alpha".to_string(), "beta".to_string()],
    )
    .unwrap_err();
    assert!(fanout_err.to_string().contains("beta"));
}

use cargo_warden::checks::actions::{
    ActionsOptions, RemoteTagInfo, collect_actions_with_fetcher, fetch_remote_tags,
    parse_ls_remote_tags, parse_semver_tag, scan_workflow_file,
};
use cargo_warden::report::Severity;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

#[test]
fn test_scan_workflow_file_parses_and_skips_non_github_uses() {
    let dir = TempDir::new().unwrap();
    let wf_dir = dir.path().join(".github/workflows");
    fs::create_dir_all(&wf_dir).unwrap();
    let wf_path = wf_dir.join("ci.yml");

    fs::write(
        &wf_path,
        r#"
name: CI
on: [push]
jobs:
  call-reusable:
    uses: org/shared/.github/workflows/build.yml@v1.2.0
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: 'actions/cache/restore@v4.0.1'
      - uses: "dtolnay/rust-toolchain@stable"
      - uses: Swatinem/rust-cache@b4ffde65f46336ab88eb53be808477a3936bae11 # v2.7.1
      # - uses: actions/checkout@v2
      - uses: ./.github/actions/local-setup
      - uses: docker://alpine:3.19
      - uses: ${{ matrix.action }}
      - name: Script containing uses
        run: |
          echo "uses: actions/checkout@v1"
          uses: fake/in-script@v1
      - uses: taiki-e/install-action@v2
"#,
    )
    .unwrap();

    let refs = scan_workflow_file(dir.path(), &wf_path).unwrap();
    assert_eq!(refs.len(), 6);

    assert_eq!(refs[0].repo_slug(), "org/shared");
    assert_eq!(
        refs[0].subpath.as_deref(),
        Some(".github/workflows/build.yml")
    );
    assert_eq!(refs[0].git_ref, "v1.2.0");

    assert_eq!(refs[1].repo_slug(), "actions/checkout");
    assert_eq!(refs[1].subpath, None);
    assert_eq!(refs[1].git_ref, "v4");

    assert_eq!(refs[2].repo_slug(), "actions/cache");
    assert_eq!(refs[2].subpath.as_deref(), Some("restore"));
    assert_eq!(refs[2].git_ref, "v4.0.1");

    assert_eq!(refs[3].repo_slug(), "dtolnay/rust-toolchain");
    assert_eq!(refs[3].git_ref, "stable");

    assert_eq!(refs[4].repo_slug(), "Swatinem/rust-cache");
    assert_eq!(refs[4].git_ref, "b4ffde65f46336ab88eb53be808477a3936bae11");

    assert_eq!(refs[5].repo_slug(), "taiki-e/install-action");
    assert_eq!(refs[5].git_ref, "v2");
}

#[test]
fn test_parse_semver_tag_and_ls_remote_deref() {
    assert!(parse_semver_tag("v4").is_some());
    assert!(parse_semver_tag("v4.1").is_some());
    assert!(parse_semver_tag("v4.1.2").is_some());
    assert!(parse_semver_tag("1.2.3-rc.1").is_some());
    assert!(parse_semver_tag("stable").is_none());
    assert!(parse_semver_tag("2024-01-01").is_none());

    let ls_out = "\
1111111111111111111111111111111111111111\trefs/tags/v4.0.0\n\
2222222222222222222222222222222222222222\trefs/tags/v4.0.0^{}\n\
3333333333333333333333333333333333333333\trefs/tags/v4.1.0\n";

    let parsed = parse_ls_remote_tags(ls_out);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].tag, "v4.0.0");
    assert_eq!(parsed[0].sha, "2222222222222222222222222222222222222222");
    assert_eq!(parsed[1].tag, "v4.1.0");
}

#[test]
fn test_collect_actions_floating_full_sha_ignore_and_errors() {
    let dir = TempDir::new().unwrap();
    let wf_dir = dir.path().join(".github/workflows");
    fs::create_dir_all(&wf_dir).unwrap();
    let wf_path = wf_dir.join("ci.yml");

    let old_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let new_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fs::write(
        &wf_path,
        format!(
            r#"
jobs:
  test:
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v3
      - uses: actions/cache@v4.0.1
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@{old_sha}
      - uses: ignored/action@v1
      - uses: broken/repo@v1
"#
        ),
    )
    .unwrap();

    let fetcher = |slug: &str| -> anyhow::Result<Vec<RemoteTagInfo>> {
        match slug {
            // v4 is NOT outdated when latest is v4.2.2 (same major)
            "actions/checkout" => Ok(vec![
                RemoteTagInfo {
                    tag: "v4".into(),
                    sha: "111".into(),
                },
                RemoteTagInfo {
                    tag: "v4.2.2".into(),
                    sha: "111".into(),
                },
            ]),
            // v3 IS outdated when v4 / v4.1.0 exists -> recommends v4
            "actions/setup-node" => Ok(vec![
                RemoteTagInfo {
                    tag: "v3".into(),
                    sha: "111".into(),
                },
                RemoteTagInfo {
                    tag: "v4".into(),
                    sha: "222".into(),
                },
                RemoteTagInfo {
                    tag: "v4.1.0".into(),
                    sha: "222".into(),
                },
                RemoteTagInfo {
                    tag: "v5.0.0-rc.1".into(),
                    sha: "333".into(),
                },
            ]),
            // v4.0.1 IS outdated when v4.2.0 exists -> recommends v4.2.0
            "actions/cache" => Ok(vec![
                RemoteTagInfo {
                    tag: "v4.0.1".into(),
                    sha: "111".into(),
                },
                RemoteTagInfo {
                    tag: "v4.2.0".into(),
                    sha: "222".into(),
                },
            ]),
            // stable branch -> not outdated
            "dtolnay/rust-toolchain" => Ok(vec![RemoteTagInfo {
                tag: "v1.0.0".into(),
                sha: "111".into(),
            }]),
            // SHA-pinned to v2.7.0 -> v2.7.5 available
            "Swatinem/rust-cache" => Ok(vec![
                RemoteTagInfo {
                    tag: "v2.7.0".into(),
                    sha: old_sha.into(),
                },
                RemoteTagInfo {
                    tag: "v2.7.5".into(),
                    sha: new_sha.into(),
                },
            ]),
            "broken/repo" => anyhow::bail!("network timeout"),
            other => panic!("unexpected repo query: {other}"),
        }
    };

    let ignore = vec!["ignored/*".to_string()];
    let diags = collect_actions_with_fetcher(
        dir.path(),
        &ActionsOptions {
            workflows: &[],
            ignore: &ignore,
            strict: true,
            require_sha_pin: false,
            offline: false,
            changeset: None,
            emit_text: false,
            parallel: false,
        },
        fetcher,
    )
    .unwrap();

    assert_eq!(diags.len(), 4);

    assert_eq!(diags[0].rule_id, "warden/actions/outdated");
    assert_eq!(diags[0].severity, Severity::Error);
    assert!(diags[0].message.contains("actions/setup-node"));
    assert!(diags[0].message.contains("`v4`"));

    assert_eq!(diags[1].rule_id, "warden/actions/outdated");
    assert!(diags[1].message.contains("actions/cache"));
    assert!(diags[1].message.contains("`v4.2.0`"));

    assert_eq!(diags[2].rule_id, "warden/actions/outdated");
    assert!(diags[2].message.contains("Swatinem/rust-cache"));
    assert!(diags[2].message.contains("`v2.7.5`"));

    assert_eq!(diags[3].rule_id, "warden/actions/fetch-failed");
    assert_eq!(diags[3].severity, Severity::Warning);
    assert!(diags[3].message.contains("broken/repo"));

    // Offline mode returns no diagnostics
    let offline_diags = collect_actions_with_fetcher(
        dir.path(),
        &ActionsOptions {
            workflows: &[],
            ignore: &ignore,
            strict: true,
            require_sha_pin: false,
            offline: true,
            changeset: None,
            emit_text: false,
            parallel: false,
        },
        fetcher,
    )
    .unwrap();
    assert!(offline_diags.is_empty());

    // Changeset excluding ci.yml returns no diagnostics
    let mut cs = BTreeSet::new();
    cs.insert(PathBuf::from("src/lib.rs"));
    let cs_diags = collect_actions_with_fetcher(
        dir.path(),
        &ActionsOptions {
            workflows: &[],
            ignore: &ignore,
            strict: false,
            require_sha_pin: false,
            offline: false,
            changeset: Some(&cs),
            emit_text: false,
            parallel: false,
        },
        fetcher,
    )
    .unwrap();
    assert!(cs_diags.is_empty());
}

fn run_git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn test_fetch_remote_tags_from_local_git_repo() {
    let repo = TempDir::new().unwrap();
    run_git(repo.path(), &["init"]);
    run_git(repo.path(), &["config", "user.email", "test@example.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    fs::write(repo.path().join("README.md"), "v1\n").unwrap();
    run_git(repo.path(), &["add", "."]);
    run_git(repo.path(), &["commit", "-m", "init"]);
    run_git(repo.path(), &["tag", "v1.0.0"]);
    run_git(repo.path(), &["tag", "-a", "v1.1.0", "-m", "release 1.1.0"]);

    let tags = fetch_remote_tags(repo.path().to_str().unwrap()).unwrap();
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].tag, "v1.0.0");
    assert_eq!(tags[1].tag, "v1.1.0");
    // Annotated tag should resolve to the same commit SHA as lightweight tag
    assert_eq!(tags[0].sha, tags[1].sha);
}

#[test]
fn test_cli_actions_offline_json() {
    let dir = TempDir::new().unwrap();
    let wf_dir = dir.path().join(".github/workflows");
    fs::create_dir_all(&wf_dir).unwrap();
    fs::write(
        wf_dir.join("ci.yml"),
        "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v4\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_cargo-warden"))
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
            "actions",
            "--offline",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["passed"], true);
    assert_eq!(json["summary"]["total"], 0);
}

#[test]
fn test_actions_require_sha_and_fix_pin_sha() {
    use cargo_warden::checks::actions::fix_actions_with_fetcher;

    let dir = tempfile::tempdir().unwrap();
    let wf_dir = dir.path().join(".github/workflows");
    fs::create_dir_all(&wf_dir).unwrap();
    let wf_path = wf_dir.join("ci.yml");

    let initial = "\
jobs:
  build:
    steps:
      - uses: actions/checkout@v3
      - uses: \"actions/cache@v4.1.0\"
      - uses: Swatinem/rust-cache@1111111111111111111111111111111111111111 # v2.7.0
";
    fs::write(&wf_path, initial).unwrap();

    let fetcher = |slug: &str| -> anyhow::Result<Vec<RemoteTagInfo>> {
        match slug {
            "actions/checkout" => Ok(vec![
                RemoteTagInfo {
                    tag: "v3".to_string(),
                    sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                },
                RemoteTagInfo {
                    tag: "v4".to_string(),
                    sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                },
                RemoteTagInfo {
                    tag: "v4.2.2".to_string(),
                    sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                },
            ]),
            "actions/cache" => Ok(vec![
                RemoteTagInfo {
                    tag: "v4.1.0".to_string(),
                    sha: "cccccccccccccccccccccccccccccccccccccccc".to_string(),
                },
                RemoteTagInfo {
                    tag: "v4.2.0".to_string(),
                    sha: "dddddddddddddddddddddddddddddddddddddddd".to_string(),
                },
            ]),
            "Swatinem/rust-cache" => Ok(vec![
                RemoteTagInfo {
                    tag: "v2.7.0".to_string(),
                    sha: "1111111111111111111111111111111111111111".to_string(),
                },
                RemoteTagInfo {
                    tag: "v2.7.5".to_string(),
                    sha: "2222222222222222222222222222222222222222".to_string(),
                },
            ]),
            other => panic!("unexpected repo query: {other}"),
        }
    };

    // 1. require_sha_pin detects unpinned refs even offline
    let offline_unpinned = collect_actions_with_fetcher(
        dir.path(),
        &ActionsOptions {
            workflows: &[],
            ignore: &[],
            strict: true,
            require_sha_pin: true,
            offline: true,
            changeset: None,
            emit_text: false,
            parallel: false,
        },
        fetcher,
    )
    .unwrap();
    assert_eq!(offline_unpinned.len(), 2);
    assert!(
        offline_unpinned
            .iter()
            .all(|d| d.rule_id == "warden/actions/unpinned-sha")
    );

    // 2. fix + pin_sha upgrades and pins all 3 refs
    let stats = fix_actions_with_fetcher(
        dir.path(),
        &ActionsOptions {
            workflows: &[],
            ignore: &[],
            strict: false,
            require_sha_pin: true,
            offline: false,
            changeset: None,
            emit_text: false,
            parallel: false,
        },
        true,
        true,
        fetcher,
    )
    .unwrap();
    assert_eq!(stats.files_updated, 1);
    assert_eq!(stats.refs_updated, 3);

    let updated = fs::read_to_string(&wf_path).unwrap();
    assert!(
        updated
            .contains("uses: actions/checkout@bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb # v4.2.2"),
        "actual:\n{updated}"
    );
    assert!(
        updated
            .contains("uses: \"actions/cache@dddddddddddddddddddddddddddddddddddddddd\" # v4.2.0"),
        "actual:\n{updated}"
    );
    assert!(
        updated.contains(
            "uses: Swatinem/rust-cache@2222222222222222222222222222222222222222 # v2.7.5"
        ),
        "actual:\n{updated}"
    );

    // 3. Re-checking with require_sha_pin passes with 0 diagnostics
    let after = collect_actions_with_fetcher(
        dir.path(),
        &ActionsOptions {
            workflows: &[],
            ignore: &[],
            strict: true,
            require_sha_pin: true,
            offline: false,
            changeset: None,
            emit_text: false,
            parallel: false,
        },
        fetcher,
    )
    .unwrap();
    assert!(after.is_empty(), "unexpected diagnostics: {after:#?}");
}

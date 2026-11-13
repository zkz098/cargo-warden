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

fn write_crate(root: &Path, name: &str, deps: &[&str], dev_deps: &[&str]) {
    let crate_dir = root.join("crates").join(name);
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    let mut toml =
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    if !deps.is_empty() {
        toml.push_str("\n[dependencies]\n");
        for d in deps {
            toml.push_str(&format!("{d} = {{ path = \"../{d}\" }}\n"));
        }
    }
    if !dev_deps.is_empty() {
        toml.push_str("\n[dev-dependencies]\n");
        for d in dev_deps {
            toml.push_str(&format!("{d} = {{ path = \"../{d}\" }}\n"));
        }
    }
    fs::write(crate_dir.join("Cargo.toml"), toml).unwrap();
    fs::write(crate_dir.join("src/lib.rs"), "pub fn ping() {}\n").unwrap();
}

#[test]
fn test_workspace_dag_layers_forbid_cycles_and_suppression() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/domain\", \"crates/infra\", \"crates/app\", \"crates/bootstrap\"]\n",
    )
    .unwrap();

    write_crate(root, "domain", &[], &[]);
    write_crate(root, "infra", &["domain"], &[]);
    write_crate(root, "app", &["domain", "infra"], &[]);
    write_crate(root, "bootstrap", &["app"], &[]);

    let clean_cfg = r#"
[dag]
roots = []

[dag.workspace]
forbid_deps = ["bootstrap"]

[[dag.workspace.layers]]
name = "domain"
crates = ["domain"]
allow = []

[[dag.workspace.layers]]
name = "infra"
crates = ["infra"]
allow = ["domain"]

[[dag.workspace.layers]]
name = "app"
crates = ["app"]
allow = ["domain", "infra"]
"#;
    fs::write(root.join("warden.toml"), clean_cfg).unwrap();

    // 1. Clean workspace passes via `dag --workspace` and `check`
    let ok = Command::new(warden_bin())
        .args(["-C", root.to_str().unwrap(), "dag", "--workspace"])
        .output()
        .unwrap();
    assert!(
        ok.status.success(),
        "clean workspace dag failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&ok.stdout),
        String::from_utf8_lossy(&ok.stderr)
    );

    // 2. Introduce layer violation (domain -> infra) + forbidden dep (infra -> bootstrap)
    write_crate(root, "domain", &["infra"], &[]);
    write_crate(root, "infra", &["domain", "bootstrap"], &[]);

    let fail = Command::new(warden_bin())
        .args(["-C", root.to_str().unwrap(), "--format", "json", "check"])
        .output()
        .unwrap();
    assert!(!fail.status.success());
    let report: serde_json::Value = serde_json::from_slice(&fail.stdout).unwrap();
    let rule_ids: Vec<&str> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["rule_id"].as_str().unwrap())
        .collect();
    assert!(rule_ids.contains(&"warden/dag/workspace-layer-violation"));
    assert!(rule_ids.contains(&"warden/dag/workspace-forbid-dep"));

    // 3. Inline suppression in Cargo.toml suppresses violations
    fs::write(
        root.join("crates/domain/Cargo.toml"),
        "[package]\nname = \"domain\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ninfra = { path = \"../infra\" } # warden:allow(workspace-dag)\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/infra/Cargo.toml"),
        "[package]\nname = \"infra\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndomain = { path = \"../domain\" }\nbootstrap = { path = \"../bootstrap\" } # warden:allow(workspace-dag)\n",
    )
    .unwrap();
    let suppressed = Command::new(warden_bin())
        .args(["-C", root.to_str().unwrap(), "--format", "json", "check"])
        .output()
        .unwrap();
    assert!(
        suppressed.status.success(),
        "suppressed workspace dag failed: {}",
        String::from_utf8_lossy(&suppressed.stdout)
    );

    // 4. Dev-dependency cycle detected when `include_dev_dependencies = true`
    write_crate(root, "domain", &[], &["infra"]);
    write_crate(root, "infra", &["domain"], &[]);
    fs::write(
        root.join("warden.toml"),
        clean_cfg.replace(
            "[dag.workspace]\n",
            "[dag.workspace]\ninclude_dev_dependencies = true\n",
        ),
    )
    .unwrap();
    let cycle = Command::new(warden_bin())
        .args([
            "-C",
            root.to_str().unwrap(),
            "--format",
            "json",
            "dag",
            "--workspace",
        ])
        .output()
        .unwrap();
    assert!(!cycle.status.success());
    let cycle_report: serde_json::Value = serde_json::from_slice(&cycle.stdout).unwrap();
    assert!(
        cycle_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["rule_id"] == "warden/dag/workspace-cycle")
    );
}

#[test]
fn test_baseline_prune_and_unused_suppression_warnings() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();

    fs::write(
        root.join("src/a.rs"),
        "pub fn bad1(x: i32) { if x > 0 { if x > 1 { if x > 2 {} } } }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn bad2(x: i32) { if x > 0 { if x > 1 { if x > 2 {} } } }\n",
    )
    .unwrap();
    fs::write(
        root.join("warden.toml"),
        "baseline = \"warden-baseline.json\"\n\n[complexity]\nmax = 2\npaths = [\"src\"]\n",
    )
    .unwrap();

    let generate = Command::new(warden_bin())
        .args(["-C", root.to_str().unwrap(), "baseline", "generate"])
        .output()
        .unwrap();
    assert!(generate.status.success());

    // Fix src/a.rs and add an unused `// warden:allow(complexity)` on a simple function
    fs::write(
        root.join("src/a.rs"),
        "// warden:allow(complexity)\npub fn bad1() -> i32 { 1 }\n",
    )
    .unwrap();

    // Check passes (only warning + 1 baselined error) and mentions stale baseline entry
    let check_txt = Command::new(warden_bin())
        .args(["-C", root.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(check_txt.status.success());
    let stdout = String::from_utf8_lossy(&check_txt.stdout);
    assert!(stdout.contains("stale baseline entry"));
    assert!(stdout.contains("unused `warden:allow(complexity)`"));

    // Dry-run prune reports 1 removed without altering file
    let dry = Command::new(warden_bin())
        .args([
            "-C",
            root.to_str().unwrap(),
            "--format",
            "json",
            "baseline",
            "prune",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(dry.status.success());
    let dry_val: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry_val["total_before"], 2);
    assert_eq!(dry_val["removed"], 1);
    assert_eq!(dry_val["kept"], 1);

    // Actual prune removes stale entry
    let prune = Command::new(warden_bin())
        .args([
            "-C",
            root.to_str().unwrap(),
            "--format",
            "json",
            "baseline",
            "prune",
        ])
        .output()
        .unwrap();
    assert!(prune.status.success());
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("warden-baseline.json")).unwrap())
            .unwrap();
    assert_eq!(saved["entries"].as_object().unwrap().len(), 1);
}

#[test]
fn test_complexity_metrics_overrides_and_github_actions_format() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/core")).unwrap();
    fs::create_dir_all(root.join("src/legacy")).unwrap();

    // core/a.rs violates function length (6 > 4) and nesting depth (3 > 2)
    fs::write(
        root.join("src/core/a.rs"),
        r#"pub fn deep_and_long(x: i32) {
    if x > 0 {
        for _ in 0..2 {
            if x > 1 {
                let _ = x;
            }
        }
    }
}
"#,
    )
    .unwrap();

    // legacy/b.rs is relaxed via [[complexity.overrides]] and [[file_length.overrides]]
    fs::write(
        root.join("src/legacy/b.rs"),
        r#"// comment
// comment
// comment
pub fn legacy_fn(x: i32) {
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
        root.join("warden.toml"),
        r#"
[complexity]
max = 10
max_function_lines = 4
max_nesting = 2
paths = ["src"]

[[complexity.overrides]]
paths = ["src/legacy/**"]
max = 20
max_function_lines = 20
max_nesting = 5

[file_length]
max_lines = 10
extensions = ["rs"]
paths = ["src"]

[[file_length.overrides]]
paths = ["src/legacy/**"]
max_lines = 8
mode = "effective"
"#,
    )
    .unwrap();

    let out = Command::new(warden_bin())
        .args([
            "-C",
            root.to_str().unwrap(),
            "--format",
            "github-actions",
            "check",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let gha = String::from_utf8_lossy(&out.stdout);
    assert!(gha.contains("::error file=src/core/a.rs,line=1,title=warden/function-length::"));
    assert!(gha.contains("::error file=src/core/a.rs,line=1,title=warden/nesting-depth::"));
    assert!(!gha.contains("src/legacy/b.rs"));

    // CLI `--max-lines` and `--max-nesting` flags on `complexity`
    let cli_out = Command::new(warden_bin())
        .args([
            "-C",
            root.to_str().unwrap(),
            "--format",
            "json",
            "complexity",
            "--max-lines",
            "4",
            "--max-nesting",
            "2",
            "--paths",
            "src/core",
        ])
        .output()
        .unwrap();
    assert!(!cli_out.status.success());
    let json_val: serde_json::Value = serde_json::from_slice(&cli_out.stdout).unwrap();
    assert_eq!(json_val["summary"]["errors"], 2);
}

#[test]
fn test_completions_and_doctor_commands() {
    let comp = Command::new(warden_bin())
        .args(["completions", "bash"])
        .output()
        .unwrap();
    assert!(comp.status.success());
    let script = String::from_utf8_lossy(&comp.stdout);
    assert!(script.contains("cargo-warden"));
    assert!(script.contains("doctor"));

    let dir = tempdir().unwrap();
    let root = dir.path();
    git(root, &["init"]);

    // Without config, `doctor --strict` fails while non-strict passes
    let non_strict = Command::new(warden_bin())
        .args(["-C", root.to_str().unwrap(), "--format", "json", "doctor"])
        .output()
        .unwrap();
    assert!(non_strict.status.success());
    let doc_val: serde_json::Value = serde_json::from_slice(&non_strict.stdout).unwrap();
    assert_eq!(doc_val["healthy"], true);
    assert!(doc_val["summary"]["warnings"].as_u64().unwrap() >= 1);

    let strict = Command::new(warden_bin())
        .args(["-C", root.to_str().unwrap(), "doctor", "--strict"])
        .output()
        .unwrap();
    assert!(!strict.status.success());

    // With valid config, `doctor` reports ok for configuration
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("warden.toml"), "[dag]\nroots = [\"src\"]\n").unwrap();
    let healthy = Command::new(warden_bin())
        .args(["-C", root.to_str().unwrap(), "--format", "json", "doctor"])
        .output()
        .unwrap();
    assert!(healthy.status.success());
    let healthy_val: serde_json::Value = serde_json::from_slice(&healthy.stdout).unwrap();
    assert!(
        healthy_val["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "configuration" && c["status"] == "ok")
    );
}

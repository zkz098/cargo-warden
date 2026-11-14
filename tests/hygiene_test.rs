use cargo_warden::checks::complexity::{
    ComplexityOptions, ComplexityPolicy, collect_complexity_with_policy,
};
use cargo_warden::checks::hygiene::{HygieneOptions, collect_hygiene};
use cargo_warden::config::{HygieneConfig, HygieneOverride};
use cargo_warden::report::Severity;
use std::fs;
use std::path::PathBuf;

#[test]
fn test_hygiene_unsafe_unwrap_dbg_todo_println_and_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/bin")).unwrap();

    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn bad_prod(x: Option<i32>) -> i32 {
    dbg!(x);
    x.unwrap()
}

pub fn with_todo() {
    todo!("later");
}

pub fn unsafe_missing_safety() {
    unsafe {
        let _ = 1;
    }
}

pub fn unsafe_with_safety() {
    // SAFETY: trivial integer operation in test fixture
    unsafe {
        let _ = 2;
    }
}

pub fn suppressed_unwrap(x: Option<i32>) -> i32 {
    // warden:allow(unwrap)
    x.unwrap()
}

#[cfg(test)]
mod tests {
    #[test]
    fn allowed_in_tests() {
        let x: Option<i32> = Some(1);
        assert_eq!(x.unwrap(), 1);
        dbg!(x);
    }
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("src/bin/cli.rs"),
        r#"
fn main() {
    println!("hello");
    let x: Option<i32> = Some(1);
    let _ = x.expect("ok");
}
"#,
    )
    .unwrap();

    let cfg = HygieneConfig {
        paths: vec![PathBuf::from("src")],
        forbid_println: true,
        forbid_expect: true,
        overrides: vec![HygieneOverride {
            paths: vec!["src/bin/**".to_string()],
            forbid_println: Some(false),
            forbid_expect: Some(false),
            ..Default::default()
        }],
        ..Default::default()
    };

    let (scanned, diags) = collect_hygiene(
        &[root.join("src")],
        &HygieneOptions {
            base_dir: root,
            config: &cfg,
            changeset: None,
            emit_text: false,
        },
    )
    .unwrap();

    assert_eq!(scanned, 2);
    let active: Vec<_> = diags
        .iter()
        .filter(|d| !d.suppressed && d.severity == Severity::Error)
        .collect();
    let rule_ids: Vec<&str> = active.iter().map(|d| d.rule_id.as_str()).collect();
    assert!(rule_ids.contains(&"warden/hygiene/dbg"), "{rule_ids:?}");
    assert!(rule_ids.contains(&"warden/hygiene/unwrap"), "{rule_ids:?}");
    assert!(rule_ids.contains(&"warden/hygiene/todo"), "{rule_ids:?}");
    assert!(
        rule_ids.contains(&"warden/hygiene/unsafe-without-safety"),
        "{rule_ids:?}"
    );
    assert_eq!(active.len(), 4, "unexpected active diags: {active:#?}");
    assert!(
        !diags
            .iter()
            .any(|d| d.rule_id == "warden/unused-suppression"),
        "unexpected unused suppression warning: {diags:#?}"
    );
}

#[test]
fn test_complexity_max_params() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn ok_fn(a: i32, b: i32) -> i32 {
    a + b
}

pub fn too_many(a: i32, b: i32, c: i32, d: i32) -> i32 {
    a + b + c + d
}
"#,
    )
    .unwrap();

    let (_, _, diags) = collect_complexity_with_policy(
        &[root.join("src")],
        &ComplexityOptions {
            base_dir: root,
            max_complexity: 20,
            ignore_tests: true,
            ignore_patterns: &[],
            respect_gitignore: false,
            changeset: None,
            emit_text: false,
        },
        &ComplexityPolicy {
            max_params: Some(3),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].rule_id, "warden/function-params");
    assert!(diags[0].message.contains("too_many()"));
    assert_eq!(diags[0].metric_value, Some(4));
}

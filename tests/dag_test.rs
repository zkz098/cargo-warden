use cargo_warden::checks::dag::check_dag;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_clean_dag() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    // Module A
    fs::write(src.join("a.rs"), "pub fn hello() {}").unwrap();

    // Module B depends on A
    fs::write(
        src.join("b.rs"),
        "use crate::a::hello;\npub fn run() { hello(); }",
    )
    .unwrap();

    assert!(check_dag(&src, &[]).is_ok());
}

#[test]
fn test_cycle_detection() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    // Module A depends on B
    fs::write(
        src.join("a.rs"),
        "use crate::b::bar;\npub fn foo() { bar(); }",
    )
    .unwrap();

    // Module B depends on A
    fs::write(
        src.join("b.rs"),
        "use crate::a::foo;\npub fn bar() { foo(); }",
    )
    .unwrap();

    let res = check_dag(&src, &[]);
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("cycle"));
}

#[test]
fn test_forbid_imports() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    // bootstrap module
    fs::write(src.join("bootstrap.rs"), "pub fn start() {}").unwrap();

    // worker module imports bootstrap
    fs::write(
        src.join("worker.rs"),
        "use crate::bootstrap::start;\npub fn work() { start(); }",
    )
    .unwrap();

    let res = check_dag(&src, &["bootstrap".to_string()]);
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("Forbidden imports detected"));
}

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

#[test]
fn test_nested_use_and_inline_paths() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(src.join("a.rs"), "pub struct Foo; pub fn bar() {}").unwrap();
    fs::write(src.join("b.rs"), "pub struct Bar;").unwrap();

    // c depends on a (via nested use) and b (via inline type path)
    fs::write(
        src.join("c.rs"),
        r#"
        use crate::a::{self, Foo, bar as my_bar};

        pub fn test_fn() {
            my_bar();
            let _val: crate::b::Bar = crate::b::Bar;
        }
        "#,
    )
    .unwrap();

    assert!(check_dag(&src, &[]).is_ok());
}

#[test]
fn test_cfg_test_skipped() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    // Module A depends on B in production code
    fs::write(
        src.join("a.rs"),
        "use crate::b::bar;\npub fn foo() { bar(); }",
    )
    .unwrap();

    // Module B only depends on A inside #[cfg(test)] tests!
    fs::write(
        src.join("b.rs"),
        r#"
        pub fn bar() {}

        #[cfg(test)]
        mod tests {
            use crate::a::foo;
            #[test]
            fn it_works() {
                foo();
            }
        }
        "#,
    )
    .unwrap();

    // Production DAG should NOT report a cycle because #[cfg(test)] is skipped!
    assert!(check_dag(&src, &[]).is_ok());
}

#[test]
fn test_raw_identifiers() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(src.join("type.rs"), "pub fn test_fn() {}").unwrap();
    fs::write(
        src.join("consumer.rs"),
        "use crate::r#type::test_fn;\npub fn run() { test_fn(); }",
    )
    .unwrap();

    assert!(check_dag(&src, &[]).is_ok());
}

#[test]
fn test_super_self_submodules_rename_glob_and_three_node_cycle() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(src.join("alpha")).unwrap();
    fs::create_dir_all(src.join("beta")).unwrap();
    fs::create_dir_all(src.join("gamma")).unwrap();

    // alpha -> beta (via super::beta::*)
    fs::write(
        src.join("alpha/sub.rs"),
        "use super::super::beta::*;\npub fn run() { helper(); }\n",
    )
    .unwrap();
    // beta -> gamma (via rename)
    fs::write(
        src.join("beta/mod.rs"),
        "use crate::gamma::val as gval;\npub fn helper() -> i32 { gval() }\n",
    )
    .unwrap();
    // gamma -> alpha (closes 3-node cycle) + self reference (not a cycle by itself)
    fs::write(
        src.join("gamma/mod.rs"),
        "use self::inner::v;\nuse crate::alpha::sub::run;\nmod inner { pub fn v() -> i32 { 1 } }\npub fn val() -> i32 { run(); v() }\n",
    )
    .unwrap();

    let err = check_dag(&src, &[]).unwrap_err().to_string();
    assert!(err.contains("alpha <-> beta <-> gamma"));
}

#[test]
fn test_missing_src_root_and_forbidden_self_import_allowed() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("does_not_exist");
    assert!(check_dag(&missing, &[]).is_err());

    let src = dir.path().join("src");
    fs::create_dir_all(src.join("bootstrap")).unwrap();
    fs::write(src.join("core.rs"), "pub fn init() {}\n").unwrap();
    fs::write(
        src.join("bootstrap/mod.rs"),
        "use crate::core::init;\npub fn boot() { init(); }\n",
    )
    .unwrap();
    fs::write(
        src.join("bootstrap/wire.rs"),
        "use crate::bootstrap::boot;\npub fn start() { boot(); }\n",
    )
    .unwrap();

    // bootstrap referencing itself is fine; no non-bootstrap module imports bootstrap
    assert!(check_dag(&src, &["bootstrap".to_string()]).is_ok());
}

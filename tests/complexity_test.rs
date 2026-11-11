use cargo_warden::checks::complexity::{analyze_file, check_complexity};
use std::fs;
use tempfile::tempdir;

#[test]
fn test_simple_function_complexity() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("lib.rs");

    fs::write(
        &file,
        r#"
        pub fn add(a: i32, b: i32) -> i32 {
            a + b
        }
    "#,
    )
    .unwrap();

    let reports = analyze_file(&file).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].function_name, "add");
    assert_eq!(reports[0].score, 0);
}

#[test]
fn test_nested_control_flow_complexity() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("lib.rs");

    // if (1) + nested if (1 + 1) + loop (1 + 2) = 6
    fs::write(
        &file,
        r#"
        pub fn complex_fn(x: i32) {
            if x > 0 {
                if x > 10 {
                    loop {
                        break;
                    }
                }
            }
        }
    "#,
    )
    .unwrap();

    let reports = analyze_file(&file).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].function_name, "complex_fn");
    // if: 1 + 0 = 1
    // nested if: 1 + 1 = 2
    // loop: 1 + 2 = 3
    // total = 6
    assert_eq!(reports[0].score, 6);
}

#[test]
fn test_threshold_violation() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("lib.rs");

    fs::write(
        &file,
        r#"
        pub fn exceeds(x: i32) {
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

    // Max threshold 5, but nesting 4 ifs gives 1 + 2 + 3 + 4 = 10 > 5
    let res = check_complexity(&[dir.path().to_path_buf()], 5, false, &[]);
    assert!(res.is_err());
    assert!(
        res.unwrap_err()
            .to_string()
            .contains("Cognitive complexity threshold exceeded")
    );
}

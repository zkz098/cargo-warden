use cargo_warden::checks::complexity::{
    ComplexityOptions, analyze_file, check_complexity, collect_complexity,
};
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

#[test]
fn test_else_if_match_loops_closures_impl_and_boolean_ops() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("rules.rs");

    fs::write(
        &file,
        r#"
        struct Calc;

        impl Calc {
            pub fn eval(&self, x: i32, items: &[i32]) -> i32 {
                // if (+1) + bool(&& +1, || +1) + else if (+1) + else (+1) = 5
                if x > 0 && x < 10 || x == 42 {
                    1
                } else if x < 0 {
                    2
                } else {
                    // match (+1 + nesting 1 = 2) + guard (+1) = 3
                    match items.first() {
                        Some(v) if *v > 0 => *v,
                        _ => 0,
                    }
                }
            }
        }

        pub fn loops_and_labels(mut n: i32) {
            // while (+1) + closure (nesting +1) + for (+1 + nesting 2 = 3) + labeled break (+1) + labeled continue (+1)
            'outer: while n > 0 {
                let f = || {
                    for i in 0..3 {
                        if i == 1 {
                            continue 'outer;
                        }
                        break 'outer;
                    }
                };
                f();
                n -= 1;
            }
        }
        "#,
    )
    .unwrap();

    let reports = analyze_file(&file).unwrap();
    let eval_score = reports
        .iter()
        .find(|r| r.function_name == "eval")
        .unwrap()
        .score;
    assert_eq!(eval_score, 8);

    let loops_score = reports
        .iter()
        .find(|r| r.function_name == "loops_and_labels")
        .unwrap()
        .score;
    // while (1) + for inside closure (1 + 2 = 3) + continue 'outer (1) + if inside closure+while+for (1 + 3 = 4) + break 'outer (1) = 10
    assert_eq!(loops_score, 10);
}

#[test]
fn test_boolean_sequence_same_op_counts_once() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("bools.rs");

    fs::write(
        &file,
        r#"
        pub fn all_and(a: bool, b: bool, c: bool, d: bool) -> bool {
            a && b && c && d
        }
        pub fn mixed_ops(a: bool, b: bool, c: bool) -> bool {
            a && b || c
        }
        "#,
    )
    .unwrap();

    let reports = analyze_file(&file).unwrap();
    assert_eq!(
        reports
            .iter()
            .find(|r| r.function_name == "all_and")
            .unwrap()
            .score,
        1
    );
    assert_eq!(
        reports
            .iter()
            .find(|r| r.function_name == "mixed_ops")
            .unwrap()
            .score,
        2
    );
}

#[test]
fn test_ignore_tests_and_ignore_patterns() {
    let dir = tempdir().unwrap();
    let sub_tests = dir.path().join("tests");
    let ignored_dir = dir.path().join("generated");
    fs::create_dir_all(&sub_tests).unwrap();
    fs::create_dir_all(&ignored_dir).unwrap();

    let complex_body = "pub fn bad(x: i32) { if x > 0 { if x > 1 { if x > 2 {} } } }\n";
    fs::write(sub_tests.join("integration.rs"), complex_body).unwrap();
    fs::write(dir.path().join("foo_test.rs"), complex_body).unwrap();
    fs::write(dir.path().join("tests.rs"), complex_body).unwrap();
    fs::write(ignored_dir.join("gen.rs"), complex_body).unwrap();
    fs::write(dir.path().join("clean.rs"), "pub fn ok() -> i32 { 1 }\n").unwrap();

    let reports = check_complexity(
        &[dir.path().to_path_buf()],
        2,
        true,
        &["generated".to_string()],
    )
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].function_name, "ok");

    // When ignore_tests = false, test files are scanned and exceed max=2
    assert!(
        check_complexity(
            &[dir.path().to_path_buf()],
            2,
            false,
            &["generated".to_string()],
        )
        .is_err()
    );
}

#[test]
fn test_inline_complexity_suppression_across_attribute() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("legacy.rs"),
        r#"
// warden:allow(complexity: legacy state machine)
#[inline]
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

    let (_reports, _files, diagnostics) = collect_complexity(
        &[dir.path().to_path_buf()],
        &ComplexityOptions {
            base_dir: dir.path(),
            max_complexity: 2,
            ignore_tests: true,
            ignore_patterns: &[],
            respect_gitignore: true,
            changeset: None,
            emit_text: false,
        },
    )
    .unwrap();

    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].suppressed);
    assert_eq!(diagnostics[0].suppression_reason.as_deref(), Some("inline"));
}

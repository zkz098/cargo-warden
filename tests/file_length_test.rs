use cargo_warden::checks::file_length::{
    FileLengthOptions, check_file_length, check_file_length_with_options, count_effective_lines,
};
use cargo_warden::config::LineCountMode;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn test_file_length_pass() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("short.rs");

    fs::write(&file, "line 1\nline 2\nline 3\n").unwrap();

    let res = check_file_length(&[dir.path().to_path_buf()], 10, &["rs".to_string()], &[]);
    assert!(res.is_ok());
    let reports = res.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].lines, 3);
}

#[test]
fn test_file_length_exceed() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("long.rs");

    let content: String = (0..20).map(|i| format!("line {i}\n")).collect();
    fs::write(&file, content).unwrap();

    let res = check_file_length(&[dir.path().to_path_buf()], 10, &["rs".to_string()], &[]);
    assert!(res.is_err());
    assert!(
        res.unwrap_err()
            .to_string()
            .contains("Maximum file line count exceeded")
    );
}

#[test]
fn test_multiple_extensions_ignore_patterns_and_descending_sort() {
    let dir = tempdir().unwrap();
    let ignored = dir.path().join("target");
    fs::create_dir_all(&ignored).unwrap();

    fs::write(dir.path().join("a.rs"), "1\n2\n").unwrap();
    fs::write(dir.path().join("b.slint"), "1\n2\n3\n4\n5\n").unwrap();
    fs::write(dir.path().join("README"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").unwrap();
    fs::write(ignored.join("huge.rs"), "1\n".repeat(100)).unwrap();

    let reports = check_file_length(
        &[dir.path().to_path_buf()],
        10,
        &[".rs".to_string(), "slint".to_string()],
        &["target".to_string()],
    )
    .unwrap();

    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].lines, 5);
    assert_eq!(reports[1].lines, 2);
}

#[test]
fn test_gitignore_and_glob_patterns_respected() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join(".gitignore"),
        "ignored_dir/\n*.ignored.rs\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("ignored_dir")).unwrap();
    fs::create_dir_all(dir.path().join("gen")).unwrap();

    let huge = "fn a() {}\n".repeat(30);
    fs::write(dir.path().join("ignored_dir/big.rs"), &huge).unwrap();
    fs::write(dir.path().join("foo.ignored.rs"), &huge).unwrap();
    fs::write(dir.path().join("gen/auto.pb.rs"), &huge).unwrap();
    fs::write(dir.path().join("keep.rs"), "fn ok() {}\n").unwrap();

    let exts = vec!["rs".to_string()];
    let globs = vec!["**/*.pb.rs".to_string()];

    // With respect_gitignore = true and glob ignore, only keep.rs is checked
    let reports = check_file_length_with_options(
        &[dir.path().to_path_buf()],
        &FileLengthOptions {
            base_dir: dir.path(),
            max_lines: 10,
            extensions: &exts,
            ignore_patterns: &globs,
            mode: LineCountMode::Physical,
            respect_gitignore: true,
            changeset: None,
            emit_text: false,
        },
    )
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert!(reports[0].path.ends_with("keep.rs"));

    // With respect_gitignore = false, ignored_dir/big.rs and foo.ignored.rs are scanned and fail
    let res = check_file_length_with_options(
        &[dir.path().to_path_buf()],
        &FileLengthOptions {
            base_dir: dir.path(),
            max_lines: 10,
            extensions: &exts,
            ignore_patterns: &globs,
            mode: LineCountMode::Physical,
            respect_gitignore: false,
            changeset: None,
            emit_text: false,
        },
    );
    assert!(res.is_err());
}

#[test]
fn test_effective_lines_sloc_calculation_and_threshold() {
    let rust_src = r##"
// single line comment
/// doc comment
/* block comment
   /* nested block comment */
   still in block comment
*/

pub fn demo() {
    let url = "https://example.com"; // trailing comment counts as 1
    let raw = r#"/* not a comment */"#;
    let x = 1;
}
"##;
    // Code lines: pub fn demo(), let url = ..., let raw = ..., let x = 1;, } => 5
    assert_eq!(count_effective_lines(rust_src, "rs"), 5);

    let toml_src = "# comment\n\n[package]\nname = \"x\" # inline\n";
    assert_eq!(count_effective_lines(toml_src, "toml"), 2);

    let dir = tempdir().unwrap();
    let heavily_commented = format!("{}{}", "// comment\n\n".repeat(20), "pub fn tiny() {}\n");
    fs::write(dir.path().join("commented.rs"), heavily_commented).unwrap();

    let exts = vec!["rs".to_string()];
    // Physical lines (41 > 5) fails
    assert!(
        check_file_length_with_options(
            &[dir.path().to_path_buf()],
            &FileLengthOptions {
                base_dir: Path::new("."),
                max_lines: 5,
                extensions: &exts,
                ignore_patterns: &[],
                mode: LineCountMode::Physical,
                respect_gitignore: true,
                changeset: None,
                emit_text: false,
            },
        )
        .is_err()
    );

    // Effective SLOC (1 <= 5) passes
    let reports = check_file_length_with_options(
        &[dir.path().to_path_buf()],
        &FileLengthOptions {
            base_dir: Path::new("."),
            max_lines: 5,
            extensions: &exts,
            ignore_patterns: &[],
            mode: LineCountMode::Effective,
            respect_gitignore: true,
            changeset: None,
            emit_text: false,
        },
    )
    .unwrap();
    assert_eq!(reports[0].effective_lines, 1);
    assert_eq!(reports[0].physical_lines, 41);
    assert_eq!(reports[0].lines, 1);
}

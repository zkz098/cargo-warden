use cargo_warden::checks::file_length::check_file_length;
use std::fs;
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

//! Maximum file length and effective line count (SLOC) checker.
//!
//! Enforces maintainability by preventing source files from growing into
//! monolithic "god files" that resist review and comprehension.

use crate::config::LineCountMode;
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use crate::suppression::FileSuppressions;
use crate::walker::{WalkOptions, collect_files};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FileLengthReport {
    pub path: PathBuf,
    /// Measured line count according to the active `LineCountMode`.
    pub lines: usize,
    pub physical_lines: usize,
    pub effective_lines: usize,
}

/// Options for running the file length check with `.gitignore`, SLOC, and changeset support.
#[derive(Debug, Clone)]
pub struct FileLengthOptions<'a> {
    pub base_dir: &'a Path,
    pub max_lines: usize,
    pub extensions: &'a [String],
    pub ignore_patterns: &'a [String],
    pub mode: LineCountMode,
    pub respect_gitignore: bool,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentStyle {
    Slash { nested_block: bool },
    Hash,
    BlankOnly,
}

fn comment_style_for_ext(ext: &str) -> CommentStyle {
    match ext.to_ascii_lowercase().as_str() {
        "rs" => CommentStyle::Slash { nested_block: true },
        "slint" | "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "js" | "jsx" | "ts" | "tsx"
        | "java" | "go" | "kt" | "kts" | "swift" | "css" | "scss" => CommentStyle::Slash {
            nested_block: false,
        },
        "py" | "toml" | "yml" | "yaml" | "sh" | "bash" | "zsh" | "rb" | "pl" | "r" => {
            CommentStyle::Hash
        }
        _ => CommentStyle::BlankOnly,
    }
}

/// Count effective code lines (SLOC), excluding blank lines and comments.
pub fn count_effective_lines(content: &str, ext: &str) -> usize {
    let style = comment_style_for_ext(ext);
    match style {
        CommentStyle::Slash { nested_block } => {
            count_slash_sloc(content, nested_block, ext == "rs")
        }
        CommentStyle::Hash => count_hash_sloc(content),
        CommentStyle::BlankOnly => content.lines().filter(|l| !l.trim().is_empty()).count(),
    }
}

fn count_hash_sloc(content: &str) -> usize {
    let mut sloc = 0usize;
    for line in content.lines() {
        let mut has_code = false;
        let mut quote: Option<char> = None;
        let mut escaped = false;
        for ch in line.chars() {
            if let Some(q) = quote {
                has_code = true;
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == q {
                    quote = None;
                }
                continue;
            }
            if ch.is_whitespace() {
                continue;
            }
            if ch == '#' {
                break;
            }
            has_code = true;
            if ch == '"' || ch == '\'' {
                quote = Some(ch);
            }
        }
        if has_code {
            sloc += 1;
        }
    }
    sloc
}

fn parse_raw_string_start(bytes: &[u8], pos: usize) -> Option<(usize, usize)> {
    // Matches r"...", r#"..."#, br#"..."#, cr#"..."#
    let mut idx = pos;
    if bytes.get(idx) == Some(&b'b') || bytes.get(idx) == Some(&b'c') {
        idx += 1;
    }
    if bytes.get(idx) != Some(&b'r') {
        return None;
    }
    idx += 1;
    let mut hashes = 0usize;
    while bytes.get(idx) == Some(&b'#') {
        hashes += 1;
        idx += 1;
    }
    if bytes.get(idx) == Some(&b'"') {
        Some((idx + 1, hashes))
    } else {
        None
    }
}

struct SlashSlocScanner {
    nested_block: bool,
    is_rust: bool,
    block_depth: usize,
    in_string: bool,
    raw_hashes: Option<usize>,
}

impl SlashSlocScanner {
    fn new(nested_block: bool, is_rust: bool) -> Self {
        Self {
            nested_block,
            is_rust,
            block_depth: 0,
            in_string: false,
            raw_hashes: None,
        }
    }

    fn scan_line(&mut self, line: &str) -> bool {
        let bytes = line.as_bytes();
        let len = bytes.len();
        let mut i = 0usize;
        let mut has_code = false;
        let mut escaped = false;

        while i < len {
            if let Some(hashes) = self.raw_hashes {
                has_code = true;
                i += self.step_raw_string(bytes, i, hashes);
                continue;
            }
            if self.in_string {
                has_code = true;
                self.step_string(bytes[i], &mut escaped);
                i += 1;
                continue;
            }
            if self.block_depth > 0 {
                i += self.step_block_comment(bytes, i);
                continue;
            }
            if bytes[i].is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if is_pair(bytes, i, b'/', b'/') {
                break;
            }
            if is_pair(bytes, i, b'/', b'*') {
                self.block_depth = 1;
                i += 2;
                continue;
            }
            if let Some(next_i) = self.try_enter_raw_string(bytes, i) {
                has_code = true;
                i = next_i;
                continue;
            }
            has_code = true;
            if bytes[i] == b'"' {
                self.in_string = true;
            }
            i += 1;
        }

        has_code
    }

    fn step_raw_string(&mut self, bytes: &[u8], i: usize, hashes: usize) -> usize {
        let end = i + hashes;
        if bytes[i] == b'"' && end < bytes.len() && bytes[i + 1..=end].iter().all(|&b| b == b'#') {
            self.raw_hashes = None;
            return hashes + 1;
        }
        1
    }

    fn step_string(&mut self, byte: u8, escaped: &mut bool) {
        if *escaped {
            *escaped = false;
        } else if byte == b'\\' {
            *escaped = true;
        } else if byte == b'"' {
            self.in_string = false;
        }
    }

    fn step_block_comment(&mut self, bytes: &[u8], i: usize) -> usize {
        if self.nested_block && is_pair(bytes, i, b'/', b'*') {
            self.block_depth += 1;
            return 2;
        }
        if is_pair(bytes, i, b'*', b'/') {
            self.block_depth -= 1;
            return 2;
        }
        1
    }

    fn try_enter_raw_string(&mut self, bytes: &[u8], i: usize) -> Option<usize> {
        if !self.is_rust {
            return None;
        }
        let prev_is_ident = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if prev_is_ident {
            return None;
        }
        let (after_quote, hashes) = parse_raw_string_start(bytes, i)?;
        self.raw_hashes = Some(hashes);
        Some(after_quote)
    }
}

fn is_pair(bytes: &[u8], i: usize, first: u8, second: u8) -> bool {
    i + 1 < bytes.len() && bytes[i] == first && bytes[i + 1] == second
}

fn count_slash_sloc(content: &str, nested_block: bool, is_rust: bool) -> usize {
    let mut scanner = SlashSlocScanner::new(nested_block, is_rust);
    content.lines().filter(|l| scanner.scan_line(l)).count()
}

/// Collect structured diagnostics and reports for file lengths.
pub fn collect_file_length(
    paths: &[PathBuf],
    opts: &FileLengthOptions<'_>,
) -> Result<(Vec<FileLengthReport>, Vec<Diagnostic>)> {
    let mode_label = match opts.mode {
        LineCountMode::Physical => "physical",
        LineCountMode::Effective => "effective",
    };

    if opts.emit_text {
        println!(
            "[file-length] scanning files (max allowed {mode_label} lines: {})...",
            opts.max_lines
        );
    }

    tracing::debug!(
        roots = ?paths,
        max_lines = opts.max_lines,
        mode = mode_label,
        respect_gitignore = opts.respect_gitignore,
        extensions = ?opts.extensions,
        ignore_patterns = ?opts.ignore_patterns,
        "starting file length scan"
    );

    let files = collect_files(
        paths,
        &WalkOptions {
            extensions: opts.extensions,
            ignore_patterns: opts.ignore_patterns,
            respect_gitignore: opts.respect_gitignore,
            base_dir: Some(opts.base_dir),
            changeset: opts.changeset,
        },
    );

    let mut reports = Vec::new();
    let mut diagnostics = Vec::new();

    for path in files {
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        match fs::read_to_string(&path) {
            Ok(content) => {
                let physical_lines = content.lines().count();
                let effective_lines = count_effective_lines(&content, ext);
                let measured = match opts.mode {
                    LineCountMode::Physical => physical_lines,
                    LineCountMode::Effective => effective_lines,
                };
                tracing::trace!(
                    file = %path.display(),
                    physical_lines,
                    effective_lines,
                    mode = mode_label,
                    "counted file lines"
                );
                reports.push(FileLengthReport {
                    path: path.clone(),
                    lines: measured,
                    physical_lines,
                    effective_lines,
                });

                if measured > opts.max_lines {
                    let rel = normalize_rel_path(opts.base_dir, &path);
                    let suppressions = FileSuppressions::parse(&content);
                    let suppressed = suppressions.is_suppressed("file-length", None);
                    diagnostics.push(Diagnostic {
                        rule_id: "warden/file-length".to_string(),
                        check: "file-length".to_string(),
                        severity: Severity::Error,
                        message: format!(
                            "{} has {measured} {mode_label} lines (> {})",
                            rel.display(),
                            opts.max_lines
                        ),
                        file: Some(rel.clone()),
                        line: None,
                        column: None,
                        fingerprint: format!(
                            "file-length:{}",
                            rel.to_string_lossy().replace('\\', "/")
                        ),
                        metric_value: Some(measured),
                        suppressed,
                        suppression_reason: suppressed.then(|| "inline".to_string()),
                    });
                }
            }
            Err(err) => {
                tracing::warn!(file = %path.display(), error = %err, "failed to read file; skipping");
            }
        }
    }

    reports.sort_by_key(|a| std::cmp::Reverse(a.lines));
    Ok((reports, diagnostics))
}

pub fn check_file_length(
    paths: &[PathBuf],
    max_lines: usize,
    extensions: &[String],
    ignore_patterns: &[String],
) -> Result<Vec<FileLengthReport>> {
    check_file_length_with_options(
        paths,
        &FileLengthOptions {
            base_dir: Path::new("."),
            max_lines,
            extensions,
            ignore_patterns,
            mode: LineCountMode::Physical,
            respect_gitignore: true,
            changeset: None,
            emit_text: true,
        },
    )
}

pub fn check_file_length_with_options(
    paths: &[PathBuf],
    opts: &FileLengthOptions<'_>,
) -> Result<Vec<FileLengthReport>> {
    let (reports, diagnostics) = collect_file_length(paths, opts)?;
    let active: Vec<&Diagnostic> = diagnostics.iter().filter(|d| !d.suppressed).collect();

    if active.is_empty() {
        if opts.emit_text {
            let mode_label = match opts.mode {
                LineCountMode::Physical => "lines",
                LineCountMode::Effective => "effective lines",
            };
            println!(
                "[file-length] OK: scanned {} file(s), all within {} {}",
                reports.len(),
                opts.max_lines,
                mode_label
            );
        }
        Ok(reports)
    } else {
        if opts.emit_text {
            println!(
                "[file-length] ❌ {} file(s) exceed maximum length (threshold: {} lines):",
                active.len(),
                opts.max_lines
            );
            for d in &active {
                println!("  ❌ {}", d.message);
            }
        }
        bail!("Maximum file line count exceeded");
    }
}

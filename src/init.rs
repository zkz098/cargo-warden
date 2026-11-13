//! Interactive and auto-detecting project initialization wizard (`cargo warden init`).

use crate::config::{LineCountMode, WardenConfig};
use crate::report::normalize_rel_path;
use crate::walker::{WalkOptions, collect_files};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

/// Options controlling `cargo warden init`.
#[derive(Debug, Clone, Default)]
pub struct InitOptions {
    pub force: bool,
    pub dotfile: bool,
    pub interactive: bool,
}

/// Summary of auto-detected project characteristics.
#[derive(Debug, Clone)]
pub struct DetectedProject {
    pub dag_roots: Vec<String>,
    pub scan_paths: Vec<String>,
    pub workflow: Option<String>,
    pub test_patterns: Vec<String>,
    pub slint_ui_dir: Option<String>,
    pub slint_out_file: Option<String>,
    pub i18n_lang_dir: Option<String>,
    pub i18n_domain: Option<String>,
    pub i18n_locales: Vec<String>,
    pub i18n_out_file: Option<String>,
    pub complexity_max: usize,
    pub file_max_lines: usize,
    pub file_mode: LineCountMode,
    pub check_fmt: bool,
    pub check_clippy: bool,
}

impl Default for DetectedProject {
    fn default() -> Self {
        Self {
            dag_roots: Vec::new(),
            scan_paths: vec!["src".to_string()],
            workflow: None,
            test_patterns: vec!["cargo test".to_string(), "nextest".to_string()],
            slint_ui_dir: None,
            slint_out_file: None,
            i18n_lang_dir: None,
            i18n_domain: None,
            i18n_locales: Vec::new(),
            i18n_out_file: None,
            complexity_max: 25,
            file_max_lines: 800,
            file_mode: LineCountMode::Physical,
            check_fmt: true,
            check_clippy: false,
        }
    }
}

/// Inspect `base_dir` to infer sensible defaults for `warden.toml`.
pub fn detect_project(base_dir: &Path) -> DetectedProject {
    let mut detected = DetectedProject::default();
    detect_scan_and_dag_roots(base_dir, &mut detected);
    detect_workflow(base_dir, &mut detected);
    detect_slint(base_dir, &mut detected);
    detect_i18n(base_dir, &mut detected);
    detected
}

fn detect_scan_and_dag_roots(base_dir: &Path, detected: &mut DetectedProject) {
    let mut scan_paths = Vec::new();
    let mut dag_roots = Vec::new();

    if base_dir.join("crates").is_dir() {
        scan_paths.push("crates".to_string());
        if let Ok(entries) = fs::read_dir(base_dir.join("crates")) {
            let mut sub_roots: Vec<String> = entries
                .flatten()
                .map(|e| e.path().join("src"))
                .filter(|src| src.is_dir())
                .map(|src| {
                    normalize_rel_path(base_dir, &src)
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect();
            sub_roots.sort();
            dag_roots.extend(sub_roots);
        }
    }

    if base_dir.join("src").is_dir() {
        scan_paths.push("src".to_string());
        dag_roots.push("src".to_string());
    }
    if scan_paths.is_empty() {
        scan_paths.push(".".to_string());
    }

    detected.scan_paths = scan_paths;
    detected.dag_roots = dag_roots;
}

fn detect_workflow(base_dir: &Path, detected: &mut DetectedProject) {
    let workflows_dir = base_dir.join(".github/workflows");
    let Ok(entries) = fs::read_dir(&workflows_dir) else {
        return;
    };

    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|e| e == "yml" || e == "yaml")
        })
        .collect();

    candidates.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let priority = usize::from(!(name == "ci.yml" || name == "ci.yaml"));
        (priority, name.to_string())
    });

    for wf in candidates {
        let Ok(content) = fs::read_to_string(&wf) else {
            continue;
        };
        let patterns: Vec<String> = ["nextest", "llvm-cov", "cargo test"]
            .into_iter()
            .filter(|pat| content.contains(pat))
            .map(str::to_string)
            .collect();
        let is_ci_yml = wf.file_name().and_then(|n| n.to_str()) == Some("ci.yml");
        if !patterns.is_empty() || is_ci_yml {
            detected.workflow = Some(
                normalize_rel_path(base_dir, &wf)
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
            if !patterns.is_empty() {
                detected.test_patterns = patterns;
            }
            break;
        }
    }
}

fn detect_slint(base_dir: &Path, detected: &mut DetectedProject) {
    let slint_exts = vec!["slint".to_string()];
    let slint_files = collect_files(
        &[base_dir.to_path_buf()],
        &WalkOptions {
            extensions: &slint_exts,
            ignore_patterns: &["target".to_string()],
            respect_gitignore: true,
            base_dir: Some(base_dir),
            changeset: None,
        },
    );
    if slint_files.is_empty() {
        return;
    }

    let mut theme_path = None;
    let mut ui_dirs = BTreeSet::new();
    for f in &slint_files {
        if f.file_name().and_then(|n| n.to_str()) == Some("theme.slint") {
            theme_path = Some(normalize_rel_path(base_dir, f));
        }
        if let Some(parent) = f.parent() {
            let rel_parent = normalize_rel_path(base_dir, parent)
                .to_string_lossy()
                .replace('\\', "/");
            let candidate = rel_parent
                .strip_suffix("/components")
                .unwrap_or(&rel_parent)
                .to_string();
            ui_dirs.insert(candidate);
        }
    }

    if let Some(ui_dir) = ui_dirs.into_iter().next() {
        let out_file = theme_path
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| format!("{ui_dir}/theme.slint"));
        detected.slint_ui_dir = Some(ui_dir);
        detected.slint_out_file = Some(out_file);
    }
}

fn detect_i18n(base_dir: &Path, detected: &mut DetectedProject) {
    let po_exts = vec!["po".to_string()];
    let po_files = collect_files(
        &[base_dir.to_path_buf()],
        &WalkOptions {
            extensions: &po_exts,
            ignore_patterns: &["target".to_string()],
            respect_gitignore: true,
            base_dir: Some(base_dir),
            changeset: None,
        },
    );

    let mut locales = BTreeSet::new();
    for f in &po_files {
        let rel = normalize_rel_path(base_dir, f);
        let comps: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if comps.len() >= 4 && comps[comps.len() - 2] == "LC_MESSAGES" {
            let locale = &comps[comps.len() - 3];
            let domain = comps[comps.len() - 1]
                .strip_suffix(".po")
                .unwrap_or("messages")
                .to_string();
            let lang_dir = comps[..comps.len() - 3].join("/");
            detected.i18n_lang_dir.get_or_insert(lang_dir);
            detected.i18n_domain.get_or_insert(domain);
            locales.insert(locale.clone());
        }
    }
    if !locales.is_empty() {
        detected.i18n_locales = locales.into_iter().collect();
        detected.i18n_out_file = Some("src/i18n/catalog.rs".to_string());
    }
}

fn prompt_line<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    default: &str,
) -> Result<String> {
    write!(writer, "{label} [{default}]: ")?;
    writer.flush()?;
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_bool<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    default: bool,
) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    write!(writer, "{label} [{hint}]: ")?;
    writer.flush()?;
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    let trimmed = buf.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Ok(default);
    }
    Ok(matches!(trimmed.as_str(), "y" | "yes" | "true" | "1"))
}

fn run_interactive_prompts(detected: &mut DetectedProject) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    writeln!(writer, "🛠️  cargo-warden interactive setup\n")?;

    let max_comp = prompt_line(
        &mut reader,
        &mut writer,
        "Max function cognitive complexity",
        &detected.complexity_max.to_string(),
    )?;
    if let Ok(parsed) = max_comp.parse::<usize>() {
        detected.complexity_max = parsed;
    }

    let max_lines = prompt_line(
        &mut reader,
        &mut writer,
        "Max file line count",
        &detected.file_max_lines.to_string(),
    )?;
    if let Ok(parsed) = max_lines.parse::<usize>() {
        detected.file_max_lines = parsed;
    }

    let mode_str = prompt_line(
        &mut reader,
        &mut writer,
        "File line counting mode (physical/effective)",
        match detected.file_mode {
            LineCountMode::Physical => "physical",
            LineCountMode::Effective => "effective",
        },
    )?;
    detected.file_mode =
        if mode_str.eq_ignore_ascii_case("effective") || mode_str.eq_ignore_ascii_case("sloc") {
            LineCountMode::Effective
        } else {
            LineCountMode::Physical
        };

    if !detected.dag_roots.is_empty()
        && !prompt_bool(
            &mut reader,
            &mut writer,
            &format!(
                "Enable module DAG guard ({})?",
                detected.dag_roots.join(", ")
            ),
            true,
        )?
    {
        detected.dag_roots.clear();
    }

    if detected.workflow.is_some()
        && !prompt_bool(&mut reader, &mut writer, "Enable CI parity check?", true)?
    {
        detected.workflow = None;
    }

    detected.check_fmt = prompt_bool(
        &mut reader,
        &mut writer,
        "Run `cargo fmt --check` during `cargo warden check`?",
        detected.check_fmt,
    )?;
    detected.check_clippy = prompt_bool(
        &mut reader,
        &mut writer,
        "Run `cargo clippy` during `cargo warden check`?",
        detected.check_clippy,
    )?;

    writeln!(writer)?;
    Ok(())
}

/// Render a well-commented `warden.toml` from detected settings.
pub fn render_warden_toml(detected: &DetectedProject) -> String {
    let mut out = String::from("# Generated by `cargo warden init`\n");

    let mut exts = vec!["\"rs\""];
    if detected.slint_ui_dir.is_some() {
        exts.push("\"slint\"");
    }

    if !detected.dag_roots.is_empty() {
        let roots_toml: Vec<String> = detected
            .dag_roots
            .iter()
            .map(|r| format!("\"{r}\""))
            .collect();
        out.push_str(&format!(
            "\n[dag]\nroots = [{}]\nforbid_imports = []\n\n# Optional layered architecture rules:\n# [[dag.layers]]\n# name = \"domain\"\n# modules = [\"domain\"]\n# allow = []\n",
            roots_toml.join(", ")
        ));
    }

    if let Some(wf) = &detected.workflow {
        let pats: Vec<String> = detected
            .test_patterns
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect();
        out.push_str(&format!(
            "\n[ci_parity]\nworkflow = \"{wf}\"\ntest_patterns = [{}]\nignore_crates = []\nfanout_crates = []\n",
            pats.join(", ")
        ));
    }

    if let (Some(ui_dir), Some(out_file)) = (&detected.slint_ui_dir, &detected.slint_out_file) {
        out.push_str(&format!(
            "\n[slint_theme]\nui_dir = \"{ui_dir}\"\nout_file = \"{out_file}\"\nunchanged_colors = [\"ffffff\", \"000000\", \"6b7280\"]\nrespect_gitignore = true\n"
        ));
    }

    if let (Some(lang_dir), Some(domain), Some(out_file)) = (
        &detected.i18n_lang_dir,
        &detected.i18n_domain,
        &detected.i18n_out_file,
    ) {
        let locales: Vec<String> = detected
            .i18n_locales
            .iter()
            .map(|l| format!("\"{l}\""))
            .collect();
        out.push_str(&format!(
            "\n[i18n]\nlang_dir = \"{lang_dir}\"\nout_file = \"{out_file}\"\ndomain = \"{domain}\"\nlocales = [{}]\n",
            locales.join(", ")
        ));
    }

    let paths_toml: Vec<String> = detected
        .scan_paths
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect();
    let mode_str = match detected.file_mode {
        LineCountMode::Physical => "physical",
        LineCountMode::Effective => "effective",
    };

    out.push_str(&format!(
        "\n[complexity]\nmax = {}\npaths = [{}]\nignore_tests = true\nignore = [\"target\"]\nrespect_gitignore = true\n",
        detected.complexity_max,
        paths_toml.join(", ")
    ));

    out.push_str(&format!(
        "\n[file_length]\nmax_lines = {}\nextensions = [{}]\npaths = [{}]\nignore = [\"target\", \"tests\"]\nmode = \"{mode_str}\"\nrespect_gitignore = true\n",
        detected.file_max_lines,
        exts.join(", "),
        paths_toml.join(", ")
    ));

    out.push_str(&format!(
        "\n[check]\nfmt = {}\nclippy = {}\ndoc = false\n",
        detected.check_fmt, detected.check_clippy
    ));

    out.push_str("\n[gate]\ninclude_checks = true\ntest_groups = []\n");
    out
}

/// Run the initialization wizard in `base_dir`, returning the path written.
pub fn run_init(base_dir: &Path, opts: &InitOptions) -> Result<PathBuf> {
    let file_name = if opts.dotfile {
        ".warden.toml"
    } else {
        "warden.toml"
    };
    let target = base_dir.join(file_name);

    if target.exists() && !opts.force {
        bail!(
            "Configuration file already exists at {}. Pass `--force` to overwrite.",
            target.display()
        );
    }

    let mut detected = detect_project(base_dir);
    if opts.interactive {
        run_interactive_prompts(&mut detected)?;
    }

    let toml_text = render_warden_toml(&detected);
    // Sanity-check generated TOML against WardenConfig before writing.
    let _: WardenConfig =
        toml::from_str(&toml_text).context("Internal error: generated invalid warden.toml")?;

    fs::write(&target, &toml_text)
        .with_context(|| format!("Failed to write {}", target.display()))?;

    println!("✅ [init] wrote {}", target.display());
    println!("   - scan paths: {}", detected.scan_paths.join(", "));
    if !detected.dag_roots.is_empty() {
        println!("   - dag roots: {}", detected.dag_roots.join(", "));
    }
    if let Some(wf) = &detected.workflow {
        println!("   - ci workflow: {wf}");
    }
    if let Some(ui_dir) = &detected.slint_ui_dir {
        println!("   - slint ui_dir: {ui_dir}");
    }
    if let Some(lang_dir) = &detected.i18n_lang_dir {
        println!(
            "   - i18n lang_dir: {lang_dir} ({})",
            detected.i18n_locales.join(", ")
        );
    }

    Ok(target)
}

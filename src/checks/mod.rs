pub mod ci_parity;
pub mod complexity;
pub mod dag;
pub mod file_length;
pub mod i18n_po;
pub mod slint_theme;

use crate::baseline::{BaselineApplyStats, BaselineFile};
use crate::config::WardenConfig;
use crate::report::{CheckReport, Diagnostic, OutputFormat};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn join_paths(base_dir: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().map(|p| base_dir.join(p)).collect()
}

/// Runtime modifiers applied across static quality checks.
#[derive(Debug, Clone, Default)]
pub struct StaticRunOptions<'a> {
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub baseline_override: Option<&'a Path>,
    pub no_baseline: bool,
    pub format: OutputFormat,
}

/// Resolve the active baseline path from CLI override or `config.baseline`.
pub fn resolve_baseline_path(
    base_dir: &Path,
    config: &WardenConfig,
    override_path: Option<&Path>,
    no_baseline: bool,
) -> Option<PathBuf> {
    if no_baseline {
        return None;
    }
    if let Some(p) = override_path {
        return Some(if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        });
    }
    config.baseline.as_ref().map(|p| {
        if p.is_absolute() {
            p.clone()
        } else {
            base_dir.join(p)
        }
    })
}

/// Apply configured baseline (if present) and build a `CheckReport`.
pub fn apply_baseline_and_report(
    mut diagnostics: Vec<Diagnostic>,
    base_dir: &Path,
    config: &WardenConfig,
    override_path: Option<&Path>,
    no_baseline: bool,
) -> Result<(CheckReport, BaselineApplyStats)> {
    let mut stats = BaselineApplyStats::default();
    if let Some(bp) = resolve_baseline_path(base_dir, config, override_path, no_baseline) {
        if bp.is_file() {
            let baseline = BaselineFile::load(&bp)?;
            stats = baseline.apply(&mut diagnostics);
            tracing::debug!(
                baseline = %bp.display(),
                suppressed = stats.suppressed,
                regressions = stats.regressions,
                "applied baseline suppressions"
            );
        } else if override_path.is_some() {
            bail!("Baseline file not found: {}", bp.display());
        }
    }
    Ok((CheckReport::from_diagnostics(diagnostics), stats))
}

/// Collect diagnostics across all configured static checks without aborting on the first failure.
pub fn collect_all_static_checks(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Vec<Diagnostic>> {
    let emit_text = !opts.format.is_structured();
    let mut diagnostics = Vec::new();

    if let Some(fl_cfg) = &config.file_length {
        let search_paths = join_paths(base_dir, &fl_cfg.paths);
        let (reports, diags) = file_length::collect_file_length(
            &search_paths,
            &file_length::FileLengthOptions {
                base_dir,
                max_lines: fl_cfg.max_lines,
                extensions: &fl_cfg.extensions,
                ignore_patterns: &fl_cfg.ignore,
                mode: fl_cfg.mode,
                respect_gitignore: fl_cfg.respect_gitignore,
                changeset: opts.changeset,
                emit_text,
            },
        )?;
        if emit_text && !diags.iter().any(|d| !d.suppressed) {
            println!(
                "[file-length] OK: scanned {} file(s), all within {} lines",
                reports.len(),
                fl_cfg.max_lines
            );
        }
        diagnostics.extend(diags);
    }

    if let Some(comp_cfg) = &config.complexity {
        let search_paths = join_paths(base_dir, &comp_cfg.paths);
        let (reports, files_scanned, diags) = complexity::collect_complexity(
            &search_paths,
            &complexity::ComplexityOptions {
                base_dir,
                max_complexity: comp_cfg.max,
                ignore_tests: comp_cfg.ignore_tests,
                ignore_patterns: &comp_cfg.ignore,
                respect_gitignore: comp_cfg.respect_gitignore,
                changeset: opts.changeset,
                emit_text,
            },
        )?;
        if emit_text && !diags.iter().any(|d| !d.suppressed) {
            println!(
                "[complexity] OK: scanned {} functions across {} files, all within limit ({})",
                reports.len(),
                files_scanned,
                comp_cfg.max
            );
        }
        diagnostics.extend(diags);
    }

    if let Some(dag_cfg) = &config.dag {
        let mut dag_diags = Vec::new();
        for r in &dag_cfg.roots {
            let diags = dag::collect_dag(
                &base_dir.join(r),
                &dag::DagOptions {
                    base_dir,
                    forbid_imports: &dag_cfg.forbid_imports,
                    layers: &dag_cfg.layers,
                    rules: &dag_cfg.rules,
                    changeset: opts.changeset,
                    emit_text,
                },
            )?;
            dag_diags.extend(diags);
        }
        if emit_text && !dag_diags.iter().any(|d| !d.suppressed) {
            println!("[dag] OK: dependencies form a clean DAG with 0 cycle(s)");
        }
        diagnostics.extend(dag_diags);
    }

    if let Some(ci_cfg) = &config.ci_parity {
        let diags = ci_parity::collect_ci_parity(
            base_dir,
            &ci_parity::CiParityOptions {
                ci_file: &ci_cfg.workflow,
                test_patterns: &ci_cfg.test_patterns,
                ignore_crates: &ci_cfg.ignore_crates,
                fanout_expected: &ci_cfg.fanout_crates,
                gate_cfg: config.gate.as_ref(),
                changeset: opts.changeset,
                emit_text,
            },
        )?;
        diagnostics.extend(diags);
    }

    if let Some(theme_cfg) = &config.slint_theme
        && base_dir.join(&theme_cfg.ui_dir).exists()
    {
        let diags = slint_theme::collect_theme(
            &base_dir.join(&theme_cfg.ui_dir),
            &slint_theme::SlintThemeOptions {
                base_dir,
                unchanged: &theme_cfg.unchanged_colors,
                respect_gitignore: theme_cfg.respect_gitignore,
                changeset: opts.changeset,
                emit_text,
            },
        )?;
        if emit_text && !diags.iter().any(|d| !d.suppressed) {
            println!(
                "[slint-theme] OK: all hex colors in {} are mapped or whitelisted",
                base_dir.join(&theme_cfg.ui_dir).display()
            );
        }
        diagnostics.extend(diags);
    }

    if let Some(i18n_cfg) = &config.i18n
        && base_dir.join(&i18n_cfg.lang_dir).exists()
    {
        let diags = i18n_po::collect_i18n(&i18n_po::I18nOptions {
            base_dir,
            lang_dir: &base_dir.join(&i18n_cfg.lang_dir),
            domain: &i18n_cfg.domain,
            locales: &i18n_cfg.locales,
            out_file: &base_dir.join(&i18n_cfg.out_file),
            changeset: opts.changeset,
            emit_text,
        })?;
        diagnostics.extend(diags);
    }

    Ok(diagnostics)
}

/// Render diagnostics in the selected output format and return an error if active errors remain.
pub fn emit_report_and_enforce(
    report: &CheckReport,
    stats: BaselineApplyStats,
    format: OutputFormat,
    failure_summary: &str,
) -> Result<()> {
    match format {
        OutputFormat::Json => {
            println!("{}", report.to_json_pretty()?);
            if !report.passed {
                bail!("{failure_summary}");
            }
            Ok(())
        }
        OutputFormat::Sarif => {
            println!("{}", report.to_sarif_pretty()?);
            if !report.passed {
                bail!("{failure_summary}");
            }
            Ok(())
        }
        OutputFormat::Text => {
            let inline_suppressed = report
                .diagnostics
                .iter()
                .filter(|d| d.suppressed && d.suppression_reason.as_deref() == Some("inline"))
                .count();
            if inline_suppressed > 0 || stats.suppressed > 0 {
                println!(
                    "ℹ️  [warden] suppressed {} diagnostic(s) (inline: {}, baseline: {})",
                    inline_suppressed + stats.suppressed,
                    inline_suppressed,
                    stats.suppressed
                );
            }

            let active: Vec<&Diagnostic> = report
                .diagnostics
                .iter()
                .filter(|d| !d.suppressed)
                .collect();

            if active.is_empty() {
                println!("✨ [warden] all static quality checks passed successfully!");
                Ok(())
            } else {
                println!("❌ [warden] {} violation(s) found:", active.len());
                for d in &active {
                    println!("  ❌ [{}] {}", d.check, d.message);
                }
                bail!("{failure_summary}");
            }
        }
    }
}

/// Run all static quality checks configured in `WardenConfig`.
pub fn run_static_checks(config: &WardenConfig, base_dir: &Path) -> Result<()> {
    run_static_checks_with_options(config, base_dir, &StaticRunOptions::default())
}

/// Run all static quality checks with changeset, baseline, and structured format options.
pub fn run_static_checks_with_options(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<()> {
    if !opts.format.is_structured() {
        println!("🔍 [warden] running all configured static quality checks...");
    }

    let diagnostics = collect_all_static_checks(config, base_dir, opts)?;
    let (report, stats) = apply_baseline_and_report(
        diagnostics,
        base_dir,
        config,
        opts.baseline_override,
        opts.no_baseline,
    )?;

    let summary_err = summarize_failure(&report);
    emit_report_and_enforce(&report, stats, opts.format, &summary_err)
}

fn summarize_failure(report: &CheckReport) -> String {
    let active_checks: BTreeSet<&str> = report
        .diagnostics
        .iter()
        .filter(|d| !d.suppressed)
        .map(|d| d.check.as_str())
        .collect();

    if active_checks.len() == 1 {
        match active_checks.into_iter().next() {
            Some("file-length") => return "Maximum file line count exceeded".to_string(),
            Some("complexity") => return "Cognitive complexity threshold exceeded".to_string(),
            _ => {}
        }
    }
    format!(
        "{} static quality check violation(s) found",
        report.summary.errors
    )
}

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
#[derive(Debug, Clone)]
pub struct StaticRunOptions<'a> {
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub baseline_override: Option<&'a Path>,
    pub no_baseline: bool,
    pub format: OutputFormat,
    /// Run independent read-only static checks in parallel (default: true).
    pub parallel: bool,
}

impl Default for StaticRunOptions<'_> {
    fn default() -> Self {
        Self {
            changeset: None,
            baseline_override: None,
            no_baseline: false,
            format: OutputFormat::Text,
            parallel: true,
        }
    }
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

struct CheckTaskOutcome {
    diagnostics: Vec<Diagnostic>,
    ok_line: Option<String>,
}

fn run_file_length_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(fl_cfg) = &config.file_length else {
        return Ok(None);
    };
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
            emit_text: false,
        },
    )?;
    let ok_line = (!diags.iter().any(|d| !d.suppressed)).then(|| {
        format!(
            "[file-length] OK: scanned {} file(s), all within {} lines",
            reports.len(),
            fl_cfg.max_lines
        )
    });
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    }))
}

fn run_complexity_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(comp_cfg) = &config.complexity else {
        return Ok(None);
    };
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
            emit_text: false,
        },
    )?;
    let ok_line = (!diags.iter().any(|d| !d.suppressed)).then(|| {
        format!(
            "[complexity] OK: scanned {} functions across {} files, all within limit ({})",
            reports.len(),
            files_scanned,
            comp_cfg.max
        )
    });
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    }))
}

fn run_dag_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(dag_cfg) = &config.dag else {
        return Ok(None);
    };
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
                emit_text: false,
            },
        )?;
        dag_diags.extend(diags);
    }
    let ok_line = (!dag_diags.iter().any(|d| !d.suppressed))
        .then(|| "[dag] OK: dependencies form a clean DAG with 0 cycle(s)".to_string());
    Ok(Some(CheckTaskOutcome {
        diagnostics: dag_diags,
        ok_line,
    }))
}

fn run_ci_parity_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(ci_cfg) = &config.ci_parity else {
        return Ok(None);
    };
    let diags = ci_parity::collect_ci_parity(
        base_dir,
        &ci_parity::CiParityOptions {
            ci_file: &ci_cfg.workflow,
            test_patterns: &ci_cfg.test_patterns,
            ignore_crates: &ci_cfg.ignore_crates,
            fanout_expected: &ci_cfg.fanout_crates,
            gate_cfg: config.gate.as_ref(),
            changeset: opts.changeset,
            emit_text: false,
        },
    )?;
    let ok_line = (!diags.iter().any(|d| !d.suppressed)).then(|| {
        format!(
            "[ci-parity] OK: workspace parity verified in {}",
            ci_cfg.workflow.display()
        )
    });
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    }))
}

fn run_slint_theme_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(theme_cfg) = &config.slint_theme else {
        return Ok(None);
    };
    let ui_dir = base_dir.join(&theme_cfg.ui_dir);
    if !ui_dir.exists() {
        return Ok(None);
    }
    let diags = slint_theme::collect_theme(
        &ui_dir,
        &slint_theme::SlintThemeOptions {
            base_dir,
            unchanged: &theme_cfg.unchanged_colors,
            respect_gitignore: theme_cfg.respect_gitignore,
            changeset: opts.changeset,
            emit_text: false,
        },
    )?;
    let ok_line = (!diags.iter().any(|d| !d.suppressed)).then(|| {
        format!(
            "[slint-theme] OK: all hex colors in {} are mapped or whitelisted",
            ui_dir.display()
        )
    });
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    }))
}

fn run_i18n_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(i18n_cfg) = &config.i18n else {
        return Ok(None);
    };
    let lang_dir = base_dir.join(&i18n_cfg.lang_dir);
    if !lang_dir.exists() {
        return Ok(None);
    }
    let out_file = base_dir.join(&i18n_cfg.out_file);
    let diags = i18n_po::collect_i18n(&i18n_po::I18nOptions {
        base_dir,
        lang_dir: &lang_dir,
        domain: &i18n_cfg.domain,
        locales: &i18n_cfg.locales,
        out_file: &out_file,
        changeset: opts.changeset,
        emit_text: false,
    })?;
    let ok_line = (!diags.iter().any(|d| !d.suppressed)).then(|| {
        format!(
            "[i18n] OK: {} is in sync with .po catalogs",
            out_file.display()
        )
    });
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    }))
}

/// Collect diagnostics across all configured static checks (optionally in parallel).
pub fn collect_all_static_checks(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Vec<Diagnostic>> {
    let emit_text = !opts.format.is_structured();
    let outcomes: Vec<Result<Option<CheckTaskOutcome>>> = if opts.parallel {
        tracing::debug!("running static quality checks in parallel");
        std::thread::scope(|s| {
            let handles = [
                s.spawn(|| run_file_length_task(config, base_dir, opts)),
                s.spawn(|| run_complexity_task(config, base_dir, opts)),
                s.spawn(|| run_dag_task(config, base_dir, opts)),
                s.spawn(|| run_ci_parity_task(config, base_dir, opts)),
                s.spawn(|| run_slint_theme_task(config, base_dir, opts)),
                s.spawn(|| run_i18n_task(config, base_dir, opts)),
            ];
            handles
                .into_iter()
                .map(|h| match h.join() {
                    Ok(res) => res,
                    Err(_) => bail!("Static check worker thread panicked"),
                })
                .collect()
        })
    } else {
        tracing::debug!("running static quality checks sequentially");
        vec![
            run_file_length_task(config, base_dir, opts),
            run_complexity_task(config, base_dir, opts),
            run_dag_task(config, base_dir, opts),
            run_ci_parity_task(config, base_dir, opts),
            run_slint_theme_task(config, base_dir, opts),
            run_i18n_task(config, base_dir, opts),
        ]
    };

    let mut diagnostics = Vec::new();
    for outcome_res in outcomes {
        if let Some(outcome) = outcome_res? {
            if emit_text && let Some(ok_line) = outcome.ok_line {
                println!("{ok_line}");
            }
            diagnostics.extend(outcome.diagnostics);
        }
    }

    Ok(diagnostics)
}

/// Apply automatic fixes for configured static asset generators/rewriters (`slint_theme`, `i18n`).
pub fn run_static_fixes(config: &WardenConfig, base_dir: &Path, emit_text: bool) -> Result<()> {
    if let Some(theme_cfg) = &config.slint_theme {
        let ui_dir = base_dir.join(&theme_cfg.ui_dir);
        if ui_dir.exists() {
            if let Some(def_path) = &theme_cfg.theme_def {
                let full_def = base_dir.join(def_path);
                if full_def.is_file() {
                    let content = std::fs::read_to_string(&full_def)?;
                    let def: slint_theme::ThemeDefinition = toml::from_str(&content)?;
                    slint_theme::generate_theme(&def, &base_dir.join(&theme_cfg.out_file))?;
                }
            }
            let mapping = slint_theme::load_token_mapping(base_dir, theme_cfg.theme_def.as_deref());
            slint_theme::apply_tokens(&ui_dir, &mapping, &theme_cfg.unchanged_colors)?;
        }
    }

    if let Some(i18n_cfg) = &config.i18n {
        let lang_dir = base_dir.join(&i18n_cfg.lang_dir);
        if lang_dir.exists() {
            if emit_text {
                println!(
                    "[fix] syncing i18n catalog -> {}",
                    base_dir.join(&i18n_cfg.out_file).display()
                );
            }
            i18n_po::sync_i18n(
                &lang_dir,
                &i18n_cfg.domain,
                &i18n_cfg.locales,
                &base_dir.join(&i18n_cfg.out_file),
            )?;
        }
    }

    Ok(())
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
        let mode = if opts.parallel {
            "in parallel"
        } else {
            "sequentially"
        };
        println!("🔍 [warden] running all configured static quality checks ({mode})...");
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

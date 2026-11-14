pub mod actions;
mod ast_pipeline;
pub mod ci_parity;
pub mod complexity;
pub mod dag;
pub mod file_length;
pub mod hygiene;
pub mod i18n_po;
pub mod i18n_source;
pub mod manifest;
mod parallel;
pub mod slint_theme;

use crate::baseline::{BaselineApplyStats, BaselineFile};
use crate::config::WardenConfig;
use crate::report::{CheckReport, Diagnostic, OutputFormat, Severity};
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
    diagnostics: Vec<Diagnostic>,
    base_dir: &Path,
    config: &WardenConfig,
    override_path: Option<&Path>,
    no_baseline: bool,
) -> Result<(CheckReport, BaselineApplyStats)> {
    apply_baseline_and_report_with_scope(
        diagnostics,
        base_dir,
        config,
        override_path,
        no_baseline,
        false,
    )
}

/// Apply configured baseline (if present), optionally count stale entries on full runs, and build a `CheckReport`.
pub fn apply_baseline_and_report_with_scope(
    mut diagnostics: Vec<Diagnostic>,
    base_dir: &Path,
    config: &WardenConfig,
    override_path: Option<&Path>,
    no_baseline: bool,
    is_full_run: bool,
) -> Result<(CheckReport, BaselineApplyStats)> {
    let mut stats = BaselineApplyStats::default();
    if let Some(bp) = resolve_baseline_path(base_dir, config, override_path, no_baseline) {
        if bp.is_file() {
            let baseline = BaselineFile::load(&bp)?;
            stats = baseline.apply(&mut diagnostics);
            if is_full_run {
                stats.stale = baseline.stale_count(&diagnostics);
            }
            tracing::debug!(
                baseline = %bp.display(),
                suppressed = stats.suppressed,
                regressions = stats.regressions,
                stale = stats.stale,
                "applied baseline suppressions"
            );
        } else if override_path.is_some() {
            bail!("Baseline file not found: {}", bp.display());
        }
    }
    Ok((CheckReport::from_diagnostics(diagnostics), stats))
}

pub(crate) struct CheckTaskOutcome {
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) ok_line: Option<String>,
}

fn run_file_length_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
    parallel: bool,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(fl_cfg) = &config.file_length else {
        return Ok(None);
    };
    let search_paths = join_paths(base_dir, &fl_cfg.paths);
    let (reports, diags) = file_length::collect_file_length_with_overrides(
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
        &fl_cfg.overrides,
        parallel,
    )?;
    let ok_line = (!diags
        .iter()
        .any(|d| !d.suppressed && d.severity == Severity::Error))
    .then(|| {
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

fn run_workspace_dag_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(dag_cfg) = &config.dag else {
        return Ok(None);
    };
    let Some(ws_cfg) = &dag_cfg.workspace else {
        return Ok(None);
    };
    if !ws_cfg.is_active() {
        return Ok(None);
    }
    let diags = dag::workspace::collect_workspace_dag(&dag::workspace::WorkspaceDagOptions {
        base_dir,
        config: ws_cfg,
        changeset: opts.changeset,
        emit_text: false,
    })?;
    let ok_line = (!diags
        .iter()
        .any(|d| !d.suppressed && d.severity == Severity::Error))
    .then(|| {
        "[workspace-dag] OK: workspace crate dependencies satisfy all boundary rules".to_string()
    });
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
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
    let ok_line = (!diags
        .iter()
        .any(|d| !d.suppressed && d.severity == Severity::Error))
    .then(|| {
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

fn run_actions_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(actions_cfg) = &config.actions else {
        return Ok(None);
    };
    if !actions_cfg.enabled_in_check {
        return Ok(None);
    }
    let diags = actions::collect_actions(
        base_dir,
        &actions::ActionsOptions {
            workflows: &actions_cfg.workflows,
            ignore: &actions_cfg.ignore,
            strict: actions_cfg.strict,
            require_sha_pin: actions_cfg.require_sha_pin,
            offline: false,
            changeset: opts.changeset,
            emit_text: false,
            parallel: opts.parallel,
        },
    )?;
    let ok_line = diags
        .is_empty()
        .then(|| "[actions] OK: all GitHub Actions references are up to date".to_string());
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    }))
}

fn run_manifest_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(manifest_cfg) = &config.manifest else {
        return Ok(None);
    };
    let diags = manifest::collect_manifest(&manifest::ManifestOptions {
        base_dir,
        config: manifest_cfg,
        changeset: opts.changeset,
        emit_text: false,
    })?;
    let ok_line = (!diags
        .iter()
        .any(|d| !d.suppressed && d.severity == Severity::Error))
    .then(|| "[manifest] OK: workspace manifests satisfy all consistency rules".to_string());
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    }))
}

fn run_slint_theme_task(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
    parallel: bool,
) -> Result<Option<CheckTaskOutcome>> {
    let Some(theme_cfg) = &config.slint_theme else {
        return Ok(None);
    };
    let ui_dir = base_dir.join(&theme_cfg.ui_dir);
    if !ui_dir.exists() {
        return Ok(None);
    }
    let diags = slint_theme::collect_theme_with_policy(
        &ui_dir,
        &slint_theme::SlintThemePolicy {
            base_dir,
            out_file: &theme_cfg.out_file,
            theme_def: theme_cfg.theme_def.as_deref(),
            unchanged_colors: &theme_cfg.unchanged_colors,
            exclude: &theme_cfg.exclude,
            respect_gitignore: theme_cfg.respect_gitignore,
            verify_generated: theme_cfg.verify_generated,
            check_unknown_tokens: theme_cfg.check_unknown_tokens,
            check_unused_tokens: theme_cfg.check_unused_tokens,
            rules: &theme_cfg.rules,
            changeset: opts.changeset,
            emit_text: false,
            parallel,
        },
    )?;
    let ok_line = (!diags
        .iter()
        .any(|d| !d.suppressed && d.severity == Severity::Error))
    .then(|| {
        format!(
            "[slint-theme] OK: {} satisfies all enabled Slint token/UI rules",
            ui_dir.display()
        )
    });
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    }))
}

/// Optional CLI overrides when invoking `cargo warden i18n check`.
#[derive(Debug, Clone, Default)]
pub struct I18nOverrides<'a> {
    pub lang_dir: Option<PathBuf>,
    pub out_file: Option<PathBuf>,
    pub source_dirs: &'a [PathBuf],
    pub strict: bool,
    pub check_unused: bool,
    pub no_placeholders: bool,
}

/// Collect i18n diagnostics from `I18nConfig` plus optional CLI overrides.
pub fn collect_i18n_from_config(
    i18n_cfg: &crate::config::I18nConfig,
    base_dir: &Path,
    overrides: &I18nOverrides<'_>,
    changeset: Option<&BTreeSet<PathBuf>>,
    emit_text: bool,
) -> Result<Vec<Diagnostic>> {
    let ldir = overrides
        .lang_dir
        .clone()
        .unwrap_or_else(|| i18n_cfg.lang_dir.clone());
    let ofile = overrides
        .out_file
        .clone()
        .unwrap_or_else(|| i18n_cfg.out_file.clone());
    let src_dirs = if overrides.source_dirs.is_empty() {
        &i18n_cfg.source_dirs
    } else {
        overrides.source_dirs
    };
    i18n_po::collect_i18n(&i18n_po::I18nOptions {
        base_dir,
        lang_dir: &base_dir.join(ldir),
        domain: &i18n_cfg.domain,
        locales: &i18n_cfg.locales,
        out_file: &base_dir.join(ofile),
        source_dirs: src_dirs,
        tr_macros: &i18n_cfg.tr_macros,
        strict: i18n_cfg.strict || overrides.strict,
        check_placeholders: i18n_cfg.check_placeholders && !overrides.no_placeholders,
        check_unused: i18n_cfg.check_unused || overrides.check_unused,
        respect_gitignore: i18n_cfg.respect_gitignore,
        changeset,
        emit_text,
    })
}

/// Collect diagnostics across all configured static checks (optionally in parallel).
pub fn collect_all_static_checks(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<Vec<Diagnostic>> {
    let emit_text = !opts.format.is_structured();
    let ast_plans = ast_pipeline::build_plans(config, base_dir, opts)?;
    let hygiene_ignore_tests = config.hygiene.as_ref().is_none_or(|h| h.ignore_tests);

    let (fl_res, ci_res, actions_res, theme_res, ws_dag_res, manifest_res, ast_results) = if opts
        .parallel
    {
        tracing::debug!("running static quality checks with shared AST and file-level parallelism");
        std::thread::scope(|s| {
            let ci_handle = s.spawn(|| run_ci_parity_task(config, base_dir, opts));
            let actions_handle = s.spawn(|| run_actions_task(config, base_dir, opts));
            let fl_handle = s.spawn(|| run_file_length_task(config, base_dir, opts, true));
            let theme_handle = s.spawn(|| run_slint_theme_task(config, base_dir, opts, true));
            let ws_dag_handle = s.spawn(|| run_workspace_dag_task(config, base_dir, opts));
            let manifest_handle = s.spawn(|| run_manifest_task(config, base_dir, opts));
            let ast_results =
                ast_pipeline::run_shared_ast_tasks(&ast_plans, hygiene_ignore_tests, true);

            let join_task = |h: std::thread::ScopedJoinHandle<
                '_,
                Result<Option<CheckTaskOutcome>>,
            >| match h.join() {
                Ok(res) => res,
                Err(_) => bail!("Static check worker thread panicked"),
            };
            (
                join_task(fl_handle),
                join_task(ci_handle),
                join_task(actions_handle),
                join_task(theme_handle),
                join_task(ws_dag_handle),
                join_task(manifest_handle),
                ast_results,
            )
        })
    } else {
        tracing::debug!("running static quality checks sequentially with shared AST");
        let fl_res = run_file_length_task(config, base_dir, opts, false);
        let ast_results =
            ast_pipeline::run_shared_ast_tasks(&ast_plans, hygiene_ignore_tests, false);
        let ci_res = run_ci_parity_task(config, base_dir, opts);
        let actions_res = run_actions_task(config, base_dir, opts);
        let theme_res = run_slint_theme_task(config, base_dir, opts, false);
        let ws_dag_res = run_workspace_dag_task(config, base_dir, opts);
        let manifest_res = run_manifest_task(config, base_dir, opts);
        (
            fl_res,
            ci_res,
            actions_res,
            theme_res,
            ws_dag_res,
            manifest_res,
            ast_results,
        )
    };

    let comp_outcome = ast_pipeline::finish_complexity_outcome(
        ast_plans.complexity.as_ref(),
        config,
        base_dir,
        opts,
        &ast_plans.tasks,
        &ast_results,
    );
    let hygiene_outcome = ast_pipeline::finish_hygiene_outcome(
        ast_plans.hygiene.as_ref(),
        config,
        base_dir,
        &ast_plans.tasks,
        &ast_results,
    );
    let dag_outcome = ast_pipeline::finish_dag_outcome(
        ast_plans.dag_roots.as_deref(),
        config,
        base_dir,
        opts,
        &ast_results,
    );
    let i18n_outcome = ast_pipeline::finish_i18n_outcome(
        ast_plans.i18n.as_ref(),
        config,
        base_dir,
        opts,
        &ast_results,
    )?;

    let outcomes = [
        fl_res?,
        comp_outcome,
        hygiene_outcome,
        dag_outcome,
        ws_dag_res?,
        manifest_res?,
        ci_res?,
        actions_res?,
        theme_res?,
        i18n_outcome,
    ];

    let warn_unused = config
        .check
        .as_ref()
        .is_none_or(|c| c.warn_unused_suppressions);

    let mut diagnostics = Vec::new();
    for outcome in outcomes.into_iter().flatten() {
        if emit_text && let Some(ok_line) = outcome.ok_line {
            println!("{ok_line}");
        }
        diagnostics.extend(outcome.diagnostics);
    }

    if !warn_unused {
        diagnostics.retain(|d| d.rule_id != "warden/unused-suppression");
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
            slint_theme::apply_tokens_with_out_file(
                &ui_dir,
                &base_dir.join(&theme_cfg.out_file),
                &mapping,
                &theme_cfg.unchanged_colors,
            )?;
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

    if let Some(actions_cfg) = &config.actions
        && actions_cfg.enabled_in_check
    {
        let parallel = config.check.as_ref().is_none_or(|c| c.parallel);
        actions::fix_actions(
            base_dir,
            &actions::ActionsOptions {
                workflows: &actions_cfg.workflows,
                ignore: &actions_cfg.ignore,
                strict: actions_cfg.strict,
                require_sha_pin: actions_cfg.require_sha_pin,
                offline: false,
                changeset: None,
                emit_text,
                parallel,
            },
            true,
            actions_cfg.require_sha_pin,
        )?;
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
        OutputFormat::GithubActions => {
            let gha = report.to_github_actions();
            if !gha.is_empty() {
                println!("{gha}");
            }
            if !report.passed {
                bail!("{failure_summary}");
            }
            Ok(())
        }
        OutputFormat::Text => emit_text_report(report, stats, failure_summary),
    }
}

fn emit_text_report(
    report: &CheckReport,
    stats: BaselineApplyStats,
    failure_summary: &str,
) -> Result<()> {
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
    if stats.stale > 0 {
        println!(
            "💡 [baseline] {} stale baseline entry(ies) can be removed with `cargo warden baseline prune`",
            stats.stale
        );
    }

    let warnings: Vec<&Diagnostic> = report
        .diagnostics
        .iter()
        .filter(|d| !d.suppressed && d.severity == Severity::Warning)
        .collect();
    for w in &warnings {
        println!("  ⚠️  [{}] {}", w.check, w.message);
    }

    let errors: Vec<&Diagnostic> = report
        .diagnostics
        .iter()
        .filter(|d| !d.suppressed && d.severity == Severity::Error)
        .collect();

    if errors.is_empty() {
        println!("✨ [warden] all static quality checks passed successfully!");
        Ok(())
    } else {
        println!("❌ [warden] {} violation(s) found:", errors.len());
        for d in &errors {
            println!("  ❌ [{}] {}", d.check, d.message);
        }
        bail!("{failure_summary}");
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
    let (report, stats) = apply_baseline_and_report_with_scope(
        diagnostics,
        base_dir,
        config,
        opts.baseline_override,
        opts.no_baseline,
        opts.changeset.is_none(),
    )?;

    let summary_err = summarize_failure(&report);
    emit_report_and_enforce(&report, stats, opts.format, &summary_err)
}

fn summarize_failure(report: &CheckReport) -> String {
    let active_checks: BTreeSet<&str> = report
        .diagnostics
        .iter()
        .filter(|d| !d.suppressed && d.severity == Severity::Error)
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
